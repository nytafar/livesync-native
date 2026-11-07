//! Desktop notifications (spec D7, opt-in): the service's events that need
//! a person, each announced once per change of state, never again while it
//! holds.
//!
//! - **Stops:** the mass-delete safety stop, the upstream stop and a remote
//!   lock without a rebuild. Announced when one starts or its reason
//!   changes; announced again only after it was lifted and came back.
//! - **Conflicts:** files newly in conflict. A file is announced once; it is
//!   announced again only after it was resolved and fell into conflict anew.
//! - **A vault that keeps failing:** its engine ended with an error (the
//!   supervisor retries), or it has reported errors for
//!   [`KEEPS_FAILING`] without getting through to the server in between.
//!   Announced once per such episode; the episode ends when the vault is
//!   connected again (or paused, or stopped). A vault that keeps crashing
//!   right after it connects is announced at most once per
//!   [`FAILING_AGAIN`].
//!
//! What a vault shows when the notifier first hears of it (at the start, or
//! when a reload adds it) is not news. When events were missed, the
//! service's status is read and what changed meanwhile is announced.
//!
//! What is shown comes from [`Watch`], which only reads events, so tests feed
//! it events and read its notices. A [`Notifier`] shows them: `notify-send`
//! on Linux, `osascript` on macOS, or a test's fake.

use crate::protocol::{event, Event};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// How long a vault must keep reporting errors, without reaching the server
/// in between, before it is announced as failing.
pub const KEEPS_FAILING: Duration = Duration::from_secs(5 * 60);

/// A vault that failed is announced again only this long after the last
/// time, even if it connected in between (a crash loop).
pub const FAILING_AGAIN: Duration = Duration::from_secs(30 * 60);

/// Paths named in one conflict notice.
const NAMED: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Normal,
    Critical,
}

/// One notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub vault: Option<String>,
    /// `safety-stop`, `upstream-stop`, `remote-locked`, `conflict` or
    /// `failing`.
    pub kind: &'static str,
    pub title: String,
    pub body: String,
    pub urgency: Urgency,
}

/// Shows notices.
pub trait Notifier: Send + Sync {
    fn notify(&self, n: &Notice);
}

/// What [`Watch`] knows about one vault.
#[derive(Debug, Default)]
struct VaultSeen {
    /// The stops last announced, by event kind (none while lifted).
    stops: HashMap<&'static str, String>,
    /// Files in conflict, as last announced.
    conflicts: BTreeSet<String>,
    /// When the current run of errors began (ms), if one is going on.
    failing_since: Option<i64>,
    /// This episode of failing was announced.
    failing_told: bool,
    /// When failing was last announced (ms).
    failing_told_at: Option<i64>,
}

impl VaultSeen {
    /// Whether failing may be announced now (`ts`), and if so, mark it so.
    fn tell_failing(&mut self, ts: i64) -> bool {
        if self.failing_told {
            return false;
        }
        self.failing_told = true;
        if self
            .failing_told_at
            .is_some_and(|t| ts - t < FAILING_AGAIN.as_millis() as i64)
        {
            return false;
        }
        self.failing_told_at = Some(ts);
        true
    }
}

/// Turns the service's events into notices, once per change of state.
#[derive(Debug, Default)]
pub struct Watch {
    vaults: HashMap<String, VaultSeen>,
    /// The vaults whose state at first sight was taken as known.
    seeded: BTreeSet<String>,
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string).filter(|s| !s.is_empty())
}

impl Watch {
    pub fn new() -> Watch {
        Watch::default()
    }

    /// A watch that knows the state the service shows now: the stops and
    /// the files in conflict when it starts are not news.
    pub fn knowing(st: &crate::protocol::StatusResult) -> Watch {
        let mut w = Watch::new();
        w.catch_up(st);
        w
    }

