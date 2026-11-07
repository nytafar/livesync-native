//! `Engine::adopt` against the mock CouchDB: a folder that already matches
//! the remote is taken over without fetching or writing anything for its
//! identical files, the report says what the first sync then does with the
//! rest (and the first sync does exactly that), and the mismatches that
//! must stop a take-over do.

mod common;

use common::{Mock, DB};
use livesync_engine::{AdoptOptions, AdoptReport, Engine, EngineConfig, Index, State};
use livesync_model::path::path_to_id;
use livesync_model::splitter::{chunk_file, SplitParams};
use livesync_remote::{Couch, RemoteConfig};
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

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
    fn engine(&self) -> Engine {
        self.engine_with(|_| {})
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
        cfg.batch_limit = 50;
        cfg.rebuild_quiet = Duration::ZERO;
        f(&mut cfg);
        Engine::open(couch, cfg).unwrap()
    }
    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.vault().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    fn set_mtime(&self, rel: &str, t: SystemTime) {
        let f = std::fs::File::options()
            .write(true)
            .open(self.vault().join(rel))
            .unwrap();
        f.set_modified(t).unwrap();
    }
    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.vault().join(rel)).ok()
    }
    /// A remote file split as this client splits it (the mock's tweaks).
    fn put_file(&self, path: &str, bytes: &[u8]) -> String {
        let params = SplitParams::from_tweaks(common::preferred().as_object().unwrap());
        let c = chunk_file(path, bytes, &params);
        let pieces: Vec<&str> = c.chunks.iter().map(|(_, d)| d.as_str()).collect();
        let children = self.mock.put_chunks(&pieces);
        let kind = match c.kind {
            livesync_model::FileKind::Plain => "plain",
            livesync_model::FileKind::Binary => "newnote",
        };
        self.mock.put(json!({
            "_id": path_to_id(path, true), "path": path, "type": kind, "children": children,
            "ctime": 1_700_000_000_000i64, "mtime": 1_700_000_100_000i64, "size": c.size, "eden": {}
        }))
    }
    /// The same file on both sides.
    fn both(&self, path: &str, bytes: &[u8]) {
        self.put_file(path, bytes);
        self.write(path, bytes);
    }
}

fn paths(l: &[livesync_engine::AdoptEntry]) -> Vec<&str> {
    l.iter().map(|e| e.path.as_str()).collect()
}

fn binary(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32)
        .map(|i| (i.wrapping_add(seed).wrapping_mul(2654435761) >> 13) as u8)
        .collect()
}

#[tokio::test]
async fn a_matching_folder_is_adopted_without_fetching_or_writing_anything() {
    let env = Env::new().await;
    let text = "# A note\n\nWith some æøå text and a list:\n- one\n- two\n".repeat(300);
    env.both("Notes/Long.md", text.as_bytes());
    env.both("Notes/Short.md", b"short");
    env.both("img/pic.png", &binary(200_000, 1));
    env.both("empty.md", b"");
    let rep: AdoptReport = env.engine().adopt(AdoptOptions::dry_run()).await.unwrap();
    assert_eq!((rep.identical, rep.identical_by_content), (4, 0), "{rep:#?}");
    assert_eq!((rep.local_files, rep.remote_files), (4, 4));
    assert!(rep.differing.is_empty() && rep.only_local.is_empty() && rep.only_remote.is_empty());
    assert_eq!(rep.chunks_fetched, 0, "identical chunk lists need no chunk");
    assert!(rep.refusal(false).is_none());
    assert!(!rep.applied);
    // A dry run records nothing.
    let ix = Index::open(&env.index_path()).unwrap();
    assert!(ix.all_rows().unwrap().is_empty());
    drop(ix);

    let writes_before = env.mock.writes().len();
    let rep = env.engine().adopt(AdoptOptions::apply()).await.unwrap();
    assert!(rep.applied);
    assert_eq!(
        env.mock.writes().len(),
        writes_before,
        "adopt never writes to the remote"
    );
    let row = Index::open(&env.index_path())
        .unwrap()
        .by_doc(&path_to_id("Notes/Long.md", true))
        .unwrap()
        .unwrap();
    assert_eq!(row.state, State::Clean);
    assert_eq!(
        row.base_rev.as_deref(),
        Some(
            env.mock.doc(&path_to_id("Notes/Long.md", true)).unwrap()["_rev"]
                .as_str()
                .unwrap()
        )
    );

    // The first sync fetches and writes nothing for them, and pushes nothing.
    let s = env.engine().sync_once().await.unwrap();
    assert_eq!(
        (s.pull.written, s.pull.adopted, s.pull.chunks_fetched),
        (0, 0, 0),
        "{s:#?}"
    );
    assert_eq!(
        (s.push.pushed, s.push.conflicts, s.push.chunks_uploaded),
        (0, 0, 0),
        "{s:#?}"
    );
    assert_eq!(s.pull.unchanged, 4);
    // And neither does the next one.
    let s = env.engine().sync_once().await.unwrap();
    assert_eq!(
        (s.pull.written, s.push.pushed, s.pull.chunks_fetched),
        (0, 0, 0),
        "{s:#?}"
    );
    assert_eq!(env.read("Notes/Long.md").unwrap(), text.as_bytes());
}

