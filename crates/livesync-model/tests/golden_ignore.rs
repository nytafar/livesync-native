//! Ignore-file decisions against commonlib's own `isAccepted` and
//! `isAcceptedAll` (fixtures from `tools/golden/ignore.mjs`, commonlib 0.4.3).
use livesync_model::ignore::{decide, IgnoreList, Lookup};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

fn fixture(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden")
        .join(name);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["meta"]["version"],
        livesync_model::upstream::COMMONLIB,
        "fixtures must come from the pinned commonlib (upstream.env)"
    );
    v
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn is_accepted_matches_upstream() {
    let v = fixture("ignore_accepted.json");
    let paths = strings(&v["paths"]);
    let cases = v["cases"].as_array().unwrap();
    let mut failures = Vec::new();
    let mut n = 0;
    for c in cases {
        let lines = strings(&c["lines"]);
        let results: Vec<char> = c["results"].as_str().unwrap().chars().collect();
        assert_eq!(results.len(), paths.len());
        let list = IgnoreList::from_lines(&lines).unwrap_or_else(|e| panic!("{lines:?}: {e}"));
        for (path, r) in paths.iter().zip(results) {
            n += 1;
            let want = match r {
                't' => Some(true),
                'f' => Some(false),
                _ => None,
            };
            let got = list.judge(path).map(|v| v.accepted);
            if got != want {
                failures.push(format!("{lines:?} {path:?}: want {want:?}, got {got:?}"));
            }
        }
    }
    assert!(n > 15_000, "{n} cases");
    assert!(
        failures.is_empty(),
        "{} of {n} differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn is_accepted_all_matches_upstream() {
    let v = fixture("ignore_all.json");
    let vault: HashMap<String, Arc<IgnoreList>> = v["vault"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, t)| (k.clone(), Arc::new(IgnoreList::parse(t.as_str().unwrap()).unwrap())))
        .collect();
    let cases = v["cases"].as_array().unwrap();
    assert!(cases.len() > 100);
    let mut root_asked = 0;
    for c in cases {
        let names = strings(&c["ignoreFiles"]);
        let path = c["path"].as_str().unwrap();
        let mut asked = Vec::new();
        let d = decide(path, &names, |dir, name| {
            asked.push(livesync_model::ignore::ignore_file_key(dir, name));
            let key = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{dir}/{name}")
            };
            match vault.get(&key) {
                Some(l) => Lookup::Found(l.clone()),
                None => Lookup::Missing,
            }
        });
        assert_eq!(d.accepted(), c["accepted"].as_bool(), "{names:?} {path:?}");
        assert_eq!(
            asked,
            strings(&c["asked"]),
            "{names:?} {path:?}: ignore files asked for"
        );
        root_asked += asked.iter().filter(|a| a.starts_with("/.")).count();
    }
    // The root ignore file is asked for as "/.gitignore" (settled here).
    assert!(root_asked > 0);
}
