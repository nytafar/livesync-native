# lsync: Self-hosted LiveSync without Obsidian

**A native client for [Self-hosted LiveSync](https://github.com/vrtmrz/obsidian-livesync) that keeps vault folders in sync with your CouchDB on desktops, servers and Android, whether or not Obsidian is open.**

Self-hosted LiveSync syncs Obsidian vaults through your own CouchDB, but only while Obsidian runs: the plugin lives inside the app. It also keeps a full replica of the database (for our vault 1.8 GB and over 300,000 documents) and does its chunking and hashing on the thread you type on.

lsync is a small sync engine in Rust that talks to the same database, wire-compatible with the plugin. It runs on its own as a service and keeps plain folders in sync: a laptop's vault, a server's notes folder that agents and scripts write to, or a phone's vault. Obsidian opens such a folder as an ordinary vault, with the LiveSync plugin turned off on that device. The desktop plugin and the official CLI keep working against the same database.

- **One binary, three roles:** the service (`lsync serve`, every vault in one process), the CLI for people and scripts, and a stand-in for the official LiveSync CLI.
- **Linux, macOS and Termux;** the [Android app](#android-app) shares the engine.
- **Small and fast:** a first sync of 2,200 files in 21 s with 96 MB of memory, and a 2.2 MB index instead of a replica (see [How it compares](#how-it-compares)).
- **Fails closed:** an unknown database format, setting or plugin release stops that vault with a message, instead of being guessed at ([`COMPATIBILITY.md`](COMPATIBILITY.md)).

This is an independent client, not affiliated with the Self-hosted LiveSync project.

## Install

Every release has static binaries for Linux (x86_64 and aarch64, musl), a universal macOS binary and a Termux build, with `SHA256SUMS`, on the [releases page](https://github.com/nytafar/livesync-native/releases).

**Script** (Linux and macOS, into `~/.local/bin`, no sudo): it downloads the archive for this machine, checks it against the release's `SHA256SUMS` and stops on a mismatch.

```sh
curl -fsSLO https://github.com/nytafar/livesync-native/releases/latest/download/install.sh
sh install.sh                         # --version 0.4.0, --prefix DIR or --bin-dir DIR; --help
```

**Arch and Omarchy** (AUR): `yay -S lsync-bin` (or `paru -S lsync-bin`).

**Homebrew** (macOS and Linux): `brew install nytafar/tap/lsync`. `brew services start lsync` then runs the service ([below](#macos)).

**Docker:** `ghcr.io/nytafar/lsync` for amd64 and arm64, [below](#docker).

**By hand:** download `lsync-<version>-<target>.tar.gz` and `SHA256SUMS`, check with `sha256sum -c --ignore-missing SHA256SUMS` (macOS: `shasum -a 256 -c`), and put `lsync` on your `PATH`. On macOS a binary downloaded with a browser is quarantined: `xattr -d com.apple.quarantine lsync` once.

**From source** (Rust stable): `cargo install --locked --git https://github.com/nytafar/livesync-native lsync`, or `cargo build --release -p lsync` in a checkout. Build and test instructions are in [AGENTS.md](AGENTS.md).

## Quick start

```sh
lsync account add home --url https://couch.example.net --user me   # asks for the password
lsync vault add notes --db vault --folder ~/Notes                    # --include/--exclude for some folders
lsync connect                     # check that the database is one lsync can sync
lsync sync                        # optional: the first sync now, in the foreground
lsync service install             # run `lsync serve` at login (systemd user service, or a LaunchAgent)
lsync status --all                # every vault of the service
```

On a server, `lsync service install --linger` starts the user service at boot without a login; for a system service, see [Running on a server](#running-on-a-server).

**Taking over a vault the LiveSync plugin keeps in sync.** Import its settings, turn the plugin off on this machine, check, then take the folder over:

```sh
lsync import ~/Notes/.obsidian/plugins/obsidian-livesync/data.json   # account and vault "notes", added disabled
# In Obsidian on this machine: turn Self-hosted LiveSync off (lsync refuses a vault where it is on).
lsync --vault notes adopt --dry-run   # identical, differing, only here, only on the server
lsync --vault notes adopt             # adopt the identical files without a transfer, then the first sync
lsync vault enable notes              # let the service run it
lsync service install
```

Files that differ from the server's version become conflicts with no common ancestor, so the dry run lists them first. `import` also reads the official CLI's `settings.json`; with end-to-end encryption on, the passphrase goes into the secret store with the password. Settings lsync cannot honour (path obfuscation, encryption other than V2, another kind of remote) stop it with the reason. Details under [What syncs](#reference).

**An end-to-end encrypted database** (the plugin's "End-to-End Encryption", V2, without path obfuscation) needs its passphrase, kept in the secret store like a password:

```sh
lsync vault add notes --db vault --folder ~/Notes --e2ee   # asks for the passphrase (or --passphrase-stdin)
lsync vault passphrase notes                                 # change it later; --remove for a plaintext database
```

A database that a plugin or CLI from 1.0.46 on set up new is refused for now (NFC path identity); databases set up before keep working. [`COMPATIBILITY.md`](COMPATIBILITY.md) has the tested versions and what to do.

## The service

`lsync serve` runs every enabled vault in `lsync.toml` in one process, with one HTTP connection pool per server. A failing vault is retried, from 15 s to 15 minutes, without stopping the others. `lsync service install` runs it under systemd or launchd; `kill -HUP` or `lsync reload` rereads the config, and `lsync account …` and `lsync vault …` tell it to.

**The control socket.** The service listens on a Unix socket, never on the network: `$XDG_RUNTIME_DIR/lsync/control.sock` on Linux, `~/Library/Application Support/lsync/control.sock` on macOS, `/run/lsync/control.sock` for the system service (`$LSYNC_RUNTIME_DIR` overrides). Only the service's own user may connect (checked per connection with `SO_PEERCRED` or `getpeereid`), and for the system service the members of its `--group`. The protocol is newline-delimited JSON, stable within a major version and documented under [Reference](#reference), so a status bar, a script or an agent can use it: `printf '%s\n' '{"v":1,"id":1,"method":"status"}' | socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/lsync/control.sock"`.

**One CLI, two paths.** Every command for a vault asks the socket first. When the service runs that vault, the command runs inside the service, which holds the vault's locks; otherwise the CLI opens the engine itself, one-shot. The output and the exit code are the same either way.

## Commands

| Command | What |
|---|---|
| `lsync sync` | One two-way sync: pull, register this device, push local changes. |
| `lsync pull` | One pull into the folder; never writes to the server. |
| `lsync fetch` | Start over from the server (upstream's "Fetch"), then sync. |
| `lsync flush [PATH…]` | Push now and return once stored on the server, with the revisions. |
| `lsync wait --applied PATH[@REV]`, `--idle` | Block until that revision is in the local file, or nothing is pending. |
| `lsync status [--all] [--json]` | A vault's index, last gate result and files needing attention; `--all`: every vault of the service. |
| `lsync status --follow --waybar` | One [Waybar](https://github.com/Alexays/Waybar) JSON line per change; `--format=swiftbar\|text\|json`. |
| `lsync check-ignore PATH…` | Which ignore file and rule (or other filter) keeps each path out of sync. |
| `lsync allow-deletes` | Confirm a vault's mass-delete stop: the files it listed, and only those, are deleted. |
| `lsync doctor` | Check the config, the secret stores, the vault folders, the watch limit and the service. |
| `lsync import FILE` | The plugin's `data.json` or the official CLI's `settings.json` to an account and a vault. |
| `lsync adopt [--dry-run]` | Take over a folder that is already in sync, before its first sync here. |
| `lsync serve`, `lsync service install\|uninstall\|status\|logs` | The service, and installing it. |
| `lsync account add\|ls\|passwd\|rm`, `lsync vault add\|ls\|edit\|enable\|disable\|rm` | Edit `lsync.toml`, keeping its comments. |
| `lsync pause`, `resume`, `sync-now`, `reload`, `events` | Talk to the running service. |
| `lsync connect`, `folders`, `daemon` | Check the database; list the server's folders; one vault in the foreground. |

With more than one vault, name one: `lsync --vault notes sync`. `flush` and `wait` let an agent on a server write a note and flush it while a script on a laptop waits for exactly that revision. Every option, the environment variables and the exit codes are under [Reference](#reference).

## What syncs

- **Ignore files** (`ignore_files = [".gitignore"]` for a vault) work as in the plugin and the official CLI, so every client agrees on what is in the vault. They are **not git's rules**: `*.log` matches only beside its ignore file, so write `**/*.log` for git's behaviour. Edits apply while running, an unreadable ignore file stops its folder instead of syncing it, and ignored folders such as `node_modules/` are never watched. `lsync check-ignore` explains any path.
- **The trash.** A file deleted on another device is moved to `<vault>/.trash/` with its folders kept (or to the system trash, or deleted: `trash = "system"`, `"none"`). Only a file that still matches its synced version is moved; an edited one is kept.
- **Dot-paths** (`.obsidian/`, other dot-folders and dot-files) stay out, except the ones listed in `sync_dot_paths`; `.obsidian/` never syncs.
- **Folder scope:** all folders, only some (`--include`) or all but some (`--exclude`).

## Safety

- **It never overwrites or deletes a local edit it has not synced.** If the server changed too, your edit becomes a conflict for the desktop plugin to resolve.
- **The upstream guard.** Before every write session (at start, after every reconnect, and every 5 minutes) it reads the database's version document, `used_features` and the milestone's tweaks. A newer generation, an unknown feature or setting, or a setting that changed stops that vault, and only that vault, with a message naming what changed and the newest plugin version known to work. It resumes by itself once the remote is compatible again. A weekly CI job runs the interop scenarios against the latest upstream release ([`COMPATIBILITY.md`](COMPATIBILITY.md)).
- **The plugin guard.** Two-way sync refuses an Obsidian vault whose config folder (`.obsidian/`, or another one a device uses) has Self-hosted LiveSync enabled: two clients would write the same folder and database. `--allow-plugin` (or `allow_plugin = true`) is for a deliberate handover.
- **The mass-delete stop.** A pass that would delete more than 20 files with no known destination, when they are also more than half of what it tracks, stops and asks; so does one that finds the synced folders empty and would delete 3 or more. The service judges a burst of deletes as one, so a slow wipe still meets the stop. `lsync status` shows the stop and `lsync allow-deletes` confirms exactly the files it lists. Moves into `.trash/` and renames never count.
- **Deletes are LiveSync's soft deletes,** and only for files that were in sync.
- **One engine per vault folder and per state folder,** whichever process it comes from; a second one is refused and told who holds it.

## Running on a server

- **A lingering user service** (`lsync service install --linger`) is the simple way: it starts at boot and runs as you.
- **A system service** keeps the config away from the service:

  ```sh
  sudo useradd --system --user-group --create-home --home-dir /var/lib/lsync-home lsync
  sudo lsync --system account add home --url https://couch.example.net --user me --secret credential:lsync-home
  sudo lsync --system vault add notes --account home --db vault --folder /srv/notes
  sudo lsync service install --system --run-as lsync [--group agents]
  lsync --system status --all       # as lsync, or a member of --group
  ```

  - **`--run-as` is required,** and root only when named. The binary must be one only root can change (`/usr/local/bin`, or the AUR package's `/usr/bin`).
  - **The config stays root's:** `/etc/lsync` is 0750 and its files 0640, owned by root with the service's group, so the service reads them and cannot change them. Every edit is `sudo lsync --system …`. That group must be the service user's alone; install refuses a shared one.
  - **Passwords and passphrases:** `credential:lsync-NAME` keeps a password, or a vault's end-to-end encryption passphrase (`sudo lsync --system vault passphrase NAME --secret credential:lsync-NAME`), encrypted with `systemd-creds`, for the service alone; `file:` keeps it in `secrets.toml`, readable by the service's group, and install warns about each one.
  - **The socket group:** with `--group GROUP`, members of that group (agents running as other users) may use the control socket: status, pause, resume, sync-now, events, reload and `lsync --system --vault NAME status|sync|fetch|pull`. They cannot read the config.
  - **Hardening:** `ProtectSystem=strict` with the vault folders as the only `ReadWritePaths=`, `PrivateTmp`, `NoNewPrivileges`, `UMask=0007`; `Type=notify` with a watchdog.

  Full details under [Reference](#reference).

### Docker

The image is `FROM scratch`: the static binary (it bundles its CA roots) and nothing else. It runs `lsync serve` as a system service as uid 10001, with the config in `/config`, the indexes and the control socket in `/state`, and the vault folders mounted where the config says, by convention under `/vaults`. With bind mounts, run it as the folders' owner:

```sh
mkdir -p config state vaults/notes
run="docker run --user $(id -u):$(id -g) -v ./config:/config -v ./state:/state -v ./vaults:/vaults"
$run --rm -it ghcr.io/nytafar/lsync account add home --url https://couch.example.net --user me
$run --rm ghcr.io/nytafar/lsync vault add notes --db vault --folder /vaults/notes
$run -d --name lsync --restart unless-stopped ghcr.io/nytafar/lsync     # lsync serve
docker exec lsync lsync status --all
```

Run the other commands in the running container (`docker exec lsync lsync vault add …`) so the service reloads at once. Without `--user`, the folders must be writable by uid 10001. `pkg/` has the Dockerfile and how the image is built.

## macOS

- `lsync service install` writes the LaunchAgent `~/Library/LaunchAgents/no.nyta.lsync.plist` and loads it into your login session: started at login, restarted after an unsuccessful exit, logging to `~/Library/Logs/lsync/lsync.log` (`lsync service logs`). It is not a `Background` process, which would throttle its I/O and timers.
- **Homebrew:** `brew services start lsync` runs the same service under Homebrew's label, logging to `$(brew --prefix)/var/log/lsync.log`. Use it or `lsync service install`, not both: the second service is refused.
- **Privacy permissions:** a vault under `~/Documents`, `~/Desktop`, `~/Downloads` or iCloud Drive needs Full Disk Access for the `lsync` binary (System Settings, Privacy & Security). A service cannot ask for it. Without it the vault is not started at all, never read as empty, so nothing counts as deleted; the service retries and picks it up once access is granted. `lsync doctor` says when it is missing.
- **Names:** lsync probes each vault's volume for how it folds case and Unicode normalisation (APFS keeps `é` in either form as one file) and matches local names to the server's by exactly that. A file evicted to iCloud Drive (only `.NAME.icloud` left) counts as not checked, never as deleted.
- **Renames:** FSEvents reports the two halves of a rename separately; lsync pairs them when both are the same file (device and inode), so a rename is sent as a rename. A file lsync had not seen since it started can't be paired that way, so its old name is held like any delete for 10 s, and the new name goes up as a new note.
- **Passwords** can go in the Keychain: `lsync account add home … --secret keyring:home`.
- **Menu bar:** `lsync status --follow --format=swiftbar` is a streamable [SwiftBar](https://github.com/swiftbar/SwiftBar) plugin.
- The macOS build passes the whole test suite in CI, but has not yet been field-checked on a Mac with a real vault. Case-insensitive volumes behave as Android's shared storage does: a remote rename that changes only case keeps the old spelling on disk. On a case-sensitive APFS volume, a case-only rename is sent as a rename of the same note.

## Android app

A foreground service keeps one or more vault folders on the phone in sync, whether or not Obsidian is open. Edits from other devices land in under a second, phone edits are pushed about 2 s after Obsidian saves them, and it comes back by itself after a reboot or an app update. It keeps only a small index and fetches content on demand: the first sync of 2,458 files took 62 s. The password is encrypted with a key in the Android Keystore.

Requires Android 11 or later. Each release has a signed APK, `livesync-android-<version>.apk`; to build it yourself, see `scripts/build-apk.sh` in [AGENTS.md](AGENTS.md). Install it, then:

1. Grant all-files access, notifications and the battery exemption on the setup screen.
2. Pick a **new** folder under `Documents/Obsidian/`.
3. Enter your CouchDB URL, database, user and password, then press Test connection.
4. Choose the folders to sync: a passing test lists the server's top-level folders with their file counts and sizes (on mobile data only when you tap "List the folders on the server", since it reads about 1 KB per file); tick them, or type a path.
5. Save. Once the first sync finishes, open the folder in Obsidian ("Open folder as vault") and leave the LiveSync plugin off there.

The home screen lists every vault with one line of status; "Add vault" sets up another one the same way. Each vault can be pull-only (the safe way to start) or two-way. Vault folders may not be the same or inside each other, but two vaults may sync different folders of the same database: each is its own device to the other LiveSync clients. One notification summarises all of them, and removing a vault stops only that one.

**Termux:** the `aarch64-linux-android` archive is the CLI for Termux.

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

## Status and limits

Working: pull-only and two-way sync, the service with its control socket and installers, and the Android app, tested against the official CLI on every scenario and on a Samsung S22+ (Android 16) syncing a real 10,000-file vault. The macOS build passes the test suite in CI.

End-to-end encryption (V2, as the plugin and CLI write it by default) works in every command, desktop only: the Android app refuses an encrypted database and says the desktop CLI syncs it.

Not yet:
- path obfuscation, V1 encryption, encrypted internal metadata and NFC path identity; such databases are refused;
- setup URIs and QR codes (`lsync import` reads the settings files);
- merging conflicts: they are left for the desktop plugin;
- Hidden File Sync and Customisation Sync (`.obsidian/`);
- code signing and notarisation on macOS, and an app-store listing for Android.

## Reference

<details>
<summary>What the engine does, in detail</summary>

- Follows the database's `_changes` feed with a `_selector` that leaves out chunks, then fetches chunks on demand through `POST _all_docs {keys}`.
- **Pushes local changes** the way the official clients store them: the `v3-rabin-karp` splitter ported byte for byte (so chunk IDs match and deduplicate), chunks uploaded before the metadata that names them, metadata written against the revision the file was synced at. If the remote moved on meanwhile, the local edit is stored as a child of that revision instead (a conflict the desktop plugin resolves); nothing is overwritten.
- **Deletes are soft deletes** (`deleted: true`) against the synced revision, only for files that were in sync. A remote delete moves the local file into the trash (see "What syncs" below). A move into `.trash/` is Obsidian's delete; a move out of the synced folders (or into another dot-folder) is not a delete. A pass that would delete more than 20 files with no known destination (`MASS_DELETE_MIN`), when they are also more than half of the tracked files, stops and asks instead, as does one that finds the synced folders empty and would delete 3 or more.
- Watches the vault (one watch per folder, never `.obsidian/` nor a folder an ignore file ignores as a whole), waits until a file has been stable for 2 s, and rescans every 15 minutes in case an event was missed.
- **Notices a dead connection quickly.** The longpoll asks CouchDB for a heartbeat every 10 s and reconnects after 25 s without one, so a link that died in sleep or a Wi-Fi switch is noticed within 25 s. When the wall clock jumps 10 s or more past the monotonic clock, the machine slept: it drops its connections, reconnects and rescans the vault at once. The Android app keeps a 30 s heartbeat (each heartbeat wakes the radio) with a 75 s timeout, and relies on the system's network callbacks instead of the clock check; a network change also drops the pooled connections. Both timeouts stay below the HTTP read timeout (90 s), so a dead longpoll is noticed by its own check. Each reconnect runs the gates again. All three are per-vault settings (below).
- **The daemon judges a burst of deletes as one:** a delete with no known destination goes out only once no other file has gone missing for 10 s (or after 2 minutes of steady deleting), so a burst the watcher spreads over several passes still meets the mass-delete stop. Steady deleting that goes on past the 2 minutes stays one burst until a 10 s pause: the deletes already sent count with the ones still missing, so a slow wipe stops before more than half of the files are gone. Allowing a stopped deletion covers the files that stop listed, and only if the stop is still the one shown when you allow it; files deleted after it are checked again.
- **The stop fails closed when files cannot be checked.** Files under a folder that cannot be read (or that an unreadable ignore file leaves undecided) are never deleted, and when the stop or a burst depends on them they count as still missing: the stop stands and lists them, and an allowed stop waits until it can check them all (`lsync status` and the app say so). The deletes the daemon holds and the burst's count are kept in the index, so a restart neither forgets them nor ends the burst. A one-shot `lsync sync` counts an open burst (the deletes the daemon sent, and the ones it held, which this pass decides) and, when the command began within 10 s of the burst's last delete, adds what it deletes to it; a command that began more than 10 s after it ends the burst, as the daemon's quiet period does. The 10 s are measured to when the command began (for a routed `lsync sync`, before the service stops the daemon), not to when the pass gets to the deletes, so a slow pull or a slow daemon stop does not make a burst look over. So a sync in the middle of a wipe neither forgets the wipe's start nor lets the rest through as a new, smaller burst; the one case it cannot see is a daemon that was killed and a wipe that went on unwatched for more than 10 s before the next command. While some of a burst's files cannot be checked, the burst is kept and counted, but deletes after a quiet period no longer join it. A stop saved by v0.2.1 (without its list of files) counts every file that cannot be checked as missing, and is shown anew with its list.
- **Allowing a stop:** `lsync allow-deletes` shows the stop and asks; `lsync allow-deletes --stop TEXT` (the `safety_stop` from `lsync status --json`) is for scripts and agents. It deletes exactly the listed files, in the service when it runs the vault, else with a sync here. The confirmation is kept in the index until a pass applies it (or the stop changes), so a restart, a reload or a sync in between does not lose it; `lsync status --json` shows `mass_delete_allowed`, and `mass_delete_waits` while it waits for files that cannot be checked. `lsync sync --allow-mass-delete` still lets one pass delete whatever is missing.
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
  - a missing milestone, or one that is locked and doesn't list us. When the database was not rebuilt (its `created` is the one this device synced with), the message says so and names the device that holds the lock and when it last connected; `lsync status` shows it as `REMOTE LOCKED`. lsync never unlocks it by itself: unlock it from an official client if the rebuild was not meant, or run `lsync fetch` to start over from the remote;
  - incompatible chunk-version ranges;
  - path obfuscation, encryption other than V2 (or with encrypted internal metadata or dynamic iteration counts), a `hashAlg` other than xxhash64, or `pathNormalizationVersion=1`;
  - an encrypted database without the vault's passphrase, a passphrase for a database that is not encrypted, or a passphrase that does not decrypt the database (see "End-to-end encryption" below);
  - PREFERRED tweaks that changed since setup, or that it doesn't know (a setting a newer plugin added), until adopted with `lsync connect --adopt-tweaks`;
  - encrypted content in a database whose settings say it is not encrypted, and plaintext content in one that is (unless accepted);
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
| `heartbeat_secs` | 10 (Android 30) | the longpoll's heartbeat; at most 44 |
| `longpoll_timeout_secs` | 25 (Android 75; 2.5 heartbeats, at most 89, when only the heartbeat is set) | silence before the connection is taken for dead; at least two heartbeats and below the HTTP read timeout of 90 s |
| `clock_jump_secs` | 10 (Android: off) | a wall-clock jump this long past the monotonic clock is sleep: reconnect and rescan; `0` turns it off |

A reverse proxy that buffers responses holds the heartbeats back, and the daemon then reconnects every `longpoll_timeout_secs`. Turn buffering off for `/_changes` (nginx: `proxy_buffering off`), or raise the timeout.

```sh
lsync account add home --url https://couch.example.net --user me   # asks for the password
printf '%s' "$PASSWORD" | lsync account add home --url ... --user me --password-stdin
lsync account add ci --url ... --user me --secret env:LSYNC_CI_PASSWORD   # read from the environment
lsync account add home --url ... --user me --secret keyring:home       # the Keychain or the Secret Service
lsync account ls [--json]       # every account, where its password is kept, whether it is set
lsync account passwd home       # asks; or --password-stdin
lsync account rm home           # only when no vault uses it

lsync vault add notes --account home --db vault --folder ~/Notes [--exclude Archive] [--mode pull-only]
lsync vault ls [--json]
lsync vault edit notes --include Notes,Projects --device-name desk   # --unset KEY clears a setting
lsync vault passphrase notes    # an end-to-end encrypted database's passphrase (asked; --passphrase-stdin, --remove)
lsync vault disable notes       # and enable
lsync vault rm notes            # the folder and the index are kept

lsync --vault notes sync        # --profile is the same; with one vault there is nothing to name
```

These commands edit the file in place and keep its comments and layout; a change that would make the file invalid is refused and nothing is written. They take their settings from the command line only, never from `LSYNC_*` variables, and never a password from the command line.

**Folder rules.** No two vaults share a folder or a state folder. A vault folder inside another one is allowed only when it syncs a database of its own and the outer vault's scope excludes it; the refusal says what to add to which `exclude`.

**Passwords** never go in `lsync.toml`, in logs or in error messages. An account's `secret` names its store:

| `secret` | Where the password is |
|---|---|
| `file:KEY` (the default, `file:<account>`) | `secrets.toml` beside `lsync.toml`, mode 0600, `"KEY" = "password"` per line; refused while others can read it (but for the system service's: root's, 0640 to the service's group, in root's `/etc/lsync`) |
| `keyring:KEY` | the system keyring: the macOS Keychain (a generic password, service `lsync`, account `KEY`), or on Linux the Secret Service of the desktop session (gnome-keyring, KWallet, KeePassXC), in the default keyring with `service=lsync` and `username=KEY` (`secret-tool lookup service lsync username KEY` finds it) |
| `credential:NAME` | a systemd credential: the file `NAME` in `$CREDENTIALS_DIRECTORY` (`LoadCredential=` or `LoadCredentialEncrypted=` in the unit). For the system service, root sets it with `sudo lsync --system account add|passwd`, which runs `systemd-creds encrypt` into `/etc/credstore.encrypted/NAME` (the password on its stdin). The system service's names start with `lsync-` (`credential:lsync-home`): root never reads, writes, removes or loads into the unit any other credential in those folders |
| `env:VAR` | the environment variable `VAR` |

An account without `--user` is a server without authentication: it has no password and is not asked for one. The server URL holds no user or password (`https://me:pw@...` is refused: give `--user`, and the password when asked). `account add` never takes over a `file:` key that another account uses or that already holds a password, unless given `--replace-secret`.

`--password` and `LSYNC_PASSWORD` still override the store for one run.

**End-to-end encryption.** A vault whose database is encrypted names its passphrase's store the same way, in `passphrase` (`passphrase = "file:e2ee.home.vault"`). `lsync vault add NAME ... --e2ee` asks for it (or reads it with `--passphrase-stdin`; `--passphrase-secret STORE:KEY` puts it elsewhere than the default `file:e2ee.ACCOUNT.DATABASE`, which the vaults of one account on one database share: a second vault there finds it already stored and is not asked). `lsync vault passphrase NAME` sets or changes it (`--passphrase-stdin`, `--secret STORE:KEY`), for every vault that shares it, and `--remove` takes it off the vault (and out of the store when no other vault uses it). It is never taken from the command line, never logged, and never in `status --json` or `vault ls --json` (which say only where it is and whether it is set); `lsync doctor` checks that it can be read. `LSYNC_PASSPHRASE` gives it to one run, for runs without a vault in `lsync.toml`.

What lsync does with it, as the plugin 1.0.49 does: the database's `encrypt` (in the milestone's PREFERRED settings) and the vault's passphrase must agree, or the vault is refused with what to do; lsync never writes plaintext into an encrypted database or ciphertext into a plaintext one. The key is derived once per session from the passphrase and the database's salt (`_local/obsidian_livesync_sync_parameters`), and the passphrase is checked against the database (its `syncinfo`, else its first encrypted chunks) before anything is pulled or pushed: a wrong one stops the vault and writes nothing. A two-way sync creates a missing salt as the plugin does, in a database with no encrypted chunks yet; a read-only pull refuses instead. Chunks are encrypted (`%=`, AES-256-GCM with HKDF) under the passphrase-mixed `h:+` IDs the official clients make, so they deduplicate with theirs; file metadata (paths, sizes, times) stays readable on the server, as it does for the plugin without path obfuscation. A document with plaintext content (a device without encryption wrote it) stops the pull before it, as the plugin's replication does, and shows as `UPSTREAM STOP` in `lsync status` until it is gone, unless the vault sets `accept_unencrypted_content = true` (the plugin's "Accept unencrypted content"; `lsync vault edit NAME --accept-unencrypted-content true`). A single document or chunk that does not decrypt (changed ciphertext, the V1 format) is held: it stays pending in `status` with why, its local file is left alone, and nothing is pushed for it. `lsync folders` lists an encrypted database without the passphrase.

A keyring that is still locked (a desktop service that starts before the login unlocks it) is not an error: the vault shows "waiting for the keyring" in `lsync status --all`, and the service tries again every 20 s, so the vault starts once the keyring is unlocked. lsync never shows an unlock prompt itself. Without a desktop session (a server, an SSH login) there is no Secret Service: use `file:` or, for the system service, `credential:`. `file:` stays the default; `--secret keyring:NAME` puts an account's password in the keyring.

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
lsync --vault notes allow-deletes   # confirm notes' mass-delete stop (shows it and asks)
lsync reload                  # reread lsync.toml; only the vaults that changed restart
lsync flush [PATH...]         # push now; returns once stored on the server, with the revisions
lsync wait --applied PATH@REV # block until that revision is in the local file (or --idle)
lsync status --follow --waybar  # one Waybar JSON line per change (--format=swiftbar|text|json)
lsync serve --notify          # desktop notifications for stops, conflicts and a failing vault
```

- **Reload.** `kill -HUP`, `lsync reload`, and every `lsync account ...`, `lsync vault ...` or `lsync configure` that changes the file make the service reread it. Only the vaults whose settings changed (an account's URL, user or password included) are started, stopped or restarted; the others keep running, also while the reload waits for a command running on one that changed. A file that cannot be read changes nothing. With `--vault NAME`, a vault removed from the file or disabled is stopped (it must be there when the service starts). `kill -USR1` is "sync now" for every vault; SIGTERM and SIGINT stop the service.
- **Pause** stops a vault's engine and lets go of its folders until `resume`. A paused vault takes changes from a reload and starts with them at `resume`. Pausing is not kept across a restart of the service.
- **`lsync daemon`** runs one vault, from `lsync.toml`, a profile file or flags and `LSYNC_*` alone, as before (`lsync --profile bench daemon` included). It fails at once when the vault cannot start; after that it is supervised like the service's vaults (retried instead of exiting). It takes the control socket when no other service holds it, and runs without one otherwise.

**Routing.** A command for a vault the service runs is run by the service, which holds the vault's locks; otherwise the CLI opens the engine itself, one-shot, as before. Either way the output and the exit code are the same. A vault is the service's when the service runs one with the same state folder or the same vault folder.

| Command | While the service runs the vault |
|---|---|
| `status` | Read from the service (the running engine's status). |
| `sync`, `fetch`, `pull`, `connect` | The service stops the vault's engine, runs the command on an engine of its own with the vault's settings (and the command's `--allow-mass-delete`, `--remove-out-of-scope-copies`, `--adopt-tweaks`), and starts the vault's engine again, unless it was paused. Settings given only to this command (flags, `LSYNC_*`, a profile file that says otherwise) would not apply, so such a command is refused with what differs and what to do; for a paused vault it runs here instead, with its settings. The password is not compared: the service uses its own. |
| `allow-deletes` | The service confirms the stop it shows (with the stop text the CLI read), and the vault's next full pass deletes the listed files. A stop holds only its own vault: the others keep syncing. |
| `flush` | The vault's running engine pushes the paths now, without the settle wait, and answers once they are stored. Without a service: one two-way sync here, reported the same way. |
| `wait` | The vault's running engine answers once the condition holds. Without a service: the vault runs here (only its pull side for `--applied`) until it holds. |
| `daemon` | Refused: the service already runs the vault. |
| `folders`, `probe`, `account`, `vault`, `configure` | Run here (they don't open the vault's engine); the edits tell the service to reload. |
| `serve` | Refused while another service holds the control socket's folder. |

`pause`, `resume`, `sync-now`, `reload`, `events` and `status --all` need a running service; they name a vault with `--vault` (or `--profile`) on the command line, never from `LSYNC_PROFILE`, and act on every vault without one.

**Where the socket is.** `$LSYNC_RUNTIME_DIR/control.sock` when that is set; else `$XDG_RUNTIME_DIR/lsync/control.sock` (Linux) or `~/Library/Application Support/lsync/control.sock` (macOS) for a user's service, and `$RUNTIME_DIRECTORY` or `/run/lsync/control.sock` for `lsync --system serve`. Clients look at `$LSYNC_RUNTIME_DIR` alone when it is set, else at the user's socket and then the system's (with `--system`, only at the system's). It is a Unix socket and never listens on the network; a remote client goes through SSH (`ssh server lsync status --all --json`).

**Who may connect.** Before anything is bound, the socket's folder must be a real folder (not a link) owned by the service's user; it is made 0700, and a wider folder is tightened only when it holds nothing but the socket and its lock. The socket is 0600. Every connection's peer is checked (`SO_PEERCRED` on Linux, `getpeereid` on macOS) and must be the service's own user; `lsync --system serve --socket-group GROUP` also lets in that group's members (folder 0750 and socket 0660, owned by the group). Anyone else gets one `forbidden` line and is disconnected. The service holds an flock on `control.lock` beside the socket while it runs: a second service is refused and told which process holds it, and a socket left by a crashed service is replaced. A client uses a socket only in a folder no one but its owner can write to, owned by the client's own user (another owner only for the system folder `/run/lsync`), and only when the process behind it runs as that owner. A socket the client cannot connect to (none, a stale one, no permission, as a system service's folder is to users outside its group) is no service: it tries the next one, and the CLI then runs the command itself, which the engine's locks keep off a vault a service holds. A service that is there but turns the user away (`forbidden`) is an error that says so.

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
| `status` | `vault`? | `vaults`: per vault `name`, `state`, `message`, `paused`, `enabled`, `account`, `command`, `settings` (URL without credentials, user, database, folders, scope, mode, what syncs: ignore files, regexes, dot-paths, trash; never the password), `status` (as `lsync status --json`), `session` and `push` (this engine's counters), `deletes_waiting`, `mass_delete_allowed`, `mass_delete_waits` (allowed, but some of its files cannot be checked now), `fingerprint` (names `settings`; changes when they do), `activity` (running only: `running`, `two_way`, `pull`: `starting`, `catching-up`, `live`, `applying` or `retrying` (`unknown` for a phase a newer service has); `pushes`, `settling`, `pushing`, `push_due`, `deletes_waiting`, `flushes`, `unwatched` (some folders are not watched), `full_passes` and `full_pass_done` (full passes begun, and the number of the last one that completed), `refused` (a gate refusal that needs a person), `unreachable` (it waits because the server could not be reached, or failed in a way that passes: a network error, no data, a 5xx)) |
| `events` | `vault`? | `subscribed`, then events |
| `sync-now`, `pause`, `resume` | `vault`? (else every vault) | `vaults`: the names acted on |
| `reload` | `wait`? (`false`: return at once with `scheduled`) | `started`, `stopped`, `restarted`, `unchanged`, `failed` (`vault`, `message`) |
| `allow-deletes` | `vault`, `stop` (the vault's `status.safety_stop`, whole) | `vaults`: the vault. The next full pass deletes the files that stop listed. `no_stop` when it shows none; `stop_changed` when it shows another (the message quotes it); nothing is allowed then |
| `sync`, `fetch`, `pull`, `connect` | `vault`, and `allow_mass_delete`, `remove_out_of_scope_copies`, `adopt_tweaks`, `fingerprint` (from the `status` whose settings the client compared: when the vault's settings changed since, nothing runs and the error is `changed`) | `report` (as the command's `--json` prints it), `wall_ms`, `peak_rss_kib` (the service's) |
| `flush` | `vault`, `paths` (vault-relative; a folder stands for its files; none: every pending change), `timeout_ms`? | `all`, `paths` (per path `path`, `doc_id`, `rev`, `outcome`: `stored`, `deleted`, `conflict`, `not-synced`, `failed`, `held` or `missing`; `pushed`, `note`; with `all`, every revision the flush stored), `complete`, `safety_stop`, `issues`, `conflicts`. `timeout` when the time ran out, `blocked` at a gate's stop, `paused`, `failed` for a path through a symbolic link |
| `wait` | `vault`, and `idle: true` or `path` with `rev`? (without it, the server's revision when the wait begins), `timeout_ms`? | `vault`, `condition` (`idle` or `applied`), `path`, `rev`, `current_rev` (the revision in the local file now), `deleted`, `superseded` (a later revision is there, and the server's history shows `rev` before it), `waited_ms`. `bad_request` for a `rev` that is not `<generation>-<hash>`; `timeout` (the message says what was outstanding), `blocked` (a stop, a gate's refusal, a local edit or a conflict holds the revision back, the revision is on another branch than the local file's or the server cannot tell, the file is not synced here), `paused` |

A vault's `state` is `starting`, `running`, `failed` (retrying), `stopped`, `paused`, `busy` (running a command for a client), `disabled` or `invalid` (its settings cannot be used, for example a missing password; `message` says why).

Events: `state` (`state`, `message`), `paused` (`paused`), `pulled` (`written`, `deleted`, `adopted`, `dirty`, `conflicted`, `restored_edits`, `bytes_written`), `pushed` (`pushed`, `created`, `deleted`, `conflicts`, `chunks_uploaded`, `chunk_bytes`), `conflict` (`pulled`, `pushed`, `open`: the files in conflict now, the first 50 by path; also sent with both counts 0 when that list changes otherwise, as when a conflict is resolved), `stored` and `applied` (`path`, `doc_id`, `rev`, `deleted`: one per revision this device stored on the server, or took from it into the local file, as the index records them; they come on a channel of their own, so their order relative to the other events is not kept, and a flood of them never pushes the others out of a slow reader's buffer), `activity` (`idle`, `connected`, `pull`: when one of them changes), `safety-stop` (`stop`: the text, or `null` once cleared), `upstream-stop` (`stop`: what changed upstream, or `null` once the remote is compatible again), `remote-locked` (`lock`: who holds a lock that came without a rebuild, or `null` once it no longer refuses this device; both are states, sent when one starts, ends or its reason changes, not at every recheck), `error` (`message`), `command` (`command`, `phase`: `started` or `finished`, `ok`, `message`), `reload` (the reload's result) and `lagged` (`missed`: events a slow reader lost; with a `vault`, revision events lost before they reached the service; read `status` again). Counts are what happened since the vault's previous event of that kind, read from the engines about twice a second; `status` is the source of truth.

Error codes: `bad_request`, `unsupported_version`, `unknown_method`, `unknown_vault`, `not_running`, `paused`, `busy`, `refused` (a gate refused; the message is the refusal), `invalid_config`, `failed`, `forbidden`, `shutting_down`, `changed`, `no_stop`, `stop_changed`, `timeout` and `blocked` (`flush`, `wait`).

A `wait` or `flush` ends with its connection: a client that goes away leaves nothing waiting in the service.

From a shell: `printf '%s\n' '{"v":1,"id":1,"method":"status"}' | socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/lsync/control.sock"`.

</details>

<details>
<summary>Handing files over: flush and wait</summary>

An agent on a server writes a note and needs to know when the laptop has it; a script on the laptop waits for exactly that version:

```sh
# on the server
$ lsync flush notes/plan.md
stored     3-9f2c41e0b8a5d7c6e4f1a2b3c4d5e6f7   notes/plan.md

# on the laptop
$ lsync wait --applied notes/plan.md@3-9f2c41e0b8a5d7c6e4f1a2b3c4d5e6f7 --timeout 120
notes/plan.md is at 3-9f2c41e0b8a5d7c6e4f1a2b3c4d5e6f7 (after 0.4s)
```

- **`lsync flush [PATH...]`** pushes the files (a folder stands for its files) at once, without waiting for the save to settle, and returns once each is stored on the server, printing its outcome and the revision there. Without paths it pushes every pending change and lists the revisions it stored. A deletion goes out with the burst it is part of, so its answer can take the burst's quiet period (10 s). `--json` prints the report.
- **`lsync wait --applied PATH[@REV]`** returns once that revision, or a later one, is in the local file (a later one is reported as `superseded`). A later one counts only when it descends from REV: when REV was never in the local file, the server's revision history decides, and a REV on another branch (the losing side of a conflict) or a history the server cannot give ends the wait with exit code 2, never 0. Without `@REV` it waits for the revision the server has when the wait begins, or for the deletion when the server deleted the file. **`lsync wait --idle`** returns once nothing is pending or settling: the vault follows the server live, no local change waits to be pushed, no deletion is held, no document waits to be fetched again, no push failed, for 0.6 s in a row. Where some folders are not watched (watching is off, or the system's watch limit was reached), it also waits for a full pass over the vault, which it starts.
- **Timeouts.** Both give up after `--timeout SECS` (60 by default; 0 waits as long as it takes) with exit code 124 and what was still outstanding. A wait that cannot end well without a person ends at once with exit code 2: a safety stop, the upstream stop, a remote lock, a gate's refusal, a paused vault, a revision held back by a local edit or a conflict, or a file this vault does not sync (ignored, a dot-path, out of the folder scope). A flushed file this vault skips is reported `not-synced` (exit code 3), and a path through a symbolic link is refused.
- Both run in the service when it runs the vault, and wait while it starts or restarts. Without a service, `flush` runs one two-way sync and `wait` runs the vault here until the condition holds (only its pull side for `--applied`, which never writes to the server).

</details>

<details>
<summary>Status bars and notifications</summary>

`lsync status --follow --waybar` prints one [Waybar](https://github.com/Alexays/Waybar) JSON line whenever the state of the service's vaults changes (`--vault NAME` for one). `class` and `alt` are the most pressing state of any vault: `stopped` (a safety stop, the upstream stop or a remote lock: something to act on), `error` (a vault fails or cannot run, or the server answers but refuses it, as with a wrong password), `conflict`, `offline` (the server cannot be reached), `syncing`, `paused`, `ok`, or `off` (no service; it keeps looking for one). The tooltip has a line per vault. A Waybar module:

```jsonc
"custom/lsync": {
    "exec": "lsync status --follow --waybar",
    "return-type": "json",
    "restart-interval": 10,
    "format": "lsync {}"
}
```

Style it with the classes, for example `#custom-lsync.stopped, #custom-lsync.error { color: #e06c75; }`. For icons, use `"format": "{icon} {}"` with `"format-icons"` keyed by the states above (`alt`).

`lsync status --follow --format=swiftbar` does the same for [SwiftBar](https://github.com/swiftbar/SwiftBar) on macOS as a streamable plugin: put `exec lsync status --follow --format=swiftbar` in a plugin script with `# <swiftbar.type>streamable</swiftbar.type>`. `--format=text` and `--format=json` print a line for people or scripts. Without `--follow`, each prints one line and exits.

**Notifications.** `lsync serve --notify` (or `LSYNC_NOTIFY=1`) shows desktop notifications, with `notify-send` on Linux and `osascript` on macOS, for a safety stop, the upstream stop, a remote lock, files newly in conflict, and a vault that keeps failing (its engine ends with an error, or it reports errors for 5 minutes without reaching the server in between; at most once every 30 minutes for a vault that keeps crashing). Each is shown once when it starts or its reason changes, never again while it holds; a file in conflict is shown again only after it was resolved and fell into conflict anew. What a vault shows when the service starts, or when a reload adds it, is not news. `--notify-command PROGRAM` runs your own program instead, as `PROGRAM -- TITLE BODY` (the `--` keeps a body that starts with a dash, such as a file named `-u.md`, from being read as an option; `notify-send` takes it as is) with `LSYNC_NOTIFY_KIND`, `LSYNC_NOTIFY_VAULT` and `LSYNC_NOTIFY_URGENCY` set.

</details>

<details>
<summary>Installing the service (systemd, launchd)</summary>

`lsync service install` runs `lsync serve` as a service of the machine's service manager, enables it and (re)starts it. Run it again after an upgrade or after moving the binary; it rewrites only files it wrote itself (`--force` replaces another one). `--print` shows what it would write and changes nothing; `--no-start` enables without starting.

```sh
lsync service install            # Linux desktop: a systemd user service, from login to logout
lsync service install --linger   # a server: the user service starts at boot and runs without a login
lsync service status [--json]    # installed, enabled, running, lingering, and whether the socket answers
lsync service logs [-f] [-n 100] # the journal (macOS: ~/Library/Logs/lsync/lsync.log)
lsync service uninstall          # stops it and removes the unit; config, state and vaults stay
lsync doctor [--json]            # what the service needs, checked (below)
```

`lsync doctor` checks, without changing anything: the config; every secret store (ready, locked or not available here) and whether each account's password can be read; each enabled vault folder (exists, readable, writable; on macOS an EPERM says to grant Full Disk Access; with `--system`, a missing folder or `trash = "system"` is an error, as the system service can use neither); on Linux, the folders an inotify watcher needs (every folder but dot-folders) against `fs.inotify.max_user_watches`, with a warning past half of it; and the service (installed, running, lingering, whether its socket answers). It exits 1 when a check fails.

- **systemd user service:** `$XDG_CONFIG_HOME/systemd/user/lsync.service`. `Type=notify`: the service says `READY=1` once every vault's first start is decided (it runs, or is being retried), `RELOADING=1` and `READY=1` around a SIGHUP reload, and `WATCHDOG=1` with `WatchdogSec=60`, each ping only after the service's vault locks could be taken (a stuck vault gets the service restarted). `systemctl --user reload lsync` runs `lsync reload` (`ExecReload=`), which returns once the service has reread its config; `Restart=on-failure`; the log goes to the journal. When the installing shell sets `XDG_CONFIG_HOME`, `XDG_STATE_HOME` (to other than the defaults) or `LSYNC_RUNTIME_DIR`, the unit sets them too, so the service and the CLI agree; but the user's service manager reads units only from its own `XDG_CONFIG_HOME`, so `install` refuses a unit folder the manager does not read (`systemctl --user show -p UnitPath`) and says so. A user service stops at logout unless lingering is on: `--linger` runs `loginctl enable-linger`; `uninstall` leaves lingering as it is.
- **systemd system service:** `lsync --system ...` works with the system service's own config (`/etc/lsync/lsync.toml`), state (`/var/lib/lsync/<vault>`) and socket (`/run/lsync/control.sock`).

  ```sh
  sudo lsync --system account add home --url https://couch.example.net --user me
  sudo lsync --system vault add notes --account home --db vault --folder /srv/notes
  sudo lsync service install --system --run-as notes-user [--group agents]
  lsync --system status --all     # as the service's user, or a member of --group
  ```

  `/etc/systemd/system/lsync.service` runs `lsync --system serve` as `--run-as` (required: no default, and root only when named), with `ConfigurationDirectory=`, `StateDirectory=` and `RuntimeDirectory=lsync`, `UMask=0007` (files the service writes are group-writable and not readable by others), `ProtectSystem=strict`, `PrivateTmp` and `NoNewPrivileges`; `systemctl reload lsync` runs `lsync --system reload`. The binary must be one only root can change (`sudo install -m755 target/release/lsync /usr/local/bin/lsync`), and `--config` is refused: the service reads `/etc/lsync/lsync.toml`. **The config stays root's.** `/etc/lsync` is root's, 0750 with the service's group (the `--run-as` user's primary group, the unit's `Group=`; `ConfigurationDirectoryMode=0750`), and `lsync.toml` and `secrets.toml` in it are root's, 0640 with that group: the service reads them and cannot change them, and every edit is root's (`sudo lsync --system account|vault ...`, which keep that layout). `install` refuses a config folder, or a `lsync.toml` or `secrets.toml`, that is not root's, is a symlink or has a second hard link, and touches nothing else in the folder (the config lock stays root's, 0600, and is never opened through a symlink). That group must be the user's alone: `install` refuses a primary group that another user has as theirs or that lists other members (a shared `users` group, or `nobody`'s `nogroup`), and `lsync --system doctor` reports it; give the service a user of its own (`sudo useradd --system --user-group --create-home --home-dir /var/lib/lsync-home lsync`). `secrets.toml` holds the passwords of accounts on the `file:` store in plain text, readable by the service's group, so `install` warns about each one: `credential:lsync-NAME` keeps them encrypted, for the service alone. The drop-in `lsync.service.d/lsync-vaults.conf` lists the vault folders as `ReadWritePaths=` (the only places the service may write besides its state folder) and the accounts' systemd credentials (`LoadCredentialEncrypted=` from `/etc/credstore.encrypted/NAME`, or `LoadCredential=` from `/etc/credstore/NAME`); `lsync --system vault ...` and `account ...` rewrite it when that changes, and a restart applies it. The service cannot create a missing folder there, so root does: `install` creates each missing vault folder (below) and state folder (0700) the config names, for the service's user (`--print` lists them first), and `sudo lsync --system vault add|edit` creates the `--folder` or `--state-dir` it names; no other command creates one. A folder is created only beneath folders nobody but root can change, reached through no one else's symlink; parents made on the way stay root's (0755). A vault with `trash = "system"` is refused: the home trash is read-only to the service. Root never runs a system vault itself (`sudo lsync --system sync` and the like, and `sudo lsync --system serve` by hand while the unit runs the service as another user, are refused: the folders and state are the service user's); run those as the service's user (`sudo -u USER -g GROUP lsync --system ...`) or through the service. With `--group GROUP` the service gets that group as a supplementary group (`SupplementaryGroups=`) and its members may use the control socket (folder 0750, socket 0660, both the group's), for agents running as other users: `status --all`, `pause`, `resume`, `sync-now`, `events`, `reload`, and `lsync --system --vault NAME status|sync|fetch|pull`. They cannot read the config (`--group` may not be the user's own group), so the service runs those by name, with its own settings. Vault folders that `install` or `vault add|edit` creates are shared with that group (2770: setgid, so what the service writes there is the group's too); without `--group` they are 0770 with the user's own group.
- **macOS:** `~/Library/LaunchAgents/no.nyta.lsync.plist`, loaded into `gui/<uid>` with `launchctl bootstrap`: `RunAtLoad`, `KeepAlive` when it exits with an error, the log in `~/Library/Logs/lsync/`. It is not a `Background` process, which would throttle its I/O and timers. A vault under `~/Documents`, `~/Desktop`, `~/Downloads` or iCloud Drive needs Full Disk Access for the lsync binary.

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

**From the LiveSync plugin or the official CLI.** `lsync import FILE` reads the plugin's `data.json` (`<vault>/.obsidian/plugins/obsidian-livesync/data.json`) or the official CLI's `settings.json` (`<vault>/.livesync/settings.json`) and adds an account and a vault for it: the server, user and database, `syncIgnoreRegEx`, `syncOnlyRegEx` and the ignore files. The password goes into the secret store. An account for the same server and user is reused (and an import that brings another password for it is refused). The folder is the one the file lies in, or `--folder`; the name is the folder's, or `--name`. The connection in these files is encrypted with the configuration passphrase: `*` unless the plugin keeps one on the device or asks for it, and then `--config-passphrase-stdin` gives it. With end-to-end encryption on, the passphrase (`encryptedPassphrase`, decrypted with the connection, or `passphrase` in older files) goes into the secret store as the vault's `passphrase`, and `acceptUnencryptedContent` becomes `accept_unencrypted_content`. Settings lsync cannot honour stop the import with the reason: another kind of remote, encryption other than V2, encrypted internal metadata, path obfuscation, JWT or custom headers. So do the ones lsync leaves out on purpose (hidden-file sync, customisation sync, resolving conflicts by the newer file, writing conflicted documents, P2P), unless `--accept KEY` says to go on without them. `--dry-run` shows what would be added. The vault is added disabled.

`lsync adopt` then takes the folder over, before its first sync here. It compares every file with the server's version by its chunks, the way a push splits it, and by content where those differ (another client's splitter), and says what the first sync does with each file: identical files are adopted with no transfer and no write; differing ones are pushed as roots of their own beside the server's version, which makes conflicts with no common ancestor; files only here are pushed, files only on the server are written; files the server deleted are put back, removed or kept as conflicts, as the first sync's reconcile rules decide. `--dry-run` only reports, on a throwaway index; without it, the identical files are recorded and the first sync runs. Identical files are only adopted when every chunk of the server's version is on the server; the others are left to the first sync. It refuses a vault whose index already tracks files, a folder the server shares no file with, names that differ from the server's only in case or Unicode normalisation, folders it cannot read, and a folder whose LiveSync plugin is still enabled, as the plugin guard does (`--allow-plugin`, or `allow_plugin = true` for the vault, for a deliberate handover). A weak match, where fewer than half of the files on the smaller side (empty files not counted) are identical, needs `--accept-weak-match` once you checked it is the right folder and database. `--json` exits as `sync --json` does.

```sh
lsync import ~/Notes/.obsidian/plugins/obsidian-livesync/data.json   # adds vault "notes", disabled
lsync --vault notes adopt --dry-run    # identical, differing, only here, only on the server, deleted
lsync --vault notes adopt              # adopt the identical files, then the first sync
lsync vault enable notes               # let the service run it
```

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
lsync import FILE           # the plugin's data.json or the official CLI's settings.json -> account and vault
lsync adopt [--dry-run]     # take over a folder that is already in sync, before its first sync here

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
- 1: error, including a command line that does not parse (`check-ignore`: every path syncs). `--help` and `--version` exit 0.
- 2: refused by a gate (`adopt`: also a take-over that must not go ahead).
- 3: pull or sync finished, but something needs attention (dirty or pending files, local files that cannot sync, a safety stop). (`reload` exits 0 once the new config is applied, and names any vault that cannot run; it fails only when the service cannot read the config, which then changes nothing.)
- 4: probe failed or was inconclusive.
- 5: `allow-deletes`: the stop shown is not the one given; nothing was allowed. (`allow-deletes` exits 1 when no safety stop is shown.)
- `flush` and `wait`: 0 stored, or the condition holds; 1 an error (a command line that does not parse, as for every command, or a revision that is not one); 2 blocked until a person acts (a safety stop, the upstream stop, a remote lock, a gate's refusal, a paused vault, a revision a local edit or a conflict holds back, a revision on another branch or whose history the server cannot give, a file not synced here); 3 (`flush`) done, but not everything is on the server; 124 the timeout ran out (as `timeout(1)`).

</details>

## Layout

| Path | What |
|---|---|
| `crates/livesync-model` | Pure model: doc types, path→ID, chunk IDs, the splitter, decompression codec, gate evaluation, milestone registration, folder scope |
| `crates/livesync-crypto` | Pure end-to-end encryption core (E2EE V2/HKDF): the key context, `%=` envelopes, the document transforms with the unencrypted-content policy, the sync-parameters salt; `livesync-remote` applies it at the database boundary |
| `crates/livesync-remote` | CouchDB over HTTP (reqwest + rustls with bundled webpki roots): `_changes`, `_all_docs`, `_local` docs, backoff |
| `crates/livesync-engine` | Pull and push loops, daemons, SQLite index, vault file operations; platform seams (`Watcher`, `FsRules`) |
| `crates/livesync-host` | Hosting vaults for every front-end: one supervisor per vault (retries from 15 s to 15 min), accounts and vaults (`lsync.toml`), the secret stores, migration from profile files, the service (reload, pause, commands, events, flush and wait), the control socket with its protocol and client, and notifications |
| `crates/lsync` | The CLI, including `serve`, the routing through the control socket, `flush` and `wait`, the status bar output, `probe` and the account and vault commands |
| `crates/livesync-ffi` | UniFFI surface for the app over `livesync-host`: one engine per profile, connect test, status, logs |
| `crates/uniffi-bindgen` | UniFFI's Kotlin binding generator, pinned to the same version |
| `android/` | The Android app (Kotlin, Compose): foreground service, onboarding, vault list, status screens |
| `tools/golden` | Node program pinned to `@vrtmrz/livesync-commonlib` 0.2.1; it emits the fixtures in `fixtures/golden` |
| `scripts/` | APK and Termux builds, dev-database environment, a vault comparison script, and `interop.sh` (the M2 scenarios against the official CLI) |
| `install.sh`, `pkg/` | The install script, and the AUR package, Homebrew formula and Docker image, published at each release ([`pkg/README.md`](pkg/README.md)) |

Build and test instructions are in [AGENTS.md](AGENTS.md).

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT licence](LICENSE-MIT), at your option. Parts ported from Self-hosted LiveSync keep its MIT notice; see [NOTICE](NOTICE).

Unless you explicitly state otherwise, any contribution you submit for inclusion in this project, as defined in the Apache-2.0 licence, is dual-licensed as above, without any additional terms or conditions.

