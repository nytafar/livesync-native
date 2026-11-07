//! The engine host: one Tokio runtime, N vaults, one supervisor task each.
//!
//! A supervisor opens the vault's engine, runs it (two-way, or read-only
//! when the vault is pull-only) until told to stop, and reopens it with a
//! growing delay when it ends with an error. Gate refusals and network
//! trouble never end an engine (the daemon waits and re-checks by itself);
//! what ends one is local and fatal, such as an index error, and is shown as
//! `Failed` with the error until the next attempt.
//!
//! The lifecycle calls (`start`, `stop`, `fetch_from_remote`, the connect
//! test and the folder listing) block. Call them from a thread that is not
//! running async code: the app's worker, or `spawn_blocking` in a service.

use crate::cancel::Cancel;
use crate::paths::paths_overlap;
use crate::run::{check_url, RunConfig};
use crate::{explain_remote, HostError};
use livesync_engine::engine::status_from_index;
use livesync_engine::engine::{AccessCheck, STATE_LOCK};
use livesync_engine::platform::{LockError, LockFile};
use livesync_engine::{
    Confirmation, Engine, EngineError, FolderListing, GateReport, Index, PullReport, PushReport, Status,
};
use livesync_remote::{Couch, SharedClient};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;
use tracing::{error, info, warn, Instrument};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// No engine for this vault.
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

/// Run-state changes. Called on an engine thread: implementations must
/// return quickly. A panic in it is caught and logged.
pub trait StateListener: Send + Sync {
    fn on_state_changed(&self, id: String, state: RunState, message: Option<String>);
}

/// The front-end's answer to "is the whole vault still visible to this
/// process?" (Android: all-files access is still granted; macOS: TCC).
/// Asked before any pass that could delete remote documents; while it
/// reports a problem, missing files are never treated as deleted.
pub trait VaultAccess: Send + Sync {
    /// `None` when the vault is fully accessible, otherwise the reason.
    fn problem(&self, vault_path: String) -> Option<String>;
}

/// How long `stop` waits for an engine to wind down before abandoning it.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(15);
/// First and longest delay before reopening an engine that failed.
pub const RETRY_FIRST: Duration = Duration::from_secs(15);
pub const RETRY_MAX: Duration = Duration::from_secs(15 * 60);
/// How long a stopped engine may take to let go of its state folder.
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(30);
/// A run this long counts as healthy: the next failure starts the delays over.
pub const HEALTHY_RUN: Duration = Duration::from_secs(10 * 60);
/// How long a folder listing may take (a slow mobile link, a large database).
pub const LIST_TIMEOUT: Duration = Duration::from_secs(90);
/// How long the connect test waits for the gate.
pub const CONNECT_TEST_TIMEOUT: Duration = Duration::from_secs(60);

type Listener = Arc<Mutex<Option<Arc<dyn StateListener>>>>;
type Access = Arc<Mutex<Option<Arc<dyn VaultAccess>>>>;

/// Called with every engine a vault runs (or a command runs on), as soon as
/// it is opened and before it syncs: with the vault's ID. Must return quickly.
pub type EngineHook = Arc<dyn Fn(&str, &Engine) + Send + Sync>;
type Hook = Arc<Mutex<Option<EngineHook>>>;

fn call_hook(hook: &Hook, id: &str, engine: &Engine) {
    let h = lock(hook).clone();
    if let Some(h) = h {
        if std::panic::catch_unwind(AssertUnwindSafe(|| h(id, engine))).is_err() {
            warn!("the engine hook failed");
        }
    }
}

/// One HTTP client (and so one connection pool and its TLS sessions) per
/// server, shared by every vault on it (spec D6). Keyed by the server URL as
/// accounts compare it; credentials go with each request, not the client.
/// When one vault drops its connections (after sleep, or a silent longpoll,
/// spec D8), the others on that server get new ones too, and still share.
#[derive(Clone, Default)]
pub(crate) struct Pool(Arc<Mutex<HashMap<String, SharedClient>>>);

impl Pool {
    fn couch(&self, cfg: &RunConfig) -> Result<Couch, HostError> {
        let invalid = |e: livesync_remote::RemoteError| HostError::Invalid { msg: e.to_string() };
        let key = crate::config::server_key(&cfg.url);
        let client = {
            let mut clients = lock(&self.0);
            match clients.get(&key) {
                Some(c) => c.clone(),
                None => {
                    let c = livesync_remote::shared_http_client().map_err(invalid)?;
                    clients.insert(key, c.clone());
                    c
                }
            }
        };
        Couch::with_shared_client(&cfg.remote(), client).map_err(invalid)
    }

