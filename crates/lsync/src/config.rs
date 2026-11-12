//! Which settings a command runs with. Flags override environment, which
//! overrides the config file.
//!
//! The config file is `$XDG_CONFIG_HOME/lsync/lsync.toml`, with accounts and
//! vaults (`livesync_host::config`); `--vault NAME` (or `--profile NAME`)
//! picks a vault, and with one vault, or one named `default`, there is
//! nothing to pick. The profile files that came before it
//! (`<profile>.toml`, or `--config FILE`) are migrated on the first run and
//! still read where they were not.

use anyhow::{anyhow, bail, Context, Result};
use livesync_host::config::{check_name, Config, Dirs, Mode, SECRETS_FILE};
use livesync_host::migrate;
use livesync_host::secret::{FileStore, SecretStores};
use livesync_host::Tuning;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

pub use livesync_host::legacy::Profile as FileConfig;

/// Values given on the command line or through `LSYNC_*` variables.
/// No `Debug`: it holds the password.
#[derive(Default, Clone, clap::Args)]
pub struct Overrides {
    /// CouchDB server URL (http:// or https://)
    #[arg(long, env = "LSYNC_URL", global = true)]
    pub url: Option<String>,
    /// Database name
    #[arg(long = "db", env = "LSYNC_DB", global = true)]
    pub database: Option<String>,
    /// CouchDB user
    #[arg(long = "user", env = "LSYNC_USER", global = true)]
    pub username: Option<String>,
    /// CouchDB password (prefer the secret store, see `lsync account`)
    #[arg(long, env = "LSYNC_PASSWORD", global = true, hide_env_values = true)]
    pub password: Option<String>,
    /// Vault folder to materialise into
    #[arg(long, env = "LSYNC_VAULT", global = true)]
    pub folder: Option<PathBuf>,
    /// Directory for the sync index (default: $XDG_STATE_HOME/lsync/<vault>)
    #[arg(long, env = "LSYNC_STATE_DIR", global = true)]
    pub state_dir: Option<PathBuf>,
    /// Local syncIgnoreRegEx (upstream format, patterns joined by `|[]|`)
    #[arg(long, env = "LSYNC_IGNORE_REGEX", global = true)]
    pub ignore_regex: Option<String>,
    /// Override syncMaxSizeInMB from the remote's PREFERRED tweaks
    #[arg(long, env = "LSYNC_MAX_SIZE_MB", global = true)]
    pub max_size_mb: Option<u64>,
    /// Chunk-fetch requests in flight
    #[arg(long, env = "LSYNC_CONCURRENCY", global = true)]
    pub concurrency: Option<usize>,
    /// Honour ignore files, as the plugin's useIgnoreFiles (true or false)
    #[arg(long, env = "LSYNC_USE_IGNORE_FILES", global = true, value_name = "BOOL")]
    pub use_ignore_files: Option<bool>,
    /// Ignore file names, as the plugin's ignoreFiles (comma-separated; default .gitignore)
    #[arg(long, env = "LSYNC_IGNORE_FILES", global = true, value_delimiter = ',')]
    pub ignore_files: Vec<String>,
    /// Local syncOnlyRegEx (upstream format): only matching paths sync
    #[arg(long, env = "LSYNC_SYNC_ONLY_REGEX", global = true)]
    pub sync_only_regex: Option<String>,
    /// Dot-folders or dot-files that sync after all (comma-separated; never .obsidian)
    #[arg(long, env = "LSYNC_SYNC_DOT_PATHS", global = true, value_delimiter = ',')]
    pub sync_dot_paths: Vec<String>,
    /// Where remote deletions go: vault (<folder>/.trash/, default), system or none
    #[arg(long, env = "LSYNC_TRASH", global = true)]
    pub trash: Option<String>,
    /// Sync only these vault folders (repeatable or comma-separated; case-insensitive)
    #[arg(
        long,
        env = "LSYNC_INCLUDE",
        global = true,
        value_delimiter = ',',
        conflicts_with = "exclude"
    )]
    pub include: Vec<String>,
    /// Sync everything except these vault folders (repeatable or comma-separated)
    #[arg(long, env = "LSYNC_EXCLUDE", global = true, value_delimiter = ',')]
    pub exclude: Vec<String>,
    /// Sync all folders (clears a configured include or exclude list)
    #[arg(long, global = true, conflicts_with_all = ["include", "exclude"])]
    pub all_folders: bool,
    /// Device name shown to other LiveSync clients
    #[arg(long, env = "LSYNC_DEVICE_NAME", global = true)]
    pub device_name: Option<String>,
}

