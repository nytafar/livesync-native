//! What a running engine is doing, which revisions it stored and applied,
//! and waiting for a condition on it (spec D7: `lsync wait`, `lsync flush`,
//! the service's events and the status bars).
//!
//! - **Revisions.** Every revision the index takes as a file's synced
//!   version is an [`EngineEvent`] once it is committed: `Stored` when this
//!   device wrote it to the server, `Applied` when a revision from the
//!   server is in the local file (written, adopted, a deletion carried out).
//! - **Activity.** The pull and push sides publish what they are doing
//!   ([`Activity`]); [`Engine::idle`] says whether nothing is pending or
//!   settling, and if not, what is.
//! - **Waiting.** [`Engine::wait_for`] blocks until a condition holds on
//!   this engine, it cannot hold without a person, the engine stops, or a
//!   deadline passes. The events only wake it: every answer is read from
//!   the index and the activity, so a missed event never makes it wrong.

use crate::engine::{meta_keys as meta, Engine, EngineError};
use crate::index::{FileRow, Origin, RevisionRow, State};
use livesync_remote::DocFetch;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

type Result<T> = std::result::Result<T, EngineError>;

/// Events kept for a subscriber that is slow to read; one that falls
/// further behind is told so (`Lagged`) and reads the state instead.
pub(crate) const EVENT_BUFFER: usize = 1024;

/// How long the engine must stay idle before [`Engine::wait_for`] calls it
/// idle: a save the watcher has not reported yet is not missed, and a pull
/// between two batches does not count.
pub const IDLE_GRACE: Duration = Duration::from_millis(600);

/// How often a wait looks again without an event.
const WAIT_TICK: Duration = Duration::from_millis(250);

/// One file revision the index took as the file's synced version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RevisionEvent {
    pub path: String,
    pub doc_id: String,
    pub rev: String,
    pub deleted: bool,
}

/// What an engine tells its subscribers ([`Engine::subscribe`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// This device wrote the revision to the server (a push, a deletion).
    Stored(RevisionEvent),
    /// A revision from the server is in the local file now: written,
    /// adopted as identical, a deletion carried out; or a push found the
    /// server holding exactly this content already.
    Applied(RevisionEvent),
}

impl EngineEvent {
    pub(crate) fn of(r: &RevisionRow) -> EngineEvent {
        let e = RevisionEvent {
            path: r.path.clone(),
            doc_id: r.doc_id.clone(),
            rev: r.rev.clone(),
            deleted: r.deleted,
        };
        match r.origin {
            Origin::Local => EngineEvent::Stored(e),
            Origin::Remote | Origin::Migrated => EngineEvent::Applied(e),
        }
    }

    pub fn revision(&self) -> &RevisionEvent {
        match self {
            EngineEvent::Stored(e) | EngineEvent::Applied(e) => e,
        }
    }
}

/// What the pull side is doing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullPhase {
    /// No daemon runs on this engine.
    #[default]
    Stopped,
    /// Connecting: the gate.
    Starting,
    /// Reading what changed on the server since the last pull.
    CatchingUp,
    /// Caught up, following the server's changes as they come.
    Live,
    /// Applying a batch of changes (or retrying pending documents).
    Applying,
    /// Waiting before it tries again: the server cannot be reached, or a
    /// gate refused (`last_error` says which).
    Retrying,
}

impl PullPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            PullPhase::Stopped => "stopped",
            PullPhase::Starting => "starting",
            PullPhase::CatchingUp => "catching-up",
            PullPhase::Live => "live",
            PullPhase::Applying => "applying",
            PullPhase::Retrying => "retrying",
        }
    }

    /// Whether the server answers (the pull side is past its gate).
    pub fn connected(self) -> bool {
        matches!(self, PullPhase::CatchingUp | PullPhase::Live | PullPhase::Applying)
    }
}

/// What a running engine is doing ([`Engine::activity`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Activity {
    /// A daemon (two-way or pull-only) runs on this engine.
    pub running: bool,
    /// ... and it ran and ended: this engine will not sync again.
    pub ended: bool,
    pub two_way: bool,
    pub pull: PullPhase,
    /// Pushes are allowed (the gate passed, an owed fetch and the
    /// registration are done). Always false for a pull-only daemon.
    pub pushes: bool,
    /// Local paths that changed and wait for their writes to settle.
    pub settling: u64,
    /// A push pass is running.
    pub pushing: bool,
    /// A push pass is due (a full rescan, or a retry after a failed pass).
    pub push_due: bool,
    /// Deletions held until their burst is over.
    pub deletes_waiting: u64,
    /// `flush` requests waiting for their changes to be stored.
    pub flushes: u64,
}

