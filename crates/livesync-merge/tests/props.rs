//! Property tests for the merge's safety bar: no automatic merge drops a line
//! (or a JSON value) that either side added, nor brings back one a side
//! deleted; changes are placed by base position; two runs inserted at one
//! point come out whole, the older first; `merge3` agrees with `merge`; and a
//! merge is symmetric up to the order the mtimes give.
//!
//! The primary oracle for line merges is ground truth (`truth/mod.rs`):
//! streams build a base and apply a known edit script to each side, so the
//! true merge comes from what each side did to each line, not from the
//! texts. A clean merge must be the true merge, or fall in one of the named
//! history-ambiguous classes, each with its own exact check. The streams are
//! the reviewers' (review 5's truth and notes streams, review 6's dup fuzz)
//! and the lost-update guard's, with a writer that saw R2 and a stale one.
//! Every clean line merge is also checked against the secondary oracles
//! (`oracle/mod.rs`), which read the texts alone.
mod oracle;
mod truth;

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

// ---------- ground truth: the primary oracle ----------

/// Every clean line merge of a history (`truth::judge`), in each side and
/// mtime order, and the secondary oracles (`passes_oracle`) on each.
fn right_by_history(h: &truth::History, seed: u64) -> Result<(), TestCaseError> {
    if h.artifact() {
        return Ok(());
    }
    let [b, o, t] = h.texts();
    if o == t || o == b || t == b {
        return Ok(());
    }
    // As a note (a line merge unless the frontmatter rule applies) for a
    // third of the seeds.
    let kind = if seed.is_multiple_of(3) {
        Kind::Markdown
    } else {
        Kind::Text
    };
    if let Err(e) = judged(h, kind) {
        prop_assert!(false, "{}", e);
    }
    let c = Case {
        base: b,
        ours: o,
        theirs: t,
    };
    for (mo, mt) in [(2000, 3000), (3000, 2000)] {
        passes_oracle(&c, mo, mt)?;
        passes_oracle(&swapped(&c), mo, mt)?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    /// Review 5's truth stream (repeated lines, moves, copies, line ends,
    /// final newlines): every clean merge is the true merge of the edit
    /// history, or falls in a named history-ambiguous class.
    #[test]
    fn clean_merges_are_the_true_merge_of_the_history(seed in any::<u64>()) {
        right_by_history(&truth::truth_stream(seed), seed)?;
    }

    /// Review 5's realistic notes (paragraphs, tasks, headings, `---`, blank
    /// lines; edits, ticks, paragraph breaks, appends, moves): the same.
    #[test]
    fn notes_merge_to_the_true_merge_of_the_history(seed in any::<u64>()) {
        right_by_history(&truth::notes_stream(seed), seed)?;
    }

    /// Review 6's dup fuzz: no line both sides added is taken twice where
    /// the history has it once, and every clean merge is right.
    #[test]
    fn the_dup_fuzz_merges_to_the_true_merge(seed in any::<u64>()) {
        right_by_history(&truth::dup_stream(seed).0, seed)?;
    }

    /// The lost-update guard, a writer that saw R2 (L is R2 with the
    /// writer's script): a clean merge is the true merge, which is L unless
    /// the writer reverted or moved R2's own lines (no text shows that).
    #[test]
    fn a_writer_that_saw_the_change_gets_the_true_merge(seed in any::<u64>()) {
        right_by_history(&truth::guard_stream(seed, Some(true)).0, seed)?;
    }

    /// The guard's silent path, a stale writer (L is R1 with the writer's
    /// script, never having seen R2): a clean merge equal to L reads as "L
    /// already holds R2", so R2's change would be lost without a word. It may
    /// be L only where the true merge is L, or the texts cannot show the
    /// difference (a named class).
    #[test]
    fn a_stale_writer_is_never_taken_for_one_that_saw_the_change(seed in any::<u64>()) {
        right_by_history(&truth::guard_stream(seed, Some(false)).0, seed)?;
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
    let history = |f: fn(u64) -> truth::History| {
        any::<u64>().prop_map(move |seed| {
            let [base, ours, theirs] = f(seed).texts();
            Case { base, ours, theirs }
        })
    };
    share(
        "truth stream",
        history(truth::truth_stream).boxed(),
        Kind::Text,
        Rule::Lines,
        0.2,
    );
    share(
        "notes stream",
        history(truth::notes_stream).boxed(),
        Kind::Markdown,
        Rule::Lines,
        0.5,
    );
    share(
        "dup stream",
        history(|s| truth::dup_stream(s).0).boxed(),
        Kind::Text,
        Rule::Lines,
        0.2,
    );
    share(
        "stale writer",
        history(|s| truth::guard_stream(s, Some(false)).0).boxed(),
        Kind::Text,
        Rule::Lines,
        0.2,
    );
    share(
        "ordinary notes",
        ordinary_case().prop_map(|(c, _, _)| c).boxed(),
        Kind::Text,
        Rule::Lines,
        0.99,
    );
}

/// Review 7's journal notes with a task carried forward (`truth::journal_stream`):
/// every clean merge is the true merge or a named ambiguity, and where the
/// other side's edits are all in sections the carry does not touch, the
/// merge is clean in every side and mtime order (finding 1: a carry used to
/// open on any change of the other side in the gaps it touched).
fn a_carry_forward_merges(h: &truth::History, apart: bool, seed: u64) -> Result<(), TestCaseError> {
    right_by_history(h, seed)?;
    let [b, o, t] = h.texts();
    if !apart || o == b || t == b || o == t {
        return Ok(());
    }
    let c = Case {
        base: b,
        ours: o,
        theirs: t,
    };
    for (mo, mt) in [(2000, 3000), (3000, 2000)] {
        for x in [c.clone(), swapped(&c)] {
            let m = run(Kind::Text, &x, mo, mt);
            prop_assert!(
                matches!(m.outcome, Outcome::Write(_)),
                "open: {:?}\nbase {:?}\nours {:?}\ntheirs {:?}\n{}",
                m.outcome,
                x.base,
                x.ours,
                x.theirs,
                h.describe()
            );
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    /// A task carried forward in a journal merges where the other side's
    /// edits are elsewhere, and is never merged wrong.
    #[test]
    fn a_task_carried_forward_merges_with_edits_elsewhere(seed in any::<u64>()) {
        let (h, apart) = truth::journal_stream(seed);
        a_carry_forward_merges(&h, apart, seed)?;
    }
}

// ---------- ground truth: the judge is no rubber stamp ----------

/// Review 7's garbles, each a merge the judge once let through under a
/// history-ambiguous class. Each history's true merge passes; the garble
/// does not. (Its third, a stale writer's file, is right by an as short
/// history: see `the_judge_accepts_a_line_both_put_in_once`.)
#[test]
fn the_judge_rejects_review_7s_garbles() {
    use truth::{edited, file, History};
    let cases = [
        // Smaller had no count bound where the true merge is clean: `- buy
        // milk`, which both added, twice.
        (
            History {
                base: file(
                    &[
                        (0, "line 1"),
                        (1, "## Notes"),
                        (2, "line 2"),
                        (3, ""),
                        (4, "line 3"),
                        (5, "line 4"),
                        (6, "line 5"),
                    ],
                    false,
                    true,
                ),
                ours: edited(
                    file(
                        &[
                            (1001, "line 1"),
                            (0, "line 1"),
                            (1, "## Notes"),
                            (2, "line 2"),
                            (1002, "- buy milk"),
                            (3, ""),
                            (4, "line 3"),
                            (5, "line 4"),
                            (6, "line 5"),
                        ],
                        false,
                        true,
                    ),
                    &[],
                ),
                theirs: edited(
                    file(
                        &[
                            (1003, "line 4"),
                            (0, "line 1"),
                            (1, "## Notes"),
                            (3, ""),
                            (4, "line 3"),
                            (2, "line 2"),
                            (1004, "- buy milk"),
                            (5, "line 4"),
                            (6, "line 5"),
                        ],
                        false,
                        true,
                    ),
                    &[3, 4],
                ),
            },
            "line 1\nline 4\nline 1\n## Notes\n- buy milk\n\nline 3\nline 2\n- buy milk\nline 4\nline 5\n",
        ),
        // Copies exempted a line by its text wherever two runs at one point
        // went in whole: `line 1` four times, the true merge has it three.
        (
            History {
                base: file(&[(0, "line 1"), (1, "line 1")], false, false),
                ours: edited(
                    file(&[(0, "line 1"), (1, "line 1"), (1001, "line 1")], false, false),
                    &[],
                ),
                theirs: edited(
                    file(
                        &[(0, "line 1"), (1, "line 1"), (1002, "t 3"), (1003, "line 1")],
                        false,
                        true,
                    ),
                    &[],
                ),
            },
            "line 1\nline 1\nline 1\nt 3\nline 1\n",
        ),
    ];
    for (h, garble) in cases {
        let truth = h.truth(true).expect("a clean true merge");
        assert_eq!(truth::judge(&h, &truth, true), Ok(truth::Class::Exact));
        assert!(
            truth::judge(&h, garble, true).is_err(),
            "{garble:?} passes the judge\n{}",
            h.describe()
        );
    }
}

/// Review 7, finding 4: merges the judge took for wrong where two histories
/// allow them. Each is the true merge of a history as short as the real one.
#[test]
fn the_judge_accepts_what_two_histories_allow() {
    use truth::{edited, file, History};
    let lines =
        |ids: std::ops::Range<usize>| -> Vec<(usize, String)> { ids.map(|i| (i, format!("line {}", i + 1))).collect() };
    fn refs(v: &[(usize, String)]) -> Vec<(usize, &str)> {
        v.iter().map(|(i, t)| (*i, t.as_str())).collect()
    }
    // Two blocks of four swapped (or the other four moved), and ours put a
    // copy of `line 10` in: the history that moved the other block takes it
    // clean.
    let base = lines(0..16);
    let mut ours = refs(&base);
    ours.insert(3, (1001, "line 10"));
    let mut theirs = refs(&base);
    let block: Vec<(usize, &str)> = theirs.drain(2..6).collect();
    for (y, l) in block.into_iter().enumerate() {
        theirs.insert(6 + y, l);
    }
    let swap = History {
        base: file(&refs(&base), false, true),
        ours: edited(file(&ours, false, true), &[]),
        theirs: edited(file(&theirs, false, true), &[2, 3, 4, 5]),
    };
    let m = "line 1\nline 2\nline 7\nline 8\nline 9\nline 10\nline 3\nline 10\nline 4\nline 5\nline 6\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\n";
    assert!(
        truth::judge(&swap, m, true).is_ok(),
        "{}",
        truth::explain(&swap, m, true)
    );
    // Theirs moved `line 14` up (or four lines down), ours turned `line 11`
    // into a second `---`: `line 14`, which every file has once, moved, so it
    // parts no segments, and the `---` both added are not counted apart from
    // the ones they sit with.
    let base: Vec<(usize, &str)> = vec![
        (0, "line 1"),
        (1, "---"),
        (2, "line 2"),
        (3, ""),
        (4, "line 3"),
        (5, "line 4"),
        (6, "line 5"),
        (7, "line 6"),
        (8, "line 7"),
        (9, "line 8"),
        (10, "line 9"),
        (11, "line 10"),
        (12, "line 11"),
        (13, "---"),
        (14, "line 12"),
        (15, "line 13"),
        (16, "  "),
        (17, "  "),
        (18, "line 14"),
    ];
    let mut ours = base.clone();
    ours.remove(12);
    ours.insert(13, (1002, "---"));
    let mut theirs = base.clone();
    theirs.insert(3, (1001, "---"));
    let tail: Vec<(usize, &str)> = theirs.drain(13..17).collect();
    theirs.extend(tail);
    let moved = History {
        base: file(&base, false, true),
        ours: edited(file(&ours, false, true), &[]),
        theirs: edited(file(&theirs, false, true), &[12, 13, 14, 15]),
    };
    let m = "line 1\n---\nline 2\n---\n\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\n  \n  \nline 14\n---\n---\nline 12\nline 13\n";
    assert_eq!(moved.texts()[2], "line 1\n---\nline 2\n---\n\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\n  \n  \nline 14\nline 11\n---\nline 12\nline 13\n");
    assert!(
        truth::judge(&moved, m, true).is_ok(),
        "{}",
        truth::explain(&moved, m, true)
    );
}

/// Review 8, finding 2: a line both sides put in at one point, taken once,
/// where one side also removed a copy of it. That side's reading as putting
/// the line in and removing another copy, where it really moved the lines
/// around it, gives a merge with fewer copies than ours plus theirs less the
/// base: the true merge of a history shorter than the real one, or as short.
#[test]
fn the_judge_accepts_a_line_both_put_in_once() {
    use truth::{edited, file, History};
    // Ours moved `B` and `C` down past `---` (or `---` up), theirs put a
    // `---` in after `A`.
    let h = History {
        base: file(&[(0, "A"), (1, "B"), (2, "C"), (3, "---")], false, true),
        ours: edited(file(&[(0, "A"), (3, "---"), (1, "B"), (2, "C")], false, true), &[1, 2]),
        theirs: edited(
            file(&[(0, "A"), (10, "---"), (1, "B"), (2, "C"), (3, "---")], false, true),
            &[],
        ),
    };
    assert_eq!(h.texts()[1], "A\n---\nB\nC\n");
    assert_eq!(h.texts()[2], "A\n---\nB\nC\n---\n");
    let m = "A\n---\nB\nC\n";
    for ours_first in [true, false] {
        assert!(
            truth::judge(&h, m, ours_first).is_ok(),
            "{}",
            truth::explain(&h, m, ours_first)
        );
    }
    // Ours moved a blank line and `- x` down past a `---` and turned the
    // last `---` into `--- o` (without a final newline); theirs put a `---`
    // in after `line 1`. The true merge has two `---`; ours read as putting
    // one in after `line 1` (once, with theirs) has one.
    let base = [(0, "- x"), (1, "line 1"), (2, ""), (3, "- x"), (4, "---"), (5, "---")];
    let h = History {
        base: file(&base, false, true),
        ours: edited(
            file(
                &[
                    (0, "- x"),
                    (1, "line 1"),
                    (4, "---"),
                    (2, ""),
                    (3, "- x"),
                    (1001, "--- o"),
                ],
                false,
                false,
            ),
            &[2, 3],
        ),
        theirs: edited(
            file(
                &[
                    (0, "- x"),
                    (1, "line 1"),
                    (1002, "---"),
                    (2, ""),
                    (3, "- x"),
                    (4, "---"),
                    (5, "---"),
                ],
                false,
                true,
            ),
            &[],
        ),
    };
    let [b, o, t] = h.texts();
    assert_eq!(b, "- x\nline 1\n\n- x\n---\n---\n");
    assert_eq!(o, "- x\nline 1\n---\n\n- x\n--- o");
    assert_eq!(t, "- x\nline 1\n---\n\n- x\n---\n---\n");
    for ours_first in [true, false] {
        assert!(
            truth::judge(&h, &o, ours_first).is_ok(),
            "{}",
            truth::explain(&h, &o, ours_first)
        );
    }
    // The first case with one line where it moved two (review 7's third
    // garble): ours moved `---` down past `line 1`, which reads as well as
    // `line 1` moved up, as short a history. Theirs put a `line 1` in at the
    // top: by that reading, both put it in there, once, and ours removed the
    // other one.
    let h = History {
        base: file(&[(0, "---"), (1, "line 1")], true, true),
        ours: edited(file(&[(1, "line 1"), (0, "---")], true, true), &[0]),
        theirs: edited(file(&[(1001, "line 1"), (0, "---"), (1, "line 1")], true, true), &[]),
    };
    let m = "line 1\r\n---\r\n";
    for ours_first in [true, false] {
        assert_eq!(
            truth::judge(&h, m, ours_first),
            Ok(truth::Class::Twin),
            "{}",
            truth::explain(&h, m, ours_first)
        );
    }
}

/// Lines once in every file of `files` (not blank), by content.
fn once_in_all(files: &[&str]) -> std::collections::HashSet<String> {
    let counts: Vec<HashMap<String, usize>> = files
        .iter()
        .map(|f| {
            let mut n: HashMap<String, usize> = HashMap::new();
            for l in f.split_inclusive('\n').filter(|l| !l.trim().is_empty()) {
                *n.entry(l.trim_end_matches(['\n', '\r']).to_string()).or_default() += 1;
            }
            n
        })
        .collect();
    counts[0]
        .iter()
        .filter(|(k, _)| counts.iter().all(|n| n.get(*k) == Some(&1)))
        .map(|(k, _)| k.clone())
        .collect()
}

/// Lines (not blank) every file of `files` has once that no side moved: for
/// each other file, in every longest run of them in the first file's order
/// that the file keeps.
fn unmoved(files: &[&str]) -> std::collections::HashSet<String> {
    let key = |l: &str| l.trim_end_matches(['\n', '\r']).to_string();
    let at: Vec<HashMap<String, usize>> = files
        .iter()
        .map(|f| f.split_inclusive('\n').enumerate().map(|(i, l)| (key(l), i)).collect())
        .collect();
    let mut v: Vec<String> = once_in_all(files).into_iter().collect();
    v.sort_by_key(|k| at[0][k]);
    let n = v.len();
    let mut keep: std::collections::HashSet<String> = v.iter().cloned().collect();
    for p in &at[1..] {
        let rises = |x: usize, y: usize| p[&v[x]] < p[&v[y]];
        let mut end = vec![1usize; n];
        for y in 0..n {
            for x in 0..y {
                if rises(x, y) {
                    end[y] = end[y].max(end[x] + 1);
                }
            }
        }
        let mut start = vec![1usize; n];
        for x in (0..n).rev() {
            for y in x + 1..n {
                if rises(x, y) {
                    start[x] = start[x].max(start[y] + 1);
                }
            }
        }
        let longest = end.iter().copied().max().unwrap_or(0);
        let some: Vec<usize> = (0..n).filter(|&x| end[x] + start[x] - 1 == longest).collect();
        let every: std::collections::HashSet<&String> = some
            .iter()
            .filter(|&&x| some.iter().filter(|&&y| end[y] == end[x]).count() == 1)
            .map(|&x| &v[x])
            .collect();
        keep.retain(|k| every.contains(k));
    }
    keep
}

/// The true merge of a history, mutated: a line put in once more, a line
/// dropped, two lines next to each other swapped. The judge rejects every
/// mutant no reading of the texts can give by the placement rules. These
/// conditions come from what the rules can do with any reading, not from the
/// judge's count bounds (which a mutant may pass and still be no reading's
/// merge, or fail where a reading gives it):
///
/// - a line (not blank) more often than ours and theirs together: every
///   copy in a merge is a base line both sides kept or a line one side put
///   in, and each side's copies are the ones it kept and the ones it put in;
/// - a line less often than a side has it, less the base's copies: every
///   line a side put in is in the merge (two sides' lines at one point taken
///   once leave one), and of the base copies it kept, the other side can
///   have removed at most all the base has;
/// - a line base and both sides each have once, dropped from the merge:
///   each side kept the base's copy or put one in, and where one side
///   removed the base's copy, its own copy is the one it put in;
/// - two lines (not blank) every file has once swapped, where base and both
///   sides have them in the same order and no side moved either, nor any
///   lines past them (both are in every longest run of such lines each file
///   keeps in the base's order: two sides' moves compose).
///
/// Each case tries a few mutants, from every stream.
fn mutants_are_rejected(h: &truth::History, seed: u64) -> Result<(), TestCaseError> {
    if h.artifact() {
        return Ok(());
    }
    let [b, o, t] = h.texts();
    if o == t || o == b || t == b {
        return Ok(());
    }
    let ours_first = seed.is_multiple_of(2);
    let Some(w) = h.truth(ours_first) else {
        return Ok(());
    };
    let key = |l: &str| l.trim_end_matches(['\n', '\r']).to_string();
    let n = |s: &str, k: &str| s.split_inclusive('\n').filter(|l| key(l) == k).count();
    let wl: Vec<String> = w.split_inclusive('\n').map(str::to_string).collect();
    if wl.is_empty() {
        return Ok(());
    }
    let ids = |f: &truth::File| f.lines.iter().map(|l| l.id).collect::<Vec<_>>();
    let moved = |text: &str| {
        [&h.ours, &h.theirs].iter().any(|s| {
            s.moved.iter().any(|id| {
                s.file.lines.iter().any(|l| l.id == *id && l.text == text)
                    || h.base.lines.iter().any(|l| l.id == *id && l.text == text)
            })
        })
    };
    let _ = ids;
    let once = once_in_all(&[&b, &o, &t, &w]);
    let pos = |s: &str, k: &str| s.split_inclusive('\n').position(|l| key(l) == k);
    let mut r = truth::Rng(seed ^ 0x5eed);
    for _ in 0..6 {
        let i = r.below(wl.len());
        let line = &wl[i];
        let k = key(line);
        let mut v = wl.clone();
        let (what, must) = match r.below(3) {
            0 if !line.trim().is_empty() => {
                let mut x = line.clone();
                if !x.ends_with('\n') {
                    x.push('\n');
                }
                // Next to the line it copies.
                v.insert(i + r.below(2), x);
                let m = v.concat();
                ("a line put in again", n(&m, &k) > n(&o, &k) + n(&t, &k))
            }
            1 if !line.trim().is_empty() => {
                v.remove(i);
                let m = v.concat();
                let (cm, cb, co, ct) = (n(&m, &k), n(&b, &k), n(&o, &k), n(&t, &k));
                let every = cb == 1 && co == 1 && ct == 1 && cm == 0;
                ("a line dropped", cm + cb < co.max(ct) || every)
            }
            _ if i + 1 < wl.len() => {
                let q = key(&wl[i + 1]);
                v.swap(i, i + 1);
                let still = unmoved(&[&b, &o, &t, &w]);
                let fixed = once.contains(&k)
                    && still.contains(&k)
                    && still.contains(&q)
                    && k != q
                    && !moved(&k)
                    && !moved(&q)
                    && wl[i].ends_with('\n')
                    && wl[i + 1].ends_with('\n')
                    && [&b, &o, &t].iter().all(|f| pos(f, &k) < pos(f, &q));
                ("two lines swapped", fixed)
            }
            _ => continue,
        };
        if !must {
            continue;
        }
        let m = v.concat();
        match truth::judge(h, &m, ours_first) {
            Err(_) => {}
            Ok(c) => prop_assert!(
                false,
                "{what} passes as {c:?}\nbase {b:?}\nours {o:?}\ntheirs {t:?}\ntrue {w:?}\nmutant {m:?} (ours_first {ours_first})\n{}",
                h.describe()
            ),
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    /// The ground-truth judge rejects mutants of true merges from every
    /// stream (see `mutants_are_rejected`).
    #[test]
    fn the_judge_rejects_mutated_true_merges(seed in any::<u64>()) {
        let h = match seed % 5 {
            0 => truth::truth_stream(seed),
            1 => truth::notes_stream(seed),
            2 => truth::dup_stream(seed).0,
            3 => truth::guard_stream(seed, Some(true)).0,
            _ => truth::guard_stream(seed, Some(false)).0,
        };
        mutants_are_rejected(&h, seed)?;
    }
}

// ---------- ground truth: tallies ----------

/// Every clean line merge of a history in each side and mtime order, judged
/// against its ground truth (`truth::judge`); `Err` with the first wrong one.
fn judged(h: &truth::History, kind: Kind) -> Result<Vec<Option<truth::Class>>, String> {
    let [b, o, t] = h.texts();
    let mut out = Vec::new();
    for swap in [false, true] {
        let hs = if swap {
            truth::History {
                base: h.base.clone(),
                ours: h.theirs.clone(),
                theirs: h.ours.clone(),
            }
        } else {
            h.clone()
        };
        let c = if swap {
            Case {
                base: b.clone(),
                ours: t.clone(),
                theirs: o.clone(),
            }
        } else {
            Case {
                base: b.clone(),
                ours: o.clone(),
                theirs: t.clone(),
            }
        };
        for ours_newer in [false, true] {
            let (mo, mt) = if ours_newer { (3000, 2000) } else { (2000, 3000) };
            let m = run(kind, &c, mo, mt);
            // The frontmatter rule merges by key, which the line rules do
            // not model; its own properties judge it.
            if m.rule != Rule::Lines {
                continue;
            }
            let got = match m.outcome {
                Outcome::Write(w) => Some(String::from_utf8(w).unwrap()),
                Outcome::Identical => Some(c.ours.clone()),
                _ => None,
            };
            match got {
                None => out.push(None),
                Some(m) => match truth::judge(&hs, &m, !ours_newer) {
                    Ok(k) => out.push(Some(k)),
                    Err(e) => {
                        return Err(format!(
                            "{e}\nbase {:?}\nours {:?}\ntheirs {:?}\nmerged {m:?} (ours_first {})\n{}\n{}",
                            c.base,
                            c.ours,
                            c.theirs,
                            !ours_newer,
                            hs.describe(),
                            truth::explain(&hs, &m, !ours_newer)
                        ))
                    }
                },
            }
        }
    }
    Ok(out)
}

/// Tallies of the ground-truth judge over the streams: `N` seeds (default
/// 100k) from `SEED`, printed. Not part of the gate:
/// `cargo test --release -p livesync-merge --test props tallies -- --ignored --nocapture`.
#[test]
#[ignore]
fn tallies() {
    let n: u64 = std::env::var("N").ok().and_then(|x| x.parse().ok()).unwrap_or(100_000);
    let seed: u64 = std::env::var("SEED").ok().and_then(|x| x.parse().ok()).unwrap_or(0);
    let show: usize = std::env::var("SHOW").ok().and_then(|x| x.parse().ok()).unwrap_or(3);
    let streams: [(&str, &dyn Fn(u64) -> truth::History); 6] = [
        ("truth", &truth::truth_stream),
        ("notes", &truth::notes_stream),
        ("dup", &|s| truth::dup_stream(s).0),
        ("guard-fresh", &|s| truth::guard_stream(s, Some(true)).0),
        ("guard-stale", &|s| truth::guard_stream(s, Some(false)).0),
        ("journal", &|s| truth::journal_stream(s).0),
    ];
    for (name, gen) in streams {
        let mut tally: std::collections::BTreeMap<String, usize> = Default::default();
        let mut wrong: Vec<String> = Vec::new();
        for k in 0..n {
            let hs = seed.wrapping_mul(1_000_003).wrapping_add(k);
            let h = gen(hs);
            if h.artifact() {
                continue;
            }
            let [b, o, t] = h.texts();
            if o == t || o == b || t == b {
                continue;
            }
            if name == "guard-fresh" {
                if let Some(w) = h.truth(true) {
                    if w != o {
                        *tally.entry("fresh: truth is not L".into()).or_default() += 1;
                    }
                }
            }
            let kind = if k.is_multiple_of(3) {
                Kind::Markdown
            } else {
                Kind::Text
            };
            match judged(&h, kind) {
                Ok(v) => {
                    let truth_clean = h.truth(true).is_some();
                    for x in v {
                        let key = match x {
                            None if truth_clean => "open, truth clean".to_string(),
                            None => "open, truth conflict".to_string(),
                            Some(c) => format!("{c:?}"),
                        };
                        *tally.entry(key).or_default() += 1;
                    }
                }
                Err(e) => {
                    *tally.entry("WRONG".into()).or_default() += 1;
                    if wrong.len() < show {
                        wrong.push(format!("stream seed {hs} ({kind:?})\n{e}"));
                    }
                }
            }
        }
        println!("== {name}: {tally:?}");
        for w in &wrong {
            println!("{w}\n");
        }
    }
}
