//! The upstream pin (`upstream.env`) is the single source of truth for the
//! upstream release: everything that names it must agree with it, so a bump
//! cannot forget a place (AGENTS.md, "Bumping upstream"). Each failure names
//! the file to fix. No network: it reads the repository only.
//!
//! Pins and history are told apart like this:
//! - In Markdown, a version that states the pin is written after a marker,
//!   `<!--pin:KEY-->VALUE` (hidden when rendered), and must equal the pin.
//!   `scripts/bump-upstream.sh` rewrites them. A version right after a pin
//!   phrase ("pinned to", "tested against", ...) without a marker fails, so a
//!   new pin statement cannot go unmarked. Everything else ("new in 1.0.46",
//!   "as 1.0.49 does", "unchanged since 0.2.1") is history and not checked.
//! - Code cites upstream by version in comments as provenance; those are not
//!   checked. Code that needs the pinned version takes it from
//!   `livesync_model::upstream`, which `build.rs` reads from the pin.
//! - Scripts and the workflow carry no version at all: they ask
//!   `scripts/upstream.sh`, which reads the pin.
use livesync_model::upstream;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const KEYS: [&str; 6] = [
    "PLUGIN_RELEASE",
    "COMMONLIB",
    "OCTAGONAL_WHEELS",
    "MINIMATCH",
    "NONRECURSIVE_GLOB_MATCH",
    "BRACE_EXPANSION",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    let p = root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn json(rel: &str) -> Value {
    serde_json::from_str(&read(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// The pin, read from the file (not from the compile-time copy).
fn pin() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for (n, line) in read("upstream.env").lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("upstream.env:{}: not KEY=value: {line}", n + 1));
        assert!(KEYS.contains(&k), "upstream.env:{}: unknown key {k}", n + 1);
        assert!(
            !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)),
            "upstream.env:{}: odd value {v:?}",
            n + 1
        );
        assert!(
            m.insert(k.to_string(), v.to_string()).is_none(),
            "upstream.env: {k} twice"
        );
    }
    for k in KEYS {
        assert!(m.contains_key(k), "upstream.env: {k} missing");
    }
    m
}

/// Collects every disagreement, so one run lists them all.
#[derive(Default)]
struct Problems(Vec<String>);

impl Problems {
    fn check(&mut self, ok: bool, msg: impl FnOnce() -> String) {
        if !ok {
            self.0.push(msg());
        }
    }
    fn want(&mut self, file: &str, what: &str, got: &Value, want: &str) {
        self.check(got.as_str() == Some(want), || {
            format!("{file}: {what} is {got}, the pin (upstream.env) says {want}")
        });
    }
    fn finish(self) {
        assert!(
            self.0.is_empty(),
            "out of step with the upstream pin (upstream.env; scripts/bump-upstream.sh moves everything together):\n  {}",
            self.0.join("\n  ")
        );
    }
}

#[test]
fn compiled_constants_are_the_pin() {
    let p = pin();
    let compiled = [
        ("PLUGIN_RELEASE", upstream::PLUGIN_RELEASE),
        ("COMMONLIB", upstream::COMMONLIB),
        ("OCTAGONAL_WHEELS", upstream::OCTAGONAL_WHEELS),
        ("MINIMATCH", upstream::MINIMATCH),
        ("NONRECURSIVE_GLOB_MATCH", upstream::NONRECURSIVE_GLOB_MATCH),
        ("BRACE_EXPANSION", upstream::BRACE_EXPANSION),
    ];
    for (k, v) in compiled {
        assert_eq!(
            p[k], v,
            "livesync_model::upstream::{k} (build.rs) differs from upstream.env"
        );
    }
    assert_eq!(livesync_model::gate::TESTED_PLUGIN_VERSION, p["PLUGIN_RELEASE"]);
}

