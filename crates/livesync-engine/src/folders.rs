//! The remote's top-level folders, for picking a folder scope (spec "Picking
//! folders from the server (M2)").
//!
//! Reads the file metadata documents only: `_all_docs?include_docs=true` over
//! the ID ranges around the chunks (`h:`) and the internal namespaces (`i:`,
//! `ix:`, `ps:`), so no chunk is ever read. Read-only: no document writes, no
//! milestone registration, no gate. Raw (code point) collation, as CouchDB
//! uses for `_all_docs`, makes the ranges exact.
//!
//! A file counts when its document is live and its path is one the engine
//! would sync at all ([`classify_path`]: no dot-path anywhere in it, no flag
//! file, nothing unsafe). The size limit and `syncIgnoreRegEx` are per-profile
//! settings and are not applied. Folder names keep the case of the documents'
//! `path`; spellings that differ only in case are one folder (scopes match
//! case-insensitively), named after the spelling with the most files.

use crate::engine::EngineError;
use livesync_model::doc::{classify_doc, Classified};
use livesync_model::gate::Refusal;
use livesync_model::path::{classify_path, is_internal_id, js_to_lower_case};
use livesync_remote::Couch;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

type Result<T> = std::result::Result<T, EngineError>;

/// Documents per `_all_docs` page.
const PAGE: usize = 2000;

/// One top-level folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FolderEntry {
    /// The folder's name as the documents spell it (or as on disk when it is
    /// local only).
    pub name: String,
    /// Live files under it on the remote.
    pub files: u64,
    /// Their total size in bytes (the documents' `size`).
    pub bytes: u64,
    /// Whether the remote holds any live file under it.
    pub remote: bool,
    /// Whether the vault folder has a folder of this name (any case). `false`
    /// when the local side was not looked at.
    pub local: bool,
}

/// The remote's top-level folders, plus the files at the vault root.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FolderListing {
    /// Sorted case-insensitively by name.
    pub folders: Vec<FolderEntry>,
    pub root_files: u64,
    pub root_bytes: u64,
    /// Live file documents counted (all folders and the root).
    pub files: u64,
    /// Documents read (file metadata and the few other non-chunk documents).
    pub docs_read: u64,
    /// Response bytes fetched for the listing.
    pub bytes_fetched: u64,
    pub elapsed_ms: u64,
}

/// `_all_docs` ranges (inclusive, raw collation) of every document that is
/// not a chunk (`h:`) and not internal (`i:`, `ix:`, `ps:`). `None` is open.
/// Each excluded prefix `p:` is the half-open range `[p:, p;)`; `";"` follows
/// `":"`, so an inclusive end at `"p:"` and a start at `"p;"` leave out
/// exactly the IDs starting with `p:` (plus `"p:"` itself, which nothing uses).
fn ranges() -> Vec<(Option<&'static str>, Option<&'static str>)> {
    vec![
        (None, Some("h:")),
        (Some("h;"), Some("i:")),
        (Some("i;"), Some("ix:")),
        (Some("ix;"), Some("ps:")),
        (Some("ps;"), None),
    ]
}

#[derive(Default)]
struct Tally {
    /// Spelling → (files, bytes).
    spellings: BTreeMap<String, (u64, u64)>,
}