    /// Whether this vault's state at first sight was taken in (a vault the
    /// notifier has not seen yet wants [`Watch::catch_up`] first).
    pub fn knows(&self, vault: &str) -> bool {
        self.seeded.contains(vault)
    }

    /// Bring the watch up to the status the service shows now: for a vault
    /// it knows, announce what changed (after missed events); a vault it
    /// sees for the first time (the start, a reload added it) is taken as
    /// it is, without notices.
    pub fn catch_up(&mut self, st: &crate::protocol::StatusResult) -> Vec<Notice> {
        let mut out = Vec::new();
        for v in &st.vaults {
            let Some(s) = &v.status else { continue };
            let known = self.seeded.contains(&v.name);
            self.seeded.insert(v.name.clone());
            let ev = |kind: &str, data: Value| Event {
                v: crate::protocol::PROTOCOL,
                event: kind.into(),
                vault: Some(v.name.clone()),
                ts: crate::protocol::now_ms(),
                data,
            };
            // As the service's events give them: the guards' stops without
            // their time, the safety stop as recorded.
            let text = |x: &Option<String>| {
                x.as_deref()
                    .map(|t| t.split_once(' ').map_or(t, |(_, m)| m).to_string())
            };
            let open: Vec<&str> = s.conflicted.iter().map(|r| r.path.as_str()).collect();
            for e in [
                ev(event::SAFETY_STOP, serde_json::json!({ "stop": s.safety_stop })),
                ev(
                    event::UPSTREAM_STOP,
                    serde_json::json!({ "stop": text(&s.upstream_stop) }),
                ),
                ev(
                    event::REMOTE_LOCKED,
                    serde_json::json!({ "lock": text(&s.remote_locked) }),
                ),
                ev(event::CONFLICT, serde_json::json!({ "open": open })),
            ] {
                let told = self.on_event(&e);
                if known {
                    out.extend(told);
                }
            }
        }
        out
    }

