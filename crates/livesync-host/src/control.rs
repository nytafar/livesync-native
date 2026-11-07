//! The local control socket (spec D7): where it lives, how it is made safe,
//! the server in front of a [`Service`], and a small blocking client.
//!
//! **Where.** `$LSYNC_RUNTIME_DIR/control.sock` when that is set; else, for
//! a user service, `$XDG_RUNTIME_DIR/lsync/control.sock` on Linux and
//! `~/Library/Application Support/lsync/control.sock` on macOS; for a system
//! service, `$RUNTIME_DIRECTORY` (systemd's `RuntimeDirectory=`) or
//! `/run/lsync`. Never on the network.
//!
//! **Who may connect.** The directory is made 0700 (0750 with a group) and
//! must be a real directory owned by the service's user before anything is
//! bound in it, so nobody else can reach the socket or put one there. Then
//! every connection's peer is checked (`SO_PEERCRED` on Linux and Android,
//! `getpeereid` elsewhere): the service's own user, or for a system service
//! with `--socket-group`, the members of that group. Anyone else gets one
//! `forbidden` line and is disconnected before a request is read.
//!
//! **One service per directory.** The service holds an flock on
//! `control.lock` beside the socket for as long as it runs. A second
//! service finds it locked and refuses to start, naming the holder. A
//! socket left by a crashed service has no lock behind it, so it is
//! removed and bound again; with the directory private and the lock held,
//! no other process can take the path meanwhile.
//!
//! **The client checks the service too.** It uses a socket only in a
//! directory no one but its owner can write to, and only when the process
//! at the other end runs as that owner.

use crate::protocol::{
    code, event, method, CommandParams, CtlError, Event, Incoming, ReloadParams, Request, Response, VaultParam,
    MAX_LINE, PROTOCOL,
};
use crate::service::{Command, Service};
use livesync_engine::platform::{read_holder, Holder, LockError, LockFile};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

pub const SOCKET_NAME: &str = "control.sock";
pub const LOCK_NAME: &str = "control.lock";
/// The system service's directory without systemd's `RuntimeDirectory=`.
pub const SYSTEM_DIR: &str = "/run/lsync";
/// The longest socket path every platform takes (`sun_path` is 104 bytes
/// on macOS, 108 on Linux, with the terminating NUL).
const MAX_SOCKET_PATH: usize = 103;

/// Which service a socket is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    User,
    System,
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The directory a service of this kind binds its socket in.
pub fn service_dir(kind: Kind) -> Result<PathBuf, String> {
    if let Some(d) = env_dir("LSYNC_RUNTIME_DIR") {
        return Ok(d);
    }
    match kind {
        Kind::System => {
            if cfg!(target_os = "macos") {
                return Err("a system service is not supported on macOS yet; set LSYNC_RUNTIME_DIR".into());
            }
            // systemd may list several directories, separated by ':'.
            let from_systemd = std::env::var("RUNTIME_DIRECTORY")
                .ok()
                .and_then(|v| v.split(':').next().map(PathBuf::from))
                .filter(|p| !p.as_os_str().is_empty());
            Ok(from_systemd.unwrap_or_else(|| PathBuf::from(SYSTEM_DIR)))
        }
        Kind::User => user_dir().ok_or_else(|| {
            "neither XDG_RUNTIME_DIR nor LSYNC_RUNTIME_DIR is set, so there is no place for the control socket".into()
        }),
    }
}

fn user_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return env_dir("HOME").map(|h| h.join("Library/Application Support/lsync"));
    }
    env_dir("XDG_RUNTIME_DIR").map(|d| d.join("lsync"))
}

/// The sockets a client tries, in order: `$LSYNC_RUNTIME_DIR` alone when
/// it is set; else the user service's, then the system service's.
pub fn client_sockets() -> Vec<PathBuf> {
    if let Some(d) = env_dir("LSYNC_RUNTIME_DIR") {
        return vec![d.join(SOCKET_NAME)];
    }
    let mut out: Vec<PathBuf> = user_dir().into_iter().map(|d| d.join(SOCKET_NAME)).collect();
    if !cfg!(target_os = "macos") {
        out.push(Path::new(SYSTEM_DIR).join(SOCKET_NAME));
    }
    out
}

