//! Deep module: owns the SQLite schema, gzip envelope, type codes and
//! byte-compat rules from docs/interop.md. Single adapter (rusqlite), no seam.

use crate::capture::{Block, ClipEvent, Format};
use crate::envelope;
use anyhow::{Context, bail};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

/// log.type values, matching the original app's database.
pub mod type_code {
    pub const REGISTERED_IMAGE: i64 = 0;
    pub const TEXT: i64 = 1;
    pub const SECONDARY_TEXT: i64 = 2;
    pub const URL: i64 = 3;
    pub const FILES: i64 = 4;
    pub const IMAGE: i64 = 5;
}

#[derive(Debug, Clone, Default)]
pub struct StoreOpts {
    /// Some(key) opens/creates an SQLCipher-encrypted database (encryption feature).
    pub key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Filter {
    pub query: Option<String>,
    pub starred_only: bool,
    pub kind: Option<i64>,
    /// Inclusive `created` bounds, SQLite `CURRENT_TIMESTAMP` format
    /// (`YYYY-MM-DD HH:MM:SS`; date-only prefixes also compare correctly).
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: u32,
    pub offset: u32,
}

impl Default for Filter {
    fn default() -> Self {
        Filter {
            query: None,
            starred_only: false,
            kind: None,
            from: None,
            to: None,
            limit: 200,
            offset: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClipRow {
    pub id: i64,
    pub created: String,
    pub title: String,
    pub kind: i64,
    pub appname: String,
    pub starred: bool,
    pub tags: String,
    pub datasize: i64,
    pub url: String,
}

/// Row of snippets.sdb `common` (attached second database).
#[derive(Debug, Clone)]
pub struct Snippet {
    pub id: i64,
    pub pid: i64,
    pub pos: i64,
    pub kind: i64,
    pub name: String,
    pub data: String,
    pub description: String,
}

pub struct Store {
    conn: Mutex<Connection>,
    /// Key the DB was opened with (SQLCipher), reused for encrypted backups.
    key: Option<String>,
    snips_attached: AtomicBool,
    /// Attached snippets file, recorded for backup_to's plain copy.
    snips_path: Mutex<Option<PathBuf>>,
    #[allow(dead_code)]
    path: PathBuf,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS log(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  created TIMESTAMP not null default CURRENT_TIMESTAMP,
  title CHAR(55) not null default '',
  type INTEGER not null default 0,
  description TEXT not null default '',
  data TEXT not null default '',
  datasize INTEGER not null default 0,
  appname CHAR(20) not null default '',
  apptitle CHAR(100) not null default '',
  starred INTEGER not null default 0,
  tags TEXT not null default ',',
  url TEXT not null default '');
CREATE TABLE IF NOT EXISTS options(key CHAR(30) PRIMARY KEY not null default '', value TEXT not null default '');
";

/// External-content FTS5 mirror of log(title,appname,url), kept in sync by triggers.
/// ponytail: MATCH-only via match_clause; if rank/snippet/prefix queries are ever
/// needed, extend match_clause — callers stay on Filter.query.
const FTS_SCHEMA: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS log_fts USING fts5(title, appname, url, content='log', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS log_fts_ai AFTER INSERT ON log BEGIN
  INSERT INTO log_fts(rowid,title,appname,url) VALUES(new.id,new.title,new.appname,new.url);
END;
CREATE TRIGGER IF NOT EXISTS log_fts_ad AFTER DELETE ON log BEGIN
  INSERT INTO log_fts(log_fts,rowid,title,appname,url) VALUES('delete',old.id,old.title,old.appname,old.url);
END;
CREATE TRIGGER IF NOT EXISTS log_fts_au AFTER UPDATE ON log BEGIN
  INSERT INTO log_fts(log_fts,rowid,title,appname,url) VALUES('delete',old.id,old.title,old.appname,old.url);
  INSERT INTO log_fts(rowid,title,appname,url) VALUES(new.id,new.title,new.appname,new.url);
END;
";

const FTS_MATCH_CLAUSE: &str =
    " AND id IN (SELECT rowid FROM log_fts WHERE log_fts MATCH ?1 AND log.title LIKE '%'||?1||'%')";
const LIKE_CLAUSE: &str =
    " AND (title LIKE '%'||?1||'%' OR appname LIKE '%'||?1||'%' OR url LIKE '%'||?1||'%')";

fn setup_fts(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(FTS_SCHEMA)?;
    // one-time population of pre-existing rows; options flag makes it idempotent
    let built: bool = conn
        .query_row(
            "SELECT value FROM options WHERE key='fts5_built'",
            [],
            |r| r.get::<_, String>(0),
        )
        .map(|v| v == "1")
        .unwrap_or(false);
    if !built {
        conn.execute_batch("INSERT INTO log_fts(log_fts) VALUES('rebuild');")?;
        conn.execute(
            "INSERT OR REPLACE INTO options(key,value) VALUES('fts5_built','1')",
            [],
        )?;
    }
    Ok(())
}

/// MATCH if q parses as FTS5 syntax, else LIKE (percent wildcards, bare punctuation).
fn match_clause(conn: &Connection, q: &str) -> anyhow::Result<&'static str> {
    // FTS5 MATCH is word-token; SPEC parity search is substring LIKE %q%.
    // Use MATCH only for plain words (no wildcards) and LIKE for everything
    // the user means as a substring ("hello worl" must still find rows).
    if q.contains(['%', '_', '*', '"', '\'']) {
        return Ok(LIKE_CLAUSE);
    }
    // ponytail: any probe error falls back to LIKE — only failure mode is bad FTS5 syntax
    Ok(
        if conn
            .query_row(
                "SELECT count(*) FROM log_fts WHERE log_fts MATCH ?1",
                params![q],
                |_| Ok(()),
            )
            .is_ok()
        {
            FTS_MATCH_CLAUSE
        } else {
            LIKE_CLAUSE
        },
    )
}

const SNIPPETS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS snippets.common(
  id INTEGER PRIMARY KEY AUTOINCREMENT, pid INTEGER not null default 0,
  pos INTEGER not null default 1000, type INTEGER not null default 2,
  name TEXT not null default '', data TEXT not null default '',
  description TEXT not null default '');
";

/// Runs `PRAGMA key` before any schema op and verifies it immediately, so a
/// wrong key or a legacy unencrypted database fails with one clear message
/// (both surface as SQLITE_NOTADB). SPEC Q9.
#[cfg(feature = "encryption")]
fn apply_key(conn: &Connection, key: &str) -> anyhow::Result<()> {
    if key.is_empty() {
        bail!("StoreOpts.key must not be empty");
    }
    // Quote-wrapped string literal (SQLCipher). rusqlite pragma_update helpers
    // mishandle PRAGMA key on reopen, so build the statement by hand with the
    // key SQL-escaped (doubling embedded single quotes).
    let esc = key.replace(
        char::from(0x27),
        char::from(0x27).to_string().repeat(2).as_str(),
    );
    conn.execute_batch(&format!("PRAGMA key = '{}';", esc))
        .context("PRAGMA key")?;
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))
        .map_err(|e| {
            anyhow::anyhow!("cannot open database with key: wrong key, or the database is not encrypted. Legacy databases are unlocked: open them without a key. Encrypted databases are new DBs only (SPEC Q9): {e}")
        })?;
    Ok(())
}

impl Store {
    /// Opens an existing database in place; creates schema when absent.
    /// `StoreOpts.key` creates/opens an SQLCipher-encrypted database; encrypted
    /// databases are new DBs only (SPEC Q9). Legacy unencrypted files must be
    /// opened without a key.
    pub fn open(path: &Path, opts: StoreOpts) -> anyhow::Result<Store> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        if let Some(key) = opts.key.as_deref() {
            #[cfg(feature = "encryption")]
            apply_key(&conn, key)?;
            #[cfg(not(feature = "encryption"))]
            {
                let _ = key;
                bail!(
                    "StoreOpts.key given but this build lacks the `encryption` feature (rebuild with --features clip-line-core/encryption)"
                );
            }
        }
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA).with_context(|| {
            format!(
                "schema access failed on {}: wrong or missing key, or not a database",
                path.display()
            )
        })?;
        setup_fts(&conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
            key: opts.key,
            snips_attached: AtomicBool::new(false),
            snips_path: Mutex::new(None),
            path: path.to_path_buf(),
        })
    }