/// List the remote's top-level folders (see the module docs). Refuses a
/// database whose paths are encrypted or obfuscated (E2EE, M4).
pub async fn list_remote_folders(couch: &Couch) -> Result<FolderListing> {
    let t0 = Instant::now();
    let rx0 = couch.bytes_received();
    let mut out = FolderListing::default();
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    for (start, end) in ranges() {
        let mut from = start.map(str::to_string);
        let mut skip_first = false;
        loop {
            let want = PAGE + usize::from(skip_first);
            let rows = couch.all_docs_page(from.as_deref(), end, want, true).await?;
            let n = rows.len();
            let mut last = None;
            for (i, r) in rows.into_iter().enumerate() {
                if skip_first && i == 0 {
                    continue;
                }
                let Some(id) = r.key else { continue };
                last = Some(id.clone());
                if id.starts_with("h:") || is_internal_id(&id) {
                    continue;
                }
                let Some(doc) = r.doc.filter(|d| !d.is_null()) else {
                    continue;
                };
                out.docs_read += 1;
                let f = match classify_doc(&doc) {
                    Classified::File(f) => f,
                    Classified::Encrypted { id } => {
                        return Err(EngineError::Refused(Refusal::EncryptedContent(format!(
                            "document {id}: its path is encrypted or obfuscated, so the folders cannot be listed; type them instead"
                        ))))
                    }
                    _ => continue,
                };
                if f.deleted || classify_path(&f.path).is_some() {
                    continue;
                }
                out.files += 1;
                match f.path.split_once('/') {
                    None => {
                        out.root_files += 1;
                        out.root_bytes += f.size;
                    }
                    Some((top, _)) => {
                        let t = tallies.entry(js_to_lower_case(top)).or_default();
                        let s = t.spellings.entry(top.to_string()).or_default();
                        s.0 += 1;
                        s.1 += f.size;
                    }
                }
            }
            if n < want {
                break;
            }
            match last {
                Some(l) => {
                    from = Some(l);
                    skip_first = true;
                }
                None => break,
            }
        }
    }
    out.folders = tallies
        .into_values()
        .map(|t| {
            let (files, bytes) = t.spellings.values().fold((0, 0), |(f, b), (f2, b2)| (f + f2, b + b2));
            // The most common spelling; on a tie the first in code point order.
            let name = t
                .spellings
                .iter()
                .max_by(|a, b| a.1 .0.cmp(&b.1 .0).then_with(|| b.0.cmp(a.0)))
                .map(|(s, _)| s.clone())
                .unwrap_or_default();
            FolderEntry {
                name,
                files,
                bytes,
                remote: true,
                local: false,
            }
        })
        .collect();
    sort(&mut out.folders);
    out.bytes_fetched = couch.bytes_received() - rx0;
    out.elapsed_ms = t0.elapsed().as_millis() as u64;
    Ok(out)
}

fn sort(folders: &mut [FolderEntry]) {
    folders.sort_by(|a, b| {
        js_to_lower_case(&a.name)
            .cmp(&js_to_lower_case(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// The vault folder's top-level folders that could sync: directories (not
/// links to them) whose names are not dot-names. Empty when the folder does
/// not exist or cannot be read.
pub fn local_top_folders(vault: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(vault) else {
        return vec![];
    };
    let mut out: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| classify_path(n).is_none())
        .collect();
    out.sort_by_key(|n| js_to_lower_case(n));
    out
}

impl FolderListing {
    /// Mark the remote folders that also exist locally (any case) and add the
    /// local-only ones, with no files on the remote.
    pub fn merge_local(&mut self, local: &[String]) {
        for name in local {
            let key = js_to_lower_case(name);
            match self.folders.iter_mut().find(|f| js_to_lower_case(&f.name) == key) {
                Some(f) => f.local = true,
                None => self.folders.push(FolderEntry {
                    name: name.clone(),
                    files: 0,
                    bytes: 0,
                    remote: false,
                    local: true,
                }),
            }
        }
        sort(&mut self.folders);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ranges_leave_out_exactly_chunks_and_internal_ids() {
        let inside = |id: &str| {
            ranges()
                .iter()
                .any(|(a, b)| a.is_none_or(|a| id >= a) && b.is_none_or(|b| id <= b))
        };
        for id in ["h:abc", "h:+enc", "i:.obsidian/app.json", "ix:x/y", "ps:z"] {
            assert!(!inside(id), "{id} is read");
        }
        for id in [
            "/_templates/a.md",
            "Archive/a.md",
            "_design/x",
            "h.md",
            "h1/a.md",
            "h_x/a.md",
            "hobbies/a.md",
            "i.md",
            "inputs/a.md",
            "iy/a.md",
            "ix.md",
            "ixx/a.md",
            "obsydian_livesync_version",
            "p/a.md",
            "ps.md",
            "psy/a.md",
            "zeta/a.md",
            "Ølfolder/a.md",
        ] {
            assert!(inside(id), "{id} is missed");
        }
    }

    #[test]
    fn local_only_folders_are_merged_case_insensitively() {
        let mut l = FolderListing {
            folders: vec![FolderEntry {
                name: "Notes".into(),
                files: 2,
                bytes: 10,
                remote: true,
                local: false,
            }],
            ..Default::default()
        };
        l.merge_local(&["notes".into(), "Drafts".into()]);
        let got: Vec<(&str, bool, bool)> = l.folders.iter().map(|f| (f.name.as_str(), f.remote, f.local)).collect();
        assert_eq!(got, vec![("Drafts", false, true), ("Notes", true, true)]);
    }
}
