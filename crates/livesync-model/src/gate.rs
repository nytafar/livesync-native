//! Compatibility gates, evaluated fail-closed.
//!
//! Upstream: `pouchdb/remoteFeatureCompatibility.ts` (version document),
//! `pouchdb/LiveSyncDBFunctions.ts` (`ensureRemoteIsCompatible`: chunk-version
//! ranges, tweak comparison, lock), `common/models/tweak.definition.ts` (rules).
//!
//! This client is read-only in M1: it evaluates the gates but never writes the
//! milestone (no node registration) and never creates the salt.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const REMOTE_FEATURE_GENERATION: i64 = 13;
pub const KNOWN_FEATURES: [&str; 3] = [
    "encrypted-internal-metadata-v1",
    "independent-id-derivation-v1",
    "nfc-path-identity-v1",
];
pub const PREFERRED: &str = "PREFERRED";

/// The chunk-format range this client reads. Matches upstream's advertised
/// `node_chunk_info` (`{min:0, max:2400, current:2}`).
pub const OUR_CHUNK_RANGE: (i64, i64) = (0, 2400);

/// A reason to stop syncing. Every variant is a hard stop, shown to the user.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("the server is not CouchDB (no {{\"couchdb\":\"Welcome\"}} at the server root); only CouchDB remotes are supported")]
    NotCouchDb,
    #[error("the remote database has documents but no version document `obsydian_livesync_version`")]
    MissingVersionDoc,
    #[error("the version document is invalid: {0}")]
    InvalidVersionDoc(String),
    #[error("the remote database generation {0} is older than 12 and needs migration by an official client")]
    OlderGeneration(i64),
    #[error("the remote database generation {0} is newer than this client supports (max {REMOTE_FEATURE_GENERATION})")]
    UnsupportedGeneration(i64),
    #[error("the remote database uses features this client does not know: {0}")]
    UnknownFeatures(String),
    #[error("the remote database uses features this client does not implement yet: {0}")]
    UnsupportedFeatures(String),
    #[error("the remote database has no milestone document `_local/obsydian_livesync_milestone`; set it up with an official client first")]
    MissingMilestone,
    #[error("the milestone document is invalid: {0}")]
    InvalidMilestone(String),
    #[error("the chunk-format ranges of the accepted nodes do not overlap with this client's ({0}..{1})")]
    IncompatibleChunkVersion(i64, i64),
    #[error("the remote database is locked (a rebuild is in progress or was done elsewhere) and this node is not accepted; fetch again with an official client or unlock the remote")]
    Locked,
    #[error("the remote database was cleaned and is locked against this node")]
    Cleaned,
    #[error("the milestone has no PREFERRED tweak values")]
    MissingPreferredTweaks,
    #[error("remote setting not supported by this client: {0}")]
    UnsupportedSetting(String),
    #[error("the remote's PREFERRED settings changed since setup: {0}; re-adopt them with `lsync connect --adopt-tweaks` after checking")]
    TweakMismatch(String),
    #[error("encrypted or obfuscated content found ({0}); end-to-end encryption is not supported yet")]
    EncryptedContent(String),
    #[error("the local index belongs to a different remote ({0}); use another state directory or reset the index")]
    ForeignIndex(String),
}

/// Version document: `obsydian_livesync_version`.
/// `doc` is `None` when the document does not exist.
pub fn assess_version_doc(doc: Option<&Value>, db_has_docs: bool) -> Result<i64, Refusal> {
    let Some(doc) = doc else {
        return if db_has_docs { Err(Refusal::MissingVersionDoc) } else { Ok(0) };
    };
    let invalid = |m: &str| Err(Refusal::InvalidVersionDoc(m.to_string()));
    if doc.get("type").and_then(Value::as_str) != Some("versioninfo") {
        return invalid("type is not versioninfo");
    }
    if doc.get("_deleted").and_then(Value::as_bool) == Some(true) {
        return invalid("deleted");
    }
    let Some(version) = doc.get("version").and_then(Value::as_i64) else {
        return invalid("version is not a non-negative integer");
    };
    if version < 0 {
        return invalid("version is negative");
    }
    let has_features = doc.get("used_features").is_some();
    if version < 12 {
        return if has_features {
            invalid("used_features before generation 13")
        } else {
            Err(Refusal::OlderGeneration(version))
        };
    }
    if version > REMOTE_FEATURE_GENERATION {
        return Err(Refusal::UnsupportedGeneration(version));
    }
    if version == 12 {
        return if has_features { invalid("used_features at generation 12") } else { Ok(12) };
    }
    let Some(features) = doc.get("used_features").and_then(Value::as_array) else {
        return invalid("used_features missing at generation 13");
    };
    let mut names = Vec::new();
    for f in features {
        match f.as_str() {
            Some(s) if !s.is_empty() => names.push(s.to_string()),
            _ => return invalid("used_features has a non-string entry"),
        }
    }
    names.sort();
    names.dedup();
    let unknown: Vec<_> = names.iter().filter(|n| !KNOWN_FEATURES.contains(&n.as_str())).cloned().collect();
    if !unknown.is_empty() {
        return Err(Refusal::UnknownFeatures(unknown.join(", ")));
    }
    // All known features imply settings this client cannot handle yet
    // (encryption, keyed IDs or NFC path identity).
    if !names.is_empty() {
        return Err(Refusal::UnsupportedFeatures(names.join(", ")));
    }
    Ok(13)
}

