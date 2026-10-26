//! Gate evaluation, the pull loop, the daemons (pull-only and two-way) and
//! milestone registration. The push side lives in `push.rs`.

use crate::fsrules::{FsRules, PROBED_CHARS};
use crate::index::{FileRow, Index, LocalIssue, PendingRow, State};
use crate::vaultfs::{self, DiskState, Expect, LockError, LockFile, RemoveOutcome, WriteError};
use futures::stream::{self, StreamExt};
use livesync_model::chunk::chunk_id_xxhash64;
use livesync_model::codec::{self, CodecError};
use livesync_model::doc::{classify_doc, Classified, MILESTONE_LOCAL_ID, SYNC_PARAMS_LOCAL_ID, VERSION_DOC_ID};
use livesync_model::gate::{self, Milestone, Refusal};
use livesync_model::milestone::{register_node, DeviceInfo};
use livesync_model::path::{classify_path, is_internal_id, PathSkip};
use livesync_model::scope::Scope;
use livesync_model::splitter::SplitParams;
use livesync_model::{FileDoc, FileKind};
use livesync_remote::{Backoff, ChangeRow, ChangesQuery, Couch, DocFetch, PutOutcome, RemoteError};
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{watch, Notify, Semaphore};
use tracing::{debug, info, warn};

use meta_keys as meta;

/// Keys used in the index `meta` table.
pub(crate) mod meta_keys {
    pub const SINCE: &str = "since";
    pub const REMOTE: &str = "remote";
    pub const NODE_ID: &str = "node_id";
    pub const CREATED: &str = "remote_created";
    pub const SALT: &str = "remote_pbkdf2salt";
    pub const TWEAKS: &str = "adopted_tweaks";
    pub const VERSION: &str = "remote_version";
    pub const LAST_GATE: &str = "last_gate";
    pub const LAST_GATE_OK: &str = "last_gate_ok_ms";
    pub const LAST_ERROR: &str = "last_error";
    pub const LAST_PULL: &str = "last_pull_ms";
    pub const REBUILD_AT: &str = "rebuild_detected_ms";
    pub const DAEMON_SEEN: &str = "daemon_heartbeat_ms";
    pub const VAULT: &str = "vault_path";
    /// The folder scope the index was last synced with (JSON).
    pub const SCOPE: &str = "scope";
    /// The probed filesystem rules (JSON).
    pub const FS_RULES: &str = "fs_rules";
    /// Set when a full fetch is owed: a fresh index, a rebuild seen on the
    /// remote, or an explicit `request_full_fetch`. Cleared by the two-way
    /// side once it caught up (after the quiet wait when one is owed) and
    /// registered; a read-only pull clears it only when neither a quiet wait
    /// nor a lock resolution is owed with it. While set, pushes refuse to run.
    pub const NEEDS_FETCH: &str = "needs_full_fetch";
    /// The milestone `created` value a lock was let through for (a full
    /// fetch was owed). Cleared once this node is marked resolved
    /// (`accepted_nodes`), which only the two-way side does, or once the
    /// milestone no longer locks us out. While set, pushes refuse to run.
    pub const RESOLVE_OWED: &str = "resolve_owed";
    /// A rebuild was seen: the refetch must wait until the remote stops
    /// changing before it counts as complete. `<ms> <description>`; shown
    /// by `status` while owed or running, cleared when the remote settled.
    pub const REBUILD_WAIT: &str = "rebuild_wait";
    /// Files a rebuilt (or refetched) database lacked and this device pushed
    /// back, since the last full fetch (JSON `OrphanRecord`). For `status`.
    pub const ORPHANS: &str = "rebuild_orphans";
    /// The first full push pass after a full fetch reconciles files that
    /// have no known revision (a fresh index next to an existing vault):
    /// there, a local copy older than a remote deletion of the same content
    /// is stale. Set with `needs_full_fetch`, cleared after that pass.
    pub const RECONCILE: &str = "reconcile_pending";
    /// When the node was last registered in the milestone.
    pub const REGISTERED: &str = "registered_ms";
    pub const LAST_PUSH: &str = "last_push_ms";
    /// A safety stop that needs a human (for example a mass deletion).
    pub const SAFETY_STOP: &str = "safety_stop";
}

/// Note prefix (and status filter) for documents whose names the vault's
/// filesystem cannot hold.
pub const UNWRITABLE_NOTE: &str = "unwritable name";
/// Note prefix for documents skipped by the case-collision guard.
pub const COLLISION_NOTE: &str = "case collision with";

/// Lock file in the state directory (next to the index).
pub const STATE_LOCK: &str = "lsync.lock";
/// Lock file at the vault root, so two profiles cannot share a vault either.
pub const VAULT_LOCK: &str = ".lsync.lock";

/// Minimum spacing between pending retries in the daemon, so a retry that
/// fails before it can reschedule its rows does not spin.
const PENDING_RETRY_MIN_GAP: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Vault root folder.
    pub vault: PathBuf,
    /// SQLite index file. Keep it outside the vault (or under a dot-directory).
    pub index_path: PathBuf,
    /// Local `syncIgnoreRegEx` (upstream format: patterns joined by `|[]|`,
    /// `!!` prefix negates, spaces and newlines are removed). Not part of the
    /// remote tweaks.
    pub ignore_regex: Option<String>,
    /// Override for `syncMaxSizeInMB` (otherwise taken from PREFERRED).
    pub max_size_mb: Option<u64>,
    /// Chunk-fetch units in flight.
    pub concurrency: usize,
    /// `_changes` page size.
    pub batch_limit: usize,
    /// How often the daemon re-runs the connect gate.
    pub regate_interval: Duration,
    /// Memory budget for in-flight chunk data, in MiB.
    pub memory_budget_mb: usize,
    /// Skip names with characters FAT-style storage refuses (`" * : < > ? \ |`
    /// and control characters) without trying them. `None`: as probed on the
    /// vault's filesystem at open. Names the filesystem rejects at write time
    /// are skipped either way.
    pub restrict_names: Option<bool>,
    /// Treat the vault's filesystem as folding case and Unicode
    /// normalisation (`Some(true)`) or not. `None`: as probed.
    pub case_insensitive: Option<bool>,
    /// Folder scope (spec "Folder scope"). Changing it means opening a new
    /// engine; the change is reconciled at the next sync.
    pub scope: Scope,
    /// When the scope narrows, also remove the local copies of files that
    /// fell out of it (clean ones only; the remote is never touched).
    pub remove_out_of_scope_copies: bool,
    /// A changed file counts as settled once its size and mtime were stable
    /// this long.
    pub settle: Duration,
    /// Full rescan interval of the two-way daemon.
    pub rescan_interval: Duration,
    /// Use the platform's file watcher in the two-way daemon (otherwise
    /// periodic rescans only).
    pub watch: bool,
    /// Device name advertised in the milestone (default: `lsync-<host>`).
    pub device_name: Option<String>,
    /// After a rebuild elsewhere, the remote's update sequence must stay
    /// unchanged this long before the refetch counts as complete (the
    /// rebuilding device uploads for a while after the lock).
    pub rebuild_quiet: Duration,
    /// Allow a single sync pass to delete more than half of the tracked files
    /// (otherwise it stops and asks, in case the vault folder vanished).
    pub allow_mass_delete: bool,
    /// Asked before any pass that could delete remote documents (a seam for
    /// the front-end, e.g. Android's "all files access" check). `None`: no
    /// check beyond the engine's own.
    pub access_check: Option<AccessCheck>,
}

/// A front-end check that the vault is fully visible to this process (for
/// example that Android's all-files access is still granted). `Err` carries
/// the reason; while it fails, missing files are never treated as deleted.
#[derive(Clone)]
pub struct AccessCheck(pub Arc<dyn Fn() -> std::result::Result<(), String> + Send + Sync>);

impl AccessCheck {
    pub fn new(f: impl Fn() -> std::result::Result<(), String> + Send + Sync + 'static) -> Self {
        AccessCheck(Arc::new(f))
    }
}

impl std::fmt::Debug for AccessCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccessCheck(..)")
    }
}

