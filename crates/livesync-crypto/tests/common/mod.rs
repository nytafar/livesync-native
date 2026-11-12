#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use livesync_crypto::{CryptoError, KeyContext, RandomSource};
use serde_json::Value;

/// The pinned versions (upstream.env).
pub const COMMONLIB: &str = livesync_model::upstream::COMMONLIB;
pub const WHEELS: &str = livesync_model::upstream::OCTAGONAL_WHEELS;

/// A fixture file from `tools/golden/crypto.mjs`, checked for the upstream versions.
pub fn fixture(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden")
        .join(name);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["meta"]["version"], COMMONLIB, "{name}: commonlib version");
    assert_eq!(v["upstream"]["commonlib"], COMMONLIB, "{name}: commonlib version");
    assert_eq!(
        v["upstream"]["octagonalWheels"], WHEELS,
        "{name}: octagonal-wheels version"
    );
    v
}

/// The seeded `crypto.getRandomValues` of crypto.mjs: xorshift32, one byte per step.
pub struct Xorshift(u32);

impl Xorshift {
    pub fn new(seed: u64) -> Self {
        Xorshift(seed as u32)
    }
}

impl RandomSource for Xorshift {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), CryptoError> {
        for b in buf {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            *b = self.0 as u8;
        }
        Ok(())
    }
}

/// Key contexts by fixture key name, derived once per test binary (PBKDF2 is slow).
pub fn key(keys: &Value, name: &str) -> KeyContext {
    static CACHE: OnceLock<Mutex<HashMap<String, KeyContext>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(k) = cache.lock().unwrap().get(name) {
        return k.clone();
    }
    let k = &keys[name];
    let salt = livesync_crypto::parse_pbkdf2_salt(&serde_json::json!({ "pbkdf2salt": k["pbkdf2Salt"] })).unwrap();
    let ctx = KeyContext::derive(k["passphrase"].as_str().unwrap(), &salt);
    cache.lock().unwrap().insert(name.to_string(), ctx.clone());
    ctx
}

/// Whether `err` is the error a fixture expects (`{"error": ...}`).
pub fn assert_error(err: &CryptoError, expected: &Value, what: &str) {
    let kind = expected["error"]
        .as_str()
        .unwrap_or_else(|| panic!("{what}: no error kind"));
    match (kind, err) {
        ("authentication", CryptoError::Authentication) => {}
        ("malformed", CryptoError::Malformed(_)) => {}
        (
            "rejected",
            CryptoError::Rejected {
                document_id,
                content_kind,
            },
        ) => {
            assert_eq!(content_kind.as_str(), expected["contentKind"], "{what}");
            if let Some(id) = expected["documentId"].as_str() {
                assert_eq!(document_id, id, "{what}");
            }
        }
        ("unsupported", CryptoError::Unsupported(u)) => {
            assert_eq!(format!("{u:?}"), expected["unsupported"].as_str().unwrap(), "{what}");
        }
        _ => panic!("{what}: expected {kind}, got {err:?}"),
    }
}
