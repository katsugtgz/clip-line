//! clipLine desktop app — SPEC.md "ui" section.
//! Main window + hide-after-paste + global hotkey toggle + tray (Windows).
//! TODO(spec ui, phase 2 rest): caret-positioned popup viewport.

use std::sync::atomic::{AtomicU64, Ordering};

use clip_line_core::config::Config;
use clip_line_core::paste::{ClipData, PasteMode};
use clip_line_core::store::{ClipRow, Filter, Snippet, Store, StoreOpts};
use eframe::egui;
use egui::{Context, Key, ViewportCommand};
use egui_extras::{Column, TableBuilder};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Self-paste suppression slot: the UI thread stores the payload it is about to
/// write to the clipboard BEFORE writing; the capture thread skips the event
/// whose packed payload matches, then clears the slot. Payload identity is
/// race-free where sequence-number arming is not (the WM_CLIPBOARDUPDATE lands
/// before a cross-thread arm could). `envelope::pack` is deterministic.
/// ponytail: replace with `WinCapture` sequence suppression when the win crate
/// grows a shared ignore slot.
type SuppressSlot = Arc<Mutex<Option<Vec<u8>>>>;

/// ponytail: i18n scaffold — identity fn, English inline; swap for rust-i18n
/// `t!` when locales arrive (mechanical rename).
pub fn t(s: &str) -> &str {
    s
}

const DEFAULT_TOML: &str = r#"# clipLine config — see SPEC.md. Regenerated if deleted.
[hotkeys]
show = "Ctrl+Shift+Up"      # global show/hide toggle

[paste]
default_mode = "inject_ctrl_v"   # inject_ctrl_v | set_only | type_in

[ui]
hide_after_ms = 1200

[autostart]
enabled = false

[update]
check_on_start = true    # Q11: GitHub Releases check on start; no telemetry

# [storage]
# path = "C:\path\to\clipline.db"
# backup_on_exit = false   # snapshot next to the DB when the app exits
"#;

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct Hotkeys {
    show: String,
}

impl Hotkeys {
    fn with_default_show() -> Self {
        Self {
            show: "Ctrl+Shift+Up".into(),
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct Autostart {
    enabled: bool,
}

/// `[update]` (Q11). Default true via manual Default (derive would give false).
#[derive(Debug, serde::Deserialize)]
#[serde(default)]
struct UpdateCfg {
    check_on_start: bool,
}

impl Default for UpdateCfg {
    fn default() -> Self {
        Self {
            check_on_start: true,
        }
    }
}

/// App-level config sections on top of `clip_line_core::config::Config`
/// (storage/paste/ui). Parsed from the same TOML text; core ignores unknown keys.
#[derive(Debug)]
struct Extras {
    hotkeys: Hotkeys,
    autostart: Autostart,
    update: UpdateCfg,
}

fn load_config(path: &Path) -> anyhow::Result<(Config, Extras)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::write(path, DEFAULT_TOML); // first run: defaults were missing
            DEFAULT_TOML.to_string()
        }
        Err(e) => return Err(e.into()),
    };
    let hotkeys = toml::from_str::<Hotkeys>(&text).unwrap_or_else(|_| Hotkeys::with_default_show());
    let autostart = toml::from_str::<Autostart>(&text).unwrap_or_default();
    let update = toml::from_str::<UpdateCfg>(&text).unwrap_or_default();
    Ok((
        toml::from_str(&text)?,
        Extras {
            hotkeys,
            autostart,
            update,
        },
    ))
}

/// "Ctrl+Shift+V" → (modifier bits Alt=1|Ctrl=2|Shift=4|Win=8, virtual key).
/// Bit layout mirrors the legacy decode in `clip_line_core::config`.
pub fn parse_hotkey(s: &str) -> Option<(u32, u32)> {
    let mut mods = 0u32;
    let mut key: Option<u32> = None;
    for part in s.split('+') {
        let p = part.trim();
        if p.is_empty() {
            return None;
        }
        if key.is_some() {
            return None; // the key must come last
        }
        match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= 2,
            "shift" => mods |= 4,
            "alt" => mods |= 1,
            "win" | "super" | "meta" => mods |= 8,
            _ => {
                let k = p.to_ascii_uppercase();
                let v = if k.len() == 1 {
                    let c = k.as_bytes()[0];
                    match c {
                        b'A'..=b'Z' | b'0'..=b'9' => Some(c as u32),
                        _ => None,
                    }
                } else if let Some(f) = k.strip_prefix('F') {
                    f.parse::<u32>()
                        .ok()
                        .filter(|n| (1..=24).contains(n))
                        .map(|n| 0x6F + n) // VK_F1 = 0x70
                } else {
                    // Windows virtual-key codes for non-printable keys users bind.
                    match k.as_str() {
                        "UP" => Some(0x26),
                        "DOWN" => Some(0x28),
                        "LEFT" => Some(0x25),
                        "RIGHT" => Some(0x27),
                        "SPACE" => Some(0x20),
                        "INSERT" => Some(0x2D),
                        "DELETE" => Some(0x2E),
                        "HOME" => Some(0x24),
                        "END" => Some(0x23),
                        "PAGEUP" => Some(0x21),
                        "PAGEDOWN" => Some(0x22),
                        "ESC" | "ESCAPE" => Some(0x1B),
                        "TAB" => Some(0x09),
                        "ENTER" => Some(0x0D),
                        "BACKSPACE" => Some(0x08),
                        _ => None,
                    }
                };
                key = Some(v?);
            }
        }
    }
    key.map(|k| (mods, k))
}

/// Config `[paste] default_mode` string → enum; unknown values fall back to
/// InjectCtrlV (the app default: paste into the active window).
pub fn parse_mode(s: &str) -> PasteMode {
    match s.trim().to_ascii_lowercase().as_str() {
        "set_only" | "setonly" | "copy" => PasteMode::SetOnly,
        "type_in" | "typein" => PasteMode::TypeIn,
        _ => PasteMode::InjectCtrlV,
    }
}

