//! Upstream's three-way line merge (`ConflictManager.mergeSensibly`), with D9's
//! two clarifications, two fixes for edits upstream loses or garbles (a trailing
//! deletion, two runs inserted at one point) and one safety stop, and the same
//! walk producing conflict hunks for `merge3`.

use crate::dmp::{self, Op};
use crate::{Deviation, LineOverlap};
use std::ops::Range;

/// One line of a side's diff against the base.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    op: Op,
    text: String,
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
    /// A side's diff ended in a deletion, carried as a deletion.
    pub trailing: bool,
    /// Two insertion runs were placed whole where upstream would interleave
    /// them (or join two lines by the order alone).
    pub reordered: bool,
}

/// Which of this crate's fixes to upstream's line merge apply. Both are on in
/// [`Mode::FIXED`]; switching one off gives upstream's behaviour for it, which
/// is how [`walk`] finds out whether a fix changed the result (and so is a
/// deviation to report).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mode {
    /// Carry a deletion that is the last diff ([`Deviation::TrailingDeletion`]).
    pub carry_trailing: bool,
    /// Two runs inserted at one point go in whole, the older first
    /// ([`Deviation::RunOrder`]).
    pub whole_runs: bool,
}

impl Mode {
    pub const FIXED: Mode = Mode {
        carry_trailing: true,
        whole_runs: true,
    };
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
}

/// Lines keeping their `\n`; the last may lack it. Upstream's
/// `split(/([^\n]*\n)/).filter(f => f != "")`.
fn pieces(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n')
}

/// `splitDiffPiece`: one item per line, a deletion's lines paired with the
/// insertion that follows it. Upstream turns a deletion that is the last diff
/// into EQUAL lines, so a deletion at the end of a file is not carried; with
/// `carry` it stays a deletion ([`Deviation::TrailingDeletion`]). The flag
/// says whether there was such a deletion.
fn split_diff(src: Vec<(Op, String)>, carry: bool) -> (Vec<Item>, bool) {
    let mut trailing = false;
    let mut ret = Vec::new();
    let mut src = src.into_iter();
    while let Some((op, text)) = src.next() {
        let ps: Vec<&str> = pieces(&text).collect();
        if op != Op::Delete {
            ret.extend(ps.iter().map(|p| Item {
                op,
                text: p.to_string(),
            }));
            continue;
        }
        match src.next() {
            Some((nop, ntext)) => {
                let mut pair: std::collections::VecDeque<&str> = pieces(&ntext).collect();
                if nop == Op::Insert {
                    for p in &ps {
                        ret.push(Item {
                            op: Op::Delete,
                            text: p.to_string(),
                        });
                        if let Some(q) = pair.pop_front() {
                            ret.push(Item {
                                op: Op::Insert,
                                text: q.to_string(),
                            });
                        }
                    }
                    ret.extend(pair.iter().map(|q| Item {
                        op: nop,
                        text: q.to_string(),
                    }));
                } else {
                    ret.extend(ps.iter().map(|p| Item {
                        op: Op::Delete,
                        text: p.to_string(),
                    }));
                    ret.extend(pair.iter().map(|q| Item {
                        op: nop,
                        text: q.to_string(),
                    }));
                }
            }
            None => {
                trailing = true;
                let op = if carry { Op::Delete } else { Op::Equal };
                ret.extend(ps.iter().map(|p| Item {
                    op,
                    text: p.to_string(),
                }))
            }
        }
    }
    (ret, trailing)
}

/// Positions along one side's items.
struct Track {
    items: Vec<Item>,
    /// base lines consumed before item i (len + 1 entries)
    base_before: Vec<usize>,
    /// side lines produced before item i (len + 1 entries)
    side_before: Vec<usize>,
    /// item index of base line p when that line is EQUAL on this side
    equal_at: Vec<Option<usize>>,
}

impl Track {
    fn new(items: Vec<Item>, base_lines: usize) -> Track {
        let mut base_before = Vec::with_capacity(items.len() + 1);
        let mut side_before = Vec::with_capacity(items.len() + 1);
        let mut equal_at = vec![None; base_lines + 1];
        let (mut b, mut s) = (0usize, 0usize);
        for (i, it) in items.iter().enumerate() {
            base_before.push(b);
            side_before.push(s);
            match it.op {
                Op::Equal => {
                    if b < equal_at.len() {
                        equal_at[b] = Some(i);
                    }
                    b += 1;
                    s += 1;
                }
                Op::Delete => b += 1,
                Op::Insert => s += 1,
            }
        }
        base_before.push(b);
        side_before.push(s);
        Track {
            items,
            base_before,
            side_before,
            equal_at,
        }
    }
    fn get(&self, i: usize) -> (Op, &str) {
        self.items
            .get(i)
            .map(|it| (it.op, it.text.as_str()))
            .unwrap_or((Op::Equal, ""))
    }
    fn len(&self) -> usize {
        self.items.len()
    }
    fn base_at(&self, i: usize) -> usize {
        self.base_before[i.min(self.len())]
    }
    fn side_at(&self, i: usize) -> usize {
        self.side_before[i.min(self.len())]
    }
    fn text(&self, r: Range<usize>) -> String {
        let r = r.start.min(self.len())..r.end.min(self.len());
        self.items[r]
            .iter()
            .filter(|it| it.op != Op::Delete)
            .map(|it| it.text.as_str())
            .collect()
    }
}

