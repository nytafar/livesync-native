//! `lsync`: Linux/Termux CLI over the LiveSync engine.

mod config;
mod probe;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::{FileConfig, Overrides};
use livesync_engine::{engine::status_from_index, Engine, EngineConfig, EngineError, Index, SyncReport};
use livesync_remote::Couch;
use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "lsync", version, about = "Self-hosted LiveSync client")]
struct Cli {
    /// Profile name (selects the config file and state directory)
    #[arg(long, short = 'p', env = "LSYNC_PROFILE", default_value = "default", global = true)]
    profile: String,
    /// Config file (default: $XDG_CONFIG_HOME/lsync/<profile>.toml)
    #[arg(long, env = "LSYNC_CONFIG", global = true)]
    config: Option<PathBuf>,
    #[command(flatten)]
    overrides: Overrides,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Save remote and vault settings to the profile's config file (mode 0600)
    Configure {
        /// Read the password from stdin instead of --password/LSYNC_PASSWORD
        #[arg(long)]
        password_stdin: bool,
    },
    /// Run the connect gate against the remote and print the result
    Connect {
        /// Accept changed PREFERRED tweak values on the remote
        #[arg(long)]
        adopt_tweaks: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// One-shot pull of the remote into the vault folder (never writes to the remote)
    Pull {
        /// Print the report as JSON
        #[arg(long)]
        json: bool,
    },
    /// One-shot two-way sync: pull, register this device, push local changes
    Sync {
        /// Print the report as JSON
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        push: PushOpts,
    },
    /// Start over from the remote (upstream's "Fetch"), then sync; clears a
    /// lock that came without a rebuild
    Fetch {
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        push: PushOpts,
    },
    /// Follow the remote continuously and push local changes as they happen
    Daemon {
        /// Only pull; never write to the remote
        #[arg(long)]
        pull_only: bool,
        #[command(flatten)]
        push: PushOpts,
    },
    /// Show index state, last gate result and files needing attention
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Interactive on-device check: does Obsidian see outside writes, and does
    /// inotify see Obsidian's saves?
    Probe(probe::ProbeArgs),
}

#[derive(clap::Args, Debug, Clone, Default)]
struct PushOpts {
    /// Allow one pass to delete more than half of the tracked files remotely
    #[arg(long)]
    allow_mass_delete: bool,
    /// When the folder scope narrows, remove the clean local copies that fell out of it
    #[arg(long)]
    remove_out_of_scope_copies: bool,
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let sub = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .finish();
    tracing::subscriber::set_global_default(sub).expect("a tracing subscriber is already set");
    // Our own `log` bridge: it passes known-harmless library warnings on at debug.
    livesync_engine::logbridge::init().expect("a log logger is already set");
}

fn main() -> std::process::ExitCode {
    init_logging();
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::ExitCode::from(code),
        Err(e) => {
            error!("{e:#}");
            std::process::ExitCode::from(1)
        }
    }
}

