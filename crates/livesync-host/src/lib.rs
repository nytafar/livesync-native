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
//! - [`service`]: one process hosting a set of vaults (`lsync serve`), with
//!   reload, pause and resume, commands for clients, and events.
//! - [`control`]: the local control socket in front of a service, and its
//!   client; [`protocol`]: what goes over it (spec D7).
//!
//! The front-end owns the process, the UI, the network callback and the
//! platform's secret store. The control socket is Unix-only (`cfg(unix)`).

pub mod cancel;
pub mod config;
#[cfg(unix)]
pub mod control;
pub mod edit;
pub mod keyring;
pub mod legacy;
pub mod migrate;
pub mod notify;
pub mod paths;
pub mod protocol;
pub mod run;
pub mod secret;
pub mod service;
pub mod supervisor;

pub use cancel::Cancel;
pub use paths::paths_overlap;
pub use run::{Handover, RunConfig, Tuning, WhatSyncs};
pub use supervisor::{EngineHook, GateCheck, Host, RunState, StateListener, VaultAccess, VaultStatus};

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
    let message = without_values(message.trim());
    let message = message.as_str();
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

/// A serde message without the value it rejects: "invalid type: integer
/// `98765432`, expected a string" becomes "invalid type: an integer,
/// expected a string", and "unknown variant `x`, expected ..." becomes
/// "unknown value, expected ...". A misplaced password must not come back
/// in an error. Keys (unknown field `x`) stay: they are not values.
pub fn without_values(message: &str) -> String {
    let mut out = String::new();
    for (i, line) in message.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&line_without_values(line));
    }
    out
}

fn line_without_values(line: &str) -> String {
    for lead in ["invalid type: ", "invalid value: "] {
        if let Some(at) = line.find(lead) {
            let rest = &line[at + lead.len()..];
            let (unexpected, tail) = match rest.find(", expected") {
                Some(e) => (&rest[..e], &rest[e..]),
                None => (rest, ""),
            };
            // `Unexpected` reads "integer `1`", "string \"x\"", "map", ...:
            // keep the kind, drop what follows it.
            let kind = unexpected
                .split([' ', '`', '"', '\''])
                .next()
                .filter(|k| !k.is_empty())
                .unwrap_or("value");
            let kind = match kind {
                "floating" => "a floating point number".to_string(),
                "unit" | "other" => "a value of another type".to_string(),
                k if k.starts_with(['a', 'e', 'i', 'o', 'u']) => format!("an {k}"),
                k => format!("a {k}"),
            };
            return format!("{}{lead}{kind}{tail}", &line[..at]);
        }
    }
    if let Some(at) = line.find("unknown variant ") {
        let rest = &line[at..];
        let tail = rest.find(", expected").map_or("", |e| &rest[e..]);
        return format!("{}unknown value{tail}", &line[..at]);
    }
    line.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_messages_lose_the_values_they_reject() {
        #[derive(serde::Deserialize, Debug)]
        #[allow(dead_code)]
        struct T {
            a: Option<String>,
            m: Option<crate::config::Mode>,
            n: Option<u64>,
        }
        for text in [
            "a = 98765432\n",
            "a = true\n",
            "a = 1.5\n",
            "a = [\"leaky\"]\n",
            "m = \"Hunter2Leaky\"\n",
            "n = \"Hunter2Leaky\"\n",
            "n = -98765432\n",
        ] {
            let e = toml::from_str::<T>(text).unwrap_err();
            let msg = toml_de_error(text, &e);
            for leak in ["98765432", "true", "1.5", "leaky", "Leaky"] {
                assert!(!msg.contains(leak), "{text:?} gave {msg:?}");
            }
            assert!(msg.starts_with("line 1"), "{msg}");
        }
        assert_eq!(
            without_values("invalid type: integer `98765432`, expected a string"),
            "invalid type: an integer, expected a string"
        );
        assert_eq!(
            without_values("unknown variant `x`, expected `two-way` or `pull-only`"),
            "unknown value, expected `two-way` or `pull-only`"
        );
        assert_eq!(
            without_values("unknown field `x`, expected `a`"),
            "unknown field `x`, expected `a`"
        );
    }
}
