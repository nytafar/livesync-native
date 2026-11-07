//! Status for menu bars (spec D7): `lsync status --follow --waybar` prints
//! one Waybar JSON line per change, `--format=swiftbar` the same for
//! SwiftBar, `--format=text` and `--format=json` a line for people and
//! scripts. Without `--follow`, one and done.
//!
//! The state comes from the running service: one connection follows its
//! events, another reads `status` after them (at most a few times a
//! second). Without a service the bar says `off` and looks again every few
//! seconds, so a bar started before the service catches up by itself.
//!
//! Each vault is one of these, and the bar shows the most pressing:
//! `stopped` (a safety stop, the upstream stop, a remote lock: a person
//! must act), `error` (the engine fails, or the settings cannot be used),
//! `conflict`, `offline` (the server cannot be reached), `syncing`,
//! `paused`, `ok`. `off` is no service.

use livesync_host::control::Client;
use livesync_host::protocol::{code, method, StatusResult, VaultView};
use serde::Serialize;
use std::io::Write;
use std::time::{Duration, Instant};

/// How a follower waits for more events before it reads the status.
const SETTLE: Duration = Duration::from_millis(250);
/// ... but never longer than this after the first one.
const SETTLE_MAX: Duration = Duration::from_secs(1);
/// The status is read again this often without events.
const REFRESH: Duration = Duration::from_secs(30);
/// Without a service, it looks for one again this often.
const RECONNECT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Text,
    Json,
    Waybar,
    Swiftbar,
}

/// The classes, most pressing first.
const ORDER: [&str; 8] = [
    "stopped", "error", "conflict", "offline", "syncing", "paused", "ok", "off",
];

fn rank(class: &str) -> usize {
    ORDER.iter().position(|c| *c == class).unwrap_or(ORDER.len())
}

/// One vault on the bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VaultLine {
    pub name: String,
    pub class: &'static str,
    pub text: String,
}

/// What the bar shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Bar {
    pub class: &'static str,
    pub text: String,
    pub vaults: Vec<VaultLine>,
    /// Why there is nothing to show (no service, no such vault).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A recorded `<ms> <message>` without the time, cut to one short line.
fn short(s: &str) -> String {
    let t = s
        .split_once(' ')
        .filter(|(ms, _)| ms.len() >= 10 && ms.bytes().all(|b| b.is_ascii_digit()))
        .map_or(s, |(_, m)| m);
    let line = t.lines().next().unwrap_or("");
    if line.chars().count() > 120 {
        format!("{}…", line.chars().take(119).collect::<String>())
    } else {
        line.to_string()
    }
}

/// One vault: its class and a few words.
pub fn vault_line(v: &VaultView) -> Option<VaultLine> {
    let st = v.status.as_ref();
    let line = |class: &'static str, text: String| {
        Some(VaultLine {
            name: v.name.clone(),
            class,
            text,
        })
    };
    if v.state == "disabled" {
        return None;
    }
    if let Some(s) = st.and_then(|s| s.safety_stop.as_deref()) {
        return line("stopped", format!("safety stop: {}", short(s)));
    }
    if let Some(s) = st.and_then(|s| s.upstream_stop.as_deref()) {
        return line("stopped", format!("upstream stop: {}", short(s)));
    }
    if let Some(s) = st.and_then(|s| s.remote_locked.as_deref()) {
        return line("stopped", format!("locked: {}", short(s)));
    }
    match v.state.as_str() {
        "failed" | "invalid" => {
            let why = v.message.as_deref().map(short).unwrap_or_else(|| v.state.clone());
            return line("error", format!("error: {why}"));
        }
        "paused" => return line("paused", "paused".into()),
        _ => {}
    }
    let conflicts = st.map_or(0, |s| s.conflicted.len());
    if conflicts > 0 {
        let s = if conflicts == 1 { "" } else { "s" };
        return line("conflict", format!("{conflicts} conflict{s}"));
    }
    match v.state.as_str() {
        "starting" => return line("syncing", "starting".into()),
        "busy" => {
            return line(
                "syncing",
                v.command.as_deref().map_or("busy".into(), |c| format!("running {c}")),
            )
        }
        "stopped" => return line("off", "stopped".into()),
        _ => {}
    }
    let Some(a) = &v.activity else {
        return line("ok", "synced".into());
    };
    use livesync_engine::PullPhase::*;
    match a.pull {
        Retrying => {
            let why = st.and_then(|s| s.last_error.as_deref()).map(short);
            line("offline", why.map_or("offline".into(), |w| format!("offline: {w}")))
        }
        Starting | Stopped => line("syncing", "connecting".into()),
        // A newer service's phase.
        Unknown => line("syncing", "busy".into()),
        CatchingUp | Applying => line("syncing", "pulling".into()),
        Live => {
            let pushing =
                a.two_way && (!a.pushes || a.settling > 0 || a.pushing || a.push_due || a.deletes_waiting > 0);
            if pushing {
                let n = a.settling;
                line(
                    "syncing",
                    if n > 0 {
                        format!("pushing {n} change{}", if n == 1 { "" } else { "s" })
                    } else {
                        "pushing".into()
                    },
                )
            } else {
                line("ok", "synced".into())
            }
        }
    }
}

