//! `flush` (spec D7): push local changes now, and answer once they are
//! stored on the server.
//!
//! A flush is a request to the running two-way daemon's push loop
//! ([`Engine::flush`]), which owns the settle queue and the delete hold:
//!
//! - **Paths.** They skip the settle wait and go out in a pass at once. A
//!   folder stands for the files under it. Each path is answered once it is
//!   settled: stored (with the revision now on the server), deleted, stored
//!   as a conflicting branch, or not storable (not synced, failed, held by
//!   the safety stop). A deletion is decided with the burst it is part of
//!   (the delete hold), so its answer can take the hold's quiet period.
//! - **Everything.** A full pass runs at once, without deferring files that
//!   were written moments ago; the flush is answered after a full pass that
//!   began after it found nothing still being written and no deletion held.
//!   The answer lists every revision this flush stored.
//!
//! A gate's stop (the upstream guard, a remote lock) answers at once:
//! nothing would be pushed until a person acts. Anything else (the network,
//! an owed fetch) is waited out; the caller bounds the wait.

use crate::engine::{meta_keys as meta, Engine, EngineError};
use crate::index::{FileRow, LocalIssue, State};
use crate::push::Found;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use tokio::sync::{broadcast, oneshot};

type Result<T> = std::result::Result<T, EngineError>;

/// What became of one path a flush covered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlushedPath {
    pub path: String,
    pub doc_id: String,
    /// The revision on the server: the file's, or its deletion's.
    pub rev: Option<String>,
    /// `stored`, `deleted`, `conflict` (stored as a conflicting branch),
    /// `not-synced` (the filters or the folder scope leave it out),
    /// `failed` (a local issue: too large, unreadable, a failed push),
    /// `held` (a deletion the safety stop holds), `missing` (no such file,
    /// and none known) or, from a one-shot flush, `pending` (written to
    /// while it ran).
    pub outcome: String,
    /// This flush wrote it (else it was on the server already).
    pub pushed: bool,
    pub note: Option<String>,
}

impl FlushedPath {
    /// On the server as it is here.
    pub fn stored(&self) -> bool {
        matches!(self.outcome.as_str(), "stored" | "deleted")
    }
}

/// The answer to a flush.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlushReport {
    /// Everything pending was flushed (no paths given).
    pub all: bool,
    /// The paths given (folders as their files), or with `all`, every
    /// revision this flush stored.
    pub paths: Vec<FlushedPath>,
    /// Everything asked for is on the server as it is here.
    pub complete: bool,
    /// The mass-delete safety stop, when one is shown.
    pub safety_stop: Option<String>,
    /// With `all`: local files that cannot be synced.
    pub issues: Vec<LocalIssue>,
    /// With `all`: files in conflict (stored, as a branch).
    pub conflicts: Vec<String>,
}

/// How a flush ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushOutcome {
    Done(FlushReport),
    /// Nothing can be pushed until a person acts: why.
    Blocked(String),
    /// The daemon ended before the flush was answered.
    Stopped,
}

/// A flush waiting in the push loop.
pub(crate) struct FlushWaiter {
    /// `None`: everything.
    pub(crate) paths: Option<Vec<String>>,
    pub(crate) reply: oneshot::Sender<Answer>,
}

pub(crate) enum Answer {
    Done(Vec<FlushedPath>),
    Blocked(String),
}

/// A flush the push loop took in, with when.
struct Taken {
    w: FlushWaiter,
    /// Passes completed before it.
    passes: u64,
    full_passes: u64,
}

/// The push loop's side of flushing.
#[derive(Default)]
pub(crate) struct Flushes {
    waiting: Vec<Taken>,
    /// Paths to push in the next pass, without settling.
    pub(crate) now: BTreeSet<String>,
    passes: u64,
    full_passes: u64,
    /// The last full pass found files still being written.
    full_deferred: bool,
}

impl Flushes {
    /// The flushes still waiting; one whose caller gave up is dropped.
    pub(crate) fn len(&mut self) -> usize {
        self.waiting.retain(|t| !t.w.reply.is_closed());
        self.waiting.len()
    }

    /// A full pass must not defer files written moments ago.
    pub(crate) fn wants_all(&self) -> bool {
        self.waiting.iter().any(|t| t.w.paths.is_none())
    }

