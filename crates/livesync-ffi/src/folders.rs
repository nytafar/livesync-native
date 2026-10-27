//! The remote's top-level folders for the folder-scope picker, as the app
//! shows them (spec "Picking folders from the server (M2)").

use livesync_engine::{FolderEntry, FolderListing};

/// One top-level folder.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FolderInfo {
    /// As the remote's documents spell it (as on disk when local only).
    pub name: String,
    /// Live files under it on the remote.
    pub files: i64,
    /// Their total size in bytes.
    pub bytes: i64,
    /// The remote holds live files under it.
    pub remote: bool,
    /// The vault folder has a folder of this name (any case).
    pub local: bool,
}

/// The remote's top-level folders, local-only ones marked, plus the files at
/// the vault root.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FolderList {
    /// Sorted case-insensitively.
    pub folders: Vec<FolderInfo>,
    pub root_files: i64,
    pub root_bytes: i64,
    /// Live files on the remote, all folders and the root.
    pub files: i64,
    pub docs_read: i64,
    pub bytes_fetched: i64,
    pub elapsed_ms: i64,
}

impl From<FolderListing> for FolderList {
    fn from(l: FolderListing) -> Self {
        FolderList {
            folders: l.folders.into_iter().map(FolderInfo::from).collect(),
            root_files: l.root_files as i64,
            root_bytes: l.root_bytes as i64,
            files: l.files as i64,
            docs_read: l.docs_read as i64,
            bytes_fetched: l.bytes_fetched as i64,
            elapsed_ms: l.elapsed_ms as i64,
        }
    }
}

impl From<FolderEntry> for FolderInfo {
    fn from(f: FolderEntry) -> Self {
        FolderInfo {
            name: f.name,
            files: f.files as i64,
            bytes: f.bytes as i64,
            remote: f.remote,
            local: f.local,
        }
    }
}
