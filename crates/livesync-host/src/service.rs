//! One service process hosting a set of vaults (spec D6): what `lsync serve`
//! and `lsync daemon` run, and what the control socket ([`crate::control`])
//! talks to.
//!
//! The service keeps, per vault, what should be true (its settings, whether
//! it is paused, whether a client's command is running on it) and makes the
//! [`Host`] match it:
//!
//! - **Reload** rereads the [`Source`] and starts, stops or restarts only
//!   the vaults whose settings changed (the password included).
//! - **Pause** stops a vault's engine (its folders are let go) until
//!   **resume**; a paused vault takes new settings from a reload and starts
//!   with them at resume. Not kept across a restart of the service.
//! - **Commands** (`sync`, `pull`, `fetch`, `connect` for a client): the
//!   vault's engine is stopped, the command runs on an engine of its own
//!   with the same settings, and the vault's engine is started again
//!   (unless it was paused). The engine's locks never meet a second engine,
//!   and the client gets the same report as from a one-shot `lsync`.
//!
//! **Order.** Every change to one vault (reload, pause, resume, a command)
//! holds that vault's control lock, so they happen one after another and
//! each finds the vault in a defined state; a reload takes the locks of the
//! vaults it changes in name order, and nothing else takes two, so they
//! cannot deadlock. A reload waits for a command running on a vault it
//! changes. `status` and `sync-now` never wait for these locks.
//!
//! **Events** come from the host's run-state changes, from the service's
//! own changes, and from watching each running engine's counters and its
//! safety stop and last error (polled; the engine has no callbacks).

use crate::config::{server_key, Config, Dirs, Mode};
use crate::protocol::{
    code, event, CommandParams, CommandResult, CtlError, Event, Hello, ReloadReport, Settings, StatusResult,
    VaultError, VaultView, PROTOCOL,
};
use crate::secret::SecretStores;
use crate::supervisor::{wait_released, RELEASE_TIMEOUT, STOP_TIMEOUT};
use crate::{Host, HostError, RunConfig, RunState, StateListener};
use livesync_engine::{EngineError, PullReport, PushReport};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Mutex as AsyncMutex};
use tracing::{info, warn};

/// How often the engines' counters are looked at for events.
pub const ACTIVITY_POLL: Duration = Duration::from_millis(500);
/// How often each engine's safety stop and last error are read when its
/// counters did not move (they are read at once when they did).
pub const STATUS_POLL: Duration = Duration::from_secs(10);
/// Events kept for a subscriber that is slow to read.
const EVENT_BUFFER: usize = 1024;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One vault as a [`Source`] gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub name: String,
    pub account: Option<String>,
    pub enabled: bool,
    pub folder: String,
    pub state_dir: String,
    /// What the host runs, or why it cannot (no password, ...).
    pub run: Result<RunConfig, String>,
}

impl Planned {
    fn runnable(&self) -> Option<&RunConfig> {
        self.run.as_ref().ok().filter(|_| self.enabled)
    }
}

/// Where the service's vaults come from.
pub trait Source: Send + Sync {
    /// Every vault, enabled or not. `Err` when the config cannot be read:
    /// then nothing changes.
    fn load(&self) -> Result<Vec<Planned>, String>;
    /// For `hello` and the logs: the config file.
    fn describe(&self) -> String;
}

/// The vaults in `lsync.toml`, with their passwords from the secret stores.
pub struct ConfigSource {
    pub path: PathBuf,
    pub dirs: Dirs,
    pub secrets: SecretStores,
    /// Only this vault (`lsync serve --vault NAME`).
    pub only: Option<String>,
}

