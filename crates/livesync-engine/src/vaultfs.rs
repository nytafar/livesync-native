//! Vault file operations. Writes go to a hidden temp file in the target's
//! directory, are fsynced, the target is re-checked against what the index
//! expects, and only then renamed into place. Nothing is overwritten or
//! deleted unless it still hashes to what the index recorded.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use xxhash_rust::xxh64::Xxh64;

/// Prefix of our temp files. Dot-prefixed so Obsidian and the sync rules ignore them.
pub const TEMP_PREFIX: &str = ".lsync-";
pub const TEMP_SUFFIX: &str = ".tmp";

pub fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:016x}", xxhash_rust::xxh64::xxh64(bytes, 0))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskState {
    pub hash: String,
    pub size: u64,
    pub mtime_ms: i64,
    #[cfg(unix)]
    pub ino: (u64, u64),
}

fn mtime_ms(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Hash a file; `None` if it does not exist. Directories count as "present
/// with an unhashable value" so they are never replaced.
pub fn read_state(path: &Path) -> io::Result<Option<DiskState>> {
    let md = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    #[cfg(unix)]
    let ino = {
        use std::os::unix::fs::MetadataExt;
        (md.dev(), md.ino())
    };
    if !md.is_file() {
        return Ok(Some(DiskState {
            hash: format!("not-a-file:{:?}", md.file_type()),
            size: 0,
            mtime_ms: mtime_ms(&md),
            #[cfg(unix)]
            ino,
        }));
    }
    let mut f = File::open(path)?;
    let mut h = Xxh64::new(0);
    let mut buf = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        h.update(&buf[..n]);
    }
    Ok(Some(DiskState {
        hash: format!("{:016x}", h.digest()),
        size,
        mtime_ms: mtime_ms(&md),
        #[cfg(unix)]
        ino,
    }))
}

/// What the target must look like right before the swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// No file at the target.
    Absent,
    /// A file with this xxh64.
    Hash(String),
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("local file changed (expected {expected:?}, found {found:?})")]
    Diverged { expected: Expect, found: Option<String> },
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> WriteError + '_ {
    move |source| WriteError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn matches(expect: &Expect, now: &Option<DiskState>) -> bool {
    match (expect, now) {
        (Expect::Absent, None) => true,
        (Expect::Hash(h), Some(s)) => &s.hash == h,
        _ => false,
    }
}

fn temp_name() -> String {
    let n: u64 = rand::random();
    format!("{TEMP_PREFIX}{n:016x}{TEMP_SUFFIX}")
}

/// Write `bytes` to `target` atomically, provided the target still matches
/// `expect`. Sets the mtime to `mtime_ms` (best effort). Returns the state of
/// the file now on disk.
pub fn atomic_write(target: &Path, bytes: &[u8], mtime_ms: i64, expect: &Expect) -> Result<DiskState, WriteError> {
    let dir = target.parent().ok_or_else(|| WriteError::Io {
        path: target.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"),
    })?;
    fs::create_dir_all(dir).map_err(io_err(dir))?;
    let tmp = dir.join(temp_name());
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(io_err(&tmp))?;
        f.write_all(bytes).map_err(io_err(&tmp))?;
        if mtime_ms > 0 {
            let t = UNIX_EPOCH + Duration::from_millis(mtime_ms as u64);
            // Best effort: some FUSE mounts refuse to set times.
            let _ = f.set_modified(t);
        }
        f.sync_all().map_err(io_err(&tmp))?;
        drop(f);
        let now = read_state(target).map_err(io_err(target))?;
        if !matches(expect, &now) {
            return Err(WriteError::Diverged {
                expected: expect.clone(),
                found: now.map(|s| s.hash),
            });
        }
        fs::rename(&tmp, target).map_err(io_err(target))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    let st = read_state(target)
        .map_err(io_err(target))?
        .ok_or_else(|| WriteError::Io {
            path: target.to_path_buf(),
            source: io::Error::new(io::ErrorKind::NotFound, "file vanished after rename"),
        })?;
    Ok(st)
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    AlreadyGone,
    Diverged { found: String },
}

