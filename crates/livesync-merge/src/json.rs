//! Upstream's object merge (`ConflictManager.mergeObject` with `utils.patch.ts`)
//! on an order-preserving JSON value, written back in the file's own style.

use crate::{Deviation, PathSeg, Which};

/// A JSON value that keeps object key order and each number's source text.
#[derive(Clone, Debug)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl PartialEq for Json {
    fn eq(&self, other: &Json) -> bool {
        match (self, other) {
            (Json::Null, Json::Null) => true,
            (Json::Bool(a), Json::Bool(b)) => a == b,
            (Json::Num(a), Json::Num(b)) => num_eq(a, b),
            (Json::Str(a), Json::Str(b)) => a == b,
            (Json::Arr(a), Json::Arr(b)) => a == b,
            (Json::Obj(a), Json::Obj(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|((ka, va), (kb, vb))| ka == kb && va == vb)
            }
            _ => false,
        }
    }
}

/// Upstream compares numbers through `JSON.stringify`, i.e. as doubles. Two
/// texts are equal here only when they are the same text, or both are finite
/// doubles JavaScript treats as equal, with the same sign (`0` and `-0`
/// differ, which `JSON.stringify` hides), and neither has more digits than a
/// double holds. So a change is never dropped silently: `1e400` against
/// `2e400` (both infinite as doubles) is a change, as is one in the 17th digit.
fn num_eq(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let digits = |s: &str| {
        let mantissa = s.split(['e', 'E']).next().unwrap_or("");
        mantissa
            .chars()
            .filter(char::is_ascii_digit)
            .skip_while(|c| *c == '0')
            .count()
    };
    if digits(a) > 15 || digits(b) > 15 {
        return false;
    }
    matches!((a.parse::<f64>(), b.parse::<f64>()),
        (Ok(x), Ok(y)) if x.is_finite() && y.is_finite() && x == y && x.is_sign_negative() == y.is_sign_negative())
}

impl Json {
    /// JavaScript's `typeof`, with arrays apart.
    fn kind(&self) -> u8 {
        match self {
            Json::Null => 0,
            Json::Bool(_) => 1,
            Json::Num(_) => 2,
            Json::Str(_) => 3,
            Json::Arr(_) => 4,
            Json::Obj(_) => 5,
        }
    }
    fn is_object_like(&self) -> bool {
        matches!(self, Json::Null | Json::Arr(_) | Json::Obj(_))
    }
}

fn obj_get<'a>(o: &'a [(String, Json)], k: &str) -> Option<&'a Json> {
    o.iter().find(|(kk, _)| kk == k).map(|(_, v)| v)
}

// ---------- parsing ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ParseError {
    /// Not JSON (`JSON.parse` would throw too).
    Invalid,
    /// JSON that `JSON.parse` accepts but this merge does not take on: a key
    /// twice in one object, or a key with upstream's patch marker (U+0001).
    Unsupported,
}

