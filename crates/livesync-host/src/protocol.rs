//! The control socket's protocol (spec D7): newline-delimited JSON, one
//! object per line, in both directions.
//!
//! - A request: `{"v":1,"id":7,"method":"status","params":{"vault":"notes"}}`.
//!   `id` is any JSON value the client picks; `params` may be left out.
//! - Its response, with the same `id`: `{"v":1,"id":7,"result":{...}}`, or
//!   `{"v":1,"id":7,"error":{"code":"unknown_vault","message":"..."}}`.
//!   Requests on one connection run concurrently, so responses may come in
//!   another order than the requests; match them by `id`.
//! - After `events`, the connection also carries event lines, which have
//!   `event` instead of `id`:
//!   `{"v":1,"event":"pulled","vault":"notes","ts":1730000000000,"data":{...}}`.
//!
//! **Versioning.** `v` is the protocol's major version. Within a major
//! version, methods, parameters, result fields, event kinds and error codes
//! are only ever added, never renamed or removed, and their meaning stays.
//! Clients ignore fields and event kinds they don't know; the service
//! ignores parameters it doesn't know. A request whose `v` is another major
//! version gets `unsupported_version` (with the versions spoken here); one
//! without `v` is read as version 1.
//!
//! **Reading replies.** Every type here that a client decodes takes a
//! missing field as its default (`#[serde(default)]` on the struct), and so
//! do the engine's reports and status they carry: a client reads an older
//! service's replies, which lack the fields added since. A client checks
//! `hello` first ([`check_hello`]): the major version, and that the methods
//! it is about to use are there.

use livesync_engine::{PullReport, PushReport, Status};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol's major version.
pub const PROTOCOL: u32 = 1;

/// The longest request line the service reads (the rest of the connection
/// is dropped): requests are small; this bounds a misbehaving client.
pub const MAX_LINE: usize = 1 << 20;

pub mod method {
    /// The service's versions and methods.
    pub const HELLO: &str = "hello";
    /// Every vault (or one, `{"vault":NAME}`) with its state and status.
    pub const STATUS: &str = "status";
    /// Subscribe to events (all vaults, or one).
    pub const EVENTS: &str = "events";
    /// Reconnect and rescan now (all vaults, or one).
    pub const SYNC_NOW: &str = "sync-now";
    /// Stop a vault's engine until `resume` (all vaults, or one).
    pub const PAUSE: &str = "pause";
    pub const RESUME: &str = "resume";
    /// Reread the config; start, stop or restart only what changed.
    pub const RELOAD: &str = "reload";
    /// One command run by the service for a client, on a vault it runs
    /// (`lsync sync` and the others while the service holds the vault).
    pub const SYNC: &str = "sync";
    pub const PULL: &str = "pull";
    pub const FETCH: &str = "fetch";
    pub const CONNECT: &str = "connect";

    pub const ALL: &[&str] = &[
        HELLO, STATUS, EVENTS, SYNC_NOW, PAUSE, RESUME, RELOAD, SYNC, PULL, FETCH, CONNECT,
    ];
}

/// Error codes. Clients act on the code; the message is for people.
pub mod code {
    /// Not JSON, not an object, or a parameter of the wrong type.
    pub const BAD_REQUEST: &str = "bad_request";
    pub const UNSUPPORTED_VERSION: &str = "unsupported_version";
    pub const UNKNOWN_METHOD: &str = "unknown_method";
    /// No vault of that name in the service.
    pub const UNKNOWN_VAULT: &str = "unknown_vault";
    /// The vault is in the config but not running: disabled, or its
    /// settings cannot be used (a missing password, ...).
    pub const NOT_RUNNING: &str = "not_running";
    /// The vault is paused.
    pub const PAUSED: &str = "paused";
    /// The vault is busy (a command for another client), or its engine did
    /// not let go of its folders in time.
    pub const BUSY: &str = "busy";
    /// A compatibility gate refused (the message is the refusal).
    pub const REFUSED: &str = "refused";
    /// The config could not be read; nothing was changed.
    pub const INVALID_CONFIG: &str = "invalid_config";
    /// The command ran and failed (the message says why).
    pub const FAILED: &str = "failed";
    /// The connecting user may not use this service.
    pub const FORBIDDEN: &str = "forbidden";
    /// The service is stopping.
    pub const SHUTTING_DOWN: &str = "shutting_down";
    /// A command's `fingerprint` is not the vault's: its settings changed
    /// (a reload) since the client compared them. Nothing was run.
    pub const CHANGED: &str = "changed";

