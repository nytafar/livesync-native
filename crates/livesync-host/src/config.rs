//! Accounts and vaults (spec D13): `$XDG_CONFIG_HOME/lsync/lsync.toml`.
//!
//! ```toml
//! [account.home]
//! url = "https://couch.example.net"
//! username = "me"
//! secret = "file:home"          # the password is in the secret store
//!
//! [vault.notes]
//! account = "home"
//! database = "notes"
//! folder = "~/Notes"
//! exclude = ["Archive"]
//! ```
//!
//! Every key is optional unless stated, so later settings (trash, a dot
//! allowlist, ignore files, merge settings, timings) are new optional keys
//! and old files stay valid. Anything this version does not know is refused
//! with a message, never ignored.

use crate::paths::{comparable, nested_under, normalise, paths_overlap};
use crate::run::{check_url, split_userinfo, RunConfig, Tuning, WhatSyncs};
use crate::secret::SecretRef;
use livesync_engine::vaultfs::TrashMode;
use livesync_model::scope::{Scope, ScopeMode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "lsync.toml";
pub const SECRETS_FILE: &str = "secrets.toml";
/// A system service's config folder, without systemd's `ConfigurationDirectory=`.
pub const SYSTEM_CONFIG_DIR: &str = "/etc/lsync";
/// A system service's state folder, without systemd's `StateDirectory=`.
pub const SYSTEM_STATE_DIR: &str = "/var/lib/lsync";

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub account: BTreeMap<String, Account>,
    #[serde(default)]
    pub vault: BTreeMap<String, Vault>,
    /// What the migration from profile files did, kept so that a profile it
    /// skipped is still read as before and one it migrated is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<Migration>,
}

/// `[migration]`: the profile files (`NAME.toml`) a migration turned into
/// vaults, and the ones it left as they were.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Migration {
    #[serde(default)]
    pub migrated: Vec<String>,
    #[serde(default)]
    pub skipped: Vec<String>,
}

impl Migration {
    /// Whether the profile file `file_name` was left by the migration.
    pub fn skipped(&self, file_name: &str) -> bool {
        self.skipped.iter().any(|s| s == file_name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    /// Stable ID (kept across renames; keys a platform keyring entry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Server URL (required).
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Where the password is: `file:KEY`, `credential:NAME` or `env:VAR`
    /// (default `file:<account name>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Pull and push.
    #[default]
    TwoWay,
    /// Follow the remote; never write to it.
    PullOnly,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::TwoWay => "two-way",
            Mode::PullOnly => "pull-only",
        }
    }

