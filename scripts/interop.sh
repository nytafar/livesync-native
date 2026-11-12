#!/usr/bin/env bash
# Interop scenarios (spec M2) between lsync and the official LiveSync CLI,
# against a throwaway database on the dev CouchDB container (never 8410).
#
#   scripts/interop.sh            # build lsync, run every scenario, drop the database
#   KEEP=1 scripts/interop.sh     # keep the database and the work folder
#   ONLY="S1 S4" scripts/interop.sh
#
# Needs: node, the official CLI (the pinned release in upstream.env, built on
# first use; see CLI_JS below), python3, curl, and
# ~/.config/livesync-android/dev.env (see scripts/dev-env.sh).
#
# Scenarios:
#   S1  the CLI creates, edits, renames and deletes; lsync converges
#   S2  lsync creates, edits, renames and deletes; the CLI converges
#   S3  chunk IDs: the same files pushed by both sides have identical children
#   S4  concurrent edits leave a branch the CLI lists as conflicted (both orders),
#       and a resolution made with the CLI comes back down
#   S5  folder scope with a second lsync profile on the same database:
#       include-only, a move out of scope (not a delete), widening, narrowing,
#       a move into .trash/ (a delete)
#   S6  a rebuild run from the CLI is detected and recovered from (fetch,
#       mark resolved, orphan pushed back, a local edit kept as a branch)
#   S7  both daemons running: edits, a rename and a delete on either side
#       reach the other, and the two vault folders end up identical
#   S8  the lsync daemon: 30 of 40 notes deleted in two bursts across settle
#       windows still stop (nothing deleted remotely); a confirmed sync
#       deletes them, and a second daemon takes the remote deletes in
#   S9  ignore files both ways: a fixture with nested .gitignores pushes the
#       set commonlib's rules predict, from lsync and from the official CLI
#       (useIgnoreFiles on); remote documents at ignored paths are not
#       written; a path that becomes ignored is untracked, never deleted
#   S10 remote deletes land in .trash/ with their folders, the emptied
#       folder goes, and nothing is pushed back
#   S11 the upstream guard on the CLI's own defaults: a database the CLI sets
#       up untouched is synced, or (when it turns features on) refused with a
#       message naming each feature and the newest plugin version known to work
#   E1  end-to-end encryption (V2, no path obfuscation), CLI -> lsync: a
#       database the CLI encrypts (generation 12, no syncinfo) is read, with
#       edits, renames and deletes
#   E2  lsync -> CLI: what lsync writes is ciphertext under h:+ chunk IDs the
#       CLI makes too, and the CLI reads it byte for byte (edits, a move, a delete)
#   E3  concurrent edits in the encrypted database: a branch both list as a
#       conflict, resolved with the CLI
#   E4  a wrong passphrase on either side: lsync refuses and writes nothing;
#       the CLI cannot read lsync's notes
#   E5  lsync as the first writer: it creates the missing encryption salt, and
#       a new CLI device reads what it wrote
#   E6  plaintext content injected into the encrypted database stops lsync's
#       pull (status shows it); once it is gone, syncing resumes
#   E7  a file whose chunk is damaged in place on the server is held: left
#       as it was on disk, a local edit of it not pushed, other files syncing
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$PWD
# shellcheck source=/dev/null
source scripts/dev-env.sh
case "$LSYNC_URL" in
  http://127.0.0.1:8411*|http://localhost:8411*) ;;
  *) echo "refusing: interop runs only against the dev container on 127.0.0.1:8411" >&2; exit 1 ;;
esac
# The official CLI of the pinned release (upstream.env), built into the
# reference layout when missing; LSYNC_CLI_JS or LSYNC_UPSTREAM_TAG run
# another one, and say so (scripts/upstream.sh).
CLI_JS=$(scripts/upstream.sh cli)
cli_version_of() { node -p 'require(process.argv[1]).version' "$(dirname "$1")/../package.json" 2>/dev/null || echo '?'; }
[[ -f "$CLI_JS" ]] || { echo "official CLI not found at $CLI_JS" >&2; exit 1; }
cargo build --release -q -p lsync
BIN=$REPO/target/release/lsync

RUN="$(date +%s)-$RANDOM"
DB="lsync-it-$RUN"
W=$(mktemp -d /tmp/lsync-it.XXXXXX)
# The daemons' control socket stays in the work folder, never the user's
# runtime folder (where a real `lsync serve` may listen).
export LSYNC_RUNTIME_DIR="$W/run"
RESULTS="$W/results.txt"
PASS=0
FAIL=0
PIDS=()

couch() { curl -sS -u "$LSYNC_USER:$LSYNC_PASSWORD" "$@"; }
cleanup() {
  local rc=$?
  for p in "${PIDS[@]:-}"; do [[ -n "$p" ]] && kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  echo
  echo "passed $PASS, failed $FAIL (database $DB, work folder $W)" | tee -a "$RESULTS"
  if [[ "${KEEP:-0}" != 1 ]]; then
    couch -X DELETE "$LSYNC_URL/$DB" >/dev/null || true
    couch -X DELETE "$LSYNC_URL/$DB-defaults" >/dev/null || true
    couch -X DELETE "$LSYNC_URL/$DB-e2ee" >/dev/null || true
    couch -X DELETE "$LSYNC_URL/$DB-e2ee-first" >/dev/null || true
    [[ "$W" == /tmp/lsync-it.* ]] && rm -rf -- "$W"
  fi
  exit $rc
}
trap cleanup EXIT

log() { echo "$*" | tee -a "$RESULTS"; }
ok() { PASS=$((PASS + 1)); log "  PASS $*"; }
bad() { FAIL=$((FAIL + 1)); log "  FAIL $*"; }
check() { local what=$1; shift; if "$@"; then ok "$what"; else bad "$what"; fi; }
want() { [[ -z "${ONLY:-}" || " $ONLY " == *" $1 "* ]]; }

# The official CLI (command mode) on its own PouchDB in $W/clidb.
cli() { node "$CLI_JS" "$W/clidb" --settings "$W/cli.json" "$@" 2>>"$W/cli.log"; }
cli_cat() { cli cat "$1" | sed '/^\[CLIWatchAdapter\]/d'; }
cli_ls_line() { cli ls "$1" | awk -F'\t' -v p="$1" '$1 == p { print; exit }'; }
cli_has() { [[ -n "$(cli_ls_line "$1")" ]]; }
cli_info() { cli info "$1" | sed -n '/^{/,$p'; }

# lsync, profile A (everything) and profile B (folder scope tests). Exit 3
# ("finished, but something needs attention", e.g. a conflict) is expected.
lsa() { local rc=0; LSYNC_DB=$DB LSYNC_VAULT=$W/vault LSYNC_STATE_DIR=$W/state LSYNC_CONFIG=$W/a.toml \
        LSYNC_DEVICE_NAME=interop-a "$BIN" "$@" 2>>"$W/lsync-a.log" || rc=$?; [[ $rc == 0 || $rc == 3 ]] || return $rc; }
lsb() { local rc=0; LSYNC_DB=$DB LSYNC_VAULT=$W/vault-b LSYNC_STATE_DIR=$W/state-b LSYNC_CONFIG=$W/b.toml \
        LSYNC_DEVICE_NAME=interop-b "$BIN" "$@" 2>>"$W/lsync-b.log" || rc=$?; [[ $rc == 0 || $rc == 3 ]] || return $rc; }
export LSYNC_REBUILD_QUIET_SECS=3

# The document for a vault path (IDs are lower-cased paths, `/_` for a leading underscore).
doc() { couch "$LSYNC_URL/$DB/$(python3 -c 'import sys,urllib.parse; p=sys.argv[1].lower(); p="/"+p if p.startswith("_") else p; print(urllib.parse.quote(p,safe=""))' "$1")${2:-}"; }
jfield() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(json.dumps(eval("d"+sys.argv[1])))' "$1"; }
remote_deleted() { [[ "$(doc "$1" | jfield '.get("deleted", False)')" == true ]]; }
remote_live() { local d; d=$(doc "$1"); [[ "$(echo "$d" | jfield '.get("deleted", False)')" == false && "$(echo "$d" | jfield '.get("_id")')" != null ]]; }
same_file() { cmp -s "$1" "$2"; }
file_is() { [[ -f "$1" && "$(cat "$1")" == "$2" ]]; }
gone() { [[ ! -e "$1" ]]; }
lsa_status() { lsa status --json; }

