//! The parts of Google's diff-match-patch (npm `diff-match-patch` 1.0.5, as
//! commonlib uses it) that upstream's line merge runs: `diff_linesToChars_`,
//! `diff_main` with `checklines = false`, and `diff_charsToLines_`.
//!
//! Upstream maps every distinct line to one UTF-16 code unit and diffs those
//! strings. Here a line is a `u32` token, which is the same thing without the
//! string. The port keeps upstream's control flow, including the half-match
//! speedup (on, because upstream's `Diff_Timeout` is 1 s) and the line-count
//! caps in `diff_linesToChars_`. It leaves out the timeout itself: upstream gives
//! up on a diff that takes longer than a second and returns delete-all/insert-all,
//! which depends on the machine. Without it the result is the one upstream
//! computes whenever it finishes in time.
//!
//! Every step counts against a [`Budget`] instead: a deterministic count of
//! the work done (lines read, tokens compared, diagonals searched), so the
//! same input runs out of it on every machine, and a diff that would take too
//! long ends with [`OutOfBudget`] rather than a result.

/// A diff operation, with upstream's numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Delete = -1,
    Equal = 0,
    Insert = 1,
}

pub(crate) type Diff = (Op, Vec<u32>);

/// The work a merge may do, in steps (see the module docs).
#[derive(Debug)]
pub(crate) struct Budget {
    left: u64,
}

/// The [`Budget`] ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutOfBudget;

impl Budget {
    pub fn new(steps: u64) -> Budget {
        Budget { left: steps }
    }
    /// For tests and the model of upstream, which are not bounded.
    pub fn unlimited() -> Budget {
        Budget { left: u64::MAX }
    }
    /// Count `n` steps.
    pub fn spend(&mut self, n: usize) -> Result<(), OutOfBudget> {
        match self.left.checked_sub(n as u64 + 1) {
            Some(l) => {
                self.left = l;
                Ok(())
            }
            None => {
                self.left = 0;
                Err(OutOfBudget)
            }
        }
    }
    /// Count the work of reading `text` (splitting, hashing or copying its
    /// lines): four steps a line and one per five bytes, which costs about
    /// what a step of the diff does.
    pub fn read(&mut self, text: &str) -> Result<(), OutOfBudget> {
        self.spend(text.len() / 5 + 4 * text.bytes().filter(|b| *b == b'\n').count())
    }
    #[cfg(test)]
    pub fn left(&self) -> u64 {
        self.left
    }
}

/// Lines as tokens: `line_array[token]` is the line, `line_array[0]` is unused.
pub(crate) struct Lines<'a> {
    pub line_array: Vec<&'a str>,
    pub chars1: Vec<u32>,
    pub chars2: Vec<u32>,
}

/// `diff_linesToChars_`: split both texts into lines (each keeping its `\n`)
/// and number the distinct lines. As upstream, text1 may use 40 000 distinct
/// lines and both together 65 535; past that the rest of a text is one line.
pub(crate) fn lines_to_chars<'a>(
    text1: &'a str,
    text2: &'a str,
    budget: &mut Budget,
) -> Result<Lines<'a>, OutOfBudget> {
    budget.read(text1)?;
    budget.read(text2)?;
    let mut line_array: Vec<&'a str> = vec![""];
    let mut hash: std::collections::HashMap<&'a str, u32> = std::collections::HashMap::new();
    let chars1 = munge(text1, 40_000, &mut line_array, &mut hash);
    let chars2 = munge(text2, 65_535, &mut line_array, &mut hash);
    Ok(Lines {
        line_array,
        chars1,
        chars2,
    })
}