    pub fn parse(s: &str) -> Result<Mode, String> {
        match s {
            "two-way" => Ok(Mode::TwoWay),
            "pull-only" => Ok(Mode::PullOnly),
            _ => Err("unknown mode (use two-way or pull-only)".into()),
        }
    }
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn yes() -> bool {
    true
}

fn is_default_mode(m: &Mode) -> bool {
    *m == Mode::TwoWay
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Vault {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The account's name (required).
    pub account: String,
    /// The CouchDB database (required).
    pub database: String,
    /// The vault folder, absolute or under `~/` (required).
    pub folder: String,
    /// The index folder (default `$XDG_STATE_HOME/lsync/<vault name>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<String>,
    /// Sync only these vault folders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    /// Sync everything except these vault folders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "is_default_mode")]
    pub mode: Mode,
    /// Local `syncIgnoreRegEx` in upstream format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_regex: Option<String>,
    /// Honour ignore files (upstream `useIgnoreFiles`). Default: on when
    /// `ignore_files` is set, otherwise off, as upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_ignore_files: Option<bool>,
    /// The ignore files' names (upstream `ignoreFiles`, default `.gitignore`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_files: Option<Vec<String>>,
    /// Local `syncOnlyRegEx` in upstream format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_only_regex: Option<String>,
    /// Dot-folders and dot-files that sync after all (never `.obsidian`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_dot_paths: Option<Vec<String>>,
    /// Where remote deletions go: `vault` (default), `system` or `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trash: Option<String>,
    /// Device name shown to other LiveSync clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    /// Whether the service runs it.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Tuning: override of the remote's `syncMaxSizeInMB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size_mb: Option<u64>,
    /// Tuning: chunk-fetch requests in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    /// Run two-way although this Obsidian vault's Self-hosted LiveSync
    /// plugin is enabled (a deliberate handover; see `--allow-plugin`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_plugin: bool,
    /// Tuning: how often the remote's version document, features and tweaks
    /// are checked again while running, in seconds (default 300).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regate_secs: Option<u64>,
    /// Tuning: the longpoll's heartbeat in seconds (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_secs: Option<u64>,
    /// Tuning: seconds without a heartbeat before reconnecting (default 25).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longpoll_timeout_secs: Option<u64>,
    /// Tuning: a wall-clock jump this long past the monotonic clock is
    /// taken for sleep: reconnect and rescan (default 10; 0 turns it off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_jump_secs: Option<u64>,
}

impl Vault {
    pub fn new(account: &str, database: &str, folder: &str) -> Vault {
        Vault {
            id: None,
            account: account.into(),
            database: database.into(),
            folder: folder.into(),
            state_dir: None,
            include: None,
            exclude: None,
            mode: Mode::TwoWay,
            ignore_regex: None,
            use_ignore_files: None,
            ignore_files: None,
            sync_only_regex: None,
            sync_dot_paths: None,
            trash: None,
            device_name: None,
            enabled: true,
            max_size_mb: None,
            concurrency: None,
            allow_plugin: false,
            regate_secs: None,
            heartbeat_secs: None,
            longpoll_timeout_secs: None,
            clock_jump_secs: None,
        }
    }

    /// The plugin guard's switch and the timings, as the host runs them.
    pub fn tuning(&self) -> Tuning {
        Tuning {
            allow_plugin: self.allow_plugin,
            regate_secs: self.regate_secs,
            heartbeat_secs: self.heartbeat_secs,
            longpoll_timeout_secs: self.longpoll_timeout_secs,
            clock_jump_secs: self.clock_jump_secs,
            handover: Default::default(),
        }
    }

    pub fn scope(&self) -> Result<Scope, String> {
        match (&self.include, &self.exclude) {
            (Some(_), Some(_)) => Err("set include or exclude, not both".into()),
            (Some(i), None) => Scope::new(ScopeMode::Include, i),
            (None, Some(e)) => Scope::new(ScopeMode::Exclude, e),
            (None, None) => Ok(Scope::all()),
        }
    }

    /// Ignore files, `sync_only_regex`, `sync_dot_paths` and `trash`, checked.
    pub fn what_syncs(&self) -> Result<WhatSyncs, String> {
        what_syncs(
            self.use_ignore_files,
            self.ignore_files.as_deref(),
            self.sync_only_regex.as_deref(),
            self.sync_dot_paths.as_deref(),
            self.trash.as_deref(),
        )
    }

    pub fn folder_path(&self, dirs: &Dirs) -> PathBuf {
        dirs.expand(&self.folder)
    }

    pub fn state_path(&self, name: &str, dirs: &Dirs) -> PathBuf {
        match &self.state_dir {
            Some(s) => dirs.expand(s),
            None => dirs.default_state_dir(name),
        }
    }

    /// What the host runs for this vault, with the account's password
    /// already looked up.
    pub fn run_config(
        &self,
        name: &str,
        account: &Account,
        password: Option<&str>,
        dirs: &Dirs,
    ) -> Result<RunConfig, String> {
        let scope = self.scope()?;
        Ok(RunConfig {
            id: name.to_string(),
            vault_path: self.folder_path(dirs).display().to_string(),
            state_dir: self.state_path(name, dirs).display().to_string(),
            url: account.url.trim().to_string(),
            database: self.database.trim().to_string(),
            username: account.username.clone().unwrap_or_default(),
            password: password.unwrap_or_default().to_string(),
            scope_mode: scope.mode,
            scope_folders: scope.prefixes,
            pull_only: self.mode == Mode::PullOnly,
            device_name: self.device_name.clone().unwrap_or_default(),
            ignore_regex: self.ignore_regex.clone().unwrap_or_default(),
            concurrency: self.concurrency,
            max_size_mb: self.max_size_mb,
            allow_mass_delete: false,
            remove_out_of_scope_copies: false,
            rebuild_quiet: None,
            what_syncs: self.what_syncs()?,
            tuning: self.tuning(),
        })
    }
}

/// Build and check [`WhatSyncs`] from the optional settings (shared by
/// vaults in `lsync.toml` and profile files).
pub fn what_syncs(
    use_ignore_files: Option<bool>,
    ignore_files: Option<&[String]>,
    sync_only_regex: Option<&str>,
    sync_dot_paths: Option<&[String]>,
    trash: Option<&str>,
) -> Result<WhatSyncs, String> {
    let d = WhatSyncs::default();
    let names = match ignore_files {
        Some(n) => livesync_engine::filters::check_ignore_file_names(n)?,
        None => d.ignore_files.clone(),
    };
    let use_ignore_files = use_ignore_files.unwrap_or(ignore_files.is_some());
    if use_ignore_files && names.is_empty() {
        return Err("use_ignore_files is on, but ignore_files names no file".into());
    }
    let dots = sync_dot_paths.unwrap_or_default().to_vec();
    livesync_model::path::DotAllow::new(&dots)?;
    let trash = match trash {
        Some(t) => TrashMode::parse(t)?,
        None => d.trash,
    };
    Ok(WhatSyncs {
        use_ignore_files,
        ignore_files: names,
        sync_only_regex: sync_only_regex.unwrap_or_default().to_string(),
        sync_dot_paths: dots,
        trash,
    })
}

impl Account {
    pub fn secret_ref(&self, name: &str) -> Result<SecretRef, String> {
        match &self.secret {
            Some(s) => SecretRef::parse(s),
            None => Ok(SecretRef::file(name)),
        }
    }

    /// The server as compared between accounts: trimmed, without a
    /// trailing slash, lower-cased.
    pub fn server_key(&self) -> String {
        server_key(&self.url)
    }
}

pub fn server_key(url: &str) -> String {
    url.trim().trim_end_matches('/').to_lowercase()
}

/// Where config, state and `~` are. Built once from the environment by the
/// front-end; tests build their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub home: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME/lsync`.
    pub config: PathBuf,
    /// `$XDG_STATE_HOME/lsync`.
    pub state: PathBuf,
}

impl Dirs {
    pub fn from_env() -> Dirs {
        let home = std::env::var_os("HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
        let base = home.clone().unwrap_or_else(|| PathBuf::from("."));
        let xdg = |var: &str, fallback: &str| {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| base.join(fallback))
        };
        Dirs {
            config: xdg("XDG_CONFIG_HOME", ".config").join("lsync"),
            state: xdg("XDG_STATE_HOME", ".local/state").join("lsync"),
            home,
        }
    }

