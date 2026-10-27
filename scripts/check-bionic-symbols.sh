#!/usr/bin/env bash
# Check that an arm64 Android ELF only needs what the platform has at a
# given API level: every undefined, non-weak dynamic symbol must be defined by
# one of its NEEDED libraries' NDK stubs for that level (libc, libm, libdl,
# liblog, ...), and every NEEDED library must exist at that level. A symbol
# or library that only exists from a later level would make dlopen fail on
# the older device. Two loader features are checked as well:
#   - a PT_TLS segment (ELF TLS) needs API 29; below it the NDK uses
#     emulated TLS and the loader refuses the library;
#   - plain DT_RELR (packed relative relocations) needs API 30; below it only
#     DT_ANDROID_RELR (lld --use-android-relr-tags) is understood, from API 28.
#
# Usage: scripts/check-bionic-symbols.sh <lib.so or binary> <api level>
# Needs ANDROID_NDK_HOME (default: the newest NDK under ~/Android/Sdk/ndk).
set -euo pipefail
elf=${1:?usage: $0 <elf> <api>}
api=${2:?usage: $0 <elf> <api>}
if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
  ANDROID_NDK_HOME=$(ls -d "$HOME"/Android/Sdk/ndk/* 2>/dev/null | sort -V | tail -1)
fi
tc=$(ls -d "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/* | head -1)
nm="$tc/bin/llvm-nm"
readelf="$tc/bin/llvm-readelf"
stubs="$tc/sysroot/usr/lib/aarch64-linux-android/$api"
[[ -d "$stubs" ]] || { echo "no NDK stubs for API $api under $stubs" >&2; exit 1; }

fail=0
if (( api < 29 )) && "$readelf" -lW "$elf" | awk '$1 == "TLS" {found = 1} END {exit !found}'; then
  echo "$(basename "$elf") has a PT_TLS segment (ELF TLS), which needs API 29; API $api has only emulated TLS" >&2
  fail=1
fi
# DT_RELR is tag 0x24; DT_ANDROID_RELR (0x6fffe000) is fine from API 28.
if (( api < 30 )) && "$readelf" -dW "$elf" | awk '$1 ~ /^0x0*24$/ {found = 1} END {exit !found}'; then
  echo "$(basename "$elf") uses DT_RELR, which needs API 30; link with --use-android-relr-tags for API $api" >&2
  fail=1
fi
(( fail == 0 )) || exit 1

defined=$(mktemp)
trap 'rm -f "$defined"' EXIT
needed=$("$readelf" -d "$elf" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p')
for lib in $needed; do
  if [[ -f "$stubs/$lib" ]]; then
    "$nm" -D --defined-only "$stubs/$lib" | awk '{print $NF}' | sed 's/@.*//' >> "$defined"
  else
    # The loader needs every DT_NEEDED library, used or not; this check knows
    # only the platform's, so a bundled library has to be checked on its own.
    echo "$(basename "$elf") needs $lib, which API $api does not provide" >&2
    exit 1
  fi
done
sort -u -o "$defined" "$defined"

# Strong undefined symbols ("U"); weak ones ("w") may be absent at run time.
missing=$("$nm" -D --undefined-only "$elf" | awk '$1 == "U" {print $NF}' | sed 's/@.*//' | sort -u |
  comm -23 - "$defined")
count=$("$nm" -D --undefined-only "$elf" | awk '$1 == "U"' | wc -l)
weak_absent=$("$nm" -D --undefined-only "$elf" | awk '$1 == "w" {print $NF}' | sed 's/@.*//' | sort -u |
  comm -23 - "$defined")
[[ -z "$weak_absent" ]] || echo "note: weak symbols absent at API $api (resolved to null, the caller falls back):" $weak_absent
if [[ -n "$missing" ]]; then
  echo "$(basename "$elf") needs symbols that API $api does not have:" >&2
  echo "$missing" >&2
  exit 1
fi
echo "$(basename "$elf"): all $count undefined symbols exist at API $api ($(echo $needed))"
