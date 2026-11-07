//! What syncs (spec D11): the dot-path allowlist, `syncIgnoreRegEx` and
//! `syncOnlyRegEx`, and ignore files (`useIgnoreFiles` / `ignoreFiles`).
//! The folder scope is separate (`livesync_model::scope`).
//!
//! Ignore files are judged with commonlib's own rules
//! (`livesync_model::ignore`), in both directions: an ignored path is not
//! pushed, a remote document at an ignored path is not written, and a path
//! that becomes ignored is untracked, never deleted remotely. Two choices go
//! beyond upstream:
//!
//! - **Fails closed.** An ignore file that cannot be read (or that uses a
//!   pattern the port does not evaluate) makes everything in its folder and
//!   below undecidable: nothing there is pushed, written, untracked or
//!   treated as deleted until it can be read. Upstream skips such a file.
//! - **Ignored folders are not looked into.** A folder that some ignore file
//!   surely ignores as a whole (`node_modules/`, or `*` in the folder's own
//!   ignore file) is neither walked nor watched, so ignore files further
//!   inside it are not read. The plugin would honour a `!` rule in one of
//!   them; here those paths stay ignored. A folder is only treated as a
//!   whole when no ignore file on the way could accept anything inside it.
//!
//! What the filters looked like when the index last synced is recorded
//! (`meta.filters`, the ignore files' contents included), so a path that
//! stops being ignored is caught up like a widened scope.

use crate::index::Index;
use livesync_model::ignore::{decide, ignore_file_key, intermediate_dirs, Decision, IgnoreList, Lookup};
use livesync_model::path::{classify_path_allowing, js_to_lower_case, DotAllow, PathSkip};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// One upstream `syncIgnoreRegEx` / `syncOnlyRegEx` entry.
#[derive(Debug, Clone)]
pub(crate) struct RegexRule {
    pub(crate) re: Regex,
    pub(crate) negate: bool,
}

/// Parse a regex list as upstream does (`parseCustomRegExpList` in
/// commonlib `utils.ts`): drop every space and newline from the whole list,
/// split on `|[]|`, drop empty entries; a `!!` prefix negates.
pub(crate) fn parse_regex_list(src: &str, case_sensitive: bool) -> Result<Vec<RegexRule>, regex::Error> {
    let cleaned: String = src.chars().filter(|c| *c != ' ' && *c != '\n').collect();
    cleaned
        .split("|[]|")
        .filter(|s| !s.is_empty())
        .map(|s| {
            let (negate, pat) = match s.strip_prefix("!!") {
                Some(p) => (true, p),
                None => (false, s),
            };
            let re = regex::RegexBuilder::new(pat)
                .case_insensitive(!case_sensitive)
                .build()?;
            Ok(RegexRule { re, negate })
        })
        .collect()
}

fn rule_hit(r: &RegexRule, path: &str) -> bool {
    r.re.is_match(path) != r.negate
}

/// Why the ignore files keep a path out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IgnoredBy {
    /// The ignore file, vault-relative (`.gitignore` for the root one).
    pub file: String,
    /// 1-based line of the deciding pattern, when one decided.
    pub line: Option<usize>,
    pub pattern: Option<String>,
    /// Set when a whole folder is ignored (and so not looked into).
    pub folder: Option<String>,
}

impl IgnoredBy {
    pub fn describe(&self) -> String {
        let at = match (self.line, &self.pattern) {
            (Some(l), Some(p)) => format!("{}:{l}:{p}", self.file),
            _ => format!("{} (the path contains ./ or ../, which ignore files refuse)", self.file),
        };
        match &self.folder {
            Some(f) => format!("{at} (folder {f} is ignored as a whole)"),
            None => at,
        }
    }
}

/// How the ignore files judge a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IgnoreState {
    Synced {
        /// The `!` rule that accepted it, if one did.
        accepted_by: Option<IgnoredBy>,
    },
    Ignored(IgnoredBy),
    /// An ignore file at or above it cannot be read: undecidable.
    Unknown {
        file: String,
        why: String,
    },
}

