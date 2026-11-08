//! An oracle for clean line merges, shared by the corpus test and the property
//! tests. It knows nothing of how the merge was made: it takes each side's
//! alignment with the base (which base line each side kept, and as which of
//! its lines) and, for each merged line, the line it claims to be, and checks
//! that against the rules:
//!
//! - every line is the line it claims to be, and the text is those lines;
//! - every line either side added is there once, every base line both sides
//!   kept is there once, and no base line a side deleted comes back;
//! - each side's lines keep that side's order;
//! - lines of the two sides follow base positions: a piece of one side that
//!   ends at or before the point where a piece of the other side starts comes
//!   first (two insertions at one point excepted);
//! - no piece of one side was merged into a piece of the other that touches the
//!   same base lines (overlaps stay open, or are the same on both sides);
//! - no line both sides inserted beside each other is there twice, except two
//!   runs inserted at one point that are not the same, nor one a whole prefix
//!   or suffix of the other: those go in whole, the older side's first;
//! - no line follows a line without its newline.
//!
//! Those rules take each side's alignment from the merge, so a merge that
//! went wrong because a diff aligned repeated lines badly (a line both sides
//! inserted, put at two points; a line a side deleted, kept as another copy)
//! can pass them. [`independent`] checks what no alignment enters: lines each
//! file has once are the same line in all of them, and between those, a
//! stretch only one side changed must come out as that side's. [`guard`]
//! judges a lost-update guard's merge, where the writer's file holds the
//! other revision, against the writer's file.
#![allow(dead_code)]

use livesync_merge::{LineTrace, Src};
use std::collections::HashMap;
use std::ops::Range;

fn content(l: &str) -> &str {
    l.trim_end_matches(['\n', '\r'])
}

/// The same inserted line, up to a missing final line end.
fn same_line(a: &str, b: &str) -> bool {
    a == b || ((!a.ends_with('\n') || !b.ends_with('\n')) && content(a) == content(b))
}

/// `short` is `long`, or a whole prefix or suffix of it.
fn holds(long: &[&str], short: &[&str]) -> bool {
    let eq = |x: &[&str]| x.iter().zip(short).all(|(a, b)| same_line(a, b));
    short.len() <= long.len() && (eq(&long[..short.len()]) || eq(&long[long.len() - short.len()..]))
}

#[derive(Clone, Debug)]
struct Change {
    base: Range<usize>,
    side: Range<usize>,
}

impl Change {
    fn inserts(&self) -> bool {
        self.base.is_empty()
    }
    /// Touch the same base lines: overlap, or an insertion strictly inside.
    fn interferes(&self, o: &Change) -> bool {
        match (self.inserts(), o.inserts()) {
            (false, false) => self.base.start < o.base.end && o.base.start < self.base.end,
            (true, false) => o.base.start < self.base.start && self.base.start < o.base.end,
            (false, true) => self.base.start < o.base.start && o.base.start < self.base.end,
            (true, true) => false,
        }
    }
    /// Meet or overlap, ends included.
    fn touches(&self, o: &Change) -> bool {
        self.base.start <= o.base.end && o.base.start <= self.base.end
    }
}

struct Side<'a> {
    lines: Vec<&'a str>,
    /// Base line -> this side's line.
    of_base: Vec<Option<usize>>,
    /// This side's line -> base line.
    base_of: Vec<Option<usize>>,
    /// This side's added line -> its change.
    change_of: Vec<Option<usize>>,
    changes: Vec<Change>,
}

fn side<'a>(name: &str, base: &[&str], text: &'a str, align: &[Option<usize>]) -> Result<Side<'a>, String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if align.len() != base.len() {
        return Err(format!(
            "{name}: alignment of {} lines for a base of {}",
            align.len(),
            base.len()
        ));
    }
    let mut base_of = vec![None; lines.len()];
    let mut last: Option<usize> = None;
    let mut pairs = Vec::new();
    for (i, a) in align.iter().enumerate() {
        let Some(j) = *a else { continue };
        if j >= lines.len() || last.is_some_and(|l| j <= l) {
            return Err(format!("{name}: alignment not increasing at base line {i}"));
        }
        // The base's last line without its newline is kept by a side that
        // only added one.
        let unterminated = i + 1 == base.len() && !base[i].ends_with('\n') && content(lines[j]) == content(base[i]);
        if lines[j] != base[i] && !unterminated {
            return Err(format!("{name}: line {j} is aligned with base line {i} but differs"));
        }
        base_of[j] = Some(i);
        last = Some(j);
        pairs.push((i, j));
    }
    let mut changes = Vec::new();
    let mut change_of = vec![None; lines.len()];
    let (mut pi, mut pj) = (0usize, 0usize);
    for (i, j) in pairs.iter().copied().chain([(base.len(), lines.len())]) {
        if i > pi || j > pj {
            for c in change_of.iter_mut().take(j).skip(pj) {
                *c = Some(changes.len());
            }
            changes.push(Change {
                base: pi..i,
                side: pj..j,
            });
        }
        pi = i + 1;
        pj = j + 1;
    }
    Ok(Side {
        lines,
        of_base: align.to_vec(),
        base_of,
        change_of,
        changes,
    })
}

