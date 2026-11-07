//! "No access" against "not there" (spec "Platform seams", `AccessCheck`).
//!
//! A folder this process may not read is never an empty folder, and a file
//! it may not look at is never a deleted one. On macOS a vault under
//! `~/Documents`, `~/Desktop`, `~/Downloads`, iCloud Drive or an external
//! volume needs a privacy (TCC) grant; without it every read fails with
//! `EPERM`, and a launchd-started binary never gets to ask for the grant.
//! The engine then does not start for that vault (`Engine::open`), and while
//! it runs, what it cannot read is reported and never counted as deleted.

use std::io;
use std::path::Path;

/// The full message for a vault folder this process may not read, or `None`
/// when `e` is not a permission error.
pub(crate) fn no_access(vault: &Path, e: &io::Error) -> Option<String> {
    no_access_on(vault, e, crate::platform::PRIVACY_EPERM, &exe())
}

/// [`no_access`] for a platform where `EPERM` is a privacy block
/// (`privacy`) and the binary at `exe`.
pub fn no_access_on(vault: &Path, e: &io::Error, privacy: bool, exe: &str) -> Option<String> {
    if e.kind() != io::ErrorKind::PermissionDenied {
        return None;
    }
    let what = format!(
        "no access to the vault folder {}: nothing is synced until this process can read it, and no file in it counts as deleted",
        vault.display()
    );
    if is_privacy_block(e, privacy) {
        Some(format!(
            "{what}. macOS privacy protection blocks it ({e}): grant Full Disk Access to {exe} (System Settings, Privacy & Security, Full Disk Access), or allow it the folder when macOS asks (Files and Folders), or move the vault out of Documents, Desktop, Downloads, iCloud Drive and external volumes; a running service tries again by itself"
        ))
    } else {
        Some(format!("{what} ({e})"))
    }
}

/// `e` as a short reason for a file or folder that cannot be read, with
/// the way out when it is a macOS privacy block.
pub(crate) fn describe(e: &io::Error) -> String {
    describe_on(e, crate::platform::PRIVACY_EPERM, &exe())
}

pub fn describe_on(e: &io::Error, privacy: bool, exe: &str) -> String {
    if is_privacy_block(e, privacy) {
        format!("{e}: macOS privacy protection; grant Full Disk Access to {exe}, or move the vault")
    } else {
        e.to_string()
    }
}

/// Whether `e` is what a macOS privacy block answers: `EPERM` (an ordinary
/// permission problem is `EACCES`).
pub fn is_privacy_block(e: &io::Error, privacy: bool) -> bool {
    privacy && e.raw_os_error() == Some(libc::EPERM)
}

/// Refuse a vault folder that exists but cannot be listed. A missing one is
/// fine here (it is created); other errors show up where they happen.
pub(crate) fn check_vault_root(vault: &Path) -> Result<(), String> {
    match std::fs::read_dir(vault) {
        Ok(_) => Ok(()),
        Err(e) => match no_access(vault, &e) {
            Some(msg) => Err(msg),
            None => Ok(()),
        },
    }
}

/// Whether only iCloud Drive's placeholder for `path` is there
/// (`.NAME.icloud` beside it): the file was evicted to iCloud ("Optimise Mac
/// Storage"), not deleted. Plain names, so it is checked everywhere.
pub fn icloud_placeholder(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let mut stub = std::ffi::OsString::from(".");
    stub.push(name);
    stub.push(".icloud");
    std::fs::symlink_metadata(dir.join(stub)).is_ok()
}

/// This binary, as the privacy settings list it (the resolved path: a
/// Homebrew link is not what gets the grant).
fn exe() -> String {
    std::env::current_exe()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "lsync".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eperm() -> io::Error {
        io::Error::from_raw_os_error(libc::EPERM)
    }
    fn eacces() -> io::Error {
        io::Error::from_raw_os_error(libc::EACCES)
    }

    #[test]
    fn eperm_on_macos_is_a_privacy_block_with_the_way_out() {
        let vault = Path::new("/Users/u/Documents/Notes");
        let msg = no_access_on(vault, &eperm(), true, "/opt/homebrew/Cellar/lsync/0.3.0/bin/lsync").unwrap();
        assert!(
            msg.contains("no access to the vault folder /Users/u/Documents/Notes"),
            "{msg}"
        );
        assert!(msg.contains("no file in it counts as deleted"), "{msg}");
        assert!(
            msg.contains("Full Disk Access to /opt/homebrew/Cellar/lsync/0.3.0/bin/lsync"),
            "{msg}"
        );
        assert!(msg.contains("move the vault"), "{msg}");
        let short = describe_on(&eperm(), true, "/usr/local/bin/lsync");
        assert!(short.contains("Full Disk Access to /usr/local/bin/lsync"), "{short}");
    }

    #[test]
    fn other_permission_errors_are_no_access_without_the_macos_hint() {
        let vault = Path::new("/srv/notes");
        // EACCES on macOS is an ordinary permission problem.
        let msg = no_access_on(vault, &eacces(), true, "lsync").unwrap();
        assert!(msg.contains("no access to the vault folder /srv/notes"), "{msg}");
        assert!(!msg.contains("Full Disk Access"), "{msg}");
        // EPERM elsewhere (a FUSE layer, a security module) is no privacy block.
        let msg = no_access_on(vault, &eperm(), false, "lsync").unwrap();
        assert!(!msg.contains("Full Disk Access"), "{msg}");
        assert!(!describe_on(&eperm(), false, "lsync").contains("Full Disk Access"));
    }

    #[test]
    fn missing_is_not_no_access() {
        let e = io::Error::from(io::ErrorKind::NotFound);
        assert!(no_access_on(Path::new("/v"), &e, true, "lsync").is_none());
        assert!(!is_privacy_block(&e, true));
    }

    #[test]
    fn an_icloud_placeholder_stands_for_its_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("Note \u{e5}.md");
        assert!(!icloud_placeholder(&f));
        std::fs::write(d.path().join(".Note \u{e5}.md.icloud"), b"stub").unwrap();
        assert!(icloud_placeholder(&f));
        assert!(!icloud_placeholder(&d.path().join("Other.md")));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_vault_root_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let v = d.path().join("v");
        std::fs::create_dir(&v).unwrap();
        assert!(check_vault_root(&v).is_ok());
        assert!(check_vault_root(&d.path().join("not-yet")).is_ok());
        std::fs::set_permissions(&v, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(&v).is_ok(); // root reads anything
        let r = check_vault_root(&v);
        std::fs::set_permissions(&v, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !readable {
            assert!(r.unwrap_err().contains("no access to the vault folder"));
        }
    }
}
