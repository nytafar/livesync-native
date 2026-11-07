//! Migration from `<profile>.toml` files on copies of profile files (neutral
//! fixtures): the same state folders and node IDs, the index untouched, the
//! passwords in the secret store, a second run a no-op, and a crash before
//! `lsync.toml` is written loses nothing.

use livesync_engine::Index;
use livesync_host::config::{Config, Dirs, Mode};
use livesync_host::migrate::{self, migrate};
use livesync_host::secret::{FileStore, SecretStore};
use std::path::{Path, PathBuf};

struct Machine {
    _dir: tempfile::TempDir,
    root: PathBuf,
    dirs: Dirs,
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let dirs = Dirs {
            home: Some(root.join("home")),
            config: root.join("config/lsync"),
            state: root.join("state/lsync"),
        };
        std::fs::create_dir_all(&dirs.config).unwrap();
        Machine { _dir: dir, root, dirs }
    }

    fn profile(&self, name: &str, text: &str) -> PathBuf {
        let p = self.dirs.config.join(format!("{name}.toml"));
        std::fs::write(&p, text.replace("@ROOT@", &self.root.display().to_string())).unwrap();
        p
    }

    /// An index with a node ID, where the profile's engine would keep it.
    fn index(&self, state: &Path, node: &str) -> Vec<u8> {
        std::fs::create_dir_all(state).unwrap();
        let path = state.join("index.sqlite");
        let ix = Index::open(&path).unwrap();
        ix.set_meta("node_id", node).unwrap();
        drop(ix);
        std::fs::read(&path).unwrap()
    }

    fn store(&self) -> FileStore {
        FileStore::new(self.dirs.secrets_file())
    }

    fn config(&self) -> Config {
        Config::load(&self.dirs.config_file(), &self.dirs).unwrap().unwrap()
    }
}

const NOTES: &str = r#"url = "https://couch.example.net"
database = "notes"
username = "me"
password = "pw-home"
vault = "@ROOT@/vaults/notes"
scope_mode = "exclude"
scope_folders = ["Archive"]
device_name = "desk"
"#;

const NOTES_B: &str = r#"url = "https://couch.example.net/"
database = "notes"
username = "me"
password = "pw-home"
vault = "@ROOT@/vaults/notes-b"
state_dir = "@ROOT@/custom-state/b"
scope_mode = "include"
scope_folders = ["Archive"]
ignore_regex = "\\/\\.git\\/"
concurrency = 8
"#;

const WORK: &str = r#"url = "https://couch.example.org"
database = "work"
username = "me"
password = "pw-work"
vault = "@ROOT@/vaults/work"
"#;

