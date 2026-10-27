//! `Watcher` over the `notify` crate, one non-recursive watch per directory.
//!
//! Events carry the paths the engine watched, not the paths the kernel
//! resolved: FSEvents (macOS) reports real paths (`/private/var/...` for a
//! vault under `/var/...`), so a watched directory whose canonical path
//! differs is remembered and reported paths are mapped back to it.

use crate::watcher::{WatchEvent, WatchSender, Watcher};
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher as _};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::debug;

/// (canonical path, path as watched) for watched directories where they differ.
type Aliases = Arc<Mutex<Vec<(PathBuf, PathBuf)>>>;

pub struct NotifyWatcher {
    inner: notify::RecommendedWatcher,
    aliases: Aliases,
}

/// `p` under the path the engine watched, when the backend reported it under
/// the canonical one. The longest canonical prefix wins.
fn unalias(p: PathBuf, aliases: &[(PathBuf, PathBuf)]) -> PathBuf {
    if aliases.iter().any(|(_, given)| p.starts_with(given)) {
        return p;
    }
    let best = aliases
        .iter()
        .filter(|(real, _)| p.starts_with(real))
        .max_by_key(|(real, _)| real.as_os_str().len());
    match best {
        Some((real, given)) => match p.strip_prefix(real) {
            Ok(rest) if rest.as_os_str().is_empty() => given.clone(),
            Ok(rest) => given.join(rest),
            Err(_) => p,
        },
        None => p,
    }
}

fn map_event(ev: notify::Event, tx: &WatchSender, aliases: &Aliases) {
    if ev.need_rescan() {
        let _ = tx.send(WatchEvent::Overflow);
        return;
    }
    let paths: Vec<PathBuf> = {
        let a = aliases.lock().unwrap_or_else(|e| e.into_inner());
        ev.paths.into_iter().map(|p| unalias(p, &a)).collect()
    };
    match ev.kind {
        // Reads (including our own hashing) are not changes.
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => {}
        EventKind::Access(_) => return,
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() == 2 => {
            let _ = tx.send(WatchEvent::Renamed {
                from: paths[0].clone(),
                to: paths[1].clone(),
            });
            return;
        }
        _ => {}
    }
    for p in paths {
        let _ = tx.send(WatchEvent::Changed(p));
    }
}

impl NotifyWatcher {
    pub fn new(tx: WatchSender) -> io::Result<Self> {
        let aliases: Aliases = Arc::default();
        let cb_aliases = aliases.clone();
        let inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(ev) => map_event(ev, &tx, &cb_aliases),
            Err(e) => {
                debug!("watcher error: {e}");
                let _ = tx.send(WatchEvent::Overflow);
            }
        })
        .map_err(io::Error::other)?;
        Ok(NotifyWatcher { inner, aliases })
    }
}

impl Watcher for NotifyWatcher {
    fn watch_dir(&mut self, dir: &Path) -> io::Result<()> {
        if let Ok(real) = std::fs::canonicalize(dir) {
            if real != dir {
                let mut a = self.aliases.lock().unwrap_or_else(|e| e.into_inner());
                if !a.iter().any(|(_, given)| given == dir) {
                    a.push((real, dir.to_path_buf()));
                }
            }
        }
        self.inner
            .watch(dir, RecursiveMode::NonRecursive)
            .map_err(io::Error::other)
    }

    fn unwatch_dir(&mut self, dir: &Path) {
        let _ = self.inner.unwatch(dir);
        self.aliases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, given)| given != dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn unalias_maps_canonical_paths_back() {
        let a = vec![
            (PathBuf::from("/private/var/v"), PathBuf::from("/var/v")),
            (PathBuf::from("/private/var/v/sub"), PathBuf::from("/var/v/sub")),
        ];
        assert_eq!(unalias("/private/var/v/n.md".into(), &a), PathBuf::from("/var/v/n.md"));
        assert_eq!(unalias("/private/var/v".into(), &a), PathBuf::from("/var/v"));
        assert_eq!(
            unalias("/private/var/v/sub/x".into(), &a),
            PathBuf::from("/var/v/sub/x")
        );
        assert_eq!(unalias("/var/v/n.md".into(), &a), PathBuf::from("/var/v/n.md"));
        assert_eq!(unalias("/elsewhere/n.md".into(), &a), PathBuf::from("/elsewhere/n.md"));
    }

    /// Events name files under the directory as it was given, also through a
    /// symlink (and under macOS's `/var` -> `/private/var` temp dir).
    #[test]
    fn events_use_the_watched_path() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = d.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        let link = real.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut w = NotifyWatcher::new(tx).unwrap();
        w.watch_dir(&link).unwrap();
        // The FSEvents stream starts on its own thread; let it come up.
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(link.join("n.md"), b"x").unwrap();
        let want = link.join("n.md");
        let until = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while Instant::now() < until {
            match rx.try_recv() {
                Ok(WatchEvent::Changed(p)) if p == want => return,
                Ok(ev) => seen.push(ev),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        panic!("no event for {}; saw {seen:?}", want.display());
    }
}
