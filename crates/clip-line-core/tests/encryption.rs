//! SQLCipher tests; built only with `--features encryption` (sqlcipher build
//! is slow — acceptable, SPEC store section).

#![cfg(feature = "encryption")]

use clip_line_core::capture::{Block, ClipEvent, Format};
use clip_line_core::store::{Filter, Store, StoreOpts};

fn text_event(s: &str) -> ClipEvent {
    let mut bytes: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    bytes.extend_from_slice(&[0, 0]);
    ClipEvent {
        blocks: vec![Block {
            format: Format::UnicodeText,
            bytes,
        }],
        source_app: "demo".into(),
        source_title: String::new(),
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("clipline-enc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn opts(key: &str) -> StoreOpts {
    StoreOpts {
        key: Some(key.into()),
    }
}

#[test]
fn key_roundtrip_append_load() {
    let dir = temp_dir("roundtrip");
    let p = dir.join("t.db");

    let store = Store::open(&p, opts("sekrit")).unwrap();
    let id = store.append(&text_event("encrypted hello")).unwrap();
    drop(store);

    // file is not plaintext sqlite
    let head = std::fs::read(&p).unwrap();
    assert!(
        !head.starts_with(b"SQLite format 3"),
        "db must be encrypted"
    );
    // plaintext reader cannot open it
    assert!(
        rusqlite::Connection::open(&p)
            .unwrap()
            .query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))
            .is_err()
    );

    let store2 = Store::open(&p, opts("sekrit")).unwrap();
    let blocks = store2.load_data(id as i64).unwrap();
    assert_eq!(blocks.len(), 1);
    let units: Vec<u16> = blocks[0]
        .bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let s = String::from_utf16(&units).unwrap();
    assert_eq!(s.trim_end_matches('\0'), "encrypted hello");
    assert_eq!(store2.list(&Filter::default()).unwrap().len(), 1);
}

#[test]
fn wrong_key_fails_clearly() {
    let dir = temp_dir("wrongkey");
    let p = dir.join("t.db");
    {
        let store = Store::open(&p, opts("right")).unwrap();
        store.append(&text_event("data")).unwrap();
    }
    let err = match Store::open(&p, opts("wrong")) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("wrong key must fail"),
    };
    assert!(
        err.contains("wrong key"),
        "message should name the cause: {err}"
    );
    // no key on an encrypted db also fails with the clear message
    let err = match Store::open(&p, StoreOpts::default()) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("no-key open of encrypted db must fail"),
    };
    assert!(err.contains("key") || err.contains("database"), "{err}");
}

#[test]
fn legacy_plaintext_with_key_fails_clearly() {
    let dir = temp_dir("legacykey");
    let p = dir.join("t.db");
    {
        let store = Store::open(&p, StoreOpts::default()).unwrap(); // plaintext
        store.append(&text_event("legacy row")).unwrap();
    }
    let err = match Store::open(&p, opts("k")) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("key on plaintext db must fail"),
    };
    assert!(
        err.contains("not encrypted") || err.contains("legacy"),
        "{err}"
    );
}

#[test]
fn import_encrypted_source_refused() {
    let dir = temp_dir("import");
    let enc = dir.join("enc.db");
    {
        let store = Store::open(&enc, opts("k1")).unwrap();
        store.append(&text_event("hidden")).unwrap();
    }
    let plain_target = dir.join("plain.db");
    let store = Store::open(&plain_target, StoreOpts::default()).unwrap();
    let err = match store.import_legacy(&enc) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("encrypted source must be refused"),
    };
    assert!(
        err.contains("encrypted") || err.contains("SPEC Q9"),
        "{err}"
    );
}

#[test]
fn import_plaintext_source_still_works() {
    let dir = temp_dir("import-ok");
    let legacy = dir.join("legacy.db");
    {
        let s = Store::open(&legacy, StoreOpts::default()).unwrap();
        s.append(&text_event("legacy row one")).unwrap();
    }
    let target = dir.join("target.db");
    let store = Store::open(&target, opts("k2")).unwrap();
    let n = store.import_legacy(&legacy).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn empty_key_rejected() {
    let dir = temp_dir("emptykey");
    let err = match Store::open(&dir.join("t.db"), opts("")) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("empty key must be rejected"),
    };
    assert!(err.contains("empty"), "{err}");
}
