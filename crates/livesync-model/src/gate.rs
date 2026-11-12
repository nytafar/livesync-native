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

/// The newest Self-hosted LiveSync release this client was checked against
/// (the interop scenarios against the official CLI of the same release tag,
/// which shares the plugin's `livesync-commonlib`): the pinned release
/// ([`crate::upstream::PLUGIN_RELEASE`], from `upstream.env`). Named in every
/// refusal that an upstream change causes.
pub const TESTED_PLUGIN_VERSION: &str = crate::upstream::PLUGIN_RELEASE;

/// What a refusal caused by an upstream change says about known-good versions.
const KNOWN_GOOD: &str = concat!(
    "Self-hosted LiveSync ",
    env!("LSYNC_UPSTREAM_PLUGIN_RELEASE"),
    " is the newest version known to work with lsync, as long as it does not turn on what is named here (see COMPATIBILITY.md)"
);

/// What `pathNormalizationVersion` 1 and the feature `nfc-path-identity-v1`
/// mean, for their refusals.
const NFC_PATH_IDENTITY: &str = "NFC path identity, new in Self-hosted LiveSync 1.0.46 for new databases: each path is normalised to NFC and that becomes part of its document ID, which lsync does not implement yet; a database whose first device set pathNormalizationVersion to 0 before its first sync works";

