//! The running service's side of a config change.
//!
//! `lsync account ...` and `lsync vault ...` call [`config_changed`] after
//! they wrote `lsync.toml` or the secret store, so a running `lsync serve`
//! rereads its config and restarts only the vaults that changed (spec D6).
//! The service and its control socket are not built yet: until they are,
//! this only logs. The socket client goes here; a missing service is not an
//! error (the change applies at its next start).

use std::path::Path;
use tracing::debug;

/// What changed, for the service to decide what to restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Account(String),
    Vault(String),
}

/// Tell a running service to reload `config_file`.
pub fn config_changed(config_file: &Path, change: &Change) {
    debug!("{} changed ({change:?}); no service to tell yet", config_file.display());
}
