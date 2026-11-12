# AGENTS.md

Build, test and safety rules for this repo. Project docs (spec, research, findings, handoffs) live outside the repo.

## Gate

Everything below must pass before a commit:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`cargo test` needs no network or database. The engine tests run against an in-process mock CouchDB with revision trees (`crates/livesync-engine/tests/common`). The golden tests read `fixtures/golden/*.json`, including the splitter vectors.

## Golden vectors

`fixtures/golden/` is generated. Don't edit it by hand. To regenerate it from the pinned commonlib (a bump does this itself, below):

```sh
cd tools/golden
npm ci --ignore-scripts          # the commonlib and matcher versions of upstream.env (package.json)
node generate.mjs ../../fixtures/golden   # refuses packages other than the pin
```

`overrides` in `tools/golden/package.json` pins commonlib's matcher dependencies (`minimatch`, `@isaacs/nonrecursive-glob-match`, `brace-expansion`) to what commonlib resolves to in the `package-lock.json` of the pinned release, so the ignore vectors are what that plugin and CLI do. The golden tests compare each fixture's recorded versions with the pin (`livesync_model::upstream`). A bump can change upstream's merge results; the merge corpus test then says which cases differ.

## Bumping upstream

`upstream.env` is the single source of truth for the upstream release lsync is built and tested against: the plugin and CLI release tag (`PLUGIN_RELEASE`), the commonlib that release pins, the octagonal-wheels that commonlib pins and the glob matcher it resolves to. Everything else derives from it or is checked against it:

- Rust reads it at compile time (`crates/livesync-model/build.rs` -> `livesync_model::upstream`): `TESTED_PLUGIN_VERSION`, the version every refusal names, and the golden tests' version checks.
- Scripts ask `scripts/upstream.sh` (`get KEY`, `cli`, `ref`, `ls`): `scripts/interop.sh`, `scripts/testbed/testbed.sh`, `scripts/bench/*` and the weekly workflow run the official CLI of the pinned release, built on first use. `LSYNC_CLI_JS` (a CLI build) or `LSYNC_UPSTREAM_TAG` (another release) override it, and every override is announced in the output.
- `tools/golden/generate.mjs` refuses to generate from packages other than the pin.

To move to a new release (default: the latest; `--dry-run` shows what would change):

```sh
scripts/bump-upstream.sh [TAG]
```

It derives the versions from the release's own `package.json` and `package-lock.json`, writes `upstream.env` and `tools/golden/package.json`, installs (lockfile), regenerates `fixtures/golden`, rewrites the pin markers in the docs, fetches the reference source and builds the CLI, shows which fixtures changed beyond their version stamp, and prints what is left: the gate, `scripts/interop.sh`, reading upstream's changes between the two tags, and the notes in `COMPATIBILITY.md`. It is safe to re-run and never commits.

The gate test `crates/livesync-model/tests/upstream_pin.rs` fails, naming the file, when any of these disagree with the pin: `tools/golden/package.json` and its lockfile (as node would resolve each package), the versions recorded in `fixtures/golden/*.json`, the tested-versions table in `COMPATIBILITY.md`, the pin markers in `README.md`, `AGENTS.md` and `COMPATIBILITY.md`, and the scripts and workflow (no version defaults, no unversioned reference checkout, the CLI from `scripts/upstream.sh`).

- **Pins and history.** In the docs a version that states the pin is written after a hidden marker, `<!--pin:KEY-->VALUE`; the bump rewrites those, and the test fails on a version right after "pinned to", "tested against" or "known to work" without one. Every other version is history ("new in 1.0.46", "unchanged since 0.2.1") and stays. Prefer pointing at `upstream.env` to repeating a number.
- **Cite by the pinned version.** Code and docs that describe upstream behaviour cite the release it was read or checked in (the pin at the time), e.g. "commonlib 0.4.3 `isValidChunk`"; read it in the pinned reference source, not in an older checkout.
- **Reference source, one folder per release**, under `~/code/ref` (`LSYNC_REF_DIR` overrides): `obsidian-livesync-<tag>` (the release, with its CLI and the plug-in built), `livesync-commonlib-<version>` (its tag) and `octagonal-wheels-<version>` (the npm package). `scripts/upstream.sh ref` fetches those of the pin. Each is built aside and renamed into place, then never changed, so a bump never touches files another session uses; `scripts/upstream.sh ls` lists them, and old ones are removed by hand. The unversioned `~/code/ref/obsidian-livesync` and `~/code/ref/livesync-commonlib` checkouts are not used by any script.

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