/// Every key a PREFERRED tweak set may hold that this client knows about:
/// the compared ones ([`TWEAK_RULES`]), the rest of upstream's
/// `TweakValuesTemplate` (`tweak.definition.ts`), and keys older plugin
/// versions wrote there. Any other key is a setting a newer plugin added,
/// and is refused until it is adopted ([`unknown_tweaks`]).
pub const KNOWN_UNCOMPARED_TWEAKS: [&str; 15] = [
    // TweakValuesRecommendedTemplate
    "useIgnoreFiles",
    "useCustomRequestHandler",
    "batch_size",
    "batches_limit",
    "useTimeouts",
    "readChunksOnline",
    "hashCacheMaxCount",
    "hashCacheMaxAmount",
    "concurrencyOfReadChunksOnline",
    "minimumIntervalOfReadChunksOnline",
    "ignoreFiles",
    "syncMaxSizeInMB",
    // TweakValuesTemplate
    "tweakModified",
    // Written by older plugin versions (0.2x).
    "doNotUseFixedRevisionForChunks",
    "useIndexedDBAdapter",
];

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
    #[error("the remote database's version document (`obsydian_livesync_version`) is at generation {0}, newer than this client knows (12 and {REMOTE_FEATURE_GENERATION}): a newer Self-hosted LiveSync raised it. {KNOWN_GOOD}")]
    UnsupportedGeneration(i64),
    #[error("the remote database's version document lists features this client does not know: {0}; a newer Self-hosted LiveSync turned them on, and they stay on. {KNOWN_GOOD}")]
    UnknownFeatures(String),
    #[error(
        "the remote database's version document lists features this client does not implement yet: {0}. {KNOWN_GOOD}"
    )]
    UnsupportedFeatures(String),
    #[error("the remote database has no milestone document `_local/obsydian_livesync_milestone`; set it up with an official client first")]
    MissingMilestone,
    #[error("the milestone document is invalid: {0}")]
    InvalidMilestone(String),
    #[error("the chunk-format ranges of the accepted nodes do not overlap with this client's ({0}..{1}): a newer Self-hosted LiveSync may write chunks in a format this client does not read. {KNOWN_GOOD}")]
    IncompatibleChunkVersion(i64, i64),
    #[error("the remote database is locked (a rebuild is in progress or was done elsewhere) and this node is not accepted; fetch again with an official client or unlock the remote")]
    Locked,
    /// Locked against this device while its `created` is still the one this
    /// device synced with: no rebuild came of the lock (yet). Only a person
    /// can tell whether one was meant, so it is never unlocked automatically.
    #[error("the remote database is locked against this device, but it was not rebuilt (its `created` is still the one this device synced with){}: another device locked it for a rebuild that has not happened (it may have failed, or still be under way). This client never unlocks it by itself, because only you can tell whether the rebuild was meant. If it was not, unlock the database from an official client (the plugin's or the CLI's \"unlock\"); if you want this device to start over from the remote as it is, fetch (`lsync fetch`, or Fetch in the app), which marks it accepted once it completes. Syncing resumes by itself after an unlock or once the rebuild is done", holders_text(.holders, true))]
    LockedWithoutRebuild { holders: Vec<LockHolder> },
    #[error("the remote database was cleaned and is locked against this node")]
    Cleaned,
    #[error("the milestone has no PREFERRED tweak values")]
    MissingPreferredTweaks,
    #[error("remote setting not supported by this client: {0}. {KNOWN_GOOD}")]
    UnsupportedSetting(String),
    #[error("the remote's PREFERRED settings (the milestone's tweaks) changed since this device adopted them: {0}. {KNOWN_GOOD}; once you checked the change was meant, adopt it with `lsync connect --adopt-tweaks`")]
    TweakMismatch(String),
    #[error("the remote's PREFERRED settings (the milestone's tweaks) hold settings this client does not know: {0}; a newer Self-hosted LiveSync may have added them. {KNOWN_GOOD}; once you checked they do not change what is stored, adopt them with `lsync connect --adopt-tweaks`")]
    UnknownTweaks(String),
    #[error("encrypted or obfuscated content found ({0}) that this vault cannot read: lsync reads end-to-end encrypted databases (encrypt=true with a passphrase set for the vault, E2EE algorithm V2) but not path obfuscation, and never encrypted chunks in a database whose settings say encrypt=false. {KNOWN_GOOD}")]
    EncryptedContent(String),
    /// The 1.0.49 unencrypted-content policy rejected a remote document:
    /// plaintext content in an end-to-end encrypted database. Like upstream's
    /// replication, the pull stops there (nothing of that batch is applied).
    #[error("the remote document {document} holds unencrypted {kind} content, which an end-to-end encrypted database must not; it was rejected and the pull stops before it, as Self-hosted LiveSync 1.0.49 does. A device without end-to-end encryption may have written it: check that device's settings. If this content is meant, set `accept_unencrypted_content = true` for the vault (the plugin's \"acceptUnencryptedContent\")")]
    UnencryptedContent { document: String, kind: String },
    /// `encrypt=true` on the remote, and no passphrase for this vault.
    /// `how`: how this front-end sets one.
    #[error("the remote database is end-to-end encrypted (encrypt=true in its PREFERRED settings) and this vault has no passphrase; {how}")]
    PassphraseMissing { how: String },
    /// A passphrase for this vault, and a remote that is not encrypted.
    #[error("this vault has an end-to-end encryption passphrase, but the remote database is not encrypted (encrypt is not true in its PREFERRED settings); lsync never writes encrypted content into a plaintext database. Remove the passphrase from the vault (`lsync vault passphrase NAME --remove`) or check that the vault points at the right database")]
    PassphraseOnPlainDatabase,
    /// The passphrase does not decrypt what the database holds.
    #[error("the vault's passphrase does not decrypt the remote database ({0} failed authentication): it is not the passphrase this database was encrypted with. Nothing was written to the vault folder or the remote; set the right passphrase with `lsync vault passphrase NAME`")]
    WrongPassphrase(String),
    /// Nothing the passphrase could be checked against decrypts with V2.
    #[error("the vault's passphrase cannot be checked: {0}. Nothing is synced until it can")]
    UnverifiablePassphrase(String),
    /// No PBKDF2 salt (`_local/obsidian_livesync_sync_parameters`).
    #[error("the remote database has no encryption salt (`_local/obsidian_livesync_sync_parameters` {0}); {1}")]
    NoSyncParameters(String, String),
    #[error("the local index belongs to a different remote ({0}); use another state directory or reset the index")]
    ForeignIndex(String),
    #[error("the local index belongs to the vault folder {index_vault}, not {vault}; use the profile or state directory that belongs to {vault}, or a new one")]
    ForeignVault { index_vault: String, vault: String },
    /// `handover`: how this front-end allows a deliberate handover (a flag,
    /// a setting), or `None` where it has no such switch (the Android app).
    #[error("{vault} is an Obsidian vault with the Self-hosted LiveSync plugin enabled ({why}): the plugin and this client would both write this folder and the database. Disable the plugin in Obsidian on this device (Settings, Community plugins){}", handover_text(.handover))]
    PluginEnabled {
        vault: String,
        why: String,
        handover: Option<String>,
    },
    /// The plugin guard could not look into `folder` (a top-level
    /// dot-folder, or the vault itself), so it fails closed. The folder may
    /// have nothing to do with Obsidian (a root-owned `.snapshots`).
    #[error("{}", unchecked_text(.vault, .folder, .why, .handover))]
    PluginUnchecked {
        vault: String,
        folder: String,
        why: String,
        handover: Option<String>,
    },
}

