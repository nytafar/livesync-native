//! The three-way line merge, and the same merge producing conflict hunks for
//! `merge3`.
//!
//! Each side is diffed against the base with diff-match-patch's line mode, as
//! upstream (commonlib's `ConflictManager.mergeSensibly`) does, and each diff
//! becomes a list of changes: base lines `a..b` became these side lines. The
//! two sides' changes are then put together in three steps (see [`walk`]):
//!
//! 1. **Overlaps.** Changes of the two sides that touch the same base lines
//!    form a group, merged only where that is unambiguous.
//! 2. **Placement.** Everything else is placed by base position, so a line
//!    inserted between base lines `i` and `i + 1` comes after everything the
//!    other side made of lines up to `i` and before everything it made of
//!    lines from `i + 1`.
//! 3. **Seams.** Where pieces of the two sides meet at one base point with no
//!    base line between them, a line both sides inserted there could sit in
//!    both pieces. A side's insertion is taken once where the other side's
//!    change there begins or ends with exactly those lines; any other line the
//!    two pieces share leaves them one conflict, never a line taken twice.
//!
//! Each step trusts the two diffs' alignments with the base, and where lines
//! repeat (blank lines, `---`, copies) the two diffs can line up the same
//! lines differently. So a clean result is checked last by line counts alone
//! ([`counts_hold`]), which no alignment enters, and is open where they are
//! not counts a right merge can have.
//!
//! Every merged line records where it comes from ([`Src`]), which lets the
//! tests check a merge against an oracle that knows nothing of how it was
//! made.
//!
//! [`upstream`] is a faithful port of upstream's own walk. It decides nothing
//! here: the merge's result is compared with it to report how it differs.

use crate::dmp::{self, Budget, Op, OutOfBudget};
use crate::{Deviation, LineOverlap};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

/// Where a merged line comes from (by 0-based line index): a base line both
/// sides kept, a line of one side, or a line of each side taken once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Src {
    Base(usize),
    Ours(usize),
    Theirs(usize),
    Both(usize, usize),
}

/// A conflicting region: the base lines `base` and what each side made of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub at: LineOverlap,
    pub base: String,
    pub ours: String,
    pub theirs: String,
}

#[derive(Clone, Debug)]
pub(crate) enum Seg {
    Clean(String),
    Conflict(Hunk),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Walk {
    pub segs: Vec<Seg>,
    pub deviations: Vec<Deviation>,
    /// A line was emitted right after a line without its newline.
    pub glued: bool,
    /// A clean walk's merged lines, by where each comes from; empty otherwise.
    pub trace: Vec<Src>,
    /// For ours and theirs: the side's line for each base line it kept.
    pub align: [Vec<Option<usize>>; 2],
}

impl Walk {
    pub fn is_clean(&self) -> bool {
        self.segs.iter().all(|s| matches!(s, Seg::Clean(_)))
    }
    pub fn hunks(&self) -> impl Iterator<Item = &Hunk> {
        self.segs.iter().filter_map(|s| match s {
            Seg::Conflict(h) => Some(h),
            Seg::Clean(_) => None,
        })
    }
    /// The merged text, if the walk found no conflict.
    pub fn text(&self) -> Option<String> {
        if !self.is_clean() {
            return None;
        }
        let mut out = String::new();
        for s in &self.segs {
            if let Seg::Clean(t) = s {
                out.push_str(t);
            }
        }
        Some(out)
    }
    fn push_clean(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        let prev_open = self.segs.iter().rev().find_map(|s| match s {
            Seg::Clean(s) if !s.is_empty() => Some(!s.ends_with('\n')),
            Seg::Clean(_) => None,
            Seg::Conflict(_) => Some(false),
        });
        if prev_open == Some(true) {
            self.glued = true;
        }
        if let Some(Seg::Clean(s)) = self.segs.last_mut() {
            s.push_str(t);
        } else {
            self.segs.push(Seg::Clean(t.to_string()));
        }
    }
    fn note(&mut self, d: Deviation) {
        if !self.deviations.contains(&d) {
            self.deviations.push(d);
        }
    }
    /// The whole file as one conflict.
    pub fn whole(base: &str, ours: &str, theirs: &str) -> Walk {
        let n = |s: &str| pieces(s).count();
        Walk {
            segs: vec![Seg::Conflict(Hunk {
                at: LineOverlap {
                    base: 0..n(base),
                    ours: 0..n(ours),
                    theirs: 0..n(theirs),
                },
                base: base.to_string(),
                ours: ours.to_string(),
                theirs: theirs.to_string(),
            })],
            ..Walk::default()
        }
    }
}

/// Lines keeping their `\n`; the last may lack it. Upstream's
/// `split(/([^\n]*\n)/).filter(f => f != "")`.
pub(crate) fn pieces(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n')
}

/// A line without its line end, for comparing lines that may end a file.
fn content(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

/// Two inserted lines that are the same line: equal, or the same but for the
/// line end that one of them, the last line of its file, lacks.
fn same_line(a: &str, b: &str) -> bool {
    a == b || ((!a.ends_with('\n') || !b.ends_with('\n')) && content(a) == content(b))
}

/// Where `short` sits in `long` as a whole prefix or suffix, by [`same_line`].
fn held(long: &[&str], short: &[&str], prefer_end: bool) -> Option<usize> {
    if short.len() > long.len() {
        return None;
    }
    let at = |x: usize| long[x..x + short.len()].iter().zip(short).all(|(a, b)| same_line(a, b));
    let (start, end) = (
        at(0).then_some(0),
        at(long.len() - short.len()).then(|| long.len() - short.len()),
    );
    if prefer_end {
        end.or(start)
    } else {
        start.or(end)
    }
}

/// A line with nothing but white space on it.
fn blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// Whether `long` has every line of `short` that is not blank (at least one),
/// as often, in any order, by content.
fn among(long: &[&str], short: &[&str]) -> bool {
    let mut n: HashMap<&str, isize> = HashMap::new();
    for l in long.iter().filter(|l| !blank(l)) {
        *n.entry(content(l)).or_insert(0) += 1;
    }
    let mut any = false;
    for l in short.iter().filter(|l| !blank(l)) {
        any = true;
        let k = n.entry(content(l)).or_insert(0);
        *k -= 1;
        if *k < 0 {
            return false;
        }
    }
    any
}

/// What two runs inserted at one point become.
enum Runs {
    /// One held the other: the longer, the shared lines once.
    Once(Vec<Src>),
    /// Both whole, the first first.
    Whole(Vec<Src>),
    /// One holds the other's lines with lines between them: open.
    Open,
}

// ---------- the merge ----------

/// One change of a side against the base: base lines `base` became the side's
/// lines `side`. An insertion has an empty `base`, a deletion an empty `side`.
#[derive(Clone, Debug)]
struct Change {
    base: Range<usize>,
    side: Range<usize>,
}

impl Change {
    fn inserts(&self) -> bool {
        self.base.is_empty()
    }
}

struct SideDiff<'a> {
    lines: Vec<&'a str>,
    /// In base order, with at least one unchanged base line between two.
    changes: Vec<Change>,
    /// The side's line of each base line it left unchanged.
    equal_at: Vec<Option<usize>>,
    /// The base line of each of the side's lines that is one.
    base_of: Vec<Option<usize>>,
    /// The diff ends in a deletion (upstream does not carry it).
    trailing: bool,
}

fn side_diff<'a>(diffs: &[(Op, String)], base_lines: usize, side: &'a str) -> SideDiff<'a> {
    let lines: Vec<&str> = pieces(side).collect();
    let mut d = SideDiff {
        base_of: vec![None; lines.len()],
        lines,
        changes: Vec::new(),
        equal_at: vec![None; base_lines],
        trailing: matches!(diffs.last(), Some((Op::Delete, _))),
    };
    let (mut b, mut x) = (0usize, 0usize);
    let mut open: Option<(usize, usize)> = None;
    for (op, text) in diffs {
        let n = pieces(text).count();
        match op {
            Op::Equal => {
                if let Some((sb, sx)) = open.take() {
                    d.changes.push(Change {
                        base: sb..b,
                        side: sx..x,
                    });
                }
                for k in 0..n {
                    if let Some(e) = d.equal_at.get_mut(b + k) {
                        *e = Some(x + k);
                    }
                    if let Some(e) = d.base_of.get_mut(x + k) {
                        *e = Some(b + k);
                    }
                }
                b += n;
                x += n;
            }
            Op::Delete => {
                open.get_or_insert((b, x));
                b += n;
            }
            Op::Insert => {
                open.get_or_insert((b, x));
                x += n;
            }
        }
    }
    if let Some((sb, sx)) = open {
        d.changes.push(Change {
            base: sb..b,
            side: sx..x,
        });
    }
    d
}

