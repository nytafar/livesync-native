//! Encrypted-database chunk IDs from `tools/golden/crypto.mjs`
//! (commonlib 0.4.3, octagonal-wheels 0.1.56).
use livesync_model::chunk::{ChunkIdScheme, PassphraseHash};
use serde_json::Value;
use std::path::PathBuf;

#[test]
fn encrypted_chunk_ids_match_upstream() {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/crypto_chunk_ids.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(v["meta"]["version"], "0.4.3", "fixtures must come from commonlib 0.4.3");
    assert_eq!(v["upstream"]["octagonalWheels"], "0.1.56");
    let cases = v["cases"].as_array().unwrap();
    assert!(cases.len() >= 40);
    let mut ids = 0;
    for c in cases {
        let passphrase = c["passphrase"].as_str().unwrap();
        let hash = PassphraseHash::from_passphrase(passphrase);
        assert_eq!(
            hash.as_str(),
            c["hashedPassphrase"].as_str().unwrap(),
            "passphrase {passphrase:?}"
        );
        let scheme = ChunkIdScheme::Encrypted(hash);
        for i in c["ids"].as_array().unwrap() {
            let piece = i["piece"].as_str().unwrap();
            assert_eq!(
                scheme.chunk_id(piece),
                i["id"].as_str().unwrap(),
                "passphrase {passphrase:?}, piece {piece:?}"
            );
            ids += 1;
        }
    }
    assert!(ids >= 300);
}

#[test]
fn passphrase_hash_is_not_printed() {
    let hash = PassphraseHash::from_passphrase("synthetic");
    assert_eq!(format!("{hash:?}"), "PassphraseHash(..)");
    assert_eq!(
        format!("{:?}", ChunkIdScheme::Encrypted(hash)),
        "Encrypted(PassphraseHash(..))"
    );
}
