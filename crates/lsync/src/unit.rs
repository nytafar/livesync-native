//! The service definitions `lsync service install` writes (spec D14): a
//! systemd user unit, a systemd system unit with its drop-in, and a macOS
//! LaunchAgent. Pure text; [`crate::install`] puts them in place.

use std::path::{Path, PathBuf};

pub const UNIT_NAME: &str = "lsync.service";
/// The system unit's drop-in with what follows from `lsync.toml` (the
/// folders the service may write, its credentials); rewritten when that
/// changes.
pub const DROPIN_NAME: &str = "lsync-vaults.conf";
pub const LAUNCHD_LABEL: &str = "no.nyta.lsync";
/// The first line of every file lsync writes, so it only ever replaces or
/// removes its own.
pub const MARKER: &str = "Written by `lsync service install`";
const DOCS: &str = "https://github.com/nytafar/livesync-native";
const DESCRIPTION: &str = "lsync: folders in two-way sync with a LiveSync CouchDB";

/// Whether a file (its text) was written by lsync.
pub fn is_ours(text: &str) -> bool {
    text.lines().take(3).any(|l| l.contains(MARKER))
}

/// One argument of `ExecStart=`: quoted when it has to be, `%` and `$` kept
/// literal (systemd would expand them).
pub fn exec_arg(s: &str) -> String {
    let s = s.replace('%', "%%").replace('$', "$$");
    if !s.is_empty()
        && !s
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ';'))
    {
        return s;
    }
    quoted(&s)
}

fn quoted(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `Environment="KEY=value"` (`%` kept literal).
fn environment(k: &str, v: &str) -> String {
    format!("Environment={}", quoted(&format!("{k}={}", v.replace('%', "%%"))))
}

/// A path in a space-separated list (`ReadWritePaths=`): always quoted,
/// `-` first so a folder that does not exist is no start failure. systemd
/// splits such a list like a shell: an unquoted `'` or `"` in one path
/// drops that path and every later one, so a quote is never left to chance.
fn listed_path(p: &Path) -> String {
    quoted(&format!("-{}", p.display().to_string().replace('%', "%%")))
}

/// What the service runs, and the environment it needs to agree with the
/// CLI that installed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    /// The program and its arguments.
    pub argv: Vec<String>,
    /// `ExecReload=`: `lsync [--system] reload`, which returns once the
    /// service has reread its config (empty: none).
    pub reload: Vec<String>,
    pub env: Vec<(String, String)>,
}

fn command_line(argv: &[String]) -> String {
    argv.iter().map(|a| exec_arg(a)).collect::<Vec<_>>().join(" ")
}

fn service_common(exec: &Exec, out: &mut String) {
    out.push_str("Type=notify\nNotifyAccess=main\n");
    out.push_str(&format!("ExecStart={}\n", command_line(&exec.argv)));
    // Through the control socket, not a signal: `systemctl reload` returns
    // when the reload is done, and two reloads never overlap.
    if !exec.reload.is_empty() {
        out.push_str(&format!("ExecReload={}\n", command_line(&exec.reload)));
    }
    out.push_str("Restart=on-failure\nRestartSec=10\n");
    out.push_str("WatchdogSec=60\n");
    for (k, v) in &exec.env {
        out.push_str(&environment(k, v));
        out.push('\n');
    }
}

/// `~/.config/systemd/user/lsync.service`.
pub fn user_unit(exec: &Exec) -> String {
    let mut s = format!(
        "# {MARKER}; `lsync service install` rewrites it and `lsync service uninstall` removes it.\n\
         [Unit]\nDescription={DESCRIPTION}\nDocumentation={DOCS}\n\n[Service]\n"
    );
    service_common(exec, &mut s);
    s.push_str("\n[Install]\nWantedBy=default.target\n");
    s
}

/// The system unit's own settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemService {
    pub exec: Exec,
    pub user: String,
    pub group: String,
    /// The group may use the control socket.
    pub socket_group: bool,
}

/// `/etc/systemd/system/lsync.service`.
pub fn system_unit(u: &SystemService) -> String {
    let mut s = format!(
        "# {MARKER}; `lsync service install --system` rewrites it and `lsync service uninstall --system` removes it.\n\
         [Unit]\nDescription={DESCRIPTION}\nDocumentation={DOCS}\n\
         Wants=network-online.target\nAfter=network-online.target\n\n[Service]\n"
    );
    s.push_str(&format!("User={}\nGroup={}\n", u.user, u.group));
    service_common(&u.exec, &mut s);
    s.push_str(&format!(
        "# Files the service writes are group-writable, for vaults shared with a group,\n\
         # and not readable by other users.\n\
         UMask=0007\n\
         ConfigurationDirectory=lsync\n\
         StateDirectory=lsync\nStateDirectoryMode=0700\n\
         RuntimeDirectory=lsync\nRuntimeDirectoryMode={}\n\
         ProtectSystem=strict\nPrivateTmp=yes\nNoNewPrivileges=yes\n\
         # The vault folders (ReadWritePaths=) and the credentials are in lsync.service.d/{DROPIN_NAME}.\n",
        if u.socket_group { "0750" } else { "0700" }
    ));
    s.push_str("\n[Install]\nWantedBy=multi-user.target\n");
    s
}

