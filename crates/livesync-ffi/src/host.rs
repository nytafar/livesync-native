//! The engine host: one Tokio runtime, N profiles, one supervisor task each.
//!
//! A supervisor opens the profile's engine, runs it (two-way, or read-only
//! when the profile is pull-only) until told to stop, and reopens it with a
//! growing delay when it ends with an error. Gate refusals and network
//! trouble never end an engine (the daemon waits and re-checks by itself);
//! what ends one is local and fatal, such as an index error, and is shown as
//! `Failed` with the error until the next attempt.

use crate::profile::{paths_overlap, ProfileConfig};
use crate::status::{self, GateResult, ProfileStatus};
use crate::{logging, LiveSyncError};
use livesync_engine::engine::status_from_index;
use livesync_engine::engine::AccessCheck;
use livesync_engine::{Engine, Index};
use livesync_remote::Couch;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;
use tracing::{error, info, warn, Instrument};

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RunState {
    /// No engine for this profile.
    Stopped,
    /// Opening the index and probing the vault folder.
    Starting,
    /// The engine is running. Gate refusals and network outages are waited
    /// out inside the engine; `last_gate` and `last_error` say which.
    Running,
    /// The engine ended with an error; the supervisor retries after a delay
    /// (sooner on `sync_now` or when the network comes back).
    Failed,
}

/// Run-state changes, for the service's notification. Called on an engine
/// thread: implementations must return quickly.
#[uniffi::export(callback_interface)]
pub trait StatusListener: Send + Sync {
    fn on_state_changed(&self, profile_id: String, state: RunState, message: Option<String>);
}

/// How long `stop` waits for an engine to wind down before abandoning it.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
/// First and longest delay before reopening an engine that failed.
const RETRY_FIRST: Duration = Duration::from_secs(15);
const RETRY_MAX: Duration = Duration::from_secs(15 * 60);
/// A run this long counts as healthy: the next failure starts the delays over.
const HEALTHY_RUN: Duration = Duration::from_secs(10 * 60);

/// The front-end's answer to "is the whole vault still visible to this
/// process?" (Android: all-files access is still granted). Asked before any
/// pass that could delete remote documents; while it reports a problem,
/// missing files are never treated as deleted (the engine's `AccessCheck`).
#[uniffi::export(callback_interface)]
pub trait VaultAccess: Send + Sync {
    /// `None` when the vault is fully accessible, otherwise the reason.
    fn problem(&self, vault_path: String) -> Option<String>;
}

type Listener = Arc<Mutex<Option<Arc<dyn StatusListener>>>>;
type Access = Arc<Mutex<Option<Arc<dyn VaultAccess>>>>;

/// Lock, recovering from poisoning: a panic elsewhere must not wedge every
/// later call (the data behind these locks is always left consistent).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// State shared between a profile's slot and its supervisor task.
struct Shared {
    cfg: ProfileConfig,
    engine: Mutex<Option<Engine>>,
    state: Mutex<(RunState, Option<String>)>,
    /// Cuts a retry delay short.
    wake: Notify,
    listener: Listener,
    access: Access,
}

impl Shared {
    fn set_state(&self, state: RunState, message: Option<String>) {
        *lock(&self.state) = (state, message.clone());
        // Call outside the lock, and never let a failing foreign callback
        // (a panic once it crosses back) take the supervisor down.
        let listener = lock(&self.listener).clone();
        if let Some(l) = listener {
            let id = self.cfg.id.clone();
            if std::panic::catch_unwind(AssertUnwindSafe(|| l.on_state_changed(id, state, message))).is_err() {
                warn!("the status listener failed");
            }
        }
    }

    fn engine(&self) -> Option<Engine> {
        lock(&self.engine).clone()
    }
}