fn munge<'a>(
    text: &'a str,
    max_lines: usize,
    line_array: &mut Vec<&'a str>,
    hash: &mut std::collections::HashMap<&'a str, u32>,
) -> Vec<u32> {
    let mut chars = Vec::new();
    let len = text.len() as isize;
    let mut line_start: usize = 0;
    let mut line_end: isize = -1;
    while line_end < len - 1 {
        line_end = match text[line_start..].find('\n') {
            Some(p) => (line_start + p) as isize,
            None => len - 1,
        };
        let mut line = &text[line_start..(line_end + 1) as usize];
        // Upstream keys a plain object by line, and `lineHash["__proto__"] = n`
        // does not create an own property: that line never matches itself.
        if let Some(&t) = hash.get(line).filter(|_| line != "__proto__") {
            chars.push(t);
        } else {
            if line_array.len() == max_lines {
                line = &text[line_start..];
                line_end = len;
            }
            let t = line_array.len() as u32;
            chars.push(t);
            if line != "__proto__" {
                hash.insert(line, t);
            }
            line_array.push(line);
        }
        line_start = (line_end + 1) as usize;
    }
    chars
}

/// `diff_main(text1, text2, false)` on tokens.
pub(crate) fn diff_main(text1: &[u32], text2: &[u32], b: &mut Budget) -> Result<Vec<Diff>, OutOfBudget> {
    b.spend(text1.len() + text2.len())?;
    if text1 == text2 {
        if !text1.is_empty() {
            return Ok(vec![(Op::Equal, text1.to_vec())]);
        }
        return Ok(Vec::new());
    }
    let prefix = common_prefix(text1, text2);
    let (common_prefix, text1, text2) = (&text1[..prefix], &text1[prefix..], &text2[prefix..]);
    let suffix = common_suffix(text1, text2);
    let common_suffix = &text1[text1.len() - suffix..];
    let text1 = &text1[..text1.len() - suffix];
    let text2 = &text2[..text2.len() - suffix];

    let mut diffs = compute(text1, text2, b)?;
    if !common_prefix.is_empty() {
        diffs.insert(0, (Op::Equal, common_prefix.to_vec()));
    }
    if !common_suffix.is_empty() {
        diffs.push((Op::Equal, common_suffix.to_vec()));
    }
    cleanup_merge(&mut diffs, b)?;
    Ok(diffs)
}

fn compute(text1: &[u32], text2: &[u32], b: &mut Budget) -> Result<Vec<Diff>, OutOfBudget> {
    if text1.is_empty() {
        return Ok(vec![(Op::Insert, text2.to_vec())]);
    }
    if text2.is_empty() {
        return Ok(vec![(Op::Delete, text1.to_vec())]);
    }
    let (long, short) = if text1.len() > text2.len() {
        (text1, text2)
    } else {
        (text2, text1)
    };
    if let Some(i) = index_of(long, short, 0, b)? {
        let op = if text1.len() > text2.len() {
            Op::Delete
        } else {
            Op::Insert
        };
        return Ok(vec![
            (op, long[..i].to_vec()),
            (Op::Equal, short.to_vec()),
            (op, long[i + short.len()..].to_vec()),
        ]);
    }
    if short.len() == 1 {
        return Ok(vec![(Op::Delete, text1.to_vec()), (Op::Insert, text2.to_vec())]);
    }
    if let Some(hm) = half_match(text1, text2, b)? {
        let mut diffs = diff_main(hm.text1_a, hm.text2_a, b)?;
        diffs.push((Op::Equal, hm.common));
        diffs.extend(diff_main(hm.text1_b, hm.text2_b, b)?);
        return Ok(diffs);
    }
    bisect(text1, text2, b)
}

/// The first `needle` in `hay` at or after `from`, as `indexOf`.
fn index_of(hay: &[u32], needle: &[u32], from: usize, b: &mut Budget) -> Result<Option<usize>, OutOfBudget> {
    if needle.is_empty() {
        return Ok((from <= hay.len()).then_some(from));
    }
    if from >= hay.len() || needle.len() > hay.len() - from {
        return Ok(None);
    }
    for at in from..=hay.len() - needle.len() {
        let same = common_prefix(&hay[at..at + needle.len()], needle);
        b.spend(same)?;
        if same == needle.len() {
            return Ok(Some(at));
        }
    }
    Ok(None)
}

