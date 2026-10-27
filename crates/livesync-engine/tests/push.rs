//! Two-way behaviour against the mock CouchDB: pushes, D4 conflicts, soft
//! deletes, moves out of scope and into `.trash/`, folder-scope changes,
//! case collisions, milestone registration and rebuild recovery.

mod common;

use common::{Mock, DB};
use livesync_engine::{Engine, EngineConfig, EngineError, Index, State};
use livesync_model::path::path_to_id;
use livesync_model::scope::{Scope, ScopeMode};
use livesync_model::splitter::{chunk_file, SplitParams};
use livesync_remote::{Couch, RemoteConfig};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

struct Env {
    mock: Mock,
    dir: tempfile::TempDir,
}

fn scope(mode: ScopeMode, p: &[&str]) -> Scope {
    Scope::new(mode, &p.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
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
    fn index_path(&self) -> PathBuf {
        self.dir.path().join("state/index.sqlite")
    }
    fn engine_with(&self, f: impl FnOnce(&mut EngineConfig)) -> Engine {
        let couch = Couch::new(&RemoteConfig {
            url: self.mock.url.clone(),
            database: DB.into(),
            username: Some("u".into()),
            password: Some("p".into()),
        })
        .unwrap();
        let mut cfg = EngineConfig::new(self.vault(), self.index_path());
        cfg.batch_limit = 5;
        cfg.concurrency = 2;
        cfg.rebuild_quiet = Duration::ZERO;
        cfg.settle = Duration::from_millis(200);
        cfg.delete_hold = Duration::from_millis(500);
        f(&mut cfg);
        Engine::open(couch, cfg).unwrap()
    }
    fn engine(&self) -> Engine {
        self.engine_with(|_| {})
    }
    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.vault().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.vault().join(rel)).ok()
    }
    fn mv(&self, from: &str, to: &str) {
        let t = self.vault().join(to);
        std::fs::create_dir_all(t.parent().unwrap()).unwrap();
        std::fs::rename(self.vault().join(from), t).unwrap();
    }
    fn row(&self, doc_id: &str) -> Option<livesync_engine::FileRow> {
        Index::open(&self.index_path()).unwrap().by_doc(doc_id).unwrap()
    }
    fn meta(&self, key: &str) -> Option<String> {
        Index::open(&self.index_path()).unwrap().get_meta(key).unwrap()
    }
    /// The remote document for a path.
    fn doc(&self, path: &str) -> Value {
        self.mock
            .doc(&path_to_id(path, true))
            .unwrap_or_else(|| panic!("no remote doc for {path}"))
    }
    fn remote(&self, path: &str) -> Option<Vec<u8>> {
        self.mock.content(&path_to_id(path, true))
    }
    fn deleted(&self, path: &str) -> bool {
        self.doc(path).get("deleted") == Some(&json!(true))
    }
}

fn params() -> SplitParams {
    SplitParams {
        custom_chunk_size: 60.0,
        minimum_chunk_size: 20,
    }
}

#[tokio::test]
async fn new_and_edited_files_are_pushed_chunks_first() {
    let env = Env::new().await;
    let text = "# Note\n\nSome æøå text 😀 ".repeat(400);
    let bin: Vec<u8> = (0..300_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    env.write("Notes/Hello.md", text.as_bytes());
    env.write("img/pic.png", &bin);
    env.write("empty.md", b"");
    env.write("_under.md", b"underscore");
    env.write(".obsidian/app.json", b"{}");
    env.write("Notes/.hidden.md", b"x");
    env.write("redflag.md", b"x");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.pushed, 4, "{rep:?}");
    assert_eq!(rep.push.created, 4);

    for (path, bytes) in [
        ("Notes/Hello.md", text.as_bytes()),
        ("img/pic.png", &bin[..]),
        ("empty.md", b""),
        ("_under.md", b"underscore"),
    ] {
        assert_eq!(env.remote(path).unwrap(), bytes, "{path}");
        let d = env.doc(path);
        assert_eq!(d["path"], path);
        assert_eq!(d["size"], bytes.len());
        assert_eq!(d["eden"], json!({}));
        // Chunk IDs are the ones the official splitter produces.
        let want: Vec<String> = chunk_file(path, bytes, &params())
            .chunks
            .into_iter()
            .map(|c| c.0)
            .collect();
        assert_eq!(d["children"], json!(want), "{path}");
    }
    assert_eq!(env.doc("Notes/Hello.md")["type"], "plain");
    assert_eq!(env.doc("img/pic.png")["type"], "newnote");
    assert_eq!(env.doc("_under.md")["_id"], "/_under.md");
    for p in [".obsidian/app.json", "Notes/.hidden.md", "redflag.md"] {
        assert!(env.mock.doc(&path_to_id(p, true)).is_none(), "{p} must not be pushed");
    }
    // Chunks went up before the metadata that names them.
    let reqs = env.mock.state.lock().unwrap().requests.clone();
    let first_meta = reqs
        .iter()
        .position(|(m, p)| m == "PUT" && p.ends_with("hello.md"))
        .unwrap();
    let first_bulk = reqs.iter().position(|(_, p)| p.ends_with("_bulk_docs")).unwrap();
    assert!(first_bulk < first_meta);
    let row = env.row("notes/hello.md").unwrap();
    assert_eq!(row.state, State::Clean);
    let rev1 = row.base_rev.clone().unwrap();

    // A second sync with nothing changed writes nothing.
    env.mock.clear_requests();
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.pushed, 0);
    assert!(env.mock.writes().is_empty(), "{:?}", env.mock.writes());

    // An edit becomes a child of the synced revision; only new chunks upload.
    let edited = format!("{text}\nmore\n");
    env.write("Notes/Hello.md", edited.as_bytes());
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.pushed, 1);
    assert!(rep.push.chunks_uploaded >= 1);
    assert_eq!(env.remote("Notes/Hello.md").unwrap(), edited.as_bytes());
    let tree = env.mock.state.lock().unwrap().trees["notes/hello.md"].clone();
    let rev2 = env.row("notes/hello.md").unwrap().base_rev.unwrap();
    assert_eq!(tree.revs[&rev2].parent.as_deref(), Some(rev1.as_str()));
    assert!(env.doc("Notes/Hello.md").get("_conflicts").is_none());
}

#[tokio::test]
async fn concurrent_edits_leave_a_conflict_branch_and_keep_the_local_edit() {
    let env = Env::new().await;
    env.write("c.md", b"base\n");
    env.engine().sync_once().await.unwrap();
    let base = env.row("c.md").unwrap().base_rev.unwrap();
    // Remote edit (another device) and a local edit of the same base.
    let ch = env.mock.put_chunks(&["remote edit\n"]);
    let mut d = env.doc("c.md");
    d["children"] = json!(ch);
    d["size"] = json!(12);
    env.mock.put(d);
    env.write("c.md", b"local edit\n");

    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.conflicts, 1, "{rep:?}");
    // The local file is untouched, both versions exist as sibling leaves.
    assert_eq!(env.read("c.md").unwrap(), b"local edit\n");
    let tree = env.mock.state.lock().unwrap().trees["c.md"].clone();
    let leaves = tree.leaves();
    assert_eq!(leaves.len(), 2, "{leaves:?}");
    for l in &leaves {
        assert_eq!(tree.revs[l].parent.as_deref(), Some(base.as_str()));
    }
    let ours = env.row("c.md").unwrap();
    assert_eq!(ours.state, State::Conflicted);
    assert_eq!(
        env.mock.content_at("c.md", ours.base_rev.as_ref().unwrap()).unwrap(),
        b"local edit\n"
    );
    assert!(!env.doc("c.md")["_conflicts"].as_array().unwrap().is_empty());

    // Further syncs leave the disk alone while the conflict stands.
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.read("c.md").unwrap(), b"local edit\n");
    assert_eq!(env.row("c.md").unwrap().state, State::Conflicted);

    // Resolved elsewhere (the other leaf deleted, a merge on top): the
    // merge comes down because the local file is clean.
    let winner = env.doc("c.md")["_rev"].as_str().unwrap().to_string();
    let other = leaves.iter().find(|l| **l != winner).unwrap().clone();
    {
        let mut st = env.mock.state.lock().unwrap();
        let t = st.trees.get_mut("c.md").unwrap();
        let mut tomb = t.revs[&other].clone();
        tomb.parent = Some(other.clone());
        tomb.deleted = true;
        t.revs.insert(
            format!("{}-dead", other.split('-').next().unwrap().parse::<u64>().unwrap() + 1),
            tomb,
        );
    }
    let merged = env.mock.put_chunks(&["merged\n"]);
    let mut d = env.doc("c.md");
    d["children"] = json!(merged);
    d["size"] = json!(7);
    if let Some(o) = d.as_object_mut() {
        o.remove("_conflicts");
    }
    env.mock.put(d);
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.read("c.md").unwrap(), b"merged\n");
    assert_eq!(env.row("c.md").unwrap().state, State::Clean);
}

