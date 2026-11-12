//! The V2 (HKDF) envelope: octagonal-wheels `encryption/hkdf.ts`.
//!
//! ```text
//! master   = PBKDF2-HMAC-SHA256(utf8(passphrase), pbkdf2Salt, 310000, 32 bytes)
//! key      = HKDF-SHA256(ikm = master, salt = hkdfSalt, info = "", 32 bytes)
//! envelope = "%=" + base64(iv[12] | hkdfSalt[32] | AES-256-GCM(key, iv, utf8(text)) | tag[16])
//! ```
//!
//! No associated data. Each encryption draws a fresh HKDF salt and IV, in
//! that order (upstream fills the salt first).

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::alphabet;
use base64::engine::{DecodePaddingMode, Engine, GeneralPurpose, GeneralPurposeConfig};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::error::CryptoError;

/// Prefix of a V2 envelope (`HKDF_ENCRYPTED_PREFIX`).
pub const HKDF_PREFIX: &str = "%=";
/// PBKDF2 iterations (`PBKDF2_ITERATIONS`); fixed for V2.
pub const PBKDF2_ITERATIONS: u32 = 310_000;
const IV_LEN: usize = 12;
const HKDF_SALT_LEN: usize = 32;
const KEY_LEN: usize = 32;

/// Standard base64 with padding on output; on input, padding is optional and
/// stray trailing bits are ignored, as `atob` does.
pub(crate) const B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_encode_padding(true)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// Where the per-envelope HKDF salt and IV come from.
pub trait RandomSource {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), CryptoError>;
}

/// The operating system's random source (`getrandom`).
#[derive(Debug, Default, Clone, Copy)]
pub struct OsRandom;

impl RandomSource for OsRandom {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), CryptoError> {
        getrandom::fill(buf).map_err(|e| CryptoError::Random(e.to_string()))
    }
}

/// The derived master key for one (passphrase, PBKDF2 salt) pair.
///
/// Building it runs PBKDF2 once (310 000 rounds); build it once per database
/// and reuse it. It keeps neither the passphrase nor the salt, prints no key
/// material in `Debug`, and wipes the key when dropped.
#[derive(Clone)]
pub struct KeyContext {
    master: Zeroizing<[u8; KEY_LEN]>,
}

impl fmt::Debug for KeyContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyContext { .. }")
    }
}

impl KeyContext {
    /// Derive the master key (slow: PBKDF2 with 310 000 rounds).
    pub fn derive(passphrase: &str, pbkdf2_salt: &[u8]) -> Self {
        let mut master = Zeroizing::new([0u8; KEY_LEN]);
        pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), pbkdf2_salt, PBKDF2_ITERATIONS, master.as_mut());
        KeyContext { master }
    }

    fn cipher(&self, hkdf_salt: &[u8]) -> Aes256Gcm {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        Hkdf::<Sha256>::new(Some(hkdf_salt), self.master.as_ref())
            .expand(&[], key.as_mut())
            .expect("32 bytes is a valid HKDF-SHA256 length");
        Aes256Gcm::new_from_slice(key.as_ref()).expect("32-byte key")
    }

    /// Encrypt `plaintext` into a `%=` envelope, with OS randomness.
    pub fn encrypt(&self, plaintext: &str) -> Result<String, CryptoError> {
        self.encrypt_with(plaintext, &mut OsRandom)
    }

    /// Encrypt with a caller-supplied random source (which must never repeat
    /// outputs; tests use a seeded one to reproduce upstream's vectors).
    pub fn encrypt_with(&self, plaintext: &str, rng: &mut dyn RandomSource) -> Result<String, CryptoError> {
        let mut hkdf_salt = [0u8; HKDF_SALT_LEN];
        rng.fill(&mut hkdf_salt)?;
        let mut iv = [0u8; IV_LEN];
        rng.fill(&mut iv)?;
        let sealed = self
            .cipher(&hkdf_salt)
            .encrypt(&Nonce::from(iv), plaintext.as_bytes())
            .map_err(|_| CryptoError::malformed("AES-GCM refused the plaintext"))?;
        let mut out = Vec::with_capacity(IV_LEN + HKDF_SALT_LEN + sealed.len());
        out.extend_from_slice(&iv);
        out.extend_from_slice(&hkdf_salt);
        out.extend_from_slice(&sealed);
        Ok(format!("{HKDF_PREFIX}{}", B64.encode(out)))
    }

    /// Decrypt a `%=` envelope.
    pub fn decrypt(&self, envelope: &str) -> Result<String, CryptoError> {
        let body = envelope
            .strip_prefix(HKDF_PREFIX)
            .ok_or_else(|| CryptoError::malformed("not a V2 envelope (no \"%=\" prefix)"))?;
        let bin = B64
            .decode(body)
            .map_err(|e| CryptoError::malformed(format!("envelope is not base64: {e}")))?;
        if bin.len() < IV_LEN + HKDF_SALT_LEN {
            return Err(CryptoError::malformed("envelope shorter than IV and HKDF salt"));
        }
        let (iv, rest) = bin.split_at(IV_LEN);
        let (hkdf_salt, sealed) = rest.split_at(HKDF_SALT_LEN);
        let iv: [u8; IV_LEN] = iv.try_into().expect("split at IV_LEN");
        let plain = self
            .cipher(hkdf_salt)
            .decrypt(&Nonce::from(iv), sealed)
            .map_err(|_| CryptoError::Authentication)?;
        String::from_utf8(plain).map_err(|_| CryptoError::malformed("decrypted content is not UTF-8"))
    }
}
