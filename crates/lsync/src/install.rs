//! `lsync service install|uninstall|status|logs` (spec D14): `lsync serve`
//! as a systemd user service (with `--linger` for a server without a
//! login), a systemd system service (`--system`), or a macOS LaunchAgent.
//!
//! Every path and every command goes through [`Ctx`], so the tests install
//! into temp folders with a recording [`Runner`] and never reach the
//! machine's service manager. The real context takes the user unit folder
//! from `$XDG_CONFIG_HOME` (as systemd does) and runs `systemctl`,
//! `loginctl`, `launchctl` and `journalctl` from `PATH`.

use crate::config::Setup;
use crate::unit::{self, Credential, Exec, SystemService, DROPIN_NAME, LAUNCHD_LABEL, UNIT_NAME};
use anyhow::{anyhow, bail, Context, Result};
use livesync_host::config::Config;
use livesync_host::secret::{SecretRef, StoreKind};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(clap::Subcommand, Debug)]
pub enum ServiceCmd {
    /// Install `lsync serve` as a service and start it: a systemd user
    /// service (Linux), a system service with --system, or a LaunchAgent
    /// (macOS). Run it again after an upgrade or a move of the binary
    Install {
        /// Let the user service run without a login (`loginctl
        /// enable-linger`): for a server, where a user service otherwise
        /// stops at logout
        #[arg(long)]
        linger: bool,
        /// Write and enable the service, but do not start or restart it
        #[arg(long)]
        no_start: bool,
        /// Print what would be written, and change nothing
        #[arg(long)]
        print: bool,
        /// With --system (required there): the user the service runs as, a
        /// user of its own or the one whose vaults these are. The config
        /// folder, which holds the passwords kept in secrets.toml, is
        /// handed to this user
        #[arg(long, value_name = "USER")]
        run_as: Option<String>,
        /// With --system: the service's group. Its members may use the
        /// control socket, and the files the service writes are
        /// group-writable (for vaults shared with agents)
        #[arg(long, value_name = "GROUP")]
        group: Option<String>,
        /// Replace a service file that lsync did not write
        #[arg(long)]
        force: bool,
    },
    /// Stop the service and remove what `service install` wrote (the
    /// config, the state and the vault folders stay)
    Uninstall {
        /// Remove a service file that lsync did not write
        #[arg(long)]
        force: bool,
    },
    /// Whether the service is installed, enabled and running, and whether
    /// its control socket answers
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Show the service's log (the journal, or ~/Library/Logs/lsync on macOS)
    Logs {
        /// Keep following it
        #[arg(long, short = 'f')]
        follow: bool,
        /// How many lines (default 100)
        #[arg(long, short = 'n')]
        lines: Option<usize>,
    },
}

/// The outcome of a command `Ctx` ran.
#[derive(Debug, Clone, Default)]
pub struct Ran {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs the service manager's commands.
pub trait Runner {
    /// Run `prog args`, `stdin` fed to it; its output captured.
    fn run(&self, prog: &str, args: &[&str], stdin: Option<&[u8]>) -> std::io::Result<Ran>;
    /// Run `prog args` with this terminal (`logs`); its exit code.
    fn interactive(&self, prog: &str, args: &[&str]) -> std::io::Result<i32>;
}

/// The programs on `PATH`.
pub struct System;

impl Runner for System {
    fn run(&self, prog: &str, args: &[&str], stdin: Option<&[u8]>) -> std::io::Result<Ran> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut c = Command::new(prog);
        c.args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn()?;
        if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
            si.write_all(data)?;
        }
        let out = child.wait_with_output()?;
        Ok(Ran {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn interactive(&self, prog: &str, args: &[&str]) -> std::io::Result<i32> {
        Ok(std::process::Command::new(prog)
            .args(args)
            .status()?
            .code()
            .unwrap_or(1))
    }
}

/// Which service manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    SystemdUser,
    SystemdSystem,
    Launchd,
}

/// Everything install and uninstall touch.
pub struct Ctx<'a> {
    pub manager: Manager,
    pub setup: &'a Setup,
    pub runner: &'a dyn Runner,
    /// Where the unit (or the plist) goes.
    pub unit_dir: PathBuf,
    /// macOS: `~/Library/Logs/lsync`.
    pub log_dir: PathBuf,
    /// The system's `/etc`, where `credstore` and `credstore.encrypted` are.
    pub etc: PathBuf,
    pub euid: u32,
    /// This user's name and uid.
    pub user: String,
    pub uid: u32,
    /// The binary the service runs.
    pub exe: PathBuf,
    /// The environment the user service needs to find what this CLI finds.
    pub env: Vec<(String, String)>,
    /// Whether `status` asks the control socket (not in the unit tests).
    pub ask_socket: bool,
}

impl Ctx<'_> {
    fn unit_file(&self) -> PathBuf {
        match self.manager {
            Manager::Launchd => self.unit_dir.join(format!("{LAUNCHD_LABEL}.plist")),
            _ => self.unit_dir.join(UNIT_NAME),
        }
    }

    fn dropin_file(&self) -> PathBuf {
        self.unit_dir.join(format!("{UNIT_NAME}.d")).join(DROPIN_NAME)
    }

    fn log_file(&self) -> PathBuf {
        self.log_dir.join("lsync.log")
    }

    fn systemctl(&self, args: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = vec![];
        if self.manager == Manager::SystemdUser {
            v.push("--user".into());
        }
        v.extend(args.iter().map(|s| s.to_string()));
        v
    }

    /// `systemctl [--user] ARGS`, failing with its message.
    fn must(&self, prog: &str, args: &[String]) -> Result<Ran> {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let r = self
            .runner
            .run(prog, &a, None)
            .with_context(|| format!("running {prog}"))?;
        if !r.ok {
            bail!("`{prog} {}` failed: {}", a.join(" "), r.stderr.trim());
        }
        Ok(r)
    }

    fn may(&self, prog: &str, args: &[String]) -> Option<Ran> {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run(prog, &a, None).ok()
    }

    fn gui_domain(&self) -> String {
        format!("gui/{}", self.uid)
    }
}

/// The real context of this process.
pub fn real_ctx<'a>(setup: &'a Setup, runner: &'a dyn Runner) -> Result<Ctx<'a>> {
    let macos = cfg!(target_os = "macos");
    let manager = match (macos, setup.system) {
        (true, true) => bail!(
            "a system service is not supported on macOS; `lsync service install` installs a LaunchAgent for this user"
        ),
        (true, false) => Manager::Launchd,
        (false, true) => Manager::SystemdSystem,
        (false, false) => Manager::SystemdUser,
    };
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let home = var("HOME").map(PathBuf::from);
    let need_home = || home.clone().ok_or_else(|| anyhow!("HOME is not set"));
    let (unit_dir, log_dir) = match manager {
        Manager::SystemdSystem => (PathBuf::from("/etc/systemd/system"), PathBuf::new()),
        Manager::SystemdUser => {
            let config = match var("XDG_CONFIG_HOME") {
                Some(c) => PathBuf::from(c),
                None => need_home()?.join(".config"),
            };
            (config.join("systemd/user"), PathBuf::new())
        }
        Manager::Launchd => {
            let h = need_home()?;
            (h.join("Library/LaunchAgents"), h.join("Library/Logs/lsync"))
        }
    };
    let uid = livesync_host::control::current_uid();
    let exe = std::env::current_exe().context("finding this lsync binary")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let env = if setup.system {
        vec![]
    } else {
        carried_env(home.as_deref(), &|k| var(k))
    };
    Ok(Ctx {
        manager,
        setup,
        runner,
        unit_dir,
        log_dir,
        etc: PathBuf::from("/etc"),
        euid: geteuid(),
        user: user_name(uid).unwrap_or_else(|| uid.to_string()),
        uid,
        exe,
        env,
        ask_socket: true,
    })
}

/// What a user service must see to agree with this CLI: config and state
/// folders other than the defaults, and the control socket's folder.
pub fn carried_env(home: Option<&Path>, var: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let mut out = vec![];
    let abs = |v: String| std::path::absolute(&v).map(|p| p.display().to_string()).unwrap_or(v);
    for (k, default) in [("XDG_CONFIG_HOME", ".config"), ("XDG_STATE_HOME", ".local/state")] {
        if let Some(v) = var(k) {
            let v = abs(v);
            if home.is_none_or(|h| h.join(default) != Path::new(&v)) {
                out.push((k.to_string(), v));
            }
        }
    }
    if let Some(v) = var("LSYNC_RUNTIME_DIR") {
        out.push(("LSYNC_RUNTIME_DIR".into(), abs(v)));
    }
    out
}

/// Where root writes the system service's encrypted credentials.
pub const CREDSTORE_ENCRYPTED: &str = "/etc/credstore.encrypted";

/// `systemd-creds`, with the secret on stdin or stdout, never on a command line.
pub struct SystemdCreds;

