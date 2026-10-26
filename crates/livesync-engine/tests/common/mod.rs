//! A small in-process CouchDB stand-in: `/`, `/{db}`, documents with real
//! revision trees (deterministic winner, `_conflicts`, MVCC on `PUT`,
//! `_bulk_docs` with and without `new_edits`), `?rev=`, `_local` documents,
//! filtered `_changes` (normal and longpoll) and `_all_docs` (by keys, with
//! or without bodies, and by key range). Records every request so tests can
//! assert what the client wrote.

#![allow(dead_code)]

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use livesync_model::chunk::chunk_id_xxhash64;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

pub const DB: &str = "testdb";

/// One revision in a document's tree.
#[derive(Clone, Debug)]
pub struct RevNode {
    pub parent: Option<String>,
    /// `None` for ancestors known only by ID (`_revisions` imports).
    pub body: Option<Value>,
    pub deleted: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub revs: HashMap<String, RevNode>,
}

fn rev_key(rev: &str) -> (u64, String) {
    let (n, h) = rev.split_once('-').unwrap_or(("0", rev));
    (n.parse().unwrap_or(0), h.to_string())
}

impl Tree {
    pub fn leaves(&self) -> Vec<String> {
        let parents: HashSet<&String> = self.revs.values().filter_map(|n| n.parent.as_ref()).collect();
        let mut v: Vec<String> = self
            .revs
            .iter()
            .filter(|(r, n)| !parents.contains(r) && n.body.is_some())
            .map(|(r, _)| r.clone())
            .collect();
        v.sort_by_key(|r| rev_key(r));
        v
    }

    /// CouchDB's winner: live leaves first, then the highest position, then
    /// the highest revision hash.
    pub fn winner(&self) -> Option<String> {
        let leaves = self.leaves();
        let live: Vec<&String> = leaves.iter().filter(|r| !self.revs[*r].deleted).collect();
        let pool: Vec<&String> = if live.is_empty() { leaves.iter().collect() } else { live };
        pool.into_iter().max_by_key(|r| rev_key(r)).cloned()
    }

    pub fn conflicts(&self) -> Vec<String> {
        let w = self.winner();
        self.leaves()
            .into_iter()
            .filter(|r| Some(r) != w.as_ref() && !self.revs[r].deleted)
            .collect()
    }

    pub fn is_live_leaf(&self, rev: &str) -> bool {
        self.leaves().iter().any(|r| r == rev)
    }
}

#[derive(Default)]
pub struct MockState {
    pub root: Value,
    /// The winning revision's body per document (what reads see). Tests may
    /// edit this directly to simulate odd server states.
    pub docs: BTreeMap<String, Value>,
    pub trees: BTreeMap<String, Tree>,
    pub local: HashMap<String, Value>,
    /// (seq, id) for every change, in order.
    pub log: Vec<(u64, String)>,
    pub seq: u64,
    pub requests: Vec<(String, String)>,
    pub fail_changes: u32,
    /// Fail this many document writes (PUT and `_bulk_docs`) with a 503.
    pub fail_writes: u32,
    pub db_missing: bool,
}

#[derive(Clone)]
pub struct Mock {
    pub state: Arc<Mutex<MockState>>,
    pub notify: Arc<Notify>,
    pub url: String,
}

fn rand_hex() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    static C: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    let x = t ^ C.fetch_add(0x9e37_79b9_7f4a_7c15, std::sync::atomic::Ordering::Relaxed);
    format!("{:016x}{:016x}", x, x.rotate_left(17) ^ 0x5555)
}

pub fn preferred() -> Value {
    json!({
        "encrypt": false, "usePathObfuscation": false, "hashAlg": "xxhash64",
        "handleFilenameCaseSensitive": false, "customChunkSize": 60, "minimumChunkSize": 20,
        "chunkSplitterVersion": "v3-rabin-karp", "syncMaxSizeInMB": 50,
        "useDynamicIterationCount": false, "enableCompression": false
    })
}

