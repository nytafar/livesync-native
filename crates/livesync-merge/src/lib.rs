//! Conflict merging for Obsidian vaults synced with Self-hosted LiveSync, as a
//! pure library: no I/O, no network, no engine. A conflict resolver calls
//! [`merge`]; a lost-update guard calls it with the revision a writer started
//! from as the base, the writer's file as ours and the revision it overwrote
//! as theirs; `lsync conflicts show` calls [`merge3`].
//!
//! Given a base (or none), ours and theirs, each with content, mtime and the
//! deleted flag, [`merge`] returns which rule decided ([`Rule`], for the log)
//! and the outcome: the two sides are the same, keep one side as it is, write
//! new content, or open with the overlaps that stop it. The guiding rule: an
//! automatic merge never drops or garbles a line either side added, and never
//! brings back one a side deleted; any doubt stays open for a person or an
//! agent. The rules apply in order:
//!
//! 1. **Identical** content and deleted flag: [`Outcome::Identical`].
//! 2. **Deleted against live**: the live side is kept, [`Outcome::Keep`], with
//!    or without a base. A delete can be repeated; a lost edit cannot be
//!    recovered. Two deleted sides with different data stay open.
//! 3. **No base** (or a deleted base): open.
//! 4. **Emptied**: a side with no content (or only whitespace) against a base
//!    with content: open (see below).
//! 5. **Too large**: a merge whose work would pass [`MAX_COST`] steps (a
//!    deterministic count, the same on every machine), or a JSON file past
//!    [`MAX_BYTES`], is open ([`Reason::TooLarge`]).
//! 6. **Markdown and text** ([`Kind::Markdown`], [`Kind::Text`]): a three-way
//!    line merge (below).
//! 7. **Frontmatter** ([`Kind::Markdown`] only), when both sides changed the
//!    YAML block, each differently: key by key. A key changed on one side is
//!    taken; a key changed on both sides differently stays open, unless it is
//!    listed in [`Settings::newest_keys`], where the newer side's value is taken.
//!    A base without frontmatter counts as an empty block. The body merges by
//!    lines. When only one side changed the block, 6 applies to the file.
//! 8. **`.json`/`.canvas`** ([`Kind::Object`]): an object merge: a patch per
//!    side against the base; arrays whose elements all have an `id` merge by
//!    id, other arrays are replaced whole; a key changed on both sides stays
//!    open. The result keeps the file's own formatting.
//! 9. **Anything else** ([`Kind::Other`]): open.
//!
//! Which paths are which kind is a setting ([`Settings::kind_of`]), as is the
//! list of timestamp keys.
//!
//! # The line merge
//!
//! Each side is diffed against the base by lines (diff-match-patch's line
//! mode), and each diff becomes changes: base lines `a..b` became these lines.
//!
//! - A change only one side made is taken; the same change on both sides once.
//! - Changes are placed by base position: a line one side inserted between
//!   base lines `i` and `i + 1` comes after everything the other side made of
//!   lines up to `i`, and before everything it made of lines from `i + 1`. A
//!   line replaced on one side keeps a line the other side added under it
//!   beneath it, and a multi-line replacement stays one block.
//! - Two insertions at one point: the same lines are taken once, and so is a
//!   run that the other holds as a whole prefix or suffix (the longer is
//!   taken); otherwise both runs go in whole, the older side's first, so two
//!   appends to a log both survive intact (two fenced code blocks, each
//!   whole). But where the runs are likely one, it is open: one run has all
//!   the other's lines (not blank) in any order (a writer that saw R2's
//!   appended lines and put a line between them), or one run is only blank
//!   lines (the other side may have typed onto them). The same holds for two
//!   insertions with only unchanged lines between them, compared with those
//!   lines (a blank line both added, one above a blank line and one below it,
//!   once).
//! - Where a side's insertion meets the other side's change (at its start or
//!   end), the shared lines are taken once where the change's lines end (or
//!   begin) with exactly the insertion's, at the end where they meet (L marked
//!   a log entry done and appended the line R2 appended: L). Not at the
//!   change's far end (a paragraph break one side added before an edited line
//!   and the other after it are two). Where the insertion holds the whole
//!   change instead (L ticked a task and added it anew; R2 only ticked it),
//!   the inserting side may have made the same change and typed the old line
//!   again: open.
//! - Two different changes of overlapping lines stay open, also where one's
//!   lines begin with the other's (both ticked the second task, one also the
//!   first): no text says which of a changed block's lines is which base
//!   line's, so one side may have deleted a line the other edited.
//! - Overlapping deletions on both sides: every line either deleted goes
//!   (where each side keeps, beside its own deletion, a line the other
//!   deleted, the check below leaves it open).
//! - Everything else where the sides' changes touch the same base lines stays
//!   open: two different replacements of overlapping lines (whole replacements
//!   are compared, not their first lines), a deletion against a change, an
//!   insertion strictly inside lines the other side replaced. So do two pieces
//!   of the two sides that meet at one point and share any other line, a
//!   blank line too: which copy is whose cannot be told (one side's note and
//!   the other's same note plus one more, after an edited line; two code
//!   blocks sharing a fence; a paragraph break one side added before a line
//!   and one the other added around its edit of it), and taking both would
//!   put a line in twice.
//! - The base's last line without its newline is kept by a side that only
//!   added one (that is how a line is appended after it); where both sides
//!   keep it, the copy with the newline is used. A line a side inserted as its
//!   file's last line without a newline is the same line as one with it.
//! - A result that would join a line without its newline to the next stays
//!   open.
//! - The merge above only proposes. Every clean result is decided by a check
//!   on what the three files say, with no alignment of the merge's: lines
//!   each file has once (anchors) are the same line in all three, so between
//!   them a stretch only one side changed must come out as that side's, byte
//!   for byte. A stretch both sides changed is clean only where every way the
//!   texts can be read (every longest common subsequence of each side with
//!   the base) merges by the placement rules, all to the same text, blank
//!   lines and `---` included, and the merge is that text; otherwise it is a
//!   conflict hunk of its own. It is also open where a side may be the other
//!   side's file with lines of its own on top, or with a line the other
//!   deleted typed back right beside its own change (both readings are
//!   plausible, and they disagree); where a line one side moved meets a
//!   change of the other side: of the line itself, right around where it
//!   left, or right around where it arrived where the lines it passed may
//!   have moved instead (a line moved to where the other side inserted, with
//!   no such rival reading, is two insertions at one point); where a line
//!   both sides added would be there more often than either side has it, or
//!   with only blank lines between the copies, from the two sides at two
//!   places in one stretch (one insertion read at two points: a paragraph
//!   break or a deleted line between); where a line
//!   would be there more often (or less often) than any side's change
//!   explains; where the merge would mix `\r\n` and `\n` line ends that no
//!   file mixes. A lost-update guard reads
//!   "clean and equal to L" as "L holds R2", so a line taken twice, dropped or
//!   brought back must never be clean. The price: a line one side deleted
//!   right beside the other side's change (an append after it, an edit of the
//!   next line) stays open, as git's merge does; and where the texts can be
//!   read two ways that merge differently (a copy inserted next to its twin,
//!   a paragraph added next to a blank line where the other side inserted),
//!   it stays open rather than guessing.
//!
//! # Where this differs from upstream, and why
//!
//! Upstream is commonlib's `ConflictManager` (`@vrtmrz/livesync-commonlib`,
//! pinned in `tools/golden`). A corpus generated by upstream's own code
//! (`fixtures/golden/merge.json`) is the oracle: every case must give
//! upstream's result, or be open where upstream merges (stricter), or carry a
//! [`Deviation`] whose shape the corpus test checks. Every line merge that
//! writes, labelled or not, must also pass a placement check that knows
//! nothing of how it was made: each side's added lines once and in that
//! side's order, the base lines both kept, none a side deleted, pieces of the
//! two sides in base order, no overlap merged, nothing glued. So a label never
//! stands in for a right merge. This crate also holds a
//! model of upstream's line and object merges, which the corpus test checks
//! case by case against upstream's output, and each line-merge result is
//! compared with that model to report its deviations. Never garbling a file
//! outranks matching upstream. The deviations:
//!
//! - **Changes placed by base position** ([`Deviation::PositionOrder`]).
//!   Upstream walks both diffs line by line, so a side's insertion can land in
//!   the middle of the other side's replacement (base `a b c`, ours `a B C`,
//!   theirs adds `d` after `c`: upstream can give `a B d C`), a sub-item added
//!   under a ticked checkbox can come out above it, and two different
//!   replacements that start with the same line merge into a mix of both.
//!   Here placement follows base positions, and the last case is open.
//!   Reported, too, where this merge merges and upstream gives up.
//! - **Two runs inserted at one point go in whole, the older side's first**
//!   ([`Deviation::RunOrder`]). Upstream pairs them line by line (ours `A B`
//!   and theirs `C D` after one line give `A C B D`), which garbles two
//!   multi-line appends. Where whole runs alone would join a line without its
//!   newline to the next (a run ending a file without one, going first), it
//!   is open.
//! - **The same insertion on both sides is taken once**
//!   ([`Deviation::SameInsertionOnce`]): where one run equals the other or
//!   holds it as a whole prefix or suffix, at one point, beside the other
//!   side's change, or across unchanged lines. Upstream's pairing can take a
//!   line twice: `[X]` against `[Y, X]` gives `X Y X`; here `Y X`; one side's
//!   ticked task with a note against the other's same note gives the note
//!   twice, above the task; here once, below it.
//! - **A trailing deletion is carried** ([`Deviation::TrailingDeletion`]).
//!   Upstream turns a deletion that is the last change of a side's diff into
//!   unchanged lines: base `a b c`, ours `a b`, theirs `a B c` merges to
//!   `a B c`, and ours' deletion is lost. Here it is carried like any other
//!   (`a B`). A lost-update guard depends on it: with a lost trailing
//!   deletion, "the merge equals the writer's file" would wrongly say the
//!   writer's file already holds the other revision.
//! - **An emptied side never merges** ([`Deviation::EmptiedOpen`],
//!   [`Rule::Emptied`]). Where the base has content and a side is empty (no
//!   bytes, or only spaces, tabs and line ends), the merge is open whatever the
//!   other side did, an unchanged side included. An accidental truncation (an
//!   editor crash, a stray `> file`, a partial write) is the most destructive
//!   one-sided change and the commonest glitch, so a person confirms it. Both
//!   sides emptied alike is rule 1; a deleted leaf is not emptied (rule 2).
//! - **No glued lines** ([`Deviation::GluedLines`]). Where a line follows a
//!   line that has no newline (two appends after an unterminated last line),
//!   upstream joins them: `left` and `right` become `leftright`. Here that is
//!   open (or, where whole runs avoid it, merged). Notes without a final
//!   newline are common, so this is not rare.
//! - **Upstream's truncated merge is not copied.** Its walk has an exit that
//!   returns what it merged so far as the result; this merge has no such exit.
//! - **JSON is written in the file's own style** (indent, line ends, final
//!   newline; the newer side's where the sides differ), where upstream writes
//!   `JSON.stringify(v)` unindented; where the merged value is one side's, that
//!   side's bytes are returned as they are. Numbers keep their source text
//!   (upstream rounds integers past 2^53), and a change is never dropped:
//!   `1e400` against `2e400`, or `0` against `-0`, is a change. Keys keep their
//!   order (JavaScript moves integer-like keys and ids first,
//!   [`Deviation::NumericKeyOrder`]).
//! - **JSON that upstream would guess at stays open**
//!   ([`Deviation::UnparsableJson`]): invalid JSON (upstream reads it as `{}`,
//!   i.e. everything deleted), a key twice in one object, lone surrogates, ids
//!   that are not strings or plain integers, two elements with one id.
//! - **JSON overlaps upstream misses stay open**
//!   ([`Deviation::StricterObjectCheck`]): upstream compares changed leaves by
//!   dotted path with JavaScript's `==`, so `1` against `"1"` is no conflict,
//!   and neither is a deleted (or replaced) object against an edit inside it,
//!   or two different replacements of one value; each loses one side.
//! - **A plain array changed on one side is taken**
//!   ([`Deviation::ObjectResultDiffers`]). Upstream's patch swaps every plain
//!   array whole even when it did not change, so a one-sided change conflicts
//!   upstream, and an unchanged array's swap can revert the other side's
//!   change to that key.
//! - **Frontmatter** merges key by key (rule 7), which upstream does not do at
//!   all ([`Rule::Frontmatter`]); where its result differs from upstream's line
//!   merge of the file, merged or open, that is
//!   [`Deviation::FrontmatterByKey`]. (Upstream's merge for the label has a
//!   budget of its own, so the label never makes a merge too large; where it
//!   does not fit, there is no label.) Both sides changing the block with
//!   fences that end their lines differently stays open
//!   ([`Reason::FrontmatterFences`]): the line merge would put one block in
//!   the body. A line merge that would leave a frontmatter block the YAML
//!   parser does not understand stays open ([`Deviation::BrokenFrontmatter`]).
//! - **Work is counted, not guessed from size.** Upstream gives its diff one
//!   second of wall time, then falls back to delete-all/insert-all, so its
//!   result depends on the machine. Here the work is counted against
//!   [`MAX_COST`] and a merge past it is open.
//!
//! Kept from upstream on purpose, though it surprises: with equal mtimes, ours
//! goes first (upstream's `<=`), so only distinct mtimes make the merge
//! symmetric.
//!
//! # Stable names
//!
//! [`Rule`], [`Deviation`], [`Reason`], [`Overlap`], [`Outcome`], [`Which`]
//! and [`Side`] have an `as_str` form (snake case), for a resolver's log and
//! JSON output. These strings are an interface: a variant may be added, but an
//! existing string never changes meaning or spelling. [`Rule::parse`] and
//! [`Deviation::parse`] read them back.

