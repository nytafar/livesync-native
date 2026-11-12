//! CouchDB over HTTP for the LiveSync engine: server/database info, single
//! documents and `_local` documents, the filtered `_changes` feed (normal and
//! longpoll with heartbeat), chunk fetches through `POST _all_docs`, and the
//! writes two-way sync needs: `PUT` with an explicit `_rev`, `_bulk_docs`
//! (normal and `new_edits:false`) and `_local` document updates.
//!
//! End-to-end encryption is a transform at this boundary, as upstream's
//! transform-pouch does it. A `Couch` starts not knowing whether its database
//! is encrypted ([`Encryption::Undetermined`]): until the caller settles it
//! ([`Couch::set_encryption`], after checking the database), every document
//! write and every read that would need the transform is refused with
//! [`RemoteError::EncryptionUndetermined`]; only `_local/` documents and the
//! explicitly raw reads go through. With a cipher set, every
//! document body read is passed through [`DocumentCipher::outgoing`] before
//! the caller sees it, and every document written through
//! [`DocumentCipher::incoming`], so the engine works on plaintext documents
//! only. `_local/` documents and tombstones pass untouched. A document the
//! transform refuses is never handed on as if it were valid: a row carries it
//! as [`Undecryptable`] (with no body), a single read fails with
//! [`RemoteError::Crypto`]. Writes are guarded both ways: nothing with
//! plaintext content or a plain `h:` chunk ID goes into an encrypted database,
//! and nothing encrypted into a plaintext one ([`RemoteError::Guard`]).

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use url::Url;

pub use livesync_crypto::{CryptoError, DocumentCipher};

pub mod backoff;
pub use backoff::Backoff;
/// The HTTP client a [`Couch`] uses; one per server lets vaults share its connection pool.
pub use reqwest::Client as HttpClient;

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
    /// The server sent nothing for this long on a request that should have
    /// kept talking (the longpoll's heartbeats): the connection is taken for dead.
    #[error("no data during {what} for {after:?}; the connection is taken for dead")]
    Silent { what: String, after: Duration },
    /// A document read through the end-to-end encryption transform could
    /// not be decrypted, or was rejected by the unencrypted-content policy.
    #[error("{0}")]
    Crypto(Box<Undecryptable>),
    /// A write refused before it was sent: it would have put plaintext into
    /// an encrypted database, or encrypted content into a plaintext one.
    #[error("refused to write {0}")]
    Guard(String),
    /// A document write, or a read that needs the encryption transform,
    /// while whether the database is end-to-end encrypted is not settled
    /// ([`Encryption::Undetermined`]): nothing is sent.
    #[error("refused {0}: whether the remote database is end-to-end encrypted is not settled (the connect gate has not passed)")]
    EncryptionUndetermined(String),
}

/// What a [`Couch`] knows of its database's end-to-end encryption.
#[derive(Clone, Default)]
pub enum Encryption {
    /// Not settled yet, or no longer: document writes and reads that would
    /// need the transform are refused.
    #[default]
    Undetermined,
    /// A plaintext database: nothing encrypted goes in.
    Plaintext,
    /// An encrypted database, read and written with this transform.
    Encrypted(Arc<DocumentCipher>),
}

impl std::fmt::Debug for Encryption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Encryption::Undetermined => "Undetermined",
            Encryption::Plaintext => "Plaintext",
            Encryption::Encrypted(_) => "Encrypted",
        })
    }
}

/// A remote document the end-to-end encryption transform refused, with what
/// is known of it without decrypting (its metadata is not encrypted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undecryptable {
    pub id: String,
    pub rev: Option<String>,
    /// The file path, for a file document.
    pub path: Option<String>,
    pub error: CryptoError,
}

impl std::fmt::Display for Undecryptable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.error {
            // Its own message names the document.
            CryptoError::Rejected { .. } => write!(f, "{}", self.error),
            e => write!(f, "remote document {}: {e}", self.id),
        }
    }
}