    /// How many servers have a client (for tests).
    fn servers(&self) -> usize {
        lock(&self.0).len()
    }
}

/// Lock, recovering from poisoning: a panic elsewhere must not wedge every
/// later call (the data behind these locks is always left consistent).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the host knows about one vault, running or not.
#[derive(Debug, Clone)]
pub struct VaultStatus {
    pub state: RunState,
    /// Why it is in this state (the error for `Failed`).
    pub message: Option<String>,
    pub pull_only: bool,
    /// The engine's status, or the index's when no engine runs (`None`
    /// before the first start).
    pub snapshot: Option<Status>,
    /// This engine session's pull counters (running only).
    pub session: Option<PullReport>,
    /// This engine session's push totals (running only).
    pub push: Option<PushReport>,
    pub mass_delete_allowed: bool,
    /// That confirmation waits because some of the files it covers cannot
    /// be checked now (a folder or an ignore file that cannot be read).
    pub mass_delete_waits: bool,
    pub deletes_waiting: u64,
}

/// The result of a connect test.
#[derive(Debug)]
pub enum GateCheck {
    Passed(GateReport),
    /// The gate refused or the remote failed.
    Failed(EngineError),
    /// The settings are not usable.
    Invalid(String),
    /// The throwaway engine could not be opened.
    OpenFailed(String),
    /// No answer from the server in time.
    Unreachable(String),
}

/// State shared between a vault's slot and its supervisor task.
struct Shared {
    cfg: RunConfig,
    engine: Mutex<Option<Engine>>,
    state: Mutex<(RunState, Option<String>)>,
    /// Cuts a retry delay short.
    wake: Notify,
    listener: Listener,
    access: Access,
    hook: Hook,
    /// The running engine task, so a stop that times out can cancel it too
    /// (the supervisor alone is not enough: the task would run on, detached).
    run_abort: Mutex<Option<tokio::task::AbortHandle>>,
    pool: Pool,
}

