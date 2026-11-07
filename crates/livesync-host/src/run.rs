//! What one supervised engine needs, fully resolved (the password included),
//! and the checks that apply before an engine is opened for it.

use crate::paths::{nested_under, paths_overlap};
use livesync_engine::vaultfs::TrashMode;
use livesync_engine::EngineConfig;
use livesync_model::scope::{Scope, ScopeMode};
use livesync_remote::RemoteConfig;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What syncs besides the folder scope (spec D11): ignore files,
/// `syncOnlyRegEx`, the dot-path allowlist, and where remote deletions go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhatSyncs {
    /// Honour ignore files (upstream `useIgnoreFiles`).
    pub use_ignore_files: bool,
    /// Their names (upstream `ignoreFiles`).
    pub ignore_files: Vec<String>,
    /// Local `syncOnlyRegEx` (upstream format); empty for none.
    pub sync_only_regex: String,
    /// Dot-folders and dot-files that sync after all.
    pub sync_dot_paths: Vec<String>,
    pub trash: TrashMode,
}

impl Default for WhatSyncs {
    fn default() -> Self {
        WhatSyncs {
            use_ignore_files: false,
            ignore_files: vec![".gitignore".into()],
            sync_only_regex: String::new(),
            sync_dot_paths: vec![],
            trash: TrashMode::Vault,
        }
    }
}

/// One vault as the host runs it. Front-ends build it from their own
/// settings (the Android app's profile, a vault in `lsync.toml`) with the
/// secret already resolved; it is never stored.
#[derive(Clone, PartialEq, Eq)]
pub struct RunConfig {
    /// Stable ID: the slot key, and what the listener reports.
    pub id: String,
    /// Vault folder (absolute).
    pub vault_path: String,
    /// Folder for the sync index, outside the vault.
    pub state_dir: String,
    pub url: String,
    pub database: String,
    pub username: String,
    pub password: String,
    pub scope_mode: ScopeMode,
    pub scope_folders: Vec<String>,
    /// Follow the remote read-only (`lsync daemon --pull-only`).
    pub pull_only: bool,
    /// Device name advertised in the milestone.
    pub device_name: String,
    /// Local `syncIgnoreRegEx` (upstream format); empty for none.
    pub ignore_regex: String,
    /// Chunk-fetch requests in flight; `None` for the engine's default.
    pub concurrency: Option<usize>,
    /// Override of the remote's `syncMaxSizeInMB`.
    pub max_size_mb: Option<u64>,
    /// Let a pass delete files en masse (`lsync daemon --allow-mass-delete`);
    /// the app confirms single stops instead.
    pub allow_mass_delete: bool,
    /// When the scope narrows, remove the clean local copies that fell out.
    pub remove_out_of_scope_copies: bool,
    /// After a rebuild elsewhere, how long the remote must stay unchanged
    /// (`None`: the engine's default; tests shorten it).
    pub rebuild_quiet: Option<std::time::Duration>,
    pub what_syncs: WhatSyncs,
    /// The plugin guard's handover switch and the gate and connection timings.
    pub tuning: Tuning,
}

/// A vault's guard and connection settings (spec D8, D16). `None` keeps the
/// engine's platform default (`livesync_engine::engine::connection_defaults`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tuning {
    /// Run two-way although the vault's Self-hosted LiveSync plugin is
    /// enabled (a deliberate handover).
    pub allow_plugin: bool,
    /// How often the remote's version document, features and tweaks are
    /// checked again while running, in seconds (default 300).
    pub regate_secs: Option<u64>,
    /// The longpoll's heartbeat in seconds (default 10; Android 30).
    pub heartbeat_secs: Option<u64>,
    /// Seconds without a heartbeat before the connection is taken for dead
    /// (default 25, or 2.5 heartbeats, at most 89, when only the heartbeat is
    /// set; Android 75). Below `livesync_remote::READ_TIMEOUT`.
    pub longpoll_timeout_secs: Option<u64>,
    /// A wall-clock jump of this many seconds past the monotonic clock
    /// counts as sleep: reconnect and rescan (default 10; 0 turns it off;
    /// Android: off).
    pub clock_jump_secs: Option<u64>,
    /// Whether this front-end has a handover switch to name when the plugin
    /// guard refuses (`lsync` has; the Android app has not).
    pub handover: Handover,
}