/// Upstream's merge of `ours` (left, the winning revision) and `theirs` (right)
/// against `base`, with every fix of [`Mode::FIXED`]. `ours_first` is upstream's
/// `leftLeaf.mtime <= rightLeaf.mtime`: where both sides insert at one point,
/// that side's lines go first.
///
/// Where upstream stops at the first conflict, the walk records the conflicting
/// region, resynchronises on the next base line both sides kept, and goes on, so
/// the caller gets every overlap and a full `merge3`. Up to the first conflict
/// the walk is upstream's, so "clean" here is "merged" there, apart from the
/// recorded deviations.
pub(crate) fn walk(base: &str, ours: &str, theirs: &str, ours_first: bool) -> Walk {
    walk_in(base, ours, theirs, ours_first, Mode::FIXED)
}

/// [`walk`] with only the fixes `mode` names. A fix is reported as a deviation
/// when switching it off changes the result: another text, or merged against
/// open.
pub(crate) fn walk_in(base: &str, ours: &str, theirs: &str, ours_first: bool, mode: Mode) -> Walk {
    let mut w = walk_raw(base, ours, theirs, ours_first, mode);
    let fixes = [
        (
            w.trailing && mode.carry_trailing,
            Deviation::TrailingDeletion,
            Mode {
                carry_trailing: false,
                ..mode
            },
        ),
        (
            w.reordered && mode.whole_runs,
            Deviation::RunOrder,
            Mode {
                whole_runs: false,
                ..mode
            },
        ),
    ];
    let text = w.text();
    for (applied, d, without) in fixes {
        if applied && walk_raw(base, ours, theirs, ours_first, without).text() != text {
            w.note(d);
        }
    }
    w
}

fn walk_raw(base: &str, ours: &str, theirs: &str, ours_first: bool, mode: Mode) -> Walk {
    let base_lines: Vec<&str> = pieces(base).collect();
    let (li, lt) = split_diff(dmp::line_diff(base, ours), mode.carry_trailing);
    let (ri, rt) = split_diff(dmp::line_diff(base, theirs), mode.carry_trailing);
    let l = Track::new(li, base_lines.len());
    let r = Track::new(ri, base_lines.len());
    let mut w = Walk {
        trailing: lt || rt,
        ..Walk::default()
    };
    let (mut li, mut ri) = (0usize, 0usize);
    'walk: loop {
        if li >= l.len() && ri >= r.len() {
            break;
        }
        let (li0, ri0) = (li, ri);
        let (lop, ltext) = l.get(li);
        let (rop, rtext) = r.get(ri);
        li += 1;
        ri += 1;
        let conflict = 'step: {
            if lop == Op::Equal && rop == Op::Equal && ltext == rtext {
                w.push_clean(ltext);
                continue 'walk;
            }
            if lop == Op::Delete && rop == Op::Delete && ltext == rtext {
                let (nlop, nltext) = l.get(li);
                let (nrop, nrtext) = r.get(ri);
                let nl_ins = nlop == Op::Insert;
                let nr_ins = nrop == Op::Insert;
                if nl_ins != nr_ins || (nl_ins && nr_ins && nltext != nrtext) {
                    break 'step Some(false);
                }
                continue 'walk;
            }
            if lop == Op::Insert && rop == Op::Insert {
                let lend = run_end(&l, li0);
                let rend = run_end(&r, ri0);
                let lrun: Vec<&str> = l.items[li0..lend].iter().map(|i| i.text.as_str()).collect();
                let rrun: Vec<&str> = r.items[ri0..rend].iter().map(|i| i.text.as_str()).collect();
                let runs = merge_runs(
                    &lrun,
                    &rrun,
                    ours_first,
                    lend == l.len() && rend == r.len(),
                    mode.whole_runs,
                );
                w.reordered |= runs.reordered;
                match runs.result {
                    Ok((text, dev)) => {
                        if let Some(d) = dev {
                            w.note(d);
                        }
                        w.push_clean(&text);
                        li = lend;
                        ri = rend;
                        continue 'walk;
                    }
                    Err(dev) => {
                        if let Some(d) = dev {
                            w.note(d);
                        }
                        break 'step Some(true);
                    }
                }
            }
            if lop == Op::Insert {
                ri -= 1;
                w.push_clean(ltext);
                continue 'walk;
            }
            if rop == Op::Insert {
                li -= 1;
                w.push_clean(rtext);
                continue 'walk;
            }
            if ltext != rtext {
                break 'step Some(false);
            }
            if lop == Op::Delete {
                if rop == Op::Equal {
                    continue 'walk;
                }
                break 'step Some(false);
            }
            if rop == Op::Delete {
                if lop == Op::Equal {
                    continue 'walk;
                }
                break 'step Some(false);
            }
            // Upstream's "Weird condition" breaks out of the loop with the merge
            // so far marked good, which would return a truncated text. D9: open.
            w.note(Deviation::FallthroughOpen);
            Some(false)
        };
        if let Some(insert_only) = conflict {
            let (nli, nri) = record_conflict(&mut w, &l, &r, &base_lines, li0, ri0, insert_only);
            li = nli;
            ri = nri;
        }
    }
    // Safety net: a line emitted right after a line without its newline glues
    // the two into one (upstream does, e.g. two appends after an unterminated
    // last line). The whole file is then one conflict.
    if w.glued {
        w.note(Deviation::GluedLines);
        let nb = base_lines.len();
        w.segs = vec![Seg::Conflict(Hunk {
            at: LineOverlap {
                base: 0..nb,
                ours: 0..l.side_at(l.len()),
                theirs: 0..r.side_at(r.len()),
            },
            base: base.to_string(),
            ours: ours.to_string(),
            theirs: theirs.to_string(),
        })];
    }
    w
}

