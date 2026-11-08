//! Ground truth for line merges, from known edit histories: the primary
//! oracle of the property tests.
//!
//! A stream builds a base whose every line has an identity, and applies an
//! edit script to a copy of it for each side: insertions (fresh lines, copies
//! of lines, lines from a pool that repeats: blank lines, `---`, `- x`, tasks,
//! fences, headings), deletions, edits, moves, paragraph breaks, line-end and
//! final-newline changes. So what each side did to each base line is known,
//! not read from the texts: a base line the side still has (not moved) is the
//! line it kept, everything else is its change. The true merge is the
//! placement rules applied to those changes ([`rules_traced`]): never to an
//! alignment of the texts, and never to anything of the merge's.
//!
//! The rules, as the crate documents them:
//!
//! - a change only one side made is taken, in place: placed by base position;
//! - the same change of the same base lines on both sides (by text) once;
//!   where both sides only removed lines that touch, every line either
//!   removed goes; any other two changes that touch (overlapping lines, an
//!   insertion strictly inside the other side's lines) are a conflict;
//! - two insertions at one point: one where one is the other or holds it as a
//!   whole prefix or suffix (the longer); a conflict where one holds all the
//!   other's lines (not blank) in any order, or one is blank lines only;
//!   else both whole, the older side's first;
//! - an insertion right at the start or end of the other side's change, whose
//!   lines begin (or end) there with exactly the insertion's, is taken once;
//!   other pieces of the two sides that meet at one point and share a line
//!   (a blank line too) are a conflict;
//! - a line both kept is the copy whose line end a side changed (the base's
//!   last line, given a different line end by each side, is a conflict); a
//!   line after a line without its newline is a conflict.
//!
//! An edit that put back the very line it removed, in place (a line deleted
//! and typed again where it was), is no change, and a line moved back among
//! the same lines it was between is no move: no text could show either.
//!
//! A clean merge must be the true merge ([`Class::Exact`]), or fall in one of
//! these history-ambiguous classes, each checked exactly ([`judge`]). Where
//! two histories give the same three texts, no merge of texts can be right
//! for both; these are the only such differences a clean merge may show:
//!
//! - [`Class::Copies`]: which of two equal lines next to each other a side
//!   kept, removed or inserted beside. A side that inserted `- x` after a
//!   `- x` may as well have inserted it before; one that deleted one of two
//!   blank lines in a row, either. The texts are the same either way. Check:
//!   the merge is the true merge of the history with such lines swapped
//!   ([`slides`]). So the class covers both sides making the same change at
//!   places only copies part (taken once, as D9 asks), and a writer that
//!   changed one of R2's copies.
//! - [`Class::Smaller`]: the history is longer than the texts show. A side
//!   removed a line and typed the same text elsewhere, moved a line, or
//!   copied one and changed the original, so that the texts read as fewer
//!   changes: the lines it kept are not a longest common subsequence with
//!   the base ([`minimal`]). Check: the merge is the true merge of the
//!   shortest history the texts show that keeps the most of the real one's
//!   pairs ([`nearest`]), or of it with copies swapped.
//! - [`Class::Twin`]: a side moved lines past as many other lines (two
//!   blocks of the same size swapped), so a history as short moved the other
//!   ones. Check: the merge is the true merge of the history with one side
//!   read as the shortest history that keeps the fewest of the real one's
//!   pairs ([`farthest`]), or of it with copies swapped.
//! - [`Class::Same`]: both sides' texts are the same over some lines (the
//!   same change, as D9 takes once), reached by histories that differ (one
//!   moved a heading down, the other a blank line up). Check: the merge is
//!   the true merge of the history with one side read as the other wherever
//!   their texts agree ([`carried`]), or of it with copies swapped. This is
//!   review 6's settled class (a), identical stretches on both sides.
//!
//! Every class is held to the same count bounds ([`bounded`]), so none
//! takes a line twice or drops one beyond what two histories allow (order
//! needs none of its own: the readings are each side's own, and two sides'
//! moves compose):
//!
//! - no line less often than all of the true merge, either side, and the
//!   count each side's own change gives (where both removed a copy, they may
//!   be two copies; where both added one at two places, one);
//! - no line more often than the true merge and either side (where the true
//!   merge is a conflict: than either side), but for copies the rules put in
//!   by position: the same text from edits of different lines on both sides
//!   (each ticked another copy of a task), and, where the true merge is a
//!   conflict, two runs at one point that go in whole;
//! - a line over that only as often, in each segment between lines no side
//!   moved, as the true merge or either side has it there: copies a line
//!   every file has once parts are two lines (the crate's rule for a line
//!   both added at two places holds within one stretch between such lines).
//!
//! So finding 1 of review 6 (a line both added, at two places with only
//! blank or deleted lines between), and review 7's three garbles, are in no
//! class. Everything else that is clean and not the true merge is wrong.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;

/// A line with an identity, to know what really happened to it.
#[derive(Clone, Debug)]
pub struct Line {
    pub id: usize,
    pub text: String,
    pub crlf: bool,
}

/// A file: its lines, and whether the last one ends with a line end.
#[derive(Clone, Debug)]
pub struct File {
    pub lines: Vec<Line>,
    pub fin: bool,
}

impl File {
    /// Each line with its line end (the last without, unless `fin`).
    pub fn texts(&self) -> Vec<String> {
        let n = self.lines.len();
        self.lines
            .iter()
            .enumerate()
            .map(|(k, l)| {
                let end = if k + 1 < n || self.fin {
                    if l.crlf {
                        "\r\n"
                    } else {
                        "\n"
                    }
                } else {
                    ""
                };
                format!("{}{end}", l.text)
            })
            .collect()
    }

    pub fn render(&self) -> String {
        self.texts().concat()
    }

    /// A file that ends in an empty line without a final newline has no
    /// such line as text: the history and the text disagree.
    pub fn artifact(&self) -> bool {
        !self.fin && self.lines.last().is_some_and(|l| l.text.is_empty())
    }
}

/// A side's file and the identities its script moved.
#[derive(Clone, Debug)]
pub struct Edited {
    pub file: File,
    pub moved: HashSet<usize>,
}

/// For each base line, the side's line it is (kept), or `None` (changed).
pub type Al = Vec<Option<usize>>;

fn content(l: &str) -> &str {
    l.trim_end_matches(['\n', '\r'])
}

fn blank(l: &str) -> bool {
    l.trim().is_empty()
}

/// Two lines that are the same line: equal, or the same but for the line end
/// that one of them, the last line of its file, lacks.
fn same_line(a: &str, b: &str) -> bool {
    a == b || ((!a.ends_with('\n') || !b.ends_with('\n')) && content(a) == content(b))
}

// ---------- what each side did ----------

/// The side's alignment with the base by identity: the base lines it still
/// has and did not move. (Kept lines that would not rise, from a move the
/// script did not record, count as moved: the longest rising run stays.)
pub fn by_identity(base: &File, side: &Edited) -> Al {
    let pos: HashMap<usize, usize> = side
        .file
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| !side.moved.contains(&l.id))
        .map(|(j, l)| (l.id, j))
        .collect();
    let al: Al = base.lines.iter().map(|l| pos.get(&l.id).copied()).collect();
    // The longest rising run of the kept lines' places.
    let kept: Vec<(usize, usize)> = al.iter().enumerate().filter_map(|(i, j)| j.map(|j| (i, j))).collect();
    let n = kept.len();
    let mut len = vec![1usize; n];
    let mut prev = vec![usize::MAX; n];
    for x in 0..n {
        for y in 0..x {
            if kept[y].1 < kept[x].1 && len[y] + 1 > len[x] {
                len[x] = len[y] + 1;
                prev[x] = y;
            }
        }
    }
    let mut out: Al = vec![None; al.len()];
    if let Some(mut x) = (0..n).max_by_key(|&x| (len[x], usize::MAX - x)) {
        loop {
            out[kept[x].0] = Some(kept[x].1);
            if prev[x] == usize::MAX {
                break;
            }
            x = prev[x];
        }
    }
    // A line moved back among the same lines it was between is no move.
    for (i, l) in base.lines.iter().enumerate() {
        let Some(j) = side.file.lines.iter().position(|x| x.id == l.id) else {
            continue;
        };
        if out[i].is_some() {
            continue;
        }
        let before = out[..i].iter().flatten().all(|&p| p < j);
        let after = out[i + 1..].iter().flatten().all(|&p| p > j);
        if before && after {
            out[i] = Some(j);
        }
    }
    out
}