impl EngineConfig {
    pub fn new(vault: PathBuf, index_path: PathBuf) -> Self {
        EngineConfig {
            vault,
            index_path,
            ignore_regex: None,
            max_size_mb: None,
            concurrency: 6,
            batch_limit: 250,
            regate_interval: Duration::from_secs(300),
            memory_budget_mb: 160,
            restrict_names: None,
            case_insensitive: None,
            scope: Scope::all(),
            remove_out_of_scope_copies: false,
            settle: Duration::from_secs(2),
            rescan_interval: Duration::from_secs(15 * 60),
            watch: true,
            device_name: None,
            rebuild_quiet: Duration::from_secs(30),
            allow_mass_delete: false,
            access_check: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("refused: {0}")]
    Refused(#[from] Refusal),
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error("index: {0}")]
    Index(#[from] rusqlite::Error),
    /// Another process holds the state directory or the vault.
    #[error("{0}")]
    Busy(String),
    #[error("{0}")]
    Other(String),
}

impl EngineError {
    pub(crate) fn other(e: impl std::fmt::Display) -> Self {
        EngineError::Other(e.to_string())
    }
}

type Result<T> = std::result::Result<T, EngineError>;

/// Outcome of the connect gate.
#[derive(Debug, Clone, Serialize)]
pub struct GateReport {
    pub remote: String,
    pub node_id: String,
    pub version: i64,
    pub doc_count: u64,
    pub created: i64,
    pub locked: bool,
    pub accepted_nodes: usize,
    pub adopted_tweaks_now: bool,
    pub rebuild_detected: bool,
    pub salt_present: bool,
    pub max_size_mb: u64,
    pub case_sensitive: bool,
    /// Locked against this node, but a full fetch may proceed (fresh index or
    /// rebuild seen); the node is marked resolved after it.
    pub fetch_to_resolve: bool,
    /// A full fetch is still owed before pushing (see `fetch_to_resolve`).
    pub needs_full_fetch: bool,
}

/// Result of [`Engine::sync_once`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncReport {
    pub pull: PullReport,
    pub push: crate::push::PushReport,
}

/// Counters for one pull (or one daemon session).
#[derive(Debug, Clone, Default, Serialize)]
pub struct PullReport {
    pub batches: u64,
    pub changes: u64,
    pub written: u64,
    pub adopted: u64,
    pub unchanged: u64,
    pub deleted: u64,
    pub dirty: u64,
    pub conflicted: u64,
    pub pending: u64,
    pub skipped: BTreeMap<String, u64>,
    pub ignored_docs: u64,
    pub chunks_fetched: u64,
    pub chunk_bytes: u64,
    pub bytes_written: u64,
    pub size_mismatches: u64,
    pub chunk_id_mismatches: u64,
    /// Local edits that landed while a remote change was being written, put back.
    pub restored_edits: u64,
    pub elapsed_ms: u64,
}

impl PullReport {
    /// `self - before`, for per-batch log lines.
    fn delta(&self, before: &PullReport) -> PullReport {
        let mut skipped = self.skipped.clone();
        for (k, v) in &before.skipped {
            if let Some(x) = skipped.get_mut(k) {
                *x = x.saturating_sub(*v);
            }
        }
        skipped.retain(|_, v| *v > 0);
        PullReport {
            batches: self.batches - before.batches,
            changes: self.changes - before.changes,
            written: self.written - before.written,
            adopted: self.adopted - before.adopted,
            unchanged: self.unchanged - before.unchanged,
            deleted: self.deleted - before.deleted,
            dirty: self.dirty - before.dirty,
            conflicted: self.conflicted - before.conflicted,
            pending: self.pending - before.pending,
            skipped,
            ignored_docs: self.ignored_docs - before.ignored_docs,
            chunks_fetched: self.chunks_fetched - before.chunks_fetched,
            chunk_bytes: self.chunk_bytes - before.chunk_bytes,
            bytes_written: self.bytes_written - before.bytes_written,
            size_mismatches: self.size_mismatches - before.size_mismatches,
            chunk_id_mismatches: self.chunk_id_mismatches - before.chunk_id_mismatches,
            restored_edits: self.restored_edits - before.restored_edits,
            elapsed_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub remote: Option<String>,
    pub vault: String,
    pub index: String,
    pub node_id: Option<String>,
    pub since: Option<String>,
    pub remote_created: Option<i64>,
    pub remote_version: Option<String>,
    pub last_gate: Option<String>,
    pub last_gate_ok_ms: Option<i64>,
    pub last_pull_ms: Option<i64>,
    pub last_error: Option<String>,
    pub daemon_heartbeat_ms: Option<i64>,
    pub rebuild_detected_ms: Option<i64>,
    pub counts: BTreeMap<String, u64>,
    pub dirty: Vec<FileRow>,
    pub conflicted: Vec<FileRow>,
    pub pending: Vec<PendingRow>,
    /// Skipped because the vault's filesystem cannot hold the name.
    pub unwritable: Vec<FileRow>,
    /// Skipped because another file's name folds to the same one here.
    pub collisions: Vec<FileRow>,
    /// Moved out of the folder scope locally; the remote document is kept.
    pub out_of_scope: Vec<FileRow>,
    /// Local files the push side cannot sync.
    pub local_issues: Vec<LocalIssue>,
    pub scope: Option<String>,
    pub fs_rules: Option<String>,
    pub registered_ms: Option<i64>,
    pub last_push_ms: Option<i64>,
    /// A full fetch is owed, or this node still has to be marked resolved;
    /// pushes are held until the two-way side completes it.
    pub needs_full_fetch: bool,
    pub safety_stop: Option<String>,
    /// After a rebuild: the refetch is waiting (or owes a wait) for the
    /// remote to stop changing. `<ms> <description>`.
    pub rebuild_wait: Option<String>,
    /// The last push-back of files a rebuilt or refetched database lacked:
    /// `<ms> <count> <description and paths>`.
    pub rebuild_orphans: Option<String>,
}

/// One upstream `syncIgnoreRegEx` entry.
#[derive(Debug, Clone)]
struct IgnoreRule {
    re: Regex,
    negate: bool,
}

/// Parse a `syncIgnoreRegEx` list as upstream does (`parseCustomRegExpList`
/// in commonlib `utils.ts`): drop every space and newline from the whole
/// list, split on `|[]|`, drop empty entries; a `!!` prefix negates.
fn parse_ignore(src: &str, case_sensitive: bool) -> std::result::Result<Vec<IgnoreRule>, regex::Error> {
    let cleaned: String = src.chars().filter(|c| *c != ' ' && *c != '\n').collect();
    cleaned
        .split("|[]|")
        .filter(|s| !s.is_empty())
        .map(|s| {
            let (negate, pat) = match s.strip_prefix("!!") {
                Some(p) => (true, p),
                None => (false, s),
            };
            let re = regex::RegexBuilder::new(pat)
                .case_insensitive(!case_sensitive)
                .build()?;
            Ok(IgnoreRule { re, negate })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub(crate) struct Runtime {
    pub(crate) max_size_bytes: Option<u64>,
    ignore: Vec<IgnoreRule>,
    /// Document IDs keep their case (`handleFilenameCaseSensitive`).
    pub(crate) case_sensitive_ids: bool,
    pub(crate) split: SplitParams,
    /// The PREFERRED tweaks, advertised as ours when registering.
    pub(crate) preferred: Map<String, Value>,
}

impl Runtime {
    pub(crate) fn ignored(&self, path: &str) -> bool {
        self.ignore.iter().any(|r| r.re.is_match(path) != r.negate)
    }
}

pub(crate) struct Inner {
    pub(crate) couch: Couch,
    pub(crate) cfg: EngineConfig,
    /// Canonical vault path, recorded in (and checked against) the index.
    pub(crate) vault_canon: String,
    pub(crate) state_dir: PathBuf,
    /// What the vault's filesystem can hold (probed at open).
    pub(crate) fs: FsRules,
    _locks: Vec<LockFile>,
    pub(crate) index: Mutex<Index>,
    pub(crate) runtime: Mutex<Runtime>,
    mem: Semaphore,
    report: Mutex<PullReport>,
    pub(crate) nudge: Notify,
    /// Wakes the push loop for a full rescan.
    pub(crate) rescan: Notify,
    /// Serialises the pull and push sides: a batch of remote changes and a
    /// pass of local changes never run at the same time.
    pub(crate) ops: tokio::sync::Mutex<()>,
    /// The two-way daemon allows pushes only after the gate passed and the
    /// owed full fetch and registration are done.
    pub(crate) push_enabled: AtomicBool,
    /// Held while materialising on a filesystem that folds names, so the
    /// collision check and the write cannot interleave across units.
    fold_lock: Mutex<()>,
    /// The milestone the gate read last (for log lines).
    last_milestone: Mutex<Option<Value>>,
}

#[derive(Clone)]
pub struct Engine {
    pub(crate) inner: Arc<Inner>,
}

/// Why a file document is not materialised.
#[derive(Debug, Clone)]
enum Skip {
    Path(PathSkip),
    Ignored,
    TooLarge(u64),
    Legacy,
    /// The vault's filesystem cannot hold this name.
    Unwritable(String),
    /// Another document's file already has this name here (case or
    /// normalisation folding).
    Collision(String),
}

impl Skip {
    fn label(&self) -> &'static str {
        match self {
            Skip::Path(p) => p.as_str(),
            Skip::Ignored => "ignore-regex",
            Skip::TooLarge(_) => "too-large",
            Skip::Legacy => "legacy-notes-doc",
            Skip::Unwritable(_) => "unwritable-name",
            Skip::Collision(_) => "case-collision",
        }
    }
}

/// How a quiet wait ended.
enum Quiet {
    Settled,
    Interrupted,
}

/// Planned work for one file document whose content must be fetched.
struct Plan {
    doc: FileDoc,
    base: Option<FileRow>,
    local: Option<DiskState>,
}

fn random_node_id() -> String {
    use rand::Rng;
    const A: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut r = rand::thread_rng();
    (0..10).map(|_| A[r.gen_range(0..A.len())] as char).collect()
}

fn conflict_note(doc: &FileDoc) -> String {
    format!(
        "{} conflicting revision(s); resolve with an official client",
        doc.conflicts.len()
    )
}

pub(crate) fn seq_to_string(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

impl Engine {
    /// Open the index and take the locks on the state directory and the
    /// vault (`EngineError::Busy` when another process holds either).
    pub fn open(couch: Couch, cfg: EngineConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.vault).map_err(EngineError::other)?;
        let canon = std::fs::canonicalize(&cfg.vault).map_err(EngineError::other)?;
        let state_dir = cfg.index_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut locks = Vec::new();
        for (path, required) in [(state_dir.join(STATE_LOCK), true), (canon.join(VAULT_LOCK), false)] {
            match LockFile::try_acquire(&path) {
                Ok(l) => locks.push(l),
                Err(LockError::Busy(p)) => {
                    return Err(EngineError::Busy(format!(
                        "another lsync process is using this {} ({} is locked); stop it first",
                        if required { "state directory" } else { "vault" },
                        p.display()
                    )))
                }
                // The state-dir lock is the one that must work; a vault on a
                // filesystem without locking only loses the cross-profile check.
                Err(e) if required => return Err(EngineError::other(e)),
                Err(e) => warn!("{e}; continuing without the vault lock"),
            }
        }
        let index = Index::open(&cfg.index_path)?;
        let probed = FsRules::probe(&canon);
        let probe_ok = probed.is_ok();
        let mut fs = match probed {
            Ok(r) => r,
            Err(e) => {
                // Never fall back to permissive rules: on Android's shared
                // storage that would switch off the case-collision guard.
                let recorded: Option<FsRules> = index
                    .get_meta(meta::FS_RULES)?
                    .and_then(|s| serde_json::from_str(&s).ok());
                match recorded {
                    Some(r) => {
                        warn!("cannot probe the vault's filesystem ({e}); using the rules recorded at an earlier open");
                        r
                    }
                    None => {
                        warn!("cannot probe the vault's filesystem ({e}); assuming FAT-style rules (case-insensitive, restricted names)");
                        FsRules::fat()
                    }
                }
            }
        };
        match cfg.restrict_names {
            Some(true) => {
                fs.forbidden = PROBED_CHARS.to_vec();
                fs.control_chars_forbidden = true;
            }
            Some(false) => {
                fs.forbidden.clear();
                fs.control_chars_forbidden = false;
            }
            None => {}
        }
        if let Some(ci) = cfg.case_insensitive {
            fs.case_insensitive = ci;
            fs.normalization_insensitive = ci;
        }
        if index.get_meta(meta::NODE_ID)?.is_none() {
            index.set_meta(meta::NODE_ID, &random_node_id())?;
        }
        if probe_ok {
            index.set_meta(meta::FS_RULES, &serde_json::to_string(&fs).unwrap_or_default())?;
        }
        let ignore = match &cfg.ignore_regex {
            Some(s) => parse_ignore(s, false).map_err(|e| EngineError::Other(format!("invalid ignore regex: {e}")))?,
            None => Vec::new(),
        };
        let budget = cfg.memory_budget_mb.max(16);
        Ok(Engine {
            inner: Arc::new(Inner {
                couch,
                vault_canon: canon.to_string_lossy().into_owned(),
                state_dir,
                fs,
                _locks: locks,
                runtime: Mutex::new(Runtime {
                    max_size_bytes: cfg.max_size_mb.map(|m| m * 1024 * 1024),
                    ignore,
                    case_sensitive_ids: false,
                    split: SplitParams::default(),
                    preferred: Map::new(),
                }),
                cfg,
                index: Mutex::new(index),
                mem: Semaphore::new(budget),
                report: Mutex::new(PullReport::default()),
                nudge: Notify::new(),
                rescan: Notify::new(),
                ops: tokio::sync::Mutex::new(()),
                push_enabled: AtomicBool::new(false),
                fold_lock: Mutex::new(()),
                last_milestone: Mutex::new(None),
            }),
        })
    }

    /// The probed (or configured) filesystem rules.
    pub fn fs_rules(&self) -> &FsRules {
        &self.inner.fs
    }

    pub fn config(&self) -> &EngineConfig {
        &self.inner.cfg
    }

    /// Ask the two-way daemon for a full rescan of the vault (for example
    /// after the network came back, or from a "sync now" button).
    pub fn rescan_now(&self) {
        self.inner.rescan.notify_one();
    }

    /// The network came up or went down. When up: reconnect at once and
    /// rescan (spec: rescan when the network comes back).
    pub fn network_changed(&self, up: bool) {
        if up {
            self.nudge();
            self.rescan_now();
        }
    }

    pub fn node_id(&self) -> String {
        self.inner
            .index
            .lock()
            .unwrap()
            .get_meta(meta::NODE_ID)
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// Wake the daemon from a backoff sleep (e.g. the network came back).
    pub fn nudge(&self) {
        self.inner.nudge.notify_one();
    }

    pub fn report(&self) -> PullReport {
        self.inner.report.lock().unwrap().clone()
    }

    fn reset_report(&self) {
        *self.inner.report.lock().unwrap() = PullReport::default();
    }

    pub(crate) fn with_index<T>(&self, f: impl FnOnce(&Index) -> rusqlite::Result<T>) -> Result<T> {
        let ix = self.inner.index.lock().unwrap();
        Ok(f(&ix)?)
    }

    pub(crate) fn record_error(&self, e: &EngineError) {
        let _ = self.with_index(|ix| ix.set_meta(meta::LAST_ERROR, &format!("{} {e}", vaultfs::now_ms())));
    }

    // ------------------------------------------------------------------
    // Connect gate
    // ------------------------------------------------------------------

    /// Run the connect gate. Never writes to the remote. On first contact the
    /// PREFERRED tweaks are adopted; later they must stay the same unless
    /// `adopt_tweaks` is set. A changed milestone `created` resets the index.
    pub async fn connect_gate(&self, adopt_tweaks: bool) -> Result<GateReport> {
        let r = self.connect_gate_inner(adopt_tweaks).await;
        let summary = match &r {
            Ok(g) => format!("ok: version {} created {} locked {}", g.version, g.created, g.locked),
            Err(e) => format!("failed: {e}"),
        };
        let now = vaultfs::now_ms();
        let _ = self.with_index(|ix| {
            ix.set_meta(meta::LAST_GATE, &format!("{now} {summary}"))?;
            if r.is_ok() {
                ix.set_meta(meta::LAST_GATE_OK, &now.to_string())?;
                // A passed gate supersedes any earlier failure.
                ix.delete_meta(meta::LAST_ERROR)?;
            }
            Ok(())
        });
        if let Err(e) = &r {
            self.record_error(e);
        }
        r
    }

    /// Refuse when the index was made for another vault folder.
    fn check_vault(&self, ix: &Index) -> Result<()> {
        match ix.get_meta(meta::VAULT)? {
            Some(v) if v != self.inner.vault_canon => Err(Refusal::ForeignVault {
                index_vault: v,
                vault: self.inner.vault_canon.clone(),
            }
            .into()),
            _ => Ok(()),
        }
    }

    async fn connect_gate_inner(&self, adopt_tweaks: bool) -> Result<GateReport> {
        self.check_vault(&self.inner.index.lock().unwrap())?;
        let couch = &self.inner.couch;
        let root = match couch.server_root().await {
            Ok(v) => v,
            // A web page or a proxy's 404 instead of CouchDB's welcome JSON.
            Err(RemoteError::Decode { .. }) => return Err(Refusal::NotCouchDb.into()),
            Err(RemoteError::Http { status: 404 | 405, .. }) => return Err(Refusal::NotCouchDb.into()),
            Err(e) => return Err(e.into()),
        };
        if root.get("couchdb").and_then(Value::as_str) != Some("Welcome") {
            return Err(Refusal::NotCouchDb.into());
        }
        let info = couch.db_info().await?;
        let version_doc = couch.get_doc(VERSION_DOC_ID, false).await?;
        let version = gate::assess_version_doc(version_doc.as_ref(), info.doc_count > 0)?;
        let milestone = couch
            .get_doc(&format!("_local/{MILESTONE_LOCAL_ID}"), false)
            .await?
            .ok_or(Refusal::MissingMilestone)?;
        *self.inner.last_milestone.lock().unwrap() = Some(milestone.clone());
        let milestone = Milestone::parse(&milestone)?;
        let node_id = self.node_id();
        let (fresh, rebuilt, owed) = {
            let ix = self.inner.index.lock().unwrap();
            let stored = ix.get_meta(meta::CREATED)?.and_then(|s| s.parse::<i64>().ok());
            // A lingering `resolve_owed` only counts for the database
            // (`created`) whose lock it was set for.
            let resolve_owed = ix.get_meta(meta::RESOLVE_OWED)?.as_deref() == Some(&milestone.created.to_string());
            (
                stored.is_none(),
                stored.is_some_and(|c| c != milestone.created),
                ix.get_meta(meta::NEEDS_FETCH)?.is_some() || resolve_owed,
            )
        };
        // A lock (after a rebuild elsewhere) is upstream's "fetch again":
        // a client that is about to do a full fetch anyway may proceed, and
        // is marked resolved once the fetch is complete.
        let mut fetch_to_resolve = false;
        let mut lets_us_in = false;
        match gate::assess_milestone(&milestone, &node_id) {
            Ok(()) => lets_us_in = true,
            Err(Refusal::Locked | Refusal::Cleaned) if fresh || rebuilt || owed => fetch_to_resolve = true,
            Err(e) => return Err(e.into()),
        }
        let preferred = milestone.preferred().ok_or(Refusal::MissingPreferredTweaks)?.clone();
        gate::assess_supported_tweaks(&preferred)?;
        let params = couch.get_doc(&format!("_local/{SYNC_PARAMS_LOCAL_ID}"), false).await?;
        let salt = params
            .as_ref()
            .and_then(|p| p.get("pbkdf2salt"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let label = couch.label().to_string();
        let mut adopted_now = false;
        let mut rebuild = false;
        {
            // No push pass or batch runs while the index may be reset.
            let _ops = self.inner.ops.lock().await;
            let ix = self.inner.index.lock().unwrap();
            self.check_vault(&ix)?;
            if ix.get_meta(meta::VAULT)?.is_none() {
                ix.set_meta(meta::VAULT, &self.inner.vault_canon)?;
            }
            match ix.get_meta(meta::REMOTE)? {
                Some(r) if r != label => return Err(Refusal::ForeignIndex(r).into()),
                Some(_) => {}
                None => ix.set_meta(meta::REMOTE, &label)?,
            }
            let adopted: Option<Map<String, Value>> =
                ix.get_meta(meta::TWEAKS)?.and_then(|s| serde_json::from_str(&s).ok());
            match adopted {
                Some(a) => {
                    let diff = gate::compare_tweaks(&a, &preferred);
                    if !diff.is_empty() {
                        if adopt_tweaks {
                            warn!("re-adopting changed PREFERRED tweaks: {}", diff.join("; "));
                            ix.set_meta(meta::TWEAKS, &serde_json::to_string(&preferred).unwrap())?;
                            adopted_now = true;
                        } else {
                            return Err(Refusal::TweakMismatch(diff.join("; ")).into());
                        }
                    }
                }
                None => {
                    ix.set_meta(meta::TWEAKS, &serde_json::to_string(&preferred).unwrap())?;
                    adopted_now = true;
                }
            }
            match ix.get_meta(meta::CREATED)?.and_then(|s| s.parse::<i64>().ok()) {
                Some(c) if c != milestone.created => {
                    warn!(
                        "remote milestone `created` changed ({c} -> {}): the remote was rebuilt elsewhere; resetting the index and refetching",
                        milestone.created
                    );
                    ix.reset_for_rebuild()?;
                    let now = vaultfs::now_ms();
                    ix.set_meta(meta::REBUILD_AT, &now.to_string())?;
                    ix.set_meta(
                        meta::REBUILD_WAIT,
                        &format!("{now} owed: the refetch waits for the rebuilt remote to stop changing before anything is pushed"),
                    )?;
                    rebuild = true;
                }
                _ => {}
            }
            if fresh || rebuild {
                ix.set_meta(meta::NEEDS_FETCH, &vaultfs::now_ms().to_string())?;
                ix.set_meta(meta::RECONCILE, &vaultfs::now_ms().to_string())?;
            }
            if fetch_to_resolve {
                ix.set_meta(meta::RESOLVE_OWED, &milestone.created.to_string())?;
            } else if lets_us_in {
                // Nothing left to resolve (unlocked, or we are accepted).
                ix.delete_meta(meta::RESOLVE_OWED)?;
            }
            ix.set_meta(meta::CREATED, &milestone.created.to_string())?;
            ix.set_meta(meta::VERSION, &version.to_string())?;
            match &salt {
                Some(s) => ix.set_meta(meta::SALT, s)?,
                None => ix.delete_meta(meta::SALT)?,
            }
        }

        let case_sensitive = preferred
            .get("handleFilenameCaseSensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let max_mb = self
            .inner
            .cfg
            .max_size_mb
            .unwrap_or_else(|| gate::max_size_mb(&preferred));
        let needs_full_fetch = self.with_index(|ix| ix.get_meta(meta::NEEDS_FETCH))?.is_some();
        {
            let mut rt = self.inner.runtime.lock().unwrap();
            rt.case_sensitive_ids = case_sensitive;
            rt.split = SplitParams::from_tweaks(&preferred);
            rt.preferred = preferred.clone();
            rt.max_size_bytes = if max_mb == 0 { None } else { Some(max_mb * 1024 * 1024) };
            if let Some(src) = &self.inner.cfg.ignore_regex {
                rt.ignore = parse_ignore(src, case_sensitive)
                    .map_err(|e| EngineError::Other(format!("invalid ignore regex: {e}")))?;
            }
        }
        Ok(GateReport {
            remote: label,
            node_id,
            version,
            doc_count: info.doc_count,
            created: milestone.created,
            locked: milestone.locked,
            accepted_nodes: milestone.accepted_nodes.len(),
            adopted_tweaks_now: adopted_now,
            rebuild_detected: rebuild,
            salt_present: salt.is_some(),
            max_size_mb: max_mb,
            case_sensitive,
            fetch_to_resolve,
            needs_full_fetch,
        })
    }

    /// Register this node in the milestone (`node_info`, `tweak_values`,
    /// `node_chunk_info`), and with `mark_resolved` add it to
    /// `accepted_nodes`. Writes only when something changed. Returns whether
    /// it wrote.
    pub(crate) async fn register_node(&self, mark_resolved: bool) -> Result<bool> {
        let id = format!("_local/{MILESTONE_LOCAL_ID}");
        let device = self.device_info();
        let node = self.node_id();
        let tweaks = self.inner.runtime.lock().unwrap().preferred.clone();
        for _ in 0..3 {
            let current = self
                .inner
                .couch
                .get_doc(&id, false)
                .await?
                .ok_or(Refusal::MissingMilestone)?;
            // Never register (let alone mark resolved) against a database
            // other than the one this index was fetched from: it may have
            // been rebuilt again since the gate ran.
            let stored = self.with_index(|ix| ix.get_meta(meta::CREATED))?;
            let created = current.get("created").and_then(Value::as_i64).map(|c| c.to_string());
            if stored.is_some() && created != stored {
                return Err(EngineError::Other(format!(
                    "the remote milestone changed (created {} -> {}); checking the remote again before registering",
                    stored.unwrap_or_default(),
                    created.unwrap_or_default()
                )));
            }
            let Some(updated) = register_node(&current, &node, &device, &tweaks, vaultfs::now_ms(), mark_resolved)
            else {
                return Ok(false);
            };
            match self.inner.couch.put_doc(&updated).await? {
                PutOutcome::Saved { .. } => {
                    self.with_index(|ix| ix.set_meta(meta::REGISTERED, &vaultfs::now_ms().to_string()))?;
                    info!(
                        "registered node {node} in the milestone{}",
                        if mark_resolved { " (marked resolved)" } else { "" }
                    );
                    return Ok(true);
                }
                PutOutcome::Conflict => continue, // someone else wrote it; re-read
            }
        }
        Err(EngineError::Other(
            "milestone kept changing while registering this node".into(),
        ))
    }

    fn device_info(&self) -> DeviceInfo {
        let vault_name = Path::new(&self.inner.vault_canon)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        DeviceInfo {
            device_name: self
                .inner
                .cfg
                .device_name
                .clone()
                .unwrap_or_else(|| format!("lsync-{}", crate::platform::hostname())),
            vault_name,
            app_version: "lsync".into(),
            plugin_version: env!("CARGO_PKG_VERSION").into(),
            platform: Some(crate::platform::platform_name().into()),
        }
    }

    // ------------------------------------------------------------------
    // Pull
    // ------------------------------------------------------------------

    fn since(&self) -> Result<Option<Value>> {
        Ok(self
            .with_index(|ix| ix.get_meta(meta::SINCE))?
            .and_then(|s| serde_json::from_str(&s).ok()))
    }

    /// One-shot pull: gate, then page through `_changes` until caught up,
    /// then retry pending documents once.
    pub async fn pull_once(&self) -> Result<PullReport> {
        let start = Instant::now();
        self.reset_report();
        let gate = self.connect_gate(false).await?;
        info!(
            "gate ok: {} (version {}, {} docs)",
            gate.remote, gate.version, gate.doc_count
        );
        let old_scope = self.apply_scope_narrowing()?;
        self.prepare_vault();
        self.catch_up(start).await?;
        self.finish_read_only_fetch()?;
        self.scope_catch_up(&old_scope).await?;
        self.retry_pending(true).await?;
        self.with_index(|ix| ix.set_meta(meta::LAST_PULL, &vaultfs::now_ms().to_string()))?;
        let mut rep = self.report();
        rep.elapsed_ms = start.elapsed().as_millis() as u64;
        rep.pending = self.with_index(|ix| ix.pending(None))?.len() as u64;
        Ok(rep)
    }

    /// A read-only pull caught up. It completes an owed full fetch only when
    /// nothing else is owed with it: after a rebuild the quiet wait, and
    /// after a lock was let through the resolution, belong to the two-way
    /// side, so the flag stays set for it (and pushes stay held).
    fn finish_read_only_fetch(&self) -> Result<()> {
        self.with_index(|ix| {
            if ix.get_meta(meta::NEEDS_FETCH)?.is_some()
                && ix.get_meta(meta::REBUILD_WAIT)?.is_none()
                && ix.get_meta(meta::RESOLVE_OWED)?.is_none()
            {
                ix.delete_meta(meta::NEEDS_FETCH)?;
            }
            Ok(())
        })
    }

    /// Page through `_changes` (no longpoll) until caught up, persisting
    /// `since` after each fully applied page.
    pub(crate) async fn catch_up(&self, start: Instant) -> Result<()> {
        loop {
            let before = self.report();
            let since = self.since()?;
            let resp = self
                .inner
                .couch
                .changes(&ChangesQuery {
                    since,
                    limit: self.inner.cfg.batch_limit,
                    longpoll: false,
                    heartbeat_ms: 0,
                })
                .await?;
            let n = resp.results.len();
            self.apply_batch(resp.results).await?;
            self.with_index(|ix| ix.set_meta(meta::SINCE, &seq_to_string(&resp.last_seq)))?;
            if n > 0 {
                self.log_progress(start, &before, resp.pending);
            }
            if n == 0 || resp.pending == Some(0) {
                return Ok(());
            }
        }
    }

    /// After a passed gate: remove temp files left by an interrupted run (safe,
    /// since this process holds the vault) and make sure `.nomedia` exists.
    pub(crate) fn prepare_vault(&self) {
        let vault = &self.inner.cfg.vault;
        let swept = vaultfs::sweep_temp_files(vault);
        if swept > 0 {
            info!("removed {swept} stale temp files");
        }
        match vaultfs::ensure_nomedia(vault) {
            Ok(true) => info!("created .nomedia at the vault root"),
            Ok(false) => {}
            Err(e) => warn!("cannot create .nomedia at the vault root: {e}"),
        }
    }

    /// One line per applied batch: what this batch did, then running totals.
    fn log_progress(&self, start: Instant, before: &PullReport, pending: Option<u64>) {
        let total = self.report();
        let d = total.delta(before);
        info!(
            "batch: {} changes, {} written, {} adopted, {} unchanged, {} deleted, {} dirty, {} chunks ({:.1} MB); remaining {}; totals since {:.0}s ago: {} batches, {} changes, {} written",
            d.changes,
            d.written,
            d.adopted,
            d.unchanged,
            d.deleted,
            d.dirty,
            d.chunks_fetched,
            d.chunk_bytes as f64 / 1e6,
            pending.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
            start.elapsed().as_secs_f64(),
            total.batches,
            total.changes,
            total.written
        );
    }

    /// Apply one page of changes. The caller persists `since` afterwards.
    pub(crate) async fn apply_batch(&self, rows: Vec<ChangeRow>) -> Result<()> {
        let _ops = self.inner.ops.lock().await;
        {
            let mut r = self.inner.report.lock().unwrap();
            r.batches += 1;
            r.changes += rows.len() as u64;
        }
        // Last occurrence of each document wins.
        let mut order: Vec<String> = Vec::new();
        let mut by_id: HashMap<String, Value> = HashMap::new();
        for row in rows {
            let doc = match row.doc {
                Some(d) => d,
                None if row.deleted => {
                    serde_json::json!({"_id": row.id, "_rev": row.changes.first().map(|c| c.rev.clone()), "_deleted": true})
                }
                None => continue,
            };
            if !by_id.contains_key(&row.id) {
                order.push(row.id.clone());
            }
            by_id.insert(row.id, doc);
        }

        let mut writes: Vec<FileDoc> = Vec::new();
        let mut deletes: Vec<(String, Option<String>)> = Vec::new();
        // Deleted winners that still have live conflicting revisions.
        let mut conflicted_deletes: Vec<FileDoc> = Vec::new();
        let mut skips: Vec<(FileDoc, Skip)> = Vec::new();
        let mut ignored = 0u64;
        // Pre-scan: refusals stop the batch before anything is applied.
        for id in &order {
            let doc = &by_id[id];
            match classify_doc(doc) {
                Classified::VersionDoc(v) => {
                    gate::assess_version_doc(Some(&v), true)?;
                    ignored += 1;
                }
                Classified::Encrypted { id } => return Err(Refusal::EncryptedContent(format!("document {id}")).into()),
                Classified::Tombstone { id, .. } => {
                    if id.starts_with("h:") {
                        ignored += 1;
                    } else {
                        let rev = doc.get("_rev").and_then(Value::as_str).map(str::to_string);
                        deletes.push((id, rev));
                    }
                }
                Classified::LegacyNote { id, path } => {
                    let path = path.unwrap_or_else(|| id.clone());
                    if is_internal_id(&id) || classify_path(&path).is_some() || !self.inner.cfg.scope.contains(&path) {
                        ignored += 1;
                    } else {
                        let f = FileDoc {
                            id,
                            rev: doc.get("_rev").and_then(Value::as_str).unwrap_or_default().to_string(),
                            path,
                            kind: FileKind::Plain,
                            children: vec![],
                            mtime: 0,
                            ctime: 0,
                            size: 0,
                            deleted: false,
                            conflicts: vec![],
                            eden: Default::default(),
                        };
                        skips.push((f, Skip::Legacy));
                    }
                }
                Classified::Other { .. } => ignored += 1,
                Classified::File(f) => {
                    if let Some(c) = f.children.iter().find(|c| c.starts_with("h:+")) {
                        return Err(Refusal::EncryptedContent(format!("chunk {c} of {}", f.path)).into());
                    }
                    if is_internal_id(&f.id) {
                        ignored += 1;
                    } else if classify_path(&f.path).is_none() && !self.inner.cfg.scope.contains(&f.path) {
                        // Outside the folder scope: invisible. No row, and a
                        // remote deletion never touches the local file.
                        *self
                            .inner
                            .report
                            .lock()
                            .unwrap()
                            .skipped
                            .entry("out-of-scope".into())
                            .or_default() += 1;
                    } else if f.deleted && !f.conflicts.is_empty() {
                        conflicted_deletes.push(f);
                    } else if f.deleted {
                        deletes.push((f.id, Some(f.rev)));
                    } else if let Some(s) = self.skip_reason(&f) {
                        skips.push((f, s));
                    } else {
                        writes.push(f);
                    }
                }
            }
        }
        self.inner.report.lock().unwrap().ignored_docs += ignored;

        // Deletions and skips are quick, local-only work.
        let this = self.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            for (id, rev) in deletes {
                this.apply_delete(&id, rev.as_deref())?;
            }
            for f in conflicted_deletes {
                this.apply_conflicted_delete(&f)?;
            }
            for (f, s) in skips {
                this.apply_skip(&f, &s)?;
            }
            Ok(())
        })
        .await
        .map_err(EngineError::other)??;

        // Writes: grouped into units so chunk fetches are batched, with a
        // bounded number of units in flight.
        let units = self.group_units(writes);
        let results: Vec<Result<()>> = stream::iter(units)
            .map(|u| {
                let this = self.clone();
                async move { this.process_unit(u).await }
            })
            .buffer_unordered(self.inner.cfg.concurrency.max(1))
            .collect()
            .await;
        for r in results {
            r?;
        }
        Ok(())
    }

    fn skip_reason(&self, f: &FileDoc) -> Option<Skip> {
        if let Some(p) = classify_path(&f.path) {
            return Some(Skip::Path(p));
        }
        if let Some(why) = self.inner.fs.unwritable_reason(&f.path) {
            return Some(Skip::Unwritable(why));
        }
        // Rejected by the filesystem before: do not fetch it again.
        if let Ok(Some(b)) = self.with_index(|ix| ix.by_doc(&f.id)) {
            if b.state == State::Skipped && b.path == f.path {
                if let Some(n) = b.note.as_deref().filter(|n| n.starts_with(UNWRITABLE_NOTE)) {
                    let detail = n.strip_prefix(UNWRITABLE_NOTE).unwrap_or_default();
                    return Some(Skip::Unwritable(detail.trim_start_matches([':', ' ']).to_string()));
                }
            }
        }
        let rt = self.inner.runtime.lock().unwrap();
        if rt.ignored(&f.path) {
            return Some(Skip::Ignored);
        }
        if let Some(max) = rt.max_size_bytes {
            if f.size > max {
                return Some(Skip::TooLarge(f.size));
            }
        }
        None
    }

    fn apply_skip(&self, f: &FileDoc, s: &Skip) -> Result<()> {
        *self
            .inner
            .report
            .lock()
            .unwrap()
            .skipped
            .entry(s.label().to_string())
            .or_default() += 1;
        let note = match s {
            Skip::TooLarge(n) => format!("too large ({n} bytes)"),
            Skip::Unwritable(why) => {
                warn!(
                    "{}: skipped, the vault's filesystem cannot hold this name ({why})",
                    f.path
                );
                format!("{UNWRITABLE_NOTE}: {why}")
            }
            Skip::Collision(other) => {
                warn!(
                    "{}: skipped, it would land on the same file as {other} on this filesystem",
                    f.path
                );
                format!("{COLLISION_NOTE} {other}")
            }
            other => other.label().to_string(),
        };
        self.with_index(|ix| {
            let row = match ix.by_doc(&f.id)? {
                // Keep what we know about the file on disk; just stop syncing it.
                Some(mut b) if b.path == f.path => {
                    b.state = State::Skipped;
                    b.note = Some(note);
                    b
                }
                _ => FileRow {
                    path: f.path.clone(),
                    doc_id: f.id.clone(),
                    base_rev: None,
                    base_children: vec![],
                    disk_mtime: None,
                    disk_size: None,
                    disk_hash: None,
                    state: State::Skipped,
                    note: Some(note),
                },
            };
            ix.upsert(&row)?;
            ix.remove_pending(&f.id)
        })
    }

    fn apply_delete(&self, id: &str, rev: Option<&str>) -> Result<()> {
        let base = self.with_index(|ix| ix.by_doc(id))?;
        let Some(mut b) = base else {
            return self.with_index(|ix| ix.remove_pending(id));
        };
        if matches!(b.state, State::Skipped | State::OutOfScope) {
            // No file of ours at this path (or the user moved it away).
            return self.with_index(|ix| {
                ix.delete_doc(id)?;
                ix.remove_pending(id)
            });
        }
        if b.state == State::Conflicted {
            // Our revision was a conflicting branch, and the document now
            // ends in a deletion. That may be an automatic resolution
            // (upstream keeps the newer mtime, and a deletion's mtime is
            // "now"), so the local copy is the only place our edit is
            // easy to find: keep it and say so.
            warn!(
                "{}: deleted on another device while this device's version was in conflict; the local copy is kept (delete it here to accept the deletion)",
                b.path
            );
            self.inner.report.lock().unwrap().dirty += 1;
            b.state = State::Dirty;
            b.note = Some(
                "deleted on another device while this device's version was in conflict; local copy kept (delete it here to accept the deletion)".into(),
            );
            // No hash to match any more: a replayed deletion (a full fetch,
            // a rebuild) or a remote change sees a diverged file and keeps it.
            b.disk_hash = None;
            return self.with_index(|ix| {
                ix.upsert(&b)?;
                ix.remove_pending(id)
            });
        }
        let target = self.inner.cfg.vault.join(&b.path);
        let outcome = match &b.disk_hash {
            Some(h) => vaultfs::remove_if_matches(&self.inner.cfg.vault, &target, h),
            None => vaultfs::read_state(&target).map(|st| match st {
                None => RemoveOutcome::AlreadyGone,
                Some(s) => RemoveOutcome::Diverged { found: s.hash },
            }),
        };
        let outcome = match outcome {
            Ok(o) => o,
            // This file only: keep it and its row, retry the deletion later.
            Err(e) => return self.defer(id, rev, &b.path, &format!("deletion not applied: {e}")),
        };
        match outcome {
            RemoveOutcome::Removed | RemoveOutcome::AlreadyGone => {
                self.inner.report.lock().unwrap().deleted += 1;
                debug!("deleted {}", b.path);
                self.with_index(|ix| {
                    ix.delete_doc(id)?;
                    ix.remove_pending(id)
                })?;
                self.requeue_collisions(&b.path)
            }
            RemoveOutcome::Raced { displaced } => {
                // An edit landed while we removed the file: put it back.
                warn!("{}: edited while being deleted; the edit is kept", b.path);
                self.inner.report.lock().unwrap().restored_edits += 1;
                self.put_back(&b.path, displaced, None);
                b.state = State::Dirty;
                b.note = Some("deleted remotely; a local edit made meanwhile was kept".into());
                self.with_index(|ix| {
                    ix.upsert(&b)?;
                    ix.remove_pending(id)
                })
            }
            RemoveOutcome::Diverged { .. } => {
                warn!(
                    "{}: deleted remotely, but the local file has changes; keeping it",
                    b.path
                );
                self.inner.report.lock().unwrap().dirty += 1;
                b.state = State::Dirty;
                b.note = Some("deleted remotely; local changes kept".into());
                self.with_index(|ix| {
                    ix.upsert(&b)?;
                    ix.remove_pending(id)
                })
            }
        }
    }

    /// A deleted winner that still has live conflicting revisions (D4:
    /// a document with `_conflicts` leaves the disk alone). Typically our
    /// own edit, pushed as a branch, against a deletion on another device:
    /// the file is kept and reported as conflicted, never removed.
    fn apply_conflicted_delete(&self, f: &FileDoc) -> Result<()> {
        let Some(mut b) = self.with_index(|ix| ix.by_doc(&f.id))? else {
            // Nothing of ours on disk; the conflict is resolved elsewhere.
            return self.with_index(|ix| ix.remove_pending(&f.id));
        };
        match b.state {
            State::Skipped | State::OutOfScope => {
                return self.with_index(|ix| {
                    ix.delete_doc(&f.id)?;
                    ix.remove_pending(&f.id)
                })
            }
            // An unpushed local edit: the push side writes it as a branch.
            State::Dirty => return self.with_index(|ix| ix.remove_pending(&f.id)),
            State::Clean | State::Conflicted => {}
        }
        let ours_is_leaf = b.base_rev.as_ref().is_some_and(|r| f.conflicts.contains(r));
        let note = if ours_is_leaf {
            format!(
                "deleted on another device; this device's version is kept as a conflicting revision ({} in conflict); resolve with an official client",
                f.conflicts.len()
            )
        } else {
            format!(
                "deleted on another device, with {}; the local file is kept",
                conflict_note(f)
            )
        };
        if b.state != State::Conflicted || b.note.as_deref() != Some(note.as_str()) {
            warn!("{}: {note}", b.path);
        }
        self.inner.report.lock().unwrap().conflicted += 1;
        b.state = State::Conflicted;
        b.note = Some(note);
        self.with_index(|ix| {
            ix.upsert(&b)?;
            ix.remove_pending(&f.id)
        })
    }

    fn group_units(&self, docs: Vec<FileDoc>) -> Vec<Vec<FileDoc>> {
        const MAX_CHUNKS: usize = 100;
        const MAX_BYTES: u64 = 4 * 1024 * 1024;
        let mut units = Vec::new();
        let mut cur: Vec<FileDoc> = Vec::new();
        let (mut chunks, mut bytes) = (0usize, 0u64);
        for d in docs {
            let (c, b) = (d.children.len(), d.size);
            if !cur.is_empty() && (chunks + c > MAX_CHUNKS || bytes + b > MAX_BYTES) {
                units.push(std::mem::take(&mut cur));
                chunks = 0;
                bytes = 0;
            }
            chunks += c;
            bytes += b;
            cur.push(d);
        }
        if !cur.is_empty() {
            units.push(cur);
        }
        units
    }

    async fn process_unit(&self, docs: Vec<FileDoc>) -> Result<()> {
        // Plan (local I/O): skip unchanged revisions, hash local files.
        let this = self.clone();
        let plans = tokio::task::spawn_blocking(move || this.plan_unit(docs))
            .await
            .map_err(EngineError::other)??;
        if plans.is_empty() {
            return Ok(());
        }
        // Memory budget: roughly 3x the base64-inflated content size.
        let est: u64 = plans.iter().map(|p| p.doc.size).sum::<u64>() * 4 / 3;
        let permits = ((est * 3) / (1024 * 1024) + 1).min(self.inner.cfg.memory_budget_mb.max(16) as u64) as u32;
        let _permit = self.inner.mem.acquire_many(permits).await.map_err(EngineError::other)?;

        // Fetch every chunk the unit needs (eden entries first).
        let mut wanted: Vec<(String, u64)> = Vec::new();
        let mut seen = HashSet::new();
        for p in &plans {
            let per_chunk = if p.doc.children.is_empty() {
                0
            } else {
                p.doc.size * 4 / 3 / p.doc.children.len() as u64 + 64
            };
            for c in &p.doc.children {
                if !p.doc.eden.contains_key(c) && seen.insert(c.clone()) {
                    wanted.push((c.clone(), per_chunk));
                }
            }
        }
        let chunks = self.fetch_chunks(wanted).await?;

        // Assemble, then write (blocking I/O).
        let mut ready: Vec<(Plan, std::result::Result<Vec<u8>, String>)> = Vec::new();
        for p in plans {
            let content = self.assemble(&p.doc, &chunks);
            ready.push((p, content));
        }
        drop(chunks);
        let this = self.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            for (p, content) in ready {
                match content {
                    Ok(bytes) => this.materialise(p, bytes)?,
                    Err(reason) => this.defer_doc(&p.doc, &reason)?,
                }
            }
            Ok(())
        })
        .await
        .map_err(EngineError::other)??;
        Ok(())
    }

    fn plan_unit(&self, docs: Vec<FileDoc>) -> Result<Vec<Plan>> {
        let mut plans = Vec::new();
        for doc in docs {
            let base = self.with_index(|ix| ix.by_doc(&doc.id))?;
            if base.as_ref().is_some_and(|b| b.state == State::OutOfScope) {
                // Moved out of scope locally: the remote copy is not brought back.
                self.inner.report.lock().unwrap().unchanged += 1;
                self.with_index(|ix| ix.remove_pending(&doc.id))?;
                continue;
            }
            if let Some(b) = &base {
                if b.path == doc.path && b.base_rev.as_deref() == Some(doc.rev.as_str()) {
                    // Same revision as on disk.
                    let mut b = b.clone();
                    let mut changed = false;
                    if b.state == State::Conflicted && doc.conflicts.is_empty() {
                        b.state = State::Clean;
                        b.note = None;
                        changed = true;
                    }
                    // Another device branched off, and our revision won:
                    // the file is right, but the conflict must be reported.
                    if b.state == State::Clean && !doc.conflicts.is_empty() {
                        b.state = State::Conflicted;
                        b.note = Some(conflict_note(&doc));
                        changed = true;
                        self.inner.report.lock().unwrap().conflicted += 1;
                    }
                    if b.state == State::Dirty {
                        let local = match vaultfs::read_state(&self.inner.cfg.vault.join(&b.path)) {
                            Ok(l) => l,
                            Err(e) => {
                                // Cannot tell whether the edit was reverted: stays dirty.
                                warn!("{}: cannot read the local file ({e}); it stays dirty", b.path);
                                Some(DiskState {
                                    hash: format!("unreadable: {e}"),
                                    size: 0,
                                    mtime_ms: 0,
                                    file_id: None,
                                })
                            }
                        };
                        if local.as_ref().map(|l| &l.hash) == b.disk_hash.as_ref() {
                            b.state = if doc.conflicts.is_empty() {
                                State::Clean
                            } else {
                                State::Conflicted
                            };
                            b.note = None;
                            changed = true;
                        }
                    }
                    if changed {
                        self.with_index(|ix| ix.upsert(&b))?;
                    }
                    self.with_index(|ix| ix.remove_pending(&doc.id))?;
                    self.inner.report.lock().unwrap().unchanged += 1;
                    continue;
                }
            }
            let local = match vaultfs::read_state(&self.inner.cfg.vault.join(&doc.path)) {
                Ok(l) => l,
                Err(e) => {
                    // This document only; the rest of the batch goes on.
                    self.io_failure(&doc, &e)?;
                    continue;
                }
            };
            // D4: the document has conflicts and our revision is one of its
            // live leaves (we pushed a branch). Keep our content on disk.
            if let Some(b) = &base {
                let ours_is_leaf = b
                    .base_rev
                    .as_ref()
                    .is_some_and(|r| *r == doc.rev || doc.conflicts.contains(r));
                let clean = local.as_ref().map(|l| &l.hash) == b.disk_hash.as_ref();
                if !doc.conflicts.is_empty() && ours_is_leaf && b.path == doc.path && clean {
                    let mut b = b.clone();
                    b.state = State::Conflicted;
                    b.note = Some(conflict_note(&doc));
                    self.with_index(|ix| {
                        ix.upsert(&b)?;
                        ix.remove_pending(&doc.id)
                    })?;
                    let mut r = self.inner.report.lock().unwrap();
                    r.conflicted += 1;
                    r.unchanged += 1;
                    continue;
                }
            }
            plans.push(Plan { doc, base, local });
        }
        Ok(plans)
    }

    /// Fetch chunks with `POST _all_docs {keys}`: at most 100 keys and about
    /// 16 MB of expected data per request. Missing chunks are left out.
    async fn fetch_chunks(&self, wanted: Vec<(String, u64)>) -> Result<HashMap<String, String>> {
        const MAX_KEYS: usize = 100;
        const MAX_BYTES: u64 = 16 * 1024 * 1024;
        let mut out = HashMap::with_capacity(wanted.len());
        let mut groups: Vec<Vec<String>> = Vec::new();
        let (mut cur, mut bytes) = (Vec::new(), 0u64);
        for (id, est) in wanted {
            if !cur.is_empty() && (cur.len() >= MAX_KEYS || bytes + est > MAX_BYTES) {
                groups.push(std::mem::take(&mut cur));
                bytes = 0;
            }
            bytes += est;
            cur.push(id);
        }
        if !cur.is_empty() {
            groups.push(cur);
        }
        for keys in groups {
            let rows = self.fetch_with_retry(&keys).await?;
            let mut fetched = 0u64;
            let mut nbytes = 0u64;
            for row in rows {
                let Some(doc) = row.doc else { continue };
                let chunk: livesync_model::ChunkDoc = match serde_json::from_value(doc) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("malformed chunk {:?}: {e}", row.key);
                        continue;
                    }
                };
                if chunk.encrypted == Some(true) || chunk.id.starts_with("h:+") {
                    return Err(Refusal::EncryptedContent(format!("chunk {}", chunk.id)).into());
                }
                let data = match codec::decompress(&chunk.data) {
                    Ok(d) => d.into_owned(),
                    Err(e) => {
                        warn!("chunk {}: {e}", chunk.id);
                        continue;
                    }
                };
                fetched += 1;
                nbytes += data.len() as u64;
                out.insert(chunk.id, data);
            }
            let mut r = self.inner.report.lock().unwrap();
            r.chunks_fetched += fetched;
            r.chunk_bytes += nbytes;
        }
        Ok(out)
    }

    async fn fetch_with_retry(&self, keys: &[String]) -> Result<Vec<livesync_remote::AllDocsRow>> {
        let mut backoff = Backoff::new(Duration::from_millis(500), Duration::from_secs(10));
        loop {
            match self.inner.couch.all_docs_by_keys(keys).await {
                Ok(rows) => return Ok(rows),
                Err(e) if e.is_transient() && backoff.attempt() < 4 => {
                    let d = backoff.next_delay();
                    warn!("chunk fetch failed ({e}); retrying in {d:?}");
                    tokio::time::sleep(d).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Concatenate a document's chunks. `Err` carries a reason to defer.
    fn assemble(&self, doc: &FileDoc, chunks: &HashMap<String, String>) -> std::result::Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(doc.size as usize);
        let mut missing = 0usize;
        let mut id_mismatch = 0u64;
        for c in &doc.children {
            let piece: std::borrow::Cow<'_, str> = match (doc.eden.get(c), chunks.get(c)) {
                (Some(e), _) => codec::decompress(e).map_err(|e| format!("eden chunk {c}: {e}"))?,
                (None, Some(d)) => std::borrow::Cow::Borrowed(d.as_str()),
                (None, None) => {
                    missing += 1;
                    continue;
                }
            };
            if chunk_id_xxhash64(&piece) != *c {
                id_mismatch += 1;
            }
            match doc.kind {
                FileKind::Plain => out.extend_from_slice(piece.as_bytes()),
                FileKind::Binary => match codec::decode_binary_piece(&piece) {
                    Ok(b) => out.extend_from_slice(&b),
                    Err(CodecError::LegacyPercentEncoding) => return Err("legacy %-encoded binary chunk".into()),
                    Err(e) => return Err(format!("chunk {c}: {e}")),
                },
            }
        }
        if missing > 0 {
            return Err(format!(
                "{missing} of {} chunks missing on the remote",
                doc.children.len()
            ));
        }
        let mut r = self.inner.report.lock().unwrap();
        r.chunk_id_mismatches += id_mismatch;
        if out.len() as u64 != doc.size {
            r.size_mismatches += 1;
            debug!(
                "{}: size field {} but content is {} bytes",
                doc.path,
                doc.size,
                out.len()
            );
        }
        Ok(out)
    }

    fn defer(&self, id: &str, rev: Option<&str>, path: &str, reason: &str) -> Result<()> {
        warn!("{path}: deferred ({reason})");
        self.inner.report.lock().unwrap().pending += 1;
        self.with_index(|ix| ix.add_pending(id, rev, reason, vaultfs::now_ms()))
    }

    fn defer_doc(&self, doc: &FileDoc, reason: &str) -> Result<()> {
        self.defer(&doc.id, Some(&doc.rev), &doc.path, reason)
    }

    /// A local I/O error on one document: skip it for good when the name
    /// cannot exist on this filesystem, otherwise retry it later.
    fn io_failure(&self, doc: &FileDoc, e: &std::io::Error) -> Result<()> {
        if vaultfs::is_unwritable_name_error(e, &doc.path) {
            self.apply_skip(doc, &Skip::Unwritable(e.to_string()))
        } else {
            self.defer_doc(doc, &format!("local I/O error: {e}"))
        }
    }

    /// Put fetched content on disk, if the local file is clean.
    fn materialise(&self, p: Plan, bytes: Vec<u8>) -> Result<()> {
        let Plan { doc, base, local } = p;
        let vault = &self.inner.cfg.vault;
        let target = vault.join(&doc.path);
        let remote_hash = vaultfs::hash_bytes(&bytes);
        let state = if doc.conflicts.is_empty() {
            State::Clean
        } else {
            State::Conflicted
        };
        let note = (!doc.conflicts.is_empty()).then(|| conflict_note(&doc));
        let row_for = |disk: &DiskState| FileRow {
            path: doc.path.clone(),
            doc_id: doc.id.clone(),
            base_rev: Some(doc.rev.clone()),
            base_children: doc.children.clone(),
            disk_mtime: Some(disk.mtime_ms),
            disk_size: Some(disk.size),
            disk_hash: Some(disk.hash.clone()),
            state,
            note: note.clone(),
        };
        let count = |f: fn(&mut PullReport)| f(&mut self.inner.report.lock().unwrap());
        // Case-collision guard: on a filesystem that folds names, never let
        // a second document land on the file of another.
        let _fold = self.inner.fs.folds().then(|| self.inner.fold_lock.lock().unwrap());
        if self.inner.fs.folds() {
            let others = self.with_index(|ix| ix.fold_collisions(&doc.path, &doc.id))?;
            if let Some(o) = others
                .iter()
                .find(|o| matches!(o.state, State::Clean | State::Dirty | State::Conflicted))
            {
                return self.apply_skip(&doc, &Skip::Collision(o.path.clone()));
            }
        }
        if state == State::Conflicted {
            count(|r| r.conflicted += 1);
        }

        // Already identical on disk: adopt without touching the file.
        if let Some(l) = &local {
            if l.hash == remote_hash {
                self.with_index(|ix| {
                    ix.upsert(&row_for(l))?;
                    ix.remove_pending(&doc.id)
                })?;
                self.cleanup_old_path(base.as_ref(), &doc.path, l);
                count(|r| r.adopted += 1);
                return Ok(());
            }
        }

        let clean = match (&local, &base) {
            (None, _) => true,
            (Some(l), Some(b)) => b.disk_hash.as_ref() == Some(&l.hash),
            (Some(_), None) => false,
        };
        if !clean {
            return self.mark_dirty(
                &doc,
                base,
                "local file differs from the last synced version; remote change not applied",
            );
        }
        let expect = match &local {
            None => Expect::Absent,
            Some(l) => Expect::Hash(l.hash.clone()),
        };
        match vaultfs::atomic_write(&target, &bytes, doc.mtime, &expect) {
            Ok(w) if w.displaced.is_some() => {
                // An edit landed between our check and the swap. Put it back
                // and keep the old base, so the push side sends it as a
                // child of what the editor saw (a conflict, not a loss).
                warn!(
                    "{}: edited while a remote change was written; the edit is kept",
                    doc.path
                );
                count(|r| r.restored_edits += 1);
                self.put_back(&doc.path, w.displaced.unwrap(), Some(&w.state.hash));
                let row = match base {
                    Some(mut b) if b.path == doc.path => {
                        b.state = State::Dirty;
                        b.note = Some(format!("local edit during a remote update (remote {})", doc.rev));
                        b
                    }
                    _ => FileRow {
                        path: doc.path.clone(),
                        doc_id: doc.id.clone(),
                        base_rev: None,
                        base_children: vec![],
                        disk_mtime: None,
                        disk_size: None,
                        disk_hash: None,
                        state: State::Dirty,
                        note: Some("local edit during a remote update".into()),
                    },
                };
                self.with_index(|ix| {
                    ix.upsert(&row)?;
                    ix.remove_pending(&doc.id)
                })
            }
            Ok(w) => {
                let disk = w.state;
                self.with_index(|ix| {
                    ix.upsert(&row_for(&disk))?;
                    ix.remove_pending(&doc.id)
                })?;
                self.cleanup_old_path(base.as_ref(), &doc.path, &disk);
                let mut r = self.inner.report.lock().unwrap();
                r.written += 1;
                r.bytes_written += bytes.len() as u64;
                debug!("wrote {}", doc.path);
                Ok(())
            }
            Err(WriteError::Diverged { .. }) => self.mark_dirty(
                &doc,
                base,
                "local file changed during the write; remote change not applied",
            ),
            Err(WriteError::Io { source, .. }) => self.io_failure(&doc, &source),
        }
    }

    /// After a case-only rename, remove the old path if it is a different,
    /// still-clean file.
    fn cleanup_old_path(&self, base: Option<&FileRow>, new_path: &str, new_disk: &DiskState) {
        let Some(b) = base else { return };
        if b.path == new_path {
            return;
        }
        let vault = &self.inner.cfg.vault;
        // A case-only rename on a case-insensitive filesystem: the old name
        // may be the very file just written or adopted, even when the inode
        // numbers differ (FUSE need not keep them stable). Remove it only when
        // the directory listings show both spellings as separate entries.
        if b.path.to_lowercase() == new_path.to_lowercase()
            && !(vaultfs::exists_with_exact_case(vault, &b.path) && vaultfs::exists_with_exact_case(vault, new_path))
        {
            return;
        }
        let old = vault.join(&b.path);
        let Ok(Some(old_state)) = vaultfs::read_state(&old) else {
            return;
        };
        if old_state.file_id.is_some() && old_state.file_id == new_disk.file_id {
            return; // same file on a case-insensitive filesystem
        }
        if let Some(h) = &b.disk_hash {
            if *h == old_state.hash {
                if let Ok(RemoveOutcome::Raced { displaced }) =
                    vaultfs::remove_if_matches(&self.inner.cfg.vault, &old, h)
                {
                    self.put_back(&b.path, displaced, None);
                }
            }
        }
    }

    /// Put content displaced by a race back at `rel`: over the file we just
    /// wrote (`over` is its hash) or where nothing is now. If that fails
    /// too, keep it in the state directory's `recovered/` folder.
    pub(crate) fn put_back(&self, rel: &str, content: Vec<u8>, over: Option<&str>) {
        let target = self.inner.cfg.vault.join(rel);
        let expect = match over {
            Some(h) => Expect::Hash(h.to_string()),
            None => Expect::Absent,
        };
        match vaultfs::atomic_write(&target, &content, vaultfs::now_ms(), &expect) {
            Ok(w) if w.displaced.is_none() => {}
            Ok(w) => {
                self.save_recovered(rel, &content);
                if let Some(d) = w.displaced {
                    self.save_recovered(rel, &d);
                }
            }
            Err(_) => {
                self.save_recovered(rel, &content);
            }
        }
    }

    /// Save content to the state directory's `recovered/` folder. Returns
    /// whether it was saved.
    pub(crate) fn save_recovered(&self, rel: &str, content: &[u8]) -> bool {
        let dir = self.inner.state_dir.join("recovered");
        let name = format!("{}-{}", vaultfs::now_ms(), rel.replace('/', "%2F"));
        let p = dir.join(name);
        match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&p, content)) {
            Ok(()) => {
                warn!("{rel}: a copy was saved to {}", p.display());
                true
            }
            Err(e) => {
                warn!("{rel}: could not save a copy to {}: {e}", dir.display());
                false
            }
        }
    }

    /// Documents skipped because they collided with `path` get another try.
    fn requeue_collisions(&self, path: &str) -> Result<()> {
        let note = format!("{COLLISION_NOTE} {path}");
        self.with_index(|ix| {
            for r in ix.list_state_with_note(State::Skipped, &note, 100)? {
                if r.note.as_deref() == Some(note.as_str()) {
                    ix.delete_doc(&r.doc_id)?;
                    ix.add_pending_now(&r.doc_id, "the colliding file is gone", vaultfs::now_ms())?;
                }
            }
            Ok(())
        })
    }

    fn mark_dirty(&self, doc: &FileDoc, base: Option<FileRow>, why: &str) -> Result<()> {
        warn!("{}: {why}", doc.path);
        self.inner.report.lock().unwrap().dirty += 1;
        let note = format!("{why} (remote {})", doc.rev);
        let row = match base {
            Some(mut b) if b.path == doc.path => {
                b.state = State::Dirty;
                b.note = Some(note);
                b
            }
            _ => FileRow {
                path: doc.path.clone(),
                doc_id: doc.id.clone(),
                base_rev: None,
                base_children: vec![],
                disk_mtime: None,
                disk_size: None,
                disk_hash: None,
                state: State::Dirty,
                note: Some(note),
            },
        };
        self.with_index(|ix| {
            ix.upsert(&row)?;
            ix.remove_pending(&doc.id)
        })
    }

    /// Re-fetch documents that could not be applied earlier.
    async fn retry_pending(&self, all: bool) -> Result<usize> {
        let due = self.with_index(|ix| ix.pending(if all { None } else { Some(vaultfs::now_ms()) }))?;
        if due.is_empty() {
            return Ok(0);
        }
        info!("retrying {} pending document(s)", due.len());
        let mut rows = Vec::new();
        for p in &due {
            let doc = match self.inner.couch.fetch_doc(&p.doc_id, true).await? {
                DocFetch::Found(d) => d,
                DocFetch::Deleted => serde_json::json!({"_id": p.doc_id, "_deleted": true}),
                // Not a deletion: the database may have been recreated, or a
                // proxy answered for something else. Touch nothing; retry later.
                DocFetch::Missing(reason) => {
                    if self.with_index(|ix| ix.by_doc(&p.doc_id))?.is_none() {
                        // Nothing local depends on it (for example a refetch
                        // queued for a document a rebuild dropped).
                        debug!("{}: gone from the remote and not tracked locally; dropped", p.doc_id);
                        self.with_index(|ix| ix.remove_pending(&p.doc_id))?;
                        continue;
                    }
                    let why = format!("document missing on the remote (404 {reason}); local file left alone");
                    warn!("{}: {why}", p.doc_id);
                    self.with_index(|ix| ix.add_pending(&p.doc_id, p.rev.as_deref(), &why, vaultfs::now_ms()))?;
                    continue;
                }
            };
            rows.push(ChangeRow {
                seq: Value::Null,
                id: p.doc_id.clone(),
                changes: vec![],
                deleted: false,
                doc: Some(doc),
            });
        }
        self.apply_batch(rows).await?;
        Ok(due.len())
    }

    // ------------------------------------------------------------------
    // Daemon
    // ------------------------------------------------------------------

    /// Follow `_changes` with longpoll until `shutdown` turns true, without
    /// ever writing to the remote. Gate refusals stop syncing; the gate is
    /// re-checked periodically so a lifted lock resumes without a restart.
    pub async fn daemon_pull_only(&self, shutdown: watch::Receiver<bool>) -> Result<()> {
        self.daemon(shutdown, false).await
    }

    /// The two-way daemon: the pull loop here, the push loop (watcher,
    /// settle queue, periodic rescans) beside it. Pushes start once the gate
    /// passed, the remote was caught up (with a full fetch when one is owed)
    /// and this node is registered in the milestone.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        // Each side stops when the caller says so, or when the other side
        // ended (a fatal error on one side stops both).
        let (stop_tx, stop_rx) = watch::channel(false);
        let mut push = tokio::spawn(self.clone().push_loop(stop_rx.clone()));
        let pull = self.daemon(stop_rx, true);
        tokio::pin!(pull);
        let result = tokio::select! {
            r = &mut pull => {
                let _ = stop_tx.send(true);
                let pushed = push.await.map_err(EngineError::other)?;
                r.and(pushed.map(|_| ()))
            }
            r = &mut push => {
                let _ = stop_tx.send(true);
                let pulled = pull.await;
                r.map_err(EngineError::other)?.map(|_| ()).and(pulled)
            }
            _ = shutdown.changed() => {
                let _ = stop_tx.send(true);
                let pulled = pull.await;
                let pushed = push.await.map_err(EngineError::other)?;
                pulled.and(pushed.map(|_| ()))
            }
        };
        self.inner.push_enabled.store(false, Ordering::SeqCst);
        result
    }

    /// One two-way pass: gate, pull until caught up (a full fetch when one is
    /// owed), register, then push everything that changed locally.
    pub async fn sync_once(&self) -> Result<SyncReport> {
        let start = Instant::now();
        self.reset_report();
        let gate = self.connect_gate(false).await?;
        info!(
            "gate ok: {} (version {}, {} docs)",
            gate.remote, gate.version, gate.doc_count
        );
        let old_scope = self.apply_scope_narrowing()?;
        self.prepare_vault();
        self.catch_up(start).await?;
        let done = self.after_catch_up(&gate, &old_scope, None).await?;
        debug_assert!(done, "only a shutdown interrupts, and sync_once has none");
        let push = self.push_all(false).await?;
        // Our own writes come back through the changes feed; take them in so
        // `since` is current.
        self.catch_up(start).await?;
        self.retry_pending(true).await?;
        self.with_index(|ix| ix.set_meta(meta::LAST_PULL, &vaultfs::now_ms().to_string()))?;
        let mut pull = self.report();
        pull.elapsed_ms = start.elapsed().as_millis() as u64;
        pull.pending = self.with_index(|ix| ix.pending(None))?.len() as u64;
        Ok(SyncReport { pull, push })
    }

    /// Start over from the remote, as upstream's "Fetch": forget every
    /// revision and `since` (disk hashes are kept, so clean files are
    /// refreshed and edited ones protected), then do a full fetch at the
    /// next sync and mark this node resolved after it. This is how a lock
    /// that does not come with a rebuild (or `cleaned`) is cleared.
    pub fn request_full_fetch(&self) -> Result<()> {
        self.with_index(|ix| {
            ix.reset_for_rebuild()?;
            ix.set_meta(meta::NEEDS_FETCH, &vaultfs::now_ms().to_string())?;
            ix.set_meta(meta::RECONCILE, &vaultfs::now_ms().to_string())
        })
    }

    /// Why pushes must wait, if they must: a full fetch is owed, or this
    /// node still has to be marked resolved after a lock was let through.
    /// The push passes check this themselves, so no caller can push into a
    /// database that is being rebuilt or that we have not fully fetched.
    pub(crate) fn pushes_held(&self) -> Result<Option<&'static str>> {
        self.with_index(|ix| {
            Ok(if ix.get_meta(meta::NEEDS_FETCH)?.is_some() {
                Some("a full fetch of the remote is still owed")
            } else if ix.get_meta(meta::RESOLVE_OWED)?.is_some() {
                Some("this device is not yet marked resolved after a remote lock")
            } else {
                None
            })
        })
    }