/// The settings with the overrides applied.
pub fn merged(mut cfg: FileConfig, o: &Overrides) -> FileConfig {
    macro_rules! take {
        ($to:ident, $from:ident) => {
            if o.$from.is_some() {
                cfg.$to = o.$from.clone();
            }
        };
    }
    take!(url, url);
    take!(database, database);
    take!(username, username);
    take!(password, password);
    take!(vault, folder);
    take!(state_dir, state_dir);
    take!(ignore_regex, ignore_regex);
    take!(max_size_mb, max_size_mb);
    take!(concurrency, concurrency);
    take!(device_name, device_name);
    take!(use_ignore_files, use_ignore_files);
    take!(sync_only_regex, sync_only_regex);
    take!(trash, trash);
    if !o.ignore_files.is_empty() {
        cfg.ignore_files = Some(o.ignore_files.clone());
    }
    if !o.sync_dot_paths.is_empty() {
        cfg.sync_dot_paths = Some(o.sync_dot_paths.clone());
    }
    if !o.include.is_empty() {
        cfg.scope_mode = Some("include".into());
        cfg.scope_folders = Some(o.include.clone());
    } else if !o.exclude.is_empty() {
        cfg.scope_mode = Some("exclude".into());
        cfg.scope_folders = Some(o.exclude.clone());
    } else if o.all_folders {
        cfg.scope_mode = Some("all".into());
        cfg.scope_folders = None;
    }
    cfg
}

/// Where the selected settings came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A profile file as before `lsync.toml` (or none: flags and environment only).
    Profile { path: PathBuf },
    /// A vault in an accounts-and-vaults file.
    Vault { path: PathBuf },
}

/// The settings a command runs with, before the overrides.
#[derive(Debug)]
pub struct Selected {
    /// The vault's or profile's name (the default state folder's name).
    pub name: String,
    pub origin: Origin,
    pub file: FileConfig,
    pub mode: Mode,
    /// The vault's plugin-guard switch and timings (defaults for a profile).
    pub tuning: Tuning,
    /// Why the account's password could not be read, if it could not.
    pub password_error: Option<String>,
    /// The vault's end-to-end encryption passphrase, from its secret store.
    pub passphrase: Option<livesync_engine::Passphrase>,
    /// Why it could not be read, if the vault has one and it could not.
    pub passphrase_error: Option<String>,
    /// The vault accepts plaintext content in an encrypted database.
    pub accept_unencrypted_content: bool,
    /// Vaults there was no telling apart (nothing was named).
    pub ambiguous: Vec<String>,
}

/// What the command line says about where the settings are.
#[derive(Debug, Clone, Default)]
pub struct Selector {
    pub vault: Option<String>,
    pub profile: Option<String>,
}

/// A `--vault` value that is clearly a folder (the meaning `--vault` had
/// before vaults had names).
pub fn looks_like_path(v: &str) -> bool {
    v.contains('/') || v.contains('\\') || v.starts_with('~') || v.starts_with('.')
}

impl Selector {
    /// The name asked for, after the `--vault PATH` compatibility rule
    /// (moved into `folder`, with a warning).
    pub fn name(&mut self, o: &mut Overrides) -> Result<Option<String>> {
        if let Some(v) = self.vault.clone() {
            if looks_like_path(&v) {
                warn!("--vault {v} is a folder: use --folder (or LSYNC_VAULT) for the vault folder; --vault now takes a vault's name. Read as --folder this time");
                if o.folder.is_some() && o.folder.as_deref() != Some(Path::new(&v)) {
                    bail!("--vault {v} looks like a folder, and --folder (or LSYNC_VAULT) names another one; give the folder once, with --folder");
                }
                o.folder = Some(PathBuf::from(&v));
                self.vault = None;
            }
        }
        match (&self.vault, &self.profile) {
            (Some(v), Some(p)) if v != p => {
                bail!("--vault {v} and --profile {p} name different vaults; --profile is another name for --vault, give one")
            }
            (Some(v), _) => Ok(Some(v.clone())),
            (None, p) => Ok(p.clone()),
        }
    }
}

