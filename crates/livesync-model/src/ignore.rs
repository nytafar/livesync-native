//! Ignore files as the plugin and the official CLI read them ("Use ignore
//! files", `useIgnoreFiles` / `ignoreFiles`).
//!
//! Upstream: commonlib `string_and_binary/path.ts` (`isAccepted`,
//! `isAcceptedAll`), which matches with minimatch 10 (`dot`, `nocase`,
//! `flipNegate`, platform `linux`) and brace-expansion 5. This is a port of
//! the parts of both that those calls reach: brace expansion, `*`, `?`,
//! `**`, character classes and escapes, the minimatch fast paths for common
//! segment shapes, and its JavaScript case folding (`/i` without `/u`, on
//! UTF-16 code units). Extglobs (`+(a|b)` and friends), POSIX classes
//! (`[[:alpha:]]`) and an escaped `|` are not ported: a pattern that uses
//! them is [`Unsupported`], and the caller treats its ignore file as one it
//! cannot read (it fails closed). Golden vectors from commonlib itself pin
//! the rest (`fixtures/golden/ignore_*.json`).
//!
//! These are not git's rules: a pattern is matched against the path relative
//! to its ignore file's folder, so `*.log` ignores only files beside the
//! ignore file, `dir/` ignores everything below `dir` (and no later `!` can
//! take it back), any other pattern `p` also covers `p/**`, the last matching
//! pattern in a file decides, and the nearest ignore file that decides wins.

use std::collections::HashSet;

/// A pattern that uses minimatch syntax this port does not evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported(pub String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ----------------------------------------------------------------------
// JavaScript string helpers
// ----------------------------------------------------------------------

/// `String.prototype.trim`: WhiteSpace and LineTerminator, which include
/// the byte order mark but not U+0085.
fn js_is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\u{b}'
            | '\u{c}'
            | ' '
            | '\u{a0}'
            | '\u{feff}'
            | '\n'
            | '\r'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

pub fn js_trim(s: &str) -> &str {
    s.trim_matches(js_is_space)
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `Canonicalize` for a `/i` regular expression without `/u` (ECMA-262):
/// the code unit upper-cased, unless that is not one code unit or would map
/// a non-ASCII unit to ASCII.
fn canon(u: u16) -> u16 {
    let Some(c) = char::from_u32(u as u32) else {
        return u; // a lone surrogate
    };
    let mut up = c.to_uppercase();
    match (up.next(), up.next()) {
        (Some(x), None) => {
            let x = x as u32;
            if x > 0xFFFF || (u >= 128 && x < 128) {
                u
            } else {
                x as u16
            }
        }
        _ => u,
    }
}

/// Code units whose canonical form may equal `canon(u)` (for class
/// membership under `/i`): the unit, its canonical form and that form's
/// lower case.
fn canon_candidates(u: u16) -> [u16; 3] {
    let cu = canon(u);
    let lower = char::from_u32(cu as u32)
        .map(|c| {
            let mut l = c.to_lowercase();
            match (l.next(), l.next()) {
                (Some(x), None) if (x as u32) <= 0xFFFF => x as u16,
                _ => cu,
            }
        })
        .unwrap_or(cu);
    [u, cu, lower]
}

// ----------------------------------------------------------------------
// brace-expansion 5
// ----------------------------------------------------------------------

const ESC_SLASH: char = '\u{F0001}';
const ESC_OPEN: char = '\u{F0002}';
const ESC_CLOSE: char = '\u{F0003}';
const ESC_COMMA: char = '\u{F0004}';
const ESC_PERIOD: char = '\u{F0005}';
const EXPANSION_MAX: usize = 100_000;
const EXPANSION_MAX_LENGTH: usize = 4_000_000;
const EXPANSION_MAX_DEPTH: usize = 1_000;
const EXPANSION_MAX_REWRITES: usize = 1_000;

struct Balanced<'a> {
    pre: &'a str,
    body: &'a str,
    post: &'a str,
}

/// balanced-match's `range` for single-character delimiters (byte offsets;
/// -1 stands for "not found", as `indexOf` returns).
fn balanced_range(s: &str, a: char, b: char) -> Option<(usize, usize)> {
    let find = |c: char, from: i64| -> i64 {
        let from = from.max(0) as usize;
        match s.get(from..) {
            Some(t) => t.find(c).map(|i| (i + from) as i64).unwrap_or(-1),
            None => -1,
        }
    };
    let mut ai = find(a, 0);
    let mut bi = find(b, ai + 1);
    let mut i = ai;
    if !(ai >= 0 && bi > 0) {
        return None;
    }
    let mut begs: Vec<i64> = Vec::new();
    let mut left = s.len() as i64;
    let mut right: Option<i64> = None;
    let mut result: Option<(i64, i64)> = None;
    while i >= 0 && result.is_none() {
        if i == ai {
            begs.push(i);
            ai = find(a, i + 1);
        } else if begs.len() == 1 {
            let r = begs.pop().unwrap();
            result = Some((r, bi));
        } else {
            if let Some(beg) = begs.pop() {
                if beg < left {
                    left = beg;
                    right = Some(bi);
                }
            }
            bi = find(b, i + 1);
        }
        i = if ai < bi && ai >= 0 { ai } else { bi };
    }
    if !begs.is_empty() {
        if let Some(r) = right {
            result = Some((left, r));
        }
    }
    result.and_then(|(x, y)| (x >= 0 && y >= 0).then_some((x as usize, y as usize)))
}

fn balanced(s: &str) -> Option<Balanced<'_>> {
    let (start, end) = balanced_range(s, '{', '}')?;
    Some(Balanced {
        pre: &s[..start],
        body: &s[start + 1..end],
        post: &s[end + 1..],
    })
}