wait_for() { # wait_for <seconds> <description> <command...>
  local t=$1 what=$2; shift 2
  local start=$SECONDS
  while (( SECONDS - start < t )); do
    if "$@" 2>/dev/null; then ok "$what ($((SECONDS - start)) s)"; return 0; fi
    sleep 0.5
  done
  bad "$what (timed out after $t s)"
}

gen() { # gen <file> <kind> <bytes> [seed]
  python3 - "$@" <<'PY'
import os, random, sys
path, kind, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
seed = int(sys.argv[4]) if len(sys.argv) > 4 else 1
r = random.Random(seed)
os.makedirs(os.path.dirname(path), exist_ok=True)
if kind == "bin":
    data = bytes(r.getrandbits(8) for _ in range(n))
else:
    words = ["the ", "and ", "note ", "æøå ", "日本語の", "😀", "\n", "\n\n", "# Heading\n", "- [ ] task\n", "[[link]] ", "Straße ", "x"]
    out = []
    size = 0
    while size < n:
        w = r.choice(words); out.append(w); size += len(w.encode())
    data = "".join(out).encode()
open(path, "wb").write(data)
PY
}

CLI_VERSION=$(cli_version_of "$CLI_JS")
CLI_REF=$(git -C "$(dirname "$CLI_JS")" rev-parse --short HEAD 2>/dev/null || echo '?')
PINNED=$(scripts/upstream.sh get PLUGIN_RELEASE)
[[ "$CLI_VERSION" == "$PINNED-cli" ]] && CLI_PIN="the pinned release" || CLI_PIN="NOT the pinned $PINNED-cli"
log "== interop run $RUN: lsync $(git rev-parse --short HEAD) vs official CLI $CLI_VERSION ($CLI_PIN, ref $CLI_REF), database $DB"
couch -X PUT "$LSYNC_URL/$DB" >/dev/null

# --- setup: the CLI initialises the database with our setup's tweaks -----------
# pathNormalizationVersion 0 is the path identity of every database set up
# before 1.0.46, which a newer plugin joining it keeps. From 1.0.46 on, a new
# database gets NFC path identity (`nfc-path-identity-v1`), which lsync does
# not implement yet: S11 checks how such a database is refused.
node "$CLI_JS" init-settings --force "$W/cli-defaults.json" >>"$W/cli.log" 2>&1
cli_settings() { # cli_settings <src> <dst> <database> [JSON of more settings]
  local more=${4:-}
  [[ -n "$more" ]] || more='{}'
  SRC="$1" DST="$2" DBN="$3" MORE="$more" node -e '
const fs = require("fs"); const d = JSON.parse(fs.readFileSync(process.env.SRC, "utf8"));
Object.assign(d, { couchDB_URI: process.env.LSYNC_URL, couchDB_USER: process.env.LSYNC_USER,
  couchDB_PASSWORD: process.env.LSYNC_PASSWORD, couchDB_DBNAME: process.env.DBN, remoteType: "",
  isConfigured: true, liveSync: false, syncOnStart: false, syncOnSave: false }, JSON.parse(process.env.MORE));
fs.writeFileSync(process.env.DST, JSON.stringify(d, null, 2), { mode: 0o600 });'
}
cli_settings "$W/cli-defaults.json" "$W/cli.json" "$DB" \
  '{"customChunkSize": 60, "usePluginSync": false, "pathNormalizationVersion": 0}'
mkdir -p "$W/clidb" "$W/vault" "$W/vault-b" "$W/src"
printf 'init\n' | cli put setup/init.md >/dev/null
cli sync >/dev/null
check "CLI initialised the database (milestone with customChunkSize 60)" \
  test "$(couch "$LSYNC_URL/$DB/_local/obsydian_livesync_milestone" | jfield '["tweak_values"]["PREFERRED"]["customChunkSize"]')" == 60
lsa sync >/dev/null
NODE_A=$(lsa_status | jfield '["node_id"]' | tr -d '"')
check "lsync registered node $NODE_A (accepted after its first full fetch)" \
  bash -c "curl -sS -u '$LSYNC_USER:$LSYNC_PASSWORD' '$LSYNC_URL/$DB/_local/obsydian_livesync_milestone' | grep -q '\"$NODE_A\"'"

# --- S1: CLI -> lsync -----------------------------------------------------------
if want S1; then
log "S1 the CLI creates, edits, renames and deletes; lsync converges"
gen "$W/src/pic.png" bin 700000 11
printf 'A1 æøå\n' | cli put s1/a.md >/dev/null
printf 'rename me\n' | cli put s1/rename-me.md >/dev/null
printf 'delete me\n' | cli put s1/del.md >/dev/null
cli push "$W/src/pic.png" s1/pic.png >/dev/null
cli sync >/dev/null
lsa sync >/dev/null
check "created text arrives" file_is "$W/vault/s1/a.md" "A1 æøå"
check "created binary arrives byte-identical" same_file "$W/src/pic.png" "$W/vault/s1/pic.png"
printf 'A2 edited\n' | cli put s1/a.md >/dev/null
cli rm s1/rename-me.md >/dev/null
printf 'rename me\n' | cli put s1/renamed.md >/dev/null
cli rm s1/del.md >/dev/null
cli sync >/dev/null
lsa sync >/dev/null
check "edit arrives" file_is "$W/vault/s1/a.md" "A2 edited"
check "rename: old name removed" gone "$W/vault/s1/rename-me.md"
check "rename: new name present" file_is "$W/vault/s1/renamed.md" "rename me"
check "delete arrives" gone "$W/vault/s1/del.md"
fi

# --- S2: lsync -> CLI -----------------------------------------------------------
if want S2; then
log "S2 lsync creates, edits, renames and deletes; the CLI converges"
mkdir -p "$W/vault/s2"
printf 'mine æøå 😀\n' > "$W/vault/s2/x.md"
printf '{"a": [1, 2, 3]}\n' > "$W/vault/s2/data.json"
gen "$W/vault/s2/big.md" text 1500000 3
gen "$W/vault/s2/photo.jpg" bin 3000000 4
printf 'gone soon\n' > "$W/vault/s2/gone.md"
printf 'move me\n' > "$W/vault/s2/move.md"
lsa sync >/dev/null
cli sync >/dev/null
check "text arrives (cat)" test "$(cli_cat s2/x.md)" == "mine æøå 😀"
cli pull s2/photo.jpg "$W/out-photo.jpg" >/dev/null
check "3 MB binary arrives byte-identical (pull)" same_file "$W/vault/s2/photo.jpg" "$W/out-photo.jpg"
cli pull s2/big.md "$W/out-big.md" >/dev/null
check "1.5 MB text arrives byte-identical" same_file "$W/vault/s2/big.md" "$W/out-big.md"
check ".json is stored as newnote, like the official clients" test "$(doc s2/data.json | jfield '["type"]')" == '"newnote"'
printf 'mine, edited\n' > "$W/vault/s2/x.md"
mkdir -p "$W/vault/s2/moved"
mv "$W/vault/s2/move.md" "$W/vault/s2/moved/move.md"
rm "$W/vault/s2/gone.md"
lsa sync >/dev/null
cli sync >/dev/null
check "edit arrives" test "$(cli_cat s2/x.md)" == "mine, edited"
check "rename: old name deleted" bash -c "! node '$CLI_JS' '$W/clidb' --settings '$W/cli.json' cat s2/move.md >/dev/null 2>&1"
check "rename: new name present" test "$(cli_cat s2/moved/move.md)" == "move me"
check "delete is a soft delete (deleted:true, children kept)" bash -c \
  "curl -sS -u '$LSYNC_USER:$LSYNC_PASSWORD' '$LSYNC_URL/$DB/s2%2Fgone.md' | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get(\"deleted\") and d.get(\"children\") else 1)'"
check "the CLI no longer lists the deleted file" bash -c "! node '$CLI_JS' '$W/clidb' --settings '$W/cli.json' cat s2/gone.md >/dev/null 2>&1"
fi

