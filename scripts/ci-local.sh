#!/usr/bin/env bash
#
# ci-local.sh — the local gate. Run this before every commit/push.
#
# This script is the single definition of the gate. `.github/workflows/ci.yml`
# runs exactly this on a clean machine; nothing is checked there that is not
# checked here.
#
# Everything runs at --features sml,pml,emf, the feature set that ships.
# `--all-targets` clippy is reported but NOT fatal: it surfaces lints in
# test-only code that has never been linted. Burning those down is its own
# task; gating on them now would just get this script disabled. Run
# `bun run ci:strict` to see them.
#
# What this CANNOT catch, and why the workflow exists: anything that only
# breaks on a CLEAN CHECKOUT. Your machine has fonts fetched, wasm built and
# node_modules populated. A fresh clone once could not build because tsdown's
# config loader fell through to an optional peer that bun's native loader was
# masking; that was invisible locally and broken for everyone else.
#
# Usage:  bun run ci          (or: ./scripts/ci-local.sh)
#         bun run ci:strict   (adds the non-fatal --all-targets lint report)
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

STRICT=0
[ "${1:-}" = "--strict" ] && STRICT=1

FAILED=()
START=$SECONDS

bold()  { printf '\n\033[1m── %s\033[0m\n' "$*"; }
ok()    { printf '\033[32m  ✓ %s\033[0m\n' "$*"; }
bad()   { printf '\033[31m  ✗ %s\033[0m\n' "$*"; FAILED+=("$1"); }
note()  { printf '\033[33m  ! %s\033[0m\n' "$*"; }

run() { # run <label> <cmd...>
  local label="$1"; shift
  if "$@" >/tmp/ci-local.$$ 2>&1; then
    ok "$label"
  else
    bad "$label"
    printf '\033[2m'; tail -25 /tmp/ci-local.$$ | sed 's/^/      /'; printf '\033[0m'
  fi
  rm -f /tmp/ci-local.$$
}

# ── 0. Preconditions ─────────────────────────────────────────────────────────
bold "preconditions"
if [ ! -f packages/parasma-core/src/wasm/parasma_wasm.d.ts ]; then
  bad "wasm bindings missing — TS typecheck cannot run"
  note "fix: bun run build:wasm:full"
  echo
  printf '\033[31mGATE FAILED (missing prerequisite)\033[0m\n'
  exit 1
fi
ok "wasm bindings present"
if [ ! -f fixtures/manifest.json ]; then
  bad "fixtures missing — seven native tests read documents under fixtures/"
  note "fix: bun run scripts/fetch-fixtures.ts"
  echo
  printf '\033[31mGATE FAILED (missing prerequisite)\033[0m\n'
  exit 1
fi
ok "fixtures present"

# STALE-WASM GUARD: typecheck reads packages/parasma-core/src/wasm/*.d.ts, which is
# BUILD OUTPUT. Change a #[wasm_bindgen] export without rebuilding and the TS arm
# validates against a stale contract and passes — a known false-negative in this
# repo. Compare the bindings' mtime against the newest Rust source.
NEWEST_RS="$(find crates -name '*.rs' -newer packages/parasma-core/src/wasm/parasma_wasm.d.ts -print -quit 2>/dev/null)"
if [ -n "$NEWEST_RS" ]; then
  note "wasm bindings are OLDER than $NEWEST_RS"
  note "TS typecheck may be validating a stale contract — run: bun run build:wasm:full"
else
  ok "wasm bindings newer than all Rust sources"
fi

# ── 1. Architecture invariants (cheap, fail fast) ────────────────────────────
# Framework ban: @parasma/core and @parasma/headless MUST stay framework-free.
bold "architecture invariants"
if grep -rnE "from ['\"](react|react-dom|vue|svelte|@angular/|preact|solid-js)" \
     packages/parasma-core/src packages/headless/src >/dev/null 2>&1; then
  bad "framework import leaked into core/headless (framework ban)"
  grep -rnE "from ['\"](react|react-dom|vue|svelte|@angular/|preact|solid-js)" \
     packages/parasma-core/src packages/headless/src | sed 's/^/      /'
else
  ok "core + headless are framework-free"
fi

# The layering rules (root-scoping, import direction, engine/ membership in
# both directions), enforced by scripts/check-layering.mjs.
if layering_out=$(node scripts/check-layering.mjs 2>&1); then
  ok "layering rules hold"
else
  bad "layering violation"
  printf '%s\n' "$layering_out" | sed 's/^/      /'
fi

# ── 2. Rust ──────────────────────────────────────────────────────────────────
bold "rust (features: sml,pml,emf — what ships)"
run "clippy -D warnings" \
  cargo clippy -p parasma-wasm --features sml,pml,emf --locked -- -D warnings
run "native tests" \
  cargo test -p parasma-wasm --features sml,pml,emf --locked