/// A base whose last line has no newline, and a side that has that line with
/// one: adding the newline is not an edit of the line (it is how a line gets
/// appended after it), so the side counts as keeping it.
fn keep_unterminated_last(base: &[&str], d: &mut SideDiff) {
    let nb = base.len();
    let Some(last) = base.last().filter(|l| !l.ends_with('\n')) else {
        return;
    };
    let Some(ci) = d.changes.iter().position(|c| c.base.end == nb && !c.base.is_empty()) else {
        return;
    };
    let c = d.changes[ci].clone();
    let Some(x) = c
        .side
        .clone()
        .find(|&x| d.lines[x].ends_with('\n') && content(d.lines[x]) == content(last))
    else {
        return;
    };
    let mut parts = Vec::new();
    if c.base.start < nb - 1 || c.side.start < x {
        parts.push(Change {
            base: c.base.start..nb - 1,
            side: c.side.start..x,
        });
    }
    if x + 1 < c.side.end {
        parts.push(Change {
            base: nb..nb,
            side: x + 1..c.side.end,
        });
    }
    d.changes.splice(ci..=ci, parts);
    d.equal_at[nb - 1] = Some(x);
    d.base_of[x] = Some(nb - 1);
}

/// Whether two changes of different sides touch the same base lines: two
/// replacements of overlapping ranges, an insertion strictly inside a
/// replaced range, or two insertions at one point.
fn interferes(a: &Change, b: &Change) -> bool {
    match (a.inserts(), b.inserts()) {
        (false, false) => a.base.start < b.base.end && b.base.start < a.base.end,
        (true, false) => b.base.start < a.base.start && a.base.start < b.base.end,
        (false, true) => a.base.start < b.base.start && b.base.start < a.base.end,
        (true, true) => a.base.start == b.base.start,
    }
}

/// Split changes where they hold the other side's change of the lines at one
/// of their ends: a change of base lines `a..b` into `S`, against the other
/// side's change of `a..d` (or `c..b`) into `T`, where `S` begins (or ends)
/// with exactly `T`, becomes `a..d` into `T` and `d..b` into the rest. The
/// first part is then the same change on both sides, taken once; the rest
/// goes on as a change of its own. Both ticked the second task, one also the
/// first: one change each of the second, and one side's of the first.
fn peel(base: &[&str], sides: &mut [SideDiff; 2], budget: &mut Budget) -> Result<(), OutOfBudget> {
    loop {
        let mut any = false;
        for s in 0..2 {
            let (mine, other) = if s == 0 {
                let (a, b) = sides.split_at_mut(1);
                (&mut a[0], &b[0])
            } else {
                let (a, b) = sides.split_at_mut(1);
                (&mut b[0], &a[0])
            };
            let mut out = Vec::with_capacity(mine.changes.len());
            for c in std::mem::take(&mut mine.changes) {
                let lo = other.changes.partition_point(|d| d.base.end < c.base.start);
                let hi = lo + other.changes[lo..].partition_point(|d| d.base.start <= c.base.end);
                budget.spend(hi - lo + c.side.len())?;
                let hits: Vec<&Change> = other.changes[lo..hi].iter().filter(|d| interferes(&c, d)).collect();
                let mut rest = c;
                // At the start: the first change of the other side there.
                if let Some(d) = hits.first() {
                    if let Some(x) = held_at(base, &mine.lines, &rest, &other.lines, d, false) {
                        out.push(Change {
                            base: rest.base.start..d.base.end,
                            side: rest.side.start..x,
                        });
                        rest = Change {
                            base: d.base.end..rest.base.end,
                            side: x..rest.side.end,
                        };
                        any = true;
                    }
                }
                // At the end: the last.
                let mut tail = None;
                if let Some(d) = hits.last() {
                    if let Some(x) = held_at(base, &mine.lines, &rest, &other.lines, d, true) {
                        tail = Some(Change {
                            base: d.base.start..rest.base.end,
                            side: x..rest.side.end,
                        });
                        rest = Change {
                            base: rest.base.start..d.base.start,
                            side: rest.side.start..x,
                        };
                        any = true;
                    }
                }
                if !rest.base.is_empty() || !rest.side.is_empty() {
                    out.push(rest);
                }
                out.extend(tail);
            }
            mine.changes = out;
        }
        if !any {
            return Ok(());
        }
    }
}

/// Where `c` (of lines `cl`) can be split so that its part at its start (or
/// end) is exactly `d` (of the other side's lines `dl`): `d` replaces lines at
/// that end of `c`'s, and `c`'s lines begin (or end) with `d`'s (at least
/// one), with something left over. The split's line in `c`'s side.
///
/// Not where the rest holds a base line `d` replaced (by its content): that
/// is one side keeping (or editing) a line the other removed, which stays
/// open.
fn held_at(base: &[&str], cl: &[&str], c: &Change, dl: &[&str], d: &Change, end: bool) -> Option<usize> {
    let nested = d.base.start >= c.base.start && d.base.end <= c.base.end;
    let at_end = if end {
        d.base.end == c.base.end
    } else {
        d.base.start == c.base.start
    };
    // A deletion holds nothing: against a change of its lines it stays open.
    if !nested || !at_end || d.inserts() || d.side.is_empty() || d.side.len() > c.side.len() {
        return None;
    }
    if c.base.len() == d.base.len() && c.side.len() == d.side.len() {
        return None;
    }
    let x = if end { c.side.end - d.side.len() } else { c.side.start };
    let same = cl[x..x + d.side.len()]
        .iter()
        .zip(&dl[d.side.clone()])
        .all(|(a, b)| same_line(a, b));
    let rest = if end {
        c.side.start..x
    } else {
        x + d.side.len()..c.side.end
    };
    let replaced: HashSet<&str> = base[d.base.clone()].iter().map(|l| content(l)).collect();
    let keeps_replaced = cl[rest].iter().any(|l| replaced.contains(content(l)));
    (same && !keeps_replaced).then_some(if end { x } else { x + d.side.len() })
}

