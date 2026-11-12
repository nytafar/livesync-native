//! The unencrypted-content policy of plugin 1.0.49 (`encryption.policy.ts`,
//! `assertUnencryptedContentPolicy`), for the V2 algorithm.
//!
//! With E2EE on, a remote document read through the transform may not carry
//! plaintext content (a plaintext `eden` key or a readable `data`) unless the
//! user accepted unencrypted content. Tombstones, `_local/` documents, the
//! control documents with their expected `type`, and deleted metadata-only
//! notes are exempt.

use serde_json::{Map, Value};

use crate::error::{ContentKind, CryptoError, Unsupported};
use crate::ids;

/// Control documents exempt from the policy when their `type` matches.
const CONTROL_DOCUMENTS: &[(&str, &str)] = &[
    (ids::SYNC_PARAMETERS_ID, "sync-parameters"),
    ("_local/obsydian_livesync_milestone", "milestoneinfo"),
    ("_local/obsydian_livesync_nodeinfo", "nodeinfo"),
    (ids::SYNCINFO_ID, "syncinfo"),
    ("obsydian_livesync_version", "versioninfo"),
];

/// Entry types whose bodies live in chunks: deleted ones carry no content.
const METADATA_ONLY_TYPES: &[&str] = &["newnote", "plain", "internalfile"];

/// The plaintext content kinds upstream finds in `doc` (in its order).
pub fn unencrypted_content_kinds(doc: &Value) -> Vec<ContentKind> {
    match doc {
        Value::Object(m) => kinds(m),
        _ => vec![],
    }
}

fn kinds(doc: &Map<String, Value>) -> Vec<ContentKind> {
    let id = doc.get("_id").and_then(Value::as_str).unwrap_or("");
    let ty = doc.get("type").and_then(Value::as_str);
    if doc.get("_deleted") == Some(&Value::Bool(true)) || id.starts_with("_local/") {
        return vec![];
    }
    let is_chunk_id = id.starts_with(ids::CHUNK_PREFIX);
    let is_control = CONTROL_DOCUMENTS.iter().any(|(cid, cty)| *cid == id && ty == Some(cty));
    if !is_chunk_id && is_control {
        return vec![];
    }
    let is_chunk_body = is_chunk_id || matches!(ty, Some("leaf" | "chunkpack"));
    if !is_chunk_body
        && ty.is_some_and(|t| METADATA_ONLY_TYPES.contains(&t))
        && doc.get("deleted") == Some(&Value::Bool(true))
    {
        return vec![];
    }
    let mut kinds = vec![];
    if has_plaintext_eden(doc) {
        kinds.push(ContentKind::Eden);
    }
    // `record.data !== undefined`: a JSON `null` counts as readable.
    if doc.get("data").is_some() {
        kinds.push(if is_chunk_body {
            ContentKind::Chunk
        } else {
            ContentKind::InlineNote
        });
    }
    kinds
}

/// Any `eden` key other than the two encrypted ones (V1 and HKDF, both of
/// which a V2 reader treats as encrypted). Arrays count by their indices.
fn has_plaintext_eden(doc: &Map<String, Value>) -> bool {
    match doc.get("eden") {
        Some(Value::Object(m)) => m
            .keys()
            .any(|k| k != ids::EDEN_ENCRYPTED_KEY && k != ids::EDEN_ENCRYPTED_KEY_HKDF),
        Some(Value::Array(a)) => !a.is_empty(),
        _ => false,
    }
}

/// `assertUnencryptedContentPolicy(doc, "v2", accept)`.
///
/// Errors: [`CryptoError::Rejected`] for plaintext content when
/// `accept_unencrypted_content` is false, and
/// [`Unsupported::UnreadableEncryptedContent`] for `e_: true` on a document
/// whose `data` no E2EE reader decrypts (checked regardless of the flag).
pub fn check_unencrypted_content(doc: &Value, accept_unencrypted_content: bool) -> Result<(), CryptoError> {
    match doc {
        Value::Object(m) => check(m, accept_unencrypted_content),
        _ => Ok(()),
    }
}

pub(crate) fn check(doc: &Map<String, Value>, accept_unencrypted_content: bool) -> Result<(), CryptoError> {
    let kinds = kinds(doc);
    if kinds.is_empty() {
        return Ok(());
    }
    let id = doc.get("_id").and_then(Value::as_str).unwrap_or("");
    let data_is_handled = ids::is_encrypted_chunk_id(id) || id == ids::SYNCINFO_ID;
    let marked = doc.get("e_") == Some(&Value::Bool(true));
    for kind in kinds {
        if kind != ContentKind::Eden && marked {
            if data_is_handled {
                continue;
            }
            return Err(CryptoError::Unsupported(Unsupported::UnreadableEncryptedContent));
        }
        if !accept_unencrypted_content {
            return Err(CryptoError::Rejected {
                document_id: id.to_string(),
                content_kind: kind,
            });
        }
    }
    Ok(())
}
