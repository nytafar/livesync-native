//! What a Mac's vault folder does that a Linux one does not, run on any
//! filesystem: Unicode normalisation folded by APFS and HFS+ (through the
//! probe seam, `EngineConfig::fs_probe`), privacy blocks (TCC) and iCloud
//! Drive's evicted files.

mod common;

use common::{Mock, DB};
use livesync_engine::engine::FsProbe;
use livesync_engine::fsrules::FsRules;
use livesync_engine::{AdoptOptions, Engine, EngineConfig};
use livesync_model::path::path_to_id;
use livesync_remote::{Couch, RemoteConfig};
use serde_json::{json, Value};
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
    fn couch(&self) -> Couch {
        Couch::new(&RemoteConfig {
            url: self.mock.url.clone(),
            database: DB.into(),
            username: Some("u".into()),
            password: Some("p".into()),
        })
        .unwrap()
    }
    fn config(&self) -> EngineConfig {
        let mut cfg = EngineConfig::new(self.vault(), self.index_path());
        cfg.batch_limit = 5;
        cfg.concurrency = 2;
        cfg.rebuild_quiet = Duration::ZERO;
        cfg
    }
    /// The engine as on a filesystem with `rules` (lookups on the disk stay
    /// this filesystem's).
    fn engine_on(&self, rules: &FsRules) -> Engine {
        let mut cfg = self.config();
        let r = rules.clone();
        cfg.fs_probe = Some(FsProbe::new(move |_| Ok(r.clone())));
        Engine::open(self.couch(), cfg).unwrap()
    }
    fn engine(&self) -> Engine {
        Engine::open(self.couch(), self.config()).unwrap()
    }
    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.vault().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.vault().join(rel)).ok()
    }
    /// Rename, and drop the old folders it leaves empty.
    fn mv(&self, from: &str, to: &str) {
        let t = self.vault().join(to);
        std::fs::create_dir_all(t.parent().unwrap()).unwrap();
        std::fs::rename(self.vault().join(from), t).unwrap();
        let mut at = self.vault().join(from);
        while at.pop() && at != self.vault() {
            if std::fs::remove_dir(&at).is_err() {
                break;
            }
        }
    }
    fn doc(&self, path: &str) -> Option<Value> {
        self.mock.doc(&path_to_id(path, true))
    }
    fn remote(&self, path: &str) -> Option<Vec<u8>> {
        self.mock.content(&path_to_id(path, true))
    }
    fn deleted(&self, path: &str) -> bool {
        self.doc(path).is_some_and(|d| d.get("deleted") == Some(&json!(true)))
    }
    /// Writes other than the milestone's.
    fn doc_writes(&self) -> Vec<(String, String)> {
        self.mock
            .writes()
            .into_iter()
            .filter(|(_, p)| !p.contains("_local/"))
            .collect()
    }
}

/// APFS as macOS formats it, and HFS+: case and Unicode normalisation fold.
fn apfs() -> FsRules {
    FsRules {
        case_insensitive: true,
        normalization_insensitive: true,
        ..FsRules::permissive()
    }
}

/// Case-sensitive APFS: normalisation folds, case does not.
fn apfs_case_sensitive() -> FsRules {
    FsRules {
        normalization_insensitive: true,
        ..FsRules::permissive()
    }
}

const NAMES: [(&str, &str); 3] = [
    ("\u{e5}l.md", "a\u{30a}l.md"),
    ("Caf\u{e9}/caf\u{e9}.md", "Cafe\u{301}/cafe\u{301}.md"),
    ("\u{fc}ber.md", "u\u{308}ber.md"),
];

