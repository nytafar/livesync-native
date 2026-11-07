//! What one supervised engine needs, fully resolved (the password included),
//! and the checks that apply before an engine is opened for it.

use crate::paths::paths_overlap;
use livesync_engine::EngineConfig;
use livesync_model::scope::{Scope, ScopeMode};
use livesync_remote::RemoteConfig;
use std::path::{Path, PathBuf};

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

    /// Check the fields that do not need the network or the disk.
    pub fn validate(&self) -> Result<(), String> {
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
            return Err("the state folder must be outside the vault folder".into());
        }
        check_url(&self.url)?;
        if self.database.trim().is_empty() {
            return Err("no database name".into());
        }
        self.scope()?;
        Ok(())
    }

    pub fn engine_config(&self) -> Result<EngineConfig, String> {
        let mut ec = EngineConfig::new(PathBuf::from(&self.vault_path), self.index_path());
        ec.scope = self.scope()?;
        ec.device_name = Some(self.device_name.trim().to_string()).filter(|s| !s.is_empty());
        ec.ignore_regex = Some(self.ignore_regex.clone()).filter(|s| !s.trim().is_empty());
        ec.max_size_mb = self.max_size_mb;
        if let Some(c) = self.concurrency {
            ec.concurrency = c.max(1);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
            concurrency: Some(4),
            max_size_mb: None,
        }
    }

    #[test]
    fn validation() {
        assert!(cfg().validate().is_ok());
        let mut c = cfg();
        c.state_dir = format!("{}/.state", c.vault_path);
        assert!(c.validate().unwrap_err().contains("outside the vault"));
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