    /// Every code above, for clients mapping a code to a constant.
    pub const ALL: &[&str] = &[
        BAD_REQUEST,
        UNSUPPORTED_VERSION,
        UNKNOWN_METHOD,
        UNKNOWN_VAULT,
        NOT_RUNNING,
        PAUSED,
        BUSY,
        REFUSED,
        INVALID_CONFIG,
        FAILED,
        FORBIDDEN,
        SHUTTING_DOWN,
        CHANGED,
    ];
}

/// Event kinds. Counts in `pulled`, `pushed` and `conflict` are what
/// happened since the previous event of that kind for the vault (best
/// effort: `status` is the source of truth).
pub mod event {
    /// `{"state": "starting"|"running"|"failed"|"stopped", "message"}`.
    pub const STATE: &str = "state";
    /// `{"paused": bool}`.
    pub const PAUSED: &str = "paused";
    /// Remote changes applied: `{"written","deleted","adopted","dirty","conflicted","restored_edits","bytes_written"}`.
    pub const PULLED: &str = "pulled";
    /// Local changes stored on the server: `{"pushed","created","deleted","conflicts","chunks_uploaded","chunk_bytes"}`.
    pub const PUSHED: &str = "pushed";
    /// New conflicts: `{"pulled": n, "pushed": n}` (pulled: conflicted
    /// documents seen; pushed: local edits stored as a branch).
    pub const CONFLICT: &str = "conflict";
    /// The mass-delete safety stop: `{"stop": text}`, or `{"stop": null}`
    /// once it is cleared.
    pub const SAFETY_STOP: &str = "safety-stop";
    /// A new error recorded by the engine: `{"message"}`.
    pub const ERROR: &str = "error";
    /// A command run for a client: `{"command","phase":"started"|"finished","ok","message"}`.
    pub const COMMAND: &str = "command";
    /// The config was reread (`vault` is null): the reload report.
    pub const RELOAD: &str = "reload";
    /// This subscriber fell behind and missed `{"missed": n}` events.
    pub const LAGGED: &str = "lagged";
}

/// A request as sent by a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    #[serde(default = "protocol")]
    pub v: u32,
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

fn protocol() -> u32 {
    PROTOCOL
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

/// A response to one request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    #[serde(default = "protocol")]
    pub v: u32,
    #[serde(default)]
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Response {
        Response {
            v: PROTOCOL,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, e: CtlError) -> Response {
        Response {
            v: PROTOCOL,
            id,
            result: None,
            error: Some(ErrorBody {
                code: e.code.to_string(),
                message: e.message,
            }),
        }
    }
}

/// One event line.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Event {
    pub v: u32,
    pub event: String,
    /// The vault it is about; `null` for the service as a whole.
    pub vault: Option<String>,
    /// When, in milliseconds since the Unix epoch.
    pub ts: i64,
    pub data: Value,
}

/// A line a client reads: a response or an event.
#[derive(Debug, Clone)]
pub enum Incoming {
    Response(Response),
    Event(Event),
}

impl Incoming {
    pub fn parse(line: &str) -> Result<Incoming, String> {
        let v: Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
        if v.get("event").is_some() {
            serde_json::from_value(v)
                .map(Incoming::Event)
                .map_err(|e| e.to_string())
        } else {
            serde_json::from_value(v)
                .map(Incoming::Response)
                .map_err(|e| e.to_string())
        }
    }
}

/// An error answer: a code from [`code`] and a message for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtlError {
    pub code: &'static str,
    pub message: String,
}

impl CtlError {
    pub fn new(code: &'static str, message: impl Into<String>) -> CtlError {
        CtlError {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CtlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// `{"vault": NAME}`, optional: without it, every vault.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VaultParam {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<String>,
}

/// The parameters of `sync`, `pull`, `fetch` and `connect`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandParams {
    pub vault: String,
    /// `sync`, `fetch`: as `lsync sync --allow-mass-delete`.
    #[serde(default)]
    pub allow_mass_delete: bool,
    /// `sync`, `fetch`: as `--remove-out-of-scope-copies`.
    #[serde(default)]
    pub remove_out_of_scope_copies: bool,
    /// `connect`: accept changed PREFERRED tweaks.
    #[serde(default)]
    pub adopt_tweaks: bool,
    /// The vault's `fingerprint` from the `status` the client compared its
    /// settings with. When given and the vault's settings are no longer
    /// those, the command is not run (`changed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// The result of `sync`, `pull`, `fetch` and `connect`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandResult {
    /// The engine's report: a sync report (`sync`, `fetch`), a pull report
    /// (`pull`) or the gate's (`connect`), as `lsync ... --json` prints it.
    pub report: Value,
    pub wall_ms: u64,
    /// The service's peak resident set size.
    pub peak_rss_kib: Option<u64>,
}

/// `{"wait": false}` returns at once with `{"scheduled": true}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReloadParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<bool>,
}