/// Whether nothing is pending or settling ([`Engine::idle`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Idle {
    pub idle: bool,
    /// What is still going on, for people (empty when idle).
    pub busy: Vec<String>,
    /// Documents the pull side will fetch again.
    pub pending: u64,
}

/// What a wait wants of a local file's revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "rev")]
pub enum Target {
    /// This revision (or a later one) is in the local file.
    Rev(String),
    /// The server deleted the file: it is gone here too.
    Deleted,
    /// The server has no such file yet: any revision of it arrives.
    Any,
}

/// A condition [`Engine::wait_for`] waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitFor {
    /// Nothing is pending or settling, for [`IDLE_GRACE`].
    Idle,
    /// A revision of a file (vault-relative) is in the local file.
    Applied { path: String, target: Target },
}

/// A condition that holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reached {
    /// `--applied`: the file, the revision asked for, and the one the local
    /// file is at now (later than asked when another edit followed).
    pub path: Option<String>,
    pub rev: Option<String>,
    pub current_rev: Option<String>,
    /// The file is deleted here, as on the server.
    pub deleted: bool,
    /// A later revision is in the local file (`current_rev`).
    pub superseded: bool,
}

/// How a wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    Reached(Reached),
    /// It cannot happen without a person: why.
    Blocked(String),
    /// The engine's daemon ended; the next engine may get there.
    Stopped,
    /// The deadline passed: what was still outstanding.
    TimedOut(String),
}

/// One check of a condition.
enum Check {
    Holds(Reached),
    Blocked(String),
    NotYet(String),
}

fn generation(rev: &str) -> Option<u64> {
    rev.split_once('-').and_then(|(g, _)| g.parse().ok())
}

