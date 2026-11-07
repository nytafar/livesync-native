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