/// Check a clean line merge's trace against the rules (see the module docs).
/// `ours_first`: ours is the older side (or the mtimes are equal).
pub fn check(base: &str, ours: &str, theirs: &str, ours_first: bool, t: &LineTrace) -> Result<(), String> {
    let Some(merged) = &t.merged else {
        return Ok(());
    };
    let b: Vec<&str> = base.split_inclusive('\n').collect();
    let s = [side("ours", &b, ours, &t.ours)?, side("theirs", &b, theirs, &t.theirs)?];

    // Each line is what it claims, and the text is those lines.
    let mut text = String::new();
    for (n, l) in merged.iter().enumerate() {
        let line = match *l {
            Src::Base(i) => *b.get(i).ok_or(format!("line {n}: no base line {i}"))?,
            Src::Ours(j) => *s[0].lines.get(j).ok_or(format!("line {n}: no ours line {j}"))?,
            Src::Theirs(k) => *s[1].lines.get(k).ok_or(format!("line {n}: no theirs line {k}"))?,
            Src::Both(j, k) => {
                let (x, y) = (
                    *s[0].lines.get(j).ok_or(format!("line {n}: no ours line {j}"))?,
                    *s[1].lines.get(k).ok_or(format!("line {n}: no theirs line {k}"))?,
                );
                if !same_line(x, y) {
                    return Err(format!("line {n}: ours {j} {x:?} and theirs {k} {y:?} taken as one"));
                }
                if x.ends_with('\n') {
                    x
                } else {
                    y
                }
            }
        };
        if !text.is_empty() && !text.ends_with('\n') {
            return Err(format!("line {n} {line:?} glued to the line before"));
        }
        text.push_str(line);
    }
    if t.text.as_deref() != Some(text.as_str()) {
        return Err(format!("the text is not its lines: {:?} vs {text:?}", t.text));
    }

    // Each side's line for each merged line, where it has one.
    let at = |l: &Src, x: usize| -> Option<usize> {
        match (*l, x) {
            (Src::Base(i), _) => s[x].of_base[i],
            (Src::Ours(j), 0) | (Src::Both(j, _), 0) => Some(j),
            (Src::Theirs(k), 1) | (Src::Both(_, k), 1) => Some(k),
            _ => None,
        }
    };

    // Coverage.
    for (x, name) in [(0usize, "ours"), (1, "theirs")] {
        let mut used = vec![false; s[x].lines.len()];
        for (n, l) in merged.iter().enumerate() {
            match *l {
                Src::Base(i) if s[0].of_base[i].is_none() || s[1].of_base[i].is_none() => {
                    return Err(format!("line {n}: base line {i} {:?} was deleted by a side", b[i]));
                }
                Src::Ours(j) if x == 0 && s[0].base_of[j].is_some() => {
                    return Err(format!("line {n}: ours' base line {j} taken alone"));
                }
                Src::Theirs(k) if x == 1 && s[1].base_of[k].is_some() => {
                    return Err(format!("line {n}: theirs' base line {k} taken alone"));
                }
                _ => {}
            }
            if let Some(j) = at(l, x) {
                if std::mem::replace(&mut used[j], true) {
                    return Err(format!("line {n}: {name} line {j} {:?} is there twice", s[x].lines[j]));
                }
            }
        }
        for (j, u) in used.iter().enumerate() {
            let kept_by_both = s[x].base_of[j].is_some_and(|i| s[1 - x].of_base[i].is_some());
            let added = s[x].base_of[j].is_none();
            if !u && (added || kept_by_both) {
                return Err(format!("{name} line {j} {:?} is missing", s[x].lines[j]));
            }
        }
        // The side's order.
        let order: Vec<usize> = merged.iter().filter_map(|l| at(l, x)).collect();
        if order.windows(2).any(|w| w[0] >= w[1]) {
            return Err(format!("{name}'s lines out of their order: {order:?}"));
        }
    }

    // Lines of one side alone, with their change.
    let alone: Vec<(usize, usize, usize, &str)> = merged
        .iter()
        .enumerate()
        .filter_map(|(n, l)| match *l {
            Src::Ours(j) => Some((n, 0, s[0].change_of[j]?, s[0].lines[j])),
            Src::Theirs(k) => Some((n, 1, s[1].change_of[k]?, s[1].lines[k])),
            _ => None,
        })
        .collect();
    let runs_whole = |co: &Change, ct: &Change| {
        let (ro, rt) = (&s[0].lines[co.side.clone()], &s[1].lines[ct.side.clone()]);
        !holds(ro, rt) && !holds(rt, ro)
    };
    for &(n, x, cx, lx) in &alone {
        for &(m, y, cy, ly) in &alone {
            if x != 0 || y != 1 {
                continue;
            }
            let (co, ct) = (&s[0].changes[cx], &s[1].changes[cy]);
            if co.interferes(ct) {
                return Err(format!(
                    "ours' change of base {:?} and theirs' of {:?} overlap, but merged",
                    co.base, ct.base
                ));
            }
            let at_one_point = co.inserts() && ct.inserts() && co.base.start == ct.base.start;
            if at_one_point {
                if !runs_whole(co, ct) && same_line(lx, ly) {
                    return Err(format!(
                        "{lx:?} inserted by both at base {} is there twice",
                        co.base.start
                    ));
                }
                continue;
            }
            if co.touches(ct) && content(lx) == content(ly) {
                return Err(format!(
                    "{lx:?} of ours' change of base {:?} and theirs' of {:?}, side by side, is there twice",
                    co.base, ct.base
                ));
            }
            if co.base.end <= ct.base.start && n > m {
                return Err(format!(
                    "theirs' {ly:?} (base {:?}) before ours' {lx:?} (base {:?})",
                    ct.base, co.base
                ));
            }
            if ct.base.end <= co.base.start && m > n {
                return Err(format!(
                    "ours' {lx:?} (base {:?}) before theirs' {ly:?} (base {:?})",
                    co.base, ct.base
                ));
            }
        }
    }

    // Two runs at one point that go in whole: each in one block, older first.
    let mut blocks: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
    for &(n, x, c, _) in &alone {
        blocks.entry((x, c)).or_default().push(n);
    }
    for (co_i, co) in s[0].changes.iter().enumerate() {
        for (ct_i, ct) in s[1].changes.iter().enumerate() {
            if !(co.inserts() && ct.inserts() && co.base.start == ct.base.start) || !runs_whole(co, ct) {
                continue;
            }
            let (po, pt) = (blocks.get(&(0, co_i)), blocks.get(&(1, ct_i)));
            let (Some(po), Some(pt)) = (po, pt) else {
                return Err(format!("two runs at base {} not both whole", co.base.start));
            };
            let whole = |p: &Vec<usize>, c: &Change| p.len() == c.side.len() && p.windows(2).all(|w| w[1] == w[0] + 1);
            if !whole(po, co) || !whole(pt, ct) {
                return Err(format!("a run at base {} is not in one block", co.base.start));
            }
            let (older, newer) = if ours_first { (po, pt) } else { (pt, po) };
            if older.last() > newer.first() {
                return Err(format!("the newer run at base {} goes first", co.base.start));
            }
        }
    }
    Ok(())
}

/// Whether the two sides inserted runs at one base point that share a line
/// but neither equals the other nor holds it as a whole prefix or suffix:
/// those go in whole, so the shared lines are there twice by design.
pub fn shares_lines_in_whole_runs(base: &str, ours: &str, theirs: &str, t: &LineTrace) -> bool {
    let b: Vec<&str> = base.split_inclusive('\n').collect();
    let (Ok(o), Ok(th)) = (side("ours", &b, ours, &t.ours), side("theirs", &b, theirs, &t.theirs)) else {
        return false;
    };
    o.changes.iter().any(|co| {
        th.changes.iter().any(|ct| {
            let (ro, rt) = (&o.lines[co.side.clone()], &th.lines[ct.side.clone()]);
            co.inserts()
                && ct.inserts()
                && co.base.start == ct.base.start
                && !holds(ro, rt)
                && !holds(rt, ro)
                && ro.iter().any(|l| rt.iter().any(|m| same_line(l, m)))
        })
    })
}

