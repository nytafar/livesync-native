//! `flush` and `wait` through the service's control socket, on the engine's
//! mock CouchDB: two vaults on one database stand for two machines. A flush
//! on one answers with the revision on the server, a wait on the other ends
//! when exactly that revision arrived; timeouts, stops and bad requests get
//! their codes; the revisions show up as events.

#[path = "../../livesync-engine/tests/common/mod.rs"]
mod common;

use common::{Mock, DB};
use livesync_host::config::Dirs;
use livesync_host::control::{self, Client, Policy};
use livesync_host::protocol::{code, method, Event, StatusResult, WaitResult};
use livesync_host::secret::SecretStores;
use livesync_host::service::{ConfigSource, Service};
use livesync_host::Host;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

struct Fixture {
    rt: tokio::runtime::Runtime,
    mock: Mock,
    dir: tempfile::TempDir,
    dirs: Dirs,
}

impl Fixture {
    fn new(vaults: &str) -> Fixture {
        Fixture::with_workers(vaults, 4)
    }

    fn with_workers(vaults: &str, workers: usize) -> Fixture {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()
            .unwrap();
        let mock = rt.block_on(Mock::start());
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            home: Some(dir.path().join("home")),
            config: dir.path().join("config/lsync"),
            state: dir.path().join("state/lsync"),
        };
        std::fs::create_dir_all(&dirs.config).unwrap();
        let secrets = dirs.config.join("secrets.toml");
        std::fs::write(&secrets, "\"dev\" = \"pw\"\n").unwrap();
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(
            dirs.config_file(),
            format!("[account.dev]\nurl = \"{}\"\nusername = \"u\"\n\n{vaults}", mock.url),
        )
        .unwrap();
        Fixture { rt, mock, dir, dirs }
    }

    fn start(&self) -> (Arc<Service>, PathBuf, tokio::sync::watch::Sender<bool>) {
        let host = Arc::new(Host::on_runtime(self.rt.handle().clone()));
        host.set_stop_timeouts_for_tests(Duration::from_secs(5), Duration::from_secs(5));
        let source = ConfigSource {
            path: self.dirs.config_file(),
            dirs: self.dirs.clone(),
            secrets: SecretStores::standard(self.dirs.config.join("secrets.toml")),
            only: None,
        };
        let svc = {
            let _rt = self.rt.enter();
            let svc = Service::new(host, Box::new(source), "test");
            svc.spawn_activity();
            svc
        };
        self.rt.block_on(svc.reload()).unwrap();
        let bound = control::bind(&self.dir.path().join("run"), &Policy::own_user()).unwrap();
        let path = bound.path().to_path_buf();
        let (tx, rx) = tokio::sync::watch::channel(false);
        self.rt
            .spawn(control::serve(bound, svc.clone(), Policy::own_user(), rx));
        (svc, path, tx)
    }

    fn home(&self, rel: &str) -> PathBuf {
        self.dir.path().join("home").join(rel)
    }
}

fn vault(name: &str, extra: &str) -> String {
    format!("[vault.{name}]\naccount = \"dev\"\ndatabase = \"{DB}\"\nfolder = \"~/{name}\"\n{extra}\n")
}

fn subscribe(path: &std::path::Path) -> Arc<Mutex<Vec<Event>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut c = Client::connect(path).unwrap().unwrap();
    c.subscribe(None).unwrap();
    let out = seen.clone();
    std::thread::spawn(move || {
        while let Ok(e) = c.next_event() {
            out.lock().unwrap().push(e);
        }
    });
    seen
}

fn live(c: &mut Client, name: &str) -> bool {
    let st: StatusResult = serde_json::from_value(c.call(method::STATUS, json!({"vault": name})).unwrap()).unwrap();
    st.vaults[0]
        .activity
        .as_ref()
        .is_some_and(|a| a.pull == livesync_engine::PullPhase::Live && (a.pushes || !a.two_way))
}

