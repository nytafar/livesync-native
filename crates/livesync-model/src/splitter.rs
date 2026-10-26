//! The `v3-rabin-karp` content splitter, ported exactly so our chunks
//! deduplicate with the official clients.
//!
//! Upstream: `livesync-commonlib/src/string_and_binary/chunks.ts`
//! (`splitPiecesRabinKarp`) with the parameters from
//! `ContentSplitter/ContentSplitterBase.ts` (`getParamsFor`). The golden
//! vectors in `fixtures/golden/splitter.json` pin the equivalence.

use crate::chunk::chunk_id_xxhash64;
use crate::codec::base64_encode;
use crate::doc::FileKind;
use crate::path::is_plain_text;
use std::ops::Range;

/// `MAX_DOC_SIZE_BIN` in upstream.
const MAX_DOC_SIZE_BIN: f64 = 102_400.0;
const WINDOW: usize = 48;
const PRIME: i32 = 31;
const MAX_CHUNK_COUNT: f64 = 500.0;
const PLAIN_TOO_LARGE: usize = 4 * 1024 * 1024;
const CHUNK_UNIT_BINARY: usize = 256 * 1024;
const RK_ABS_MAX_FLOOR: usize = 30 * 1024;

/// The settings that shape the split (both are PREFERRED tweak values).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitParams {
    /// `customChunkSize` (0 when absent).
    pub custom_chunk_size: f64,
    /// `minimumChunkSize` (upstream default 20).
    pub minimum_chunk_size: usize,
}

impl Default for SplitParams {
    fn default() -> Self {
        SplitParams {
            custom_chunk_size: 0.0,
            minimum_chunk_size: 20,
        }
    }
}

impl SplitParams {
    /// Read the parameters from a PREFERRED tweak map.
    pub fn from_tweaks(t: &serde_json::Map<String, serde_json::Value>) -> Self {
        let d = SplitParams::default();
        SplitParams {
            custom_chunk_size: t
                .get("customChunkSize")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(d.custom_chunk_size),
            minimum_chunk_size: t
                .get("minimumChunkSize")
                .and_then(serde_json::Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(d.minimum_chunk_size),
        }
    }

    /// `pieceSize` from `getParamsFor`: `floor(MAX_DOC_SIZE_BIN * (customChunkSize + 1))`.
    pub fn absolute_max_piece_size(&self) -> usize {
        (MAX_DOC_SIZE_BIN * (self.custom_chunk_size + 1.0)).floor().max(0.0) as usize
    }
}

/// `shouldSplitAsPlainText` in upstream `path.ts`.
pub fn should_split_as_plain_text(path: &str) -> bool {
    path.ends_with(".md") || path.ends_with(".txt") || path.ends_with(".canvas")
}

/// Boundaries chosen by `splitPiecesRabinKarp` for `data`, as byte ranges.
/// `is_text` is upstream's `isTextBlob` (the file is read as a string).
pub fn split_ranges(data: &[u8], is_text: bool, path: &str, params: &SplitParams) -> Vec<Range<usize>> {
    let absolute_max = params.absolute_max_piece_size();
    let mut plain_split = should_split_as_plain_text(path) || is_text;
    let size = data.len();
    let mut unit_plain: usize = 64;
    if plain_split {
        if size >= PLAIN_TOO_LARGE {
            plain_split = false;
        } else {
            while size as f64 / (unit_plain * 4) as f64 > MAX_CHUNK_COUNT {
                unit_plain += 32;
            }
        }
    }
    let fixed_max = if plain_split {
        unit_plain * 16
    } else {
        CHUNK_UNIT_BINARY * 16
    };
    let effective_abs_max = absolute_max.max(RK_ABS_MAX_FLOOR);
    let max_chunk = fixed_max.min(effective_abs_max);
    let gap = (max_chunk / 4).max(1);
    let min_chunk = (if plain_split { unit_plain * 2 } else { gap })
        .max(params.minimum_chunk_size)
        .min(max_chunk);
    let avg = if plain_split {
        (unit_plain * 4).max(min_chunk).min(max_chunk)
    } else {
        gap
    };
    let modulus = avg as u64;

    let mut p_pow_w: i32 = 1;
    for _ in 0..WINDOW - 1 {
        p_pow_w = p_pow_w.wrapping_mul(PRIME);
    }

    let mut out = Vec::with_capacity(size / avg.max(1) + 1);
    let mut hash: i32 = 0;
    let mut start = 0usize;
    for pos in 0..size {
        let byte = data[pos] as i32;
        if pos >= WINDOW {
            let old = data[pos - WINDOW] as i32;
            hash = hash.wrapping_sub(old.wrapping_mul(p_pow_w));
        }
        hash = hash.wrapping_mul(PRIME).wrapping_add(byte);

        let current = pos - start + 1;
        let mut candidate = current >= min_chunk && (hash as u32 as u64) % modulus == 1;
        if current >= max_chunk {
            candidate = true;
        }
        if candidate {
            // Text never splits in front of a UTF-8 continuation byte.
            let safe = !(is_text && pos + 1 < size && (data[pos + 1] & 0xC0) == 0x80);
            if safe {
                out.push(start..pos + 1);
                start = pos + 1;
            }
        }
    }
    if start < size {
        out.push(start..size);
    }
    out
}

/// A file prepared for upload: its document type, content size and chunks.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunked {
    pub kind: FileKind,
    /// Size of the stored content in bytes (the UTF-8 length for text).
    pub size: u64,
    /// `(chunk id, chunk data)` in document order. Duplicates are kept, as
    /// `children` lists them.
    pub chunks: Vec<(String, String)>,
    /// Text that was not valid UTF-8 and was stored with U+FFFD replacements,
    /// as upstream's string read would.
    pub lossy: bool,
}

