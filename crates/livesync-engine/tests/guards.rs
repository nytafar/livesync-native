//! The upstream guard and the plugin guard (spec D16) against the mock
//! CouchDB: an upstream change stops syncing with a message that names it
//! and resumes once the remote is compatible again; every reconnect re-runs
//! the gate; an Obsidian vault with the plugin enabled is refused two-way
//! unless allowed.

mod common;

use common::{Mock, DB};
use livesync_engine::{Engine, EngineConfig, EngineError};
use livesync_model::gate::TESTED_PLUGIN_VERSION;
use livesync_model::path::path_to_id;
use livesync_model::Refusal;
use livesync_remote::{Couch, RemoteConfig};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinHandle;

struct Env {
    mock: Mock,
    dir: tempfile::TempDir,
}

impl Env {
    async fn new() -> Env {
        Env {
            mock: Mock::start().await,
            dir: tempfile::tempdir().unwrap(),
        }
    }
    fn vault(&self) -> PathBuf {
        self.dir.path().join("vault")
    }
    fn engine_with(&self, f: impl FnOnce(&mut EngineConfig)) -> Engine {
        let couch = Couch::new(&RemoteConfig {
            url: self.mock.url.clone(),
            database: DB.into(),
            username: Some("u".into()),
            password: Some("p".into()),
        })
        .unwrap();
        let mut cfg = EngineConfig::new(self.vault(), self.dir.path().join("state/index.sqlite"));
        cfg.rebuild_quiet = Duration::ZERO;
        cfg.settle = Duration::from_millis(100);
        cfg.watch = false;
        cfg.rescan_interval = Duration::from_secs(3600);
        cfg.heartbeat = Duration::from_millis(100);
        cfg.longpoll_timeout = Duration::from_millis(500);
        cfg.clock_jump = None;
        f(&mut cfg);
        Engine::open(couch, cfg).unwrap()
    }
    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.vault().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.vault().join(rel)).ok()
    }
    fn pushed(&self, path: &str) -> bool {
        self.mock.content(&path_to_id(path, true)).is_some()
    }
    /// How often the gate read the milestone (once per gate run).
    fn gates(&self) -> usize {
        self.mock
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(m, p)| m == "GET" && p.ends_with("/_local/obsydian_livesync_milestone"))
            .count()
    }
    fn enable_plugin(&self) {
        let o = self.vault().join(".obsidian");
        std::fs::create_dir_all(o.join("plugins/obsidian-livesync")).unwrap();
        std::fs::write(o.join("community-plugins.json"), r#"["dataview","obsidian-livesync"]"#).unwrap();
    }
}

struct Daemon {
    stop: watch::Sender<bool>,
    task: JoinHandle<Result<(), EngineError>>,
}

impl Daemon {
    fn two_way(engine: &Engine) -> Daemon {
        let (stop, rx) = watch::channel(false);
        let e = engine.clone();
        Daemon {
            stop,
            task: tokio::spawn(async move { e.run(rx).await }),
        }
    }
    async fn stop(self) -> Result<(), EngineError> {
        let _ = self.stop.send(true);
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("the daemon stops")
            .expect("the daemon does not panic")
    }
}

async fn until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

fn upstream_stop(engine: &Engine) -> Option<String> {
    engine.status().unwrap().upstream_stop
}

