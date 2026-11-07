//! `flush` and `wait` on running daemons against the mock CouchDB: a flush
//! stores before the settle time and says which revision, another node
//! waits for exactly that revision, and both end on time or at a stop.

mod common;

use common::{Mock, DB};
use livesync_engine::{
    Engine, EngineConfig, EngineEvent, FlushOutcome, Index, PullPhase, Target, WaitFor, WaitOutcome,
};
use livesync_model::path::path_to_id;
use livesync_remote::{Couch, RemoteConfig};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::watch;

struct Node {
    dir: tempfile::TempDir,
    engine: Engine,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<(), livesync_engine::EngineError>>,
}

fn config(mock: &Mock, dir: &std::path::Path, f: impl FnOnce(&mut EngineConfig)) -> (Couch, EngineConfig) {
    let couch = Couch::new(&RemoteConfig {
        url: mock.url.clone(),
        database: DB.into(),
        username: Some("u".into()),
        password: Some("p".into()),
    })
    .unwrap();
    let mut cfg = EngineConfig::new(dir.join("vault"), dir.join("state/index.sqlite"));
    cfg.rebuild_quiet = Duration::ZERO;
    cfg.delete_hold = Duration::from_millis(300);
    cfg.rescan_interval = Duration::from_secs(3600);
    f(&mut cfg);
    (couch, cfg)
}

impl Node {
    /// A two-way daemon (pull-only with `pull_only`) on its own folders.
    async fn start(mock: &Mock, pull_only: bool, f: impl FnOnce(&mut EngineConfig)) -> Node {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vault")).unwrap();
        let (couch, cfg) = config(mock, dir.path(), f);
        let engine = Engine::open(couch, cfg).unwrap();
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn({
            let e = engine.clone();
            async move {
                if pull_only {
                    e.daemon_pull_only(rx).await
                } else {
                    e.run(rx).await
                }
            }
        });
        let node = Node {
            dir,
            engine,
            stop,
            task,
        };
        node.until("the daemon is live", |n| {
            let a = n.engine.activity();
            a.pull == PullPhase::Live && (pull_only || a.pushes)
        })
        .await;
        node
    }

