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
lsync serve              # every vault, two-way, continuously, in one process
lsync daemon             # one vault, two-way, continuously
lsync daemon --pull-only # follow the server, never write to it
```

With more than one vault, name the one to act on: `lsync --vault notes sync`. While `lsync serve` runs, the other commands go through it (below), so `lsync sync`, `lsync status` and the rest work the same with or without the service.

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
- **It notices when the plugin moves on (the upstream guard).** Before every write session (at start, after every reconnect, and every 5 minutes while running) it reads the database's version document, `used_features` and the milestone's tweaks. A newer generation, a feature or a setting it doesn't know, or a setting that changed, stops that vault, and only that vault, with a message naming what changed and the newest plugin version known to work; it resumes by itself once the remote is compatible again. [`COMPATIBILITY.md`](COMPATIBILITY.md) lists the tested plugin and CLI versions, what each gate checks and what to do when one trips. A weekly CI job runs the interop scenarios against the latest upstream release.
- **It doesn't share a vault with the plugin (the plugin guard).** Two-way sync refuses an Obsidian vault (a folder with `.obsidian/`) whose `.obsidian/community-plugins.json` enables Self-hosted LiveSync: two clients would write the same folder and database. `--allow-plugin` (or `allow_plugin = true` for the vault) is for a deliberate handover. Plain folders are not affected.
- **Deletes are LiveSync's soft deletes,** and only for files that were in sync. A pass that would delete more than 20 files with no known destination, when they are also more than half of what it tracks, stops and asks; so does one that finds the synced folders empty and would delete 3 or more. A move into `.trash/` or a rename never counts (as long as the file is still there under its new name), and smaller deletions go through without asking.
- **A file deleted on another device goes to the trash,** `<vault>/.trash/` by default, with its folders kept, so a wrong delete can be undone. Only files that still match their synced version are moved.
- **It stays out of `.obsidian/` and every other dot-folder,** unless you allowlist one (never `.obsidian/`).

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
- **Deletes are soft deletes** (`deleted: true`) against the synced revision, only for files that were in sync. A remote delete moves the local file into the trash (see "What syncs" below). A move into `.trash/` is Obsidian's delete; a move out of the synced folders (or into another dot-folder) is not a delete. A pass that would delete more than 20 files with no known destination (`MASS_DELETE_MIN`), when they are also more than half of the tracked files, stops and asks instead, as does one that finds the synced folders empty and would delete 3 or more.
- Watches the vault (one watch per folder, never `.obsidian/` nor a folder an ignore file ignores as a whole), waits until a file has been stable for 2 s, and rescans every 15 minutes in case an event was missed.
- **Notices a dead connection quickly.** The longpoll asks CouchDB for a heartbeat every 10 s and reconnects after 25 s without one, so a link that died in sleep or a Wi-Fi switch is noticed within 25 s. When the wall clock jumps 10 s or more past the monotonic clock, the machine slept: it drops its connections, reconnects and rescans the vault at once. The Android app keeps a 30 s heartbeat and a 90 s timeout (each heartbeat wakes the radio) and relies on the system's network callbacks instead of the clock check. Each reconnect runs the gates again. All three are per-vault settings (below).
- **The daemon judges a burst of deletes as one:** a delete with no known destination goes out only once no other file has gone missing for 10 s (or after 2 minutes of steady deleting), so a burst the watcher spreads over several passes still meets the mass-delete stop. Steady deleting that goes on past the 2 minutes stays one burst until a 10 s pause: the deletes already sent count with the ones still missing, so a slow wipe stops before more than half of the files are gone. Allowing a stopped deletion covers the files that stop listed, and only if the stop is still the one shown when you allow it; files deleted after it are checked again.
- **Folder scope:** all folders by default, or only some (`--include`), or all but some (`--exclude`), matched case-insensitively. Files outside the scope are invisible in both directions. Widening fetches the newly visible documents; narrowing stops tracking without touching the remote.
- Registers itself in the milestone (`node_info`, `tweak_values`, `node_chunk_info`) after a complete fetch, and marks itself resolved after a fresh start or a rebuild elsewhere, so a rebuilt and locked database is recovered from automatically.
- Reassembles `plain` (text) and `newnote` (binary) files. It decompresses chunks when needed, reads legacy eden chunks, and writes each file through a hidden temp file plus rename.
- Keeps a small SQLite **sync index**. It records one row per path: the revision on disk, its chunk IDs, and the size, mtime and xxh64 hash of the bytes. It also stores the changes sequence, the remote identity and the PREFERRED tweaks adopted at setup.
- **Never overwrites or deletes a local file that changed since it was synced.** Such files are reported as `dirty`.
- Skips the following:
  - hidden-file and customisation documents (`i:`, `ix:`, `ps:`) and every dot-path (`.obsidian/`, `.trash/`, dotfiles) not in `sync_dot_paths`;
  - `redflag*.md`, `flag_*.md` and `livesync_log_*` at the vault root;
  - anything matching the local `syncIgnoreRegEx`, or not matching a set `syncOnlyRegEx` (upstream syntax: entries joined by `|[]|`, `!!` negates, spaces and newlines are dropped);
  - anything an ignore file ignores, when ignore files are on;
  - files over `syncMaxSizeInMB`;
  - names the vault's filesystem cannot hold. The rules (forbidden characters, case and Unicode folding, name length) are probed on the vault's filesystem when it is opened, so Android's shared storage, an exFAT stick and ext4 each get their own. Such names are marked `skipped` ("unwritable name"), shown by `status` and never retried;
  - a second document whose name folds to the same file on a case-insensitive filesystem ("case collision with …").
- **Handles errors one file at a time.** A local I/O error on one file (unreadable, a file where a folder should be) defers that file to the retry queue; the rest of the batch and the changes sequence move on.
- On Android, creates an empty `.nomedia` at the vault root, so MediaStore and Google Photos leave the vault's images alone.
- `lsync status` says whether the folder is an Obsidian vault (it has `.obsidian/`) or a plain folder of notes.
- Allows one engine per state directory and per vault folder, whichever profile or process it comes from. The state directory is locked with `flock` on `lsync.lock`. The vault folder is locked with `flock` on `.lsync.lock` at its root and with a host lock (an abstract Unix socket on Linux and Android, a flock in `/var/tmp` on macOS), because flock does not work on Android's shared storage. The kernel releases both when the process dies, so a crash, a force-stop or a reboot never leaves the vault locked. A second engine refuses to start and names the holder (process, pid, uid) from `.lsync.holder`, a small record next to the lock file; `lsync status` shows which locks were taken. The host lock covers one machine: two machines sharing a vault over a network filesystem are kept apart only by flock, where that filesystem supports it. Deleting the macOS lock files in `/var/tmp` while an engine runs lets a second one in.
- Catches a local edit that lands while a remote change is being written (the old file is held open across the swap and checked again), and puts it back as a local edit.
- **Fails closed.** It refuses to sync when it meets any of these:
  - a server that is not CouchDB;
  - a version document that is missing, above 13 or invalid, or that has unknown `used_features`;
  - a missing milestone, or one that is locked and doesn't list us;
  - incompatible chunk-version ranges;
  - encryption, path obfuscation, a `hashAlg` other than xxhash64, or `pathNormalizationVersion=1`;
  - PREFERRED tweaks that changed since setup, or that it doesn't know (a setting a newer plugin added), until adopted with `lsync connect --adopt-tweaks`;
  - encrypted content;
  - an index that belongs to another remote or another vault folder;
  - for two-way sync, an Obsidian vault with the Self-hosted LiveSync plugin enabled (unless allowed).

  `lsync status` shows a stop caused by the remote moving on as `UPSTREAM STOP` (`upstream_stop` in `--json`); see [`COMPATIBILITY.md`](COMPATIBILITY.md).
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

A vault also takes `state_dir` (default `$XDG_STATE_HOME/lsync/<vault>`), `device_name`, `ignore_regex` (upstream `syncIgnoreRegEx`), `sync_only_regex` (upstream `syncOnlyRegEx`), `use_ignore_files` and `ignore_files`, `sync_dot_paths`, `trash` (all under "What syncs" below), `enabled` (default true; `false` keeps it configured but out of the service), `max_size_mb` and `concurrency`. Anything else, including a password, is refused with a message rather than ignored.

The guards and the connection take these (in seconds; edit them in the file):

| Key | Default | What |
|---|---|---|
| `allow_plugin` | `false` | sync two-way although the vault's Self-hosted LiveSync plugin is enabled (a handover) |
| `regate_secs` | 300 | how often the running daemon checks the remote's version, features and tweaks again |
| `heartbeat_secs` | 10 (Android 30) | the longpoll's heartbeat |
| `longpoll_timeout_secs` | 25 (Android 90; 2.5 heartbeats when only the heartbeat is set) | silence before the connection is taken for dead; must be longer than the heartbeat |
| `clock_jump_secs` | 10 (Android: off) | a wall-clock jump this long past the monotonic clock is sleep: reconnect and rescan; `0` turns it off |

A reverse proxy that buffers responses holds the heartbeats back, and the daemon then reconnects every `longpoll_timeout_secs`. Turn buffering off for `/_changes` (nginx: `proxy_buffering off`), or raise the timeout.

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

An account without `--user` is a server without authentication: it has no password and is not asked for one. The server URL holds no user or password (`https://me:pw@...` is refused: give `--user`, and the password when asked). `account add` never takes over a `file:` key that another account uses or that already holds a password, unless given `--replace-secret`.