pub fn milestone(created: i64) -> Value {
    json!({
        "_id": "_local/obsydian_livesync_milestone", "_rev": "0-1", "type": "milestoneinfo",
        "created": created, "locked": false, "accepted_nodes": ["desktop1"],
        "node_chunk_info": {"desktop1": {"min": 0, "max": 2400, "current": 2}},
        "tweak_values": {"PREFERRED": preferred(), "desktop1": preferred()}
    })
}

/// Outcome of one write against a tree.
enum WriteResult {
    Ok(String),
    Conflict,
}

impl MockState {
    fn refresh(&mut self, id: &str) {
        let Some(tree) = self.trees.get(id) else {
            self.docs.remove(id);
            return;
        };
        let Some(w) = tree.winner() else {
            self.docs.remove(id);
            return;
        };
        let node = &tree.revs[&w];
        let mut body = node.body.clone().unwrap_or(json!({}));
        body["_id"] = json!(id);
        body["_rev"] = json!(w);
        if node.deleted {
            body["_deleted"] = json!(true);
        }
        self.docs.insert(id.to_string(), body);
    }

    fn log_change(&mut self, id: &str) {
        self.seq += 1;
        let seq = self.seq;
        self.log.push((seq, id.to_string()));
    }

    /// A normal (`new_edits:true`) write: `_rev` must be a live leaf, or
    /// absent when the document does not exist or its winner is deleted.
    fn write(&mut self, mut doc: Value) -> WriteResult {
        let id = doc["_id"].as_str().unwrap().to_string();
        let given = doc.get("_rev").and_then(Value::as_str).map(str::to_string);
        let tree = self.trees.entry(id.clone()).or_default();
        let parent = match (&given, tree.winner()) {
            (None, None) => None,
            (None, Some(w)) if tree.revs[&w].deleted => Some(w),
            (None, Some(_)) => return WriteResult::Conflict,
            (Some(r), _) if tree.is_live_leaf(r) => Some(r.clone()),
            (Some(_), _) => return WriteResult::Conflict,
        };
        let pos = parent.as_deref().map(|p| rev_key(p).0).unwrap_or(0) + 1;
        let rev = format!("{pos}-{}", rand_hex());
        let deleted = doc.get("_deleted").and_then(Value::as_bool).unwrap_or(false);
        if let Some(o) = doc.as_object_mut() {
            o.remove("_rev");
            o.remove("_deleted");
            o.remove("_conflicts");
        }
        tree.revs.insert(
            rev.clone(),
            RevNode {
                parent,
                body: Some(doc),
                deleted,
            },
        );
        self.refresh(&id);
        self.log_change(&id);
        WriteResult::Ok(rev)
    }

