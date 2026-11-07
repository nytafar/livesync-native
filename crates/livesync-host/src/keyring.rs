//! `keyring:KEY`: the operating system's keyring (spec D13). The macOS
//! Keychain (a generic password, service `lsync`, account `KEY`) or, on
//! Linux, the Secret Service over the session D-Bus (gnome-keyring,
//! KWallet, KeePassXC), in the default collection, as an item with the
//! attributes `service=lsync` and `username=KEY`: `secret-tool lookup
//! service lsync username KEY` finds it.
//!
//! A keyring that is still locked when the service starts (a desktop before
//! login) is [`Availability::Locked`]: "waiting for the keyring", and the
//! service asks again. The Secret Service is never asked to prompt. The
//! Keychain shows no dialog either (its unlock prompt, or "allow lsync to
//! use this item" after a new build), unless [`allow_prompts`] said a user
//! at a terminal can answer it: never in the service.
//!
//! Elsewhere (Android, other systems) the store is unavailable.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::secret::Secret;
use crate::secret::{Availability, SecretError, SecretStore, StoreKind};

/// The service name items are filed under.
pub const SERVICE: &str = "lsync";
/// How every "locked" message starts, so the service can tell a vault that
/// waits for the keyring from one that failed.
pub const WAITING: &str = "waiting for the keyring";

static PROMPTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Let the keyring show its dialogs (the macOS Keychain's unlock and
/// "allow" prompts), for a command a user runs at a terminal. Off unless
/// called: a process nobody watches (the service) gets "waiting for the
/// keyring" instead of a dialog it would wait on. Set it before the first
/// keyring call; on macOS the first call fixes it for the process.
pub fn allow_prompts(on: bool) {
    PROMPTS.store(on, std::sync::atomic::Ordering::SeqCst);
}

pub fn prompts_allowed() -> bool {
    PROMPTS.load(std::sync::atomic::Ordering::SeqCst)
}

pub struct KeyringStore {
    #[cfg(target_os = "linux")]
    linux: linux::SecretService,
}

impl KeyringStore {
    pub fn new() -> KeyringStore {
        KeyringStore {
            #[cfg(target_os = "linux")]
            linux: linux::SecretService::new(),
        }
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        KeyringStore::new()
    }
}

#[cfg(target_os = "linux")]
impl SecretStore for KeyringStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Keyring
    }
    fn location(&self) -> String {
        "the Secret Service (the desktop keyring)".into()
    }
    fn availability(&self) -> Availability {
        self.linux.availability()
    }
    fn get(&self, key: &str) -> Result<Option<Secret>, SecretError> {
        self.linux.get(key)
    }
    fn writable(&self) -> bool {
        true
    }
    fn set(&self, key: &str, value: &Secret) -> Result<(), SecretError> {
        self.linux.set(key, value)
    }
    fn delete(&self, key: &str) -> Result<bool, SecretError> {
        self.linux.delete(key)
    }
}