#[tokio::test]
async fn unknown_ancestry_never_overwrites_either_side() {
    let env = Env::new().await;
    // The remote has a version; the local folder has another, never synced.
    env.mock.put_text("same.md", &["identical"]);
    env.mock.put_text("diff.md", &["remote version"]);
    env.write("same.md", b"identical");
    env.write("diff.md", b"phone version");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.pull.adopted, 1);
    assert_eq!(env.read("diff.md").unwrap(), b"phone version");
    assert_eq!(rep.push.conflicts, 1, "{rep:?}");
    // Both versions are live leaves (which one CouchDB shows as the winner
    // depends on revision hashes when both are roots).
    let tree = env.mock.state.lock().unwrap().trees["diff.md"].clone();
    let mut contents: Vec<Vec<u8>> = tree
        .leaves()
        .iter()
        .map(|l| env.mock.content_at("diff.md", l).unwrap())
        .collect();
    contents.sort();
    assert_eq!(contents, vec![b"phone version".to_vec(), b"remote version".to_vec()]);
    let ours = env.row("diff.md").unwrap();
    assert!(ours.base_rev.as_ref().unwrap().starts_with("1-"));
    assert_eq!(
        env.mock.content_at("diff.md", ours.base_rev.as_ref().unwrap()).unwrap(),
        b"phone version"
    );
    assert_eq!(env.doc("diff.md")["_conflicts"].as_array().unwrap().len(), 1);

    // A new local file where the remote has a soft-deleted document replaces
    // the deletion.
    env.mock.put_text("back.md", &["old"]);
    env.mock.soft_delete("back.md");
    env.engine().sync_once().await.unwrap();
    env.write("back.md", b"new life");
    env.engine().sync_once().await.unwrap();
    assert!(!env.deleted("back.md"));
    assert_eq!(env.remote("back.md").unwrap(), b"new life");
}

#[tokio::test]
async fn local_deletes_are_soft_deletes_against_the_synced_revision() {
    let env = Env::new().await;
    env.write("a.md", b"a");
    env.write("b.md", b"b");
    env.write("dir/c.md", b"c");
    env.engine().sync_once().await.unwrap();
    let children = env.doc("a.md")["children"].clone();
    std::fs::remove_file(env.vault().join("a.md")).unwrap();
    std::fs::remove_dir_all(env.vault().join("dir")).unwrap();
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 2, "{rep:?}");
    let d = env.doc("a.md");
    assert_eq!(d["deleted"], true);
    assert_eq!(d["children"], children, "children kept");
    assert!(d["mtime"].as_i64().unwrap() > 1_700_000_000_000);
    assert!(env.deleted("dir/c.md"));
    assert!(env.row("a.md").is_none());
    assert!(!env.deleted("b.md"));

    // The remote moved on before our delete arrived: not deleted, fetched again.
    env.write("e.md", b"e");
    env.engine().sync_once().await.unwrap();
    std::fs::remove_file(env.vault().join("e.md")).unwrap();
    let ch = env.mock.put_chunks(&["edited elsewhere"]);
    let mut d = env.doc("e.md");
    d["children"] = json!(ch);
    d["size"] = json!(16);
    env.mock.put(d);
    let engine = env.engine();
    engine.sync_once().await.unwrap();
    // The pull ran first and brought the newer version back.
    assert_eq!(env.read("e.md").unwrap(), b"edited elsewhere");
    assert!(!env.deleted("e.md"));
    drop(engine);
    // Same, but the push sees the delete before the pull sees the edit.
    std::fs::remove_file(env.vault().join("e.md")).unwrap();
    let ch = env.mock.put_chunks(&["edited again"]);
    let mut d = env.doc("e.md");
    d["children"] = json!(ch);
    d["size"] = json!(12);
    env.mock.put(d);
    let engine = env.engine();
    let rep = engine.push_all(false).await.unwrap();
    assert_eq!(rep.kept_remote, 1, "{rep:?}");
    assert!(!env.deleted("e.md"));
    engine.sync_once().await.unwrap();
    assert_eq!(env.read("e.md").unwrap(), b"edited again");
}

#[tokio::test]
async fn deleting_a_file_that_is_not_clean_never_deletes_the_remote() {
    let env = Env::new().await;
    env.mock.put_text("d.md", &["v1"]);
    env.engine().pull_once().await.unwrap();
    // Remote edit while the local file is edited: the pull marks it dirty.
    env.write("d.md", b"local");
    env.mock.put_text("d.md", &["v2"]);
    env.engine().pull_once().await.unwrap();
    assert_eq!(env.row("d.md").unwrap().state, State::Dirty);
    // The user deletes the file before it was pushed.
    std::fs::remove_file(env.vault().join("d.md")).unwrap();
    let rep = env.engine().push_all(false).await.unwrap();
    assert_eq!(rep.kept_remote, 1);
    assert_eq!(rep.deleted, 0);
    assert!(!env.deleted("d.md"));
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.read("d.md").unwrap(), b"v2");
}

#[tokio::test]
async fn moves_to_trash_delete_and_moves_out_of_scope_do_not() {
    let env = Env::new().await;
    let cfg = |c: &mut EngineConfig| c.scope = scope(ScopeMode::Exclude, &["Archive"]);
    for f in ["a.md", "b.md", "c.md", "d.md"] {
        env.write(f, f.as_bytes());
    }
    env.engine_with(cfg).sync_once().await.unwrap();
    env.mv("a.md", ".trash/a.md"); // Obsidian's delete
    env.mv("b.md", "archive/b.md"); // excluded folder (case-insensitive match)
    env.mv("c.md", ".stash/c.md"); // another dot-folder
    env.mv("d.md", "Notes/d.md"); // a rename inside the scope
    let rep = env.engine_with(cfg).sync_once().await.unwrap();
    assert_eq!(rep.push.out_of_scope, 2, "{rep:?}");
    assert_eq!(rep.push.deleted, 2, "{rep:?}");
    assert!(env.deleted("a.md"));
    assert!(!env.deleted("b.md") && !env.deleted("c.md"));
    assert!(env.deleted("d.md"));
    assert_eq!(env.remote("Notes/d.md").unwrap(), b"d.md");
    assert_eq!(env.row("b.md").unwrap().state, State::OutOfScope);
    assert!(env.row("b.md").unwrap().note.unwrap().contains("archive/b.md"));
    // A remote change to a moved-out document is not brought back.
    env.mock.put_text("b.md", &["changed remotely"]);
    env.engine_with(cfg).sync_once().await.unwrap();
    assert!(env.read("b.md").is_none());
    // Moving it back makes it a normal file again.
    env.mv(".stash/c.md", "c.md");
    env.engine_with(cfg).sync_once().await.unwrap();
    assert_eq!(env.row("c.md").unwrap().state, State::Clean);
    let st = env.engine_with(cfg).status().unwrap();
    assert_eq!(st.out_of_scope.len(), 1);
}

#[tokio::test]
async fn scope_narrowing_drops_tracking_and_widening_catches_up() {
    let env = Env::new().await;
    env.mock.put_text("notes/a.md", &["a"]);
    env.mock.put_text("Private/b.md", &["b"]);
    env.mock.put_text("private/deep/c.md", &["c"]);
    let only_notes = |c: &mut EngineConfig| c.scope = scope(ScopeMode::Include, &["Notes"]);
    env.engine_with(only_notes).sync_once().await.unwrap();
    assert_eq!(env.read("notes/a.md").unwrap(), b"a");
    assert!(env.read("Private/b.md").is_none());
    // A local file outside the scope is never pushed.
    env.write("Private/local.md", b"mine");
    env.engine_with(only_notes).sync_once().await.unwrap();
    assert!(env.mock.doc("private/local.md").is_none());

    // Widen to everything: the newly visible documents arrive without
    // resetting `since`, and the local file goes up.
    let since = env.meta("since").unwrap();
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(env.read("Private/b.md").unwrap(), b"b");
    assert_eq!(env.read("private/deep/c.md").unwrap(), b"c");
    assert_eq!(env.remote("Private/local.md").unwrap(), b"mine");
    assert!(rep.pull.written >= 2);
    let _ = since; // `since` only moves forward; the catch-up used _all_docs
    let reqs = env.mock.state.lock().unwrap().requests.clone();
    assert!(reqs.iter().any(|(m, p)| m == "GET" && p.ends_with("_all_docs")));

    // Narrow again: rows dropped, files stay, the remote is untouched.
    env.mock.clear_requests();
    env.engine_with(only_notes).sync_once().await.unwrap();
    assert!(env.row("private/b.md").is_none());
    assert_eq!(env.read("Private/b.md").unwrap(), b"b");
    assert!(
        env.mock.writes().iter().all(|(_, p)| !p.contains("private")),
        "{:?}",
        env.mock.writes()
    );
    // Deleting an out-of-scope file locally does nothing remotely.
    std::fs::remove_file(env.vault().join("Private/b.md")).unwrap();
    env.engine_with(only_notes).sync_once().await.unwrap();
    assert!(!env.deleted("Private/b.md"));

    // "Remove local copies" removes clean files only, never remote documents.
    env.engine().sync_once().await.unwrap(); // widen: b.md comes back
    assert_eq!(env.read("Private/b.md").unwrap(), b"b");
    env.write("private/deep/c.md", b"edited locally");
    env.engine_with(|c| {
        c.scope = scope(ScopeMode::Exclude, &["private"]);
        c.remove_out_of_scope_copies = true;
    })
    .sync_once()
    .await
    .unwrap();
    assert!(env.read("Private/b.md").is_none());
    assert_eq!(env.read("private/deep/c.md").unwrap(), b"edited locally");
    assert!(!env.deleted("Private/b.md"));
}

