//! The service and its control socket on the engine's mock CouchDB: the
//! protocol over a real socket, the peer check, the socket directory's
//! safety, reload of only the vaults that changed, pause and resume, and
//! commands run for a client, alone and racing each other.

#[path = "../../livesync-engine/tests/common/mod.rs"]
mod common;

use common::{Mock, DB};
use livesync_engine::platform::LockFile;
use livesync_host::config::Dirs;
use livesync_host::control::{self, BindError, Client, ClientError, Policy};
use livesync_host::protocol::{code, method, Event, ReloadReport, StatusResult};
use livesync_host::secret::SecretStores;
use livesync_host::service::{ConfigSource, Service};
use livesync_host::Host;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

struct Fixture {
    rt: tokio::runtime::Runtime,
    mock: Mock,
    dir: tempfile::TempDir,
    dirs: Dirs,
}

impl Fixture {
    fn new() -> Fixture {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let mock = rt.block_on(Mock::start());
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            home: Some(dir.path().join("home")),
            config: dir.path().join("config/lsync"),
            state: dir.path().join("state/lsync"),
        };
        std::fs::create_dir_all(&dirs.config).unwrap();
        let secrets = dirs.config.join("secrets.toml");
        std::fs::write(&secrets, "\"dev\" = \"pw\"\n\"other\" = \"pw\"\n").unwrap();
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
        Fixture { rt, mock, dir, dirs }
    }

    /// Two accounts on the same mock server, as two servers (127.0.0.1 and
    /// localhost), and the vaults given.
    fn write_config(&self, vaults: &str) {
        let other = self.mock.url.replace("127.0.0.1", "localhost");
        let text = format!(
            "[account.dev]\nurl = \"{}\"\nusername = \"u\"\n\n[account.other]\nurl = \"{other}\"\nusername = \"u\"\n\n{vaults}",
            self.mock.url
        );
        std::fs::write(self.dirs.config_file(), text).unwrap();
    }

    fn service(&self) -> Arc<Service> {
        let host = Arc::new(Host::on_runtime(self.rt.handle().clone()));
        host.set_stop_timeouts_for_tests(Duration::from_secs(5), Duration::from_secs(5));
        let source = ConfigSource {
            path: self.dirs.config_file(),
            dirs: self.dirs.clone(),
            secrets: SecretStores::standard(self.dirs.config.join("secrets.toml")),
            only: None,
        };
        let _rt = self.rt.enter();
        let svc = Service::new(host, Box::new(source), "test");
        svc.spawn_activity();
        svc
    }

    fn reload(&self, svc: &Arc<Service>) -> ReloadReport {
        self.rt.block_on(svc.reload()).unwrap()
    }

    fn runtime_dir(&self) -> PathBuf {
        self.dir.path().join("run")
    }

    /// Bind and serve the socket; returns its path.
    fn serve(&self, svc: &Arc<Service>, policy: Policy) -> (PathBuf, tokio::sync::watch::Sender<bool>) {
        let bound = control::bind(&self.runtime_dir(), &policy).unwrap();
        let path = bound.path().to_path_buf();
        let (tx, rx) = tokio::sync::watch::channel(false);
        self.rt.spawn(control::serve(bound, svc.clone(), policy, rx));
        (path, tx)
    }

    fn home(&self, rel: &str) -> PathBuf {
        self.dir.path().join("home").join(rel)
    }
}

fn vault(name: &str, account: &str, folder: &str, extra: &str) -> String {
    format!("[vault.{name}]\naccount = \"{account}\"\ndatabase = \"{DB}\"\nfolder = \"~/{folder}\"\n{extra}\n")
}

fn state_of(svc: &Service, name: &str) -> String {
    svc.status(Some(name)).unwrap().vaults[0].state.clone()
}

/// Events seen by a subscriber on its own thread.
fn subscribe(path: &Path) -> Arc<Mutex<Vec<Event>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut c = Client::connect(path).unwrap().unwrap();
    c.subscribe(None).unwrap();
    let out = seen.clone();
    std::thread::spawn(move || {
        while let Ok(e) = c.next_event() {
            out.lock().unwrap().push(e);
        }
    });
    seen
}

