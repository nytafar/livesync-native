//! The FFI host against the engine's in-process mock CouchDB: profiles start
//! and stop, pull-only never writes, two-way pushes, overlapping vault
//! folders are refused, the connect test reports what the gate found.

#[path = "../../livesync-engine/tests/common/mod.rs"]
mod common;

use common::{Mock, DB};
use livesync_ffi::{GateOutcome, LiveSync, LiveSyncError, ProfileConfig, RunState, ScopeKind, StatusListener};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Env {
    _rt: tokio::runtime::Runtime,
    mock: Mock,
    dir: tempfile::TempDir,
    host: Arc<LiveSync>,
}

impl Env {
    fn new() -> Env {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mock = rt.block_on(Mock::start());
        let dir = tempfile::tempdir().unwrap();
        // Only the logging test sets a log folder: the log file is process-wide.
        let host = LiveSync::new(String::new()).unwrap();
        Env {
            _rt: rt,
            mock,
            dir,
            host,
        }
    }

    fn profile(&self, id: &str, vault: &str) -> ProfileConfig {
        ProfileConfig {
            id: id.into(),
            vault_path: self.dir.path().join(vault).display().to_string(),
            state_dir: self.dir.path().join("state").join(id).display().to_string(),
            url: self.mock.url.clone(),
            database: DB.into(),
            username: "u".into(),
            password: "p".into(),
            scope_kind: ScopeKind::All,
            scope_folders: vec![],
            pull_only: true,
            device_name: "test-phone".into(),
            ignore_regex: String::new(),
        }
    }

    fn status(&self, p: &ProfileConfig) -> livesync_ffi::ProfileStatus {
        self.host
            .status(p.id.clone(), p.vault_path.clone(), p.state_dir.clone())
    }
}

fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

fn read(p: &ProfileConfig, rel: &str) -> Option<String> {
    std::fs::read_to_string(Path::new(&p.vault_path).join(rel)).ok()
}

#[derive(Default, Clone)]
struct Recorder(Arc<Mutex<Vec<(String, RunState)>>>);

impl StatusListener for Recorder {
    fn on_state_changed(&self, profile_id: String, state: RunState, _message: Option<String>) {
        self.0.lock().unwrap().push((profile_id, state));
    }
}

#[test]
fn pull_only_profile_follows_the_remote_and_never_writes() {
    let env = Env::new();
    let rec = Recorder::default();
    env.host.set_listener(Box::new(rec.clone()));
    env.mock.put_text("notes/a.md", &["hello ", "world"]);
    env.mock.clear_requests();
    let p = env.profile("p1", "vault");
    env.host.start(p.clone()).unwrap();
    wait_until("a.md", Duration::from_secs(20), || {
        read(&p, "notes/a.md").as_deref() == Some("hello world")
    });
    let st = env.status(&p);
    assert_eq!(st.run_state, RunState::Running);
    assert!(st.pull_only);
    assert!(st.has_index);
    assert_eq!(st.tracked_files, 1);
    assert!(st.since.is_some());
    assert!(
        st.last_gate.as_deref().unwrap_or("").contains("ok"),
        "{:?}",
        st.last_gate
    );

    // A live change arrives through the longpoll.
    env.mock.put_text("notes/b.md", &["second"]);
    wait_until("b.md", Duration::from_secs(20), || {
        read(&p, "notes/b.md").as_deref() == Some("second")
    });

    // A local edit stays local in pull-only mode.
    std::fs::write(Path::new(&p.vault_path).join("notes/local.md"), "mine").unwrap();
    env.host.sync_now("p1".into()).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert!(env.mock.writes().is_empty(), "pull-only wrote: {:?}", env.mock.writes());

    env.host.stop("p1".into());
    let st = env.status(&p);
    assert_eq!(st.run_state, RunState::Stopped);
    assert!(st.has_index, "the index is read from the state folder when stopped");
    assert_eq!(st.tracked_files, 2);
    let seen: Vec<RunState> = rec.0.lock().unwrap().iter().map(|(_, s)| *s).collect();
    assert_eq!(seen.first(), Some(&RunState::Starting));
    assert!(seen.contains(&RunState::Running));
    assert_eq!(seen.last(), Some(&RunState::Stopped));
    assert!(env.host.active_profiles().is_empty());
}