fn escape_braces(s: &str) -> String {
    s.replace("\\\\", &ESC_SLASH.to_string())
        .replace("\\{", &ESC_OPEN.to_string())
        .replace("\\}", &ESC_CLOSE.to_string())
        .replace("\\,", &ESC_COMMA.to_string())
        .replace("\\.", &ESC_PERIOD.to_string())
}

fn unescape_braces(s: &str) -> String {
    s.replace(ESC_SLASH, "\\")
        .replace(ESC_OPEN, "{")
        .replace(ESC_CLOSE, "}")
        .replace(ESC_COMMA, ",")
        .replace(ESC_PERIOD, ".")
}

fn parse_comma_parts(s: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut carry = String::new();
    let mut s = s.to_string();
    loop {
        let Some(m) = balanced(&s) else {
            let mut tail: Vec<String> = s.split(',').map(str::to_string).collect();
            tail[0] = format!("{carry}{}", tail[0]);
            parts.extend(tail);
            return parts;
        };
        let mut p: Vec<String> = m.pre.split(',').map(str::to_string).collect();
        p[0] = format!("{carry}{}", p[0]);
        let last = p.len() - 1;
        p[last].push('{');
        p[last].push_str(m.body);
        p[last].push('}');
        if m.post.is_empty() {
            parts.extend(p);
            return parts;
        }
        let post = m.post.to_string();
        carry = p.pop().unwrap();
        parts.extend(p);
        s = post;
    }
}

fn combine(acc: &[String], pre: &str, values: &[String], drop_empties: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut length = 0usize;
    for a in acc {
        for v in values {
            if out.len() >= EXPANSION_MAX {
                return out;
            }
            let e = format!("{a}{pre}{v}");
            if drop_empties && e.is_empty() {
                continue;
            }
            if length + js_len(&e) > EXPANSION_MAX_LENGTH {
                return out;
            }
            length += js_len(&e);
            out.push(e);
        }
    }
    out
}

fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `^-?\d+$`-ish numbers as `parseInt` reads them, else the first code unit.
fn bx_numeric(s: &str) -> i64 {
    // Only called on parts the sequence regexes accepted: an optional `-`
    // and ASCII digits, or a single ASCII letter.
    match s.parse::<i64>() {
        Ok(n) => n,
        Err(_) => s.encode_utf16().next().map(|u| u as i64).unwrap_or(0),
    }
}

fn is_numeric_part(s: &str) -> bool {
    let d = s.strip_prefix('-').unwrap_or(s);
    !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
}

fn is_numeric_sequence(body: &str) -> bool {
    let n: Vec<&str> = body.split("..").collect();
    (n.len() == 2 || n.len() == 3) && n.iter().all(|p| is_numeric_part(p))
}

fn is_alpha_sequence(body: &str) -> bool {
    let n: Vec<&str> = body.split("..").collect();
    let alpha = |p: &str| p.len() == 1 && p.bytes().all(|b| b.is_ascii_alphabetic());
    (n.len() == 2 || n.len() == 3) && alpha(n[0]) && alpha(n[1]) && (n.len() == 2 || is_numeric_part(n[2]))
}

fn expand_sequence(body: &str, alpha: bool) -> Vec<String> {
    let n: Vec<&str> = body.split("..").collect();
    let x = bx_numeric(n[0]);
    let y = bx_numeric(n[1]);
    let width = n[0].len().max(n[1].len());
    let mut incr: i64 = if n.len() == 3 { bx_numeric(n[2]).abs().max(1) } else { 1 };
    let reverse = y < x;
    if reverse {
        incr = -incr;
    }
    let padded = |e: &str| {
        let d = e.strip_prefix('-').unwrap_or(e);
        d.len() >= 2 && d.starts_with('0') && d.as_bytes()[1].is_ascii_digit()
    };
    let pad = n.iter().any(|e| padded(e));
    let mut out = Vec::new();
    let mut length = 0usize;
    let mut i = x;
    while (if reverse { i >= y } else { i <= y }) && out.len() < EXPANSION_MAX {
        let c = if alpha {
            let s = char::from_u32(i as u32).map(|c| c.to_string()).unwrap_or_default();
            if s == "\\" {
                String::new()
            } else {
                s
            }
        } else {
            let mut c = i.to_string();
            if pad {
                let need = width as i64 - c.len() as i64;
                if need > 0 {
                    let z = "0".repeat(need as usize);
                    c = if i < 0 {
                        format!("-{z}{}", &c[1..])
                    } else {
                        format!("{z}{c}")
                    };
                }
            }
            c
        };
        if length + js_len(&c) > EXPANSION_MAX_LENGTH {
            break;
        }
        length += js_len(&c);
        out.push(c);
        i += incr;
    }
    out
}

/// `post.match(/,(?!,).*\}/)`: a comma not followed by a comma, and a `}`
/// later on the line.
fn rewrite_quirk(post: &str) -> bool {
    let b = post.as_bytes();
    for i in 0..b.len() {
        if b[i] == b',' && b.get(i + 1) != Some(&b',') {
            let rest = &post[i + 1..];
            if rest
                .find(['\n', '\r', '\u{2028}', '\u{2029}'])
                .map_or(rest, |e| &rest[..e])
                .contains('}')
            {
                return true;
            }
        }
    }
    false
}

