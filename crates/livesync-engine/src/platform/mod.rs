//! Platform-specific seam implementations. This is the only module in the
//! engine where `cfg(target_os)` may appear (spec "Platform seams").
//!
//! The watcher uses the `notify` crate (inotify on Linux and Android, FSEvents
//! on macOS, ReadDirectoryChangesW on Windows). The profile lock is `flock`
//! through `std::fs::File::try_lock` for the state directory, and for the
//! vault folder `flock` plus a host lock (`vault_lock`), because flock fails
//! on Android's shared storage.

mod lock;
mod notify_watcher;
mod vault_lock;

pub use lock::{read_holder, Holder, LockError, LockFile};
pub use vault_lock::{lock_vault, record_path, VaultLock, VaultLockError};

use crate::watcher::{WatchSender, Watcher};
use std::io;

/// The platform's default local-change watcher.
pub fn default_watcher(tx: WatchSender) -> io::Result<Box<dyn Watcher>> {
    Ok(Box::new(notify_watcher::NotifyWatcher::new(tx)?))
}

/// The native change-notification API behind `default_watcher`, for reports.
pub fn watcher_backend() -> &'static str {
    if cfg!(any(target_os = "linux", target_os = "android")) {
        "inotify"
    } else if cfg!(target_os = "macos") {
        "FSEvents"
    } else if cfg!(windows) {
        "ReadDirectoryChangesW"
    } else {
        "kqueue"
    }
}

/// A short platform name for the milestone's `node_info.platform`.
pub fn platform_name() -> &'static str {
    std::env::consts::OS
}

/// This process's user ID, where the platform has one.
pub fn current_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        Some(unsafe { libc::getuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// A short name for this process, for lock holder records: the first word
/// of `/proc/self/cmdline` where there is one (the package name of an
/// Android app, which runs as `app_process`), else the executable's name.
pub fn process_name() -> String {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        if let Ok(cmd) = std::fs::read("/proc/self/cmdline") {
            let first = cmd.split(|&b| b == 0).next().unwrap_or_default();
            let first = String::from_utf8_lossy(first);
            if let Some(name) = first.rsplit('/').next().filter(|n| !n.is_empty()) {
                return name.to_string();
            }
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// This machine's host name, for the default device name.
pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(system_hostname)
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "device".into())
}

#[cfg(unix)]
fn system_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for `buf.len()` bytes; gethostname writes at
    // most that many and the result is cut at the first NUL below.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

#[cfg(not(unix))]
fn system_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

/// Peak resident set size of this process in KiB, where the platform reports it.
pub fn peak_rss_kib() -> Option<u64> {
    #[cfg(unix)]
    {
        // SAFETY: getrusage fills the zeroed struct it is given.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
            return None;
        }
        let max = u64::try_from(ru.ru_maxrss).ok()?;
        // Linux and Android report KiB; macOS reports bytes.
        if cfg!(target_vendor = "apple") {
            Some(max / 1024)
        } else {
            Some(max)
        }
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// A file's identity (device, inode) where the platform has one, so two
/// names can be recognised as the same file. `None` elsewhere.
pub fn file_id(md: &std::fs::Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((md.dev(), md.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        None
    }
}
