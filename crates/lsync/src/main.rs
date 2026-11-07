//! `lsync`: Linux/Termux CLI over the LiveSync engine.

mod adopt;
mod bar;
mod config;
mod doctor;
mod handoff;
mod import;
mod install;
mod manage;
mod probe;
mod prompt;
mod route;
mod sdnotify;
mod serve;
mod service;
mod unit;

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use config::{FileConfig, Origin, Overrides, Selected, Selector, Setup};
use livesync_engine::{
    engine::status_from_index, Confirmation, Engine, EngineConfig, EngineError, GateReport, Index, PullReport,
    SyncReport,
};
use livesync_host::config::{Dirs, Mode};
use livesync_host::control::Client;
use livesync_host::protocol::{code, method, CommandResult};
use livesync_remote::Couch;
use route::Route;
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::error;

/// The release version (set by the release workflow from the tag), else the crate version.
const VERSION: &str = match option_env!("LSYNC_RELEASE_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(name = "lsync", version = VERSION, about = "Self-hosted LiveSync client")]
struct Cli {
    /// The vault to act on, by name (see `lsync vault ls`); not needed with
    /// one vault. A folder given here is read as --folder, with a warning
    #[arg(long, global = true, value_name = "NAME")]
    vault: Option<String>,
    /// Another name for --vault; also names a profile file (<NAME>.toml)
    /// that was not migrated
    #[arg(long, short = 'p', env = "LSYNC_PROFILE", global = true, value_name = "NAME")]
    profile: Option<String>,
    /// Config file (default: $XDG_CONFIG_HOME/lsync/lsync.toml); a profile
    /// file in the format before lsync.toml is read as such
    #[arg(long, env = "LSYNC_CONFIG", global = true)]
    config: Option<PathBuf>,
    /// Work with the system service: its config ($CONFIGURATION_DIRECTORY
    /// or /etc/lsync), its state ($STATE_DIRECTORY or /var/lib/lsync) and
    /// its control socket (/run/lsync), not this user's
    #[arg(long, env = "LSYNC_SYSTEM", global = true)]
    system: bool,
    #[command(flatten)]
    overrides: Overrides,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Save the given settings (and the password, from --password,
    /// LSYNC_PASSWORD or --password-stdin) to the vault in lsync.toml and
    /// its account, or to a profile file that was not migrated
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
    /// Confirm the vault's mass-delete safety stop: the files it listed, and
    /// only those, are deleted remotely (files that went missing since are
    /// checked again). Shows the stop and asks first, unless --stop gives
    /// it. Done by the service when it runs the vault, else by a sync here
    AllowDeletes {
        /// The stop as `lsync status --json` shows it (`safety_stop`, whole);
        /// nothing is allowed if the stop shown now is another one
        #[arg(long, value_name = "TEXT")]
        stop: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Run every enabled vault in lsync.toml in this process (or one, with
    /// --vault), with the control socket other lsync commands use. SIGHUP
    /// rereads the config, SIGUSR1 syncs now
    Serve {
        #[command(flatten)]
        opts: serve::ServeOpts,
    },
    /// Run `lsync serve` as a service: install, uninstall, status, logs
    /// (systemd user or, with --system, system service; launchd on macOS)
    Service {
        #[command(subcommand)]
        cmd: install::ServiceCmd,
    },
    /// Check what the service needs: the config, each account's secret
    /// store, the vault folders' access, the inotify watch limit, and the
    /// service. Exits 1 when something is wrong
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Follow the remote continuously and push local changes as they happen
    /// (one vault, from any settings; refused for a vault `lsync serve` runs)
    Daemon {
        /// Only pull; never write to the remote
        #[arg(long)]
        pull_only: bool,
        #[command(flatten)]
        push: PushOpts,
    },
    /// List the remote's top-level folders (files and size each) against the
    /// folder scope, to choose includes or excludes. Reads file metadata only,
    /// never chunks, and never writes to the remote
    Folders {
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Show index state, last gate result and files needing attention.
    /// With --follow, --waybar or --format: one line for a status bar, from
    /// the running service (every vault, or the one named with --vault)
    Status {
        #[arg(long, conflicts_with = "format")]
        json: bool,
        /// Every vault of the running service: its state, mode and last sync
        #[arg(long)]
        all: bool,
        /// Print a new line whenever the status changes, until interrupted
        #[arg(long)]
        follow: bool,
        /// Waybar's JSON (text, alt, class, tooltip); --format=waybar
        #[arg(long, conflicts_with = "format")]
        waybar: bool,
        /// A status bar line: text, json, waybar or swiftbar
        #[arg(long, value_enum, value_name = "FORMAT")]
        format: Option<bar::Format>,
    },
    /// Push local changes now and return once they are stored on the
    /// server, with their revisions: the files or folders given, or every
    /// pending change. Exit 0 when all is stored, 2 when blocked (a stop
    /// or a refusal), 3 when some could not be stored, 124 on timeout
    Flush(handoff::FlushArgs),
    /// Block until a file's revision is in the local file (--applied
    /// PATH[@REV], as `lsync flush` printed it elsewhere) or the vault is
    /// idle. Exit 0 when it holds, 2 when it cannot without a person (a
    /// stop, a local edit holding the revision back), 124 on timeout
    Wait(handoff::WaitArgs),
    /// Pause the service's vaults (or the one named with --vault): their
    /// engines stop and let go of the folders until `lsync resume`
    Pause {
        #[arg(long)]
        json: bool,
    },
    /// Resume the service's paused vaults (or the one named with --vault)
    Resume {
        #[arg(long)]
        json: bool,
    },
    /// Ask the service to reconnect and rescan now (every vault, or the one
    /// named with --vault)
    SyncNow {
        #[arg(long)]
        json: bool,
    },
    /// Make the service reread lsync.toml; only the vaults that changed
    /// start, stop or restart
    Reload {
        #[arg(long)]
        json: bool,
    },
    /// Follow the service's events (every vault, or the one named with
    /// --vault) until interrupted
    Events {
        /// Print the protocol's JSON lines as they come
        #[arg(long)]
        json: bool,
    },
    /// Say which rule keeps each path out of sync, like `git check-ignore -v`
    /// (ignore files use the plugin's rules, not git's). Exits 0 when some
    /// path is not synced, 1 when all are. Reads the vault folder only
    CheckIgnore {
        /// Paths: vault-relative, or file paths inside the vault
        #[arg(required = true)]
        paths: Vec<String>,
        /// Also list the paths that do sync (`::` when no rule matched)
        #[arg(long, short = 'n')]
        non_matching: bool,
        /// Print JSON (every path)
        #[arg(long)]
        json: bool,
    },
    /// Interactive on-device check: does Obsidian see outside writes, and does
    /// file watcher see Obsidian's saves?
    Probe(probe::ProbeArgs),
    /// Accounts in lsync.toml: a server, a user, and where the password is
    Account {
        #[command(subcommand)]
        cmd: manage::AccountCmd,
    },
    /// Vaults in lsync.toml: an account, a database, a folder, and options
    Vault {
        #[command(subcommand)]
        cmd: manage::VaultCmd,
    },
    /// Turn the LiveSync plugin's data.json or the official CLI's
    /// settings.json into an account and a vault in lsync.toml (the
    /// password into the secret store). Settings lsync cannot honour stop
    /// it. The vault is added disabled: take its folder over with `adopt`
    Import(import::ImportArgs),
    /// Take over a folder another client kept in sync, before its first
    /// sync here: files identical to the server's are adopted with no
    /// transfer, and the first sync runs. --dry-run only compares and says
    /// what that sync would do with every file. Never writes to the server
    /// beyond that sync
    Adopt {
        /// Compare and report; change nothing
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
        /// Take the folder over although fewer than half of the files on
        /// the smaller side (server or folder) match: you checked it is the
        /// right folder and database
        #[arg(long)]
        accept_weak_match: bool,
        #[command(flatten)]
        push: PushOpts,
    },
}

#[derive(clap::Args, Debug, Clone, Default)]
pub(crate) struct PushOpts {
    /// Allow one pass to delete files en masse remotely: more than 20 with no
    /// known destination (a move into .trash/ or a rename doesn't count) that
    /// are also more than half of the tracked files, or 3 or more when the
    /// synced folders look empty
    #[arg(long)]
    allow_mass_delete: bool,
    /// When the folder scope narrows, remove the clean local copies that fell out of it
    #[arg(long)]
    remove_out_of_scope_copies: bool,
    /// Sync two-way although this Obsidian vault has the Self-hosted
    /// LiveSync plugin enabled: a deliberate handover, where you make sure
    /// the plugin does not sync at the same time (also: allow_plugin = true
    /// for the vault in lsync.toml)
    #[arg(long)]
    allow_plugin: bool,
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
    let matches = match Cli::command().try_get_matches() {
        Ok(m) => m,
        Err(e) => return usage_error(e),
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => return usage_error(e),
    };
    match run(cli, &matches) {
        Ok(code) => std::process::ExitCode::from(code),
        Err(e) => {
            error!("{e:#}");
            std::process::ExitCode::from(1)
        }
    }
}

/// A command line clap cannot take: exit 1, as every other error does
/// (clap's own 2 is `lsync flush`/`lsync wait`'s "blocked"). `--help` and
/// `--version` print and exit 0.
fn usage_error(e: clap::Error) -> std::process::ExitCode {
    let _ = e.print();
    std::process::ExitCode::from(if e.use_stderr() { 1 } else { 0 })
}

/// Whether an argument was given on the command line (not the environment
/// or a default), at the top or in any subcommand.
fn on_command_line(m: &clap::ArgMatches, id: &str) -> bool {
    let here = m
        .try_contains_id(id)
        .ok()
        .filter(|&has| has)
        .and_then(|_| m.value_source(id))
        .is_some_and(|s| s == clap::parser::ValueSource::CommandLine);
    here || m.subcommand().is_some_and(|(_, sub)| on_command_line(sub, id))
}

/// Peak resident set size of this process in KiB.
pub fn peak_rss_kib() -> Option<u64> {
    livesync_engine::platform::peak_rss_kib()
}

/// Whether the keyring may show its dialogs (the Keychain's unlock and
/// "allow" prompts): for a command a user runs at a terminal, never for the
/// long-running ones, which nobody watches.
fn keyring_may_prompt(cmd: &Cmd, terminal: bool) -> bool {
    terminal && !matches!(cmd, Cmd::Serve { .. } | Cmd::Daemon { .. })
}

fn run(cli: Cli, matches: &clap::ArgMatches) -> Result<u8> {
    let mut overrides = cli.overrides.clone();
    if cli.system {
        livesync_host::control::use_system_service();
    }
    {
        use std::io::IsTerminal;
        let terminal = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        livesync_host::keyring::allow_prompts(keyring_may_prompt(&cli.cmd, terminal));
    }
    let ctx = Setup::new(cli.config.clone(), cli.system);
    ctx.migrate_if_needed();
    if let Cmd::Account { .. } | Cmd::Vault { .. } = &cli.cmd {
        if cli.vault.is_some() || cli.profile.is_some() {
            bail!("the account and vault commands take the name as an argument, not --vault or --profile");
        }
    }
    if let Cmd::Import(_) = &cli.cmd {
        if cli.vault.is_some() || on_command_line(matches, "profile") {
            bail!("`lsync import` names the new vault with --name, not --vault or --profile");
        }
    }
    // Checked only by the verbs that take a vault name of the service.
    let named = cli_vault(&cli, matches);
    let cmd = match cli.cmd {
        Cmd::Account { cmd } => {
            let given = manage::Given {
                o: &overrides,
                on_command_line: &|id| on_command_line(matches, id),
            };
            return manage::account(&ctx, cmd, &given);
        }
        Cmd::Vault { cmd } => {
            let given = manage::Given {
                o: &overrides,
                on_command_line: &|id| on_command_line(matches, id),
            };
            return manage::vault(&ctx, cmd, &given);
        }
        Cmd::Import(args) => {
            let given = manage::Given {
                o: &overrides,
                on_command_line: &|id| on_command_line(matches, id),
            };
            return import::run(&ctx, args, &given);
        }
        Cmd::Probe(args) => return probe::run(args),
        Cmd::Service { cmd } => return install::run(&ctx, cmd),
        Cmd::Doctor { json } => return doctor::run(&ctx, json),
        Cmd::Pause { json } => return service_verb(named?, method::PAUSE, "paused", json),
        Cmd::Resume { json } => return service_verb(named?, method::RESUME, "resumed", json),
        Cmd::SyncNow { json } => return service_verb(named?, method::SYNC_NOW, "syncing now", json),
        Cmd::Reload { json } => return reload(json),
        Cmd::Events { json } => return events(named?, json),
        Cmd::Status {
            follow,
            waybar,
            format,
            json,
            ..
        } if follow || waybar || format.is_some() => {
            let format = if waybar {
                bar::Format::Waybar
            } else {
                format.unwrap_or(if json { bar::Format::Json } else { bar::Format::Text })
            };
            return bar::run(named?, format, follow);
        }
        Cmd::Status { json, all: true, .. } => return status_all(json),
        Cmd::Serve { opts } => {
            if ctx.system && install::geteuid() == 0 {
                root_serves_no_users_vaults(&install::system_service_user(std::path::Path::new(
                    install::SYSTEM_UNIT,
                )))?;
            }
            let only = named?;
            return serve::serve(&ctx, only, &opts, VERSION);
        }
        other => other,
    };
    root_runs_no_system_vault(ctx.system, install::geteuid(), &cmd)?;
    // A user outside the system service's group cannot read its config
    // (root's, 0640 to that group: it may hold passwords). A command for one
    // of its vaults, named with --vault, goes to the service by that name
    // (a socket-group member who is not in the service's group).
    if ctx.system && unreadable(&ctx.config_file) {
        if let Some(vault) = cli.vault.clone().filter(|v| !config::looks_like_path(v)) {
            return by_name(cmd, &vault, &ctx, matches);
        }
    }
    let mut selector = Selector {
        vault: cli.vault.clone(),
        // A flag wins over the environment: `--vault NAME` over `LSYNC_PROFILE`.
        profile: cli.profile.clone().filter(|_| {
            cli.vault.as_deref().is_none_or(config::looks_like_path) || on_command_line(matches, "profile")
        }),
    };
    let name = selector.name(&mut overrides)?;
    let sel = ctx.select(name, &overrides)?;
    let cfg = config::merged(sel.file.clone(), &overrides);

    match cmd {
        Cmd::Configure { password_stdin } => {
            let password = if password_stdin {
                Some(prompt::from_stdin()?)
            } else {
                overrides.password.clone().map(livesync_host::secret::Secret::new)
            };
            let to_accounts = match &sel.origin {
                Origin::Vault { .. } => true,
                // A new name beside an lsync.toml becomes a vault there.
                Origin::Profile { path } => ctx.explicit.is_none() && ctx.config_file.exists() && !path.exists(),
            };
            if to_accounts {
                let path = manage::configure(&ctx, &sel, &overrides, password)?;
                println!("saved {}", path.display());
                let again = ctx.select(Some(sel.name.clone()), &Overrides::default())?;
                print_config(&again.file);
                return Ok(0);
            }
            let Origin::Profile { path: cfg_path } = &sel.origin else {
                unreachable!("handled above")
            };
            let mut new = cfg;
            if let Some(pw) = password {
                new.password = Some(pw.expose().to_string());
            }
            if let Some(v) = &new.vault {
                new.vault = Some(absolute(v)?);
            }
            livesync_host::legacy::save(cfg_path, &new).with_context(|| format!("writing {}", cfg_path.display()))?;
            println!("saved {}", cfg_path.display());
            print_config(&new);
            Ok(0)
        }
        Cmd::Status { json, .. } => {
            let r = config::resolve(&sel, &cfg, &ctx.dirs, false)?;
            let index_path = r.state_dir.join("index.sqlite");
            let st = match route::route(&r, "status", false)? {
                Route::Service { mut client, vault, .. } => {
                    let Some(mut st) = service_status(&mut client, &vault)? else {
                        bail!("no index at {} yet; run `lsync pull` first", index_path.display());
                    };
                    // As this command names them, like the one-shot status.
                    st.vault = r.vault.display().to_string();
                    st.index = index_path.display().to_string();
                    st
                }
                Route::Local => {
                    if !index_path.exists() {
                        bail!("no index at {} yet; run `lsync pull` first", index_path.display());
                    }
                    let ix = Index::open(&index_path)?;
                    status_from_index(&ix, &r.vault, &index_path)?
                }
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                print_status(&st);
            }
            Ok(0)
        }
        Cmd::Folders { json } => rt()?.block_on(folders(&sel, &cfg, &ctx.dirs, json)),
        Cmd::Flush(a) => handoff::flush(&sel, &cfg, &ctx.dirs, &a),
        Cmd::Wait(a) => handoff::wait(&sel, &cfg, &ctx.dirs, &a),
        Cmd::CheckIgnore {
            paths,
            non_matching,
            json,
        } => check_ignore(&sel, &cfg, &ctx.dirs, &paths, non_matching, json),
        Cmd::Connect { adopt_tweaks, json } => rt()?.block_on(async {
            let gate = match routed(&sel, &cfg, &ctx.dirs, "connect")? {
                Some((mut client, vault, fingerprint)) => {
                    let p = json!({"vault": vault, "adopt_tweaks": adopt_tweaks, "fingerprint": fingerprint});
                    match command(&mut client, method::CONNECT, p)? {
                        Ok(r) => Ok(serde_json::from_value::<GateReport>(r.report)?),
                        Err(refusal) => Err(refusal),
                    }
                }
                None => {
                    let engine = engine_for(&sel, &cfg, &ctx.dirs, &PushOpts::default())?;
                    match engine.connect_gate(adopt_tweaks).await {
                        Ok(g) => Ok(g),
                        Err(EngineError::Refused(r)) => Err(r.to_string()),
                        Err(e) => return Err(e.into()),
                    }
                }
            };
            match gate {
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
                Err(r) => {
                    eprintln!("gate: REFUSED\n  {r}");
                    Ok(2)
                }
            }
        }),
        Cmd::Pull { json } => rt()?.block_on(async {
            if let Some((mut client, vault, fingerprint)) = routed(&sel, &cfg, &ctx.dirs, "pull")? {
                let p = json!({ "vault": vault, "fingerprint": fingerprint });
                return match command(&mut client, method::PULL, p)? {
                    Ok(r) => print_pull(
                        &serde_json::from_value(r.report)?,
                        Duration::from_millis(r.wall_ms),
                        r.peak_rss_kib,
                        json,
                    ),
                    Err(refusal) => {
                        eprintln!("pull refused: {refusal}");
                        Ok(2)
                    }
                };
            }
            let engine = engine_for(&sel, &cfg, &ctx.dirs, &PushOpts::default())?;
            let t0 = Instant::now();
            let rep = match engine.pull_once().await {
                Ok(r) => r,
                Err(EngineError::Refused(r)) => {
                    eprintln!("pull refused: {r}");
                    return Ok(2);
                }
                Err(e) => return Err(e.into()),
            };
            print_pull(&rep, t0.elapsed(), peak_rss_kib(), json)
        }),
        Cmd::Sync { json, push } => rt()?.block_on(async {
            two_way_allowed(&sel, "sync")?;
            if let Some((mut client, vault, fingerprint)) = routed(&sel, &cfg, &ctx.dirs, "sync")? {
                return run_routed(
                    &mut client,
                    method::SYNC,
                    &vault,
                    fingerprint,
                    sel.tuning.allow_plugin,
                    &push,
                    json,
                );
            }
            let engine = engine_for(&sel, &cfg, &ctx.dirs, &push)?;
            run_sync(&engine, json, false).await
        }),
        Cmd::Fetch { json, push } => rt()?.block_on(async {
            two_way_allowed(&sel, "fetch")?;
            if let Some((mut client, vault, fingerprint)) = routed(&sel, &cfg, &ctx.dirs, "fetch")? {
                return run_routed(
                    &mut client,
                    method::FETCH,
                    &vault,
                    fingerprint,
                    sel.tuning.allow_plugin,
                    &push,
                    json,
                );
            }
            let engine = engine_for(&sel, &cfg, &ctx.dirs, &push)?;
            run_sync(&engine, json, true).await
        }),
        Cmd::AllowDeletes { stop, json } => rt()?.block_on(allow_deletes(&sel, &cfg, &ctx.dirs, stop, json)),
        Cmd::Adopt {
            dry_run,
            json,
            accept_weak_match,
            push,
        } => {
            if let Some((client, vault, _)) = routed(&sel, &cfg, &ctx.dirs, "adopt")? {
                bail!(
                    "vault {vault} is run by the lsync service (socket {}); adopt takes a folder over before its first sync, so stop the service, or disable the vault (`lsync vault disable {vault}`), first",
                    client.path().display()
                );
            }
            rt()?.block_on(adopt::run(
                &sel,
                &cfg,
                &ctx.dirs,
                adopt::Opts {
                    dry_run,
                    json,
                    accept_weak_match,
                    push,
                },
            ))
        }
        Cmd::Daemon { pull_only, push } => {
            let pull_only = pull_only || sel.mode == Mode::PullOnly;
            if let Some((client, vault, _)) = routed(&sel, &cfg, &ctx.dirs, "daemon")? {
                bail!(
                    "vault {vault} is already run by the lsync service (socket {}); follow it with `lsync --vault {vault} events`, sync it now with `lsync --vault {vault} sync-now`, or stop the service to run it here",
                    client.path().display()
                );
            }
            let run = run_config_for(&sel, &cfg, &ctx.dirs, &push, pull_only)?;
            serve::daemon(serve::planned(run), VERSION)
        }
        Cmd::Probe(_)
        | Cmd::Import(_)
        | Cmd::Account { .. }
        | Cmd::Vault { .. }
        | Cmd::Serve { .. }
        | Cmd::Service { .. }
        | Cmd::Doctor { .. }
        | Cmd::Pause { .. }
        | Cmd::Resume { .. }
        | Cmd::SyncNow { .. }
        | Cmd::Reload { .. }
        | Cmd::Events { .. } => unreachable!("dispatched above"),
    }
}

/// The vault a service verb names: `--vault` or `--profile` on the command
/// line, never `LSYNC_PROFILE`; without one the verb is for every vault.
fn cli_vault(cli: &Cli, m: &clap::ArgMatches) -> Result<Option<String>> {
    let v = cli
        .vault
        .clone()
        .or_else(|| cli.profile.clone().filter(|_| on_command_line(m, "profile")));
    if let Some(n) = &v {
        if config::looks_like_path(n) {
            bail!("--vault takes a vault's name here, not a folder: {n}");
        }
    }
    Ok(v)
}

fn vault_names(v: &serde_json::Value) -> String {
    let names: Vec<&str> = v["vaults"]
        .as_array()
        .map(|a| a.iter().filter_map(|n| n.as_str()).collect())
        .unwrap_or_default();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// `pause`, `resume`, `sync-now`: one request to the service.
fn service_verb(vault: Option<String>, m: &str, done: &str, json: bool) -> Result<u8> {
    let mut c = route::require(&[m])?;
    let v = c
        .call(m, json!({ "vault": vault }))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("{done}: {}", vault_names(&v));
    }
    Ok(0)
}

fn reload(json: bool) -> Result<u8> {
    let mut c = route::require(&[method::RELOAD])?;
    let v = c.call(method::RELOAD, json!({})).map_err(|e| anyhow::anyhow!("{e}"))?;
    let r: livesync_host::protocol::ReloadReport = serde_json::from_value(v.clone())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        let show = |l: &[String]| if l.is_empty() { "-".to_string() } else { l.join(", ") };
        println!("started    {}", show(&r.started));
        println!("stopped    {}", show(&r.stopped));
        println!("restarted  {}", show(&r.restarted));
        println!("unchanged  {}", show(&r.unchanged));
        for f in &r.failed {
            println!("NOT RUNNING {}: {}", f.vault, f.message);
        }
    }
    // The new config is applied: a reload succeeded, also for
    // `systemctl reload` (ExecReload=). A vault that is not running is
    // said, here and in the service's status and log; an invalid config
    // is the service's error above, and changes nothing.
    if !r.failed.is_empty() {
        eprintln!(
            "warning: the config was reloaded, but {} vault(s) are not running: {}",
            r.failed.len(),
            r.failed.iter().map(|f| f.vault.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    Ok(0)
}

/// Local wall-clock time of a timestamp, `HH:MM:SS`.
pub(crate) fn clock(ms: i64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: zeroed tm is a valid out-parameter for localtime_r.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return ms.to_string();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// An event's data as `key=value`, leaving out zeros and nulls.
fn event_summary(data: &serde_json::Value) -> String {
    let Some(m) = data.as_object() else {
        return String::new();
    };
    m.iter()
        .filter(|(_, v)| !(v.is_null() || v.as_u64() == Some(0) || v.as_array().is_some_and(|a| a.is_empty())))
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => format!("{k}={s}"),
            other => format!("{k}={other}"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn events(vault: Option<String>, json: bool) -> Result<u8> {
    use std::io::Write;
    let mut c = route::require(&[method::EVENTS])?;
    let mut out = std::io::stdout();
    c.subscribe(vault.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?;
    loop {
        let e = c.next_event().map_err(|e| anyhow::anyhow!("{e}"))?;
        let line = if json {
            serde_json::to_string(&e).unwrap_or_default()
        } else {
            format!(
                "{} {:<12} {:<11} {}",
                clock(e.ts),
                e.vault.as_deref().unwrap_or("-"),
                e.event,
                event_summary(&e.data)
            )
        };
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            // The reader went away (a closed pipe).
            return Ok(0);
        }
    }
}

/// `lsync status --all`: the service's view of every vault.
fn status_all(json: bool) -> Result<u8> {
    let mut c = route::require(&[method::STATUS])?;
    let st = route::status_of(&mut c, None)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&st)?);
        return Ok(0);
    }
    println!(
        "{:<16} {:<9} {:<10} {:<22} message",
        "vault", "state", "mode", "last pull"
    );
    for v in &st.vaults {
        let mode = v
            .settings
            .as_ref()
            .map_or("-", |s| livesync_host::service::mode_name(s.pull_only));
        let last = v.status.as_ref().and_then(|s| s.last_pull_ms);
        let ago = last.map_or("-".to_string(), |ms| {
            format!("{}s ago", (livesync_engine::vaultfs::now_ms() - ms) / 1000)
        });
        let message = v
            .status
            .as_ref()
            .and_then(|s| s.safety_stop.as_ref().map(|t| format!("SAFETY STOP {t}")))
            .or_else(|| {
                v.status
                    .as_ref()
                    .and_then(|s| s.upstream_stop.as_ref().map(|t| format!("UPSTREAM STOP {t}")))
            })
            .or_else(|| {
                v.status.as_ref().and_then(|s| {
                    s.remote_locked
                        .as_ref()
                        .map(|t| format!("LOCKED BY ANOTHER DEVICE {t}"))
                })
            })
            // A running engine's message is its mode, shown already.
            .or_else(|| v.message.clone().filter(|m| m != mode))
            .unwrap_or_default();
        println!("{:<16} {:<9} {:<10} {:<22} {message}", v.name, v.state, mode, ago);
    }
    println!("(service at {})", c.path().display());
    Ok(0)
}

/// The service, the vault name and its settings' fingerprint, when the
/// service holds this command's vault (see `route`).
type Routed = (Client, String, Option<String>);

fn routed(sel: &Selected, cfg: &FileConfig, dirs: &Dirs, what: &str) -> Result<Option<Routed>> {
    let r = config::resolve(sel, cfg, dirs, true)?;
    Ok(match route::route(&r, what, true)? {
        Route::Local => None,
        Route::Service {
            client,
            vault,
            fingerprint,
        } => Some((client, vault, fingerprint)),
    })
}

/// Run a command in the service. `Ok(Err(refusal))` when a gate refused.
/// It waits as long as the command runs and the service answers (see
/// `Client`), so a long fetch is not cut off.
fn command(c: &mut Client, m: &str, p: serde_json::Value) -> Result<std::result::Result<CommandResult, String>> {
    c.set_timeout(None);
    match c.call(m, p) {
        Ok(v) => Ok(Ok(serde_json::from_value(v)?)),
        Err(e) if e.code() == Some(livesync_host::protocol::code::REFUSED) => Ok(Err(e.to_string())),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

/// The status of a vault the service runs, with what the service's view
/// adds (`None`: no index yet).
fn service_status(client: &mut Client, vault: &str) -> Result<Option<livesync_engine::Status>> {
    let view = route::status_of(client, Some(vault))?.vaults.into_iter().next();
    // A service older than these fields in the status has them in its
    // vault view.
    let confirmed = view
        .as_ref()
        .map_or((false, false), |v| (v.mass_delete_allowed, v.mass_delete_waits));
    Ok(view.and_then(|v| v.status).map(|mut st| {
        st.mass_delete_allowed |= confirmed.0;
        st.mass_delete_waits |= confirmed.1;
        st
    }))
}

/// Whether the config file is there but this user may not read it.
/// Root, with `--system`, never opens one of the system service's vaults
/// here: their folders and state belong to the service's user, who could
/// have put links there for root to follow and write through. Only the
/// config (root's) is root's to change.
fn root_runs_no_system_vault(system: bool, euid: u32, cmd: &Cmd) -> Result<()> {
    if !system || euid != 0 || matches!(cmd, Cmd::Configure { .. }) {
        return Ok(());
    }
    bail!(
        "root does not run the system service's vaults itself: their folders and state belong to the service's user, and root would act on whatever that user put there. Run it as that user, with the service's group (`sudo -u USER -g GROUP lsync --system ...`, as User= and Group= in lsync.service say), or through the service as a member of its group (`lsync --system --vault NAME status|sync|fetch|pull`, `sync-now`, `status --all`)"
    )
}

/// `sudo lsync --system serve` by hand, while the installed unit runs the
/// service as another user (`User=` in it, root's file): refused, for the
/// reason [`root_runs_no_system_vault`] gives. A unit with `User=root`
/// (`--run-as root`), or none installed, is root's own.
fn root_serves_no_users_vaults(unit_user: &Option<String>) -> Result<()> {
    match unit_user {
        Some(u) if u != "root" && u != "0" => bail!(
            "the system service runs as {u} (lsync.service); root does not run it by hand, as the vaults' folders and state are {u}'s. Start it with `sudo systemctl start lsync`, or run it as that user (`sudo -u {u} -g GROUP lsync --system serve`)"
        ),
        _ => Ok(()),
    }
}

fn unreadable(path: &std::path::Path) -> bool {
    matches!(std::fs::File::open(path), Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied)
}

/// Settings given to this command (flags or `LSYNC_*`), which a command run
/// by the service's settings would silently ignore.
fn settings_given(matches: &clap::ArgMatches) -> Vec<String> {
    let probe = <config::Overrides as clap::Args>::augment_args(clap::Command::new("probe"));
    probe
        .get_arguments()
        .filter(|a| {
            matches!(
                matches.value_source(a.get_id().as_str()),
                Some(clap::parser::ValueSource::CommandLine | clap::parser::ValueSource::EnvVariable)
            )
        })
        .map(|a| format!("--{}", a.get_long().unwrap_or(a.get_id().as_str())))
        .collect()
}

/// `lsync --system --vault NAME status|sync|fetch|pull` for a user who may
/// use the system service (its socket group) but not read its config: the
/// service runs it by name, with its own settings.
fn by_name(cmd: Cmd, vault: &str, ctx: &Setup, matches: &clap::ArgMatches) -> Result<u8> {
    let cannot = format!(
        "{} can be read only by root and the system service's group",
        ctx.config_file.display()
    );
    let given = settings_given(matches);
    if !given.is_empty() {
        bail!(
            "{cannot}, so this command runs with the service's settings for {vault}; {} would not apply. Leave them out",
            given.join(", ")
        );
    }
    match cmd {
        Cmd::Status { json, .. } => {
            let mut c = route::require(&[method::STATUS])?;
            let Some(st) = service_status(&mut c, vault)? else {
                bail!("the lsync service has no index of {vault} yet");
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                print_status(&st);
            }
            Ok(0)
        }
        Cmd::Sync { json, push } => {
            let mut c = route::require(&[method::SYNC])?;
            run_routed(&mut c, method::SYNC, vault, None, false, &push, json)
        }
        Cmd::Fetch { json, push } => {
            let mut c = route::require(&[method::FETCH])?;
            run_routed(&mut c, method::FETCH, vault, None, false, &push, json)
        }
        Cmd::Pull { json } => {
            let mut c = route::require(&[method::PULL])?;
            match command(&mut c, method::PULL, json!({ "vault": vault }))? {
                Ok(r) => print_pull(
                    &serde_json::from_value(r.report)?,
                    Duration::from_millis(r.wall_ms),
                    r.peak_rss_kib,
                    json,
                ),
                Err(refusal) => {
                    eprintln!("pull refused: {refusal}");
                    Ok(2)
                }
            }
        }
        _ => bail!(
            "{cannot}; a member of its group can run `lsync --system --vault {vault} status|sync|fetch|pull`, and `status --all`, `pause`, `resume`, `sync-now`, `events` and `reload`"
        ),
    }
}

/// `sync` or `fetch` in the service, printed as the one-shot prints it.
fn run_routed(
    c: &mut Client,
    m: &str,
    vault: &str,
    fingerprint: Option<String>,
    allow_plugin: bool,
    push: &PushOpts,
    json: bool,
) -> Result<u8> {
    let p = json!({
        "vault": vault,
        "fingerprint": fingerprint,
        "allow_mass_delete": push.allow_mass_delete,
        "remove_out_of_scope_copies": push.remove_out_of_scope_copies,
        "allow_plugin": push.allow_plugin || allow_plugin,
    });
    match command(c, m, p)? {
        Ok(r) => print_sync(
            &serde_json::from_value(r.report)?,
            Duration::from_millis(r.wall_ms),
            r.peak_rss_kib,
            json,
        ),
        Err(refusal) => {
            eprintln!("sync refused: {refusal}");
            Ok(2)
        }
    }
}

/// What `lsync daemon` runs: this command's vault, as `engine_for` would
/// open it.
fn run_config_for(
    sel: &Selected,
    cfg: &FileConfig,
    dirs: &Dirs,
    push: &PushOpts,
    pull_only: bool,
) -> Result<livesync_host::RunConfig> {
    let quiet = std::env::var("LSYNC_REBUILD_QUIET_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let r = config::resolve(sel, cfg, dirs, true)?;
    let vault = absolute(&r.vault)?;
    let state = absolute(&r.state_dir)?;
    let index = state.join("index.sqlite");
    if index.starts_with(&vault) && !index.strip_prefix(&vault)?.to_string_lossy().starts_with('.') {
        bail!("the state directory must not be a visible folder inside the vault");
    }
    Ok(livesync_host::RunConfig {
        id: sel.name.clone(),
        vault_path: vault.display().to_string(),
        state_dir: state.display().to_string(),
        url: r.remote.url,
        database: r.remote.database,
        username: r.remote.username.unwrap_or_default(),
        password: r.remote.password.unwrap_or_default(),
        scope_mode: r.scope.mode,
        scope_folders: r.scope.prefixes,
        pull_only,
        device_name: r.device_name.unwrap_or_default(),
        ignore_regex: r.ignore_regex.unwrap_or_default(),
        what_syncs: r.what_syncs,
        concurrency: r.concurrency,
        max_size_mb: r.max_size_mb,
        allow_mass_delete: push.allow_mass_delete,
        remove_out_of_scope_copies: push.remove_out_of_scope_copies,
        rebuild_quiet: quiet.map(Duration::from_secs),
        tuning: livesync_host::Tuning {
            allow_plugin: sel.tuning.allow_plugin || push.allow_plugin,
            ..sel.tuning.clone()
        },
    })
}

/// `allow-deletes`: the stop shown is not the one given; nothing was allowed.
const EXIT_STOP_CHANGED: u8 = 5;

/// The stop to confirm: the one given, or the one shown after a yes on the
/// terminal. `None` when no stop is shown (and none given).
fn stop_to_confirm(shown: Option<String>, given: Option<String>) -> Result<Option<String>> {
    if given.is_some() {
        return Ok(given);
    }
    let Some(stop) = shown else { return Ok(None) };
    let text = stop.split_once(' ').map_or(stop.as_str(), |(_, t)| t);
    println!("Safety stop: {text}");
    let yes = prompt::confirm(
        "Delete exactly the files this stop listed on the server, and so on every device?",
        "pass the stop with --stop (`safety_stop` in `lsync status --json`), so that only the stop you read is confirmed",
    )?;
    if !yes {
        bail!("nothing was allowed");
    }
    Ok(Some(stop))
}

/// `lsync allow-deletes`: confirm the vault's mass-delete stop, in the
/// service when it runs the vault (its next full pass deletes the files),
/// else here, with a sync that deletes them. The confirmation is kept in the
/// index until a pass applies it. Exit 1 when no stop is shown,
/// [`EXIT_STOP_CHANGED`] when the stop changed since it was read (2 stays a
/// gate's refusal of the sync).
async fn allow_deletes(sel: &Selected, cfg: &FileConfig, dirs: &Dirs, given: Option<String>, json: bool) -> Result<u8> {
    two_way_allowed(sel, "allow-deletes")?;
    let r = config::resolve(sel, cfg, dirs, false)?;
    let none = || {
        eprintln!("no mass-delete safety stop is shown; nothing was allowed");
        Ok(1)
    };
    match route::route(&r, "allow-deletes", false)? {
        Route::Service { mut client, vault, .. } => {
            let shown = route::status_of(&mut client, Some(&vault))?
                .vaults
                .into_iter()
                .next()
                .and_then(|v| v.status)
                .and_then(|s| s.safety_stop);
            let Some(stop) = stop_to_confirm(shown, given)? else {
                return none();
            };
            client.set_timeout(None);
            match client.call(method::ALLOW_DELETES, json!({"vault": vault, "stop": stop})) {
                Ok(v) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&v)?);
                    } else {
                        println!("allowed: the lsync service deletes the files the stop listed at the next full pass of {vault}, which starts now. If some of them cannot be checked then (a folder that cannot be read), it waits until they can, and `lsync status` says so");
                    }
                    Ok(0)
                }
                Err(e) if e.code() == Some(code::NO_STOP) => none(),
                Err(e) if e.code() == Some(code::STOP_CHANGED) => {
                    eprintln!("{e}");
                    Ok(EXIT_STOP_CHANGED)
                }
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }
        Route::Local => {
            let engine = engine_for(sel, cfg, dirs, &PushOpts::default())?;
            let shown = engine.status()?.safety_stop;
            let Some(stop) = stop_to_confirm(shown, given)? else {
                return none();
            };
            match engine.allow_mass_delete_once(&stop)? {
                Confirmation::Allowed => {
                    let code = run_sync(&engine, json, false)
                        .await
                        .context("allowed, and the confirmation is kept for the next sync; but this sync failed")?;
                    if engine.mass_delete_waits() {
                        eprintln!("allowed, but waiting: some of the files the stop listed cannot be checked now (see `lsync status`), so none was deleted on it. The confirmation is kept: the next sync (or the service) that can check them all deletes them");
                    } else if engine.mass_delete_allowed() {
                        eprintln!("allowed, but not applied yet: this sync did not get to the deletions. The confirmation is kept for the next sync that does");
                    }
                    Ok(code)
                }
                Confirmation::NoStop => none(),
                Confirmation::Changed(now) => {
                    let now = now.split_once(' ').map_or(now.as_str(), |(_, t)| t);
                    eprintln!("the safety stop changed after it was read, so nothing was allowed; it now says: {now}");
                    Ok(EXIT_STOP_CHANGED)
                }
            }
        }
    }
}

/// A pull-only vault is never written to the remote from here either.
fn two_way_allowed(sel: &Selected, what: &str) -> Result<()> {
    if sel.mode == Mode::PullOnly {
        bail!(
            "vault {} is pull-only (mode = \"pull-only\"), and `{what}` writes to the remote; use `lsync pull`, or `lsync vault edit {} --mode two-way`",
            sel.name,
            sel.name
        );
    }
    Ok(())
}

async fn run_sync(engine: &Engine, json: bool, fetch: bool) -> Result<u8> {
    if fetch {
        // A fetch here is the start of a two-way sync: refused like one,
        // before the index is reset for it.
        match engine.check_plugin_guard().and_then(|()| engine.request_full_fetch()) {
            Ok(()) => {}
            Err(EngineError::Refused(r)) => {
                eprintln!("fetch refused: {r}");
                return Ok(2);
            }
            Err(e) => return Err(e.into()),
        }
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
    print_sync(&rep, t0.elapsed(), peak_rss_kib(), json)
}

fn print_sync(rep: &SyncReport, wall: Duration, rss: Option<u64>, json: bool) -> Result<u8> {
    if json {
        let mut v = serde_json::to_value(rep)?;
        v["wall_ms"] = (wall.as_millis() as u64).into();
        v["peak_rss_kib"] = rss.into();
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
    Ok(sync_exit_code(rep))
}

/// `sync`'s exit code: 3 when something needs attention.
pub(crate) fn sync_exit_code(rep: &SyncReport) -> u8 {
    let attention = rep.pull.dirty > 0
        || rep.pull.pending > 0
        || rep.push.issues > 0
        || rep.push.safety_stops > 0
        || rep.push.failed > 0
        || rep.push.orphans_pushed > 0;
    if attention {
        3
    } else {
        0
    }
}

/// `pull`'s exit code: 3 when something needs attention.
pub(crate) fn pull_exit_code(rep: &PullReport) -> u8 {
    if rep.dirty > 0 || rep.pending > 0 {
        3
    } else {
        0
    }
}

fn print_pull(rep: &PullReport, wall: Duration, rss: Option<u64>, json: bool) -> Result<u8> {
    if json {
        let mut v = serde_json::to_value(rep)?;
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
    Ok(pull_exit_code(rep))
}

async fn folders(sel: &Selected, cfg: &FileConfig, dirs: &Dirs, json: bool) -> Result<u8> {
    use livesync_model::scope::Coverage;
    let mut probe = cfg.clone();
    // The folder is optional here: only its top-level folders are merged in.
    probe.vault = Some(cfg.vault.clone().unwrap_or_default());
    let r = config::resolve(sel, &probe, dirs, true)?;
    let scope = r.scope;
    let couch = Couch::new(&r.remote)?;
    let mut l = match livesync_engine::list_remote_folders(&couch).await {
        Ok(l) => l,
        Err(EngineError::Refused(r)) => {
            eprintln!("folders refused: {r}");
            return Ok(2);
        }
        Err(e) => return Err(e.into()),
    };
    if let Some(v) = &cfg.vault {
        l.merge_local(&livesync_engine::local_top_folders(v));
    }
    // A root file syncs unless the scope lists only the folders to sync.
    let root_synced = scope.mode != livesync_model::scope::ScopeMode::Include;
    if json {
        let mut v = serde_json::to_value(&l)?;
        for (f, out) in l.folders.iter().zip(v["folders"].as_array_mut().into_iter().flatten()) {
            out["coverage"] = serde_json::to_value(scope.coverage(&f.name))?;
        }
        v["root_synced"] = root_synced.into();
        v["scope"] = scope.describe().into();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    let mark = |c: Coverage| match c {
        Coverage::Whole => "yes",
        Coverage::Part => "part",
        Coverage::None => "no",
    };
    println!("folder scope: {}", scope.describe());
    println!("{:<5} {:>8} {:>10}  folder", "sync", "files", "size");
    for f in &l.folders {
        let note = if f.remote { "" } else { "  (local only)" };
        println!(
            "{:<5} {:>8} {:>10}  {}/{note}",
            mark(scope.coverage(&f.name)),
            f.files,
            human_bytes(f.bytes),
            f.name
        );
    }
    println!(
        "{:<5} {:>8} {:>10}  (files at the vault root)",
        if root_synced { "yes" } else { "no" },
        l.root_files,
        human_bytes(l.root_bytes)
    );
    println!(
        "{} live files in {} folders; read {} documents ({} fetched) in {:.1}s",
        l.files,
        l.folders.iter().filter(|f| f.remote).count(),
        l.docs_read,
        human_bytes(l.bytes_fetched),
        l.elapsed_ms as f64 / 1000.0
    );
    Ok(0)
}

fn human_bytes(b: u64) -> String {
    match b {
        b if b >= 1_000_000_000 => format!("{:.1} GB", b as f64 / 1e9),
        b if b >= 1_000_000 => format!("{:.1} MB", b as f64 / 1e6),
        b if b >= 1_000 => format!("{:.1} kB", b as f64 / 1e3),
        b => format!("{b} B"),
    }
}

pub(crate) fn rt() -> Result<tokio::runtime::Runtime> {
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

/// A path given to `check-ignore`, vault-relative: an absolute path, or one
/// relative to the current folder when that is inside the vault, must lie
/// in the vault; anything else is taken as vault-relative already.
pub(crate) fn vault_relative(vault: &std::path::Path, given: &str) -> Result<String> {
    use std::path::{Component, Path};
    let vault = std::fs::canonicalize(vault).unwrap_or_else(|_| vault.to_path_buf());
    let cwd = std::env::current_dir().ok().and_then(|c| std::fs::canonicalize(c).ok());
    let p = Path::new(given);
    let abs = if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        cwd.filter(|c| c.starts_with(&vault)).map(|c| c.join(p))
    };
    let Some(abs) = abs else {
        return Ok(given.replace('\\', "/").trim_matches('/').to_string());
    };
    // Lexically: the path need not exist.
    let mut norm = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other),
        }
    }
    let rel = norm
        .strip_prefix(&vault)
        .map_err(|_| anyhow::anyhow!("{given}: outside the vault folder {}", vault.display()))?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

fn check_ignore(
    sel: &Selected,
    cfg: &FileConfig,
    dirs: &Dirs,
    paths: &[String],
    non_matching: bool,
    json: bool,
) -> Result<u8> {
    let r = config::resolve(sel, cfg, dirs, false)?;
    let mut ec = EngineConfig::new(absolute(&r.vault)?, r.state_dir.join("index.sqlite"));
    ec.ignore_regex = r.ignore_regex.clone();
    ec.scope = r.scope.clone();
    let w = r.what_syncs.clone();
    ec.use_ignore_files = w.use_ignore_files;
    ec.ignore_files = w.ignore_files;
    ec.sync_only_regex = Some(w.sync_only_regex).filter(|s| !s.trim().is_empty());
    ec.sync_dot_paths = w.sync_dot_paths;
    // The remote's case rule, as the index adopted it (default: case-insensitive).
    let case_sensitive = Some(&ec.index_path)
        .filter(|p| p.exists())
        .and_then(|p| Index::open(p).ok())
        .and_then(|ix| ix.get_meta("adopted_tweaks").ok().flatten())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|t| t.get("handleFilenameCaseSensitive").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    let rels: Vec<String> = paths
        .iter()
        .map(|p| vault_relative(&ec.vault, p))
        .collect::<Result<_>>()?;
    let checks = livesync_engine::filters::check_paths(&ec, &rels, case_sensitive).map_err(|e| anyhow::anyhow!(e))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        for (c, given) in checks.iter().zip(paths) {
            let source = match &c.source {
                Some(s) => match (s.line, &s.pattern) {
                    (Some(l), Some(p)) => format!("{}:{l}:{p}", s.file),
                    _ => format!("{}::", s.file),
                },
                None if c.synced => "::".to_string(),
                None => format!("[{}]", c.reason),
            };
            if c.synced && !non_matching {
                continue;
            }
            println!("{source}\t{given}");
            if c.reason == "undecidable" {
                eprintln!("  {}: {}", c.path, c.detail);
            }
        }
    }
    Ok(if checks.iter().any(|c| !c.synced) { 0 } else { 1 })
}

pub(crate) fn engine_for(sel: &Selected, cfg: &FileConfig, dirs: &Dirs, push: &PushOpts) -> Result<Engine> {
    engine_with_index(sel, cfg, dirs, push, None)
}

/// `engine_for`, with the index somewhere else than the vault's state
/// folder when `index` is given (a dry run's throwaway index).
fn engine_with_index(
    sel: &Selected,
    cfg: &FileConfig,
    dirs: &Dirs,
    push: &PushOpts,
    index: Option<PathBuf>,
) -> Result<Engine> {
    // After a rebuild elsewhere, how long the remote must stay unchanged
    // before the refetch counts as complete (default 30 s; tests shorten it).
    let quiet = std::env::var("LSYNC_REBUILD_QUIET_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let r = config::resolve(sel, cfg, dirs, true)?;
    let couch = Couch::new(&r.remote)?;
    let index = index.unwrap_or_else(|| r.state_dir.join("index.sqlite"));
    let mut ec = EngineConfig::new(absolute(&r.vault)?, index);
    ec.ignore_regex = r.ignore_regex;
    ec.max_size_mb = r.max_size_mb;
    ec.scope = r.scope;
    ec.device_name = r.device_name;
    let w = r.what_syncs;
    ec.use_ignore_files = w.use_ignore_files;
    ec.ignore_files = w.ignore_files;
    ec.sync_only_regex = Some(w.sync_only_regex).filter(|s| !s.trim().is_empty());
    ec.sync_dot_paths = w.sync_dot_paths;
    ec.trash = w.trash;
    ec.allow_mass_delete = push.allow_mass_delete;
    ec.remove_out_of_scope_copies = push.remove_out_of_scope_copies;
    if let Some(q) = quiet {
        ec.rebuild_quiet = std::time::Duration::from_secs(q);
    }
    if let Some(c) = r.concurrency {
        ec.concurrency = c.max(1);
    }
    sel.tuning
        .apply(&mut ec)
        .map_err(|e| anyhow::anyhow!("[vault.{}]: {e}", sel.name))?;
    ec.allow_plugin |= push.allow_plugin;
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
    // A password inside the URL is not shown either.
    println!(
        "  url        {}",
        show(&c.url.as_deref().map(livesync_host::run::url_for_display))
    );
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
    match config::what_syncs(c) {
        Ok(w) => {
            if w.use_ignore_files {
                println!("  ignore     {}", w.ignore_files.join(", "));
            } else {
                println!("  ignore     - (ignore files off)");
            }
            println!(
                "  sync only  {}",
                show(&Some(w.sync_only_regex).filter(|s| !s.is_empty()))
            );
            if !w.sync_dot_paths.is_empty() {
                println!("  dot-paths  {}", w.sync_dot_paths.join(", "));
            }
            println!("  trash      {}", w.trash.as_str());
        }
        Err(e) => println!("  filters    INVALID: {e}"),
    }
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
    println!("folder kind     {}", s.folder_kind.map_or("-", |k| k.as_str()));
    println!("vault lock      {}", s.vault_lock.as_deref().unwrap_or("-"));
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
        println!("SAFETY STOP     {stop} (restore the files, or `lsync allow-deletes` to delete exactly these)");
    }
    match (s.mass_delete_allowed, s.mass_delete_waits) {
        (true, true) => println!(
            "ALLOWED         waiting: some of the stop's files cannot be checked now (see the local issues); nothing is deleted until they can be"
        ),
        (true, false) => println!("ALLOWED         the stop's files are deleted at the next full pass"),
        _ => {}
    }
    if let Some(stop) = &s.upstream_stop {
        println!("UPSTREAM STOP   {stop}");
        println!(
            "                (syncing resumes by itself once the remote is compatible again; see COMPATIBILITY.md)"
        );
    }
    if let Some(lock) = &s.remote_locked {
        println!("REMOTE LOCKED   {lock}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_command_at_a_terminal_may_show_keyring_dialogs() {
        let doctor = Cmd::Doctor { json: false };
        assert!(keyring_may_prompt(&doctor, true));
        assert!(!keyring_may_prompt(&doctor, false));
        let serve = Cmd::Serve {
            opts: serve::ServeOpts::default(),
        };
        assert!(!keyring_may_prompt(&serve, true), "the service never waits on a dialog");
        assert!(!livesync_host::keyring::prompts_allowed(), "off unless allowed");
    }

    /// Root under `--system` opens no vault's folder or state of the
    /// service's user; it may change the config (root's) only.
    #[test]
    fn root_runs_no_system_vault_here() {
        let status = Cmd::Status {
            json: false,
            all: false,
            follow: false,
            waybar: false,
            format: None,
        };
        let folders = Cmd::Folders { json: false };
        for cmd in [&status, &folders] {
            let e = root_runs_no_system_vault(true, 0, cmd).unwrap_err().to_string();
            assert!(e.contains("sudo -u USER -g GROUP"), "{e}");
            assert!(root_runs_no_system_vault(true, 1000, cmd).is_ok(), "the service's user");
            assert!(root_runs_no_system_vault(false, 0, cmd).is_ok(), "root's own vaults");
        }
        let configure = Cmd::Configure { password_stdin: false };
        assert!(root_runs_no_system_vault(true, 0, &configure).is_ok());

        // `sudo lsync --system serve` by hand: only when the unit runs it
        // as root, or there is no unit.
        let e = root_serves_no_users_vaults(&Some("lsync".into()))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("runs as lsync") && e.contains("systemctl start lsync"),
            "{e}"
        );
        assert!(root_serves_no_users_vaults(&Some("root".into())).is_ok());
        assert!(root_serves_no_users_vaults(&None).is_ok());
        let d = tempfile::tempdir().unwrap();
        let unit = d.path().join("lsync.service");
        std::fs::write(&unit, "[Service]\nUser=lsync\nGroup=agents\n").unwrap();
        assert_eq!(install::system_service_user(&unit).as_deref(), Some("lsync"));
        assert_eq!(install::system_service_user(&d.path().join("none")), None);
    }
}