impl livesync_host::secret::CredentialTool for SystemdCreds {
    fn encrypt(&self, name: &str, value: &livesync_host::secret::Secret, out: &Path) -> Result<(), String> {
        let name_arg = format!("--name={name}");
        let out_arg = out.display().to_string();
        let r = System
            .run(
                "systemd-creds",
                &["encrypt", &name_arg, "-", &out_arg],
                Some(value.expose().as_bytes()),
            )
            .map_err(|e| format!("running systemd-creds: {e}"))?;
        if !r.ok {
            return Err(r.stderr.trim().to_string());
        }
        Ok(())
    }

    fn decrypt(&self, name: &str, path: &Path) -> Result<livesync_host::secret::Secret, String> {
        let name_arg = format!("--name={name}");
        let path_arg = path.display().to_string();
        let r = System
            .run("systemd-creds", &["decrypt", &name_arg, &path_arg, "-"], None)
            .map_err(|e| format!("running systemd-creds: {e}"))?;
        if !r.ok {
            return Err(r.stderr.trim().to_string());
        }
        Ok(livesync_host::secret::Secret::new(
            r.stdout.trim_end_matches(['\n', '\r']),
        ))
    }
}

pub fn geteuid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// A user from the password database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

fn passwd(name: Option<&str>, uid: u32) -> Option<User> {
    let cname = match name {
        Some(n) => Some(std::ffi::CString::new(n).ok()?),
        None => None,
    };
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: a zeroed passwd is a valid out-parameter, and `buf`
        // outlives every pointer the call leaves in it.
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            match &cname {
                Some(n) => libc::getpwnam_r(n.as_ptr(), &mut pw, buf.as_mut_ptr().cast(), buf.len(), &mut out),
                None => libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr().cast(), buf.len(), &mut out),
            }
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if rc != 0 || out.is_null() || pw.pw_name.is_null() {
            return None;
        }
        // SAFETY: the call succeeded: the strings are NUL-terminated in `buf`.
        let s = |p: *const libc::c_char| {
            if p.is_null() {
                String::new()
            } else {
                unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
            }
        };
        return Some(User {
            name: s(pw.pw_name),
            uid: pw.pw_uid,
            gid: pw.pw_gid,
            home: PathBuf::from(s(pw.pw_dir)),
        });
    }
}

pub fn user_named(name: &str) -> Option<User> {
    passwd(Some(name), 0)
}

pub fn user_name(uid: u32) -> Option<String> {
    passwd(None, uid).map(|u| u.name)
}

fn group(name: Option<&str>, gid: u32) -> Option<(String, u32)> {
    let cname = match name {
        Some(n) => Some(std::ffi::CString::new(n).ok()?),
        None => None,
    };
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: as in `passwd`.
        let mut gr: libc::group = unsafe { std::mem::zeroed() };
        let mut out: *mut libc::group = std::ptr::null_mut();
        let rc = unsafe {
            match &cname {
                Some(n) => libc::getgrnam_r(n.as_ptr(), &mut gr, buf.as_mut_ptr().cast(), buf.len(), &mut out),
                None => libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr().cast(), buf.len(), &mut out),
            }
        };
        if rc == libc::ERANGE && buf.len() < 1 << 22 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if rc != 0 || out.is_null() || gr.gr_name.is_null() {
            return None;
        }
        // SAFETY: as in `passwd`.
        let n = unsafe { std::ffi::CStr::from_ptr(gr.gr_name) }
            .to_string_lossy()
            .into_owned();
        return Some((n, gr.gr_gid));
    }
}

/// Write a service file: atomically, mode 0644, and only over one of ours
/// unless `force`.
fn write_service_file(path: &Path, text: &str, force: bool) -> Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(old) if old == text => return Ok(false),
        Ok(old) if !unit::is_ours(&old) && !force => bail!(
            "{} exists and was not written by lsync; move it away, or give --force to replace it",
            path.display()
        ),
        _ => {}
    }
    let dir = path.parent().expect("a service file is in a folder");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Remove a service file of ours; `false` when there was none.
fn remove_service_file(path: &Path, force: bool) -> Result<bool> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        Ok(old) if !unit::is_ours(&old) && !force => bail!(
            "{} was not written by lsync; remove it yourself, or give --force",
            path.display()
        ),
        Ok(_) => {
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
            Ok(true)
        }
    }
}

/// The config the service will run: it must be there (serve refuses to
/// start without it), and an lsync.toml.
fn service_config(ctx: &Ctx) -> Result<Config> {
    let s = ctx.setup;
    let add = if s.system {
        "sudo lsync --system account add ... and sudo lsync --system vault add ..."
    } else {
        "lsync account add ... and lsync vault add ..."
    };
    if !s.config_file.exists() {
        bail!(
            "there is no {} yet, so the service would have nothing to run; add an account and a vault first ({add})",
            s.config_file.display()
        );
    }
    if !s.is_accounts_file()? {
        bail!(
            "{} is a profile file; the service runs an lsync.toml (add vaults with `lsync vault add`)",
            s.config_file.display()
        );
    }
    Ok(s.load_accounts()?.unwrap_or_default())
}

fn exec_for(ctx: &Ctx, socket_group: Option<&str>) -> Exec {
    let mut argv = vec![ctx.exe.display().to_string()];
    if ctx.setup.system {
        argv.push("--system".into());
    }
    // `lsync [--system] reload` finds the service's socket as the service
    // does (the unit's environment); it needs no config.
    let mut reload = argv.clone();
    reload.push("reload".into());
    if let Some(p) = &ctx.setup.explicit {
        argv.push("--config".into());
        argv.push(
            std::path::absolute(p)
                .unwrap_or_else(|_| p.clone())
                .display()
                .to_string(),
        );
    }
    argv.push("serve".into());
    if let Some(g) = socket_group {
        argv.push("--socket-group".into());
        argv.push(g.into());
    }
    Exec {
        argv,
        reload,
        // A system service has its own folders; never the installer's.
        env: if ctx.setup.system { vec![] } else { ctx.env.clone() },
    }
}

/// A folder the system service may write: a vault folder, or a vault's
/// state folder outside the service's own state folder.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Writable {
    pub path: PathBuf,
    /// A vault folder (shared with the group), not a state folder.
    pub vault: bool,
}

/// The folders the system service may write, for `home` as `~`.
pub fn writable_for(ctx: &Ctx, cfg: &Config, home: Option<&Path>) -> Vec<Writable> {
    let mut dirs = ctx.setup.dirs.clone();
    if let Some(h) = home {
        dirs.home = Some(h.to_path_buf());
    }
    let mut writable: Vec<Writable> = vec![];
    for (name, v) in &cfg.vault {
        writable.push(Writable {
            path: v.folder_path(&dirs),
            vault: true,
        });
        let state = v.state_path(name, &dirs);
        if !state.starts_with(&dirs.state) {
            writable.push(Writable {
                path: state,
                vault: false,
            });
        }
    }
    writable.sort();
    writable.dedup_by(|a, b| a.path == b.path);
    writable
}

/// What the system unit's drop-in lists for this config: the folders the
/// service may write (see [`writable_for`]), and its systemd credentials,
/// for `home` as `~`.
pub fn dropin_for(ctx: &Ctx, cfg: &Config, home: Option<&Path>) -> (String, Vec<String>) {
    let writable: Vec<PathBuf> = writable_for(ctx, cfg, home).into_iter().map(|w| w.path).collect();
    let mut creds = vec![];
    let mut missing = vec![];
    for (name, a) in &cfg.account {
        let Ok(r) = a.secret_ref(name) else { continue };
        if r.kind != StoreKind::Credential {
            continue;
        }
        match credential_file(&ctx.etc, &r) {
            Some(c) => creds.push(c),
            None => missing.push(format!(
                "account {name}: no credential {key:?} in {etc}/credstore.encrypted or {etc}/credstore; set it with `sudo lsync --system account passwd {name}` (or `systemd-creds encrypt --name={key} - {etc}/credstore.encrypted/{key}`), then run `sudo lsync service install --system` again",
                key = r.key,
                etc = ctx.etc.display()
            )),
        }
    }
    creds.sort_by(|a, b| a.name.cmp(&b.name));
    creds.dedup_by(|a, b| a.name == b.name);
    (unit::system_dropin(&writable, &creds), missing)
}

/// Where systemd finds a credential: encrypted first.
pub fn credential_file(etc: &Path, r: &SecretRef) -> Option<Credential> {
    for (dir, encrypted) in [("credstore.encrypted", true), ("credstore", false)] {
        let p = etc.join(dir).join(&r.key);
        if p.is_file() {
            return Some(Credential {
                name: r.key.clone(),
                path: p,
                encrypted,
            });
        }
    }
    None
}

/// The files of the system config folder that the service's user gets:
/// nothing else in it is ever touched.
pub const CONFIG_FILES: [&str; 3] = [
    livesync_host::config::CONFIG_FILE,
    livesync_host::config::SECRETS_FILE,
    ".lsync.toml.lock",
];

