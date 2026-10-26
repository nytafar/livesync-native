//! Node registration in the remote milestone, as upstream clients do it.
//!
//! Upstream: `pouchdb/LiveSyncDBFunctions.ts` (`ensureRemoteIsCompatible`,
//! the write half) and `replication/couchdb/LiveSyncReplicator.ts`
//! (`markRemoteResolved`). Pure: takes the current milestone body and returns
//! the body to write back, if anything changed.

use serde_json::{json, Map, Value};

/// How often `last_connected` is refreshed (upstream: more than 60 s apart).
pub const LAST_CONNECTED_MIN_GAP_MS: i64 = 60_000;

/// What this device advertises in `node_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device_name: String,
    pub vault_name: String,
    pub app_version: String,
    pub plugin_version: String,
    pub platform: Option<String>,
}

/// The chunk-format range this client advertises (`currentVersionRange`).
pub fn our_chunk_info() -> Value {
    json!({"min": crate::gate::OUR_CHUNK_RANGE.0, "max": crate::gate::OUR_CHUNK_RANGE.1, "current": 2})
}

fn obj<'a>(m: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    if !m.get(key).is_some_and(Value::is_object) {
        m.insert(key.to_string(), Value::Object(Map::new()));
    }
    m.get_mut(key).and_then(Value::as_object_mut).expect("object")
}

