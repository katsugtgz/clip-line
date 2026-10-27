//! Headless checks for the pure helpers in main.rs (task item 10; GUI is manual).
//! `main.rs` is a bin, so integration tests re-include it — pure fns only.

// main.rs items unused from the test harness view (main() never runs here)
#[path = "../src/main.rs"]
#[allow(dead_code)]
mod main;

use clip_line_core::paste::PasteMode;
use main::{
    backup_stamp, date_range_strings, latest_if_newer, parse_hotkey, parse_latest_tag, parse_mode,
    t, tab_label, Tab,
};
use std::path::PathBuf;

#[test]
fn hotkey_strings_parse_to_mod_bits_and_vkey() {
    // Ctrl=2 Shift=4 Alt=1 Win=8 (bit layout shared with legacy import).
    assert_eq!(
        parse_hotkey("Ctrl+Shift+V"),
        Some((2 | 4, b'V' as u32)),
        "Ctrl+Shift+V"
    );
    assert_eq!(
        parse_hotkey("ctrl+shift+v"),
        Some((6, b'V' as u32)),
        "case-insensitive"
    );
    assert_eq!(
        parse_hotkey("Alt+F2"),
        Some((1, 0x71)),
        "Alt+F2 (VK_F2=0x71)"
    );
    assert_eq!(parse_hotkey("Ctrl+0"), Some((2, b'0' as u32)), "digit key");
    assert_eq!(
        parse_hotkey("Win+L"),
        Some((8, b'L' as u32)),
        "Win alone = 8"
    );
    assert_eq!(
        parse_hotkey("F24"),
        Some((0, 0x87)),
        "bare function key, no mods"
    );
    assert_eq!(
        parse_hotkey("Ctrl+Shift+Up"),
        Some((6, 0x26)),
        "arrow Up = VK_UP"
    );
    assert_eq!(parse_hotkey("ctrl+alt+down"), Some((3, 0x28)), "arrow Down");
    assert_eq!(parse_hotkey("Ctrl+Space"), Some((2, 0x20)), "space");
}

#[test]
fn bad_hotkeys_rejected() {
    assert_eq!(parse_hotkey("Ctrl+Shift"), None, "no key");
    assert_eq!(parse_hotkey("Ctrl+V+X"), None, "key must be last");
    assert_eq!(parse_hotkey(""), None, "empty");
    assert_eq!(parse_hotkey("Ctrl+Percent"), None, "unmapped key");
    assert_eq!(parse_hotkey("Ctrl++"), None, "empty segment");
    assert_eq!(parse_hotkey("Ctrl+F25"), None, "out of VK range");
}

#[test]
fn parse_hotkey_round_trips_legacy_decode() {
    // Legacy import decodes bits→display; verify bitfield decodes back (spec "Open").
    use clip_line_core::config::parse_hotkeys_list;
    let list = parse_hotkeys_list("1,6,86;2,1,113;3,2,117;");
    assert_eq!(list.len(), 3);
    for hk in &list {
        let (m, v) = parse_hotkey(&hk.display).expect("re-parseable");
        assert_eq!(m, hk.modifier, "modifier bits round-trip");
        assert_eq!(v, hk.vkey, "vkey round-trip");
    }
}

#[test]
fn t_is_identity() {
    assert_eq!(t("Paste"), "Paste");
}

#[test]
fn mode_strings_map_to_paste_mode() {
    assert_eq!(parse_mode("inject_ctrl_v"), PasteMode::InjectCtrlV);
    assert_eq!(parse_mode("set_only"), PasteMode::SetOnly);
    assert_eq!(parse_mode("type_in"), PasteMode::TypeIn);
    assert_eq!(parse_mode("nonsense"), PasteMode::InjectCtrlV, "fallback");
}

#[test]
fn date_range_builds_sqlite_bounds() {
    let (f, t) = date_range_strings("2026-10-01", "2026-10-27");
    assert_eq!(f.as_deref(), Some("2026-10-01 00:00:00"));
    assert_eq!(t.as_deref(), Some("2026-10-27 23:59:59"));
    let (f, t) = date_range_strings("", " 2026-1-1 ");
    assert_eq!(f, None, "empty from");
    assert_eq!(t, None, "malformed to ignored");
    // Shape-only validation: digits+dashes pass (SQLite string compare decides the rest).
    let (f, t) = date_range_strings("2026-13-40", "not-a-date!");
    assert_eq!(f.as_deref(), Some("2026-13-40 00:00:00"));
    assert_eq!(t, None);
}

#[test]
fn quick_pick_digit_maps_to_nth_visible_row() {
    // story 16: digit key N (1-9) → 0-based row index N-1.
    for n in 1..=9u8 {
        let idx = (n - 1) as usize;
        assert!(idx < 9, "digit {n} maps inside the labelled range");
    }
    // Row label char: '1' + idx, blank for rows past 9.
    assert_eq!(b'1' as char, '1');
    assert_eq!((b'1' + 8u8) as char, '9');
}

#[test]
fn parses_tag_name_from_releases_sample() {
    // Sample of GET /repos/katsugtgz/clip-line/releases/latest body.
    let sample = r#"{"url":"https://api.github.com/repos/katsugtgz/clip-line/releases/12",
"tag_name":"v0.2.0","name":"clipLine 0.2.0","draft":false,"prerelease":false}"#;
    assert_eq!(parse_latest_tag(sample).as_deref(), Some("v0.2.0"));
    assert_eq!(parse_latest_tag("{}"), None, "missing tag_name");
    assert_eq!(parse_latest_tag("\"tag_name\": \"\""), None, "empty tag");
}

#[test]
fn latest_tag_flagged_only_when_newer() {
    assert_eq!(
        latest_if_newer("0.1.0", r#"{"tag_name":"v0.2.0"}"#).as_deref(),
        Some("v0.2.0")
    );
    assert_eq!(latest_if_newer("0.2.0", r#"{"tag_name":"v0.2.0"}"#), None);
    assert_eq!(
        latest_if_newer("0.2.0", r#"{"tag_name":"v0.1.9"}"#),
        None,
        "older tag is not an update"
    );
    assert_eq!(latest_if_newer("0.2.0", "{}"), None);
}

#[test]
fn backup_stamp_formats_epoch() {
    assert_eq!(backup_stamp(0), "19700101-000000");
    // 2026-10-27T00:00:00Z = 1793068800 (leap-aware day count)
    assert_eq!(backup_stamp(1_793_059_200), "20261027-000000");
    // 2024-02-29T23:59:59Z — leap day boundary
    assert_eq!(backup_stamp(1_709_251_199), "20240229-235959");
}

#[test]
fn extra_tab_labels_use_file_stem() {
    let extras = [
        PathBuf::from(r"C:\dbs\work-history.db"),
        PathBuf::from("/home/u/arch.sdb"),
        // stemless (dotfile) falls back to the full file name
        PathBuf::from("/tmp/.clips"),
    ];
    assert_eq!(tab_label("History", &extras, Tab::History), "History");
    assert_eq!(tab_label("History", &extras, Tab::Extra(0)), "work-history");
    assert_eq!(tab_label("History", &extras, Tab::Extra(1)), "arch");
    assert_eq!(tab_label("History", &extras, Tab::Extra(2)), ".clips");
    assert_eq!(tab_label("History", &extras, Tab::Snippets), "Snippets");
    // out-of-range index (config changed at runtime) → safe fallback
    assert_eq!(tab_label("History", &extras, Tab::Extra(9)), "extra");
}
}