/// A side's changes by its alignment: base lines `base` became the side's
/// lines `side`.
#[derive(Clone, Debug)]
struct Change {
    base: Range<usize>,
    side: Range<usize>,
    who: usize,
}

fn changes(n: usize, len: usize, al: &[Option<usize>], who: usize) -> Vec<Change> {
    let mut out = Vec::new();
    let (mut pi, mut pj) = (0, 0);
    for (i, j) in al
        .iter()
        .enumerate()
        .filter_map(|(i, j)| j.map(|j| (i, j)))
        .chain([(n, len)])
    {
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

/// A line removed and the very same line put back where it was is no change:
/// the ends of each change that are the base's lines again are kept lines.
fn canon(b: &[String], s: &[String], al: Al) -> Al {
    canon_but(b, s, al, &HashSet::new())
}

/// [`canon`], leaving the side's lines `shared` as they are: lines the other
/// side has too (a writer kept R2's line), which are no typed-again copy.
fn canon_but(b: &[String], s: &[String], mut al: Al, shared: &HashSet<usize>) -> Al {
    for c in changes(b.len(), s.len(), &al, 0) {
        let (mut lo, mut lj) = (c.base.start, c.side.start);
        let (mut hi, mut hj) = (c.base.end, c.side.end);
        while lo < hi && lj < hj && !shared.contains(&lj) && same_line(&b[lo], &s[lj]) {
            al[lo] = Some(lj);
            lo += 1;
            lj += 1;
        }
        while lo < hi && lj < hj && !shared.contains(&(hj - 1)) && same_line(&b[hi - 1], &s[hj - 1]) {
            al[hi - 1] = Some(hj - 1);
            hi -= 1;
            hj -= 1;
        }
    }
    al
}

/// The alignments that differ from `al` only in which of two equal lines
/// is the kept one: a removed base line and a kept one beside it with the
/// same text, or an inserted side line and a kept one beside it; and a
/// removed (or inserted) block slid by one along a kept line that equals its
/// far end. Up to `cap` of them, `al` first.
pub fn slides(b: &[String], s: &[String], al: &Al, cap: usize) -> Vec<Al> {
    let start = canon(b, s, al.clone());
    let mut seen: HashSet<Al> = HashSet::new();
    let mut out = Vec::new();
    let mut queue = VecDeque::from([start]);
    while let Some(a) = queue.pop_front() {
        if out.len() >= cap {
            break;
        }
        if !seen.insert(a.clone()) {
            continue;
        }
        out.push(a.clone());
        let mut next = Vec::new();
        // A base line kept and its neighbour removed, with the same text.
        for i in 0..b.len().saturating_sub(1) {
            if !same_line(&b[i], &b[i + 1]) {
                continue;
            }
            match (a[i], a[i + 1]) {
                (Some(j), None) => {
                    let mut v = a.clone();
                    v[i] = None;
                    v[i + 1] = Some(j);
                    next.push(v);
                }
                (None, Some(j)) => {
                    let mut v = a.clone();
                    v[i] = Some(j);
                    v[i + 1] = None;
                    next.push(v);
                }
                _ => {}
            }
        }
        // A removed block slid by one: the kept line right after it has the
        // text of its first line (or the kept line right before it, of its
        // last), so the block may as well have been the lines one further on.
        let kept_at = |i: usize| a.get(i).copied().flatten();
        let mut i = 0;
        while i < b.len() {
            if a[i].is_some() {
                i += 1;
                continue;
            }
            let start = i;
            while i < b.len() && a[i].is_none() {
                i += 1;
            }
            if let Some(j) = kept_at(i).filter(|_| i - start > 1 && same_line(&b[start], &b[i])) {
                let mut v = a.clone();
                v[start] = Some(j);
                v[i] = None;
                next.push(v);
            }
            if let Some(j) = start
                .checked_sub(1)
                .and_then(kept_at)
                .filter(|_| i - start > 1 && same_line(&b[i - 1], &b[start - 1]))
            {
                let mut v = a.clone();
                v[i - 1] = Some(j);
                v[start - 1] = None;
                next.push(v);
            }
        }
        // A side line kept and its neighbour inserted, with the same text.
        let of: HashMap<usize, usize> = a.iter().enumerate().filter_map(|(i, j)| j.map(|j| (j, i))).collect();
        for j in 0..s.len().saturating_sub(1) {
            if !same_line(&s[j], &s[j + 1]) {
                continue;
            }
            match (of.get(&j), of.get(&(j + 1))) {
                (Some(&i), None) => {
                    let mut v = a.clone();
                    v[i] = Some(j + 1);
                    next.push(v);
                }
                (None, Some(&i)) => {
                    let mut v = a.clone();
                    v[i] = Some(j);
                    next.push(v);
                }
                _ => {}
            }
        }
        // An inserted block slid by one, the same way on the side.
        let mut j = 0;
        while j < s.len() {
            if of.contains_key(&j) {
                j += 1;
                continue;
            }
            let start = j;
            while j < s.len() && !of.contains_key(&j) {
                j += 1;
            }
            if j - start > 1 {
                if let Some(&i) = of.get(&j).filter(|_| same_line(&s[start], &s[j])) {
                    let mut v = a.clone();
                    v[i] = Some(start);
                    next.push(v);
                }
                if let Some(&i) = start
                    .checked_sub(1)
                    .and_then(|p| of.get(&p))
                    .filter(|_| same_line(&s[j - 1], &s[start - 1]))
                {
                    let mut v = a.clone();
                    v[i] = Some(j - 1);
                    next.push(v);
                }
            }
        }
        for v in next {
            let v = canon(b, s, v);
            if !seen.contains(&v) {
                queue.push_back(v);
            }
        }
    }
    out
}

/// The number of lines a longest common subsequence of `b` and `s` has.
fn lcs_len(b: &[String], s: &[String]) -> usize {
    let (n, m) = (b.len(), s.len());
    let mut l = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i][j] = if same_line(&b[i], &s[j]) {
                l[i + 1][j + 1] + 1
            } else {
                l[i + 1][j].max(l[i][j + 1])
            };
        }
    }
    l[0][0]
}

/// Whether `al` keeps as many lines as the texts allow: a longest common
/// subsequence.
pub fn minimal(b: &[String], s: &[String], al: &Al) -> bool {
    al.iter().flatten().count() == lcs_len(b, s)
}

/// The shortest history the texts show that is nearest the real one: a
/// longest common subsequence of `b` and `s` with the most of `al`'s kept
/// pairs in it.
pub fn nearest(b: &[String], s: &[String], al: &Al) -> Al {
    let (n, m) = (b.len(), s.len());
    let real: HashSet<(usize, usize)> = al.iter().enumerate().filter_map(|(i, j)| j.map(|j| (i, j))).collect();
    // Score: lines matched first, the real history's pairs second.
    let big = n + m + 1;
    let mut l = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let mut best = l[i + 1][j].max(l[i][j + 1]);
            if same_line(&b[i], &s[j]) {
                best = best.max(l[i + 1][j + 1] + big + usize::from(real.contains(&(i, j))));
            }
            l[i][j] = best;
        }
    }
    let mut out: Al = vec![None; n];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if same_line(&b[i], &s[j]) && l[i][j] == l[i + 1][j + 1] + big + usize::from(real.contains(&(i, j))) {
            out[i] = Some(j);
            i += 1;
            j += 1;
        } else if l[i][j] == l[i + 1][j] {
            i += 1;
        } else {
            j += 1;
        }
    }
    canon(b, s, out)
}

