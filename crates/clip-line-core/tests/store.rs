use clip_line_core::capture::{Block, ClipEvent, Format};
use clip_line_core::store::{Filter, Store, StoreOpts};

fn text_event(s: &str) -> ClipEvent {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut bytes = Vec::new();
    for u in units {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes.push(0);
    bytes.push(0);
    ClipEvent {
        blocks: vec![Block {
            format: Format::UnicodeText,
            bytes,
        }],
        source_app: "demo".into(),
        source_title: "Demo Window".into(),
    }
}

fn temp_store(tag: &str) -> (Store, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("clipline-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("t.db");
    (Store::open(&p, StoreOpts::default()).unwrap(), p)
}

fn count(store: &Store) -> usize {
    store
        .list(&Filter {
            limit: 1000,
            ..Default::default()
        })
        .unwrap()
        .len()
}

#[test]
fn append_load_roundtrip() {
    let (store, _p) = temp_store("roundtrip");
    let id = store.append(&text_event("hello clipLine")).unwrap();
    assert!(id > 0);
    let blocks = store.load_data(id as i64).unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].format, Format::UnicodeText);
    // decode back
    let units: Vec<u16> = blocks[0]
        .bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let s = String::from_utf16(&units).unwrap();
    assert_eq!(s.trim_end_matches('\0'), "hello clipLine");
}

#[test]
fn dedup_identical_consecutive() {
    let (store, _p) = temp_store("dedup");
    assert!(store.append(&text_event("same")).unwrap() > 0);
    assert_eq!(store.append(&text_event("same")).unwrap(), 0);
    assert!(store.append(&text_event("different")).unwrap() > 0);
}

