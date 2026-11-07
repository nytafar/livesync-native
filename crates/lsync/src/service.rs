//! The running service's side of a config change.
//!
//! `lsync account ...`, `lsync vault ...` and `lsync configure` call
//! [`config_changed`] after they wrote `lsync.toml` or the secret store, so
//! a running `lsync serve` rereads its config and restarts only the vaults
//! that changed (spec D6). The request does not wait for the reload (which
//! may wait for a command running on a vault). A service that reads another
//! config file is left alone, and no service is not an error: the change
//! applies at its next start.

use livesync_host::control::{Client, ClientError};
use livesync_host::protocol::{method, Hello};
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tracing::{debug, info, warn};

/// What changed, for the logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Account(String),
    Vault(String),
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => std::path::absolute(a).ok() == std::path::absolute(b).ok(),
    }
}

/// Tell a running service to reload `config_file`.
pub fn config_changed(config_file: &Path, change: &Change) {
    let mut client = match Client::connect_default() {
        Ok(Some(c)) => c,
        Ok(None) => {
            debug!(
                "{} changed ({change:?}); no lsync service is running",
                config_file.display()
            );
            return;
        }
        Err(e) => {
            warn!(
                "{} changed, but the lsync service was not told: {e}",
                config_file.display()
            );
            return;
        }
    };
    client.set_timeout(Some(Duration::from_secs(5)));
    let hello: Option<Hello> = match client.hello(&[method::RELOAD]) {
        Ok(h) => Some(h),
        Err(e @ ClientError::Version(_)) => {
            warn!(
                "{} changed, but the lsync service was not told: {e}",
                config_file.display()
            );
            return;
        }
        Err(e) => {
            debug!("hello: {e}");
            None
        }
    };
    match hello {
        Some(h) if same_file(Path::new(&h.config), config_file) => {}
        Some(h) => {
            debug!(
                "the lsync service runs {}, not {}; not told",
                h.config,
                config_file.display()
            );
            return;
        }
        None => {
            warn!(
                "{} changed, but the lsync service at {} did not answer; run `lsync reload`",
                config_file.display(),
                client.path().display()
            );
            return;
        }
    }
    match client.call(method::RELOAD, json!({"wait": false})) {
        Ok(_) => info!("the lsync service rereads {} ({change:?})", config_file.display()),
        Err(e) => warn!("the lsync service did not take the change ({e}); run `lsync reload`"),
    }
}