impl Shared {
    fn set_state(&self, state: RunState, message: Option<String>) {
        *lock(&self.state) = (state, message.clone());
        // Call outside the lock, and never let a failing callback (a panic
        // once it crosses back from foreign code) take the supervisor down.
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

pub struct Host {
    /// The runtime when the host owns it (the app); `None` when it runs on
    /// the caller's (a service).
    rt: Option<Runtime>,
    handle: Handle,
    slots: Mutex<HashMap<String, Slot>>,
    /// Serialises start, stop and the full-fetch restart: callers come from
    /// several threads (the service's worker, the UI).
    lifecycle: Mutex<()>,
    /// Vaults whose engine had not let go of its state folder when it was
    /// stopped: id -> state folder.
    draining: Mutex<HashMap<String, String>>,
    /// The stop and release grace periods.
    stop_timeouts: Mutex<(Duration, Duration)>,
    listener: Listener,
    access: Access,
    hook: Hook,
    pool: Pool,
    /// The state folder may be a dot-folder inside the vault (the CLI).
    hidden_state: std::sync::atomic::AtomicBool,
}

fn open_engine(cfg: &RunConfig, access: &Access, pool: &Pool) -> Result<Engine, HostError> {
    let mut ec = cfg.engine_config().map_err(|msg| HostError::Invalid { msg })?;
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
    let couch = pool.couch(cfg)?;
    std::fs::create_dir_all(&cfg.state_dir).map_err(|e| HostError::Other {
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
        let pool = shared.pool.clone();
        let opened = tokio::task::spawn_blocking(move || open_engine(&cfg, &access, &pool)).await;
        let failure = match opened {
            Ok(Ok(engine)) => {
                call_hook(&shared.hook, &shared.cfg.id, &engine);
                *lock(&shared.engine) = Some(engine.clone());
                let mode = if shared.cfg.pull_only { "pull-only" } else { "two-way" };
                info!("engine started ({mode}) for {}", shared.cfg.vault_path);
                shared.set_state(RunState::Running, Some(mode.into()));
                let started = Instant::now();
                let rx = stop.clone();
                let pull_only = shared.cfg.pull_only;
                let ran = engine.clone();
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
                // `run` can return before it marked itself running (the
                // plugin guard); a wait or flush on it must still see it end.
                ran.mark_ended();
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
        delay = next_delay(delay);
    }
    info!("engine stopped");
    shared.set_state(RunState::Stopped, None);
}

/// The delay after `delay`: doubled, at most [`RETRY_MAX`].
pub fn next_delay(delay: Duration) -> Duration {
    (delay * 2).min(RETRY_MAX)
}

fn in_quiet_wait(e: &Engine) -> bool {
    e.status().map(|st| st.rebuild_wait.is_some()).unwrap_or(false)
}

/// Wait until no engine holds the state folder's lock (the engine's own
/// `flock` on `lsync.lock`, which conflicts within one process too).
/// Blocks; true once it is free (or when there is nothing to wait for).
pub fn wait_released(state_dir: &str, timeout: Duration) -> bool {
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

impl Host {
    /// A host with a runtime of its own (two worker threads).
    pub fn new() -> Result<Host, HostError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("livesync")
            .enable_all()
            .build()
            .map_err(|e| HostError::Other { msg: e.to_string() })?;
        let handle = rt.handle().clone();
        Ok(Self::build(Some(rt), handle))
    }

    /// A host whose supervisors run on the caller's runtime (a service that
    /// also serves a control socket from it). Its blocking calls must still
    /// come from outside async code (`spawn_blocking`).
    pub fn on_runtime(handle: Handle) -> Host {
        Self::build(None, handle)
    }

    fn build(rt: Option<Runtime>, handle: Handle) -> Host {
        Host {
            rt,
            handle,
            slots: Mutex::new(HashMap::new()),
            lifecycle: Mutex::new(()),
            draining: Mutex::new(HashMap::new()),
            stop_timeouts: Mutex::new((STOP_TIMEOUT, RELEASE_TIMEOUT)),
            listener: Arc::new(Mutex::new(None)),
            access: Arc::new(Mutex::new(None)),
            hook: Arc::new(Mutex::new(None)),
            pool: Pool::default(),
            hidden_state: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Allow a vault's state folder in a dot-folder inside its vault folder,
    /// as the CLI always did (see [`RunConfig::validate_with`]).
    pub fn allow_hidden_state(&self) {
        self.hidden_state.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn validate(&self, cfg: &RunConfig) -> Result<(), HostError> {
        cfg.validate_with(self.hidden_state.load(std::sync::atomic::Ordering::SeqCst))
            .map_err(|msg| HostError::Invalid { msg })
    }

    /// Open an engine for `cfg` outside any slot, for one command run by a
    /// service on behalf of a client (`lsync sync` while the service runs
    /// the vault): with the vault access check and the server's shared
    /// connection pool, like a supervised engine. The caller must have
    /// stopped the vault's slot first; the engine's own locks refuse a
    /// second engine on the same folders (`Busy`). Blocks.
    pub fn open_engine(&self, cfg: &RunConfig) -> Result<Engine, HostError> {
        self.validate(cfg)?;
        let engine = open_engine(cfg, &self.access, &self.pool)?;
        call_hook(&self.hook, &cfg.id, &engine);
        Ok(engine)
    }

    /// How many servers the shared connection pools are for.
    #[doc(hidden)]
    pub fn pooled_servers(&self) -> usize {
        self.pool.servers()
    }

    /// The runtime the supervisors run on.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    fn slot_shared(&self, id: &str) -> Option<Arc<Shared>> {
        lock(&self.slots).get(id).map(|s| s.shared.clone())
    }

    fn running_engine(&self, id: &str) -> Result<Engine, HostError> {
        self.slot_shared(id)
            .and_then(|s| s.engine())
            .ok_or_else(|| HostError::NotRunning {
                msg: format!("no engine is running for profile {id}"),
            })
    }

    /// The running engine of a vault, for a front-end that serves more than
    /// the host's own calls (a control socket).
    pub fn engine(&self, id: &str) -> Option<Engine> {
        self.slot_shared(id).and_then(|s| s.engine())
    }

    /// A vault's run state and why, without touching its index; `None`
    /// when it has no slot.
    pub fn state(&self, id: &str) -> Option<(RunState, Option<String>)> {
        self.slot_shared(id).map(|s| lock(&s.state).clone())
    }

    /// The settings a slot was started with.
    pub fn config(&self, id: &str) -> Option<RunConfig> {
        self.slot_shared(id).map(|s| s.cfg.clone())
    }

    /// Stop a slot's supervisor and engine, and wait until the engine has
    /// let go of its state folder. Returns false if it still holds it after
    /// the grace period (the caller must not delete the state then).
    fn stop_slot(&self, slot: Slot) -> bool {
        let _ = slot.stop.send(true);
        let mut task = slot.task;
        let finished = self
            .handle
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

    fn start_locked(&self, cfg: RunConfig) -> Result<(), HostError> {
        self.validate(&cfg)?;
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
                    return Err(HostError::Busy {
                        msg: format!(
                            "the vault folder overlaps profile {id}'s ({}); vault folders may not be the same or inside each other",
                            o.vault_path
                        ),
                    });
                }
                if paths_overlap(&o.state_dir, &cfg.state_dir) {
                    return Err(HostError::Busy {
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
            hook: self.hook.clone(),
            run_abort: Mutex::new(None),
            pool: self.pool.clone(),
        });
        let span = tracing::info_span!("profile", id = %cfg.id);
        let task = self.handle.spawn(supervise(shared.clone(), rx).instrument(span));
        // A new slot's own stop checks the same state lock again.
        lock(&self.draining).remove(&cfg.id);
        lock(&self.slots).insert(cfg.id.clone(), Slot { shared, stop: tx, task });
        Ok(())
    }

    fn stop_locked(&self, id: &str) -> bool {
        let slot = lock(&self.slots).remove(id);
        match slot {
            Some(slot) => {
                let dir = slot.shared.cfg.state_dir.clone();
                let released = self.stop_slot(slot);
                if !released {
                    // Remembered, so a later stop checks again instead of
                    // reporting a vault without a slot as released.
                    lock(&self.draining).insert(id.to_string(), dir);
                }
                released
            }
            None => self.drained(id),
        }
    }

    /// For a vault without a slot: whether an engine that did not let go
    /// at its stop has done so by now.
    fn drained(&self, id: &str) -> bool {
        let Some(dir) = lock(&self.draining).get(id).cloned() else {
            return true;
        };
        let released = wait_released(&dir, Duration::from_secs(2));
        if released {
            lock(&self.draining).remove(id);
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

    /// Install the vault access check every engine asks before a pass that
    /// could delete remote documents (see [`VaultAccess`]).
    pub fn set_vault_access(&self, access: Arc<dyn VaultAccess>) {
        *lock(&self.access) = Some(access);
    }

    /// Be told of every engine a vault runs, before it syncs (see
    /// [`EngineHook`]): a service subscribes to its events there, so it
    /// misses none.
    pub fn set_engine_hook(&self, hook: EngineHook) {
        *lock(&self.hook) = Some(hook);
    }

    /// Receive run-state changes of every vault.
    pub fn set_listener(&self, listener: Arc<dyn StateListener>) {
        *lock(&self.listener) = Some(listener);
    }

    /// Start (or restart with new settings) the engine for a vault. Refused
    /// when another running vault's folder is the same as, inside or above
    /// this one, or shares its state folder.
    pub fn start(&self, cfg: RunConfig) -> Result<(), HostError> {
        let _serial = lock(&self.lifecycle);
        self.start_locked(cfg)
    }

    /// The user confirmed a mass-delete safety stop: `safety_stop` is the
    /// stop the dialog showed (the status's `safety_stop`, as it was when
    /// the dialog opened). The next full push pass, and only that one,
    /// deletes the files that stop showed (`Engine::allow_mass_delete_once`);
    /// it is asked for at once. Files that went missing since are judged as
    /// usual. `StopChanged` when the stop shown now is another one (nothing
    /// is allowed; show it and ask again), `Invalid` when no mass-delete
    /// stop is shown.
    pub fn allow_mass_delete_once(&self, id: &str, safety_stop: &str) -> Result<(), HostError> {
        let shared = self.slot_shared(id).ok_or_else(|| HostError::NotRunning {
            msg: format!("profile {id} is not started"),
        })?;
        if shared.cfg.pull_only {
            return Err(HostError::Invalid {
                msg: "a pull-only profile never deletes anything remotely".into(),
            });
        }
        let engine = shared.engine().ok_or_else(|| HostError::NotRunning {
            msg: format!("no engine is running for profile {id}"),
        })?;
        match engine.allow_mass_delete_once(safety_stop)? {
            Confirmation::Allowed => {}
            Confirmation::NoStop => {
                return Err(HostError::Invalid {
                    msg: "no mass-delete safety stop is shown; nothing to allow".into(),
                })
            }
            Confirmation::Changed(now) => {
                let now = now.split_once(' ').map_or(now.as_str(), |(_, text)| text);
                return Err(HostError::StopChanged {
                    msg: format!(
                        "the safety stop changed after it was shown, so nothing was allowed; it now says: {now}"
                    ),
                });
            }
        }
        warn!("profile {id}: the deletions the safety stop showed are allowed for the next full push pass (confirmed by the user)");
        Ok(())
    }

    /// Stop a vault's engine: wait up to 15 s for it to wind down, cancel
    /// it after that, and wait until it let go of its state folder. Returns
    /// false if it still holds the folder (do not delete the state then).
    pub fn stop(&self, id: &str) -> bool {
        let _serial = lock(&self.lifecycle);
        self.stop_locked(id)
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

    /// IDs of the vaults that have an engine slot (running, starting or
    /// waiting to retry), sorted.
    pub fn active(&self) -> Vec<String> {
        let mut v: Vec<String> = lock(&self.slots).keys().cloned().collect();
        v.sort();
        v
    }

    /// "Sync now": reconnect at once and rescan the vault; a failed engine is
    /// retried at once.
    pub fn sync_now(&self, id: &str) -> Result<(), HostError> {
        let shared = self.slot_shared(id).ok_or_else(|| HostError::NotRunning {
            msg: format!("profile {id} is not started"),
        })?;
        match shared.engine() {
            Some(e) => e.network_changed(true),
            None => shared.wake.notify_one(),
        }
        Ok(())
    }

    /// The network came up or went down. On up, every engine reconnects and
    /// rescans, and failed engines retry at once.
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

    /// Status of a vault, running or not. For a vault without an engine the
    /// index is read from `state_dir` (no network).
    pub fn status(&self, id: &str, vault_path: &str, state_dir: &str) -> VaultStatus {
        let shared = self.slot_shared(id);
        let (state, message, pull_only) = match &shared {
            Some(s) => {
                let (st, msg) = lock(&s.state).clone();
                (st, msg, s.cfg.pull_only)
            }
            None => (RunState::Stopped, None, false),
        };
        let engine = shared.as_ref().and_then(|s| s.engine());
        let push = engine.as_ref().map(|e| e.push_report());
        let (snapshot, session) = match &engine {
            Some(e) => (e.status().ok(), Some(e.report())),
            None => {
                let index_path = PathBuf::from(state_dir).join("index.sqlite");
                let snap = if index_path.exists() {
                    Index::open(&index_path)
                        .ok()
                        .and_then(|ix| status_from_index(&ix, Path::new(vault_path), &index_path).ok())
                } else {
                    None
                };
                (snap, None)
            }
        };
        VaultStatus {
            state,
            message,
            pull_only,
            snapshot,
            session,
            push,
            mass_delete_allowed: engine.as_ref().is_some_and(|e| e.mass_delete_allowed()),
            mass_delete_waits: engine.as_ref().is_some_and(|e| e.mass_delete_waits()),
            deletes_waiting: engine.as_ref().map_or(0, |e| e.deletes_waiting()),
        }
    }

    /// Run the connect gate for settings that are not saved yet, against a
    /// throwaway index and vault under `scratch_dir` (removed afterwards).
    /// Reads only; never writes to the remote or to the real vault.
    pub fn connect_test(&self, cfg: &RunConfig, scratch_dir: &str) -> GateCheck {
        if let Err(msg) = cfg.validate() {
            return GateCheck::Invalid(msg);
        }
        let scratch = PathBuf::from(scratch_dir).join(format!("connect-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let mut test = cfg.clone();
        test.vault_path = scratch.join("vault").display().to_string();
        test.state_dir = scratch.join("state").display().to_string();
        let result = self.handle.block_on(async {
            let access = self.access.clone();
            // A throwaway pool: the test's settings are not saved yet.
            let engine = match tokio::task::spawn_blocking(move || open_engine(&test, &access, &Pool::default())).await
            {
                Ok(Ok(e)) => e,
                Ok(Err(e)) => return GateCheck::OpenFailed(e.to_string()),
                Err(e) => return GateCheck::OpenFailed(e.to_string()),
            };
            match tokio::time::timeout(CONNECT_TEST_TIMEOUT, engine.connect_gate(false)).await {
                Ok(Ok(g)) => GateCheck::Passed(g),
                Ok(Err(e)) => GateCheck::Failed(e),
                Err(_) => GateCheck::Unreachable(format!(
                    "no answer from the server within {} s",
                    CONNECT_TEST_TIMEOUT.as_secs()
                )),
            }
        });
        let _ = std::fs::remove_dir_all(&scratch);
        result
    }

    /// The remote's top-level folders with their live file counts and sizes,
    /// plus the files at the vault root, for picking a folder scope; the vault
    /// folder's own top-level folders are merged in (local-only ones marked)
    /// when it exists. Reads file metadata documents only, never chunks, and
    /// never writes to the remote. Works for settings that are not saved yet
    /// (no engine, no index). Refuses a database with encrypted paths.
    /// Cancelling `cancel` ends it at once with `Other`.
    pub fn list_folders(&self, cfg: &RunConfig, cancel: Option<&Cancel>) -> Result<FolderListing, HostError> {
        check_url(&cfg.url).map_err(|msg| HostError::Invalid { msg })?;
        if cfg.database.trim().is_empty() {
            return Err(HostError::Invalid {
                msg: "no database name".into(),
            });
        }
        let couch = Couch::new(&cfg.remote()).map_err(|e| HostError::Invalid { msg: e.to_string() })?;
        let listed = self.handle.block_on(async {
            let listing = tokio::time::timeout(LIST_TIMEOUT, livesync_engine::list_remote_folders(&couch));
            match cancel {
                // Dropping the listing closes its connection: nothing more is fetched.
                Some(c) => tokio::select! {
                    l = listing => Some(l),
                    _ = c.cancelled() => None,
                },
                None => Some(listing.await),
            }
        });
        let Some(listed) = listed else {
            info!("listing the folders of {} cancelled", cfg.database);
            return Err(HostError::Other {
                msg: "the listing was cancelled".into(),
            });
        };
        let mut l = match listed {
            Ok(Ok(l)) => l,
            Ok(Err(EngineError::Remote(e))) => {
                warn!("listing the folders of {}: {e}", cfg.database);
                return Err(HostError::Remote {
                    msg: explain_remote(&e),
                });
            }
            Ok(Err(e)) => {
                warn!("listing the folders of {}: {e}", cfg.database);
                return Err(e.into());
            }
            Err(_) => {
                return Err(HostError::Remote {
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
        Ok(l)
    }

    /// Accept changed PREFERRED tweak values on the remote (the gate's
    /// "tweaks changed" refusal) for a running vault, then reconnect.
    pub fn adopt_tweaks(&self, id: &str) -> Result<Result<GateReport, EngineError>, HostError> {
        let engine = self.running_engine(id)?;
        let r = self.handle.block_on(engine.connect_gate(true));
        engine.nudge();
        Ok(r)
    }

    /// Start over from the remote (upstream's "Fetch"): forget every
    /// revision and `since`, keep the disk hashes (clean files are refreshed,
    /// edited ones kept), fetch everything, and in two-way mode mark this
    /// device resolved. This is how a milestone lock without a rebuild is
    /// cleared. The engine is restarted.
    pub fn fetch_from_remote(&self, id: &str) -> Result<(), HostError> {
        // Held throughout, so no start or stop (a pause, a switch to
        // pull-only) can land between taking the slot down and restarting it.
        let _serial = lock(&self.lifecycle);
        let slot = lock(&self.slots).remove(id);
        let Some(slot) = slot else {
            return Err(HostError::NotRunning {
                msg: format!("profile {id} is not started"),
            });
        };
        let cfg = slot.shared.cfg.clone();
        if !self.stop_slot(slot) {
            self.start_locked(cfg)?;
            return Err(HostError::Busy {
                msg: "the engine did not stop; the full fetch was not scheduled".into(),
            });
        }
        // A pull-only profile fetches read-only, so the plugin guard is
        // for two-way profiles only, as in their runs.
        let pull_only = cfg.pull_only;
        let r = open_engine(&cfg, &self.access, &self.pool).and_then(|e| {
            if !pull_only {
                e.check_plugin_guard()?;
            }
            Ok(e.request_full_fetch()?)
        });
        if let Err(e) = &r {
            warn!("profile {id}: full fetch not scheduled: {e}");
        } else {
            info!("profile {id}: full fetch scheduled");
        }
        self.start_locked(cfg)?;
        r
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let slots: Vec<Slot> = lock(&self.slots).drain().map(|(_, s)| s).collect();
        for s in slots {
            let _ = s.stop.send(true);
        }
        // An owned runtime goes down with the host, as before the move.
        drop(self.rt.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delays_grow_from_15_s_to_15_min() {
        let mut d = RETRY_FIRST;
        let mut seen = vec![d.as_secs()];
        for _ in 0..8 {
            d = next_delay(d);
            seen.push(d.as_secs());
        }
        assert_eq!(seen, vec![15, 30, 60, 120, 240, 480, 900, 900, 900]);
    }
}
