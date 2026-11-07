//! The local SQLite sync index (spec D2): one row per vault path, plus `meta`
//! (since, remote identity, adopted tweaks), `pending` (documents to retry)
//! and `revisions` (the history of every file revision this device applied,
//! pushed or saw, spec D12).

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Clean,
    Dirty,
    Conflicted,
    Skipped,
    /// The local file was moved out of the folder scope (or into a dot-folder);
    /// the remote document is left alone.
    OutOfScope,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Clean => "clean",
            State::Dirty => "dirty",
            State::Conflicted => "conflicted",
            State::Skipped => "skipped",
            State::OutOfScope => "out_of_scope",
        }
    }

    fn parse(s: &str) -> State {
        match s {
            "clean" => State::Clean,
            "dirty" => State::Dirty,
            "conflicted" => State::Conflicted,
            "out_of_scope" => State::OutOfScope,
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

/// A local file the push side cannot sync (no document involved), such as a
/// case collision between two local names or a file over the size limit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LocalIssue {
    pub path: String,
    pub kind: String,
    pub note: String,
    pub seen_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingRow {
    pub doc_id: String,
    pub rev: Option<String>,
    pub reason: String,
    pub attempts: u32,
    pub next_attempt_ms: i64,
}

/// Where a recorded revision came from. LiveSync documents don't say which
/// device wrote them, so only our own pushes are known to be `Local`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Written to the remote by this device.
    Local,
    /// Read from the remote (the changes feed, or a version the remote
    /// already had when we pushed the same content).
    Remote,
    /// Taken from the `files` table when the history was introduced (schema
    /// v3): the revision a file was synced at, of unknown origin and without
    /// its encoding.
    Migrated,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Local => "local",
            Origin::Remote => "remote",
            Origin::Migrated => "migrated",
        }
    }

    fn parse(s: &str) -> Origin {
        match s {
            "local" => Origin::Local,
            "remote" => Origin::Remote,
            _ => Origin::Migrated,
        }
    }
}

/// One file revision in the history (spec D12): enough to rebuild its
/// content from the chunks on the server after CouchDB compacted the body.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RevisionRow {
    pub doc_id: String,
    pub rev: String,
    pub path: String,
    /// The document's `type`: `plain` (text chunks) or `newnote` (base64
    /// chunks). `None` for migrated rows, whose encoding the index never
    /// kept; `hash` tells which reading is right.
    pub kind: Option<String>,
    /// Chunk IDs, in order.
    pub children: Vec<String>,
    /// Chunks kept inline in the document (`eden`), by ID, for the children
    /// that have them: they exist nowhere else on the server.
    pub eden: std::collections::BTreeMap<String, String>,
    pub mtime: i64,
    pub ctime: Option<i64>,
    pub size: u64,
    pub deleted: bool,
    /// xxh64 of the assembled content as 16 hex digits, when this device
    /// had the bytes (applied, pushed, or migrated from a synced file).
    pub hash: Option<String>,
    pub origin: Origin,
    /// Whether the index took this revision as a file's synced version
    /// (written, adopted, pushed, a deletion carried out). `false` for a
    /// revision only seen: held back by a local edit, skipped, or a deletion
    /// the local file survived.
    pub applied: bool,
    /// When this device first recorded the revision (ms since the epoch).
    pub seen_ms: i64,
}

