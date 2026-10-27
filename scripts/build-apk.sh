#!/usr/bin/env bash
# Build the signed sideload APK: the Rust core for arm64 Android (cargo-ndk),
# the Kotlin bindings (uniffi-bindgen), then Gradle: JVM unit tests (including
# the binding tests against a host build of the core), lint, assembleRelease.
#
# Output: dist/livesync-android-<version>.apk and its .sha256.
#
# Needs: rustup target aarch64-linux-android, cargo-ndk, an Android SDK
# (ANDROID_HOME, default ~/Android/Sdk) with platform 35, build-tools 35.0.0
# and an NDK; a JDK 17-21 (LIVESYNC_JAVA_HOME, default: mise's java@21).
#
# Signing: a sideload keystore outside the repo, created on first use:
#   ~/.config/livesync-android/sideload.jks         (mode 0600)
#   ~/.config/livesync-android/sideload.properties  (mode 0600; its passwords)
# Override the properties file with LIVESYNC_SIGNING=<path>. Under CI (CI=true)
# the keystore is never created: a missing properties file is an error.
#
# Version: LIVESYNC_VERSION_CODE and LIVESYNC_VERSION_NAME override the
# defaults (928; <crate version>-api28+<sha>). The release workflow sets both.
#
# Branch android9-minsdk28 (minSdk 28, for Android 9): the default versionCode
# is fixed at API28_VERSION_CODE, inside 900-999. Release codes are
# VERSION_CODE_BASE (1000, release.yml) plus the run number, so always 1001 or
# more: no release can share this code, a release (should one ever support
# Android 9) installs over this build as an ordinary upgrade, and this build
# never replaces an installed release (Android refuses the downgrade). Hand
# builds from main use the commit count (60 when this was set), so on an
# Android 11+ phone running a main hand build this build installs as an
# upgrade, and main hand builds are then refused until the app is uninstalled
# (losing its settings) or LIVESYNC_VERSION_CODE is set above 928. Raise the
# code within 900-999 if a newer API 28 build must install over an older one
# with a lower code.
#
# Flags: --skip-checks  build only (no unit tests, no lint)
#        --unsigned     no signing; output is livesync-android-<version>-unsigned.apk
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
SKIP_CHECKS=0
UNSIGNED=0
for a in "$@"; do
  case "$a" in
    --skip-checks) SKIP_CHECKS=1 ;;
    --unsigned) UNSIGNED=1 ;;
    *) echo "unknown flag $a" >&2; exit 2 ;;
  esac
done

MIN_SDK=28
export ANDROID_HOME=${ANDROID_HOME:-$HOME/Android/Sdk}
[[ -d "$ANDROID_HOME/platforms/android-35" ]] || { echo "no android-35 platform under $ANDROID_HOME" >&2; exit 1; }
if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
  ANDROID_NDK_HOME=$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -1)
fi
export ANDROID_NDK_HOME
[[ -d "$ANDROID_NDK_HOME" ]] || { echo "no NDK found; set ANDROID_NDK_HOME" >&2; exit 1; }
# Gradle 8.14 runs on JDK 17-24; the ambient JAVA_HOME may be newer.
JAVA_HOME=${LIVESYNC_JAVA_HOME:-$(mise where java@21 2>/dev/null || echo "${JAVA_HOME:-}")}
[[ -x "${JAVA_HOME:-}/bin/java" ]] || { echo "set LIVESYNC_JAVA_HOME to a JDK 17-21 (or: mise install java@21)" >&2; exit 1; }
export JAVA_HOME
export PATH="$JAVA_HOME/bin:$PATH"
echo "== JDK: $("$JAVA_HOME/bin/java" -version 2>&1 | head -1)"
# Pinned: the output format of `apksigner verify --print-certs` differs between
# build-tools releases, and CI images carry several.
BUILD_TOOLS="$ANDROID_HOME/build-tools/35.0.0"
[[ -x "$BUILD_TOOLS/apksigner" ]] || { echo "no build-tools 35.0.0 under $ANDROID_HOME" >&2; exit 1; }

# --- signing keystore (outside the repo, never committed) ---------------------
SIGN_DIR="$HOME/.config/livesync-android"
SIGN_PROPS=${LIVESYNC_SIGNING:-$SIGN_DIR/sideload.properties}
if [[ $UNSIGNED == 1 ]]; then
  # A path that does not exist: Gradle then builds without a signing config.
  SIGN_PROPS="$ROOT/android/build/no-signing.properties"
  rm -f "$SIGN_PROPS"
elif [[ ! -f "$SIGN_PROPS" && "${CI:-}" == "true" ]]; then
  echo "no signing properties at $SIGN_PROPS (CI never creates a keystore; use --unsigned)" >&2
  exit 1