/// Footer/tray label for the current paste mode.
fn mode_str(m: PasteMode) -> &'static str {
    match m {
        PasteMode::SetOnly => "set only",
        PasteMode::InjectCtrlV => "inject Ctrl+V",
        PasteMode::TypeIn => "type in",
    }
}

/// Tray mode item click cycles the paste mode.
#[cfg(windows)]
fn next_mode(m: PasteMode) -> PasteMode {
    match m {
        PasteMode::InjectCtrlV => PasteMode::SetOnly,
        PasteMode::SetOnly => PasteMode::TypeIn,
        PasteMode::TypeIn => PasteMode::InjectCtrlV,
    }
}

/// "YYYY-MM-DD" text inputs → inclusive SQLite bounds for `Filter.from/to`.
/// Empty or malformed input → None (no bound).
pub fn date_range_strings(from: &str, to: &str) -> (Option<String>, Option<String>) {
    let one = |s: &str, suffix: &str| -> Option<String> {
        let s = s.trim();
        let ok = s.len() == 10
            && s.is_ascii()
            && s.as_bytes()[4] == b'-'
            && s.as_bytes()[7] == b'-'
            && s[..4].bytes().all(|b| b.is_ascii_digit())
            && s[5..7].bytes().all(|b| b.is_ascii_digit())
            && s[8..].bytes().all(|b| b.is_ascii_digit());
        ok.then(|| format!("{s}{suffix}"))
    };
    (one(from, " 00:00:00"), one(to, " 23:59:59"))
}

/// UNIX-epoch seconds → "YYYYMMDD-HHMMSS" (UTC). Hand-rolled civil-from-days
/// (Hinnant) so backups add no date dependency. UTC, not local: backups are
/// chronological files, the exact wall clock is cosmetic.
pub fn backup_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    let s = secs % 86_400;
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

/// days since 1970-01-01 → (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|p| p.join("clip-line"))
}

pub fn default_config_path() -> anyhow::Result<PathBuf> {
    let d = config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
    std::fs::create_dir_all(&d)?;
    Ok(d.join("clipline.toml"))
}

pub fn default_db_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()
        .ok_or_else(|| anyhow::anyhow!("no config dir"))?
        .join("clipline.db"))
}

/// CLIP_LINE_DB env (file, or dir → dir/clipline.db) > `[storage] path` > default.
fn db_path(cfg_storage: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("CLIP_LINE_DB").map(PathBuf::from) {
        if p.extension().is_some() {
            return Ok(p);
        }
        std::fs::create_dir_all(&p)?;
        return Ok(p.join("clipline.db"));
    }
    match cfg_storage {
        Some(p) => Ok(p.to_path_buf()),
        None => default_db_path(),
    }
}