impl RevisionRow {
    /// A file document's revision, as read from the remote or written to it.
    pub fn of_doc(
        doc: &livesync_model::FileDoc,
        hash: Option<String>,
        origin: Origin,
        applied: bool,
        now_ms: i64,
    ) -> Self {
        RevisionRow {
            doc_id: doc.id.clone(),
            rev: doc.rev.clone(),
            path: doc.path.clone(),
            kind: Some(
                match doc.kind {
                    livesync_model::FileKind::Plain => "plain",
                    livesync_model::FileKind::Binary => "newnote",
                }
                .into(),
            ),
            children: doc.children.clone(),
            eden: doc
                .eden
                .iter()
                .filter(|(k, _)| doc.children.contains(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            mtime: doc.mtime,
            ctime: Some(doc.ctime),
            size: doc.size,
            deleted: doc.deleted,
            hash,
            origin,
            applied,
            seen_ms: now_ms,
        }
    }

    /// A deletion known only by its revision (a `_deleted` tombstone, whose
    /// body is gone): the path is the index's.
    pub fn tombstone(doc_id: &str, rev: &str, path: &str, applied: bool, now_ms: i64) -> Self {
        RevisionRow {
            doc_id: doc_id.into(),
            rev: rev.into(),
            path: path.into(),
            kind: None,
            children: vec![],
            eden: Default::default(),
            mtime: 0,
            ctime: None,
            size: 0,
            deleted: true,
            hash: None,
            origin: Origin::Remote,
            applied,
            seen_ms: now_ms,
        }
    }
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
    note          TEXT,
    fold          TEXT
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
CREATE TABLE IF NOT EXISTS local_issues (
    path    TEXT PRIMARY KEY,
    kind    TEXT NOT NULL,
    note    TEXT NOT NULL,
    seen_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS revisions (
    doc_id   TEXT NOT NULL,
    rev      TEXT NOT NULL,
    path     TEXT NOT NULL,
    kind     TEXT,
    children TEXT NOT NULL DEFAULT '[]',
    eden     TEXT,
    mtime    INTEGER NOT NULL,
    ctime    INTEGER,
    size     INTEGER NOT NULL,
    deleted  INTEGER NOT NULL,
    hash     TEXT,
    origin   TEXT NOT NULL,
    applied  INTEGER NOT NULL,
    seen_ms  INTEGER NOT NULL,
    PRIMARY KEY (doc_id, rev)
);
CREATE INDEX IF NOT EXISTS revisions_path ON revisions(path, seen_ms);
"#;

pub const SCHEMA_VERSION: &str = "3";

/// The key two paths collide on when the vault's filesystem folds case and
/// Unicode normalisation: JavaScript lower-casing, then NFC.
pub fn fold_key(path: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    livesync_model::path::js_to_lower_case(path).nfc().collect()
}

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

const REV_COLS: &str =
    "doc_id, rev, path, kind, children, eden, mtime, ctime, size, deleted, hash, origin, applied, seen_ms";

fn revision_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<RevisionRow> {
    let children: String = r.get(4)?;
    let eden: Option<String> = r.get(5)?;
    Ok(RevisionRow {
        doc_id: r.get(0)?,
        rev: r.get(1)?,
        path: r.get(2)?,
        kind: r.get(3)?,
        children: serde_json::from_str(&children).unwrap_or_default(),
        eden: eden.and_then(|e| serde_json::from_str(&e).ok()).unwrap_or_default(),
        mtime: r.get(6)?,
        ctime: r.get(7)?,
        size: r.get::<_, i64>(8)? as u64,
        deleted: r.get(9)?,
        hash: r.get(10)?,
        origin: Origin::parse(&r.get::<_, String>(11)?),
        applied: r.get(12)?,
        seen_ms: r.get(13)?,
    })
}

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
        let version = idx.get_meta("schema_version")?;
        if version.as_deref() == Some("1") {
            idx.migrate_v1()?;
        }
        idx.conn
            .execute_batch("CREATE INDEX IF NOT EXISTS files_fold ON files(fold);")?;
        // "2" also after an older binary opened a v3 index and wrote its own
        // version back: the rows recorded before are kept, and the revisions
        // it synced meanwhile are added now.
        if matches!(version.as_deref(), Some("1" | "2")) {
            idx.migrate_v2(crate::vaultfs::now_ms())?;
        }
        if version.as_deref() != Some(SCHEMA_VERSION) {
            idx.set_meta("schema_version", SCHEMA_VERSION)?;
        }
        Ok(idx)
    }

    /// v1 -> v2: the `fold` column (and the `local_issues` table, created above).
    fn migrate_v1(&self) -> rusqlite::Result<()> {
        let has_fold = self.conn.prepare("SELECT fold FROM files LIMIT 0").is_ok();
        if !has_fold {
            self.conn.execute_batch("ALTER TABLE files ADD COLUMN fold TEXT;")?;
        }
        let paths: Vec<String> = {
            let mut st = self.conn.prepare("SELECT path FROM files")?;
            let rows = st.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let tx = self.conn.unchecked_transaction()?;
        for p in paths {
            tx.execute("UPDATE files SET fold = ?1 WHERE path = ?2", params![fold_key(&p), p])?;
        }
        tx.commit()
    }

    /// v2 -> v3: the `revisions` table (created above), seeded with the
    /// revision each file is synced at, so the history starts with the
    /// versions on disk. Rows already recorded are kept.
    fn migrate_v2(&self, now_ms: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            &format!(
                "INSERT OR IGNORE INTO revisions({REV_COLS})
                 SELECT doc_id, base_rev, path, NULL, base_children, NULL, COALESCE(disk_mtime, 0), NULL,
                        COALESCE(disk_size, 0), 0, disk_hash, ?1, 1, ?2
                 FROM files WHERE base_rev IS NOT NULL"
            ),
            params![Origin::Migrated.as_str(), now_ms],
        )?;
        Ok(())
    }

    /// Run `f` in one transaction (a savepoint, so it nests): everything it
    /// writes is committed together, or nothing is when it fails.
    pub fn atomically<T>(&self, f: impl FnOnce(&Index) -> rusqlite::Result<T>) -> rusqlite::Result<T> {
        self.conn.execute_batch("SAVEPOINT atomically")?;
        match f(self) {
            Ok(v) => {
                self.conn.execute_batch("RELEASE atomically")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK TO atomically; RELEASE atomically");
                Err(e)
            }
        }
    }

    /// Record a revision in the history. Seen again, it keeps when it was
    /// first seen, becomes applied once it is, and stays `local` once this
    /// device is known to have written it.
    pub fn record_revision(&self, r: &RevisionRow) -> rusqlite::Result<()> {
        let eden = (!r.eden.is_empty()).then(|| serde_json::to_string(&r.eden).unwrap());
        self.conn.execute(
            &format!(
                "INSERT INTO revisions({REV_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT(doc_id, rev) DO UPDATE SET path = excluded.path,
                   kind = COALESCE(excluded.kind, revisions.kind),
                   eden = COALESCE(excluded.eden, revisions.eden),
                   ctime = COALESCE(excluded.ctime, revisions.ctime),
                   hash = COALESCE(excluded.hash, revisions.hash),
                   origin = CASE WHEN revisions.origin = 'local' THEN 'local' ELSE excluded.origin END,
                   applied = MAX(revisions.applied, excluded.applied)"
            ),
            params![
                r.doc_id,
                r.rev,
                r.path,
                r.kind,
                serde_json::to_string(&r.children).unwrap(),
                eden,
                r.mtime,
                r.ctime,
                r.size as i64,
                r.deleted,
                r.hash,
                r.origin.as_str(),
                r.applied,
                r.seen_ms,
            ],
        )?;
        Ok(())
    }

    /// One revision of a document.
    pub fn revision(&self, doc_id: &str, rev: &str) -> rusqlite::Result<Option<RevisionRow>> {
        self.conn
            .query_row(
                &format!("SELECT {REV_COLS} FROM revisions WHERE doc_id = ?1 AND rev = ?2"),
                params![doc_id, rev],
                revision_from,
            )
            .optional()
    }

    /// The recorded revisions at `path`, in the order they were first seen.
    pub fn revisions_at(&self, path: &str) -> rusqlite::Result<Vec<RevisionRow>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {REV_COLS} FROM revisions WHERE path = ?1 ORDER BY seen_ms, rowid"
        ))?;
        let rows = st.query_map([path], revision_from)?;
        rows.collect()
    }

    /// How many revisions the history holds.
    pub fn revision_count(&self) -> rusqlite::Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM revisions", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
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
        self.atomically(|ix| ix.upsert_inner(row))
    }

    fn upsert_inner(&self, row: &FileRow) -> rusqlite::Result<()> {
        let tx = &self.conn;
        tx.execute(
            "DELETE FROM files WHERE doc_id = ?1 AND path <> ?2",
            params![row.doc_id, row.path],
        )?;
        tx.execute(
            &format!(
                "INSERT INTO files({COLS}, fold) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(path) DO UPDATE SET doc_id = excluded.doc_id, base_rev = excluded.base_rev,
                   base_children = excluded.base_children, disk_mtime = excluded.disk_mtime,
                   disk_size = excluded.disk_size, disk_hash = excluded.disk_hash,
                   state = excluded.state, note = excluded.note, fold = excluded.fold"
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
                fold_key(&row.path),
            ],
        )?;
        Ok(())
    }

    /// Rows whose path folds to the same key as `path`, other than `doc_id`'s.
    pub fn fold_collisions(&self, path: &str, doc_id: &str) -> rusqlite::Result<Vec<FileRow>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM files WHERE fold = ?1 AND doc_id <> ?2 ORDER BY path"
        ))?;
        let rows = st.query_map(params![fold_key(path), doc_id], row_from)?;
        rows.collect()
    }

    /// Rows at `prefix` itself or below `prefix/`.
    pub fn rows_under(&self, prefix: &str) -> rusqlite::Result<Vec<FileRow>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM files WHERE path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/' ORDER BY path"
        ))?;
        let rows = st.query_map([prefix], row_from)?;
        rows.collect()
    }

    pub fn delete_path(&self, path: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM files WHERE path = ?1", [path])?;
        Ok(())
    }

    pub fn set_issue(&self, path: &str, kind: &str, note: &str, now_ms: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO local_issues(path, kind, note, seen_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET kind = excluded.kind, note = excluded.note, seen_ms = excluded.seen_ms",
            params![path, kind, note, now_ms],
        )?;
        Ok(())
    }

    pub fn clear_issue(&self, path: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM local_issues WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Drop issues for paths a full scan no longer reports.
    pub fn retain_issues(&self, keep: &std::collections::HashSet<String>) -> rusqlite::Result<()> {
        for i in self.issues()? {
            if !keep.contains(&i.path) {
                self.clear_issue(&i.path)?;
            }
        }
        Ok(())
    }

    pub fn issues(&self) -> rusqlite::Result<Vec<LocalIssue>> {
        let mut st = self
            .conn
            .prepare("SELECT path, kind, note, seen_ms FROM local_issues ORDER BY path")?;
        let rows = st.query_map([], |r| {
            Ok(LocalIssue {
                path: r.get(0)?,
                kind: r.get(1)?,
                note: r.get(2)?,
                seen_ms: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Queue a document to be fetched again at once (for example after a
    /// local deletion was not propagated because the remote moved on).
    pub fn add_pending_now(&self, doc_id: &str, reason: &str, now_ms: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO pending(doc_id, rev, reason, attempts, next_attempt_ms) VALUES (?1, NULL, ?2, 0, ?3)
             ON CONFLICT(doc_id) DO UPDATE SET reason = excluded.reason, next_attempt_ms = excluded.next_attempt_ms",
            params![doc_id, reason, now_ms],
        )?;
        Ok(())
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

    /// Rows of `state` whose note starts with `prefix`.
    pub fn list_state_with_note(&self, state: State, prefix: &str, limit: usize) -> rusqlite::Result<Vec<FileRow>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {COLS} FROM files WHERE state = ?1 AND substr(note, 1, length(?2)) = ?2 ORDER BY path LIMIT ?3"
        ))?;
        let rows = st.query_map(params![state.as_str(), prefix, limit as i64], row_from)?;
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
    /// `out_of_scope` rows stay too (without a revision): the file was moved
    /// away on purpose, and the refetch must not write the remote copy back
    /// beside it.
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

    /// When the earliest pending row is due, if there is one.
    pub fn next_pending_due(&self) -> rusqlite::Result<Option<i64>> {
        self.conn
            .query_row("SELECT MIN(next_attempt_ms) FROM pending", [], |r| r.get(0))
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
        let mut o = row("moved.md", "moved.md");
        o.state = State::OutOfScope;
        ix.upsert(&o).unwrap();
        ix.set_meta("since", "\"5-x\"").unwrap();
        ix.add_pending("b.md", Some("1-b"), "missing chunks", 0).unwrap();
        ix.reset_for_rebuild().unwrap();
        let r = ix.by_doc("a.md").unwrap().unwrap();
        assert_eq!(r.base_rev, None);
        assert_eq!(r.disk_hash.as_deref(), Some("00"));
        assert!(ix.by_doc(".obsidian/x").unwrap().is_none());
        let o = ix.by_doc("moved.md").unwrap().unwrap();
        assert_eq!((o.state, o.base_rev), (State::OutOfScope, None));
        assert!(ix.get_meta("since").unwrap().is_none());
        assert!(ix.pending(None).unwrap().is_empty());
    }

    #[test]
    fn fold_and_issues() {
        let ix = Index::open_in_memory().unwrap();
        ix.upsert(&row("Notes/Café.md", "notes/café.md")).unwrap();
        // NFD spelling and other case fold to the same key.
        let other = "notes/cafe\u{301}.md";
        assert_eq!(ix.fold_collisions(other, "x").unwrap().len(), 1);
        assert!(ix.fold_collisions("Notes/Café.md", "notes/café.md").unwrap().is_empty());
        assert_eq!(ix.rows_under("Notes").unwrap().len(), 1);
        assert!(ix.rows_under("Note").unwrap().is_empty());
        ix.set_issue("a.md", "case-collision", "x", 1).unwrap();
        ix.set_issue("b.md", "too-large", "y", 1).unwrap();
        ix.retain_issues(&["b.md".to_string()].into_iter().collect()).unwrap();
        assert_eq!(ix.issues().unwrap().len(), 1);
    }

    #[test]
    fn migrates_v1_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files (path TEXT PRIMARY KEY, doc_id TEXT NOT NULL UNIQUE, base_rev TEXT,
               base_children TEXT NOT NULL DEFAULT '[]', disk_mtime INTEGER, disk_size INTEGER,
               disk_hash TEXT, state TEXT NOT NULL, note TEXT);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '1');
             INSERT INTO files(path, doc_id, state) VALUES ('A.md', 'a.md', 'clean');",
        )
        .unwrap();
        let ix = Index::init(conn).unwrap();
        assert_eq!(ix.get_meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
        assert_eq!(ix.fold_collisions("a.MD", "other").unwrap().len(), 1);
    }

    /// A v2 index as the previous release wrote it.
    fn v2_index() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files (path TEXT PRIMARY KEY, doc_id TEXT NOT NULL UNIQUE, base_rev TEXT,
               base_children TEXT NOT NULL DEFAULT '[]', disk_mtime INTEGER, disk_size INTEGER,
               disk_hash TEXT, state TEXT NOT NULL, note TEXT, fold TEXT);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE pending (doc_id TEXT PRIMARY KEY, rev TEXT, reason TEXT NOT NULL,
               attempts INTEGER NOT NULL DEFAULT 0, next_attempt_ms INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE local_issues (path TEXT PRIMARY KEY, kind TEXT NOT NULL, note TEXT NOT NULL,
               seen_ms INTEGER NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '2');
             INSERT INTO files VALUES ('A.md', 'a.md', '3-a', '[\"h:1\",\"h:2\"]', 10, 20, 'ab', 'clean', NULL, 'a.md');
             INSERT INTO files VALUES ('d.md', 'd.md', '1-d', '[]', 1, 0, 'cd', 'dirty', 'x', 'd.md');
             INSERT INTO files VALUES ('n.md', 'n.md', NULL, '[]', 1, 1, 'ef', 'clean', NULL, 'n.md');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn migrates_v2_indexes_seeding_the_history() {
        let ix = Index::init(v2_index()).unwrap();
        assert_eq!(ix.get_meta("schema_version").unwrap().as_deref(), Some("3"));
        // One row per file with a synced revision.
        assert_eq!(ix.revision_count().unwrap(), 2);
        let a = ix.revision("a.md", "3-a").unwrap().unwrap();
        assert_eq!(a.path, "A.md");
        assert_eq!(a.children, vec!["h:1", "h:2"]);
        assert_eq!((a.mtime, a.size, a.hash.as_deref()), (10, 20, Some("ab")));
        assert_eq!(
            (a.origin, a.applied, a.deleted, a.kind.as_deref()),
            (Origin::Migrated, true, false, None)
        );
        assert_eq!(ix.revisions_at("d.md").unwrap().len(), 1);
        assert!(ix.revisions_at("n.md").unwrap().is_empty());
        // The old tables are untouched.
        assert_eq!(ix.all_rows().unwrap().len(), 3);
    }

    #[test]
    fn a_v3_index_opened_by_an_older_binary_catches_up_on_return() {
        let ix = Index::init(v2_index()).unwrap();
        let mut r = ix.revision("a.md", "3-a").unwrap().unwrap();
        r.rev = "4-b".into();
        r.origin = Origin::Local;
        ix.record_revision(&r).unwrap();
        // The older binary syncs on, and writes its own version back.
        ix.conn
            .execute_batch(
                "UPDATE files SET base_rev = '5-c' WHERE path = 'A.md';
                 UPDATE meta SET value = '2' WHERE key = 'schema_version';",
            )
            .unwrap();
        let Index { conn } = ix;
        let ix = Index::init(conn).unwrap();
        let revs: Vec<(String, Origin)> = ix
            .revisions_at("A.md")
            .unwrap()
            .into_iter()
            .map(|r| (r.rev, r.origin))
            .collect();
        assert_eq!(
            revs,
            vec![
                ("3-a".into(), Origin::Migrated),
                ("4-b".into(), Origin::Local),
                ("5-c".into(), Origin::Migrated)
            ]
        );
        assert_eq!(ix.get_meta("schema_version").unwrap().as_deref(), Some("3"));
        // Opening a current index again changes nothing.
        let Index { conn } = ix;
        assert_eq!(Index::init(conn).unwrap().revision_count().unwrap(), 4);
    }

    fn rev(doc: &str, rev: &str) -> RevisionRow {
        RevisionRow {
            doc_id: doc.into(),
            rev: rev.into(),
            path: doc.into(),
            kind: Some("plain".into()),
            children: vec!["h:1".into()],
            eden: [("h:1".to_string(), "x".to_string())].into_iter().collect(),
            mtime: 5,
            ctime: Some(4),
            size: 1,
            deleted: false,
            hash: None,
            origin: Origin::Remote,
            applied: false,
            seen_ms: 100,
        }
    }

    #[test]
    fn a_revision_seen_again_keeps_what_was_known() {
        let ix = Index::open_in_memory().unwrap();
        ix.record_revision(&rev("a.md", "1-a")).unwrap();
        assert_eq!(ix.revision("a.md", "1-a").unwrap().unwrap(), rev("a.md", "1-a"));
        let mut again = rev("a.md", "1-a");
        again.applied = true;
        again.hash = Some("ff".into());
        again.seen_ms = 200;
        ix.record_revision(&again).unwrap();
        let got = ix.revision("a.md", "1-a").unwrap().unwrap();
        assert_eq!((got.applied, got.hash.as_deref(), got.seen_ms), (true, Some("ff"), 100));
        // Applied stays applied, and ours stays ours.
        let mut ours = rev("a.md", "2-b");
        ours.origin = Origin::Local;
        ours.applied = true;
        ix.record_revision(&ours).unwrap();
        ix.record_revision(&rev("a.md", "2-b")).unwrap();
        let got = ix.revision("a.md", "2-b").unwrap().unwrap();
        assert_eq!((got.origin, got.applied), (Origin::Local, true));
    }

    #[test]
    fn atomically_commits_all_or_nothing() {
        let ix = Index::open_in_memory().unwrap();
        let failed: rusqlite::Result<()> = ix.atomically(|ix| {
            ix.upsert(&row("a.md", "a.md"))?;
            ix.record_revision(&rev("a.md", "1-a"))?;
            Err(rusqlite::Error::InvalidQuery)
        });
        assert!(failed.is_err());
        assert!(ix.by_doc("a.md").unwrap().is_none());
        assert_eq!(ix.revision_count().unwrap(), 0);
        ix.atomically(|ix| {
            ix.upsert(&row("a.md", "a.md"))?;
            ix.record_revision(&rev("a.md", "1-a"))
        })
        .unwrap();
        assert!(ix.by_doc("a.md").unwrap().is_some());
        assert_eq!(ix.revision_count().unwrap(), 1);
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
        assert_eq!(ix.next_pending_due().unwrap(), Some(91_000));
        ix.remove_pending("b.md").unwrap();
        assert_eq!(ix.next_pending_due().unwrap(), None);
    }
}