/// Register `node` in `milestone`: its `node_chunk_info`, `tweak_values` and
/// `node_info` (with `last_connected` refreshed at most once a minute).
/// With `mark_resolved`, also add it to `accepted_nodes` (what upstream's
/// `markRemoteResolved` does after a fetch). Returns the new body when a
/// write is needed, `None` when the milestone is already up to date.
pub fn register_node(
    milestone: &Value,
    node: &str,
    device: &DeviceInfo,
    tweaks: &Map<String, Value>,
    now_ms: i64,
    mark_resolved: bool,
) -> Option<Value> {
    let mut m = milestone.as_object()?.clone();
    let mut write = false;
    let ours = our_chunk_info();

    // node_chunk_info: the remote's entry wins in the merge, then ours is enforced.
    let nci = obj(&mut m, "node_chunk_info");
    let entry = nci.entry(node.to_string()).or_insert_with(|| ours.clone());
    if entry.get("min") != ours.get("min") || entry.get("max") != ours.get("max") {
        write = true;
    }
    let tweaks_differ = m
        .get("tweak_values")
        .and_then(|t| t.get(node))
        .is_none_or(|t| t != &Value::Object(tweaks.clone()));
    let preferred_missing = m
        .get("tweak_values")
        .and_then(|t| t.get(crate::gate::PREFERRED))
        .is_none();
    write |= tweaks_differ || preferred_missing;

    let infos = obj(&mut m, "node_info");
    if !infos.contains_key(node) {
        infos.insert(
            node.to_string(),
            json!({
                "device_name": device.device_name, "vault_name": device.vault_name,
                "app_version": device.app_version, "plugin_version": device.plugin_version,
                "progress": "", "last_connected": 0
            }),
        );
        write = true;
    }
    let info = infos.get_mut(node).and_then(Value::as_object_mut).expect("object");
    for (k, v) in [
        ("device_name", &device.device_name),
        ("app_version", &device.app_version),
        ("plugin_version", &device.plugin_version),
        ("vault_name", &device.vault_name),
    ] {
        if info.get(k).and_then(Value::as_str) != Some(v.as_str()) {
            info.insert(k.into(), Value::from(v.as_str()));
            write = true;
        }
    }
    if info.get("progress").and_then(Value::as_str) != Some("") {
        info.insert("progress".into(), Value::from(""));
        write = true;
    }
    if let Some(p) = &device.platform {
        if info.get("platform").and_then(Value::as_str) != Some(p.as_str()) {
            info.insert("platform".into(), Value::from(p.as_str()));
            write = true;
        }
    }
    let last = info.get("last_connected").and_then(Value::as_i64).unwrap_or(0);
    if now_ms - last > LAST_CONNECTED_MIN_GAP_MS {
        info.insert("last_connected".into(), Value::from(now_ms));
        write = true;
    }

    if mark_resolved {
        let accepted = m.entry("accepted_nodes").or_insert_with(|| Value::Array(vec![]));
        if let Some(a) = accepted.as_array_mut() {
            if !a.iter().any(|v| v.as_str() == Some(node)) {
                a.push(Value::from(node));
                write = true;
            }
        }
    }

    if !write {
        return None;
    }
    let nci = obj(&mut m, "node_chunk_info");
    let e = nci.get_mut(node).and_then(Value::as_object_mut).expect("object");
    e.insert("min".into(), ours["min"].clone());
    e.insert("max".into(), ours["max"].clone());
    let tv = obj(&mut m, "tweak_values");
    tv.insert(node.to_string(), Value::Object(tweaks.clone()));
    if !tv.contains_key(crate::gate::PREFERRED) {
        tv.insert(crate::gate::PREFERRED.into(), Value::Object(tweaks.clone()));
    }
    Some(Value::Object(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> DeviceInfo {
        DeviceInfo {
            device_name: "phone".into(),
            vault_name: "hvelv".into(),
            app_version: "lsync".into(),
            plugin_version: "0.1.0".into(),
            platform: Some("android".into()),
        }
    }

    fn base() -> Value {
        json!({
            "_id": "_local/obsydian_livesync_milestone", "_rev": "0-3", "type": "milestoneinfo",
            "created": 5, "locked": false, "accepted_nodes": ["desk"],
            "node_chunk_info": {"desk": {"min": 0, "max": 2400, "current": 2}},
            "node_info": {"desk": {"device_name": "d", "last_connected": 1}},
            "tweak_values": {"PREFERRED": {"customChunkSize": 60}, "desk": {"customChunkSize": 60}}
        })
    }

    #[test]
    fn registers_once_then_refreshes_only_after_a_minute() {
        let t = json!({"customChunkSize": 60});
        let t = t.as_object().unwrap();
        let new = register_node(&base(), "me", &device(), t, 1_000_000, false).unwrap();
        assert_eq!(new["_rev"], "0-3", "keeps the revision for the PUT");
        assert_eq!(new["node_chunk_info"]["me"], our_chunk_info());
        assert_eq!(new["tweak_values"]["me"], json!({"customChunkSize": 60}));
        assert_eq!(new["node_info"]["me"]["last_connected"], 1_000_000);
        assert_eq!(new["node_info"]["me"]["platform"], "android");
        assert_eq!(new["accepted_nodes"], json!(["desk"]), "not resolved");
        assert_eq!(new["node_info"]["desk"], base()["node_info"]["desk"]);
        // Nothing to do within the minute.
        assert!(register_node(&new, "me", &device(), t, 1_030_000, false).is_none());
        // last_connected refresh after a minute.
        let later = register_node(&new, "me", &device(), t, 1_070_000, false).unwrap();
        assert_eq!(later["node_info"]["me"]["last_connected"], 1_070_000);
        // Resolving adds the node once.
        let res = register_node(&later, "me", &device(), t, 1_070_001, true).unwrap();
        assert_eq!(res["accepted_nodes"], json!(["desk", "me"]));
        assert!(register_node(&res, "me", &device(), t, 1_070_002, true).is_none());
    }

    #[test]
    fn changed_tweaks_or_device_name_trigger_a_write() {
        let t = json!({"customChunkSize": 60});
        let t = t.as_object().unwrap();
        let reg = register_node(&base(), "me", &device(), t, 1_000_000, false).unwrap();
        let t2 = json!({"customChunkSize": 61});
        assert!(register_node(&reg, "me", &device(), t2.as_object().unwrap(), 1_000_001, false).is_some());
        let mut d = device();
        d.device_name = "phone2".into();
        let r = register_node(&reg, "me", &d, t, 1_000_001, false).unwrap();
        assert_eq!(r["node_info"]["me"]["device_name"], "phone2");
    }
}
