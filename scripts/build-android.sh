#!/usr/bin/env bash
# Cross-compile lsync for aarch64-linux-android (Termux on the phone).
# Needs: rustup target add aarch64-linux-android; cargo install cargo-ndk;
# an NDK under $ANDROID_NDK_HOME (default: newest in ~/Android/Sdk/ndk).
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
  ANDROID_NDK_HOME=$(ls -d "$HOME"/Android/Sdk/ndk/* 2>/dev/null | sort -V | tail -1)
  export ANDROID_NDK_HOME
fi
[[ -d "$ANDROID_NDK_HOME" ]] || { echo "no NDK found; set ANDROID_NDK_HOME" >&2; exit 1; }
cargo ndk -t arm64-v8a -P 30 build --release -p lsync
out=target/aarch64-linux-android/release/lsync
file "$out"
echo "copy to the phone with: tailscale file cp $out <phone>:"