fn lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

fn blank(l: &str) -> bool {
    l.trim().is_empty()
}

/// A text as Markdown reads it: its lines that are not blank, each with
/// whether a blank line stands before it (a paragraph break; not before the
/// first).
pub fn shape(s: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let mut gap = false;
    for l in s.split_inclusive('\n') {
        if blank(l) {
            gap = true;
        } else {
            out.push((gap && !out.is_empty(), content(l).to_string()));
            gap = false;
        }
    }
    out
}

/// Lines (not blank) that base, ours and theirs each have once, with the same
/// text: their line numbers in each. Of those, the ones in every longest run
/// (of lines each has once, a missing final line end aside) rising in ours'
/// order and in every one rising in theirs': where two lines swapped places,
/// neither is taken.
fn anchors(t: [&[&str]; 3]) -> Vec<[usize; 3]> {
    let mut n: HashMap<&str, [(u32, usize); 3]> = HashMap::new();
    for (k, text) in t.iter().enumerate() {
        for (i, l) in text.iter().enumerate().filter(|(_, l)| !blank(l)) {
            let e = n.entry(content(l)).or_insert([(0, 0); 3]);
            e[k] = (e[k].0 + 1, i);
        }
    }
    let cand: Vec<[usize; 3]> = t[0]
        .iter()
        .enumerate()
        .filter(|(_, l)| !blank(l))
        .filter_map(|(i, l)| {
            let e = n.get(content(l))?;
            e.iter().all(|x| x.0 == 1).then_some([i, e[1].1, e[2].1])
        })
        .collect();
    let theirs = firm(&cand, 2);
    firm(&cand, 1)
        .into_iter()
        .filter(|a| theirs.contains(a) && t[1][a[1]] == t[0][a[0]] && t[2][a[2]] == t[0][a[0]])
        .collect()
}

/// The elements of `v` in every longest run whose `k`th field rises, counted
/// by brute force (test inputs are small): the number of longest runs through
/// an element equals the number of longest runs.
fn firm(v: &[[usize; 3]], k: usize) -> Vec<[usize; 3]> {
    let n = v.len();
    // Longest run (and how many) ending at each element, and starting at it.
    let (mut end, mut ways_end) = (vec![1usize; n], vec![1u128; n]);
    for i in 0..n {
        for j in 0..i {
            if v[j][k] < v[i][k] {
                if end[j] + 1 > end[i] {
                    end[i] = end[j] + 1;
                    ways_end[i] = ways_end[j];
                } else if end[j] + 1 == end[i] {
                    ways_end[i] = ways_end[i].saturating_add(ways_end[j]);
                }
            }
        }
    }
    let (mut start, mut ways_start) = (vec![1usize; n], vec![1u128; n]);
    for i in (0..n).rev() {
        for j in i + 1..n {
            if v[i][k] < v[j][k] {
                if start[j] + 1 > start[i] {
                    start[i] = start[j] + 1;
                    ways_start[i] = ways_start[j];
                } else if start[j] + 1 == start[i] {
                    ways_start[i] = ways_start[i].saturating_add(ways_start[j]);
                }
            }
        }
    }
    let longest = end.iter().copied().max().unwrap_or(0);
    let total: u128 = (0..n).filter(|&i| end[i] == longest).map(|i| ways_end[i]).sum();
    (0..n)
        .filter(|&i| end[i] + start[i] - 1 == longest && ways_end[i].saturating_mul(ways_start[i]) == total)
        .map(|i| v[i])
        .collect()
}

/// The stretches between anchors, as (base, ours, theirs) line ranges, with
/// the anchors' lines.
fn stretches(t: [&[&str]; 3], an: &[[usize; 3]]) -> Vec<[Range<usize>; 3]> {
    (0..=an.len())
        .map(|g| {
            [0, 1, 2].map(|x| {
                let from = if g == 0 { 0 } else { an[g - 1][x] + 1 };
                let to = if g == an.len() { t[x].len() } else { an[g][x] };
                from..to
            })
        })
        .collect()
}

/// The anchors left once two stretches around an anchor are judged as one:
/// where a side has fewer blank lines than the base on one side of the anchor
/// and more on the other (in all, or of one text), or where a longest common
/// subsequence of a side's lines around the anchor (two anchors each way)
/// with the base's is as long without matching the anchor.
fn joined(t: [&[&str]; 3], an: &[[usize; 3]]) -> Vec<[usize; 3]> {
    let st = stretches(t, an);
    let blanks = |g: usize, x: usize, w: Option<&str>| {
        t[x][st[g][x].clone()]
            .iter()
            .filter(|l| blank(l) && w.is_none_or(|w| content(l) == w))
            .count() as isize
    };
    let kinds: Vec<Option<&str>> = std::iter::once(None)
        .chain(
            t.iter()
                .flat_map(|x| x.iter())
                .filter(|l| blank(l))
                .map(|l| Some(content(l))),
        )
        .collect();
    let lcs = |b: &[&str], s: &[&str], skip: Option<(usize, usize)>| -> usize {
        let mut l = vec![vec![0usize; s.len() + 1]; b.len() + 1];
        for i in (0..b.len()).rev() {
            for j in (0..s.len()).rev() {
                l[i][j] = if same_line(b[i], s[j]) && skip != Some((i, j)) {
                    l[i + 1][j + 1] + 1
                } else {
                    l[i + 1][j].max(l[i][j + 1])
                };
            }
        }
        l[0][0]
    };
    let weak = |k: usize, x: usize| {
        let (lo, hi) = (k.saturating_sub(2), (k + 3).min(an.len()));
        let (rb, rs) = (st[lo][0].start..st[hi][0].end, st[lo][x].start..st[hi][x].end);
        let (b, s) = (&t[0][rb.clone()], &t[x][rs.clone()]);
        if b == s {
            return false;
        }
        lcs(b, s, Some((an[k][0] - rb.start, an[k][x] - rs.start))) == lcs(b, s, None)
    };
    (0..an.len())
        .filter(|&k| {
            ![1, 2].iter().any(|&x| {
                weak(k, x)
                    || kinds.iter().any(|&w| {
                        let (a, c) = (
                            blanks(k, x, w) - blanks(k, 0, w),
                            blanks(k + 1, x, w) - blanks(k + 1, 0, w),
                        );
                        a * c < 0
                    })
            })
        })
        .map(|k| an[k])
        .collect()
}

