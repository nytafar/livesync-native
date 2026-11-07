//! `lsync flush` and `lsync wait` (spec D7): hand a file from one machine to
//! another and know it arrived. With `flush` on one machine and `wait` on
//! the other, an agent writes a note, flushes it (which prints the revision
//! now on the server) and the other side waits for exactly that revision.
//!
//! Both run in the service when it runs the vault. Without a service:
//! `flush` runs one two-way sync here and reports from it; `wait` runs the
//! vault here (read-only for `--applied`) until the condition holds.
//!
//! Exit codes, for scripts and agents:
//! - 0: stored (flush), or the condition holds (wait);
//! - 1: an error (no such vault, bad arguments, the service failed);
//! - 2: blocked: it cannot happen until a person acts (a safety stop, the
//!   upstream stop, a remote lock, a gate's refusal, a paused vault, or a
//!   revision a local edit or a conflict holds back);
//! - 3: flush: done, but not everything is on the server (see the report);
//! - 124: the timeout ran out first (as `timeout(1)` exits).

use crate::config::{self, FileConfig, Selected};
use crate::route::{self, Route};
use anyhow::{anyhow, bail, Result};
use livesync_engine::{EngineError, FlushReport, Reached, Target, WaitFor, WaitOutcome};
use livesync_host::config::{Dirs, Mode};
use livesync_host::control::Client;
use livesync_host::protocol::{code, method, WaitResult};
use serde_json::json;
use std::time::{Duration, Instant};

pub const EXIT_BLOCKED: u8 = 2;
pub const EXIT_PARTIAL: u8 = 3;
pub const EXIT_TIMEOUT: u8 = 124;

/// How much longer than the service's own timeout the CLI waits for its
/// answer.
const MARGIN: Duration = Duration::from_secs(15);

#[derive(clap::Args, Debug, Clone)]
pub struct FlushArgs {
    /// Files or folders (vault-relative, or paths inside the vault); none:
    /// every pending change
    pub paths: Vec<String>,
    /// Give up after this many seconds (0: no limit); exit 124
    #[arg(long, value_name = "SECS", default_value_t = 60)]
    pub timeout: u64,
    /// Print the report as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Debug, Clone)]
pub struct WaitArgs {
    /// Until this file is in the local file at this revision (or a later
    /// one): PATH@REV as `lsync flush` printed it. Without @REV, the
    /// revision the server has when the wait begins
    #[arg(
        long,
        value_name = "PATH[@REV]",
        required_unless_present = "idle",
        conflicts_with = "idle"
    )]
    pub applied: Option<String>,
    /// Until nothing is pending or settling: the vault follows the server
    /// live, and no local change waits to be pushed
    #[arg(long)]
    pub idle: bool,
    /// Give up after this many seconds (0: no limit); exit 124
    #[arg(long, value_name = "SECS", default_value_t = 60)]
    pub timeout: u64,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

/// `PATH@REV` as a path and a revision: the part after the last `@` is a
/// revision only when it looks like one (`<generation>-<hash>`), so a path
/// with an `@` in it stays whole.
pub fn split_rev(s: &str) -> (String, Option<String>) {
    if let Some((p, r)) = s.rsplit_once('@') {
        let looks = r.split_once('-').is_some_and(|(g, h)| {
            !g.is_empty()
                && g.bytes().all(|b| b.is_ascii_digit())
                && !h.is_empty()
                && h.bytes().all(|b| b.is_ascii_alphanumeric())
        });
        if looks && !p.is_empty() {
            return (p.to_string(), Some(r.to_string()));
        }
    }
    (s.to_string(), None)
}

