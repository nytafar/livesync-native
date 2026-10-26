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
//!   read first: identical content is adopted; a deletion is replaced only
//!   by a file changed after it (an older copy of the deleted content is
//!   removed locally, anything else becomes a conflict); anything else gets
//!   an independent root (a conflict, never an overwrite).
//! - **vanished file:** find out where it went. A move into `.trash/` or out
//!   of the vault is a delete; a move to an in-scope path is a rename
//!   (delete plus create); a move out of the folder scope (or into another
//!   dot-folder) is not a delete: the row becomes `out_of_scope`. Deletes are
//!   soft deletes written against `base_rev`, only for clean rows; anything
//!   else is fetched again instead. A file counts as vanished only when the
//!   filesystem says it is not there (`NotFound`); folders or entries that
//!   cannot be read protect everything under them.
//!
//! Passes refuse to run while a full fetch is owed or this node still has to
//! be marked resolved (`Engine::pushes_held`). One document's failure is
//! recorded as a local issue and the pass goes on; only a lost connection
//! (or repeated failures) ends a pass.
//!
//! Chunks always go before metadata.

use crate::engine::{meta_keys as meta, Engine, EngineError};
use crate::index::{FileRow, State};
use crate::vaultfs::{self, DiskState, RemoveOutcome};
use crate::watcher::{NoWatcher, WatchEvent, Watcher};
use livesync_model::path::{classify_path, path_to_id};
use livesync_model::splitter::chunk_file;
use livesync_model::FileKind;
use livesync_remote::{Backoff, DocFetch, PutOutcome, RemoteError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, UNIX_EPOCH};
use tokio::sync::watch;
use tracing::{debug, info, warn};

type Result<T> = std::result::Result<T, EngineError>;

/// The mass-delete stop: more than this many deletions without a known
/// destination, and more than half of the files tracked at the start of the
/// pass, need `allow_mass_delete`.
const MASS_DELETE_MIN: u64 = 20;
/// When the whole visible tree is empty, this many deletions already stop.
const EMPTIED_TREE_MIN: u64 = 3;
/// Consecutive per-document failures that end a pass (the server, not one
/// document, is the problem).
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// Paths kept in a report of files pushed back after a rebuild.
const MAX_ORPHAN_PATHS: usize = 100;

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
    /// Local copies removed because, with no known base (a fresh index),
    /// the remote had deleted exactly this content after it was last changed.
    pub deleted_locally: u64,
    /// Files the remote lacked after a rebuild or full fetch, pushed back
    /// from this device (also counted in `pushed`). See `status`.
    pub orphans_pushed: u64,
    /// Their paths (at most 100).
    pub orphan_paths: Vec<String>,
    /// Documents whose push or delete failed in this pass (recorded as local
    /// issues); the rest of the pass went on.
    pub failed: u64,
    /// Tracked files under folders or entries that could not be read, so not
    /// treated as deleted.
    pub unreadable: u64,
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
        self.deleted_locally += o.deleted_locally;
        self.orphans_pushed += o.orphans_pushed;
        for p in &o.orphan_paths {
            if self.orphan_paths.len() < MAX_ORPHAN_PATHS {
                self.orphan_paths.push(p.clone());
            }
        }
        self.failed += o.failed;
        self.unreadable += o.unreadable;
    }

    fn is_quiet(&self) -> bool {
        self.pushed + self.deleted + self.kept_remote + self.out_of_scope + self.adopted + self.deleted_locally == 0
    }
}

/// Files pushed back after a rebuild or full fetch, accumulated in
/// `meta.rebuild_orphans` until the next full fetch.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct OrphanRecord {
    pub(crate) last_ms: i64,
    pub(crate) count: u64,
    pub(crate) paths: Vec<String>,
}

impl OrphanRecord {
    pub(crate) fn describe(&self) -> String {
        let more = self.count.saturating_sub(self.paths.len() as u64);
        format!(
            "{} {} file(s) the remote lacked after a rebuild or full fetch were pushed back from this device (notes deleted elsewhere just before the rebuild come back this way; check them): {}{}",
            self.last_ms,
            self.count,
            self.paths.join(", "),
            if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            }
        )
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

/// The result of walking (part of) the vault.
#[derive(Debug, Default)]
struct Walk {
    found: Vec<Found>,
    /// Folders and entries that could not be read: vault-relative path ("" is
    /// the root) and why. Nothing under them counts as deleted.
    unreadable: Vec<(String, String)>,
}

/// Whether `path` is `prefix` or lies under it ("" covers everything).
fn under(path: &str, prefix: &str) -> bool {
    prefix.is_empty() || path == prefix || path.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))
}

