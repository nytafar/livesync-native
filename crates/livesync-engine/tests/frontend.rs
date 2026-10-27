//! Engine API used by front-ends (the Android app's FFI host): the one-pass
//! mass-delete allowance and cancelling a running daemon.

mod common;

use common::{Mock, DB};
use livesync_engine::{AccessCheck, Confirmation, Engine, EngineConfig, Index};
use livesync_model::path::path_to_id;
use livesync_remote::{Couch, RemoteConfig};
use serde_json::json;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    fn engine_at(&self, url: &str) -> Engine {
        self.engine_with(url, |_| {})
    }
    fn engine_with(&self, url: &str, f: impl FnOnce(&mut EngineConfig)) -> Engine {
        let couch = Couch::new(&RemoteConfig {
            url: url.into(),
            database: DB.into(),
            username: Some("u".into()),
            password: Some("p".into()),
        })
        .unwrap();
        let mut cfg = EngineConfig::new(self.vault(), self.dir.path().join("state/index.sqlite"));
        cfg.rebuild_quiet = Duration::ZERO;
        cfg.settle = Duration::from_millis(200);
        f(&mut cfg);
        Engine::open(couch, cfg).unwrap()
    }
    fn index(&self) -> Index {
        Index::open(&self.dir.path().join("state/index.sqlite")).unwrap()
    }
    fn remove(&self, range: std::ops::Range<usize>) {
        for i in range {
            std::fs::remove_file(self.vault().join(format!("f{i:02}.md"))).unwrap();
        }
    }
    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.vault().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    fn deleted(&self, path: &str) -> bool {
        self.mock
            .doc(&path_to_id(path, true))
            .map(|d| d.get("deleted") == Some(&json!(true)))
            .unwrap_or(false)
    }
}

/// The user confirms the stop the engine shows now.
fn confirm_shown(engine: &Engine) -> Confirmation {
    let stop = engine.status().unwrap().safety_stop.unwrap_or_default();
    engine.allow_mass_delete_once(&stop).unwrap()
}

#[tokio::test]
async fn a_one_time_mass_delete_allowance_covers_exactly_one_full_pass() {
    let env = Env::new().await;
    for i in 0..60 {
        env.write(&format!("f{i:02}.md"), format!("note {i}").as_bytes());
    }
    let engine = env.engine_at(&env.mock.url);
    engine.sync_once().await.unwrap();
    for i in 0..31 {
        std::fs::remove_file(env.vault().join(format!("f{i:02}.md"))).unwrap();
    }
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.safety_stops, 1);
    assert!(!env.deleted("f00.md"));

    assert_eq!(confirm_shown(&engine), Confirmation::Allowed);
    assert!(engine.mass_delete_allowed());
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 31, "{:?}", rep.push);
    assert_eq!(rep.push.safety_stops, 0);
    assert!(!engine.mass_delete_allowed(), "used up by that pass");
    assert!(engine.status().unwrap().safety_stop.is_none());

    // A later mass deletion stops again: the allowance does not linger.
    for i in 31..52 {
        std::fs::remove_file(env.vault().join(format!("f{i:02}.md"))).unwrap();
    }
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.safety_stops, 1, "{:?}", rep.push);
    assert_eq!(rep.push.deleted, 0);
    assert!(!env.deleted("f31.md"));
}

/// Review of #10: the confirmation covers the files the stop showed. Files
/// deleted after it are judged on their own, and stop again here.
#[tokio::test]
async fn a_confirmation_covers_only_what_the_stop_showed() {
    let env = Env::new().await;
    for i in 0..60 {
        env.write(&format!("f{i:02}.md"), format!("note {i}").as_bytes());
    }
    let engine = env.engine_at(&env.mock.url);
    engine.sync_once().await.unwrap();
    for i in 0..31 {
        std::fs::remove_file(env.vault().join(format!("f{i:02}.md"))).unwrap();
    }
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.safety_stops, 1);
    assert!(engine.status().unwrap().safety_stop.unwrap().contains("31 of 60"));
    // More go missing before the user answers.
    for i in 31..56 {
        std::fs::remove_file(env.vault().join(format!("f{i:02}.md"))).unwrap();
    }
    assert_eq!(confirm_shown(&engine), Confirmation::Allowed);
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 31, "{:?}", rep.push);
    assert_eq!(rep.push.safety_stops, 1, "{:?}", rep.push);
    assert!(env.deleted("f30.md"));
    assert!((31..56).all(|i| !env.deleted(&format!("f{i:02}.md"))));
    let st = engine.status().unwrap();
    assert!(st.safety_stop.as_deref().unwrap().contains("25 of 29"), "{st:?}");
    assert!(!engine.mass_delete_allowed());
}

