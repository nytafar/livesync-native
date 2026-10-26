//! Tests driven by fixtures from `tools/golden` (upstream commonlib 0.2.1).
use livesync_model::splitter::{chunk_file, SplitParams};
use livesync_model::{chunk, codec, path, FileKind};
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

/// Inputs for `splitter.json`, regenerated exactly as `tools/golden/generate.mjs` does.
const VOCAB: [&str; 28] = [
    "the ",
    "and ",
    "of ",
    "note ",
    "LiveSync ",
    " ",
    " ",
    "\n",
    "\n\n",
    "# Heading\n",
    "- [ ] task\n",
    "[[link]] ",
    "æøå ",
    "ÆØÅ ",
    "Straße ",
    "日本語の",
    "テキスト",
    "😀",
    "🎉 ",
    "ü",
    "€",
    "\t",
    "x",
    "é",
    "```js\nconst a = 1;\n```\n",
    "| a | b |\n",
    "> quote\n",
    "CRLF\r\n",
];

fn xorshift(mut s: u32) -> impl FnMut() -> u32 {
    move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    }
}

fn make_input(recipe: &Value) -> Vec<u8> {
    let n = || recipe["bytes"].as_u64().unwrap() as usize;
    let mut out = match recipe["kind"].as_str().unwrap() {
        "literal" => recipe["text"].as_str().unwrap().as_bytes().to_vec(),
        "repeat" => recipe["unit"]
            .as_str()
            .unwrap()
            .repeat(recipe["count"].as_u64().unwrap() as usize)
            .into_bytes(),
        "vocab" => {
            let mut r = xorshift(recipe["seed"].as_u64().unwrap() as u32);
            let mut s = String::new();
            while s.len() < n() {
                s.push_str(VOCAB[(r() as usize) % VOCAB.len()]);
            }
            s.into_bytes()
        }
        "random" => {
            let mut r = xorshift(recipe["seed"].as_u64().unwrap() as u32);
            (0..n()).map(|_| (r() & 0xff) as u8).collect()
        }
        "fill" => vec![recipe["value"].as_u64().unwrap() as u8; n()],
        other => panic!("unknown recipe {other}"),
    };
    if let Some(p) = recipe.get("prefix").and_then(Value::as_str) {
        let mut v = p.as_bytes().to_vec();
        v.append(&mut out);
        out = v;
    }
    out
}

fn fnv1a(b: &[u8]) -> u32 {
    b.iter()
        .fold(0x811c_9dc5u32, |h, &x| (h ^ x as u32).wrapping_mul(0x0100_0193))
}

#[test]
fn splitter_matches_upstream() {
    let cases = fixture("splitter.json");
    assert!(cases.len() >= 30);
    let (mut text, mut binary) = (0, 0);
    for c in cases {
        let path = c["path"].as_str().unwrap();
        let input = make_input(&c["recipe"]);
        assert_eq!(
            input.len() as u64,
            c["inputBytes"].as_u64().unwrap(),
            "{path}: recipe drift"
        );
        assert_eq!(
            fnv1a(&input) as u64,
            c["inputFnv1a"].as_u64().unwrap(),
            "{path}: recipe drift"
        );
        let params = SplitParams {
            custom_chunk_size: c["customChunkSize"].as_f64().unwrap(),
            minimum_chunk_size: c["minimumChunkSize"].as_u64().unwrap() as usize,
        };
        let out = chunk_file(path, &input, &params);
        match c["type"].as_str().unwrap() {
            "plain" => {
                text += 1;
                assert_eq!(out.kind, FileKind::Plain, "{path}");
            }
            _ => {
                binary += 1;
                assert_eq!(out.kind, FileKind::Binary, "{path}");
            }
        }
        assert!(!out.lossy);
        assert_eq!(out.size, input.len() as u64);
        let want: Vec<(u64, &str)> = c["pieces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["len"].as_u64().unwrap(), p["id"].as_str().unwrap()))
            .collect();
        let got: Vec<(u64, &str)> = out
            .chunks
            .iter()
            .map(|(id, data)| {
                let len = match out.kind {
                    FileKind::Plain => data.len() as u64,
                    FileKind::Binary => codec::base64_decode(data).unwrap().len() as u64,
                };
                (len, id.as_str())
            })
            .collect();
        assert_eq!(got, want, "{path}");
    }
    assert!(text >= 20 && binary >= 8, "text {text} binary {binary}");
}
