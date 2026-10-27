//! The engine host: one Tokio runtime, N profiles, one supervisor task each.
//!
//! A supervisor opens the profile's engine, runs it (two-way, or read-only
//! when the profile is pull-only) until told to stop, and reopens it with a
//! growing delay when it ends with an error. Gate refusals and network
//! trouble never end an engine (the daemon waits and re-checks by itself);
//! what ends one is local and fatal, such as an index error, and is shown as
//! `Failed` with the error until the next attempt.

use crate::folders::FolderList;
use crate::profile::{paths_overlap, ProfileConfig};
use crate::status::{self, GateResult, ProfileStatus};
use crate::{logging, LiveSyncError};
use livesync_engine::engine::status_from_index;
use livesync_engine::engine::{AccessCheck, STATE_LOCK};
use livesync_engine::platform::{LockError, LockFile};
use livesync_engine::{Engine, EngineError, Index};
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
/// How long a stopped engine may take to let go of its state folder.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(30);
/// A run this long counts as healthy: the next failure starts the delays over.
const HEALTHY_RUN: Duration = Duration::from_secs(10 * 60);
/// How long a folder listing may take (a slow mobile link, a large database).
const LIST_TIMEOUT: Duration = Duration::from_secs(90);

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
    /// The running engine task, so a stop that times out can cancel it too
    /// (the supervisor alone is not enough: the task would run on, detached).
    run_abort: Mutex<Option<tokio::task::AbortHandle>>,
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
    /// Serialises start, stop and the full-fetch restart: callers come from
    /// several threads (the service's worker, the UI).
    lifecycle: Mutex<()>,
    /// Profiles whose engine had not let go of its state folder when it was
    /// stopped: id -> state folder.
    draining: Mutex<HashMap<String, String>>,
    /// The stop and release grace periods.
    stop_timeouts: Mutex<(Duration, Duration)>,
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
                // instead of taking the supervisor down with it. Aborting it
                // cancels the engine's push side too (`Engine::run`).
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
                *lock(&shared.run_abort) = Some(run.abort_handle());
                let result = run.await;
                *lock(&shared.run_abort) = None;
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

fn in_quiet_wait(e: &Engine) -> bool {
    e.status().map(|st| st.rebuild_wait.is_some()).unwrap_or(false)
}

