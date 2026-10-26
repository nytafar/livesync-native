//! The `ProfileLock` seam (spec "Platform seams"): one engine per state
//! directory and vault. `flock` where the filesystem supports it; on
//! Android's shared storage it is a no-op, so the profile store's overlap
//! check is what protects there.

use std::fs::{self, File, OpenOptions};
use std::io;
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

impl LockFile {
    /// Take the lock without waiting.
    pub fn try_acquire(path: &Path) -> Result<LockFile, LockError> {
        let io_err = |source| LockError::Io {
            path: path.to_path_buf(),
            source,
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(io_err)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io_err)?;
        match file.try_lock() {
            Ok(()) => Ok(LockFile {
                _file: file,
                path: path.to_path_buf(),
            }),
            Err(fs::TryLockError::WouldBlock) => Err(LockError::Busy(path.to_path_buf())),
            Err(fs::TryLockError::Error(e)) => Err(io_err(e)),
        }
    }
}
