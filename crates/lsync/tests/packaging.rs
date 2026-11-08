//! The release plumbing's shell scripts, on a fake release in a temporary
//! folder (no network): `install.sh` installs only what matches the release's
//! SHA256SUMS, and `pkg/fill.sh` fills every placeholder of the AUR and
//! Homebrew templates or fails. Runs `sh`, `tar` and `curl` (for `file://`),
//! and `sha256sum` or `shasum`, as on the CI runners.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const VERSION: &str = "9.9.9-test";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The archive install.sh picks on this machine.
fn host_target() -> &'static str {
    if cfg!(target_os = "macos") {
        "universal-apple-darwin"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-unknown-linux-musl"
    } else {
        "x86_64-unknown-linux-musl"
    }
}

fn sh(script: &str, dir: &Path) -> Output {
    Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .env("TMPDIR", dir)
        .output()
        .unwrap()
}

fn ok(out: &Output) -> String {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    text
}

/// A release folder with a fake lsync (a script printing its version) packed
/// by the release's own `pack-cli.sh` for every Linux and macOS target, and
/// SHA256SUMS over them.
fn fake_release(root: &Path) -> PathBuf {
    let rel = root.join("rel");
    std::fs::create_dir_all(&rel).unwrap();
    let bin = root.join("lsync");
    std::fs::write(&bin, format!("#!/bin/sh\necho 'lsync {VERSION}'\n")).unwrap();
    for target in [
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "universal-apple-darwin",
    ] {
        ok(&sh(
            &format!(
                "'{}/scripts/ci/pack-cli.sh' '{}' {target} {VERSION} '{}'",
                repo().display(),
                bin.display(),
                rel.display()
            ),
            root,
        ));
    }
    ok(&sh(
        "if command -v sha256sum >/dev/null; then sha256sum -- *.tar.gz; else shasum -a 256 -- *.tar.gz; fi > SHA256SUMS",
        &rel,
    ));
    rel
}

fn install(root: &Path, rel: &Path, prefix: &Path) -> Output {
    sh(
        &format!(
            "sh '{}/install.sh' --version {VERSION} --base-url 'file://{}' --prefix '{}'",
            repo().display(),
            rel.display(),
            prefix.display()
        ),
        root,
    )
}

#[test]
fn install_sh_installs_a_verified_archive_into_the_prefix() {
    let d = tempfile::tempdir().unwrap();
    let rel = fake_release(d.path());
    let prefix = d.path().join("prefix");
    let text = ok(&install(d.path(), &rel, &prefix));
    assert!(text.contains("Checksum OK"), "{text}");
    let bin = prefix.join("bin/lsync");
    let out = Command::new(&bin).arg("--version").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("lsync {VERSION}"));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777, 0o755);
    // Nothing else is left beside it, and its temporary folder is gone.
    let names: Vec<_> = std::fs::read_dir(prefix.join("bin"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("lsync")]);
    assert!(!std::fs::read_dir(d.path()).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("lsync-install.")));
}

#[test]
fn install_sh_fails_closed_on_a_bad_or_missing_checksum() {
    let d = tempfile::tempdir().unwrap();
    let rel = fake_release(d.path());
    let sums = std::fs::read_to_string(rel.join("SHA256SUMS")).unwrap();
    let archive = format!("lsync-{VERSION}-{}.tar.gz", host_target());
    let prefix = d.path().join("prefix");

    // A checksum that does not match.
    let wrong: String = sums
        .lines()
        .map(|l| {
            if l.ends_with(&archive) {
                format!("{}{}", "0".repeat(64), &l[64..])
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(rel.join("SHA256SUMS"), wrong + "\n").unwrap();
    let out = install(d.path(), &rel, &prefix);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("checksum mismatch"),
        "{out:?}"
    );
    assert!(!prefix.join("bin/lsync").exists());

    // No line for the archive.
    let without: String = sums
        .lines()
        .filter(|l| !l.ends_with(&archive))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(rel.join("SHA256SUMS"), without).unwrap();
    let out = install(d.path(), &rel, &prefix);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no single line"),
        "{out:?}"
    );
    assert!(!prefix.join("bin/lsync").exists());

    // No SHA256SUMS at all.
    std::fs::remove_file(rel.join("SHA256SUMS")).unwrap();
    let out = install(d.path(), &rel, &prefix);
    assert!(!out.status.success());
    assert!(!prefix.join("bin/lsync").exists());
}

#[test]
fn fill_sh_fills_every_placeholder_or_fails() {
    let d = tempfile::tempdir().unwrap();
    let rel = fake_release(d.path());
    let out_dir = d.path().join("filled");
    let fill = |sums: &Path, out: &Path| {
        sh(
            &format!(
                "'{}/pkg/fill.sh' v{VERSION} '{}' '{}'",
                repo().display(),
                sums.display(),
                out.display()
            ),
            d.path(),
        )
    };
    ok(&fill(&rel.join("SHA256SUMS"), &out_dir));
    let sums = std::fs::read_to_string(rel.join("SHA256SUMS")).unwrap();
    let sum_of = |target: &str| {
        let name = format!("lsync-{VERSION}-{target}.tar.gz");
        sums.lines().find(|l| l.ends_with(&name)).unwrap()[..64].to_string()
    };
    let pkgbuild = std::fs::read_to_string(out_dir.join("aur/PKGBUILD")).unwrap();
    let formula = std::fs::read_to_string(out_dir.join("homebrew/lsync.rb")).unwrap();
    for text in [&pkgbuild, &formula] {
        assert!(!text.contains('@'), "a placeholder is left:\n{text}");
    }
    assert!(pkgbuild.contains(&format!("_version={VERSION}\n")), "{pkgbuild}");
    assert!(pkgbuild.contains("pkgver=9.9.9_test\n"), "{pkgbuild}");
    assert!(pkgbuild.contains(&sum_of("x86_64-unknown-linux-musl")));
    assert!(pkgbuild.contains(&sum_of("aarch64-unknown-linux-musl")));
    assert!(formula.contains(&format!("version \"{VERSION}\"")));
    for t in [
        "universal-apple-darwin",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    ] {
        assert!(formula.contains(&format!("sha256 \"{}\"", sum_of(t))), "{t}");
    }

    // A release without the universal archive cannot fill the formula.
    let partial = d.path().join("partial");
    std::fs::write(
        &partial,
        sums.lines()
            .filter(|l| !l.contains("universal"))
            .map(|l| format!("{l}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let out = fill(&partial, &d.path().join("partial-out"));
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("universal-apple-darwin"),
        "{out:?}"
    );
}