/// The shortest history the texts show that is farthest from the real one:
/// a longest common subsequence of `b` and `s` with the fewest of `al`'s kept
/// pairs in it (where a side moved a block past one as long, the other block
/// moved).
pub fn farthest(b: &[String], s: &[String], al: &Al) -> Al {
    let (n, m) = (b.len(), s.len());
    let real: HashSet<(usize, usize)> = al.iter().enumerate().filter_map(|(i, j)| j.map(|j| (i, j))).collect();
    let big = n + m + 1;
    let score = |i: usize, j: usize| big + usize::from(!real.contains(&(i, j)));
    let mut l = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let mut best = l[i + 1][j].max(l[i][j + 1]);
            if same_line(&b[i], &s[j]) {
                best = best.max(l[i + 1][j + 1] + score(i, j));
            }
            l[i][j] = best;
        }
    }
    let mut out: Al = vec![None; n];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if same_line(&b[i], &s[j]) && l[i][j] == l[i + 1][j + 1] + score(i, j) {
            out[i] = Some(j);
            i += 1;
            j += 1;
        } else if l[i][j] == l[i + 1][j] {
            i += 1;
        } else {
            j += 1;
        }
    }
    canon(b, s, out)
}

/// A side `s` read as the other side `x` (with its alignment `ax`) changed
/// the base, wherever their texts agree: `x`'s kept pairs carried to `s` by a
/// longest common subsequence of the two, and the shortest history of `s`
/// nearest those.
pub fn carried(b: &[String], x: &[String], s: &[String], ax: &Al) -> Al {
    let (n, m) = (x.len(), s.len());
    let mut l = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i][j] = if x[i] == s[j] {
                l[i + 1][j + 1] + 1
            } else {
                l[i + 1][j].max(l[i][j + 1])
            };
        }
    }
    let mut to = vec![None; n];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if x[i] == s[j] {
            to[i] = Some(j);
            i += 1;
            j += 1;
        } else if l[i + 1][j] >= l[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    let target: Al = (0..b.len())
        .map(|i| ax[i].and_then(|j| to[j]).filter(|&k| same_line(&b[i], &s[k])))
        .collect();
    nearest(b, s, &target)
}

// ---------- the placement rules ----------

/// Whether two changes of the two sides touch the same base lines.
fn touch(a: &Change, c: &Change) -> bool {
    match (a.base.is_empty(), c.base.is_empty()) {
        (false, false) => a.base.start < c.base.end && c.base.start < a.base.end,
        (true, false) => c.base.start < a.base.start && a.base.start < c.base.end,
        (false, true) => a.base.start < c.base.start && c.base.start < a.base.end,
        (true, true) => a.base.start == c.base.start,
    }
}

/// How often each line (not blank) is in `x`, by content.
fn count<S: AsRef<str>>(x: &[S]) -> HashMap<String, usize> {
    let mut n: HashMap<String, usize> = HashMap::new();
    for l in x.iter().map(|l| l.as_ref()).filter(|l| !blank(l)) {
        *n.entry(content(l).to_string()).or_default() += 1;
    }
    n
}

/// Of two lines that are the same line, the one with its line end.
fn pick(a: &str, c: &str) -> String {
    if a.ends_with('\n') {
        a.to_string()
    } else {
        c.to_string()
    }
}

/// Two runs inserted at one point, `first` the older side's.
fn two_runs(first: &[String], second: &[String]) -> Option<Vec<String>> {
    let within = |long: &[String], short: &[String]| -> Option<Vec<String>> {
        if short.len() > long.len() {
            return None;
        }
        for at in [0, long.len() - short.len()] {
            if long[at..at + short.len()]
                .iter()
                .zip(short)
                .all(|(p, q)| same_line(p, q))
            {
                let mut v = long.to_vec();
                for (y, q) in short.iter().enumerate() {
                    v[at + y] = pick(&long[at + y], q);
                }
                return Some(v);
            }
        }
        None
    };
    if let Some(v) = within(second, first).or_else(|| within(first, second)) {
        return Some(v);
    }
    let has_all = |long: &[String], short: &[String]| {
        let (cl, cs) = (count(long), count(short));
        !cs.is_empty() && cs.iter().all(|(k, v)| cl.get(k).copied().unwrap_or(0) >= *v)
    };
    let blank_only = |x: &[String]| x.iter().all(|l| blank(l));
    if has_all(first, second) || has_all(second, first) || blank_only(first) || blank_only(second) {
        return None;
    }
    Some(first.iter().chain(second).cloned().collect())
}

/// Copies of a line the placement rules put in more often than either side
/// has it, by rule, where they put them.
#[derive(Default)]
struct Extra {
    /// Two runs inserted at one point that went in whole: how many more
    /// copies of each line than either run has.
    whole: Counts,
    /// Changes of different base lines by the two sides that give the same
    /// line (each side ticked another copy of a task): how many copies of
    /// each line both sides' edits (changes that put in no more lines than
    /// they took out) put in.
    edits: Counts,
}

/// The true merge: the placement rules (see the module docs) applied to each
/// side's changes by its alignment `al` with the base `b` (`None` where the
/// rules say conflict), with the copies the rules put in more often than
/// either side has them ([`Extra`]).
fn rules_traced(b: &[String], sides: [(&[String], &Al); 2], ours_first: bool) -> Option<(String, Extra)> {
    let n = b.len();
    let mut whole: Counts = HashMap::new();
    let all: Vec<Change> = (0..2)
        .flat_map(|x| changes(n, sides[x].0.len(), sides[x].1, x))
        .collect();
    let lines = |c: &Change| -> Vec<String> { sides[c.who].0[c.side.clone()].to_vec() };
    // Groups of changes that touch, across the sides.
    let mut group: Vec<usize> = (0..all.len()).collect();
    loop {
        let mut again = false;
        for x in 0..all.len() {
            for y in 0..all.len() {
                if all[x].who != all[y].who && group[x] != group[y] && touch(&all[x], &all[y]) {
                    let (keep, from) = (group[x].min(group[y]), group[x].max(group[y]));
                    for g in group.iter_mut() {
                        if *g == from {
                            *g = keep;
                        }
                    }
                    again = true;
                }
            }
        }
        if !again {
            break;
        }
    }
    // (base lines, lines, the one side that made it alone)
    let mut pieces: Vec<(Range<usize>, Vec<String>, Option<usize>)> = Vec::new();
    let mut ids = group.clone();
    ids.sort();
    ids.dedup();
    for id in ids {
        let cs: Vec<&Change> = (0..all.len()).filter(|&x| group[x] == id).map(|x| &all[x]).collect();
        let lo = cs.iter().map(|c| c.base.start).min().expect("a change");
        let hi = cs.iter().map(|c| c.base.end).max().expect("a change");
        if let [c] = cs[..] {
            pieces.push((c.base.clone(), lines(c), Some(c.who)));
        } else if cs.len() == 2 && cs.iter().all(|c| c.base.is_empty()) {
            let run = |w: usize| lines(cs.iter().find(|c| c.who == w).expect("one each"));
            let (f, s) = if ours_first { (run(0), run(1)) } else { (run(1), run(0)) };
            let v = two_runs(&f, &s)?;
            if v.len() == f.len() + s.len() {
                let (cf, cs) = (count(&f), count(&s));
                for (k, &x) in &cf {
                    let both = x.min(cs.get(k).copied().unwrap_or(0));
                    if both > 0 {
                        *whole.entry(k.clone()).or_default() += both;
                    }
                }
            }
            pieces.push((lo..lo, v, None));
        } else if cs.iter().all(|c| c.side.is_empty()) {
            let gone = |p: usize| cs.iter().any(|c| c.base.contains(&p));
            pieces.push((
                lo..hi,
                (lo..hi).filter(|&p| !gone(p)).map(|p| b[p].clone()).collect(),
                None,
            ));
        } else if let [c, d] = cs[..] {
            let (x, y) = (lines(c), lines(d));
            if c.base != d.base || x.len() != y.len() || !x.iter().zip(&y).all(|(p, q)| same_line(p, q)) {
                return None;
            }
            pieces.push((lo..hi, x.iter().zip(&y).map(|(p, q)| pick(p, q)).collect(), None));
        } else {
            return None;
        }
    }
    pieces.sort_by_key(|p| (p.0.start, p.0.end));
    // Seams: pieces that meet at one point.
    let mut k = 1;
    while k < pieces.len() {
        if pieces[k - 1].0.end != pieces[k].0.start {
            k += 1;
            continue;
        }
        let (u, v) = (&pieces[k - 1], &pieces[k]);
        if let (Some(su), Some(sv)) = (u.2, v.2) {
            if su != sv && u.0.is_empty() != v.0.is_empty() {
                let (ins, rep, at_end) = if v.0.is_empty() {
                    (k, k - 1, true)
                } else {
                    (k - 1, k, false)
                };
                let (i, r) = (pieces[ins].1.clone(), pieces[rep].1.clone());
                if !i.is_empty() && i.len() <= r.len() {
                    let off = if at_end { r.len() - i.len() } else { 0 };
                    if r[off..off + i.len()].iter().zip(&i).all(|(p, q)| same_line(p, q)) {
                        for (y, q) in i.iter().enumerate() {
                            pieces[rep].1[off + y] = pick(&r[off + y], q);
                        }
                        pieces[rep].2 = None;
                        pieces.remove(ins);
                        continue;
                    }
                }
            }
        }
        let (u, v) = (&pieces[k - 1], &pieces[k]);
        let one_side = u.2.is_some() && u.2 == v.2;
        if !one_side && u.1.iter().any(|p| v.1.iter().any(|q| content(p) == content(q))) {
            return None;
        }
        k += 1;
    }
    let kept = |i: usize| -> Option<String> {
        let (Some(j), Some(k)) = (sides[0].1[i], sides[1].1[i]) else {
            return Some(b[i].clone());
        };
        let (x, y) = (&sides[0].0[j], &sides[1].0[k]);
        if b[i].ends_with('\n') {
            return Some(if !x.ends_with('\n') { x.clone() } else { y.clone() });
        }
        match (x.ends_with('\n'), y.ends_with('\n')) {
            (true, true) if x != y => None,
            (true, _) => Some(x.clone()),
            _ => Some(y.clone()),
        }
    };
    let mut out: Vec<String> = Vec::new();
    let mut p = 0;
    let mut replaced: [Counts; 2] = Default::default();
    for (r, l, who) in &pieces {
        for i in p..r.start {
            out.push(kept(i)?);
        }
        out.extend(l.iter().cloned());
        // Edits only: a change that put in more lines than it took out holds
        // an insertion.
        if let (Some(w), false) = (who, r.is_empty() || l.len() > r.len()) {
            for (k, c) in count(l) {
                *replaced[*w].entry(k).or_default() += c;
            }
        }
        p = r.end;
    }
    for i in p..n {
        out.push(kept(i)?);
    }
    if out.iter().rev().skip(1).any(|l| !l.ends_with('\n')) {
        return None;
    }
    let edits = replaced[0]
        .iter()
        .filter_map(|(k, &c)| replaced[1].get(k).map(|&d| (k.clone(), c.min(d))))
        .collect();
    Some((out.concat(), Extra { whole, edits }))
}

