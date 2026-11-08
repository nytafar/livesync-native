#!/bin/sh
# Install lsync from a GitHub release: download the archive for this machine,
# check it against the release's SHA256SUMS, and put the binary in PREFIX/bin.
#
#   curl -fsSLO https://github.com/nytafar/livesync-native/releases/latest/download/install.sh
#   sh install.sh                       # the latest release into ~/.local/bin
#   sh install.sh --version 0.4.0 --prefix /opt/lsync
#
# It never uses sudo. For a system-wide install, run it as root yourself
# (`sudo sh install.sh --prefix /usr/local`). A checksum that is missing or
# does not match stops it before anything is installed.
set -eu

REPO_URL=${LSYNC_REPO_URL:-https://github.com/nytafar/livesync-native}

usage() {
	cat <<'EOF'
Usage: install.sh [options]

  --version VERSION   the release to install, e.g. 0.4.0 (default: the latest)
  --prefix DIR        install into DIR/bin (default: $HOME/.local)
  --bin-dir DIR       install into DIR itself (overrides --prefix)
  --target TARGET     the archive's target instead of this machine's, e.g.
                      x86_64-unknown-linux-musl, aarch64-unknown-linux-musl,
                      universal-apple-darwin, aarch64-linux-android
  --base-url URL      where the release's files are, instead of GitHub's
                      download URL for the version (a mirror, or file:///dir)
  -h, --help          this text

The environment variables LSYNC_VERSION, LSYNC_PREFIX, LSYNC_BIN_DIR,
LSYNC_TARGET and LSYNC_BASE_URL set the same.
EOF
}

die() {
	printf 'install.sh: %s\n' "$*" >&2
	exit 1
}

version=${LSYNC_VERSION:-}
prefix=${LSYNC_PREFIX:-}
bin_dir=${LSYNC_BIN_DIR:-}
target=${LSYNC_TARGET:-}
base_url=${LSYNC_BASE_URL:-}

need_arg() {
	[ "$#" -ge 2 ] && [ -n "$2" ] || die "$1 needs a value"
}

while [ "$#" -gt 0 ]; do
	case $1 in
	--version) need_arg "$@"; version=$2; shift 2 ;;
	--version=*) version=${1#*=}; shift ;;
	--prefix) need_arg "$@"; prefix=$2; shift 2 ;;
	--prefix=*) prefix=${1#*=}; shift ;;
	--bin-dir) need_arg "$@"; bin_dir=$2; shift 2 ;;
	--bin-dir=*) bin_dir=${1#*=}; shift ;;
	--target) need_arg "$@"; target=$2; shift 2 ;;
	--target=*) target=${1#*=}; shift ;;
	--base-url) need_arg "$@"; base_url=$2; shift 2 ;;
	--base-url=*) base_url=${1#*=}; shift ;;
	-h | --help) usage; exit 0 ;;
	*) usage >&2; die "unknown option: $1" ;;
	esac
done

command -v curl >/dev/null 2>&1 || die "curl is needed to download the release"
command -v tar >/dev/null 2>&1 || die "tar is needed to unpack the release"

# This machine's archive.
if [ -z "$target" ]; then
	os=$(uname -s)
	arch=$(uname -m)
	case $os in
	Linux)
		if [ "$(uname -o 2>/dev/null || true)" = Android ]; then
			case $arch in
			aarch64 | arm64) target=aarch64-linux-android ;;
			*) die "no build for Android on $arch" ;;
			esac
		else
			case $arch in
			x86_64 | amd64) target=x86_64-unknown-linux-musl ;;
			aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
			*) die "no Linux build for $arch (there are x86_64 and aarch64)" ;;
			esac
		fi
		;;
	Darwin) target=universal-apple-darwin ;;
	*) die "no build for $os; build from source (see the README)" ;;
	esac
fi