/// Union-find over the changes of both sides (ours first).
struct Groups(Vec<usize>);

impl Groups {
    fn find(&mut self, mut i: usize) -> usize {
        while self.0[i] != i {
            self.0[i] = self.0[self.0[i]];
            i = self.0[i];
        }
        i
    }
    fn join(&mut self, a: usize, b: usize) {
        let (x, y) = (self.find(a), self.find(b));
        self.0[x] = y;
    }
    /// The groups, each as (side, change index), in order of first change.
    fn list(&mut self, no: usize) -> Vec<Vec<(usize, usize)>> {
        let mut groups: Vec<Vec<(usize, usize)>> = Vec::new();
        let mut slot = std::collections::HashMap::new();
        for id in 0..self.0.len() {
            let root = self.find(id);
            let g = *slot.entry(root).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[g].push(if id < no { (0, id) } else { (1, id - no) });
        }
        groups
    }
}

/// What the merge did that upstream's walk does differently; reported only
/// when the results differ.
#[derive(Default)]
struct Events {
    trailing: bool,
    /// Two runs at one point taken once.
    once: bool,
    /// Two runs at one point placed whole.
    runs: bool,
    /// An insertion taken once into the other side's change beside it.
    absorbed: bool,
    /// Pieces of both sides placed by position at a seam.
    placed: bool,
    /// Two runs at one point that went in whole: (side, lines) of each.
    whole: Vec<[(usize, Range<usize>); 2]>,
}

/// What a group of changes becomes.
enum Piece {
    Clean(Vec<Src>),
    Conflict(Hunk),
}

/// One placed piece of the result: base lines `at` replaced by `piece`.
struct Unit {
    at: Range<usize>,
    piece: Piece,
    /// Made by this side alone (one change, or one insertion).
    alone: Option<usize>,
    /// One of its changes, to join its group with another.
    member: usize,
}

/// A group that makes the whole file one conflict.
struct WholeFile {
    glued: bool,
}

