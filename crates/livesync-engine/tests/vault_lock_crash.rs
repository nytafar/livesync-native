//! A killed vault-lock holder frees the vault at once (issue #9).
//!
//! In its own test binary on purpose: it spawns a child process, and a
//! child briefly inherits every descriptor of this process between fork and
//! exec, which would hold other tests' locks for that moment.

use livesync_engine::platform::{lock_vault, read_holder, record_path, Holder, VaultLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CHILD_ENV: &str = "LIVESYNC_TEST_VAULT_LOCK_CHILD";

fn vault_in(d: &Path) -> (PathBuf, PathBuf) {
    let v = d.join("vault");
    std::fs::create_dir_all(&v).unwrap();
    let v = std::fs::canonicalize(v).unwrap();
    let f = v.join(".lsync.lock");
    (v, f)
}

/// Helper for `a_killed_holder_frees_the_vault_at_once`: does nothing
/// unless that test spawned this binary to run it.
#[test]
fn child_holder() {
    let Some(dir) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let (v, f) = vault_in(&dir);
    let _held = lock_vault(&v, &f, &Holder::current(Path::new("/state/child")), false).unwrap();
    std::fs::write(dir.join("ready"), b"").unwrap();
    std::thread::sleep(Duration::from_secs(120));
}

/// SIGKILL (as a force-stop or a crash) frees the vault at once: no stale
/// lock, no wait, nothing to delete. Without flock, as on Android's shared
/// storage, so only the host lock is at work.
#[test]
fn a_killed_holder_frees_the_vault_at_once() {
    let d = tempfile::tempdir().unwrap();
    let (v, f) = vault_in(d.path());
    let me = Holder::current(Path::new("/state/parent"));
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_holder", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, d.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(60);
    while !d.path().join("ready").exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("the child exited early: {status}");
        }
        assert!(Instant::now() < until, "the child never took the lock");
        std::thread::sleep(Duration::from_millis(20));
    }
    match lock_vault(&v, &f, &me, false) {
        Err(VaultLockError::Busy { holder: Some(h), .. }) => {
            assert_eq!(h.pid, child.id());
        }
        other => panic!("expected busy with a named holder, got {other:?}"),
    }
    // With flock on too: the child's host lock alone refuses it.
    assert!(matches!(
        lock_vault(&v, &f, &me, true),
        Err(VaultLockError::Busy { .. })
    ));
    child.kill().unwrap();
    child.wait().unwrap();
    let held = lock_vault(&v, &f, &me, false).expect("free as soon as the holder is gone");
    assert_eq!(read_holder(&record_path(&f)).unwrap().pid, std::process::id());
    drop(held);
}
