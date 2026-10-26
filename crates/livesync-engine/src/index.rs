//! The local SQLite sync index (spec D2): one row per vault path, plus `meta`
//! (since, remote identity, adopted tweaks) and `pending` (documents to retry).

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Clean,
    Dirty,
    Conflicted,
    Skipped,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Clean => "clean",
            State::Dirty => "dirty",
            State::Conflicted => "conflicted",
            State::Skipped => "skipped",
        }
    }

    fn parse(s: &str) -> State {
        match s {
            "clean" => State::Clean,
            "dirty" => State::Dirty,
            "conflicted" => State::Conflicted,
            _ => State::Skipped,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FileRow {
    pub path: String,
    pub doc_id: String,
    /// The CouchDB revision the file on disk corresponds to.
    pub base_rev: Option<String>,
    /// Chunk IDs of `base_rev`.
    pub base_children: Vec<String>,
    pub disk_mtime: Option<i64>,
    pub disk_size: Option<u64>,
    /// xxh64 of the bytes on disk at `base_rev`, as 16 hex digits.
    pub disk_hash: Option<String>,
    pub state: State,
    /// Human-readable reason for dirty/conflicted/skipped.
    pub note: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingRow {
    pub doc_id: String,
    pub rev: Option<String>,
    pub reason: String,
    pub attempts: u32,
    pub next_attempt_ms: i64,
}

pub struct Index {
    conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    path          TEXT PRIMARY KEY,
    doc_id        TEXT NOT NULL UNIQUE,
    base_rev      TEXT,
    base_children TEXT NOT NULL DEFAULT '[]',
    disk_mtime    INTEGER,
    disk_size     INTEGER,
    disk_hash     TEXT,
    state         TEXT NOT NULL,
    note          TEXT
);
CREATE INDEX IF NOT EXISTS files_state ON files(state);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS pending (
    doc_id          TEXT PRIMARY KEY,
    rev             TEXT,
    reason          TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_ms INTEGER NOT NULL DEFAULT 0
);
"#;

pub const SCHEMA_VERSION: &str = "1";

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<FileRow> {
    let children: String = r.get(3)?;
    Ok(FileRow {
        path: r.get(0)?,
        doc_id: r.get(1)?,
        base_rev: r.get(2)?,
        base_children: serde_json::from_str(&children).unwrap_or_default(),
        disk_mtime: r.get(4)?,
        disk_size: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
        disk_hash: r.get(6)?,
        state: State::parse(&r.get::<_, String>(7)?),
        note: r.get(8)?,
    })
}

const COLS: &str = "path, doc_id, base_rev, base_children, disk_mtime, disk_size, disk_hash, state, note";

impl Index {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(SCHEMA)?;
        let idx = Index { conn };
        if idx.get_meta("schema_version")?.is_none() {
            idx.set_meta("schema_version", SCHEMA_VERSION)?;
        }
        Ok(idx)
    }

    pub fn get_meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_meta(&self, key: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn by_doc(&self, doc_id: &str) -> rusqlite::Result<Option<FileRow>> {
        self.conn
            .query_row(
                &format!("SELECT {COLS} FROM files WHERE doc_id = ?1"),
                [doc_id],
                row_from,
            )
            .optional()
    }

    pub fn by_path(&self, path: &str) -> rusqlite::Result<Option<FileRow>> {
        self.conn
            .query_row(&format!("SELECT {COLS} FROM files WHERE path = ?1"), [path], row_from)
            .optional()
    }

    /// Insert or replace the row for `row.doc_id`, dropping any older row for
    /// the same document under another path (a case-only rename).
    pub fn upsert(&self, row: &FileRow) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM files WHERE doc_id = ?1 AND path <> ?2",
            params![row.doc_id, row.path],
        )?;
        tx.execute(
            &format!(
                "INSERT INTO files({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(path) DO UPDATE SET doc_id = excluded.doc_id, base_rev = excluded.base_rev,
                   base_children = excluded.base_children, disk_mtime = excluded.disk_mtime,
                   disk_size = excluded.disk_size, disk_hash = excluded.disk_hash,
                   state = excluded.state, note = excluded.note"
            ),
            params![
                row.path,
                row.doc_id,
                row.base_rev,
                serde_json::to_string(&row.base_children).unwrap(),
                row.disk_mtime,
                row.disk_size.map(|v| v as i64),
                row.disk_hash,
                row.state.as_str(),
                row.note,
            ],
        )?;
        tx.commit()
    }

    pub fn delete_doc(&self, doc_id: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM files WHERE doc_id = ?1", [doc_id])?;
        Ok(())
    }

    pub fn counts_by_state(&self) -> rusqlite::Result<Vec<(String, u64)>> {
        let mut st = self
            .conn
            .prepare("SELECT state, COUNT(*) FROM files GROUP BY state ORDER BY state")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?;
        rows.collect()
    }

    pub fn list_state(&self, state: State, limit: usize) -> rusqlite::Result<Vec<FileRow>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM files WHERE state = ?1 ORDER BY path LIMIT ?2"
        ))?;
        let rows = st.query_map(params![state.as_str(), limit as i64], row_from)?;
        rows.collect()
    }

    pub fn all_rows(&self) -> rusqlite::Result<Vec<FileRow>> {
        let mut st = self.conn.prepare(&format!("SELECT {COLS} FROM files ORDER BY path"))?;
        let rows = st.query_map([], row_from)?;
        rows.collect()
    }

    /// A rebuild elsewhere invalidates every revision and the changes sequence.
    /// Keep the rows' disk hashes so files that are still clean may be
    /// overwritten by the refetch, while dirty files stay protected.
    pub fn reset_for_rebuild(&self) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("UPDATE files SET base_rev = NULL, base_children = '[]'", [])?;
        tx.execute("DELETE FROM files WHERE state = 'skipped'", [])?;
        tx.execute("DELETE FROM pending", [])?;
        tx.execute("DELETE FROM meta WHERE key = 'since'", [])?;
        tx.commit()
    }

    pub fn add_pending(&self, doc_id: &str, rev: Option<&str>, reason: &str, now_ms: i64) -> rusqlite::Result<()> {
        // Retry after 30 s, doubling per attempt up to an hour.
        self.conn.execute(
            "INSERT INTO pending(doc_id, rev, reason, attempts, next_attempt_ms) VALUES (?1, ?2, ?3, 1, ?4 + 30000)
             ON CONFLICT(doc_id) DO UPDATE SET rev = excluded.rev, reason = excluded.reason,
               attempts = pending.attempts + 1,
               next_attempt_ms = ?4 + MIN(3600000, 30000 * (1 << MIN(pending.attempts, 7)))",
            params![doc_id, rev, reason, now_ms],
        )?;
        Ok(())
    }

    pub fn remove_pending(&self, doc_id: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM pending WHERE doc_id = ?1", [doc_id])?;
        Ok(())
    }

    pub fn pending(&self, due_before_ms: Option<i64>) -> rusqlite::Result<Vec<PendingRow>> {
        let mut st = self.conn.prepare(
            "SELECT doc_id, rev, reason, attempts, next_attempt_ms FROM pending
             WHERE ?1 IS NULL OR next_attempt_ms <= ?1 ORDER BY doc_id",
        )?;
        let rows = st.query_map([due_before_ms], |r| {
            Ok(PendingRow {
                doc_id: r.get(0)?,
                rev: r.get(1)?,
                reason: r.get(2)?,
                attempts: r.get::<_, i64>(3)? as u32,
                next_attempt_ms: r.get(4)?,
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, id: &str) -> FileRow {
        FileRow {
            path: path.into(),
            doc_id: id.into(),
            base_rev: Some("1-a".into()),
            base_children: vec!["h:1".into()],
            disk_mtime: Some(1),
            disk_size: Some(2),
            disk_hash: Some("00".into()),
            state: State::Clean,
            note: None,
        }
    }

    #[test]
    fn upsert_and_rename() {
        let ix = Index::open_in_memory().unwrap();
        ix.upsert(&row("A.md", "a.md")).unwrap();
        assert_eq!(ix.by_doc("a.md").unwrap().unwrap().path, "A.md");
        // Case-only rename replaces the old row.
        ix.upsert(&row("a.md", "a.md")).unwrap();
        assert!(ix.by_path("A.md").unwrap().is_none());
        assert_eq!(ix.by_doc("a.md").unwrap().unwrap().base_children, vec!["h:1"]);
        assert_eq!(ix.counts_by_state().unwrap(), vec![("clean".to_string(), 1)]);
    }

    #[test]
    fn rebuild_reset_keeps_hashes() {
        let ix = Index::open_in_memory().unwrap();
        ix.upsert(&row("a.md", "a.md")).unwrap();
        let mut s = row(".obsidian/x", ".obsidian/x");
        s.state = State::Skipped;
        ix.upsert(&s).unwrap();
        ix.set_meta("since", "\"5-x\"").unwrap();
        ix.add_pending("b.md", Some("1-b"), "missing chunks", 0).unwrap();
        ix.reset_for_rebuild().unwrap();
        let r = ix.by_doc("a.md").unwrap().unwrap();
        assert_eq!(r.base_rev, None);
        assert_eq!(r.disk_hash.as_deref(), Some("00"));
        assert!(ix.by_doc(".obsidian/x").unwrap().is_none());
        assert!(ix.get_meta("since").unwrap().is_none());
        assert!(ix.pending(None).unwrap().is_empty());
    }

    #[test]
    fn pending_backoff() {
        let ix = Index::open_in_memory().unwrap();
        ix.add_pending("b.md", Some("1-b"), "x", 1000).unwrap();
        assert!(ix.pending(Some(1000)).unwrap().is_empty());
        assert_eq!(ix.pending(Some(31_000)).unwrap().len(), 1);
        ix.add_pending("b.md", Some("1-b"), "x", 31_000).unwrap();
        let p = &ix.pending(None).unwrap()[0];
        assert_eq!(p.attempts, 2);
        assert_eq!(p.next_attempt_ms, 31_000 + 60_000);
    }
}
