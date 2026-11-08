#!/bin/sh
# Pack an lsync binary as a release archive:
#   OUTDIR/lsync-VERSION-TARGET.tar.gz, holding lsync-VERSION-TARGET/ with
#   lsync, README.md, LICENSE-MIT, LICENSE-APACHE and NOTICE.
# install.sh, the AUR package, the Homebrew formula and the Docker build all
# expect this layout.
#
#   scripts/ci/pack-cli.sh BIN TARGET VERSION OUTDIR
set -eu
[ "$#" -eq 4 ] || { echo "usage: $0 BIN TARGET VERSION OUTDIR" >&2; exit 1; }
bin=$1 target=$2 version=$3 out=$4
root=$(cd "$(dirname "$0")/../.." && pwd)
name=lsync-$version-$target
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/$name" "$out"
cp "$bin" "$work/$name/lsync"
chmod 755 "$work/$name/lsync"
for f in README.md LICENSE-MIT LICENSE-APACHE NOTICE; do
	cp "$root/$f" "$work/$name/"
done
tar -C "$work" -czf "$out/$name.tar.gz" "$name"
echo "$out/$name.tar.gz"
