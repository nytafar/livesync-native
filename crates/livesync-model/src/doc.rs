//! Document shapes as they appear on the wire.
//!
//! Upstream: `livesync-commonlib/src/common/models/db.type.ts`.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

pub const VERSION_DOC_ID: &str = "obsydian_livesync_version";
pub const MILESTONE_LOCAL_ID: &str = "obsydian_livesync_milestone";
pub const SYNC_PARAMS_LOCAL_ID: &str = "obsidian_livesync_sync_parameters";

/// Content encoding of a file document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// `type: "plain"` — chunks are UTF-8 text pieces.
    Plain,
    /// `type: "newnote"` — chunks are base64 pieces of the bytes.
    Binary,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EdenEntry {
    pub data: String,
}

/// A `plain` or `newnote` metadata document.
#[derive(Debug, Clone)]
pub struct FileDoc {
    pub id: String,
    pub rev: String,
    pub path: String,
    pub kind: FileKind,
    pub children: Vec<String>,
    pub mtime: i64,
    pub ctime: i64,
    pub size: u64,
    pub deleted: bool,
    pub conflicts: Vec<String>,
    pub eden: BTreeMap<String, String>,
}

/// What a document in the changes feed is, from a file-sync point of view.
#[derive(Debug, Clone)]
pub enum Classified {
    /// A live or soft-deleted file document.
    File(FileDoc),
    /// A `_deleted` tombstone (body gone); only the ID is known.
    Tombstone { id: String, rev: String },
    /// The version/feature control document.
    VersionDoc(Value),
    /// A legacy `notes` document (inline data); not supported.
    LegacyNote { id: String, path: Option<String> },
    /// A document whose metadata is encrypted or obfuscated.
    Encrypted { id: String },
    /// Chunks and other documents the file view does not care about.
    Other { id: String, kind: String },
}

fn str_field<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn i64_field(v: &Value, k: &str) -> i64 {
    match v.get(k) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        _ => 0,
    }
}

/// Classify a document body (from `include_docs` or a `GET`).
pub fn classify_doc(doc: &Value) -> Classified {
    let id = str_field(doc, "_id").unwrap_or_default().to_string();
    let rev = str_field(doc, "_rev").unwrap_or_default().to_string();
    if doc.get("_deleted").and_then(Value::as_bool) == Some(true) && doc.get("type").is_none() {
        return Classified::Tombstone { id, rev };
    }
    if id == VERSION_DOC_ID {
        return Classified::VersionDoc(doc.clone());
    }
    let ty = str_field(doc, "type");
    match ty {
        Some("plain") | Some("newnote") => {}
        Some("notes") | None => {
            if id.starts_with("_design/") {
                return Classified::Other {
                    id,
                    kind: "design".into(),
                };
            }
            return Classified::LegacyNote {
                id,
                path: str_field(doc, "path").map(str::to_string),
            };
        }
        Some(other) => {
            return Classified::Other {
                id,
                kind: other.to_string(),
            };
        }
    }
    let path = str_field(doc, "path").unwrap_or_default().to_string();
    // Obfuscated IDs (`f:`) or the encrypted metadata envelope (`/\:`).
    let body_id = id
        .strip_prefix("i:")
        .or_else(|| id.strip_prefix("ix:"))
        .or_else(|| id.strip_prefix("ps:"))
        .unwrap_or(&id);
    if body_id.starts_with("f:") || path.starts_with("/\\:") {
        return Classified::Encrypted { id };
    }
    let kind = if ty == Some("plain") {
        FileKind::Plain
    } else {
        FileKind::Binary
    };
    let children = doc
        .get("children")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let conflicts = doc
        .get("_conflicts")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let eden = doc
        .get("eden")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    v.get("data")
                        .and_then(Value::as_str)
                        .map(|d| (k.clone(), d.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let deleted = doc.get("deleted").and_then(Value::as_bool).unwrap_or(false)
        || doc.get("_deleted").and_then(Value::as_bool).unwrap_or(false);
    let size = match doc.get("size") {
        Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)).unwrap_or(0),
        _ => 0,
    };
    Classified::File(FileDoc {
        id,
        rev,
        path,
        kind,
        children,
        mtime: i64_field(doc, "mtime"),
        ctime: i64_field(doc, "ctime"),
        size,
        deleted,
        conflicts,
        eden,
    })
}

/// A chunk (`type: "leaf"`) as returned by `_all_docs?include_docs=true`.
#[derive(Debug, Clone, Deserialize)]
pub struct ChunkDoc {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(default)]
    pub data: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(rename = "e_", default)]
    pub encrypted: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classify_file() {
        let d = json!({"_id":"a.md","_rev":"1-x","path":"A.md","type":"plain","children":["h:1"],
            "mtime":5,"ctime":4,"size":3,"eden":{"h:2":{"data":"x","epoch":1}},"_conflicts":["2-y"]});
        match classify_doc(&d) {
            Classified::File(f) => {
                assert_eq!(f.path, "A.md");
                assert_eq!(f.kind, FileKind::Plain);
                assert_eq!(f.children, vec!["h:1"]);
                assert_eq!(f.eden.get("h:2").unwrap(), "x");
                assert_eq!(f.conflicts, vec!["2-y"]);
                assert!(!f.deleted);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn classify_others() {
        assert!(matches!(
            classify_doc(&json!({"_id":"x","_rev":"2-a","_deleted":true})),
            Classified::Tombstone { .. }
        ));
        assert!(matches!(
            classify_doc(&json!({"_id":"f:abc","_rev":"1-a","type":"plain","path":"/\\:%=xx"})),
            Classified::Encrypted { .. }
        ));
        assert!(matches!(
            classify_doc(&json!({"_id":"n.md","_rev":"1-a","data":"x"})),
            Classified::LegacyNote { .. }
        ));
        assert!(matches!(
            classify_doc(&json!({"_id":"h:1","_rev":"1-a","type":"leaf","data":"x"})),
            Classified::Other { .. }
        ));
        assert!(matches!(
            classify_doc(&json!({"_id":"obsydian_livesync_version","_rev":"1-a","type":"versioninfo","version":12})),
            Classified::VersionDoc(_)
        ));
    }
}
