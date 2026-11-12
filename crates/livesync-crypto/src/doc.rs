//! Document transforms: upstream's `incomingEncryptHKDF` (before a document is
//! written to the remote) and `outgoingDecryptHKDF` (after it is read), as
//! `getConfiguredFunctionsForEncryption` sets them up for `E2EEAlgorithm: "v2"`
//! without path obfuscation or internal-metadata encryption.
//!
//! What is transformed, keyed on `_id`:
//! - `h:+…` chunks and `syncinfo`: `data` is encrypted and `e_: true` set
//!   (reading reverses it and removes `e_`);
//! - a non-empty `eden` becomes `{"h:++encrypted-hkdf": {data, epoch: 999999}}`
//!   holding the JSON of the old eden (reading puts it back).
//!
//! Every other document passes through unchanged (after the unencrypted-content
//! policy, when reading).

use serde_json::{Map, Value};

use crate::error::{CryptoError, Unsupported};
use crate::ids;
use crate::key::{KeyContext, OsRandom, RandomSource, HKDF_PREFIX};
use crate::policy;

/// Encrypts documents for the remote and decrypts them back.
#[derive(Debug, Clone)]
pub struct DocumentCipher {
    key: KeyContext,
    accept_unencrypted_content: bool,
}

/// How `data` of a chunk or `syncinfo` is held (`getEncryptionVersion`).
enum DataState<'a> {
    Plain,
    Hkdf(&'a str),
    V1,
    Unknown,
}

fn data_state(doc: &Map<String, Value>) -> Result<DataState<'_>, CryptoError> {
    if doc.get("e_") != Some(&Value::Bool(true)) {
        return Ok(DataState::Plain);
    }
    let data = doc
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| CryptoError::malformed("encrypted data is not a string"))?;
    Ok(if data.starts_with(HKDF_PREFIX) {
        DataState::Hkdf(data)
    } else if data.starts_with('%') {
        DataState::V1
    } else {
        DataState::Unknown
    })
}

fn object(doc: Value) -> Result<(Map<String, Value>, String), CryptoError> {
    let Value::Object(map) = doc else {
        return Err(CryptoError::malformed("document is not a JSON object"));
    };
    let id = map
        .get("_id")
        .and_then(Value::as_str)
        .ok_or_else(|| CryptoError::malformed("document has no string _id"))?
        .to_string();
    Ok((map, id))
}

fn owns_data(id: &str) -> bool {
    ids::is_encrypted_chunk_id(id) || id == ids::SYNCINFO_ID
}

/// `eden` keys, as JavaScript's `in` and `Object.keys` see them; `None` when
/// there is no `eden`. Upstream throws a TypeError on a non-object eden.
enum Eden<'a> {
    Object(&'a Map<String, Value>),
    Array(&'a [Value]),
}

fn eden(doc: &Map<String, Value>) -> Result<Option<Eden<'_>>, CryptoError> {
    match doc.get("eden") {
        None => Ok(None),
        Some(Value::Object(m)) => Ok(Some(Eden::Object(m))),
        Some(Value::Array(a)) => Ok(Some(Eden::Array(a))),
        Some(_) => Err(CryptoError::malformed("eden is not an object")),
    }
}

impl DocumentCipher {
    /// `accept_unencrypted_content` is upstream's `acceptUnencryptedContent`
    /// (default false: plaintext content read from the remote is an error).
    pub fn new(key: KeyContext, accept_unencrypted_content: bool) -> Self {
        DocumentCipher {
            key,
            accept_unencrypted_content,
        }
    }

    pub fn key(&self) -> &KeyContext {
        &self.key
    }

    /// Encrypt a document before it is written to the remote (`incoming`).
    pub fn incoming(&self, doc: Value) -> Result<Value, CryptoError> {
        self.incoming_with(doc, &mut OsRandom)
    }

