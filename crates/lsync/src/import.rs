//! `lsync import FILE` (spec D15): the LiveSync plugin's `data.json` or the
//! official CLI's `settings.json` becomes an account and a vault in
//! `lsync.toml`, the password goes into the secret store.
//!
//! Both files are upstream's `ObsidianLiveSyncSettings` as its
//! `SettingService` saves them (livesync-commonlib
//! `services/base/SettingService.ts`, `common/models/setting.policy.ts`):
//!
//! - The connection (`couchDB_URI`, `couchDB_USER`, `couchDB_PASSWORD`,
//!   `couchDB_DBNAME`, ...) is kept encrypted in `encryptedCouchDBConnection`,
//!   remote profiles (`remoteConfigurations`) in their `uri` with
//!   `isEncrypted`, and an E2EE passphrase in `encryptedPassphrase`. The key
//!   is the configuration passphrase plus `SALT_OF_PASSPHRASE`; the
//!   passphrase is `*` unless `configPassphraseStore` keeps one on the device
//!   (`LOCALSTORAGE`) or asks for it (`ASK_AT_LAUNCH`), and then it must be
//!   given (`--config-passphrase-stdin`). Older files hold the fields in
//!   plain text.
//! - As upstream loads the file, the active remote profile
//!   (`activeConfigurationId`) wins over the connection fields.
//!
//! Settings lsync cannot honour stop the import with the reason, never
//! ignored: another kind of remote, end-to-end encryption, path
//! obfuscation, authentication lsync does not do. The settings whose effect
//! lsync leaves out on purpose (hidden-file sync, customisation sync,
//! resolving conflicts by the newer file, P2P) also stop it, unless the user
//! accepts going on without them (`--accept KEY`).

use crate::config::Setup;
use crate::manage::{self, Given};
use crate::service::{self, Change};
use anyhow::{anyhow, bail, Result};
use livesync_host::config::{check_name, server_key, Account, Vault};
use livesync_host::migrate;
use livesync_host::run::split_userinfo;
use livesync_host::secret::{Secret, SecretRef, StoreKind};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

/// Upstream's `SALT_OF_PASSPHRASE` (common/models/shared.const.behabiour.ts).
const SALT_OF_PASSPHRASE: &str = "rHGMPtr6oWw7VSa3W3wpa8fT8U";
/// octagonal-wheels `encryption/hkdf.js`.
const HKDF_SALTED_PREFIX: &str = "%$";
const PBKDF2_ITERATIONS: u32 = 310_000;
const PBKDF2_SALT_LEN: usize = 32;
const IV_LEN: usize = 12;
const HKDF_SALT_LEN: usize = 32;

/// The connection fields upstream encrypts together
/// (`selectConnectionSettings`).
const CONNECTION_KEYS: [&str; 14] = [
    "couchDB_URI",
    "couchDB_USER",
    "couchDB_PASSWORD",
    "couchDB_DBNAME",
    "couchDB_CustomHeaders",
    "jwtKey",
    "jwtKid",
    "jwtSub",
    "accessKey",
    "secretKey",
    "bucket",
    "region",
    "endpoint",
    "bucketCustomHeaders",
];

/// Decrypts configuration items, deriving each PBKDF2 key once (upstream
/// uses one PBKDF2 salt per session, so a file's items usually share it).
pub struct Decryptor {
    passphrase: String,
    masters: HashMap<Vec<u8>, [u8; 32]>,
}

impl Decryptor {
    /// For the configuration passphrase as upstream stores it (`*` for the
    /// default store).
    pub fn new(config_passphrase: &str) -> Decryptor {
        Decryptor {
            passphrase: format!("{config_passphrase}{SALT_OF_PASSPHRASE}"),
            masters: HashMap::new(),
        }
    }

