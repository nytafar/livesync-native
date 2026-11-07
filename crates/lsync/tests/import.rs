//! `lsync import` through the binary, on the plugin and CLI settings files
//! upstream's own SettingService wrote (`fixtures/golden/import_settings.json`,
//! made by `tools/golden/settings.mjs`; every value is synthetic): the
//! connection decrypted, the password in the secret store and never in
//! `lsync.toml`, the vault's folder found from where the file lies, and
//! every setting lsync cannot honour refused with its reason. No network.

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Machine {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn fixture() -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/import_settings.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn case(name: &str) -> Value {
    fixture()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no fixture {name}"))["data"]
        .clone()
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["home", "config", "state"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Machine { _dir: dir, root }
    }

    /// Put a settings file where the plugin keeps it in `vault`.
    fn plugin_file(&self, vault: &str, data: &Value) -> PathBuf {
        let p = self
            .root
            .join(vault)
            .join(".obsidian/plugins/obsidian-livesync/data.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, serde_json::to_string_pretty(data).unwrap()).unwrap();
        p
    }

    fn file_at(&self, rel: &str, data: &Value) -> PathBuf {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, serde_json::to_string_pretty(data).unwrap()).unwrap();
        p
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.root.join("config/lsync/lsync.toml")).unwrap_or_default()
    }

    fn secrets(&self) -> String {
        std::fs::read_to_string(self.root.join("config/lsync/secrets.toml")).unwrap_or_default()
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        self.run_env(args, stdin, &[])
    }

    fn run_env(&self, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lsync"));
        c.env_clear()
            .envs(env.iter().copied())
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
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

    fn ok(&self, args: &[&str], stdin: Option<&str>) -> String {
        let out = self.run(args, stdin);
        assert!(
            out.status.success(),
            "lsync {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn fails(&self, args: &[&str], stdin: Option<&str>) -> String {
        let out = self.run(args, stdin);
        assert!(
            !out.status.success(),
            "lsync {args:?} succeeded:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn toml(m: &Machine) -> toml::Table {
    toml::from_str(&m.config()).unwrap()
}

#[test]
fn a_plugin_data_json_becomes_an_account_and_a_disabled_vault() {
    let m = Machine::new();
    let f = m.plugin_file("My Notes", &case("plugin-default"));
    let out = m.ok(&["import", s(&f)], None);
    assert!(out.contains("added vault my-notes"), "{out}");
    assert!(out.contains("lsync --vault my-notes adopt --dry-run"), "{out}");
    let t = toml(&m);
    let acc = &t["account"]["couch-example-invalid"];
    assert_eq!(acc["url"].as_str(), Some("https://couch.example.invalid"));
    assert_eq!(acc["username"].as_str(), Some("synthetic-user"));
    let v = &t["vault"]["my-notes"];
    assert_eq!(v["account"].as_str(), Some("couch-example-invalid"));
    assert_eq!(v["database"].as_str(), Some("notes-db"));
    assert_eq!(v["folder"].as_str(), Some(s(&m.root.join("My Notes"))));
    assert_eq!(v["enabled"].as_bool(), Some(false));
    assert_eq!(v["ignore_regex"].as_str(), Some("\\/\\.git\\/|[]|^Archive\\/"));
    assert_eq!(v["use_ignore_files"].as_bool(), Some(true));
    let names: Vec<&str> = v["ignore_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap())
        .collect();
    assert_eq!(names, [".gitignore", ".notesignore"]);
    // The password: in the secret store, never in lsync.toml.
    assert!(!m.config().contains("synthetic-password-1"));
    assert!(m.secrets().contains("synthetic-password-1"), "{}", m.secrets());
    let mode = std::fs::metadata(m.root.join("config/lsync/secrets.toml"))
        .unwrap()
        .permissions();
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777, 0o600);
    // The vault reads as any other.
    let ls = m.ok(&["vault", "ls", "--json"], None);
    assert!(ls.contains("my-notes"), "{ls}");
}

#[test]
fn the_active_remote_profile_wins_over_the_connection_fields() {
    let m = Machine::new();
    let f = m.plugin_file("vault", &case("plugin-profile-wins"));
    m.ok(&["import", s(&f), "--name", "notes"], None);
    let want = &fixture()["profile_wins"];
    assert_eq!(
        toml(&m)["vault"]["notes"]["database"].as_str(),
        want["database"].as_str()
    );
    assert!(m.secrets().contains(want["password"].as_str().unwrap()));
}

#[test]
fn the_official_clis_settings_json_finds_its_folder() {
    let m = Machine::new();
    let f = m.file_at("server-notes/.livesync/settings.json", &case("cli-settings"));
    let out = m.ok(&["import", s(&f)], None);
    assert!(out.contains("official CLI settings.json"), "{out}");
    let v = &toml(&m)["vault"]["server-notes"];
    assert_eq!(v["folder"].as_str(), Some(s(&m.root.join("server-notes"))));
    assert_eq!(v["database"].as_str(), Some("notes-db"));
}

#[test]
fn a_file_elsewhere_needs_the_folder_and_old_plain_files_work() {
    let m = Machine::new();
    let f = m.file_at("exports/old-data.json", &case("plugin-plain"));
    let e = m.fails(&["import", s(&f)], None);
    assert!(e.contains("give the folder with --folder"), "{e}");
    assert!(m.config().is_empty());
    std::fs::create_dir_all(m.root.join("Plain")).unwrap();
    m.ok(&["import", s(&f), "--folder", s(&m.root.join("Plain"))], None);
    assert_eq!(toml(&m)["vault"]["plain"]["database"].as_str(), Some("notes-db"));
    assert!(m.secrets().contains("synthetic-password-1"));
}

#[test]
fn a_configuration_passphrase_is_needed_when_the_plugin_keeps_one() {
    let m = Machine::new();
    let f = m.plugin_file("vault", &case("plugin-config-passphrase"));
    let e = m.fails(&["import", s(&f)], None);
    assert!(
        e.contains("--config-passphrase-stdin") && e.contains("LOCALSTORAGE"),
        "{e}"
    );
    let e = m.fails(
        &["import", s(&f), "--config-passphrase-stdin"],
        Some("not the passphrase\n"),
    );
    assert!(e.contains("does not decrypt with this configuration passphrase"), "{e}");
    assert!(m.config().is_empty() && m.secrets().is_empty());
    let pass = fixture()["config_passphrase"].as_str().unwrap().to_string();
    m.ok(
        &["import", s(&f), "--config-passphrase-stdin"],
        Some(&format!("{pass}\n")),
    );
    assert!(m.secrets().contains("synthetic-password-1"));
}

#[test]
fn end_to_end_encryption_stops_the_import() {
    let m = Machine::new();
    let f = m.plugin_file("vault", &case("plugin-e2ee"));
    let e = m.fails(&["import", s(&f)], None);
    assert!(e.contains("encrypt=true") && e.contains("end-to-end encryption"), "{e}");
    assert!(m.config().is_empty() && m.secrets().is_empty(), "nothing is written");
}

#[test]
fn settings_lsync_cannot_honour_are_refused_with_their_reason() {
    let base = case("plugin-default");
    let with = |k: &str, v: Value| {
        let mut d = base.clone();
        d[k] = v;
        d
    };
    for (data, expect) in [
        (with("remoteType", "MINIO".into()), "remoteType=MINIO"),
        (with("useJWT", true.into()), "useJWT=true"),
        (with("usePathObfuscation", true.into()), "usePathObfuscation"),
        (with("hashAlg", "sha1".into()), "hashAlg"),
        (with("pathNormalizationVersion", 1.into()), "pathNormalizationVersion"),
        (with("idDerivationVersion", 1.into()), "idDerivationVersion"),
    ] {
        let m = Machine::new();
        let f = m.plugin_file("vault", &data);
        let e = m.fails(&["import", s(&f)], None);
        assert!(e.contains(expect), "{expect}: {e}");
        assert!(
            m.config().is_empty() && m.secrets().is_empty(),
            "{expect}: nothing is written"
        );
    }
    // Every reason at once.
    let mut d = with("remoteType", "MINIO".into());
    d["syncInternalFiles"] = true.into();
    let m = Machine::new();
    let e = m.fails(&["import", s(&m.plugin_file("vault", &d))], None);
    assert!(e.contains("remoteType=MINIO") && e.contains("syncInternalFiles"), "{e}");
    // Custom headers hide in the plain connection fields of older files.
    let mut d = case("plugin-plain");
    d["couchDB_CustomHeaders"] = "X-Token: synthetic".into();
    let m = Machine::new();
    let e = m.fails(&["import", s(&m.plugin_file("vault", &d))], None);
    assert!(e.contains("couchDB_CustomHeaders"), "{e}");
    // Not a settings file at all.
    let m = Machine::new();
    let e = m.fails(
        &[
            "import",
            s(&m.plugin_file("vault", &serde_json::json!({"theme": "dark"}))),
        ],
        None,
    );
    assert!(e.contains("not a LiveSync settings file"), "{e}");
}

#[test]
fn what_lsync_leaves_out_on_purpose_goes_through_only_when_accepted() {
    let mut d = case("plugin-default");
    d["syncInternalFiles"] = true.into();
    d["resolveConflictsByNewerFile"] = true.into();
    let m = Machine::new();
    let f = m.plugin_file("vault", &d);
    let e = m.fails(&["import", s(&f)], None);
    assert!(
        e.contains("--accept syncInternalFiles") && e.contains("--accept resolveConflictsByNewerFile"),
        "{e}"
    );
    let e = m.fails(&["import", s(&f), "--accept", "syncInternalFiles"], None);
    assert!(
        e.contains("resolveConflictsByNewerFile") && !e.contains("--accept syncInternalFiles"),
        "{e}"
    );
    let e = m.fails(
        &[
            "import",
            s(&f),
            "--accept",
            "syncInternalFiles",
            "--accept",
            "usePluginSync",
        ],
        None,
    );
    assert!(
        e.contains("--accept usePluginSync: this file has no such setting"),
        "{e}"
    );
    let out = m.ok(
        &[
            "import",
            s(&f),
            "--accept",
            "syncInternalFiles",
            "--accept",
            "resolveConflictsByNewerFile",
        ],
        None,
    );
    assert!(out.contains("going on without (--accept):"), "{out}");
}

#[test]
fn an_account_for_the_same_server_and_user_is_reused() {
    let m = Machine::new();
    m.ok(&["import", s(&m.plugin_file("one", &case("plugin-default")))], None);
    let f = m.plugin_file("two", &case("cli-settings"));
    let out = m.ok(&["import", s(&f)], None);
    assert!(out.contains("uses account couch-example-invalid"), "{out}");
    let t = toml(&m);
    assert_eq!(t["account"].as_table().unwrap().len(), 1);
    assert_eq!(t["vault"]["two"]["account"].as_str(), Some("couch-example-invalid"));
    // Another password for that server and user is not taken over unasked.
    let mut d = case("plugin-plain");
    d["couchDB_PASSWORD"] = "synthetic-password-other".into();
    let f = m.plugin_file("three", &d);
    let e = m.fails(&["import", s(&f)], None);
    assert!(e.contains("already holds another password"), "{e}");
    assert!(!m.secrets().contains("synthetic-password-other"));
    // A vault name taken, or a folder inside another vault's, is refused.
    let e = m.fails(
        &[
            "import",
            s(&m.plugin_file("one", &case("plugin-default"))),
            "--name",
            "two",
        ],
        None,
    );
    assert!(e.contains("vault named two"), "{e}");
}

#[test]
fn a_dry_run_changes_nothing_and_json_says_what_would_be_added() {
    let m = Machine::new();
    let f = m.plugin_file("vault", &case("plugin-default"));
    let out = m.ok(&["import", s(&f), "--dry-run", "--json"], None);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["vault"]["name"], "vault");
    assert_eq!(v["vault"]["enabled"], false);
    assert_eq!(v["account"]["new"], true);
    assert!(!out.contains("synthetic-password-1"), "the password is never printed");
    assert!(m.config().is_empty() && m.secrets().is_empty());
}

#[test]
fn the_import_takes_no_connection_settings_from_the_command_line() {
    let m = Machine::new();
    let f = m.plugin_file("vault", &case("plugin-default"));
    let e = m.fails(&["import", s(&f), "--db", "other"], None);
    assert!(e.contains("--db is read from the file"), "{e}");
    let e = m.fails(&["--vault", "x", "import", s(&f)], None);
    assert!(e.contains("--name"), "{e}");
}

#[test]
fn an_account_whose_password_lsync_cannot_store_is_reused_only_when_it_matches() {
    // An account for the same server and user that takes its password from
    // the environment: lsync cannot write it, so it must hold the file's.
    let m = Machine::new();
    std::fs::create_dir_all(m.root.join("config/lsync")).unwrap();
    std::fs::write(
        m.root.join("config/lsync/lsync.toml"),
        "[account.home]\nurl = \"https://couch.example.invalid\"\nusername = \"synthetic-user\"\nsecret = \"env:LSYNC_TEST_IMPORT_PW\"\n",
    )
    .unwrap();
    let before = m.config();
    let f = m.plugin_file("vault", &case("plugin-default"));
    let fails = |env: &[(&str, &str)]| {
        let out = m.run_env(&["import", s(&f)], None, env);
        assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        assert_eq!(m.config(), before, "nothing is written");
        assert!(m.secrets().is_empty());
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let e = fails(&[]);
    assert!(
        e.contains("environment variable LSYNC_TEST_IMPORT_PW") && e.contains("not set here"),
        "{e}"
    );
    let e = fails(&[("LSYNC_TEST_IMPORT_PW", "synthetic-password-other")]);
    assert!(e.contains("holds another password"), "{e}");
    let out = m.run_env(
        &["import", s(&f)],
        None,
        &[("LSYNC_TEST_IMPORT_PW", "synthetic-password-1")],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        text.contains("uses account home") && text.contains("unchanged"),
        "{text}"
    );
    assert!(m.secrets().is_empty(), "nothing goes to secrets.toml");
    assert!(!m.config().contains("synthetic-password-1"));
}

#[test]
fn every_unsupported_setting_is_listed_and_markdown_merge_is_noted() {
    let mut d = case("plugin-default");
    d["usePathObfuscation"] = true.into();
    d["hashAlg"] = "sha1".into();
    d["pathNormalizationVersion"] = 1.into();
    let m = Machine::new();
    let e = m.fails(&["import", s(&m.plugin_file("vault", &d))], None);
    for want in ["usePathObfuscation", "hashAlg", "pathNormalizationVersion"] {
        assert!(e.contains(want), "{want}: {e}");
    }
    let mut d = case("plugin-default");
    d["disableMarkdownAutoMerge"] = true.into();
    let m = Machine::new();
    let out = m.ok(&["import", s(&m.plugin_file("vault", &d))], None);
    assert!(out.contains("disableMarkdownAutoMerge"), "{out}");
}