fn limit(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// What a service's error means for the exit code (`None`: an error).
fn exit_for(c: Option<&str>) -> Option<u8> {
    match c? {
        code::TIMEOUT => Some(EXIT_TIMEOUT),
        code::BLOCKED | code::PAUSED | code::REFUSED => Some(EXIT_BLOCKED),
        _ => None,
    }
}

/// A call to the service that may take as long as the timeout.
fn call(
    client: &mut Client,
    m: &str,
    timeout: Option<Duration>,
    p: serde_json::Value,
) -> Result<std::result::Result<serde_json::Value, (u8, String)>> {
    client.set_timeout(timeout.map(|t| t + MARGIN));
    match client.call(m, p) {
        Ok(v) => Ok(Ok(v)),
        Err(e) => match exit_for(e.code()) {
            Some(code) => Ok(Err((code, e.to_string()))),
            None => Err(anyhow!("{e}")),
        },
    }
}

fn print_flush(rep: &FlushReport, json: bool) -> Result<u8> {
    if json {
        println!("{}", serde_json::to_string_pretty(rep)?);
    } else {
        if rep.all && rep.paths.is_empty() {
            println!("nothing to push: the server has every local change");
        }
        for p in &rep.paths {
            let note = p.note.as_deref().map(|n| format!("  ({n})")).unwrap_or_default();
            println!(
                "{:<10} {:<36} {}{note}",
                p.outcome,
                p.rev.as_deref().unwrap_or("-"),
                p.path
            );
        }
        for i in &rep.issues {
            println!("{:<10} {:<36} {}  ({})", "not-synced", "-", i.path, i.note);
        }
        if let Some(s) = &rep.safety_stop {
            println!("SAFETY STOP {s} (see `lsync status`)");
        }
        if !rep.complete {
            eprintln!("not everything is on the server (see above)");
        }
    }
    Ok(if rep.complete { 0 } else { EXIT_PARTIAL })
}

fn print_wait(r: &WaitResult, json: bool) -> Result<u8> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(0);
    }
    let after = format!("after {:.1}s", r.waited_ms as f64 / 1000.0);
    let x: &Reached = &r.reached;
    match (&x.path, x.deleted, x.superseded) {
        (None, _, _) => println!("{} is idle ({after})", r.vault),
        (Some(p), true, _) => println!("{p} is deleted here, as on the server ({after})"),
        (Some(p), false, true) => println!(
            "{p} is at {}, later than {} ({after})",
            x.current_rev.as_deref().unwrap_or("-"),
            x.rev.as_deref().unwrap_or("-")
        ),
        (Some(p), false, false) => println!("{p} is at {} ({after})", x.current_rev.as_deref().unwrap_or("-")),
    }
    Ok(0)
}

fn local_paths(r: &config::Resolved, given: &[String]) -> Result<Vec<String>> {
    given.iter().map(|p| crate::vault_relative(&r.vault, p)).collect()
}

/// `lsync flush [PATH...]`.
pub fn flush(sel: &Selected, cfg: &FileConfig, dirs: &Dirs, a: &FlushArgs) -> Result<u8> {
    if sel.mode == Mode::PullOnly {
        bail!(
            "vault {} is pull-only (mode = \"pull-only\") and never writes to the server; there is nothing to flush",
            sel.name
        );
    }
    let r = config::resolve(sel, cfg, dirs, false)?;
    let paths = local_paths(&r, &a.paths)?;
    let timeout = limit(a.timeout);
    match route::route(&r, "flush", false)? {
        Route::Service { mut client, vault, .. } => {
            let p = json!({
                "vault": vault,
                "paths": paths,
                "timeout_ms": timeout.map(|t| t.as_millis() as u64),
            });
            match call(&mut client, method::FLUSH, timeout, p)? {
                Ok(v) => print_flush(&serde_json::from_value(v)?, a.json),
                Err((code, msg)) => {
                    eprintln!("flush: {msg}");
                    Ok(code)
                }
            }
        }
        Route::Local => {
            let engine = crate::engine_for(sel, cfg, dirs, &crate::PushOpts::default())?;
            let paths = (!paths.is_empty()).then_some(paths);
            crate::rt()?.block_on(async {
                let run = engine.flush_once(paths);
                let r = match timeout {
                    Some(t) => match tokio::time::timeout(t, run).await {
                        Ok(r) => r,
                        Err(_) => {
                            eprintln!("flush: the sync did not finish within {}s", t.as_secs());
                            return Ok(EXIT_TIMEOUT);
                        }
                    },
                    None => run.await,
                };
                match r {
                    Ok(rep) => print_flush(&rep, a.json),
                    Err(EngineError::Refused(why)) => {
                        eprintln!("flush refused: {why}");
                        Ok(EXIT_BLOCKED)
                    }
                    Err(e) => Err(e.into()),
                }
            })
        }
    }
}

/// `lsync wait --applied PATH[@REV] | --idle`.
pub fn wait(sel: &Selected, cfg: &FileConfig, dirs: &Dirs, a: &WaitArgs) -> Result<u8> {
    let r = config::resolve(sel, cfg, dirs, false)?;
    let applied = match &a.applied {
        Some(s) => {
            let (p, rev) = split_rev(s);
            Some((crate::vault_relative(&r.vault, &p)?, rev))
        }
        None => None,
    };
    let timeout = limit(a.timeout);
    match route::route(&r, "wait", false)? {
        Route::Service { mut client, vault, .. } => {
            let p = json!({
                "vault": vault,
                "idle": a.idle,
                "path": applied.as_ref().map(|x| &x.0),
                "rev": applied.as_ref().and_then(|x| x.1.as_ref()),
                "timeout_ms": timeout.map(|t| t.as_millis() as u64),
            });
            match call(&mut client, method::WAIT, timeout, p)? {
                Ok(v) => print_wait(&serde_json::from_value(v)?, a.json),
                Err((code, msg)) => {
                    eprintln!("wait: {msg}");
                    Ok(code)
                }
            }
        }
        Route::Local => wait_here(sel, cfg, dirs, a, applied, timeout),
    }
}