#[test]
fn profiles_become_accounts_and_vaults_with_their_state_kept() {
    let m = Machine::new();
    let notes_file = m.profile("notes", NOTES);
    m.profile("notes-b", NOTES_B);
    m.profile("work", WORK);
    let before_notes = std::fs::read(&notes_file).unwrap();
    let idx_notes = m.index(&m.dirs.default_state_dir("notes"), "node-notes");
    let idx_b = m.index(&m.root.join("custom-state/b"), "node-b");
    let idx_work = m.index(&m.dirs.default_state_dir("work"), "node-work");

    assert!(migrate::needed(&m.dirs));
    let r = migrate(&m.dirs, &m.store()).unwrap().unwrap();
    assert!(r.written);
    assert!(r.skipped.is_empty(), "{:?}", r.skipped);
    assert_eq!(
        r.accounts,
        vec!["couch-example-net".to_string(), "couch-example-org".to_string()]
    );
    assert_eq!(r.vaults.len(), 3);

    let c = m.config();
    // One account per server and user; a trailing slash is the same server.
    assert_eq!(c.account.len(), 2);
    let home = &c.account["couch-example-net"];
    assert_eq!(home.url, "https://couch.example.net");
    assert_eq!(home.username.as_deref(), Some("me"));
    assert_eq!(home.secret.as_deref(), Some("file:couch-example-net"));
    assert_eq!(c.vaults_of("couch-example-net"), vec!["notes", "notes-b"]);

    // The same state folders, so the same indexes and node IDs.
    for (name, node, bytes) in [
        ("notes", "node-notes", &idx_notes),
        ("notes-b", "node-b", &idx_b),
        ("work", "node-work", &idx_work),
    ] {
        let state = c.vault[name].state_path(name, &m.dirs);
        let index = state.join("index.sqlite");
        assert_eq!(&std::fs::read(&index).unwrap(), bytes, "{name}: the index is untouched");
        let ix = Index::open(&index).unwrap();
        assert_eq!(ix.get_meta("node_id").unwrap().as_deref(), Some(node));
    }
    assert_eq!(
        c.vault["notes"].state_dir, None,
        "the default state folder stays implicit"
    );
    assert_eq!(
        c.vault["notes-b"].state_path("notes-b", &m.dirs),
        m.root.join("custom-state/b")
    );

    // Settings carried over.
    let n = &c.vault["notes"];
    assert_eq!(n.exclude.as_deref(), Some(&["Archive".to_string()][..]));
    assert_eq!(n.device_name.as_deref(), Some("desk"));
    assert_eq!(n.mode, Mode::TwoWay);
    assert!(n.enabled);
    let b = &c.vault["notes-b"];
    assert_eq!(b.include.as_deref(), Some(&["Archive".to_string()][..]));
    assert_eq!(b.ignore_regex.as_deref(), Some("\\/\\.git\\/"));
    assert_eq!(b.concurrency, Some(8));

    // The passwords are in the secret store, and not in lsync.toml.
    let s = m.store();
    assert_eq!(s.get("couch-example-net").unwrap().unwrap().expose(), "pw-home");
    assert_eq!(s.get("couch-example-org").unwrap().unwrap().expose(), "pw-work");
    let text = std::fs::read_to_string(m.dirs.config_file()).unwrap();
    assert!(!text.contains("pw-"), "{text}");
    assert!(text.contains("# migrated from notes.toml\n[vault.notes]"), "{text}");
    assert!(text.starts_with("# lsync accounts and vaults."), "{text}");
    assert!(
        text.contains("# Migrated from notes.toml, notes-b.toml, work.toml."),
        "{text}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(m.dirs.secrets_file()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // The profile files are left as they were (an older lsync still runs).
    assert_eq!(std::fs::read(&notes_file).unwrap(), before_notes);

    // A second run does nothing.
    let config_before = std::fs::read(m.dirs.config_file()).unwrap();
    let secrets_before = std::fs::read(m.dirs.secrets_file()).unwrap();
    assert!(!migrate::needed(&m.dirs));
    assert_eq!(migrate(&m.dirs, &m.store()).unwrap(), None);
    assert_eq!(std::fs::read(m.dirs.config_file()).unwrap(), config_before);
    assert_eq!(std::fs::read(m.dirs.secrets_file()).unwrap(), secrets_before);
}

#[test]
fn a_crash_before_lsync_toml_is_written_loses_nothing() {
    let m = Machine::new();
    m.profile("notes", NOTES);
    m.profile("work", WORK);
    // The passwords went in, then the process died: no lsync.toml.
    let r = migrate(&m.dirs, &m.store()).unwrap().unwrap();
    let first = m.config();
    std::fs::remove_file(m.dirs.config_file()).unwrap();
    assert!(migrate::needed(&m.dirs));
    let again = migrate(&m.dirs, &m.store()).unwrap().unwrap();
    assert_eq!(again.accounts, r.accounts, "the same names on the second try");
    let second = m.config();
    assert_eq!(
        first.account.keys().collect::<Vec<_>>(),
        second.account.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        first.vault.keys().collect::<Vec<_>>(),
        second.vault.keys().collect::<Vec<_>>()
    );
    assert_eq!(m.store().keys().unwrap().len(), 2);
    assert_eq!(m.store().get("couch-example-net").unwrap().unwrap().expose(), "pw-home");
}

#[test]
fn the_same_user_with_another_password_is_another_account_and_a_foreign_key_is_not_overwritten() {
    let m = Machine::new();
    m.profile("notes", NOTES);
    m.profile(
        "old",
        &NOTES
            .replace("pw-home", "pw-stale")
            .replace("vaults/notes", "vaults/old"),
    );
    // A key of that name already holds something else.
    m.store()
        .set("couch-example-net", &livesync_host::secret::Secret::new("unrelated"))
        .unwrap();
    let r = migrate(&m.dirs, &m.store()).unwrap().unwrap();
    assert_eq!(
        r.accounts,
        vec!["couch-example-net-me".to_string(), "couch-example-net-me-2".to_string()]
    );
    let s = m.store();
    assert_eq!(s.get("couch-example-net").unwrap().unwrap().expose(), "unrelated");
    assert_eq!(s.get("couch-example-net-me").unwrap().unwrap().expose(), "pw-home");
    assert_eq!(s.get("couch-example-net-me-2").unwrap().unwrap().expose(), "pw-stale");
}

#[test]
fn profiles_that_cannot_be_vaults_are_left_and_reported() {
    let m = Machine::new();
    m.profile("notes", NOTES);
    m.profile("partial", "vault = \"@ROOT@/vaults/p\"\n");
    m.profile("broken", "url = [\n");
    m.profile("bad name", WORK);
    // The same folder as `notes` (which comes first by name): the folder
    // rules hold for migrated vaults too.
    m.profile("other", &NOTES.replace("database = \"notes\"", "database = \"other\""));
    let r = migrate(&m.dirs, &m.store()).unwrap().unwrap();
    assert!(r.written);
    assert_eq!(
        r.vaults.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        vec!["notes"]
    );
    let why = |n: &str| {
        r.skipped
            .iter()
            .find(|(p, _)| p.file_stem().unwrap() == n)
            .map(|(_, w)| w.clone())
            .unwrap_or_else(|| panic!("{n} not skipped: {:?}", r.skipped))
    };
    assert!(why("partial").contains("no url"));
    assert!(why("broken").contains("parsing"));
    assert!(why("bad name").contains("not valid"));
    assert!(why("other").contains("use the same folder"), "{}", why("other"));
    // Only what a vault uses becomes an account.
    assert_eq!(m.config().account.len(), 1);
}

#[test]
fn nothing_to_migrate_writes_nothing() {
    let m = Machine::new();
    assert_eq!(migrate(&m.dirs, &m.store()).unwrap(), None);
    assert!(!m.dirs.config_file().exists());
    // Only incomplete profiles: still nothing written, so they are retried.
    m.profile("partial", "vault = \"/srv/p\"\n");
    let r = migrate(&m.dirs, &m.store()).unwrap().unwrap();
    assert!(!r.written);
    assert!(!m.dirs.config_file().exists());
    assert!(!m.dirs.secrets_file().exists());
}