#[derive(Debug, Clone)]
pub struct Milestone {
    pub created: i64,
    pub locked: bool,
    pub cleaned: bool,
    pub accepted_nodes: Vec<String>,
    pub node_chunk_info: BTreeMap<String, (i64, i64)>,
    pub tweak_values: BTreeMap<String, Map<String, Value>>,
}

impl Milestone {
    pub fn parse(doc: &Value) -> Result<Self, Refusal> {
        let bad = |m: &str| Refusal::InvalidMilestone(m.to_string());
        let created = match doc.get("created") {
            Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).ok_or_else(|| bad("created"))?,
            _ => return Err(bad("created missing")),
        };
        let accepted_nodes = doc
            .get("accepted_nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("accepted_nodes missing"))?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        let mut node_chunk_info = BTreeMap::new();
        if let Some(m) = doc.get("node_chunk_info").and_then(Value::as_object) {
            for (k, v) in m {
                let min = v.get("min").and_then(Value::as_i64).ok_or_else(|| bad("node_chunk_info.min"))?;
                let max = v.get("max").and_then(Value::as_i64).ok_or_else(|| bad("node_chunk_info.max"))?;
                node_chunk_info.insert(k.clone(), (min, max));
            }
        }
        let mut tweak_values = BTreeMap::new();
        if let Some(m) = doc.get("tweak_values").and_then(Value::as_object) {
            for (k, v) in m {
                if let Some(o) = v.as_object() {
                    tweak_values.insert(k.clone(), o.clone());
                }
            }
        }
        Ok(Milestone {
            created,
            locked: doc.get("locked").and_then(Value::as_bool).unwrap_or(false),
            cleaned: doc.get("cleaned").and_then(Value::as_bool).unwrap_or(false),
            accepted_nodes,
            node_chunk_info,
            tweak_values,
        })
    }

    pub fn preferred(&self) -> Option<&Map<String, Value>> {
        self.tweak_values.get(PREFERRED)
    }
}

/// `ensureRemoteIsCompatible` minus the writes: chunk ranges, then lock.
/// (Tweaks are assessed separately, see [`assess_supported_tweaks`] and
/// [`compare_tweaks`].)
pub fn assess_milestone(m: &Milestone, our_node: &str) -> Result<(), Refusal> {
    let (mut lo, mut hi) = OUR_CHUNK_RANGE;
    for node in &m.accepted_nodes {
        if node == our_node {
            continue;
        }
        match m.node_chunk_info.get(node) {
            Some(&(min, max)) => {
                lo = lo.max(min);
                hi = hi.min(max);
            }
            None => {
                lo = 0;
                hi = 0;
            }
        }
    }
    if hi < lo {
        return Err(Refusal::IncompatibleChunkVersion(OUR_CHUNK_RANGE.0, OUR_CHUNK_RANGE.1));
    }
    if m.locked && !m.accepted_nodes.iter().any(|n| n == our_node) {
        return Err(if m.cleaned { Refusal::Cleaned } else { Refusal::Locked });
    }
    Ok(())
}

