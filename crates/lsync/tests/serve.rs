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
fn a_served_vault_syncs_what_its_settings_say() {
    let rt = runtime();
    let mock = rt.block_on(Mock::start());
    mock.put_text("Notes/a.md", &["note a"]);
    mock.put_text("Notes/skip/b.md", &["ignored here"]);
    let m = Machine::with_vault(&mock);
    let cfg = m.root.join("config/lsync/lsync.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("mode = \"pull-only\"\nignore_files = [\".gitignore\"]\n");
    std::fs::write(&cfg, text).unwrap();
    std::fs::create_dir_all(m.folder("notes/Notes")).unwrap();
    std::fs::write(m.folder("notes/Notes/.gitignore"), "skip/\n").unwrap();

    let svc = m.spawn(&["serve"], "serve.log");
    wait_until("the vault running in the service", || {
        m.served_state("notes").as_deref() == Some("running") && m.folder("notes/Notes/a.md").exists()
    });
    // The ignore file holds in the service's engine.
    assert!(!m.folder("notes/Notes/skip").exists());
    let settings = m.json(&["status", "--all", "--json"])["vaults"][0]["settings"].clone();
    assert_eq!(settings["use_ignore_files"], true);
    assert_eq!(settings["ignore_files"], serde_json::json!([".gitignore"]));
    assert_eq!(settings["trash"], "vault");
    // A remote delete goes to the vault's trash.
    mock.tombstone("Notes/a.md");
    wait_until("the delete in the trash", || {
        !m.folder("notes/Notes/a.md").exists() && m.folder("notes/.trash/Notes/a.md").exists()
    });
    // What-syncs settings given only to a command are refused, as the others.
    let e = m.fails(&["--vault", "notes", "--sync-only-regex", "^Notes/", "pull"]);
    assert!(e.contains("other settings") && e.contains("sync-only regex"), "{e}");
    let e = m.fails(&["--vault", "notes", "--trash", "none", "pull"]);
    assert!(e.contains("trash none (the service: vault)"), "{e}");
    assert!(m.json(&["--vault", "notes", "pull", "--json"])["changes"].is_u64());

    // An edit restarts the vault with it.
    m.ok(&["vault", "edit", "notes", "--trash", "none"]);
    wait_until("the new trash setting in the service", || {
        let v = m.json(&["status", "--all", "--json"]);
        v["vaults"][0]["settings"]["trash"] == "none" && v["vaults"][0]["state"] == "running"
    });
    assert_eq!(svc.stop(), Some(0));
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

#[test]
fn a_socket_this_user_cannot_reach_means_running_here() {
    use std::os::unix::fs::PermissionsExt;
    let rt = runtime();
    let mock = rt.block_on(Mock::start());
    mock.put_text("Notes/a.md", &["note a"]);
    let m = Machine::with_vault(&mock);
    // As a system service's folder looks to a user outside its group.
    let run = m.root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o000)).unwrap();
    let synced = m.run(&["--vault", "notes", "sync"]);
    let status = m.run(&["--vault", "notes", "status"]);
    let pause = m.run(&["pause"]);
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    let err = |o: &Output| String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(synced.status.success(), "{}", err(&synced));
    assert!(m.folder("notes/Notes/a.md").exists());
    assert!(status.status.success(), "{}", err(&status));
    assert!(err(&pause).contains("no lsync service is running"), "{}", err(&pause));
}

#[test]
fn serve_with_one_vault_stops_it_when_it_is_removed() {
    let rt = runtime();
    let mock = rt.block_on(Mock::start());
    let m = Machine::with_vault(&mock);
    let e = m.fails(&["serve", "--vault", "nope"]);
    assert!(e.contains("no vault named \"nope\""), "{e}");
    let svc = m.spawn(&["serve", "--vault", "notes"], "serve.log");
    wait_until("the vault running in the service", || {
        m.served_state("notes").as_deref() == Some("running")
    });
    m.ok(&["vault", "rm", "notes"]);
    wait_until("the vault gone from the service", || {
        m.run(&["status", "--all", "--json"]).status.success() && m.served_state("notes").is_none()
    });
    let r = m.ok(&["reload"]);
    assert!(r.contains("unchanged  -"), "{r}");
    assert_eq!(svc.stop(), Some(0));
}

