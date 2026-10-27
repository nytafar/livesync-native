//! Engine behaviour against the mock CouchDB.

mod common;

use common::{milestone, Mock, DB};
use livesync_engine::{Engine, EngineConfig, EngineError, Index, State};
use livesync_model::Refusal;
use livesync_remote::{Couch, RemoteConfig};
use serde_json::json;
use std::path::Path;
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
    fn vault(&self) -> std::path::PathBuf {
        self.dir.path().join("vault")
    }
    fn engine_with(&self, f: impl FnOnce(&mut EngineConfig)) -> Engine {
        self.engine_try(f).unwrap()
    }
    fn engine_try(&self, f: impl FnOnce(&mut EngineConfig)) -> Result<Engine, EngineError> {
        let couch = Couch::new(&RemoteConfig {
            url: self.mock.url.clone(),
            database: DB.into(),
            username: Some("u".into()),
            password: Some("p".into()),
        })
        .unwrap();
        let mut cfg = EngineConfig::new(self.vault(), self.dir.path().join("state/index.sqlite"));
        cfg.batch_limit = 3; // force several pages
        cfg.concurrency = 2;
        f(&mut cfg);
        Engine::open(couch, cfg)
    }
    fn engine(&self) -> Engine {
        self.engine_with(|_| {})
    }
    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.vault().join(rel)).ok()
    }
    fn row(&self, doc_id: &str) -> Option<livesync_engine::FileRow> {
        Index::open(&self.dir.path().join("state/index.sqlite"))
            .unwrap()
            .by_doc(doc_id)
            .unwrap()
    }
}