/// Exit-worthy single-instance check (SPEC cross-cutting). Windows: named
/// mutex, ERROR_ALREADY_EXISTS means a copy is running. Unix: O_EXCL lockfile.
/// ponytail: stale-lock cleanup after a crash needs PID liveness checking.
fn single_instance_guard() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::ERROR_ALREADY_EXISTS;
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::core::PCWSTR;
        let name: Vec<u16> = "Local\\clipLine-single-instance\0".encode_utf16().collect();
        unsafe {
            let _ = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
            if windows::Win32::Foundation::GetLastError() == ERROR_ALREADY_EXISTS {
                anyhow::bail!("clipLine is already running");
            }
        }
        // Hold the handle for the process lifetime (leak is intentional).
    }
    #[cfg(not(windows))]
    {
        use std::io::OpenOptions;
        let lock = config_dir()
            .ok_or_else(|| anyhow::anyhow!("no config dir"))?
            .join("clipline.lock");
        match OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(_) => {} // hold for process lifetime
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                anyhow::bail!("clipLine is already running (lockfile {})", lock.display());
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Replace (or insert) `[hotkeys] show = "…"` in the TOML text.
/// Minimal line-based edit: the file is ours, tiny and stable-shaped.
fn set_toml_show_hotkey(text: &str, hotkey: &str) -> anyhow::Result<String> {
    let line = format!("show = \"{hotkey}\"");
    let mut out = String::new();
    let mut replaced = false;
    let mut in_hotkeys = false;
    for l in text.lines() {
        let trimmed = l.trim_start();
        if trimmed.starts_with('[') {
            in_hotkeys = trimmed.starts_with("[hotkeys]");
        }
        if in_hotkeys && trimmed.starts_with("show") && trimmed.contains('=') {
            out.push_str(&line);
            replaced = true;
        } else {
            out.push_str(l);
        }
        out.push('\n');
    }
    if !replaced {
        if !out.contains("[hotkeys]") {
            out.push_str("[hotkeys]\n");
        }
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut import_ini = None;
    while let Some(a) = args.next() {
        if a == "--import-ini" {
            import_ini = args.next().map(PathBuf::from);
        }
    }
    if let Some(ini) = import_ini {
        let hks = clip_line_core::config::import_legacy_ini(&ini)?;
        if hks.is_empty() {
            println!("no hotkeys imported from {}", ini.display());
        }
        let mut best: Option<(u32, String)> = None; // lowest verified slot
        for hk in hks {
            // spec "Open": verify the modifier/key bitfield decodes back cleanly
            let ok = parse_hotkey(&hk.display)
                .map(|(m, v)| m == hk.modifier && v == hk.vkey)
                .unwrap_or(false);
            println!(
                "slot {}: {} (modifier={}, vkey={}) {}",
                hk.slot,
                hk.display,
                hk.modifier,
                hk.vkey,
                if ok { "verified" } else { "UNVERIFIED" }
            );
            if ok && best.as_ref().map(|(s, _)| hk.slot < *s).unwrap_or(true) {
                best = Some((hk.slot, hk.display.clone()));
            }
        }
        // Merge the first verified slot into clipline.toml [hotkeys] show —
        // without this the import only printed and discarded the legacy key.
        if let Some((slot, display)) = best {
            let cfg_path = default_config_path()?;
            let text = match std::fs::read_to_string(&cfg_path) {
                Ok(t) => t,
                Err(_) => DEFAULT_TOML.to_string(),
            };
            match set_toml_show_hotkey(&text, &display) {
                Ok(new_text) => {
                    std::fs::write(&cfg_path, new_text)?;
                    println!("slot {slot}: show hotkey written to {}", cfg_path.display());
                }
                Err(e) => println!("could not write {display:?} to config: {e}"),
            }
        } else {
            println!("no verifiable hotkey found; config left unchanged");
        }
        return Ok(());
    }

    // SPEC cross-cutting: single instance. Hand-rolled named mutex on
    // Windows, O_EXCL lockfile elsewhere (single-instance crate unmaintained).
    single_instance_guard()?;

    let cfg_path = default_config_path()?;
    let (cfg, extras) = load_config(&cfg_path)?;
    if parse_hotkey(&extras.hotkeys.show).is_none() {
        eprintln!(
            "config: hotkeys.show {:?} not understood; global hotkey disabled",
            extras.hotkeys.show
        );
    }

    let db = db_path(cfg.storage.path.as_deref())?;
    let store = Arc::new(Store::open(&db, StoreOpts::default())?);
    if let Err(e) = store.attach_snippets(&db.with_file_name("snippets.sdb")) {
        eprintln!("snippets disabled: {e}");
    }

    #[cfg(windows)]
    if let Err(e) = set_autostart(extras.autostart.enabled) {
        eprintln!("autostart: {e}");
    }
    // TODO(spec Q15): macOS Login Items / XDG autostart .desktop land with the
    // phase-3 non-Windows adapters.

    let suppress: SuppressSlot = Arc::new(Mutex::new(None));
    let (capture_tx, capture_rx) = std::sync::mpsc::channel::<String>();
    spawn_capture(store.clone(), suppress.clone(), capture_tx)?;
    // Capture-disabled messages land in the footer once; drained in logic().
    // Shared with the update-check channel pattern (Q11).
    CAPTURE_RX.set(Mutex::new(capture_rx)).ok();

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([860.0, 600.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "clipLine",
        opts,
        Box::new(move |_cc| {
            let mut app = App::new(store, &cfg, suppress, db);
            if extras.update.check_on_start {
                let tx = app.update_tx();
                app.spawn_update_check(tx);
            }
            // Both want the main thread (macOS tray rule); eframe's creator runs there.
            app.register_show_hotkey(&extras.hotkeys.show);
            #[cfg(windows)]
            app.init_tray();
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Registry Run key write/remove per `[autostart] enabled` at startup.
#[cfg(windows)]
fn set_autostart(enabled: bool) -> anyhow::Result<()> {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
    };
    let subkey: Vec<u16> = r"Software\Microsoft\Windows\CurrentVersion\Run"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value: Vec<u16> = "clipLine\0".encode_utf16().collect();
    unsafe {
        if enabled {
            let exe = std::env::current_exe()?;
            // Quote: Run entries with spaces in the path are split at the
            // first space by the shell otherwise.
            let quoted = format!("\"{}\"", exe.display());
            let data: Vec<u16> = quoted.encode_utf16().chain(std::iter::once(0)).collect();
            let r = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                windows::core::PCWSTR(subkey.as_ptr()),
                windows::core::PCWSTR(value.as_ptr()),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            );
            if r.0 != 0 {
                anyhow::bail!("RegSetKeyValueW error {}", r.0);
            }
        } else {
            let _ = RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                windows::core::PCWSTR(subkey.as_ptr()),
                windows::core::PCWSTR(value.as_ptr()),
            ); // missing value is fine
        }
    }
    Ok(())
}

/// Receiver end of the capture-status channel; taken once in main().
/// SPEC capture: Err only on fatal listener loss — surface it in the GUI.
static CAPTURE_RX: std::sync::OnceLock<Mutex<std::sync::mpsc::Receiver<String>>> =
    std::sync::OnceLock::new();

#[cfg(windows)]
fn spawn_capture(
    store: Arc<Store>,
    suppress: SuppressSlot,
    status_tx: std::sync::mpsc::Sender<String>,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            let mut cap = match clip_line_win::WinCapture::new() {
                Ok(c) => c,
                Err(e) => {
                    let _ = status_tx.send(format!("capture disabled: {e}"));
                    return;
                }
            };
            capture_loop(&mut cap, store, suppress, status_tx);
        })
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("spawn capture: {e}"))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn spawn_capture(
    store: Arc<Store>,
    suppress: SuppressSlot,
    status_tx: std::sync::mpsc::Sender<String>,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            use clip_line_core::capture::Capture;
            let mut cap = match clip_line_macos::MacCapture::new() {
                Ok(c) => c,
                Err(e) => {
                    let _ = status_tx.send(format!("capture disabled: {e}"));
                    return;
                }
            };
            capture_loop(&mut cap, store, suppress, status_tx);
        })
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("spawn capture: {e}"))
}

/// X11 vs Wayland decided at runtime: WAYLAND_DISPLAY wins, X11 falls back.
#[cfg(all(unix, not(target_os = "macos")))]
fn spawn_capture(
    store: Arc<Store>,
    suppress: SuppressSlot,
    status_tx: std::sync::mpsc::Sender<String>,
) -> anyhow::Result<()> {
    use clip_line_core::capture::Capture;
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                #[cfg(feature = "wayland")]
                match clip_line_wayland::WaylandCapture::new() {
                    Ok(mut c) => capture_loop(&mut c, store, suppress, status_tx),
                    Err(e) => {
                        // story 7: say why, not silence
                        let _ = status_tx.send(format!("capture disabled: {e}"));
                    }
                }
                #[cfg(not(feature = "wayland"))]
                let _ = (store, suppress, status_tx); // build lacks wayland support
            } else {
                #[cfg(feature = "x11")]
                match clip_line_x11::X11Capture::new() {
                    Ok(mut c) => capture_loop(&mut c, store, suppress, status_tx),
                    Err(e) => {
                        let _ = status_tx.send(format!("capture disabled: {e}"));
                    }
                }
                #[cfg(not(feature = "x11"))]
                let _ = (store, suppress, status_tx); // build lacks x11 support
            }
        })
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("spawn capture: {e}"))
}

