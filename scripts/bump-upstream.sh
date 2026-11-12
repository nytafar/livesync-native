#!/usr/bin/env bash
# Move lsync to an upstream Self-hosted LiveSync release: the pin
# (upstream.env) and everything derived from it, in one go.
#
#   scripts/bump-upstream.sh                 # the latest upstream release
#   scripts/bump-upstream.sh 1.0.50          # a given release tag
#   scripts/bump-upstream.sh --dry-run [TAG] # only show what would change
#
# Steps:
#   1. Read the release's package.json and package-lock.json from GitHub and
#      take from them the commonlib it pins, the octagonal-wheels that
#      commonlib resolves to, and its glob matcher (minimatch and its two
#      dependencies, as resolved from commonlib).
#   2. Write upstream.env, and tools/golden/package.json (the commonlib
#      dependency and the matcher overrides).
#   3. npm install in tools/golden (updates package-lock.json; no install
#      scripts), then npm ci to check the lockfile installs as written.
#   4. Regenerate fixtures/golden (node generate.mjs).
#   5. Rewrite the pin markers (<!--pin:KEY-->VALUE) in README.md, AGENTS.md
#      and COMPATIBILITY.md.
#   6. Fetch the reference source of the release and build its CLI and
#      plug-in (scripts/upstream.sh ref; per-release folders, never clobbered).
#   7. Show what changed, and what is left to do.
#
# Safe to re-run (each step is idempotent). Never commits or pushes.
set -euo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$REPO"

DRY=0
TAG=
for a in "$@"; do
  case $a in
    --dry-run) DRY=1 ;;
    -h | --help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "bump: unknown option $a" >&2; exit 2 ;;
    *) TAG=$a ;;
  esac
done

step() { printf '\n== %s\n' "$*"; }
KEYS=(PLUGIN_RELEASE COMMONLIB OCTAGONAL_WHEELS MINIMATCH NONRECURSIVE_GLOB_MATCH BRACE_EXPANSION)

if [[ -z "$TAG" ]]; then
  TAG=$(scripts/ci/upstream-cli.sh --tag | sed -n 's/^tag=//p')
  echo "latest upstream release: $TAG"
fi
[[ "$TAG" =~ ^[0-9A-Za-z._-]+$ ]] || { echo "bump: odd release tag: $TAG" >&2; exit 1; }

step "1. what release $TAG ships"
WORK=$(mktemp -d)
trap 'rm -rf -- "$WORK"' EXIT
RAW=https://raw.githubusercontent.com/vrtmrz/obsidian-livesync/$TAG
curl -fsSL "$RAW/package.json" -o "$WORK/package.json"
curl -fsSL "$RAW/package-lock.json" -o "$WORK/package-lock.json"
declare -A NEW OLD
while IFS='=' read -r k v; do NEW[$k]=$v; done < <(TAG="$TAG" node - "$WORK" <<'JS'
const fs = require("fs"), path = require("path");
const dir = process.argv[2];
const pkg = JSON.parse(fs.readFileSync(path.join(dir, "package.json"), "utf8"));
const lock = JSON.parse(fs.readFileSync(path.join(dir, "package-lock.json"), "utf8"));
const P = lock.packages;
const die = (m) => { console.error(`bump: ${m}`); process.exit(1); };
if (pkg.version !== process.env.TAG) die(`release ${process.env.TAG} declares version ${pkg.version}`);
// Where node finds `name` for a package installed at `from` (a lockfile key).
const resolve = (from, name) => {
  let base = from;
  for (;;) {
    const key = base ? `${base}/node_modules/${name}` : `node_modules/${name}`;
    if (P[key]) return key;
    if (!base) die(`${name} (from ${from}) is not in the release's package-lock.json`);
    const i = base.lastIndexOf("node_modules/");
    base = i <= 0 ? "" : base.slice(0, i).replace(/\/$/, "");
  }
};
const lib = "node_modules/@vrtmrz/livesync-commonlib";
if (!P[lib]) die("the release does not depend on @vrtmrz/livesync-commonlib");
const mm = resolve(lib, "minimatch");
const out = {
  PLUGIN_RELEASE: pkg.version,
  COMMONLIB: P[lib].version,
  OCTAGONAL_WHEELS: P[resolve(lib, "octagonal-wheels")].version,
  MINIMATCH: P[mm].version,
  NONRECURSIVE_GLOB_MATCH: P[resolve(mm, "@isaacs/nonrecursive-glob-match")].version,
  BRACE_EXPANSION: P[resolve(mm, "brace-expansion")].version,
};
for (const [k, v] of Object.entries(out)) {
  if (!/^[0-9A-Za-z._-]+$/.test(v ?? "")) die(`odd ${k}: ${v}`);
  console.log(`${k}=${v}`);
}
JS
)
changed=0
for k in "${KEYS[@]}"; do
  OLD[$k]=$(scripts/upstream.sh get "$k")
  [[ -n "${NEW[$k]:-}" ]] || { echo "bump: could not determine $k" >&2; exit 1; }
  if [[ "${OLD[$k]}" == "${NEW[$k]}" ]]; then
    printf '  %-24s %s (unchanged)\n' "$k" "${NEW[$k]}"
  else
    printf '  %-24s %s -> %s\n' "$k" "${OLD[$k]}" "${NEW[$k]}"
    changed=1
  fi