fn expand_inner(s: &str, depth: usize, mut is_top: bool) -> Vec<String> {
    if depth > EXPANSION_MAX_DEPTH {
        return vec![s.to_string()];
    }
    let mut acc: Vec<String> = vec![String::new()];
    let mut rewrites = 0usize;
    let mut drop_empties = false;
    let mut first_group = true;
    let mut s = s.to_string();
    loop {
        let Some(m) = balanced(&s) else {
            return combine(&acc, &s, &[String::new()], drop_empties);
        };
        let (pre, body, post) = (m.pre.to_string(), m.body.to_string(), m.post.to_string());
        if pre.ends_with('$') {
            acc = combine(
                &acc,
                &format!("{pre}{{{body}}}"),
                &[String::new()],
                drop_empties && post.is_empty(),
            );
            first_group = false;
            if post.is_empty() {
                break;
            }
            s = post;
            continue;
        }
        let numeric = is_numeric_sequence(&body);
        let alpha = !numeric && is_alpha_sequence(&body);
        let is_sequence = numeric || alpha;
        let is_options = body.contains(',');
        if !is_sequence && !is_options {
            if rewrites < EXPANSION_MAX_REWRITES && rewrite_quirk(&post) {
                rewrites += 1;
                s = format!("{pre}{{{body}{ESC_CLOSE}{post}");
                is_top = true;
                continue;
            }
            return combine(&acc, &format!("{pre}{{{body}}}{post}"), &[String::new()], drop_empties);
        }
        if first_group {
            drop_empties = is_top && !is_sequence;
            first_group = false;
        }
        let values: Vec<String> = if is_sequence {
            expand_sequence(&body, alpha)
        } else {
            let mut n = parse_comma_parts(&body);
            if n.len() == 1 {
                n = expand_inner(&n[0], depth + 1, false)
                    .into_iter()
                    .map(|e| format!("{{{e}}}"))
                    .collect();
                if n.len() == 1 {
                    acc = combine(
                        &acc,
                        &format!("{pre}{}", n[0]),
                        &[String::new()],
                        drop_empties && post.is_empty(),
                    );
                    if post.is_empty() {
                        break;
                    }
                    s = post;
                    continue;
                }
            }
            let mut drops = drop_empties && post.is_empty() && pre.is_empty();
            if acc.iter().any(|a| !a.is_empty()) {
                drops = false;
            }
            let mut values = Vec::new();
            let mut len = 0usize;
            'outer: for part in &n {
                for v in expand_inner(part, depth + 1, false) {
                    if drops && v.is_empty() {
                        continue;
                    }
                    if values.len() >= EXPANSION_MAX || len + js_len(&v) > EXPANSION_MAX_LENGTH {
                        break 'outer;
                    }
                    len += js_len(&v);
                    values.push(v);
                }
            }
            values
        };
        acc = combine(&acc, &pre, &values, drop_empties && post.is_empty());
        if post.is_empty() {
            break;
        }
        s = post;
    }
    acc
}

/// brace-expansion's `expand`.
fn brace_expand_str(s: &str) -> Vec<String> {
    if s.is_empty() {
        return vec![];
    }
    let s = if let Some(rest) = s.strip_prefix("{}") {
        format!("\\{{\\}}{rest}")
    } else {
        s.to_string()
    };
    expand_inner(&escape_braces(&s), 0, true)
        .iter()
        .map(|e| unescape_braces(e))
        .collect()
}

/// minimatch's `braceExpand`: only patterns with a `{...}` (no `{` inside)
/// are expanded.
fn mm_brace_expand(pattern: &str) -> Vec<String> {
    let mut open = false;
    let mut has = false;
    for c in pattern.chars() {
        match c {
            '{' => open = true,
            '}' if open => {
                has = true;
                break;
            }
            // `.` in the shortcut regex does not match line terminators.
            '\n' | '\r' | '\u{2028}' | '\u{2029}' => open = false,
            _ => {}
        }
    }
    if !has {
        return vec![pattern.to_string()];
    }
    brace_expand_str(pattern)
}

// ----------------------------------------------------------------------
// minimatch 10: segments
// ----------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Tok {
    /// One code unit, compared case-insensitively.
    Lit(u16),
    /// `?`: any one code unit.
    Any,
    /// `*`: any run of code units (`+` when `nonempty`).
    Star {
        nonempty: bool,
    },
    Class {
        negate: bool,
        ranges: Vec<(u16, u16)>,
    },
}

#[derive(Debug, Clone)]
enum FastTest {
    /// `*` (and `**` inside a segment): non-empty, not `.` or `..`.
    Star,
    /// `*<ext>`: ends with ext (lower-cased).
    StarDotExt(String),
    /// `?..?<ext>`: this many code units, not `.`/`..`, ends with ext.
    Qmarks(usize, String),
    /// `*.*`
    StarDotStar,
    /// `.*`
    DotStar,
}

#[derive(Debug, Clone)]
enum Seg {
    Globstar,
    /// Matches nothing (an empty character class poisons the pattern).
    Never,
    Fast(FastTest),
    Toks {
        no_traversal: bool,
        toks: Vec<Tok>,
    },
}

fn is_reserved_char(c: u16) -> bool {
    // reSpecials in minimatch's ast.ts
    matches!(
        char::from_u32(c as u32),
        Some('(' | ')' | '.' | '*' | '{' | '}' | '+' | '?' | '[' | ']' | '^' | '$' | '\\' | '!')
    )
}

/// regExpEscape of one code unit, as source text (for the start-of-pattern
/// checks, which look at the regular expression's first characters).
fn re_escaped(c: u16) -> String {
    let s = String::from_utf16_lossy(&[c]);
    match s.as_str() {
        "-" | "[" | "]" | "{" | "}" | "(" | ")" | "*" | "+" | "?" | "." | "," | "\\" | "^" | "$" | "|" | "#" => {
            format!("\\{s}")
        }
        _ if s.chars().all(js_is_space) => format!("\\{s}"),
        _ => s,
    }
}

const POSIX_CLASSES: [&str; 14] = [
    "[:alnum:]",
    "[:alpha:]",
    "[:ascii:]",
    "[:blank:]",
    "[:cntrl:]",
    "[:digit:]",
    "[:graph:]",
    "[:lower:]",
    "[:print:]",
    "[:punct:]",
    "[:space:]",
    "[:upper:]",
    "[:word:]",
    "[:xdigit:]",
];

enum ClassParse {
    /// Not a class: `[` is literal.
    NotAClass,
    /// The class poisons the whole pattern (`$.`); consumes the rest.
    Never,
    /// One literal code unit; consumed units.
    Literal(u16, usize),
    Class {
        negate: bool,
        ranges: Vec<(u16, u16)>,
        consumed: usize,
    },
}

