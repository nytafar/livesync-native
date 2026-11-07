//! The plugin guard (spec D16). In an Obsidian vault (a folder with
//! `.obsidian/`, [`crate::vaultfs::FolderKind::Obsidian`]) where the
//! Self-hosted LiveSync plugin is enabled, the plugin and lsync would both
//! write the folder and the database. Two-way sync refuses such a vault
//! unless told it is a deliberate handover.

use std::path::Path;

/// The plugin's ID in `.obsidian/community-plugins.json` and its folder
/// under `.obsidian/plugins/`.
pub const LIVESYNC_PLUGIN_ID: &str = "obsidian-livesync";

/// Why the Self-hosted LiveSync plugin counts as enabled in `vault`, if it
/// does. Enabled means the plugin is installed (`.obsidian/plugins/obsidian-livesync/`)
/// and `.obsidian/community-plugins.json` lists it, which is when Obsidian
/// loads it. A list that exists but cannot be read or parsed counts as
/// enabled (fail closed). A plain folder never does.
pub fn livesync_plugin_enabled(vault: &Path) -> Option<String> {
    let obsidian = vault.join(".obsidian");
    if !obsidian.join("plugins").join(LIVESYNC_PLUGIN_ID).is_dir() {
        return None;
    }
    let list = obsidian.join("community-plugins.json");
    let text = match std::fs::read_to_string(&list) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(format!("{} exists but cannot be read ({e})", list.display())),
    };
    match serde_json::from_str::<Vec<String>>(&text) {
        Ok(ids) if ids.iter().any(|i| i == LIVESYNC_PLUGIN_ID) => {
            Some(format!("{} lists {LIVESYNC_PLUGIN_ID}", list.display()))
        }
        Ok(_) => None,
        Err(e) => Some(format!("{} cannot be parsed ({e})", list.display())),
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
}