#[cfg(windows)]
fn paste_on_this_os(data: &ClipData, mode: PasteMode) -> anyhow::Result<()> {
    use clip_line_core::paste::Paster;
    clip_line_win::WinPaster.paste(data, mode)
}

#[cfg(target_os = "macos")]
fn paste_on_this_os(data: &ClipData, mode: PasteMode) -> anyhow::Result<()> {
    use clip_line_core::paste::Paster;
    clip_line_macos::MacPaster.paste(data, mode)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn paste_on_this_os(data: &ClipData, mode: PasteMode) -> anyhow::Result<()> {
    use clip_line_core::paste::Paster;
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        clip_line_wayland::WaylandPaster.paste(data, mode)
    } else {
        // X11Paster owns the selection; cheap to build per paste.
        use clip_line_core::paste::Paster;
        clip_line_x11::X11Paster::new()?.paste(data, mode)
    }
}

/// Shared drain loop: skip self-writes, persist foreign events, report fatal loss.
fn capture_loop<C: clip_line_core::capture::Capture>(
    cap: &mut C,
    store: Arc<Store>,
    suppress: SuppressSlot,
    status_tx: std::sync::mpsc::Sender<String>,
) {
    loop {
        match cap.next_event() {
            Ok(ev) => {
                // Our own paste write: payload was armed before the
                // clipboard write; skip and clear the slot.
                let mine = clip_line_core::envelope::pack(&ev.blocks)
                    .ok()
                    .and_then(|p| {
                        let mut g = suppress.lock().unwrap();
                        match &*g {
                            Some(want) if *want == p => {
                                *g = None;
                                Some(())
                            }
                            _ => None,
                        }
                    });
                if mine.is_some() {
                    continue;
                }
                if let Err(e) = store.append(&ev) {
                    eprintln!("store append: {e}");
                }
            }
            Err(e) => {
                let _ = status_tx.send(format!("capture disabled: {e}"));
                break;
            }
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    History,
    Snippets,
}

struct App {
    store: Arc<Store>,
    suppress: SuppressSlot,
    mode: PasteMode,
    hide_after_ms: u64,
    /// Bumped on every show; pending hide timers check it before hiding.
    show_gen: Arc<AtomicU64>,
    tab: Tab,
    query: String,
    search_focus: bool,
    /// Focus+select the search box on next frame (hotkey/tray "show" request).
    select_after_show: bool,
    starred_only: bool,
    kind_sel: usize,
    date_from: String,
    date_to: String,
    rows: Vec<ClipRow>,
    selected: Option<usize>,
    dirty: bool,
    last_poll: Instant,
    status: String,
    snips: Vec<Snippet>,
    snip_name: String,
    snip_data: String,
    rename: Option<(i64, String)>,
    labels_open: bool,
    labels: Vec<String>,
    label_edit: Option<(String, String)>, // (tag, draft)
    // Global show/hide toggle (SPEC "ui": global-hotkey + tray-icon).
    gh_manager: Option<global_hotkey::GlobalHotKeyManager>,
    gh_show: Option<global_hotkey::hotkey::HotKey>,
    window_visible: bool,
    #[cfg(windows)]
    tray: Option<tray_icon::TrayIcon>,
    #[cfg(windows)]
    tray_mode_id: tray_icon::menu::MenuId,
    update: Option<String>, // footer line (Q11), e.g. "update: 0.2.0 available"
    update_tx: std::sync::mpsc::Sender<String>,
    update_rx: std::sync::mpsc::Receiver<String>,
    // SPEC phase 4 backup: db location for backup file names + exit flag.
    db_path: PathBuf,
    backup_on_exit: bool,
}

impl App {
    fn new(store: Arc<Store>, cfg: &Config, suppress: SuppressSlot, db_path: PathBuf) -> Self {
        let (update_tx, update_rx) = std::sync::mpsc::channel::<String>();
        App {
            mode: parse_mode(&cfg.paste.default_mode),
            hide_after_ms: cfg.ui.hide_after_ms,
            show_gen: Arc::new(AtomicU64::new(0)),
            store,
            suppress,
            tab: Tab::History,
            query: String::new(),
            search_focus: false,
            select_after_show: false,
            starred_only: false,
            kind_sel: 0,
            date_from: String::new(),
            date_to: String::new(),
            rows: Vec::new(),
            selected: None,
            dirty: true,
            last_poll: Instant::now(),
            status: String::new(),
            snips: Vec::new(),
            snip_name: String::new(),
            snip_data: String::new(),
            rename: None,
            labels_open: false,
            labels: Vec::new(),
            label_edit: None,
            gh_manager: None,
            gh_show: None,
            window_visible: true,
            #[cfg(windows)]
            tray: None,
            #[cfg(windows)]
            tray_mode_id: tray_icon::menu::MenuId::new("clipline-paste-mode"),
            update: None,
            update_tx,
            update_rx,
            db_path: db_path.clone(),
            backup_on_exit: cfg.storage.backup_on_exit,
        }
    }

    /// Q11 update check, off-thread; result lands in the footer, never a modal.
    fn spawn_update_check(&self, tx: std::sync::mpsc::Sender<String>) {
        let current = env!("CARGO_PKG_VERSION").to_string();
        std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || {
                let msg = match check_for_update(&current) {
                    Ok(Some(latest)) => format!("update: {latest} available"),
                    Ok(None) => "up to date".into(),
                    Err(_) => "update check failed".into(), // offline is normal
                };
                let _ = tx.send(msg);
            })
            .ok();
    }

    /// SPEC phase 4: snapshot to <db dir>/backup-YYYYMMDD-HHMMSS.db.
    fn run_backup(&mut self) {
        let dst = self
            .db_path
            .with_file_name(format!("backup-{}.db", backup_stamp(now_secs())));
        match self.store.backup_to(&dst) {
            Ok(()) => self.status = dst.display().to_string(),
            Err(e) => self.status = format!("backup failed: {e}"),
        }
    }

    fn poll_update(&mut self) {
        if let Ok(m) = self.update_rx.try_recv() {
            self.update = Some(m);
        }
    }

    /// One-shot capture-failure message from the capture thread (SPEC capture:
    /// "capture off must say why") into the status bar.
    fn poll_capture_status(&mut self) {
        if let Some(rx) = CAPTURE_RX.get() {
            if let Ok(m) = rx.lock().unwrap().try_recv() {
                self.status = m;
            }
        }
    }

    fn update_tx(&self) -> std::sync::mpsc::Sender<String> {
        self.update_tx.clone()
    }

    /// Global show/hide hotkey (spec: failure warns, app continues).
    /// Registration reuses `parse_hotkey` for validation, then the crate's own
    /// `FromStr` (same semantics: `Ctrl+Shift+Up`) for the actual binding.
    fn register_show_hotkey(&mut self, hotkey_str: &str) {
        if parse_hotkey(hotkey_str).is_none() {
            return;
        }
        let hk: global_hotkey::hotkey::HotKey = match hotkey_str.parse() {
            Ok(hk) => hk,
            Err(e) => {
                eprintln!("hotkey: parse {hotkey_str:?}: {e}");
                return;
            }
        };
        let manager = match global_hotkey::GlobalHotKeyManager::new() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("hotkey: manager init failed: {e}");
                return;
            }
        };
        match manager.register(hk) {
            Ok(_) => {
                self.gh_show = Some(hk);
                self.gh_manager = Some(manager);
            }
            Err(e) => eprintln!("hotkey: register {hotkey_str:?} failed: {e}"),
        }
    }

    /// Tray (Windows only; other OSes later per SPEC). Programmatically built
    /// 32x32 solid RGBA icon — no asset file. Menu: Show / paste mode / Quit.
    #[cfg(windows)]
    fn init_tray(&mut self) {
        use tray_icon::menu::{Menu, MenuItem};
        let rgba = [120u8, 30, 180, 255];
        let mut px = Vec::with_capacity(32 * 32 * 4);
        for _ in 0..32 * 32 {
            px.extend_from_slice(&rgba);
        }
        let icon = tray_icon::Icon::from_rgba(px, 32, 32).expect("32x32 RGBA valid");

        let show = MenuItem::with_id("clipline-show", "Show clipLine", true, None);
        let mode_item = MenuItem::with_id(
            self.tray_mode_id.clone(),
            format!("Paste mode: {}", mode_str(self.mode)),
            true,
            None,
        );
        let backup = MenuItem::with_id("clipline-backup", "Backup", true, None);
        let quit = MenuItem::with_id("clipline-quit", "Quit", true, None);
        let menu = Menu::new();
        let _ = menu.append_items(&[&show, &mode_item, &backup, &quit]);

        match tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("clipLine")
            .with_icon(icon)
            .build()
        {
            Ok(tray) => self.tray = Some(tray),
            Err(e) => eprintln!("tray: init failed: {e}"),
        }
    }

    /// Show/hide toggle shared by hotkey, tray menu and tray click.
    fn toggle_visible(&mut self, ctx: &Context) {
        if self.window_visible {
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            self.window_visible = false;
        } else {
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
            self.select_search();
            self.window_visible = true;
        }
    }

    fn show_main(&mut self, ctx: &Context) {
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        self.select_search();
        self.window_visible = true;
    }

    /// Drain global hotkey + tray channels (handler fns only request a repaint).
    fn handle_global_events(&mut self, ctx: &Context) {
        use global_hotkey::GlobalHotKeyEvent;
        if let Some(hk) = self.gh_show {
            while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
                if ev.state == global_hotkey::HotKeyState::Pressed && ev.id() == hk.id() {
                    self.toggle_visible(ctx);
                }
            }
        }
        #[cfg(windows)]
        if self.tray.is_some() {
            use tray_icon::menu::MenuEvent;
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                match ev.id().as_ref() {
                    "clipline-show" => self.show_main(ctx),
                    "clipline-paste-mode" => {
                        self.mode = next_mode(self.mode);
                        if let Some(tray) = &self.tray {
                            tray.set_menu(Some(Box::new(Self::tray_menu(self.mode))));
                        }
                    }
                    "clipline-backup" => self.run_backup(),
                    "clipline-quit" => ctx.send_viewport_cmd(ViewportCommand::Close),
                    _ => {}
                }
            }
            while let Ok(ev) = tray_icon::TrayIconEvent::receiver().try_recv() {
                if let tray_icon::TrayIconEvent::Click { button, .. } = ev {
                    if button == tray_icon::MouseButton::Left {
                        self.toggle_visible(ctx);
                    }
                }
            }
        }
    }

    #[cfg(windows)]
    fn tray_menu(mode: PasteMode) -> tray_icon::menu::Menu {
        use tray_icon::menu::{Menu, MenuId, MenuItem};
        let show = MenuItem::with_id("clipline-show", "Show clipLine", true, None);
        let mode_item = MenuItem::with_id(
            MenuId::new("clipline-paste-mode"),
            format!("Paste mode: {}", mode_str(mode)),
            true,
            None,
        );
        let backup = MenuItem::with_id("clipline-backup", "Backup", true, None);
        let quit = MenuItem::with_id("clipline-quit", "Quit", true, None);
        let menu = Menu::new();
        let _ = menu.append_items(&[&show, &mode_item, &backup, &quit]);
        menu
    }

    /// Select the search field: focus it and select its text (hotkey show).
    fn select_search(&mut self) {
        self.search_focus = false;
        self.select_after_show = true;
        // a fresh show cancels any pending hide timer
        self.show_gen.fetch_add(1, Ordering::SeqCst);
    }

    fn refresh_if_needed(&mut self) {
        if self.dirty || self.last_poll.elapsed() >= Duration::from_secs(1) {
            self.last_poll = Instant::now();
            self.dirty = false;
            let (from, to) = date_range_strings(&self.date_from, &self.date_to);
            let f = Filter {
                query: (!self.query.trim().is_empty()).then(|| self.query.trim().to_string()),
                starred_only: self.starred_only,
                kind: match self.kind_sel {
                    1 => Some(1), // text
                    2 => Some(3), // url
                    3 => Some(4), // files
                    4 => Some(5), // image
                    _ => None,
                },
                from,
                to,
                limit: 500,
                ..Default::default()
            };
            match self.store.list(&f) {
                Ok(rows) => {
                    if self.selected.is_some_and(|s| s >= rows.len()) {
                        self.selected = rows.len().checked_sub(1);
                    }
                    self.rows = rows;
                }
                Err(e) => self.status = format!("list: {e}"),
            }
            if self.tab == Tab::Snippets {
                self.reload_snips();
            }
        }
    }

    fn reload_snips(&mut self) {
        match self.store.snippet_list(0) {
            Ok(v) => self.snips = v,
            Err(e) => self.status = format!("snippets: {e}"),
        }
    }

    fn handle_keys(&mut self, ctx: &Context) {
        if self.search_focus {
            // typing a query then Enter pastes the top hit — parity with the original app
            if ctx.input(|i| i.key_pressed(Key::Enter)) {
                self.paste_selected(ctx);
            }
            return;
        }
        // any focused widget (date fields etc.) owns the keyboard: typing
        // 1-9 there must not quick-pick a row, Enter must not paste.
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let mut delta = 0isize;
        ctx.input(|i| {
            if i.key_pressed(Key::ArrowDown) {
                delta = 1;
            }
            if i.key_pressed(Key::ArrowUp) {
                delta = -1;
            }
        });
        if delta != 0 && !self.rows.is_empty() {
            let max = (self.rows.len() - 1) as isize;
            let cur = self.selected.map(|s| s as isize).unwrap_or(-1) + delta;
            self.selected = Some(cur.clamp(0, max) as usize);
        }
        // story 16: quick-pick — digit N pastes the Nth visible row (only while
        // history is on screen and the row exists; no-op otherwise).
        let mut quick: Option<usize> = None;
        ctx.input(|i| {
            for (n, k) in [
                Key::Num1,
                Key::Num2,
                Key::Num3,
                Key::Num4,
                Key::Num5,
                Key::Num6,
                Key::Num7,
                Key::Num8,
                Key::Num9,
            ]
            .iter()
            .enumerate()
            {
                if i.key_pressed(*k) {
                    quick = Some(n);
                }
            }
        });
        if let Some(n) = quick {
            if self.tab == Tab::History {
                if let Some(r) = self.rows.get(n).cloned() {
                    let mode = self.mode;
                    self.paste_row(ctx, r.id, mode, false);
                }
            }
        }
        if ctx.input(|i| i.key_pressed(Key::Enter)) {
            self.paste_selected(ctx);
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            // TODO(spec ui): Esc closes the hotkey popup in phase 2; v1 hides the window.
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }
    }

    fn paste_selected(&mut self, ctx: &Context) {
        let idx = self.selected.unwrap_or(0);
        if let Some(r) = self.rows.get(idx) {
            let id = r.id;
            let mode = self.mode;
            self.paste_row(ctx, id, mode, false);
        }
    }

    fn paste_row(&mut self, ctx: &Context, id: i64, mode: PasteMode, plain: bool) {
        match self.do_paste(id, mode, plain) {
            Ok(()) => {
                if mode != PasteMode::SetOnly {
                    hide_soon(
                        ctx.clone(),
                        self.hide_after_ms,
                        self.show_gen.clone(),
                        self.show_gen.load(Ordering::SeqCst),
                    );
                }
            }
            Err(e) => {
                // Disarm suppression: the clipboard write may have failed, and
                // an armed slot would swallow the next identical foreign copy.
                *self.suppress.lock().unwrap() = None;
                self.status = format!("paste: {e}");
            }
        }
    }

    fn do_paste(&mut self, id: i64, mode: PasteMode, plain: bool) -> anyhow::Result<()> {
        let mut blocks = self.store.load_data(id)?;
        if plain {
            blocks.retain(|b| b.format == clip_line_core::Format::UnicodeText);
            if blocks.is_empty() {
                anyhow::bail!("no text block in this clip");
            }
        }
        // Arm suppression BEFORE the clipboard write — see SuppressSlot.
        // Disarm on failure so a stale slot cannot swallow the next foreign copy.
        let payload = clip_line_core::envelope::pack(&blocks)?;
        *self.suppress.lock().unwrap() = Some(payload);
        let r = paste_on_this_os(&ClipData { blocks }, mode);
        if r.is_err() {
            *self.suppress.lock().unwrap() = None;
        }
        r
    }

    fn reload_labels(&mut self) {
        match self.store.tag_list() {
            Ok(v) => self.labels = v,
            Err(e) => self.status = format!("tags: {e}"),
        }
    }

    /// Labels manager popup (story 12): list all tags, rename or delete.
    fn labels_ui(&mut self, ctx: &egui::Context) {
        let mut open = self.labels_open;
        egui::Window::new(t("Labels"))
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .show(ui, |ui| {
                        if self.labels.is_empty() {
                            ui.weak(t("No labels"));
                        }
                        for tag in self.labels.clone() {
                            let mut editing = false;
                            ui.horizontal(|ui| {
                                if ui.button("x").clicked() {
                                    // ponytail: no confirm — bulk delete is recoverable
                                    // by re-tagging rows; add undo when store grows it.
                                    if let Err(e) = self.store.tag_delete(&tag) {
                                        self.status = format!("tag_delete: {e}");
                                    }
                                    self.dirty = true;
                                    self.reload_labels();
                                    return;
                                }
                                if let Some((t0, draft)) = self.label_edit.clone() {
                                    if t0 == tag {
                                        editing = true;
                                        let mut d = draft.clone();
                                        let r = ui.text_edit_singleline(&mut d);
                                        if r.changed() {
                                            self.label_edit = Some((tag.clone(), d));
                                        }
                                        if ui.button(t("Apply")).clicked() {
                                            let new = draft.trim().to_string();
                                            if !new.is_empty() {
                                                if let Err(e) = self.store.tag_rename(&tag, &new) {
                                                    self.status = format!("tag_rename: {e}");
                                                }
                                            }
                                            self.label_edit = None;
                                            self.dirty = true;
                                            self.reload_labels();
                                        }
                                        if ui.button(t("Cancel")).clicked() {
                                            self.label_edit = None;
                                        }
                                    }
                                }
                                if !editing {
                                    ui.label(&tag);
                                }
                                if !editing
                                    && ui.button(t("Rename")).clicked()
                                    && self.label_edit.is_none()
                                {
                                    self.label_edit = Some((tag.clone(), tag.clone()));
                                }
                            });
                        }
                    });
            });
        self.labels_open = open;
    }

    fn history_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let n = self.rows.len();
        let selected = self.selected;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(Column::exact(24.0)) // star
            .column(Column::exact(18.0)) // quick-pick # (story 16)
            .column(Column::exact(140.0)) // created
            .column(Column::exact(56.0)) // kind
            .column(Column::remainder()) // title
            .column(Column::exact(90.0)) // app
            .body(|body| {
                body.rows(20.0, n, |mut row| {
                    let idx = row.index();
                    let Some(r0) = self.rows.get(idx) else { return };
                    let r = r0.clone(); // visible rows only: cheap
                    row.set_selected(selected == Some(idx));
                    row.col(|ui| {
                        let star = if r.starred { "\u{2605}" } else { "\u{2606}" };
                        if ui.label(star).clicked() {
                            let _ = self.store.set_star(r.id, !r.starred);
                            self.dirty = true;
                        }
                    });
                    row.col(|ui| {
                        // 1-9 label for the visible row (quick-pick target)
                        ui.weak(if idx < 9 {
                            ((b'1' + idx as u8) as char).to_string()
                        } else {
                            " ".into()
                        });
                    });
                    row.col(|ui| {
                        ui.label(&r.created);
                    });
                    row.col(|ui| {
                        ui.label(kind_name(r.kind));
                    });
                    row.col(|ui| {
                        ui.label(&r.title);
                    });
                    row.col(|ui| {
                        ui.label(&r.appname);
                    });
                    let resp = row.response();
                    if resp.clicked() {
                        self.selected = Some(idx);
                        let mode = self.mode;
                        self.paste_row(&ctx, r.id, mode, false);
                    }
                    resp.context_menu(|ui| {
                        let mode = self.mode;
                        if ui.button(t("Paste")).clicked() {
                            self.paste_row(ui.ctx(), r.id, mode, false);
                            ui.close();
                        }
                        if ui.button(t("Paste plain text")).clicked() {
                            self.paste_row(ui.ctx(), r.id, mode, true);
                            ui.close();
                        }
                        if ui.button(t("Copy only")).clicked() {
                            self.paste_row(ui.ctx(), r.id, PasteMode::SetOnly, false);
                            ui.close();
                        }
                        if ui.button(t("Type in")).clicked() {
                            self.paste_row(ui.ctx(), r.id, PasteMode::TypeIn, false);
                            ui.close();
                        }
                        if ui
                            .button(if r.starred { t("Unstar") } else { t("Star") })
                            .clicked()
                        {
                            let _ = self.store.set_star(r.id, !r.starred);
                            self.dirty = true;
                            ui.close();
                        }
                        if ui.button(t("Delete")).clicked() {
                            let _ = self.store.delete(&[r.id]);
                            self.dirty = true;
                            ui.close();
                        }
                    });
                });
            });
    }

    fn snippets_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(t("Name"));
            ui.text_edit_singleline(&mut self.snip_name);
            ui.label(t("Text"));
            ui.add(egui::TextEdit::singleline(&mut self.snip_data).desired_width(f32::INFINITY));
            if ui.button(t("Add snippet")).clicked() && !self.snip_name.trim().is_empty() {
                match self
                    .store
                    .snippet_add(0, self.snip_name.trim(), self.snip_data.trim())
                {
                    Ok(_) => {
                        self.snip_name.clear();
                        self.snip_data.clear();
                        self.reload_snips();
                    }
                    Err(e) => self.status = format!("snippet add: {e}"),
                }
            }
        });
        ui.separator();
        let n = self.snips.len();
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::exact(180.0))
            .column(Column::remainder())
            .column(Column::exact(80.0))
            .column(Column::exact(80.0))
            .body(|body| {
                body.rows(22.0, n, |mut row| {
                    let idx = row.index();
                    let Some(s0) = self.snips.get(idx) else {
                        return;
                    };
                    let s = s0.clone();
                    row.col(|ui| {
                        if self.rename.as_ref().is_some_and(|(id, _)| *id == s.id) {
                            let Some((_, buf)) = self.rename.as_mut() else {
                                return;
                            };
                            let r = ui.add(egui::TextEdit::singleline(buf).desired_width(160.0));
                            if r.lost_focus() && !buf.trim().is_empty() {
                                let name = std::mem::take(&mut self.rename).unwrap().1;
                                if let Err(e) = self.store.snippet_rename(s.id, name.trim()) {
                                    self.status = format!("snippet rename: {e}");
                                }
                                self.reload_snips();
                            }
                        } else {
                            ui.label(&s.name);
                        }
                    });
                    row.col(|ui| {
                        ui.label(&s.data);
                    });
                    row.col(|ui| {
                        if ui.button(t("Rename")).clicked() {
                            self.rename = Some((s.id, s.name.clone()));
                        }
                    });
                    row.col(|ui| {
                        if ui.button(t("Delete")).clicked() {
                            if let Err(e) = self.store.snippet_delete(s.id) {
                                self.status = format!("snippet delete: {e}");
                            }
                            self.reload_snips();
                        }
                    });
                });
            });
    }
}

