#!/usr/bin/env bash
#
# publish.sh — publish every @parasma package at the current version, in
# dependency order.
#
#   ./scripts/publish.sh [--dry-run] [any other `bun publish` flag]
#
# All packages share one version and are released together: a package with
# no changes still gets the version bump, so "same version" always means
# "built and tested together". Run the gate and ./scripts/pack-smoke.sh first.
#
# Each package's prepack hook builds dist/ and swaps in its publish manifest;
# bun rewrites the workspace:* ranges to the shared version. npm asks for a
# one-time code per package when the account has two-factor auth on writes.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ORDER=(parasma-core headless snippet vue react)

version=""
for p in "${ORDER[@]}"; do
  v="$(node -p "require('./packages/$p/package.json').version")"
  if [ -z "$version" ]; then version="$v"; fi
  [ "$v" = "$version" ] || { echo "version mismatch: packages/$p is $v, expected $version" >&2; exit 1; }
done

echo "Publishing @parasma/* $version: ${ORDER[*]}"
for p in "${ORDER[@]}"; do
  echo "── @parasma/${p#parasma-}"
  ( cd "packages/$p" && bun publish --access public "$@" )
done
