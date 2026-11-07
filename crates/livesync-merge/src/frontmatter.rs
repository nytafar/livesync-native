//! Frontmatter merged key by key (not upstream's; upstream merges the block as
//! lines).
//!
//! The block is split into its top-level entries as written (a key line and the
//! lines under it), so a merged block keeps each entry's own text, comments and
//! quoting. A YAML parser only checks that split: the block must be one mapping
//! whose keys are exactly the entries found, in order. Anything else is not
//! understood and stays open.

use crate::lines;
use std::collections::HashSet;
use yaml_rust2::{Yaml, YamlLoader};

/// A Markdown file cut at its frontmatter.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Doc<'a> {
    /// `---\n` or `---\r\n`
    pub open: &'a str,
    /// The lines between the fences.
    pub block: &'a str,
    /// The closing fence with its line end: `---\n`, `---\r\n`, or `---` at
    /// the end of the file.
    pub close: &'a str,
    /// Everything after the closing fence's line.
    pub body: &'a str,
}

impl Doc<'_> {
    /// Lines before the body's first line (both fences and the block).
    pub fn body_line_offset(&self) -> usize {
        2 + self.block.split_inclusive('\n').count()
    }
}

/// Obsidian's frontmatter: `---` as the very first line, closed by a line `---`.
pub(crate) fn split(text: &str) -> Option<Doc<'_>> {
    let open_len = if text.starts_with("---\n") {
        4
    } else if text.starts_with("---\r\n") {
        5
    } else {
        return None;
    };
    let mut pos = open_len;
    for line in text[open_len..].split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == "---" {
            return Some(Doc {
                open: &text[..open_len],
                block: &text[open_len..pos],
                close: line,
                body: &text[pos + line.len()..],
            });
        }
        pos += line.len();
    }
    None
}

/// One top-level entry: its key (`None` for comments or blank lines before the
/// first key) and its text, newline included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry<'a> {
    pub key: Option<String>,
    pub text: &'a str,
}

pub(crate) struct NotUnderstood;

pub(crate) fn entries(block: &str) -> Result<Vec<Entry<'_>>, NotUnderstood> {
    let mut out: Vec<(Option<String>, usize, usize)> = Vec::new();
    let mut raw_keys: Vec<&str> = Vec::new();
    let mut pos = 0;
    for line in block.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let continuation = content.starts_with([' ', '\t', '-']);
        let filler = content.trim().is_empty() || content.starts_with('#');
        if continuation || filler {
            match out.last_mut() {
                Some(e) => e.2 = pos + line.len(),
                None if filler => out.push((None, pos, pos + line.len())),
                None => return Err(NotUnderstood),
            }
        } else {
            let raw = key_of(content).ok_or(NotUnderstood)?;
            let name = scalar_name(raw).ok_or(NotUnderstood)?;
            out.push((Some(name), pos, pos + line.len()));
            raw_keys.push(raw);
        }
        pos += line.len();
    }
    // Check the split against a real parse.
    let docs = YamlLoader::load_from_str(block).map_err(|_| NotUnderstood)?;
    let keyed: Vec<&String> = out.iter().filter_map(|e| e.0.as_ref()).collect();
    match docs.as_slice() {
        [] | [Yaml::Null] if keyed.is_empty() => {}
        [Yaml::Hash(h)] => {
            if h.len() != keyed.len() || keyed.iter().collect::<HashSet<_>>().len() != keyed.len() {
                return Err(NotUnderstood);
            }
            for ((k, _), raw) in h.iter().zip(&raw_keys) {
                let alone = YamlLoader::load_from_str(raw).map_err(|_| NotUnderstood)?;
                if alone.first() != Some(k) {
                    return Err(NotUnderstood);
                }
            }
        }
        _ => return Err(NotUnderstood),
    }
    Ok(out
        .into_iter()
        .map(|(key, s, e)| Entry {
            key,
            text: &block[s..e],
        })
        .collect())
}

/// The key as written on an entry's first line, quotes included.
fn key_of(line: &str) -> Option<&str> {
    let b = line.as_bytes();
    let end = match b.first()? {
        b'"' => {
            let mut i = 1;
            loop {
                match b.get(i)? {
                    b'\\' => i += 2,
                    b'"' => break i + 1,
                    _ => i += 1,
                }
            }
        }
        b'\'' => {
            let mut i = 1;
            loop {
                match b.get(i)? {
                    b'\'' if b.get(i + 1) == Some(&b'\'') => i += 2,
                    b'\'' => break i + 1,
                    _ => i += 1,
                }
            }
        }
        b'?' | b'{' | b'[' | b'&' | b'*' | b'!' | b'|' | b'>' | b'%' | b'@' | b'`' | b':' => return None,
        _ => {
            let mut i = 0;
            loop {
                if i >= b.len() {
                    return None;
                }
                if b[i] == b':' && matches!(b.get(i + 1), None | Some(b' ' | b'\t')) {
                    break i;
                }
                i += 1;
            }
        }
    };
    let rest = line[end..].trim_start_matches([' ', '\t']);
    if !rest.starts_with(':') {
        return None;
    }
    let key = line[..end].trim_end();
    (!key.is_empty()).then_some(key)
}