/// A local name that differs from the remote path only in Unicode
/// normalisation (NFD on disk, as HFS+ stores names and the Finder may
/// write them; NFC on the remote) is that document's file where the probe
/// says the filesystem folds normalisation: no second document, no delete
/// and re-create, no conflict.
#[tokio::test]
async fn an_nfd_name_is_the_nfc_documents_file_where_names_fold() {
    for (nfc, nfd) in NAMES {
        let env = Env::new().await;
        env.mock.put_text(nfc, &["remote text"]);
        env.engine_on(&apfs()).sync_once().await.unwrap();
        assert_eq!(env.read(nfc).unwrap(), b"remote text");
        // The filesystem hands the name back in NFD.
        env.mv(nfc, nfd);
        env.mock.clear_requests();
        let rep = env.engine_on(&apfs()).sync_once().await.unwrap();
        assert_eq!(
            rep.push.pushed + rep.push.deleted + rep.push.conflicts,
            0,
            "{nfd}: {rep:?}"
        );
        assert!(env.doc_writes().is_empty(), "{nfd}: {:?}", env.doc_writes());
        assert_eq!(env.mock.live_paths(), [nfc]);
        // An edit under the NFD name updates the NFC document.
        env.write(nfd, b"edited on the Mac");
        let rep = env.engine_on(&apfs()).sync_once().await.unwrap();
        assert_eq!(
            (rep.push.pushed, rep.push.created, rep.push.conflicts, rep.push.deleted),
            (1, 0, 0, 0),
            "{nfd}: {rep:?}"
        );
        assert_eq!(env.mock.live_paths(), [nfc]);
        assert_eq!(env.remote(nfc).unwrap(), b"edited on the Mac");
        assert!(env.doc(nfd).is_none(), "{nfd}: a second document");
    }
}

/// Where names do not fold (Linux), two spellings are two names, and the
/// same move is a rename: what the probe says decides, not the platform.
#[tokio::test]
async fn an_nfd_name_is_a_rename_where_names_do_not_fold() {
    let (nfc, nfd) = NAMES[0];
    let env = Env::new().await;
    std::fs::create_dir_all(env.vault()).unwrap();
    if FsRules::probe(&env.vault()).unwrap().normalization_insensitive {
        // The disk itself finds the NFD name by the NFC one (APFS).
        eprintln!("SKIPPED: this filesystem folds Unicode normalisation");
        return;
    }
    env.mock.put_text(nfc, &["remote text"]);
    env.engine_on(&FsRules::permissive()).sync_once().await.unwrap();
    env.mv(nfc, nfd);
    let rep = env.engine_on(&FsRules::permissive()).sync_once().await.unwrap();
    assert_eq!(env.mock.live_paths(), [nfd], "{rep:?}");
    assert!(env.deleted(nfc));
}

/// Case-sensitive APFS folds normalisation but not case: names that differ
/// in case are two files, and a case-only rename is a rename.
#[tokio::test]
async fn a_filesystem_that_folds_only_normalisation_keeps_case_apart() {
    let env = Env::new().await;
    if !common::case_sensitive(env.dir.path()) {
        eprintln!("SKIPPED: this filesystem folds case");
        return;
    }
    env.mock
        .update_milestone(|m| m["tweak_values"]["PREFERRED"]["handleFilenameCaseSensitive"] = json!(true));
    let put = |path: &str, body: &str| {
        let ch = env.mock.put_chunks(&[body]);
        env.mock
            .put(json!({"_id": path, "path": path, "type": "plain", "children": ch,
            "mtime": 1, "ctime": 1, "size": body.len(), "eden": {}}))
    };
    put("Note.md", "upper");
    put("note.md", "lower");
    let fs = apfs_case_sensitive();
    let rep = env.engine_on(&fs).sync_once().await.unwrap();
    assert!(rep.pull.skipped.is_empty(), "no collision on this filesystem: {rep:?}");
    assert_eq!(env.read("Note.md").unwrap(), b"upper");
    assert_eq!(env.read("note.md").unwrap(), b"lower");
    // A case-only rename reaches the remote.
    env.write("Draft.md", b"draft");
    env.engine_on(&fs).sync_once().await.unwrap();
    env.mv("Draft.md", "draft.md");
    env.engine_on(&fs).sync_once().await.unwrap();
    let live = env.mock.live_paths();
    assert!(
        live.contains(&"draft.md".to_string()) && !live.contains(&"Draft.md".to_string()),
        "{live:?}"
    );
    // NFD and NFC still name one file.
    let (nfc, nfd) = NAMES[0];
    env.write(nfc, b"x");
    env.engine_on(&fs).sync_once().await.unwrap();
    env.mv(nfc, nfd);
    let rep = env.engine_on(&fs).sync_once().await.unwrap();
    assert_eq!(rep.push.pushed + rep.push.deleted, 0, "{rep:?}");
    assert!(env.mock.live_paths().contains(&nfc.to_string()));
}

