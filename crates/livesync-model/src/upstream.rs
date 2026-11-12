//! The upstream release this crate is built and tested against, from the pin
//! file `upstream.env` at the repository root (read by `build.rs`). Every
//! version that must move with an upstream bump comes from here or from that
//! file, never from a literal (AGENTS.md, "Bumping upstream").

/// The Self-hosted LiveSync release tag (plugin and official CLI).
pub const PLUGIN_RELEASE: &str = env!("LSYNC_UPSTREAM_PLUGIN_RELEASE");
/// `@vrtmrz/livesync-commonlib`, as that release pins it.
pub const COMMONLIB: &str = env!("LSYNC_UPSTREAM_COMMONLIB");
/// `octagonal-wheels`, as that commonlib pins it.
pub const OCTAGONAL_WHEELS: &str = env!("LSYNC_UPSTREAM_OCTAGONAL_WHEELS");
/// The glob matcher commonlib resolves to in that release (`minimatch` and
/// its dependencies).
pub const MINIMATCH: &str = env!("LSYNC_UPSTREAM_MINIMATCH");
pub const NONRECURSIVE_GLOB_MATCH: &str = env!("LSYNC_UPSTREAM_NONRECURSIVE_GLOB_MATCH");
pub const BRACE_EXPANSION: &str = env!("LSYNC_UPSTREAM_BRACE_EXPANSION");
