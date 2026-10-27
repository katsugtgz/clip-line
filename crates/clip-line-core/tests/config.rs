use clip_line_core::config::{Config, Paste, Storage, Ui, import_legacy_ini, parse_hotkeys_list};

fn tmp(tag: &str, name: &str, text: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("clipline-cfg-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p
}

#[test]
fn toml_all_sections() {
    let p = tmp(
        "toml",
        "clipline.toml",
        r#"
[storage]
path = 'C:\History\diary.db'

[paste]
default_mode = "inject_ctrl_v"

[ui]
hide_after_ms = 2000
"#,
    );
    let c = Config::load(&p).unwrap();
    assert_eq!(
        c.storage.path.as_deref(),
        Some(std::path::Path::new(r"C:\History\diary.db"))
    );
    assert_eq!(c.paste.default_mode, "inject_ctrl_v");
    assert_eq!(c.ui.hide_after_ms, 2000);
}

#[test]
fn toml_backup_on_exit() {
    let p = tmp("bk", "clipline.toml", "[storage]\nbackup_on_exit = true\n");
    let c = Config::load(&p).unwrap();
    assert!(c.storage.backup_on_exit);

    let p = tmp("bk2", "clipline.toml", "");
    assert!(
        !Config::load(&p).unwrap().storage.backup_on_exit,
        "default off"
    );
}

#[test]
fn toml_defaults_when_empty() {
    let p = tmp("empty", "clipline.toml", "");
    let c = Config::load(&p).unwrap();
    assert!(c.storage.path.is_none());
    assert_eq!(c.paste.default_mode, "set_only");
    assert_eq!(c.ui.hide_after_ms, 1200);
    // sections default-constructible (serde(default) on struct)
    let _ = (Storage::default(), Paste::default(), Ui::default());
}

#[test]
fn hotkeys_triplets_decode() {
    let hs = parse_hotkeys_list("1,2,86;2,4,65;");
    assert_eq!(hs.len(), 2);
    assert_eq!(hs[0].slot, 1);
    assert_eq!(hs[0].display, "Ctrl+V"); // 86 = 0x56 'V'
    assert_eq!(hs[0].modifier, 2);
    assert_eq!(hs[0].vkey, 86);
    assert_eq!(hs[1].display, "Shift+A"); // 65 = 0x41 'A'
}

#[test]
fn hotkeys_modifiers_combine() {
    // modifier 6 = Ctrl(2)+Shift(4)
    let hs = parse_hotkeys_list("1,6,86;");
    assert_eq!(hs[0].display, "Ctrl+Shift+V");
    // modifier 7 = Alt+Ctrl+Shift
    let hs = parse_hotkeys_list("1,7,49;");
    assert_eq!(hs[0].display, "Alt+Ctrl+Shift+1");
    // modifier 15 = all four
    let hs = parse_hotkeys_list("1,15,112;");
    assert_eq!(hs[0].display, "Alt+Ctrl+Shift+Win+F1");
    // F-key range and digits
    let hs = parse_hotkeys_list("1,0,48;");
    assert_eq!(hs[0].display, "0");
}

#[test]
fn hotkeys_malformed_skipped() {
    let hs = parse_hotkeys_list("1,2,86;x,y,z;3,99,86;4,2,900;5,2; ;6,1,71;");
    // valid: slot 1 (Ctrl+V), slot 6 (Alt+G); others skipped: garbage/unknown-mod/unknown-vkey/short
    assert_eq!(hs.len(), 2);
    assert_eq!(hs[0].display, "Ctrl+V");
    assert_eq!(hs[1].slot, 6);
    assert_eq!(hs[1].display, "Alt+G");
}

#[test]
fn legacy_ini_import() {
    let ini = r#"[Options]
HotKeysList=1,6,86;2,2,162;
ShowTrayIcon=1
[Stat]
CountCopy=99
"#;
    let p = tmp("ini", "clipdiary.cfg", ini);
    let hs = import_legacy_ini(&p).unwrap();
    // slot 1: modifier 6 = Ctrl+Shift; slot 2 (vkey 162 = 0xA2 VK_LCONTROL,
    // outside nameable range) skipped per fall-back-to-defaults rule
    assert_eq!(hs.len(), 1);
    assert_eq!(hs[0].slot, 1);
    assert_eq!(hs[0].display, "Ctrl+Shift+V");
}

#[test]
fn legacy_ini_missing_keys_defaults_empty() {
    let p = tmp("ini2", "clipdiary.cfg", "[Options]\nSomeOther=1\n");
    let hs = import_legacy_ini(&p).unwrap();
    assert!(hs.is_empty());
}

#[test]
fn storage_extra_dbs_parse() {
    let p = tmp(
        "extra",
        "clipline.toml",
        r#"
[storage]
path = 'C:\History\diary.db'
extra_dbs = ['C:\History\work.db', '/home/u/arch.sdb']
"#,
    );
    let c = Config::load(&p).unwrap();
    assert_eq!(c.storage.extra_dbs.len(), 2);
    assert_eq!(
        c.storage.extra_dbs[0],
        std::path::PathBuf::from(r"C:\History\work.db")
    );
    assert_eq!(
        c.storage.extra_dbs[1],
        std::path::PathBuf::from("/home/u/arch.sdb")
    );
}

#[test]
fn storage_extra_dbs_default_empty() {
    let p = tmp(
        "extra-default",
        "clipline.toml",
        "[storage]
path = 'x.db'
",
    );
    let c = Config::load(&p).unwrap();
    assert!(c.storage.extra_dbs.is_empty());
}

#[test]
fn hotkeys_nonprintable_keys_survive_import() {
    // cubic 6693: Up/Space/Enter/Escape were silently dropped at import.
    let got = parse_hotkeys_list("1,6,38;2,0,32;3,2,13;4,4,27;");
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(got[0].display, "Ctrl+Shift+Up");
    assert_eq!(got[1].display, "Space");
    assert_eq!(got[2].display, "Ctrl+Enter");
    assert_eq!(got[3].display, "Shift+Escape");
}

#[test]
fn hotkeys_slot_outside_13_rejected() {
    // cubic 6682: slot 0/14+ is corrupt data and must not shadow a real slot.
    let got = parse_hotkeys_list("0,6,86;14,2,86;1,2,86;");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].slot, 1);
}

#[test]
fn legacy_ini_ignores_hotkeyslist_outside_options() {
    // cubic 6166: a duplicate key in another section must not win.
    let p = tmp(
        "ini-section",
        "clipdiary.cfg",
        "[Whatever]\nHotKeysList=9,2,86;\n[Options]\nHotKeysList=1,6,86;\n",
    );
    let got = import_legacy_ini(&p).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].slot, 1, "only [Options] supplies the hotkeys list");
}