fn refused(r: Result<impl std::fmt::Debug, EngineError>) -> Refusal {
    match r {
        Err(EngineError::Refused(x)) => x,
        other => panic!("expected refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn pull_materialises_files_and_skips_the_rest() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("Notes/Hello World.md", &["# Hello\n", "æøå 😀\n"]);
    let bin: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
    m.put_binary("attachments/pic.png", &bin, 1000);
    m.put_text("_underscore.md", &["under"]);
    // Compressed chunk (raw DEFLATE + marker), stored under the ID of its plain text.
    let plain = "compressed text ".repeat(50);
    let cid = livesync_model::chunk::chunk_id_xxhash64(&plain);
    let deflated = livesync_engine_test_deflate(plain.as_bytes());
    m.put(json!({"_id": cid, "type": "leaf", "data": format!("\u{000E}LZ\u{001D}{}", livesync_model::codec::base64_encode(&deflated))}));
    m.put(json!({"_id": "zipped.md", "path": "zipped.md", "type": "plain", "children": [cid], "mtime": 5, "ctime": 5, "size": plain.len(), "eden": {}}));
    // Legacy eden chunk.
    let eid = livesync_model::chunk::chunk_id_xxhash64("eden");
    m.put(json!({"_id": "eden.md", "path": "eden.md", "type": "plain", "children": [eid.clone()], "mtime": 5, "ctime": 5, "size": 4,
        "eden": {eid: {"data": "eden", "epoch": 1}}}));
    // Never materialised.
    m.put_text(".obsidian/app.json", &["{}"]);
    m.put_text("folder/.hidden.md", &["x"]);
    m.put(json!({"_id": "i:.obsidian/x.json", "path": "i:.obsidian/x.json", "type": "plain", "children": [], "mtime": 1, "ctime": 1, "size": 0, "eden": {}}));
    m.put_text("redflag.md", &["stop"]);
    m.put_text("flag_fetch.md", &["x"]);
    m.put_text("livesync_log_1.md", &["x"]);
    m.put_text("repo/.git/config", &["x"]);
    m.put_text("project/sub/.git/HEAD", &["x"]); // dot-path anyway
    m.put_text("Private/secret.md", &["x"]);
    m.put(json!({"_id": "big.bin", "path": "big.bin", "type": "newnote", "children": ["h:nope"], "mtime": 1, "ctime": 1, "size": 3u64 * 1024 * 1024, "eden": {}}));
    m.put_text("gone.md", &["bye"]);
    m.soft_delete("gone.md");
    // Conflicted document: the winner is written and marked.
    let children = m.put_chunks(&["winner"]);
    let loser = m.put_chunks(&["loser!"]);
    let doc = json!({"_id": "conflict.md", "path": "conflict.md", "type": "plain", "children": children, "mtime": 1, "ctime": 1, "size": 6, "eden": {}});
    let first = m.put(doc.clone());
    m.put(doc.clone());
    m.put(doc.clone()); // the winner is two revisions deep, the branch one
    let mut other = doc;
    other["children"] = json!(loser);
    m.put_branch(other, &first);

    let engine = env.engine_with(|c| {
        c.ignore_regex = Some("^private/".into());
        c.max_size_mb = Some(1);
    });
    let rep = engine.pull_once().await.unwrap();

    assert_eq!(
        env.read("Notes/Hello World.md").unwrap(),
        "# Hello\næøå 😀\n".as_bytes()
    );
    assert_eq!(env.read("attachments/pic.png").unwrap(), bin);
    assert_eq!(env.read("_underscore.md").unwrap(), b"under");
    assert_eq!(env.read("zipped.md").unwrap(), plain.as_bytes());
    assert_eq!(env.read("eden.md").unwrap(), b"eden");
    assert_eq!(env.read("conflict.md").unwrap(), b"winner");
    for skipped in [
        ".obsidian/app.json",
        "folder/.hidden.md",
        "redflag.md",
        "flag_fetch.md",
        "livesync_log_1.md",
        "repo/.git/config",
        "Private/secret.md",
        "big.bin",
        "gone.md",
        "i:.obsidian/x.json",
    ] {
        assert!(env.read(skipped).is_none(), "{skipped} must not be written");
    }
    // File mtime comes from the document.
    let md = std::fs::metadata(env.vault().join("Notes/Hello World.md")).unwrap();
    let mtime = md
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    assert_eq!(mtime, 1_700_000_100_000);

    assert_eq!(rep.written, 6);
    assert_eq!(rep.conflicted, 1);
    assert_eq!(rep.chunk_id_mismatches, 0);
    assert_eq!(rep.skipped.get("dot-path"), Some(&4));
    assert_eq!(rep.skipped.get("flag-file"), Some(&3));
    assert_eq!(rep.skipped.get("ignore-regex"), Some(&1));
    assert_eq!(rep.skipped.get("too-large"), Some(&1));
    assert_eq!(env.row("notes/hello world.md").unwrap().state, State::Clean);
    assert_eq!(env.row("/_underscore.md").unwrap().path, "_underscore.md");
    assert_eq!(env.row("conflict.md").unwrap().state, State::Conflicted);
    assert_eq!(env.row(".obsidian/app.json").unwrap().state, State::Skipped);
    assert!(env.row("gone.md").is_none());
    // No temp files remain anywhere.
    assert_eq!(count_temp(&env.vault()), 0);
    // Read-only: nothing but GETs, _changes and _all_docs.
    assert!(m.writes().is_empty(), "unexpected requests: {:?}", m.writes());
    drop(engine);

    // A second pull is a no-op.
    let rep2 = env.engine().pull_once().await.unwrap();
    assert_eq!(rep2.written, 0);
    assert_eq!(rep2.changes, 0);
}

fn livesync_engine_test_deflate(b: &[u8]) -> Vec<u8> {
    // Raw DEFLATE, like fflate's `deflate`.
    miniz_oxide_deflate(b)
}

fn miniz_oxide_deflate(b: &[u8]) -> Vec<u8> {
    // miniz_oxide is a dependency of livesync-model; reach it through a tiny
    // stored-block encoder instead to avoid another dev-dependency.
    let mut out = Vec::new();
    let chunks: Vec<&[u8]> = if b.is_empty() {
        vec![&[][..]]
    } else {
        b.chunks(65535).collect()
    };
    for (i, c) in chunks.iter().enumerate() {
        out.push(if i + 1 == chunks.len() { 1 } else { 0 });
        let len = c.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(c);
    }
    out
}

fn count_temp(root: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else if e.file_name().to_string_lossy().starts_with(".lsync-") {
                n += 1;
            }
        }
    }
    n
}

#[tokio::test]
async fn remote_updates_and_deletes_apply_to_clean_files() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("a/b/note.md", &["v1"]);
    m.put_text("keep.md", &["k"]);
    env.engine().pull_once().await.unwrap();
    assert_eq!(env.read("a/b/note.md").unwrap(), b"v1");

    m.put_text("a/b/note.md", &["v2 ", "longer"]);
    let rep = env.engine().pull_once().await.unwrap();
    assert_eq!(rep.written, 1);
    assert_eq!(env.read("a/b/note.md").unwrap(), b"v2 longer");
    let row = env.row("a/b/note.md").unwrap();
    assert_eq!(row.base_children.len(), 2);

    m.soft_delete("a/b/note.md");
    let rep = env.engine().pull_once().await.unwrap();
    assert_eq!(rep.deleted, 1);
    assert!(env.read("a/b/note.md").is_none());
    assert!(!env.vault().join("a").exists(), "empty folders pruned");
    assert!(env.row("a/b/note.md").is_none());

    // A hard tombstone (no body) is mapped back through the index.
    m.tombstone("keep.md");
    let rep = env.engine().pull_once().await.unwrap();
    assert_eq!(rep.deleted, 1);
    assert!(env.read("keep.md").is_none());
}

