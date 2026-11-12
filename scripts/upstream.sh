#!/usr/bin/env bash
# The pinned upstream release (upstream.env) and the reference checkouts of
# it, for every script that runs or reads upstream code.
#
#   scripts/upstream.sh get KEY        a value from upstream.env (PLUGIN_RELEASE, COMMONLIB, ...)
#   scripts/upstream.sh tag            the release to run: the pin, or $LSYNC_UPSTREAM_TAG (announced)
#   scripts/upstream.sh cli [--no-build]
#                                      the official CLI's dist/index.cjs for that release, on stdout:
#                                      $LSYNC_CLI_JS when set (announced), else the build in
#                                      $REF/obsidian-livesync-<tag>, built there first when missing
#   scripts/upstream.sh ref [--no-build]
#                                      fetch the reference source of the pin: the plugin and CLI (built),
#                                      livesync-commonlib and octagonal-wheels; prints the folders
#   scripts/upstream.sh ls             the reference folders on disk, the pinned ones marked
#
# Reference layout, one folder per release, under $LSYNC_REF_DIR (default ~/code/ref):
#   obsidian-livesync-<tag>            the release tag of github.com/vrtmrz/obsidian-livesync,
#                                      with `npm ci --ignore-scripts`, the CLI built
#                                      (src/apps/cli/dist/index.cjs; scripts/ci/upstream-cli.sh)
#                                      and the plug-in built (main.js)
#   livesync-commonlib-<version>       the tag of github.com/vrtmrz/livesync-commonlib
#   octagonal-wheels-<version>         the npm package (its source repo has no release tags)
# A folder is created whole (built aside, then renamed into place) and never
# changed again, so a bump never touches files another session is using.
# Old releases stay until removed by hand (`ls` lists them).
#
# Messages go to stderr; only the asked-for value goes to stdout.
set -euo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PIN_FILE=$REPO/upstream.env
REF=${LSYNC_REF_DIR:-$HOME/code/ref}

say() { echo "upstream: $*" >&2; }
die() { say "$*"; exit 1; }

pin() { # pin KEY
  local v
  [[ "$1" =~ ^[A-Z_]+$ ]] || die "odd key: $1"
  v=$(sed -n "s/^$1=//p" "$PIN_FILE")
  [[ "$v" =~ ^[0-9A-Za-z._-]+$ ]] || die "no valid $1 in $PIN_FILE"
  printf '%s\n' "$v"
}

release() { # the release to run, with an override announced
  local pinned; pinned=$(pin PLUGIN_RELEASE)
  if [[ -n "${LSYNC_UPSTREAM_TAG:-}" && "$LSYNC_UPSTREAM_TAG" != "$pinned" ]]; then
    [[ "$LSYNC_UPSTREAM_TAG" =~ ^[0-9A-Za-z._-]+$ ]] || die "odd LSYNC_UPSTREAM_TAG: $LSYNC_UPSTREAM_TAG"
    say "OVERRIDE: release $LSYNC_UPSTREAM_TAG from LSYNC_UPSTREAM_TAG, not the pinned $pinned"
    printf '%s\n' "$LSYNC_UPSTREAM_TAG"
  else
    printf '%s\n' "$pinned"
  fi
}

cli_version_of() { # the version the CLI's package.json declares, or ?
  node -p 'require(process.argv[1]).version' "$(dirname "$1")/../package.json" 2>/dev/null || echo '?'
}

# install_whole DEST CMD...: run CMD with $STAGE (an empty folder beside DEST)
# to fill it, then rename it to DEST. One builder at a time per DEST; a DEST
# that exists is left alone.
install_whole() {
  local dest=$1; shift
  mkdir -p "$(dirname "$dest")"
  local lock="$(dirname "$dest")/.$(basename "$dest").lock"
  exec 9>"$lock"
  flock 9
  if [[ ! -e "$dest" ]]; then
    STAGE=$(mktemp -d "$(dirname "$dest")/.$(basename "$dest").tmp.XXXXXX")
    if ! "$@"; then rm -rf -- "$STAGE"; exec 9>&-; die "could not create $dest"; fi
    mv -T -- "$STAGE" "$dest"
  fi
  exec 9>&-
}

build_cli_into_stage() { # build_cli_into_stage TAG
  # upstream-cli.sh clones and builds the CLI into $STAGE/obsidian-livesync
  # (no credentials in reach, no install scripts); the plug-in's main.js is
  # built there too, for the benchmark's plug-in contender. Then lift it up.
  "$REPO/scripts/ci/upstream-cli.sh" "$STAGE" "$1" >/dev/null || return 1
  local inner="$STAGE/obsidian-livesync" tmp="$STAGE.inner"
  (cd "$inner" && env -u GITHUB_TOKEN -u GH_TOKEN npm run build >&2) || return 1
  mv -T -- "$inner" "$tmp" && rmdir -- "$STAGE" && mv -T -- "$tmp" "$STAGE"
}

