//! The plugin guard (spec D16). In an Obsidian vault where the Self-hosted
//! LiveSync plugin is enabled, the plugin and lsync would both write the
//! folder and the database. Two-way sync refuses such a vault unless told it
//! is a deliberate handover.
//!
//! Obsidian keeps its settings in `.obsidian/` by default, but each device
//! may use another config folder (Settings, Files and links, "Override
//! config folder", e.g. `.obsidian-mobile`), and a synced vault often holds
//! several. So every top-level dot-folder that holds the plugin counts.

use std::io::ErrorKind;
use std::path::Path;

/// The plugin's ID in `community-plugins.json` and its folder under
/// `plugins/` of a config folder.
pub const LIVESYNC_PLUGIN_ID: &str = "obsidian-livesync";

/// What the plugin guard found in a vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginCheck {
    /// No config folder enables the plugin.
    Clear,
    /// A config folder enables it (or holds the plugin with a plugin list
    /// that cannot be read or parsed), and why.
    Enabled(String),
    /// `folder` (the vault, or a top-level dot-folder) could not be looked
    /// into. It may enable the plugin, so this fails closed, but it may have
    /// nothing to do with Obsidian (a root-owned `.snapshots`).
    Unchecked { folder: String, why: String },
}

/// Whether the Self-hosted LiveSync plugin counts as enabled in `vault`.
/// Enabled means a top-level dot-folder (a config folder: `.obsidian/`
/// or any other Obsidian may be told to use) holds the plugin
/// (`plugins/obsidian-livesync/`) and its `community-plugins.json` lists it,
/// which is when Obsidian loads it.
///
/// It fails closed: a folder that cannot be looked into (the vault's own
/// listing, a dot-folder) is [`PluginCheck::Unchecked`], and a plugin list
/// that cannot be read or parsed counts as enabled. A plain folder is clear.
pub fn check_livesync_plugin(vault: &Path) -> PluginCheck {
    let unlisted = |e: std::io::Error| PluginCheck::Unchecked {
        folder: vault.display().to_string(),
        why: format!("it cannot be listed: {e}"),
    };
    let entries = match std::fs::read_dir(vault) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return PluginCheck::Clear,
        Err(e) => return unlisted(e),
    };
    let mut names = Vec::new();
    for entry in entries {
        match entry {
            Ok(e) => names.push(e.file_name()),
            Err(e) => return unlisted(e),
        }
    }
    // `.obsidian` first, then the others in a fixed order.
    names.sort_by_key(|n| (n != ".obsidian", n.clone()));
    for name in names {
        if !name.to_string_lossy().starts_with('.') {
            continue;
        }
        match config_folder_enables(&vault.join(&name)) {
            PluginCheck::Clear => {}
            found => return found,
        }
    }
    PluginCheck::Clear
}

/// Why the plugin counts as enabled in `vault`, or could not be ruled out,
/// if so ([`check_livesync_plugin`] as one line).
pub fn livesync_plugin_enabled(vault: &Path) -> Option<String> {
    match check_livesync_plugin(vault) {
        PluginCheck::Clear => None,
        PluginCheck::Enabled(why) => Some(why),
        PluginCheck::Unchecked { folder, why } => Some(format!("{folder} cannot be checked ({why})")),
    }
}

