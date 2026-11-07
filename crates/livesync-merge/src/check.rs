//! The check every clean line merge passes before it is written, built on
//! what the three files say rather than on the merge's own alignment.
//!
//! The line merge trusts how the diff lined up each side with the base. Where
//! lines repeat (blank lines, `---`, a recurring task, a line a side typed
//! again) the diff can line them up in more than one way, and the merge then
//! takes a line twice, drops a line a side added, brings back a line a side
//! removed or puts lines out of order. Patching the merge's steps one by one
//! did not close that, so "clean" is decided here:
//!
//! 1. **Anchors.** A line (not blank) that the base, ours and theirs each have
//!    exactly once, with the same text, is the same line in all three. The
//!    longest run of such lines in the same order in all three are the
//!    anchors. A right merge has each anchor once, in that order.
//! 2. **Gaps between anchors.** Where one side's gap is the base's, that side
//!    changed nothing there, and the merge's gap must be the other side's,
//!    byte for byte (blank lines and `---` included); where both sides' gaps
//!    are the same, it must be that. (A side that only added the newline to
//!    the base's last line changed nothing.) No alignment enters.
//! 3. **Moves.** Where a side has a line more often than the base in one gap
//!    and less often in another, or both sides added a line in different gaps
//!    and one of them removed lines in the gaps between, a line may have
//!    moved across the anchors between (or an anchor is a copy typed
//!    elsewhere): those gaps are checked as one stretch. A line one side moved
//!    and the other removed stays open.
//! 4. **Stretches both sides changed.** The merge's stretch must be what the
//!    merge makes of the stretch alone (with the anchors around it), and the
//!    same, up to how many blank lines stand together, under every other way of
//!    lining up the stretch: the longest common subsequences that match lines
//!    as early or as late as they can, on each side, in every pairing. Any of
//!    them that does not merge cleanly leaves it open. Further, it is open
//!    where:
//!    - one side's stretch is the other's with lines added (a writer that saw
//!      the other change and wrote on top of it), and the merge is not that;
//!    - a side keeps, right beside a change of its own, a base line the other
//!      side removed, and has every line the other side added in the file:
//!      it may be the other side's file with that line typed back (a blank
//!      line only where that would add a paragraph break);
//!    - a side has a line more often than the merge, where the other side
//!      kept every copy the base has (a copy taken as the other side's moved
//!      line);
//!    - a line both sides added is in the merge more often than either side
//!      has it, two copies standing at different places of the base (not two
//!      runs inserted at one point, which go in whole);
//!    - a side's lines that it and the merge each have once are out of its
//!      order.
//!
//! A stretch that fails is a conflict hunk of its own; anchors the merge does
//! not have once and in order leave the whole file one.

use crate::dmp::{self, Budget, Op, OutOfBudget};
use crate::lines::{self, blank, content, Hunk, Seg, Src, Walk};
use crate::LineOverlap;
use std::collections::HashMap;
use std::ops::Range;

/// The most cells (base lines times side lines) of a stretch both sides
/// changed that the check lines up in every way it tries; a larger stretch is
/// not checked, and stays open.
const MAX_CELLS: usize = 1 << 20;

