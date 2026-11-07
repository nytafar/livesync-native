#!/usr/bin/env bash
# Build the official Self-hosted LiveSync CLI from an upstream release, for
# the interop scenarios (scripts/interop.sh) and the weekly upstream job
# (.github/workflows/upstream-interop.yml).
#
#   scripts/ci/upstream-cli.sh DIR            # the latest release
#   scripts/ci/upstream-cli.sh DIR 1.0.46     # a given release tag
#
# Clones github.com/vrtmrz/obsidian-livesync at the release tag into
# DIR/obsidian-livesync (shallow), runs `npm ci` and builds the CLI workspace
# the way upstream's own README does. Prints two lines on stdout:
#   tag=<release tag>
#   cli_js=<path of the built dist/index.cjs>
# Reads only public GitHub data (GITHUB_TOKEN, when set, only raises the API
# rate limit). Never pushes or publishes anything.
set -euo pipefail
DIR=${1:?usage: upstream-cli.sh DIR [TAG]}
TAG=${2:-}
REPO_URL=https://github.com/vrtmrz/obsidian-livesync
API=https://api.github.com/repos/vrtmrz/obsidian-livesync

if [[ -z "$TAG" ]]; then
  auth=()
  [[ -n "${GITHUB_TOKEN:-}" ]] && auth=(-H "Authorization: Bearer $GITHUB_TOKEN")
  # The latest full release (GitHub's "latest" skips drafts and pre-releases).
  TAG=$(curl -fsSL "${auth[@]}" -H 'Accept: application/vnd.github+json' "$API/releases/latest" |
    python3 -I -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')
fi
[[ "$TAG" =~ ^[0-9A-Za-z._-]+$ ]] || { echo "upstream-cli: odd release tag: $TAG" >&2; exit 1; }

mkdir -p "$DIR"
SRC="$DIR/obsidian-livesync"
if [[ -d "$SRC/.git" && "$(git -C "$SRC" describe --tags --exact-match 2>/dev/null || true)" == "$TAG" ]]; then
  echo "upstream-cli: reusing $SRC at $TAG" >&2
else
  rm -rf -- "$SRC"
  git clone -q --depth 1 --branch "$TAG" "$REPO_URL" "$SRC"
fi
(
  cd "$SRC"
  npm ci --no-audit --no-fund --loglevel=error >&2
  npm run build -w self-hosted-livesync-cli >&2
)
CLI_JS="$SRC/src/apps/cli/dist/index.cjs"
[[ -f "$CLI_JS" ]] || { echo "upstream-cli: the build left no $CLI_JS" >&2; exit 1; }
echo "tag=$TAG"
echo "cli_js=$CLI_JS"
