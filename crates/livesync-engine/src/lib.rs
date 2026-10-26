//! LiveSync sync engine (M1: read-only pull).
//!
//! The engine talks to CouchDB directly, follows the filtered `_changes`
//! feed, fetches chunks on demand and materialises files into a vault folder.
//! Locally it keeps only a SQLite sync index (spec D2). It never writes to the
//! remote.

pub mod engine;
pub mod index;
pub mod vaultfs;

pub use engine::{Engine, EngineConfig, EngineError, GateReport, PullReport, Status};
pub use index::{FileRow, Index, State};
