//! `Watcher` over the `notify` crate, one non-recursive watch per directory.
//!
//! Events carry the paths the engine watched, not the paths the kernel
//! resolved: FSEvents (macOS) reports real paths (`/private/var/...` for a
//! vault under `/var/...`), so a watched directory whose canonical path
//! differs is remembered and reported paths are mapped back to it.
//!
//! FSEvents reports the two names of a rename as two separate events
//! (`RenameMode::Any`), with nothing that ties them together, where inotify
//! pairs them by cookie (`RenameMode::Both`). [`RenamePairer`] pairs them: a
//! half whose path is gone, followed within the coalescing window by a half
//! whose path exists, is one rename, unless the file's identity (device and
//! inode), where it was seen at the old name, says otherwise. Each half is
//! still reported as a change at once; the pair only adds the rename, so the
//! push side matches the old document with the new name instead of holding
//! its deletion as part of a burst.

use crate::watcher::{WatchEvent, WatchSender, Watcher};
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher as _};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::debug;

/// How long the old name of a rename waits for its new one. FSEvents
/// delivers both halves in one callback; this is the coalescing window.
pub(crate) const RENAME_WINDOW: Duration = Duration::from_millis(100);

/// File identities remembered for pairing, at most.
const KNOWN_IDS: usize = 4096;

/// A file's identity (device, inode).
type FileId = (u64, u64);

/// Pairs the separate halves of FSEvents renames (see the module docs).
#[derive(Debug, Default)]
pub(crate) struct RenamePairer {
    /// Old names (gone when reported), newest last, with when they came and
    /// the identity last seen there.
    gone: VecDeque<(PathBuf, Instant, Option<FileId>)>,
    /// The identity last seen at a path, for paths that had events.
    ids: HashMap<PathBuf, FileId>,
    order: VecDeque<PathBuf>,
}

impl RenamePairer {
    /// One half of a rename at `path`, seen at `now`; `id` is what is at
    /// `path` now (`None`: nothing). Returns the rename it completes.
    pub(crate) fn half(&mut self, path: PathBuf, now: Instant, id: Option<FileId>) -> Option<(PathBuf, PathBuf)> {
        self.gone
            .retain(|(_, at, _)| now.saturating_duration_since(*at) <= RENAME_WINDOW);
        let Some(id) = id else {
            let was = self.ids.get(&path).copied();
            self.gone.retain(|(p, _, _)| *p != path);
            self.gone.push_back((path, now, was));
            return None;
        };
        self.seen(&path, id);
        // The newest old name that may be this file: the same identity, or
        // one never seen (unknown).
        let i = self
            .gone
            .iter()
            .rposition(|(p, _, was)| *p != path && was.is_none_or(|w| w == id))?;
        let (from, _, _) = self.gone.remove(i)?;
        self.ids.remove(&from);
        Some((from, path))
    }

    /// Remember the identity at `path` (any event for an existing path).
    pub(crate) fn seen(&mut self, path: &Path, id: FileId) {
        if self.ids.insert(path.to_path_buf(), id).is_none() {
            self.order.push_back(path.to_path_buf());
            while self.order.len() > KNOWN_IDS {
                if let Some(old) = self.order.pop_front() {
                    self.ids.remove(&old);
                }
            }
        }
    }
}

fn identity(p: &Path) -> Option<FileId> {
    std::fs::symlink_metadata(p).ok().as_ref().and_then(super::file_id)
}

type Pairer = Arc<Mutex<RenamePairer>>;

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

/// Whether this platform's backend reports renames as separate halves
/// (FSEvents), so file identities are worth remembering for pairing them.
const HALF_RENAMES: bool = cfg!(target_os = "macos");

