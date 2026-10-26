# livesync-android

A native client for [Self-hosted LiveSync](https://github.com/vrtmrz/obsidian-livesync): a Rust sync engine that talks to the LiveSync CouchDB database directly and keeps a plain folder (an Obsidian vault) in step with it. The engine becomes the core of an always-on Android app. For now it ships as the `lsync` command-line tool for Linux and for Termux on Android.

**Status: milestone M1, read-only.** `lsync` pulls from CouchDB and never writes to it. It does not register a node in the milestone, create the salt or upload anything. Two-way sync is M2.

## What it does

- Follows the database's `_changes` feed with a `_selector` that leaves out chunks, then fetches chunks on demand through `POST _all_docs {keys}`.
- Reassembles `plain` (text) and `newnote` (binary) files. It decompresses chunks when needed, reads legacy eden chunks, and writes each file through a hidden temp file plus rename.
- Keeps a small SQLite **sync index**. It records one row per path: the revision on disk, its chunk IDs, and the size, mtime and xxh64 hash of the bytes. It also stores the changes sequence, the remote identity and the PREFERRED tweaks adopted at setup.
- **Never overwrites or deletes a local file that changed since it was synced.** Such files are reported as `dirty`.
- Skips the following:
  - hidden-file and customisation documents (`i:`, `ix:`, `ps:`) and every dot-path (`.obsidian/`, `.trash/`, dotfiles);
  - `redflag*.md`, `flag_*.md` and `livesync_log_*` at the vault root;
  - anything matching the local `syncIgnoreRegEx` (upstream syntax: entries joined by `|[]|`, `!!` negates, spaces and newlines are dropped);
  - files over `syncMaxSizeInMB`;
  - names the vault's filesystem cannot hold. On Android shared storage (`/storage/...`) that means any of `" * : < > ? \ |` or a control character, checked up front; elsewhere, names the filesystem rejects. They are marked `skipped` ("unwritable name"), shown by `status` and never retried.
- **Handles errors one file at a time.** A local I/O error on one file (unreadable, a file where a folder should be) defers that file to the retry queue; the rest of the batch and the changes sequence move on.
- Creates an empty `.nomedia` at the vault root, so MediaStore and Google Photos leave the vault's images alone.
- Allows one process per state directory and per vault (lock files `lsync.lock` in the state directory and `.lsync.lock` at the vault root).
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
lsync pull                  # one-shot pull into the vault folder
lsync daemon --pull-only    # follow changes (longpoll + heartbeat, jittered reconnect)
lsync status                # index summary, last gate result, dirty/conflicted/pending files
lsync probe <vault>         # on-phone check: Obsidian vs outside writes, inotify vs Obsidian saves
                            # (refuses vaults where the LiveSync plugin is enabled)
```

Every setting can also come from flags or `LSYNC_*` environment variables, for example `LSYNC_URL`, `LSYNC_DB`, `LSYNC_USER`, `LSYNC_PASSWORD`, `LSYNC_VAULT`, `LSYNC_STATE_DIR` and `LSYNC_IGNORE_REGEX`. Flags win over the environment, and the environment wins over the config file. The index lives in `$XDG_STATE_HOME/lsync/<profile>/index.sqlite` unless `--state-dir` says otherwise.

Exit codes:

- 0: OK.
- 1: error.
- 2: refused by a gate.
- 3: pull finished, but some files are dirty or pending.
- 4: probe failed or was inconclusive.

## Layout

| Path | What |
|---|---|
| `crates/livesync-model` | Pure model: doc types, path→ID, chunk IDs, decompression codec, gate evaluation |
| `crates/livesync-remote` | CouchDB over HTTP (reqwest + rustls with bundled webpki roots): `_changes`, `_all_docs`, `_local` docs, backoff |
| `crates/livesync-engine` | Pull loop, daemon, SQLite index, vault file operations |
| `crates/lsync` | The CLI, including `probe` |
| `tools/golden` | Node program pinned to `@vrtmrz/livesync-commonlib` 0.2.1; it emits the fixtures in `fixtures/golden` |
| `scripts/` | Android build, dev-database environment, and a vault comparison script |

Build and test instructions are in [AGENTS.md](AGENTS.md).
