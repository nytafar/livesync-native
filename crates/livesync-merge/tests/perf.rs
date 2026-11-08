//! Review 5's performance probes: inputs at the size limits that once ran for
//! seconds (a loop of the check no budget counted). Every merge's work is
//! counted against `MAX_COST`, so each ends, written or open, in about 100 ms
//! on a 2020s desktop in a release build. Here a release build allows ten
//! times that (machines and load differ); a debug build runs the shapes
//! smaller, only to see they end.

use livesync_merge::{merge, Input, Kind, Outcome, Settings, Version};
use std::time::{Duration, Instant};

fn ver(s: &str, mtime: i64) -> Version<'_> {
    Version {
        content: s.as_bytes(),
        mtime,
        deleted: false,
    }
}

fn join(v: &[String]) -> String {
    v.join("\n") + "\n"
}

/// The scale: full size in a release build, a tenth in a debug build.
fn n(full: usize) -> usize {
    if cfg!(debug_assertions) {
        full / 10
    } else {
        full
    }
}

/// Merge and time it; `Some(text)` where it writes.
fn timed(what: &str, b: &str, o: &str, t: &str) -> Option<String> {
    let input = Input {
        base: Some(ver(b, 1000)),
        ours: ver(o, 2000),
        theirs: ver(t, 3000),
    };
    let start = Instant::now();
    let m = merge(Kind::Text, &input, &Settings::default());
    let took = start.elapsed();
    if !cfg!(debug_assertions) {
        assert!(took < Duration::from_secs(1), "{what}: {took:?}");
    }
    match m.outcome {
        Outcome::Write(w) => Some(String::from_utf8(w).unwrap()),
        _ => None,
    }
}

#[test]
fn blank_lines_or_rules_added_in_many_gaps() {
    for size in [3000, 12000, 30000] {
        let size = n(size);
        for ins in ["", "---"] {
            let base: Vec<String> = (0..size).map(|i| format!("line {i}")).collect();
            let (mut o, mut t) = (Vec::new(), Vec::new());
            for (i, l) in base.iter().enumerate() {
                o.push(l.clone());
                t.push(l.clone());
                if i % 3 == 0 {
                    o.push(ins.into());
                }
                if i % 3 == 1 {
                    t.push(ins.into());
                }
            }
            timed(
                &format!("{ins:?} in many gaps, {size} lines"),
                &join(&base),
                &join(&o),
                &join(&t),
            );
            // Each side's breaks in its own half: they merge (where the diff
            // fits the budget).
            let (mut o, mut t) = (Vec::new(), Vec::new());
            for (i, l) in base.iter().enumerate() {
                o.push(l.clone());
                t.push(l.clone());
                if i % 5 == 4 && i < size / 2 {
                    o.push(ins.into());
                }
                if i % 5 == 4 && i >= size / 2 {
                    t.push(ins.into());
                }
            }
            let mut want = Vec::new();
            for (i, l) in base.iter().enumerate() {
                want.push(l.clone());
                if i % 5 == 4 {
                    want.push(ins.into());
                }
            }
            if let Some(m) = timed(
                &format!("{ins:?} in halves, {size} lines"),
                &join(&base),
                &join(&o),
                &join(&t),
            ) {
                assert_eq!(m, join(&want));
            }
        }
    }
}

#[test]
fn long_lines_identical_or_distinct() {
    for (lines, width) in [(300, 2000), (500, 2000), (1000, 1000), (2000, 200)] {
        let lines = n(lines).max(10);
        let pre = "x".repeat(width);
        let base: Vec<String> = (0..lines).map(|_| pre.clone()).collect();
        let mut o = base.clone();
        o[lines / 3] = format!("{pre} o");
        o.insert(lines / 2, format!("{pre} new o"));
        let mut t = base.clone();
        t[2 * lines / 3] = format!("{pre} t");
        t.remove(lines / 4);
        timed(
            &format!("{lines} identical lines of {width} B"),
            &join(&base),
            &join(&o),
            &join(&t),
        );
        let base: Vec<String> = (0..lines).map(|i| format!("{pre}{i}")).collect();
        let mut o = base.clone();
        o.reverse();
        let mut t = base.clone();
        t.rotate_left(lines / 2);
        timed(
            &format!("{lines} distinct lines of {width} B, both reorder"),
            &join(&base),
            &join(&o),
            &join(&t),
        );
    }
}

#[test]
fn repeated_lines_with_scattered_edits() {
    for size in [500, 1500, 3000] {
        let size = n(size).max(100);
        let base: Vec<String> = (0..size)
            .map(|i| ["- x", "", "---", "- [ ] gym"][i % 4].to_string())
            .collect();
        let (mut o, mut t) = (base.clone(), base.clone());
        for k in 0..size / 50 {
            o.insert(k * 50 + 7, "- x".into());
            let at = (k * 50 + 30).min(t.len() - 1);
            t.remove(at);
        }
        timed(&format!("{size} repeated lines"), &join(&base), &join(&o), &join(&t));
    }
}

