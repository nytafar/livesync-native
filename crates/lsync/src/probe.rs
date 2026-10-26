//! `lsync probe <vault>`: the two on-device checks the design rests on.
//!
//! 1. Does Obsidian Android pick up files written by another process, both
//!    while it is open and while it is in the background?
//! 2. Does inotify see Obsidian's saves under shared storage?
//!
//! Also records filesystem facts that M2 depends on (atomic rename, setting
//! mtime, case sensitivity, forbidden characters). Writes a Markdown report.

use anyhow::{bail, Context, Result};
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use livesync_engine::vaultfs::{self, Expect};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, clap::Args)]
pub struct ProbeArgs {
    /// Vault folder (the device-storage vault Obsidian has open)
    pub vault: PathBuf,
    /// Where to write the report (default: ./lsync-probe-report-<time>.md)
    #[arg(long)]
    pub report: Option<PathBuf>,
    /// Seconds to wait between writing and asking (gives time to switch apps)
    #[arg(long, default_value_t = 10)]
    pub delay: u64,
    /// Watch for this many seconds instead of waiting for Enter in the inotify step
    #[arg(long)]
    pub watch_secs: Option<u64>,
    /// Keep the probe notes afterwards
    #[arg(long)]
    pub keep: bool,
}

const NOTE: &str = "lsync-probe.md";
const NOTE_OPEN: &str = "lsync-probe-new-while-open.md";
const NOTE_BG: &str = "lsync-probe-new-while-background.md";

struct Prompter {
    stdin: std::io::StdinLock<'static>,
    eof: bool,
}

impl Prompter {
    fn line(&mut self) -> String {
        let mut s = String::new();
        if self.eof {
            return s;
        }
        match self.stdin.read_line(&mut s) {
            Ok(0) | Err(_) => {
                self.eof = true;
            }
            Ok(_) => {}
        }
        s.trim().to_string()
    }

    fn enter(&mut self, msg: &str) {
        print!("\n{msg}\n  [press Enter] ");
        let _ = std::io::stdout().flush();
        self.line();
    }

    /// Returns "yes", "no" or "unsure".
    fn ask(&mut self, q: &str) -> &'static str {
        loop {
            print!("\n{q}\n  [y]es / [n]o / [u]nsure: ");
            let _ = std::io::stdout().flush();
            let a = self.line().to_lowercase();
            if self.eof && a.is_empty() {
                return "unsure";
            }
            match a.as_str() {
                "y" | "yes" | "j" | "ja" => return "yes",
                "n" | "no" | "nei" => return "no",
                "u" | "unsure" | "?" | "" => return "unsure",
                _ => println!("  please answer y, n or u"),
            }
        }
    }

    fn text(&mut self, q: &str) -> String {
        print!("\n{q}\n  > ");
        let _ = std::io::stdout().flush();
        self.line()
    }
}

fn stamp() -> String {
    let s = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    format!("{s}")
}

fn note_body(step: &str, detail: &str) -> String {
    format!(
        "# lsync probe\n\n**{step}**\n\n{detail}\n\nWritten by `lsync probe` at unix time {}. Safe to delete.\n",
        stamp()
    )
}