    /// A `new_edits:false` import: the revision path from `_revisions`.
    fn import(&mut self, mut doc: Value) {
        let id = doc["_id"].as_str().unwrap().to_string();
        let rev = doc["_rev"].as_str().unwrap().to_string();
        let revs = doc.get("_revisions").cloned();
        let deleted = doc.get("_deleted").and_then(Value::as_bool).unwrap_or(false);
        if let Some(o) = doc.as_object_mut() {
            o.remove("_rev");
            o.remove("_revisions");
            o.remove("_deleted");
        }
        let tree = self.trees.entry(id.clone()).or_default();
        let (start, ids): (u64, Vec<String>) = match revs {
            Some(r) => (
                r["start"].as_u64().unwrap(),
                r["ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect(),
            ),
            None => (rev_key(&rev).0, vec![rev_key(&rev).1]),
        };
        // ids[0] is the newest; each next one is its parent.
        for (i, h) in ids.iter().enumerate() {
            let r = format!("{}-{h}", start - i as u64);
            let parent = ids.get(i + 1).map(|p| format!("{}-{p}", start - i as u64 - 1));
            let entry = tree.revs.entry(r.clone()).or_insert(RevNode {
                parent: parent.clone(),
                body: None,
                deleted: false,
            });
            if entry.parent.is_none() {
                entry.parent = parent;
            }
            if i == 0 {
                entry.body = Some(doc.clone());
                entry.deleted = deleted;
            }
        }
        self.refresh(&id);
        self.log_change(&id);
    }

    fn body_with(&self, id: &str, conflicts: bool) -> Option<Value> {
        let mut d = self.docs.get(id)?.clone();
        if conflicts {
            if let Some(t) = self.trees.get(id) {
                let c = t.conflicts();
                if !c.is_empty() && d.get("_deleted").is_none() {
                    d["_conflicts"] = json!(c);
                }
            }
        }
        Some(d)
    }
}

impl Mock {
    pub async fn start() -> Mock {
        let mut st = MockState {
            root: json!({"couchdb": "Welcome", "version": "3.4.2"}),
            ..Default::default()
        };
        st.local.insert("obsydian_livesync_milestone".into(), milestone(1000));
        st.local.insert(
            "obsidian_livesync_sync_parameters".into(),
            json!({"_id": "_local/obsidian_livesync_sync_parameters", "_rev": "0-1", "type": "sync-parameters", "protocolVersion": 2, "pbkdf2salt": "c2FsdA=="}),
        );
        let state = Arc::new(Mutex::new(st));
        let notify = Arc::new(Notify::new());
        let app = Router::new()
            .route("/", get(root))
            .route(&format!("/{DB}"), get(db_info))
            .route(&format!("/{DB}/_changes"), post(changes))
            .route(&format!("/{DB}/_all_docs"), post(all_docs).get(all_docs_range))
            .route(&format!("/{DB}/_bulk_docs"), post(bulk_docs))
            .route(&format!("/{DB}/_local/{{name}}"), get(get_local).put(put_local))
            .route(&format!("/{DB}/{{id}}"), get(get_doc).put(put_doc))
            .fallback(fallback)
            .with_state((state.clone(), notify.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let m = Mock {
            state,
            notify,
            url: format!("http://{addr}"),
        };
        m.put(json!({"_id": "obsydian_livesync_version", "type": "versioninfo", "version": 12}));
        m
    }

    /// Store a document as a new revision on top of the current winner (as
    /// another client would) and append it to the changes log.
    pub fn put(&self, mut doc: Value) -> String {
        let mut st = self.state.lock().unwrap();
        let id = doc["_id"].as_str().unwrap().to_string();
        if let Some(w) = st.trees.get(&id).and_then(Tree::winner) {
            doc["_rev"] = json!(w);
        } else if let Some(o) = doc.as_object_mut() {
            o.remove("_rev");
        }
        let rev = match st.write(doc) {
            WriteResult::Ok(r) => r,
            WriteResult::Conflict => unreachable!("put on the winner"),
        };
        drop(st);
        self.notify.notify_waiters();
        rev
    }

    /// Store a document as a child of a specific revision, creating a branch
    /// (what a second client's edit of the same base looks like).
    pub fn put_branch(&self, doc: Value, parent: &str) -> String {
        self.put_branch_with(doc, parent, &rand_hex())
    }

    /// `put_branch` with a chosen revision hash (to decide the winner: the
    /// higher hash wins between leaves at the same position).
    pub fn put_branch_with(&self, mut doc: Value, parent: &str, hash: &str) -> String {
        let (n, h) = parent.split_once('-').unwrap();
        let n: u64 = n.parse().unwrap();
        let r = hash.to_string();
        doc["_rev"] = json!(format!("{}-{r}", n + 1));
        doc["_revisions"] = json!({"start": n + 1, "ids": [r, h]});
        self.state.lock().unwrap().import(doc);
        self.notify.notify_waiters();
        format!("{}-{r}", n + 1)
    }

    /// Store chunks for `pieces` and return their IDs.
    pub fn put_chunks(&self, pieces: &[&str]) -> Vec<String> {
        pieces
            .iter()
            .map(|p| {
                let id = chunk_id_xxhash64(p);
                if !self.state.lock().unwrap().docs.contains_key(&id) {
                    self.put(json!({"_id": id, "type": "leaf", "data": p}));
                }
                id
            })
            .collect()
    }

    /// A text file split into the given pieces.
    pub fn put_text(&self, path: &str, pieces: &[&str]) -> String {
        let children = self.put_chunks(pieces);
        let size: usize = pieces.iter().map(|p| p.len()).sum();
        self.put(json!({
            "_id": livesync_model::path::path_to_id(path, true), "path": path, "type": "plain",
            "children": children, "ctime": 1_700_000_000_000i64, "mtime": 1_700_000_100_000i64,
            "size": size, "eden": {}
        }))
    }

    pub fn put_binary(&self, path: &str, bytes: &[u8], piece_len: usize) -> String {
        let pieces: Vec<String> = bytes
            .chunks(piece_len)
            .map(livesync_model::codec::base64_encode)
            .collect();
        let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
        let children = self.put_chunks(&refs);
        self.put(json!({
            "_id": livesync_model::path::path_to_id(path, true), "path": path, "type": "newnote",
            "children": children, "ctime": 1, "mtime": 1_700_000_200_000i64, "size": bytes.len(), "eden": {}
        }))
    }

    pub fn soft_delete(&self, path: &str) {
        let id = livesync_model::path::path_to_id(path, true);
        let mut doc = self.state.lock().unwrap().docs[&id].clone();
        doc["deleted"] = json!(true);
        doc["mtime"] = json!(1_800_000_000_000i64);
        self.put(doc);
    }

    pub fn soft_delete_id(&self, id: &str) {
        let mut doc = self.state.lock().unwrap().docs[id].clone();
        doc["deleted"] = json!(true);
        doc["mtime"] = json!(1_800_000_000_000i64);
        self.put(doc);
    }

    pub fn tombstone(&self, path: &str) {
        let id = livesync_model::path::path_to_id(path, true);
        self.put(json!({"_id": id, "_deleted": true}));
    }

    pub fn set_local(&self, name: &str, doc: Value) {
        self.state.lock().unwrap().local.insert(name.into(), doc);
    }

    pub fn local(&self, name: &str) -> Option<Value> {
        self.state.lock().unwrap().local.get(name).cloned()
    }

    pub fn update_milestone(&self, f: impl FnOnce(&mut Value)) {
        let mut st = self.state.lock().unwrap();
        let m = st.local.get_mut("obsydian_livesync_milestone").unwrap();
        f(m);
    }

    pub fn remove_doc(&self, id: &str) {
        let mut st = self.state.lock().unwrap();
        st.docs.remove(id);
        st.trees.remove(id);
        st.log.retain(|(_, i)| i != id);
    }

    /// Replace a document's body without a change-feed entry (only GETs see it).
    pub fn replace_silently(&self, id: &str, doc: Value) {
        self.state.lock().unwrap().docs.insert(id.into(), doc);
    }

    /// The current winner of a document (with `_conflicts`).
    pub fn doc(&self, id: &str) -> Option<Value> {
        self.state.lock().unwrap().body_with(id, true)
    }

    /// The winner's content, reassembled from its chunks.
    pub fn content(&self, id: &str) -> Option<Vec<u8>> {
        let st = self.state.lock().unwrap();
        let d = st.docs.get(id)?;
        if d.get("_deleted").is_some() || d.get("deleted") == Some(&json!(true)) {
            return None;
        }
        let mut out = Vec::new();
        for c in d["children"].as_array()? {
            let data = st.docs.get(c.as_str()?)?["data"].as_str()?.to_string();
            if d["type"] == "plain" {
                out.extend_from_slice(data.as_bytes());
            } else {
                out.extend(livesync_model::codec::base64_decode(&data).ok()?);
            }
        }
        Some(out)
    }

    /// The content of one specific revision.
    pub fn content_at(&self, id: &str, rev: &str) -> Option<Vec<u8>> {
        let st = self.state.lock().unwrap();
        let body = st.trees.get(id)?.revs.get(rev)?.body.clone()?;
        let mut out = Vec::new();
        for c in body["children"].as_array()? {
            let data = st.docs.get(c.as_str()?)?["data"].as_str()?.to_string();
            if body["type"] == "plain" {
                out.extend_from_slice(data.as_bytes());
            } else {
                out.extend(livesync_model::codec::base64_decode(&data).ok()?);
            }
        }
        Some(out)
    }

    /// Live (not soft-deleted) file documents by path.
    pub fn live_paths(&self) -> Vec<String> {
        let st = self.state.lock().unwrap();
        let mut v: Vec<String> = st
            .docs
            .values()
            .filter(|d| matches!(d["type"].as_str(), Some("plain" | "newnote")))
            .filter(|d| d.get("_deleted").is_none() && d.get("deleted") != Some(&json!(true)))
            .map(|d| d["path"].as_str().unwrap_or_default().to_string())
            .collect();
        v.sort();
        v
    }

    /// Number of `_changes` requests so far.
    pub fn changes_requests(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(_, p)| p.ends_with("/_changes"))
            .count()
    }

    /// Requests that were not reads.
    pub fn writes(&self) -> Vec<(String, String)> {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(m, p)| !(m == "GET" || (m == "POST" && (p.ends_with("/_changes") || p.ends_with("/_all_docs")))))
            .cloned()
            .collect()
    }

