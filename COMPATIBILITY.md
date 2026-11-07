# Compatibility with Self-hosted LiveSync

lsync writes every document in the format of the [Self-hosted LiveSync](https://github.com/vrtmrz/obsidian-livesync) plugin and its official CLI, so either can come back to a database lsync wrote to. It reads only what it knows: anything newer stops syncing for that vault, with a message that says what changed, instead of being guessed at.

## Tested versions

| Client | Version | How |
|---|---|---|
| Self-hosted LiveSync plugin | 1.0.46 (the newest known to work), on databases set up before it | shares `livesync-commonlib` 0.4.0 with the official CLI of the same release |
| Official LiveSync CLI | 1.0.46-cli (release tag `1.0.46`) | `scripts/interop.sh`, every scenario |
| Official LiveSync CLI | 1.0.40-cli | `scripts/interop.sh`, every scenario |
| `livesync-commonlib` | 0.2.1 | the golden vectors (`tools/golden`) |
| CouchDB | 3.x | the dev container and the weekly job (3.5) |

Databases at generation 12 (no feature list) and 13 with an empty `used_features` are supported. Plugin releases from 0.25 on have written databases lsync syncs with.

**New databases from 1.0.46 on.** A plugin or CLI from 1.0.46 that sets up a new, empty database gives it NFC path identity (`pathNormalizationVersion` 1, the feature `nfc-path-identity-v1`, generation 13). lsync does not implement that yet and refuses such a database (interop scenario S11 checks the message). A 1.0.46 device that joins an existing database keeps that database's path identity, so databases set up before 1.0.46 stay compatible. To set up a new database that lsync can sync, set `pathNormalizationVersion` to 0 in the first device's settings before its first sync, as `scripts/interop.sh` does.

`TESTED_PLUGIN_VERSION` in `crates/livesync-model/src/gate.rs` is the version the refusals name; change it with this table.

**The weekly upstream job** (`.github/workflows/upstream-interop.yml`, Mondays, and by hand from the Actions tab with an optional release tag) builds the official CLI from the latest upstream release and runs every interop scenario against it on a throwaway CouchDB. A breaking release shows up as a failed run before any device updates: a format change in what the CLI writes, a setting lsync does not know in the milestone, or a database feature in the CLI's defaults that lsync has never heard of. It never publishes anything.

## What the gates check

Each is read before every write session: when a sync or the daemon starts, after every reconnect (a dropped connection, a network change, the machine waking from sleep) and every regate interval (`regate_secs`, default 300) while the daemon runs. A version-document change in the changes feed stops the batch it comes in.

| Gate | Refuses | Why it matters |
|---|---|---|
| Server root | anything but CouchDB | only CouchDB remotes exist here |
| Version document (`obsydian_livesync_version`) | a missing document in a non-empty database, generation above 13 or below 12, a malformed document | a newer generation means a format change older clients must not write into |
| `used_features` | any feature, by name; unknown ones and the known `encrypted-internal-metadata-v1`, `independent-id-derivation-v1`, `nfc-path-identity-v1` (not implemented yet) | features are sticky: they stay on after the setting is turned off |
| Milestone (`_local/obsydian_livesync_milestone`) | missing; locked against this device without a rebuild; chunk-format ranges that do not overlap | a rebuild in progress, or devices that cannot read each other's chunks |
| Tweaks (the milestone's PREFERRED settings) | encryption, path obfuscation, dynamic iteration counts, encrypted internal metadata, `pathNormalizationVersion` or `idDerivationVersion` other than 0, a `hashAlg` other than xxhash64 | not implemented yet |
| Tweaks, changed | any compared setting that differs from what this device adopted at setup, with both values | chunking and IDs must match the other devices |
| Tweaks, new | any setting this client does not know, with its value | a newer plugin added it |
| Content | encrypted documents or chunks | end-to-end encryption is not implemented yet |
| Plugin guard | two-way sync in an Obsidian vault (a folder with `.obsidian/`) whose `.obsidian/community-plugins.json` enables `obsidian-livesync` and whose plugin folder exists | two clients would write one folder and one database |

Plain folders (no `.obsidian/`) are never refused by the plugin guard, and read-only runs (`lsync pull`, `lsync daemon --pull-only`) are not two-way. A plugin list that cannot be read counts as enabled.

## When a guard trips

The vault stops syncing: nothing is pulled or pushed, local edits stay on disk, and every other vault runs on. `lsync status` shows an `UPSTREAM STOP` line (and `upstream_stop` in `--json`) with what changed and the newest plugin version known to work, and the service reports it in its status and events. The daemon checks again every regate interval and resumes by itself once the remote is compatible again.

1. **Read the message.** It names the generation, the feature, or the setting and its values, and the newest plugin release the milestone lists (the device that most likely made the change).
2. **Find the device.** Usually a plugin or CLI that updated past the version above, or a setting changed in its "Sync settings" or "Remote configuration".
3. **Undo it, if it was not meant.** Turning a setting back clears a changed or new tweak. A feature in `used_features` stays even after its setting is off: going back needs a rebuild from a device without it (upstream's "Rebuild everything"), which every other device then fetches.
4. **Or keep it, if lsync supports it.** For a changed or new tweak that does not change what is stored (check the upstream release notes), adopt it on each lsync device: `lsync connect --adopt-tweaks`. A new generation or feature needs a newer lsync: check this file in the latest release.
5. **Pin the plugin** on devices that come back, at the version above, until lsync lists a newer one.

**The plugin guard** stops two-way sync before anything is read or written. Disable the plugin on this device (Obsidian, Settings, Community plugins), or, for a deliberate handover where you make sure the plugin does not sync at the same time, run with `--allow-plugin` or set `allow_plugin = true` for the vault in `lsync.toml`.

## Reporting a break

If the weekly job fails or a guard trips on a new upstream release, open an issue with the upstream release tag, the refusal message (it holds no credentials) and, for a failed job, the failing scenario lines from its log.