/// Check a clean merge `merged` of `texts` (base, ours, theirs), all as lines
/// with their line ends. `None` where it passes; otherwise the view to show,
/// with a conflict hunk for each stretch that fails.
pub(crate) fn check(
    texts: [&[&str]; 3],
    merged: &[&str],
    ours_first: bool,
    budget: &mut Budget,
) -> Result<Option<Walk>, OutOfBudget> {
    let an = anchors(texts, budget)?;
    let Some(at_m) = in_merge(&an, texts[0], merged, budget)? else {
        let [b, o, t] = texts.map(|x| x.concat());
        return Ok(Some(Walk::whole(&b, &o, &t)));
    };
    // Whether a side has every line the other added, anywhere in the file:
    // it may be the other side's file with changes of its own on top.
    budget.spend(4 * texts.iter().map(|x| x.len()).sum::<usize>())?;
    let holds = [
        holds_added(texts[0], texts[2], texts[1]),
        holds_added(texts[0], texts[1], texts[2]),
    ];
    let mut view = View::default();
    let mut ok = true;
    for (lo, hi, clash) in stretches(&an, texts, budget)? {
        let from = |x: usize| if lo == 0 { 0 } else { an[lo - 1][x] + 1 };
        let to = |x: usize, n: usize| if hi == an.len() { n } else { an[hi][x] };
        let r: [Range<usize>; 3] = [0, 1, 2].map(|x| from(x)..to(x, texts[x].len()));
        let rm = if lo == 0 { 0 } else { at_m[lo - 1] + 1 }..if hi == an.len() { merged.len() } else { at_m[hi] };
        let [b, o, t] = [0, 1, 2].map(|x| &texts[x][r[x].clone()]);
        let m = &merged[rm];
        let before = (lo > 0).then(|| texts[0][an[lo - 1][0]]);
        let after = (hi < an.len()).then(|| texts[0][an[hi][0]]);
        // Each gap only one side changed (or both alike) is that side's,
        // byte for byte, inside a joined stretch too.
        let gaps_right = (lo..=hi).all(|g| {
            let from = |x: usize| if g == 0 { 0 } else { an[g - 1][x] + 1 };
            let to = |x: usize, n: usize| if g == an.len() { n } else { an[g][x] };
            let [gb, go, gt] = [0, 1, 2].map(|x| &texts[x][from(x)..to(x, texts[x].len())]);
            let gm = if g == 0 { 0 } else { at_m[g - 1] + 1 }..if g == an.len() { merged.len() } else { at_m[g] };
            trivial(gb, go, gt).is_none_or(|want| want.as_slice() == &merged[gm])
        });
        let right = !clash
            && gaps_right
            && match trivial(b, o, t) {
                Some(want) => want.as_slice() == m,
                None => stretch([before, after], [b, o, t], m, holds, ours_first, budget)?,
            };
        if right {
            view.clean(&m.concat());
        } else {
            ok = false;
            view.conflict(r, [b, o, t]);
        }
        if let Some(a) = after {
            view.clean(a);
        }
    }
    Ok((!ok).then(|| Walk {
        segs: view.segs,
        ..Walk::default()
    }))
}

/// What a stretch must be where at most one side changed it: the changed
/// side's (or both sides', where they changed it alike). A side that only
/// added the newline to the base's last line changed nothing; the other
/// side's stretch then ends with that line with its newline.
fn trivial<'a>(b: &[&'a str], o: &[&'a str], t: &[&'a str]) -> Option<Vec<&'a str>> {
    if o == b {
        return Some(t.to_vec());
    }
    if t == b || o == t {
        return Some(o.to_vec());
    }
    let open_end = b.last().filter(|l| !l.ends_with('\n'));
    let newline_only = |x: &[&str]| {
        open_end.is_some_and(|l| {
            x.len() == b.len() && x[..x.len() - 1] == b[..b.len() - 1] && content(x[x.len() - 1]) == content(l)
        })
    };
    let with_newline = |x: &[&'a str], from: &[&'a str]| -> Vec<&'a str> {
        let mut v = x.to_vec();
        if v.last() == b.last() {
            *v.last_mut().expect("not empty") = from[from.len() - 1];
        }
        v
    };
    if newline_only(o) {
        Some(with_newline(t, o))
    } else if newline_only(t) {
        Some(with_newline(o, t))
    } else {
        None
    }
}

/// The anchors: base, ours and theirs line numbers of lines (not blank) each
/// of the three has once (by content, the same text in each), in an order all
/// three keep.
fn anchors(t: [&[&str]; 3], budget: &mut Budget) -> Result<Vec<[usize; 3]>, OutOfBudget> {
    budget.spend(4 * t.iter().map(|x| x.len()).sum::<usize>())?;
    let mut n: HashMap<&str, [(u32, usize); 3]> = HashMap::new();
    for (k, text) in t.iter().enumerate() {
        for (i, l) in text.iter().enumerate() {
            if !blank(l) {
                let e = n.entry(content(l)).or_insert([(0, 0); 3]);
                e[k] = (e[k].0 + 1, i);
            }
        }
    }
    let cand: Vec<[usize; 3]> = t[0]
        .iter()
        .enumerate()
        .filter(|(_, l)| !blank(l))
        .filter_map(|(i, l)| {
            let e = n.get(content(l))?;
            (e.iter().all(|x| x.0 == 1) && t[1][e[1].1] == *l && t[2][e[2].1] == *l).then_some([i, e[1].1, e[2].1])
        })
        .collect();
    // Moved lines drop out: the longest run in ours' order, then of that in
    // theirs'.
    Ok(longest_rising(&longest_rising(&cand, 1), 2))
}