// ---------- the judge ----------

/// How a clean merge was found right.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Class {
    /// The true merge.
    Exact,
    /// The true merge with equal lines next to each other swapped.
    Copies,
    /// The true merge of the shortest history the texts show.
    Smaller,
    /// The true merge of a history as short, that moved the lines a side's
    /// moved lines passed instead.
    Twin,
    /// The same change by text on both sides, read as one.
    Same,
}

/// A merge case with its history: base, ours, theirs.
#[derive(Clone, Debug)]
pub struct History {
    pub base: File,
    pub ours: Edited,
    pub theirs: Edited,
}

impl History {
    pub fn texts(&self) -> [String; 3] {
        [self.base.render(), self.ours.file.render(), self.theirs.file.render()]
    }

    /// Whether the texts cannot carry this history (see [`File::artifact`]).
    pub fn artifact(&self) -> bool {
        self.base.artifact() || self.ours.file.artifact() || self.theirs.file.artifact()
    }

    /// For each side, its lines (not the base's) the other side has too, by
    /// identity: a writer that saw R2 kept R2's new lines.
    fn shared(&self) -> (HashSet<usize>, HashSet<usize>) {
        let base: HashSet<usize> = self.base.lines.iter().map(|l| l.id).collect();
        let ids = |f: &File| -> HashSet<usize> { f.lines.iter().map(|l| l.id).filter(|i| !base.contains(i)).collect() };
        let (io, it) = (ids(&self.ours.file), ids(&self.theirs.file));
        let at = |f: &File, other: &HashSet<usize>| -> HashSet<usize> {
            f.lines
                .iter()
                .enumerate()
                .filter(|(_, l)| other.contains(&l.id))
                .map(|(j, _)| j)
                .collect()
        };
        (at(&self.ours.file, &it), at(&self.theirs.file, &io))
    }

    /// The true merge (`None`: a conflict).
    pub fn truth(&self, ours_first: bool) -> Option<String> {
        self.truth_traced(ours_first).map(|x| x.0)
    }

    /// [`History::truth`], with the copies the rules put in it ([`Extra`]).
    fn truth_traced(&self, ours_first: bool) -> Option<(String, Extra)> {
        let (b, o, t) = (self.base.texts(), self.ours.file.texts(), self.theirs.file.texts());
        let (so, st) = self.shared();
        let ao = canon_but(&b, &o, by_identity(&self.base, &self.ours), &so);
        let at = canon_but(&b, &t, by_identity(&self.base, &self.theirs), &st);
        rules_traced(&b, [(&o, &ao), (&t, &at)], ours_first)
    }
}

/// How often each line (not blank) is somewhere, by content.
type Counts = HashMap<String, usize>;

/// The lines (not blank) every file of `files` has once, that no side moved:
/// in every longest run of them that keeps the first file's order in each
/// other file (where a line moved, the lines it passed stay; where two swapped,
/// neither is fixed). They part each file into the same segments.
fn fixed(files: &[&[String]]) -> HashSet<String> {
    let counts: Vec<Counts> = files.iter().map(|f| count(f)).collect();
    let at: Vec<HashMap<&str, usize>> = files
        .iter()
        .map(|f| {
            f.iter()
                .enumerate()
                .filter(|(_, l)| !blank(l))
                .map(|(i, l)| (content(l), i))
                .collect()
        })
        .collect();
    // Lines each file has once, in the first file's order.
    let mut once: Vec<&str> = counts[0]
        .iter()
        .filter(|(k, &c)| c == 1 && counts.iter().all(|n| n.get(*k) == Some(&1)))
        .map(|(k, _)| k.as_str())
        .collect();
    once.sort_by_key(|k| at[0][k]);
    let mut keep: HashSet<&str> = once.iter().copied().collect();
    for p in &at[1..] {
        let keys: Vec<usize> = once.iter().map(|k| p[k]).collect();
        let every = in_every_rising(&keys);
        keep.retain(|k| every.contains(&p[k]));
    }
    keep.into_iter().map(str::to_string).collect()
}

/// The values of `v` in every longest rising subsequence of it: those in
/// some longest one, alone at their place in it.
fn in_every_rising(v: &[usize]) -> HashSet<usize> {
    let n = v.len();
    // The longest rising run ending at each element, and starting at it.
    let mut end = vec![1usize; n];
    for x in 0..n {
        for y in 0..x {
            if v[y] < v[x] {
                end[x] = end[x].max(end[y] + 1);
            }
        }
    }
    let mut start = vec![1usize; n];
    for x in (0..n).rev() {
        for y in x + 1..n {
            if v[x] < v[y] {
                start[x] = start[x].max(start[y] + 1);
            }
        }
    }
    let longest = end.iter().copied().max().unwrap_or(0);
    let some: Vec<usize> = (0..n).filter(|&x| end[x] + start[x] - 1 == longest).collect();
    some.iter()
        .filter(|&&x| some.iter().filter(|&&y| end[y] == end[x]).count() == 1)
        .map(|&x| v[x])
        .collect()
}