/// How often each line (not blank) is in `x`, by content.
fn count(x: &[&str]) -> HashMap<String, usize> {
    let mut n: HashMap<String, usize> = HashMap::new();
    for l in x.iter().filter(|l| !blank(l)) {
        *n.entry(content(l).to_string()).or_default() += 1;
    }
    n
}

/// The checks that take nothing from the merge, for a clean line merge
/// `merged` of `base`, `ours` and `theirs` (`ours_first`: ours is the older
/// side). At least as strict as the merge's own check, written apart from it:
///
/// - each anchor (a line, not blank, each of the three has once with the same
///   text, in every longest run rising in all of them) is in the merge once,
///   in that order;
/// - a line (not blank) neither side has more often than the base is not in
///   the merge more often than in both sides; one neither side has less often,
///   not less often than in either;
/// - a line a side has more often than the base between two anchors and less
///   often between two others (it may have moved): the other side changed
///   neither how often it is in those stretches nor the lines right around it
///   where it left;
/// - between anchors, where one side's stretch is the base's, the merge's is
///   the other side's (where both sides' are the same, that), byte for byte;
/// - where both changed it differently: every pair of longest common
///   subsequences of each side's stretch with the base's merges by the
///   placement rules ([`plain_merge`]) to the merge's stretch, byte for byte;
///   no line one side moved within the stretch (removed, and has elsewhere)
///   that the other removed;
///   not one side's stretch with lines added unless the merge is that (where
///   that side's change added lines, not only deleted); no
///   base line kept by one side right beside its own change, that the other
///   side removed, where the one has every line the other added.
pub fn independent(base: &str, ours: &str, theirs: &str, merged: &str, ours_first: bool) -> Result<(), String> {
    let (b, o, t, m) = (lines(base), lines(ours), lines(theirs), lines(merged));
    let an = anchors([&b, &o, &t]);
    let mut at_m = Vec::new();
    for a in &an {
        let l = b[a[0]];
        let found: Vec<usize> = (0..m.len()).filter(|&i| content(m[i]) == content(l)).collect();
        match found[..] {
            [i] if m[i] == l && at_m.last().is_none_or(|&p| p < i) => at_m.push(i),
            _ => {
                return Err(format!(
                    "{l:?}, once in each file, is not once and in order in the merge"
                ))
            }
        }
    }
    let (nb, no, nt, nm) = (count(&b), count(&o), count(&t), count(&m));
    let get = |n: &HashMap<String, usize>, l: &str| n.get(l).copied().unwrap_or(0);
    for l in nb.keys().chain(no.keys()).chain(nt.keys()).chain(nm.keys()) {
        let (x, y, z, w) = (get(&nb, l), get(&no, l), get(&nt, l), get(&nm, l));
        if y <= x && z <= x && w > y.max(z) {
            return Err(format!(
                "{l:?} is there {w}x; neither side added it, they have {y}x and {z}x"
            ));
        }
        if y >= x && z >= x && w < y.max(z) {
            return Err(format!(
                "{l:?} is there {w}x; neither side removed it, they have {y}x and {z}x"
            ));
        }
    }
    let gaps = stretches([&b, &o, &t], &an);
    // Moves: per line, per stretch, how often each text has it.
    let mut per: HashMap<String, Vec<[usize; 3]>> = HashMap::new();
    for (g, r) in gaps.iter().enumerate() {
        for (k, text) in [&b, &o, &t].into_iter().enumerate() {
            for l in text[r[k].clone()].iter().filter(|l| !blank(l)) {
                per.entry(content(l).to_string())
                    .or_insert_with(|| vec![[0; 3]; gaps.len()])[g][k] += 1;
            }
        }
    }
    for (l, v) in &per {
        for k in [1, 2] {
            let mover = v.iter().any(|c| c[k] > c[0]) && v.iter().any(|c| c[k] < c[0]);
            if !mover {
                continue;
            }
            for (g, c) in v.iter().enumerate() {
                let other = 3 - k;
                if c[k] == c[0] {
                    continue;
                }
                // Where the other side changed the stretch: from its first
                // to its last line that differs from the base, one more each
                // way.
                let r = &gaps[g];
                let (sb, so) = (&b[r[0].clone()], &[&o, &t][other - 1][r[other].clone()]);
                let first = (0..sb.len().max(so.len())).find(|&i| sb.get(i) != so.get(i));
                let touched = first.is_some_and(|f| {
                    let back = (0..sb.len().max(so.len()))
                        .find(|&i| {
                            sb.len().checked_sub(i + 1).map(|x| sb[x]) != so.len().checked_sub(i + 1).map(|x| so[x])
                        })
                        .unwrap_or(0);
                    let last = sb.len().saturating_sub(back).max(f);
                    (f.saturating_sub(1)..(last + 1).min(sb.len())).any(|i| content(sb[i]) == l)
                });
                if c[other] != c[0] || (c[k] < c[0] && touched) {
                    return Err(format!(
                        "{l:?} may have moved through stretch {g}, where the other side changed it"
                    ));
                }
            }
        }
    }
    // Stretches are judged between the anchors left once moved blank lines
    // join two (see `joined`).
    let firm = joined([&b, &o, &t], &an);
    let at_m: Vec<usize> = an
        .iter()
        .zip(&at_m)
        .filter(|(a, _)| firm.contains(a))
        .map(|(_, &i)| i)
        .collect();
    let an = firm;
    let gaps = stretches([&b, &o, &t], &an);
    for (g, [rb, ro, rt]) in gaps.into_iter().enumerate() {
        let (sb, so, st) = (&b[rb], &o[ro], &t[rt]);
        let rm = if g == 0 { 0 } else { at_m[g - 1] + 1 }..if g == an.len() { m.len() } else { at_m[g] };
        let sm = &m[rm];
        let ctx = [(g > 0).then(|| b[an[g - 1][0]]), (g < an.len()).then(|| b[an[g][0]])];
        // A side that only added the newline to the base's last line changed
        // nothing: the other side's stretch, with that line's newline.
        let newline_only = |x: &[&str]| {
            sb.last().is_some_and(|l| !l.ends_with('\n'))
                && x.len() == sb.len()
                && x[..x.len() - 1] == sb[..sb.len() - 1]
                && content(x[x.len() - 1]) == content(sb[sb.len() - 1])
        };
        let with_newline = |x: &[&'_ str], from: &[&'_ str]| -> String {
            let mut v: Vec<&str> = x.to_vec();
            if v.last() == sb.last() {
                let n = v.len();
                v[n - 1] = from[from.len() - 1];
            }
            v.concat()
        };
        // ... unless the other side gave that line another line end.
        let clash = |x: &[&str], from: &[&str]| {
            let last = sb.last().map(|l| content(l));
            x.iter()
                .any(|l| l.ends_with('\n') && Some(content(l)) == last && *l != from[from.len() - 1])
        };
        let want = if so == sb {
            Some(st.concat())
        } else if st == sb || so == st {
            Some(so.concat())
        } else if newline_only(so) && !clash(st, so) {
            Some(with_newline(st, so))
        } else if newline_only(st) && !clash(so, st) {
            Some(with_newline(so, st))
        } else {
            None
        };
        match want {
            Some(w) if w != sm.concat() => {
                return Err(format!(
                    "stretch {g}: one side changed it, but the merge has {sm:?}, not {w:?}"
                ));
            }
            Some(_) => {}
            None => contested(ctx, sb, so, st, sm, ours_first).map_err(|e| format!("stretch {g}: {e}"))?,
        }
    }
    Ok(())
}

