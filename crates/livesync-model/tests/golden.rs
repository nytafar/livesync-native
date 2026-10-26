//! Tests driven by fixtures from `tools/golden` (upstream commonlib 0.2.1).
use livesync_model::{chunk, codec, path};
use serde_json::Value;
use std::path::PathBuf;

fn fixture(name: &str) -> Vec<Value> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden")
        .join(name);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["meta"]["version"], "0.2.1", "fixtures must come from commonlib 0.2.1");
    v["cases"].as_array().unwrap().clone()
}

#[test]
fn path_to_id_matches_upstream() {
    let cases = fixture("path2id.json");
    assert!(cases.len() >= 40);
    for c in cases {
        let p = c["path"].as_str().unwrap();
        let ci = c["caseInsensitive"].as_bool().unwrap();
        assert_eq!(path::path_to_id(p, ci), c["id"].as_str().unwrap(), "path {p:?} ci={ci}");
    }
}

#[test]
fn chunk_ids_match_upstream() {
    let cases = fixture("chunk_ids.json");
    assert!(cases.len() >= 10);
    for c in cases {
        let piece = c["piece"].as_str().unwrap();
        assert_eq!(
            chunk::chunk_id_xxhash64(piece),
            c["id"].as_str().unwrap(),
            "piece len {}",
            piece.len()
        );
    }
}

#[test]
fn decompression_matches_upstream() {
    let cases = fixture("decompress.json");
    let mut compressed_seen = 0;
    for c in cases {
        let input = c["compressed"].as_str().unwrap();
        if input.starts_with(codec::COMPRESSED_MARK) {
            compressed_seen += 1;
        }
        let out = codec::decompress(input).unwrap_or_else(|e| panic!("{}: {e}", c["kind"]));
        assert_eq!(out, c["expected"].as_str().unwrap(), "kind {}", c["kind"]);
    }
    assert!(compressed_seen >= 6);
}

#[test]
fn classification_matches_upstream() {
    for c in fixture("classify.json") {
        let p = c["path"].as_str().unwrap();
        assert_eq!(
            path::upstream_should_be_ignored(p),
            c["ignored"].as_bool().unwrap(),
            "{p}"
        );
        assert_eq!(path::is_plain_text(p), c["plainText"].as_bool().unwrap(), "{p}");
        // Our skip rule is a superset of upstream's.
        if c["ignored"].as_bool().unwrap() {
            assert!(path::classify_path(p).is_some(), "{p}");
        }
    }
}