fn starts_with_units(g: &[u16], at: usize, s: &str) -> bool {
    let u = utf16(s);
    g.len() >= at + u.len() && g[at..at + u.len()] == u[..]
}

/// minimatch's `parseClass` (brace-expressions.ts) on code units.
fn parse_class(g: &[u16], pos: usize) -> Result<ClassParse, Unsupported> {
    const BANG: u16 = b'!' as u16;
    const CARET: u16 = b'^' as u16;
    const CLOSE: u16 = b']' as u16;
    const BSLASH: u16 = b'\\' as u16;
    const OPEN: u16 = b'[' as u16;
    const DASH: u16 = b'-' as u16;
    let mut ranges: Vec<(u16, u16)> = Vec::new();
    let mut i = pos + 1;
    let mut saw_start = false;
    let mut escaping = false;
    let mut negate = false;
    let mut end_pos = pos;
    let mut range_start: Option<u16> = None;
    while i < g.len() {
        let c = g[i];
        if (c == BANG || c == CARET) && i == pos + 1 {
            negate = true;
            i += 1;
            continue;
        }
        if c == CLOSE && saw_start && !escaping {
            end_pos = i + 1;
            break;
        }
        saw_start = true;
        if c == BSLASH && !escaping {
            escaping = true;
            i += 1;
            continue;
        }
        if c == OPEN && !escaping && POSIX_CLASSES.iter().any(|cls| starts_with_units(g, i, cls)) {
            return Err(Unsupported(
                "POSIX character classes such as [[:alpha:]] are not supported".into(),
            ));
        }
        escaping = false;
        if let Some(rs) = range_start.take() {
            if c > rs {
                ranges.push((rs, c));
            } else if c == rs {
                ranges.push((c, c));
            }
            i += 1;
            continue;
        }
        if g.get(i + 1) == Some(&DASH) && g.get(i + 2) == Some(&CLOSE) {
            ranges.push((c, c));
            ranges.push((DASH, DASH));
            i += 2;
            continue;
        }
        if g.get(i + 1) == Some(&DASH) {
            range_start = Some(c);
            i += 2;
            continue;
        }
        ranges.push((c, c));
        i += 1;
    }
    if end_pos < i {
        return Ok(ClassParse::NotAClass);
    }
    if ranges.is_empty() {
        return Ok(ClassParse::Never);
    }
    // `c-]` pushed two entries above where upstream pushes one escaped
    // string (`c\-`); that only matters for the single-literal shortcut,
    // which needs exactly one one-character entry.
    let single = ranges.len() == 1 && ranges[0].0 == ranges[0].1 && !negate;
    if single {
        return Ok(ClassParse::Literal(ranges[0].0, end_pos - pos));
    }
    Ok(ClassParse::Class {
        negate,
        ranges,
        consumed: end_pos - pos,
    })
}

/// The glob regexes minimatch tries before building a segment's regexp.
fn fast_test(p: &str) -> Option<FastTest> {
    let bad = |c: char| matches!(c, '+' | '@' | '!' | '?' | '*' | '[' | '(');
    if !p.is_empty() && p.chars().all(|c| c == '*') {
        return Some(FastTest::Star);
    }
    // ^\*+([^+@!?*[(]*)$
    let stars = p.chars().take_while(|c| *c == '*').count();
    if stars > 0 {
        let ext = &p[stars..];
        if !ext.chars().any(bad) {
            return Some(FastTest::StarDotExt(crate::path::js_to_lower_case(ext)));
        }
    }
    // ^\?+([^+@!?*[(]*)?$
    let q = p.chars().take_while(|c| *c == '?').count();
    if q > 0 {
        let ext = &p[q..];
        if !ext.chars().any(bad) {
            return Some(FastTest::Qmarks(js_len(p), crate::path::js_to_lower_case(ext)));
        }
    }
    // ^\*+\.\*+$
    if let Some((a, b)) = p.split_once('.') {
        if !a.is_empty() && !b.is_empty() && a.chars().all(|c| c == '*') && b.chars().all(|c| c == '*') {
            return Some(FastTest::StarDotStar);
        }
    }
    // ^\.\*+$
    if let Some(rest) = p.strip_prefix('.') {
        if !rest.is_empty() && rest.chars().all(|c| c == '*') {
            return Some(FastTest::DotStar);
        }
    }
    None
}

/// Whether a segment uses extglob syntax (`?(`, `*(`, `+(`, `@(`, `!(`)
/// outside a character class and not escaped, or an escaped `|` (which
/// minimatch leaves unescaped in its regexp).
fn uses_extglob(g: &[u16]) -> Option<&'static str> {
    let mut escaping = false;
    let mut in_brace = false;
    let mut brace_start = 0usize;
    let mut brace_neg = false;
    let mut i = 0;
    while i < g.len() {
        let c = g[i];
        i += 1;
        if escaping {
            escaping = false;
            if c == b'|' as u16 {
                return Some("an escaped | is not supported");
            }
            continue;
        }
        if c == b'\\' as u16 {
            escaping = true;
            continue;
        }
        if in_brace {
            if i == brace_start + 1 {
                if c == b'^' as u16 || c == b'!' as u16 {
                    brace_neg = true;
                }
            } else if c == b']' as u16 && !(i == brace_start + 2 && brace_neg) {
                in_brace = false;
            }
            continue;
        } else if c == b'[' as u16 {
            in_brace = true;
            brace_start = i;
            brace_neg = false;
            continue;
        }
        if matches!(char::from_u32(c as u32), Some('!' | '?' | '+' | '*' | '@')) && g.get(i) == Some(&(b'(' as u16)) {
            return Some("extglob patterns such as +(a|b) are not supported");
        }
    }
    None
}