impl Engine {
    /// Every revision the index takes as a file's synced version from now
    /// on ([`EngineEvent`]).
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.inner.events.subscribe()
    }

    /// What the engine is doing now.
    pub fn activity(&self) -> Activity {
        let mut a = self.inner.activity.borrow().clone();
        a.pushes = a.two_way && self.inner.push_enabled.load(Ordering::SeqCst);
        a.deletes_waiting = self.inner.deletes_waiting.load(Ordering::SeqCst);
        a
    }

    /// Changes to [`Engine::activity`] (as the two sides publish them).
    pub fn watch_activity(&self) -> tokio::sync::watch::Receiver<Activity> {
        self.inner.activity.subscribe()
    }

    pub(crate) fn set_activity(&self, f: impl FnOnce(&mut Activity)) {
        self.inner.activity.send_if_modified(|a| {
            let before = a.clone();
            f(a);
            *a != before
        });
    }

    pub(crate) fn set_pull_phase(&self, p: PullPhase) {
        self.set_activity(|a| a.pull = p);
    }

    /// Whether nothing is pending or settling: the daemon runs and follows
    /// the server live, no local change waits to be pushed or is being
    /// pushed, no deletion is held and no document waits to be fetched
    /// again. Reads the index's pending count.
    pub fn idle(&self) -> Result<Idle> {
        let a = self.activity();
        let pending = self.with_index(|ix| ix.pending(None))?.len() as u64;
        let mut busy = Vec::new();
        if !a.running {
            busy.push("the engine is not running".to_string());
        } else {
            match a.pull {
                PullPhase::Live => {}
                PullPhase::Stopped | PullPhase::Starting => busy.push("connecting to the server".into()),
                PullPhase::CatchingUp => busy.push("catching up with the server".into()),
                PullPhase::Applying => busy.push("applying changes from the server".into()),
                PullPhase::Retrying => busy.push("waiting to retry the server (see last_error)".into()),
            }
            if a.two_way {
                if !a.pushes {
                    busy.push("pushes wait for the gate, an owed full fetch or the registration".into());
                }
                if a.settling > 0 {
                    busy.push(format!("{} local change(s) settling", a.settling));
                }
                if a.pushing {
                    busy.push("pushing local changes".into());
                } else if a.push_due {
                    busy.push("a push pass is due".into());
                }
                if a.deletes_waiting > 0 {
                    busy.push(format!(
                        "{} deletion(s) held until their burst is over",
                        a.deletes_waiting
                    ));
                }
            }
        }
        if pending > 0 {
            busy.push(format!("{pending} document(s) from the server to fetch again"));
        }
        Ok(Idle {
            idle: busy.is_empty(),
            busy,
            pending,
        })
    }

    /// What a wait for `rev` of `path` (vault-relative) wants: that
    /// revision when given, else the server's current revision of the file,
    /// read now (`Deleted` when the server deleted it, `Any` when it has no
    /// such file).
    pub async fn target_of(&self, path: &str, rev: Option<&str>) -> Result<Target> {
        if let Some(r) = rev {
            return Ok(Target::Rev(r.to_string()));
        }
        let id = self.doc_id_for(path);
        Ok(match self.inner.couch.fetch_doc(&id, false).await? {
            // A soft deletion (the plugin's `deleted: true`) is a revision
            // of its own: the wait is for it.
            DocFetch::Found(d) => match d.get("_rev").and_then(|r| r.as_str()) {
                Some(r) => Target::Rev(r.to_string()),
                None => Target::Any,
            },
            DocFetch::Deleted => Target::Deleted,
            DocFetch::Missing(_) => Target::Any,
        })
    }

    /// The stops that keep the pull side (`pulls`) or everything from
    /// moving until a person acts.
    fn blocking_stop(&self, pulls: bool) -> Result<Option<String>> {
        let a = self.alerts()?;
        let text = |s: String| s.split_once(' ').map_or(s.clone(), |(_, m)| m.to_string());
        if let Some(s) = a.upstream_stop {
            return Ok(Some(format!("upstream stop: {}", text(s))));
        }
        if let Some(s) = a.remote_locked {
            return Ok(Some(format!("remote locked: {}", text(s))));
        }
        if !pulls {
            if let Some(s) = a.safety_stop {
                return Ok(Some(format!(
                    "safety stop: {} (`lsync status`; `lsync allow-deletes`)",
                    text(s)
                )));
            }
        }
        Ok(None)
    }

    /// Whether a revision of `path` is in the local file.
    fn check_applied(&self, path: &str, target: &Target) -> Result<Check> {
        let id = self.doc_id_for(path);
        let (row, seen): (Option<FileRow>, Option<RevisionRow>) = self.with_index(|ix| {
            let row = match ix.by_doc(&id)? {
                Some(r) => Some(r),
                None => ix.by_path(path)?,
            };
            let seen = match target {
                Target::Rev(r) => ix.revision(&id, r)?,
                _ => None,
            };
            Ok((row, seen))
        })?;
        let exists = self.inner.cfg.vault.join(path).exists();
        let current = row.as_ref().and_then(|r| r.base_rev.clone());
        let tracked = row
            .as_ref()
            .is_some_and(|r| matches!(r.state, State::Clean | State::Dirty | State::Conflicted));
        let held = |r: &FileRow| -> Option<String> {
            match r.state {
                State::Dirty => Some(format!(
                    "{path} has a local edit, so the server's change was not applied over it ({}); see `lsync status`",
                    r.note.as_deref().unwrap_or("dirty")
                )),
                State::Conflicted => Some(format!(
                    "{path} is in conflict ({}); see `lsync status`",
                    r.note.as_deref().unwrap_or("conflicted")
                )),
                State::Skipped => Some(format!(
                    "{path} is not synced here ({})",
                    r.note.as_deref().unwrap_or("skipped")
                )),
                State::OutOfScope => Some(format!("{path} was moved out of the synced folders here")),
                State::Clean => None,
            }
        };
        let holds = |rev: Option<String>, superseded: bool| {
            Check::Holds(Reached {
                path: Some(path.to_string()),
                rev,
                current_rev: current.clone(),
                deleted: false,
                superseded,
            })
        };
        Ok(match target {
            Target::Rev(want) => {
                if current.as_deref() == Some(want.as_str()) {
                    holds(Some(want.clone()), false)
                } else if seen.as_ref().is_some_and(|s| s.applied) {
                    // It was in the local file, and a later one is now.
                    if seen.as_ref().is_some_and(|s| s.deleted) && !exists {
                        Check::Holds(Reached {
                            path: Some(path.to_string()),
                            rev: Some(want.clone()),
                            current_rev: None,
                            deleted: true,
                            superseded: false,
                        })
                    } else {
                        holds(Some(want.clone()), true)
                    }
                } else if let Some(why) = seen.as_ref().and(row.as_ref()).and_then(held) {
                    // Seen, and held back: a local edit, a conflict, a skip.
                    Check::Blocked(why)
                } else if row.as_ref().is_some_and(|r| r.state == State::Clean)
                    && current.as_deref().and_then(generation) > generation(want)
                {
                    // Not seen here: a later revision came first (the changes
                    // feed gives only the newest).
                    holds(Some(want.clone()), true)
                } else {
                    Check::NotYet(format!("revision {want} of {path} has not arrived"))
                }
            }
            Target::Deleted => {
                if !tracked && !exists {
                    Check::Holds(Reached {
                        path: Some(path.to_string()),
                        rev: None,
                        current_rev: None,
                        deleted: true,
                        superseded: false,
                    })
                } else if let Some(why) = row.as_ref().and_then(held) {
                    Check::Blocked(why)
                } else {
                    Check::NotYet(format!("the server's deletion of {path} has not arrived"))
                }
            }
            Target::Any => match &row {
                Some(r) if r.base_rev.is_some() && exists && r.state == State::Clean => holds(current.clone(), false),
                Some(r) if r.base_rev.is_some() && exists => match held(r) {
                    Some(why) => Check::Blocked(why),
                    None => holds(current.clone(), false),
                },
                _ => Check::NotYet(format!("{path} has not arrived")),
            },
        })
    }

    /// Wait until `cond` holds on this engine (see the module docs). A
    /// stop that keeps it from holding ends the wait at once (`Blocked`), as
    /// does a revision held back by a local edit or a conflict.
    pub async fn wait_for(&self, cond: &WaitFor, deadline: Instant) -> Result<WaitOutcome> {
        let mut events = self.subscribe();
        let mut activity = self.watch_activity();
        let mut idle_since: Option<Instant> = None;
        loop {
            let a = self.activity();
            if a.ended {
                return Ok(WaitOutcome::Stopped);
            }
            let check = match cond {
                WaitFor::Idle => {
                    if let Some(stop) = self.blocking_stop(false)? {
                        Check::Blocked(stop)
                    } else {
                        let idle = self.idle()?;
                        if idle.idle {
                            let since = *idle_since.get_or_insert_with(Instant::now);
                            if since.elapsed() >= IDLE_GRACE {
                                Check::Holds(Reached::default())
                            } else {
                                Check::NotYet("settling".into())
                            }
                        } else {
                            idle_since = None;
                            Check::NotYet(idle.busy.join("; "))
                        }
                    }
                }
                WaitFor::Applied { path, target } => match self.check_applied(path, target)? {
                    Check::NotYet(why) => match self.blocking_stop(true)? {
                        Some(stop) => Check::Blocked(stop),
                        None => Check::NotYet(why),
                    },
                    other => other,
                },
            };
            let why = match check {
                Check::Holds(r) => return Ok(WaitOutcome::Reached(r)),
                Check::Blocked(why) => return Ok(WaitOutcome::Blocked(why)),
                Check::NotYet(why) => why,
            };
            let now = Instant::now();
            if now >= deadline {
                return Ok(WaitOutcome::TimedOut(why));
            }
            let tick = WAIT_TICK.min(deadline - now);
            tokio::select! {
                _ = events.recv() => {}
                _ = activity.changed() => {}
                _ = tokio::time::sleep(tick) => {}
            }
        }
    }

    /// The safety stop shown, if any (for flush and wait reports).
    pub(crate) fn safety_stop_shown(&self) -> Result<Option<String>> {
        self.with_index(|ix| ix.get_meta(meta::SAFETY_STOP))
    }

    /// Open conflicts: the paths in conflict, at most `limit`.
    pub fn open_conflicts(&self, limit: usize) -> Result<Vec<String>> {
        Ok(self
            .with_index(|ix| ix.list_state(State::Conflicted, limit))?
            .into_iter()
            .map(|r| r.path)
            .collect())
    }
}