/// Whether the count of each line (not blank) in `m` is bound by the true
/// merge (see the module docs):
///
/// - no line is less often in `m` than all of the true merge (where it is
///   clean), either side and the count each side's own change gives (ours,
///   plus what theirs added or less what it removed: where both removed a
///   copy, they may be two copies);
/// - no line is more often in `m` than both the true merge (where it is
///   clean) and either side have it, but for the copies the rules put in by
///   the reading `m` came from, where they put them (`extra`), beyond those
///   they put in the true merge: the same line from both sides' edits of
///   different base lines (each ticked another copy), and two runs at one
///   point that went in whole;
/// - except that a line over that may be there as often in each segment
///   between lines no side moved ([`fixed`]) as the true merge or either
///   side has it there: the crate keeps a line both added at two places
///   apart only within one stretch between such lines, so copies a segment
///   apart are two lines.
fn bounded(m: &str, truth: Option<(&str, &Extra)>, [b, o, t]: [&str; 3], extra: &Extra) -> bool {
    let lines = |s: &str| s.split_inclusive('\n').map(str::to_string).collect::<Vec<_>>();
    let (lm, lb, lo, lt) = (lines(m), lines(b), lines(o), lines(t));
    let lw = truth.map(|(w, _)| lines(w)).unwrap_or_default();
    let get = |n: &Counts, k: &str| n.get(k).copied().unwrap_or(0);
    let (nm, nb, no, nt, nw) = (count(&lm), count(&lb), count(&lo), count(&lt), count(&lw));
    // What the reading's rules put in beyond what they put in the true merge,
    // which its count already holds.
    let extra = |k: &str| {
        let mine = get(&extra.edits, k) + get(&extra.whole, k);
        let theirs = truth.map_or(0, |(_, x)| get(&x.edits, k) + get(&x.whole, k));
        mine.saturating_sub(theirs)
    };
    let keys: HashSet<&String> = nm.keys().chain(nw.keys()).chain(no.keys()).chain(nt.keys()).collect();
    let mut over: Vec<&String> = Vec::new();
    for k in keys {
        let (cm, cb, co, ct, cw) = (get(&nm, k), get(&nb, k), get(&no, k), get(&nt, k), get(&nw, k));
        let mut low = co.max(ct).min((co + ct).saturating_sub(cb));
        if truth.is_some() {
            low = low.min(cw);
        }
        if cm < low {
            return false;
        }
        if cm > cw.max(co).max(ct) + extra(k) {
            over.push(k);
        }
    }
    if over.is_empty() {
        return true;
    }
    let mut files: Vec<&[String]> = vec![&lb, &lm, &lo, &lt];
    if truth.is_some() {
        files.push(&lw);
    }
    let parts = fixed(&files);
    // Counts per segment: the line that starts it, and the line's content.
    let segments = |ls: &[String]| -> HashMap<(Option<String>, String), usize> {
        let mut n: HashMap<(Option<String>, String), usize> = HashMap::new();
        let mut at: Option<String> = None;
        for l in ls.iter().filter(|l| !blank(l)) {
            let c = content(l).to_string();
            if parts.contains(&c) {
                at = Some(c);
            } else {
                *n.entry((at.clone(), c)).or_default() += 1;
            }
        }
        n
    };
    let (sm, so, st, sw) = (segments(&lm), segments(&lo), segments(&lt), segments(&lw));
    let get2 =
        |n: &HashMap<(Option<String>, String), usize>, k: &(Option<String>, String)| n.get(k).copied().unwrap_or(0);
    sm.iter()
        .filter(|(key, _)| over.contains(&&key.1))
        .all(|(key, &c)| c <= get2(&sw, key).max(get2(&so, key)).max(get2(&st, key)) + extra(&key.1))
}

/// Judge a clean merge `m` of a history (see the module docs): the class it
/// falls in, or what is wrong.
pub fn judge(h: &History, m: &str, ours_first: bool) -> Result<Class, String> {
    judge_why(h, m, ours_first).map(|x| x.0)
}

/// [`judge`], and whether the merge has a line more often than the count
/// bounds allow but for the copies the placement rules put in (two runs at
/// one point that went in whole, edits of different lines to the same text):
/// right by those rules only.
pub fn judge_why(h: &History, m: &str, ours_first: bool) -> Result<(Class, bool), String> {
    let class = judge_class(h, m, ours_first)?;
    if class == Class::Exact {
        return Ok((class, false));
    }
    let traced = h.truth_traced(ours_first);
    let [bs, os, ts] = h.texts();
    let truth = traced.as_ref().map(|(w, x)| (w.as_str(), x));
    let by_rule = !bounded(m, truth, [&bs, &os, &ts], &Extra::default());
    Ok((class, by_rule))
}

fn judge_class(h: &History, m: &str, ours_first: bool) -> Result<Class, String> {
    let traced = h.truth_traced(ours_first);
    if traced.as_ref().is_some_and(|(w, _)| w == m) {
        return Ok(Class::Exact);
    }
    let (b, o, t) = (h.base.texts(), h.ours.file.texts(), h.theirs.file.texts());
    let [bs, os, ts] = h.texts();
    let texts = [bs.as_str(), os.as_str(), ts.as_str()];
    let truth = traced.as_ref().map(|(w, x)| (w.as_str(), x));
    // Whether some pair of the alignments' copies swapped merges to `m`,
    // within the count bounds.
    let fits = |ao: &Al, at: &Al| -> bool {
        let (vo, vt) = (slides(&b, &o, ao, 32), slides(&b, &t, at, 32));
        vo.iter().any(|x| {
            vt.iter().any(|y| {
                rules_traced(&b, [(&o, x), (&t, y)], ours_first)
                    .is_some_and(|(r, extra)| r == m && bounded(m, truth, texts, &extra))
            })
        })
    };
    let (ao, at) = (by_identity(&h.base, &h.ours), by_identity(&h.base, &h.theirs));
    if fits(&ao, &at) {
        return Ok(Class::Copies);
    }
    let (so, st) = h.shared();
    let (co, ct) = (canon_but(&b, &o, ao.clone(), &so), canon_but(&b, &t, at.clone(), &st));
    let (lo, lt) = (minimal(&b, &o, &co), minimal(&b, &t, &ct));
    if !lo || !lt {
        let (no, nt) = (nearest(&b, &o, &co), nearest(&b, &t, &ct));
        if fits(&no, &nt) {
            return Ok(Class::Smaller);
        }
    }
    // A side that moved lines: the lines they passed may as well have moved
    // the other way, as few of them.
    let (mo, mt) = (!h.ours.moved.is_empty(), !h.theirs.moved.is_empty());
    if mo || mt {
        let read = |s: &[String], c: &Al, minimal: bool, moved: bool| -> Vec<Al> {
            let mut v = vec![if minimal { c.clone() } else { nearest(&b, s, c) }];
            if moved {
                v.push(farthest(&b, s, c));
            }
            v
        };
        // One side at a time: two such readings at once may meet at one
        // point and order lines against every file.
        let (vo, vt) = (read(&o, &co, lo, mo), read(&t, &ct, lt, mt));
        let one = |i: usize, j: usize| i == 0 || j == 0;
        if (0..vo.len()).any(|i| (0..vt.len()).any(|j| one(i, j) && fits(&vo[i], &vt[j]))) {
            return Ok(Class::Twin);
        }
    }
    // Where the texts agree, one side read as having made the other's
    // change.
    if fits(&co, &carried(&b, &o, &t, &co)) || fits(&carried(&b, &t, &o, &ct), &ct) {
        return Ok(Class::Same);
    }
    Err(match truth {
        Some((w, _)) => format!("the true merge is {w:?}"),
        None => "the true merge is a conflict".to_string(),
    })
}