#[tokio::test]
async fn local_changes_are_never_overwritten_or_deleted() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("edited.md", &["base"]);
    m.put_text("deleted-remotely.md", &["base"]);
    env.engine().pull_once().await.unwrap();

    std::fs::write(env.vault().join("edited.md"), b"my local edit").unwrap();
    std::fs::write(env.vault().join("deleted-remotely.md"), b"also edited").unwrap();
    m.put_text("edited.md", &["remote v2"]);
    m.soft_delete("deleted-remotely.md");
    let rep = env.engine().pull_once().await.unwrap();
    assert_eq!(rep.dirty, 2);
    assert_eq!(env.read("edited.md").unwrap(), b"my local edit");
    assert_eq!(env.read("deleted-remotely.md").unwrap(), b"also edited");
    let row = env.row("edited.md").unwrap();
    assert_eq!(row.state, State::Dirty);
    assert!(row.note.unwrap().contains("not applied"));
    let st = env.engine().status().unwrap();
    assert_eq!(st.dirty.len(), 2);

    // Reverting the local edit to the synced content makes it clean again,
    // and the next remote change applies.
    std::fs::write(env.vault().join("edited.md"), b"base").unwrap();
    m.put_text("edited.md", &["remote v3"]);
    env.engine().pull_once().await.unwrap();
    assert_eq!(env.read("edited.md").unwrap(), b"remote v3");
    assert_eq!(env.row("edited.md").unwrap().state, State::Clean);
}

#[tokio::test]
async fn existing_files_without_index_rows_are_adopted_or_protected() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("same.md", &["identical"]);
    m.put_text("other.md", &["remote"]);
    std::fs::create_dir_all(env.vault()).unwrap();
    std::fs::write(env.vault().join("same.md"), b"identical").unwrap();
    std::fs::write(env.vault().join("other.md"), b"local, unknown origin").unwrap();
    let rep = env.engine().pull_once().await.unwrap();
    assert_eq!(rep.adopted, 1);
    assert_eq!(rep.dirty, 1);
    assert_eq!(env.read("other.md").unwrap(), b"local, unknown origin");
    assert_eq!(env.row("same.md").unwrap().state, State::Clean);
    let other = env.row("other.md").unwrap();
    assert_eq!(other.state, State::Dirty);
    assert_eq!(other.base_rev, None);
}

#[tokio::test]
async fn missing_chunks_are_deferred_and_retried() {
    let env = Env::new().await;
    let m = &env.mock;
    let ids = m.put_chunks(&["part1", "part2"]);
    m.remove_doc(&ids[1]); // the uploader has not sent this chunk yet
    m.put(json!({"_id": "late.md", "path": "late.md", "type": "plain", "children": ids, "mtime": 1, "ctime": 1, "size": 10, "eden": {}}));
    let rep = env.engine().pull_once().await.unwrap();
    assert_eq!(rep.pending, 1);
    assert!(env.read("late.md").is_none());
    let st = env.engine().status().unwrap();
    assert_eq!(st.pending.len(), 1);
    assert!(st.pending[0].reason.contains("missing"));

    m.put_chunks(&["part2"]);
    let rep = env.engine().pull_once().await.unwrap();
    assert_eq!(rep.pending, 0);
    assert_eq!(env.read("late.md").unwrap(), b"part1part2");
}

