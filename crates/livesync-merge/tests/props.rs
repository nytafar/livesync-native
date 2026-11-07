//! Property tests for D9's safety bar: no automatic merge drops a line (or a
//! JSON value) that either side added, and a merge is symmetric up to the
//! order the mtimes give.
use livesync_merge::{merge, Input, Kind, Merge, Outcome, Settings, Side, Version};
use proptest::prelude::*;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

const WORDS: &[&str] = &[
    "alpha",
    "bravo",
    "charlie",
    "",
    "- [ ] task",
    "# Heading",
    "x",
    "y",
    "> quote",
    "---",
    "a: 1",
];

fn word() -> impl Strategy<Value = String> {
    prop::sample::select(WORDS).prop_map(str::to_string)
}

#[derive(Clone, Debug)]
enum Edit {
    Insert(prop::sample::Index, String),
    Delete(prop::sample::Index),
    Replace(prop::sample::Index, String),
    Append(String),
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (any::<prop::sample::Index>(), word()).prop_map(|(i, w)| Edit::Insert(i, w)),
        any::<prop::sample::Index>().prop_map(Edit::Delete),
        (any::<prop::sample::Index>(), word()).prop_map(|(i, w)| Edit::Replace(i, format!("{w} changed"))),
        word().prop_map(Edit::Append),
    ]
}

fn apply(base: &[String], edits: &[Edit]) -> Vec<String> {
    let mut v = base.to_vec();
    for e in edits {
        match e {
            Edit::Insert(i, w) => {
                let at = i.index(v.len() + 1);
                v.insert(at, w.clone());
            }
            Edit::Delete(i) if !v.is_empty() => {
                let at = i.index(v.len());
                v.remove(at);
            }
            Edit::Replace(i, w) if !v.is_empty() => {
                let at = i.index(v.len());
                v[at] = w.clone();
            }
            Edit::Append(w) => v.push(w.clone()),
            _ => {}
        }
    }
    v
}

fn join(lines: &[String], eol: &str, final_newline: bool) -> String {
    let mut s = lines.join(eol);
    if final_newline && !lines.is_empty() {
        s.push_str(eol);
    }
    s
}

#[derive(Clone, Debug)]
struct Case {
    base: String,
    ours: String,
    theirs: String,
}

fn text_case() -> impl Strategy<Value = Case> {
    (
        prop::collection::vec(word(), 0..10),
        prop::collection::vec(edit(), 0..5),
        prop::collection::vec(edit(), 0..5),
        any::<bool>(),
        prop::array::uniform3(prop::bool::weighted(0.8)),
    )
        .prop_map(|(base, eo, et, crlf, finals)| {
            let eol = if crlf { "\r\n" } else { "\n" };
            Case {
                ours: join(&apply(&base, &eo), eol, finals[1]),
                theirs: join(&apply(&base, &et), eol, finals[2]),
                base: join(&base, eol, finals[0]),
            }
        })
}

/// Markdown with frontmatter: a few keys, values that change, a body.
fn markdown_case() -> impl Strategy<Value = Case> {
    let fm = prop::collection::btree_map(
        prop::sample::select(&["title", "tags", "status", "date", "aliases"][..]),
        0..4u8,
        0..4,
    );
    let side = || {
        (
            prop::collection::btree_map(
                prop::sample::select(&["title", "tags", "status", "date", "aliases"][..]),
                0..4u8,
                0..4,
            ),
            prop::collection::vec(edit(), 0..4),
            any::<bool>(),
        )
    };
    (fm, prop::collection::vec(word(), 0..6), side(), side()).prop_map(|(fb, body, (fo, eo, ko), (ft, et, kt))| {
        let render = |m: &std::collections::BTreeMap<&'static str, u8>, body: &[String]| {
            let mut s = String::from("---\n");
            for (k, v) in m {
                if *k == "tags" {
                    s.push_str(&format!("tags:\n  - t{v}\n"));
                } else {
                    s.push_str(&format!("{k}: v{v}\n"));
                }
            }
            s.push_str("---\n");
            s.push_str(&join(body, "\n", true));
            s
        };
        let pick = |keep: bool, side: &std::collections::BTreeMap<&'static str, u8>| {
            // Keep the base block (one-sided change) or take the side's.
            if keep {
                fb.clone()
            } else {
                side.clone()
            }
        };
        Case {
            base: render(&fb, &body),
            ours: render(&pick(ko, &fo), &apply(&body, &eo)),
            theirs: render(&pick(kt, &ft), &apply(&body, &et)),
        }
    })
}