mod check;
mod dmp;
mod frontmatter;
mod json;
mod lines;

use dmp::{Budget, OutOfBudget};
pub use lines::Labels;
use std::ops::Range;

/// One side of a conflict, or the base.
#[derive(Clone, Copy, Debug)]
pub struct Version<'a> {
    pub content: &'a [u8],
    /// Milliseconds, as LiveSync stores it. Only the order matters.
    pub mtime: i64,
    pub deleted: bool,
}

/// What to merge. Ours is upstream's left (the winning revision in the
/// resolver; the local file in the lost-update guard), theirs its right.
#[derive(Clone, Copy, Debug)]
pub struct Input<'a> {
    pub base: Option<Version<'a>>,
    pub ours: Version<'a>,
    pub theirs: Version<'a>,
}

/// How a file is merged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Line merge, with the frontmatter rule.
    Markdown,
    /// Line merge.
    Text,
    /// JSON object merge.
    Object,
    /// Not merged.
    Other,
}

/// Per-vault merge settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Path suffixes merged as Markdown. Default `.md`.
    pub markdown: Vec<String>,
    /// Path suffixes merged as plain text. Default `.txt`.
    pub text: Vec<String>,
    /// Path suffixes merged as JSON. Default `.json`, `.canvas`.
    pub object: Vec<String>,
    /// Frontmatter keys that take the newer side's value when both sides
    /// changed them (`merge.newest_keys`). Empty by default.
    pub newest_keys: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            markdown: vec![".md".into()],
            text: vec![".txt".into()],
            object: vec![".json".into(), ".canvas".into()],
            newest_keys: Vec::new(),
        }
    }
}

