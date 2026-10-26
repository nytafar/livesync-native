//! Chunk IDs for `hashAlg = "xxhash64"` with encryption off.
//!
//! Upstream: `XXHashHashManager.ts` — `xxh64_utf8(piece + "-" + piece.length)`
//! rendered in base36, where `length` counts UTF-16 code units.

use xxhash_rust::xxh64::xxh64;

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
