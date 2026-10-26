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

## Android (Termux) build

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
# NDK: ~/Android/Sdk/cmdline-tools/latest/bin/sdkmanager "ndk;29.0.14206865"
scripts/build-android.sh         # -> target/aarch64-linux-android/release/lsync
```

The binary links only `libc`, `libm` and `libdl`. TLS is rustls with bundled webpki roots, so it needs no JVM and no platform verifier.

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
