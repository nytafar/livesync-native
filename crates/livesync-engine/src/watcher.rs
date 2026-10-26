//! The local-change watcher seam (spec "Platform seams", `Watcher`).
//!
//! Watcher events are hints only: they say which paths to look at, and which
//! renames happened, so the push loop does not have to rescan the vault on
//! every save. Correctness comes from comparing the disk with the index; a
//! missed event costs latency until the next periodic rescan, never data.

use std::io;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// Something happened at this path (created, written, closed after
    /// writing, removed, or moved in or out). Absolute path.
    Changed(PathBuf),
    /// A rename seen from both ends (inotify `MOVED_FROM`/`MOVED_TO` with the
    /// same cookie). Absolute paths; may be directories.
    Renamed { from: PathBuf, to: PathBuf },
    /// Events were lost (queue overflow): rescan everything.
    Overflow,
}

pub type WatchSender = UnboundedSender<WatchEvent>;

/// Watches individual directories (not recursively), so the engine decides
/// which folders are watched: never `.obsidian/`, whose `workspace*.json`
/// churns on every UI action.
pub trait Watcher: Send {
    fn watch_dir(&mut self, dir: &Path) -> io::Result<()>;
    fn unwatch_dir(&mut self, dir: &Path);
}

/// A watcher that reports nothing; the push loop then relies on periodic
/// rescans alone.
pub struct NoWatcher;

impl Watcher for NoWatcher {
    fn watch_dir(&mut self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }
    fn unwatch_dir(&mut self, _dir: &Path) {}
}