    /// After the pull caught up: finish an owed full fetch (waiting for a
    /// rebuilding device to finish uploading), catch up a widened scope,
    /// register this node, and allow pushes. Returns false when a shutdown
    /// interrupted the quiet wait: then nothing is registered, no flag is
    /// cleared and pushes stay off, so the next start does it all again.
    async fn after_catch_up(
        &self,
        gate: &GateReport,
        old_scope: &Scope,
        shutdown: Option<&mut watch::Receiver<bool>>,
    ) -> Result<bool> {
        let (needs_fetch, wait_owed) = self.with_index(|ix| {
            Ok((
                ix.get_meta(meta::NEEDS_FETCH)?.is_some(),
                ix.get_meta(meta::REBUILD_WAIT)?.is_some(),
            ))
        })?;
        if wait_owed && !needs_fetch {
            self.with_index(|ix| ix.delete_meta(meta::REBUILD_WAIT))?; // stale
        }
        if needs_fetch && (wait_owed || gate.rebuild_detected || gate.fetch_to_resolve || gate.locked) {
            match self.wait_until_quiet(shutdown).await? {
                Quiet::Settled => {}
                Quiet::Interrupted => {
                    info!("stopped during the quiet wait; the full fetch is completed at the next start");
                    return Ok(false);
                }
            }
        }
        self.scope_catch_up(old_scope).await?;
        self.retry_pending(true).await?;
        let resolve_owed = self.with_index(|ix| ix.get_meta(meta::RESOLVE_OWED))?.is_some();
        let mark_resolved = needs_fetch || resolve_owed;
        self.register_node(mark_resolved).await?;
        if mark_resolved {
            self.with_index(|ix| {
                if needs_fetch {
                    // A new full fetch: its push-backs are reported afresh.
                    ix.delete_meta(meta::ORPHANS)?;
                }
                ix.delete_meta(meta::NEEDS_FETCH)?;
                ix.delete_meta(meta::RESOLVE_OWED)
            })?;
            info!("full fetch complete; this node is registered and marked resolved");
        }
        self.inner.push_enabled.store(true, Ordering::SeqCst);
        Ok(true)
    }