impl Settings {
    /// The kind of a path, by suffix. Case-sensitive, as upstream's
    /// `endsWith(".md")`; Markdown wins over text over object.
    pub fn kind_of(&self, path: &str) -> Kind {
        let has = |list: &[String]| list.iter().any(|s| path.ends_with(s.as_str()));
        if has(&self.markdown) {
            Kind::Markdown
        } else if has(&self.text) {
            Kind::Text
        } else if has(&self.object) {
            Kind::Object
        } else {
            Kind::Other
        }
    }
}

/// The base, ours or theirs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Which {
    Base,
    Ours,
    Theirs,
}

/// Ours or theirs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Ours,
    Theirs,
}

/// The rule that decided, for the resolver's log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// Same content, same deleted flag.
    Identical,
    /// Deleted on one side, live on the other.
    EditBeatsDelete,
    /// Both sides deleted, with different data: open.
    BothDeleted,
    /// No common ancestor: open.
    NoBase,
    /// The ancestor is a deleted leaf: open.
    BaseDeleted,
    /// A side was emptied (the base has content): open.
    Emptied,
    /// The three-way line merge.
    Lines,
    /// Frontmatter key by key, body by lines.
    Frontmatter,
    /// The JSON object merge.
    Object,
    /// Binary or any other kind: open.
    Unsupported,
}