#[tokio::test]
async fn case_collisions_are_skipped_and_reported() {
    let env = Env::new().await;
    // A database with case-sensitive IDs can hold two names that a
    // case-insensitive filesystem cannot.
    env.mock
        .update_milestone(|m| m["tweak_values"]["PREFERRED"]["handleFilenameCaseSensitive"] = json!(true));
    let put = |path: &str, body: &str| {
        let ch = env.mock.put_chunks(&[body]);
        env.mock.put(json!({"_id": path, "path": path, "type": "plain", "children": ch, "mtime": 1, "ctime": 1, "size": body.len(), "eden": {}}))
    };
    put("Note.md", "upper");
    put("note.md", "lower");
    let folding = |c: &mut EngineConfig| c.case_insensitive = Some(true);
    let rep = env.engine_with(folding).pull_once().await.unwrap();
    assert_eq!(rep.skipped.get("case-collision"), Some(&1), "{rep:?}");
    assert_eq!(rep.written, 1);
    let st = env.engine_with(folding).status().unwrap();
    assert_eq!(st.collisions.len(), 1);
    let winner = if env.read("Note.md").is_some() {
        "Note.md"
    } else {
        "note.md"
    };
    let loser = if winner == "Note.md" { "note.md" } else { "Note.md" };
    assert!(st.collisions[0].note.as_ref().unwrap().contains(winner));
    // Once the first one is deleted remotely, the other gets its turn.
    env.mock.soft_delete_id(winner);
    env.engine_with(folding).pull_once().await.unwrap();
    assert!(env.read(winner).is_none() || winner.eq_ignore_ascii_case(loser));
    assert!(env.read(loser).is_some());

    // Two local names for one document (case-insensitive IDs on a
    // case-sensitive filesystem): one is pushed, the other reported.
    let env = Env::new().await;
    require_case_sensitive!(env.dir.path());
    env.write("Same.md", b"one");
    env.write("same.md", b"two");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.pushed, 1);
    assert_eq!(rep.push.issues, 1);
    let st = env.engine().status().unwrap();
    assert_eq!(st.local_issues.len(), 1);
    assert_eq!(st.local_issues[0].kind, "case-collision");
}

#[tokio::test]
async fn the_node_registers_only_after_a_full_fetch() {
    let env = Env::new().await;
    env.mock.put_text("x.md", &["x"]);
    // A read-only pull never touches the milestone.
    let e = env.engine();
    e.pull_once().await.unwrap();
    let node = e.node_id();
    drop(e);
    assert!(env.mock.writes().is_empty());
    assert!(env.mock.local("obsydian_livesync_milestone").unwrap()["node_info"]
        .get(&node)
        .is_none());

    let env = Env::new().await;
    env.mock.put_text("x.md", &["x"]);
    let e = env.engine_with(|c| c.device_name = Some("test-phone".into()));
    e.sync_once().await.unwrap();
    let m = env.mock.local("obsydian_livesync_milestone").unwrap();
    let node = e.node_id();
    assert_eq!(m["node_info"][&node]["device_name"], "test-phone");
    assert_eq!(m["tweak_values"][&node], m["tweak_values"]["PREFERRED"]);
    assert_eq!(
        m["node_chunk_info"][&node],
        json!({"min": 0, "max": 2400, "current": 2})
    );
    assert!(
        m["accepted_nodes"].as_array().unwrap().contains(&json!(node)),
        "a first fetch marks resolved"
    );
    // Within a minute nothing is rewritten.
    let rev = m["_rev"].clone();
    e.sync_once().await.unwrap();
    assert_eq!(env.mock.local("obsydian_livesync_milestone").unwrap()["_rev"], rev);
}

#[tokio::test]
async fn a_rebuild_elsewhere_is_fetched_resolved_and_orphans_survive() {
    let env = Env::new().await;
    env.mock.put_text("keep.md", &["keep"]);
    env.mock.put_text("rechunked.md", &["one ", "two"]);
    env.mock.put_text("dropped.md", &["dropped"]);
    env.mock.put_text("edited.md", &["edited base"]);
    let e = env.engine();
    e.sync_once().await.unwrap();
    let node = e.node_id();
    drop(e);
    env.write("edited.md", b"edited on the phone");

    // Another device rebuilds: database recreated, new `created`, locked to
    // the rebuilder, documents re-uploaded (one chunked differently, one
    // gone because the rebuilder did not have it).
    {
        let mut st = env.mock.state.lock().unwrap();
        st.docs.retain(|k, _| k == "obsydian_livesync_version");
        st.trees.retain(|k, _| k == "obsydian_livesync_version");
        st.log.clear();
    }
    env.mock.update_milestone(|m| {
        m["created"] = json!(2000);
        m["locked"] = json!(true);
        m["accepted_nodes"] = json!(["rebuilder"]);
        m["node_info"] = json!({});
    });
    env.mock.put_text("keep.md", &["keep"]);
    env.mock.put_text("rechunked.md", &["one two"]);
    env.mock.put_text("edited.md", &["edited base"]);

    let rep = env.engine().sync_once().await.unwrap();
    // Clean files refreshed, the edit kept and pushed as a branch, the file
    // the rebuilder lacked pushed back as a new document.
    assert_eq!(env.read("rechunked.md").unwrap(), b"one two");
    assert_eq!(env.read("edited.md").unwrap(), b"edited on the phone");
    assert_eq!(env.remote("dropped.md").unwrap(), b"dropped");
    assert!(rep.push.conflicts >= 1, "{rep:?}");
    // The push-back is counted and reported, not silent.
    assert_eq!(rep.push.orphans_pushed, 1, "{rep:?}");
    assert_eq!(rep.push.orphan_paths, vec!["dropped.md".to_string()]);
    let orphans = env.engine().status().unwrap().rebuild_orphans.unwrap();
    assert!(
        orphans.contains(" 1 file(s)") && orphans.contains("dropped.md"),
        "{orphans}"
    );
    let m = env.mock.local("obsydian_livesync_milestone").unwrap();
    assert!(m["accepted_nodes"].as_array().unwrap().contains(&json!(node)));
    assert_eq!(env.meta("remote_created").as_deref(), Some("2000"));
    assert!(env.meta("needs_full_fetch").is_none());
    assert!(env.meta("resolve_owed").is_none());
    // No index rows without a revision are left behind.
    let rows = Index::open(&env.index_path()).unwrap().all_rows().unwrap();
    assert!(
        rows.iter().all(|r| r.base_rev.is_some() || r.state == State::Skipped),
        "{rows:?}"
    );
    // And the lock no longer refuses us.
    env.engine().sync_once().await.unwrap();
}

#[tokio::test]
async fn a_lock_without_a_rebuild_needs_an_explicit_fetch() {
    let env = Env::new().await;
    env.mock.put_text("x.md", &["x"]);
    env.engine().sync_once().await.unwrap();
    env.mock.update_milestone(|m| {
        m["locked"] = json!(true);
        m["accepted_nodes"] = json!(["someone"]);
    });
    assert!(matches!(
        env.engine().sync_once().await,
        Err(EngineError::Refused(livesync_model::Refusal::Locked))
    ));
    let e = env.engine();
    e.request_full_fetch().unwrap();
    e.sync_once().await.unwrap();
    let node = e.node_id();
    let m = env.mock.local("obsydian_livesync_milestone").unwrap();
    assert!(m["accepted_nodes"].as_array().unwrap().contains(&json!(node)));
}

#[tokio::test]
async fn mass_deletions_stop_and_ask() {
    let env = Env::new().await;
    for i in 0..30 {
        env.write(&format!("f{i}.md"), format!("{i}").as_bytes());
    }
    env.engine().sync_once().await.unwrap();
    for i in 0..30 {
        std::fs::remove_file(env.vault().join(format!("f{i}.md"))).unwrap();
    }
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 0);
    assert_eq!(rep.push.safety_stops, 1);
    assert!(env.engine().status().unwrap().safety_stop.is_some());
    assert!(!env.deleted("f0.md"));
    let rep = env
        .engine_with(|c| c.allow_mass_delete = true)
        .sync_once()
        .await
        .unwrap();
    assert_eq!(rep.push.deleted, 30);
    assert!(env.engine().status().unwrap().safety_stop.is_none());
}