fn raw_exchange(path: &Path, line: &str) -> Value {
    let mut s = std::os::unix::net::UnixStream::connect(path).unwrap();
    s.write_all(line.as_bytes()).unwrap();
    s.write_all(b"\n").unwrap();
    let mut r = BufReader::new(s);
    let mut out = String::new();
    r.read_line(&mut out).unwrap();
    serde_json::from_str(&out).unwrap()
}

#[test]
fn the_socket_speaks_the_protocol_and_streams_events() {
    let f = Fixture::new();
    f.mock.put_text("Notes/a.md", &["note a"]);
    f.mock.put_text("Work/b.md", &["work b"]);
    f.write_config(&format!(
        "{}{}",
        vault("notes", "dev", "notes", "include = [\"Notes\"]\nmode = \"pull-only\""),
        vault("work", "other", "work", "include = [\"Work\"]\nmode = \"pull-only\"")
    ));
    let svc = f.service();
    let r = f.reload(&svc);
    assert_eq!(r.started, vec!["notes".to_string(), "work".to_string()]);
    let (path, stop) = f.serve(&svc, Policy::own_user());
    wait_until("both vaults pulled", || {
        f.home("notes/Notes/a.md").exists() && f.home("work/Work/b.md").exists()
    });
    assert_eq!(svc.host().pooled_servers(), 2, "one connection pool per server");

    let mut c = Client::connect(&path).unwrap().expect("a service listens");
    let hello = c.call(method::HELLO, Value::Null).unwrap();
    assert_eq!(hello["protocol"], 1);
    assert!(hello["methods"].as_array().unwrap().contains(&json!("sync-now")));
    let st: StatusResult = serde_json::from_value(c.call(method::STATUS, Value::Null).unwrap()).unwrap();
    let names: Vec<&str> = st.vaults.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["notes", "work"]);
    let notes = &st.vaults[0];
    assert_eq!(notes.state, "running");
    let s = notes.settings.as_ref().unwrap();
    assert!(s.pull_only && s.scope_folders == ["notes"]);
    assert!(notes.status.as_ref().unwrap().node_id.is_some());
    assert!(
        !serde_json::to_string(&st).unwrap().contains("\"pw\""),
        "no password in status"
    );

    // Errors carry a code, for clients to act on.
    let e = c.call(method::STATUS, json!({"vault": "nope"})).unwrap_err();
    assert_eq!(e.code(), Some(code::UNKNOWN_VAULT));
    let e = c.call("frobnicate", Value::Null).unwrap_err();
    assert_eq!(e.code(), Some(code::UNKNOWN_METHOD));
    let e = c.call(method::PAUSE, json!({"vault": 3})).unwrap_err();
    assert_eq!(e.code(), Some(code::BAD_REQUEST));
    let v = raw_exchange(&path, r#"{"v":2,"id":"x","method":"status"}"#);
    assert_eq!(
        (v["id"].as_str(), v["error"]["code"].as_str()),
        (Some("x"), Some(code::UNSUPPORTED_VERSION))
    );
    let v = raw_exchange(&path, "this is not json");
    assert_eq!(v["error"]["code"], code::BAD_REQUEST);
    let v = raw_exchange(&path, r#"{"id":9,"method":"hello","later":true}"#);
    assert_eq!(
        (v["id"].as_u64(), v["result"]["protocol"].as_u64()),
        (Some(9), Some(1)),
        "v defaults to 1"
    );

    // A change on the server shows up as an event of that vault only.
    // The first pulls' events go out before anyone listens.
    std::thread::sleep(livesync_host::service::ACTIVITY_POLL * 3);
    let seen = subscribe(&path);
    f.mock.put_text("Notes/new.md", &["new"]);
    wait_until("the new note", || f.home("notes/Notes/new.md").exists());
    wait_until("the pulled event", || {
        seen.lock().unwrap().iter().any(|e| {
            e.event == "pulled" && e.vault.as_deref() == Some("notes") && e.data["written"].as_u64() >= Some(1)
        })
    });
    assert!(!seen
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.event == "pulled" && e.vault.as_deref() == Some("work")));
    // Pause and resume, over the socket, as events.
    let r = c.call(method::PAUSE, json!({"vault": "work"})).unwrap();
    assert_eq!(r["vaults"], json!(["work"]));
    assert_eq!(state_of(&svc, "work"), "paused");
    c.call(method::RESUME, Value::Null).unwrap();
    wait_until("work running again", || state_of(&svc, "work") == "running");
    let events = seen.lock().unwrap().clone();
    assert!(events.iter().any(|e| e.event == "paused" && e.data["paused"] == true));
    assert!(events.iter().any(|e| e.event == "paused" && e.data["paused"] == false));
    assert!(events
        .iter()
        .any(|e| e.event == "state" && e.vault.as_deref() == Some("work") && e.data["state"] == "running"));
    let _ = stop.send(true);
    f.rt.block_on(svc.shutdown());
    assert!(f.mock.writes().is_empty(), "pull-only vaults never write");
}