    /// The notices an event calls for (often none).
    pub fn on_event(&mut self, e: &Event) -> Vec<Notice> {
        let Some(name) = e.vault.clone() else {
            return vec![];
        };
        let v = self.vaults.entry(name.clone()).or_default();
        let notice = |kind: &'static str, title: String, body: String, urgency: Urgency| Notice {
            vault: Some(name.clone()),
            kind,
            title,
            body,
            urgency,
        };
        match e.event.as_str() {
            k @ (event::SAFETY_STOP | event::UPSTREAM_STOP | event::REMOTE_LOCKED) => {
                let kind: &'static str = match k {
                    event::SAFETY_STOP => event::SAFETY_STOP,
                    event::UPSTREAM_STOP => event::UPSTREAM_STOP,
                    _ => event::REMOTE_LOCKED,
                };
                let field = if kind == event::REMOTE_LOCKED { "lock" } else { "stop" };
                match text(&e.data[field]) {
                    None => {
                        v.stops.remove(kind);
                        vec![]
                    }
                    Some(t) if v.stops.get(kind) == Some(&t) => vec![],
                    Some(t) => {
                        v.stops.insert(kind, t.clone());
                        let (title, hint) = match kind {
                            event::SAFETY_STOP => (
                                format!("lsync: {name} stopped deleting"),
                                "\nRestore the files, or confirm with `lsync allow-deletes`.",
                            ),
                            event::UPSTREAM_STOP => (format!("lsync: {name} stopped syncing"), ""),
                            _ => (format!("lsync: {name} is locked by another device"), ""),
                        };
                        // The safety stop comes as recorded, `<ms> <text>`.
                        let shown = t
                            .split_once(' ')
                            .filter(|(ms, _)| ms.len() >= 10 && ms.bytes().all(|b| b.is_ascii_digit()))
                            .map_or(t.as_str(), |(_, m)| m);
                        vec![notice(kind, title, format!("{shown}{hint}"), Urgency::Critical)]
                    }
                }
            }
            event::CONFLICT => {
                // The files in conflict now; an older service sends counts only.
                let Some(open) = e.data["open"].as_array() else {
                    return vec![];
                };
                let open: BTreeSet<String> = open.iter().filter_map(|p| p.as_str().map(str::to_string)).collect();
                let new: Vec<&String> = open.difference(&v.conflicts).collect();
                let out = if new.is_empty() {
                    vec![]
                } else {
                    let mut names: Vec<String> = new.iter().take(NAMED).map(|p| p.to_string()).collect();
                    if new.len() > NAMED {
                        names.push(format!("and {} more", new.len() - NAMED));
                    }
                    let title = if new.len() == 1 {
                        format!("lsync: a conflict in {name}")
                    } else {
                        format!("lsync: {} conflicts in {name}", new.len())
                    };
                    vec![notice(
                        event::CONFLICT,
                        title,
                        format!("{}\nSee `lsync status`.", names.join(", ")),
                        Urgency::Normal,
                    )]
                };
                v.conflicts = open;
                out
            }
            event::ERROR => {
                let since = *v.failing_since.get_or_insert(e.ts);
                if !v.failing_told && e.ts - since >= KEEPS_FAILING.as_millis() as i64 && v.tell_failing(e.ts) {
                    let msg = text(&e.data["message"]).unwrap_or_default();
                    let mins = (e.ts - since) / 60_000;
                    return vec![notice(
                        "failing",
                        format!("lsync: {name} keeps failing"),
                        format!("For {mins} minutes now. The last error: {msg}"),
                        Urgency::Critical,
                    )];
                }
                vec![]
            }
            event::STATE => match e.data["state"].as_str() {
                Some("failed") => {
                    v.failing_since.get_or_insert(e.ts);
                    if !v.tell_failing(e.ts) {
                        return vec![];
                    }
                    let msg = text(&e.data["message"]).unwrap_or_else(|| "it ended with an error".into());
                    vec![notice(
                        "failing",
                        format!("lsync: {name} stopped working"),
                        format!("{msg}\nIt is retried by itself; `lsync status --all` shows it."),
                        Urgency::Critical,
                    )]
                }
                Some("stopped") => {
                    v.failing_since = None;
                    v.failing_told = false;
                    vec![]
                }
                _ => vec![],
            },
            // Through to the server again: an episode of failing is over.
            event::ACTIVITY if e.data["connected"].as_bool() == Some(true) => {
                v.failing_since = None;
                v.failing_told = false;
                vec![]
            }
            event::PULLED | event::PUSHED => {
                v.failing_since = None;
                v.failing_told = false;
                vec![]
            }
            event::PAUSED if e.data["paused"].as_bool() == Some(true) => {
                v.failing_since = None;
                v.failing_told = false;
                vec![]
            }
            _ => vec![],
        }
    }
}

/// Reads the service's status (blocking), for [`spawn`].
pub type StatusReader = Arc<dyn Fn() -> Option<crate::protocol::StatusResult> + Send + Sync>;

/// Feed the service's events to a [`Watch`] and show its notices, until the
/// events end. `status` is read when events were missed (what changed is
/// announced) and when a vault is first heard of (what it shows is not
/// news). Notifiers and status reads run on a blocking thread.
pub fn spawn(
    mut events: broadcast::Receiver<Event>,
    mut watch: Watch,
    notifier: Arc<dyn Notifier>,
    status: StatusReader,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let read = |status: &StatusReader| {
            let s = status.clone();
            async move { tokio::task::spawn_blocking(move || s()).await.ok().flatten() }
        };
        loop {
            let notices = match events.recv().await {
                Ok(e) => {
                    let mut notices = Vec::new();
                    if e.vault.as_ref().is_some_and(|v| !watch.knows(v)) {
                        if let Some(st) = read(&status).await {
                            notices.extend(watch.catch_up(&st));
                        }
                    }
                    notices.extend(watch.on_event(&e));
                    notices
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!("notifications: {n} events missed; reading the status");
                    // The status is newer than anything still buffered:
                    // skip the backlog, or its old events are announced again.
                    events = events.resubscribe();
                    match read(&status).await {
                        Some(st) => watch.catch_up(&st),
                        None => vec![],
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            };
            if notices.is_empty() {
                continue;
            }
            let n = notifier.clone();
            let _ = tokio::task::spawn_blocking(move || {
                for x in &notices {
                    n.notify(x);
                }
            })
            .await;
        }
    })
}

