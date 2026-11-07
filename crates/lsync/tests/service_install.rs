//! `lsync service install|status|uninstall` through the binary, into a
//! machine of temp folders. `PATH` holds only fake `systemctl`, `loginctl`
//! and `launchctl` scripts that record their arguments, so no command ever
//! reaches this machine's service manager.
//!
//! The binary manages systemd units on Linux and a LaunchAgent on macOS
//! (where `--system` is refused), so the unit tests run on Linux and the
//! LaunchAgent tests on macOS.

use std::path::PathBuf;
use std::process::{Command, Output};

struct Machine {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["home", "config/lsync", "state", "bin", "run"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(
            root.join("config/lsync/lsync.toml"),
            "[account.dev]\nurl = \"http://127.0.0.1:9\"\nusername = \"u\"\n\n[vault.notes]\naccount = \"dev\"\ndatabase = \"notes\"\nfolder = \"~/Notes\"\n",
        )
        .unwrap();
        let log = root.join("calls.log");
        for (prog, answer) in [
            ("systemctl", "enabled"),
            ("loginctl", "no"),
            ("launchctl", "state = running"),
        ] {
            let p = root.join("bin").join(prog);
            std::fs::write(
                &p,
                format!("#!/bin/sh\necho \"{prog} $*\" >> '{}'\necho {answer}\n", log.display()),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Machine { _dir: dir, root }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lsync"));
        c.env_clear()
            .env("PATH", self.root.join("bin"))
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("LSYNC_RUNTIME_DIR", self.root.join("run"))
            .current_dir(&self.root)
            .args(args);
        c
    }

    fn ok(&self, args: &[&str]) -> String {
        let out: Output = self.cmd(args).output().unwrap();
        assert!(
            out.status.success(),
            "lsync {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The commands run, but the read-only `systemctl show` queries.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.contains(" show "))
            .map(str::to_string)
            .collect()
    }

    /// `systemctl --user show --property=UnitPath` answers `paths`.
    #[cfg(target_os = "linux")]
    fn manager_reads(&self, paths: &str) {
        let p = self.root.join("bin/systemctl");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\necho \"systemctl $*\" >> '{}'\ncase \"$*\" in *UnitPath*) echo '{paths}';; *) echo enabled;; esac\n",
                self.root.join("calls.log").display()
            ),
        )
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    fn unit(&self) -> PathBuf {
        self.root.join("config/systemd/user/lsync.service")
    }

    #[cfg(target_os = "macos")]
    fn plist(&self) -> PathBuf {
        self.root.join("home/Library/LaunchAgents/no.nyta.lsync.plist")
    }
}

/// `systemd-analyze verify` on a unit file, when this machine has it.
#[cfg(target_os = "linux")]
fn verify(path: &std::path::Path) {
    let Ok(v) = Command::new("systemd-analyze").arg("--version").output() else {
        eprintln!("skipped systemd-analyze verify: not installed");
        return;
    };
    if !v.status.success() {
        return;
    }
    let out = Command::new("systemd-analyze")
        .arg("verify")
        .arg(path)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    let ours: Vec<&str> = err.lines().filter(|l| l.contains("lsync.service")).collect();
    assert!(out.status.success() && ours.is_empty(), "{err}");
}

// systemd units: Linux only (macOS gets a LaunchAgent, tested below).
#[cfg(target_os = "linux")]
#[test]
fn a_user_unit_is_not_written_where_the_user_manager_does_not_look() {
    // The testbed's machines set XDG_CONFIG_HOME; the user's manager reads
    // its own (finding 13 of the review).
    let m = Machine::new();
    m.manager_reads("/home/u/.config/systemd/user /usr/lib/systemd/user");
    let out = m.cmd(&["service", "install"]).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && err.contains("does not read units from"),
        "{err}"
    );
    assert!(!m.unit().exists());
    assert!(m.calls().is_empty(), "{:?}", m.calls());
    m.manager_reads(&format!(
        "/usr/lib/systemd/user {}",
        m.root.join("config/systemd/user").display()
    ));
    m.ok(&["service", "install", "--no-start"]);
    assert!(m.unit().exists());
}

// systemd units: Linux only (macOS gets a LaunchAgent, tested below).
#[cfg(target_os = "linux")]
#[test]
fn a_user_service_installs_into_the_config_home_and_uninstalls() {
    let m = Machine::new();
    let out = m.ok(&["service", "install"]);
    assert!(out.contains("enabled and started lsync.service"), "{out}");
    assert!(out.contains("--linger"), "a hint for servers: {out}");
    let unit = std::fs::read_to_string(m.unit()).unwrap();
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_lsync")).unwrap();
    assert!(unit.contains(&format!("ExecStart={} serve\n", exe.display())), "{unit}");
    assert!(
        unit.contains(&format!("ExecReload={} reload\n", exe.display())),
        "{unit}"
    );
    // The service finds the same config, state and socket as this CLI.
    for k in ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "LSYNC_RUNTIME_DIR"] {
        assert!(unit.contains(&format!("Environment=\"{k}=")), "{k}: {unit}");
    }
    verify(&m.unit());
    let calls = m.calls();
    assert_eq!(
        calls[..3],
        [
            "systemctl --user daemon-reload",
            "systemctl --user enable lsync.service",
            "systemctl --user restart lsync.service"
        ]
    );

