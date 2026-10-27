# AGENTS.md

Build, test and safety rules for this repo. Project docs (spec, research, findings, handoffs) live outside the repo in `~/hvelv/repos/livesync-android/`.

## Gate

Everything below must pass before a commit:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`cargo test` needs no network or database. The engine tests run against an in-process mock CouchDB with revision trees (`crates/livesync-engine/tests/common`). The golden tests read `fixtures/golden/*.json`, including the splitter vectors.

## Golden vectors

`fixtures/golden/` is generated. Don't edit it by hand. To regenerate it, for example when upstream bumps `@vrtmrz/livesync-commonlib`:

```sh
cd tools/golden
npm ci                           # installs the pinned commonlib (see package.json)
node generate.mjs ../../fixtures/golden
```

Bump the pinned version in `tools/golden/package.json`, and the version assertion in `crates/livesync-model/tests/golden.rs`, together.

## Android app (APK)

```sh
scripts/build-apk.sh             # -> dist/livesync-android-<version>.apk + .sha256
```

It builds `livesync-ffi` for `arm64-v8a` (cargo-ndk, API 30) and for the host, generates the Kotlin bindings with the workspace's `uniffi-bindgen` into `android/app/build/generated/` (never committed), then runs Gradle: `testReleaseUnitTest` (including `CoreBindingTest`, which loads the host build of the core through the generated bindings and cannot be skipped here), `lintRelease` (no errors allowed) and `assembleRelease`. That is the Android gate; run it after any change under `android/` or `crates/livesync-ffi`. `--skip-checks` builds only.

- Toolchain: Android SDK at `$ANDROID_HOME` (default `~/Android/Sdk`) with platform 35, build-tools 35.0.0 and NDK r29; JDK 21 from mise (`LIVESYNC_JAVA_HOME` overrides; Gradle 8.14 does not run on JDK 27). Gradle comes from the wrapper (checksum-pinned).
- Signing: the sideload keystore lives in `~/.config/livesync-android/` (`sideload.jks` and `sideload.properties`, both 0600), created by the script on first use. Never commit them; `LIVESYNC_SIGNING` points at another properties file.
- `cargo test -p livesync-ffi` runs the FFI host against the engine's mock CouchDB. The opt-in `devdb` test runs it against the dev container (read-only, asserts `update_seq` did not move): `source scripts/dev-env.sh && cargo test --release -p livesync-ffi --test devdb -- --ignored`.