#[test]
fn long_logs() {
    for size in [10000, 30000] {
        let size = n(size);
        let base: Vec<String> = (0..size).map(|i| format!("- {i:05} log entry")).collect();
        let mut o = base.clone();
        o.push("- ours".into());
        let mut t = base.clone();
        t.push("- theirs".into());
        let m = timed(
            &format!("log of {size}, both append"),
            &join(&base),
            &join(&o),
            &join(&t),
        );
        let mut want = base.clone();
        want.extend(["- ours".to_string(), "- theirs".to_string()]);
        assert_eq!(m, Some(join(&want)));
        let mut o = base.clone();
        o[10] = "- edited".into();
        o.push(String::new());
        let mut t = base.clone();
        t.push(String::new());
        t.push("- theirs".into());
        timed(
            &format!("log of {size}, edit and blank vs blank and append"),
            &join(&base),
            &join(&o),
            &join(&t),
        );
    }
}

/// Review 7, finding 3: a long note with sparse unique lines (a heading every
/// few hundred lines, repeated lines between) merges where each side changed
/// lines far apart or one per section: the check lines up only what the
/// sides changed around each anchor, so no window is too large to read and
/// the file is no stretch of its own (895839e opened these, or ran out of
/// budget).
#[test]
fn sparse_unique_lines_merge() {
    let rep = ["", "- [ ] gym", "", "---", "- x"];
    for (size, k) in [(1500, 300), (3000, 50), (3000, 200), (3000, 1000), (10000, 200)] {
        let size = n(size);
        let k = k.min(size / 3).max(5);
        let base: Vec<String> = (0..size)
            .map(|i| {
                if i % k == 0 {
                    format!("## Day {i}")
                } else {
                    rep[i % rep.len()].to_string()
                }
            })
            .collect();
        // One insertion each, far apart.
        let (mut o, mut t) = (base.clone(), base.clone());
        o.insert(3, "o note".into());
        t.insert(size - 3, "t note".into());
        let what = format!("{size} lines, a heading every {k}, far apart");
        assert!(
            timed(&what, &join(&base), &join(&o), &join(&t)).is_some(),
            "{what}: open"
        );
        // One insertion per section each.
        let (mut o, mut t) = (base.clone(), base.clone());
        for s in (0..size).step_by(k).rev() {
            if s + k / 2 < size {
                t.insert(s + k / 2, format!("t note {s}"));
            }
            if s + 3 < size {
                o.insert(s + 2, format!("o note {s}"));
            }
        }
        let what = format!("{size} lines, a heading every {k}, one each per section");
        assert!(
            timed(&what, &join(&base), &join(&o), &join(&t)).is_some(),
            "{what}: open"
        );
    }
}

/// Review 8, finding 1: a block of tasks that recur elsewhere in the note
/// (so none is an anchor) carried from one long section to the next, while
/// the other side adds a line further down. Every carried line asks whether
/// a longest common subsequence of the same window pairs it; the window's
/// table is built once, not once per line, so the merge stays far inside the
/// budget (it ran out of budget, `too_large`, at 300-line sections).
#[test]
fn recurring_block_carried_between_long_sections() {
    for (s, k) in [(100, 20), (300, 20), (600, 20), (300, 60)] {
        let s = n(s);
        let tasks: Vec<String> = (0..k).map(|i| format!("- [ ] task {i}")).collect();
        let fill = |tag: &str, n: usize| -> Vec<String> {
            (0..n)
                .map(|i| if i % 3 == 0 { String::new() } else { format!("- {tag}") })
                .collect()
        };
        let mut b = vec!["# Top".to_string()];
        b.extend(tasks.clone());
        b.push("## A".into());
        b.extend(tasks.clone());
        b.extend(fill("a", s));
        b.push("## B".into());
        b.extend(fill("b", s));
        b.push("## C".into());
        b.extend(fill("c", 20));
        // Ours carries the block from the top of A to the top of B.
        let mut o = b.clone();
        let a = o.iter().position(|l| l == "## A").unwrap();
        let block: Vec<String> = o.drain(a + 1..a + 1 + k).collect();
        let at = o.iter().position(|l| l == "## B").unwrap() + 1;
        o.splice(at..at, block);
        // Theirs adds a note in C.
        let mut t = b.clone();
        let c = t.iter().position(|l| l == "## C").unwrap();
        t.insert(c + 2, "theirs note".into());
        let mut want = o.clone();
        let c = want.iter().position(|l| l == "## C").unwrap();
        want.insert(c + 2, "theirs note".into());

        let what = format!("block of {k} recurring tasks carried, sections of {s}");
        let start = Instant::now();
        let m = timed(&what, &join(&b), &join(&o), &join(&t));
        let took = start.elapsed();
        assert_eq!(m, Some(join(&want)), "{what}");
        if !cfg!(debug_assertions) {
            assert!(took < Duration::from_millis(100), "{what}: {took:?}");
        }
    }
}