# --- S3: chunk IDs ----------------------------------------------------------------
if want S3; then
log "S3 the same files pushed by both sides have identical chunk IDs"
gen "$W/src/s3/text.md" text 200000 21
gen "$W/src/s3/large.md" text 1200000 22
gen "$W/src/s3/huge.md" text 4300000 23
gen "$W/src/s3/blob.pdf" bin 2500000 24
gen "$W/src/s3/small.png" bin 40000 25
printf '{"nodes":[{"id":"a","text":"æøå"}],"edges":[]}\n' > "$W/src/s3/board.canvas"
printf 'col1,col2\næ,ø\n' > "$W/src/s3/table.csv"
mkdir -p "$W/vault/s3"
for f in text.md large.md huge.md blob.pdf small.png board.canvas table.csv; do
  cp "$W/src/s3/$f" "$W/vault/s3/$f"
  cli push "$W/src/s3/$f" "s3-cli/$f" >/dev/null
done
cli sync >/dev/null
lsa sync >/dev/null
for f in text.md large.md huge.md blob.pdf small.png board.canvas table.csv; do
  a=$(doc "s3/$f" | jfield '["children"]')
  b=$(doc "s3-cli/$f" | jfield '["children"]')
  n=$(echo "$a" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
  check "$f: $n chunk(s), same IDs as the CLI's" test "$a" == "$b"
done
fi

# --- S4: conflicts ----------------------------------------------------------------
if want S4; then
log "S4 concurrent edits leave a branch the CLI lists as conflicted"
mkdir -p "$W/vault/s4"
printf 'base\n' > "$W/vault/s4/c.md"
lsa sync >/dev/null
cli sync >/dev/null
# The CLI edits and syncs first; lsync edits the same base and syncs after (D4 force-base).
printf 'cli edit\n' | cli put s4/c.md >/dev/null
cli sync >/dev/null
printf 'lsync edit\n' > "$W/vault/s4/c.md"
lsa sync >/dev/null
check "lsync kept its local edit on disk" file_is "$W/vault/s4/c.md" "lsync edit"
check "lsync reports the file as conflicted" bash -c \
  "LSYNC_DB=$DB LSYNC_VAULT=$W/vault LSYNC_STATE_DIR=$W/state LSYNC_CONFIG=$W/a.toml '$BIN' status --json 2>/dev/null | grep -q 's4/c.md'"
cli sync >/dev/null
line=$(cli_ls_line s4/c.md)
log "    cli ls: $line"
check "the CLI lists s4/c.md as conflicted (rev ends with *)" test "${line: -1}" == "*"
info=$(cli_info s4/c.md)
check "the CLI's info shows the conflicting revision" bash -c "! grep -q '\"conflicts\": \"N/A\"' <<< '$info'"
leaves=$(doc s4/c.md '?conflicts=true' | jfield '.get("_conflicts", [])')
log "    remote _conflicts: $leaves"
# The other order: lsync pushes first, the CLI's own sync creates the branch.
printf 'base2\n' > "$W/vault/s4/d.md"
lsa sync >/dev/null
cli sync >/dev/null
printf 'lsync first\n' > "$W/vault/s4/d.md"
lsa sync >/dev/null
printf 'cli second\n' | cli put s4/d.md >/dev/null
cli sync >/dev/null
lsa sync >/dev/null
line=$(cli_ls_line s4/d.md)
check "CLI-side branch: the CLI lists s4/d.md as conflicted" test "${line: -1}" == "*"
check "CLI-side branch: lsync keeps its version and reports the conflict" bash -c \
  "[[ \$(cat '$W/vault/s4/d.md') == 'lsync first' ]] && LSYNC_DB=$DB LSYNC_VAULT=$W/vault LSYNC_STATE_DIR=$W/state LSYNC_CONFIG=$W/a.toml '$BIN' status --json 2>/dev/null | grep -q 's4/d.md'"
# Resolve both with the CLI (keep the CLI's revision); the result comes down.
for f in s4/c.md s4/d.md; do
  rev=$(cli_info "$f" | jfield '["revision"]' | tr -d '"')
  cli resolve "$f" "$rev" >/dev/null
done
cli sync >/dev/null
lsa sync >/dev/null
check "after the CLI resolved it, lsync has the kept version (c.md)" file_is "$W/vault/s4/c.md" "$(cli_cat s4/c.md)"
check "after the CLI resolved it, lsync has the kept version (d.md)" file_is "$W/vault/s4/d.md" "$(cli_cat s4/d.md)"
check "no conflicts left in lsync's status" bash -c \
  "LSYNC_DB=$DB LSYNC_VAULT=$W/vault LSYNC_STATE_DIR=$W/state LSYNC_CONFIG=$W/a.toml '$BIN' status --json 2>/dev/null | python3 -c 'import json,sys; sys.exit(1 if json.load(sys.stdin)[\"conflicted\"] else 0)'"
fi

# --- S5: folder scope ---------------------------------------------------------------
if want S5; then
log "S5 folder scope (second lsync profile, include-only, on the same database)"
printf 'in scope\n' | cli put Notes/a.md >/dev/null
printf 'in scope 2\n' | cli put Notes/b.md >/dev/null
printf 'outside\n' | cli put Other/o.md >/dev/null
cli sync >/dev/null
lsb --include notes sync >/dev/null
check "include-only: Notes/a.md arrives (case-insensitive prefix)" file_is "$W/vault-b/Notes/a.md" "in scope"
check "include-only: Other/o.md does not" gone "$W/vault-b/Other/o.md"
check "include-only: s2/ does not" gone "$W/vault-b/s2"
mkdir -p "$W/vault-b/Elsewhere"
mv "$W/vault-b/Notes/a.md" "$W/vault-b/Elsewhere/a.md"
printf 'local only\n' > "$W/vault-b/Elsewhere/new.md"
lsb --include notes sync >/dev/null
check "a move out of scope is not a delete (Notes/a.md still live)" remote_live Notes/a.md
check "a file outside the scope is not pushed" test "$(couch -o /dev/null -w '%{http_code}' "$LSYNC_URL/$DB/elsewhere%2Fnew.md")" == 404
check "status lists the move out of scope" bash -c \
  "LSYNC_DB=$DB LSYNC_VAULT=$W/vault-b LSYNC_STATE_DIR=$W/state-b LSYNC_CONFIG=$W/b.toml '$BIN' --include notes status --json 2>/dev/null | python3 -c 'import json,sys; s=json.load(sys.stdin); sys.exit(0 if s[\"out_of_scope\"] and s[\"out_of_scope\"][0][\"path\"]==\"Notes/a.md\" else 1)'"
lsb --include notes,other sync >/dev/null
check "widening include notes -> notes,other (an _all_docs key range): Other/o.md arrives" file_is "$W/vault-b/Other/o.md" "outside"
check "  ... and setup/ stays outside" gone "$W/vault-b/setup"
lsb --all-folders sync >/dev/null
check "widening: Other/o.md arrives" file_is "$W/vault-b/Other/o.md" "outside"
check "widening: setup/init.md arrives" file_is "$W/vault-b/setup/init.md" "init"
check "widening: the local files there are pushed" remote_live Elsewhere/new.md
grep -q "folder scope widened" "$W/lsync-b.log" && ok "widening used the catch-up (log: $(grep -o 'folder scope widened[^"]*' "$W/lsync-b.log" | tail -1))" || bad "widening catch-up not logged"
lsb --exclude other sync >/dev/null
check "narrowing (exclude Other): the local copy stays" file_is "$W/vault-b/Other/o.md" "outside"
rm "$W/vault-b/Other/o.md"
lsb --exclude other sync >/dev/null
check "narrowing: deleting an excluded file locally deletes nothing remotely" remote_live Other/o.md
mkdir -p "$W/vault-b/.trash"
mv "$W/vault-b/Notes/b.md" "$W/vault-b/.trash/b.md"
lsb --exclude other sync >/dev/null
check "a move into .trash/ is a delete" remote_deleted Notes/b.md
lsb --all-folders sync >/dev/null
check "widening exclude other -> all (key range): the excluded file comes back" file_is "$W/vault-b/Other/o.md" "outside"
lsa sync >/dev/null
check "profile A sees the trash delete" gone "$W/vault/Notes/b.md"
fi

