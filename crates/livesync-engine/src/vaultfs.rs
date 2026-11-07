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
/// moved into place by a rename that never replaces anything (see
/// [`rename_noreplace`]); where the kernel or filesystem cannot do that, it
/// is linked into place, and where there are no hard links either
/// (Android's FUSE) the absence check runs right before the rename.
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
        let diverged = || WriteError::Diverged {
            expected: expect.clone(),
            found: read_state(target).ok().flatten().map(|s| s.hash),
        };
        match old {
            // An exclusive rename first: a file that was ever a hard link
            // is invisible to FSEvents on macOS (no event when it is
            // edited or deleted later), so linking is only the fallback.
            None => match rename_noreplace(&tmp, target) {
                Some(Ok(())) => Ok(None),
                Some(Err(e)) if e.kind() == io::ErrorKind::AlreadyExists => Err(diverged()),
                Some(Err(e)) => Err(io_err(target)(e)),
                None => match fs::hard_link(&tmp, target) {
                    Ok(()) => {
                        let _ = fs::remove_file(&tmp);
                        Ok(None)
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(diverged()),
                    Err(_) => {
                        // No hard links here: check once more, then rename.
                        open_expected(target, expect)?;
                        fs::rename(&tmp, target).map_err(io_err(target))?;
                        Ok(None)
                    }
                },
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
    /// Moved into a trash (`<vault>/.trash/` or the system's) instead.
    Trashed {
        to: PathBuf,
    },
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

// ----------------------------------------------------------------------
// Trash
// ----------------------------------------------------------------------

/// Where a remote deletion puts the local file (`trash` in the vault's
/// settings).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrashMode {
    /// `<vault>/.trash/<path>`, folders kept (a dot-folder, so never synced).
    #[default]
    Vault,
    /// The freedesktop trash, or `~/.Trash` on macOS.
    System,
    /// Delete permanently, as the official CLI does.
    None,
}

impl TrashMode {
    pub fn parse(s: &str) -> Result<TrashMode, String> {
        match s.trim() {
            "vault" => Ok(TrashMode::Vault),
            "system" => Ok(TrashMode::System),
            "none" => Ok(TrashMode::None),
            _ => Err("unknown trash (use vault, system or none)".into()),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TrashMode::Vault => "vault",
            TrashMode::System => "system",
            TrashMode::None => "none",
        }
    }
}

/// The vault trash folder, at the vault root.
pub const VAULT_TRASH: &str = ".trash";

/// A resolved trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trash {
    Vault,
    System(SystemTrash),
    Delete,
}

/// A system trash on the vault's filesystem: `files` takes the file, and
/// `info` (freedesktop only) a `.trashinfo` record of where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemTrash {
    pub files: PathBuf,
    pub info: Option<PathBuf>,
}

impl Trash {
    /// Resolve a mode for a vault; `Err` says why the system trash cannot
    /// be used here.
    pub fn for_vault(mode: TrashMode, vault: &Path) -> Result<Trash, String> {
        Ok(match mode {
            TrashMode::Vault => Trash::Vault,
            TrashMode::None => Trash::Delete,
            TrashMode::System => Trash::System(system_trash(vault)?),
        })
    }

    pub fn describe(&self) -> String {
        match self {
            Trash::Vault => format!("the vault's {VAULT_TRASH}/ folder"),
            Trash::System(s) => format!("the system trash ({})", s.files.display()),
            Trash::Delete => "nowhere (deleted permanently)".into(),
        }
    }
}

#[cfg(all(unix, not(target_os = "android")))]
fn device_of(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    // The nearest existing ancestor (the trash may not exist yet).
    let mut cur = Some(p);
    while let Some(c) = cur {
        if let Ok(md) = fs::metadata(c) {
            return Some(md.dev());
        }
        cur = c.parent();
    }
    None
}

/// A freedesktop trash at `base` (`files/` and `info/`, made 0700 as needed).
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
fn freedesktop_trash(base: &Path) -> Result<SystemTrash, String> {
    use std::os::unix::fs::DirBuilderExt;
    let mk = |d: &Path| -> Result<(), String> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(d)
            .map_err(|e| format!("cannot create {}: {e}", d.display()))
    };
    mk(&base.join("files"))?;
    mk(&base.join("info"))?;
    Ok(SystemTrash {
        files: base.join("files"),
        info: Some(base.join("info")),
    })
}