/// Two remote documents whose paths differ only in normalisation (made on
/// Linux) cannot both be files where names fold: one is written, the other
/// is reported as a collision, and neither overwrites the other.
#[tokio::test]
async fn two_remote_spellings_of_one_name_collide_where_names_fold() {
    let (nfc, nfd) = NAMES[0];
    let env = Env::new().await;
    env.mock.put_text(nfc, &["composed"]);
    env.mock.put_text(nfd, &["decomposed"]);
    let rep = env.engine_on(&apfs()).pull_once().await.unwrap();
    assert_eq!(rep.skipped.get("case-collision"), Some(&1), "{rep:?}");
    assert_eq!(rep.written, 1);
}

/// Adopting a Mac's folder: a local NFD name for a remote NFC document is
/// compared with it (the first pull writes into that very file), not
/// refused as a second file.
#[tokio::test]
async fn adopt_matches_an_nfd_name_with_its_nfc_document_where_names_fold() {
    let (nfc, nfd) = NAMES[1];
    let env = Env::new().await;
    for (p, body) in [("a.md", "a"), ("b.md", "b"), (nfc, "coffee")] {
        env.mock.put_text(p, &[body]);
    }
    env.write("a.md", b"a");
    env.write("b.md", b"b");
    env.write(nfd, b"coffee");
    let rep = env.engine_on(&apfs()).adopt(AdoptOptions::dry_run()).await.unwrap();
    assert!(rep.name_case.is_empty(), "{rep:#?}");
    assert_eq!(rep.identical, 3, "{rep:#?}");
    assert!(rep.only_local.is_empty() && rep.only_remote.is_empty(), "{rep:#?}");
    // Where names do not fold, the same folder is refused (two files).
    let rep = env
        .engine_on(&FsRules::permissive())
        .adopt(AdoptOptions::dry_run())
        .await
        .unwrap();
    assert_eq!(rep.name_case.len(), 1, "{rep:#?}");
}

/// A vault folder this process may not read (macOS answers EPERM without a
/// privacy grant; EACCES stands in for it here) is "no access": the engine
/// does not start, so nothing in it can look deleted, and the message says
/// so. Once it can read the folder again, nothing was lost.
#[cfg(unix)]
#[tokio::test]
async fn a_vault_without_access_is_refused_never_taken_for_empty() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new().await;
    for i in 0..30 {
        env.write(&format!("n{i}.md"), b"x");
    }
    env.engine().sync_once().await.unwrap();
    let mode = |m| std::fs::set_permissions(env.vault(), std::fs::Permissions::from_mode(m)).unwrap();
    mode(0o000);
    if std::fs::read_dir(env.vault()).is_ok() {
        mode(0o755);
        eprintln!("SKIPPED: root reads anything");
        return;
    }
    let opened = Engine::open(env.couch(), env.config());
    mode(0o755);
    let err = opened.err().expect("opened a vault it cannot read").to_string();
    assert!(err.contains("no access to the vault folder"), "{err}");
    assert!(err.contains("no file in it counts as deleted"), "{err}");
    assert_eq!(env.mock.live_paths().len(), 30);
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 0, "{rep:?}");
    assert_eq!(env.mock.live_paths().len(), 30);
}

/// A file evicted to iCloud Drive (only `.NAME.icloud` is left beside it)
/// is not a deleted file.
#[tokio::test]
async fn a_file_evicted_to_icloud_is_not_deleted() {
    let env = Env::new().await;
    env.write("a.md", b"a");
    env.write("b.md", b"b");
    env.engine().sync_once().await.unwrap();
    std::fs::remove_file(env.vault().join("a.md")).unwrap();
    env.write(".a.md.icloud", b"placeholder");
    let rep = env.engine().sync_once().await.unwrap();
    assert_eq!(rep.push.deleted, 0, "{rep:?}");
    assert!(!env.deleted("a.md"));
    let st = env.engine().status().unwrap();
    assert!(
        st.local_issues
            .iter()
            .any(|i| i.path == "a.md" && i.note.contains("iCloud")),
        "{:?}",
        st.local_issues
    );
    // Gone for real, placeholder and all: deleted.
    std::fs::remove_file(env.vault().join(".a.md.icloud")).unwrap();
    env.engine().sync_once().await.unwrap();
    assert!(env.deleted("a.md"));
}