/// The remote changes one way or another, the daemon stops with a message
/// naming exactly that, holds pulls and pushes, and resumes by itself once
/// the remote is as before.
async fn stops_and_resumes(change: impl Fn(&Mock), undo: impl Fn(&Mock), named: &str) {
    let env = Env::new().await;
    env.mock.put_text("first.md", &["one"]);
    let engine = env.engine_with(|c| c.regate_interval = Duration::from_millis(300));
    let d = Daemon::two_way(&engine);
    until("the first pull", 10, || env.read("first.md").as_deref() == Some(b"one")).await;
    until("pushes allowed", 10, || {
        engine.status().unwrap().registered_ms.is_some()
    })
    .await;

    change(&env.mock);
    until("the upstream stop", 10, || upstream_stop(&engine).is_some()).await;
    let msg = upstream_stop(&engine).unwrap();
    assert!(msg.contains(named), "{named:?} not in {msg}");
    assert!(
        msg.contains(&format!(
            "Self-hosted LiveSync {TESTED_PLUGIN_VERSION} is the newest version known to work"
        )),
        "{msg}"
    );
    let st = engine.status().unwrap();
    assert!(st.last_error.as_deref().is_some_and(|e| e.contains(named)), "{st:?}");

    // Stopped: nothing comes down, nothing goes up.
    env.mock.put_text("while-stopped.md", &["remote"]);
    env.write("local-while-stopped.md", b"local");
    engine.rescan_now();
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(env.read("while-stopped.md").is_none(), "pulled while stopped");
    assert!(!env.pushed("local-while-stopped.md"), "pushed while stopped");
    assert!(upstream_stop(&engine).is_some(), "still stopped at the next regate");

    // Compatible again: it resumes on its own and catches up both ways.
    undo(&env.mock);
    until("the stop to clear", 10, || upstream_stop(&engine).is_none()).await;
    until("the remote file", 10, || {
        env.read("while-stopped.md").as_deref() == Some(b"remote")
    })
    .await;
    engine.rescan_now();
    until("the local file pushed", 10, || env.pushed("local-while-stopped.md")).await;
    d.stop().await.unwrap();
}

fn version_doc(m: &Mock, doc: Value) {
    let mut d = doc;
    d["_id"] = json!("obsydian_livesync_version");
    d["type"] = json!("versioninfo");
    m.put(d);
}

#[tokio::test]
async fn an_unknown_feature_stops_with_its_name() {
    stops_and_resumes(
        |m| version_doc(m, json!({"version": 13, "used_features": ["teleport-v1"]})),
        |m| version_doc(m, json!({"version": 12})),
        "features this client does not know: teleport-v1",
    )
    .await;
}

#[tokio::test]
async fn a_raised_version_stops_with_the_generation() {
    stops_and_resumes(
        |m| version_doc(m, json!({"version": 14})),
        |m| version_doc(m, json!({"version": 12})),
        "is at generation 14",
    )
    .await;
}

#[tokio::test]
async fn a_changed_tweak_stops_with_the_tweak_and_its_values() {
    stops_and_resumes(
        |m| m.update_milestone(|ms| ms["tweak_values"]["PREFERRED"]["customChunkSize"] = json!(100)),
        |m| m.update_milestone(|ms| ms["tweak_values"]["PREFERRED"]["customChunkSize"] = json!(60)),
        "customChunkSize: 60 -> 100",
    )
    .await;
}

#[tokio::test]
async fn a_new_tweak_stops_with_its_name_and_value() {
    stops_and_resumes(
        |m| m.update_milestone(|ms| ms["tweak_values"]["PREFERRED"]["useWarpChunks"] = json!(true)),
        |m| {
            m.update_milestone(|ms| {
                ms["tweak_values"]["PREFERRED"]
                    .as_object_mut()
                    .unwrap()
                    .remove("useWarpChunks");
            })
        },
        "settings this client does not know: useWarpChunks=true",
    )
    .await;
}

#[tokio::test]
async fn the_stop_names_the_newest_plugin_on_the_remote_and_adopting_lifts_a_tweak_stop() {
    let env = Env::new().await;
    env.mock.update_milestone(|m| {
        m["node_info"] =
            json!({"desktop1": {"device_name": "laptop", "app_version": "1.12.7", "plugin_version": "1.0.99"}});
        m["tweak_values"]["PREFERRED"]["useWarpChunks"] = json!(true);
    });
    let engine = env.engine_with(|_| {});
    // Also at first contact: a setting a newer plugin added is not adopted unasked.
    let r = engine.connect_gate(false).await;
    assert!(
        matches!(r, Err(EngineError::Refused(Refusal::UnknownTweaks(_)))),
        "{r:?}"
    );
    let msg = upstream_stop(&engine).unwrap();
    assert!(msg.contains("Self-hosted LiveSync 1.0.99 on \"laptop\""), "{msg}");
    // A person checked it: `connect --adopt-tweaks` takes it, and the stop goes.
    engine.connect_gate(true).await.unwrap();
    assert!(upstream_stop(&engine).is_none());
    engine.connect_gate(false).await.unwrap();
    // A pull-only pass is stopped the same way, and stays read-only.
    version_doc(&env.mock, json!({"version": 13, "used_features": ["teleport-v1"]}));
    assert!(matches!(
        engine.pull_once().await,
        Err(EngineError::Refused(Refusal::UnknownFeatures(_)))
    ));
    assert!(upstream_stop(&engine).unwrap().contains("teleport-v1"));
    assert!(env.mock.writes().is_empty(), "{:?}", env.mock.writes());
}

