//! Property tests for the merge's safety bar: no automatic merge drops a line
//! (or a JSON value) that either side added, nor brings back one a side
//! deleted; changes are placed by base position; two runs inserted at one
//! point come out whole, the older first; `merge3` agrees with `merge`; and a
//! merge is symmetric up to the order the mtimes give.
use livesync_merge::{
    merge, merge3, Input, Kind, Labels, Merge, Outcome, Overlap, Reason, Rule, Settings, Side, Version,
};
use proptest::prelude::*;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// 1000 cases, or `PROPTEST_CASES` when set (`with_cases` would override it).
fn config() -> ProptestConfig {
    let mut c = ProptestConfig::default();
    if std::env::var_os("PROPTEST_CASES").is_none() {
        c.cases = 1000;
    }
    c
}

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
        0..16u8,
    )
        .prop_map(|(base, eo, et, crlf, finals, blank)| {
            let eol = if crlf { "\r\n" } else { "\n" };
            let mut c = Case {
                ours: join(&apply(&base, &eo), eol, finals[1]),
                theirs: join(&apply(&base, &et), eol, finals[2]),
                base: join(&base, eol, finals[0]),
            };
            // Now and then a side emptied, or left with only whitespace.
            match blank {
                0 => c.ours = String::new(),
                1 => c.theirs = eol.repeat(2),
                2 => c.ours = " \t".into(),
                _ => {}
            }
            c
        })
}