fn write_note(vault: &Path, name: &str, body: &str) -> Result<()> {
    let target = vault.join(name);
    let expect = match vaultfs::read_state(&target)? {
        None => Expect::Absent,
        Some(s) => Expect::Hash(s.hash),
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    vaultfs::atomic_write(&target, body.as_bytes(), now, &expect)
        .with_context(|| format!("writing {}", target.display()))?;
    Ok(())
}

fn countdown(secs: u64, what: &str) {
    for i in (1..=secs).rev() {
        print!("\r  {what} in {i:>2}s ");
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("\r  {what} now.        ");
}

#[derive(Debug, Clone)]
struct Ev {
    t_ms: u128,
    path: String,
    mask: String,
    cookie: u32,
}

fn mask_names(m: EventMask) -> String {
    let names = [
        (EventMask::CREATE, "CREATE"),
        (EventMask::MODIFY, "MODIFY"),
        (EventMask::CLOSE_WRITE, "CLOSE_WRITE"),
        (EventMask::MOVED_FROM, "MOVED_FROM"),
        (EventMask::MOVED_TO, "MOVED_TO"),
        (EventMask::DELETE, "DELETE"),
        (EventMask::ATTRIB, "ATTRIB"),
        (EventMask::DELETE_SELF, "DELETE_SELF"),
        (EventMask::MOVE_SELF, "MOVE_SELF"),
        (EventMask::ISDIR, "ISDIR"),
        (EventMask::Q_OVERFLOW, "Q_OVERFLOW"),
        (EventMask::IGNORED, "IGNORED"),
    ];
    names
        .iter()
        .filter(|(f, _)| m.contains(*f))
        .map(|(_, n)| *n)
        .collect::<Vec<_>>()
        .join("|")
}

struct Watcher {
    events: Arc<Mutex<Vec<Ev>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    watched_dirs: usize,
    watch_errors: Vec<String>,
}

const MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::MODIFY)
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::DELETE)
    .union(WatchMask::ATTRIB);

fn start_watcher(vault: &Path) -> Result<Watcher> {
    let mut ino = Inotify::init().context("inotify_init")?;
    let mut wds: HashMap<WatchDescriptor, PathBuf> = HashMap::new();
    let mut errors = Vec::new();
    let mut stack = vec![vault.to_path_buf()];
    while let Some(dir) = stack.pop() {
        match ino.watches().add(&dir, MASK) {
            Ok(wd) => {
                wds.insert(wd, dir.clone());
            }
            Err(e) => errors.push(format!("{}: {e}", dir.display())),
        }
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(e.path());
                }
            }
        }
    }
    let watched = wds.len();
    let events = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (ev2, stop2, root) = (events.clone(), stop.clone(), vault.to_path_buf());
    let t0 = Instant::now();
    let handle = std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        while !stop2.load(Ordering::Relaxed) {
            let mut new_dirs = Vec::new();
            match ino.read_events(&mut buf) {
                Ok(evs) => {
                    for e in evs {
                        let dir = wds.get(&e.wd).cloned().unwrap_or_default();
                        let full = match e.name {
                            Some(n) => dir.join(n),
                            None => dir.clone(),
                        };
                        if e.mask.contains(EventMask::ISDIR)
                            && e.mask.intersects(EventMask::CREATE | EventMask::MOVED_TO)
                        {
                            new_dirs.push(full.clone());
                        }
                        let rel = full.strip_prefix(&root).unwrap_or(&full).display().to_string();
                        ev2.lock().unwrap().push(Ev {
                            t_ms: t0.elapsed().as_millis(),
                            path: rel,
                            mask: mask_names(e.mask),
                            cookie: e.cookie,
                        });
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
            for d in new_dirs {
                if let Ok(wd) = ino.watches().add(&d, MASK) {
                    wds.insert(wd, d);
                }
            }
        }
    });
    Ok(Watcher {
        events,
        stop,
        handle: Some(handle),
        watched_dirs: watched,
        watch_errors: errors,
    })
}

impl Watcher {
    fn take(&self) -> Vec<Ev> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
    fn finish(mut self) -> Vec<Ev> {
        std::thread::sleep(Duration::from_millis(300));
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.take()
    }
}