/// Review of #10: a confirmation with no stop shown allows nothing.
#[tokio::test]
async fn a_confirmation_without_a_stop_allows_nothing() {
    let env = Env::new().await;
    for i in 0..40 {
        env.write(&format!("f{i:02}.md"), format!("note {i}").as_bytes());
    }
    let engine = env.engine_at(&env.mock.url);
    engine.sync_once().await.unwrap();
    assert_eq!(engine.allow_mass_delete_once("").unwrap(), Confirmation::NoStop);
    assert_eq!(confirm_shown(&engine), Confirmation::NoStop);
    assert!(!engine.mass_delete_allowed());
    for i in 0..30 {
        std::fs::remove_file(env.vault().join(format!("f{i:02}.md"))).unwrap();
    }
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.safety_stops, 1, "{:?}", rep.push);
    assert_eq!(rep.push.deleted, 0);
}

/// Review of #10, second round: a confirmation names the stop the user saw.
/// A stop checked again with the same files keeps its name; one that grew
/// while the dialog was open is not confirmed by an answer to the old one.
#[tokio::test]
async fn a_confirmation_of_a_stop_that_changed_is_refused() {
    let env = Env::new().await;
    for i in 0..60 {
        env.write(&format!("f{i:02}.md"), format!("note {i}").as_bytes());
    }
    let engine = env.engine_at(&env.mock.url);
    engine.sync_once().await.unwrap();
    env.remove(0..31);
    engine.sync_once().await.unwrap();
    let seen = engine.status().unwrap().safety_stop.unwrap();
    assert!(seen.contains("31 of 60"), "{seen}");
    // Checked again with nothing changed: the same stop.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.safety_stops, 1);
    assert_eq!(engine.status().unwrap().safety_stop.as_deref(), Some(seen.as_str()));
    // Eight more go missing and a pass records a new stop meanwhile.
    env.remove(31..39);
    engine.sync_once().await.unwrap();
    let now = engine.status().unwrap().safety_stop.unwrap();
    assert!(now.contains("39 of 60"), "{now}");
    assert_eq!(
        engine.allow_mass_delete_once(&seen).unwrap(),
        Confirmation::Changed(now.clone())
    );
    assert!(!engine.mass_delete_allowed());
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 0, "{:?}", rep.push);
    assert_eq!(rep.push.safety_stops, 1);
    // Confirming the stop shown now deletes its 39.
    assert_eq!(engine.allow_mass_delete_once(&now).unwrap(), Confirmation::Allowed);
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 39, "{:?}", rep.push);
    assert!(engine.status().unwrap().safety_stop.is_none());
}

/// Review of #10, second round: a stop saved by an older version (no list
/// of the files it refused) can be confirmed. It covers no more files than
/// it showed: if more are missing by the time it is applied, none goes and
/// the stop is shown anew.
#[tokio::test]
async fn a_stop_saved_by_an_older_version_can_be_confirmed() {
    let env = Env::new().await;
    for i in 0..60 {
        env.write(&format!("f{i:02}.md"), format!("note {i}").as_bytes());
    }
    let engine = env.engine_at(&env.mock.url);
    engine.sync_once().await.unwrap();
    env.remove(0..31);
    engine.sync_once().await.unwrap();
    // As v0.2.1 left it.
    env.index().delete_meta("safety_stop_docs").unwrap();
    let old = engine.status().unwrap().safety_stop.unwrap();
    assert_eq!(engine.allow_mass_delete_once(&old).unwrap(), Confirmation::Allowed);
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 31, "{:?}", rep.push);
    assert_eq!(rep.push.safety_stops, 0);
    assert!(engine.status().unwrap().safety_stop.is_none());

    // Again, with more missing than that stop showed when it is applied.
    env.remove(31..53);
    engine.sync_once().await.unwrap();
    env.index().delete_meta("safety_stop_docs").unwrap();
    let old = engine.status().unwrap().safety_stop.unwrap();
    assert!(old.contains("22 of 29"), "{old}");
    env.remove(53..56);
    assert_eq!(engine.allow_mass_delete_once(&old).unwrap(), Confirmation::Allowed);
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 0, "{:?}", rep.push);
    assert_eq!(rep.push.safety_stops, 1);
    let now = engine.status().unwrap().safety_stop.unwrap();
    assert!(now.contains("25 of 29"), "{now}");
    assert_eq!(engine.allow_mass_delete_once(&now).unwrap(), Confirmation::Allowed);
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 25, "{:?}", rep.push);
}

