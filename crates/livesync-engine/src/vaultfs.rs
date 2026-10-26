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
    /// The file's identity (device and inode) where the platform has one
    /// (`platform::file_id`); `None` when unknown.
    pub file_id: Option<(u64, u64)>,
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
    let file_id = crate::platform::file_id(&md);
    if !md.is_file() {
        return Ok(Some(DiskState {
            hash: format!("not-a-file:{:?}", md.file_type()),
            size: 0,
            mtime_ms: mtime_ms(&md),
            file_id,
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
        file_id,
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

fn temp_name() -> String {
    let n: u64 = rand::random();
    format!("{TEMP_PREFIX}{n:016x}{TEMP_SUFFIX}")
}

/// Result of [`atomic_write`].
#[derive(Debug)]
pub struct Written {
    /// The file now on disk.
    pub state: DiskState,
    /// Content an editor wrote into the old file between our check and the
    /// swap. The caller must put it back; it is a local edit.
    pub displaced: Option<Vec<u8>>,
}

fn hash_reader(f: &mut File) -> io::Result<(String, Vec<u8>)> {
    use std::io::Seek;
    f.seek(io::SeekFrom::Start(0))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok((hash_bytes(&buf), buf))
}

/// Open `target` for reading if it is a regular file matching `hash`.
/// `Ok(None)`: nothing there. `Err(Diverged)`: something else is there.
fn open_expected(target: &Path, expect: &Expect) -> Result<Option<File>, WriteError> {
    let md = match fs::symlink_metadata(target) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_err(target)(e)),
    };
    match (expect, md) {
        (Expect::Absent, None) => Ok(None),
        (Expect::Absent, Some(_)) => Err(WriteError::Diverged {
            expected: expect.clone(),
            found: read_state(target).ok().flatten().map(|s| s.hash),
        }),
        (Expect::Hash(_), None) => Err(WriteError::Diverged {
            expected: expect.clone(),
            found: None,
        }),
        (Expect::Hash(h), Some(md)) => {
            if !md.is_file() {
                return Err(WriteError::Diverged {
                    expected: expect.clone(),
                    found: Some(format!("not-a-file:{:?}", md.file_type())),
                });
            }
            let mut f = File::open(target).map_err(io_err(target))?;
            let (found, _) = hash_reader(&mut f).map_err(io_err(target))?;
            if &found != h {
                return Err(WriteError::Diverged {
                    expected: expect.clone(),
                    found: Some(found),
                });
            }
            Ok(Some(f))
        }
    }
}

/// Write `bytes` to `target` atomically, provided the target still matches
/// `expect`. Sets the mtime to `mtime_ms` (best effort). Returns the state of
/// the file now on disk.
///
/// The old file is held open across the swap and hashed again afterwards,
/// so an edit that lands between the check and the rename is caught and
/// returned in [`Written::displaced`] instead of being lost. A new file is
/// linked into place without replacing anything where the filesystem
/// supports hard links; elsewhere (Android's FUSE) the absence check runs
/// right before the rename.
pub fn atomic_write(target: &Path, bytes: &[u8], mtime_ms: i64, expect: &Expect) -> Result<Written, WriteError> {
    let dir = target.parent().ok_or_else(|| WriteError::Io {
        path: target.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"),
    })?;
    fs::create_dir_all(dir).map_err(io_err(dir))?;
    let tmp = dir.join(temp_name());
    let result = (|| -> Result<Option<Vec<u8>>, WriteError> {
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
        let old = open_expected(target, expect)?;
        match old {
            None => match fs::hard_link(&tmp, target) {
                Ok(()) => {
                    let _ = fs::remove_file(&tmp);
                    Ok(None)
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(WriteError::Diverged {
                    expected: expect.clone(),
                    found: read_state(target).ok().flatten().map(|s| s.hash),
                }),
                Err(_) => {
                    // No hard links here: check once more, then rename.
                    open_expected(target, expect)?;
                    fs::rename(&tmp, target).map_err(io_err(target))?;
                    Ok(None)
                }
            },
            Some(mut old) => {
                fs::rename(&tmp, target).map_err(io_err(target))?;
                let Expect::Hash(h) = expect else { unreachable!() };
                let (after, content) = hash_reader(&mut old).map_err(io_err(target))?;
                Ok((&after != h).then_some(content))
            }
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    let displaced = result?;
    let st = read_state(target)
        .map_err(io_err(target))?
        .ok_or_else(|| WriteError::Io {
            path: target.to_path_buf(),
            source: io::Error::new(io::ErrorKind::NotFound, "file vanished after rename"),
        })?;
    Ok(Written { state: st, displaced })
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    AlreadyGone,
    Diverged {
        found: String,
    },
    /// Removed, but an edit landed between the check and the unlink; its
    /// content is returned so the caller can put it back.
    Raced {
        displaced: Vec<u8>,
    },
}

/// Remove `target` only if it still hashes to `hash`; then prune empty parent
/// directories up to (not including) `root`. The file is held open across
/// the unlink and hashed again, so a racing edit is returned, not lost.
pub fn remove_if_matches(root: &Path, target: &Path, hash: &str) -> io::Result<RemoveOutcome> {
    let md = match fs::symlink_metadata(target) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(RemoveOutcome::AlreadyGone),
        Err(e) => return Err(e),
    };
    if !md.is_file() {
        return Ok(RemoveOutcome::Diverged {
            found: format!("not-a-file:{:?}", md.file_type()),
        });
    }
    let mut f = File::open(target)?;
    let (found, _) = hash_reader(&mut f)?;
    if found != hash {
        return Ok(RemoveOutcome::Diverged { found });
    }
    fs::remove_file(target)?;
    let (after, content) = hash_reader(&mut f)?;
    prune_empty_dirs(root, target);
    if after != hash {
        return Ok(RemoveOutcome::Raced { displaced: content });
    }
    Ok(RemoveOutcome::Removed)
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

// ----------------------------------------------------------------------
// Names the filesystem cannot hold
// ----------------------------------------------------------------------

/// Characters Android's shared storage (FAT rules, enforced by the FUSE layer
/// over `/storage/emulated`) refuses in a name, as in Android's
/// `FileUtils.isValidFatFilenameChar`. `/` separates components and is not
/// listed.
pub fn is_fat_invalid_char(c: char) -> bool {
    matches!(c, '"' | '*' | ':' | '<' | '>' | '?' | '\\' | '|' | '\u{7f}') || (c as u32) < 0x20
}

/// The distinct characters in `rel` that FAT-style storage refuses.
pub fn fat_invalid_chars(rel: &str) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for c in rel.chars().filter(|c| is_fat_invalid_char(*c)) {
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// errno values (identical on Linux and Android/bionic).
mod errno {
    pub const EPERM: i32 = 1;
    pub const EINVAL: i32 = 22;
    pub const ENAMETOOLONG: i32 = 36;
    pub const EILSEQ: i32 = 84;
}

/// Whether an I/O error on the vault path `rel` means the name itself cannot
/// exist on this filesystem, so retrying is pointless.
///
/// `ENAMETOOLONG` and `EILSEQ` always are. `EPERM` and `EINVAL` are what the
/// Android FUSE layer answers for forbidden characters, but `EPERM` also means
/// an ordinary permission problem, so those two count only when the path
/// contains a character FAT-style storage refuses.
pub fn is_unwritable_name_error(e: &io::Error, rel: &str) -> bool {
    match e.raw_os_error() {
        Some(errno::ENAMETOOLONG | errno::EILSEQ) => true,
        Some(errno::EPERM | errno::EINVAL) => !fat_invalid_chars(rel).is_empty(),
        _ => false,
    }
}

/// Whether `rel` exists under `root` with exactly this spelling in every
/// component, judged from directory listings. On a case-insensitive
/// filesystem `stat("A.md")` finds `a.md`, but only one spelling is listed.
pub fn exists_with_exact_case(root: &Path, rel: &str) -> bool {
    let mut dir = root.to_path_buf();
    for comp in rel.split('/').filter(|c| !c.is_empty()) {
        let Ok(rd) = fs::read_dir(&dir) else { return false };
        if !rd.flatten().any(|e| e.file_name() == std::ffi::OsStr::new(comp)) {
            return false;
        }
        dir.push(comp);
    }
    true
}

/// Create an empty `.nomedia` at the vault root, so MediaStore and Google
/// Photos skip the vault's images. Returns true when it was created.
pub fn ensure_nomedia(root: &Path) -> io::Result<bool> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(".nomedia"))
    {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

// The profile lock (a seam) lives in `platform`; re-exported for callers.
pub use crate::platform::{LockError, LockFile};

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
        let s = atomic_write(&t, b"one", 1_700_000_000_123, &Expect::Absent)
            .unwrap()
            .state;
        assert_eq!(fs::read(&t).unwrap(), b"one");
        assert_eq!(s.hash, hash_bytes(b"one"));
        assert_eq!(s.mtime_ms, 1_700_000_000_123);
        let w = atomic_write(&t, b"two", 0, &Expect::Hash(s.hash.clone())).unwrap();
        assert!(w.displaced.is_none());
        let s2 = w.state;
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
        let s = atomic_write(&t, b"base", 0, &Expect::Absent).unwrap().state;
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
        let s = atomic_write(&t, b"base", 0, &Expect::Absent).unwrap().state;
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
    fn an_edit_through_an_open_handle_is_not_lost() {
        // An editor that opened the file before our swap and writes after
        // it: the write lands in the old inode, which we re-read.
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("x.md");
        let s = atomic_write(&t, b"base", 0, &Expect::Absent).unwrap().state;
        let mut editor = OpenOptions::new().write(true).open(&t).unwrap();
        // Simulate the race: the editor writes between our check and rename
        // by writing now and making the check see the old hash.
        let w = {
            let expect = Expect::Hash(s.hash.clone());
            let mut old = open_expected(&t, &expect).unwrap().unwrap();
            editor.write_all(b"EDIT").unwrap();
            editor.sync_all().unwrap();
            let tmp = d.path().join(temp_name());
            fs::write(&tmp, b"remote").unwrap();
            fs::rename(&tmp, &t).unwrap();
            hash_reader(&mut old).unwrap()
        };
        assert_ne!(w.0, s.hash);
        assert_eq!(w.1, b"EDIT");
        assert_eq!(fs::read(&t).unwrap(), b"remote");
    }

    #[test]
    fn new_files_never_replace_one_that_appeared() {
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("n.md");
        fs::write(&t, b"user").unwrap();
        assert!(matches!(
            atomic_write(&t, b"remote", 0, &Expect::Absent),
            Err(WriteError::Diverged { .. })
        ));
        assert_eq!(fs::read(&t).unwrap(), b"user");
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1, "temp file removed");
    }

    #[test]
    fn unwritable_name_errors() {
        let e = |n| io::Error::from_raw_os_error(n);
        assert!(is_unwritable_name_error(&e(errno::EPERM), "He said \"hi\".md"));
        assert!(is_unwritable_name_error(&e(errno::EINVAL), "a/b:c.md"));
        assert!(is_unwritable_name_error(&e(errno::ENAMETOOLONG), "plain.md"));
        assert!(is_unwritable_name_error(&e(errno::EILSEQ), "plain.md"));
        // A permission problem on an ordinary name is retried, not skipped.
        assert!(!is_unwritable_name_error(&e(errno::EPERM), "plain.md"));
        assert!(!is_unwritable_name_error(&e(13), "a:b.md"), "EACCES");
        assert!(!is_unwritable_name_error(&e(20), "a:b.md"), "ENOTDIR");
        assert_eq!(fat_invalid_chars("a\"b\"*<>?|:\\c\u{1}.md").len(), 9);
        assert!(fat_invalid_chars("Notes/æøå [x] (y) #z.md").is_empty());
    }

    #[test]
    fn exact_case_lookup() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("Dir")).unwrap();
        fs::write(d.path().join("Dir/note.md"), b"x").unwrap();
        assert!(exists_with_exact_case(d.path(), "Dir/note.md"));
        assert!(!exists_with_exact_case(d.path(), "Dir/Note.md"));
        assert!(!exists_with_exact_case(d.path(), "dir/note.md"));
        assert!(!exists_with_exact_case(d.path(), "Dir/missing.md"));
    }

    #[test]
    fn nomedia_and_lock() {
        let d = tempfile::tempdir().unwrap();
        assert!(ensure_nomedia(d.path()).unwrap());
        assert!(!ensure_nomedia(d.path()).unwrap());
        let p = d.path().join("s/lsync.lock");
        let held = LockFile::try_acquire(&p).unwrap();
        assert!(matches!(LockFile::try_acquire(&p), Err(LockError::Busy(_))));
        drop(held);
        LockFile::try_acquire(&p).unwrap();
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