/// minimatch's `parse` of one path portion.
fn parse_segment(p: &str) -> Result<Seg, Unsupported> {
    if p == "**" {
        return Ok(Seg::Globstar);
    }
    if p.is_empty() {
        return Ok(Seg::Toks {
            no_traversal: false,
            toks: vec![],
        });
    }
    let g = utf16(p);
    if let Some(why) = uses_extglob(&g) {
        return Err(Unsupported(format!("{p:?}: {why}")));
    }
    let fast = fast_test(p);
    // #parseGlob
    let mut toks: Vec<Tok> = Vec::new();
    let mut src = String::new();
    let mut escaping = false;
    let mut in_star = false;
    let only_stars = p.chars().all(|c| c == '*');
    let mut i = 0;
    while i < g.len() {
        let c = g[i];
        if escaping {
            escaping = false;
            if is_reserved_char(c) {
                src.push('\\');
            }
            src.push_str(&String::from_utf16_lossy(&[c]));
            toks.push(Tok::Lit(c));
            i += 1;
            continue;
        }
        if c == b'*' as u16 {
            if !in_star {
                in_star = true;
                toks.push(Tok::Star { nonempty: only_stars });
                src.push_str(if only_stars { "[^/]+?" } else { "[^/]*?" });
            }
            i += 1;
            continue;
        }
        in_star = false;
        if c == b'\\' as u16 {
            if i == g.len() - 1 {
                src.push_str("\\\\");
                toks.push(Tok::Lit(c));
            } else {
                escaping = true;
            }
            i += 1;
            continue;
        }
        if c == b'[' as u16 {
            match parse_class(&g, i)? {
                ClassParse::NotAClass => {}
                ClassParse::Never => {
                    return Ok(match fast {
                        Some(f) => Seg::Fast(f),
                        None => Seg::Never,
                    })
                }
                ClassParse::Literal(u, consumed) => {
                    src.push_str(&re_escaped(u));
                    toks.push(Tok::Lit(u));
                    i += consumed;
                    continue;
                }
                ClassParse::Class {
                    negate,
                    ranges,
                    consumed,
                } => {
                    src.push('[');
                    toks.push(Tok::Class { negate, ranges });
                    i += consumed;
                    continue;
                }
            }
        }
        if c == b'?' as u16 {
            src.push_str("[^/]");
            toks.push(Tok::Any);
            i += 1;
            continue;
        }
        src.push_str(&re_escaped(c));
        toks.push(Tok::Lit(c));
        i += 1;
    }
    if let Some(f) = fast {
        return Ok(Seg::Fast(f));
    }
    // The start-of-pattern guard (dot: true): `.` and `..` match only a
    // pattern that is exactly that.
    let dot_trav_allowed = p == "." || p == "..";
    let aps = |c: Option<char>| matches!(c, Some('[' | '.'));
    let ch = |n: usize| src.chars().nth(n);
    let no_traversal = !dot_trav_allowed
        && (aps(ch(0)) || (src.starts_with("\\.") && aps(ch(2))) || (src.starts_with("\\.\\.") && aps(ch(4))));
    Ok(Seg::Toks { no_traversal, toks })
}

fn class_hit(negate: bool, ranges: &[(u16, u16)], u: u16) -> bool {
    let cu = canon(u);
    let hit = canon_candidates(u)
        .iter()
        .any(|m| canon(*m) == cu && ranges.iter().any(|(lo, hi)| *lo <= *m && *m <= *hi));
    hit != negate
}

fn toks_match(toks: &[Tok], f: &[u16]) -> bool {
    match toks.first() {
        None => f.is_empty(),
        Some(Tok::Lit(c)) => !f.is_empty() && canon(f[0]) == canon(*c) && toks_match(&toks[1..], &f[1..]),
        Some(Tok::Any) => !f.is_empty() && f[0] != b'/' as u16 && toks_match(&toks[1..], &f[1..]),
        Some(Tok::Class { negate, ranges }) => {
            !f.is_empty() && class_hit(*negate, ranges, f[0]) && toks_match(&toks[1..], &f[1..])
        }
        Some(Tok::Star { nonempty }) => {
            let min = usize::from(*nonempty);
            (min..=f.len()).any(|k| toks_match(&toks[1..], &f[k..]))
        }
    }
}

impl Seg {
    fn test(&self, f: &str) -> bool {
        match self {
            Seg::Globstar => false,
            Seg::Never => false,
            Seg::Fast(t) => match t {
                FastTest::Star => !f.is_empty() && f != "." && f != "..",
                FastTest::StarDotExt(ext) => crate::path::js_to_lower_case(f).ends_with(ext.as_str()),
                FastTest::Qmarks(len, ext) => {
                    js_len(f) == *len
                        && f != "."
                        && f != ".."
                        && (ext.is_empty() || crate::path::js_to_lower_case(f).ends_with(ext.as_str()))
                }
                FastTest::StarDotStar => f != "." && f != ".." && f.contains('.'),
                FastTest::DotStar => f != "." && f != ".." && f.starts_with('.'),
            },
            Seg::Toks { no_traversal, toks } => {
                if *no_traversal && (f == "." || f == "..") {
                    return false;
                }
                toks_match(toks, &utf16(f))
            }
        }
    }
}

// ----------------------------------------------------------------------
// minimatch 10: whole patterns
// ----------------------------------------------------------------------

/// One minimatch pattern with the options commonlib uses (`dot`, `nocase`,
/// `flipNegate`): `matches` is true on a hit, negated or not.
#[derive(Debug, Clone)]
pub struct Glob {
    comment: bool,
    empty: bool,
    set: Vec<Vec<Seg>>,
}

/// `split(/\/+/)`
fn slash_split(p: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut prev_slash = false;
    for c in p.chars() {
        if c == '/' {
            if !prev_slash {
                out.push(String::new());
            }
            prev_slash = true;
        } else {
            prev_slash = false;
            out.last_mut().unwrap().push(c);
        }
    }
    out
}