    pub fn clear_requests(&self) {
        self.state.lock().unwrap().requests.clear();
    }
}

type S = State<(Arc<Mutex<MockState>>, Arc<Notify>)>;

fn record(st: &Arc<Mutex<MockState>>, m: &str, p: &str) {
    st.lock().unwrap().requests.push((m.into(), p.into()));
}

fn not_found(reason: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "not_found", "reason": reason})),
    )
        .into_response()
}

async fn root(State((st, _)): S) -> Json<Value> {
    record(&st, "GET", "/");
    Json(st.lock().unwrap().root.clone())
}

async fn db_info(State((st, _)): S) -> Response {
    record(&st, "GET", "/db");
    let s = st.lock().unwrap();
    if s.db_missing {
        return not_found("Database does not exist.");
    }
    Json(json!({"db_name": DB, "doc_count": s.docs.len(), "doc_del_count": 0, "update_seq": format!("{}-x", s.seq)}))
        .into_response()
}

async fn get_doc(State((st, _)): S, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    record(&st, "GET", &format!("/db/{id}"));
    let s = st.lock().unwrap();
    if let Some(rev) = q.get("rev") {
        let Some(node) = s.trees.get(&id).and_then(|t| t.revs.get(rev)) else {
            return not_found("missing");
        };
        let Some(mut body) = node.body.clone() else {
            return not_found("missing");
        };
        body["_id"] = json!(id);
        body["_rev"] = json!(rev);
        if node.deleted {
            body["_deleted"] = json!(true);
        }
        return Json(body).into_response();
    }
    let conflicts = q.get("conflicts").map(String::as_str) == Some("true");
    match s.body_with(&id, conflicts) {
        Some(d) if d.get("_deleted") != Some(&json!(true)) => Json(d).into_response(),
        // CouchDB tells a deleted document apart from one that never existed
        // (or a database that is gone) by the `reason`.
        Some(_) => not_found("deleted"),
        None => not_found("missing"),
    }
}

