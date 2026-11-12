//! End-to-end encrypted vaults against the mock CouchDB: the database is
//! encrypted as the official clients do it (V2, no path obfuscation), the
//! engine works through the transform in `Couch`. The mock's write guard is
//! on for every test here: nothing with plaintext content or a plain `h:`
//! chunk ID may reach the encrypted database (and nothing encrypted the
//! plaintext one in the tests that use one).

mod common;

use common::{Mock, DB};
use livesync_crypto::Passphrase;
use livesync_engine::{AdoptOptions, Engine, EngineConfig, EngineError, Index, State};
use livesync_model::path::path_to_id;
use livesync_model::splitter::{chunk_file_with, SplitParams};
use livesync_model::Refusal;
use livesync_remote::{Couch, RemoteConfig};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

/// Obvious throwaways.
const PASS: &str = "correct horse battery staple (test only)";
const WRONG: &str = "not the passphrase (test only)";

struct Env {
    mock: Mock,
    dir: tempfile::TempDir,
}

impl Env {
    async fn new() -> Env {
        Env {
            mock: Mock::start_encrypted(PASS).await,
            dir: tempfile::tempdir().unwrap(),
        }
    }
    async fn plain() -> Env {
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
    fn couch(&self) -> Couch {
        Couch::new(&RemoteConfig {
            url: self.mock.url.clone(),
            database: DB.into(),
            username: Some("u".into()),
            password: Some("p".into()),
        })
        .unwrap()
    }
    fn engine_with(&self, pass: Option<&str>, f: impl FnOnce(&mut EngineConfig)) -> Engine {
        let mut cfg = EngineConfig::new(self.vault(), self.index_path());
        cfg.batch_limit = 5;
        cfg.concurrency = 2;
        cfg.rebuild_quiet = Duration::ZERO;
        cfg.settle = Duration::from_millis(200);
        cfg.delete_hold = Duration::from_millis(500);
        cfg.passphrase = pass.map(Passphrase::new);
        f(&mut cfg);
        Engine::open(self.couch(), cfg).unwrap()
    }
    fn engine(&self) -> Engine {
        self.engine_with(Some(PASS), |_| {})
    }
    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.vault().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.vault().join(rel)).ok()
    }
    fn row(&self, doc_id: &str) -> Option<livesync_engine::FileRow> {
        Index::open(&self.index_path()).unwrap().by_doc(doc_id).unwrap()
    }
    fn doc(&self, path: &str) -> Value {
        self.mock
            .doc(&path_to_id(path, true))
            .unwrap_or_else(|| panic!("no remote doc for {path}"))
    }
    fn remote(&self, path: &str) -> Option<Vec<u8>> {
        self.mock.content(&path_to_id(path, true))
    }
    /// Every chunk document on the remote is V2 ciphertext under an `h:+` ID.
    fn assert_all_chunks_sealed(&self) {
        let st = self.mock.state.lock().unwrap();
        for (id, d) in &st.docs {
            if d.get("type") == Some(&json!("leaf")) {
                assert!(id.starts_with("h:+"), "{id}");
                assert_eq!(d["e_"], json!(true), "{id}");
                assert!(d["data"].as_str().unwrap().starts_with("%="), "{id}");
            }
        }
    }
}

fn refused(r: Result<impl std::fmt::Debug, EngineError>) -> Refusal {
    match r {
        Err(EngineError::Refused(x)) => x,
        other => panic!("expected refusal, got {other:?}"),
    }
}

fn params() -> SplitParams {
    SplitParams {
        custom_chunk_size: 60.0,
        minimum_chunk_size: 20,
    }
}