# --- S6: rebuild from the CLI ---------------------------------------------------------
if want S6; then
log "S6 a rebuild run from the CLI is detected and recovered from"
mkdir -p "$W/clivault"
cli mirror "$W/clivault" >/dev/null
# After the CLI mirrored: lsync adds a file the rebuilder will not have, and
# edits another without syncing.
printf 'orphan\n' > "$W/vault/s6-orphan.md"
lsa sync >/dev/null
printf 'edited during the rebuild\n' > "$W/vault/s1/a.md"
created_before=$(couch "$LSYNC_URL/$DB/_local/obsydian_livesync_milestone" | jfield '["created"]')
node "$CLI_JS" "$W/clidb" --settings "$W/cli.json" --vault "$W/clivault" rebuild --force --write-settings >>"$W/cli.log" 2>&1
m=$(couch "$LSYNC_URL/$DB/_local/obsydian_livesync_milestone")
log "    after rebuild: created $(echo "$m" | jfield '["created"]') (was $created_before), locked $(echo "$m" | jfield '["locked"]'), accepted $(echo "$m" | jfield '["accepted_nodes"]')"
check "the milestone is locked against lsync" bash -c "[[ \$(echo '$m' | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d[\"locked\"] and \"$NODE_A\" not in d[\"accepted_nodes\"])') == True ]]"
rc=0; lsa sync >/dev/null || rc=$?
check "lsync sync after the rebuild succeeds (exit $rc)" test "$rc" -eq 0
check "lsync logged the rebuild" grep -q "rebuilt elsewhere" "$W/lsync-a.log"
m=$(couch "$LSYNC_URL/$DB/_local/obsydian_livesync_milestone")
check "lsync marked itself resolved (in accepted_nodes)" bash -c "echo '$m' | grep -q '\"$NODE_A\"'"
check "the orphan was pushed back" remote_live s6-orphan.md
check "the local edit is kept on disk" file_is "$W/vault/s1/a.md" "edited during the rebuild"
cli sync >/dev/null
line=$(cli_ls_line s1/a.md)
check "the local edit is a branch the CLI sees as conflicted" test "${line: -1}" == "*"
check "files match the rebuilt remote (s2/photo.jpg)" bash -c "node '$CLI_JS' '$W/clidb' --settings '$W/cli.json' pull s2/photo.jpg '$W/out2.jpg' >/dev/null 2>&1 && cmp -s '$W/out2.jpg' '$W/vault/s2/photo.jpg'"
rc=0; lsb --exclude s2,s3,s3-cli sync >/dev/null || rc=$?
check "the second profile recovers too (exit $rc)" test "$rc" -eq 0
rev=$(cli_info s1/a.md | jfield '["revision"]' | tr -d '"')
cli resolve s1/a.md "$rev" >/dev/null
cli sync >/dev/null
lsa sync >/dev/null
fi

# --- S7: both daemons ---------------------------------------------------------------
if want S7; then
log "S7 both daemons running; changes on either side converge"
SRC="$W/cli.json" DST="$W/cli-d.json" node -e '
const fs = require("fs"); const d = JSON.parse(fs.readFileSync(process.env.SRC, "utf8"));
d.liveSync = true; // the daemon follows _changes and watches its vault
fs.writeFileSync(process.env.DST, JSON.stringify(d, null, 2), { mode: 0o600 });'
mkdir -p "$W/clidb-d" "$W/clivault-d"
# A new CLI device after S6's rebuild: upstream's way in is a fetch, which
# also marks it resolved against the lock.
node "$CLI_JS" "$W/clidb-d" --settings "$W/cli-d.json" --vault "$W/clivault-d" fetch --force --write-settings >>"$W/cli-daemon.log" 2>&1
node "$CLI_JS" "$W/clidb-d" --settings "$W/cli-d.json" --vault "$W/clivault-d" daemon >>"$W/cli-daemon.log" 2>&1 &
PIDS+=($!)
LSYNC_DB=$DB LSYNC_VAULT=$W/vault LSYNC_STATE_DIR=$W/state LSYNC_CONFIG=$W/a.toml LSYNC_DEVICE_NAME=interop-a \
  "$BIN" daemon >>"$W/lsync-daemon.log" 2>&1 &
PIDS+=($!)
wait_for 120 "the CLI daemon mirrored the database" test -f "$W/clivault-d/setup/init.md"
mkdir -p "$W/vault/live"
printf 'from lsync\n' > "$W/vault/live/from-lsync.md"
wait_for 60 "lsync create reaches the CLI vault" file_is "$W/clivault-d/live/from-lsync.md" "from lsync"
mkdir -p "$W/clivault-d/live"
printf 'from cli\n' > "$W/clivault-d/live/from-cli.md"
wait_for 60 "CLI create reaches the lsync vault" file_is "$W/vault/live/from-cli.md" "from cli"
printf 'from lsync, edited\n' > "$W/vault/live/from-lsync.md"
wait_for 60 "lsync edit (in place) reaches the CLI vault" file_is "$W/clivault-d/live/from-lsync.md" "from lsync, edited"
printf 'from cli, edited\n' > "$W/clivault-d/live/from-cli.md"
wait_for 60 "CLI edit reaches the lsync vault" file_is "$W/vault/live/from-cli.md" "from cli, edited"
mv "$W/vault/live/from-cli.md" "$W/vault/live/renamed-by-lsync.md"
wait_for 60 "lsync rename reaches the CLI vault" bash -c "[[ -f '$W/clivault-d/live/renamed-by-lsync.md' && ! -e '$W/clivault-d/live/from-cli.md' ]]"
rm "$W/clivault-d/live/from-lsync.md"
wait_for 60 "CLI delete reaches the lsync vault" gone "$W/vault/live/from-lsync.md"
mkdir -p "$W/vault/.trash"
mv "$W/vault/live/renamed-by-lsync.md" "$W/vault/.trash/renamed-by-lsync.md"
wait_for 60 "lsync move into .trash/ deletes it in the CLI vault" gone "$W/clivault-d/live/renamed-by-lsync.md"
sleep 3
diffs=$(diff -rq -x '.*' "$W/vault" "$W/clivault-d" || true)
if [[ -z "$diffs" ]]; then
  ok "the two vault folders are identical ($(find "$W/vault" -type f -not -path '*/.*' | wc -l) files, dot-paths aside)"
else
  bad "the two vault folders differ:"
  echo "$diffs" | head -20 | sed 's/^/      /' | tee -a "$RESULTS"
fi
fi

# --- S8: a mass delete split over two passes ------------------------------------
if want S8; then
log "S8 the lsync daemon: a mass delete in two bursts across settle windows still stops"
lsc() { local rc=0; LSYNC_DB=$DB LSYNC_VAULT=$W/vault-c LSYNC_STATE_DIR=$W/state-c LSYNC_CONFIG=$W/c.toml \
        LSYNC_DEVICE_NAME=interop-c "$BIN" --include s8 "$@" 2>>"$W/lsync-c.log" || rc=$?; [[ $rc == 0 || $rc == 3 ]] || return $rc; }
lsc_field() { lsc status --json | jfield "$1"; }
for i in $(seq -w 0 39); do mkdir -p "$W/vault-c/s8"; printf 'note %s\n' "$i" > "$W/vault-c/s8/n$i.md"; done
lsc sync >/dev/null
check "40 notes pushed" remote_live s8/n39.md
LSYNC_DB=$DB LSYNC_VAULT=$W/vault-c LSYNC_STATE_DIR=$W/state-c LSYNC_CONFIG=$W/c.toml LSYNC_DEVICE_NAME=interop-c \
  "$BIN" --include s8 daemon >>"$W/lsync-c-daemon.log" 2>&1 &
C_PID=$!
PIDS+=("$C_PID")
lsc_up() { [[ "$(lsc_field '.get("daemon_heartbeat_ms")')" != null ]]; }
lsc_stopped() { lsc status --json | grep -q '30 of 40'; }
deleted_of() { local n=0 i; for i in $(seq -w "$1" "$2"); do remote_deleted "s8/n$i.md" && n=$((n + 1)); done; echo "$n"; }
wait_for 60 "the daemon is up" lsc_up
sleep 3
for i in $(seq -w 0 14); do rm "$W/vault-c/s8/n$i.md"; done
sleep 6 # three settle windows (the first burst gets passes of its own), within the 10 s delete hold
for i in $(seq -w 15 29); do rm "$W/vault-c/s8/n$i.md"; done
wait_for 60 "the daemon stops (30 of 40 missing)" lsc_stopped
deleted=$(deleted_of 0 29)
check "none of the 30 was deleted remotely ($deleted were)" test "$deleted" -eq 0
kill "$C_PID" 2>/dev/null || true
wait "$C_PID" 2>/dev/null || true
if want S7; then
  wait_for 60 "profile A's daemon (S7) has the 40 notes" file_is "$W/vault/s8/n29.md" "note 29"
