//! Hosting LiveSync vaults, for every front-end (spec D6, D13): the Android
//! app through `livesync-ffi`, and `lsync` on desktops and servers.
//!
//! - [`supervisor`]: one Tokio runtime, one supervisor task per vault, with
//!   retries and stop and release timeouts ([`Host`]).
//! - [`config`]: accounts and vaults (`lsync.toml`), and the folder rules
//!   between vaults.
//! - [`edit`]: changing `lsync.toml` with its comments and layout kept.
//! - [`secret`]: the `SecretStore` seam and its backends.
//! - [`migrate`]: from `<profile>.toml` files to `lsync.toml`.
//!
//! Nothing here is platform-specific: the front-end owns the service, the
//! UI, the network callback and the platform's secret store.

pub mod cancel;
pub mod config;
pub mod edit;
pub mod legacy;
pub mod migrate;
pub mod paths;
pub mod run;
pub mod secret;
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

/// A TOML error as "line L, column C: message", without the source line
/// the parser's own rendering quotes: a config or profile file may hold a
/// password on that line.
pub fn toml_error(text: &str, message: &str, span: Option<std::ops::Range<usize>>) -> String {
    let message = message.trim();
    match span {
        Some(r) => {
            let before = &text[..r.start.min(text.len())];
            let line = before.matches('\n').count() + 1;
            let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
            format!("line {line}, column {column}: {message}")
        }
        None => message.to_string(),
    }
}

/// [`toml_error`] for `toml::from_str`.
pub fn toml_de_error(text: &str, e: &toml::de::Error) -> String {
    toml_error(text, e.message(), e.span())
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