/// What a reload changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReloadReport {
    pub started: Vec<String>,
    pub stopped: Vec<String>,
    pub restarted: Vec<String>,
    pub unchanged: Vec<String>,
    /// Vaults that are configured but cannot run, with why.
    pub failed: Vec<VaultError>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VaultError {
    pub vault: String,
    pub message: String,
}

/// The vault names a `pause`, `resume` or `sync-now` acted on.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VaultsResult {
    pub vaults: Vec<String>,
}

/// The result of `hello`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Hello {
    pub protocol: u32,
    /// The service's `lsync` version.
    pub version: String,
    pub pid: u32,
    pub methods: Vec<String>,
    /// Where its vaults come from (the config file).
    pub config: String,
}

/// A vault's settings as the service runs them, without the password.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub url: String,
    pub username: String,
    pub database: String,
    pub folder: String,
    pub state_dir: String,
    /// `all`, `include` or `exclude`.
    pub scope_mode: String,
    pub scope_folders: Vec<String>,
    pub pull_only: bool,
    pub ignore_regex: String,
    pub device_name: String,
    pub max_size_mb: Option<u64>,
    pub concurrency: Option<usize>,
}

/// One vault in `status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VaultView {
    pub name: String,
    /// `starting`, `running`, `failed` (retrying), `stopped`, `paused`,
    /// `busy` (running a command for a client), `disabled` (in the config
    /// with `enabled = false`) or `invalid` (its settings cannot be used;
    /// `message` says why).
    pub state: String,
    pub message: Option<String>,
    pub paused: bool,
    pub enabled: bool,
    pub account: Option<String>,
    /// The command running for a client, if any (`sync`, ...).
    pub command: Option<String>,
    /// `None` when the settings cannot be used (`invalid`).
    pub settings: Option<Settings>,
    /// The engine's status (as `lsync status --json`), or the index's when
    /// no engine runs; `None` before the first sync.
    pub status: Option<Status>,
    /// This engine session's pull and push counters (running only).
    pub session: Option<PullReport>,
    pub push: Option<PushReport>,
    pub deletes_waiting: u64,
    pub mass_delete_allowed: bool,
    /// Names the settings the service runs the vault with (opaque; changes
    /// whenever `settings` does). A client that compared `settings` sends it
    /// back with a command (see [`CommandParams::fingerprint`]).
    pub fingerprint: Option<String>,
}

/// The result of `status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusResult {
    pub vaults: Vec<VaultView>,
}

