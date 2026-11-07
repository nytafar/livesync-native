//! CLI routing (spec D7): a command for a vault that a running service holds
//! is run by the service; otherwise the CLI opens the engine itself.
//!
//! A vault is the service's when the service runs one with the same state
//! folder or the same vault folder as the command resolved (either would
//! meet the engine's locks). Then the command's settings must be the
//! service's: settings given only to this command (flags, `LSYNC_*`, a
//! profile file that says otherwise) would silently not apply, so such a
//! command is refused with what to do instead. The password is not
//! compared; the service uses its own.

use crate::config::Resolved;
use anyhow::{anyhow, bail, Result};
use livesync_host::control::{client_sockets, Client, ClientError};
use livesync_host::paths::comparable;
use livesync_host::protocol::{method, Settings, StatusResult, VaultView};
use livesync_host::service::same_server;
use std::path::Path;
use std::time::Duration;

/// How long the CLI waits for the service's `status`.
const STATUS_TIMEOUT: Duration = Duration::from_secs(30);

pub enum Route {
    /// No service holds the vault: open the engine here.
    Local,
    /// The service holds it, under this name.
    Service { client: Client, vault: String },
}

/// A running service, or `None`.
pub fn connect() -> Result<Option<Client>> {
    Client::connect_default().map_err(|e| anyhow!("{e}"))
}

/// A running service, or an error saying there is none.
pub fn require() -> Result<Client> {
    match connect()? {
        Some(c) => Ok(c),
        None => bail!(
            "no lsync service is running (looked for {}); start one with `lsync serve`",
            client_sockets()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub fn status_of(client: &mut Client, vault: Option<&str>) -> Result<StatusResult> {
    client.set_timeout(Some(STATUS_TIMEOUT));
    let v = client
        .call(method::STATUS, serde_json::json!({ "vault": vault }))
        .map_err(|e| anyhow!("the lsync service at {}: {e}", client.path().display()))?;
    client.set_timeout(None);
    Ok(serde_json::from_value(v)?)
}

fn same_dir(a: &str, b: &Path) -> bool {
    comparable(a) == comparable(&b.display().to_string())
}

fn absolute(p: &Path) -> std::path::PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// What differs between the command's settings and the service's.
pub fn differences(r: &Resolved, s: &Settings) -> Vec<String> {
    let mut d = Vec::new();
    let mut check = |what: &str, same: bool, mine: String, theirs: String| {
        if !same {
            d.push(format!("{what} {mine} (the service: {theirs})"));
        }
    };
    let show = |o: &str| {
        if o.is_empty() {
            "none".to_string()
        } else {
            o.to_string()
        }
    };
    check(
        "server",
        same_server(&r.remote.url, &s.url),
        livesync_host::service::redact_url(&r.remote.url),
        s.url.clone(),
    );
    let user = r.remote.username.clone().unwrap_or_default();
    check("user", user == s.username, show(&user), show(&s.username));
    check(
        "database",
        r.remote.database.trim() == s.database,
        r.remote.database.clone(),
        s.database.clone(),
    );
    let folder = absolute(&r.vault);
    check(
        "folder",
        same_dir(&s.folder, &folder),
        folder.display().to_string(),
        s.folder.clone(),
    );
    let state = absolute(&r.state_dir);
    check(
        "state folder",
        same_dir(&s.state_dir, &state),
        state.display().to_string(),
        s.state_dir.clone(),
    );
    let mode = format!("{:?}", r.scope.mode).to_lowercase();
    check(
        "folder scope",
        mode == s.scope_mode && r.scope.prefixes == s.scope_folders,
        r.scope.describe(),
        format!("{} {}", s.scope_mode, s.scope_folders.join(", ")),
    );
    let ignore = r.ignore_regex.clone().unwrap_or_default();
    check(
        "ignore regex",
        ignore == s.ignore_regex,
        show(&ignore),
        show(&s.ignore_regex),
    );
    let device = r.device_name.clone().unwrap_or_default();
    check(
        "device name",
        device.trim() == s.device_name.trim(),
        show(&device),
        show(&s.device_name),
    );
    let num = |o: Option<u64>| o.map_or("default".to_string(), |n| n.to_string());
    check(
        "max size",
        r.max_size_mb == s.max_size_mb,
        num(r.max_size_mb),
        num(s.max_size_mb),
    );
    check(
        "concurrency",
        r.concurrency == s.concurrency,
        num(r.concurrency.map(|c| c as u64)),
        num(s.concurrency.map(|c| c as u64)),
    );
    d
}

/// The service's vault with this state folder or vault folder, if any.
pub fn find<'a>(st: &'a StatusResult, r: &Resolved) -> Option<&'a VaultView> {
    let (state, folder) = (absolute(&r.state_dir), absolute(&r.vault));
    st.vaults.iter().find(|v| {
        // A disabled vault is not held by the service.
        v.state != "disabled"
            && v.settings
                .as_ref()
                .is_some_and(|s| same_dir(&s.state_dir, &state) || same_dir(&s.folder, &folder))
    })
}

/// Where a command for the vault `r` runs (see the module docs). With
/// `settings` false (`status`, which reads and applies nothing), only the
/// folders have to match.
pub fn route(r: &Resolved, what: &str, settings: bool) -> Result<Route> {
    let mut client = match Client::connect_default() {
        Ok(Some(c)) => c,
        Ok(None) => return Ok(Route::Local),
        Err(e @ ClientError::Unsafe(_)) => bail!("{e}"),
        Err(e) => bail!("cannot reach the lsync service: {e}"),
    };
    let st = status_of(&mut client, None)?;
    let Some(v) = find(&st, r) else {
        return Ok(Route::Local);
    };
    let diff = if settings {
        differences(r, v.settings.as_ref().expect("found by its settings"))
    } else {
        vec![]
    };
    if !diff.is_empty() && v.paused {
        // A paused vault's engine is stopped and its folders are free: the
        // command runs here, with its own settings, as without a service.
        return Ok(Route::Local);
    }
    if !diff.is_empty() {
        bail!(
            "vault {name} is run by the lsync service (socket {sock}), with other settings than this `{what}` was given: {list}. Settings given to one command don't apply to a vault the service runs: change the vault with `lsync vault edit {name} ...` (the service reloads it), or pause it with `lsync --vault {name} pause`, or stop the service",
            name = v.name,
            sock = client.path().display(),
            list = diff.join("; ")
        );
    }
    Ok(Route::Service {
        client,
        vault: v.name.clone(),
    })
}