/// This process's real user ID.
pub fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// The user and group of the process at the other end of a Unix socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub uid: u32,
    pub gid: u32,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn peer_of(fd: RawFd) -> std::io::Result<Peer> {
    let mut cred: libc::ucred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for writes of the sizes given.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Peer {
        uid: cred.uid,
        gid: cred.gid,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn peer_of(fd: RawFd) -> std::io::Result<Peer> {
    let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
    // SAFETY: both out-pointers are valid for writes.
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Peer { uid, gid })
}

/// Who may use a service's socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// The service's own user.
    pub uid: u32,
    /// A group whose members may connect too (system service only).
    pub group: Option<u32>,
}

impl Policy {
    pub fn own_user() -> Policy {
        Policy {
            uid: current_uid(),
            group: None,
        }
    }

    /// Whether `peer` may connect. Membership of the group is read from the
    /// system's user and group databases at each connection.
    pub fn admits(&self, peer: Peer) -> bool {
        self.admits_with(peer, in_group)
    }

    fn admits_with(&self, peer: Peer, member: impl Fn(u32, u32) -> bool) -> bool {
        if peer.uid == self.uid {
            return true;
        }
        match self.group {
            Some(g) => peer.gid == g || member(peer.uid, g),
            None => false,
        }
    }
}

/// A user's name and primary group.
fn passwd_of(uid: u32) -> Option<(String, u32)> {
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: zeroed passwd is a valid out-parameter; `buf` outlives the call.
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr().cast(), buf.len(), &mut out) };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if rc != 0 || out.is_null() || pw.pw_name.is_null() {
            return None;
        }
        // SAFETY: getpwuid_r succeeded, so pw_name points into `buf`, NUL-terminated.
        let name = unsafe { std::ffi::CStr::from_ptr(pw.pw_name) }
            .to_string_lossy()
            .into_owned();
        return Some((name, pw.pw_gid));
    }
}

/// The members a group lists, with the group's ID, by name or by ID.
fn group_entry(name: Option<&str>, gid: u32) -> Option<(u32, Vec<String>)> {
    let cname = match name {
        Some(n) => Some(std::ffi::CString::new(n).ok()?),
        None => None,
    };
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: as in passwd_of.
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
        if rc != 0 || out.is_null() {
            return None;
        }
        let mut members = Vec::new();
        let mut p = gr.gr_mem;
        // SAFETY: gr_mem is a NULL-terminated array of C strings in `buf`.
        unsafe {
            while !p.is_null() && !(*p).is_null() {
                members.push(std::ffi::CStr::from_ptr(*p).to_string_lossy().into_owned());
                p = p.add(1);
            }
        }
        return Some((gr.gr_gid, members));
    }
}

/// A group's ID by name.
pub fn group_id(name: &str) -> Result<u32, String> {
    group_entry(Some(name), 0)
        .map(|(gid, _)| gid)
        .ok_or_else(|| format!("no group named {name:?}"))
}

fn in_group(uid: u32, gid: u32) -> bool {
    let Some((name, primary)) = passwd_of(uid) else {
        return false;
    };
    primary == gid || group_entry(None, gid).is_some_and(|(_, members)| members.contains(&name))
}

/// Why a socket could not be bound.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// Another service holds this directory.
    #[error("another lsync service is already running with its control socket at {path}{holder}; stop it first, or give this one another LSYNC_RUNTIME_DIR")]
    Running { path: PathBuf, holder: String },
    /// The directory is not safe to bind in.
    #[error("{0}")]
    Unsafe(String),
    #[error("{0}")]
    Io(String),
}

