//! `keyring:KEY` against a real Secret Service: a private D-Bus session bus
//! and a throwaway gnome-keyring in temp folders. The environment is cleared
//! for every process, so nothing reaches this machine's own session bus or
//! keyring. Skipped (with the reason) where dbus-daemon or gnome-keyring is
//! missing, as on macOS, whose Keychain is checked by CI's build and the
//! field check only.

#![cfg(target_os = "linux")]

#[path = "../../livesync-engine/tests/common/mod.rs"]
mod common;

use common::{Mock, DB};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn which(prog: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .iter()
        .map(|d| Path::new(d).join(prog))
        .find(|p| p.is_file())
}

/// A session bus and a keyring of their own, stopped when dropped.
struct Session {
    _dir: tempfile::TempDir,
    root: PathBuf,
    address: String,
    children: Vec<Child>,
}

impl Drop for Session {
    fn drop(&mut self) {
        for c in self.children.iter_mut().rev() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Session {
    fn start() -> Option<Session> {
        let (Some(bus), Some(keyring)) = (which("dbus-daemon"), which("gnome-keyring-daemon")) else {
            eprintln!("skipped: needs dbus-daemon and gnome-keyring-daemon for a private Secret Service");
            return None;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["home", "data", "run", "config/lsync", "state", "vaults"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("run/bus");
        let address = format!("unix:path={}", socket.display());
        let mut s = Session {
            _dir: dir,
            root: root.clone(),
            address: address.clone(),
            children: vec![],
        };
        let child = Command::new(bus)
            .env_clear()
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("PATH", "/usr/bin:/bin")
            .args(["--session", "--nofork", "--nopidfile", &format!("--address={address}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        s.children.push(child);
        let end = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            if Instant::now() > end {
                eprintln!("skipped: the private session bus did not start");
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // --unlock creates the login keyring with the password on stdin.
        let mut cmd = Command::new(keyring);
        let mut child = s
            .env(&mut cmd)
            .args(["--unlock", "--components=secrets", "--foreground"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"throwaway").unwrap();
        s.children.push(child);
        Some(s)
    }

    /// Only this session's folders and bus.
    fn env<'a>(&self, c: &'a mut Command) -> &'a mut Command {
        c.env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("LSYNC_RUNTIME_DIR", self.root.join("run/lsync"))
            .env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&self.root)
    }

    fn lsync(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lsync"));
        self.env(&mut c)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        let mut child = c.spawn().unwrap();
        if let Some(text) = stdin {
            child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
        }
        child.wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str], stdin: Option<&str>) -> String {
        let out = self.lsync(args, stdin);
        assert!(
            out.status.success(),
            "lsync {args:?}:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `secret-tool lookup service lsync username KEY` in this session.
    fn lookup(&self, key: &str) -> Option<String> {
        let tool = which("secret-tool")?;
        let out = self
            .env(&mut Command::new(tool))
            .args(["lookup", "service", "lsync", "username", key])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn account_state(&self) -> String {
        let v: serde_json::Value = serde_json::from_str(&self.ok(&["account", "ls", "--json"], None)).unwrap();
        v[0]["password"].as_str().unwrap_or_default().to_string()
    }
}

#[test]
fn passwords_live_in_the_secret_service() {
    let Some(s) = Session::start() else { return };
    // The keyring takes a moment to claim its bus name.
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        let out = s.lsync(
            &[
                "account",
                "add",
                "dev",
                "--url",
                "http://127.0.0.1:9",
                "--user",
                "u",
                "--secret",
                "keyring:dev",
                "--password-stdin",
            ],
            Some("first-pw"),
        );
        if out.status.success() {
            break;
        }
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        // Nothing was written before the store answered.
        std::fs::remove_file(s.root.join("config/lsync/lsync.toml")).ok();
        assert!(Instant::now() < end, "the private keyring never answered: {err}");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(s.account_state(), "set");
    assert!(!std::fs::read_to_string(s.root.join("config/lsync/lsync.toml"))
        .unwrap()
        .contains("first-pw"));
    assert!(!s.root.join("config/lsync/secrets.toml").exists());
    // Filed as secret-tool and other clients look for it.
    if which("secret-tool").is_some() {
        assert_eq!(s.lookup("dev").as_deref(), Some("first-pw"));
    }

    // The engine reads it: a sync against the mock server with this account.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mock = rt.block_on(Mock::start());
    mock.put_text("Notes/a.md", &["note a"]);
    s.ok(&["account", "rm", "dev"], None);
    assert_eq!(s.lookup("dev"), None, "rm removes the keyring item");
    s.ok(
        &[
            "account",
            "add",
            "dev",
            "--url",
            &mock.url,
            "--user",
            "u",
            "--secret",
            "keyring:dev",
            "--password-stdin",
        ],
        Some("pw"),
    );
    let folder = s.root.join("vaults/notes");
    s.ok(
        &[
            "vault",
            "add",
            "notes",
            "--account",
            "dev",
            "--db",
            DB,
            "--folder",
            folder.to_str().unwrap(),
        ],
        None,
    );
    s.ok(&["--vault", "notes", "sync"], None);
    assert!(folder.join("Notes/a.md").exists());

    s.ok(&["account", "passwd", "dev", "--password-stdin"], Some("second-pw"));
    if which("secret-tool").is_some() {
        assert_eq!(s.lookup("dev").as_deref(), Some("second-pw"));
    }

    // A locked keyring: "waiting for the keyring", and no password is read.
    if let Some(send) = which("dbus-send") {
        let out = s
            .env(&mut Command::new(send))
            .args([
                "--session",
                "--print-reply",
                "--dest=org.freedesktop.secrets",
                "/org/freedesktop/secrets",
                "org.freedesktop.Secret.Service.Lock",
                "array:objpath:/org/freedesktop/secrets/aliases/default",
            ])
            .output()
            .unwrap();
        if out.status.success() {
            let state = s.account_state();
            assert!(state.starts_with("locked: waiting for the keyring"), "{state}");
            let out = s.lsync(&["--vault", "notes", "sync"], None);
            assert!(!out.status.success());
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("waiting for the keyring"),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        } else {
            eprintln!("skipped the locked case: {}", String::from_utf8_lossy(&out.stderr));
        }
    }
}

#[test]
fn without_a_session_bus_the_keyring_is_refused_and_nothing_is_written() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    std::fs::create_dir_all(root.join("run")).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_lsync"));
    c.env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("LSYNC_RUNTIME_DIR", root.join("run/lsync"))
        .args([
            "account",
            "add",
            "dev",
            "--url",
            "http://127.0.0.1:9",
            "--user",
            "u",
            "--secret",
            "keyring:dev",
            "--password-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"pw").unwrap();
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("no Secret Service"), "{err}");
    assert!(!root.join("config/lsync/lsync.toml").exists());
}