#[tokio::test]
async fn the_report_is_what_the_first_sync_does() {
    let env = Env::new().await;
    env.both("same.md", b"the same on both sides");
    env.both("also-same.md", b"also the same");
    // Differing content: a conflict with no common ancestor.
    env.put_file("differs.md", b"the remote's version");
    env.write("differs.md", b"the local version");
    // The same content, split another way (another client's splitter).
    let content = "split elsewhere ".repeat(20);
    env.mock.put_text("resplit.md", &[&content[..100], &content[100..]]);
    env.write("resplit.md", content.as_bytes());
    // Only one side.
    env.write("local-only.md", b"new here");
    env.put_file("remote-only.md", b"new there");
    // Deleted on the remote: the very version (removed here), and one
    // changed here after the deletion (put back).
    env.both("deleted-same.md", b"deleted elsewhere");
    env.mock.soft_delete("deleted-same.md");
    env.set_mtime(
        "deleted-same.md",
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    );
    env.both("deleted-edited.md", b"deleted elsewhere, edited here");
    env.mock.soft_delete("deleted-edited.md");
    env.set_mtime(
        "deleted-edited.md",
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_900_000_000),
    );
    // Gone outright (a CouchDB tombstone).
    env.both("tombstoned.md", b"gone");
    env.mock.tombstone("tombstoned.md");

    let rep = env.engine().adopt(AdoptOptions::dry_run()).await.unwrap();
    assert_eq!(rep.identical, 3, "{rep:#?}");
    assert_eq!(rep.identical_by_content, 1);
    assert!(rep.chunks_fetched > 0);
    assert_eq!(paths(&rep.differing), ["differs.md"]);
    assert_eq!(paths(&rep.only_remote), ["remote-only.md"]);
    assert_eq!(paths(&rep.only_local), ["local-only.md", "tombstoned.md"]);
    assert!(rep.only_local[0].note.is_none());
    assert!(rep.only_local[1]
        .note
        .as_deref()
        .unwrap()
        .contains("deleted on the remote"));
    assert_eq!(paths(&rep.deleted_remotely), ["deleted-edited.md", "deleted-same.md"]);
    assert!(rep.deleted_remotely[0].note.as_deref().unwrap().contains("pushed back"));
    assert!(rep.deleted_remotely[1]
        .note
        .as_deref()
        .unwrap()
        .contains("removed here"));
    assert!(rep.refusal(false).is_none());

    let rep = env.engine().adopt(AdoptOptions::apply()).await.unwrap();
    assert_eq!(rep.identical, 3);
    let s = env.engine().sync_once().await.unwrap();
    // Only the remote-only file is written; nothing identical is fetched again.
    assert_eq!(s.pull.written, 1, "{s:#?}");
    assert_eq!(env.read("remote-only.md").unwrap(), b"new there");
    // The differing file: our version beside theirs, as a conflict.
    assert_eq!(s.push.conflicts, 1, "{s:#?}");
    let differs = env.mock.doc(&path_to_id("differs.md", true)).unwrap();
    assert_eq!(differs["_conflicts"].as_array().map(Vec::len), Some(1), "{differs}");
    assert_eq!(
        env.read("differs.md").unwrap(),
        b"the local version",
        "the local file stays"
    );
    // Local-only and tombstoned are pushed as new; the edited deletion comes back.
    assert_eq!(
        env.mock.content(&path_to_id("local-only.md", true)).unwrap(),
        b"new here"
    );
    assert_eq!(env.mock.content(&path_to_id("tombstoned.md", true)).unwrap(), b"gone");
    assert_eq!(
        env.mock.content(&path_to_id("deleted-edited.md", true)).unwrap(),
        b"deleted elsewhere, edited here"
    );
    // The deleted version is removed here.
    assert!(env.read("deleted-same.md").is_none());
    assert_eq!(env.read("resplit.md").unwrap(), content.as_bytes());

    // The next sync has nothing left to do.
    let s = env.engine().sync_once().await.unwrap();
    assert_eq!((s.pull.written, s.push.pushed, s.push.conflicts), (0, 0, 0), "{s:#?}");
}