async fn put_doc(State((st, notify)): S, Path(id): Path<String>, Json(mut body): Json<Value>) -> Response {
    record(&st, "PUT", &format!("/db/{id}"));
    let mut s = st.lock().unwrap();
    if s.fail_writes > 0 {
        s.fail_writes -= 1;
        return (StatusCode::SERVICE_UNAVAILABLE, "down").into_response();
    }
    body["_id"] = json!(id);
    let r = s.write(body);
    drop(s);
    match r {
        WriteResult::Ok(rev) => {
            notify.notify_waiters();
            (StatusCode::CREATED, Json(json!({"ok": true, "id": id, "rev": rev}))).into_response()
        }
        WriteResult::Conflict => (
            StatusCode::CONFLICT,
            Json(json!({"error": "conflict", "reason": "Document update conflict."})),
        )
            .into_response(),
    }
}

async fn bulk_docs(State((st, notify)): S, Json(body): Json<Value>) -> Response {
    record(&st, "POST", "/db/_bulk_docs");
    let new_edits = body.get("new_edits").and_then(Value::as_bool).unwrap_or(true);
    let mut s = st.lock().unwrap();
    if s.fail_writes > 0 {
        s.fail_writes -= 1;
        return (StatusCode::SERVICE_UNAVAILABLE, "down").into_response();
    }
    let mut out = Vec::new();
    for d in body["docs"].as_array().unwrap() {
        let id = d["_id"].as_str().unwrap().to_string();
        if new_edits {
            match s.write(d.clone()) {
                WriteResult::Ok(rev) => out.push(json!({"ok": true, "id": id, "rev": rev})),
                WriteResult::Conflict => {
                    out.push(json!({"id": id, "error": "conflict", "reason": "Document update conflict."}))
                }
            }
        } else {
            s.import(d.clone());
        }
    }
    drop(s);
    notify.notify_waiters();
    (StatusCode::CREATED, Json(Value::Array(out))).into_response()
}

