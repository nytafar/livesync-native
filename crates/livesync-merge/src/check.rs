//! The check every clean line merge passes before it is written. It decides
//! on what the three files say, never on how the merge lined them up: the
//! merge proposes, the check decides.
//!
//! The line merge trusts how the diff lined up each side with the base. Where
//! lines repeat (blank lines, `---`, a recurring task, a line a side typed
//! again) the diff can line them up in more than one way, and the merge then
//! takes a line twice, drops a line a side added, brings back a line a side
//! removed or puts lines out of order. So "clean" is decided here:
//!
//! 0. **Line ends.** A merge that mixes `\r\n` and bare `\n` line ends
//!    where none of the three files does (one side rewrote every line end,
//!    the other added lines) is open, the whole file.
//! 1. **Anchors.** A line (not blank) that the base, ours and theirs each have
//!    exactly once, with the same text, is the same line in all three, if it
//!    is in every longest run of lines each has once rising in ours' order and
//!    in every one rising in theirs' (where two lines swapped places, either
//!    may be the one that moved, so neither is an anchor; a line that differs
//!    only in a missing final line end counts here too). A right merge has
//!    each anchor once, in that order. The anchors cut the files into gaps.
//!    Two gaps around an anchor are judged as one stretch where:
//!    - a side has fewer blank lines than the base on one side of the anchor
//!      and more on the other, in all or of one text (an empty line, a line of
//!      spaces): the anchor may have moved across a blank line, or the blank
//!      line across it;
//!    - a longest common subsequence of a side's lines around the anchor
//!      (its two gaps, and two anchors each way, or its own two gaps where
//!      those are too many to line up) with the base's need not match the
//!      anchor (lines matched across it instead are as many): the anchor may
//!      be the line that moved, and the lines it passed stayed. Only the lines
//!      between the two texts' common start and end are lined up: an anchor
//!      in the common start or end is in every longest common subsequence,
//!      and so is one where all of one side's lines there are in the other's.
//! 2. **Stretches only one side changed.** Where one side's stretch is the
//!    base's, that side changed nothing there, and the merge's stretch must be
//!    the other side's, byte for byte (blank lines and `---` included); where
//!    both sides' stretches are the same, it must be that. (A side that only
//!    added the newline to the base's last line changed nothing.)
//! 3. **Moves.** A line (not blank) that a side inserted in one gap and
//!    removed from another (by a longest common subsequence of each gap it
//!    changed with the base's) may have moved between them, or the lines
//!    between may have moved past it. The two readings place the other
//!    side's change alike but at the ends of what moved, so those gaps stay
//!    open only where the other side's change can meet the move: where it
//!    changed how often the line is in one of them; where it changed (or
//!    inserted between) the lines right around the line where it left (a
//!    deletion right beside the other side's change); or where it did so
//!    where the line arrived and the lines it passed may have moved instead
//!    (a longest common subsequence of the gaps between pairs the line's old
//!    copy with its new one). Elsewhere, and where the line arrived at the
//!    other side's insertion with no such rival reading (a task carried to
//!    the end, a line appended there), the placement rules decide as for any
//!    change. And whatever moved: a line neither side has more often than the
//!    base is not in the merge more often than in both sides, and one neither
//!    side has less often is not in it less often than in either.
//! 4. **Stretches both sides changed.** Each side's change is read in every
//!    way the texts allow: every longest common subsequence of the side's
//!    stretch with the base's ([`readings`]). Every pair of readings is merged
//!    by the placement rules ([`plain`]), which share nothing with the merge's
//!    own code. The stretch is clean only where every pair merges, all to the
//!    same text, blank lines and `---` included, and the merge's stretch is
//!    that text. A reading's merge is a conflict where it has a line (not
//!    blank) placed by pieces of the two sides at two places, one of them an
//!    insertion, more often than either side's stretch or with only blank
//!    lines between the two copies ([`twice`]): both sides added it, and
//!    another reading has them at one point, one insertion taken once.
//!    (Within one stretch only: copies apart across an anchor are two lines.)
//!    Further, it is open where:
//!    - one side's stretch is the other's with lines added (a writer that saw
//!      the other change and wrote on top of it), and the merge is not that,
//!      where the other side's change added lines (not only deleted some);
//!    - under some reading a side keeps, right beside a change of its own, a
//!      base line the other side deleted (or replaced with lines this side
//!      inserted right there), and has every line the other side added in the
//!      stretch: it may be the other side's file with that line typed back (a
//!      blank line only where that would add a paragraph break). This is the
//!      price of a symmetric merge: a deletion right beside the other side's
//!      change stays open;
//!    - under some reading a side removed a line and put the same line
//!      elsewhere in the stretch (a move), where the other side removed it.
//!
//!    Where the readings are too many to try ([`MAX_READINGS`], [`MAX_PAIRS`]),
//!    the stretch too large to line up ([`MAX_CELLS`]), or judging it would
//!    cost too much ([`MAX_STRETCH_COST`]), it is open.
//!
//! A stretch that fails is a conflict hunk of its own; anchors the merge does
//! not have once and in order leave the whole file one. All the work is
//! counted against the merge's budget.

use crate::dmp::{Budget, OutOfBudget};
use crate::lines::{blank, content, Hunk, Seg, Walk};
use crate::LineOverlap;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

/// The most cells (base lines times side lines) of a stretch both sides
/// changed that the check lines up; a larger one stays open.
const MAX_CELLS: usize = 1 << 20;

/// How many anchors each way the test for an anchor a reading may skip
/// looks past.
const WINDOW: usize = 2;

/// The most readings of one side's change in a stretch that the check tries;
/// a stretch with more stays open.
const MAX_READINGS: usize = 64;

/// The most pairs of readings the check merges for one stretch.
const MAX_PAIRS: usize = 1024;

/// The most work (in the budget's steps) the check spends judging the
/// readings of one stretch; a stretch that would take more stays open.
const MAX_STRETCH_COST: usize = 4_000_000;