#[test]
fn a_flush_on_one_vault_and_a_wait_on_another_hand_a_file_over() {
    let f = Fixture::new(&format!(
        "{}{}{}",
        vault("a", ""),
        vault("b", ""),
        vault("reader", "mode = \"pull-only\"")
    ));
    let (svc, path, stop) = f.start();
    let seen = subscribe(&path);
    // The service's other events, without the revisions (their own channel).
    let mut others = svc.subscribe();
    let mut c = Client::connect(&path).unwrap().unwrap();
    let hello = c.call(method::HELLO, Value::Null).unwrap();
    for m in [method::FLUSH, method::WAIT] {
        assert!(hello["methods"].as_array().unwrap().contains(&json!(m)), "{m}");
    }
    wait_until("all three live", || {
        live(&mut c, "a") && live(&mut c, "b") && live(&mut c, "reader")
    });

    std::fs::write(f.home("a/handoff.md"), "for the other machine").unwrap();
    let rep = c
        .call(
            method::FLUSH,
            json!({"vault": "a", "paths": ["handoff.md"], "timeout_ms": 20_000}),
        )
        .unwrap();
    assert_eq!(rep["complete"], true, "{rep}");
    let p = &rep["paths"][0];
    assert_eq!(
        (p["path"].as_str(), p["outcome"].as_str()),
        (Some("handoff.md"), Some("stored"))
    );
    let rev = p["rev"].as_str().unwrap().to_string();
    assert_eq!(f.mock.doc("handoff.md").unwrap()["_rev"], rev.as_str());

    let r: WaitResult = serde_json::from_value(
        c.call(
            method::WAIT,
            json!({"vault": "b", "path": "handoff.md", "rev": rev, "timeout_ms": 20_000}),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!((r.vault.as_str(), r.condition.as_str()), ("b", "applied"));
    assert_eq!(r.reached.current_rev.as_deref(), Some(rev.as_str()));
    assert_eq!(
        std::fs::read_to_string(f.home("b/handoff.md")).unwrap(),
        "for the other machine"
    );
    // Without a revision: the server's current one. The pull-only vault gets it too.
    let r = c
        .call(
            method::WAIT,
            json!({"vault": "reader", "path": "handoff.md", "timeout_ms": 20_000}),
        )
        .unwrap();
    assert_eq!(r["current_rev"], rev.as_str());
    // Idle, once nothing is pending.
    let r = c
        .call(method::WAIT, json!({"vault": "b", "idle": true, "timeout_ms": 20_000}))
        .unwrap();
    assert_eq!(r["condition"], "idle");

    // The revisions as events, with the vault they are about.
    wait_until("stored and applied events", || {
        let ev = seen.lock().unwrap();
        let has = |kind: &str, vault: &str| {
            ev.iter().any(|e| {
                e.event == kind
                    && e.vault.as_deref() == Some(vault)
                    && e.data["path"] == "handoff.md"
                    && e.data["rev"] == rev.as_str()
            })
        };
        has("stored", "a") && has("applied", "b") && has("applied", "reader")
    });
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.event == "activity" && e.vault.as_deref() == Some("b") && e.data["connected"] == true));

    // A revision that never comes: the timeout's code, on time.
    let t0 = Instant::now();
    let e = c
        .call(
            method::WAIT,
            json!({"vault": "b", "path": "handoff.md", "rev": "9-0000", "timeout_ms": 500}),
        )
        .unwrap_err();
    assert_eq!(e.code(), Some(code::TIMEOUT), "{e}");
    assert!(e.to_string().contains("9-0000"), "{e}");
    assert!(t0.elapsed() < Duration::from_secs(5));

    // What cannot be done gets its code at once.
    let e = c.call(method::FLUSH, json!({"vault": "reader"})).unwrap_err();
    assert!(
        e.code() == Some(code::FAILED) && e.to_string().contains("pull-only"),
        "{e}"
    );
    let e = c
        .call(method::FLUSH, json!({"vault": "a", "paths": ["../outside.md"]}))
        .unwrap_err();
    assert_eq!(e.code(), Some(code::BAD_REQUEST));
    let e = c.call(method::WAIT, json!({"vault": "b"})).unwrap_err();
    assert_eq!(e.code(), Some(code::BAD_REQUEST));
    for bad in ["nope", "3-", "99999999999999999999999-abc"] {
        let e = c
            .call(method::WAIT, json!({"vault": "b", "path": "handoff.md", "rev": bad}))
            .unwrap_err();
        assert_eq!(e.code(), Some(code::BAD_REQUEST), "{bad}: {e}");
    }
    loop {
        match others.try_recv() {
            Ok(e) => assert!(e.event != "stored" && e.event != "applied", "{e:?}"),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(e) => panic!("{e}"),
        }
    }
    let e = c
        .call(method::WAIT, json!({"vault": "nope", "idle": true}))
        .unwrap_err();
    assert_eq!(e.code(), Some(code::UNKNOWN_VAULT));
    c.call(method::PAUSE, json!({"vault": "b"})).unwrap();
    let e = c
        .call(method::WAIT, json!({"vault": "b", "idle": true, "timeout_ms": 5_000}))
        .unwrap_err();
    assert_eq!(e.code(), Some(code::PAUSED), "{e}");
    c.call(method::RESUME, json!({"vault": "b"})).unwrap();

    // A safety stop is for a person: a wait for idle ends at once.
    livesync_engine::Index::open(&f.dirs.state.join("b/index.sqlite"))
        .unwrap()
        .set_meta("safety_stop", "1 30 files went missing")
        .unwrap();
    let e = c
        .call(method::WAIT, json!({"vault": "b", "idle": true, "timeout_ms": 20_000}))
        .unwrap_err();
    assert_eq!(e.code(), Some(code::BLOCKED), "{e}");
    assert!(e.to_string().contains("30 files"), "{e}");

    let _ = stop.send(true);
    f.rt.block_on(svc.shutdown());
}