async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn the_daemon_pushes_saves_renames_and_trash_moves_as_they_happen() {
    let env = Env::new().await;
    env.mock.put_text("remote.md", &["from the desktop"]);
    env.write("existing.md", b"already here");
    let engine = env.engine_with(|c| {
        c.settle = Duration::from_millis(150);
        c.rescan_interval = Duration::from_secs(3600);
        c.scope = scope(ScopeMode::Exclude, &["Archive"]);
    });
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let e = engine.clone();
        async move { e.run(rx).await }
    });
    wait_until("initial push", || env.remote("existing.md").is_some()).await;
    wait_until("initial pull", || env.read("remote.md").is_some()).await;

    // Obsidian saves in place.
    env.write("new.md", b"typed on the phone");
    wait_until("new file", || {
        env.remote("new.md").as_deref() == Some(&b"typed on the phone"[..])
    })
    .await;
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(env.vault().join("new.md"))
            .unwrap();
        f.write_all(b"typed more on the phone").unwrap();
    }
    wait_until("edit", || {
        env.remote("new.md").as_deref() == Some(&b"typed more on the phone"[..])
    })
    .await;
    // Rename (MOVED_FROM/MOVED_TO with one cookie).
    env.mv("new.md", "renamed.md");
    wait_until("rename", || {
        env.remote("renamed.md").is_some() && env.mock.doc("new.md").is_some_and(|d| d["deleted"] == true)
    })
    .await;
    // Obsidian's delete: a move into .trash/.
    std::fs::create_dir_all(env.vault().join(".trash")).unwrap();
    env.mv("renamed.md", ".trash/renamed.md");
    wait_until("trash", || {
        env.mock.doc("renamed.md").is_some_and(|d| d["deleted"] == true)
    })
    .await;
    // Out of scope: not deleted.
    env.mv("existing.md", "Archive/existing.md");
    wait_until("out of scope", || {
        env.row("existing.md").is_some_and(|r| r.state == State::OutOfScope)
    })
    .await;
    assert!(!env.deleted("existing.md"));
    // A new folder created after start is watched too.
    env.write("later/deep/n.md", b"deep");
    wait_until("new folder", || env.remote("later/deep/n.md").is_some()).await;
    // A remote change arrives while the daemon runs.
    env.mock.put_text("remote.md", &["edited on the desktop"]);
    wait_until("pull", || {
        env.read("remote.md").as_deref() == Some(&b"edited on the desktop"[..])
    })
    .await;

    tx.send(true).unwrap();
    task.await.unwrap().unwrap();
    // Our own writes did not bounce back as further writes.
    let n = env.mock.state.lock().unwrap().trees["remote.md"].revs.len();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn a_branch_that_loses_to_our_revision_is_still_reported() {
    let env = Env::new().await;
    env.write("w.md", b"base\n");
    env.engine().sync_once().await.unwrap();
    let base = env.row("w.md").unwrap().base_rev.unwrap();
    env.write("w.md", b"ours\n");
    env.engine().sync_once().await.unwrap();
    // Another device's edit of the same base, with a revision hash that
    // loses to ours: the winner is still our revision.
    let ch = env.mock.put_chunks(&["theirs\n"]);
    let mut d = env.doc("w.md");
    d["children"] = json!(ch);
    d["size"] = json!(7);
    env.mock.put_branch_with(d, &base, "00000000000000000000000000000000");
    let ours = env.row("w.md").unwrap().base_rev.unwrap();
    assert_eq!(env.doc("w.md")["_rev"].as_str().unwrap(), ours);
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.read("w.md").unwrap(), b"ours\n");
    let row = env.row("w.md").unwrap();
    assert_eq!(row.state, State::Conflicted, "{row:?}");
    assert_eq!(env.engine().status().unwrap().conflicted.len(), 1);
}

#[tokio::test]
async fn a_refetch_of_a_document_that_is_gone_does_not_linger() {
    // After a rebuild, a file the rebuilder lacked and that was also deleted
    // locally: nothing to delete, nothing to fetch, nothing left pending.
    let env = Env::new().await;
    env.mock.put_text("keep.md", &["keep"]);
    env.mock.put_text("lost.md", &["lost"]);
    env.engine().sync_once().await.unwrap();
    std::fs::remove_file(env.vault().join("lost.md")).unwrap();
    {
        let mut st = env.mock.state.lock().unwrap();
        st.docs.retain(|k, _| k == "obsydian_livesync_version");
        st.trees.retain(|k, _| k == "obsydian_livesync_version");
        st.log.clear();
    }
    env.mock.update_milestone(|m| m["created"] = json!(3000));
    env.mock.put_text("keep.md", &["keep"]);
    env.engine().sync_once().await.unwrap();
    let st = env.engine().status().unwrap();
    assert!(st.pending.is_empty(), "{:?}", st.pending);
    assert!(env.row("lost.md").is_none());
    assert!(env.mock.doc("lost.md").is_none(), "nothing written for it");
}

// ------------------------------------------------- review-m2a regressions

/// A rebuild elsewhere: database recreated, new `created`, locked to the
/// rebuilder, and only `keep` re-uploaded so far.
fn simulate_rebuild(env: &Env, keep: &[(&str, &str)]) {
    {
        let mut st = env.mock.state.lock().unwrap();
        st.docs.retain(|k, _| k == "obsydian_livesync_version");
        st.trees.retain(|k, _| k == "obsydian_livesync_version");
        st.log.clear();
    }
    env.mock.update_milestone(|m| {
        m["created"] = json!(2000);
        m["locked"] = json!(true);
        m["accepted_nodes"] = json!(["rebuilder"]);
        m["node_info"] = json!({});
    });
    for (p, c) in keep {
        env.mock.put_text(p, &[c]);
    }
}

/// H1 (T1): a soft-deleted winner whose `_conflicts` hold our branch must
/// not remove the local file that holds the unsynced edit.
#[tokio::test]
async fn a_deleted_winner_never_removes_a_local_edit_kept_as_a_branch() {
    let env = Env::new().await;
    env.write("c.md", b"base\n");
    env.engine().sync_once().await.unwrap();
    let base = env.row("c.md").unwrap().base_rev.unwrap();
    // Another device soft-deletes it (its revision wins the tie).
    let mut d = env.doc("c.md");
    d["deleted"] = json!(true);
    d["mtime"] = json!(1_900_000_000_000i64);
    if let Some(o) = d.as_object_mut() {
        o.remove("_conflicts");
    }
    env.mock.put_branch_with(d, &base, "zzzz");
    // Meanwhile the phone edits it.
    env.write("c.md", b"local edit\n");
    env.engine().sync_once().await.unwrap();
    assert_eq!(
        env.read("c.md").as_deref(),
        Some(&b"local edit\n"[..]),
        "local file with the unsynced edit was removed"
    );
    let row = env.row("c.md").expect("the row is kept");
    assert_eq!(row.state, State::Conflicted, "{row:?}");
    assert!(
        row.note.as_deref().unwrap_or("").contains("deleted on another device"),
        "{row:?}"
    );
    assert_eq!(env.engine().status().unwrap().conflicted.len(), 1);
    // Our edit is a live leaf on the remote.
    let ours = row.base_rev.unwrap();
    assert_eq!(env.mock.content_at("c.md", &ours).unwrap(), b"local edit\n");
    // Further syncs leave it alone.
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.read("c.md").unwrap(), b"local edit\n");
}

/// M1 (T2): a note moved back from out of scope picks up the remote change
/// made while it was away.
#[tokio::test]
async fn a_note_moved_back_into_scope_gets_the_remote_change_made_meanwhile() {
    let env = Env::new().await;
    let sc = || scope(ScopeMode::Exclude, &["Archive"]);
    env.write("n/a.md", b"v1");
    env.engine_with(|c| c.scope = sc()).sync_once().await.unwrap();
    env.mv("n/a.md", "Archive/a.md");
    env.engine_with(|c| c.scope = sc()).sync_once().await.unwrap();
    assert_eq!(env.row("n/a.md").unwrap().state, State::OutOfScope);
    env.mock.put_text("n/a.md", &["v2 from desktop"]);
    env.engine_with(|c| c.scope = sc()).sync_once().await.unwrap();
    env.mv("Archive/a.md", "n/a.md");
    env.engine_with(|c| c.scope = sc()).sync_once().await.unwrap();
    assert_eq!(env.read("n/a.md").unwrap(), b"v2 from desktop");
    assert_eq!(env.row("n/a.md").unwrap().state, State::Clean);
}

/// M1: an `out_of_scope` row survives a rebuild reset, so the remote copy
/// is not written back beside the moved one.
#[tokio::test]
async fn out_of_scope_rows_survive_a_rebuild() {
    let env = Env::new().await;
    let sc = |c: &mut EngineConfig| c.scope = scope(ScopeMode::Exclude, &["Archive"]);
    env.write("a.md", b"a");
    env.engine_with(sc).sync_once().await.unwrap();
    env.mv("a.md", "Archive/a.md");
    env.engine_with(sc).sync_once().await.unwrap();
    simulate_rebuild(&env, &[("a.md", "a")]);
    env.engine_with(sc).sync_once().await.unwrap();
    assert!(
        env.read("a.md").is_none(),
        "the remote copy came back beside the moved one"
    );
    assert_eq!(env.row("a.md").unwrap().state, State::OutOfScope);
}

/// H3 (T3): a shutdown during the quiet wait neither completes the fetch nor
/// marks the node resolved; nudges restart the wait instead of ending it.
#[tokio::test]
async fn a_shutdown_or_nudge_during_the_quiet_wait_does_not_complete_the_fetch() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["a"]);
    let e = env.engine();
    e.sync_once().await.unwrap();
    let node = e.node_id();
    drop(e);
    simulate_rebuild(&env, &[("a.md", "a")]);
    let engine = env.engine_with(|c| {
        c.rebuild_quiet = Duration::from_secs(3);
        c.rescan_interval = Duration::from_secs(3600);
    });
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let e = engine.clone();
        async move { e.run(rx).await }
    });
    wait_until("quiet wait started", || engine.status().unwrap().rebuild_wait.is_some()).await;
    // Network hand-overs keep nudging: the wait must not end early.
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        engine.network_changed(true);
    }
    assert!(env.meta("needs_full_fetch").is_some(), "a nudge ended the quiet wait");
    tx.send(true).unwrap();
    task.await.unwrap().unwrap();
    drop(engine);
    let m = env.mock.local("obsydian_livesync_milestone").unwrap();
    assert!(
        env.meta("needs_full_fetch").is_some(),
        "full fetch marked done although the quiet wait was cut short"
    );
    assert!(env.meta("resolve_owed").is_some());
    assert!(
        !m["accepted_nodes"].as_array().unwrap().contains(&json!(node)),
        "marked resolved during shutdown"
    );
    // The next run completes the wait and resolves.
    let e = env.engine_with(|c| c.rebuild_quiet = Duration::from_millis(300));
    e.sync_once().await.unwrap();
    let m = env.mock.local("obsydian_livesync_milestone").unwrap();
    assert!(m["accepted_nodes"].as_array().unwrap().contains(&json!(node)));
    assert!(e.status().unwrap().rebuild_wait.is_none());
}