/// The stretches to check, as ranges of the gaps between anchors (gap `k`
/// before anchor `k`, the last after the last anchor), in order. A gap is a
/// stretch of its own, except where a side has a line more often than the
/// base in one gap and less often in another: the line may have moved across
/// the anchors between (or the anchor itself may be a copy typed elsewhere),
/// so those gaps and anchors are one stretch. With each stretch, whether a
/// line one side moved there is one the other side removed (a move against a
/// delete, which stays open).
fn stretches(
    an: &[[usize; 3]],
    t: [&[&str]; 3],
    budget: &mut Budget,
) -> Result<Vec<(usize, usize, bool)>, OutOfBudget> {
    budget.spend(4 * t.iter().map(|x| x.len()).sum::<usize>())?;
    // (line, gap) -> how often each text has it.
    let mut n: HashMap<(&str, usize), [usize; 3]> = HashMap::new();
    for (k, text) in t.iter().enumerate() {
        let mut gap = 0;
        for (i, l) in text.iter().enumerate() {
            if an.get(gap).is_some_and(|a| a[k] == i) {
                gap += 1;
                continue;
            }
            n.entry((key(l), gap)).or_insert([0; 3])[k] += 1;
        }
    }
    // For each line and side: the gaps where it has the line more often, and
    // less often, than the base.
    let mut moved: HashMap<(&str, usize), (usize, usize, bool, bool)> = HashMap::new();
    for (&(l, gap), c) in &n {
        for side in [1, 2] {
            if c[side] != c[0] {
                let e = moved.entry((l, side)).or_insert((gap, gap, false, false));
                e.0 = e.0.min(gap);
                e.1 = e.1.max(gap);
                if c[side] > c[0] {
                    e.2 = true;
                } else {
                    e.3 = true;
                }
            }
        }
    }
    let mut join: Vec<(usize, usize)> = moved
        .values()
        .filter(|e| e.2 && e.3 && e.0 < e.1)
        .map(|e| (e.0, e.1))
        .collect();
    // A line one side moved and the other removed: a move against a delete.
    let mut total: HashMap<&str, [usize; 3]> = HashMap::new();
    for (&(l, _), c) in &n {
        let e = total.entry(l).or_insert([0; 3]);
        for k in 0..3 {
            e[k] += c[k];
        }
    }
    let clash: Vec<usize> = moved
        .iter()
        .filter(|((l, side), e)| e.2 && e.3 && e.0 < e.1 && total[l][3 - side] < total[l][0])
        .map(|(_, e)| e.0)
        .collect();
    // A line (not blank) both sides added, in different gaps, where a side that added it
    // removed lines in another gap between: that side may have moved a line
    // the other changed in place (or the anchor between is a copy). Not two
    // lines added at two places, nor each side changing its own copy in place
    // (two copies of a recurring task).
    let mut removes = [vec![false; an.len() + 1], vec![false; an.len() + 1]];
    let mut added: HashMap<&str, [Vec<usize>; 2]> = HashMap::new();
    for (&(l, gap), c) in &n {
        for side in [1, 2] {
            if c[side] < c[0] {
                removes[side - 1][gap] = true;
            }
            if c[side] > c[0] {
                added.entry(l).or_default()[side - 1].push(gap);
            }
        }
    }
    for [o, t] in added.values() {
        for &g in o {
            for &h in t.iter().filter(|&&h| h != g) {
                let (lo, hi) = (g.min(h), g.max(h));
                let moved = |side: usize, at: usize| (lo..=hi).any(|x| x != at && removes[side][x]);
                if moved(0, g) || moved(1, h) {
                    join.push((lo, hi));
                }
            }
        }
    }
    join.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut next = 0;
    for (lo, hi) in join {
        if let Some(last) = out.last_mut().filter(|l| lo <= l.1) {
            last.1 = last.1.max(hi);
            next = last.1 + 1;
            continue;
        }
        out.extend((next..lo).map(|g| (g, g)));
        out.push((lo, hi));
        next = hi + 1;
    }
    out.extend((next..=an.len()).map(|g| (g, g)));
    Ok(out
        .into_iter()
        .map(|(lo, hi)| (lo, hi, clash.iter().any(|g| (lo..=hi).contains(g))))
        .collect())
}