/// An ignore file as recorded in `meta.filters`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Saved {
    Lines(Vec<String>),
    Error(String),
}

enum Source {
    /// Read from the vault folder.
    Disk(PathBuf),
    /// Recorded contents, keyed by the lower-cased file path.
    Snapshot(BTreeMap<String, Saved>),
}

#[derive(Debug, Clone)]
enum Folder {
    Open,
    Ignored(IgnoredBy),
}

/// The ignore files of one vault, read lazily and cached until `refresh`.
pub(crate) struct IgnoreFiles {
    names: Vec<String>,
    source: Source,
    files: Mutex<HashMap<String, Lookup<Arc<IgnoreList>>>>,
    folders: Mutex<HashMap<String, Folder>>,
    chain_errors: Mutex<HashMap<String, Option<(String, String)>>>,
    /// Raw lines of what was read (for the snapshot).
    raw: Mutex<BTreeMap<String, Saved>>,
}

fn shown(key: &str) -> String {
    key.strip_prefix('/').unwrap_or(key).to_string()
}

impl IgnoreFiles {
    pub(crate) fn on_disk(root: &Path, names: &[String]) -> Self {
        Self::with(Source::Disk(root.to_path_buf()), names)
    }

    fn snapshot(files: BTreeMap<String, Saved>, names: &[String]) -> Self {
        Self::with(Source::Snapshot(files), names)
    }