async fn get_local(State((st, _)): S, Path(name): Path<String>) -> Response {
    record(&st, "GET", &format!("/db/_local/{name}"));
    match st.lock().unwrap().local.get(&name) {
        Some(d) => Json(d.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))).into_response(),
    }
}

async fn put_local(State((st, _)): S, Path(name): Path<String>, Json(mut body): Json<Value>) -> Response {
    record(&st, "PUT", &format!("/db/_local/{name}"));
    let mut s = st.lock().unwrap();
    let cur = s.local.get(&name).and_then(|d| d.get("_rev")).cloned();
    if body.get("_rev").cloned() != cur {
        return (StatusCode::CONFLICT, Json(json!({"error": "conflict"}))).into_response();
    }
    let n = cur
        .as_ref()
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("0-"))
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0);
    let rev = format!("0-{}", n + 1);
    body["_rev"] = json!(rev);
    body["_id"] = json!(format!("_local/{name}"));
    s.local.insert(name.clone(), body);
    (
        StatusCode::CREATED,
        Json(json!({"ok": true, "id": format!("_local/{name}"), "rev": rev})),
    )
        .into_response()
}

fn parse_since(s: Option<&String>) -> u64 {
    s.and_then(|s| s.split('-').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn selected(doc: &Value) -> bool {
    doc.get("_deleted") == Some(&json!(true)) || doc.get("type") != Some(&json!("leaf"))
}

async fn changes(State((st, notify)): S, Query(q): Query<HashMap<String, String>>, body: Bytes) -> Response {
    record(&st, "POST", "/db/_changes");
    let sel: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    assert!(sel.get("selector").is_some(), "changes must use a selector");
    {
        let mut s = st.lock().unwrap();
        if s.fail_changes > 0 {
            s.fail_changes -= 1;
            return (StatusCode::SERVICE_UNAVAILABLE, "down").into_response();
        }
        if s.db_missing {
            return not_found("Database does not exist.");
        }
    }
    let since = parse_since(q.get("since"));
    let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(usize::MAX);
    let longpoll = q.get("feed").map(String::as_str) == Some("longpoll");
    let conflicts = q.get("conflicts").map(String::as_str) == Some("true");
    for _ in 0..2 {
        let notified = notify.notified();
        let (results, last, pending) = {
            let s = st.lock().unwrap();
            // Latest change per doc only, like CouchDB.
            let mut latest: BTreeMap<String, u64> = BTreeMap::new();
            for (seq, id) in &s.log {
                if *seq > since {
                    latest.insert(id.clone(), *seq);
                }
            }
            let mut rows: Vec<(u64, String)> = latest.into_iter().map(|(id, seq)| (seq, id)).collect();
            rows.sort();
            let rows: Vec<(u64, String)> = rows
                .into_iter()
                .filter(|(_, id)| s.docs.get(id).is_some_and(selected))
                .collect();
            let total = rows.len();
            let page: Vec<_> = rows.into_iter().take(limit).collect();
            let last = page.last().map(|(seq, _)| *seq).unwrap_or(s.seq);
            let results: Vec<Value> = page
                .iter()
                .map(|(seq, id)| {
                    let d = s.body_with(id, conflicts).unwrap();
                    let deleted = d.get("_deleted") == Some(&json!(true));
                    json!({"seq": format!("{seq}-x"), "id": id, "changes": [{"rev": d["_rev"]}], "deleted": deleted, "doc": d})
                })
                .collect();
            (results, last, (total - page.len()) as u64)
        };
        if !results.is_empty() || !longpoll {
            return Json(json!({"results": results, "last_seq": format!("{last}-x"), "pending": pending}))
                .into_response();
        }
        let _ = tokio::time::timeout(Duration::from_secs(20), notified).await;
    }
    let s = st.lock().unwrap();
    Json(json!({"results": [], "last_seq": format!("{}-x", s.seq), "pending": 0})).into_response()
}

async fn all_docs(State((st, _)): S, Query(q): Query<HashMap<String, String>>, Json(body): Json<Value>) -> Json<Value> {
    record(&st, "POST", "/db/_all_docs");
    let include = q.get("include_docs").map(String::as_str) == Some("true");
    let conflicts = q.get("conflicts").map(String::as_str) == Some("true");
    let keys = body["keys"].as_array().unwrap();
    assert!(keys.len() <= 100, "at most 100 keys per request");
    let s = st.lock().unwrap();
    let rows: Vec<Value> = keys
        .iter()
        .map(|k| {
            let k = k.as_str().unwrap();
            match s.docs.get(k) {
                Some(d) if d.get("_deleted") == Some(&json!(true)) => {
                    json!({"id": k, "key": k, "value": {"rev": d["_rev"], "deleted": true}, "doc": null})
                }
                Some(d) if include => {
                    json!({"id": k, "key": k, "value": {"rev": d["_rev"]}, "doc": s.body_with(k, conflicts)})
                }
                Some(d) => json!({"id": k, "key": k, "value": {"rev": d["_rev"]}}),
                None => json!({"key": k, "error": "not_found"}),
            }
        })
        .collect();
    Json(json!({"rows": rows}))
}

async fn all_docs_range(State((st, _)): S, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    record(&st, "GET", "/db/_all_docs");
    let key = |k: &str| q.get(k).map(|v| serde_json::from_str::<String>(v).unwrap());
    let start = key("start_key");
    let end = key("end_key");
    let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(usize::MAX);
    let include = q.get("include_docs").map(String::as_str) == Some("true");
    let s = st.lock().unwrap();
    // Raw (code point) collation, as CouchDB uses for `_all_docs`.
    let rows: Vec<Value> = s
        .docs
        .iter()
        .filter(|(id, d)| d.get("_deleted").is_none() && !id.starts_with('_'))
        .filter(|(id, _)| start.as_deref().is_none_or(|st| id.as_str() >= st))
        .filter(|(id, _)| end.as_deref().is_none_or(|e| id.as_str() <= e))
        .take(limit)
        .map(|(id, d)| {
            if include {
                json!({"id": id, "key": id, "value": {"rev": d["_rev"]}, "doc": s.body_with(id, true)})
            } else {
                json!({"id": id, "key": id, "value": {"rev": d["_rev"]}})
            }
        })
        .collect();
    Json(json!({"rows": rows}))
}

async fn fallback(State((st, _)): S, method: Method, uri: Uri) -> Response {
    record(&st, method.as_str(), uri.path());
    (StatusCode::METHOD_NOT_ALLOWED, "not supported by mock").into_response()
}