pub(crate) fn parse(s: &str) -> Result<Json, ParseError> {
    let mut p = Parser {
        b: s.as_bytes(),
        s,
        i: 0,
        unsupported: false,
    };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(ParseError::Invalid);
    }
    if p.unsupported {
        return Err(ParseError::Unsupported);
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    s: &'a str,
    i: usize,
    unsupported: bool,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn lit(&mut self, word: &str, v: Json) -> Result<Json, ParseError> {
        if self.s[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(ParseError::Invalid)
        }
    }
    fn value(&mut self, depth: usize) -> Result<Json, ParseError> {
        if depth > 512 {
            return Err(ParseError::Unsupported);
        }
        match self.b.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let mut o: Vec<(String, Json)> = Vec::new();
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                self.ws();
                if self.eat(b'}') {
                    return Ok(Json::Obj(o));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    if !self.eat(b':') {
                        return Err(ParseError::Invalid);
                    }
                    self.ws();
                    let v = self.value(depth + 1)?;
                    if k.contains('\u{1}') || !seen.insert(k.clone()) {
                        self.unsupported = true;
                    }
                    o.push((k, v));
                    self.ws();
                    if self.eat(b',') {
                        continue;
                    }
                    if self.eat(b'}') {
                        return Ok(Json::Obj(o));
                    }
                    return Err(ParseError::Invalid);
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.eat(b']') {
                    return Ok(Json::Arr(a));
                }
                loop {
                    self.ws();
                    a.push(self.value(depth + 1)?);
                    self.ws();
                    if self.eat(b',') {
                        continue;
                    }
                    if self.eat(b']') {
                        return Ok(Json::Arr(a));
                    }
                    return Err(ParseError::Invalid);
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'n') => self.lit("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(ParseError::Invalid),
        }
    }
    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.i;
        self.eat(b'-');
        let digits = |p: &mut Self| {
            let s = p.i;
            while p.i < p.b.len() && p.b[p.i].is_ascii_digit() {
                p.i += 1;
            }
            p.i - s
        };
        if self.eat(b'0') {
        } else if digits(self) == 0 {
            return Err(ParseError::Invalid);
        }
        if self.eat(b'.') && digits(self) == 0 {
            return Err(ParseError::Invalid);
        }
        if self.eat(b'e') || self.eat(b'E') {
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            if digits(self) == 0 {
                return Err(ParseError::Invalid);
            }
        }
        Ok(Json::Num(self.s[start..self.i].to_string()))
    }
    fn hex4(&mut self) -> Result<u32, ParseError> {
        let h = self.s.get(self.i..self.i + 4).ok_or(ParseError::Invalid)?;
        let v = u32::from_str_radix(h, 16).map_err(|_| ParseError::Invalid)?;
        if !h.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(ParseError::Invalid);
        }
        self.i += 4;
        Ok(v)
    }
    fn string(&mut self) -> Result<String, ParseError> {
        if !self.eat(b'"') {
            return Err(ParseError::Invalid);
        }
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.b.len() && self.b[self.i] != b'"' && self.b[self.i] != b'\\' && self.b[self.i] >= 0x20 {
                self.i += 1;
            }
            out.push_str(&self.s[start..self.i]);
            match self.b.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = *self.b.get(self.i).ok_or(ParseError::Invalid)?;
                    self.i += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                // A lone surrogate is valid in JavaScript but not
                                // in a Rust string: not taken on.
                                if !self.s[self.i..].starts_with("\\u") {
                                    return Err(ParseError::Unsupported);
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(ParseError::Unsupported);
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return Err(ParseError::Unsupported);
                            } else {
                                hi
                            };
                            out.push(char::from_u32(cp).ok_or(ParseError::Invalid)?);
                        }
                        _ => return Err(ParseError::Invalid),
                    }
                }
                _ => return Err(ParseError::Invalid),
            }
        }
    }
}

// ---------- writing ----------

/// How a file is laid out: `JSON.stringify(v, null, indent)` plus the end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Style {
    /// `None`: compact, as `JSON.stringify(v)`.
    pub indent: Option<String>,
    pub eol: &'static str,
    pub final_newline: bool,
}

impl Style {
    pub fn detect(text: &str) -> Style {
        let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let final_newline = text.ends_with('\n');
        let body = text.trim_end_matches(['\n', '\r']);
        let indent = if !body.contains('\n') {
            None
        } else {
            body.split('\n')
                .skip(1)
                .map(|l| l.trim_end_matches('\r'))
                .find(|l| l.starts_with([' ', '\t']) && !l.trim().is_empty())
                .map(|l| l[..l.len() - l.trim_start_matches([' ', '\t']).len()].to_string())
                .or_else(|| Some("  ".to_string()))
        };
        Style {
            indent,
            eol,
            final_newline,
        }
    }

    pub fn write(&self, v: &Json) -> String {
        let mut out = String::new();
        write_value(v, self, 0, &mut out);
        if self.final_newline {
            out.push_str(self.eol);
        }
        out
    }
}

fn write_value(v: &Json, st: &Style, depth: usize, out: &mut String) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) => out.push_str(n),
        Json::Str(s) => write_str(s, out),
        Json::Arr(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(st, depth + 1, out);
                write_value(x, st, depth + 1, out);
            }
            newline(st, depth, out);
            out.push(']');
        }
        Json::Obj(o) => {
            if o.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(st, depth + 1, out);
                write_str(k, out);
                out.push(':');
                if st.indent.is_some() {
                    out.push(' ');
                }
                write_value(x, st, depth + 1, out);
            }
            newline(st, depth, out);
            out.push('}');
        }
    }
}

fn newline(st: &Style, depth: usize, out: &mut String) {
    if let Some(ind) = &st.indent {
        out.push_str(st.eol);
        for _ in 0..depth {
            out.push_str(ind);
        }
    }
}

/// As `JSON.stringify` escapes strings.
fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------- patches ----------