/// The result of [`merge`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merge {
    pub rule: Rule,
    pub outcome: Outcome,
    /// Where this result knowingly differs from what upstream would do with
    /// the same input (see the crate docs). Empty when it is upstream's.
    pub deviations: Vec<Deviation>,
    /// Frontmatter keys decided by `newest_keys` (the older side's value lost).
    pub newest_taken: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Both sides hold the same: keep either leaf, tombstone the other, write
    /// nothing new.
    Identical,
    /// Keep this side's leaf as it is (its content, its deleted flag) and
    /// tombstone the other.
    Keep(Side),
    /// The merged content, to store on the winning leaf.
    Write(Vec<u8>),
    /// Not merged automatically: what overlaps.
    Open(Vec<Overlap>),
}

impl Outcome {
    pub fn is_open(&self) -> bool {
        matches!(self, Outcome::Open(_))
    }
}

/// Why a merge stays open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Overlap {
    /// Lines both sides changed differently, or one deleted and the other
    /// changed. Ranges are 0-based, half-open line indices into each file.
    Lines(LineOverlap),
    /// A JSON path both sides changed differently (the shorter, where one side
    /// changed a value and the other something inside it).
    Key(Vec<PathSeg>),
    /// A frontmatter key both sides changed differently. `None` is the comment
    /// or blank lines before the first key.
    FrontmatterKey(Option<String>),
    /// The frontmatter keys were reordered differently on both sides.
    FrontmatterOrder,
    /// The whole file.
    Whole(Reason),
}

/// Lines of one overlap, as 0-based half-open line ranges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineOverlap {
    pub base: Range<usize>,
    pub ours: Range<usize>,
    pub theirs: Range<usize>,
}

/// One step of a JSON path: an object key, or an element of an id-array.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PathSeg {
    Key(String),
    Id(String),
}

/// Why a whole file stays open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    NoBase,
    BaseDeleted,
    BothDeleted,
    /// Binary, or a kind that is not merged.
    Unsupported,
    NotUtf8(Which),
    /// Not JSON.
    InvalidJson(Which),
    /// JSON this merge does not take on (see [`Deviation::UnparsableJson`]).
    UnsupportedJson(Which),
    /// The JSON's shape stops the merge (an id-array with odd ids, a value
    /// changed in a way the patch cannot apply).
    JsonShape,
    /// A frontmatter block the merge does not understand.
    InvalidFrontmatter(Which),
    /// The merged frontmatter would not parse.
    MergedFrontmatterInvalid,
    /// The merge would join two lines into one (see [`Deviation::GluedLines`]).
    GluedLines,
    /// This side is empty, or only whitespace, where the base has content.
    /// Both sides emptied (differently) names ours.
    Emptied(Which),
    /// The merge would do more work than [`MAX_COST`] allows, or the JSON is
    /// larger than [`MAX_BYTES`].
    TooLarge,
    /// Both sides changed the frontmatter, and their fences end their lines
    /// differently (`\r\n` and `\n`).
    FrontmatterFences,
}

/// A place where this crate's result differs from upstream's by design (see
/// the crate docs). Reported so the resolver can log it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Deviation {
    /// A line both sides inserted is taken once where upstream takes it twice
    /// or elsewhere: two runs at one point, an insertion beside the other
    /// side's change, the same change inside a larger one, or two insertions
    /// across unchanged lines.
    SameInsertionOnce,
    /// Upstream would join two lines into one: open.
    GluedLines,
    /// JSON upstream would read as `{}` or take on with a guess: open.
    UnparsableJson,
    /// A JSON overlap upstream does not see (it would drop one side): open.
    StricterObjectCheck,
    /// A JSON merge where upstream would conflict or give another result.
    ObjectResultDiffers,
    /// Upstream (JavaScript) would list integer-like keys or ids first.
    NumericKeyOrder,
    /// The line merge would leave frontmatter that does not parse: open.
    BrokenFrontmatter,
    /// A deletion at the end of a side's diff is carried (upstream brings the
    /// lines back), or is open against a change of the same lines.
    TrailingDeletion,
    /// Two runs inserted at one point are placed whole, the older first, where
    /// upstream interleaves them line by line (or open, where that order would
    /// join two lines).
    RunOrder,
    /// A side was emptied: open, where upstream would merge.
    EmptiedOpen,
    /// Changes placed by their base position where upstream's walk puts
    /// them elsewhere, or merged where upstream gives up.
    PositionOrder,
    /// Both sides changed the frontmatter: merged key by key, or open on a
    /// key, the key order or a block the merge does not take on, where
    /// upstream's line merge gives another result.
    FrontmatterByKey,
}

fn open(rule: Rule, overlaps: Vec<Overlap>) -> Merge {
    Merge {
        rule,
        outcome: Outcome::Open(overlaps),
        deviations: Vec::new(),
        newest_taken: Vec::new(),
    }
}

fn utf8<'a>(v: &Version<'a>, w: Which) -> Result<&'a str, Reason> {
    std::str::from_utf8(v.content).map_err(|_| Reason::NotUtf8(w))
}

/// The most JSON an object merge takes on, the base and both sides together:
/// past it, the merge is open ([`Reason::TooLarge`]). The object merge's work
/// grows with the bytes and no faster (about 100 ms a MiB at worst on a 2020s
/// desktop, release build), so its size bounds it.
pub const MAX_BYTES: usize = 1 << 20;
/// The most work a line merge (with its frontmatter) or a `merge3` view may
/// do, in steps: lines and bytes read, lines compared, diagonals searched by
/// the diff. Past it, the merge is open ([`Reason::TooLarge`]). The count does
/// not depend on the machine, so the same input always gives the same answer.
///
/// A long file where each side changed a little costs about its length: a
/// 30 000-line log where each side appended a line takes about 2.5 million
/// steps (about 20 ms on a 2020s desktop, release build). The diff's worst
/// case, both sides rewritten, grows with the square of the changed lines and
/// runs out at about 2 500 of them; a merge that runs out takes at most about
/// 100 ms. (The diff, as upstream's, also lumps everything past 40 000
/// distinct lines of the base into one line, so such a file merges only
/// where the change is in its first 40 000 lines.)
pub const MAX_COST: u64 = 15_000_000;