#[tokio::test]
async fn a_folder_with_an_index_already_is_refused() {
    let env = Env::new().await;
    env.both("a.md", b"a");
    env.engine().sync_once().await.unwrap();
    let e = env
        .engine()
        .adopt(AdoptOptions::dry_run())
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("already tracks 1 files"), "{e}");
}

#[tokio::test]
async fn a_folder_that_matches_nothing_is_refused() {
    let env = Env::new().await;
    env.put_file("theirs.md", b"theirs");
    env.write("mine.md", b"mine");
    let rep = env.engine().adopt(AdoptOptions::dry_run()).await.unwrap();
    assert!(rep.refusal(false).unwrap().contains("right folder"), "{rep:?}");
    let rep = env.engine().adopt(AdoptOptions::apply()).await.unwrap();
    assert!(!rep.applied && rep.refusal(false).is_some());
    assert!(Index::open(&env.index_path()).unwrap().all_rows().unwrap().is_empty());
}

#[tokio::test]
async fn a_name_that_differs_only_in_case_is_refused() {
    let env = Env::new().await;
    std::fs::create_dir_all(env.vault()).unwrap();
    require_case_sensitive!(&env.vault());
    env.both("same.md", b"same");
    env.put_file("Notes/Case.md", b"case");
    env.write("notes/case.md", b"case");
    let rep = env.engine().adopt(AdoptOptions::dry_run()).await.unwrap();
    assert_eq!(paths(&rep.name_case), ["Notes/Case.md"]);
    assert_eq!(rep.name_case[0].local_path.as_deref(), Some("notes/case.md"));
    assert!(rep.refusal(false).unwrap().contains("only in case"));
    assert!(!env.engine().adopt(AdoptOptions::apply()).await.unwrap().applied);
    assert!(Index::open(&env.index_path()).unwrap().all_rows().unwrap().is_empty());
}

#[tokio::test]
async fn files_outside_the_vaults_filters_are_left_out() {
    let env = Env::new().await;
    env.both("kept.md", b"kept");
    env.both(".hidden/dot.md", b"dot");
    env.put_file("big.bin", &binary(10, 2));
    // Over the remote's size limit (set to 1 MB here).
    env.mock.update_milestone(|m| {
        m["tweak_values"]["PREFERRED"]["syncMaxSizeInMB"] = json!(1);
    });
    env.both("huge.png", &binary(1_200_000, 3));
    let rep = env.engine().adopt(AdoptOptions::dry_run()).await.unwrap();
    assert_eq!(rep.identical, 1, "{rep:#?}");
    assert_eq!(paths(&rep.only_remote), ["big.bin"]);
    assert_eq!(rep.skipped_remote.get("too-large"), Some(&1));
    assert_eq!(paths(&rep.not_synced), ["huge.png"]);
    assert!(rep.only_local.is_empty(), "dot-paths are not synced: {rep:#?}");
}