struct Merger<'a> {
    base: Vec<&'a str>,
    sides: [SideDiff<'a>; 2],
    ours_first: bool,
    /// For each side, the base lines its diff deleted that are not blank, by
    /// content.
    deleted: [HashSet<&'a str>; 2],
}

/// The side lines of one side's piece.
fn side_range(v: &[Src]) -> Range<usize> {
    let at = |l: &Src| match *l {
        Src::Ours(i) | Src::Theirs(i) => i,
        _ => unreachable!("one side's lines"),
    };
    match (v.first(), v.last()) {
        (Some(f), Some(l)) => at(f)..at(l) + 1,
        _ => 0..0,
    }
}

fn tag(s: usize, i: usize) -> Src {
    if s == 0 {
        Src::Ours(i)
    } else {
        Src::Theirs(i)
    }
}

/// Line `i` of side `s` and line `k` of the other side, taken once.
fn both(s: usize, i: usize, k: usize) -> Src {
    if s == 0 {
        Src::Both(i, k)
    } else {
        Src::Both(k, i)
    }
}

impl<'a> Merger<'a> {
    fn text(&self, s: Src) -> &'a str {
        match s {
            Src::Base(i) => self.base[i],
            Src::Ours(j) => self.sides[0].lines[j],
            Src::Theirs(k) => self.sides[1].lines[k],
            // The one with its line end, where one lacks it.
            Src::Both(j, k) if !self.sides[0].lines[j].ends_with('\n') => self.sides[1].lines[k],
            Src::Both(j, _) => self.sides[0].lines[j],
        }
    }
    fn id(&self, s: usize, i: usize) -> usize {
        if s == 0 {
            i
        } else {
            self.sides[0].changes.len() + i
        }
    }
    fn is_trailing(&self, s: usize, c: &Change) -> bool {
        self.sides[s].trailing && c.base.end == self.base.len() && !c.base.is_empty()
    }

    /// The piece one group of changes makes.
    fn unit(&self, g: &[(usize, usize)], ev: &mut Events) -> Result<Unit, WholeFile> {
        let changes: Vec<(usize, &Change)> = g.iter().map(|&(s, i)| (s, &self.sides[s].changes[i])).collect();
        let member = self.id(g[0].0, g[0].1);
        let start = changes.iter().map(|(_, c)| c.base.start).min().unwrap();
        let end = changes.iter().map(|(_, c)| c.base.end).max().unwrap();
        if let [(s, c)] = changes[..] {
            ev.trailing |= self.is_trailing(s, c);
            return Ok(Unit {
                at: c.base.clone(),
                piece: Piece::Clean(c.side.clone().map(|j| tag(s, j)).collect()),
                alone: Some(s),
                member,
            });
        }
        if changes.len() == 2 && changes.iter().all(|(_, c)| c.inserts()) {
            let run = |s: usize| changes.iter().find(|(x, _)| *x == s).unwrap().1.side.clone();
            let (fs, ss) = if self.ours_first { (0, 1) } else { (1, 0) };
            let (out, once) = match self.runs(fs, run(fs), ss, run(ss), start) {
                Runs::Once(out) => (out, true),
                Runs::Whole(out) => {
                    ev.whole.push([(fs, run(fs)), (ss, run(ss))]);
                    (out, false)
                }
                Runs::Open => {
                    let text = |s: usize| self.sides[s].lines[run(s)].concat();
                    return Ok(Unit {
                        at: start..start,
                        piece: Piece::Conflict(Hunk {
                            at: LineOverlap {
                                base: start..start,
                                ours: run(0),
                                theirs: run(1),
                            },
                            base: String::new(),
                            ours: text(0),
                            theirs: text(1),
                        }),
                        alone: None,
                        member,
                    });
                }
            };
            // A run line without its newline (the end of a file) not last.
            if out.iter().rev().skip(1).any(|l| !self.text(*l).ends_with('\n')) {
                return Err(WholeFile { glued: true });
            }
            let texts = |s: usize| -> Vec<&str> { self.sides[s].lines[run(s)].to_vec() };
            let mine: Vec<&str> = out.iter().map(|l| self.text(*l)).collect();
            if mine != zip(&texts(fs), &texts(ss)) {
                if once {
                    ev.once = true;
                } else {
                    ev.runs = true;
                }
            }
            return Ok(Unit {
                at: start..start,
                piece: Piece::Clean(out),
                alone: None,
                member,
            });
        }
        // The region both sides changed, and what each made of it.
        let range = |s: usize| -> Option<Range<usize>> {
            let mine: Vec<&Change> = changes.iter().filter(|(x, _)| *x == s).map(|(_, c)| *c).collect();
            match (mine.first(), mine.last()) {
                (Some(f), Some(l)) => Some(f.side.start - (f.base.start - start)..l.side.end + (end - l.base.end)),
                _ => self.sides[s]
                    .equal_at
                    .get(start)
                    .copied()
                    .flatten()
                    .map(|x| x..x + (end - start)),
            }
        };
        let (Some(ro), Some(rt)) = (range(0), range(1)) else {
            return Err(WholeFile { glued: false });
        };
        let text = |s: usize, r: &Range<usize>| self.sides[s].lines[r.clone()].concat();
        let (vo, vt) = (text(0, &ro), text(1, &rt));
        let piece = if vo == vt {
            // The same text on both sides: once, a base line both kept as one.
            let (bo, bt) = (&self.sides[0].base_of, &self.sides[1].base_of);
            Piece::Clean(
                ro.clone()
                    .zip(rt.clone())
                    .map(|(j, k)| match (bo[j], bt[k]) {
                        (Some(x), Some(y)) if x == y && self.base[x].ends_with('\n') => Src::Base(x),
                        _ => Src::Both(j, k),
                    })
                    .collect(),
            )
        } else if changes.iter().all(|(_, c)| c.side.is_empty()) {
            // Deletions only: every line either side deleted goes.
            Piece::Clean(
                (start..end)
                    .filter(|p| !changes.iter().any(|(_, c)| c.base.contains(p)))
                    .map(|p| match (self.sides[0].equal_at[p], self.sides[1].equal_at[p]) {
                        (Some(j), Some(k)) if !self.base[p].ends_with('\n') => Src::Both(j, k),
                        _ => Src::Base(p),
                    })
                    .collect(),
            )
        } else {
            Piece::Conflict(Hunk {
                at: LineOverlap {
                    base: start..end,
                    ours: ro,
                    theirs: rt,
                },
                base: self.base[start..end].concat(),
                ours: vo,
                theirs: vt,
            })
        };
        if let Piece::Clean(_) = piece {
            ev.trailing |= changes.iter().any(|(s, c)| self.is_trailing(*s, c));
        }
        Ok(Unit {
            at: start..end,
            piece,
            alone: None,
            member,
        })
    }

    /// Two runs inserted at one point, side `fs`'s first. The same run, or one
    /// holding the other as a whole prefix or suffix, is taken once (the
    /// longer); otherwise both whole, the first first, two separate blocks
    /// (two fenced code blocks, each whole). But both whole puts the lines
    /// they share in twice, so it is open wherever which lines are the runs is
    /// not sure, or they are likely one run:
    ///
    /// - one has all the other's lines that are not blank (in any order): one
    ///   run with lines put in it or moved (a writer that saw the other side's
    ///   run and added to it);
    /// - a run holds a copy of a base line its side deleted: the diff may have
    ///   kept the wrong copy (a blank line instead of the line);
    /// - a run holds a copy of the base line before or after the point (the
    ///   nearest that is not blank: blank lines align any way): the diff could
    ///   as well have kept that copy and split the run around it;
    /// - they share a line (not blank) at a point beside a blank base line:
    ///   the diffs could have cut either side's lines at another blank line.
    fn runs(&self, fs: usize, first: Range<usize>, ss: usize, second: Range<usize>, at: usize) -> Runs {
        let lines = |s: usize, r: &Range<usize>| &self.sides[s].lines[r.clone()];
        let (a, b) = (lines(fs, &first), lines(ss, &second));
        let base = &self.base;
        let beside: Vec<&str> = [
            base[..at].iter().rev().find(|l| !blank(l)),
            base[at..].iter().find(|l| !blank(l)),
        ]
        .into_iter()
        .flatten()
        .map(|l| content(l))
        .collect();
        let splits = a.iter().chain(b).any(|l| beside.contains(&content(l)));
        let moved = |s: usize, run: &[&str]| run.iter().any(|l| self.deleted[s].contains(content(l)));
        let loose = [at.checked_sub(1), Some(at)]
            .into_iter()
            .flatten()
            .any(|i| base.get(i).is_some_and(|l| blank(l)));
        let shared = a
            .iter()
            .any(|l| !blank(l) && b.iter().any(|m| content(m) == content(l)));
        let unsure = among(a, b) || among(b, a) || moved(fs, a) || moved(ss, b) || splits || (shared && loose);
        // `long` of side `ls` holds `short` of the other side at offset `at`.
        let once = |ls: usize, long: &Range<usize>, short: &Range<usize>, at: usize| -> Vec<Src> {
            long.clone()
                .enumerate()
                .map(|(x, i)| {
                    if x >= at && x < at + short.len() {
                        both(ls, i, short.start + x - at)
                    } else {
                        tag(ls, i)
                    }
                })
                .collect()
        };
        if let Some(at) = held(b, a, false) {
            Runs::Once(once(ss, &second, &first, at))
        } else if let Some(at) = held(a, b, false) {
            Runs::Once(once(fs, &first, &second, at))
        } else if unsure {
            Runs::Open
        } else {
            Runs::Whole(first.map(|i| tag(fs, i)).chain(second.map(|k| tag(ss, k))).collect())
        }
    }

    /// The lines a unit shows, by content: its merged lines, or both sides'
    /// lines of a conflict.
    fn contents(&self, u: &Unit) -> HashSet<&'a str> {
        match &u.piece {
            Piece::Clean(v) => v.iter().map(|l| content(self.text(*l))).collect(),
            Piece::Conflict(h) => {
                let side = |s: usize, r: &Range<usize>| self.sides[s].lines[r.clone()].iter().map(|l| content(l));
                side(0, &h.at.ours).chain(side(1, &h.at.theirs)).collect()
            }
        }
    }

    /// `ins`, an insertion of one side beside `change` of the other side (at
    /// its end where `at_end`, else at its start), taken once with it: where
    /// one holds the other as a whole prefix or suffix, as two runs at one
    /// point are, the longer's lines with the shorter's marked as shared. The
    /// change's base lines stay replaced.
    fn absorb(&self, change: &[Src], ins: &[Src], at_end: bool) -> Option<Vec<Src>> {
        let text = |v: &[Src]| -> Vec<&str> { v.iter().map(|l| self.text(*l)).collect() };
        let (c, i) = (text(change), text(ins));
        let mark = |long: &[Src], short: &[Src], at: usize| -> Option<Vec<Src>> {
            let mut out = long.to_vec();
            for (x, l) in short.iter().enumerate() {
                out[at + x] = match (out[at + x], *l) {
                    (Src::Ours(j), Src::Theirs(k)) | (Src::Theirs(k), Src::Ours(j)) => Src::Both(j, k),
                    _ => return None,
                };
            }
            Some(out)
        };
        if change.is_empty() {
            return None;
        }
        if let Some(at) = held(&c, &i, at_end) {
            return mark(change, ins, at);
        }
        let at = held(&i, &c, !at_end)?;
        mark(ins, change, at)
    }

    /// Two insertions of the two sides at different points with only base
    /// lines both kept between them: where one side's view of that stretch
    /// (its insertion and the base lines) equals the other's or holds it as a
    /// whole prefix or suffix, the stretch is the longer view, the shared lines
    /// taken once. The same as two runs at one point, for a line the diff
    /// placed on the other side of an equal line (a blank line inserted next
    /// to a blank line). Other lines of the longer view must be inserted ones.
    fn slide(&self, units: &mut Vec<Unit>, ev: &mut Events, b: &mut Budget) -> Result<(), OutOfBudget> {
        let mut x = 1;
        while x < units.len() {
            let (u, v) = (&units[x - 1], &units[x]);
            let pair = match (u.alone, v.alone, &u.piece, &v.piece) {
                (Some(su), Some(sv), Piece::Clean(a), Piece::Clean(c))
                    if su != sv && u.at.is_empty() && v.at.is_empty() && u.at.start < v.at.start =>
                {
                    Some((su, side_range(a), sv, side_range(c)))
                }
                _ => None,
            };
            let Some((sp, jp, sq, iq)) = pair else {
                x += 1;
                continue;
            };
            let (p, q) = (u.at.start, v.at.start);
            b.spend(jp.len() + iq.len() + 2 * (q - p))?;
            // Side `sp` inserted `jp` at p, then kept p..q; side `sq` kept
            // p..q, then inserted `iq` at q.
            let vp = jp.start..jp.end + (q - p);
            let vq = iq.start.checked_sub(q - p).map(|s| s..iq.end);
            let kept = |s: usize, r: &Range<usize>, from: usize| {
                (from..q)
                    .zip(r.clone())
                    .all(|(i, l)| self.sides[s].equal_at[i] == Some(l))
            };
            let Some(vq) =
                vq.filter(|r| kept(sq, r, p) && vp.end <= self.sides[sp].lines.len() && kept(sp, &(jp.end..vp.end), p))
            else {
                x += 1;
                continue;
            };
            let text = |s: usize, r: &Range<usize>| -> Vec<&str> { self.sides[s].lines[r.clone()].to_vec() };
            let (tp, tq) = (text(sp, &vp), text(sq, &vq));
            let found = if tp.len() >= tq.len() {
                held(&tp, &tq, false).map(|at| (sp, vp.clone(), sq, vq.clone(), at))
            } else {
                held(&tq, &tp, false).map(|at| (sq, vq.clone(), sp, vp.clone(), at))
            };
            let Some((ls, long, ss, short, at)) = found else {
                x += 1;
                continue;
            };
            let paired = at..at + short.len();
            let unpaired_inserted = long
                .clone()
                .enumerate()
                .all(|(n, l)| paired.contains(&n) || self.sides[ls].base_of[l].is_none());
            if !unpaired_inserted {
                x += 1;
                continue;
            }
            let lines = long
                .clone()
                .enumerate()
                .map(|(n, l)| {
                    if !paired.contains(&n) {
                        return tag(ls, l);
                    }
                    let k = short.start + n - at;
                    let (bl, bk) = (self.sides[ls].base_of[l], self.sides[ss].base_of[k]);
                    match (bl, bk) {
                        (Some(i), Some(i2)) if i == i2 && self.base[i].ends_with('\n') => Src::Base(i),
                        _ => both(ls, l, k),
                    }
                })
                .collect();
            let member = units[x - 1].member;
            units.splice(
                x - 1..=x,
                [Unit {
                    at: p..q,
                    piece: Piece::Clean(lines),
                    alone: None,
                    member,
                }],
            );
            ev.once = true;
        }
        Ok(())
    }

    /// Settle the seams between units that meet at one base point. Returns
    /// the groups to join (their pieces share lines), or nothing once every
    /// seam is settled, with insertions taken once removed from `units`.
    fn seams(
        &self,
        units: &mut Vec<Unit>,
        ev: &mut Events,
        b: &mut Budget,
    ) -> Result<Vec<(usize, usize)>, OutOfBudget> {
        let mut joins = Vec::new();
        let mut gone = vec![false; units.len()];
        for x in 1..units.len() {
            if units[x - 1].at.end != units[x].at.start || gone[x - 1] {
                continue;
            }
            let (u, v) = (&units[x - 1], &units[x]);
            let size = |u: &Unit| match &u.piece {
                Piece::Clean(l) => l.len(),
                Piece::Conflict(h) => h.at.ours.len() + h.at.theirs.len(),
            };
            b.spend(size(u) + size(v))?;
            // An insertion of one side beside a change of the other alone.
            let pair = match (u.alone, v.alone, &u.piece, &v.piece) {
                (Some(su), Some(sv), Piece::Clean(_), Piece::Clean(_)) if su != sv => {
                    match (u.at.is_empty(), v.at.is_empty()) {
                        (false, true) => Some((x - 1, x, true)),
                        (true, false) => Some((x, x - 1, false)),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some((chg, ins, at_end)) = pair {
                let (Piece::Clean(c), Piece::Clean(i)) = (&units[chg].piece, &units[ins].piece) else {
                    unreachable!()
                };
                if let Some(once) = self.absorb(c, i, at_end) {
                    units[chg].piece = Piece::Clean(once);
                    // Its lines are no longer one side's alone: an insertion
                    // at its other end is not taken into it as well.
                    units[chg].alone = None;
                    gone[ins] = true;
                    ev.absorbed = true;
                    continue;
                }
            }
            let (u, v) = (&units[x - 1], &units[x]);
            let (cu, cv) = (self.contents(u), self.contents(v));
            if cu.is_empty() || cv.is_empty() {
                continue;
            }
            if cu.intersection(&cv).next().is_some() {
                joins.push((u.member, v.member));
            } else if u.alone.is_some() || v.alone.is_some() {
                ev.placed = true;
            }
        }
        if joins.is_empty() {
            let mut keep = gone.iter().map(|g| !g);
            units.retain(|_| keep.next().unwrap());
        }
        Ok(joins)
    }
}

/// The merge of `ours` (upstream's left, the winning revision) and `theirs`
/// (right) against `base`. `ours_first` is upstream's
/// `leftLeaf.mtime <= rightLeaf.mtime`: where both sides insert at one point,
/// that side's lines go first.
///
/// - A change only one side made is taken.
/// - The same change on both sides is taken once.
/// - Changes at different base positions are placed by position (see the
///   module docs). Where a change on one side ends (or starts) at the point
///   the other side inserts lines, the insertion goes after (or before) the
///   whole change.
/// - Two insertions at one point: taken once where they are the same, or one
///   holds the other as a whole prefix or suffix (then the longer); otherwise
///   both whole, the older side's first, or a conflict where both whole would
///   put a shared line in twice and the runs are likely one (see
///   [`Merger::runs`]).
/// - An insertion beside the other side's change (at its start or end) is
///   taken once where the change's lines begin or end with exactly those
///   lines.
/// - Deletions of overlapping lines on both sides: every line either deleted
///   is gone.
/// - Anything else where the two sides' changes touch the same base lines (two
///   different replacements of overlapping ranges, a deletion against a
///   change, an insertion strictly inside the other side's replaced range) is
///   a conflict hunk covering the base lines involved, and so are two pieces
///   of the two sides that meet at one point and share a line otherwise.
/// - A result that would join a line without its newline to the next one is
///   one whole-file conflict, and so is a result whose line counts no right
///   merge can have ([`counts_hold`]).
///
/// Out of budget: the diffs or the merge would do more work than `budget`.
pub(crate) fn walk(
    base: &str,
    ours: &str,
    theirs: &str,
    ours_first: bool,
    budget: &mut Budget,
) -> Result<Walk, OutOfBudget> {
    let base_lines: Vec<&str> = pieces(base).collect();
    let nb = base_lines.len();
    let (dl, dr) = (
        dmp::line_diff(base, ours, budget)?,
        dmp::line_diff(base, theirs, budget)?,
    );
    let mut sides = [side_diff(&dl, nb, ours), side_diff(&dr, nb, theirs)];
    for side in &mut sides {
        keep_unterminated_last(&base_lines, side);
    }
    peel(&base_lines, &mut sides, budget)?;
    budget.spend(2 * nb)?;
    let deleted = [0, 1].map(|s| {
        (0..nb)
            .filter(|&i| sides[s].equal_at[i].is_none() && !blank(base_lines[i]))
            .map(|i| content(base_lines[i]))
            .collect()
    });
    let m = Merger {
        base: base_lines,
        sides,
        ours_first,
        deleted,
    };
    let (no, nt) = (m.sides[0].changes.len(), m.sides[1].changes.len());
    budget.spend(m.sides[0].lines.len() + m.sides[1].lines.len() + nb + no + nt)?;
    let mut groups = Groups((0..no + nt).collect());
    for (i, a) in m.sides[0].changes.iter().enumerate() {
        let theirs = &m.sides[1].changes;
        let mut j = theirs.partition_point(|c| c.base.end < a.base.start);
        while j < theirs.len() && theirs[j].base.start <= a.base.end {
            if interferes(a, &theirs[j]) {
                groups.join(i, no + j);
            }
            j += 1;
        }
    }
    let whole = |glued: bool| -> Result<Walk, OutOfBudget> {
        let mut w = Walk::whole(base, ours, theirs);
        w.glued = glued;
        Ok(w)
    };
    // Pieces in base order, until no seam joins two groups.
    let (units, ev) = loop {
        let mut ev = Events::default();
        let mut units = Vec::new();
        for g in groups.list(no) {
            budget.spend(g.len())?;
            match m.unit(&g, &mut ev) {
                Ok(u) => units.push(u),
                Err(WholeFile { glued }) => {
                    let mut w = whole(glued)?;
                    if glued {
                        let up = upstream_from(dl, dr, ours_first);
                        if up.text.is_some() {
                            w.note(if up.glued {
                                Deviation::GluedLines
                            } else {
                                Deviation::RunOrder
                            });
                        }
                    }
                    return Ok(w);
                }
            }
        }
        // An insertion at a point goes before a change starting there.
        units.sort_by_key(|u| (u.at.start, u.at.end));
        m.slide(&mut units, &mut ev, budget)?;
        let joins = m.seams(&mut units, &mut ev, budget)?;
        if joins.is_empty() {
            break (units, ev);
        }
        for (a, b) in joins {
            groups.join(a, b);
        }
    };

    let mut w = Walk {
        align: [m.sides[0].equal_at.clone(), m.sides[1].equal_at.clone()],
        ..Walk::default()
    };
    let mut out = Out::default();
    // A base line both kept, as its base line; the base's last line without
    // its newline as the sides' lines, so the one with a newline is used.
    let kept = |i: usize| match (
        m.base[i].ends_with('\n'),
        m.sides[0].equal_at[i],
        m.sides[1].equal_at[i],
    ) {
        (false, Some(j), Some(k)) => Src::Both(j, k),
        _ => Src::Base(i),
    };
    let mut p = 0;
    for u in units {
        for i in p..u.at.start {
            out.line(m.text(kept(i)), kept(i));
        }
        match u.piece {
            Piece::Clean(v) => {
                for s in v {
                    out.line(m.text(s), s);
                }
            }
            Piece::Conflict(h) => out.conflict(h),
        }
        p = u.at.end;
    }
    for i in p..m.base.len() {
        out.line(m.text(kept(i)), kept(i));
    }
    let Out {
        mut segs,
        clean,
        trace,
        glued,
        ..
    } = out;
    if !clean.is_empty() {
        segs.push(Seg::Clean(clean));
    }
    w.segs = segs;
    w.glued = glued;
    budget.spend(trace.len())?;
    if w.is_clean() {
        w.trace = trace;
    }

    // Safety net: a line emitted right after a line without its newline would
    // be glued to it. The whole file is then one conflict.
    if w.glued {
        let align = w.align;
        w = whole(true)?;
        w.align = align;
    }
    // Safety net: a clean merge whose line counts no right merge can have
    // (see [`counts_hold`]) is one conflict.
    if w.is_clean() {
        budget.spend(m.base.len() + m.sides[0].lines.len() + m.sides[1].lines.len() + w.trace.len())?;
        let merged: Vec<&str> = w.trace.iter().map(|l| m.text(*l)).collect();
        let texts = [&m.base[..], &m.sides[0].lines[..], &m.sides[1].lines[..], &merged[..]];
        let pairs: Vec<[(usize, Range<usize>); 2]> = ev
            .whole
            .iter()
            .map(|[(a, r), (b, q)]| [(a + 1, r.clone()), (b + 1, q.clone())])
            .collect();
        if !counts_hold(texts, &pairs) {
            let align = w.align;
            w = whole(false)?;
            w.align = align;
        }
    }

    let up = upstream_from(dl, dr, ours_first);
    let mine = w.text();
    if mine != up.text {
        match mine {
            None if up.glued => w.note(Deviation::GluedLines),
            None => {}
            Some(_) => {
                if ev.trailing {
                    w.note(Deviation::TrailingDeletion);
                }
                if ev.once || ev.absorbed {
                    w.note(Deviation::SameInsertionOnce);
                }
                if ev.runs {
                    w.note(Deviation::RunOrder);
                }
                if up.glued {
                    w.note(Deviation::GluedLines);
                }
                if ev.placed || w.deviations.is_empty() {
                    w.note(Deviation::PositionOrder);
                }
            }
        }
    }
    Ok(w)
}

/// Whether a clean merge's line counts are ones a right merge can have.
/// `texts` are the base `B`, ours `O`, theirs `T` and the merge `M`, as
/// lines; `whole` the runs (by text and lines) that went in whole in pairs.
///
/// This knows nothing of how the merge was made: not how each side's diff
/// aligned its lines with the base, which is where repeated lines go wrong
/// (a line taken twice because the two diffs put it at different points, a
/// deleted line come back because a diff kept another copy of it). It counts
/// how often each line that is not blank is in each text, by content, and
/// asks what a right merge can have. A right merge takes each side's added
/// lines and drops each side's deleted ones; it cannot tell whether a line
/// both sides added (or deleted) is one line or two, so where that decides
/// the count, the merge stays open:
///
/// - `M <= max(O, T)`: more than either side has means both sides added the
///   line, each its own copy. Two runs at one point that went in whole are
///   the one exception, by design: their shared lines are there twice.
/// - Where neither side has the line more often than the base, `M <=
///   min(O, T)`: more is a deleted line come back (one side moved it, the
///   other deleted it: open).
/// - `M >= min(O, T)`: fewer means each side deleted another copy. Counted
///   over the whole file, two copies far apart (each side ticking another of
///   two tasks with the same text) would look like that. So for this one the
///   texts are cut at anchors, lines that are not blank and are in each of
///   the four texts exactly once, in the same order (as patience diff's), and
///   each stretch between anchors is counted on its own; the rule above is
///   checked per stretch too.
fn counts_hold(texts: [&[&str]; 4], whole: &[[(usize, Range<usize>); 2]]) -> bool {
    let c: [Vec<&str>; 4] = texts.map(|t| t.iter().map(|l| content(l)).collect());
    let mut total: HashMap<&str, [usize; 4]> = HashMap::new();
    for (k, t) in c.iter().enumerate() {
        for l in t.iter().filter(|l| !blank(l)) {
            total.entry(l).or_insert([0; 4])[k] += 1;
        }
    }
    let mut twice: HashMap<&str, usize> = HashMap::new();
    for [(a, r), (b, q)] in whole {
        let tally = |k: usize, r: &Range<usize>| {
            let mut h: HashMap<&str, usize> = HashMap::new();
            for l in c[k][r.clone()].iter().filter(|l| !blank(l)) {
                *h.entry(l).or_insert(0) += 1;
            }
            h
        };
        let (x, y) = (tally(*a, r), tally(*b, q));
        for (l, m) in x {
            *twice.entry(l).or_insert(0) += m.min(y.get(l).copied().unwrap_or(0));
        }
    }
    let come_back = |[nb, no, nt, nm]: [usize; 4]| no <= nb && nt <= nb && nm > no.min(nt);
    let whole_file = total.iter().all(|(l, &n)| {
        let [_, no, nt, nm] = n;
        nm <= no.max(nt) + twice.get(l).copied().unwrap_or(0) && !come_back(n)
    });
    if !whole_file {
        return false;
    }
    // Where each line that is once in all four texts is in each.
    let mut pos: HashMap<&str, [usize; 4]> = HashMap::new();
    for (k, t) in c.iter().enumerate() {
        for (i, l) in t.iter().enumerate() {
            if total.get(l) == Some(&[1; 4]) {
                pos.entry(l).or_insert([0; 4])[k] = i;
            }
        }
    }
    // In base order, an anchor where it comes after the last in every text.
    let mut anchors: Vec<[usize; 4]> = Vec::new();
    for l in &c[0] {
        if let Some(p) = pos.get(l) {
            if anchors.last().is_none_or(|a| (0..4).all(|k| p[k] > a[k])) {
                anchors.push(*p);
            }
        }
    }
    // Each text's lines by stretch: (stretch, content) -> count per text.
    let mut n: HashMap<(usize, &str), [usize; 4]> = HashMap::new();
    for (k, t) in c.iter().enumerate() {
        let mut at = 0;
        for (i, l) in t.iter().enumerate() {
            while at < anchors.len() && anchors[at][k] < i {
                at += 1;
            }
            if blank(l) || anchors.get(at).is_some_and(|a| a[k] == i) {
                continue;
            }
            n.entry((at, l)).or_insert([0; 4])[k] += 1;
        }
    }
    n.values().all(|&x| {
        let [_, no, nt, nm] = x;
        nm >= no.min(nt) && !come_back(x)
    })
}

/// The walk's result as it is put together.
#[derive(Default)]
struct Out {
    segs: Vec<Seg>,
    clean: String,
    trace: Vec<Src>,
    /// The last line put has no newline.
    open: bool,
    glued: bool,
}

impl Out {
    fn line(&mut self, text: &str, s: Src) {
        self.glued |= self.open;
        self.open = !text.ends_with('\n');
        self.clean.push_str(text);
        self.trace.push(s);
    }
    fn conflict(&mut self, h: Hunk) {
        if !self.clean.is_empty() {
            self.segs.push(Seg::Clean(std::mem::take(&mut self.clean)));
        }
        self.segs.push(Seg::Conflict(h));
        // A marker follows on a line of its own.
        self.open = false;
    }
}

/// Upstream's pairing of two runs at one point: line by line, equal lines
/// once, otherwise the older side's line first, then the longer run's tail.
fn zip<'a>(first: &[&'a str], second: &[&'a str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    for k in 0..first.len().max(second.len()) {
        match (first.get(k), second.get(k)) {
            (Some(a), Some(b)) if a == b => out.push(*a),
            (Some(a), Some(b)) => {
                out.push(*a);
                out.push(*b);
            }
            (Some(a), None) => out.push(*a),
            (None, Some(b)) => out.push(*b),
            (None, None) => {}
        }
    }
    out
}