#[test]
fn two_way_profile_pushes_local_files() {
    let env = Env::new();
    env.mock.put_text("a.md", &["remote"]);
    let mut p = env.profile("p2", "vault2");
    p.pull_only = false;
    env.host.start(p.clone()).unwrap();
    wait_until("a.md", Duration::from_secs(20), || read(&p, "a.md").is_some());
    std::fs::write(Path::new(&p.vault_path).join("new.md"), "from the phone").unwrap();
    wait_until("the push", Duration::from_secs(30), || {
        env.mock.content("new.md").as_deref() == Some(b"from the phone".as_slice())
    });
    let st = env.status(&p);
    assert!(st.last_push_ms.is_some());
    assert!(
        st.registered_ms.is_some(),
        "registered in the milestone after the first fetch"
    );
    env.host.stop_all();
}

#[test]
fn overlapping_vault_folders_are_refused() {
    let env = Env::new();
    let a = env.profile("a", "vaults/one");
    env.host.start(a.clone()).unwrap();
    for (id, vault) in [("b", "vaults/one"), ("c", "vaults/one/inner"), ("d", "vaults")] {
        let r = env.host.start(env.profile(id, vault));
        assert!(matches!(r, Err(LiveSyncError::Busy { .. })), "{id}: {r:?}");
    }
    // Same-case-insensitive spelling of the same folder.
    let r = env.host.start(env.profile("e", "VAULTS/ONE"));
    assert!(matches!(r, Err(LiveSyncError::Busy { .. })), "{r:?}");
    // A sibling is fine, and so is restarting the same profile.
    env.host.start(env.profile("f", "vaults/two")).unwrap();
    let mut a2 = a.clone();
    a2.scope_kind = ScopeKind::Include;
    a2.scope_folders = vec!["notes".into()];
    env.host.start(a2).unwrap();
    assert_eq!(env.host.active_profiles(), vec!["a".to_string(), "f".to_string()]);
    env.host.stop_all();
    assert!(env.host.active_profiles().is_empty());
}

#[test]
fn invalid_profiles_are_refused_before_starting() {
    let env = Env::new();
    let mut p = env.profile("x", "vault");
    p.state_dir = format!("{}/state", p.vault_path);
    assert!(matches!(env.host.start(p), Err(LiveSyncError::Invalid { .. })));
    let mut p = env.profile("x", "vault");
    p.scope_kind = ScopeKind::Exclude;
    assert!(matches!(env.host.start(p), Err(LiveSyncError::Invalid { .. })));
    assert!(env.host.active_profiles().is_empty());
}

#[test]
fn connect_test_reports_the_gate_without_touching_the_vault() {
    let env = Env::new();
    let scratch = env.dir.path().join("cache").display().to_string();
    let p = env.profile("t", "vault-t");
    let g = env.host.connect_test(p.clone(), scratch.clone());
    assert_eq!(g.outcome, GateOutcome::Passed, "{}", g.message);
    assert_eq!(g.version, 12);
    assert!(
        !Path::new(&p.vault_path).exists(),
        "the real vault folder is not created"
    );
    assert!(
        !Path::new(&p.state_dir).exists(),
        "the real state folder is not created"
    );
    assert_eq!(
        std::fs::read_dir(&scratch).map(|d| d.count()).unwrap_or(0),
        0,
        "scratch cleaned"
    );
    assert!(env.mock.writes().is_empty());

    let mut wrong_db = p.clone();
    wrong_db.database = "nope".into();
    let g = env.host.connect_test(wrong_db, scratch.clone());
    assert_eq!(g.outcome, GateOutcome::Failed, "{}", g.message);

    env.mock.update_milestone(|m| {
        m["locked"] = serde_json::json!(true);
        m["accepted_nodes"] = serde_json::json!(["someone-else"]);
    });
    let g = env.host.connect_test(p.clone(), scratch.clone());
    // A fresh index may pass a lock (the first full fetch marks it resolved).
    assert!(
        g.outcome == GateOutcome::Passed && g.fetch_to_resolve || g.outcome == GateOutcome::Refused,
        "{:?} {}",
        g.outcome,
        g.message
    );

    let mut closed = p.clone();
    closed.url = "http://127.0.0.1:9".into();
    let g = env.host.connect_test(closed, scratch);
    assert_eq!(g.outcome, GateOutcome::Unreachable, "{}", g.message);
}