/// How a front-end lets a vault run beside an enabled plugin, for the plugin
/// guard's message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Handover {
    /// `--allow-plugin`, or `allow_plugin = true` for the vault (`lsync`).
    #[default]
    Flag,
    /// No switch: the plugin has to be off (the Android app).
    Unavailable,
}

impl Tuning {
    /// The timings as the engine takes them.
    fn timings(&self) -> Result<(Duration, Duration, Duration, Option<Duration>), String> {
        use livesync_engine::engine::connection_defaults as d;
        let regate = self.regate_secs.map_or(d::REGATE, Duration::from_secs);
        if regate < Duration::from_secs(1) {
            return Err("regate_secs must be at least 1".into());
        }
        let heartbeat = self.heartbeat_secs.map_or(d::HEARTBEAT, Duration::from_secs);
        if heartbeat < Duration::from_secs(1) {
            return Err("heartbeat_secs must be at least 1".into());
        }
        // The HTTP client gives up on any response silent this long, as a
        // plain transport error: the longpoll's own silence limit has to
        // fire before it to be recognised (and reconnect on fresh connections).
        let read = livesync_remote::READ_TIMEOUT;
        let limit = read - Duration::from_secs(1);
        if heartbeat >= limit {
            return Err(format!(
                "heartbeat_secs ({}) must be below {} (the HTTP read timeout is {} s, and the silence limit has to fit between them)",
                heartbeat.as_secs(),
                limit.as_secs(),
                read.as_secs()
            ));
        }
        let timeout = match (self.longpoll_timeout_secs, self.heartbeat_secs) {
            (Some(t), _) => Duration::from_secs(t),
            (None, Some(_)) => d::LONGPOLL_TIMEOUT.max(heartbeat * 5 / 2).min(limit),
            (None, None) => d::LONGPOLL_TIMEOUT,
        };
        if timeout <= heartbeat {
            return Err(format!(
                "longpoll_timeout_secs ({}) must be longer than heartbeat_secs ({}), or every longpoll is taken for dead",
                timeout.as_secs(),
                heartbeat.as_secs()
            ));
        }
        if timeout >= read {
            return Err(format!(
                "longpoll_timeout_secs ({}) must be below {} s, the HTTP read timeout, or a dead connection is only noticed by that, without fresh connections",
                timeout.as_secs(),
                read.as_secs()
            ));
        }
        let jump = match self.clock_jump_secs {
            None => d::CLOCK_JUMP,
            Some(0) => None,
            Some(s) => Some(Duration::from_secs(s)),
        };
        Ok((regate, heartbeat, timeout, jump))
    }

    pub fn check(&self) -> Result<(), String> {
        self.timings().map(|_| ())
    }

    pub fn apply(&self, ec: &mut EngineConfig) -> Result<(), String> {
        let (regate, heartbeat, timeout, jump) = self.timings()?;
        ec.allow_plugin = self.allow_plugin;
        ec.plugin_handover = match self.handover {
            Handover::Flag => Some(livesync_engine::engine::PLUGIN_HANDOVER_CLI.into()),
            Handover::Unavailable => None,
        };
        ec.regate_interval = regate;
        ec.heartbeat = heartbeat;
        ec.longpoll_timeout = timeout;
        ec.clock_jump = jump;
        Ok(())
    }
}

impl std::fmt::Debug for RunConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunConfig")
            .field("id", &self.id)
            .field("vault_path", &self.vault_path)
            .field("state_dir", &self.state_dir)
            .field("url", &self.url)
            .field("database", &self.database)
            .field("username", &self.username)
            .field("password", &"***")
            .field("scope_mode", &self.scope_mode)
            .field("scope_folders", &self.scope_folders)
            .field("pull_only", &self.pull_only)
            .field("device_name", &self.device_name)
            .field("concurrency", &self.concurrency)
            .field("max_size_mb", &self.max_size_mb)
            .field("allow_mass_delete", &self.allow_mass_delete)
            .field("remove_out_of_scope_copies", &self.remove_out_of_scope_copies)
            .field("rebuild_quiet", &self.rebuild_quiet)
            .field("what_syncs", &self.what_syncs)
            .field("tuning", &self.tuning)
            .finish()
    }
}

impl RunConfig {
    pub fn scope(&self) -> Result<Scope, String> {
        let folders: Vec<String> = self
            .scope_folders
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Scope::new(self.scope_mode, &folders)
    }

