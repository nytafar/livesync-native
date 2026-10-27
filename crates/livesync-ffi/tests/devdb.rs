//! Opt-in end-to-end check of the FFI host against the dev CouchDB (never
//! port 8410). Pull-only, so `vault-copy` is only read; the test asserts the
//! database's update sequence did not move.
//!
//! ```sh
//! source scripts/dev-env.sh
//! cargo test --release -p livesync-ffi --test devdb -- --ignored --nocapture
//! ```

use livesync_ffi::{GateOutcome, LiveSync, ProfileConfig, RunState, ScopeKind};
use livesync_remote::{Couch, RemoteConfig};
use std::time::{Duration, Instant};

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set; source scripts/dev-env.sh"))
}

#[test]
#[ignore = "needs the dev CouchDB (source scripts/dev-env.sh)"]
fn pull_only_profile_mirrors_the_dev_database_without_writing() {
    let url = env("LSYNC_URL");
    assert!(
        url.contains("127.0.0.1:8411") && !url.contains("8410"),
        "refusing anything but the dev container"
    );
    let db = env("LSYNC_DB");
    let remote = RemoteConfig {
        url: url.clone(),
        database: db.clone(),
        username: Some(env("LSYNC_USER")),
        password: Some(env("LSYNC_PASSWORD")),
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let couch = Couch::new(&remote).unwrap();
    let before = rt.block_on(couch.db_info()).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let host = LiveSync::new(dir.path().join("logs").display().to_string()).unwrap();
    let cfg = ProfileConfig {
        id: "dev".into(),
        vault_path: dir.path().join("vault").display().to_string(),
        state_dir: dir.path().join("state").display().to_string(),
        url,
        database: db,
        username: remote.username.clone().unwrap(),
        password: remote.password.clone().unwrap(),
        scope_kind: ScopeKind::All,
        scope_folders: vec![],
        pull_only: true,
        device_name: "ffi-devdb-test".into(),
        ignore_regex: "\\/\\.git\\/".into(),
    };
    let gate = host.connect_test(cfg.clone(), dir.path().join("cache").display().to_string());
    println!("connect test: {:?} {}", gate.outcome, gate.message);
    assert_eq!(gate.outcome, GateOutcome::Passed);

    let t0 = Instant::now();
    host.start(cfg.clone()).unwrap();
    // Caught up when the tracked count stops moving and `since` is set.
    let mut last = -1;
    let mut stable_since = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let st = host.status(cfg.id.clone(), cfg.vault_path.clone(), cfg.state_dir.clone());
        assert_ne!(st.run_state, RunState::Failed, "{:?}", st.run_message);
        if st.tracked_files != last {
            last = st.tracked_files;
            stable_since = Instant::now();
        }
        if last > 0 && st.since.is_some() && stable_since.elapsed() > Duration::from_secs(8) {
            println!(
                "caught up in {:.0?}: {} files tracked, counts {:?}, since {:?}",
                t0.elapsed(),
                st.tracked_files,
                st.counts,
                st.since
            );
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(600), "no catch-up within 10 min");
    }
    host.stop(cfg.id.clone());
    let after = rt.block_on(couch.db_info()).unwrap();
    assert_eq!(before.update_seq, after.update_seq, "pull-only must not write");
    assert_eq!(before.doc_count, after.doc_count);
}

#[test]
#[ignore = "needs the dev CouchDB (source scripts/dev-env.sh)"]
fn listing_the_dev_database_folders_is_quick_and_read_only() {
    let url = env("LSYNC_URL");
    assert!(
        url.contains("127.0.0.1:8411") && !url.contains("8410"),
        "refusing anything but the dev container"
    );
    let db = env("LSYNC_DB");
    let remote = RemoteConfig {
        url: url.clone(),
        database: db.clone(),
        username: Some(env("LSYNC_USER")),
        password: Some(env("LSYNC_PASSWORD")),
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let couch = Couch::new(&remote).unwrap();
    let before = rt.block_on(couch.db_info()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let host = LiveSync::new(String::new()).unwrap();
    let cfg = ProfileConfig {
        id: "dev".into(),
        vault_path: dir.path().join("vault").display().to_string(),
        state_dir: dir.path().join("state").display().to_string(),
        url,
        database: db,
        username: remote.username.clone().unwrap(),
        password: remote.password.clone().unwrap(),
        scope_kind: ScopeKind::All,
        scope_folders: vec![],
        pull_only: true,
        device_name: "ffi-devdb-test".into(),
        ignore_regex: String::new(),
    };
    let t0 = Instant::now();
    let l = host.list_folders(cfg, None).unwrap();
    let wall = t0.elapsed();
    for f in &l.folders {
        println!("{:>7} {:>12}  {}/", f.files, f.bytes, f.name);
    }
    println!(
        "{} files ({} at the root) in {} folders; {} docs read, {} bytes fetched, {} ms in the core, {:.0?} wall",
        l.files,
        l.root_files,
        l.folders.len(),
        l.docs_read,
        l.bytes_fetched,
        l.elapsed_ms,
        wall
    );
    assert!(l.files > 0 && !l.folders.is_empty());
    assert!(l.folders.iter().all(|f| !f.name.starts_with('.')));
    assert!(wall < Duration::from_secs(5), "took {wall:.0?}");
    let after = rt.block_on(couch.db_info()).unwrap();
    assert_eq!(before.update_seq, after.update_seq, "listing must not write");
    assert_eq!(before.doc_count, after.doc_count);
}