fn common_prefix(a: &[u32], b: &[u32]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn common_suffix(a: &[u32], b: &[u32]) -> usize {
    a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count()
}

struct HalfMatch<'a> {
    text1_a: &'a [u32],
    text1_b: &'a [u32],
    text2_a: &'a [u32],
    text2_b: &'a [u32],
    common: Vec<u32>,
}

/// `[long_a, long_b, short_a, short_b, common]`
type Hm<'a> = (&'a [u32], &'a [u32], &'a [u32], &'a [u32], Vec<u32>);

fn half_match<'a>(text1: &'a [u32], text2: &'a [u32], b: &mut Budget) -> Result<Option<HalfMatch<'a>>, OutOfBudget> {
    let (long, short) = if text1.len() > text2.len() {
        (text1, text2)
    } else {
        (text2, text1)
    };
    if long.len() < 4 || short.len() * 2 < long.len() {
        return Ok(None);
    }
    let hm1 = half_match_i(long, short, long.len().div_ceil(4), b)?;
    let hm2 = half_match_i(long, short, long.len().div_ceil(2), b)?;
    let hm = match (hm1, hm2) {
        (None, None) => return Ok(None),
        (Some(h), None) | (None, Some(h)) => h,
        (Some(h1), Some(h2)) => {
            if h1.4.len() > h2.4.len() {
                h1
            } else {
                h2
            }
        }
    };
    let (la, lb, sa, sb, common) = hm;
    Ok(Some(if text1.len() > text2.len() {
        HalfMatch {
            text1_a: la,
            text1_b: lb,
            text2_a: sa,
            text2_b: sb,
            common,
        }
    } else {
        HalfMatch {
            text1_a: sa,
            text1_b: sb,
            text2_a: la,
            text2_b: lb,
            common,
        }
    }))
}

fn half_match_i<'a>(
    long: &'a [u32],
    short: &'a [u32],
    i: usize,
    b: &mut Budget,
) -> Result<Option<Hm<'a>>, OutOfBudget> {
    let seed = &long[i..(i + long.len() / 4).min(long.len())];
    let mut best_common: Vec<u32> = Vec::new();
    let mut best: Option<(usize, usize, usize, usize)> = None;
    let mut from = 0usize;
    while let Some(j) = index_of(short, seed, from, b)? {
        let prefix_len = common_prefix(&long[i..], &short[j..]);
        let suffix_len = common_suffix(&long[..i], &short[..j]);
        b.spend(prefix_len + suffix_len)?;
        if best_common.len() < suffix_len + prefix_len {
            best_common = short[j - suffix_len..j + prefix_len].to_vec();
            best = Some((i - suffix_len, i + prefix_len, j - suffix_len, j + prefix_len));
        }
        from = j + 1;
    }
    if best_common.len() * 2 >= long.len() {
        let Some((la, lb, sa, sb)) = best else {
            return Ok(None);
        };
        Ok(Some((
            &long[..la],
            &long[lb..],
            &short[..sa],
            &short[sb..],
            best_common,
        )))
    } else {
        Ok(None)
    }
}