    let st: serde_json::Value = serde_json::from_str(&m.ok(&["service", "status", "--json"])).unwrap();
    assert_eq!(st["installed"], true);
    assert_eq!(st["manager"], "systemd-user");
    assert_eq!(st["socket"]["answers"], false);

    m.ok(&["service", "install", "--linger", "--no-start"]);
    assert!(m.calls().iter().any(|c| c.starts_with("loginctl enable-linger ")));

    let out = m.ok(&["service", "uninstall"]);
    assert!(out.contains("removed"), "{out}");
    assert!(!m.unit().exists());
    assert!(m.root.join("config/lsync/lsync.toml").exists());
    assert!(m
        .calls()
        .contains(&"systemctl --user disable --now lsync.service".to_string()));
    let out = m.cmd(&["service", "status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stdout).contains("not installed"));
}

// systemd units: Linux only (macOS gets a LaunchAgent, tested below).
#[cfg(target_os = "linux")]
#[test]
fn a_system_unit_is_printed_without_root_and_installed_only_with_it() {
    let m = Machine::new();
    let etc = m.root.join("etc/lsync");
    std::fs::create_dir_all(&etc).unwrap();
    std::fs::copy(m.root.join("config/lsync/lsync.toml"), etc.join("lsync.toml")).unwrap();
    let me = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
    let me = me.trim();
    let print = |extra: &[&str]| {
        let mut args = vec!["--system", "service", "install", "--print", "--run-as", me];
        args.extend_from_slice(extra);
        let mut c = m.cmd(&args);
        c.env("CONFIGURATION_DIRECTORY", &etc);
        let out = c.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let text = print(&[]);
    assert!(text.contains(&format!("User={me}\n")), "{text}");
    assert!(text.contains(" --system serve\n"), "{text}");
    assert!(text.contains(" --system reload\n"), "{text}");
    assert!(text.contains("UMask=0007\n"), "{text}");
    assert!(text.contains("ReadWritePaths=\"-"), "{text}");
    // This test binary is the user's, not root's: said, and refused below.
    assert!(text.contains("# warning: the service would run"), "{text}");
    assert!(!text.contains("XDG_CONFIG_HOME"), "{text}");
    // The unit as printed passes systemd-analyze.
    let unit = m.root.join("print/lsync.service");
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    let body: String = text
        .split("# /etc/systemd/system/lsync.service.d/")
        .next()
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&unit, body).unwrap();
    verify(&unit);

    // --config with --system (finding 1 of the review): refused before
    // anything is read, so a config file's folder (/etc for
    // /etc/lsync.toml) is never handed to the service's user.
    let parent = m.root.join("etc");
    let before = std::fs::metadata(&parent).unwrap().permissions();
    for extra in [&[][..], &["--print"][..]] {
        let config = parent.join("lsync.toml");
        std::fs::copy(etc.join("lsync.toml"), &config).unwrap();
        let mut args = vec!["--system", "--config", config.to_str().unwrap(), "service", "install"];
        args.extend_from_slice(&["--run-as", me]);
        args.extend_from_slice(extra);
        let out = m.cmd(&args).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && err.contains("--config") && err.contains("not for the system service"),
            "{err}"
        );
        // LSYNC_CONFIG is --config too.
        let mut c = m.cmd(&["--system", "service", "install", "--run-as", me]);
        c.env("LSYNC_CONFIG", &config);
        let out = c.output().unwrap();
        assert!(String::from_utf8_lossy(&out.stderr).contains("not for the system service"));
    }
    assert_eq!(std::fs::metadata(&parent).unwrap().permissions(), before);
    assert!(m.calls().is_empty(), "{:?}", m.calls());

    if unsafe { libc::geteuid() } != 0 {
        let mut c = m.cmd(&["--system", "service", "install", "--run-as", me]);
        c.env("CONFIGURATION_DIRECTORY", &etc);
        let out = c.output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("sudo"));
        assert!(m.calls().is_empty(), "{:?}", m.calls());
    }
}

