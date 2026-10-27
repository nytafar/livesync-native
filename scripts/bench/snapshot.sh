#!/usr/bin/env bash
# One-time: copy the real `vault` database into $SNAP_DB on the dev container.
#
#   scripts/bench/snapshot.sh            # refuses if $SNAP_DB exists
#   FORCE=1 scripts/bench/snapshot.sh    # drop and re-take it
#
# The real CouchDB is only read: the dev container pulls from it by replication
# with use_checkpoints=false (so no checkpoint docs are written to the source),
# and the LiveSync _local docs (not replicated by CouchDB) are copied with GETs.
# Its credentials are read from inside its container and never printed.
source "$(dirname "$0")/lib.sh"
REAL_CONTAINER=${REAL_CONTAINER:-obsidian-livesync-couchdb}
REAL_DB=${REAL_DB:-vault}
# How the dev container reaches the real server (published port via the docker0 gateway).
REAL_FROM_DEV=${REAL_FROM_DEV:-http://172.17.0.1:8410}
REAL_FROM_HOST=${REAL_FROM_HOST:-http://127.0.0.1:8410}

if [[ "$(couch -o /dev/null -w '%{http_code}' "$LSYNC_URL/$SNAP_DB")" == 200 ]]; then
  [[ "${FORCE:-0}" == 1 ]] || { echo "$SNAP_DB exists; FORCE=1 to re-take it" >&2; exit 1; }
  couch -X DELETE "$LSYNC_URL/$SNAP_DB" >/dev/null
fi

REAL_AUTH=$(docker exec "$REAL_CONTAINER" sh -c 'printf %s "$COUCHDB_USER:$COUCHDB_PASSWORD"' | base64 -w0)
real_get() { curl -sS -H "Authorization: Basic $REAL_AUTH" "$REAL_FROM_HOST/$REAL_DB$1"; }

echo "source before: $(real_get '' | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["doc_count"], "docs, update_seq", d["update_seq"][:24])')"
body=$(REAL_AUTH=$REAL_AUTH python3 - "$REAL_FROM_DEV/$REAL_DB" "$SNAP_DB" <<'PY'
import json, os, sys, base64
dev = base64.b64encode(f"{os.environ['LSYNC_USER']}:{os.environ['LSYNC_PASSWORD']}".encode()).decode()
print(json.dumps({
  "source": {"url": sys.argv[1], "headers": {"Authorization": "Basic " + os.environ["REAL_AUTH"]}},
  "target": {"url": f"http://127.0.0.1:5984/{sys.argv[2]}", "headers": {"Authorization": "Basic " + dev}},
  "create_target": True, "use_checkpoints": False, "worker_processes": 8, "worker_batch_size": 2000}))
PY
)
start=$(date +%s)
out=$(couch -m 7200 -X POST -H 'Content-Type: application/json' "$LSYNC_URL/_replicate" --data-binary @- <<<"$body")
echo "replication: $(python3 -c 'import json,sys; d=json.load(sys.stdin); h=(d.get("history") or [{}])[0]; print(d.get("ok"), d.get("error",""), h.get("docs_written"), "docs written,", h.get("doc_write_failures"), "failures")' <<<"$out") in $(( $(date +%s) - start )) s"

for id in obsydian_livesync_milestone obsidian_livesync_sync_parameters; do
  doc=$(real_get "/_local/$id")
  if [[ "$(jget '.get("error")' <<<"$doc")" == None ]]; then
    python3 -c 'import json,sys; d=json.load(sys.stdin); d.pop("_rev",None); print(json.dumps(d))' <<<"$doc" |
      couch -X PUT -H 'Content-Type: application/json' "$LSYNC_URL/$SNAP_DB/_local/$id" --data-binary @- >/dev/null
    echo "copied _local/$id"
  fi
done
echo "source after:  $(real_get '' | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["doc_count"], "docs, update_seq", d["update_seq"][:24])')"
echo "snapshot:      $(couch "$LSYNC_URL/$SNAP_DB" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["doc_count"], "docs,", d["doc_del_count"], "deleted, update_seq", d["update_seq"][:24], ", sizes", d["sizes"])')"
echo "taken at $(date -Iseconds)"
