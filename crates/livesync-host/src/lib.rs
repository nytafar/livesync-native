//! Hosting LiveSync vaults, for every front-end (spec D6, D13): the Android
//! app through `livesync-ffi`, and `lsync` on desktops and servers.
//!
//! - [`supervisor`]: one Tokio runtime, one supervisor task per vault, with
//!   retries and stop and release timeouts ([`Host`]).
//! - [`run`]: what one supervised engine needs, and its checks.
//!
//! Nothing here is platform-specific: the front-end owns the service, the
//! UI, the network callback and the platform's secret store.

pub mod cancel;
pub mod paths;
pub mod run;
pub mod supervisor;

pub use cancel::Cancel;
pub use paths::paths_overlap;
pub use run::RunConfig;
pub use supervisor::{GateCheck, Host, RunState, StateListener, VaultAccess, VaultStatus};

use livesync_engine::EngineError;
use livesync_remote::RemoteError;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The vault or an argument is not usable as given.
    #[error("{msg}")]
    Invalid { msg: String },
    /// Another engine holds this vault's state folder, or a running vault
    /// overlaps this one's folder.
    #[error("{msg}")]
    Busy { msg: String },
    /// No engine is running for this vault.
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

impl From<EngineError> for HostError {
    fn from(e: EngineError) -> Self {
        let msg = e.to_string();
        match e {
            EngineError::Refused(_) => HostError::Refused { msg },
            EngineError::Remote(_) => HostError::Remote { msg },
            EngineError::Busy(_) => HostError::Busy { msg },
            EngineError::Index(_) | EngineError::Other(_) => HostError::Other { msg },
        }
    }
}

/// A sentence for the user about a remote error, without the request
/// details that only matter in the log.
pub fn explain_remote(e: &RemoteError) -> String {
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