fn run_end(t: &Track, from: usize) -> usize {
    let mut e = from;
    while e < t.len() && t.items[e].op == Op::Insert {
        e += 1;
    }
    e
}

/// Record a conflict starting at items `li0`/`ri0` and return where both sides
/// meet again: the next base line both kept unchanged (or the end).
#[allow(clippy::too_many_arguments)]
fn record_conflict(
    w: &mut Walk,
    l: &Track,
    r: &Track,
    base_lines: &[&str],
    li0: usize,
    ri0: usize,
    insert_only: bool,
) -> (usize, usize) {
    let b = l.base_at(li0).min(r.base_at(ri0));
    let nb = base_lines.len();
    let start = if insert_only { b } else { b + 1 };
    let mut end = (nb, l.len(), r.len());
    for p in start..nb {
        if let (Some(i), Some(j)) = (l.equal_at[p], r.equal_at[p]) {
            if i >= li0 && j >= ri0 {
                end = (p, i, j);
                break;
            }
        }
    }
    let (p, lend, rend) = end;
    let li0 = li0.min(l.len());
    let ri0 = ri0.min(r.len());
    w.segs.push(Seg::Conflict(Hunk {
        at: LineOverlap {
            base: b..p,
            ours: l.side_at(li0)..l.side_at(lend),
            theirs: r.side_at(ri0)..r.side_at(rend),
        },
        base: base_lines[b.min(p)..p].concat(),
        ours: l.text(li0..lend),
        theirs: r.text(ri0..rend),
    }));
    (lend, rend)
}

/// What [`merge_runs`] made of two runs inserted at one point.
struct Runs {
    /// The merged lines and the deviation they carry, or a conflict (with the
    /// deviation that made it one, if upstream would have merged).
    result: Result<(String, Option<Deviation>), Option<Deviation>>,
    /// The whole-run order gave another result than upstream's interleaving.
    pub reordered: bool,
}

/// Both sides insert at the same point.
///
/// Upstream pairs the two runs line by line (equal lines once, otherwise the
/// older side's line first) and appends the longer run's tail, so two
/// multi-line appends come out interleaved. D9 keeps both "the older first" so
/// that two appends to a log both survive intact: with `whole`, the older
/// side's run goes in whole, then the newer side's ([`Deviation::RunOrder`]).
///
/// D9's first clarification: the same insertion made on both sides is taken
/// once. The runs are aligned on their common lines and only the lines between
/// them are placed (whole, or paired as upstream); if a shared line still
/// cannot be taken once (the sides inserted it in a different order), it is a
/// conflict. Upstream's own pairing (without `whole`) is used as it is unless
/// it would take a line both sides inserted twice
/// ([`Deviation::SameInsertionOnce`]).
fn merge_runs(l: &[&str], r: &[&str], ours_first: bool, at_end: bool, whole: bool) -> Runs {
    let (first, second) = if ours_first { (l, r) } else { (r, l) };
    let upstream = interleaved(first, second, at_end);
    if !whole {
        return Runs {
            result: upstream,
            reordered: false,
        };
    }
    let (mut uf, mut us) = (Vec::new(), Vec::new());
    zip(first, second, &mut Vec::new(), &mut uf, &mut us);
    let once = shares(&uf, &us).then_some(Deviation::SameInsertionOnce);
    let (out, uf, us) = aligned(first, second, true);
    let result = if shares(&uf, &us) {
        Err(once)
    } else if glues(&out, at_end) {
        Err(match upstream {
            Err(Some(Deviation::GluedLines)) => Some(Deviation::GluedLines),
            _ => once,
        })
    } else {
        Ok((out.concat(), once))
    };
    let text = |r: &Result<(String, Option<Deviation>), Option<Deviation>>| r.as_ref().ok().map(|x| x.0.clone());
    Runs {
        reordered: text(&result) != text(&upstream),
        result,
    }
}