done
[[ $changed == 1 ]] || echo "  the pin is already at $TAG; refreshing what derives from it"
if [[ "${OLD[PLUGIN_RELEASE]}" != "$TAG" && "$(printf '%s\n%s\n' "${OLD[PLUGIN_RELEASE]}" "$TAG" | sort -V | tail -n1)" != "$TAG" ]]; then
  echo "  warning: $TAG is OLDER than the pinned ${OLD[PLUGIN_RELEASE]}"
fi
if [[ $DRY == 1 ]]; then
  echo
  echo "dry run: nothing written"
  exit 0
fi

step "2. upstream.env and tools/golden/package.json"
for k in "${KEYS[@]}"; do
  sed -i -E "s/^$k=.*/$k=${NEW[$k]}/" upstream.env
  [[ "$(scripts/upstream.sh get "$k")" == "${NEW[$k]}" ]] || { echo "bump: upstream.env has no $k line" >&2; exit 1; }
done
COMMONLIB=${NEW[COMMONLIB]} MINIMATCH=${NEW[MINIMATCH]} NRGM=${NEW[NONRECURSIVE_GLOB_MATCH]} BRACE=${NEW[BRACE_EXPANSION]} node -e '
const fs = require("fs"); const p = "tools/golden/package.json";
const pkg = JSON.parse(fs.readFileSync(p, "utf8")); const e = process.env;
pkg.dependencies = { "@vrtmrz/livesync-commonlib": e.COMMONLIB };
pkg.overrides = { "minimatch": e.MINIMATCH, "@isaacs/nonrecursive-glob-match": e.NRGM, "brace-expansion": e.BRACE };
fs.writeFileSync(p, JSON.stringify(pkg, null, 2) + "\n");'
echo "  written"

step "3. install tools/golden"
(
  cd tools/golden
  npm install --ignore-scripts --no-audit --no-fund --loglevel=error
  npm ci --ignore-scripts --no-audit --no-fund --loglevel=error
)

step "4. regenerate fixtures/golden"
(cd tools/golden && node generate.mjs ../../fixtures/golden)

step "5. pin markers in the docs"
for f in README.md AGENTS.md COMPATIBILITY.md; do
  for k in "${KEYS[@]}"; do
    sed -i -E "s/<!--pin:$k-->[0-9][0-9A-Za-z.]*[0-9A-Za-z]/<!--pin:$k-->${NEW[$k]}/g" "$f"
  done
done
echo "  done"

step "6. reference source and the official CLI of $TAG"
scripts/upstream.sh ref

step "7. what changed"
git status --short -- upstream.env tools/golden fixtures/golden README.md AGENTS.md COMPATIBILITY.md
fixtures=$(git diff --name-only -- fixtures/golden | wc -l)
echo "fixtures whose content changed beyond the version stamp:"
moved=0
for f in $(git diff --name-only -- fixtures/golden); do
  # Lines changed other than the commonlib/octagonal-wheels version lines.
  n=$(git diff -U0 -- "$f" | grep -E '^[-+][^-+]' | grep -cvE '"(version|commonlib|octagonalWheels)": ' || true)
  if [[ $n -gt 0 ]]; then echo "  $f: $n changed lines"; moved=1; fi
done
[[ $moved == 1 ]] || echo "  none (fixtures with any change: $fixtures)"

# The release before this bump: the committed pin (so a re-run still knows it).
old_tag=$(git show HEAD:upstream.env 2>/dev/null | sed -n 's/^PLUGIN_RELEASE=//p')
[[ -n "$old_tag" ]] || old_tag=${OLD[PLUGIN_RELEASE]}
if [[ "$old_tag" == "$TAG" ]]; then
  echo
  echo "The committed pin is already $TAG: nothing moved, nothing is left to do but the gate."
  exit 0
fi
cat <<EOF

Left to do (nothing is committed):
  1. The gate: cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
     (a changed fixture fails the test that ports it; port upstream's change, never edit fixtures).
  2. The interop scenarios against $TAG: scripts/interop.sh (dev container only).
  3. Read upstream's changes from $old_tag to $TAG:
       https://github.com/vrtmrz/obsidian-livesync/compare/$old_tag...$TAG
       https://github.com/vrtmrz/obsidian-livesync/releases
       ${LSYNC_REF_DIR:-$HOME/code/ref}/obsidian-livesync-$TAG/updates.md
     looking for new settings, tweaks, features, generations and format changes.
  4. COMPATIBILITY.md: its table now names $TAG through the pin markers; keep a
     row for ${old_tag}-cli if it stays tested, and update the notes on what
     changed between $old_tag and $TAG.
  5. Cite upstream behaviour you port or check by the pinned version ($TAG).
EOF