/// What [`merge3`] shows for a merge, built with it so the two always agree.
enum View {
    /// Nothing to show as text.
    Nothing,
    /// The file as it is (identical sides, or a clean merge).
    Text(String),
    /// The line walk, with its conflict hunks.
    Walk(lines::Walk),
    /// The whole file as one conflict.
    Whole,
    /// No base: every difference between the sides.
    TwoWay,
}

/// Merge `input` as `kind`. See the crate docs for the rules.
pub fn merge(kind: Kind, input: &Input, settings: &Settings) -> Merge {
    decide(kind, input, settings).0
}

fn too_large(input: &Input) -> bool {
    let all = [
        input.base.map(|b| b.content).unwrap_or_default(),
        input.ours.content,
        input.theirs.content,
    ];
    all.iter().map(|c| c.len()).sum::<usize>() > MAX_BYTES
}

fn too_large_open(rule: Rule) -> (Merge, View) {
    (open(rule, vec![Overlap::Whole(Reason::TooLarge)]), View::Whole)
}

fn decide(kind: Kind, input: &Input, settings: &Settings) -> (Merge, View) {
    let (ours, theirs) = (&input.ours, &input.theirs);
    if ours.content == theirs.content && ours.deleted == theirs.deleted {
        let view = match std::str::from_utf8(ours.content) {
            Ok(s) if !ours.deleted && kind != Kind::Other => View::Text(s.to_string()),
            _ => View::Nothing,
        };
        let m = Merge {
            rule: Rule::Identical,
            outcome: Outcome::Identical,
            deviations: Vec::new(),
            newest_taken: Vec::new(),
        };
        return (m, view);
    }
    match (ours.deleted, theirs.deleted) {
        (true, true) => {
            return (
                open(Rule::BothDeleted, vec![Overlap::Whole(Reason::BothDeleted)]),
                View::Nothing,
            )
        }
        (true, false) | (false, true) => {
            let m = Merge {
                rule: Rule::EditBeatsDelete,
                outcome: Outcome::Keep(if ours.deleted { Side::Theirs } else { Side::Ours }),
                deviations: Vec::new(),
                newest_taken: Vec::new(),
            };
            return (m, View::Nothing);
        }
        (false, false) => {}
    }
    let two_way = if kind == Kind::Other {
        View::Nothing
    } else {
        View::TwoWay
    };
    let base = match input.base {
        None => return (open(Rule::NoBase, vec![Overlap::Whole(Reason::NoBase)]), two_way),
        Some(b) if b.deleted => {
            return (
                open(Rule::BaseDeleted, vec![Overlap::Whole(Reason::BaseDeleted)]),
                two_way,
            )
        }
        Some(b) => b,
    };
    let whole = if kind == Kind::Other {
        View::Nothing
    } else {
        View::Whole
    };
    let blank = |v: &Version| v.content.iter().all(u8::is_ascii_whitespace);
    if !blank(&base) && (blank(ours) || blank(theirs)) {
        let which = if blank(ours) { Which::Ours } else { Which::Theirs };
        let mut m = open(Rule::Emptied, vec![Overlap::Whole(Reason::Emptied(which))]);
        if kind != Kind::Other {
            m.deviations.push(Deviation::EmptiedOpen);
        }
        return (m, whole);
    }
    let rule = match kind {
        Kind::Markdown | Kind::Text => Rule::Lines,
        Kind::Object => Rule::Object,
        Kind::Other => {
            return (
                open(Rule::Unsupported, vec![Overlap::Whole(Reason::Unsupported)]),
                View::Nothing,
            )
        }
    };
    let texts = (|| {
        Ok::<_, Reason>((
            utf8(&base, Which::Base)?,
            utf8(ours, Which::Ours)?,
            utf8(theirs, Which::Theirs)?,
        ))
    })();
    let (b, o, t) = match texts {
        Ok(x) => x,
        Err(r) => return (open(rule, vec![Overlap::Whole(r)]), View::Nothing),
    };
    let ours_first = ours.mtime <= theirs.mtime;
    let budget = &mut Budget::new(MAX_COST);
    match kind {
        Kind::Object if too_large(input) => too_large_open(rule),
        Kind::Object => {
            let m = merge_object(b, o, t, ours, theirs, ours_first);
            let view = match &m.outcome {
                Outcome::Write(out) => View::Text(String::from_utf8(out.clone()).expect("UTF-8")),
                _ => View::Whole,
            };
            (m, view)
        }
        Kind::Markdown => {
            match merge_frontmatter(b, o, t, ours, theirs, settings, budget) {
                Ok(Some(r)) => return r,
                Ok(None) => {}
                Err(OutOfBudget) => return too_large_open(Rule::Frontmatter),
            }
            let (mut m, mut view) = merge_lines(b, o, t, ours_first, budget);
            if let Outcome::Write(out) = &m.outcome {
                let out = std::str::from_utf8(out).expect("merged from UTF-8");
                if let Some(d) = frontmatter::split(out) {
                    let known = [b, o, t]
                        .iter()
                        .any(|s| frontmatter::split(s).is_some_and(|x| x.block == d.block));
                    if !known && !frontmatter::understood(d.block) {
                        m.deviations.push(Deviation::BrokenFrontmatter);
                        m.outcome = Outcome::Open(vec![Overlap::Whole(Reason::MergedFrontmatterInvalid)]);
                        view = View::Whole;
                    }
                }
            }
            (m, view)
        }
        _ => merge_lines(b, o, t, ours_first, budget),
    }
}