/// macOS: the LaunchAgent goes to `~/Library/LaunchAgents`, is loaded
/// into the user's GUI domain, carries the folders this CLI uses, and is
/// unloaded and removed again; the config stays.
#[cfg(target_os = "macos")]
#[test]
fn a_launch_agent_installs_into_the_home_and_uninstalls() {
    let m = Machine::new();
    let uid = unsafe { libc::getuid() };
    let out = m.ok(&["service", "install"]);
    assert!(out.contains("loaded no.nyta.lsync"), "{out}");
    let plist = std::fs::read_to_string(m.plist()).unwrap();
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_lsync")).unwrap();
    assert!(
        plist.contains(&format!(
            "<string>{}</string>\n\t\t<string>serve</string>",
            exe.display()
        )),
        "{plist}"
    );
    for k in ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "LSYNC_RUNTIME_DIR"] {
        assert!(plist.contains(&format!("<key>{k}</key>")), "{k}: {plist}");
    }
    assert!(m.root.join("home/Library/Logs/lsync").is_dir());
    // The plist as written is one launchd reads.
    let lint = Command::new("/usr/bin/plutil")
        .arg("-lint")
        .arg(m.plist())
        .output()
        .unwrap();
    assert!(lint.status.success(), "{}", String::from_utf8_lossy(&lint.stdout));
    let target = format!("gui/{uid}/no.nyta.lsync");
    assert_eq!(
        m.calls(),
        [
            format!("launchctl bootout {target}"),
            format!("launchctl enable {target}"),
            format!("launchctl bootstrap gui/{uid} {}", m.plist().display()),
        ]
    );

    let st: serde_json::Value = serde_json::from_str(&m.ok(&["service", "status", "--json"])).unwrap();
    assert_eq!(st["installed"], true);
    assert_eq!(st["manager"], "launchd");
    assert_eq!(st["loaded"], true);
    assert_eq!(st["active"], "running");
    assert_eq!(st["socket"]["answers"], false);

    let out = m.ok(&["service", "uninstall"]);
    assert!(out.contains("removed"), "{out}");
    assert!(!m.plist().exists());
    assert!(m.root.join("config/lsync/lsync.toml").exists());
    let out = m.cmd(&["service", "status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stdout).contains("not installed"));
}

/// macOS has no system service: `--system service install` says so, and
/// runs nothing.
#[cfg(target_os = "macos")]
#[test]
fn a_system_service_is_refused_on_macos() {
    let m = Machine::new();
    let out = m.cmd(&["--system", "service", "install", "--print"]).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("not supported on macOS"), "{err}");
    assert!(m.calls().is_empty(), "{:?}", m.calls());
}

