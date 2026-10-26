//! The push side (spec "Push loop", D4): local changes go to the remote.
//!
//! The unit of work is a document: a vault file and its index row. A pass
//! compares what is on disk with the index (size and mtime first, the xxh64
//! of the bytes when those differ) and then:
//!
//! - **changed or new file:** split it, upload the chunks the remote lacks,
//!   then write the metadata: `PUT` against `base_rev` (`live-base`); on 409
//!   a child of `base_rev` with a random revision ID through `_bulk_docs`
//!   `new_edits:false` (`force-base`). With no known base the document is
//!   read first: identical content is adopted, a deleted one is replaced,
//!   anything else gets an independent root (a conflict, never an overwrite).
//! - **vanished file:** find out where it went. A move into `.trash/` or out
//!   of the vault is a delete; a move to an in-scope path is a rename
//!   (delete plus create); a move out of the folder scope (or into another
//!   dot-folder) is not a delete: the row becomes `out_of_scope`. Deletes are
//!   soft deletes written against `base_rev`, only for clean rows; anything
//!   else is fetched again instead.
//!
//! Chunks always go before metadata.

use crate::engine::{meta_keys as meta, Engine, EngineError};
use crate::index::{FileRow, State};
use crate::vaultfs::{self, DiskState};
use crate::watcher::{NoWatcher, WatchEvent, Watcher};
use livesync_model::path::{classify_path, path_to_id};
use livesync_model::splitter::chunk_file;
use livesync_model::FileKind;
use livesync_remote::{Backoff, PutOutcome};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, UNIX_EPOCH};
use tokio::sync::watch;
use tracing::{debug, info, warn};

type Result<T> = std::result::Result<T, EngineError>;

/// Counters for push passes.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PushReport {
    pub passes: u64,
    /// Files looked at (stat).
    pub scanned: u64,
    /// Files read and hashed because size or mtime changed.
    pub hashed: u64,
    /// Documents written (updates and new files).
    pub pushed: u64,
    pub created: u64,
    /// Written as a branch (`force-base` or an independent root).
    pub conflicts: u64,
    /// New local files whose content the remote already had.
    pub adopted: u64,
    pub chunks_uploaded: u64,
    pub chunk_bytes: u64,
    /// Soft deletes written.
    pub deleted: u64,
    /// Vanished files whose remote copy moved on (or was not clean): fetched
    /// again instead of deleted.
    pub kept_remote: u64,
    /// Files moved out of the folder scope (remote left alone).
    pub out_of_scope: u64,
    /// Files still being written; looked at again later.
    pub deferred: u64,
    /// Local files that cannot be synced (see `status`).
    pub issues: u64,
    /// A mass deletion was refused.
    pub safety_stops: u64,
}

impl PushReport {
    fn add(&mut self, o: &PushReport) {
        self.passes += o.passes;
        self.scanned += o.scanned;
        self.hashed += o.hashed;
        self.pushed += o.pushed;
        self.created += o.created;
        self.conflicts += o.conflicts;
        self.adopted += o.adopted;
        self.chunks_uploaded += o.chunks_uploaded;
        self.chunk_bytes += o.chunk_bytes;
        self.deleted += o.deleted;
        self.kept_remote += o.kept_remote;
        self.out_of_scope += o.out_of_scope;
        self.deferred += o.deferred;
        self.issues += o.issues;
        self.safety_stops += o.safety_stops;
    }

    fn is_quiet(&self) -> bool {
        self.pushed + self.deleted + self.kept_remote + self.out_of_scope + self.adopted == 0
    }
}

/// Renames reported by the watcher, kept for a minute so a vanished file
/// can be matched with its destination.
#[derive(Debug, Default)]
pub(crate) struct RenameHints {
    pairs: Vec<(String, String, Instant)>,
}

impl RenameHints {
    pub(crate) fn add(&mut self, from: String, to: String) {
        self.pairs.push((from, to, Instant::now()));
        let cutoff = Instant::now() - Duration::from_secs(60);
        self.pairs.retain(|p| p.2 >= cutoff);
    }

    /// Where `path` went, following file or folder renames.
    pub(crate) fn dest(&self, path: &str) -> Option<String> {
        self.pairs.iter().rev().find_map(|(from, to, _)| {
            if path == from {
                Some(to.clone())
            } else {
                path.strip_prefix(from.as_str())
                    .and_then(|rest| rest.strip_prefix('/'))
                    .map(|rest| format!("{to}/{rest}"))
            }
        })
    }
}

/// A file found on disk.
#[derive(Debug, Clone)]
struct Found {
    rel: String,
    size: u64,
    mtime_ms: i64,
}

fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ctime_ms(md: &std::fs::Metadata) -> i64 {
    md.created()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(|| mtime_ms(md))
}

fn rel_of(root: &Path, p: &Path) -> Option<String> {
    let r = p.strip_prefix(root).ok()?;
    let s = r.to_str()?;
    (!s.is_empty()).then(|| s.replace('\\', "/"))
}

fn random_rev_id() -> String {
    format!("{:016x}{:016x}", rand::random::<u64>(), rand::random::<u64>())
}

/// `N-hash` -> `(N, hash)`.
fn split_rev(rev: &str) -> Option<(u64, &str)> {
    let (n, h) = rev.split_once('-')?;
    Some((n.parse().ok()?, h))
}

