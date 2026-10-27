# livesync-android

**Self-hosted LiveSync for Android that keeps syncing when Obsidian is closed.**

[Self-hosted LiveSync](https://github.com/vrtmrz/obsidian-livesync) keeps Obsidian vaults in sync through your own CouchDB. On the desktop it works well. On Android it struggles:

- **It only syncs while Obsidian is open.** The plugin runs inside the app, and Android suspends it in the background. Notes edited elsewhere arrive only after you open Obsidian, and then you wait while it catches up.
- **It keeps the whole database on the phone.** The plugin stores a full replica in Obsidian's IndexedDB: every document and every chunk, including old revisions. For our vault that is 1.8 GB and over 300,000 documents.
- **It does the heavy work on Obsidian's UI thread.** Chunking, hashing and replication share the thread you type on.

So we rebuilt the client: a small native sync engine in Rust that talks to the same CouchDB database, wire-compatible with the official plugin. It runs on its own, always on, and keeps a plain folder in sync. Obsidian opens that folder as an ordinary vault, with the LiveSync plugin turned off on that device. The desktop plugin and the official CLI keep working against the same database.

## Two ways to run it

### Android app

A foreground service keeps one or more vault folders in sync, whether or not Obsidian is open.

- Edits from other devices land in the phone's vault in under a second, and phone edits are pushed about 2 s after Obsidian saves them.
- It keeps only a small index on the phone and fetches content on demand: the first sync of 2,458 files took 62 s.
- It comes back by itself after a reboot or an app update.
- Each vault can be pull-only (read-only, the safe way to start) or two-way, and can sync all folders, only some, or all but some.
- The password is encrypted with a key in the Android Keystore.

Requires Android 11 or later. Build the APK with `scripts/build-apk.sh` (see [AGENTS.md](AGENTS.md)), install it, then:

1. Grant all-files access, notifications and the battery exemption on the setup screen.
2. Pick a **new** folder under `Documents/Obsidian/`.
3. Enter your CouchDB URL, database, user and password, then press Test connection.
4. Save. Once the first sync finishes, open the folder in Obsidian ("Open folder as vault") and leave the LiveSync plugin off there.

### `lsync`, the command-line client

The same engine as a single binary for Linux (and Termux on Android): one-shot pulls and syncs, or a long-running daemon for a server or a desktop without Obsidian.

```sh
printf '%s' "$PASSWORD" | lsync --url https://couch.example.net --db vault --user me \
    --vault ~/Notes configure --password-stdin
lsync connect            # check that the database is compatible
lsync daemon             # two-way, continuously
lsync daemon --pull-only # follow the server, never write to it
```

Full usage is under [Reference](#reference).

## How it compares

Measured on oma (i9-10900K, btrfs on NVMe, Linux), on clones of one snapshot of a real 425,000-document database (about 2,200 files in scope after the phone's folder excludes), with lsync's release build, the official LiveSync CLI 1.0.40 and Node 26. Medians of 3 runs for lsync and 1–2 for the CLI.

| | lsync | Official LiveSync CLI | Obsidian plugin |
|---|---|---|---|
| First sync (wall time) | 21 s | 20–25 min | deferred |
| CPU time, first sync | 2.4 s | 1,440–1,760 s | deferred |
| Peak memory, first sync | 96 MB | 2.9 GB | deferred |
| Local state on disk | 2.2 MB (SQLite index) | 156 MB (full PouchDB replica) | deferred |
| Catch-up after 1,000 changes | 3.7 s | 12 min | deferred |
| Change on the server to file, live | 0.01 s | 0.8 s | deferred |
| File change to the server, live | 2.5 s | 0.9 s | deferred |
| Idle (4 min) | 0.02 s CPU, 27 MB | 0.2 s CPU, 430 MB | deferred |

Both clients wrote the same 2,202 files byte for byte (lsync also writes the two conflicted files, which the CLI skips). The CLI's slow first sync is mostly one upstream hot spot: its key-value store rewrites a 34 MB JSON file on every write. The desktop plugin doesn't use that store, so the CLI numbers are not the plugin's. lsync waits 2 s for a changed file to settle before it pushes, which guards against half-written files. That is why it is slower in that one row.

To rerun: `scripts/bench/snapshot.sh` once, then `scripts/bench/run.sh --contenders lsync,official` and `python3 scripts/bench/summarise.py "$BENCH_WORK"`. See `AGENTS.md` for the prerequisites.

## Safety

- **It never overwrites or deletes a local edit it has not synced.** If the server changed too, your edit becomes a conflict for the desktop plugin to resolve.
- **It fails closed.** Anything it doesn't recognise stops the sync instead of being guessed at, for example an unknown database version, encryption, or settings that changed since setup.
- **Deletes are LiveSync's soft deletes,** and only for files that were in sync. A pass that would delete more than half of what it tracks stops and asks.
- **It stays out of `.obsidian/` and every other dot-folder.**

## Status and limits

Working: pull-only and two-way sync, the Android app and the CLI, tested against the official CLI and on a Samsung S22+ (Android 16) syncing a real 10,000-file vault.

Not yet:
- end-to-end encryption and path obfuscation; such databases are refused;
- setup import (QR or setup URI);
- merging conflicts on the device: they are left for the desktop plugin;
- Hidden File Sync and Customisation Sync (`.obsidian/`);
- release builds and an app-store listing.

This is an independent client, not affiliated with the Self-hosted LiveSync project.

## Reference

<details>
<summary>What the engine does, in detail</summary>

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

</details>

<details>
<summary>Full <code>lsync</code> usage, environment and exit codes</summary>

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

</details>

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
