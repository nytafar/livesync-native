//! The merge corpus from `tools/golden` (upstream commonlib 0.2.1's own
//! `ConflictManager`) as an oracle. Every case must be upstream's result (byte
//! for byte for text, token for token for JSON, which upstream writes
//! unindented and this crate in the file's style), or open where upstream
//! merges (stricter), or carry a deviation whose shape is checked here. Every
//! line merge that writes, labelled or not, must also pass the placement
//! oracle (`oracle/mod.rs`), so no label can cover a wrong merge. The crate's
//! own model of upstream must match upstream's output, case by case.
mod oracle;

use livesync_merge::{
    line_merge_trace, merge, upstream_line_merge, upstream_object_merge, Deviation, Input, Kind, Outcome, Rule,
    Settings, Version,
};
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

fn str_of<'a>(c: &'a Value, k: &str) -> &'a str {
    c[k].as_str().unwrap()
}

fn ours_first(c: &Value) -> bool {
    c["leftMtime"].as_i64().unwrap() <= c["rightMtime"].as_i64().unwrap()
}

/// The crate's model of upstream must be upstream: byte for byte for the line
/// merge on every text case, token for token for the object merge wherever the
/// model covers the input. The deviation checks below lean on it.
#[test]
fn the_model_of_upstream_is_upstream() {
    let (mut lines, mut objects, mut uncovered) = (0, 0, 0);
    for c in fixture("merge.json") {
        let name = str_of(&c, "name");
        let upstream = c["result"].as_str();
        let (b, l, r) = (str_of(&c, "base"), str_of(&c, "left"), str_of(&c, "right"));
        if str_of(&c, "path").ends_with(".md") {
            assert_eq!(
                upstream_line_merge(b, l, r, ours_first(&c)).as_deref(),
                upstream,
                "{name}: the model of upstream's line merge is wrong\nbase {b:?}\nleft {l:?}\nright {r:?}"
            );
            lines += 1;
            continue;
        }
        if l == r {
            continue;
        }
        match upstream_object_merge(b, l, r, ours_first(&c)) {
            Some(model) => {
                // Up to JavaScript's order of integer-like keys and ids.
                let canon = |s: &str| canonical(serde_json::from_str(s).unwrap());
                assert_eq!(
                    model.as_deref().map(canon),
                    upstream.map(canon),
                    "{name}: the model of upstream's object merge is wrong"
                );
                objects += 1;
            }
            None => uncovered += 1,
        }
    }
    eprintln!("model of upstream: {lines} line merges, {objects} object merges, {uncovered} not covered");
    assert!(uncovered <= 10, "too many object cases outside the model: {uncovered}");
}

/// Objects by key, id-arrays by id, numbers as doubles.
fn canonical(v: Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, canonical(v))).collect()),
        Value::Array(a) if !a.is_empty() && a.iter().all(|e| e.get("id").is_some()) => {
            let mut a: Vec<Value> = a.into_iter().map(canonical).collect();
            a.sort_by_key(|e| e["id"].to_string());
            Value::Array(a)
        }
        Value::Array(a) => Value::Array(a.into_iter().map(canonical).collect()),
        Value::Number(n) => serde_json::json!(n.as_f64()),
        other => other,
    }
}

