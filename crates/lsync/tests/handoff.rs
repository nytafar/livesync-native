//! `lsync flush`, `lsync wait`, `lsync status --follow --waybar` and the
//! service's notifications, through the binary, with two machines (each its
//! own HOME, config, state and runtime folders) on one mock CouchDB: the
//! handoff with and without a service, the exit codes for timeouts and
//! stops, a status line per change, and one notification per conflict
//! through a fake notifier (never the desktop's own).

#[path = "../../livesync-engine/tests/common/mod.rs"]
mod common;

use common::{Mock, DB};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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

struct Machine {
    _dir: tempfile::TempDir,
    root: PathBuf,
    env: Vec<(String, String)>,
}

impl Machine {
    /// A machine with the vault `notes` (two-way) on the mock.
    fn new(mock: &Mock) -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["home", "config/lsync", "state", "vaults/notes"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(
            root.join("config/lsync/lsync.toml"),
            format!(
                "[account.dev]\nurl = \"{}\"\nusername = \"u\"\n\n[vault.notes]\naccount = \"dev\"\ndatabase = \"{DB}\"\nfolder = \"{}\"\n",
                mock.url,
                root.join("vaults/notes").display()
            ),
        )
        .unwrap();
        let secrets = root.join("config/lsync/secrets.toml");
        std::fs::write(&secrets, "\"dev\" = \"pw\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
        Machine {
            _dir: dir,
            root,
            env: vec![],
        }
    }

    fn file(&self, rel: &str) -> PathBuf {
        self.root.join("vaults/notes").join(rel)
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

    /// Run, expecting exit code `code`; stdout.
    fn exits(&self, code: i32, args: &[&str]) -> String {
        let out = self.run(args);
        assert_eq!(
            out.status.code(),
            Some(code),
            "lsync {args:?}:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.exits(0, args)).unwrap()
    }

    fn serve(&self, args: &[&str]) -> Served {
        let log = self.root.join("serve.log");
        let mut all = vec!["serve"];
        all.extend_from_slice(args);
        let child = self
            .cmd(&all)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let s = Served { child, log };
        wait_until("the vault live in the service", || {
            let out = self.run(&["status", "--all", "--json"]);
            serde_json::from_slice::<Value>(&out.stdout).is_ok_and(|v| {
                v["vaults"][0]["activity"]["pull"] == "live" && v["vaults"][0]["activity"]["pushes"] == true
            })
        });
        s
    }
}

struct Served {
    child: Child,
    log: PathBuf,
}

impl Served {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn stop(mut self) {
        // SAFETY: kill has no memory preconditions.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let end = Instant::now() + Duration::from_secs(30);
        while Instant::now() < end {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the service did not stop:\n{}", self.log());
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_file_flushed_on_one_machine_is_waited_for_on_the_other() {
    let rt = runtime();
    let mock = rt.block_on(Mock::start());
    let a = Machine::new(&mock);
    let b = Machine::new(&mock);
    let sa = a.serve(&[]);
    let sb = b.serve(&[]);

    std::fs::write(a.file("handoff.md"), "from machine a").unwrap();
    let rep = a.json(&["flush", "handoff.md", "--json"]);
    assert_eq!(rep["complete"], true, "{rep}");
    let rev = rep["paths"][0]["rev"].as_str().unwrap().to_string();
    assert_eq!(mock.doc("handoff.md").unwrap()["_rev"], rev.as_str());
    // As people read it: the outcome, the revision, the path.
    std::fs::write(a.file("second.md"), "two").unwrap();
    let text = a.exits(0, &["flush", "second.md"]);
    assert!(text.starts_with("stored") && text.contains("second.md"), "{text}");

    let at = format!("handoff.md@{rev}");
    let r = b.json(&["wait", "--applied", &at, "--timeout", "30", "--json"]);
    assert_eq!(
        (r["condition"].as_str(), r["current_rev"].as_str()),
        (Some("applied"), Some(rev.as_str()))
    );
    assert_eq!(std::fs::read_to_string(b.file("handoff.md")).unwrap(), "from machine a");
    // A path given as a file path inside the vault, and the text output.
    let abs = b.file("handoff.md").display().to_string();
    let text = b.exits(0, &["wait", "--applied", &abs, "--timeout", "30"]);
    assert!(text.contains(&format!("handoff.md is at {rev}")), "{text}");
    assert!(b.exits(0, &["wait", "--idle"]).contains("notes is idle"));

    // Exit codes an agent can act on: 124 on timeout, on time.
    let t0 = Instant::now();
    let out = b.run(&["wait", "--applied", "handoff.md@9-0000", "--timeout", "1"]);
    assert_eq!(out.status.code(), Some(124), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("9-0000"));
    assert!(t0.elapsed() < Duration::from_secs(10));
    // 2 when it cannot happen without a person: a paused vault, a stop.
    b.exits(0, &["--vault", "notes", "pause"]);
    let out = b.run(&["wait", "--idle", "--timeout", "5"]);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    b.exits(0, &["--vault", "notes", "resume"]);
    // 3 when the flush is done but not everything is on the server.
    let out = a.run(&["flush", "nope.md", "--json"]);
    assert_eq!(out.status.code(), Some(3));
    let rep: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rep["paths"][0]["outcome"], "missing");
    // Exactly one condition.
    let out = b.run(&["wait"]);
    assert_eq!(out.status.code(), Some(2), "clap's usage error");
    assert!(String::from_utf8_lossy(&out.stderr).contains("--applied"));

    sa.stop();
    sb.stop();

    // Without a service: a one-shot sync flushes, and the wait runs the
    // vault here (read-only) until the revision is in.
    std::fs::write(a.file("offline.md"), "no service").unwrap();
    let rep = a.json(&["flush", "--json"]);
    assert_eq!(rep["all"], true);
    let stored: Vec<&Value> = rep["paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["path"] == "offline.md")
        .collect();
    let rev = stored[0]["rev"].as_str().unwrap().to_string();
    let r = b.json(&[
        "wait",
        "--applied",
        &format!("offline.md@{rev}"),
        "--timeout",
        "30",
        "--json",
    ]);
    assert_eq!(r["current_rev"], rev.as_str());
    assert_eq!(std::fs::read_to_string(b.file("offline.md")).unwrap(), "no service");
    let out = b.run(&["wait", "--applied", "offline.md@9-0000", "--timeout", "1"]);
    assert_eq!(out.status.code(), Some(124), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(b.json(&["wait", "--idle", "--json"])["condition"] == "idle");
}

#[test]
fn the_status_line_follows_the_service_and_conflicts_are_notified_once() {
    let rt = runtime();
    let mock = rt.block_on(Mock::start());
    let mut a = Machine::new(&mock);
    // The fake notifier: a script that appends what it is asked to show.
    let told = a.root.join("told.txt");
    let script = a.root.join("notify.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s|%s|%s|%s\\n' \"$LSYNC_NOTIFY_KIND\" \"$LSYNC_NOTIFY_VAULT\" \"$1\" \"$2\" >> '{}'\n",
            told.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    a.env
        .push(("LSYNC_NOTIFY_COMMAND".into(), script.display().to_string()));

    // No service yet: the bar says so (and a one-shot line exits).
    let off: Value = serde_json::from_str(a.exits(0, &["status", "--waybar"]).trim()).unwrap();
    assert_eq!(
        (off["class"].as_str(), off["text"].as_str()),
        (Some("off"), Some("off"))
    );

    let served = a.serve(&[]);
    let mut follow = a
        .cmd(&["status", "--follow", "--waybar"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    let stdout = follow.stdout.take().unwrap();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(serde_json::from_str(&l).unwrap()).is_err() {
                return;
            }
        }
    });
    let next_class = |want: &str| -> Value {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let v = rx.recv_timeout(left).unwrap_or_else(|_| panic!("no {want} line"));
            if v["class"] == want {
                return v;
            }
        }
    };
    let ok = next_class("ok");
    assert_eq!(ok["tooltip"], "notes: synced");

    // A conflict: told once, and shown.
    let rev = mock.put_text("c.md", &["base"]);
    wait_until("c.md pulled", || a.file("c.md").exists());
    // Two edits of the same revision: two branches.
    let doc = mock.doc("c.md").unwrap();
    mock.put_text("c.md", &["edited"]);
    mock.put_branch(
        serde_json::json!({"_id": "c.md", "path": "c.md", "type": "plain", "children": doc["children"], "ctime": 1, "mtime": 2, "size": 4, "eden": {}}),
        &rev,
    );
    let c = next_class("conflict");
    assert_eq!(c["text"], "1 conflict");
    wait_until("the notification", || {
        std::fs::read_to_string(&told).is_ok_and(|t| !t.is_empty())
    });
    // More changes to the same file in conflict: no second notice.
    mock.put_text("c.md", &["edited again"]);
    wait_until("the second edit", || {
        std::fs::read_to_string(a.file("c.md")).is_ok_and(|t| t == "edited again")
    });
    std::thread::sleep(Duration::from_secs(2));
    // One notice (its body has two lines).
    let all = std::fs::read_to_string(&told).unwrap();
    let notices: Vec<&str> = all.lines().filter(|l| l.contains('|')).collect();
    assert_eq!(
        notices,
        ["conflict|notes|lsync: a conflict in notes|c.md"],
        "{all}\n{}",
        served.log()
    );

    // Paused: a new line.
    a.exits(0, &["--vault", "notes", "pause"]);
    assert_eq!(next_class("paused")["text"], "paused");
    // The service goes away: off, and the follower keeps going.
    served.stop();
    assert_eq!(next_class("off")["tooltip"], "the lsync service stopped");
    assert!(follow.try_wait().unwrap().is_none(), "it follows on");
    let _ = follow.kill();
    let _ = follow.wait();
}