/// Markdown with frontmatter: a few keys, each side setting, adding or
/// deleting one or two of them, and editing the body.
fn markdown_case() -> impl Strategy<Value = Case> {
    const KEYS: [&str; 5] = ["title", "tags", "status", "date", "aliases"];
    type Fm = std::collections::BTreeMap<&'static str, u8>;
    let fm = prop::collection::btree_map(prop::sample::select(&KEYS[..]), 0..4u8, 0..4);
    let edits = || prop::collection::vec((prop::sample::select(&KEYS[..]), 0..5u8), 0..3);
    let side = move || (edits(), prop::collection::vec(edit(), 0..4));
    (fm, prop::collection::vec(word(), 0..6), side(), side()).prop_map(|(fb, body, (ko, eo), (kt, et))| {
        let render = |m: &Fm, body: &[String]| {
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
        // 4 deletes the key; anything else sets it.
        let apply_keys = |edits: &[(&'static str, u8)]| {
            let mut m: Fm = fb.clone();
            for (k, v) in edits {
                if *v == 4 {
                    m.remove(k);
                } else {
                    m.insert(k, *v);
                }
            }
            m
        };
        Case {
            base: render(&fb, &body),
            ours: render(&apply_keys(&ko), &apply(&body, &eo)),
            theirs: render(&apply_keys(&kt), &apply(&body, &et)),
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

/// A base line a side deleted must not be in the result more often than on
/// that side (a lost-update guard relies on a deletion being carried). Only meaningful where
/// no side inserts a line the base has (which would be a move).
fn keeps_deletions(c: &Case, out: &str) -> Result<(), TestCaseError> {
    let (b, o, t, m) = (counts(&c.base), counts(&c.ours), counts(&c.theirs), counts(out));
    for (line, nb) in &b {
        let (no, nt) = (o.get(line).copied().unwrap_or(0), t.get(line).copied().unwrap_or(0));
        if no <= *nb && nt <= *nb {
            let got = m.get(line).copied().unwrap_or(0);
            prop_assert!(
                got <= no.min(nt),
                "line {line:?} base {nb}x, ours {no}x, theirs {nt}x, merged {got}x\n{c:?}\nmerged {out:?}"
            );
        }
    }
    Ok(())
}

/// A side emptied (blank) against a base with content never merges, unless
/// both sides are the same.
fn emptied_stays_open(c: &Case, m: &Merge) -> Result<(), TestCaseError> {
    let blank = |s: &str| s.bytes().all(|b| b.is_ascii_whitespace());
    if !blank(&c.base) && (blank(&c.ours) || blank(&c.theirs)) && c.ours != c.theirs {
        prop_assert_eq!(m.rule, Rule::Emptied, "{:?}\n{:?}", c, m);
        prop_assert!(
            matches!(&m.outcome, Outcome::Open(o) if matches!(o.as_slice(), [Overlap::Whole(Reason::Emptied(_))])),
            "{:?}\n{:?}",
            c,
            m
        );
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
    #![proptest_config(config())]

    #[test]
    fn line_merges_keep_every_added_line(c in text_case(), ours_newer in any::<bool>()) {
        let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
        // As Markdown too: `---` lines make frontmatter blocks out of plain text.
        for kind in [Kind::Text, Kind::Markdown] {
            let m = run(kind, &c, mo, mt);
            emptied_stays_open(&c, &m)?;
            if let Outcome::Write(out) = m.outcome {
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

/// Two runs inserted at one point of a base of distinct lines, each side maybe
/// also changing a line away from that point.
#[derive(Clone, Debug)]
struct Runs {
    base: Vec<String>,
    at: usize,
    ours: Vec<String>,
    theirs: Vec<String>,
    ours_change: Option<usize>,
    theirs_change: Option<usize>,
    ours_newer: bool,
}

fn runs_case() -> impl Strategy<Value = Runs> {
    let shared = || {
        prop::collection::vec(
            prop::option::weighted(0.4, prop::sample::select(&["```", "", "X", "- item"][..])),
            4,
        )
    };
    (
        0..8usize,
        any::<prop::sample::Index>(),
        1..5usize,
        1..5usize,
        shared(),
        shared(),
    )
        .prop_flat_map(|(n, at, ko, kt, so, st)| {
            let at = at.index(n + 1);
            // A change away from the insertion point: not the line before or after it.
            let away: Vec<usize> = (0..n).filter(|q| *q + 1 != at && *q != at).collect();
            let change = if away.is_empty() {
                Just(None).boxed()
            } else {
                prop::option::of(prop::sample::select(away)).boxed()
            };
            (
                Just((n, at, ko, kt)),
                Just((so, st)),
                change.clone(),
                change,
                any::<bool>(),
                prop::sample::select(&["entry", "- item", "", "  detail"][..]),
            )
        })
        .prop_map(|((n, at, ko, kt), (so, st), oc, tc, ours_newer, word)| Runs {
            base: (0..n).map(|i| format!("base {i}")).collect(),
            at,
            // Some lines both runs may share (fences, blank lines).
            ours: (0..ko)
                .map(|i| so[i].map_or_else(|| format!("{word} ours {i}"), str::to_string))
                .collect(),
            theirs: (0..kt)
                .map(|i| st[i].map_or_else(|| format!("{word} theirs {i}"), str::to_string))
                .collect(),
            ours_change: oc,
            theirs_change: tc.filter(|q| Some(*q) != oc),
            ours_newer,
        })
}

impl Runs {
    fn side(&self, run: &[String], change: Option<usize>, other: &[String], other_change: Option<usize>) -> String {
        let mut v = Vec::new();
        for (i, l) in self.base.iter().enumerate() {
            if i == self.at {
                v.extend(run.iter().cloned());
                v.extend(other.iter().cloned());
            }
            v.push(if Some(i) == change || Some(i) == other_change {
                format!("{l} changed")
            } else {
                l.clone()
            });
        }
        if self.at == self.base.len() {
            v.extend(run.iter().cloned());
            v.extend(other.iter().cloned());
        }
        join(&v, "\n", true)
    }
}

proptest! {
    #![proptest_config(config())]

    /// Both sides insert a run at one point: the merge is clean and holds the
    /// older side's run whole, then the newer side's; a run the other holds as
    /// a whole prefix or suffix (or the same run) once.
    #[test]
    fn inserted_runs_stay_whole_older_first(r in runs_case()) {
        let c = Case {
            base: join(&r.base, "\n", true),
            ours: r.side(&r.ours, r.ours_change, &[], None),
            theirs: r.side(&r.theirs, r.theirs_change, &[], None),
        };
        let (older, newer) = if r.ours_newer { (&r.theirs, &r.ours) } else { (&r.ours, &r.theirs) };
        let holds = |long: &[String], short: &[String]| long.starts_with(short) || long.ends_with(short);
        let want = if holds(newer, older) {
            r.side(newer, r.ours_change, &[], r.theirs_change)
        } else if holds(older, newer) {
            r.side(older, r.ours_change, &[], r.theirs_change)
        } else {
            r.side(older, r.ours_change, newer, r.theirs_change)
        };
        let (mo, mt) = if r.ours_newer { (3000, 2000) } else { (2000, 3000) };
        for kind in [Kind::Text, Kind::Markdown] {
            match run(kind, &c, mo, mt).outcome {
                Outcome::Write(out) => prop_assert_eq!(std::str::from_utf8(&out).unwrap(), want.as_str(), "{:?}", c),
                Outcome::Identical => prop_assert_eq!(&c.ours, &want),
                other => prop_assert!(false, "{other:?}\n{c:?}"),
            }
        }
    }
}

/// Changes at distinct base positions on each side: per base line, kept or
/// replaced (with zero to two lines) by one side; per gap between lines,
/// lines inserted by one side, both, or none. Never two changes touching the
/// same base lines, so the merge is clean and its order is known.
#[derive(Clone, Debug)]
struct Positions {
    n: usize,
    /// Per base line: 0 kept, 1 ours replaced, 2 theirs replaced; and how many lines.
    lines: Vec<(u8, usize)>,
    /// Per gap (n + 1): 0 none, 1 ours, 2 theirs, 3 both; and how many lines.
    gaps: Vec<(u8, usize)>,
    ours_newer: bool,
}

fn positions_case() -> impl Strategy<Value = Positions> {
    (0..10usize, any::<bool>())
        .prop_flat_map(|(n, ours_newer)| {
            (
                Just(n),
                prop::collection::vec((0..3u8, 0..3usize), n),
                prop::collection::vec((0..4u8, 1..3usize), n + 1),
                Just(ours_newer),
            )
        })
        .prop_map(|(n, lines, mut gaps, ours_newer)| {
            for g in 0..=n {
                let before = if g > 0 { lines[g - 1].0 } else { 0 };
                let after = if g < n { lines[g].0 } else { 0 };
                // Both inserting next to a replaced line: no order to expect.
                if gaps[g].0 == 3 && (before != 0 || after != 0) {
                    gaps[g].0 = 1;
                }
                // Inside one side's replaced block, only that side inserts.
                if before != 0 && before == after && gaps[g].0 != 0 {
                    gaps[g].0 = before;
                }
            }
            Positions {
                n,
                lines,
                gaps,
                ours_newer,
            }
        })
}

impl Positions {
    fn fresh(who: &str, at: &str, k: usize) -> Vec<String> {
        (0..k).map(|i| format!("{who} {at} {i}")).collect()
    }
    /// One side's file (`side` 1 ours, 2 theirs), or the expected merge (0).
    fn text(&self, side: u8) -> String {
        let mut v: Vec<String> = Vec::new();
        let older_first = if self.ours_newer { [2u8, 1] } else { [1, 2] };
        for g in 0..=self.n {
            let (a, k) = self.gaps[g];
            for who in older_first {
                if (a == who || a == 3) && (side == 0 || side == who) {
                    v.extend(Self::fresh(
                        if who == 1 { "ours" } else { "theirs" },
                        &format!("gap {g}"),
                        k,
                    ));
                }
            }
            if g < self.n {
                let (a, k) = self.lines[g];
                if a != 0 && (side == 0 || side == a) {
                    v.extend(Self::fresh(
                        if a == 1 { "ours" } else { "theirs" },
                        &format!("line {g}"),
                        k,
                    ));
                } else {
                    v.push(format!("base {g}"));
                }
            }
        }
        join(&v, "\n", true)
    }
}

proptest! {
    #![proptest_config(config())]

    /// Changes at distinct positions: the merge places each by its base
    /// position (an insertion between base lines i and i + 1 after what the
    /// other side made of line i, before what it made of line i + 1).
    #[test]
    fn changes_are_placed_by_base_position(p in positions_case()) {
        let base: Vec<String> = (0..p.n).map(|i| format!("base {i}")).collect();
        let c = Case { base: join(&base, "\n", true), ours: p.text(1), theirs: p.text(2) };
        let (mo, mt) = if p.ours_newer { (3000, 2000) } else { (2000, 3000) };
        let want = p.text(0);
        for kind in [Kind::Text, Kind::Markdown] {
            let m = run(kind, &c, mo, mt);
            match m.outcome {
                Outcome::Write(out) => prop_assert_eq!(std::str::from_utf8(&out).unwrap(), want.as_str(), "{:?}", c),
                Outcome::Identical => prop_assert_eq!(&c.ours, &want),
                Outcome::Open(o) if c.base.trim().is_empty() || !(c.ours.trim().is_empty() || c.theirs.trim().is_empty()) => {
                    prop_assert!(false, "{o:?}\n{c:?}")
                }
                // An emptied side stays open.
                Outcome::Open(_) => {}
                other => prop_assert!(false, "{other:?}\n{c:?}"),
            }
        }
    }
}

/// `merge3` agrees with `merge`: clean exactly when `merge` writes (or finds
/// the sides identical), and then the same text; every line either side
/// added is in it.
fn merge3_agrees(kind: Kind, c: &Case, mo: i64, mt: i64) -> Result<(), TestCaseError> {
    let input = Input {
        base: Some(Version {
            content: c.base.as_bytes(),
            mtime: 1000,
            deleted: false,
        }),
        ours: Version {
            content: c.ours.as_bytes(),
            mtime: mo,
            deleted: false,
        },
        theirs: Version {
            content: c.theirs.as_bytes(),
            mtime: mt,
            deleted: false,
        },
    };
    let m = merge(kind, &input, &Settings::default());
    let r = merge3(kind, &input, &Settings::default(), &Labels::default()).expect("text");
    match &m.outcome {
        Outcome::Write(w) => {
            prop_assert_eq!(r.conflicts, 0, "{:?}", c);
            prop_assert_eq!(r.text.as_bytes(), w.as_slice(), "{:?}", c);
        }
        Outcome::Identical => prop_assert_eq!((r.conflicts, r.text.as_str()), (0, c.ours.as_str())),
        Outcome::Open(_) => {
            prop_assert!(r.conflicts >= 1, "open, but merge3 has no conflict\n{:?}\n{:?}", c, r);
            if kind != Kind::Object {
                let (b, o) = (counts(&c.base), counts(&r.text));
                for side in [&c.ours, &c.theirs] {
                    for (line, n) in counts(side) {
                        let added = n.saturating_sub(b.get(line).copied().unwrap_or(0));
                        // A last line without its newline gets one before a marker.
                        let with = |e: &str| o.get(format!("{line}{e}").as_str()).copied().unwrap_or(0);
                        let got = o.get(line).copied().unwrap_or(0)
                            + if line.ends_with('\n') {
                                0
                            } else {
                                with("\n") + with("\r\n")
                            };
                        prop_assert!(got >= added, "{line:?} not shown\n{c:?}\n{}", r.text);
                    }
                }
            }
        }
        Outcome::Keep(_) => prop_assert!(false, "no deleted leaves here"),
    }
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn merge3_agrees_with_merge_on_lines(c in text_case(), ours_newer in any::<bool>()) {
        let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
        for kind in [Kind::Text, Kind::Markdown] {
            merge3_agrees(kind, &c, mo, mt)?;
        }
    }

    #[test]
    fn merge3_agrees_with_merge_on_frontmatter(c in markdown_case(), ours_newer in any::<bool>()) {
        let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
        merge3_agrees(Kind::Markdown, &c, mo, mt)?;
    }

    #[test]
    fn merge3_agrees_with_merge_on_json(c in json_case(), ours_newer in any::<bool>()) {
        let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
        merge3_agrees(Kind::Object, &c, mo, mt)?;
    }
}

/// Distinct base lines; each side deletes, replaces or inserts lines of its
/// own, so a base line missing on a side was deleted there, never moved.
fn fresh_case() -> impl Strategy<Value = Case> {
    let edits = || prop::collection::vec((0..3u8, any::<prop::sample::Index>()), 0..5);
    (
        0..10usize,
        edits(),
        edits(),
        any::<bool>(),
        prop::array::uniform3(prop::bool::weighted(0.8)),
    )
        .prop_map(|(n, eo, et, crlf, finals)| {
            let base: Vec<String> = (0..n).map(|i| format!("base {i}")).collect();
            let side = |who: &str, edits: &[(u8, prop::sample::Index)]| {
                let mut v = base.clone();
                for (k, (op, i)) in edits.iter().enumerate() {
                    match op {
                        0 => v.insert(i.index(v.len() + 1), format!("{who} {k}")),
                        1 if !v.is_empty() => {
                            v.remove(i.index(v.len()));
                        }
                        2 if !v.is_empty() => {
                            let at = i.index(v.len());
                            v[at] = format!("{} by {who} {k}", v[at]);
                        }
                        _ => {}
                    }
                }
                v
            };
            let eol = if crlf { "\r\n" } else { "\n" };
            Case {
                ours: join(&side("ours", &eo), eol, finals[1]),
                theirs: join(&side("theirs", &et), eol, finals[2]),
                base: join(&base, eol, finals[0]),
            }
        })
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn line_merges_carry_every_deletion(c in fresh_case(), ours_newer in any::<bool>()) {
        let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
        for kind in [Kind::Text, Kind::Markdown] {
            let m = run(kind, &c, mo, mt);
            emptied_stays_open(&c, &m)?;
            if let Outcome::Write(out) = m.outcome {
                let out = std::str::from_utf8(&out).unwrap();
                keeps_added_lines(&c, out)?;
                keeps_deletions(&c, out)?;
            }
        }
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

/// A base value one side deleted, and the other left as it was, is not in
/// the result.
fn keeps_deletions_json(c: &Case, out: &str) -> Result<(), TestCaseError> {
    let get = |s: &str| {
        let mut l = Vec::new();
        leaves(&serde_json::from_str(s).unwrap(), String::new(), &mut l);
        l
    };
    let (b, o, t, m) = (get(&c.base), get(&c.ours), get(&c.theirs), get(out));
    let has_path = |l: &[(String, String)], p: &str| l.iter().any(|(q, _)| q == p);
    for leaf in &b {
        for (side, other) in [(&o, &t), (&t, &o)] {
            if !has_path(side, &leaf.0) && other.contains(leaf) {
                prop_assert!(
                    !has_path(&m, &leaf.0),
                    "{leaf:?} deleted, but merged\n{c:?}\nmerged {out}"
                );
            }
        }
    }
    Ok(())
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
    #![proptest_config(config())]

    #[test]
    fn object_merges_keep_every_added_value(c in json_case(), ours_newer in any::<bool>()) {
        let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
        if let Outcome::Write(out) = run(Kind::Object, &c, mo, mt).outcome {
            keeps_added_values(&c, std::str::from_utf8(&out).unwrap())?;
            keeps_deletions_json(&c, std::str::from_utf8(&out).unwrap())?;
        }
    }

    #[test]
    fn object_merges_are_symmetric(c in json_case()) {
        symmetric(&run(Kind::Object, &c, 2000, 3000), &run(Kind::Object, &swapped(&c), 3000, 2000))?;
    }
}

/// The generators reach real merges, not just opens: a share of each one's
/// cases is written.
#[test]
fn generators_reach_merges() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let mut share = |name: &str, strat: BoxedStrategy<Case>, kind: Kind, rule: Rule, min: f64| {
        let (mut written, n) = (0, 2000);
        for i in 0..n {
            let c = strat.new_tree(&mut runner).unwrap().current();
            let (mo, mt) = if i % 2 == 0 { (2000, 3000) } else { (3000, 2000) };
            let m = run(kind, &c, mo, mt);
            if m.rule == rule && matches!(m.outcome, Outcome::Write(_)) {
                written += 1;
            }
        }
        let got = written as f64 / n as f64;
        eprintln!("{name}: {:.0}% written", got * 100.0);
        assert!(got >= min, "{name}: only {:.0}% of cases merge", got * 100.0);
    };
    share("lines", text_case().boxed(), Kind::Text, Rule::Lines, 0.3);
    share(
        "frontmatter",
        markdown_case().boxed(),
        Kind::Markdown,
        Rule::Frontmatter,
        0.15,
    );
    share("json", json_case().boxed(), Kind::Object, Rule::Object, 0.3);
}