    fn with(source: Source, names: &[String]) -> Self {
        IgnoreFiles {
            names: names.to_vec(),
            source,
            files: Mutex::default(),
            folders: Mutex::default(),
            chain_errors: Mutex::default(),
            raw: Mutex::default(),
        }
    }

    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }

    /// Forget what was read: ignore files are read again when next needed.
    pub(crate) fn refresh(&self) {
        if matches!(self.source, Source::Disk(_)) {
            self.files.lock().unwrap().clear();
            self.folders.lock().unwrap().clear();
            self.chain_errors.lock().unwrap().clear();
            self.raw.lock().unwrap().clear();
        }
    }

    /// What was read since the last refresh, as recorded in `meta.filters`.
    pub(crate) fn read_so_far(&self) -> BTreeMap<String, Saved> {
        let raw = self.raw.lock().unwrap();
        let mut out = BTreeMap::new();
        for (k, v) in raw.iter() {
            // Two spellings of one folder on a case-sensitive filesystem
            // land on one key: keep an undecidable entry then.
            let key = js_to_lower_case(k);
            match out.get(&key) {
                Some(prev) if prev != v => {
                    out.insert(key, Saved::Error("two ignore files differ only in case".into()));
                }
                _ => {
                    out.insert(key, v.clone());
                }
            }
        }
        out
    }

    fn load(&self, dir: &str, name: &str) -> Lookup<Arc<IgnoreList>> {
        let key = ignore_file_key(dir, name);
        if let Some(l) = self.files.lock().unwrap().get(&key) {
            return l.clone();
        }
        let (lookup, saved) = match &self.source {
            Source::Snapshot(m) => match m.get(&js_to_lower_case(&key)) {
                None => (Lookup::Missing, None),
                Some(Saved::Error(e)) => (Lookup::Error(e.clone()), None),
                Some(Saved::Lines(lines)) => match IgnoreList::from_lines(lines) {
                    Ok(l) => (Lookup::Found(Arc::new(l)), None),
                    Err(e) => (Lookup::Error(e.to_string()), None),
                },
            },
            Source::Disk(root) => {
                let p = if dir.is_empty() {
                    root.join(name)
                } else {
                    root.join(dir).join(name)
                };
                match std::fs::read(&p) {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        let lines: Vec<String> = text
                            .split('\n')
                            .map(|l| l.strip_suffix('\r').unwrap_or(l))
                            .map(|l| livesync_model::ignore::js_trim(l).to_string())
                            .collect();
                        match IgnoreList::from_lines(&lines) {
                            Ok(l) => (Lookup::Found(Arc::new(l)), Some(Saved::Lines(lines))),
                            Err(e) => {
                                let why = format!("uses a pattern lsync does not evaluate ({e})");
                                (Lookup::Error(why.clone()), Some(Saved::Error(why)))
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ENOTDIR) => {
                        (Lookup::Missing, None)
                    }
                    Err(e) => {
                        let why = format!("cannot be read ({e})");
                        (Lookup::Error(why.clone()), Some(Saved::Error(why)))
                    }
                }
            }
        };
        if let Some(s) = saved {
            self.raw.lock().unwrap().insert(key.clone(), s);
        }
        self.files.lock().unwrap().insert(key, lookup.clone());
        lookup
    }

    /// The folders whose ignore files apply inside `dir`, nearest first.
    fn chain(dir: &str) -> Vec<String> {
        if dir.is_empty() {
            vec![String::new()]
        } else {
            intermediate_dirs(&format!("{dir}/x"))
        }
    }

    /// An ignore file at or above `dir` that cannot be read.
    pub(crate) fn chain_error(&self, dir: &str) -> Option<(String, String)> {
        if let Some(c) = self.chain_errors.lock().unwrap().get(dir) {
            return c.clone();
        }
        let mut found = None;
        'out: for d in Self::chain(dir) {
            for n in &self.names {
                if let Lookup::Error(why) = self.load(&d, n) {
                    found = Some((shown(&ignore_file_key(&d, n)), why));
                    break 'out;
                }
            }
        }
        self.chain_errors.lock().unwrap().insert(dir.to_string(), found.clone());
        found
    }

    /// Whether some ignore file surely ignores everything inside `dir`.
    fn folder(&self, dir: &str) -> Folder {
        if let Some(f) = self.folders.lock().unwrap().get(dir) {
            return f.clone();
        }
        let mut out = Folder::Open;
        'out: for d in Self::chain(dir) {
            let rel = if d == dir {
                ""
            } else if d.is_empty() {
                dir
            } else {
                &dir[d.len() + 1..]
            };
            for n in &self.names {
                let Lookup::Found(list) = self.load(&d, n) else {
                    continue;
                };
                if let Some(i) = list.ignores_all_under(rel) {
                    let r = &list.rules[i];
                    out = Folder::Ignored(IgnoredBy {
                        file: shown(&ignore_file_key(&d, n)),
                        line: Some(r.line),
                        pattern: Some(r.pattern.clone()),
                        folder: Some(dir.to_string()),
                    });
                    break 'out;
                }
                if list.may_accept_under(rel) {
                    break 'out;
                }
            }
        }
        self.folders.lock().unwrap().insert(dir.to_string(), out.clone());
        out
    }

    /// How the ignore files judge the folder `dir` itself: whether it may be
    /// walked (`Synced`), is ignored as a whole, or is undecidable.
    pub(crate) fn folder_state(&self, dir: &str) -> IgnoreState {
        if let Some((file, why)) = self.chain_error(dir) {
            return IgnoreState::Unknown { file, why };
        }
        match self.folder(dir) {
            Folder::Ignored(by) => IgnoreState::Ignored(by),
            Folder::Open => IgnoreState::Synced { accepted_by: None },
        }
    }

    /// How the ignore files judge the file at `path`.
    pub(crate) fn state(&self, path: &str) -> IgnoreState {
        let dir = match path.rfind('/') {
            Some(i) => &path[..i],
            None => "",
        };
        if let Some((file, why)) = self.chain_error(dir) {
            return IgnoreState::Unknown { file, why };
        }
        // Outermost first, as a walk meets them.
        let mut at = 0usize;
        while let Some(i) = path[at..].find('/') {
            let folder = &path[..at + i];
            if let Folder::Ignored(by) = self.folder(folder) {
                return IgnoreState::Ignored(by);
            }
            at += i + 1;
        }
        match decide(path, &self.names, |d, n| self.load(d, n)) {
            Decision::Accepted => IgnoreState::Synced { accepted_by: None },
            Decision::Unknown { file, why } => IgnoreState::Unknown {
                file: shown(&file),
                why,
            },
            Decision::By { file, verdict } => {
                let rule = verdict.rule.map(|i| {
                    // The list is cached; look it up again for the rule text.
                    let (d, n) = file.rsplit_once('/').unwrap_or(("", &file));
                    match self.load(d, n) {
                        Lookup::Found(l) => (l.rules[i].line, l.rules[i].pattern.clone()),
                        _ => (0, String::new()),
                    }
                });
                let by = IgnoredBy {
                    file: shown(&file),
                    line: rule.as_ref().map(|r| r.0),
                    pattern: rule.map(|r| r.1),
                    folder: None,
                };
                if verdict.accepted {
                    IgnoreState::Synced { accepted_by: Some(by) }
                } else {
                    IgnoreState::Ignored(by)
                }
            }
        }
    }
}