/// The shape of the first real database this serves: version document at
/// generation 12 without a feature list, no `syncinfo`, the tweaks a
/// 1.0.37 plugin wrote (with `doNotUseFixedRevisionForChunks`), and devices
/// older than 1.0.49 that know nothing of the unencrypted-content policy.
fn real_shape(m: &Mock) {
    m.update_milestone(|ms| {
        for k in ["PREFERRED", "desktop1"] {
            ms["tweak_values"][k]["doNotUseFixedRevisionForChunks"] = json!(true);
            ms["tweak_values"][k]["customChunkSize"] = json!(60);
        }
        ms["accepted_nodes"] = json!(["desktop1", "phone1"]);
        ms["node_chunk_info"]["phone1"] = json!({"min": 0, "max": 2400, "current": 2});
        ms["tweak_values"]["phone1"] = ms["tweak_values"]["desktop1"].clone();
        ms["node_info"] = json!({
            "desktop1": {"device_name": "desk", "app_version": "1.10.0", "plugin_version": "1.0.37"},
            "phone1": {"device_name": "phone", "app_version": "1.8.0", "plugin_version": "0.25.43"}
        });
    });
    let v = m.doc("obsydian_livesync_version").unwrap();
    assert_eq!(v["version"], json!(12));
    assert!(v.get("used_features").is_none());
    assert!(m.doc("syncinfo").is_none());
}

#[tokio::test]
async fn an_encrypted_database_pulls_into_plain_files_and_pull_never_writes() {
    let env = Env::new().await;
    let m = &env.mock;
    real_shape(m);
    m.put_text("Notes/Hello.md", &["# Hello\n", "æøå 😀 encrypted\n"]);
    let bin: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
    m.put_binary("img/pic.png", &bin, 1000);
    // A note an older device wrote with an encrypted eden.
    let piece = "kept in eden";
    let cid = m.chunk_id(piece);
    let sealed = {
        let st = m.state.lock().unwrap();
        st.cipher
            .as_ref()
            .unwrap()
            .incoming(json!({"_id": "eden.md", "eden": {cid.clone(): {"data": piece, "epoch": 1}}}))
            .unwrap()
    };
    assert!(sealed["eden"].get("h:++encrypted-hkdf").is_some());
    m.put(
        json!({"_id": "eden.md", "path": "eden.md", "type": "plain", "children": [cid], "mtime": 1, "ctime": 1,
        "size": piece.len(), "eden": sealed["eden"].clone()}),
    );
    // Every chunk on the remote is ciphertext.
    env.assert_all_chunks_sealed();
    m.clear_requests();

    let rep = env.engine().pull_once().await.unwrap();
    assert_eq!(rep.written, 3, "{rep:?}");
    assert_eq!(rep.chunk_id_mismatches, 0, "the passphrase-mixed IDs match");
    assert_eq!(
        env.read("Notes/Hello.md").unwrap(),
        "# Hello\næøå 😀 encrypted\n".as_bytes()
    );
    assert_eq!(env.read("img/pic.png").unwrap(), bin);
    assert_eq!(env.read("eden.md").unwrap(), piece.as_bytes());
    assert!(m.writes().is_empty(), "a pull never writes: {:?}", m.writes());
}