    /// [`Self::incoming`] with a caller-supplied random source. Draws happen in
    /// upstream's order: `data` first, then `eden`.
    pub fn incoming_with(&self, doc: Value, rng: &mut dyn RandomSource) -> Result<Value, CryptoError> {
        let (mut doc, id) = object(doc)?;
        if id.starts_with(ids::OBFUSCATED_PREFIX) {
            return Err(CryptoError::Unsupported(Unsupported::PathObfuscation));
        }
        if owns_data(&id) {
            match data_state(&doc)? {
                DataState::Plain => {
                    let data = doc
                        .get("data")
                        .and_then(Value::as_str)
                        .ok_or_else(|| CryptoError::malformed("data is not a string"))?;
                    let sealed = self.key.encrypt_with(data, rng)?;
                    doc.insert("data".into(), Value::String(sealed));
                    doc.insert("e_".into(), Value::Bool(true));
                }
                DataState::Hkdf(_) => {}
                DataState::V1 => return Err(CryptoError::Unsupported(Unsupported::V1Ciphertext)),
                // Upstream leaves it as it is.
                DataState::Unknown => {}
            }
        }
        let plain_eden = match eden(&doc)? {
            Some(Eden::Object(m)) if !m.is_empty() && !m.contains_key(ids::EDEN_ENCRYPTED_KEY_HKDF) => {
                Some(serde_json::to_string(m).expect("JSON value"))
            }
            Some(Eden::Array(a)) if !a.is_empty() => Some(serde_json::to_string(a).expect("JSON value")),
            _ => None,
        };
        if let Some(json) = plain_eden {
            let sealed = self.key.encrypt_with(&json, rng)?;
            let mut inner = Map::new();
            inner.insert("data".into(), Value::String(sealed));
            inner.insert("epoch".into(), Value::from(ids::EDEN_ENCRYPTED_EPOCH));
            let mut wrapped = Map::new();
            wrapped.insert(ids::EDEN_ENCRYPTED_KEY_HKDF.into(), Value::Object(inner));
            doc.insert("eden".into(), Value::Object(wrapped));
        }
        Ok(Value::Object(doc))
    }

    /// Decrypt a document read from the remote (`outgoing`), after the
    /// unencrypted-content policy.
    pub fn outgoing(&self, doc: Value) -> Result<Value, CryptoError> {
        let (mut doc, id) = object(doc)?;
        policy::check(&doc, self.accept_unencrypted_content)?;
        if owns_data(&id) {
            let plain = match data_state(&doc)? {
                DataState::Plain => None,
                DataState::Hkdf(envelope) => Some(self.key.decrypt(envelope)?),
                DataState::V1 => return Err(CryptoError::Unsupported(Unsupported::V1Ciphertext)),
                DataState::Unknown => return Err(CryptoError::Unsupported(Unsupported::UnknownCiphertext)),
            };
            if let Some(plain) = plain {
                doc.insert("data".into(), Value::String(plain));
                doc.remove("e_");
            }
        }
        if id.starts_with(ids::OBFUSCATED_PREFIX) {
            return Err(CryptoError::Unsupported(Unsupported::PathObfuscation));
        }
        if ids::is_internal_obfuscated_id(&id)
            && doc
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|p| p.starts_with(ids::ENCRYPTED_META_PREFIX))
        {
            return Err(CryptoError::Unsupported(Unsupported::EncryptedMetadata));
        }
        let sealed_eden = match eden(&doc)? {
            Some(Eden::Object(m)) => {
                if m.contains_key(ids::EDEN_ENCRYPTED_KEY) {
                    return Err(CryptoError::Unsupported(Unsupported::V1Eden));
                }
                match m.get(ids::EDEN_ENCRYPTED_KEY_HKDF) {
                    None => None,
                    Some(v) => Some(
                        v.get("data")
                            .and_then(Value::as_str)
                            .ok_or_else(|| CryptoError::malformed("encrypted eden has no data"))?
                            .to_string(),
                    ),
                }
            }
            _ => None,
        };
        if let Some(envelope) = sealed_eden {
            let json = self.key.decrypt(&envelope)?;
            let parsed: Value = serde_json::from_str(&json)
                .map_err(|e| CryptoError::malformed(format!("decrypted eden is not JSON: {e}")))?;
            // `{ ...JSON.parse(x) }`: an object keeps its keys, an array
            // becomes its indices.
            let restored = match parsed {
                Value::Object(m) => m,
                Value::Array(a) => a.into_iter().enumerate().map(|(i, v)| (i.to_string(), v)).collect(),
                _ => return Err(CryptoError::malformed("decrypted eden is not an object")),
            };
            doc.insert("eden".into(), Value::Object(restored));
        }
        Ok(Value::Object(doc))
    }
}