#[test]
fn a_connection_from_another_user_is_refused() {
    let f = Fixture::new();
    f.write_config("");
    let svc = f.service();
    f.reload(&svc);
    // The service admits only uid N + 1 (as if it ran as another user): this
    // test's own connection is the other user's, checked through the real
    // peer credentials of the socket.
    let other = Policy {
        uid: control::current_uid() + 1,
        group: None,
    };
    let (path, stop) = f.serve(&svc, other);
    let mut c = Client::connect(&path).unwrap().unwrap();
    let e = c.call(method::HELLO, Value::Null).unwrap_err();
    assert_eq!(e.code(), Some(code::FORBIDDEN), "{e}");
    // The connection is closed after the refusal.
    let e = c.call(method::HELLO, Value::Null).unwrap_err();
    assert!(matches!(e, ClientError::Io(_)), "{e}");
    let _ = stop.send(true);
    f.rt.block_on(svc.shutdown());
}

#[test]
fn the_socket_directory_is_private_and_held_by_one_service() {
    let d = tempfile::tempdir().unwrap();
    let run = d.path().join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A crashed service's socket, with no lock behind it.
    {
        let _stale = std::os::unix::net::UnixListener::bind(run.join(control::SOCKET_NAME)).unwrap();
    }
    assert!(run.join(control::SOCKET_NAME).exists());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _g = rt.enter();
    let bound = control::bind(&run, &Policy::own_user()).unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&run), 0o700, "the directory is made private");
    assert_eq!(mode(bound.path()), 0o600);
    // A second service on the same directory is refused, naming the holder.
    match control::bind(&run, &Policy::own_user()) {
        Err(e @ BindError::Running { .. }) => assert!(e.to_string().contains("already running"), "{e}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("a second service bound the socket"),
    }
    let sock = bound.path().to_path_buf();
    drop(bound);
    assert!(!sock.exists(), "the socket goes with the service");
    assert!(LockFile::try_acquire(&run.join(control::LOCK_NAME)).is_ok());

    // A link in place of the directory is refused.
    let link = d.path().join("link");
    std::os::unix::fs::symlink(&run, &link).unwrap();
    assert!(matches!(
        control::bind(&link, &Policy::own_user()),
        Err(BindError::Unsafe(_))
    ));
    // A wider folder that holds other files is not tightened, but refused.
    let shared = d.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::write(shared.join("notes.txt"), "x").unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        control::bind(&shared, &Policy::own_user()),
        Err(BindError::Unsafe(_))
    ));
    assert_eq!(mode(&shared), 0o755);
    // A client never uses a socket in a directory others can write to.
    let open = d.path().join("open");
    std::fs::create_dir(&open).unwrap();
    let _l = std::os::unix::net::UnixListener::bind(open.join(control::SOCKET_NAME)).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        Client::connect(&open.join(control::SOCKET_NAME)),
        Err(ClientError::Unsafe(_))
    ));
    // No socket, or a stale one: no service, not an error.
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).unwrap();
    drop(_l);
    assert!(Client::connect(&open.join(control::SOCKET_NAME)).unwrap().is_none());
    assert!(Client::connect(&run.join(control::SOCKET_NAME)).unwrap().is_none());
}