/// What the judge tried for a clean merge `m` of a history, for reading a
/// failure: each class's readings, the merge each gives and whether it is
/// within the count bounds.
pub fn explain(h: &History, m: &str, ours_first: bool) -> String {
    let traced = h.truth_traced(ours_first);
    let truth = traced.as_ref().map(|(w, _)| w.clone());
    let (b, o, t) = (h.base.texts(), h.ours.file.texts(), h.theirs.file.texts());
    let [bs, os, ts] = h.texts();
    let texts = [bs.as_str(), os.as_str(), ts.as_str()];
    let mut out = format!("truth {truth:?}\n");
    let mut show = |name: &str, ao: &Al, at: &Al| {
        let r = rules_traced(&b, [(&o, ao), (&t, at)], ours_first);
        let line = match r {
            None => "conflict".to_string(),
            Some((r, extra)) => format!(
                "{:?} same={} bounded={}",
                r,
                r == m,
                bounded(m, traced.as_ref().map(|(w, x)| (w.as_str(), x)), texts, &extra)
            ),
        };
        out += &format!("  {name}: ours {ao:?} theirs {at:?}\n    {line}\n");
    };
    let (ao, at) = (by_identity(&h.base, &h.ours), by_identity(&h.base, &h.theirs));
    show("identity", &ao, &at);
    for x in slides(&b, &o, &ao, 32) {
        for y in slides(&b, &t, &at, 32) {
            if rules_traced(&b, [(&o, &x), (&t, &y)], ours_first).is_some_and(|(r, _)| r == m) {
                show("slide", &x, &y);
            }
        }
    }
    let (so, st) = h.shared();
    let (co, ct) = (canon_but(&b, &o, ao.clone(), &so), canon_but(&b, &t, at.clone(), &st));
    show("canon", &co, &ct);
    let (no, nt) = (nearest(&b, &o, &co), nearest(&b, &t, &ct));
    show("nearest", &no, &nt);
    let (fo, ft) = (farthest(&b, &o, &co), farthest(&b, &t, &ct));
    show("farthest", &fo, &ft);
    show("nearest/farthest", &no, &ft);
    show("farthest/nearest", &fo, &nt);
    out += &format!(
        "  minimal ours {} theirs {}\n",
        minimal(&b, &o, &co),
        minimal(&b, &t, &ct)
    );
    out
}

// ---------- the streams ----------

/// A small deterministic generator (SplitMix64), so the reviewers' streams
/// run as they were written, one proptest seed per case.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next() % 1_000_000) as f64 / 1e6 < p
    }
}

/// Fresh identities, and the lines a script draws from.
struct Script<'a> {
    next: usize,
    pool: &'a dyn Fn(&mut Rng, &str, &mut usize) -> String,
    uq: usize,
    crlf_p: f64,
}

impl Script<'_> {
    fn fresh(&mut self, text: String, crlf: bool) -> Line {
        self.next += 1;
        Line {
            id: self.next,
            text,
            crlf,
        }
    }

    /// One step of a side's script on `s` (the reviewers' truth stream's
    /// operations, and paragraph breaks and ticks).
    fn step(&mut self, r: &mut Rng, s: &mut Edited, who: &str) {
        let n = s.file.lines.len();
        match r.below(24) {
            0..=3 => {
                let t = (self.pool)(r, who, &mut self.uq);
                let c = r.chance(self.crlf_p);
                let l = self.fresh(t, c);
                s.file.lines.insert(r.below(n + 1), l);
            }
            4 => {
                let t = ["new A", "new B", "- x", ""][r.below(4)].to_string();
                let l = self.fresh(t, false);
                s.file.lines.insert(r.below(n + 1), l);
            }
            5..=8 if n > 0 => {
                s.file.lines.remove(r.below(n));
            }
            9..=12 if n > 0 => {
                let at = r.below(n);
                let old = &s.file.lines[at];
                if !old.text.trim().is_empty() {
                    let t = if old.text.starts_with("- [ ]") && r.chance(0.5) {
                        old.text.replacen("- [ ]", "- [x]", 1)
                    } else if r.chance(0.2) {
                        format!("{} e", old.text)
                    } else {
                        format!("{} {who}", old.text)
                    };
                    let c = old.crlf;
                    s.file.lines[at] = self.fresh(t, c);
                }
            }
            13 | 14 if n > 1 => {
                let a = r.below(n);
                if !s.file.lines[a].text.trim().is_empty() || r.chance(0.3) {
                    let l = s.file.lines.remove(a);
                    s.moved.insert(l.id);
                    s.file.lines.insert(r.below(n), l);
                }
            }
            15 if n > 0 => {
                let a = r.below(n);
                let (t, c) = (s.file.lines[a].text.clone(), s.file.lines[a].crlf);
                let l = self.fresh(t, c);
                s.file.lines.insert(r.below(n + 1), l);
            }
            16 if n > 0 && self.crlf_p > 0.0 => {
                let at = r.below(n);
                let (t, c) = (s.file.lines[at].text.clone(), !s.file.lines[at].crlf);
                s.file.lines[at] = self.fresh(t, c);
            }
            17 => s.file.fin = !s.file.fin,
            // A paragraph break between two lines, or one taken away.
            18 | 19 if n > 1 => {
                let at = 1 + r.below(n - 1);
                let (p, q) = (&s.file.lines[at - 1].text, &s.file.lines[at].text);
                if !p.trim().is_empty() && !q.trim().is_empty() {
                    let l = self.fresh(String::new(), false);
                    s.file.lines.insert(at, l);
                } else if q.trim().is_empty() {
                    s.file.lines.remove(at);
                }
            }
            // `---` or a task put in.
            20 => {
                let t = ["---", "- [ ] gym", "- [ ] task"][r.below(3)].to_string();
                let l = self.fresh(t, false);
                s.file.lines.insert(r.below(n + 1), l);
            }
            // A block of lines moved.
            21 if n > 2 => move_block(r, s),
            _ => {}
        }
    }
}

/// Move a block of two to four lines (fewer where the file is short) to
/// another place.
fn move_block(r: &mut Rng, s: &mut Edited) {
    let n = s.file.lines.len();
    let k = (2 + r.below(3)).min(n - 1);
    let a = r.below(n - k + 1);
    let block: Vec<Line> = s.file.lines.drain(a..a + k).collect();
    for l in &block {
        s.moved.insert(l.id);
    }
    let to = r.below(s.file.lines.len() + 1);
    for (y, l) in block.into_iter().enumerate() {
        s.file.lines.insert(to + y, l);
    }
}

/// A frontmatter block for the top of a base, with identities from `id`.
fn frontmatter(id: &mut usize, crlf: bool) -> Vec<Line> {
    ["---", "title: Note", "tags: [a, b]", "---"]
        .iter()
        .map(|t| {
            *id += 1;
            Line {
                id: *id,
                text: t.to_string(),
                crlf,
            }
        })
        .collect()
}

/// Keep only the moves a script made from `from` to `to` that changed a
/// line's order against the other lines both files have: a line moved back
/// among the same lines is no move.
fn settle(from: &Edited, to: &mut Edited) {
    let pos = |f: &File| -> HashMap<usize, usize> { f.lines.iter().enumerate().map(|(i, l)| (l.id, i)).collect() };
    let (a, b) = (pos(&from.file), pos(&to.file));
    let both: Vec<usize> = a.keys().filter(|i| b.contains_key(i)).copied().collect();
    let new: Vec<usize> = to.moved.difference(&from.moved).copied().collect();
    for id in new {
        let (Some(&x), Some(&y)) = (a.get(&id), b.get(&id)) else {
            continue;
        };
        let same = both.iter().filter(|&&o| o != id).all(|o| (a[o] < x) == (b[o] < y));
        if same {
            to.moved.remove(&id);
        }
    }
}

/// The repeated lines of review 5's truth stream.
const REP: &[&str] = &["- x", "- [ ] gym", "---", "", "", "```", "## Notes", "  ", "| a | b |"];

