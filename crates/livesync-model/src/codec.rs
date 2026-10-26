//! Read-side codec: the compression marker and binary (base64) decoding.
//!
//! Upstream: `livesync-commonlib/src/pouchdb/compress.ts` and
//! `octagonal-wheels/dist/binary/index.js` (`decodeBinary`).

use base64::Engine as _;
use std::borrow::Cow;

/// `MARK_SHIFT_COMPRESSED` in upstream.
pub const COMPRESSED_MARK: &str = "\u{000E}LZ\u{001D}";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("invalid base64 in chunk data")]
    Base64,
    #[error("invalid DEFLATE stream in compressed chunk")]
    Inflate,
    #[error("legacy %-encoded binary data is not supported")]
    LegacyPercentEncoding,
    #[error("encrypted data (prefix {0:?}) is not supported")]
    Encrypted(String),
}

fn b64() -> base64::engine::GeneralPurpose {
    // Standard alphabet; accept missing padding the way the JS decoders do.
    base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    )
}

pub fn base64_decode(s: &str) -> Result<Vec<u8>, CodecError> {
    if s.is_empty() {
        return Ok(Vec::new());
    }
    b64().decode(s).map_err(|_| CodecError::Base64)
}

pub fn base64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// `decompressDoc` applied to a chunk's `data`: strips the marker and inflates
/// (raw DEFLATE, as fflate's `inflate`). Data without the marker passes through.
pub fn decompress(data: &str) -> Result<Cow<'_, str>, CodecError> {
    let Some(rest) = data.strip_prefix(COMPRESSED_MARK) else {
        return Ok(Cow::Borrowed(data));
    };
    if rest.is_empty() {
        return Ok(Cow::Borrowed(""));
    }
    let (converted, src) = match rest.strip_prefix('~') {
        Some(s) => (true, s),
        None => (false, rest),
    };
    if src.is_empty() {
        return Ok(Cow::Borrowed(""));
    }
    let raw = base64_decode(src)?;
    if raw.is_empty() {
        return Ok(Cow::Borrowed(""));
    }
    let inflated = miniz_oxide::inflate::decompress_to_vec(&raw).map_err(|_| CodecError::Inflate)?;
    if converted {
        Ok(Cow::Owned(base64_encode(&inflated)))
    } else {
        // Blob.text() decodes UTF-8 with replacement characters.
        Ok(Cow::Owned(String::from_utf8_lossy(&inflated).into_owned()))
    }
}

/// Refuse encrypted chunk payloads (V2 `%=`, V1 `%`, V3 `%~`, JSON-array `[`)
/// when the chunk is flagged encrypted.
pub fn check_not_encrypted(data: &str, e_flag: bool) -> Result<(), CodecError> {
    if e_flag {
        let prefix: String = data.chars().take(2).collect();
        return Err(CodecError::Encrypted(prefix));
    }
    Ok(())
}

/// Decode one binary (`newnote`) chunk: base64 to bytes.
pub fn decode_binary_piece(piece: &str) -> Result<Vec<u8>, CodecError> {
    if piece.starts_with('%') {
        return Err(CodecError::LegacyPercentEncoding);
    }
    base64_decode(piece)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough() {
        assert_eq!(decompress("plain").unwrap(), "plain");
    }

    #[test]
    fn percent_refused() {
        assert_eq!(decode_binary_piece("%abc"), Err(CodecError::LegacyPercentEncoding));
    }
}