/// A systemd credential the service loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub name: String,
    pub path: PathBuf,
    /// `LoadCredentialEncrypted=` (made with `systemd-creds encrypt`), else
    /// `LoadCredential=`.
    pub encrypted: bool,
}

/// `/etc/systemd/system/lsync.service.d/lsync-vaults.conf`.
pub fn system_dropin(writable: &[PathBuf], creds: &[Credential]) -> String {
    let mut s = format!(
        "# {MARKER}, and rewritten when `lsync --system vault ...` or `account ...` changes what it lists:\n\
         # the folders the service may write, and the credentials it loads. A change applies at the next restart.\n\
         [Service]\n"
    );
    if !writable.is_empty() {
        let list: Vec<String> = writable.iter().map(|p| listed_path(p)).collect();
        s.push_str(&format!("ReadWritePaths={}\n", list.join(" ")));
    }
    for c in creds {
        s.push_str(&format!(
            "{}={}:{}\n",
            if c.encrypted {
                "LoadCredentialEncrypted"
            } else {
                "LoadCredential"
            },
            c.name,
            c.path.display().to_string().replace('%', "%%")
        ));
    }
    s
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `~/Library/LaunchAgents/no.nyta.lsync.plist`. Not `ProcessType=Background`,
/// which throttles I/O and coalesces timers (spec D14).
pub fn launchd_plist(exec: &Exec, log_file: &Path) -> String {
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!-- {MARKER}; `lsync service install` rewrites it and `lsync service uninstall` removes it. -->\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key>\n\t<string>{LAUNCHD_LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n"
    );
    for a in &exec.argv {
        s.push_str(&format!("\t\t<string>{}</string>\n", xml(a)));
    }
    s.push_str("\t</array>\n");
    s.push_str(
        "\t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n\
         \t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n\
         \t<key>ProcessType</key>\n\t<string>Standard</string>\n",
    );
    let log = xml(&log_file.display().to_string());
    s.push_str(&format!(
        "\t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n"
    ));
    if !exec.env.is_empty() {
        s.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
        for (k, v) in &exec.env {
            s.push_str(&format!("\t\t<key>{}</key>\n\t\t<string>{}</string>\n", xml(k), xml(v)));
        }
        s.push_str("\t</dict>\n");
    }
    s.push_str("</dict>\n</plist>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(argv: &[&str]) -> Exec {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let mut reload: Vec<String> = argv.iter().take_while(|a| *a != "serve").cloned().collect();
        reload.push("reload".into());
        Exec {
            argv,
            reload,
            env: vec![],
        }
    }

    #[test]
    fn exec_arguments_are_quoted_and_escaped() {
        assert_eq!(exec_arg("/usr/bin/lsync"), "/usr/bin/lsync");
        assert_eq!(exec_arg("/opt/my tools/lsync"), "\"/opt/my tools/lsync\"");
        assert_eq!(exec_arg("100%"), "100%%");
        assert_eq!(exec_arg("$HOME"), "$$HOME");
        assert_eq!(exec_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(exec_arg(""), "\"\"");
    }

    #[test]
    fn the_user_unit_is_a_notify_service_with_a_watchdog() {
        let mut e = exec(&["/home/u/.local/bin/lsync", "serve"]);
        e.env.push(("XDG_STATE_HOME".into(), "/data/state 1".into()));
        let u = user_unit(&e);
        assert!(is_ours(&u));
        for line in [
            "Type=notify",
            "ExecStart=/home/u/.local/bin/lsync serve",
            "ExecReload=/home/u/.local/bin/lsync reload",
            "Restart=on-failure",
            "WatchdogSec=60",
            "Environment=\"XDG_STATE_HOME=/data/state 1\"",
            "WantedBy=default.target",
        ] {
            assert!(u.lines().any(|l| l == line), "{line} missing:\n{u}");
        }
        assert!(!u.contains("User="));
    }

    #[test]
    fn the_system_unit_runs_as_its_user_with_the_hardening() {
        let u = system_unit(&SystemService {
            exec: exec(&["/usr/local/bin/lsync", "--system", "serve", "--socket-group", "notes"]),
            user: "lsync".into(),
            group: "notes".into(),
            socket_group: true,
        });
        for line in [
            "User=lsync",
            "Group=notes",
            "ExecStart=/usr/local/bin/lsync --system serve --socket-group notes",
            "ExecReload=/usr/local/bin/lsync --system reload",
            "UMask=0007",
            "ConfigurationDirectory=lsync",
            "StateDirectory=lsync",
            "RuntimeDirectory=lsync",
            "RuntimeDirectoryMode=0750",
            "ProtectSystem=strict",
            "PrivateTmp=yes",
            "NoNewPrivileges=yes",
            "After=network-online.target",
            "WantedBy=multi-user.target",
        ] {
            assert!(u.lines().any(|l| l == line), "{line} missing:\n{u}");
        }
        let d = system_dropin(
            &[PathBuf::from("/srv/notes"), PathBuf::from("/srv/my notes")],
            &[
                Credential {
                    name: "lsync-home".into(),
                    path: "/etc/credstore.encrypted/lsync-home".into(),
                    encrypted: true,
                },
                Credential {
                    name: "lsync-work".into(),
                    path: "/etc/credstore/lsync-work".into(),
                    encrypted: false,
                },
            ],
        );
        assert!(is_ours(&d));
        assert!(d.contains("ReadWritePaths=\"-/srv/notes\" \"-/srv/my notes\"\n"), "{d}");
        assert!(d.contains("LoadCredentialEncrypted=lsync-home:/etc/credstore.encrypted/lsync-home\n"));
        assert!(d.contains("LoadCredential=lsync-work:/etc/credstore/lsync-work\n"));
        assert!(!system_dropin(&[], &[]).contains("ReadWritePaths"));
        // A quote or a backslash in one path never cuts the list short.
        let d = system_dropin(
            &[
                PathBuf::from("/srv/Lasse's notes"),
                PathBuf::from("/srv/a \"b\" c\\d"),
                PathBuf::from("/srv/100%"),
                PathBuf::from("/srv/later"),
            ],
            &[],
        );
        assert!(
            d.contains(
                "ReadWritePaths=\"-/srv/Lasse's notes\" \"-/srv/a \\\"b\\\" c\\\\d\" \"-/srv/100%%\" \"-/srv/later\"\n"
            ),
            "{d}"
        );
    }

    #[test]
    fn the_launch_agent_keeps_alive_on_failure_and_is_not_background() {
        let mut e = exec(&["/Users/u/bin/lsync", "serve", "--config", "/Users/u/a&b/lsync.toml"]);
        e.env.push(("LSYNC_RUNTIME_DIR".into(), "/tmp/x".into()));
        let p = launchd_plist(&e, Path::new("/Users/u/Library/Logs/lsync/lsync.log"));
        assert!(is_ours(&p));
        assert!(p.contains("<string>no.nyta.lsync</string>"));
        assert!(p.contains("<string>/Users/u/a&amp;b/lsync.toml</string>"));
        assert!(p.contains("<key>SuccessfulExit</key>\n\t\t<false/>"));
        assert!(p.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(p.contains("<string>Standard</string>"));
        assert!(!p.contains("Background"));
        assert!(p.contains("<key>LSYNC_RUNTIME_DIR</key>"));
        assert!(p.contains("/Users/u/Library/Logs/lsync/lsync.log"));
    }

    /// `systemd-analyze verify` reads the units as systemd would (offline:
    /// it loads them into a test manager, never into the running one).
    #[test]
    fn systemd_analyze_verifies_the_units() {
        let Ok(out) = std::process::Command::new("systemd-analyze").arg("--version").output() else {
            eprintln!("skipped: no systemd-analyze on this machine");
            return;
        };
        if !out.status.success() {
            eprintln!("skipped: systemd-analyze does not run here");
            return;
        }
        let exe = std::env::current_exe().unwrap().display().to_string();
        let d = tempfile::tempdir().unwrap();
        let me = crate::install::user_name(crate::install::geteuid()).unwrap_or_else(|| "root".into());
        let verify = |name: &str, text: &str, extra: &[&str]| {
            let dir = d.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(UNIT_NAME);
            std::fs::write(&p, text).unwrap();
            let mut c = std::process::Command::new("systemd-analyze");
            c.args(extra).arg("verify").arg(&p);
            let out = c.output().unwrap();
            let err = String::from_utf8_lossy(&out.stderr).into_owned();
            // Only what is about our unit counts (the host's own units may warn).
            let ours: Vec<&str> = err.lines().filter(|l| l.contains(UNIT_NAME)).collect();
            assert!(out.status.success() && ours.is_empty(), "{name}:\n{err}\n{text}");
        };
        let mut e = exec(&[&exe, "serve"]);
        e.env.push(("XDG_CONFIG_HOME".into(), "/tmp/x y".into()));
        verify("user", &user_unit(&e), &[]);
        let sys = system_unit(&SystemService {
            exec: exec(&[&exe, "--system", "serve"]),
            user: me.clone(),
            group: me,
            socket_group: false,
        });
        verify("system", &sys, &[]);
        let dir = d.path().join("system");
        std::fs::create_dir_all(dir.join("lsync.service.d")).unwrap();
        // Paths with a quote, a space or a backslash: systemd reads the whole
        // list (an unquoted `'` made it drop that path and the later ones).
        std::fs::write(
            dir.join("lsync.service.d").join(DROPIN_NAME),
            system_dropin(
                &[
                    PathBuf::from("/tmp"),
                    PathBuf::from("/srv/Lasse's notes"),
                    PathBuf::from("/srv/a \"b\" c\\d"),
                    PathBuf::from("/srv/later"),
                ],
                &[],
            ),
        )
        .unwrap();
        verify("system", &sys, &[]);
    }
}