/// Make `dir` a private directory of this user: created 0700 when missing
/// (0750 and owned by `group` with a group), refused when it is a symlink,
/// not a directory, or someone else's, and its mode set when it is wider.
pub fn prepare_dir(dir: &Path, group: Option<u32>) -> Result<(), BindError> {
    let mode = if group.is_some() { 0o750 } else { 0o700 };
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| BindError::Io(format!("creating {}: {e}", parent.display())))?;
    }
    match std::fs::DirBuilder::new().mode(mode).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(BindError::Io(format!("creating {}: {e}", dir.display()))),
    }
    // Opened without following a link, and checked and changed through the
    // descriptor, so what is checked is what is changed.
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)
        .map_err(|e| {
            BindError::Unsafe(format!(
                "{} is not a directory of its own ({e}); remove it or set LSYNC_RUNTIME_DIR",
                dir.display()
            ))
        })?;
    let md = f
        .metadata()
        .map_err(|e| BindError::Io(format!("{}: {e}", dir.display())))?;
    let uid = current_uid();
    if md.uid() != uid {
        return Err(BindError::Unsafe(format!(
            "{} belongs to uid {}, not to this user (uid {uid}); the control socket needs a directory of its own",
            dir.display(),
            md.uid()
        )));
    }
    if let Some(g) = group {
        if md.gid() != g {
            std::os::unix::fs::fchown(&f, None, Some(g)).map_err(|e| {
                BindError::Unsafe(format!(
                    "cannot give {} to group {g} ({e}); the service's user must be a member",
                    dir.display()
                ))
            })?;
        }
    }
    if md.mode() & 0o7777 != mode {
        // Only a folder of the socket's own is tightened (as systemd
        // creates a runtime directory, 0755): never one that holds anything
        // else, such as a home folder named by LSYNC_RUNTIME_DIR by mistake.
        let foreign = std::fs::read_dir(dir)
            .map_err(|e| BindError::Io(format!("{}: {e}", dir.display())))?
            .filter_map(|e| e.ok())
            .any(|e| e.file_name() != SOCKET_NAME && e.file_name() != LOCK_NAME);
        if foreign {
            return Err(BindError::Unsafe(format!(
                "{} has mode {:o} and holds other files; the control socket needs a folder of its own, mode {mode:o} (set LSYNC_RUNTIME_DIR to an empty folder)",
                dir.display(),
                md.mode() & 0o7777
            )));
        }
        f.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| BindError::Io(format!("chmod {}: {e}", dir.display())))?;
    }
    let md = f
        .metadata()
        .map_err(|e| BindError::Io(format!("{}: {e}", dir.display())))?;
    if md.mode() & 0o7777 != mode || group.is_some_and(|g| md.gid() != g) {
        return Err(BindError::Unsafe(format!(
            "{} keeps mode {:o}; it must be {mode:o}",
            dir.display(),
            md.mode() & 0o7777
        )));
    }
    Ok(())
}

/// A bound control socket: the listener, and the lock that makes it ours.
pub struct Bound {
    listener: std::os::unix::net::UnixListener,
    path: PathBuf,
    /// The socket's inode, so only our own socket is removed at the end.
    ino: u64,
    _lock: LockFile,
}