#[tokio::test]
async fn pushes_edits_renames_and_deletes_are_encrypted() {
    let env = Env::new().await;
    real_shape(&env.mock);
    let text = "# Note\n\nSome æøå text 😀 ".repeat(400);
    let bin: Vec<u8> = (0..300_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    env.write("Notes/Hello.md", text.as_bytes());
    env.write("img/pic.png", &bin);
    env.write("empty.md", b"");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.pushed, 3, "{rep:?}");
    env.assert_all_chunks_sealed();
    assert_eq!(env.remote("Notes/Hello.md").unwrap(), text.as_bytes());
    assert_eq!(env.remote("img/pic.png").unwrap(), bin);
    // The IDs are the ones an official client makes with this passphrase.
    let ids =
        livesync_model::chunk::ChunkIdScheme::Encrypted(livesync_model::chunk::PassphraseHash::from_passphrase(PASS));
    let want: Vec<String> = chunk_file_with("Notes/Hello.md", text.as_bytes(), &params(), &ids)
        .chunks
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(env.doc("Notes/Hello.md")["children"], json!(want));
    // Metadata stays readable, as upstream leaves it without path obfuscation.
    assert_eq!(env.doc("Notes/Hello.md")["path"], json!("Notes/Hello.md"));

    // Edit, rename, delete.
    env.write("Notes/Hello.md", b"edited\n");
    std::fs::rename(env.vault().join("img/pic.png"), env.vault().join("img/moved.png")).unwrap();
    std::fs::remove_file(env.vault().join("empty.md")).unwrap();
    let rep = env.engine().sync_once().await.unwrap();
    assert!(rep.push.pushed >= 2, "{rep:?}");
    env.assert_all_chunks_sealed();
    assert_eq!(env.remote("Notes/Hello.md").unwrap(), b"edited\n");
    assert_eq!(env.remote("img/moved.png").unwrap(), bin);
    assert_eq!(env.doc("img/pic.png")["deleted"], json!(true));
    assert_eq!(env.doc("empty.md")["deleted"], json!(true));

    // A second device with the same passphrase reads it all back.
    let dir2 = tempfile::tempdir().unwrap();
    let mut cfg = EngineConfig::new(dir2.path().join("vault"), dir2.path().join("state/index.sqlite"));
    cfg.passphrase = Some(Passphrase::new(PASS));
    let other = Engine::open(env.couch(), cfg).unwrap();
    other.pull_once().await.unwrap();
    assert_eq!(
        std::fs::read(dir2.path().join("vault/Notes/Hello.md")).unwrap(),
        b"edited\n"
    );
    assert_eq!(std::fs::read(dir2.path().join("vault/img/moved.png")).unwrap(), bin);
    assert!(!dir2.path().join("vault/empty.md").exists());
}

#[tokio::test]
async fn remote_edits_deletes_and_conflicts_come_down_decrypted() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("a.md", &["first"]);
    m.put_text("b.md", &["bee"]);
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.read("a.md").unwrap(), b"first");

    // Another device edits a.md and deletes b.md.
    m.put_text("a.md", &["second"]);
    m.soft_delete("b.md");
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.read("a.md").unwrap(), b"second");
    assert!(env.read("b.md").is_none());

    // Concurrent edits: ours is kept as a conflicting revision, encrypted.
    let base = env.row("a.md").unwrap().base_rev.unwrap();
    let theirs = m.text_doc("a.md", "theirs");
    m.put_branch_with(theirs, &base, "f".repeat(32).as_str());
    env.write("a.md", b"ours");
    let rep = env.engine().sync_once().await.unwrap();
    assert!(rep.push.conflicts + rep.pull.conflicted >= 1, "{rep:?}");
    env.assert_all_chunks_sealed();
    let d = m.doc("a.md").unwrap();
    let revs: Vec<String> = std::iter::once(d["_rev"].as_str().unwrap().to_string())
        .chain(
            d["_conflicts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_str().unwrap().to_string()),
        )
        .collect();
    let contents: Vec<Vec<u8>> = revs.iter().map(|r| m.content_at("a.md", r).unwrap()).collect();
    assert!(contents.contains(&b"ours".to_vec()), "{contents:?}");
    assert!(contents.contains(&b"theirs".to_vec()), "{contents:?}");
    assert_eq!(env.engine().status().unwrap().conflicted.len(), 1);
}