impl RemoteError {
    /// Worth retrying with backoff (network trouble, server errors, rate limits).
    pub fn is_transient(&self) -> bool {
        match self {
            RemoteError::Transport { .. } | RemoteError::Silent { .. } => true,
            RemoteError::Http { status, .. } => *status >= 500 || *status == 429 || *status == 408,
            RemoteError::Decode { .. } => true, // truncated bodies on flaky links
            RemoteError::Url(_)
            | RemoteError::Crypto(_)
            | RemoteError::Guard(_)
            | RemoteError::EncryptionUndetermined(_) => false,
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
    /// The document the encryption transform refused (`doc` is then `None`).
    #[serde(skip)]
    pub undecryptable: Option<Box<Undecryptable>>,
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
    /// The longest the response may stay silent (no headers, no body bytes)
    /// before the connection is taken for dead ([`RemoteError::Silent`]).
    /// With longpoll, the heartbeats are what keeps it talking, so this
    /// should be a few heartbeats long. `None`: the client's read timeout.
    pub idle_timeout: Option<Duration>,
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
    /// The document the encryption transform refused (`doc` is then `None`).
    #[serde(skip)]
    pub undecryptable: Option<Box<Undecryptable>>,
}

#[derive(Debug, Deserialize)]
struct AllDocsResponse {
    rows: Vec<AllDocsRow>,
}

/// Result of a `PUT` with an explicit revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    Saved {
        rev: String,
    },
    /// 409: the given `_rev` is not the current leaf (or a document exists
    /// and no `_rev` was given).
    Conflict,
}

/// One row of a `_bulk_docs` response.
#[derive(Debug, Clone, Deserialize)]
pub struct BulkResult {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub rev: Option<String>,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// The `_selector` used for the changes feed: every document except chunks,
/// plus deletion tombstones (whose bodies have no `type`).
pub fn changes_selector() -> Value {
    json!({"selector": {"$or": [{"type": {"$ne": "leaf"}}, {"_deleted": true}]}})
}

/// An HTTP client that several [`Couch`] clients share, and that
/// [`Couch::reset_connections`] replaces for all of them at once.
pub type SharedClient = Arc<RwLock<Client>>;

/// A [`SharedClient`] from [`http_client`].
pub fn shared_http_client() -> Result<SharedClient> {
    Ok(Arc::new(RwLock::new(http_client()?)))
}

/// How long any response may stay silent before the client gives up on it
/// (a transport error). The longpoll's silence limit
/// ([`ChangesQuery::idle_timeout`]) and its heartbeat must stay below it, or
/// this fires first and the dead connection is not recognised as such.
pub const READ_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone)]
pub struct Couch {
    /// Shared by this client's clones (and by every client made with
    /// [`Couch::with_shared_client`] from the same one), so
    /// [`Couch::reset_connections`] replaces the pool for all of them.
    http: SharedClient,
    server: Url,
    db: Url,
    user: Option<String>,
    password: Option<String>,
    label: String,
    /// Response body bytes read by this client and its clones.
    rx_bytes: Arc<AtomicU64>,
    /// The database's end-to-end encryption as far as it is settled, shared
    /// by this client's clones.
    encryption: Arc<RwLock<Encryption>>,
}

impl std::fmt::Debug for Couch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Couch({})", self.label)
    }
}

/// An HTTP client with the timeouts the engine expects. Profiles that talk to
/// the same host may share one (its connection pool) through
/// [`Couch::with_client`]; nothing here is process-wide.
pub fn http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("lsync/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        // The longpoll has a shorter silence limit of its own
        // (`ChangesQuery::idle_timeout`, kept talking by heartbeats). The
        // other requests have no heartbeat, and a busy server may think for
        // a while before answering a large `_bulk_docs` or `_all_docs`.
        .read_timeout(READ_TIMEOUT)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| RemoteError::Transport {
            what: "client setup".into(),
            source: e,
        })
}

impl Couch {
    pub fn new(cfg: &RemoteConfig) -> Result<Self> {
        Self::with_client(cfg, http_client()?)
    }

    /// Like [`Couch::new`], reusing an existing client and its connection pool.
    pub fn with_client(cfg: &RemoteConfig, http: Client) -> Result<Self> {
        Self::with_shared_client(cfg, Arc::new(RwLock::new(http)))
    }

