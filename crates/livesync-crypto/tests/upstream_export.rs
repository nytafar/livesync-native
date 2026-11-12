//! Opt-in: write envelopes and documents Rust encrypted with real randomness,
//! for upstream to decrypt (`tools/golden/crypto-verify.mjs`). Not in the gate,
//! because the check needs node:
//!
//! ```sh
//! LSYNC_CRYPTO_EXPORT=$PWD/target/crypto-export.json \
//!   cargo test -p livesync-crypto --test upstream_export -- --ignored
//! node tools/golden/crypto-verify.mjs target/crypto-export.json
//! ```
use base64::Engine;
use livesync_crypto::{DocumentCipher, KeyContext, OsRandom, RandomSource};
use serde_json::{json, Value};

#[test]
#[ignore = "writes a file for tools/golden/crypto-verify.mjs; run by hand"]
fn export_for_upstream() {
    let out = std::env::var("LSYNC_CRYPTO_EXPORT").expect("set LSYNC_CRYPTO_EXPORT to the output file");
    let passphrase = "synthetic-rust-export-😀-passphrase";
    let mut salt = [0u8; 32];
    OsRandom.fill(&mut salt).unwrap();
    let key = KeyContext::derive(passphrase, &salt);

    let plaintexts = [
        String::new(),
        "a".into(),
        "hello world".into(),
        "æøå ÆØÅ 日本語 😀🎉".into(),
        "y".repeat(128),
        "é".repeat(129),
        "\u{feff}BOM first".into(),
        "x".repeat(100_000),
    ];
    let envelopes: Vec<Value> = plaintexts
        .iter()
        .map(|p| json!({ "plaintext": p, "envelope": key.encrypt(p).unwrap() }))
        .collect();

    let meta = json!({ "ctime": 1, "mtime": 2, "size": 3 });
    let mut note = json!({
        "_id": "note.md", "type": "plain", "path": "note.md", "children": ["h:+c1"],
        "eden": { "h:+e1": { "data": "inline 😀", "epoch": 1 }, "h:+e2": { "data": "two", "epoch": 2 } }
    });
    note.as_object_mut().unwrap().extend(meta.as_object().unwrap().clone());
    let locals = [
        json!({ "_id": "h:+abc", "type": "leaf", "data": "chunk body 日本語" }),
        json!({ "_id": "h:+big", "type": "leaf", "data": "z".repeat(50_000) }),
        json!({ "_id": "syncinfo", "type": "syncinfo", "data": "sync info" }),
        note,
        json!({ "_id": "plain.md", "type": "plain", "path": "plain.md", "children": [], "eden": {} }),
    ];
    let cipher = DocumentCipher::new(key, false);
    let documents: Vec<Value> = locals
        .into_iter()
        .map(|local| {
            let remote = cipher.incoming(local.clone()).unwrap();
            json!({ "local": local, "remote": remote })
        })
        .collect();

    let file = json!({
        "generator": "crates/livesync-crypto/tests/upstream_export.rs",
        "passphrase": passphrase,
        "pbkdf2Salt": base64::engine::general_purpose::STANDARD.encode(salt),
        "envelopes": envelopes,
        "documents": documents,
    });
    std::fs::write(&out, serde_json::to_string_pretty(&file).unwrap()).unwrap();
    eprintln!("wrote {out}");
}