#[tokio::test]
async fn a_reconnect_runs_the_gate_again() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["a"]);
    // The regate interval is far off: only the reconnect can run the gate.
    let engine = env.engine_with(|c| c.regate_interval = Duration::from_secs(3600));
    let d = Daemon::two_way(&engine);
    until("the first pull", 10, || env.read("a.md").is_some()).await;
    until("pushes allowed", 10, || {
        engine.status().unwrap().registered_ms.is_some()
    })
    .await;
    let before = env.gates();
    // A tweak changes, then the connection drops (the longpoll goes silent).
    env.mock
        .update_milestone(|ms| ms["tweak_values"]["PREFERRED"]["customChunkSize"] = json!(100));
    env.mock.state.lock().unwrap().stall_longpoll = true;
    until("the upstream stop after the reconnect", 10, || {
        upstream_stop(&engine).is_some()
    })
    .await;
    assert!(env.gates() > before, "the reconnect ran the gate");
    assert!(env.mock.state.lock().unwrap().stalled_longpolls >= 1);
    assert!(upstream_stop(&engine).unwrap().contains("customChunkSize: 60 -> 100"));
    // A network change (a nudge) is a reconnect too.
    env.mock.state.lock().unwrap().stall_longpoll = false;
    env.mock
        .update_milestone(|ms| ms["tweak_values"]["PREFERRED"]["customChunkSize"] = json!(60));
    engine.network_changed(true);
    until("the stop to clear after the nudge", 10, || {
        upstream_stop(&engine).is_none()
    })
    .await;
    d.stop().await.unwrap();
}

#[tokio::test]
async fn an_obsidian_vault_with_the_plugin_enabled_is_refused_two_way() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["a"]);
    env.enable_plugin();
    env.write("mine.md", b"local");
    let engine = env.engine_with(|_| {});
    let refused = |r: Result<(), EngineError>| match r {
        Err(EngineError::Refused(Refusal::PluginEnabled { why, .. })) => why,
        other => panic!("expected the plugin guard, got {other:?}"),
    };
    // One-shot sync (and so fetch) and the daemon, before anything is read or written.
    let why = refused(engine.sync_once().await.map(|_| ()));
    assert!(why.contains("community-plugins.json lists obsidian-livesync"), "{why}");
    let (_tx, rx) = watch::channel(false);
    refused(engine.run(rx).await);
    assert!(env.mock.writes().is_empty(), "{:?}", env.mock.writes());
    assert!(env.read("a.md").is_none());
    let msg = engine.status().unwrap().last_error.unwrap();
    assert!(
        msg.contains("--allow-plugin") && msg.contains("Self-hosted LiveSync plugin enabled"),
        "{msg}"
    );
    // Read-only pulls are not two-way, and are not refused.
    engine.pull_once().await.unwrap();
    assert_eq!(env.read("a.md").as_deref(), Some(&b"a"[..]));
    drop(engine);
    // A deliberate handover.
    let engine = env.engine_with(|c| c.allow_plugin = true);
    engine.sync_once().await.unwrap();
    assert!(env.pushed("mine.md"));
}

#[tokio::test]
async fn plain_folders_and_disabled_plugins_are_not_refused() {
    let env = Env::new().await;
    env.write("mine.md", b"local");
    // An Obsidian vault with the plugin installed but not enabled.
    let o = env.vault().join(".obsidian");
    std::fs::create_dir_all(o.join("plugins/obsidian-livesync")).unwrap();
    std::fs::write(o.join("community-plugins.json"), r#"["dataview"]"#).unwrap();
    env.engine_with(|_| {}).sync_once().await.unwrap();
    assert!(env.pushed("mine.md"));
}

#[tokio::test]
async fn the_daemon_stops_when_the_plugin_is_turned_on_while_it_runs() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["a"]);
    let engine = env.engine_with(|c| c.regate_interval = Duration::from_millis(300));
    let (_tx, rx) = watch::channel(false);
    let e = engine.clone();
    let task = tokio::spawn(async move { e.run(rx).await });
    until("the first pull", 10, || env.read("a.md").is_some()).await;
    env.enable_plugin();
    let r = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(r, Err(EngineError::Refused(Refusal::PluginEnabled { .. }))),
        "{r:?}"
    );
}

