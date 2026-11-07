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
/// text: their line numbers in each, the longest run in the same order in all
/// three.
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
            (e.iter().all(|x| x.0 == 1) && t[1][e[1].1] == *l && t[2][e[2].1] == *l).then_some([i, e[1].1, e[2].1])
        })
        .collect();
    // The longest chain rising in all three (quadratic: test inputs are small).
    let k = cand.len();
    let (mut best, mut prev) = (vec![1usize; k], vec![usize::MAX; k]);
    for i in 0..k {
        for j in 0..i {
            if cand[j][1] < cand[i][1] && cand[j][2] < cand[i][2] && best[j] + 1 > best[i] {
                best[i] = best[j] + 1;
                prev[i] = j;
            }
        }
    }
    let mut out = Vec::new();
    let mut cur = (0..k).max_by_key(|&i| best[i]);
    while let Some(i) = cur {
        out.push(cand[i]);
        cur = (prev[i] != usize::MAX).then_some(prev[i]);
    }
    out.reverse();
    out
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

/// The checks that know nothing of the merge's alignment, for a clean line
/// merge `merged` of `base`, `ours` and `theirs`:
///
/// - each anchor (a line, not blank, each of the three has once with the same
///   text, in an order all three keep) is in the merge once, in that order;
/// - between anchors, where one side's stretch is the base's, the merge's is
///   the other side's (where both sides' are the same, that), byte for byte;
/// - no line is there more often than both sides have it together;
/// - where both sides changed a stretch differently, and no line moved
///   between stretches: each side's lines it has once (and the base and the
///   other side at most once) keep that side's order.
pub fn independent(base: &str, ours: &str, theirs: &str, merged: &str) -> Result<(), String> {
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
    // No line more often than both sides have it together.
    let count = |x: &[&str]| {
        let mut n: HashMap<String, usize> = HashMap::new();
        for l in x.iter().filter(|l| !blank(l)) {
            *n.entry(content(l).to_string()).or_default() += 1;
        }
        n
    };
    let (no, nt, nm) = (count(&o), count(&t), count(&m));
    for (l, &k) in &nm {
        if k > no.get(l).copied().unwrap_or(0) + nt.get(l).copied().unwrap_or(0) {
            return Err(format!("{l:?} is there {k}x, more than both sides together"));
        }
    }
    let gaps = stretches([&b, &o, &t], &an);
    // A line a side has more often than the base in one stretch and less
    // often in another may have moved between them: order is not judged.
    let mut per: HashMap<String, Vec<[usize; 3]>> = HashMap::new();
    for (g, r) in gaps.iter().enumerate() {
        for (k, text) in [&b, &o, &t].into_iter().enumerate() {
            for l in text[r[k].clone()].iter().filter(|l| !blank(l)) {
                per.entry(content(l).to_string())
                    .or_insert_with(|| vec![[0; 3]; gaps.len()])[g][k] += 1;
            }
        }
    }
    let moved = per.values().any(|v| {
        [1, 2]
            .iter()
            .any(|&k| v.iter().any(|c| c[k] > c[0]) && v.iter().any(|c| c[k] < c[0]))
    });
    for (g, [rb, ro, rt]) in gaps.into_iter().enumerate() {
        let (sb, so, st) = (&b[rb], &o[ro], &t[rt]);
        let rm = if g == 0 { 0 } else { at_m[g - 1] + 1 }..if g == an.len() { m.len() } else { at_m[g] };
        let sm = &m[rm];
        let want = if so == sb {
            Some(st)
        } else if st == sb || so == st {
            Some(so)
        } else {
            None
        };
        // A side that only added the newline to the base's last line is
        // judged as the merge's own checks judge it, not here.
        let newline_only = |x: &[&str]| {
            sb.last().is_some_and(|l| !l.ends_with('\n'))
                && x.len() == sb.len()
                && x.iter().zip(sb.iter()).all(|(a, c)| content(a) == content(c))
        };
        match want {
            Some(w) if w != sm => {
                return Err(format!(
                    "stretch {g}: one side changed it, but the merge has {sm:?}, not {w:?}"
                ));
            }
            Some(_) => {}
            None if newline_only(so) || newline_only(st) => {}
            None if moved => {}
            None => contested(sb, so, st, sm).map_err(|e| format!("stretch {g}: {e}"))?,
        }
    }
    Ok(())
}

