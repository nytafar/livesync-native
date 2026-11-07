//! The revision history (spec D12): every revision the engine applies,
//! pushes or sees is recorded with what it takes to rebuild its content,
//! in the same commit as the index update it belongs to.

mod common;

use common::{Mock, DB};
use livesync_engine::{Engine, EngineConfig, Index, Origin, RevisionRow, State};
use livesync_model::path::path_to_id;
use livesync_remote::{Couch, RemoteConfig};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

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
    fn index(&self) -> Index {
        Index::open(&self.dir.path().join("state/index.sqlite")).unwrap()
    }
    fn engine(&self) -> Engine {
        let couch = Couch::new(&RemoteConfig {
            url: self.mock.url.clone(),
            database: DB.into(),
            username: Some("u".into()),
            password: Some("p".into()),
        })
        .unwrap();
        let mut cfg = EngineConfig::new(self.vault(), self.dir.path().join("state/index.sqlite"));
        cfg.batch_limit = 5;
        cfg.concurrency = 2;
        cfg.rebuild_quiet = Duration::ZERO;
        cfg.settle = Duration::from_millis(200);
        cfg.delete_hold = Duration::from_millis(500);
        Engine::open(couch, cfg).unwrap()
    }
    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.vault().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    fn history(&self, path: &str) -> Vec<RevisionRow> {
        self.index().revisions_at(path).unwrap()
    }
    fn base_rev(&self, path: &str) -> String {
        self.index()
            .by_doc(&path_to_id(path, true))
            .unwrap()
            .unwrap()
            .base_rev
            .unwrap()
    }
    fn hash(bytes: &[u8]) -> String {
        livesync_engine::vaultfs::hash_bytes(bytes)
    }
}

#[tokio::test]
async fn pulled_revisions_are_recorded_as_remote() {
    let env = Env::new().await;
    let r1 = env.mock.put_text("Notes/a.md", &["first ", "version"]);
    let bin: Vec<u8> = (0..200u8).collect();
    let b1 = env.mock.put_binary("img/b.png", &bin, 64);
    env.engine().pull_once().await.unwrap();

    let h = env.history("Notes/a.md");
    assert_eq!(h.len(), 1, "{h:?}");
    let r = &h[0];
    assert_eq!((r.rev.as_str(), r.doc_id.as_str()), (r1.as_str(), "notes/a.md"));
    assert_eq!((r.origin, r.applied, r.deleted), (Origin::Remote, true, false));
    assert_eq!(r.kind.as_deref(), Some("plain"));
    assert_eq!(
        r.children,
        env.mock.doc("notes/a.md").unwrap()["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        (r.mtime, r.ctime, r.size),
        (1_700_000_100_000, Some(1_700_000_000_000), 13)
    );
    assert_eq!(r.hash.as_deref(), Some(Env::hash(b"first version").as_str()));
    let b = env.index().revision("img/b.png", &b1).unwrap().unwrap();
    assert_eq!(b.kind.as_deref(), Some("newnote"));
    assert_eq!(b.children.len(), 4);
    assert_eq!(b.hash.as_deref(), Some(Env::hash(&bin).as_str()));

    // A remote edit adds a row; seeing the same revision again does not.
    let r2 = env.mock.put_text("Notes/a.md", &["second"]);
    env.engine().pull_once().await.unwrap();
    env.engine().pull_once().await.unwrap();
    let revs: Vec<String> = env.history("Notes/a.md").into_iter().map(|r| r.rev).collect();
    assert_eq!(revs, vec![r1, r2.clone()]);

    // A remote soft deletion: recorded with the body it keeps, applied.
    env.mock.soft_delete("Notes/a.md");
    env.engine().pull_once().await.unwrap();
    assert!(!env.vault().join("Notes/a.md").exists());
    let h = env.history("Notes/a.md");
    let d = h.last().unwrap();
    assert!(d.deleted && d.applied, "{d:?}");
    assert_eq!(d.origin, Origin::Remote);
    assert_eq!(d.children, h[1].children, "a soft deletion keeps its chunks");

    // A tombstone (no body): filed under the path the index knew.
    env.mock.tombstone("img/b.png");
    env.engine().pull_once().await.unwrap();
    let t = env.history("img/b.png");
    assert_eq!(t.len(), 2, "{t:?}");
    assert!(t[1].deleted && t[1].applied && t[1].children.is_empty());
    assert_eq!(t[1].kind, None);
}

#[tokio::test]
async fn a_remote_revision_held_back_by_a_local_edit_is_recorded_as_seen() {
    let env = Env::new().await;
    let r1 = env.mock.put_text("n.md", &["base"]);
    env.engine().pull_once().await.unwrap();
    env.write("n.md", b"local edit");
    let r2 = env.mock.put_text("n.md", &["remote edit"]);
    env.engine().pull_once().await.unwrap();
    assert_eq!(env.index().by_doc("n.md").unwrap().unwrap().state, State::Dirty);
    let h = env.history("n.md");
    assert_eq!(h.iter().map(|r| r.rev.clone()).collect::<Vec<_>>(), vec![r1, r2]);
    assert!(h[0].applied);
    assert!(!h[1].applied, "not on disk: {:?}", h[1]);
    assert_eq!(h[1].hash.as_deref(), Some(Env::hash(b"remote edit").as_str()));

    // Skipped documents are seen too (here: a dot-path), out-of-scope ones
    // are not recorded at all.
    env.mock.put_text(".obsidian/app.json", &["{}"]);
    env.engine().pull_once().await.unwrap();
    let s = env.history(".obsidian/app.json");
    assert_eq!(s.len(), 1);
    assert!(!s[0].applied && s[0].hash.is_none());
}

#[tokio::test]
async fn pushed_revisions_are_recorded_as_local_including_branches_and_deletes() {
    let env = Env::new().await;
    env.write("c.md", b"base\n");
    env.engine().sync_once().await.unwrap();
    let h = env.history("c.md");
    assert_eq!(h.len(), 1);
    assert_eq!((h[0].origin, h[0].applied), (Origin::Local, true));
    assert_eq!(h[0].rev, env.base_rev("c.md"));
    assert_eq!(h[0].kind.as_deref(), Some("plain"));
    assert_eq!(h[0].hash.as_deref(), Some(Env::hash(b"base\n").as_str()));
    assert_eq!(
        serde_json::to_value(&h[0].children).unwrap(),
        env.mock.doc("c.md").unwrap()["children"]
    );

    // A remote edit and a local one of the same base: the pull sees the
    // remote one (held back), the push stores ours as a branch.
    let ch = env.mock.put_chunks(&["remote edit\n"]);
    let mut d = env.mock.doc("c.md").unwrap();
    d["children"] = json!(ch);
    d["size"] = json!(12);
    let theirs = env.mock.put(d);
    env.write("c.md", b"local edit\n");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.conflicts, 1, "{rep:?}");
    let ours = env.base_rev("c.md");
    let ix = env.index();
    let t = ix.revision("c.md", &theirs).unwrap().unwrap();
    assert_eq!((t.origin, t.applied), (Origin::Remote, false));
    let o = ix.revision("c.md", &ours).unwrap().unwrap();
    assert_eq!((o.origin, o.applied), (Origin::Local, true));
    assert_eq!(o.hash.as_deref(), Some(Env::hash(b"local edit\n").as_str()));
    assert_eq!(ix.revision_count().unwrap(), 3);

    // A local delete of a clean file: the soft deletion we pushed.
    env.write("gone.md", b"bye");
    env.engine().sync_once().await.unwrap();
    std::fs::remove_file(env.vault().join("gone.md")).unwrap();
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 1, "{rep:?}");
    let g = env.history("gone.md");
    assert_eq!(g.len(), 2, "{g:?}");
    let del = &g[1];
    assert_eq!(del.rev, env.mock.doc("gone.md").unwrap()["_rev"].as_str().unwrap());
    assert!(del.deleted && del.applied);
    assert_eq!(del.origin, Origin::Local);
    assert_eq!(del.children, g[0].children);
    assert_eq!(del.kind.as_deref(), Some("plain"));
}