/// Wait until no engine holds the state folder's lock (the engine's own
/// `flock` on `lsync.lock`, which conflicts within one process too).
fn wait_released(state_dir: &str, timeout: Duration) -> bool {
    let path = Path::new(state_dir).join(STATE_LOCK);
    if !Path::new(state_dir).exists() {
        return true;
    }
    let end = Instant::now() + timeout;
    loop {
        match LockFile::try_acquire(&path) {
            Ok(_held) => return true,
            Err(LockError::Busy(_)) if Instant::now() < end => std::thread::sleep(Duration::from_millis(100)),
            Err(LockError::Busy(_)) => return false,
            // Cannot lock at all (should not happen in app storage): nothing to wait for.
            Err(_) => return true,
        }
    }
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

    /// Stop a slot's supervisor and engine, and wait until the engine has
    /// let go of its state folder. Returns false if it still holds it after
    /// the grace period (the caller must not delete the state then).
    fn stop_slot(&self, slot: Slot) -> bool {
        let _ = slot.stop.send(true);
        let mut task = slot.task;
        let finished = self
            .rt
            .block_on(async { tokio::time::timeout(self.timeouts().0, &mut task).await.is_ok() });
        if !finished {
            warn!(
                "profile {}: the engine did not stop within {}s; cancelling it",
                slot.shared.cfg.id,
                self.timeouts().0.as_secs()
            );
            // Cancelling is like a crash without the exit: every batch and
            // pass is crash-safe (`since` only moves after a whole batch).
            if let Some(run) = lock(&slot.shared.run_abort).take() {
                run.abort();
            }
            task.abort();
            *lock(&slot.shared.engine) = None;
        }
        let released = wait_released(&slot.shared.cfg.state_dir, self.timeouts().1);
        if !released {
            error!(
                "profile {}: the engine still holds its state folder {}s after stopping",
                slot.shared.cfg.id,
                self.timeouts().1.as_secs()
            );
        }
        if !finished {
            slot.shared.set_state(
                RunState::Stopped,
                Some(
                    if released {
                        "cancelled while stopping"
                    } else {
                        "still winding down"
                    }
                    .into(),
                ),
            );
        }
        released
    }

    fn start_locked(&self, cfg: ProfileConfig) -> Result<(), LiveSyncError> {
        cfg.validate().map_err(|msg| LiveSyncError::Invalid { msg })?;
        let previous = {
            let mut slots = lock(&self.slots);
            if slots.get(&cfg.id).is_some_and(|s| s.shared.cfg == cfg) {
                // Already running with exactly these settings.
                return Ok(());
            }
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
            run_abort: Mutex::new(None),
        });
        let span = tracing::info_span!("profile", id = %cfg.id);
        let task = self.rt.spawn(supervise(shared.clone(), rx).instrument(span));
        // A new slot's own stop checks the same state lock again.
        lock(&self.draining).remove(&cfg.id);
        lock(&self.slots).insert(cfg.id.clone(), Slot { shared, stop: tx, task });
        Ok(())
    }

    fn stop_locked(&self, profile_id: &str) -> bool {
        let slot = lock(&self.slots).remove(profile_id);
        match slot {
            Some(slot) => {
                let dir = slot.shared.cfg.state_dir.clone();
                let released = self.stop_slot(slot);
                if !released {
                    // Remembered, so a later stop checks again instead of
                    // reporting a profile without a slot as released.
                    lock(&self.draining).insert(profile_id.to_string(), dir);
                }
                released
            }
            None => self.drained(profile_id),
        }
    }

    /// For a profile without a slot: whether an engine that did not let go
    /// at its stop has done so by now.
    fn drained(&self, profile_id: &str) -> bool {
        let Some(dir) = lock(&self.draining).get(profile_id).cloned() else {
            return true;
        };
        let released = wait_released(&dir, Duration::from_secs(2));
        if released {
            lock(&self.draining).remove(profile_id);
        }
        released
    }

    fn timeouts(&self) -> (Duration, Duration) {
        *lock(&self.stop_timeouts)
    }

    /// Shorter stop and release grace periods, for tests.
    #[doc(hidden)]
    pub fn set_stop_timeouts_for_tests(&self, stop: Duration, release: Duration) {
        *lock(&self.stop_timeouts) = (stop, release);
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
            lifecycle: Mutex::new(()),
            draining: Mutex::new(HashMap::new()),
            stop_timeouts: Mutex::new((STOP_TIMEOUT, RELEASE_TIMEOUT)),
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
        let _serial = lock(&self.lifecycle);
        self.start_locked(cfg)
    }

    /// The user confirmed a mass-delete safety stop: the next full push pass,
    /// and only that one, deletes the files the stop showed
    /// (`Engine::allow_mass_delete_once`); it is asked for at once. Files
    /// that went missing since are judged as usual. `Invalid` when no
    /// mass-delete stop is shown.
    pub fn allow_mass_delete_once(&self, profile_id: String) -> Result<(), LiveSyncError> {
        let shared = self.slot_shared(&profile_id).ok_or_else(|| LiveSyncError::NotRunning {
            msg: format!("profile {profile_id} is not started"),
        })?;
        if shared.cfg.pull_only {
            return Err(LiveSyncError::Invalid {
                msg: "a pull-only profile never deletes anything remotely".into(),
            });
        }
        let engine = shared.engine().ok_or_else(|| LiveSyncError::NotRunning {
            msg: format!("no engine is running for profile {profile_id}"),
        })?;
        if !engine.allow_mass_delete_once()? {
            return Err(LiveSyncError::Invalid {
                msg: "no mass-delete safety stop is shown; nothing to allow".into(),
            });
        }
        warn!("profile {profile_id}: the deletions the safety stop showed are allowed for the next full push pass (confirmed by the user)");
        Ok(())
    }

    /// Stop a profile's engine: wait up to 15 s for it to wind down, cancel
    /// it after that, and wait until it let go of its state folder. Returns
    /// false if it still holds the folder (do not delete the state then).
    pub fn stop(&self, profile_id: String) -> bool {
        let _serial = lock(&self.lifecycle);
        self.stop_locked(&profile_id)
    }

    pub fn stop_all(&self) {
        let _serial = lock(&self.lifecycle);
        let slots: Vec<Slot> = lock(&self.slots).drain().map(|(_, s)| s).collect();
        for s in &slots {
            let _ = s.stop.send(true);
        }
        for s in slots {
            let (id, dir) = (s.shared.cfg.id.clone(), s.shared.cfg.state_dir.clone());
            if !self.stop_slot(s) {
                lock(&self.draining).insert(id, dir);
            }
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
                // A nudge restarts a rebuild's quiet wait; a flapping network
                // must not hold pushes back indefinitely.
                Some(e) if in_quiet_wait(&e) => info!("profile {}: in a rebuild wait; not nudged", s.cfg.id),
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
        let push = engine.as_ref().map(|e| e.push_report());
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
        let mut out = status::from_engine(&profile_id, state, message, pull_only, snap.as_ref(), session.as_ref());
        status::add_push(&mut out, push.as_ref());
        out.mass_delete_allowed = engine.as_ref().is_some_and(|e| e.mass_delete_allowed());
        out.deletes_waiting = engine.as_ref().map_or(0, |e| e.deletes_waiting());
        out
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

    /// The remote's top-level folders with their live file counts and sizes,
    /// plus the files at the vault root, for picking a folder scope; the vault
    /// folder's own top-level folders are merged in (local-only ones marked)
    /// when it exists. Reads file metadata documents only, never chunks, and
    /// never writes to the remote. Works for settings that are not saved yet
    /// (no engine, no index). Refuses a database with encrypted paths.
    pub fn list_folders(&self, cfg: ProfileConfig) -> Result<FolderList, LiveSyncError> {
        let url = cfg.url.trim();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(LiveSyncError::Invalid {
                msg: "the server URL must start with https:// or http://".into(),
            });
        }
        if cfg.database.trim().is_empty() {
            return Err(LiveSyncError::Invalid {
                msg: "no database name".into(),
            });
        }
        let couch = Couch::new(&cfg.remote()).map_err(|e| LiveSyncError::Invalid { msg: e.to_string() })?;
        let listed = self
            .rt
            .block_on(async { tokio::time::timeout(LIST_TIMEOUT, livesync_engine::list_remote_folders(&couch)).await });
        let mut l = match listed {
            Ok(Ok(l)) => l,
            Ok(Err(EngineError::Remote(e))) => {
                warn!("listing the folders of {}: {e}", cfg.database);
                return Err(LiveSyncError::Remote {
                    msg: crate::explain_remote(&e),
                });
            }
            Ok(Err(e)) => {
                warn!("listing the folders of {}: {e}", cfg.database);
                return Err(e.into());
            }
            Err(_) => {
                return Err(LiveSyncError::Remote {
                    msg: format!(
                        "the server did not list its folders within {} s",
                        LIST_TIMEOUT.as_secs()
                    ),
                })
            }
        };
        let vault = Path::new(&cfg.vault_path);
        if vault.is_absolute() {
            l.merge_local(&livesync_engine::local_top_folders(vault));
        }
        info!(
            "listed {} folder(s) of {}: {} files, {} documents read, {} bytes in {} ms",
            l.folders.len(),
            cfg.database,
            l.files,
            l.docs_read,
            l.bytes_fetched,
            l.elapsed_ms
        );
        Ok(l.into())
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
        // Held throughout, so no start or stop (a pause, a switch to
        // pull-only) can land between taking the slot down and restarting it.
        let _serial = lock(&self.lifecycle);
        let slot = lock(&self.slots).remove(&profile_id);
        let Some(slot) = slot else {
            return Err(LiveSyncError::NotRunning {
                msg: format!("profile {profile_id} is not started"),
            });
        };
        let cfg = slot.shared.cfg.clone();
        if !self.stop_slot(slot) {
            self.start_locked(cfg)?;
            return Err(LiveSyncError::Busy {
                msg: "the engine did not stop; the full fetch was not scheduled".into(),
            });
        }
        let r = open_engine(&cfg, &self.access).and_then(|e| Ok(e.request_full_fetch()?));
        if let Err(e) = &r {
            warn!("profile {profile_id}: full fetch not scheduled: {e}");
        } else {
            info!("profile {profile_id}: full fetch scheduled");
        }
        self.start_locked(cfg)?;
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