/// Per-pass bookkeeping shared by the pushes and the vanished-file handling.
#[derive(Debug, Default)]
struct PassCtx {
    /// Content hash -> path of files new to the index in this pass, so a
    /// vanished file with the same content is known to have been renamed.
    new_files: HashMap<String, String>,
    /// Per-document failures in a row.
    consecutive_failures: u32,
    /// The first full pass after a full fetch (`meta.reconcile_pending`).
    reconcile: bool,
}

/// Whether an error ends the whole pass rather than just this document.
fn pass_fatal(e: &EngineError) -> bool {
    match e {
        EngineError::Remote(RemoteError::Transport { .. } | RemoteError::Url(_)) => true,
        EngineError::Remote(_) | EngineError::Other(_) => false,
        EngineError::Refused(_) | EngineError::Index(_) | EngineError::Busy(_) => true,
    }
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

fn is_tracked(s: State) -> bool {
    matches!(s, State::Clean | State::Dirty | State::Conflicted)
}

fn same_content(a: &Value, b: &Value) -> bool {
    a.get("children") == b.get("children") && a.get("type") == b.get("type")
}

/// What became of a vanished file.
enum Fate {
    /// A delete; `dest` is where it went when known (`.trash/`, or a rename
    /// inside the synced folders).
    Delete {
        dest: Option<String>,
    },
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

    /// All visible files under `start` (a vault-relative folder, "" for the
    /// root), and what could not be read on the way.
    fn walk_visible(&self, start: &str) -> Walk {
        use std::io::ErrorKind::NotFound;
        let root = self.inner.cfg.vault.clone();
        let mut w = Walk::default();
        let mut stack = vec![if start.is_empty() {
            root.clone()
        } else {
            root.join(start)
        }];
        while let Some(dir) = stack.pop() {
            let dir_rel = rel_of(&root, &dir).unwrap_or_default();
            let rd = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) if e.kind() == NotFound => continue, // removed meanwhile
                Err(e) => {
                    w.unreadable.push((dir_rel, e.to_string()));
                    continue;
                }
            };
            for e in rd {
                let e = match e {
                    Ok(e) => e,
                    Err(err) => {
                        // A partial listing: nothing in this folder is known
                        // to be gone.
                        w.unreadable.push((dir_rel.clone(), format!("listing failed: {err}")));
                        break;
                    }
                };
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') {
                    continue;
                }
                let Some(rel) = rel_of(&root, &e.path()) else { continue };
                let ft = match e.file_type() {
                    Ok(t) => t,
                    Err(err) if err.kind() == NotFound => continue,
                    Err(err) => {
                        w.unreadable.push((rel, err.to_string()));
                        continue;
                    }
                };
                if ft.is_dir() {
                    if self.inner.cfg.scope.may_contain_under(&rel) {
                        stack.push(e.path());
                    }
                } else if ft.is_file() && self.local_visible(&rel) {
                    match e.metadata() {
                        Ok(md) => w.found.push(Found {
                            rel,
                            size: md.len(),
                            mtime_ms: mtime_ms(&md),
                        }),
                        Err(err) if err.kind() == NotFound => {}
                        Err(err) => w.unreadable.push((rel, err.to_string())),
                    }
                }
            }
        }
        w
    }

    /// Report what could not be read (log and `status` local issues).
    fn note_unreadable(
        &self,
        unreadable: &[(String, String)],
        issues_seen: &mut HashSet<String>,
        rep: &mut PushReport,
    ) -> Result<()> {
        let now = vaultfs::now_ms();
        for (p, why) in unreadable {
            let shown = if p.is_empty() { "." } else { p.as_str() };
            let note = format!("cannot be read ({why}); nothing under it is treated as deleted");
            warn!("{shown}: {note}");
            self.with_index(|ix| ix.set_issue(shown, "unreadable", &note, now))?;
            if issues_seen.insert(shown.to_string()) {
                rep.issues += 1;
            }
        }
        Ok(())
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

    /// Whether a row's file is unchanged since it was synced (by stat), so
    /// the pass can skip it.
    fn unchanged(&self, r: &FileRow, f: &Found) -> bool {
        r.disk_size == Some(f.size)
            && r.disk_mtime == Some(f.mtime_ms)
            && r.base_rev.is_some()
            && r.state != State::OutOfScope
            // A case-only rename on a case-sensitive filesystem keeps the
            // document but changes its path.
            && (self.inner.fs.folds() || r.path == f.rel)
    }

    /// One document failed. Record it and go on, unless the error means the
    /// pass cannot go on (connection lost, local index trouble, or several
    /// documents in a row failing).
    fn doc_failed(
        &self,
        rel: &str,
        what: &str,
        e: EngineError,
        ctx: &mut PassCtx,
        rep: &mut PushReport,
        issues_seen: &mut HashSet<String>,
    ) -> Result<()> {
        ctx.consecutive_failures += 1;
        if pass_fatal(&e) || ctx.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            return Err(e);
        }
        let note = format!("{what} failed: {e}; retried at the next pass");
        warn!("{rel}: {note}");
        self.with_index(|ix| ix.set_issue(rel, "push-failed", &note, vaultfs::now_ms()))?;
        rep.failed += 1;
        if issues_seen.insert(rel.to_string()) {
            rep.issues += 1;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Passes
    // ------------------------------------------------------------------

    /// Compare the whole vault with the index and push what changed. With
    /// `defer_recent`, files modified within the settle time are returned
    /// for later instead of pushed now. Refuses (an error) while a full
    /// fetch is owed or this node still has to be marked resolved.
    pub async fn push_all(&self, defer_recent: bool) -> Result<PushReport> {
        match self.full_pass(&RenameHints::default(), defer_recent).await? {
            Some((rep, _)) => Ok(rep),
            None => Err(EngineError::Other(format!(
                "not pushing: {}; a two-way sync completes it first",
                self.pushes_held()?.unwrap_or("pushes are held")
            ))),
        }
    }

    /// `None`: pushes are held (see `Engine::pushes_held`); nothing was done.
    async fn full_pass(&self, hints: &RenameHints, defer_recent: bool) -> Result<Option<(PushReport, Vec<String>)>> {
        let _ops = self.inner.ops.lock().await;
        if let Some(why) = self.pushes_held()? {
            debug!("full pass held: {why}");
            return Ok(None);
        }
        let mut rep = PushReport {
            passes: 1,
            ..Default::default()
        };
        let this = self.clone();
        let walk = tokio::task::spawn_blocking(move || this.walk_visible(""))
            .await
            .map_err(EngineError::other)?;
        rep.scanned = walk.found.len() as u64;
        let rows = self.with_index(|ix| ix.all_rows())?;
        // The mass-delete stop compares with what was tracked before this
        // pass added anything.
        let tracked = rows.iter().filter(|r| is_tracked(r.state)).count() as u64;
        let visible_empty = walk.found.is_empty() && walk.unreadable.is_empty();
        let mut issues_seen: HashSet<String> = HashSet::new();
        self.note_unreadable(&walk.unreadable, &mut issues_seen, &mut rep)?;

        // Group local files by document ID: two names for one document is a
        // case collision on a case-sensitive filesystem.
        let mut by_id: BTreeMap<String, Vec<Found>> = BTreeMap::new();
        for f in walk.found {
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
                if r.state == State::Skipped || self.unchanged(r, &f) {
                    continue;
                }
            }
            if defer_recent && (now - f.mtime_ms).abs() < self.inner.cfg.settle.as_millis() as i64 {
                deferred.push(f.rel.clone());
                rep.deferred += 1;
                continue;
            }
            work.push((f, row));
        }
        let mut ctx = PassCtx {
            reconcile: self.with_index(|ix| ix.get_meta(meta::RECONCILE))?.is_some(),
            ..Default::default()
        };
        for (f, row) in work {
            match self.push_local(&f.rel, row, &mut rep, &mut ctx).await {
                Ok(issue) => {
                    ctx.consecutive_failures = 0;
                    if let Some(i) = issue {
                        issues_seen.insert(i);
                    }
                }
                Err(e) => self.doc_failed(&f.rel, "push", e, &mut ctx, &mut rep, &mut issues_seen)?,
            }
        }
        let vanished: Vec<FileRow> = rows
            .into_iter()
            .filter(|r| !present.contains(&r.doc_id))
            .filter(|r| is_tracked(r.state))
            .collect();
        self.handle_vanished(
            vanished,
            hints,
            &walk.unreadable,
            Tally { tracked, visible_empty },
            &mut ctx,
            &mut rep,
            &mut issues_seen,
        )
        .await?;
        if deferred.is_empty() {
            self.with_index(|ix| ix.retain_issues(&issues_seen))?;
        }
        if ctx.reconcile {
            self.with_index(|ix| ix.delete_meta(meta::RECONCILE))?;
        }
        self.finish_pass(&rep, true)?;
        Ok(Some((rep, deferred)))
    }

    /// Look at specific paths (files, folders, or paths that vanished).
    /// `None`: pushes are held; nothing was done.
    async fn partial_pass(&self, paths: Vec<String>, hints: &RenameHints) -> Result<Option<PushReport>> {
        use std::io::ErrorKind::NotFound;
        let _ops = self.inner.ops.lock().await;
        if let Some(why) = self.pushes_held()? {
            debug!("partial pass held: {why}");
            return Ok(None);
        }
        let mut rep = PushReport {
            passes: 1,
            ..Default::default()
        };
        let tracked: u64 = self
            .with_index(|ix| ix.counts_by_state())?
            .iter()
            .filter(|(s, _)| s == "clean" || s == "dirty" || s == "conflicted")
            .map(|(_, n)| *n)
            .sum();
        let mut files: BTreeMap<String, Found> = BTreeMap::new();
        let mut gone: BTreeMap<String, FileRow> = BTreeMap::new();
        let mut unreadable: Vec<(String, String)> = Vec::new();
        for p in paths {
            let abs = self.inner.cfg.vault.join(&p);
            match std::fs::symlink_metadata(&abs) {
                Ok(md) if md.is_dir() => {
                    let w = self.walk_visible(&p);
                    let here: HashSet<String> = w.found.iter().map(|f| f.rel.clone()).collect();
                    for f in w.found {
                        files.insert(f.rel.clone(), f);
                    }
                    unreadable.extend(w.unreadable);
                    // Rows under the folder whose files were not listed
                    // (`handle_vanished` checks that they are really gone).
                    for r in self.with_index(|ix| ix.rows_under(&p))? {
                        if !here.contains(&r.path) {
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
                Err(e) if e.kind() == NotFound => {
                    for r in self.with_index(|ix| ix.rows_under(&p))? {
                        gone.insert(r.doc_id.clone(), r);
                    }
                }
                Err(e) => unreadable.push((p.clone(), e.to_string())),
            }
        }
        let mut issues_seen: HashSet<String> = HashSet::new();
        self.note_unreadable(&unreadable, &mut issues_seen, &mut rep)?;
        rep.scanned = files.len() as u64;
        let mut ctx = PassCtx::default();
        for (_, f) in files {
            let row = self.row_for_local(&f.rel)?;
            if let Some(r) = &row {
                if r.state == State::Skipped || self.unchanged(r, &f) {
                    continue;
                }
            }
            match self.push_local(&f.rel, row, &mut rep, &mut ctx).await {
                Ok(_) => ctx.consecutive_failures = 0,
                Err(e) => self.doc_failed(&f.rel, "push", e, &mut ctx, &mut rep, &mut issues_seen)?,
            }
        }
        let vanished: Vec<FileRow> = gone.into_values().filter(|r| is_tracked(r.state)).collect();
        self.handle_vanished(
            vanished,
            hints,
            &unreadable,
            Tally {
                tracked,
                visible_empty: false,
            },
            &mut ctx,
            &mut rep,
            &mut issues_seen,
        )
        .await?;
        self.finish_pass(&rep, false)?;
        Ok(Some(rep))
    }

    fn finish_pass(&self, rep: &PushReport, full: bool) -> Result<()> {
        if !rep.is_quiet() {
            info!(
                "push: {} written ({} new, {} as conflict branches), {} adopted, {} chunks ({:.1} MB) uploaded, {} deleted, {} kept from the remote, {} moved out of scope, {} stale local copies removed, {} failed",
                rep.pushed,
                rep.created,
                rep.conflicts,
                rep.adopted,
                rep.chunks_uploaded,
                rep.chunk_bytes as f64 / 1e6,
                rep.deleted,
                rep.kept_remote,
                rep.out_of_scope,
                rep.deleted_locally,
                rep.failed
            );
            self.with_index(|ix| ix.set_meta(meta::LAST_PUSH, &vaultfs::now_ms().to_string()))?;
        }
        if rep.orphans_pushed > 0 {
            let mut rec: OrphanRecord = self
                .with_index(|ix| ix.get_meta(meta::ORPHANS))?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            rec.last_ms = vaultfs::now_ms();
            rec.count += rep.orphans_pushed;
            for p in &rep.orphan_paths {
                if rec.paths.len() < MAX_ORPHAN_PATHS {
                    rec.paths.push(p.clone());
                }
            }
            warn!(
                "{} file(s) the remote lacked after a rebuild or full fetch were pushed back from this device: {}",
                rep.orphans_pushed,
                rep.orphan_paths.join(", ")
            );
            self.with_index(|ix| ix.set_meta(meta::ORPHANS, &serde_json::to_string(&rec).unwrap_or_default()))?;
        }
        // A stop stays visible until a full pass completes without one; a
        // partial pass sees too little to say it is over.
        if full && rep.safety_stops == 0 {
            self.with_index(|ix| ix.delete_meta(meta::SAFETY_STOP))?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // One file
    // ------------------------------------------------------------------

    /// Push one local file if its content differs from its row. Returns the
    /// path when it was recorded as a local issue.
    async fn push_local(
        &self,
        rel: &str,
        row: Option<FileRow>,
        rep: &mut PushReport,
        ctx: &mut PassCtx,
    ) -> Result<Option<String>> {
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
            file_id: None,
        };
        // Keep the remote spelling when the filesystem says it is the same file.
        let doc_path = match &row {
            Some(r) if self.inner.fs.folds() && crate::index::fold_key(&r.path) == crate::index::fold_key(rel) => {
                r.path.clone()
            }
            _ => rel.to_string(),
        };
        if let Some(r) = &row {
            if r.disk_hash.as_deref() == Some(hash.as_str()) && r.base_rev.is_some() && r.path == doc_path {
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
                    match was {
                        // A remote change was held back for this file: apply it now.
                        State::Dirty => {
                            ix.add_pending_now(&r.doc_id, "local file back to its synced version", vaultfs::now_ms())
                        }
                        // Remote changes made while it was out of scope were
                        // passed over: fetch the current version.
                        State::OutOfScope => {
                            ix.add_pending_now(&r.doc_id, "moved back into the synced folders", vaultfs::now_ms())
                        }
                        _ => Ok(()),
                    }
                })?;
                if was == State::OutOfScope {
                    info!("{rel}: moved back into the synced folders; fetching the current remote version");
                }
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
        let written = match self.write_meta(doc, base.as_deref(), ctx.reconcile).await? {
            MetaWrite::Written(w) => w,
            MetaWrite::DeletedElsewhere => {
                self.remove_deleted_elsewhere(rel, &doc_id, row.as_ref(), &hash, rep)?;
                return Ok(issue);
            }
        };
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
        // A row that lost its revision to a rebuild or full fetch, whose
        // document the remote no longer has: pushed back from this device.
        let orphan = row
            .as_ref()
            .is_some_and(|r| r.base_rev.is_none() && r.disk_hash.is_some());
        if orphan && written.created {
            rep.orphans_pushed += 1;
            if rep.orphan_paths.len() < MAX_ORPHAN_PATHS {
                rep.orphan_paths.push(doc_path.clone());
            }
        }
        if row.is_none() {
            ctx.new_files.insert(hash.clone(), doc_path.clone());
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

    /// With no known base, the remote deleted exactly this content after
    /// the local copy last changed: the copy is stale (for example a vault
    /// folder kept across an app reinstall). Remove it, as a pull would have.
    fn remove_deleted_elsewhere(
        &self,
        rel: &str,
        doc_id: &str,
        row: Option<&FileRow>,
        hash: &str,
        rep: &mut PushReport,
    ) -> Result<()> {
        let abs = self.inner.cfg.vault.join(rel);
        // Keep a copy in the state directory first: a removal decided on
        // mtimes must never be the end of the only copy.
        match std::fs::read(&abs) {
            Ok(bytes) if vaultfs::hash_bytes(&bytes) == hash => self.save_recovered(rel, &bytes),
            Ok(_) => return Ok(()), // changed since it was read: next pass
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(EngineError::Other(format!("reading {rel}: {e}"))),
        }
        match vaultfs::remove_if_matches(&self.inner.cfg.vault, &abs, hash) {
            Ok(RemoveOutcome::Removed | RemoveOutcome::AlreadyGone) => {
                warn!("{rel}: deleted on another device after this copy last changed (same content); the stale local copy was removed (a copy is in the state directory's recovered/ folder)");
                rep.deleted_locally += 1;
                self.with_index(|ix| {
                    if let Some(r) = row {
                        ix.delete_doc(&r.doc_id)?;
                    }
                    ix.remove_pending(doc_id)
                })
            }
            Ok(RemoveOutcome::Raced { displaced }) => {
                warn!("{rel}: edited while its stale copy was being removed; the edit is kept");
                self.put_back(rel, displaced, None);
                Ok(())
            }
            // Changed since it was read: the next pass looks again.
            Ok(RemoveOutcome::Diverged { .. }) => Ok(()),
            Err(e) => Err(EngineError::Other(format!("removing the stale copy of {rel}: {e}"))),
        }
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
        const MAX_BATCH_DOCS: usize = 100;
        let (mut count, mut bytes) = (0u64, 0u64);
        let mut batch: Vec<Value> = Vec::new();
        let mut batch_bytes = 0usize;
        for (id, data) in &missing {
            // Flush before a chunk would take the batch over the cap (a
            // single chunk larger than the cap goes alone).
            if !batch.is_empty() && (batch_bytes + data.len() > MAX_BATCH_BYTES || batch.len() >= MAX_BATCH_DOCS) {
                self.upload_batch(&batch).await?;
                batch.clear();
                batch_bytes = 0;
            }
            batch.push(json!({"_id": id, "type": "leaf", "data": data}));
            batch_bytes += data.len();
            count += 1;
            bytes += data.len() as u64;
        }
        if !batch.is_empty() {
            self.upload_batch(&batch).await?;
        }
        Ok((count, bytes))
    }

    async fn upload_batch(&self, batch: &[Value]) -> Result<()> {
        let results = self.inner.couch.bulk_docs(batch, true).await?;
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
        Ok(())
    }

    /// Write file metadata against `base` (D4), or with unknown ancestry.
    async fn write_meta(&self, doc: Value, base: Option<&str>, reconcile: bool) -> Result<MetaWrite> {
        match base {
            Some(b) => {
                let mut d = doc.clone();
                d["_rev"] = json!(b);
                match self.inner.couch.put_doc(&d).await? {
                    PutOutcome::Saved { rev } => Ok(MetaWrite::Written(Written::plain(rev))),
                    PutOutcome::Conflict => {
                        // The remote moved on, or our own earlier write
                        // landed but its response was lost: when the
                        // current version is exactly ours, adopt it rather
                        // than branch off against ourselves.
                        let id = doc["_id"].as_str().unwrap_or_default().to_string();
                        if let DocFetch::Found(cur) = self.inner.couch.fetch_doc(&id, true).await? {
                            let deleted = cur.get("deleted").and_then(Value::as_bool).unwrap_or(false);
                            if !deleted && same_content(&cur, &doc) {
                                debug!("{id}: the remote already has this version; adopted");
                                return Ok(MetaWrite::Written(Written {
                                    rev: cur["_rev"].as_str().unwrap_or_default().to_string(),
                                    branched: has_conflicts(&cur),
                                    adopted: true,
                                    created: false,
                                }));
                            }
                        }
                        Ok(MetaWrite::Written(self.force_base(doc, b).await?))
                    }
                }
            }
            None => self.write_unknown_ancestry(doc, reconcile).await,
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
            created: false,
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
            DocFetch::Found(_) => Ok(()),
            other => Err(EngineError::Other(format!(
                "{id}: revision {rev} not stored ({other:?})"
            ))),
        }
    }

    /// An independent root `1-<random>`: our content beside whatever the
    /// remote has, so neither version is lost.
    async fn write_root(&self, doc: &Value) -> Result<Written> {
        let r = random_rev_id();
        let rev = format!("1-{r}");
        let mut d = doc.clone();
        d["_rev"] = json!(rev);
        d["_revisions"] = json!({"start": 1, "ids": [r]});
        self.import_rev(d, &rev).await?;
        Ok(Written {
            rev,
            branched: true,
            adopted: false,
            created: false,
        })
    }

    /// No known base: read the current document first.
    ///
    /// A soft-deleted document is replaced by the local file (upstream's
    /// "latest" mode), except in the reconcile pass after a full fetch (a
    /// fresh index next to an existing vault), where the local copy may
    /// predate the deletion: there a file older than the deletion with the
    /// deleted content is stale, and one with other content becomes a
    /// conflict beside the deletion.
    async fn write_unknown_ancestry(&self, doc: Value, reconcile: bool) -> Result<MetaWrite> {
        let id = doc["_id"].as_str().unwrap_or_default().to_string();
        for _ in 0..3 {
            match self.inner.couch.fetch_doc(&id, true).await? {
                DocFetch::Found(cur) => {
                    let rev = cur["_rev"].as_str().unwrap_or_default().to_string();
                    let soft_deleted = cur.get("deleted").and_then(Value::as_bool).unwrap_or(false);
                    if soft_deleted {
                        let deleted_at = cur.get("mtime").and_then(Value::as_i64).unwrap_or(0);
                        let ours = doc.get("mtime").and_then(Value::as_i64).unwrap_or(0);
                        if !reconcile || ours > deleted_at {
                            // A new file at a deleted path (or one restored
                            // from the trash), or one changed after the
                            // deletion: a new life, as upstream's "latest".
                            let mut d = doc.clone();
                            d["_rev"] = json!(rev);
                            match self.inner.couch.put_doc(&d).await? {
                                PutOutcome::Saved { rev } => {
                                    return Ok(MetaWrite::Written(Written {
                                        created: true,
                                        ..Written::plain(rev)
                                    }))
                                }
                                PutOutcome::Conflict => continue,
                            }
                        }
                        if same_content(&cur, &doc) {
                            // The very content that was deleted, and not
                            // changed since: it was deleted elsewhere.
                            return Ok(MetaWrite::DeletedElsewhere);
                        }
                        // Older than the deletion but different from what
                        // was deleted: keep both, as a conflict.
                        warn!(
                            "{id}: deleted on another device, but this copy differs from what was deleted; kept as a conflicting revision"
                        );
                        return Ok(MetaWrite::Written(self.write_root(&doc).await?));
                    }
                    if same_content(&cur, &doc) {
                        return Ok(MetaWrite::Written(Written {
                            rev,
                            branched: has_conflicts(&cur),
                            adopted: true,
                            created: false,
                        }));
                    }
                    // Different content of unknown origin: an independent root,
                    // so neither version is lost.
                    return Ok(MetaWrite::Written(self.write_root(&doc).await?));
                }
                DocFetch::Deleted | DocFetch::Missing(_) => match self.inner.couch.put_doc(&doc).await? {
                    PutOutcome::Saved { rev } => {
                        return Ok(MetaWrite::Written(Written {
                            created: true,
                            ..Written::plain(rev)
                        }))
                    }
                    PutOutcome::Conflict => continue, // created meanwhile; look again
                },
            }
        }
        Err(EngineError::Other(format!(
            "{id}: the document kept changing while writing it"
        )))
    }

    // ------------------------------------------------------------------
    // Vanished files
    // ------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn handle_vanished(
        &self,
        rows: Vec<FileRow>,
        hints: &RenameHints,
        unreadable: &[(String, String)],
        tally: Tally,
        ctx: &mut PassCtx,
        rep: &mut PushReport,
        issues_seen: &mut HashSet<String>,
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let n = rows.len();
        // The vault folder itself must be there; an unmounted card or a
        // revoked permission must not look like "everything was deleted".
        if std::fs::read_dir(&self.inner.cfg.vault).is_err() {
            return Err(EngineError::Other(format!(
                "the vault folder {} cannot be read; not treating missing files as deleted",
                self.inner.cfg.vault.display()
            )));
        }
        if let Some(check) = &self.inner.cfg.access_check {
            if let Err(why) = (check.0)() {
                return Err(EngineError::Other(format!(
                    "vault access check failed ({why}); not treating {n} missing file(s) as deleted"
                )));
            }
        }
        // Only files the filesystem says are not there count as vanished.
        let mut gone: Vec<FileRow> = Vec::new();
        for r in rows {
            if unreadable.iter().any(|(p, _)| under(&r.path, p)) {
                rep.unreadable += 1;
                continue;
            }
            match std::fs::symlink_metadata(self.inner.cfg.vault.join(&r.path)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => gone.push(r),
                Ok(_) => debug!("{}: still there; not treated as deleted", r.path),
                Err(e) => {
                    let note = format!("cannot be checked ({e}); not treated as deleted");
                    warn!("{}: {note}", r.path);
                    self.with_index(|ix| ix.set_issue(&r.path, "unreadable", &note, vaultfs::now_ms()))?;
                    if issues_seen.insert(r.path.clone()) {
                        rep.issues += 1;
                    }
                    rep.unreadable += 1;
                }
            }
        }
        if gone.is_empty() {
            return Ok(());
        }
        // Decide every fate first.
        let mut fates: Vec<(FileRow, Fate)> = Vec::new();
        let mut unknown: Vec<FileRow> = Vec::new();
        for r in gone {
            if let Some(d) = hints.dest(&r.path) {
                let fate = self.fate_of(&d);
                fates.push((r, fate));
            } else if let Some(d) = r.disk_hash.as_ref().and_then(|h| ctx.new_files.get(h)) {
                // The same content appeared under a new name in this pass.
                let dest = Some(d.clone());
                fates.push((r, Fate::Delete { dest }));
            } else {
                unknown.push(r);
            }
        }
        if !unknown.is_empty() {
            let found = self.find_moved_out(&unknown).await?;
            for r in unknown {
                let fate = match found.get(&r.doc_id) {
                    Some(d) => self.fate_of(d),
                    None => Fate::Delete { dest: None },
                };
                fates.push((r, fate));
            }
        }
        // The mass-delete stop: deletions nobody can account for (no rename,
        // no move to `.trash/` or out of scope), against what was tracked at
        // the start of the pass.
        let undestined = fates
            .iter()
            .filter(|(_, f)| matches!(f, Fate::Delete { dest: None }))
            .count() as u64;
        let tracked = tally.tracked;
        let mass = undestined > MASS_DELETE_MIN && undestined * 2 > tracked;
        let emptied = tally.visible_empty && undestined >= EMPTIED_TREE_MIN;
        let mut withhold = false;
        if mass || emptied {
            let msg = if emptied {
                format!("the synced folders look empty: {undestined} of {tracked} tracked files are missing")
            } else {
                format!("{undestined} of {tracked} tracked files are missing from the vault")
            };
            if self.inner.cfg.allow_mass_delete {
                warn!("{msg}; deleting them remotely (mass deletes allowed)");
            } else {
                let msg = format!("{msg}; not deleting them remotely (allow mass deletes if this is intended)");
                warn!("{msg}");
                self.with_index(|ix| ix.set_meta(meta::SAFETY_STOP, &format!("{} {msg}", vaultfs::now_ms())))?;
                rep.safety_stops += 1;
                withhold = true;
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
                Fate::Delete { dest: None } if withhold => {}
                Fate::Delete { dest } => {
                    if let Some(d) = &dest {
                        debug!("{}: moved to {d}", r.path);
                    }
                    let path = r.path.clone();
                    match self.delete_remote(r, rep).await {
                        Ok(()) => {
                            ctx.consecutive_failures = 0;
                            self.with_index(|ix| ix.clear_issue(&path))?;
                        }
                        Err(e) => self.doc_failed(&path, "remote delete", e, ctx, rep, issues_seen)?,
                    }
                }
            }
        }
        Ok(())
    }

    fn fate_of(&self, dest: &str) -> Fate {
        if in_trash(dest) || self.local_visible(dest) {
            // Obsidian's delete, or a rename inside the synced folders (the
            // new name is pushed as a new document).
            Fate::Delete {
                dest: Some(dest.to_string()),
            }
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
            DocFetch::Found(d) => d,
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
            // Pushes wait for the gate, the owed full fetch (with its quiet
            // wait after a rebuild) and the registration. Queued paths and
            // a due full pass stay queued meanwhile.
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
                    Ok(Some((rep, deferred))) => {
                        full_due = false;
                        for p in deferred {
                            queue.entry(p).or_insert_with(Settling::new);
                        }
                        Ok(rep)
                    }
                    // Held: stays due.
                    Ok(None) => continue,
                    Err(e) => Err(e),
                }
            } else {
                let ready = take_settled(&mut queue, &root, settle);
                if ready.is_empty() {
                    continue;
                }
                match self.partial_pass(ready.clone(), &hints).await {
                    Ok(Some(rep)) => Ok(rep),
                    Ok(None) => {
                        // Held: look at them again once pushes resume.
                        for p in ready {
                            queue.entry(p).or_insert_with(Settling::new);
                        }
                        continue;
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
            // Obsidian creates `.trash/` on its first delete; watch it then,
            // so later deletes are matched by their rename cookie.
            if rel == ".trash" && abs.is_dir() && !watched.contains(abs) && !watched.is_empty() {
                if watcher.watch_dir(abs).is_ok() {
                    watched.insert(abs.to_path_buf());
                }
                return;
            }
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
    /// No live document existed: created (or a deletion replaced).
    created: bool,
}

impl Written {
    fn plain(rev: String) -> Self {
        Written {
            rev,
            branched: false,
            adopted: false,
            created: false,
        }
    }
}

/// What writing a document's metadata came to.
enum MetaWrite {
    Written(Written),
    /// No known base, and the remote deleted exactly this content after the
    /// local copy last changed: the local copy is stale.
    DeletedElsewhere,
}

fn has_conflicts(doc: &Value) -> bool {
    doc.get("_conflicts")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
}

/// What the mass-delete stop compares against.
#[derive(Debug, Clone, Copy)]
struct Tally {
    /// Tracked files at the start of the pass.
    tracked: u64,
    /// A full pass found no visible file at all (and nothing unreadable).
    visible_empty: bool,
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
    fn under_prefixes() {
        assert!(under("a/b.md", ""));
        assert!(under("a/b.md", "a"));
        assert!(under("a", "a"));
        assert!(!under("ab/c.md", "a"));
        assert!(!under("a", "a/b"));
    }

    #[test]
    fn revs() {
        assert_eq!(split_rev("3-abc"), Some((3, "abc")));
        assert_eq!(split_rev("x"), None);
        assert_eq!(random_rev_id().len(), 32);
    }
}