/// Peak resident set size of this process in KiB (Linux and Android).
pub fn peak_rss_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find(|l| l.starts_with("VmHWM:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

fn run(cli: Cli) -> Result<u8> {
    let cfg_path = cli
        .config
        .clone()
        .unwrap_or_else(|| config::default_config_path(&cli.profile));
    let file_cfg = config::load(&cfg_path)?;
    let cfg = file_cfg.clone().merged(&cli.overrides);

    match cli.cmd {
        Cmd::Configure { password_stdin } => {
            let mut new = cfg;
            if password_stdin {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                new.password = Some(s.trim_end_matches(['\n', '\r']).to_string());
            }
            if let Some(v) = &new.vault {
                new.vault = Some(absolute(v)?);
            }
            config::save(&cfg_path, &new)?;
            println!("saved {}", cfg_path.display());
            print_config(&new);
            Ok(0)
        }
        Cmd::Status { json } => {
            let r = config::resolve(&cfg, &cli.profile, false)?;
            let index_path = r.state_dir.join("index.sqlite");
            if !index_path.exists() {
                bail!("no index at {} yet; run `lsync pull` first", index_path.display());
            }
            let ix = Index::open(&index_path)?;
            let st = status_from_index(&ix, &r.vault, &index_path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                print_status(&st);
            }
            Ok(0)
        }
        Cmd::Probe(args) => probe::run(args),
        Cmd::Connect { adopt_tweaks, json } => rt()?.block_on(async {
            let engine = engine_for(&cfg, &cli.profile, &PushOpts::default())?;
            match engine.connect_gate(adopt_tweaks).await {
                Ok(g) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&g)?);
                    } else {
                        println!("gate: OK");
                        println!("  remote          {}", g.remote);
                        println!("  version doc     {}", g.version);
                        println!("  documents       {}", g.doc_count);
                        println!("  created         {}", g.created);
                        println!("  locked          {}", g.locked);
                        println!("  accepted nodes  {}", g.accepted_nodes);
                        println!("  our node id     {}", g.node_id);
                        if g.fetch_to_resolve {
                            println!("  lock            locked against us; a full fetch will mark us resolved");
                        }
                        println!(
                            "  tweaks adopted  {}",
                            if g.adopted_tweaks_now { "now" } else { "unchanged" }
                        );
                        println!("  rebuild seen    {}", g.rebuild_detected);
                        println!("  salt present    {}", g.salt_present);
                        println!("  max file size   {} MB", g.max_size_mb);
                    }
                    Ok(0)
                }
                Err(EngineError::Refused(r)) => {
                    eprintln!("gate: REFUSED\n  {r}");
                    Ok(2)
                }
                Err(e) => Err(e.into()),
            }
        }),
        Cmd::Pull { json } => rt()?.block_on(async {
            let engine = engine_for(&cfg, &cli.profile, &PushOpts::default())?;
            let t0 = Instant::now();
            let rep = match engine.pull_once().await {
                Ok(r) => r,
                Err(EngineError::Refused(r)) => {
                    eprintln!("pull refused: {r}");
                    return Ok(2);
                }
                Err(e) => return Err(e.into()),
            };
            let wall = t0.elapsed();
            let rss = peak_rss_kib();
            if json {
                let mut v = serde_json::to_value(&rep)?;
                v["wall_ms"] = (wall.as_millis() as u64).into();
                v["peak_rss_kib"] = rss.into();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("pull complete in {:.1}s", wall.as_secs_f64());
                println!("  changes seen     {}", rep.changes);
                println!(
                    "  files written    {} ({:.1} MB)",
                    rep.written,
                    rep.bytes_written as f64 / 1e6
                );
                println!("  already on disk  {}", rep.adopted);
                println!("  unchanged        {}", rep.unchanged);
                println!("  deleted          {}", rep.deleted);
                println!("  dirty (kept)     {}", rep.dirty);
                println!("  conflicted       {}", rep.conflicted);
                println!("  pending (retry)  {}", rep.pending);
                println!("  skipped          {:?}", rep.skipped);
                println!("  other docs       {}", rep.ignored_docs);
                println!(
                    "  chunks fetched   {} ({:.1} MB)",
                    rep.chunks_fetched,
                    rep.chunk_bytes as f64 / 1e6
                );
                println!("  size mismatches  {}", rep.size_mismatches);
                println!("  chunk-id misses  {}", rep.chunk_id_mismatches);
                if let Some(k) = rss {
                    println!("  peak RSS         {:.1} MB", k as f64 / 1024.0);
                }
            }
            Ok(if rep.dirty > 0 || rep.pending > 0 { 3 } else { 0 })
        }),
        Cmd::Sync { json, push } => rt()?.block_on(async {
            let engine = engine_for(&cfg, &cli.profile, &push)?;
            run_sync(&engine, json, false).await
        }),
        Cmd::Fetch { json, push } => rt()?.block_on(async {
            let engine = engine_for(&cfg, &cli.profile, &push)?;
            run_sync(&engine, json, true).await
        }),
        Cmd::Daemon { pull_only, push } => rt()?.block_on(async {
            let engine = engine_for(&cfg, &cli.profile, &push)?;
            let (tx, rx) = tokio::sync::watch::channel(false);
            let nudger = engine.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{signal, SignalKind};
                let mut term = signal(SignalKind::terminate()).ok();
                let mut usr1 = signal(SignalKind::user_defined1()).ok();
                loop {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => break,
                        _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending::<()>().await } } => break,
                        // `kill -USR1`: reconnect now and rescan the vault ("sync now").
                        _ = async { match usr1.as_mut() { Some(u) => { u.recv().await; } None => std::future::pending::<()>().await } } => {
                            info!("SIGUSR1: reconnecting and rescanning");
                            nudger.network_changed(true);
                            continue;
                        }
                    }
                }
                info!("shutting down");
                let _ = tx.send(true);
            });
            if pull_only {
                engine.daemon_pull_only(rx).await.context("daemon")?;
            } else {
                engine.run(rx).await.context("daemon")?;
            }
            Ok(0)
        }),
    }
}

