//! `lsync serve`, `lsync daemon` and the CLI routing through the control
//! socket, through the binary, on the engine's mock CouchDB, in a machine of
//! temp folders (its own HOME, config, state and runtime folders, nothing
//! from the caller's environment).

#[path = "../../livesync-engine/tests/common/mod.rs"]
mod common;

use common::{Mock, DB};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Machine {
    _dir: tempfile::TempDir,
    root: PathBuf,
    env: Vec<(String, String)>,
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["home", "config/lsync", "state", "vaults"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Machine {
            _dir: dir,
            root,
            env: vec![],
        }
    }

    /// A vault `notes` (two-way) on the mock, with its password stored.
    fn with_vault(mock: &Mock) -> Machine {
        let m = Machine::new();
        std::fs::write(
            m.root.join("config/lsync/lsync.toml"),
            format!(
                "[account.dev]\nurl = \"{}\"\nusername = \"u\"\n\n[vault.notes]\naccount = \"dev\"\ndatabase = \"{DB}\"\nfolder = \"{}\"\n",
                mock.url,
                m.folder("notes").display()
            ),
        )
        .unwrap();
        let secrets = m.root.join("config/lsync/secrets.toml");
        std::fs::write(&secrets, "\"dev\" = \"pw\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
        m
    }

    fn folder(&self, rel: &str) -> PathBuf {
        self.root.join("vaults").join(rel)
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/control.sock")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lsync"));
        c.env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("LSYNC_RUNTIME_DIR", self.root.join("run"))
            .env("RUST_LOG", "info")
            .current_dir(&self.root)
            .args(args);
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).stdin(Stdio::null()).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "lsync {args:?} failed ({:?}):\n{}\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            !out.status.success(),
            "lsync {args:?} succeeded:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.ok(args)).unwrap()
    }

    /// Start a long-running command; its log goes to `log`.
    fn spawn(&self, args: &[&str], log: &str) -> Service {
        let log = self.root.join(log);
        let f = std::fs::File::create(&log).unwrap();
        let child = self
            .cmd(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(f)
            .spawn()
            .unwrap();
        Service { child, log }
    }

    fn served_state(&self, vault: &str) -> Option<String> {
        let out = self.run(&["status", "--all", "--json"]);
        if !out.status.success() {
            return None;
        }
        let v: Value = serde_json::from_slice(&out.stdout).ok()?;
        v["vaults"]
            .as_array()?
            .iter()
            .find(|x| x["name"] == vault)
            .and_then(|x| x["state"].as_str().map(str::to_string))
    }
}

struct Service {
    child: Child,
    log: PathBuf,
}