    /// Take in new requests: their paths skip the settle queue; a flush of
    /// everything makes a full pass due. True when there were any.
    pub(crate) fn take(
        &mut self,
        engine: &Engine,
        queue: &mut HashMap<String, impl Sized>,
        full_due: &mut bool,
    ) -> bool {
        let new: Vec<FlushWaiter> = std::mem::take(&mut *engine.inner.flushes.lock().unwrap());
        if new.is_empty() {
            return false;
        }
        for w in new {
            match &w.paths {
                Some(paths) => {
                    for p in paths {
                        queue.remove(p);
                        self.now.insert(p.clone());
                    }
                }
                None => *full_due = true,
            }
            self.waiting.push(Taken {
                w,
                passes: self.passes,
                full_passes: self.full_passes,
            });
        }
        true
    }

    /// A pass completed.
    pub(crate) fn passed(&mut self, full: bool, deferred: bool) {
        self.passes += 1;
        if full {
            self.full_passes += 1;
            self.full_deferred = deferred;
            // A full pass covered every path waiting to go out now.
            self.now.clear();
        }
    }

    /// Answer every flush that is done. `held`: deletions the hold keeps
    /// undecided. Returns true when a flush of everything still waits for a
    /// full pass (to run one soon).
    pub(crate) fn answer(&mut self, engine: &Engine, held: u64) -> Result<bool> {
        let mut again_full = false;
        let mut keep = Vec::new();
        for t in std::mem::take(&mut self.waiting) {
            if t.w.reply.is_closed() {
                continue;
            }
            match &t.w.paths {
                None => {
                    if self.full_passes > t.full_passes && !self.full_deferred && held == 0 {
                        let _ = t.w.reply.send(Answer::Done(vec![]));
                    } else {
                        again_full |= self.full_passes > t.full_passes && self.full_deferred;
                        keep.push(t);
                    }
                }
                Some(paths) => {
                    if self.passes <= t.passes {
                        keep.push(t);
                        continue;
                    }
                    let mut done = Vec::new();
                    let mut open = false;
                    for p in paths {
                        match engine.flushed_path(p)? {
                            Some(f) => done.push(f),
                            None => {
                                open = true;
                                // Still to go out (it was being written):
                                // the next pass takes it; a deletion waits
                                // for the hold.
                                if engine.inner.cfg.vault.join(p).is_file() {
                                    self.now.insert(p.clone());
                                }
                            }
                        }
                    }
                    if open {
                        keep.push(t);
                    } else {
                        let _ = t.w.reply.send(Answer::Done(done));
                    }
                }
            }
        }
        self.waiting = keep;
        Ok(again_full)
    }

    /// Pushes are held: a gate's stop answers every flush at once.
    pub(crate) fn held(&mut self, engine: &Engine) -> Result<()> {
        if self.waiting.is_empty() {
            return Ok(());
        }
        if let Some(stop) = engine.gate_stop()? {
            for t in self.waiting.drain(..) {
                let _ = t.w.reply.send(Answer::Blocked(stop.clone()));
            }
            self.now.clear();
        }
        Ok(())
    }
}

impl Engine {
    /// A gate's stop, which keeps everything from being pushed.
    fn gate_stop(&self) -> Result<Option<String>> {
        let a = self.alerts()?;
        let text = |s: String| s.split_once(' ').map_or(s.clone(), |(_, m)| m.to_string());
        Ok(a.upstream_stop
            .map(|s| format!("upstream stop: {}", text(s)))
            .or_else(|| a.remote_locked.map(|s| format!("remote locked: {}", text(s)))))
    }