impl Refusal {
    /// Whether this refusal means the remote moved on past what this client
    /// knows (a newer plugin's generation, features or settings): the upstream
    /// guard's stop, shown as such in `status` and lifted only when the
    /// remote is compatible again (or the change is adopted).
    pub fn is_upstream_change(&self) -> bool {
        matches!(
            self,
            Refusal::UnsupportedGeneration(_)
                | Refusal::UnknownFeatures(_)
                | Refusal::UnsupportedFeatures(_)
                | Refusal::IncompatibleChunkVersion(..)
                | Refusal::UnsupportedSetting(_)
                | Refusal::TweakMismatch(_)
                | Refusal::UnknownTweaks(_)
                | Refusal::EncryptedContent(_)
                | Refusal::UnencryptedContent { .. }
        )
    }

    /// What stays the same for as long as the cause does: the message
    /// without what moves on its own (a lock holder's last connection). Two
    /// refusals with the same reason are one state, so a repeated refusal
    /// is neither recorded, reported nor logged again.
    pub fn reason(&self) -> String {
        match self {
            Refusal::LockedWithoutRebuild { holders } => {
                format!("locked without a rebuild{}", holders_text(holders, false))
            }
            r => r.to_string(),
        }
    }
}

/// A node that holds a lock ([`Refusal::LockedWithoutRebuild`]), from the
/// milestone's `accepted_nodes` and `node_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockHolder {
    pub node: String,
    /// Its `device_name`, when it has one other than the node ID.
    pub device: Option<String>,
    /// Its `last_connected`, in ms since the epoch: the closest to the lock
    /// time the milestone tells (upstream records no lock time).
    pub last_connected: Option<i64>,
}

/// `"; locked by ..."` (empty for no holders), with or without when each
/// last connected.
fn holders_text(holders: &[LockHolder], with_times: bool) -> String {
    if holders.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = holders
        .iter()
        .map(|h| {
            let who = match &h.device {
                Some(n) => format!("{n:?} (node {})", h.node),
                None => format!("node {}", h.node),
            };
            match h.last_connected {
                Some(t) if with_times => format!("{who}, last connected {}", utc_minute(t)),
                _ => who,
            }
        })
        .collect();
    format!("; locked by {}", parts.join(" and "))
}

/// How a refused plugin vault can be handed over, for [`Refusal::PluginEnabled`].
fn handover_text(handover: &Option<String>) -> String {
    match handover {
        Some(h) => format!(", or, for a deliberate handover, {h}"),
        None => ", then start syncing here again; this app does not run beside the plugin".into(),
    }
}

/// The message of [`Refusal::PluginUnchecked`]: a dot-folder in the vault,
/// or the vault folder itself.
fn unchecked_text(vault: &str, folder: &str, why: &str, handover: &Option<String>) -> String {
    let fix = if folder == vault {
        format!("this client does not sync it two-way: make {vault} listable by this user")
    } else {
        format!("this client does not sync {vault} two-way: make {folder} readable to this user, or move it out of the vault")
    };
    format!(
        "{folder} could not be checked for the Self-hosted LiveSync plugin ({why}), so {fix}{}",
        unchecked_handover_text(handover)
    )
}