/// Sleep `ms` then hide the main window (pasting keeps the target window's
/// focus this way). The `Context` clone is safe for cross-thread use.
fn hide_soon(ctx: Context, ms: u64, gen_slot: Arc<AtomicU64>, my_gen: u64) {
    std::thread::Builder::new()
        .name("hide-timer".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            // A newer show (gen bumped) cancels this stale hide.
            if gen_slot.load(Ordering::SeqCst) == my_gen {
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
        })
        .ok();
}

impl eframe::App for App {
    /// Runs even while the window is hidden (0.37 trait) — keeps history fresh.
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.poll_update();
        self.poll_capture_status();
        self.refresh_if_needed();
        self.handle_global_events(ctx);
        self.handle_keys(ctx);
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    /// SPEC phase 4 backup_on_exit: eframe non-glow trait (0.37) — runs once on
    /// shutdown, after save; errors to stderr only (window is already gone).
    fn on_exit(&mut self) {
        if self.backup_on_exit {
            let dst = self
                .db_path
                .with_file_name(format!("backup-exit-{}.db", backup_stamp(now_secs())));
            if let Err(e) = self.store.backup_to(&dst) {
                eprintln!("backup_on_exit: {e}");
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.search_focus = false;
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::History, t("History"));
                ui.selectable_value(&mut self.tab, Tab::Snippets, t("Snippets"));
            });
        });
        if self.tab == Tab::History {
            egui::Panel::top("filter").show(ui, |ui| {
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text(t("Search"))
                            .desired_width(220.0),
                    );
                    if self.select_after_show {
                        // ponytail: select-all, not just focus — needs a cursor-state
                        // edit via egui::text_edit; plain focus is enough for v1.
                        r.request_focus();
                        self.select_after_show = false;
                    }
                    self.search_focus = r.has_focus();
                    if r.changed() {
                        self.dirty = true;
                    }
                    if ui.checkbox(&mut self.starred_only, t("Starred")).changed() {
                        self.dirty = true;
                    }
                    if ui.button(t("Labels")).clicked() {
                        self.labels_open = !self.labels_open;
                        if self.labels_open {
                            self.reload_labels();
                        }
                    }
                    let kinds = [t("All"), t("Text"), t("Url"), t("Files"), t("Image")];
                    let cr = egui::ComboBox::from_id_salt("kind")
                        .selected_text(kinds[self.kind_sel.min(4)])
                        .show_ui(ui, |ui| {
                            for (i, k) in kinds.iter().enumerate() {
                                ui.selectable_value(&mut self.kind_sel, i, *k);
                            }
                        });
                    if cr.response.changed() {
                        self.dirty = true;
                    }
                    for (buf, hint) in [
                        (&mut self.date_from, t("From YYYY-MM-DD")),
                        (&mut self.date_to, t("To YYYY-MM-DD")),
                    ] {
                        let r = ui.add(
                            egui::TextEdit::singleline(buf)
                                .hint_text(hint)
                                .desired_width(110.0),
                        );
                        if r.changed() {
                            self.dirty = true;
                        }
                    }
                });
            });
        }
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("{} clips", self.rows.len()));
                ui.separator();
                ui.label(mode_str(self.mode));
                ui.separator();
                // SPEC phase 4: manual backup, same dir as the DB, status reports path.
                if ui.button(t("Backup")).clicked() {
                    self.run_backup();
                }
                ui.separator();
                ui.label(self.status.as_str());
                // Q11: update status lives in the footer — never a modal, no telemetry.
                if let Some(u) = &self.update {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(u);
                    });
                }
            });
        });
        if self.labels_open {
            self.labels_ui(ui.ctx());
        }
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::History => self.history_ui(ui),
            Tab::Snippets => self.snippets_ui(ui),
        });
    }
}