/// The checks of [`independent`] for a stretch both sides changed.
fn contested(b: &[&str], o: &[&str], t: &[&str], m: &[&str]) -> Result<(), String> {
    let count = |x: &[&str]| {
        let mut n: HashMap<String, usize> = HashMap::new();
        for l in x.iter().filter(|l| !blank(l)) {
            *n.entry(content(l).to_string()).or_default() += 1;
        }
        n
    };
    let (nb, no, nt, nm) = (count(b), count(o), count(t), count(m));
    let get = |n: &HashMap<String, usize>, l: &str| n.get(l).copied().unwrap_or(0);
    // A line the side has once is its own in the merge where neither the
    // base nor the other side has it more than once.
    for (name, side, n, other) in [("ours", o, &no, &nt), ("theirs", t, &nt, &no)] {
        let mut order: Vec<(usize, usize)> = Vec::new();
        for (i, l) in side.iter().enumerate().filter(|(_, l)| !blank(l)) {
            let c = content(l);
            if n[c] == 1 && get(&nm, c) == 1 && get(&nb, c) <= 1 && get(other, c) <= 1 {
                order.push((i, m.iter().position(|x| content(x) == c).expect("counted")));
            }
        }
        if order.windows(2).any(|w| w[0].1 > w[1].1) {
            return Err(format!("{name}'s lines out of their order"));
        }
    }
    Ok(())
}