    /// `decryptString` for the format upstream writes today
    /// (`encryptWithEphemeralSalt`: `%$` + base64 of PBKDF2 salt, IV, HKDF
    /// salt and the AES-256-GCM ciphertext with its tag).
    pub fn decrypt(&mut self, item: &str) -> std::result::Result<String, String> {
        use ring::{aead, hkdf, pbkdf2};
        let Some(b64) = item.strip_prefix(HKDF_SALTED_PREFIX) else {
            return Err(if item.starts_with('%') {
                "it is encrypted in an older format that lsync does not read (save the settings again with a current plugin)".into()
            } else {
                "it is not in an encrypted format lsync knows".into()
            });
        };
        let bin = livesync_model::codec::base64_decode(b64).map_err(|e| format!("not valid base64 ({e})"))?;
        if bin.len() < PBKDF2_SALT_LEN + IV_LEN + HKDF_SALT_LEN + 16 {
            return Err("too short to be encrypted data".into());
        }
        let (pbkdf2_salt, rest) = bin.split_at(PBKDF2_SALT_LEN);
        let (iv, rest) = rest.split_at(IV_LEN);
        let (hkdf_salt, data) = rest.split_at(HKDF_SALT_LEN);
        let passphrase = self.passphrase.as_bytes();
        let master = *self.masters.entry(pbkdf2_salt.to_vec()).or_insert_with(|| {
            let mut out = [0u8; 32];
            pbkdf2::derive(
                pbkdf2::PBKDF2_HMAC_SHA256,
                NonZeroU32::new(PBKDF2_ITERATIONS).expect("not zero"),
                pbkdf2_salt,
                passphrase,
                &mut out,
            );
            out
        });
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, hkdf_salt).extract(&master);
        let okm = prk
            .expand(&[], &aead::AES_256_GCM)
            .map_err(|_| "key derivation failed".to_string())?;
        let key = aead::LessSafeKey::new(aead::UnboundKey::from(okm));
        let nonce = aead::Nonce::try_assume_unique_for_key(iv).map_err(|_| "bad IV".to_string())?;
        let mut buf = data.to_vec();
        let plain = key
            .open_in_place(nonce, aead::Aad::empty(), &mut buf)
            .map_err(|_| "it does not decrypt with this configuration passphrase".to_string())?;
        String::from_utf8(plain.to_vec()).map_err(|_| "it decrypts to something that is not text".into())
    }
}

/// Where a settings file comes from, by its place in a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `<vault>/<config dir>/plugins/<plugin>/data.json`.
    Plugin,
    /// `<vault>/.livesync/settings.json`.
    Cli,
    /// Anywhere else: the folder must be given.
    Unknown,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Plugin => "plugin data.json",
            Source::Cli => "official CLI settings.json",
            Source::Unknown => "LiveSync settings",
        }
    }
}

/// The vault folder a settings file belongs to, when its place says so.
pub fn folder_of(file: &Path) -> (Source, Option<PathBuf>) {
    let parts: Vec<String> = file
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let n = parts.len();
    let up = |k: usize| -> Option<PathBuf> {
        let mut p = file.to_path_buf();
        for _ in 0..k {
            p = p.parent()?.to_path_buf();
        }
        Some(p)
    };
    if n >= 5 && parts[n - 1] == "data.json" && parts[n - 3] == "plugins" && parts[n - 4].starts_with('.') {
        return (Source::Plugin, up(4));
    }
    if n >= 3 && parts[n - 1] == "settings.json" && parts[n - 2] == ".livesync" {
        return (Source::Cli, up(2));
    }
    (Source::Unknown, None)
}

/// What an import makes of a settings file.
pub struct Imported {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<Secret>,
    pub database: String,
    pub ignore_regex: Option<String>,
    pub sync_only_regex: Option<String>,
    /// Set when the file turns ignore files on.
    pub ignore_files: Option<Vec<String>>,
    /// `(setting, what lsync does with it)`.
    pub carried: Vec<(String, String)>,
    /// Settings with no counterpart, and what lsync does instead.
    pub not_used: Vec<(String, String)>,
    /// Settings the user accepted going on without (`--accept`).
    pub accepted: Vec<(String, String)>,
}

/// Settings lsync leaves out on purpose: refused unless accepted.
/// `(key, applies, why)`.
fn acceptable(s: &Map<String, Value>) -> Vec<(&'static str, String)> {
    let on = |k: &str| s.get(k).and_then(Value::as_bool).unwrap_or(false);
    let mut out = vec![];
    if on("syncInternalFiles") {
        out.push((
            "syncInternalFiles",
            "hidden-file sync is on; lsync does not sync the Obsidian config folder or other dot-paths (settings sync comes later)".to_string(),
        ));
    }
    if on("usePluginSync") {
        out.push((
            "usePluginSync",
            "customisation sync is on; lsync does not sync plugins and settings that way".to_string(),
        ));
    }
    if on("resolveConflictsByNewerFile") {
        out.push((
            "resolveConflictsByNewerFile",
            "conflicts are resolved by keeping the newer file, which drops the other side; lsync never does that and leaves such conflicts open".to_string(),
        ));
    }
    if on("writeDocumentsIfConflicted") {
        out.push((
            "writeDocumentsIfConflicted",
            "conflicted documents are written over the file; lsync keeps the local file until the conflict is resolved"
                .to_string(),
        ));
    }
    if on("P2P_Enabled") {
        out.push((
            "P2P_Enabled",
            "peer-to-peer sync is on; lsync syncs only through CouchDB".to_string(),
        ));
    }
    out
}