fn bisect(text1: &[u32], text2: &[u32], b: &mut Budget) -> Result<Vec<Diff>, OutOfBudget> {
    let n1 = text1.len() as isize;
    let n2 = text2.len() as isize;
    let max_d = (n1 + n2 + 1) / 2;
    let v_offset = max_d;
    let v_length = 2 * max_d;
    b.spend(2 * v_length as usize)?;
    let mut v1 = vec![-1isize; v_length as usize];
    let mut v2 = vec![-1isize; v_length as usize];
    v1[(v_offset + 1) as usize] = 0;
    v2[(v_offset + 1) as usize] = 0;
    let delta = n1 - n2;
    let front = delta % 2 != 0;
    let (mut k1start, mut k1end, mut k2start, mut k2end) = (0isize, 0isize, 0isize, 0isize);
    for d in 0..max_d {
        let mut k1 = -d + k1start;
        while k1 <= d - k1end {
            let k1_offset = v_offset + k1;
            let mut x1 = if k1 == -d || (k1 != d && v1[(k1_offset - 1) as usize] < v1[(k1_offset + 1) as usize]) {
                v1[(k1_offset + 1) as usize]
            } else {
                v1[(k1_offset - 1) as usize] + 1
            };
            let mut y1 = x1 - k1;
            let from = x1;
            while x1 < n1 && y1 < n2 && text1[x1 as usize] == text2[y1 as usize] {
                x1 += 1;
                y1 += 1;
            }
            b.spend((x1 - from) as usize)?;
            v1[k1_offset as usize] = x1;
            if x1 > n1 {
                k1end += 2;
            } else if y1 > n2 {
                k1start += 2;
            } else if front {
                let k2_offset = v_offset + delta - k1;
                if k2_offset >= 0 && k2_offset < v_length && v2[k2_offset as usize] != -1 {
                    let x2 = n1 - v2[k2_offset as usize];
                    if x1 >= x2 {
                        return bisect_split(text1, text2, x1 as usize, y1 as usize, b);
                    }
                }
            }
            k1 += 2;
        }
        let mut k2 = -d + k2start;
        while k2 <= d - k2end {
            let k2_offset = v_offset + k2;
            let mut x2 = if k2 == -d || (k2 != d && v2[(k2_offset - 1) as usize] < v2[(k2_offset + 1) as usize]) {
                v2[(k2_offset + 1) as usize]
            } else {
                v2[(k2_offset - 1) as usize] + 1
            };
            let mut y2 = x2 - k2;
            let from = x2;
            while x2 < n1 && y2 < n2 && text1[(n1 - x2 - 1) as usize] == text2[(n2 - y2 - 1) as usize] {
                x2 += 1;
                y2 += 1;
            }
            b.spend((x2 - from) as usize)?;
            v2[k2_offset as usize] = x2;
            if x2 > n1 {
                k2end += 2;
            } else if y2 > n2 {
                k2start += 2;
            } else if !front {
                let k1_offset = v_offset + delta - k2;
                if k1_offset >= 0 && k1_offset < v_length && v1[k1_offset as usize] != -1 {
                    let x1 = v1[k1_offset as usize];
                    let y1 = v_offset + x1 - k1_offset;
                    let x2m = n1 - x2;
                    if x1 >= x2m {
                        return bisect_split(text1, text2, x1 as usize, y1 as usize, b);
                    }
                }
            }
            k2 += 2;
        }
    }
    Ok(vec![(Op::Delete, text1.to_vec()), (Op::Insert, text2.to_vec())])
}

fn bisect_split(text1: &[u32], text2: &[u32], x: usize, y: usize, b: &mut Budget) -> Result<Vec<Diff>, OutOfBudget> {
    let mut diffs = diff_main(&text1[..x], &text2[..y], b)?;
    diffs.extend(diff_main(&text1[x..], &text2[y..], b)?);
    Ok(diffs)
}

