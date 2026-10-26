//! Folder scope: which vault paths a profile syncs (spec "Folder scope").
//!
//! Prefixes are vault-relative folders, matched case-insensitively (the same
//! lower-casing as document IDs). The scope is applied after the built-in
//! skips (dot-paths, flag files, `syncIgnoreRegEx`).

use crate::path::js_to_lower_case;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeMode {
    /// Everything syncs.
    All,
    /// Only the listed folders sync.
    Include,
    /// Everything except the listed folders syncs.
    Exclude,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub mode: ScopeMode,
    /// Normalised, lower-cased folder prefixes without leading or trailing `/`.
    pub prefixes: Vec<String>,
}

impl Default for Scope {
    fn default() -> Self {
        Scope::all()
    }
}

/// What became visible when the scope changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Widening {
    /// Nothing new is visible.
    None,
    /// Newly visible paths all lie under these (lower-cased) prefixes.
    Prefixes(Vec<String>),
    /// Newly visible paths cannot be bounded by prefixes; scan everything.
    Everything,
}

fn normalise_prefix(p: &str) -> Result<String, String> {
    let segs: Vec<&str> = p.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return Err(format!("empty folder prefix {p:?}"));
    }
    for s in &segs {
        if *s == "." || *s == ".." {
            return Err(format!("folder prefix {p:?} contains {s:?}"));
        }
        if s.starts_with('.') {
            return Err(format!("folder prefix {p:?} is a dot-path; dot-paths never sync"));
        }
    }
    Ok(js_to_lower_case(&segs.join("/")))
}

fn under(key: &str, prefix: &str) -> bool {
    key == prefix || (key.len() > prefix.len() && key.starts_with(prefix) && key.as_bytes()[prefix.len()] == b'/')
}

impl Scope {
    pub fn all() -> Self {
        Scope {
            mode: ScopeMode::All,
            prefixes: vec![],
        }
    }

    /// Build a scope. `All` ignores the prefixes; `Include` and `Exclude`
    /// need at least one.
    pub fn new(mode: ScopeMode, prefixes: &[String]) -> Result<Self, String> {
        if mode == ScopeMode::All {
            return Ok(Scope::all());
        }
        let mut out: Vec<String> = prefixes.iter().map(|p| normalise_prefix(p)).collect::<Result<_, _>>()?;
        if out.is_empty() {
            return Err(format!("{mode:?} mode needs at least one folder").to_lowercase());
        }
        out.sort();
        out.dedup();
        // Drop prefixes covered by a shorter one.
        let mut kept: Vec<String> = Vec::new();
        for p in out {
            if !kept.iter().any(|k| under(&p, k)) {
                kept.push(p);
            }
        }
        Ok(Scope { mode, prefixes: kept })
    }

    fn covered(&self, key: &str) -> bool {
        self.prefixes.iter().any(|p| under(key, p))
    }

    /// Whether a vault path is inside the scope.
    pub fn contains(&self, path: &str) -> bool {
        match self.mode {
            ScopeMode::All => true,
            ScopeMode::Include => self.covered(&js_to_lower_case(path)),
            ScopeMode::Exclude => !self.covered(&js_to_lower_case(path)),
        }
    }

    /// Whether a folder may hold files inside the scope (so a scan must
    /// descend into it).
    pub fn may_contain_under(&self, dir: &str) -> bool {
        let key = js_to_lower_case(dir.trim_matches('/'));
        match self.mode {
            ScopeMode::All => true,
            ScopeMode::Exclude => !self.covered(&key),
            ScopeMode::Include => self.covered(&key) || self.prefixes.iter().any(|p| under(p, &key)),
        }
    }

    pub fn is_all(&self) -> bool {
        self.mode == ScopeMode::All
    }

