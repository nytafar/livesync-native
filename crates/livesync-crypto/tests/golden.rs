//! Vectors from `tools/golden/crypto.mjs` (commonlib 0.4.3, octagonal-wheels 0.1.56).
mod common;

use common::{assert_error, fixture, key, Xorshift};
use livesync_crypto::{check_unencrypted_content, parse_pbkdf2_salt, DocumentCipher, SyncParamsError};
use serde_json::{json, Value};

#[test]
fn envelopes_from_upstream_decrypt_and_reencrypt_byte_for_byte() {
    let f = fixture("crypto_envelopes.json");
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 20);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let k = key(&f["keys"], c["key"].as_str().unwrap());
        let envelope = c["envelope"].as_str().unwrap();
        let plaintext = c["plaintext"].as_str().unwrap();
        assert_eq!(
            k.decrypt(envelope).unwrap_or_else(|e| panic!("{name}: {e}")),
            plaintext,
            "{name}"
        );
        let mut rng = Xorshift::new(c["seed"].as_u64().unwrap());
        assert_eq!(
            k.encrypt_with(plaintext, &mut rng).unwrap(),
            envelope,
            "{name}: encrypt"
        );
    }
}

#[test]
fn envelopes_upstream_refuses_fail_with_the_right_kind() {
    let f = fixture("crypto_envelopes.json");
    let failures = f["failures"].as_array().unwrap();
    assert!(failures.len() >= 10);
    for c in failures {
        let name = c["name"].as_str().unwrap();
        let k = key(&f["keys"], c["key"].as_str().unwrap());
        let err = k.decrypt(c["envelope"].as_str().unwrap()).expect_err(name);
        assert_error(&err, &json!({ "error": c["expect"] }), name);
    }
}

fn check(result: Result<Value, livesync_crypto::CryptoError>, expected: &Value, what: &str) {
    if let Some(ok) = expected.get("ok") {
        let got = result.unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(&got, ok, "{what}");
    } else {
        let err = result.err().unwrap_or_else(|| panic!("{what}: expected {expected}"));
        assert_error(&err, expected, what);
    }
}

#[test]
fn documents_incoming_and_outgoing_match_upstream() {
    let f = fixture("crypto_documents.json");
    let cipher = DocumentCipher::new(key(&f["keys"], "main"), false);
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 25);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let mut rng = Xorshift::new(c["seed"].as_u64().unwrap());
        let inc = cipher.incoming_with(c["local"].clone(), &mut rng);
        let encrypted = inc.as_ref().ok().cloned();
        check(inc, &c["incoming"], &format!("{name}: incoming"));
        if let Some(expected) = c.get("outgoing") {
            check(
                cipher.outgoing(encrypted.unwrap()),
                expected,
                &format!("{name}: outgoing"),
            );
        }
    }
}

#[test]
fn remote_documents_outgoing_matches_upstream() {
    let f = fixture("crypto_documents.json");
    let cases = f["remote"].as_array().unwrap();
    assert!(cases.len() >= 40);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let accept = c["accept"].as_bool().unwrap();
        let cipher = DocumentCipher::new(key(&f["keys"], c["key"].as_str().unwrap()), accept);
        check(
            cipher.outgoing(c["remote"].clone()),
            &c["outgoing"],
            &format!("{name} (accept={accept})"),
        );
    }
}

#[test]
fn policy_verdicts_match_upstream() {
    let f = fixture("crypto_policy.json");
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 60);
    for c in cases {
        let accept = c["accept"].as_bool().unwrap();
        let what = format!("{} (accept={accept})", c["doc"]);
        let got = check_unencrypted_content(&c["doc"], accept);
        if c["verdict"] == "ok" {
            got.unwrap_or_else(|e| panic!("{what}: {e}"));
        } else {
            assert_error(&got.expect_err(&what), &c["verdict"], &what);
        }
    }
}

#[test]
fn sync_parameter_salts_decode_like_upstream() {
    use base64::Engine;
    let f = fixture("crypto_sync_params.json");
    for c in f["cases"].as_array().unwrap() {
        let input = &c["pbkdf2salt"];
        let got = parse_pbkdf2_salt(&json!({ "_id": "_local/obsidian_livesync_sync_parameters", "pbkdf2salt": input }));
        if c["upstreamCreatesSalt"] == true {
            assert_eq!(got, Err(SyncParamsError::NoSalt), "{input}");
        } else if c["decodedLength"] == 0 {
            // Upstream's atob fails and it goes on with an empty salt; we refuse.
            assert_eq!(got, Err(SyncParamsError::InvalidSalt), "{input}");
        } else {
            let expected = base64::engine::general_purpose::STANDARD
                .decode(c["decoded"].as_str().unwrap())
                .unwrap();
            assert_eq!(got.unwrap_or_else(|e| panic!("{input}: {e}")), expected, "{input}");
        }
    }
    assert_eq!(parse_pbkdf2_salt(&json!({})), Err(SyncParamsError::NoSalt));
    assert_eq!(parse_pbkdf2_salt(&json!("x")), Err(SyncParamsError::NotAnObject));
}