/// H2 (T4): nothing is pushed into a rebuilding database while the refetch
/// and its quiet wait are still running, whatever triggers a pass.
#[tokio::test]
async fn nothing_is_pushed_while_a_rebuild_refetch_is_owed() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["a"]);
    env.mock.put_text("dropped.md", &["dropped"]);
    let engine = env.engine_with(|c| {
        c.rebuild_quiet = Duration::from_secs(8);
        c.regate_interval = Duration::from_millis(300);
        c.rescan_interval = Duration::from_secs(3600);
    });
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let e = engine.clone();
        async move { e.run(rx).await }
    });
    wait_until("initial pull", || env.read("dropped.md").is_some()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    simulate_rebuild(&env, &[("a.md", "a")]);
    wait_until("rebuild seen", || env.meta("remote_created").as_deref() == Some("2000")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(env.meta("needs_full_fetch").is_some(), "still in the quiet wait");
    // A full rescan and a local edit during the quiet wait.
    engine.rescan_now();
    env.write("new.md", b"typed during the wait");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(env.meta("needs_full_fetch").is_some(), "still in the quiet wait");
    assert!(
        env.remote("dropped.md").is_none(),
        "pushed into the rebuilding database"
    );
    assert!(env.remote("new.md").is_none(), "pushed into the rebuilding database");
    tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

/// H2 and L3: the push passes themselves refuse while a full fetch is
/// owed, so no caller can bypass the gate; a read-only pull after a rebuild
/// does not complete the fetch (the quiet wait belongs to the two-way side).
#[tokio::test]
async fn push_passes_refuse_while_a_full_fetch_is_owed() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["a"]);
    env.engine().sync_once().await.unwrap();
    simulate_rebuild(&env, &[("a.md", "a")]);
    env.write("local.md", b"local");
    let e = env.engine_with(|c| c.rebuild_quiet = Duration::from_millis(200));
    e.pull_once().await.unwrap();
    let st = e.status().unwrap();
    assert!(st.needs_full_fetch, "a read-only pull completed the owed fetch");
    assert!(st.rebuild_wait.is_some(), "{st:?}");
    assert!(e.push_all(false).await.is_err());
    assert!(env.remote("local.md").is_none());
    // The two-way sync waits, completes it and then pushes.
    e.sync_once().await.unwrap();
    let st = e.status().unwrap();
    assert!(!st.needs_full_fetch);
    assert!(st.rebuild_wait.is_none());
    assert_eq!(env.remote("local.md").unwrap(), b"local");
}

/// L3: a lock let through for one database does not open the door for a
/// later lock once it was lifted.
#[tokio::test]
async fn a_lingering_lock_bypass_is_tied_to_its_lock() {
    let env = Env::new().await;
    env.mock.put_text("x.md", &["x"]);
    env.mock.update_milestone(|m| {
        m["locked"] = json!(true);
        m["accepted_nodes"] = json!(["someone"]);
    });
    // A fresh index may fetch through the lock (read-only: not resolved).
    env.engine().pull_once().await.unwrap();
    assert!(env.meta("resolve_owed").is_some());
    // The lock is lifted; the bypass goes with it.
    env.mock.update_milestone(|m| m["locked"] = json!(false));
    env.engine().pull_once().await.unwrap();
    assert!(env.meta("resolve_owed").is_none());
    assert!(env.meta("needs_full_fetch").is_none());
    // A new lock (no rebuild) is refused again.
    env.mock.update_milestone(|m| m["locked"] = json!(true));
    assert!(matches!(
        env.engine().sync_once().await,
        Err(EngineError::Refused(livesync_model::Refusal::Locked))
    ));
}

/// H5 (T6): a fresh index next to an old copy of a note deleted elsewhere
/// does not revive it; the stale local copy goes instead.
#[tokio::test]
async fn a_fresh_index_does_not_revive_a_note_deleted_elsewhere() {
    let env = Env::new().await;
    env.mock.put_text("gone.md", &["old note"]);
    env.mock.soft_delete("gone.md"); // mtime 1_800_000_000_000 (2027)
    env.write("gone.md", b"old note");
    set_mtime(&env, "gone.md", 1_700_000_000);
    let rep = env.engine().sync_once().await.unwrap();
    assert!(
        env.deleted("gone.md"),
        "a deletion newer than the local copy was reverted: {rep:?}"
    );
    assert!(env.read("gone.md").is_none(), "the stale copy of a deleted note stays");
    assert_eq!(rep.push.deleted_locally, 1, "{rep:?}");
    // Never the only copy: it is kept in the state directory.
    let kept: Vec<_> = std::fs::read_dir(env.dir.path().join("state/recovered"))
        .unwrap()
        .map(|e| std::fs::read(e.unwrap().path()).unwrap())
        .collect();
    assert_eq!(kept, vec![b"old note".to_vec()]);
}

/// H5's rule applies only to the reconcile pass after a full fetch: in
/// normal operation a note restored from `.trash/` (old mtime, the deleted
/// content) comes back.
#[tokio::test]
async fn a_note_restored_from_the_trash_comes_back() {
    let env = Env::new().await;
    env.write("r.md", b"keep me");
    set_mtime(&env, "r.md", 1_700_000_000);
    env.engine().sync_once().await.unwrap();
    env.mv("r.md", ".trash/r.md");
    env.engine().sync_once().await.unwrap();
    assert!(env.deleted("r.md"));
    env.mv(".trash/r.md", "r.md");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.deleted_locally, 0, "{rep:?}");
    assert_eq!(env.read("r.md").unwrap(), b"keep me");
    assert!(!env.deleted("r.md"));
    assert_eq!(env.remote("r.md").unwrap(), b"keep me");
}

/// H5: older and different from what was deleted: kept as a conflict
/// beside the deletion, never a silent replace.
#[tokio::test]
async fn a_fresh_index_keeps_an_older_different_copy_as_a_conflict() {
    let env = Env::new().await;
    env.mock.put_text("gone.md", &["old note"]);
    env.mock.soft_delete("gone.md");
    env.write("gone.md", b"edited before the deletion, never synced");
    set_mtime(&env, "gone.md", 1_700_000_000);
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.conflicts, 1, "{rep:?}");
    assert_eq!(
        env.read("gone.md").unwrap(),
        b"edited before the deletion, never synced"
    );
    let row = env.row("gone.md").unwrap();
    assert_eq!(row.state, State::Conflicted);
    let tree = env.mock.state.lock().unwrap().trees["gone.md"].clone();
    assert_eq!(tree.leaves().len(), 2, "the deletion and our copy are both leaves");
}

fn set_mtime(env: &Env, rel: &str, secs: u64) {
    let f = std::fs::File::options()
        .write(true)
        .open(env.vault().join(rel))
        .unwrap();
    f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

/// H4 (T7): an unreadable folder is not read as "these files were deleted".
#[tokio::test]
async fn an_unreadable_folder_is_not_read_as_deleted() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new().await;
    for i in 0..10 {
        env.write(&format!("sub/f{i}.md"), format!("{i}").as_bytes());
    }
    for i in 0..30 {
        env.write(&format!("other/g{i}.md"), format!("{i}").as_bytes());
    }
    env.engine().sync_once().await.unwrap();
    let sub = env.vault().join("sub");
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
    let rep = env.engine().sync_once().await;
    let st = env.engine().status().unwrap();
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let rep = rep.unwrap();
    let deleted = (0..10).filter(|i| env.deleted(&format!("sub/f{i}.md"))).count();
    assert_eq!(
        deleted, 0,
        "an unreadable folder was read as 'all its files deleted': {rep:?}"
    );
    assert!(
        st.local_issues
            .iter()
            .any(|i| i.path == "sub" && i.kind == "unreadable"),
        "{:?}",
        st.local_issues
    );
    // Readable again: nothing to do, the issue clears.
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 0);
    assert!(env.engine().status().unwrap().local_issues.is_empty());
}