fn run(kind: Kind, c: &Case, ours_mtime: i64, theirs_mtime: i64) -> Merge {
    let input = Input {
        base: Some(Version {
            content: c.base.as_bytes(),
            mtime: 1000,
            deleted: false,
        }),
        ours: Version {
            content: c.ours.as_bytes(),
            mtime: ours_mtime,
            deleted: false,
        },
        theirs: Version {
            content: c.theirs.as_bytes(),
            mtime: theirs_mtime,
            deleted: false,
        },
    };
    merge(kind, &input, &Settings::default())
}

fn counts(s: &str) -> HashMap<&str, usize> {
    let mut m = HashMap::new();
    for l in s.split_inclusive('\n') {
        *m.entry(l).or_default() += 1;
    }
    m
}

/// Every line a side has more often than the base must be in the result at
/// least that many times more.
fn keeps_added_lines(c: &Case, out: &str) -> Result<(), TestCaseError> {
    let (b, o) = (counts(&c.base), counts(out));
    for side in [&c.ours, &c.theirs] {
        for (line, n) in counts(side) {
            let added = n.saturating_sub(b.get(line).copied().unwrap_or(0));
            let got = o.get(line).copied().unwrap_or(0);
            prop_assert!(
                got >= added,
                "line {line:?} added {added}x, merged has {got}x\n{c:?}\nmerged {out:?}"
            );
        }
    }
    Ok(())
}

fn swapped(c: &Case) -> Case {
    Case {
        base: c.base.clone(),
        ours: c.theirs.clone(),
        theirs: c.ours.clone(),
    }
}

/// The same outcome with the sides (and their mtimes) swapped.
fn symmetric(a: &Merge, b: &Merge) -> Result<(), TestCaseError> {
    prop_assert_eq!(a.rule, b.rule);
    match (&a.outcome, &b.outcome) {
        (Outcome::Write(x), Outcome::Write(y)) => prop_assert_eq!(x, y),
        (Outcome::Open(_), Outcome::Open(_)) | (Outcome::Identical, Outcome::Identical) => {}
        (Outcome::Keep(x), Outcome::Keep(y)) => prop_assert_ne!(x == &Side::Ours, y == &Side::Ours),
        (x, y) => prop_assert!(false, "{x:?} vs {y:?}"),
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn line_merges_keep_every_added_line(c in text_case(), ours_newer in any::<bool>()) {
        let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
        // As Markdown too: `---` lines make frontmatter blocks out of plain text.
        for kind in [Kind::Text, Kind::Markdown] {
            if let Outcome::Write(out) = run(kind, &c, mo, mt).outcome {
                keeps_added_lines(&c, std::str::from_utf8(&out).unwrap())?;
            }
        }
    }

    #[test]
    fn line_merges_are_symmetric(c in text_case()) {
        for kind in [Kind::Text, Kind::Markdown] {
            symmetric(&run(kind, &c, 2000, 3000), &run(kind, &swapped(&c), 3000, 2000))?;
        }
    }

    #[test]
    fn frontmatter_merges_keep_every_added_line(c in markdown_case(), ours_newer in any::<bool>()) {
        let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
        if let Outcome::Write(out) = run(Kind::Markdown, &c, mo, mt).outcome {
            keeps_added_lines(&c, std::str::from_utf8(&out).unwrap())?;
        }
    }

    #[test]
    fn frontmatter_merges_are_symmetric(c in markdown_case()) {
        symmetric(&run(Kind::Markdown, &c, 2000, 3000), &run(Kind::Markdown, &swapped(&c), 3000, 2000))?;
    }
}

// ---------- JSON ----------

fn leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        (0..4i64).prop_map(Value::from),
        prop::sample::select(&["a", "b", "1"][..]).prop_map(Value::from),
        any::<bool>().prop_map(Value::from),
        Just(Value::Null),
    ]
}

fn value() -> impl Strategy<Value = Value> {
    leaf().prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(leaf(), 0..3).prop_map(Value::Array),
            prop::collection::btree_map(prop::sample::select(&["p", "q", "r"][..]), inner.clone(), 0..3)
                .prop_map(|m| Value::Object(m.into_iter().map(|(k, v)| (k.to_string(), v)).collect())),
            prop::collection::btree_map(prop::sample::select(&["n1", "n2", "n3"][..]), inner, 0..3)
                .prop_map(|m| { Value::Array(m.into_iter().map(|(id, v)| json!({"id": id, "v": v})).collect()) }),
        ]
    })
}