impl Service {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// SIGTERM, and the exit code.
    fn stop(mut self) -> Option<i32> {
        // SAFETY: kill has no memory preconditions.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let end = Instant::now() + Duration::from_secs(30);
        while Instant::now() < end {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s.code();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        panic!("the service did not stop on SIGTERM:\n{}", self.log());
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The keys of a JSON report and the kind of each value (the counts differ
/// from run to run).
fn shape(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, at: &str, out: &mut Vec<String>) {
        let kind = match v {
            Value::Object(m) => {
                for (k, x) in m {
                    walk(x, &format!("{at}.{k}"), out);
                }
                return;
            }
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
        };
        out.push(format!("{at}: {kind}"));
    }
    walk(v, "", &mut out);
    out
}

/// The human lines with numbers taken out.
fn labels(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.chars().filter(|c| !c.is_ascii_digit() && *c != '.').collect())
        .collect()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn commands_for_a_served_vault_run_in_the_service_with_the_same_output() {
    let rt = runtime();
    let mock = rt.block_on(Mock::start());
    mock.put_text("Notes/a.md", &["note a"]);
    let m = Machine::with_vault(&mock);
    // Without a service: one-shot, as always.
    let local = m.json(&["--vault", "notes", "sync", "--json"]);
    let local_text = m.ok(&["--vault", "notes", "sync"]);
    let local_status = m.json(&["--vault", "notes", "status", "--json"]);
    assert!(m.folder("notes/Notes/a.md").exists());
    assert!(m.fails(&["pause"]).contains("no lsync service is running"));

    let svc = m.spawn(&["serve"], "serve.log");
    wait_until("the vault running in the service", || {
        m.served_state("notes").as_deref() == Some("running")
    });
    let dir_mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(m.root.join("run")).unwrap().permissions().mode() & 0o777
    };
    assert_eq!(dir_mode, 0o700);
    // A second service on the same runtime folder is refused.
    let e = m.fails(&["serve"]);
    assert!(e.contains("already running"), "{e}");

    // The same output through the service.
    let routed = m.json(&["--vault", "notes", "sync", "--json"]);
    assert_eq!(shape(&routed), shape(&local), "same report");
    let routed_text = m.ok(&["--vault", "notes", "sync"]);
    assert_eq!(labels(&routed_text), labels(&local_text));
    let routed_status = m.json(&["--vault", "notes", "status", "--json"]);
    for k in ["node_id", "vault", "index", "remote"] {
        assert_eq!(routed_status[k], local_status[k], "{k}");
    }
    assert!(m.json(&["--vault", "notes", "pull", "--json"])["changes"].is_u64());
    assert!(m.ok(&["--vault", "notes", "connect"]).contains("gate: OK"));
    assert!(svc.log().contains("running `sync` for a client"), "{}", svc.log());
    // With one vault there is nothing to name.
    assert!(m.ok(&["status"]).contains("node id"));

    // A local edit pushed by the service's own engine, seen as an event.
    let mut events = m
        .cmd(&["events", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let lines = BufReader::new(events.stdout.take().unwrap());
    std::thread::sleep(Duration::from_millis(500));
    mock.put_text("Notes/remote.md", &["from elsewhere"]);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in lines.lines().map_while(Result::ok) {
            if tx.send(l).is_err() {
                return;
            }
        }
    });
    let end = Instant::now() + Duration::from_secs(30);
    let mut got = Vec::new();
    while Instant::now() < end {
        if let Ok(l) = rx.recv_timeout(Duration::from_millis(200)) {
            let v: Value = serde_json::from_str(&l).unwrap();
            got.push(v["event"].as_str().unwrap_or_default().to_string());
            if v["event"] == "pulled" && v["vault"] == "notes" {
                break;
            }
        }
    }
    let _ = events.kill();
    let _ = events.wait();
    assert!(got.contains(&"pulled".to_string()), "events: {got:?}");

    // What can't run beside the service says what to do.
    let e = m.fails(&["--vault", "notes", "daemon"]);
    assert!(e.contains("already run by the lsync service"), "{e}");
    let e = m.fails(&["--vault", "notes", "--include", "Other", "sync"]);
    assert!(e.contains("other settings") && e.contains("folder scope"), "{e}");

    // Pause and resume.
    assert!(m.ok(&["pause"]).contains("paused: notes"));
    assert_eq!(m.served_state("notes").as_deref(), Some("paused"));
    assert!(m.ok(&["sync-now"]).contains("syncing now: none"));
    let e = m.fails(&["--vault", "notes", "sync-now"]);
    assert!(e.contains("is paused"), "{e}");
    // Paused, a command with other settings runs here, as without a service.
    let narrowed = m.json(&["--vault", "notes", "--include", "Notes", "status", "--json"]);
    assert!(narrowed["node_id"].is_string());
    assert!(m.ok(&["--vault", "notes", "resume"]).contains("resumed: notes"));
    wait_until("running again", || {
        m.served_state("notes").as_deref() == Some("running")
    });

    // A vault edit tells the service, which restarts that vault.
    m.ok(&["vault", "edit", "notes", "--device-name", "desk"]);
    wait_until("the new device name in the service", || {
        let v = m.json(&["status", "--all", "--json"]);
        v["vaults"][0]["settings"]["device_name"] == "desk" && v["vaults"][0]["state"] == "running"
    });
    let r = m.json(&["reload", "--json"]);
    assert_eq!(r["unchanged"], serde_json::json!(["notes"]));

    let socket = m.socket();
    assert_eq!(svc.stop(), Some(0));
    assert!(!socket.exists(), "the socket is removed at the end");
    // Without the service, the CLI opens the engine itself again.
    assert!(m.json(&["--vault", "notes", "status", "--json"])["node_id"].is_string());
}

#[test]
fn a_daemon_from_flags_serves_its_vault_and_the_bench_command_line_works() {
    let rt = runtime();
    let mock = rt.block_on(Mock::start());
    mock.put_text("a.md", &["a"]);
    let mut m = Machine::new();
    let vault = m.folder("flags").display().to_string();
    // In a dot-folder inside the vault, as the CLI has always allowed.
    let state = m.folder("flags/.lsync-state").display().to_string();
    m.env = vec![
        ("LSYNC_URL".into(), mock.url.clone()),
        ("LSYNC_DB".into(), DB.into()),
        ("LSYNC_USER".into(), "u".into()),
        ("LSYNC_PASSWORD".into(), "pw".into()),
        ("LSYNC_VAULT".into(), vault.clone()),
        ("LSYNC_STATE_DIR".into(), state),
    ];
    let d = m.spawn(&["daemon"], "daemon.log");
    wait_until("the daemon's first pull", || Path::new(&vault).join("a.md").exists());
    wait_until("the daemon's socket", || m.socket().exists());
    // `status` and `sync` with the same flags go through the daemon.
    let st = m.json(&["status", "--json"]);
    assert!(st["node_id"].is_string(), "{st}");
    let r = m.json(&["sync", "--json"]);
    assert!(r["push"].is_object(), "{r}");
    assert!(d.log().contains("running `sync` for a client"), "{}", d.log());
    // A second daemon for the same vault is refused, with what to do.
    let e = m.fails(&["daemon"]);
    assert!(e.contains("already run by the lsync service"), "{e}");
    assert_eq!(d.stop(), Some(0));

    // A profile file, as the bench unit runs it: `--profile NAME daemon`.
    let p = Machine::new();
    std::fs::write(
        p.root.join("config/lsync/bench.toml"),
        format!(
            "url = \"{}\"\ndatabase = \"{DB}\"\nusername = \"u\"\npassword = \"pw\"\nvault = \"{}\"\n",
            mock.url,
            p.folder("bench").display()
        ),
    )
    .unwrap();
    let d = p.spawn(&["--profile", "bench", "daemon"], "bench.log");
    wait_until("the bench daemon's pull", || p.folder("bench/a.md").exists());
    let st = p.json(&["--profile", "bench", "status", "--json"]);
    assert!(st["node_id"].is_string(), "{st}");
    assert_eq!(d.stop(), Some(0));

    // A daemon that cannot start fails at once, as before.
    let mut bad = Machine::new();
    bad.env = vec![
        ("LSYNC_URL".into(), "http://127.0.0.1:9".into()),
        ("LSYNC_DB".into(), DB.into()),
        ("LSYNC_VAULT".into(), bad.folder("x").display().to_string()),
        (
            "LSYNC_STATE_DIR".into(),
            bad.folder("x/visible-state").display().to_string(),
        ),
    ];
    let e = bad.fails(&["daemon"]);
    assert!(e.contains("visible folder inside the vault"), "{e}");
}

#[test]
fn serve_needs_an_lsync_toml() {
    let m = Machine::new();
    let e = m.fails(&["serve"]);
    assert!(e.contains("lsync vault add"), "{e}");
    assert!(!m.socket().exists());
}