elif [[ ! -f "$SIGN_PROPS" ]]; then
  echo "== creating the sideload keystore in $SIGN_DIR"
  mkdir -p "$SIGN_DIR" && chmod 700 "$SIGN_DIR"
  umask 077
  pw=$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')
  "$JAVA_HOME/bin/keytool" -genkeypair -v -keystore "$SIGN_DIR/sideload.jks" -storetype PKCS12 \
    -alias livesync -keyalg RSA -keysize 4096 -validity 10000 \
    -storepass "$pw" -keypass "$pw" \
    -dname "CN=LiveSync for Android (sideload), O=nytafar" >/dev/null 2>&1
  printf 'storeFile=%s\nstorePassword=%s\nkeyAlias=livesync\nkeyPassword=%s\n' \
    "$SIGN_DIR/sideload.jks" "$pw" "$pw" > "$SIGN_PROPS"
  chmod 600 "$SIGN_DIR/sideload.jks" "$SIGN_PROPS"
  unset pw
  umask 022
fi

# --- version -----------------------------------------------------------------
CRATE_VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
SHA=$(git rev-parse --short=8 HEAD)
DIRTY=$(git diff --quiet HEAD -- . ':!dist' 2>/dev/null || echo "-dirty")
API28_VERSION_CODE=928
VERSION_NAME=${LIVESYNC_VERSION_NAME:-"$CRATE_VERSION-api28+$SHA$DIRTY"}
VERSION_CODE=${LIVESYNC_VERSION_CODE:-$API28_VERSION_CODE}
[[ "$VERSION_CODE" =~ ^[1-9][0-9]*$ ]] || { echo "bad versionCode $VERSION_CODE" >&2; exit 1; }

GEN="$ROOT/android/app/build/generated"
echo "== Rust core: host build (bindings, JVM binding tests)"
cargo build -p livesync-ffi -p uniffi-bindgen
echo "== Kotlin bindings"
rm -rf "$GEN/uniffi"
./target/debug/uniffi-bindgen generate --library target/debug/liblivesync_ffi.so \
  --language kotlin --out-dir "$GEN/uniffi/kotlin" --no-format
echo "== Rust core: arm64-v8a release (API $MIN_SDK)"
rm -rf "$GEN/jniLibs"
cargo ndk -t arm64-v8a -P "$MIN_SDK" -o "$GEN/jniLibs" build --release -p livesync-ffi
ls -l "$GEN/jniLibs/arm64-v8a/"
echo "== bionic symbols at API $MIN_SDK"
scripts/check-bionic-symbols.sh "$GEN/jniLibs/arm64-v8a/liblivesync_ffi.so" "$MIN_SDK"

echo "== Gradle"
printf 'sdk.dir=%s\n' "$ANDROID_HOME" > android/local.properties
tasks=(assembleRelease)
if [[ $SKIP_CHECKS == 0 ]]; then
  tasks=(testReleaseUnitTest lintRelease assembleRelease)
fi
(cd android && ./gradlew --console=plain \
  -Plivesync.signing="$SIGN_PROPS" \
  -Plivesync.versionCode="$VERSION_CODE" \
  -Plivesync.versionName="$VERSION_NAME" \
  -Plivesync.requireHostLib=true \
  "${tasks[@]}")

mkdir -p dist
if [[ $UNSIGNED == 1 ]]; then
  APK=android/app/build/outputs/apk/release/app-release-unsigned.apk
  [[ -f "$APK" ]] || { echo "no unsigned APK at $APK" >&2; exit 1; }
  OUT="dist/livesync-android-$VERSION_NAME-unsigned.apk"
else
  APK=android/app/build/outputs/apk/release/app-release.apk
  [[ -f "$APK" ]] || { echo "no signed APK at $APK (is the signing file readable?)" >&2; exit 1; }
  # apksigner reports verification errors on stdout, so don't filter them away.
  certs=$("$BUILD_TOOLS/apksigner" verify --print-certs "$APK" 2>&1) || {
    echo "apksigner verify failed for $APK:" >&2; echo "$certs" >&2; exit 1; }
  echo "$certs"
  grep -qE "Signer #1 certificate SHA-256" <<<"$certs" || {
    echo "apksigner printed no 'Signer #1 certificate SHA-256' line (output above)" >&2; exit 1; }
  OUT="dist/livesync-android-$VERSION_NAME.apk"
fi
cp "$APK" "$OUT"
(cd dist && sha256sum "$(basename "$OUT")" | tee "$(basename "$OUT").sha256")
echo "APK: $ROOT/$OUT ($(du -h "$OUT" | cut -f1), versionCode $VERSION_CODE)"