    /// Where paths visible under `self` but not under `old` can lie.
    /// Callers still filter each path with `self.contains && !old.contains`.
    pub fn widening_from(&self, old: &Scope) -> Widening {
        use ScopeMode::*;
        let list = |v: Vec<String>| {
            if v.is_empty() {
                Widening::None
            } else {
                Widening::Prefixes(v)
            }
        };
        match (old.mode, self.mode) {
            (All, _) => Widening::None,
            (Include, Include) => list(self.prefixes.iter().filter(|q| !old.covered(q)).cloned().collect()),
            (Exclude, Exclude) => list(old.prefixes.iter().filter(|e| !self.covered(e)).cloned().collect()),
            (Exclude, All) => list(old.prefixes.clone()),
            (Exclude, Include) => {
                // Newly visible: included folders that were excluded, at
                // either end of the nesting.
                let mut v: Vec<String> = Vec::new();
                for q in &self.prefixes {
                    if old.covered(q) {
                        v.push(q.clone());
                    }
                    for e in &old.prefixes {
                        if under(e, q) {
                            v.push(e.clone());
                        }
                    }
                }
                v.sort();
                v.dedup();
                list(v)
            }
            (Include, All) | (Include, Exclude) => Widening::Everything,
        }
    }

    /// One-line description for logs and status.
    pub fn describe(&self) -> String {
        match self.mode {
            ScopeMode::All => "all folders".into(),
            ScopeMode::Include => format!("only {}", self.prefixes.join(", ")),
            ScopeMode::Exclude => format!("all except {}", self.prefixes.join(", ")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(mode: ScopeMode, p: &[&str]) -> Scope {
        Scope::new(mode, &p.iter().map(|x| x.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn matching_is_case_insensitive_and_folder_based() {
        let inc = s(ScopeMode::Include, &["/Notes/", "Projects/Alpha"]);
        assert_eq!(inc.prefixes, vec!["notes", "projects/alpha"]);
        assert!(inc.contains("notes/a.md"));
        assert!(inc.contains("NOTES/Sub/b.md"));
        assert!(inc.contains("Projects/ALPHA/x.png"));
        assert!(!inc.contains("Notesxyz/a.md"));
        assert!(!inc.contains("projects/beta/a.md"));
        assert!(!inc.contains("a.md"));
        let exc = s(ScopeMode::Exclude, &["Private"]);
        assert!(!exc.contains("private/x.md"));
        assert!(exc.contains("privateer.md"));
        assert!(Scope::all().contains("anything/at/all.md"));
        assert_eq!(s(ScopeMode::Include, &["a", "a/b", "A/c"]).prefixes, vec!["a"]);
        assert!(inc.may_contain_under("Projects"));
        assert!(inc.may_contain_under("projects/alpha/deep"));
        assert!(!inc.may_contain_under("projects/beta"));
        assert!(!exc.may_contain_under("Private/sub"));
        assert!(exc.may_contain_under("public"));
    }

    #[test]
    fn invalid_prefixes() {
        assert!(Scope::new(ScopeMode::Include, &[]).is_err());
        assert!(Scope::new(ScopeMode::Include, &["/".into()]).is_err());
        assert!(Scope::new(ScopeMode::Exclude, &["a/../b".into()]).is_err());
        assert!(Scope::new(ScopeMode::Include, &[".claude".into()]).is_err());
    }

    #[test]
    fn widening() {
        use Widening::*;
        let all = Scope::all();
        let inc_a = s(ScopeMode::Include, &["a"]);
        let inc_ab = s(ScopeMode::Include, &["a", "b"]);
        let exc_a = s(ScopeMode::Exclude, &["a"]);
        let exc_ab = s(ScopeMode::Exclude, &["a", "b"]);
        assert_eq!(inc_a.widening_from(&all), None);
        assert_eq!(inc_ab.widening_from(&inc_a), Prefixes(vec!["b".into()]));
        assert_eq!(inc_a.widening_from(&inc_ab), None);
        assert_eq!(exc_a.widening_from(&exc_ab), Prefixes(vec!["b".into()]));
        assert_eq!(all.widening_from(&exc_ab), Prefixes(vec!["a".into(), "b".into()]));
        assert_eq!(all.widening_from(&inc_a), Everything);
        assert_eq!(exc_a.widening_from(&inc_a), Everything);
        let inc_sub = s(ScopeMode::Include, &["a/x", "c"]);
        assert_eq!(inc_sub.widening_from(&exc_a), Prefixes(vec!["a/x".into()]));
        let inc_top = s(ScopeMode::Include, &["top"]);
        let exc_nested = s(ScopeMode::Exclude, &["top/secret"]);
        assert_eq!(inc_top.widening_from(&exc_nested), Prefixes(vec!["top/secret".into()]));
    }
}
