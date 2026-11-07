//! The host runs vaults from `lsync.toml` (the seam `lsync serve` builds
//! on): two vaults on one database with different scopes, each its own
//! node, side by side under one host on the engine's mock CouchDB. The
//! Android wrapper's own tests (`livesync-ffi/tests/host.rs`) cover the
//! supervision in depth.

#[path = "../../livesync-engine/tests/common/mod.rs"]
mod common;

use common::{Mock, DB};
use livesync_host::config::{Config, Dirs};
use livesync_host::{Host, RunState, StateListener};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

#[derive(Default)]
struct Seen(Mutex<Vec<(String, RunState)>>);

impl StateListener for Seen {
    fn on_state_changed(&self, id: String, state: RunState, _message: Option<String>) {
        self.0.lock().unwrap().push((id, state));
    }
}

#[test]
fn vaults_from_the_config_run_side_by_side_as_separate_nodes() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mock = rt.block_on(Mock::start());
    mock.put_text("Notes/a.md", &["note a"]);
    mock.put_text("Work/b.md", &["work b"]);
    let d = tempfile::tempdir().unwrap();
    let dirs = Dirs {
        home: Some(d.path().join("home")),
        config: d.path().join("config/lsync"),
        state: d.path().join("state/lsync"),
    };
    let text = format!(
        "[account.dev]\nurl = \"{}\"\nusername = \"u\"\n\n[vault.notes]\naccount = \"dev\"\ndatabase = \"{DB}\"\nfolder = \"~/notes\"\ninclude = [\"Notes\"]\nmode = \"pull-only\"\n\n[vault.work]\naccount = \"dev\"\ndatabase = \"{DB}\"\nfolder = \"~/work\"\nexclude = [\"Notes\"]\nmode = \"pull-only\"\n",
        mock.url
    );
    let cfg = Config::parse(&text, &dirs).unwrap();
    let host = Host::new().unwrap();
    let seen = Arc::new(Seen::default());
    host.set_listener(seen.clone());
    for (name, v) in &cfg.vault {
        let rc = v.run_config(name, &cfg.account[&v.account], Some("p"), &dirs).unwrap();
        host.start(rc).unwrap();
    }
    assert_eq!(host.active(), vec!["notes".to_string(), "work".to_string()]);
    let home = d.path().join("home");
    wait_until("both vaults", || {
        home.join("notes/Notes/a.md").exists() && home.join("work/Work/b.md").exists()
    });
    assert!(!home.join("notes/Work").exists(), "each vault syncs its own scope");
    assert!(!home.join("work/Notes").exists());
    let node = |name: &str| {
        let v = &cfg.vault[name];
        host.status(
            name,
            &v.folder_path(&dirs).display().to_string(),
            &v.state_path(name, &dirs).display().to_string(),
        )
        .snapshot
        .and_then(|s| s.node_id)
        .unwrap()
    };
    assert_ne!(node("notes"), node("work"), "two nodes on one database");
    assert_eq!(host.status("notes", "", "").state, RunState::Running);
    host.stop_all();
    assert!(host.active().is_empty());
    let states = seen.0.lock().unwrap().clone();
    for name in ["notes", "work"] {
        assert!(states.contains(&(name.to_string(), RunState::Running)), "{states:?}");
        assert!(states.contains(&(name.to_string(), RunState::Stopped)), "{states:?}");
    }
    assert!(mock.writes().is_empty(), "pull-only vaults never write");
}

