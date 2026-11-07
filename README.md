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
4. Choose the folders to sync: a passing test lists the server's top-level folders with their file counts and sizes (on mobile data only when you tap "List the folders on the server", since it reads about 1 KB per file); tick them, or type a path.
5. Save. Once the first sync finishes, open the folder in Obsidian ("Open folder as vault") and leave the LiveSync plugin off there.

The home screen lists every vault with one line of status; "Add vault" sets up another one the same way. Vault folders may not be the same or inside each other, but two vaults may sync different folders of the same database: each is its own device to the other LiveSync clients. One notification summarises all of them, and removing a vault stops only that one.

### `lsync`, the command-line client

The same engine as a single binary for Linux (and Termux on Android): one-shot pulls and syncs, or a long-running daemon for a server or a desktop without Obsidian.

```sh
lsync account add home --url https://couch.example.net --user me   # asks for the password
lsync vault add notes --db vault --folder ~/Notes
lsync connect            # check that the database is compatible
lsync daemon             # two-way, continuously
lsync daemon --pull-only # follow the server, never write to it
```

With more than one vault, name the one to act on: `lsync --vault notes sync`.

Full usage is under [Reference](#reference).

## How it compares

Measured on a Linux desktop (i9-10900K, btrfs on NVMe, Linux), on clones of one snapshot of a real 425,000-document database (about 2,200 files in scope after the phone's folder excludes), with lsync's release build, the official LiveSync CLI 1.0.40 and Node 26. Medians of 3 runs for lsync and 1–2 for the CLI.

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
- **Deletes are LiveSync's soft deletes,** and only for files that were in sync. A pass that would delete more than 20 files with no known destination, when they are also more than half of what it tracks, stops and asks; so does one that finds the synced folders empty and would delete 3 or more. A move into `.trash/` or a rename never counts (as long as the file is still there under its new name), and smaller deletions go through without asking.
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
- **Deletes are soft deletes** (`deleted: true`) against the synced revision, only for files that were in sync. A move into `.trash/` is Obsidian's delete; a move out of the synced folders (or into another dot-folder) is not a delete. A pass that would delete more than 20 files with no known destination (`MASS_DELETE_MIN`), when they are also more than half of the tracked files, stops and asks instead, as does one that finds the synced folders empty and would delete 3 or more.
- Watches the vault (one watch per folder, never `.obsidian/`), waits until a file has been stable for 2 s, and rescans every 15 minutes in case an event was missed.
- **The daemon judges a burst of deletes as one:** a delete with no known destination goes out only once no other file has gone missing for 10 s (or after 2 minutes of steady deleting), so a burst the watcher spreads over several passes still meets the mass-delete stop. Steady deleting that goes on past the 2 minutes stays one burst until a 10 s pause: the deletes already sent count with the ones still missing, so a slow wipe stops before more than half of the files are gone. Allowing a stopped deletion covers the files that stop listed, and only if the stop is still the one shown when you allow it; files deleted after it are checked again.
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
- Allows one engine per state directory and per vault folder, whichever profile or process it comes from. The state directory is locked with `flock` on `lsync.lock`. The vault folder is locked with `flock` on `.lsync.lock` at its root and with a host lock (an abstract Unix socket on Linux and Android, a flock in `/var/tmp` on macOS), because flock does not work on Android's shared storage. The kernel releases both when the process dies, so a crash, a force-stop or a reboot never leaves the vault locked. A second engine refuses to start and names the holder (process, pid, uid) from `.lsync.holder`, a small record next to the lock file; `lsync status` shows which locks were taken. The host lock covers one machine: two machines sharing a vault over a network filesystem are kept apart only by flock, where that filesystem supports it. Deleting the macOS lock files in `/var/tmp` while an engine runs lets a second one in.
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
<summary>Accounts, vaults and passwords (<code>lsync.toml</code>)</summary>

One file, `$XDG_CONFIG_HOME/lsync/lsync.toml` (`~/.config/lsync/lsync.toml`), holds every account and vault of the machine. An **account** is a server, a user and where its password is kept; a **vault** is an account, a database, a folder and its options. Several vaults may use one account (the password is entered once), and two vaults may sync one database with different folder scopes: each is its own device to the other LiveSync clients.

```toml
[account.home]
url = "https://couch.example.net"
username = "me"
secret = "file:home"            # where the password is (below); never the password itself

[vault.notes]
account = "home"
database = "vault"
folder = "~/Notes"
exclude = ["Archive"]           # or include = [...]; neither: every folder

[vault.archive]                 # nested in notes: another database, excluded by notes' scope
account = "home"
database = "archive"
folder = "~/Notes/Archive"
mode = "pull-only"              # default two-way
```

A vault also takes `state_dir` (default `$XDG_STATE_HOME/lsync/<vault>`), `device_name`, `ignore_regex` (upstream `syncIgnoreRegEx`), `enabled` (default true; `false` keeps it configured but out of the service), `max_size_mb` and `concurrency`. Anything else, including a password, is refused with a message rather than ignored.

```sh
lsync account add home --url https://couch.example.net --user me   # asks for the password
printf '%s' "$PASSWORD" | lsync account add home --url ... --user me --password-stdin
lsync account add ci --url ... --user me --secret env:LSYNC_CI_PASSWORD   # read from the environment
lsync account ls [--json]       # every account, where its password is kept, whether it is set
lsync account passwd home       # asks; or --password-stdin
lsync account rm home           # only when no vault uses it

lsync vault add notes --account home --db vault --folder ~/Notes [--exclude Archive] [--mode pull-only]
lsync vault ls [--json]
lsync vault edit notes --include Notes,Projects --device-name desk   # --unset KEY clears a setting
lsync vault disable notes       # and enable
lsync vault rm notes            # the folder and the index are kept

lsync --vault notes sync        # --profile is the same; with one vault there is nothing to name
```

These commands edit the file in place and keep its comments and layout; a change that would make the file invalid is refused and nothing is written. They take their settings from the command line only, never from `LSYNC_*` variables, and never a password from the command line.

**Folder rules.** No two vaults share a folder or a state folder. A vault folder inside another one is allowed only when it syncs a database of its own and the outer vault's scope excludes it; the refusal says what to add to which `exclude`.

**Passwords** never go in `lsync.toml`, in logs or in error messages. An account's `secret` names its store:

| `secret` | Where the password is |
|---|---|
| `file:KEY` (the default, `file:<account>`) | `secrets.toml` beside `lsync.toml`, mode 0600, `"KEY" = "password"` per line; refused while others can read it |
| `credential:NAME` | a systemd credential: the file `NAME` in `$CREDENTIALS_DIRECTORY` (`LoadCredential=` or `LoadCredentialEncrypted=` in the unit) |
| `env:VAR` | the environment variable `VAR` |

`--password` and `LSYNC_PASSWORD` still override the store for one run. The macOS Keychain and the Linux Secret Service come later as more stores.

**Selecting a vault.** `--vault NAME` (or `--profile NAME`, or `LSYNC_PROFILE`) picks a vault. Without one, a vault named `default` is used, or else the only vault; with several, the command asks for `--vault`. When a run gives its own folder (`--folder` or `LSYNC_VAULT`) without naming a vault, it uses only flags and the environment, never another vault's index; a vault named `default` stands in for the old default profile and is still merged with them. `--vault` used to name the folder: a value that is clearly a path (with a `/`, or starting with `~` or `.`) is still read as `--folder`, with a warning.

**From profile files.** Before `lsync.toml`, each profile was its own `~/.config/lsync/<profile>.toml` with the password inside. On the first run without an `lsync.toml`, they are migrated: one account per server, user and password, one vault per profile with the profile's name, and the passwords into `secrets.toml`. A vault keeps its profile's state folder (the default one has the same path), so its index and node ID stay and nothing is fetched again. The profile files are left as they were, so an older `lsync` still runs from them, but they still hold their passwords: delete them once you no longer need them. A profile that cannot become a vault (incomplete, or overlapping another one) is left, reported, and still read with `--profile NAME`. `--config FILE` reads a profile file as before, and never migrates it. `lsync configure` still works: on a vault in `lsync.toml` it changes that vault (and its account, and the password in the account's store); on a profile file, that file.

</details>

<details>
<summary>Full <code>lsync</code> usage, environment and exit codes</summary>

```sh
lsync connect               # run the compatibility gate and print what it found
lsync pull                  # one-shot pull into the vault folder (read-only)
lsync sync                  # one-shot two-way sync: pull, register, push local changes
lsync daemon                # two-way, continuously (watcher + longpoll); kill -USR1 = "sync now"
lsync daemon --pull-only    # follow changes, read-only
lsync fetch                 # start over from the remote (upstream's "Fetch"), clears a lock
lsync status                # index summary, last gate result, files needing attention

lsync folders               # the server's top-level folders (files, size) against the folder scope;
                            # reads file metadata only, never chunks, and never writes
lsync vault edit notes --include Notes,Projects   # sync only these folders
lsync vault edit notes --exclude Private          # sync everything except these
lsync vault edit notes --all-folders              # back to everything
lsync probe <vault>         # on-phone check: Obsidian vs outside writes, inotify vs Obsidian saves
                            # (refuses vaults where the LiveSync plugin is enabled)
```

Every setting can also come from flags or `LSYNC_*` environment variables, for example `LSYNC_URL`, `LSYNC_DB`, `LSYNC_USER`, `LSYNC_PASSWORD`, `LSYNC_VAULT` (the vault folder, `--folder`), `LSYNC_STATE_DIR`, `LSYNC_IGNORE_REGEX`, `LSYNC_INCLUDE`, `LSYNC_EXCLUDE` and `LSYNC_DEVICE_NAME`. Flags win over the environment, and the environment wins over the config file, so `lsync` also runs with no config file at all. The index lives in `$XDG_STATE_HOME/lsync/<vault>/index.sqlite` unless `--state-dir` says otherwise.

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
| `crates/livesync-host` | Hosting vaults for every front-end: one supervisor per vault (retries from 15 s to 15 min), accounts and vaults (`lsync.toml`), the secret stores, migration from profile files |
| `crates/lsync` | The CLI, including `probe` and the account and vault commands |
| `crates/livesync-ffi` | UniFFI surface for the app over `livesync-host`: one engine per profile, connect test, status, logs |
| `crates/uniffi-bindgen` | UniFFI's Kotlin binding generator, pinned to the same version |
| `android/` | The Android app (Kotlin, Compose): foreground service, onboarding, vault list, status screens |
| `tools/golden` | Node program pinned to `@vrtmrz/livesync-commonlib` 0.2.1; it emits the fixtures in `fixtures/golden` |
| `scripts/` | APK and Termux builds, dev-database environment, a vault comparison script, and `interop.sh` (the M2 scenarios against the official CLI) |

Build and test instructions are in [AGENTS.md](AGENTS.md).

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT licence](LICENSE-MIT), at your option. Parts ported from Self-hosted LiveSync keep its MIT notice; see [NOTICE](NOTICE).

Unless you explicitly state otherwise, any contribution you submit for inclusion in this project, as defined in the Apache-2.0 licence, is dual-licensed as above, without any additional terms or conditions.

