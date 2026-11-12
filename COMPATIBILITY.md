# Compatibility with Self-hosted LiveSync

lsync writes every document in the format of the [Self-hosted LiveSync](https://github.com/vrtmrz/obsidian-livesync) plugin and its official CLI, so either can come back to a database lsync wrote to. It reads only what it knows: anything newer stops syncing for that vault, with a message that says what changed, instead of being guessed at.

## Tested versions

The release lsync is built and tested against is pinned in [`upstream.env`](upstream.env); the versions below that state it follow the pin (AGENTS.md, "Bumping upstream").

| Client | Version | How |
|---|---|---|
| Self-hosted LiveSync plugin | <!--pin:PLUGIN_RELEASE-->1.0.49 (the newest known to work), on databases set up before 1.0.46 | shares `livesync-commonlib` <!--pin:COMMONLIB-->0.4.3 with the official CLI of the same release |
| Official LiveSync CLI | <!--pin:PLUGIN_RELEASE-->1.0.49-cli (release tag <!--pin:PLUGIN_RELEASE-->1.0.49) | `scripts/interop.sh`, every scenario (its default) |
| Official LiveSync CLI | 1.0.46-cli (release tag `1.0.46`) | `scripts/interop.sh`, every scenario |
| Official LiveSync CLI | 1.0.40-cli | `scripts/interop.sh`, every scenario |
| `livesync-commonlib` | <!--pin:COMMONLIB-->0.4.3 | the golden vectors (`tools/golden`, with the matcher versions <!--pin:PLUGIN_RELEASE-->1.0.49 ships) |
| CouchDB | 3.x | the dev container and the weekly job (3.5) |

Databases at generation 12 (no feature list) and 13 with an empty `used_features` are supported. Plugin releases from 0.25 on have written databases lsync syncs with.

**End-to-end encryption** is supported as the plugin and CLI write it by default (`E2EEAlgorithm` V2, HKDF; `%=` envelopes; passphrase-mixed `h:+` chunk IDs with `hashAlg` xxhash64), without path obfuscation, byte-compatible with `livesync-commonlib` <!--pin:COMMONLIB-->0.4.3 (the golden vectors, and the opt-in check that upstream decrypts what lsync encrypts, `tools/golden/crypto-verify.mjs`). Interop scenarios E1 to E7 run it against the official CLI both ways, on a database the CLI encrypts as 1.0.49 does (generation 12, no `syncinfo`), and with lsync as the first writer. Refused: path obfuscation (`usePathObfuscation`), V1 and ForceV1 (`E2EEAlgorithm` other than `v2`), `useDynamicIterationCount`, `encryptInternalMetadata`, `idDerivationVersion` other than 0 (commonlib 0.4's keyed chunk IDs), a V1 eden; and per document, held rather than refused: V1 ciphertext and anything that does not authenticate. Turning encryption on or off for an existing database (a rebuild) is not done by lsync.

**New databases from 1.0.46 on.** A plugin or CLI from 1.0.46 that sets up a new, empty database gives it NFC path identity (`pathNormalizationVersion` 1, the feature `nfc-path-identity-v1`, generation 13). lsync does not implement that yet and refuses such a database (interop scenario S11 checks the message). A 1.0.46 device that joins an existing database keeps that database's path identity, so databases set up before 1.0.46 stay compatible. To set up a new database that lsync can sync, set `pathNormalizationVersion` to 0 in the first device's settings before its first sync, as `scripts/interop.sh` does.

**From 1.0.47 to 1.0.49** (commonlib 0.4.0 to 0.4.3) nothing changed in what is stored on an unencrypted database: no new feature, generation or tweak. What lsync follows:

- A document's `size` is the byte length of its content: commonlib 0.4.0 stores that instead of the file size the host reported, which could be stale. lsync has always written the byte length. Like the plugin, it does not write a file whose content does not have the length its `size` says (except a `size` of 0): the document stays pending, with the two numbers, until a writer stores it again.
- Only a `type: "leaf"` document with string `data` is a chunk; anything else under a chunk's ID counts as a missing chunk.
- Ignore files are matched with minimatch 11, on Unicode code points and with Unicode case folding: `?` matches one emoji, `K` (the Kelvin sign) matches `k`, `ς` matches `σ`, and an escaped `\|` is a literal `|`. A path segment that is `.` or `..` matches only literal dots.
- With end-to-end encryption on, 1.0.49 rejects documents with unencrypted content unless "Accept unencrypted content" is set: its replication fails at such a document (it is neither skipped nor written), and an on-demand chunk read leaves the chunk missing. lsync does the same: the pull stops before the document (`UPSTREAM STOP` in `lsync status` until it is gone), unless the vault sets `accept_unencrypted_content`. Devices older than 1.0.49 know nothing of this and may have written such documents.

`TESTED_PLUGIN_VERSION` in `crates/livesync-model/src/gate.rs`, the version the refusals name, is the pinned release, read from `upstream.env` at build time; so is the official CLI the interop scenarios, the testbed and the benchmark run by default.

**The weekly upstream job** (`.github/workflows/upstream-interop.yml`, Mondays, and by hand from the Actions tab with an optional release tag) builds the official CLI of the pinned release, and of the latest upstream release when that is newer, and runs every interop scenario against each on a throwaway CouchDB. A breaking release shows up as a failed run before any device updates: a format change in what the CLI writes, a setting lsync does not know in the milestone, or a database feature in the CLI's defaults that lsync has never heard of. The same workflow fails when upstream has a newer release, or npm a newer `livesync-commonlib`, than the pin. It never publishes anything.

## What the gates check

Each is read before every write session: when a sync or the daemon starts, after every reconnect (a dropped connection, a network change, the machine waking from sleep) and every regate interval (`regate_secs`, default 300) while the daemon runs. A version-document change in the changes feed stops the batch it comes in.

| Gate | Refuses | Why it matters |
|---|---|---|
| Server root | anything but CouchDB | only CouchDB remotes exist here |
| Version document (`obsydian_livesync_version`) | a missing document in a non-empty database, generation above 13 or below 12, a malformed document | a newer generation means a format change older clients must not write into |
| `used_features` | any feature, by name; unknown ones and the known `encrypted-internal-metadata-v1`, `independent-id-derivation-v1`, `nfc-path-identity-v1` (not implemented yet) | features are sticky: they stay on after the setting is turned off |
| Milestone (`_local/obsydian_livesync_milestone`) | missing; locked against this device without a rebuild; chunk-format ranges that do not overlap | a rebuild in progress, or devices that cannot read each other's chunks |
| Tweaks (the milestone's PREFERRED settings) | path obfuscation, encryption other than V2, dynamic iteration counts, encrypted internal metadata, `pathNormalizationVersion` or `idDerivationVersion` other than 0, a `hashAlg` other than xxhash64 | not implemented yet |
| End-to-end encryption | `encrypt` without a passphrase for the vault, a passphrase for a database without `encrypt`, a passphrase that does not decrypt the database (`syncinfo`, the chunks of the most recently changed files, else its first encrypted chunks), a missing salt in a read-only run (or in a database that holds encrypted chunks already) | lsync never writes plaintext into an encrypted database, or ciphertext into a plaintext one |
| Tweaks, changed | any compared setting that differs from what this device adopted at setup, with both values | chunking and IDs must match the other devices |
| Tweaks, new | any setting this client does not know, with its value | a newer plugin added it |
| Content | encrypted documents or chunks in a database whose settings say `encrypt=false`, obfuscated paths; plaintext content in an encrypted one (unless `accept_unencrypted_content`) | content the vault cannot read, or must not trust |
| Plugin guard | two-way sync in an Obsidian vault whose config folder enables `obsidian-livesync`: `.obsidian/` or any other top-level dot-folder (Obsidian's per-device "Override config folder", e.g. `.obsidian-mobile/`) that holds `plugins/obsidian-livesync/` and lists the plugin in its `community-plugins.json` | two clients would write one folder and one database |

Plain folders are never refused by the plugin guard, and read-only runs (`lsync pull`, `lsync daemon --pull-only`, a pull-only vault or app profile, and its fetch) are not two-way. A config folder, plugin folder or plugin list that cannot be read counts as enabled.

## When a guard trips

The vault stops syncing: nothing is pulled or pushed, local edits stay on disk, and every other vault runs on. `lsync status` shows an `UPSTREAM STOP` line (and `upstream_stop` in `--json`) with what changed and the newest plugin version known to work, and the service reports it in its status and events. The daemon checks again every regate interval and resumes by itself once the remote is compatible again. The stop is a state: its event and log line come when it starts, ends or its reason changes, not at every check. Encrypted content found in a pull (or plaintext content in an encrypted database) stays a stop until a pull gets past it (the gate has nothing against such a remote, so a passing gate does not lift it).

1. **Read the message.** It names the generation, the feature, or the setting and its values, and the newest plugin release the milestone lists (the device that most likely made the change).
2. **Find the device.** Usually a plugin or CLI that updated past the version above, or a setting changed in its "Sync settings" or "Remote configuration".
3. **Undo it, if it was not meant.** Turning a setting back clears a changed or new tweak. A feature in `used_features` stays even after its setting is off: going back needs a rebuild from a device without it (upstream's "Rebuild everything"), which every other device then fetches.
4. **Or keep it, if lsync supports it.** For a changed or new tweak that does not change what is stored (check the upstream release notes), adopt it on each lsync device: `lsync connect --adopt-tweaks`. A new generation or feature needs a newer lsync: check this file in the latest release.
5. **Pin the plugin** on devices that come back, at the version above, until lsync lists a newer one.

**A lock without a rebuild.** When another device locked the database (upstream's "rebuild" or "lock") and its `created` is still the one this device synced with, no rebuild came of the lock: it may have failed (a plugin whose database delete was refused, for example), or still be under way. lsync refuses with a message naming the device that holds the lock and when it last connected (as of when the lock was first seen; the closest to the lock time the milestone tells), shows `REMOTE LOCKED` in `lsync status` (`remote_locked` in `--json`, the `remote-locked` event in the service) and checks again every 30 s. The state, its event and its log line come once, and again only when the lock ends or another device holds it. It never unlocks the database itself. If the rebuild was not meant, unlock the database from an official client; if this device should start over from the remote as it is, run `lsync fetch`, which marks it accepted once it completes. A lock that comes with a rebuild (a new `created`) is followed by a refetch without asking.

**The plugin guard** stops two-way sync before anything is read or written. Disable the plugin on this device (Obsidian, Settings, Community plugins), or, for a deliberate handover where you make sure the plugin does not sync at the same time, run with `--allow-plugin` or set `allow_plugin = true` for the vault in `lsync.toml`. The Android app has no such switch: there the plugin has to be off for a two-way profile.

## Reporting a break

If the weekly job fails or a guard trips on a new upstream release, open an issue with the upstream release tag, the refusal message (it holds no credentials) and, for a failed job, the failing scenario lines from its log.
