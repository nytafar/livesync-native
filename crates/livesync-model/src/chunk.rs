//! Chunk IDs for `hashAlg = "xxhash64"`, with encryption off and on.
//!
//! Upstream: `XXHashHashManager.ts` — `xxh64_utf8(piece + "-" + piece.length)`
//! rendered in base36, where `length` counts UTF-16 code units. With
//! end-to-end encryption on (`HashManagerCore.ts`), the ID is
//! `h:+` + base36(`xxh64_utf8(piece + "-" + hashedPassphrase + "-" + piece.length)`),
//! where `hashedPassphrase` is [`PassphraseHash`].
//!
//! Not covered: commonlib 0.4's keyed ID derivation (`idDerivationVersion: 1`
//! with an `idDerivationKey`), which replaces encrypted chunk IDs with
//! HMAC-SHA256 hex digests (`common/idDerivation.ts`). A database using it
//! must be refused, not given these IDs.

use std::fmt;

use xxhash_rust::xxh64::xxh64;
use zeroize::Zeroize;

/// `SALT_OF_ID` in upstream's `shared.const.behabiour.ts`. It holds a raw
/// U+0003 control character after `a83hrf7f`, invisible in most editors.
pub const SALT_OF_ID: &str = "a83hrf7f\u{3}y7sa8g31";

/// Prefix of every chunk ID (`IDPrefixes.Chunk`).
pub const CHUNK_PREFIX: &str = "h:";

/// Prefix of chunk IDs made with encryption on (`IDPrefixes.EncryptedChunk`).
pub const ENCRYPTED_CHUNK_PREFIX: &str = "h:+";

/// Number of UTF-16 code units in `s` (JavaScript `String.length`).
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Lower-case base36 rendering of an unsigned integer (JavaScript `toString(36)`).
pub fn to_base36(mut v: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if v == 0 {
        return "0".into();
    }
    let mut buf = Vec::with_capacity(13);
    while v > 0 {
        buf.push(DIGITS[(v % 36) as usize]);
        v /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).expect("ascii")
}

/// The hash part of a chunk ID (without the `h:` prefix).
pub fn chunk_hash_xxhash64(piece: &str) -> String {
    let input = format!("{piece}-{}", utf16_len(piece));
    to_base36(xxh64(input.as_bytes(), 0))
}

/// Full chunk document ID: `h:<hash>`.
pub fn chunk_id_xxhash64(piece: &str) -> String {
    format!("h:{}", chunk_hash_xxhash64(piece))
}

/// The passphrase-derived value upstream mixes into chunk IDs when
/// encryption is on (`HashManagerCore.hashedPassphrase`):
/// `fallbackMixedHashEach(SALT_OF_ID + passphrase.substring(0, ~~(len / 4 * 3)))`,
/// with `len` and the cut in UTF-16 code units (a cut may split a surrogate pair).
///
/// It is derived from the passphrase with a fast, non-cryptographic hash, so it is
/// kept out of `Debug` output and wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct PassphraseHash(String);

impl PassphraseHash {
    pub fn from_passphrase(passphrase: &str) -> Self {
        let mut units: Vec<u16> = passphrase.encode_utf16().collect();
        let using = units.len() * 3 / 4;
        let mut input: Vec<u16> = SALT_OF_ID.encode_utf16().collect();
        input.extend_from_slice(&units[..using]);
        let hash = fallback_mixed_hash_each(&input);
        units.zeroize();
        input.zeroize();
        PassphraseHash(hash)
    }

    /// The hash as upstream renders it (two base36 numbers, concatenated).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PassphraseHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PassphraseHash(..)")
    }
}

impl Drop for PassphraseHash {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// The hash part of an encrypted-database chunk ID (without the `h:+` prefix).
pub fn chunk_hash_xxhash64_encrypted(piece: &str, passphrase: &PassphraseHash) -> String {
    let input = format!("{piece}-{}-{}", passphrase.as_str(), utf16_len(piece));
    to_base36(xxh64(input.as_bytes(), 0))
}

/// Full chunk document ID with encryption on: `h:+<hash>`.
pub fn chunk_id_xxhash64_encrypted(piece: &str, passphrase: &PassphraseHash) -> String {
    format!(
        "{ENCRYPTED_CHUNK_PREFIX}{}",
        chunk_hash_xxhash64_encrypted(piece, passphrase)
    )
}

/// How a database names its chunks (`hashAlg = "xxhash64"`).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ChunkIdScheme {
    /// Encryption off: `h:<hash>`.
    #[default]
    Plain,
    /// Encryption on: `h:+<hash>`, mixed with the passphrase hash.
    Encrypted(PassphraseHash),
}

impl ChunkIdScheme {
    pub fn chunk_id(&self, piece: &str) -> String {
        match self {
            ChunkIdScheme::Plain => chunk_id_xxhash64(piece),
            ChunkIdScheme::Encrypted(p) => chunk_id_xxhash64_encrypted(piece, p),
        }
    }
}

/// JavaScript `ToInt32`: truncate, then wrap modulo 2^32.
fn to_int32(x: f64) -> i32 {
    x.trunc().rem_euclid(4_294_967_296.0) as u64 as u32 as i32
}

/// `mixedHash` from octagonal-wheels `hash/purejs`, over UTF-16 code units.
///
/// It mixes integer and floating-point arithmetic: `k1 *= c2` and
/// `h1 = h1 * m + n` are plain JavaScript number operations, and the first can
/// exceed 2^53, so it is reproduced in `f64` and wrapped with `ToInt32`.
fn mixed_hash(units: &[u16], seed: u32, fnv_init: u32) -> (u32, u32) {
    const C1: u32 = 0xcc9e_2d51;
    const C2: u32 = 0x1b87_3593;
    const N: u32 = 0xe654_6b64;
    let mut h1: f64 = seed as f64;
    let mut fnv = fnv_init;
    for &cu in units {
        let c = cu as u32;
        fnv = (fnv ^ c).wrapping_mul(0x0100_0193);
        let k1 = c as f64 * C1 as f64;
        let k1 = (to_int32(k1) as u32).rotate_left(15) as i32;
        let k1 = k1 as f64 * C2 as f64;
        let h = (to_int32(h1) ^ to_int32(k1)) as u32;
        let h = h.rotate_left(13) as i32;
        h1 = h as f64 * 5.0 + N as f64;
    }
    let mut h = (to_int32(h1) ^ units.len() as i32) as u32;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    (h, fnv)
}

/// `fallbackMixedHashEach` from octagonal-wheels `hash/purejs`.
fn fallback_mixed_hash_each(src: &[u16]) -> String {
    const EPOCH_FNV1A: u32 = 2_166_136_261;
    let mut input: Vec<u16> = src.len().to_string().encode_utf16().collect();
    input.extend_from_slice(src);
    let (m, f) = mixed_hash(&input, 1, EPOCH_FNV1A);
    input.zeroize();
    format!("{}{}", to_base36(m as u64), to_base36(f as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base36() {
        assert_eq!(to_base36(0), "0");
        assert_eq!(to_base36(35), "z");
        assert_eq!(to_base36(36), "10");
        assert_eq!(to_base36(u64::MAX), "3w5e11264sgsf");
    }

    #[test]
    fn utf16() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("æøå"), 3);
        assert_eq!(utf16_len("😀"), 2);
    }
}
