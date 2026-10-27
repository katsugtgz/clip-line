//! TOML config + one-time legacy `clipdiary.cfg` import (subset).

use anyhow::Context;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub storage: Storage,
    pub paste: Paste,
    pub ui: Ui,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Storage {
    pub path: Option<std::path::PathBuf>,
    /// Back up the main DB on app exit (SPEC phase 4). Default false.
    #[serde(default)]
    pub backup_on_exit: bool,
    /// Q14 phase 2: extra read/write history DBs, each shown as its own tab.
    pub extra_dbs: Vec<std::path::PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Paste {
    pub default_mode: String,
}

impl Default for Paste {
    fn default() -> Self {
        Self {
            default_mode: "set_only".into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Ui {
    pub hide_after_ms: u64,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            hide_after_ms: 1200,
        }
    }
}

/// One imported legacy hotkey slot: 13 slots in `HotKeysList`, format
/// `id,modifier,vkey;` per FINDINGS.md. Modifier is a bitfield:
/// Ctrl=2, Shift=4, Alt=1, Win=8 (FINDINGS.md, unverified upstream).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LegacyHotkey {
    pub slot: u32,
    /// Human form, e.g. "Ctrl+Shift+V".
    pub display: String,
    pub modifier: u32,
    pub vkey: u32,
}

/// Ctrl=2, Shift=4, Alt=1, Win=8 → "Ctrl+Shift+" (trailing '+').
fn decode_modifier(bits: u32) -> Option<String> {
    let mut s = String::new();
    if bits & 1 != 0 {
        s.push_str("Alt+");
    }
    if bits & 2 != 0 {
        s.push_str("Ctrl+");
    }
    if bits & 4 != 0 {
        s.push_str("Shift+");
    }
    if bits & 8 != 0 {
        s.push_str("Win+");
    }
    if bits & !0xF != 0 {
        return None; // unknown bits → caller falls back
    }
    Some(s)
}

/// Virtual-key name for the printable/common range; None elsewhere (caller falls back).
fn key_name(vkey: u32) -> Option<String> {
    match vkey {
        0x30..=0x39 => Some((vkey as u8 as char).to_string()), // 0-9
        0x41..=0x5A => Some((vkey as u8 as char).to_string()), // A-Z
        0x70..=0x87 => Some(format!("F{}", vkey - 0x6F)),      // F1-F24
        0x08 => Some("Backspace".into()), // shared VK table with the app parser
        0x09 => Some("Tab".into()),
        0x0D => Some("Enter".into()),
        0x13 => Some("Pause".into()),
        0x14 => Some("CapsLock".into()),
        0x1B => Some("Escape".into()),
        0x20 => Some("Space".into()),
        0x21 => Some("PageUp".into()),
        0x22 => Some("PageDown".into()),
        0x23 => Some("End".into()),
        0x24 => Some("Home".into()),
        0x25 => Some("Left".into()),
        0x26 => Some("Up".into()),
        0x27 => Some("Right".into()),
        0x28 => Some("Down".into()),
        0x2D => Some("Insert".into()),
        0x2E => Some("Delete".into()),
        _ => None,
    }
}

/// Parses `HotKeysList=1,6,86;2,2,162;` — semicolon-separated triplets.
/// Unparseable slots are skipped (spec: fall back to defaults on parse failure).
pub fn parse_hotkeys_list(list: &str) -> Vec<LegacyHotkey> {
    let mut out = Vec::new();
    for entry in list.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut it = entry.split(',');
        let (Ok(slot), Ok(modifier), Ok(vkey), None) = (
            it.next().map(str::trim).unwrap_or("").parse::<u32>(),
            it.next().map(str::trim).unwrap_or("").parse::<u32>(),
            it.next().map(str::trim).unwrap_or("").parse::<u32>(),
            it.next(),
        ) else {
            continue; // malformed triplet → skip slot
        };
        let (Some(mods), Some(key)) = (decode_modifier(modifier), key_name(vkey)) else {
            continue;
        };
        if !(1..=13).contains(&slot) {
            // HotKeysList is 13 slots; anything else is corrupt data, not a binding
            continue;
        }
        out.push(LegacyHotkey {
            slot,
            display: format!("{mods}{key}"),
            modifier,
            vkey,
        });
    }
    out
}

/// Reads a legacy `clipdiary.cfg` `[Options]` subset. Only hotkeys import today.
pub fn import_legacy_ini(path: &Path) -> anyhow::Result<Vec<LegacyHotkey>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut list = String::new();
    let mut in_options = false;
    for l in text.lines() {
        let l = l.trim();
        if let Some(section) = l.strip_prefix('[') {
            in_options = section
                .trim_end()
                .trim_end_matches(']')
                .eq_ignore_ascii_case("Options");
            continue;
        }
        if !in_options {
            continue; // the hotkeys contract is [Options]-only; other sections can carry the same key
        }
        if let Some(v) = l
            .strip_prefix("HotKeysList")
            .and_then(|r| r.strip_prefix('='))
        {
            list = v.to_string();
        }
    }
    Ok(parse_hotkeys_list(&list))
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Ok(toml::from_str(&text)?)
    }
}