fn level_one_optimize(parts: Vec<String>) -> Vec<String> {
    let mut set: Vec<String> = Vec::new();
    for part in parts {
        let prev = set.last().cloned();
        if part == "**" && prev.as_deref() == Some("**") {
            continue;
        }
        if part == ".." {
            if let Some(pv) = &prev {
                if !pv.is_empty() && pv != ".." && pv != "." && pv != "**" {
                    set.pop();
                    continue;
                }
            }
        }
        set.push(part);
    }
    if set.is_empty() {
        vec![String::new()]
    } else {
        set
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tri {
    False,
    Null,
    True,
}

impl Glob {
    pub fn new(pattern: &str) -> Result<Glob, Unsupported> {
        if pattern.starts_with('#') {
            return Ok(Glob {
                comment: true,
                empty: false,
                set: vec![],
            });
        }
        if pattern.is_empty() {
            return Ok(Glob {
                comment: false,
                empty: true,
                set: vec![],
            });
        }
        // parseNegate: leading `!`s are dropped (flipNegate ignores them).
        let pattern = pattern.trim_start_matches('!');
        let mut seen = HashSet::new();
        let mut set = Vec::new();
        for g in mm_brace_expand(pattern) {
            if !seen.insert(g.clone()) {
                continue;
            }
            let parts = level_one_optimize(slash_split(&g));
            set.push(
                parts
                    .iter()
                    .map(|s| parse_segment(s))
                    .collect::<Result<Vec<Seg>, _>>()?,
            );
        }
        Ok(Glob {
            comment: false,
            empty: false,
            set,
        })
    }

    /// minimatch's `match` (with `flipNegate`: a hit is true).
    pub fn matches(&self, path: &str) -> bool {
        self.match_with(path, false)
    }

    /// `match(path, partial = true)`: `path` (a folder) does not contradict
    /// the pattern, so something below it may match.
    pub fn matches_partial(&self, path: &str) -> bool {
        self.match_with(path, true)
    }

    fn match_with(&self, f: &str, partial: bool) -> bool {
        if self.comment {
            return false;
        }
        if self.empty {
            return f.is_empty();
        }
        if f == "/" && partial {
            return true;
        }
        let file = slash_split(f);
        self.set.iter().any(|p| match_one(&file, p, partial))
    }
}

fn match_one(file: &[String], pattern: &[Seg], partial: bool) -> bool {
    if pattern.iter().any(|s| matches!(s, Seg::Globstar)) {
        return match_globstar(file, pattern, partial, 0, 0);
    }
    match_parts(file, pattern, partial, 0, 0)
}

fn match_parts(file: &[String], pattern: &[Seg], partial: bool, fi0: usize, pi0: usize) -> bool {
    let (mut fi, mut pi) = (fi0, pi0);
    let (fl, pl) = (file.len(), pattern.len());
    while fi < fl && pi < pl {
        let p = &pattern[pi];
        if matches!(p, Seg::Globstar) {
            return false;
        }
        if !p.test(&file[fi]) {
            return false;
        }
        fi += 1;
        pi += 1;
    }
    if fi == fl && pi == pl {
        true
    } else if fi == fl {
        partial
    } else {
        fi == fl - 1 && file[fi].is_empty()
    }
}

fn bad_dot(f: &str) -> bool {
    // dot: true, so only `.` and `..` are bad.
    f == "." || f == ".."
}

fn match_globstar(file: &[String], pattern: &[Seg], partial: bool, mut fi: usize, mut pi: usize) -> bool {
    let firstgs = pattern
        .iter()
        .enumerate()
        .skip(pi)
        .find(|(_, s)| matches!(s, Seg::Globstar))
        .map(|(i, _)| i)
        .unwrap();
    let lastgs = pattern.iter().rposition(|s| matches!(s, Seg::Globstar)).unwrap();
    let (head, body, tail): (&[Seg], &[Seg], &[Seg]) = if partial {
        (&pattern[pi..firstgs], &pattern[firstgs + 1..], &[])
    } else {
        (
            &pattern[pi..firstgs],
            if lastgs > firstgs {
                &pattern[firstgs + 1..lastgs]
            } else {
                &[]
            },
            &pattern[lastgs + 1..],
        )
    };
    if !head.is_empty() {
        let end = (fi + head.len()).min(file.len());
        let file_head = &file[fi.min(end)..end];
        if !match_parts(file_head, head, partial, 0, 0) {
            return false;
        }
        fi += head.len();
        pi += head.len();
    }
    let _ = pi;
    let mut file_tail_match = 0usize;
    if !tail.is_empty() {
        if tail.len() + fi > file.len() {
            return false;
        }
        let mut tail_start = file.len() - tail.len();
        if match_parts(file, tail, partial, tail_start, 0) {
            file_tail_match = tail.len();
        } else {
            if !file[file.len() - 1].is_empty() || fi + tail.len() == file.len() {
                return false;
            }
            tail_start -= 1;
            if !match_parts(file, tail, partial, tail_start, 0) {
                return false;
            }
            file_tail_match = tail.len() + 1;
        }
    }
    if body.is_empty() {
        let mut saw_some = file_tail_match > 0;
        let stop = file.len().saturating_sub(file_tail_match);
        for f in file.iter().take(stop).skip(fi) {
            saw_some = true;
            if bad_dot(f) {
                return false;
            }
        }
        return partial || saw_some;
    }
    // Body sections between globstars, with the last index each may start at.
    let mut sections: Vec<Vec<Seg>> = vec![vec![]];
    let mut non_gs = 0usize;
    let mut sums = vec![0usize];
    for b in body {
        if matches!(b, Seg::Globstar) {
            sums.push(non_gs);
            sections.push(vec![]);
        } else {
            sections.last_mut().unwrap().push(b.clone());
            non_gs += 1;
        }
    }
    let file_length = file.len() as i64 - file_tail_match as i64;
    let n = sections.len();
    let afters: Vec<i64> = sections
        .iter()
        .enumerate()
        .map(|(k, sec)| file_length - (sums[n - 1 - k] as i64 + sec.len() as i64))
        .collect();
    body_sections(file, &sections, &afters, fi, 0, partial, 0, file_tail_match > 0) == Tri::True
}

#[allow(clippy::too_many_arguments)]
fn body_sections(
    file: &[String],
    sections: &[Vec<Seg>],
    afters: &[i64],
    mut fi: usize,
    bi: usize,
    partial: bool,
    depth: usize,
    mut saw_tail: bool,
) -> Tri {
    const MAX_GLOBSTAR_RECURSION: usize = 200;
    let Some(body) = sections.get(bi) else {
        for f in file.iter().skip(fi) {
            saw_tail = true;
            if bad_dot(f) {
                return Tri::False;
            }
        }
        return if saw_tail { Tri::True } else { Tri::False };
    };
    let after = afters[bi];
    while (fi as i64) <= after {
        let end = (fi + body.len()).min(file.len());
        let m = match_parts(&file[..end], body, partial, fi, 0);
        if m && depth < MAX_GLOBSTAR_RECURSION {
            let sub = body_sections(
                file,
                sections,
                afters,
                fi + body.len(),
                bi + 1,
                partial,
                depth + 1,
                saw_tail,
            );
            if sub != Tri::False {
                return sub;
            }
        }
        match file.get(fi) {
            Some(f) if bad_dot(f) => return Tri::False,
            _ => {}
        }
        fi += 1;
    }
    if partial {
        Tri::True
    } else {
        Tri::Null
    }
}

// ----------------------------------------------------------------------
// commonlib: isAccepted / isAcceptedAll
// ----------------------------------------------------------------------

/// One pattern line of an ignore file, compiled the way `isAccepted` uses it.
#[derive(Debug, Clone)]
pub struct Rule {
    /// 1-based line in the ignore file.
    pub line: usize,
    /// The pattern as written (trimmed).
    pub pattern: String,
    /// Starts with `!`: a match accepts the path.
    pub accept: bool,
    /// Ends with `/`: `pattern**` ignores for good (no later rule undoes it).
    dir: Option<Glob>,
    main: Glob,
    /// `pattern/**`, for patterns that do not end with `/`.
    sub: Option<Glob>,
    /// The pattern without its trailing `/` (for folder checks).
    folder: Option<Glob>,
}

/// The patterns of one ignore file.
#[derive(Debug, Clone, Default)]
pub struct IgnoreList {
    pub rules: Vec<Rule>,
}

/// How one ignore file judged a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub accepted: bool,
    /// Index into `IgnoreList::rules` of the deciding rule; `None` when the
    /// path itself is refused (it contains `./` or `../`).
    pub rule: Option<usize>,
}

impl IgnoreList {
    /// Parse an ignore file's text as the plugin reads it (lines split on
    /// `\r?\n`, trimmed; empty lines and `#` comments skipped).
    pub fn parse(text: &str) -> Result<IgnoreList, Unsupported> {
        let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
        IgnoreList::from_lines(&lines)
    }

    pub fn from_lines<S: AsRef<str>>(lines: &[S]) -> Result<IgnoreList, Unsupported> {
        let mut rules = Vec::new();
        for (n, l) in lines.iter().enumerate() {
            let p = js_trim(js_trim(l.as_ref()));
            if p.is_empty() || p.starts_with('#') {
                continue;
            }
            let is_dir = p.ends_with('/');
            let wrap = |e: Unsupported| Unsupported(format!("line {}: {e}", n + 1));
            rules.push(Rule {
                line: n + 1,
                pattern: p.to_string(),
                accept: p.starts_with('!'),
                dir: if is_dir {
                    Some(Glob::new(&format!("{p}**")).map_err(wrap)?)
                } else {
                    None
                },
                main: Glob::new(p).map_err(wrap)?,
                sub: if is_dir {
                    None
                } else {
                    Some(Glob::new(&format!("{p}/**")).map_err(wrap)?)
                },
                folder: if is_dir {
                    Some(Glob::new(&p[..p.len() - 1]).map_err(wrap)?)
                } else {
                    None
                },
            });
        }
        Ok(IgnoreList { rules })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// `isAccepted(path, lines)`: `None` when no pattern mentions the path.
    pub fn judge(&self, path: &str) -> Option<Verdict> {
        if path.contains("./") || path.contains("../") {
            return Some(Verdict {
                accepted: false,
                rule: None,
            });
        }
        let mut result: Option<Verdict> = None;
        for (i, r) in self.rules.iter().enumerate() {
            if let Some(d) = &r.dir {
                if d.matches(path) {
                    return Some(Verdict {
                        accepted: false,
                        rule: Some(i),
                    });
                }
            }
            let matched = r.main.matches(path) || r.sub.as_ref().is_some_and(|s| s.matches(path));
            if matched {
                result = Some(Verdict {
                    accepted: r.accept,
                    rule: Some(i),
                });
            }
        }
        result
    }

    /// Whether this file can accept some path below the folder `dir`
    /// (relative to the file's folder; "" is the file's own folder).
    pub fn may_accept_under(&self, dir: &str) -> bool {
        self.rules.iter().any(|r| {
            r.accept
                && (dir.is_empty()
                    || r.main.matches_partial(dir)
                    || r.sub.as_ref().is_some_and(|s| s.matches_partial(dir))
                    || r.dir.as_ref().is_some_and(|d| d.matches_partial(dir)))
        })
    }

    /// The rule that ignores everything below the folder `dir` (relative to
    /// the file's folder; "" is its own folder), if one surely does.
    pub fn ignores_all_under(&self, dir: &str) -> Option<usize> {
        if dir.is_empty() {
            // Every path in the file's own folder: `*` or `**` with no `!`.
            if self.rules.iter().any(|r| r.accept) {
                return None;
            }
            return self.rules.iter().position(|r| r.pattern == "*" || r.pattern == "**");
        }
        // `dir/` style: decided at once, whatever follows.
        let probe = format!("{dir}/x");
        if let Some(i) = self.rules.iter().position(|r| {
            r.folder.as_ref().is_some_and(|f| f.matches(dir)) && r.dir.as_ref().is_some_and(|d| d.matches(&probe))
        }) {
            return Some(i);
        }
        if self.may_accept_under(dir) {
            return None;
        }
        // Any other rule that matches the folder covers `folder/**`.
        self.rules.iter().position(|r| {
            !r.accept && r.dir.is_none() && r.main.matches(dir) && r.sub.as_ref().is_some_and(|x| x.matches(&probe))
        })
    }
}

/// What reading one ignore file gave.
#[derive(Debug, Clone)]
pub enum Lookup<L> {
    /// No such file.
    Missing,
    Found(L),
    /// It exists but cannot be read (or holds a pattern this port does not
    /// evaluate).
    Error(String),
}

/// How `isAcceptedAll` decided a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// No ignore file mentions it.
    Accepted,
    /// Decided by the ignore file at this path (vault-relative, as
    /// commonlib asks for it: the root file is `/.gitignore`).
    By { file: String, verdict: Verdict },
    /// An ignore file on the way could not be read: undecidable.
    Unknown { file: String, why: String },
}

impl Decision {
    pub fn accepted(&self) -> Option<bool> {
        match self {
            Decision::Accepted => Some(true),
            Decision::By { verdict, .. } => Some(verdict.accepted),
            Decision::Unknown { .. } => None,
        }
    }
}

/// The folders whose ignore files `isAcceptedAll` reads for `path`, nearest
/// first ("" is the vault root).
pub fn intermediate_dirs(path: &str) -> Vec<String> {
    // pathBase.split("/").reduce((p, c) => [...p, p.at(-1) + "/" + c], [""])
    //   .map((e) => e.substring(1)), then unique, then reverse.
    let base = match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    };
    let mut acc: Vec<String> = vec![String::new()];
    for seg in base.split('/') {
        let next = format!("{}/{seg}", acc.last().unwrap());
        acc.push(next);
    }
    let mut seen = HashSet::new();
    let mut dirs: Vec<String> = acc
        .into_iter()
        .map(|e| e.chars().skip(1).collect::<String>())
        .filter(|d| seen.insert(d.clone()))
        .collect();
    dirs.reverse();
    dirs
}

