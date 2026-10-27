#!/usr/bin/env bash
# Install (not enable) the lsync-bench user unit, pointing at this checkout's release build.
#
#   scripts/bench/install-unit.sh
#   systemctl --user start lsync-bench      # run it
#   systemctl --user stop lsync-bench       # stop it
#   systemctl --user enable lsync-bench     # only if it should start at login
#
# Configure the profile first, e.g.:
#   source scripts/dev-env.sh
#   target/release/lsync --profile bench configure --db <dev-db> --vault <folder> \
#     --exclude Archive,Attachments
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
UNIT_DIR=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
mkdir -p "$UNIT_DIR"
sed "s#@BIN@#$REPO/target/release/lsync#" "$REPO/scripts/bench/lsync-bench.service.in" >"$UNIT_DIR/lsync-bench.service"
systemctl --user daemon-reload
echo "installed $UNIT_DIR/lsync-bench.service (not enabled)"
