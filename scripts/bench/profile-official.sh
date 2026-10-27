#!/usr/bin/env bash
# CPU-profile the official CLI's cold fetch for a few minutes (node --cpu-prof),
# on a fresh clone, then stop it with SIGINT (the CLI exits through
# process.exit, which writes the profile).
#
#   scripts/bench/profile-official.sh [seconds]     # default 360
#   python3 scripts/bench/cpuprofile_top.py $BENCH_WORK/profile-official/*.cpuprofile
set -euo pipefail
source "$(dirname "$0")/lib.sh"
SECS=${1:-360}
W=$BENCH_WORK/profile-official
rm -rf "${W:?}"
mkdir -p "$W/db" "$W/vault" "$W/prof"
clone_db bench-official
cli_settings bench-official "" "$W/cli.json"
python3 - "$W/cli.json" <<'PY'
import json, os, sys
p = sys.argv[1]; d = json.load(open(p)); d["customChunkSize"] = 60
json.dump(d, open(p, "w"), indent=2); os.chmod(p, 0o600)
PY
node --cpu-prof --cpu-prof-dir="$W/prof" "$CLI_JS" "$W/db" --settings "$W/cli.json" --vault "$W/vault" \
  fetch --force --write-settings >"$W/log" 2>&1 &
pid=$!
for _ in $(seq "$SECS"); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
kill -INT "$pid" 2>/dev/null || true
wait "$pid" || true
chmod 600 "$W/cli.json"
ls -la "$W/prof"