/// The bar for a service's status (`None`: no service, with why).
pub fn summarise(st: Result<&StatusResult, String>) -> Bar {
    let st = match st {
        Ok(s) => s,
        Err(why) => {
            return Bar {
                class: "off",
                text: "off".into(),
                vaults: vec![],
                note: Some(why),
            }
        }
    };
    let vaults: Vec<VaultLine> = st.vaults.iter().filter_map(vault_line).collect();
    let Some(worst) = vaults.iter().map(|v| v.class).min_by_key(|c| rank(c)) else {
        return Bar {
            class: "ok",
            text: "no vaults".into(),
            vaults,
            note: None,
        };
    };
    let text = match worst {
        "ok" => "synced".to_string(),
        "conflict" => {
            let n: usize = st
                .vaults
                .iter()
                .filter(|v| v.state != "disabled")
                .filter_map(|v| v.status.as_ref())
                .map(|s| s.conflicted.len())
                .sum();
            format!("{n} conflict{}", if n == 1 { "" } else { "s" })
        }
        // One vault: its own words; several: the class.
        _ if vaults.len() == 1 => vaults[0].text.split(':').next().unwrap_or(worst).to_string(),
        other => other.to_string(),
    };
    Bar {
        class: worst,
        text,
        vaults,
        note: None,
    }
}

/// Pango markup (Waybar renders text and tooltips as markup).
fn markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn tooltip(b: &Bar) -> String {
    let mut lines: Vec<String> = b.vaults.iter().map(|v| format!("{}: {}", v.name, v.text)).collect();
    if let Some(n) = &b.note {
        lines.push(n.clone());
    }
    if lines.is_empty() {
        lines.push("lsync: no vaults".into());
    }
    lines.join("\n")
}

/// One Waybar line: a custom module with `"return-type": "json"`.
pub fn waybar(b: &Bar) -> String {
    serde_json::json!({
        "text": markup(&b.text),
        "alt": b.class,
        "class": b.class,
        "tooltip": markup(&tooltip(b)),
    })
    .to_string()
}

/// A SwiftBar plugin's output: the menu bar title, `---`, one line per vault.
pub fn swiftbar(b: &Bar) -> String {
    let color = match b.class {
        "stopped" | "error" => " | color=red",
        "conflict" => " | color=orange",
        "offline" | "off" | "paused" => " | color=gray",
        _ => "",
    };
    // `|` starts SwiftBar's parameters: never inside a text.
    let clean = |s: &str| s.replace('|', "/");
    let mut out = format!("lsync {}{color}\n---\n", clean(&b.text));
    for line in tooltip(b).lines() {
        out.push_str(&clean(line));
        out.push('\n');
    }
    out
}

/// One line for people.
pub fn text_line(b: &Bar) -> String {
    let parts: Vec<String> = b.vaults.iter().map(|v| format!("{}: {}", v.name, v.text)).collect();
    let rest = if parts.is_empty() {
        b.note.clone().unwrap_or_default()
    } else {
        parts.join("; ")
    };
    format!(
        "{} {}{}",
        crate::clock(livesync_engine::vaultfs::now_ms()),
        b.text,
        if rest.is_empty() {
            String::new()
        } else {
            format!(" — {rest}")
        }
    )
}

