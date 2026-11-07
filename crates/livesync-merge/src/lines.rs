//! The three-way line merge, and the same merge producing conflict hunks for
//! `merge3`.
//!
//! Each side is diffed against the base with diff-match-patch's line mode, as
//! upstream (commonlib's `ConflictManager.mergeSensibly`) does, and each diff
//! becomes a list of changes: base lines `a..b` became these side lines. The
//! merge places every change by its base position, so a line inserted between
//! base lines `i` and `i + 1` comes after everything the other side made of
//! lines up to `i` and before everything it made of lines from `i + 1`. Changes
//! of the two sides that touch the same base lines are merged only when that
//! is unambiguous (see [`walk`]); everything else is a conflict hunk.
//!
//! [`upstream`] is a faithful port of upstream's own walk. It decides nothing
//! here: the merge's result is compared with it to report how it differs.

use crate::dmp::{self, Op};
use crate::{Deviation, LineOverlap};
use std::ops::Range;

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
    /// The diff ends in a deletion (upstream does not carry it).
    trailing: bool,
}

fn side_diff<'a>(diffs: &[(Op, String)], base_lines: usize, side: &'a str) -> SideDiff<'a> {
    let mut d = SideDiff {
        lines: pieces(side).collect(),
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

fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// What the merge did that upstream's walk does differently; reported only
/// when the results differ.
#[derive(Default)]
struct Events {
    trailing: bool,
    once: bool,
    runs: bool,
}

/// One placed piece of the result: base lines `at` replaced by `seg`.
struct Unit {
    at: Range<usize>,
    seg: Seg,
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
///   both whole, the older side's first.
/// - Deletions of overlapping lines on both sides: every line either deleted
///   is gone.
/// - Anything else where the two sides' changes touch the same base lines (two
///   different replacements of overlapping ranges, a deletion against a
///   change, an insertion strictly inside the other side's replaced range) is
///   a conflict hunk covering the base lines involved.
/// - A result that would join a line without its newline to the next one is
///   one whole-file conflict.
pub(crate) fn walk(base: &str, ours: &str, theirs: &str, ours_first: bool) -> Walk {
    let base_lines: Vec<&str> = pieces(base).collect();
    let nb = base_lines.len();
    let (dl, dr) = (dmp::line_diff(base, ours), dmp::line_diff(base, theirs));
    let sides = [side_diff(&dl, nb, ours), side_diff(&dr, nb, theirs)];
    let no = sides[0].changes.len();
    let id = |s: usize, i: usize| if s == 0 { i } else { no + i };
    let mut parent: Vec<usize> = (0..no + sides[1].changes.len()).collect();
    for (i, a) in sides[0].changes.iter().enumerate() {
        let theirs = &sides[1].changes;
        let mut j = theirs.partition_point(|c| c.base.end < a.base.start);
        while j < theirs.len() && theirs[j].base.start <= a.base.end {
            if interferes(a, &theirs[j]) {
                let (x, y) = (find(&mut parent, i), find(&mut parent, id(1, j)));
                parent[x] = y;
            }
            j += 1;
        }
    }
    // Groups in order of their first change.
    let mut groups: Vec<Vec<(usize, usize)>> = Vec::new();
    let mut slot: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let mut members: Vec<(usize, usize)> = (0..no).map(|i| (0, i)).collect();
    members.extend((0..sides[1].changes.len()).map(|j| (1, j)));
    for (s, i) in members {
        let root = find(&mut parent, id(s, i));
        let g = *slot.entry(root).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[g].push((s, i));
    }

    let mut ev = Events::default();
    let mut units: Vec<Unit> = Vec::new();
    let text = |s: usize, r: &Range<usize>| sides[s].lines[r.clone()].concat();
    let is_trailing = |s: usize, c: &Change| sides[s].trailing && c.base.end == nb && !c.base.is_empty();
    for g in groups {
        let changes: Vec<(usize, &Change)> = g.iter().map(|&(s, i)| (s, &sides[s].changes[i])).collect();
        let start = changes.iter().map(|(_, c)| c.base.start).min().unwrap();
        let end = changes.iter().map(|(_, c)| c.base.end).max().unwrap();
        if let [(s, c)] = changes[..] {
            ev.trailing |= is_trailing(s, c);
            units.push(Unit {
                at: c.base.clone(),
                seg: Seg::Clean(text(s, &c.side)),
            });
            continue;
        }
        if changes.len() == 2 && changes.iter().all(|(_, c)| c.inserts()) {
            let run = |s: usize| {
                let c = changes.iter().find(|(x, _)| *x == s).unwrap().1;
                &sides[s].lines[c.side.clone()]
            };
            let (first, second) = if ours_first { (run(0), run(1)) } else { (run(1), run(0)) };
            let (out, once) = runs(first, second);
            // A run line without its newline (the end of a file) not last.
            if out.iter().rev().skip(1).any(|l| !l.ends_with('\n')) {
                let mut w = Walk::whole(base, ours, theirs);
                w.glued = true;
                let up = upstream_from(dl, dr, ours_first);
                if up.text.is_some() {
                    w.note(if up.glued {
                        Deviation::GluedLines
                    } else {
                        Deviation::RunOrder
                    });
                }
                return w;
            }
            if out != zip(first, second) {
                if once {
                    ev.once = true;
                } else {
                    ev.runs = true;
                }
            }
            units.push(Unit {
                at: start..start,
                seg: Seg::Clean(out.concat()),
            });
            continue;
        }
        // The region both sides changed, and what each made of it.
        let range = |s: usize| -> Option<Range<usize>> {
            let mine: Vec<&Change> = changes.iter().filter(|(x, _)| *x == s).map(|(_, c)| *c).collect();
            match (mine.first(), mine.last()) {
                (Some(f), Some(l)) => Some(f.side.start - (f.base.start - start)..l.side.end + (end - l.base.end)),
                _ => sides[s]
                    .equal_at
                    .get(start)
                    .copied()
                    .flatten()
                    .map(|x| x..x + (end - start)),
            }
        };
        let (Some(ro), Some(rt)) = (range(0), range(1)) else {
            return Walk::whole(base, ours, theirs);
        };
        let (vo, vt) = (text(0, &ro), text(1, &rt));
        let seg = if vo == vt {
            Seg::Clean(vo)
        } else if changes.iter().all(|(_, c)| c.side.is_empty()) {
            // Deletions only: every line either side deleted goes.
            let kept: String = (start..end)
                .filter(|p| !changes.iter().any(|(_, c)| c.base.contains(p)))
                .map(|p| base_lines[p])
                .collect();
            Seg::Clean(kept)
        } else {
            Seg::Conflict(Hunk {
                at: LineOverlap {
                    base: start..end,
                    ours: ro,
                    theirs: rt,
                },
                base: base_lines[start..end].concat(),
                ours: vo,
                theirs: vt,
            })
        };
        if let Seg::Clean(_) = seg {
            ev.trailing |= changes.iter().any(|(s, c)| is_trailing(*s, c));
        }
        units.push(Unit { at: start..end, seg });
    }
    // An insertion at a point goes before a change starting there.
    units.sort_by_key(|u| (u.at.start, u.at.end));

    let mut w = Walk::default();
    let mut p = 0;
    for u in units {
        w.push_clean(&base_lines[p..u.at.start].concat());
        match u.seg {
            Seg::Clean(t) => w.push_clean(&t),
            c @ Seg::Conflict(_) => w.segs.push(c),
        }
        p = u.at.end;
    }
    w.push_clean(&base_lines[p..].concat());

    // Safety net: a line emitted right after a line without its newline would
    // be glued to it. The whole file is then one conflict.
    let glued = w.glued;
    if glued {
        w = Walk::whole(base, ours, theirs);
        w.glued = true;
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
                if ev.once {
                    w.note(Deviation::SameInsertionOnce);
                }
                if ev.runs {
                    w.note(Deviation::RunOrder);
                }
                if up.glued {
                    w.note(Deviation::GluedLines);
                }
                if w.deviations.is_empty() {
                    w.note(Deviation::PositionOrder);
                }
            }
        }
    }
    w
}

/// Two runs inserted at one point, the older side's first. The same run, or
/// one holding the other as a whole prefix or suffix, is taken once (the
/// longer); otherwise both whole, the older first. The flag: taken once.
fn runs<'a>(first: &[&'a str], second: &[&'a str]) -> (Vec<&'a str>, bool) {
    let holds = |long: &[&str], short: &[&str]| long.starts_with(short) || long.ends_with(short);
    if holds(second, first) {
        (second.to_vec(), true)
    } else if holds(first, second) {
        (first.to_vec(), true)
    } else {
        ([first, second].concat(), false)
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
    upstream_from(dmp::line_diff(base, ours), dmp::line_diff(base, theirs), ours_first)
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

/// Two-way: no base. Equal lines once, every difference a conflict hunk.
pub(crate) fn walk_two_way(ours: &str, theirs: &str) -> Walk {
    let mut w = Walk::default();
    let diffs = dmp::line_diff(ours, theirs);
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
    split_diff(dmp::line_diff(a, b))
        .into_iter()
        .map(|(op, t)| (op as i8, t))
        .collect()
}