#[test]
fn doctor_reports_the_password_the_folders_and_the_service() {
    let m = Machine::new();
    let doctor = |m: &Machine| {
        let out = m.cmd(&["doctor", "--json"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        (out.status.code(), v)
    };
    let find = |v: &serde_json::Value, area: &str| -> Vec<(String, String)> {
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["area"] == area)
            .map(|c| {
                (
                    c["level"].as_str().unwrap().to_string(),
                    c["message"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    // No password yet: an error, and the exit code says so.
    let (code, v) = doctor(&m);
    assert_eq!(code, Some(1), "{v:#}");
    let acct = find(&v, "account");
    assert!(
        acct[0].0 == "error" && acct[0].1.contains("lsync account passwd dev"),
        "{acct:?}"
    );
    // The folder does not exist yet: a warning only.
    assert_eq!(find(&v, "vault")[0].0, "warn", "{v:#}");
    assert!(find(&v, "config")[0].1.contains("1 account(s), 1 vault(s)"));
    // Without a session bus, the keyring is simply not available here.
    assert!(find(&v, "secrets").iter().any(|(_, m)| m.starts_with("keyring:")));

    let secrets = m.root.join("config/lsync/secrets.toml");
    std::fs::write(&secrets, "\"dev\" = \"pw\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::create_dir_all(m.root.join("home/Notes/a/b")).unwrap();
    let (code, v) = doctor(&m);
    assert_eq!(code, Some(0), "{v:#}");
    assert_eq!(find(&v, "account")[0].0, "ok");
    assert_eq!(find(&v, "vault")[0].0, "ok");
    if cfg!(target_os = "linux") {
        let ino = find(&v, "inotify");
        assert!(ino[0].1.starts_with("3 folder(s) to watch"), "{ino:?}");
    }
    let svc = find(&v, "service");
    assert!(svc[0].1.contains("not installed"), "{svc:?}");
}

#[test]
fn doctor_for_the_system_service_wants_the_folder_and_no_system_trash() {
    let m = Machine::new();
    let etc = m.root.join("etc/lsync");
    std::fs::create_dir_all(&etc).unwrap();
    let folder = m.root.join("srv/notes");
    std::fs::write(
        etc.join("lsync.toml"),
        format!(
            "[account.dev]\nurl = \"http://127.0.0.1:9\"\n\n[vault.notes]\naccount = \"dev\"\ndatabase = \"notes\"\nfolder = \"{}\"\ntrash = \"system\"\n",
            folder.display()
        ),
    )
    .unwrap();
    let mut c = m.cmd(&["--system", "doctor", "--json"]);
    c.env("CONFIGURATION_DIRECTORY", &etc);
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let vault: Vec<String> = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["area"] == "vault" && c["level"] == "error")
        .map(|c| c["message"].as_str().unwrap().to_string())
        .collect();
    assert!(vault.iter().any(|m| m.contains("cannot create it")), "{vault:?}");
    assert!(vault.iter().any(|m| m.contains("trash = \"system\"")), "{vault:?}");
}

/// Findings B and C of the review, through the binary: root's config lock
/// never follows a symlink planted at its name (a dangling one to a file
/// that does not exist yet, as /etc/nologin), and `--system` accounts take
/// only lsync's own credential names (never `mysql-root`). Both are
/// refused before the config changes.
#[test]
fn a_system_config_edit_never_follows_a_planted_lock_or_names_a_foreign_credential() {
    let m = Machine::new();
    let etc = m.root.join("etc/lsync");
    std::fs::create_dir_all(&etc).unwrap();
    std::fs::copy(m.root.join("config/lsync/lsync.toml"), etc.join("lsync.toml")).unwrap();
    let before = std::fs::read_to_string(etc.join("lsync.toml")).unwrap();
    let system = |args: &[&str]| {
        let mut a = vec!["--system"];
        a.extend_from_slice(args);
        let mut c = m.cmd(&a);
        c.env("CONFIGURATION_DIRECTORY", &etc)
            .env("STATE_DIRECTORY", m.root.join("var/lib/lsync"));
        let out = c.output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let nologin = m.root.join("nologin");
    std::os::unix::fs::symlink(&nologin, etc.join(".lsync.toml.lock")).unwrap();
    let folder = m.root.join("srv/more");
    let (ok, err) = system(&[
        "vault",
        "add",
        "more",
        "--db",
        "more",
        "--folder",
        folder.to_str().unwrap(),
    ]);
    assert!(!ok && err.contains("symbolic link"), "{err}");
    assert!(!nologin.exists(), "nothing created through the link");
    std::fs::remove_file(etc.join(".lsync.toml.lock")).unwrap();

    let (ok, err) = system(&[
        "account",
        "add",
        "db",
        "--url",
        "http://127.0.0.1:9",
        "--user",
        "u",
        "--secret",
        "credential:mysql-root",
        "--no-password",
    ]);
    assert!(!ok && err.contains("lsync-NAME"), "{err}");
    assert_eq!(std::fs::read_to_string(etc.join("lsync.toml")).unwrap(), before);
    assert!(m.calls().is_empty(), "{:?}", m.calls());
    // An lsync- name is taken (not root here: nothing is encrypted).
    let (ok, err) = system(&[
        "account",
        "add",
        "db",
        "--url",
        "http://127.0.0.1:9",
        "--user",
        "u",
        "--secret",
        "credential:lsync-db",
        "--no-password",
    ]);
    assert!(ok, "{err}");
}