/// Filesystem facts M2 relies on, checked in a hidden scratch directory.
fn fs_checks(vault: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let dir = vault.join(".lsync-probe-fs");
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        out.push(("create scratch dir".into(), format!("FAILED: {e}")));
        return out;
    }
    // Atomic replace by rename, and setting mtime.
    let a = dir.join("a.md");
    let r1 = vaultfs::atomic_write(&a, b"first", 1_600_000_000_000, &Expect::Absent).map(|w| w.state);
    let r2 = r1.as_ref().ok().map(|s| {
        vaultfs::atomic_write(&a, b"second", 1_600_000_123_000, &Expect::Hash(s.hash.clone())).map(|w| w.state)
    });
    match (&r1, &r2) {
        (Ok(_), Some(Ok(s2))) => {
            let content = std::fs::read(&a).unwrap_or_default();
            out.push((
                "temp file + rename over existing file".into(),
                if content == b"second" {
                    "ok".into()
                } else {
                    "content wrong after rename".into()
                },
            ));
            out.push((
                "set mtime".into(),
                if s2.mtime_ms == 1_600_000_123_000 {
                    "ok (exact to the millisecond)".into()
                } else if (s2.mtime_ms - 1_600_000_123_000).abs() < 2000 {
                    format!("ok within 2 s (read back {})", s2.mtime_ms)
                } else {
                    format!("NOT applied (read back {})", s2.mtime_ms)
                },
            ));
        }
        (Err(e), _) | (_, Some(Err(e))) => out.push(("temp file + rename".into(), format!("FAILED: {e}"))),
        _ => {}
    }
    // The rules the engine probes at vault open (FsRules).
    match livesync_engine::fsrules::FsRules::probe(&dir) {
        Ok(r) => {
            out.push((
                "file names".into(),
                if r.case_insensitive {
                    "case-INsensitive".into()
                } else {
                    "case-sensitive".into()
                },
            ));
            out.push((
                "Unicode normalisation".into(),
                if r.normalization_insensitive {
                    "NFC and NFD name the same file".into()
                } else {
                    "NFC and NFD are different names".into()
                },
            ));
            out.push((
                "forbidden characters in names".into(),
                if r.forbidden.is_empty() {
                    "none of : ? \" * < > | \\".into()
                } else {
                    format!(
                        "rejected: {}",
                        r.forbidden
                            .iter()
                            .map(|c| format!("{c:?}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                },
            ));
            out.push((
                "control characters in names".into(),
                if r.control_chars_forbidden {
                    "rejected"
                } else {
                    "allowed"
                }
                .into(),
            ));
            out.push(("longest name".into(), format!("{} bytes", r.max_name_bytes)));
        }
        Err(e) => out.push(("filesystem rules".into(), format!("FAILED: {e}"))),
    }
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn mount_info(vault: &Path) -> String {
    let canon = std::fs::canonicalize(vault).unwrap_or_else(|_| vault.to_path_buf());
    let Ok(m) = std::fs::read_to_string("/proc/mounts") else {
        return "unknown".into();
    };
    let mut best: Option<(usize, String)> = None;
    for line in m.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        let mp = f[1].replace("\\040", " ");
        if canon.starts_with(&mp) && best.as_ref().map(|(l, _)| mp.len() > *l).unwrap_or(true) {
            best = Some((mp.len(), format!("{} on {} type {}", f[0], mp, f[2])));
        }
    }
    best.map(|b| b.1).unwrap_or_else(|| "unknown".into())
}

fn getprop(key: &str) -> Option<String> {
    let out = std::process::Command::new("getprop").arg(key).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Why probing this vault would leak into a LiveSync database, if it would.
///
/// The probe writes, edits, renames and deletes `lsync-probe*.md`. In a vault
/// where the Self-hosted LiveSync plugin is enabled, the plugin pushes those
/// notes and their deletions to the real database. Enabled means the plugin
/// folder exists and `.obsidian/community-plugins.json` lists it; when that
/// file exists but cannot be read, assume enabled.
pub fn livesync_enabled(vault: &Path) -> Option<String> {
    const ID: &str = "obsidian-livesync";
    let obsidian = vault.join(".obsidian");
    if !obsidian.join("plugins").join(ID).is_dir() {
        return None;
    }
    let list = obsidian.join("community-plugins.json");
    let text = match std::fs::read_to_string(&list) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(format!("{} exists but cannot be read ({e})", list.display())),
    };
    match serde_json::from_str::<Vec<String>>(&text) {
        Ok(ids) if ids.iter().any(|i| i == ID) => Some(format!("{} lists {ID}", list.display())),
        Ok(_) => None,
        Err(e) => Some(format!("{} cannot be parsed ({e})", list.display())),
    }
}

pub fn run(args: ProbeArgs) -> Result<u8> {
    let vault = std::fs::canonicalize(&args.vault).with_context(|| format!("vault {}", args.vault.display()))?;
    if !vault.is_dir() {
        bail!("{} is not a directory", vault.display());
    }
    if let Some(why) = livesync_enabled(&vault) {
        bail!(
            "refusing to probe {}: the Self-hosted LiveSync plugin is enabled in this vault ({why}). \
             The probe writes, renames and deletes lsync-probe*.md notes, and the plugin would push \
             them and their deletions to the real database. Probe a vault without LiveSync, \
             or disable the plugin in Obsidian first.",
            vault.display()
        );
    }
    let mut p = Prompter {
        stdin: Box::leak(Box::new(std::io::stdin())).lock(),
        eof: false,
    };
    let mut answers: Vec<(String, String)> = Vec::new();
    println!("lsync probe — vault: {}", vault.display());
    println!("This writes a few notes named lsync-probe*.md at the vault root and asks what Obsidian shows.");

    let fs_facts = fs_checks(&vault);
    for (k, v) in &fs_facts {
        println!("  fs: {k}: {v}");
    }

    // ---- Check 1: outside writes while Obsidian is open ----
    write_note(
        &vault,
        NOTE,
        &note_body(
            "Step 1",
            "If you can read this, Obsidian found a file written by lsync.",
        ),
    )?;
    p.enter(&format!(
        "Step 1. Open Obsidian on this vault and open the note `{}` (use the file list or quick switcher). Then come back here.",
        NOTE.trim_end_matches(".md")
    ));
    answers.push((
        "1. Note written before opening Obsidian shows 'Step 1'".into(),
        p.ask("Did the note exist and show 'Step 1'?").into(),
    ));

    p.enter(&format!(
        "Step 2. When you press Enter, lsync waits {}s, then changes the note and creates `{}`. Switch to Obsidian right away and keep `{}` open on screen.",
        args.delay,
        NOTE_OPEN.trim_end_matches(".md"),
        NOTE.trim_end_matches(".md")
    ));
    countdown(args.delay, "writing");
    let t_open = Instant::now();
    write_note(
        &vault,
        NOTE,
        &note_body("Step 2", "This text was replaced while the note was open in Obsidian."),
    )?;
    write_note(
        &vault,
        NOTE_OPEN,
        &note_body("New while open", "Created while Obsidian was in the foreground."),
    )?;
    println!("  written. Watch Obsidian, then come back.");
    answers.push((
        "2a. Open note updated to 'Step 2' without reopening".into(),
        p.ask("Did the open note change to 'Step 2' by itself?").into(),
    ));
    answers.push((
        "2b. Time until it updated (user estimate)".into(),
        p.text("Roughly how long did it take? (e.g. instantly, 2s, only after reopening)"),
    ));
    answers.push((
        "2c. New file created while open appeared in the file list".into(),
        p.ask(&format!(
            "Did `{}` appear in Obsidian's file list?",
            NOTE_OPEN.trim_end_matches(".md")
        ))
        .into(),
    ));
    let open_secs = t_open.elapsed().as_secs();

    // ---- Check 1b: outside writes while Obsidian is in the background ----
    p.enter("Step 3. Make sure Obsidian is in the background (you are in the terminal now, so it is). Press Enter to write.");
    write_note(
        &vault,
        NOTE,
        &note_body("Step 3", "This text was replaced while Obsidian was in the background."),
    )?;
    write_note(
        &vault,
        NOTE_BG,
        &note_body(
            "New while in background",
            "Created while Obsidian was in the background.",
        ),
    )?;
    println!("  written. Wait ~{}s, then switch to Obsidian.", args.delay);
    std::thread::sleep(Duration::from_secs(args.delay.min(30)));
    answers.push((
        "3a. Note shows 'Step 3' after returning to Obsidian".into(),
        p.ask("Switch to Obsidian now. Does the open note show 'Step 3'?")
            .into(),
    ));
    answers.push((
        "3b. File created while in background appeared".into(),
        p.ask(&format!(
            "Did `{}` appear in the file list?",
            NOTE_BG.trim_end_matches(".md")
        ))
        .into(),
    ));

    // ---- Check 2: inotify sees Obsidian's saves ----
    let before = vaultfs::read_state(&vault.join(NOTE))?.map(|s| s.hash);
    let watcher = start_watcher(&vault)?;
    let (watched_dirs, watch_errors) = (watcher.watched_dirs, watcher.watch_errors.clone());
    println!(
        "\n  inotify: watching {} directories ({} could not be watched)",
        watcher.watched_dirs,
        watcher.watch_errors.len()
    );
    let instructions = format!(
        "Step 4. In Obsidian: type a line into `{}` and wait 3 seconds; create a new note and type something; rename that note; delete it. Then come back here.",
        NOTE.trim_end_matches(".md")
    );
    match args.watch_secs {
        Some(s) => {
            println!("\n{instructions}\n  (watching for {s}s)");
            std::thread::sleep(Duration::from_secs(s));
        }
        None => p.enter(&instructions),
    }
    let events = watcher.finish();
    let after = vaultfs::read_state(&vault.join(NOTE))?.map(|s| s.hash);
    let note_changed = before != after;

    // Summarise the events.
    let probe_events: Vec<&Ev> = events.iter().filter(|e| e.path == NOTE).collect();
    let saw_note_write = probe_events
        .iter()
        .any(|e| e.mask.contains("CLOSE_WRITE") || e.mask.contains("MOVED_TO") || e.mask.contains("MODIFY"));
    let save_style = if probe_events.iter().any(|e| e.mask.contains("MOVED_TO")) {
        "temp file + rename"
    } else if probe_events.iter().any(|e| e.mask.contains("MODIFY")) {
        "in-place write"
    } else {
        "unknown"
    };
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for e in &events {
        for k in e.mask.split('|') {
            *kinds.entry(k.to_string()).or_default() += 1;
        }
    }
    let outside_obsidian_dir = events.iter().filter(|e| !e.path.starts_with(".obsidian")).count();
    println!(
        "  inotify: {} events ({} outside .obsidian/), {} for {NOTE}",
        events.len(),
        outside_obsidian_dir,
        probe_events.len()
    );
    println!("  note content changed on disk: {note_changed}; inotify saw the save: {saw_note_write}");

    // ---- Report ----
    let mut r = String::new();
    let _ = writeln!(r, "# lsync probe report\n");
    let _ = writeln!(r, "- time (unix): {}", stamp());
    let _ = writeln!(r, "- lsync: {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(r, "- vault: `{}`", vault.display());
    let _ = writeln!(r, "- mount: `{}`", mount_info(&vault));
    if let Some(v) = getprop("ro.build.version.release") {
        let _ = writeln!(
            r,
            "- device: {} {} (Android {v}, SDK {})",
            getprop("ro.product.manufacturer").unwrap_or_default(),
            getprop("ro.product.model").unwrap_or_default(),
            getprop("ro.build.version.sdk").unwrap_or_default()
        );
    }
    let _ = writeln!(r, "\n## Filesystem\n");
    for (k, v) in &fs_facts {
        let _ = writeln!(r, "- {k}: {v}");
    }
    let _ = writeln!(r, "\n## Check 1: Obsidian picks up outside writes\n");
    for (q, a) in &answers {
        let _ = writeln!(r, "- {q}: **{a}**");
    }
    let _ = writeln!(r, "- seconds spent in step 2: {open_secs}");
    let pass1 = answers
        .iter()
        .filter(|(q, _)| q.starts_with("2a") || q.starts_with("2c") || q.starts_with("3a") || q.starts_with("3b"))
        .all(|(_, a)| a == "yes");
    let _ = writeln!(
        r,
        "- **result: {}**",
        if pass1 { "PASS" } else { "FAIL or inconclusive" }
    );
    let _ = writeln!(r, "\n## Check 2: inotify sees Obsidian's saves\n");
    let _ = writeln!(
        r,
        "- directories watched: {watched_dirs} (one inotify watch each; {} failed)",
        watch_errors.len()
    );
    for e in watch_errors.iter().take(20) {
        let _ = writeln!(r, "  - watch failed: {e}");
    }
    let _ = writeln!(
        r,
        "- events: {} total, {} outside `.obsidian/`",
        events.len(),
        outside_obsidian_dir
    );
    let _ = writeln!(r, "- event kinds: {kinds:?}");
    let _ = writeln!(r, "- `{NOTE}` changed on disk: {note_changed}");
    let _ = writeln!(
        r,
        "- inotify events for `{NOTE}`: {} (save style: {save_style})",
        probe_events.len()
    );
    let pass2 = note_changed && saw_note_write;
    let _ = writeln!(
        r,
        "- **result: {}**",
        if pass2 {
            "PASS"
        } else if !note_changed {
            "inconclusive (the note was not edited)"
        } else {
            "FAIL"
        }
    );
    let _ = writeln!(r, "\n### Event log (first 300)\n\n```");
    for e in events.iter().take(300) {
        let _ = writeln!(
            r,
            "{:>7}ms {:<28} {}{}",
            e.t_ms,
            e.mask,
            e.path,
            if e.cookie != 0 {
                format!(" (cookie {})", e.cookie)
            } else {
                String::new()
            }
        );
    }
    let _ = writeln!(r, "```");

    let report_path = args
        .report
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("lsync-probe-report-{}.md", stamp())));
    std::fs::write(&report_path, &r).with_context(|| format!("writing {}", report_path.display()))?;
    println!("\nreport written to {}", report_path.display());
    println!(
        "check 1 (Obsidian sees outside writes): {}",
        if pass1 { "PASS" } else { "FAIL/inconclusive" }
    );
    println!(
        "check 2 (inotify sees Obsidian saves):  {}",
        if pass2 { "PASS" } else { "FAIL/inconclusive" }
    );

    if !args.keep {
        for n in [NOTE, NOTE_OPEN, NOTE_BG] {
            let _ = std::fs::remove_file(vault.join(n));
        }
        println!("probe notes removed");
    }
    Ok(if pass1 && pass2 { 0 } else { 4 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_vaults_with_livesync_enabled() {
        let d = tempfile::tempdir().unwrap();
        let v = d.path();
        assert!(livesync_enabled(v).is_none(), "no .obsidian at all");
        std::fs::create_dir_all(v.join(".obsidian/plugins/obsidian-livesync")).unwrap();
        assert!(livesync_enabled(v).is_none(), "installed, but no plugin list");
        let list = v.join(".obsidian/community-plugins.json");
        std::fs::write(&list, r#"["dataview"]"#).unwrap();
        assert!(livesync_enabled(v).is_none(), "installed but disabled");
        std::fs::write(&list, r#"["dataview", "obsidian-livesync"]"#).unwrap();
        assert!(livesync_enabled(v).unwrap().contains("obsidian-livesync"));
        std::fs::write(&list, "not json").unwrap();
        assert!(livesync_enabled(v).is_some(), "unreadable list fails closed");

        // `run` stops before writing anything.
        std::fs::write(&list, r#"["obsidian-livesync"]"#).unwrap();
        let err = run(ProbeArgs {
            vault: v.to_path_buf(),
            report: Some(v.join("report.md")),
            delay: 0,
            watch_secs: Some(0),
            keep: false,
        })
        .unwrap_err();
        assert!(err.to_string().contains("LiveSync plugin is enabled"), "{err}");
        assert!(!v.join(NOTE).exists());
        assert!(!v.join(".lsync-probe-fs").exists());

        // Listed but not installed: fine.
        std::fs::remove_dir_all(v.join(".obsidian/plugins")).unwrap();
        assert!(livesync_enabled(v).is_none());
    }
}