fn object() -> impl Strategy<Value = Map<String, Value>> {
    prop::collection::btree_map(prop::sample::select(&["a", "b", "c", "d"][..]), value(), 0..4)
        .prop_map(|m| m.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Change the object a little: set, delete or recurse into one key.
fn mutate(o: &Map<String, Value>, ops: &[(u8, prop::sample::Index, Value)]) -> Map<String, Value> {
    let mut o = o.clone();
    for (op, i, v) in ops {
        let keys: Vec<String> = o.keys().cloned().collect();
        match op % 4 {
            0 => {
                o.insert(["a", "b", "c", "d", "e"][i.index(5)].to_string(), v.clone());
            }
            1 if !keys.is_empty() => {
                o.remove(&keys[i.index(keys.len())]);
            }
            2 if !keys.is_empty() => {
                let k = &keys[i.index(keys.len())];
                if let Some(Value::Object(inner)) = o.get(k) {
                    let changed = mutate(inner, &[(0, *i, v.clone())]);
                    o.insert(k.clone(), Value::Object(changed));
                } else if let Some(Value::Array(a)) = o.get(k).cloned() {
                    let mut a = a;
                    let at = i.index(a.len().max(1)) % a.len().max(1);
                    if let Some(Value::Object(e)) = a.get_mut(at) {
                        e.insert("v".into(), v.clone());
                    } else {
                        a.push(json!({"id": format!("x{}", i.index(3)), "v": v}));
                    }
                    o.insert(k.clone(), Value::Array(a));
                }
            }
            _ => {}
        }
    }
    o
}

fn json_case() -> impl Strategy<Value = Case> {
    let ops = || prop::collection::vec((any::<u8>(), any::<prop::sample::Index>(), value()), 0..3);
    (object(), ops(), ops()).prop_map(|(b, eo, et)| Case {
        ours: serde_json::to_string_pretty(&mutate(&b, &eo)).unwrap(),
        theirs: serde_json::to_string_pretty(&mutate(&b, &et)).unwrap(),
        base: serde_json::to_string_pretty(&b).unwrap(),
    })
}

/// Leaves as (path, value): objects walked by key, arrays whose elements all
/// have a unique string `id` walked by id, other arrays whole.
fn leaves(v: &Value, path: String, out: &mut Vec<(String, String)>) {
    match v {
        // An empty object carries no value of its own.
        Value::Object(o) => {
            for (k, x) in o {
                leaves(x, format!("{path}/{k}"), out);
            }
        }
        Value::Array(a) => {
            let ids: Vec<Option<&str>> = a.iter().map(|e| e.get("id").and_then(Value::as_str)).collect();
            let unique = ids.iter().flatten().collect::<std::collections::HashSet<_>>().len() == a.len();
            // An empty array counts as an id-array, as it does to the merge.
            if ids.iter().all(Option::is_some) && unique {
                for (e, id) in a.iter().zip(ids) {
                    leaves(e, format!("{path}[{}]", id.unwrap()), out);
                }
            } else {
                out.push((path, v.to_string()));
            }
        }
        _ => out.push((path, v.to_string())),
    }
}

fn keeps_added_values(c: &Case, out: &str) -> Result<(), TestCaseError> {
    let get = |s: &str| {
        let mut l = Vec::new();
        leaves(&serde_json::from_str(s).unwrap(), String::new(), &mut l);
        l
    };
    let (b, o) = (get(&c.base), get(out));
    for side in [&c.ours, &c.theirs] {
        for leaf in get(side) {
            if !b.contains(&leaf) {
                prop_assert!(o.contains(&leaf), "{leaf:?} lost\n{c:?}\nmerged {out}");
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn object_merges_keep_every_added_value(c in json_case(), ours_newer in any::<bool>()) {
        let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
        if let Outcome::Write(out) = run(Kind::Object, &c, mo, mt).outcome {
            keeps_added_values(&c, std::str::from_utf8(&out).unwrap())?;
        }
    }

    #[test]
    fn object_merges_are_symmetric(c in json_case()) {
        symmetric(&run(Kind::Object, &c, 2000, 3000), &run(Kind::Object, &swapped(&c), 3000, 2000))?;
    }
}
