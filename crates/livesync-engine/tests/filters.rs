//! What syncs (spec D11) against the mock CouchDB: ignore files in both
//! directions, paths that become ignored or stop being ignored, unreadable
//! ignore files, `syncOnlyRegEx`, the dot-path allowlist, and remote
//! deletions into the trash.

mod common;

use common::{Mock, DB};
use livesync_engine::vaultfs::TrashMode;
use livesync_engine::{Engine, EngineConfig, Index, State};
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
        cfg.settle = Duration::from_millis(150);
        cfg.delete_hold = Duration::from_millis(300);
        cfg.use_ignore_files = true;
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
    fn row(&self, path: &str) -> Option<livesync_engine::FileRow> {
        Index::open(&self.index_path())
            .unwrap()
            .by_doc(&path_to_id(path, true))
            .unwrap()
    }
    fn remote(&self, path: &str) -> Option<Vec<u8>> {
        self.mock.content(&path_to_id(path, true))
    }
    fn deleted(&self, path: &str) -> bool {
        self.mock
            .doc(&path_to_id(path, true))
            .is_some_and(|d| d.get("deleted") == Some(&json!(true)))
    }
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

/// The fixture of #12's done line: nested ignore files, a negation, a
/// `dir/` rule, a nested file that accepts again inside an ignored folder,
/// a whole folder ignored by its own ignore file, and git-style patterns
/// that upstream does not match at depth.
fn nested_fixture(env: &Env) {
    env.write(".gitignore", b"*.log\nbuild/\n!important.log\nprivate.md\n");
    env.write(
        "notes/.gitignore",
        b"drafts/\n*.tmp\n!keep.tmp\nsecret\n!secret/public.md\n",
    );
    env.write("notes/drafts/.gitignore", b"!*.md\n");
    env.write("code/.gitignore", b"node_modules/\n");
    env.write("code/.venv/.gitignore", b"*\n");
    env.write("code/venv/.gitignore", b"*\n");
    for p in [
        "a.md",
        "a.log",
        "important.log",
        "private.md",
        "sub/private.md",
        "sub/a.log",
        "build/out.md",
        "sub/build/out.md",
        "notes/n.md",
        "notes/x.tmp",
        "notes/keep.tmp",
        "notes/sub/x.tmp",
        "notes/drafts/d.md",
        "notes/drafts/d.txt",
        "notes/secret/x.md",
        "notes/secret/public.md",
        "code/main.py",
        "code/node_modules/pkg/readme.md",
        "code/.venv/lib/x.py",
        "code/venv/lib/x.py",
    ] {
        env.write(p, p.as_bytes());
    }
}

/// What commonlib's `isAcceptedAll` says for the fixture (generated from
/// it; see `fixtures/golden/ignore_all.json` for the same rules), minus
/// dot-paths, which never sync.
const SYNCED: [&str; 11] = [
    "a.md",
    "important.log",
    "sub/private.md",
    "sub/a.log",
    "sub/build/out.md",
    "notes/n.md",
    "notes/keep.tmp",
    "notes/sub/x.tmp",
    "notes/drafts/d.md",
    "notes/secret/public.md",
    "code/main.py",
];

#[tokio::test]
async fn a_vault_with_nested_ignore_files_pushes_what_the_plugin_would() {
    let env = Env::new().await;
    nested_fixture(&env);
    env.engine().sync_once().await.unwrap();
    let mut want: Vec<String> = SYNCED.iter().map(|s| s.to_string()).collect();
    want.sort();
    want.dedup();
    assert_eq!(env.mock.live_paths(), want);
    // Ignore files are dot-files: never pushed.
    assert!(env.mock.doc(&path_to_id(".gitignore", true)).is_none());
    // A second pass changes nothing.
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.pushed, 0, "{rep:?}");
    // And `check-ignore` style verdicts agree.
    let f = env.engine().explain("notes/x.tmp").detail;
    assert!(f.contains("notes/.gitignore:2:*.tmp"), "{f}");
    let f = env.engine().explain("code/node_modules/pkg/readme.md").detail;
    assert!(f.contains("code/.gitignore:1:node_modules/"), "{f}");
}

#[tokio::test]
async fn remote_documents_at_ignored_paths_are_not_written() {
    let env = Env::new().await;
    env.write(".gitignore", b"*.log\nbuild/\n");
    for p in ["a.md", "a.log", "build/x.md", "sub/a.log"] {
        env.mock.put_text(p, &[p]);
    }
    let rep = env.engine().pull_once().await.unwrap();
    assert_eq!(rep.skipped.get("ignore-file"), Some(&2), "{rep:?}");
    assert!(env.read("a.md").is_some());
    assert!(env.read("sub/a.log").is_some(), "only beside the ignore file");
    assert!(env.read("a.log").is_none());
    assert!(env.read("build/x.md").is_none());
    assert!(env.row("a.log").is_none(), "no row for an ignored path");
    // A remote deletion of an ignored path never touches a local file there.
    env.write("a.log", b"local only");
    env.mock.soft_delete("a.log");
    env.engine().pull_once().await.unwrap();
    assert_eq!(env.read("a.log").unwrap(), b"local only");
}