/// Tweak keys upstream compares, with the default used when a key is absent
/// (`knownDefault`). `None` means "unadvertised when absent".
pub const TWEAK_RULES: [(&str, Option<KnownDefault>); 21] = [
    ("minimumChunkSize", None),
    ("longLineThreshold", None),
    ("encrypt", None),
    ("usePathObfuscation", None),
    ("idDerivationVersion", Some(KnownDefault::Int(0))),
    ("pathNormalizationVersion", Some(KnownDefault::Int(0))),
    ("encryptInternalMetadata", Some(KnownDefault::Bool(false))),
    ("enableCompression", None),
    ("useEden", None),
    ("customChunkSize", None),
    ("useDynamicIterationCount", None),
    ("hashAlg", None),
    ("enableChunkSplitterV2", None),
    ("maxChunksInEden", None),
    ("maxTotalLengthInEden", None),
    ("maxAgeInEden", None),
    ("usePluginSyncV2", Some(KnownDefault::Bool(false))),
    ("handleFilenameCaseSensitive", Some(KnownDefault::Bool(false))),
    ("useSegmenter", None),
    ("E2EEAlgorithm", Some(KnownDefault::Str("v2"))),
    ("chunkSplitterVersion", Some(KnownDefault::Str("v3-rabin-karp"))),
];

#[derive(Debug, Clone, Copy)]
pub enum KnownDefault {
    Int(i64),
    Bool(bool),
    Str(&'static str),
}

impl KnownDefault {
    fn to_value(self) -> Value {
        match self {
            KnownDefault::Int(i) => Value::from(i),
            KnownDefault::Bool(b) => Value::from(b),
            KnownDefault::Str(s) => Value::from(s),
        }
    }
}

fn effective(m: &Map<String, Value>, key: &str, default: Option<KnownDefault>) -> Option<Value> {
    match m.get(key) {
        Some(Value::Null) | None => default.map(KnownDefault::to_value),
        Some(v) => Some(v.clone()),
    }
}

/// Keys whose effective values differ between the tweaks adopted at setup and
/// the remote's current PREFERRED values (upstream's "different" relation).
pub fn compare_tweaks(adopted: &Map<String, Value>, preferred: &Map<String, Value>) -> Vec<String> {
    let keyed = |m: &Map<String, Value>| {
        m.get("encrypt").and_then(Value::as_bool).unwrap_or(false)
            && m.get("usePathObfuscation").and_then(Value::as_bool).unwrap_or(false)
    };
    let mut out = Vec::new();
    for (key, default) in TWEAK_RULES {
        if key == "idDerivationVersion" && !keyed(adopted) && !keyed(preferred) {
            continue;
        }
        let a = effective(adopted, key, default);
        let p = effective(preferred, key, default);
        match (a, p) {
            (Some(a), Some(p)) if a != p => out.push(format!("{key}: {a} -> {p}")),
            _ => {} // equal or unadvertised
        }
    }
    out
}

/// The settings this client can read. Absent keys take upstream's defaults.
pub fn assess_supported_tweaks(p: &Map<String, Value>) -> Result<(), Refusal> {
    let unsupported = |m: String| Err(Refusal::UnsupportedSetting(m));
    let b = |k: &str| p.get(k).and_then(Value::as_bool).unwrap_or(false);
    if b("encrypt") {
        return unsupported("encrypt=true (end-to-end encryption arrives in M3)".into());
    }
    if b("usePathObfuscation") {
        return unsupported("usePathObfuscation=true".into());
    }
    if b("useDynamicIterationCount") {
        return unsupported("useDynamicIterationCount=true".into());
    }
    if b("encryptInternalMetadata") {
        return unsupported("encryptInternalMetadata=true".into());
    }
    match p.get("pathNormalizationVersion") {
        None | Some(Value::Null) => {}
        Some(v) if v.as_i64() == Some(0) => {}
        Some(v) => return unsupported(format!("pathNormalizationVersion={v}")),
    }
    match p.get("idDerivationVersion") {
        None | Some(Value::Null) => {}
        Some(v) if v.as_i64() == Some(0) => {}
        Some(v) => return unsupported(format!("idDerivationVersion={v}")),
    }
    match p.get("hashAlg") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if s == "xxhash64" => {}
        Some(v) => return unsupported(format!("hashAlg={v} (only xxhash64)")),
    }
    match p.get("handleFilenameCaseSensitive") {
        None | Some(Value::Null) | Some(Value::Bool(_)) => {}
        Some(v) => return unsupported(format!("handleFilenameCaseSensitive={v}")),
    }
    Ok(())
}