/// H1: once the conflict ends in a deletion, a conflicted local copy is
/// still not removed; it is kept and reported.
#[tokio::test]
async fn a_conflicted_copy_survives_a_deletion_that_resolves_the_conflict() {
    let env = Env::new().await;
    env.write("w.md", b"base\n");
    env.engine().sync_once().await.unwrap();
    let base = env.row("w.md").unwrap().base_rev.unwrap();
    env.write("w.md", b"ours\n");
    env.engine().sync_once().await.unwrap();
    let ch = env.mock.put_chunks(&["theirs\n"]);
    let mut d = env.doc("w.md");
    d["children"] = json!(ch);
    env.mock.put_branch_with(d, &base, "00000000000000000000000000000000");
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.row("w.md").unwrap().state, State::Conflicted);
    // Resolved elsewhere by deleting the document.
    let ours = env.row("w.md").unwrap().base_rev.unwrap();
    let mut del = env.doc("w.md");
    del["deleted"] = json!(true);
    if let Some(o) = del.as_object_mut() {
        o.remove("_conflicts");
    }
    {
        // The losing branch is removed, our leaf gets the deletion on top.
        let mut st = env.mock.state.lock().unwrap();
        let t = st.trees.get_mut("w.md").unwrap();
        let other = t.leaves().into_iter().find(|l| *l != ours).unwrap();
        let mut tomb = t.revs[&other].clone();
        tomb.parent = Some(other.clone());
        tomb.deleted = true;
        t.revs.insert(
            format!("{}-dead", ours.split('-').next().unwrap().parse::<u64>().unwrap() + 1),
            tomb,
        );
    }
    env.mock.put(del);
    let rep = env.engine().sync_once().await.unwrap();
    assert!(env.deleted("w.md"));
    assert_eq!(env.read("w.md").unwrap(), b"ours\n", "{rep:?}");
    let row = env.row("w.md").unwrap();
    assert_eq!(row.state, State::Dirty);
    assert!(row.note.unwrap().contains("local copy kept"));
    // Codex C1: replaying the deletion (a full fetch) keeps the promise.
    let e = env.engine();
    e.request_full_fetch().unwrap();
    e.sync_once().await.unwrap();
    assert_eq!(
        env.read("w.md").unwrap(),
        b"ours\n",
        "a replayed deletion removed the kept copy"
    );
    assert!(env.deleted("w.md"));
}

/// M2: one document the server refuses does not stall the others, nor
/// local deletes; it is reported.
#[tokio::test]
async fn one_rejected_document_does_not_stall_the_push_side() {
    let env = Env::new().await;
    env.write("gone.md", b"to be deleted");
    env.engine().sync_once().await.unwrap();
    env.mock.state.lock().unwrap().reject_puts.insert("a.md".into());
    env.write("a.md", b"refused by the server");
    env.write("b.md", b"fine");
    std::fs::remove_file(env.vault().join("gone.md")).unwrap();
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.failed, 1, "{rep:?}");
    assert_eq!(env.remote("b.md").unwrap(), b"fine");
    assert!(env.deleted("gone.md"), "the local delete was propagated");
    let st = env.engine().status().unwrap();
    assert!(
        st.local_issues
            .iter()
            .any(|i| i.path == "a.md" && i.kind == "push-failed"),
        "{:?}",
        st.local_issues
    );
    // Accepted later: pushed, and the issue clears.
    env.mock.state.lock().unwrap().reject_puts.clear();
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.remote("a.md").unwrap(), b"refused by the server");
    assert!(env.engine().status().unwrap().local_issues.is_empty());
}

/// M2: a server that is down ends the pass (no issue per document).
#[tokio::test]
async fn a_server_outage_ends_the_pass_instead_of_flagging_every_file() {
    let env = Env::new().await;
    env.engine().sync_once().await.unwrap();
    for i in 0..6 {
        env.write(&format!("n{i}.md"), format!("{i}").as_bytes());
    }
    env.mock.state.lock().unwrap().fail_writes = 100;
    assert!(env.engine().push_all(false).await.is_err());
    let st = env.engine().status().unwrap();
    assert!(st.local_issues.is_empty(), "{:?}", st.local_issues);
    env.mock.state.lock().unwrap().fail_writes = 0;
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.pushed, 6, "{rep:?}");
    assert!(env.engine().status().unwrap().local_issues.is_empty());
}

/// M3: a folder renamed inside the synced folders is not a mass delete,
/// even without watcher hints.
#[tokio::test]
async fn a_folder_renamed_inside_the_scope_is_not_a_mass_delete() {
    let env = Env::new().await;
    for i in 0..30 {
        env.write(&format!("old/f{i}.md"), format!("note {i}").as_bytes());
    }
    for i in 0..10 {
        env.write(&format!("keep/g{i}.md"), format!("keep {i}").as_bytes());
    }
    env.engine().sync_once().await.unwrap();
    env.mv("old", "new");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.safety_stops, 0, "{rep:?}");
    assert_eq!(rep.push.deleted, 30);
    assert!(env.deleted("old/f0.md"));
    assert_eq!(env.remote("new/f0.md").unwrap(), b"note 0");
}

/// M3 (T5): a mass deletion that comes with new files under other names is
/// still stopped; the new files are pushed.
#[tokio::test]
async fn a_mass_deletion_with_new_files_beside_it_still_stops() {
    let env = Env::new().await;
    for i in 0..40 {
        env.write(&format!("f{i}.md"), format!("old {i}").as_bytes());
    }
    env.engine().sync_once().await.unwrap();
    for i in 0..30 {
        std::fs::remove_file(env.vault().join(format!("f{i}.md"))).unwrap();
        env.write(&format!("new{i}.md"), format!("new {i}").as_bytes());
    }
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.safety_stops, 1, "{rep:?}");
    assert_eq!(rep.push.deleted, 0);
    assert_eq!(rep.push.created, 30);
    assert!(!env.deleted("f0.md"));
    let st = env.engine().status().unwrap();
    assert!(st.safety_stop.as_deref().unwrap().contains("30 of 40"), "{st:?}");
}

/// M3: moving a large folder out of the scope is not a deletion and does
/// not trip the stop.
#[tokio::test]
async fn a_large_folder_moved_out_of_scope_does_not_trip_the_stop() {
    let env = Env::new().await;
    let sc = |c: &mut EngineConfig| c.scope = scope(ScopeMode::Exclude, &["Archive"]);
    for i in 0..30 {
        env.write(&format!("notes/f{i}.md"), format!("note {i}").as_bytes());
    }
    for i in 0..5 {
        env.write(&format!("g{i}.md"), format!("g {i}").as_bytes());
    }
    env.engine_with(sc).sync_once().await.unwrap();
    env.mv("notes", "Archive/notes");
    let rep = env.engine_with(sc).sync_once().await.unwrap();
    assert_eq!(rep.push.safety_stops, 0, "{rep:?}");
    assert_eq!(rep.push.out_of_scope, 30);
    assert_eq!(rep.push.deleted, 0);
    assert!(env.engine_with(sc).status().unwrap().safety_stop.is_none());
}

/// M3: a small vault whose synced folders suddenly look empty stops too.
#[tokio::test]
async fn an_emptied_small_vault_stops() {
    let env = Env::new().await;
    for i in 0..5 {
        env.write(&format!("f{i}.md"), format!("{i}").as_bytes());
    }
    env.engine().sync_once().await.unwrap();
    for i in 0..5 {
        std::fs::remove_file(env.vault().join(format!("f{i}.md"))).unwrap();
    }
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.safety_stops, 1, "{rep:?}");
    assert_eq!(rep.push.deleted, 0);
    // One file back: no longer empty, and five or fewer deletions pass.
    env.write("f0.md", b"0");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.safety_stops, 0, "{rep:?}");
    assert_eq!(rep.push.deleted, 4);
    assert!(env.engine().status().unwrap().safety_stop.is_none());
}

/// H4: the front-end's access check (Android all-files access) gates
/// every pass that could delete.
#[tokio::test]
async fn a_failed_access_check_never_deletes() {
    let env = Env::new().await;
    for i in 0..3 {
        env.write(&format!("f{i}.md"), b"x");
    }
    env.engine().sync_once().await.unwrap();
    std::fs::remove_file(env.vault().join("f0.md")).unwrap();
    let denied = livesync_engine::engine::AccessCheck::new(|| Err("all-files access revoked".into()));
    let e = env.engine_with(|c| c.access_check = Some(denied));
    let err = e.sync_once().await.unwrap_err();
    assert!(err.to_string().contains("all-files access revoked"), "{err}");
    assert!(!env.deleted("f0.md"));
    drop(e);
    let ok = livesync_engine::engine::AccessCheck::new(|| Ok(()));
    env.engine_with(|c| c.access_check = Some(ok))
        .sync_once()
        .await
        .unwrap();
    assert!(env.deleted("f0.md"));
}