#[test]
fn reload_restarts_only_the_vaults_that_changed() {
    let f = Fixture::new();
    f.mock.put_text("A/a.md", &["a"]);
    let three = |b_extra: &str| {
        format!(
            "{}{}{}",
            vault("a", "dev", "a", "include = [\"A\"]\nmode = \"pull-only\""),
            vault(
                "b",
                "dev",
                "b",
                &format!("include = [\"A\"]\nmode = \"pull-only\"\n{b_extra}")
            ),
            vault("c", "other", "c", "include = [\"A\"]\nmode = \"pull-only\"")
        )
    };
    f.write_config(&three(""));
    let svc = f.service();
    let starts = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let mut rx = svc.subscribe();
        let starts = starts.clone();
        f.rt.spawn(async move {
            while let Ok(e) = rx.recv().await {
                if e.event == "state" && e.data["state"] == "starting" {
                    starts.lock().unwrap().push(e.vault.unwrap_or_default());
                }
            }
        });
    }
    let r = f.reload(&svc);
    assert_eq!(r.started, ["a", "b", "c"]);
    wait_until("three running", || {
        ["a", "b", "c"].iter().all(|n| state_of(&svc, n) == "running")
    });
    starts.lock().unwrap().clear();

    // Nothing changed: nothing restarts.
    let r = f.reload(&svc);
    assert_eq!((r.unchanged.len(), r.restarted.len()), (3, 0));
    // One vault's setting changed: only that one restarts.
    f.write_config(&three("device_name = \"desk\""));
    let r = f.reload(&svc);
    assert_eq!(r.restarted, ["b"]);
    assert_eq!(r.unchanged, ["a", "c"]);
    wait_until("b running again", || state_of(&svc, "b") == "running");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(*starts.lock().unwrap(), ["b"], "only b was started again");
    let st = svc.status(Some("b")).unwrap();
    assert_eq!(st.vaults[0].settings.as_ref().unwrap().device_name, "desk");

    // The account's password changed: its vaults restart (a, b), not c.
    std::fs::write(
        f.dirs.config.join("secrets.toml"),
        "\"dev\" = \"new\"\n\"other\" = \"pw\"\n",
    )
    .unwrap();
    let r = f.reload(&svc);
    assert_eq!(r.restarted, ["a", "b"]);
    assert_eq!(r.unchanged, ["c"]);

    // Removed, disabled and added vaults.
    f.write_config(&format!(
        "{}{}",
        vault(
            "a",
            "dev",
            "a",
            "include = [\"A\"]\nmode = \"pull-only\"\nenabled = false"
        ),
        vault("d", "dev", "d", "include = [\"A\"]\nmode = \"pull-only\"")
    ));
    let r = f.reload(&svc);
    assert_eq!(r.started, ["d"]);
    assert_eq!(r.stopped, ["a", "b", "c"]);
    assert_eq!(svc.names(), ["a", "d"]);
    assert_eq!(state_of(&svc, "a"), "disabled");
    assert!(svc.host().state("a").is_none() && svc.host().state("b").is_none());

    // A config that cannot be read changes nothing.
    std::fs::write(f.dirs.config_file(), "[vault.x]\nnot valid").unwrap();
    let e = f.rt.block_on(svc.reload()).unwrap_err();
    assert_eq!(e.code, code::INVALID_CONFIG);
    assert_eq!(svc.names(), ["a", "d"]);
    wait_until("d running", || state_of(&svc, "d") == "running");
    // A vault whose password is missing is reported, not started.
    f.write_config(&vault("e", "dev", "e", "include = [\"A\"]\nmode = \"pull-only\""));
    std::fs::write(f.dirs.config.join("secrets.toml"), "\"other\" = \"pw\"\n").unwrap();
    let r = f.reload(&svc);
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].message.contains("no password"), "{:?}", r.failed);
    assert_eq!(state_of(&svc, "e"), "invalid");
    f.rt.block_on(svc.shutdown());
}