/// The override for [`Refusal::PluginUnchecked`], where there is one.
fn unchecked_handover_text(handover: &Option<String>) -> String {
    match handover {
        Some(h) => format!("; if you know the plugin does not run in this vault, {h}"),
        None => String::new(),
    }
}

/// Version document: `obsydian_livesync_version`.
/// `doc` is `None` when the document does not exist.
pub fn assess_version_doc(doc: Option<&Value>, db_has_docs: bool) -> Result<i64, Refusal> {
    let Some(doc) = doc else {
        return if db_has_docs {
            Err(Refusal::MissingVersionDoc)
        } else {
            Ok(0)
        };
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
        return if has_features {
            invalid("used_features at generation 12")
        } else {
            Ok(12)
        };
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
    let unknown: Vec<_> = names
        .iter()
        .filter(|n| !KNOWN_FEATURES.contains(&n.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(Refusal::UnknownFeatures(unknown.join(", ")));
    }
    // All known features imply settings this client cannot handle yet
    // (encryption, keyed IDs or NFC path identity).
    if !names.is_empty() {
        let named: Vec<String> = names
            .iter()
            .map(|n| match n.as_str() {
                "nfc-path-identity-v1" => format!("{n} ({NFC_PATH_IDENTITY})"),
                _ => n.clone(),
            })
            .collect();
        return Err(Refusal::UnsupportedFeatures(named.join(", ")));
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
            Some(Value::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().map(|f| f as i64))
                .ok_or_else(|| bad("created"))?,
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
                let min = v
                    .get("min")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| bad("node_chunk_info.min"))?;
                let max = v
                    .get("max")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| bad("node_chunk_info.max"))?;
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

/// PREFERRED tweaks this client does not know ([`TWEAK_RULES`] and
/// [`KNOWN_UNCOMPARED_TWEAKS`] aside), unless `adopted` holds the same key with
/// the same value (a person adopted it with `--adopt-tweaks`, or it was there
/// when this device first adopted the tweaks). Each as `key=value`, or
/// `key: old -> new` when an adopted value changed. A key that went away is
/// not new, so it is not listed.
pub fn unknown_tweaks(adopted: Option<&Map<String, Value>>, preferred: &Map<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    for (key, value) in preferred {
        let known = TWEAK_RULES.iter().any(|(k, _)| k == key) || KNOWN_UNCOMPARED_TWEAKS.contains(&key.as_str());
        if known {
            continue;
        }
        match adopted.and_then(|a| a.get(key)) {
            Some(a) if a == value => {}
            Some(a) => out.push(format!("{key}: {a} -> {value}")),
            None => out.push(format!("{key}={value}")),
        }
    }
    out
}

/// The newest Self-hosted LiveSync the milestone's `node_info` lists (lsync
/// nodes and `our_node` aside), for refusal messages: which release raised
/// the remote past this client, as far as the milestone tells.
pub fn newest_remote_plugin(milestone: &Value, our_node: &str) -> Option<String> {
    let info = milestone.get("node_info")?.as_object()?;
    let mut best: Option<(Vec<u64>, String, String)> = None;
    for (node, i) in info {
        if node == our_node || i.get("app_version").and_then(Value::as_str) == Some("lsync") {
            continue;
        }
        let Some(v) = i.get("plugin_version").and_then(Value::as_str) else {
            continue;
        };
        let key = version_key(v);
        if best.as_ref().is_none_or(|(b, _, _)| key > *b) {
            let device = i.get("device_name").and_then(Value::as_str).unwrap_or(node).to_string();
            best = Some((key, v.to_string(), device));
        }
    }
    best.map(|(_, v, device)| format!("Self-hosted LiveSync {v} on {device:?}"))
}

/// Who holds a lock, for [`Refusal::LockedWithoutRebuild`]: the nodes the
/// milestone accepts (the device that locked it), each with its device name
/// and when it last connected; empty when the milestone names none.
pub fn lock_holders(milestone: &Value) -> Vec<LockHolder> {
    let accepted: Vec<&str> = milestone
        .get("accepted_nodes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    accepted
        .iter()
        .map(|node| {
            let info = milestone.get("node_info").and_then(|i| i.get(*node));
            let device = info
                .and_then(|i| i.get("device_name"))
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty() && n != node)
                .map(str::to_string);
            let last_connected = info
                .and_then(|i| i.get("last_connected"))
                .and_then(Value::as_i64)
                .filter(|t| *t > 0);
            LockHolder {
                node: node.to_string(),
                device,
                last_connected,
            }
        })
        .collect()
}

/// A time in ms since the epoch as `2026-11-07 09:12 UTC`: fixed, so a
/// message that holds it stays the same from one look to the next.
fn utc_minute(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// `1.0.46` -> `[1, 0, 46]`; anything after the numbers is ignored.
fn version_key(v: &str) -> Vec<u64> {
    v.split(['.', '-', '+']).map_while(|p| p.parse::<u64>().ok()).collect()
}

/// The settings this client can read. Absent keys take upstream's defaults.
/// Refuses with the first setting [`unsupported_tweaks`] finds.
pub fn assess_supported_tweaks(p: &Map<String, Value>) -> Result<(), Refusal> {
    match unsupported_tweaks(p).into_iter().next() {
        Some(r) => Err(r),
        None => Ok(()),
    }
}

/// Every setting in `p` this client cannot read, in a fixed order (empty when
/// it can read them all).
pub fn unsupported_tweaks(p: &Map<String, Value>) -> Vec<Refusal> {
    let mut out = Vec::new();
    let mut unsupported = |m: String| out.push(Refusal::UnsupportedSetting(m));
    let b = |k: &str| p.get(k).and_then(Value::as_bool).unwrap_or(false);
    if b("encrypt") {
        // V2 only: `""` is V1 and `forceV1` writes V1; absent is upstream's
        // known default, V2.
        match p.get("E2EEAlgorithm") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if s == "v2" => {}
            Some(v) => unsupported(format!(
                "E2EEAlgorithm={v} with encrypt=true (lsync reads and writes only the V2 algorithm, \"v2\")"
            )),
        }
    }
    if b("usePathObfuscation") {
        unsupported("usePathObfuscation=true".into());
    }
    if b("useDynamicIterationCount") {
        unsupported("useDynamicIterationCount=true".into());
    }
    if b("encryptInternalMetadata") {
        unsupported("encryptInternalMetadata=true".into());
    }
    match p.get("pathNormalizationVersion") {
        None | Some(Value::Null) => {}
        Some(v) if v.as_i64() == Some(0) => {}
        Some(v) => unsupported(format!("pathNormalizationVersion={v} ({NFC_PATH_IDENTITY})")),
    }
    match p.get("idDerivationVersion") {
        None | Some(Value::Null) => {}
        Some(v) if v.as_i64() == Some(0) => {}
        Some(v) => unsupported(format!("idDerivationVersion={v}")),
    }
    match p.get("hashAlg") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if s == "xxhash64" => {}
        Some(v) => unsupported(format!("hashAlg={v} (only xxhash64)")),
    }
    match p.get("handleFilenameCaseSensitive") {
        None | Some(Value::Null) | Some(Value::Bool(_)) => {}
        Some(v) => unsupported(format!("handleFilenameCaseSensitive={v}")),
    }
    out
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
        assert_eq!(
            vd(json!({"type":"versioninfo","version":13,"used_features":[]})),
            Ok(13)
        );
        assert!(matches!(
            vd(json!({"type":"versioninfo","version":14})),
            Err(Refusal::UnsupportedGeneration(14))
        ));
        assert!(matches!(
            vd(json!({"type":"versioninfo","version":13,"used_features":["warp-drive"]})),
            Err(Refusal::UnknownFeatures(_))
        ));
        // NFC path identity (1.0.46) is refused, with what it is, both as a
        // feature and as the setting.
        let nfc = vd(json!({"type":"versioninfo","version":13,"used_features":["nfc-path-identity-v1"]}));
        let Err(r @ Refusal::UnsupportedFeatures(_)) = nfc else {
            panic!("{nfc:?}")
        };
        let msg = r.to_string();
        assert!(
            msg.contains("nfc-path-identity-v1 (NFC path identity, new in Self-hosted LiveSync 1.0.46"),
            "{msg}"
        );
        let setting = unsupported_tweaks(json!({"pathNormalizationVersion":1}).as_object().unwrap());
        let msg = setting[0].to_string();
        assert!(msg.contains("pathNormalizationVersion=1 (NFC path identity"), "{msg}");
        assert!(msg.contains("set pathNormalizationVersion to 0"), "{msg}");
        assert!(matches!(
            vd(json!({"type":"versioninfo","version":13})),
            Err(Refusal::InvalidVersionDoc(_))
        ));
        assert!(matches!(
            vd(json!({"type":"versioninfo","version":12,"used_features":[]})),
            Err(Refusal::InvalidVersionDoc(_))
        ));
        assert!(matches!(
            vd(json!({"type":"versioninfo","version":11})),
            Err(Refusal::OlderGeneration(11))
        ));
        assert!(matches!(
            vd(json!({"type":"x","version":12})),
            Err(Refusal::InvalidVersionDoc(_))
        ));
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
        assert!(matches!(
            assess_milestone(&m, "me"),
            Err(Refusal::IncompatibleChunkVersion(..))
        ));
        // A node without chunk info collapses the range to 0..0, which still overlaps.
        let m = milestone(false, &["a", "z"]);
        assert_eq!(assess_milestone(&m, "me"), Ok(()));
    }

    #[test]
    fn tweaks() {
        let ok = json!({"encrypt":false,"usePathObfuscation":false,"hashAlg":"xxhash64","handleFilenameCaseSensitive":false});
        assert_eq!(assess_supported_tweaks(ok.as_object().unwrap()), Ok(()));
        // End-to-end encryption with V2 (or upstream's default) passes here;
        // whether the vault has the passphrase is the engine's to judge.
        for good in [
            json!({"encrypt":true}),
            json!({"encrypt":true,"E2EEAlgorithm":"v2"}),
            json!({"encrypt":false,"E2EEAlgorithm":""}),
        ] {
            assert_eq!(assess_supported_tweaks(good.as_object().unwrap()), Ok(()), "{good}");
        }
        for bad in [
            json!({"encrypt":true,"E2EEAlgorithm":""}),
            json!({"encrypt":true,"E2EEAlgorithm":"forceV1"}),
            json!({"encrypt":true,"encryptInternalMetadata":true}),
            json!({"encrypt":true,"idDerivationVersion":1}),
            json!({"usePathObfuscation":true}),
            json!({"hashAlg":"sha1"}),
            json!({"hashAlg":""}),
            json!({"pathNormalizationVersion":1}),
            json!({"useDynamicIterationCount":true}),
        ] {
            assert!(assess_supported_tweaks(bad.as_object().unwrap()).is_err(), "{bad}");
        }
        // All of them at once, the first one also refusing alone.
        let all = json!({"usePathObfuscation":true,"hashAlg":"sha1","pathNormalizationVersion":1});
        let found = unsupported_tweaks(all.as_object().unwrap());
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(assess_supported_tweaks(all.as_object().unwrap()), Err(found[0].clone()));
        assert!(unsupported_tweaks(ok.as_object().unwrap()).is_empty());
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

    #[test]
    fn unknown_tweaks_are_named_with_their_values() {
        let o = |v: Value| v.as_object().unwrap().clone();
        // Upstream's whole template, and the legacy keys, are known.
        let p = o(json!({"customChunkSize": 60, "batch_size": 25, "tweakModified": 1,
            "doNotUseFixedRevisionForChunks": true, "ignoreFiles": ".gitignore"}));
        assert!(unknown_tweaks(None, &p).is_empty());
        // A new key is refused with its value...
        let p = o(json!({"customChunkSize": 60, "useWarpChunks": true}));
        assert_eq!(unknown_tweaks(None, &p), vec!["useWarpChunks=true".to_string()]);
        assert_eq!(
            unknown_tweaks(Some(&o(json!({"customChunkSize": 60}))), &p),
            vec!["useWarpChunks=true".to_string()]
        );
        // ... until it is adopted with that value; a changed value is named again.
        assert!(unknown_tweaks(Some(&p), &p).is_empty());
        let changed = o(json!({"customChunkSize": 60, "useWarpChunks": false}));
        assert_eq!(
            unknown_tweaks(Some(&p), &changed),
            vec!["useWarpChunks: true -> false".to_string()]
        );
        // A key that went away is not new.
        assert!(unknown_tweaks(Some(&p), &o(json!({"customChunkSize": 60}))).is_empty());
    }

    #[test]
    fn upstream_refusals_name_the_change_and_the_known_good_version() {
        // Every refusal `is_upstream_change` counts (the upstream stop).
        for (r, what) in [
            (Refusal::UnsupportedGeneration(14), "generation 14"),
            (Refusal::UnknownFeatures("teleport-v1".into()), "teleport-v1"),
            (Refusal::TweakMismatch("customChunkSize: 60 -> 100".into()), "60 -> 100"),
            (
                Refusal::UnknownTweaks("useWarpChunks=true".into()),
                "useWarpChunks=true",
            ),
            (
                Refusal::UnsupportedFeatures("nfc-path-identity-v1".into()),
                "nfc-path-identity-v1",
            ),
            (Refusal::IncompatibleChunkVersion(0, 2400), "0..2400"),
            (Refusal::UnsupportedSetting("encrypt=true".into()), "encrypt=true"),
            (Refusal::EncryptedContent("chunk h:1".into()), "chunk h:1"),
        ] {
            let msg = r.to_string();
            assert!(r.is_upstream_change(), "{msg}");
            assert!(msg.contains(what), "{msg}");
            assert!(
                msg.contains(&format!("Self-hosted LiveSync {TESTED_PLUGIN_VERSION} is the newest")),
                "{msg}"
            );
        }
        assert!(!Refusal::Locked.is_upstream_change());
        assert!(!Refusal::NotCouchDb.is_upstream_change());
    }

    #[test]
    fn a_lock_without_a_rebuild_names_who_holds_it() {
        let m = json!({"accepted_nodes": ["n1", "n2"], "node_info": {
            "n1": {"device_name": "laptop", "last_connected": 1_762_506_720_000_i64},
            "n2": {"device_name": "", "last_connected": 0}
        }});
        let holders = lock_holders(&m);
        let msg = Refusal::LockedWithoutRebuild {
            holders: holders.clone(),
        }
        .to_string();
        assert!(
            msg.contains("; locked by \"laptop\" (node n1), last connected 2025-11-07 09:12 UTC and node n2:"),
            "{msg}"
        );
        assert!(lock_holders(&json!({"accepted_nodes": []})).is_empty());
        assert!(msg.contains("not rebuilt") && msg.contains("never unlocks"), "{msg}");
        assert!(msg.contains("lsync fetch") && msg.contains("Fetch in the app"), "{msg}");
        assert!(!Refusal::LockedWithoutRebuild { holders: vec![] }.is_upstream_change());
    }

    #[test]
    fn a_reason_stays_while_only_a_holders_last_connection_moves() {
        let at = |t: i64| {
            lock_holders(&json!({"accepted_nodes": ["n1"], "node_info": {
                "n1": {"device_name": "laptop", "last_connected": t}}}))
        };
        let (a, b) = (
            Refusal::LockedWithoutRebuild { holders: at(1_000_000) },
            Refusal::LockedWithoutRebuild { holders: at(9_000_000) },
        );
        assert_ne!(a.to_string(), b.to_string(), "the message tells when");
        assert_eq!(a.reason(), b.reason(), "the reason does not");
        assert!(a.reason().contains("laptop"), "{}", a.reason());
        // Another holder is another reason.
        let c = Refusal::LockedWithoutRebuild {
            holders: lock_holders(&json!({"accepted_nodes": ["n9"]})),
        };
        assert_ne!(a.reason(), c.reason());
        // Other refusals: the message is the reason.
        let r = Refusal::UnknownFeatures("teleport-v1".into());
        assert_eq!(r.reason(), r.to_string());
    }

    #[test]
    fn times_are_written_as_utc_minutes() {
        assert_eq!(utc_minute(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_minute(951_782_400_000), "2000-02-29 00:00 UTC");
        assert_eq!(utc_minute(1_762_506_720_000), "2025-11-07 09:12 UTC");
        assert_eq!(utc_minute(4_102_444_799_000), "2099-12-31 23:59 UTC");
    }

    #[test]
    fn the_plugin_refusal_names_the_handover_only_where_there_is_one() {
        let r = |handover: Option<&str>| {
            Refusal::PluginEnabled {
                vault: "/v".into(),
                why: "listed".into(),
                handover: handover.map(str::to_string),
            }
            .to_string()
        };
        let cli = r(Some("run with --allow-plugin"));
        assert!(
            cli.contains("Disable the plugin") && cli.contains("deliberate handover, run with --allow-plugin"),
            "{cli}"
        );
        let app = r(None);
        assert!(app.contains("Disable the plugin"), "{app}");
        assert!(
            !app.contains("--allow-plugin") && !app.contains("allow_plugin") && !app.contains("lsync"),
            "{app}"
        );
    }

    #[test]
    fn an_unchecked_folder_is_named_and_the_plugin_is_not_blamed() {
        let r = |handover: Option<&str>| {
            Refusal::PluginUnchecked {
                vault: "/v".into(),
                folder: "/v/.snapshots".into(),
                why: "Permission denied".into(),
                handover: handover.map(str::to_string),
            }
            .to_string()
        };
        for m in [r(Some("run with --allow-plugin")), r(None)] {
            assert!(m.contains("/v/.snapshots could not be checked"), "{m}");
            assert!(
                !m.contains("Disable the plugin") && !m.contains("plugin enabled"),
                "{m}"
            );
        }
        assert!(r(Some("run with --allow-plugin")).contains("run with --allow-plugin"));
        assert!(!r(None).contains("allow"));
        // The vault folder itself cannot be listed.
        let m = Refusal::PluginUnchecked {
            vault: "/v".into(),
            folder: "/v".into(),
            why: "it cannot be listed: Permission denied".into(),
            handover: None,
        }
        .to_string();
        assert!(
            m.contains("/v could not be checked") && m.contains("make /v listable") && !m.contains("out of the vault"),
            "{m}"
        );
    }

    #[test]
    fn newest_remote_plugin_skips_lsync_and_this_node() {
        let m = json!({"node_info": {
            "a": {"device_name": "desk", "app_version": "1.12.7", "plugin_version": "1.0.9"},
            "b": {"device_name": "laptop", "app_version": "1.12.7", "plugin_version": "1.0.46"},
            "c": {"device_name": "phone", "app_version": "1.10.3", "plugin_version": "0.25.61"},
            "me": {"device_name": "me", "app_version": "1.12.7", "plugin_version": "9.9.9"},
            "l": {"device_name": "server", "app_version": "lsync", "plugin_version": "5.0.0"}
        }});
        assert_eq!(
            newest_remote_plugin(&m, "me").as_deref(),
            Some("Self-hosted LiveSync 1.0.46 on \"laptop\"")
        );
        assert_eq!(newest_remote_plugin(&json!({}), "me"), None);
    }
}