## Android (Termux) build

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
# NDK: ~/Android/Sdk/cmdline-tools/latest/bin/sdkmanager "ndk;29.0.14206865"
scripts/build-android.sh         # -> target/aarch64-linux-android/release/lsync
```

The binary links only `libc`, `libm` and `libdl`. TLS is rustls with bundled webpki roots, so it needs no JVM and no platform verifier.

## CI and releasing

`.github/workflows/ci.yml` runs the gate on Linux and macOS and the Android gate (`scripts/build-apk.sh --unsigned`) on every push and PR.

A release is a pushed tag; `.github/workflows/release.yml` does the rest:

```sh
git tag -a v0.2.0 -m "v0.2.0" && git push origin v0.2.0
gh run watch "$(gh run list -w Release -L1 --json databaseId -q '.[0].databaseId')"
```

- Tags are `v<major>.<minor>.<patch>[-<pre>]`; a `-<pre>` tag makes a pre-release. The APK's versionName is the tag without `v`; its versionCode is `VERSION_CODE_BASE` (in the workflow) plus the workflow's run number, so it rises with every run. Only ever raise the base; renaming the workflow file resets the run number.
- Assets: `livesync-android-<version>.apk` (signed with the sideload key), `lsync-<version>-<target>.tar.gz` for `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`, `x86_64-apple-darwin` and `aarch64-linux-android` (Termux), `SHA256SUMS`, notes with the commits since the previous tag.
- Signing comes from repository secrets `SIDELOAD_KEYSTORE_BASE64`, `SIDELOAD_STORE_PASSWORD`, `SIDELOAD_KEY_PASSWORD` (and optional `SIDELOAD_KEY_ALIAS`, default `livesync`); the optional repository variable `SIDELOAD_CERT_SHA256` pins the signer. A tag push without them fails; it is never released unsigned.
- `gh workflow run release.yml -f version=0.0.0-ci-test` builds the same assets into a draft release (unsigned when the secrets are missing) without creating a tag. Delete it afterwards: `gh release delete v0.0.0-ci-test --yes`.
- A hand build with `scripts/build-apk.sh` uses the commit count as versionCode, which is below every CI release: set `LIVESYNC_VERSION_CODE` above the installed one to install it over a release.

## Dev database: rules

- **Never connect to the real CouchDB (port 8410).** Development uses only the dev container `lsync-dev-couchdb` at `127.0.0.1:8411`. Its credentials are in `~/.config/livesync-android/dev.env`.
- **Writes go to throwaway databases only.** `vault-copy` on the dev container is a copy of the real vault: read it, or copy it for rebuild realism, but never sync two-way against it. The interop script (`scripts/interop.sh`) creates and drops its own `lsync-it-*` databases.
- `source scripts/dev-env.sh` maps the dev credentials to `LSYNC_*` variables and refuses anything on port 8410.
- Credentials never go into the repo, test fixtures or commit messages.
- `lsync pull` and `lsync daemon --pull-only` (`Engine::pull_once`, `Engine::daemon_pull_only`) never write to the remote: no document writes and no milestone registration. The mock-server tests assert it. Writes happen only in `sync`, `fetch` and the two-way `daemon`.

## Interop check against the official CLI

Not part of the gate (it needs the dev container and node), but run it after any change to the engine:

```sh
scripts/interop.sh                 # every scenario; drops its throwaway database
KEEP=1 ONLY="S4 S6" scripts/interop.sh
```

It creates `lsync-it-<run>` on the dev container, drives the official CLI (`~/code/ref/obsidian-livesync/src/apps/cli/dist/index.cjs`, run with `node` directly) and `lsync` through the M2 scenarios, and prints PASS/FAIL lines. It refuses any URL other than `127.0.0.1:8411`.

## Benchmark against the official clients

Not part of the gate. `scripts/bench/` compares lsync with the official LiveSync CLI and the official plug-in (in a throwaway, headless Obsidian) on clones of one snapshot of the real database, with the phone's folder excludes:

```sh
scripts/bench/snapshot.sh          # once: copy the real `vault` into bench-snap on the dev container
scripts/bench/run.sh --reps 3      # cold sync, catch-up, idle, live latency -> $BENCH_WORK/results.jsonl
python3 scripts/bench/summarise.py "$BENCH_WORK"
```

- `snapshot.sh` is the one script allowed to touch port 8410, and only to read: the dev container replicates from it with `use_checkpoints: false` (nothing is written to the source) and the `_local` LiveSync docs are copied with GETs.
- Every run writes to its own `bench-*` clone on the dev container, re-cloned for each repetition.
- lsync runs as the `lsync-bench` systemd user unit (`scripts/bench/install-unit.sh` installs it, never enables it; profile in `~/.config/lsync/bench.toml`, mode 0600).
- The plug-in contender needs `npm run build` in the official repo and an extracted Obsidian AppImage at `~/scratch/lsync-bench/obsidian/squashfs-root` (`OBSIDIAN_DIR` overrides). It uses its own `--user-data-dir`, HOME and remote-debugging port (9333, never 9222).
- `run.sh` turns on CORS on the dev container (the same origins as the real server), which the plug-in needs.

## End-to-end check against the dev database

```sh
cargo build --release
source scripts/dev-env.sh
export LSYNC_VAULT=/tmp/lsync-e2e/vault LSYNC_STATE_DIR=/tmp/lsync-e2e/state
./target/release/lsync connect
./target/release/lsync pull --json
scripts/verify-against-vault.py /tmp/lsync-e2e/vault --index /tmp/lsync-e2e/state/index.sqlite
```

`verify-against-vault.py` only reads the reference vault (`~/hvelv`). It explains differences by:

- the reference file changing after the copy time;
- the reference vault's `.gitignore`, which the desktop LiveSync node uses as ignore file;
- symlinks;
- conflicted documents.