impl Source for ConfigSource {
    fn load(&self) -> Result<Vec<Planned>, String> {
        let Some(cfg) = Config::load(&self.path, &self.dirs)? else {
            return Err(format!(
                "there is no {}; add a vault with `lsync vault add`",
                self.path.display()
            ));
        };
        if let Some(only) = &self.only {
            if !cfg.vault.contains_key(only) {
                return Err(format!("no vault named {only:?} in {}", self.path.display()));
            }
        }
        let mut out = Vec::new();
        for (name, v) in &cfg.vault {
            if self.only.as_ref().is_some_and(|o| o != name) {
                continue;
            }
            let a = &cfg.account[&v.account];
            // As the CLI decides: an account without a user is a server
            // without authentication and needs no password.
            let has_user = a.username.as_deref().is_some_and(|u| !u.is_empty());
            let password = a.secret_ref(&v.account).and_then(|r| match self.secrets.get(&r) {
                Ok(Some(s)) => Ok(Some(s)),
                Ok(None) if !has_user => Ok(None),
                Ok(None) => Err(format!(
                    "account {:?} has no password in its secret store ({}); set it with `lsync account passwd {}`",
                    v.account,
                    self.secrets.describe(&r),
                    v.account
                )),
                Err(_) if !has_user => Ok(None),
                Err(e) => Err(format!("account {:?}: {e}", v.account)),
            });
            let run = password.and_then(|pw| v.run_config(name, a, pw.as_ref().map(|s| s.expose()), &self.dirs));
            out.push(Planned {
                name: name.clone(),
                account: Some(v.account.clone()),
                enabled: v.enabled,
                folder: v.folder_path(&self.dirs).display().to_string(),
                state_dir: v.state_path(name, &self.dirs).display().to_string(),
                run,
            });
        }
        Ok(out)
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// A fixed set of vaults (`lsync daemon`, whose settings may come from
/// flags and the environment).
pub struct FixedSource {
    pub vaults: Vec<Planned>,
    pub description: String,
}

impl Source for FixedSource {
    fn load(&self) -> Result<Vec<Planned>, String> {
        Ok(self.vaults.clone())
    }

    fn describe(&self) -> String {
        self.description.clone()
    }
}

/// The commands a client can have the service run on a vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Sync,
    Pull,
    Fetch,
    Connect,
}

impl Command {
    pub fn name(self) -> &'static str {
        match self {
            Command::Sync => "sync",
            Command::Pull => "pull",
            Command::Fetch => "fetch",
            Command::Connect => "connect",
        }
    }

    fn writes(self) -> bool {
        matches!(self, Command::Sync | Command::Fetch)
    }
}

/// What the service holds per vault, changed only under `ctl`.
#[derive(Debug, Clone)]
struct View {
    planned: Planned,
    paused: bool,
    command: Option<Command>,
    /// Why the host refused to start it (an overlap, invalid settings).
    start_error: Option<String>,
    removed: bool,
}

struct Entry {
    /// Serialises every change to this vault (see the module docs).
    ctl: Arc<AsyncMutex<()>>,
    view: Mutex<View>,
}

impl Entry {
    fn view(&self) -> View {
        lock(&self.view).clone()
    }
}

/// Forwards the host's run-state changes as events.
struct Forward(broadcast::Sender<Event>);

impl StateListener for Forward {
    fn on_state_changed(&self, id: String, state: RunState, message: Option<String>) {
        let _ = self.0.send(make_event(
            event::STATE,
            Some(id),
            json!({"state": state_name(state), "message": message}),
        ));
    }
}

fn make_event(kind: &str, vault: Option<String>, data: Value) -> Event {
    Event {
        v: PROTOCOL,
        event: kind.to_string(),
        vault,
        ts: crate::protocol::now_ms(),
        data,
    }
}

pub fn state_name(s: RunState) -> &'static str {
    match s {
        RunState::Stopped => "stopped",
        RunState::Starting => "starting",
        RunState::Running => "running",
        RunState::Failed => "failed",
    }
}

fn host_error(e: HostError) -> CtlError {
    match e {
        HostError::Busy { msg } => CtlError::new(code::BUSY, msg),
        HostError::Refused { msg } => CtlError::new(code::REFUSED, msg),
        HostError::NotRunning { msg } => CtlError::new(code::NOT_RUNNING, msg),
        other => CtlError::new(code::FAILED, other.to_string()),
    }
}

fn engine_error(e: EngineError) -> CtlError {
    match e {
        // The refusal itself, as `lsync sync` prints it after "sync refused: ".
        EngineError::Refused(r) => CtlError::new(code::REFUSED, r.to_string()),
        EngineError::Busy(m) => CtlError::new(code::BUSY, m),
        other => CtlError::new(code::FAILED, other.to_string()),
    }
}

/// A server URL without any user or password in it.
pub fn redact_url(url: &str) -> String {
    match crate::run::split_userinfo(url) {
        Some(u) => u.url,
        None => url.trim().to_string(),
    }
}