/// `syncMaxSizeInMB` from the tweaks (0 or absent means "no limit" upstream;
/// we fall back to 50 MB when absent, upstream's default).
pub fn max_size_mb(p: &Map<String, Value>) -> u64 {
    p.get("syncMaxSizeInMB").and_then(Value::as_u64).unwrap_or(50)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vd(v: Value) -> Result<i64, Refusal> {
        assess_version_doc(Some(&v), true)
    }

    #[test]
    fn version_doc() {
        assert_eq!(vd(json!({"type":"versioninfo","version":12})), Ok(12));
        assert_eq!(vd(json!({"type":"versioninfo","version":13,"used_features":[]})), Ok(13));
        assert!(matches!(vd(json!({"type":"versioninfo","version":14})), Err(Refusal::UnsupportedGeneration(14))));
        assert!(matches!(
            vd(json!({"type":"versioninfo","version":13,"used_features":["warp-drive"]})),
            Err(Refusal::UnknownFeatures(_))
        ));
        assert!(matches!(
            vd(json!({"type":"versioninfo","version":13,"used_features":["nfc-path-identity-v1"]})),
            Err(Refusal::UnsupportedFeatures(_))
        ));
        assert!(matches!(vd(json!({"type":"versioninfo","version":13})), Err(Refusal::InvalidVersionDoc(_))));
        assert!(matches!(vd(json!({"type":"versioninfo","version":12,"used_features":[]})), Err(Refusal::InvalidVersionDoc(_))));
        assert!(matches!(vd(json!({"type":"versioninfo","version":11})), Err(Refusal::OlderGeneration(11))));
        assert!(matches!(vd(json!({"type":"x","version":12})), Err(Refusal::InvalidVersionDoc(_))));
        assert_eq!(assess_version_doc(None, true), Err(Refusal::MissingVersionDoc));
        assert_eq!(assess_version_doc(None, false), Ok(0));
    }

    fn milestone(locked: bool, nodes: &[&str]) -> Milestone {
        Milestone::parse(&json!({
            "created": 1, "locked": locked, "accepted_nodes": nodes,
            "node_chunk_info": {"a": {"min":0,"max":2400,"current":2}},
            "tweak_values": {"PREFERRED": {"encrypt": false}}
        }))
        .unwrap()
    }

    #[test]
    fn lock() {
        assert_eq!(assess_milestone(&milestone(false, &["a"]), "me"), Ok(()));
        assert_eq!(assess_milestone(&milestone(true, &["a"]), "me"), Err(Refusal::Locked));
        assert_eq!(assess_milestone(&milestone(true, &["a", "me"]), "me"), Ok(()));
        let mut m = milestone(true, &["a"]);
        m.cleaned = true;
        assert_eq!(assess_milestone(&m, "me"), Err(Refusal::Cleaned));
    }

    #[test]
    fn chunk_ranges() {
        let mut m = milestone(false, &["a", "b"]);
        m.node_chunk_info.insert("b".into(), (2500, 3000));
        assert!(matches!(assess_milestone(&m, "me"), Err(Refusal::IncompatibleChunkVersion(..))));
        // A node without chunk info collapses the range to 0..0, which still overlaps.
        let m = milestone(false, &["a", "z"]);
        assert_eq!(assess_milestone(&m, "me"), Ok(()));
    }

    #[test]
    fn tweaks() {
        let ok = json!({"encrypt":false,"usePathObfuscation":false,"hashAlg":"xxhash64","handleFilenameCaseSensitive":false});
        assert_eq!(assess_supported_tweaks(ok.as_object().unwrap()), Ok(()));
        for bad in [
            json!({"encrypt":true}),
            json!({"usePathObfuscation":true}),
            json!({"hashAlg":"sha1"}),
            json!({"hashAlg":""}),
            json!({"pathNormalizationVersion":1}),
            json!({"useDynamicIterationCount":true}),
        ] {
            assert!(assess_supported_tweaks(bad.as_object().unwrap()).is_err(), "{bad}");
        }
        let a = json!({"customChunkSize":60,"hashAlg":"xxhash64"});
        let b = json!({"customChunkSize":50,"hashAlg":"xxhash64","newKey":1});
        let diff = compare_tweaks(a.as_object().unwrap(), b.as_object().unwrap());
        assert_eq!(diff.len(), 1);
        assert!(diff[0].starts_with("customChunkSize"));
        // Known default: absent equals the default.
        let a = json!({"pathNormalizationVersion":0});
        let b = json!({});
        assert!(compare_tweaks(a.as_object().unwrap(), b.as_object().unwrap()).is_empty());
        // Unadvertised (no known default) is not a mismatch.
        let a = json!({"useEden":false});
        assert!(compare_tweaks(a.as_object().unwrap(), b.as_object().unwrap()).is_empty());
    }
}