/// A line as the stretches count it: blank lines as one.
fn key(l: &str) -> &str {
    if blank(l) {
        ""
    } else {
        content(l)
    }
}

/// The longest subsequence of `v` whose `k`th field rises.
fn longest_rising(v: &[[usize; 3]], k: usize) -> Vec<[usize; 3]> {
    let mut tails: Vec<usize> = Vec::new();
    let mut prev = vec![None; v.len()];
    for (i, x) in v.iter().enumerate() {
        let at = tails.partition_point(|&j| v[j][k] < x[k]);
        if at > 0 {
            prev[i] = Some(tails[at - 1]);
        }
        if at == tails.len() {
            tails.push(i);
        } else {
            tails[at] = i;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        out.push(v[i]);
        cur = prev[i];
    }
    out.reverse();
    out
}

/// Where the merge has each anchor: once each, the same text, in order; or
/// `None`.
fn in_merge(
    an: &[[usize; 3]],
    base: &[&str],
    merged: &[&str],
    budget: &mut Budget,
) -> Result<Option<Vec<usize>>, OutOfBudget> {
    budget.spend(4 * merged.len() + an.len())?;
    let mut n: HashMap<&str, (u32, usize)> = HashMap::new();
    for (i, l) in merged.iter().enumerate() {
        if !blank(l) {
            let e = n.entry(content(l)).or_insert((0, 0));
            *e = (e.0 + 1, i);
        }
    }
    let mut at = Vec::with_capacity(an.len());
    for a in an {
        let l = base[a[0]];
        match n.get(content(l)) {
            Some(&(1, i)) if merged[i] == l && at.last().is_none_or(|&p| p < i) => at.push(i),
            _ => return Ok(None),
        }
    }
    Ok(Some(at))
}

/// Check a stretch both sides changed (`b`, `o`, `t`, between the anchors
/// `ctx`) against the merge's stretch `m` (see the module docs).
fn stretch(
    ctx: [Option<&str>; 2],
    [b, o, t]: [&[&str]; 3],
    m: &[&str],
    holds: [bool; 2],
    ours_first: bool,
    budget: &mut Budget,
) -> Result<bool, OutOfBudget> {
    // A writer that saw the other side's stretch and only added lines to it.
    if (subsequence(t, o) && m != o) || (subsequence(o, t) && m != t) {
        return Ok(false);
    }
    for side in [o, t] {
        if !keeps_order(side, m) {
            return Ok(false);
        }
    }
    if lost([b, o, t], m) {
        return Ok(false);
    }
    let size = |x: &[&str]| x.len();
    if size(b) * size(o).max(size(t)) > MAX_CELLS {
        return Ok(false);
    }
    let text = |x: &[&str]| -> String {
        let mut s = String::new();
        s.extend(ctx[0]);
        s.extend(x.iter().copied());
        s.extend(ctx[1]);
        s
    };
    let (sb, so, st) = (text(b), text(o), text(t));
    let want = text(m);
    budget.read(&sb)?;
    budget.read(&so)?;
    budget.read(&st)?;
    // The merge of the stretch alone, with the diff's alignment.
    let (dl, dr) = (dmp::line_diff(&sb, &so, budget)?, dmp::line_diff(&sb, &st, budget)?);
    let texts = [&sb, &so, &st].map(|x| lines::pieces(x).collect::<Vec<_>>());
    let at = [
        lines::positions(&texts[0], &so, &dl),
        lines::positions(&texts[0], &st, &dr),
    ];
    match lines::merge_with(&sb, &so, &st, &dl, &dr, ours_first, false, budget)? {
        Some((r, trace)) if r == want && !split_by_base(&texts, &at, &trace) => {}
        _ => return Ok(false),
    }
    // Every other alignment: as it merges cleanly, the same.
    let lb: Vec<&str> = lines::pieces(&sb).collect();
    let wl: Vec<&str> = lines::pieces(&want).collect();
    let mut ways = [vec![dl], vec![dr]];
    for (x, side) in [&so, &st].into_iter().enumerate() {
        let ls: Vec<&str> = lines::pieces(side).collect();
        for d in alignments(&lb, &ls, budget)? {
            if !ways[x].contains(&d) {
                ways[x].push(d);
            }
        }
    }
    // Where one side has every line the other added, it may be the other
    // side's stretch with lines of its own: a base line it keeps and the
    // other removed may be one it typed back.
    let holds = [holds[0] || holds_added(b, t, o), holds[1] || holds_added(b, o, t)];
    for (i, dl) in ways[0].iter().enumerate() {
        for (j, dr) in ways[1].iter().enumerate() {
            let ((ko, bo), (kt, bt)) = (lines::kept(&lb, &so, dl), lines::kept(&lb, &st, dr));
            // A base line `mine` keeps beside a change of its own, and the
            // other side removed. A blank line only where typing it back
            // would part two lines the merge has together (the nearest lines
            // around it that are not blank: a paragraph break).
            let back = |mine: &[Option<usize>], lines: &[&str], beside: &[bool], other: &[Option<usize>]| {
                (0..mine.len()).any(|i| match mine[i] {
                    Some(x) if beside[i] && other[i].is_none() => {
                        let before = lines[..x].iter().rev().find(|l| !blank(l));
                        let after = lines[x + 1..].iter().find(|l| !blank(l));
                        !blank(lb[i]) || before.zip(after).is_some_and(|(a, b)| together(&wl, a, b))
                    }
                    _ => false,
                })
            };
            if (holds[0] && back(&ko, &texts[1], &bo, &kt)) || (holds[1] && back(&kt, &texts[2], &bt, &ko)) {
                return Ok(false);
            }
            if i == 0 && j == 0 {
                continue;
            }
            match lines::merge_with(&sb, &so, &st, dl, dr, ours_first, true, budget)? {
                Some((other, _)) if same_shape(&other, &want) => {}
                _ => return Ok(false),
            }
        }
    }
    Ok(true)
}

/// Whether a line (not blank) is in `m` less often than a side has it, where
/// the other side kept every copy the base has: each copy one side added is
/// in the merge unless the other side removed one. Fewer means the merge took
/// a copy as the other side's line that side moved there (it removed one
/// elsewhere and put one here), which is one reading of two.
fn lost([b, o, t]: [&[&str]; 3], m: &[&str]) -> bool {
    fn count<'a>(x: &[&'a str]) -> HashMap<&'a str, usize> {
        let mut n: HashMap<&str, usize> = HashMap::new();
        for l in x.iter().filter(|l| !blank(l)) {
            *n.entry(content(l)).or_default() += 1;
        }
        n
    }
    let (nb, no, nt, nm) = (count(b), count(o), count(t), count(m));
    let get = |n: &HashMap<&str, usize>, l: &str| n.get(l).copied().unwrap_or(0);
    let short = |mine: &HashMap<&str, usize>, other: &HashMap<&str, usize>| {
        mine.iter()
            .any(|(l, &k)| get(other, l) >= get(&nb, l) && get(&nm, l) < k)
    };
    short(&no, &nt) || short(&nt, &no)
}