#[cfg(target_os = "macos")]
impl SecretStore for KeyringStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Keyring
    }
    fn location(&self) -> String {
        "the macOS Keychain".into()
    }
    fn availability(&self) -> Availability {
        macos::availability()
    }
    fn get(&self, key: &str) -> Result<Option<Secret>, SecretError> {
        macos::get(key)
    }
    fn writable(&self) -> bool {
        true
    }
    fn set(&self, key: &str, value: &Secret) -> Result<(), SecretError> {
        macos::set(key, value)
    }
    fn delete(&self, key: &str) -> Result<bool, SecretError> {
        macos::delete(key)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl SecretStore for KeyringStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Keyring
    }
    fn location(&self) -> String {
        "the system keyring (none on this system)".into()
    }
    fn availability(&self) -> Availability {
        Availability::Unavailable("there is no system keyring lsync can use here".into())
    }
    fn get(&self, _key: &str) -> Result<Option<crate::secret::Secret>, SecretError> {
        Err(SecretError::Unavailable(
            "there is no system keyring lsync can use here; keep the password in file: or env:".into(),
        ))
    }
    fn writable(&self) -> bool {
        false
    }
    fn set(&self, key: &str, _value: &crate::secret::Secret) -> Result<(), SecretError> {
        self.get(key).map(|_| ())
    }
    fn delete(&self, key: &str) -> Result<bool, SecretError> {
        self.get(key).map(|_| false)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Availability, Secret, SecretError, SERVICE, WAITING};
    use secret_service::{Collection, EncryptionType, Error};
    use std::collections::HashMap;

    pub struct SecretService;

    fn attributes(key: &str) -> HashMap<&str, &str> {
        HashMap::from([("service", SERVICE), ("username", key)])
    }

    fn failed(what: &str, e: Error) -> SecretError {
        match e {
            Error::Locked => locked(),
            Error::Unavailable => SecretError::Unavailable(unavailable()),
            Error::NoResult => SecretError::Unavailable(
                "the Secret Service has no default keyring; create one (for example in Seahorse), or keep the password in file:".into(),
            ),
            e => SecretError::Failed(format!("the Secret Service, {what}: {e}")),
        }
    }

    fn locked() -> SecretError {
        SecretError::Locked(format!(
            "{WAITING}: the default keyring is locked; unlock it (log in to the desktop) and lsync goes on"
        ))
    }

    fn unavailable() -> String {
        "no Secret Service on the session bus (no desktop session, or no keyring daemon); keep the password in file: or credential:".into()
    }

    /// Run `f` on a runtime of its own, on a thread of its own: the store
    /// is called from plain threads and from inside the service's runtime.
    fn on_runtime<T: Send + 'static>(
        f: impl FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>> + Send + 'static,
    ) -> Result<T, SecretError> {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| SecretError::Failed(format!("the Secret Service: {e}")))?;
            Ok(rt.block_on(f()))
        })
        .join()
        .map_err(|_| SecretError::Failed("the Secret Service: the client panicked".into()))?
    }

    async fn collection<'a>(ss: &'a secret_service::SecretService<'a>) -> Result<Collection<'a>, SecretError> {
        let c = ss
            .get_default_collection()
            .await
            .map_err(|e| failed("opening the keyring", e))?;
        if c.is_locked().await.map_err(|e| failed("opening the keyring", e))? {
            return Err(locked());
        }
        Ok(c)
    }

    async fn connect() -> Result<secret_service::SecretService<'static>, SecretError> {
        secret_service::SecretService::connect(EncryptionType::Dh)
            .await
            .map_err(|e| match e {
                Error::Zbus(_) | Error::ZbusFdo(_) | Error::Unavailable => SecretError::Unavailable(unavailable()),
                e => failed("connecting", e),
            })
    }

    impl SecretService {
        pub fn new() -> SecretService {
            SecretService
        }

        pub fn availability(&self) -> Availability {
            let r = on_runtime(|| {
                Box::pin(async {
                    let ss = connect().await?;
                    collection(&ss).await.map(|_| ())
                })
            });
            match r.and_then(|x| x) {
                Ok(()) => Availability::Ready,
                Err(SecretError::Locked(why)) => Availability::Locked(why),
                Err(e) => Availability::Unavailable(e.to_string()),
            }
        }

        pub fn get(&self, key: &str) -> Result<Option<Secret>, SecretError> {
            let key = key.to_string();
            on_runtime(move || {
                Box::pin(async move {
                    let ss = connect().await?;
                    let c = collection(&ss).await?;
                    let items = c
                        .search_items(attributes(&key))
                        .await
                        .map_err(|e| failed("searching", e))?;
                    let Some(item) = items.first() else {
                        return Ok(None);
                    };
                    if item.is_locked().await.map_err(|e| failed("reading", e))? {
                        return Err(locked());
                    }
                    let bytes = item.get_secret().await.map_err(|e| failed("reading", e))?;
                    let s = String::from_utf8(bytes).map_err(|_| {
                        SecretError::Failed(format!("the keyring's password for {key:?} is not UTF-8 text"))
                    })?;
                    Ok(Some(Secret::new(s)))
                })
            })?
        }

        pub fn set(&self, key: &str, value: &Secret) -> Result<(), SecretError> {
            let (key, value) = (key.to_string(), value.clone());
            on_runtime(move || {
                Box::pin(async move {
                    let ss = connect().await?;
                    let c = collection(&ss).await?;
                    c.create_item(
                        &format!("lsync: {key}"),
                        attributes(&key),
                        value.expose().as_bytes(),
                        true,
                        "text/plain",
                    )
                    .await
                    .map_err(|e| failed("storing", e))?;
                    Ok(())
                })
            })?
        }

        pub fn delete(&self, key: &str) -> Result<bool, SecretError> {
            let key = key.to_string();
            on_runtime(move || {
                Box::pin(async move {
                    let ss = connect().await?;
                    let c = collection(&ss).await?;
                    let items = c
                        .search_items(attributes(&key))
                        .await
                        .map_err(|e| failed("searching", e))?;
                    for item in &items {
                        item.delete().await.map_err(|e| failed("deleting", e))?;
                    }
                    Ok(!items.is_empty())
                })
            })?
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{Availability, Secret, SecretError, SERVICE, WAITING};
    use security_framework::os::macos::keychain::SecKeychain;
    use security_framework::passwords;

    /// No Keychain dialog in this process unless prompts are allowed: a
    /// locked keychain, or an item that wants the user's "allow", is then
    /// errSecInteractionNotAllowed. For the whole process, never turned
    /// back on (the setting is global, and calls may overlap).
    fn no_dialogs() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            if super::prompts_allowed() {
                return;
            }
            match SecKeychain::disable_user_interaction() {
                Ok(lock) => std::mem::forget(lock),
                Err(e) => tracing::warn!("the Keychain may show dialogs: {e}"),
            }
        });
    }

    /// errSecItemNotFound.
    const NOT_FOUND: i32 = -25300;
    /// errSecInteractionNotAllowed: the keychain is locked and no prompt may
    /// be shown (a LaunchAgent before login).
    const INTERACTION_NOT_ALLOWED: i32 = -25308;
    /// errSecAuthFailed, errSecUserCanceled.
    const DENIED: [i32; 2] = [-25293, -128];
    /// errSecNoDefaultKeychain, errSecNotAvailable.
    const NO_KEYCHAIN: [i32; 2] = [-25307, -25291];

    fn failed(what: &str, e: security_framework::base::Error) -> SecretError {
        let code = e.code();
        if code == INTERACTION_NOT_ALLOWED {
            return SecretError::Locked(format!(
                "{WAITING}: the login keychain is locked, or this lsync is not allowed to read the item yet (a new build); log in, or run `lsync doctor` in a terminal and allow it, and lsync goes on"
            ));
        }
        if NO_KEYCHAIN.contains(&code) {
            return SecretError::Unavailable(format!("no keychain to use ({e}); keep the password in file:"));
        }
        if DENIED.contains(&code) {
            return SecretError::Failed(format!(
                "the Keychain refused {what} ({e}); allow lsync when macOS asks"
            ));
        }
        SecretError::Failed(format!("the Keychain, {what}: {e}"))
    }

    pub fn availability() -> Availability {
        no_dialogs();
        // Asking for an item that is not there opens the keychain without
        // reading any secret.
        match passwords::get_generic_password(SERVICE, "lsync-availability-probe") {
            Ok(_) => Availability::Ready,
            Err(e) if e.code() == NOT_FOUND => Availability::Ready,
            Err(e) => match failed("opening", e) {
                SecretError::Locked(why) => Availability::Locked(why),
                e => Availability::Unavailable(e.to_string()),
            },
        }
    }

    pub fn get(key: &str) -> Result<Option<Secret>, SecretError> {
        no_dialogs();
        match passwords::get_generic_password(SERVICE, key) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(|s| Some(Secret::new(s)))
                .map_err(|_| SecretError::Failed(format!("the Keychain's password for {key:?} is not UTF-8 text"))),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(failed("reading", e)),
        }
    }

    pub fn set(key: &str, value: &Secret) -> Result<(), SecretError> {
        no_dialogs();
        passwords::set_generic_password(SERVICE, key, value.expose().as_bytes()).map_err(|e| failed("storing", e))
    }

    pub fn delete(key: &str) -> Result<bool, SecretError> {
        no_dialogs();
        match passwords::delete_generic_password(SERVICE, key) {
            Ok(()) => Ok(true),
            Err(e) if e.code() == NOT_FOUND => Ok(false),
            Err(e) => Err(failed("deleting", e)),
        }
    }
}