/// A per-user trash at the top of a filesystem (`.Trash/$uid` or
/// `.Trash-$uid`): it must be a real folder of user `uid`'s on device
/// `dev` (freedesktop: never a link, never another user's).
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
fn user_trash(base: &Path, uid: u32, dev: u64) -> Result<SystemTrash, String> {
    use std::os::unix::fs::MetadataExt;
    let check = |p: &Path| -> Result<(), String> {
        match fs::symlink_metadata(p) {
            Ok(md) if md.is_dir() && !md.file_type().is_symlink() && md.uid() == uid && md.dev() == dev => Ok(()),
            Ok(_) => Err(format!(
                "{} is not a folder of this user's on the vault's filesystem",
                p.display()
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot check {}: {e}", p.display())),
        }
    };
    check(base)?;
    let t = freedesktop_trash(base)?;
    // Checked again once made, and the folders inside it too.
    check(base)?;
    check(&t.files)?;
    if let Some(i) = &t.info {
        check(i)?;
    }
    Ok(t)
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
fn system_trash(vault: &Path) -> Result<SystemTrash, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let vault = fs::canonicalize(vault).map_err(|e| format!("cannot resolve the vault folder: {e}"))?;
    let vdev = device_of(&vault).ok_or("cannot stat the vault folder")?;
    // The home trash, when the vault is on the same filesystem.
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let mut why = String::new();
    if let Some(d) = data {
        let home_trash = d.join("Trash");
        if device_of(&home_trash) == Some(vdev) {
            return freedesktop_trash(&home_trash);
        }
        why = format!("{} is on another filesystem", home_trash.display());
    }
    // Otherwise the trash at the top of the vault's filesystem.
    let mut top = vault.clone();
    while let Some(p) = top.parent() {
        if device_of(p) != Some(vdev) {
            break;
        }
        top = p.to_path_buf();
    }
    let uid = unsafe { libc::getuid() };
    let shared = top.join(".Trash");
    if let Ok(md) = fs::symlink_metadata(&shared) {
        let sticky = md.permissions().mode() & 0o1000 != 0;
        if md.is_dir() && !md.file_type().is_symlink() && sticky && md.dev() == vdev {
            if let Ok(t) = user_trash(&shared.join(uid.to_string()), uid, vdev) {
                return Ok(t);
            }
        }
    }
    match user_trash(&top.join(format!(".Trash-{uid}")), uid, vdev) {
        Ok(t) => Ok(t),
        Err(e) => Err(format!("no system trash for {}: {why}; {e}", vault.display())),
    }
}

#[cfg(target_os = "macos")]
fn system_trash(vault: &Path) -> Result<SystemTrash, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let t = PathBuf::from(home).join(".Trash");
    if !t.is_dir() {
        return Err(format!("{} does not exist", t.display()));
    }
    let vault = fs::canonicalize(vault).map_err(|e| format!("cannot resolve the vault folder: {e}"))?;
    if device_of(&t) != device_of(&vault) {
        return Err(format!(
            "{} is on another volume than {}; use trash = \"vault\"",
            t.display(),
            vault.display()
        ));
    }
    Ok(SystemTrash { files: t, info: None })
}

#[cfg(any(target_os = "android", not(unix)))]
fn system_trash(_vault: &Path) -> Result<SystemTrash, String> {
    Err("there is no system trash on this platform; use trash = \"vault\"".into())
}

/// `name`, then `stem 1.ext`, `stem 2.ext`, ...
fn numbered(name: &str, n: usize) -> String {
    if n == 0 {
        return name.to_string();
    }
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} {n}{}", &name[..i], &name[i..]),
        _ => format!("{name} {n}"),
    }
}

/// Move `src` to `dst` without replacing anything at `dst`.
///
/// A rename, so the source moves in one step (an editor's atomic save that
/// lands meanwhile is either moved whole or left in place; the caller
/// checks which file moved). The kernel refuses an existing `dst`
/// (`renameat2` with `RENAME_NOREPLACE` on Linux and Android, `renamex_np`
/// with `RENAME_EXCL` on macOS). Where the filesystem or the kernel cannot
/// do that, the existence check before a plain rename only races another
/// writer of the same fresh name in a trash folder.
fn move_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    match rename_noreplace(src, dst) {
        Some(r) => r,
        None => {
            if fs::symlink_metadata(dst).is_ok() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            fs::rename(src, dst)
        }
    }
}