/// The local filters of one vault (all but the folder scope).
pub(crate) struct Filters {
    pub(crate) dot: DotAllow,
    pub(crate) ignore_re: Vec<RegexRule>,
    pub(crate) only_re: Vec<RegexRule>,
    /// `None` when ignore files are off.
    pub(crate) files: Option<IgnoreFiles>,
}

/// Why a path does not sync, or that it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Synced { accepted_by: Option<IgnoredBy> },
    Skip(PathSkip),
    IgnoreRegex,
    NotInSyncOnly,
    OutOfScope,
    Ignored(IgnoredBy),
    Unknown { file: String, why: String },
}

/// Three-way visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Vis {
    Visible,
    Hidden,
    Unknown,
}

impl Filters {
    pub(crate) fn path_skip(&self, path: &str) -> Option<PathSkip> {
        classify_path_allowing(path, &self.dot)
    }

    /// `Some(true)`: `syncOnlyRegEx` leaves it out; `Some(false)`:
    /// `syncIgnoreRegEx` ignores it.
    pub(crate) fn regex_skip(&self, path: &str) -> Option<bool> {
        self.regex_verdict(path).map(|v| v == Verdict::NotInSyncOnly)
    }

    fn regex_verdict(&self, path: &str) -> Option<Verdict> {
        // Upstream checks syncOnlyRegEx first, then syncIgnoreRegEx.
        if !self.only_re.is_empty() && !self.only_re.iter().any(|r| rule_hit(r, path)) {
            return Some(Verdict::NotInSyncOnly);
        }
        if self.ignore_re.iter().any(|r| rule_hit(r, path)) {
            return Some(Verdict::IgnoreRegex);
        }
        None
    }

    pub(crate) fn ignore_state(&self, path: &str) -> IgnoreState {
        match &self.files {
            None => IgnoreState::Synced { accepted_by: None },
            Some(f) => f.state(path),
        }
    }

    pub(crate) fn folder_state(&self, dir: &str) -> IgnoreState {
        match &self.files {
            None => IgnoreState::Synced { accepted_by: None },
            Some(f) => f.folder_state(dir),
        }
    }

    pub(crate) fn refresh(&self) {
        if let Some(f) = &self.files {
            f.refresh();
        }
    }

    /// The whole verdict for a file path, scope included (for
    /// `check-ignore` and the filter catch-up).
    pub fn verdict(&self, path: &str, scope: &livesync_model::scope::Scope) -> Verdict {
        if let Some(s) = self.path_skip(path) {
            return Verdict::Skip(s);
        }
        if let Some(v) = self.regex_verdict(path) {
            return v;
        }
        if !scope.contains(path) {
            return Verdict::OutOfScope;
        }
        match self.ignore_state(path) {
            IgnoreState::Synced { accepted_by } => Verdict::Synced { accepted_by },
            IgnoreState::Ignored(by) => Verdict::Ignored(by),
            IgnoreState::Unknown { file, why } => Verdict::Unknown { file, why },
        }
    }