#[tokio::test]
async fn an_identical_file_whose_chunks_are_missing_on_the_remote_is_not_adopted() {
    let env = Env::new().await;
    env.both("kept.md", b"kept as it is");
    env.both("also.md", b"another one");
    env.both("gone.md", b"its chunk was compacted away");
    let gone_id = path_to_id("gone.md", true);
    let chunk = env.mock.doc(&gone_id).unwrap()["children"][0]
        .as_str()
        .unwrap()
        .to_string();
    env.mock.remove_doc(&chunk);
    let rep = env.engine().adopt(AdoptOptions::dry_run()).await.unwrap();
    assert_eq!(rep.identical, 2, "{rep:#?}");
    assert_eq!(paths(&rep.undecided), ["gone.md"]);
    let note = rep.undecided[0].note.as_deref().unwrap();
    assert!(note.contains("1 of 1 chunks missing"), "{note}");
    assert!(rep.refusal(false).is_none());

    let rep = env.engine().adopt(AdoptOptions::apply()).await.unwrap();
    assert!(rep.applied);
    let ix = Index::open(&env.index_path()).unwrap();
    assert!(ix.by_doc(&gone_id).unwrap().is_none(), "not recorded as synced");
    assert!(ix.by_doc(&path_to_id("kept.md", true)).unwrap().is_some());
    drop(ix);
    // The first sync is free to put the chunk back: nothing claims the
    // remote has it.
    env.engine().sync_once().await.unwrap();
    assert!(env.mock.doc(&chunk).is_some(), "the missing chunk is uploaded again");
    assert_eq!(env.read("gone.md").unwrap(), b"its chunk was compacted away");
}

#[tokio::test]
async fn a_weak_match_is_taken_over_only_when_accepted() {
    let env = Env::new().await;
    env.both("same.md", b"the one file that matches");
    for i in 0..3 {
        env.put_file(&format!("n{i}.md"), format!("theirs {i}").as_bytes());
        env.write(&format!("n{i}.md"), format!("mine {i}").as_bytes());
    }
    let rep = env.engine().adopt(AdoptOptions::dry_run()).await.unwrap();
    assert_eq!((rep.identical, rep.differing.len()), (1, 3), "{rep:#?}");
    assert!(
        rep.weak_match.as_deref().unwrap().contains("only 1 of the 4 local"),
        "{rep:#?}"
    );
    let why = rep.refusal(false).unwrap();
    assert!(why.contains("--accept-weak-match"), "{why}");
    assert!(rep.refusal(true).is_none());
    let rep = env.engine().adopt(AdoptOptions::apply()).await.unwrap();
    assert!(!rep.applied);
    assert!(Index::open(&env.index_path()).unwrap().all_rows().unwrap().is_empty());
    let rep = env
        .engine()
        .adopt(AdoptOptions {
            apply: true,
            accept_weak_match: true,
            ..AdoptOptions::default()
        })
        .await
        .unwrap();
    assert!(rep.applied);
    assert_eq!(Index::open(&env.index_path()).unwrap().all_rows().unwrap().len(), 1);
}

#[tokio::test]
async fn empty_files_alone_are_a_weak_match() {
    let env = Env::new().await;
    env.both("empty.md", b"");
    env.put_file("theirs.md", b"theirs");
    env.write("mine.md", b"mine");
    let rep = env.engine().adopt(AdoptOptions::dry_run()).await.unwrap();
    assert_eq!((rep.identical, rep.identical_empty), (1, 1), "{rep:#?}");
    assert!(rep.refusal(false).unwrap().contains("accept-weak-match"), "{rep:#?}");
    // A match that is not weak.
    let env = Env::new().await;
    env.both("a.md", b"a");
    env.both("b.md", b"b");
    env.write("mine.md", b"mine");
    let rep = env.engine().adopt(AdoptOptions::dry_run()).await.unwrap();
    assert!(rep.weak_match.is_none() && rep.refusal(false).is_none(), "{rep:#?}");
}

