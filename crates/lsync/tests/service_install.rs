//! `lsync service install|status|uninstall` through the binary, into a
//! machine of temp folders. `PATH` holds only fake `systemctl` and
//! `loginctl` scripts that record their arguments, so no command ever
//! reaches this machine's service manager.

use std::path::{Path, PathBuf};
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
        for (prog, answer) in [("systemctl", "enabled"), ("loginctl", "no")] {
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

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn unit(&self) -> PathBuf {
        self.root.join("config/systemd/user/lsync.service")
    }
}

/// `systemd-analyze verify` on a unit file, when this machine has it.
fn verify(path: &Path) {
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

#[test]
fn a_user_service_installs_into_the_config_home_and_uninstalls() {
    let m = Machine::new();
    let out = m.ok(&["service", "install"]);
    assert!(out.contains("enabled and started lsync.service"), "{out}");
    assert!(out.contains("--linger"), "a hint for servers: {out}");
    let unit = std::fs::read_to_string(m.unit()).unwrap();
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_lsync")).unwrap();
    assert!(unit.contains(&format!("ExecStart={} serve\n", exe.display())), "{unit}");
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
    assert!(text.contains("ReadWritePaths=-"), "{text}");
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

    if unsafe { libc::geteuid() } != 0 {
        let mut c = m.cmd(&["--system", "service", "install", "--run-as", me]);
        c.env("CONFIGURATION_DIRECTORY", &etc);
        let out = c.output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("sudo"));
        assert!(m.calls().is_empty(), "{:?}", m.calls());
    }
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