/// Whether a client speaking [`PROTOCOL`] can use the service that sent
/// this `hello` for `needs` (the methods it is about to call). `Err` says
/// what to do, for people.
pub fn check_hello(h: &Hello, needs: &[&str]) -> Result<(), String> {
    let theirs = if h.version.is_empty() {
        "an unknown version".to_string()
    } else {
        h.version.clone()
    };
    if h.protocol > PROTOCOL {
        return Err(format!(
            "the running lsync service ({theirs}) speaks protocol version {}, newer than this lsync's ({PROTOCOL}); update this lsync, or restart the service with this one",
            h.protocol
        ));
    }
    if h.protocol < PROTOCOL {
        return Err(format!(
            "the running lsync service ({theirs}) speaks protocol version {}, older than this lsync's ({PROTOCOL}); restart it, so that it runs this lsync",
            h.protocol
        ));
    }
    let missing: Vec<&str> = needs
        .iter()
        .copied()
        .filter(|m| !h.methods.iter().any(|x| x == m))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "the running lsync service ({theirs}) is older than this lsync and has no `{}`; restart it, so that it runs this lsync",
            missing.join("`, `")
        ));
    }
    Ok(())
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_default_to_version_1_and_no_params() {
        let r: Request = serde_json::from_str(r#"{"id":"a","method":"status"}"#).unwrap();
        assert_eq!(r.v, PROTOCOL);
        assert!(r.params.is_null());
        let line = serde_json::to_string(&r).unwrap();
        assert_eq!(line, r#"{"v":1,"id":"a","method":"status"}"#);
    }

    #[test]
    fn lines_are_told_apart_by_event() {
        let ok = serde_json::to_string(&Response::ok(json!(3), json!({"x": 1}))).unwrap();
        assert_eq!(ok, r#"{"v":1,"id":3,"result":{"x":1}}"#);
        assert!(matches!(Incoming::parse(&ok).unwrap(), Incoming::Response(r) if r.id == json!(3)));
        let err = serde_json::to_string(&Response::err(json!(4), CtlError::new(code::PAUSED, "paused"))).unwrap();
        assert_eq!(err, r#"{"v":1,"id":4,"error":{"code":"paused","message":"paused"}}"#);
        let ev = r#"{"v":1,"event":"pulled","vault":"n","ts":5,"data":{"written":1},"later":"ignored"}"#;
        match Incoming::parse(ev).unwrap() {
            Incoming::Event(e) => {
                assert_eq!((e.event.as_str(), e.vault.as_deref(), e.ts), ("pulled", Some("n"), 5));
                assert_eq!(e.data["written"], 1);
            }
            other => panic!("{other:?}"),
        }
        assert!(Incoming::parse("nope").is_err());
    }

    #[test]
    fn replies_with_fields_missing_are_read_with_defaults() {
        // As an older service sends them: fields added since are missing,
        // in this crate's types and in the engine's reports and status.
        let st: StatusResult = serde_json::from_value(json!({
            "vaults": [{
                "name": "notes",
                "state": "running",
                "settings": {"folder": "/v", "state_dir": "/s"},
                "status": {"vault": "/v", "dirty": [{"path": "a.md"}], "pending": [{}], "local_issues": [{}]},
                "session": {"written": 1},
                "push": {}
            }]
        }))
        .unwrap();
        let v = &st.vaults[0];
        assert_eq!(
            (v.name.as_str(), v.paused, v.fingerprint.as_deref()),
            ("notes", false, None)
        );
        assert_eq!(v.settings.as_ref().unwrap().scope_folders, Vec::<String>::new());
        let status = v.status.as_ref().unwrap();
        assert!(!status.needs_full_fetch && status.counts.is_empty());
        assert_eq!(status.dirty[0].path, "a.md");
        assert_eq!(v.session.as_ref().unwrap().written, 1);
        assert_eq!(v.push.as_ref().unwrap().passes, 0);
        assert!(serde_json::from_value::<StatusResult>(json!({}))
            .unwrap()
            .vaults
            .is_empty());

        let r: CommandResult = serde_json::from_value(json!({"report": {"pull": {"changes": 2}}})).unwrap();
        let sync: livesync_engine::SyncReport = serde_json::from_value(r.report).unwrap();
        assert_eq!((sync.pull.changes, sync.push.pushed), (2, 0));
        let gate: livesync_engine::GateReport = serde_json::from_value(json!({"remote": "r"})).unwrap();
        assert!(!gate.fetch_to_resolve && gate.node_id.is_empty());
        let pull: PullReport = serde_json::from_value(json!({})).unwrap();
        assert_eq!(pull.restored_edits, 0);
        let push: PushReport = serde_json::from_value(json!({"pushed": 3})).unwrap();
        assert_eq!(push.pushed, 3);
        let reload: ReloadReport =
            serde_json::from_value(json!({"restarted": ["a"], "failed": [{"vault": "b"}]})).unwrap();
        assert_eq!((reload.restarted.len(), reload.failed[0].message.as_str()), (1, ""));
        let hello: Hello = serde_json::from_value(json!({"protocol": 1})).unwrap();
        assert!(hello.methods.is_empty());
        let resp: Response = serde_json::from_str(r#"{"id":1,"error":{"code":"busy"}}"#).unwrap();
        assert_eq!((resp.v, resp.error.unwrap().message.as_str()), (PROTOCOL, ""));
        let ev = Incoming::parse(r#"{"event":"pulled"}"#).unwrap();
        assert!(matches!(ev, Incoming::Event(e) if e.vault.is_none() && e.data.is_null()));
    }

    #[test]
    fn hello_says_when_the_service_cannot_be_used() {
        let hello = |protocol: u32, methods: &[&str]| Hello {
            protocol,
            version: "0.9.0".into(),
            methods: methods.iter().map(|m| m.to_string()).collect(),
            ..Default::default()
        };
        assert!(check_hello(&hello(PROTOCOL, method::ALL), method::ALL).is_ok());
        let e = check_hello(&hello(PROTOCOL + 1, method::ALL), &[]).unwrap_err();
        assert!(e.contains("newer") && e.contains("update this lsync"), "{e}");
        let e = check_hello(&hello(0, &[]), &[]).unwrap_err();
        assert!(e.contains("older") && e.contains("restart"), "{e}");
        let e = check_hello(
            &hello(PROTOCOL, &[method::HELLO, method::STATUS]),
            &[method::STATUS, "flush"],
        )
        .unwrap_err();
        assert!(e.contains("no `flush`") && e.contains("0.9.0"), "{e}");
    }

    #[test]
    fn unknown_result_fields_are_ignored() {
        let r: ReloadReport = serde_json::from_value(json!({
            "started": ["a"], "stopped": [], "restarted": [], "unchanged": [], "failed": [], "added_later": 1
        }))
        .unwrap();
        assert_eq!(r.started, vec!["a".to_string()]);
    }
}
