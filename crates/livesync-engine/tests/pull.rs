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
        Engine::open(couch, cfg).unwrap()
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
    m.put(json!({"_id": "conflict.md", "path": "conflict.md", "type": "plain", "children": children, "mtime": 1, "ctime": 1, "size": 6, "eden": {}, "_conflicts": ["2-abc"]}));

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

    // Locked milestone that does not list us.
    let env = Env::new().await;
    env.mock.update_milestone(|m| m["locked"] = json!(true));
    assert_eq!(refused(env.engine().connect_gate(false).await), Refusal::Locked);

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