#[test]
fn logs_reach_the_ring_and_the_file() {
    let env = Env::new();
    let dir = env.dir.path().join("logs");
    let host = LiveSync::new(dir.display().to_string()).unwrap();
    host.log("info".into(), "hello from the app".into());
    let lines = host.recent_log_lines(50);
    assert!(lines.iter().any(|l| l.contains("hello from the app")), "{lines:?}");
    let files = host.log_files();
    assert!(!files.is_empty());
    let text: String = files
        .iter()
        .map(|f| std::fs::read_to_string(f).unwrap_or_default())
        .collect();
    assert!(text.contains("hello from the app"));
    assert!(files.iter().all(|f| f.starts_with(&dir.display().to_string())));
}

#[derive(Clone, Default)]
struct Access(Arc<Mutex<Option<String>>>);

impl livesync_ffi::VaultAccess for Access {
    fn problem(&self, _vault_path: String) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
}

fn soft_deleted(env: &Env, id: &str) -> bool {
    env.mock
        .doc(id)
        .map(|d| d["deleted"] == serde_json::json!(true) || d["_deleted"] == serde_json::json!(true))
        .unwrap_or(false)
}

#[test]
fn a_failing_access_check_keeps_local_deletions_off_the_server() {
    let env = Env::new();
    let access = Access::default();
    env.host.set_vault_access(Box::new(access.clone()));
    env.mock.put_text("keep.md", &["kept"]);
    let mut p = env.profile("acc", "vault-acc");
    p.pull_only = false;
    env.host.start(p.clone()).unwrap();
    wait_until("keep.md", Duration::from_secs(20), || read(&p, "keep.md").is_some());
    wait_until("registration", Duration::from_secs(20), || {
        env.status(&p).registered_ms.is_some()
    });

    // All-files access revoked: the file looks gone, but must not be deleted remotely.
    *access.0.lock().unwrap() = Some("all-files access is not granted".into());
    std::fs::remove_file(Path::new(&p.vault_path).join("keep.md")).unwrap();
    env.host.sync_now("acc".into()).unwrap();
    std::thread::sleep(Duration::from_secs(6));
    assert!(!soft_deleted(&env, "keep.md"), "deleted while the access check failed");

    // Access is back: the deletion is real and goes through.
    *access.0.lock().unwrap() = None;
    env.host.sync_now("acc".into()).unwrap();
    wait_until("the soft delete", Duration::from_secs(30), || {
        soft_deleted(&env, "keep.md")
    });
    env.host.stop_all();
}

struct Throwing;

impl StatusListener for Throwing {
    fn on_state_changed(&self, _profile_id: String, _state: RunState, _message: Option<String>) {
        panic!("a foreign callback failed");
    }
}

#[test]
fn a_failing_listener_does_not_stop_syncing() {
    let env = Env::new();
    env.host.set_listener(Box::new(Throwing));
    env.mock.put_text("x.md", &["x"]);
    let p = env.profile("thr", "vault-thr");
    env.host.start(p.clone()).unwrap();
    wait_until("x.md", Duration::from_secs(20), || read(&p, "x.md").is_some());
    assert_eq!(env.status(&p).run_state, RunState::Running);
    env.host.stop("thr".into());
    assert_eq!(env.status(&p).run_state, RunState::Stopped);
}
