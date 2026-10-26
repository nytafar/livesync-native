//! `lsync`: Linux/Termux CLI over the LiveSync engine (M1: read-only).

mod config;
mod probe;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::{FileConfig, Overrides};
use livesync_engine::{engine::status_from_index, Engine, EngineConfig, EngineError, Index};
use livesync_remote::Couch;
use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "lsync", version, about = "Self-hosted LiveSync client (read-only in M1)")]
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
    /// One-shot pull of the remote into the vault folder
    Pull {
        /// Print the report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Follow the remote continuously
    Daemon {
        /// Only pull (required in M1; two-way sync arrives in M2)
        #[arg(long)]
        pull_only: bool,
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

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
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
            let engine = engine_for(&cfg, &cli.profile)?;
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
                        println!("  our node id     {} (not registered: read-only)", g.node_id);
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
            let engine = engine_for(&cfg, &cli.profile)?;
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
        Cmd::Daemon { pull_only } => {
            if !pull_only {
                bail!("two-way sync arrives in M2; run `lsync daemon --pull-only`");
            }
            rt()?.block_on(async {
                let engine = engine_for(&cfg, &cli.profile)?;
                let (tx, rx) = tokio::sync::watch::channel(false);
                tokio::spawn(async move {
                    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {},
                        _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending::<()>().await } } => {},
                    }
                    info!("shutting down");
                    let _ = tx.send(true);
                });
                engine.daemon_pull_only(rx).await.context("daemon")?;
                Ok(0)
            })
        }
    }
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

fn engine_for(cfg: &FileConfig, profile: &str) -> Result<Engine> {
    let r = config::resolve(cfg, profile, true)?;
    let couch = Couch::new(&r.remote)?;
    let mut ec = EngineConfig::new(absolute(&r.vault)?, r.state_dir.join("index.sqlite"));
    ec.ignore_regex = r.ignore_regex;
    ec.max_size_mb = r.max_size_mb;
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
    if !s.pending.is_empty() {
        println!("pending (will retry):");
        for p in &s.pending {
            println!("  {}  — {} (attempts {})", p.doc_id, p.reason, p.attempts);
        }
    }
}
