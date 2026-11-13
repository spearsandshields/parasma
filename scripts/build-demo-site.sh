#!/usr/bin/env bash
#
# build-demo-site.sh — build the public demo site into one directory.
#
#   ./scripts/build-demo-site.sh [OUT_DIR] [BASE]
#
# OUT_DIR defaults to target/demo-site; BASE is the URL path the site is served
# under (default /parasma/, the GitHub Pages path). The site is the snippet demo
# at BASE with its sample documents, and the Vue example at BASE/vue/.
# .github/workflows/pages.yml runs this and deploys the result; run it locally
# to see exactly what would ship.
#
# Prerequisites: the wasm is built (bun run build:wasm:full) and fixtures/ is
# populated (bun run scripts/fetch-fixtures.ts).
#
# The check at the end is fail-closed: the site must contain nothing from
# fixtures/user/ (by path, by manifest entry, or by content) and no absolute
# home-directory path.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/target/demo-site}"
BASE="${2:-/parasma/}"
case "$OUT" in /*) ;; *) OUT="$PWD/$OUT" ;; esac
USERDIR="$ROOT/fixtures/user"

say()  { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
fail() { printf '\033[31m  ✗ %s\033[0m\n' "$*"; exit 1; }

[ -f "$ROOT/packages/parasma-core/src/wasm/parasma_wasm_bg.wasm" ] \
  || fail "wasm missing: run bun run build:wasm:full"
[ -f "$ROOT/fixtures/manifest.json" ] \
  || fail "fixtures missing: run bun run scripts/fetch-fixtures.ts"

say "Snippet demo (base $BASE)"
cd "$ROOT/packages/snippet"
bun scripts/copy-demo-fixtures.ts
rm -rf demo/dist
bunx vite build --base="$BASE" >/dev/null
cp -r demo/fixtures demo/dist/fixtures

say "Vue example (base ${BASE}vue/)"
(cd "$ROOT/packages/vue" && rm -rf demo/dist && bunx vite build --base="${BASE}vue/" >/dev/null)

rm -rf "$OUT"
mkdir -p "$OUT/vue"
cp -r "$ROOT/packages/snippet/demo/dist/." "$OUT/"
cp -r "$ROOT/packages/vue/demo/dist/." "$OUT/vue/"
touch "$OUT/.nojekyll"
find "$OUT" -name '.DS_Store' -delete

say "Check the built site"
bad=0
n=$(find "$OUT" -ipath '*/user/*' | wc -l | tr -d ' ')
[ "$n" = 0 ] || { echo "  user/ paths in the site: $n"; bad=1; }
n=$(grep -c '"fixtures/user/' "$OUT/fixtures/manifest.json" || true)
[ "$n" = 0 ] || { echo "  fixtures/user/ entries in the site manifest: $n"; bad=1; }
n=$( { grep -rlE '/(Users|home)/[A-Za-z0-9._-]+/' "$OUT" || true; } | wc -l | tr -d ' ')
[ "$n" = 0 ] || { echo "  files containing a home-directory path: $n"; bad=1; }
if [ -d "$USERDIR" ]; then
  # Content check: no shipped file may be byte-identical to a local document.
  hashes="$(find -L "$USERDIR" -type f -exec shasum -a 256 {} + | awk '{print $1}' | sort -u)"
  while IFS= read -r f; do
    h="$(shasum -a 256 "$f" | cut -d' ' -f1)"
    if grep -qx "$h" <<<"$hashes"; then echo "  local document content in the site: ${f#"$OUT"/}"; bad=1; fi
  done < <(find "$OUT" -type f \( -name '*.docx' -o -name '*.xlsx' -o -name '*.pptx' \
             -o -name '*.csv' -o -name '*.png' -o -name '*.jpg' -o -name '*.jpeg' \))
fi
[ "$bad" = 0 ] || fail "site check failed; nothing was deployed"
echo "  ✓ site clean: $(find "$OUT" -type f | wc -l | tr -d ' ') files, $(du -sh "$OUT" | cut -f1)"
echo "  → $OUT"