/// Where node would find `name` for a package installed at `from` (a
/// lockfile key such as `node_modules/a/node_modules/b`).
fn resolve<'a>(packages: &'a Value, from: &str, name: &str) -> Option<(String, &'a Value)> {
    let mut base = from.to_string();
    loop {
        let key = if base.is_empty() {
            format!("node_modules/{name}")
        } else {
            format!("{base}/node_modules/{name}")
        };
        if let Some(v) = packages.get(&key) {
            return Some((key, v));
        }
        if base.is_empty() {
            return None;
        }
        base = match base.rfind("node_modules/") {
            Some(0) => String::new(),
            Some(i) => base[..i].trim_end_matches('/').to_string(),
            None => String::new(),
        };
    }
}

#[test]
fn golden_tool_follows_the_pin() {
    let p = pin();
    let mut out = Problems::default();
    let file = "tools/golden/package.json";
    let pkg = json(file);
    out.want(
        file,
        "dependencies[\"@vrtmrz/livesync-commonlib\"]",
        &pkg["dependencies"]["@vrtmrz/livesync-commonlib"],
        &p["COMMONLIB"],
    );
    let overrides = [
        ("minimatch", "MINIMATCH"),
        ("@isaacs/nonrecursive-glob-match", "NONRECURSIVE_GLOB_MATCH"),
        ("brace-expansion", "BRACE_EXPANSION"),
    ];
    for (name, key) in overrides {
        out.want(
            file,
            &format!("overrides[\"{name}\"]"),
            &pkg["overrides"][name],
            &p[key],
        );
    }
    out.check(pkg["overrides"].as_object().map(|o| o.len()) == Some(3), || {
        format!("{file}: overrides should be exactly the three matcher packages of the pin")
    });

    let file = "tools/golden/package-lock.json";
    let lock = json(file);
    let pk = &lock["packages"];
    out.want(
        file,
        "packages[\"\"].dependencies[\"@vrtmrz/livesync-commonlib\"]",
        &pk[""]["dependencies"]["@vrtmrz/livesync-commonlib"],
        &p["COMMONLIB"],
    );
    let lib = "node_modules/@vrtmrz/livesync-commonlib";
    out.want(file, lib, &pk[lib]["version"], &p["COMMONLIB"]);
    let mut check_dep = |from: &str, name: &str, key: &str| -> Option<String> {
        match resolve(pk, from, name) {
            Some((at, v)) => {
                out.want(file, &at, &v["version"], &p[key]);
                Some(at)
            }
            None => {
                out.check(false, || format!("{file}: {name} (from {from}) not installed"));
                None
            }
        }
    };
    check_dep(lib, "octagonal-wheels", "OCTAGONAL_WHEELS");
    if let Some(mm) = check_dep(lib, "minimatch", "MINIMATCH") {
        check_dep(&mm, "@isaacs/nonrecursive-glob-match", "NONRECURSIVE_GLOB_MATCH");
        check_dep(&mm, "brace-expansion", "BRACE_EXPANSION");
    }

    let gen = read("tools/golden/generate.mjs");
    out.check(gen.contains("upstream.env"), || {
        "tools/golden/generate.mjs: should check the installed versions against upstream.env".into()
    });
    out.finish();
}

#[test]
fn golden_fixtures_come_from_the_pin() {
    let p = pin();
    let mut out = Problems::default();
    let dir = root().join("fixtures/golden");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".json"))
        .collect();
    names.sort();
    assert!(names.len() >= 10, "fixtures/golden: only {} fixtures", names.len());
    for n in names {
        let file = format!("fixtures/golden/{n}");
        let v = json(&file);
        out.want(&file, "meta.version", &v["meta"]["version"], &p["COMMONLIB"]);
        if let Some(u) = v.get("upstream") {
            out.want(&file, "upstream.commonlib", &u["commonlib"], &p["COMMONLIB"]);
            out.want(
                &file,
                "upstream.octagonalWheels",
                &u["octagonalWheels"],
                &p["OCTAGONAL_WHEELS"],
            );
        }
    }
    out.finish();
}