fi
lsc sync --allow-mass-delete >/dev/null
deleted=$(deleted_of 0 29)
check "confirmed: all 30 deleted remotely ($deleted)" test "$deleted" -eq 30
check "the other 10 stay" remote_live s8/n35.md
check "the stop is cleared" test "$(lsc_field '.get("safety_stop")')" == null
if want S7; then
  # Profile A's daemon (S7) takes the 30 remote deletes in: they are not its
  # own deletes, so they neither stop it nor go back up.
  wait_for 60 "profile A's daemon applied the remote deletes" gone "$W/vault/s8/n29.md"
  sleep 3
  check "profile A's daemon did not stop on them" test "$(lsa_status | jfield '.get("safety_stop")')" == null
  check "profile A kept the other 10" file_is "$W/vault/s8/n35.md" "note 35"
fi
fi

# --- S9: ignore files ------------------------------------------------------------
# A separate lsync profile (D) with ignore files on, scoped to its folders.
lsd() { local rc=0; LSYNC_DB=$DB LSYNC_VAULT=$W/vault-d LSYNC_STATE_DIR=$W/state-d LSYNC_CONFIG=$W/d.toml \
        LSYNC_DEVICE_NAME=interop-d LSYNC_IGNORE_FILES=.gitignore "$BIN" --include s9l,s10 "$@" 2>>"$W/lsync-d.log" || rc=$?; [[ $rc == 0 || $rc == 3 ]] || return $rc; }
# The live documents under a prefix, as paths relative to it.
live_under() {
  couch "$LSYNC_URL/$DB/_all_docs?include_docs=true&startkey=%22$1%2F%22&endkey=%22$1%2F%EF%BF%B0%22" | python3 -c '
import json, sys
pre = sys.argv[1] + "/"
rows = json.load(sys.stdin)["rows"]
out = sorted(r["doc"]["path"][len(pre):] for r in rows
             if r.get("doc") and r["doc"].get("type") in ("plain", "newnote") and not r["doc"].get("deleted"))
print(" ".join(out))' "$1"
}
s9_fixture() { # s9_fixture <folder>
  local d=$1
  mkdir -p "$d/notes/drafts" "$d/code/venv/lib" "$d/build" "$d/sub/build"
  printf '*.log\nbuild/\n!keep.log\nprivate.md\n' > "$d/.gitignore"
  printf 'drafts/\n*.tmp\n!keep.tmp\nsecret\n!secret/public.md\n' > "$d/notes/.gitignore"
  printf '!*.md\n' > "$d/notes/drafts/.gitignore"
  printf '*\n' > "$d/code/venv/.gitignore"
  local f
  for f in a.md a.log keep.log private.md sub/private.md sub/a.log build/out.md sub/build/out.md \
           notes/n.md notes/x.tmp notes/keep.tmp notes/drafts/d.md notes/drafts/d.txt \
           notes/secret/x.md notes/secret/public.md code/main.py code/venv/lib/x.py; do
    mkdir -p "$(dirname "$d/$f")"; printf '%s\n' "$f" > "$d/$f"
  done
}
# What commonlib's isAcceptedAll accepts in that fixture (see
# fixtures/golden/ignore_all.json and crates/livesync-engine/tests/filters.rs).
S9_WANT="a.md code/main.py keep.log notes/drafts/d.md notes/keep.tmp notes/n.md notes/secret/public.md sub/a.log sub/build/out.md sub/private.md"
if want S9; then
log "S9 ignore files both ways (nested .gitignores; lsync and the official CLI)"
s9_fixture "$W/vault-d/s9l"
lsd sync >/dev/null
got=$(live_under s9l)
check "lsync pushes exactly what commonlib's rules accept" test "$got" == "$S9_WANT"
[[ "$got" == "$S9_WANT" ]] || log "    got:  $got"$'\n'"    want: $S9_WANT"
# The official CLI on the same fixture, with useIgnoreFiles on. A new CLI
# device starts with a fetch (also after S6's rebuild).
SRC="$W/cli.json" DST="$W/cli-i.json" node -e '
const fs = require("fs"); const d = JSON.parse(fs.readFileSync(process.env.SRC, "utf8"));
d.useIgnoreFiles = true; d.ignoreFiles = ".gitignore";
fs.writeFileSync(process.env.DST, JSON.stringify(d, null, 2), { mode: 0o600 });'
mkdir -p "$W/clidb-i" "$W/clivault-i"
clii() { node "$CLI_JS" "$W/clidb-i" --settings "$W/cli-i.json" --vault "$W/clivault-i" "$@" >>"$W/cli-ignore.log" 2>&1; }
clii fetch --force --write-settings || true
s9_fixture "$W/clivault-i/s9c"
clii mirror || true
clii sync || true
got_cli=$(live_under s9c)
if [[ -z "$got_cli" ]]; then
  bad "the official CLI pushed nothing from its vault (see cli-ignore.log)"
else
  log "    official CLI pushed: $got_cli"
  check "the official CLI (useIgnoreFiles) pushes the same set as lsync" test "$got_cli" == "$got"
fi
# The vault-root ignore file: commonlib asks for it as "/.gitignore". lsync
# reads it, as the plugin does; the official CLI's storage layer refuses a
# path with a leading slash, so it does not (logged, not judged).
printf 's9l/rootcheck.md\n' > "$W/vault-d/.gitignore"
printf 'root rule\n' > "$W/vault-d/s9l/rootcheck.md"
lsd sync >/dev/null
check "lsync honours the vault-root ignore file" test "$(couch -o /dev/null -w '%{http_code}' "$LSYNC_URL/$DB/s9l%2Frootcheck.md")" == 404
printf 's9c/rootcheck.md\n' > "$W/clivault-i/.gitignore"
printf 'root rule\n' > "$W/clivault-i/s9c/rootcheck.md"
clii mirror || true
clii sync || true
if [[ "$(couch -o /dev/null -w '%{http_code}' "$LSYNC_URL/$DB/s9c%2Frootcheck.md")" == 404 ]]; then
  log "    note: the official CLI honoured the vault-root ignore file"
else
  log "    note: the official CLI does not honour the vault-root ignore file (it pushed s9c/rootcheck.md)"
fi
# Remote -> local: documents at ignored paths are not written.
printf 'r\n' | cli put s9l/remote.log >/dev/null
printf 'r\n' | cli put s9l/build/r.md >/dev/null
printf 'r\n' | cli put s9l/notes/drafts/r.md >/dev/null
printf 'r\n' | cli put s9l/r.md >/dev/null
cli sync >/dev/null
lsd sync >/dev/null
check "a remote document at a synced path arrives" file_is "$W/vault-d/s9l/r.md" "r"
check "an accepted path inside an ignored folder arrives (nearest ignore file decides)" file_is "$W/vault-d/s9l/notes/drafts/r.md" "r"
check "remote documents at ignored paths are not written (*.log)" gone "$W/vault-d/s9l/remote.log"
check "remote documents at ignored paths are not written (build/)" gone "$W/vault-d/s9l/build/r.md"
check "check-ignore names the rule" bash -c "LSYNC_DB=$DB LSYNC_VAULT=$W/vault-d LSYNC_STATE_DIR=$W/state-d LSYNC_CONFIG=$W/d.toml LSYNC_IGNORE_FILES=.gitignore '$BIN' --include s9l,s10 check-ignore s9l/notes/x.tmp 2>/dev/null | grep -q '^s9l/notes/.gitignore:2:\*.tmp'"
# A path that becomes ignored: untracked, never deleted remotely.
printf '*.md\n' > "$W/vault-d/s9l/sub/.gitignore"
lsd sync >/dev/null
rm "$W/vault-d/s9l/sub/private.md"
lsd sync >/dev/null
check "a file deleted after it became ignored stays live remotely" remote_live s9l/sub/private.md
# And one that stops being ignored is fetched back.
rm "$W/vault-d/s9l/sub/.gitignore"
lsd sync >/dev/null
check "a path that stops being ignored is caught up" file_is "$W/vault-d/s9l/sub/private.md" "sub/private.md"
fi

