//! The PBKDF2 salt in `_local/obsidian_livesync_sync_parameters`
//! (`SyncParamsHandler.ts`, `sync.definition.ts`).
//!
//! The document looks like
//! `{"_id": "_local/obsidian_livesync_sync_parameters", "type": "sync-parameters",
//!   "protocolVersion": 2, "pbkdf2salt": "<base64 of 32 random bytes>"}`.
//!
//! Upstream, when fetching it:
//! - document missing (404): creates it from `DEFAULT_SYNC_PARAMETERS`
//!   (empty salt), writes it, and fetches again;
//! - `pbkdf2salt` empty or absent: generates 32 random bytes, writes the
//!   document back with them, and fetches again;
//! - decodes the salt with `atob` and does not check `type`, `protocolVersion`
//!   or the decoded length. A salt that is not base64 decodes to *zero bytes*
//!   without an error (upstream logs and goes on with an empty PBKDF2 salt).
//!
//! This module only parses; creating or repairing the document is the
//! caller's (remote layer's) decision. Unlike upstream, a non-base64 salt is
//! an error here rather than an empty salt.

use base64::Engine;
use serde_json::Value;

use crate::key::B64;

/// Why the sync parameters give no usable salt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyncParamsError {
    #[error("sync parameters are not a JSON object")]
    NotAnObject,
    /// `pbkdf2salt` is absent or empty: upstream would generate and store one.
    #[error("sync parameters have no PBKDF2 salt")]
    NoSalt,
    /// `pbkdf2salt` is not a base64 string (upstream would use an empty salt).
    #[error("PBKDF2 salt in sync parameters is not base64")]
    InvalidSalt,
}

/// The decoded PBKDF2 salt from the sync-parameters document.
///
/// Decoding follows `atob`: ASCII whitespace is ignored and padding is optional.
pub fn parse_pbkdf2_salt(doc: &Value) -> Result<Vec<u8>, SyncParamsError> {
    let obj = doc.as_object().ok_or(SyncParamsError::NotAnObject)?;
    let salt = match obj.get("pbkdf2salt") {
        None | Some(Value::Null) => return Err(SyncParamsError::NoSalt),
        Some(Value::String(s)) if s.is_empty() => return Err(SyncParamsError::NoSalt),
        Some(Value::String(s)) => s,
        // `false`, `0` and `""` are falsy upstream; anything else non-string
        // goes to `atob` and fails.
        Some(Value::Bool(false)) => return Err(SyncParamsError::NoSalt),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => return Err(SyncParamsError::NoSalt),
        Some(_) => return Err(SyncParamsError::InvalidSalt),
    };
    let compact: String = salt
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' '))
        .collect();
    B64.decode(compact).map_err(|_| SyncParamsError::InvalidSalt)
}
