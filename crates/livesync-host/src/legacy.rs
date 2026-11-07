//! The single-profile config files that came before `lsync.toml`
//! (`$XDG_CONFIG_HOME/lsync/<profile>.toml`, one remote and one vault each,
//! the password inline). Still read for `--config FILE` and for profiles
//! that were not migrated; [`crate::migrate`] turns them into accounts and
//! vaults.

use livesync_model::scope::{Scope, ScopeMode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub url: Option<String>,
    pub database: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub vault: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    /// Local `syncIgnoreRegEx` in upstream format (`|[]|`-separated).
    pub ignore_regex: Option<String>,
    pub max_size_mb: Option<u64>,
    pub concurrency: Option<usize>,
    /// Honour ignore files (upstream `useIgnoreFiles`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_ignore_files: Option<bool>,
    /// The ignore files' names (upstream `ignoreFiles`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_files: Option<Vec<String>>,
    /// Local `syncOnlyRegEx` in upstream format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_only_regex: Option<String>,
    /// Dot-folders and dot-files that sync after all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_dot_paths: Option<Vec<String>>,
    /// `vault` (default), `system` or `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trash: Option<String>,
    /// Folder scope: `all` (default), `include` or `exclude`.
    pub scope_mode: Option<String>,
    /// Vault-relative folders for `include`/`exclude` (case-insensitive).
    pub scope_folders: Option<Vec<String>>,
    /// Device name advertised in the milestone.
    pub device_name: Option<String>,
}

impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile")
            .field("url", &self.url)
            .field("database", &self.database)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("vault", &self.vault)
            .field("state_dir", &self.state_dir)
            .field("ignore_regex", &self.ignore_regex)
            .field("max_size_mb", &self.max_size_mb)
            .field("concurrency", &self.concurrency)
            .field("use_ignore_files", &self.use_ignore_files)
            .field("ignore_files", &self.ignore_files)
            .field("sync_only_regex", &self.sync_only_regex)
            .field("sync_dot_paths", &self.sync_dot_paths)
            .field("trash", &self.trash)
            .field("scope_mode", &self.scope_mode)
            .field("scope_folders", &self.scope_folders)
            .field("device_name", &self.device_name)
            .finish()
    }
}

impl Profile {
    /// The scope mode as written, checked.
    pub fn scope_mode(&self) -> Result<ScopeMode, String> {
        match self.scope_mode.as_deref().unwrap_or("all") {
            "all" | "" => Ok(ScopeMode::All),
            "include" => Ok(ScopeMode::Include),
            "exclude" => Ok(ScopeMode::Exclude),
            _ => Err("unknown scope_mode (use all, include or exclude)".into()),
        }
    }

    /// The folder scope this profile describes.
    pub fn scope(&self) -> Result<Scope, String> {
        let mode = self.scope_mode()?;
        let folders = self.scope_folders.as_deref().unwrap_or_default();
        if mode != ScopeMode::All {
            // Which entry, never what it holds.
            for (i, f) in folders.iter().enumerate() {
                if Scope::new(mode, std::slice::from_ref(f)).is_err() {
                    return Err(format!(
                        "entry {} of the folder scope is not a folder that can sync: give a folder relative to the vault, not empty, without . or .. and not a dot-folder",
                        i + 1
                    ));
                }
            }
        }
        Scope::new(mode, folders)
    }
}

/// Read a profile file; a missing file is an empty profile.
pub fn load(path: &Path) -> Result<Profile, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => parse(&s).map_err(|e| format!("parsing {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Profile::default()),
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    }
}

pub fn parse(text: &str) -> Result<Profile, String> {
    toml::from_str(text).map_err(|e| crate::toml_de_error(text, &e))
}

/// Write a profile file (mode 0600: it holds the password).
pub fn save(path: &Path, p: &Profile) -> std::io::Result<()> {
    let text = toml::to_string_pretty(p).map_err(std::io::Error::other)?;
    crate::edit::write_private(path, &text)
}

#[cfg(test)]
mod tests {
    #[test]
    fn errors_and_debug_never_show_the_password() {
        let e = super::parse("password = \"leaky-secret\"\nurl = [\n").unwrap_err();
        assert!(e.starts_with("line 2") && !e.contains("leaky"), "{e}");
        let p = super::parse("password = \"leaky-secret\"\n").unwrap();
        assert!(!format!("{p:?}").contains("leaky"));
        // A password of the wrong type, or in the wrong key.
        let e = super::parse("url = \"https://couch.example.net\"\npassword = 98765432\n").unwrap_err();
        assert!(e.starts_with("line 2") && !e.contains("98765432"), "{e}");
        let e = super::parse("max_size_mb = \"leaky-secret\"\n").unwrap_err();
        assert!(!e.contains("leaky"), "{e}");
        let p = super::parse("scope_mode = \"leaky-secret\"\n").unwrap();
        assert!(!p.scope().unwrap_err().contains("leaky"));
        let p = super::parse("scope_mode = \"include\"\nscope_folders = [\"../leaky-secret\"]\n").unwrap();
        let e = p.scope().unwrap_err();
        assert!(!e.contains("leaky") && e.contains("entry 1"), "{e}");
    }
}