/// Remove `target` only if it still hashes to `hash`; then prune empty parent
/// directories up to (not including) `root`.
pub fn remove_if_matches(root: &Path, target: &Path, hash: &str) -> io::Result<RemoveOutcome> {
    match read_state(target)? {
        None => Ok(RemoveOutcome::AlreadyGone),
        Some(s) if s.hash != hash => Ok(RemoveOutcome::Diverged { found: s.hash }),
        Some(_) => {
            fs::remove_file(target)?;
            prune_empty_dirs(root, target);
            Ok(RemoveOutcome::Removed)
        }
    }
}

pub fn prune_empty_dirs(root: &Path, from: &Path) {
    let mut cur = from.parent();
    while let Some(dir) = cur {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break; // not empty, or not removable
        }
        cur = dir.parent();
    }
}

/// Remove stale temp files left by an interrupted run.
pub fn sweep_temp_files(root: &Path) -> usize {
    let mut removed = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if !name.starts_with('.') {
                    stack.push(e.path());
                }
            } else if name.starts_with(TEMP_PREFIX) && name.ends_with(TEMP_SUFFIX) && fs::remove_file(e.path()).is_ok()
            {
                removed += 1;
            }
        }
    }
    removed
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_new_then_replace_clean() {
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("a/b/c.md");
        let s = atomic_write(&t, b"one", 1_700_000_000_123, &Expect::Absent).unwrap();
        assert_eq!(fs::read(&t).unwrap(), b"one");
        assert_eq!(s.hash, hash_bytes(b"one"));
        assert_eq!(s.mtime_ms, 1_700_000_000_123);
        let s2 = atomic_write(&t, b"two", 0, &Expect::Hash(s.hash.clone())).unwrap();
        assert_eq!(fs::read(&t).unwrap(), b"two");
        assert_eq!(s2.size, 3);
        // No temp files left behind.
        let names: Vec<_> = fs::read_dir(t.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn refuses_to_overwrite_changed_file() {
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("x.md");
        let s = atomic_write(&t, b"base", 0, &Expect::Absent).unwrap();
        fs::write(&t, b"local edit").unwrap();
        let e = atomic_write(&t, b"remote", 0, &Expect::Hash(s.hash)).unwrap_err();
        assert!(matches!(e, WriteError::Diverged { .. }));
        assert_eq!(fs::read(&t).unwrap(), b"local edit");
        // Expect::Absent never replaces an existing file.
        assert!(matches!(
            atomic_write(&t, b"r", 0, &Expect::Absent),
            Err(WriteError::Diverged { .. })
        ));
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn remove_only_when_clean() {
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("dir/sub/x.md");
        let s = atomic_write(&t, b"base", 0, &Expect::Absent).unwrap();
        fs::write(&t, b"edited").unwrap();
        assert!(matches!(
            remove_if_matches(d.path(), &t, &s.hash).unwrap(),
            RemoveOutcome::Diverged { .. }
        ));
        fs::write(&t, b"base").unwrap();
        assert_eq!(
            remove_if_matches(d.path(), &t, &s.hash).unwrap(),
            RemoveOutcome::Removed
        );
        assert!(!d.path().join("dir").exists(), "empty parents pruned");
        assert_eq!(
            remove_if_matches(d.path(), &t, &s.hash).unwrap(),
            RemoveOutcome::AlreadyGone
        );
    }

    #[test]
    fn sweep() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("a")).unwrap();
        fs::write(d.path().join("a/.lsync-00.tmp"), b"x").unwrap();
        fs::write(d.path().join("a/keep.md"), b"x").unwrap();
        assert_eq!(sweep_temp_files(d.path()), 1);
        assert!(d.path().join("a/keep.md").exists());
    }
}