    fn vault(&self) -> PathBuf {
        self.dir.path().join("vault")
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.vault().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn index(&self) -> Index {
        Index::open(&self.dir.path().join("state/index.sqlite")).unwrap()
    }

    async fn until(&self, what: &str, f: impl Fn(&Node) -> bool) {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            if f(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(15)
}

fn rev_of(mock: &Mock, path: &str) -> String {
    mock.doc(&path_to_id(path, true)).unwrap()["_rev"]
        .as_str()
        .unwrap()
        .to_string()
}

fn done(o: FlushOutcome) -> livesync_engine::FlushReport {
    match o {
        FlushOutcome::Done(r) => r,
        other => panic!("not done: {other:?}"),
    }
}

#[tokio::test]
async fn a_flushed_file_is_stored_before_its_settle_time_and_arrives_at_exactly_that_revision() {
    let mock = Mock::start().await;
    // A settle time far beyond the test: only the flush pushes the file.
    let a = Node::start(&mock, false, |c| c.settle = Duration::from_secs(60)).await;
    let b = Node::start(&mock, false, |c| c.settle = Duration::from_secs(60)).await;
    let mut stored = a.engine.subscribe();
    let mut applied = b.engine.subscribe();

    a.write("handoff.md", b"from the agent");
    let t0 = Instant::now();
    let rep = done(a.engine.flush(Some(vec!["handoff.md".into()])).await.unwrap());
    assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
    assert!(rep.complete, "{rep:?}");
    assert_eq!(rep.paths.len(), 1);
    let p = &rep.paths[0];
    assert_eq!(
        (p.path.as_str(), p.outcome.as_str(), p.pushed),
        ("handoff.md", "stored", true)
    );
    let rev = p.rev.clone().unwrap();
    assert_eq!(rev, rev_of(&mock, "handoff.md"), "the revision on the server");
    // The engine said so too, precisely.
    let ev = stored.recv().await.unwrap();
    assert_eq!(
        ev,
        EngineEvent::Stored(livesync_engine::RevisionEvent {
            path: "handoff.md".into(),
            doc_id: "handoff.md".into(),
            rev: rev.clone(),
            deleted: false
        })
    );

    // The other node waits for that revision.
    let cond = WaitFor::Applied {
        path: "handoff.md".into(),
        target: Target::Rev(rev.clone()),
    };
    match b.engine.wait_for(&cond, soon()).await.unwrap() {
        WaitOutcome::Reached(r) => {
            assert_eq!(r.current_rev.as_deref(), Some(rev.as_str()));
            assert!(!r.superseded);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read(b.vault().join("handoff.md")).unwrap(), b"from the agent");
    let ev = loop {
        match applied.recv().await.unwrap() {
            EngineEvent::Applied(e) if e.path == "handoff.md" => break e,
            _ => {}
        }
    };
    assert_eq!(ev.rev, rev);

    // Flushing it again stores nothing new: it is on the server already.
    let again = done(a.engine.flush(Some(vec!["handoff.md".into()])).await.unwrap());
    assert_eq!(again.paths[0].rev.as_deref(), Some(rev.as_str()));
    assert!(!again.paths[0].pushed);

    // An older revision is passed already; the server's current one is the target without a revision.
    a.write("handoff.md", b"edited again");
    let rep = done(a.engine.flush(Some(vec!["handoff.md".into()])).await.unwrap());
    let newer = rep.paths[0].rev.clone().unwrap();
    assert_ne!(newer, rev);
    let target = b.engine.target_of("handoff.md", None).await.unwrap();
    assert_eq!(target, Target::Rev(newer.clone()));
    let cond = WaitFor::Applied {
        path: "handoff.md".into(),
        target,
    };
    assert!(matches!(
        b.engine.wait_for(&cond, soon()).await.unwrap(),
        WaitOutcome::Reached(r) if r.current_rev.as_deref() == Some(newer.as_str())
    ));
    let old = WaitFor::Applied {
        path: "handoff.md".into(),
        target: Target::Rev(rev.clone()),
    };
    assert!(matches!(
        b.engine.wait_for(&old, soon()).await.unwrap(),
        WaitOutcome::Reached(r) if r.superseded
    ));
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
async fn a_flush_of_everything_stores_edits_and_deletions_and_lists_their_revisions() {
    let mock = Mock::start().await;
    mock.put_text("gone.md", &["to be deleted"]);
    let a = Node::start(&mock, false, |c| c.settle = Duration::from_secs(60)).await;
    a.until("pulled", |n| n.vault().join("gone.md").exists()).await;
    a.write("one.md", b"one");
    a.write("dir/two.md", b"two");
    std::fs::remove_file(a.vault().join("gone.md")).unwrap();

    let rep = done(a.engine.flush(None).await.unwrap());
    assert!(rep.all && rep.complete, "{rep:?}");
    let by: std::collections::BTreeMap<&str, &livesync_engine::FlushedPath> =
        rep.paths.iter().map(|p| (p.path.as_str(), p)).collect();
    assert_eq!(
        by.keys().copied().collect::<Vec<_>>(),
        ["dir/two.md", "gone.md", "one.md"]
    );
    assert_eq!(by["one.md"].rev.clone().unwrap(), rev_of(&mock, "one.md"));
    assert_eq!(by["gone.md"].outcome, "deleted");
    assert_eq!(by["gone.md"].rev.clone().unwrap(), rev_of(&mock, "gone.md"));
    assert_eq!(mock.doc("gone.md").unwrap()["deleted"], true);
    assert!(rep.issues.is_empty() && rep.safety_stop.is_none());

    // A folder stands for its files; a deletion given by path waits for the hold.
    std::fs::remove_file(a.vault().join("one.md")).unwrap();
    a.write("dir/three.md", b"three");
    let rep = done(a.engine.flush(Some(vec!["dir".into(), "one.md".into()])).await.unwrap());
    let got: Vec<(&str, &str)> = rep
        .paths
        .iter()
        .map(|p| (p.path.as_str(), p.outcome.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("dir/three.md", "stored"),
            ("dir/two.md", "stored"),
            ("one.md", "deleted")
        ]
    );
    assert!(rep.complete);
    assert_eq!(mock.doc("one.md").unwrap()["deleted"], true);
    a.stop().await;
}

#[tokio::test]
async fn a_flush_says_what_it_could_not_store_and_a_gate_stop_answers_at_once() {
    let mock = Mock::start().await;
    let a = Node::start(&mock, false, |c| {
        c.settle = Duration::from_secs(60);
        c.ignore_regex = Some("^private/".into());
    })
    .await;
    a.write("private/x.md", b"x");
    a.write("ok.md", b"ok");
    let rep = done(
        a.engine
            .flush(Some(vec!["private/x.md".into(), "ok.md".into(), "nope.md".into()]))
            .await
            .unwrap(),
    );
    let got: Vec<(&str, &str)> = rep
        .paths
        .iter()
        .map(|p| (p.path.as_str(), p.outcome.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("nope.md", "missing"),
            ("ok.md", "stored"),
            ("private/x.md", "not-synced")
        ]
    );
    assert!(!rep.complete);
    assert!(mock.doc("private/x.md").is_none());
    // Stored with a caveat: stored, and the caveat as its note.
    a.write("latin1.md", b"caf\xe9");
    let rep = done(a.engine.flush(Some(vec!["latin1.md".into()])).await.unwrap());
    let p = &rep.paths[0];
    assert_eq!(p.outcome, "stored", "{p:?}");
    assert!(p.note.as_deref().is_some_and(|n| n.contains("UTF-8")), "{p:?}");
    assert!(rep.complete);

    // The upstream guard holds every push: the flush does not wait for it.
    a.index()
        .set_meta("upstream_stop", "1 the remote moved to a newer format")
        .unwrap();
    a.write("later.md", b"later");
    let t0 = Instant::now();
    match a.engine.flush(None).await.unwrap() {
        FlushOutcome::Blocked(why) => assert!(why.contains("newer format"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(t0.elapsed() < Duration::from_secs(2));
    a.stop().await;
}

#[tokio::test]
async fn a_pull_only_node_refuses_to_flush() {
    let mock = Mock::start().await;
    let b = Node::start(&mock, true, |_| {}).await;
    let e = b.engine.flush(None).await.unwrap_err();
    assert!(e.to_string().contains("pull-only"), "{e}");
    b.stop().await;
}

#[tokio::test]
async fn a_wait_times_out_on_time_and_ends_at_a_local_edit_or_when_the_daemon_stops() {
    let mock = Mock::start().await;
    mock.put_text("n.md", &["first"]);
    let b = Node::start(&mock, false, |c| c.settle = Duration::from_secs(60)).await;
    b.until("pulled", |n| n.vault().join("n.md").exists()).await;

    // A revision that never comes: the deadline ends it, saying what is missing.
    let never = WaitFor::Applied {
        path: "n.md".into(),
        target: Target::Rev("1-0000".into()),
    };
    let t0 = Instant::now();
    match b
        .engine
        .wait_for(&never, Instant::now() + Duration::from_millis(400))
        .await
        .unwrap()
    {
        WaitOutcome::TimedOut(why) => assert!(why.contains("1-0000"), "{why}"),
        other => panic!("{other:?}"),
    }
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_millis(400) && took < Duration::from_secs(2),
        "{took:?}"
    );

    // A local edit holds the server's next revision back: no use waiting.
    b.write("n.md", b"edited here, not pushed yet");
    let rev = mock.put_text("n.md", &["second"]);
    let cond = WaitFor::Applied {
        path: "n.md".into(),
        target: Target::Rev(rev.clone()),
    };
    match b.engine.wait_for(&cond, soon()).await.unwrap() {
        WaitOutcome::Blocked(why) => assert!(why.contains("local edit"), "{why}"),
        other => panic!("{other:?}"),
    }

    // The daemon stops while a wait runs.
    let engine = b.engine.clone();
    let waiting = tokio::spawn(async move { engine.wait_for(&never, soon()).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    b.stop().await;
    assert_eq!(waiting.await.unwrap().unwrap(), WaitOutcome::Stopped);
}

#[tokio::test]
async fn idle_means_nothing_settling_or_pending_and_deleted_files_arrive_as_deletions() {
    let mock = Mock::start().await;
    mock.put_text("d.md", &["doomed"]);
    let a = Node::start(&mock, false, |c| c.settle = Duration::from_millis(1500)).await;
    a.until("pulled", |n| n.vault().join("d.md").exists()).await;
    assert!(matches!(
        a.engine.wait_for(&WaitFor::Idle, soon()).await.unwrap(),
        WaitOutcome::Reached(_)
    ));

    // A save settles for 1.5 s: not idle meanwhile, and a short wait says why.
    a.write("busy.md", b"being written");
    a.until("seen", |n| n.engine.activity().settling > 0).await;
    let idle = a.engine.idle().unwrap();
    assert!(
        !idle.idle && idle.busy.iter().any(|b| b.contains("settling")),
        "{idle:?}"
    );
    match a
        .engine
        .wait_for(&WaitFor::Idle, Instant::now() + Duration::from_millis(200))
        .await
        .unwrap()
    {
        WaitOutcome::TimedOut(why) => assert!(why.contains("settling"), "{why}"),
        other => panic!("{other:?}"),
    }
    // Then it is pushed, and the node is idle.
    assert!(matches!(
        a.engine.wait_for(&WaitFor::Idle, soon()).await.unwrap(),
        WaitOutcome::Reached(_)
    ));
    assert!(mock.doc("busy.md").is_some());

    // The server deletes a file: waiting for the server's current state of
    // it waits for the deletion.
    mock.soft_delete("d.md");
    let target = a.engine.target_of("d.md", None).await.unwrap();
    let cond = WaitFor::Applied {
        path: "d.md".into(),
        target,
    };
    match a.engine.wait_for(&cond, soon()).await.unwrap() {
        WaitOutcome::Reached(r) => assert!(r.deleted, "{r:?}"),
        other => panic!("{other:?}"),
    }
    assert!(!a.vault().join("d.md").exists());

    // A safety stop is something a person must look at.
    a.index().set_meta("safety_stop", "1 30 files went missing").unwrap();
    match a.engine.wait_for(&WaitFor::Idle, soon()).await.unwrap() {
        WaitOutcome::Blocked(why) => assert!(why.contains("30 files"), "{why}"),
        other => panic!("{other:?}"),
    }
    a.stop().await;
}

fn applied(path: &str, rev: &str) -> WaitFor {
    WaitFor::Applied {
        path: path.into(),
        target: Target::Rev(rev.into()),
    }
}

#[tokio::test]
async fn a_revision_on_a_losing_branch_is_never_reported_as_arrived() {
    let mock = Mock::start().await;
    // 1-a, then two edits of it: the loser (a low hash) is deleted, as a
    // conflict's resolution does, and the winner edited once more. This
    // node never sees the loser: the changes feed gives the winner only.
    let first = mock.put_text("h.md", &["one"]);
    let loser = mock.put_branch_with(mock.text_doc("h.md", "lost"), &first, "0000aaaa");
    let kept = mock.put_branch_with(mock.text_doc("h.md", "kept"), &first, "ffffbbbb");
    mock.put_branch(serde_json::json!({"_id": "h.md", "_deleted": true}), &loser);
    let last = mock.put(mock.text_doc("h.md", "kept, edited"));
    assert_eq!(rev_of(&mock, "h.md"), last);
    let b = Node::start(&mock, true, |_| {}).await;
    b.until("pulled", |n| n.vault().join("h.md").exists()).await;
    assert!(b.index().revision("h.md", &loser).unwrap().is_none(), "never seen here");

    // The loser is a later generation than nothing here but an earlier one
    // than the local file: only the server's history tells it apart.
    match b.engine.wait_for(&applied("h.md", &loser), soon()).await.unwrap() {
        WaitOutcome::Blocked(why) => assert!(why.contains(&loser) && why.contains("history"), "{why}"),
        other => panic!("a losing revision must not count as arrived: {other:?}"),
    }
    // Ancestors of the local revision, never seen here either: passed.
    for old in [&kept, &first] {
        match b.engine.wait_for(&applied("h.md", old), soon()).await.unwrap() {
            WaitOutcome::Reached(r) => {
                assert!(r.superseded, "{r:?}");
                assert_eq!(r.current_rev.as_deref(), Some(last.as_str()));
            }
            other => panic!("{old}: {other:?}"),
        }
    }
    // A history cut at `_revs_limit` (here: the last two) does not reach
    // back to 1-…: blocked, and said so, not "on another branch".
    mock.state.lock().unwrap().revs_limit = Some(2);
    match b.engine.wait_for(&applied("h.md", &first), soon()).await.unwrap() {
        WaitOutcome::Blocked(why) => {
            assert!(why.contains("does not reach that far back"), "{why}");
            assert!(!why.contains("another branch"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    mock.state.lock().unwrap().revs_limit = None;
    // Without the server's history there is no telling: blocked, never success.
    mock.state.lock().unwrap().forget_rev_bodies = true;
    match b.engine.wait_for(&applied("h.md", &kept), soon()).await.unwrap() {
        WaitOutcome::Blocked(why) => assert!(why.contains("history"), "{why}"),
        other => panic!("{other:?}"),
    }
    // A revision that is not one is a mistake of the caller's.
    for bad in ["nope", "3-", "-abc", "99999999999999999999999-abc"] {
        let e = b.engine.wait_for(&applied("h.md", bad), soon()).await.unwrap_err();
        assert!(e.to_string().contains("not a revision"), "{bad}: {e}");
    }
    b.stop().await;
}

#[tokio::test]
async fn idle_waits_for_a_pass_over_unwatched_folders_and_for_failed_pushes() {
    let mock = Mock::start().await;
    // No watcher: a change is seen only by a full pass, and the next
    // periodic one is an hour away.
    let a = Node::start(&mock, false, |c| {
        c.watch = false;
        c.settle = Duration::from_millis(100);
    })
    .await;
    assert!(a.engine.activity().unwatched);
    a.write("unseen.md", b"written where nothing watches");
    match a.engine.wait_for(&WaitFor::Idle, soon()).await.unwrap() {
        WaitOutcome::Reached(_) => {}
        other => panic!("{other:?}"),
    }
    assert!(
        mock.doc("unseen.md").is_some(),
        "idle only once the unwatched change is on the server"
    );
    a.stop().await;

    // A push the server refuses: retried at the next pass, so not idle.
    let a = Node::start(&mock, false, |c| c.settle = Duration::from_millis(100)).await;
    assert!(!a.engine.activity().unwatched);
    mock.state.lock().unwrap().reject_puts.insert("refused.md".into());
    a.write("refused.md", b"the server says no");
    a.until("the push failed", |n| {
        n.index().issues().unwrap().iter().any(|i| i.path == "refused.md")
    })
    .await;
    match a
        .engine
        .wait_for(&WaitFor::Idle, Instant::now() + Duration::from_millis(1500))
        .await
        .unwrap()
    {
        WaitOutcome::TimedOut(why) => assert!(why.contains("push failed"), "{why}"),
        other => panic!("a failed push is not idle: {other:?}"),
    }
    mock.state.lock().unwrap().reject_puts.clear();
    a.engine.rescan_now();
    assert!(matches!(
        a.engine.wait_for(&WaitFor::Idle, soon()).await.unwrap(),
        WaitOutcome::Reached(_)
    ));
    assert!(mock.doc("refused.md").is_some());
    a.stop().await;
}

#[tokio::test]
async fn a_wait_ends_at_a_gate_refusal_and_for_a_path_this_vault_does_not_sync() {
    let mock = Mock::start().await;
    let a = Node::start(&mock, false, |c| c.ignore_regex = Some("^private/".into())).await;
    let t0 = Instant::now();
    match a
        .engine
        .wait_for(
            &WaitFor::Applied {
                path: "private/x.md".into(),
                target: Target::Any,
            },
            soon(),
        )
        .await
        .unwrap()
    {
        WaitOutcome::Blocked(why) => assert!(why.contains("not synced here"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(t0.elapsed() < Duration::from_secs(2));

    // The milestone loses its preferred settings: the gate refuses (with no
    // stop recorded for it) until a person sets the remote up again, so a
    // wait says so at once instead of running into its timeout.
    mock.update_milestone(|m| {
        m["tweak_values"].as_object_mut().unwrap().remove("PREFERRED");
    });
    a.engine.network_changed(true);
    a.until("the gate refused", |n| n.engine.activity().refused.is_some())
        .await;
    assert!(a.engine.alerts().unwrap().upstream_stop.is_none());
    let t0 = Instant::now();
    for cond in [WaitFor::Idle, applied("n.md", "3-abc")] {
        match a.engine.wait_for(&cond, soon()).await.unwrap() {
            WaitOutcome::Blocked(why) => assert!(why.contains("refused") && why.contains("PREFERRED"), "{why}"),
            other => panic!("{other:?}"),
        }
    }
    assert!(t0.elapsed() < Duration::from_secs(2));
    a.stop().await;
}

#[tokio::test]
async fn a_server_that_briefly_is_not_couchdb_is_retried_not_refused() {
    let mock = Mock::start().await;
    let a = Node::start(&mock, false, |_| {}).await;
    // A restarting proxy answers the root with something else.
    mock.state.lock().unwrap().root = serde_json::json!({"not": "couchdb"});
    a.engine.network_changed(true);
    a.until("the gate failed", |n| n.engine.activity().pull == PullPhase::Retrying)
        .await;
    assert!(a.engine.activity().refused.is_none(), "it clears by itself");
    match a
        .engine
        .wait_for(&WaitFor::Idle, Instant::now() + Duration::from_millis(500))
        .await
        .unwrap()
    {
        WaitOutcome::TimedOut(_) => {}
        other => panic!("a wait rides it out: {other:?}"),
    }
    a.stop().await;
}

#[tokio::test]
async fn a_flush_refuses_symlinks_and_answers_for_a_skipped_file() {
    let mock = Mock::start().await;
    let a = Node::start(&mock, false, |c| c.settle = Duration::from_secs(60)).await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.md"), "not the vault's").unwrap();
    std::os::unix::fs::symlink(outside.path(), a.vault().join("link")).unwrap();
    std::fs::create_dir_all(a.vault().join("real")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.md"), a.vault().join("real/file.md")).unwrap();
    for p in ["link", "link/secret.md", "real/file.md"] {
        let e = a.engine.flush(Some(vec![p.into()])).await.unwrap_err();
        assert!(e.to_string().contains("symbolic link"), "{p}: {e}");
    }
    assert!(mock.doc("link/secret.md").is_none() && mock.doc("real/file.md").is_none());

    // A file this index skips (a server document it does not write here):
    // never pushed, so not stored, and said at once.
    a.index()
        .upsert(&livesync_engine::FileRow {
            path: "skipped.md".into(),
            doc_id: "skipped.md".into(),
            state: livesync_engine::State::Skipped,
            note: Some("a name collision".into()),
            ..Default::default()
        })
        .unwrap();
    a.write("skipped.md", b"here only");
    let rep = tokio::time::timeout(Duration::from_secs(10), a.engine.flush(Some(vec!["skipped.md".into()])))
        .await
        .expect("a flush of a skipped file is answered")
        .unwrap();
    let rep = done(rep);
    assert_eq!(rep.paths[0].outcome, "not-synced", "{rep:?}");
    assert!(!rep.complete);
    a.stop().await;
}

#[tokio::test]
async fn a_flush_of_many_files_lists_every_revision_it_stored() {
    let mock = Mock::start().await;
    let a = Node::start(&mock, false, |c| c.settle = Duration::from_secs(60)).await;
    // More than the engine's event buffer, which a report must not depend on.
    let n = 1100;
    for i in 0..n {
        a.write(&format!("many/{i:04}.md"), format!("note {i}").as_bytes());
    }
    let rep = done(a.engine.flush(None).await.unwrap());
    assert!(rep.complete, "{:?}", rep.issues);
    assert_eq!(rep.paths.len(), n);
    a.stop().await;
}