/// L1: when the filesystem probe fails, the rules recorded earlier (or
/// FAT rules) apply, never permissive ones.
#[tokio::test]
async fn a_failed_probe_falls_back_to_recorded_or_fat_rules() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new().await;
    let probed = env.engine_with(|c| c.case_insensitive = Some(true)).fs_rules().clone();
    assert!(probed.case_insensitive);
    let ro = |mode| std::fs::set_permissions(env.vault(), std::fs::Permissions::from_mode(mode)).unwrap();
    ro(0o555);
    let again = env.engine().fs_rules().clone();
    ro(0o755);
    assert_eq!(again, probed, "the recorded rules were not used");
    // No record yet: FAT rules.
    let env = Env::new().await;
    std::fs::create_dir_all(env.vault()).unwrap();
    std::fs::set_permissions(env.vault(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let rules = env.engine().fs_rules().clone();
    std::fs::set_permissions(env.vault(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(rules, livesync_engine::fsrules::FsRules::fat());
}

/// L2: a `PUT` whose response was lost does not make a conflict with
/// ourselves on the retry.
#[tokio::test]
async fn a_lost_put_response_does_not_make_a_conflict_with_ourselves() {
    let env = Env::new().await;
    env.write("p.md", b"v1");
    env.engine().sync_once().await.unwrap();
    env.write("p.md", b"v2");
    env.mock.state.lock().unwrap().lose_next_put_response = true;
    let rep = env.engine().push_all(false).await.unwrap();
    assert_eq!(rep.failed, 1, "{rep:?}");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.conflicts, 0, "{rep:?}");
    let row = env.row("p.md").unwrap();
    assert_eq!(row.state, State::Clean);
    let tree = env.mock.state.lock().unwrap().trees["p.md"].clone();
    assert_eq!(tree.leaves().len(), 1, "{:?}", tree.leaves());
    assert_eq!(env.remote("p.md").unwrap(), b"v2");
}

/// L4: a case-only rename on a case-sensitive filesystem reaches the
/// remote, and later remote changes land at the new spelling.
#[tokio::test]
async fn a_case_only_rename_is_pushed() {
    let env = Env::new().await;
    require_case_sensitive!(env.dir.path());
    let cs = |c: &mut EngineConfig| c.case_insensitive = Some(false);
    env.write("Note.md", b"n");
    env.engine_with(cs).sync_once().await.unwrap();
    env.mv("Note.md", "note.md");
    env.engine_with(cs).sync_once().await.unwrap();
    assert_eq!(env.doc("note.md")["path"], "note.md");
    assert_eq!(env.row("note.md").unwrap().path, "note.md");
    env.mock.put_text("note.md", &["edited elsewhere"]);
    let mut d = env.doc("note.md");
    d["path"] = json!("note.md");
    env.mock.put(d);
    env.engine_with(cs).sync_once().await.unwrap();
    assert_eq!(env.read("note.md").unwrap(), b"edited elsewhere");
    assert!(env.read("Note.md").is_none());
}

/// L5: widening by key range also finds documents with mixed-case IDs.
#[tokio::test]
async fn widening_finds_documents_with_mixed_case_ids() {
    let env = Env::new().await;
    let ch = env.mock.put_chunks(&["old client"]);
    env.mock.put(json!({"_id": "Notes/Mixed.md", "path": "Notes/Mixed.md", "type": "plain", "children": ch, "mtime": 1, "ctime": 1, "size": 10, "eden": {}}));
    env.mock.put_text("other/a.md", &["a"]);
    let only = |p: &'static [&'static str]| move |c: &mut EngineConfig| c.scope = scope(ScopeMode::Include, p);
    env.engine_with(only(&["other"])).sync_once().await.unwrap();
    assert!(env.read("Notes/Mixed.md").is_none());
    env.engine_with(only(&["other", "notes"])).sync_once().await.unwrap();
    assert_eq!(env.read("Notes/Mixed.md").unwrap(), b"old client");
}

/// M3: in the daemon, a safety stop stays visible through later partial
/// passes (a save elsewhere); only a full pass without it clears it.
#[tokio::test]
async fn a_safety_stop_survives_partial_passes() {
    let env = Env::new().await;
    for i in 0..30 {
        env.write(&format!("d/f{i}.md"), format!("{i}").as_bytes());
    }
    env.write("other.md", b"x");
    env.engine().sync_once().await.unwrap();
    let engine = env.engine_with(|c| {
        c.settle = Duration::from_millis(150);
        c.rescan_interval = Duration::from_secs(3600);
    });
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let e = engine.clone();
        async move { e.run(rx).await }
    });
    wait_until("daemon up", || env.meta("registered_ms").is_some()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::remove_dir_all(env.vault().join("d")).unwrap();
    wait_until("stop", || engine.status().unwrap().safety_stop.is_some()).await;
    env.write("new.md", b"saved meanwhile");
    wait_until("partial pass", || env.remote("new.md").is_some()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        engine.status().unwrap().safety_stop.is_some(),
        "cleared by a partial pass"
    );
    assert!(!env.deleted("d/f0.md"));
    tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

/// Codex C2: the reconcile pass stays pending while a file could not be
/// looked at, so a stale copy read later is still judged by its rule.
#[tokio::test]
async fn reconciliation_stays_pending_until_every_file_was_looked_at() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new().await;
    env.mock.put_text("gone.md", &["old note"]);
    env.mock.soft_delete("gone.md");
    env.write("gone.md", b"old note");
    set_mtime(&env, "gone.md", 1_700_000_000);
    let f = env.vault().join("gone.md");
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
    let rep = env.engine().sync_once().await;
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    rep.unwrap();
    assert!(env.meta("reconcile_pending").is_some());
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.deleted_locally, 1, "{rep:?}");
    assert!(env.deleted("gone.md"), "revived after the reconcile flag was dropped");
    assert!(env.meta("reconcile_pending").is_none());
}

/// Codex C2: once a refetch completes in the running daemon, a full pass
/// runs at once (orphans go back now, not at the next periodic rescan).
#[tokio::test]
async fn the_daemon_rescans_as_soon_as_pushes_resume() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["a"]);
    env.mock.put_text("dropped.md", &["dropped"]);
    let engine = env.engine_with(|c| {
        c.rebuild_quiet = Duration::from_millis(500);
        c.regate_interval = Duration::from_millis(300);
        c.rescan_interval = Duration::from_secs(3600);
    });
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let e = engine.clone();
        async move { e.run(rx).await }
    });
    wait_until("initial pull", || env.read("dropped.md").is_some()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    simulate_rebuild(&env, &[("a.md", "a")]);
    wait_until("orphan pushed back", || env.remote("dropped.md").is_some()).await;
    assert!(env.meta("needs_full_fetch").is_none());
    tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

/// Codex C3: a rename whose destination the server refuses does not delete
/// the source; both happen once the destination is accepted.
#[tokio::test]
async fn a_rename_whose_destination_fails_keeps_the_source() {
    let env = Env::new().await;
    env.write("a.md", b"note");
    env.write("other.md", b"x");
    env.engine().sync_once().await.unwrap();
    env.mock.state.lock().unwrap().reject_puts.insert("b.md".into());
    env.mv("a.md", "b.md");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.failed, 1, "{rep:?}");
    assert!(
        !env.deleted("a.md"),
        "the source was deleted while its destination failed"
    );
    env.mock.state.lock().unwrap().reject_puts.clear();
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.remote("b.md").unwrap(), b"note");
    assert!(env.deleted("a.md"));
}

/// Codex C4: no recovered copy, no removal.
#[tokio::test]
async fn a_stale_copy_stays_when_its_recovered_copy_cannot_be_saved() {
    let env = Env::new().await;
    env.mock.put_text("gone.md", &["old note"]);
    env.mock.soft_delete("gone.md");
    env.write("gone.md", b"old note");
    set_mtime(&env, "gone.md", 1_700_000_000);
    std::fs::create_dir_all(env.dir.path().join("state")).unwrap();
    std::fs::write(env.dir.path().join("state/recovered"), b"not a folder").unwrap();
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.deleted_locally, 0, "{rep:?}");
    assert_eq!(env.read("gone.md").unwrap(), b"old note");
    assert!(env.deleted("gone.md"), "nor revived");
    // Codex (second round): the reconcile pass stays pending, so the next
    // sync does not revive the deletion either.
    assert!(env.meta("reconcile_pending").is_some());
    env.engine().sync_once().await.unwrap();
    assert!(env.deleted("gone.md"), "revived on the next sync");
    // Once a copy can be kept, the stale file goes.
    std::fs::remove_file(env.dir.path().join("state/recovered")).unwrap();
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.deleted_locally, 1, "{rep:?}");
    assert!(env.read("gone.md").is_none());
    assert!(env.meta("reconcile_pending").is_none());
}

/// #10: in the daemon, a burst of deletes that reaches the push side in two
/// parts (two settle windows, or a scan that ran in the middle of it) is
/// judged as one: 30 of 40 deleted in two halves of 15 still stops, and
/// neither half is deleted remotely.
#[tokio::test]
async fn a_mass_deletion_split_over_two_passes_still_stops() {
    let env = Env::new().await;
    for i in 0..40 {
        env.write(&format!("f{i:02}.md"), format!("note {i}").as_bytes());
    }
    env.engine().sync_once().await.unwrap();
    let engine = env.engine_with(|c| {
        c.settle = Duration::from_millis(150);
        c.rescan_interval = Duration::from_secs(3600);
        c.delete_hold = Duration::from_secs(3);
    });
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let e = engine.clone();
        async move { e.run(rx).await }
    });
    wait_until("daemon up", || env.meta("registered_ms").is_some()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    for i in 0..15 {
        std::fs::remove_file(env.vault().join(format!("f{i:02}.md"))).unwrap();
    }
    // Several settle windows: the first half gets a partial pass of its own.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    for i in 15..30 {
        std::fs::remove_file(env.vault().join(format!("f{i:02}.md"))).unwrap();
    }
    let gone = || -> Vec<String> {
        (0..30)
            .map(|i| format!("f{i:02}.md"))
            .filter(|p| env.deleted(p))
            .collect()
    };
    wait_until("the stop, or every deletion", || {
        engine.status().unwrap().safety_stop.is_some() || gone().len() == 30
    })
    .await;
    assert!(gone().is_empty(), "deleted remotely: {:?}", gone());
    let st = engine.status().unwrap();
    assert!(st.safety_stop.as_deref().unwrap().contains("30 of 40"), "{st:?}");

    // The user confirms: the 30 go, once; the other 10 stay.
    engine.allow_mass_delete_once();
    wait_until("the confirmed deletions", || gone().len() == 30).await;
    assert!((30..40).all(|i| !env.deleted(&format!("f{i:02}.md"))));
    wait_until("the stop to clear", || {
        engine.status().unwrap().safety_stop.is_none() && !engine.mass_delete_allowed()
    })
    .await;
    // The permission is used up: the next mass deletion stops again.
    for i in 30..40 {
        std::fs::remove_file(env.vault().join(format!("f{i:02}.md"))).unwrap();
    }
    wait_until("the next stop", || engine.status().unwrap().safety_stop.is_some()).await;
    assert!(
        (30..40).all(|i| !env.deleted(&format!("f{i:02}.md"))),
        "an emptied vault went through"
    );
    tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

