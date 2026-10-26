#!/usr/bin/env bash
# Interop scenarios (spec M2) between lsync and the official LiveSync CLI,
# against a throwaway database on the dev CouchDB container (never 8410).
#
#   scripts/interop.sh            # build lsync, run every scenario, drop the database
#   KEEP=1 scripts/interop.sh     # keep the database and the work folder
#   ONLY="S1 S4" scripts/interop.sh
#
# Needs: node, the official CLI built at $LSYNC_CLI_JS (default
# ~/code/ref/obsidian-livesync/src/apps/cli/dist/index.cjs), python3, curl,
# and ~/.config/livesync-android/dev.env (see scripts/dev-env.sh).
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
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$PWD
# shellcheck source=/dev/null
source scripts/dev-env.sh
case "$LSYNC_URL" in
  http://127.0.0.1:8411*|http://localhost:8411*) ;;
  *) echo "refusing: interop runs only against the dev container on 127.0.0.1:8411" >&2; exit 1 ;;
esac
CLI_JS=${LSYNC_CLI_JS:-$HOME/code/ref/obsidian-livesync/src/apps/cli/dist/index.cjs}
[[ -f "$CLI_JS" ]] || { echo "official CLI not found at $CLI_JS" >&2; exit 1; }
cargo build --release -q -p lsync
BIN=$REPO/target/release/lsync

RUN="$(date +%s)-$RANDOM"
DB="lsync-it-$RUN"
W=$(mktemp -d /tmp/lsync-it.XXXXXX)
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

CLI_VERSION=$(node -p 'require(process.argv[1]).version' "$(dirname "$CLI_JS")/../package.json" 2>/dev/null || echo '?')
CLI_REF=$(git -C "$(dirname "$CLI_JS")" rev-parse --short HEAD 2>/dev/null || echo '?')
log "== interop run $RUN: lsync $(git rev-parse --short HEAD) vs official CLI $CLI_VERSION (ref $CLI_REF), database $DB"
couch -X PUT "$LSYNC_URL/$DB" >/dev/null

# --- setup: the CLI initialises the database with our setup's tweaks -----------
node "$CLI_JS" init-settings --force "$W/cli.json" >>"$W/cli.log" 2>&1
SETTINGS="$W/cli.json" DBN="$DB" node -e '
const fs = require("fs"); const p = process.env.SETTINGS; const d = JSON.parse(fs.readFileSync(p, "utf8"));
Object.assign(d, { couchDB_URI: process.env.LSYNC_URL, couchDB_USER: process.env.LSYNC_USER,
  couchDB_PASSWORD: process.env.LSYNC_PASSWORD, couchDB_DBNAME: process.env.DBN, remoteType: "",
  customChunkSize: 60, isConfigured: true, liveSync: false, syncOnStart: false, syncOnSave: false,
  usePluginSync: false });
fs.writeFileSync(p, JSON.stringify(d, null, 2), { mode: 0o600 });'
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
wait_for() { # wait_for <seconds> <description> <command...>
  local t=$1 what=$2; shift 2
  local start=$SECONDS
  while (( SECONDS - start < t )); do
    if "$@" 2>/dev/null; then ok "$what ($((SECONDS - start)) s)"; return 0; fi
    sleep 0.5
  done
  bad "$what (timed out after $t s)"
}
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

[[ $FAIL -eq 0 ]]