#[tokio::test]
async fn a_name_in_another_unicode_normalisation_is_refused() {
    let env = Env::new().await;
    env.both("a.md", b"a");
    env.both("b.md", b"b");
    env.put_file("caf\u{e9}.md", b"coffee");
    env.write("cafe\u{301}.md", b"coffee");
    // A filesystem that normalises names (or folds them) has one file here.
    let names: Vec<String> = std::fs::read_dir(env.vault())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    if !names.iter().any(|n| n == "cafe\u{301}.md") {
        eprintln!("SKIPPED: this filesystem normalises names");
        return;
    }
    let rep = env.engine().adopt(AdoptOptions::dry_run()).await.unwrap();
    assert_eq!(paths(&rep.name_case), ["caf\u{e9}.md"], "{rep:#?}");
    assert_eq!(rep.name_case[0].local_path.as_deref(), Some("cafe\u{301}.md"));
    assert!(rep.only_local.is_empty() && rep.only_remote.is_empty(), "{rep:#?}");
    assert!(rep.refusal(false).unwrap().contains("Unicode normalisation"));
    assert!(!env.engine().adopt(AdoptOptions::apply()).await.unwrap().applied);
}

#[tokio::test]
async fn a_pull_only_vault_gets_pull_notes() {
    let env = Env::new().await;
    env.both("same.md", b"same");
    env.both("deleted.md", b"deleted elsewhere");
    env.mock.soft_delete("deleted.md");
    env.both("tombstoned.md", b"gone");
    env.mock.tombstone("tombstoned.md");
    let opts = AdoptOptions {
        pull_only: true,
        ..AdoptOptions::default()
    };
    let rep = env.engine().adopt(opts).await.unwrap();
    assert_eq!(paths(&rep.deleted_remotely), ["deleted.md"]);
    let note = rep.deleted_remotely[0].note.as_deref().unwrap();
    assert!(note.contains("kept here") && note.contains("pull-only"), "{note}");
    let note = rep.only_local[0].note.as_deref().unwrap();
    assert!(note.contains("kept here") && !note.contains("pushed"), "{note}");
    // And the first pull does keep them.
    let mut opts = opts;
    opts.apply = true;
    opts.accept_weak_match = true;
    let engine = env.engine();
    assert!(engine.adopt(opts).await.unwrap().applied);
    engine.pull_once().await.unwrap();
    assert!(env.read("deleted.md").is_some() && env.read("tombstoned.md").is_some());
}

#[tokio::test]
async fn a_local_file_over_the_size_limit_is_listed_once() {
    let env = Env::new().await;
    env.both("kept.md", b"kept");
    env.mock.update_milestone(|m| {
        m["tweak_values"]["PREFERRED"]["syncMaxSizeInMB"] = json!(1);
    });
    env.put_file("big.png", &binary(1000, 4));
    env.write("big.png", &binary(1_200_000, 5));
    let rep = env.engine().adopt(AdoptOptions::dry_run()).await.unwrap();
    assert_eq!(paths(&rep.not_synced), ["big.png"], "{rep:#?}");
    assert!(rep.only_remote.is_empty(), "{rep:#?}");
    let note = rep.not_synced[0].note.as_deref().unwrap();
    assert!(note.contains("not applied"), "{note}");
}

#[tokio::test]
async fn a_real_adopt_beside_the_plugin_in_any_config_folder_is_refused() {
    let env = Env::new().await;
    env.both("a.md", b"a");
    let cfg = env.vault().join(".obsidian-mobile");
    std::fs::create_dir_all(cfg.join("plugins/obsidian-livesync")).unwrap();
    std::fs::write(cfg.join("community-plugins.json"), r#"["obsidian-livesync"]"#).unwrap();
    // A dry run only compares.
    assert_eq!(env.engine().adopt(AdoptOptions::dry_run()).await.unwrap().identical, 1);
    let e = env.engine().adopt(AdoptOptions::apply()).await.unwrap_err();
    assert!(e.to_string().contains(".obsidian-mobile"), "{e}");
    assert!(Index::open(&env.index_path()).unwrap().all_rows().unwrap().is_empty());
    // A deliberate handover.
    let rep = env
        .engine_with(|c| c.allow_plugin = true)
        .adopt(AdoptOptions::apply())
        .await
        .unwrap();
    assert!(rep.applied);
}