/// The system service's config folder (`ConfigurationDirectory=lsync`):
/// `/etc/lsync`, and nothing else. `--config`, or a `$CONFIGURATION_DIRECTORY`
/// that names another folder, is refused: the service would not read that
/// config, and install would hand that folder to the service's user.
fn system_config_dir(ctx: &Ctx, print: bool) -> Result<(PathBuf, Option<String>)> {
    let dir = ctx.etc.join("lsync");
    if let Some(p) = &ctx.setup.explicit {
        bail!(
            "--config ({}) is not for the system service: it reads {}, which `sudo lsync --system account add ...` and `vault add ...` write; drop --config (and LSYNC_CONFIG)",
            p.display(),
            dir.join(livesync_host::config::CONFIG_FILE).display()
        );
    }
    if ctx.euid != 0 && !print {
        bail!("a system service is installed as root: sudo lsync service install --system --run-as USER");
    }
    if ctx.setup.dirs.config != dir {
        let why = format!(
            "the system service reads its config from {}, but this lsync uses {} ($CONFIGURATION_DIRECTORY)",
            dir.display(),
            ctx.setup.dirs.config.display()
        );
        if !print {
            bail!("{why}; unset CONFIGURATION_DIRECTORY");
        }
        return Ok((dir, Some(why)));
    }
    Ok((dir, None))
}

/// Hand the system config folder (0700) and its known files ([`CONFIG_FILES`],
/// whoever owns them now) to the service's user, who must read them. Never
/// anything else: another file, a symlink or a hard link in the folder is
/// left as it is. The lines to print.
fn hand_over_config(dir: &Path, user: &User, gid: u32) -> Result<Vec<String>> {
    use std::os::unix::fs::PermissionsExt;
    let md = std::fs::symlink_metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
    if !md.is_dir() {
        bail!("{} is not a folder", dir.display());
    }
    // A folder in root's /etc, so its name cannot be swapped meanwhile.
    let d = std::fs::File::open(dir).with_context(|| format!("opening {}", dir.display()))?;
    // Taken back first (root's, 0700), so that a previous service user can
    // no longer change its entries: a file checked to have one link keeps
    // it (a planted hard link cannot be unlinked between the check and the
    // chown).
    std::os::unix::fs::fchown(&d, Some(geteuid()), None).with_context(|| format!("chown {}", dir.display()))?;
    d.set_permissions(std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {}", dir.display()))?;
    let mut out = vec![];
    for name in CONFIG_FILES {
        let handed = livesync_host::edit::chown_plain_file(dir, name, user.uid, gid)
            .with_context(|| format!("chown {}", dir.join(name).display()))?;
        if !handed && std::fs::symlink_metadata(dir.join(name)).is_ok() {
            out.push(format!(
                "warning: {} is not a plain file with one link; it was left alone, and the service may not read it",
                dir.join(name).display()
            ));
        }
    }
    std::os::unix::fs::fchown(&d, Some(user.uid), Some(gid)).with_context(|| format!("chown {}", dir.display()))?;
    Ok(out)
}

/// Create the folders the service may write that do not exist yet (it
/// cannot: `ProtectSystem=strict` leaves a missing path read-only), owned
/// by the service's user and group: vault folders 0770, state folders 0700.
/// Folders that exist are left as they are. The lines to print.
fn create_writable(writable: &[Writable], uid: u32, gid: u32) -> Vec<String> {
    use std::os::unix::fs::DirBuilderExt;
    let mut out = vec![];
    for w in writable {
        if std::fs::symlink_metadata(&w.path).is_ok() {
            continue;
        }
        // Each missing folder from the top, so the ones made here are known.
        let mut missing = vec![];
        let mut p = w.path.as_path();
        while std::fs::symlink_metadata(p).is_err() {
            missing.push(p.to_path_buf());
            match p.parent() {
                Some(q) => p = q,
                None => break,
            }
        }
        let mode = if w.vault { 0o770 } else { 0o700 };
        let made = missing.iter().rev().try_for_each(|m| {
            std::fs::DirBuilder::new().mode(mode).create(m)?;
            std::os::unix::fs::lchown(m, Some(uid), Some(gid))?;
            // The umask does not narrow it.
            std::fs::set_permissions(m, std::os::unix::fs::PermissionsExt::from_mode(mode))
        });
        match made {
            Ok(()) => out.push(format!("created {} for the service", w.path.display())),
            Err(e) => out.push(format!(
                "warning: could not create {} ({e}); the service cannot create it itself, so that vault will not start until it exists",
                w.path.display()
            )),
        }
    }
    out
}

/// Vaults with `trash = "system"`, which the system service cannot run:
/// the home trash is read-only to it, and a move across its bind mounts fails.
fn system_trash_vaults(cfg: &Config) -> Vec<String> {
    cfg.vault
        .iter()
        .filter(|(_, v)| v.enabled)
        .filter(|(_, v)| {
            v.what_syncs()
                .is_ok_and(|w| w.trash == livesync_engine::vaultfs::TrashMode::System)
        })
        .map(|(n, _)| n.clone())
        .collect()
}

pub fn system_trash_message(names: &[String]) -> String {
    format!(
        "vault(s) {} use trash = \"system\", which the system service cannot use (the home trash is read-only to it); set trash = \"vault\" (`sudo lsync --system vault edit NAME --trash vault`) or \"none\"",
        names.join(", ")
    )
}

/// Accounts whose password is in secrets.toml, a plain-text file in the
/// config folder that the service's user owns.
fn file_store_accounts(cfg: &Config) -> Vec<String> {
    cfg.account
        .iter()
        .filter(|(_, a)| a.username.as_deref().is_some_and(|u| !u.is_empty()))
        .filter(|(n, a)| a.secret_ref(n).is_ok_and(|r| r.kind == StoreKind::File))
        .map(|(n, _)| n.clone())
        .collect()
}

/// Why the system unit should not run `exe`: it, or a folder above it, can
/// be changed by a user other than root (who could then run code as the
/// service's user, at every start and reload). `None` when only root can.
pub fn exe_not_roots(exe: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let mut p = Some(exe);
    while let Some(here) = p {
        let md = match std::fs::symlink_metadata(here) {
            Ok(m) => m,
            Err(e) => return Some(format!("cannot be checked: {}: {e}", here.display())),
        };
        if md.uid() != 0 {
            return Some(format!("{} belongs to uid {}, not root", here.display(), md.uid()));
        }
        let sticky_dir = md.is_dir() && md.mode() & 0o1000 != 0;
        if md.mode() & 0o022 != 0 && !sticky_dir {
            return Some(format!(
                "{} can be written by its group or by everyone (mode {:o})",
                here.display(),
                md.mode() & 0o7777
            ));
        }
        p = here.parent();
    }
    None
}

pub fn run(setup: &Setup, cmd: ServiceCmd) -> Result<u8> {
    let runner = System;
    let ctx = real_ctx(setup, &runner)?;
    match cmd {
        ServiceCmd::Install {
            linger,
            no_start,
            print,
            run_as,
            group,
            force,
        } => {
            let opts = InstallOpts {
                linger,
                no_start,
                print,
                run_as,
                group,
                force,
            };
            for line in install(&ctx, &opts)? {
                println!("{line}");
            }
            Ok(0)
        }
        ServiceCmd::Uninstall { force } => {
            for line in uninstall(&ctx, force)? {
                println!("{line}");
            }
            Ok(0)
        }
        ServiceCmd::Status { json } => {
            let st = status(&ctx);
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                for line in status_lines(&st) {
                    println!("{line}");
                }
            }
            Ok(if st["installed"] == true { 0 } else { 3 })
        }
        ServiceCmd::Logs { follow, lines } => logs(&ctx, follow, lines.unwrap_or(100)),
    }
}

#[derive(Debug, Default, Clone)]
pub struct InstallOpts {
    pub linger: bool,
    pub no_start: bool,
    pub print: bool,
    pub run_as: Option<String>,
    pub group: Option<String>,
    pub force: bool,
}

/// Install (see [`ServiceCmd::Install`]); the lines to print.
pub fn install(ctx: &Ctx, o: &InstallOpts) -> Result<Vec<String>> {
    if ctx.manager != Manager::SystemdSystem && (o.run_as.is_some() || o.group.is_some()) {
        bail!("--run-as and --group are for the system service (`lsync service install --system`)");
    }
    if o.linger && ctx.manager != Manager::SystemdUser {
        bail!("--linger is for a systemd user service; a system service starts at boot anyway");
    }
    // Before the config is even read: --config is never the system's.
    let system_dir = match ctx.manager {
        Manager::SystemdSystem => Some(system_config_dir(ctx, o.print)?),
        _ => None,
    };
    let cfg = service_config(ctx)?;
    match ctx.manager {
        Manager::SystemdUser => install_user(ctx, o),
        Manager::SystemdSystem => install_system(ctx, o, &cfg, system_dir.expect("set above")),
        Manager::Launchd => install_launchd(ctx, o),
    }
}