    /// After a rebuild elsewhere the rebuilding device keeps uploading for a
    /// while. Pull until the remote's update sequence stays the same for
    /// `rebuild_quiet`, so the fetch is complete before we push anything.
    ///
    /// - A nudge (network change, "sync now") restarts the window; it never
    ///   ends it early.
    /// - A database holding nothing but the version document is not quiet
    ///   yet (the rebuilder has not started uploading), up to 20 windows.
    /// - A shutdown returns `Interrupted`.
    async fn wait_until_quiet(&self, mut shutdown: Option<&mut watch::Receiver<bool>>) -> Result<Quiet> {
        let quiet = self.inner.cfg.rebuild_quiet;
        let started = Instant::now();
        let started_ms = vaultfs::now_ms();
        let max_empty = quiet * 20;
        let mut rounds = 0u32;
        loop {
            if quiet.is_zero() {
                break;
            }
            let info = self.inner.couch.db_info().await?;
            let before = seq_to_string(&info.update_seq);
            let msg = format!(
                "waiting since {started_ms} for the rebuilt remote to stay unchanged for {quiet:.0?} ({} documents so far){}",
                info.doc_count,
                self.rebuilder_note()
            );
            self.with_index(|ix| ix.set_meta(meta::REBUILD_WAIT, &format!("{} {msg}", vaultfs::now_ms())))?;
            if rounds == 0 {
                info!("{msg}");
            } else {
                debug!("{msg}");
            }
            rounds += 1;
            enum Woke {
                Timer,
                Nudged,
                Shutdown,
            }
            let woke = match shutdown.as_deref_mut() {
                Some(rx) => tokio::select! {
                    _ = tokio::time::sleep(quiet) => Woke::Timer,
                    _ = self.inner.nudge.notified() => Woke::Nudged,
                    _ = rx.changed() => Woke::Shutdown,
                },
                None => tokio::select! {
                    _ = tokio::time::sleep(quiet) => Woke::Timer,
                    _ = self.inner.nudge.notified() => Woke::Nudged,
                },
            };
            match woke {
                Woke::Shutdown => return Ok(Quiet::Interrupted),
                Woke::Nudged => {
                    info!("nudged during the quiet wait; catching up and restarting the {quiet:.0?} window");
                    self.catch_up(Instant::now()).await?;
                    continue;
                }
                Woke::Timer => {}
            }
            let after = self.inner.couch.db_info().await?;
            if before != seq_to_string(&after.update_seq) {
                self.catch_up(Instant::now()).await?;
                continue;
            }
            if after.doc_count <= 1 && started.elapsed() < max_empty {
                info!("the rebuilt remote is quiet but still empty; waiting for the rebuilding device to upload");
                continue;
            }
            if after.doc_count <= 1 {
                warn!(
                    "the rebuilt remote stayed empty for {:.0?}; treating it as settled",
                    started.elapsed()
                );
            }
            break;
        }
        self.with_index(|ix| ix.delete_meta(meta::REBUILD_WAIT))?;
        if !quiet.is_zero() {
            info!("the rebuilt remote settled after {:.0?}", started.elapsed());
        }
        Ok(Quiet::Settled)
    }

