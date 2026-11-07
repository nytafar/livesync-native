//! The merge corpus from `tools/golden` (upstream commonlib 0.2.1's own
//! `ConflictManager`): where this crate reports no deviation, its result must be
//! upstream's, byte for byte for text and token for token for JSON (upstream
//! writes JSON unindented, this crate in the file's style).
use livesync_merge::{merge, Deviation, Input, Kind, Outcome, Rule, Settings, Version};
use serde_json::Value;
use std::collections::BTreeMap;
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

/// JSON as a token stream: punctuation, decoded strings, numbers as doubles.
#[derive(Debug, PartialEq)]
enum Tok {
    P(char),
    S(String),
    N(f64),
    L(&'static str),
}

fn tokens(s: &str) -> Vec<Tok> {
    let b: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '{' | '}' | '[' | ']' | ':' | ',' => {
                out.push(Tok::P(c));
                i += 1;
            }
            '"' => {
                let start = i;
                i += 1;
                while b[i] != '"' {
                    if b[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                let raw: String = b[start..i].iter().collect();
                out.push(Tok::S(serde_json::from_str::<String>(&raw).unwrap()));
            }
            't' => {
                out.push(Tok::L("true"));
                i += 4;
            }
            'f' => {
                out.push(Tok::L("false"));
                i += 5;
            }
            'n' => {
                out.push(Tok::L("null"));
                i += 4;
            }
            _ => {
                let start = i;
                while i < b.len() && "+-.eE0123456789".contains(b[i]) {
                    i += 1;
                }
                let raw: String = b[start..i].iter().collect();
                out.push(Tok::N(raw.parse().unwrap()));
            }
        }
    }
    out
}

struct Run {
    ours: Option<String>,
    rule: Rule,
    deviations: Vec<Deviation>,
}

fn run(c: &Value, kind: Kind) -> Run {
    let s = |k: &str| c[k].as_str().unwrap().as_bytes();
    let n = |k: &str| c[k].as_i64().unwrap();
    let input = Input {
        base: Some(Version {
            content: s("base"),
            mtime: 1000,
            deleted: false,
        }),
        ours: Version {
            content: s("left"),
            mtime: n("leftMtime"),
            deleted: false,
        },
        theirs: Version {
            content: s("right"),
            mtime: n("rightMtime"),
            deleted: false,
        },
    };
    let m = merge(kind, &input, &Settings::default());
    let ours = match m.outcome {
        Outcome::Write(b) => Some(String::from_utf8(b).unwrap()),
        // Upstream's merge of identical sides is that content.
        Outcome::Identical => Some(c["left"].as_str().unwrap().to_string()),
        Outcome::Keep(_) => panic!("no deleted leaves in the corpus"),
        Outcome::Open(_) => None,
    };
    Run {
        ours,
        rule: m.rule,
        deviations: m.deviations,
    }
}

#[test]
fn merges_match_upstream() {
    let cases = fixture("merge.json");
    assert!(cases.len() >= 600, "corpus too small: {}", cases.len());
    let mut agreed = 0;
    let mut deviated: BTreeMap<String, usize> = BTreeMap::new();
    for c in &cases {
        let name = c["name"].as_str().unwrap();
        let upstream = c["result"].as_str().map(str::to_string);
        let kind = if c["path"].as_str().unwrap().ends_with(".md") {
            Kind::Text
        } else {
            Kind::Object
        };
        let r = run(c, kind);
        if r.rule == Rule::Identical {
            // Rule 1 runs before the merge, upstream (`tryAutoMerge`) as here.
            continue;
        }
        if name.contains("/deviation-") {
            assert!(!r.deviations.is_empty(), "{name}: expected a deviation, got none");
        }
        if !r.deviations.is_empty() {
            if std::env::var("MERGE_DIAG").is_ok() {
                eprintln!(
                    "DIAG {name} {:?} ours_open={} up_open={}\n  base {:?}\n  left {:?}\n  right {:?}\n  ours {:?}\n  up   {:?}",
                    r.deviations,
                    r.ours.is_none(),
                    upstream.is_none(),
                    c["base"],
                    c["left"],
                    c["right"],
                    r.ours,
                    upstream
                );
            }
            for d in &r.deviations {
                justify(name, *d, c, &r.ours, &upstream);
            }
            *deviated.entry(format!("{:?}", r.deviations)).or_default() += 1;
            continue;
        }
        match kind {
            Kind::Text => assert_eq!(
                r.ours, upstream,
                "{name}: line merge differs from upstream\nbase {:?}\nleft {:?}\nright {:?}",
                c["base"], c["left"], c["right"]
            ),
            _ => match (&r.ours, &upstream) {
                (Some(a), Some(b)) => assert_eq!(
                    tokens(a),
                    tokens(b),
                    "{name}: object merge differs\nours {a}\nupstream {b}"
                ),
                (a, b) => assert_eq!(a.is_some(), b.is_some(), "{name}: ours {a:?}, upstream {b:?}"),
            },
        }
        agreed += 1;
    }
    eprintln!(
        "merge corpus: {} cases, {agreed} agree with upstream, deviations: {deviated:?}",
        cases.len()
    );
    assert!(agreed >= 500, "too few cases agree: {agreed}, deviations: {deviated:?}");
}

/// A reported deviation must be the kind of difference it names.
fn justify(name: &str, d: Deviation, c: &Value, ours: &Option<String>, upstream: &Option<String>) {
    let lines = |s: &str| {
        s.split_inclusive('\n')
            .map(|l| l.trim_end_matches(['\n', '\r']).to_string())
            .collect::<Vec<_>>()
    };
    let inputs: Vec<String> = ["base", "left", "right"]
        .iter()
        .flat_map(|k| lines(c[*k].as_str().unwrap()))
        .collect();
    match d {
        Deviation::GluedLines => {
            assert!(ours.is_none(), "{name}: glued lines must stay open");
            if let Some(up) = upstream {
                assert!(
                    lines(up).iter().any(|l| !inputs.contains(l)),
                    "{name}: upstream glued nothing: {up:?}"
                );
            }
        }
        Deviation::SameInsertionOnce => {
            let up = upstream
                .as_ref()
                .unwrap_or_else(|| panic!("{name}: upstream did not merge"));
            if let Some(o) = ours {
                assert!(
                    lines(o).len() < lines(up).len(),
                    "{name}: not shorter than upstream's {up:?}: {o:?}"
                );
            }
        }
        Deviation::StricterObjectCheck | Deviation::UnparsableJson => {
            assert!(ours.is_none(), "{name}: {d:?} must be open");
        }
        Deviation::ObjectResultDiffers => assert!(ours.is_some(), "{name}: {d:?} must be a merge"),
        Deviation::NumericKeyOrder => {
            let (o, up) = (ours.as_ref().unwrap(), upstream.as_ref().unwrap());
            let sorted = |s: &str| {
                let mut t: Vec<String> = tokens(s).iter().map(|t| format!("{t:?}")).collect();
                t.sort();
                t
            };
            assert_eq!(sorted(o), sorted(up), "{name}: more than key order differs");
        }
        other => panic!("{name}: unexpected deviation {other:?} in the corpus"),
    }
}

/// Markdown routes to the line merge unless both sides changed the frontmatter;
/// where it does, it is upstream's merge.
#[test]
fn markdown_is_the_line_merge_unless_frontmatter_applies() {
    for c in fixture("merge.json") {
        if !c["path"].as_str().unwrap().ends_with(".md") {
            continue;
        }
        let text = run(&c, Kind::Text);
        let md = run(&c, Kind::Markdown);
        if md.rule == Rule::Lines && md.deviations.is_empty() {
            assert_eq!(md.ours, text.ours, "{}", c["name"]);
        }
    }
}

#[test]
fn object_merge_writes_the_files_own_style() {
    let cases = fixture("merge.json");
    let c = cases.iter().find(|c| c["name"] == "object/canvas-add-both").unwrap();
    let r = run(c, Kind::Object);
    let out = r.ours.unwrap();
    assert!(out.starts_with("{\n\t\"nodes\": [\n\t\t{"), "{out}");
    assert!(out.ends_with("}\n"));
    assert_eq!(tokens(&out), tokens(c["result"].as_str().unwrap()));
}
