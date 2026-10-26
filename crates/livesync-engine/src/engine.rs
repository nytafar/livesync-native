//! Gate evaluation, the pull loop and the pull-only daemon.

use crate::index::{FileRow, Index, PendingRow, State};
use crate::vaultfs::{self, DiskState, Expect, LockError, LockFile, RemoveOutcome, WriteError};
use futures::stream::{self, StreamExt};
use livesync_model::chunk::chunk_id_xxhash64;
use livesync_model::codec::{self, CodecError};
use livesync_model::doc::{classify_doc, Classified, MILESTONE_LOCAL_ID, SYNC_PARAMS_LOCAL_ID, VERSION_DOC_ID};
use livesync_model::gate::{self, Milestone, Refusal};
use livesync_model::path::{classify_path, is_internal_id, PathSkip};
use livesync_model::{FileDoc, FileKind};
use livesync_remote::{Backoff, ChangeRow, ChangesQuery, Couch, DocFetch, RemoteError};
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{watch, Notify, Semaphore};
use tracing::{debug, info, warn};

/// Keys used in the index `meta` table.
mod meta {
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
}

/// Note prefix (and status filter) for documents whose names the vault's
/// filesystem cannot hold.
pub const UNWRITABLE_NOTE: &str = "unwritable name";

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
    /// and control characters) without trying them. `None`: on when the vault
    /// is on Android shared storage. Names the filesystem rejects at write
    /// time are skipped either way.
    pub restrict_names: Option<bool>,
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
    fn other(e: impl std::fmt::Display) -> Self {
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
    pub elapsed_ms: u64,
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
struct Runtime {
    max_size_bytes: Option<u64>,
    ignore: Vec<IgnoreRule>,
}

struct Inner {
    couch: Couch,
    cfg: EngineConfig,
    /// Canonical vault path, recorded in (and checked against) the index.
    vault_canon: String,
    restrict_names: bool,
    _locks: Vec<LockFile>,
    index: Mutex<Index>,
    runtime: Mutex<Runtime>,
    mem: Semaphore,
    report: Mutex<PullReport>,
    nudge: Notify,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
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
}

impl Skip {
    fn label(&self) -> &'static str {
        match self {
            Skip::Path(p) => p.as_str(),
            Skip::Ignored => "ignore-regex",
            Skip::TooLarge(_) => "too-large",
            Skip::Legacy => "legacy-notes-doc",
            Skip::Unwritable(_) => "unwritable-name",
        }
    }
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

fn seq_to_string(v: &Value) -> String {
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
        let restrict_names = cfg
            .restrict_names
            .unwrap_or_else(|| vaultfs::is_android_shared_storage(&canon));
        let index = Index::open(&cfg.index_path)?;
        if index.get_meta(meta::NODE_ID)?.is_none() {
            index.set_meta(meta::NODE_ID, &random_node_id())?;
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
                restrict_names,
                _locks: locks,
                runtime: Mutex::new(Runtime {
                    max_size_bytes: cfg.max_size_mb.map(|m| m * 1024 * 1024),
                    ignore,
                }),
                cfg,
                index: Mutex::new(index),
                mem: Semaphore::new(budget),
                report: Mutex::new(PullReport::default()),
                nudge: Notify::new(),
            }),
        })
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

    fn with_index<T>(&self, f: impl FnOnce(&Index) -> rusqlite::Result<T>) -> Result<T> {
        let ix = self.inner.index.lock().unwrap();
        Ok(f(&ix)?)
    }

    fn record_error(&self, e: &EngineError) {
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
        let milestone = Milestone::parse(&milestone)?;
        let node_id = self.node_id();
        gate::assess_milestone(&milestone, &node_id)?;
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
                    ix.set_meta(meta::REBUILD_AT, &vaultfs::now_ms().to_string())?;
                    rebuild = true;
                }
                _ => {}
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
        {
            let mut rt = self.inner.runtime.lock().unwrap();
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
        })
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
        self.prepare_vault();
        loop {
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
            self.log_progress(start, resp.pending);
            if n == 0 || resp.pending == Some(0) {
                break;
            }
        }
        self.retry_pending(true).await?;
        self.with_index(|ix| ix.set_meta(meta::LAST_PULL, &vaultfs::now_ms().to_string()))?;
        let mut rep = self.report();
        rep.elapsed_ms = start.elapsed().as_millis() as u64;
        rep.pending = self.with_index(|ix| ix.pending(None))?.len() as u64;
        Ok(rep)
    }

    /// After a passed gate: remove temp files left by an interrupted run (safe,
    /// since this process holds the vault) and make sure `.nomedia` exists.
    fn prepare_vault(&self) {
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

    fn log_progress(&self, start: Instant, pending: Option<u64>) {
        let r = self.report();
        info!(
            "batch {}: {} changes, {} written, {} adopted, {} unchanged, {} deleted, {} dirty, {} chunks ({:.1} MB), remaining {}, {:.1}s",
            r.batches,
            r.changes,
            r.written,
            r.adopted,
            r.unchanged,
            r.deleted,
            r.dirty,
            r.chunks_fetched,
            r.chunk_bytes as f64 / 1e6,
            pending.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
            start.elapsed().as_secs_f64()
        );
    }

    /// Apply one page of changes. The caller persists `since` afterwards.
    async fn apply_batch(&self, rows: Vec<ChangeRow>) -> Result<()> {
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
                    if is_internal_id(&id) || classify_path(&path).is_some() {
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
        if self.inner.restrict_names {
            let bad = vaultfs::fat_invalid_chars(&f.path);
            if !bad.is_empty() {
                let list: Vec<String> = bad.iter().map(|c| format!("{c:?}")).collect();
                return Some(Skip::Unwritable(format!(
                    "contains {} (not allowed on Android shared storage)",
                    list.join(" ")
                )));
            }
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
        if rt.ignore.iter().any(|r| r.re.is_match(&f.path) != r.negate) {
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
        if b.state == State::Skipped {
            return self.with_index(|ix| {
                ix.delete_doc(id)?;
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
                                    #[cfg(unix)]
                                    ino: (0, 0),
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
        let note = (!doc.conflicts.is_empty()).then(|| {
            format!(
                "{} conflicting revision(s); resolve with an official client",
                doc.conflicts.len()
            )
        });
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
            Ok(disk) => {
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
        #[cfg(unix)]
        if old_state.ino == new_disk.ino {
            return; // same file on a case-insensitive filesystem
        }
        if let Some(h) = &b.disk_hash {
            if *h == old_state.hash {
                let _ = vaultfs::remove_if_matches(&self.inner.cfg.vault, &old, h);
            }
        }
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

    /// Follow `_changes` with longpoll until `shutdown` turns true. Gate
    /// refusals stop syncing; the gate is re-checked periodically so a lifted
    /// lock resumes without a restart.
    pub async fn daemon_pull_only(&self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut backoff = Backoff::default();
        let mut prepared = false;
        'outer: loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            self.reset_report();
            match self.connect_gate(false).await {
                Ok(g) => {
                    info!("gate ok: {} (version {}, created {})", g.remote, g.version, g.created);
                    if g.rebuild_detected {
                        warn!("remote was rebuilt; refetching everything");
                    }
                    if !prepared {
                        let this = self.clone();
                        tokio::task::spawn_blocking(move || this.prepare_vault()).await.ok();
                        prepared = true;
                    }
                }
                Err(e) => {
                    let wait = match &e {
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
            }
            let regate_at = Instant::now() + self.inner.cfg.regate_interval;
            let mut last_retry: Option<Instant> = None;
            let session = Instant::now();
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
                        match self.apply_batch(resp.results).await {
                            Ok(()) => {
                                self.with_index(|ix| {
                                    ix.set_meta(meta::SINCE, &seq_to_string(&resp.last_seq))?;
                                    ix.set_meta(meta::LAST_PULL, &vaultfs::now_ms().to_string())
                                })?;
                                self.log_progress(session, resp.pending);
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