    /// For the quiet-wait log: which node(s) the milestone accepts (the
    /// rebuilder) and when they last connected.
    fn rebuilder_note(&self) -> String {
        // Best effort, from the milestone the gate read last.
        let Some(v) = self.inner.last_milestone.lock().unwrap().clone() else {
            return String::new();
        };
        let accepted: Vec<String> = v
            .get("accepted_nodes")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if accepted.is_empty() {
            return String::new();
        }
        let parts: Vec<String> = accepted
            .iter()
            .map(|n| {
                let info = v.get("node_info").and_then(|i| i.get(n));
                let name = info
                    .and_then(|i| i.get("device_name"))
                    .and_then(Value::as_str)
                    .unwrap_or(n);
                match info.and_then(|i| i.get("last_connected")).and_then(Value::as_i64) {
                    Some(t) => format!("{name} (last connected {t})"),
                    None => name.to_string(),
                }
            })
            .collect();
        format!("; accepted node(s): {}", parts.join(", "))
    }

    async fn daemon(&self, mut shutdown: watch::Receiver<bool>, two_way: bool) -> Result<()> {
        let mut backoff = Backoff::default();
        let mut prepared = false;
        let mut caught_up_once = false;
        let mut old_scope: Option<Scope> = None;
        'outer: loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            self.reset_report();
            let gate = match self.connect_gate(false).await {
                Ok(g) => {
                    info!("gate ok: {} (version {}, created {})", g.remote, g.version, g.created);
                    if g.rebuild_detected {
                        warn!("remote was rebuilt; refetching everything");
                    }
                    if self.pushes_held()?.is_some() {
                        // Stop pushing at once; `after_catch_up` turns it
                        // back on when the owed fetch is complete.
                        self.inner.push_enabled.store(false, Ordering::SeqCst);
                    }
                    if !prepared {
                        old_scope = Some(self.apply_scope_narrowing()?);
                        let this = self.clone();
                        tokio::task::spawn_blocking(move || this.prepare_vault()).await.ok();
                        prepared = true;
                    }
                    g
                }
                Err(e) => {
                    self.inner.push_enabled.store(false, Ordering::SeqCst);
                    let wait = match &e {
                        // Mid-rebuild states clear within seconds to minutes.
                        EngineError::Refused(
                            Refusal::MissingMilestone | Refusal::Locked | Refusal::MissingVersionDoc,
                        ) => Duration::from_secs(30),
                        EngineError::Refused(_) => Duration::from_secs(300),
                        EngineError::Remote(_) => backoff.next_delay(),
                        _ => return Err(e),
                    };
                    warn!("{e}; not syncing, checking again in {:.0?}", wait);
                    if self.sleep_or_shutdown(wait, &mut shutdown).await {
                        return Ok(());
                    }
                    continue;
                }
            };
            let session = Instant::now();
            // Catch up without longpoll first, so we know when we are current.
            let caught = match self.catch_up(session).await {
                Ok(()) => {
                    let scope = old_scope.clone().unwrap_or_else(|| self.inner.cfg.scope.clone());
                    let owed = match self.pushes_held() {
                        Ok(h) => h.is_some(),
                        Err(e) => return Err(e),
                    };
                    let r = if two_way && caught_up_once && !owed {
                        // A routine re-gate: refresh the registration (at
                        // most once a minute) and carry on pushing.
                        match self.register_node(false).await {
                            Ok(_) => {
                                self.inner.push_enabled.store(true, Ordering::SeqCst);
                                Ok(())
                            }
                            Err(e) => Err(e),
                        }
                    } else if two_way {
                        match self.after_catch_up(&gate, &scope, Some(&mut shutdown)).await {
                            Ok(true) => Ok(()),
                            Ok(false) => return Ok(()), // shut down during the quiet wait
                            Err(e) => Err(e),
                        }
                    } else {
                        match self.finish_read_only_fetch() {
                            Ok(()) => self.scope_catch_up(&scope).await,
                            Err(e) => Err(e),
                        }
                    };
                    if r.is_ok() {
                        old_scope = Some(self.inner.cfg.scope.clone());
                        caught_up_once = true;
                    }
                    r
                }
                Err(e) => Err(e),
            };
            if let Err(e) = caught {
                self.record_error(&e);
                self.inner.push_enabled.store(false, Ordering::SeqCst);
                let wait = match &e {
                    EngineError::Refused(_) => Duration::from_secs(300),
                    EngineError::Remote(_) | EngineError::Other(_) => backoff.next_delay(),
                    _ => return Err(e),
                };
                warn!("catching up failed: {e}; retrying in {wait:.0?}");
                if self.sleep_or_shutdown(wait, &mut shutdown).await {
                    return Ok(());
                }
                continue 'outer;
            }
            if *shutdown.borrow() {
                return Ok(());
            }
            let regate_at = Instant::now() + self.inner.cfg.regate_interval;
            let mut last_retry: Option<Instant> = None;
            loop {
                let retry_allowed_at = last_retry.map(|t| t + PENDING_RETRY_MIN_GAP);
                let due = self.pending_due_at(retry_allowed_at)?;
                if due.is_some_and(|d| d <= Instant::now()) {
                    if let Err(e) = self.retry_pending(false).await {
                        warn!("pending retry failed: {e}");
                    }
                    last_retry = Some(Instant::now());
                }
                let _ = self.with_index(|ix| ix.set_meta(meta::DAEMON_SEEN, &vaultfs::now_ms().to_string()));
                // Leave the longpoll open unless a pending row falls due or
                // it is time to re-run the gate.
                let retry_allowed_at = last_retry.map(|t| t + PENDING_RETRY_MIN_GAP);
                let wake_at = match self.pending_due_at(retry_allowed_at)? {
                    Some(d) => d.min(regate_at),
                    None => regate_at,
                };
                let since = self.since()?;
                let q = ChangesQuery {
                    since,
                    limit: self.inner.cfg.batch_limit,
                    longpoll: true,
                    heartbeat_ms: 30_000,
                };
                let res = tokio::select! {
                    r = self.inner.couch.changes(&q) => Some(r),
                    _ = tokio::time::sleep_until(wake_at.into()) => None,
                    // Network change or similar: reconnect and re-check now.
                    _ = self.inner.nudge.notified() => None,
                    _ = shutdown.changed() => return Ok(()),
                };
                match res {
                    None => {
                        if Instant::now() >= regate_at {
                            continue 'outer;
                        }
                    }
                    Some(Ok(resp)) => {
                        backoff.reset();
                        if resp.results.is_empty() {
                            self.with_index(|ix| ix.set_meta(meta::SINCE, &seq_to_string(&resp.last_seq)))?;
                            continue;
                        }
                        let before = self.report();
                        match self.apply_batch(resp.results).await {
                            Ok(()) => {
                                self.with_index(|ix| {
                                    ix.set_meta(meta::SINCE, &seq_to_string(&resp.last_seq))?;
                                    ix.set_meta(meta::LAST_PULL, &vaultfs::now_ms().to_string())
                                })?;
                                self.log_progress(session, &before, resp.pending);
                            }
                            Err(e) => {
                                self.record_error(&e);
                                let wait = match &e {
                                    EngineError::Refused(_) => Duration::from_secs(300),
                                    EngineError::Remote(_) => backoff.next_delay(),
                                    _ => return Err(e),
                                };
                                warn!("batch failed: {e}; reconnecting in {wait:.0?}");
                                if self.sleep_or_shutdown(wait, &mut shutdown).await {
                                    return Ok(());
                                }
                                continue 'outer;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        let wait = backoff.next_delay();
                        let e = EngineError::from(e);
                        self.record_error(&e);
                        warn!("changes feed failed: {e}; reconnecting in {wait:.0?}");
                        if self.sleep_or_shutdown(wait, &mut shutdown).await {
                            return Ok(());
                        }
                        continue 'outer;
                    }
                }
            }
        }
    }

    /// When the earliest pending row is due, as an `Instant`, but not before
    /// `not_before`. `None` when nothing is pending.
    fn pending_due_at(&self, not_before: Option<Instant>) -> Result<Option<Instant>> {
        let Some(due_ms) = self.with_index(|ix| ix.next_pending_due())? else {
            return Ok(None);
        };
        let wait = Duration::from_millis((due_ms - vaultfs::now_ms()).max(0) as u64);
        let at = Instant::now() + wait;
        Ok(Some(not_before.map_or(at, |nb| at.max(nb))))
    }

    /// Sleep, waking early on a nudge. Returns true on shutdown.
    async fn sleep_or_shutdown(&self, d: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(d) => false,
            _ = self.inner.nudge.notified() => false,
            _ = shutdown.changed() => true,
        }
    }