# Surface the ground-truth summary. libtest swallows a PASSING test's
# stdout, so without this a run that compared NOTHING to Word (clean checkout,
# CI — every calibration document absent) is indistinguishable from a green one.
if [ -f target/ground-truth-report.txt ]; then
  gt_line=$(grep -m1 "GROUND-TRUTH SUMMARY" target/ground-truth-report.txt || true)
  if grep -q "0 CHECKED" target/ground-truth-report.txt; then
    note "${gt_line:-ground truth: no summary} — compared nothing to Word"
  else
    ok "${gt_line:-ground truth reported}"
  fi
fi

# ── 3. TypeScript ────────────────────────────────────────────────────────────
bold "typescript"
for p in parasma-core headless snippet vue react; do
  run "typecheck @parasma/$p" bash -c "cd packages/$p && bunx tsc --noEmit"
done
run "unit tests" bun run --filter '*' test

# ── 4. Ship-path coverage ────────────────────────────────────────────────────
# Three things the earlier gate did not cover:
# size (report-only here; the CI workflow enforces the gzip budget), the demo build (what
# scripts/build-demo-site.sh actually ships), and the publish manifest swap
# (a prepack/postpack regression otherwise surfaces only at publish).
bold "ship-path"
WASM_ARTIFACT=packages/parasma-core/src/wasm/parasma_wasm_bg.wasm
if [ -f "$WASM_ARTIFACT" ]; then
  raw_b=$(wc -c < "$WASM_ARTIFACT" | tr -d ' ')
  gz_b=$(gzip -9 -c "$WASM_ARTIFACT" | wc -c | tr -d ' ')
  raw_mb=$(awk -v b="$raw_b" 'BEGIN{printf "%.2f", b/1048576}')
  gz_mb=$(awk -v b="$gz_b" 'BEGIN{printf "%.2f", b/1048576}')
  note "wasm size: ${raw_mb} MB raw, ${gz_mb} MB gzip — REPORT ONLY (the CI workflow enforces the gzip budget)"
fi
run "snippet styles generated-module agreement" node scripts/gen-snippet-styles.mjs --check
# The wasm licence inventory is generated from Cargo.lock and records the
# lockfile's sha256 on its first line; a dependency change must re-run the
# generator (needs cargo-about), but detecting the drift needs only shasum.
run "runtime JS dependency licences allowed" node scripts/check-licenses.mjs
run "wasm licence inventory matches Cargo.lock" bash -c '
  want="$(shasum -a 256 Cargo.lock | cut -d" " -f1)"
  have="$(head -1 packages/parasma-core/THIRD-PARTY-LICENSES.md | grep -oE "[0-9a-f]{64}")"
  [ "$want" = "$have" ] || { echo "stale: run ./scripts/gen-wasm-licenses.sh"; exit 1; }
'
run "demo build (what the demo site ships)" bash -c "cd packages/snippet && bun run build:demo >/dev/null"
# npm (not `bun pm`) on purpose: this script usually runs under `bun run ci`,
# and a nested `bun pm pack` deadlocks on bun's own process lock (measured:
# the gate hung forever; direct ./scripts/ci-local.sh passed). npm runs the
# same prepack/postpack lifecycle without touching bun's lock.
# Compares each package.json BEFORE vs AFTER the pack — NOT against git: a
# legitimately-dirty manifest (work in progress) must not fail the round-trip,
# only a pack that fails to restore what it found. (A git-diff version of
# this check false-positived on ordinary in-progress edits.)
run "publish manifest swap round-trips" bash -c '
  set -euo pipefail
  tmp="$(mktemp -d)"
  for p in parasma-core headless snippet vue react; do
    cp "packages/$p/package.json" "$tmp/$p.before"
  done
  for p in parasma-core headless snippet vue react; do
    (cd "packages/$p" && npm pack --quiet --pack-destination "$tmp" >/dev/null)
  done
  for p in parasma-core headless snippet vue react; do
    if ! cmp -s "$tmp/$p.before" "packages/$p/package.json"; then
      echo "prepack/postpack did not restore packages/$p/package.json to its pre-pack bytes:"
      diff "$tmp/$p.before" "packages/$p/package.json" | head -10; exit 1
    fi
  done
  rm -rf "$tmp"
  if ls packages/*/package.json.prepack-orig >/dev/null 2>&1; then
    echo "leftover package.json.prepack-orig after pack"; exit 1
  fi
'

# ── 5. Non-fatal: lints CI never compiled ────────────────────────────────────
if [ "$STRICT" = 1 ]; then
  bold "strict lint report (non-fatal)"
  n=$(cargo clippy -p parasma-wasm --features sml,pml,emf --all-targets --locked 2>&1 \
        | grep -cE "^(warning|error)(\[|:)" || true)
  note "--all-targets surfaces $n lints (test-code included). Not gated."
fi

# ── Summary ──────────────────────────────────────────────────────────────────
ELAPSED=$((SECONDS - START))
echo
if [ ${#FAILED[@]} -eq 0 ]; then
  printf '\033[32m\033[1mGATE PASSED\033[0m  (%ss)\n' "$ELAPSED"
  exit 0
fi
printf '\033[31m\033[1mGATE FAILED\033[0m  (%ss) — %d check(s):\n' "$ELAPSED" "${#FAILED[@]}"
for f in "${FAILED[@]}"; do printf '   • %s\n' "$f"; done
exit 1
