#!/usr/bin/env bash
# Run the benchmark (see bench.py for what it measures). Examples:
#   scripts/bench/run.sh                              # 3 reps x lsync, official CLI, plug-in
#   scripts/bench/run.sh --contenders lsync --reps 1 --idle 60
# Prerequisites: scripts/bench/snapshot.sh (once), node + the official CLI built,
# for the plug-in: main.js built in the official repo (npm run build) and an
# extracted Obsidian AppImage at $OBSIDIAN_DIR/squashfs-root (default
# ~/scratch/lsync-bench/obsidian), for example:
#   mkdir -p ~/scratch/lsync-bench/obsidian && cd ~/scratch/lsync-bench/obsidian
#   curl -LO https://github.com/obsidianmd/obsidian-releases/releases/download/v1.14.4/Obsidian-1.14.4.AppImage
#   chmod +x Obsidian-1.14.4.AppImage && ./Obsidian-1.14.4.AppImage --appimage-extract
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/bench/lib.sh
cargo build --release -q -p lsync
scripts/bench/install-unit.sh >/dev/null
dev_cors
mkdir -p "$BENCH_WORK"
exec python3 scripts/bench/bench.py "$@"
