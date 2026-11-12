#!/usr/bin/env bash
# The desktop testbed: throwaway LiveSync databases on the dev CouchDB
# container (127.0.0.1:8411), and simulated machines on this one. Each machine
# has its own vault folder, config home, state home and runtime dir under
# $TESTBED, so `lsync` (and later `lsync serve`) run as separate devices.
#
#   testbed.sh up                       start the dev container (restart policy unless-stopped)
#   testbed.sh snapshot [--force]       fresh copy of the real database into real-snap (read-only on
#                                       the real server, via scripts/bench/snapshot.sh) and a reflink
#                                       copy of the matching vault folder into $TESTBED/real-copy
#   testbed.sh new NAME [--from empty|snapshot] [--machines a,b] [--adopt M]
#                                       database tb-NAME and one folder per machine. `empty` (default)
#                                       is initialised by the official CLI of the pinned release
#                                       (upstream.env), with the path identity of databases set up
#                                       before 1.0.46 (pathNormalizationVersion 0), which lsync syncs;
#                                       `snapshot` clones real-snap.
#                                       --adopt M starts machine M's vault as a copy of real-copy (the
#                                       folder matching the snapshot)
#   testbed.sh run NAME M [ARGS...]     run $LSYNC_BIN (default target/release/lsync) as machine M
#   testbed.sh env NAME M               print the exports for machine M: eval "$(testbed.sh env t1 a)"
#   testbed.sh cli NAME [ARGS...]       the official CLI as a node of NAME (own PouchDB and vault)
#   testbed.sh status                   the container, testbed databases and folders
#   testbed.sh drop NAME                delete tb-NAME and its folders
#
# Safety: writes go only to the dev container, only to databases named tb-*,
# and only under $TESTBED. The real CouchDB (8410) and the source vault folder
# are only read, and only by `snapshot`.
#
# Machine-local settings (kept out of the repo) come from
# ~/.config/livesync-android/testbed.env (LSYNC_TESTBED_ENV overrides):
#   TESTBED_SOURCE_VAULT=<the desktop vault folder that matches the real database>
#   REAL_DB=<the real database>   REAL_CONTAINER=<its container>   (bench/snapshot.sh defaults)
set -euo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck source=/dev/null
source "$REPO/scripts/dev-env.sh"
TESTBED_ENV=${LSYNC_TESTBED_ENV:-$HOME/.config/livesync-android/testbed.env}
if [[ -f "$TESTBED_ENV" ]]; then set -a; . "$TESTBED_ENV"; set +a; fi
case "$LSYNC_URL" in
  http://127.0.0.1:8411 | http://127.0.0.1:8411/ | http://localhost:8411 | http://localhost:8411/) ;;
  *) echo "refusing: the testbed runs only against the dev container on 127.0.0.1:8411" >&2; exit 1 ;;
esac
DEV_URL=${LSYNC_URL%/} DEV_USER=$LSYNC_USER DEV_PASSWORD=$LSYNC_PASSWORD
DEV_CONTAINER=${DEV_CONTAINER:-lsync-dev-couchdb}
TESTBED=${TESTBED:-$HOME/scratch/lsync-testbed}
SNAP_DB=real-snap
COPY_DIR=$TESTBED/real-copy
REAL_VAULT_DIR=${TESTBED_SOURCE_VAULT:-}
BIN=${LSYNC_BIN:-$REPO/target/release/lsync}
# The official CLI of the pinned release (upstream.env), resolved (and built
# on first use) by scripts/upstream.sh when a command needs it; LSYNC_CLI_JS
# or LSYNC_UPSTREAM_TAG run another one, and it says so.
CLI_JS=
need_cli() { [[ -n "$CLI_JS" ]] || CLI_JS=$("$REPO/scripts/upstream.sh" cli) || die "no official CLI"; }

