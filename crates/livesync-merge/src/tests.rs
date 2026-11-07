use super::*;

const OLD: i64 = 2000;
const NEW: i64 = 3000;

fn ver(s: &str, mtime: i64) -> Version<'_> {
    Version {
        content: s.as_bytes(),
        mtime,
        deleted: false,
    }
}

fn gone(mtime: i64) -> Version<'static> {
    Version {
        content: b"",
        mtime,
        deleted: true,
    }
}

/// Ours older than theirs unless `ours_newer`.
fn run(kind: Kind, base: Option<&str>, ours: &str, theirs: &str, ours_newer: bool) -> Merge {
    run_with(kind, base, ours, theirs, ours_newer, &Settings::default())
}

fn run_with(kind: Kind, base: Option<&str>, ours: &str, theirs: &str, ours_newer: bool, s: &Settings) -> Merge {
    let (mo, mt) = if ours_newer { (NEW, OLD) } else { (OLD, NEW) };
    let input = Input {
        base: base.map(|b| ver(b, 1000)),
        ours: ver(ours, mo),
        theirs: ver(theirs, mt),
    };
    merge(kind, &input, s)
}

fn written(m: &Merge) -> String {
    match &m.outcome {
        Outcome::Write(b) => String::from_utf8(b.clone()).unwrap(),
        other => panic!("expected a merge, got {other:?} ({:?})", m.rule),
    }
}

fn overlaps(m: &Merge) -> &[Overlap] {
    match &m.outcome {
        Outcome::Open(o) => o,
        other => panic!("expected open, got {other:?}"),
    }
}

fn md(base: &str, ours: &str, theirs: &str) -> Merge {
    run(Kind::Markdown, Some(base), ours, theirs, false)
}

// ---------- the diff port ----------

