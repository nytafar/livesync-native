//! Document IDs and keys the transform keys on (upstream `shared.const.behabiour.ts`,
//! `db.const.ts`, `encryption.eden.ts`, `fileaccess.const.ts`).

/// `IDPrefixes.Chunk`.
pub const CHUNK_PREFIX: &str = "h:";
/// `IDPrefixes.EncryptedChunk`: chunks of an encrypted database.
pub const ENCRYPTED_CHUNK_PREFIX: &str = "h:+";
/// `IDPrefixes.Obfuscated`: documents with an obfuscated path.
pub const OBFUSCATED_PREFIX: &str = "f:";
/// `SYNCINFO_ID`.
pub const SYNCINFO_ID: &str = "syncinfo";
/// `DOCID_SYNC_PARAMETERS`: holds the PBKDF2 salt.
pub const SYNC_PARAMETERS_ID: &str = "_local/obsidian_livesync_sync_parameters";
/// Eden key of a V1-encrypted eden.
pub const EDEN_ENCRYPTED_KEY: &str = "h:++encrypted";
/// Eden key of an HKDF-encrypted eden.
pub const EDEN_ENCRYPTED_KEY_HKDF: &str = "h:++encrypted-hkdf";
/// `epoch` upstream writes next to the encrypted eden.
pub const EDEN_ENCRYPTED_EPOCH: u64 = 999_999;
/// `ENCRYPTED_META_PREFIX`: an encrypted metadata envelope in `path`.
pub const ENCRYPTED_META_PREFIX: &str = "/\\:";
/// Internal-file prefixes (`ICHeader`, `ICXHeader`, `PSCHeader`).
pub const INTERNAL_PREFIXES: &[&str] = &["i:", "ix:", "ps:"];

pub fn is_encrypted_chunk_id(id: &str) -> bool {
    id.starts_with(ENCRYPTED_CHUNK_PREFIX)
}

/// `isInternalObfuscatedEntry`: `i:f:`, `ix:f:` or `ps:f:`.
pub fn is_internal_obfuscated_id(id: &str) -> bool {
    INTERNAL_PREFIXES.iter().any(|p| {
        id.strip_prefix(p)
            .is_some_and(|rest| rest.starts_with(OBFUSCATED_PREFIX))
    })
}