#[tokio::test]
async fn adopt_matches_a_folder_by_its_encrypted_chunk_ids() {
    let env = Env::new().await;
    let text = "adopted text, the same on both sides\n".repeat(30);
    let ids =
        livesync_model::chunk::ChunkIdScheme::Encrypted(livesync_model::chunk::PassphraseHash::from_passphrase(PASS));
    let chunked = chunk_file_with("same.md", text.as_bytes(), &params(), &ids);
    let pieces: Vec<&str> = chunked.chunks.iter().map(|(_, d)| d.as_str()).collect();
    env.mock.put_text("same.md", &pieces);
    env.mock.put_text("other.md", &["remote version"]);
    env.write("same.md", text.as_bytes());
    env.write("other.md", b"local version");
    env.mock.clear_requests();
    let rep = env
        .engine()
        .adopt(AdoptOptions {
            apply: true,
            accept_weak_match: true,
            pull_only: false,
        })
        .await
        .unwrap();
    assert_eq!(rep.identical, 1, "{rep:?}");
    assert_eq!(rep.differing.len(), 1, "{rep:?}");
    assert!(
        env.mock.writes().is_empty(),
        "adopt writes nothing: {:?}",
        env.mock.writes()
    );
    assert_eq!(env.row("same.md").unwrap().state, State::Clean);
    // The first sync keeps both versions of the other file.
    env.engine().sync_once().await.unwrap();
    env.assert_all_chunks_sealed();
}

#[tokio::test]
async fn a_wrong_passphrase_stops_before_anything_is_written() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["secret note"]);
    env.write("local.md", b"a local file");
    env.mock.clear_requests();
    let r = refused(env.engine_with(Some(WRONG), |_| {}).sync_once().await);
    assert!(matches!(r, Refusal::WrongPassphrase(_)), "{r:?}");
    assert!(r.to_string().contains("does not decrypt"), "{r}");
    assert!(env.read("a.md").is_none());
    assert!(env.mock.writes().is_empty(), "{:?}", env.mock.writes());
    assert_eq!(env.read("local.md").unwrap(), b"a local file");
    // Status says so, and never shows the passphrase.
    let st = env.engine_with(Some(WRONG), |_| {}).status().unwrap();
    let shown = serde_json::to_string(&st).unwrap();
    assert!(shown.contains("does not decrypt"), "{shown}");
    assert!(!shown.contains(WRONG) && !shown.contains(PASS));
    // The right one goes on from there.
    env.engine().sync_once().await.unwrap();
    assert_eq!(env.read("a.md").unwrap(), b"secret note");
}

#[tokio::test]
async fn a_wrong_passphrase_is_found_by_syncinfo_too() {
    let env = Env::new().await;
    let sealed = {
        let st = env.mock.state.lock().unwrap();
        st.cipher
            .as_ref()
            .unwrap()
            .incoming(json!({"_id": "syncinfo", "type": "syncinfo", "data": "random"}))
            .unwrap()
    };
    env.mock.put(sealed);
    let r = refused(env.engine_with(Some(WRONG), |_| {}).connect_gate(false).await);
    assert!(
        matches!(&r, Refusal::WrongPassphrase(w) if w.contains("syncinfo")),
        "{r:?}"
    );
    env.engine().connect_gate(false).await.unwrap();
}

#[tokio::test]
async fn passphrase_and_encrypt_must_agree() {
    // Encrypted database, no passphrase.
    let env = Env::new().await;
    env.mock.put_text("a.md", &["x"]);
    let r = refused(env.engine_with(None, |_| {}).pull_once().await);
    assert!(matches!(r, Refusal::PassphraseMissing { .. }), "{r:?}");
    assert!(env.read("a.md").is_none());
    // The front-end's own advice.
    let r = refused(
        env.engine_with(None, |c| c.passphrase_how = "the desktop CLI supports it".into())
            .pull_once()
            .await,
    );
    assert!(r.to_string().contains("the desktop CLI supports it"), "{r}");

    // A passphrase for a plaintext database.
    let env = Env::plain().await;
    env.mock.put_text("a.md", &["x"]);
    env.write("b.md", b"local");
    let r = refused(env.engine_with(Some(PASS), |_| {}).sync_once().await);
    assert_eq!(r, Refusal::PassphraseOnPlainDatabase);
    assert!(env.mock.writes().is_empty(), "{:?}", env.mock.writes());
    assert!(env.read("a.md").is_none());
    // Without it, the plaintext vault syncs as before, nothing encrypted.
    env.engine_with(None, |_| {}).sync_once().await.unwrap();
    assert_eq!(env.read("a.md").unwrap(), b"x");
    assert!(env.doc("b.md")["children"][0].as_str().unwrap().starts_with("h:"));
    assert!(!env.doc("b.md")["children"][0].as_str().unwrap().starts_with("h:+"));
}

