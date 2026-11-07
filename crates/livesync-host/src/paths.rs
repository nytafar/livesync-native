//! Comparing folders: whether two vault or state folders overlap.

use std::path::{Component, Path, PathBuf};

/// The path as compared for overlap: canonical when it exists (resolves
/// `/sdcard` and other links), otherwise lexically normalised; then
/// lower-cased, because Android shared storage and the default macOS volume
/// fold case (two spellings of one folder are one folder there, and refusing
/// them elsewhere is harmless).
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

/// Lexical normalisation: drops `.` and resolves `..` without the disk.
pub fn normalise(p: &Path) -> PathBuf {
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

/// Whether two folders are the same, or one lies inside the other (vault
/// folders may not overlap).
pub fn paths_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (comparable(a), comparable(b));
    a.starts_with(&b) || b.starts_with(&a)
}

/// `inner` relative to `outer` when `inner` lies strictly inside it, as a
/// vault-relative folder (`/`-separated, lower-cased like [`comparable`]).
pub fn nested_under(outer: &str, inner: &str) -> Option<String> {
    let (o, i) = (comparable(outer), comparable(inner));
    if o == i {
        return None;
    }
    let rel = i.strip_prefix(&o).ok()?;
    let segs: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!segs.is_empty()).then(|| segs.join("/"))
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

    #[test]
    fn nested_gives_the_relative_folder() {
        assert_eq!(
            nested_under("/x/notes", "/x/notes/Work/Sub").as_deref(),
            Some("work/sub")
        );
        assert_eq!(nested_under("/x/notes", "/x/notes"), None);
        assert_eq!(nested_under("/x/notes", "/x/other"), None);
        assert_eq!(nested_under("/x/notes/a", "/x/notes"), None);
    }
}