/// An atomic no-replace rename, or `None` when this system cannot do one.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn rename_noreplace(src: &Path, dst: &Path) -> Option<io::Result<()>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let (Ok(s), Ok(d)) = (
        CString::new(src.as_os_str().as_bytes()),
        CString::new(dst.as_os_str().as_bytes()),
    ) else {
        return Some(Err(io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a path")));
    };
    // SAFETY: both are valid NUL-terminated strings for the whole call.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let rc = unsafe {
        // The raw syscall: libc wrappers for renameat2 are missing from
        // older C libraries.
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            s.as_ptr(),
            libc::AT_FDCWD,
            d.as_ptr(),
            1 as libc::c_uint, // RENAME_NOREPLACE
        ) as libc::c_int
    };
    // SAFETY: as above.
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::renamex_np(s.as_ptr(), d.as_ptr(), libc::RENAME_EXCL) };
    if rc == 0 {
        return Some(Ok(()));
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        // No such syscall, a seccomp filter, or a filesystem without the flag.
        Some(libc::ENOSYS | libc::EINVAL | libc::EPERM | libc::ENOTSUP) => None,
        #[allow(unreachable_patterns)]
        Some(libc::EOPNOTSUPP) => None,
        _ => Some(Err(e)),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn rename_noreplace(_src: &Path, _dst: &Path) -> Option<io::Result<()>> {
    None
}

/// Percent-encode a path for a `.trashinfo` file (RFC 2396, `/` kept).
fn trashinfo_path(p: &Path) -> String {
    let mut out = String::new();
    for b in p.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(unix)]
fn local_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(not(unix))]
fn local_timestamp() -> String {
    String::new()
}