#[test]
fn classify_url_text_image() {
    let (store, _p) = temp_store("classify");
    let url_id = store
        .append(&text_event("https://example.com/page"))
        .unwrap();
    let rows = store
        .list(&Filter {
            kind: Some(3),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id as u64, url_id);
    assert_eq!(rows[0].url, "https://example.com/page");

    let _txt = store.append(&text_event("plain words here")).unwrap();
    let rows = store
        .list(&Filter {
            kind: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);

    let dib = ClipEvent {
        blocks: vec![Block {
            format: Format::ImageDib,
            bytes: vec![1, 2, 3, 4],
        }],
        source_app: "demo".into(),
        source_title: String::new(),
    };
    let img_id = store.append(&dib).unwrap();
    let rows = store
        .list(&Filter {
            kind: Some(5),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows[0].id as u64, img_id);
}

#[test]
fn classify_registered_image() {
    // macOS/Wayland adapters deliver images as Registered blocks (interop.md
    // type 0: registered image format). They must classify as images, not text.
    let (store, _p) = temp_store("classify_reg_img");
    let mut seen = 0;
    for (id, name) in [(8u32, "PNG"), (0, "image/png"), (0, "image/tiff")] {
        seen += 1;
        let ev = ClipEvent {
            blocks: vec![Block {
                format: Format::Registered(id, name.into()),
                bytes: vec![seen as u8, 2, 3], // distinct payload: append dedups identical envelopes
            }],
            source_app: "demo".into(),
            source_title: String::new(),
        };
        let rid = store.append(&ev).unwrap();
        assert!(rid > 0, "{name} stored");
        let rows = store
            .list(&Filter {
                kind: Some(0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), seen, "{name} classified as registered image");
        assert_eq!(rows[0].title, "Image");
    }
}

#[test]
fn title_truncated_to_55() {
    let (store, _p) = temp_store("title55");
    let long = "x".repeat(200);
    store.append(&text_event(&long)).unwrap();
    let rows = store.list(&Filter::default()).unwrap();
    assert_eq!(rows[0].title.chars().count(), 55);
}

#[test]
fn star_search_delete() {
    let (store, _p) = temp_store("ops");
    let a = store.append(&text_event("alpha needle")).unwrap();
    let _b = store.append(&text_event("beta hay")).unwrap();
    store.set_star(a as i64, true).unwrap();

    let f = Filter {
        query: Some("needle".into()),
        ..Default::default()
    };
    assert_eq!(store.list(&f).unwrap().len(), 1);

    let f = Filter {
        starred_only: true,
        ..Default::default()
    };
    assert_eq!(store.list(&f).unwrap().len(), 1);

    store.delete(&[a as i64]).unwrap();
    assert_eq!(count(&store), 1);
}

#[test]
fn set_tags_comma_wrapped() {
    let (store, _p) = temp_store("tags");
    let a = store.append(&text_event("tagged")).unwrap() as i64;
    store
        .set_tags(a, &["Important".into(), "Work".into()])
        .unwrap();
    assert_eq!(
        store.list(&Filter::default()).unwrap()[0].tags,
        ",Important,Work,"
    );
    store.set_tags(a, &[]).unwrap();
    assert_eq!(store.list(&Filter::default()).unwrap()[0].tags, ",");
}

#[test]
fn delete_range_inclusive() {
    let (store, p) = temp_store("range");
    for t in ["alpha", "beta"] {
        store.append(&text_event(t)).unwrap();
    }
    // rewrite created to fixed stamps: 2 old (Oct 20-21), 1 new (Oct 27)
    let conn = rusqlite::Connection::open(&p).unwrap();
    conn.execute_batch(
        "UPDATE log SET created='2026-10-20 10:00:00' WHERE id=1;
         UPDATE log SET created='2026-10-21 10:00:00' WHERE id=2;",
    )
    .unwrap();
    drop(conn);
    store.append(&text_event("fresh")).unwrap(); // created = now (2026-10-27)

    // window strictly between stamps: boundary rows at both ends survive? no — inclusive:
    store
        .delete_range("2026-10-20 10:00:00", "2026-10-21 10:00:00")
        .unwrap();
    assert_eq!(count(&store), 1, "boundary rows deleted, fresh survives");

    // wide range that clearly covers all rows
    store
        .delete_range("2000-01-01 00:00:00", "2100-01-01 00:00:00")
        .unwrap();
    assert_eq!(count(&store), 0);
}

#[test]
fn filter_date_bounds() {
    let (store, p) = temp_store("filter-dates");
    store.append(&text_event("old")).unwrap();
    let created: String = {
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.query_row("SELECT created FROM log WHERE id=1", [], |r| r.get(0))
            .unwrap()
    };
    // to = day before created → excluded
    let day = &created[..10];
    let mut prev = day.to_string();
    prev.replace_range(8..10, "01"); // same month, earlier day
    assert_eq!(
        store
            .list(&Filter {
                to: Some(prev),
                ..Default::default()
            })
            .unwrap()
            .len(),
        0
    );

    // range covering today → included
    assert_eq!(
        store
            .list(&Filter {
                from: Some("2000-01-01".into()),
                to: Some("2100-01-01".into()),
                ..Default::default()
            })
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn import_legacy_preserves_rows() {
    let (src_dir, _) = (
        std::env::temp_dir().join(format!("clipline-legacy-{}", std::process::id())),
        (),
    );
    let _ = std::fs::remove_dir_all(&src_dir);
    std::fs::create_dir_all(&src_dir).unwrap();
    let legacy = src_dir.join("legacy.db");
    {
        let s = Store::open(&legacy, StoreOpts::default()).unwrap();
        s.append(&text_event("legacy row one")).unwrap();
        s.append(&text_event("https://legacy.example.com")).unwrap();
    }
    let (store, _p) = temp_store("import");
    let n = store.import_legacy(&legacy).unwrap();
    assert_eq!(n, 2);
    assert_eq!(count(&store), 2);
}

#[test]
fn snippets_tree_ops() {
    let (store, p) = temp_store("snippets");
    let sdb = p.parent().unwrap().join("snippets.sdb");

    // ops before attach must fail
    assert!(store.snippet_list(0).is_err());

    store.attach_snippets(&sdb).unwrap();
    assert!(store.attach_snippets(&sdb).is_err(), "second attach errors");

    let root = store.snippet_add(0, "Canned replies", "").unwrap();
    let child = store.snippet_add(root, "greet", "Hello there!").unwrap();

    let roots = store.snippet_list(0).unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].id, root);
    assert_eq!(roots[0].kind, 2);

    let kids = store.snippet_list(root).unwrap();
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].id, child);
    assert_eq!(kids[0].data, "Hello there!");
    assert_eq!(kids[0].pid, root);

    store.snippet_rename(child, "greeting").unwrap();
    assert_eq!(store.snippet_list(root).unwrap()[0].name, "greeting");

    let other = store.snippet_add(0, "Other", "").unwrap();
    store.snippet_move(child, other).unwrap();
    assert_eq!(store.snippet_list(root).unwrap().len(), 0);
    assert_eq!(store.snippet_list(other).unwrap().len(), 1);

    store.snippet_delete(child).unwrap();
    assert_eq!(store.snippet_list(other).unwrap().len(), 0);

    // survives reopen (schema + data persisted in attached file)
    drop(store);
    let s2 = Store::open(&p, StoreOpts::default()).unwrap();
    s2.attach_snippets(&sdb).unwrap();
    assert_eq!(s2.snippet_list(0).unwrap().len(), 2); // root + other
}

#[test]
fn gzip_envelope_shape() {
    let bytes = clip_line_core::envelope::pack(&[Block {
        format: Format::UnicodeText,
        bytes: vec![0x41, 0x00],
    }])
    .unwrap();
    assert_eq!(&bytes[..3], &[0x1f, 0x8b, 0x08], "gzip magic");
    let blocks = clip_line_core::envelope::unpack(&bytes).unwrap();
    assert_eq!(blocks[0].bytes, vec![0x41, 0x00]);
}

#[test]
fn fts_search_matches_title() {
    let (store, _p) = temp_store("fts-title");
    store.append(&text_event("alpha needle here")).unwrap();
    store.append(&text_event("beta hay there")).unwrap();
    let rows = store
        .list(&Filter {
            query: Some("needle".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].title.contains("needle"));
}

#[test]
fn fts_percent_wildcard_falls_back_like() {
    let (store, _p) = temp_store("fts-like");
    store.append(&text_event("foobarbaz qux")).unwrap();
    store.append(&text_event("other")).unwrap();
    // '%' is invalid FTS5 syntax (unquoted) → LIKE fallback still matches substring
    let rows = store
        .list(&Filter {
            query: Some("%barba%".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].title.contains("foobarbaz"));
}

#[test]
fn fts_legacy_db_backfilled_on_open() {
    // DB with rows but no FTS table (simulates pre-FTS database)
    let (store, p) = temp_store("fts-legacy");
    store.append(&text_event("old row searchable")).unwrap();
    {
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute_batch(
            "DROP TRIGGER log_fts_ai; DROP TRIGGER log_fts_ad; DROP TRIGGER log_fts_au;
             DROP TABLE log_fts; DELETE FROM options WHERE key='fts5_built';",
        )
        .unwrap();
    }
    drop(store);
    // reopen: FTS recreated + pre-existing rows indexed, search works
    let store = Store::open(&p, StoreOpts::default()).unwrap();
    let rows = store
        .list(&Filter {
            query: Some("searchable".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1, "legacy row found via FTS after backfill");
}

#[test]
fn fts_odd_query_no_panic() {
    let (store, _p) = temp_store("fts-odd");
    store.append(&text_event("some clip")).unwrap();
    for q in [
        "",
        " ",
        "%",
        "_",
        "(",
        "\"",
        "*",
        "OR",
        "NEAR(",
        "\\",
        "a b -",
        "ünïcödé",
    ] {
        // must not panic; any result count acceptable
        let _ = store.list(&Filter {
            query: Some(q.into()),
            ..Default::default()
        });
    }
}

#[test]
fn tag_list_counts_desc_dedup() {
    let (store, _p) = temp_store("taglist");
    let a = store.append(&text_event("one")).unwrap() as i64;
    let b = store.append(&text_event("two")).unwrap() as i64;
    let c = store.append(&text_event("three")).unwrap() as i64;
    store.set_tags(a, &["x".into(), "y".into()]).unwrap();
    store.set_tags(b, &["y".into()]).unwrap();
    store.set_tags(c, &["y".into(), "x".into()]).unwrap();
    // y appears in 3 rows, x in 2 -> y first; count desc.
    assert_eq!(store.tag_list().unwrap(), vec!["y", "x"]);
}

#[test]
fn tag_rename_whole_tag_no_substring_collision() {
    let (store, _p) = temp_store("tagren");
    let a = store.append(&text_event("r1")).unwrap() as i64;
    let b = store.append(&text_event("r2")).unwrap() as i64;
    store.set_tags(a, &["red".into(), "blue".into()]).unwrap();
    store.set_tags(b, &["redish".into()]).unwrap();
    store.tag_rename("red", "scarlet").unwrap();
    let rows = store.list(&Filter::default()).unwrap();
    // newest first: b, a
    assert_eq!(rows[0].tags, ",redish,");
    assert_eq!(rows[1].tags, ",scarlet,blue,");
}

#[test]
fn tag_delete_whole_tag_no_substring_collision() {
    let (store, _p) = temp_store("tagdel");
    let a = store.append(&text_event("d1")).unwrap() as i64;
    let b = store.append(&text_event("d2")).unwrap() as i64;
    store.set_tags(a, &["red".into(), "blue".into()]).unwrap();
    store.set_tags(b, &["redish".into()]).unwrap();
    store.tag_delete("red").unwrap();
    let rows = store.list(&Filter::default()).unwrap();
    assert_eq!(rows[0].tags, ",redish,");
    assert_eq!(rows[1].tags, ",blue,");
}