/// The checks of [`independent`] for a stretch both sides changed.
fn contested(
    ctx: [Option<&str>; 2],
    b: &[&str],
    o: &[&str],
    t: &[&str],
    m: &[&str],
    ours_first: bool,
) -> Result<(), String> {
    let sub = |short: &[&str], long: &[&str]| {
        let mut it = long.iter();
        short.iter().all(|s| it.any(|l| l == s))
    };
    if (!sub(t, b) && sub(t, o) && m != o) || (!sub(o, b) && sub(o, t) && m != t) {
        return Err("a side is the other's with lines added, and the merge is not that".into());
    }
    let (Some(ro), Some(rt)) = (all_readings(b, o, 64), all_readings(b, t, 64)) else {
        return Err("too many readings to judge: should be open".into());
    };
    let holds = |other: &[&str], mine: &[&str]| {
        let (nb, no, nm) = (count(b), count(other), count(mine));
        no.iter()
            .all(|(l, &k)| k <= nb.get(l).copied().unwrap_or(0) || nm.get(l).copied().unwrap_or(0) >= k)
    };
    let (ho, ht) = (holds(t, o), holds(o, t));
    for ao in &ro {
        for at in &rt {
            if (ho && typed_back(ctx, b, o, ao, at, t, m)) || (ht && typed_back(ctx, b, t, at, ao, o, m)) {
                return Err("a side keeps beside its change a line the other removed".into());
            }
            // A line one side removed and put elsewhere in the stretch, that
            // the other side removed too: a move against a deletion.
            let moved = |s: &[&str], al: &[Option<usize>], i: usize| {
                al[i].is_none()
                    && !blank(b[i])
                    && changes(b.len(), s, al)
                        .iter()
                        .any(|(_, lines)| lines.iter().any(|l| content(l) == content(b[i])))
            };
            if (0..b.len()).any(|i| ao[i].is_none() && at[i].is_none() && (moved(o, ao, i) || moved(t, at, i))) {
                return Err("a line one side moved, the other removed".into());
            }
            match plain_merge(b, [(o, ao), (t, at)], ours_first, true) {
                Some(x) if x == m.concat() => {}
                Some(x) => return Err(format!("a reading merges to {x:?}, not to the merge")),
                None => return Err("a reading conflicts".into()),
            }
        }
    }
    Ok(())
}

/// Whether `mine`, by its reading `al`, keeps a base line the `other` reading
/// of the other side's lines `theirs` deleted (removed, adding nothing but
/// blank lines) or replaced with lines `mine` inserted right there, with a
/// change of its own next to it (blank lines between do not
/// part them); a blank line only where it would part two lines `m` (with the
/// anchors `ctx` around it) has together.
fn typed_back(
    ctx: [Option<&str>; 2],
    b: &[&str],
    mine: &[&str],
    al: &[Option<usize>],
    other: &[Option<usize>],
    theirs: &[&str],
    m: &[&str],
) -> bool {
    let n = b.len();
    // The side's changes, as base ranges with whether they insert lines.
    let mut ch: Vec<(Range<usize>, bool)> = Vec::new();
    let (mut pi, mut pj) = (0usize, 0usize);
    for (i, j) in al
        .iter()
        .enumerate()
        .filter_map(|(i, j)| j.map(|j| (i, j)))
        .chain([(n, mine.len())])
    {
        if i > pi || j > pj {
            ch.push((pi..i, j > pj));
        }
        pi = i + 1;
        pj = j + 1;
    }
    let near = |i: usize| -> bool {
        let lo = (0..i).rev().find(|&j| !blank(b[j]));
        let hi = (i + 1..n).find(|&j| !blank(b[j]));
        let first = lo.unwrap_or(0);
        let last = hi.unwrap_or(n - 1);
        if (first..=last).any(|j| j != i && al[j].is_none()) {
            return true;
        }
        let (g0, g1) = (lo.map_or(0, |l| l + 1), hi.unwrap_or(n));
        ch.iter()
            .any(|(r, ins)| *ins && ((g0..=g1).contains(&r.start) || (g0..=g1).contains(&r.end)))
    };
    let all: Vec<&str> = ctx[0].into_iter().chain(m.iter().copied()).chain(ctx[1]).collect();
    // For each base line the other reading removed, the lines (not blank)
    // of the change that removed it.
    let mut cut: Vec<Option<Vec<&str>>> = vec![None; n];
    for (r, added) in changes(n, theirs, other) {
        let new: Vec<&str> = added.into_iter().filter(|l| !blank(l)).collect();
        for x in r {
            cut[x] = Some(new.clone());
        }
    }
    // Whether `mine` inserted (removing nothing) all of `new` at a gap in
    // g0..=g1.
    let inserted_here = |new: &[&str], g0: usize, g1: usize| {
        changes(n, mine, al).into_iter().any(|(r, lines)| {
            let have = count(&lines);
            r.is_empty()
                && (g0..=g1).contains(&r.start)
                && count(new).iter().all(|(k, v)| have.get(k).copied().unwrap_or(0) >= *v)
        })
    };
    (0..n).any(|i| {
        let Some(x) = al[i] else { return false };
        let Some(new) = &cut[i] else { return false };
        if !near(i) {
            return false;
        }
        if !new.is_empty() {
            let lo = (0..i).rev().find(|&j| !blank(b[j]));
            let hi = (i + 1..n).find(|&j| !blank(b[j]));
            if !inserted_here(new, lo.map_or(0, |l| l + 1), hi.unwrap_or(n)) {
                return false;
            }
        }
        if !blank(b[i]) {
            return true;
        }
        let p = mine[..x].iter().rev().find(|l| !blank(l)).copied().or(ctx[0]);
        let q = mine[x + 1..].iter().find(|l| !blank(l)).copied().or(ctx[1]);
        p.zip(q).is_some_and(|(p, q)| {
            all.windows(2)
                .any(|w| content(w[0]) == content(p) && content(w[1]) == content(q))
        })
    })
}