`--password` and `LSYNC_PASSWORD` still override the store for one run. The macOS Keychain and the Linux Secret Service come later as more stores.

**Selecting a vault.** `--vault NAME` (or `--profile NAME`, or `LSYNC_PROFILE`) picks a vault. Without one, a vault named `default` is used, or else a `default.toml` that was not migrated, or else the only vault; with several, the command asks for `--vault`. When a run gives its own folder (`--folder` or `LSYNC_VAULT`) without naming a vault, it uses only flags and the environment, never another vault's index; a vault named `default` stands in for the old default profile and is still merged with them. `--vault` used to name the folder: a value that is clearly a path (with a `/`, or starting with `~` or `.`) is still read as `--folder`, with a warning.

**From profile files.** Before `lsync.toml`, each profile was its own `~/.config/lsync/<profile>.toml` with the password inside. On the first run without an `lsync.toml`, they are migrated: one account per server, user and password, one vault per profile with the profile's name, and the passwords into `secrets.toml` (a user and password inside a profile's URL move there too). A vault keeps its profile's state folder (the default one has the same path), so its index and node ID stay and nothing is fetched again. The profile files are left as they were, so an older `lsync` still runs from them, but they still hold their passwords: delete them once you no longer need them. A profile that cannot become a vault (incomplete, or overlapping another one) is left, reported once, listed at the top of `lsync.toml` and by `lsync vault ls`, and still read as before: with `--profile NAME`, and `default.toml` when no vault is named (unless there is a `[vault.default]`). `[migration]` in `lsync.toml` records which profiles were migrated and which were left, so a migrated `default.toml` whose vault was later removed is not read again. When none can, `lsync.toml` is written with no vaults, so the migration is not tried again on every run; move it away to try again. `--config FILE` reads a profile file as before, and never migrates it. `lsync configure` still works: on a vault in `lsync.toml` it changes that vault (and its account, and the password in the account's store); on a profile file, that file.