#[test]
fn a_wait_covers_a_vault_still_starting_and_a_flush_ends_with_its_client() {
    let f = Fixture::new(&vault("a", ""));
    let (svc, path, stop) = f.start();
    // At once after the start: the wait covers the engine's start.
    let mut c = Client::connect(&path).unwrap().unwrap();
    let r = c
        .call(method::WAIT, json!({"vault": "a", "idle": true, "timeout_ms": 20_000}))
        .unwrap();
    assert_eq!(r["condition"], "idle");

    // A flush without a timeout whose client goes away is not left behind.
    // Pushes are held here (a full fetch is owed), so it would wait forever.
    livesync_engine::Index::open(&f.dirs.state.join("a/index.sqlite"))
        .unwrap()
        .set_meta("needs_full_fetch", "1")
        .unwrap();
    std::fs::write(f.home("a/held.md"), "held").unwrap();
    let flushes = |c: &mut Client| -> u64 {
        let st: StatusResult = serde_json::from_value(c.call(method::STATUS, json!({"vault": "a"})).unwrap()).unwrap();
        st.vaults[0].activity.as_ref().map_or(0, |a| a.flushes)
    };
    {
        let mut gone = Client::connect(&path).unwrap().unwrap();
        gone.set_timeout(Some(Duration::from_millis(500)));
        let e = gone.call(method::FLUSH, json!({"vault": "a"})).unwrap_err();
        assert!(e.to_string().contains("did not answer"), "{e}");
        assert_eq!(flushes(&mut c), 1, "it waits in the push loop");
    }
    wait_until("the flush dropped with its client", || flushes(&mut c) == 0);
    let t0 = Instant::now();
    let _ = stop.send(true);
    f.rt.block_on(svc.shutdown());
    assert!(t0.elapsed() < Duration::from_secs(10));
}

