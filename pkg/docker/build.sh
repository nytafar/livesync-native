#!/bin/sh
# Build the lsync image from a release's Linux archives: check them against
# the release's SHA256SUMS, lay out the build context (amd64/lsync,
# arm64/lsync) and run `docker buildx build` with pkg/docker/Dockerfile.
# Extra arguments go to buildx: nothing is pushed unless you pass --push.
#
#   gh release download v0.4.0 -D /tmp/rel \
#     -p SHA256SUMS -p 'lsync-0.4.0-*-unknown-linux-musl.tar.gz'
#   pkg/docker/build.sh 0.4.0 /tmp/rel --load --platform linux/amd64     # try it locally
#   pkg/docker/build.sh 0.4.0 /tmp/rel --push --platform linux/amd64,linux/arm64 \
#     -t ghcr.io/nytafar/lsync:0.4.0 -t ghcr.io/nytafar/lsync:latest
set -eu
[ "$#" -ge 2 ] || { echo "usage: $0 VERSION RELEASE_DIR [docker buildx build args...]" >&2; exit 1; }
version=${1#v} rel=$2
shift 2
here=$(cd "$(dirname "$0")" && pwd)

sha256() {
	if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d ' ' -f 1
	else shasum -a 256 "$1" | cut -d ' ' -f 1; fi
}

ctx=$(mktemp -d)
trap 'rm -rf "$ctx"' EXIT
for pair in amd64:x86_64 arm64:aarch64; do
	arch=${pair%%:*} triple=${pair#*:}
	name=lsync-$version-$triple-unknown-linux-musl
	archive=$rel/$name.tar.gz
	if [ ! -f "$archive" ]; then
		echo "skipping $arch: no $archive" >&2
		continue
	fi
	want=$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f { print $1; n++ } END { if (n != 1) exit 1 }' "$rel/SHA256SUMS") || {
		echo "error: $rel/SHA256SUMS has no single line for $name.tar.gz" >&2
		exit 1
	}
	got=$(sha256 "$archive")
	[ "$got" = "$want" ] || { echo "error: checksum mismatch for $archive" >&2; exit 1; }
	mkdir -p "$ctx/$arch"
	tar -xzf "$archive" -C "$ctx" "$name/lsync"
	mv "$ctx/$name/lsync" "$ctx/$arch/lsync"
	rmdir "$ctx/$name"
done
[ -n "$(ls "$ctx")" ] || { echo "error: no Linux archive of $version in $rel" >&2; exit 1; }

docker buildx build -f "$here/Dockerfile" \
	--label "org.opencontainers.image.version=$version" \
	--label "org.opencontainers.image.source=https://github.com/nytafar/livesync-native" \
	--label "org.opencontainers.image.licenses=MIT OR Apache-2.0" \
	"$@" "$ctx"