</details>

<details>
<summary>The service, its control socket and routing</summary>

`lsync serve` runs every enabled vault in `lsync.toml` in one process: one runtime, one log, one control socket, and one HTTP connection pool per server, shared by the vaults on it. Each vault has its own engine, index, locks, node ID and status, and a failing vault is retried (from 15 s to 15 minutes) without stopping the others. `lsync serve --vault NAME` runs one vault, for a container or a unit per vault.

```sh
lsync serve                   # every enabled vault; logs to stderr
lsync status --all [--json]   # every vault of the service: state, mode, last pull
lsync events [--json]         # follow what the service does (Ctrl-C to stop)
lsync pause                   # stop every vault's engine (or --vault NAME); resume starts them again
lsync resume
lsync sync-now                # reconnect and rescan now (every vault, or --vault NAME)
lsync reload                  # reread lsync.toml; only the vaults that changed restart
```

- **Reload.** `kill -HUP`, `lsync reload`, and every `lsync account ...`, `lsync vault ...` or `lsync configure` that changes the file make the service reread it. Only the vaults whose settings changed (an account's URL, user or password included) are started, stopped or restarted; the others keep running, also while the reload waits for a command running on one that changed. A file that cannot be read changes nothing. With `--vault NAME`, a vault removed from the file or disabled is stopped (it must be there when the service starts). `kill -USR1` is "sync now" for every vault; SIGTERM and SIGINT stop the service.
- **Pause** stops a vault's engine and lets go of its folders until `resume`. A paused vault takes changes from a reload and starts with them at `resume`. Pausing is not kept across a restart of the service.
- **`lsync daemon`** runs one vault, from `lsync.toml`, a profile file or flags and `LSYNC_*` alone, as before (`lsync --profile bench daemon` included). It fails at once when the vault cannot start; after that it is supervised like the service's vaults (retried instead of exiting). It takes the control socket when no other service holds it, and runs without one otherwise.

**Routing.** A command for a vault the service runs is run by the service, which holds the vault's locks; otherwise the CLI opens the engine itself, one-shot, as before. Either way the output and the exit code are the same. A vault is the service's when the service runs one with the same state folder or the same vault folder.

| Command | While the service runs the vault |
|---|---|
| `status` | Read from the service (the running engine's status). |
| `sync`, `fetch`, `pull`, `connect` | The service stops the vault's engine, runs the command on an engine of its own with the vault's settings (and the command's `--allow-mass-delete`, `--remove-out-of-scope-copies`, `--adopt-tweaks`), and starts the vault's engine again, unless it was paused. Settings given only to this command (flags, `LSYNC_*`, a profile file that says otherwise) would not apply, so such a command is refused with what differs and what to do; for a paused vault it runs here instead, with its settings. The password is not compared: the service uses its own. |
| `daemon` | Refused: the service already runs the vault. |
| `folders`, `probe`, `account`, `vault`, `configure` | Run here (they don't open the vault's engine); the edits tell the service to reload. |
| `serve` | Refused while another service holds the control socket's folder. |

`pause`, `resume`, `sync-now`, `reload`, `events` and `status --all` need a running service; they name a vault with `--vault` (or `--profile`) on the command line, never from `LSYNC_PROFILE`, and act on every vault without one.

**Where the socket is.** `$LSYNC_RUNTIME_DIR/control.sock` when that is set; else `$XDG_RUNTIME_DIR/lsync/control.sock` (Linux) or `~/Library/Application Support/lsync/control.sock` (macOS) for a user's service, and `$RUNTIME_DIRECTORY` or `/run/lsync/control.sock` for `lsync serve --system`. Clients look at `$LSYNC_RUNTIME_DIR` alone when it is set, else at the user's socket and then the system's. It is a Unix socket and never listens on the network; a remote client goes through SSH (`ssh server lsync status --all --json`).

**Who may connect.** Before anything is bound, the socket's folder must be a real folder (not a link) owned by the service's user; it is made 0700, and a wider folder is tightened only when it holds nothing but the socket and its lock. The socket is 0600. Every connection's peer is checked (`SO_PEERCRED` on Linux, `getpeereid` on macOS) and must be the service's own user; `lsync serve --system --socket-group GROUP` also lets in that group's members (folder 0750 and socket 0660, owned by the group). Anyone else gets one `forbidden` line and is disconnected. The service holds an flock on `control.lock` beside the socket while it runs: a second service is refused and told which process holds it, and a socket left by a crashed service is replaced. A client uses a socket only in a folder no one but its owner can write to, owned by the client's own user (another owner only for the system folder `/run/lsync`), and only when the process behind it runs as that owner. A socket the client cannot connect to (none, a stale one, no permission, as a system service's folder is to users outside its group) is no service: it tries the next one, and the CLI then runs the command itself, which the engine's locks keep off a vault a service holds. A service that is there but turns the user away (`forbidden`) is an error that says so.

**Protocol.** Newline-delimited JSON over the socket, one object per line, version 1. The shapes below are stable within a major version: methods, parameters, result fields, event kinds and error codes are only added, never renamed or removed. Clients ignore what they don't know.

```json
{"v":1,"id":1,"method":"status","params":{"vault":"notes"}}
{"v":1,"id":1,"result":{"vaults":[{"name":"notes","state":"running", ...}]}}
{"v":1,"id":2,"method":"pause","params":{"vault":"nope"}}
{"v":1,"id":2,"error":{"code":"unknown_vault","message":"the service runs no vault named \"nope\" (it has: notes)"}}
{"v":1,"id":3,"method":"events"}
{"v":1,"id":3,"result":{"subscribed":true}}
{"v":1,"event":"pulled","vault":"notes","ts":1730000000000,"data":{"written":1,"deleted":0, ...}}
```

- A request has `v` (the major version; 1 when left out), an `id` of the client's choosing (echoed in the response), a `method` and optional `params`. Requests on one connection run concurrently, so match responses by `id`. A request for another major version gets `unsupported_version`.
- A response has `result` or `error` (`code` and `message`).
- After `events`, the connection also carries event lines: they have `event` instead of `id`, the `vault` (or `null` for the service as a whole), `ts` (milliseconds since the epoch) and `data`.
- A client reads a missing field as its default (an older service lacks the fields added since), and calls `hello` first: when `protocol` is not its own, or a method it needs is not in `methods`, it says whether the service or the client is the older one. The CLI waits for a command as long as it runs, sending a `hello` on the same connection every 20 s, and gives up when one goes unanswered for 20 s or the connection closes. A command that fails inside the service, even by a bug, is answered with `failed`, and the vault is restored.

| Method | Params | Result |
|---|---|---|
| `hello` | | `protocol`, `version`, `pid`, `methods`, `config` (the file the service reads) |
| `status` | `vault`? | `vaults`: per vault `name`, `state`, `message`, `paused`, `enabled`, `account`, `command`, `settings` (URL without credentials, user, database, folders, scope, mode, what syncs: ignore files, regexes, dot-paths, trash; never the password), `status` (as `lsync status --json`), `session` and `push` (this engine's counters), `deletes_waiting`, `mass_delete_allowed`, `fingerprint` (names `settings`; changes when they do) |
| `events` | `vault`? | `subscribed`, then events |
| `sync-now`, `pause`, `resume` | `vault`? (else every vault) | `vaults`: the names acted on |
| `reload` | `wait`? (`false`: return at once with `scheduled`) | `started`, `stopped`, `restarted`, `unchanged`, `failed` (`vault`, `message`) |
| `sync`, `fetch`, `pull`, `connect` | `vault`, and `allow_mass_delete`, `remove_out_of_scope_copies`, `adopt_tweaks`, `fingerprint` (from the `status` whose settings the client compared: when the vault's settings changed since, nothing runs and the error is `changed`) | `report` (as the command's `--json` prints it), `wall_ms`, `peak_rss_kib` (the service's) |

A vault's `state` is `starting`, `running`, `failed` (retrying), `stopped`, `paused`, `busy` (running a command for a client), `disabled` or `invalid` (its settings cannot be used, for example a missing password; `message` says why).

Events: `state` (`state`, `message`), `paused` (`paused`), `pulled` (`written`, `deleted`, `adopted`, `dirty`, `conflicted`, `restored_edits`, `bytes_written`), `pushed` (`pushed`, `created`, `deleted`, `conflicts`, `chunks_uploaded`, `chunk_bytes`), `conflict` (`pulled`, `pushed`), `safety-stop` (`stop`: the text, or `null` once cleared), `error` (`message`), `command` (`command`, `phase`: `started` or `finished`, `ok`, `message`), `reload` (the reload's result) and `lagged` (`missed`: events a slow reader lost). Counts are what happened since the vault's previous event of that kind, read from the engines about twice a second; `status` is the source of truth.

Error codes: `bad_request`, `unsupported_version`, `unknown_method`, `unknown_vault`, `not_running`, `paused`, `busy`, `refused` (a gate refused; the message is the refusal), `invalid_config`, `failed`, `forbidden`, `shutting_down`, `changed`.

From a shell: `printf '%s\n' '{"v":1,"id":1,"method":"status"}' | socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/lsync/control.sock"`.

</details>

<details>
<summary>What syncs: ignore files, the trash, dot-paths</summary>

Per vault, in `lsync.toml` (or a profile file, or flags and `LSYNC_*` variables like the other settings):

```toml
[vault.notes]
# ...
ignore_files = [".gitignore"]     # sets use_ignore_files too
sync_only_regex = "^notes/"       # optional: only matching paths sync
sync_dot_paths = [".tool-notes"]  # dot-folders or dot-files that sync after all
trash = "vault"                   # or "system" or "none"
```

| Setting | Flag / variable | Default | Upstream |
|---|---|---|---|
| `use_ignore_files` | `--use-ignore-files BOOL`, `LSYNC_USE_IGNORE_FILES` | on when `ignore_files` is set, otherwise off | `useIgnoreFiles` (default off) |
| `ignore_files` | `--ignore-files`, `LSYNC_IGNORE_FILES` (comma-separated) | `.gitignore` | `ignoreFiles` |
| `sync_only_regex` | `--sync-only-regex`, `LSYNC_SYNC_ONLY_REGEX` | none | `syncOnlyRegEx` |
| `ignore_regex` | `--ignore-regex`, `LSYNC_IGNORE_REGEX` | none | `syncIgnoreRegEx` |
| `sync_dot_paths` | `--sync-dot-paths`, `LSYNC_SYNC_DOT_PATHS` (comma-separated) | none | (none: upstream stores such paths when a node pushes them) |
| `trash` | `--trash`, `LSYNC_TRASH` | `vault` | `trashInsteadDelete` (on) |

**Ignore files** work as in the LiveSync plugin and the official CLI, so every client agrees on what is in the vault. Ignored paths are not pushed, and remote documents at ignored paths are not written here. The rules are commonlib's, checked against its own code by golden vectors, and **they are not git's rules**:

- For a file, the ignore files named in `ignore_files` are read from its own folder and from every folder above it. The nearest ignore file that mentions the path decides; within one file, the last matching pattern wins.
- A pattern is matched against the path relative to its ignore file's folder, with minimatch (`*`, `?`, `**`, `[a-z]`, `{a,b}`), case-insensitively, dot-files included.
- `!pattern` accepts a path again.
- `dir/` ignores everything below `dir`, and no `!` in that file takes it back.
- Any other pattern also covers everything below a folder it names: `secret` ignores `secret/x.md`, and `!secret/ok.md` accepts that one file again.
- **No match at any depth.** `*.log` in the root `.gitignore` ignores `a.log` but not `notes/a.log`; `node_modules/` ignores `node_modules/` at the root only. Write `**/*.log` or `**/node_modules/` for git's behaviour.
- A pattern starting with `/` matches nothing (it would be an absolute path).
- Paths containing `./` (a folder whose name ends in a dot) are ignored wherever an ignore file exists on the way.

Examples, for a root `.gitignore` of `*.log`, `build/`, `!keep.log` and a `notes/.gitignore` of `drafts/`:

| Path | Synced? | Why |
|---|---|---|
| `a.log` | no | `.gitignore:1:*.log` |
| `keep.log` | yes | `.gitignore:3:!keep.log` |
| `sub/a.log` | yes | `*.log` matches only beside its file |
| `build/x.md` | no | `.gitignore:2:build/` |
| `sub/build/x.md` | yes | `build/` matches only beside its file |
| `notes/drafts/d.md` | no | `notes/.gitignore:1:drafts/` |

Beyond the plugin:

- **Edits apply while running.** Changing an ignore file takes effect at once in `lsync daemon` and `lsync serve` (the plugin rechecks only when its settings change).
- **A path that becomes ignored** is no longer tracked: its file stays, its remote document stays, and deleting it here deletes nothing remotely. **A path that stops being ignored** is fetched, as when a folder scope widens.
- **Fails closed.** An ignore file that cannot be read, or that uses a pattern lsync does not evaluate (extglobs like `+(a|b)`, POSIX classes like `[[:alpha:]]`), stops everything in its folder and below: nothing there is pushed, written, untracked or treated as deleted until it is fixed. `lsync status` lists the folder, and a running daemon or service watches it, so the fix takes effect at once. (The plugin skips such a file.)
- **The vault-root ignore file counts.** commonlib asks for it as `/.gitignore`; the plugin reads it, and so does lsync. The official CLI (1.0.40) does not: its storage layer refuses a path with a leading slash, so it pushes what only the root file ignores.
- **Ignored folders are not looked into.** A folder that is surely ignored as a whole (`**/node_modules/`, or a `*` in the folder's own ignore file, as virtualenvs write) is neither scanned nor watched, which keeps code folders cheap. Ignore files inside it are not read, so a `!` rule in one of them is not honoured here, where the plugin would honour it: neither for local files nor for documents arriving from the remote, which stay unwritten. (Writing them would track files that no scan here ever looks at again.)

`lsync check-ignore PATH...` says what decides each path, like `git check-ignore -v`: `source<TAB>path`, where the source is `file:line:pattern` for an ignore file and `[reason]` for the other rules (`dot-path`, `ignore-regex`, `sync-only-regex`, `out-of-scope`, ...). Synced paths are listed with `-n` (`::` when nothing matched, or the `!` rule that accepted them). `--json` gives every path with `synced`, `reason`, `source` and `detail`. It reads only the vault folder, and exits 0 when some path is not synced, 1 when all are.

```sh
$ lsync check-ignore a.log sub/a.log notes/drafts/d.md .obsidian/app.json -n
.gitignore:1:*.log	a.log
::	sub/a.log
notes/.gitignore:1:drafts/	notes/drafts/d.md
[dot-path]	.obsidian/app.json
```

**The trash.** A file deleted on another device is moved, not removed:

- `vault` (default): to `<vault>/.trash/<path>`, keeping its folders, with ` 1`, ` 2`, ... before the extension when that name is taken. `.trash/` is a dot-folder, so it never syncs, and it is where Obsidian puts its own deletes. It works headless.
- `system`: to the freedesktop trash (`~/.local/share/Trash`, or the `.Trash-<uid>` folder of the vault's filesystem when that is another one), or `~/.Trash` on macOS. Where there is none (Android, a macOS vault on another volume), lsync refuses to start and says why.
- `none`: deleted permanently, as the official CLI does.

Only a file that still matches its synced version is moved; an edited one is kept and reported as `dirty`. Folders the move leaves empty are removed, as upstream does, but never the vault folder itself nor a folder holding anything. Our own move is never reported back as a local delete, and it never counts toward the mass-delete stop.

**Dot-paths** stay out of sync, except the ones in `sync_dot_paths`. Each entry names a dot-folder or dot-file itself (`.tool-notes`, `notes/.drafts`); a dot-folder deeper inside it needs an entry of its own. `.obsidian`, `.trash`, `.git` and lsync's own `.lsync*` files can never be listed (settings sync is its own feature).

</details>

<details>
<summary>Full <code>lsync</code> usage, environment and exit codes</summary>

```sh
lsync connect               # run the compatibility gate and print what it found
lsync pull                  # one-shot pull into the vault folder (read-only)
lsync sync                  # one-shot two-way sync: pull, register, push local changes
lsync serve                 # every enabled vault in one process, with the control socket (above)
lsync daemon                # one vault, two-way, continuously (watcher + longpoll); kill -USR1 = "sync now"
lsync daemon --pull-only    # follow changes, read-only
lsync fetch                 # start over from the remote (upstream's "Fetch"), clears a lock
lsync status                # index summary, last gate result, files needing attention
lsync check-ignore PATH...  # which ignore file and rule (or other filter) decides each path
lsync sync --allow-plugin   # (also fetch, daemon) a handover from the plugin in this Obsidian vault
lsync connect --adopt-tweaks  # accept changed or new PREFERRED tweaks after checking them

lsync folders               # the server's top-level folders (files, size) against the folder scope;
                            # reads file metadata only, never chunks, and never writes
lsync vault edit notes --include Notes,Projects   # sync only these folders
lsync vault edit notes --exclude Private          # sync everything except these
lsync vault edit notes --all-folders              # back to everything
lsync probe <vault>         # on-phone check: Obsidian vs outside writes, inotify vs Obsidian saves
                            # (refuses vaults where the LiveSync plugin is enabled)
```

Every setting can also come from flags or `LSYNC_*` environment variables, for example `LSYNC_URL`, `LSYNC_DB`, `LSYNC_USER`, `LSYNC_PASSWORD`, `LSYNC_VAULT` (the vault folder, `--folder`), `LSYNC_STATE_DIR`, `LSYNC_IGNORE_REGEX`, `LSYNC_SYNC_ONLY_REGEX`, `LSYNC_USE_IGNORE_FILES`, `LSYNC_IGNORE_FILES`, `LSYNC_SYNC_DOT_PATHS`, `LSYNC_TRASH`, `LSYNC_INCLUDE`, `LSYNC_EXCLUDE` and `LSYNC_DEVICE_NAME`. Flags win over the environment, and the environment wins over the config file, so `lsync` also runs with no config file at all. The index lives in `$XDG_STATE_HOME/lsync/<vault>/index.sqlite` unless `--state-dir` says otherwise.

Exit codes:

- 0: OK (`check-ignore`: some path is not synced).
- 1: error (`check-ignore`: every path syncs).
- 2: refused by a gate.
- 3: pull or sync finished, but something needs attention (dirty or pending files, local files that cannot sync, a safety stop); for `reload`, a vault that cannot run.
- 4: probe failed or was inconclusive.

</details>

## Layout

| Path | What |
|---|---|
| `crates/livesync-model` | Pure model: doc types, path→ID, chunk IDs, the splitter, decompression codec, gate evaluation, milestone registration, folder scope |
| `crates/livesync-remote` | CouchDB over HTTP (reqwest + rustls with bundled webpki roots): `_changes`, `_all_docs`, `_local` docs, backoff |
| `crates/livesync-engine` | Pull and push loops, daemons, SQLite index, vault file operations; platform seams (`Watcher`, `FsRules`) |
| `crates/livesync-host` | Hosting vaults for every front-end: one supervisor per vault (retries from 15 s to 15 min), accounts and vaults (`lsync.toml`), the secret stores, migration from profile files, the service (reload, pause, commands, events) and the control socket with its protocol and client |
| `crates/lsync` | The CLI, including `serve`, the routing through the control socket, `probe` and the account and vault commands |
| `crates/livesync-ffi` | UniFFI surface for the app over `livesync-host`: one engine per profile, connect test, status, logs |
| `crates/uniffi-bindgen` | UniFFI's Kotlin binding generator, pinned to the same version |
| `android/` | The Android app (Kotlin, Compose): foreground service, onboarding, vault list, status screens |
| `tools/golden` | Node program pinned to `@vrtmrz/livesync-commonlib` 0.2.1; it emits the fixtures in `fixtures/golden` |
| `scripts/` | APK and Termux builds, dev-database environment, a vault comparison script, and `interop.sh` (the M2 scenarios against the official CLI) |

Build and test instructions are in [AGENTS.md](AGENTS.md).

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT licence](LICENSE-MIT), at your option. Parts ported from Self-hosted LiveSync keep its MIT notice; see [NOTICE](NOTICE).

Unless you explicitly state otherwise, any contribution you submit for inclusion in this project, as defined in the Apache-2.0 licence, is dual-licensed as above, without any additional terms or conditions.