impl Bound {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Bound {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.ino() == self.ino) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Bind the control socket in `dir` (see the module docs for the steps).
pub fn bind(dir: &Path, policy: &Policy) -> Result<Bound, BindError> {
    let path = dir.join(SOCKET_NAME);
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(BindError::Unsafe(format!(
            "the control socket's path {} is longer than {MAX_SOCKET_PATH} bytes; set LSYNC_RUNTIME_DIR to a shorter one",
            path.display()
        )));
    }
    prepare_dir(dir, policy.group)?;
    let lock_path = dir.join(LOCK_NAME);
    let lock = match LockFile::try_acquire(&lock_path) {
        Ok(l) => l,
        Err(LockError::Busy(p)) => {
            return Err(BindError::Running {
                path: path.clone(),
                holder: read_holder(&p).map(|h| format!(" ({h})")).unwrap_or_default(),
            })
        }
        Err(e) => return Err(BindError::Io(e.to_string())),
    };
    if let Err(e) = lock.record(&Holder::current(dir)) {
        debug!("cannot record the holder in {}: {e}", lock_path.display());
    }
    // With the lock held, whatever is at the path is a crashed service's.
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_dir() => {
            return Err(BindError::Unsafe(format!(
                "{} is a directory; remove it",
                path.display()
            )))
        }
        Ok(_) => {
            info!("removing a stale control socket at {}", path.display());
            std::fs::remove_file(&path).map_err(|e| BindError::Io(format!("removing {}: {e}", path.display())))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(BindError::Io(format!("{}: {e}", path.display()))),
    }
    let listener = std::os::unix::net::UnixListener::bind(&path)
        .map_err(|e| BindError::Io(format!("binding {}: {e}", path.display())))?;
    // Nothing is accepted before this, and only this user (or the group)
    // can reach the directory meanwhile.
    let mode = if policy.group.is_some() { 0o660 } else { 0o600 };
    if let Some(g) = policy.group {
        std::os::unix::fs::chown(&path, None, Some(g))
            .map_err(|e| BindError::Io(format!("chown {}: {e}", path.display())))?;
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| BindError::Io(format!("chmod {}: {e}", path.display())))?;
    let ino = std::fs::symlink_metadata(&path)
        .map_err(|e| BindError::Io(format!("{}: {e}", path.display())))?
        .ino();
    listener
        .set_nonblocking(true)
        .map_err(|e| BindError::Io(e.to_string()))?;
    Ok(Bound {
        listener,
        path,
        ino,
        _lock: lock,
    })
}

/// Serve the socket until `shutdown` turns true. Must run on the runtime.
pub async fn serve(bound: Bound, service: Arc<Service>, policy: Policy, mut shutdown: watch::Receiver<bool>) {
    let listener = match bound.listener.try_clone().and_then(tokio::net::UnixListener::from_std) {
        Ok(l) => l,
        Err(e) => {
            warn!("the control socket cannot be served: {e}");
            return;
        }
    };
    let policy = Arc::new(policy);
    info!("control socket at {}", bound.path().display());
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(connection(stream, service.clone(), policy.clone()));
                }
                Err(e) => {
                    warn!("control socket: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            },
            _ = shutdown.changed() => break,
        }
    }
    drop(listener);
    drop(bound);
}

fn line(v: &impl serde::Serialize) -> String {
    let mut s = serde_json::to_string(v).unwrap_or_else(|_| "{}".into());
    s.push('\n');
    s
}