/// Without a service: run the vault here until the condition holds. Only
/// the pull side for `--applied` (it never writes to the server); for
/// `--idle`, the vault as it is configured.
fn wait_here(
    sel: &Selected,
    cfg: &FileConfig,
    dirs: &Dirs,
    a: &WaitArgs,
    applied: Option<(String, Option<String>)>,
    timeout: Option<Duration>,
) -> Result<u8> {
    let engine = crate::engine_for(sel, cfg, dirs, &crate::PushOpts::default())?;
    let read_only = applied.is_some() || sel.mode == Mode::PullOnly;
    let t0 = Instant::now();
    let until = t0 + timeout.unwrap_or(Duration::from_secs(10 * 365 * 86_400));
    crate::rt()?.block_on(async {
        let (stop, rx) = tokio::sync::watch::channel(false);
        let daemon = tokio::spawn({
            let e = engine.clone();
            async move {
                if read_only {
                    e.daemon_pull_only(rx).await
                } else {
                    e.run(rx).await
                }
            }
        });
        let outcome = async {
            // The gate first: it settles how paths name documents.
            let mut activity = engine.watch_activity();
            let _ = tokio::time::timeout_at(until.into(), activity.wait_for(|a| a.pull.connected() || a.ended)).await;
            let cond = match &applied {
                None => WaitFor::Idle,
                Some((path, rev)) => {
                    let target = match rev {
                        Some(r) => Target::Rev(r.clone()),
                        None => engine.target_of(path, None).await?,
                    };
                    WaitFor::Applied {
                        path: path.clone(),
                        target,
                    }
                }
            };
            engine.wait_for(&cond, until).await
        }
        .await;
        let _ = stop.send(true);
        let ended = daemon.await.map_err(|e| anyhow!("{e}"))?;
        let vault = sel.name.clone();
        match outcome {
            Ok(WaitOutcome::Reached(reached)) => print_wait(
                &WaitResult {
                    vault,
                    condition: if applied.is_some() { "applied" } else { "idle" }.into(),
                    reached,
                    waited_ms: t0.elapsed().as_millis() as u64,
                },
                a.json,
            ),
            Ok(WaitOutcome::Blocked(why)) => {
                eprintln!("wait: vault {vault}: {why}");
                Ok(EXIT_BLOCKED)
            }
            Ok(WaitOutcome::TimedOut(why)) => {
                eprintln!(
                    "wait: vault {vault}: not within {}s: {why}",
                    timeout.unwrap_or_default().as_secs()
                );
                Ok(EXIT_TIMEOUT)
            }
            // The daemon ended by itself: why.
            Ok(WaitOutcome::Stopped) => match ended {
                Err(EngineError::Refused(why)) => {
                    eprintln!("wait refused: {why}");
                    Ok(EXIT_BLOCKED)
                }
                Err(e) => Err(e.into()),
                Ok(()) => bail!("the sync stopped before the condition held"),
            },
            Err(EngineError::Refused(why)) => {
                eprintln!("wait refused: {why}");
                Ok(EXIT_BLOCKED)
            }
            Err(e) => Err(e.into()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revision_is_split_off_only_when_it_looks_like_one() {
        assert_eq!(split_rev("a.md@3-abc"), ("a.md".into(), Some("3-abc".into())));
        assert_eq!(
            split_rev("me@host.md@12-0f9e"),
            ("me@host.md".into(), Some("12-0f9e".into()))
        );
        for whole in [
            "a.md",
            "me@host.md",
            "a.md@",
            "a.md@x-1",
            "a.md@3-",
            "a.md@-a",
            "@3-abc",
            "a.md@3-a.b",
        ] {
            assert_eq!(split_rev(whole), (whole.to_string(), None), "{whole}");
        }
    }

    #[test]
    fn the_service_s_codes_map_to_exit_codes() {
        assert_eq!(exit_for(Some(code::TIMEOUT)), Some(EXIT_TIMEOUT));
        for c in [code::BLOCKED, code::PAUSED, code::REFUSED] {
            assert_eq!(exit_for(Some(c)), Some(EXIT_BLOCKED), "{c}");
        }
        assert_eq!(exit_for(Some(code::UNKNOWN_VAULT)), None);
        assert_eq!(exit_for(None), None);
    }
}