/// Upstream's pairing, aligned on common lines where it would take a shared
/// line twice. `Err(Some(_))` is a conflict upstream would have merged.
fn interleaved(
    first: &[&str],
    second: &[&str],
    at_end: bool,
) -> Result<(String, Option<Deviation>), Option<Deviation>> {
    let mut out: Vec<&str> = Vec::new();
    let (mut un_first, mut un_second) = (Vec::new(), Vec::new());
    zip(first, second, &mut out, &mut un_first, &mut un_second);
    let mut dev = None;
    if shares(&un_first, &un_second) {
        dev = Some(Deviation::SameInsertionOnce);
        let (o, uf, us) = aligned(first, second, false);
        if shares(&uf, &us) {
            return Err(dev);
        }
        out = o;
    }
    if glues(&out, at_end) {
        return Err(Some(Deviation::GluedLines));
    }
    Ok((out.concat(), dev))
}

/// The two runs aligned on their common lines (taken once); the lines between
/// two common lines placed whole (`whole`: the first run's, then the second's)
/// or paired as upstream does. Also returns the placed lines of each run that
/// were not taken once.
#[allow(clippy::type_complexity)]
fn aligned<'a>(first: &[&'a str], second: &[&'a str], whole: bool) -> (Vec<&'a str>, Vec<&'a str>, Vec<&'a str>) {
    let (mut out, mut un_first, mut un_second) = (Vec::new(), Vec::new(), Vec::new());
    let mut gap = |f: &[&'a str], s: &[&'a str], out: &mut Vec<&'a str>| {
        if whole {
            out.extend(f);
            out.extend(s);
            un_first.extend(f);
            un_second.extend(s);
        } else {
            zip(f, s, out, &mut un_first, &mut un_second);
        }
    };
    let joined_first: String = first.concat();
    let joined_second: String = second.concat();
    let (mut fseg, mut sseg): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
    let (mut fi, mut si) = (0usize, 0usize);
    for (op, text) in &dmp::line_diff(&joined_first, &joined_second) {
        let n = pieces(text).count();
        match op {
            Op::Delete => {
                fseg.extend(&first[fi..fi + n]);
                fi += n;
            }
            Op::Insert => {
                sseg.extend(&second[si..si + n]);
                si += n;
            }
            Op::Equal => {
                gap(&fseg, &sseg, &mut out);
                fseg.clear();
                sseg.clear();
                out.extend(&first[fi..fi + n]);
                fi += n;
                si += n;
            }
        }
    }
    gap(&fseg, &sseg, &mut out);
    (out, un_first, un_second)
}

/// A line without its newline that is not the file's last line.
fn glues(out: &[&str], at_end: bool) -> bool {
    let last = out.len().saturating_sub(1);
    out.iter()
        .enumerate()
        .any(|(i, p)| !p.ends_with('\n') && (i < last || !at_end))
}

fn zip<'a>(
    first: &[&'a str],
    second: &[&'a str],
    out: &mut Vec<&'a str>,
    un_first: &mut Vec<&'a str>,
    un_second: &mut Vec<&'a str>,
) {
    for k in 0..first.len().max(second.len()) {
        match (first.get(k), second.get(k)) {
            (Some(a), Some(b)) if a == b => out.push(a),
            (Some(a), Some(b)) => {
                out.push(a);
                out.push(b);
                un_first.push(a);
                un_second.push(b);
            }
            (Some(a), None) => {
                out.push(a);
                un_first.push(a);
            }
            (None, Some(b)) => {
                out.push(b);
                un_second.push(b);
            }
            (None, None) => {}
        }
    }
}

fn shares(a: &[&str], b: &[&str]) -> bool {
    a.iter().any(|x| b.contains(x))
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
pub(crate) fn split_for_test(a: &str, b: &str, carry: bool) -> Vec<(i8, String)> {
    split_diff(dmp::line_diff(a, b), carry)
        .0
        .into_iter()
        .map(|i| (i.op as i8, i.text))
        .collect()
}
