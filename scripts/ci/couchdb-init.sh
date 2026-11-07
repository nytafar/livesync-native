#!/usr/bin/env bash
# Configure a fresh, throwaway CouchDB the way Self-hosted LiveSync expects
# (single node, a valid user required, large documents and requests), as
# upstream's own CLI tests do (src/apps/cli/util/couchdb-init.sh). For the CI
# service container of the weekly upstream job; never run it against a
# CouchDB you keep.
#
#   COUCHDB_URL=http://127.0.0.1:8411 COUCHDB_USER=... COUCHDB_PASSWORD=... scripts/ci/couchdb-init.sh
set -euo pipefail
URL=${COUCHDB_URL:?COUCHDB_URL}
USER=${COUCHDB_USER:?COUCHDB_USER}
PASS=${COUCHDB_PASSWORD:?COUCHDB_PASSWORD}
case "$URL" in
  *:8410*) echo "refusing: port 8410 is the real CouchDB" >&2; exit 1 ;;
esac

req() { curl -4 -sS --fail --connect-timeout 3 --max-time 15 -u "$USER:$PASS" "$@"; }

for _ in $(seq 1 60); do
  req -o /dev/null "$URL/_up" 2>/dev/null && break
  sleep 2
done
req -o /dev/null "$URL/_up"

req -X POST "$URL/_cluster_setup" -H 'Content-Type: application/json' \
  -d "{\"action\":\"enable_single_node\",\"username\":\"$USER\",\"password\":\"$PASS\",\"bind_address\":\"0.0.0.0\",\"port\":5984,\"singlenode\":true}" >/dev/null
set_config() { req -X PUT "$URL/_node/_local/_config/$1" -H 'Content-Type: application/json' -d "$2" >/dev/null; }
set_config chttpd/require_valid_user '"true"'
set_config chttpd_auth/require_valid_user '"true"'
set_config httpd/WWW-Authenticate '"Basic realm=\"couchdb\""'
set_config chttpd/max_http_request_size '"4294967296"'
set_config couchdb/max_document_size '"50000000"'
echo "couchdb-init: $(req "$URL/" | head -c 120)"