fn string<'a>(s: &'a Map<String, Value>, k: &str) -> &'a str {
    s.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Settings that make the import impossible, whatever the user accepts.
fn hard_refusals(s: &Map<String, Value>) -> Vec<String> {
    let mut out = vec![];
    match string(s, "remoteType") {
        "" | "COUCHDB" => {}
        "MINIO" => out.push("remoteType=MINIO: an S3 or object-storage remote; lsync syncs only with CouchDB".into()),
        "ONLY_P2P" => out.push("remoteType=ONLY_P2P: a peer-to-peer remote; lsync syncs only with CouchDB".into()),
        other => out.push(format!("remoteType={other}: lsync syncs only with CouchDB")),
    }
    let on = |k: &str| s.get(k).and_then(Value::as_bool).unwrap_or(false);
    if on("encrypt") {
        out.push("encrypt=true: the database uses end-to-end encryption, which lsync does not support yet (P7)".into());
    }
    if on("useJWT") {
        out.push("useJWT=true: lsync signs in with a user and password only".into());
    }
    match s.get("idDerivationVersion") {
        None | Some(Value::Null) => {}
        Some(v) if v.as_i64() == Some(0) => {}
        Some(v) => out.push(format!("idDerivationVersion={v}: keyed document IDs are not supported")),
    }
    if !string(s, "encryptedIdDerivationKey").is_empty() {
        out.push("encryptedIdDerivationKey is set: keyed document IDs are not supported".into());
    }
    // The connect gate's rules, on the values this file was set up with.
    // `encryptInternalMetadata` only takes effect with `encrypt`
    // (upstream `usesEncryptedInternalMetadata`), which is refused above.
    let mut tweaks = s.clone();
    if !on("encrypt") {
        tweaks.remove("encryptInternalMetadata");
    }
    tweaks.remove("encrypt");
    out.extend(
        livesync_model::gate::unsupported_tweaks(&tweaks)
            .into_iter()
            .map(|e| e.to_string()),
    );
    out
}

/// A remote profile's `sls+` URI as the connection fields
/// (`ConnectionStringParser.parse` for CouchDB).
fn parse_profile(uri: &str) -> std::result::Result<Map<String, Value>, String> {
    let (sub, rest) = uri
        .strip_prefix("sls+")
        .and_then(|r| r.split_once(':'))
        .ok_or("not an sls+ connection string")?;
    match sub {
        "http" | "https" => {}
        "s3" => return Err("an S3 or object-storage remote; lsync syncs only with CouchDB".into()),
        "p2p" => return Err("a peer-to-peer remote; lsync syncs only with CouchDB".into()),
        other => return Err(format!("an sls+{other} remote; lsync syncs only with CouchDB")),
    }
    let u = url::Url::parse(&format!("https:{rest}")).map_err(|e| format!("not a valid URL ({e})"))?;
    let decode =
        |s: &str| percent_decode(s).map_err(|_| "the user or password is not valid percent-encoded UTF-8".to_string());
    let q = |k: &str| u.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
    let path = if u.path() == "/" { "" } else { u.path() };
    let host = match u.port() {
        Some(p) => format!("{}:{p}", u.host_str().unwrap_or_default()),
        None => u.host_str().unwrap_or_default().to_string(),
    };
    let mut m = Map::new();
    m.insert("couchDB_URI".into(), json!(format!("{sub}://{host}{path}")));
    m.insert("couchDB_USER".into(), json!(decode(u.username())?));
    m.insert("couchDB_PASSWORD".into(), json!(decode(u.password().unwrap_or(""))?));
    m.insert("couchDB_DBNAME".into(), json!(q("db").unwrap_or_default()));
    m.insert("couchDB_CustomHeaders".into(), json!(q("headers").unwrap_or_default()));
    m.insert("useJWT".into(), json!(q("useJWT").as_deref() == Some("true")));
    Ok(m)
}