fn merge_lines(b: &str, o: &str, t: &str, ours_first: bool, budget: &mut Budget) -> (Merge, View) {
    let Ok(w) = lines::walk(b, o, t, ours_first, budget) else {
        return too_large_open(Rule::Lines);
    };
    let outcome = match w.text() {
        Some(text) => Outcome::Write(text.into_bytes()),
        None if w.glued => Outcome::Open(vec![Overlap::Whole(Reason::GluedLines)]),
        None => Outcome::Open(w.hunks().map(|h| Overlap::Lines(h.at.clone())).collect()),
    };
    let m = Merge {
        rule: Rule::Lines,
        outcome,
        deviations: w.deviations.clone(),
        newest_taken: Vec::new(),
    };
    (m, View::Walk(w))
}

/// Upstream's line merge (commonlib 0.2.1 `mergeSensibly`), as this crate
/// models it: the merged text, or `None` where upstream does not merge. For
/// tests that check the model against upstream's own output; not part of the
/// interface.
#[doc(hidden)]
pub fn upstream_line_merge(base: &str, ours: &str, theirs: &str, ours_first: bool) -> Option<String> {
    lines::upstream(base, ours, theirs, ours_first).text
}

pub use lines::Src;

/// The line merge of [`merge`] (rule 6, [`Kind::Text`]) with where each line
/// comes from, for tests that check a merge against an oracle. Not part of the
/// interface.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct LineTrace {
    /// For each base line, the line of ours that is it, where ours kept it.
    pub ours: Vec<Option<usize>>,
    /// The same for theirs.
    pub theirs: Vec<Option<usize>>,
    /// The merged lines, where the line merge is clean.
    pub merged: Option<Vec<Src>>,
    /// The merged text.
    pub text: Option<String>,
}

/// See [`LineTrace`]. `None` past the cost budget.
#[doc(hidden)]
pub fn line_merge_trace(base: &str, ours: &str, theirs: &str, ours_first: bool) -> Option<LineTrace> {
    let w = lines::walk(base, ours, theirs, ours_first, &mut Budget::new(MAX_COST)).ok()?;
    let text = w.text();
    let [o, t] = w.align;
    Some(LineTrace {
        ours: o,
        theirs: t,
        merged: text.is_some().then_some(w.trace),
        text,
    })
}

/// Upstream's object merge (commonlib 0.2.1 `mergeObject`), as this crate
/// models it, written as `JSON.stringify` would (keys in this crate's order):
/// `Some(None)` where upstream does not merge, `None` where the model does not
/// cover the input. For tests; not part of the interface.
#[doc(hidden)]
pub fn upstream_object_merge(base: &str, ours: &str, theirs: &str, ours_first: bool) -> Option<Option<String>> {
    json::upstream_merge(base, ours, theirs, ours_first)
}

fn json_style(b: &str, o: &str, t: &str, ours: &Version, theirs: &Version) -> json::Style {
    let (so, st) = (json::Style::detect(o), json::Style::detect(t));
    // The same layout on both sides, or the newer side's; with equal mtimes the base's.
    if so == st || ours.mtime > theirs.mtime {
        so
    } else if theirs.mtime > ours.mtime {
        st
    } else {
        json::Style::detect(b)
    }
}

fn merge_object(b: &str, o: &str, t: &str, ours: &Version, theirs: &Version, ours_first: bool) -> Merge {
    let r = json::merge(b, o, t, ours_first, json_style(b, o, t, ours, theirs));
    let outcome = match r.outcome {
        json::ObjectOutcome::Merged(s) => Outcome::Write(s.into_bytes()),
        json::ObjectOutcome::Open(paths) => Outcome::Open(paths.into_iter().map(Overlap::Key).collect()),
        json::ObjectOutcome::Unparsable(w, json::ParseError::Invalid) => {
            Outcome::Open(vec![Overlap::Whole(Reason::InvalidJson(w))])
        }
        json::ObjectOutcome::Unparsable(w, json::ParseError::Unsupported) => {
            Outcome::Open(vec![Overlap::Whole(Reason::UnsupportedJson(w))])
        }
        json::ObjectOutcome::Shape => Outcome::Open(vec![Overlap::Whole(Reason::JsonShape)]),
    };
    Merge {
        rule: Rule::Object,
        outcome,
        deviations: r.deviations,
        newest_taken: Vec::new(),
    }
}