    /// Like [`Couch::new`], on a client shared with other databases on the
    /// same server: a reset by any of them gives all of them new connections.
    pub fn with_shared_client(cfg: &RemoteConfig, http: SharedClient) -> Result<Self> {
        // Never the URL itself in the message: it may hold a password.
        let mut server =
            Url::parse(cfg.url.trim()).map_err(|e| RemoteError::Url(format!("the server URL is not valid: {e}")))?;
        match server.scheme() {
            "http" | "https" => {}
            _ => {
                return Err(RemoteError::Url(
                    "the server URL must start with http:// or https://".into(),
                ))
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
        let label = format!("{}{}", server, cfg.database);
        Ok(Couch {
            http,
            server,
            db,
            user,
            password,
            label,
            rx_bytes: Arc::new(AtomicU64::new(0)),
            encryption: Arc::new(RwLock::new(Encryption::Undetermined)),
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Settle (or unsettle, with [`Encryption::Undetermined`]) the
    /// database's encryption, for this client and its clones. Takes effect
    /// for the next request: a write already sent is not called back.
    pub fn set_encryption(&self, e: Encryption) {
        *self.encryption.write().unwrap_or_else(|e| e.into_inner()) = e;
    }

    /// The database's encryption as far as it is settled.
    pub fn encryption(&self) -> Encryption {
        self.encryption.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The transform in use: `Some` only for a database settled as encrypted.
    pub fn cipher(&self) -> Option<Arc<DocumentCipher>> {
        match self.encryption() {
            Encryption::Encrypted(c) => Some(c),
            Encryption::Undetermined | Encryption::Plaintext => None,
        }
    }

    /// Fails while the encryption is not settled, for anything but `_local/`.
    fn settled(&self, id: Option<&str>, what: impl FnOnce() -> String) -> Result<Encryption> {
        match self.encryption() {
            Encryption::Undetermined if !id.is_some_and(|i| i.starts_with("_local/")) => {
                Err(RemoteError::EncryptionUndetermined(what()))
            }
            e => Ok(e),
        }
    }

    /// A document as it is to be written, through the transform and the
    /// write guard.
    pub fn encrypt(&self, doc: &Value) -> Result<Value> {
        encrypt_with(&self.encryption(), doc)
    }

    fn decrypt_rows<T>(
        &self,
        e: &Encryption,
        rows: &mut [T],
        parts: impl Fn(&mut T) -> (&mut Option<Value>, &mut Option<Box<Undecryptable>>),
    ) {
        let Encryption::Encrypted(c) = e else { return };
        for r in rows {
            let (doc, failed) = parts(r);
            if let Some(d) = doc.take() {
                match decrypt_with(c, d) {
                    Ok(d) => *doc = Some(d),
                    Err(u) => *failed = Some(u),
                }
            }
        }
    }

    /// Response body bytes read so far by this client and its clones (the
    /// decoded JSON responses; 404 bodies of single-document reads excluded).
    pub fn bytes_received(&self) -> u64 {
        self.rx_bytes.load(Ordering::Relaxed)
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

    /// The client all requests go through (clones share its pool).
    fn http(&self) -> Client {
        self.http.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Drop every pooled connection, for this client and its clones: later
    /// requests open new ones. After the machine slept, or a connection was
    /// found dead, the others in the pool are likely dead too, and a request
    /// on one would hang until the read timeout. Requests in flight keep
    /// their connection. A client given to [`Couch::with_client`] or
    /// [`Couch::with_shared_client`] is replaced by one from [`http_client`],
    /// for every `Couch` that shares it.
    pub fn reset_connections(&self) {
        match http_client() {
            Ok(c) => *self.http.write().unwrap_or_else(|e| e.into_inner()) = c,
            Err(e) => tracing::warn!("cannot set up a new HTTP client ({e}); keeping the old connections"),
        }
    }

    /// `send`, waiting at most `idle` for the response headers.
    async fn send_within(
        &self,
        what: &str,
        rb: reqwest::RequestBuilder,
        idle: Option<Duration>,
    ) -> Result<reqwest::Response> {
        match idle {
            None => self.send(what, rb).await,
            Some(after) => tokio::time::timeout(after, self.send(what, rb))
                .await
                .map_err(|_| RemoteError::Silent {
                    what: what.into(),
                    after,
                })?,
        }
    }

    /// The whole body, failing when no bytes arrive for `idle`.
    async fn body_within(what: &str, mut resp: reqwest::Response, after: Duration) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(after, resp.chunk()).await {
                Err(_) => {
                    return Err(RemoteError::Silent {
                        what: what.into(),
                        after,
                    })
                }
                Ok(Err(e)) => {
                    return Err(RemoteError::Transport {
                        what: what.into(),
                        source: e,
                    })
                }
                Ok(Ok(None)) => return Ok(out),
                Ok(Ok(Some(b))) => out.extend_from_slice(&b),
            }
        }
    }

    async fn json<T: serde::de::DeserializeOwned>(&self, what: &str, resp: reqwest::Response) -> Result<T> {
        self.json_within(what, resp, None).await
    }

    async fn json_within<T: serde::de::DeserializeOwned>(
        &self,
        what: &str,
        resp: reqwest::Response,
        idle: Option<Duration>,
    ) -> Result<T> {
        let status = resp.status();
        let bytes = match idle {
            None => resp
                .bytes()
                .await
                .map_err(|e| RemoteError::Transport {
                    what: what.into(),
                    source: e,
                })?
                .to_vec(),
            Some(after) => Self::body_within(what, resp, after).await?,
        };
        self.rx_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
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
        let r = self.send("server root", self.http().get(self.server.clone())).await?;
        self.json("server root", r).await
    }

    /// `GET /{db}`.
    pub async fn db_info(&self) -> Result<DbInfo> {
        let r = self.send("database info", self.http().get(self.db.clone())).await?;
        self.json("database info", r).await
    }

    /// `GET /{db}/{id}`, `None` on 404. Supports `_local/<name>` IDs.
    pub async fn get_doc(&self, id: &str, conflicts: bool) -> Result<Option<Value>> {
        Ok(match self.fetch_doc(id, conflicts).await? {
            DocFetch::Found(v) => Some(v),
            DocFetch::Deleted | DocFetch::Missing(_) => None,
        })
    }

    fn id_url(&self, id: &str) -> Url {
        match id.strip_prefix("_local/") {
            Some(name) => self.doc_url(&["_local", name]),
            None => self.doc_url(&[id]),
        }
    }

    /// `GET /{db}/{id}?rev=…` for one specific revision.
    pub async fn fetch_rev(&self, id: &str, rev: &str) -> Result<DocFetch> {
        let e = self.settled(Some(id), || format!("to read {id}"))?;
        let mut url = self.id_url(id);
        url.query_pairs_mut().append_pair("rev", rev);
        self.fetch_decrypted(&e, id, url).await
    }

    /// The ancestry of one revision (`GET /{db}/{id}?rev=…&revs=true`): the
    /// revision and every one before it, newest first, as `<gen>-<hash>`.
    /// `None` when the server no longer has that revision's body (a 404:
    /// compacted away, or never there), so its ancestry cannot be read.
    pub async fn rev_history(&self, id: &str, rev: &str) -> Result<Option<Vec<String>>> {
        let mut url = self.id_url(id);
        url.query_pairs_mut()
            .append_pair("rev", rev)
            .append_pair("revs", "true");
        let doc = match self.fetch_url(id, url).await? {
            DocFetch::Found(d) => d,
            DocFetch::Deleted | DocFetch::Missing(_) => return Ok(None),
        };
        let bad = |msg: &str| RemoteError::Decode {
            what: format!("GET {id}?revs=true"),
            msg: msg.into(),
        };
        let revs = doc
            .get("_revisions")
            .ok_or_else(|| bad("no _revisions in the answer"))?;
        let start = revs
            .get("start")
            .and_then(Value::as_u64)
            .ok_or_else(|| bad("_revisions without start"))?;
        let ids = revs
            .get("ids")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("_revisions without ids"))?;
        let mut out = Vec::with_capacity(ids.len());
        for (i, h) in ids.iter().enumerate() {
            let h = h.as_str().ok_or_else(|| bad("a revision ID that is not a string"))?;
            let generation = start
                .checked_sub(i as u64)
                .filter(|g| *g > 0)
                .ok_or_else(|| bad("more ids than generations"))?;
            out.push(format!("{generation}-{h}"));
        }
        Ok(Some(out))
    }

    /// `PUT /{db}/{id}` with the body's `_rev` (absent for a new document).
    /// Works for `_local/` IDs too.
    pub async fn put_doc(&self, doc: &Value) -> Result<PutOutcome> {
        let id = doc
            .get("_id")
            .and_then(Value::as_str)
            .ok_or_else(|| RemoteError::Url("document without _id".into()))?;
        let what = format!("PUT {id}");
        let doc = self.encrypt(doc)?;
        let r = self.send(&what, self.http().put(self.id_url(id)).json(&doc)).await?;
        if r.status() == StatusCode::CONFLICT {
            return Ok(PutOutcome::Conflict);
        }
        let v: Value = self.json(&what, r).await?;
        let rev = v
            .get("rev")
            .and_then(Value::as_str)
            .ok_or_else(|| RemoteError::Decode {
                what,
                msg: "no rev in response".into(),
            })?;
        Ok(PutOutcome::Saved { rev: rev.to_string() })
    }

    /// `POST /{db}/_bulk_docs`. With `new_edits: false` the documents carry
    /// their own `_rev` and `_revisions` and are merged into the revision
    /// tree as given (CouchDB then answers with an empty list on success).
    pub async fn bulk_docs(&self, docs: &[Value], new_edits: bool) -> Result<Vec<BulkResult>> {
        let url = self.doc_url(&["_bulk_docs"]);
        let docs = docs.iter().map(|d| self.encrypt(d)).collect::<Result<Vec<Value>>>()?;
        let body = if new_edits {
            json!({ "docs": docs })
        } else {
            json!({ "docs": docs, "new_edits": false })
        };
        let r = self.send("_bulk_docs", self.http().post(url).json(&body)).await?;
        self.json("_bulk_docs", r).await
    }

    /// `POST /{db}/_all_docs` with `keys` and without bodies: which documents
    /// exist (`value.rev`, `value.deleted`) and which do not (`error`).
    pub async fn all_docs_revs(&self, keys: &[String]) -> Result<Vec<AllDocsRow>> {
        let url = self.doc_url(&["_all_docs"]);
        let r = self
            .send("_all_docs", self.http().post(url).json(&json!({ "keys": keys })))
            .await?;
        let resp: AllDocsResponse = self.json("_all_docs", r).await?;
        Ok(resp.rows)
    }

    /// One page of `GET /{db}/_all_docs` in key order, starting at
    /// `start_key` (inclusive) and stopping at `end_key` (inclusive).
    pub async fn all_docs_page(
        &self,
        start_key: Option<&str>,
        end_key: Option<&str>,
        limit: usize,
        include_docs: bool,
    ) -> Result<Vec<AllDocsRow>> {
        let e = self.settled(None, || "to read _all_docs".into())?;
        let mut rows = self.all_docs_page_raw(start_key, end_key, limit, include_docs).await?;
        self.decrypt_rows(&e, &mut rows, |r| (&mut r.doc, &mut r.undecryptable));
        Ok(rows)
    }

    /// [`Couch::all_docs_page`] without the encryption transform: the
    /// bodies as stored (for checking a passphrase against them).
    pub async fn all_docs_page_raw(
        &self,
        start_key: Option<&str>,
        end_key: Option<&str>,
        limit: usize,
        include_docs: bool,
    ) -> Result<Vec<AllDocsRow>> {
        let mut url = self.doc_url(&["_all_docs"]);
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("limit", &limit.to_string());
            if include_docs {
                qp.append_pair("include_docs", "true").append_pair("conflicts", "true");
            }
            if let Some(k) = start_key {
                qp.append_pair("start_key", &serde_json::to_string(k).unwrap());
            }
            if let Some(k) = end_key {
                qp.append_pair("end_key", &serde_json::to_string(k).unwrap());
            }
        }
        let r = self.send("_all_docs", self.http().get(url)).await?;
        let resp: AllDocsResponse = self.json("_all_docs", r).await?;
        Ok(resp.rows)
    }

    /// `GET /{db}/{id}`, telling a deleted document (404 with
    /// `reason: "deleted"`) apart from any other 404: a document that never
    /// existed, a database that is gone, or a proxy answering for something else.
    pub async fn fetch_doc(&self, id: &str, conflicts: bool) -> Result<DocFetch> {
        let e = self.settled(Some(id), || format!("to read {id}"))?;
        let mut url = self.id_url(id);
        if conflicts {
            url.query_pairs_mut().append_pair("conflicts", "true");
        }
        self.fetch_decrypted(&e, id, url).await
    }

    /// [`Couch::fetch_doc`] without the encryption transform: the body as
    /// stored, whatever the encryption (for the connect gate's own checks).
    pub async fn fetch_doc_raw(&self, id: &str) -> Result<DocFetch> {
        self.fetch_url(id, self.id_url(id)).await
    }

    async fn fetch_decrypted(&self, e: &Encryption, id: &str, url: Url) -> Result<DocFetch> {
        match (self.fetch_url(id, url).await?, e) {
            (DocFetch::Found(d), Encryption::Encrypted(c)) => {
                decrypt_with(c, d).map(DocFetch::Found).map_err(RemoteError::Crypto)
            }
            (other, _) => Ok(other),
        }
    }

    async fn fetch_url(&self, id: &str, url: Url) -> Result<DocFetch> {
        let what = format!("GET {id}");
        let r = self.send(&what, self.http().get(url)).await?;
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
        self.json(&what, r).await.map(DocFetch::Found)
    }

    /// Filtered `_changes` with `include_docs` and `conflicts`.
    pub async fn changes(&self, q: &ChangesQuery) -> Result<ChangesResponse> {
        let e = self.settled(None, || "to read _changes".into())?;
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
        let rb = self.http().post(url).json(&changes_selector());
        let r = self.send_within("_changes", rb, q.idle_timeout).await?;
        let mut resp: ChangesResponse = self.json_within("_changes", r, q.idle_timeout).await?;
        self.decrypt_rows(&e, &mut resp.results, |r| (&mut r.doc, &mut r.undecryptable));
        Ok(resp)
    }

    /// The most recently changed documents (chunks excluded, as in
    /// [`Couch::changes`]), newest first, as stored: no transform. At most
    /// `limit`; deletions are left out. For the connect gate's own checks.
    pub async fn recent_docs_raw(&self, limit: usize) -> Result<Vec<Value>> {
        let mut url = self.doc_url(&["_changes"]);
        url.query_pairs_mut()
            .append_pair("filter", "_selector")
            .append_pair("include_docs", "true")
            .append_pair("descending", "true")
            .append_pair("limit", &limit.to_string());
        let r = self
            .send("_changes", self.http().post(url).json(&changes_selector()))
            .await?;
        let resp: ChangesResponse = self.json("_changes", r).await?;
        Ok(resp
            .results
            .into_iter()
            .filter(|r| !r.deleted)
            .filter_map(|r| r.doc)
            .filter(|d| !d.is_null())
            .collect())
    }

    /// `POST /{db}/_all_docs?include_docs=true` with `keys`.
    pub async fn all_docs_by_keys(&self, keys: &[String]) -> Result<Vec<AllDocsRow>> {
        let e = self.settled(None, || "to read _all_docs".into())?;
        let mut resp = self.all_docs_by_keys_raw(keys).await?;
        self.decrypt_rows(&e, &mut resp, |r| (&mut r.doc, &mut r.undecryptable));
        Ok(resp)
    }

    /// [`Couch::all_docs_by_keys`] without the encryption transform.
    pub async fn all_docs_by_keys_raw(&self, keys: &[String]) -> Result<Vec<AllDocsRow>> {
        let mut url = self.doc_url(&["_all_docs"]);
        url.query_pairs_mut()
            .append_pair("include_docs", "true")
            .append_pair("conflicts", "true");
        let r = self
            .send("_all_docs", self.http().post(url).json(&json!({ "keys": keys })))
            .await?;
        let resp: AllDocsResponse = self.json("_all_docs", r).await?;
        Ok(resp.rows)
    }
}

/// Bodies that never go through the transform: `_local/` documents (the
/// milestone, the sync parameters) and chunk tombstones without `data`
/// (nothing to encrypt; upstream's `incoming` fails on them).
fn passes_untouched(doc: &Value) -> bool {
    let id = doc.get("_id").and_then(Value::as_str).unwrap_or_default();
    let owns_data = id.starts_with(livesync_crypto::ids::CHUNK_PREFIX) || id == livesync_crypto::ids::SYNCINFO_ID;
    id.starts_with("_local/")
        || (owns_data && doc.get("_deleted").and_then(Value::as_bool) == Some(true) && doc.get("data").is_none())
}

fn decrypt_with(c: &DocumentCipher, doc: Value) -> std::result::Result<Value, Box<Undecryptable>> {
    if passes_untouched(&doc) || doc.is_null() {
        return Ok(doc);
    }
    let field = |k: &str| doc.get(k).and_then(Value::as_str).map(str::to_string);
    let (id, rev, path) = (field("_id").unwrap_or_default(), field("_rev"), field("path"));
    c.outgoing(doc)
        .map_err(|error| Box::new(Undecryptable { id, rev, path, error }))
}

/// `incoming` (with a cipher) and the write guard, both ways; nothing but
/// `_local/` while the encryption is not settled.
fn encrypt_with(e: &Encryption, doc: &Value) -> Result<Value> {
    let id = doc.get("_id").and_then(Value::as_str).unwrap_or_default();
    if id.starts_with("_local/") {
        return Ok(doc.clone());
    }
    let c = match e {
        Encryption::Undetermined => return Err(RemoteError::EncryptionUndetermined(format!("to write {id}"))),
        Encryption::Plaintext => None,
        Encryption::Encrypted(c) => Some(&**c),
    };
    let is_chunk_id = id.starts_with(livesync_crypto::ids::CHUNK_PREFIX);
    let encrypted_id = livesync_crypto::ids::is_encrypted_chunk_id(id);
    let children = doc
        .get("children")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let Some(c) = c else {
        // A plaintext database: nothing encrypted may go in.
        let sealed_eden = doc.get("eden").and_then(Value::as_object).is_some_and(|e| {
            e.contains_key(livesync_crypto::ids::EDEN_ENCRYPTED_KEY)
                || e.contains_key(livesync_crypto::ids::EDEN_ENCRYPTED_KEY_HKDF)
        });
        if encrypted_id
            || doc.get("e_").is_some()
            || sealed_eden
            || children.iter().any(|c| livesync_crypto::ids::is_encrypted_chunk_id(c))
        {
            return Err(RemoteError::Guard(format!(
                "{id}: encrypted content or an encrypted chunk ID into a database that is not encrypted"
            )));
        }
        return Ok(doc.clone());
    };
    if is_chunk_id && !encrypted_id {
        return Err(RemoteError::Guard(format!(
            "{id}: a plain chunk ID into an end-to-end encrypted database"
        )));
    }
    if let Some(p) = children
        .iter()
        .find(|c| !livesync_crypto::ids::is_encrypted_chunk_id(c))
    {
        return Err(RemoteError::Guard(format!(
            "{id}: a reference to the plain chunk ID {p} into an end-to-end encrypted database"
        )));
    }
    if passes_untouched(doc) {
        return Ok(doc.clone());
    }
    let out = c
        .incoming(doc.clone())
        .map_err(|e| RemoteError::Guard(format!("{id}: it cannot be encrypted ({e})")))?;
    // Nothing readable may be left: what upstream's reader would reject.
    if let Err(e) = livesync_crypto::check_unencrypted_content(&out, false) {
        return Err(RemoteError::Guard(format!(
            "{id}: plaintext content into an end-to-end encrypted database ({e})"
        )));
    }
    if encrypted_id
        && !out
            .get("data")
            .and_then(Value::as_str)
            .is_some_and(|d| d.starts_with(livesync_crypto::HKDF_PREFIX))
    {
        return Err(RemoteError::Guard(format!("{id}: a chunk that is not V2 ciphertext")));
    }
    Ok(out)
}

/// The user name and password written into a server URL
/// (`https://user:password@host/`), decoded as [`Couch::new`] reads them;
/// `None` when it has neither (or does not parse).
pub fn url_userinfo(url: &str) -> Option<(Option<String>, Option<String>)> {
    let u = Url::parse(url.trim()).ok()?;
    if u.username().is_empty() && u.password().is_none() {
        return None;
    }
    Some((
        Some(decode(u.username())).filter(|s| !s.is_empty()),
        u.password().map(decode),
    ))
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

    fn enc(c: &DocumentCipher) -> Encryption {
        Encryption::Encrypted(Arc::new(c.clone()))
    }

    fn cipher() -> DocumentCipher {
        DocumentCipher::new(
            livesync_crypto::KeyContext::derive("throwaway-test-passphrase", b"salt-for-tests"),
            false,
        )
    }

    #[test]
    fn writes_are_encrypted_and_guarded() {
        let c = cipher();
        let chunk = json!({"_id": "h:+abc", "type": "leaf", "data": "secret text"});
        let out = encrypt_with(&enc(&c), &chunk).unwrap();
        assert_eq!(out["e_"], json!(true));
        assert!(out["data"].as_str().unwrap().starts_with("%="));
        assert_eq!(decrypt_with(&c, out).unwrap()["data"], json!("secret text"));
        // Plain chunk IDs, references to them and inline content never go in.
        for bad in [
            json!({"_id": "h:abc", "type": "leaf", "data": "x"}),
            json!({"_id": "a.md", "path": "a.md", "type": "plain", "children": ["h:abc"], "eden": {}}),
            json!({"_id": "a.md", "path": "a.md", "type": "notes", "data": "inline"}),
        ] {
            assert!(
                matches!(encrypt_with(&enc(&c), &bad), Err(RemoteError::Guard(_))),
                "{bad}"
            );
        }
        // Metadata and `_local/` documents pass; a chunk tombstone is not touched.
        let meta = json!({"_id": "a.md", "path": "a.md", "type": "plain", "children": ["h:+1"], "eden": {}});
        assert_eq!(encrypt_with(&enc(&c), &meta).unwrap(), meta);
        let tomb = json!({"_id": "h:+1", "_rev": "1-a", "_deleted": true});
        assert_eq!(encrypt_with(&enc(&c), &tomb).unwrap(), tomb);
        let local = json!({"_id": "_local/x", "data": "anything"});
        assert_eq!(encrypt_with(&enc(&c), &local).unwrap(), local);
        // The mirror: nothing encrypted into a plaintext database.
        for bad in [
            json!({"_id": "h:+abc", "type": "leaf", "data": "x"}),
            json!({"_id": "h:abc", "type": "leaf", "data": "%=x", "e_": true}),
            json!({"_id": "a.md", "path": "a.md", "type": "plain", "children": ["h:+abc"]}),
            json!({"_id": "a.md", "type": "plain", "eden": {"h:++encrypted-hkdf": {"data": "%=x"}}}),
        ] {
            assert!(
                matches!(encrypt_with(&Encryption::Plaintext, &bad), Err(RemoteError::Guard(_))),
                "{bad}"
            );
        }
        let plain = json!({"_id": "h:abc", "type": "leaf", "data": "x"});
        assert_eq!(encrypt_with(&Encryption::Plaintext, &plain).unwrap(), plain);
        // Not settled: no document write at all, plaintext or not; `_local/` only.
        for doc in [plain, meta, tomb] {
            assert!(
                matches!(
                    encrypt_with(&Encryption::Undetermined, &doc),
                    Err(RemoteError::EncryptionUndetermined(_))
                ),
                "{doc}"
            );
        }
        assert_eq!(encrypt_with(&Encryption::Undetermined, &local).unwrap(), local);
    }

    /// Until the encryption is settled nothing is sent that would write a
    /// document or read one through the transform (the port is closed: an
    /// attempt would fail with a transport error instead).
    #[tokio::test]
    async fn an_unsettled_client_neither_writes_nor_reads_documents() {
        let c = Couch::new(&RemoteConfig {
            url: "http://127.0.0.1:9".into(),
            database: "db".into(),
            username: None,
            password: None,
        })
        .unwrap();
        let undetermined = |r: Result<()>| matches!(r, Err(RemoteError::EncryptionUndetermined(_)));
        let doc = json!({"_id": "h:abc", "type": "leaf", "data": "x"});
        assert!(undetermined(c.put_doc(&doc).await.map(|_| ())));
        assert!(undetermined(c.bulk_docs(&[doc], true).await.map(|_| ())));
        assert!(undetermined(c.fetch_doc("a.md", false).await.map(|_| ())));
        assert!(undetermined(c.get_doc("a.md", false).await.map(|_| ())));
        assert!(undetermined(c.fetch_rev("a.md", "1-a").await.map(|_| ())));
        assert!(undetermined(c.all_docs_by_keys(&["h:abc".into()]).await.map(|_| ())));
        assert!(undetermined(c.all_docs_page(None, None, 1, true).await.map(|_| ())));
        let q = ChangesQuery {
            since: None,
            limit: 1,
            longpoll: false,
            heartbeat_ms: 0,
            idle_timeout: None,
        };
        assert!(undetermined(c.changes(&q).await.map(|_| ())));
        // `_local/` documents are not encrypted: they go out (and fail here
        // for want of a server).
        let local = c.get_doc("_local/x", false).await;
        assert!(matches!(local, Err(RemoteError::Transport { .. })), "{local:?}");
        // Settled, a write goes out.
        c.set_encryption(Encryption::Plaintext);
        let put = c.put_doc(&json!({"_id": "a.md"})).await;
        assert!(matches!(put, Err(RemoteError::Transport { .. })), "{put:?}");
        // And unsettled again.
        c.set_encryption(Encryption::Undetermined);
        assert!(undetermined(c.put_doc(&json!({"_id": "a.md"})).await.map(|_| ())));
    }

    #[test]
    fn reads_that_fail_are_reported_with_what_is_known() {
        let c = cipher();
        let other = DocumentCipher::new(
            livesync_crypto::KeyContext::derive("another-passphrase", b"salt-for-tests"),
            false,
        );
        let sealed = encrypt_with(&enc(&other), &json!({"_id": "h:+abc", "type": "leaf", "data": "x"})).unwrap();
        let u = decrypt_with(&c, sealed).unwrap_err();
        assert_eq!(u.id, "h:+abc");
        assert_eq!(u.error, CryptoError::Authentication);
        let plain = json!({"_id": "n.md", "_rev": "1-a", "path": "N.md", "type": "notes", "data": "x"});
        let u = decrypt_with(&c, plain).unwrap_err();
        assert!(matches!(u.error, CryptoError::Rejected { .. }), "{u:?}");
        assert_eq!(u.path.as_deref(), Some("N.md"));
        assert_eq!(u.rev.as_deref(), Some("1-a"));
        assert!(!format!("{u}").is_empty());
    }
}
