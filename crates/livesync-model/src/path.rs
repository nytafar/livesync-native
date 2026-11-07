//! Vault paths: document ID derivation and the "never synced" rules.
//!
//! Upstream: `livesync-commonlib/src/string_and_binary/path.ts` (`path2id_base`,
//! `shouldBeIgnored`, `isPlainText`) and `common/models/redflag.const.ts`.

/// Document-ID prefixes for non-file documents (hidden files, customisation sync, obsolete).
pub const INTERNAL_PREFIXES: [&str; 3] = ["i:", "ix:", "ps:"];

/// Host normalisation used by the official CLI before deriving an ID:
/// backslashes become slashes and repeated slashes collapse.
pub fn normalize_path(path: &str) -> String {
    let replaced = path.replace('\\', "/");
    let mut out = String::with_capacity(replaced.len());
    let mut prev_slash = false;
    for c in replaced.chars() {
        if c == '/' {
            if prev_slash {
                continue;
            }
            prev_slash = true;
        } else {
            prev_slash = false;
        }
        out.push(c);
    }
    out
}

/// `path2id_base` without obfuscation: optional lower-casing, then the `/_` rule
/// (CouchDB rejects IDs that start with an underscore).
pub fn path_to_id(path: &str, case_insensitive: bool) -> String {
    let body = if case_insensitive {
        js_to_lower_case(path)
    } else {
        path.to_string()
    };
    if body.starts_with('_') {
        format!("/{body}")
    } else {
        body
    }
}

/// JavaScript `String.prototype.toLowerCase` (locale-independent Unicode full
/// lower-casing, including the final-sigma rule). Rust's `str::to_lowercase`
/// implements the same Unicode algorithm; golden vectors pin the equivalence.
pub fn js_to_lower_case(s: &str) -> String {
    s.to_lowercase()
}

/// True when the document ID belongs to hidden-file sync, customisation sync or
/// the obsolete `ps:` namespace.
pub fn is_internal_id(id: &str) -> bool {
    INTERNAL_PREFIXES.iter().any(|p| id.starts_with(p))
}

/// Why a path is never materialised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSkip {
    /// `i:`, `ix:` or `ps:` path.
    Internal,
    /// Some segment starts with a dot (`.obsidian/`, `.trash/`, dotfiles).
    DotPath,
    /// LiveSync's flag files and log files.
    FlagFile,
    /// Empty, absolute, or otherwise unsafe to join under the vault root.
    Unsafe,
}

impl PathSkip {
    pub fn as_str(self) -> &'static str {
        match self {
            PathSkip::Internal => "internal",
            PathSkip::DotPath => "dot-path",
            PathSkip::FlagFile => "flag-file",
            PathSkip::Unsafe => "unsafe-path",
        }
    }
}

/// Classify a vault path (as stored in the metadata `path` field).
/// `None` means the path may be materialised, subject to the regex and size rules.
pub fn classify_path(path: &str) -> Option<PathSkip> {
    if is_internal_id(path) {
        return Some(PathSkip::Internal);
    }
    if path.is_empty() || path.starts_with('/') || path.contains('\0') {
        return Some(PathSkip::Unsafe);
    }
    for seg in path.split('/') {
        if seg.is_empty() {
            return Some(PathSkip::Unsafe);
        }
        if seg.starts_with('.') {
            // Covers "." and ".." as well as dotfiles and dot-directories.
            return Some(PathSkip::DotPath);
        }
    }
    if is_flag_file(path) {
        return Some(PathSkip::FlagFile);
    }
    None
}

/// Dot-paths that may sync after all (`sync_dot_paths`): each entry names
/// a dot-folder or dot-file, vault-relative, compared case-insensitively.
/// Everything below an allowed dot-folder follows the usual rules, so a dot
/// segment further down needs an entry of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DotAllow {
    /// Normalised, lower-cased entries.
    entries: Vec<String>,
}

/// Dot names that can never be allowlisted: Obsidian's settings (settings
/// sync is its own feature), the vault trash, git's internals, and lsync's
/// own temp and lock files.
fn never_allowed(seg: &str) -> bool {
    let s = js_to_lower_case(seg);
    s == ".obsidian" || s == ".trash" || s == ".git" || s.starts_with(".lsync")
}

impl DotAllow {
    pub fn none() -> Self {
        DotAllow::default()
    }

    /// Check and normalise the configured entries.
    pub fn new(entries: &[String]) -> Result<DotAllow, String> {
        let mut out: Vec<String> = Vec::new();
        for e in entries {
            let segs: Vec<&str> = e.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
            let Some(last) = segs.last() else {
                return Err(format!("sync_dot_paths: empty entry {e:?}"));
            };
            for s in &segs {
                if *s == "." || *s == ".." {
                    return Err(format!("sync_dot_paths: {e:?} contains {s:?}"));
                }
                if never_allowed(s) {
                    return Err(format!("sync_dot_paths: {e:?}: {s} can never sync as ordinary files"));
                }
            }
            if !last.starts_with('.') {
                return Err(format!(
                    "sync_dot_paths: {e:?} is not a dot-path (name the dot-folder or dot-file itself)"
                ));
            }
            out.push(js_to_lower_case(&segs.join("/")));
        }
        out.sort();
        out.dedup();
        Ok(DotAllow { entries: out })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Whether every dot segment of `path` is an allowed entry.
    pub fn covers(&self, path: &str) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let lower = js_to_lower_case(path);
        let mut end = 0usize;
        for seg in lower.split('/') {
            end += seg.len();
            if seg.starts_with('.') {
                if seg == "." || seg == ".." {
                    return false;
                }
                let prefix = &lower[..end];
                if !self.entries.iter().any(|e| e == prefix) {
                    return false;
                }
            }
            end += 1; // the '/'
        }
        true
    }
}