    /// A system service's folders (`lsync --system`): systemd's
    /// `ConfigurationDirectory=` and `StateDirectory=` when it set them,
    /// else `/etc/lsync` and `/var/lib/lsync`.
    pub fn system() -> Dirs {
        Dirs::system_from(&|k| std::env::var_os(k))
    }

    pub fn system_from(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Dirs {
        // systemd lists several directories separated by ':'; ours is the first.
        let from = |var: &str, fallback: &str| {
            env(var)
                .and_then(|v| v.to_string_lossy().split(':').next().map(PathBuf::from))
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| PathBuf::from(fallback))
        };
        Dirs {
            home: env("HOME").filter(|v| !v.is_empty()).map(PathBuf::from),
            config: from("CONFIGURATION_DIRECTORY", SYSTEM_CONFIG_DIR),
            state: from("STATE_DIRECTORY", SYSTEM_STATE_DIR),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join(CONFIG_FILE)
    }

    pub fn secrets_file(&self) -> PathBuf {
        self.config.join(SECRETS_FILE)
    }

    /// A profile's or a vault's default index folder (the same path for
    /// both, so a migrated vault keeps its index).
    pub fn default_state_dir(&self, name: &str) -> PathBuf {
        self.state.join(name)
    }

    /// `~` and `~/...` against the home folder; other paths as given.
    pub fn expand(&self, p: &str) -> PathBuf {
        match (p.strip_prefix('~'), &self.home) {
            (Some(""), Some(h)) => h.clone(),
            (Some(rest), Some(h)) if rest.starts_with('/') => h.join(rest.trim_start_matches('/')),
            _ => PathBuf::from(p),
        }
    }
}

/// Account and vault names: letters, digits, `.`, `_` and `-`, starting
/// with a letter or digit (a vault's name is its state folder's name).
pub fn check_name(kind: &str, name: &str) -> Result<(), String> {
    let ok = name.len() <= 64
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{kind} name {name:?} is not valid: use letters, digits, '.', '_' and '-', starting with a letter or digit"
        ))
    }
}

