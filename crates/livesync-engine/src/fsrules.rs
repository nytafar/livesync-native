//! What the vault's filesystem can hold, probed at vault open (spec
//! "Platform seams", `FsRules`): case folding, Unicode-normalisation folding,
//! forbidden characters and the longest file name. Nothing here assumes a
//! platform; a vault on an exFAT stick under Linux gets the same rules as
//! Android's shared storage because the probe measures them.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;

/// Characters some filesystems refuse in names (FAT, exFAT, NTFS, Android's
/// shared storage). `/` separates components and is never probed.
pub const PROBED_CHARS: [char; 8] = ['"', '*', ':', '<', '>', '?', '\\', '|'];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsRules {
    /// `A.md` and `a.md` name the same file.
    pub case_insensitive: bool,
    /// NFC and NFD spellings name the same file.
    pub normalization_insensitive: bool,
    /// Characters a name may not contain.
    pub forbidden: Vec<char>,
    /// Control characters (U+0001..U+001F, U+007F) are refused.
    pub control_chars_forbidden: bool,
    /// Longest file name, in bytes.
    pub max_name_bytes: usize,
}

impl FsRules {
    /// A POSIX filesystem that takes any name (ext4, btrfs, tmpfs).
    pub fn permissive() -> Self {
        FsRules {
            case_insensitive: false,
            normalization_insensitive: false,
            forbidden: vec![],
            control_chars_forbidden: false,
            max_name_bytes: 255,
        }
    }

    /// FAT-style rules, as on Android's shared storage.
    pub fn fat() -> Self {
        FsRules {
            case_insensitive: true,
            normalization_insensitive: true,
            forbidden: PROBED_CHARS.to_vec(),
            control_chars_forbidden: true,
            max_name_bytes: 255,
        }
    }

    /// Whether two different paths can name one file here, so a case (or
    /// normalisation) collision guard is needed.
    pub fn folds(&self) -> bool {
        self.case_insensitive || self.normalization_insensitive
    }

    /// Measure the rules in a hidden scratch directory under `root`.
    pub fn probe(root: &Path) -> io::Result<FsRules> {
        let dir = root.join(format!(".lsync-probe-{:08x}", rand::random::<u32>()));
        fs::create_dir_all(&dir)?;
        let r = probe_in(&dir);
        let _ = fs::remove_dir_all(&dir);
        r
    }

    /// Why `rel` cannot be created here, checked without touching the disk.
    pub fn unwritable_reason(&self, rel: &str) -> Option<String> {
        let mut bad: Vec<char> = Vec::new();
        for c in rel.chars() {
            let refused =
                self.forbidden.contains(&c) || (self.control_chars_forbidden && ((c as u32) < 0x20 || c == '\u{7f}'));
            if refused && !bad.contains(&c) {
                bad.push(c);
            }
        }
        if !bad.is_empty() {
            let list: Vec<String> = bad.iter().map(|c| format!("{c:?}")).collect();
            return Some(format!("contains {} (not allowed on this filesystem)", list.join(" ")));
        }
        if let Some(seg) = rel.split('/').find(|s| s.len() > self.max_name_bytes) {
            return Some(format!(
                "name {:?}… is {} bytes (this filesystem allows {})",
                seg.chars().take(20).collect::<String>(),
                seg.len(),
                self.max_name_bytes
            ));
        }
        None
    }
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn probe_in(dir: &Path) -> io::Result<FsRules> {
    fs::write(dir.join("case-probe.txt"), b"x")?;
    let case_insensitive = exists(&dir.join("CASE-PROBE.txt"));
    // NFD on disk, looked up as NFC.
    fs::write(dir.join("nfd-e\u{301}.txt"), b"x")?;
    let normalization_insensitive = exists(&dir.join("nfd-\u{e9}.txt"));
    let mut forbidden = Vec::new();
    for c in PROBED_CHARS {
        if fs::write(dir.join(format!("c{c}c.txt")), b"x").is_err() {
            forbidden.push(c);
        }
    }
    let control_chars_forbidden = ['\u{1}', '\u{1f}', '\u{7f}']
        .iter()
        .any(|c| fs::write(dir.join(format!("k{c}k.txt")), b"x").is_err());
    let mut max_name_bytes = 0;
    for n in [255usize, 143, 128, 100, 64] {
        if fs::write(dir.join("n".repeat(n)), b"x").is_ok() {
            max_name_bytes = n;
            break;
        }
    }
    Ok(FsRules {
        case_insensitive,
        normalization_insensitive,
        forbidden,
        control_chars_forbidden,
        max_name_bytes: max_name_bytes.max(64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_on_the_test_filesystem() {
        let d = tempfile::tempdir().unwrap();
        let r = FsRules::probe(d.path()).unwrap();
        // tmpfs/ext4/btrfs under Linux take any name.
        assert!(!r.case_insensitive);
        assert!(r.forbidden.is_empty());
        assert!(r.max_name_bytes >= 255);
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 0, "scratch removed");
    }

    #[test]
    fn reasons() {
        let fat = FsRules::fat();
        assert!(fat.unwritable_reason("a/b:c.md").unwrap().contains("':'"));
        assert!(fat.unwritable_reason("x\u{1}.md").is_some());
        assert!(fat.unwritable_reason("Notes/æøå [x] (y) #z.md").is_none());
        let p = FsRules::permissive();
        assert!(p.unwritable_reason("a:b.md").is_none());
        assert!(p.unwritable_reason(&"x".repeat(300)).is_some());
        assert!(fat.folds() && !p.folds());
    }
}