/// Whether a merge (by its `trace` over `texts`, base, ours and theirs, and
/// where each side's lines stand in the base, `at`) has a line both sides
/// added more often than either side has it, two of its copies standing at
/// different places of the base: they are not two runs inserted at one
/// point, which go in whole by design, but likely the same line both sides
/// added, which the diffs put at two points.
fn split_by_base(texts: &[Vec<&str>; 3], at: &[Vec<usize>; 2], trace: &[Src]) -> bool {
    let line = |s: &Src| match *s {
        Src::Base(i) => texts[0][i],
        Src::Ours(j) | Src::Both(j, _) => texts[1][j],
        Src::Theirs(k) => texts[2][k],
    };
    let place = |s: &Src| match *s {
        Src::Base(i) => 2 * i + 1,
        Src::Ours(j) | Src::Both(j, _) => at[0][j],
        Src::Theirs(k) => at[1][k],
    };
    fn count<'a>(x: impl Iterator<Item = &'a str>) -> HashMap<&'a str, usize> {
        let mut n: HashMap<&str, usize> = HashMap::new();
        for l in x {
            *n.entry(key(l)).or_default() += 1;
        }
        n
    }
    let [nb, no, nt] = [0, 1, 2].map(|k| count(texts[k].iter().copied()));
    let nm = count(trace.iter().map(line));
    let get = |n: &HashMap<&str, usize>, l: &str| n.get(l).copied().unwrap_or(0);
    let mut last: HashMap<&str, usize> = HashMap::new();
    for (i, s) in trace.iter().enumerate() {
        let l = key(line(s));
        let (x, y, z) = (get(&nb, l), get(&no, l), get(&nt, l));
        if let Some(p) = last.insert(l, i) {
            if y > x && z > x && get(&nm, l) > y.max(z) && place(&trace[p]) != place(s) {
                return true;
            }
        }
    }
    false
}