/// Review 5's truth stream, ported with identities: lines from a pool
/// (`- x`, tasks, `---`, blank lines, fences, headings, table rows) or
/// unique; each side inserts, deletes, edits, moves and copies lines, adds or
/// takes away paragraph breaks, ticks tasks, toggles CRLF and the final
/// newline.
pub fn truth_stream(seed: u64) -> History {
    let mut r = Rng(seed);
    let rep = [0.15, 0.45, 0.7][r.below(3)];
    let crlf_p = if r.chance(0.25) { 0.3 } else { 0.0 };
    let pool = move |r: &mut Rng, tag: &str, uq: &mut usize| {
        if r.chance(rep) {
            REP[r.below(REP.len())].to_string()
        } else {
            *uq += 1;
            format!("{tag} {uq}")
        }
    };
    let mut sc = Script {
        next: 1000,
        pool: &pool,
        uq: 0,
        crlf_p,
    };
    // Mostly short files; some longer, some with frontmatter.
    let nb = if r.chance(0.15) { 10 + r.below(30) } else { r.below(10) };
    let mut lines = Vec::new();
    let mut fm = 500;
    if r.chance(0.1) {
        lines.extend(frontmatter(&mut fm, r.chance(crlf_p)));
    }
    for i in 0..nb {
        let text = pool(&mut r, "line", &mut sc.uq);
        lines.push(Line {
            id: i,
            text,
            crlf: r.chance(crlf_p),
        });
    }
    let base = File {
        lines,
        fin: !r.chance(0.2),
    };
    let mut side = |r: &mut Rng, who: &str| {
        let mut s = Edited {
            file: base.clone(),
            moved: HashSet::new(),
        };
        for _ in 0..1 + r.below(3) {
            sc.step(r, &mut s, who);
        }
        let from = Edited {
            file: base.clone(),
            moved: HashSet::new(),
        };
        settle(&from, &mut s);
        s
    };
    let ours = side(&mut r, "o");
    let theirs = side(&mut r, "t");
    History { base, ours, theirs }
}