#[test]
fn merges_match_upstream() {
    let cases = fixture("merge.json");
    assert!(cases.len() >= 600, "corpus too small: {}", cases.len());
    let mut agreed = 0;
    let mut stricter = 0;
    let mut oracle_checked = 0;
    let mut per_deviation: BTreeMap<&str, usize> = BTreeMap::new();
    let mut now_open: BTreeMap<String, usize> = BTreeMap::new();
    for c in &cases {
        let name = str_of(c, "name");
        let upstream = c["result"].as_str().map(str::to_string);
        let kind = if str_of(c, "path").ends_with(".md") {
            Kind::Text
        } else {
            Kind::Object
        };
        let r = run(c, kind);
        if r.rule == Rule::Identical {
            // Rule 1 runs before the merge, upstream (`tryAutoMerge`) as here.
            continue;
        }
        if r.rule == Rule::Lines {
            let (b, l, rt) = (str_of(c, "base"), str_of(c, "left"), str_of(c, "right"));
            let t = line_merge_trace(b, l, rt, ours_first(c)).expect("within the budget");
            assert_eq!(t.text, r.ours, "{name}: the trace is not the merge");
            if let Err(e) = oracle::check(b, l, rt, ours_first(c), &t).and_then(|_| oracle::counts(b, l, rt, &t)) {
                panic!(
                    "{name}: {e} ({:?})\nbase {b:?}\nleft {l:?}\nright {rt:?}\nmerged {:?}\nupstream {upstream:?}",
                    r.deviations, r.ours
                );
            }
            if r.ours.is_some() {
                oracle_checked += 1;
            }
        }
        if name.contains("/deviation-") {
            assert!(
                !r.deviations.is_empty() || (r.ours.is_none() && upstream.is_some()),
                "{name}: expected a deviation or a stricter open, got neither"
            );
        }
        if std::env::var("MERGE_DIAG").is_ok() && (!r.deviations.is_empty() || r.ours != upstream) {
            eprintln!(
                "DIAG {name} {:?}\n  base {:?}\n  left {:?}\n  right {:?}\n  ours {:?}\n  up   {:?}",
                r.deviations, c["base"], c["left"], c["right"], r.ours, upstream
            );
        }
        if r.ours.is_none() && upstream.is_some() {
            *now_open.entry(format!("{:?}", r.rule)).or_default() += 1;
        }
        for d in &r.deviations {
            justify(name, *d, &r.deviations, c, &r.ours, &upstream);
            *per_deviation.entry(d.as_str()).or_default() += 1;
        }
        if r.deviations.is_empty() {
            match (&r.ours, &upstream) {
                // Open where upstream merges: stricter, allowed.
                (None, Some(_)) => stricter += 1,
                (a, b) if kind == Kind::Text => {
                    assert_eq!(
                        a, b,
                        "{name}: line merge differs from upstream\nbase {:?}\nleft {:?}\nright {:?}",
                        c["base"], c["left"], c["right"]
                    );
                    agreed += 1;
                }
                (Some(a), Some(b)) => {
                    assert_eq!(
                        tokens(a),
                        tokens(b),
                        "{name}: object merge differs\nours {a}\nupstream {b}"
                    );
                    agreed += 1;
                }
                (None, None) => agreed += 1,
                (a, b) => panic!("{name}: ours {a:?}, upstream {b:?}"),
            }
        }
    }
    eprintln!(
        "merge corpus: {} cases, {agreed} agree with upstream, {stricter} open where upstream merges \
         (no deviation); {oracle_checked} line merges pass the oracle; per deviation: {per_deviation:?}; \
         open where upstream merges, by rule: {now_open:?}",
        cases.len()
    );
    assert!(
        agreed >= 400,
        "too few cases agree: {agreed}, deviations: {per_deviation:?}"
    );
}

/// Lines without their line end, in order.
fn lines_of(s: &str) -> Vec<String> {
    s.split_inclusive('\n')
        .map(|l| l.trim_end_matches(['\n', '\r']).to_string())
        .collect()
}

fn counts(v: &[String]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for l in v {
        *m.entry(l.clone()).or_default() += 1;
    }
    m
}

/// `a` minus `b`, as multisets.
fn minus(a: &[String], b: &[String]) -> Vec<String> {
    let mut cb = counts(b);
    let mut out = Vec::new();
    for l in a {
        match cb.get_mut(l) {
            Some(n) if *n > 0 => *n -= 1,
            _ => out.push(l.clone()),
        }
    }
    out
}

/// Whether some value along `path` in `v` is a plain array (one whose
/// elements do not all have an `id`), which upstream replaces whole.
fn plain_array_on(v: &Value, path: &[String]) -> bool {
    let mut cur = v;
    for k in path {
        if let Value::Array(a) = cur {
            if !a.iter().all(|e| e.get("id").is_some()) {
                return true;
            }
        }
        cur = match cur {
            Value::Object(o) => match o.get(k) {
                Some(x) => x,
                None => return false,
            },
            Value::Array(a) => match a.iter().find(|e| e.get("id").map(|i| i.to_string()) == Some(k.clone())) {
                Some(x) => x,
                None => return false,
            },
            _ => return false,
        };
    }
    matches!(cur, Value::Array(a) if !a.iter().all(|e| e.get("id").is_some()))
}

/// The paths where two JSON values differ (id-array elements by id).
fn differing(a: &Value, b: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    let by_id = |x: &Vec<Value>| -> Option<BTreeMap<String, Value>> {
        x.iter()
            .map(|e| e.get("id").map(|i| (i.to_string(), e.clone())))
            .collect()
    };
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: std::collections::BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                path.push(k.clone());
                match (x.get(k), y.get(k)) {
                    (Some(p), Some(q)) => differing(p, q, path, out),
                    _ => out.push(path.clone()),
                }
                path.pop();
            }
        }
        (Value::Array(x), Value::Array(y)) => match (by_id(x), by_id(y)) {
            (Some(p), Some(q)) => differing(
                &Value::Object(p.into_iter().collect()),
                &Value::Object(q.into_iter().collect()),
                path,
                out,
            ),
            _ if a != b => out.push(path.clone()),
            _ => {}
        },
        _ if a != b => out.push(path.clone()),
        _ => {}
    }
}