/// Check a clean merge `merged` of `texts` (base, ours, theirs), all as lines
/// with their line ends. `None` where it passes; otherwise the view to show,
/// with a conflict hunk for each stretch that fails.
pub(crate) fn check(
    texts: [&[&str]; 3],
    merged: &[&str],
    ours_first: bool,
    budget: &mut Budget,
) -> Result<Option<Walk>, OutOfBudget> {
    // Line ends mixed where no file mixes them: one side rewrote the line
    // ends, and the merge took the other side's lines into it.
    budget.spend(texts.iter().map(|x| x.len()).sum::<usize>() + merged.len())?;
    if mixes(merged) && !texts.iter().any(|x| mixes(x)) {
        let [b, o, t] = texts.map(|x| x.concat());
        return Ok(Some(Walk::whole(&b, &o, &t)));
    }
    let an = anchors(texts, budget)?;
    let Some(at_m) = in_merge(&an, texts[0], merged, budget)? else {
        let [b, o, t] = texts.map(|x| x.concat());
        return Ok(Some(Walk::whole(&b, &o, &t)));
    };
    // Each gap's lines in base, ours, theirs and the merge.
    let lens = [texts[0].len(), texts[1].len(), texts[2].len(), merged.len()];
    let at = |g: usize, x: usize| if x < 3 { an[g][x] } else { at_m[g] };
    let gaps: Vec<[Range<usize>; 4]> = (0..=an.len())
        .map(|g| {
            [0, 1, 2, 3].map(|x| {
                let from = if g == 0 { 0 } else { at(g - 1, x) + 1 };
                let to = if g == an.len() { lens[x] } else { at(g, x) };
                from..to
            })
        })
        .collect();
    let doubt = in_doubt([texts[0], texts[1], texts[2], merged], &gaps, budget)?;
    // A side with fewer blank lines than the base on one side of an anchor
    // and more on the other may have moved the anchor across a blank line
    // (or the blank line across it): which of the two lines stayed is open to
    // reading, so the two gaps are judged as one stretch. Blank lines count
    // in all, and by their text (a line of spaces is not an empty line).
    budget.spend(4 * lens.iter().sum::<usize>())?;
    let blanks = |g: usize, x: usize| -> HashMap<&str, usize> {
        let mut n: HashMap<&str, usize> = HashMap::new();
        for l in texts[x][gaps[g][x].clone()].iter().filter(|l| blank(l)) {
            *n.entry(content(l)).or_default() += 1;
            *n.entry("\n").or_default() += 1;
        }
        n
    };
    // An anchor that a longest common subsequence of a side's text with the
    // base's may skip (matching as many lines across it instead) may be the
    // line that moved, not the lines it passed: judged within one stretch,
    // every reading counts.
    // Judged over the anchor's gaps and [`WINDOW`] anchors each way, so that a
    // block of lines that moved past another (two anchors skipped together)
    // shows too.
    let mut weak = vec![false; an.len()];
    for (k, w) in weak.iter_mut().enumerate() {
        for x in [1, 2] {
            // The window: two anchors each way, or (where that is too large to
            // line up) the anchor's own two gaps.
            for width in [WINDOW, 0] {
                let (lo, hi) = (k.saturating_sub(width), (k + 1 + width).min(an.len()));
                let same = |g: usize| texts[x][gaps[g][x].clone()] == texts[0][gaps[g][0].clone()];
                if (lo..=hi).all(same) {
                    break;
                }
                let rb = gaps[lo][0].start..gaps[hi][0].end;
                let rs = gaps[lo][x].start..gaps[hi][x].end;
                let at = (an[k][0] - rb.start, an[k][x] - rs.start);
                match skippable(&texts[0][rb], &texts[x][rs], at, budget)? {
                    Some(skip) => {
                        *w |= skip;
                        break;
                    }
                    None if width == 0 => *w = true,
                    None => {}
                }
            }
        }
    }
    let joined: Vec<bool> = (0..an.len())
        .map(|k| {
            if weak[k] {
                return true;
            }
            let ([b0, b1], [s0, s1]) = (
                [blanks(k, 0), blanks(k + 1, 0)],
                [1, 2].map(|x| [blanks(k, x), blanks(k + 1, x)]),
            );
            let get = |n: &HashMap<&str, usize>, w: &str| n.get(w).copied().unwrap_or(0);
            [s0, s1].iter().any(|[x0, x1]| {
                x0.keys().chain(x1.keys()).chain(b0.keys()).chain(b1.keys()).any(|w| {
                    let (a, c) = (get(x0, w).cmp(&get(&b0, w)), get(x1, w).cmp(&get(&b1, w)));
                    a != c && a.is_ne() && c.is_ne()
                })
            })
        })
        .collect();
    let mut view = View::default();
    let mut ok = true;
    let mut lo = 0;
    while lo <= an.len() {
        let mut hi = lo;
        while hi < an.len() && joined[hi] {
            hi += 1;
        }
        let r: [Range<usize>; 4] = [0, 1, 2, 3].map(|x| gaps[lo][x].start..gaps[hi][x].end);
        budget.spend(2 * r.iter().map(|x| x.len()).sum::<usize>())?;
        let [b, o, t] = [0, 1, 2].map(|x| &texts[x][r[x].clone()]);
        let m = &merged[r[3].clone()];
        let ctx = [
            (lo > 0).then(|| texts[0][an[lo - 1][0]]),
            (hi < an.len()).then(|| texts[0][an[hi][0]]),
        ];
        let right = !doubt[lo..=hi].contains(&true)
            && match trivial(b, o, t) {
                Some(want) => want.as_slice() == m,
                None => contested(ctx, [b, o, t], m, ours_first, budget)?,
            };
        if right {
            view.clean(&m.concat());
        } else {
            ok = false;
            view.conflict([r[0].clone(), r[1].clone(), r[2].clone()], [b, o, t]);
        }
        if let Some(a) = ctx[1] {
            view.clean(a);
        }
        lo = hi + 1;
    }
    Ok((!ok).then(|| Walk {
        segs: view.segs,
        ..Walk::default()
    }))
}

/// Whether a longest common subsequence of `b` and `s` need not match the
/// pair `at` (an anchor, a line each has once): one that leaves it out is as
/// long. `None` where the lines it would take to tell are past
/// [`MAX_CELLS`].
///
/// Only the lines between their common start and end need lining up: one
/// longest common subsequence matches the common start and end line by line,
/// so an anchor there is in every one (leaving it out loses it, and gains
/// nothing). Nor need they where one side's middle lines are all in the
/// other's in order: a longest common subsequence is all of the shorter, and
/// one without the anchor is shorter.
fn skippable(b: &[&str], s: &[&str], at: (usize, usize), budget: &mut Budget) -> Result<Option<bool>, OutOfBudget> {
    budget.spend(b.len() + s.len())?;
    let pre = b.iter().zip(s).take_while(|(x, y)| same_line(x, y)).count();
    let suf = b[pre..]
        .iter()
        .rev()
        .zip(s[pre..].iter().rev())
        .take_while(|(x, y)| same_line(x, y))
        .count();
    let (b, s) = (&b[pre..b.len() - suf], &s[pre..s.len() - suf]);
    if at.0 < pre || at.1 < pre || at.0 >= pre + b.len() || at.1 >= pre + s.len() {
        return Ok(Some(false));
    }
    let at = (at.0 - pre, at.1 - pre);
    let within = |short: &[&str], long: &[&str]| {
        let mut it = long.iter();
        short.iter().all(|x| it.any(|y| same_line(x, y)))
    };
    if within(b, s) || within(s, b) {
        return Ok(Some(false));
    }
    let (n, m) = (b.len(), s.len());
    if n * m > MAX_CELLS {
        return Ok(None);
    }
    budget.spend(2 * (n + 1) * (m + 1))?;
    let lcs = |skip: Option<(usize, usize)>| -> u32 {
        let w = m + 1;
        let mut l = vec![0u32; (n + 1) * w];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                l[i * w + j] = if same_line(b[i], s[j]) && skip != Some((i, j)) {
                    l[(i + 1) * w + j + 1] + 1
                } else {
                    l[(i + 1) * w + j].max(l[i * w + j + 1])
                };
            }
        }
        l[0]
    };
    Ok(Some(lcs(Some(at)) == lcs(None)))
}

