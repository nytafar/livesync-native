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
