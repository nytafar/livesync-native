//! The connection checks (spec D8) against the mock CouchDB: heartbeats
//! keep the longpoll open, a silent longpoll reconnects with fresh
//! connections, and a jump of the wall clock past the monotonic clock (the
//! machine slept) reconnects and rescans at once.

mod common;

use common::{Mock, DB};
use livesync_engine::{Engine, EngineConfig, EngineError, WallClock};
use livesync_model::path::path_to_id;
use livesync_remote::{Couch, RemoteConfig};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
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
        self.engine_and_couch(f).0
    }
    /// The engine, and a clone of its client (the same connection pool).
    fn engine_and_couch(&self, f: impl FnOnce(&mut EngineConfig)) -> (Engine, Couch) {
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
        (Engine::open(couch.clone(), cfg).unwrap(), couch)
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

#[tokio::test]
async fn heartbeats_keep_the_longpoll_and_silence_reconnects() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["a"]);
    let engine = env.engine_with(|c| c.regate_interval = Duration::from_secs(3600));
    let d = Daemon::two_way(&engine);
    until("the first pull", 10, || env.read("a.md").is_some()).await;
    until("pushes allowed", 10, || {
        engine.status().unwrap().registered_ms.is_some()
    })
    .await;
    // Heartbeats every 100 ms keep a 500 ms timeout from firing.
    let before = env.gates();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let heartbeats = env.mock.state.lock().unwrap().heartbeats_sent;
    assert!(heartbeats >= 5, "heartbeats: {heartbeats}");
    assert_eq!(env.gates(), before, "no reconnect while the heartbeats come");
    // The link dies: within the timeout (plus a heartbeat of backoff) the
    // daemon reconnects with fresh connections, and changes come through again.
    env.mock.state.lock().unwrap().stall_longpoll = true;
    until("a reconnect", 5, || env.gates() > before).await;
    env.mock.state.lock().unwrap().stall_longpoll = false;
    env.mock.put_text("b.md", &["b"]);
    until("a change after the reconnect", 10, || env.read("b.md").is_some()).await;
    assert!(env.mock.state.lock().unwrap().stalled_longpolls >= 1);
    d.stop().await.unwrap();
}

#[tokio::test]
async fn a_clock_jump_reconnects_and_rescans_at_once() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["a"]);
    let offset = Arc::new(AtomicI64::new(0));
    let o = offset.clone();
    let engine = env.engine_with(|c| {
        c.regate_interval = Duration::from_secs(3600);
        // Without the jump, no heartbeat timeout or rescan would fire.
        c.heartbeat = Duration::from_secs(5);
        c.longpoll_timeout = Duration::from_secs(60);
        c.clock_jump = Some(Duration::from_millis(400));
        c.wall_clock = Some(WallClock::new(move || {
            livesync_engine::vaultfs::now_ms() + o.load(Ordering::SeqCst)
        }));
    });
    let d = Daemon::two_way(&engine);
    until("the first pull", 10, || env.read("a.md").is_some()).await;
    until("pushes allowed", 10, || {
        engine.status().unwrap().registered_ms.is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = env.gates();
    // A file saved during "sleep" (no watcher here, and the rescan is an hour off).
    env.write("written-in-sleep.md", b"zzz");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!env.pushed("written-in-sleep.md"));
    assert_eq!(env.gates(), before);
    // The machine "wakes": the wall clock is an hour ahead of the monotonic one.
    offset.store(3_600_000, Ordering::SeqCst);
    until("the reconnect", 5, || env.gates() > before).await;
    until("the rescan pushed the file", 5, || env.pushed("written-in-sleep.md")).await;
    d.stop().await.unwrap();
}

#[tokio::test]
async fn a_network_change_reconnects_on_fresh_connections() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["a"]);
    let (engine, couch) = env.engine_and_couch(|c| c.regate_interval = Duration::from_secs(3600));
    let d = Daemon::two_way(&engine);
    until("the first pull", 10, || env.read("a.md").is_some()).await;
    until("pushes allowed", 10, || {
        engine.status().unwrap().registered_ms.is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // An idle connection in the pool, beside the longpoll's: after a Wi-Fi
    // switch it would lead nowhere, and hang until the read timeout.
    couch.server_root().await.unwrap();
    let old = env.mock.peers();
    let seen = env.mock.state.lock().unwrap().conns.len();
    let before = env.gates();
    engine.network_changed(true);
    until("the gate after the nudge", 5, || env.gates() > before).await;
    let conns = env.mock.state.lock().unwrap().conns.clone();
    let (_, _, peer) = conns[seen..]
        .iter()
        .find(|(m, p, _)| m == "GET" && p == "/")
        .expect("the gate asked the server root");
    assert!(
        !old.contains(peer),
        "the gate after a network change went out on a pooled connection ({peer})"
    );
    d.stop().await.unwrap();
}
