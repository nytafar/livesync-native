//! Property tests for the merge's safety bar: no automatic merge drops a line
//! (or a JSON value) that either side added, nor brings back one a side
//! deleted; changes are placed by base position; two runs inserted at one
//! point come out whole, the older first; `merge3` agrees with `merge`; and a
//! merge is symmetric up to the order the mtimes give. Every clean line merge
//! is also checked against the oracle (`oracle/mod.rs`), and a placement fuzz
//! checks merges against the ground truth its generator knows.
mod oracle;

use livesync_merge::{
    line_merge_trace, merge, merge3, upstream_line_merge, Input, Kind, Labels, Merge, Outcome, Overlap, Reason, Rule,
    Settings, Side, Version,
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

/// The base's lines, its last line without a newline counted with one too: a
/// side that only added the newline did not add a line.
fn base_counts(base: &str) -> HashMap<String, usize> {
    let mut m: HashMap<String, usize> = counts(base).into_iter().map(|(l, n)| (l.to_string(), n)).collect();
    if let Some(last) = base.split_inclusive('\n').next_back().filter(|l| !l.ends_with('\n')) {
        for eol in ["\n", "\r\n"] {
            *m.entry(format!("{last}{eol}")).or_default() += 1;
        }
    }
    m
}

/// How often `line` is in a text's `counts`, a last line without its newline
/// counted as the same line with one, and the other way round.
fn found(o: &HashMap<&str, usize>, line: &str) -> usize {
    let n = |l: &str| o.get(l).copied().unwrap_or(0);
    match line.strip_suffix('\n') {
        None => n(line) + n(&format!("{line}\n")) + n(&format!("{line}\r\n")),
        Some(l) => n(line) + n(l.strip_suffix('\r').unwrap_or(l)),
    }
}

/// Every line a side has more often than the base must be in the result at
/// least that many times more (a side's last line without its newline may be
/// there with one, where both sides added it).
fn keeps_added_lines(c: &Case, out: &str) -> Result<(), TestCaseError> {
    let (b, o) = (base_counts(&c.base), counts(out));
    for side in [&c.ours, &c.theirs] {
        for (line, n) in counts(side) {
            let added = n.saturating_sub(b.get(line).copied().unwrap_or(0));
            let got = found(&o, line);
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

/// The line merge of `c` (as [`Kind::Text`]) passes the oracle, and its trace
/// is the merge.
fn passes_oracle(c: &Case, ours_mtime: i64, theirs_mtime: i64) -> Result<(), TestCaseError> {
    let m = run(Kind::Text, c, ours_mtime, theirs_mtime);
    if m.rule != Rule::Lines {
        return Ok(());
    }
    let ours_first = ours_mtime <= theirs_mtime;
    let t = line_merge_trace(&c.base, &c.ours, &c.theirs, ours_first).expect("within the budget");
    let written = match &m.outcome {
        Outcome::Write(w) => Some(String::from_utf8(w.clone()).unwrap()),
        _ => None,
    };
    prop_assert_eq!(&t.text, &written, "the trace is not the merge\n{:?}", c);
    if let Err(e) = oracle::check(&c.base, &c.ours, &c.theirs, ours_first, &t) {
        prop_assert!(false, "{}\n{:?}\nmerged {:?}", e, c, written);
    }
    if let Some(m) = &written {
        if let Err(e) = oracle::independent(&c.base, &c.ours, &c.theirs, m, ours_first) {
            prop_assert!(false, "{}\n{:?}\nmerged {:?}", e, c, written);
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
        passes_oracle(&c, mo, mt)?;
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
    /// a whole prefix or suffix (or the same run) once. Where one run has all
    /// the other's lines that are not blank (at least one), in any order, it
    /// is open: likely one run with lines put in it, and both whole would put
    /// them in twice. So it is where one run is only blank lines: the other
    /// side may have typed its lines onto them.
    #[test]
    fn inserted_runs_stay_whole_older_first(r in runs_case()) {
        let c = Case {
            base: join(&r.base, "\n", true),
            ours: r.side(&r.ours, r.ours_change, &[], None),
            theirs: r.side(&r.theirs, r.theirs_change, &[], None),
        };
        let (older, newer) = if r.ours_newer { (&r.theirs, &r.ours) } else { (&r.ours, &r.theirs) };
        let holds = |long: &[String], short: &[String]| long.starts_with(short) || long.ends_with(short);
        let solid = |v: &[String]| -> Vec<String> { v.iter().filter(|l| !l.trim().is_empty()).cloned().collect() };
        let within = |long: &[String], short: &[String]| {
            let (mut long, short) = (solid(long), solid(short));
            !short.is_empty()
                && short.iter().all(|s| match long.iter().position(|l| l == s) {
                    Some(i) => {
                        long.remove(i);
                        true
                    }
                    None => false,
                })
        };
        let blank_only = |v: &[String]| solid(v).is_empty();
        let open = !holds(newer, older)
            && !holds(older, newer)
            && (within(newer, older) || within(older, newer) || blank_only(newer) || blank_only(older));
        let want = if holds(newer, older) {
            r.side(newer, r.ours_change, &[], r.theirs_change)
        } else if holds(older, newer) {
            r.side(older, r.ours_change, &[], r.theirs_change)
        } else {
            r.side(older, r.ours_change, newer, r.theirs_change)
        };
        let (mo, mt) = if r.ours_newer { (3000, 2000) } else { (2000, 3000) };
        passes_oracle(&c, mo, mt)?;
        for kind in [Kind::Text, Kind::Markdown] {
            match run(kind, &c, mo, mt).outcome {
                Outcome::Write(out) if !open => {
                    prop_assert_eq!(std::str::from_utf8(&out).unwrap(), want.as_str(), "{:?}", c)
                }
                Outcome::Identical => prop_assert_eq!(&c.ours, &want),
                Outcome::Open(_) if open => {}
                other => prop_assert!(false, "{other:?}\n{c:?}"),
            }
        }
    }
}

/// Changes at distinct base positions on each side: per base line, kept or
/// replaced (with zero to two lines) by one side; per gap between lines,
/// lines inserted by one side, both, or none. Never two changes touching the
/// same base lines, nor a line one side deleted next to the other side's
/// change (open by design), so the merge is clean and its order is known.
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
        .prop_map(|(n, mut lines, mut gaps, ours_newer)| {
            // A line one side deleted, next to the other side's change: that
            // side may have typed the line back, which stays open. Keep the
            // line instead.
            for i in 0..n {
                let side = lines[i].0;
                if side == 0 || lines[i].1 != 0 {
                    continue;
                }
                let other = |s: u8| s != 0 && s != side;
                let next_to = [i.wrapping_sub(1), i + 1].iter().any(|&j| j < n && other(lines[j].0))
                    || other(gaps[i].0)
                    || other(gaps[i + 1].0);
                if next_to {
                    lines[i].0 = 0;
                }
            }
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
        passes_oracle(&c, mo, mt)?;
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
                let (b, o) = (base_counts(&c.base), counts(&r.text));
                for side in [&c.ours, &c.theirs] {
                    for (line, n) in counts(side) {
                        let added = n.saturating_sub(b.get(line).copied().unwrap_or(0));
                        // A last line without its newline gets one before a marker.
                        let got = found(&o, line);
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
        passes_oracle(&c, mo, mt)?;
    }
}

/// Every kind of change on each side, per base line and per gap between
/// lines, touching ones included, with lines named after where they belong:
/// `b i` base line i, `o l i k`/`t l i k`/`s l i k` a replacement of line i by
/// ours, theirs or both alike, `o g i k`/`t g i k`/`s g i k` lines inserted in
/// gap i (before base line i). So the right merge is known without a merge.
#[derive(Clone, Debug)]
struct Truth {
    n: usize,
    /// Per base line: 0 kept, 1 ours deletes, 2 theirs deletes, 3 both delete,
    /// 4 ours replaces, 5 theirs replaces, 6 both replace alike, 7 both
    /// replace differently; and with how many lines.
    lines: Vec<(u8, usize)>,
    /// Per gap: 0 nothing, 1 ours inserts, 2 theirs inserts, 3 both the same,
    /// 4 both different lines, 5 theirs inserts what ours does and one more, 6
    /// ours what theirs does and one more; and how many lines.
    gaps: Vec<(u8, usize)>,
    ours_newer: bool,
    /// Whether base, ours and theirs end with a newline.
    fin: [bool; 3],
}

fn truth_case() -> impl Strategy<Value = Truth> {
    (
        0..8usize,
        any::<bool>(),
        prop::array::uniform3(prop::bool::weighted(0.85)),
    )
        .prop_flat_map(|(n, ours_newer, fin)| {
            (
                Just(n),
                prop::collection::vec(
                    (prop::sample::select(&[0u8, 0, 0, 1, 2, 3, 4, 5, 6, 7][..]), 1..3usize),
                    n,
                ),
                prop::collection::vec(
                    (prop::sample::select(&[0u8, 0, 0, 1, 2, 3, 4, 5, 6][..]), 1..3usize),
                    n + 1,
                ),
                Just(ours_newer),
                Just(fin),
            )
        })
        .prop_map(|(n, lines, gaps, ours_newer, fin)| Truth {
            n,
            lines,
            gaps,
            ours_newer,
            fin,
        })
}

impl Truth {
    /// Side 1 ours, 2 theirs.
    fn side(&self, side: u8) -> Vec<String> {
        let mut v = Vec::new();
        for g in 0..=self.n {
            let (op, k) = self.gaps[g];
            let mk = |who: &str, k: usize| (0..k).map(|j| format!("{who} g {g} {j}")).collect::<Vec<_>>();
            let extra = |who: &str| format!("{who} g {g} extra");
            match (op, side) {
                (1, 1) => v.extend(mk("o", k)),
                (2, 2) => v.extend(mk("t", k)),
                (3, _) => v.extend(mk("s", k)),
                (4, 1) => v.extend(mk("o", k)),
                (4, 2) => v.extend(mk("t", k)),
                (5, _) | (6, _) => {
                    v.extend(mk("s", k));
                    if (op, side) == (5, 2) || (op, side) == (6, 1) {
                        v.push(extra(if side == 1 { "o" } else { "t" }));
                    }
                }
                _ => {}
            }
            if g < self.n {
                let (op, k) = self.lines[g];
                let mk = |who: &str, k: usize| (0..k).map(|j| format!("{who} l {g} {j}")).collect::<Vec<_>>();
                match (op, side) {
                    (0, _) | (1, 2) | (2, 1) | (4, 2) | (5, 1) => v.push(format!("b {g}")),
                    (4, 1) | (7, 1) => v.extend(mk("o", k)),
                    (5, 2) | (7, 2) => v.extend(mk("t", k)),
                    (6, _) => v.extend(mk("s", k)),
                    _ => {}
                }
            }
        }
        v
    }
    fn text(&self, lines: &[String], fin: bool) -> String {
        join(lines, "\n", fin)
    }
    fn case(&self) -> Case {
        let base: Vec<String> = (0..self.n).map(|i| format!("b {i}")).collect();
        Case {
            base: self.text(&base, self.fin[0]),
            ours: self.text(&self.side(1), self.fin[1]),
            theirs: self.text(&self.side(2), self.fin[2]),
        }
    }
    /// Where a line belongs: base line i at 2i + 1, gap i at 2i.
    fn coord(line: &str) -> usize {
        let w: Vec<&str> = line.split(' ').collect();
        match w.as_slice() {
            ["b", i] => 2 * i.parse::<usize>().unwrap() + 1,
            [_, "l", i, ..] => 2 * i.parse::<usize>().unwrap() + 1,
            [_, "g", i, ..] => 2 * i.parse::<usize>().unwrap(),
            _ => panic!("{line:?}"),
        }
    }
    /// Whether `out` is a right merge: every line either side added once, the
    /// base lines both kept, nothing else; in base order; each side's lines in
    /// that side's order; two different runs at a gap with nothing changed
    /// beside it whole, the older first.
    fn right(&self, out: &str) -> Result<(), String> {
        let got: Vec<&str> = out.split('\n').filter(|l| !l.is_empty()).collect();
        let (o, t) = (self.side(1), self.side(2));
        let mut want: Vec<&str> = Vec::new();
        for l in o.iter().chain(&t) {
            if !l.starts_with("b ") && !want.contains(&l.as_str()) {
                want.push(l);
            }
        }
        for l in o.iter().filter(|l| l.starts_with("b ") && t.contains(l)) {
            want.push(l);
        }
        for w in &want {
            let n = got.iter().filter(|g| *g == w).count();
            if n != 1 {
                return Err(format!("{w:?} is there {n}x"));
            }
        }
        if let Some(x) = got.iter().find(|g| !want.contains(g)) {
            return Err(format!("{x:?} should not be there"));
        }
        if got.windows(2).any(|p| Self::coord(p[0]) > Self::coord(p[1])) {
            return Err("not in base order".into());
        }
        for side in [&o, &t] {
            let mut it = side.iter();
            if !got
                .iter()
                .filter(|l| side.iter().any(|s| s == *l))
                .all(|l| it.any(|s| s == l))
            {
                return Err("a side's own order changed".into());
            }
        }
        for g in 0..=self.n {
            // Unchanged on both sides, its line end included.
            let c = self.case();
            let piece = |text: &str, i: usize| -> Option<String> {
                text.split_inclusive('\n')
                    .find(|l| l.trim_end_matches('\n') == format!("b {i}"))
                    .map(str::to_string)
            };
            let quiet = |i: Option<usize>| {
                i.is_none_or(|i| {
                    let b = piece(&c.base, i);
                    self.lines[i].0 == 0 && piece(&c.ours, i) == b && piece(&c.theirs, i) == b
                })
            };
            if self.gaps[g].0 != 4 || !quiet(g.checked_sub(1)) || !quiet((g < self.n).then_some(g)) {
                continue;
            }
            let at = |who: &str| -> Vec<usize> {
                let p = format!("{who} g {g} ");
                got.iter()
                    .enumerate()
                    .filter(|(_, l)| l.starts_with(&p))
                    .map(|(i, _)| i)
                    .collect()
            };
            let (older, newer) = if self.ours_newer {
                (at("t"), at("o"))
            } else {
                (at("o"), at("t"))
            };
            let block = |v: &[usize]| v.windows(2).all(|w| w[1] == w[0] + 1);
            if !block(&older) || !block(&newer) || older.last() > newer.first() {
                return Err(format!("the runs at gap {g} are not whole, older first"));
            }
        }
        Ok(())
    }
}

proptest! {
    #![proptest_config(config())]

    /// Every clean line merge of every kind of change, touching ones
    /// included, is the right merge by the generator's ground truth, and
    /// passes the oracle.
    #[test]
    fn line_merges_are_right_by_ground_truth(g in truth_case()) {
        let c = g.case();
        let (mo, mt) = if g.ours_newer { (3000, 2000) } else { (2000, 3000) };
        passes_oracle(&c, mo, mt)?;
        if let Outcome::Write(out) = run(Kind::Text, &c, mo, mt).outcome {
            let out = String::from_utf8(out).unwrap();
            if let Err(e) = g.right(&out) {
                prop_assert!(false, "{}\n{:?}\nmerged {:?}\nupstream {:?}", e, c, out,
                    upstream_line_merge(&c.base, &c.ours, &c.theirs, !g.ours_newer));
            }
        }
    }
}

/// A lost-update guard's case: R2 is R1 with remote changes, and L is R2 with
/// a writer's changes that insert lines, or delete or replace lines R1 had,
/// leaving every line R2 added (and every deletion it made) alone. L already
/// holds R2. Lines are distinct; L and R2 end their lines.
fn guard_case() -> impl Strategy<Value = (Case, bool)> {
    let edits = || prop::collection::vec((0..3u8, any::<prop::sample::Index>()), 0..5);
    (0..8usize, edits(), edits(), any::<bool>(), any::<bool>()).prop_map(|(n, remote, writer, l_newer, fin)| {
        let r1: Vec<String> = (0..n).map(|i| format!("- {i:02} entry")).collect();
        let edit =
            |v: &mut Vec<String>, who: &str, edits: &[(u8, prop::sample::Index)], may_touch: &dyn Fn(&str) -> bool| {
                for (k, (op, i)) in edits.iter().enumerate() {
                    match op {
                        0 => {
                            let at = i.index(v.len() + 1);
                            v.insert(at, format!("- {who} {k}"));
                        }
                        1 if !v.is_empty() => {
                            let at = i.index(v.len());
                            if may_touch(&v[at]) {
                                v.remove(at);
                            }
                        }
                        2 if !v.is_empty() => {
                            let at = i.index(v.len());
                            if may_touch(&v[at]) {
                                v[at] = format!("{} by {who} {k}", v[at]);
                            }
                        }
                        _ => {}
                    }
                }
            };
        let mut r2 = r1.clone();
        edit(&mut r2, "remote", &remote, &|_| true);
        let mut l = r2.clone();
        edit(&mut l, "writer", &writer, &|x| r1.iter().any(|b| b == x));
        let c = Case {
            base: join(&r1, "\n", fin),
            ours: join(&l, "\n", true),
            theirs: join(&r2, "\n", true),
        };
        (c, l_newer)
    })
}

proptest! {
    #![proptest_config(config())]

    /// The lost-update guard's contract: where L already holds R2, the merge
    /// of base R1, ours L and theirs R2 is L, or open. (Where L and R2
    /// inserted runs at one point and the writer put lines between R2's, L's
    /// run holds R2's with lines between: open, not both whole.)
    #[test]
    fn a_writer_that_saw_the_remote_change_already_holds_it((c, l_newer) in guard_case()) {
        let (mo, mt) = if l_newer { (3000, 2000) } else { (2000, 3000) };
        passes_oracle(&c, mo, mt)?;
        for kind in [Kind::Text, Kind::Markdown] {
            match run(kind, &c, mo, mt).outcome {
                Outcome::Write(out) => {
                    prop_assert_eq!(String::from_utf8(out).unwrap(), c.ours.clone(), "{:?}", c);
                }
                Outcome::Keep(_) => prop_assert!(false),
                _ => {}
            }
        }
    }
}

/// Which of the reviewers' guard streams (`/tmp/rev4-probe`'s order_guard
/// and `/tmp/rev3-probe`'s guard3): plain lines, blank lines and `---`; also
/// `- x` and copies of lines already there; also copies of R1's lines (a
/// writer typing back text R2 removed or changed).
#[derive(Clone, Copy, Debug)]
enum Stream {
    Plain,
    Copies,
    Orig,
}

/// A lost-update guard's case from one of the streams: R2 is R1 with a
/// remote's inserts (copies too), deletes of R1's lines and edits; L is R2
/// with a writer's inserts (copies of L's lines, or of R1's for `Orig`), and
/// deletes and edits of R1's lines. L holds R2.
///
/// Three things no merge can see are left out, as the texts would show
/// another history: the writer changing a line R2 has more than once (which
/// copy?), the writer copying a line (or typing it again) and changing the
/// original (a move that reads as two separate edits), and an R1 ending in an
/// empty line without a final newline (the text has no such line).
fn guard_stream(stream: Stream) -> impl Strategy<Value = (Case, bool)> {
    const PLAIN: &[&str] = &["", "", "", "---", "u", "u", "u", "u", "u", "u", "u", "u"];
    const COPIES: &[&str] = &["", "", "", "---", "- x", "u", "u", "u", "u", "u"];
    let (pool, ops, len) = match stream {
        Stream::Plain => (PLAIN, 3u8, 4),
        Stream::Copies | Stream::Orig => (COPIES, 4u8, 5),
    };
    type Edits = Vec<(u8, prop::sample::Index, &'static str, prop::sample::Index)>;
    let edits = move || {
        prop::collection::vec(
            (
                0..ops,
                any::<prop::sample::Index>(),
                prop::sample::select(pool),
                any::<prop::sample::Index>(),
            ),
            0..len,
        )
    };
    (
        prop::collection::vec(prop::sample::select(pool), 0..12),
        edits(),
        edits(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            move |(r1, remote, writer, l_newer, fin): (Vec<&str>, Edits, Edits, bool, bool)| {
                let mut fresh = 0;
                let mut word = |w: &str| {
                    if w == "u" {
                        fresh += 1;
                        format!("line {fresh}")
                    } else {
                        w.to_string()
                    }
                };
                let r1: Vec<String> = r1.iter().map(|w| word(w)).collect();
                // Each line with whether it is R1's, untouched.
                let mut r2: Vec<(String, bool)> = r1.iter().map(|l| (l.clone(), true)).collect();
                for (op, i, w, j) in &remote {
                    match op {
                        0 => {
                            let at = i.index(r2.len() + 1);
                            r2.insert(at, (word(w), false));
                        }
                        3 if !r2.is_empty() => {
                            let copy = r2[j.index(r2.len())].0.clone();
                            r2.insert(i.index(r2.len() + 1), (copy, false));
                        }
                        1 if !r2.is_empty() => {
                            let at = i.index(r2.len());
                            if r2[at].1 {
                                r2.remove(at);
                            }
                        }
                        2 if !r2.is_empty() => {
                            let at = i.index(r2.len());
                            if r2[at].1 || matches!(stream, Stream::Plain) {
                                r2[at] = (format!("{} r", r2[at].0), false);
                            }
                        }
                        _ => {}
                    }
                }
                let once: Vec<String> = r2
                    .iter()
                    .filter(|l| r2.iter().filter(|m| m.0 == l.0).count() == 1)
                    .map(|l| l.0.clone())
                    .collect();
                let (mut copied, mut changed): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
                let mut l = r2.clone();
                for (op, i, w, j) in &writer {
                    match op {
                        0 => {
                            // A line typed again (the text of a line the
                            // writer changed, or will) is a copy too.
                            let new = word(w);
                            if changed.contains(&new) {
                                continue;
                            }
                            copied.push(new.clone());
                            let at = i.index(l.len() + 1);
                            l.insert(at, (new, false));
                        }
                        3 => {
                            let copy = match stream {
                                Stream::Orig if !r1.is_empty() => r1[j.index(r1.len())].clone(),
                                Stream::Copies if !l.is_empty() => l[j.index(l.len())].0.clone(),
                                _ => continue,
                            };
                            if !changed.contains(&copy) {
                                copied.push(copy.clone());
                                l.insert(i.index(l.len() + 1), (copy, false));
                            }
                        }
                        1 | 2 if !l.is_empty() => {
                            let at = i.index(l.len());
                            let x = l[at].clone();
                            if !x.1 || !once.contains(&x.0) || copied.contains(&x.0) {
                                continue;
                            }
                            changed.push(x.0.clone());
                            if *op == 1 {
                                l.remove(at);
                            } else {
                                l[at] = (format!("{} w", x.0), false);
                            }
                        }
                        _ => {}
                    }
                }
                let fin = fin || r1.last().is_some_and(|l| l.is_empty());
                let text =
                    |v: &[(String, bool)], f: bool| join(&v.iter().map(|x| x.0.clone()).collect::<Vec<_>>(), "\n", f);
                let c = Case {
                    base: join(&r1, "\n", fin),
                    ours: text(&l, true),
                    theirs: text(&r2, true),
                };
                (c, l_newer)
            },
        )
}

fn guard_streams() -> impl Strategy<Value = (Case, bool)> {
    prop_oneof![
        guard_stream(Stream::Plain),
        guard_stream(Stream::Copies),
        guard_stream(Stream::Orig)
    ]
}

proptest! {
    #![proptest_config(config())]

    /// The lost-update guard, where L holds R2, on the reviewers' streams:
    /// a clean merge of base R1, ours L and theirs R2 is L, in both
    /// directions (no line more often than L, none of L's lines dropped), in
    /// L's order, with L's paragraph breaks; except where the texts cannot
    /// show that L holds R2 (see `oracle::guard`). It also passes the
    /// placement oracle and the one no alignment enters.
    #[test]
    fn a_guard_merge_is_the_writers_file((c, l_newer) in guard_streams()) {
        let (mo, mt) = if l_newer { (3000, 2000) } else { (2000, 3000) };
        passes_oracle(&c, mo, mt)?;
        for kind in [Kind::Text, Kind::Markdown] {
            match run(kind, &c, mo, mt).outcome {
                Outcome::Write(out) => {
                    let out = String::from_utf8(out).unwrap();
                    if let Err(e) = oracle::guard(&c.base, &c.ours, &c.theirs, &out, mo <= mt) {
                        prop_assert!(false, "{}\n{:?}", e, c);
                    }
                }
                Outcome::Keep(_) => prop_assert!(false),
                _ => {}
            }
        }
    }
}

/// A stale writer: R2 and L are each R1 with edits of their own (L never saw
/// R2). Lines from `---`, blank lines, a recurring `- x` and unique lines;
/// each side inserts (fresh lines or copies), deletes, edits and moves lines.
fn stale_case() -> impl Strategy<Value = (Case, bool)> {
    const POOL: &[&str] = &["", "", "---", "- x", "- x", "u", "u", "u", "u", "u"];
    type Edits = Vec<(u8, prop::sample::Index, &'static str, prop::sample::Index)>;
    let edits = || {
        prop::collection::vec(
            (
                0..5u8,
                any::<prop::sample::Index>(),
                prop::sample::select(POOL),
                any::<prop::sample::Index>(),
            ),
            1..4,
        )
    };
    (
        prop::collection::vec(prop::sample::select(POOL), 0..10),
        edits(),
        edits(),
        any::<bool>(),
    )
        .prop_map(|(r1, remote, writer, l_newer): (Vec<&str>, Edits, Edits, bool)| {
            let mut fresh = 0;
            let mut word = |w: &str, who: &str| {
                if w == "u" {
                    fresh += 1;
                    format!("{who} {fresh}")
                } else {
                    w.to_string()
                }
            };
            let r1: Vec<String> = r1.iter().map(|w| word(w, "line")).collect();
            let mut side = |edits: &Edits, who: &str| -> Vec<String> {
                let mut v = r1.clone();
                for (op, i, w, j) in edits {
                    match op {
                        0 => {
                            let at = i.index(v.len() + 1);
                            v.insert(at, word(w, who));
                        }
                        1 if !v.is_empty() => {
                            v.remove(i.index(v.len()));
                        }
                        2 if !v.is_empty() => {
                            let at = i.index(v.len());
                            if !v[at].trim().is_empty() {
                                v[at] = format!("{} {who}", v[at]);
                            }
                        }
                        3 if !v.is_empty() => {
                            let copy = v[j.index(v.len())].clone();
                            v.insert(i.index(v.len() + 1), copy);
                        }
                        4 if v.len() > 1 => {
                            let l = v.remove(j.index(v.len()));
                            v.insert(i.index(v.len() + 1), l);
                        }
                        _ => {}
                    }
                }
                v
            };
            let r2 = side(&remote, "remote");
            let l = side(&writer, "writer");
            let c = Case {
                base: join(&r1, "\n", true),
                ours: join(&l, "\n", true),
                theirs: join(&r2, "\n", true),
            };
            (c, l_newer)
        })
}

/// Whether the writer's file `l` already has the change from `r1` to `r2`, by
/// its text: every line (not blank) R2 has more often than R1, at least as
/// often; every line R2 has less often, at most as often; two lines next to
/// each other in R1 (blank lines aside, each once in every file) that R2
/// parted with a paragraph break or joined, the same where L has them next to
/// each other; and two lines
/// each file has once, that R2 put in the other order, in R2's order.
fn holds_change(r1: &str, l: &str, r2: &str) -> Result<(), String> {
    let count = |s: &str| -> HashMap<String, usize> {
        let mut n = HashMap::new();
        for x in oracle::shape(s) {
            *n.entry(x.1).or_default() += 1;
        }
        n
    };
    let (a, b, c) = (count(r1), count(l), count(r2));
    let get = |n: &HashMap<String, usize>, k: &str| n.get(k).copied().unwrap_or(0);
    for k in a.keys().chain(c.keys()) {
        let (x, y, z) = (get(&a, k), get(&b, k), get(&c, k));
        if (z > x && y < z) || (z < x && y > z) {
            return Err(format!("the line {k:?}: R1 {x}x, R2 {z}x, L {y}x"));
        }
    }
    // Two lines next to each other, each once in all three files: whether a
    // paragraph break parts them.
    let all = [r1, l, r2].map(count);
    let unique = |x: &str| all.iter().all(|n| n.get(x) == Some(&1));
    let pairs = |s: &str| -> HashMap<(String, String), bool> {
        let sh = oracle::shape(s);
        sh.windows(2)
            .filter(|w| unique(&w[0].1) && unique(&w[1].1))
            .map(|w| ((w[0].1.clone(), w[1].1.clone()), w[1].0))
            .collect()
    };
    let (pa, pb, pc) = (pairs(r1), pairs(l), pairs(r2));
    for (k, gap) in &pc {
        if let (Some(was), Some(now)) = (pa.get(k), pb.get(k)) {
            if was != gap && now == was {
                return Err(format!("R2 changed the paragraph break between {k:?}; L has R1's"));
            }
        }
    }
    let once = |s: &str| -> HashMap<String, usize> {
        let sh = oracle::shape(s);
        let mut n: HashMap<String, (usize, usize)> = HashMap::new();
        for (i, x) in sh.iter().enumerate() {
            let e = n.entry(x.1.clone()).or_insert((0, i));
            e.0 += 1;
        }
        n.into_iter().filter(|(_, v)| v.0 == 1).map(|(k, v)| (k, v.1)).collect()
    };
    let (a, b, c) = (once(r1), once(l), once(r2));
    for (p, &pc) in &c {
        for (q, &qc) in &c {
            if let (Some(&pa), Some(&qa), Some(&pb), Some(&qb)) = (a.get(p), a.get(q), b.get(p), b.get(q)) {
                if pc < qc && pa > qa && pb > qb {
                    return Err(format!("R2 put {p:?} before {q:?}; L has R1's order"));
                }
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    /// The lost-update guard's silent path: a writer that never saw R2. A
    /// clean merge equal to L reads as "L already holds R2", so R2's change
    /// would be lost without a word. It may be so only where L's text already
    /// has R2's change (see `holds_change`). Every clean merge also passes
    /// the oracles.
    #[test]
    fn a_stale_writer_is_never_taken_for_one_that_saw_the_change((c, l_newer) in stale_case()) {
        let (mo, mt) = if l_newer { (3000, 2000) } else { (2000, 3000) };
        passes_oracle(&c, mo, mt)?;
        for kind in [Kind::Text, Kind::Markdown] {
            if let Outcome::Write(out) = run(kind, &c, mo, mt).outcome {
                if out == c.ours.as_bytes() {
                    if let Err(e) = holds_change(&c.base, &c.ours, &c.theirs) {
                        prop_assert!(false, "clean and L, but R2's change is lost: {}\n{:?}", e, c);
                    }
                }
            }
        }
    }
}

// ---------- the reviewers' streams ----------

/// A small deterministic generator (SplitMix64), so the reviewers' streams
/// run as they were written, one proptest seed per case.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() % 1_000_000) as f64 / 1e6 < p
    }
}

/// A line with an identity, to know what really happened to it.
#[derive(Clone, Debug)]
struct Line {
    id: usize,
    text: String,
    crlf: bool,
}

fn render(v: &[Line], fin: bool) -> String {
    let mut s = String::new();
    for (k, l) in v.iter().enumerate() {
        s.push_str(&l.text);
        if k + 1 < v.len() || fin {
            s.push_str(if l.crlf { "\r\n" } else { "\n" });
        }
    }
    s
}

/// What each side did, by identity: base lines `a..b` became `items`; `None`
/// where a line moved.
fn truth_changes(base: &[Line], side: &[Line]) -> Option<Vec<(usize, usize, Vec<Line>)>> {
    let pos: HashMap<usize, usize> = base.iter().enumerate().map(|(i, l)| (l.id, i)).collect();
    let mut out = Vec::new();
    let (mut pb, mut items): (usize, Vec<Line>) = (0, Vec::new());
    for l in side {
        if let Some(&i) = pos.get(&l.id) {
            if i < pb {
                return None;
            }
            if i > pb || !items.is_empty() {
                out.push((pb, i, std::mem::take(&mut items)));
            }
            pb = i + 1;
        } else {
            items.push(l.clone());
        }
    }
    if base.len() > pb || !items.is_empty() {
        out.push((pb, base.len(), items));
    }
    Some(out)
}

/// The right merge where the two sides' changes are apart (no change of one
/// touches one of the other, nothing moved): every change in place.
fn apart_truth(base: &[Line], o: &[Line], t: &[Line]) -> Option<Vec<Line>> {
    let (co, ct) = (truth_changes(base, o)?, truth_changes(base, t)?);
    if co.iter().any(|a| ct.iter().any(|b| a.0 <= b.1 && b.0 <= a.1)) {
        return None;
    }
    let mut all: Vec<&(usize, usize, Vec<Line>)> = co.iter().chain(ct.iter()).collect();
    all.sort_by_key(|c| (c.0, c.1));
    let mut v = Vec::new();
    let mut p = 0;
    for c in all {
        v.extend(base[p..c.0].iter().cloned());
        v.extend(c.2.iter().cloned());
        p = c.1;
    }
    v.extend(base[p..].iter().cloned());
    Some(v)
}

/// Review 5's truth stream: lines from a pool (`- x`, tasks, `---`, blank
/// lines, fences, headings, table rows) or unique; each side inserts, deletes,
/// edits, moves and copies lines, toggles CRLF and the final newline.
fn truth_stream(seed: u64) -> (Vec<Line>, bool, Vec<Line>, bool, Vec<Line>, bool, bool) {
    const REP: &[&str] = &["- x", "- [ ] gym", "---", "", "", "```", "## Notes", "  ", "| a | b |"];
    let mut r = Rng(seed);
    let rep = [0.15, 0.45, 0.7][r.below(3)];
    let crlf_p = if r.chance(0.25) { 0.3 } else { 0.0 };
    let mut uq = 0;
    let mut pool = |r: &mut Rng, tag: &str| {
        if r.chance(rep) {
            REP[r.below(REP.len())].to_string()
        } else {
            uq += 1;
            format!("{tag}{uq}")
        }
    };
    let nb = r.below(10);
    let base: Vec<Line> = (0..nb)
        .map(|i| Line {
            id: i,
            text: pool(&mut r, "line "),
            crlf: r.chance(crlf_p),
        })
        .collect();
    let fb = !r.chance(0.2);
    let mut next = 1000;
    let mut side = |r: &mut Rng, who: &str| -> (Vec<Line>, bool) {
        let (mut v, mut fin) = (base.clone(), fb);
        for _ in 0..1 + r.below(3) {
            let n = v.len();
            let mut fresh = |text: String, crlf: bool| {
                next += 1;
                Line { id: next, text, crlf }
            };
            match r.below(20) {
                0..=3 => {
                    let t = pool(r, &format!("{who} "));
                    let c = r.chance(crlf_p);
                    let at = r.below(n + 1);
                    v.insert(at, fresh(t, c));
                }
                4 => {
                    let t = ["new A", "new B", "- x", ""][r.below(4)].to_string();
                    let at = r.below(n + 1);
                    v.insert(at, fresh(t, false));
                }
                5..=8 if n > 0 => {
                    v.remove(r.below(n));
                }
                9..=12 if n > 0 => {
                    let at = r.below(n);
                    if !v[at].text.trim().is_empty() {
                        let suf = if r.chance(0.2) { " e" } else { who };
                        let (t, c) = (format!("{} {suf}", v[at].text), v[at].crlf);
                        v[at] = fresh(t, c);
                    }
                }
                13 | 14 if n > 1 => {
                    let a = r.below(n);
                    if !v[a].text.trim().is_empty() {
                        let l = v.remove(a);
                        let b = r.below(n);
                        v.insert(b, l);
                    }
                }
                15 if n > 0 => {
                    let a = r.below(n);
                    let (t, c) = (v[a].text.clone(), v[a].crlf);
                    let b = r.below(n + 1);
                    v.insert(b, fresh(t, c));
                }
                16 if n > 0 && crlf_p > 0.0 => {
                    let at = r.below(n);
                    let (t, c) = (v[at].text.clone(), !v[at].crlf);
                    v[at] = fresh(t, c);
                }
                17 => fin = !fin,
                _ => {}
            }
        }
        (v, fin)
    };
    let (o, fo) = side(&mut r, "o");
    let (t, ft) = side(&mut r, "t");
    (base, fb, o, fo, t, ft, r.chance(0.5))
}

/// Review 5's realistic notes: paragraphs, task lists (some tasks recur),
/// headings, `---` and blank lines; each side makes one to three ordinary
/// edits: an edit or a tick, a paragraph split or joined, a line or a
/// paragraph added, a line deleted, a log line appended, a task added.
fn notes_stream(seed: u64) -> (Vec<Line>, Vec<Line>, Vec<Line>, bool) {
    let mut r = Rng(seed);
    let flavour = r.below(4);
    let mut ids = 0;
    let mut base = Vec::new();
    let mut push = |v: &mut Vec<Line>, t: String| {
        ids += 1;
        v.push(Line {
            id: ids,
            text: t,
            crlf: false,
        });
    };
    let mut pc = 0;
    for b in 0..2 + r.below(4) {
        if b > 0 {
            push(&mut base, String::new());
        }
        let kind = match flavour {
            3 => 0,
            1 => [0, 1, 1, 1, 2][r.below(5)],
            2 => [0, 0, 0, 3, 2][r.below(5)],
            _ => r.below(5),
        };
        match if b == 0 && kind == 4 { 0 } else { kind } {
            0 | 3 => {
                pc += 1;
                for k in 0..1 + r.below(3) {
                    push(&mut base, format!("Para {pc} sentence {k}."));
                }
            }
            1 => {
                for k in 0..1 + r.below(4) {
                    let t = ["- [ ] gym", "- [ ] water plants", "- [ ] call mom"];
                    let line = if r.chance(0.5) {
                        t[r.below(3)].to_string()
                    } else {
                        format!("- [ ] task {pc}{k}")
                    };
                    push(&mut base, line);
                }
            }
            2 => {
                let h = if r.chance(0.5) {
                    "## Notes".to_string()
                } else {
                    format!("## Heading {b}")
                };
                push(&mut base, h);
            }
            _ => push(&mut base, "---".into()),
        }
    }
    let mut ids = 10_000;
    let mut edit = |r: &mut Rng, v: &mut Vec<Line>, who: &str| {
        let n = v.len();
        let mut fresh = |t: String| {
            ids += 1;
            Line {
                id: ids,
                text: t,
                crlf: false,
            }
        };
        let at = r.below(n.max(1));
        match r.below(10) {
            0 | 1 if n > 0 && !v[at].text.is_empty() => {
                let t = if v[at].text.starts_with("- [ ]") {
                    v[at].text.replacen("- [ ]", "- [x]", 1)
                } else {
                    format!("{} ({who})", v[at].text)
                };
                v[at] = fresh(t);
            }
            2 if n > 0 && !v[at].text.is_empty() && at + 1 < n && !v[at + 1].text.is_empty() => {
                v.insert(at + 1, fresh(String::new()));
            }
            3 if n > 0 && v[at].text.is_empty() => {
                v.remove(at);
            }
            4 => v.insert((at + 1).min(n), fresh(format!("Added by {who}."))),
            5 if n > 0 && !v[at].text.is_empty() => {
                let p = at + 1;
                v.insert(p, fresh(String::new()));
                v.insert(p + 1, fresh(format!("New paragraph by {who}.")));
                if p + 2 < v.len() && !v[p + 2].text.is_empty() {
                    v.insert(p + 2, fresh(String::new()));
                }
            }
            6 if n > 0 && !v[at].text.is_empty() => {
                v.remove(at);
            }
            7 => v.push(fresh(format!("- {who} log"))),
            8 => {
                let t = ["- [ ] milk", "- [ ] gym", "---"][r.below(3)].to_string();
                v.insert((at + 1).min(n), fresh(t));
            }
            9 if n > 0 && v[at].text.starts_with("- [") => v.insert(at + 1, fresh(format!("- [ ] task by {who}"))),
            _ => {}
        }
    };
    let (mut o, mut t) = (base.clone(), base.clone());
    for _ in 0..1 + r.below(3) {
        edit(&mut r, &mut o, "ours");
    }
    for _ in 0..1 + r.below(3) {
        edit(&mut r, &mut t, "theirs");
    }
    (base, o, t, r.chance(0.5))
}

/// Whether a clean merge `m` of a stream case is right, where the truth is
/// known (`want`, the sides' changes apart): `m` is it, or it differs only
/// where the texts cannot show what happened: both sides made the same change
/// at one place (taken once), a side's only change was to add the final
/// newline (no change, by the crate's rule), or the texts read only one way
/// and that gives `m` (the history moved repeated lines no text tells apart).
fn right_where_apart(b: &str, o: &str, t: &str, m: &str, want: &str, ours_first: bool) -> Result<(), String> {
    if m == want
        || m.trim_end_matches(['\n', '\r']) == want.trim_end_matches(['\n', '\r'])
        || oracle::same_change(b, o, t)
        || oracle::every_reading_gives(b, o, t, m, ours_first)
    {
        return Ok(());
    }
    Err(format!("the sides' changes are apart, the merge is not {want:?}"))
}

proptest! {
    #![proptest_config(config())]

    /// Review 5's truth stream: every clean merge passes both oracles, and
    /// where the two sides' changes are apart it is the right merge.
    #[test]
    fn the_truth_stream_merges_right(seed in any::<u64>()) {
        let (base, fb, o, fo, t, ft, ours_newer) = truth_stream(seed);
        // A file that ends in an empty line without a final newline has no
        // such line as text.
        let artifact = |v: &[Line], f: bool| !f && v.last().is_some_and(|l| l.text.is_empty());
        if artifact(&base, fb) || artifact(&o, fo) || artifact(&t, ft) {
            return Ok(());
        }
        let c = Case { base: render(&base, fb), ours: render(&o, fo), theirs: render(&t, ft) };
        if c.ours == c.theirs || c.ours == c.base || c.theirs == c.base {
            return Ok(());
        }
        let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
        passes_oracle(&c, mo, mt)?;
        if let Outcome::Write(m) = run(Kind::Text, &c, mo, mt).outcome {
            let m = String::from_utf8(m).unwrap();
            if let Some(want) = apart_truth(&base, &o, &t) {
                let fin = if fo == fb { ft } else { fo };
                if let Err(e) = right_where_apart(&c.base, &c.ours, &c.theirs, &m, &render(&want, fin), !ours_newer) {
                    prop_assert!(false, "{}\n{:?}\nmerged {:?}", e, c, m);
                }
            }
        }
    }

    /// Review 5's realistic notes stream: every clean merge passes both
    /// oracles, and where the two sides' changes are apart it is the right
    /// merge.
    #[test]
    fn the_notes_stream_merges_right(seed in any::<u64>()) {
        let (base, o, t, ours_newer) = notes_stream(seed);
        let c = Case { base: render(&base, true), ours: render(&o, true), theirs: render(&t, true) };
        if c.ours == c.theirs || c.ours == c.base || c.theirs == c.base {
            return Ok(());
        }
        let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
        passes_oracle(&c, mo, mt)?;
        if let Outcome::Write(m) = run(Kind::Markdown, &c, mo, mt).outcome {
            let m = String::from_utf8(m).unwrap();
            if let Some(want) = apart_truth(&base, &o, &t) {
                if let Err(e) = right_where_apart(&c.base, &c.ours, &c.theirs, &m, &render(&want, true), !ours_newer) {
                    prop_assert!(false, "{}\n{:?}\nmerged {:?}", e, c, m);
                }
            }
        }
    }
}

/// Review 5's ordinary notes (`ordinary4`): sections with headings,
/// paragraphs, recurring tasks, fences, tables, `---`, callouts and `#done`,
/// with or without blank lines between them, or a daily note whose sections
/// repeat the same lines under unique date headings. Two devices change
/// different sections, and maybe both append to the log at the end.
fn ordinary_case() -> impl Strategy<Value = (Case, String, bool)> {
    fn section(kind: u8, k: usize, repeat: bool) -> Vec<String> {
        if repeat {
            let mut v = vec![format!("## 2026-11-{:02}", k + 1)];
            match kind % 3 {
                0 => v.extend(["- [ ] gym", "- [ ] water plants", "", "### Notes"].map(String::from)),
                1 => v.extend(["### Notes", "", "---"].map(String::from)),
                _ => v.extend(["| a | b |", "|---|---|", "", "```", "```"].map(String::from)),
            }
            return v;
        }
        let mut v = vec![format!("## Section {k}")];
        match kind {
            0 => v.extend([format!("Paragraph {k} line one."), format!("Paragraph {k} line two.")]),
            1 => v.extend([
                "- [ ] gym".into(),
                "- [ ] water plants".into(),
                format!("- [ ] task {k}"),
            ]),
            2 => v.extend([format!("Code {k}:"), "```".into(), format!("print({k})"), "```".into()]),
            3 => v.extend(["| a | b |".into(), "|---|---|".into(), format!("| {k} | {k} |")]),
            4 => v.extend([
                format!("Text {k}."),
                "---".into(),
                "> [!note]".into(),
                format!("> note {k}"),
            ]),
            _ => v.extend(["### Notes".into(), format!("note {k}"), "#done".into()]),
        }
        v
    }
    fn change(s: &mut Vec<String>, op: u8, at: prop::sample::Index, who: &str) {
        let n = s.len();
        let i = 1 + at.index(n - 1);
        match op {
            0 => s[i] = format!("{} (edited by {who})", s[i]),
            1 => match (1..n).find(|&j| s[j].starts_with("- [ ]")) {
                Some(j) => s[j] = s[j].replacen("- [ ]", "- [x]", 1),
                None => s[i] = format!("{} (edited by {who})", s[i]),
            },
            2 => s.push(format!("added by {who}")),
            3 => s.insert(i + 1, format!("inserted by {who}")),
            4 if n > 2 => {
                s.remove(i);
            }
            _ => s.insert(i, format!("- [ ] new {who}")),
        }
    }
    type Ops = Vec<(u8, prop::sample::Index)>;
    (
        prop::collection::vec(0..6u8, 2..7),
        any::<bool>(),
        any::<bool>(),
        prop::collection::vec((0..6u8, any::<prop::sample::Index>()), 1..3),
        prop::collection::vec((0..6u8, any::<prop::sample::Index>()), 1..3),
        any::<prop::sample::Index>(),
        any::<prop::sample::Index>(),
        0..4u8,
        any::<bool>(),
    )
        .prop_filter_map(
            "a change that changed nothing",
            |(kinds, repeat, blanks, eo, et, so, st, appends, ours_newer): (
                Vec<u8>,
                bool,
                bool,
                Ops,
                Ops,
                prop::sample::Index,
                prop::sample::Index,
                u8,
                bool,
            )| {
                let base: Vec<Vec<String>> = kinds.iter().enumerate().map(|(k, &x)| section(x, k, repeat)).collect();
                // Two different sections.
                let io = so.index(base.len());
                let it = (io + 1 + st.index(base.len() - 1)) % base.len();
                let (mut o, mut t) = (base.clone(), base.clone());
                for (op, at) in &eo {
                    change(&mut o[io], *op, *at, "ours");
                }
                for (op, at) in &et {
                    change(&mut t[it], *op, *at, "theirs");
                }
                let end_o: Vec<String> = (appends & 1 == 1)
                    .then(|| "- 18:00 ours".to_string())
                    .into_iter()
                    .collect();
                let end_t: Vec<String> = (appends & 2 == 2)
                    .then(|| "- 18:00 theirs".to_string())
                    .into_iter()
                    .collect();
                let render = |secs: &[Vec<String>], end: &[String]| {
                    let mut out = String::new();
                    for (k, s) in secs.iter().enumerate() {
                        if k > 0 && blanks {
                            out.push('\n');
                        }
                        for l in s {
                            out.push_str(l);
                            out.push('\n');
                        }
                    }
                    if blanks {
                        out.push('\n');
                    }
                    out.push_str("## Log\n");
                    for l in end {
                        out.push_str(l);
                        out.push('\n');
                    }
                    out
                };
                let c = Case {
                    base: render(&base, &[]),
                    ours: render(&o, &end_o),
                    theirs: render(&t, &end_t),
                };
                if c.ours == c.base || c.theirs == c.base {
                    return None;
                }
                let mut want = base.clone();
                want[io] = o[io].clone();
                want[it] = t[it].clone();
                let end: Vec<String> = if ours_newer {
                    end_t.iter().chain(&end_o).cloned().collect()
                } else {
                    end_o.iter().chain(&end_t).cloned().collect()
                };
                Some((c, render(&want, &end), ours_newer))
            },
        )
}

proptest! {
    #![proptest_config(config())]

    /// Review 5's ordinary notes: changes in different sections always
    /// merge, and right (ef5a400 and upstream merge every one of them).
    #[test]
    fn ordinary_notes_changed_in_different_sections_merge((c, want, ours_newer) in ordinary_case()) {
        let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
        for kind in [Kind::Text, Kind::Markdown] {
            match run(kind, &c, mo, mt).outcome {
                Outcome::Write(m) => prop_assert_eq!(String::from_utf8(m).unwrap(), want.clone(), "{:?}", c),
                other => prop_assert!(false, "{:?}\n{:?}", other, c),
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

/// The oracle is not a rubber stamp: a merge made wrong in each way the safety
/// bar names fails it.
#[test]
fn the_oracle_rejects_wrong_merges() {
    use livesync_merge::{LineTrace, Src};
    let text = |t: &LineTrace, b: &str, o: &str, th: &str| -> String {
        let (bl, ol, tl): (Vec<&str>, Vec<&str>, Vec<&str>) = (
            b.split_inclusive('\n').collect(),
            o.split_inclusive('\n').collect(),
            th.split_inclusive('\n').collect(),
        );
        t.merged
            .as_ref()
            .unwrap()
            .iter()
            .map(|s| match *s {
                Src::Base(i) => bl[i],
                Src::Ours(j) | Src::Both(j, _) => ol[j],
                Src::Theirs(k) => tl[k],
            })
            .collect()
    };
    let wrong = |b: &str, o: &str, th: &str, f: &dyn Fn(&mut Vec<Src>)| {
        let mut t = line_merge_trace(b, o, th, true).unwrap();
        assert!(oracle::check(b, o, th, true, &t).is_ok(), "the right merge passes");
        f(t.merged.as_mut().unwrap());
        t.text = Some(text(&t, b, o, th));
        oracle::check(b, o, th, true, &t).err()
    };
    let (b, o, t) = ("a\nb\nc\n", "a\nB\nc\nd\n", "x\na\nb\nc\n");
    // A line taken twice.
    assert!(wrong(b, o, t, &|m| m.push(Src::Ours(3))).is_some());
    // A line dropped.
    assert!(wrong(b, o, t, &|m| {
        m.retain(|s| *s != Src::Theirs(0));
    })
    .is_some());
    // Out of a side's order.
    assert!(wrong(b, o, t, &|m| m.swap(1, 3)).is_some());
    // A base line ours replaced, back.
    assert!(wrong(b, o, t, &|m| m.insert(2, Src::Base(1))).is_some());
    // A line glued: theirs' last line without its newline, not last.
    let (b2, o2, t2) = ("a\n", "a\nx\n", "a\ny");
    assert!(wrong(b2, o2, t2, &|m| {
        let last = m.pop().unwrap();
        m.insert(1, last);
    })
    .is_some());
    // An insertion placed against base order: theirs' x after ours' B.
    assert!(wrong(b, o, t, &|m| {
        let x = m.remove(0);
        m.insert(2, x);
    })
    .is_some());
    // A shared insertion taken from both sides.
    let (b4, o4, t4) = ("# Log\n- 09\n", "# Log\n- 09 done\n- 10\n", "# Log\n- 09\n- 10\n");
    assert!(wrong(b4, o4, t4, &|m| {
        // Ours' "- 10" and theirs' "- 10" both, instead of once.
        let both = m.iter().position(|s| matches!(s, Src::Both(..))).unwrap();
        let Src::Both(j, k) = m[both] else { unreachable!() };
        m[both] = Src::Ours(j);
        m.insert(both + 1, Src::Theirs(k));
    })
    .is_some());
}

/// The oracles that no alignment of the merge's enters reject the wrong
/// merges earlier versions made (each found by a review, each from two diffs
/// that lined up repeated lines differently, or a line one side typed again),
/// and the merge now leaves each open. Base R1, L (which saw R2), R2, and the
/// wrong merge.
#[test]
fn the_independent_oracles_reject_what_alignments_hide() {
    let cases = [
        // Review 3: a line both inserted, put at two points across blank
        // lines; a line ours changed, back as theirs' "moved" copy; a copy
        // inserted next to the line it copies.
        (
            "# Day\n- a\n- b\n\n\n",
            "# Day\n- a done\n\n- c\n- d\n",
            "# Day\n- a done\n\n- c\n\n",
            "# Day\n- a done\n- c\n\n- c\n- d\n",
        ),
        (
            "line 1\n\nline 2\n\nline 3\n",
            "line 1\n\n\nline 4\n\n\nline 3 r\n",
            "line 1\n\nline 4\nline 2\n\nline 3 r\n",
            "line 1\n\nline 4\n\nline 4\n\n\nline 3 r\n",
        ),
        (
            "\n\nline 1\nline 2",
            "line 1 w\n\n",
            "line 1\n\nline 2\n",
            "line 1\nline 1 w\n",
        ),
        (
            "\nline 1\nline 2",
            "line 2\nline 3\nline 4\n\n",
            "line 2\nline 3\n",
            "line 2\nline 3\nline 4\nline 3\n",
        ),
        (
            "A\n- x\nB\n",
            "A'\n- x\n- x\nB\n",
            "A\n- x\n- x\nB\n",
            "A'\n- x\n- x\n- x\nB\n",
        ),
        ("B\nC\nB\n", "B\nC r\nB\nC\n", "B\nC r\nB\n", "B\nC r\nB\nC r\n"),
        // Review 4: the writer's new line dropped where it repeats a line
        // the other side edited or deleted; a paragraph break lost; lines
        // out of the writer's order.
        (
            "# Today\n- [ ] water plants\n- [ ] call mom\n",
            "# Today\n- [x] water plants\n- [ ] water plants\n- [ ] call mom\n",
            "# Today\n- [x] water plants\n- [ ] call mom\n",
            "# Today\n- [x] water plants\n- [ ] call mom\n",
        ),
        (
            "Call Bob\nAgenda\n",
            "Called Bob\nAgenda\nCall Bob\n",
            "Called Bob\nAgenda\n",
            "Called Bob\nAgenda\n",
        ),
        ("line 1\nline 2\n", "line 2\nline 1\n- x\n", "line 2\n", "line 2\n- x\n"),
        (
            "Intro\n\nPara A\nPara B\n",
            "Intro\nmore intro\nPara A\n\nPara B\n",
            "Intro\nmore intro\nPara A\nPara B\n",
            "Intro\nmore intro\nPara A\nPara B\n",
        ),
        ("A\nB\n", "X\nY\nA\nB\n", "X\nB\n", "X\nY\nB\n"),
        (
            "# N\n```\n```",
            "# N\nintro\n```\nprint(1)\n```\n",
            "# N\nintro\n```\n```\n",
            "# N\nintro\n```\n```\nprint(1)\n",
        ),
        (
            "line 1\nline 2\n---\n---",
            "line 1\nline 2\nline 3\n---\nline 4\n---\n",
            "line 1\nline 2\nline 3\n---\n---\n",
            "line 1\nline 2\nline 3\n---\n---\nline 4\n",
        ),
    ];
    for (b, l, r2, wrong) in cases {
        for l_first in [true, false] {
            let t = line_merge_trace(b, l, r2, l_first).unwrap();
            assert_eq!(t.merged, None, "open now: {b:?} {l:?} {r2:?}");
            assert!(
                oracle::independent(b, l, r2, wrong, l_first).is_err()
                    || oracle::guard(b, l, r2, wrong, l_first).is_err(),
                "{wrong:?} passes both oracles"
            );
            assert!(
                oracle::guard(b, l, r2, wrong, l_first).is_err(),
                "{wrong:?} passes the guard oracle"
            );
        }
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
    share(
        "guard streams",
        guard_streams().prop_map(|(c, _)| c).boxed(),
        Kind::Text,
        Rule::Lines,
        0.3,
    );
    share(
        "stale writer",
        stale_case().prop_map(|(c, _)| c).boxed(),
        Kind::Text,
        Rule::Lines,
        0.3,
    );
    share(
        "ordinary notes",
        ordinary_case().prop_map(|(c, _, _)| c).boxed(),
        Kind::Text,
        Rule::Lines,
        0.99,
    );
    let notes = any::<u64>().prop_map(|seed| {
        let (b, o, t, _) = notes_stream(seed);
        Case {
            base: render(&b, true),
            ours: render(&o, true),
            theirs: render(&t, true),
        }
    });
    share("notes stream", notes.boxed(), Kind::Markdown, Rule::Lines, 0.5);
    let truth = any::<u64>().prop_map(|seed| {
        let (b, fb, o, fo, t, ft, _) = truth_stream(seed);
        Case {
            base: render(&b, fb),
            ours: render(&o, fo),
            theirs: render(&t, ft),
        }
    });
    share("truth stream", truth.boxed(), Kind::Text, Rule::Lines, 0.2);
}