fn percent_decode(s: &str) -> std::result::Result<String, std::str::Utf8Error> {
    percent_encoding::percent_decode_str(s)
        .decode_utf8()
        .map(|c| c.into_owned())
}

/// Read a settings file's JSON into what lsync would import. Refuses (with
/// every reason at once) what lsync cannot honour. `config_passphrase` is
/// the configuration passphrase when the file's store needs one.
pub fn parse(v: &Value, config_passphrase: Option<&str>, accept: &[String]) -> std::result::Result<Imported, String> {
    let s = v.as_object().ok_or("not a JSON object")?;
    let looks_like = [
        "couchDB_URI",
        "encryptedCouchDBConnection",
        "remoteConfigurations",
        "settingVersion",
    ]
    .iter()
    .any(|k| s.contains_key(*k));
    if !looks_like {
        return Err(
            "not a LiveSync settings file (no couchDB_URI, encryptedCouchDBConnection or remoteConfigurations)".into(),
        );
    }
    let mut refusals = hard_refusals(s);
    let mut accepted = vec![];
    let acceptable = acceptable(s);
    for a in accept {
        if !acceptable.iter().any(|(k, _)| k == a) {
            let known: Vec<&str> = acceptable.iter().map(|(k, _)| *k).collect();
            return Err(format!(
                "--accept {a}: this file has no such setting turned on{}",
                if known.is_empty() {
                    String::new()
                } else {
                    format!(" (the ones to accept here: {})", known.join(", "))
                }
            ));
        }
    }
    for (k, why) in acceptable {
        if accept.iter().any(|a| a == k) {
            accepted.push((k.to_string(), why));
        } else {
            refusals.push(format!("{k}=true: {why} (go on without it with --accept {k})"));
        }
    }
    if !refusals.is_empty() {
        return Err(format!(
            "lsync cannot honour these settings:\n  - {}",
            refusals.join("\n  - ")
        ));
    }

    // The connection, decrypted where it is encrypted.
    let store = string(s, "configPassphraseStore").to_string();
    let mut settings = s.clone();
    let mut decryptor: Option<Decryptor> = None;
    let mut decrypt = |what: &str, item: &str| -> std::result::Result<String, String> {
        if decryptor.is_none() {
            let pass = match (store.as_str(), config_passphrase) {
                (_, Some(p)) => p.to_string(),
                ("", None) => "*".to_string(),
                (other, None) => {
                    return Err(format!(
                        "the connection is encrypted with a configuration passphrase (configPassphraseStore = {other}), which the file does not hold; give it with --config-passphrase-stdin"
                    ))
                }
            };
            decryptor = Some(Decryptor::new(&pass));
        }
        decryptor
            .as_mut()
            .expect("set above")
            .decrypt(item)
            .map_err(|e| format!("{what} cannot be read: {e}"))
    };
    let enc = string(s, "encryptedCouchDBConnection").to_string();
    if !enc.is_empty() {
        let text = decrypt("encryptedCouchDBConnection", &enc)?;
        let conn: Value = serde_json::from_str(&text)
            .map_err(|_| "encryptedCouchDBConnection decrypts to something that is not JSON")?;
        let conn = conn
            .as_object()
            .ok_or("encryptedCouchDBConnection is not a JSON object")?;
        for k in CONNECTION_KEYS {
            match conn.get(k) {
                Some(Value::String(v)) => {
                    settings.insert(k.into(), json!(v));
                }
                Some(_) => return Err(format!("encryptedCouchDBConnection: {k} is not text")),
                None => {}
            }
        }
    }
    let active = string(s, "activeConfigurationId").to_string();
    if !active.is_empty() {
        if let Some(p) = s.get("remoteConfigurations").and_then(|r| r.get(&active)) {
            let uri = p.get("uri").and_then(Value::as_str).unwrap_or("");
            let uri = if p.get("isEncrypted").and_then(Value::as_bool).unwrap_or(false) {
                decrypt(&format!("the remote profile {active}"), uri)?
            } else {
                uri.to_string()
            };
            let conn = parse_profile(&uri).map_err(|e| format!("the active remote profile {active} is {e}"))?;
            settings.extend(conn);
        }
    }
    if !string(&settings, "couchDB_CustomHeaders").trim().is_empty() {
        return Err(
            "lsync cannot honour these settings:\n  - couchDB_CustomHeaders: lsync sends no custom headers".into(),
        );
    }
    if settings.get("useJWT").and_then(Value::as_bool).unwrap_or(false) {
        return Err(
            "lsync cannot honour these settings:\n  - useJWT=true: lsync signs in with a user and password only".into(),
        );
    }
    let mut url = string(&settings, "couchDB_URI").trim().to_string();
    let database = string(&settings, "couchDB_DBNAME").trim().to_string();
    if url.is_empty() || database.is_empty() {
        return Err("the file holds no CouchDB connection (couchDB_URI and couchDB_DBNAME are empty)".into());
    }
    let mut username = Some(string(&settings, "couchDB_USER").to_string()).filter(|u| !u.is_empty());
    let mut password = Some(string(&settings, "couchDB_PASSWORD").to_string()).filter(|p| !p.is_empty());
    if let Some(u) = split_userinfo(&url) {
        url = u.url;
        username = username.or(u.username);
        password = password.or(u.password);
    }
    livesync_host::run::check_url(&url)?;
    if username.is_none() {
        password = None;
    }

    let mut carried = vec![
        ("couchDB_URI".into(), format!("account url {url}")),
        ("couchDB_DBNAME".into(), format!("vault database {database}")),
    ];
    if let Some(u) = &username {
        carried.push(("couchDB_USER".into(), format!("account username {u}")));
    }
    if password.is_some() {
        carried.push(("couchDB_PASSWORD".into(), "the secret store".into()));
    }
    let ignore_regex = Some(string(s, "syncIgnoreRegEx").to_string()).filter(|r| !r.trim().is_empty());
    if let Some(r) = &ignore_regex {
        carried.push(("syncIgnoreRegEx".into(), format!("vault ignore_regex {r}")));
    }
    let sync_only_regex = Some(string(s, "syncOnlyRegEx").to_string()).filter(|r| !r.trim().is_empty());
    if let Some(r) = &sync_only_regex {
        carried.push(("syncOnlyRegEx".into(), format!("vault sync_only_regex {r}")));
    }
    let ignore_files = if s.get("useIgnoreFiles").and_then(Value::as_bool).unwrap_or(false) {
        let names: Vec<String> = string(s, "ignoreFiles")
            .split(',')
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect();
        carried.push(("ignoreFiles".into(), format!("vault ignore_files {}", names.join(", "))));
        Some(names)
    } else {
        None
    };

    let mut not_used = vec![];
    let note = |out: &mut Vec<(String, String)>, k: &str, why: &str| {
        if s.contains_key(k) {
            out.push((k.to_string(), why.to_string()));
        }
    };
    note(
        &mut not_used,
        "liveSync",
        "lsync serve syncs continuously; one-shot commands sync once",
    );
    note(
        &mut not_used,
        "syncMaxSizeInMB",
        "lsync follows the remote's syncMaxSizeInMB (a vault's max_size_mb overrides it)",
    );
    note(
        &mut not_used,
        "disableMarkdownAutoMerge",
        "lsync never merges a conflict on its own; both versions are kept for you to resolve (`lsync status`)",
    );
    note(
        &mut not_used,
        "trashInsteadDelete",
        "files deleted elsewhere go to the folder's .trash/ (the vault's trash setting)",
    );
    if s.get("encryptInternalMetadata").and_then(Value::as_bool) == Some(true) {
        not_used.push((
            "encryptInternalMetadata".into(),
            "has no effect without end-to-end encryption".into(),
        ));
    }
    Ok(Imported {
        url,
        username,
        password: password.map(Secret::new),
        database,
        ignore_regex,
        sync_only_regex,
        ignore_files,
        carried,
        not_used,
        accepted,
    })
}