`.github/workflows/ci.yml` runs the gate on Linux and macOS, the Android gate (`scripts/build-apk.sh --unsigned`) and the universal macOS binary (both architectures, `scripts/ci/macos-universal.sh`, then `install.sh` from the built archives) on every push and PR.

`.github/workflows/upstream-interop.yml` runs `scripts/interop.sh` weekly (and by hand: `gh workflow run upstream-interop.yml [-f tag=<release>]`) against the official CLI of the pinned release and, when upstream has a newer one, of that release too (built by `scripts/upstream.sh cli`), on a throwaway CouchDB service container published on 127.0.0.1:8411 and set up by `scripts/ci/couchdb-init.sh`. Its pin job fails when upstream has a newer release or npm a newer commonlib than `upstream.env`: run `scripts/bump-upstream.sh` ("Bumping upstream"). It publishes nothing. To try another release locally: `LSYNC_UPSTREAM_TAG=<tag> scripts/interop.sh` (it builds that release into its own reference folder and says it is not the pin).

A release is a pushed tag; `.github/workflows/release.yml` does the rest:

```sh
git tag -a v0.2.0 -m "v0.2.0" && git push origin v0.2.0
gh run watch "$(gh run list -w Release -L1 --json databaseId -q '.[0].databaseId')"
```

- Tags are `v<major>.<minor>.<patch>[-<pre>]`; a `-<pre>` tag makes a pre-release. The APK's versionName is the tag without `v`; its versionCode is `VERSION_CODE_BASE` (in the workflow) plus the workflow's run number, so it rises with every run. Only ever raise the base; renaming the workflow file resets the run number.
- Assets: `livesync-android-<version>.apk` (signed with the sideload key), `lsync-<version>-<target>.tar.gz` for `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`, `x86_64-apple-darwin`, `universal-apple-darwin` (lipo of the two) and `aarch64-linux-android` (Termux), `install.sh`, `SHA256SUMS`, notes with the commits since the previous tag. Each archive holds `lsync-<version>-<target>/` with `lsync`, `README.md`, the licences and `NOTICE` (`scripts/ci/pack-cli.sh`); `install.sh`, `pkg/` and the Docker build rely on that layout.
- After a release, the AUR package, the Homebrew formula and the Docker image are published by hand from `pkg/` (`pkg/README.md`): `pkg/fill.sh` fills the templates' version and checksums from the release's `SHA256SUMS`. `crates/lsync/tests/packaging.rs` runs `install.sh` and `fill.sh` on a fake release in the gate.
- Signing comes from repository secrets `SIDELOAD_KEYSTORE_BASE64`, `SIDELOAD_STORE_PASSWORD`, `SIDELOAD_KEY_PASSWORD` (and optional `SIDELOAD_KEY_ALIAS`, default `livesync`); the optional repository variable `SIDELOAD_CERT_SHA256` pins the signer. A tag push without them fails; it is never released unsigned.
- `gh workflow run release.yml -f version=0.0.0-ci-test` builds the same assets into a draft release (unsigned when the secrets are missing) without creating a tag. Delete it afterwards: `gh release delete v0.0.0-ci-test --yes`.
- A hand build with `scripts/build-apk.sh` uses the commit count as versionCode, which is below every CI release: set `LIVESYNC_VERSION_CODE` above the installed one to install it over a release.

## Dev database: rules

- **Never connect to the real CouchDB (port 8410).** Development uses only the dev container `lsync-dev-couchdb` at `127.0.0.1:8411`. Its credentials are in `~/.config/livesync-android/dev.env`.
- **Writes go to throwaway databases only.** `vault-copy` on the dev container is a copy of the real vault: read it, or copy it for rebuild realism, but never sync two-way against it. The interop script (`scripts/interop.sh`) creates and drops its own `lsync-it-*` databases.
- `source scripts/dev-env.sh` maps the dev credentials to `LSYNC_*` variables and refuses anything on port 8410.
- Credentials never go into the repo, test fixtures or commit messages.
- `lsync pull` and `lsync daemon --pull-only` (`Engine::pull_once`, `Engine::daemon_pull_only`) never write to the remote: no document writes and no milestone registration. The mock-server tests assert it. Writes happen only in `sync`, `fetch` and the two-way `daemon`.
- **The real database and the live desktop vault are used only through the testbed's snapshot** (below). A field check against the real database, when a phase's exit calls for one, is a deliberate step: run the nightly backup first (`systemctl --user start livesync-backup.service`) and check that it verified, then use a scoped vault folder of its own. Never point a test at the live desktop vault itself.