fn render(b: &Bar, f: Format, follow: bool) -> String {
    match f {
        Format::Waybar => waybar(b),
        // A streamable SwiftBar plugin separates its updates with `~~~`.
        Format::Swiftbar if follow => format!("~~~\n{}", swiftbar(b).trim_end()),
        Format::Swiftbar => swiftbar(b).trim_end().to_string(),
        Format::Json => serde_json::to_string(b).unwrap_or_default(),
        Format::Text => text_line(b),
    }
}

/// The service, if one runs (an error that is not "no service" is the
/// bar's note).
fn connect() -> Result<Client, String> {
    match crate::route::connect(&[method::STATUS, method::EVENTS]) {
        Ok(Some(c)) => Ok(c),
        Ok(None) => Err("no lsync service is running".into()),
        Err(e) => Err(format!("{e:#}")),
    }
}

fn read(client: &mut Client, vault: Option<&str>) -> Result<Bar, String> {
    client.set_timeout(Some(Duration::from_secs(30)));
    let v = client.call(method::STATUS, serde_json::json!({ "vault": vault }));
    client.set_timeout(None);
    match v {
        Ok(v) => serde_json::from_value::<StatusResult>(v)
            .map(|st| summarise(Ok(&st)))
            .map_err(|e| e.to_string()),
        // A vault that is not (yet) in the service: shown, not an end.
        Err(e) if e.code() == Some(code::UNKNOWN_VAULT) => Ok(Bar {
            class: "error",
            text: "error".into(),
            vaults: vec![],
            note: Some(e.to_string()),
        }),
        Err(e) => Err(e.to_string()),
    }
}

/// Print `b` unless it is what was printed last. `false` when stdout is
/// gone (the bar went away).
fn show(out: &mut impl Write, last: &mut Option<String>, s: String) -> bool {
    if last.as_deref() == Some(s.as_str()) {
        return true;
    }
    let ok = writeln!(out, "{s}").and_then(|_| out.flush()).is_ok();
    *last = Some(s);
    ok
}

