//! LiveSync sync engine: two-way sync between a vault folder and a LiveSync
//! CouchDB database.
//!
//! The engine talks to CouchDB directly: it follows the filtered `_changes`
//! feed, fetches chunks on demand and materialises files into a vault folder,
//! and pushes local changes back (chunks first, then metadata against the
//! revision the file was synced at). Locally it keeps only a SQLite sync
//! index (spec D2). `Engine::pull_once` and `Engine::daemon_pull_only` never
//! write to the remote.

pub mod engine;
pub mod folders;
pub mod fsrules;
pub mod index;
pub mod logbridge;
pub mod platform;
pub mod push;
mod scopesync;
pub mod vaultfs;
pub mod watcher;

pub use engine::{AccessCheck, Engine, EngineConfig, EngineError, GateReport, PullReport, Status, SyncReport};
pub use folders::{list_remote_folders, local_top_folders, FolderEntry, FolderListing};
pub use index::{FileRow, Index, LocalIssue, State};
pub use push::{Confirmation, PushReport};