#[test]
fn waits_and_flushes_across_an_engine_restart_leave_the_service_running() {
    // Few workers: waits that spun on an ended engine would hold all of
    // them, and the restart (and everything else) would never get one.
    let f = Fixture::with_workers(&vault("a", ""), 2);
    let (svc, path, stop) = f.start();
    let mut c = Client::connect(&path).unwrap().unwrap();
    c.set_timeout(Some(Duration::from_secs(10)));
    wait_until("live", || live(&mut c, "a"));

    // A slow server: a push pass is in flight when the engine ends, and the
    // engine is only gone once that pass is over.
    f.mock.state.lock().unwrap().write_delay = Some(Duration::from_secs(3));
    let mut calls = Vec::new();
    for i in 0..3 {
        std::fs::write(f.home(&format!("a/f{i}.md")), format!("flushed {i}")).unwrap();
        let p = path.clone();
        calls.push(std::thread::spawn(move || {
            let mut c = Client::connect(&p).unwrap().unwrap();
            c.set_timeout(Some(Duration::from_secs(90)));
            c.call(
                method::FLUSH,
                json!({"vault": "a", "paths": [format!("f{i}.md")], "timeout_ms": 60_000}),
            )
        }));
        let p = path.clone();
        calls.push(std::thread::spawn(move || {
            let mut c = Client::connect(&p).unwrap().unwrap();
            c.set_timeout(Some(Duration::from_secs(90)));
            c.call(method::WAIT, json!({"vault": "a", "idle": true, "timeout_ms": 60_000}))
        }));
    }
    let flushes = |c: &mut Client| -> u64 {
        let st: StatusResult = serde_json::from_value(c.call(method::STATUS, json!({"vault": "a"})).unwrap()).unwrap();
        st.vaults[0].activity.as_ref().map_or(0, |a| a.flushes)
    };
    let pushing = |c: &mut Client| -> bool {
        let st: StatusResult = serde_json::from_value(c.call(method::STATUS, json!({"vault": "a"})).unwrap()).unwrap();
        st.vaults[0].activity.as_ref().is_some_and(|a| a.pushing)
    };
    wait_until("a flush's pass in flight", || flushes(&mut c) > 0 && pushing(&mut c));

    // The engine ends by itself: the plugin was turned on in the folder.
    let obsidian = f.home("a/.obsidian");
    std::fs::create_dir_all(obsidian.join("plugins/obsidian-livesync")).unwrap();
    std::fs::write(obsidian.join("community-plugins.json"), r#"["obsidian-livesync"]"#).unwrap();
    c.call(method::SYNC_NOW, json!({"vault": "a"})).unwrap();
    let ended = || {
        let st = svc.status(Some("a")).unwrap();
        st.vaults[0].state == "failed"
    };
    // Checked from this thread: a starved runtime cannot answer at all.
    let t0 = Instant::now();
    while !ended() && t0.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let answers = c.call(method::STATUS, json!({"vault": "a"}));
    if !ended() || answers.is_err() {
        // The runtime is stuck: leave it behind rather than hang on it.
        std::mem::forget(svc);
        std::mem::forget(f);
        panic!("the service stopped answering while its engine restarted: {answers:?}");
    }

    // Back: the plugin off again, the server taking writes, a retry now.
    std::fs::remove_file(obsidian.join("community-plugins.json")).unwrap();
    f.mock.state.lock().unwrap().write_delay = None;
    c.call(method::SYNC_NOW, json!({"vault": "a"})).unwrap();
    for (i, call) in calls.into_iter().enumerate() {
        let r = call.join().unwrap();
        let v = r.unwrap_or_else(|e| panic!("call {i}: {e}"));
        if i % 2 == 0 {
            assert_eq!(v["complete"], true, "{v}");
        } else {
            assert_eq!(v["condition"], "idle", "{v}");
        }
    }
    for i in 0..3 {
        assert!(f.mock.doc(&format!("f{i}.md")).is_some());
    }
    let _ = stop.send(true);
    f.rt.block_on(svc.shutdown());
}

#[test]
fn a_file_in_conflict_again_after_it_was_resolved_is_told_again() {
    let f = Fixture::new(&vault("a", "mode = \"pull-only\""));
    let first = f.mock.put_text("c.md", &["one"]);
    let (svc, path, stop) = f.start();
    let seen = subscribe(&path);
    let mut c = Client::connect(&path).unwrap().unwrap();
    wait_until("live", || live(&mut c, "a") && f.home("a/c.md").exists());
    let conflicts = || -> Vec<Vec<String>> {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|e| e.event == "conflict")
            .map(|e| serde_json::from_value(e.data["open"].clone()).unwrap())
            .collect()
    };

    // Two edits of one revision: a conflict.
    let loser = f
        .mock
        .put_branch_with(f.mock.text_doc("c.md", "lost"), &first, "0000aaaa");
    let kept = f
        .mock
        .put_branch_with(f.mock.text_doc("c.md", "kept"), &first, "ffffbbbb");
    wait_until("in conflict", || conflicts().last() == Some(&vec!["c.md".to_string()]));
    // Resolved on the server: the losing branch deleted.
    f.mock.put_branch(json!({"_id": "c.md", "_deleted": true}), &loser);
    wait_until("resolved", || conflicts().last() == Some(&vec![]));
    // And in conflict again.
    f.mock
        .put_branch_with(f.mock.text_doc("c.md", "again 1"), &kept, "0000cccc");
    f.mock
        .put_branch_with(f.mock.text_doc("c.md", "again 2"), &kept, "ffffdddd");
    wait_until("in conflict again", || {
        conflicts().len() >= 3 && conflicts().last() == Some(&vec!["c.md".to_string()])
    });

    // What a notifier makes of these events: a notice each time.
    let mut w = livesync_host::notify::Watch::new();
    let notices: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .flat_map(|e| w.on_event(e))
        .filter(|n| n.kind == "conflict")
        .collect();
    assert_eq!(notices.len(), 2, "{notices:?}");
    let _ = stop.send(true);
    f.rt.block_on(svc.shutdown());
}
