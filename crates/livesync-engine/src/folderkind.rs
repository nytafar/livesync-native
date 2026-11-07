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

/// Why the Self-hosted LiveSync plugin counts as enabled in `vault`, if it
/// does. Enabled means a top-level dot-folder (a config folder: `.obsidian/`
/// or any other Obsidian may be told to use) holds the plugin
/// (`plugins/obsidian-livesync/`) and its `community-plugins.json` lists it,
/// which is when Obsidian loads it.
///
/// It fails closed: anything that cannot be looked at (the vault's own
/// listing, a dot-folder, the plugin folder, a plugin list that cannot be
/// read or parsed) counts as enabled. A plain folder never does.
pub fn livesync_plugin_enabled(vault: &Path) -> Option<String> {
    let entries = match std::fs::read_dir(vault) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => return Some(format!("{} cannot be listed ({e})", vault.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        match entry {
            Ok(e) => names.push(e.file_name()),
            Err(e) => return Some(format!("{} cannot be listed ({e})", vault.display())),
        }
    }
    // `.obsidian` first, then the others in a fixed order.
    names.sort_by_key(|n| (n != ".obsidian", n.clone()));
    for name in names {
        if !name.to_string_lossy().starts_with('.') {
            continue;
        }
        if let Some(why) = config_folder_enables(&vault.join(&name)) {
            return Some(why);
        }
    }
    None
}

/// Whether the folder `dir` (a top-level dot-entry) is a config folder that
/// enables the plugin, and why.
fn config_folder_enables(dir: &Path) -> Option<String> {
    let plugin = dir.join("plugins").join(LIVESYNC_PLUGIN_ID);
    match std::fs::metadata(&plugin) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return None,
        // Not there, or `dir` (or its `plugins`) is a file, not a folder.
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => return None,
        Err(e) => return Some(format!("{} cannot be checked ({e})", plugin.display())),
    }
    let list = dir.join("community-plugins.json");
    let text = match std::fs::read_to_string(&list) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
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
}