#[tokio::test]
async fn gates_fail_closed() {
    // Version document above 13.
    let env = Env::new().await;
    env.mock
        .put(json!({"_id": "obsydian_livesync_version", "type": "versioninfo", "version": 14}));
    assert!(matches!(
        refused(env.engine().connect_gate(false).await),
        Refusal::UnsupportedGeneration(14)
    ));

    // Unknown used_features.
    let env = Env::new().await;
    env.mock.put(json!({"_id": "obsydian_livesync_version", "type": "versioninfo", "version": 13, "used_features": ["teleport-v1"]}));
    assert!(matches!(
        refused(env.engine().connect_gate(false).await),
        Refusal::UnknownFeatures(_)
    ));

    // Locked milestone that does not list us: an index that already synced
    // with this remote is refused (a lock without a rebuild is a human's call)...
    let env = Env::new().await;
    env.engine().pull_once().await.unwrap();
    env.mock.update_milestone(|m| m["locked"] = json!(true));
    assert_eq!(refused(env.engine().connect_gate(false).await), Refusal::Locked);
    // ... while a fresh index may fetch, and is marked resolved afterwards.
    let env = Env::new().await;
    env.mock.update_milestone(|m| m["locked"] = json!(true));
    let g = env.engine().connect_gate(false).await.unwrap();
    assert!(g.fetch_to_resolve && g.needs_full_fetch);
    // A read-only pull completes the fetch but cannot mark this node
    // resolved: status says which of the two is still owed.
    env.engine().pull_once().await.unwrap();
    let st = env.engine().status().unwrap();
    assert!(st.needs_full_fetch && st.resolve_owed, "{st:?}");

    // Unsupported remote settings.
    for (k, v) in [
        ("encrypt", json!(true)),
        ("usePathObfuscation", json!(true)),
        ("hashAlg", json!("sha1")),
        ("pathNormalizationVersion", json!(1)),
    ] {
        let env = Env::new().await;
        env.mock
            .update_milestone(|m| m["tweak_values"]["PREFERRED"][k] = v.clone());
        assert!(
            matches!(
                refused(env.engine().connect_gate(false).await),
                Refusal::UnsupportedSetting(_)
            ),
            "{k}"
        );
    }

    // Not CouchDB.
    let env = Env::new().await;
    env.mock.state.lock().unwrap().root = json!({"minio": true});
    assert_eq!(refused(env.engine().connect_gate(false).await), Refusal::NotCouchDb);

    // A web page at the server root.
    let env = Env::new().await;
    let couch = Couch::new(&RemoteConfig {
        url: format!("{}/nowhere", env.mock.url),
        database: DB.into(),
        username: None,
        password: None,
    })
    .unwrap();
    let engine = Engine::open(couch, EngineConfig::new(env.vault(), env.dir.path().join("i.sqlite"))).unwrap();
    assert_eq!(refused(engine.connect_gate(false).await), Refusal::NotCouchDb);

    // Missing milestone.
    let env = Env::new().await;
    env.mock
        .state
        .lock()
        .unwrap()
        .local
        .remove("obsydian_livesync_milestone");
    assert_eq!(
        refused(env.engine().connect_gate(false).await),
        Refusal::MissingMilestone
    );

    // PREFERRED changes after adoption.
    let env = Env::new().await;
    env.engine().connect_gate(false).await.unwrap();
    env.mock
        .update_milestone(|m| m["tweak_values"]["PREFERRED"]["customChunkSize"] = json!(100));
    assert!(matches!(
        refused(env.engine().connect_gate(false).await),
        Refusal::TweakMismatch(_)
    ));
    env.engine().connect_gate(true).await.unwrap();
    env.engine().connect_gate(false).await.unwrap();

    // Encrypted chunk content stops the pull.
    let env = Env::new().await;
    env.mock
        .put(json!({"_id": "h:abc", "type": "leaf", "data": "%=AAAA", "e_": true}));
    env.mock.put(json!({"_id": "enc.md", "path": "enc.md", "type": "plain", "children": ["h:abc"], "mtime": 1, "ctime": 1, "size": 3, "eden": {}}));
    assert!(matches!(
        refused(env.engine().pull_once().await),
        Refusal::EncryptedContent(_)
    ));
    assert!(env.read("enc.md").is_none());

    // A version-doc change inside the feed stops the batch before it applies.
    let env = Env::new().await;
    env.engine().pull_once().await.unwrap();
    env.mock.put_text("after.md", &["x"]);
    env.mock
        .put(json!({"_id": "obsydian_livesync_version", "type": "versioninfo", "version": 14}));
    let r = env.engine_with(|c| c.batch_limit = 10).pull_once().await;
    assert!(matches!(refused(r), Refusal::UnsupportedGeneration(14)));
}

#[tokio::test]
async fn rebuild_elsewhere_resets_index_and_refetches() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("stable.md", &["same"]);
    m.put_text("edited-locally.md", &["base"]);
    env.engine().pull_once().await.unwrap();
    std::fs::write(env.vault().join("edited-locally.md"), b"local").unwrap();

    // Another device rebuilds: new `created`, new revisions.
    m.update_milestone(|ms| *ms = milestone(2000));
    m.put_text("stable.md", &["same"]);
    m.put_text("edited-locally.md", &["rebuilt"]);
    m.put_text("new.md", &["fresh"]);
    let engine = env.engine();
    let gate = engine.connect_gate(false).await.unwrap();
    assert!(gate.rebuild_detected);
    let rep = engine.pull_once().await.unwrap();
    assert_eq!(env.read("new.md").unwrap(), b"fresh");
    assert_eq!(env.read("stable.md").unwrap(), b"same");
    assert_eq!(env.read("edited-locally.md").unwrap(), b"local", "dirty file kept");
    assert!(rep.adopted >= 1);
    assert!(engine.status().unwrap().rebuild_detected_ms.is_some());
}

