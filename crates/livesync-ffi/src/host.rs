//! The app's handle on the engine host (`livesync_host::Host`): one Tokio
//! runtime, N profiles, one supervisor each. This file only translates
//! between the UniFFI types and the host's; the supervision itself (retries
//! from 15 s to 15 min, the stop and release timeouts) lives in the host.

use crate::folders::{Cancel, FolderList};
use crate::profile::ProfileConfig;
use crate::status::{self, GateResult, ProfileStatus};
use crate::{logging, GateOutcome, LiveSyncError};
use livesync_host::{GateCheck, Host};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

pub use livesync_host::RunState;

#[uniffi::remote(Enum)]
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

/// The front-end's answer to "is the whole vault still visible to this
/// process?" (Android: all-files access is still granted). Asked before any
/// pass that could delete remote documents; while it reports a problem,
/// missing files are never treated as deleted (the engine's `AccessCheck`).
#[uniffi::export(callback_interface)]
pub trait VaultAccess: Send + Sync {
    /// `None` when the vault is fully accessible, otherwise the reason.
    fn problem(&self, vault_path: String) -> Option<String>;
}

struct Listener(Box<dyn StatusListener>);

impl livesync_host::StateListener for Listener {
    fn on_state_changed(&self, id: String, state: RunState, message: Option<String>) {
        self.0.on_state_changed(id, state, message)
    }
}

struct Access(Box<dyn VaultAccess>);

impl livesync_host::VaultAccess for Access {
    fn problem(&self, vault_path: String) -> Option<String> {
        self.0.problem(vault_path)
    }
}

#[derive(uniffi::Object)]
pub struct LiveSync {
    host: Host,
}

impl LiveSync {
    /// Shorter stop and release grace periods, for tests.
    #[doc(hidden)]
    pub fn set_stop_timeouts_for_tests(&self, stop: Duration, release: Duration) {
        self.host.set_stop_timeouts_for_tests(stop, release);
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
        let host = Host::new()?;
        info!("livesync core {} ready", env!("CARGO_PKG_VERSION"));
        Ok(Arc::new(LiveSync { host }))
    }

    /// Install the vault access check every engine asks before a pass that
    /// could delete remote documents (see [`VaultAccess`]).
    pub fn set_vault_access(&self, access: Box<dyn VaultAccess>) {
        self.host.set_vault_access(Arc::new(Access(access)));
    }

    /// Receive run-state changes of every profile.
    pub fn set_listener(&self, listener: Box<dyn StatusListener>) {
        self.host.set_listener(Arc::new(Listener(listener)));
    }

    /// Start (or restart with new settings) the engine for a profile. Refused
    /// when another running profile's vault folder is the same as, inside or
    /// above this one, or shares its state folder.
    pub fn start(&self, cfg: ProfileConfig) -> Result<(), LiveSyncError> {
        Ok(self.host.start(cfg.run_config())?)
    }

    /// The user confirmed a mass-delete safety stop: `safety_stop` is the
    /// stop the dialog showed (`ProfileStatus.safety_stop`, as it was when
    /// the dialog opened). The next full push pass, and only that one,
    /// deletes the files that stop showed (`Engine::allow_mass_delete_once`);
    /// it is asked for at once. Files that went missing since are judged as
    /// usual. `StopChanged` when the stop shown now is another one (nothing
    /// is allowed; show it and ask again), `Invalid` when no mass-delete
    /// stop is shown.
    pub fn allow_mass_delete_once(&self, profile_id: String, safety_stop: String) -> Result<(), LiveSyncError> {
        Ok(self.host.allow_mass_delete_once(&profile_id, &safety_stop)?)
    }

    /// Stop a profile's engine: wait up to 15 s for it to wind down, cancel
    /// it after that, and wait until it let go of its state folder. Returns
    /// false if it still holds the folder (do not delete the state then).
    pub fn stop(&self, profile_id: String) -> bool {
        self.host.stop(&profile_id)
    }

    pub fn stop_all(&self) {
        self.host.stop_all()
    }

    /// IDs of the profiles that have an engine slot (running, starting or
    /// waiting to retry).
    pub fn active_profiles(&self) -> Vec<String> {
        self.host.active()
    }

    /// "Sync now": reconnect at once and rescan the vault; a failed engine is
    /// retried at once.
    pub fn sync_now(&self, profile_id: String) -> Result<(), LiveSyncError> {
        Ok(self.host.sync_now(&profile_id)?)
    }

    /// The network came up or went down (NetworkMonitor). On up, every
    /// engine reconnects and rescans, and failed engines retry at once.
    pub fn network_changed(&self, up: bool, metered: bool) {
        self.host.network_changed(up, metered)
    }

    /// Status of a profile, running or not. For a profile without an engine
    /// the index is read from `state_dir` (no network).
    pub fn status(&self, profile_id: String, vault_path: String, state_dir: String) -> ProfileStatus {
        let st = self.host.status(&profile_id, &vault_path, &state_dir);
        let mut out = status::from_engine(
            &profile_id,
            st.state,
            st.message,
            st.pull_only,
            st.snapshot.as_ref(),
            st.session.as_ref(),
        );
        status::add_push(&mut out, st.push.as_ref());
        out.mass_delete_allowed = st.mass_delete_allowed;
        out.mass_delete_waits = st.mass_delete_waits;
        out.deletes_waiting = st.deletes_waiting;
        out
    }

    /// Run the connect gate for settings that are not saved yet, against a
    /// throwaway index and vault under `scratch_dir` (removed afterwards).
    /// Reads only; never writes to the remote or to the real vault.
    pub fn connect_test(&self, cfg: ProfileConfig, scratch_dir: String) -> GateResult {
        let run = cfg.run_config();
        let result = match self.host.connect_test(&run, &scratch_dir) {
            GateCheck::Passed(g) => GateResult::passed(&g),
            GateCheck::Failed(e) => GateResult::failed(&e),
            GateCheck::Invalid(msg) => return GateResult::with(GateOutcome::Failed, msg),
            GateCheck::OpenFailed(msg) => GateResult::with(GateOutcome::Failed, msg),
            GateCheck::Unreachable(msg) => GateResult::with(GateOutcome::Unreachable, msg),
        };
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
    /// [`Cancel::cancel`] on `cancel` ends it at once with `Other`.
    pub fn list_folders(&self, cfg: ProfileConfig, cancel: Option<Arc<Cancel>>) -> Result<FolderList, LiveSyncError> {
        let run = cfg.run_config();
        let l = self.host.list_folders(&run, cancel.as_deref().map(|c| c.inner()))?;
        Ok(l.into())
    }

    /// Accept changed PREFERRED tweak values on the remote (the gate's
    /// "tweaks changed" refusal) for a running profile, then reconnect.
    pub fn adopt_tweaks(&self, profile_id: String) -> Result<GateResult, LiveSyncError> {
        Ok(match self.host.adopt_tweaks(&profile_id)? {
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
        Ok(self.host.fetch_from_remote(&profile_id)?)
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