struct Slot {
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

#[derive(uniffi::Object)]
pub struct LiveSync {
    rt: tokio::runtime::Runtime,
    slots: Mutex<HashMap<String, Slot>>,
    listener: Listener,
    access: Access,
}

fn open_engine(cfg: &ProfileConfig, access: &Access) -> Result<Engine, LiveSyncError> {
    let mut ec = cfg.engine_config().map_err(|msg| LiveSyncError::Invalid { msg })?;
    // Read at every check, so a check installed after the start still applies.
    let access = access.clone();
    let vault = cfg.vault_path.clone();
    ec.access_check = Some(AccessCheck::new(move || {
        let Some(check) = lock(&access).clone() else {
            return Ok(());
        };
        // A check that fails to answer counts as "no access": the safe side.
        match std::panic::catch_unwind(AssertUnwindSafe(|| check.problem(vault.clone()))) {
            Ok(None) => Ok(()),
            Ok(Some(why)) => Err(why),
            Err(_) => Err("the vault access check failed".into()),
        }
    }));
    let couch = Couch::new(&cfg.remote()).map_err(|e| LiveSyncError::Invalid { msg: e.to_string() })?;
    std::fs::create_dir_all(&cfg.state_dir).map_err(|e| LiveSyncError::Other {
        msg: format!("cannot create the state folder: {e}"),
    })?;
    Ok(Engine::open(couch, ec)?)
}

async fn supervise(shared: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    let mut delay = RETRY_FIRST;
    loop {
        if *stop.borrow() {
            break;
        }
        shared.set_state(RunState::Starting, None);
        let cfg = shared.cfg.clone();
        let access = shared.access.clone();
        let opened = tokio::task::spawn_blocking(move || open_engine(&cfg, &access)).await;
        let failure = match opened {
            Ok(Ok(engine)) => {
                *lock(&shared.engine) = Some(engine.clone());
                let mode = if shared.cfg.pull_only { "pull-only" } else { "two-way" };
                info!("engine started ({mode}) for {}", shared.cfg.vault_path);
                shared.set_state(RunState::Running, Some(mode.into()));
                let started = Instant::now();
                let rx = stop.clone();
                let pull_only = shared.cfg.pull_only;
                // A separate task, so a panic in the engine is reported here
                // instead of taking the supervisor down with it.
                let run = tokio::spawn(
                    async move {
                        if pull_only {
                            engine.daemon_pull_only(rx).await
                        } else {
                            engine.run(rx).await
                        }
                    }
                    .in_current_span(),
                );
                let result = run.await;
                *lock(&shared.engine) = None;
                if *stop.borrow() {
                    break;
                }
                if started.elapsed() >= HEALTHY_RUN {
                    delay = RETRY_FIRST;
                }
                match result {
                    Ok(Ok(())) => "the engine stopped by itself".to_string(),
                    Ok(Err(e)) => e.to_string(),
                    Err(j) => format!("the engine crashed: {j}"),
                }
            }
            Ok(Err(e)) => e.to_string(),
            Err(j) => format!("opening the engine crashed: {j}"),
        };
        error!("{failure}; retrying in {}s", delay.as_secs());
        shared.set_state(RunState::Failed, Some(failure));
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shared.wake.notified() => {}
            _ = stop.changed() => {}
        }
        delay = (delay * 2).min(RETRY_MAX);
    }
    info!("engine stopped");
    shared.set_state(RunState::Stopped, None);
}

impl LiveSync {
    fn slot_shared(&self, id: &str) -> Option<Arc<Shared>> {
        lock(&self.slots).get(id).map(|s| s.shared.clone())
    }

    fn running_engine(&self, id: &str) -> Result<Engine, LiveSyncError> {
        self.slot_shared(id)
            .and_then(|s| s.engine())
            .ok_or_else(|| LiveSyncError::NotRunning {
                msg: format!("no engine is running for profile {id}"),
            })
    }

    fn stop_slot(&self, slot: Slot) {
        let _ = slot.stop.send(true);
        let mut task = slot.task;
        let finished = self
            .rt
            .block_on(async { tokio::time::timeout(STOP_TIMEOUT, &mut task).await.is_ok() });
        if !finished {
            warn!(
                "profile {}: the engine did not stop in time; abandoning it",
                slot.shared.cfg.id
            );
            task.abort();
            *lock(&slot.shared.engine) = None;
            slot.shared
                .set_state(RunState::Stopped, Some("abandoned while stopping".into()));
        }
    }
}