// ---------- upstream's walk, as a model ----------

/// What upstream's `mergeSensibly` returns for these inputs: the merged text,
/// or `None` where it does not merge, and whether it joined a line without
/// its newline to the next.
pub(crate) struct Upstream {
    pub text: Option<String>,
    pub glued: bool,
}

/// `splitDiffPiece`: one item per line, a deletion's lines paired with the
/// insertion that follows it, and a deletion that is the last diff turned into
/// EQUAL lines.
fn split_diff(src: Vec<(Op, String)>) -> Vec<(Op, String)> {
    let mut ret = Vec::new();
    let mut src = src.into_iter();
    let each = |op: Op, t: &str| pieces(t).map(move |p| (op, p.to_string())).collect::<Vec<_>>();
    while let Some((op, text)) = src.next() {
        if op != Op::Delete {
            ret.extend(each(op, &text));
            continue;
        }
        match src.next() {
            Some((Op::Insert, ntext)) => {
                let mut pair = each(Op::Insert, &ntext).into_iter();
                for d in each(Op::Delete, &text) {
                    ret.push(d);
                    ret.extend(pair.next());
                }
                ret.extend(pair);
            }
            Some((nop, ntext)) => {
                ret.extend(each(Op::Delete, &text));
                ret.extend(each(nop, &ntext));
            }
            None => ret.extend(each(Op::Equal, &text)),
        }
    }
    ret
}

