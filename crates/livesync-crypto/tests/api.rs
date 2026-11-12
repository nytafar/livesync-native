use livesync_crypto::{CryptoError, DocumentCipher, KeyContext, Unsupported};
use serde_json::json;

fn key() -> KeyContext {
    KeyContext::derive("synthetic-api-passphrase", &[7u8; 32])
}

#[test]
fn debug_output_holds_no_secrets() {
    let k = key();
    assert_eq!(format!("{k:?}"), "KeyContext { .. }");
    let c = DocumentCipher::new(k, false);
    let shown = format!("{c:?}");
    assert!(!shown.contains("synthetic"), "{shown}");
    assert_eq!(
        shown,
        "DocumentCipher { key: KeyContext { .. }, accept_unencrypted_content: false }"
    );
}

#[test]
fn os_random_envelopes_round_trip_and_differ() {
    let k = key();
    let a = k.encrypt("same text").unwrap();
    let b = k.encrypt("same text").unwrap();
    assert_ne!(a, b);
    assert!(a.starts_with("%="));
    assert_eq!(k.decrypt(&a).unwrap(), "same text");
    assert_eq!(k.decrypt(&b).unwrap(), "same text");
}

#[test]
fn documents_round_trip_and_pass_through() {
    let c = DocumentCipher::new(key(), false);
    let chunk = json!({ "_id": "h:+x", "type": "leaf", "data": "body" });
    let remote = c.incoming(chunk.clone()).unwrap();
    assert_eq!(remote["e_"], true);
    assert_ne!(remote["data"], "body");
    assert_eq!(c.outgoing(remote).unwrap(), chunk);

    let note = json!({ "_id": "a.md", "type": "plain", "children": ["h:+x"] });
    assert_eq!(c.incoming(note.clone()).unwrap(), note);
    assert_eq!(c.outgoing(note.clone()).unwrap(), note);
}

#[test]
fn errors_are_typed() {
    let c = DocumentCipher::new(key(), false);
    let other = DocumentCipher::new(KeyContext::derive("other", &[7u8; 32]), false);
    let remote = c.incoming(json!({ "_id": "h:+x", "data": "body" })).unwrap();
    assert_eq!(other.outgoing(remote), Err(CryptoError::Authentication));
    assert!(matches!(
        c.outgoing(json!({ "_id": "h:+y", "data": "plain" })),
        Err(CryptoError::Rejected { .. })
    ));
    assert_eq!(
        c.incoming(json!({ "_id": "f:abc", "path": "x.md" })),
        Err(CryptoError::Unsupported(Unsupported::PathObfuscation))
    );
    assert!(matches!(c.incoming(json!([1])), Err(CryptoError::Malformed(_))));
}
