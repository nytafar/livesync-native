use std::fmt;

/// What a failed encryption step means for the caller.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum CryptoError {
    /// AES-GCM authentication failed: the passphrase (or the PBKDF2 salt) is
    /// wrong, or the ciphertext was changed. The two cannot be told apart.
    #[error("decryption failed: wrong passphrase or damaged ciphertext")]
    Authentication,
    /// Plaintext content in a remote document, refused by the 1.0.49
    /// unencrypted-content policy (`UnencryptedContentRejectedError` upstream).
    #[error("unencrypted {content_kind} content in remote document {document_id} was rejected while E2EE is enabled")]
    Rejected {
        document_id: String,
        content_kind: ContentKind,
    },
    /// A representation this build does not read or write.
    #[error("unsupported: {0}")]
    Unsupported(Unsupported),
    /// The input is not what any upstream writer produces (bad base64, a
    /// too-short envelope, a non-string `data`, invalid UTF-8 after
    /// decryption, an eden that is not an object, ...).
    #[error("malformed: {0}")]
    Malformed(String),
    /// The operating system's random source failed.
    #[error("random source failed: {0}")]
    Random(String),
}

impl CryptoError {
    pub(crate) fn malformed(msg: impl Into<String>) -> Self {
        CryptoError::Malformed(msg.into())
    }
}

/// The content kinds of upstream's `UnencryptedContentRejectedError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// A plaintext key in `eden`.
    Eden,
    /// `data` on a chunk (`h:` id, or type `leaf`/`chunkpack`).
    Chunk,
    /// `data` on any other document (a note stored inline).
    InlineNote,
}

impl ContentKind {
    /// Upstream's spelling (`"eden"`, `"chunk"`, `"inline-note"`).
    pub fn as_str(self) -> &'static str {
        match self {
            ContentKind::Eden => "eden",
            ContentKind::Chunk => "chunk",
            ContentKind::InlineNote => "inline-note",
        }
    }
}

impl fmt::Display for ContentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Representations upstream can read or write that this build refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// V1 ciphertext (`%…`, `%~…` or the JSON-array form) in `data`.
    V1Ciphertext,
    /// `e_: true` on a chunk or `syncinfo` whose `data` has no known prefix
    /// (upstream: "Unknown encryption version").
    UnknownCiphertext,
    /// An eden encrypted with V1 (`h:++encrypted`).
    V1Eden,
    /// Path obfuscation: an `f:` document (upstream encrypts its metadata
    /// into a `/\:` envelope in `path`).
    PathObfuscation,
    /// Encrypted internal-file metadata (`i:f:`, `ix:f:`, `ps:f:` ids with a
    /// `/\:` path).
    EncryptedMetadata,
    /// `e_: true` on a document whose content no E2EE reader decrypts
    /// (upstream: "Encrypted file content uses a representation which this
    /// E2EE reader cannot decrypt").
    UnreadableEncryptedContent,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Unsupported::V1Ciphertext => "V1 ciphertext",
            Unsupported::UnknownCiphertext => "ciphertext with an unknown encryption version",
            Unsupported::V1Eden => "eden encrypted with V1",
            Unsupported::PathObfuscation => "path obfuscation",
            Unsupported::EncryptedMetadata => "encrypted internal-file metadata",
            Unsupported::UnreadableEncryptedContent => {
                "encrypted file content in a representation no E2EE reader decrypts"
            }
        })
    }
}