/// Whether the path lies in the vault's root `.trash/` (Obsidian's "move to
/// Obsidian trash", which is how Obsidian deletes).
fn in_trash(rel: &str) -> bool {
    rel == ".trash" || rel.starts_with(".trash/")
}

/// What became of a vanished file.
enum Fate {
    Delete,
    OutOfScope(String),
}

impl Engine {
    fn ids_case_insensitive(&self) -> bool {
        !self.inner.runtime.lock().unwrap().case_sensitive_ids
    }

    fn doc_id_for(&self, rel: &str) -> String {
        path_to_id(rel, self.ids_case_insensitive())
    }

    /// Whether a local path takes part in sync: not a built-in skip, not
    /// ignored, inside the folder scope.
    pub(crate) fn local_visible(&self, rel: &str) -> bool {
        classify_path(rel).is_none()
            && !self.inner.runtime.lock().unwrap().ignored(rel)
            && self.inner.cfg.scope.contains(rel)
    }

    /// All visible files under `start` (a vault-relative folder, "" for the root).
    fn walk_visible(&self, start: &str, out: &mut Vec<Found>) {
        let root = self.inner.cfg.vault.clone();
        let mut stack = vec![if start.is_empty() {
            root.clone()
        } else {
            root.join(start)
        }];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') {
                    continue;
                }
                let Ok(ft) = e.file_type() else { continue };
                let Some(rel) = rel_of(&root, &e.path()) else { continue };
                if ft.is_dir() {
                    if self.inner.cfg.scope.may_contain_under(&rel) {
                        stack.push(e.path());
                    }
                } else if ft.is_file() && self.local_visible(&rel) {
                    if let Ok(md) = e.metadata() {
                        out.push(Found {
                            rel,
                            size: md.len(),
                            mtime_ms: mtime_ms(&md),
                        });
                    }
                }
            }
        }
    }

    /// The index row a local file belongs to: by exact path, by document ID,
    /// and on folding filesystems by the folded name.
    fn row_for_local(&self, rel: &str) -> Result<Option<FileRow>> {
        let id = self.doc_id_for(rel);
        let folds = self.inner.fs.folds();
        self.with_index(|ix| {
            if let Some(r) = ix.by_path(rel)? {
                return Ok(Some(r));
            }
            if let Some(r) = ix.by_doc(&id)? {
                return Ok(Some(r));
            }
            if folds {
                return Ok(ix.fold_collisions(rel, "")?.into_iter().next());
            }
            Ok(None)
        })
    }

    // ------------------------------------------------------------------
    // Passes
    // ------------------------------------------------------------------

    /// Compare the whole vault with the index and push what changed. With
    /// `defer_recent`, files modified within the settle time are returned
    /// for later instead of pushed now.
    pub async fn push_all(&self, defer_recent: bool) -> Result<PushReport> {
        let (rep, _) = self.full_pass(&RenameHints::default(), defer_recent).await?;
        Ok(rep)
    }

    async fn full_pass(&self, hints: &RenameHints, defer_recent: bool) -> Result<(PushReport, Vec<String>)> {
        let _ops = self.inner.ops.lock().await;
        let mut rep = PushReport {
            passes: 1,
            ..Default::default()
        };
        let this = self.clone();
        let found = tokio::task::spawn_blocking(move || {
            let mut v = Vec::new();
            this.walk_visible("", &mut v);
            v
        })
        .await
        .map_err(EngineError::other)?;
        rep.scanned = found.len() as u64;
        let rows = self.with_index(|ix| ix.all_rows())?;
        let mut issues_seen: HashSet<String> = HashSet::new();

        // Group local files by document ID: two names for one document is a
        // case collision on a case-sensitive filesystem.
        let mut by_id: BTreeMap<String, Vec<Found>> = BTreeMap::new();
        for f in found {
            by_id.entry(self.doc_id_for(&f.rel)).or_default().push(f);
        }
        let row_by_id: HashMap<String, FileRow> = rows.iter().map(|r| (r.doc_id.clone(), r.clone())).collect();
        let row_by_fold: HashMap<String, FileRow> = if self.inner.fs.folds() {
            rows.iter()
                .map(|r| (crate::index::fold_key(&r.path), r.clone()))
                .collect()
        } else {
            HashMap::new()
        };
        let mut present: HashSet<String> = HashSet::new();
        let mut work: Vec<(Found, Option<FileRow>)> = Vec::new();
        let mut deferred = Vec::new();
        let now = vaultfs::now_ms();
        for (id, mut files) in by_id {
            let row = row_by_id
                .get(&id)
                .cloned()
                .or_else(|| row_by_fold.get(&crate::index::fold_key(&files[0].rel)).cloned());
            if files.len() > 1 {
                files.sort_by(|a, b| a.rel.cmp(&b.rel));
                let keep = row
                    .as_ref()
                    .and_then(|r| files.iter().position(|f| f.rel == r.path))
                    .unwrap_or(0);
                let kept = files.remove(keep);
                for f in &files {
                    let note = format!("case collision with {}; not synced", kept.rel);
                    warn!("{}: {note}", f.rel);
                    self.with_index(|ix| ix.set_issue(&f.rel, "case-collision", &note, now))?;
                    issues_seen.insert(f.rel.clone());
                    rep.issues += 1;
                }
                files = vec![kept];
            }
            let f = files.pop().expect("one file");
            if let Some(r) = &row {
                present.insert(r.doc_id.clone());
                if r.state == State::Skipped {
                    continue;
                }
                if r.disk_size == Some(f.size)
                    && r.disk_mtime == Some(f.mtime_ms)
                    && r.base_rev.is_some()
                    && r.state != State::OutOfScope
                {
                    continue; // unchanged
                }
            }
            if defer_recent && (now - f.mtime_ms).abs() < self.inner.cfg.settle.as_millis() as i64 {
                deferred.push(f.rel.clone());
                rep.deferred += 1;
                continue;
            }
            work.push((f, row));
        }
        for (f, row) in work {
            match self.push_local(&f.rel, row, &mut rep).await {
                Ok(Some(issue)) => {
                    issues_seen.insert(issue);
                }
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }
        let vanished: Vec<FileRow> = rows
            .into_iter()
            .filter(|r| !present.contains(&r.doc_id))
            .filter(|r| matches!(r.state, State::Clean | State::Dirty | State::Conflicted))
            .collect();
        self.handle_vanished(vanished, hints, &mut rep).await?;
        if deferred.is_empty() {
            self.with_index(|ix| ix.retain_issues(&issues_seen))?;
        }
        self.finish_pass(&rep)?;
        Ok((rep, deferred))
    }

    /// Look at specific paths (files, folders, or paths that vanished).
    async fn partial_pass(&self, paths: Vec<String>, hints: &RenameHints) -> Result<PushReport> {
        let _ops = self.inner.ops.lock().await;
        let mut rep = PushReport {
            passes: 1,
            ..Default::default()
        };
        let mut files: BTreeMap<String, Found> = BTreeMap::new();
        let mut gone: BTreeMap<String, FileRow> = BTreeMap::new();
        for p in paths {
            let abs = self.inner.cfg.vault.join(&p);
            match std::fs::symlink_metadata(&abs) {
                Ok(md) if md.is_dir() => {
                    let mut v = Vec::new();
                    self.walk_visible(&p, &mut v);
                    let here: HashSet<String> = v.iter().map(|f| f.rel.clone()).collect();
                    for f in v {
                        files.insert(f.rel.clone(), f);
                    }
                    // Rows under the folder whose files are gone.
                    for r in self.with_index(|ix| ix.rows_under(&p))? {
                        if !here.contains(&r.path) && !self.inner.cfg.vault.join(&r.path).exists() {
                            gone.insert(r.doc_id.clone(), r);
                        }
                    }
                }
                Ok(md) if md.is_file() => {
                    if self.local_visible(&p) {
                        files.insert(
                            p.clone(),
                            Found {
                                rel: p.clone(),
                                size: md.len(),
                                mtime_ms: mtime_ms(&md),
                            },
                        );
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    for r in self.with_index(|ix| ix.rows_under(&p))? {
                        if !self.inner.cfg.vault.join(&r.path).exists() {
                            gone.insert(r.doc_id.clone(), r);
                        }
                    }
                }
            }
        }
        rep.scanned = files.len() as u64;
        for (_, f) in files {
            let row = self.row_for_local(&f.rel)?;
            if let Some(r) = &row {
                if r.state == State::Skipped {
                    continue;
                }
                if r.disk_size == Some(f.size)
                    && r.disk_mtime == Some(f.mtime_ms)
                    && r.base_rev.is_some()
                    && r.state != State::OutOfScope
                {
                    continue;
                }
            }
            self.push_local(&f.rel, row, &mut rep).await?;
        }
        let vanished: Vec<FileRow> = gone
            .into_values()
            .filter(|r| matches!(r.state, State::Clean | State::Dirty | State::Conflicted))
            .collect();
        self.handle_vanished(vanished, hints, &mut rep).await?;
        self.finish_pass(&rep)?;
        Ok(rep)
    }

    fn finish_pass(&self, rep: &PushReport) -> Result<()> {
        if !rep.is_quiet() {
            info!(
                "push: {} written ({} new, {} as conflict branches), {} adopted, {} chunks ({:.1} MB) uploaded, {} deleted, {} kept from the remote, {} moved out of scope",
                rep.pushed,
                rep.created,
                rep.conflicts,
                rep.adopted,
                rep.chunks_uploaded,
                rep.chunk_bytes as f64 / 1e6,
                rep.deleted,
                rep.kept_remote,
                rep.out_of_scope
            );
            self.with_index(|ix| ix.set_meta(meta::LAST_PUSH, &vaultfs::now_ms().to_string()))?;
        }
        if rep.safety_stops == 0 {
            self.with_index(|ix| ix.delete_meta(meta::SAFETY_STOP))?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // One file
    // ------------------------------------------------------------------

    /// Push one local file if its content differs from its row. Returns the
    /// path when it was recorded as a local issue.
    async fn push_local(&self, rel: &str, row: Option<FileRow>, rep: &mut PushReport) -> Result<Option<String>> {
        let abs = self.inner.cfg.vault.join(rel);
        let read = {
            let abs = abs.clone();
            tokio::task::spawn_blocking(move || -> std::io::Result<Option<(Vec<u8>, std::fs::Metadata)>> {
                let before = std::fs::metadata(&abs)?;
                let bytes = std::fs::read(&abs)?;
                let after = std::fs::metadata(&abs)?;
                if before.len() != after.len()
                    || mtime_ms(&before) != mtime_ms(&after)
                    || bytes.len() as u64 != after.len()
                {
                    return Ok(None); // still being written
                }
                Ok(Some((bytes, after)))
            })
            .await
            .map_err(EngineError::other)?
        };
        let (bytes, md) = match read {
            Ok(Some(x)) => x,
            Ok(None) => {
                rep.deferred += 1;
                return Ok(None);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                let note = format!("cannot read: {e}");
                warn!("{rel}: {note}");
                self.with_index(|ix| ix.set_issue(rel, "unreadable", &note, vaultfs::now_ms()))?;
                rep.issues += 1;
                return Ok(Some(rel.to_string()));
            }
        };
        rep.hashed += 1;
        let hash = vaultfs::hash_bytes(&bytes);
        let disk = DiskState {
            hash: hash.clone(),
            size: bytes.len() as u64,
            mtime_ms: mtime_ms(&md),
            #[cfg(unix)]
            ino: (0, 0),
        };
        if let Some(r) = &row {
            if r.disk_hash.as_deref() == Some(hash.as_str()) && r.base_rev.is_some() {
                // Same content as the base: only the stat changed (a touch,
                // or the user reverted an edit, or moved the file back).
                let mut r = r.clone();
                let was = r.state;
                r.disk_mtime = Some(disk.mtime_ms);
                r.disk_size = Some(disk.size);
                if matches!(was, State::Dirty | State::OutOfScope) {
                    r.state = State::Clean;
                    r.note = None;
                }
                self.with_index(|ix| {
                    ix.upsert(&r)?;
                    if was == State::Dirty {
                        // A remote change was held back for this file: apply it now.
                        ix.add_pending_now(&r.doc_id, "local file back to its synced version", vaultfs::now_ms())?;
                    }
                    Ok(())
                })?;
                return Ok(None);
            }
        }
        let max = self.inner.runtime.lock().unwrap().max_size_bytes;
        if let Some(max) = max {
            if disk.size > max {
                let note = format!("{} bytes is over the size limit; not synced", disk.size);
                self.with_index(|ix| ix.set_issue(rel, "too-large", &note, vaultfs::now_ms()))?;
                rep.issues += 1;
                return Ok(Some(rel.to_string()));
            }
        }
        // Keep the remote spelling when the filesystem says it is the same file.
        let doc_path = match &row {
            Some(r) if self.inner.fs.folds() && crate::index::fold_key(&r.path) == crate::index::fold_key(rel) => {
                r.path.clone()
            }
            _ => rel.to_string(),
        };
        let doc_id = row
            .as_ref()
            .filter(|r| r.path == doc_path || crate::index::fold_key(&r.path) == crate::index::fold_key(&doc_path))
            .map(|r| r.doc_id.clone())
            .unwrap_or_else(|| self.doc_id_for(&doc_path));
        let split = self.inner.runtime.lock().unwrap().split;
        let chunked = {
            let p = doc_path.clone();
            tokio::task::spawn_blocking(move || chunk_file(&p, &bytes, &split))
                .await
                .map_err(EngineError::other)?
        };
        let mut issue = None;
        if chunked.lossy {
            let note = "not valid UTF-8; synced with replacement characters".to_string();
            warn!("{rel}: {note}");
            self.with_index(|ix| ix.set_issue(rel, "not-utf8", &note, vaultfs::now_ms()))?;
            issue = Some(rel.to_string());
        } else {
            self.with_index(|ix| ix.clear_issue(rel))?;
        }
        let known: HashSet<String> = row
            .as_ref()
            .map(|r| r.base_children.iter().cloned().collect())
            .unwrap_or_default();
        let (n, b) = self.upload_chunks(&chunked.chunks, &known).await?;
        rep.chunks_uploaded += n;
        rep.chunk_bytes += b;
        let children: Vec<String> = chunked.chunks.iter().map(|(id, _)| id.clone()).collect();
        let doc = json!({
            "_id": doc_id,
            "path": doc_path,
            "ctime": ctime_ms(&md),
            "mtime": disk.mtime_ms,
            "size": chunked.size,
            "type": match chunked.kind { FileKind::Plain => "plain", FileKind::Binary => "newnote" },
            "children": children,
            "eden": {},
        });
        let base = row.as_ref().and_then(|r| r.base_rev.clone());
        let written = self.write_meta(doc, base.as_deref()).await?;
        let state = if written.branched {
            State::Conflicted
        } else {
            State::Clean
        };
        if written.branched {
            rep.conflicts += 1;
            warn!(
                "{doc_path}: the remote changed too; the local edit was saved as a conflicting revision ({})",
                written.rev
            );
        }
        if written.adopted {
            rep.adopted += 1;
        } else {
            rep.pushed += 1;
            if base.is_none() {
                rep.created += 1;
            }
        }
        let new_row = FileRow {
            path: doc_path.clone(),
            doc_id: doc_id.clone(),
            base_rev: Some(written.rev.clone()),
            base_children: children,
            disk_mtime: Some(disk.mtime_ms),
            disk_size: Some(disk.size),
            disk_hash: Some(hash),
            state,
            note: written
                .branched
                .then(|| "local edit saved as a conflicting revision; resolve with an official client".into()),
        };
        self.with_index(|ix| {
            if let Some(r) = &row {
                if r.doc_id != doc_id {
                    ix.delete_doc(&r.doc_id)?;
                }
            }
            ix.upsert(&new_row)?;
            ix.remove_pending(&doc_id)
        })?;
        debug!("pushed {doc_path} as {}", written.rev);
        Ok(issue)
    }

    /// Upload the chunks the remote does not have yet. `known` are chunk IDs
    /// of the base revision (present on the remote already).
    async fn upload_chunks(&self, chunks: &[(String, String)], known: &HashSet<String>) -> Result<(u64, u64)> {
        let mut seen = HashSet::new();
        let fresh: Vec<&(String, String)> = chunks
            .iter()
            .filter(|(id, _)| !known.contains(id) && seen.insert(id.clone()))
            .collect();
        if fresh.is_empty() {
            return Ok((0, 0));
        }
        let mut missing: Vec<&(String, String)> = Vec::new();
        for group in fresh.chunks(100) {
            let keys: Vec<String> = group.iter().map(|(id, _)| id.clone()).collect();
            let rows = self.inner.couch.all_docs_revs(&keys).await?;
            let present: HashSet<String> = rows
                .iter()
                .filter(|r| r.error.is_none())
                .filter(|r| {
                    !r.value
                        .as_ref()
                        .and_then(|v| v.get("deleted"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .filter_map(|r| r.key.clone())
                .collect();
            missing.extend(group.iter().filter(|(id, _)| !present.contains(id)));
        }
        const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
        let (mut count, mut bytes) = (0u64, 0u64);
        let mut batch: Vec<Value> = Vec::new();
        let mut batch_bytes = 0usize;
        for (i, (id, data)) in missing.iter().enumerate() {
            batch.push(json!({"_id": id, "type": "leaf", "data": data}));
            batch_bytes += data.len();
            count += 1;
            bytes += data.len() as u64;
            if batch_bytes >= MAX_BATCH_BYTES || batch.len() >= 100 || i + 1 == missing.len() {
                let results = self.inner.couch.bulk_docs(&batch, true).await?;
                for r in results {
                    // `conflict`: the chunk exists already (same ID, same content).
                    if let Some(e) = r.error.as_deref().filter(|e| *e != "conflict") {
                        return Err(EngineError::Other(format!(
                            "uploading chunk {}: {e} {}",
                            r.id.unwrap_or_default(),
                            r.reason.unwrap_or_default()
                        )));
                    }
                }
                batch.clear();
                batch_bytes = 0;
            }
        }
        Ok((count, bytes))
    }

    /// Write file metadata against `base` (D4), or with unknown ancestry.
    async fn write_meta(&self, doc: Value, base: Option<&str>) -> Result<Written> {
        match base {
            Some(b) => {
                let mut d = doc.clone();
                d["_rev"] = json!(b);
                match self.inner.couch.put_doc(&d).await? {
                    PutOutcome::Saved { rev } => Ok(Written::plain(rev)),
                    PutOutcome::Conflict => self.force_base(doc, b).await,
                }
            }
            None => self.write_unknown_ancestry(doc).await,
        }
    }

    /// `force-base`: the edit as a child of `base`, kept even though the
    /// remote moved on (a conflict the official clients resolve).
    async fn force_base(&self, doc: Value, base: &str) -> Result<Written> {
        let (n, h) = split_rev(base).ok_or_else(|| EngineError::Other(format!("malformed revision {base}")))?;
        let r = random_rev_id();
        let rev = format!("{}-{r}", n + 1);
        let mut d = doc;
        d["_rev"] = json!(rev);
        d["_revisions"] = json!({"start": n + 1, "ids": [r, h]});
        self.import_rev(d, &rev).await?;
        Ok(Written {
            rev,
            branched: true,
            adopted: false,
        })
    }

    /// Store a document revision as given (`new_edits:false`) and check it landed.
    async fn import_rev(&self, d: Value, rev: &str) -> Result<()> {
        let id = d["_id"].as_str().unwrap_or_default().to_string();
        let results = self.inner.couch.bulk_docs(std::slice::from_ref(&d), false).await?;
        if let Some(e) = results.iter().find_map(|r| r.error.clone()) {
            return Err(EngineError::Other(format!("writing {id} at {rev}: {e}")));
        }
        match self.inner.couch.fetch_rev(&id, rev).await? {
            livesync_remote::DocFetch::Found(_) => Ok(()),
            other => Err(EngineError::Other(format!(
                "{id}: revision {rev} not stored ({other:?})"
            ))),
        }
    }

    /// No known base: read the current document first.
    async fn write_unknown_ancestry(&self, doc: Value) -> Result<Written> {
        let id = doc["_id"].as_str().unwrap_or_default().to_string();
        for _ in 0..3 {
            match self.inner.couch.fetch_doc(&id, true).await? {
                livesync_remote::DocFetch::Found(cur) => {
                    let rev = cur["_rev"].as_str().unwrap_or_default().to_string();
                    let soft_deleted = cur.get("deleted").and_then(Value::as_bool).unwrap_or(false);
                    if soft_deleted {
                        // Replaces a deletion, as upstream's "latest" mode does.
                        let mut d = doc.clone();
                        d["_rev"] = json!(rev);
                        match self.inner.couch.put_doc(&d).await? {
                            PutOutcome::Saved { rev } => return Ok(Written::plain(rev)),
                            PutOutcome::Conflict => continue,
                        }
                    }
                    if cur.get("children") == doc.get("children") && cur.get("type") == doc.get("type") {
                        let conflicted = cur
                            .get("_conflicts")
                            .and_then(Value::as_array)
                            .is_some_and(|a| !a.is_empty());
                        return Ok(Written {
                            rev,
                            branched: conflicted,
                            adopted: true,
                        });
                    }
                    // Different content of unknown origin: an independent root,
                    // so neither version is lost.
                    let r = random_rev_id();
                    let rev = format!("1-{r}");
                    let mut d = doc.clone();
                    d["_rev"] = json!(rev);
                    d["_revisions"] = json!({"start": 1, "ids": [r]});
                    self.import_rev(d, &rev).await?;
                    return Ok(Written {
                        rev,
                        branched: true,
                        adopted: false,
                    });
                }
                livesync_remote::DocFetch::Deleted | livesync_remote::DocFetch::Missing(_) => {
                    match self.inner.couch.put_doc(&doc).await? {
                        PutOutcome::Saved { rev } => return Ok(Written::plain(rev)),
                        PutOutcome::Conflict => continue, // created meanwhile; look again
                    }
                }
            }
        }
        Err(EngineError::Other(format!(
            "{id}: the document kept changing while writing it"
        )))
    }

    // ------------------------------------------------------------------
    // Vanished files
    // ------------------------------------------------------------------

    async fn handle_vanished(&self, rows: Vec<FileRow>, hints: &RenameHints, rep: &mut PushReport) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        // The vault folder itself must be there; an unmounted card or a
        // revoked permission must not look like "everything was deleted".
        if std::fs::read_dir(&self.inner.cfg.vault).is_err() {
            return Err(EngineError::Other(format!(
                "the vault folder {} cannot be read; not treating missing files as deleted",
                self.inner.cfg.vault.display()
            )));
        }
        let tracked = self.with_index(|ix| ix.counts_by_state())?;
        let tracked: u64 = tracked
            .iter()
            .filter(|(s, _)| s == "clean" || s == "dirty" || s == "conflicted")
            .map(|(_, n)| *n)
            .sum();
        let n = rows.len() as u64;
        if n > 20 && n * 2 > tracked && !self.inner.cfg.allow_mass_delete {
            let msg = format!(
                "{n} of {tracked} tracked files are missing from the vault; not deleting them remotely (run with --allow-mass-delete if this is intended)"
            );
            warn!("{msg}");
            self.with_index(|ix| ix.set_meta(meta::SAFETY_STOP, &format!("{} {msg}", vaultfs::now_ms())))?;
            rep.safety_stops += 1;
            return Ok(());
        }
        let mut fates: Vec<(FileRow, Fate)> = Vec::new();
        let mut unknown: Vec<FileRow> = Vec::new();
        for r in rows {
            match hints.dest(&r.path) {
                Some(d) => {
                    let fate = self.fate_of(&d);
                    fates.push((r, fate));
                }
                None => unknown.push(r),
            }
        }
        if !unknown.is_empty() {
            let found = self.find_moved_out(&unknown).await?;
            for r in unknown {
                let fate = match found.get(&r.doc_id) {
                    Some(d) => self.fate_of(d),
                    None => Fate::Delete,
                };
                fates.push((r, fate));
            }
        }
        for (r, fate) in fates {
            match fate {
                Fate::OutOfScope(dest) => {
                    info!(
                        "{}: moved to {dest}, outside the synced folders; the remote copy is kept",
                        r.path
                    );
                    let mut r = r;
                    r.state = State::OutOfScope;
                    r.note = Some(format!("moved to {dest}; the remote document is left alone"));
                    self.with_index(|ix| ix.upsert(&r))?;
                    rep.out_of_scope += 1;
                }
                Fate::Delete => self.delete_remote(r, rep).await?,
            }
        }
        Ok(())
    }

    fn fate_of(&self, dest: &str) -> Fate {
        if in_trash(dest) || self.local_visible(dest) {
            // Obsidian's delete, or a rename inside the synced folders (the
            // new name is pushed as a new document).
            Fate::Delete
        } else {
            Fate::OutOfScope(dest.to_string())
        }
    }

    /// Look for vanished files outside the synced area (other dot-folders,
    /// excluded folders, ignored names, `.trash/`), matched by size, mtime
    /// and content hash. Returns doc ID -> where it is now.
    async fn find_moved_out(&self, rows: &[FileRow]) -> Result<HashMap<String, String>> {
        let mut want: HashMap<(u64, i64), Vec<FileRow>> = HashMap::new();
        for r in rows {
            if let (Some(s), Some(m), Some(_)) = (r.disk_size, r.disk_mtime, &r.disk_hash) {
                want.entry((s, m)).or_default().push(r.clone());
            }
        }
        if want.is_empty() {
            return Ok(HashMap::new());
        }
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            const MAX_ENTRIES: usize = 200_000;
            let root = this.inner.cfg.vault.clone();
            let mut out = HashMap::new();
            let mut stack = vec![root.clone()];
            let mut seen = 0usize;
            while let Some(dir) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                for e in rd.flatten() {
                    seen += 1;
                    if seen > MAX_ENTRIES {
                        return out;
                    }
                    let Ok(ft) = e.file_type() else { continue };
                    let Some(rel) = rel_of(&root, &e.path()) else { continue };
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    if ft.is_dir() {
                        if rel != ".obsidian" && rel != ".git" && !name.starts_with(vaultfs::TEMP_PREFIX) {
                            stack.push(e.path());
                        }
                        continue;
                    }
                    if !ft.is_file() || this.local_visible(&rel) {
                        continue;
                    }
                    let Ok(md) = e.metadata() else { continue };
                    let Some(cands) = want.get(&(md.len(), mtime_ms(&md))) else {
                        continue;
                    };
                    let Ok(Some(st)) = vaultfs::read_state(&e.path()) else {
                        continue;
                    };
                    for c in cands {
                        if c.disk_hash.as_deref() == Some(st.hash.as_str()) {
                            out.entry(c.doc_id.clone()).or_insert_with(|| rel.clone());
                        }
                    }
                }
            }
            out
        })
        .await
        .map_err(EngineError::other)
    }

    /// Soft-delete a document whose local file was deleted, against its base
    /// revision. Rows that are not clean, and documents that moved on, are
    /// fetched again instead (the remote holds an edit this device never saw).
    async fn delete_remote(&self, r: FileRow, rep: &mut PushReport) -> Result<()> {
        let keep = |this: &Engine, why: &str| -> Result<()> {
            info!(
                "{}: deleted locally, but {why}; fetching the remote version again",
                r.path
            );
            this.with_index(|ix| {
                ix.delete_doc(&r.doc_id)?;
                ix.add_pending_now(&r.doc_id, why, vaultfs::now_ms())
            })
        };
        let Some(base) = r.base_rev.clone().filter(|_| r.state == State::Clean) else {
            rep.kept_remote += 1;
            return keep(self, "it was not in sync (dirty or conflicted)");
        };
        let cur = match self.inner.couch.fetch_rev(&r.doc_id, &base).await? {
            livesync_remote::DocFetch::Found(d) => d,
            _ => {
                rep.kept_remote += 1;
                return keep(self, "its synced revision is gone from the remote");
            }
        };
        let mut d = cur;
        if let Some(o) = d.as_object_mut() {
            o.remove("_conflicts");
            o.remove("_revisions");
            o.remove("_deleted");
        }
        d["deleted"] = json!(true);
        d["mtime"] = json!(vaultfs::now_ms());
        d["_rev"] = json!(base);
        match self.inner.couch.put_doc(&d).await? {
            PutOutcome::Saved { rev } => {
                debug!("deleted {} remotely ({rev})", r.path);
                self.with_index(|ix| {
                    ix.delete_doc(&r.doc_id)?;
                    ix.remove_pending(&r.doc_id)
                })?;
                rep.deleted += 1;
                Ok(())
            }
            PutOutcome::Conflict => {
                rep.kept_remote += 1;
                keep(self, "the remote changed since it was synced")
            }
        }
    }

    // ------------------------------------------------------------------
    // Loop
    // ------------------------------------------------------------------

    /// Watch directories under `start` (vault-relative, "" for the root):
    /// every folder except dot-folders, plus the root `.trash/` (moves into
    /// it are Obsidian's deletes). Returns how many were added.
    fn watch_tree(&self, w: &mut dyn Watcher, start: &str, watched: &mut HashSet<PathBuf>) -> usize {
        let root = self.inner.cfg.vault.clone();
        let mut added = 0;
        let mut stack = vec![if start.is_empty() {
            root.clone()
        } else {
            root.join(start)
        }];
        if start.is_empty() {
            let trash = root.join(".trash");
            if trash.is_dir() && watched.insert(trash.clone()) && w.watch_dir(&trash).is_ok() {
                added += 1;
            }
        }
        while let Some(dir) = stack.pop() {
            if !watched.contains(&dir) {
                if let Err(e) = w.watch_dir(&dir) {
                    warn!("cannot watch {}: {e}; relying on periodic rescans there", dir.display());
                    continue;
                }
                watched.insert(dir.clone());
                added += 1;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) && !e.file_name().to_string_lossy().starts_with('.') {
                    stack.push(e.path());
                }
            }
        }
        added
    }

    /// The push side of the two-way daemon: watcher events and periodic
    /// rescans feed passes, once pushes are enabled.
    pub(crate) async fn push_loop(self, mut shutdown: watch::Receiver<bool>) -> Result<PushReport> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher: Box<dyn Watcher> = if self.inner.cfg.watch {
            match crate::platform::default_watcher(tx) {
                Ok(w) => w,
                Err(e) => {
                    warn!("file watcher unavailable ({e}); using periodic rescans only");
                    Box::new(NoWatcher)
                }
            }
        } else {
            Box::new(NoWatcher)
        };
        let root = self.inner.cfg.vault.clone();
        let settle = self.inner.cfg.settle;
        let mut watched: HashSet<PathBuf> = HashSet::new();
        let mut started = false;
        let mut hints = RenameHints::default();
        let mut queue: HashMap<String, Settling> = HashMap::new();
        let mut full_due = true;
        let mut next_rescan = Instant::now() + self.inner.cfg.rescan_interval;
        let mut retry_at: Option<Instant> = None;
        let mut backoff = Backoff::default();
        let mut total = PushReport::default();
        let tick_every = (settle / 4).clamp(Duration::from_millis(50), Duration::from_millis(500));
        let mut tick = tokio::time::interval(tick_every);
        loop {
            tokio::select! {
                _ = shutdown.changed() => return Ok(total),
                ev = rx.recv() => {
                    if let Some(ev) = ev {
                        self.on_watch_event(ev, &root, &mut *watcher, &mut watched, &mut hints, &mut queue, &mut full_due);
                    }
                }
                _ = tick.tick() => {}
                _ = self.inner.rescan.notified() => full_due = true,
            }
            while let Ok(ev) = rx.try_recv() {
                self.on_watch_event(
                    ev,
                    &root,
                    &mut *watcher,
                    &mut watched,
                    &mut hints,
                    &mut queue,
                    &mut full_due,
                );
            }
            if !self.inner.push_enabled.load(Ordering::SeqCst) {
                continue;
            }
            if !started {
                let n = self.watch_tree(&mut *watcher, "", &mut watched);
                debug!("watching {n} folders");
                started = true;
                full_due = true;
            }
            if retry_at.is_some_and(|t| Instant::now() < t) {
                continue;
            }
            if Instant::now() >= next_rescan {
                full_due = true;
            }
            let result = if full_due {
                next_rescan = Instant::now() + self.inner.cfg.rescan_interval;
                match self.full_pass(&hints, true).await {
                    Ok((rep, deferred)) => {
                        full_due = false;
                        for p in deferred {
                            queue.entry(p).or_insert_with(Settling::new);
                        }
                        Ok(rep)
                    }
                    Err(e) => Err(e),
                }
            } else {
                let ready = take_settled(&mut queue, &root, settle);
                if ready.is_empty() {
                    continue;
                }
                match self.partial_pass(ready.clone(), &hints).await {
                    Ok(rep) => {
                        // Files still being written come back later.
                        Ok(rep)
                    }
                    Err(e) => {
                        for p in ready {
                            queue.entry(p).or_insert_with(Settling::new);
                        }
                        Err(e)
                    }
                }
            };
            match result {
                Ok(rep) => {
                    backoff.reset();
                    retry_at = None;
                    total.add(&rep);
                }
                Err(e) => {
                    let wait = backoff.next_delay();
                    self.record_error(&e);
                    warn!("push failed: {e}; retrying in {wait:.0?}");
                    retry_at = Some(Instant::now() + wait);
                    if !matches!(e, EngineError::Remote(_) | EngineError::Other(_)) {
                        return Err(e);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_watch_event(
        &self,
        ev: WatchEvent,
        root: &Path,
        watcher: &mut dyn Watcher,
        watched: &mut HashSet<PathBuf>,
        hints: &mut RenameHints,
        queue: &mut HashMap<String, Settling>,
        full_due: &mut bool,
    ) {
        let mut note_path = |abs: &Path, watcher: &mut dyn Watcher, watched: &mut HashSet<PathBuf>| {
            let Some(rel) = rel_of(root, abs) else { return };
            if classify_path(&rel).is_some() {
                return; // dot-paths (including our temp files) and flag files
            }
            if abs.is_dir() && !watched.contains(abs) && !watched.is_empty() {
                self.watch_tree(watcher, &rel, watched);
            }
            queue.entry(rel).or_insert_with(Settling::new).touch();
        };
        match ev {
            WatchEvent::Overflow => *full_due = true,
            WatchEvent::Changed(p) => note_path(&p, watcher, watched),
            WatchEvent::Renamed { from, to } => {
                if let (Some(f), Some(t)) = (rel_of(root, &from), rel_of(root, &to)) {
                    hints.add(f, t);
                }
                note_path(&from, watcher, watched);
                note_path(&to, watcher, watched);
            }
        }
    }
}

/// What a path looked like when last checked, for settling.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Observed {
    Unknown,
    Missing,
    Dir,
    File(u64, i64),
}

#[derive(Debug)]
struct Settling {
    seen: Observed,
    since: Instant,
}

impl Settling {
    fn new() -> Self {
        Settling {
            seen: Observed::Unknown,
            since: Instant::now(),
        }
    }

    fn touch(&mut self) {
        self.seen = Observed::Unknown;
        self.since = Instant::now();
    }
}

fn observe(p: &Path) -> Observed {
    match std::fs::symlink_metadata(p) {
        Ok(md) if md.is_dir() => Observed::Dir,
        Ok(md) => Observed::File(md.len(), mtime_ms(&md)),
        Err(_) => Observed::Missing,
    }
}

/// Paths whose size and mtime stayed the same for `settle`.
fn take_settled(queue: &mut HashMap<String, Settling>, root: &Path, settle: Duration) -> Vec<String> {
    let now = Instant::now();
    let mut ready = Vec::new();
    for (rel, s) in queue.iter_mut() {
        let o = observe(&root.join(rel));
        if o != s.seen {
            s.seen = o;
            s.since = now;
        } else if now.duration_since(s.since) >= settle {
            ready.push(rel.clone());
        }
    }
    for r in &ready {
        queue.remove(r);
    }
    ready.sort();
    ready
}

/// The result of writing metadata.
struct Written {
    rev: String,
    /// Stored as a branch beside the remote's version.
    branched: bool,
    /// The remote already had exactly this content.
    adopted: bool,
}

impl Written {
    fn plain(rev: String) -> Self {
        Written {
            rev,
            branched: false,
            adopted: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_hints_follow_folders() {
        let mut h = RenameHints::default();
        h.add("a/b.md".into(), "c/b.md".into());
        h.add("dir".into(), ".trash/dir".into());
        assert_eq!(h.dest("a/b.md").as_deref(), Some("c/b.md"));
        assert_eq!(h.dest("dir/x/y.md").as_deref(), Some(".trash/dir/x/y.md"));
        assert_eq!(h.dest("dirx/y.md"), None);
        assert!(in_trash(".trash/x.md") && !in_trash("a/.trash/x.md"));
    }

    #[test]
    fn revs() {
        assert_eq!(split_rev("3-abc"), Some((3, "abc")));
        assert_eq!(split_rev("x"), None);
        assert_eq!(random_rev_id().len(), 32);
    }
}