    /// Attaches snippets.sdb as second database `snippets`; errors if already attached.
    pub fn attach_snippets(&self, path: &Path) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        if self.snips_attached.load(Ordering::Relaxed) {
            bail!("snippets database already attached");
        }
        conn.execute_batch(&format!(
            "ATTACH DATABASE '{}' AS snippets;",
            path.display().to_string().replace('\'', "''")
        ))
        .with_context(|| format!("attach {}", path.display()))?;
        conn.execute_batch(SNIPPETS_SCHEMA)?;
        self.snips_attached.store(true, Ordering::Relaxed);
        drop(conn);
        *self.snips_path.lock().unwrap() = Some(path.to_path_buf());
        Ok(())
    }

    /// Backs up the main database to `dst` via the SQLite online-backup API,
    /// then copies an attached snippets file next to it (`<dst stem>.snippets.sdb`).
    /// NOTE: the SQLite backup API moves ONE schema (main) per call — attached
    /// databases are NOT included, so snippets get a plain file copy under the
    /// conn lock (journal mode is rollback-journal, so no WAL sidecars to chase).
    /// ponytail: if snippets ever move to WAL, checkpoint or backup-api them too.
    pub fn backup_to(&self, dst: &Path) -> anyhow::Result<()> {
        use rusqlite::backup::{Backup, StepResult};
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let conn = self.conn.lock().unwrap();
        let mut out =
            Connection::open(dst).with_context(|| format!("open backup {}", dst.display()))?;
        // An encrypted source needs an encrypted destination: apply the same
        // key before any page write or the backup lands as ciphertext garbage.
        #[cfg(feature = "encryption")]
        if let Some(k) = &self.key {
            use rusqlite::config::DbConfig;
            let _ = out.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, false);
            apply_key(&out, k)?;
        }
        #[cfg(not(feature = "encryption"))]
        let _ = &self.key;
        let b = Backup::new(&conn, &mut out)?;
        let mut r = StepResult::More;
        while r == StepResult::More {
            r = b.step(100)?;
        }
        if r != StepResult::Done {
            bail!("backup step ended {r:?}");
        }
        drop(b);
        if let Some(sp) = self.snips_path.lock().unwrap().as_ref() {
            let to = dst.with_file_name(format!(
                "{}.snippets.sdb",
                dst.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("snippets")
            ));
            std::fs::copy(sp, &to)
                .with_context(|| format!("copy snippets {} -> {}", sp.display(), to.display()))?;
        }
        Ok(())
    }

    fn snips(&self) -> anyhow::Result<MutexGuard<'_, Connection>> {
        if !self.snips_attached.load(Ordering::Relaxed) {
            bail!("snippets database not attached");
        }
        Ok(self.conn.lock().unwrap())
    }

    /// Stores an event. Deduplicates against the last row's payload.
    /// Empty events (clipboard open failure, no recognized formats) are skipped.
    pub fn append(&self, ev: &ClipEvent) -> anyhow::Result<u64> {
        if ev.blocks.is_empty() {
            return Ok(0);
        }
        let payload = envelope::pack(&ev.blocks)?;
        if let Some(last) = self.last_payload()? {
            if last == payload {
                return Ok(0);
            }
        }
        let text = text_of(&ev.blocks);
        let (title, url, kind) = classify(&text, &ev.blocks);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO log(title,type,data,datasize,appname,apptitle,starred,tags,url)
             VALUES(?1,?2,?3,?4,?5,?6,0,',',?7)",
            params![
                truncate(&title, 55),
                kind,
                payload,
                payload.len() as i64,
                truncate(&ev.source_app, 20),
                truncate(&ev.source_title, 100),
                url
            ],
        )?;
        Ok(conn.last_insert_rowid() as u64)
    }

    pub fn list(&self, f: &Filter) -> anyhow::Result<Vec<ClipRow>> {
        let mut sql = String::from(
            "SELECT id,created,title,type,appname,starred,tags,datasize,url FROM log WHERE 1=1",
        );
        if f.starred_only {
            sql.push_str(" AND starred=1");
        }
        if let Some(k) = f.kind {
            sql.push_str(&format!(" AND type={k}"));
        }
        let conn = self.conn.lock().unwrap();
        if f.query.is_some() {
            // SPEC: search is substring LIKE %q% parity ("FTS5 later"); MATCH
            // is word-token and drops substring/prefix queries (cubic 5983).
            sql.push_str(LIKE_CLAUSE);
        }
        if f.from.is_some() {
            sql.push_str(" AND created>=?2");
        }
        if f.to.is_some() {
            sql.push_str(" AND created<=?3");
        }
        // A date-only upper bound ("2026-10-27", 10 chars) excludes every row
        // of that day under string compare; widen it to the end of the day.
        let to = f.to.as_deref().map(|t| {
            if t.len() == 10 {
                format!("{t} 23:59:59")
            } else {
                t.to_string()
            }
        });
        sql.push_str(" ORDER BY id DESC LIMIT ?4 OFFSET ?5");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                f.query.clone().unwrap_or_default(),
                f.from.clone().unwrap_or_default(),
                to.unwrap_or_default(),
                f.limit.max(1),
                f.offset
            ],
            |r| {
                Ok(ClipRow {
                    id: r.get(0)?,
                    created: r.get(1)?,
                    title: r.get(2)?,
                    kind: r.get(3)?,
                    appname: r.get(4).unwrap_or_default(),
                    starred: r.get::<_, i64>(5)? != 0,
                    tags: r.get(6).unwrap_or_default(),
                    datasize: r.get(7).unwrap_or_default(),
                    url: r.get(8).unwrap_or_default(),
                })
            },
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn load_data(&self, id: i64) -> anyhow::Result<Vec<Block>> {
        let conn = self.conn.lock().unwrap();
        let blob: Vec<u8> =
            conn.query_row("SELECT data FROM log WHERE id=?1", params![id], |r| {
                r.get(0)
            })?;
        drop(conn);
        envelope::unpack(&blob)
    }

    pub fn set_star(&self, id: i64, on: bool) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE log SET starred=?2 WHERE id=?1",
            params![id, on as i64],
        )?;
        Ok(())
    }

    /// Writes tags comma-wrapped (`,a,b,`; empty list → `,`) per interop.md.
    pub fn set_tags(&self, id: i64, tags: &[String]) -> anyhow::Result<()> {
        let mut wrapped = String::from(",");
        for t in tags {
            wrapped.push_str(t);
            wrapped.push(',');
        }
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE log SET tags=?2 WHERE id=?1", params![id, wrapped])?;
        Ok(())
    }

    /// All distinct tags, most-used first (ties alphabetical).
    pub fn tag_list(&self) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT tags FROM log")?;
        let mut counts: std::collections::HashMap<String, usize> = Default::default();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for wrapped in rows {
            let wrapped = wrapped?;
            for tag in wrapped.split(',').filter(|t| !t.is_empty()) {
                *counts.entry(tag.to_string()).or_default() += 1;
            }
        }
        drop(stmt);
        drop(conn);
        let mut v: Vec<(String, usize)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(v.into_iter().map(|(t, _)| t).collect())
    }

    /// Renames tag `old` to `new` across all rows. Whole-tag match only on the
    /// comma-wrapped column, so substring tags (`red` vs `redish`) stay intact.
    pub fn tag_rename(&self, old: &str, new: &str) -> anyhow::Result<()> {
        let old_w = format!(",{old},");
        let new_w = format!(",{new},");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE log SET tags = substr(REPLACE(','||tags||',', ?1, ?2), 2, length(REPLACE(','||tags||',', ?1, ?2)) - 2) WHERE ','||tags||',' LIKE ?3 ESCAPE '\\'",
            params![old_w, new_w, format!("%{}%", like_escape(&old_w))],
        )?;
        Ok(())
    }

    /// Removes tag `tag` from all rows (whole-tag match).
    pub fn tag_delete(&self, tag: &str) -> anyhow::Result<()> {
        let tag_w = format!(",{tag},");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE log SET tags = substr(REPLACE(','||tags||',', ?1, ','), 2, length(REPLACE(','||tags||',', ?1, ',')) - 2) WHERE ','||tags||',' LIKE ?2 ESCAPE '\\'",
            params![tag_w, format!("%{}%", like_escape(&tag_w))],
        )?;
        Ok(())
    }

    pub fn delete(&self, ids: &[i64]) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        for id in ids {
            conn.execute("DELETE FROM log WHERE id=?1", params![id])?;
        }
        Ok(())
    }

    /// Deletes rows with `from <= created <= to` (inclusive, string comparison
    /// over `CURRENT_TIMESTAMP` format).
    pub fn delete_range(&self, from: &str, to: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM log WHERE created>=?1 AND created<=?2",
            params![from, to],
        )?;
        Ok(())
    }

    pub fn snippet_list(&self, pid: i64) -> anyhow::Result<Vec<Snippet>> {
        let conn = self.snips()?;
        let mut stmt = conn.prepare(
            "SELECT id,pid,pos,type,name,data,description
             FROM snippets.common WHERE pid=?1 ORDER BY pos,name",
        )?;
        let rows = stmt.query_map(params![pid], |r| {
            Ok(Snippet {
                id: r.get(0)?,
                pid: r.get(1)?,
                pos: r.get(2)?,
                kind: r.get(3)?,
                name: r.get(4)?,
                data: r.get(5)?,
                description: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Inserts a text snippet (type 2) under `pid`; returns its id.
    pub fn snippet_add(&self, pid: i64, name: &str, data: &str) -> anyhow::Result<i64> {
        let conn = self.snips()?;
        conn.execute(
            "INSERT INTO snippets.common(pid,name,data) VALUES(?1,?2,?3)",
            params![pid, name, data],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn snippet_rename(&self, id: i64, name: &str) -> anyhow::Result<()> {
        let conn = self.snips()?;
        conn.execute(
            "UPDATE snippets.common SET name=?2 WHERE id=?1",
            params![id, name],
        )?;
        Ok(())
    }

    pub fn snippet_move(&self, id: i64, new_pid: i64) -> anyhow::Result<()> {
        let conn = self.snips()?;
        conn.execute(
            "UPDATE snippets.common SET pid=?2 WHERE id=?1",
            params![id, new_pid],
        )?;
        Ok(())
    }

    /// Deletes a snippet and its whole subtree (interop.md `common` is a pid
    /// tree; deleting a parent must not orphan its descendants).
    pub fn snippet_delete(&self, id: i64) -> anyhow::Result<()> {
        let conn = self.snips()?;
        let mut front = vec![id];
        while let Some(cur) = front.pop() {
            conn.execute("DELETE FROM snippets.common WHERE id=?1", params![cur])?;
            let mut stmt = conn.prepare("SELECT id FROM snippets.common WHERE pid=?1")?;
            let kids: Vec<i64> = stmt
                .query_map(params![cur], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            front.extend(kids);
        }
        Ok(())
    }

    /// Copies every row from a legacy database into this one, preserving ids.
    /// Refuses encrypted/corrupt sources with an explanatory error (SPEC Q9).
    pub fn import_legacy(&self, legacy: &Path) -> anyhow::Result<u64> {
        let conn = self.conn.lock().unwrap();
        // KEY '' keeps the source plaintext: SQLCipher would otherwise encrypt
        // the attached file with the main database key.
        #[cfg(feature = "encryption")]
        let attach = format!(
            "ATTACH DATABASE '{}' AS src KEY '';",
            legacy.display().to_string().replace('\'', "''")
        );
        #[cfg(not(feature = "encryption"))]
        let attach = format!(
            "ATTACH DATABASE '{}' AS src;",
            legacy.display().to_string().replace('\'', "''")
        );
        if let Err(e) = conn.execute_batch(&attach) {
            // Attach validates the key at once on SQLCipher builds: an
            // encrypted (or non-SQLite) source fails here, not at first read.
            bail!(
                "cannot import {}: {}. The source is encrypted (or not a SQLite database); clipLine cannot read encrypted sources (SPEC Q9). Import from the unencrypted original",
                legacy.display(),
                e
            );
        }
        if let Err(e) = conn.query_row("SELECT count(*) FROM src.sqlite_schema", [], |_| Ok(())) {
            let _ = conn.execute_batch("DETACH DATABASE src;");
            bail!(
                "cannot import {}: {}. The source is encrypted (or not a SQLite \
                 database); clipLine cannot read encrypted sources (SPEC Q9). \
                 Import from the unencrypted original",
                legacy.display(),
                e
            );
        }
        let before: i64 = conn.query_row("SELECT count(*) FROM log", [], |r| r.get(0))?;
        let copy = conn.execute_batch(
            "INSERT OR IGNORE INTO log SELECT * FROM src.log;
             INSERT OR IGNORE INTO options SELECT * FROM src.options;",
        );
        let after: Result<i64, _> = conn.query_row("SELECT count(*) FROM log", [], |r| r.get(0));
        let _ = conn.execute_batch("DETACH DATABASE src;"); // never leak the attach
        copy?;
        Ok((after? - before) as u64)
    }

    fn last_payload(&self) -> anyhow::Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM log ORDER BY id DESC LIMIT 1")?;
        let mut rows = stmt.query([])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }
}

/// UTF-16LE → String for CF_UNICODETEXT blocks.
fn text_of(blocks: &[Block]) -> String {
    for b in blocks {
        if b.format == Format::UnicodeText {
            let units: Vec<u16> = b
                .bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            if let Ok(s) = String::from_utf16(&units) {
                return s.trim_end_matches('\0').to_string();
            }
        }
    }
    String::new()
}

/// Registered-format names the adapters emit for image payloads (macOS UTIs,
/// Wayland MIME types, Windows "PNG"/"JFIF" twins). Name-only matching: the
/// registered numeric id is machine-local and carries no meaning.
fn is_registered_image(name: &str) -> bool {
    const KNOWN: &[&str] = &[
        "PNG",
        "TIFF",
        "JFIF",
        "image/png",
        "image/tiff",
        "image/jpeg",
        "image/bmp",
        "image/webp",
        "application/x-qt-image",
    ];
    KNOWN.iter().any(|k| name.eq_ignore_ascii_case(k))
}

fn classify(text: &str, blocks: &[Block]) -> (String, String, i64) {
    if blocks
        .iter()
        .any(|b| matches!(b.format, Format::ImageDib | Format::ImageDibV5))
    {
        return ("Image".into(), String::new(), type_code::IMAGE);
    }
    // Registered image formats (type 0 per interop.md: PNG-like screen
    // captures from non-Windows adapters and PNG twins).
    if blocks.iter().any(|b| match &b.format {
        Format::Registered(_, name) => is_registered_image(name),
        _ => false,
    }) {
        return ("Image".into(), String::new(), type_code::REGISTERED_IMAGE);
    }
    if blocks.iter().any(|b| b.format == Format::FileDrop) {
        return ("Files".into(), String::new(), type_code::FILES);
    }
    let first_line = text.lines().next().unwrap_or_default().to_string();
    let text = text.trim();
    let looks_url = text.len() < 2048
        && (text.starts_with("http://") || text.starts_with("https://"))
        && !text.contains(char::is_whitespace);
    if looks_url {
        return (first_line.clone(), text.to_string(), type_code::URL);
    }
    (first_line, String::new(), type_code::TEXT)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

/// `%`/`_`/`\` escaped for a LIKE pattern with `ESCAPE '\\'`.
fn like_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            o.push('\\');
        }
        o.push(c);
    }
    o
}
