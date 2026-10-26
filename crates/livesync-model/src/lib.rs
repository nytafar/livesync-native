//! Pure LiveSync data model: document types, path -> ID derivation, chunk IDs,
//! the read-side codec and compatibility-gate evaluation. No I/O.

pub mod chunk;
pub mod codec;
pub mod doc;
pub mod gate;
pub mod path;

pub use doc::{ChunkDoc, FileDoc, FileKind};
pub use gate::Refusal;