/// Whether the folder `dir` (a top-level dot-entry) is a config folder that
/// enables the plugin, and why.
fn config_folder_enables(dir: &Path) -> PluginCheck {
    let plugin = dir.join("plugins").join(LIVESYNC_PLUGIN_ID);
    match std::fs::metadata(&plugin) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return PluginCheck::Clear,
        // Not there, or `dir` (or its `plugins`) is a file, not a folder.
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => return PluginCheck::Clear,
        Err(e) => {
            return PluginCheck::Unchecked {
                folder: dir.display().to_string(),
                why: format!("{}: {e}", plugin.display()),
            }
        }
    }
    let list = dir.join("community-plugins.json");
    let text = match std::fs::read_to_string(&list) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return PluginCheck::Clear,
        Err(e) => return PluginCheck::Enabled(format!("{} exists but cannot be read ({e})", list.display())),
    };
    match serde_json::from_str::<Vec<String>>(&text) {
        Ok(ids) if ids.iter().any(|i| i == LIVESYNC_PLUGIN_ID) => {
            PluginCheck::Enabled(format!("{} lists {LIVESYNC_PLUGIN_ID}", list.display()))
        }
        Ok(_) => PluginCheck::Clear,
        Err(e) => PluginCheck::Enabled(format!("{} cannot be parsed ({e})", list.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vaultfs::FolderKind;

    #[test]
    fn only_an_installed_and_listed_plugin_counts() {
        let d = tempfile::tempdir().unwrap();
        let v = d.path();
        assert_eq!(FolderKind::of(v), FolderKind::Plain);
        assert!(livesync_plugin_enabled(v).is_none(), "a plain folder");
        assert!(livesync_plugin_enabled(&v.join("missing")).is_none(), "no folder yet");
        std::fs::create_dir_all(v.join(".obsidian/plugins/obsidian-livesync")).unwrap();
        assert_eq!(FolderKind::of(v), FolderKind::Obsidian);
        assert!(livesync_plugin_enabled(v).is_none(), "installed, but no plugin list");
        let list = v.join(".obsidian/community-plugins.json");
        std::fs::write(&list, r#"["dataview"]"#).unwrap();
        assert!(livesync_plugin_enabled(v).is_none(), "installed but disabled");
        std::fs::write(&list, r#"["dataview", "obsidian-livesync"]"#).unwrap();
        assert!(livesync_plugin_enabled(v).unwrap().contains("obsidian-livesync"));
        std::fs::write(&list, "not json").unwrap();
        assert!(livesync_plugin_enabled(v).is_some(), "an unreadable list fails closed");
        std::fs::write(&list, r#"["obsidian-livesync"]"#).unwrap();
        std::fs::remove_dir_all(v.join(".obsidian/plugins")).unwrap();
        assert!(livesync_plugin_enabled(v).is_none(), "listed but not installed");
    }

    #[test]
    fn any_config_folder_counts() {
        let d = tempfile::tempdir().unwrap();
        let v = d.path();
        // `.obsidian` with the plugin off, and other dot-entries that are no
        // config folder at all (a file, a folder without plugins).
        std::fs::create_dir_all(v.join(".obsidian/plugins/obsidian-livesync")).unwrap();
        std::fs::write(v.join(".obsidian/community-plugins.json"), r#"["dataview"]"#).unwrap();
        std::fs::write(v.join(".gitignore"), "x\n").unwrap();
        std::fs::create_dir_all(v.join(".git/objects")).unwrap();
        std::fs::create_dir_all(v.join("notes/.hidden/plugins/obsidian-livesync")).unwrap();
        assert!(livesync_plugin_enabled(v).is_none());
        // A device that uses `.obsidian-mobile` as its config folder.
        let m = v.join(".obsidian-mobile");
        std::fs::create_dir_all(m.join("plugins/obsidian-livesync")).unwrap();
        std::fs::write(m.join("community-plugins.json"), r#"["obsidian-livesync"]"#).unwrap();
        let why = livesync_plugin_enabled(v).unwrap();
        assert!(
            why.contains(".obsidian-mobile") && why.contains("lists obsidian-livesync"),
            "{why}"
        );
        // Also with no `.obsidian/` at all.
        std::fs::remove_dir_all(v.join(".obsidian")).unwrap();
        assert!(livesync_plugin_enabled(v).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_looked_into_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let v = d.path();
        let c = v.join(".obsidian");
        std::fs::create_dir_all(c.join("plugins/obsidian-livesync")).unwrap();
        std::fs::write(c.join("community-plugins.json"), r#"["obsidian-livesync"]"#).unwrap();
        let mode = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        mode(&c, 0o000);
        // Root reads anything; the check is moot there.
        let blocked = std::fs::metadata(c.join("plugins")).is_err();
        let r = livesync_plugin_enabled(v);
        mode(&c, 0o755);
        if blocked {
            let why = r.expect("an unreadable config folder fails closed");
            assert!(why.contains("cannot be checked"), "{why}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_dot_folder_is_named_as_unchecked() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let v = d.path();
        let s = v.join(".snapshots");
        std::fs::create_dir_all(s.join("1")).unwrap();
        let mode = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        mode(&s, 0o000);
        // Root reads anything; the check is moot there.
        let blocked = std::fs::metadata(s.join("plugins")).is_err_and(|e| e.kind() == ErrorKind::PermissionDenied);
        let r = check_livesync_plugin(v);
        mode(&s, 0o755);
        if blocked {
            match r {
                PluginCheck::Unchecked { folder, .. } => assert_eq!(folder, s.display().to_string()),
                other => panic!("an unreadable dot-folder fails closed, named: {other:?}"),
            }
        }
    }
}