/// A vault name from a folder's name: letters, digits, '.', '_' and '-'.
fn name_from(s: &str) -> Option<String> {
    let mapped: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let t = mapped.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let t: String = t.chars().take(64).collect();
    let t = t.trim_end_matches('-').to_string();
    check_name("vault", &t).ok().map(|_| t)
}

#[derive(clap::Args, Debug)]
pub struct ImportArgs {
    /// The plugin's data.json (in <vault>/.obsidian/plugins/obsidian-livesync/)
    /// or the official CLI's settings.json (in <vault>/.livesync/)
    pub file: PathBuf,
    /// The new vault's name (default: from the folder's name)
    #[arg(long)]
    pub name: Option<String>,
    /// The account's name, when a new one is made (default: from the
    /// server's host). An account for the same server and user is reused
    #[arg(long)]
    pub account: Option<String>,
    /// Read the configuration passphrase from stdin, for a file whose
    /// plugin keeps one (configPassphraseStore set)
    #[arg(long)]
    pub config_passphrase_stdin: bool,
    /// Go on without a setting lsync leaves out on purpose (named by its
    /// key in the file: syncInternalFiles, usePluginSync,
    /// resolveConflictsByNewerFile, writeDocumentsIfConflicted,
    /// P2P_Enabled); repeatable
    #[arg(long, value_name = "KEY")]
    pub accept: Vec<String>,
    /// Show what would be added; change nothing
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub json: bool,
}