/// A key's name for `newest_keys` and reports: the scalar it parses to.
fn scalar_name(raw: &str) -> Option<String> {
    match YamlLoader::load_from_str(raw).ok()?.first()? {
        Yaml::String(s) | Yaml::Real(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Boolean(b) => Some(b.to_string()),
        Yaml::Null => Some(raw.to_string()),
        _ => None,
    }
}

/// Is this block one the merge understands?
pub(crate) fn understood(block: &str) -> bool {
    entries(block).is_ok()
}

pub(crate) enum FmOutcome {
    /// Rule 3 does not apply: not both sides changed the block.
    NotApplicable,
    /// One of the blocks is not understood.
    Unparsable(crate::Which),
    /// The block merged key by key (possibly with conflicting keys); the body
    /// still has to merge.
    Block {
        parts: Vec<Part>,
        newest_taken: Vec<String>,
    },
    /// The key order could not be merged.
    Order,
}

/// A piece of the merged block, in order.
#[derive(Clone, Debug)]
pub(crate) enum Part {
    Text(String),
    /// A key both sides changed differently (or one deleted, one changed).
    Conflict {
        key: Option<String>,
        base: Option<String>,
        ours: Option<String>,
        theirs: Option<String>,
    },
}

/// Merge the blocks of three documents. `ours_newer` decides `newest_keys`;
/// `None` when the mtimes are equal, and then the greater text is taken, so
/// the choice does not depend on which side is which.
pub(crate) fn merge_block(
    base: &Doc,
    ours: &Doc,
    theirs: &Doc,
    ours_first: bool,
    ours_newer: Option<bool>,
    newest_keys: &[String],
) -> FmOutcome {
    if base.block == ours.block || base.block == theirs.block || ours.block == theirs.block {
        return FmOutcome::NotApplicable;
    }
    if base.open != ours.open || base.open != theirs.open {
        return FmOutcome::NotApplicable;
    }
    let parsed = [
        (crate::Which::Base, entries(base.block)),
        (crate::Which::Ours, entries(ours.block)),
        (crate::Which::Theirs, entries(theirs.block)),
    ];
    let mut es = Vec::new();
    for (w, p) in parsed {
        match p {
            Ok(e) => es.push(e),
            Err(_) => return FmOutcome::Unparsable(w),
        }
    }
    let [b, o, t] = [&es[0], &es[1], &es[2]];

    // The key order, merged with the line merge: one line per key.
    let order = |e: &[Entry]| -> String {
        e.iter()
            .map(|x| match &x.key {
                Some(k) => format!("k{k}\n"),
                None => "p\n".to_string(),
            })
            .collect()
    };
    let w = lines::walk(&order(b), &order(o), &order(t), ours_first);
    let Some(merged_order) = w.text() else {
        return FmOutcome::Order;
    };
    let order_keys: Vec<Option<String>> = merged_order
        .lines()
        .map(|l| l.strip_prefix('k').map(str::to_string))
        .collect();
    if order_keys.iter().collect::<HashSet<_>>().len() != order_keys.len() {
        return FmOutcome::Order;
    }

    let get = |e: &[Entry], k: &Option<String>| e.iter().find(|x| &x.key == k).map(|x| x.text.to_string());
    let mut keys: Vec<Option<String>> = Vec::new();
    for e in b.iter().chain(o.iter()).chain(t.iter()) {
        if !keys.contains(&e.key) {
            keys.push(e.key.clone());
        }
    }
    let mut newest_taken = Vec::new();
    let mut decided: Vec<(Option<String>, Option<Part>)> = Vec::new();
    for k in &keys {
        let (bv, ov, tv) = (get(b, k), get(o, k), get(t, k));
        let newest = k.as_ref().is_some_and(|name| newest_keys.iter().any(|n| n == name));
        let part = if ov == tv || tv == bv {
            ov.map(Part::Text)
        } else if ov == bv {
            tv.map(Part::Text)
        } else if let (true, Some(x), Some(y)) = (newest, &ov, &tv) {
            newest_taken.push(k.clone().unwrap_or_default());
            Some(Part::Text(match ours_newer {
                Some(true) => x.clone(),
                Some(false) => y.clone(),
                None => x.max(y).clone(),
            }))
        } else {
            Some(Part::Conflict {
                key: k.clone(),
                base: bv,
                ours: ov,
                theirs: tv,
            })
        };
        decided.push((k.clone(), part));
    }
    let mut placed: Vec<Option<String>> = Vec::new();
    let mut parts = Vec::new();
    for k in &order_keys {
        if let Some((_, Some(p))) = decided.iter().find(|(kk, _)| kk == k) {
            parts.push(p.clone());
            placed.push(k.clone());
        }
    }
    // A key one side deleted and the other changed is not in the merged order:
    // show it after the key it followed in the base (or the side that has it).
    for (k, p) in &decided {
        let Some(p) = p else { continue };
        if placed.contains(k) {
            continue;
        }
        if !matches!(p, Part::Conflict { .. }) {
            // Every entry kept must have found its place.
            return FmOutcome::Order;
        }
        let src = [b, o, t]
            .into_iter()
            .find(|e| e.iter().any(|x| &x.key == k))
            .expect("key from an input");
        let at = src.iter().position(|x| &x.key == k).expect("present");
        let after = src[..at]
            .iter()
            .rev()
            .find_map(|x| placed.iter().position(|pk| pk == &x.key));
        let i = after.map_or(0, |i| i + 1);
        parts.insert(i, p.clone());
        placed.insert(i, k.clone());
    }
    FmOutcome::Block { parts, newest_taken }
}
