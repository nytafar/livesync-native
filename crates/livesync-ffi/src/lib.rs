//! UniFFI surface over the LiveSync engine, for the Android app (spec D1,
//! "Designed for multiple vaults"): a thin wrapper over `livesync-host`.
//!
//! One [`LiveSync`] host holds N engines, one per profile, each run by its own
//! supervisor task on a shared Tokio runtime (`livesync_host::Host`). Every
//! call takes a profile ID. Nothing here is Android-specific except the
//! logcat sink in `logging`; the app owns the service, the Keystore, the
//! network callback and the UI, and hands each profile over with its
//! credentials already resolved.

uniffi::setup_scaffolding!();

pub mod folders;
pub mod host;
pub mod logging;
pub mod profile;
pub mod status;

pub use folders::{Cancel, FolderInfo, FolderList};
pub use host::{LiveSync, RunState, StatusListener, VaultAccess};
pub use profile::{check_scope, paths_overlap, ProfileConfig, ScopeKind};
pub use status::{AttentionItem, ConflictItem, Connection, GateOutcome, GateResult, ProfileStatus, StateCount};

use livesync_engine::EngineError;
use livesync_host::HostError;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum LiveSyncError {
    /// The profile or an argument is not usable as given.
    #[error("{msg}")]
    Invalid { msg: String },
    /// Another engine holds this profile's state folder, or a running profile
    /// overlaps this one's vault folder.
    #[error("{msg}")]
    Busy { msg: String },
    /// No engine is running for this profile.
    #[error("{msg}")]
    NotRunning { msg: String },
    /// A compatibility gate refused.
    #[error("{msg}")]
    Refused { msg: String },
    /// The safety stop the user confirmed is no longer the one shown (more
    /// files went missing and a new stop was recorded): nothing was allowed.
    #[error("{msg}")]
    StopChanged { msg: String },
    #[error("{msg}")]
    Remote { msg: String },
    #[error("{msg}")]
    Other { msg: String },
}

impl From<EngineError> for LiveSyncError {
    fn from(e: EngineError) -> Self {
        HostError::from(e).into()
    }
}

impl From<HostError> for LiveSyncError {
    fn from(e: HostError) -> Self {
        match e {
            HostError::Invalid { msg } => LiveSyncError::Invalid { msg },
            HostError::Busy { msg } => LiveSyncError::Busy { msg },
            HostError::NotRunning { msg } => LiveSyncError::NotRunning { msg },
            HostError::Refused { msg } => LiveSyncError::Refused { msg },
            HostError::StopChanged { msg } => LiveSyncError::StopChanged { msg },
            HostError::Remote { msg } => LiveSyncError::Remote { msg },
            HostError::Other { msg } => LiveSyncError::Other { msg },
        }
    }
}

pub(crate) use livesync_host::explain_remote;

/// The FFI crate's version (the workspace version).
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