/// Split a file the way upstream's `putDBEntry` does: plain-text extensions
/// are stored as `plain` with UTF-8 text pieces, everything else as `newnote`
/// with base64 pieces. Empty pieces are dropped.
pub fn chunk_file(path: &str, bytes: &[u8], params: &SplitParams) -> Chunked {
    if is_plain_text(path) {
        let text = String::from_utf8_lossy(bytes);
        let lossy = matches!(text, std::borrow::Cow::Owned(_));
        let data = text.as_bytes();
        let chunks = split_ranges(data, true, path, params)
            .into_iter()
            .filter(|r| !r.is_empty())
            .map(|r| {
                // Boundaries never fall inside a character, so this is valid UTF-8.
                let piece = std::str::from_utf8(&data[r]).expect("split on a character boundary");
                (chunk_id_xxhash64(piece), piece.to_string())
            })
            .collect();
        Chunked {
            kind: FileKind::Plain,
            size: data.len() as u64,
            chunks,
            lossy,
        }
    } else {
        let chunks = split_ranges(bytes, false, path, params)
            .into_iter()
            .filter(|r| !r.is_empty())
            .map(|r| {
                let piece = base64_encode(&bytes[r]);
                (chunk_id_xxhash64(&piece), piece)
            })
            .collect();
        Chunked {
            kind: FileKind::Binary,
            size: bytes.len() as u64,
            chunks,
            lossy: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params() {
        let p = SplitParams {
            custom_chunk_size: 60.0,
            minimum_chunk_size: 20,
        };
        assert_eq!(p.absolute_max_piece_size(), 6_246_400);
        assert_eq!(SplitParams::default().absolute_max_piece_size(), 102_400);
    }

    #[test]
    fn ranges_cover_input_and_respect_characters() {
        let text = "æøå 😀 ".repeat(5000);
        let r = split_ranges(text.as_bytes(), true, "a.md", &SplitParams::default());
        assert_eq!(r.first().unwrap().start, 0);
        assert_eq!(r.last().unwrap().end, text.len());
        for w in r.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
        for x in &r {
            assert!(text.is_char_boundary(x.start));
        }
    }

    #[test]
    fn lossy_text_is_flagged() {
        let c = chunk_file("bad.md", b"ok \xff\xfe end", &SplitParams::default());
        assert!(c.lossy);
        assert_eq!(c.kind, FileKind::Plain);
        let joined: String = c.chunks.iter().map(|(_, d)| d.as_str()).collect();
        assert_eq!(joined, "ok \u{FFFD}\u{FFFD} end");
        assert_eq!(c.size as usize, joined.len());
        assert!(chunk_file("empty.md", b"", &SplitParams::default()).chunks.is_empty());
    }
}
