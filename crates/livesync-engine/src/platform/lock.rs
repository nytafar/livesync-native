//! `flock` lock files and the holder record they carry (part of the
//! `ProfileLock` seam, spec "Platform seams").
//!
//! A [`LockFile`] is an exclusive `flock` (`std::fs::File::try_lock`), which
//! the kernel drops when the process dies, so it never goes stale. The state
//! directory's lock is one; the vault folder's flock is part of
//! [`super::VaultLock`]. The file's content is only a [`Holder`] record, so
//! a refused engine can say who has the lock: it plays no part in the
//! locking itself.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// An exclusive advisory lock (`flock`) on a lock file, held until dropped.
#[derive(Debug)]
pub struct LockFile {
    _file: File,
    pub path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("{0} is locked by another lsync process")]
    Busy(PathBuf),
    #[error("cannot lock {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Who holds a lock: written into the lock file by the process that took it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    #[serde(default)]
    pub uid: Option<u32>,
    /// Process name (`/proc/self/cmdline` where there is one: the package
    /// name for the Android app, `lsync` for the CLI).
    #[serde(default)]
    pub process: String,
    #[serde(default)]
    pub host: String,
    /// The holder's state directory (which profile).
    #[serde(default)]
    pub state_dir: String,
    #[serde(default)]
    pub since_ms: i64,
}

impl Holder {
    /// This process, for an engine with this state directory.
    pub fn current(state_dir: &Path) -> Holder {
        Holder {
            pid: std::process::id(),
            uid: super::current_uid(),
            process: super::process_name(),
            host: super::hostname(),
            state_dir: state_dir.display().to_string(),
            since_ms: crate::vaultfs::now_ms(),
        }
    }
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = if self.process.is_empty() {
            "a process"
        } else {
            &self.process
        };
        write!(f, "{name} (pid {}", self.pid)?;
        if let Some(uid) = self.uid {
            write!(f, ", uid {uid}")?;
        }
        if !self.host.is_empty() && self.host != super::hostname() {
            write!(f, ", on {}", self.host)?;
        }
        write!(f, ")")?;
        if !self.state_dir.is_empty() {
            write!(f, " with state folder {}", self.state_dir)?;
        }
        Ok(())
    }
}

/// The holder record in a lock file, if it has a readable one.
pub fn read_holder(path: &Path) -> Option<Holder> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `flock` needs a writable descriptor here (Linux emulates it with `fcntl`
/// locks on NFS, which refuse a write lock on a read-only descriptor).
fn wants_write_access(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EBADF)
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

fn try_lock(file: &File) -> Result<(), Option<io::Error>> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(fs::TryLockError::WouldBlock) => Err(None),
        Err(fs::TryLockError::Error(e)) => Err(Some(e)),
    }
}

impl LockFile {
    /// Take the lock without waiting. An existing lock file is opened
    /// read-only, so a refused attempt never writes to it (no change event
    /// for the holder's watcher, no write on shared storage).
    pub fn try_acquire(path: &Path) -> Result<LockFile, LockError> {
        let io_err = |source| LockError::Io {
            path: path.to_path_buf(),
            source,
        };
        let busy = || LockError::Busy(path.to_path_buf());
        let open_rw = || {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
        };
        let (file, writable) = match File::open(path) {
            Ok(f) => (f, false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir).map_err(io_err)?;
                }
                (open_rw().map_err(io_err)?, true)
            }
            Err(e) => return Err(io_err(e)),
        };
        let file = match try_lock(&file) {
            Ok(()) => file,
            Err(None) => return Err(busy()),
            Err(Some(e)) if !writable && wants_write_access(&e) => {
                let file = open_rw().map_err(io_err)?;
                match try_lock(&file) {
                    Ok(()) => file,
                    Err(None) => return Err(busy()),
                    Err(Some(e)) => return Err(io_err(e)),
                }
            }
            Err(Some(e)) => return Err(io_err(e)),
        };
        Ok(LockFile {
            _file: file,
            path: path.to_path_buf(),
        })
    }

    /// Write the holder record (best effort; it only names the holder).
    pub fn record(&self, holder: &Holder) -> io::Result<()> {
        write_holder(&self.path, holder)
    }
}

/// Replace a lock file's holder record. Only the lock's holder calls this.
pub(crate) fn write_holder(path: &Path, holder: &Holder) -> io::Result<()> {
    let json = serde_json::to_vec(holder).map_err(io::Error::other)?;
    let mut f = OpenOptions::new().write(true).truncate(true).open(path)?;
    f.write_all(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_lock_refuses_and_names_its_holder() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s/lsync.lock");
        let held = LockFile::try_acquire(&p).unwrap();
        let me = Holder::current(Path::new("/state/a"));
        held.record(&me).unwrap();
        assert!(matches!(LockFile::try_acquire(&p), Err(LockError::Busy(_))));
        assert_eq!(read_holder(&p), Some(me.clone()));
        let shown = me.to_string();
        assert!(shown.contains(&format!("pid {}", std::process::id())), "{shown}");
        assert!(shown.contains("/state/a"), "{shown}");
        drop(held);
        LockFile::try_acquire(&p).unwrap();
    }

    #[test]
    fn a_refused_attempt_leaves_the_file_alone() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("lsync.lock");
        let held = LockFile::try_acquire(&p).unwrap();
        held.record(&Holder::current(d.path())).unwrap();
        let before = (fs::read(&p).unwrap(), fs::metadata(&p).unwrap().modified().unwrap());
        assert!(LockFile::try_acquire(&p).is_err());
        let after = (fs::read(&p).unwrap(), fs::metadata(&p).unwrap().modified().unwrap());
        assert_eq!(before, after);
    }

    #[test]
    fn an_unreadable_record_names_nobody() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("lsync.lock");
        fs::write(&p, b"").unwrap();
        assert_eq!(read_holder(&p), None);
        fs::write(&p, b"{\"pid\":7}").unwrap();
        assert_eq!(read_holder(&p).map(|h| h.pid), Some(7));
    }
}