/// `diff_cleanupMerge`: join adjacent edits, factor out common affixes and
/// shift single edits sideways, as upstream.
fn cleanup_merge(diffs: &mut Vec<Diff>, b: &mut Budget) -> Result<(), OutOfBudget> {
    loop {
        // Each pass reads (and may move) every token.
        b.spend(diffs.len() + diffs.iter().map(|d| d.1.len()).sum::<usize>())?;
        diffs.push((Op::Equal, Vec::new()));
        let mut pointer = 0usize;
        let mut count_delete = 0usize;
        let mut count_insert = 0usize;
        let mut text_delete: Vec<u32> = Vec::new();
        let mut text_insert: Vec<u32> = Vec::new();
        while pointer < diffs.len() {
            match diffs[pointer].0 {
                Op::Insert => {
                    count_insert += 1;
                    text_insert.extend_from_slice(&diffs[pointer].1);
                    pointer += 1;
                }
                Op::Delete => {
                    count_delete += 1;
                    text_delete.extend_from_slice(&diffs[pointer].1);
                    pointer += 1;
                }
                Op::Equal => {
                    if count_delete + count_insert > 1 {
                        if count_delete != 0 && count_insert != 0 {
                            let common = common_prefix(&text_insert, &text_delete);
                            if common != 0 {
                                let at = pointer - count_delete - count_insert;
                                if at > 0 && diffs[at - 1].0 == Op::Equal {
                                    diffs[at - 1].1.extend_from_slice(&text_insert[..common]);
                                } else {
                                    diffs.insert(0, (Op::Equal, text_insert[..common].to_vec()));
                                    pointer += 1;
                                }
                                text_insert.drain(..common);
                                text_delete.drain(..common);
                            }
                            let common = common_suffix(&text_insert, &text_delete);
                            if common != 0 {
                                let mut joined = text_insert[text_insert.len() - common..].to_vec();
                                joined.extend_from_slice(&diffs[pointer].1);
                                diffs[pointer].1 = joined;
                                text_insert.truncate(text_insert.len() - common);
                                text_delete.truncate(text_delete.len() - common);
                            }
                        }
                        pointer -= count_delete + count_insert;
                        diffs.drain(pointer..pointer + count_delete + count_insert);
                        if !text_delete.is_empty() {
                            diffs.insert(pointer, (Op::Delete, std::mem::take(&mut text_delete)));
                            pointer += 1;
                        }
                        if !text_insert.is_empty() {
                            diffs.insert(pointer, (Op::Insert, std::mem::take(&mut text_insert)));
                            pointer += 1;
                        }
                        pointer += 1;
                    } else if pointer != 0 && diffs[pointer - 1].0 == Op::Equal {
                        let t = std::mem::take(&mut diffs[pointer].1);
                        diffs[pointer - 1].1.extend(t);
                        diffs.remove(pointer);
                    } else {
                        pointer += 1;
                    }
                    count_insert = 0;
                    count_delete = 0;
                    text_delete.clear();
                    text_insert.clear();
                }
            }
        }
        if diffs.last().is_some_and(|d| d.1.is_empty()) {
            diffs.pop();
        }

        let mut changes = false;
        let mut pointer = 1usize;
        while pointer + 1 < diffs.len() {
            if diffs[pointer - 1].0 == Op::Equal && diffs[pointer + 1].0 == Op::Equal {
                b.spend(diffs[pointer - 1].1.len() + diffs[pointer].1.len() + diffs[pointer + 1].1.len())?;
                let prev = diffs[pointer - 1].1.clone();
                let next = diffs[pointer + 1].1.clone();
                let cur = diffs[pointer].1.clone();
                if cur.ends_with(&prev) {
                    let mut moved = prev.clone();
                    moved.extend_from_slice(&cur[..cur.len() - prev.len()]);
                    diffs[pointer].1 = moved;
                    let mut nxt = prev;
                    nxt.extend_from_slice(&next);
                    diffs[pointer + 1].1 = nxt;
                    diffs.remove(pointer - 1);
                    changes = true;
                } else if cur.starts_with(&next) {
                    diffs[pointer - 1].1.extend_from_slice(&next);
                    let mut moved = cur[next.len()..].to_vec();
                    moved.extend_from_slice(&next);
                    diffs[pointer].1 = moved;
                    diffs.remove(pointer + 1);
                    changes = true;
                }
            }
            pointer += 1;
        }
        if !changes {
            return Ok(());
        }
    }
}

/// `diff_charsToLines_` followed by joining each diff's lines.
pub(crate) fn to_text(diffs: &[Diff], line_array: &[&str]) -> Vec<(Op, String)> {
    diffs
        .iter()
        .map(|(op, toks)| {
            let mut s = String::new();
            for &t in toks {
                s.push_str(line_array[t as usize]);
            }
            (*op, s)
        })
        .collect()
}

/// The line-mode diff mergeSensibly computes between two texts.
pub(crate) fn line_diff(a: &str, b: &str, budget: &mut Budget) -> Result<Vec<(Op, String)>, OutOfBudget> {
    let lines = lines_to_chars(a, b, budget)?;
    let diffs = diff_main(&lines.chars1, &lines.chars2, budget)?;
    budget.read(a)?;
    budget.read(b)?;
    Ok(to_text(&diffs, &lines.line_array))
}
