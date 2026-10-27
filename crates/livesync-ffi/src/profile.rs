//! A profile as the app hands it over, and the checks that apply before an
//! engine is opened for it.

use livesync_engine::EngineConfig;
use livesync_model::scope::{Scope, ScopeMode};
use livesync_remote::RemoteConfig;
use std::path::{Component, Path, PathBuf};

/// Folder-scope mode (spec "Folder scope").
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ScopeKind {
    All,
    Include,
    Exclude,
}

/// Everything one engine needs. The app resolves the password from its
/// SecretStore just before handing this over; it is never stored here.
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct ProfileConfig {
    /// Stable profile ID (also keys the app's secret and state folder).
    pub id: String,
    /// Vault folder (absolute, e.g. `/storage/emulated/0/Documents/Obsidian/notes`).
    pub vault_path: String,
    /// Folder for the sync index, outside the vault (app-private storage).
    pub state_dir: String,
    pub url: String,
    pub database: String,
    pub username: String,
    pub password: String,
    pub scope_kind: ScopeKind,
    pub scope_folders: Vec<String>,
    /// Follow the remote read-only (`lsync daemon --pull-only`).
    pub pull_only: bool,
    /// Device name advertised in the milestone.
    pub device_name: String,
    /// Local `syncIgnoreRegEx` (upstream format); empty for none.
    pub ignore_regex: String,
}

impl std::fmt::Debug for ProfileConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileConfig")
            .field("id", &self.id)
            .field("vault_path", &self.vault_path)
            .field("state_dir", &self.state_dir)
            .field("url", &self.url)
            .field("database", &self.database)
            .field("username", &self.username)
            .field("password", &"***")
            .field("scope_kind", &self.scope_kind)
            .field("scope_folders", &self.scope_folders)
            .field("pull_only", &self.pull_only)
            .field("device_name", &self.device_name)
            .finish()
    }
}

impl ProfileConfig {
    pub fn scope(&self) -> Result<Scope, String> {
        let mode = match self.scope_kind {
            ScopeKind::All => ScopeMode::All,
            ScopeKind::Include => ScopeMode::Include,
            ScopeKind::Exclude => ScopeMode::Exclude,
        };
        let folders: Vec<String> = self
            .scope_folders
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Scope::new(mode, &folders)
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
        let url = self.url.trim();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err("the server URL must start with https:// or http://".into());
        }
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
        // A phone: fewer chunk fetches in flight than on a desktop.
        ec.concurrency = 4;
        Ok(ec)
    }
}

/// The path as compared for overlap: canonical when it exists (resolves
/// `/sdcard` and other links), otherwise lexically normalised; then
/// lower-cased, because Android shared storage folds case (two spellings of
/// one folder are one folder there, and refusing them elsewhere is harmless).
pub fn comparable(path: &str) -> PathBuf {
    let p = normalise(Path::new(path));
    // Canonicalise the longest existing ancestor, so a folder that does not
    // exist yet under a link (`/sdcard/...`) compares like its siblings.
    let mut base = p.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let resolved = loop {
        if let Ok(c) = std::fs::canonicalize(&base) {
            break rest.iter().rev().fold(c, |acc, seg| acc.join(seg));
        }
        match (base.file_name().map(|s| s.to_owned()), base.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                base = parent.to_path_buf();
            }
            _ => break p.clone(),
        }
    };
    PathBuf::from(resolved.to_string_lossy().to_lowercase())
}

fn normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether two folders are the same, or one lies inside the other (spec
/// "Designed for multiple vaults": vault folders may not overlap).
#[uniffi::export]
pub fn paths_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (comparable(a), comparable(b));
    a.starts_with(&b) || b.starts_with(&a)
}

/// Normalise and check a folder list for a scope mode; returns the folders
/// as the engine will match them (lower-cased, without slashes at either end).
#[uniffi::export]
pub fn check_scope(kind: ScopeKind, folders: Vec<String>) -> Result<Vec<String>, crate::LiveSyncError> {
    let probe = ProfileConfig {
        id: "x".into(),
        vault_path: "/x".into(),
        state_dir: "/y".into(),
        url: String::new(),
        database: String::new(),
        username: String::new(),
        password: String::new(),
        scope_kind: kind,
        scope_folders: folders,
        pull_only: true,
        device_name: String::new(),
        ignore_regex: String::new(),
    };
    probe
        .scope()
        .map(|s| s.prefixes)
        .map_err(|msg| crate::LiveSyncError::Invalid { msg })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_is_component_wise_and_case_insensitive() {
        assert!(paths_overlap(
            "/storage/emulated/0/Documents/Obsidian/a",
            "/storage/emulated/0/Documents/Obsidian/a/"
        ));
        assert!(paths_overlap(
            "/storage/emulated/0/Documents/Obsidian/a",
            "/storage/emulated/0/documents/obsidian/A"
        ));
        assert!(paths_overlap(
            "/storage/emulated/0/Documents/Obsidian",
            "/storage/emulated/0/Documents/Obsidian/a"
        ));
        assert!(paths_overlap("/x/a/b", "/x/a"));
        assert!(
            !paths_overlap("/x/a", "/x/ab"),
            "a sibling with a common name prefix is not inside"
        );
        assert!(!paths_overlap("/x/a", "/x/b"));
        assert!(paths_overlap("/x/a/../b", "/x/b"));
    }

    #[test]
    fn overlap_follows_links() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, d.path().join("link")).unwrap();
        assert!(paths_overlap(
            &d.path().join("link").display().to_string(),
            &real.join("inner").display().to_string()
        ));
        // Neither folder exists yet; the link above them still resolves.
        assert!(paths_overlap(
            &d.path().join("link/new/a").display().to_string(),
            &real.join("new").display().to_string()
        ));
        assert!(!paths_overlap(
            &d.path().join("link/new/a").display().to_string(),
            &real.join("new/b").display().to_string()
        ));
    }

    fn cfg() -> ProfileConfig {
        ProfileConfig {
            id: "p1".into(),
            vault_path: "/storage/emulated/0/Documents/Obsidian/v".into(),
            state_dir: "/data/user/0/app/files/profiles/p1".into(),
            url: "https://couch.example.net".into(),
            database: "vault".into(),
            username: "u".into(),
            password: "secret".into(),
            scope_kind: ScopeKind::All,
            scope_folders: vec![],
            pull_only: true,
            device_name: "phone".into(),
            ignore_regex: String::new(),
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
        c.scope_kind = ScopeKind::Include;
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
    fn engine_config_carries_scope_and_device() {
        let mut c = cfg();
        c.scope_kind = ScopeKind::Exclude;
        c.scope_folders = vec!["Private".into()];
        let ec = c.engine_config().unwrap();
        assert_eq!(ec.scope.prefixes, vec!["private".to_string()]);
        assert_eq!(ec.device_name.as_deref(), Some("phone"));
        assert!(ec.ignore_regex.is_none());
        assert!(ec.index_path.ends_with("p1/index.sqlite"));
    }
}
