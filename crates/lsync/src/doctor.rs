//! `lsync doctor` (spec D13, D14): what a running service would need, in
//! one report: the config, the secret store of every account, each vault
//! folder's access (macOS privacy permissions included), the inotify watch
//! limit against the folders to watch, and the service itself. It reads and
//! changes nothing else.

use crate::config::Setup;
use crate::install;
use anyhow::Result;
use livesync_host::secret::{Availability, StoreKind};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub level: Level,
    pub area: String,
    pub message: String,
}

fn check(level: Level, area: &str, message: impl Into<String>) -> Check {
    Check {
        level,
        area: area.into(),
        message: message.into(),
    }
}

/// The folders an inotify watcher would watch under `root`: every folder
/// but dot-folders (never synced), counted up to `cap`.
pub fn count_watched_dirs(root: &Path, cap: u64) -> u64 {
    let mut n = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        n += 1;
        if n >= cap {
            return n;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && !e.file_name().to_string_lossy().starts_with('.') {
                stack.push(e.path());
            }
        }
    }
    n
}

/// Whether this process may read and write a vault folder. `system`: the
/// folder is the system service's, which cannot create a missing one.
pub fn folder_access(folder: &Path, system: bool) -> Check {
    let area = "vault";
    match std::fs::metadata(folder) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && system => {
            return check(
                Level::Error,
                area,
                format!(
                    "{} does not exist, and the system service cannot create it (ProtectSystem=strict leaves a missing folder read-only); root creates it for the service's user with `sudo lsync --system vault edit NAME --folder {}` or `sudo lsync service install --system --run-as USER`",
                    folder.display(),
                    folder.display()
                ),
            )
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return check(
                Level::Warn,
                area,
                format!("{} does not exist yet (the first sync creates it)", folder.display()),
            )
        }
        Err(e) => return access_error(folder, e),
        Ok(md) if !md.is_dir() => return check(Level::Error, area, format!("{} is not a folder", folder.display())),
        Ok(_) => {}
    }
    if let Err(e) = std::fs::read_dir(folder) {
        return access_error(folder, e);
    }
    let c = std::ffi::CString::new(folder.as_os_str().as_encoded_bytes()).unwrap_or_default();
    // SAFETY: c is a valid NUL-terminated path.
    if unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) } != 0 {
        return check(
            Level::Error,
            area,
            format!(
                "{} cannot be written by this user: {}",
                folder.display(),
                std::io::Error::last_os_error()
            ),
        );
    }
    check(Level::Ok, area, format!("{} can be read and written", folder.display()))
}

fn access_error(folder: &Path, e: std::io::Error) -> Check {
    let tcc = cfg!(target_os = "macos") && e.raw_os_error() == Some(libc::EPERM);
    let message = if tcc {
        format!(
            "{} cannot be read ({e}): macOS privacy protection. Grant Full Disk Access to {} (System Settings, Privacy & Security), or move the vault",
            folder.display(),
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "lsync".into())
        )
    } else {
        format!("{} cannot be read: {e}", folder.display())
    };
    check(Level::Error, "vault", message)
}

/// `/proc/sys/fs/inotify/max_user_watches` against the folders to watch.
pub fn inotify_check(watched: u64, limit: Option<u64>) -> Check {
    let area = "inotify";
    match limit {
        None => check(
            Level::Info,
            area,
            format!("{watched} folder(s) to watch; the watch limit could not be read"),
        ),
        Some(l) if watched >= l => check(
            Level::Error,
            area,
            format!(
                "{watched} folder(s) to watch, but fs.inotify.max_user_watches is {l}; raise it (e.g. `echo fs.inotify.max_user_watches=524288 | sudo tee /etc/sysctl.d/90-lsync.conf && sudo sysctl --system`)"
            ),
        ),
        // Other programs of this user (editors, Syncthing) watch too.
        Some(l) if watched * 2 > l => check(
            Level::Warn,
            area,
            format!(
                "{watched} folder(s) to watch, more than half of fs.inotify.max_user_watches ({l}); consider raising it"
            ),
        ),
        Some(l) => check(
            Level::Ok,
            area,
            format!("{watched} folder(s) to watch; fs.inotify.max_user_watches is {l}"),
        ),
    }
}