/// [`classify_path`], with the dot-paths `allow` names let through.
pub fn classify_path_allowing(path: &str, allow: &DotAllow) -> Option<PathSkip> {
    match classify_path(path) {
        Some(PathSkip::DotPath) if allow.covers(path) => {
            // The other rules still apply.
            if path.split('/').any(|s| s.is_empty()) {
                Some(PathSkip::Unsafe)
            } else if is_flag_file(path) {
                Some(PathSkip::FlagFile)
            } else {
                None
            }
        }
        other => other,
    }
}

/// Upstream ignores exactly `redflag.md`, `redflag2.md`, `redflag3.md`,
/// `flag_rebuild.md`, `flag_fetch.md` at the vault root and anything starting
/// with `livesync_log_` / `LIVESYNC_LOG_`. We skip the broader root-level
/// patterns `redflag*.md` and `flag_*.md` as well.
pub fn is_flag_file(path: &str) -> bool {
    if path.starts_with("livesync_log_") || path.starts_with("LIVESYNC_LOG_") {
        return true;
    }
    if path.contains('/') {
        return false;
    }
    (path.starts_with("redflag") || path.starts_with("flag_")) && path.ends_with(".md")
}

/// Upstream's exact `shouldBeIgnored` (used only by tests against golden vectors).
pub fn upstream_should_be_ignored(path: &str) -> bool {
    matches!(
        path,
        "redflag.md" | "redflag2.md" | "redflag3.md" | "flag_rebuild.md" | "flag_fetch.md"
    ) || path.starts_with("livesync_log_")
        || path.starts_with("LIVESYNC_LOG_")
}

/// Extensions LiveSync stores as text (`type: "plain"`).
pub fn is_plain_text(path: &str) -> bool {
    const EXT: [&str; 9] = [".md", ".txt", ".svg", ".html", ".csv", ".css", ".js", ".xml", ".canvas"];
    EXT.iter().any(|e| path.ends_with(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_paths_and_flags() {
        assert_eq!(classify_path(".obsidian/app.json"), Some(PathSkip::DotPath));
        assert_eq!(classify_path("a/.trash/x.md"), Some(PathSkip::DotPath));
        assert_eq!(classify_path("a/../x.md"), Some(PathSkip::DotPath));
        assert_eq!(classify_path("i:.obsidian/x"), Some(PathSkip::Internal));
        assert_eq!(classify_path("/abs.md"), Some(PathSkip::Unsafe));
        assert_eq!(classify_path("a//b.md"), Some(PathSkip::Unsafe));
        assert_eq!(classify_path("redflag.md"), Some(PathSkip::FlagFile));
        assert_eq!(classify_path("redflag9.md"), Some(PathSkip::FlagFile));
        assert_eq!(classify_path("flag_anything.md"), Some(PathSkip::FlagFile));
        assert_eq!(classify_path("livesync_log_1.md"), Some(PathSkip::FlagFile));
        assert_eq!(classify_path("sub/redflag.md"), None);
        assert_eq!(classify_path("notes/a.md"), None);
        assert_eq!(classify_path("x.v1.md"), None);
    }

    #[test]
    fn dot_allowlist() {
        let a = DotAllow::new(&[".notes".into(), "Work/.Drafts/".into()]).unwrap();
        assert_eq!(classify_path_allowing(".notes/a.md", &a), None);
        assert_eq!(classify_path_allowing(".NOTES/sub/a.md", &a), None);
        assert_eq!(classify_path_allowing("work/.drafts/x.md", &a), None);
        assert_eq!(
            classify_path_allowing(".notes/.hidden/a.md", &a),
            Some(PathSkip::DotPath)
        );
        assert_eq!(classify_path_allowing(".other/a.md", &a), Some(PathSkip::DotPath));
        assert_eq!(
            classify_path_allowing(".notes/.lsync-1.tmp", &a),
            Some(PathSkip::DotPath)
        );
        assert_eq!(classify_path_allowing(".notesx/a.md", &a), Some(PathSkip::DotPath));
        assert_eq!(
            classify_path_allowing(".obsidian/app.json", &a),
            Some(PathSkip::DotPath)
        );
        for bad in [
            ".obsidian",
            ".trash",
            ".git",
            "a/.obsidian",
            ".lsync.lock",
            "notes",
            "..",
            "a/../.x",
            "",
        ] {
            assert!(DotAllow::new(&[bad.into()]).is_err(), "{bad:?} must be refused");
        }
        assert_eq!(
            classify_path_allowing(".notes/a.md", &DotAllow::none()),
            Some(PathSkip::DotPath)
        );
    }

    #[test]
    fn normalisation() {
        assert_eq!(normalize_path("a\\b//c///d.md"), "a/b/c/d.md");
    }
}