/// Whether two texts are the same lines, blank lines aside, with a blank line
/// between the same two lines (a paragraph break): they may differ only in how
/// many blank lines stand together, or before the first line or after the
/// last.
fn same_shape(a: &str, b: &str) -> bool {
    fn shape(s: &str) -> Vec<(bool, &str)> {
        let mut out = Vec::new();
        let mut gap = false;
        for l in lines::pieces(s) {
            if blank(l) {
                gap = true;
            } else {
                out.push((gap && !out.is_empty(), content(l)));
                gap = false;
            }
        }
        out
    }
    shape(a) == shape(b)
}

/// Whether `m` has the lines `a` and `b` (not blank) next to each other.
fn together(m: &[&str], a: &str, b: &str) -> bool {
    !blank(a)
        && !blank(b)
        && m.windows(2)
            .any(|w| content(w[0]) == content(a) && content(w[1]) == content(b))
}

/// Whether `mine` has every line (not blank) `other` has more often than the
/// base `b`, at least as often as `other`.
fn holds_added(b: &[&str], other: &[&str], mine: &[&str]) -> bool {
    fn count<'a>(x: &[&'a str]) -> HashMap<&'a str, usize> {
        let mut n: HashMap<&str, usize> = HashMap::new();
        for l in x.iter().filter(|l| !blank(l)) {
            *n.entry(content(l)).or_default() += 1;
        }
        n
    }
    let (nb, no, nm) = (count(b), count(other), count(mine));
    let get = |n: &HashMap<&str, usize>, l: &str| n.get(l).copied().unwrap_or(0);
    no.iter().all(|(l, &k)| k <= get(&nb, l) || get(&nm, l) >= k)
}

/// Whether `short`'s lines are all in `long`, in order.
fn subsequence(short: &[&str], long: &[&str]) -> bool {
    let mut it = long.iter();
    short.iter().all(|s| it.any(|l| l == s))
}

/// Whether the lines (not blank) that `side` has once and `m` has once come
/// in `m` in `side`'s order.
fn keeps_order(side: &[&str], m: &[&str]) -> bool {
    fn once<'a>(x: &[&'a str]) -> HashMap<&'a str, (u32, usize)> {
        let mut n: HashMap<&str, (u32, usize)> = HashMap::new();
        for (i, l) in x.iter().enumerate().filter(|(_, l)| !blank(l)) {
            let e = n.entry(content(l)).or_insert((0, 0));
            *e = (e.0 + 1, i);
        }
        n
    }
    let (a, b) = (once(side), once(m));
    let mut pairs: Vec<(usize, usize)> = a
        .iter()
        .filter(|(_, e)| e.0 == 1)
        .filter_map(|(l, e)| b.get(l).filter(|f| f.0 == 1).map(|f| (e.1, f.1)))
        .collect();
    pairs.sort_unstable();
    pairs.windows(2).all(|w| w[0].1 < w[1].1)
}