/// Marks the daemon as ended (and the pull side stopped) when dropped.
pub(crate) struct Running(pub(crate) Engine);

impl Running {
    pub(crate) fn start(engine: &Engine, two_way: bool) -> Running {
        engine.set_activity(|a| {
            a.running = true;
            a.ended = false;
            a.two_way = two_way;
            a.pull = PullPhase::Starting;
        });
        Running(engine.clone())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.0.set_activity(|a| {
            a.running = false;
            a.ended = true;
            a.pull = PullPhase::Stopped;
            a.settling = 0;
            a.pushing = false;
            a.push_due = false;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_compare() {
        assert_eq!(generation("3-abc"), Some(3));
        assert_eq!(generation("x"), None);
        assert!(Some(4) > generation("3-a"));
        assert!(generation("nope") < Some(1), "an unreadable revision is never later");
    }

    #[test]
    fn local_revisions_are_stored_and_remote_ones_applied() {
        let mut r = RevisionRow::tombstone("a.md", "2-x", "a.md", true, 0);
        r.origin = Origin::Local;
        assert!(matches!(EngineEvent::of(&r), EngineEvent::Stored(e) if e.rev == "2-x" && e.deleted));
        r.origin = Origin::Remote;
        assert!(matches!(EngineEvent::of(&r), EngineEvent::Applied(e) if e.path == "a.md"));
    }
}