#[test]
fn an_upstream_change_stops_only_the_vault_on_that_database() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (changed, other) = rt.block_on(async { (Mock::start().await, Mock::start().await) });
    changed.put_text("a.md", &["a"]);
    other.put_text("b.md", &["b"]);
    let d = tempfile::tempdir().unwrap();
    let dirs = Dirs {
        home: Some(d.path().join("home")),
        config: d.path().join("config/lsync"),
        state: d.path().join("state/lsync"),
    };
    // Two-way vaults with the guard's timings from lsync.toml.
    let text = format!(
        "[account.one]\nurl = \"{}\"\nusername = \"u\"\n\n[account.two]\nurl = \"{}\"\nusername = \"u\"\n\n\
         [vault.changed]\naccount = \"one\"\ndatabase = \"{DB}\"\nfolder = \"~/changed\"\nregate_secs = 1\nheartbeat_secs = 1\nlongpoll_timeout_secs = 3\n\n\
         [vault.other]\naccount = \"two\"\ndatabase = \"{DB}\"\nfolder = \"~/other\"\nregate_secs = 1\nclock_jump_secs = 0\n",
        changed.url, other.url
    );
    let cfg = Config::parse(&text, &dirs).unwrap();
    let host = Host::new().unwrap();
    for (name, v) in &cfg.vault {
        let rc = v.run_config(name, &cfg.account[&v.account], Some("p"), &dirs).unwrap();
        host.start(rc).unwrap();
    }
    let home = d.path().join("home");
    wait_until("both vaults", || {
        home.join("changed/a.md").exists() && home.join("other/b.md").exists()
    });
    let status = |name: &str| {
        let v = &cfg.vault[name];
        host.status(
            name,
            &v.folder_path(&dirs).display().to_string(),
            &v.state_path(name, &dirs).display().to_string(),
        )
    };
    // A newer plugin turns a feature on in one database.
    changed.put(serde_json::json!({"_id": "obsydian_livesync_version", "type": "versioninfo", "version": 13, "used_features": ["teleport-v1"]}));
    wait_until("the upstream stop", || {
        status("changed")
            .snapshot
            .and_then(|s| s.upstream_stop)
            .is_some_and(|m| m.contains("teleport-v1"))
    });
    // The vault keeps running (stopped inside the engine, not failed), and
    // the other vault syncs on.
    assert_eq!(status("changed").state, RunState::Running);
    other.put_text("b2.md", &["b2"]);
    wait_until("the other vault syncs on", || home.join("other/b2.md").exists());
    assert!(status("other").snapshot.unwrap().upstream_stop.is_none());
    changed.put_text("a2.md", &["a2"]);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!home.join("changed/a2.md").exists(), "the stopped vault pulled");
    // The remote goes back: the vault resumes by itself.
    changed.put(serde_json::json!({"_id": "obsydian_livesync_version", "type": "versioninfo", "version": 12}));
    wait_until("the stopped vault resumes", || home.join("changed/a2.md").exists());
    assert!(status("changed").snapshot.unwrap().upstream_stop.is_none());
    host.stop_all();
}

#[test]
fn the_guard_settings_are_checked_with_the_config() {
    let d = tempfile::tempdir().unwrap();
    let dirs = Dirs {
        home: Some(d.path().join("home")),
        config: d.path().join("config/lsync"),
        state: d.path().join("state/lsync"),
    };
    let base = "[account.a]\nurl = \"https://couch.example.org\"\n\n[vault.v]\naccount = \"a\"\ndatabase = \"db\"\nfolder = \"~/v\"\n";
    let ec = |extra: &str| {
        let cfg = Config::parse(&format!("{base}{extra}"), &dirs).unwrap();
        cfg.vault["v"]
            .run_config("v", &cfg.account["a"], None, &dirs)
            .unwrap()
            .engine_config()
            .unwrap()
    };
    let c = ec("allow_plugin = true\nregate_secs = 60\n");
    assert!(c.allow_plugin);
    assert_eq!(c.regate_interval, Duration::from_secs(60));
    // Only a heartbeat: the timeout follows at 2.5 heartbeats.
    let c = ec("heartbeat_secs = 30\nclock_jump_secs = 0\n");
    assert_eq!(c.longpoll_timeout, Duration::from_secs(75));
    assert_eq!(c.clock_jump, None);
    // A long heartbeat: the derived timeout stays below the HTTP read
    // timeout, so the longpoll's silence check still fires first.
    let c = ec("heartbeat_secs = 60\n");
    assert_eq!(c.longpoll_timeout, Duration::from_secs(89));
    assert!(c.longpoll_timeout < livesync_remote::READ_TIMEOUT);
    let c = ec("");
    assert!(!c.allow_plugin);
    // lsync names its handover switch in the plugin guard's refusal.
    assert!(c
        .plugin_handover
        .as_deref()
        .is_some_and(|h| h.contains("--allow-plugin")));
    for (bad, says) in [
        (
            "heartbeat_secs = 30\nlongpoll_timeout_secs = 20\n",
            "must be longer than heartbeat_secs",
        ),
        // Past the HTTP client's 90 s read timeout: every longpoll would end
        // in a transport error, or a dead link would never count as silent.
        ("heartbeat_secs = 120\n", "heartbeat_secs (120) must be below 89"),
        ("heartbeat_secs = 89\n", "heartbeat_secs (89) must be below 89"),
        (
            "longpoll_timeout_secs = 95\n",
            "longpoll_timeout_secs (95) must be below 90 s",
        ),
        (
            "longpoll_timeout_secs = 90\n",
            "longpoll_timeout_secs (90) must be below 90 s",
        ),
        ("regate_secs = 0\n", "regate_secs must be at least 1"),
        ("heartbeat_secs = 0\n", "heartbeat_secs must be at least 1"),
    ] {
        let e = Config::parse(&format!("{base}{bad}"), &dirs).unwrap_err();
        assert!(e.contains("[vault.v]") && e.contains(says), "{e}");
    }
}