/// A port of upstream's walk (commonlib 0.2.1 `ConflictManager.mergeSensibly`),
/// line for line, including its "weird condition" exit that returns what it
/// merged so far. The corpus test checks it against upstream's own output.
pub(crate) fn upstream(base: &str, ours: &str, theirs: &str, ours_first: bool) -> Upstream {
    let mut b = Budget::unlimited();
    let diff = |side: &str, b: &mut Budget| dmp::line_diff(base, side, b).expect("unlimited");
    upstream_from(diff(ours, &mut b), diff(theirs, &mut b), ours_first)
}

/// [`upstream`] within `budget`.
pub(crate) fn upstream_within(
    base: &str,
    ours: &str,
    theirs: &str,
    ours_first: bool,
    budget: &mut Budget,
) -> Result<Upstream, OutOfBudget> {
    let dl = dmp::line_diff(base, ours, budget)?;
    let dr = dmp::line_diff(base, theirs, budget)?;
    Ok(upstream_from(dl, dr, ours_first))
}

/// [`upstream`] from the two sides' line diffs against the base.
fn upstream_from(dl: Vec<(Op, String)>, dr: Vec<(Op, String)>, ours_first: bool) -> Upstream {
    let l = split_diff(dl);
    let r = split_diff(dr);
    let none = (Op::Equal, String::new());
    let mut merged: Vec<&(Op, String)> = Vec::new();
    let (mut li, mut ri) = (0usize, 0usize);
    let mut auto = true;
    loop {
        if li >= l.len() && ri >= r.len() {
            break;
        }
        let left = l.get(li).unwrap_or(&none);
        let right = r.get(ri).unwrap_or(&none);
        li += 1;
        ri += 1;
        if left.0 == Op::Equal && right.0 == Op::Equal && left.1 == right.1 {
            merged.push(left);
            continue;
        }
        if left.0 == Op::Delete && right.0 == Op::Delete && left.1 == right.1 {
            let nl = l.get(li).unwrap_or(&none);
            let nr = r.get(ri).unwrap_or(&none);
            let (il, ir) = (nl.0 == Op::Insert, nr.0 == Op::Insert);
            if il != ir || (il && ir && nl.1 != nr.1) {
                auto = false;
                break;
            }
            merged.push(left);
            continue;
        }
        if left.0 == Op::Insert && right.0 == Op::Insert {
            if left.1 == right.1 {
                merged.push(left);
            } else if ours_first {
                merged.push(left);
                merged.push(right);
            } else {
                merged.push(right);
                merged.push(left);
            }
            continue;
        }
        if left.0 == Op::Insert {
            ri -= 1;
            merged.push(left);
            continue;
        }
        if right.0 == Op::Insert {
            li -= 1;
            merged.push(right);
            continue;
        }
        if right.1 != left.1 {
            auto = false;
            break;
        }
        if left.0 == Op::Delete {
            if right.0 == Op::Equal {
                merged.push(left);
                continue;
            }
            auto = false;
            break;
        }
        if right.0 == Op::Delete {
            if left.0 == Op::Equal {
                merged.push(right);
                continue;
            }
            auto = false;
            break;
        }
        // "Weird condition": out of the loop, the merge so far marked good.
        break;
    }
    if !auto {
        return Upstream {
            text: None,
            glued: false,
        };
    }
    let mut text = String::new();
    let mut glued = false;
    for (op, t) in merged {
        if *op == Op::Delete {
            continue;
        }
        if !text.is_empty() && !text.ends_with('\n') {
            glued = true;
        }
        text.push_str(t);
    }
    Upstream {
        text: Some(text),
        glued,
    }
}