#[tokio::test]
async fn daemon_follows_changes_and_survives_outages() {
    let env = Env::new().await;
    let m = env.mock.clone();
    m.put_text("first.md", &["1"]);
    let engine = env.engine_with(|c| c.regate_interval = Duration::from_secs(60));
    let (tx, rx) = tokio::sync::watch::channel(false);
    let e2 = engine.clone();
    let h = tokio::spawn(async move { e2.daemon_pull_only(rx).await });

    wait_for(&env.vault().join("first.md"), b"1").await;
    m.put_text("second.md", &["2"]);
    wait_for(&env.vault().join("second.md"), b"2").await;
    // The server fails a few times; the daemon backs off and reconnects.
    m.state.lock().unwrap().fail_changes = 1;
    m.put_text("first.md", &["1 updated"]);
    wait_for(&env.vault().join("first.md"), b"1 updated").await;
    assert!(m.writes().is_empty());

    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let st = engine.status().unwrap();
    assert!(st.since.is_some());
    assert!(st.daemon_heartbeat_ms.is_some());
}

async fn wait_for(path: &Path, content: &[u8]) {
    for _ in 0..200 {
        if std::fs::read(path).ok().as_deref() == Some(content) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "{} never became {:?} (now {:?})",
        path.display(),
        String::from_utf8_lossy(content),
        std::fs::read(path)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    );
}

fn since_of(engine: &Engine) -> Option<String> {
    engine.status().unwrap().since
}

/// True when the current user can still read a file after `chmod 000`
/// (root), which makes the permission-based checks meaningless.
fn chmod_000_is_ignored(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::File::open(path).is_ok()
}

// Item 1: a per-file I/O error defers that file only; the rest of the batch
// and `since` move on.
#[tokio::test]
async fn per_file_io_error_defers_only_that_file() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("x", &["a file named x"]);
    env.engine().pull_once().await.unwrap();
    let since_before = since_of(&env.engine());

    // `x/y.md` cannot be stat'ed (ENOTDIR): `x` is a file.
    m.put_text("x/y.md", &["under x"]);
    m.put_text("unrelated.md", &["fine"]);
    let engine = env.engine_with(|c| c.batch_limit = 10);
    let rep = engine.pull_once().await.expect("one bad path must not fail the pull");
    assert_eq!(env.read("unrelated.md").unwrap(), b"fine");
    assert_eq!(env.read("x").unwrap(), b"a file named x");
    assert_eq!(rep.pending, 1);
    let st = engine.status().unwrap();
    assert_eq!(st.pending.len(), 1);
    assert_eq!(st.pending[0].doc_id, "x/y.md");
    assert_ne!(st.since, since_before, "since advanced past the bad document");
}

#[tokio::test]
async fn unreadable_local_files_do_not_stop_the_batch() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("locked.md", &["v1"]);
    m.put_text("gone.md", &["bye"]);
    env.engine().pull_once().await.unwrap();

    let unreadable = ["locked.md", "gone.md"];
    if unreadable.iter().any(|p| chmod_000_is_ignored(&env.vault().join(p))) {
        eprintln!("running as root; skipping the permission part");
        return;
    }
    m.put_text("locked.md", &["v2"]);
    m.soft_delete("gone.md");
    m.put_text("after.md", &["after"]);
    let engine = env.engine_with(|c| c.batch_limit = 20);
    let rep = engine
        .pull_once()
        .await
        .expect("unreadable files must not fail the pull");
    assert_eq!(env.read("after.md").unwrap(), b"after");
    assert!(
        env.vault().join("gone.md").exists(),
        "an unreadable file is never deleted"
    );
    assert_eq!(rep.pending, 2, "the update and the delete wait for a retry");
    let ids: Vec<String> = engine.status().unwrap().pending.into_iter().map(|p| p.doc_id).collect();
    assert_eq!(ids, vec!["gone.md".to_string(), "locked.md".to_string()]);
    // Retries back off instead of resetting to the first delay.
    drop(engine);
    env.engine().pull_once().await.unwrap();
    let attempts: Vec<u32> = env
        .engine()
        .status()
        .unwrap()
        .pending
        .iter()
        .map(|p| p.attempts)
        .collect();
    assert!(attempts.iter().all(|a| *a >= 2), "attempts {attempts:?}");
}