/// `lsync status [--follow] --waybar | --format=...`.
pub fn run(vault: Option<String>, format: Format, follow: bool) -> anyhow::Result<u8> {
    let mut out = std::io::stdout();
    let mut last: Option<String> = None;
    loop {
        let session = connect().and_then(|mut status| {
            let mut events = connect()?;
            events.subscribe(vault.as_deref()).map_err(|e| e.to_string())?;
            let bar = read(&mut status, vault.as_deref())?;
            Ok((status, events, bar))
        });
        let (mut status, mut events, bar) = match session {
            Ok(s) => s,
            Err(why) => {
                if !show(&mut out, &mut last, render(&summarise(Err(why)), format, follow)) || !follow {
                    return Ok(0);
                }
                std::thread::sleep(RECONNECT);
                continue;
            }
        };
        if !show(&mut out, &mut last, render(&bar, format, follow)) || !follow {
            return Ok(0);
        }
        // Follow: read the status after a burst of events (or now and then).
        loop {
            match events.next_event_within(REFRESH) {
                Ok(Some(_)) => {
                    let first = Instant::now();
                    loop {
                        let left = SETTLE_MAX.saturating_sub(first.elapsed());
                        if left.is_zero() {
                            break;
                        }
                        match events.next_event_within(SETTLE.min(left)) {
                            Ok(Some(_)) => {}
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => break,
            }
            match read(&mut status, vault.as_deref()) {
                Ok(bar) => {
                    if !show(&mut out, &mut last, render(&bar, format, follow)) {
                        return Ok(0);
                    }
                }
                Err(_) => break,
            }
        }
        // The service went away: say so, and look for the next one.
        let gone = summarise(Err("the lsync service stopped".into()));
        if !show(&mut out, &mut last, render(&gone, format, follow)) {
            return Ok(0);
        }
        std::thread::sleep(RECONNECT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A service's status with one vault in each state the bar tells apart.
    fn status(vaults: serde_json::Value) -> StatusResult {
        serde_json::from_value(json!({ "vaults": vaults })).unwrap()
    }

    fn live(extra: serde_json::Value) -> serde_json::Value {
        let mut a = json!({"running": true, "two_way": true, "pull": "live", "pushes": true});
        for (k, v) in extra.as_object().unwrap() {
            a[k] = v.clone();
        }
        a
    }

    fn cases() -> Vec<(&'static str, StatusResult)> {
        vec![
            (
                "synced",
                status(json!([{"name": "notes", "state": "running", "activity": live(json!({})), "status": {}}])),
            ),
            (
                "pushing",
                status(
                    json!([{"name": "notes", "state": "running", "activity": live(json!({"settling": 2})), "status": {}}]),
                ),
            ),
            (
                "conflicts",
                status(json!([
                    {"name": "notes", "state": "running", "activity": live(json!({})),
                     "status": {"conflicted": [{"path": "a.md"}, {"path": "b.md"}]}},
                    {"name": "work", "state": "running", "activity": live(json!({"pull": "catching-up"})), "status": {}}
                ])),
            ),
            (
                "safety stop",
                status(json!([
                    {"name": "notes", "state": "running", "activity": live(json!({})),
                     "status": {"safety_stop": "1730000000000 30 files went missing <all of Notes/> & more"}},
                    {"name": "work", "state": "failed", "message": "index error"}
                ])),
            ),
            (
                "offline",
                status(
                    json!([{"name": "notes", "state": "running", "activity": live(json!({"pull": "retrying"})),
                               "status": {"last_error": "1730000000000 changes feed failed: connection refused"}}]),
                ),
            ),
            (
                "paused and disabled",
                status(json!([{"name": "notes", "state": "paused"}, {"name": "old", "state": "disabled"}])),
            ),
        ]
    }

    #[test]
    fn the_waybar_lines_match_the_golden_file() {
        let mut got = String::new();
        for (what, st) in cases() {
            got.push_str(&format!("# {what}\n{}\n", waybar(&summarise(Ok(&st)))));
        }
        got.push_str(&format!(
            "# no service\n{}\n",
            waybar(&summarise(Err("no lsync service is running".into())))
        ));
        let golden = include_str!("../tests/golden/waybar.jsonl");
        assert_eq!(
            got, golden,
            "the Waybar output changed; if on purpose, update tests/golden/waybar.jsonl:\n{got}"
        );
        // Every line is one JSON object with the fields Waybar reads.
        for l in golden.lines().filter(|l| !l.starts_with('#')) {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            for k in ["text", "alt", "class", "tooltip"] {
                assert!(v[k].is_string(), "{k} in {l}");
            }
        }
    }

    #[test]
    fn the_most_pressing_vault_wins() {
        let classes: Vec<&str> = cases().iter().map(|(_, st)| summarise(Ok(st)).class).collect();
        assert_eq!(classes, ["ok", "syncing", "conflict", "stopped", "offline", "paused"]);
        let b = summarise(Ok(&cases()[2].1));
        assert_eq!(b.text, "2 conflicts");
        assert_eq!(b.vaults[1].text, "pulling");
    }

    #[test]
    fn swiftbar_gets_a_title_and_a_menu() {
        let b = summarise(Ok(&cases()[3].1));
        let s = swiftbar(&b);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "lsync stopped | color=red");
        assert_eq!(lines[1], "---");
        assert!(lines[2].starts_with("notes: safety stop: 30 files went missing"), "{s}");
        assert_eq!(lines[3], "work: error: index error");
        // Followed, each update starts with SwiftBar's separator.
        assert!(render(&b, Format::Swiftbar, true).starts_with("~~~\nlsync stopped"));
        let off = swiftbar(&summarise(Err("no lsync service is running".into())));
        assert_eq!(off, "lsync off | color=gray\n---\nno lsync service is running\n");
    }

    #[test]
    fn a_recorded_time_is_left_out_and_nothing_else() {
        assert_eq!(short("1730000000000 30 files went missing"), "30 files went missing");
        assert_eq!(short("30 files went missing"), "30 files went missing");
        assert_eq!(short("1730000000000 two\nlines"), "two");
        assert_eq!(short(&"x".repeat(200)).chars().count(), 120);
    }

    #[test]
    fn only_changes_are_printed() {
        let mut out = Vec::new();
        let mut last = None;
        assert!(show(&mut out, &mut last, "a".into()));
        assert!(show(&mut out, &mut last, "a".into()));
        assert!(show(&mut out, &mut last, "b".into()));
        assert_eq!(String::from_utf8(out).unwrap(), "a\nb\n");
    }
}