/// A version-looking token at the start of `s`: digits and dots, at least
/// one dot (`1.0.49`, `0.4.3`).
fn version_at(s: &str) -> Option<&str> {
    let end = s
        .char_indices()
        .find(|&(_, c)| !(c.is_ascii_digit() || c == '.'))
        .map_or(s.len(), |(i, _)| i);
    let v = s[..end].trim_end_matches('.');
    (v.contains('.') && v.starts_with(|c: char| c.is_ascii_digit()) && v.split('.').all(|p| !p.is_empty())).then_some(v)
}

const MARK: &str = "<!--pin:";

/// Phrases that make the version after them a statement of the pin.
const PIN_PHRASES: [&str; 8] = [
    "pinned to",
    "pinned at",
    "pins ",
    "tested against",
    "built against",
    "runs against",
    "run against",
    "known to work",
];

const DOCS: [&str; 3] = ["README.md", "AGENTS.md", "COMPATIBILITY.md"];

#[test]
fn docs_state_the_pin_only_through_markers() {
    let p = pin();
    let mut out = Problems::default();
    for file in DOCS {
        let text = read(file);
        for (n, line) in text.lines().enumerate() {
            let at = format!("{file}:{}", n + 1);
            // Every marker holds the pinned value.
            let mut rest = line;
            while let Some(i) = rest.find(MARK) {
                // In a code span (`<!--pin:KEY-->VALUE`) it is the syntax, not a marker.
                let quoted = rest[..i].ends_with('`');
                rest = &rest[i + MARK.len()..];
                if quoted {
                    continue;
                }
                let Some((key, after)) = rest.split_once("-->") else {
                    out.check(false, || format!("{at}: unterminated pin marker"));
                    break;
                };
                match p.get(key) {
                    None => out.check(false, || format!("{at}: unknown pin key {key}")),
                    Some(want) => {
                        let got = version_at(after);
                        out.check(got == Some(want.as_str()), || {
                            format!(
                                "{at}: {key} is stated as {}, the pin (upstream.env) says {want}",
                                got.unwrap_or("nothing")
                            )
                        });
                    }
                }
                rest = after;
            }
            // A version right after a pin phrase must be marked.
            let lower = line.to_ascii_lowercase();
            for phrase in PIN_PHRASES {
                let mut from = 0;
                while let Some(i) = lower[from..].find(phrase) {
                    let start = from + i + phrase.len();
                    from = start;
                    // The phrase's clause: up to 40 bytes, stopping at a comma,
                    // semicolon, colon or full stop that ends it.
                    let window: String = line[start..].chars().take(40).collect();
                    let clause = window.split([',', ';', ':', '(', ')']).next().unwrap_or("");
                    let clause = clause.split(". ").next().unwrap_or("");
                    for (j, _) in clause.char_indices() {
                        let prev = clause[..j].chars().last();
                        if prev.is_some_and(|c| c.is_ascii_alphanumeric() || c == '.') {
                            continue;
                        }
                        if let Some(v) = version_at(&clause[j..]) {
                            if !clause[..j].ends_with("-->") {
                                out.check(false, || {
                                    format!(
                                        "{at}: \"{phrase}\" {v} states a pin without a marker; write <!--pin:KEY-->{v} (or point at upstream.env)"
                                    )
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    out.finish();
}

/// The tested-versions table in COMPATIBILITY.md names the pin in its
/// plugin, default-CLI and commonlib rows, through markers.
#[test]
fn compatibility_table_names_the_pin() {
    let p = pin();
    let file = "COMPATIBILITY.md";
    let text = read(file);
    let table: Vec<&str> = text
        .split("## Tested versions")
        .nth(1)
        .unwrap_or_else(|| panic!("{file}: no \"## Tested versions\" section"))
        .lines()
        .skip_while(|l| !l.starts_with('|'))
        .take_while(|l| l.starts_with('|'))
        .collect();
    let mut out = Problems::default();
    let marked = |row: &str, key: &str| row.contains(&format!("{MARK}{key}-->{}", p[key]));
    let rows = |prefix: &str| {
        table
            .iter()
            .filter(move |r| r.starts_with(prefix))
            .copied()
            .collect::<Vec<_>>()
    };
    let plugin = rows("| Self-hosted LiveSync plugin |");
    out.check(
        plugin.len() == 1 && marked(plugin[0], "PLUGIN_RELEASE") && marked(plugin[0], "COMMONLIB"),
        || format!("{file}: the plugin row must name the pinned release and its commonlib with pin markers"),
    );
    let default_cli: Vec<_> = rows("| Official LiveSync CLI |")
        .into_iter()
        .filter(|r| r.contains("(its default)"))
        .collect();
    out.check(
        default_cli.len() == 1 && marked(default_cli[0], "PLUGIN_RELEASE"),
        || {
            format!(
                "{file}: one official CLI row, the scripts' default, must name the pinned release with a pin marker"
            )
        },
    );
    let commonlib = rows("| `livesync-commonlib` |");
    out.check(commonlib.len() == 1 && marked(commonlib[0], "COMMONLIB"), || {
        format!("{file}: the livesync-commonlib row must name the pinned version with a pin marker")
    });
    out.finish();
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Scripts and the workflow take the release from `scripts/upstream.sh`
/// and carry no version or unversioned reference checkout of their own.
#[test]
fn scripts_and_workflow_resolve_the_pin() {
    let mut out = Problems::default();
    let mut files = Vec::new();
    files_under(&root().join("scripts"), &mut files);
    files.push(root().join(".github/workflows/upstream-interop.yml"));
    files.sort();
    for path in &files {
        let rel = path.strip_prefix(root()).unwrap().display().to_string();
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            let at = format!("{rel}:{}", n + 1);
            for old in ["code/ref/obsidian-livesync", "code/ref/livesync-commonlib"] {
                for (i, _) in line.match_indices(old) {
                    let next = line[i + old.len()..].chars().next();
                    out.check(next == Some('-'), || {
                        format!(
                            "{at}: {old} without a release; use scripts/upstream.sh (per-release reference folders)"
                        )
                    });
                }
            }
            out.check(!line.contains("scratch/upstream-cli"), || {
                format!("{at}: ~/scratch/upstream-cli is not the reference layout; use scripts/upstream.sh cli")
            });
            // A version as a default value: ${X:-1.0.49}, X=1.0.49.
            for (i, _) in line.match_indices(":-").chain(line.match_indices('=')) {
                let after = &line[i..].trim_start_matches([':', '-', '=', '"', '\'']);
                out.check(version_at(after).is_none_or(|v| v.matches('.').count() != 2), || {
                    format!("{at}: a version as a default; take it from scripts/upstream.sh")
                });
            }
        }
    }
    for rel in [
        "scripts/interop.sh",
        "scripts/testbed/testbed.sh",
        "scripts/bench/lib.sh",
    ] {
        out.check(
            read(rel).contains("scripts/upstream.sh\" cli") || read(rel).contains("scripts/upstream.sh cli"),
            || format!("{rel}: must take the official CLI from scripts/upstream.sh cli"),
        );
    }
    let wf = ".github/workflows/upstream-interop.yml";
    let text = read(wf);
    out.check(text.contains("scripts/upstream.sh"), || {
        format!("{wf}: must take the pinned release from scripts/upstream.sh")
    });
    for (n, line) in text.lines().enumerate() {
        let code = line.split(" #").next().unwrap_or("");
        if code.trim_start().starts_with('#') {
            continue;
        }
        for (j, _) in code.char_indices() {
            let prev = code[..j].chars().last();
            if prev.is_some_and(|c| c.is_ascii_alphanumeric() || c == '.' || c == '@') {
                continue;
            }
            if let Some(v) = version_at(&code[j..]) {
                out.check(v.matches('.').count() != 2, || {
                    format!(
                        "{wf}:{}: version {v} in the workflow; it takes versions from upstream.env",
                        n + 1
                    )
                });
            }
        }
        out.check(!line.contains("tools/golden/package.json"), || {
            format!(
                "{wf}:{}: reads the commonlib pin from tools/golden/package.json, not upstream.env",
                n + 1
            )
        });
    }
    out.finish();
}