# --- S10: the trash ----------------------------------------------------------------
if want S10; then
log "S10 remote deletes land in .trash/ with their folders; nothing is pushed back"
printf 'to be deleted\n' | cli put s10/sub/t.md >/dev/null
printf 'stays\n' | cli put s10/keep.md >/dev/null
cli sync >/dev/null
lsd sync >/dev/null
check "the file arrives" file_is "$W/vault-d/s10/sub/t.md" "to be deleted"
cli rm s10/sub/t.md >/dev/null
cli sync >/dev/null
lsd sync >/dev/null
check "the remote delete moved it to .trash/ with its folders" file_is "$W/vault-d/.trash/s10/sub/t.md" "to be deleted"
check "it is gone from its folder" gone "$W/vault-d/s10/sub/t.md"
check "the emptied folder is removed" gone "$W/vault-d/s10/sub"
check "a folder holding anything stays" file_is "$W/vault-d/s10/keep.md" "stays"
lsd sync >/dev/null
check "nothing is pushed back: still deleted remotely" remote_deleted s10/sub/t.md
check "nothing in .trash/ is pushed" test "$(couch -o /dev/null -w '%{http_code}' "$LSYNC_URL/$DB/.trash%2Fs10%2Fsub%2Ft.md")" == 404
check "the CLI no longer lists it" bash -c "! node '$CLI_JS' '$W/clidb' --settings '$W/cli.json' cat s10/sub/t.md >/dev/null 2>&1"
fi

# --- S11: the upstream guard on the CLI's own defaults ---------------------------
if want S11; then
log "S11 a database the CLI sets up with its own defaults"
couch -X PUT "$LSYNC_URL/$DB-defaults" >/dev/null
cli_settings "$W/cli-defaults.json" "$W/cli-d9.json" "$DB-defaults"
mkdir -p "$W/clidb-d9" "$W/vault-d9"
cli9() { node "$CLI_JS" "$W/clidb-d9" --settings "$W/cli-d9.json" "$@" 2>>"$W/cli.log"; }
lsd9() { LSYNC_DB=$DB-defaults LSYNC_VAULT=$W/vault-d9 LSYNC_STATE_DIR=$W/state-d9 LSYNC_CONFIG=$W/d9.toml \
         LSYNC_DEVICE_NAME=interop-d9 "$BIN" "$@"; }
printf 'defaults\n' | cli9 put s9/init.md >/dev/null
cli9 sync >/dev/null
features=$(couch "$LSYNC_URL/$DB-defaults/obsydian_livesync_version" |
  python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin).get("used_features", [])))')
log "    the CLI's defaults turn on: ${features:-no features}"
rc=0
lsd9 sync >/dev/null 2>"$W/lsync-d9.log" || rc=$?
if [[ -n "$features" ]]; then
  check "lsync refuses it (exit $rc; 2 is a gate's refusal)" test "$rc" -eq 2
  for f in $features; do
    check "  ... naming $f" grep -q -- "$f" "$W/lsync-d9.log"
  done
  # A feature lsync has never heard of is news: fail, so someone looks.
  check "  ... all of them features lsync knows of (if not, see COMPATIBILITY.md)" \
    bash -c "! grep -q 'features this client does not know' '$W/lsync-d9.log'"
  check "  ... and the newest plugin version known to work" \
    grep -q "is the newest version known to work with lsync" "$W/lsync-d9.log"
  st9=$(lsd9 status --json 2>/dev/null || true)
  check "  ... and status shows the upstream stop" \
    test "$(echo "$st9" | jfield '.get("upstream_stop") is not None')" == true
  check "  ... and nothing was written to the database" bash -c \
    "! curl -sS -u '$LSYNC_USER:$LSYNC_PASSWORD' '$LSYNC_URL/$DB-defaults/_local/obsydian_livesync_milestone' | grep -q interop-d9"
else
  check "lsync syncs it (exit $rc)" test "$rc" -eq 0 -o "$rc" -eq 3
  check "  ... and has the CLI's note" file_is "$W/vault-d9/s9/init.md" "defaults"
fi
fi

# --- E1-E6: end-to-end encryption ------------------------------------------------
if want E1 || want E2 || want E3 || want E4 || want E5 || want E6 || want E7; then
DBE="$DB-e2ee"
# A throwaway, never logged.
E2EE_PASS="interop-throwaway-$RUN"
E2EE_TWEAKS='"customChunkSize": 60, "usePluginSync": false, "pathNormalizationVersion": 0, "encrypt": true, "E2EEAlgorithm": "v2", "usePathObfuscation": false, "encryptInternalMetadata": false, "idDerivationVersion": 0, "hashAlg": "xxhash64"'
couch -X PUT "$LSYNC_URL/$DBE" >/dev/null
cli_settings "$W/cli-defaults.json" "$W/cli-e.json" "$DBE" "{$E2EE_TWEAKS, \"passphrase\": \"$E2EE_PASS\"}"
mkdir -p "$W/clidb-e" "$W/vault-e"
clie() { node "$CLI_JS" "$W/clidb-e" --settings "$W/cli-e.json" "$@" 2>>"$W/cli.log"; }
clie_cat() { clie cat "$1" | sed '/^\[CLIWatchAdapter\]/d'; }
clie_ls_line() { clie ls "$1" | awk -F'\t' -v p="$1" '$1 == p { print; exit }'; }
lse() { local rc=0; LSYNC_DB=$DBE LSYNC_VAULT=$W/vault-e LSYNC_STATE_DIR=$W/state-e LSYNC_CONFIG=$W/e.toml \
        LSYNC_DEVICE_NAME=interop-e LSYNC_PASSPHRASE=$E2EE_PASS "$BIN" "$@" 2>>"$W/lsync-e.log" || rc=$?; [[ $rc == 0 || $rc == 3 ]] || return $rc; }
edoc() { couch "$LSYNC_URL/$DBE/$(python3 -c 'import sys,urllib.parse; p=sys.argv[1].lower(); p="/"+p if p.startswith("_") else p; print(urllib.parse.quote(p,safe=""))' "$1")${2:-}"; }
# Every chunk in $1 is V2 ciphertext (e_, "%=") under an h:+ ID; prints the count.
all_sealed() {
  couch "$LSYNC_URL/$1/_all_docs?include_docs=true&start_key=%22h%3A%22&end_key=%22h%3B%22" | python3 -c '
import json, sys
rows = [r for r in json.load(sys.stdin)["rows"] if r.get("doc")]
bad = [r["id"] for r in rows if not (r["id"].startswith("h:+") and r["doc"].get("e_") is True and r["doc"].get("data", "").startswith("%="))]
print(len(rows)); sys.exit(1 if bad or not rows else 0)' >/dev/null
}
printf 'encrypted init\n' | clie put e/init.md >/dev/null
clie sync >/dev/null
check "the CLI encrypted the database (encrypt in the tweaks, a salt, h:+ chunks)" bash -c \
  "curl -sS -u '$LSYNC_USER:$LSYNC_PASSWORD' '$LSYNC_URL/$DBE/_local/obsydian_livesync_milestone' | grep -q '\"encrypt\":true' && curl -sS -u '$LSYNC_USER:$LSYNC_PASSWORD' '$LSYNC_URL/$DBE/_local/obsidian_livesync_sync_parameters' | grep -q pbkdf2salt"
log "    version document: $(couch "$LSYNC_URL/$DBE/obsydian_livesync_version" | jfield '.get("version")'), syncinfo: $(couch -o /dev/null -w '%{http_code}' "$LSYNC_URL/$DBE/syncinfo")"
check "every chunk the CLI wrote is sealed" all_sealed "$DBE"
lse sync >/dev/null
check "lsync reads the encrypted database" file_is "$W/vault-e/e/init.md" "encrypted init"
fi

if want E1; then
log "E1 the CLI writes an encrypted database; lsync converges"
gen "$W/src/e1.png" bin 700000 31
printf 'E1 æøå 😀\n' | clie put e1/a.md >/dev/null
printf 'rename me\n' | clie put e1/rename-me.md >/dev/null
printf 'delete me\n' | clie put e1/del.md >/dev/null
clie push "$W/src/e1.png" e1/pic.png >/dev/null
clie sync >/dev/null
lse sync >/dev/null
check "created text arrives decrypted" file_is "$W/vault-e/e1/a.md" "E1 æøå 😀"
check "created binary arrives byte-identical" same_file "$W/src/e1.png" "$W/vault-e/e1/pic.png"
printf 'E1 edited\n' | clie put e1/a.md >/dev/null
clie rm e1/rename-me.md >/dev/null
printf 'rename me\n' | clie put e1/renamed.md >/dev/null
clie rm e1/del.md >/dev/null
clie sync >/dev/null
lse sync >/dev/null
check "edit arrives" file_is "$W/vault-e/e1/a.md" "E1 edited"
check "rename: old name removed" gone "$W/vault-e/e1/rename-me.md"
check "rename: new name present" file_is "$W/vault-e/e1/renamed.md" "rename me"
check "delete arrives" gone "$W/vault-e/e1/del.md"
fi