/// The desktop's own notifications: `notify-send` (freedesktop, over the
/// session bus) on Linux, `osascript` on macOS.
#[derive(Debug, Default)]
pub struct Desktop {
    warned: std::sync::atomic::AtomicBool,
}

impl Desktop {
    pub fn new() -> Desktop {
        Desktop::default()
    }

    /// The command that shows `n`.
    pub fn command(n: &Notice) -> std::process::Command {
        if cfg!(target_os = "macos") {
            let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
            let mut c = std::process::Command::new("osascript");
            c.arg("-e").arg(format!(
                "display notification {} with title {}",
                quote(&n.body),
                quote(&n.title)
            ));
            c
        } else {
            let mut c = std::process::Command::new("notify-send");
            c.arg("--app-name=lsync")
                .arg(match n.urgency {
                    Urgency::Critical => "--urgency=critical",
                    Urgency::Normal => "--urgency=normal",
                })
                .arg("--")
                .arg(&n.title)
                .arg(&n.body);
            c
        }
    }
}

/// Run a notifier's command; warn once when it fails.
fn run(mut c: std::process::Command, n: &Notice, warned: &std::sync::atomic::AtomicBool) {
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match c.status() {
        Ok(s) if s.success() => debug!("notified: {}", n.title),
        other => {
            // Once: a desktop without a notification daemon says so every
            // time otherwise.
            if !warned.swap(true, std::sync::atomic::Ordering::SeqCst) {
                warn!(
                    "cannot show a notification ({:?}: {other:?}); is a notification daemon running in this session?",
                    c.get_program()
                );
            }
        }
    }
}

impl Notifier for Desktop {
    fn notify(&self, n: &Notice) {
        run(Desktop::command(n), n, &self.warned);
    }
}

/// A program of the user's choice instead of the desktop's notifications
/// (`lsync serve --notify-command`): run as `PROGRAM -- TITLE BODY` (the
/// `--` because the body may start with a file name such as `-u.md`, which
/// an option parser like `notify-send`'s would take for an option), with
/// `LSYNC_NOTIFY_KIND` (`safety-stop`, `upstream-stop`, `remote-locked`,
/// `conflict`, `failing`), `LSYNC_NOTIFY_VAULT` and `LSYNC_NOTIFY_URGENCY`
/// (`normal`, `critical`) set.
#[derive(Debug)]
pub struct Program {
    pub program: std::path::PathBuf,
    warned: std::sync::atomic::AtomicBool,
}

impl Program {
    pub fn new(program: std::path::PathBuf) -> Program {
        Program {
            program,
            warned: Default::default(),
        }
    }

    pub fn command(&self, n: &Notice) -> std::process::Command {
        let mut c = std::process::Command::new(&self.program);
        c.arg("--")
            .arg(&n.title)
            .arg(&n.body)
            .env("LSYNC_NOTIFY_KIND", n.kind)
            .env("LSYNC_NOTIFY_VAULT", n.vault.as_deref().unwrap_or(""))
            .env(
                "LSYNC_NOTIFY_URGENCY",
                match n.urgency {
                    Urgency::Critical => "critical",
                    Urgency::Normal => "normal",
                },
            );
        c
    }
}