/// A reported deviation must be the kind of difference it names.
fn justify(name: &str, d: Deviation, all: &[Deviation], c: &Value, ours: &Option<String>, upstream: &Option<String>) {
    let inputs: Vec<String> = ["base", "left", "right"]
        .iter()
        .flat_map(|k| lines_of(str_of(c, k)))
        .collect();
    let base = lines_of(str_of(c, "base"));
    let up_glued = upstream
        .as_ref()
        .is_some_and(|u| lines_of(u).iter().any(|l| !inputs.contains(l)));
    // A line merge never makes a line of its own.
    if let (Some(o), Kind::Text) = (ours, kind_of(c)) {
        for l in lines_of(o) {
            assert!(inputs.contains(&l), "{name}: {l:?} is in no input");
        }
    }
    match d {
        Deviation::GluedLines => assert!(up_glued, "{name}: upstream glued nothing: {upstream:?}"),
        Deviation::SameInsertionOnce | Deviation::TrailingDeletion | Deviation::RunOrder | Deviation::PositionOrder => {
            let Some(o) = ours else {
                // Open: only where whole runs would glue a line without its
                // newline that upstream's interleaving does not.
                assert_eq!(d, Deviation::RunOrder, "{name}: {d:?} must be a merge");
                assert!(
                    [str_of(c, "left"), str_of(c, "right")]
                        .iter()
                        .any(|s| !s.is_empty() && !s.ends_with('\n')),
                    "{name}: open, but no run without a final newline"
                );
                return;
            };
            let Some(up) = upstream else {
                // Merged where upstream gives up: only by position.
                assert_eq!(all, [Deviation::PositionOrder], "{name}: upstream did not merge");
                return;
            };
            assert_ne!(o, up, "{name}: {d:?} but the same as upstream");
            let (o, up) = (lines_of(o), lines_of(up));
            // Lines upstream has and this merge does not: base lines a side
            // deleted (a trailing deletion, or by position), or a second copy
            // of an inserted line (taken once).
            for l in minus(&up, &o) {
                let base_line = base.contains(&l);
                let duplicate = counts(&up)[&l] > 1;
                assert!(
                    (base_line
                        && (all.contains(&Deviation::TrailingDeletion) || all.contains(&Deviation::PositionOrder)))
                        || (duplicate && all.contains(&Deviation::SameInsertionOnce))
                        || up_glued,
                    "{name}: {all:?} drops {l:?} that upstream keeps"
                );
            }
            // Lines this merge has more often than upstream: only where
            // upstream glued them away, or dropped them out of place.
            for l in minus(&o, &up) {
                assert!(
                    up_glued || all.contains(&Deviation::PositionOrder) || all.contains(&Deviation::RunOrder),
                    "{name}: {all:?} adds {l:?} upstream does not have"
                );
            }
        }
        Deviation::StricterObjectCheck | Deviation::UnparsableJson | Deviation::EmptiedOpen => {
            assert!(ours.is_none(), "{name}: {d:?} must be open");
            if d == Deviation::EmptiedOpen {
                let blank = |k: &str| str_of(c, k).trim().is_empty();
                assert!(
                    !blank("base") && (blank("left") || blank("right")),
                    "{name}: no side emptied"
                );
            }
        }
        Deviation::ObjectResultDiffers => {
            let o = ours.as_ref().unwrap_or_else(|| panic!("{name}: {d:?} must be a merge"));
            // Outside plain arrays, which upstream replaces whole, the result is
            // upstream's.
            if let Some(up) = upstream {
                let (ov, uv): (Value, Value) = (serde_json::from_str(o).unwrap(), serde_json::from_str(up).unwrap());
                let mut paths = Vec::new();
                differing(&ov, &uv, &mut Vec::new(), &mut paths);
                assert!(!paths.is_empty(), "{name}: same value as upstream");
                for p in paths {
                    let on = |k: &str| plain_array_on(&serde_json::from_str(str_of(c, k)).unwrap(), &p);
                    assert!(
                        on("base") || on("left") || on("right"),
                        "{name}: differs from upstream at {p:?}, not a plain array"
                    );
                }
            }
        }
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

fn kind_of(c: &Value) -> Kind {
    if str_of(c, "path").ends_with(".md") {
        Kind::Text
    } else {
        Kind::Object
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