#[test]
fn line_diffs_match_diff_match_patch() {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/line_diff.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    assert_eq!(v["meta"]["version"], "0.2.1", "fixtures must come from commonlib 0.2.1");
    let cases = v["cases"].as_array().unwrap();
    assert!(cases.len() > 400);
    for c in cases {
        let (a, b) = (c["a"].as_str().unwrap(), c["b"].as_str().unwrap());
        let ours: Vec<(i64, String)> = dmp::line_diff(a, b).into_iter().map(|(op, t)| (op as i64, t)).collect();
        let want: Vec<(i64, String)> = c["diffs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| (d[0].as_i64().unwrap(), d[1].as_str().unwrap().to_string()))
            .collect();
        assert_eq!(ours, want, "diff of {a:?} -> {b:?}");
    }
}

#[test]
fn upstreams_split_turns_a_trailing_deletion_into_equal_lines() {
    // The model of upstream: splitDiffPiece pushes a deletion with nothing
    // after it as EQUAL.
    assert_eq!(
        lines::split_for_test("a\nb\nc\n", "a\nb\n"),
        vec![(0, "a\n".into()), (0, "b\n".into()), (0, "c\n".into())]
    );
}

// ---------- identical ----------

#[test]
fn identical_content_needs_nothing_new() {
    let m = md("a\n", "same\n", "same\n");
    assert_eq!((m.rule, &m.outcome), (Rule::Identical, &Outcome::Identical));
    // Identical without a base, and for kinds that never merge.
    assert_eq!(run(Kind::Other, None, "x", "x", false).outcome, Outcome::Identical);
    let both_gone = Input {
        base: None,
        ours: gone(OLD),
        theirs: gone(NEW),
    };
    assert_eq!(
        merge(Kind::Markdown, &both_gone, &Settings::default()).outcome,
        Outcome::Identical
    );
}

#[test]
fn same_data_different_deleted_flag_is_not_identical() {
    let input = Input {
        base: None,
        ours: Version {
            content: b"x",
            mtime: OLD,
            deleted: true,
        },
        theirs: ver("x", NEW),
    };
    assert_eq!(
        merge(Kind::Text, &input, &Settings::default()).rule,
        Rule::EditBeatsDelete
    );
}

// ---------- delete against edit ----------

#[test]
fn the_edit_beats_the_delete() {
    for (ours, theirs, keep) in [
        (gone(NEW), ver("edited\n", OLD), Side::Theirs),
        (ver("edited\n", OLD), gone(NEW), Side::Ours),
    ] {
        let input = Input {
            base: Some(ver("orig\n", 1000)),
            ours,
            theirs,
        };
        let m = merge(Kind::Markdown, &input, &Settings::default());
        assert_eq!((m.rule, m.outcome), (Rule::EditBeatsDelete, Outcome::Keep(keep)));
    }
    // It needs no ancestor (without one only rules 1 and 2 apply), and it
    // applies to binaries too: the live leaf is kept as it is.
    let input = Input {
        base: None,
        ours: gone(OLD),
        theirs: ver("\u{0}\u{1}", NEW),
    };
    assert_eq!(
        merge(Kind::Other, &input, &Settings::default()).outcome,
        Outcome::Keep(Side::Theirs)
    );
}

#[test]
fn two_deletes_with_different_data_stay_open() {
    let input = Input {
        base: Some(ver("a", 1000)),
        ours: Version {
            content: b"x",
            mtime: OLD,
            deleted: true,
        },
        theirs: Version {
            content: b"y",
            mtime: NEW,
            deleted: true,
        },
    };
    let m = merge(Kind::Text, &input, &Settings::default());
    assert_eq!(m.rule, Rule::BothDeleted);
    assert_eq!(overlaps(&m), [Overlap::Whole(Reason::BothDeleted)]);
}

// ---------- no ancestor, binaries ----------

#[test]
fn without_an_ancestor_only_identical_resolves() {
    let m = run(Kind::Markdown, None, "a\nleft\n", "a\nright\n", false);
    assert_eq!(
        (m.rule, overlaps(&m)),
        (Rule::NoBase, &[Overlap::Whole(Reason::NoBase)][..])
    );
    let m = run(Kind::Object, None, "{\"a\":1}", "{\"b\":1}", false);
    assert_eq!(m.rule, Rule::NoBase);
}

#[test]
fn a_deleted_base_is_no_base() {
    let input = Input {
        base: Some(gone(1000)),
        ours: ver("a\n", OLD),
        theirs: ver("b\n", NEW),
    };
    assert_eq!(merge(Kind::Text, &input, &Settings::default()).rule, Rule::BaseDeleted);
}

#[test]
fn binaries_and_other_kinds_stay_open() {
    let m = run(Kind::Other, Some("a"), "b", "c", false);
    assert_eq!(
        (m.rule, overlaps(&m)),
        (Rule::Unsupported, &[Overlap::Whole(Reason::Unsupported)][..])
    );
    let input = Input {
        base: Some(ver("a\n", 1000)),
        ours: Version {
            content: &[0xff, 0xfe, b'\n'],
            mtime: OLD,
            deleted: false,
        },
        theirs: ver("b\n", NEW),
    };
    let m = merge(Kind::Text, &input, &Settings::default());
    assert_eq!(overlaps(&m), [Overlap::Whole(Reason::NotUtf8(Which::Ours))]);
}

#[test]
fn kinds_come_from_the_settings() {
    let s = Settings::default();
    assert_eq!(s.kind_of("a/b.md"), Kind::Markdown);
    assert_eq!(s.kind_of("b.txt"), Kind::Text);
    assert_eq!(s.kind_of("board.canvas"), Kind::Object);
    assert_eq!(s.kind_of(".obsidian/app.json"), Kind::Object);
    assert_eq!(s.kind_of("image.png"), Kind::Other);
    assert_eq!(s.kind_of("README.MD"), Kind::Other, "case-sensitive, as upstream");
    let custom = Settings {
        text: vec![".org".into()],
        object: vec![],
        ..Settings::default()
    };
    assert_eq!(custom.kind_of("x.org"), Kind::Text);
    assert_eq!(custom.kind_of("x.json"), Kind::Other);
}

// ---------- lines ----------

#[test]
fn an_insertion_at_the_end_of_a_replaced_range_goes_after_it() {
    let m = md("a\nb\nc\n", "a\nB\nC\n", "a\nb\nc\nd\n");
    assert_eq!(written(&m), "a\nB\nC\nd\n");
    // With ours newer upstream gives a B d C (theirs' line inside ours'
    // replacement).
    let m = run(Kind::Text, Some("a\nb\nc\n"), "a\nB\nC\n", "a\nb\nc\nd\n", true);
    assert_eq!(written(&m), "a\nB\nC\nd\n");
    assert_eq!(
        upstream_line_merge("a\nb\nc\n", "a\nB\nC\n", "a\nb\nc\nd\n", false).unwrap(),
        "a\nB\nd\nC\n"
    );
    assert_eq!(m.deviations, [Deviation::PositionOrder]);
    // At its start: before it.
    let m = md("a\nb\nc\n", "a\nB\nC\n", "a\nx\nb\nc\n");
    assert_eq!(written(&m), "a\nx\nB\nC\n");
}

#[test]
fn a_sub_item_stays_under_its_ticked_parent() {
    let base = "- [ ] task\n- [ ] next\n";
    let ticked = "- [x] task\n- [ ] next\n";
    let sub = "- [ ] task\n  - detail\n- [ ] next\n";
    for ours_newer in [false, true] {
        let m = run(Kind::Markdown, Some(base), ticked, sub, ours_newer);
        assert_eq!(written(&m), "- [x] task\n  - detail\n- [ ] next\n");
        let m = run(Kind::Markdown, Some(base), sub, ticked, ours_newer);
        assert_eq!(written(&m), "- [x] task\n  - detail\n- [ ] next\n");
    }
}

#[test]
fn a_line_changed_and_a_line_appended_after_it() {
    // A lost-update guard's log case: the stale writer marked the entry done,
    // the overwritten revision appended one.
    let base = "# Log\n- 09 start\n";
    let done = "# Log\n- 09 start (done)\n";
    let more = "# Log\n- 09 start\n- 10 call\n";
    for ours_newer in [false, true] {
        let m = run(Kind::Markdown, Some(base), done, more, ours_newer);
        assert_eq!(written(&m), "# Log\n- 09 start (done)\n- 10 call\n");
        let m = run(Kind::Markdown, Some(base), more, done, ours_newer);
        assert_eq!(written(&m), "# Log\n- 09 start (done)\n- 10 call\n");
    }
}

#[test]
fn an_insertion_inside_a_replaced_range_stays_open() {
    let m = md("a\nb\nc\nd\n", "a\nX\nY\nd\n", "a\nb\nnew\nc\nd\n");
    assert_eq!(
        overlaps(&m),
        [Overlap::Lines(LineOverlap {
            base: 1..3,
            ours: 1..3,
            theirs: 1..4
        })]
    );
    // Deleted lines with an insertion between them, too.
    assert!(md("a\nb\nc\nd\n", "a\nd\n", "a\nb\nnew\nc\nd\n").outcome.is_open());
}

#[test]
fn replacements_are_compared_whole() {
    // Upstream merges these to "Meeting at 10, Room 1, Room 2".
    let m = md(
        "# M\nMeeting TBD\nend\n",
        "# M\nMeeting at 10\nRoom 1\nend\n",
        "# M\nMeeting at 10\nRoom 2\nend\n",
    );
    assert_eq!(
        overlaps(&m),
        [Overlap::Lines(LineOverlap {
            base: 1..2,
            ours: 1..3,
            theirs: 1..3
        })]
    );
    assert!(m.deviations.is_empty());
    // The same multi-line replacement on both sides: once, as one block.
    let m = md("a\nb\nc\n", "a\nB1\nB2\nB3\nc\n", "a\nB1\nB2\nB3\nc\nd\n");
    assert_eq!(written(&m), "a\nB1\nB2\nB3\nc\nd\n");
    // Two overlapping replacements of different ranges.
    assert!(md("a\nb\nc\nd\n", "a\nX\nY\nd\n", "a\nb\nZ\nW\n").outcome.is_open());
    // Adjacent ones merge.
    assert_eq!(
        written(&md("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nb\nC\nd\n")),
        "a\nB\nC\nd\n"
    );
}

#[test]
fn overlapping_deletions_delete_every_line() {
    let m = md("a\nb\nc\nd\ne\n", "a\nd\ne\n", "a\nb\ne\n");
    assert_eq!(written(&m), "a\ne\n");
}

#[test]
fn one_sided_changes_are_taken() {
    let m = md("a\nb\nc\n", "a\nB\nc\n", "a\nb\nc\n");
    assert_eq!((m.rule, written(&m)), (Rule::Lines, "a\nB\nc\n".into()));
    assert_eq!(
        written(&md("one\ntwo\nthree\n", "ONE\ntwo\nthree\n", "one\ntwo\nTHREE\n")),
        "ONE\ntwo\nTHREE\n"
    );
    assert!(m.deviations.is_empty());
}

#[test]
fn a_line_changed_on_both_sides_stays_open() {
    let m = md("a\nb\nc\n", "a\nX\nc\n", "a\nY\nc\n");
    assert_eq!(
        overlaps(&m),
        [Overlap::Lines(LineOverlap {
            base: 1..2,
            ours: 1..2,
            theirs: 1..2
        })]
    );
}

#[test]
fn delete_against_change_of_a_line_stays_open() {
    let m = md("a\nb\nc\n", "a\nc\n", "a\nY\nc\n");
    assert_eq!(
        overlaps(&m),
        [Overlap::Lines(LineOverlap {
            base: 1..2,
            ours: 1..1,
            theirs: 1..2
        })]
    );
}

#[test]
fn every_overlap_is_reported() {
    let base = "a\nb\nc\nd\ne\n";
    let m = md(base, "a\nB1\nc\nD1\ne\n", "a\nB2\nc\nD2\ne\n");
    assert_eq!(overlaps(&m).len(), 2);
}

#[test]
fn both_appending_keeps_both_older_first() {
    let base = "# Log\n- 09:00 start\n";
    let ours = "# Log\n- 09:00 start\n- 10:00 ours\n";
    let theirs = "# Log\n- 09:00 start\n- 11:00 theirs\n";
    assert_eq!(
        written(&run(Kind::Text, Some(base), ours, theirs, false)),
        "# Log\n- 09:00 start\n- 10:00 ours\n- 11:00 theirs\n"
    );
    assert_eq!(
        written(&run(Kind::Text, Some(base), ours, theirs, true)),
        "# Log\n- 09:00 start\n- 11:00 theirs\n- 10:00 ours\n"
    );
}

#[test]
fn two_inserted_runs_go_in_whole_older_first() {
    let base = "# Log\n";
    let ours = "# Log\nours 1\nours 2\n";
    let theirs = "# Log\ntheirs 1\ntheirs 2\ntheirs 3\n";
    // Upstream interleaves: ours 1, theirs 1, ours 2, theirs 2, theirs 3.
    let m = run(Kind::Text, Some(base), ours, theirs, false);
    assert_eq!(written(&m), "# Log\nours 1\nours 2\ntheirs 1\ntheirs 2\ntheirs 3\n");
    assert_eq!(m.deviations, [Deviation::RunOrder]);
    let m = run(Kind::Text, Some(base), ours, theirs, true);
    assert_eq!(written(&m), "# Log\ntheirs 1\ntheirs 2\ntheirs 3\nours 1\nours 2\n");
    assert_eq!(m.deviations, [Deviation::RunOrder]);
    // Upstream interleaves.
    assert_eq!(
        upstream_line_merge(base, ours, theirs, true).unwrap(),
        "# Log\nours 1\ntheirs 1\nours 2\ntheirs 2\ntheirs 3\n"
    );
}

#[test]
fn two_appends_in_the_middle_of_a_daily_note_stay_blocks() {
    let base = "# Day\n## Log\n- 09:00 start\n## Notes\nend\n";
    let ours = "# Day\n## Log\n- 09:00 start\n- 10:00 call\n  with notes\n## Notes\nend\n";
    let theirs = "# Day\n## Log\n- 09:00 start\n- 11:00 review\n  - item one\n  - item two\n## Notes\nmore\nend\n";
    let m = run(Kind::Markdown, Some(base), ours, theirs, false);
    assert_eq!(
        written(&m),
        "# Day\n## Log\n- 09:00 start\n- 10:00 call\n  with notes\n- 11:00 review\n  - item one\n  - item two\n## Notes\nmore\nend\n"
    );
    assert_eq!(m.deviations, [Deviation::RunOrder]);
}

#[test]
fn single_lines_inserted_at_one_point_are_upstreams_order() {
    // One line each: whole runs and upstream's pairing agree, no deviation.
    let m = run(Kind::Text, Some("a\n"), "a\nours\n", "a\ntheirs\n", false);
    assert_eq!(written(&m), "a\nours\ntheirs\n");
    assert!(m.deviations.is_empty());
    // A run against one line: upstream gives o1 t1 o2.
    let m = run(Kind::Text, Some("a\n"), "a\no1\no2\n", "a\nt1\n", false);
    assert_eq!(written(&m), "a\no1\no2\nt1\n");
    assert_eq!(m.deviations, [Deviation::RunOrder]);
}

#[test]
fn runs_sharing_lines_go_in_whole() {
    // A shared last line that is not a whole prefix or suffix of the other run:
    // both runs whole, the shared line in each.
    let m = run(Kind::Text, Some("a\n"), "a\no1\no2\nX\n", "a\nt1\nt2\nX\n", false);
    assert_eq!(written(&m), "a\no1\no2\nX\nt1\nt2\nX\n");
    // Two fenced blocks, each whole (upstream pairs the fences).
    let m = run(Kind::Text, Some("x\n"), "x\n```\nA\n```\n", "x\n```\nB\n```\n", false);
    assert_eq!(written(&m), "x\n```\nA\n```\n```\nB\n```\n");
    assert_eq!(m.deviations, [Deviation::RunOrder]);
    // Two paragraphs, each with its blank line.
    let m = run(
        Kind::Text,
        Some("# Log\n"),
        "# Log\n\nPara A\n",
        "# Log\n\nPara B\n",
        false,
    );
    assert_eq!(written(&m), "# Log\n\nPara A\n\nPara B\n");
    // One run a whole suffix of the other: the longer, once.
    let m = run(Kind::Text, Some("a\n"), "a\nX\n", "a\nY\nX\n", false);
    assert_eq!(written(&m), "a\nY\nX\n");
    // A whole prefix.
    let m = run(Kind::Text, Some("a\n"), "a\nX\nY\n", "a\nX\n", true);
    assert_eq!(written(&m), "a\nX\nY\n");
}

#[test]
fn whole_runs_that_would_glue_stay_open() {
    // Ours' run ends the file without a newline; going first it would be glued
    // to theirs' line. Upstream interleaves to "o1 t1 o2" and no line is glued.
    let m = run(Kind::Text, Some("a\n"), "a\no1\no2", "a\nt1\n", false);
    assert!(m.outcome.is_open());
    assert_eq!(m.deviations, [Deviation::RunOrder]);
    // The other way round it merges.
    let m = run(Kind::Text, Some("a\n"), "a\no1\no2", "a\nt1\n", true);
    assert_eq!(written(&m), "a\nt1\no1\no2");
}

#[test]
fn the_same_insertion_on_both_sides_is_taken_once() {
    // Aligned: upstream already takes it once.
    let m = md("a\n", "a\nsame\n", "a\nsame\n more\n");
    assert_eq!(written(&m), "a\nsame\n more\n");
    assert!(m.deviations.is_empty());
    // Shifted: upstream gives a X Y X.
    let m = md("a\n", "a\nX\n", "a\nY\nX\n");
    assert_eq!(written(&m), "a\nY\nX\n");
    assert_eq!(m.deviations, [Deviation::SameInsertionOnce]);
    let m = run(Kind::Text, Some("a\n"), "a\nX\n", "a\nY\nX\n", true);
    assert_eq!(written(&m), "a\nY\nX\n");
    // A blank line both added, around different text: both runs whole.
    let m = md("x\n", "x\nours\n\n", "x\n\ntheirs\n");
    assert_eq!(written(&m), "x\nours\n\n\ntheirs\n");
}

#[test]
fn the_same_lines_inserted_in_another_order_go_in_whole() {
    // Upstream pairs them to the same lines.
    let m = md("a\n", "a\nP\nQ\n", "a\nQ\nP\n");
    assert_eq!(written(&m), "a\nP\nQ\nQ\nP\n");
    assert!(m.deviations.is_empty());
}

#[test]
fn crlf_lines_merge_as_lines() {
    let m = md("a\r\nb\r\nc\r\n", "A\r\nb\r\nc\r\n", "a\r\nb\r\nC\r\n");
    assert_eq!(written(&m), "A\r\nb\r\nC\r\n");
    let m = md("a\r\nb\r\n", "a\r\nb\r\nours\r\n", "a\r\nb\r\ntheirs\r\n");
    assert_eq!(written(&m), "a\r\nb\r\nours\r\ntheirs\r\n");
}

#[test]
fn a_missing_final_newline() {
    assert_eq!(written(&md("a\nb\nc", "A\nb\nc", "a\nb\nC")), "A\nb\nC");
    // One side appends after a last line without newline: that side's lines.
    assert_eq!(written(&md("a\nb", "a\nb\nc", "A\nb")), "A\nb\nc");
}

#[test]
fn two_lines_glued_together_stay_open() {
    // Upstream returns "a\nb\nleftright".
    let m = md("a\nb", "a\nb\nleft", "a\nb\nright");
    assert_eq!(m.deviations, [Deviation::GluedLines]);
    assert!(m.outcome.is_open());
    // Glued into a single line (found by the property test): upstream gives "alphaalpha".
    let m = md("alpha\nalpha\n", "alpha\nalpha\nalpha", "alpha");
    assert_eq!(m.deviations, [Deviation::GluedLines]);
    assert!(m.outcome.is_open());
}

#[test]
fn the_closing_fence_keeps_its_line_end() {
    // Found by the property test: the body's first line must not take the
    // fence's newline with it.
    let m = md(
        "---\n---\n\n- [ ] task\n",
        "---\nstatus: v0\n---\n# Heading\n\n- [ ] task\n",
        "---\ntitle: v0\n---\n- [ ] task\n",
    );
    assert_eq!(written(&m), "---\nstatus: v0\ntitle: v0\n---\n# Heading\n- [ ] task\n");
}

#[test]
fn empty_files() {
    assert_eq!(written(&md("", "ours\n", "theirs\n")), "ours\ntheirs\n");
    assert_eq!(md("", "same\n", "same\n").outcome, Outcome::Identical);
    assert_eq!(written(&md("", "", "new\n")), "new\n");
    // Whitespace only in the base is no content to lose.
    assert_eq!(written(&md("\n", "", "\nnew\n")), "new\n");
}

fn emptied(m: &Merge) -> Which {
    assert_eq!(m.rule, Rule::Emptied, "{m:?}");
    match overlaps(m) {
        [Overlap::Whole(Reason::Emptied(w))] => *w,
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_emptied_side_never_merges() {
    let base = "a\nb\n";
    for ours_newer in [false, true] {
        for empty in ["", "\n", " \r\n\t\n"] {
            // Against an unchanged side, an edit, an append: open, either side.
            for other in [base, "a\nB\n", "a\nb\nc\n"] {
                for kind in [Kind::Markdown, Kind::Text] {
                    let m = run(kind, Some(base), empty, other, ours_newer);
                    assert_eq!(emptied(&m), Which::Ours);
                    assert_eq!(m.deviations, [Deviation::EmptiedOpen]);
                    let m = run(kind, Some(base), other, empty, ours_newer);
                    assert_eq!(emptied(&m), Which::Theirs);
                }
            }
        }
    }
    // JSON too; other kinds stay open as ever, with no deviation from upstream.
    let m = run(Kind::Object, Some("{\"a\": 1}\n"), "{\"a\": 1, \"b\": 2}\n", "", false);
    assert_eq!(emptied(&m), Which::Theirs);
    let m = run(Kind::Other, Some("x"), "", "x", true);
    assert_eq!(emptied(&m), Which::Ours);
    assert!(m.deviations.is_empty());
    // Both emptied differently: open, naming ours.
    assert_eq!(emptied(&md(base, "", "\n")), Which::Ours);
    // Both emptied alike is rule 1.
    assert_eq!(md(base, "", "").outcome, Outcome::Identical);
    assert_eq!(md(base, "\n", "\n").rule, Rule::Identical);
}

#[test]
fn a_deleted_leaf_is_not_emptied() {
    let input = Input {
        base: Some(ver("a\n", 1000)),
        ours: gone(OLD),
        theirs: ver("a\nb\n", NEW),
    };
    let m = merge(Kind::Markdown, &input, &Settings::default());
    assert_eq!(m.rule, Rule::EditBeatsDelete);
    // A deleted leaf against an emptied live one: the live (empty) side is kept.
    let input = Input {
        base: Some(ver("a\n", 1000)),
        ours: ver("", NEW),
        theirs: gone(OLD),
    };
    let m = merge(Kind::Markdown, &input, &Settings::default());
    assert_eq!(m.outcome, Outcome::Keep(Side::Ours));
}

#[test]
fn a_trailing_deletion_is_carried() {
    // Upstream gives a B c: ours' deletion of c is lost.
    let m = md("a\nb\nc\n", "a\nb\n", "a\nB\nc\n");
    assert_eq!(written(&m), "a\nB\n");
    assert_eq!(m.deviations, [Deviation::TrailingDeletion]);
    // Even against an unchanged other side, upstream brings c back.
    let m = md("a\nb\nc\nd\n", "a\nb\n", "a\nb\nc\nd\n");
    assert_eq!(written(&m), "a\nb\n");
    // Theirs' trailing deletion, ours newer.
    let m = run(Kind::Text, Some("a\nb\nc\n"), "A\nb\nc\n", "a\nb\n", true);
    assert_eq!(written(&m), "A\nb\n");
    assert_eq!(m.deviations, [Deviation::TrailingDeletion]);
    // A deletion that is not the last change was always carried.
    let m = md("a\nb\nc\n", "b\nc\n", "a\nb\nC\n");
    assert_eq!(written(&m), "b\nC\n");
    assert!(m.deviations.is_empty());
    // The same trailing deletion on both sides.
    let m = md("a\nb\nc\n", "A\nb\n", "a\nb\n");
    assert_eq!(written(&m), "A\nb\n");
    // A trailing deletion and an append after it: both carried.
    let m = md("a\nb\nc\n", "a\nb\n", "a\nb\nc\nd\n");
    assert_eq!(written(&m), "a\nb\nd\n");
    // Without a final newline.
    let m = md("a\nb\nc", "a\nb\n", "A\nb\nc");
    assert_eq!(written(&m), "A\nb\n");
}

#[test]
fn a_trailing_deletion_against_a_change_of_it_stays_open() {
    // Upstream gives a b C, taking the change over the deletion.
    let m = md("a\nb\nc\n", "a\nb\n", "a\nb\nC\n");
    assert!(m.outcome.is_open());
    assert!(m.deviations.is_empty(), "open is stricter, not a deviation");
}

/// A lost-update guard: base R1 (what a writer read), ours L (what it wrote),
/// theirs R2 (the revision it overwrote). "Clean and M = L" must mean L holds R2.
fn guard(r1: &str, l: &str, r2: &str) -> Option<String> {
    let input = Input {
        base: Some(ver(r1, 1000)),
        ours: ver(l, NEW),
        theirs: ver(r2, OLD),
    };
    match merge(Kind::Markdown, &input, &Settings::default()).outcome {
        Outcome::Write(b) => Some(String::from_utf8(b).unwrap()),
        Outcome::Identical => Some(l.to_string()),
        _ => None,
    }
}

#[test]
fn the_lost_update_guard_sees_a_lost_trailing_deletion() {
    let r1 = "# Todo\n- a\n- b\n- c\n";
    // R2 deleted the last item.
    let r2 = "# Todo\n- a\n- b\n";
    // A stale writer read R1 and added a line at the top: R2's deletion is lost.
    let l = "new\n# Todo\n- a\n- b\n- c\n";
    let m = guard(r1, l, r2).expect("clean");
    assert_ne!(m, l, "the guard must see that L lost R2's change");
    assert_eq!(m, "new\n# Todo\n- a\n- b\n");
    // A writer that saw R2: L already holds R2.
    let l = "new\n# Todo\n- a\n- b\n";
    assert_eq!(guard(r1, l, r2).as_deref(), Some(l));
    // The other way round: L deleted the last item of R1, R2 changed the top.
    let r2 = "# Todo!\n- a\n- b\n- c\n";
    let l = "# Todo\n- a\n- b\n";
    assert_eq!(guard(r1, l, r2).as_deref(), Some("# Todo!\n- a\n- b\n"));
    let l = "# Todo!\n- a\n- b\n";
    assert_eq!(guard(r1, l, r2).as_deref(), Some(l));
}

#[test]
fn the_lost_update_guard_sees_lost_appends_as_blocks() {
    let r1 = "# Log\n- 09:00 start\n";
    // R2 appended two lines; a stale writer appended two of its own.
    let r2 = "# Log\n- 09:00 start\n- 10:00 remote\n  detail\n";
    let l = "# Log\n- 09:00 start\n- 11:00 local\n  detail 2\n";
    let m = guard(r1, l, r2).expect("clean");
    assert_eq!(
        m,
        "# Log\n- 09:00 start\n- 10:00 remote\n  detail\n- 11:00 local\n  detail 2\n"
    );
    // A writer that saw R2 and appended after it: L already holds R2.
    let l = "# Log\n- 09:00 start\n- 10:00 remote\n  detail\n- 11:00 local\n  detail 2\n";
    assert_eq!(guard(r1, l, r2).as_deref(), Some(l));
}

// ---------- stable names ----------

#[test]
fn names_are_stable() {
    let rules: Vec<&str> = Rule::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(
        rules,
        [
            "identical",
            "edit_beats_delete",
            "both_deleted",
            "no_base",
            "base_deleted",
            "emptied",
            "lines",
            "frontmatter",
            "object",
            "unsupported"
        ]
    );
    let devs: Vec<&str> = Deviation::ALL.iter().map(|d| d.as_str()).collect();
    assert_eq!(
        devs,
        [
            "same_insertion_once",
            "glued_lines",
            "unparsable_json",
            "stricter_object_check",
            "object_result_differs",
            "numeric_key_order",
            "broken_frontmatter",
            "trailing_deletion",
            "run_order",
            "emptied_open",
            "position_order"
        ]
    );
    for r in Rule::ALL {
        assert_eq!(Rule::parse(r.as_str()), Some(*r));
        assert_eq!(r.to_string(), r.as_str());
    }
    for d in Deviation::ALL {
        assert_eq!(Deviation::parse(d.as_str()), Some(*d));
    }
    assert_eq!(Rule::parse("Lines"), None);
    let which: Vec<&str> = Which::ALL.iter().map(|w| w.as_str()).collect();
    assert_eq!(which, ["base", "ours", "theirs"]);
    let sides: Vec<&str> = Side::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(sides, ["ours", "theirs"]);
    let reasons = [
        (Reason::NoBase, "no_base"),
        (Reason::BaseDeleted, "base_deleted"),
        (Reason::BothDeleted, "both_deleted"),
        (Reason::Unsupported, "unsupported"),
        (Reason::NotUtf8(Which::Base), "not_utf8"),
        (Reason::InvalidJson(Which::Ours), "invalid_json"),
        (Reason::UnsupportedJson(Which::Theirs), "unsupported_json"),
        (Reason::JsonShape, "json_shape"),
        (Reason::InvalidFrontmatter(Which::Ours), "invalid_frontmatter"),
        (Reason::MergedFrontmatterInvalid, "merged_frontmatter_invalid"),
        (Reason::GluedLines, "glued_lines"),
        (Reason::Emptied(Which::Theirs), "emptied"),
    ];
    for (r, s) in reasons {
        assert_eq!(r.as_str(), s);
    }
    assert_eq!(Reason::InvalidJson(Which::Ours).to_string(), "invalid_json(ours)");
    assert_eq!(Reason::InvalidJson(Which::Ours).which(), Some(Which::Ours));
    assert_eq!(Reason::JsonShape.to_string(), "json_shape");
    assert_eq!(Reason::Emptied(Which::Ours).to_string(), "emptied(ours)");
    let overlaps = [
        (
            Overlap::Lines(LineOverlap {
                base: 0..0,
                ours: 0..0,
                theirs: 0..0,
            }),
            "lines",
        ),
        (Overlap::Key(Vec::new()), "key"),
        (Overlap::FrontmatterKey(None), "frontmatter_key"),
        (Overlap::FrontmatterOrder, "frontmatter_order"),
        (Overlap::Whole(Reason::NoBase), "whole"),
    ];
    for (o, s) in overlaps {
        assert_eq!(o.as_str(), s);
    }
    let outcomes = [
        (Outcome::Identical, "identical"),
        (Outcome::Keep(Side::Ours), "keep"),
        (Outcome::Write(Vec::new()), "write"),
        (Outcome::Open(Vec::new()), "open"),
    ];
    for (o, s) in outcomes {
        assert_eq!(o.as_str(), s);
    }
}

#[test]
fn equal_mtimes_put_ours_first() {
    let input = Input {
        base: Some(ver("a\n", 1000)),
        ours: ver("a\nours\n", OLD),
        theirs: ver("a\ntheirs\n", OLD),
    };
    assert_eq!(
        written(&merge(Kind::Text, &input, &Settings::default())),
        "a\nours\ntheirs\n"
    );
}

// ---------- frontmatter ----------

const FM_BASE: &str = "---\ntitle: Note\ntags:\n  - a\nstatus: draft\n---\nbody\n";

#[test]
fn frontmatter_merges_key_by_key() {
    let ours = "---\ntitle: Renamed\ntags:\n  - a\nstatus: draft\n---\nbody\n";
    let theirs = "---\ntitle: Note\ntags:\n  - a\n  - b\nstatus: draft\n---\nbody\nmore\n";
    let m = md(FM_BASE, ours, theirs);
    assert_eq!(m.rule, Rule::Frontmatter);
    assert_eq!(
        written(&m),
        "---\ntitle: Renamed\ntags:\n  - a\n  - b\nstatus: draft\n---\nbody\nmore\n"
    );
}

#[test]
fn a_key_changed_on_both_sides_stays_open() {
    let ours = "---\ntitle: Ours\ntags:\n  - a\nstatus: draft\n---\nbody\n";
    let theirs = "---\ntitle: Theirs\ntags:\n  - a\nstatus: done\n---\nbody\n";
    let m = md(FM_BASE, ours, theirs);
    assert_eq!(overlaps(&m), [Overlap::FrontmatterKey(Some("title".into()))]);
}

#[test]
fn newest_keys_take_the_newer_side() {
    let base = "---\nupdated: 2026-01-01\ntitle: a\n---\nbody\n";
    let ours = "---\nupdated: 2026-03-01\ntitle: b\n---\nbody\n";
    let theirs = "---\nupdated: 2026-02-01\ntitle: a\n---\nbody\nmore\n";
    let s = Settings {
        newest_keys: vec!["updated".into()],
        ..Settings::default()
    };
    assert!(md(base, ours, theirs).outcome.is_open(), "not listed: open");
    let m = run_with(Kind::Markdown, Some(base), ours, theirs, false, &s);
    assert_eq!(written(&m), "---\nupdated: 2026-02-01\ntitle: b\n---\nbody\nmore\n");
    assert_eq!(m.newest_taken, ["updated"]);
    let m = run_with(Kind::Markdown, Some(base), ours, theirs, true, &s);
    assert_eq!(written(&m), "---\nupdated: 2026-03-01\ntitle: b\n---\nbody\nmore\n");
    // Equal mtimes: the greater value, whichever side has it.
    let input = Input {
        base: Some(ver(base, 1000)),
        ours: ver(theirs, OLD),
        theirs: ver(ours, OLD),
    };
    assert_eq!(
        written(&merge(Kind::Markdown, &input, &s)),
        "---\nupdated: 2026-03-01\ntitle: b\n---\nbody\nmore\n"
    );
}

#[test]
fn keys_added_and_deleted() {
    let base = "---\na: 1\nb: 2\nc: 3\n---\n";
    let ours = "---\na: 1\nnew: x\nb: 2\nc: 3\n---\n";
    let theirs = "---\na: 1\nc: 4\n---\n";
    assert_eq!(written(&md(base, ours, theirs)), "---\na: 1\nnew: x\nc: 4\n---\n");
    // Deleted on one side, changed on the other.
    let m = md(base, "---\na: 1\nc: 3\n---\n", "---\na: 1\nb: 20\nc: 3\nd: 1\n---\n");
    assert_eq!(overlaps(&m), [Overlap::FrontmatterKey(Some("b".into()))]);
    // Added on both sides with different values.
    let m = md(
        base,
        "---\na: 1\nb: 2\nc: 3\nd: x\n---\n",
        "---\na: 0\nb: 2\nc: 3\nd: y\n---\n",
    );
    assert_eq!(overlaps(&m), [Overlap::FrontmatterKey(Some("d".into()))]);
}

#[test]
fn frontmatter_with_no_body() {
    let base = "---\na: 1\nb: 2\n---";
    let m = md(base, "---\na: 10\nb: 2\n---", "---\na: 1\nb: 20\n---");
    assert_eq!(written(&m), "---\na: 10\nb: 20\n---");
    // A body added on one side.
    let m = md(base, "---\na: 10\nb: 2\n---", "---\na: 1\nb: 20\n---\nbody\n");
    assert_eq!(written(&m), "---\na: 10\nb: 20\n---\nbody\n");
}

#[test]
fn comments_quotes_and_lists_survive() {
    let base = "---\n# a comment\n\"quoted key\": 1\nlist:\n- x\n- y\nother: 'v'\n---\n";
    let ours = "---\n# a comment\n\"quoted key\": 2\nlist:\n- x\n- y\nother: 'v'\n---\n";
    let theirs = "---\n# a comment\n\"quoted key\": 1\nlist:\n- x\n- y\n- z\nother: 'w'\n---\n";
    assert_eq!(
        written(&md(base, ours, theirs)),
        "---\n# a comment\n\"quoted key\": 2\nlist:\n- x\n- y\n- z\nother: 'w'\n---\n"
    );
}

#[test]
fn broken_yaml_stays_open() {
    let base = "---\na: 1\nb: 2\n---\n";
    let m = md(base, "---\na: [1\nb: 2\n---\n", "---\na: 1\nb: 3\n---\n");
    assert_eq!(overlaps(&m), [Overlap::Whole(Reason::InvalidFrontmatter(Which::Ours))]);
    // A key twice.
    let m = md(base, "---\na: 1\na: 2\nb: 2\n---\n", "---\na: 1\nb: 3\n---\n");
    assert_eq!(overlaps(&m), [Overlap::Whole(Reason::InvalidFrontmatter(Which::Ours))]);
    // Flow mappings are not split.
    let m = md("---\n{a: 1}\n---\n", "---\n{a: 2}\n---\n", "---\n{a: 3}\n---\n");
    assert_eq!(overlaps(&m), [Overlap::Whole(Reason::InvalidFrontmatter(Which::Base))]);
}

#[test]
fn frontmatter_changed_on_one_side_is_the_line_merge() {
    let theirs = "---\ntitle: Note\ntags:\n  - a\nstatus: draft\n---\nbody\nmore\n";
    let ours = "---\ntitle: New\ntags:\n  - a\nstatus: draft\n---\nbody\n";
    let m = md(FM_BASE, ours, theirs);
    assert_eq!(m.rule, Rule::Lines);
    assert_eq!(
        written(&m),
        "---\ntitle: New\ntags:\n  - a\nstatus: draft\n---\nbody\nmore\n"
    );
}

#[test]
fn frontmatter_added_on_both_sides_merges_key_by_key() {
    // Both add frontmatter to a note without: merged key by key against an
    // empty block, so `a` set differently is open, not two blocks of lines.
    let m = md("body\n", "---\na: 1\n---\nbody\n", "---\na: 2\n---\nbody\n");
    assert_eq!(m.rule, Rule::Frontmatter);
    assert_eq!(overlaps(&m), [Overlap::FrontmatterKey(Some("a".into()))]);
    let m = md("body\n", "---\na: 1\n---\nbody\n", "---\nb: 2\n---\nbody\nmore\n");
    assert_eq!(written(&m), "---\na: 1\nb: 2\n---\nbody\nmore\n");
    // A block one side already had, unparsable or not, is that side's.
    let m = md(
        "---\na: 1\n---\nbody\n",
        "---\na: 1\nb: [\n---\nbody\n",
        "---\na: 1\n---\nbody\nmore\n",
    );
    assert_eq!(written(&m), "---\na: 1\nb: [\n---\nbody\nmore\n");
}

#[test]
fn body_overlaps_are_in_file_lines() {
    let ours = "---\ntitle: A\ntags:\n  - a\nstatus: draft\n---\nours body\n";
    let theirs = "---\ntitle: Note\ntags:\n  - a\nstatus: done\nextra: 1\n---\ntheirs body\n";
    let m = md(FM_BASE, ours, theirs);
    assert_eq!(
        overlaps(&m),
        [Overlap::Lines(LineOverlap {
            base: 6..7,
            ours: 6..7,
            theirs: 7..8
        })]
    );
}

// ---------- JSON ----------

#[test]
fn json_equal_to_one_side_is_that_sides_bytes() {
    // Ours' change is the only one; ours' formatting (odd as it is) is kept.
    let b = "{\"a\": 1, \"b\": 1}\n";
    let o = "{ \"a\" : 1,\n   \"b\": 2 }";
    let t = "{\"a\": 1, \"b\": 1, \"c\": [] }\n";
    let m = run(Kind::Object, Some(b), o, "{\"a\": 1.0, \"b\": 1}\n", true);
    assert_eq!(written(&m), o);
    // Otherwise the merged value in a side's style.
    let m = run(Kind::Object, Some(b), o, t, false);
    assert!(written(&m).contains("\"c\""));
}

#[test]
fn json_number_changes_are_never_dropped() {
    let m = run(
        Kind::Object,
        Some("{\"a\": 1e400, \"b\": 0}"),
        "{\"a\": 2e400, \"b\": 0}",
        "{\"a\": 1e400, \"b\": -0}",
        false,
    );
    assert_eq!(written(&m), "{\"a\":2e400,\"b\":-0}");
    let m = run(Kind::Object, Some("{\"a\": 0}"), "{\"a\": -0}", "{\"a\": 1}", false);
    assert!(m.outcome.is_open());
}

const CANVAS: &str =
    "{\n\t\"nodes\": [\n\t\t{\n\t\t\t\"id\": \"n1\",\n\t\t\t\"text\": \"First\"\n\t\t}\n\t],\n\t\"edges\": []\n}";

fn canvas(nodes: &[(&str, &str)]) -> String {
    let n: Vec<String> = nodes
        .iter()
        .map(|(id, t)| format!("\t\t{{\n\t\t\t\"id\": \"{id}\",\n\t\t\t\"text\": \"{t}\"\n\t\t}}"))
        .collect();
    format!("{{\n\t\"nodes\": [\n{}\n\t],\n\t\"edges\": []\n}}", n.join(",\n"))
}

#[test]
fn canvas_nodes_merge_by_id_in_the_files_style() {
    let ours = canvas(&[("n1", "First"), ("n2", "Ours")]);
    let theirs = canvas(&[("n1", "Edited"), ("n3", "Theirs")]);
    let m = run(Kind::Object, Some(CANVAS), &ours, &theirs, false);
    assert_eq!(m.rule, Rule::Object);
    assert_eq!(
        written(&m),
        canvas(&[("n1", "Edited"), ("n2", "Ours"), ("n3", "Theirs")])
    );
    // Newer first: the older side's additions come first.
    let m = run(Kind::Object, Some(CANVAS), &ours, &theirs, true);
    assert_eq!(
        written(&m),
        canvas(&[("n1", "Edited"), ("n3", "Theirs"), ("n2", "Ours")])
    );
}

#[test]
fn a_node_changed_on_both_sides_stays_open() {
    let m = run(
        Kind::Object,
        Some(CANVAS),
        &canvas(&[("n1", "Ours")]),
        &canvas(&[("n1", "Theirs")]),
        false,
    );
    let path = vec![
        PathSeg::Key("nodes".into()),
        PathSeg::Id("n1".into()),
        PathSeg::Key("text".into()),
    ];
    assert_eq!(overlaps(&m), [Overlap::Key(path)]);
}

#[test]
fn nested_id_arrays() {
    let base = "{\"groups\":[{\"id\":\"g1\",\"items\":[{\"id\":\"i1\",\"v\":1}]}]}";
    let ours = "{\"groups\":[{\"id\":\"g1\",\"items\":[{\"id\":\"i1\",\"v\":10}]}]}";
    let theirs = "{\"groups\":[{\"id\":\"g1\",\"items\":[{\"id\":\"i1\",\"v\":1},{\"id\":\"i2\",\"v\":2}]},{\"id\":\"g2\",\"items\":[]}]}";
    assert_eq!(
        written(&run(Kind::Object, Some(base), ours, theirs, false)),
        "{\"groups\":[{\"id\":\"g1\",\"items\":[{\"id\":\"i1\",\"v\":10},{\"id\":\"i2\",\"v\":2}]},{\"id\":\"g2\",\"items\":[]}]}"
    );
    // Deleting an element against editing inside it.
    let m = run(
        Kind::Object,
        Some(base),
        ours,
        "{\"groups\":[{\"id\":\"g1\",\"items\":[]}]}",
        false,
    );
    let path = vec![
        PathSeg::Key("groups".into()),
        PathSeg::Id("g1".into()),
        PathSeg::Key("items".into()),
        PathSeg::Id("i1".into()),
    ];
    assert_eq!(overlaps(&m), [Overlap::Key(path)]);
}

#[test]
fn other_arrays_are_replaced_whole() {
    let base = "{\n  \"list\": [1, 2],\n  \"x\": 1\n}\n";
    let m = run(
        Kind::Object,
        Some(base),
        "{\"list\":[1,2,3],\"x\":1}",
        "{\"list\":[1,2],\"x\":2}",
        false,
    );
    assert_eq!(
        m.deviations,
        [Deviation::ObjectResultDiffers],
        "upstream conflicts here"
    );
    let m = run(
        Kind::Object,
        Some(base),
        "{\n  \"list\": [1, 2, 3],\n  \"x\": 1\n}\n",
        "{\n  \"list\": [1, 2],\n  \"x\": 2\n}\n",
        false,
    );
    assert_eq!(
        written(&m),
        "{\n  \"list\": [\n    1,\n    2,\n    3\n  ],\n  \"x\": 2\n}\n"
    );
    let m = run(
        Kind::Object,
        Some(base),
        "{\"list\":[1,2,3],\"x\":1}",
        "{\"list\":[0],\"x\":1}",
        false,
    );
    assert_eq!(overlaps(&m), [Overlap::Key(vec![PathSeg::Key("list".into())])]);
}

#[test]
fn json_keeps_its_own_layout() {
    // Compact stays compact, CRLF stays CRLF, a missing final newline stays missing,
    // numbers keep their text.
    let m = run(
        Kind::Object,
        Some("{\"a\":1.50,\"b\":2}"),
        "{\"a\":1.50,\"b\":3}",
        "{\"a\":1.50,\"b\":2,\"c\":1e3}",
        false,
    );
    assert_eq!(written(&m), "{\"a\":1.50,\"b\":3,\"c\":1e3}");
    let crlf = |s: &str| s.replace('\n', "\r\n");
    let m = run(
        Kind::Object,
        Some(&crlf("{\n  \"a\": 1,\n  \"b\": 2\n}\n")),
        &crlf("{\n  \"a\": 2,\n  \"b\": 2\n}\n"),
        &crlf("{\n  \"a\": 1,\n  \"b\": 3\n}\n"),
        false,
    );
    assert_eq!(written(&m), crlf("{\n  \"a\": 2,\n  \"b\": 3\n}\n"));
    let m = run(
        Kind::Object,
        Some("{\n    \"a\": 1\n}"),
        "{\n    \"a\": 2\n}",
        "{\n    \"a\": 1,\n    \"b\": \"\u{e6}\\\"\\n\"\n}",
        false,
    );
    assert_eq!(written(&m), "{\n    \"a\": 2,\n    \"b\": \"\u{e6}\\\"\\n\"\n}");
}

#[test]
fn json_layouts_that_differ_follow_the_newer_side() {
    let base = "{\"a\":1,\"b\":1}";
    let ours = "{\n  \"a\": 2,\n  \"b\": 1\n}\n";
    let theirs = "{\"a\":1,\"b\":2}";
    assert_eq!(
        written(&run(Kind::Object, Some(base), ours, theirs, true)),
        "{\n  \"a\": 2,\n  \"b\": 2\n}\n"
    );
    assert_eq!(
        written(&run(Kind::Object, Some(base), ours, theirs, false)),
        "{\"a\":2,\"b\":2}"
    );
}

#[test]
fn json_upstream_would_guess_at_stays_open() {
    let m = run(
        Kind::Object,
        Some("{\"a\":1}"),
        "{ not json",
        "{\"a\":1,\"b\":2}",
        false,
    );
    assert_eq!(overlaps(&m), [Overlap::Whole(Reason::InvalidJson(Which::Ours))]);
    assert_eq!(m.deviations, [Deviation::UnparsableJson]);
    let m = run(
        Kind::Object,
        Some("{\"a\":1}"),
        "{\"a\":1,\"a\":2}",
        "{\"a\":1,\"b\":2}",
        false,
    );
    assert_eq!(overlaps(&m), [Overlap::Whole(Reason::UnsupportedJson(Which::Ours))]);
    let m = run(Kind::Object, Some(""), "{\"a\":1}", "{\"b\":1}", false);
    assert_eq!(overlaps(&m), [Overlap::Whole(Reason::InvalidJson(Which::Base))]);
    // 1 against "1": upstream's `==` calls them equal.
    let m = run(Kind::Object, Some("{\"a\":0}"), "{\"a\":1}", "{\"a\":\"1\"}", false);
    assert_eq!(m.deviations, [Deviation::StricterObjectCheck]);
    assert!(m.outcome.is_open());
    // Two elements with one id.
    let m = run(
        Kind::Object,
        Some("{\"l\":[{\"id\":\"a\"}]}"),
        "{\"l\":[{\"id\":\"a\"},{\"id\":\"a\",\"v\":1}]}",
        "{\"l\":[]}",
        false,
    );
    assert_eq!(overlaps(&m), [Overlap::Whole(Reason::JsonShape)]);
}

#[test]
fn a_deleted_object_against_an_edit_inside_it_stays_open() {
    let base = "{\"p\":{\"a\":1,\"b\":2},\"q\":1}";
    let m = run(
        Kind::Object,
        Some(base),
        "{\"q\":1}",
        "{\"p\":{\"a\":1,\"b\":3},\"q\":1}",
        false,
    );
    assert_eq!(overlaps(&m), [Overlap::Key(vec![PathSeg::Key("p".into())])]);
}

// ---------- merge3 ----------

fn m3(kind: Kind, base: Option<&str>, ours: &str, theirs: &str) -> Merge3 {
    let input = Input {
        base: base.map(|b| ver(b, 1000)),
        ours: ver(ours, OLD),
        theirs: ver(theirs, NEW),
    };
    merge3(kind, &input, &Settings::default(), &Labels::default()).unwrap()
}

#[test]
fn merge3_marks_just_the_overlapping_hunks() {
    let r = m3(Kind::Text, Some("a\nb\nc\nd\n"), "A\nb\nX\nd\n", "a\nb\nY\nd\ne\n");
    assert_eq!(r.conflicts, 1);
    assert_eq!(
        r.text,
        "A\nb\n<<<<<<< ours\nX\n||||||| base\nc\n=======\nY\n>>>>>>> theirs\nd\ne\n"
    );
}

#[test]
fn merge3_of_a_clean_merge_is_the_merge() {
    let (b, o, t) = ("a\nb\nc\n", "A\nb\nc\n", "a\nb\nC\n");
    let r = m3(Kind::Text, Some(b), o, t);
    assert_eq!((r.conflicts, r.text.as_str()), (0, "A\nb\nC\n"));
}

#[test]
fn merge3_without_a_base_marks_every_difference() {
    let r = m3(Kind::Text, None, "same\nours\nend", "same\ntheirs\nend");
    assert_eq!(r.conflicts, 1);
    assert_eq!(
        r.text,
        "same\n<<<<<<< ours\nours\n||||||| base\n=======\ntheirs\n>>>>>>> theirs\nend"
    );
}

#[test]
fn merge3_marks_frontmatter_keys() {
    let ours = "---\ntitle: Ours\ntags:\n  - a\nstatus: draft\n---\nbody\n";
    let theirs = "---\ntitle: Theirs\ntags:\n  - a\nstatus: done\n---\nbody\n";
    let r = m3(Kind::Markdown, Some(FM_BASE), ours, theirs);
    assert_eq!(r.conflicts, 1);
    assert_eq!(
        r.text,
        "---\n<<<<<<< ours\ntitle: Ours\n||||||| base\ntitle: Note\n=======\ntitle: Theirs\n>>>>>>> theirs\ntags:\n  - a\nstatus: done\n---\nbody\n"
    );
}

#[test]
fn merge3_closes_a_last_line_without_newline_before_a_marker() {
    let r = m3(Kind::Text, Some("a\nb"), "a\nX", "a\nY");
    assert_eq!(
        r.text,
        "a\n<<<<<<< ours\nX\n||||||| base\nb\n=======\nY\n>>>>>>> theirs\n"
    );
    let crlf = m3(Kind::Text, Some("a\r\nb\r\n"), "a\r\nX\r\n", "a\r\nY\r\n");
    assert!(crlf.text.contains("<<<<<<< ours\r\nX\r\n||||||| base\r\n"));
}

#[test]
fn merge3_agrees_with_merge() {
    let cases = [
        ("a\nb\nc\n", "a\nB\nC\n", "a\nb\nc\nd\n"),
        ("a\nb\n", "", "a\nb\nc\n"),
        ("a\nb", "a\nb\nleft", "a\nb\nright"),
        ("---\na: 1\n---\nx\n", "---\na: [\n---\nx\n", "---\na: 2\n---\nx\n"),
        (
            "# M\nTBD\nend\n",
            "# M\nAt 10\nRoom 1\nend\n",
            "# M\nAt 10\nRoom 2\nend\n",
        ),
    ];
    for (b, o, t) in cases {
        let input = Input {
            base: Some(ver(b, 1000)),
            ours: ver(o, OLD),
            theirs: ver(t, NEW),
        };
        let m = merge(Kind::Markdown, &input, &Settings::default());
        let r = merge3(Kind::Markdown, &input, &Settings::default(), &Labels::default()).unwrap();
        match &m.outcome {
            Outcome::Write(w) => assert_eq!((r.conflicts, r.text.as_bytes()), (0, w.as_slice())),
            Outcome::Open(_) => assert!(r.conflicts >= 1, "{b:?} {o:?} {t:?}: {r:?}"),
            other => panic!("{other:?}"),
        }
    }
    // A whole-file reason is one whole-file hunk.
    let r = m3(Kind::Text, Some("a\nb\n"), "", "a\nb\nc\n");
    assert_eq!(r.conflicts, 1);
    assert_eq!(
        r.text,
        "<<<<<<< ours\n||||||| base\na\nb\n=======\na\nb\nc\n>>>>>>> theirs\n"
    );
}

#[test]
fn merge3_of_json_is_the_object_merges_view() {
    let b = "{\n  \"a\": 1,\n  \"b\": 1\n}\n";
    // Clean: the merged file.
    let r = m3(
        Kind::Object,
        Some(b),
        "{\n  \"a\": 2,\n  \"b\": 1\n}\n",
        "{\n  \"a\": 1,\n  \"b\": 2\n}\n",
    );
    assert_eq!((r.conflicts, r.text.as_str()), (0, "{\n  \"a\": 2,\n  \"b\": 2\n}\n"));
    // Open, though the lines would merge: one whole-file hunk.
    let r = m3(
        Kind::Object,
        Some(b),
        "{\n  \"a\": 2,\n  \"b\": 1\n}\n",
        "{\n  \"a\": \"2\",\n  \"b\": 1\n}\n",
    );
    assert_eq!(r.conflicts, 1);
    assert!(r.text.starts_with("<<<<<<< ours\n{"));
}

#[test]
fn too_large_stays_open() {
    let base: String = (0..MAX_LINES / 3 + 1).map(|i| format!("{i}\n")).collect();
    let ours = format!("{base}ours\n");
    let theirs = format!("x\n{base}");
    let m = run(Kind::Text, Some(&base), &ours, &theirs, false);
    assert_eq!(overlaps(&m), [Overlap::Whole(Reason::TooLarge)]);
    assert_eq!(m3(Kind::Text, Some(&base), &ours, &theirs).conflicts, 1);
    let big = "x".repeat(MAX_BYTES / 2);
    let m = run(
        Kind::Object,
        Some("{}"),
        &format!("{{\"a\":\"{big}\"}}"),
        &format!("{{\"b\":\"{big}\"}}"),
        false,
    );
    assert_eq!(overlaps(&m), [Overlap::Whole(Reason::TooLarge)]);
}

#[test]
fn merge3_has_nothing_for_binaries_or_deletes() {
    let input = Input {
        base: Some(ver("a", 1000)),
        ours: gone(OLD),
        theirs: ver("b", NEW),
    };
    assert!(merge3(Kind::Text, &input, &Settings::default(), &Labels::default()).is_none());
    let input = Input {
        base: Some(ver("a", 1000)),
        ours: ver("b", OLD),
        theirs: ver("c", NEW),
    };
    assert!(merge3(Kind::Other, &input, &Settings::default(), &Labels::default()).is_none());
}