if want E2; then
log "E2 lsync writes into the encrypted database; the CLI reads it"
mkdir -p "$W/vault-e/e2"
printf 'mine æøå 😀\n' > "$W/vault-e/e2/x.md"
gen "$W/vault-e/e2/big.md" text 1500000 33
gen "$W/vault-e/e2/photo.jpg" bin 3000000 34
printf 'gone soon\n' > "$W/vault-e/e2/gone.md"
printf 'move me\n' > "$W/vault-e/e2/move.md"
lse sync >/dev/null
check "everything on the server is still sealed (no plain chunk, no plain h: ID)" all_sealed "$DBE"
check "metadata stays readable without path obfuscation" test "$(edoc e2/x.md | jfield '["path"]')" == '"e2/x.md"'
clie sync >/dev/null
check "the CLI decrypts lsync's text" test "$(clie_cat e2/x.md)" == "mine æøå 😀"
clie pull e2/photo.jpg "$W/out-e-photo.jpg" >/dev/null
check "3 MB binary arrives byte-identical at the CLI" same_file "$W/vault-e/e2/photo.jpg" "$W/out-e-photo.jpg"
clie pull e2/big.md "$W/out-e-big.md" >/dev/null
check "1.5 MB text arrives byte-identical at the CLI" same_file "$W/vault-e/e2/big.md" "$W/out-e-big.md"
for f in big.md photo.jpg; do
  clie push "$W/vault-e/e2/$f" "e2-cli/$f" >/dev/null
done
clie sync >/dev/null
for f in big.md photo.jpg; do
  a=$(edoc "e2/$f" | jfield '["children"]')
  b=$(edoc "e2-cli/$f" | jfield '["children"]')
  check "$f: the same passphrase-mixed h:+ chunk IDs as the CLI's" test "$a" == "$b"
done
printf 'mine, edited\n' > "$W/vault-e/e2/x.md"
mkdir -p "$W/vault-e/e2/moved"
mv "$W/vault-e/e2/move.md" "$W/vault-e/e2/moved/move.md"
rm "$W/vault-e/e2/gone.md"
lse sync >/dev/null
clie sync >/dev/null
check "edit arrives at the CLI" test "$(clie_cat e2/x.md)" == "mine, edited"
check "move arrives at the CLI" test "$(clie_cat e2/moved/move.md)" == "move me"
check "delete arrives at the CLI" bash -c "! node '$CLI_JS' '$W/clidb-e' --settings '$W/cli-e.json' cat e2/gone.md >/dev/null 2>&1"
check "still everything sealed" all_sealed "$DBE"
fi

if want E3; then
log "E3 concurrent edits in the encrypted database"
mkdir -p "$W/vault-e/e3"
printf 'base\n' > "$W/vault-e/e3/c.md"
lse sync >/dev/null
clie sync >/dev/null
printf 'cli edit\n' | clie put e3/c.md >/dev/null
clie sync >/dev/null
printf 'lsync edit\n' > "$W/vault-e/e3/c.md"
lse sync >/dev/null
check "lsync kept its local edit" file_is "$W/vault-e/e3/c.md" "lsync edit"
check "lsync reports the conflict" bash -c \
  "LSYNC_DB=$DBE LSYNC_VAULT=$W/vault-e LSYNC_STATE_DIR=$W/state-e LSYNC_CONFIG=$W/e.toml '$BIN' status --json 2>/dev/null | grep -q 'e3/c.md'"
clie sync >/dev/null
line=$(clie_ls_line e3/c.md)
check "the CLI lists e3/c.md as conflicted" test "${line: -1}" == "*"
check "the branch lsync wrote is sealed too" all_sealed "$DBE"
rev=$(clie info e3/c.md | sed -n '/^{/,$p' | jfield '["revision"]' | tr -d '"')
clie resolve e3/c.md "$rev" >/dev/null
clie sync >/dev/null
lse sync >/dev/null
check "the CLI's resolution comes down decrypted" file_is "$W/vault-e/e3/c.md" "$(clie_cat e3/c.md)"
fi

if want E4; then
log "E4 a wrong passphrase on either side"
# What lsync could write besides documents: the milestone and the sync
# parameters (`_local`, which update_seq does not count).
e_locals() { for n in obsydian_livesync_milestone obsidian_livesync_sync_parameters; do couch "$LSYNC_URL/$DBE/_local/$n"; echo; done; }
# Every file in a vault folder (lsync's own lock files aside) with its size,
# mtime and content hash.
tree_sum() { (cd "$1" && find . -type f ! -name '.lsync.lock' ! -name '.lsync.holder' -printf '%P %s %T@\n' | sort \
  && find . -type f ! -name '.lsync.lock' ! -name '.lsync.holder' -exec sha256sum {} + | sort); }
# The vault is not empty: a file the remote has too (other content), and one of its own.
mkdir -p "$W/vault-e4/e" "$W/vault-e4/e4"
printf 'a local copy, older\n' > "$W/vault-e4/e/init.md"
printf 'only here\n' > "$W/vault-e4/e4/local.md"
tree_before=$(tree_sum "$W/vault-e4")
seq_before=$(couch "$LSYNC_URL/$DBE" | jfield '["update_seq"]')
locals_before=$(e_locals)
for how in sync fetch; do
  rc=0
  LSYNC_DB=$DBE LSYNC_VAULT=$W/vault-e4 LSYNC_STATE_DIR=$W/state-e4 LSYNC_CONFIG=$W/e4.toml LSYNC_DEVICE_NAME=interop-e4 \
    LSYNC_PASSPHRASE="wrong-$RUN" "$BIN" "$how" >/dev/null 2>"$W/lsync-e4.log" || rc=$?
  check "lsync $how refuses a wrong passphrase (exit $rc)" test "$rc" -eq 2
  check "  ... saying so" grep -q "does not decrypt" "$W/lsync-e4.log"
  check "  ... leaving the vault's files as they were" test "$(tree_sum "$W/vault-e4")" == "$tree_before"
  check "  ... writing no document to the database" test "$(couch "$LSYNC_URL/$DBE" | jfield '["update_seq"]')" == "$seq_before"
  check "  ... nor its milestone or sync parameters" test "$(e_locals)" == "$locals_before"
  check "  ... and never prints the passphrase" bash -c "! grep -q -- 'wrong-$RUN' '$W/lsync-e4.log'"
done
# The CLI with another passphrase cannot read what lsync wrote. It does not
# say why (`cat` just fails), so the other causes are ruled out: it did
# replicate (the file's metadata, which is not encrypted, is listed), and the
# same steps with the right passphrase on a fresh CLI database read it.
cli_settings "$W/cli-defaults.json" "$W/cli-e4.json" "$DBE" "{$E2EE_TWEAKS, \"passphrase\": \"wrong-$RUN\"}"
mkdir -p "$W/clidb-e4" "$W/clidb-e4ok"
cli4() { node "$CLI_JS" "$1" --settings "$2" "${@:3}" 2>>"$W/cli-e4.log"; }
cli4 "$W/clidb-e4" "$W/cli-e4.json" sync >/dev/null || true
got=$(cli4 "$W/clidb-e4" "$W/cli-e4.json" cat e/init.md | sed '/^\[CLIWatchAdapter\]/d' || true)
check "the CLI with a wrong passphrase does not get the content" test "$got" != "encrypted init"
check "  ... though it replicated the database (it lists the file)" bash -c \
  "node '$CLI_JS' '$W/clidb-e4' --settings '$W/cli-e4.json' ls e/ 2>/dev/null | grep -q '^e/init.md'"
cli4 "$W/clidb-e4ok" "$W/cli-e.json" sync >/dev/null || true
got=$(cli4 "$W/clidb-e4ok" "$W/cli-e.json" cat e/init.md | sed '/^\[CLIWatchAdapter\]/d' || true)
check "  ... and the right passphrase, the same way, reads it" test "$got" == "encrypted init"
fi