    /// The paths a flush of `given` covers: a folder stands for the files
    /// under it and the tracked files that were in it.
    fn flush_paths(&self, given: &[String]) -> Result<Vec<String>> {
        let mut out: BTreeSet<String> = BTreeSet::new();
        for p in given {
            let p = p.trim_matches('/').to_string();
            if self.inner.cfg.vault.join(&p).is_dir() {
                for f in self.walk_visible(&p).found {
                    out.insert(f.rel);
                }
                for r in self.with_index(|ix| ix.rows_under(&p))? {
                    if r.path != p {
                        out.insert(r.path);
                    }
                }
            } else {
                out.insert(p);
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Push local changes now and answer once they are stored (see the
    /// module docs). `None`: everything pending. Needs the two-way daemon
    /// running on this engine (`Engine::run`); the caller bounds the wait.
    pub async fn flush(&self, paths: Option<Vec<String>>) -> Result<FlushOutcome> {
        let a = self.activity();
        if a.ended {
            return Ok(FlushOutcome::Stopped);
        }
        if a.running && !a.two_way {
            return Err(EngineError::Other(
                "this vault is pull-only and never writes to the server".into(),
            ));
        }
        if let Some(stop) = self.gate_stop()? {
            return Ok(FlushOutcome::Blocked(stop));
        }
        let paths = match paths {
            Some(p) => Some(self.flush_paths(&p)?),
            None => None,
        };
        let mut events = self.subscribe();
        let (tx, rx) = oneshot::channel();
        self.inner.flushes.lock().unwrap().push(FlushWaiter {
            paths: paths.clone(),
            reply: tx,
        });
        self.inner.flush_wake.notify_one();
        let mut activity = self.watch_activity();
        let answer = tokio::select! {
            a = rx => match a {
                Ok(a) => a,
                Err(_) => return Ok(FlushOutcome::Stopped),
            },
            // The daemon ended before the push loop took it in.
            _ = activity.wait_for(|a| a.ended) => return Ok(FlushOutcome::Stopped),
        };
        let mut done = match answer {
            Answer::Blocked(why) => return Ok(FlushOutcome::Blocked(why)),
            Answer::Done(d) => d,
        };
        // What this flush stored: every event is in the channel by now (they
        // are sent at the commit, before the answer).
        let mut stored: BTreeMap<String, crate::activity::RevisionEvent> = BTreeMap::new();
        loop {
            match events.try_recv() {
                Ok(crate::activity::EngineEvent::Stored(e)) => {
                    stored.insert(e.path.clone(), e);
                }
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        Ok(FlushOutcome::Done(self.flush_report(
            paths.is_none(),
            &mut done,
            stored,
        )?))
    }

    /// A flush without a daemon (no service runs the vault): one two-way
    /// sync (gate, pull, push everything, as `sync_once`), then what became
    /// of the paths given (or every revision it stored). A path still on
    /// its way after the sync (written to while it ran) is `pending`.
    pub async fn flush_once(&self, paths: Option<Vec<String>>) -> Result<FlushReport> {
        let paths = match paths {
            Some(p) => Some(self.flush_paths(&p)?),
            None => None,
        };
        let mut events = self.subscribe();
        self.sync_once().await?;
        let mut stored: BTreeMap<String, crate::activity::RevisionEvent> = BTreeMap::new();
        loop {
            match events.try_recv() {
                Ok(crate::activity::EngineEvent::Stored(e)) => {
                    stored.insert(e.path.clone(), e);
                }
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        let mut done = Vec::new();
        for p in paths.iter().flatten() {
            done.push(match self.flushed_path(p)? {
                Some(f) => f,
                None => FlushedPath {
                    path: p.clone(),
                    doc_id: self.doc_id_for(p),
                    outcome: "pending".into(),
                    note: Some("changed while the sync ran; flush again".into()),
                    ..Default::default()
                },
            });
        }
        self.flush_report(paths.is_none(), &mut done, stored)
    }

    /// The report of a flush (`done`: the paths given, as answered), with
    /// what it `stored`.
    pub(crate) fn flush_report(
        &self,
        all: bool,
        done: &mut Vec<FlushedPath>,
        stored: BTreeMap<String, crate::activity::RevisionEvent>,
    ) -> Result<FlushReport> {
        let safety_stop = self
            .safety_stop_shown()?
            .map(|s| s.split_once(' ').map_or(s.clone(), |(_, m)| m.to_string()));
        if all {
            let paths: Vec<FlushedPath> = stored
                .into_values()
                .map(|e| FlushedPath {
                    path: e.path,
                    doc_id: e.doc_id,
                    rev: Some(e.rev),
                    outcome: if e.deleted { "deleted" } else { "stored" }.into(),
                    pushed: true,
                    note: None,
                })
                .collect();
            let issues = self.with_index(|ix| ix.issues())?;
            let conflicts = self.open_conflicts(100)?;
            return Ok(FlushReport {
                all,
                complete: safety_stop.is_none() && issues.is_empty(),
                paths,
                safety_stop,
                issues,
                conflicts,
            });
        }
        for f in done.iter_mut() {
            f.pushed = stored.get(&f.path).is_some_and(|e| Some(&e.rev) == f.rev.as_ref());
        }
        Ok(FlushReport {
            all,
            complete: done.iter().all(FlushedPath::stored),
            paths: std::mem::take(done),
            safety_stop,
            issues: vec![],
            conflicts: vec![],
        })
    }

    /// What became of a path after the passes so far, or `None` while it is
    /// still on its way (being written, or a deletion not decided yet).
    pub(crate) fn flushed_path(&self, rel: &str) -> Result<Option<FlushedPath>> {
        let doc_id = self.doc_id_for(rel);
        let out = |outcome: &str, rev: Option<String>, note: Option<String>| {
            Some(FlushedPath {
                path: rel.to_string(),
                doc_id: doc_id.clone(),
                rev,
                outcome: outcome.into(),
                pushed: false,
                note,
            })
        };
        let row: Option<FileRow> = self.row_for_local(rel)?;
        let issue = self.with_index(|ix| ix.issues())?.into_iter().find(|i| i.path == rel);
        let md = std::fs::symlink_metadata(self.inner.cfg.vault.join(rel));
        match md {
            Ok(md) if md.is_file() => {
                if !self.local_visible(rel) {
                    return Ok(out("not-synced", None, Some(self.explain(rel).detail)));
                }
                let found = Found {
                    rel: rel.to_string(),
                    size: md.len(),
                    mtime_ms: crate::push::mtime_ms(&md),
                };
                // Stored as it is here (an issue is then only a note, as a
                // file stored with replacement characters has); else an
                // issue is why it was not.
                let note = issue.as_ref().map(|i| i.note.clone());
                Ok(match row {
                    Some(r) if self.unchanged(&r, &found) => match r.state {
                        State::Clean => out("stored", r.base_rev, note),
                        State::Conflicted => out("conflict", r.base_rev, r.note.or(note)),
                        State::Skipped | State::OutOfScope => out("not-synced", None, r.note),
                        State::Dirty => note.and_then(|n| out("failed", None, Some(n))),
                    },
                    _ => note.and_then(|n| out("failed", None, Some(n))),
                })
            }
            Ok(_) => Ok(out("failed", None, Some("not a file".into()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let tracked = row
                    .as_ref()
                    .is_some_and(|r| matches!(r.state, State::Clean | State::Dirty | State::Conflicted));
                if tracked {
                    let stop = self.safety_stop_shown()?;
                    let held = self
                        .with_index(|ix| ix.get_meta(meta::SAFETY_STOP_DOCS))?
                        .and_then(|d| serde_json::from_str::<HashSet<String>>(&d).ok())
                        .map_or(stop.is_some(), |docs| docs.contains(&doc_id));
                    return Ok(match stop {
                        Some(s) if held => out(
                            "held",
                            None,
                            Some(s.split_once(' ').map_or(s.clone(), |(_, m)| m.to_string())),
                        ),
                        _ => None,
                    });
                }
                if row.as_ref().is_some_and(|r| r.state == State::OutOfScope) {
                    return Ok(out("not-synced", None, row.and_then(|r| r.note)));
                }
                // Gone here and not tracked: the deletion this device stored,
                // or the one it applied.
                let last = self
                    .with_index(|ix| ix.revisions_at(rel))?
                    .into_iter()
                    .rev()
                    .find(|r| r.applied);
                Ok(match last {
                    Some(r) if r.deleted => out("deleted", Some(r.rev), None),
                    // Its last revision was a live one: the file left
                    // without a deletion (a rename, a move out of scope).
                    Some(_) => out("deleted", None, None),
                    None => out("missing", None, Some("no such file here, and none known".into())),
                })
            }
            Err(e) => Ok(out("failed", None, Some(format!("cannot read: {e}")))),
        }
    }

    /// The flushes waiting in the push loop, answered with `Stopped` when
    /// it ends (their channels close).
    pub(crate) fn drop_flushes(&self) {
        self.inner.flushes.lock().unwrap().clear();
    }
}
