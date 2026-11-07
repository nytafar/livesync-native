//! `lsync adopt [--dry-run]` (spec D15): take a folder over that another
//! client kept in sync, before its first sync here (`Engine::adopt`).
//!
//! A dry run works on a throwaway index, so the vault's own state folder is
//! not created or touched; it refuses, as a real run does, a vault whose
//! index already tracks files. A real run records the identical files and
//! then runs the first sync (a pull for a pull-only vault).

use crate::config::{FileConfig, Selected};
use crate::PushOpts;
use anyhow::{bail, Result};
use livesync_engine::{AdoptEntry, AdoptReport, EngineError, Index};
use livesync_host::config::{Dirs, Mode};
use serde_json::json;
use std::path::PathBuf;
use std::time::Instant;

pub struct Opts {
    pub dry_run: bool,
    pub json: bool,
    pub allow_plugin: bool,
    pub push: PushOpts,
}

/// A folder removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How many entries of a long list the text output shows.
const SHOWN: usize = 20;

pub async fn run(sel: &Selected, cfg: &FileConfig, dirs: &Dirs, o: Opts) -> Result<u8> {
    let r = crate::config::resolve(sel, cfg, dirs, true)?;
    let folder = crate::absolute(&r.vault)?;
    if !folder.is_dir() {
        bail!(
            "the vault folder {} does not exist; adopt takes over a folder that is there",
            folder.display()
        );
    }
    let plugin = crate::probe::livesync_enabled(&folder);
    if let (Some(why), false, false) = (&plugin, o.dry_run, o.allow_plugin) {
        bail!(
            "the LiveSync plugin is still enabled in this folder ({why}); two clients would sync it. Disable the plugin on this device, or give --allow-plugin for a deliberate handover"
        );
    }
    let index_path = r.state_dir.join("index.sqlite");
    if index_path.exists() {
        let tracked = Index::open(&index_path)?.all_rows()?.len();
        if tracked > 0 {
            bail!(
                "this vault's index ({}) already tracks {tracked} files: adopt is for a folder's first sync, and this one has had it (use `lsync sync`)",
                index_path.display()
            );
        }
    }
    let pull_only = sel.mode == Mode::PullOnly;
    let t0 = Instant::now();
    let scratch = o.dry_run.then(|| {
        Scratch(std::env::temp_dir().join(format!(
            "lsync-adopt-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        )))
    });
    let engine = crate::engine_with_index(
        sel,
        cfg,
        dirs,
        &o.push,
        scratch.as_ref().map(|s| s.0.join("index.sqlite")),
    )?;
    let rep = match engine.adopt(!o.dry_run).await {
        Ok(r) => r,
        Err(EngineError::Refused(r)) => {
            eprintln!("adopt refused: {r}");
            return Ok(2);
        }
        Err(e) => return Err(e.into()),
    };
    let refusal = rep.refusal();
    if o.dry_run || !rep.applied {
        drop(engine);
        if o.json {
            let mut v = serde_json::to_value(&rep)?;
            v["dry_run"] = o.dry_run.into();
            v["refusal"] = refusal.clone().into();
            v["plugin_enabled"] = plugin.clone().into();
            v["pull_only"] = pull_only.into();
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            print_report(&rep, &folder, o.dry_run, pull_only);
            if let Some(why) = &plugin {
                println!("WARNING: the LiveSync plugin is still enabled here ({why}); disable it on this device before the real adopt");
            }
            match &refusal {
                Some(why) if o.dry_run => println!("a real adopt would stop: {why}"),
                Some(why) => println!("NOT ADOPTED: {why}"),
                None => println!("nothing was changed (dry run)"),
            }
        }
        return Ok(if refusal.is_some() { 2 } else { 0 });
    }
    // Taken over: the first sync, with the engine that holds the folder.
    let first = if pull_only {
        engine.pull_once().await.map(|p| json!({ "pull": p }))
    } else {
        engine
            .sync_once()
            .await
            .map(|s| serde_json::to_value(s).unwrap_or_default())
    };
    let first = match first {
        Ok(v) => v,
        Err(EngineError::Refused(r)) => {
            eprintln!("adopted {} files, but the first sync was refused: {r}", rep.identical);
            return Ok(2);
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "adopted {} files, but the first sync failed: {e}",
                rep.identical
            ))
        }
    };
    let wall = t0.elapsed();
    if o.json {
        let mut v = serde_json::to_value(&rep)?;
        v["dry_run"] = false.into();
        v["pull_only"] = pull_only.into();
        v["first_sync"] = first;
        v["wall_ms"] = (wall.as_millis() as u64).into();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    print_report(&rep, &folder, false, pull_only);
    println!("adopted {} files; the first sync:", rep.identical);
    if pull_only {
        let p: livesync_engine::PullReport = serde_json::from_value(first["pull"].clone())?;
        crate::print_pull(&p, wall, crate::peak_rss_kib(), false)
    } else {
        let s: livesync_engine::SyncReport = serde_json::from_value(first)?;
        crate::print_sync(&s, wall, crate::peak_rss_kib(), false)
    }
}

fn mb(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1e6)
}

fn size(n: Option<u64>) -> String {
    n.map_or("-".into(), |n| n.to_string())
}

fn list(title: &str, l: &[AdoptEntry], all: bool) {
    if l.is_empty() {
        return;
    }
    println!("{title}:");
    let n = if all { l.len() } else { l.len().min(SHOWN) };
    for e in &l[..n] {
        let mut line = format!("  {}", e.path);
        if let Some(lp) = &e.local_path {
            line.push_str(&format!(" (here: {lp})"));
        }
        if e.local_size.is_some() || e.remote_size.is_some() {
            line.push_str(&format!(
                "  [here {} B, server {} B]",
                size(e.local_size),
                size(e.remote_size)
            ));
        }
        if let Some(note) = &e.note {
            line.push_str(&format!("  {note}"));
        }
        println!("{line}");
    }
    if n < l.len() {
        println!("  ... and {} more (--json lists them all)", l.len() - n);
    }
}

fn print_report(rep: &AdoptReport, folder: &std::path::Path, dry_run: bool, pull_only: bool) {
    println!("adopt{}: {}", if dry_run { " (dry run)" } else { "" }, folder.display());
    println!("  server files      {}", rep.remote_files);
    println!("  local files       {}", rep.local_files);
    println!(
        "  identical         {}  (adopted as they are; {} compared by content)",
        rep.identical, rep.identical_by_content
    );
    let (differ, local) = if pull_only {
        (
            "kept here; the server's version is not applied",
            "kept here, never pushed (pull-only)",
        )
    } else {
        ("conflicts with no common ancestor at the first sync", "pushed as new")
    };
    println!("  differing         {}  ({differ})", rep.differing.len());
    println!("  only here         {}  ({local})", rep.only_local.len());
    println!("  only on server    {}  (written here)", rep.only_remote.len());
    println!("  deleted on server {}  (see below)", rep.deleted_remotely.len());
    if !rep.conflicted_remotely.is_empty() {
        println!(
            "  conflicted        {}  (already on the server)",
            rep.conflicted_remotely.len()
        );
    }
    if !rep.undecided.is_empty() {
        println!("  undecided         {}  (see below)", rep.undecided.len());
    }
    if !rep.not_synced.is_empty() {
        println!("  not synced        {}", rep.not_synced.len());
    }
    if !rep.skipped_remote.is_empty() {
        let s: Vec<String> = rep.skipped_remote.iter().map(|(k, v)| format!("{k} {v}")).collect();
        println!("  skipped on server {}", s.join(", "));
    }
    println!(
        "  read              {} from the server, {} chunks ({}) to compare, in {:.1}s",
        mb(rep.bytes_fetched),
        rep.chunks_fetched,
        mb(rep.chunk_bytes),
        rep.elapsed_ms as f64 / 1000.0
    );
    // The differing files are the conflicts the first sync opens: all of them.
    list("differing (each becomes a conflict)", &rep.differing, true);
    list("name differs only in case", &rep.name_case, true);
    list("deleted on the server, still here", &rep.deleted_remotely, true);
    list("could not be read", &rep.unreadable, true);
    list("undecided", &rep.undecided, true);
    list("only here", &rep.only_local, false);
    list("only on the server", &rep.only_remote, false);
    list("not synced", &rep.not_synced, false);
    if !rep.conflicted_remotely.is_empty() {
        println!("already conflicted on the server:");
        for p in rep.conflicted_remotely.iter().take(SHOWN) {
            println!("  {p}");
        }
        if rep.conflicted_remotely.len() > SHOWN {
            println!("  ... and {} more", rep.conflicted_remotely.len() - SHOWN);
        }
    }
}
