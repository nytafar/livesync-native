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
/// pass, need `allow_mass_delete`. In the daemon the deletions of one burst
/// are judged together (see `DeleteHold`).
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

/// Renames reported by the watcher, kept for a minute (plus however long
/// a rename may wait for its new name to be pushed) so a vanished file can
/// be matched with its destination.
#[derive(Debug, Default)]
pub(crate) struct RenameHints {
    pairs: Vec<(String, String, Instant)>,
    /// Kept beyond the minute (the daemon's `delete_hold_max`).
    extra: Duration,
}

impl RenameHints {
    pub(crate) fn keeping_extra(extra: Duration) -> Self {
        RenameHints {
            pairs: Vec::new(),
            extra,
        }
    }

    pub(crate) fn add(&mut self, from: String, to: String) {
        let now = Instant::now();
        self.pairs.push((from, to, now));
        if let Some(cutoff) = now.checked_sub(Duration::from_secs(60) + self.extra) {
            self.pairs.retain(|p| p.2 >= cutoff);
        }
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

/// The daemon's hold on local deletes (#10). A deletion with no known
/// destination is not sent when a pass first sees it: the watcher can spread
/// one burst over several settle windows, and a full pass can run in the
/// middle of it, and each part alone may stay under the mass-delete stop.
/// So such deletions wait until no new one has turned up for `period`; then
/// a full pass whose walk began after that judges everything missing at
/// once, as a single pass over the finished burst would. Partial passes
/// never decide them.
///
/// A trickle that keeps the hold open is judged anyway once the hold has
/// lasted `max`, so nothing waits longer than that. Such a cut does not end
/// the burst: what it sent is counted (`burst`), and each later part is
/// judged together with it, against what was tracked before it was sent,
/// until a real quiet period ends the burst (a full pass, walked `period`
/// after the last new deletion, finds none). So a wipe slower than `max`
/// still stops before more than half of the vault is gone, while deletions
/// with quiet periods between them are separate bursts. A stop keeps the
/// burst's count until it is confirmed or the files come back.
///
/// A full pass that cannot check some of the files the burst or its stop
/// depends on (their folder became unreadable, say) neither ends the burst
/// nor clears the stop. Files it cannot check that belong to neither do not
/// matter to them.
///
/// Holding a deletion only keeps the remote copy longer. The hold lives in
/// memory: if the process is killed or restarted, the rows are still
/// tracked and the files still missing, so the next full pass finds them
/// again and holds them anew; the burst's count is kept in the index
/// (`meta::DELETE_BURST`). A sleeping device stops the (monotonic) clock,
/// which only lengthens the hold.
#[derive(Debug)]
pub(crate) struct DeleteHold {
    period: Duration,
    max: Duration,
    /// Documents seen missing with no known destination, not decided on yet
    /// or refused by the stop: seeing them again is not a new deletion.
    seen: HashSet<String>,
    /// When the newest of them was first seen, while undecided.
    newest: Option<Instant>,
    /// When the undecided hold began.
    first: Option<Instant>,
    /// Renamed documents whose new name is not pushed yet, and since when
    /// they wait for it.
    pending: HashMap<String, Instant>,
    /// The documents the current burst already deleted remotely, in parts
    /// the maximum cut. One that came back (a row again) no longer counts;
    /// `Engine::prune_burst` drops it.
    burst: HashSet<String>,
    /// When a deletion last newly went missing.
    last_new: Option<Instant>,
    /// The last decision was a stop, not answered yet: the burst stays.
    refused: bool,
    /// The decision being made was forced by the maximum, or its pass could
    /// not check some of the burst's files: the burst goes on.
    cut: bool,
    /// The burst's or its stop's documents the last full pass could not
    /// check (`Some(empty)`: a stop saved without its list, so which ones is
    /// not known). They stay seen and the burst cannot end, but no look of
    /// its own is scheduled for that: the next full pass that comes anyway
    /// (the periodic rescan, Sync now, a confirmation) looks again.
    unchecked: Option<HashSet<String>>,
}

impl DeleteHold {
    pub(crate) fn new(period: Duration, max: Duration) -> Self {
        DeleteHold {
            period,
            max,
            seen: HashSet::new(),
            newest: None,
            first: None,
            pending: HashMap::new(),
            burst: HashSet::new(),
            last_new: None,
            refused: false,
            cut: false,
            unchecked: None,
        }
    }

    /// Take up a burst a previous run left (`meta::DELETE_BURST`), and
    /// whether a stop stands. The burst goes on until a quiet period from
    /// now.
    pub(crate) fn resume(&mut self, sent: HashSet<String>, stopped: bool) {
        if !sent.is_empty() {
            self.last_new = Some(Instant::now());
        }
        self.burst = sent;
        self.refused = stopped;
    }

    /// When the undecided hold is over (quiet, or at its maximum).
    fn hold_due(&self) -> Option<Instant> {
        let quiet = self.newest? + self.period;
        Some(match self.first {
            Some(f) => quiet.min(f + self.max),
            None => quiet,
        })
    }

    /// When a full pass must decide on the held deletions, or look whether
    /// a burst the maximum cut is over.
    pub(crate) fn due(&self) -> Option<Instant> {
        self.hold_due().or_else(|| {
            let open = !self.burst.is_empty() && !self.refused && self.unchecked.is_none();
            self.last_new.filter(|_| open).map(|t| t + self.period)
        })
    }

    /// Deletions waiting for a decision (for status).
    pub(crate) fn waiting(&self) -> u64 {
        if self.newest.is_some() {
            self.seen.len() as u64
        } else {
            0
        }
    }

    /// Whether no deletion went missing for `period` before `walked`.
    fn quiet(&self, walked: Instant) -> bool {
        self.last_new.is_none_or(|t| walked >= t + self.period)
    }

    /// Note a pass's deletions with no known destination. `scan` is when a
    /// full pass's walk began (`None` for a partial pass). `None`: they
    /// must be held. `Some(earlier)`: this full pass decides on them now,
    /// together with the `earlier` deletions of the same burst already
    /// sent. It decides when its walk began once the burst was over, so it
    /// saw all of it (files that went missing after the walk are not in
    /// it), or when the hold has lasted its maximum (the burst goes on).
    fn observe(&mut self, ids: &[&str], scan: Option<Instant>) -> Option<u64> {
        let at = Instant::now();
        let mut fresh = false;
        for id in ids {
            fresh |= self.seen.insert((*id).to_string());
        }
        if scan.is_some() {
            // A full pass sees every missing file: forget the ones that
            // came back or were deleted since. Those it could not check are
            // not known to be back.
            let current: HashSet<&str> = ids.iter().copied().collect();
            let unchecked = self.unchecked.as_ref();
            self.seen
                .retain(|id| current.contains(id.as_str()) || unchecked.is_some_and(|u| u.contains(id)));
        }
        if fresh {
            self.last_new = Some(at);
            if self.newest.is_none() {
                self.first = Some(at);
            }
            self.newest = Some(at);
        }
        let walked = scan?;
        let over = ids.is_empty() || self.hold_due().is_none_or(|due| walked >= due);
        if !over {
            return None;
        }
        self.cut = !self.quiet(walked) || self.unchecked.is_some();
        self.newest = None;
        self.first = None;
        Some(self.burst.len() as u64)
    }

    /// A full pass decided on the deletions it saw. Unless the stop refused
    /// them, `sent` were sent (or failed, and count as new when seen
    /// again), so the next deletion of the same document starts a new hold.
    /// They join the burst if the maximum cut it; after a quiet period the
    /// burst is over. A stop keeps the burst as it is.
    fn decided(&mut self, refused: bool, sent: &[&str]) {
        self.refused = refused;
        if refused {
            return;
        }
        match &self.unchecked {
            Some(u) => self.seen.retain(|id| u.contains(id)),
            None => self.seen.clear(),
        }
        if self.cut {
            self.burst.extend(sent.iter().map(|id| id.to_string()));
        } else {
            self.burst.clear();
        }
    }

    /// A full pass is about to judge what is missing (call before
    /// `observe`): the burst's or its stop's documents it could not check,
    /// `None` if it could check all of them. Such a pass cannot tell that
    /// the burst is over, so the burst goes on (as after a cut). It does
    /// not schedule another look of its own, so an unreadable folder does
    /// not make the daemon walk the vault every `period`.
    fn checked(&mut self, unchecked: Option<HashSet<String>>) {
        if let Some(u) = &unchecked {
            self.seen.extend(u.iter().cloned());
        }
        self.unchecked = unchecked;
    }

    /// The user confirmed the stop: the burst is answered.
    fn answered(&mut self) {
        self.burst.clear();
        self.refused = false;
    }

    /// Whether a renamed document may keep waiting for its new name to be
    /// pushed: for at most `max`, then it is judged as a deletion.
    fn may_wait(&mut self, doc_id: &str) -> bool {
        let since = *self.pending.entry(doc_id.to_string()).or_insert_with(Instant::now);
        since.elapsed() < self.max
    }

    /// After a full pass: only the documents still waiting stay pending.
    fn keep_pending(&mut self, ids: &HashSet<String>) {
        self.pending.retain(|id, _| ids.contains(id));
    }

    /// Something is pending (a rename whose new name is not pushed yet):
    /// make sure a full pass comes after the hold period.
    fn recheck(&mut self) {
        if self.newest.is_none() {
            let at = Instant::now();
            self.newest = Some(at);
            self.first = Some(at);
        }
    }

    /// The document's file is there: a later deletion of it is new.
    fn forget(&mut self, doc_id: &str) {
        self.seen.remove(doc_id);
    }
}

/// What a stop's confirmation lets through (`Engine::allow_mass_delete_once`).
#[derive(Debug, Clone)]
pub(crate) struct Confirmed {
    /// The stop it answers, as `meta::SAFETY_STOP` showed it.
    stop: String,
    docs: ConfirmedDocs,
}

#[derive(Debug, Clone)]
enum ConfirmedDocs {
    /// The documents the stop refused.
    Exactly(HashSet<String>),
    /// A stop saved by an older version, without its list: whatever is
    /// missing with no known destination when the confirmation is applied,
    /// if that is no more than the stop showed (else nothing).
    UpTo(u64),
}

/// The answer to `Engine::allow_mass_delete_once`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmation {
    /// The next full pass deletes what the stop refused.
    Allowed,
    /// No mass-delete stop is shown; nothing is allowed.
    NoStop,
    /// The stop shown now is not the one confirmed (it changed since); it
    /// is this one. Nothing is allowed.
    Changed(String),
}

/// How many files a mass-delete stop said were missing (`<ms> <n> of <m>
/// tracked files ...`, the form every version wrote), for a stop saved
/// without its list of documents.
fn stop_count(stop: &str) -> Option<u64> {
    let before = &stop[..stop.find(" tracked files")?];
    let mut words = before.rsplit(' ');
    words.next()?.parse::<u64>().ok()?;
    if words.next()? != "of" {
        return None;
    }
    words.next()?.parse().ok()
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
    /// Content hash -> paths of files new to the index in this pass, so a
    /// vanished file with the same content is known to have been renamed.
    /// Each new file accounts for one vanished file at most.
    new_files: HashMap<String, Vec<String>>,
    /// Per-document failures in a row.
    consecutive_failures: u32,
    /// The reconcile pass after a full fetch is pending (`meta.reconcile_pending`).
    reconcile: bool,
    /// Some file could not be read or pushed in this pass: the reconcile
    /// pass is not complete.
    incomplete: bool,
    /// The file `push_local` is working on: (document path, content hash).
    attempt: Option<(String, String)>,
    /// What the user confirmed deleting after a stop
    /// (`allow_mass_delete_once`): this pass deletes it, whatever the stop
    /// would say; any other deletion is judged as usual.
    confirmed: Option<Confirmed>,
    /// Paths and content hashes whose push failed in this pass: a vanished
    /// file renamed to one of them is not deleted yet.
    failed_paths: HashSet<String>,
    failed_hashes: HashSet<String>,
    /// Deletions were held for later (`DeleteHold`): no decision yet.
    deletes_held: bool,
    /// Some of the files `confirmed` covers could not be checked: it is not
    /// applied in this pass, and not used up, but waits for a pass that can
    /// check them all.
    confirmation_waits: bool,
    /// A renamed file's new name is not pushed yet; the old document waits.
    dests_pending: bool,
}

/// How to treat a file with no known revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoBase {
    /// A new file in normal operation.
    New,
    /// A file with no row, in the reconcile pass after a full fetch (a
    /// fresh index next to an existing vault).
    Reconcile,
    /// A tracked file whose revision a rebuild or full fetch forgot.
    Reset,
}