async fn run_sync(engine: &Engine, json: bool, fetch: bool) -> Result<u8> {
    if fetch {
        engine.request_full_fetch()?;
    }
    let t0 = Instant::now();
    let rep: SyncReport = match engine.sync_once().await {
        Ok(r) => r,
        Err(EngineError::Refused(r)) => {
            eprintln!("sync refused: {r}");
            return Ok(2);
        }
        Err(e) => return Err(e.into()),
    };
    let wall = t0.elapsed();
    if json {
        let mut v = serde_json::to_value(&rep)?;
        v["wall_ms"] = (wall.as_millis() as u64).into();
        v["peak_rss_kib"] = peak_rss_kib().into();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        let (p, u) = (&rep.pull, &rep.push);
        println!("sync complete in {:.1}s", wall.as_secs_f64());
        println!(
            "  pulled           {} changes, {} written, {} deleted, {} adopted",
            p.changes, p.written, p.deleted, p.adopted
        );
        println!(
            "  pushed           {} ({} new), {} adopted",
            u.pushed, u.created, u.adopted
        );
        println!(
            "  chunks uploaded  {} ({:.1} MB)",
            u.chunks_uploaded,
            u.chunk_bytes as f64 / 1e6
        );
        println!("  deleted remotely {}", u.deleted);
        println!(
            "  conflicts        {} pulled, {} pushed as branches",
            p.conflicted, u.conflicts
        );
        println!("  kept remote      {} (local deletes not applied)", u.kept_remote);
        if u.deleted_locally > 0 {
            println!(
                "  removed locally  {} (stale copies of notes deleted elsewhere)",
                u.deleted_locally
            );
        }
        if u.orphans_pushed > 0 {
            println!(
                "  pushed back      {} the remote lacked after a rebuild or fetch (see `lsync status`)",
                u.orphans_pushed
            );
        }
        if u.failed > 0 {
            println!("  failed           {} (see local issues in `lsync status`)", u.failed);
        }
        if u.unreadable > 0 {
            println!("  unreadable       {} (not treated as deleted)", u.unreadable);
        }
        println!("  out of scope     {} (moved out locally; remote kept)", u.out_of_scope);
        println!("  dirty (kept)     {}", p.dirty);
        println!("  pending (retry)  {}", p.pending);
        println!("  local issues     {}", u.issues);
        if u.safety_stops > 0 {
            println!("  SAFETY STOP      too many files missing; nothing deleted (see `lsync status`)");
        }
    }
    let attention = rep.pull.dirty > 0
        || rep.pull.pending > 0
        || rep.push.issues > 0
        || rep.push.safety_stops > 0
        || rep.push.failed > 0
        || rep.push.orphans_pushed > 0;
    Ok(if attention { 3 } else { 0 })
}

fn rt() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?)
}

fn absolute(p: &std::path::Path) -> Result<PathBuf> {
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    })
}