die() { echo "testbed: $*" >&2; exit 1; }
couch() { curl -sS -u "$DEV_USER:$DEV_PASSWORD" "$@"; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(eval("d"+sys.argv[1]))' "$1"; }
db_exists() { [[ "$(couch -o /dev/null -w '%{http_code}' "$DEV_URL/$1")" == 200 ]]; }
check_name() { [[ "$1" =~ ^[a-z0-9][a-z0-9-]{0,40}$ ]] || die "name must be lower-case letters, digits and dashes: $1"; }
check_machine() { [[ "$1" =~ ^[a-z0-9]{1,12}$ && "$1" != cli ]] || die "machine must be short lower-case letters or digits (not 'cli'): $1"; }
inside_testbed() { case "$(realpath -m "$1")" in "$(realpath -m "$TESTBED")"/*) ;; *) die "refusing a path outside $TESTBED: $1" ;; esac; }

# Copy the LiveSync _local docs (not replicated by CouchDB) from one dev database to another.
copy_local_docs() {
  local src=$1 dst=$2 id doc
  for id in obsydian_livesync_milestone obsidian_livesync_sync_parameters; do
    doc=$(couch "$DEV_URL/$src/_local/$id")
    if [[ "$(jget '.get("error")' <<<"$doc")" == None ]]; then
      python3 -c 'import json,sys; d=json.load(sys.stdin); d.pop("_rev",None); print(json.dumps(d))' <<<"$doc" |
        couch -X PUT -H 'Content-Type: application/json' "$DEV_URL/$dst/_local/$id" --data-binary @- >/dev/null
    fi
  done
}

cmd_up() {
  docker start "$DEV_CONTAINER" >/dev/null
  docker update --restart unless-stopped "$DEV_CONTAINER" >/dev/null
  local i
  for i in $(seq 60); do couch -f "$DEV_URL/_up" >/dev/null 2>&1 && break; sleep 1; done
  couch -f "$DEV_URL/_up" >/dev/null || die "the dev CouchDB did not come up"
  echo "dev CouchDB up at $DEV_URL (container $DEV_CONTAINER, max_document_size $(couch "$DEV_URL/_node/_local/_config/couchdb/max_document_size"))"
}

cmd_snapshot() {
  local force=0
  [[ "${1:-}" == --force ]] && force=1
  [[ -n "$REAL_VAULT_DIR" ]] || die "set TESTBED_SOURCE_VAULT (in $TESTBED_ENV) to the vault folder that matches the real database"
  [[ -d "$REAL_VAULT_DIR/.obsidian" ]] || die "$REAL_VAULT_DIR is not an Obsidian vault"
  if db_exists "$SNAP_DB" && [[ $force != 1 ]]; then die "$SNAP_DB exists; --force to re-take it"; fi
  mkdir -p "$TESTBED"
  # The database: the vetted benchmark script, which only reads the real server
  # (replication without checkpoints, plus GETs of the _local docs). Its scope
  # variables are for benchmark runs and unused here.
  SNAP_DB=$SNAP_DB FORCE=$force SCOPE_EXCLUDE=unused SCOPE_IGNORE_REGEX=unused "$REPO/scripts/bench/snapshot.sh"
  # The folder, right after: a reflink copy (instant on btrfs). The LiveSync
  # plugin's folder holds the real server's credentials and is left out.
  local tmp="$COPY_DIR.tmp"
  rm -rf -- "$tmp"
  cp -a --reflink=auto "$REAL_VAULT_DIR" "$tmp"
  rm -rf -- "$tmp/.obsidian/plugins/obsidian-livesync"
  rm -rf -- "$COPY_DIR"
  mv "$tmp" "$COPY_DIR"
  date -Iseconds >"$TESTBED/real-snap.taken"
  echo "real-copy: $(find "$COPY_DIR" -type f | wc -l) files, $(du -sh "$COPY_DIR" | cut -f1); taken $(cat "$TESTBED/real-snap.taken")"
}

write_machine() { # write_machine NAME M
  local name=$1 m=$2 dir="$TESTBED/$1/$2"
  mkdir -p "$dir/vault" "$dir/config/lsync" "$dir/state" "$dir/run"
  chmod 700 "$dir/config" "$dir/run"
  local cfg="$dir/config/lsync/default.toml"
  ( umask 077
    DB="tb-$name" VAULT="$dir/vault" STATE="$dir/state/lsync/default" DEV="tb-$name-$m" python3 - >"$cfg" <<'PY'
import json, os
s = lambda v: json.dumps(v)  # a JSON string is a valid TOML basic string
print(f'url = {s(os.environ["LSYNC_URL"].rstrip("/"))}')
print(f'database = {s(os.environ["DB"])}')
print(f'username = {s(os.environ["LSYNC_USER"])}')
print(f'password = {s(os.environ["LSYNC_PASSWORD"])}')
print(f'vault = {s(os.environ["VAULT"])}')
print(f'state_dir = {s(os.environ["STATE"])}')
print("ignore_regex = '\\/\\.git\\/'")
print(f'device_name = {s(os.environ["DEV"])}')
PY
  )
}

cli_node() { # cli_node NAME ARGS... : the official CLI on its own PouchDB and vault
  local dir="$TESTBED/$1/cli"; shift
  need_cli
  node "$CLI_JS" "$dir/db" --settings "$dir/settings.json" --vault "$dir/vault" "$@"
}

init_cli_node() { # init_cli_node NAME
  local name=$1 dir="$TESTBED/$1/cli"
  need_cli
  mkdir -p "$dir/db" "$dir/vault"
  node "$CLI_JS" init-settings --force "$dir/settings.json" >>"$dir/cli.log" 2>&1
  SETTINGS="$dir/settings.json" DBN="tb-$name" URL="$DEV_URL" node -e '
const fs = require("fs"); const p = process.env.SETTINGS; const d = JSON.parse(fs.readFileSync(p, "utf8"));
Object.assign(d, { couchDB_URI: process.env.URL, couchDB_USER: process.env.LSYNC_USER,
  couchDB_PASSWORD: process.env.LSYNC_PASSWORD, couchDB_DBNAME: process.env.DBN, remoteType: "",
  customChunkSize: 60, isConfigured: true, liveSync: false, syncOnStart: false, syncOnSave: false,
  usePluginSync: false, syncIgnoreRegEx: "\\/\\.git\\/", pathNormalizationVersion: 0 });
fs.writeFileSync(p, JSON.stringify(d, null, 2), { mode: 0o600 });'
}

cmd_new() {
  local name=${1:-}; [[ -n "$name" ]] || die "usage: new NAME [--from empty|snapshot] [--machines a,b] [--adopt M]"
  shift; check_name "$name"
  local from=empty machines=a,b adopt=
  while (($#)); do
    case $1 in
      --from) from=$2; shift 2 ;;
      --machines) machines=$2; shift 2 ;;
      --adopt) adopt=$2; shift 2 ;;
      *) die "unknown option $1" ;;
    esac
  done
  local db="tb-$name" dir="$TESTBED/$name"
  [[ ! -e "$dir" ]] || die "$dir exists; drop $name first"
  db_exists "$db" && die "$db exists; drop $name first"
  local m
  IFS=, read -ra ms <<<"$machines"
  for m in "${ms[@]}"; do check_machine "$m"; done
  [[ -z "$adopt" || ",$machines," == *",$adopt,"* ]] || die "--adopt $adopt is not one of the machines ($machines)"
  [[ -z "$adopt" || -d "$COPY_DIR" ]] || die "--adopt needs $COPY_DIR (run snapshot first)"
  mkdir -p "$dir"
  init_cli_node "$name"
  case $from in
    empty)
      couch -X PUT "$DEV_URL/$db" >/dev/null
      printf 'testbed %s\n' "$name" | cli_node "$name" put testbed/init.md >>"$dir/cli/cli.log" 2>&1
      cli_node "$name" sync >>"$dir/cli/cli.log" 2>&1
      [[ "$(couch "$DEV_URL/$db/_local/obsydian_livesync_milestone" | jget '.get("tweak_values",{}).get("PREFERRED",{}).get("customChunkSize")')" == 60 ]] ||
        die "the official CLI did not initialise $db (see $dir/cli/cli.log)"
      ;;
    snapshot)
      db_exists "$SNAP_DB" || die "$SNAP_DB does not exist (run snapshot first)"
      local body out
      body=$(SRC=$SNAP_DB DST=$db python3 - <<'PY'
import json, os, base64
auth = base64.b64encode(f"{os.environ['LSYNC_USER']}:{os.environ['LSYNC_PASSWORD']}".encode()).decode()
ep = lambda db: {"url": f"http://127.0.0.1:5984/{db}", "headers": {"Authorization": "Basic " + auth}}
print(json.dumps({"source": ep(os.environ["SRC"]), "target": ep(os.environ["DST"]), "create_target": True,
                  "use_checkpoints": False, "worker_processes": 8, "worker_batch_size": 2000}))
PY
)
      out=$(couch -m 7200 -X POST -H 'Content-Type: application/json' "$DEV_URL/_replicate" --data-binary @- <<<"$body")
      [[ "$(jget '.get("ok")' <<<"$out")" == True ]] || die "clone into $db failed: $out"
      copy_local_docs "$SNAP_DB" "$db"
      local a b
      a=$(couch "$DEV_URL/$SNAP_DB" | jget '["doc_count"]'); b=$(couch "$DEV_URL/$db" | jget '["doc_count"]')
      [[ "$a" == "$b" ]] || die "clone $db has $b docs, $SNAP_DB has $a"
      ;;
    *) die "--from must be empty or snapshot" ;;
  esac
  for m in "${ms[@]}"; do
    write_machine "$name" "$m"
    if [[ "$m" == "$adopt" ]]; then
      cp -a --reflink=auto "$COPY_DIR/." "$dir/$m/vault/"
    fi
  done
  echo "testbed $name: database $db ($(couch "$DEV_URL/$db" | jget '["doc_count"]') docs), machines ${machines}${adopt:+ (machine $adopt starts from real-copy)}, official CLI node 'cli'"
  echo "  run:  scripts/testbed/testbed.sh run $name ${ms[0]} sync"
}

machine_env() { # machine_env NAME M : exports for machine M, with no LSYNC_* leaking in
  local dir="$TESTBED/$1/$2"
  [[ -d "$dir" ]] || die "no machine $2 in testbed $1"
  local v
  for v in $(compgen -e | grep '^LSYNC_' || true); do printf 'unset %s\n' "$v"; done
  printf 'export XDG_CONFIG_HOME=%q XDG_STATE_HOME=%q LSYNC_RUNTIME_DIR=%q TESTBED_MACHINE=%q\n' \
    "$dir/config" "$dir/state" "$dir/run" "$1/$2"
}

cmd_run() {
  local name=${1:-} m=${2:-}; [[ -n "$name" && -n "$m" ]] || die "usage: run NAME MACHINE [ARGS...]"
  shift 2
  [[ -x "$BIN" ]] || die "$BIN not built (cargo build --release -p lsync)"
  eval "$(machine_env "$name" "$m")"
  exec "$BIN" "$@"
}

cmd_status() {
  echo "container: $(docker inspect -f '{{.State.Status}} (restart {{.HostConfig.RestartPolicy.Name}})' "$DEV_CONTAINER" 2>/dev/null || echo missing)"
  couch -f "$DEV_URL/_up" >/dev/null 2>&1 || { echo "dev CouchDB not reachable at $DEV_URL"; return; }
  [[ -f "$TESTBED/real-snap.taken" ]] && echo "real-snap and real-copy taken $(cat "$TESTBED/real-snap.taken")"
  local db
  for db in $(couch "$DEV_URL/_all_dbs" | python3 -c 'import json,sys; print(" ".join(d for d in json.load(sys.stdin) if d.startswith("tb-") or d == "real-snap"))'); do
    echo "  $db: $(couch "$DEV_URL/$db" | jget '["doc_count"]') docs"
  done
  local d
  for d in "$TESTBED"/*/; do
    [[ -d "$d" && "$(basename "$d")" != real-copy* ]] || continue
    echo "  folder $(basename "$d"): $(ls "$d" | paste -sd' ')"
  done
}

cmd_drop() {
  local name=${1:-}; [[ -n "$name" ]] || die "usage: drop NAME"
  check_name "$name"
  db_exists "tb-$name" && couch -X DELETE "$DEV_URL/tb-$name" >/dev/null
  if [[ -d "$TESTBED/$name" ]]; then inside_testbed "$TESTBED/$name"; rm -rf -- "${TESTBED:?}/$name"; fi
  echo "dropped $name"
}

case ${1:-} in
  up) shift; cmd_up "$@" ;;
  snapshot) shift; cmd_snapshot "$@" ;;
  new) shift; cmd_new "$@" ;;
  run) shift; cmd_run "$@" ;;
  env) shift; machine_env "$@" ;;
  cli) shift; name=${1:?usage: cli NAME [ARGS...]}; shift; cli_node "$name" "$@" ;;
  status) shift; cmd_status ;;
  drop) shift; cmd_drop "$@" ;;
  *) sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
