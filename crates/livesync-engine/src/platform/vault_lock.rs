//! The vault folder's lock (part of the `ProfileLock` seam): two engines
//! never run against one vault folder, whichever profile or process they
//! come from.
//!
//! Two locks, both released by the kernel when their process dies (crash,
//! SIGKILL, force-stop, reboot), so neither can go stale and neither needs a
//! heartbeat:
//!
//! - **flock** on `<vault>/.lsync.lock`. Works on local filesystems and NFS,
//!   across machines where the filesystem supports it. On Android's shared
//!   storage (FUSE) it fails with ENOSYS.
//! - **A host lock**, per vault folder and per machine:
//!   - Linux and Android: a datagram socket bound to a name in the abstract
//!     Unix-socket namespace. Binding is exclusive across every process and
//!     uid in the network namespace and needs no file. Android apps (and
//!     Termux, and the adb shell) share one network namespace.
//!   - Other Unix (macOS): flock on a file in `/var/tmp`. Not `/tmp`, which
//!     macOS's periodic `clean-tmps` empties of files untouched for three
//!     days, even while a long-running holder still has them locked.
//!   - Elsewhere: none.
//!
//! The host lock is keyed twice: by the folder's (device, inode) and by its
//! canonical path, Unicode-normalised and lower-cased (shared storage folds
//! case, so `Obsidian/` and `obsidian/` are one folder there). Either key in
//! use means busy, so a folder reached through another spelling is still
//! caught. The price is a refusal for two folders whose paths differ only in
//! case on a case-sensitive filesystem.
//!
//! At least one of the two must be taken; when neither is available, the
//! engine refuses to start rather than run unguarded. Contention on either
//! is a refusal.
//!
//! The lock file itself stays empty and never changes, so a tool that
//! replicates dot-files (and might rename a copy over it, which would leave
//! the holder's flock on an orphaned inode) has no reason to touch it. The
//! holder's record goes next to it, in `.lsync.holder`, written once when the
//! lock is taken so a refused engine can name the holder. It is minimal
//! ([`Holder::for_vault`]: process name, pid, uid, host), because whatever
//! replicates dot-files carries it to other machines.

use super::lock::{read_holder, write_holder, Holder, LockError, LockFile};
use std::io;
use std::path::{Path, PathBuf};
use tracing::debug;
use unicode_normalization::UnicodeNormalization;

/// The vault folder's locks, held until dropped.
#[derive(Debug)]
pub struct VaultLock {
    _host: Option<HostLock>,
    _file: Option<LockFile>,
    mode: String,
}

impl VaultLock {
    /// Which locks are held, for status (e.g. "host lock only (...)").
    pub fn mode(&self) -> &str {
        &self.mode
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VaultLockError {
    /// Another engine holds the vault folder.
    #[error("another LiveSync engine is using the vault folder {}: {}", vault.display(), match holder {
        Some(h) => format!("held by {h}"),
        None => "its holder left no record".to_string(),
    })]
    Busy { vault: PathBuf, holder: Option<Holder> },
    /// Neither lock can be taken on this filesystem and platform.
    #[error("cannot lock the vault folder {}: {reason}", vault.display())]
    Unavailable { vault: PathBuf, reason: String },
}

/// Why the host lock was not taken.
#[derive(Debug)]
pub(crate) enum HostError {
    Busy,
    Unavailable(String),
}

/// The host lock: one held key per entry.
#[derive(Debug)]
pub(crate) struct HostLock {
    _held: Vec<Held>,
}

#[derive(Debug)]
#[allow(dead_code)] // held for their Drop only
enum Held {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Socket(std::os::unix::net::UnixDatagram),
    File(LockFile),
}

/// Lock the vault folder `vault` (canonical) with `lock_file` at its root.
/// `flock: false` behaves as if flock failed with ENOSYS, as on Android's
/// shared storage, so that path runs in tests on any machine.
pub fn lock_vault(vault: &Path, lock_file: &Path, holder: &Holder, flock: bool) -> Result<VaultLock, VaultLockError> {
    let file_lock = |p: &Path| {
        if flock {
            LockFile::try_acquire(p)
        } else {
            simulated_no_flock(p)
        }
    };
    lock_vault_with(vault, lock_file, holder, &file_lock, &host_lock)
}