    pub fn remote(&self) -> RemoteConfig {
        RemoteConfig {
            url: self.url.trim().to_string(),
            database: self.database.trim().to_string(),
            username: Some(self.username.clone()).filter(|s| !s.is_empty()),
            password: Some(self.password.clone()).filter(|s| !s.is_empty()),
        }
    }

    pub fn index_path(&self) -> PathBuf {
        Path::new(&self.state_dir).join("index.sqlite")
    }

    /// Check the fields that do not need the network or the disk. The
    /// state folder must be outside the vault folder.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_with(false)
    }

    /// [`RunConfig::validate`], where `hidden_state` also allows the state
    /// folder in a dot-folder inside the vault, which never syncs (as the
    /// CLI and `lsync.toml` always allowed; the Android app never does).
    pub fn validate_with(&self, hidden_state: bool) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("the profile has no ID".into());
        }
        let vault = Path::new(&self.vault_path);
        if !vault.is_absolute() {
            return Err(format!(
                "the vault folder must be an absolute path: {:?}",
                self.vault_path
            ));
        }
        if vault.parent().is_none() {
            return Err("the vault folder cannot be the filesystem root".into());
        }
        if !Path::new(&self.state_dir).is_absolute() {
            return Err("the state folder must be an absolute path".into());
        }
        if paths_overlap(&self.vault_path, &self.state_dir) {
            let hidden = nested_under(&self.vault_path, &self.state_dir).is_some_and(|rel| rel.starts_with('.'));
            if !hidden_state {
                return Err("the state folder must be outside the vault folder".into());
            }
            if !hidden {
                return Err("the state folder must be outside the vault folder (or in a dot-folder inside it)".into());
            }
        }
        check_url(&self.url)?;
        if self.database.trim().is_empty() {
            return Err("no database name".into());
        }
        self.scope()?;
        self.tuning.check()?;
        Ok(())
    }

    pub fn engine_config(&self) -> Result<EngineConfig, String> {
        let mut ec = EngineConfig::new(PathBuf::from(&self.vault_path), self.index_path());
        ec.scope = self.scope()?;
        ec.device_name = Some(self.device_name.trim().to_string()).filter(|s| !s.is_empty());
        ec.ignore_regex = Some(self.ignore_regex.clone()).filter(|s| !s.trim().is_empty());
        ec.max_size_mb = self.max_size_mb;
        let w = &self.what_syncs;
        ec.use_ignore_files = w.use_ignore_files;
        ec.ignore_files = w.ignore_files.clone();
        ec.sync_only_regex = Some(w.sync_only_regex.clone()).filter(|s| !s.trim().is_empty());
        ec.sync_dot_paths = w.sync_dot_paths.clone();
        ec.trash = w.trash;
        if let Some(c) = self.concurrency {
            ec.concurrency = c.max(1);
        }
        ec.allow_mass_delete = self.allow_mass_delete;
        ec.remove_out_of_scope_copies = self.remove_out_of_scope_copies;
        if let Some(q) = self.rebuild_quiet {
            ec.rebuild_quiet = q;
        }
        self.tuning.apply(&mut ec)?;
        Ok(ec)
    }
}

/// A server URL the remote client accepts.
pub fn check_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("the server URL must start with https:// or http://".into());
    }
    Ok(())
}

/// A user name and password written into a server URL
/// (`https://user:password@host/`), as the remote client reads them.
#[derive(Clone, PartialEq, Eq)]
pub struct UrlUserinfo {
    /// The URL without them.
    pub url: String,
    /// Percent-decoded; `None` when empty.
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for UrlUserinfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UrlUserinfo")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

/// The user name and password in `url`, if it has any (`None` otherwise),
/// decoded as the remote client decodes them.
pub fn split_userinfo(url: &str) -> Option<UrlUserinfo> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (userinfo, host) = rest[..end].rsplit_once('@')?;
    let (username, password) = livesync_remote::url_userinfo(url).unwrap_or_else(|| match userinfo.split_once(':') {
        Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
        None => (Some(userinfo.to_string()), None),
    });
    Some(UrlUserinfo {
        url: format!("{scheme}://{host}{}", &rest[end..]),
        username: username.filter(|s| !s.is_empty()),
        password: password.filter(|s| !s.is_empty()),
    })
}