#[uniffi::export]
impl LiveSync {
    /// Create the host. `log_dir` receives `livesync.log` (rotated at 4 MB).
    #[uniffi::constructor]
    pub fn new(log_dir: String) -> Result<Arc<Self>, LiveSyncError> {
        logging::init();
        if !log_dir.is_empty() {
            logging::set_log_dir(Path::new(&log_dir)).map_err(|e| LiveSyncError::Other {
                msg: format!("cannot open the log folder: {e}"),
            })?;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("livesync")
            .enable_all()
            .build()
            .map_err(|e| LiveSyncError::Other { msg: e.to_string() })?;
        info!("livesync core {} ready", env!("CARGO_PKG_VERSION"));
        Ok(Arc::new(LiveSync {
            rt,
            slots: Mutex::new(HashMap::new()),
            listener: Arc::new(Mutex::new(None)),
            access: Arc::new(Mutex::new(None)),
        }))
    }

    /// Install the vault access check every engine asks before a pass that
    /// could delete remote documents (see [`VaultAccess`]).
    pub fn set_vault_access(&self, access: Box<dyn VaultAccess>) {
        *lock(&self.access) = Some(Arc::from(access));
    }

    /// Receive run-state changes of every profile.
    pub fn set_listener(&self, listener: Box<dyn StatusListener>) {
        *lock(&self.listener) = Some(Arc::from(listener));
    }

    /// Start (or restart with new settings) the engine for a profile. Refused
    /// when another running profile's vault folder is the same as, inside or
    /// above this one, or shares its state folder.
    pub fn start(&self, cfg: ProfileConfig) -> Result<(), LiveSyncError> {
        cfg.validate().map_err(|msg| LiveSyncError::Invalid { msg })?;
        let previous = {
            let mut slots = lock(&self.slots);
            for (id, other) in slots.iter() {
                if *id == cfg.id {
                    continue;
                }
                let o = &other.shared.cfg;
                if paths_overlap(&o.vault_path, &cfg.vault_path) {
                    return Err(LiveSyncError::Busy {
                        msg: format!(
                            "the vault folder overlaps profile {id}'s ({}); vault folders may not be the same or inside each other",
                            o.vault_path
                        ),
                    });
                }
                if paths_overlap(&o.state_dir, &cfg.state_dir) {
                    return Err(LiveSyncError::Busy {
                        msg: format!("the state folder overlaps profile {id}'s"),
                    });
                }
            }
            slots.remove(&cfg.id)
        };
        if let Some(prev) = previous {
            info!("restarting profile {} with new settings", cfg.id);
            self.stop_slot(prev);
        }
        let (tx, rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            cfg: cfg.clone(),
            engine: Mutex::new(None),
            state: Mutex::new((RunState::Starting, None)),
            wake: Notify::new(),
            listener: self.listener.clone(),
            access: self.access.clone(),
        });
        let span = tracing::info_span!("profile", id = %cfg.id);
        let task = self.rt.spawn(supervise(shared.clone(), rx).instrument(span));
        self.slots
            .lock()
            .unwrap()
            .insert(cfg.id.clone(), Slot { shared, stop: tx, task });
        Ok(())
    }

    /// Stop a profile's engine (waits up to 15 s for it to wind down).
    pub fn stop(&self, profile_id: String) {
        let slot = lock(&self.slots).remove(&profile_id);
        if let Some(slot) = slot {
            self.stop_slot(slot);
        }
    }

    pub fn stop_all(&self) {
        let slots: Vec<Slot> = lock(&self.slots).drain().map(|(_, s)| s).collect();
        for s in &slots {
            let _ = s.stop.send(true);
        }
        for s in slots {
            self.stop_slot(s);
        }
    }

    /// IDs of the profiles that have an engine slot (running, starting or
    /// waiting to retry).
    pub fn active_profiles(&self) -> Vec<String> {
        let mut v: Vec<String> = lock(&self.slots).keys().cloned().collect();
        v.sort();
        v
    }

    /// "Sync now": reconnect at once and rescan the vault; a failed engine is
    /// retried at once.
    pub fn sync_now(&self, profile_id: String) -> Result<(), LiveSyncError> {
        let shared = self.slot_shared(&profile_id).ok_or_else(|| LiveSyncError::NotRunning {
            msg: format!("profile {profile_id} is not started"),
        })?;
        match shared.engine() {
            Some(e) => e.network_changed(true),
            None => shared.wake.notify_one(),
        }
        Ok(())
    }

    /// The network came up or went down (NetworkMonitor). On up, every
    /// engine reconnects and rescans, and failed engines retry at once.
    pub fn network_changed(&self, up: bool, metered: bool) {
        info!(
            "network {}{}",
            if up { "up" } else { "down" },
            if metered { " (metered)" } else { "" }
        );
        if !up {
            return;
        }
        let all: Vec<Arc<Shared>> = lock(&self.slots).values().map(|s| s.shared.clone()).collect();
        for s in all {
            match s.engine() {
                Some(e) => e.network_changed(true),
                None => s.wake.notify_one(),
            }
        }
    }

