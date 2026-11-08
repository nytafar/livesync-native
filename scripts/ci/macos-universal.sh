#!/bin/sh
# Join the aarch64 and x86_64 macOS release archives into one universal
# binary (lipo), checked, and pack it as lsync-VERSION-universal-apple-darwin.
# Runs on macOS (lipo, codesign).
#
#   scripts/ci/macos-universal.sh VERSION DIR
#
# DIR holds lsync-VERSION-aarch64-apple-darwin.tar.gz and
# lsync-VERSION-x86_64-apple-darwin.tar.gz; the universal archive is written
# beside them.
set -eu
[ "$#" -eq 2 ] || { echo "usage: $0 VERSION DIR" >&2; exit 1; }
version=$1 dir=$2
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for arch in aarch64 x86_64; do
	name=lsync-$version-$arch-apple-darwin
	mkdir "$work/$arch"
	tar -xzf "$dir/$name.tar.gz" -C "$work/$arch" "$name/lsync"
	mv "$work/$arch/$name/lsync" "$work/lsync-$arch"
done
lipo -create -output "$work/lsync" "$work/lsync-aarch64" "$work/lsync-x86_64"
archs=$(lipo -archs "$work/lsync")
echo "lipo: $archs"
case " $archs " in
*" x86_64 "*) ;;
*) echo "error: the universal binary has no x86_64 slice ($archs)" >&2; exit 1 ;;
esac
case " $archs " in
*" arm64 "*) ;;
*) echo "error: the universal binary has no arm64 slice ($archs)" >&2; exit 1 ;;
esac
# Each slice keeps the linker's ad-hoc signature; arm64 will not run without one.
if ! codesign --verify --verbose=2 "$work/lsync"; then
	echo "re-signing ad hoc" >&2
	codesign --force --sign - "$work/lsync"
	codesign --verify --verbose=2 "$work/lsync"
fi
file "$work/lsync"
case $(uname -m) in
arm64) "$work/lsync" --version; arch -x86_64 "$work/lsync" --version || echo "no Rosetta here; the x86_64 slice was not run" ;;
x86_64) "$work/lsync" --version ;;
esac
"$here/pack-cli.sh" "$work/lsync" universal-apple-darwin "$version" "$dir"
