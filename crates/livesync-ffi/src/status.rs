//! What the app shows: the connect-gate result and a per-profile status
//! snapshot (index counts, `since`, the last gate, files needing attention).

use crate::host::RunState;
use livesync_engine::{EngineError, GateReport, PullReport, PushReport, Status};

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
    /// A full fetch or a resolution is owed; pushes are held until a two-way
    /// run completes it.
    pub needs_full_fetch: bool,
    /// `"<ms> <text>"`: a mass deletion was refused and waits for the user.
    pub safety_stop: Option<String>,
    /// A confirmed mass delete is waiting for its (one) full push pass.
    pub mass_delete_allowed: bool,
    /// `"<ms> <text>"`: waiting for a rebuilding device to finish uploading.
    pub rebuild_wait: Option<String>,
    /// `"<ms> <count> file(s) ...: paths"`: files the remote lacked after a
    /// rebuild or full fetch, pushed back from this device; worth checking.
    pub rebuild_orphans: Option<String>,
    /// Rows kept because another device deleted the document while this
    /// device's copy was in conflict (or had changed).
    pub deleted_elsewhere: i64,
    /// This engine session's push totals (two-way only; zero otherwise).
    pub pushed: i64,
    pub push_conflicts: i64,
    pub deleted_remote: i64,
    pub deleted_locally: i64,
    pub orphans_pushed: i64,
    pub push_failed: i64,
    pub unreadable: i64,
    pub push_json: String,
    /// The engine's full status as JSON (the debug screen's raw view).
    pub status_json: String,
    /// This engine session's pull counters as JSON (empty when not running).
    pub session_json: String,
}

/// The engine's note prefix for rows kept after a deletion elsewhere.
pub const DELETED_ELSEWHERE: &str = "deleted on another device";

/// Add the running two-way daemon's push totals.
pub(crate) fn add_push(out: &mut ProfileStatus, push: Option<&PushReport>) {
    let Some(p) = push else { return };
    out.pushed = p.pushed as i64;
    out.push_conflicts = p.conflicts as i64;
    out.deleted_remote = p.deleted as i64;
    out.deleted_locally = p.deleted_locally as i64;
    out.orphans_pushed = p.orphans_pushed as i64;
    out.push_failed = p.failed as i64;
    out.unreadable = p.unreadable as i64;
    out.push_json = serde_json::to_string_pretty(p).unwrap_or_default();
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
        mass_delete_allowed: false,
        rebuild_wait: None,
        rebuild_orphans: None,
        deleted_elsewhere: 0,
        pushed: 0,
        push_conflicts: 0,
        deleted_remote: 0,
        deleted_locally: 0,
        orphans_pushed: 0,
        push_failed: 0,
        unreadable: 0,
        push_json: String::new(),
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
    out.rebuild_wait = st.rebuild_wait.clone();
    out.rebuild_orphans = st.rebuild_orphans.clone();
    let rows = [
        ("conflicted", &st.conflicted),
        ("dirty", &st.dirty),
        ("out_of_scope", &st.out_of_scope),
        ("unwritable", &st.unwritable),
        ("collision", &st.collisions),
    ];
    // Deleted on another device while this copy was kept: first, so they
    // are seen (they need a decision: keep the file, or delete it here).
    let mut kept = Vec::new();
    let mut rest = Vec::new();
    for (kind, list) in rows {
        for r in list.iter() {
            let note = r.note.clone().unwrap_or_default();
            if note.contains(DELETED_ELSEWHERE) {
                kept.push(AttentionItem {
                    kind: "deleted_elsewhere".into(),
                    path: r.path.clone(),
                    note,
                });
            } else {
                rest.push(AttentionItem {
                    kind: kind.into(),
                    path: r.path.clone(),
                    note,
                });
            }
        }
    }
    out.deleted_elsewhere = kept.len() as i64;
    out.attention.extend(kept);
    out.attention.extend(rest);
    for i in &st.local_issues {
        out.attention.push(AttentionItem {
            // unreadable, push-failed, stale-copy, case collisions, ...
            kind: i.kind.clone(),
            path: i.path.clone(),
            note: i.note.clone(),
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
