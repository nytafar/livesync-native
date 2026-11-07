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