fn simulated_no_flock(p: &Path) -> Result<LockFile, LockError> {
    // The lock file still exists afterwards, as on FUSE (open works, flock
    // does not).
    let open = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(p);
    if let Err(source) = open {
        return Err(LockError::Io {
            path: p.to_path_buf(),
            source,
        });
    }
    #[cfg(unix)]
    let source = io::Error::from_raw_os_error(libc::ENOSYS);
    #[cfg(not(unix))]
    let source = io::Error::new(io::ErrorKind::Unsupported, "flock not supported");
    Err(LockError::Io {
        path: p.to_path_buf(),
        source,
    })
}

type FileLockFn<'a> = &'a dyn Fn(&Path) -> Result<LockFile, LockError>;
type HostLockFn<'a> = &'a dyn Fn(&Path) -> Result<HostLock, HostError>;

fn lock_vault_with(
    vault: &Path,
    lock_file: &Path,
    holder: &Holder,
    file_lock: FileLockFn,
    host_lock: HostLockFn,
) -> Result<VaultLock, VaultLockError> {
    let busy = || VaultLockError::Busy {
        vault: vault.to_path_buf(),
        holder: read_holder(&record_path(lock_file)),
    };
    let host = match host_lock(vault) {
        Ok(h) => Ok(h),
        Err(HostError::Busy) => return Err(busy()),
        Err(HostError::Unavailable(e)) => Err(e),
    };
    let file = match file_lock(lock_file) {
        Ok(f) => Ok(f),
        Err(LockError::Busy(_)) => return Err(busy()),
        Err(LockError::Io { source, .. }) => Err(source.to_string()),
    };
    let (host, file, mode) = match (host, file) {
        (Ok(h), Ok(f)) => (Some(h), Some(f), "flock and host lock".to_string()),
        (Ok(h), Err(f)) => (Some(h), None, format!("host lock only (flock: {f})")),
        (Err(h), Ok(f)) => (None, Some(f), format!("flock only (host lock: {h})")),
        (Err(h), Err(f)) => {
            return Err(VaultLockError::Unavailable {
                vault: vault.to_path_buf(),
                reason: format!(
                    "flock on {} failed ({f}) and there is no host lock ({h})",
                    lock_file.display()
                ),
            })
        }
    };
    let record = record_path(lock_file);
    if let Err(e) = write_holder(&record, &holder.for_vault()) {
        debug!("cannot record the holder in {}: {e}", record.display());
    }
    Ok(VaultLock {
        _host: host,
        _file: file,
        mode,
    })
}

/// Where the holder's record goes: `.lsync.holder` next to `.lsync.lock`.
pub fn record_path(lock_file: &Path) -> PathBuf {
    lock_file.with_extension("holder")
}

/// The host lock's keys for a vault folder: its (device, inode), and its
/// canonical path folded for case and Unicode normalisation.
fn host_keys(vault: &Path) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some((dev, ino)) = std::fs::metadata(vault).ok().as_ref().and_then(super::file_id) {
        keys.push(format!("id-{dev:x}-{ino:x}"));
    }
    let folded = vault.to_string_lossy().nfc().collect::<String>().to_lowercase();
    keys.push(format!("path-{:016x}", xxhash_rust::xxh64::xxh64(folded.as_bytes(), 0)));
    keys
}