/// The frontmatter rule, if it applies.
fn merge_frontmatter(
    b: &str,
    o: &str,
    t: &str,
    ours: &Version,
    theirs: &Version,
    settings: &Settings,
    budget: &mut Budget,
) -> Result<Option<(Merge, View)>, OutOfBudget> {
    let (Some(d_o), Some(dt)) = (frontmatter::split(o), frontmatter::split(t)) else {
        return Ok(None);
    };
    budget.read(d_o.block)?;
    budget.read(dt.block)?;
    // A base without frontmatter has an empty block, so two sides that each
    // added one merge key by key rather than as two blocks of lines.
    let (db, synthetic) = match frontmatter::split(b) {
        Some(d) => (d, 0),
        None => (
            frontmatter::Doc {
                open: d_o.open,
                block: "",
                close: d_o.open,
                body: b,
            },
            2,
        ),
    };
    let ours_first = ours.mtime <= theirs.mtime;
    let ours_newer = (ours.mtime != theirs.mtime).then_some(ours.mtime > theirs.mtime);
    // Upstream merges the whole file by lines: where this rule's result is
    // another, it is a deviation. The label is a report, not part of the
    // decision, so it has a budget of its own: it never makes the merge open
    // (too large), and where upstream's merge would not fit in it, there is no
    // label (whether upstream differs is not known).
    let decided = |mut m: Merge, view: View| -> Result<Option<(Merge, View)>, OutOfBudget> {
        let written = match &m.outcome {
            Outcome::Write(w) => Some(std::str::from_utf8(w).expect("merged from UTF-8")),
            _ => None,
        };
        if let Ok(up) = lines::upstream_within(b, o, t, ours_first, &mut Budget::new(MAX_COST)) {
            if written != up.text.as_deref() && !(written.is_none() && up.text.is_none()) {
                m.deviations.push(Deviation::FrontmatterByKey);
            }
        }
        Ok(Some((m, view)))
    };
    let fm = |overlaps| (open(Rule::Frontmatter, overlaps), View::Whole);
    // Both sides changed the block, with fences that end their lines
    // differently: neither the key merge nor the line merge (which would put
    // one block in the body) can take it on.
    let changed = d_o.block != db.block && dt.block != db.block && d_o.block != dt.block;
    if changed && (d_o.open != dt.open || (synthetic == 0 && (db.open != d_o.open || db.open != dt.open))) {
        let (m, v) = fm(vec![Overlap::Whole(Reason::FrontmatterFences)]);
        return decided(m, v);
    }
    let block = frontmatter::merge_block(&db, &d_o, &dt, ours_first, ours_newer, &settings.newest_keys, budget)?;
    match block {
        frontmatter::FmOutcome::NotApplicable => Ok(None),
        frontmatter::FmOutcome::Unparsable(w) => {
            let (m, v) = fm(vec![Overlap::Whole(Reason::InvalidFrontmatter(w))]);
            decided(m, v)
        }
        frontmatter::FmOutcome::Order => {
            let (m, v) = fm(vec![Overlap::FrontmatterOrder]);
            decided(m, v)
        }
        frontmatter::FmOutcome::Block { parts, newest_taken } => {
            let mut overlaps = Vec::new();
            let mut block = String::new();
            // The view: the opening fence, the block key by key, the closing
            // fence, the body's walk.
            let mut view = lines::Walk::default();
            view.segs.push(lines::Seg::Clean(db.open.to_string()));
            for p in parts {
                match p {
                    frontmatter::Part::Text(s) => {
                        block.push_str(&s);
                        view.segs.push(lines::Seg::Clean(s));
                    }
                    frontmatter::Part::Conflict {
                        key,
                        base,
                        ours,
                        theirs,
                    } => {
                        overlaps.push(Overlap::FrontmatterKey(key));
                        view.segs.push(lines::Seg::Conflict(lines::Hunk {
                            at: LineOverlap {
                                base: 0..0,
                                ours: 0..0,
                                theirs: 0..0,
                            },
                            base: base.unwrap_or_default(),
                            ours: ours.unwrap_or_default(),
                            theirs: theirs.unwrap_or_default(),
                        }));
                    }
                }
            }
            let w = lines::walk(db.body, d_o.body, dt.body, ours_first, budget)?;
            let (ob, oo, ot) = (
                db.body_line_offset() - synthetic,
                d_o.body_line_offset(),
                dt.body_line_offset(),
            );
            for h in w.hunks() {
                let shift = |r: &Range<usize>, by: usize| r.start + by..r.end + by;
                overlaps.push(Overlap::Lines(LineOverlap {
                    base: shift(&h.at.base, ob),
                    ours: shift(&h.at.ours, oo),
                    theirs: shift(&h.at.theirs, ot),
                }));
            }
            let mut m = open(Rule::Frontmatter, Vec::new());
            m.deviations = w.deviations.clone();
            m.newest_taken = newest_taken;
            // The closing fence's line end, three-way.
            let close = match (d_o.close == db.close, dt.close == db.close, d_o.close == dt.close) {
                (_, _, true) | (_, true, _) => Some(d_o.close),
                (true, _, _) => Some(dt.close),
                _ => None,
            };
            if w.glued {
                m.outcome = Outcome::Open(vec![Overlap::Whole(Reason::GluedLines)]);
                return decided(m, View::Whole);
            }
            // A body after a fence without a line end would be glued to it,
            // in the merge and in its view alike.
            let fence = close.unwrap_or(d_o.close);
            let body_after_open_fence = !fence.ends_with('\n') && !w.segs.is_empty();
            if !overlaps.is_empty() {
                m.outcome = Outcome::Open(overlaps);
                if body_after_open_fence {
                    return decided(m, View::Whole);
                }
                view.segs.push(lines::Seg::Clean(fence.to_string()));
                view.segs.extend(w.segs);
                return decided(m, View::Walk(view));
            }
            if !frontmatter::understood(&block) {
                m.outcome = Outcome::Open(vec![Overlap::Whole(Reason::MergedFrontmatterInvalid)]);
                return decided(m, View::Whole);
            }
            let Some(close) = close else {
                m.outcome = Outcome::Open(vec![Overlap::Lines(LineOverlap {
                    base: if synthetic > 0 { 0..0 } else { ob - 1..ob },
                    ours: oo - 1..oo,
                    theirs: ot - 1..ot,
                })]);
                return decided(m, View::Whole);
            };
            if body_after_open_fence {
                m.deviations.push(Deviation::GluedLines);
                m.outcome = Outcome::Open(vec![Overlap::Whole(Reason::GluedLines)]);
                return decided(m, View::Whole);
            }
            let body = w.text().expect("clean walk");
            let text = format!("{}{}{}{}", d_o.open, block, close, body);
            m.outcome = Outcome::Write(text.clone().into_bytes());
            decided(m, View::Text(text))
        }
    }
}

/// A file with conflict markers, for agents and people.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merge3 {
    /// The merged file, with diff3-style markers (`<<<<<<<`, `|||||||`,
    /// `=======`, `>>>>>>>`) around just the overlapping hunks.
    pub text: String,
    /// How many hunks carry markers. Zero exactly when [`merge`] merges (or
    /// finds the sides identical): `text` is then its result.
    pub conflicts: usize,
}