/// Whether the user's service manager reads units from `dir` (`None`: it
/// could not be asked). It reads `$XDG_CONFIG_HOME/systemd/user` with *its*
/// `XDG_CONFIG_HOME`, which a shell's own (the testbed's) need not be.
fn manager_reads(ctx: &Ctx, dir: &Path) -> Option<(bool, Vec<String>)> {
    let r = ctx.may("systemctl", &ctx.systemctl(&["show", "--property=UnitPath", "--value"]))?;
    let paths: Vec<String> = r
        .stdout
        .split_whitespace()
        .filter(|p| p.starts_with('/'))
        .map(str::to_string)
        .collect();
    if !r.ok || paths.is_empty() {
        return None;
    }
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let want = canon(dir);
    let reads = paths.iter().any(|p| canon(Path::new(p)) == want);
    Some((reads, paths))
}

fn unread_unit_dir(dir: &Path, paths: &[String]) -> String {
    format!(
        "the user's service manager does not read units from {} (this shell's XDG_CONFIG_HOME is not the manager's); it reads {}. Install from a shell without that XDG_CONFIG_HOME (give --config, XDG_STATE_HOME or LSYNC_RUNTIME_DIR for other folders), or run `systemctl --user set-environment XDG_CONFIG_HOME=...` first",
        dir.display(),
        paths
            .iter()
            .filter(|p| p.contains("/.config/") || p.ends_with("/systemd/user"))
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn install_user(ctx: &Ctx, o: &InstallOpts) -> Result<Vec<String>> {
    let text = unit::user_unit(&exec_for(ctx, None));
    let path = ctx.unit_file();
    let unread = match manager_reads(ctx, &ctx.unit_dir) {
        Some((false, paths)) => Some(unread_unit_dir(&ctx.unit_dir, &paths)),
        _ => None,
    };
    if o.print {
        let mut out = vec![format!("# {}", path.display()), text];
        out.extend(unread.map(|u| format!("# warning: {u}")));
        return Ok(out);
    }
    if let Some(u) = unread {
        bail!("{u}");
    }
    let mut out = vec![];
    write_service_file(&path, &text, o.force)?;
    out.push(format!("wrote {}", path.display()));
    ctx.must("systemctl", &ctx.systemctl(&["daemon-reload"]))?;
    ctx.must("systemctl", &ctx.systemctl(&["enable", UNIT_NAME]))?;
    if o.no_start {
        out.push(format!(
            "enabled {UNIT_NAME}; it starts at the next login (or `systemctl --user start {UNIT_NAME}`)"
        ));
    } else {
        ctx.must("systemctl", &ctx.systemctl(&["restart", UNIT_NAME]))?;
        out.push(format!("enabled and started {UNIT_NAME}"));
    }
    if o.linger {
        ctx.must("loginctl", &["enable-linger".to_string(), ctx.user.clone()])?;
        out.push(format!(
            "lingering is on for {}: the service starts at boot and keeps running without a login",
            ctx.user
        ));
    } else if linger(ctx) == Some(false) {
        out.push(format!(
            "note: the service stops when {} logs out; on a server, run `lsync service install --linger`",
            ctx.user
        ));
    }
    out.push("logs: lsync service logs -f   status: lsync service status".into());
    Ok(out)
}

fn install_system(
    ctx: &Ctx,
    o: &InstallOpts,
    cfg: &Config,
    (config_dir, elsewhere): (PathBuf, Option<String>),
) -> Result<Vec<String>> {
    // Root, and no --config: checked in `system_config_dir`.
    // Never a default: the user who ran sudo would get the config folder,
    // and the passwords in it.
    let Some(name) = &o.run_as else {
        bail!("which user should the system service run as? Give --run-as USER (a user of its own, or the one whose vaults these are); it gets {} and the passwords kept there", config_dir.display());
    };
    let user = user_named(name).ok_or_else(|| anyhow!("no user named {name:?}"))?;
    let (group_name, gid) = match &o.group {
        Some(g) => group(Some(g), 0).ok_or_else(|| anyhow!("no group named {g:?}"))?,
        None => group(None, user.gid).unwrap_or_else(|| (user.gid.to_string(), user.gid)),
    };
    let trash = system_trash_vaults(cfg);
    if !trash.is_empty() {
        bail!("{}", system_trash_message(&trash));
    }
    let mut warnings: Vec<String> = elsewhere.into_iter().collect();
    if let Some(why) = exe_not_roots(&ctx.exe) {
        let what = format!(
            "the service would run {}, but {why}; install the binary where only root can change it (`sudo install -m755 {} /usr/local/bin/lsync`) and run that one",
            ctx.exe.display(),
            ctx.exe.display()
        );
        if !o.print {
            bail!("{what}");
        }
        warnings.push(what);
    }
    let files = file_store_accounts(cfg);
    if !files.is_empty() {
        warnings.push(format!(
            "account(s) {} keep the password in plain text in {} (the service's user owns it); systemd credentials keep it encrypted: set `secret = \"credential:lsync-NAME\"` for the account in {}, then `sudo lsync --system account passwd NAME`",
            files.join(", "),
            config_dir.join(livesync_host::config::SECRETS_FILE).display(),
            config_dir.join(livesync_host::config::CONFIG_FILE).display()
        ));
    }
    let svc = SystemService {
        exec: exec_for(ctx, o.group.as_deref()),
        user: user.name.clone(),
        group: group_name.clone(),
        socket_group: o.group.is_some(),
    };
    let text = unit::system_unit(&svc);
    let (dropin, missing) = dropin_for(ctx, cfg, Some(&user.home));
    warnings.extend(missing);
    let (path, dpath) = (ctx.unit_file(), ctx.dropin_file());
    let mut out = vec![];
    if o.print {
        out.push(format!("# {}", path.display()));
        out.push(text);
        out.push(format!("# {}", dpath.display()));
        out.push(dropin);
        out.extend(warnings.into_iter().map(|m| format!("# warning: {m}")));
        return Ok(out);
    }
    out.extend(hand_over_config(&config_dir, &user, gid)?);
    out.push(format!(
        "{} belongs to {} now (the service reads it)",
        config_dir.display(),
        user.name
    ));
    out.extend(create_writable(
        &writable_for(ctx, cfg, Some(&user.home)),
        user.uid,
        gid,
    ));
    write_service_file(&path, &text, o.force)?;
    write_service_file(&dpath, &dropin, o.force)?;
    out.push(format!("wrote {} and {}", path.display(), dpath.display()));
    for m in warnings {
        out.push(format!("warning: {m}"));
    }
    ctx.must("systemctl", &ctx.systemctl(&["daemon-reload"]))?;
    ctx.must("systemctl", &ctx.systemctl(&["enable", UNIT_NAME]))?;
    if o.no_start {
        out.push(format!(
            "enabled {UNIT_NAME}; it starts at the next boot (or `systemctl start {UNIT_NAME}`)"
        ));
    } else {
        ctx.must("systemctl", &ctx.systemctl(&["restart", UNIT_NAME]))?;
        out.push(format!(
            "enabled and started {UNIT_NAME}, as {}:{group_name}",
            user.name
        ));
    }
    if o.group.is_some() {
        out.push(format!(
            "members of {group_name} may use it: lsync --system status --all"
        ));
    }
    out.push("logs: lsync --system service logs -f   status: lsync --system service status".into());
    Ok(out)
}

fn install_launchd(ctx: &Ctx, o: &InstallOpts) -> Result<Vec<String>> {
    let text = unit::launchd_plist(&exec_for(ctx, None), &ctx.log_file());
    let path = ctx.unit_file();
    if o.print {
        return Ok(vec![format!("<!-- {} -->", path.display()), text]);
    }
    std::fs::create_dir_all(&ctx.log_dir).with_context(|| format!("creating {}", ctx.log_dir.display()))?;
    let mut out = vec![];
    write_service_file(&path, &text, o.force)?;
    out.push(format!("wrote {}", path.display()));
    let target = format!("{}/{LAUNCHD_LABEL}", ctx.gui_domain());
    // A loaded agent is unloaded first, so the new plist is read.
    let _ = ctx.may("launchctl", &["bootout".into(), target.clone()]);
    let _ = ctx.may("launchctl", &["enable".into(), target.clone()]);
    if o.no_start {
        out.push("it starts at the next login (or `launchctl bootstrap` it)".into());
    } else {
        ctx.must(
            "launchctl",
            &["bootstrap".into(), ctx.gui_domain(), path.display().to_string()],
        )?;
        out.push(format!("loaded {LAUNCHD_LABEL}; it starts at every login"));
    }
    out.push(format!("logs: {}", ctx.log_file().display()));
    Ok(out)
}

/// Uninstall; the lines to print.
pub fn uninstall(ctx: &Ctx, force: bool) -> Result<Vec<String>> {
    let mut out = vec![];
    match ctx.manager {
        Manager::SystemdUser | Manager::SystemdSystem => {
            if ctx.manager == Manager::SystemdSystem && ctx.euid != 0 {
                bail!("a system service is removed as root: sudo lsync service uninstall --system");
            }
            let path = ctx.unit_file();
            if let Ok(old) = std::fs::read_to_string(&path) {
                if !unit::is_ours(&old) && !force {
                    bail!(
                        "{} was not written by lsync; remove it yourself, or give --force",
                        path.display()
                    );
                }
            }
            // Stops it as well; a unit that is not loaded is no error here.
            if let Some(r) = ctx.may("systemctl", &ctx.systemctl(&["disable", "--now", UNIT_NAME])) {
                if r.ok {
                    out.push(format!("stopped and disabled {UNIT_NAME}"));
                }
            }
            let removed = remove_service_file(&path, force)?;
            if ctx.manager == Manager::SystemdSystem {
                let d = ctx.dropin_file();
                if remove_service_file(&d, force)? {
                    if let Some(parent) = d.parent() {
                        let _ = std::fs::remove_dir(parent);
                    }
                }
            }
            if removed {
                out.push(format!("removed {}", path.display()));
            } else {
                out.push(format!("{} was not installed", path.display()));
            }
            let _ = ctx.may("systemctl", &ctx.systemctl(&["daemon-reload"]));
            if ctx.manager == Manager::SystemdUser && linger(ctx) == Some(true) {
                out.push(format!(
                    "lingering stays on for {} (other user services may need it); `loginctl disable-linger {}` turns it off",
                    ctx.user, ctx.user
                ));
            }
        }
        Manager::Launchd => {
            let path = ctx.unit_file();
            if let Ok(old) = std::fs::read_to_string(&path) {
                if !unit::is_ours(&old) && !force {
                    bail!(
                        "{} was not written by lsync; remove it yourself, or give --force",
                        path.display()
                    );
                }
            }
            let target = format!("{}/{LAUNCHD_LABEL}", ctx.gui_domain());
            if ctx.may("launchctl", &["bootout".into(), target]).is_some_and(|r| r.ok) {
                out.push(format!("stopped {LAUNCHD_LABEL}"));
            }
            if remove_service_file(&path, force)? {
                out.push(format!("removed {}", path.display()));
            } else {
                out.push(format!("{} was not installed", path.display()));
            }
            out.push(format!("the logs stay in {}", ctx.log_dir.display()));
        }
    }
    out.push(format!(
        "the config ({}) and the vaults' state stay",
        ctx.setup.config_file.display()
    ));
    Ok(out)
}

/// Whether lingering is on for this user (`None`: unknown).
fn linger(ctx: &Ctx) -> Option<bool> {
    let r = ctx.may(
        "loginctl",
        &[
            "show-user".into(),
            ctx.user.clone(),
            "--property=Linger".into(),
            "--value".into(),
        ],
    )?;
    match r.stdout.trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

/// The service's state, as `service status --json` prints it.
pub fn status(ctx: &Ctx) -> Value {
    let path = ctx.unit_file();
    let text = std::fs::read_to_string(&path).ok();
    let mut v = json!({
        "manager": match ctx.manager {
            Manager::SystemdUser => "systemd-user",
            Manager::SystemdSystem => "systemd-system",
            Manager::Launchd => "launchd",
        },
        "unit_file": path.display().to_string(),
        "installed": text.is_some(),
        "ours": text.as_deref().is_some_and(unit::is_ours),
        "config": ctx.setup.config_file.display().to_string(),
    });
    match ctx.manager {
        Manager::SystemdUser | Manager::SystemdSystem => {
            let ask = |what: &str| {
                ctx.may("systemctl", &ctx.systemctl(&[what, UNIT_NAME]))
                    .map(|r| r.stdout.trim().to_string())
                    .filter(|s| !s.is_empty())
            };
            v["enabled"] = json!(ask("is-enabled"));
            v["active"] = json!(ask("is-active"));
            if ctx.manager == Manager::SystemdUser {
                v["linger"] = json!(linger(ctx));
            }
        }
        Manager::Launchd => {
            let target = format!("{}/{LAUNCHD_LABEL}", ctx.gui_domain());
            let r = ctx.may("launchctl", &["print".into(), target]);
            let loaded = r.as_ref().is_some_and(|r| r.ok);
            let state = r.as_ref().filter(|r| r.ok).and_then(|r| {
                r.stdout
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("state = ").map(str::to_string))
            });
            v["loaded"] = json!(loaded);
            v["active"] = json!(state);
            v["log"] = json!(ctx.log_file().display().to_string());
        }
    }
    if ctx.ask_socket {
        v["socket"] = socket_state();
    }
    v
}

/// Whether a service answers on the control socket this CLI would use.
pub fn socket_state() -> Value {
    use livesync_host::control::{client_sockets, Client};
    let looked: Vec<String> = client_sockets().iter().map(|p| p.display().to_string()).collect();
    match Client::connect_default() {
        Ok(Some(mut c)) => {
            c.set_timeout(Some(std::time::Duration::from_secs(5)));
            match c.hello(&[]) {
                Ok(h) => json!({"path": c.path().display().to_string(), "answers": true,
                                "version": h.version, "pid": h.pid, "config": h.config}),
                Err(e) => json!({"path": c.path().display().to_string(), "answers": false, "error": e.to_string()}),
            }
        }
        Ok(None) => json!({"answers": false, "looked_at": looked}),
        Err(e) => json!({"answers": false, "looked_at": looked, "error": e.to_string()}),
    }
}

pub fn status_lines(st: &Value) -> Vec<String> {
    let s = |k: &str| st[k].as_str().map(str::to_string);
    let mut out = vec![];
    if st["installed"] != true {
        out.push(format!(
            "not installed ({} does not exist); install it with `lsync service install`",
            s("unit_file").unwrap_or_default()
        ));
    } else {
        out.push(format!(
            "installed   {}{}",
            s("unit_file").unwrap_or_default(),
            if st["ours"] == true {
                ""
            } else {
                " (not written by lsync)"
            }
        ));
        if let Some(e) = s("enabled") {
            out.push(format!("enabled     {e}"));
        }
        if st.get("loaded").is_some() {
            out.push(format!(
                "loaded      {}",
                if st["loaded"] == true { "yes" } else { "no" }
            ));
        }
        out.push(format!(
            "state       {}",
            s("active").unwrap_or_else(|| "unknown".into())
        ));
        if let Some(l) = st.get("linger").and_then(|l| l.as_bool()) {
            out.push(format!(
                "linger      {}",
                if l {
                    "yes (runs without a login)"
                } else {
                    "no (stops at logout)"
                }
            ));
        }
    }
    let sock = &st["socket"];
    if sock["answers"] == true {
        out.push(format!(
            "socket      {} (lsync {}, pid {}, {})",
            sock["path"].as_str().unwrap_or_default(),
            sock["version"].as_str().unwrap_or_default(),
            sock["pid"],
            sock["config"].as_str().unwrap_or_default()
        ));
    } else {
        out.push(format!(
            "socket      no service answers{}",
            sock["error"].as_str().map(|e| format!(" ({e})")).unwrap_or_default()
        ));
    }
    out
}

fn logs(ctx: &Ctx, follow: bool, lines: usize) -> Result<u8> {
    let n = lines.to_string();
    let code = match ctx.manager {
        Manager::SystemdUser | Manager::SystemdSystem => {
            let mut args: Vec<&str> = vec![];
            if ctx.manager == Manager::SystemdUser {
                args.push("--user");
            }
            args.extend(["--unit", UNIT_NAME, "--lines", &n]);
            if follow {
                args.push("--follow");
            } else {
                args.push("--no-pager");
            }
            ctx.runner.interactive("journalctl", &args)?
        }
        Manager::Launchd => {
            let f = ctx.log_file();
            if !f.exists() {
                bail!("no log yet at {}", f.display());
            }
            let fs = f.display().to_string();
            let mut args = vec!["-n", &n];
            if follow {
                args.push("-F");
            }
            args.push(&fs);
            ctx.runner.interactive("tail", &args)?
        }
    };
    Ok(u8::try_from(code).unwrap_or(1))
}

/// After `lsync --system vault|account ...` changed the config: rewrite the
/// system unit's drop-in when what it lists changed (only root can; a
/// drop-in lsync did not install is left alone). The lines to print.
pub fn refresh_system_dropin(setup: &Setup) -> Vec<String> {
    if !setup.system || cfg!(target_os = "macos") {
        return vec![];
    }
    let runner = System;
    let Ok(ctx) = real_ctx(setup, &runner) else {
        return vec![];
    };
    refresh_dropin(&ctx)
}

pub fn refresh_dropin(ctx: &Ctx) -> Vec<String> {
    let dpath = ctx.dropin_file();
    let Ok(old) = std::fs::read_to_string(&dpath) else {
        return vec![];
    };
    if !unit::is_ours(&old) {
        return vec![];
    }
    // Only the config the service reads lists its folders.
    let dir = ctx.etc.join("lsync");
    if ctx.setup.explicit.is_some() || ctx.setup.dirs.config != dir {
        return vec![format!(
            "note: the system service reads {}, not {}; its drop-in was left as it is",
            dir.join(livesync_host::config::CONFIG_FILE).display(),
            ctx.setup.config_file.display()
        )];
    }
    let Ok(Some(cfg)) = ctx.setup.load_accounts() else {
        return vec![];
    };
    // `~` is the service user's home, as at install; new folders are theirs.
    let unit_text = std::fs::read_to_string(ctx.unit_file()).unwrap_or_default();
    let setting = |k: &str| unit_text.lines().find_map(|l| l.strip_prefix(k).map(str::to_string));
    let user = setting("User=").and_then(|n| user_named(&n));
    let gid = setting("Group=")
        .and_then(|g| group(Some(&g), 0))
        .map(|(_, gid)| gid)
        .or(user.as_ref().map(|u| u.gid));
    let home = user.as_ref().map(|u| u.home.clone());
    let (text, missing) = dropin_for(ctx, &cfg, home.as_deref());
    let mut out: Vec<String> = missing.into_iter().map(|m| format!("warning: {m}")).collect();
    let trash = system_trash_vaults(&cfg);
    if !trash.is_empty() {
        out.push(format!("warning: {}", system_trash_message(&trash)));
    }
    if ctx.euid == 0 {
        if let (Some(u), Some(gid)) = (&user, gid) {
            out.extend(create_writable(&writable_for(ctx, &cfg, home.as_deref()), u.uid, gid));
        }
    }
    if text == old {
        return out;
    }
    if ctx.euid != 0 {
        out.push(format!(
            "the system service's folders or credentials changed; run `sudo lsync service install --system --run-as USER` to update {}",
            dpath.display()
        ));
        return out;
    }
    match write_service_file(&dpath, &text, false) {
        Ok(_) => {
            let _ = ctx.may("systemctl", &ctx.systemctl(&["daemon-reload"]));
            out.push(format!(
                "updated {}; restart the service for it to apply: sudo systemctl restart {UNIT_NAME}",
                dpath.display()
            ));
        }
        Err(e) => out.push(format!("warning: {e:#}")),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use livesync_host::config::Dirs;
    use std::cell::RefCell;

    /// Records every command; answers from a script.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        /// (command line prefix, stdout, ok)
        answers: Vec<(String, String, bool)>,
    }

    impl Fake {
        /// The commands that change something (not `show` queries).
        fn changes(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .filter(|c| !c.contains(" show"))
                .cloned()
                .collect()
        }
    }

    impl Runner for Fake {
        fn run(&self, prog: &str, args: &[&str], _stdin: Option<&[u8]>) -> std::io::Result<Ran> {
            let line = format!("{prog} {}", args.join(" "));
            self.calls.borrow_mut().push(line.clone());
            let a = self.answers.iter().find(|(p, _, _)| line.starts_with(p.as_str()));
            Ok(Ran {
                ok: a.is_none_or(|a| a.2),
                stdout: a.map(|a| a.1.clone()).unwrap_or_default(),
                stderr: String::new(),
            })
        }
        fn interactive(&self, prog: &str, args: &[&str]) -> std::io::Result<i32> {
            self.calls.borrow_mut().push(format!("{prog} {}", args.join(" ")));
            Ok(0)
        }
    }

    struct Home {
        _d: tempfile::TempDir,
        root: PathBuf,
        setup: Setup,
    }

    /// A machine in a temp folder: the system service's config in
    /// `etc/lsync`, a user's in `config/lsync`; every vault and state folder
    /// under `srv/`, so nothing outside the temp folder is ever created.
    fn home(system: bool) -> Home {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_path_buf();
        let dirs = Dirs {
            home: Some(root.join("home")),
            config: root.join(if system { "etc/lsync" } else { "config/lsync" }),
            state: root.join("state/lsync"),
        };
        std::fs::create_dir_all(&dirs.config).unwrap();
        let srv = root.join("srv");
        std::fs::write(
            dirs.config.join("lsync.toml"),
            format!(
                "[account.home]\nurl = \"http://127.0.0.1:5984\"\nusername = \"u\"\nsecret = \"credential:lsync-home\"\n\n[vault.notes]\naccount = \"home\"\ndatabase = \"notes\"\nfolder = \"{srv}/Notes\"\n\n[vault.work]\naccount = \"home\"\ndatabase = \"work\"\nfolder = \"{srv}/work notes\"\nstate_dir = \"{srv}/state/work\"\n",
                srv = srv.display()
            ),
        )
        .unwrap();
        let setup = Setup {
            config_file: dirs.config_file(),
            dirs,
            explicit: None,
            system,
        };
        Home { _d: d, root, setup }
    }

    /// A binary only root can change, as the system unit wants.
    fn roots_exe() -> PathBuf {
        std::fs::canonicalize("/bin/sh").unwrap()
    }

    fn ctx<'a>(h: &'a Home, runner: &'a Fake, manager: Manager) -> Ctx<'a> {
        let me = livesync_host::control::current_uid();
        Ctx {
            manager,
            setup: &h.setup,
            runner,
            unit_dir: h.root.join("units"),
            log_dir: h.root.join("logs"),
            etc: h.root.join("etc"),
            euid: 0,
            user: user_name(me).unwrap(),
            uid: me,
            exe: if manager == Manager::SystemdSystem {
                roots_exe()
            } else {
                PathBuf::from("/opt/lsync/bin/lsync")
            },
            env: vec![("XDG_STATE_HOME".into(), "/data/state".into())],
            ask_socket: false,
        }
    }

    fn me() -> String {
        user_name(livesync_host::control::current_uid()).unwrap()
    }

    fn as_me() -> InstallOpts {
        InstallOpts {
            run_as: Some(me()),
            ..Default::default()
        }
    }

    /// A group of this user's other than its primary one, when it has
    /// one: the group a chown without root can still change files to.
    fn another_group() -> Option<(String, u32)> {
        // SAFETY: getgroups with a buffer of the size it reports.
        let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        let mut gs = vec![0 as libc::gid_t; n.max(0) as usize];
        let n = unsafe { libc::getgroups(gs.len() as i32, gs.as_mut_ptr()) };
        gs.truncate(n.max(0) as usize);
        let primary = unsafe { libc::getegid() };
        gs.into_iter().filter(|g| *g != primary).find_map(|g| group(None, g))
    }

    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    fn gid_of(p: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(p).unwrap().gid()
    }

    #[test]
    fn a_user_install_writes_the_unit_enables_and_starts_it() {
        let h = home(false);
        let f = Fake {
            answers: vec![("loginctl show-user".into(), "no\n".into(), true)],
            ..Default::default()
        };
        let c = ctx(&h, &f, Manager::SystemdUser);
        let out = install(&c, &InstallOpts::default()).unwrap().join("\n");
        let unit = std::fs::read_to_string(h.root.join("units/lsync.service")).unwrap();
        assert!(unit.contains("ExecStart=/opt/lsync/bin/lsync serve\n"), "{unit}");
        assert!(unit.contains("Environment=\"XDG_STATE_HOME=/data/state\"\n"));
        assert_eq!(
            f.changes()
                .into_iter()
                .filter(|c| c.starts_with("systemctl"))
                .collect::<Vec<_>>(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable lsync.service",
                "systemctl --user restart lsync.service"
            ]
        );
        assert!(out.contains("stops when") && out.contains("--linger"), "{out}");

        // Again with --linger: the same unit, lingering turned on.
        f.calls.borrow_mut().clear();
        let o = InstallOpts {
            linger: true,
            ..Default::default()
        };
        install(&c, &o).unwrap();
        assert!(f.calls.borrow().contains(&format!("loginctl enable-linger {}", c.user)));

        // Uninstall: stopped, disabled and removed; the config stays.
        f.calls.borrow_mut().clear();
        let out = uninstall(&c, false).unwrap().join("\n");
        assert!(!h.root.join("units/lsync.service").exists());
        assert!(h.setup.config_file.exists());
        assert_eq!(f.calls.borrow()[0], "systemctl --user disable --now lsync.service");
        assert!(out.contains("removed"), "{out}");
        // Twice is fine.
        assert!(uninstall(&c, false).unwrap().join("\n").contains("was not installed"));
    }

    #[test]
    fn a_unit_lsync_did_not_write_is_left_alone() {
        let h = home(false);
        let f = Fake::default();
        let c = ctx(&h, &f, Manager::SystemdUser);
        std::fs::create_dir_all(h.root.join("units")).unwrap();
        std::fs::write(h.root.join("units/lsync.service"), "[Service]\nExecStart=/bin/true\n").unwrap();
        let e = install(&c, &InstallOpts::default()).unwrap_err().to_string();
        assert!(e.contains("not written by lsync") && e.contains("--force"), "{e}");
        assert!(uninstall(&c, false).is_err());
        assert!(f.changes().is_empty(), "{:?}", f.calls.borrow());
        install(
            &c,
            &InstallOpts {
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(unit::is_ours(
            &std::fs::read_to_string(h.root.join("units/lsync.service")).unwrap()
        ));
    }

    #[test]
    fn install_needs_a_config_and_print_changes_nothing() {
        let h = home(false);
        let f = Fake::default();
        let c = ctx(&h, &f, Manager::SystemdUser);
        let printed = install(
            &c,
            &InstallOpts {
                print: true,
                ..Default::default()
            },
        )
        .unwrap()
        .join("\n");
        assert!(printed.contains("Type=notify"));
        assert!(!h.root.join("units").exists());
        assert!(f.changes().is_empty());
        std::fs::remove_file(&h.setup.config_file).unwrap();
        let e = install(&c, &InstallOpts::default()).unwrap_err().to_string();
        assert!(e.contains("lsync vault add"), "{e}");
    }

    #[test]
    fn a_system_install_lists_the_vault_folders_and_credentials() {
        let h = home(true);
        std::fs::create_dir_all(h.root.join("etc/credstore.encrypted")).unwrap();
        std::fs::write(h.root.join("etc/credstore.encrypted/lsync-home"), "x").unwrap();
        let f = Fake::default();
        let c = ctx(&h, &f, Manager::SystemdSystem);
        let me = c.user.clone();
        let out = install(&c, &as_me()).unwrap().join("\n");
        let unit = std::fs::read_to_string(h.root.join("units/lsync.service")).unwrap();
        assert!(unit.contains(&format!("User={me}\n")), "{unit}");
        let exe = roots_exe().display().to_string();
        assert!(unit.contains(&format!("ExecStart={exe} --system serve\n")), "{unit}");
        assert!(unit.contains(&format!("ExecReload={exe} --system reload\n")), "{unit}");
        // The installer's environment is not the system service's.
        assert!(!unit.contains("XDG_STATE_HOME"), "{unit}");
        let dropin = std::fs::read_to_string(h.root.join("units/lsync.service.d/lsync-vaults.conf")).unwrap();
        let srv = h.root.join("srv").display().to_string();
        assert!(
            dropin.contains(&format!(
                "ReadWritePaths=\"-{srv}/Notes\" \"-{srv}/state/work\" \"-{srv}/work notes\"\n"
            )),
            "{dropin}"
        );
        assert!(dropin.contains(&format!(
            "LoadCredentialEncrypted=lsync-home:{}\n",
            h.root.join("etc/credstore.encrypted/lsync-home").display()
        )));
        assert!(out.contains("started"), "{out}");
        assert_eq!(
            f.calls.borrow().as_slice(),
            [
                "systemctl daemon-reload",
                "systemctl enable lsync.service",
                "systemctl restart lsync.service"
            ]
        );

        // A new vault: the drop-in follows.
        let mut text = std::fs::read_to_string(&h.setup.config_file).unwrap();
        text.push_str(&format!(
            "\n[vault.more]\naccount = \"home\"\ndatabase = \"more\"\nfolder = \"{srv}/more\"\n"
        ));
        std::fs::write(&h.setup.config_file, text).unwrap();
        let said = refresh_dropin(&c).join("\n");
        assert!(said.contains("restart"), "{said}");
        let dropin = std::fs::read_to_string(h.root.join("units/lsync.service.d/lsync-vaults.conf")).unwrap();
        assert!(dropin.contains(&format!("\"-{srv}/more\" ")), "{dropin}");
        // Root made the new vault's folder, as the service cannot.
        assert!(said.contains(&format!("created {srv}/more")), "{said}");
        assert_eq!(mode(&h.root.join("srv/more")), 0o770);
        assert!(refresh_dropin(&c).is_empty(), "nothing changed: nothing said");

        let out = uninstall(&c, false).unwrap().join("\n");
        assert!(!h.root.join("units/lsync.service").exists(), "{out}");
        assert!(!h.root.join("units/lsync.service.d").exists());
    }

    #[test]
    fn a_system_install_wants_root_and_a_user_and_warns_of_missing_credentials() {
        let h = home(true);
        let f = Fake::default();
        let mut c = ctx(&h, &f, Manager::SystemdSystem);
        c.euid = 1000;
        assert!(install(&c, &as_me()).unwrap_err().to_string().contains("sudo"));
        c.euid = 0;
        // No default user, not even the one who ran sudo.
        let e = install(&c, &InstallOpts::default()).unwrap_err().to_string();
        assert!(e.contains("--run-as") && e.contains("passwords"), "{e}");
        let o = InstallOpts {
            run_as: Some("no-such-user-here".into()),
            ..Default::default()
        };
        assert!(install(&c, &o).unwrap_err().to_string().contains("no user"));
        let o = InstallOpts { print: true, ..as_me() };
        let printed = install(&c, &o).unwrap().join("\n");
        assert!(
            printed.contains("warning: account home: no credential \"lsync-home\""),
            "{printed}"
        );
        assert!(!printed.contains("LoadCredential"), "{printed}");
        assert!(f.calls.borrow().is_empty());
        let o = InstallOpts {
            linger: true,
            ..as_me()
        };
        assert!(install(&c, &o).unwrap_err().to_string().contains("--linger"));
        assert!(!h.root.join("units").exists());
    }

    /// Finding 1 of the review: `install --system` hands a folder to the
    /// service's user, so it must only ever be the system config folder,
    /// and in it only the files lsync keeps there.
    #[test]
    fn a_system_install_refuses_config_and_touches_only_the_known_files() {
        use std::os::unix::fs::PermissionsExt;
        let h = home(true);
        let f = Fake::default();

        // `sudo lsync --system --config /etc/lsync.toml service install
        // --system`: refused before anything is read or changed, with
        // --print too. Setup::new makes the file's folder the config folder.
        let elsewhere = h.root.join("etc-like");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::copy(&h.setup.config_file, elsewhere.join("lsync.toml")).unwrap();
        std::fs::write(elsewhere.join("shadow"), "root's").unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
        let before: Vec<(u32, u32)> = ["", "lsync.toml", "shadow"]
            .iter()
            .map(|n| (mode(&elsewhere.join(n)), gid_of(&elsewhere.join(n))))
            .collect();
        let explicit = Setup {
            config_file: elsewhere.join("lsync.toml"),
            dirs: Dirs {
                config: elsewhere.clone(),
                ..h.setup.dirs.clone()
            },
            explicit: Some(elsewhere.join("lsync.toml")),
            system: true,
        };
        let mut c = ctx(&h, &f, Manager::SystemdSystem);
        c.setup = &explicit;
        for print in [false, true] {
            let o = InstallOpts {
                print,
                group: another_group().map(|g| g.0),
                ..as_me()
            };
            let e = install(&c, &o).unwrap_err().to_string();
            assert!(
                e.contains("--config") && e.contains("not for the system service"),
                "{e}"
            );
        }
        // A $CONFIGURATION_DIRECTORY elsewhere: refused too (printed with a warning).
        let env_dir = Setup {
            config_file: explicit.config_file.clone(),
            dirs: explicit.dirs.clone(),
            explicit: None,
            system: true,
        };
        c.setup = &env_dir;
        let e = install(&c, &as_me()).unwrap_err().to_string();
        assert!(e.contains("CONFIGURATION_DIRECTORY"), "{e}");
        let printed = install(&c, &InstallOpts { print: true, ..as_me() }).unwrap().join("\n");
        assert!(
            printed.contains("# warning: the system service reads its config from"),
            "{printed}"
        );
        let after: Vec<(u32, u32)> = ["", "lsync.toml", "shadow"]
            .iter()
            .map(|n| (mode(&elsewhere.join(n)), gid_of(&elsewhere.join(n))))
            .collect();
        assert_eq!(before, after, "nothing elsewhere changed");
        assert!(f.calls.borrow().is_empty() && !h.root.join("units").exists());

        // The system config folder: its known files are handed over, hard
        // links, symlinks and other files are not.
        let etc = h.setup.dirs.config.clone();
        let shadow = h.root.join("shadow");
        std::fs::write(&shadow, "root's").unwrap();
        std::fs::hard_link(&shadow, etc.join("secrets.toml")).unwrap();
        std::fs::write(etc.join(".lsync.toml.lock"), "").unwrap();
        std::fs::write(etc.join("stranger"), "").unwrap();
        std::os::unix::fs::symlink(&shadow, etc.join("link")).unwrap();
        std::fs::set_permissions(&etc, std::fs::Permissions::from_mode(0o755)).unwrap();
        let untouched = |p: &Path| (gid_of(p), mode(p));
        let shadow_was = untouched(&shadow);
        let stranger_was = untouched(&etc.join("stranger"));
        let link_was = untouched(&etc.join("link"));
        c.setup = &h.setup;
        let other = another_group();
        let o = InstallOpts {
            group: other.as_ref().map(|g| g.0.clone()),
            ..as_me()
        };
        let out = install(&c, &o).unwrap().join("\n");
        assert!(
            out.contains(&format!(
                "warning: {} is not a plain file",
                etc.join("secrets.toml").display()
            )),
            "{out}"
        );
        assert_eq!(mode(&etc), 0o700);
        assert_eq!(untouched(&shadow), shadow_was);
        assert_eq!(untouched(&etc.join("stranger")), stranger_was);
        assert_eq!(untouched(&etc.join("link")), link_was);
        if let Some((_, gid)) = &other {
            for n in ["", "lsync.toml", ".lsync.toml.lock"] {
                assert_eq!(gid_of(&etc.join(n)), *gid, "{n} handed over");
            }
            // Again with the user's own group (finding 5: whatever the
            // files' owner now, they follow the service's user).
            let primary = user_named(&me()).unwrap().gid;
            install(&c, &as_me()).unwrap();
            for n in ["", "lsync.toml", ".lsync.toml.lock"] {
                assert_eq!(gid_of(&etc.join(n)), primary, "{n} handed over again");
            }
            assert_eq!(untouched(&shadow), shadow_was);
        } else {
            eprintln!("this user has one group only: the owners were not compared");
        }
    }

    #[test]
    fn a_system_install_wants_a_binary_only_root_can_change() {
        let h = home(true);
        let f = Fake::default();
        let mut c = ctx(&h, &f, Manager::SystemdSystem);
        c.exe = std::env::current_exe().unwrap();
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let e = install(&c, &as_me()).unwrap_err().to_string();
        assert!(e.contains("not root") && e.contains("sudo install -m755"), "{e}");
        let printed = install(&c, &InstallOpts { print: true, ..as_me() }).unwrap().join("\n");
        assert!(printed.contains("# warning: the service would run"), "{printed}");
        assert!(exe_not_roots(Path::new("/no/such/lsync")).is_some());
        assert_eq!(exe_not_roots(&roots_exe()), None);
        let d = tempfile::tempdir().unwrap();
        assert!(exe_not_roots(d.path()).unwrap().contains("not root"));
        assert!(!h.root.join("units").exists());
    }

    #[test]
    fn a_system_install_refuses_the_system_trash_and_warns_of_plain_passwords() {
        let h = home(true);
        let f = Fake::default();
        let c = ctx(&h, &f, Manager::SystemdSystem);
        let text = std::fs::read_to_string(&h.setup.config_file).unwrap();
        std::fs::write(
            &h.setup.config_file,
            text.replace("database = \"work\"\n", "database = \"work\"\ntrash = \"system\"\n"),
        )
        .unwrap();
        let e = install(&c, &as_me()).unwrap_err().to_string();
        assert!(e.contains("work") && e.contains("trash = \"system\""), "{e}");
        assert!(!h.root.join("units").exists());

        // A password in secrets.toml: installed, with a warning.
        std::fs::write(
            &h.setup.config_file,
            text.replace("secret = \"credential:lsync-home\"\n", ""),
        )
        .unwrap();
        let out = install(&c, &as_me()).unwrap().join("\n");
        assert!(
            out.contains("warning: account(s) home keep the password in plain text"),
            "{out}"
        );
    }

    #[test]
    fn a_system_install_creates_the_missing_folders_for_the_service() {
        let h = home(true);
        let f = Fake::default();
        let c = ctx(&h, &f, Manager::SystemdSystem);
        // `work notes` exists, with a mode of its own; the others do not.
        let srv = h.root.join("srv");
        std::fs::create_dir_all(srv.join("work notes")).unwrap();
        std::fs::set_permissions(
            srv.join("work notes"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let out = install(&c, &as_me()).unwrap().join("\n");
        assert!(
            out.contains(&format!("created {}", srv.join("Notes").display())),
            "{out}"
        );
        assert_eq!(mode(&srv.join("Notes")), 0o770);
        assert_eq!(mode(&srv.join("state/work")), 0o700);
        assert_eq!(mode(&srv.join("work notes")), 0o755, "left as it was");
        assert!(
            !out.contains(&format!("created {}", srv.join("work notes").display())),
            "{out}"
        );
    }

    #[test]
    fn a_vault_home_is_the_service_users() {
        let h = home(true);
        let f = Fake::default();
        let c = ctx(&h, &f, Manager::SystemdSystem);
        let mut cfg = c.setup.load_accounts().unwrap().unwrap();
        cfg.vault.get_mut("notes").unwrap().folder = "~/Notes".into();
        let (dropin, _) = dropin_for(&c, &cfg, Some(Path::new("/var/lib/lsync-home")));
        assert!(dropin.contains("\"-/var/lib/lsync-home/Notes\""), "{dropin}");
    }

    #[test]
    fn a_user_unit_goes_only_where_the_user_manager_reads() {
        let h = home(false);
        let unit_dir = h.root.join("units");
        let f = Fake {
            answers: vec![(
                "systemctl --user show --property=UnitPath".into(),
                "/home/u/.config/systemd/user.control /home/u/.config/systemd/user /usr/lib/systemd/user\n".into(),
                true,
            )],
            ..Default::default()
        };
        let c = ctx(&h, &f, Manager::SystemdUser);
        let e = install(&c, &InstallOpts::default()).unwrap_err().to_string();
        assert!(
            e.contains(&format!("does not read units from {}", unit_dir.display()))
                && e.contains("/home/u/.config/systemd/user"),
            "{e}"
        );
        assert!(!unit_dir.exists());
        let printed = install(
            &c,
            &InstallOpts {
                print: true,
                ..Default::default()
            },
        )
        .unwrap()
        .join("\n");
        assert!(
            printed.contains("# warning: the user's service manager does not read"),
            "{printed}"
        );

        // Where it does read: installed.
        let f = Fake {
            answers: vec![(
                "systemctl --user show --property=UnitPath".into(),
                format!("/home/u/.config/systemd/user {}\n", unit_dir.display()),
                true,
            )],
            ..Default::default()
        };
        let c = ctx(&h, &f, Manager::SystemdUser);
        install(&c, &InstallOpts::default()).unwrap();
        assert!(unit_dir.join(UNIT_NAME).exists());
    }

    #[test]
    fn a_launch_agent_is_bootstrapped_into_the_gui_domain() {
        let h = home(false);
        let f = Fake::default();
        let c = ctx(&h, &f, Manager::Launchd);
        install(&c, &InstallOpts::default()).unwrap();
        let plist = h.root.join("units/no.nyta.lsync.plist");
        let text = std::fs::read_to_string(&plist).unwrap();
        assert!(text.contains("<string>/opt/lsync/bin/lsync</string>\n\t\t<string>serve</string>"));
        assert!(text.contains(&h.root.join("logs/lsync.log").display().to_string()));
        assert!(h.root.join("logs").is_dir());
        let gui = format!("gui/{}", c.uid);
        assert_eq!(
            f.calls.borrow().as_slice(),
            [
                format!("launchctl bootout {gui}/no.nyta.lsync"),
                format!("launchctl enable {gui}/no.nyta.lsync"),
                format!("launchctl bootstrap {gui} {}", plist.display()),
            ]
        );
        // Python's plistlib reads it as a plist, when there is a python3.
        if let Ok(out) = std::process::Command::new("python3")
            .args([
                "-I",
                "-c",
                "import plistlib,sys; d=plistlib.load(open(sys.argv[1],'rb')); assert d['KeepAlive']=={'SuccessfulExit': False}, d; print(d['Label'])",
            ])
            .arg(&plist)
            .output()
        {
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "no.nyta.lsync");
        }
        f.calls.borrow_mut().clear();
        uninstall(&c, false).unwrap();
        assert!(!plist.exists());
        assert_eq!(f.calls.borrow()[0], format!("launchctl bootout {gui}/no.nyta.lsync"));
    }

    #[test]
    fn status_reports_the_managers_answers() {
        let h = home(false);
        let f = Fake {
            answers: vec![
                ("systemctl --user is-enabled".into(), "enabled\n".into(), true),
                ("systemctl --user is-active".into(), "active\n".into(), true),
                ("loginctl show-user".into(), "yes\n".into(), true),
            ],
            ..Default::default()
        };
        let c = ctx(&h, &f, Manager::SystemdUser);
        assert_eq!(status(&c)["installed"], false);
        install(
            &c,
            &InstallOpts {
                no_start: true,
                ..Default::default()
            },
        )
        .unwrap();
        let st = status(&c);
        assert_eq!(st["installed"], true);
        assert_eq!(st["enabled"], "enabled");
        assert_eq!(st["active"], "active");
        assert_eq!(st["linger"], true);
        let lines = status_lines(&st).join("\n");
        assert!(lines.contains("linger      yes"), "{lines}");
    }

    #[test]
    fn the_user_service_carries_only_the_folders_that_differ() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(a, _)| *a == k).map(|(_, v)| v.to_string())
        };
        let home = Path::new("/home/u");
        assert!(carried_env(Some(home), &env(&[("XDG_CONFIG_HOME", "/home/u/.config")])).is_empty());
        assert_eq!(
            carried_env(
                Some(home),
                &env(&[("XDG_CONFIG_HOME", "/tb/a/config"), ("LSYNC_RUNTIME_DIR", "/tb/a/run")])
            ),
            [
                ("XDG_CONFIG_HOME".to_string(), "/tb/a/config".to_string()),
                ("LSYNC_RUNTIME_DIR".to_string(), "/tb/a/run".to_string())
            ]
        );
    }
}