/// Whether some lines end in `\r\n` and others in a bare `\n`.
fn mixes(x: &[&str]) -> bool {
    let crlf = x.iter().filter(|l| l.ends_with("\r\n")).count();
    let ended = x.iter().filter(|l| l.ends_with('\n')).count();
    crlf > 0 && crlf < ended
}

/// What a stretch must be where at most one side changed it: the changed side's
/// (or both sides', where they changed it alike). A side that only added the
/// newline to the base's last line changed nothing; the other side's stretch then
/// ends with that line with its newline (unless the other side gave it
/// another line end).
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
    // Not where the other side gave that line another line end: which is the
    // merge's is open. Nor where the other side's last line, without its line
    // end, is the base's last line and may be another copy of it (the base or
    // that side has the line elsewhere too): which copy is the base's last
    // line, the one the newline goes to, is open.
    let other_end = |x: &[&str], from: &[&str]| {
        let e = from[from.len() - 1];
        let copy = x.last() == b.last()
            && open_end.is_some_and(|l| {
                let c = content(l);
                b[..b.len() - 1]
                    .iter()
                    .chain(&x[..x.len() - 1])
                    .any(|y| content(y) == c)
            });
        copy || x
            .iter()
            .any(|l| l.ends_with('\n') && *l != e && open_end.is_some_and(|b| content(l) == content(b)))
    };
    if newline_only(o) && !other_end(t, o) {
        Some(with_newline(t, o))
    } else if newline_only(t) && !other_end(o, t) {
        Some(with_newline(o, t))
    } else {
        None
    }
}

/// The anchors: base, ours and theirs line numbers of lines (not blank) each
/// of the three has once (by content, the same text in each), in every
/// longest run all three keep in order (see the module docs).
fn anchors(t: [&[&str]; 3], budget: &mut Budget) -> Result<Vec<[usize; 3]>, OutOfBudget> {
    budget.spend(8 * t.iter().map(|x| x.len()).sum::<usize>())?;
    let mut n: HashMap<&str, [(u32, usize); 3]> = HashMap::new();
    for (k, text) in t.iter().enumerate() {
        for (i, l) in text.iter().enumerate() {
            if !blank(l) {
                let e = n.entry(content(l)).or_insert([(0, 0); 3]);
                e[k] = (e[k].0 + 1, i);
            }
        }
    }
    // Lines each file has once, by content (one may be its file's last line
    // without a line end).
    let cand: Vec<[usize; 3]> = t[0]
        .iter()
        .enumerate()
        .filter(|(_, l)| !blank(l))
        .filter_map(|(i, l)| {
            let e = n.get(content(l))?;
            e.iter().all(|x| x.0 == 1).then_some([i, e[1].1, e[2].1])
        })
        .collect();
    // Moved lines drop out: the lines in every longest run in ours' order
    // and in every longest run in theirs'. Where two lines swapped places,
    // either may be the one that moved: neither is an anchor. Of the rest,
    // the anchors are those with the same text in all three.
    let theirs: HashSet<[usize; 3]> = in_every_rising(&cand, 2).into_iter().collect();
    Ok(in_every_rising(&cand, 1)
        .into_iter()
        .filter(|a| theirs.contains(a) && t[1][a[1]] == t[0][a[0]] && t[2][a[2]] == t[0][a[0]])
        .collect())
}

/// For each gap between anchors (gap `k` before anchor `k`, the last after
/// the last anchor; `gaps` gives each text's lines of it, and the merge's
/// last), whether a line in it is in doubt (see the module docs):
///
/// - a line one side may have moved into or out of it, where the other side
///   changed how often that line is in the gap it left or the one it went
///   to, or changed the lines right around it where it left;
/// - a line neither side has more often than the base, which the merge has
///   more often than both sides; or one neither side has less often, which
///   the merge has less often than either side. No right merge does that,
///   however the lines moved.
fn in_doubt(t: [&[&str]; 4], gaps: &[[Range<usize>; 4]], budget: &mut Budget) -> Result<Vec<bool>, OutOfBudget> {
    budget.spend(8 * t.iter().map(|x| x.len()).sum::<usize>())?;
    // (line, gap) -> how often each text has it, and the totals.
    let mut n: HashMap<(&str, usize), [usize; 4]> = HashMap::new();
    let mut total: HashMap<&str, [usize; 4]> = HashMap::new();
    for (g, r) in gaps.iter().enumerate() {
        for k in 0..4 {
            for l in t[k][r[k].clone()].iter().filter(|l| !blank(l)) {
                n.entry((content(l), g)).or_insert([0; 4])[k] += 1;
                total.entry(content(l)).or_insert([0; 4])[k] += 1;
            }
        }
    }
    // For each gap and side: whether the side changed it.
    let changed: Vec<[bool; 2]> = gaps
        .iter()
        .map(|r| [1, 2].map(|k| t[k][r[k].clone()] != t[0][r[0].clone()]))
        .collect();
    // For each side: the lines (not blank) it inserted and removed in each
    // gap it changed, by a longest common subsequence of its gap with the
    // base's. A line inserted in one gap and removed in another may have
    // moved between them; so may the lines it passed (the anchors between),
    // which no count shows where the side also changed another copy.
    // (line, side) -> the gaps it inserted it in, and removed it from.
    type Gaps = (Vec<usize>, Vec<usize>);
    let mut way: HashMap<(&str, usize), Gaps> = HashMap::new();
    // (gap, side) -> the side's gap lined up with the base's.
    let mut aligned: HashMap<(usize, usize), Vec<Option<usize>>> = HashMap::new();
    for (g, r) in gaps.iter().enumerate() {
        for side in [1, 2] {
            if !changed[g][side - 1] {
                continue;
            }
            let (b, s) = (&t[0][r[0].clone()], &t[side][r[side].clone()]);
            let al = gap_align(b, s, budget)?;
            let mut kept = vec![false; b.len()];
            for &i in al.iter().flatten() {
                kept[i] = true;
            }
            for (l, _) in s.iter().zip(&al).filter(|(l, a)| a.is_none() && !blank(l)) {
                way.entry((content(l), side)).or_default().0.push(g);
            }
            for (l, _) in b.iter().zip(&kept).filter(|(l, k)| !**k && !blank(l)) {
                way.entry((content(l), side)).or_default().1.push(g);
            }
            aligned.insert((g, side), al);
        }
    }
    // A move meets the other side's change where the readings of the move
    // place that change differently, or where it is a deletion right beside
    // the other side's change (as a deletion is, see [`typed_back`]):
    // - the other side changed how often the line is in a gap the move
    //   touches;
    // - the other side changed (or inserted between) the lines right around
    //   a copy of it where it left (blank lines and lines the side removed
    //   between do not part them);
    // - the same where it arrived, but only where the lines it passed may
    //   have moved the other way instead: no more of them than the side
    //   moved (the anchors between count at least), else that reading is
    //   longer and the one the texts show is this line moved. Two readings
    //   differ only at the ends of what moved; elsewhere in those gaps, and
    //   where the line arrived at the other side's insertion (two runs at one
    //   point), the placement rules decide as for any change.
    budget.spend(4 * t.iter().map(|x| x.len()).sum::<usize>())?;
    let mut open = vec![false; gaps.len()];
    for (&(l, side), (ins, rem)) in &way {
        if !ins.iter().any(|g| rem.iter().any(|h| g != h)) {
            continue;
        }
        let other = 3 - side;
        // How many lines the side inserted in a gap, and removed from one.
        let size = |g: usize| -> (usize, usize) {
            let al = &aligned[&(g, side)];
            let kept = al.iter().flatten().count();
            (al.len() - kept, gaps[g][0].len() - kept)
        };
        let mut rival = false;
        for &g in ins {
            for &h in rem {
                if rival || g == h || g.abs_diff(h) > size(g).0.min(size(h).1) {
                    continue;
                }
                let (lo, hi) = (g.min(h), g.max(h));
                let rb = gaps[lo][0].start..gaps[hi][0].end;
                let rs = gaps[lo][side].start..gaps[hi][side].end;
                let (from, to) = (gaps[h][0].clone(), gaps[g][side].clone());
                let (wb, ws) = (&t[0][rb.clone()], &t[side][rs.clone()]);
                let pairs: Vec<(usize, usize)> = from
                    .filter(|&i| content(t[0][i]) == l)
                    .flat_map(|i| {
                        to.clone()
                            .filter(|&j| content(t[side][j]) == l)
                            .map(move |j| (i - rb.start, j - rs.start))
                    })
                    .collect();
                rival = pairable(wb, ws, &pairs, budget)?;
            }
        }
        let meets = |g: usize, arrived: bool| -> bool {
            if !changed[g][other - 1] {
                return false;
            }
            let c = n.get(&(l, g)).copied().unwrap_or([0; 4]);
            if c[other] != c[0] {
                return true;
            }
            if arrived && !rival {
                return false;
            }
            let r = &gaps[g];
            let b = &t[0][r[0].clone()];
            let al = &aligned[&(g, side)];
            let reach = changed_reach(b, &t[other][r[other].clone()]);
            // The lines around each copy, as the base lines before and after
            // (`None`: the gap's end, an anchor), kept by the side and not
            // blank.
            let mut around: Vec<(Option<usize>, Option<usize>)> = Vec::new();
            if arrived {
                let s = &t[side][r[side].clone()];
                let fixed = |j: usize| al[j].filter(|_| !blank(s[j]));
                for j in (0..s.len()).filter(|&j| content(s[j]) == l) {
                    around.push(((0..j).rev().find_map(fixed), (j + 1..s.len()).find_map(fixed)));
                }
            } else {
                let mut kept = vec![false; b.len()];
                for &i in al.iter().flatten() {
                    kept[i] = true;
                }
                let fixed = |i: &usize| kept[*i] && !blank(b[*i]);
                for i in (0..b.len()).filter(|&i| content(b[i]) == l) {
                    around.push(((0..i).rev().find(fixed), (i + 1..b.len()).find(fixed)));
                }
            }
            around.iter().any(|&(lo, hi)| {
                // The neighbours' lines (and those between), and the points
                // between them.
                let lines = lo.unwrap_or(0)..hi.map_or(b.len(), |x| x + 1);
                let points = lo.map_or(0, |x| x + 1)..=hi.unwrap_or(b.len());
                if reach.is_empty() {
                    points.contains(&reach.start)
                } else {
                    reach.start < lines.end && lines.start < reach.end
                }
            })
        };
        if ins.iter().any(|&g| meets(g, true)) || rem.iter().any(|&g| meets(g, false)) {
            for &g in ins.iter().chain(rem) {
                open[g] = true;
            }
        }
    }
    for (&(l, g), c) in &n {
        let [b, o, th, m] = total[l];
        let more = o <= b && th <= b && m > o.max(th);
        let fewer = o >= b && th >= b && m < o.max(th);
        if (more || fewer) && c[1..].iter().any(|&x| x != c[0]) {
            open[g] = true;
        }
    }
    Ok(open)
}