/// Line diffs of `side` against `base` for longest common subsequences that
/// match lines as early, or as late, as they can in either text.
fn alignments(base: &[&str], side: &[&str], budget: &mut Budget) -> Result<Vec<Vec<(Op, String)>>, OutOfBudget> {
    let (n, m) = (base.len(), side.len());
    budget.spend((n + 1) * (m + 1))?;
    let w = m + 1;
    // Longest common subsequence of base[i..] and side[j..].
    // The base's last line without its newline is the side's line with one.
    let eq = |i: usize, j: usize| {
        base[i] == side[j] || (i + 1 == n && !base[i].ends_with('\n') && content(base[i]) == content(side[j]))
    };
    let mut l = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i * w + j] = if eq(i, j) {
                l[(i + 1) * w + j + 1] + 1
            } else {
                l[(i + 1) * w + j].max(l[i * w + j + 1])
            };
        }
    }
    let mut out: Vec<Vec<(Op, String)>> = Vec::new();
    for way in 0..4u8 {
        let (mut i, mut j) = (0, 0);
        let mut pairs = Vec::new();
        while i < n && j < m {
            let here = l[i * w + j];
            let skip_base = l[(i + 1) * w + j] == here;
            let skip_side = l[i * w + j + 1] == here;
            let take = eq(i, j) && l[(i + 1) * w + j + 1] + 1 == here;
            let step = match way {
                0 if take => 0,
                0 if skip_base => 1,
                1 if take => 0,
                1 if skip_side => 2,
                2 if skip_base => 1,
                2 if skip_side => 2,
                3 if skip_side => 2,
                3 if skip_base => 1,
                0 => 2,
                1 => 1,
                _ => 0,
            };
            match step {
                0 => {
                    pairs.push((i, j));
                    i += 1;
                    j += 1;
                }
                1 => i += 1,
                _ => j += 1,
            }
        }
        let d = diff_of(base, side, &pairs);
        if !out.contains(&d) {
            out.push(d);
        }
    }
    Ok(out)
}

/// The line diff that keeps exactly the matched `pairs` (base line, side line).
fn diff_of(base: &[&str], side: &[&str], pairs: &[(usize, usize)]) -> Vec<(Op, String)> {
    let mut out: Vec<(Op, String)> = Vec::new();
    let mut put = |op: Op, s: &str| {
        if s.is_empty() {
            return;
        }
        match out.last_mut() {
            Some((o, t)) if *o == op => t.push_str(s),
            _ => out.push((op, s.to_string())),
        }
    };
    let (mut i, mut j) = (0, 0);
    for &(pi, pj) in pairs.iter().chain([(base.len(), side.len())].iter()) {
        put(Op::Delete, &base[i..pi].concat());
        put(Op::Insert, &side[j..pj].concat());
        if pi < base.len() {
            put(Op::Equal, base[pi]);
        }
        (i, j) = (pi + 1, pj + 1);
    }
    out
}

/// The view of a merge that failed the check, stretch by stretch.
#[derive(Default)]
struct View {
    segs: Vec<Seg>,
}

impl View {
    fn clean(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        match self.segs.last_mut() {
            Some(Seg::Clean(t)) => t.push_str(s),
            _ => self.segs.push(Seg::Clean(s.to_string())),
        }
    }
    fn conflict(&mut self, at: [Range<usize>; 3], [b, o, t]: [&[&str]; 3]) {
        let [base, ours, theirs] = at;
        self.segs.push(Seg::Conflict(Hunk {
            at: LineOverlap { base, ours, theirs },
            base: b.concat(),
            ours: o.concat(),
            theirs: t.concat(),
        }));
    }
}