    /// Status of a profile, running or not. For a profile without an engine
    /// the index is read from `state_dir` (no network).
    pub fn status(&self, profile_id: String, vault_path: String, state_dir: String) -> ProfileStatus {
        let shared = self.slot_shared(&profile_id);
        let (state, message, pull_only) = match &shared {
            Some(s) => {
                let (st, msg) = lock(&s.state).clone();
                (st, msg, s.cfg.pull_only)
            }
            None => (RunState::Stopped, None, false),
        };
        let engine = shared.as_ref().and_then(|s| s.engine());
        let (snap, session) = match &engine {
            Some(e) => (e.status().ok(), Some(e.report())),
            None => {
                let index_path = PathBuf::from(&state_dir).join("index.sqlite");
                let snap = if index_path.exists() {
                    Index::open(&index_path)
                        .ok()
                        .and_then(|ix| status_from_index(&ix, Path::new(&vault_path), &index_path).ok())
                } else {
                    None
                };
                (snap, None)
            }
        };
        status::from_engine(&profile_id, state, message, pull_only, snap.as_ref(), session.as_ref())
    }

    /// Run the connect gate for settings that are not saved yet, against a
    /// throwaway index and vault under `scratch_dir` (removed afterwards).
    /// Reads only; never writes to the remote or to the real vault.
    pub fn connect_test(&self, cfg: ProfileConfig, scratch_dir: String) -> GateResult {
        if let Err(msg) = cfg.validate() {
            return GateResult::with(crate::GateOutcome::Failed, msg);
        }
        let scratch = PathBuf::from(&scratch_dir).join(format!("connect-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let mut test = cfg.clone();
        test.vault_path = scratch.join("vault").display().to_string();
        test.state_dir = scratch.join("state").display().to_string();
        let result = self.rt.block_on(async {
            let t = test.clone();
            let access = self.access.clone();
            let engine = match tokio::task::spawn_blocking(move || open_engine(&t, &access)).await {
                Ok(Ok(e)) => e,
                Ok(Err(e)) => return GateResult::with(crate::GateOutcome::Failed, e.to_string()),
                Err(e) => return GateResult::with(crate::GateOutcome::Failed, e.to_string()),
            };
            match tokio::time::timeout(Duration::from_secs(60), engine.connect_gate(false)).await {
                Ok(Ok(g)) => GateResult::passed(&g),
                Ok(Err(e)) => GateResult::failed(&e),
                Err(_) => GateResult::with(
                    crate::GateOutcome::Unreachable,
                    "no answer from the server within 60 s".into(),
                ),
            }
        });
        let _ = std::fs::remove_dir_all(&scratch);
        info!(
            "connect test for {}: {:?} {}",
            cfg.database, result.outcome, result.message
        );
        result
    }

    /// Accept changed PREFERRED tweak values on the remote (the gate's
    /// "tweaks changed" refusal) for a running profile, then reconnect.
    pub fn adopt_tweaks(&self, profile_id: String) -> Result<GateResult, LiveSyncError> {
        let engine = self.running_engine(&profile_id)?;
        let r = self.rt.block_on(engine.connect_gate(true));
        engine.nudge();
        Ok(match r {
            Ok(g) => GateResult::passed(&g),
            Err(e) => GateResult::failed(&e),
        })
    }

    /// Start over from the remote (upstream's "Fetch"): forget every
    /// revision and `since`, keep the disk hashes (clean files are refreshed,
    /// edited ones kept), fetch everything, and in two-way mode mark this
    /// device resolved. This is how a milestone lock without a rebuild is
    /// cleared. The engine is restarted.
    pub fn fetch_from_remote(&self, profile_id: String) -> Result<(), LiveSyncError> {
        let slot = lock(&self.slots).remove(&profile_id);
        let Some(slot) = slot else {
            return Err(LiveSyncError::NotRunning {
                msg: format!("profile {profile_id} is not started"),
            });
        };
        let cfg = slot.shared.cfg.clone();
        self.stop_slot(slot);
        let r = open_engine(&cfg, &self.access).and_then(|e| Ok(e.request_full_fetch()?));
        if let Err(e) = &r {
            warn!("profile {profile_id}: full fetch not scheduled: {e}");
        } else {
            info!("profile {profile_id}: full fetch scheduled");
        }
        self.start(cfg)?;
        r
    }

    /// The newest log lines (at most `max`), oldest first.
    pub fn recent_log_lines(&self, max: u32) -> Vec<String> {
        logging::recent_lines(max as usize)
    }

    /// The log files on disk, oldest first.
    pub fn log_files(&self) -> Vec<String> {
        logging::log_files()
    }

    /// Write a line from the app into the same log.
    pub fn log(&self, level: String, message: String) {
        match level.as_str() {
            "error" => error!(target: "app", "{message}"),
            "warn" => warn!(target: "app", "{message}"),
            _ => info!(target: "app", "{message}"),
        }
    }
}

impl Drop for LiveSync {
    fn drop(&mut self) {
        let slots: Vec<Slot> = lock(&self.slots).drain().map(|(_, s)| s).collect();
        for s in slots {
            let _ = s.stop.send(true);
        }
    }
}