/// Every longest common subsequence of `b` and `s` (for each base line, the
/// side's line that keeps it), or `None` past `cap`. Lines match where equal,
/// or the same but for the newline one lacks.
fn all_readings(b: &[&str], s: &[&str], cap: usize) -> Option<Vec<Vec<Option<usize>>>> {
    let (n, m) = (b.len(), s.len());
    let mut l = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i][j] = if same_line(b[i], s[j]) {
                l[i + 1][j + 1] + 1
            } else {
                l[i + 1][j].max(l[i][j + 1])
            };
        }
    }
    // The next matched pair, every way: any (x, y) at or after (i, j) that
    // starts a longest common subsequence of what is left.
    fn rec(
        b: &[&str],
        s: &[&str],
        l: &[Vec<usize>],
        (i, j): (usize, usize),
        cur: &mut Vec<(usize, usize)>,
        out: &mut Vec<Vec<Option<usize>>>,
        cap: usize,
    ) -> bool {
        let here = l[i][j];
        if here == 0 {
            let mut al = vec![None; b.len()];
            for &(x, y) in cur.iter() {
                al[x] = Some(y);
            }
            out.push(al);
            return out.len() <= cap;
        }
        for x in i..b.len() {
            for y in j..s.len() {
                // (x, y) as the first pair of a longest one of what is left.
                if same_line(b[x], s[y]) && l[x + 1][y + 1] + 1 == here {
                    cur.push((x, y));
                    let ok = rec(b, s, l, (x + 1, y + 1), cur, out, cap);
                    cur.pop();
                    if !ok {
                        return false;
                    }
                }
            }
        }
        true
    }
    let mut out = Vec::new();
    if !rec(b, s, &l, (0, 0), &mut Vec::new(), &mut out, cap) {
        return None;
    }
    Some(out)
}

/// A side's changes by an alignment: (base range, side lines).
type Changes<'a> = Vec<(Range<usize>, Vec<&'a str>)>;

fn changes<'a>(n: usize, s: &[&'a str], al: &[Option<usize>]) -> Changes<'a> {
    let mut out = Vec::new();
    let (mut pi, mut pj) = (0usize, 0usize);
    let pairs = al
        .iter()
        .enumerate()
        .filter_map(|(i, j)| j.map(|j| (i, j)))
        .chain([(n, s.len())]);
    for (i, j) in pairs {
        if i > pi || j > pj {
            out.push((pi..i, s[pj..j].to_vec()));
        }
        pi = i + 1;
        pj = j + 1;
    }
    out
}