#[tokio::test]
async fn a_path_that_becomes_ignored_is_untracked_never_deleted_remotely() {
    let env = Env::new().await;
    env.write("keep.md", b"k");
    env.write("logs/today.log", b"t");
    env.write("logs/old.log", b"o");
    env.engine().sync_once().await.unwrap();
    assert!(env.remote("logs/today.log").is_some());
    env.write("logs/.gitignore", b"*.log\n");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 0);
    assert!(env.row("logs/today.log").is_none(), "untracked");
    assert!(!env.deleted("logs/today.log"));
    // Deleting the now-ignored file locally deletes nothing remotely, and an
    // edit to it is not pushed.
    std::fs::remove_file(env.vault().join("logs/today.log")).unwrap();
    env.write("logs/old.log", b"edited");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.deleted + rep.push.pushed, 0, "{rep:?}");
    assert!(!env.deleted("logs/today.log"));
    assert_eq!(env.remote("logs/old.log").unwrap(), b"o");
    // A remote edit to it is not written either.
    env.mock.put_text("logs/old.log", &["remote edit"]);
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.read("logs/old.log").unwrap(), b"edited");
}

#[tokio::test]
async fn a_path_that_stops_being_ignored_is_caught_up() {
    let env = Env::new().await;
    env.write(".gitignore", b"archive/\n");
    env.mock.put_text("archive/old.md", &["from elsewhere"]);
    env.mock.put_text("a.md", &["a"]);
    env.engine().sync_once().await.unwrap();
    assert!(env.read("archive/old.md").is_none());
    // The rule goes: the remote document is fetched without a remote change.
    env.write(".gitignore", b"# nothing ignored now\n");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(env.read("archive/old.md").unwrap(), b"from elsewhere");
    assert_eq!(env.row("archive/old.md").unwrap().state, State::Clean);
    assert_eq!(rep.push.pushed, 0, "nothing to push back: {rep:?}");
    // Turning ignore files off altogether also catches up.
    env.write(".gitignore", b"*.txt\n");
    env.mock.put_text("t.txt", &["text"]);
    env.engine().sync_once().await.unwrap();
    assert!(env.read("t.txt").is_none());
    env.engine_with(|c| c.use_ignore_files = false)
        .sync_once()
        .await
        .unwrap();
    assert_eq!(env.read("t.txt").unwrap(), b"text");
}

#[tokio::test]
async fn an_unreadable_ignore_file_stops_its_folder_both_ways() {
    let env = Env::new().await;
    env.write("notes/a.md", b"a");
    env.write("notes/deep/b.md", b"b");
    env.write("other.md", b"o");
    env.engine().sync_once().await.unwrap();
    // A pattern lsync does not evaluate: the folder is undecidable.
    env.write("notes/.gitignore", b"+(x|y).md\n");
    std::fs::remove_file(env.vault().join("notes/a.md")).unwrap();
    env.write("notes/new.md", b"new");
    env.write("other2.md", b"o2");
    env.mock.put_text("notes/remote.md", &["r"]);
    let rep = env.engine().sync_once().await.unwrap();
    assert!(!env.deleted("notes/a.md"), "not treated as deleted");
    assert!(env.row("notes/a.md").is_some(), "still tracked");
    assert!(env.remote("notes/new.md").is_none(), "not pushed");
    assert!(env.read("notes/remote.md").is_none(), "not written");
    assert!(env.remote("other2.md").is_some(), "other folders go on");
    assert!(rep.pull.pending >= 1, "{rep:?}");
    let st = env.engine().status().unwrap();
    assert!(
        st.local_issues
            .iter()
            .any(|i| i.path == "notes" && i.note.contains("notes/.gitignore")),
        "{:?}",
        st.local_issues
    );
    // Fixed: everything resumes.
    env.write("notes/.gitignore", b"*.tmp\n");
    env.engine().sync_once().await.unwrap();
    assert!(env.remote("notes/new.md").is_some());
    assert_eq!(env.read("notes/remote.md").unwrap(), b"r");
}