impl Config {
    /// Parse and check `lsync.toml`'s text.
    pub fn parse(text: &str, dirs: &Dirs) -> Result<Config, String> {
        let table: toml::Table = toml::from_str(text).map_err(|e| crate::toml_de_error(text, &e))?;
        // A password written straight into the file gets a message of its own
        // (and is never quoted back).
        if table.contains_key("password") {
            return Err(
                "a password; passwords never go in lsync.toml. Remove the line and run `lsync account passwd NAME`"
                    .into(),
            );
        }
        for kind in ["account", "vault"] {
            if let Some(toml::Value::Table(entries)) = table.get(kind) {
                for (name, a) in entries {
                    if a.get("password").is_some() {
                        let account = if kind == "account" { name.as_str() } else { "NAME" };
                        return Err(format!(
                            "[{kind}.{name}] has a password; passwords never go in lsync.toml. Remove the line and run `lsync account passwd {account}`"
                        ));
                    }
                }
            }
        }
        // From the text again, for the line of a wrong type (and never the value).
        let cfg: Config = toml::from_str(text).map_err(|e| crate::toml_de_error(text, &e))?;
        cfg.validate(dirs)?;
        Ok(cfg)
    }

    /// Read `lsync.toml`; `None` when there is none.
    pub fn load(path: &Path, dirs: &Dirs) -> Result<Option<Config>, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => Config::parse(&s, dirs)
                .map(Some)
                .map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("reading {}: {e}", path.display())),
        }
    }

    /// Everything that can be checked without the network: names, required
    /// keys, scopes, secret references, and the folder rules between vaults.
    pub fn validate(&self, dirs: &Dirs) -> Result<(), String> {
        for (name, a) in &self.account {
            check_name("account", name)?;
            check_url(&a.url).map_err(|e| format!("[account.{name}]: {e}"))?;
            if split_userinfo(&a.url).is_some() {
                return Err(format!("[account.{name}]: {USERINFO}"));
            }
            a.secret_ref(name).map_err(|e| format!("[account.{name}]: {e}"))?;
        }
        for (name, v) in &self.vault {
            check_name("vault", name)?;
            let at = |e: String| format!("[vault.{name}]: {e}");
            if !self.account.contains_key(&v.account) {
                return Err(at(format!(
                    "no account {:?}; add it with `lsync account add {}` or set account to one of: {}",
                    v.account,
                    v.account,
                    list(self.account.keys())
                )));
            }
            if v.database.trim().is_empty() {
                return Err(at("no database".into()));
            }
            let folder = v.folder_path(dirs);
            if v.folder.trim().is_empty() || !folder.is_absolute() {
                return Err(at("folder must be an absolute path or start with ~/".into()));
            }
            if folder.parent().is_none() {
                return Err(at("the folder cannot be the filesystem root".into()));
            }
            let state = v.state_path(name, dirs);
            if !state.is_absolute() {
                return Err(at("state_dir must be an absolute path or start with ~/".into()));
            }
            check_state_placement(&folder, &state).map_err(at)?;
            check_scope_lists(v).map_err(at)?;
            v.what_syncs().map_err(at)?;
            v.tuning().check().map_err(at)?;
        }
        self.check_folders(dirs)
    }

    /// The folder rules: no two vaults share a folder or a state folder,
    /// and a folder inside another vault's folder is allowed only for a
    /// vault on a database of its own that the outer vault's scope excludes.
    pub fn check_folders(&self, dirs: &Dirs) -> Result<(), String> {
        let vaults: Vec<(&String, &Vault)> = self.vault.iter().collect();
        for (i, (an, a)) in vaults.iter().enumerate() {
            for (bn, b) in &vaults[i + 1..] {
                if let Some(msg) = self.folder_conflict(an, a, bn, b, dirs) {
                    return Err(msg);
                }
                let (sa, sb) = (a.state_path(an, dirs), b.state_path(bn, dirs));
                if paths_overlap(&sa.display().to_string(), &sb.display().to_string()) {
                    return Err(format!(
                        "vaults {an:?} and {bn:?} share the state folder {}; each vault needs its own (remove state_dir from one, or point it elsewhere)",
                        sa.display()
                    ));
                }
                for (vn, v, sn, s) in [(an, a, bn, &sb), (bn, b, an, &sa)] {
                    let f = v.folder_path(dirs);
                    if paths_overlap(&f.display().to_string(), &s.display().to_string()) {
                        return Err(format!(
                            "vault {sn:?}'s state folder {} is inside vault {vn:?}'s folder {}; move it outside",
                            s.display(),
                            f.display()
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn same_database(&self, a: &Vault, b: &Vault) -> bool {
        let server = |v: &Vault| self.account.get(&v.account).map(|acc| acc.server_key());
        server(a) == server(b) && a.database.trim() == b.database.trim()
    }

    fn folder_conflict(&self, an: &str, a: &Vault, bn: &str, b: &Vault, dirs: &Dirs) -> Option<String> {
        let (fa, fb) = (
            a.folder_path(dirs).display().to_string(),
            b.folder_path(dirs).display().to_string(),
        );
        if !paths_overlap(&fa, &fb) {
            return None;
        }
        if comparable(&fa) == comparable(&fb) {
            return Some(format!(
                "vaults {an:?} and {bn:?} use the same folder {fa}; each vault needs a folder of its own (two vaults may share a database, with different folders)"
            ));
        }
        // Order them: `outer` contains `inner`.
        let (on, o, of, inn, i, inf) = if nested_under(&fa, &fb).is_some() {
            (an, a, &fa, bn, b, &fb)
        } else {
            (bn, b, &fb, an, a, &fa)
        };
        let rel = display_rel(of, inf).unwrap_or_else(|| nested_under(of, inf).unwrap_or_default());
        if self.same_database(o, i) {
            return Some(format!(
                "vault {inn:?} ({inf}) is inside vault {on:?} ({of}) and syncs the same database; a nested vault must sync a database of its own. Give {inn:?} another database, or move its folder outside {of}"
            ));
        }
        let scope = o.scope().ok()?;
        if scope.may_contain_under(&rel) {
            let fix = match scope.mode {
                ScopeMode::Include => format!("narrow include in [vault.{on}] so it no longer covers {rel:?}"),
                _ => format!("add {rel:?} to exclude in [vault.{on}] (`lsync vault edit {on} --exclude ...`)"),
            };
            return Some(format!(
                "vault {inn:?} ({inf}) is inside vault {on:?} ({of}), whose folder scope syncs {rel:?}; {fix}, or move {inn:?} outside {of}"
            ));
        }
        None
    }

    /// The vault the CLI acts on when none is named: `default` if there is
    /// one, else the only vault.
    pub fn implicit_vault(&self) -> Option<&str> {
        if self.vault.contains_key("default") {
            return Some("default");
        }
        (self.vault.len() == 1).then(|| self.vault.keys().next().map(|s| s.as_str()))?
    }

    /// Vaults that use an account.
    pub fn vaults_of(&self, account: &str) -> Vec<&str> {
        self.vault
            .iter()
            .filter(|(_, v)| v.account == account)
            .map(|(n, _)| n.as_str())
            .collect()
    }
}

/// The message for a server URL with a user or password in it.
pub const USERINFO: &str = "the server URL has a user name or password in it; give the URL without them, the user with --user, and the password when asked (`lsync account add NAME --url URL --user USER`)";

/// The folder scope, with errors that say which entry is wrong but not what
/// it holds.
fn check_scope_lists(v: &Vault) -> Result<(), String> {
    for (key, mode, list) in [
        ("include", ScopeMode::Include, &v.include),
        ("exclude", ScopeMode::Exclude, &v.exclude),
    ] {
        for (i, entry) in list.iter().flatten().enumerate() {
            if Scope::new(mode, std::slice::from_ref(entry)).is_err() {
                return Err(format!(
                    "entry {} of {key} is not a folder that can sync: give a folder relative to the vault, not empty, without . or .. and not a dot-folder",
                    i + 1
                ));
            }
        }
    }
    v.scope().map(|_| ())
}

/// The state folder may be inside the vault only in a dot-folder (which
/// never syncs), as `lsync` has always allowed.
fn check_state_placement(folder: &Path, state: &Path) -> Result<(), String> {
    let (f, s) = (normalise(folder), normalise(state));
    if paths_overlap(&f.display().to_string(), &s.display().to_string()) {
        let hidden =
            nested_under(&f.display().to_string(), &s.display().to_string()).is_some_and(|rel| rel.starts_with('.'));
        if !hidden {
            return Err(format!(
                "the state folder {} must be outside the vault folder (or in a dot-folder inside it)",
                state.display()
            ));
        }
    }
    Ok(())
}

/// `inner` relative to `outer` in its own spelling, when it lies under it
/// lexically.
fn display_rel(outer: &str, inner: &str) -> Option<String> {
    let rel = normalise(Path::new(inner))
        .strip_prefix(normalise(Path::new(outer)))
        .ok()?
        .to_string_lossy()
        .into_owned();
    (!rel.is_empty()).then_some(rel)
}

pub(crate) fn list<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<&str> = names.map(|s| s.as_str()).collect();
    if v.is_empty() {
        "(none)".into()
    } else {
        v.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> Dirs {
        Dirs {
            home: Some("/home/u".into()),
            config: "/home/u/.config/lsync".into(),
            state: "/home/u/.local/state/lsync".into(),
        }
    }

    const BASE: &str = r#"
[account.home]
url = "https://couch.example.net"
username = "me"

[account.work]
url = "https://couch.example.org/"
username = "me"
secret = "env:LSYNC_WORK_PASSWORD"

[vault.notes]
account = "home"
database = "notes"
folder = "~/Notes"
exclude = ["Archive"]
"#;

    fn with(extra: &str) -> Result<Config, String> {
        Config::parse(&format!("{BASE}{extra}"), &dirs())
    }

    #[test]
    fn a_system_service_reads_systemds_folders_or_the_fixed_ones() {
        let none = Dirs::system_from(&|_| None);
        assert_eq!(none.config, PathBuf::from(SYSTEM_CONFIG_DIR));
        assert_eq!(none.state, PathBuf::from(SYSTEM_STATE_DIR));
        assert_eq!(none.config_file(), PathBuf::from("/etc/lsync/lsync.toml"));
        let set = Dirs::system_from(&|k| match k {
            "CONFIGURATION_DIRECTORY" => Some("/etc/lsync-a:/etc/other".into()),
            "STATE_DIRECTORY" => Some("relative".into()),
            "HOME" => Some("/var/lib/lsync-home".into()),
            _ => None,
        });
        assert_eq!(set.config, PathBuf::from("/etc/lsync-a"));
        assert_eq!(set.state, PathBuf::from(SYSTEM_STATE_DIR));
        assert_eq!(set.home, Some(PathBuf::from("/var/lib/lsync-home")));
    }

    #[test]
    fn a_full_config_parses_with_defaults() {
        let c = with("").unwrap();
        let v = &c.vault["notes"];
        assert!(v.enabled);
        assert_eq!(v.mode, Mode::TwoWay);
        assert_eq!(v.folder_path(&dirs()), PathBuf::from("/home/u/Notes"));
        assert_eq!(
            v.state_path("notes", &dirs()),
            PathBuf::from("/home/u/.local/state/lsync/notes")
        );
        assert_eq!(c.account["home"].secret_ref("home").unwrap(), SecretRef::file("home"));
        assert_eq!(c.implicit_vault(), Some("notes"));
        let rc = v.run_config("notes", &c.account["home"], Some("pw"), &dirs()).unwrap();
        assert_eq!(rc.scope_mode, ScopeMode::Exclude);
        assert_eq!(rc.scope_folders, vec!["archive".to_string()]);
        assert!(rc.validate().is_ok());
    }

    #[test]
    fn what_syncs_settings() {
        use livesync_engine::vaultfs::TrashMode;
        let c = with("").unwrap();
        let w = c.vault["notes"].what_syncs().unwrap();
        assert!(!w.use_ignore_files, "off by default, as upstream");
        assert_eq!(w.ignore_files, vec![".gitignore".to_string()]);
        assert_eq!(w.trash, TrashMode::Vault);
        let v = |extra: &str| {
            Config::parse(
                &format!("{BASE}[vault.x]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"/x\"\n{extra}"),
                &dirs(),
            )
        };
        let c = v(
            "ignore_files = [\".gitignore\", \".obsidianignore\"]\ntrash = \"none\"\nsync_dot_paths = [\".notes\"]\n",
        )
        .unwrap();
        let w = c.vault["x"].what_syncs().unwrap();
        assert!(w.use_ignore_files, "naming ignore files turns them on");
        assert_eq!(w.ignore_files.len(), 2);
        assert_eq!(w.trash, TrashMode::None);
        let rc = c.vault["x"].run_config("x", &c.account["home"], None, &dirs()).unwrap();
        let ec = rc.engine_config().unwrap();
        assert!(ec.use_ignore_files && ec.trash == TrashMode::None && ec.sync_dot_paths == [".notes"]);
        let c = v("ignore_files = [\".gitignore\"]\nuse_ignore_files = false\n").unwrap();
        assert!(!c.vault["x"].what_syncs().unwrap().use_ignore_files);
        for bad in [
            "trash = \"bin\"\n",
            "sync_dot_paths = [\".obsidian\"]\n",
            "ignore_files = [\"a/.gitignore\"]\n",
        ] {
            assert!(v(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn unknown_keys_and_passwords_are_refused() {
        let e = with("typo = 1\n").unwrap_err();
        assert!(e.contains("typo"), "{e}");
        let e = Config::parse("[account.a]\nurl = \"https://x\"\npassword = \"pw\"\n", &dirs()).unwrap_err();
        assert!(e.contains("never go in lsync.toml") && !e.contains("\"pw\""), "{e}");
        let e =
            with("[vault.x]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"/x\"\nscope = \"all\"\n").unwrap_err();
        assert!(e.contains("scope"), "{e}");
        let e = with("[service]\n").unwrap_err();
        assert!(e.contains("service"), "{e}");
        let e = with("[vault.x]\naccount = \"nope\"\ndatabase = \"d\"\nfolder = \"/x\"\n").unwrap_err();
        assert!(e.contains("no account \"nope\""), "{e}");
        let e = with("[vault.x]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"rel\"\n").unwrap_err();
        assert!(e.contains("absolute"), "{e}");
        let e = with(
            "[vault.x]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"/x\"\ninclude = [\"a\"]\nexclude = [\"b\"]\n",
        )
        .unwrap_err();
        assert!(e.contains("not both"), "{e}");
        let e = with("[vault.x]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"/x\"\nmode = \"push-only\"\n")
            .unwrap_err();
        // The value is not quoted back: the line and the choices are.
        assert!(
            !e.contains("push-only") && e.contains("line ") && e.contains("pull-only"),
            "{e}"
        );
        let e = with("[vault.\"a b\"]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"/x\"\n").unwrap_err();
        assert!(e.contains("not valid"), "{e}");
    }

    #[test]
    fn errors_never_quote_the_values_they_refuse() {
        let leaky = "Hunter2-Sup3rSecret";
        for (text, says) in [
            (format!("[account.x]\nurl = \"https://couch.example.org\"\nsecret = \"{leaky}\"\n"), "STORE:KEY"),
            (format!("[account.x]\nurl = \"https://couch.example.org\"\nsecret = \"env:{leaky}\"\n"), "env:KEY"),
            (format!("[account.x]\nurl = \"https://couch.example.org\"\nusername = {}\n", 98765432), "line "),
            (format!("[account.x]\nurl = \"{leaky}\"\n"), "https://"),
            (format!("[vault.x]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"{leaky}\"\n"), "absolute"),
            (format!("[vault.x]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"/x\"\nstate_dir = \"{leaky}\"\n"), "absolute"),
            (format!("[vault.x]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"/x\"\ninclude = [\"a\", \"../{leaky}\"]\n"), "entry 2 of include"),
            (format!("[vault.x]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"/x\"\nmode = \"{leaky}\"\n"), "pull-only"),
            (format!("[vault.x]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"/x\"\nenabled = \"{leaky}\"\n"), "line "),
        ] {
            let e = with(&text).unwrap_err();
            assert!(!e.contains("Hunter2") && !e.contains("98765432"), "{e}");
            assert!(e.contains(says), "{text}: {e}");
        }
    }

    #[test]
    fn a_url_with_a_user_or_password_is_refused() {
        for url in [
            "https://me:S3cret@couch.example.org",
            "https://me@couch.example.org/",
            "http://:S3cret@127.0.0.1:8411",
        ] {
            let e = with(&format!("[account.x]\nurl = \"{url}\"\n")).unwrap_err();
            assert!(e.contains("--user") && e.contains("[account.x]"), "{e}");
            assert!(!e.contains("S3cret") && !e.contains("me@"), "{e}");
        }
        // An @ in the path is not a user.
        with("[account.x]\nurl = \"https://couch.example.org/a@b\"\n").unwrap();
    }

    #[test]
    fn two_vaults_may_share_a_database_with_different_folders_and_scopes() {
        let c = with(
            "[vault.notes-b]\naccount = \"home\"\ndatabase = \"notes\"\nfolder = \"~/Other\"\ninclude = [\"Archive\"]\n",
        )
        .unwrap();
        assert_eq!(c.vault.len(), 2);
        assert_eq!(c.implicit_vault(), None);
        assert_eq!(c.vaults_of("home"), vec!["notes", "notes-b"]);
    }

    #[test]
    fn the_same_folder_is_refused() {
        let e = with("[vault.again]\naccount = \"work\"\ndatabase = \"x\"\nfolder = \"/home/u/notes/\"\n").unwrap_err();
        assert!(e.contains("use the same folder") && e.contains("\"again\""), "{e}");
    }

    #[test]
    fn a_nested_vault_needs_its_own_database_and_the_parents_exclude() {
        // Excluded by the parent and on another database: allowed.
        with("[vault.archive]\naccount = \"work\"\ndatabase = \"archive\"\nfolder = \"~/Notes/Archive/old\"\n")
            .unwrap();
        with("[vault.archive]\naccount = \"home\"\ndatabase = \"archive\"\nfolder = \"~/Notes/Archive\"\n").unwrap();
        // The same database: refused.
        let e = with("[vault.archive]\naccount = \"home\"\ndatabase = \"notes\"\nfolder = \"~/Notes/Archive\"\n")
            .unwrap_err();
        assert!(
            e.contains("syncs the same database") && e.contains("a database of its own"),
            "{e}"
        );
        // Not excluded by the parent: refused, with what to add.
        let e =
            with("[vault.proj]\naccount = \"work\"\ndatabase = \"proj\"\nfolder = \"~/Notes/Projects\"\n").unwrap_err();
        assert!(
            e.contains("vault \"proj\"") && e.contains("add \"Projects\" to exclude in [vault.notes]"),
            "{e}"
        );
        // The outer one listed second: still found, the same message.
        let e = Config::parse(
            "[account.a]\nurl = \"https://x\"\n[vault.a-inner]\naccount = \"a\"\ndatabase = \"i\"\nfolder = \"/v/o/i\"\n[vault.b-outer]\naccount = \"a\"\ndatabase = \"o\"\nfolder = \"/v/o\"\ninclude = [\"i\", \"j\"]\n",
            &dirs(),
        )
        .unwrap_err();
        assert!(
            e.contains("inside vault \"b-outer\"") && e.contains("narrow include"),
            "{e}"
        );
    }

    #[test]
    fn state_folders_may_not_be_shared_or_inside_a_vault() {
        let e = with("[vault.b]\naccount = \"home\"\ndatabase = \"notes\"\nfolder = \"/srv/b\"\nstate_dir = \"/home/u/.local/state/lsync/notes\"\n")
            .unwrap_err();
        assert!(e.contains("share the state folder"), "{e}");
        let e = with("[vault.b]\naccount = \"home\"\ndatabase = \"notes\"\nfolder = \"/srv/b\"\nstate_dir = \"/home/u/Notes/.b-state\"\n")
            .unwrap_err();
        assert!(e.contains("inside vault \"notes\"'s folder"), "{e}");
        let e = with(
            "[vault.b]\naccount = \"home\"\ndatabase = \"notes\"\nfolder = \"/srv/b\"\nstate_dir = \"/srv/b/state\"\n",
        )
        .unwrap_err();
        assert!(e.contains("must be outside the vault folder"), "{e}");
        with(
            "[vault.b]\naccount = \"home\"\ndatabase = \"notes\"\nfolder = \"/srv/b\"\nstate_dir = \"/srv/b/.lsync\"\n",
        )
        .unwrap();
    }

    #[test]
    fn a_vault_named_default_is_the_implicit_one() {
        let c = with("[vault.default]\naccount = \"home\"\ndatabase = \"d\"\nfolder = \"/srv/d\"\n").unwrap();
        assert_eq!(c.implicit_vault(), Some("default"));
    }

    #[test]
    fn serialising_skips_defaults() {
        let v = Vault::new("home", "notes", "~/Notes");
        let text = toml::to_string(&v).unwrap();
        assert_eq!(text, "account = \"home\"\ndatabase = \"notes\"\nfolder = \"~/Notes\"\n");
    }
}