impl Notifier for Program {
    fn notify(&self, n: &Notice) {
        run(self.command(n), n, &self.warned);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PROTOCOL;
    use serde_json::json;
    use std::sync::Mutex;

    fn ev(kind: &str, vault: &str, ts: i64, data: Value) -> Event {
        Event {
            v: PROTOCOL,
            event: kind.into(),
            vault: Some(vault.into()),
            ts,
            data,
        }
    }

    fn kinds(n: &[Notice]) -> Vec<&'static str> {
        n.iter().map(|n| n.kind).collect()
    }

    #[test]
    fn a_stop_is_told_once_until_it_is_lifted_or_its_reason_changes() {
        let mut w = Watch::new();
        let stop = |t: Option<&str>| ev(event::SAFETY_STOP, "notes", 1, json!({ "stop": t }));
        let first = w.on_event(&stop(Some("1730000000000 30 files went missing")));
        assert!(first[0].body.starts_with("30 files went missing"), "{}", first[0].body);
        assert!(w.on_event(&stop(None)).is_empty());
        let first = w.on_event(&stop(Some("30 files went missing")));
        assert_eq!(kinds(&first), ["safety-stop"]);
        assert_eq!(first[0].urgency, Urgency::Critical);
        assert!(first[0].title.contains("notes") && first[0].body.contains("30 files"));
        assert!(first[0].body.contains("allow-deletes"), "{}", first[0].body);
        // The same stop again (a restart, a repeated event): nothing.
        assert!(w.on_event(&stop(Some("30 files went missing"))).is_empty());
        // Its reason changes: told.
        assert_eq!(
            kinds(&w.on_event(&stop(Some("31 files went missing")))),
            ["safety-stop"]
        );
        // Lifted, then back: told again.
        assert!(w.on_event(&stop(None)).is_empty());
        assert_eq!(
            kinds(&w.on_event(&stop(Some("31 files went missing")))),
            ["safety-stop"]
        );
        // Each vault and each kind on its own.
        let other = ev(event::SAFETY_STOP, "work", 1, json!({"stop": "31 files went missing"}));
        assert_eq!(kinds(&w.on_event(&other)), ["safety-stop"]);
        let up = ev(event::UPSTREAM_STOP, "notes", 1, json!({"stop": "a newer format"}));
        assert_eq!(kinds(&w.on_event(&up)), ["upstream-stop"]);
        assert!(w.on_event(&up).is_empty());
        let lock = ev(event::REMOTE_LOCKED, "notes", 1, json!({"lock": "locked by the phone"}));
        let told = w.on_event(&lock);
        assert_eq!(kinds(&told), ["remote-locked"]);
        assert!(told[0].body.contains("the phone"));
        assert!(w.on_event(&lock).is_empty());
    }