/// Move `target` (vault-relative `rel`) into the trash; returns where it went.
fn move_to_trash(root: &Path, rel: &str, target: &Path, trash: &Trash) -> io::Result<PathBuf> {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    match trash {
        Trash::Delete => unreachable!("not a trash"),
        Trash::Vault => {
            let base = root.join(VAULT_TRASH);
            if let Ok(md) = fs::symlink_metadata(&base) {
                if !md.is_dir() || md.file_type().is_symlink() {
                    return Err(io::Error::other(format!(
                        "{} is not a folder; not moving files into it",
                        base.display()
                    )));
                }
            }
            let dir = trash_folders(&base, Path::new(rel).parent())?;
            for n in 0..10_000 {
                let dst = dir.join(numbered(&name, n));
                match move_no_replace(target, &dst) {
                    Ok(()) => return Ok(dst),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(io::Error::other("no free name in the trash"))
        }
        Trash::System(s) => {
            for n in 0..10_000 {
                let fname = numbered(&name, n);
                let dst = s.files.join(&fname);
                let info = match &s.info {
                    Some(dir) => {
                        let p = dir.join(format!("{fname}.trashinfo"));
                        match OpenOptions::new().write(true).create_new(true).open(&p) {
                            Ok(mut f) => {
                                let text = format!(
                                    "[Trash Info]\nPath={}\nDeletionDate={}\n",
                                    trashinfo_path(target),
                                    local_timestamp()
                                );
                                f.write_all(text.as_bytes())?;
                                Some(p)
                            }
                            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    None => None,
                };
                match move_no_replace(target, &dst) {
                    Ok(()) => return Ok(dst),
                    Err(e) => {
                        if let Some(p) = info {
                            let _ = fs::remove_file(p);
                        }
                        if e.kind() == io::ErrorKind::AlreadyExists {
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
            Err(io::Error::other("no free name in the system trash"))
        }
    }
}

/// The folder in the vault trash `base` for a file whose folder in the
/// vault is `parent`, created as needed. A name taken by something that is
/// not a real folder (an earlier trashed file without an extension, a
/// link) is left alone, and ` 1`, ` 2`, ... is added to the folder's name.
fn trash_folders(base: &Path, parent: Option<&Path>) -> io::Result<PathBuf> {
    fs::create_dir_all(base)?;
    let mut dir = base.to_path_buf();
    for seg in parent.into_iter().flat_map(|p| p.iter()) {
        let seg = seg.to_string_lossy();
        let mut found = None;
        for n in 0..10_000 {
            let cand = dir.join(if n == 0 { seg.to_string() } else { format!("{seg} {n}") });
            match fs::symlink_metadata(&cand) {
                Ok(md) if md.is_dir() && !md.file_type().is_symlink() => {
                    found = Some(cand);
                    break;
                }
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir(&cand) {
                    Ok(()) => {
                        found = Some(cand);
                        break;
                    }
                    // Made meanwhile: judge it again.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match fs::symlink_metadata(&cand) {
                        Ok(md) if md.is_dir() && !md.file_type().is_symlink() => {
                            found = Some(cand);
                            break;
                        }
                        _ => continue,
                    },
                    Err(e) => return Err(e),
                },
                Err(e) => return Err(e),
            }
        }
        dir = found.ok_or_else(|| io::Error::other("no free folder name in the trash"))?;
    }
    Ok(dir)
}

/// Drop the `.trashinfo` record of a file moved back out of a system trash.
fn remove_trashinfo(trash: &Trash, moved: &Path) {
    if let (Trash::System(SystemTrash { info: Some(dir), .. }), Some(name)) = (trash, moved.file_name()) {
        let _ = fs::remove_file(dir.join(format!("{}.trashinfo", name.to_string_lossy())));
    }
}

/// [`remove_if_matches`], but into `trash` (unless it says delete). Only a
/// regular file that still hashes to `hash` is touched; empty parent
/// folders are removed afterwards, never `root` itself.
pub fn trash_if_matches(root: &Path, rel: &str, hash: &str, trash: &Trash) -> io::Result<RemoveOutcome> {
    trash_if_matches_with(root, rel, hash, trash, || {})
}

/// `before_move` runs between the check and the move (tests use it to race).
fn trash_if_matches_with(
    root: &Path,
    rel: &str,
    hash: &str,
    trash: &Trash,
    before_move: impl FnOnce(),
) -> io::Result<RemoveOutcome> {
    let target = root.join(rel);
    if *trash == Trash::Delete {
        return remove_if_matches(root, &target, hash);
    }
    let md = match fs::symlink_metadata(&target) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(RemoveOutcome::AlreadyGone),
        Err(e) => return Err(e),
    };
    if !md.is_file() {
        return Ok(RemoveOutcome::Diverged {
            found: format!("not-a-file:{:?}", md.file_type()),
        });
    }
    let mut f = File::open(&target)?;
    let (found, _) = hash_reader(&mut f)?;
    if found != hash {
        return Ok(RemoveOutcome::Diverged { found });
    }
    let ours = f.metadata().ok().and_then(|m| crate::platform::file_id(&m));
    before_move();
    let to = move_to_trash(root, rel, &target, trash)?;
    // Another file took the name between the check and the move (an
    // editor's atomic save): that one moved. Move it back, or hand its
    // content back so the caller puts it back.
    let moved = fs::symlink_metadata(&to)
        .ok()
        .and_then(|m| crate::platform::file_id(&m));
    // Inode numbers are not stable everywhere (FUSE): a moved file with the
    // synced content is the one we judged, or the same content saved anew,
    // and either may go.
    if ours.is_some() && moved.is_some() && ours != moved {
        let content = fs::read(&to)?;
        let found = hash_bytes(&content);
        if found != hash {
            if move_no_replace(&to, &target).is_ok() {
                remove_trashinfo(trash, &to);
                return Ok(RemoveOutcome::Diverged { found });
            }
            return Ok(RemoveOutcome::Raced { displaced: content });
        }
    }
    // An edit through a handle opened before the move lands in the moved
    // file; it is returned so the caller puts it back.
    let (after, content) = hash_reader(&mut f)?;
    prune_empty_dirs(root, &target);
    if after != hash {
        return Ok(RemoveOutcome::Raced { displaced: content });
    }
    Ok(RemoveOutcome::Trashed { to })
}

/// What kind of folder a vault is (spec D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderKind {
    /// It has `.obsidian/`: an Obsidian vault.
    Obsidian,
    /// Any other folder of notes.
    Plain,
}

impl FolderKind {
    pub fn of(root: &Path) -> FolderKind {
        if root.join(".obsidian").is_dir() {
            FolderKind::Obsidian
        } else {
            FolderKind::Plain
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FolderKind::Obsidian => "obsidian vault",
            FolderKind::Plain => "plain folder",
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
    sweep_temp_files_in(root, |rel| !rel.rsplit('/').next().unwrap_or(rel).starts_with('.'))
}

/// [`sweep_temp_files`] in the folders `descend` (given a vault-relative
/// folder) lets it into.
pub fn sweep_temp_files_in(root: &Path, descend: impl Fn(&str) -> bool) -> usize {
    let mut removed = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                let rel = e
                    .path()
                    .strip_prefix(root)
                    .ok()
                    .map(|r| r.to_string_lossy().replace('\\', "/"));
                if rel.is_some_and(|r| descend(&r)) {
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
    fn vault_trash_keeps_folders_and_numbers_clashes() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let put = |rel: &str, c: &[u8]| atomic_write(&root.join(rel), c, 0, &Expect::Absent).unwrap().state;
        let s = put("notes/sub/a.md", b"one");
        put("notes/keep.md", b"keep");
        let out = trash_if_matches(root, "notes/sub/a.md", &s.hash, &Trash::Vault).unwrap();
        assert_eq!(
            out,
            RemoveOutcome::Trashed {
                to: root.join(".trash/notes/sub/a.md")
            }
        );
        assert_eq!(fs::read(root.join(".trash/notes/sub/a.md")).unwrap(), b"one");
        assert!(!root.join("notes/sub").exists(), "the emptied folder is removed");
        assert!(root.join("notes/keep.md").exists(), "a folder holding anything stays");
        // A second deletion of the same path gets a numbered name.
        let s = put("notes/sub/a.md", b"two");
        let out = trash_if_matches(root, "notes/sub/a.md", &s.hash, &Trash::Vault).unwrap();
        assert_eq!(
            out,
            RemoveOutcome::Trashed {
                to: root.join(".trash/notes/sub/a 1.md")
            }
        );
        assert_eq!(fs::read(root.join(".trash/notes/sub/a.md")).unwrap(), b"one");
        // Edited files are never touched.
        let s = put("x.md", b"base");
        fs::write(root.join("x.md"), b"edit").unwrap();
        assert!(matches!(
            trash_if_matches(root, "x.md", &s.hash, &Trash::Vault).unwrap(),
            RemoveOutcome::Diverged { .. }
        ));
        assert_eq!(fs::read(root.join("x.md")).unwrap(), b"edit");
        assert!(root.exists(), "never the root");
        // A `.trash` that is not a folder: refused, the file stays.
        let e = tempfile::tempdir().unwrap();
        let s = atomic_write(&e.path().join("y.md"), b"y", 0, &Expect::Absent)
            .unwrap()
            .state;
        fs::write(e.path().join(".trash"), b"not a folder").unwrap();
        assert!(trash_if_matches(e.path(), "y.md", &s.hash, &Trash::Vault).is_err());
        assert!(e.path().join("y.md").exists());
    }

    #[test]
    fn an_atomic_save_racing_the_trash_move_is_not_lost() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let s = atomic_write(&root.join("n.md"), b"synced", 0, &Expect::Absent)
            .unwrap()
            .state;
        // The editor saves by writing a temp file and renaming it over n.md
        // after our check, before our move.
        let out = trash_if_matches_with(root, "n.md", &s.hash, &Trash::Vault, || {
            fs::write(root.join("tmp-save"), b"new edit").unwrap();
            fs::rename(root.join("tmp-save"), root.join("n.md")).unwrap();
        })
        .unwrap();
        assert!(matches!(out, RemoveOutcome::Diverged { .. }), "{out:?}");
        assert_eq!(fs::read(root.join("n.md")).unwrap(), b"new edit", "moved back");
    }

    #[test]
    fn a_file_in_the_trash_does_not_block_a_folder_of_its_name() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let put = |rel: &str, c: &[u8]| atomic_write(&root.join(rel), c, 0, &Expect::Absent).unwrap().state;
        // An earlier trashed file without an extension, and a link.
        let s = put("notes", b"a file");
        trash_if_matches(root, "notes", &s.hash, &Trash::Vault).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(d.path(), root.join(".trash/linked")).unwrap();
        let s = put("notes/a.md", b"a");
        let out = trash_if_matches(root, "notes/a.md", &s.hash, &Trash::Vault).unwrap();
        assert_eq!(
            out,
            RemoveOutcome::Trashed {
                to: root.join(".trash/notes 1/a.md")
            }
        );
        assert_eq!(fs::read(root.join(".trash/notes")).unwrap(), b"a file");
        #[cfg(unix)]
        {
            let s = put("linked/b.md", b"b");
            let out = trash_if_matches(root, "linked/b.md", &s.hash, &Trash::Vault).unwrap();
            assert_eq!(
                out,
                RemoveOutcome::Trashed {
                    to: root.join(".trash/linked 1/b.md")
                }
            );
        }
    }

    #[test]
    fn the_same_content_saved_anew_before_the_move_is_trashed() {
        // A new inode with the synced content (an atomic save of the same
        // text, or a filesystem without stable inode numbers): it may go.
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let s = atomic_write(&root.join("n.md"), b"synced", 0, &Expect::Absent)
            .unwrap()
            .state;
        let out = trash_if_matches_with(root, "n.md", &s.hash, &Trash::Vault, || {
            fs::write(root.join("tmp-save"), b"synced").unwrap();
            fs::rename(root.join("tmp-save"), root.join("n.md")).unwrap();
        })
        .unwrap();
        assert_eq!(
            out,
            RemoveOutcome::Trashed {
                to: root.join(".trash/n.md")
            }
        );
        assert!(!root.join("n.md").exists());
    }

    #[test]
    fn a_move_never_replaces_its_destination() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a"), b"a").unwrap();
        fs::write(d.path().join("b"), b"b").unwrap();
        let e = move_no_replace(&d.path().join("a"), &d.path().join("b")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(d.path().join("b")).unwrap(), b"b");
        // The kernel's own refusal, not the fallback's check (Linux temp
        // folders support RENAME_NOREPLACE).
        #[cfg(target_os = "linux")]
        assert!(matches!(
            rename_noreplace(&d.path().join("a"), &d.path().join("b")),
            Some(Err(e)) if e.kind() == io::ErrorKind::AlreadyExists
        ));
        move_no_replace(&d.path().join("a"), &d.path().join("c")).unwrap();
        assert_eq!(fs::read(d.path().join("c")).unwrap(), b"a");
        assert!(!d.path().join("a").exists());
    }

    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
    #[test]
    fn a_per_user_trash_must_be_a_real_folder_of_ours() {
        use std::os::unix::fs::MetadataExt;
        let d = tempfile::tempdir().unwrap();
        let md = fs::metadata(d.path()).unwrap();
        let (uid, dev) = (md.uid(), md.dev());
        // Made when missing.
        let t = user_trash(&d.path().join(".Trash-1"), uid, dev).unwrap();
        assert!(t.files.is_dir() && t.info.unwrap().is_dir());
        // A link, even to a folder of ours, is refused.
        fs::create_dir(d.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(d.path().join("elsewhere"), d.path().join(".Trash-2")).unwrap();
        assert!(user_trash(&d.path().join(".Trash-2"), uid, dev).is_err());
        assert!(!d.path().join("elsewhere/files").exists());
        // Another user's folder (as seen with another uid) is refused.
        assert!(user_trash(&d.path().join(".Trash-1"), uid + 1, dev).is_err());
        // So is a file.
        fs::write(d.path().join(".Trash-3"), b"").unwrap();
        assert!(user_trash(&d.path().join(".Trash-3"), uid, dev).is_err());
    }

    #[test]
    fn system_trash_records_where_a_file_came_from() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("vault");
        let t = SystemTrash {
            files: d.path().join("Trash/files"),
            info: Some(d.path().join("Trash/info")),
        };
        fs::create_dir_all(&t.files).unwrap();
        fs::create_dir_all(t.info.as_ref().unwrap()).unwrap();
        let s = atomic_write(&root.join("a b.md"), b"x", 0, &Expect::Absent)
            .unwrap()
            .state;
        let out = trash_if_matches(&root, "a b.md", &s.hash, &Trash::System(t.clone())).unwrap();
        assert_eq!(
            out,
            RemoveOutcome::Trashed {
                to: t.files.join("a b.md")
            }
        );
        let info = fs::read_to_string(t.info.unwrap().join("a b.md.trashinfo")).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath=/"), "{info}");
        assert!(info.contains("a%20b.md\nDeletionDate=20"), "{info}");
        assert!(root.exists(), "the vault root is never removed");
    }

    #[test]
    fn folder_kind() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(FolderKind::of(d.path()), FolderKind::Plain);
        fs::create_dir(d.path().join(".obsidian")).unwrap();
        assert_eq!(FolderKind::of(d.path()), FolderKind::Obsidian);
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