/// Whether some longest common subsequence of `b` and `s` pairs one of
/// `pairs` (a base line and a side line). Past [`MAX_CELLS`], taken as so.
fn pairable(b: &[&str], s: &[&str], pairs: &[(usize, usize)], budget: &mut Budget) -> Result<bool, OutOfBudget> {
    let (n, m) = (b.len(), s.len());
    if pairs.is_empty() {
        return Ok(false);
    }
    if n * m > MAX_CELLS {
        return Ok(true);
    }
    budget.spend(2 * (n + 1) * (m + 1))?;
    let w = m + 1;
    // Longest common subsequences of the ends from each cell on, and of the
    // starts up to it.
    let mut after = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            after[i * w + j] = if same_line(b[i], s[j]) {
                after[(i + 1) * w + j + 1] + 1
            } else {
                after[(i + 1) * w + j].max(after[i * w + j + 1])
            };
        }
    }
    let mut before = vec![0u32; (n + 1) * w];
    for i in 1..=n {
        for j in 1..=m {
            before[i * w + j] = if same_line(b[i - 1], s[j - 1]) {
                before[(i - 1) * w + j - 1] + 1
            } else {
                before[(i - 1) * w + j].max(before[i * w + j - 1])
            };
        }
    }
    let longest = after[0];
    Ok(pairs
        .iter()
        .any(|&(i, j)| same_line(b[i], s[j]) && before[i * w + j] + 1 + after[(i + 1) * w + j + 1] == longest))
}