/// Two-way: no base. Equal lines once, every difference a conflict hunk;
/// past the budget, the whole file as one.
pub(crate) fn walk_two_way(ours: &str, theirs: &str, budget: &mut Budget) -> Walk {
    let mut w = Walk::default();
    let Ok(diffs) = dmp::line_diff(ours, theirs, budget) else {
        return Walk::whole("", ours, theirs);
    };
    let (mut oi, mut ti) = (0usize, 0usize);
    let (mut o, mut t) = (String::new(), String::new());
    let (mut os, mut ts) = (0usize, 0usize);
    let flush = |w: &mut Walk, o: &mut String, t: &mut String, os, oi, ts, ti| {
        if !o.is_empty() || !t.is_empty() {
            w.segs.push(Seg::Conflict(Hunk {
                at: LineOverlap {
                    base: 0..0,
                    ours: os..oi,
                    theirs: ts..ti,
                },
                base: String::new(),
                ours: std::mem::take(o),
                theirs: std::mem::take(t),
            }));
        }
    };
    for (op, text) in diffs {
        let n = pieces(&text).count();
        match op {
            Op::Delete => {
                o.push_str(&text);
                oi += n;
            }
            Op::Insert => {
                t.push_str(&text);
                ti += n;
            }
            Op::Equal => {
                flush(&mut w, &mut o, &mut t, os, oi, ts, ti);
                w.push_clean(&text);
                oi += n;
                ti += n;
                os = oi;
                ts = ti;
            }
        }
    }
    flush(&mut w, &mut o, &mut t, os, oi, ts, ti);
    w
}