#[tokio::test]
async fn nothing_is_recorded_when_an_apply_or_push_fails() {
    let env = Env::new().await;
    // A pull whose chunks are missing is deferred, not applied.
    env.mock.put(
        json!({"_id": "m.md", "path": "m.md", "type": "plain", "children": ["h:missing"],
        "ctime": 1, "mtime": 2, "size": 3, "eden": {}}),
    );
    // A local file where the remote document needs a folder: the write fails.
    env.write("blocked", b"a file, not a folder");
    env.mock.put_text("blocked/x.md", &["x"]);
    let rep = env.engine().pull_once().await.unwrap();
    assert!(rep.pending >= 2, "{rep:?}");
    assert!(env.history("m.md").is_empty());
    assert!(env.history("blocked/x.md").is_empty());
    assert!(env.index().by_doc("m.md").unwrap().is_none());

    // A push the server refuses, and one whose metadata write fails.
    env.mock.state.lock().unwrap().reject_puts.insert("r.md".into());
    env.write("r.md", b"refused");
    let rep = env.engine().push_all(false).await.unwrap();
    assert_eq!(rep.failed, 1, "{rep:?}");
    assert!(env.history("r.md").is_empty());
    assert!(env.index().by_doc("r.md").unwrap().is_none());
    env.mock.state.lock().unwrap().reject_puts.clear();
    env.write("p.md", b"v1");
    env.engine().push_all(false).await.unwrap();
    env.write("p.md", b"v2");
    env.mock.state.lock().unwrap().fail_writes = 100;
    assert!(env.engine().push_all(false).await.is_err());
    env.mock.state.lock().unwrap().fail_writes = 0;
    assert_eq!(env.history("p.md").len(), 1);

    // Stored on the server, but the answer is lost: the index never took
    // the revision, so neither does the history.
    env.mock.state.lock().unwrap().lose_next_put_response = true;
    let rep = env.engine().push_all(false).await.unwrap();
    assert_eq!(rep.failed, 1, "{rep:?}");
    let on_server = env.mock.doc("p.md").unwrap()["_rev"].as_str().unwrap().to_string();
    let h = env.history("p.md");
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!(h[0].rev, env.base_rev("p.md"), "the history matches the index");
    assert_ne!(h[0].rev, on_server);
    // The next sync finds it on the server with our content and adopts it.
    env.engine().sync_once().await.unwrap();
    let h = env.history("p.md");
    assert_eq!(h.len(), 2, "{h:?}");
    assert_eq!((h[1].rev.as_str(), h[1].applied), (on_server.as_str(), true));
    assert_eq!(h[1].rev, env.base_rev("p.md"));
}
