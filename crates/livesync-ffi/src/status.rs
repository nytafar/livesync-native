//! What the app shows: the connect-gate result and a per-profile status
//! snapshot (index counts, `since`, the last gate, files needing attention).

use crate::host::RunState;
use livesync_engine::{EngineError, GateReport, PullReport, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum GateOutcome {
    Passed,
    /// A compatibility gate refused (version, milestone lock, tweaks, ...).
    Refused,
    /// The server could not be reached.
    Unreachable,
    /// The server answered, but not as expected (wrong password, no such
    /// database, not CouchDB, ...).
    Failed,
}

/// The result of a connect test or a re-run of the gate.
#[derive(Debug, Clone, uniffi::Record)]
pub struct GateResult {
    pub outcome: GateOutcome,
    /// One line for the user.
    pub message: String,
    pub remote: String,
    pub version: i64,
    pub doc_count: i64,
    pub created: i64,
    pub locked: bool,
    pub accepted_nodes: i64,
    /// Locked against this node, but the first full fetch will mark it resolved.
    pub fetch_to_resolve: bool,
    pub rebuild_detected: bool,
    pub max_size_mb: i64,
    /// Document IDs keep their case (`handleFilenameCaseSensitive`).
    pub case_sensitive_ids: bool,
}

impl GateResult {
    pub(crate) fn passed(g: &GateReport) -> Self {
        let mut msg = format!("Compatible: version {}, {} documents", g.version, g.doc_count);
        if g.fetch_to_resolve {
            msg.push_str("; the database is locked, the first full fetch will mark this device resolved");
        }
        GateResult {
            outcome: GateOutcome::Passed,
            message: msg,
            remote: g.remote.clone(),
            version: g.version,
            doc_count: g.doc_count as i64,
            created: g.created,
            locked: g.locked,
            accepted_nodes: g.accepted_nodes as i64,
            fetch_to_resolve: g.fetch_to_resolve,
            rebuild_detected: g.rebuild_detected,
            max_size_mb: g.max_size_mb as i64,
            case_sensitive_ids: g.case_sensitive,
        }
    }

    pub(crate) fn failed(e: &EngineError) -> Self {
        let (outcome, message) = match e {
            EngineError::Refused(r) => (GateOutcome::Refused, format!("Refused: {r}")),
            EngineError::Remote(r) if matches!(r, livesync_remote::RemoteError::Transport { .. }) => {
                (GateOutcome::Unreachable, crate::explain_remote(r))
            }
            EngineError::Remote(r) => (GateOutcome::Failed, crate::explain_remote(r)),
            other => (GateOutcome::Failed, other.to_string()),
        };
        Self::with(outcome, message)
    }

    pub(crate) fn with(outcome: GateOutcome, message: String) -> Self {
        GateResult {
            outcome,
            message,
            remote: String::new(),
            version: 0,
            doc_count: 0,
            created: 0,
            locked: false,
            accepted_nodes: 0,
            fetch_to_resolve: false,
            rebuild_detected: false,
            max_size_mb: 0,
            case_sensitive_ids: false,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct StateCount {
    pub state: String,
    pub count: i64,
}

/// One file (or document) that needs the user's attention.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AttentionItem {
    /// dirty, conflicted, unwritable, collision, out_of_scope, local_issue or pending.
    pub kind: String,
    pub path: String,
    pub note: String,
}

/// Cap on the attention list handed to the UI.
pub const ATTENTION_CAP: usize = 200;

#[derive(Debug, Clone, uniffi::Record)]
pub struct ProfileStatus {
    pub profile_id: String,
    pub run_state: RunState,
    /// Why the profile is in this state (the error for `Failed`).
    pub run_message: Option<String>,
    pub pull_only: bool,
    /// Whether the index exists yet (false before the first start).
    pub has_index: bool,
    pub remote: Option<String>,
    pub node_id: Option<String>,
    /// The last fully applied `_changes` sequence (shortened).
    pub since: Option<String>,
    /// `"<ms> ok: ..."` or `"<ms> failed: ..."`.
    pub last_gate: Option<String>,
    pub last_gate_ok_ms: Option<i64>,
    pub last_error: Option<String>,
    pub last_pull_ms: Option<i64>,
    pub last_push_ms: Option<i64>,
    pub daemon_heartbeat_ms: Option<i64>,
    pub registered_ms: Option<i64>,
    pub rebuild_detected_ms: Option<i64>,
    pub counts: Vec<StateCount>,
    pub tracked_files: i64,
    pub dirty: i64,
    pub conflicted: i64,
    pub pending: i64,
    pub skipped: i64,
    pub out_of_scope: i64,
    pub local_issues: i64,
    pub attention: Vec<AttentionItem>,
    pub scope: Option<String>,
    pub needs_full_fetch: bool,
    pub safety_stop: Option<String>,
    /// The engine's full status as JSON (the debug screen's raw view).
    pub status_json: String,
    /// This engine session's pull counters as JSON (empty when not running).
    pub session_json: String,
}

/// `"306714-g1AAAA..."` (JSON-quoted in the index) → `306714-g1AAAA…`.
pub(crate) fn short_since(raw: &str) -> String {
    let s = raw.trim().trim_matches('"');
    if s.chars().count() > 24 {
        format!("{}…", s.chars().take(24).collect::<String>())
    } else {
        s.to_string()
    }
}

pub(crate) fn from_engine(
    profile_id: &str,
    run_state: RunState,
    run_message: Option<String>,
    pull_only: bool,
    st: Option<&Status>,
    session: Option<&PullReport>,
) -> ProfileStatus {
    let mut out = ProfileStatus {
        profile_id: profile_id.to_string(),
        run_state,
        run_message,
        pull_only,
        has_index: st.is_some(),
        remote: None,
        node_id: None,
        since: None,
        last_gate: None,
        last_gate_ok_ms: None,
        last_error: None,
        last_pull_ms: None,
        last_push_ms: None,
        daemon_heartbeat_ms: None,
        registered_ms: None,
        rebuild_detected_ms: None,
        counts: vec![],
        tracked_files: 0,
        dirty: 0,
        conflicted: 0,
        pending: 0,
        skipped: 0,
        out_of_scope: 0,
        local_issues: 0,
        attention: vec![],
        scope: None,
        needs_full_fetch: false,
        safety_stop: None,
        status_json: String::new(),
        session_json: session
            .map(|s| serde_json::to_string_pretty(s).unwrap_or_default())
            .unwrap_or_default(),
    };
    let Some(st) = st else { return out };
    let count = |k: &str| st.counts.get(k).copied().unwrap_or(0) as i64;
    out.remote = st.remote.clone();
    out.node_id = st.node_id.clone();
    out.since = st.since.as_deref().map(short_since);
    out.last_gate = st.last_gate.clone();
    out.last_gate_ok_ms = st.last_gate_ok_ms;
    out.last_error = st.last_error.clone();
    out.last_pull_ms = st.last_pull_ms;
    out.last_push_ms = st.last_push_ms;
    out.daemon_heartbeat_ms = st.daemon_heartbeat_ms;
    out.registered_ms = st.registered_ms;
    out.rebuild_detected_ms = st.rebuild_detected_ms;
    out.counts = st
        .counts
        .iter()
        .map(|(k, v)| StateCount {
            state: k.clone(),
            count: *v as i64,
        })
        .collect();
    out.tracked_files = count("clean") + count("dirty") + count("conflicted");
    out.dirty = count("dirty");
    out.conflicted = count("conflicted");
    out.skipped = count("skipped");
    out.out_of_scope = count("out_of_scope");
    out.pending = st.pending.len() as i64;
    out.local_issues = st.local_issues.len() as i64;
    out.scope = st.scope.clone();
    out.needs_full_fetch = st.needs_full_fetch;
    out.safety_stop = st.safety_stop.clone();
    let rows = [
        ("conflicted", &st.conflicted),
        ("dirty", &st.dirty),
        ("out_of_scope", &st.out_of_scope),
        ("unwritable", &st.unwritable),
        ("collision", &st.collisions),
    ];
    for (kind, list) in rows {
        for r in list.iter() {
            out.attention.push(AttentionItem {
                kind: kind.into(),
                path: r.path.clone(),
                note: r.note.clone().unwrap_or_default(),
            });
        }
    }
    for i in &st.local_issues {
        out.attention.push(AttentionItem {
            kind: "local_issue".into(),
            path: i.path.clone(),
            note: format!("{} ({})", i.note, i.kind),
        });
    }
    for p in &st.pending {
        out.attention.push(AttentionItem {
            kind: "pending".into(),
            path: p.doc_id.clone(),
            note: format!("{} (attempts {})", p.reason, p.attempts),
        });
    }
    out.attention.truncate(ATTENTION_CAP);
    out.status_json = serde_json::to_string_pretty(st).unwrap_or_default();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_is_unquoted_and_shortened() {
        assert_eq!(short_since("\"12\""), "12");
        assert_eq!(short_since("12"), "12");
        let long = format!("\"306714-{}\"", "g".repeat(40));
        let s = short_since(&long);
        assert!(s.starts_with("306714-ggg") && s.ends_with('…'), "{s}");
    }
}