/// One key's change, as `generatePatchObj` encodes it.
#[derive(Clone, Debug)]
enum Patch {
    /// A new key, or a primitive changed in place: the value as is.
    Set(Json),
    /// `{"\u0001__SWAP": value}`: replaced whole.
    Swap(Json),
    /// `"\u0001__DELETED"`.
    Deleted,
    /// An object patched key by key.
    Nested(Vec<(String, Patch)>),
    /// `{"\u0001__ARRAY": patch}`: an array of elements with `id`, patched as an
    /// object keyed by id.
    IdArray(Vec<(String, Patch)>),
}

/// Upstream's patch has a plain array (one whose elements do not all have an
/// `id`) swapped whole even when it did not change, so a plain array changed on
/// one side always conflicts there. `ours` leaves unchanged arrays out, so a
/// one-sided change is taken ("other arrays are replaced whole" means a
/// changed array).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Upstream,
    Ours,
}

struct Shape;

/// `"id" in e` for every element. `Err` where upstream would throw (`null`).
fn all_have_id(a: &[Json]) -> Result<bool, Shape> {
    for e in a {
        match e {
            Json::Null => return Err(Shape),
            Json::Obj(o) if obj_get(o, "id").is_some() => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// `String(e.id)` for the ids this merge takes on: strings, and integers
/// written plainly. Anything else is `Err` (open).
fn id_key(e: &Json) -> Result<String, Shape> {
    let Json::Obj(o) = e else { return Err(Shape) };
    match obj_get(o, "id") {
        Some(Json::Str(s)) => Ok(s.clone()),
        Some(Json::Num(n)) if n.trim_start_matches('-').bytes().all(|c| c.is_ascii_digit()) && n.len() < 16 => {
            Ok(n.clone())
        }
        _ => Err(Shape),
    }
}

/// `unorderedArrayToObject`. Two elements with one id would collapse into one
/// upstream; that is `Err` here.
fn id_object(a: &[Json]) -> Result<Vec<(String, Json)>, Shape> {
    let mut o: Vec<(String, Json)> = Vec::with_capacity(a.len());
    let mut seen = std::collections::HashSet::new();
    for e in a {
        let k = id_key(e)?;
        if !seen.insert(k.clone()) {
            return Err(Shape);
        }
        o.push((k, e.clone()));
    }
    Ok(o)
}

/// An object's keys, for lookups in linear time overall.
fn index(o: &[(String, Json)]) -> std::collections::HashMap<&str, &Json> {
    o.iter().map(|(k, v)| (k.as_str(), v)).collect()
}

fn gen_patch(from: &[(String, Json)], to: &[(String, Json)], mode: Mode) -> Result<Vec<(String, Patch)>, Shape> {
    let mut ret = Vec::new();
    let (fi, ti) = (index(from), index(to));
    for (k, value) in to {
        let Some(v) = fi.get(k.as_str()).copied() else {
            ret.push((k.clone(), Patch::Set(value.clone())));
            continue;
        };
        let p = if v.kind() != value.kind() && !(v.is_object_like() && value.is_object_like()) {
            Some(Patch::Swap(value.clone()))
        } else {
            match (v, value) {
                (Json::Null, Json::Null) => None,
                (Json::Null, _) | (_, Json::Null) => Some(Patch::Swap(value.clone())),
                (Json::Obj(a), Json::Obj(b)) => {
                    let sub = gen_patch(a, b, mode)?;
                    (!sub.is_empty()).then_some(Patch::Nested(sub))
                }
                (Json::Arr(a), Json::Arr(b)) => {
                    if all_have_id(a)? && all_have_id(b)? {
                        let sub = gen_patch(&id_object(a)?, &id_object(b)?, mode)?;
                        (!sub.is_empty()).then_some(Patch::IdArray(sub))
                    } else if mode == Mode::Ours && a == b {
                        None
                    } else {
                        Some(Patch::Swap(value.clone()))
                    }
                }
                (Json::Arr(_), Json::Obj(_)) | (Json::Obj(_), Json::Arr(_)) => Some(Patch::Swap(value.clone())),
                (a, b) => (a != b).then(|| Patch::Set(value.clone())),
            }
        };
        if let Some(p) = p {
            ret.push((k.clone(), p));
        }
    }
    for (k, _) in from {
        if !ti.contains_key(k.as_str()) {
            ret.push((k.clone(), Patch::Deleted));
        }
    }
    Ok(ret)
}

/// One flattened change, compared between the sides.
#[derive(Clone, Debug, PartialEq)]
enum Leaf {
    Deleted,
    Value(Json),
    /// A new, empty object: it conflicts with nothing beneath it.
    EmptyObject,
}

/// Flatten a patch into leaves. Unlike upstream's `flattenObject` (which joins
/// keys with "." and walks into a swapped value), a swap is one leaf, so two
/// different replacements of one key conflict.
fn flatten(patch: &[(String, Patch)], path: &mut Vec<PathSeg>, id_level: bool, out: &mut Vec<(Vec<PathSeg>, Leaf)>) {
    for (k, p) in patch {
        path.push(if id_level {
            PathSeg::Id(k.clone())
        } else {
            PathSeg::Key(k.clone())
        });
        match p {
            Patch::Deleted => out.push((path.clone(), Leaf::Deleted)),
            Patch::Swap(v) => out.push((path.clone(), Leaf::Value(v.clone()))),
            Patch::Set(Json::Obj(o)) => {
                if o.is_empty() {
                    out.push((path.clone(), Leaf::EmptyObject));
                } else {
                    let sub: Vec<(String, Patch)> = o.iter().map(|(k, v)| (k.clone(), Patch::Set(v.clone()))).collect();
                    flatten(&sub, path, false, out);
                }
            }
            Patch::Set(v) => out.push((path.clone(), Leaf::Value(v.clone()))),
            Patch::Nested(sub) => flatten(sub, path, false, out),
            Patch::IdArray(sub) => flatten(sub, path, true, out),
        }
        path.pop();
    }
}

/// The paths where both sides changed the same thing differently: the same
/// leaf to different values, or one side a key and the other something under it.
/// In the order of the left side's leaves.
fn overlaps(left: &[(Vec<PathSeg>, Leaf)], right: &[(Vec<PathSeg>, Leaf)]) -> Vec<Vec<PathSeg>> {
    use std::collections::{HashMap, HashSet};
    let rmap: HashMap<&[PathSeg], &Leaf> = right.iter().map(|(p, v)| (p.as_slice(), v)).collect();
    // Every proper prefix of a right path.
    let mut rpre: HashSet<&[PathSeg]> = HashSet::new();
    for (p, _) in right {
        for n in 0..p.len() {
            rpre.insert(&p[..n]);
        }
    }
    let mut found: Vec<Vec<PathSeg>> = Vec::new();
    let mut seen: HashSet<Vec<PathSeg>> = HashSet::new();
    let mut hit = |p: &[PathSeg], found: &mut Vec<Vec<PathSeg>>| {
        if seen.insert(p.to_vec()) {
            found.push(p.to_vec());
        }
    };
    for (lp, lv) in left {
        // A right leaf under this one, or this one changed differently.
        if (*lv != Leaf::EmptyObject && rpre.contains(lp.as_slice()))
            || rmap.get(lp.as_slice()).is_some_and(|rv| *rv != lv)
        {
            hit(lp, &mut found);
        }
        // A right leaf above this one.
        for n in 0..lp.len() {
            if rmap.get(&lp[..n]).is_some_and(|rv| **rv != Leaf::EmptyObject) {
                hit(&lp[..n], &mut found);
            }
        }
    }
    found
}

/// Upstream's own conflict test: `flattenObject` paths joined with ".", values
/// compared with JavaScript's `==`. Used only to tell where this merge's result
/// differs from upstream's.
fn upstream_conflicts(left: &[(String, Patch)], right: &[(String, Patch)]) -> bool {
    let (mut l, mut r) = (Vec::new(), Vec::new());
    up_flatten(left, "", &mut l);
    up_flatten(right, "", &mut r);
    let lm: std::collections::HashMap<&str, &UpVal> = l.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let mut rm: std::collections::HashMap<&str, &UpVal> = r.iter().map(|(k, v)| (k.as_str(), v)).collect();
    for (k, v) in &lm {
        if rm.get(k).is_some_and(|rv| loose_eq(rv, v)) {
            rm.remove(k);
        }
    }
    rm.iter().any(|(k, v)| lm.get(k).is_some_and(|lv| !loose_eq(lv, v)))
}

#[derive(Debug)]
enum UpVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
}

fn up_flatten(patch: &[(String, Patch)], path: &str, out: &mut Vec<(String, UpVal)>) {
    let join = |k: &str| {
        if path.is_empty() {
            k.to_string()
        } else {
            format!("{path}.{k}")
        }
    };
    for (k, p) in patch {
        let here = join(k);
        match p {
            Patch::Deleted => out.push((here, UpVal::Str("\u{1}__DELETED".into()))),
            Patch::Swap(v) => up_flatten_value(v, &format!("{here}.\u{1}__SWAP"), out),
            Patch::Set(v) => up_flatten_value(v, &here, out),
            Patch::Nested(sub) => up_flatten(sub, &here, out),
            Patch::IdArray(sub) => up_flatten(sub, &format!("{here}.\u{1}__ARRAY"), out),
        }
    }
}

fn up_flatten_value(v: &Json, path: &str, out: &mut Vec<(String, UpVal)>) {
    match v {
        Json::Null => out.push((path.into(), UpVal::Null)),
        Json::Bool(b) => out.push((path.into(), UpVal::Bool(*b))),
        Json::Num(n) => out.push((path.into(), UpVal::Num(n.parse().unwrap_or(f64::NAN)))),
        Json::Str(s) => out.push((path.into(), UpVal::Str(s.clone()))),
        Json::Arr(_) => out.push((
            path.into(),
            UpVal::Str(
                Style {
                    indent: None,
                    eol: "\n",
                    final_newline: false,
                }
                .write(v),
            ),
        )),
        Json::Obj(o) => {
            for (k, x) in o {
                up_flatten_value(x, &format!("{path}.{k}"), out);
            }
        }
    }
}

/// JavaScript's `==` on the values `flattenObject` yields.
fn loose_eq(a: &UpVal, b: &UpVal) -> bool {
    fn to_num(v: &UpVal) -> Option<f64> {
        match v {
            UpVal::Null => None,
            UpVal::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            UpVal::Num(n) => Some(*n),
            UpVal::Str(s) => {
                let t = s.trim();
                if t.is_empty() {
                    Some(0.0)
                } else {
                    t.parse::<f64>().ok().filter(|_| !t.contains(['i', 'I', 'n', 'N']))
                }
            }
        }
    }
    match (a, b) {
        (UpVal::Null, UpVal::Null) => true,
        (UpVal::Null, _) | (_, UpVal::Null) => false,
        (UpVal::Str(x), UpVal::Str(y)) => x == y,
        _ => matches!((to_num(a), to_num(b)), (Some(x), Some(y)) if x == y),
    }
}

/// `applyPatch`. `Err` where upstream throws, and where it would write a patch
/// itself into the file (a nested patch on a key the other side deleted).
fn apply(target: &mut Vec<(String, Json)>, patch: &[(String, Patch)]) -> Result<(), Shape> {
    let mut at: std::collections::HashMap<String, usize> =
        target.iter().enumerate().map(|(i, (k, _))| (k.clone(), i)).collect();
    let mut gone = vec![false; target.len()];
    let set = |target: &mut Vec<(String, Json)>,
               at: &mut std::collections::HashMap<String, usize>,
               gone: &mut Vec<bool>,
               k: &str,
               v: Json| match at.get(k) {
        Some(&i) => target[i].1 = v,
        None => {
            at.insert(k.to_string(), target.len());
            target.push((k.to_string(), v));
            gone.push(false);
        }
    };
    for (k, p) in patch {
        match p {
            Patch::Deleted => {
                if let Some(i) = at.remove(k.as_str()) {
                    gone[i] = true;
                }
            }
            Patch::Swap(v) => set(target, &mut at, &mut gone, k, v.clone()),
            Patch::Set(v) => match at.get(k.as_str()) {
                Some(&i) => apply_raw_into(&mut target[i].1, v)?,
                None => set(target, &mut at, &mut gone, k, v.clone()),
            },
            Patch::Nested(sub) => match at.get(k.as_str()).map(|&i| &mut target[i].1) {
                Some(Json::Obj(o)) => apply(o, sub)?,
                _ => return Err(Shape),
            },
            Patch::IdArray(sub) => {
                let cur = match at.get(k.as_str()).map(|&i| &target[i].1) {
                    None => Vec::new(),
                    Some(Json::Arr(a)) => a.clone(),
                    Some(_) => return Err(Shape),
                };
                let mut as_obj = id_object(&cur)?;
                apply(&mut as_obj, sub)?;
                let mut arr = Vec::with_capacity(as_obj.len());
                for (id, e) in as_obj {
                    if id_key(&e)? != id {
                        return Err(Shape);
                    }
                    arr.push(e);
                }
                set(target, &mut at, &mut gone, k, Json::Arr(arr));
            }
        }
    }
    if gone.iter().any(|g| *g) {
        let mut i = 0;
        target.retain(|_| {
            i += 1;
            !gone[i - 1]
        });
    }
    Ok(())
}

/// A raw value from a patch: set it, or, where both sides added the key,
/// merge objects key by key as upstream does.
fn apply_raw_into(slot: &mut Json, v: &Json) -> Result<(), Shape> {
    match v {
        Json::Obj(vo) => match slot {
            Json::Obj(o) => {
                let patch: Vec<(String, Patch)> = vo.iter().map(|(k, x)| (k.clone(), Patch::Set(x.clone()))).collect();
                apply(o, &patch)
            }
            _ => Err(Shape),
        },
        Json::Arr(_) => {
            if *slot == *v {
                Ok(())
            } else {
                Err(Shape)
            }
        }
        _ => {
            *slot = v.clone();
            Ok(())
        }
    }
}

pub(crate) enum ObjectOutcome {
    Merged(String),
    Open(Vec<Vec<PathSeg>>),
    Unparsable(Which, ParseError),
    Shape,
}

pub(crate) struct ObjectMerge {
    pub outcome: ObjectOutcome,
    pub deviations: Vec<Deviation>,
}

/// What upstream would do with the same input, as far as this port can tell:
/// `None` where it cannot (an input shape this merge does not take on).
fn upstream_result(
    bw: &[(String, Json)],
    ow: &[(String, Json)],
    tw: &[(String, Json)],
    ours_first: bool,
) -> Option<Option<Json>> {
    let l = gen_patch(bw, ow, Mode::Upstream).ok()?;
    let r = gen_patch(bw, tw, Mode::Upstream).ok()?;
    if upstream_conflicts(&l, &r) {
        return Some(None);
    }
    let mut out = bw.to_vec();
    let order = if ours_first { [&l, &r] } else { [&r, &l] };
    for p in order {
        apply(&mut out, p).ok()?;
    }
    Some(obj_get(&out, "data").cloned())
}

/// The object merge. `ours_first` is upstream's mtime order (older patch
/// first); `style` is the layout the result is written in.
pub(crate) fn merge(base: &str, ours: &str, theirs: &str, ours_first: bool, style: Style) -> ObjectMerge {
    let mut parsed = Vec::with_capacity(3);
    for (w, s) in [(Which::Base, base), (Which::Ours, ours), (Which::Theirs, theirs)] {
        match parse(s) {
            Ok(v) => parsed.push(vec![("data".to_string(), v)]),
            Err(e) => {
                return ObjectMerge {
                    outcome: ObjectOutcome::Unparsable(w, e),
                    deviations: vec![Deviation::UnparsableJson],
                }
            }
        }
    }
    let (bw, ow, tw) = (&parsed[0], &parsed[1], &parsed[2]);
    let upstream = upstream_result(bw, ow, tw, ours_first);
    // Ours opens: a deviation unless upstream opens too.
    let open = |outcome: ObjectOutcome| ObjectMerge {
        outcome,
        deviations: if upstream == Some(None) {
            Vec::new()
        } else {
            vec![Deviation::StricterObjectCheck]
        },
    };

    let (Ok(lp), Ok(rp)) = (gen_patch(bw, ow, Mode::Ours), gen_patch(bw, tw, Mode::Ours)) else {
        return open(ObjectOutcome::Shape);
    };
    let (mut lf, mut rf) = (Vec::new(), Vec::new());
    flatten(&lp, &mut Vec::new(), false, &mut lf);
    flatten(&rp, &mut Vec::new(), false, &mut rf);
    let found = overlaps(&lf, &rf);
    if !found.is_empty() {
        let strip = |p: Vec<PathSeg>| p.into_iter().skip(1).collect();
        return open(ObjectOutcome::Open(found.into_iter().map(strip).collect()));
    }
    let mut out = bw.clone();
    let order = if ours_first { [&lp, &rp] } else { [&rp, &lp] };
    for p in order {
        if apply(&mut out, p).is_err() {
            return open(ObjectOutcome::Shape);
        }
    }
    let value = obj_get(&out, "data").cloned().unwrap_or(Json::Null);
    // The result is one side's value: that side's bytes as they are, so a
    // caller comparing with that side sees its own file.
    let side = [(ow, ours), (tw, theirs)]
        .into_iter()
        .find(|(w, _)| obj_get(w, "data") == Some(&value))
        .map(|(_, text)| text.to_string());
    let mut deviations = Vec::new();
    if upstream.as_ref() != Some(&Some(value.clone())) {
        deviations.push(Deviation::ObjectResultDiffers);
    }
    if has_index_keys(&out) {
        deviations.push(Deviation::NumericKeyOrder);
    }
    ObjectMerge {
        outcome: ObjectOutcome::Merged(side.unwrap_or_else(|| style.write(&value))),
        deviations,
    }
}

/// [`upstream_result`] on the texts, written as `JSON.stringify` writes it.
/// `None` where the model does not cover the input (any side that does not
/// parse here, which upstream reads as `{}`, or a shape the patch rejects).
pub(crate) fn upstream_merge(base: &str, ours: &str, theirs: &str, ours_first: bool) -> Option<Option<String>> {
    let wrap = |s: &str| parse(s).ok().map(|v| vec![("data".to_string(), v)]);
    let (bw, ow, tw) = (wrap(base)?, wrap(ours)?, wrap(theirs)?);
    let compact = Style {
        indent: None,
        eol: "\n",
        final_newline: false,
    };
    upstream_result(&bw, &ow, &tw, ours_first).map(|r| r.map(|v| compact.write(&v)))
}

/// JavaScript lists integer-like keys first, in numeric order, so upstream
/// reorders objects (and id-arrays) that have them. This merge keeps the order.
fn has_index_keys(v: &[(String, Json)]) -> bool {
    fn is_index(k: &str) -> bool {
        k == "0"
            || (!k.starts_with('0')
                && k.len() <= 10
                && k.bytes().all(|c| c.is_ascii_digit())
                && k.parse::<u64>().is_ok_and(|n| n < u32::MAX as u64))
    }
    fn walk(v: &Json) -> bool {
        match v {
            Json::Obj(o) => o.iter().any(|(k, x)| is_index(k) || walk(x)),
            Json::Arr(a) => {
                a.iter().any(|e| match e {
                    Json::Obj(o) => {
                        matches!(obj_get(o, "id"), Some(Json::Str(s)) if is_index(s))
                            || matches!(obj_get(o, "id"), Some(Json::Num(_)))
                    }
                    _ => false,
                }) || a.iter().any(walk)
            }
            _ => false,
        }
    }
    v.iter().any(|(_, x)| walk(x))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_write_round_trip() {
        let s = "{\n  \"a\": [\n    1,\n    {\n      \"b\": \"x\\ny\"\n    }\n  ],\n  \"c\": {},\n  \"d\": []\n}\n";
        let v = parse(s).unwrap();
        assert_eq!(Style::detect(s).write(&v), s);
        let tab = "{\n\t\"a\": 1\n}";
        assert_eq!(Style::detect(tab).write(&parse(tab).unwrap()), tab);
        assert_eq!(
            Style::detect("{\"a\":1}").write(&parse("{\"a\":1}").unwrap()),
            "{\"a\":1}"
        );
    }

    #[test]
    fn parse_rejects() {
        assert_eq!(parse("{").unwrap_err(), ParseError::Invalid);
        assert_eq!(parse("").unwrap_err(), ParseError::Invalid);
        assert_eq!(parse("[1,]").unwrap_err(), ParseError::Invalid);
        assert_eq!(parse("01").unwrap_err(), ParseError::Invalid);
        assert_eq!(parse("{\"a\":1,\"a\":2}").unwrap_err(), ParseError::Unsupported);
        assert_eq!(parse("\"\\ud800\"").unwrap_err(), ParseError::Unsupported);
        assert!(parse(" {\"a\": -1.5e3} ").is_ok());
        assert!(parse("\"\\ud83d\\ude00\"").is_ok());
    }

    #[test]
    fn numbers_compare_as_doubles_unless_too_long() {
        assert!(num_eq("1", "1.0"));
        assert!(num_eq("1e3", "1000"));
        assert!(!num_eq("12345678901234567890", "12345678901234567891"));
        assert!(!num_eq("1", "2"));
        assert!(!num_eq("1e400", "2e400"));
        assert!(!num_eq("0", "-0"));
        assert!(num_eq("-0", "-0.0"));
        assert!(num_eq("1e400", "1e400"));
    }
}