if want E5; then
log "E5 lsync is the first writer into an encrypted database without a salt"
DBF="$DB-e2ee-first"
couch -X PUT "$LSYNC_URL/$DBF" >/dev/null
cli_settings "$W/cli-defaults.json" "$W/cli-f.json" "$DBF" "{$E2EE_TWEAKS, \"passphrase\": \"$E2EE_PASS\"}"
mkdir -p "$W/clidb-f" "$W/clidb-f2" "$W/vault-f"
node "$CLI_JS" "$W/clidb-f" --settings "$W/cli-f.json" sync >/dev/null 2>>"$W/cli.log"
# The CLI set the database up; take its salt away, as if no device had made one.
prev=$(couch "$LSYNC_URL/$DBF/_local/obsidian_livesync_sync_parameters" | jfield '.get("_rev")' | tr -d '"')
[[ "$prev" != null ]] && couch -X DELETE "$LSYNC_URL/$DBF/_local/obsidian_livesync_sync_parameters?rev=$prev" >/dev/null
docs=$(couch "$LSYNC_URL/$DBF/_all_docs" | python3 -c 'import json,sys; print(sum(1 for r in json.load(sys.stdin)["rows"] if r["id"].startswith("h:")))')
log "    chunks before lsync: $docs; salt: $(couch -o /dev/null -w '%{http_code}' "$LSYNC_URL/$DBF/_local/obsidian_livesync_sync_parameters")"
lsf() { local rc=0; LSYNC_DB=$DBF LSYNC_VAULT=$W/vault-f LSYNC_STATE_DIR=$W/state-f LSYNC_CONFIG=$W/f.toml \
        LSYNC_DEVICE_NAME=interop-f LSYNC_PASSPHRASE=$E2EE_PASS "$BIN" "$@" 2>>"$W/lsync-f.log" || rc=$?; [[ $rc == 0 || $rc == 3 ]] || return $rc; }
rc=0
lsf pull >/dev/null 2>&1 || rc=$?
check "a read-only pull refuses to create the salt (exit $rc)" test "$rc" -eq 2
mkdir -p "$W/vault-f/e5"
printf 'first writer æøå\n' > "$W/vault-f/e5/first.md"
gen "$W/vault-f/e5/first.bin" bin 300000 35
lsf sync >/dev/null
check "lsync created the salt (32 bytes)" bash -c \
  "curl -sS -u '$LSYNC_USER:$LSYNC_PASSWORD' '$LSYNC_URL/$DBF/_local/obsidian_livesync_sync_parameters' | python3 -c 'import json,sys,base64; d=json.load(sys.stdin); sys.exit(0 if d[\"type\"]==\"sync-parameters\" and len(base64.b64decode(d[\"pbkdf2salt\"]))==32 else 1)'"
check "what lsync wrote is sealed" all_sealed "$DBF"
node "$CLI_JS" "$W/clidb-f2" --settings "$W/cli-f.json" sync >/dev/null 2>>"$W/cli.log"
got=$(node "$CLI_JS" "$W/clidb-f2" --settings "$W/cli-f.json" cat e5/first.md 2>>"$W/cli.log" | sed '/^\[CLIWatchAdapter\]/d')
check "a new CLI device reads lsync's note" test "$got" == "first writer æøå"
node "$CLI_JS" "$W/clidb-f2" --settings "$W/cli-f.json" pull e5/first.bin "$W/out-f.bin" >/dev/null 2>>"$W/cli.log"
check "  ... and its binary, byte-identical" same_file "$W/vault-f/e5/first.bin" "$W/out-f.bin"
fi

if want E6; then
log "E6 plaintext content in the encrypted database"
# What a device without encryption would write: a plain chunk and a note on it.
couch -X PUT "$LSYNC_URL/$DBE/h:e6plainchunk" -H 'Content-Type: application/json' -d '{"type":"leaf","data":"in the clear"}' >/dev/null
couch -X PUT "$LSYNC_URL/$DBE/e6%2Fleak.md" -H 'Content-Type: application/json' \
  -d '{"path":"e6/leak.md","type":"plain","children":["h:e6plainchunk"],"ctime":1,"mtime":1,"size":12,"eden":{}}' >/dev/null
rc=0
LSYNC_DB=$DBE LSYNC_VAULT=$W/vault-e LSYNC_STATE_DIR=$W/state-e LSYNC_CONFIG=$W/e.toml LSYNC_DEVICE_NAME=interop-e \
  LSYNC_PASSPHRASE=$E2EE_PASS "$BIN" sync >/dev/null 2>"$W/lsync-e6.log" || rc=$?
check "lsync stops at it (exit $rc)" test "$rc" -eq 2
check "  ... naming the policy" grep -q "unencrypted chunk content" "$W/lsync-e6.log"
check "  ... and does not write it" gone "$W/vault-e/e6/leak.md"
st=$(LSYNC_DB=$DBE LSYNC_VAULT=$W/vault-e LSYNC_STATE_DIR=$W/state-e LSYNC_CONFIG=$W/e.toml "$BIN" status --json 2>/dev/null || true)
check "  ... status shows the stop" test "$(echo "$st" | jfield '.get("upstream_stop") is not None')" == true
clie sync >/dev/null 2>&1 || true
log "    the official CLI's view of it: $(clie_cat e6/leak.md 2>/dev/null | head -c 60 || true)"
rev=$(edoc e6/leak.md | jfield '["_rev"]' | tr -d '"')
couch -X DELETE "$LSYNC_URL/$DBE/e6%2Fleak.md?rev=$rev" >/dev/null
lse sync >/dev/null
st=$(LSYNC_DB=$DBE LSYNC_VAULT=$W/vault-e LSYNC_STATE_DIR=$W/state-e LSYNC_CONFIG=$W/e.toml "$BIN" status --json 2>/dev/null || true)
check "once it is gone, the pull gets past it and the stop lifts" test "$(echo "$st" | jfield '.get("upstream_stop") is None')" == true
fi

if want E7; then
log "E7 a file whose chunk is damaged on the server is held"
printf 'held, readable\n' | clie put e7/held.md >/dev/null
printf 'other\n' | clie put e7/other.md >/dev/null
clie sync >/dev/null
lse sync >/dev/null
check "the file arrives" file_is "$W/vault-e/e7/held.md" "held, readable"
printf 'held, then damaged on the server\n' | clie put e7/held.md >/dev/null
clie sync >/dev/null
# One character of the new revision's chunk changed in place.
cid=$(edoc e7/held.md | jfield '["children"][0]' | tr -d '"')
cid_url=$(python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1],safe=""))' "$cid")
couch "$LSYNC_URL/$DBE/$cid_url" | python3 -c '
import json, sys
d = json.load(sys.stdin); s = d["data"]; i = len(s) - 6
d["data"] = s[:i] + ("B" if s[i] == "A" else "A") + s[i + 1:]
print(json.dumps(d))' | couch -X PUT "$LSYNC_URL/$DBE/$cid_url" -H 'Content-Type: application/json' -d @- >/dev/null
held_rev=$(edoc e7/held.md | jfield '["_rev"]')
printf 'written after\n' | clie put e7/after.md >/dev/null
clie sync >/dev/null
lse sync >/dev/null
check "the held file is left as it was on disk" file_is "$W/vault-e/e7/held.md" "held, readable"
st=$(LSYNC_DB=$DBE LSYNC_VAULT=$W/vault-e LSYNC_STATE_DIR=$W/state-e LSYNC_CONFIG=$W/e.toml "$BIN" status --json 2>/dev/null || true)
check "  ... and status lists it as held" bash -c "echo '$(echo "$st" | jfield '["pending"]')' | grep -q 'e7/held.md'"
check "other files keep coming down" file_is "$W/vault-e/e7/after.md" "written after"
printf 'edited here\n' > "$W/vault-e/e7/held.md"
printf 'mine\n' > "$W/vault-e/e7/mine.md"
lse sync >/dev/null
check "a local edit of the held file is not pushed" test "$(edoc e7/held.md | jfield '["_rev"]')" == "$held_rev"
check "  ... nor a branch made of it" test "$(edoc e7/held.md '?conflicts=true' | jfield '.get("_conflicts", [])')" == "[]"
check "  ... and it stays on disk" file_is "$W/vault-e/e7/held.md" "edited here"
clie sync >/dev/null
check "other files keep going up" test "$(clie_cat e7/mine.md)" == "mine"
fi

[[ $FAIL -eq 0 ]]