# The version: given, or the tag GitHub's "latest release" redirects to.
version=${version#v}
if [ -z "$version" ]; then
	[ -z "$base_url" ] || die "--base-url needs --version"
	latest=$(curl -fsSLI --proto '=https' -o /dev/null -w '%{url_effective}' "$REPO_URL/releases/latest") ||
		die "could not ask GitHub for the latest release"
	case $latest in
	*/releases/tag/v*) version=${latest##*/releases/tag/v} ;;
	*) die "could not tell the latest release from $latest; give --version" ;;
	esac
fi
case $version in
*/* | *' '* | '') die "not a version: $version" ;;
esac
[ -n "$base_url" ] || base_url="$REPO_URL/releases/download/v$version"
base_url=${base_url%/}

if [ -z "$bin_dir" ]; then
	if [ -z "$prefix" ]; then
		[ -n "${HOME:-}" ] || die "HOME is not set; give --prefix"
		prefix=$HOME/.local
	fi
	bin_dir=$prefix/bin
fi

# A SHA-256 tool: coreutils, macOS and BSD, or OpenSSL.
sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d ' ' -f 1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d ' ' -f 1
	elif command -v openssl >/dev/null 2>&1; then
		openssl dgst -sha256 "$1" | sed 's/^.*= *//'
	else
		return 1
	fi
}

name=lsync-$version-$target
archive=$name.tar.gz

tmp=$(mktemp -d "${TMPDIR:-/tmp}/lsync-install.XXXXXX") || die "could not make a temporary folder"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM

printf 'Downloading %s\n' "$base_url/$archive"
curl -fsSL --proto '=https,file' -o "$tmp/SHA256SUMS" "$base_url/SHA256SUMS" ||
	die "could not download SHA256SUMS from $base_url"
curl -fsSL --proto '=https,file' -o "$tmp/$archive" "$base_url/$archive" ||
	die "could not download $archive from $base_url (is there a $target build of $version?)"

# The archive's line in SHA256SUMS: "<hash>  <name>" (or "<hash> *<name>").
want=$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1; n++ } END { if (n != 1) exit 1 }' "$tmp/SHA256SUMS") ||
	die "SHA256SUMS has no single line for $archive; not installing"
case $want in
*[!0-9a-fA-F]* | '') die "SHA256SUMS has no valid checksum for $archive; not installing" ;;
esac
[ "${#want}" -eq 64 ] || die "SHA256SUMS has no valid checksum for $archive; not installing"
got=$(sha256 "$tmp/$archive") || die "no sha256sum, shasum or openssl to check the download with; not installing"
want=$(printf '%s' "$want" | tr 'A-F' 'a-f')
if [ "$got" != "$want" ]; then
	die "checksum mismatch for $archive: expected $want, got $got; not installing"
fi
printf 'Checksum OK (%s)\n' "$got"

mkdir "$tmp/x"
tar -xzf "$tmp/$archive" -C "$tmp/x" "$name/lsync" || die "$archive holds no $name/lsync"
bin=$tmp/x/$name/lsync
[ -f "$bin" ] && [ ! -L "$bin" ] || die "$archive holds no regular file $name/lsync"

mkdir -p "$bin_dir" || die "could not create $bin_dir"
[ -w "$bin_dir" ] || die "$bin_dir is not writable by you; give another --prefix, or run this as its owner"
# Copy beside the old binary, then rename over it, so a running lsync keeps
# its file and nothing is ever half-written.
cp "$bin" "$bin_dir/.lsync.new.$$" || die "could not write to $bin_dir"
chmod 755 "$bin_dir/.lsync.new.$$"
mv -f "$bin_dir/.lsync.new.$$" "$bin_dir/lsync" || {
	rm -f "$bin_dir/.lsync.new.$$"
	die "could not install $bin_dir/lsync"
}

printf 'Installed %s\n' "$bin_dir/lsync"
"$bin_dir/lsync" --version 2>/dev/null || true
case ":${PATH:-}:" in
*":$bin_dir:"*) ;;
*) printf 'Note: %s is not on your PATH.\n' "$bin_dir" ;;
esac
printf 'Next: lsync account add, lsync vault add (or lsync import), then lsync service install. See the README.\n'