#[tokio::test]
async fn daemon_keeps_running_past_per_file_errors() {
    let env = Env::new().await;
    let m = env.mock.clone();
    m.put_text("x", &["file"]);
    let engine = env.engine_with(|c| c.regate_interval = Duration::from_secs(600));
    let (tx, rx) = tokio::sync::watch::channel(false);
    let e2 = engine.clone();
    let h = tokio::spawn(async move { e2.daemon_pull_only(rx).await });
    wait_for(&env.vault().join("x"), b"file").await;
    m.put_text("x/y.md", &["under x"]);
    m.put_text("unrelated.md", &["arrives"]);
    wait_for(&env.vault().join("unrelated.md"), b"arrives").await;
    assert!(!h.is_finished(), "the daemon must not exit on a per-file error");
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

// Item 2: names the filesystem cannot hold are skipped with a reason and
// never retried.
#[tokio::test]
async fn unwritable_names_are_skipped_and_not_retried() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("Notes/He said \"hi\".md", &["quoted"]);
    m.put_text("a:b.md", &["colon"]);
    m.put_text("ok.md", &["fine"]);
    let engine = env.engine_with(|c| c.restrict_names = Some(true));
    let rep = engine.pull_once().await.unwrap();
    assert_eq!(env.read("ok.md").unwrap(), b"fine");
    assert!(env.read("a:b.md").is_none());
    assert_eq!(rep.pending, 0);
    assert_eq!(rep.skipped.get("unwritable-name"), Some(&2));
    let row = env.row("notes/he said \"hi\".md").unwrap();
    assert_eq!(row.state, State::Skipped);
    assert!(row.note.unwrap().starts_with("unwritable name"));
    let st = engine.status().unwrap();
    assert!(st.pending.is_empty());
    assert_eq!(st.unwritable.len(), 2);

    // Not retried: a later pull fetches nothing, and a new revision of the
    // same document is skipped again without downloading its chunks.
    let rep = engine.pull_once().await.unwrap();
    assert_eq!(rep.chunks_fetched, 0);
    m.put_text("a:b.md", &["colon v2"]);
    let rep = engine.pull_once().await.unwrap();
    assert_eq!(rep.chunks_fetched, 0);
    assert_eq!(rep.pending, 0);
    assert_eq!(rep.skipped.get("unwritable-name"), Some(&1));
}

#[tokio::test]
async fn names_rejected_by_the_filesystem_are_skipped_as_unwritable() {
    // ENAMETOOLONG is the failure Linux can produce; the phone's FUSE layer
    // answers EPERM/EINVAL for forbidden characters, which takes the same path.
    let env = Env::new().await;
    let m = &env.mock;
    let long = format!("{}.md", "n".repeat(300));
    m.put_text(&long, &["long"]);
    m.put_text("ok.md", &["fine"]);
    let rep = env
        .engine()
        .pull_once()
        .await
        .expect("a rejected name must not fail the pull");
    assert_eq!(env.read("ok.md").unwrap(), b"fine");
    assert_eq!(rep.pending, 0);
    assert_eq!(rep.skipped.get("unwritable-name"), Some(&1));
    let row = env.row(&long).unwrap();
    assert_eq!(row.state, State::Skipped);
    assert!(row.note.unwrap().starts_with("unwritable name"));
}

// Item 3: an index belongs to one vault folder.
#[tokio::test]
async fn index_is_bound_to_its_vault_folder() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["A"]);
    env.engine().pull_once().await.unwrap();
    let other = env.dir.path().join("other-vault");
    let engine = env.engine_with(|c| c.vault = other.clone());
    assert!(matches!(
        refused(engine.pull_once().await),
        Refusal::ForeignVault { .. }
    ));
    assert!(!other.join("a.md").exists());
    drop(engine);
    // The original vault still works.
    env.engine().pull_once().await.unwrap();
}

// Item 4: `.nomedia` at the vault root.
#[tokio::test]
async fn pull_creates_nomedia_but_keeps_an_existing_one() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["A"]);
    env.engine().pull_once().await.unwrap();
    assert_eq!(env.read(".nomedia").unwrap(), b"");
    std::fs::write(env.vault().join(".nomedia"), b"mine").unwrap();
    env.engine().pull_once().await.unwrap();
    assert_eq!(env.read(".nomedia").unwrap(), b"mine");
}