/// Labels for the conflict markers of [`crate::merge3`].
#[derive(Clone, Copy, Debug)]
pub struct Labels<'a> {
    pub ours: &'a str,
    pub base: &'a str,
    pub theirs: &'a str,
}

impl Default for Labels<'_> {
    fn default() -> Self {
        Labels {
            ours: "ours",
            base: "base",
            theirs: "theirs",
        }
    }
}

/// Render a walk with diff3-style markers around each conflict.
pub(crate) fn render(w: &Walk, labels: &Labels, eol: &str) -> String {
    let mut out = String::new();
    render_into(&mut out, w, labels, eol);
    out
}

/// [`render`], appending to `out`.
pub(crate) fn render_into(out: &mut String, w: &Walk, labels: &Labels, eol: &str) {
    let end_line = |out: &mut String| {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
    };
    for s in &w.segs {
        match s {
            Seg::Clean(t) => out.push_str(t),
            Seg::Conflict(h) => {
                end_line(out);
                out.push_str(&format!("<<<<<<< {}{eol}", labels.ours));
                out.push_str(&h.ours);
                end_line(out);
                out.push_str(&format!("||||||| {}{eol}", labels.base));
                out.push_str(&h.base);
                end_line(out);
                out.push_str(&format!("======={eol}"));
                out.push_str(&h.theirs);
                end_line(out);
                out.push_str(&format!(">>>>>>> {}{eol}", labels.theirs));
            }
        }
    }
}

pub(crate) fn eol_of(texts: &[&str]) -> &'static str {
    for t in texts {
        if let Some(i) = t.find('\n') {
            return if i > 0 && t.as_bytes()[i - 1] == b'\r' {
                "\r\n"
            } else {
                "\n"
            };
        }
    }
    "\n"
}

#[cfg(test)]
pub(crate) fn split_for_test(a: &str, b: &str) -> Vec<(i8, String)> {
    split_diff(dmp::line_diff(a, b, &mut Budget::unlimited()).unwrap())
        .into_iter()
        .map(|(op, t)| (op as i8, t))
        .collect()
}
