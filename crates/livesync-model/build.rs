//! Reads the upstream pin (`upstream.env` at the repository root) into
//! compile-time environment variables `LSYNC_UPSTREAM_<KEY>`, for
//! `livesync_model::upstream`. The file is the single source of truth for
//! the upstream release (AGENTS.md, "Bumping upstream").

use std::path::PathBuf;

const KEYS: [&str; 6] = [
    "PLUGIN_RELEASE",
    "COMMONLIB",
    "OCTAGONAL_WHEELS",
    "MINIMATCH",
    "NONRECURSIVE_GLOB_MATCH",
    "BRACE_EXPANSION",
];

fn main() {
    let path = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../upstream.env");
    println!("cargo:rerun-if-changed={}", path.display());
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    for key in KEYS {
        let values: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .collect();
        let [value] = values[..] else {
            panic!("{}: want exactly one {key}=, found {}", path.display(), values.len());
        };
        assert!(
            !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)),
            "{}: odd value for {key}: {value:?}",
            path.display()
        );
        println!("cargo:rustc-env=LSYNC_UPSTREAM_{key}={value}");
    }
}
