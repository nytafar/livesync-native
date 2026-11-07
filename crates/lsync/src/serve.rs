//! `lsync serve` (every enabled vault in `lsync.toml`, or one) and
//! `lsync daemon` (one vault, from any settings): one process, one runtime,
//! one log, the control socket (spec D6, D7).
//!
//! Signals: SIGHUP rereads the config (as the `reload` method does),
//! SIGUSR1 is "sync now" for every vault, SIGTERM and SIGINT stop.

use crate::config::Setup;
use anyhow::{anyhow, bail, Result};
use livesync_host::control::{self, BindError, Kind, Policy};
use livesync_host::service::{ConfigSource, FixedSource, Planned, Service, Source};
use livesync_host::{Host, RunConfig};
use std::sync::Arc;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tracing::{info, warn};

/// `lsync serve`'s own options.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct ServeOpts {
    /// Run as a system service: the socket in $RUNTIME_DIRECTORY or
    /// /run/lsync instead of the user's runtime folder
    #[arg(long)]
    pub system: bool,
    /// With --system: also let the members of this group use the control
    /// socket (the directory becomes 0750 and the socket 0660, both owned by
    /// the group)
    #[arg(long, value_name = "GROUP", requires = "system")]
    pub socket_group: Option<String>,
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("lsync")
        .enable_all()
        .build()?)
}

async fn recv(s: &mut Option<Signal>) {
    match s {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Run until SIGTERM or SIGINT, reloading on SIGHUP and nudging on SIGUSR1.
async fn signals(svc: &Arc<Service>) {
    let mut term = signal(SignalKind::terminate()).ok();
    let mut hup = signal(SignalKind::hangup()).ok();
    let mut usr1 = signal(SignalKind::user_defined1()).ok();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = recv(&mut term) => break,
            _ = recv(&mut hup) => {
                info!("SIGHUP: rereading the config");
                let s = svc.clone();
                // On its own task: a reload may wait for a running command.
                tokio::spawn(async move {
                    if let Err(e) = s.reload().await {
                        warn!("reload: {e}");
                    }
                });
            }
            _ = recv(&mut usr1) => {
                info!("SIGUSR1: reconnecting and rescanning");
                let _ = svc.sync_now(None);
            }
        }
    }
    info!("shutting down");
}

/// Serve the socket (when bound) and the vaults until a stop signal.
async fn run(svc: Arc<Service>, bound: Option<control::Bound>, policy: Policy) {
    let activity = svc.spawn_activity();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let server = bound.map(|b| tokio::spawn(control::serve(b, svc.clone(), policy, stop_rx)));
    // Where a `Type=notify` unit gets READY=1 (P1): every vault is started
    // and the socket accepts.
    let running = svc.host().active();
    info!(
        "ready: {} of {} vault(s) started: {}",
        running.len(),
        svc.names().len(),
        running.join(", ")
    );
    signals(&svc).await;
    let _ = stop_tx.send(true);
    if let Some(s) = server {
        let _ = s.await;
    }
    svc.shutdown().await;
    activity.abort();
}

fn policy(opts: &ServeOpts) -> Result<Policy> {
    let mut p = Policy::own_user();
    if let Some(g) = &opts.socket_group {
        p.group = Some(control::group_id(g).map_err(|e| anyhow!(e))?);
    }
    Ok(p)
}

/// `lsync serve [--vault NAME]`.
pub fn serve(ctx: &Setup, only: Option<String>, opts: &ServeOpts, version: &str) -> Result<u8> {
    if !ctx.config_file.exists() {
        bail!(
            "`lsync serve` runs the vaults in {}, and there is none; add one with `lsync vault add` (or run one vault from flags with `lsync daemon`)",
            ctx.config_file.display()
        );
    }
    if !ctx.is_accounts_file()? {
        bail!(
            "{} is a profile file; `lsync serve` runs the vaults of an lsync.toml. Run it with `lsync daemon`",
            ctx.config_file.display()
        );
    }
    let path = std::path::absolute(&ctx.config_file)?;
    let source = ConfigSource {
        path,
        dirs: ctx.dirs.clone(),
        secrets: ctx.secrets(),
        only: only.clone(),
    };
    // Refuse before anything starts when the config is unusable, or has no
    // such vault (later, a vault gone from the file is stopped instead).
    let vaults = source.load().map_err(|e| anyhow!(e))?;
    if let Some(n) = &only {
        if !vaults.iter().any(|v| &v.name == n) {
            bail!("no vault named {n:?} in {}", ctx.config_file.display());
        }
    }
    let kind = if opts.system { Kind::System } else { Kind::User };
    let policy = policy(opts)?;
    let dir = control::service_dir(kind).map_err(|e| anyhow!(e))?;
    let bound = control::bind(&dir, &policy)?;
    let rt = runtime()?;
    rt.block_on(async {
        let host = Arc::new(Host::on_runtime(tokio::runtime::Handle::current()));
        host.allow_hidden_state();
        let svc = Service::new(host, Box::new(source), version);
        let report = svc.reload().await.map_err(|e| anyhow!(e.message))?;
        for f in &report.failed {
            warn!("vault {}: not running: {}", f.vault, f.message);
        }
        if svc.names().is_empty() {
            warn!("no vaults in the config yet; `lsync vault add` adds one, and this service picks it up");
        }
        run(svc, Some(bound), policy).await;
        Ok(0)
    })
}

/// `lsync daemon`: one vault, whatever its settings came from. Its first
/// start must succeed (or the command fails, as it always did); after that
/// the vault is supervised like the service's. The control socket is taken
/// when it is free; when another service holds it, the daemon runs without.
pub fn daemon(planned: Planned, version: &str) -> Result<u8> {
    let name = planned.name.clone();
    let bound = match control::service_dir(Kind::User).map_err(BindError::Unsafe) {
        Ok(dir) => match control::bind(&dir, &Policy::own_user()) {
            Ok(b) => Some(b),
            Err(e @ BindError::Running { .. }) => {
                info!("{e}; this daemon runs without a control socket");
                None
            }
            Err(e) => {
                warn!("no control socket: {e}");
                None
            }
        },
        Err(e) => {
            info!("no control socket: {e}");
            None
        }
    };
    let source = FixedSource {
        vaults: vec![planned],
        description: "the command line (lsync daemon)".into(),
    };
    let rt = runtime()?;
    rt.block_on(async {
        let host = Arc::new(Host::on_runtime(tokio::runtime::Handle::current()));
        host.allow_hidden_state();
        let svc = Service::new(host, Box::new(source), version);
        svc.reload().await.map_err(|e| anyhow!(e.message))?;
        if let Err(e) = svc.wait_started(&name).await {
            svc.shutdown().await;
            bail!("daemon: {e}");
        }
        run(svc, bound, Policy::own_user()).await;
        Ok(0)
    })
}

/// The one vault a daemon runs, from settings resolved by the CLI.
pub fn planned(cfg: RunConfig) -> Planned {
    Planned {
        name: cfg.id.clone(),
        account: None,
        enabled: true,
        folder: cfg.vault_path.clone(),
        state_dir: cfg.state_dir.clone(),
        run: Ok(cfg),
    }
}
