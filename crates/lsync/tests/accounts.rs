//! `lsync account ...` and `lsync vault ...` through the binary, in a
//! machine of temp folders (its own HOME, config and state homes, nothing
//! from the caller's environment): comments kept, `secrets.toml` 0600,
//! passwords never in `lsync.toml` or on the command line, the folder rules
//! refused with what to change, migration on the first run, and the old
//! ways of naming settings still working. No network.

use livesync_engine::Index;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Machine {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["home", "config/lsync", "state", "vaults"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Machine { _dir: dir, root }
    }

    fn config_dir(&self) -> PathBuf {
        self.root.join("config/lsync")
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.config_dir().join("lsync.toml")).unwrap()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lsync"));
        c.env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("RUST_LOG", "info")
            .current_dir(&self.root)
            .args(args);
        c
    }

    fn run_with(&self, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Output {
        let mut c = self.cmd(args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        if let Some(s) = stdin {
            input.write_all(s.as_bytes()).unwrap();
        }
        drop(input);
        child.wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str], stdin: Option<&str>) -> String {
        let out = self.run_with(args, stdin, &[]);
        assert!(
            out.status.success(),
            "lsync {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn fails(&self, args: &[&str], stdin: Option<&str>) -> String {
        let out = self.run_with(args, stdin, &[]);
        assert!(
            !out.status.success(),
            "lsync {args:?} succeeded:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn folder(&self, rel: &str) -> String {
        self.root.join("vaults").join(rel).display().to_string()
    }

    fn index(&self, state: &Path, node: &str) -> Vec<u8> {
        std::fs::create_dir_all(state).unwrap();
        let ix = Index::open(&state.join("index.sqlite")).unwrap();
        ix.set_meta("node_id", node).unwrap();
        drop(ix);
        std::fs::read(state.join("index.sqlite")).unwrap()
    }
}

fn node_id(status_json: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(status_json).unwrap();
    v["node_id"].as_str().unwrap_or_default().to_string()
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn accounts_keep_passwords_in_the_secret_store_only() {
    let m = Machine::new();
    m.ok(
        &[
            "account",
            "add",
            "home",
            "--url",
            "https://couch.example.net",
            "--user",
            "me",
            "--password-stdin",
        ],
        Some("s3cret-home\n"),
    );
    let secrets = m.config_dir().join("secrets.toml");
    #[cfg(unix)]
    assert_eq!(mode(&secrets), 0o600);
    assert!(std::fs::read_to_string(&secrets).unwrap().contains("s3cret-home"));
    let text = m.config();
    assert!(!text.contains("s3cret"), "{text}");
    assert!(text.contains("secret = \"file:home\""), "{text}");

    // The store in use is shown, never the password.
    let ls = m.ok(&["account", "ls"], None);
    assert!(ls.contains("secrets.toml") && ls.contains("(set)"), "{ls}");
    assert!(!ls.contains("s3cret"), "{ls}");
    let js: serde_json::Value = serde_json::from_str(&m.ok(&["account", "ls", "--json"], None)).unwrap();
    assert_eq!(js[0]["store"], "file");
    assert_eq!(js[0]["password"], "set");

    // An environment-variable account: nothing is stored; the store is shown.
    m.ok(
        &[
            "account",
            "add",
            "work",
            "--url",
            "https://couch.example.org",
            "--user",
            "me",
            "--secret",
            "env:LSYNC_TEST_WORK_PW",
        ],
        None,
    );
    let ls = m.ok(&["account", "ls"], None);
    assert!(ls.contains("environment variable LSYNC_TEST_WORK_PW (missing)"), "{ls}");
    let e = m.fails(&["account", "passwd", "work", "--password-stdin"], Some("x"));
    assert!(e.contains("change it there"), "{e}");

    // A password on the command line is refused by the new commands.
    let e = m.fails(
        &[
            "account",
            "add",
            "other",
            "--url",
            "https://x.example",
            "--password",
            "pw-on-argv",
        ],
        None,
    );
    assert!(e.contains("never taken from the command line"), "{e}");
    assert!(!e.contains("pw-on-argv"), "{e}");

    m.ok(&["account", "passwd", "home", "--password-stdin"], Some("new-pw"));
    assert!(std::fs::read_to_string(&secrets).unwrap().contains("new-pw"));
    #[cfg(unix)]
    assert_eq!(mode(&secrets), 0o600);

    // An account in use cannot go; an unused one takes its password along.
    m.ok(
        &[
            "vault",
            "add",
            "notes",
            "--account",
            "home",
            "--db",
            "notes",
            "--folder",
            &m.folder("notes"),
        ],
        None,
    );
    let e = m.fails(&["account", "rm", "home"], None);
    assert!(e.contains("used by vault(s) notes"), "{e}");
    m.ok(&["vault", "rm", "notes"], None);
    m.ok(&["account", "rm", "home"], None);
    assert!(!std::fs::read_to_string(&secrets).unwrap().contains("new-pw"));
}

#[test]
fn vault_commands_keep_comments_and_refuse_overlaps_with_what_to_change() {
    let m = Machine::new();
    std::fs::write(
        m.config_dir().join("lsync.toml"),
        "# This machine's vaults.\n\n[account.home]\n# the server at home\nurl = \"https://couch.example.net\"\nusername = \"me\"\n\n[account.work]\nurl = \"https://couch.example.org\"\nsecret = \"env:LSYNC_TEST_WORK_PW\"\n",
    )
    .unwrap();
    let notes = m.folder("notes");
    m.ok(
        &[
            "vault",
            "add",
            "notes",
            "--account",
            "home",
            "--db",
            "notes",
            "--folder",
            &notes,
            "--exclude",
            "Archive",
        ],
        None,
    );
    // Two vaults on one database, with different folders and scopes.
    m.ok(
        &[
            "vault",
            "add",
            "notes-b",
            "--account",
            "home",
            "--db",
            "notes",
            "--folder",
            &m.folder("b"),
            "--include",
            "Archive",
        ],
        None,
    );
    m.ok(
        &[
            "vault",
            "edit",
            "notes-b",
            "--device-name",
            "desk-b",
            "--mode",
            "pull-only",
        ],
        None,
    );
    m.ok(&["vault", "disable", "notes-b"], None);
    let text = m.config();
    assert!(text.starts_with("# This machine's vaults.\n"), "{text}");
    assert!(
        text.contains("# the server at home\nurl = \"https://couch.example.net\""),
        "{text}"
    );
    assert!(text.contains("exclude = [\"Archive\"]"), "{text}");
    assert!(text.contains("device_name = \"desk-b\""), "{text}");
    assert!(text.contains("mode = \"pull-only\""), "{text}");
    assert!(text.contains("enabled = false"), "{text}");
    m.ok(&["vault", "enable", "notes-b"], None);
    assert!(!m.config().contains("enabled"), "{}", m.config());

    // The same folder.
    let e = m.fails(
        &[
            "vault",
            "add",
            "again",
            "--account",
            "work",
            "--db",
            "x",
            "--folder",
            &notes,
        ],
        None,
    );
    assert!(
        e.contains("use the same folder") && e.contains("each vault needs a folder of its own"),
        "{e}"
    );
    // Nested, on the same database.
    let e = m.fails(
        &[
            "vault",
            "add",
            "inner",
            "--account",
            "home",
            "--db",
            "notes",
            "--folder",
            &format!("{notes}/Archive"),
        ],
        None,
    );
    assert!(e.contains("syncs the same database"), "{e}");
    // Nested, on its own database, but synced by the parent's scope.
    let e = m.fails(
        &[
            "vault",
            "add",
            "proj",
            "--account",
            "work",
            "--db",
            "proj",
            "--folder",
            &format!("{notes}/Projects"),
        ],
        None,
    );
    assert!(e.contains("add \"Projects\" to exclude in [vault.notes]"), "{e}");
    // Excluded by the parent, on its own database: allowed.
    m.ok(
        &[
            "vault",
            "add",
            "archive",
            "--account",
            "work",
            "--db",
            "archive",
            "--folder",
            &format!("{notes}/Archive"),
        ],
        None,
    );
    // A refused edit leaves the file as it was.
    let before = m.config();
    let e = m.fails(&["vault", "edit", "notes", "--all-folders"], None);
    assert!(
        e.contains("vault \"archive\"") && e.contains("syncs \"Archive\""),
        "{e}"
    );
    assert_eq!(m.config(), before);

    // Settings come from the command line only, never from LSYNC_* variables.
    let out = m.run_with(
        &[
            "vault",
            "add",
            "envy",
            "--account",
            "home",
            "--folder",
            &m.folder("envy"),
        ],
        None,
        &[("LSYNC_DB", "from-env")],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("give the database with --db"));

    let ls: serde_json::Value = serde_json::from_str(&m.ok(&["vault", "ls", "--json"], None)).unwrap();
    let names: Vec<&str> = ls["vaults"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["archive", "notes", "notes-b"]);

    // A pull-only vault never runs a two-way command.
    let e = m.fails(&["--vault", "notes-b", "sync"], None);
    assert!(e.contains("is pull-only"), "{e}");
    // Several vaults and none named: nothing is guessed.
    let e = m.fails(&["status"], None);
    assert!(e.contains("choose one with --vault NAME"), "{e}");
    let e = m.fails(&["--vault", "nope", "status"], None);
    assert!(
        e.contains("no vault named \"nope\"") && e.contains("archive, notes, notes-b"),
        "{e}"
    );
}

#[test]
fn profiles_migrate_on_the_first_run_and_keep_their_index() {
    let m = Machine::new();
    let notes = m.folder("notes");
    std::fs::write(
        m.config_dir().join("notes.toml"),
        format!("url = \"https://couch.example.net\"\ndatabase = \"notes\"\nusername = \"me\"\npassword = \"pw-notes\"\nvault = \"{notes}\"\nscope_mode = \"exclude\"\nscope_folders = [\"Archive\"]\n"),
    )
    .unwrap();
    let custom = m.root.join("custom-state");
    std::fs::write(
        m.config_dir().join("bench.toml"),
        format!(
            "url = \"https://couch.example.net\"\ndatabase = \"bench\"\nusername = \"me\"\npassword = \"pw-notes\"\nvault = \"{}\"\nstate_dir = \"{}\"\n",
            m.folder("bench"),
            custom.display()
        ),
    )
    .unwrap();
    let ix_notes = m.index(&m.root.join("state/lsync/notes"), "node-notes");
    let ix_bench = m.index(&custom, "node-bench");

    // The first run migrates, then answers from the same index.
    let st = m.ok(&["--profile", "notes", "status", "--json"], None);
    assert_eq!(node_id(&st), "node-notes");
    let text = m.config();
    assert!(
        text.contains("[vault.notes]") && text.contains("[vault.bench]"),
        "{text}"
    );
    assert!(!text.contains("pw-notes"), "{text}");
    assert_eq!(
        std::fs::read(m.root.join("state/lsync/notes/index.sqlite")).unwrap(),
        ix_notes
    );
    assert_eq!(
        node_id(&m.ok(&["--vault", "bench", "status", "--json"], None)),
        "node-bench"
    );
    assert_eq!(std::fs::read(custom.join("index.sqlite")).unwrap(), ix_bench);
    let accounts: serde_json::Value = serde_json::from_str(&m.ok(&["account", "ls", "--json"], None)).unwrap();
    assert_eq!(
        accounts.as_array().unwrap().len(),
        1,
        "one server and user: one account"
    );
    assert_eq!(accounts[0]["password"], "set");

    // A second run changes nothing.
    let before = m.config();
    m.ok(&["vault", "ls"], None);
    assert_eq!(m.config(), before);

    // `configure` on a migrated profile updates its vault (the bench script's way).
    m.ok(
        &[
            "--profile",
            "bench",
            "configure",
            "--db",
            "bench-2",
            "--device-name",
            "bench-lsync",
            "--password-stdin",
        ],
        Some("pw-new"),
    );
    let text = m.config();
    assert!(
        text.contains("database = \"bench-2\"") && text.contains("device_name = \"bench-lsync\""),
        "{text}"
    );
    let secrets = std::fs::read_to_string(m.config_dir().join("secrets.toml")).unwrap();
    assert!(secrets.contains("pw-new"), "the shared account's password changed");
    // The old profile file is left as it was.
    assert!(std::fs::read_to_string(m.config_dir().join("bench.toml"))
        .unwrap()
        .contains("database = \"bench\"\n"));
}

#[test]
fn flags_environment_and_profile_files_work_as_before() {
    let m = Machine::new();
    let state = m.root.join("adhoc-state");
    m.index(&state, "node-adhoc");
    let folder = m.folder("adhoc");
    // No config at all: the environment (`LSYNC_VAULT` is still the folder).
    let out = m.run_with(
        &["status", "--json"],
        None,
        &[
            ("LSYNC_VAULT", folder.as_str()),
            ("LSYNC_STATE_DIR", state.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(node_id(&String::from_utf8_lossy(&out.stdout)), "node-adhoc");
    // `--vault PATH`: the folder, with a deprecation warning.
    let out = m.run_with(
        &[
            "--vault",
            &folder,
            "--state-dir",
            state.to_str().unwrap(),
            "status",
            "--json",
        ],
        None,
        &[],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("use --folder"));
    // `LSYNC_CONFIG` pointing at a profile file elsewhere: read, never migrated.
    let file = m.root.join("a.toml");
    let out = m.run_with(
        &[
            "configure",
            "--url",
            "https://couch.example.net",
            "--db",
            "x",
            "--folder",
            &folder,
            "--state-dir",
            state.to_str().unwrap(),
        ],
        None,
        &[("LSYNC_CONFIG", file.to_str().unwrap()), ("LSYNC_PASSWORD", "pw")],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(std::fs::read_to_string(&file).unwrap().contains("password = \"pw\""));
    #[cfg(unix)]
    assert_eq!(mode(&file), 0o600);
    let out = m.run_with(&["status", "--json"], None, &[("LSYNC_CONFIG", file.to_str().unwrap())]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(node_id(&String::from_utf8_lossy(&out.stdout)), "node-adhoc");
    assert!(!m.config_dir().join("lsync.toml").exists());
    // `--profile NAME configure` on a fresh machine writes NAME.toml, as before.
    m.ok(
        &[
            "--profile",
            "fresh",
            "configure",
            "--db",
            "y",
            "--folder",
            &m.folder("fresh"),
        ],
        None,
    );
    assert!(m.config_dir().join("fresh.toml").exists());
}

#[test]
fn vault_on_the_command_line_wins_over_lsync_profile() {
    let m = Machine::new();
    std::fs::write(
        m.config_dir().join("lsync.toml"),
        "[account.home]\nurl = \"https://couch.example.net\"\n\n[vault.a]\naccount = \"home\"\ndatabase = \"a\"\nfolder = \"/srv/a\"\n\n[vault.b]\naccount = \"home\"\ndatabase = \"b\"\nfolder = \"/srv/b\"\n",
    )
    .unwrap();
    m.index(&m.root.join("state/lsync/b"), "node-b");
    m.index(&m.root.join("state/lsync/a"), "node-a");
    let out = m.run_with(&["--vault", "b", "status", "--json"], None, &[("LSYNC_PROFILE", "a")]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(node_id(&String::from_utf8_lossy(&out.stdout)), "node-b");
    let e = m.fails(&["--vault", "b", "--profile", "a", "status"], None);
    assert!(e.contains("different vaults"), "{e}");
    // A hand-made mistake can still be mended with the commands.
    let text = m.config().replace("folder = \"/srv/b\"", "folder = \"/srv/a\"");
    std::fs::write(m.config_dir().join("lsync.toml"), text).unwrap();
    assert!(m.fails(&["vault", "ls"], None).contains("use the same folder"));
    m.ok(&["vault", "rm", "b"], None);
    m.ok(&["vault", "ls"], None);
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn an_unmigrated_default_profile_is_never_replaced_by_another_vault() {
    let m = Machine::new();
    // default.toml takes its URL from LSYNC_URL, so it is not migrated;
    // work.toml is complete and becomes the only vault.
    std::fs::write(
        m.config_dir().join("default.toml"),
        format!("database = \"mine\"\nvault = \"{}\"\n", m.folder("mine")),
    )
    .unwrap();
    std::fs::write(
        m.config_dir().join("work.toml"),
        format!(
            "url = \"https://couch.example.net\"\ndatabase = \"work\"\nusername = \"me\"\npassword = \"pw-work\"\nvault = \"{}\"\n",
            m.folder("work")
        ),
    )
    .unwrap();
    m.index(&m.root.join("state/lsync/work"), "node-work");
    m.index(&m.root.join("state/lsync/default"), "node-default");
    let env = [("LSYNC_URL", "https://other.example.org")];
    for _ in 0..2 {
        let out = m.run_with(&["status", "--json"], None, &env);
        assert!(out.status.success(), "{}", stderr(&out));
        assert_eq!(node_id(&String::from_utf8_lossy(&out.stdout)), "node-default");
    }
    assert!(m.config().contains("[vault.work]"));
    // Named, work is work.
    assert_eq!(
        node_id(&m.ok(&["--vault", "work", "status", "--json"], None)),
        "node-work"
    );
}

#[test]
fn a_profile_nothing_could_migrate_is_reported_once_and_never_quoted() {
    let m = Machine::new();
    std::fs::write(
        m.config_dir().join("default.toml"),
        "url = \"https://couch.example.net\"\nmax_size_mb = \"Hunter2-Sup3rSecret\"\n",
    )
    .unwrap();
    let first = m.run_with(&["vault", "ls"], None, &[]);
    let e = stderr(&first);
    assert!(e.contains("was not migrated"), "{e}");
    assert!(!e.contains("Hunter2"), "{e}");
    let second = m.run_with(&["vault", "ls"], None, &[]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(!stderr(&second).contains("migrat"), "{}", stderr(&second));
    assert!(String::from_utf8_lossy(&second.stdout).contains("default.toml"));
    assert!(!m.config().contains("Hunter2"));
    // A command still reads the profile, and its error does not quote it either.
    let out = m.run_with(&["status"], None, &[]);
    assert!(!out.status.success());
    assert!(!stderr(&out).contains("Hunter2"), "{}", stderr(&out));
    // A --secret argument is not quoted back.
    let e = m.fails(
        &[
            "account",
            "add",
            "x",
            "--url",
            "https://couch.example.net",
            "--secret",
            "Hunter2-Sup3rSecret",
        ],
        None,
    );
    assert!(e.contains("STORE:KEY") && !e.contains("Hunter2"), "{e}");
}

#[test]
fn a_url_with_a_password_is_refused_by_account_add() {
    let m = Machine::new();
    let e = m.fails(
        &[
            "account",
            "add",
            "home",
            "--url",
            "https://me:S3cret@couch.example.net",
            "--password-stdin",
        ],
        Some("pw\n"),
    );
    assert!(e.contains("--user") && e.contains("asked"), "{e}");
    assert!(!e.contains("S3cret"), "{e}");
    assert!(!m.config_dir().join("lsync.toml").exists());
    assert!(!m.config_dir().join("secrets.toml").exists());
}

#[test]
fn a_profile_with_the_password_in_its_url_migrates_and_keeps_it_out_of_lsync_toml() {
    let m = Machine::new();
    std::fs::write(
        m.config_dir().join("notes.toml"),
        format!(
            "url = \"https://me:S3cret@couch.example.net\"\ndatabase = \"notes\"\nvault = \"{}\"\n",
            m.folder("notes")
        ),
    )
    .unwrap();
    let ls = m.ok(&["account", "ls"], None);
    assert!(
        !ls.contains("S3cret") && ls.contains("https://couch.example.net"),
        "{ls}"
    );
    assert!(ls.contains("user      me") && ls.contains("(set)"), "{ls}");
    assert!(!m.config().contains("S3cret"));
    let secrets = std::fs::read_to_string(m.config_dir().join("secrets.toml")).unwrap();
    assert!(secrets.contains("S3cret"));
}

#[test]
fn removing_every_vault_and_account_leaves_a_usable_file() {
    let m = Machine::new();
    std::fs::write(
        m.config_dir().join("lsync.toml"),
        "# my notes on this machine\n\n[account.home]\nurl = \"https://couch.example.net\"\n\n[vault.work]\naccount = \"home\"\ndatabase = \"work\"\nfolder = \"/srv/work\"\n",
    )
    .unwrap();
    m.ok(&["vault", "rm", "work"], None);
    m.ok(&["account", "rm", "home"], None);
    assert!(m.config().starts_with("# my notes on this machine\n"), "{}", m.config());
    let ls = m.ok(&["vault", "ls"], None);
    assert!(ls.contains("no vaults"), "{ls}");
    m.ok(&["account", "add", "home", "--url", "https://couch.example.net"], None);
    assert!(m.config().contains("[account.home]"));
}

#[test]
fn an_account_without_a_user_has_no_password() {
    let m = Machine::new();
    // Not asked for one (stdin is empty), and none stored.
    m.ok(&["account", "add", "open", "--url", "http://127.0.0.1:5984"], None);
    m.ok(
        &["vault", "add", "notes", "--db", "notes", "--folder", &m.folder("notes")],
        None,
    );
    assert!(!m.config_dir().join("secrets.toml").exists());
    let ls = m.ok(&["account", "ls"], None);
    assert!(ls.contains("not used: no user"), "{ls}");
    m.index(&m.root.join("state/lsync/notes"), "node-open");
    assert_eq!(node_id(&m.ok(&["status", "--json"], None)), "node-open");
    let e = m.fails(
        &[
            "account",
            "add",
            "open2",
            "--url",
            "http://127.0.0.1:5984",
            "--password-stdin",
        ],
        Some("pw\n"),
    );
    assert!(e.contains("no user"), "{e}");
}

#[test]
fn one_accounts_password_is_never_overwritten_by_another_unasked() {
    let m = Machine::new();
    let secrets = m.config_dir().join("secrets.toml");
    let add = |extra: &[&str], stdin: &str| {
        let mut args = vec!["account", "add"];
        args.extend_from_slice(extra);
        m.run_with(&args, Some(stdin), &[])
    };
    let ok = |o: Output| assert!(o.status.success(), "{}", stderr(&o));
    ok(add(
        &[
            "home",
            "--url",
            "https://couch.example.net",
            "--user",
            "me",
            "--password-stdin",
        ],
        "pw-home\n",
    ));
    let out = add(
        &[
            "other",
            "--url",
            "https://couch.example.org",
            "--user",
            "me",
            "--secret",
            "file:home",
            "--password-stdin",
        ],
        "pw-other\n",
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("--replace-secret") && stderr(&out).contains("home"),
        "{}",
        stderr(&out)
    );
    assert!(std::fs::read_to_string(&secrets).unwrap().contains("pw-home"));
    assert!(!m.config().contains("[account.other]"));
    // Sharing it on purpose, without a new password, is fine.
    ok(add(
        &[
            "shared",
            "--url",
            "https://couch.example.net",
            "--user",
            "me",
            "--secret",
            "file:home",
            "--no-password",
        ],
        "",
    ));
    // A key left from before holds a password: never used or replaced unasked.
    let mut text = std::fs::read_to_string(&secrets).unwrap();
    text.push_str("left = \"pw-left\"\n");
    std::fs::write(&secrets, text).unwrap();
    let out = add(
        &[
            "left",
            "--url",
            "https://couch.example.net",
            "--user",
            "me",
            "--password-stdin",
        ],
        "pw-new\n",
    );
    assert!(
        !out.status.success() && stderr(&out).contains("already holds"),
        "{}",
        stderr(&out)
    );
    let out = add(
        &[
            "left",
            "--url",
            "https://couch.example.net",
            "--user",
            "me",
            "--no-password",
        ],
        "",
    );
    assert!(
        !out.status.success() && stderr(&out).contains("already holds"),
        "{}",
        stderr(&out)
    );
    ok(add(
        &[
            "left",
            "--url",
            "https://couch.example.net",
            "--user",
            "me",
            "--password-stdin",
            "--replace-secret",
        ],
        "pw-new\n",
    ));
    let text = std::fs::read_to_string(&secrets).unwrap();
    assert!(
        text.contains("pw-new") && !text.contains("pw-left") && text.contains("pw-home"),
        "{text}"
    );
}

#[test]
fn configure_never_gives_a_new_account_a_password_left_in_secrets_toml() {
    let m = Machine::new();
    std::fs::write(
        m.config_dir().join("lsync.toml"),
        "[account.home]\nurl = \"https://couch.example.org\"\n",
    )
    .unwrap();
    let secrets = m.config_dir().join("secrets.toml");
    std::fs::write(&secrets, "couch-example-net = \"pw-stale\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    m.ok(
        &[
            "--profile",
            "new",
            "configure",
            "--url",
            "https://couch.example.net",
            "--user",
            "me",
            "--db",
            "new",
            "--folder",
            &m.folder("new"),
        ],
        None,
    );
    let js: serde_json::Value = serde_json::from_str(&m.ok(&["vault", "ls", "--json"], None)).unwrap();
    let account = js["vaults"][0]["account"].as_str().unwrap().to_string();
    assert_ne!(account, "couch-example-net");
    let accounts: serde_json::Value = serde_json::from_str(&m.ok(&["account", "ls", "--json"], None)).unwrap();
    let a = accounts
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == account.as_str())
        .unwrap();
    assert_eq!(a["password"], "missing");
    // A password in the URL goes to the store, never into lsync.toml.
    m.ok(
        &[
            "--profile",
            "new",
            "configure",
            "--url",
            "https://me:pw-url@couch.example.net",
        ],
        None,
    );
    assert!(!m.config().contains("pw-url"), "{}", m.config());
    let text = std::fs::read_to_string(&secrets).unwrap();
    assert!(
        text.contains(&format!("{account} = \"pw-url\"")) && text.contains("pw-stale"),
        "{text}"
    );
}

#[test]
fn a_migrated_default_profile_is_not_run_after_its_vault_is_removed() {
    let m = Machine::new();
    let folder = m.folder("notes");
    std::fs::write(
        m.config_dir().join("default.toml"),
        format!(
            "url = \"https://couch.example.net\"\ndatabase = \"notes\"\nusername = \"me\"\npassword = \"pw-old\"\nvault = \"{folder}\"\n"
        ),
    )
    .unwrap();
    m.ok(&["vault", "ls"], None);
    assert!(m.config().contains("[vault.default]") && m.config().contains("[migration]"));
    m.ok(&["vault", "rm", "default"], None);
    m.ok(&["vault", "add", "notes", "--db", "notes", "--folder", &folder], None);
    // The record survives the edits.
    assert!(m.config().contains("migrated = [\"default.toml\"]"), "{}", m.config());
    m.index(&m.root.join("state/lsync/default"), "node-default");
    m.index(&m.root.join("state/lsync/notes"), "node-notes");
    assert_eq!(node_id(&m.ok(&["status", "--json"], None)), "node-notes");
}
