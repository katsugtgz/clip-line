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