/// `lsync import`.
pub fn run(ctx: &Setup, a: ImportArgs, given: &Given) -> Result<u8> {
    for (id, flag) in [
        ("url", "--url"),
        ("database", "--db"),
        ("username", "--user"),
        ("password", "--password"),
    ] {
        if given.has(id) {
            bail!("{flag} is read from the file by `lsync import`; edit the vault or account afterwards to change it");
        }
    }
    let text = std::fs::read_to_string(&a.file).map_err(|e| anyhow!("reading {}: {e}", a.file.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| anyhow!("{}: not JSON ({e})", a.file.display()))?;
    let config_passphrase = if a.config_passphrase_stdin {
        Some(crate::prompt::from_stdin()?)
    } else {
        None
    };
    let imp = parse(&v, config_passphrase.as_ref().map(|p| p.expose()), &a.accept)
        .map_err(|e| anyhow!("{}: {e}", a.file.display()))?;
    let (source, inferred) = folder_of(&std::fs::canonicalize(&a.file).unwrap_or(a.file.clone()));
    let folder = match given.o.folder.clone().filter(|_| given.has("folder")) {
        Some(f) => f,
        None => inferred.ok_or_else(|| {
            anyhow!(
                "{} is not inside a vault folder (<vault>/.obsidian/plugins/<plugin>/data.json or <vault>/.livesync/settings.json); give the folder with --folder",
                a.file.display()
            )
        })?,
    };
    let folder_text = manage::folder_value(&folder)?;

    let mut file = manage::open(ctx)?;
    let cfg = file.config(&ctx.dirs).map_err(|e| anyhow!(e))?;
    let stores = ctx.secrets();
    let name = match &a.name {
        Some(n) => {
            check_name("vault", n).map_err(|e| anyhow!(e))?;
            n.clone()
        }
        None => folder
            .file_name()
            .and_then(|n| name_from(&n.to_string_lossy()))
            .or_else(|| name_from(&imp.database))
            .ok_or_else(|| anyhow!("no vault name can be made from the folder; give one with --name"))?,
    };
    if cfg.vault.contains_key(&name) {
        bail!("there is a vault named {name} already; give the new one another name with --name");
    }

    // The account: one for this server and user, else a new one.
    let wanted = (server_key(&imp.url), imp.username.clone().unwrap_or_default());
    let key_of = |a: &Account| (a.server_key(), a.username.clone().unwrap_or_default());
    let existing = cfg
        .account
        .iter()
        .find(|(_, a)| key_of(a) == wanted)
        .map(|(n, a)| (n.clone(), a.clone()));
    let (account, new_account) = match (&existing, &a.account) {
        (Some((n, _)), Some(want)) if n != want => {
            bail!("account {n} is for this server and user already; leave out --account to use it, or remove it first")
        }
        (Some((n, _)), _) => (n.clone(), false),
        (None, Some(want)) => {
            check_name("account", want).map_err(|e| anyhow!(e))?;
            if cfg.account.contains_key(want) {
                bail!("account {want} is for another server or user; choose another name with --account");
            }
            let r = SecretRef::file(want);
            if imp.password.is_some() && stores.get(&r)?.is_some() {
                bail!("the key {r} already holds a password that no account uses; choose another --account");
            }
            (want.clone(), true)
        }
        (None, None) => {
            let base = migrate::account_base(&imp.url);
            let mut found = None;
            for c in (1..100).map(|i| if i == 1 { base.clone() } else { format!("{base}-{i}") }) {
                let r = SecretRef::file(&c);
                let used = cfg.account.contains_key(&c)
                    || cfg
                        .account
                        .iter()
                        .any(|(an, a)| a.secret_ref(an).ok().as_ref() == Some(&r));
                if !used && stores.get(&r)?.is_none() {
                    found = Some(c);
                    break;
                }
            }
            (
                found.ok_or_else(|| anyhow!("no free account name for the server {base}; give one with --account"))?,
                true,
            )
        }
    };
    let secret_ref = match &existing {
        Some((n, acc)) => acc.secret_ref(n).map_err(|e| anyhow!(e))?,
        None => SecretRef::file(&account),
    };
    // What happens to the password.
    let mut store_password = false;
    let password_line = match (&imp.password, &imp.username) {
        (_, None) => "none (no user: a server without authentication)".to_string(),
        (None, Some(_)) => format!(
            "not in the file; set it with `lsync account passwd {account}` ({})",
            stores.describe(&secret_ref)
        ),
        (Some(pw), Some(_)) => {
            if secret_ref.kind != StoreKind::File {
                // lsync cannot write these stores: the password must be the
                // imported one already, and readable here to tell.
                let where_ = stores.describe(&secret_ref);
                match stores.get(&secret_ref) {
                    Ok(Some(old)) if old == *pw => format!("{where_} (unchanged; lsync does not store it)"),
                    Ok(Some(_)) => bail!(
                        "account {account} takes its password from the {where_}, which holds another password than the file; check which is right and change it there before importing"
                    ),
                    Ok(None) => bail!(
                        "account {account} takes its password from the {where_}, which is not set here, so lsync cannot check it is the file's; import where it is set, or give --account for a new account"
                    ),
                    Err(e) => bail!(
                        "account {account} takes its password from the {where_}, which cannot be read here ({e}), so lsync cannot check it is the file's; import where it can be read, or give --account for a new account"
                    ),
                }
            } else {
                match stores.get(&secret_ref)? {
                    Some(old) if old == *pw => format!("{} (unchanged)", stores.describe(&secret_ref)),
                    Some(_) => bail!(
                        "account {account} already holds another password for this server and user; check which is right, and change it with `lsync account passwd {account}` before importing"
                    ),
                    None => {
                        store_password = true;
                        stores.describe(&secret_ref)
                    }
                }
            }
        }
    };

    let mut vault = Vault::new(&account, &imp.database, &folder_text);
    vault.id = Some(manage::new_id());
    vault.ignore_regex = imp.ignore_regex.clone();
    vault.sync_only_regex = imp.sync_only_regex.clone();
    if let Some(names) = &imp.ignore_files {
        vault.use_ignore_files = Some(true);
        vault.ignore_files = Some(names.clone());
    }
    // Added disabled: the folder is taken over with `lsync adopt` first, so
    // a running service does not start it before that.
    vault.enabled = false;
    if new_account {
        file.add_account(
            &account,
            &Account {
                id: Some(manage::new_id()),
                url: imp.url.clone(),
                username: imp.username.clone(),
                secret: Some(secret_ref.to_string()),
            },
            Some("imported by `lsync import`"),
        )
        .map_err(|e| anyhow!(e))?;
    }
    file.add_vault(&name, &vault, Some("imported by `lsync import`"))
        .map_err(|e| anyhow!(e))?;
    // The whole file is checked (overlapping folders, names) before anything is stored.
    file.config(&ctx.dirs).map_err(|e| anyhow!(e))?;

    let out = json!({
        "file": a.file,
        "source": source.as_str(),
        "dry_run": a.dry_run,
        "account": {"name": account, "new": new_account, "url": imp.url, "username": imp.username, "password": password_line},
        "vault": {"name": name, "database": imp.database, "folder": folder_text, "enabled": false,
                  "ignore_regex": imp.ignore_regex, "sync_only_regex": imp.sync_only_regex, "ignore_files": imp.ignore_files},
        "carried": imp.carried.iter().map(|(k, v)| json!({"setting": k, "to": v})).collect::<Vec<_>>(),
        "not_used": imp.not_used.iter().map(|(k, v)| json!({"setting": k, "why": v})).collect::<Vec<_>>(),
        "accepted": imp.accepted.iter().map(|(k, v)| json!({"setting": k, "why": v})).collect::<Vec<_>>(),
    });
    if !a.dry_run {
        let mut previous = None;
        if store_password {
            if let Some(pw) = &imp.password {
                previous = Some(stores.get(&secret_ref)?);
                stores.set(&secret_ref, pw)?;
            }
        }
        if let Err(e) = file.save(&ctx.dirs) {
            if let Some(previous) = previous {
                manage::undo_password(&stores, &secret_ref, previous.as_ref());
            }
            bail!(e);
        }
        service::config_changed(&ctx.config_file, &Change::Vault(name.clone()));
    }
    if a.json {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    let verb = if a.dry_run { "would add" } else { "added" };
    println!("{} ({})", a.file.display(), source.as_str());
    if new_account {
        println!("{verb} account {account}");
    } else {
        println!("uses account {account} (same server and user)");
    }
    println!("  url       {}", imp.url);
    println!("  user      {}", imp.username.as_deref().unwrap_or("-"));
    println!("  password  {password_line}");
    println!("{verb} vault {name} (disabled until it is adopted)");
    println!("  database  {}", imp.database);
    println!("  folder    {}", folder.display());
    for (k, v) in &imp.carried {
        if !k.starts_with("couchDB_") {
            println!("  {k:<16} -> {v}");
        }
    }
    if !imp.accepted.is_empty() {
        println!("going on without (--accept):");
        for (k, v) in &imp.accepted {
            println!("  {k}: {v}");
        }
    }
    if !imp.not_used.is_empty() {
        println!("not used by lsync:");
        for (k, v) in &imp.not_used {
            println!("  {k}: {v}");
        }
    }
    if !a.dry_run {
        println!("next:");
        println!("  lsync --vault {name} adopt --dry-run    compare the folder with the server");
        println!("  lsync --vault {name} adopt              take it over, and sync");
        println!("  lsync vault enable {name}               let the service run it");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_comes_from_where_the_file_lies() {
        let (s, f) = folder_of(Path::new("/x/Vault/.obsidian/plugins/obsidian-livesync/data.json"));
        assert_eq!((s, f.as_deref()), (Source::Plugin, Some(Path::new("/x/Vault"))));
        // Obsidian's config folder may have another name.
        let (s, f) = folder_of(Path::new("/x/V/.config-dir/plugins/obsidian-livesync/data.json"));
        assert_eq!((s, f.as_deref()), (Source::Plugin, Some(Path::new("/x/V"))));
        let (s, f) = folder_of(Path::new("/srv/notes/.livesync/settings.json"));
        assert_eq!((s, f.as_deref()), (Source::Cli, Some(Path::new("/srv/notes"))));
        assert_eq!(folder_of(Path::new("/tmp/data.json")), (Source::Unknown, None));
        assert_eq!(folder_of(Path::new("/x/plugins/p/data.json")).0, Source::Unknown);
    }

    #[test]
    fn names_from_folders() {
        assert_eq!(name_from("My Notes").as_deref(), Some("my-notes"));
        assert_eq!(name_from("work.notes_2").as_deref(), Some("work.notes_2"));
        assert_eq!(name_from("Ærlig").as_deref(), Some("rlig"));
        assert_eq!(name_from("---"), None);
        assert_eq!(name_from("ÆØÅ"), None);
    }

    #[test]
    fn remote_profiles_parse_as_upstream_serialises_them() {
        let m = parse_profile("sls+https://us%40er:p%3Aa%2Fss%20w@couch.example.invalid:6984/base/?db=my-db").unwrap();
        assert_eq!(m["couchDB_URI"], "https://couch.example.invalid:6984/base/");
        assert_eq!(m["couchDB_USER"], "us@er");
        assert_eq!(m["couchDB_PASSWORD"], "p:a/ss w");
        assert_eq!(m["couchDB_DBNAME"], "my-db");
        assert_eq!(m["useJWT"], false);
        let m = parse_profile("sls+http://couch.local/?db=d&useJWT=true&headers=X").unwrap();
        assert_eq!(m["couchDB_URI"], "http://couch.local");
        assert_eq!(
            (m["useJWT"].as_bool(), m["couchDB_CustomHeaders"].as_str()),
            (Some(true), Some("X"))
        );
        assert!(parse_profile("sls+s3://k:s@s3.invalid/?bucket=b")
            .unwrap_err()
            .contains("only with CouchDB"));
        assert!(parse_profile("sls+p2p://room").unwrap_err().contains("peer-to-peer"));
        assert!(parse_profile("https://x").is_err());
    }

    #[test]
    fn unknown_encryption_formats_are_named() {
        let mut d = Decryptor::new("*");
        assert!(d.decrypt("%=AAAA").unwrap_err().contains("older format"));
        assert!(d.decrypt("plain").unwrap_err().contains("not in an encrypted format"));
        assert!(d.decrypt("%$AAAA").unwrap_err().contains("too short"));
    }
}