async fn connection(stream: tokio::net::UnixStream, service: Arc<Service>, policy: Arc<Policy>) {
    let peer = peer_of(stream.as_raw_fd());
    let (read, mut write) = stream.into_split();
    match peer {
        Ok(p) if policy.admits(p) => {}
        other => {
            warn!("control socket: refused a connection from {other:?}");
            let refusal = Response::err(
                Value::Null,
                CtlError::new(code::FORBIDDEN, "this user may not use this lsync service"),
            );
            let _ = write.write_all(line(&refusal).as_bytes()).await;
            return;
        }
    }
    let (tx, mut rx) = mpsc::channel::<String>(256);
    let writer = tokio::spawn(async move {
        while let Some(l) = rx.recv().await {
            if write.write_all(l.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let mut reader = tokio::io::BufReader::new(read);
    let mut subscriptions = Vec::new();
    loop {
        let mut buf = Vec::new();
        match (&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buf.len() > MAX_LINE {
            let e = CtlError::new(
                code::BAD_REQUEST,
                format!("a request line is longer than {MAX_LINE} bytes"),
            );
            let _ = tx.send(line(&Response::err(Value::Null, e))).await;
            break;
        }
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(e) => {
                let id = serde_json::from_str::<Value>(text)
                    .ok()
                    .and_then(|v| v.get("id").cloned())
                    .unwrap_or(Value::Null);
                let e = CtlError::new(code::BAD_REQUEST, format!("not a request: {e}"));
                let _ = tx.send(line(&Response::err(id, e))).await;
                continue;
            }
        };
        if req.v != PROTOCOL {
            let e = CtlError::new(
                code::UNSUPPORTED_VERSION,
                format!("this service speaks protocol version {PROTOCOL}, not {}", req.v),
            );
            let _ = tx.send(line(&Response::err(req.id, e))).await;
            continue;
        }
        if req.method == method::EVENTS {
            match subscribe(&service, &req) {
                Ok((filter, rx_events)) => {
                    let _ = tx.send(line(&Response::ok(req.id, json!({"subscribed": true})))).await;
                    subscriptions.push(tokio::spawn(forward(rx_events, filter, tx.clone())));
                }
                Err(e) => {
                    let _ = tx.send(line(&Response::err(req.id, e))).await;
                }
            }
            continue;
        }
        // Each request on its own task: a long command does not hold up
        // the connection's other requests, and it runs to its end (the
        // vault is restored) even when the client goes away.
        let (svc, tx) = (service.clone(), tx.clone());
        tokio::spawn(async move {
            let id = req.id.clone();
            let resp = match handle(&svc, req).await {
                Ok(v) => Response::ok(id, v),
                Err(e) => Response::err(id, e),
            };
            let _ = tx.send(line(&resp)).await;
        });
    }
    for s in subscriptions {
        s.abort();
    }
    drop(tx);
    let _ = writer.await;
}

fn to_value(v: &impl serde::Serialize) -> Value {
    serde_json::to_value(v).unwrap_or_default()
}

fn params<T: serde::de::DeserializeOwned + Default>(p: &Value) -> Result<T, CtlError> {
    if p.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(p.clone()).map_err(|e| CtlError::new(code::BAD_REQUEST, format!("parameters: {e}")))
}

fn subscribe(service: &Service, req: &Request) -> Result<(Option<String>, broadcast::Receiver<Event>), CtlError> {
    let p: VaultParam = params(&req.params)?;
    if let Some(v) = &p.vault {
        if !service.names().contains(v) {
            return Err(CtlError::new(
                code::UNKNOWN_VAULT,
                format!("the service runs no vault named {v:?}"),
            ));
        }
    }
    Ok((p.vault, service.subscribe()))
}

async fn forward(mut rx: broadcast::Receiver<Event>, vault: Option<String>, tx: mpsc::Sender<String>) {
    loop {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(broadcast::error::RecvError::Lagged(n)) => Event {
                v: PROTOCOL,
                event: event::LAGGED.into(),
                vault: None,
                ts: crate::protocol::now_ms(),
                data: json!({"missed": n}),
            },
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if vault.is_some() && ev.vault.is_some() && ev.vault != vault {
            continue;
        }
        if tx.send(line(&ev)).await.is_err() {
            return;
        }
    }
}

async fn handle(svc: &Arc<Service>, req: Request) -> Result<Value, CtlError> {
    let raw = &req.params;
    let cmd = |c: Command| async move {
        let p: CommandParams = params(raw)?;
        if p.vault.is_empty() {
            return Err(CtlError::new(code::BAD_REQUEST, "parameters: vault is required"));
        }
        svc.command(c, p).await
    };
    match req.method.as_str() {
        method::HELLO => Ok(to_value(&svc.hello())),
        method::STATUS => {
            let p: VaultParam = params(&req.params)?;
            let s = svc.clone();
            let r = tokio::task::spawn_blocking(move || s.status(p.vault.as_deref()))
                .await
                .map_err(|e| CtlError::new(code::FAILED, e.to_string()))??;
            Ok(to_value(&r))
        }
        method::SYNC_NOW => {
            let p: VaultParam = params(&req.params)?;
            Ok(json!({"vaults": svc.sync_now(p.vault.as_deref())?}))
        }
        method::PAUSE => {
            let p: VaultParam = params(&req.params)?;
            Ok(json!({"vaults": svc.pause(p.vault.as_deref()).await?}))
        }
        method::RESUME => {
            let p: VaultParam = params(&req.params)?;
            Ok(json!({"vaults": svc.resume(p.vault.as_deref()).await?}))
        }
        method::RELOAD => {
            let p: ReloadParams = params(&req.params)?;
            if p.wait == Some(false) {
                let s = svc.clone();
                tokio::spawn(async move {
                    let _ = s.reload().await;
                });
                return Ok(json!({"scheduled": true}));
            }
            Ok(to_value(&svc.reload().await?))
        }
        method::SYNC => Ok(to_value(&cmd(Command::Sync).await?)),
        method::PULL => Ok(to_value(&cmd(Command::Pull).await?)),
        method::FETCH => Ok(to_value(&cmd(Command::Fetch).await?)),
        method::CONNECT => Ok(to_value(&cmd(Command::Connect).await?)),
        other => Err(CtlError::new(
            code::UNKNOWN_METHOD,
            format!(
                "unknown method {other:?} (this service has: {})",
                method::ALL.join(", ")
            ),
        )),
    }
}

/// A connection to a running service, for the CLI and other Rust clients.
/// Blocking.
pub struct Client {
    stream: std::os::unix::net::UnixStream,
    reader: BufReader<std::os::unix::net::UnixStream>,
    path: PathBuf,
    next_id: u64,
}

/// Why a client could not use a socket.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The socket is not one this user should trust.
    #[error("{0}")]
    Unsafe(String),
    #[error("{0}")]
    Io(String),
    /// The service answered with an error.
    #[error("{0}")]
    Service(CtlError),
}

impl ClientError {
    /// The service's error code, if it answered with one.
    pub fn code(&self) -> Option<&str> {
        match self {
            ClientError::Service(e) => Some(e.code),
            _ => None,
        }
    }
}

/// Whether a client may trust the socket at `path`: a directory only its
/// owner can write to, and the owner's uid, which the service must have.
fn trusted_owner(path: &Path) -> Result<u32, ClientError> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    let md = std::fs::symlink_metadata(dir).map_err(|e| ClientError::Io(format!("{}: {e}", dir.display())))?;
    if !md.file_type().is_dir() {
        return Err(ClientError::Unsafe(format!(
            "{} is not a directory; not using the control socket in it",
            dir.display()
        )));
    }
    if md.mode() & 0o022 != 0 {
        return Err(ClientError::Unsafe(format!(
            "{} can be written by others (mode {:o}); not using the control socket in it",
            dir.display(),
            md.mode() & 0o7777
        )));
    }
    Ok(md.uid())
}

fn code_of(s: &str) -> &'static str {
    // The codes this client knows, as 'static; others read as `failed`.
    [
        code::BAD_REQUEST,
        code::UNSUPPORTED_VERSION,
        code::UNKNOWN_METHOD,
        code::UNKNOWN_VAULT,
        code::NOT_RUNNING,
        code::PAUSED,
        code::BUSY,
        code::REFUSED,
        code::INVALID_CONFIG,
        code::FAILED,
        code::FORBIDDEN,
        code::SHUTTING_DOWN,
    ]
    .into_iter()
    .find(|c| *c == s)
    .unwrap_or(code::FAILED)
}

impl Client {
    /// Connect to the socket at `path`. `Ok(None)` when no service is
    /// listening there (no socket, or a stale one).
    pub fn connect(path: &Path) -> Result<Option<Client>, ClientError> {
        let owner = match trusted_owner(path) {
            Ok(o) => o,
            Err(ClientError::Io(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let stream = match std::os::unix::net::UnixStream::connect(path) {
            Ok(s) => s,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None)
            }
            Err(e) => return Err(ClientError::Io(format!("connecting to {}: {e}", path.display()))),
        };
        let peer = peer_of(stream.as_raw_fd()).map_err(|e| ClientError::Io(e.to_string()))?;
        if peer.uid != owner {
            return Err(ClientError::Unsafe(format!(
                "the process behind {} runs as uid {}, but the directory belongs to uid {owner}; not using it",
                path.display(),
                peer.uid
            )));
        }
        let reader = BufReader::new(stream.try_clone().map_err(|e| ClientError::Io(e.to_string()))?);
        Ok(Some(Client {
            stream,
            reader,
            path: path.to_path_buf(),
            next_id: 1,
        }))
    }

    /// The first service listening at [`client_sockets`].
    pub fn connect_default() -> Result<Option<Client>, ClientError> {
        for p in client_sockets() {
            if let Some(c) = Client::connect(&p)? {
                return Ok(Some(c));
            }
        }
        Ok(None)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Give up on a reply after `t` (`None`: wait as long as it takes).
    pub fn set_timeout(&self, t: Option<std::time::Duration>) {
        let _ = self.stream.set_read_timeout(t);
        let _ = self.stream.set_write_timeout(t);
    }

    fn send(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = json!(self.next_id);
        self.next_id += 1;
        let req = Request {
            v: PROTOCOL,
            id: id.clone(),
            method: method.into(),
            params,
        };
        self.stream
            .write_all(line(&req).as_bytes())
            .map_err(|e| ClientError::Io(format!("writing to {}: {e}", self.path.display())))?;
        Ok(id)
    }

    fn read(&mut self) -> Result<Incoming, ClientError> {
        let mut s = String::new();
        match self.reader.read_line(&mut s) {
            Ok(0) => Err(ClientError::Io(format!(
                "the service at {} closed the connection",
                self.path.display()
            ))),
            Ok(_) => Incoming::parse(&s).map_err(ClientError::Io),
            Err(e) => Err(ClientError::Io(format!("reading from {}: {e}", self.path.display()))),
        }
    }

    /// Send a request and wait for its response (events in between are
    /// dropped).
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = match self.send(method, params) {
            Ok(id) => id,
            // A service that refused this connection may have closed it
            // before the request went out: its refusal is still there to read.
            Err(e) => match self.read() {
                Ok(Incoming::Response(Response {
                    id: Value::Null,
                    error: Some(e),
                    ..
                })) => return Err(ClientError::Service(CtlError::new(code_of(&e.code), e.message))),
                _ => return Err(e),
            },
        };
        loop {
            match self.read()? {
                Incoming::Response(r) if r.id == id || r.id.is_null() => {
                    return match (r.result, r.error) {
                        (_, Some(e)) => Err(ClientError::Service(CtlError::new(code_of(&e.code), e.message))),
                        (Some(v), None) => Ok(v),
                        (None, None) => Ok(Value::Null),
                    };
                }
                _ => continue,
            }
        }
    }

    /// Subscribe to events (one vault's, or all). Returns once the service
    /// has taken the subscription; then [`Client::next_event`] reads them.
    pub fn subscribe(&mut self, vault: Option<&str>) -> Result<(), ClientError> {
        self.call(method::EVENTS, json!({ "vault": vault }))?;
        Ok(())
    }

    /// The next event (responses in between are dropped); an error when the
    /// service goes away.
    pub fn next_event(&mut self) -> Result<Event, ClientError> {
        loop {
            if let Incoming::Event(e) = self.read()? {
                return Ok(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_own_user_and_the_group_are_admitted() {
        let p = Policy { uid: 1000, group: None };
        let none = |_: u32, _: u32| false;
        assert!(p.admits_with(Peer { uid: 1000, gid: 1000 }, none));
        assert!(!p.admits_with(Peer { uid: 1001, gid: 1000 }, none), "another user");
        assert!(
            !p.admits_with(Peer { uid: 0, gid: 0 }, none),
            "root is another user too"
        );
        let g = Policy {
            uid: 1000,
            group: Some(500),
        };
        assert!(g.admits_with(Peer { uid: 1002, gid: 500 }, none), "primary group");
        assert!(g.admits_with(Peer { uid: 1003, gid: 7 }, |u, gid| u == 1003 && gid == 500));
        assert!(!g.admits_with(Peer { uid: 1004, gid: 7 }, |u, _| u == 1003));
    }

    #[test]
    fn this_users_own_groups_are_found() {
        let uid = current_uid();
        let (_, primary) = passwd_of(uid).expect("this user is in the user database");
        assert!(in_group(uid, primary));
    }
}
