//! End-to-end encryption for Self-hosted LiveSync databases, byte-compatible
//! with `@vrtmrz/livesync-commonlib` 0.4.3 / `octagonal-wheels` 0.1.56 for
//! `encrypt: true`, `E2EEAlgorithm: "v2"`, `usePathObfuscation: false`.
//!
//! Pure: no file, network or async I/O. Randomness comes from the OS
//! (`getrandom`) unless the caller supplies a [`RandomSource`].
//!
//! - [`KeyContext`]: the PBKDF2 master key, derived once; `%=` envelopes.
//! - [`DocumentCipher`]: upstream's `incoming`/`outgoing` document transforms,
//!   with the 1.0.49 unencrypted-content policy.
//! - [`parse_pbkdf2_salt`]: the salt from the sync-parameters document.
//!
//! Chunk IDs with a passphrase live in `livesync_model::chunk`
//! (`PassphraseHash`, `ChunkIdScheme`), next to the plain ones.
//!
//! Out of scope, reported as [`CryptoError::Unsupported`]: V1/ForceV1
//! ciphertext, V1 eden, path obfuscation (`f:` ids), encrypted internal-file
//! metadata. Dynamic iteration counts belong to V1 only.

mod doc;
mod error;
pub mod ids;
mod key;
mod passphrase;
mod policy;
mod sync_params;

pub use doc::DocumentCipher;
pub use error::{ContentKind, CryptoError, Unsupported};
pub use key::{KeyContext, OsRandom, RandomSource, HKDF_PREFIX, PBKDF2_ITERATIONS};
pub use passphrase::Passphrase;
pub use policy::{check_unencrypted_content, unencrypted_content_kinds};
pub use sync_params::{parse_pbkdf2_salt, SyncParamsError};
