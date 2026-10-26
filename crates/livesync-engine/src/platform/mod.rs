//! Platform-specific seam implementations. This is the only module in the
//! engine where `cfg(target_os)` may appear (spec "Platform seams").
//!
//! Today every seam implementation here is portable: the watcher uses the
//! `notify` crate (inotify on Linux and Android, FSEvents on macOS,
//! ReadDirectoryChangesW on Windows).

mod notify_watcher;

use crate::watcher::{WatchSender, Watcher};
use std::io;

/// The platform's default local-change watcher.
pub fn default_watcher(tx: WatchSender) -> io::Result<Box<dyn Watcher>> {
    Ok(Box::new(notify_watcher::NotifyWatcher::new(tx)?))
}

/// A short platform name for the milestone's `node_info.platform`.
pub fn platform_name() -> &'static str {
    std::env::consts::OS
}

/// This machine's host name, for the default device name.
pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|h| h.trim().to_string())
        })
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "device".into())
}
