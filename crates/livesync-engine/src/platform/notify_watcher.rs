//! `Watcher` over the `notify` crate, one non-recursive watch per directory.

use crate::watcher::{WatchEvent, WatchSender, Watcher};
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher as _};
use std::io;
use std::path::Path;
use tracing::debug;

pub struct NotifyWatcher {
    inner: notify::RecommendedWatcher,
}

fn map_event(ev: notify::Event, tx: &WatchSender) {
    if ev.need_rescan() {
        let _ = tx.send(WatchEvent::Overflow);
        return;
    }
    match ev.kind {
        // Reads (including our own hashing) are not changes.
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => {}
        EventKind::Access(_) => return,
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if ev.paths.len() == 2 => {
            let _ = tx.send(WatchEvent::Renamed {
                from: ev.paths[0].clone(),
                to: ev.paths[1].clone(),
            });
            return;
        }
        _ => {}
    }
    for p in ev.paths {
        let _ = tx.send(WatchEvent::Changed(p));
    }
}

impl NotifyWatcher {
    pub fn new(tx: WatchSender) -> io::Result<Self> {
        let inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(ev) => map_event(ev, &tx),
            Err(e) => {
                debug!("watcher error: {e}");
                let _ = tx.send(WatchEvent::Overflow);
            }
        })
        .map_err(io::Error::other)?;
        Ok(NotifyWatcher { inner })
    }
}

impl Watcher for NotifyWatcher {
    fn watch_dir(&mut self, dir: &Path) -> io::Result<()> {
        self.inner
            .watch(dir, RecursiveMode::NonRecursive)
            .map_err(io::Error::other)
    }

    fn unwatch_dir(&mut self, dir: &Path) {
        let _ = self.inner.unwatch(dir);
    }
}