fn kind_name(k: i64) -> &'static str {
    match k {
        0 => "image*",
        1 => "text",
        2 => "text2",
        3 => "url",
        4 => "files",
        5 => "image",
        _ => "?",
    }
}
// --- Q11: GitHub Releases update check (footer status only, no telemetry) ---

pub const RELEASES_URL: &str = "https://api.github.com/repos/katsugtgz/clip-line/releases/latest";

/// Fetch latest tag from GitHub Releases. `current` filters stale tags.
/// Result surfaces in the footer only (spec Q11: opt-out, no modal, no telemetry).
pub fn check_for_update(current: &str) -> anyhow::Result<Option<String>> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .build();
    let res = agent
        .get(RELEASES_URL)
        .set(
            "User-Agent",
            concat!("clip-line/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .map_err(|e| anyhow::anyhow!("releases api: {e}"))?;
    let mut body = String::new();
    res.into_reader()
        .take(64 * 1024)
        .read_to_string(&mut body)
        .map_err(|e| anyhow::anyhow!("read body: {e}"))?;
    parse_latest_tag(&body).ok_or_else(|| anyhow::anyhow!("no tag_name"))?; // shape check
    Ok(latest_if_newer(current, &body))
}

/// `"tag_name": "v0.2.0"` → `Some("v0.2.0")`; sample-shape JSON, hand-rolled scan
/// (top-level `"tag_name"` string; JSON escaping in tags not supported).
pub fn parse_latest_tag(body: &str) -> Option<String> {
    let k = body.find("\"tag_name\"")?;
    let rest = &body[k..];
    let colon = rest.find(':')?;
    let q1 = colon + rest[colon + 1..].find('"')? + 1;
    let q2 = q1 + 1 + rest[q1 + 1..].find('"')?;
    let tag = &rest[q1 + 1..q2];
    (!tag.is_empty()).then(|| tag.to_string())
}

/// Pure compare: `"0.1.0" < "0.2.0"`, `"0.1.0" < "0.1.1"`, `"0.1.0" < "0.1.0-rc1"`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let nums = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(|c: char| !c.is_ascii_digit())
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    nums(latest) > nums(current)
}

/// Parse body → latest tag, only if newer than `current` (`None` = up to date).
/// Split out so tests exercise the pure path without HTTP.
pub fn latest_if_newer(current: &str, body: &str) -> Option<String> {
    let tag = parse_latest_tag(body)?;
    let clean = tag.trim_start_matches('v');
    is_newer(clean, current).then_some(tag)
}