/// The plain three-way merge of one reading of each side, by the placement
/// rules; `None` where they say conflict:
///
/// - changes of the two sides that touch the same base lines (overlap, an
///   insertion strictly inside the other's lines, two insertions at one
///   point) are one region. One change alone is taken. Two insertions at one
///   point: one where one is the other or a whole prefix or suffix of it (the
///   longer); a conflict where one has every line (not blank) of the other, or
///   one is blank lines only; else both, the older side's first. Only
///   deletions: every line either deleted goes. The same change of the same
///   lines on both sides: once. Anything else: a conflict.
/// - regions are placed by base position, an insertion before a change at its
///   point. Where two meet at one point: an insertion of one side at the
///   near end of the other's replacement, whose lines end (or begin) with
///   exactly it, is taken once; otherwise sharing a line (a blank line too)
///   is a conflict.
///   Two insertions of the two sides with only blank lines between them may
///   not share a line that is not blank.
/// - a base line both kept is the copy whose line end a side changed (its
///   last line), or that has one where the base's lacks it (two different
///   ones: a conflict).
/// - a line after a line without its newline: a conflict.
fn plain_merge<'a>(
    b: &[&'a str],
    sides: [(&[&'a str], &[Option<usize>]); 2],
    ours_first: bool,
    one_stretch: bool,
) -> Option<String> {
    let n = b.len();
    let mut all: Vec<(Range<usize>, Vec<&str>, usize)> = Vec::new();
    for (x, (s, al)) in sides.iter().enumerate() {
        all.extend(changes(n, s, al).into_iter().map(|(r, l)| (r, l, x)));
    }
    let touch = |p: &Range<usize>, q: &Range<usize>| match (p.is_empty(), q.is_empty()) {
        (false, false) => p.start < q.end && q.start < p.end,
        (true, false) => q.start < p.start && p.start < q.end,
        (false, true) => p.start < q.start && q.start < p.end,
        (true, true) => p.start == q.start,
    };
    // Regions: changes joined while any two of different sides touch.
    let mut region: Vec<usize> = (0..all.len()).collect();
    loop {
        let mut again = false;
        for x in 0..all.len() {
            for y in 0..all.len() {
                if all[x].2 != all[y].2 && region[x] != region[y] && touch(&all[x].0, &all[y].0) {
                    let (keep, drop) = (region[x].min(region[y]), region[x].max(region[y]));
                    for r in region.iter_mut().filter(|r| **r == drop) {
                        *r = keep;
                    }
                    again = true;
                }
            }
        }
        if !again {
            break;
        }
    }
    let mut ids: Vec<usize> = region.clone();
    ids.sort();
    ids.dedup();
    // (base range, lines, the one side that made it alone)
    let mut pieces: Vec<(Range<usize>, Vec<&str>, Option<usize>)> = Vec::new();
    for id in ids {
        let mine: Vec<&(Range<usize>, Vec<&str>, usize)> =
            (0..all.len()).filter(|&x| region[x] == id).map(|x| &all[x]).collect();
        let lo = mine.iter().map(|c| c.0.start).min().unwrap();
        let hi = mine.iter().map(|c| c.0.end).max().unwrap();
        if mine.len() == 1 {
            pieces.push((mine[0].0.clone(), mine[0].1.clone(), Some(mine[0].2)));
        } else if mine.len() == 2 && mine.iter().all(|c| c.0.is_empty()) {
            let run = |s: usize| &mine.iter().find(|c| c.2 == s).unwrap().1;
            let (f, s) = if ours_first { (run(0), run(1)) } else { (run(1), run(0)) };
            let shared = |long: &Vec<&'a str>, short: &Vec<&'a str>| -> Option<Vec<&'a str>> {
                if short.len() > long.len() {
                    return None;
                }
                for at in [0, long.len() - short.len()] {
                    if long[at..at + short.len()]
                        .iter()
                        .zip(short)
                        .all(|(p, q)| same_line(p, q))
                    {
                        let mut v = long.clone();
                        for (y, q) in short.iter().enumerate() {
                            if !v[at + y].ends_with('\n') {
                                v[at + y] = q;
                            }
                        }
                        return Some(v);
                    }
                }
                None
            };
            let has_all = |long: &Vec<&str>, short: &Vec<&str>| {
                let (cl, cs) = (count(long), count(short));
                !cs.is_empty() && cs.iter().all(|(k, v)| cl.get(k).copied().unwrap_or(0) >= *v)
            };
            let v = if let Some(v) = shared(s, f).or_else(|| shared(f, s)) {
                v
            } else if has_all(f, s) || has_all(s, f) || f.iter().all(|l| blank(l)) || s.iter().all(|l| blank(l)) {
                return None;
            } else {
                f.iter().chain(s.iter()).copied().collect()
            };
            pieces.push((lo..lo, v, None));
        } else if mine.iter().all(|c| c.1.is_empty()) {
            let gone = |p: usize| mine.iter().any(|c| c.0.contains(&p));
            pieces.push((lo..hi, (lo..hi).filter(|&p| !gone(p)).map(|p| b[p]).collect(), None));
        } else if mine.len() == 2
            && mine[0].0 == mine[1].0
            && mine[0].1.len() == mine[1].1.len()
            && mine[0].1.iter().zip(&mine[1].1).all(|(p, q)| same_line(p, q))
        {
            let v = mine[0]
                .1
                .iter()
                .zip(&mine[1].1)
                .map(|(p, q)| if p.ends_with('\n') { *p } else { *q })
                .collect();
            pieces.push((lo..hi, v, None));
        } else {
            return None;
        }
    }
    pieces.sort_by_key(|p| (p.0.start, p.0.end));
    let mut k = 1;
    while k < pieces.len() {
        let (u, v) = (&pieces[k - 1], &pieces[k]);
        if u.0.end != v.0.start {
            k += 1;
            continue;
        }
        if let (Some(su), Some(sv)) = (u.2, v.2) {
            if su != sv && u.0.is_empty() != v.0.is_empty() {
                let (ins, rep) = if u.0.is_empty() { (k - 1, k) } else { (k, k - 1) };
                let (i, r) = (pieces[ins].1.clone(), pieces[rep].1.clone());
                let at_end = ins == k;
                let off = if at_end { r.len().checked_sub(i.len()) } else { Some(0) };
                if let Some(off) = off.filter(|_| !i.is_empty() && i.len() <= r.len()) {
                    if r[off..off + i.len()].iter().zip(&i).all(|(p, q)| same_line(p, q)) {
                        for (y, q) in i.iter().enumerate() {
                            if !pieces[rep].1[off + y].ends_with('\n') {
                                pieces[rep].1[off + y] = q;
                            }
                        }
                        pieces[rep].2 = None;
                        pieces.remove(ins);
                        continue;
                    }
                }
            }
        }
        let (u, v) = (&pieces[k - 1], &pieces[k]);
        if u.1.iter().any(|p| v.1.iter().any(|q| content(p) == content(q))) {
            return None;
        }
        k += 1;
    }
    // Within one stretch between anchors: a line (not blank) the merge would
    // have more often than either side, from pieces of the two sides at two
    // places, one an insertion (or a change that put in more lines than it
    // took out): one insertion read at two places.
    if one_stretch {
        let lines_of = |x: &[&str]| count(x);
        let (no, nt) = (lines_of(sides[0].0), lines_of(sides[1].0));
        let mut nm: HashMap<String, usize> = HashMap::new();
        for p in &pieces {
            for l in p.1.iter().filter(|l| !blank(l)) {
                *nm.entry(content(l).to_string()).or_default() += 1;
            }
        }
        for (i, l) in b.iter().enumerate() {
            if sides[0].1[i].is_some() && sides[1].1[i].is_some() && !blank(l) {
                *nm.entry(content(l).to_string()).or_default() += 1;
            }
        }
        let get = |n: &HashMap<String, usize>, l: &str| n.get(l).copied().unwrap_or(0);
        for (x, u) in pieces.iter().enumerate() {
            for v in &pieces[x + 1..] {
                // An insertion, or a change that put in more lines than it
                // took out.
                let one_side = u.2.is_some() && u.2 == v.2;
                let grows = |p: &(Range<usize>, Vec<&str>, Option<usize>)| p.1.len() > p.0.len();
                if one_side || !(grows(u) || grows(v)) {
                    continue;
                }
                let twice = u.1.iter().filter(|p| !blank(p)).any(|p| {
                    let c = content(p);
                    v.1.iter().any(|q| content(q) == c) && get(&nm, c) > get(&no, c).max(get(&nt, c))
                });
                if twice {
                    return None;
                }
            }
        }
    }
    let kept = |i: usize| -> Option<&str> {
        let (Some(j), Some(k)) = (sides[0].1[i], sides[1].1[i]) else {
            return Some(b[i]);
        };
        let (x, y) = (sides[0].0[j], sides[1].0[k]);
        if b[i].ends_with('\n') {
            let changed = |c: &str| !c.ends_with('\n');
            return Some(if changed(x) { x } else { y });
        }
        // The base's last line, without a line end: each side may give it
        // one; two different ones conflict.
        match (x.ends_with('\n'), y.ends_with('\n')) {
            (true, true) if x != y => None,
            (true, _) => Some(x),
            _ => Some(y),
        }
    };
    let mut out: Vec<&str> = Vec::new();
    // The piece each line came from, its side (`None`: both), and whether it
    // put in more lines than it took out (`None`: a kept line).
    let mut whose: Vec<Option<(usize, Option<usize>, bool)>> = Vec::new();
    let mut p = 0;
    for (x, (r, l, side)) in pieces.iter().enumerate() {
        for i in p..r.start {
            out.push(kept(i)?);
            whose.push(None);
        }
        out.extend(l.iter().copied());
        whose.extend(l.iter().map(|_| Some((x, *side, l.len() > r.len()))));
        p = r.end;
    }
    for i in p..n {
        out.push(kept(i)?);
        whose.push(None);
    }
    if out.iter().rev().skip(1).any(|l| !l.ends_with('\n')) {
        return None;
    }
    // Within one stretch: the same line (not blank) from pieces of the two
    // sides, one an insertion (or a change that put in more lines than it
    // took out), with only blank lines between them in the merge: one
    // insertion read at two places, whatever the counts.
    if one_stretch {
        let seen: Vec<(usize, &str)> = out
            .iter()
            .enumerate()
            .filter(|(_, l)| !blank(l))
            .map(|(i, l)| (i, content(l)))
            .collect();
        for w in seen.windows(2) {
            let ((i, a), (j, c)) = (w[0], w[1]);
            if a != c {
                continue;
            }
            if let (Some((x, sx, ix)), Some((y, sy, iy))) = (whose[i], whose[j]) {
                let one_side = sx.is_some() && sx == sy;
                if x != y && !one_side && (ix || iy) {
                    return None;
                }
            }
        }
    }
    Some(out.concat())
}

/// A lost-update guard's merge `merged` of base R1, ours L and theirs R2,
/// where L was written from R2 (it holds R2): it must be L, except where the
/// three texts cannot show that L holds R2:
///
/// - only blank lines differ, not a paragraph break (two blank lines for one,
///   or blank lines before the first line or after the last);
/// - between anchors, L's stretch is the base's (the writer's change there is
///   one no text shows, so R2's stretch is the only right merge);
/// - the texts read as a stale writer's: every pair of longest common
///   subsequences of each side with the base that merges by the placement
///   rules ([`plain_merge`]) gives the merge, up to blank lines in a row, and
///   one does; and no stretch both changed, where R2 added lines and has
///   them all in L's stretch, in order (L may be R2 with lines of its own),
///   is other than L's in the merge;
/// - L restores lines R2 removed (or a paragraph break R2 removed), in place
///   (between the lines the base has around it), or removed a paragraph
///   break R2 added: the guard undoing a revert of a change that arrived
///   moments before is the false positive D10 accepts.
pub fn guard(base: &str, l: &str, r2: &str, merged: &str, l_first: bool) -> Result<(), String> {
    let sm = shape(merged);
    if merged == l || sm == shape(l) {
        return Ok(());
    }
    // L's stretch where L changed it, R2's where L's is the base's.
    let (b, o, t) = (lines(base), lines(l), lines(r2));
    let an = anchors([&b, &o, &t]);
    let mut forced = String::new();
    for (g, [rb, ro, rt]) in stretches([&b, &o, &t], &an).into_iter().enumerate() {
        forced.push_str(&if o[ro.clone()] == b[rb] {
            t[rt].concat()
        } else {
            o[ro].concat()
        });
        if let Some(a) = an.get(g) {
            forced.push_str(b[a[0]]);
        }
    }
    if shape(&forced) == sm {
        return Ok(());
    }
    // R2's lines all in L, in order, in each stretch both changed: L may be
    // R2 with the writer's lines on top, so it reads as no stale writer.
    let in_order = |short: &[&str], long: &[&str]| {
        let mut it = long.iter();
        short.iter().all(|s| it.any(|l| l == s))
    };
    // Where each anchor is in the merge (once and in order, or no verdict).
    let m = lines(merged);
    let mut at_m = Vec::new();
    for a in &an {
        match (0..m.len()).filter(|&i| m[i] == b[a[0]]).collect::<Vec<_>>()[..] {
            [i] if at_m.last().is_none_or(|&p| p < i) => at_m.push(i),
            _ => break,
        }
    }
    let holds_r2 = at_m.len() == an.len()
        && stretches([&b, &o, &t], &an)
            .into_iter()
            .enumerate()
            .any(|(g, [rb, ro, rt])| {
                let rm = if g == 0 { 0 } else { at_m[g - 1] + 1 }..if g == an.len() { m.len() } else { at_m[g] };
                let (sb, so, st) = (&b[rb], &o[ro], &t[rt]);
                so != sb && st != sb && !in_order(st, sb) && in_order(st, so) && m[rm] != *so
            });
    let mut stale = (false, !holds_r2);
    let all = |s: &[&str]| all_readings(&b, s, 64).unwrap_or_default();
    for ao in all(&o) {
        for at in all(&t) {
            if let Some(r) = plain_merge(&b, [(&o, &ao), (&t, &at)], l_first, false) {
                let same = shape(&r) == sm;
                stale = (stale.0 || same, stale.1 && same);
            }
        }
    }
    if stale == (true, true) {
        return Ok(());
    }
    // A revert: M is L without lines (or paragraph breaks) R2 removed.
    let count = |x: &str| {
        let mut n: HashMap<String, usize> = HashMap::new();
        for p in shape(x) {
            *n.entry(p.1).or_default() += 1;
        }
        n
    };
    let (cb, cr) = (count(base), count(r2));
    let removed = |x: &str| cb.get(x).copied().unwrap_or(0) > cr.get(x).copied().unwrap_or(0);
    let sl = shape(l);
    let mut it = sl.iter();
    let mut extra = Vec::new();
    let sub = sm.iter().all(|m| loop {
        match it.next() {
            Some(x) if x.1 == m.1 => break true,
            Some(x) => extra.push(x.1.clone()),
            None => break false,
        }
    });
    extra.extend(it.map(|x| x.1.clone()));
    // Restored in place: between the same lines as in the base.
    let (pb, pr) = (shape(base), shape(r2));
    let neighbours = |v: &[(bool, String)], x: &str| -> Vec<(Option<String>, Option<String>)> {
        (0..v.len())
            .filter(|&i| v[i].1 == x)
            .map(|i| {
                (
                    i.checked_sub(1).map(|p| v[p].1.clone()),
                    v.get(i + 1).map(|n| n.1.clone()),
                )
            })
            .collect()
    };
    let in_place = |x: &str| {
        let nb = neighbours(&pb, x);
        neighbours(&sl, x).iter().any(|n| nb.contains(n))
    };
    if sub && !extra.is_empty() && extra.iter().all(|x| removed(x) && in_place(x)) {
        return Ok(());
    }
    let pair = |v: &[(bool, String)], a: &str, c: &str, gap: bool| {
        v.windows(2).any(|w| w[0].1 == a && w[1].1 == c && w[1].0 == gap)
    };
    if sl.len() == sm.len()
        && sl.iter().zip(&sm).all(|(x, y)| x.1 == y.1)
        && (1..sl.len()).all(|i| {
            let (a, c) = (&sl[i - 1].1, &sl[i].1);
            sl[i].0 == sm[i].0
                // L restored a paragraph break R2 removed,
                || (sl[i].0 && !sm[i].0 && pair(&pb, a, c, true) && pair(&pr, a, c, false))
                // or removed one R2 added.
                || (!sl[i].0 && sm[i].0 && pair(&pr, a, c, true) && !pair(&pb, a, c, true))
        })
    {
        return Ok(());
    }
    Err(format!(
        "the merge is not the writer's file, nor what the texts force: {merged:?}"
    ))
}
