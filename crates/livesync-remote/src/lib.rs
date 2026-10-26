//! CouchDB over HTTP for the LiveSync engine: server/database info, single
//! documents and `_local` documents, the filtered `_changes` feed (normal and
//! longpoll with heartbeat) and chunk fetches through `POST _all_docs`.
//!
//! M1 is read-only: this crate exposes no write methods at all.

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use url::Url;

pub mod backoff;
pub use backoff::Backoff;

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("invalid remote URL: {0}")]
    Url(String),
    #[error("HTTP {status} from {what}: {body}")]
    Http { status: u16, what: String, body: String },
    #[error("transport error during {what}: {source}")]
    Transport {
        what: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("unexpected response from {what}: {msg}")]
    Decode { what: String, msg: String },
}

impl RemoteError {
    /// Worth retrying with backoff (network trouble, server errors, rate limits).
    pub fn is_transient(&self) -> bool {
        match self {
            RemoteError::Transport { .. } => true,
            RemoteError::Http { status, .. } => *status >= 500 || *status == 429 || *status == 408,
            RemoteError::Decode { .. } => true, // truncated bodies on flaky links
            RemoteError::Url(_) => false,
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            RemoteError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, RemoteError>;

/// Result of a single-document GET.
#[derive(Debug, Clone, PartialEq)]
pub enum DocFetch {
    Found(Value),
    /// 404 with `reason: "deleted"`: the document exists as a tombstone.
    Deleted,
    /// Any other 404, with CouchDB's reason (or the start of the body).
    Missing(String),
}

/// Connection settings for one remote database.
#[derive(Clone)]
pub struct RemoteConfig {
    /// Server base URL, e.g. `https://couch.example.net` or `http://127.0.0.1:5984`.
    pub url: String,
    pub database: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for RemoteConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteConfig")
            .field("url", &self.url)
            .field("database", &self.database)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DbInfo {
    pub db_name: String,
    pub doc_count: u64,
    #[serde(default)]
    pub doc_del_count: u64,
    pub update_seq: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChangeRev {
    pub rev: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChangeRow {
    pub seq: Value,
    pub id: String,
    #[serde(default)]
    pub changes: Vec<ChangeRev>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub doc: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChangesResponse {
    pub results: Vec<ChangeRow>,
    pub last_seq: Value,
    #[serde(default)]
    pub pending: Option<u64>,
}

/// One `_changes` request.
#[derive(Debug, Clone)]
pub struct ChangesQuery {
    /// Sequence to start after; `None` means from the beginning.
    pub since: Option<Value>,
    pub limit: usize,
    /// Longpoll: wait for at least one change, keeping the connection alive
    /// with heartbeats.
    pub longpoll: bool,
    pub heartbeat_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AllDocsRow {
    pub key: Option<String>,
    #[serde(default)]
    pub doc: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AllDocsResponse {
    rows: Vec<AllDocsRow>,
}

/// The `_selector` used for the changes feed: every document except chunks,
/// plus deletion tombstones (whose bodies have no `type`).
pub fn changes_selector() -> Value {
    json!({"selector": {"$or": [{"type": {"$ne": "leaf"}}, {"_deleted": true}]}})
}

#[derive(Clone)]
pub struct Couch {
    http: Client,
    server: Url,
    db: Url,
    user: Option<String>,
    password: Option<String>,
    label: String,
}

impl std::fmt::Debug for Couch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Couch({})", self.label)
    }
}

impl Couch {
    pub fn new(cfg: &RemoteConfig) -> Result<Self> {
        let mut server = Url::parse(cfg.url.trim()).map_err(|e| RemoteError::Url(format!("{}: {e}", cfg.url)))?;
        match server.scheme() {
            "http" | "https" => {}
            s => {
                return Err(RemoteError::Url(format!(
                    "unsupported scheme {s:?}; use http:// or https://"
                )))
            }
        }
        // Credentials embedded in the URL are moved into basic auth.
        let mut user = cfg.username.clone().filter(|s| !s.is_empty());
        let mut password = cfg.password.clone();
        if !server.username().is_empty() {
            user.get_or_insert_with(|| decode(server.username()));
            if password.is_none() {
                password = server.password().map(decode);
            }
            let _ = server.set_username("");
            let _ = server.set_password(None);
        }
        if !server.path().ends_with('/') {
            let p = format!("{}/", server.path());
            server.set_path(&p);
        }
        let mut db = server.clone();
        db.path_segments_mut()
            .map_err(|_| RemoteError::Url("URL cannot be a base".into()))?
            .pop_if_empty()
            .push(&cfg.database);
        let http = Client::builder()
            .user_agent(concat!("lsync/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(20))
            // Longpoll sends a heartbeat newline every 30 s, so 90 s of silence
            // means the connection is dead. Large chunk responses keep flowing.
            .read_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| RemoteError::Transport {
                what: "client setup".into(),
                source: e,
            })?;
        let label = format!("{}{}", server, cfg.database);
        Ok(Couch {
            http,
            server,
            db,
            user,
            password,
            label,
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.user {
            Some(u) => rb.basic_auth(u, self.password.as_deref()),
            None => rb,
        }
    }

    fn doc_url(&self, segments: &[&str]) -> Url {
        let mut u = self.db.clone();
        {
            let mut s = u.path_segments_mut().expect("base url");
            for seg in segments {
                s.push(seg);
            }
        }
        u
    }

    async fn send(&self, what: &str, rb: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let resp = self.auth(rb).send().await.map_err(|e| RemoteError::Transport {
            what: what.into(),
            source: e,
        })?;
        Ok(resp)
    }

    async fn json<T: serde::de::DeserializeOwned>(what: &str, resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| RemoteError::Transport {
            what: what.into(),
            source: e,
        })?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes[..bytes.len().min(500)]).into_owned();
            return Err(RemoteError::Http {
                status: status.as_u16(),
                what: what.into(),
                body,
            });
        }
        serde_json::from_slice(&bytes).map_err(|e| RemoteError::Decode {
            what: what.into(),
            msg: e.to_string(),
        })
    }

    /// `GET /` on the server.
    pub async fn server_root(&self) -> Result<Value> {
        let r = self.send("server root", self.http.get(self.server.clone())).await?;
        Self::json("server root", r).await
    }

    /// `GET /{db}`.
    pub async fn db_info(&self) -> Result<DbInfo> {
        let r = self.send("database info", self.http.get(self.db.clone())).await?;
        Self::json("database info", r).await
    }

    /// `GET /{db}/{id}`, `None` on 404. Supports `_local/<name>` IDs.
    pub async fn get_doc(&self, id: &str, conflicts: bool) -> Result<Option<Value>> {
        Ok(match self.fetch_doc(id, conflicts).await? {
            DocFetch::Found(v) => Some(v),
            DocFetch::Deleted | DocFetch::Missing(_) => None,
        })
    }

    /// `GET /{db}/{id}`, telling a deleted document (404 with
    /// `reason: "deleted"`) apart from any other 404: a document that never
    /// existed, a database that is gone, or a proxy answering for something else.
    pub async fn fetch_doc(&self, id: &str, conflicts: bool) -> Result<DocFetch> {
        let mut url = match id.strip_prefix("_local/") {
            Some(name) => self.doc_url(&["_local", name]),
            None => self.doc_url(&[id]),
        };
        if conflicts {
            url.query_pairs_mut().append_pair("conflicts", "true");
        }
        let what = format!("GET {id}");
        let r = self.send(&what, self.http.get(url)).await?;
        if r.status() == StatusCode::NOT_FOUND {
            let bytes = r.bytes().await.map_err(|e| RemoteError::Transport {
                what: what.clone(),
                source: e,
            })?;
            let reason = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v.get("reason").and_then(Value::as_str).map(str::to_string));
            return Ok(match reason.as_deref() {
                Some("deleted") => DocFetch::Deleted,
                Some(other) => DocFetch::Missing(other.to_string()),
                None => DocFetch::Missing(String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).into_owned()),
            });
        }
        Self::json(&what, r).await.map(DocFetch::Found)
    }

    /// Filtered `_changes` with `include_docs` and `conflicts`.
    pub async fn changes(&self, q: &ChangesQuery) -> Result<ChangesResponse> {
        let mut url = self.doc_url(&["_changes"]);
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("filter", "_selector")
                .append_pair("include_docs", "true")
                .append_pair("conflicts", "true")
                .append_pair("limit", &q.limit.to_string());
            if let Some(since) = &q.since {
                let s = match since {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                qp.append_pair("since", &s);
            }
            if q.longpoll {
                qp.append_pair("feed", "longpoll")
                    .append_pair("heartbeat", &q.heartbeat_ms.to_string());
            }
        }
        let r = self
            .send("_changes", self.http.post(url).json(&changes_selector()))
            .await?;
        Self::json("_changes", r).await
    }

    /// `POST /{db}/_all_docs?include_docs=true` with `keys`.
    pub async fn all_docs_by_keys(&self, keys: &[String]) -> Result<Vec<AllDocsRow>> {
        let mut url = self.doc_url(&["_all_docs"]);
        url.query_pairs_mut().append_pair("include_docs", "true");
        let r = self
            .send("_all_docs", self.http.post(url).json(&json!({ "keys": keys })))
            .await?;
        let resp: AllDocsResponse = Self::json("_all_docs", r).await?;
        Ok(resp.rows)
    }
}

fn decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={s}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let c = Couch::new(&RemoteConfig {
            url: "http://u:p%40ss@127.0.0.1:8411".into(),
            database: "vault-copy".into(),
            username: None,
            password: None,
        })
        .unwrap();
        assert_eq!(c.user.as_deref(), Some("u"));
        assert_eq!(c.password.as_deref(), Some("p@ss"));
        assert_eq!(c.db.as_str(), "http://127.0.0.1:8411/vault-copy");
        assert_eq!(
            c.doc_url(&["_local", "obsydian_livesync_milestone"]).as_str(),
            "http://127.0.0.1:8411/vault-copy/_local/obsydian_livesync_milestone"
        );
        assert_eq!(
            c.doc_url(&["a/b c æ.md"]).as_str(),
            "http://127.0.0.1:8411/vault-copy/a%2Fb%20c%20%C3%A6.md"
        );
        let c = Couch::new(&RemoteConfig {
            url: "https://host/prefix".into(),
            database: "db".into(),
            username: Some("x".into()),
            password: Some("y".into()),
        })
        .unwrap();
        assert_eq!(c.db.as_str(), "https://host/prefix/db");
        assert!(Couch::new(&RemoteConfig {
            url: "ftp://x".into(),
            database: "d".into(),
            username: None,
            password: None
        })
        .is_err());
    }
}
