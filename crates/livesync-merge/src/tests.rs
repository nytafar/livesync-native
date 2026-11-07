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
fn trailing_deletion_becomes_equal_lines_as_upstream() {
    // splitDiffPiece: a deletion with nothing after it is pushed as EQUAL.
    assert_eq!(
        lines::split_for_test("a\nb\nc\n", "a\nb\n"),
        vec![(0, "a\n".into()), (0, "b\n".into()), (0, "c\n".into())]
    );
}

// ---------- rule 1: identical ----------

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

// ---------- rule 5: delete against edit ----------

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
    // It needs no ancestor (D9: without one only rules 1 and 5 apply), and it
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

// ---------- rule 2: lines ----------

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
fn two_inserted_runs_interleave_as_upstream() {
    let m = run(
        Kind::Text,
        Some("# Log\n"),
        "# Log\nours 1\nours 2\n",
        "# Log\ntheirs 1\ntheirs 2\n",
        false,
    );
    assert_eq!(written(&m), "# Log\nours 1\ntheirs 1\nours 2\ntheirs 2\n");
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
    // A blank line both added, around different text.
    let m = md("x\n", "x\nours\n\n", "x\n\ntheirs\n");
    assert_eq!(written(&m), "x\nours\n\ntheirs\n");
}

#[test]
fn the_same_lines_inserted_in_another_order_stay_open() {
    let m = md("a\n", "a\nP\nQ\n", "a\nQ\nP\n");
    assert_eq!(m.deviations, [Deviation::SameInsertionOnce]);
    assert_eq!(
        overlaps(&m),
        [Overlap::Lines(LineOverlap {
            base: 1..1,
            ours: 1..3,
            theirs: 1..3
        })]
    );
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
    // Emptied on one side while the other edits: upstream keeps the edit (its
    // diff of an emptied file is one trailing deletion, carried as equal lines).
    assert_eq!(written(&md("a\nb\n", "", "a\nB\n")), "a\nB\n");
    assert_eq!(written(&md("", "", "new\n")), "new\n");
}

#[test]
fn a_trailing_deletion_is_not_carried_as_upstream() {
    // Kept from upstream on purpose; see the crate docs.
    assert_eq!(written(&md("a\nb\nc\n", "a\nb\n", "a\nB\nc\n")), "a\nB\nc\n");
    // A deletion that is not the last change is carried.
    assert_eq!(written(&md("a\nb\nc\n", "b\nc\n", "a\nb\nC\n")), "b\nC\n");
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

// ---------- rule 3: frontmatter ----------

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
fn a_line_merge_that_breaks_the_frontmatter_stays_open() {
    // Both add a key at the same place by lines: no frontmatter in the base, so
    // rule 3 does not apply, and the line merge would write `a` twice.
    let m = md("body\n", "---\na: 1\n---\nbody\n", "---\na: 2\n---\nbody\n");
    assert_eq!(m.deviations, [Deviation::BrokenFrontmatter]);
    assert_eq!(overlaps(&m), [Overlap::Whole(Reason::MergedFrontmatterInvalid)]);
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

// ---------- rule 4: JSON ----------

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