    #[test]
    fn a_conflict_is_told_once_per_file_until_it_is_resolved() {
        let mut w = Watch::new();
        let c = |open: &[&str]| {
            ev(
                event::CONFLICT,
                "notes",
                1,
                json!({"pulled": 1, "pushed": 0, "open": open}),
            )
        };
        let n = w.on_event(&c(&["a.md"]));
        assert_eq!(kinds(&n), ["conflict"]);
        assert!(n[0].body.contains("a.md") && n[0].title.contains("a conflict"));
        // Still open, and counted again: nothing.
        assert!(w.on_event(&c(&["a.md"])).is_empty());
        // Another file: only it is told.
        let n = w.on_event(&c(&["a.md", "b.md"]));
        assert!(
            n[0].body.contains("b.md") && !n[0].body.contains("a.md"),
            "{}",
            n[0].body
        );
        // a.md resolved, then in conflict again: told again.
        assert!(w.on_event(&c(&["b.md"])).is_empty());
        assert_eq!(kinds(&w.on_event(&c(&["a.md", "b.md"]))), ["conflict"]);
        // Many at once: named in part.
        let many: Vec<String> = (0..6).map(|i| format!("m{i}.md")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let n = w.on_event(&c(&refs));
        assert!(
            n[0].title.contains("6 conflicts") && n[0].body.contains("and 3 more"),
            "{n:?}"
        );
        // An older service's counts alone: nothing to tell files apart by.
        assert!(w
            .on_event(&ev(event::CONFLICT, "notes", 1, json!({"pulled": 2})))
            .is_empty());
    }

    #[test]
    fn errors_are_told_only_when_a_vault_keeps_failing_and_once_per_episode() {
        let mut w = Watch::new();
        let min = 60_000;
        let err = |ts: i64| ev(event::ERROR, "notes", ts, json!({"message": "connection refused"}));
        // A laptop going offline: errors for a few minutes, then back.
        assert!(w.on_event(&err(0)).is_empty());
        assert!(w.on_event(&err(3 * min)).is_empty());
        let back = ev(
            event::ACTIVITY,
            "notes",
            4 * min,
            json!({"connected": true, "idle": true}),
        );
        assert!(w.on_event(&back).is_empty());
        // Much later another blip: the clock starts over.
        assert!(w.on_event(&err(60 * min)).is_empty());
        assert!(w.on_event(&err(63 * min)).is_empty());
        // Five minutes of errors: told once.
        let n = w.on_event(&err(65 * min));
        assert_eq!(kinds(&n), ["failing"]);
        assert!(
            n[0].body.contains("5 minutes") && n[0].body.contains("connection refused"),
            "{}",
            n[0].body
        );
        assert!(w.on_event(&err(70 * min)).is_empty());
        assert!(w.on_event(&err(90 * min)).is_empty());
        // The engine ending is part of the same episode.
        let failed = ev(
            event::STATE,
            "notes",
            91 * min,
            json!({"state": "failed", "message": "index error"}),
        );
        assert!(w.on_event(&failed).is_empty());
        // Connected again: a new episode may be told.
        assert!(w.on_event(&back).is_empty());
        let n = w.on_event(&ev(
            event::STATE,
            "notes",
            100 * min,
            json!({"state": "failed", "message": "index error"}),
        ));
        assert_eq!(kinds(&n), ["failing"]);
        assert!(n[0].body.contains("index error"));
        // Its retries fail the same way: nothing more.
        assert!(w
            .on_event(&ev(event::STATE, "notes", 101 * min, json!({"state": "starting"})))
            .is_empty());
        assert!(w.on_event(&failed).is_empty());
        // Nothing for the service as a whole, or for kinds that need no one.
        assert!(w
            .on_event(&ev(event::PULLED, "notes", 1, json!({"written": 1})))
            .is_empty());
        let reload = Event {
            vault: None,
            ..ev(event::RELOAD, "x", 1, json!({}))
        };
        assert!(w.on_event(&reload).is_empty());
    }

    #[test]
    fn what_the_service_shows_at_the_start_is_not_news() {
        let st: crate::protocol::StatusResult = serde_json::from_value(json!({"vaults": [{
            "name": "notes",
            "status": {
                "safety_stop": "30 files went missing",
                "upstream_stop": "1730000000000 a newer format",
                "conflicted": [{"path": "old.md"}]
            }
        }]}))
        .unwrap();
        let mut w = Watch::knowing(&st);
        let stop = ev(event::SAFETY_STOP, "notes", 1, json!({"stop": "30 files went missing"}));
        assert!(w.on_event(&stop).is_empty());
        let up = ev(event::UPSTREAM_STOP, "notes", 1, json!({"stop": "a newer format"}));
        assert!(w.on_event(&up).is_empty());
        let c = ev(event::CONFLICT, "notes", 1, json!({"open": ["old.md", "new.md"]}));
        let n = w.on_event(&c);
        assert_eq!(n[0].title, "lsync: a conflict in notes");
        assert!(n[0].body.starts_with("new.md"), "{}", n[0].body);
    }

    #[test]
    fn a_vault_that_keeps_crashing_is_told_at_most_once_in_a_while() {
        let mut w = Watch::new();
        let min = 60_000;
        let up = |ts: i64| ev(event::ACTIVITY, "notes", ts, json!({"connected": true}));
        let failed = |ts: i64| {
            ev(
                event::STATE,
                "notes",
                ts,
                json!({"state": "failed", "message": "panic"}),
            )
        };
        let mut told = 0;
        // It connects, then crashes, every minute for 20 minutes.
        for i in 0..20 {
            assert!(w.on_event(&up(i * min)).is_empty());
            told += w.on_event(&failed(i * min + 1000)).len();
        }
        assert_eq!(told, 1, "once, not at every retry");
        // Half an hour after the notice, still at it: told again.
        assert!(w.on_event(&up(31 * min)).is_empty());
        assert_eq!(kinds(&w.on_event(&failed(31 * min + 1000))), ["failing"]);
    }

    fn status_of(v: Value) -> crate::protocol::StatusResult {
        serde_json::from_value(json!({ "vaults": v })).unwrap()
    }

    #[test]
    fn after_missed_events_what_changed_is_told_and_a_new_vault_is_taken_as_it_is() {
        let mut w = Watch::knowing(&status_of(json!([
            {"name": "notes", "status": {"conflicted": [{"path": "a.md"}]}}
        ])));
        assert!(w.knows("notes") && !w.knows("work"));
        // Missed: a safety stop came and a.md was resolved, b.md in conflict.
        let n = w.catch_up(&status_of(json!([
            {"name": "notes", "status": {"safety_stop": "1730000000000 30 files went missing", "conflicted": [{"path": "b.md"}]}},
            {"name": "work", "status": {"safety_stop": "1730000000000 9 files went missing", "conflicted": [{"path": "w.md"}]}}
        ])));
        assert_eq!(kinds(&n), ["safety-stop", "conflict"], "{n:?}");
        assert!(n[1].body.starts_with("b.md"), "{}", n[1].body);
        // `work` came with a reload: what it showed then is no news.
        assert!(w.knows("work"));
        let c = ev(event::CONFLICT, "work", 1, json!({"open": ["w.md"]}));
        assert!(w.on_event(&c).is_empty());
        // a.md in conflict again, after it was resolved: told.
        let c = ev(event::CONFLICT, "notes", 1, json!({"open": ["a.md", "b.md"]}));
        assert_eq!(kinds(&w.on_event(&c)), ["conflict"]);
    }

    #[tokio::test]
    async fn the_notifier_reads_the_status_for_a_vault_it_has_not_seen_and_after_a_lag() {
        let (tx, rx) = broadcast::channel(2);
        let fake = Arc::new(Fake(Mutex::new(vec![])));
        let st = Arc::new(Mutex::new(status_of(json!([
            {"name": "new", "status": {"conflicted": [{"path": "old.md"}]}}
        ]))));
        let reader = st.clone();
        // Events pile up before the task reads them: it lags at once.
        let stop = |t: &str| ev(event::SAFETY_STOP, "new", 1, json!({ "stop": t }));
        tx.send(stop("x")).unwrap();
        tx.send(stop("y")).unwrap();
        tx.send(stop("z")).unwrap();
        *st.lock().unwrap() = status_of(json!([
            {"name": "new", "status": {"safety_stop": "z", "conflicted": [{"path": "old.md"}]}}
        ]));
        let (read_tx, mut read_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = spawn(
            rx,
            Watch::new(),
            fake.clone(),
            Arc::new(move || {
                let _ = read_tx.send(());
                Some(reader.lock().unwrap().clone())
            }),
        );
        // The lag's status read: what was buffered before it is skipped.
        read_rx.recv().await.unwrap();
        // A conflict event for the vault it now knows: only the new file.
        tx.send(ev(event::CONFLICT, "new", 1, json!({"open": ["old.md", "fresh.md"]})))
            .unwrap();
        drop(tx);
        task.await.unwrap();
        let got = fake.0.lock().unwrap().clone();
        // The lag's status read was its first sight of `new`: taken as it
        // is (the stop `z` included); the events read after it say nothing
        // new; the conflict names only fresh.md.
        assert_eq!(kinds(&got), ["conflict"], "{got:?}");
        assert!(got[0].body.starts_with("fresh.md"), "{}", got[0].body);
    }

    #[tokio::test]
    async fn after_a_lag_the_buffered_events_are_not_announced_again() {
        let (tx, rx) = broadcast::channel(2);
        let fake = Arc::new(Fake(Mutex::new(vec![])));
        let mut watch = Watch::new();
        // `notes` is known, with nothing to show.
        watch.catch_up(&status_of(json!([{"name": "notes", "status": {}}])));
        let stop = |t: &str| ev(event::SAFETY_STOP, "notes", 1, json!({ "stop": t }));
        let conflict = |p: &str| ev(event::CONFLICT, "notes", 1, json!({ "open": [p] }));
        // Missed: stop x; still buffered: a conflict on a.md and stop y,
        // both older than the status (which shows neither any more).
        tx.send(stop("x")).unwrap();
        tx.send(conflict("a.md")).unwrap();
        tx.send(stop("y")).unwrap();
        let st = status_of(json!([{"name": "notes", "status": {"safety_stop": "z"}}]));
        let task = spawn(rx, watch, fake.clone(), Arc::new(move || Some(st.clone())));
        drop(tx);
        task.await.unwrap();
        let got = fake.0.lock().unwrap().clone();
        // Only the status's stop; not the stale conflict nor stop y.
        assert_eq!(kinds(&got), ["safety-stop"], "{got:?}");
    }

    #[test]
    fn a_program_gets_the_title_and_body_after_a_double_dash() {
        let n = Notice {
            vault: Some("notes".into()),
            kind: "conflict",
            title: "lsync: a conflict in notes".into(),
            body: "-u.md\nSee `lsync status`.".into(),
            urgency: Urgency::Normal,
        };
        let c = Program::new("notify-send".into()).command(&n);
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["--", "lsync: a conflict in notes", "-u.md\nSee `lsync status`."]);
    }

    struct Fake(Mutex<Vec<Notice>>);

    impl Notifier for Fake {
        fn notify(&self, n: &Notice) {
            self.0.lock().unwrap().push(n.clone());
        }
    }

    #[tokio::test]
    async fn the_events_reach_the_notifier_once() {
        let (tx, rx) = broadcast::channel(16);
        let fake = Arc::new(Fake(Mutex::new(vec![])));
        let task = spawn(rx, Watch::new(), fake.clone(), Arc::new(|| None));
        for _ in 0..3 {
            tx.send(ev(event::SAFETY_STOP, "notes", 1, json!({"stop": "gone"})))
                .unwrap();
        }
        drop(tx);
        task.await.unwrap();
        assert_eq!(kinds(&fake.0.lock().unwrap()), ["safety-stop"]);
    }

    #[test]
    fn the_desktop_command_quotes_what_it_shows() {
        let n = Notice {
            vault: None,
            kind: "conflict",
            title: "lsync: \"x\"".into(),
            body: "-a.md".into(),
            urgency: Urgency::Normal,
        };
        let c = Desktop::command(&n);
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        if cfg!(target_os = "macos") {
            assert_eq!(c.get_program(), "osascript");
            assert_eq!(
                args[1],
                "display notification \"-a.md\" with title \"lsync: \\\"x\\\"\""
            );
        } else {
            assert_eq!(c.get_program(), "notify-send");
            // A body that starts with a dash is not read as an option.
            assert_eq!(
                args,
                ["--app-name=lsync", "--urgency=normal", "--", "lsync: \"x\"", "-a.md"]
            );
        }
    }
}