/// A side's gap `s` lined up with the base's `b` by one longest common
/// subsequence: the base line of each of the side's lines it kept. Past
/// [`MAX_CELLS`], only their common start and end.
fn gap_align(b: &[&str], s: &[&str], budget: &mut Budget) -> Result<Vec<Option<usize>>, OutOfBudget> {
    let pre = b.iter().zip(s).take_while(|(x, y)| x == y).count();
    let suf = b[pre..]
        .iter()
        .rev()
        .zip(s[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let mut out: Vec<Option<usize>> = vec![None; s.len()];
    for (k, x) in out.iter_mut().enumerate().take(pre) {
        *x = Some(k);
    }
    for (k, x) in out.iter_mut().rev().enumerate().take(suf) {
        *x = Some(b.len() - 1 - k);
    }
    let (bm, sm) = (&b[pre..b.len() - suf], &s[pre..s.len() - suf]);
    let (n, m) = (bm.len(), sm.len());
    if n * m > MAX_CELLS {
        budget.spend(n + m)?;
        return Ok(out);
    }
    budget.spend((n + 1) * (m + 1))?;
    let w = m + 1;
    let mut l = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i * w + j] = if bm[i] == sm[j] {
                l[(i + 1) * w + j + 1] + 1
            } else {
                l[(i + 1) * w + j].max(l[i * w + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if bm[i] == sm[j] {
            out[pre + j] = Some(pre + i);
            i += 1;
            j += 1;
        } else if l[(i + 1) * w + j] >= l[i * w + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    Ok(out)
}

/// The base lines a side's gap `s` changed against the base's `b`: between
/// their common start and end, and further back as far as what it changed
/// could slide along equal lines (a line inserted after a copy of it may as
/// well be before it). Empty: an insertion at that point.
fn changed_reach(b: &[&str], s: &[&str]) -> Range<usize> {
    let pre = b.iter().zip(s).take_while(|(x, y)| x == y).count();
    let suf = b[pre..]
        .iter()
        .rev()
        .zip(s[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let hi = b.len() - suf;
    // The change as base lines lo..bl that became the side's lo..sl (the
    // common start is the same lines in both). It may as well have been one
    // line earlier where the line before it is the last line of both parts.
    let (mut lo, mut bl, mut sl) = (pre, hi, s.len() - suf);
    while lo > 0 && (bl > lo || sl > lo) {
        let p = b[lo - 1];
        if (bl > lo && b[bl - 1] != p) || (sl > lo && s[sl - 1] != p) {
            break;
        }
        lo -= 1;
        bl -= 1;
        sl -= 1;
    }
    lo..hi
}

/// The elements of `v` that are in every longest subsequence of `v` whose
/// `k`th field rises: those in some longest one, alone at their place in it.
fn in_every_rising(v: &[[usize; 3]], k: usize) -> Vec<[usize; 3]> {
    // The longest rising run ending at each element, and starting at it.
    fn ending(keys: impl Iterator<Item = usize>) -> Vec<usize> {
        let mut tails: Vec<usize> = Vec::new();
        keys.map(|x| {
            let at = tails.partition_point(|&t| t < x);
            if at == tails.len() {
                tails.push(x);
            } else {
                tails[at] = x;
            }
            at + 1
        })
        .collect()
    }
    let f = ending(v.iter().map(|x| x[k]));
    let mut g = ending(v.iter().rev().map(|x| usize::MAX - x[k]));
    g.reverse();
    let longest = f.iter().copied().max().unwrap_or(0);
    let some = |i: usize| f[i] + g[i] - 1 == longest;
    let mut at = vec![0usize; longest + 1];
    for i in (0..v.len()).filter(|&i| some(i)) {
        at[f[i]] += 1;
    }
    (0..v.len())
        .filter(|&i| some(i) && at[f[i]] == 1)
        .map(|i| v[i])
        .collect()
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

/// Judge a stretch both sides changed (`b`, `o`, `t`, between the anchors
/// `ctx`) against the merge's stretch `m` (see the module docs).
fn contested(
    ctx: [Option<&str>; 2],
    [b, o, t]: [&[&str]; 3],
    m: &[&str],
    ours_first: bool,
    budget: &mut Budget,
) -> Result<bool, OutOfBudget> {
    // A writer that saw the other side's change and only added lines to it.
    // (A side that only deleted lines changed nothing a writer could keep:
    // one that has them again either restored them, which the guard may
    // undo, or never deleted them.)
    let added = |x: &[&str]| !subsequence(x, b);
    if (added(t) && subsequence(t, o) && m != o) || (added(o) && subsequence(o, t) && m != t) {
        return Ok(false);
    }
    if b.len() * o.len().max(t.len()) > MAX_CELLS {
        return Ok(false);
    }
    let (Some(ro), Some(rt)) = (readings(b, o, budget)?, readings(b, t, budget)?) else {
        return Ok(false);
    };
    if ro.len() * rt.len() > MAX_PAIRS {
        return Ok(false);
    }
    // Whether a side has every line the other added in the stretch: it may
    // be the other side's stretch with lines of its own on top.
    budget.spend(4 * (b.len() + o.len() + t.len()))?;
    let holds = [holds_added(b, t, o), holds_added(b, o, t)];
    // What judging every pair costs; past a share of the budget of its own,
    // the stretch stays open rather than the whole merge.
    let size = b.len() + o.len() + t.len() + 1;
    if (ro.len() + rt.len() + ro.len() * rt.len()) * 8 * size > MAX_STRETCH_COST {
        return Ok(false);
    }
    let near = around(b);
    budget.spend((ro.len() + rt.len()) * 8 * size)?;
    let ro: Vec<Reading> = ro.into_iter().map(|al| Reading::new(b, &near, o, al)).collect();
    let rt: Vec<Reading> = rt.into_iter().map(|al| Reading::new(b, &near, t, al)).collect();
    for ao in &ro {
        for at in &rt {
            budget.spend(8 * size)?;
            if (holds[0] && typed_back(ctx, b, &near, (o, ao), (t, at), m))
                || (holds[1] && typed_back(ctx, b, &near, (t, at), (o, ao), m))
            {
                return Ok(false);
            }
            // A line a side moved within the stretch (removed, and put
            // elsewhere), that the other side removed: a move against a
            // deletion or an edit.
            if (0..b.len()).any(|i| (ao.moved[i] || at.moved[i]) && ao.removed[i].is_some() && at.removed[i].is_some())
            {
                return Ok(false);
            }
            match plain(b, [(o, &ao.al), (t, &at.al)], ours_first) {
                Some(x) if x == m => {}
                _ => return Ok(false),
            }
        }
    }
    Ok(true)
}

/// One reading of a side's change in a stretch (see [`readings`]), with what the
/// rules below ask of it.
struct Reading {
    /// The side's line for each base line it keeps.
    al: Vec<Option<usize>>,
    /// For each base line, whether a change of the side is next to it: it
    /// removed the line before or after, or inserted lines right before or
    /// after it. Blank lines between do not part a line from a change.
    beside: Vec<bool>,
    /// For each base line the side removed, the side's lines of the change
    /// that removed it (empty for a deletion).
    removed: Vec<Option<Range<usize>>>,
    /// The side's insertions (that removed no base line): at which gap
    /// (before which base line), and its lines.
    ins: Vec<(usize, Range<usize>)>,
    /// For each base line, whether the side removed it and has a line it
    /// inserted with the same text (not blank): it may have moved it.
    moved: Vec<bool>,
}

impl Reading {
    fn new(b: &[&str], near: &[Around], side: &[&str], al: Vec<Option<usize>>) -> Reading {
        let n = b.len();
        let cs = changes(n, side.len(), &al, 0);
        let mut removed = vec![None; n];
        let mut ins = Vec::new();
        let mut at_gap = vec![false; n + 1];
        let mut put: HashSet<&str> = HashSet::new();
        for c in &cs {
            for x in c.base.clone() {
                removed[x] = Some(c.side.clone());
            }
            if c.base.is_empty() {
                ins.push((c.base.start, c.side.clone()));
            }
            if !c.side.is_empty() {
                at_gap[c.base.start] = true;
                at_gap[c.base.end] = true;
            }
            put.extend(side[c.side.clone()].iter().filter(|l| !blank(l)).map(|l| content(l)));
        }
        // How many lines the side removed before each line, and how many
        // gaps it inserted at before each gap.
        let mut gone = vec![0usize; n + 1];
        let mut put_at = vec![0usize; n + 2];
        for i in 0..n {
            gone[i + 1] = gone[i] + usize::from(al[i].is_none());
        }
        for g in 0..=n {
            put_at[g + 1] = put_at[g] + usize::from(at_gap[g]);
        }
        let beside = (0..n)
            .map(|i| {
                let a = &near[i];
                let removed_around = gone[a.lines.1 + 1] - gone[a.lines.0] - usize::from(al[i].is_none());
                removed_around > 0 || put_at[a.gaps.1 + 1] > put_at[a.gaps.0]
            })
            .collect();
        let moved = (0..n)
            .map(|i| removed[i].is_some() && !blank(b[i]) && put.contains(content(b[i])))
            .collect();
        Reading {
            al,
            beside,
            removed,
            ins,
            moved,
        }
    }
}

/// The base lines around a base line up to the nearest ones that are not
/// blank (those included, or the file's ends), as first and last, and the
/// gaps between them.
struct Around {
    lines: (usize, usize),
    gaps: (usize, usize),
}

/// [`Around`] for each base line, in one pass each way.
fn around(b: &[&str]) -> Vec<Around> {
    let n = b.len();
    let mut lo = vec![None; n];
    let mut last = None;
    for i in 0..n {
        lo[i] = last;
        if !blank(b[i]) {
            last = Some(i);
        }
    }
    let mut hi = vec![None; n];
    let mut next = None;
    for i in (0..n).rev() {
        hi[i] = next;
        if !blank(b[i]) {
            next = Some(i);
        }
    }
    (0..n)
        .map(|i| Around {
            lines: (lo[i].unwrap_or(0), hi[i].unwrap_or(n - 1)),
            gaps: (lo[i].map_or(0, |l: usize| l + 1), hi[i].unwrap_or(n)),
        })
        .collect()
}

/// Whether `mine` (its lines and reading) keeps, right beside a change of its
/// own, a base line the `other` side's reading removed, where `mine` may be
/// the other side's stretch with that line typed back:
///
/// - the other side deleted the line (its change added nothing, or only
///   blank lines); or
/// - the other side replaced it with lines that `mine` inserted right beside
///   it (it has the other's new lines and the old one). An edit of another
///   line that happens to give the same text (each side ticked a different
///   copy of a task) is no such insertion.
///
/// A blank line counts only where typing it back would part two lines the
/// merge `m` has together (the nearest lines around it that are not blank,
/// the anchors `ctx` at the stretch's ends: a paragraph break). This is the price
/// of a symmetric merge: a deletion right beside the other side's change
/// stays open.
fn typed_back(
    ctx: [Option<&str>; 2],
    b: &[&str],
    near: &[Around],
    (mine, me): (&[&str], &Reading),
    (theirs, other): (&[&str], &Reading),
    m: &[&str],
) -> bool {
    (0..b.len()).any(|i| {
        let (Some(x), Some(r)) = (me.al[i], other.removed[i].as_ref()) else {
            return false;
        };
        if !me.beside[i] {
            return false;
        }
        let new: Vec<&str> = theirs[r.clone()].iter().filter(|l| !blank(l)).copied().collect();
        if !new.is_empty() {
            let gaps = near[i].gaps.0..=near[i].gaps.1;
            let held = me
                .ins
                .iter()
                .any(|(g, sr)| gaps.contains(g) && holds_added(&[], &new, &mine[sr.clone()]));
            if !held {
                return false;
            }
        }
        if !blank(b[i]) {
            return true;
        }
        let before = mine[..x].iter().rev().find(|l| !blank(l)).copied().or(ctx[0]);
        let after = mine[x + 1..].iter().find(|l| !blank(l)).copied().or(ctx[1]);
        before.zip(after).is_some_and(|(p, q)| together(ctx, m, p, q))
    })
}

/// Whether `m` (with the anchors `ctx` around it) has the lines `p` and `q`
/// (not blank) next to each other.
fn together(ctx: [Option<&str>; 2], m: &[&str], p: &str, q: &str) -> bool {
    let all: Vec<&str> = ctx[0].into_iter().chain(m.iter().copied()).chain(ctx[1]).collect();
    !blank(p)
        && !blank(q)
        && all
            .windows(2)
            .any(|w| content(w[0]) == content(p) && content(w[1]) == content(q))
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

/// Two lines that are the same line: equal, or the same but for the line end
/// that one of them, the last line of its file, lacks.
fn same_line(a: &str, b: &str) -> bool {
    a == b || ((!a.ends_with('\n') || !b.ends_with('\n')) && content(a) == content(b))
}

/// Every longest common subsequence of the base `b` and a side `s`, as the
/// side's line for each base line; `None` where there are more than
/// [`MAX_READINGS`]. Lines match where they are equal, or where one is its
/// file's last line without a newline and they are otherwise the same.
fn readings(b: &[&str], s: &[&str], budget: &mut Budget) -> Result<Option<Vec<Vec<Option<usize>>>>, OutOfBudget> {
    let (n, m) = (b.len(), s.len());
    budget.spend((n + 1) * (m + 1))?;
    // Lines as numbers (whole, and without the line end), so that a cell
    // compares two numbers, however long the lines.
    for x in b.iter().chain(s) {
        budget.read(x)?;
    }
    fn token<'a>(x: &[&'a str], ids: &mut HashMap<&'a str, u32>) -> Vec<(u32, u32, bool)> {
        let mut id = |l: &'a str| {
            let k = ids.len() as u32;
            *ids.entry(l).or_insert(k)
        };
        x.iter().map(|l| (id(l), id(content(l)), l.ends_with('\n'))).collect()
    }
    let mut ids: HashMap<&str, u32> = HashMap::new();
    let (tb, ts) = (token(b, &mut ids), token(s, &mut ids));
    // `same_line`: equal, or one without its line end and otherwise equal.
    let eq = |i: usize, j: usize| tb[i].0 == ts[j].0 || ((!tb[i].2 || !ts[j].2) && tb[i].1 == ts[j].1);
    let w = m + 1;
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
    // Each distinct set of matched pairs once: between two matches, base
    // lines are skipped first, then side lines.
    struct Frame {
        i: usize,
        j: usize,
        side_phase: bool,
        next: u8,
        matched: bool,
    }
    let mut out: Vec<Vec<Option<usize>>> = Vec::new();
    let mut path: Vec<(usize, usize)> = Vec::new();
    let mut stack = vec![Frame {
        i: 0,
        j: 0,
        side_phase: false,
        next: 0,
        matched: false,
    }];
    // Paths that end nowhere cost too; past this many steps, too many
    // readings to try.
    let mut steps = 8 * (MAX_READINGS + 1) * (n + m + 2);
    while let Some(f) = stack.last_mut() {
        budget.spend(1)?;
        steps = match steps.checked_sub(1) {
            Some(x) => x,
            None => return Ok(None),
        };
        let (i, j) = (f.i, f.j);
        let here = l[i * w + j];
        if here == 0 {
            let mut al = vec![None; n];
            for &(x, y) in &path {
                al[x] = Some(y);
            }
            budget.spend(n)?;
            out.push(al);
            if out.len() > MAX_READINGS {
                return Ok(None);
            }
            if stack.pop().is_some_and(|f| f.matched) {
                path.pop();
            }
            continue;
        }
        let mut child = None;
        while f.next < 3 && child.is_none() {
            let k = f.next;
            f.next += 1;
            child = match k {
                0 if i < n && j < m && eq(i, j) && l[(i + 1) * w + j + 1] + 1 == here => {
                    Some((i + 1, j + 1, false, true))
                }
                1 if !f.side_phase && i < n && l[(i + 1) * w + j] == here => Some((i + 1, j, false, false)),
                2 if j < m && l[i * w + j + 1] == here => Some((i, j + 1, true, false)),
                _ => None,
            };
        }
        match child {
            Some((ci, cj, side_phase, matched)) => {
                if matched {
                    path.push((i, j));
                }
                stack.push(Frame {
                    i: ci,
                    j: cj,
                    side_phase,
                    next: 0,
                    matched,
                });
            }
            None => {
                if stack.pop().is_some_and(|f| f.matched) {
                    path.pop();
                }
            }
        }
    }
    Ok(Some(out))
}

/// One change of a side by a reading: base lines `base` became the side's
/// lines `side`.
#[derive(Clone, Debug)]
struct Change {
    base: Range<usize>,
    side: Range<usize>,
    who: usize,
}

/// A side's changes by its reading `al` of the base's `n` lines.
fn changes(n: usize, len: usize, al: &[Option<usize>], who: usize) -> Vec<Change> {
    let mut out = Vec::new();
    let (mut pi, mut pj) = (0usize, 0usize);
    let pairs = al
        .iter()
        .enumerate()
        .filter_map(|(i, j)| j.map(|j| (i, j)))
        .chain([(n, len)]);
    for (i, j) in pairs {
        if i > pi || j > pj {
            out.push(Change {
                base: pi..i,
                side: pj..j,
                who,
            });
        }
        pi = i + 1;
        pj = j + 1;
    }
    out
}

/// Whether two changes of the two sides touch the same base lines: two
/// replacements (or deletions) of overlapping lines, an insertion strictly
/// inside the other's lines, or two insertions at one point.
fn touch(a: &Change, c: &Change) -> bool {
    match (a.base.is_empty(), c.base.is_empty()) {
        (false, false) => a.base.start < c.base.end && c.base.start < a.base.end,
        (true, false) => c.base.start < a.base.start && a.base.start < c.base.end,
        (false, true) => a.base.start < c.base.start && c.base.start < a.base.end,
        (true, true) => a.base.start == c.base.start,
    }
}

/// One placed piece of a plain merge: base lines `at` became `lines`.
struct Piece<'a> {
    at: Range<usize>,
    lines: Vec<&'a str>,
    /// Made by this side alone (one change).
    alone: Option<usize>,
}

/// The plain three-way merge of one reading of each side (`sides`: the side's
/// lines and its reading of the base `b`), by the placement rules; `None`
/// where they say conflict:
///
/// - a change only one side made is taken; the same change of the same base
///   lines on both sides once; where both sides only remove lines that touch,
///   every line either removed goes; anything else that touches is a
///   conflict;
/// - two insertions at one point: one, where one is the other or holds it as
///   a whole prefix or suffix (the longer); a conflict where one holds all
///   the other's lines (not blank) in any order, or one is blank lines only;
///   else both, `ours_first` says whose first;
/// - an insertion right at the end (or start) of the other side's
///   replacement, whose lines end (or begin) with exactly those lines, is
///   taken once; other pieces of the two sides that meet at one point and
///   share a line (a blank line too) are a conflict;
/// - a line (not blank) the result would have more often than either side,
///   from pieces of both at two places, one an insertion, is a conflict
///   ([`twice`]);
/// - everything is placed by base position; a line kept by both is the one
///   either side changed the line end of (the base's last line, given a
///   different line end by each side, is a conflict);
/// - a line after a line without its newline is a conflict.
fn plain<'a>(b: &[&'a str], sides: [(&[&'a str], &[Option<usize>]); 2], ours_first: bool) -> Option<Vec<&'a str>> {
    let n = b.len();
    let all: Vec<Change> = sides
        .iter()
        .enumerate()
        .flat_map(|(x, (s, al))| changes(n, s.len(), al, x))
        .collect();
    let lines = |c: &Change| -> &[&'a str] { &sides[c.who].0[c.side.clone()] };
    // Groups of touching changes (union-find).
    let mut root: Vec<usize> = (0..all.len()).collect();
    fn find(root: &mut [usize], mut i: usize) -> usize {
        while root[i] != i {
            root[i] = root[root[i]];
            i = root[i];
        }
        i
    }
    // Each side's changes are in base order, ours first.
    let no = all.iter().filter(|c| c.who == 0).count();
    for x in 0..no {
        let a = &all[x];
        let mut y = no + all[no..].partition_point(|c| c.base.end < a.base.start);
        while y < all.len() && all[y].base.start <= a.base.end {
            if touch(a, &all[y]) {
                let (p, q) = (find(&mut root, x), find(&mut root, y));
                root[p] = q;
            }
            y += 1;
        }
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut slot: HashMap<usize, usize> = HashMap::new();
    for x in 0..all.len() {
        let r = find(&mut root, x);
        let g = *slot.entry(r).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[g].push(x);
    }
    let mut pieces: Vec<Piece> = Vec::new();
    for g in &groups {
        let cs: Vec<&Change> = g.iter().map(|&x| &all[x]).collect();
        if let [c] = cs[..] {
            pieces.push(Piece {
                at: c.base.clone(),
                lines: lines(c).to_vec(),
                alone: Some(c.who),
            });
            continue;
        }
        let start = cs.iter().map(|c| c.base.start).min().expect("a change");
        let end = cs.iter().map(|c| c.base.end).max().expect("a change");
        if cs.len() == 2 && cs.iter().all(|c| c.base.is_empty()) {
            let run = |who: usize| lines(cs.iter().find(|c| c.who == who).expect("one each"));
            let (first, second) = if ours_first { (run(0), run(1)) } else { (run(1), run(0)) };
            pieces.push(Piece {
                at: start..start,
                lines: runs(first, second)?,
                alone: None,
            });
            continue;
        }
        let lines = if cs.iter().all(|c| c.side.is_empty()) {
            (start..end)
                .filter(|p| !cs.iter().any(|c| c.base.contains(p)))
                .map(|p| b[p])
                .collect()
        } else if let [c, d] = cs[..] {
            // The same change of the same lines, once.
            let (x, y) = (lines(c), lines(d));
            if c.base != d.base || x.len() != y.len() || !x.iter().zip(y).all(|(p, q)| same_line(p, q)) {
                return None;
            }
            x.iter().zip(y).map(|(p, q)| pick(p, q)).collect()
        } else {
            return None;
        };
        pieces.push(Piece {
            at: start..end,
            lines,
            alone: None,
        });
    }
    // An insertion goes before a change starting at its point.
    pieces.sort_by_key(|p| (p.at.start, p.at.end));
    // Seams: pieces that meet at one base point.
    let mut gone = vec![false; pieces.len()];
    for x in 1..pieces.len() {
        if pieces[x - 1].at.end != pieces[x].at.start || gone[x - 1] {
            continue;
        }
        let (u, v) = (&pieces[x - 1], &pieces[x]);
        if let (Some(su), Some(sv)) = (u.alone, v.alone) {
            if su != sv && (u.at.is_empty() != v.at.is_empty()) {
                // An insertion at the near end of the other's replacement,
                // which ends (or begins) with exactly its lines.
                let (ins, rep, at_end) = if v.at.is_empty() {
                    (x, x - 1, true)
                } else {
                    (x - 1, x, false)
                };
                let (i, r) = (&pieces[ins].lines, &pieces[rep].lines);
                let held = !i.is_empty()
                    && i.len() <= r.len()
                    && if at_end {
                        r[r.len() - i.len()..].iter().zip(i).all(|(a, c)| same_line(a, c))
                    } else {
                        r.iter().zip(i).all(|(a, c)| same_line(a, c))
                    };
                if held {
                    let k = if at_end { r.len() - i.len() } else { 0 };
                    let take: Vec<&str> = i.iter().enumerate().map(|(y, l)| pick(r[k + y], l)).collect();
                    pieces[rep].lines.splice(k..k + take.len(), take);
                    pieces[rep].alone = None;
                    gone[ins] = true;
                    continue;
                }
            }
        }
        let (u, v) = (&pieces[x - 1], &pieces[x]);
        // Sharing a line (a blank line too: which break is whose cannot be
        // told either).
        let mine: HashSet<&str> = u.lines.iter().map(|l| content(l)).collect();
        if v.lines.iter().any(|l| mine.contains(content(l))) {
            return None;
        }
    }
    let mut out: Vec<&'a str> = Vec::new();
    // For each line of `out`, the piece it came from (`None`: a kept line).
    let mut from: Vec<Option<usize>> = Vec::new();
    let mut p = 0;
    // A line both kept: the copy whose line end a side changed. Where the
    // base's last line has none and each side gave it a different one, which
    // is the merge's is open.
    let kept = |i: usize| -> Option<&'a str> {
        let (Some(j), Some(k)) = (sides[0].1[i], sides[1].1[i]) else {
            return Some(b[i]);
        };
        let (lo, lt) = (sides[0].0[j], sides[1].0[k]);
        if b[i].ends_with('\n') {
            // A side that dropped the line end (its last line) changed it.
            Some(if !lo.ends_with('\n') { lo } else { lt })
        } else if lo.ends_with('\n') && lt.ends_with('\n') && lo != lt {
            None
        } else {
            Some(if lo.ends_with('\n') { lo } else { lt })
        }
    };
    for (x, piece) in pieces.iter().enumerate() {
        if gone[x] {
            continue;
        }
        for i in p..piece.at.start {
            out.push(kept(i)?);
            from.push(None);
        }
        out.extend(piece.lines.iter().copied());
        from.extend(piece.lines.iter().map(|_| Some(x)));
        p = piece.at.end;
    }
    for i in p..n {
        out.push(kept(i)?);
        from.push(None);
    }
    if out.iter().rev().skip(1).any(|l| !l.ends_with('\n')) {
        return None;
    }
    if twice(&pieces, sides.map(|s| s.0), &out, &from) {
        return None;
    }
    Some(out)
}

/// Whether the plain merge `out` of a stretch (with `from`, the piece of each
/// of its lines) has a line (not blank) placed by pieces of the two sides at
/// two places, one of them an insertion (or a change that put in more lines
/// than it took out), where it is more often than in either side's stretch,
/// or where only blank lines are between the two copies (a side that removed
/// another copy elsewhere in the stretch hides the count, not the place).
/// Both sides added that line; one reading puts the two at different points
/// (with blank lines, lines a side deleted or a kept line between), another at
/// one point, where they are one insertion taken once: which is whose no text
/// shows. Two runs at one point share a place and stay whole; two different
/// lines both edited to the same text (one line for one) are no insertion.
fn twice(pieces: &[Piece], [o, t]: [&[&str]; 2], out: &[&str], from: &[Option<usize>]) -> bool {
    fn count<'a>(x: &[&'a str]) -> HashMap<&'a str, usize> {
        let mut n: HashMap<&str, usize> = HashMap::new();
        for l in x.iter().filter(|l| !blank(l)) {
            *n.entry(content(l)).or_default() += 1;
        }
        n
    }
    let (no, nt, nm) = (count(o), count(t), count(out));
    let get = |n: &HashMap<&str, usize>, l: &str| n.get(l).copied().unwrap_or(0);
    // The pieces each line that is there too often came from.
    let mut at: HashMap<&str, Vec<usize>> = HashMap::new();
    for (l, f) in out.iter().zip(from) {
        if let Some(x) = *f {
            let c = content(l);
            if !blank(l) && get(&nm, c) > get(&no, c).max(get(&nt, c)) {
                let v = at.entry(c).or_default();
                if !v.contains(&x) {
                    v.push(x);
                }
            }
        }
    }
    let one_side = |p: &Piece, q: &Piece| p.alone.is_some() && p.alone == q.alone;
    // A piece that puts in more lines than it takes out holds an insertion.
    let adds = |p: &Piece| p.lines.len() > p.at.len();
    let apart = |x: usize, y: usize| {
        let (u, v) = (&pieces[x], &pieces[y]);
        x != y && !one_side(u, v) && (adds(u) || adds(v))
    };
    // Each line from a piece against the nearest line before it that is not
    // blank.
    let mut last: Option<(&str, Option<usize>)> = None;
    for (l, f) in out.iter().zip(from) {
        if blank(l) {
            continue;
        }
        if let (Some((p, Some(x))), Some(y)) = (last, *f) {
            if p == content(l) && apart(x, y) {
                return true;
            }
        }
        last = Some((content(l), *f));
    }
    at.values().any(|ps| {
        ps.iter().enumerate().any(|(k, &x)| {
            ps[k + 1..].iter().any(|&y| {
                let (u, v) = (&pieces[x], &pieces[y]);
                !one_side(u, v) && (adds(u) || adds(v))
            })
        })
    })
}

/// Of two lines that are the same line, the one with its line end.
fn pick<'a>(a: &'a str, c: &'a str) -> &'a str {
    if a.ends_with('\n') {
        a
    } else {
        c
    }
}

/// Two runs inserted at one point, `first` first (see [`plain`]).
fn runs<'a>(first: &[&'a str], second: &[&'a str]) -> Option<Vec<&'a str>> {
    let held = |long: &[&'a str], short: &[&'a str]| -> Option<Vec<&'a str>> {
        if short.len() > long.len() {
            return None;
        }
        let k = long.len() - short.len();
        let at = |o: usize| long[o..o + short.len()].iter().zip(short).all(|(a, c)| same_line(a, c));
        let o = if at(0) {
            0
        } else if at(k) {
            k
        } else {
            return None;
        };
        let mut v = long.to_vec();
        for (y, l) in short.iter().enumerate() {
            v[o + y] = pick(long[o + y], l);
        }
        Some(v)
    };
    if let Some(v) = held(second, first).or_else(|| held(first, second)) {
        return Some(v);
    }
    let among = |long: &[&str], short: &[&str]| {
        let mut n: HashMap<&str, isize> = HashMap::new();
        for l in long.iter().filter(|l| !blank(l)) {
            *n.entry(content(l)).or_default() += 1;
        }
        let mut any = false;
        for l in short.iter().filter(|l| !blank(l)) {
            any = true;
            let k = n.entry(content(l)).or_default();
            *k -= 1;
            if *k < 0 {
                return false;
            }
        }
        any
    };
    let blank_only = |x: &[&str]| x.iter().all(|l| blank(l));
    if among(first, second) || among(second, first) || blank_only(first) || blank_only(second) {
        return None;
    }
    Some(first.iter().chain(second).copied().collect())
}

/// The view of a merge that failed the check, gap by gap.
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