fn engine_for(cfg: &FileConfig, profile: &str, push: &PushOpts) -> Result<Engine> {
    // After a rebuild elsewhere, how long the remote must stay unchanged
    // before the refetch counts as complete (default 30 s; tests shorten it).
    let quiet = std::env::var("LSYNC_REBUILD_QUIET_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let r = config::resolve(cfg, profile, true)?;
    let couch = Couch::new(&r.remote)?;
    let mut ec = EngineConfig::new(absolute(&r.vault)?, r.state_dir.join("index.sqlite"));
    ec.ignore_regex = r.ignore_regex;
    ec.max_size_mb = r.max_size_mb;
    ec.scope = r.scope;
    ec.device_name = r.device_name;
    ec.allow_mass_delete = push.allow_mass_delete;
    ec.remove_out_of_scope_copies = push.remove_out_of_scope_copies;
    if let Some(q) = quiet {
        ec.rebuild_quiet = std::time::Duration::from_secs(q);
    }
    if let Some(c) = r.concurrency {
        ec.concurrency = c.max(1);
    }
    if ec.index_path.starts_with(&ec.vault)
        && !ec
            .index_path
            .strip_prefix(&ec.vault)?
            .to_string_lossy()
            .starts_with('.')
    {
        bail!("the state directory must not be a visible folder inside the vault");
    }
    Ok(Engine::open(couch, ec)?)
}

fn print_config(c: &FileConfig) {
    let show = |o: &Option<String>| o.clone().unwrap_or_else(|| "-".into());
    println!("  url        {}", show(&c.url));
    println!("  database   {}", show(&c.database));
    println!("  user       {}", show(&c.username));
    println!("  password   {}", if c.password.is_some() { "(set)" } else { "-" });
    println!(
        "  vault      {}",
        c.vault.as_ref().map(|p| p.display().to_string()).unwrap_or("-".into())
    );
    println!(
        "  state dir  {}",
        c.state_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or("(default)".into())
    );
    println!("  ignore re  {}", show(&c.ignore_regex));
    match c.scope() {
        Ok(s) => println!("  folders    {}", s.describe()),
        Err(e) => println!("  folders    INVALID: {e}"),
    }
    println!("  device     {}", show(&c.device_name));
}

fn fmt_ms(ms: Option<i64>) -> String {
    match ms {
        None => "-".into(),
        Some(ms) => {
            let ago = (livesync_engine::vaultfs::now_ms() - ms) / 1000;
            format!("{ms} ({ago}s ago)")
        }
    }
}

fn print_status(s: &livesync_engine::Status) {
    println!("remote          {}", s.remote.as_deref().unwrap_or("-"));
    println!("vault           {}", s.vault);
    println!("index           {}", s.index);
    println!("node id         {}", s.node_id.as_deref().unwrap_or("-"));
    println!(
        "remote created  {}",
        s.remote_created.map(|c| c.to_string()).unwrap_or("-".into())
    );
    println!("version doc     {}", s.remote_version.as_deref().unwrap_or("-"));
    println!(
        "since           {}",
        s.since
            .as_deref()
            .map(|v| v.chars().take(40).collect::<String>())
            .unwrap_or("-".into())
    );
    println!("last gate       {}", s.last_gate.as_deref().unwrap_or("-"));
    println!("last pull       {}", fmt_ms(s.last_pull_ms));
    println!("daemon seen     {}", fmt_ms(s.daemon_heartbeat_ms));
    if let Some(r) = s.rebuild_detected_ms {
        println!("rebuild seen    {}", fmt_ms(Some(r)));
    }
    println!("last push       {}", fmt_ms(s.last_push_ms));
    println!("registered      {}", fmt_ms(s.registered_ms));
    println!("folders         {}", s.scope.as_deref().unwrap_or("-"));
    if s.needs_full_fetch {
        println!("full fetch      owed; pushes are held until a two-way sync completes it");
    }
    if let Some(w) = &s.rebuild_wait {
        println!("rebuild wait    {w}");
    }
    if let Some(o) = &s.rebuild_orphans {
        println!("pushed back     {o}");
    }
    if let Some(stop) = &s.safety_stop {
        println!("SAFETY STOP     {stop} (lsync sync --allow-mass-delete)");
    }
    println!("last error      {}", s.last_error.as_deref().unwrap_or("-"));
    println!("files           {:?}", s.counts);
    if !s.dirty.is_empty() {
        println!("dirty (local changes not overwritten):");
        for r in &s.dirty {
            println!("  {}  — {}", r.path, r.note.as_deref().unwrap_or(""));
        }
    }
    if !s.conflicted.is_empty() {
        println!("conflicted (resolve with an official client):");
        for r in &s.conflicted {
            println!("  {}  — {}", r.path, r.note.as_deref().unwrap_or(""));
        }
    }
    if !s.unwritable.is_empty() {
        println!("skipped, the vault's filesystem cannot hold the name (not retried):");
        for r in &s.unwritable {
            println!("  {}  — {}", r.path, r.note.as_deref().unwrap_or(""));
        }
    }
    if !s.collisions.is_empty() {
        println!("skipped, another file has the same name on this filesystem:");
        for r in &s.collisions {
            println!("  {}  — {}", r.path, r.note.as_deref().unwrap_or(""));
        }
    }
    if !s.out_of_scope.is_empty() {
        println!("moved out of the synced folders (remote copy kept):");
        for r in &s.out_of_scope {
            println!("  {}  — {}", r.path, r.note.as_deref().unwrap_or(""));
        }
    }
    if !s.local_issues.is_empty() {
        println!("local files not synced:");
        for i in &s.local_issues {
            println!("  {}  — {} ({})", i.path, i.note, i.kind);
        }
    }
    if !s.pending.is_empty() {
        println!("pending (will retry):");
        for p in &s.pending {
            println!("  {}  — {} (attempts {})", p.doc_id, p.reason, p.attempts);
        }
    }
}