// Item 6: the daemon wakes for pending rows when they are due, not on a
// fixed timer.
#[tokio::test]
async fn daemon_retries_pending_rows_when_due() {
    let env = Env::new().await;
    let m = env.mock.clone();
    let ids = m.put_chunks(&["part1", "part2"]);
    m.remove_doc(&ids[1]);
    m.put(json!({"_id": "late.md", "path": "late.md", "type": "plain", "children": ids, "mtime": 1, "ctime": 1, "size": 10, "eden": {}}));
    let engine = env.engine_with(|c| c.regate_interval = Duration::from_secs(600));
    let (tx, rx) = tokio::sync::watch::channel(false);
    let e2 = engine.clone();
    let h = tokio::spawn(async move { e2.daemon_pull_only(rx).await });
    for _ in 0..100 {
        if !engine.status().unwrap().pending.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(engine.status().unwrap().pending.len(), 1);

    // Idle with a row not yet due: the longpoll stays open.
    let before = m.changes_requests();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(m.changes_requests() <= before + 1, "longpoll reopened while idle");

    // The chunk arrives (no feed entry for the file) and the row becomes due
    // in a moment: the daemon retries it then.
    m.put_chunks(&["part2"]);
    let soon = livesync_engine::vaultfs::now_ms() + 300;
    Index::open(&env.dir.path().join("state/index.sqlite"))
        .unwrap()
        .add_pending("late.md", None, "test: due soon", soon - 60_000)
        .unwrap();
    engine.nudge(); // re-evaluate now (the Android shell nudges on network changes)
    wait_for(&env.vault().join("late.md"), b"part1part2").await;
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

// Item 7: only `reason: "deleted"` counts as a deletion in the retry.
#[tokio::test]
async fn pending_retry_deletes_only_on_a_deleted_404() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("keep.md", &["v1"]);
    m.put_text("del.md", &["v1"]);
    env.engine().pull_once().await.unwrap();
    // New revisions whose chunks are not there yet: both go pending.
    for path in ["keep.md", "del.md"] {
        let ids = m.put_chunks(&["v2"]);
        m.remove_doc(&ids[0]);
        m.put(json!({"_id": path, "path": path, "type": "plain", "children": ids, "mtime": 1, "ctime": 1, "size": 2, "eden": {}}));
    }
    let rep = env.engine().pull_once().await.unwrap();
    assert_eq!(rep.pending, 2);

    // `keep.md` disappears without a tombstone (database recreated, proxy
    // misroute: 404 "missing"); `del.md` is deleted (404 "deleted").
    m.state.lock().unwrap().docs.remove("keep.md");
    m.replace_silently("del.md", json!({"_id": "del.md", "_rev": "9-x", "_deleted": true}));
    let rep = env.engine().pull_once().await.unwrap();
    assert_eq!(env.read("keep.md").unwrap(), b"v1", "a plain 404 never deletes");
    assert!(env.read("del.md").is_none(), "a deleted 404 does");
    assert_eq!(rep.pending, 1);
    let st = env.engine().status().unwrap();
    assert_eq!(st.pending[0].doc_id, "keep.md");
    assert!(st.pending[0].reason.contains("missing"), "{}", st.pending[0].reason);
}

// Item 9: a case-only rename on a case-sensitive filesystem still replaces
// the old name (the case-insensitive guard must not leave it behind). Checked
// on the names in the directory, not with `exists`, which on a folding
// filesystem finds either spelling.
#[tokio::test]
async fn case_only_rename_on_a_case_sensitive_fs_removes_the_old_name() {
    let env = Env::new().await;
    require_case_sensitive!(env.dir.path());
    let m = &env.mock;
    let names = || {
        let mut n: Vec<String> = std::fs::read_dir(env.vault().join("Dir"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        n.sort();
        n
    };
    m.put_text("Dir/Note.md", &["same"]);
    env.engine().pull_once().await.unwrap();
    assert_eq!(names(), ["Note.md"]);
    m.put_text("Dir/note.md", &["same"]);
    env.engine().pull_once().await.unwrap();
    assert_eq!(names(), ["note.md"]);
    assert_eq!(env.row("dir/note.md").unwrap().path, "Dir/note.md");
}

// Item 10: one process per state directory and per vault.
#[tokio::test]
async fn a_second_engine_on_the_same_state_or_vault_is_refused() {
    let env = Env::new().await;
    let first = env.engine();
    let same_state = env.engine_try(|_| {});
    assert!(
        matches!(same_state, Err(EngineError::Busy(_))),
        "{:?}",
        same_state.err()
    );
    let other_state = env.dir.path().join("state2/index.sqlite");
    let same_vault = env.engine_try(|c| c.index_path = other_state.clone());
    assert!(
        matches!(same_vault, Err(EngineError::Busy(_))),
        "{:?}",
        same_vault.err()
    );
    drop(first);
    env.engine_try(|_| {})
        .expect("free again once the first engine is gone");
}

// Issue #9: on a filesystem without flock (Android's shared storage, where it
// fails with ENOSYS) two profiles on one vault folder are still refused, the
// refusal names the holder, and nothing is written to the vault by it.
#[tokio::test]
async fn two_profiles_on_one_vault_without_flock_are_refused() {
    let env = Env::new().await;
    let no_flock = |c: &mut EngineConfig| c.vault_flock = false;
    let first = env.engine_try(no_flock).unwrap();
    let st = first.status().unwrap();
    let mode = st.vault_lock.unwrap();
    assert!(mode.starts_with("host lock only"), "{mode}");
    let lock = env.vault().join(livesync_engine::engine::VAULT_LOCK);
    let record = livesync_engine::platform::record_path(&lock);
    let snapshot = || {
        [&lock, &record].map(|p| {
            (
                std::fs::read(p).unwrap(),
                std::fs::metadata(p).unwrap().modified().unwrap(),
            )
        })
    };
    let before = snapshot();
    // The lock file stays empty; the record beside it names the process
    // but carries no local paths.
    assert!(before[0].0.is_empty());
    let text = String::from_utf8(before[1].0.clone()).unwrap();
    assert!(!text.contains("state"), "{text}");

    let second_state = env.dir.path().join("profile-2/index.sqlite");
    for flock in [false, true] {
        let r = env.engine_try(|c| {
            c.index_path = second_state.clone();
            c.vault_flock = flock;
        });
        let msg = match r {
            Err(EngineError::Busy(m)) => m,
            other => panic!("expected busy, got {:?}", other.err()),
        };
        assert!(msg.contains(&format!("pid {}", std::process::id())), "{msg}");
        assert!(msg.contains(&livesync_engine::platform::process_name()), "{msg}");
    }
    assert!(
        snapshot() == before,
        "a refused engine wrote the lock file or the record"
    );

    drop(first);
    let second = env
        .engine_try(|c| {
            c.index_path = second_state.clone();
            c.vault_flock = false;
        })
        .expect("free again once the first engine is gone");
    let rec = livesync_engine::platform::read_holder(&record).unwrap();
    assert_eq!(rec.pid, std::process::id());
    assert_eq!(std::fs::metadata(&lock).unwrap().len(), 0);
    drop(second);
}

/// Log lines emitted on this thread while the guard lives (the engine's
/// synchronous steps run on the test thread under the current-thread runtime).
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn start() -> (Captured, tracing::subscriber::DefaultGuard) {
        let c = Captured::default();
        let w = c.clone();
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(move || w.clone())
            .finish();
        (c.clone(), tracing::subscriber::set_default(sub))
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

fn index_paths(env: &Env) -> Vec<String> {
    Index::open(&env.dir.path().join("state/index.sqlite"))
        .unwrap()
        .all_rows()
        .unwrap()
        .into_iter()
        .map(|r| r.path)
        .collect()
}

// Phone run (issue #3): dot-paths under an excluded folder were recorded as
// skipped rows, and the next start reported a scope change that never was.
#[tokio::test]
async fn a_pull_records_no_rows_outside_the_folder_scope() {
    use livesync_model::scope::{Scope, ScopeMode};
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("notes/a.md", &["a"]);
    m.put_text("notes/.hidden.md", &["in scope, skipped"]);
    m.put_text("private/b.md", &["b"]);
    m.put_text("private/vault/.obsidian/app.json", &["{}"]);
    m.put_text("private/.DS_Store", &["x"]);
    m.put_text("Private/Mixed/.trash/c.md", &["c"]);
    let excl = || Scope::new(ScopeMode::Exclude, &["private".into()]).unwrap();

    env.engine_with(|c| c.scope = excl()).pull_once().await.unwrap();
    let paths = index_paths(&env);
    assert!(
        paths.iter().all(|p| excl().contains(p)),
        "rows outside the scope: {paths:?}"
    );
    // Skipped dot-paths inside the scope still have their row.
    assert_eq!(env.row("notes/.hidden.md").unwrap().state, State::Skipped);
    assert!(env.read("private/b.md").is_none());

    // An index with stray rows from an older build: the next start drops
    // them without claiming the scope changed.
    let ix = Index::open(&env.dir.path().join("state/index.sqlite")).unwrap();
    let mut stray = env.row("notes/.hidden.md").unwrap();
    stray.path = "private/vault/.obsidian/app.json".into();
    stray.doc_id = "private/vault/.obsidian/app.json".into();
    ix.upsert(&stray).unwrap();
    drop(ix);
    let (log, guard) = Captured::start();
    env.engine_with(|c| c.scope = excl()).pull_once().await.unwrap();
    drop(guard);
    assert!(env.row("private/vault/.obsidian/app.json").is_none());
    assert!(
        !log.text().contains("folder scope is now"),
        "unchanged scope logged as a change:\n{}",
        log.text()
    );

    // A real narrowing still says so.
    let (log, guard) = Captured::start();
    let narrower = Scope::new(ScopeMode::Exclude, &["private".into(), "notes".into()]).unwrap();
    env.engine_with(|c| c.scope = narrower).pull_once().await.unwrap();
    drop(guard);
    assert!(env.row("notes/a.md").is_none());
    assert!(log.text().contains("folder scope is now"), "{}", log.text());
}

// Issue #3: "Last pull" is set once the pull-only daemon's first catch-up is
// done, not only when a later change arrives.
#[tokio::test]
async fn the_pull_only_daemon_records_its_first_catch_up_as_a_pull() {
    let env = Env::new().await;
    env.mock.put_text("first.md", &["1"]);
    let engine = env.engine_with(|c| c.regate_interval = Duration::from_secs(60));
    let (tx, rx) = tokio::sync::watch::channel(false);
    let e2 = engine.clone();
    let h = tokio::spawn(async move { e2.daemon_pull_only(rx).await });
    let mut st = engine.status().unwrap();
    for _ in 0..200 {
        st = engine.status().unwrap();
        if st.daemon_heartbeat_ms.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(st.daemon_heartbeat_ms.is_some(), "never caught up");
    assert!(st.last_pull_ms.is_some(), "no last pull after the first catch-up");
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(env.mock.writes().is_empty());
}