#[tokio::test]
async fn sync_only_regex_limits_both_ways() {
    let env = Env::new().await;
    env.write("notes/a.md", b"a");
    env.write("img/p.png", b"p");
    env.mock.put_text("notes/r.md", &["r"]);
    env.mock.put_text("other/r.txt", &["t"]);
    env.engine_with(|c| c.sync_only_regex = Some("^notes/|[]|\\.md$".into()))
        .sync_once()
        .await
        .unwrap();
    assert!(env.remote("notes/a.md").is_some());
    assert!(env.remote("img/p.png").is_none());
    assert!(env.read("notes/r.md").is_some());
    assert!(env.read("other/r.txt").is_none());
}

#[tokio::test]
async fn allowlisted_dot_paths_sync_and_obsidian_never_does() {
    let env = Env::new().await;
    env.write(".notes/a.md", b"a");
    env.write(".notes/.inner/b.md", b"b");
    env.write(".other/c.md", b"c");
    env.mock.put_text(".notes/r.md", &["r"]);
    env.engine_with(|c| c.sync_dot_paths = vec![".notes".into()])
        .sync_once()
        .await
        .unwrap();
    assert!(env.remote(".notes/a.md").is_some());
    assert!(env.remote(".notes/.inner/b.md").is_none());
    assert!(env.remote(".other/c.md").is_none());
    assert_eq!(env.read(".notes/r.md").unwrap(), b"r");
    for bad in [".obsidian", ".trash", ".git"] {
        let couch = Couch::new(&RemoteConfig {
            url: env.mock.url.clone(),
            database: DB.into(),
            username: None,
            password: None,
        })
        .unwrap();
        let mut cfg = EngineConfig::new(env.vault(), env.dir.path().join(format!("s{bad}/index.sqlite")));
        cfg.sync_dot_paths = vec![bad.into()];
        assert!(Engine::open(couch, cfg).is_err(), "{bad} must be refused");
    }
}

#[tokio::test]
async fn remote_deletions_go_to_the_vault_trash_with_their_folders() {
    let env = Env::new().await;
    env.write("notes/sub/a.md", b"a");
    env.write("notes/b.md", b"b");
    env.write("notes/edited.md", b"base");
    env.engine().sync_once().await.unwrap();
    env.mock.soft_delete("notes/sub/a.md");
    env.mock.tombstone("notes/b.md");
    env.mock.soft_delete("notes/edited.md");
    env.write("notes/edited.md", b"edited here");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(env.read(".trash/notes/sub/a.md").unwrap(), b"a");
    assert_eq!(env.read(".trash/notes/b.md").unwrap(), b"b");
    assert!(env.read("notes/sub/a.md").is_none());
    assert!(!env.vault().join("notes/sub").exists(), "the emptied folder is removed");
    assert!(env.vault().join("notes").exists(), "a folder holding anything stays");
    // An unsynced edit is never moved.
    assert_eq!(env.read("notes/edited.md").unwrap(), b"edited here");
    assert!(env.read(".trash/notes/edited.md").is_none());
    // Nothing comes back from the trash, now or at the next pass.
    assert_eq!(rep.push.deleted, 0, "{rep:?}");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.pushed, 0);
    assert!(env.deleted("notes/sub/a.md"));
    assert!(env.mock.doc(&path_to_id(".trash/notes/sub/a.md", true)).is_none());
    // A second deletion of the same path does not overwrite the first.
    env.write("notes/sub/a.md", b"a2");
    env.engine().sync_once().await.unwrap();
    env.mock.soft_delete("notes/sub/a.md");
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.read(".trash/notes/sub/a.md").unwrap(), b"a");
    assert_eq!(env.read(".trash/notes/sub/a 1.md").unwrap(), b"a2");
}

#[tokio::test]
async fn trash_none_deletes_permanently() {
    let env = Env::new().await;
    env.write("x/a.md", b"a");
    env.engine_with(|c| c.trash = TrashMode::None)
        .sync_once()
        .await
        .unwrap();
    env.mock.soft_delete("x/a.md");
    env.engine_with(|c| c.trash = TrashMode::None)
        .sync_once()
        .await
        .unwrap();
    assert!(env.read("x/a.md").is_none());
    assert!(!env.vault().join(".trash").exists());
    assert!(!env.vault().join("x").exists());
}