## Desktop testbed

The desktop and server work is built and checked here, on this machine (`scripts/testbed/testbed.sh`):

```sh
scripts/testbed/testbed.sh up                                   # start the dev container (it survives reboots)
scripts/testbed/testbed.sh new t1 --machines a,b                # tb-t1, initialised by the pinned official CLI
scripts/testbed/testbed.sh new big --from snapshot --adopt a    # a clone of the real database (about 70 s for 440k docs);
                                                                # machine a starts from the matching folder
scripts/testbed/testbed.sh run t1 a sync                        # target/release/lsync as machine a
eval "$(scripts/testbed/testbed.sh env t1 b)"                   # this shell as machine b
scripts/testbed/testbed.sh cli t1 ls                            # the official CLI as another node of t1
scripts/testbed/testbed.sh status
scripts/testbed/testbed.sh drop t1
```

- **Machines.** Each has its own vault, `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `LSYNC_RUNTIME_DIR` under `~/scratch/lsync-testbed/<name>/<machine>/`, and no `LSYNC_*` variable leaks in. Two machines, and later two `lsync serve`, run side by side without touching any real config or service. `lsync serve` and the control socket must honour `LSYNC_RUNTIME_DIR`.
- **The snapshot.** `real-snap` is a read-only copy of the real database; `~/scratch/lsync-testbed/real-copy` is the matching vault folder (minus the LiveSync plugin's folder, which holds credentials).
  - Clone it with `new … --from snapshot`; never write to `real-snap` itself.
  - `snapshot --force` re-takes both. It is the one command that reads the real server and the desktop vault, and only reads them. Run the nightly backup first.
  - Which folder and database: `~/.config/livesync-android/testbed.env` (`TESTBED_SOURCE_VAULT`, optionally `REAL_DB`), outside the repo.
- **The official CLI** is the pinned release's (`scripts/upstream.sh cli`). A new empty database gets `pathNormalizationVersion` 0 from it, the path identity lsync syncs (a CLI from 1.0.46 on would otherwise set up NFC path identity, which lsync refuses).
- **Limits.** Writes go only to `tb-*` databases on the dev container and only under `~/scratch/lsync-testbed`. Keep vault folders there, never in `/tmp`, which is tmpfs.

## Interop check against the official CLI

Not part of the gate (it needs the dev container and node), but run it after any change to the engine:

```sh
scripts/interop.sh                 # every scenario; drops its throwaway database
KEEP=1 ONLY="S4 S6" scripts/interop.sh
```

It creates `lsync-it-<run>` on the dev container, drives the official CLI (run with `node` directly) and `lsync` through the M2 scenarios, and prints PASS/FAIL lines. It refuses any URL other than `127.0.0.1:8411`.

The official CLI is the pinned release's (`upstream.env`), from its reference folder, built on first use by `scripts/upstream.sh cli`. `LSYNC_UPSTREAM_TAG=<tag>` runs another release (built into its own folder) and `LSYNC_CLI_JS=<dist/index.cjs>` a given build; both are announced, and the run's header line says whether the CLI is the pinned release.

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
- The official CLI and the plug-in contender are the pinned release's (`scripts/upstream.sh cli` builds both into the release's reference folder, which the plug-in driver runs from). The plug-in contender also needs an extracted Obsidian AppImage at `~/scratch/lsync-bench/obsidian/squashfs-root` (`OBSIDIAN_DIR` overrides). It uses its own `--user-data-dir`, HOME and remote-debugging port (9333, never 9222).
- `run.sh` turns on CORS on the dev container (the same origins as the real server), which the plug-in needs.

## End-to-end check against the dev database

```sh
cargo build --release
source scripts/dev-env.sh
export LSYNC_VAULT=/tmp/lsync-e2e/vault LSYNC_STATE_DIR=/tmp/lsync-e2e/state
./target/release/lsync connect
./target/release/lsync pull --json
scripts/verify-against-vault.py /tmp/lsync-e2e/vault --reference <desktop-vault> --index /tmp/lsync-e2e/state/index.sqlite
```

`verify-against-vault.py` only reads the reference vault (`--reference`, required). It explains differences by:

- the reference file changing after the copy time;
- the reference vault's `.gitignore`, which the desktop LiveSync node uses as ignore file;
- symlinks;
- conflicted documents.