/// What the service runs a vault with, for clients (no password).
pub fn settings_of(cfg: &RunConfig) -> Settings {
    Settings {
        url: redact_url(&cfg.url),
        username: cfg.username.clone(),
        database: cfg.database.clone(),
        folder: cfg.vault_path.clone(),
        state_dir: cfg.state_dir.clone(),
        scope_mode: format!("{:?}", cfg.scope_mode).to_lowercase(),
        scope_folders: cfg.scope().map(|s| s.prefixes).unwrap_or_default(),
        pull_only: cfg.pull_only,
        ignore_regex: cfg.ignore_regex.clone(),
        device_name: cfg.device_name.clone(),
        max_size_mb: cfg.max_size_mb,
        concurrency: cfg.concurrency,
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

pub struct Service {
    host: Arc<Host>,
    source: Box<dyn Source>,
    vaults: Mutex<BTreeMap<String, Arc<Entry>>>,
    reload_lock: AsyncMutex<()>,
    events: broadcast::Sender<Event>,
    stopping: AtomicBool,
    version: String,
}

impl Service {
    /// A service over `host` (whose state listener it takes), with no
    /// vaults until the first [`Service::reload`].
    pub fn new(host: Arc<Host>, source: Box<dyn Source>, version: &str) -> Arc<Service> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        host.set_listener(Arc::new(Forward(events.clone())));
        Arc::new(Service {
            host,
            source,
            vaults: Mutex::new(BTreeMap::new()),
            reload_lock: AsyncMutex::new(()),
            events,
            stopping: AtomicBool::new(false),
            version: version.to_string(),
        })
    }

    pub fn host(&self) -> &Arc<Host> {
        &self.host
    }

    pub fn hello(&self) -> Hello {
        Hello {
            protocol: PROTOCOL,
            version: self.version.clone(),
            pid: std::process::id(),
            methods: crate::protocol::method::ALL.iter().map(|m| m.to_string()).collect(),
            config: self.source.describe(),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn emit(&self, kind: &str, vault: Option<&str>, data: Value) {
        let _ = self.events.send(make_event(kind, vault.map(str::to_string), data));
    }

    pub fn names(&self) -> Vec<String> {
        lock(&self.vaults).keys().cloned().collect()
    }

    fn entry(&self, name: &str) -> Result<Arc<Entry>, CtlError> {
        // The map's lock is let go before `unknown` takes it again.
        let found = lock(&self.vaults).get(name).cloned();
        found.ok_or_else(|| self.unknown(name))
    }

    fn unknown(&self, name: &str) -> CtlError {
        let names = self.names();
        CtlError::new(
            code::UNKNOWN_VAULT,
            format!(
                "the service runs no vault named {name:?} (it has: {})",
                if names.is_empty() {
                    "none".into()
                } else {
                    names.join(", ")
                }
            ),
        )
    }

    /// The vaults a request is about: one by name, or every one.
    fn targets(&self, vault: Option<&str>) -> Result<Vec<(String, Arc<Entry>)>, CtlError> {
        match vault {
            Some(n) => Ok(vec![(n.to_string(), self.entry(n)?)]),
            None => Ok(lock(&self.vaults).iter().map(|(n, e)| (n.clone(), e.clone())).collect()),
        }
    }

    fn check_running(&self) -> Result<(), CtlError> {
        if self.stopping.load(Ordering::SeqCst) {
            return Err(CtlError::new(code::SHUTTING_DOWN, "the service is stopping"));
        }
        Ok(())
    }

    fn has_slot(&self, name: &str) -> bool {
        self.host.state(name).is_some()
    }

    async fn stop_engine(&self, name: &str) -> bool {
        let host = self.host.clone();
        let id = name.to_string();
        blocking(move || host.stop(&id)).await
    }

    async fn start_engine(&self, cfg: RunConfig) -> Result<(), String> {
        let host = self.host.clone();
        blocking(move || host.start(cfg)).await.map_err(|e| e.to_string())
    }

    /// Reread the config and make the host match it. On a config that
    /// cannot be read nothing changes (`invalid_config`).
    pub async fn reload(self: &Arc<Self>) -> Result<ReloadReport, CtlError> {
        self.check_running()?;
        let _serial = self.reload_lock.lock().await;
        let planned = {
            // The source reads files and secret stores: off the runtime.
            let svc = self.clone();
            let loaded = blocking(move || svc.source.load()).await;
            loaded.map_err(|e| {
                warn!("reload: {e}; nothing changed");
                CtlError::new(code::INVALID_CONFIG, e)
            })?
        };
        let mut wanted: BTreeMap<String, Planned> = planned.into_iter().map(|p| (p.name.clone(), p)).collect();
        let current: BTreeMap<String, Arc<Entry>> = lock(&self.vaults).clone();
        let mut report = ReloadReport::default();
        let mut names: Vec<String> = current.keys().chain(wanted.keys()).cloned().collect();
        names.sort();
        names.dedup();

        // Phase 1: take each changed vault's lock (in name order) and stop
        // its engine, so that phase 2 starts every engine with the other
        // vaults' old engines already gone (two vaults may swap folders).
        struct Change {
            name: String,
            entry: Arc<Entry>,
            _guard: tokio::sync::OwnedMutexGuard<()>,
            new: Planned,
            ran: bool,
        }
        let mut changes: Vec<Change> = Vec::new();
        for name in &names {
            match (current.get(name), wanted.remove(name)) {
                (Some(entry), None) => {
                    let guard = entry.ctl.clone().lock_owned().await;
                    if self.has_slot(name) {
                        self.stop_engine(name).await;
                    }
                    lock(&entry.view).removed = true;
                    lock(&self.vaults).remove(name);
                    info!("vault {name}: removed from the config; stopped");
                    report.stopped.push(name.clone());
                    drop(guard);
                }
                (Some(entry), Some(new)) => {
                    if entry.view().planned == new {
                        report.unchanged.push(name.clone());
                        if let Err(e) = &new.run {
                            report.failed.push(VaultError {
                                vault: name.clone(),
                                message: e.clone(),
                            });
                        }
                        continue;
                    }
                    let guard = entry.ctl.clone().lock_owned().await;
                    let ran = self.has_slot(name);
                    if ran {
                        self.stop_engine(name).await;
                    }
                    changes.push(Change {
                        name: name.clone(),
                        entry: entry.clone(),
                        _guard: guard,
                        new,
                        ran,
                    });
                }
                (None, Some(new)) => {
                    let entry = Arc::new(Entry {
                        ctl: Arc::new(AsyncMutex::new(())),
                        view: Mutex::new(View {
                            planned: new.clone(),
                            paused: false,
                            command: None,
                            start_error: None,
                            removed: false,
                        }),
                    });
                    let guard = entry.ctl.clone().lock_owned().await;
                    lock(&self.vaults).insert(name.clone(), entry.clone());
                    changes.push(Change {
                        name: name.clone(),
                        entry,
                        _guard: guard,
                        new,
                        ran: false,
                    });
                }
                (None, None) => {}
            }
        }

        // Phase 2: start what should run, with the new settings.
        for c in changes {
            let paused = {
                let mut v = lock(&c.entry.view);
                v.planned = c.new.clone();
                v.start_error = None;
                v.paused
            };
            let runs = match c.new.runnable() {
                Some(cfg) if !paused => match self.start_engine(cfg.clone()).await {
                    Ok(()) => true,
                    Err(e) => {
                        warn!("vault {}: not started: {e}", c.name);
                        lock(&c.entry.view).start_error = Some(e.clone());
                        report.failed.push(VaultError {
                            vault: c.name.clone(),
                            message: e,
                        });
                        false
                    }
                },
                _ => false,
            };
            if let Err(e) = &c.new.run {
                report.failed.push(VaultError {
                    vault: c.name.clone(),
                    message: e.clone(),
                });
            }
            match (c.ran, runs) {
                (true, true) => report.restarted.push(c.name.clone()),
                (false, true) => report.started.push(c.name.clone()),
                (true, false) => report.stopped.push(c.name.clone()),
                // Disabled, paused or unusable before and after.
                (false, false) => report.unchanged.push(c.name.clone()),
            }
        }
        for list in [
            &mut report.started,
            &mut report.stopped,
            &mut report.restarted,
            &mut report.unchanged,
        ] {
            list.sort();
        }
        report.failed.sort_by(|a, b| a.vault.cmp(&b.vault));
        info!(
            "reloaded {}: started {:?}, stopped {:?}, restarted {:?}, {} unchanged",
            self.source.describe(),
            report.started,
            report.stopped,
            report.restarted,
            report.unchanged.len()
        );
        self.emit(event::RELOAD, None, serde_json::to_value(&report).unwrap_or_default());
        Ok(report)
    }

    /// Pause one vault, or every vault: its engine stops and lets go of its
    /// folders. Pausing a paused vault does nothing.
    pub async fn pause(&self, vault: Option<&str>) -> Result<Vec<String>, CtlError> {
        self.check_running()?;
        let mut done = Vec::new();
        for (name, entry) in self.targets(vault)? {
            let _guard = entry.ctl.lock().await;
            let v = entry.view();
            if v.removed {
                if vault.is_some() {
                    return Err(self.unknown(&name));
                }
                continue;
            }
            if !v.paused {
                if self.has_slot(&name) && !self.stop_engine(&name).await {
                    warn!("vault {name}: paused, but its engine is still winding down");
                }
                lock(&entry.view).paused = true;
                info!("vault {name}: paused");
                self.emit(event::PAUSED, Some(&name), json!({"paused": true}));
            }
            done.push(name);
        }
        Ok(done)
    }

    /// Resume one vault, or every paused vault, with its current settings.
    pub async fn resume(&self, vault: Option<&str>) -> Result<Vec<String>, CtlError> {
        self.check_running()?;
        let mut done = Vec::new();
        for (name, entry) in self.targets(vault)? {
            let _guard = entry.ctl.lock().await;
            let v = entry.view();
            if v.removed {
                if vault.is_some() {
                    return Err(self.unknown(&name));
                }
                continue;
            }
            if v.paused {
                {
                    let mut w = lock(&entry.view);
                    w.paused = false;
                    w.start_error = None;
                }
                if let Some(cfg) = v.planned.runnable() {
                    if let Err(e) = self.start_engine(cfg.clone()).await {
                        warn!("vault {name}: not started: {e}");
                        lock(&entry.view).start_error = Some(e);
                    }
                }
                info!("vault {name}: resumed");
                self.emit(event::PAUSED, Some(&name), json!({"paused": false}));
            }
            done.push(name);
        }
        Ok(done)
    }

    /// Reconnect and rescan now (a failed engine retries at once). For one
    /// vault, an error says why it cannot; for every vault, the ones that
    /// cannot are left out.
    pub fn sync_now(&self, vault: Option<&str>) -> Result<Vec<String>, CtlError> {
        self.check_running()?;
        let mut done = Vec::new();
        for (name, entry) in self.targets(vault)? {
            let v = entry.view();
            let why = if v.paused {
                Some(CtlError::new(
                    code::PAUSED,
                    format!("vault {name} is paused; `lsync --vault {name} resume` first"),
                ))
            } else if let Some(c) = v.command {
                Some(CtlError::new(
                    code::BUSY,
                    format!("vault {name} is running `{}` for a client", c.name()),
                ))
            } else {
                self.host.sync_now(&name).err().map(|_| self.not_running(&name, &v))
            };
            match why {
                Some(e) if vault.is_some() => return Err(e),
                Some(_) => {}
                None => done.push(name),
            }
        }
        Ok(done)
    }

    fn not_running(&self, name: &str, v: &View) -> CtlError {
        let why = if !v.planned.enabled {
            format!("vault {name} is disabled (`lsync vault enable {name}`)")
        } else if let Err(e) = &v.planned.run {
            format!("vault {name} cannot run: {e}")
        } else if let Some(e) = &v.start_error {
            format!("vault {name} did not start: {e}")
        } else {
            format!("vault {name} is not running")
        };
        CtlError::new(code::NOT_RUNNING, why)
    }

    /// Every vault's (or one's) state and status. Reads indexes: call it
    /// from a blocking thread.
    pub fn status(&self, vault: Option<&str>) -> Result<StatusResult, CtlError> {
        let mut out = Vec::new();
        for (name, entry) in self.targets(vault)? {
            let v = entry.view();
            if v.removed {
                continue;
            }
            let slot = self.host.state(&name);
            let (state, message) = if !v.planned.enabled {
                ("disabled".to_string(), None)
            } else if let Err(e) = &v.planned.run {
                ("invalid".to_string(), Some(e.clone()))
            } else if let Some(c) = v.command {
                ("busy".to_string(), Some(format!("running `{}` for a client", c.name())))
            } else if v.paused {
                ("paused".to_string(), None)
            } else if let (None, Some(e)) = (&slot, &v.start_error) {
                ("invalid".to_string(), Some(e.clone()))
            } else {
                match slot {
                    Some((s, m)) => (state_name(s).to_string(), m),
                    None => ("stopped".to_string(), None),
                }
            };
            let st = self.host.status(&name, &v.planned.folder, &v.planned.state_dir);
            out.push(VaultView {
                name: name.clone(),
                state,
                message,
                paused: v.paused,
                enabled: v.planned.enabled,
                account: v.planned.account.clone(),
                command: v.command.map(|c| c.name().to_string()),
                settings: v.planned.run.as_ref().ok().map(settings_of),
                status: st.snapshot,
                session: st.session,
                push: st.push,
                deletes_waiting: st.deletes_waiting,
                mass_delete_allowed: st.mass_delete_allowed,
            });
        }
        Ok(StatusResult { vaults: out })
    }

    /// Run a command for a client on a vault the service holds (see the
    /// module docs). The vault's engine is back as it was afterwards,
    /// whatever the command's outcome.
    pub async fn command(&self, cmd: Command, p: CommandParams) -> Result<CommandResult, CtlError> {
        self.check_running()?;
        let name = p.vault.clone();
        let entry = self.entry(&name)?;
        let _guard = entry.ctl.lock().await;
        self.check_running()?;
        let v = entry.view();
        if v.removed {
            return Err(self.unknown(&name));
        }
        let cfg = match v.planned.runnable() {
            Some(c) => c.clone(),
            None => return Err(self.not_running(&name, &v)),
        };
        if cmd.writes() && cfg.pull_only {
            return Err(CtlError::new(
                code::FAILED,
                format!(
                    "vault {name} is pull-only (mode = \"pull-only\"), and `{}` writes to the remote; use `lsync pull`, or `lsync vault edit {name} --mode two-way`",
                    cmd.name()
                ),
            ));
        }
        lock(&entry.view).command = Some(cmd);
        self.emit(
            event::COMMAND,
            Some(&name),
            json!({"command": cmd.name(), "phase": "started"}),
        );
        let had_engine = self.has_slot(&name);
        let released = !had_engine || self.stop_engine(&name).await;
        let outcome = if released {
            let mut run = cfg.clone();
            run.allow_mass_delete = p.allow_mass_delete;
            run.remove_out_of_scope_copies = p.remove_out_of_scope_copies;
            self.run_command(cmd, run, p.adopt_tweaks).await
        } else {
            Err(CtlError::new(
                code::BUSY,
                format!("vault {name}'s engine did not let go of its folders in time; try again"),
            ))
        };
        if had_engine && !self.stopping.load(Ordering::SeqCst) {
            if let Err(e) = self.start_engine(cfg).await {
                warn!("vault {name}: not started again after `{}`: {e}", cmd.name());
                lock(&entry.view).start_error = Some(e);
            }
        }
        lock(&entry.view).command = None;
        self.emit(
            event::COMMAND,
            Some(&name),
            json!({
                "command": cmd.name(),
                "phase": "finished",
                "ok": outcome.is_ok(),
                "message": outcome.as_ref().err().map(|e| e.message.clone()),
            }),
        );
        outcome
    }

    async fn run_command(&self, cmd: Command, cfg: RunConfig, adopt_tweaks: bool) -> Result<CommandResult, CtlError> {
        let host = self.host.clone();
        let open = cfg.clone();
        let engine = blocking(move || host.open_engine(&open)).await.map_err(host_error)?;
        info!("vault {}: running `{}` for a client", cfg.id, cmd.name());
        let t0 = Instant::now();
        let report = match cmd {
            Command::Sync => engine.sync_once().await.map(|r| serde_json::to_value(&r)),
            Command::Fetch => match engine.request_full_fetch() {
                Ok(()) => engine.sync_once().await.map(|r| serde_json::to_value(&r)),
                Err(e) => Err(e),
            },
            Command::Pull => engine.pull_once().await.map(|r| serde_json::to_value(&r)),
            Command::Connect => engine
                .connect_gate(adopt_tweaks)
                .await
                .map(|r| serde_json::to_value(&r)),
        };
        let wall = t0.elapsed();
        drop(engine);
        // The command's engine lets go of the folders before the vault's
        // own engine is started again.
        let dir = cfg.state_dir.clone();
        if !blocking(move || wait_released(&dir, RELEASE_TIMEOUT)).await {
            warn!("vault {}: the command's engine still holds its state folder", cfg.id);
        }
        let report = report
            .map_err(engine_error)?
            .map_err(|e| CtlError::new(code::FAILED, e.to_string()))?;
        Ok(CommandResult {
            report,
            wall_ms: wall.as_millis() as u64,
            peak_rss_kib: livesync_engine::platform::peak_rss_kib(),
        })
    }

    /// For `lsync daemon`: wait until a vault's first start is decided.
    /// `Err` with the reason when it did not start (it is then stopped, so
    /// the caller can exit as a one-shot `lsync daemon` always did).
    pub async fn wait_started(&self, name: &str) -> Result<(), String> {
        let entry = self.entry(name).map_err(|e| e.message)?;
        loop {
            let v = entry.view();
            if let Err(e) = &v.planned.run {
                return Err(e.clone());
            }
            if let Some(e) = v.start_error {
                return Err(e);
            }
            match self.host.state(name) {
                Some((RunState::Running, _)) => return Ok(()),
                Some((RunState::Failed, m)) => {
                    self.stop_engine(name).await;
                    return Err(m.unwrap_or_else(|| "the engine failed".into()));
                }
                Some((RunState::Starting, _)) => {}
                Some((RunState::Stopped, m)) => return Err(m.unwrap_or_else(|| "the engine stopped".into())),
                None => return Err(format!("vault {name} is not running")),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Stop every engine: refuse new requests, let running commands finish
    /// (up to the host's stop timeout each), then stop the vaults.
    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let entries: Vec<(String, Arc<Entry>)> =
            lock(&self.vaults).iter().map(|(n, e)| (n.clone(), e.clone())).collect();
        let mut guards = Vec::new();
        for (name, e) in entries {
            match tokio::time::timeout(STOP_TIMEOUT, e.ctl.clone().lock_owned()).await {
                Ok(g) => guards.push(g),
                Err(_) => warn!("vault {name}: a command is still running; stopping anyway"),
            }
        }
        let host = self.host.clone();
        blocking(move || host.stop_all()).await;
        drop(guards);
    }

    /// Watch the running engines and turn what they did into events, until
    /// the service is dropped.
    pub fn spawn_activity(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak: Weak<Service> = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut seen: HashMap<String, Seen> = HashMap::new();
            let mut tick = tokio::time::interval(ACTIVITY_POLL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(svc) = weak.upgrade() else { return };
                if svc.stopping.load(Ordering::SeqCst) {
                    return;
                }
                let mut state = std::mem::take(&mut seen);
                state = blocking(move || {
                    svc.poll_activity(&mut state);
                    state
                })
                .await;
                seen = state;
            }
        })
    }

    fn poll_activity(&self, seen: &mut HashMap<String, Seen>) {
        let names = self.names();
        seen.retain(|n, _| names.contains(n));
        for name in names {
            let Some(engine) = self.host.engine(&name) else {
                // The next engine starts its counters at zero.
                seen.remove(&name);
                continue;
            };
            let s = seen.entry(name.clone()).or_default();
            let (pull, push) = (engine.report(), engine.push_report());
            let moved = s.counters(self, &name, &pull, &push);
            if moved || s.status_at.is_none_or(|t| t.elapsed() >= STATUS_POLL) {
                if let Ok(st) = engine.status() {
                    s.status(self, &name, st.safety_stop, st.last_error);
                }
                s.status_at = Some(Instant::now());
            }
        }
    }
}

/// What the activity poll saw of one engine last time.
#[derive(Default)]
struct Seen {
    pull: PullReport,
    push: PushReport,
    status_at: Option<Instant>,
    /// `None` until the first read (which only sets the baseline).
    stop: Option<Option<String>>,
    error: Option<Option<String>>,
}

/// `cur - prev`, where a smaller `cur` means the counters were reset (the
/// engine reconnected): then all of `cur` is new.
fn delta(cur: u64, prev: u64) -> u64 {
    if cur >= prev {
        cur - prev
    } else {
        cur
    }
}

impl Seen {
    /// Emit what the counters say happened; true if any moved.
    fn counters(&mut self, svc: &Service, name: &str, pull: &PullReport, push: &PushReport) -> bool {
        let (p0, u0) = (&self.pull, &self.push);
        let pd = |c: fn(&PullReport) -> u64| delta(c(pull), c(p0));
        let ud = |c: fn(&PushReport) -> u64| delta(c(push), c(u0));
        let pulled = json!({
            "written": pd(|r| r.written),
            "deleted": pd(|r| r.deleted),
            "adopted": pd(|r| r.adopted),
            "dirty": pd(|r| r.dirty),
            "conflicted": pd(|r| r.conflicted),
            "restored_edits": pd(|r| r.restored_edits),
            "bytes_written": pd(|r| r.bytes_written),
        });
        let applied = pd(|r| r.written)
            + pd(|r| r.deleted)
            + pd(|r| r.adopted)
            + pd(|r| r.dirty)
            + pd(|r| r.conflicted)
            + pd(|r| r.restored_edits);
        let pushed = json!({
            "pushed": ud(|r| r.pushed),
            "created": ud(|r| r.created),
            "deleted": ud(|r| r.deleted),
            "conflicts": ud(|r| r.conflicts),
            "chunks_uploaded": ud(|r| r.chunks_uploaded),
            "chunk_bytes": ud(|r| r.chunk_bytes),
        });
        let stored = ud(|r| r.pushed) + ud(|r| r.deleted) + ud(|r| r.conflicts);
        let (conflicts_in, conflicts_out) = (pd(|r| r.conflicted), ud(|r| r.conflicts));
        let moved = pd(|r| r.changes) > 0 || ud(|r| r.passes) > 0 || applied > 0 || stored > 0;
        if applied > 0 {
            svc.emit(event::PULLED, Some(name), pulled);
        }
        if stored > 0 {
            svc.emit(event::PUSHED, Some(name), pushed);
        }
        if conflicts_in + conflicts_out > 0 {
            svc.emit(
                event::CONFLICT,
                Some(name),
                json!({"pulled": conflicts_in, "pushed": conflicts_out}),
            );
        }
        self.pull = pull.clone();
        self.push = push.clone();
        moved
    }

    fn status(&mut self, svc: &Service, name: &str, stop: Option<String>, error: Option<String>) {
        if let Some(before) = &self.stop {
            if *before != stop {
                svc.emit(event::SAFETY_STOP, Some(name), json!({"stop": stop}));
            }
        }
        if let Some(before) = &self.error {
            if *before != error {
                if let Some(e) = &error {
                    // Recorded as "<ms> <message>".
                    let msg = e.split_once(' ').map_or(e.as_str(), |(_, m)| m);
                    svc.emit(event::ERROR, Some(name), json!({"message": msg}));
                }
            }
        }
        self.stop = Some(stop);
        self.error = Some(error);
    }
}

/// Whether two server URLs name the same server, as accounts compare them.
pub fn same_server(a: &str, b: &str) -> bool {
    server_key(&redact_url(a)) == server_key(&redact_url(b))
}

/// For a vault's mode in `status` and logs.
pub fn mode_name(pull_only: bool) -> &'static str {
    if pull_only {
        Mode::PullOnly.as_str()
    } else {
        Mode::TwoWay.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_lose_their_credentials() {
        assert_eq!(
            redact_url("https://u:p@couch.example.net/db"),
            "https://couch.example.net/db"
        );
        assert_eq!(redact_url(" http://couch.example.net "), "http://couch.example.net");
        assert_eq!(
            redact_url("https://couch.example.net/a@b"),
            "https://couch.example.net/a@b"
        );
        assert!(same_server("https://Couch.example.net/", "https://u@couch.example.net"));
    }

    #[test]
    fn counter_deltas_survive_a_reset() {
        assert_eq!(delta(7, 5), 2);
        assert_eq!(delta(3, 5), 3);
        assert_eq!(delta(0, 0), 0);
    }
}
