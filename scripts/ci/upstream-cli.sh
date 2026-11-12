#!/usr/bin/env bash
# Build the official Self-hosted LiveSync CLI from an upstream release, for
# the interop scenarios (scripts/interop.sh) and the weekly upstream job
# (.github/workflows/upstream-interop.yml).
#
#   scripts/ci/upstream-cli.sh DIR            # the latest release
#   scripts/ci/upstream-cli.sh DIR 1.0.46     # a given release tag
#
#   scripts/ci/upstream-cli.sh --tag [TAG]    # only print tag=<TAG, or the latest>
#
# Clones github.com/vrtmrz/obsidian-livesync at the release tag into
# DIR/obsidian-livesync (shallow), runs `npm ci --ignore-scripts` and builds
# the CLI workspace the way upstream's own README does. Prints two lines on
# stdout:
#   tag=<release tag>
#   cli_js=<path of the built dist/index.cjs>
# Reads only public GitHub data. GITHUB_TOKEN, when set, is used only for the
# API call that finds the latest release (it raises the rate limit), and is
# dropped from the environment before any upstream code runs: no install
# scripts (--ignore-scripts), and the build runs without it. The weekly job
# looks the tag up with `--tag` in a step of its own and builds without the
# token. Never pushes or publishes anything.
set -euo pipefail

latest_tag() {
  local auth=()
  [[ -n "${GITHUB_TOKEN:-}" ]] && auth=(-H "Authorization: Bearer $GITHUB_TOKEN")
  # The latest full release (GitHub's "latest" skips drafts and pre-releases).
  curl -fsSL "${auth[@]}" -H 'Accept: application/vnd.github+json' \
    https://api.github.com/repos/vrtmrz/obsidian-livesync/releases/latest |
    python3 -I -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])'
}

check_tag() {
  [[ "$1" =~ ^[0-9A-Za-z._-]+$ ]] || { echo "upstream-cli: odd release tag: $1" >&2; exit 1; }
}

if [[ "${1:-}" == "--tag" ]]; then
  TAG=${2:-}
  [[ -n "$TAG" ]] || TAG=$(latest_tag)
  check_tag "$TAG"
  echo "tag=$TAG"
  exit 0
fi

DIR=${1:?usage: upstream-cli.sh DIR [TAG] | upstream-cli.sh --tag [TAG]}
TAG=${2:-}
REPO_URL=https://github.com/vrtmrz/obsidian-livesync
[[ -n "$TAG" ]] || TAG=$(latest_tag)
check_tag "$TAG"
# From here on upstream code runs (the build): no credentials in reach.
unset GITHUB_TOKEN GH_TOKEN

mkdir -p "$DIR"
SRC="$DIR/obsidian-livesync"
if [[ -d "$SRC/.git" && "$(git -C "$SRC" describe --tags --exact-match 2>/dev/null || true)" == "$TAG" ]]; then
  echo "upstream-cli: reusing $SRC at $TAG" >&2
else
  rm -rf -- "$SRC"
  git -c advice.detachedHead=false clone -q --depth 1 --branch "$TAG" "$REPO_URL" "$SRC"
fi
(
  cd "$SRC"
  # No install scripts: the build needs none (esbuild and the prebuilt
  # leveldown ship their binaries in optional packages).
  npm ci --ignore-scripts --no-audit --no-fund --loglevel=error >&2
  npm run build -w self-hosted-livesync-cli >&2
)
CLI_JS="$SRC/src/apps/cli/dist/index.cjs"
[[ -f "$CLI_JS" ]] || { echo "upstream-cli: the build left no $CLI_JS" >&2; exit 1; }
echo "tag=$TAG"
echo "cli_js=$CLI_JS"
