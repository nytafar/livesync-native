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
    fn normalisation() {
        assert_eq!(normalize_path("a\\b//c///d.md"), "a/b/c/d.md");
    }
}