/// The longest common subsequences of `b` and `s` that match lines as early,
/// or as late, as they can: for each base line, the side's line that keeps it.
/// The base's last line without its newline is the side's line with one.
fn readings(b: &[&str], s: &[&str]) -> Vec<Vec<Option<usize>>> {
    let (n, m) = (b.len(), s.len());
    let eq =
        |i: usize, j: usize| b[i] == s[j] || (i + 1 == n && !b[i].ends_with('\n') && content(b[i]) == content(s[j]));
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
    let mut out: Vec<Vec<Option<usize>>> = Vec::new();
    for way in 0..4 {
        let (mut i, mut j) = (0, 0);
        let mut al = vec![None; n];
        while i < n && j < m {
            let here = l[i * w + j];
            let (skip_b, skip_s) = (l[(i + 1) * w + j] == here, l[i * w + j + 1] == here);
            let take = eq(i, j) && l[(i + 1) * w + j + 1] + 1 == here;
            let step = match way {
                0 if take => 0,
                0 if skip_b => 1,
                0 => 2,
                1 if take => 0,
                1 if skip_s => 2,
                1 => 1,
                2 if skip_b => 1,
                2 if skip_s => 2,
                3 if skip_s => 2,
                3 if skip_b => 1,
                _ => 0,
            };
            match step {
                0 => {
                    al[i] = Some(j);
                    i += 1;
                    j += 1;
                }
                1 => i += 1,
                _ => j += 1,
            }
        }
        if !out.contains(&al) {
            out.push(al);
        }
    }
    out
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

/// The plain three-way merge of one reading: changes of the two sides placed
/// by base position; the same change once; two insertions at one point once
/// where one is the other or a whole prefix or suffix of it, else whole, the
/// older first; an insertion at the edge of the other side's replacement,
/// sharing no line with it, beside it. Anything else that touches: `None`.
fn plain_merge<'a>(b: &[&'a str], sides: [(&[&'a str], &[Option<usize>]); 2], ours_first: bool) -> Option<String> {
    let n = b.len();
    let mut all: Vec<(Range<usize>, Vec<&str>, usize)> = Vec::new();
    for (x, (s, al)) in sides.iter().enumerate() {
        all.extend(changes(n, s, al).into_iter().map(|(r, l)| (r, l, x)));
    }
    all.sort_by_key(|c| (c.0.start, c.0.end, c.2));
    let edge = |ins: &(Range<usize>, Vec<&str>, usize), rep: &(Range<usize>, Vec<&str>, usize)| {
        ins.0.is_empty()
            && !rep.0.is_empty()
            && !rep.1.is_empty()
            && (ins.0.start == rep.0.start || ins.0.start == rep.0.end)
            && !ins.1.iter().any(|l| rep.1.iter().any(|r| content(r) == content(l)))
    };
    let mut out: Vec<&str> = Vec::new();
    let (mut p, mut k) = (0, 0);
    while k < all.len() {
        let c = &all[k];
        let next = all.get(k + 1).filter(|d| d.0.start <= c.0.end && d.2 != c.2);
        let Some(d) = next else {
            out.extend(&b[p..c.0.start]);
            out.extend(c.1.iter());
            p = c.0.end;
            k += 1;
            continue;
        };
        if all.get(k + 2).is_some_and(|e| e.0.start <= c.0.end.max(d.0.end)) {
            return None;
        }
        out.extend(&b[p..c.0.start.min(d.0.start)]);
        if c.0 == d.0 && c.1 == d.1 {
            out.extend(c.1.iter());
        } else if c.0.is_empty() && d.0.is_empty() {
            let (f, s) = if (c.2 == 0) == ours_first {
                (&c.1, &d.1)
            } else {
                (&d.1, &c.1)
            };
            let held = |long: &Vec<&str>, short: &Vec<&str>| long.starts_with(short) || long.ends_with(short);
            if held(f, s) {
                out.extend(f.iter());
            } else if held(s, f) {
                out.extend(s.iter());
            } else {
                out.extend(f.iter().chain(s.iter()));
            }
        } else if edge(c, d) || edge(d, c) {
            let (ins, rep) = if c.0.is_empty() { (c, d) } else { (d, c) };
            if ins.0.start == rep.0.start {
                out.extend(ins.1.iter().chain(rep.1.iter()));
            } else {
                out.extend(rep.1.iter().chain(ins.1.iter()));
            }
        } else {
            return None;
        }
        p = c.0.end.max(d.0.end);
        k += 2;
    }
    out.extend(&b[p..]);
    // The base's last line without its newline, kept by both: with one where
    // a side has it.
    if let (Some(last), Some(Some(j)), Some(Some(k))) = (b.last(), sides[0].1.last(), sides[1].1.last()) {
        if !last.ends_with('\n') {
            let pick = if sides[0].0[*j].ends_with('\n') {
                sides[0].0[*j]
            } else {
                sides[1].0[*k]
            };
            if let Some(x) = out.iter().rposition(|l| std::ptr::eq(*l, *last)) {
                out[x] = pick;
            }
        }
    }
    if out.iter().rev().skip(1).any(|l| !l.ends_with('\n')) {
        return None;
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
/// - the texts read as a stale writer's: under some longest common
///   subsequence of each side with the base, the two sides' changes do not
///   touch (or one is an insertion beside the other's edit, sharing no line),
///   and their plain three-way merge is the merge, up to blank lines in a
///   row; and no other such reading merges to anything else;
/// - L restores lines R2 removed (or a paragraph break R2 removed), in place
///   (between the lines the base has around it): the guard undoing a revert of
///   a change that arrived moments before is the false positive D10 accepts.
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
    let mut stale = (false, true);
    for ao in readings(&b, &o) {
        for at in readings(&b, &t) {
            if let Some(r) = plain_merge(&b, [(&o, &ao), (&t, &at)], l_first) {
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
            sl[i].0 == sm[i].0
                || (sl[i].0
                    && !sm[i].0
                    && pair(&pb, &sl[i - 1].1, &sl[i].1, true)
                    && pair(&pr, &sl[i - 1].1, &sl[i].1, false))
        })
    {
        return Ok(());
    }
    Err(format!(
        "the merge is not the writer's file, nor what the texts force: {merged:?}"
    ))
}