/// Whether an error ends the whole pass rather than just this document.
fn pass_fatal(e: &EngineError) -> bool {
    match e {
        EngineError::Remote(RemoteError::Transport { .. } | RemoteError::Url(_)) => true,
        // The server or its proxy is down or throttling: not this document.
        EngineError::Remote(RemoteError::Http {
            status: 502 | 503 | 504 | 429 | 408,
            ..
        }) => true,
        // A 400, 403, 413 or 500 for one document, an undecodable answer,
        // a revision that was not stored: this document only.
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

/// Whether a destination accounts for a vanished file (`claim_dest`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    Yes,
    /// There, but not pushed yet.
    Pending,
    No,
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
    /// The push totals of the running two-way daemon (`run`) since this
    /// engine was opened: what a front-end shows next to `report()`.
    pub fn push_report(&self) -> PushReport {
        self.inner.push_report.lock().unwrap().clone()
    }

    /// A front-end's answer to a mass-delete safety stop the user
    /// confirmed. `shown` is the stop the user saw, as `Status::safety_stop`
    /// gave it; if the stop changed since (more files went missing and a
    /// pass recorded a new one), nothing is allowed and the answer says
    /// what is shown now. Otherwise the next full push pass that gets to
    /// the deletions, and only that one, deletes the files that stop
    /// refused (the ones it showed). Files that went missing since are
    /// judged as usual. Asks for that pass now. (The per-run
    /// `EngineConfig::allow_mass_delete` stays for the CLI.)
    ///
    /// A stop saved by an older version has no list of its files: its
    /// confirmation covers what is missing when it is applied, as long as
    /// that is no more files than the stop showed; otherwise nothing goes
    /// and the stop is shown anew.
    pub fn allow_mass_delete_once(&self, shown: &str) -> Result<Confirmation> {
        let (stop, docs) =
            self.with_index(|ix| Ok((ix.get_meta(meta::SAFETY_STOP)?, ix.get_meta(meta::SAFETY_STOP_DOCS)?)))?;
        let Some(stop) = stop else {
            return Ok(Confirmation::NoStop);
        };
        if stop != shown {
            return Ok(Confirmation::Changed(stop));
        }
        let docs = match docs {
            Some(d) => serde_json::from_str::<HashSet<String>>(&d)
                .ok()
                .map(ConfirmedDocs::Exactly),
            None => stop_count(&stop).map(ConfirmedDocs::UpTo),
        };
        let Some(docs) = docs else {
            // Not a mass-delete stop this version can read: look again, so
            // a pass records it anew.
            self.rescan_now();
            return Err(EngineError::Other(
                "this safety stop cannot be confirmed as it was saved; it is checked again now, confirm the new one"
                    .into(),
            ));
        };
        *self.inner.mass_delete_once.lock().unwrap() = Some(Confirmed { stop, docs });
        self.rescan_now();
        Ok(Confirmation::Allowed)
    }

    /// Whether an `allow_mass_delete_once` is still waiting for its pass.
    pub fn mass_delete_allowed(&self) -> bool {
        self.inner.mass_delete_once.lock().unwrap().is_some()
    }

    /// Local deletions the running daemon holds until their burst is over
    /// (`EngineConfig::delete_hold`).
    pub fn deletes_waiting(&self) -> u64 {
        self.inner.deletes_waiting.load(Ordering::SeqCst)
    }

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
        ctx.incomplete = true;
        if let Some((path, hash)) = ctx.attempt.take() {
            ctx.failed_paths.insert(path);
            ctx.failed_hashes.insert(hash);
        }
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
        match self.full_pass(&RenameHints::default(), None, defer_recent).await? {
            Some((rep, _)) => Ok(rep),
            None => Err(EngineError::Other(format!(
                "not pushing: {}; a two-way sync completes it first",
                self.pushes_held()?.unwrap_or("pushes are held")
            ))),
        }
    }

    /// `None`: pushes are held (see `Engine::pushes_held`); nothing was done.
    /// With a `hold` (the daemon), deletions with no known destination wait
    /// for the burst to end; without one, this pass decides on them.
    async fn full_pass(
        &self,
        hints: &RenameHints,
        hold: Option<&mut DeleteHold>,
        defer_recent: bool,
    ) -> Result<Option<(PushReport, Vec<String>)>> {
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
        let scan_at = Instant::now();
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
            incomplete: !walk.unreadable.is_empty(),
            // Used up once this pass got through the deletions (below), so a
            // later pass never inherits it, and a pass that fails before
            // them does not lose it.
            confirmed: self.inner.mass_delete_once.lock().unwrap().clone(),
            ..Default::default()
        };
        let confirming = ctx.confirmed.as_ref().map(|c| c.stop.clone());
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
            hold,
            &walk.unreadable,
            Tally {
                tracked,
                visible_empty,
                scan: Some(scan_at),
            },
            &mut ctx,
            &mut rep,
            &mut issues_seen,
        )
        .await?;
        // A confirmation this pass did not apply, because some of its files
        // could not be checked, waits for a pass that can check them all.
        let unapplied = ctx.confirmed.is_some() && ctx.confirmation_waits;
        if let Some(stop) = confirming.filter(|_| !unapplied) {
            let mut once = self.inner.mass_delete_once.lock().unwrap();
            // Unless the user confirmed another stop meanwhile.
            if once.as_ref().is_some_and(|c| c.stop == stop) {
                *once = None;
            }
        }
        if deferred.is_empty() {
            self.with_index(|ix| ix.retain_issues(&issues_seen))?;
        }
        // The reconcile pass is over once every file was looked at (files
        // still being written are recent, so its rule would not apply).
        if ctx.reconcile && !ctx.incomplete {
            self.with_index(|ix| ix.delete_meta(meta::RECONCILE))?;
        }
        // Held deletions are not decided yet: a stop shown for them stays.
        self.finish_pass(&rep, !ctx.deletes_held)?;
        Ok(Some((rep, deferred)))
    }

    /// Look at specific paths (files, folders, or paths that vanished).
    /// `None`: pushes are held; nothing was done. Deletions with no known
    /// destination are only noted in `hold`: a partial pass sees too little
    /// to judge them.
    async fn partial_pass(
        &self,
        paths: Vec<String>,
        hints: &RenameHints,
        hold: &mut DeleteHold,
    ) -> Result<Option<PushReport>> {
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
        let mut ctx = PassCtx {
            reconcile: self.with_index(|ix| ix.get_meta(meta::RECONCILE))?.is_some(),
            ..Default::default()
        };
        for (_, f) in files {
            let row = self.row_for_local(&f.rel)?;
            if let Some(r) = &row {
                hold.forget(&r.doc_id);
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
            Some(hold),
            &unreadable,
            Tally {
                tracked,
                visible_empty: false,
                scan: None,
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
            self.with_index(|ix| {
                ix.delete_meta(meta::SAFETY_STOP)?;
                ix.delete_meta(meta::SAFETY_STOP_DOCS)
            })?;
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
                ctx.incomplete = true;
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
        ctx.attempt = Some((doc_path.clone(), hash.clone()));
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
        let no_base = match &row {
            None if ctx.reconcile => NoBase::Reconcile,
            None => NoBase::New,
            Some(_) => NoBase::Reset,
        };
        let written = match self.write_meta(doc, base.as_deref(), no_base).await? {
            MetaWrite::Written(w) => w,
            MetaWrite::DeletedElsewhere => {
                ctx.attempt = None;
                if !self.remove_deleted_elsewhere(rel, &doc_id, row.as_ref(), &hash, rep)? {
                    // Not settled yet: the reconcile pass must come back to it.
                    ctx.incomplete = true;
                }
                return Ok(issue);
            }
        };
        ctx.attempt = None;
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
            ctx.new_files.entry(hash.clone()).or_default().push(doc_path.clone());
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
    /// Returns false when the file was left in place for now (no recovered
    /// copy could be saved).
    fn remove_deleted_elsewhere(
        &self,
        rel: &str,
        doc_id: &str,
        row: Option<&FileRow>,
        hash: &str,
        rep: &mut PushReport,
    ) -> Result<bool> {
        let abs = self.inner.cfg.vault.join(rel);
        // Keep a copy in the state directory first: a removal decided on
        // mtimes must never be the end of the only copy.
        match std::fs::read(&abs) {
            Ok(bytes) if vaultfs::hash_bytes(&bytes) == hash => {
                if !self.save_recovered(rel, &bytes) {
                    let note = "deleted on another device after this copy last changed, but no copy could be kept in the state directory; the local file is left alone";
                    warn!("{rel}: {note}");
                    self.with_index(|ix| ix.set_issue(rel, "stale-copy", note, vaultfs::now_ms()))?;
                    rep.issues += 1;
                    return Ok(false);
                }
            }
            // Changed since it was read (a fresh edit, newer than the
            // deletion): the next pass pushes it.
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
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
                })?;
                Ok(true)
            }
            // An edit landed meanwhile: a fresh change, newer than the
            // deletion, which the next pass pushes.
            Ok(RemoveOutcome::Raced { displaced }) => {
                warn!("{rel}: edited while its stale copy was being removed; the edit is kept");
                self.put_back(rel, displaced, None);
                Ok(true)
            }
            Ok(RemoveOutcome::Diverged { .. }) => Ok(true),
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
    async fn write_meta(&self, doc: Value, base: Option<&str>, no_base: NoBase) -> Result<MetaWrite> {
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
            None => self.write_unknown_ancestry(doc, no_base).await,
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
    /// A soft-deleted document is replaced by a new local file (upstream's
    /// "latest" mode). A file older than the deletion is judged instead:
    ///
    /// - in the reconcile pass after a full fetch (`NoBase::Reconcile`, a
    ///   fresh index next to an existing vault) the deleted content itself
    ///   is a stale copy (removed locally), anything else a conflict;
    /// - a tracked file that lost its revision (`NoBase::Reset`) was kept by
    ///   the pull side on purpose: it becomes a conflict beside the deletion,
    ///   never a removal.
    async fn write_unknown_ancestry(&self, doc: Value, no_base: NoBase) -> Result<MetaWrite> {
        let id = doc["_id"].as_str().unwrap_or_default().to_string();
        for _ in 0..3 {
            match self.inner.couch.fetch_doc(&id, true).await? {
                DocFetch::Found(cur) => {
                    let rev = cur["_rev"].as_str().unwrap_or_default().to_string();
                    let soft_deleted = cur.get("deleted").and_then(Value::as_bool).unwrap_or(false);
                    if soft_deleted {
                        let deleted_at = cur.get("mtime").and_then(Value::as_i64).unwrap_or(0);
                        let ours = doc.get("mtime").and_then(Value::as_i64).unwrap_or(0);
                        if no_base == NoBase::New || ours > deleted_at {
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
                        if no_base == NoBase::Reconcile && same_content(&cur, &doc) {
                            // The very content that was deleted, and not
                            // changed since: it was deleted elsewhere.
                            return Ok(MetaWrite::DeletedElsewhere);
                        }
                        // Older than the deletion but different from what
                        // was deleted: keep both, as a conflict.
                        warn!("{id}: deleted on another device, but this copy is kept as a conflicting revision beside the deletion");
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
        mut hold: Option<&mut DeleteHold>,
        unreadable: &[(String, String)],
        tally: Tally,
        ctx: &mut PassCtx,
        rep: &mut PushReport,
        issues_seen: &mut HashSet<String>,
    ) -> Result<()> {
        if rows.is_empty() {
            if tally.scan.is_some() {
                if let Some(h) = hold.as_deref_mut() {
                    h.checked(None);
                }
            }
            return self.nothing_vanished(hold, &tally, false);
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
        let mut unchecked: Vec<String> = Vec::new();
        for r in rows {
            if unreadable.iter().any(|(p, _)| under(&r.path, p)) {
                rep.unreadable += 1;
                unchecked.push(r.doc_id);
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
                    unchecked.push(r.doc_id);
                }
            }
        }
        // Some of the files a burst or its stop depends on could not be
        // checked (they may still be gone): this pass cannot tell that the
        // burst is over, nor that the stop is. Other files it could not
        // check do not matter to either. As far as this pass knows, those
        // files are still missing (`hidden`): a stop judges them so, and
        // still lists them.
        let blocked = self.depended_on(&unchecked, hold.as_deref())?;
        let hidden: HashSet<String> = blocked.clone().unwrap_or_default();
        // A confirmation is applied whole, by a pass that can check all the
        // files it covers.
        ctx.confirmation_waits = match &ctx.confirmed {
            Some(Confirmed {
                docs: ConfirmedDocs::Exactly(d),
                ..
            }) => unchecked.iter().any(|id| d.contains(id)),
            Some(Confirmed {
                docs: ConfirmedDocs::UpTo(_),
                ..
            }) => !unchecked.is_empty(),
            None => false,
        };
        let unchecked = blocked.is_some();
        if tally.scan.is_some() {
            if let Some(h) = hold.as_deref_mut() {
                h.checked(blocked);
            }
        }
        ctx.deletes_held |= unchecked;
        if gone.is_empty() {
            return self.nothing_vanished(hold, &tally, unchecked);
        }
        // Decide every fate first. A deletion counts as a rename or a move
        // into `.trash/` only when its destination is there (and, inside the
        // synced folders, on the server); each destination accounts for one
        // vanished file. A rename hint alone is not enough: a folder renamed
        // and deleted at once leaves one behind.
        let mut fates: Vec<(FileRow, Fate)> = Vec::new();
        let mut unknown: Vec<FileRow> = Vec::new();
        let mut claimed: HashSet<String> = HashSet::new();
        // Renamed documents whose new name is not pushed yet (waiting or
        // past their wait): their wait is remembered until that resolves.
        let mut waiting: HashSet<String> = HashSet::new();
        for r in gone {
            let dest = hints.dest(&r.path);
            let failed_dest = dest.as_ref().is_some_and(|d| ctx.failed_paths.contains(d))
                || r.disk_hash.as_ref().is_some_and(|h| ctx.failed_hashes.contains(h));
            if failed_dest {
                // Renamed, but the new name could not be pushed: deleting
                // the old document now would leave the note nowhere else.
                info!(
                    "{}: renamed, but its new name could not be pushed yet; the old document is kept for now",
                    r.path
                );
                continue;
            }
            if let Some(d) = dest {
                match self.fate_of(&d) {
                    Fate::OutOfScope(d) => {
                        fates.push((r, Fate::OutOfScope(d)));
                        continue;
                    }
                    Fate::Delete { .. } => match self.claim_dest(&d, &mut claimed)? {
                        Claim::Yes => {
                            fates.push((r, Fate::Delete { dest: Some(d) }));
                            continue;
                        }
                        Claim::Pending => {
                            waiting.insert(r.doc_id.clone());
                            if hold.as_deref_mut().is_none_or(|h| h.may_wait(&r.doc_id)) {
                                // Renamed, and the new name is still being
                                // written: decide once it is pushed (for at
                                // most the maximum hold in the daemon).
                                info!(
                                    "{}: renamed to {d}, which is not pushed yet; the old document is kept for now",
                                    r.path
                                );
                                ctx.dests_pending = true;
                                continue;
                            }
                        }
                        Claim::No => {}
                    },
                }
            }
            // The same content appeared under a new name in this pass.
            let twin = match r.disk_hash.as_ref().and_then(|h| ctx.new_files.get_mut(h)) {
                Some(paths) => {
                    let mut hit = None;
                    while let Some(d) = paths.pop() {
                        if self.claim_dest(&d, &mut claimed)? == Claim::Yes {
                            hit = Some(d);
                            break;
                        }
                    }
                    hit
                }
                None => None,
            };
            match twin {
                Some(d) => fates.push((r, Fate::Delete { dest: Some(d) })),
                None => unknown.push(r),
            }
        }
        if !unknown.is_empty() {
            let found = self.find_moved_out(&unknown, &claimed).await?;
            for r in unknown {
                let fate = match found.get(&r.doc_id) {
                    Some(d) => match self.fate_of(d) {
                        Fate::Delete { .. } if self.claim_dest(d, &mut claimed)? != Claim::Yes => {
                            Fate::Delete { dest: None }
                        }
                        f => f,
                    },
                    None => Fate::Delete { dest: None },
                };
                fates.push((r, fate));
            }
        }
        // The mass-delete stop: deletions nobody can account for (no rename,
        // no move to `.trash/` or out of scope), against what was tracked at
        // the start of the pass.
        // Deletions the user confirmed after a stop go now; they leave the
        // count, as if deleted before this pass.
        // A stop saved by an older version showed fewer files than are
        // missing now: none goes, and a new stop lists them.
        let mut stop_anew = false;
        let applied = if ctx.confirmation_waits {
            None
        } else {
            ctx.confirmed.take()
        };
        let confirmed_docs: Option<HashSet<String>> = match applied {
            None => None,
            Some(Confirmed {
                docs: ConfirmedDocs::Exactly(docs),
                ..
            }) => Some(docs),
            Some(Confirmed {
                stop,
                docs: ConfirmedDocs::UpTo(shown),
            }) => {
                let missing: HashSet<String> = fates
                    .iter()
                    .filter(|(_, f)| matches!(f, Fate::Delete { dest: None }))
                    .map(|(r, _)| r.doc_id.clone())
                    .collect();
                if missing.len() as u64 <= shown {
                    // From now on the confirmation covers these documents
                    // and no others, also if this pass fails and a later
                    // one applies it again.
                    let mut once = self.inner.mass_delete_once.lock().unwrap();
                    if once.as_ref().is_some_and(|c| c.stop == stop) {
                        *once = Some(Confirmed {
                            stop,
                            docs: ConfirmedDocs::Exactly(missing.clone()),
                        });
                    }
                    Some(missing)
                } else {
                    warn!(
                        "{} file(s) are missing, more than the {shown} the confirmed safety stop showed; none is deleted on that confirmation",
                        missing.len()
                    );
                    stop_anew = true;
                    None
                }
            }
        };
        let confirmed = |id: &str| confirmed_docs.as_ref().is_some_and(|c| c.contains(id));
        let undestined_ids: Vec<&str> = fates
            .iter()
            .filter(|(r, f)| matches!(f, Fate::Delete { dest: None }) && !confirmed(&r.doc_id))
            .map(|(r, _)| r.doc_id.as_str())
            .collect();
        let undestined = undestined_ids.len() as u64;
        let allowed = fates
            .iter()
            .filter(|(r, f)| matches!(f, Fate::Delete { dest: None }) && confirmed(&r.doc_id))
            .count() as u64;
        let tracked = tally.tracked.saturating_sub(allowed);
        if confirmed_docs.is_some() {
            // The stop is answered, and with it the burst that led to it;
            // anything still too much stops anew.
            warn!("{allowed} file(s) the user confirmed deleting after a safety stop; deleting them remotely");
            self.with_index(|ix| {
                ix.delete_meta(meta::SAFETY_STOP)?;
                ix.delete_meta(meta::SAFETY_STOP_DOCS)?;
                ix.delete_meta(meta::DELETE_BURST)
            })?;
            if let Some(h) = hold.as_deref_mut() {
                h.answered();
            }
        }
        // In the daemon, wait for the burst to end. A decision counts the
        // deletions its burst already sent (when the maximum hold cut it
        // into parts) as missing too, against what was tracked before them.
        let burst_before = match hold.as_deref() {
            Some(h) => h.burst.clone(),
            None => self.saved_burst()?,
        };
        let mut saved = burst_before.clone();
        let earlier = match hold.as_deref_mut() {
            Some(h) => {
                if tally.scan.is_some() {
                    self.prune_burst(&mut h.burst)?;
                }
                let before = h.hold_due();
                let decision = h.observe(&undestined_ids, tally.scan);
                if decision.is_none() && h.hold_due() != before {
                    info!(
                        "{undestined} file(s) missing with no known destination; deleting them remotely once none has gone missing for {:.0?}",
                        h.period
                    );
                }
                decision
            }
            // A one-shot pass decides now. It has no burst of its own, but
            // counts one a daemon left open (killed in the middle of it,
            // say), and the count of a stop a burst led to.
            None => {
                self.prune_burst(&mut saved)?;
                Some(saved.len() as u64)
            }
        };
        let held = earlier.is_none();
        let earlier = earlier.unwrap_or(0);
        ctx.deletes_held |= held;
        if tally.scan.is_some() {
            if let Some(h) = hold.as_deref_mut() {
                h.keep_pending(&waiting);
            }
        }
        if ctx.dests_pending {
            // Look again soon: a full pass decides once the new names are
            // pushed (a partial pass may never see the old ones again).
            if let Some(h) = hold.as_deref_mut() {
                h.recheck();
            }
            ctx.deletes_held = true;
        }
        // The burst's or the stop's files this pass could not check count as
        // still missing; they are not sent either way.
        let refusing = undestined + hidden.len() as u64;
        let missing = refusing + earlier;
        let of = tracked + earlier;
        let mass =
            (!held || stop_anew) && undestined > 0 && (stop_anew || missing > MASS_DELETE_MIN && missing * 2 > of);
        let emptied = !held && tally.visible_empty && undestined >= EMPTIED_TREE_MIN;
        let mut withhold = held;
        if mass || emptied {
            let empty = if emptied { "the synced folders look empty: " } else { "" };
            let (msg, them) = if earlier > 0 {
                (
                    format!(
                        "{empty}{missing} of {of} tracked files went missing from the vault in one burst, {earlier} of them already deleted remotely"
                    ),
                    format!("the other {refusing}"),
                )
            } else if emptied {
                (
                    format!("{empty}{refusing} of {tracked} tracked files are missing"),
                    "them".to_string(),
                )
            } else {
                (
                    format!("{refusing} of {tracked} tracked files are missing from the vault"),
                    "them".to_string(),
                )
            };
            if self.inner.cfg.allow_mass_delete {
                warn!("{msg}; deleting {them} remotely (mass deletes allowed)");
            } else {
                let msg = format!("{msg}; not deleting {them} remotely (allow mass deletes if this is intended)");
                warn!("{msg}");
                let refused: HashSet<String> = undestined_ids
                    .iter()
                    .map(|id| id.to_string())
                    .chain(hidden.iter().cloned())
                    .collect();
                self.with_index(|ix| {
                    // The same files refused again: the same stop, so a
                    // confirmation of it still matches.
                    let same = ix.get_meta(meta::SAFETY_STOP)?.is_some()
                        && ix
                            .get_meta(meta::SAFETY_STOP_DOCS)?
                            .and_then(|d| serde_json::from_str::<HashSet<String>>(&d).ok())
                            .is_some_and(|d| d == refused);
                    if !same {
                        let mut docs: Vec<&String> = refused.iter().collect();
                        docs.sort();
                        let docs = serde_json::to_string(&docs).unwrap_or_default();
                        ix.set_meta(meta::SAFETY_STOP, &format!("{} {msg}", vaultfs::now_ms()))?;
                        ix.set_meta(meta::SAFETY_STOP_DOCS, &docs)?;
                    }
                    Ok(())
                })?;
                rep.safety_stops += 1;
                withhold = true;
            }
        }
        if !held && tally.scan.is_some() {
            let sent: &[&str] = if withhold { &[] } else { &undestined_ids };
            match hold {
                Some(h) => {
                    h.decided(withhold, sent);
                    self.save_burst(&burst_before, &h.burst)?;
                }
                // A one-shot decision without a stop ends the burst, as a
                // quiet period would; unless it could not check the burst's
                // files: then what it sends joins the burst.
                None if !withhold && unchecked => {
                    saved.extend(sent.iter().map(|id| id.to_string()));
                    self.save_burst(&burst_before, &saved)?;
                }
                None if !withhold => self.with_index(|ix| ix.delete_meta(meta::DELETE_BURST))?,
                None => {}
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
                Fate::Delete { dest: None } if withhold && !confirmed(&r.doc_id) => {}
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

    /// A pass found nothing missing. In a full pass that is a decision too:
    /// nothing is held, and a burst the maximum hold cut is over once quiet.
    /// Unless some of the files the burst or its stop depends on could not
    /// be checked (`unchecked`): then they stay as they are.
    fn nothing_vanished(&self, hold: Option<&mut DeleteHold>, tally: &Tally, unchecked: bool) -> Result<()> {
        if tally.scan.is_none() {
            return Ok(());
        }
        match hold {
            Some(h) => {
                let before = h.burst.clone();
                h.observe(&[], tally.scan);
                if unchecked {
                    return Ok(());
                }
                h.decided(false, &[]);
                h.keep_pending(&HashSet::new());
                self.save_burst(&before, &h.burst)
            }
            None if unchecked => Ok(()),
            None => self.with_index(|ix| ix.delete_meta(meta::DELETE_BURST)),
        }
    }

    /// Which of the documents a pass could not check (`unchecked`) a burst
    /// or its stop depends on: the ones the hold saw missing (held, or
    /// refused by the stop) and the ones the standing stop lists. `None`
    /// when none of them. With a stop saved without its list, any document
    /// may be one of its files: `Some`, with those the hold saw.
    fn depended_on(&self, unchecked: &[String], hold: Option<&DeleteHold>) -> Result<Option<HashSet<String>>> {
        if unchecked.is_empty() {
            return Ok(None);
        }
        let (stop, docs) =
            self.with_index(|ix| Ok((ix.get_meta(meta::SAFETY_STOP)?, ix.get_meta(meta::SAFETY_STOP_DOCS)?)))?;
        let listed: Option<HashSet<String>> = docs.and_then(|d| serde_json::from_str(&d).ok());
        let unlisted = stop.is_some() && listed.is_none();
        let stopped = |id: &String| stop.is_some() && listed.as_ref().is_some_and(|l| l.contains(id));
        let theirs: HashSet<String> = unchecked
            .iter()
            .filter(|id| hold.is_some_and(|h| h.seen.contains(*id)) || stopped(id))
            .cloned()
            .collect();
        Ok((unlisted || !theirs.is_empty()).then_some(theirs))
    }

    /// Keep a burst's deletions across restarts (`meta::DELETE_BURST`).
    fn save_burst(&self, before: &HashSet<String>, now: &HashSet<String>) -> Result<()> {
        if before == now {
            return Ok(());
        }
        self.with_index(|ix| {
            if now.is_empty() {
                ix.delete_meta(meta::DELETE_BURST)
            } else {
                ix.set_meta(meta::DELETE_BURST, &serde_json::to_string(now).unwrap_or_default())
            }
        })
    }

    /// The deletions of a burst a previous run left (`meta::DELETE_BURST`).
    pub(crate) fn saved_burst(&self) -> Result<HashSet<String>> {
        Ok(self
            .with_index(|ix| ix.get_meta(meta::DELETE_BURST))?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default())
    }

    /// A document of the burst that is tracked again (its file came back,
    /// or a new one took its name) is no longer gone: as in one pass over
    /// the whole burst, it does not count, and a later deletion of it
    /// counts once.
    fn prune_burst(&self, burst: &mut HashSet<String>) -> Result<()> {
        if burst.is_empty() {
            return Ok(());
        }
        self.with_index(|ix| {
            let mut back = Vec::new();
            for id in burst.iter() {
                if ix.by_doc(id)?.is_some_and(|r| is_tracked(r.state)) {
                    back.push(id.clone());
                }
            }
            for id in back {
                burst.remove(&id);
            }
            Ok(())
        })
    }

    /// Whether a deletion's destination accounts for it: not claimed by
    /// another vanished file in this pass, a file on disk now, and inside
    /// the synced folders also on the server (tracked with a synced
    /// revision, as after its push in this pass). Claims it if so. A
    /// destination that is there but not pushed yet (still being written)
    /// is `Pending`.
    fn claim_dest(&self, dest: &str, claimed: &mut HashSet<String>) -> Result<Claim> {
        if claimed.contains(dest) {
            return Ok(Claim::No);
        }
        let there = std::fs::symlink_metadata(self.inner.cfg.vault.join(dest)).is_ok_and(|m| m.is_file());
        if !there {
            return Ok(Claim::No);
        }
        if !in_trash(dest) {
            let synced = self
                .row_for_local(dest)?
                .is_some_and(|r| r.base_rev.is_some() && is_tracked(r.state));
            if !synced {
                claimed.insert(dest.to_string());
                return Ok(Claim::Pending);
            }
        }
        claimed.insert(dest.to_string());
        Ok(Claim::Yes)
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
    /// and content hash, leaving out paths in `exclude` (destinations other
    /// vanished files already account for). Returns doc ID -> where it is
    /// now. Each file found accounts for one vanished file, and only where
    /// that is unambiguous: when more vanished files share a size, mtime and
    /// hash than files with them were found, none of them counts as moved
    /// (a stale copy left from an earlier move must not account for a new
    /// deletion, pass after pass).
    async fn find_moved_out(&self, rows: &[FileRow], exclude: &HashSet<String>) -> Result<HashMap<String, String>> {
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
        let exclude = exclude.clone();
        tokio::task::spawn_blocking(move || {
            const MAX_ENTRIES: usize = 200_000;
            let root = this.inner.cfg.vault.clone();
            // (size, mtime, hash) -> paths found with them.
            let mut found: BTreeMap<(u64, i64, String), Vec<String>> = BTreeMap::new();
            let mut stack = vec![root.clone()];
            let mut seen = 0usize;
            'walk: while let Some(dir) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                for e in rd.flatten() {
                    seen += 1;
                    if seen > MAX_ENTRIES {
                        break 'walk;
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
                    // Our own vault lock file is no note's new home (an empty
                    // note with its mtime would match it).
                    if !ft.is_file()
                        || this.local_visible(&rel)
                        || rel == crate::engine::VAULT_LOCK
                        || exclude.contains(&rel)
                    {
                        continue;
                    }
                    let Ok(md) = e.metadata() else { continue };
                    let Some(cands) = want.get(&(md.len(), mtime_ms(&md))) else {
                        continue;
                    };
                    let Ok(Some(st)) = vaultfs::read_state(&e.path()) else {
                        continue;
                    };
                    if cands.iter().any(|c| c.disk_hash.as_deref() == Some(st.hash.as_str())) {
                        found.entry((md.len(), mtime_ms(&md), st.hash)).or_default().push(rel);
                    }
                }
            }
            let mut out = HashMap::new();
            for ((size, mtime, hash), mut paths) in found {
                let mut owners: Vec<&FileRow> = want[&(size, mtime)]
                    .iter()
                    .filter(|r| r.disk_hash.as_deref() == Some(hash.as_str()))
                    .collect();
                if owners.len() > paths.len() {
                    continue;
                }
                owners.sort_by(|a, b| a.path.cmp(&b.path));
                paths.sort();
                for (r, p) in owners.into_iter().zip(paths) {
                    out.insert(r.doc_id.clone(), p);
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
        let mut was_enabled = false;
        let mut hints = RenameHints::keeping_extra(self.inner.cfg.delete_hold_max);
        let mut hold = DeleteHold::new(self.inner.cfg.delete_hold, self.inner.cfg.delete_hold_max);
        let stopped = self.with_index(|ix| ix.get_meta(meta::SAFETY_STOP))?.is_some();
        hold.resume(self.saved_burst()?, stopped);
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
                was_enabled = false;
                continue;
            }
            if !was_enabled {
                // Pushes resume (first start, or an owed fetch completed):
                // look at everything now, not at the next periodic rescan.
                was_enabled = true;
                full_due = true;
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
            // Held deletions: the burst looks over; a full pass decides.
            if hold.due().is_some_and(|d| Instant::now() >= d) {
                full_due = true;
            }
            let result = if full_due {
                next_rescan = Instant::now() + self.inner.cfg.rescan_interval;
                match self.full_pass(&hints, Some(&mut hold), true).await {
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
                match self.partial_pass(ready.clone(), &hints, &mut hold).await {
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
                    self.inner.deletes_waiting.store(hold.waiting(), Ordering::SeqCst);
                    self.inner.push_report.lock().unwrap().add(&rep);
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
    /// For a full pass (which sees every missing file): when its walk began.
    scan: Option<Instant>,
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

    /// Whether `DeleteHold::observe` held the deletions.
    fn holds(decision: Option<u64>) -> bool {
        decision.is_none()
    }

    #[test]
    fn the_delete_hold_decides_once_a_burst_is_over() {
        let walk = || Some(Instant::now());
        let mut h = DeleteHold::new(Duration::from_millis(100), Duration::from_secs(60));
        // A partial pass never decides; a new deletion starts the hold.
        assert!(holds(h.observe(&["a"], None)));
        assert!(h.due().is_some());
        assert_eq!(h.waiting(), 1);
        // A full pass within the hold waits too, and more deletions extend it.
        assert!(holds(h.observe(&["a", "b"], walk())));
        assert_eq!(h.waiting(), 2);
        let due = h.due().unwrap();
        // A full pass that walked before the hold ended does not decide,
        // even if it gets here after (its picture may miss later deletions).
        let early = walk();
        std::thread::sleep(Duration::from_millis(120));
        assert!(Instant::now() >= due);
        assert!(holds(h.observe(&["a", "b"], early)));
        // Walked past the hold with nothing new: this full pass decides.
        assert!(!holds(h.observe(&["a", "b"], walk())));
        assert!(h.due().is_none());
        assert_eq!(h.waiting(), 0);
        // Refused by the stop: still missing, nothing new, decided again at once.
        h.decided(true, &[]);
        assert!(!holds(h.observe(&["a", "b"], walk())));
        h.decided(true, &[]);
        assert!(holds(h.observe(&["a"], None)), "a partial pass waits for a full one");
        assert!(h.due().is_none(), "nothing new, nothing scheduled");
        // A refused file back and gone again is a new deletion.
        h.forget("a");
        assert!(holds(h.observe(&["a"], None)));
        assert!(h.due().is_some());
        // One more deletion: the whole set waits again.
        assert!(holds(h.observe(&["a", "b", "c"], walk())));
        std::thread::sleep(Duration::from_millis(120));
        assert!(!holds(h.observe(&["a", "b", "c"], walk())));
        // Sent, they are forgotten, so the same document deleted again
        // starts a new hold.
        h.decided(false, &["a", "b", "c"]);
        assert!(h.due().is_none() && h.seen.is_empty());
        assert!(holds(h.observe(&["a"], None)));
        assert!(h.due().is_some());
        // Nothing missing any more: nothing held.
        assert!(!holds(h.observe(&[], walk())));
        assert!(h.due().is_none() && h.seen.is_empty());
    }

    #[test]
    fn a_burst_the_maximum_cuts_goes_on_until_a_quiet_period() {
        let walk = || Some(Instant::now());
        let mut h = DeleteHold::new(Duration::from_millis(100), Duration::from_millis(200));
        let ids = ["a", "b", "c", "d", "e", "f", "g", "h"];
        // A new deletion every 50 ms: the maximum cuts the hold.
        let mut n = 1;
        let earlier = loop {
            if let Some(e) = h.observe(&ids[..n], walk()) {
                break e;
            }
            assert!(n < 6, "never cut");
            std::thread::sleep(Duration::from_millis(50));
            n += 1;
        };
        assert_eq!(earlier, 0);
        h.decided(false, &ids[..n]);
        assert_eq!(h.burst.len(), n);
        // The burst is open: a full pass looks whether it is over.
        assert!(h.due().is_some());
        // It goes on: the next part is judged together with the first.
        std::thread::sleep(Duration::from_millis(50));
        assert!(holds(h.observe(&ids[n..n + 1], walk())));
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(h.observe(&ids[n..n + 1], walk()), Some(n as u64));
        // That decision came after a quiet period: the burst is over.
        h.decided(false, &ids[n..n + 1]);
        assert!(h.burst.is_empty());
        assert!(h.due().is_none());

        // Cut again, then a stop: the stop keeps the burst, however long
        // it stands, until it is answered.
        h.resume(ids[..5].iter().map(|s| s.to_string()).collect(), false);
        assert!(h.due().is_some());
        assert!(holds(h.observe(&["x"], walk())));
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(h.observe(&["x"], walk()), Some(5));
        h.decided(true, &[]);
        assert!(h.due().is_none(), "a standing stop schedules nothing");
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(h.observe(&["x"], walk()), Some(5));
        h.decided(true, &[]);
        h.answered();
        assert!(h.burst.is_empty());

        // An open burst with nothing more missing ends after a quiet period.
        h.resume(ids[..4].iter().map(|s| s.to_string()).collect(), false);
        assert_eq!(h.observe(&[], walk()), Some(4));
        h.decided(false, &[]);
        assert_eq!(h.burst.len(), 4, "not quiet yet");
        std::thread::sleep(Duration::from_millis(120));
        assert!(h.due().is_some_and(|d| Instant::now() >= d));
        assert_eq!(h.observe(&[], walk()), Some(4));
        h.decided(false, &[]);
        assert!(h.burst.is_empty());
        assert!(h.due().is_none());

        // A quiet pass that could not check a file the burst depends on (one
        // it holds) does not end the burst, still counts that file as seen,
        // and schedules no look of its own.
        h.resume(ids[..4].iter().map(|s| s.to_string()).collect(), false);
        assert!(holds(h.observe(&["y", "z"], walk())));
        std::thread::sleep(Duration::from_millis(120));
        h.checked(Some(HashSet::from(["y".to_string()])));
        assert_eq!(h.observe(&["z"], walk()), Some(4));
        h.decided(false, &["z"]);
        assert_eq!(h.burst.len(), 5, "an incomplete look ended the burst");
        assert!(h.seen.contains("y") && !h.seen.contains("z"));
        assert!(h.due().is_none(), "an incomplete look scheduled another");
        // Checked again and still gone: not a new deletion, judged at once
        // with the burst; after a quiet period that ends it.
        h.checked(None);
        assert_eq!(h.observe(&["y"], walk()), Some(5));
        h.decided(false, &["y"]);
        assert!(h.burst.is_empty() && h.seen.is_empty());
        assert!(h.due().is_none());
    }

    #[test]
    fn stops_saved_without_their_list_say_how_many_files() {
        assert_eq!(
            stop_count("1700000000000 31 of 60 tracked files are missing from the vault; not deleting them remotely"),
            Some(31)
        );
        assert_eq!(
            stop_count("1700000000000 the synced folders look empty: 3 of 3 tracked files are missing"),
            Some(3)
        );
        assert_eq!(stop_count("1700000000000 something else"), None);
        assert_eq!(stop_count("1700000000000 x of 3 tracked files"), None);
    }

    #[test]
    fn a_trickle_of_deletions_is_judged_once_the_hold_reaches_its_maximum() {
        let walk = || Some(Instant::now());
        let mut h = DeleteHold::new(Duration::from_millis(300), Duration::from_millis(600));
        let started = Instant::now();
        let ids: Vec<String> = (0..40).map(|i| format!("d{i}")).collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let mut n = 1;
        // A new deletion every 60 ms keeps the 300 ms hold open...
        loop {
            let held = holds(h.observe(&ids[..n], walk()));
            if !held {
                break;
            }
            assert!(n < ids.len(), "never decided");
            std::thread::sleep(Duration::from_millis(60));
            n += 1;
        }
        // ...until it has lasted 600 ms; then everything so far is judged.
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(600), "{took:?}");
        assert!(took < Duration::from_millis(2000), "{took:?}");
        assert!(n >= 6, "{n}");
    }

    #[test]
    fn a_rename_waits_for_its_new_name_for_at_most_the_maximum_hold() {
        let mut h = DeleteHold::new(Duration::from_millis(10), Duration::from_millis(100));
        assert!(h.may_wait("a"));
        std::thread::sleep(Duration::from_millis(120));
        assert!(!h.may_wait("a"), "waited past the maximum");
        // Still unresolved after a full pass: the wait does not start over.
        h.keep_pending(&["a".to_string()].into());
        assert!(!h.may_wait("a"), "the expired wait restarted");
        // Once resolved, a later rename waits afresh.
        h.keep_pending(&HashSet::new());
        assert!(h.may_wait("a"));
    }

    #[test]
    fn rename_hints_last_as_long_as_a_rename_may_wait() {
        let mut h = RenameHints::keeping_extra(Duration::from_millis(200));
        h.pairs.push((
            "old.md".into(),
            "new.md".into(),
            Instant::now() - Duration::from_secs(60) - Duration::from_millis(100),
        ));
        h.add("x.md".into(), "y.md".into());
        assert_eq!(h.dest("old.md").as_deref(), Some("new.md"), "dropped within its wait");
        let mut h = RenameHints::default();
        h.pairs.push((
            "old.md".into(),
            "new.md".into(),
            Instant::now() - Duration::from_secs(61),
        ));
        h.add("x.md".into(), "y.md".into());
        assert_eq!(h.dest("old.md"), None);
    }

    /// Codex (third round): files other vanished files already account for
    /// are left out, and a copy shared by more vanished files than copies
    /// accounts for none of them.
    #[tokio::test]
    async fn found_files_are_matched_one_to_one_and_only_unambiguously() {
        use crate::engine::EngineConfig;
        use livesync_remote::{Couch, RemoteConfig};
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(vault.join(".trash")).unwrap();
        for n in ["a", "b"] {
            let p = vault.join(format!(".trash/{n}.md"));
            std::fs::write(&p, b"same").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
                .unwrap();
        }
        let couch = Couch::new(&RemoteConfig {
            url: "http://127.0.0.1:9".into(),
            database: "db".into(),
            username: None,
            password: None,
        })
        .unwrap();
        let engine = Engine::open(couch, EngineConfig::new(vault, dir.path().join("state/index.sqlite"))).unwrap();
        let hash = vaultfs::hash_bytes(b"same");
        let row = |p: &str| FileRow {
            path: p.into(),
            doc_id: p.into(),
            base_rev: Some("1-a".into()),
            base_children: vec![],
            disk_mtime: Some(1_700_000_000_000),
            disk_size: Some(4),
            disk_hash: Some(hash.clone()),
            state: State::Clean,
            note: None,
        };
        // `.trash/a.md` already accounts for another note.
        let claimed: HashSet<String> = [".trash/a.md".to_string()].into();
        let found = engine.find_moved_out(&[row("b.md")], &claimed).await.unwrap();
        assert_eq!(found.get("b.md").map(String::as_str), Some(".trash/b.md"));
        // Two vanished, one copy left: neither counts as moved.
        let found = engine
            .find_moved_out(&[row("b.md"), row("c.md")], &claimed)
            .await
            .unwrap();
        assert!(found.is_empty(), "{found:?}");
        // Two and two: one each.
        let found = engine
            .find_moved_out(&[row("b.md"), row("c.md")], &HashSet::new())
            .await
            .unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert_ne!(found["b.md"], found["c.md"]);
    }

    /// A note of a burst that is tracked again (re-created, and pushed) no
    /// longer counts as gone: a file a tool keeps re-creating and deleting
    /// counts once, as in one pass over the whole burst, not once per
    /// deletion. The burst survives a restart.
    #[test]
    fn a_burst_counts_only_the_documents_still_gone() {
        use crate::engine::EngineConfig;
        use livesync_remote::{Couch, RemoteConfig};
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vault")).unwrap();
        let couch = Couch::new(&RemoteConfig {
            url: "http://127.0.0.1:9".into(),
            database: "db".into(),
            username: None,
            password: None,
        })
        .unwrap();
        let engine = Engine::open(
            couch,
            EngineConfig::new(dir.path().join("vault"), dir.path().join("state/index.sqlite")),
        )
        .unwrap();
        let row = |p: &str, state: State| FileRow {
            path: p.into(),
            doc_id: p.into(),
            base_rev: Some("1-a".into()),
            base_children: vec![],
            disk_mtime: Some(1_700_000_000_000),
            disk_size: Some(4),
            disk_hash: None,
            state,
            note: None,
        };
        engine
            .with_index(|ix| {
                ix.upsert(&row("back.md", State::Clean))?;
                ix.upsert(&row("moved.md", State::OutOfScope))
            })
            .unwrap();
        let mut burst: HashSet<String> = ["back.md", "gone.md", "moved.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        engine.save_burst(&HashSet::new(), &burst).unwrap();
        assert_eq!(engine.saved_burst().unwrap(), burst);
        engine.prune_burst(&mut burst).unwrap();
        let left: HashSet<String> = ["gone.md", "moved.md"].iter().map(|s| s.to_string()).collect();
        assert_eq!(burst, left);
        engine.save_burst(&left, &HashSet::new()).unwrap();
        assert!(engine.saved_burst().unwrap().is_empty());
        assert!(engine
            .with_index(|ix| ix.get_meta(meta::DELETE_BURST))
            .unwrap()
            .is_none());
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
