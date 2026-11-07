//! Where account passwords live: the `SecretStore` seam (spec D13).
//!
//! An account names its secret with a reference, `STORE:KEY`:
//!
//! - `file:KEY`: the key in `secrets.toml` beside `lsync.toml` (mode 0600).
//! - `keyring:KEY`: the system keyring, the macOS Keychain or the Secret
//!   Service (see [`crate::keyring`]).
//! - `credential:NAME`: a systemd credential, the file `NAME` in
//!   `$CREDENTIALS_DIRECTORY` (`LoadCredential=`, `LoadCredentialEncrypted=`).
//!   Root can write one for the system service (`systemd-creds encrypt`
//!   into `/etc/credstore.encrypted`), through a [`CredentialTool`].
//! - `env:VAR`: the environment variable `VAR`.
//!
//! A keyring can be locked when a service starts; [`Availability::Locked`]
//! and [`SecretError::Locked`] say "wait and ask again", as opposed to an
//! error.
//!
//! A secret never reaches `Debug` output, logs or error messages: [`Secret`]
//! shows as `***`, and errors name the store and key, never the value.

use crate::edit::{write_private, ConfigLock};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A password, kept out of `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Secret {
        Secret(s.into())
    }

    /// The value, for handing to the remote client.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Which store a reference points into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StoreKind {
    File,
    Keyring,
    Credential,
    Env,
}

impl StoreKind {
    pub fn prefix(self) -> &'static str {
        match self {
            StoreKind::File => "file",
            StoreKind::Keyring => "keyring",
            StoreKind::Credential => "credential",
            StoreKind::Env => "env",
        }
    }
}

/// `STORE:KEY`, as written in an account's `secret`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretRef {
    pub kind: StoreKind,
    pub key: String,
}

impl SecretRef {
    pub fn file(key: &str) -> SecretRef {
        SecretRef {
            kind: StoreKind::File,
            key: key.to_string(),
        }
    }

    /// Errors never quote the value: a password typed into `secret` by
    /// mistake must not come back in a message.
    pub fn parse(s: &str) -> Result<SecretRef, String> {
        const FORM: &str =
            "secret must be STORE:KEY: file:KEY (secrets.toml), keyring:KEY (the macOS Keychain or the Secret Service), credential:NAME (systemd credentials) or env:VAR";
        let Some((store, key)) = s.split_once(':') else {
            return Err(FORM.into());
        };
        let kind = match store {
            "file" => StoreKind::File,
            "keyring" => StoreKind::Keyring,
            "credential" => StoreKind::Credential,
            "env" => StoreKind::Env,
            _ => return Err(format!("unknown secret store; {FORM}")),
        };
        let (ok, form) = match kind {
            StoreKind::File | StoreKind::Keyring => (
                !key.is_empty() && !key.chars().any(|c| c.is_control()),
                "a key that is not empty",
            ),
            StoreKind::Credential => (
                !key.is_empty()
                    && !key.contains('/')
                    && key != "."
                    && key != ".."
                    && !key.chars().any(|c| c.is_control()),
                "a credential name without '/'",
            ),
            StoreKind::Env => (
                key.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "a variable name of letters, digits and '_', not starting with a digit",
            ),
        };
        if !ok {
            return Err(format!("secret {}:KEY needs {form}", kind.prefix()));
        }
        Ok(SecretRef {
            kind,
            key: key.to_string(),
        })
    }
}

impl std::fmt::Display for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.kind.prefix(), self.key)
    }
}

/// Whether a store can be asked now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Ready,
    /// Present but locked (a keyring before login); ask again later.
    Locked(String),
    /// Not on this system, or not set up.
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// The store is locked for now ("waiting for the keyring"); retry.
    #[error("{0}")]
    Locked(String),
    #[error("{0}")]
    Unavailable(String),
    /// The store cannot be written from here (credentials, environment).
    #[error("{0}")]
    ReadOnly(String),
    #[error("{0}")]
    Failed(String),
}

impl SecretError {
    /// Worth retrying without a change of configuration.
    pub fn is_temporary(&self) -> bool {
        matches!(self, SecretError::Locked(_))
    }
}

/// One backend.
pub trait SecretStore: Send + Sync {
    fn kind(&self) -> StoreKind;
    /// Where it keeps its secrets, for `lsync account ls` and `lsync doctor`.
    fn location(&self) -> String;
    fn availability(&self) -> Availability;
    /// `Ok(None)` when the store works but has no such secret.
    fn get(&self, key: &str) -> Result<Option<Secret>, SecretError>;
    fn writable(&self) -> bool;
    fn set(&self, key: &str, value: &Secret) -> Result<(), SecretError>;
    /// Whether there was a secret to delete.
    fn delete(&self, key: &str) -> Result<bool, SecretError>;
}

/// `secrets.toml`: a flat table of `"KEY" = "secret"`, mode 0600, beside
/// `lsync.toml`. Refused while anyone else can read it.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: PathBuf) -> FileStore {
        FileStore { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_doc(&self) -> Result<Option<toml_edit::DocumentMut>, SecretError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(SecretError::Failed(format!("reading {}: {e}", self.path.display()))),
        };
        self.check_mode()?;
        let doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
            SecretError::Failed(format!(
                "parsing {}: {}",
                self.path.display(),
                crate::toml_error(&text, e.message(), e.span())
            ))
        })?;
        for (k, item) in doc.iter() {
            if item.as_str().is_none() {
                return Err(SecretError::Failed(format!(
                    "{}: {k:?} is not a string; the file holds only \"KEY\" = \"secret\" lines",
                    self.path.display()
                )));
            }
        }
        Ok(Some(doc))
    }

    #[cfg(unix)]
    fn check_mode(&self) -> Result<(), SecretError> {
        use std::os::unix::fs::PermissionsExt;
        let md = std::fs::metadata(&self.path)
            .map_err(|e| SecretError::Failed(format!("reading {}: {e}", self.path.display())))?;
        let mode = md.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(SecretError::Unavailable(format!(
                "{} can be read by other users (mode {mode:o}); run `chmod 600 {}`",
                self.path.display(),
                self.path.display()
            )));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn check_mode(&self) -> Result<(), SecretError> {
        Ok(())
    }

    fn lock(&self) -> Result<ConfigLock, SecretError> {
        ConfigLock::acquire(&self.path).map_err(SecretError::Failed)
    }

    fn write_doc(&self, doc: &toml_edit::DocumentMut) -> Result<(), SecretError> {
        write_private(&self.path, &doc.to_string())
            .map_err(|e| SecretError::Failed(format!("writing {}: {e}", self.path.display())))
    }

    /// Every key, for tests and migration checks.
    pub fn keys(&self) -> Result<Vec<String>, SecretError> {
        Ok(self
            .read_doc()?
            .map(|d| d.iter().map(|(k, _)| k.to_string()).collect())
            .unwrap_or_default())
    }
}

const FILE_HEADER: &str = "# lsync secrets: \"KEY\" = \"password\", one per line.\n# Keep this file mode 0600. Accounts in lsync.toml refer to a key here with secret = \"file:KEY\".\n\n";

impl SecretStore for FileStore {
    fn kind(&self) -> StoreKind {
        StoreKind::File
    }

    fn location(&self) -> String {
        self.path.display().to_string()
    }

    fn availability(&self) -> Availability {
        match self.read_doc() {
            Ok(_) => Availability::Ready,
            Err(e) => Availability::Unavailable(e.to_string()),
        }
    }

    fn get(&self, key: &str) -> Result<Option<Secret>, SecretError> {
        Ok(self
            .read_doc()?
            .and_then(|d| d.get(key).and_then(|v| v.as_str()).map(Secret::new)))
    }

    fn writable(&self) -> bool {
        true
    }

    fn set(&self, key: &str, value: &Secret) -> Result<(), SecretError> {
        let _lock = self.lock()?;
        let mut doc = match self.read_doc()? {
            Some(d) => d,
            None => FILE_HEADER.parse().expect("the header parses"),
        };
        doc[key] = toml_edit::value(value.expose());
        self.write_doc(&doc)
    }

    fn delete(&self, key: &str) -> Result<bool, SecretError> {
        let _lock = self.lock()?;
        let Some(mut doc) = self.read_doc()? else {
            return Ok(false);
        };
        let had = doc.remove(key).is_some();
        if had {
            self.write_doc(&doc)?;
        }
        Ok(had)
    }
}

/// Encrypts and decrypts systemd credentials (`systemd-creds`); injected,
/// so the tests never need root or the host key.
pub trait CredentialTool: Send + Sync {
    /// Encrypt `value` as the credential `name` into the file `out`.
    fn encrypt(&self, name: &str, value: &Secret, out: &Path) -> Result<(), String>;
    /// Decrypt the credential `name` in the file `path`.
    fn decrypt(&self, name: &str, path: &Path) -> Result<Secret, String>;
}

/// Where root keeps the system service's credentials, and how.
pub struct CredentialWriter {
    /// `/etc/credstore.encrypted`, where `LoadCredentialEncrypted=` reads.
    pub dir: PathBuf,
    pub tool: Arc<dyn CredentialTool>,
}

/// systemd credentials: one file per secret in `$CREDENTIALS_DIRECTORY`.
/// With a [`CredentialWriter`] (root, for the system service), outside a
/// unit they are read from and written to its folder instead.
pub struct CredentialStore {
    dir: Option<PathBuf>,
    writer: Option<CredentialWriter>,
}

impl CredentialStore {
    pub fn new(dir: Option<PathBuf>) -> CredentialStore {
        CredentialStore { dir, writer: None }
    }

    pub fn with_writer(dir: Option<PathBuf>, writer: CredentialWriter) -> CredentialStore {
        CredentialStore {
            dir,
            writer: Some(writer),
        }
    }

    /// The writer, when this process reads credentials from its folder
    /// (not from inside a unit).
    fn stored(&self) -> Option<&CredentialWriter> {
        self.writer.as_ref().filter(|_| self.dir.is_none())
    }
}

impl SecretStore for CredentialStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Credential
    }

    fn location(&self) -> String {
        match (&self.dir, self.stored()) {
            (Some(d), _) => d.display().to_string(),
            (None, Some(w)) => w.dir.display().to_string(),
            (None, None) => "$CREDENTIALS_DIRECTORY (not set)".into(),
        }
    }

    fn availability(&self) -> Availability {
        if self.stored().is_some() {
            return Availability::Ready;
        }
        match &self.dir {
            Some(d) if d.is_dir() => Availability::Ready,
            Some(d) => Availability::Unavailable(format!("the credentials folder {} does not exist", d.display())),
            None => Availability::Unavailable(
                "CREDENTIALS_DIRECTORY is not set: systemd credentials exist only inside a unit with LoadCredential= or LoadCredentialEncrypted=".into(),
            ),
        }
    }

    fn get(&self, key: &str) -> Result<Option<Secret>, SecretError> {
        if let Some(w) = self.stored() {
            let path = w.dir.join(key);
            if !path.exists() {
                return Ok(None);
            }
            return w
                .tool
                .decrypt(key, &path)
                .map(Some)
                .map_err(|e| SecretError::Failed(format!("decrypting the credential {key:?}: {e}")));
        }
        let Some(dir) = &self.dir else {
            return Err(SecretError::Unavailable(format!(
                "the password is the systemd credential {key:?}, but CREDENTIALS_DIRECTORY is not set (run lsync from a unit with LoadCredential={key}:... or LoadCredentialEncrypted={key}:...)"
            )));
        };
        match std::fs::read_to_string(dir.join(key)) {
            // A credential file often ends with a newline; a password never does.
            Ok(s) => Ok(Some(Secret::new(s.trim_end_matches(['\n', '\r'])))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SecretError::Failed(format!("reading the credential {key:?}: {e}"))),
        }
    }

    fn writable(&self) -> bool {
        self.stored().is_some()
    }

    fn set(&self, key: &str, value: &Secret) -> Result<(), SecretError> {
        let Some(w) = self.stored() else {
            return Err(SecretError::ReadOnly(format!(
                "systemd credential {key:?} is set in the service unit (systemd-creds encrypt), or by root with `sudo lsync --system account passwd`"
            )));
        };
        std::fs::create_dir_all(&w.dir)
            .map_err(|e| SecretError::Failed(format!("creating {}: {e}", w.dir.display())))?;
        // Into a new file first: a failed encryption leaves the old one.
        let path = w.dir.join(key);
        let tmp = w.dir.join(format!(".{key}.tmp-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let done = w
            .tool
            .encrypt(key, value, &tmp)
            .map_err(|e| SecretError::Failed(format!("encrypting the credential {key:?}: {e}")))
            .and_then(|()| {
                std::fs::rename(&tmp, &path)
                    .map_err(|e| SecretError::Failed(format!("writing {}: {e}", path.display())))
            });
        if done.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        done
    }

    fn delete(&self, key: &str) -> Result<bool, SecretError> {
        let Some(w) = self.stored() else {
            self.set(key, &Secret::new(""))?;
            return Ok(false);
        };
        match std::fs::remove_file(w.dir.join(key)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(SecretError::Failed(format!("removing the credential {key:?}: {e}"))),
        }
    }
}

/// Reads an environment variable; how the variable is looked up is
/// injected, so tests never touch the process environment.
pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct EnvStore {
    lookup: EnvLookup,
}

impl EnvStore {
    pub fn new(lookup: EnvLookup) -> EnvStore {
        EnvStore { lookup }
    }

    pub fn process() -> EnvStore {
        EnvStore::new(Arc::new(|k| std::env::var(k).ok()))
    }
}

impl SecretStore for EnvStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Env
    }

    fn location(&self) -> String {
        "the environment".into()
    }

    fn availability(&self) -> Availability {
        Availability::Ready
    }

    fn get(&self, key: &str) -> Result<Option<Secret>, SecretError> {
        Ok((self.lookup)(key).filter(|v| !v.is_empty()).map(Secret::new))
    }

    fn writable(&self) -> bool {
        false
    }

    fn set(&self, key: &str, _value: &Secret) -> Result<(), SecretError> {
        Err(SecretError::ReadOnly(format!(
            "the password comes from the environment variable {key}; set it where lsync is started"
        )))
    }

    fn delete(&self, key: &str) -> Result<bool, SecretError> {
        self.set(key, &Secret::new(""))?;
        Ok(false)
    }
}

/// Every store this process can use, by kind.
pub struct SecretStores {
    stores: BTreeMap<StoreKind, Box<dyn SecretStore>>,
}

impl SecretStores {
    pub fn new(stores: Vec<Box<dyn SecretStore>>) -> SecretStores {
        SecretStores {
            stores: stores.into_iter().map(|s| (s.kind(), s)).collect(),
        }
    }

    /// The stores of this process: `secrets_file` beside the config, the
    /// system keyring, `$CREDENTIALS_DIRECTORY`, and the environment.
    pub fn standard(secrets_file: PathBuf) -> SecretStores {
        SecretStores::with_credentials(secrets_file, None)
    }

    /// As [`SecretStores::standard`], with credentials root can write.
    pub fn with_credentials(secrets_file: PathBuf, writer: Option<CredentialWriter>) -> SecretStores {
        let creds = std::env::var_os("CREDENTIALS_DIRECTORY")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let creds = match writer {
            Some(w) => CredentialStore::with_writer(creds, w),
            None => CredentialStore::new(creds),
        };
        SecretStores::new(vec![
            Box::new(FileStore::new(secrets_file)),
            Box::new(crate::keyring::KeyringStore::new()),
            Box::new(creds),
            Box::new(EnvStore::process()),
        ])
    }

    pub fn store(&self, kind: StoreKind) -> Result<&dyn SecretStore, SecretError> {
        self.stores.get(&kind).map(|b| b.as_ref()).ok_or_else(|| {
            SecretError::Unavailable(format!("the {} secret store is not available here", kind.prefix()))
        })
    }

    pub fn get(&self, r: &SecretRef) -> Result<Option<Secret>, SecretError> {
        let s = self.store(r.kind)?;
        if let Availability::Locked(why) = s.availability() {
            return Err(SecretError::Locked(why));
        }
        s.get(&r.key)
    }

    pub fn set(&self, r: &SecretRef, value: &Secret) -> Result<(), SecretError> {
        self.store(r.kind)?.set(&r.key, value)
    }

    pub fn delete(&self, r: &SecretRef) -> Result<bool, SecretError> {
        self.store(r.kind)?.delete(&r.key)
    }

    pub fn writable(&self, r: &SecretRef) -> bool {
        self.store(r.kind).is_ok_and(|s| s.writable())
    }

    /// "file /path/secrets.toml", "env LSYNC_PASSWORD_HOME", ...
    pub fn describe(&self, r: &SecretRef) -> String {
        match (r.kind, self.store(r.kind)) {
            (StoreKind::Env, _) => format!("environment variable {}", r.key),
            (StoreKind::File, Ok(s)) => format!("key {:?} in {}", r.key, s.location()),
            (StoreKind::Keyring, Ok(s)) => format!("key {:?} in {}", r.key, s.location()),
            (StoreKind::Credential, Ok(s)) => format!("systemd credential {:?} in {}", r.key, s.location()),
            (_, Err(e)) => e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_parse_and_reject() {
        assert_eq!(SecretRef::parse("file:home").unwrap(), SecretRef::file("home"));
        assert_eq!(SecretRef::parse("env:LSYNC_PW_1").unwrap().kind, StoreKind::Env);
        assert_eq!(
            SecretRef::parse("credential:lsync-home").unwrap().kind,
            StoreKind::Credential
        );
        assert_eq!(SecretRef::parse("keyring:home").unwrap().kind, StoreKind::Keyring);
        assert!(SecretRef::parse("keyring:").is_err());
        assert!(SecretRef::parse("home").is_err());
        assert!(SecretRef::parse("keychain:x")
            .unwrap_err()
            .contains("unknown secret store"));
        assert!(SecretRef::parse("env:1BAD").is_err());
        assert!(SecretRef::parse("env:A-B").is_err());
        assert!(SecretRef::parse("credential:../x").is_err());
        assert!(SecretRef::parse("file:").is_err());
        assert_eq!(SecretRef::parse("env:X").unwrap().to_string(), "env:X");
        // A password typed in by mistake is never quoted back.
        for leaky in [
            "Hunter2-Sup3rSecret",
            "Hunter2:Sup3rSecret",
            "env:Hunter2-Sup3rSecret",
            "credential:Hunter2/Sup3rSecret",
        ] {
            let e = SecretRef::parse(leaky).unwrap_err();
            assert!(!e.contains("Hunter2") && !e.contains("Sup3r"), "{e}");
            assert!(e.contains("STORE:KEY") || e.contains(":KEY needs"), "{e}");
        }
    }

    #[test]
    fn secret_debug_is_redacted() {
        assert_eq!(format!("{:?}", Secret::new("hunter2")), "Secret(***)");
    }

    #[cfg(unix)]
    #[test]
    fn file_store_is_private_keeps_comments_and_refuses_open_modes() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("lsync/secrets.toml");
        let s = FileStore::new(path.clone());
        assert_eq!(s.get("a").unwrap(), None);
        s.set("a", &Secret::new("one")).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("# my note\n");
        std::fs::write(&path, &text).unwrap();
        s.set("b", &Secret::new("two \"quoted\"")).unwrap();
        assert_eq!(s.get("b").unwrap().unwrap().expose(), "two \"quoted\"");
        assert!(std::fs::read_to_string(&path).unwrap().contains("# my note"));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(s.delete("a").unwrap());
        assert!(!s.delete("a").unwrap());
        assert_eq!(s.keys().unwrap(), vec!["b".to_string()]);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = s.get("b").unwrap_err();
        assert!(e.to_string().contains("chmod 600"), "{e}");
        assert!(!e.to_string().contains("two"), "no secret in the message");
        assert!(matches!(s.availability(), Availability::Unavailable(_)));

        // A broken file is reported without quoting its lines.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, "a = \"leaky-secret\"\nb = \"unterminated\n").unwrap();
        let e = s.get("a").unwrap_err().to_string();
        assert!(
            e.contains("line 2") && !e.contains("leaky") && !e.contains("unterminated"),
            "{e}"
        );
    }

    #[test]
    fn credentials_and_environment_are_read_only() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("lsync-home"), "pw\n").unwrap();
        let c = CredentialStore::new(Some(d.path().to_path_buf()));
        assert_eq!(c.get("lsync-home").unwrap().unwrap().expose(), "pw");
        assert_eq!(c.get("other").unwrap(), None);
        assert!(matches!(c.set("x", &Secret::new("y")), Err(SecretError::ReadOnly(_))));
        let none = CredentialStore::new(None);
        assert!(matches!(none.get("x"), Err(SecretError::Unavailable(_))));
        assert!(matches!(none.availability(), Availability::Unavailable(_)));

        let e = EnvStore::new(Arc::new(|k| (k == "PW").then(|| "from-env".to_string())));
        assert_eq!(e.get("PW").unwrap().unwrap().expose(), "from-env");
        assert_eq!(e.get("NOPE").unwrap(), None);
        assert!(!e.writable());
    }

    /// "Encrypts" by reversing, and records the names it was given.
    struct FakeCreds;
    impl CredentialTool for FakeCreds {
        fn encrypt(&self, name: &str, value: &Secret, out: &Path) -> Result<(), String> {
            if value.expose() == "fail" {
                return Err("no host key".into());
            }
            let text: String = value.expose().chars().rev().collect();
            std::fs::write(out, format!("{name}\n{text}")).map_err(|e| e.to_string())
        }
        fn decrypt(&self, name: &str, path: &Path) -> Result<Secret, String> {
            let t = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let (n, v) = t.split_once('\n').unwrap();
            assert_eq!(n, name, "the name is checked at decryption");
            Ok(Secret::new(v.chars().rev().collect::<String>()))
        }
    }

    #[test]
    fn root_writes_the_system_services_credentials_through_the_tool() {
        let d = tempfile::tempdir().unwrap();
        let w = CredentialWriter {
            dir: d.path().join("credstore.encrypted"),
            tool: Arc::new(FakeCreds),
        };
        let c = CredentialStore::with_writer(None, w);
        assert!(c.writable());
        assert_eq!(c.availability(), Availability::Ready);
        assert_eq!(c.get("lsync-home").unwrap(), None);
        c.set("lsync-home", &Secret::new("pw1")).unwrap();
        let file = d.path().join("credstore.encrypted/lsync-home");
        assert!(!std::fs::read_to_string(&file).unwrap().contains("pw1"));
        assert_eq!(c.get("lsync-home").unwrap().unwrap().expose(), "pw1");
        // A failed encryption keeps the old credential and leaves no temp file.
        assert!(c.set("lsync-home", &Secret::new("fail")).is_err());
        assert_eq!(c.get("lsync-home").unwrap().unwrap().expose(), "pw1");
        assert_eq!(
            std::fs::read_dir(d.path().join("credstore.encrypted")).unwrap().count(),
            1
        );
        assert!(c.delete("lsync-home").unwrap());
        assert!(!c.delete("lsync-home").unwrap());

        // Inside a unit, the credentials folder is what counts, read-only.
        let unit = tempfile::tempdir().unwrap();
        std::fs::write(unit.path().join("lsync-home"), "from-unit\n").unwrap();
        let inside = CredentialStore::with_writer(
            Some(unit.path().to_path_buf()),
            CredentialWriter {
                dir: d.path().join("credstore.encrypted"),
                tool: Arc::new(FakeCreds),
            },
        );
        assert!(!inside.writable());
        assert_eq!(inside.get("lsync-home").unwrap().unwrap().expose(), "from-unit");
    }

    struct LockedStore;
    impl SecretStore for LockedStore {
        fn kind(&self) -> StoreKind {
            StoreKind::File
        }
        fn location(&self) -> String {
            "a keyring".into()
        }
        fn availability(&self) -> Availability {
            Availability::Locked("waiting for the keyring".into())
        }
        fn get(&self, _key: &str) -> Result<Option<Secret>, SecretError> {
            unreachable!("not asked while locked")
        }
        fn writable(&self) -> bool {
            true
        }
        fn set(&self, _key: &str, _value: &Secret) -> Result<(), SecretError> {
            Err(SecretError::Locked("waiting for the keyring".into()))
        }
        fn delete(&self, _key: &str) -> Result<bool, SecretError> {
            Ok(false)
        }
    }

    #[test]
    fn a_locked_store_says_retry() {
        let stores = SecretStores::new(vec![Box::new(LockedStore)]);
        let e = stores.get(&SecretRef::file("a")).unwrap_err();
        assert!(e.is_temporary());
        assert_eq!(e.to_string(), "waiting for the keyring");
        assert!(matches!(
            stores.get(&SecretRef::parse("env:X").unwrap()),
            Err(SecretError::Unavailable(_))
        ));
    }
}