    // ------------------------------------------------------------------
    // Status
    // ------------------------------------------------------------------

    pub fn status(&self) -> Result<Status> {
        status_from_index(
            &self.inner.index.lock().unwrap(),
            &self.inner.cfg.vault,
            &self.inner.cfg.index_path,
        )
    }
}

/// Status straight from an index file (no network).
pub fn status_from_index(ix: &Index, vault: &Path, index_path: &Path) -> Result<Status> {
    let num = |k: &str| -> Result<Option<i64>> { Ok(ix.get_meta(k)?.and_then(|s| s.parse().ok())) };
    Ok(Status {
        remote: ix.get_meta(meta::REMOTE)?,
        vault: vault.display().to_string(),
        index: index_path.display().to_string(),
        node_id: ix.get_meta(meta::NODE_ID)?,
        since: ix.get_meta(meta::SINCE)?,
        remote_created: num(meta::CREATED)?,
        remote_version: ix.get_meta(meta::VERSION)?,
        last_gate: ix.get_meta(meta::LAST_GATE)?,
        last_gate_ok_ms: num(meta::LAST_GATE_OK)?,
        last_pull_ms: num(meta::LAST_PULL)?,
        last_error: ix.get_meta(meta::LAST_ERROR)?,
        daemon_heartbeat_ms: num(meta::DAEMON_SEEN)?,
        rebuild_detected_ms: num(meta::REBUILD_AT)?,
        counts: ix.counts_by_state()?.into_iter().collect(),
        dirty: ix.list_state(State::Dirty, 50)?,
        conflicted: ix.list_state(State::Conflicted, 50)?,
        pending: ix.pending(None)?,
        unwritable: ix.list_state_with_note(State::Skipped, UNWRITABLE_NOTE, 200)?,
        collisions: ix.list_state_with_note(State::Skipped, COLLISION_NOTE, 200)?,
        out_of_scope: ix.list_state(State::OutOfScope, 200)?,
        local_issues: ix.issues()?,
        scope: ix
            .get_meta(meta::SCOPE)?
            .and_then(|s| serde_json::from_str::<Scope>(&s).ok())
            .map(|s| s.describe()),
        fs_rules: ix.get_meta(meta::FS_RULES)?,
        registered_ms: num(meta::REGISTERED)?,
        last_push_ms: num(meta::LAST_PUSH)?,
        needs_full_fetch: ix.get_meta(meta::NEEDS_FETCH)?.is_some() || ix.get_meta(meta::RESOLVE_OWED)?.is_some(),
        safety_stop: ix.get_meta(meta::SAFETY_STOP)?,
        rebuild_wait: ix.get_meta(meta::REBUILD_WAIT)?,
        rebuild_orphans: ix
            .get_meta(meta::ORPHANS)?
            .and_then(|s| serde_json::from_str::<crate::push::OrphanRecord>(&s).ok())
            .map(|r| r.describe()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_rules() {
        let r = parse_ignore("\\/\\.git\\/|[]|^Private/|[]|!!\\.md$", false).unwrap();
        assert_eq!(r.len(), 3);
        assert!(r[0].re.is_match("a/.git/x"));
        assert!(r[1].re.is_match("private/x.md"), "case-insensitive by default");
        assert!(r[2].negate);
        assert!(parse_ignore("", false).unwrap().is_empty());
    }

    fn ignored(rules: &[IgnoreRule], path: &str) -> bool {
        rules.iter().any(|r| r.re.is_match(path) != r.negate)
    }

    #[test]
    fn ignore_rules_match_upstream_syntax() {
        // A single `!` is part of the pattern, not a negation.
        let r = parse_ignore("!draft", false).unwrap();
        assert!(!r[0].negate);
        assert!(ignored(&r, "notes/!draft.md"));
        assert!(!ignored(&r, "notes/draft.md"));
        // `!!` negates: everything that does not match is ignored.
        let r = parse_ignore("!!^keep/", false).unwrap();
        assert!(r[0].negate);
        assert!(ignored(&r, "other/x.md"));
        assert!(!ignored(&r, "keep/x.md"));
        // Spaces and newlines are removed from the whole list before splitting.
        let r = parse_ignore("^a b/ |[]|\n^c/\n|[]|  ", false).unwrap();
        assert_eq!(r.len(), 2);
        assert!(ignored(&r, "ab/x.md"));
        assert!(!ignored(&r, "a b/x.md"));
        assert!(ignored(&r, "c/x.md"));
    }
}
