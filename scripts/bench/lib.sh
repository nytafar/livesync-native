# Shared helpers for the benchmark scripts (source this; never run directly).
#
# Every write goes to the dev container (127.0.0.1:8411). The real CouchDB
# (8410) is only ever read, and only by snapshot.sh.
set -euo pipefail
BENCH_REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck source=/dev/null
source "$BENCH_REPO/scripts/dev-env.sh"
case "$LSYNC_URL" in
  http://127.0.0.1:8411*|http://localhost:8411*) ;;
  *) echo "refusing: benchmarks run only against the dev container on 127.0.0.1:8411" >&2; exit 1 ;;
esac
export DEV_CONTAINER=${DEV_CONTAINER:-lsync-dev-couchdb}
export SNAP_DB=${SNAP_DB:-bench-snap}
export CLI_JS=${LSYNC_CLI_JS:-$HOME/code/ref/obsidian-livesync/src/apps/cli/dist/index.cjs}
export BIN=${LSYNC_BIN:-$BENCH_REPO/target/release/lsync}
# Work area on disk (not /tmp, which is tmpfs here: vault writes would land in RAM).
export BENCH_WORK=${BENCH_WORK:-$HOME/scratch/lsync-bench}
# The phone's folder scope (finding-m2b-phone-run).
export SCOPE_EXCLUDE=${SCOPE_EXCLUDE:-inputs/corpora,inputs/media,contexts/myrvann,efforts/nyta-seo}
# The same scope as an upstream syncIgnoreRegEx (patterns joined by `|[]|`).
# Upstream matches these case-sensitively against the vault path; lsync's
# folder scope is case-insensitive. The vault has no case variants of these.
export SCOPE_IGNORE_REGEX='^inputs/corpora/|[]|^inputs/media/|[]|^contexts/myrvann/|[]|^efforts/nyta-seo/'
# Ignore rule both clients get on top of the scope (dev-env.sh sets the same for lsync).
export BASE_IGNORE_REGEX='\/\.git\/'

couch() { curl -sS -u "$LSYNC_USER:$LSYNC_PASSWORD" "$@"; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(eval("d"+sys.argv[1]))' "$1"; }

# Bytes the dev CouchDB container has sent / received on its interface.
# Only the client under test talks to the container during a measured run,
# so the delta is that client's network traffic (HTTP payload plus TCP/IP).
net_bytes() { docker exec "$DEV_CONTAINER" cat /sys/class/net/eth0/statistics/tx_bytes /sys/class/net/eth0/statistics/rx_bytes | paste -sd' '; }
# CPU time (µs) the dev CouchDB container has used: the server-side cost of a run.
server_cpu_us() {
  local id; id=$(docker inspect -f '{{.Id}}' "$DEV_CONTAINER")
  local f
  for f in /sys/fs/cgroup/system.slice/docker-$id.scope/cpu.stat /sys/fs/cgroup/docker/$id/cpu.stat; do
    [[ -r $f ]] && { awk '$1=="usage_usec"{print $2}' "$f"; return; }
  done
  echo 0
}

# clone_db <target>: drop <target> and recreate it as an exact copy of $SNAP_DB
# (documents by replication without checkpoints, plus the LiveSync _local docs).
clone_db() {
  local dst=$1
  [[ "$dst" == bench-* && "$dst" != "$SNAP_DB" ]] || { echo "refusing to clone into $dst" >&2; return 1; }
  couch -X DELETE "$LSYNC_URL/$dst" >/dev/null || true
  local body
  body=$(python3 - "$SNAP_DB" "$dst" <<'PY'
import json, os, sys, base64
auth = base64.b64encode(f"{os.environ['LSYNC_USER']}:{os.environ['LSYNC_PASSWORD']}".encode()).decode()
ep = lambda db: {"url": f"http://127.0.0.1:5984/{db}", "headers": {"Authorization": "Basic " + auth}}
print(json.dumps({"source": ep(sys.argv[1]), "target": ep(sys.argv[2]), "create_target": True,
                  "use_checkpoints": False, "worker_processes": 8, "worker_batch_size": 2000}))
PY
)
  local out
  out=$(couch -m 3600 -X POST -H 'Content-Type: application/json' "$LSYNC_URL/_replicate" --data-binary @- <<<"$body")
  [[ "$(jget '.get("ok")' <<<"$out")" == True ]] || { echo "clone into $dst failed: $out" >&2; return 1; }
  local id
  for id in obsydian_livesync_milestone obsidian_livesync_sync_parameters; do
    local doc
    doc=$(couch "$LSYNC_URL/$SNAP_DB/_local/$id")
    if [[ "$(jget '.get("error")' <<<"$doc")" == None ]]; then
      python3 -c 'import json,sys; d=json.load(sys.stdin); d.pop("_rev",None); print(json.dumps(d))' <<<"$doc" |
        couch -X PUT -H 'Content-Type: application/json' "$LSYNC_URL/$dst/_local/$id" --data-binary @- >/dev/null
    fi
  done
  local a b
  a=$(couch "$LSYNC_URL/$SNAP_DB" | jget '["doc_count"]')
  b=$(couch "$LSYNC_URL/$dst" | jget '["doc_count"]')
  [[ "$a" == "$b" ]] || { echo "clone $dst has $b docs, snapshot has $a" >&2; return 1; }
}

# The official CLI's settings file for database $1, written with mode 0600.
# $2: "live" for daemon mode. The scope is the phone's, as syncIgnoreRegEx.
cli_settings() {
  local db=$1 mode=${2:-} file=$3
  ( umask 077; node "$CLI_JS" init-settings --force "$file" >/dev/null 2>&1 )
  chmod 600 "$file"
  SETTINGS="$file" DBN="$db" MODE="$mode" node -e '
const fs = require("fs"); const p = process.env.SETTINGS; const d = JSON.parse(fs.readFileSync(p, "utf8"));
Object.assign(d, { couchDB_URI: process.env.LSYNC_URL, couchDB_USER: process.env.LSYNC_USER,
  couchDB_PASSWORD: process.env.LSYNC_PASSWORD, couchDB_DBNAME: process.env.DBN, remoteType: "",
  isConfigured: true, liveSync: process.env.MODE === "live", syncOnStart: false, syncOnSave: false,
  usePluginSync: false,
  syncIgnoreRegEx: process.env.SCOPE_IGNORE_REGEX + "|[]|" + process.env.BASE_IGNORE_REGEX });
fs.writeFileSync(p, JSON.stringify(d, null, 2), { mode: 0o600 });'
}

# The plug-in runs in a browser context (origin app://obsidian.md), so the dev
# server needs the same CORS settings as the real one. Idempotent.
dev_cors() {
  local kv
  for kv in chttpd/enable_cors=true httpd/enable_cors=true cors/credentials=true \
            'cors/origins=app://obsidian.md,capacitor://localhost,http://localhost'; do
    couch -X PUT "$LSYNC_URL/_node/_local/_config/${kv%%=*}" -d "\"${kv#*=}\"" >/dev/null
  done
}