/// A vault's state lock is free (no engine holds it).
fn released(state: &Path) -> bool {
    LockFile::try_acquire(&state.join("lsync.lock")).is_ok()
}

#[test]
fn pause_resume_and_commands_leave_every_vault_in_a_defined_state() {
    let f = Fixture::new();
    f.mock.put_text("Notes/a.md", &["a"]);
    f.write_config(&vault("notes", "dev", "notes", ""));
    let svc = f.service();
    f.reload(&svc);
    let state_dir = f.dirs.state.join("notes");
    wait_until("running", || {
        state_of(&svc, "notes") == "running" && f.home("notes/Notes/a.md").exists()
    });

    // Paused: the engine is stopped and its folders are free.
    assert_eq!(f.rt.block_on(svc.pause(Some("notes"))).unwrap(), ["notes"]);
    assert_eq!(state_of(&svc, "notes"), "paused");
    assert!(svc.host().state("notes").is_none());
    assert!(released(&state_dir));
    assert_eq!(svc.sync_now(Some("notes")).unwrap_err().code, code::PAUSED);
    assert!(svc.sync_now(None).unwrap().is_empty());
    // A command on a paused vault runs, and the vault stays paused.
    std::fs::write(f.home("notes/Notes/local.md"), "written here").unwrap();
    let p = livesync_host::protocol::CommandParams {
        vault: "notes".into(),
        ..Default::default()
    };
    let r =
        f.rt.block_on(svc.command(livesync_host::service::Command::Sync, p.clone()))
            .unwrap();
    assert_eq!(r.report["push"]["created"], 1, "{}", r.report);
    assert_eq!(state_of(&svc, "notes"), "paused");
    assert!(svc.host().state("notes").is_none());
    // Resumed, with a reload of the same config in between doing nothing.
    assert_eq!(f.reload(&svc).unchanged, ["notes"]);
    f.rt.block_on(svc.resume(None)).unwrap();
    wait_until("running again", || state_of(&svc, "notes") == "running");
    // A command on a running vault: it stops, runs, and starts again.
    let r =
        f.rt.block_on(svc.command(livesync_host::service::Command::Pull, p.clone()))
            .unwrap();
    assert!(r.report["changes"].as_u64().is_some(), "{}", r.report);
    wait_until("running after the pull", || state_of(&svc, "notes") == "running");

    // Everything at once, many times: each operation waits its turn, and
    // the vault ends paused with no engine, or not paused with one.
    for round in 0..3 {
        let mut tasks = Vec::new();
        for i in 0..12 {
            let svc = svc.clone();
            let p = p.clone();
            tasks.push(f.rt.spawn(async move {
                match (i + round) % 6 {
                    0 => drop(svc.pause(Some("notes")).await),
                    1 => drop(svc.resume(Some("notes")).await),
                    2 => drop(svc.sync_now(Some("notes"))),
                    3 => drop(svc.reload().await),
                    4 => drop(svc.command(livesync_host::service::Command::Sync, p).await),
                    _ => drop(svc.pause(None).await),
                }
            }));
        }
        for t in tasks {
            f.rt.block_on(t).unwrap();
        }
        let v = svc.status(Some("notes")).unwrap().vaults.remove(0);
        assert!(v.command.is_none());
        assert_eq!(
            v.paused,
            svc.host().state("notes").is_none(),
            "round {round}: {}",
            v.state
        );
        if v.paused {
            assert!(released(&state_dir));
        }
    }
    f.rt.block_on(svc.resume(None)).unwrap();
    wait_until("running at the end", || state_of(&svc, "notes") == "running");
    f.rt.block_on(svc.shutdown());
    assert!(svc.host().active().is_empty());
    assert!(released(&state_dir));
    // After shutdown, nothing more is taken.
    assert_eq!(f.rt.block_on(svc.resume(None)).unwrap_err().code, code::SHUTTING_DOWN);
}
