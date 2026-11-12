//! `lsync import` then `lsync adopt` through the binary, on the engine's
//! mock CouchDB, in a machine of temp folders: the plugin's file becomes a
//! vault, the dry run reports and changes nothing (not even the state
//! folder), the plugin still enabled stops a real take-over, a real one
//! adopts the identical files and runs the first sync, and the next sync
//! has nothing to do.

#[path = "../../livesync-engine/tests/common/mod.rs"]
mod common;

use common::{Mock, DB};
use livesync_model::path::path_to_id;
use livesync_model::splitter::{chunk_file, SplitParams};
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Machine {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["home", "config", "state", "run"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Machine { _dir: dir, root }
    }

    fn vault(&self) -> PathBuf {
        self.root.join("Notes")
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.vault().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lsync"));
        c.env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("LSYNC_RUNTIME_DIR", self.root.join("run"))
            .env("RUST_LOG", "warn")
            .current_dir(&self.root)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        if let Some(s) = stdin {
            input.write_all(s.as_bytes()).unwrap();
        }
        drop(input);
        child.wait_with_output().unwrap()
    }

    fn code(&self, args: &[&str]) -> (i32, String, String) {
        let out = self.run(args, None);
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.code(args);
        assert_eq!(code, 0, "lsync {args:?}:\n{out}\n{err}");
        out
    }

    /// The plugin's data.json in the vault, pointing at the mock.
    fn import(&self, mock: &Mock, plugin_enabled: bool) {
        let plugin = self.vault().join(".obsidian/plugins/obsidian-livesync");
        std::fs::create_dir_all(&plugin).unwrap();
        let data = json!({
            "settingVersion": 10, "remoteType": "", "configPassphraseStore": "",
            "couchDB_URI": mock.url, "couchDB_USER": "u", "couchDB_PASSWORD": "synthetic-pw",
            "couchDB_DBNAME": DB, "encryptedCouchDBConnection": "", "remoteConfigurations": {},
            "activeConfigurationId": "", "encrypt": false, "usePathObfuscation": false,
            "hashAlg": "xxhash64", "syncIgnoreRegEx": "", "syncOnlyRegEx": "", "useIgnoreFiles": false,
            "ignoreFiles": ".gitignore"
        });
        std::fs::write(plugin.join("data.json"), data.to_string()).unwrap();
        if plugin_enabled {
            std::fs::write(
                self.vault().join(".obsidian/community-plugins.json"),
                r#"["obsidian-livesync"]"#,
            )
            .unwrap();
        }
        let out = self.ok(&["import", plugin.join("data.json").to_str().unwrap(), "--name", "notes"]);
        assert!(out.contains("added vault notes"), "{out}");
    }

    fn index(&self) -> PathBuf {
        self.root.join("state/lsync/notes/index.sqlite")
    }
}

/// A remote file split as this client splits it (the mock's tweaks).
fn put_file(mock: &Mock, path: &str, bytes: &[u8]) {
    let params = SplitParams::from_tweaks(common::preferred().as_object().unwrap());
    let c = chunk_file(path, bytes, &params);
    let pieces: Vec<&str> = c.chunks.iter().map(|(_, d)| d.as_str()).collect();
    let children = mock.put_chunks(&pieces);
    mock.put(json!({
        "_id": path_to_id(path, true), "path": path, "type": "plain", "children": children,
        "ctime": 1_700_000_000_000i64, "mtime": 1_700_000_100_000i64, "size": c.size, "eden": {}
    }));
}

fn both(m: &Machine, mock: &Mock, path: &str, text: &str) {
    put_file(mock, path, text.as_bytes());
    m.write(path, text.as_bytes());
}

