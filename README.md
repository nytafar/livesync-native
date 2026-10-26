# livesync-android

A native client for [Self-hosted LiveSync](https://github.com/vrtmrz/obsidian-livesync): a Rust sync engine that talks to the LiveSync CouchDB database directly and keeps a plain folder (an Obsidian vault) in step with it. The engine becomes the core of an always-on Android app. For now it ships as the `lsync` command-line tool for Linux and for Termux on Android.

**Status: milestone M2b, the Android app.** `lsync sync` and `lsync daemon` pull and push; `lsync pull` and `lsync daemon --pull-only` stay read-only. The Android app (`android/`, built by `scripts/build-apk.sh`) runs the same engine always-on in a foreground service, pull-only or two-way per vault. End-to-end encryption (M3) is not here yet.

## What it does

- Follows the database's `_changes` feed with a `_selector` that leaves out chunks, then fetches chunks on demand through `POST _all_docs {keys}`.
- **Pushes local changes** the way the official clients store them: the `v3-rabin-karp` splitter ported byte for byte (so chunk IDs match and deduplicate), chunks uploaded before the metadata that names them, metadata written against the revision the file was synced at. If the remote moved on meanwhile, the local edit is stored as a child of that revision instead (a conflict the desktop plugin resolves); nothing is overwritten.
- **Deletes are soft deletes** (`deleted: true`) against the synced revision, only for files that were in sync. A move into `.trash/` is Obsidian's delete; a move out of the synced folders (or into another dot-folder) is not a delete. A pass that would delete more than half of the tracked files stops and asks instead.
- Watches the vault (one watch per folder, never `.obsidian/`), waits until a file has been stable for 2 s, and rescans every 15 minutes in case an event was missed.
- **Folder scope:** all folders by default, or only some (`--include`), or all but some (`--exclude`), matched case-insensitively. Files outside the scope are invisible in both directions. Widening fetches the newly visible documents; narrowing stops tracking without touching the remote.
- Registers itself in the milestone (`node_info`, `tweak_values`, `node_chunk_info`) after a complete fetch, and marks itself resolved after a fresh start or a rebuild elsewhere, so a rebuilt and locked database is recovered from automatically.
- Reassembles `plain` (text) and `newnote` (binary) files. It decompresses chunks when needed, reads legacy eden chunks, and writes each file through a hidden temp file plus rename.
- Keeps a small SQLite **sync index**. It records one row per path: the revision on disk, its chunk IDs, and the size, mtime and xxh64 hash of the bytes. It also stores the changes sequence, the remote identity and the PREFERRED tweaks adopted at setup.
- **Never overwrites or deletes a local file that changed since it was synced.** Such files are reported as `dirty`.
- Skips the following:
  - hidden-file and customisation documents (`i:`, `ix:`, `ps:`) and every dot-path (`.obsidian/`, `.trash/`, dotfiles);
  - `redflag*.md`, `flag_*.md` and `livesync_log_*` at the vault root;
  - anything matching the local `syncIgnoreRegEx` (upstream syntax: entries joined by `|[]|`, `!!` negates, spaces and newlines are dropped);
  - files over `syncMaxSizeInMB`;
  - names the vault's filesystem cannot hold. The rules (forbidden characters, case and Unicode folding, name length) are probed on the vault's filesystem when it is opened, so Android's shared storage, an exFAT stick and ext4 each get their own. Such names are marked `skipped` ("unwritable name"), shown by `status` and never retried;
  - a second document whose name folds to the same file on a case-insensitive filesystem ("case collision with …").
- **Handles errors one file at a time.** A local I/O error on one file (unreadable, a file where a folder should be) defers that file to the retry queue; the rest of the batch and the changes sequence move on.
- Creates an empty `.nomedia` at the vault root, so MediaStore and Google Photos leave the vault's images alone.
- Allows one process per state directory and per vault (lock files `lsync.lock` in the state directory and `.lsync.lock` at the vault root).
- Catches a local edit that lands while a remote change is being written (the old file is held open across the swap and checked again), and puts it back as a local edit.
- **Fails closed.** It refuses to sync when it meets any of these:
  - a server that is not CouchDB;
  - a version document that is missing, above 13 or invalid, or that has unknown `used_features`;
  - a missing milestone, or one that is locked and doesn't list us;
  - incompatible chunk-version ranges;
  - encryption, path obfuscation, a `hashAlg` other than xxhash64, or `pathNormalizationVersion=1`;
  - PREFERRED tweaks that changed since setup;
  - encrypted content;
  - an index that belongs to another remote or another vault folder.
- If the milestone's `created` changes (the database was rebuilt elsewhere), it resets the index and refetches. Files that are still clean are refreshed; edited ones are kept.

## Usage

```sh
# Save a profile (config at ~/.config/lsync/<profile>.toml, mode 0600)
printf '%s' "$PASSWORD" | lsync --url https://couch.example.net --db vault --user me \
    --vault ~/Documents/Obsidian/Vault configure --password-stdin

lsync connect               # run the compatibility gate and print what it found
lsync pull                  # one-shot pull into the vault folder (read-only)
lsync sync                  # one-shot two-way sync: pull, register, push local changes
lsync daemon                # two-way, continuously (watcher + longpoll); kill -USR1 = "sync now"
lsync daemon --pull-only    # follow changes, read-only
lsync fetch                 # start over from the remote (upstream's "Fetch"), clears a lock
lsync status                # index summary, last gate result, files needing attention

lsync --include Notes,Projects configure   # sync only these folders (saved in the profile)
lsync --exclude Private configure          # sync everything except these
lsync --all-folders configure              # back to everything
lsync probe <vault>         # on-phone check: Obsidian vs outside writes, inotify vs Obsidian saves
                            # (refuses vaults where the LiveSync plugin is enabled)
```

Every setting can also come from flags or `LSYNC_*` environment variables, for example `LSYNC_URL`, `LSYNC_DB`, `LSYNC_USER`, `LSYNC_PASSWORD`, `LSYNC_VAULT`, `LSYNC_STATE_DIR`, `LSYNC_IGNORE_REGEX`, `LSYNC_INCLUDE`, `LSYNC_EXCLUDE` and `LSYNC_DEVICE_NAME`. Flags win over the environment, and the environment wins over the config file. The index lives in `$XDG_STATE_HOME/lsync/<profile>/index.sqlite` unless `--state-dir` says otherwise.

Exit codes:

- 0: OK.
- 1: error.
- 2: refused by a gate.
- 3: pull or sync finished, but something needs attention (dirty or pending files, local files that cannot sync, a safety stop).
- 4: probe failed or was inconclusive.

## Layout

| Path | What |
|---|---|
| `crates/livesync-model` | Pure model: doc types, path→ID, chunk IDs, the splitter, decompression codec, gate evaluation, milestone registration, folder scope |
| `crates/livesync-remote` | CouchDB over HTTP (reqwest + rustls with bundled webpki roots): `_changes`, `_all_docs`, `_local` docs, backoff |
| `crates/livesync-engine` | Pull and push loops, daemons, SQLite index, vault file operations; platform seams (`Watcher`, `FsRules`) |
| `crates/lsync` | The CLI, including `probe` |
| `crates/livesync-ffi` | UniFFI surface for the app: one host, one engine per profile, connect test, status, logs |
| `crates/uniffi-bindgen` | UniFFI's Kotlin binding generator, pinned to the same version |
| `android/` | The Android app (Kotlin, Compose): foreground service, onboarding, status screen |
| `tools/golden` | Node program pinned to `@vrtmrz/livesync-commonlib` 0.2.1; it emits the fixtures in `fixtures/golden` |
| `scripts/` | APK and Termux builds, dev-database environment, a vault comparison script, and `interop.sh` (the M2 scenarios against the official CLI) |

Build and test instructions are in [AGENTS.md](AGENTS.md).