#[tokio::test]
async fn plaintext_content_stops_the_pull_unless_accepted() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("good.md", &["fine"]);
    // An unencrypted device wrote a plain chunk and a note referring to it.
    m.put(json!({"_id": "h:plainchunk", "type": "leaf", "data": "in the clear"}));
    m.put(
        json!({"_id": "leak.md", "path": "leak.md", "type": "plain", "children": ["h:plainchunk"],
        "mtime": 1, "ctime": 1, "size": 12, "eden": {}}),
    );
    // ... and an inline note.
    m.put(json!({"_id": "inline.md", "path": "inline.md", "type": "notes", "data": "inline text"}));

    let r = refused(env.engine().pull_once().await);
    assert!(matches!(&r, Refusal::UnencryptedContent { .. }), "{r:?}");
    assert!(r.to_string().contains("accept_unencrypted_content"), "{r}");
    assert!(env.read("leak.md").is_none());
    assert!(env.read("inline.md").is_none());
    // Shown in status, and it stays until a pull gets past it.
    let st = env.engine().status().unwrap();
    assert!(
        st.upstream_stop.as_deref().is_some_and(|s| s.contains("unencrypted")),
        "{st:?}"
    );
    assert!(
        st.last_error.as_deref().is_some_and(|s| s.contains("unencrypted")),
        "{st:?}"
    );
    assert!(m.writes().is_empty());

    // Accepted: the plain chunk is read as it is (the inline note stays a
    // legacy document lsync does not write).
    let rep = env
        .engine_with(Some(PASS), |c| c.accept_unencrypted_content = true)
        .pull_once()
        .await
        .unwrap();
    assert_eq!(env.read("leak.md").unwrap(), b"in the clear", "{rep:?}");
    assert_eq!(env.read("good.md").unwrap(), b"fine");
    let st = env.engine().status().unwrap();
    assert!(st.upstream_stop.is_none(), "{st:?}");
}

#[tokio::test]
async fn a_tampered_or_v1_chunk_holds_its_file_and_nothing_else() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("ok1.md", &["readable one"]);
    m.put_text("ok2.md", &["readable two"]);
    m.put_text("tampered.md", &["will be changed"]);
    m.put_text("v1.md", &["old format"]);
    // Flip a byte inside the ciphertext of tampered.md's chunk.
    let tid = m.doc("tampered.md").unwrap()["children"][0]
        .as_str()
        .unwrap()
        .to_string();
    let mut chunk = m.doc(&tid).unwrap();
    let data = chunk["data"].as_str().unwrap().to_string();
    let mut bytes = data.into_bytes();
    let i = bytes.len() - 6;
    bytes[i] = if bytes[i] == b'A' { b'B' } else { b'A' };
    chunk["data"] = json!(String::from_utf8(bytes).unwrap());
    m.put(chunk);
    // A V1 chunk under v1.md.
    let vid = m.doc("v1.md").unwrap()["children"][0].as_str().unwrap().to_string();
    let mut chunk = m.doc(&vid).unwrap();
    chunk["data"] = json!("%~V1CIPHERTEXT");
    m.put(chunk);

    // A local copy of the tampered note from before: left alone, never pushed.
    env.write("tampered.md", b"older local copy");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(env.read("ok1.md").unwrap(), b"readable one");
    assert_eq!(env.read("ok2.md").unwrap(), b"readable two");
    assert!(env.read("v1.md").is_none());
    assert_eq!(env.read("tampered.md").unwrap(), b"older local copy");
    let st0 = env.engine().status().unwrap();
    assert!(rep.pull.pending >= 2, "{rep:?} {:?}", st0.pending);
    let st = env.engine().status().unwrap();
    let why = |id: &str| {
        st.pending
            .iter()
            .find(|p| p.doc_id == id)
            .map(|p| p.reason.clone())
            .unwrap_or_default()
    };
    assert!(
        why("tampered.md").contains("wrong passphrase or damaged ciphertext"),
        "{st:?}"
    );
    assert!(why("v1.md").contains("V1"), "{st:?}");
    // The remote documents are as they were: no edit, no deletion.
    assert_eq!(env.doc("tampered.md")["children"][0], json!(tid));
    assert!(env.doc("v1.md").get("deleted").is_none());
    assert_eq!(rep.push.pushed, 0, "{rep:?}");
    // Still held on the next pass.
    env.engine().sync_once().await.unwrap();
    assert!(env.read("v1.md").is_none());
    assert_eq!(env.read("tampered.md").unwrap(), b"older local copy");
}