cmd_cli() {
  local build=1
  [[ "${1:-}" == --no-build ]] && build=0
  local pinned; pinned=$(pin PLUGIN_RELEASE)
  if [[ -n "${LSYNC_CLI_JS:-}" ]]; then
    [[ -f "$LSYNC_CLI_JS" ]] || die "LSYNC_CLI_JS=$LSYNC_CLI_JS does not exist"
    local v; v=$(cli_version_of "$LSYNC_CLI_JS")
    if [[ "$v" == "$pinned-cli" ]]; then
      say "using LSYNC_CLI_JS=$LSYNC_CLI_JS ($v, the pinned release)"
    else
      say "OVERRIDE: using LSYNC_CLI_JS=$LSYNC_CLI_JS ($v), not the pinned $pinned-cli"
    fi
    printf '%s\n' "$LSYNC_CLI_JS"
    return
  fi
  local tag; tag=$(release)
  local dir="$REF/obsidian-livesync-$tag" cli
  cli="$dir/src/apps/cli/dist/index.cjs"
  if [[ ! -e "$dir" ]]; then
    [[ $build == 1 ]] || die "no official CLI $tag at $dir; build it with: scripts/upstream.sh cli"
    say "building the official CLI $tag into $dir (a few minutes, once)"
    install_whole "$dir" build_cli_into_stage "$tag"
  fi
  [[ -f "$cli" ]] || die "$dir has no built CLI ($cli); remove the folder and run scripts/upstream.sh cli again"
  local v; v=$(cli_version_of "$cli")
  [[ "$v" == "$tag-cli" ]] || die "$cli is $v, not $tag-cli; $dir is not the release it is named after"
  printf '%s\n' "$cli"
}

fetch_commonlib_into_stage() { # VERSION
  git -c advice.detachedHead=false clone -q --depth 1 --branch "$1" https://github.com/vrtmrz/livesync-commonlib "$STAGE/src" || return 1
  local v; v=$(node -p 'require(process.argv[1]).version' "$STAGE/src/package.json")
  [[ "$v" == "$1" ]] || { say "livesync-commonlib tag $1 declares version $v"; return 1; }
  local tmp="$STAGE.inner"
  mv -T -- "$STAGE/src" "$tmp" && rmdir -- "$STAGE" && mv -T -- "$tmp" "$STAGE"
}

fetch_npm_into_stage() { # NAME VERSION
  (cd "$STAGE" && npm pack --silent "$1@$2" >/dev/null) || return 1
  local tgz; tgz=$(ls "$STAGE"/*.tgz)
  tar -xzf "$tgz" -C "$STAGE" && rm -f -- "$tgz" || return 1
  local tmp="$STAGE.inner"
  mv -T -- "$STAGE/package" "$tmp" && rmdir -- "$STAGE" && mv -T -- "$tmp" "$STAGE"
}

cmd_ref() {
  local build=1
  [[ "${1:-}" == --no-build ]] && build=0
  local tag commonlib wheels
  tag=$(pin PLUGIN_RELEASE) commonlib=$(pin COMMONLIB) wheels=$(pin OCTAGONAL_WHEELS)
  if [[ $build == 1 ]]; then
    LSYNC_CLI_JS= LSYNC_UPSTREAM_TAG= cmd_cli >/dev/null
  fi
  local dir="$REF/livesync-commonlib-$commonlib"
  [[ -e "$dir" ]] || { say "fetching livesync-commonlib $commonlib into $dir"; install_whole "$dir" fetch_commonlib_into_stage "$commonlib"; }
  dir="$REF/octagonal-wheels-$wheels"
  [[ -e "$dir" ]] || { say "fetching octagonal-wheels $wheels into $dir"; install_whole "$dir" fetch_npm_into_stage octagonal-wheels "$wheels"; }
  printf '%s\n' "$REF/obsidian-livesync-$tag" "$REF/livesync-commonlib-$commonlib" "$REF/octagonal-wheels-$wheels"
}

cmd_ls() {
  local tag commonlib wheels d n
  tag=$(pin PLUGIN_RELEASE) commonlib=$(pin COMMONLIB) wheels=$(pin OCTAGONAL_WHEELS)
  for d in "$REF"/obsidian-livesync-* "$REF"/livesync-commonlib-* "$REF"/octagonal-wheels-*; do
    [[ -d "$d" ]] || continue
    n=$(basename "$d")
    case $n in
      "obsidian-livesync-$tag" | "livesync-commonlib-$commonlib" | "octagonal-wheels-$wheels") echo "pinned  $d" ;;
      *) echo "old     $d" ;;
    esac
  done
}

case ${1:-} in
  get) pin "${2:?usage: upstream.sh get KEY}" ;;
  tag) release ;;
  cli) shift; cmd_cli "$@" ;;
  ref) shift; cmd_ref "$@" ;;
  ls) cmd_ls ;;
  *) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
esac
