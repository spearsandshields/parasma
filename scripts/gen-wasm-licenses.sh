#!/usr/bin/env bash
#
# gen-wasm-licenses.sh — render the licence inventory of every crate compiled
# into the shipped wasm (features sml,pml,emf, target wasm32) into
# packages/parasma-core/THIRD-PARTY-LICENSES.md, so the @parasma/core tarball
# carries the texts of what it bundles.
#
# The output starts with the sha256 of Cargo.lock it was generated from; the
# gate compares that to the current lockfile, so a dependency change that is
# not followed by a re-run fails `bun run ci` without needing cargo-about on
# the gate machine. Requires: cargo install cargo-about --features cli
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
OUT=packages/parasma-core/THIRD-PARTY-LICENSES.md
LOCK_SHA="$(shasum -a 256 Cargo.lock | cut -d' ' -f1)"
{
  echo "<!-- Cargo.lock sha256: $LOCK_SHA -->"
  cargo about generate --manifest-path crates/parasma-wasm/Cargo.toml \
    --features sml,pml,emf scripts/wasm-licenses.hbs
} > "$OUT.tmp"
mv "$OUT.tmp" "$OUT"
echo "wrote $OUT ($(grep -c '^## ' "$OUT") sections)"
