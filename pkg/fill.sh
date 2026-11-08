#!/bin/sh
# Fill the packaging templates for one release:
#   OUTDIR/aur/PKGBUILD and OUTDIR/homebrew/lsync.rb
# from pkg/aur/PKGBUILD and pkg/homebrew/lsync.rb, with @VERSION@, @PKGVER@
# (the version with - as _, as pacman needs) and every @SHA256_<target>@ taken
# from the release's SHA256SUMS (the line for lsync-VERSION-<target>.tar.gz).
# A checksum that is missing stops it; nothing is left unfilled.
#
#   gh release download v0.4.0 -p SHA256SUMS -D /tmp/rel
#   pkg/fill.sh 0.4.0 /tmp/rel/SHA256SUMS /tmp/rel/pkg
set -eu
[ "$#" -eq 3 ] || { echo "usage: $0 VERSION SHA256SUMS OUTDIR" >&2; exit 1; }
version=${1#v} sums=$2 out=$3
here=$(cd "$(dirname "$0")" && pwd)
case $version in
'' | *[!0-9A-Za-z.-]*) echo "error: not a version: $version" >&2; exit 1 ;;
esac
pkgver=$(printf '%s' "$version" | tr - _)

fill() {
	src=$1 dst=$2
	mkdir -p "$(dirname "$dst")"
	text=$(sed -e "s/@VERSION@/$version/g" -e "s/@PKGVER@/$pkgver/g" "$src")
	for target in $(printf '%s\n' "$text" | grep -o '@SHA256_[0-9A-Za-z_.-]*@' | sed 's/^@SHA256_//; s/@$//' | sort -u); do
		archive=lsync-$version-$target.tar.gz
		sum=$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1; n++ } END { if (n != 1) exit 1 }' "$sums") || {
			echo "error: $sums has no single line for $archive" >&2
			exit 1
		}
		case $sum in
		*[!0-9a-f]*) echo "error: bad checksum for $archive: $sum" >&2; exit 1 ;;
		esac
		[ "${#sum}" -eq 64 ] || { echo "error: bad checksum for $archive: $sum" >&2; exit 1; }
		text=$(printf '%s\n' "$text" | sed "s/@SHA256_$target@/$sum/g")
	done
	if printf '%s\n' "$text" | grep -n '@[A-Z0-9_]*[A-Za-z0-9_.-]*@' >&2; then
		echo "error: $dst still has placeholders (above)" >&2
		exit 1
	fi
	printf '%s\n' "$text" > "$dst"
	echo "$dst"
}

fill "$here/aur/PKGBUILD" "$out/aur/PKGBUILD"
fill "$here/homebrew/lsync.rb" "$out/homebrew/lsync.rb"
