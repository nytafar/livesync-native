//! The plugin guard through the binary (spec D16): `sync`, `fetch` and
//! `daemon` refuse an Obsidian vault whose Self-hosted LiveSync plugin is
//! enabled before they touch the network, and `--allow-plugin` (or
//! `allow_plugin = true` for the vault) lets a handover through. The server
//! here is a closed local port: getting past the guard shows as a
//! connection error instead. No network.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Machine {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

/// A local port nothing listens on.
fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["home", "config/lsync", "state", "run", "vault"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Machine { _dir: dir, root }
    }

    fn vault(&self) -> PathBuf {
        self.root.join("vault")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lsync"))
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("LSYNC_RUNTIME_DIR", self.root.join("run"))
            .env("RUST_LOG", "info")
            .current_dir(&self.root)
            .args(args)
            .output()
            .unwrap()
    }

    fn write_config(&self, port: u16, extra: &str) {
        std::fs::write(
            self.root.join("config/lsync/lsync.toml"),
            format!(
                "[account.dev]\nurl = \"http://127.0.0.1:{port}\"\n\n[vault.notes]\naccount = \"dev\"\ndatabase = \"notes\"\nfolder = \"{}\"\n{extra}",
                self.vault().display()
            ),
        )
        .unwrap();
    }
}

fn enable_plugin(vault: &Path) {
    let o = vault.join(".obsidian");
    std::fs::create_dir_all(o.join("plugins/obsidian-livesync")).unwrap();
    std::fs::write(o.join("community-plugins.json"), r#"["obsidian-livesync"]"#).unwrap();
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn two_way_commands_refuse_a_vault_with_the_plugin_enabled() {
    let m = Machine::new();
    let port = closed_port();
    m.write_config(port, "");
    enable_plugin(&m.vault());
    for cmd in ["sync", "fetch"] {
        let o = m.run(&[cmd]);
        let t = text(&o);
        assert_eq!(o.status.code(), Some(2), "{cmd}: {t}");
        assert!(t.contains("Self-hosted LiveSync plugin enabled"), "{cmd}: {t}");
        assert!(t.contains("--allow-plugin"), "{cmd}: {t}");
        assert!(
            !t.contains("Connection refused") && !t.contains("transport error"),
            "{cmd}: {t}"
        );
    }
    let o = m.run(&["daemon"]);
    let t = text(&o);
    assert_eq!(o.status.code(), Some(1), "{t}");
    assert!(t.contains("Self-hosted LiveSync plugin enabled"), "{t}");

    // `--allow-plugin` gets past the guard (to the closed port).
    let o = m.run(&["sync", "--allow-plugin"]);
    let t = text(&o);
    assert!(!t.contains("plugin enabled"), "{t}");
    assert!(
        t.contains("transport error") || t.contains("error sending request"),
        "{t}"
    );

    // So does `allow_plugin = true` in lsync.toml.
    m.write_config(port, "allow_plugin = true\n");
    let o = m.run(&["sync"]);
    let t = text(&o);
    assert!(!t.contains("plugin enabled"), "{t}");
}

#[test]
fn a_plain_folder_is_not_refused() {
    let m = Machine::new();
    m.write_config(closed_port(), "");
    let o = m.run(&["sync"]);
    let t = text(&o);
    assert!(!t.contains("plugin enabled"), "{t}");
    assert!(
        t.contains("transport error") || t.contains("error sending request"),
        "{t}"
    );
}