/// The stops and the last error, as watchers read them.
fn alerts(engine: &Engine) -> livesync_engine::Alerts {
    engine.alerts().unwrap()
}

#[tokio::test]
async fn a_repeated_refusal_leaves_the_recorded_stop_as_it_was() {
    let env = Env::new().await;
    env.mock.put_text("x.md", &["x"]);
    let engine = env.engine_with(|_| {});
    engine.sync_once().await.unwrap();

    // Locked without a rebuild. The holder keeps connecting (its
    // `last_connected` moves), and time passes: one state, recorded once.
    let lock = |node: &str, device: &str, at: i64| {
        let (node, device) = (node.to_string(), device.to_string());
        env.mock.update_milestone(move |m| {
            m["locked"] = json!(true);
            m["accepted_nodes"] = json!([node.clone()]);
            m["node_info"][node.as_str()] = json!({"device_name": device, "last_connected": at});
        })
    };
    lock("someone", "laptop", 1_762_506_720_000);
    assert!(engine.connect_gate(false).await.is_err());
    let first = alerts(&engine);
    let held = first.remote_locked.clone().unwrap();
    assert!(
        held.contains("\"laptop\" (node someone), last connected 2025-11-07 09:12 UTC"),
        "{held}"
    );
    assert_eq!(first.last_error, first.remote_locked, "the error mirrors the stop");
    for at in [1_762_506_780_000, 1_762_510_000_000] {
        tokio::time::sleep(Duration::from_millis(5)).await;
        lock("someone", "laptop", at);
        assert!(engine.connect_gate(false).await.is_err());
        assert_eq!(alerts(&engine), first, "a moved timestamp is no new state");
    }
    // Another device holds the lock: a real change.
    lock("other", "phone", 1_762_510_000_000);
    assert!(engine.connect_gate(false).await.is_err());
    let changed = alerts(&engine).remote_locked.unwrap();
    assert!(changed.contains("phone") && !changed.contains("laptop"), "{changed}");

    // The upstream stop: the same, and its time is when it began.
    env.mock.update_milestone(|m| {
        m["locked"] = json!(false);
        m["tweak_values"]["PREFERRED"]["customChunkSize"] = json!(100);
    });
    assert!(engine.connect_gate(false).await.is_err());
    let first = alerts(&engine);
    assert!(first.remote_locked.is_none(), "{first:?}");
    assert!(first.upstream_stop.is_some());
    for _ in 0..2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(engine.connect_gate(false).await.is_err());
        assert_eq!(alerts(&engine), first);
    }
}

#[tokio::test]
async fn encrypted_content_stops_until_a_pull_gets_past_it_without_flapping() {
    let env = Env::new().await;
    env.mock.put_text("first.md", &["one"]);
    let engine = env.engine_with(|c| c.regate_interval = Duration::from_millis(200));
    let d = Daemon::two_way(&engine);
    until("the first pull", 10, || env.read("first.md").is_some()).await;
    env.mock
        .put(json!({"_id": "h:abc", "type": "leaf", "data": "%=AAAA", "e_": true}));
    env.mock.put(json!({"_id": "enc.md", "path": "enc.md", "type": "plain", "children": ["h:abc"], "mtime": 1, "ctime": 1, "size": 3, "eden": {}}));
    until("the upstream stop", 10, || upstream_stop(&engine).is_some()).await;
    let first = alerts(&engine);
    assert!(
        first.upstream_stop.as_deref().unwrap().contains("encrypted"),
        "{first:?}"
    );
    // The gate passes at every regate (it has nothing against the remote),
    // and the pull stops at the same place: the stop holds, unchanged.
    let gates = env.gates();
    let end = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < end {
        assert_eq!(alerts(&engine), first, "the stop came and went");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(env.gates() >= gates + 2, "regated meanwhile");
    // The document is written again in the clear: the pull gets past it.
    env.mock.put_text("enc.md", &["clear"]);
    until("the stop lifted", 10, || upstream_stop(&engine).is_none()).await;
    until("the file", 10, || env.read("enc.md").as_deref() == Some(b"clear")).await;
    assert!(alerts(&engine).last_error.is_none());
    d.stop().await.unwrap();
}