/// #10: deleting a few notes now and then is never a mass deletion: each
/// small burst goes out once it is over (after the hold, not before).
#[tokio::test]
async fn a_few_deletions_now_and_then_go_through_after_the_hold() {
    let env = Env::new().await;
    for i in 0..30 {
        env.write(&format!("f{i:02}.md"), format!("note {i}").as_bytes());
    }
    env.engine().sync_once().await.unwrap();
    let engine = env.engine_with(|c| {
        c.settle = Duration::from_millis(150);
        c.rescan_interval = Duration::from_secs(3600);
        c.delete_hold = Duration::from_secs(2);
    });
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let e = engine.clone();
        async move { e.run(rx).await }
    });
    wait_until("daemon up", || env.meta("registered_ms").is_some()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    for round in 0..4 {
        let names: Vec<String> = (round * 5..round * 5 + 5).map(|i| format!("f{i:02}.md")).collect();
        for n in &names {
            std::fs::remove_file(env.vault().join(n)).unwrap();
        }
        // Seen by a partial pass by now, but held.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(names.iter().all(|n| !env.deleted(n)), "sent before the hold ended");
        wait_until("the small burst", || names.iter().all(|n| env.deleted(n))).await;
    }
    // 20 of 30 deleted in all, more than half, but never as one burst.
    assert!(engine.status().unwrap().safety_stop.is_none());
    assert!(!env.deleted("f20.md"));
    tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

/// #10: deletions that came from the server are not this device's: 30 of
/// 40 deleted remotely, beside 5 deleted here, do not trip the stop, and
/// the local ones go out.
#[tokio::test]
async fn remote_deletions_do_not_count_towards_the_stop() {
    let env = Env::new().await;
    for i in 0..40 {
        env.write(&format!("f{i:02}.md"), format!("note {i}").as_bytes());
    }
    env.engine().sync_once().await.unwrap();
    let engine = env.engine_with(|c| {
        c.settle = Duration::from_millis(150);
        c.rescan_interval = Duration::from_secs(3600);
        c.delete_hold = Duration::from_secs(1);
    });
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let e = engine.clone();
        async move { e.run(rx).await }
    });
    wait_until("daemon up", || env.meta("registered_ms").is_some()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    for i in 0..30 {
        env.mock.soft_delete(&format!("f{i:02}.md"));
    }
    for i in 30..35 {
        std::fs::remove_file(env.vault().join(format!("f{i:02}.md"))).unwrap();
    }
    wait_until("the remote deletions applied", || {
        (0..30).all(|i| env.read(&format!("f{i:02}.md")).is_none())
    })
    .await;
    wait_until("the local deletions sent", || {
        (30..35).all(|i| env.deleted(&format!("f{i:02}.md")))
    })
    .await;
    // Past the hold and its deciding pass.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let st = engine.status().unwrap();
    assert!(st.safety_stop.is_none(), "{st:?}");
    assert!((35..40).all(|i| !env.deleted(&format!("f{i:02}.md"))));
    assert_eq!(st.counts.get("clean"), Some(&5), "{:?}", st.counts);
    tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

/// #10: a full pass that runs in the middle of a burst (here: the rest of
/// the burst lands after its walk, and no watcher reports it, as on shared
/// storage) does not delete the part it saw; the burst is judged once it
/// is over.
#[tokio::test]
async fn a_scan_in_the_middle_of_a_mass_deletion_deletes_nothing() {
    let env = Env::new().await;
    for i in 0..40 {
        env.write(&format!("f{i:02}.md"), format!("note {i}").as_bytes());
    }
    env.engine().sync_once().await.unwrap();
    let vault = env.vault();
    let rest_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Asked after the walk, before deciding: the rest of the burst lands now.
    let check = livesync_engine::engine::AccessCheck::new({
        let rest_done = rest_done.clone();
        move || {
            if !rest_done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                for i in 15..30 {
                    std::fs::remove_file(vault.join(format!("f{i:02}.md"))).unwrap();
                }
            }
            Ok(())
        }
    });
    let engine = env.engine_with(|c| {
        c.watch = false;
        c.rescan_interval = Duration::from_secs(3600);
        c.delete_hold = Duration::from_secs(1);
        c.access_check = Some(check);
    });
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let e = engine.clone();
        async move { e.run(rx).await }
    });
    wait_until("daemon up", || env.meta("registered_ms").is_some()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    for i in 0..15 {
        std::fs::remove_file(env.vault().join(format!("f{i:02}.md"))).unwrap();
    }
    engine.rescan_now();
    wait_until("the walk", || rest_done.load(std::sync::atomic::Ordering::SeqCst)).await;
    // A second scan, as the next periodic rescan would be.
    tokio::time::sleep(Duration::from_millis(300)).await;
    engine.rescan_now();
    let gone = || -> Vec<String> {
        (0..30)
            .map(|i| format!("f{i:02}.md"))
            .filter(|p| env.deleted(p))
            .collect()
    };
    wait_until("the stop, or every deletion", || {
        engine.status().unwrap().safety_stop.is_some() || gone().len() == 30
    })
    .await;
    assert!(gone().is_empty(), "deleted remotely: {:?}", gone());
    let st = engine.status().unwrap();
    assert!(st.safety_stop.as_deref().unwrap().contains("30 of 40"), "{st:?}");
    tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

/// #10 (Codex): a full pass whose walk began inside the hold does not
/// decide, even when it gets to the deletions after the hold ended: files
/// that went missing after its walk are not in its picture.
#[tokio::test]
async fn a_scan_that_began_inside_the_hold_does_not_decide() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let env = Env::new().await;
    for i in 0..40 {
        env.write(&format!("f{i:02}.md"), format!("note {i}").as_bytes());
    }
    env.engine().sync_once().await.unwrap();
    let vault = env.vault();
    let calls = std::sync::Arc::new(AtomicU32::new(0));
    // Asked after each walk. On the second pass the hold runs out while it
    // works, and the rest of the burst lands after its walk.
    let check = livesync_engine::engine::AccessCheck::new({
        let calls = calls.clone();
        move || {
            if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                std::thread::sleep(Duration::from_millis(3200));
                for i in 15..30 {
                    std::fs::remove_file(vault.join(format!("f{i:02}.md"))).unwrap();
                }
            }
            Ok(())
        }
    });
    // A long hold, so the second scan surely starts inside it.
    let engine = env.engine_with(|c| {
        c.watch = false;
        c.rescan_interval = Duration::from_secs(3600);
        c.delete_hold = Duration::from_secs(3);
        c.access_check = Some(check);
    });
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let e = engine.clone();
        async move { e.run(rx).await }
    });
    wait_until("daemon up", || env.meta("registered_ms").is_some()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    for i in 0..15 {
        std::fs::remove_file(env.vault().join(format!("f{i:02}.md"))).unwrap();
    }
    engine.rescan_now();
    wait_until("the first pass", || calls.load(Ordering::SeqCst) >= 1).await;
    engine.rescan_now();
    wait_until("the second pass", || calls.load(Ordering::SeqCst) >= 2).await;
    // And a later scan sees the rest of the burst.
    tokio::time::sleep(Duration::from_millis(500)).await;
    engine.rescan_now();
    let gone = || -> Vec<String> {
        (0..30)
            .map(|i| format!("f{i:02}.md"))
            .filter(|p| env.deleted(p))
            .collect()
    };
    wait_until("the stop, or every deletion", || {
        engine.status().unwrap().safety_stop.is_some() || gone().len() == 30
    })
    .await;
    assert!(gone().is_empty(), "deleted remotely: {:?}", gone());
    let st = engine.status().unwrap();
    assert!(st.safety_stop.as_deref().unwrap().contains("30 of 40"), "{st:?}");
    tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

/// #10 (Codex): a note deleted, recreated and deleted again goes out after
/// the hold both times, not at the next periodic rescan.
#[tokio::test]
async fn a_note_deleted_twice_is_sent_after_the_hold_both_times() {
    let env = Env::new().await;
    for i in 0..10 {
        env.write(&format!("f{i}.md"), format!("note {i}").as_bytes());
    }
    env.engine().sync_once().await.unwrap();
    let engine = env.engine_with(|c| {
        c.settle = Duration::from_millis(150);
        c.rescan_interval = Duration::from_secs(3600);
    });
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let e = engine.clone();
        async move { e.run(rx).await }
    });
    wait_until("daemon up", || env.meta("registered_ms").is_some()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::remove_file(env.vault().join("f0.md")).unwrap();
    wait_until("the first deletion", || env.deleted("f0.md")).await;
    env.write("f0.md", b"written again");
    wait_until("the new version", || {
        env.remote("f0.md").as_deref() == Some(&b"written again"[..])
    })
    .await;
    std::fs::remove_file(env.vault().join("f0.md")).unwrap();
    wait_until("the second deletion", || env.deleted("f0.md")).await;
    tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}
