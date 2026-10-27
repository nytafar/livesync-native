//! UniFFI surface over the LiveSync engine, for the Android app (spec D1,
//! "Designed for multiple vaults").
//!
//! One [`LiveSync`] host holds N engines, one per profile, each run by its own
//! supervisor task on a shared Tokio runtime. Every call takes a profile ID.
//! Nothing here is Android-specific except the logcat sink in `logging`; the
//! app owns the service, the Keystore, the network callback and the UI, and
//! hands each profile over with its credentials already resolved.

uniffi::setup_scaffolding!();

pub mod folders;
pub mod host;
pub mod logging;
pub mod profile;
pub mod status;

pub use folders::{Cancel, FolderInfo, FolderList};
pub use host::{LiveSync, RunState, StatusListener, VaultAccess};
pub use profile::{check_scope, paths_overlap, ProfileConfig, ScopeKind};
pub use status::{AttentionItem, GateOutcome, GateResult, ProfileStatus, StateCount};

use livesync_engine::EngineError;
use livesync_remote::RemoteError;

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
    #[error("{msg}")]
    Remote { msg: String },
    #[error("{msg}")]
    Other { msg: String },
}

impl From<EngineError> for LiveSyncError {
    fn from(e: EngineError) -> Self {
        let msg = e.to_string();
        match e {
            EngineError::Refused(_) => LiveSyncError::Refused { msg },
            EngineError::Remote(_) => LiveSyncError::Remote { msg },
            EngineError::Busy(_) => LiveSyncError::Busy { msg },
            EngineError::Index(_) | EngineError::Other(_) => LiveSyncError::Other { msg },
        }
    }
}

/// A sentence for the user about a remote error, without the request
/// details that only matter in the log.
pub(crate) fn explain_remote(e: &RemoteError) -> String {
    match e {
        RemoteError::Http { status: 401, .. } => "the server refused the user name or password (HTTP 401)".into(),
        RemoteError::Http { status: 403, .. } => "this user may not read the database (HTTP 403)".into(),
        RemoteError::Http { status: 404, what, .. } if what == "database info" => {
            "the database does not exist on this server (HTTP 404)".into()
        }
        RemoteError::Transport { .. } => format!("cannot reach the server: {e}"),
        _ => e.to_string(),
    }
}

/// The FFI crate's version (the workspace version).
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