/// The vault in the service's `status --all`, `Null` while there is none.
fn served_view(m: &Machine, vault: &str) -> Value {
    let out = m.run(&["status", "--all", "--json"]);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    v["vaults"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"] == vault))
        .cloned()
        .unwrap_or(Value::Null)
}

#[test]
fn the_plugin_guard_holds_in_the_service_and_a_routed_command_can_hand_over() {
    let rt = runtime();
    let mock = rt.block_on(Mock::start());
    mock.put_text("Notes/a.md", &["note a"]);
    let m = Machine::with_vault(&mock);
    let o = m.folder("notes/.obsidian");
    std::fs::create_dir_all(o.join("plugins/obsidian-livesync")).unwrap();
    std::fs::write(o.join("community-plugins.json"), r#"["obsidian-livesync"]"#).unwrap();

    let svc = m.spawn(&["serve"], "serve.log");
    // The service's engine refuses the vault, says why, and runs nothing.
    wait_until("the refusal in the service", || {
        let v = served_view(&m, "notes");
        v["state"] == "failed"
            && v["message"]
                .as_str()
                .is_some_and(|s| s.contains("Self-hosted LiveSync plugin enabled"))
    });
    assert!(!m.folder("notes/Notes/a.md").exists());
    assert_eq!(served_view(&m, "notes")["settings"]["allow_plugin"], false);
    // A routed sync is refused the same way...
    let out = m.run(&["--vault", "notes", "sync"]);
    let e = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{e}");
    assert!(e.contains("plugin enabled") && e.contains("--allow-plugin"), "{e}");
    assert!(svc.log().contains("running `sync` for a client"), "{}", svc.log());
    assert!(mock.writes().is_empty(), "nothing written past the guard");
    // ... and `--allow-plugin` hands that one command over, in the service.
    m.ok(&["--vault", "notes", "sync", "--allow-plugin"]);
    assert!(m.folder("notes/Notes/a.md").exists());
    // The service's own engine stays refused.
    wait_until("the vault refused again", || {
        served_view(&m, "notes")["state"] == "failed"
    });

    // `allow_plugin = true` for the vault lets the service run it.
    let cfg = m.root.join("config/lsync/lsync.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("allow_plugin = true\n");
    std::fs::write(&cfg, text).unwrap();
    let r = m.json(&["reload", "--json"]);
    assert_eq!(r["restarted"], serde_json::json!(["notes"]), "{r}");
    wait_until("the vault running", || {
        let v = served_view(&m, "notes");
        v["state"] == "running" && v["settings"]["allow_plugin"] == true
    });
    assert_eq!(svc.stop(), Some(0));
}

#[test]
fn the_guards_stops_show_in_the_service_status_and_events() {
    let rt = runtime();
    let mock = rt.block_on(Mock::start());
    mock.put_text("Notes/a.md", &["note a"]);
    let m = Machine::with_vault(&mock);
    // The guard's and the connection's timings from lsync.toml.
    let cfg = m.root.join("config/lsync/lsync.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("regate_secs = 1\nheartbeat_secs = 1\nlongpoll_timeout_secs = 3\n");
    std::fs::write(&cfg, text).unwrap();
    let svc = m.spawn(&["serve"], "serve.log");
    wait_until("the vault running in the service", || {
        m.served_state("notes").as_deref() == Some("running") && m.folder("notes/Notes/a.md").exists()
    });
    let events = Events::follow(&m);
    mock.put(serde_json::json!({"_id": "obsydian_livesync_version", "type": "versioninfo", "version": 13, "used_features": ["teleport-v1"]}));
    wait_until("the upstream stop in status --all", || {
        let t = m.ok(&["status", "--all"]);
        t.contains("UPSTREAM STOP") && t.contains("teleport-v1")
    });
    let st = m.json(&["--vault", "notes", "status", "--json"]);
    let stop = st["upstream_stop"].as_str().unwrap_or_default().to_string();
    assert!(stop.contains("is the newest version known to work"), "{stop}");
    let e = events.wait_for("upstream-stop", |d| d["stop"].as_str().is_some());
    assert!(e["stop"].as_str().unwrap().contains("teleport-v1"), "{e}");
    // The error that mirrors it, once.
    events.wait_for("error", |d| {
        d["message"].as_str().is_some_and(|m| m.contains("teleport-v1"))
    });
    // A state, not a tick: the regates (every second) refuse again for the
    // same reason, and neither the stop nor the error is reported again,
    // also past the service's periodic status read (every 10 s).
    let regated = mock.state.lock().unwrap().requests.len();
    let again = events.kinds_within(Duration::from_millis(11_500));
    assert!(mock.state.lock().unwrap().requests.len() > regated, "regated meanwhile");
    assert!(
        !again.iter().any(|k| k == "upstream-stop" || k == "error"),
        "reported again: {again:?}"
    );
    // Back to what lsync knows: the vault resumes by itself.
    mock.put(serde_json::json!({"_id": "obsydian_livesync_version", "type": "versioninfo", "version": 12}));
    wait_until("the stop cleared", || {
        m.json(&["--vault", "notes", "status", "--json"])["upstream_stop"].is_null()
    });
    events.wait_for("upstream-stop", |d| d["stop"].is_null());
    mock.put_text("Notes/b.md", &["note b"]);
    wait_until("syncing again", || m.folder("notes/Notes/b.md").exists());

    // Another device locks the database without rebuilding it: a state of
    // its own, naming that device, and nothing is unlocked.
    mock.update_milestone(|ms| {
        ms["locked"] = serde_json::json!(true);
        ms["accepted_nodes"] = serde_json::json!(["other"]);
        ms["node_info"]["other"] = serde_json::json!({"device_name": "laptop", "last_connected": 1});
    });
    wait_until("the lock in status --all", || {
        let t = m.ok(&["status", "--all"]);
        t.contains("LOCKED BY ANOTHER DEVICE") && t.contains("laptop")
    });
    let e = events.wait_for("remote-locked", |d| d["lock"].as_str().is_some());
    assert!(e["lock"].as_str().unwrap().contains("not rebuilt"), "{e}");
    assert_eq!(
        mock.local("obsydian_livesync_milestone").unwrap()["accepted_nodes"],
        serde_json::json!(["other"])
    );
    drop(events);
    assert_eq!(svc.stop(), Some(0));
}

/// `lsync events --json`, read on a thread of its own.
struct Events {
    child: Child,
    rx: std::sync::mpsc::Receiver<Value>,
}

impl Events {
    fn follow(m: &Machine) -> Events {
        let mut child = m
            .cmd(&["events", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for l in lines.lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&l) {
                    if tx.send(v).is_err() {
                        return;
                    }
                }
            }
        });
        // Subscribed before the caller makes anything happen.
        std::thread::sleep(Duration::from_millis(500));
        Events { child, rx }
    }

    /// The data of the next `kind` event that `f` accepts.
    fn wait_for(&self, kind: &str, f: impl Fn(&Value) -> bool) -> Value {
        let end = Instant::now() + Duration::from_secs(30);
        let mut seen = Vec::new();
        while Instant::now() < end {
            if let Ok(v) = self.rx.recv_timeout(Duration::from_millis(200)) {
                if v["event"] == kind && f(&v["data"]) {
                    return v["data"].clone();
                }
                seen.push(v["event"].as_str().unwrap_or_default().to_string());
            }
        }
        panic!("no {kind} event; saw {seen:?}");
    }
}

impl Events {
    /// The kinds of the events that come within `d`.
    fn kinds_within(&self, d: Duration) -> Vec<String> {
        let end = Instant::now() + d;
        let mut kinds = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            if let Ok(v) = self.rx.recv_timeout(left) {
                kinds.push(v["event"].as_str().unwrap_or_default().to_string());
            }
        }
        kinds
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// #11 and spec D7: `lsync allow-deletes --stop TEXT` confirms the stop
/// that was read, here (with a sync) or in the service that runs the vault;
/// another stop's text or no stop allows nothing.
#[test]
fn allow_deletes_confirms_the_stop_that_was_read_here_or_in_the_service() {
    let rt = runtime();
    let mock = rt.block_on(Mock::start());
    for i in 0..30 {
        mock.put_text(&format!("Notes/f{i:02}.md"), &[&format!("note {i}")]);
    }
    let m = Machine::with_vault(&mock);
    let live = |p: &str| mock.live_paths().iter().any(|l| l == p);
    let stop_of = || {
        m.json(&["--vault", "notes", "status", "--json"])["safety_stop"]
            .as_str()
            .map(str::to_string)
    };
    m.ok(&["--vault", "notes", "sync"]);
    for i in 0..25 {
        std::fs::remove_file(m.folder(&format!("notes/Notes/f{i:02}.md"))).unwrap();
    }
    assert_eq!(m.run(&["--vault", "notes", "sync"]).status.code(), Some(3));
    let stop = stop_of().expect("a stop");
    assert!(m.ok(&["--vault", "notes", "status"]).contains("lsync allow-deletes"));
    // Another stop's text: nothing allowed, exit 2.
    let out = m.run(&["--vault", "notes", "allow-deletes", "--stop", "1 an older stop"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("25 of 30"));
    assert!((0..25).all(|i| live(&format!("Notes/f{i:02}.md"))));
    // The stop read: deleted here, by a sync.
    let rep = m.json(&["--vault", "notes", "allow-deletes", "--stop", &stop, "--json"]);
    assert_eq!(rep["push"]["deleted"], 25, "{rep}");
    assert!(stop_of().is_none());
    assert_eq!(
        m.run(&["--vault", "notes", "allow-deletes", "--stop", &stop])
            .status
            .code(),
        Some(1),
        "no stop"
    );

    // In the service.
    for i in 30..60 {
        mock.put_text(&format!("Notes/f{i:02}.md"), &[&format!("note {i}")]);
    }
    let svc = m.spawn(&["serve"], "serve.log");
    wait_until("the new notes pulled by the service", || {
        m.served_state("notes").as_deref() == Some("running") && m.folder("notes/Notes/f59.md").exists()
    });
    for i in 30..55 {
        std::fs::remove_file(m.folder(&format!("notes/Notes/f{i:02}.md"))).unwrap();
    }
    let end = Instant::now() + Duration::from_secs(60);
    let stop = loop {
        if let Some(s) = stop_of() {
            break s;
        }
        assert!(Instant::now() < end, "no stop in the service:\n{}", svc.log());
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(stop.contains("25 of 35"), "{stop}");
    let out = m.run(&["--vault", "notes", "allow-deletes", "--stop", "1 an older stop"]);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    let r = m.json(&["--vault", "notes", "allow-deletes", "--stop", &stop, "--json"]);
    assert_eq!(r["vaults"], serde_json::json!(["notes"]));
    wait_until("the service's confirmed deletions", || {
        (30..55).all(|i| !live(&format!("Notes/f{i:02}.md")))
    });
    wait_until("the stop to clear", || stop_of().is_none());
    assert!((55..60).all(|i| live(&format!("Notes/f{i:02}.md"))));
    assert!(svc.log().contains("confirmed by a client"), "{}", svc.log());
    assert_eq!(svc.stop(), Some(0));
}
