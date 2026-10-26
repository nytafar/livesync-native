# AGENTS.md

Build, test and safety rules for this repo. Project docs (spec, research, findings, handoffs) live outside the repo in `~/hvelv/repos/livesync-android/`.

## Gate

Everything below must pass before a commit:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`cargo test` needs no network or database. The engine tests run against an in-process mock CouchDB (`crates/livesync-engine/tests/common`). The golden tests read `fixtures/golden/*.json`.

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

- **Never connect to the real CouchDB (port 8410).** Development uses only the dev container `lsync-dev-couchdb` at `127.0.0.1:8411`, database `vault-copy`. Its credentials are in `~/.config/livesync-android/dev.env`.
- `source scripts/dev-env.sh` maps the dev credentials to `LSYNC_*` variables and refuses anything on port 8410.
- Credentials never go into the repo, test fixtures or commit messages.
- M1 is read-only. Nothing in `livesync-remote` may issue `PUT`, `DELETE`, `_bulk_docs` or milestone writes. The mock-server tests assert that no write requests are made.

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
