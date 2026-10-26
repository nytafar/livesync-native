//! A small in-process CouchDB stand-in: just enough of `/`, `/{db}`, document
//! GETs, `_local` docs, filtered `_changes` (normal and longpoll) and
//! `POST _all_docs {keys}` for the engine. Records every request so tests can
//! assert the client never writes.

#![allow(dead_code)]

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use livesync_model::chunk::chunk_id_xxhash64;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

pub const DB: &str = "testdb";

#[derive(Default)]
pub struct MockState {
    pub root: Value,
    pub docs: BTreeMap<String, Value>,
    pub local: HashMap<String, Value>,
    /// (seq, id) for every change, in order.
    pub log: Vec<(u64, String)>,
    pub seq: u64,
    pub requests: Vec<(String, String)>,
    pub fail_changes: u32,
}

#[derive(Clone)]
pub struct Mock {
    pub state: Arc<Mutex<MockState>>,
    pub notify: Arc<Notify>,
    pub url: String,
}

fn rev_bump(old: Option<&Value>) -> String {
    let n = old
        .and_then(|d| d.get("_rev"))
        .and_then(Value::as_str)
        .and_then(|r| r.split('-').next())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0);
    format!("{}-{:032x}", n + 1, rand_u64())
}

fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static C: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    t ^ C.fetch_add(0x9e37_79b9_7f4a_7c15, std::sync::atomic::Ordering::Relaxed)
}

pub fn preferred() -> Value {
    json!({
        "encrypt": false, "usePathObfuscation": false, "hashAlg": "xxhash64",
        "handleFilenameCaseSensitive": false, "customChunkSize": 60,
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
            .route(&format!("/{DB}/_all_docs"), post(all_docs))
            .route(&format!("/{DB}/_local/{{name}}"), get(get_local))
            .route(&format!("/{DB}/{{id}}"), get(get_doc))
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

    /// Store a document (new revision) and append it to the changes log.
    pub fn put(&self, mut doc: Value) -> String {
        let mut st = self.state.lock().unwrap();
        let id = doc["_id"].as_str().unwrap().to_string();
        let rev = rev_bump(st.docs.get(&id));
        doc["_rev"] = json!(rev);
        st.seq += 1;
        let seq = st.seq;
        st.log.push((seq, id.clone()));
        st.docs.insert(id, doc);
        drop(st);
        self.notify.notify_waiters();
        rev
    }

    /// Store chunks for `pieces` and return their IDs.
    pub fn put_chunks(&self, pieces: &[&str]) -> Vec<String> {
        pieces
            .iter()
            .map(|p| {
                let id = chunk_id_xxhash64(p);
                self.put(json!({"_id": id, "type": "leaf", "data": p}));
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

    pub fn tombstone(&self, path: &str) {
        let id = livesync_model::path::path_to_id(path, true);
        self.put(json!({"_id": id, "_deleted": true}));
    }

    pub fn set_local(&self, name: &str, doc: Value) {
        self.state.lock().unwrap().local.insert(name.into(), doc);
    }

    pub fn update_milestone(&self, f: impl FnOnce(&mut Value)) {
        let mut st = self.state.lock().unwrap();
        let m = st.local.get_mut("obsydian_livesync_milestone").unwrap();
        f(m);
    }

    pub fn remove_doc(&self, id: &str) {
        let mut st = self.state.lock().unwrap();
        st.docs.remove(id);
        st.log.retain(|(_, i)| i != id);
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
}

type S = State<(Arc<Mutex<MockState>>, Arc<Notify>)>;

fn record(st: &Arc<Mutex<MockState>>, m: &str, p: &str) {
    st.lock().unwrap().requests.push((m.into(), p.into()));
}

async fn root(State((st, _)): S) -> Json<Value> {
    record(&st, "GET", "/");
    Json(st.lock().unwrap().root.clone())
}

async fn db_info(State((st, _)): S) -> Json<Value> {
    record(&st, "GET", "/db");
    let s = st.lock().unwrap();
    Json(json!({"db_name": DB, "doc_count": s.docs.len(), "doc_del_count": 0, "update_seq": format!("{}-x", s.seq)}))
}

async fn get_doc(State((st, _)): S, Path(id): Path<String>) -> Response {
    record(&st, "GET", &format!("/db/{id}"));
    match st.lock().unwrap().docs.get(&id) {
        Some(d) if d.get("_deleted") != Some(&json!(true)) => Json(d.clone()).into_response(),
        _ => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))).into_response(),
    }
}

async fn get_local(State((st, _)): S, Path(name): Path<String>) -> Response {
    record(&st, "GET", &format!("/db/_local/{name}"));
    match st.lock().unwrap().local.get(&name) {
        Some(d) => Json(d.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))).into_response(),
    }
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
    }
    let since = parse_since(q.get("since"));
    let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(usize::MAX);
    let longpoll = q.get("feed").map(String::as_str) == Some("longpoll");
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
            let rows: Vec<(u64, String)> = rows.into_iter().filter(|(_, id)| selected(&s.docs[id])).collect();
            let total = rows.len();
            let page: Vec<_> = rows.into_iter().take(limit).collect();
            let last = page.last().map(|(seq, _)| *seq).unwrap_or(s.seq);
            let results: Vec<Value> = page
                .iter()
                .map(|(seq, id)| {
                    let d = &s.docs[id];
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
    assert_eq!(q.get("include_docs").map(String::as_str), Some("true"));
    let keys = body["keys"].as_array().unwrap();
    assert!(keys.len() <= 100, "at most 100 keys per request");
    let s = st.lock().unwrap();
    let rows: Vec<Value> = keys
        .iter()
        .map(|k| {
            let k = k.as_str().unwrap();
            match s.docs.get(k) {
                Some(d) => json!({"id": k, "key": k, "value": {"rev": d["_rev"]}, "doc": d}),
                None => json!({"key": k, "error": "not_found"}),
            }
        })
        .collect();
    Json(json!({"rows": rows}))
}

async fn fallback(State((st, _)): S, method: Method, uri: Uri) -> Response {
    record(&st, method.as_str(), uri.path());
    (StatusCode::METHOD_NOT_ALLOWED, "not supported by mock").into_response()
}