/// The path commonlib asks for: `<dir>/<name>`, so `/.gitignore` at the root.
pub fn ignore_file_key(dir: &str, name: &str) -> String {
    format!("{dir}/{name}")
}

/// `isAcceptedAll(path, ignoreFiles, getList)`, except that an unreadable
/// ignore file makes the path undecidable instead of being skipped.
/// `get` receives commonlib's key for each file (see [`ignore_file_key`]).
pub fn decide<L: std::ops::Deref<Target = IgnoreList>>(
    path: &str,
    names: &[String],
    mut get: impl FnMut(&str, &str) -> Lookup<L>,
) -> Decision {
    for dir in intermediate_dirs(path) {
        for name in names {
            let key = ignore_file_key(&dir, name);
            let list = match get(&dir, name) {
                Lookup::Missing => continue,
                Lookup::Error(why) => return Decision::Unknown { file: key, why },
                Lookup::Found(l) => l,
            };
            let rel = if dir.is_empty() { path } else { &path[dir.len() + 1..] };
            if let Some(verdict) = list.judge(rel) {
                return Decision::By { file: key, verdict };
            }
        }
    }
    Decision::Accepted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(lines: &[&str], path: &str) -> Option<bool> {
        IgnoreList::from_lines(lines).unwrap().judge(path).map(|v| v.accepted)
    }

    #[test]
    fn basics() {
        assert_eq!(acc(&["*.log"], "a.log"), Some(false));
        assert_eq!(acc(&["*.log"], "sub/a.log"), None);
        assert_eq!(acc(&["build/"], "build/x.md"), Some(false));
        assert_eq!(acc(&["build/"], "build"), None);
        assert_eq!(acc(&["build/", "!build/keep.md"], "build/keep.md"), Some(false));
        assert_eq!(acc(&["secret", "!secret/ok.md"], "secret/ok.md"), Some(true));
        assert_eq!(acc(&["*.md"], "Mr./note.md"), Some(false));
        assert_eq!(acc(&["{a,b}.md"], "b.md"), Some(false));
        assert_eq!(acc(&["a{1..3}.md"], "a2.md"), Some(false));
        assert_eq!(acc(&["a{1..3}.md"], "a4.md"), None);
        assert_eq!(acc(&["**"], ".hidden"), Some(false));
        assert!(IgnoreList::from_lines(&["+(a|b)"]).is_err());
    }

    #[test]
    fn dirs_nearest_first() {
        assert_eq!(intermediate_dirs("a.md"), vec![""]);
        assert_eq!(intermediate_dirs("a/b/c.md"), vec!["a/b", "a", ""]);
    }

    #[test]
    fn folder_checks() {
        let l = IgnoreList::from_lines(&["node_modules/", "*.log"]).unwrap();
        assert_eq!(l.ignores_all_under("node_modules"), Some(0));
        assert_eq!(l.ignores_all_under("x/node_modules"), None);
        let l = IgnoreList::from_lines(&["secret", "!secret/ok.md"]).unwrap();
        assert_eq!(l.ignores_all_under("secret"), None);
        let l = IgnoreList::from_lines(&["secret", "!other.md"]).unwrap();
        assert_eq!(l.ignores_all_under("secret"), Some(0));
        let l = IgnoreList::from_lines(&["*"]).unwrap();
        assert_eq!(l.ignores_all_under(""), Some(0));
    }
}