    pub(crate) fn vis(&self, path: &str, scope: &livesync_model::scope::Scope) -> Vis {
        match self.verdict(path, scope) {
            Verdict::Synced { .. } => Vis::Visible,
            Verdict::Unknown { .. } => Vis::Unknown,
            _ => Vis::Hidden,
        }
    }
}

/// What `lsync check-ignore` says about one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub path: String,
    pub synced: bool,
    /// `synced`, `dot-path`, `flag-file`, `internal`, `unsafe-path`,
    /// `ignore-regex`, `sync-only-regex`, `out-of-scope`, `ignore-file` or
    /// `undecidable` (an ignore file on the way cannot be read).
    pub reason: String,
    /// The ignore file and rule that decided, when one did (for `synced`,
    /// the `!` rule that accepted it).
    pub source: Option<IgnoredBy>,
    /// One line for people.
    pub detail: String,
}

impl Filters {
    pub fn check(&self, path: &str, scope: &livesync_model::scope::Scope) -> Check {
        let path = path.trim_matches('/');
        let (synced, reason, source, detail) = match self.verdict(path, scope) {
            Verdict::Synced { accepted_by } => {
                let d = match &accepted_by {
                    Some(by) => format!("synced: accepted by {}", by.describe()),
                    None => "synced".to_string(),
                };
                (true, "synced".to_string(), accepted_by, d)
            }
            Verdict::Skip(s) => (
                false,
                s.as_str().to_string(),
                None,
                match s {
                    PathSkip::DotPath => "not synced: a dot-path (see sync_dot_paths)".to_string(),
                    PathSkip::FlagFile => "not synced: a LiveSync flag or log file".to_string(),
                    PathSkip::Internal => "not synced: an internal document name".to_string(),
                    PathSkip::Unsafe => "not synced: not a safe vault path".to_string(),
                },
            ),
            Verdict::IgnoreRegex => (
                false,
                "ignore-regex".into(),
                None,
                "not synced: matched by ignore_regex (syncIgnoreRegEx)".into(),
            ),
            Verdict::NotInSyncOnly => (
                false,
                "sync-only-regex".into(),
                None,
                "not synced: not matched by sync_only_regex (syncOnlyRegEx)".into(),
            ),
            Verdict::OutOfScope => (
                false,
                "out-of-scope".into(),
                None,
                "not synced: outside the folder scope (include/exclude)".into(),
            ),
            Verdict::Ignored(by) => {
                let d = format!("ignored by {}", by.describe());
                (false, "ignore-file".into(), Some(by), d)
            }
            Verdict::Unknown { file, why } => (
                false,
                "undecidable".into(),
                Some(IgnoredBy {
                    file: file.clone(),
                    line: None,
                    pattern: None,
                    folder: None,
                }),
                format!("not synced for now: the ignore file {file} {why}"),
            ),
        };
        Check {
            path: path.to_string(),
            synced,
            reason,
            source,
            detail,
        }
    }
}

/// `lsync check-ignore` without an engine: judge `paths` (vault-relative)
/// with a config's filters against the vault folder as it is now.
pub fn check_paths(cfg: &crate::EngineConfig, paths: &[String], case_sensitive: bool) -> Result<Vec<Check>, String> {
    let f = cfg.filter_settings().build(&cfg.vault, case_sensitive)?;
    Ok(paths.iter().map(|p| f.check(p, &cfg.scope)).collect())
}

/// The settings behind [`Filters`], as `EngineConfig` carries them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FilterSettings {
    pub use_ignore_files: bool,
    pub ignore_files: Vec<String>,
    pub ignore_regex: Option<String>,
    pub sync_only_regex: Option<String>,
    pub sync_dot_paths: Vec<String>,
}