/// Review 5's realistic notes, ported with identities: paragraphs, task
/// lists (some tasks recur), headings, `---` and blank lines; each side makes
/// one to three ordinary edits: an edit or a tick, a paragraph split or
/// joined, a line or a paragraph added, a line deleted, a log line appended,
/// a task added, a line moved.
pub fn notes_stream(seed: u64) -> History {
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
    if r.chance(0.15) {
        let mut fm = 5000;
        base.extend(frontmatter(&mut fm, false));
    }
    let blocks = if r.chance(0.15) { 6 + r.below(8) } else { 2 + r.below(4) };
    for b in 0..blocks {
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
    let base = File { lines: base, fin: true };
    let mut ids = 10_000;
    let mut edit = |r: &mut Rng, s: &mut Edited, who: &str| {
        let v = &mut s.file.lines;
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
        match r.below(12) {
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
            10 if n > 1 => {
                let l = v.remove(at);
                s.moved.insert(l.id);
                let to = r.below(n);
                s.file.lines.insert(to, l);
            }
            // A paragraph or a run of tasks moved: the lines from `at` to the
            // next blank line.
            11 if n > 2 && !v[at].text.is_empty() => {
                let end = (at..n).find(|&i| v[i].text.is_empty()).unwrap_or(n);
                let block: Vec<Line> = v.drain(at..end).collect();
                for l in &block {
                    s.moved.insert(l.id);
                }
                let to = r.below(v.len() + 1);
                for (y, l) in block.into_iter().enumerate() {
                    s.file.lines.insert(to + y, l);
                }
            }
            _ => {}
        }
    };
    let mut ours = Edited {
        file: base.clone(),
        moved: HashSet::new(),
    };
    let mut theirs = ours.clone();
    for _ in 0..1 + r.below(3) {
        edit(&mut r, &mut ours, "ours");
    }
    for _ in 0..1 + r.below(3) {
        edit(&mut r, &mut theirs, "theirs");
    }
    let from = Edited {
        file: base.clone(),
        moved: HashSet::new(),
    };
    settle(&from, &mut ours);
    settle(&from, &mut theirs);
    History { base, ours, theirs }
}

/// The lost-update guard's histories (review 6's dup fuzz, and the guard
/// streams): R2 is R1 with a remote's script; L is a writer's script on R2
/// (`fresh`: it saw R2) or on R1 (stale: it never did). Lines from blank
/// lines, `---`, `- x` and unique lines; inserts, deletes, edits, copies,
/// moves and paragraph breaks. Ours is L, theirs R2.
pub fn guard_stream(seed: u64, fresh: Option<bool>) -> (History, bool) {
    let mut r = Rng(seed);
    const POOL: &[&str] = &["", "", "---", "- x", "u", "u", "u", "u"];
    let pool = |r: &mut Rng, tag: &str, uq: &mut usize| {
        let w = POOL[r.below(POOL.len())];
        if w == "u" {
            *uq += 1;
            format!("{tag} {uq}")
        } else {
            w.to_string()
        }
    };
    // Some files with `\r\n` line ends, some without a final newline, some
    // longer, some with frontmatter.
    let crlf_p = if r.chance(0.15) { 0.3 } else { 0.0 };
    let mut sc = Script {
        next: 1000,
        pool: &pool,
        uq: 0,
        crlf_p,
    };
    let nb = if r.chance(0.15) { 8 + r.below(25) } else { r.below(8) };
    let mut lines = Vec::new();
    let mut fm = 500;
    if r.chance(0.1) {
        lines.extend(frontmatter(&mut fm, r.chance(crlf_p)));
    }
    for i in 0..nb {
        let text = pool(&mut r, "line", &mut sc.uq);
        lines.push(Line {
            id: i,
            text,
            crlf: r.chance(crlf_p),
        });
    }
    let r1 = File {
        lines,
        fin: !r.chance(0.1),
    };
    let start = Edited {
        file: r1.clone(),
        moved: HashSet::new(),
    };
    let mut script = |r: &mut Rng, from: &Edited, who: &str| {
        let mut s = from.clone();
        for _ in 0..1 + r.below(3) {
            sc.step(r, &mut s, who);
        }
        settle(from, &mut s);
        s
    };
    let r2 = script(&mut r, &start, "remote");
    let fresh = fresh.unwrap_or_else(|| r.chance(0.5));
    let l = if fresh {
        script(&mut r, &r2, "writer")
    } else {
        script(&mut r, &start, "writer")
    };
    (
        History {
            base: r1,
            ours: l,
            theirs: r2,
        },
        fresh,
    )
}

/// A file written out by hand: each line's identity and text, all with the
/// same line end (`crlf`), and whether the last one ends with it.
pub fn file(lines: &[(usize, &str)], crlf: bool, fin: bool) -> File {
    File {
        lines: lines
            .iter()
            .map(|&(id, text)| Line {
                id,
                text: text.to_string(),
                crlf,
            })
            .collect(),
        fin,
    }
}

/// A side written out by hand: its file and the identities it moved.
pub fn edited(file: File, moved: &[usize]) -> Edited {
    Edited {
        file,
        moved: moved.iter().copied().collect(),
    }
}

/// Review 7's journal notes (`rev7-probe` `journal`), ported with
/// identities: unique day headings, sections of tasks, notes, blank lines and
/// `---`. Ours carries an open task forward, from its section to near the top
/// of a later one (or to the end), and maybe edits more; theirs makes one to
/// three ordinary edits: a tick or an edit, a task or a line added, a line
/// deleted, a log line appended. The bool: whether all of theirs' edits are
/// in sections other than the two the carry touches (and, where the task went
/// to the last section, theirs appended nothing).
pub fn journal_stream(seed: u64) -> (History, bool) {
    let mut r = Rng(seed);
    const POOL: &[&str] = &[
        "",
        "- [ ] gym",
        "- [ ] water plants",
        "",
        "---",
        "- x",
        "- [ ] call mom",
    ];
    let mut id = 0;
    let mut fresh = |text: String| {
        id += 1;
        Line { id, text, crlf: false }
    };
    let mut lines = Vec::new();
    // Each line's section.
    let mut section = Vec::new();
    let mut note = 0;
    let sections = 3 + r.below(6);
    for k in 0..sections {
        lines.push(fresh(format!("## Day {k}")));
        section.push(k);
        for _ in 0..2 + r.below(6) {
            let t = if r.chance(0.4) {
                note += 1;
                format!("note {note}")
            } else {
                POOL[r.below(POOL.len())].to_string()
            };
            lines.push(fresh(t));
            section.push(k);
        }
    }
    let base = File { lines, fin: true };
    let sec_of = |f: &File, i: usize| -> usize {
        // The section of a line: the last heading at or before it.
        f.lines[..=i.min(f.lines.len().saturating_sub(1))]
            .iter()
            .rev()
            .find_map(|l| l.text.strip_prefix("## Day ").and_then(|x| x.parse().ok()))
            .unwrap_or(0)
    };
    let mut ours = Edited {
        file: base.clone(),
        moved: HashSet::new(),
    };
    // The carry: an open task, to near the top of a later section or the end.
    let tasks: Vec<usize> = (0..base.lines.len())
        .filter(|&i| base.lines[i].text.starts_with("- [ ]"))
        .collect();
    let mut touched: HashSet<usize> = HashSet::new();
    let mut to_last = false;
    if !tasks.is_empty() {
        let i = tasks[r.below(tasks.len())];
        let from = section[i];
        let l = ours.file.lines.remove(i);
        ours.moved.insert(l.id);
        let heads: Vec<usize> = (i..ours.file.lines.len())
            .filter(|&j| ours.file.lines[j].text.starts_with("## "))
            .collect();
        let n = ours.file.lines.len();
        let to = if heads.is_empty() || r.chance(0.2) {
            n
        } else {
            (heads[r.below(heads.len())] + 1 + r.below(3)).min(n)
        };
        let into = if to == 0 { 0 } else { sec_of(&ours.file, to - 1) };
        ours.file.lines.insert(to, l);
        touched.insert(from);
        touched.insert(into);
        to_last = into + 1 == sections;
    }
    let mut next = 10_000;
    let mut more = |r: &mut Rng, s: &mut Edited, who: &str, log: &mut bool| -> Option<usize> {
        let v = &mut s.file.lines;
        let n = v.len();
        let at = r.below(n);
        next += 1;
        let line = |t: String| Line {
            id: next,
            text: t,
            crlf: false,
        };
        match r.below(5) {
            0 => {
                v.insert(at, line(format!("{who} added {next}")));
                Some(at)
            }
            1 if !v[at].text.is_empty() && !v[at].text.starts_with("## ") => {
                let t = if v[at].text.starts_with("- [ ]") {
                    v[at].text.replacen("- [ ]", "- [x]", 1)
                } else {
                    format!("{} {who}", v[at].text)
                };
                v[at] = line(t);
                Some(at)
            }
            2 => {
                v.insert(at, line(format!("- [ ] {who} task {next}")));
                Some(at)
            }
            3 if !v[at].text.starts_with("## ") && at > 0 => {
                v.remove(at);
                Some(at - 1)
            }
            4 => {
                v.push(line(format!("- {who} log {next}")));
                *log = true;
                Some(v.len() - 1)
            }
            _ => None,
        }
    };
    let mut log = false;
    if r.chance(0.3) {
        let mut ignore = false;
        if let Some(at) = more(&mut r, &mut ours, "o", &mut ignore) {
            touched.insert(sec_of(&ours.file, at));
        }
    }
    let mut theirs = Edited {
        file: base.clone(),
        moved: HashSet::new(),
    };
    let mut apart = true;
    for _ in 0..1 + r.below(3) {
        if let Some(at) = more(&mut r, &mut theirs, "t", &mut log) {
            // An edit at a section's first line after its heading touches the
            // section before too (the line it went in after).
            let k = sec_of(&theirs.file, at);
            let before = sec_of(&theirs.file, at.saturating_sub(1));
            if touched.contains(&k) || touched.contains(&before) {
                apart = false;
            }
        }
    }
    if to_last && log {
        apart = false;
    }
    let from = Edited {
        file: base.clone(),
        moved: HashSet::new(),
    };
    settle(&from, &mut ours);
    (History { base, ours, theirs }, apart)
}

impl History {
    /// The history, by identity: each file's lines as `id:text`, and what
    /// each side moved.
    pub fn describe(&self) -> String {
        let f = |x: &File| {
            x.lines
                .iter()
                .map(|l| format!("{}:{:?}", l.id, l.text))
                .collect::<Vec<_>>()
                .join(" ")
        };
        format!(
            "  base   {}\n  ours   {} moved {:?}\n  theirs {} moved {:?}",
            f(&self.base),
            f(&self.ours.file),
            self.ours.moved,
            f(&self.theirs.file),
            self.theirs.moved
        )
    }
}

/// Review 6's dup fuzz (`rev6-probe` `dup`), ported with identities: lines
/// from blank lines, `---`, `- x` and unique lines; R2 is R1 with one to
/// three inserts, deletes, edits, copies or moves; L is either the same kind
/// of script on R1 (a stale writer) or R2 with one to three inserts, deletes
/// or edits (a writer that saw R2). Ours is L, theirs R2; the bool is
/// whether L is the newer.
pub fn dup_stream(seed: u64) -> (History, bool) {
    let mut r = Rng(seed);
    const POOL: &[&str] = &["", "", "---", "- x", "u", "u", "u", "u"];
    let mut next = 1000;
    let mut uq = 0;
    // Some files with `\r\n` line ends (on some lines), some without a final
    // newline, some longer.
    let crlf_p = if r.chance(0.15) { 0.4 } else { 0.0 };
    let mut word = |r: &mut Rng, who: &str, next: &mut usize| {
        let w = POOL[r.below(POOL.len())];
        let text = if w == "u" {
            uq += 1;
            format!("{who} {uq}")
        } else {
            w.to_string()
        };
        *next += 1;
        Line {
            id: *next,
            text,
            crlf: r.chance(crlf_p),
        }
    };
    let len = if r.chance(0.15) { 8 + r.below(25) } else { r.below(8) };
    let mut lines = Vec::new();
    for i in 0..len {
        let mut l = word(&mut r, "line", &mut next);
        l.id = i;
        lines.push(l);
    }
    let r1 = File {
        lines,
        fin: !r.chance(0.1),
    };
    let mut side = |r: &mut Rng, from: &Edited, who: &str, ops: usize, next: &mut usize| {
        let mut s = from.clone();
        for _ in 0..1 + r.below(3) {
            let v = &mut s.file.lines;
            match r.below(ops) {
                0 => {
                    let l = word(r, who, next);
                    let at = r.below(v.len() + 1);
                    v.insert(at, l);
                }
                1 if !v.is_empty() => {
                    let at = r.below(v.len());
                    v.remove(at);
                }
                2 if !v.is_empty() => {
                    let at = r.below(v.len());
                    if !v[at].text.trim().is_empty() {
                        *next += 1;
                        v[at] = Line {
                            id: *next,
                            text: format!("{} {who}", v[at].text),
                            crlf: false,
                        };
                    }
                }
                3 if !v.is_empty() => {
                    let c = v[r.below(v.len())].text.clone();
                    let at = r.below(v.len() + 1);
                    *next += 1;
                    v.insert(
                        at,
                        Line {
                            id: *next,
                            text: c,
                            crlf: false,
                        },
                    );
                }
                4 if v.len() > 1 => {
                    let l = v.remove(r.below(v.len()));
                    s.moved.insert(l.id);
                    let at = r.below(v.len() + 1);
                    v.insert(at, l);
                }
                5 if v.len() > 2 => move_block(r, &mut s),
                6 if !v.is_empty() && crlf_p > 0.0 => {
                    let at = r.below(v.len());
                    *next += 1;
                    v[at] = Line {
                        id: *next,
                        text: v[at].text.clone(),
                        crlf: !v[at].crlf,
                    };
                }
                7 => s.file.fin = !s.file.fin,
                _ => {}
            }
        }
        settle(from, &mut s);
        s
    };
    let start = Edited {
        file: r1.clone(),
        moved: HashSet::new(),
    };
    let r2 = side(&mut r, &start, "remote", 8, &mut next);
    let l = if r.below(2) == 0 {
        side(&mut r, &start, "writer", 8, &mut next)
    } else {
        side(&mut r, &r2, "writer", 3, &mut next)
    };
    let l_newer = r.below(2) == 1;
    (
        History {
            base: r1,
            ours: l,
            theirs: r2,
        },
        l_newer,
    )
}