/// The `merge3` text of `lsync conflicts show`, built from the same decision
/// as [`merge`], so the two agree: a clean merge is shown as the merged file,
/// with no markers; an open one with at least one marked hunk. Line conflicts
/// are marked hunk by hunk, frontmatter conflicts key by key. Where the reason
/// is about the whole file (a side emptied, frontmatter that does not parse,
/// JSON, a file too large to merge), the whole file is one hunk. Without a
/// base, every difference is marked.
///
/// `None` when there is nothing to show as text: a deleted side, content that
/// is not UTF-8, or [`Kind::Other`].
pub fn merge3(kind: Kind, input: &Input, settings: &Settings, labels: &Labels) -> Option<Merge3> {
    let view = decide(kind, input, settings).1;
    fn text<'a>(v: Option<&Version<'a>>) -> Option<&'a str> {
        match v {
            Some(v) if !v.deleted => std::str::from_utf8(v.content).ok(),
            _ => None,
        }
    }
    let (o, t) = (text(Some(&input.ours))?, text(Some(&input.theirs))?);
    let b = match &input.base {
        Some(v) if !v.deleted => text(Some(v))?,
        _ => "",
    };
    let eol = lines::eol_of(&[o, t, b]);
    let walk = match view {
        View::Nothing => return None,
        View::Text(text) => return Some(Merge3 { text, conflicts: 0 }),
        View::Walk(w) => w,
        View::Whole => lines::Walk::whole(b, o, t),
        View::TwoWay => lines::walk_two_way(o, t, &mut Budget::new(MAX_COST)),
    };
    Some(Merge3 {
        conflicts: walk.hunks().count(),
        text: lines::render(&walk, labels, eol),
    })
}

#[cfg(test)]
mod tests;

/// `as_str` and `parse` over a fixed table of names.
macro_rules! names {
    ($t:ty { $($v:ident => $s:literal,)* }) => {
        impl $t {
            /// Every variant, in declaration order.
            pub const ALL: &'static [$t] = &[$(<$t>::$v,)*];
            /// The stable name (see the crate docs).
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(<$t>::$v => $s,)*
                }
            }
            /// The variant named `s` by [`Self::as_str`].
            pub fn parse(s: &str) -> Option<$t> {
                match s {
                    $($s => Some(<$t>::$v),)*
                    _ => None,
                }
            }
        }
        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

names!(Rule {
    Identical => "identical",
    EditBeatsDelete => "edit_beats_delete",
    BothDeleted => "both_deleted",
    NoBase => "no_base",
    BaseDeleted => "base_deleted",
    Emptied => "emptied",
    Lines => "lines",
    Frontmatter => "frontmatter",
    Object => "object",
    Unsupported => "unsupported",
});

names!(Deviation {
    SameInsertionOnce => "same_insertion_once",
    GluedLines => "glued_lines",
    UnparsableJson => "unparsable_json",
    StricterObjectCheck => "stricter_object_check",
    ObjectResultDiffers => "object_result_differs",
    NumericKeyOrder => "numeric_key_order",
    BrokenFrontmatter => "broken_frontmatter",
    TrailingDeletion => "trailing_deletion",
    RunOrder => "run_order",
    EmptiedOpen => "emptied_open",
    PositionOrder => "position_order",
    FrontmatterByKey => "frontmatter_by_key",
});

names!(Which {
    Base => "base",
    Ours => "ours",
    Theirs => "theirs",
});

names!(Side {
    Ours => "ours",
    Theirs => "theirs",
});

impl Reason {
    /// The stable name of the reason, without the side it names (see
    /// [`Reason::which`]).
    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::NoBase => "no_base",
            Reason::BaseDeleted => "base_deleted",
            Reason::BothDeleted => "both_deleted",
            Reason::Unsupported => "unsupported",
            Reason::NotUtf8(_) => "not_utf8",
            Reason::InvalidJson(_) => "invalid_json",
            Reason::UnsupportedJson(_) => "unsupported_json",
            Reason::JsonShape => "json_shape",
            Reason::InvalidFrontmatter(_) => "invalid_frontmatter",
            Reason::MergedFrontmatterInvalid => "merged_frontmatter_invalid",
            Reason::GluedLines => "glued_lines",
            Reason::Emptied(_) => "emptied",
            Reason::TooLarge => "too_large",
            Reason::FrontmatterFences => "frontmatter_fences",
        }
    }
    /// The version the reason is about, where it names one.
    pub const fn which(self) -> Option<Which> {
        match self {
            Reason::NotUtf8(w)
            | Reason::InvalidJson(w)
            | Reason::UnsupportedJson(w)
            | Reason::InvalidFrontmatter(w)
            | Reason::Emptied(w) => Some(w),
            _ => None,
        }
    }
}

impl std::fmt::Display for Reason {
    /// The name, and the side where it names one: `invalid_json(ours)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.which() {
            Some(w) => write!(f, "{}({})", self.as_str(), w.as_str()),
            None => f.write_str(self.as_str()),
        }
    }
}

impl Overlap {
    /// The stable name of the kind of overlap.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Overlap::Lines(_) => "lines",
            Overlap::Key(_) => "key",
            Overlap::FrontmatterKey(_) => "frontmatter_key",
            Overlap::FrontmatterOrder => "frontmatter_order",
            Overlap::Whole(_) => "whole",
        }
    }
}

impl Outcome {
    /// The stable name of the kind of outcome.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Outcome::Identical => "identical",
            Outcome::Keep(_) => "keep",
            Outcome::Write(_) => "write",
            Outcome::Open(_) => "open",
        }
    }
}