#[tokio::test]
async fn one_damaged_chunk_does_not_make_the_passphrase_wrong() {
    let env = Env::new().await;
    let m = &env.mock;
    m.put_text("a.md", &["aaa"]);
    m.put_text("b.md", &["bbb"]);
    m.put_text("c.md", &["ccc"]);
    let mut ids: Vec<String> = ["a.md", "b.md", "c.md"]
        .iter()
        .map(|p| m.doc(p).unwrap()["children"][0].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    // Damage the first chunk in key order (the first the check reads).
    let mut chunk = m.doc(&ids[0]).unwrap();
    chunk["data"] = json!(format!("%={}", "A".repeat(80)));
    m.put(chunk);
    env.engine().pull_once().await.unwrap();
    assert_eq!(env.engine().status().unwrap().pending.len(), 1);
}

#[tokio::test]
async fn missing_sync_parameters_are_created_only_by_a_session_that_may_write() {
    let env = Env::new().await;
    let m = &env.mock;
    m.state
        .lock()
        .unwrap()
        .local
        .remove("obsidian_livesync_sync_parameters");
    // Pull-only: a clear refusal, no write.
    let r = refused(env.engine().pull_once().await);
    assert!(matches!(&r, Refusal::NoSyncParameters(..)), "{r:?}");
    assert!(r.to_string().contains("never writes"), "{r}");
    assert!(m.writes().is_empty());
    // Two-way, empty database: created as upstream's SyncParamsHandler does.
    env.write("first.md", b"the first note");
    env.engine().sync_once().await.unwrap();
    let params = m.local("obsidian_livesync_sync_parameters").unwrap();
    assert_eq!(params["type"], json!("sync-parameters"));
    assert_eq!(params["protocolVersion"], json!(2));
    let salt = livesync_crypto::parse_pbkdf2_salt(&params).unwrap();
    assert_eq!(salt.len(), 32);
    // What was pushed is encrypted with that salt: another device reads it.
    let key = livesync_crypto::KeyContext::derive(PASS, &salt);
    let cipher = livesync_crypto::DocumentCipher::new(key, false);
    let cid = env.doc("first.md")["children"][0].as_str().unwrap().to_string();
    let chunk = cipher.outgoing(m.doc(&cid).unwrap()).unwrap();
    assert_eq!(chunk["data"], json!("the first note"));

    // A salt-less database that holds encrypted chunks already is refused.
    let env = Env::new().await;
    env.mock.put_text("a.md", &["x"]);
    env.mock
        .state
        .lock()
        .unwrap()
        .local
        .remove("obsidian_livesync_sync_parameters");
    let r = refused(env.engine().sync_once().await);
    assert!(matches!(&r, Refusal::NoSyncParameters(..)), "{r:?}");
    assert!(r.to_string().contains("holds encrypted chunks"), "{r}");
    assert!(env.mock.writes().is_empty(), "{:?}", env.mock.writes());
}

#[tokio::test]
async fn the_key_is_derived_once_and_again_when_the_salt_changes() {
    let env = Env::new().await;
    env.mock.put_text("a.md", &["one"]);
    let e = env.engine();
    e.pull_once().await.unwrap();
    // The same engine (a daemon's regates) does not check the passphrase again.
    env.mock.clear_requests();
    e.connect_gate(false).await.unwrap();
    let reqs = env.mock.state.lock().unwrap().requests.clone();
    assert!(!reqs.iter().any(|(_, p)| p.contains("_all_docs")), "{reqs:?}");
    // A new salt (a rebuilt database): derived and checked again; the old
    // chunks no longer decrypt with it, so it is refused.
    env.mock.update_milestone(|_| {});
    env.mock.set_local(
        "obsidian_livesync_sync_parameters",
        json!({"_id": "_local/obsidian_livesync_sync_parameters", "_rev": "0-2", "type": "sync-parameters", "protocolVersion": 2, "pbkdf2salt": "b3RoZXItc2FsdC1vdGhlci1zYWx0LW90aGVyLXNhbHQ="}),
    );
    let r = refused(e.connect_gate(false).await);
    assert!(matches!(r, Refusal::WrongPassphrase(_)), "{r:?}");
}

#[tokio::test]
async fn the_daemon_syncs_an_encrypted_vault_both_ways() {
    let env = Env::new().await;
    real_shape(&env.mock);
    env.mock.put_text("remote.md", &["from elsewhere"]);
    let e = env.engine();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let run = tokio::spawn({
        let e = e.clone();
        async move { e.run(rx).await }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while env.read("remote.md").is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(env.read("remote.md").unwrap(), b"from elsewhere");
    env.write("local.md", b"written here");
    while env.mock.doc("local.md").is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tx.send(true).unwrap();
    run.await.unwrap().unwrap();
    assert_eq!(env.remote("local.md").unwrap(), b"written here");
    env.assert_all_chunks_sealed();
    // This node advertises the database's own tweaks (encryption on).
    let ms = env.mock.local("obsydian_livesync_milestone").unwrap();
    let node = e.node_id();
    assert_eq!(ms["tweak_values"][&node]["encrypt"], json!(true));
    assert_eq!(ms["tweak_values"][&node]["E2EEAlgorithm"], json!("v2"));
}

#[tokio::test]
async fn folders_are_listed_without_the_passphrase() {
    let env = Env::new().await;
    env.mock.put_text("Work/a.md", &["x"]);
    env.mock.put_text("Home/b.md", &["y"]);
    let l = livesync_engine::list_remote_folders(&env.couch()).await.unwrap();
    let names: Vec<&str> = l.folders.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["Home", "Work"], "{l:?}");
}

#[test]
fn the_mock_guard_refuses_plaintext_and_ciphertext_where_they_do_not_belong() {
    use common::guard_violation;
    // Encrypted database.
    for bad in [
        json!({"_id": "h:abc", "type": "leaf", "data": "x"}),
        json!({"_id": "h:+abc", "type": "leaf", "data": "x"}),
        json!({"_id": "a.md", "type": "plain", "children": ["h:abc"]}),
        json!({"_id": "a.md", "type": "plain", "children": ["h:+a"], "eden": {"h:+a": {"data": "x"}}}),
        json!({"_id": "n.md", "type": "notes", "data": "x"}),
    ] {
        assert!(guard_violation(&bad, true).is_some(), "{bad}");
    }
    assert!(guard_violation(
        &json!({"_id": "h:+abc", "type": "leaf", "data": "%=xx", "e_": true}),
        true
    )
    .is_none());
    // Plaintext database.
    for bad in [
        json!({"_id": "h:+abc", "type": "leaf", "data": "x"}),
        json!({"_id": "h:abc", "type": "leaf", "data": "%=x", "e_": true}),
        json!({"_id": "a.md", "type": "plain", "children": ["h:+a"]}),
    ] {
        assert!(guard_violation(&bad, false).is_some(), "{bad}");
    }
    assert!(guard_violation(&json!({"_id": "h:abc", "type": "leaf", "data": "x"}), false).is_none());
}