/// Check the names in `ignore_files`.
pub fn check_ignore_file_names(names: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for (i, n) in names.iter().enumerate() {
        let n = n.trim();
        if n.is_empty() {
            continue;
        }
        if n.contains('/') || n.contains('\\') || n == "." || n == ".." {
            return Err(format!("ignore_files: entry {} must be a file name, not a path", i + 1));
        }
        if !out.iter().any(|o: &String| o == n) {
            out.push(n.to_string());
        }
    }
    Ok(out)
}

impl FilterSettings {
    /// Build the filters (reading ignore files from `vault` when on).
    pub(crate) fn build(&self, vault: &Path, case_sensitive: bool) -> Result<Filters, String> {
        let dot = DotAllow::new(&self.sync_dot_paths)?;
        let ignore_re = match &self.ignore_regex {
            Some(s) => parse_regex_list(s, case_sensitive).map_err(|e| format!("invalid ignore regex: {e}"))?,
            None => vec![],
        };
        let only_re = match &self.sync_only_regex {
            Some(s) => parse_regex_list(s, case_sensitive).map_err(|e| format!("invalid sync-only regex: {e}"))?,
            None => vec![],
        };
        let files = if self.use_ignore_files {
            Some(IgnoreFiles::on_disk(
                vault,
                &check_ignore_file_names(&self.ignore_files)?,
            ))
        } else {
            None
        };
        Ok(Filters {
            dot,
            ignore_re,
            only_re,
            files,
        })
    }
}

/// What the filters were when the index last synced (`meta.filters`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct FilterState {
    pub(crate) settings: FilterSettings,
    /// Ignore files read by the last scan (lower-cased path -> contents).
    pub(crate) files: BTreeMap<String, Saved>,
}

impl FilterState {
    /// Normalised, so settings that mean the same compare equal.
    pub(crate) fn new(settings: FilterSettings, files: BTreeMap<String, Saved>) -> FilterState {
        let opt = |s: Option<String>| s.filter(|x| !x.trim().is_empty());
        let mut settings = FilterSettings {
            ignore_regex: opt(settings.ignore_regex),
            sync_only_regex: opt(settings.sync_only_regex),
            ..settings
        };
        if !settings.use_ignore_files {
            settings.ignore_files.clear();
        }
        settings.sync_dot_paths = DotAllow::new(&settings.sync_dot_paths)
            .map(|d| d.entries().to_vec())
            .unwrap_or(settings.sync_dot_paths);
        let files = if settings.use_ignore_files {
            files
        } else {
            BTreeMap::new()
        };
        FilterState { settings, files }
    }

    /// The filters before ignore files and `syncOnlyRegEx` existed here:
    /// `syncIgnoreRegEx` as now, nothing else.
    pub(crate) fn before_filters(now: &FilterSettings) -> FilterState {
        FilterState::new(
            FilterSettings {
                ignore_regex: now.ignore_regex.clone(),
                ..Default::default()
            },
            BTreeMap::new(),
        )
    }

    /// An evaluator over the recorded contents (paths may be lower-cased
    /// document IDs; the ignore rules ignore case anyway).
    pub(crate) fn evaluator(&self, case_sensitive: bool) -> Result<Filters, String> {
        let s = &self.settings;
        let dot = DotAllow::new(&s.sync_dot_paths)?;
        let ignore_re = match &s.ignore_regex {
            Some(x) => parse_regex_list(x, case_sensitive).map_err(|e| e.to_string())?,
            None => vec![],
        };
        let only_re = match &s.sync_only_regex {
            Some(x) => parse_regex_list(x, case_sensitive).map_err(|e| e.to_string())?,
            None => vec![],
        };
        let files = s.use_ignore_files.then(|| {
            IgnoreFiles::snapshot(
                self.files.clone(),
                &check_ignore_file_names(&s.ignore_files).unwrap_or_default(),
            )
        });
        Ok(Filters {
            dot,
            ignore_re,
            only_re,
            files,
        })
    }