/// Take every key, or none.
fn take_keys(vault: &Path, take: impl Fn(&str) -> Result<Held, HostError>) -> Result<HostLock, HostError> {
    let mut held = Vec::new();
    for key in host_keys(vault) {
        held.push(take(&key)?);
    }
    Ok(HostLock { _held: held })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn host_lock(vault: &Path) -> Result<HostLock, HostError> {
    take_keys(vault, |key| bind_abstract(&format!("livesync-native/vault/v1/{key}")))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn host_lock(vault: &Path) -> Result<HostLock, HostError> {
    host_lock_in(vault, Path::new(HOST_LOCK_DIR))
}

/// Where the flock host lock keeps its files: local, shared by all users,
/// and (unlike `/tmp`) not emptied by macOS's periodic `clean-tmps`.
#[cfg(unix)]
#[cfg_attr(any(target_os = "linux", target_os = "android"), allow(dead_code))]
const HOST_LOCK_DIR: &str = "/var/tmp";

#[cfg(not(unix))]
fn host_lock(_vault: &Path) -> Result<HostLock, HostError> {
    Err(HostError::Unavailable("not implemented on this platform".into()))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn bind_abstract(name: &str) -> Result<Held, HostError> {
    #[cfg(target_os = "android")]
    use std::os::android::net::SocketAddrExt;
    #[cfg(target_os = "linux")]
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let addr = SocketAddr::from_abstract_name(name.as_bytes())
        .map_err(|e| HostError::Unavailable(format!("abstract socket name: {e}")))?;
    match UnixDatagram::bind_addr(&addr) {
        Ok(s) => Ok(Held::Socket(s)),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => Err(HostError::Busy),
        Err(e) => Err(HostError::Unavailable(format!("abstract socket: {e}"))),
    }
}

/// The host lock as flock files in a shared local directory
/// (`HOST_LOCK_DIR` on macOS). Built on every Unix so its tests run on Linux too.
#[cfg(unix)]
#[cfg_attr(any(target_os = "linux", target_os = "android"), allow(dead_code))]
fn host_lock_in(vault: &Path, dir: &Path) -> Result<HostLock, HostError> {
    take_keys(vault, |key| {
        let path = dir.join(format!("livesync-native-vault-v1-{key}.lock"));
        let existed = path.exists();
        match LockFile::try_acquire(&path) {
            Ok(l) => {
                if !existed {
                    // Other users must be able to open it (read-only) to lock it.
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
                }
                Ok(Held::File(l))
            }
            Err(LockError::Busy(_)) => Err(HostError::Busy),
            Err(e) => Err(HostError::Unavailable(e.to_string())),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A holder told apart by its process name (they all share our pid).
    fn holder(name: &str) -> Holder {
        let mut h = Holder::current(Path::new("/state"));
        h.process = name.to_string();
        h
    }

    fn vault_in(d: &Path, name: &str) -> (PathBuf, PathBuf) {
        let v = d.join(name);
        fs::create_dir_all(&v).unwrap();
        let v = fs::canonicalize(v).unwrap();
        let f = v.join(".lsync.lock");
        (v, f)
    }

    fn holder_of(r: Result<VaultLock, VaultLockError>) -> Option<Holder> {
        match r {
            Err(VaultLockError::Busy { holder, .. }) => holder,
            other => panic!("expected busy, got {other:?}"),
        }
    }

    #[test]
    fn a_second_lock_on_one_folder_is_refused_with_and_without_flock() {
        let d = tempfile::tempdir().unwrap();
        let (v, f) = vault_in(d.path(), "vault");
        for flock in [true, false] {
            let first = lock_vault(&v, &f, &holder("/state/one"), flock).unwrap();
            let want = if flock { "flock and host lock" } else { "host lock only" };
            assert!(first.mode().starts_with(want), "{}", first.mode());
            // A second profile (another state folder), with or without flock.
            for second_flock in [true, false] {
                let got = holder_of(lock_vault(&v, &f, &holder("/state/two"), second_flock));
                assert_eq!(got.map(|h| h.process), Some("/state/one".to_string()));
            }
            drop(first);
            lock_vault(&v, &f, &holder("/state/two"), flock).expect("free once the first is gone");
        }
    }

    #[test]
    fn the_no_flock_path_is_enosys_and_still_exclusive() {
        let d = tempfile::tempdir().unwrap();
        let (v, f) = vault_in(d.path(), "vault");
        let first = lock_vault(&v, &f, &holder("/state/one"), false).unwrap();
        if cfg!(target_os = "linux") {
            assert!(first.mode().contains("os error 38"), "{}", first.mode());
        }
        let refused = lock_vault(&v, &f, &holder("/state/two"), false).unwrap_err();
        let msg = refused.to_string();
        assert!(msg.contains(&format!("pid {}", std::process::id())), "{msg}");
        assert!(msg.contains("/state/one"), "{msg}");
        // The lock file itself stays empty; the record is beside it.
        assert_eq!(fs::metadata(&f).unwrap().len(), 0);
        assert!(record_path(&f).ends_with(".lsync.holder"));
        assert_eq!(read_holder(&record_path(&f)).unwrap().state_dir, "");
    }

    #[test]
    fn other_spellings_of_the_folder_are_the_same_folder() {
        let d = tempfile::tempdir().unwrap();
        let (v, f) = vault_in(d.path(), "Vault");
        let _first = lock_vault(&v, &f, &holder("/s/1"), false).unwrap();
        // Through a symlink, not canonicalised: same (device, inode).
        #[cfg(unix)]
        {
            let link = d.path().join("link");
            std::os::unix::fs::symlink(&v, &link).unwrap();
            holder_of(lock_vault(&link, &link.join(".lsync.lock"), &holder("/s/2"), false));
        }
        // In another case: one folder on shared storage, refused everywhere.
        let upper = PathBuf::from(v.to_string_lossy().to_uppercase());
        holder_of(lock_vault(&upper, &upper.join(".lsync.lock"), &holder("/s/3"), false));
        // Another folder is free.
        let (w, g) = vault_in(d.path(), "other");
        lock_vault(&w, &g, &holder("/s/4"), false).unwrap();
    }

    #[test]
    fn neither_lock_means_refusal_and_either_lock_is_enough() {
        let d = tempfile::tempdir().unwrap();
        let (v, f) = vault_in(d.path(), "vault");
        let no_host = |_: &Path| -> Result<HostLock, HostError> { Err(HostError::Unavailable("denied".into())) };
        let err = lock_vault_with(&v, &f, &holder("/s"), &simulated_no_flock, &no_host).unwrap_err();
        assert!(matches!(err, VaultLockError::Unavailable { .. }), "{err:?}");
        assert!(err.to_string().contains("denied"), "{err}");
        let l = lock_vault_with(&v, &f, &holder("/s"), &LockFile::try_acquire, &no_host).unwrap();
        assert!(l.mode().starts_with("flock only"), "{}", l.mode());
        drop(l);
        let l = lock_vault_with(&v, &f, &holder("/s"), &simulated_no_flock, &host_lock).unwrap();
        assert!(l.mode().starts_with("host lock only"), "{}", l.mode());
    }

    #[test]
    fn a_leftover_record_from_a_dead_holder_does_not_lock() {
        let d = tempfile::tempdir().unwrap();
        let (v, f) = vault_in(d.path(), "vault");
        let mut dead = holder("/s/dead");
        dead.pid = 999_999;
        write_holder_new(&record_path(&f), &dead);
        let l = lock_vault(&v, &f, &holder("/s/new"), false).unwrap();
        assert_eq!(read_holder(&record_path(&f)).unwrap().process, "/s/new");
        drop(l);
        fs::write(record_path(&f), b"garbage").unwrap();
        fs::write(&f, b"garbage").unwrap();
        lock_vault(&v, &f, &holder("/s/new"), false).unwrap();
    }

    fn write_holder_new(p: &Path, h: &Holder) {
        fs::write(p, serde_json::to_vec(h).unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_tmp_flock_host_lock_is_exclusive() {
        let d = tempfile::tempdir().unwrap();
        let (v, f) = vault_in(d.path(), "vault");
        let locks = d.path().join("locks");
        fs::create_dir(&locks).unwrap();
        let host = |p: &Path| host_lock_in(p, &locks);
        let first = lock_vault_with(&v, &f, &holder("/s/1"), &simulated_no_flock, &host).unwrap();
        assert!(matches!(
            lock_vault_with(&v, &f, &holder("/s/2"), &simulated_no_flock, &host),
            Err(VaultLockError::Busy { .. })
        ));
        drop(first);
        lock_vault_with(&v, &f, &holder("/s/2"), &simulated_no_flock, &host).unwrap();
    }

    /// The macOS host lock in its real directory (`/var/tmp`, present on
    /// Linux too): exclusive, and its files are removed again afterwards.
    #[cfg(unix)]
    #[test]
    fn the_flock_host_lock_works_in_its_real_directory() {
        let d = tempfile::tempdir().unwrap();
        let (v, f) = vault_in(d.path(), "vault");
        let dir = Path::new(HOST_LOCK_DIR);
        let host = |p: &Path| host_lock_in(p, dir);
        let first = lock_vault_with(&v, &f, &holder("/s/1"), &simulated_no_flock, &host).unwrap();
        assert!(matches!(
            lock_vault_with(&v, &f, &holder("/s/2"), &simulated_no_flock, &host),
            Err(VaultLockError::Busy { .. })
        ));
        drop(first);
        for key in host_keys(&v) {
            let p = dir.join(format!("livesync-native-vault-v1-{key}.lock"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o644, "{}", p.display());
            }
            fs::remove_file(p).unwrap();
        }
    }
}