fn map_event(ev: notify::Event, tx: &WatchSender, aliases: &Aliases, pairer: &Pairer, track_ids: bool) {
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
        // One half of an FSEvents rename: a change now, and the rename once
        // both halves are in.
        EventKind::Modify(ModifyKind::Name(RenameMode::Any)) if paths.len() == 1 => {
            let p = paths[0].clone();
            let id = identity(&p);
            let pair = pairer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .half(p.clone(), Instant::now(), id);
            let _ = tx.send(WatchEvent::Changed(p));
            if let Some((from, to)) = pair {
                let _ = tx.send(WatchEvent::Renamed { from, to });
            }
            return;
        }
        // Creations and writes: remember what file is there, so a later
        // rename half can be checked against it.
        EventKind::Create(_) | EventKind::Modify(_) if track_ids => {
            let mut pr = pairer.lock().unwrap_or_else(|e| e.into_inner());
            for p in &paths {
                if let Some(id) = identity(p) {
                    pr.seen(p, id);
                }
            }
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
        let pairer: Pairer = Arc::default();
        let inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(ev) => map_event(ev, &tx, &cb_aliases, &pairer, HALF_RENAMES),
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

    fn rename_half(p: &Path) -> notify::Event {
        notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Any))).add_path(p.to_path_buf())
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<WatchEvent>) -> Vec<WatchEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn the_halves_of_a_rename_are_paired() {
        let t = Instant::now();
        let mut p = RenamePairer::default();
        assert_eq!(p.half("/v/old.md".into(), t, None), None);
        assert_eq!(
            p.half("/v/new.md".into(), t + Duration::from_millis(5), Some((1, 7))),
            Some(("/v/old.md".into(), "/v/new.md".into()))
        );
        // Each old name pairs once.
        assert_eq!(
            p.half("/v/other.md".into(), t + Duration::from_millis(6), Some((1, 8))),
            None
        );
    }

    #[test]
    fn halves_too_far_apart_are_not_a_rename() {
        let t = Instant::now();
        let mut p = RenamePairer::default();
        p.half("/v/old.md".into(), t, None);
        assert_eq!(p.half("/v/new.md".into(), t + RENAME_WINDOW * 3, Some((1, 7))), None);
    }

    #[test]
    fn a_known_identity_must_match() {
        let t = Instant::now();
        let ms = Duration::from_millis;
        let mut p = RenamePairer::default();
        p.seen(Path::new("/v/a.md"), (1, 10));
        p.seen(Path::new("/v/b.md"), (1, 20));
        // a.md went to the Finder's Trash (outside the watched folders),
        // then b.md was renamed to c.md: c.md is b.md, never a.md.
        p.half("/v/a.md".into(), t, None);
        p.half("/v/b.md".into(), t + ms(1), None);
        assert_eq!(
            p.half("/v/c.md".into(), t + ms(2), Some((1, 20))),
            Some(("/v/b.md".into(), "/v/c.md".into()))
        );
        // Nor is a new file with another identity.
        assert_eq!(p.half("/v/d.md".into(), t + ms(3), Some((1, 30))), None);
        // An atomic save: the temp file, seen when it was written, becomes
        // the note.
        p.seen(Path::new("/v/.note.md.tmp"), (1, 40));
        p.half("/v/.note.md.tmp".into(), t + ms(4), None);
        assert_eq!(
            p.half("/v/note.md".into(), t + ms(5), Some((1, 40))),
            Some(("/v/.note.md.tmp".into(), "/v/note.md".into()))
        );
    }

    #[test]
    fn halves_that_both_exist_are_not_a_rename() {
        let t = Instant::now();
        let mut p = RenamePairer::default();
        assert_eq!(p.half("/v/a.md".into(), t, Some((1, 1))), None);
        assert_eq!(p.half("/v/b.md".into(), t, Some((1, 2))), None);
    }

    /// The mapping as FSEvents feeds it: each half is a change at once, and
    /// the second also reports the rename. Real files, so identities come
    /// from the disk; NFD names pass through as they are.
    #[test]
    fn fsevents_halves_become_a_rename() {
        let d = tempfile::tempdir().unwrap();
        let (old, new) = (d.path().join("cafe\u{301}.md"), d.path().join("Caf\u{e9} 2.md"));
        std::fs::write(&old, b"x").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (aliases, pairer): (Aliases, Pairer) = Default::default();
        let created = notify::Event::new(EventKind::Create(notify::event::CreateKind::File)).add_path(old.clone());
        map_event(created, &tx, &aliases, &pairer, true);
        std::fs::rename(&old, &new).unwrap();
        map_event(rename_half(&old), &tx, &aliases, &pairer, true);
        map_event(rename_half(&new), &tx, &aliases, &pairer, true);
        assert_eq!(
            drain(&mut rx),
            [
                WatchEvent::Changed(old.clone()),
                WatchEvent::Changed(old.clone()),
                WatchEvent::Changed(new.clone()),
                WatchEvent::Renamed { from: old, to: new },
            ]
        );
        // inotify's paired renames are passed on as they are.
        let both = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path("/v/a".into())
            .add_path("/v/b".into());
        map_event(both, &tx, &aliases, &pairer, false);
        assert_eq!(
            drain(&mut rx),
            [WatchEvent::Renamed {
                from: "/v/a".into(),
                to: "/v/b".into()
            }]
        );
    }

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