/// A URL as it may be shown: a user name and password in it become `***@`.
pub fn url_for_display(url: &str) -> String {
    match split_userinfo(url) {
        Some(u) => match u.url.split_once("://") {
            Some((scheme, rest)) => format!("{scheme}://***@{rest}"),
            None => u.url,
        },
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_and_password_come_out_of_a_url() {
        let u = split_userinfo(" https://me:p%40ss%2Bw@couch.example.org:6984/base?x=1 ").unwrap();
        assert_eq!(u.url, "https://couch.example.org:6984/base?x=1");
        assert_eq!(u.username.as_deref(), Some("me"));
        assert_eq!(u.password.as_deref(), Some("p@ss+w"));
        assert!(!format!("{u:?}").contains("p@ss"));
        let u = split_userinfo("http://me@127.0.0.1:8411").unwrap();
        assert_eq!(
            (u.url.as_str(), u.username.as_deref(), u.password),
            ("http://127.0.0.1:8411", Some("me"), None)
        );
        assert_eq!(split_userinfo("https://couch.example.org/a@b"), None);
        assert_eq!(split_userinfo("https://couch.example.org"), None);
        assert_eq!(
            url_for_display("https://me:pw@couch.example.org/"),
            "https://***@couch.example.org/"
        );
        assert_eq!(
            url_for_display("https://couch.example.org/"),
            "https://couch.example.org/"
        );
    }

    fn cfg() -> RunConfig {
        RunConfig {
            id: "p1".into(),
            vault_path: "/storage/emulated/0/Documents/Obsidian/v".into(),
            state_dir: "/data/user/0/app/files/profiles/p1".into(),
            url: "https://couch.example.net".into(),
            database: "vault".into(),
            username: "u".into(),
            password: "secret".into(),
            scope_mode: ScopeMode::All,
            scope_folders: vec![],
            pull_only: true,
            device_name: "phone".into(),
            ignore_regex: String::new(),
            what_syncs: WhatSyncs::default(),
            concurrency: Some(4),
            max_size_mb: None,
            allow_mass_delete: false,
            remove_out_of_scope_copies: false,
            rebuild_quiet: None,
            tuning: Default::default(),
        }
    }

    #[test]
    fn validation() {
        assert!(cfg().validate().is_ok());
        let mut c = cfg();
        c.state_dir = format!("{}/state", c.vault_path);
        assert!(c.validate().unwrap_err().contains("outside the vault"));
        c.state_dir = c.vault_path.clone();
        assert!(c.validate().unwrap_err().contains("outside the vault"));
        // A dot-folder inside the vault never syncs: the CLI may keep the
        // index there, the app never.
        c.state_dir = format!("{}/.lsync-state", c.vault_path);
        assert!(c.validate().unwrap_err().contains("outside the vault"));
        assert!(c.validate_with(true).is_ok());
        c.state_dir = format!("{}/state", c.vault_path);
        assert!(c.validate_with(true).unwrap_err().contains("dot-folder"));
        let mut c = cfg();
        c.url = "couch.example.net".into();
        assert!(c.validate().is_err());
        let mut c = cfg();
        c.scope_mode = ScopeMode::Include;
        assert!(c.validate().unwrap_err().contains("at least one folder"));
        c.scope_folders = vec![" Notes/ ".into(), "".into()];
        assert!(c.validate().is_ok());
        assert_eq!(c.scope().unwrap().prefixes, vec!["notes".to_string()]);
        c.scope_folders = vec![".obsidian".into()];
        assert!(c.validate().is_err());
        let mut c = cfg();
        c.vault_path = "relative/vault".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn debug_never_shows_the_password() {
        assert!(!format!("{:?}", cfg()).contains("secret"));
    }

    #[test]
    fn engine_config_carries_scope_device_and_tuning() {
        let mut c = cfg();
        c.scope_mode = ScopeMode::Exclude;
        c.scope_folders = vec!["Private".into()];
        c.max_size_mb = Some(7);
        let ec = c.engine_config().unwrap();
        assert_eq!(ec.scope.prefixes, vec!["private".to_string()]);
        assert_eq!(ec.device_name.as_deref(), Some("phone"));
        assert!(ec.ignore_regex.is_none());
        assert!(ec.index_path.ends_with("p1/index.sqlite"));
        assert_eq!(ec.concurrency, 4);
        assert_eq!(ec.max_size_mb, Some(7));
    }
}