    pub(crate) fn load(ix: &Index) -> rusqlite::Result<Option<FilterState>> {
        Ok(ix
            .get_meta(crate::engine::meta_keys::FILTERS)?
            .and_then(|s| serde_json::from_str(&s).ok()))
    }

    pub(crate) fn save(&self, ix: &Index) -> rusqlite::Result<()> {
        ix.set_meta(
            crate::engine::meta_keys::FILTERS,
            &serde_json::to_string(self).unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use livesync_model::scope::Scope;

    fn vault(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (p, c) in files {
            let p = d.path().join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, c).unwrap();
        }
        d
    }

    fn filters(d: &Path) -> Filters {
        FilterSettings {
            use_ignore_files: true,
            ignore_files: vec![".gitignore".into()],
            ..Default::default()
        }
        .build(d, false)
        .unwrap()
    }

    #[test]
    fn nested_files_and_whole_folders() {
        let d = vault(&[
            (".gitignore", "*.log\nnode_modules/\n"),
            ("notes/.gitignore", "drafts/\n"),
            ("notes/drafts/.gitignore", "!*.md\n"),
            ("venv/.gitignore", "*\n"),
        ]);
        let f = filters(d.path());
        let all = Scope::all();
        assert_eq!(f.vis("a.md", &all), Vis::Visible);
        assert_eq!(f.vis("a.log", &all), Vis::Hidden);
        assert_eq!(f.vis("sub/a.log", &all), Vis::Visible, "not git's any-depth match");
        assert!(matches!(f.folder_state("node_modules"), IgnoreState::Ignored(_)));
        assert!(matches!(f.folder_state("sub/node_modules"), IgnoreState::Synced { .. }));
        assert!(matches!(f.folder_state("venv"), IgnoreState::Ignored(_)));
        // A nested ignore file re-accepts: the folder must be walked.
        assert!(matches!(f.folder_state("notes/drafts"), IgnoreState::Synced { .. }));
        assert_eq!(f.vis("notes/drafts/d.md", &all), Vis::Visible);
        assert_eq!(f.vis("notes/drafts/d.txt", &all), Vis::Hidden);
        match f.verdict("a.log", &all) {
            Verdict::Ignored(by) => assert_eq!(by.describe(), ".gitignore:1:*.log"),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn unreadable_ignore_files_fail_closed() {
        let d = vault(&[("notes/.gitignore", "+(a|b)\n"), ("ok/x.md", "")]);
        let f = filters(d.path());
        let all = Scope::all();
        assert_eq!(f.vis("notes/a.md", &all), Vis::Unknown);
        assert_eq!(f.vis("notes/sub/a.md", &all), Vis::Unknown);
        assert_eq!(f.vis("ok/x.md", &all), Vis::Visible);
        assert!(matches!(f.folder_state("notes"), IgnoreState::Unknown { .. }));
    }

    #[test]
    fn snapshots_judge_like_the_disk() {
        let d = vault(&[(".gitignore", "Drafts/\n"), ("Notes/.gitignore", "*.tmp\n")]);
        let f = filters(d.path());
        let all = Scope::all();
        assert_eq!(f.vis("Notes/x.tmp", &all), Vis::Hidden);
        assert_eq!(f.vis("Drafts/x.md", &all), Vis::Hidden);
        let state = FilterState {
            settings: FilterSettings {
                use_ignore_files: true,
                ignore_files: vec![".gitignore".into()],
                ..Default::default()
            },
            files: f.files.as_ref().unwrap().read_so_far(),
        };
        let s = state.evaluator(false).unwrap();
        // Lower-cased document IDs judge the same.
        assert_eq!(s.vis("notes/x.tmp", &all), Vis::Hidden);
        assert_eq!(s.vis("drafts/x.md", &all), Vis::Hidden);
        assert_eq!(s.vis("notes/x.md", &all), Vis::Visible);
    }
}
