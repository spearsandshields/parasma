#!/usr/bin/env bash
#
# pack-smoke.sh — pack the publishable packages the way they will be
# published and prove the tarballs install and resolve outside the workspace.
#
# `bun pm pack` is used on purpose: it rewrites `workspace:*` dependency
# ranges to the sibling's version, which `npm pack` does not (that tarball
# installs nowhere). It is NOT run from inside `bun run ci`: a nested
# `bun pm pack` deadlocks on bun's process lock, so the workflow calls this
# script directly as its own step. Locally: ./scripts/pack-smoke.sh
#
# Checks, per package:
#   - the tarball carries LICENSE, NOTICE, THIRD-PARTY-NOTICES.md (and for
#     core THIRD-PARTY-LICENSES.md and the wasm binary)
#   - its package.json has no `workspace:` range left
# Then all tarballs are installed into a throwaway consumer (npm adds the
# adapters' vue/react peers), each package's entry and subpath exports are
# resolved with Node, and the adapters are imported under plain Node: their
# module scope must touch no DOM, so a server render can import them.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PACKAGES=(parasma-core headless snippet vue react)
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail() { echo "  ✗ $*" >&2; exit 1; }

for p in "${PACKAGES[@]}"; do
  echo "── pack @parasma/${p#parasma-}"
  ( cd "packages/$p" && bun pm pack --destination "$TMP" --quiet ) >/dev/null
done

for tgz in "$TMP"/*.tgz; do
  name="$(basename "$tgz")"
  listing="$(tar -tzf "$tgz")"
  for f in LICENSE NOTICE THIRD-PARTY-NOTICES.md; do
    grep -qx "package/$f" <<<"$listing" || fail "$name lacks $f"
  done
  if [[ "$name" == parasma-core-* ]]; then
    grep -qx "package/THIRD-PARTY-LICENSES.md" <<<"$listing" || fail "$name lacks THIRD-PARTY-LICENSES.md"
    grep -qx "package/dist/wasm/parasma_wasm_bg.wasm" <<<"$listing" || fail "$name lacks the wasm binary"
  fi
  if tar -xzOf "$tgz" package/package.json | grep -q '"workspace:'; then
    fail "$name still has a workspace: range"
  fi
  echo "  ✓ $name: licence files present, no workspace: ranges"
done

echo "── install into a throwaway consumer"
CONSUMER="$TMP/consumer"
mkdir -p "$CONSUMER"
( cd "$CONSUMER" && npm init -y >/dev/null && npm install --silent --no-audit --no-fund "$TMP"/*.tgz )

# import.meta.resolve walks the `exports` map exactly as a bundler would.
( cd "$CONSUMER" && node --input-type=module -e '
  const specs = [
    "@parasma/core", "@parasma/core/parasma_wasm_bg.wasm",
    "@parasma/headless", "@parasma/headless/internal",
    "@parasma/snippet", "@parasma/snippet/headless", "@parasma/snippet/chrome", "@parasma/snippet/snippet.css",
    "@parasma/vue", "@parasma/react",
  ];
  const { existsSync } = await import("node:fs");
  const { fileURLToPath } = await import("node:url");
  for (const s of specs) {
    const url = import.meta.resolve(s);
    if (!existsSync(fileURLToPath(url))) throw new Error(`${s} resolves to a missing file: ${url}`);
    console.log(`  ✓ ${s}`);
  }
  for (const s of ["@parasma/vue", "@parasma/react"]) {
    const m = await import(s);
    if (typeof m.useParasma !== "function") throw new Error(`${s} has no useParasma export`);
    console.log(`  ✓ ${s} imports under plain Node`);
  }
' )
echo "PACK SMOKE PASSED"