#[tokio::test]
async fn the_daemon_applies_edited_ignore_files_and_its_own_trash_moves_stay_put() {
    let env = Env::new().await;
    env.write("a.md", b"a");
    env.write("logs/x.log", b"x");
    let engine = env.engine_with(|c| c.rescan_interval = Duration::from_secs(3600));
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let e = engine.clone();
        async move { e.run(rx).await }
    });
    wait_until("initial push", || env.remote("logs/x.log").is_some()).await;
    // Editing an ignore file takes effect while running.
    env.write(".gitignore", b"logs/\n*.tmp\n");
    wait_until("untracked", || env.row("logs/x.log").is_none()).await;
    env.write("new.tmp", b"t");
    env.write("logs/y.log", b"y");
    env.write("after.md", b"after");
    wait_until("after.md pushed", || env.remote("after.md").is_some()).await;
    assert!(env.remote("new.tmp").is_none());
    assert!(env.remote("logs/y.log").is_none());
    assert!(!env.deleted("logs/x.log"));
    // A remote deletion lands in the trash and is not pushed back.
    env.mock.soft_delete("a.md");
    wait_until("trashed", || env.read(".trash/a.md").is_some()).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(env.deleted("a.md"));
    assert!(env.mock.doc(&path_to_id(".trash/a.md", true)).is_none());
    let rep = engine.push_report();
    assert_eq!(rep.deleted, 0, "no local delete reported for our own move: {rep:?}");
    tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn status_shows_the_folder_kind() {
    let env = Env::new().await;
    env.write("a.md", b"a");
    let e = env.engine();
    e.sync_once().await.unwrap();
    assert_eq!(e.status().unwrap().folder_kind.unwrap().as_str(), "plain folder");
    std::fs::create_dir_all(env.vault().join(".obsidian")).unwrap();
    assert_eq!(e.status().unwrap().folder_kind.unwrap().as_str(), "obsidian vault");
}

/// A file synced under `before`, deleted on another device, then a run
/// under the narrower `after`: the local file stays where it is (the
/// plugin leaves files it does not sync alone).
async fn narrowed_then_deleted_remotely(
    path: &str,
    before: impl Fn(&mut EngineConfig),
    after: impl Fn(&mut EngineConfig),
    how: Delete,
    run: Run,
) {
    let env = Env::new().await;
    env.write(path, b"synced");
    env.write("notes/keep.md", b"keep");
    env.engine_with(&before).sync_once().await.unwrap();
    assert!(env.row(path).is_some(), "{path} synced first");
    match how {
        Delete::Soft => env.mock.soft_delete(path),
        Delete::Tombstone => env.mock.tombstone(path),
        Delete::Conflicted => {
            // The deletion wins over a live branch of the same base.
            let id = path_to_id(path, true);
            let base = env.mock.doc(&id).unwrap()["_rev"].as_str().unwrap().to_string();
            env.mock.soft_delete(path);
            let mut doc = env.mock.doc(&id).unwrap();
            doc["deleted"] = json!(false);
            env.mock.put_branch_with(doc, &base, "0000");
        }
    }
    let e = env.engine_with(|c| {
        c.trash = TrashMode::None;
        after(c)
    });
    match run {
        Run::Pull => {
            e.pull_once().await.unwrap();
        }
        Run::Sync => {
            e.sync_once().await.unwrap();
        }
    }
    assert_eq!(env.read(path).as_deref(), Some(&b"synced"[..]), "{path} left alone");
    assert!(env.row(path).is_none(), "{path} no longer tracked");
    assert_eq!(env.read("notes/keep.md").as_deref(), Some(&b"keep"[..]));
}

#[derive(Clone, Copy)]
enum Delete {
    Soft,
    Tombstone,
    Conflicted,
}

#[derive(Clone, Copy)]
enum Run {
    Pull,
    Sync,
}

#[tokio::test]
async fn a_narrowed_sync_only_regex_keeps_remote_deletes_off_the_files_it_hides() {
    for (how, run) in [
        (Delete::Soft, Run::Pull),
        (Delete::Tombstone, Run::Sync),
        (Delete::Conflicted, Run::Sync),
    ] {
        narrowed_then_deleted_remotely(
            "other/b.md",
            |_| {},
            |c| c.sync_only_regex = Some("^notes/".into()),
            how,
            run,
        )
        .await;
    }
}

#[tokio::test]
async fn a_narrowed_ignore_regex_keeps_remote_deletes_off_the_files_it_hides() {
    for (how, run) in [
        (Delete::Soft, Run::Sync),
        (Delete::Tombstone, Run::Pull),
        (Delete::Conflicted, Run::Pull),
    ] {
        narrowed_then_deleted_remotely(
            "other/b.md",
            |_| {},
            |c| c.ignore_regex = Some("^other/".into()),
            how,
            run,
        )
        .await;
    }
}

#[tokio::test]
async fn a_dot_path_dropped_from_the_allowlist_keeps_remote_deletes_off_its_files() {
    for (how, run) in [
        (Delete::Soft, Run::Sync),
        (Delete::Tombstone, Run::Pull),
        (Delete::Conflicted, Run::Sync),
    ] {
        narrowed_then_deleted_remotely(
            ".notes/c.md",
            |c| c.sync_dot_paths = vec![".notes".into()],
            |_| {},
            how,
            run,
        )
        .await;
    }
}