/// The format of a config file's text: accounts and vaults, or a profile.
/// A file with no keys at all is an accounts file when `empty_is_accounts`
/// (lsync's own `lsync.toml`, emptied by `vault rm` and `account rm` or
/// written by a migration with nothing to migrate); for `--config FILE` it
/// stays the empty profile it always was.
fn is_accounts_file(text: &str, path: &Path, empty_is_accounts: bool) -> Result<bool> {
    let table: toml::Table = toml::from_str(text)
        .map_err(|e| anyhow!("parsing {}: {}", path.display(), livesync_host::toml_de_error(text, &e)))?;
    // A profile's `vault` is its folder (a string); here it is a table.
    let section = |k: &str, v: &toml::Value| matches!(k, "account" | "vault" | "migration") && v.is_table();
    let new = table.iter().filter(|(k, v)| section(k, v)).count();
    if new > 0 && new < table.len() {
        bail!(
            "{}: has [account]/[vault] sections and profile keys ({}) at once; an lsync.toml holds only [account.NAME] and [vault.NAME] sections",
            path.display(),
            table.iter().filter(|(k, v)| !section(k, v)).map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    Ok(new > 0 || (empty_is_accounts && table.is_empty()))
}

/// The config and secrets locations for this run.
pub struct Setup {
    pub dirs: Dirs,
    /// The accounts-and-vaults file this run reads and edits.
    pub config_file: PathBuf,
    /// An explicit `--config` file.
    pub explicit: Option<PathBuf>,
    /// `lsync --system`: the system service's config, state and socket.
    pub system: bool,
}

impl Setup {
    pub fn new(explicit: Option<PathBuf>, system: bool) -> Setup {
        let mut dirs = if system { Dirs::system() } else { Dirs::from_env() };
        let config_file = match &explicit {
            Some(p) => {
                // `secrets.toml` and relative paths live beside an explicit file.
                if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                    dirs.config = dir.to_path_buf();
                } else {
                    dirs.config = PathBuf::from(".");
                }
                p.clone()
            }
            None => dirs.config_file(),
        };
        Setup {
            dirs,
            config_file,
            explicit,
            system,
        }
    }

    pub fn secrets(&self) -> SecretStores {
        // Root, for the system service: credentials go to /etc/credstore.encrypted.
        let writer = (self.system && cfg!(target_os = "linux") && crate::install::geteuid() == 0).then(|| {
            livesync_host::secret::CredentialWriter {
                dir: std::path::PathBuf::from(crate::install::CREDSTORE_ENCRYPTED),
                tool: std::sync::Arc::new(crate::install::SystemdCreds),
            }
        });
        SecretStores::with_credentials(self.dirs.config.join(SECRETS_FILE), writer)
    }

    /// Migrate the profile files on the first run (never for `--config`).
    /// A failed migration leaves the profiles as they are; they are read as
    /// before.
    pub fn migrate_if_needed(&self) {
        if self.explicit.is_some() || self.system || !migrate::needed(&self.dirs) {
            return;
        }
        let store = FileStore::new(self.dirs.secrets_file());
        match migrate::migrate(&self.dirs, &store) {
            Ok(Some(r)) => {
                for (p, why) in &r.skipped {
                    warn!(
                        "profile {} was not migrated: {why}. It is still read for --profile {}",
                        p.display(),
                        p.file_stem().unwrap_or_default().to_string_lossy()
                    );
                }
                if r.written && r.vaults.is_empty() {
                    warn!(
                        "no profile file could be migrated; {} records which and why (see also `lsync vault ls`), and the migration is not tried again while it exists",
                        self.dirs.config_file().display()
                    );
                } else if r.written {
                    info!(
                        "migrated {} profile(s) into {}: vaults {}; accounts {}; passwords in {}",
                        r.vaults.len(),
                        self.dirs.config_file().display(),
                        r.vaults.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "),
                        r.accounts.join(", "),
                        self.dirs.secrets_file().display()
                    );
                    warn!(
                        "the old profile files ({}) are left for going back to an older lsync, and still hold their passwords; delete them once you no longer need them",
                        r.vaults
                            .iter()
                            .map(|(_, p)| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            Ok(None) => {}
            Err(e) => warn!("the profile files were not migrated ({e}); they are read as before"),
        }
    }

    /// Whether the config file (which must exist) is an accounts-and-vaults
    /// file, as opposed to a profile.
    pub fn is_accounts_file(&self) -> Result<bool> {
        let text = std::fs::read_to_string(&self.config_file)
            .with_context(|| format!("reading {}", self.config_file.display()))?;
        is_accounts_file(&text, &self.config_file, self.explicit.is_none())
    }

    /// The accounts-and-vaults file, when there is one.
    pub fn load_accounts(&self) -> Result<Option<Config>> {
        let text = match std::fs::read_to_string(&self.config_file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.config_file.display())),
        };
        if !is_accounts_file(&text, &self.config_file, self.explicit.is_none())? {
            if self.explicit.is_none() {
                bail!(
                    "{} is not an accounts-and-vaults file (it has profile keys); move it to another name, such as default.toml, to use it as a profile",
                    self.config_file.display()
                );
            }
            return Ok(None);
        }
        Config::parse(&text, &self.dirs)
            .map(Some)
            .map_err(|e| anyhow!("{}: {e}", self.config_file.display()))
    }

    /// Pick the settings for a command that acts on one vault.
    pub fn select(&self, name: Option<String>, o: &Overrides) -> Result<Selected> {
        let accounts = self.load_accounts()?;
        let profile = |name: String, path: PathBuf| -> Result<Selected> {
            let file = livesync_host::legacy::load(&path).map_err(|e| anyhow!(e))?;
            Ok(Selected {
                name,
                origin: Origin::Profile { path },
                file,
                mode: Mode::TwoWay,
                tuning: Tuning::default(),
                password_error: None,
                passphrase: None,
                passphrase_error: None,
                accept_unencrypted_content: false,
                ambiguous: vec![],
            })
        };
        let Some(cfg) = accounts else {
            // No accounts-and-vaults file: profiles, as before.
            let name = name.unwrap_or_else(|| "default".into());
            let path = match &self.explicit {
                Some(p) => p.clone(),
                None => migrate::profile_path(&self.dirs, &name),
            };
            return profile(name, path);
        };
        let chosen = match name {
            Some(n) => {
                if cfg.vault.contains_key(&n) {
                    n
                } else {
                    // A profile that was not migrated is still read.
                    let path = migrate::profile_path(&self.dirs, &n);
                    if self.explicit.is_none() && (path.exists() || check_name("vault", &n).is_err()) {
                        return profile(n, path);
                    }
                    if self.explicit.is_none() && o.folder.is_some() {
                        // `--profile NAME` with the folder given: a profile
                        // made of flags and environment, as before.
                        return profile(n, path);
                    }
                    bail!(
                        "no vault named {n:?} in {} (vaults: {}). Add it with `lsync vault add {n} ...`; for a vault folder, use --folder",
                        self.config_file.display(),
                        list(cfg.vault.keys())
                    );
                }
            }
            None => {
                let implicit = cfg.implicit_vault().map(str::to_string);
                let default_profile = migrate::profile_path(&self.dirs, "default");
                if self.explicit.is_none() && !cfg.vault.contains_key("default") && default_profile.exists() {
                    match &cfg.migration {
                        // Skipped by the migration: with no name, it is read
                        // as before, never another vault in its place.
                        Some(m) if m.skipped("default.toml") => return profile("default".into(), default_profile),
                        // Migrated (and its vault since removed), or new:
                        // the vaults decide.
                        Some(_) => {}
                        // No record of a migration: there is no telling which
                        // one was meant.
                        None if !cfg.vault.is_empty() => bail!(
                            "{} and the vaults in {} ({}) could both be meant; name one with --vault NAME, or --profile default for the profile file",
                            default_profile.display(),
                            self.config_file.display(),
                            list(cfg.vault.keys())
                        ),
                        None => {}
                    }
                }
                match implicit {
                    // The folder given without a vault: flags and
                    // environment only, as before (never another vault's index).
                    Some(n) if n == "default" || o.folder.is_none() => n,
                    _ => {
                        let mut s = profile("default".into(), migrate::profile_path(&self.dirs, "default"))?;
                        if self.explicit.is_some() {
                            s.file = FileConfig::default();
                        }
                        if cfg.vault.len() > 1 {
                            s.ambiguous = cfg.vault.keys().cloned().collect();
                        }
                        return Ok(s);
                    }
                }
            }
        };
        let v = &cfg.vault[&chosen];
        let a = &cfg.account[&v.account];
        let scope = v.scope().map_err(|e| anyhow!("[vault.{chosen}]: {e}"))?;
        // Looked up even without a user here: --user or LSYNC_USER may give
        // one. Whether a missing password matters is decided in `resolve`,
        // with the overrides applied.
        let (password, password_error) = match a.secret_ref(&v.account) {
            Ok(r) => match self.secrets().get(&r) {
                Ok(Some(s)) => (Some(s.expose().to_string()), None),
                Ok(None) => (
                    None,
                    Some(format!(
                        "account {:?} has no password in its secret store ({}); set it with `lsync account passwd {}`",
                        v.account,
                        self.secrets().describe(&r),
                        v.account
                    )),
                ),
                Err(e) => (None, Some(format!("account {:?}: {e}", v.account))),
            },
            Err(e) => (None, Some(e)),
        };
        let (passphrase, passphrase_error) = match v.passphrase_ref() {
            Ok(None) => (None, None),
            Ok(Some(r)) => match self.secrets().get(&r) {
                Ok(Some(s)) => (Some(livesync_engine::Passphrase::new(s.expose())), None),
                Ok(None) => (
                    None,
                    Some(format!(
                        "vault {chosen:?} has no end-to-end encryption passphrase in its secret store ({}); set it with `lsync vault passphrase {chosen}`",
                        self.secrets().describe(&r)
                    )),
                ),
                Err(e) => (None, Some(format!("vault {chosen:?}: the passphrase: {e}"))),
            },
            Err(e) => (None, Some(format!("[vault.{chosen}]: {e}"))),
        };
        let file = FileConfig {
            url: Some(a.url.trim().to_string()),
            database: Some(v.database.clone()),
            username: a.username.clone(),
            password,
            vault: Some(v.folder_path(&self.dirs)),
            state_dir: Some(v.state_path(&chosen, &self.dirs)),
            ignore_regex: v.ignore_regex.clone(),
            max_size_mb: v.max_size_mb,
            concurrency: v.concurrency,
            use_ignore_files: v.use_ignore_files,
            ignore_files: v.ignore_files.clone(),
            sync_only_regex: v.sync_only_regex.clone(),
            sync_dot_paths: v.sync_dot_paths.clone(),
            trash: v.trash.clone(),
            scope_mode: Some(format!("{:?}", scope.mode).to_lowercase()),
            scope_folders: v.include.clone().or_else(|| v.exclude.clone()),
            device_name: v.device_name.clone(),
        };
        Ok(Selected {
            name: chosen,
            origin: Origin::Vault {
                path: self.config_file.clone(),
            },
            file,
            mode: v.mode,
            tuning: v.tuning(),
            password_error,
            passphrase,
            passphrase_error,
            accept_unencrypted_content: v.accept_unencrypted_content,
            ambiguous: vec![],
        })
    }
}

fn list<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<&str> = names.map(|s| s.as_str()).collect();
    if v.is_empty() {
        "none".into()
    } else {
        v.join(", ")
    }
}

/// A fully resolved vault.
pub struct Resolved {
    pub remote: livesync_remote::RemoteConfig,
    pub vault: PathBuf,
    pub state_dir: PathBuf,
    pub ignore_regex: Option<String>,
    pub max_size_mb: Option<u64>,
    pub concurrency: Option<usize>,
    pub scope: livesync_model::scope::Scope,
    pub device_name: Option<String>,
    pub what_syncs: livesync_host::WhatSyncs,
    /// The end-to-end encryption passphrase: `LSYNC_PASSPHRASE`, else the
    /// vault's from its secret store.
    pub passphrase: Option<livesync_engine::Passphrase>,
    pub accept_unencrypted_content: bool,
}

/// The environment variable that gives the passphrase to a run (for runs
/// without a vault in `lsync.toml`; a vault's `passphrase` is the usual way).
pub const PASSPHRASE_ENV: &str = "LSYNC_PASSPHRASE";

pub fn default_state_dir(dirs: &Dirs, name: &str) -> PathBuf {
    dirs.default_state_dir(name)
}

fn several(sel: &Selected) -> String {
    if sel.ambiguous.is_empty() {
        String::new()
    } else {
        format!(
            "; several vaults are configured ({}): choose one with --vault NAME",
            sel.ambiguous.join(", ")
        )
    }
}

pub fn resolve(sel: &Selected, cfg: &FileConfig, dirs: &Dirs, need_remote: bool) -> Result<Resolved> {
    let Some(vault) = cfg.vault.clone() else {
        bail!(
            "no vault folder configured (use --vault NAME, --folder, LSYNC_VAULT or `lsync vault add`){}",
            several(sel)
        )
    };
    let url = cfg.url.clone().unwrap_or_default();
    let database = cfg.database.clone().unwrap_or_default();
    if need_remote && (url.is_empty() || database.is_empty()) {
        bail!(
            "no remote configured (use --url/--db, LSYNC_URL/LSYNC_DB or `lsync vault add`){}",
            several(sel)
        );
    }
    // No user (after the overrides): a server without authentication, no
    // password needed.
    let has_user = cfg.username.as_deref().is_some_and(|u| !u.is_empty());
    if need_remote && has_user && cfg.password.is_none() {
        if let Some(e) = &sel.password_error {
            bail!("{e} (or give LSYNC_PASSWORD)");
        }
    }
    let passphrase = std::env::var(PASSPHRASE_ENV)
        .ok()
        .filter(|p| !p.is_empty())
        .map(livesync_engine::Passphrase::new)
        .or_else(|| sel.passphrase.clone());
    if need_remote && passphrase.is_none() {
        if let Some(e) = &sel.passphrase_error {
            bail!("{e} (or give {PASSPHRASE_ENV})");
        }
    }
    Ok(Resolved {
        remote: livesync_remote::RemoteConfig {
            url,
            database,
            username: cfg.username.clone(),
            password: cfg.password.clone(),
        },
        vault,
        state_dir: cfg
            .state_dir
            .clone()
            .unwrap_or_else(|| default_state_dir(dirs, &sel.name)),
        ignore_regex: cfg.ignore_regex.clone().filter(|s| !s.is_empty()),
        max_size_mb: cfg.max_size_mb,
        concurrency: cfg.concurrency,
        scope: cfg.scope().map_err(|e| anyhow!(e))?,
        device_name: cfg.device_name.clone(),
        what_syncs: what_syncs(cfg)?,
        passphrase,
        accept_unencrypted_content: sel.accept_unencrypted_content,
    })
}

/// A profile's ignore files, `sync_only_regex`, `sync_dot_paths` and `trash`.
pub fn what_syncs(cfg: &FileConfig) -> Result<livesync_host::WhatSyncs> {
    livesync_host::config::what_syncs(
        cfg.use_ignore_files,
        cfg.ignore_files.as_deref(),
        cfg.sync_only_regex.as_deref().filter(|s| !s.is_empty()),
        cfg.sync_dot_paths.as_deref(),
        cfg.trash.as_deref(),
    )
    .map_err(|e| anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(d: &Path) -> Setup {
        Setup {
            dirs: Dirs {
                home: Some(d.join("home")),
                config: d.join("config/lsync"),
                state: d.join("state/lsync"),
            },
            config_file: d.join("config/lsync/lsync.toml"),
            explicit: None,
            system: false,
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const TWO: &str = "[account.home]\nurl = \"https://couch.example.net\"\nusername = \"me\"\nsecret = \"env:LSYNC_TEST_UNSET_PASSWORD_VAR\"\n\n[vault.notes]\naccount = \"home\"\ndatabase = \"notes\"\nfolder = \"/srv/notes\"\n\n[vault.work]\naccount = \"home\"\ndatabase = \"work\"\nfolder = \"/srv/work\"\nmode = \"pull-only\"\n";

    #[test]
    fn a_vault_path_is_read_as_the_folder_with_a_warning() {
        let mut o = Overrides::default();
        let mut s = Selector {
            vault: Some("/srv/notes".into()),
            ..Default::default()
        };
        assert_eq!(s.name(&mut o).unwrap(), None);
        assert_eq!(o.folder.as_deref(), Some(Path::new("/srv/notes")));
        let mut s = Selector {
            vault: Some("notes".into()),
            profile: Some("work".into()),
        };
        assert!(s.name(&mut o).unwrap_err().to_string().contains("different vaults"));
        let mut s = Selector {
            vault: Some("./here".into()),
            ..Default::default()
        };
        let mut o = Overrides {
            folder: Some("/elsewhere".into()),
            ..Default::default()
        };
        assert!(s.name(&mut o).is_err());
        assert!(looks_like_path("~/Notes") && looks_like_path("a/b") && !looks_like_path("notes"));
    }

    #[test]
    fn vaults_are_chosen_by_name_and_never_guessed() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        write(&c.config_file, TWO);
        let o = Overrides::default();
        let s = c.select(Some("work".into()), &o).unwrap();
        assert_eq!(s.name, "work");
        assert_eq!(s.mode, Mode::PullOnly);
        assert_eq!(s.file.state_dir, Some(d.path().join("state/lsync/work")));
        assert!(s.password_error.as_deref().unwrap().contains("no password"));
        // Two vaults, none named default: nothing is guessed.
        let s = c.select(None, &o).unwrap();
        assert_eq!(s.ambiguous, vec!["notes".to_string(), "work".to_string()]);
        assert_eq!(s.file, FileConfig::default());
        let e = resolve(&s, &merged(s.file.clone(), &o), &c.dirs, true).err().unwrap();
        assert!(e.to_string().contains("choose one with --vault NAME"), "{e}");
        // An unknown name is refused, with the names there are.
        let e = c.select(Some("nope".into()), &o).unwrap_err();
        assert!(e.to_string().contains("vaults: notes, work"), "{e}");
    }

    #[test]
    fn one_vault_is_the_one_unless_a_folder_is_given() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        write(
            &c.config_file,
            "[account.home]\nurl = \"https://couch.example.net\"\n\n[vault.notes]\naccount = \"home\"\ndatabase = \"notes\"\nfolder = \"/srv/notes\"\n",
        );
        let s = c.select(None, &Overrides::default()).unwrap();
        assert_eq!(s.name, "notes");
        // With a folder of its own, a run never borrows the vault's index.
        let o = Overrides {
            folder: Some("/tmp/other".into()),
            ..Default::default()
        };
        let s = c.select(None, &o).unwrap();
        assert_eq!(s.name, "default");
        assert!(matches!(s.origin, Origin::Profile { .. }));
    }

    #[test]
    fn an_unmigrated_default_profile_is_read_when_no_vault_is_named() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        // default.toml was skipped (its URL comes from LSYNC_URL); work migrated.
        write(
            &c.dirs.config.join("default.toml"),
            "database = \"mine\"\nvault = \"/srv/mine\"\n",
        );
        write(
            &c.config_file,
            "[migration]\nmigrated = [\"work.toml\"]\nskipped = [\"default.toml\"]\n\n[account.home]\nurl = \"https://couch.example.net\"\nusername = \"me\"\n\n[vault.work]\naccount = \"home\"\ndatabase = \"work\"\nfolder = \"/srv/work\"\n",
        );
        let o = Overrides {
            url: Some("https://other.example.org".into()),
            ..Default::default()
        };
        let s = c.select(None, &o).unwrap();
        assert_eq!(s.name, "default");
        assert_eq!(
            s.origin,
            Origin::Profile {
                path: c.dirs.config.join("default.toml")
            }
        );
        assert_eq!(s.file.database.as_deref(), Some("mine"));
        let r = resolve(&s, &merged(s.file.clone(), &o), &c.dirs, true).unwrap();
        assert_eq!(r.state_dir, c.dirs.state.join("default"));
        assert_eq!(r.remote.password, None);
        // Named, work is work; a [vault.default] wins over the profile.
        assert_eq!(c.select(Some("work".into()), &o).unwrap().name, "work");
        write(
            &c.config_file,
            "[account.home]\nurl = \"https://couch.example.net\"\n\n[vault.default]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"/srv/d\"\n\n[vault.work]\naccount = \"home\"\ndatabase = \"work\"\nfolder = \"/srv/work\"\n",
        );
        let s = c.select(None, &o).unwrap();
        assert!(matches!(s.origin, Origin::Vault { .. }));
        assert_eq!(s.file.database.as_deref(), Some("d"));
    }

    #[test]
    fn a_vault_migrated_from_a_url_with_a_password_runs_with_it() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        write(
            &c.dirs.config.join("notes.toml"),
            "url = \"https://me:S3cret@couch.example.net\"\ndatabase = \"notes\"\nvault = \"/srv/notes\"\n",
        );
        c.migrate_if_needed();
        let o = Overrides::default();
        let s = c.select(None, &o).unwrap();
        assert!(matches!(s.origin, Origin::Vault { .. }));
        let r = resolve(&s, &merged(s.file.clone(), &o), &c.dirs, true).unwrap();
        assert_eq!(r.remote.url, "https://couch.example.net");
        assert_eq!(r.remote.username.as_deref(), Some("me"));
        assert_eq!(r.remote.password.as_deref(), Some("S3cret"));
    }

    #[test]
    fn an_account_without_a_user_needs_no_password() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        write(
            &c.config_file,
            "[account.open]\nurl = \"http://127.0.0.1:5984\"\n\n[vault.notes]\naccount = \"open\"\ndatabase = \"notes\"\nfolder = \"/srv/notes\"\n",
        );
        let o = Overrides::default();
        let s = c.select(None, &o).unwrap();
        let r = resolve(&s, &merged(s.file.clone(), &o), &c.dirs, true).unwrap();
        assert_eq!((r.remote.username, r.remote.password), (None, None));
        // A user given for the run: now the missing password matters.
        let o = Overrides {
            username: Some("bob".into()),
            ..Default::default()
        };
        let e = resolve(&s, &merged(s.file.clone(), &o), &c.dirs, true).err().unwrap();
        assert!(e.to_string().contains("no password"), "{e}");
    }

    #[test]
    fn a_stored_password_is_used_when_the_user_comes_from_the_run() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        // Migrated from a profile with a password and no username, run
        // with LSYNC_USER.
        write(
            &c.dirs.config.join("notes.toml"),
            "url = \"https://couch.example.net\"\ndatabase = \"notes\"\npassword = \"pw-notes\"\nvault = \"/srv/notes\"\n",
        );
        c.migrate_if_needed();
        let o = Overrides {
            username: Some("bob".into()),
            ..Default::default()
        };
        let s = c.select(None, &o).unwrap();
        assert!(matches!(s.origin, Origin::Vault { .. }));
        let r = resolve(&s, &merged(s.file.clone(), &o), &c.dirs, true).unwrap();
        assert_eq!(r.remote.username.as_deref(), Some("bob"));
        assert_eq!(r.remote.password.as_deref(), Some("pw-notes"));
    }

    #[test]
    fn a_migrated_default_profile_is_not_read_once_its_vault_is_gone() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        write(
            &c.dirs.config.join("default.toml"),
            "database = \"stale\"\nvault = \"/srv/notes\"\n",
        );
        let notes = "[account.home]\nurl = \"https://couch.example.net\"\n\n[vault.notes]\naccount = \"home\"\ndatabase = \"notes\"\nfolder = \"/srv/notes\"\n";
        let o = Overrides::default();
        // Migrated, then `vault rm default`: the only vault is the one.
        write(
            &c.config_file,
            &format!("[migration]\nmigrated = [\"default.toml\"]\n\n{notes}"),
        );
        let s = c.select(None, &o).unwrap();
        assert_eq!(s.name, "notes");
        // Skipped: read as before.
        write(
            &c.config_file,
            &format!("[migration]\nskipped = [\"default.toml\"]\n\n{notes}"),
        );
        assert_eq!(c.select(None, &o).unwrap().file.database.as_deref(), Some("stale"));
        // No record: neither is guessed.
        write(&c.config_file, notes);
        let e = c.select(None, &o).unwrap_err().to_string();
        assert!(e.contains("--vault NAME") && e.contains("--profile default"), "{e}");
    }

    #[test]
    fn an_empty_lsync_toml_is_an_empty_accounts_file() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        for text in ["", "# only comments\n\n# more\n"] {
            write(&c.config_file, text);
            assert_eq!(c.load_accounts().unwrap(), Some(Config::default()));
            let s = c.select(None, &Overrides::default()).unwrap();
            assert_eq!(s.file, FileConfig::default());
        }
    }

    #[test]
    fn profiles_still_work_without_and_beside_lsync_toml() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        let o = Overrides::default();
        // Nothing at all: flags and environment only, as before.
        let s = c.select(None, &o).unwrap();
        assert_eq!(s.name, "default");
        assert_eq!(s.file, FileConfig::default());
        write(&c.dirs.config.join("old.toml"), "database = \"x\"\n");
        let s = c.select(Some("old".into()), &o).unwrap();
        assert_eq!(s.file.database.as_deref(), Some("x"));
        // Beside an lsync.toml, a profile that was not migrated is still read.
        write(&c.config_file, TWO);
        let s = c.select(Some("old".into()), &o).unwrap();
        assert_eq!(s.file.database.as_deref(), Some("x"));
        assert!(matches!(s.origin, Origin::Profile { .. }));
    }

    #[test]
    fn an_explicit_config_file_is_read_in_either_format() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("elsewhere/a.toml");
        write(&p, "database = \"legacy\"\nvault = \"/srv/legacy\"\n");
        let explicit = |p: PathBuf| Setup {
            dirs: Dirs {
                config: p.parent().unwrap().to_path_buf(),
                ..ctx(d.path()).dirs
            },
            config_file: p.clone(),
            explicit: Some(p),
            system: false,
        };
        let c = explicit(p.clone());
        let s = c.select(None, &Overrides::default()).unwrap();
        assert_eq!(s.file.database.as_deref(), Some("legacy"));
        write(&p, TWO);
        let s = c.select(Some("notes".into()), &Overrides::default()).unwrap();
        assert_eq!(s.file.database.as_deref(), Some("notes"));
        write(&p, "database = \"x\"\n[vault.a]\n");
        assert!(c
            .select(None, &Overrides::default())
            .unwrap_err()
            .to_string()
            .contains("at once"));
        // Empty: an empty profile, as before (configure writes it).
        write(&p, "");
        assert!(matches!(c.select(None, &Overrides::default()).unwrap().origin, Origin::Profile { path } if path == p));
        // Missing: an empty profile, as before.
        let c = explicit(d.path().join("missing.toml"));
        assert_eq!(
            c.select(None, &Overrides::default()).unwrap().file,
            FileConfig::default()
        );
    }
}