/// Review of #10, second round: a pass that fails before it gets to the
/// deletions does not use up the confirmation.
#[tokio::test]
async fn a_confirmation_survives_a_pass_that_fails_early() {
    let env = Env::new().await;
    for i in 0..60 {
        env.write(&format!("f{i:02}.md"), format!("note {i}").as_bytes());
    }
    let denied = Arc::new(AtomicBool::new(false));
    let engine = env.engine_with(&env.mock.url, |c| {
        let d = denied.clone();
        c.access_check = Some(AccessCheck::new(move || {
            if d.load(Ordering::SeqCst) {
                Err("storage permission revoked".into())
            } else {
                Ok(())
            }
        }));
    });
    engine.sync_once().await.unwrap();
    env.remove(0..31);
    engine.sync_once().await.unwrap();
    assert_eq!(confirm_shown(&engine), Confirmation::Allowed);
    denied.store(true, Ordering::SeqCst);
    assert!(engine.sync_once().await.is_err());
    assert!(engine.mass_delete_allowed(), "the failed pass used the confirmation up");
    denied.store(false, Ordering::SeqCst);
    let rep = engine.sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 31, "{:?}", rep.push);
    assert!(!engine.mass_delete_allowed());
    assert!(engine.status().unwrap().safety_stop.is_none());
}

/// A TCP proxy that can stop forwarding (requests and responses hang, as
/// on a dead link) and counts client bytes that arrive while stalled.
struct Stall {
    url: String,
    stalled: Arc<AtomicBool>,
    held_bytes: Arc<AtomicU64>,
}

impl Stall {
    fn start(target: &str) -> Stall {
        let target = target.trim_start_matches("http://").to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stalled = Arc::new(AtomicBool::new(false));
        let held_bytes = Arc::new(AtomicU64::new(0));
        let (s, h) = (stalled.clone(), held_bytes.clone());
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let Ok(server) = TcpStream::connect(&target) else {
                    continue;
                };
                let pipe =
                    |mut from: TcpStream, mut to: TcpStream, count: Option<Arc<AtomicU64>>, s: Arc<AtomicBool>| {
                        std::thread::spawn(move || {
                            let mut buf = [0u8; 16384];
                            while let Ok(n) = from.read(&mut buf) {
                                if n == 0 {
                                    break;
                                }
                                if s.load(Ordering::SeqCst) {
                                    if let Some(c) = &count {
                                        c.fetch_add(n as u64, Ordering::SeqCst);
                                    }
                                }
                                while s.load(Ordering::SeqCst) {
                                    std::thread::sleep(Duration::from_millis(20));
                                }
                                if to.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                            let _ = to.shutdown(std::net::Shutdown::Write);
                        });
                    };
                pipe(
                    client.try_clone().unwrap(),
                    server.try_clone().unwrap(),
                    Some(h.clone()),
                    s.clone(),
                );
                pipe(server, client, None, s.clone());
            }
        });
        Stall {
            url,
            stalled,
            held_bytes,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_run_cancels_a_push_in_flight() {
    let env = Env::new().await;
    let proxy = Stall::start(&env.mock.url);
    env.write("a.md", b"first");
    let engine = env.engine_at(&proxy.url);
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let e = engine.clone();
    let run = tokio::spawn(async move { e.run(rx).await });
    let t0 = Instant::now();
    while engine.status().unwrap().registered_ms.is_none() || engine.push_report().passes == 0 {
        assert!(t0.elapsed() < Duration::from_secs(20), "the daemon never got going");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The link dies, then a local edit starts a push that hangs on it.
    proxy.stalled.store(true, Ordering::SeqCst);
    env.write("b.md", b"second");
    let t0 = Instant::now();
    while proxy.held_bytes.load(Ordering::SeqCst) == 0 {
        assert!(t0.elapsed() < Duration::from_secs(10), "no push request was sent");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Cancel the run, as a front-end does when a stop takes too long.
    run.abort();
    let _ = run.await;
    drop(engine);
    // The push side went with it: the state lock is free at once, not after
    // the HTTP read timeout.
    let lock = env.dir.path().join("state/lsync.lock");
    let t0 = Instant::now();
    while livesync_engine::platform::LockFile::try_acquire(&lock).is_err() {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "the push task outlived the cancelled run"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    proxy.stalled.store(false, Ordering::SeqCst);
}