#[tokio::test(flavor = "multi_thread")]
async fn import_then_adopt_takes_the_folder_over_without_transfers() {
    let mock = Mock::start().await;
    let m = Machine::new();
    for i in 0..12 {
        both(
            &m,
            &mock,
            &format!("Daily/2026-11-{i:02}.md"),
            &format!("# Day {i}\n\n{}", "text ".repeat(50 * i)),
        );
    }
    put_file(&mock, "Inbox/remote.md", b"only on the server");
    m.write("Inbox/local.md", b"only here");
    put_file(&mock, "differs.md", b"the server's version");
    m.write("differs.md", b"the local version");
    m.import(&mock, true);

    // A dry run: the report, nothing changed (no state folder at all).
    let (code, out, err) = m.code(&["--vault", "notes", "adopt", "--dry-run"]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("identical         12"), "{out}");
    assert!(out.contains("differing         1"), "{out}");
    assert!(out.contains("differs.md"), "{out}");
    assert!(
        out.contains("only here         1") && out.contains("only on server    1"),
        "{out}"
    );
    assert!(out.contains("WARNING: the LiveSync plugin is still enabled"), "{out}");
    assert!(
        !m.root.join("state/lsync/notes").exists(),
        "a dry run keeps out of the state folder"
    );
    let v: Value = serde_json::from_str(&m.ok(&["--vault", "notes", "adopt", "--dry-run", "--json"])).unwrap();
    assert_eq!(v["identical"], 12);
    assert_eq!(v["differing"][0]["path"], "differs.md");
    assert_eq!(v["only_local"][0]["path"], "Inbox/local.md");
    assert_eq!(v["only_remote"][0]["path"], "Inbox/remote.md");
    assert_eq!(
        v["chunks_fetched"].as_u64(),
        Some(1),
        "only the differing file's chunk: {v}"
    );
    assert_eq!(v["dry_run"], true);

    // The plugin still enabled here stops the real thing, also when
    // another config folder enables it.
    let (code, _, err) = m.code(&["--vault", "notes", "adopt"]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("adopt refused") && err.contains("LiveSync plugin enabled") && err.contains("--allow-plugin"),
        "{err}"
    );
    std::fs::write(m.vault().join(".obsidian/community-plugins.json"), "[]").unwrap();
    let mobile = m.vault().join(".obsidian-mobile");
    std::fs::create_dir_all(mobile.join("plugins/obsidian-livesync")).unwrap();
    std::fs::write(mobile.join("community-plugins.json"), r#"["obsidian-livesync"]"#).unwrap();
    let (code, _, err) = m.code(&["--vault", "notes", "adopt"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains(".obsidian-mobile"), "{err}");
    assert!(
        livesync_engine::Index::open(&m.index())
            .unwrap()
            .all_rows()
            .unwrap()
            .is_empty(),
        "nothing recorded"
    );
    std::fs::remove_dir_all(&mobile).unwrap();

    let writes_before = mock.writes().len();
    // Exit 3, as `sync --json`: the differing file is kept here (dirty).
    let (code, out, err) = m.code(&["--vault", "notes", "adopt", "--json"]);
    assert_eq!(code, 3, "{out}\n{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["applied"], true, "{v}");
    assert_eq!(v["identical"], 12);
    assert_eq!(v["first_sync"]["pull"]["dirty"], 1, "{v}");
    let first = &v["first_sync"];
    assert_eq!(first["pull"]["written"], 1, "only the server-only file: {first}");
    assert_eq!(first["pull"]["chunks_fetched"].as_u64(), Some(2), "{first}");
    // The local-only file, and the differing one as a root of its own.
    assert_eq!(first["push"]["created"], 2, "{first}");
    assert_eq!(first["push"]["conflicts"], 1, "{first}");
    assert!(mock.writes().len() > writes_before);
    assert!(m.index().exists());

    // The next sync has nothing to do; adopting again is refused.
    let s: Value = serde_json::from_str(&m.ok(&["--vault", "notes", "sync", "--json"])).unwrap();
    assert_eq!(s["pull"]["written"], 0, "{s}");
    assert_eq!(s["pull"]["chunks_fetched"], 0, "{s}");
    assert_eq!(s["push"]["pushed"], 0, "{s}");
    let (code, _, err) = m.code(&["--vault", "notes", "adopt", "--dry-run"]);
    assert_ne!(code, 0);
    assert!(err.contains("already tracks"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_that_matches_nothing_is_not_taken_over() {
    let mock = Mock::start().await;
    let m = Machine::new();
    put_file(&mock, "theirs.md", b"theirs");
    m.write("mine.md", b"mine");
    m.import(&mock, false);
    let (code, out, _) = m.code(&["--vault", "notes", "adopt", "--dry-run"]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("a real adopt would stop") && out.contains("right folder"),
        "{out}"
    );
    let (code, out, _) = m.code(&["--vault", "notes", "adopt"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("NOT ADOPTED"), "{out}");
    assert!(mock.writes().is_empty(), "nothing written to the server");
    let rows = livesync_engine::Index::open(&m.index()).unwrap().all_rows().unwrap();
    assert!(rows.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_folder_is_refused() {
    let mock = Mock::start().await;
    let m = Machine::new();
    m.import(&mock, false);
    std::fs::remove_dir_all(m.vault()).unwrap();
    let (code, _, err) = m.code(&["--vault", "notes", "adopt", "--dry-run"]);
    assert_ne!(code, 0);
    assert!(err.contains("does not exist"), "{err}");
}

/// The machine's lsync.toml, with `f` applied to the vault `notes`.
fn edit_vault(m: &Machine, f: impl FnOnce(&mut toml::Table)) {
    let p = m.root.join("config/lsync/lsync.toml");
    let mut t: toml::Table = toml::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    f(t["vault"]["notes"].as_table_mut().unwrap());
    std::fs::write(&p, toml::to_string(&t).unwrap()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deliberate_handover_takes_the_flag_or_the_vaults_setting() {
    for how in ["flag", "vault"] {
        let mock = Mock::start().await;
        let m = Machine::new();
        both(&m, &mock, "a.md", "a");
        m.import(&mock, true);
        let mut args = vec!["--vault", "notes", "adopt", "--json"];
        match how {
            "flag" => args.push("--allow-plugin"),
            _ => edit_vault(&m, |v| {
                v.insert("allow_plugin".into(), true.into());
            }),
        }
        let v: Value = serde_json::from_str(&m.ok(&args)).unwrap();
        assert_eq!(v["applied"], true, "{how}: {v}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_weak_match_needs_to_be_accepted() {
    let mock = Mock::start().await;
    let m = Machine::new();
    both(&m, &mock, "same.md", "the one that matches");
    for i in 0..3 {
        put_file(&mock, &format!("n{i}.md"), format!("theirs {i}").as_bytes());
        m.write(&format!("n{i}.md"), format!("mine {i}").as_bytes());
    }
    m.import(&mock, false);
    let (code, out, _) = m.code(&["--vault", "notes", "adopt", "--dry-run"]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("WEAK MATCH") && out.contains("--accept-weak-match"),
        "{out}"
    );
    let (code, out, _) = m.code(&["--vault", "notes", "adopt"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("NOT ADOPTED"), "{out}");
    assert!(mock.writes().is_empty(), "nothing written to the server");
    // Applied; exit 3 for the three differing files the first sync keeps.
    let (code, out, err) = m.code(&["--vault", "notes", "adopt", "--accept-weak-match", "--json"]);
    assert_eq!(code, 3, "{out}\n{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["applied"], true, "{v}");
    assert!(v["weak_match"].is_string(), "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn adopt_exits_as_sync_does_when_the_first_sync_needs_attention() {
    for json in [true, false] {
        let mock = Mock::start().await;
        mock.update_milestone(|m| {
            m["tweak_values"]["PREFERRED"]["syncMaxSizeInMB"] = json!(1);
        });
        let m = Machine::new();
        both(&m, &mock, "a.md", "a");
        both(&m, &mock, "b.md", "b");
        // Over the size limit: a local issue of the first sync.
        m.write("big.md", &vec![b'x'; 1_200_000]);
        m.import(&mock, false);
        let mut args = vec!["--vault", "notes", "adopt"];
        if json {
            args.push("--json");
        }
        let (code, out, err) = m.code(&args);
        assert_eq!(code, 3, "json {json}:\n{out}\n{err}");
        if json {
            let v: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["applied"], true, "{v}");
        }
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_dot_folder_that_cannot_be_checked_stops_a_real_adopt_without_blaming_the_plugin() {
    use std::os::unix::fs::PermissionsExt;
    let mock = Mock::start().await;
    let m = Machine::new();
    both(&m, &mock, "a.md", "a");
    m.import(&mock, false);
    let snap = m.vault().join(".snapshots");
    std::fs::create_dir_all(snap.join("1")).unwrap();
    let mode = |mode: u32| std::fs::set_permissions(&snap, std::fs::Permissions::from_mode(mode)).unwrap();
    mode(0o000);
    // Root reads anything; the check is moot there.
    if std::fs::read_dir(&snap).is_ok() {
        mode(0o755);
        return;
    }
    let (code, out, err) = m.code(&["--vault", "notes", "adopt", "--dry-run"]);
    let (real, _, real_err) = m.code(&["--vault", "notes", "adopt"]);
    mode(0o755);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        out.contains("could not be checked for the LiveSync plugin") && !out.contains("still enabled"),
        "{out}"
    );
    assert_eq!(real, 2, "{real_err}");
    assert!(real_err.contains("could not be checked"), "{real_err}");
    assert!(livesync_engine::Index::open(&m.index())
        .unwrap()
        .all_rows()
        .unwrap()
        .is_empty());
}

/// An end-to-end encrypted database through the binary: the plugin's file
/// (passphrase in plain text, as older plugins saved it) imported, the
/// folder adopted and synced, the passphrase never shown; with a wrong one,
/// nothing happens.
#[tokio::test(flavor = "multi_thread")]
async fn an_encrypted_vault_is_imported_adopted_and_synced() {
    const PASS: &str = "synthetic-e2ee-passphrase (test only)";
    let mock = Mock::start_encrypted(PASS).await;
    let m = Machine::new();
    mock.put_text("remote.md", &["only on the encrypted server"]);
    m.write("local.md", b"only here");
    for i in 0..3 {
        both(
            &m,
            &mock,
            &format!("same-{i}.md"),
            &format!("# Same {i}\n{}", "text ".repeat(40 * i)),
        );
    }
    let plugin = m.vault().join(".obsidian/plugins/obsidian-livesync");
    std::fs::create_dir_all(&plugin).unwrap();
    let data = json!({
        "settingVersion": 10, "remoteType": "", "configPassphraseStore": "",
        "couchDB_URI": mock.url, "couchDB_USER": "u", "couchDB_PASSWORD": "synthetic-pw",
        "couchDB_DBNAME": DB, "encryptedCouchDBConnection": "", "remoteConfigurations": {},
        "activeConfigurationId": "", "encrypt": true, "passphrase": PASS, "E2EEAlgorithm": "v2",
        "usePathObfuscation": false, "hashAlg": "xxhash64", "syncIgnoreRegEx": "", "syncOnlyRegEx": "",
        "useIgnoreFiles": false, "ignoreFiles": ".gitignore"
    });
    std::fs::write(plugin.join("data.json"), data.to_string()).unwrap();
    let out = m.ok(&["import", plugin.join("data.json").to_str().unwrap(), "--name", "notes"]);
    assert!(out.contains("e2ee      passphrase in"), "{out}");
    assert!(!out.contains(PASS));

    let (code, out, err) = m.code(&["--vault", "notes", "adopt", "--json"]);
    assert!(code == 0 || code == 3, "{out}\n{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["identical"], 3, "matched by their encrypted chunk IDs: {v}");
    assert_eq!(
        std::fs::read(m.vault().join("remote.md")).unwrap(),
        b"only on the encrypted server"
    );
    let id = path_to_id("local.md", true);
    let d = mock.doc(&id).expect("pushed");
    assert!(d["children"][0].as_str().unwrap().starts_with("h:+"), "{d}");
    assert_eq!(mock.content(&id).unwrap(), b"only here");
    let status = m.ok(&["--vault", "notes", "status", "--json"]);
    assert!(!status.contains(PASS), "{status}");

    // A wrong passphrase from the environment: refused, nothing written.
    m.write("another.md", b"not sent");
    mock.clear_requests();
    let mut c = Command::new(env!("CARGO_BIN_EXE_lsync"));
    let out = c
        .env_clear()
        .env("HOME", m.root.join("home"))
        .env("XDG_CONFIG_HOME", m.root.join("config"))
        .env("XDG_STATE_HOME", m.root.join("state"))
        .env("LSYNC_RUNTIME_DIR", m.root.join("run"))
        .env("LSYNC_PASSPHRASE", "wrong-synthetic-guess")
        .args(["--vault", "notes", "sync"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("does not decrypt"), "{err}");
    assert!(!err.contains("wrong-synthetic-guess") && !err.contains(PASS), "{err}");
    assert!(mock.writes().is_empty(), "{:?}", mock.writes());
    assert!(mock.doc(&path_to_id("another.md", true)).is_none());
}