fn read_number(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn checks(ctx: &Setup) -> Vec<Check> {
    let mut out = vec![];
    let cfg = match ctx.load_accounts() {
        Ok(Some(c)) => {
            out.push(check(
                Level::Ok,
                "config",
                format!(
                    "{}: {} account(s), {} vault(s)",
                    ctx.config_file.display(),
                    c.account.len(),
                    c.vault.len()
                ),
            ));
            Some(c)
        }
        Ok(None) => {
            out.push(check(
                Level::Warn,
                "config",
                format!(
                    "no {} yet; add an account and a vault with `lsync account add` and `lsync vault add`",
                    ctx.config_file.display()
                ),
            ));
            None
        }
        Err(e) => {
            out.push(check(Level::Error, "config", format!("{e:#}")));
            None
        }
    };

    // The stores, and each account's password.
    let stores = ctx.secrets();
    for kind in [
        StoreKind::File,
        StoreKind::Keyring,
        StoreKind::Credential,
        StoreKind::Env,
    ] {
        let Ok(s) = stores.store(kind) else { continue };
        let (level, state) = match s.availability() {
            Availability::Ready => (Level::Ok, "ready".to_string()),
            Availability::Locked(why) => (Level::Warn, format!("locked: {why}")),
            Availability::Unavailable(why) => (Level::Info, format!("not available: {why}")),
        };
        out.push(check(
            level,
            "secrets",
            format!("{}: {} ({state})", kind.prefix(), s.location()),
        ));
    }
    if let Some(cfg) = &cfg {
        for (name, a) in &cfg.account {
            if a.username.as_deref().is_none_or(|u| u.is_empty()) {
                out.push(check(Level::Ok, "account", format!("{name}: no user, no password")));
                continue;
            }
            let r = match a.secret_ref(name) {
                Ok(r) => r,
                Err(e) => {
                    out.push(check(Level::Error, "account", format!("{name}: {e}")));
                    continue;
                }
            };
            let (level, state) = match stores.get(&r) {
                Ok(Some(_)) => (Level::Ok, "set".to_string()),
                Ok(None) => (
                    Level::Error,
                    format!("missing; set it with `lsync account passwd {name}`"),
                ),
                Err(e) if e.is_temporary() => (Level::Warn, e.to_string()),
                // Only inside its unit: the service has $CREDENTIALS_DIRECTORY.
                Err(e) if r.kind == StoreKind::Credential => (Level::Info, e.to_string()),
                Err(e) => (Level::Error, e.to_string()),
            };
            out.push(check(
                level,
                "account",
                format!("{name}: password in {} ({state})", stores.describe(&r)),
            ));
        }
    }

    // The vault folders, and what watching them takes.
    let mut watched = 0u64;
    if let Some(cfg) = &cfg {
        for (name, v) in &cfg.vault {
            if !v.enabled {
                out.push(check(Level::Info, "vault", format!("{name}: disabled")));
                continue;
            }
            let folder = v.folder_path(&ctx.dirs);
            let mut c = folder_access(&folder, ctx.system);
            c.message = format!("{name}: {}", c.message);
            out.push(c);
            let system_trash = v
                .what_syncs()
                .is_ok_and(|w| w.trash == livesync_engine::vaultfs::TrashMode::System);
            if ctx.system && system_trash {
                out.push(check(
                    Level::Error,
                    "vault",
                    format!("{name}: {}", install::system_trash_message(std::slice::from_ref(name))),
                ));
            }
            watched += count_watched_dirs(&folder, 10_000_000);
        }
    }
    if cfg!(any(target_os = "linux", target_os = "android")) && watched > 0 {
        out.push(inotify_check(
            watched,
            read_number("/proc/sys/fs/inotify/max_user_watches"),
        ));
    }

    // The service.
    let runner = install::System;
    match install::real_ctx(ctx, &runner) {
        Ok(ic) => {
            let st = install::status(&ic);
            let installed = st["installed"] == true;
            let answers = st["socket"]["answers"] == true;
            let state = st["active"].as_str().unwrap_or("unknown");
            let level = match (installed, answers) {
                (_, true) => Level::Ok,
                (true, false) => Level::Warn,
                (false, false) => Level::Info,
            };
            let what = if installed {
                format!("installed ({}), {state}", st["unit_file"].as_str().unwrap_or_default())
            } else {
                "not installed (`lsync service install`)".into()
            };
            let sock = if answers {
                format!(
                    "a service answers at {} (lsync {})",
                    st["socket"]["path"].as_str().unwrap_or_default(),
                    st["socket"]["version"].as_str().unwrap_or_default()
                )
            } else {
                "no service answers on the control socket".into()
            };
            out.push(check(level, "service", format!("{what}; {sock}")));
            // The config's group must be the service user's alone: it reads
            // secrets.toml.
            match install::config_group_check(&ic) {
                Some(Ok(m)) => out.push(check(Level::Ok, "service", m)),
                Some(Err(m)) => out.push(check(Level::Error, "service", m)),
                None => {}
            }
            if st["linger"] == false && installed {
                out.push(check(
                    Level::Info,
                    "service",
                    "lingering is off: the user service stops at logout (`lsync service install --linger` for a server)",
                ));
            }
        }
        Err(e) => out.push(check(Level::Info, "service", format!("{e:#}"))),
    }
    out
}

pub fn run(ctx: &Setup, json: bool) -> Result<u8> {
    let all = checks(ctx);
    let worst = all.iter().map(|c| c.level).max().unwrap_or(Level::Ok);
    if json {
        let v: Vec<Value> = all
            .iter()
            .map(|c| json!({"level": c.level.as_str(), "area": c.area, "message": c.message}))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"checks": v, "worst": worst.as_str()}))?
        );
    } else {
        for c in &all {
            println!("{:<5} {:<8} {}", c.level.as_str(), c.area, c.message);
        }
    }
    Ok(if worst == Level::Error { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_folders_are_not_watched() {
        let d = tempfile::tempdir().unwrap();
        for p in ["a/b", "a/c", ".obsidian/plugins", ".trash/x", "d"] {
            std::fs::create_dir_all(d.path().join(p)).unwrap();
        }
        std::fs::write(d.path().join("a/note.md"), "x").unwrap();
        // The root, a, a/b, a/c, d.
        assert_eq!(count_watched_dirs(d.path(), 1000), 5);
        assert_eq!(count_watched_dirs(d.path(), 3), 3);
    }

    #[test]
    fn the_watch_limit_is_judged_with_room_for_others() {
        assert_eq!(inotify_check(10, Some(8192)).level, Level::Ok);
        assert_eq!(inotify_check(5000, Some(8192)).level, Level::Warn);
        let e = inotify_check(9000, Some(8192));
        assert_eq!(e.level, Level::Error);
        assert!(e.message.contains("max_user_watches"));
        assert_eq!(inotify_check(10, None).level, Level::Info);
    }

    #[test]
    fn folder_access_says_what_is_wrong() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(folder_access(d.path(), false).level, Level::Ok);
        assert_eq!(folder_access(&d.path().join("missing"), false).level, Level::Warn);
        // The system service never creates it: an error, and what to do.
        let c = folder_access(&d.path().join("missing"), true);
        assert_eq!(c.level, Level::Error);
        assert!(c.message.contains("cannot create it"), "{}", c.message);
        let f = d.path().join("file");
        std::fs::write(&f, "x").unwrap();
        assert_eq!(folder_access(&f, false).level, Level::Error);
        // Root reads everything; the mode check means nothing then.
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let ro = d.path().join("ro");
            std::fs::create_dir(&ro).unwrap();
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
            let c = folder_access(&ro, false);
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(c.level, Level::Error, "{}", c.message);
            assert!(c.message.contains("cannot be written"));
        }
    }
}
