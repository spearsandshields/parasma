#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# check-wasm-identity.sh — docx-only wasm binary-identity + gzip size-budget gate
# ============================================================================
#
# Purpose: the mechanical "the docx-only build is
# unaffected" proof to run after every future SML/PML milestone. Building with
# no new format features and asserting the wasm is byte-identical to a recorded
# baseline mechanically proves that adding Excel/PowerPoint code changed neither
# the behavior nor the download size paid by docx-only users.
#
# Reproducibility finding (2026-06-06):
#   wasm-pack's output (packages/parasma-core/src/wasm/parasma_wasm_bg.wasm) is
#   BIT-REPRODUCIBLE on this toolchain across back-to-back AND full-recompile
#   builds. Three builds — incremental, cached, and a forced full recompile
#   after `touch lib.rs` — all produced sha256
#     62f205db85c88505e8dd26746d1c113350e9b6d2cf7c11c36e340b1c28b76b04
#   (rustc embeds no build timestamp; wasm-pack runs its bundled wasm-opt
#   deterministically; the Rust path-remapping makes object code path-stable).
#   So byte-identity is the PRIMARY assertion.
#
#   Caveat for future hands / CI: bit-reproducibility is contingent on an
#   IDENTICAL toolchain (rustc, wasm-bindgen, wasm-pack/wasm-opt versions).
#   A toolchain bump can legitimately change bytes without any source change.
#   For that reason a differing sha is NOT a bare failure — the script reports
#   exactly what differs: raw/gzip size deltas and (if `wasm-objdump` is
#   available) the exported-symbol set diff. A changed symbol set is a real
#   behavioral red flag for a docx-only build; identical symbols + tiny size
#   delta points at a toolchain/codegen change to investigate, not necessarily
#   a regression. The gzip BUDGET is always enforced regardless of sha.
#
# Modes:
#   --record   build wasm, write {sha256, raw, gzip, date, git rev} to
#              wasm-baseline.json (gitignored: it names a commit and a compiler
#              the next reader does not have, so each machine records its own).
#   (default)  build wasm, compare against wasm-baseline.json:
#                identical sha           -> PASS "byte-identical"
#                differing sha           -> report size/symbol diffs (FAIL)
#              and ALWAYS enforce GZIP_BUDGET. Non-zero exit on any failure.
#
# Writes nothing but wasm-baseline.json.
# ============================================================================

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASELINE="$REPO_ROOT/wasm-baseline.json"

# Build the docx-only wasm into a throwaway temp dir, NOT the served pkg dir
# (packages/parasma-core/src/wasm). The served dir holds the demo's
# sml,pml artifact; wasm-pack'ing a docx-only build into it would clobber that
# 4.6MB build with a docx-only shrink. Building to a temp out-dir keeps this
# gate self-contained — it edits only this file + wasm-baseline.json and leaves
# the served dir untouched.
WASM_OUT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/parasma-wasm-identity.XXXXXX")"
trap 'rm -rf "$WASM_OUT_DIR"' EXIT
WASM="$WASM_OUT_DIR/parasma_wasm_bg.wasm"

# Hard ceiling on the gzip-transfer size of the docx-only wasm (bytes).
# The original baseline gzip was ~978 KB. Bumped 1050000 → 1075000 (2026-06-09) for
# the consolidated parser-repair module (src/repair/): RULE 2 (docx gutter-less
# <w:pgMar>) legitimately compiles the zip-WRITE path into the docx-only build,
# because the WML fork takes the whole package and the repair must re-zip the
# patched document.xml (~6 KB gzip). Deliberate, not feature creep — lenient
# codegen in the parser fork would delete the module and reclaim this. Still the
# line that, if crossed, should prompt splitting out a second, lazily loaded wasm.
GZIP_BUDGET=1075000

MODE="verify"
if [ "${1:-}" = "--record" ]; then
  MODE="record"
elif [ -n "${1:-}" ]; then
  echo "usage: $0 [--record]" >&2
  exit 2
fi

# --- helpers ----------------------------------------------------------------

sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
raw_size_of() { stat -f%z "$1" 2>/dev/null || stat -c%s "$1"; }
gzip_size_of() { gzip -c "$1" | wc -c | tr -d ' '; }

# Read a flat string/number field out of wasm-baseline.json without a JSON dep.
json_field() {
  # $1 = key, $2 = file
  grep -o "\"$1\"[[:space:]]*:[[:space:]]*\"\?[^\",}]*" "$2" \
    | head -1 | sed -E 's/.*:[[:space:]]*"?//'
}

# Exported-symbol set (sorted), used as a behavioral diff when sha differs.
# Empty if wasm-objdump is unavailable.
export_symbols_of() {
  command -v wasm-objdump >/dev/null 2>&1 || return 0
  wasm-objdump -j Export -x "$1" 2>/dev/null \
    | grep -oE -- '-> "[^"]*"' | sed -E 's/-> "//; s/"$//' | sort
}

# --- build ------------------------------------------------------------------

echo ">> building docx-only wasm (wasm-pack -> temp out-dir) ..."
"$REPO_ROOT/scripts/build-wasm.sh" --out-dir "$WASM_OUT_DIR" >/dev/null 2>&1 \
  || { echo "!! wasm build failed — re-run './scripts/build-wasm.sh --out-dir <dir>' to see errors" >&2; exit 1; }

[ -f "$WASM" ] || { echo "!! expected wasm not found at $WASM" >&2; exit 1; }

CUR_SHA="$(sha256_of "$WASM")"
CUR_RAW="$(raw_size_of "$WASM")"
CUR_GZIP="$(gzip_size_of "$WASM")"
GIT_REV="$(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo unknown)"
TODAY="$(date +%Y-%m-%d)"

# --- record mode ------------------------------------------------------------

if [ "$MODE" = "record" ]; then
  SYMCOUNT="$(export_symbols_of "$WASM" | wc -l | tr -d ' ')"
  SYMHASH="$(export_symbols_of "$WASM" | shasum -a 256 | awk '{print $1}')"
  cat > "$BASELINE" <<EOF
{
  "_comment": "docx-only (no SML/PML features) wasm binary-identity baseline. Recorded deliberately at milestone boundaries via scripts/check-wasm-identity.sh --record. sha256 is bit-reproducible on a fixed toolchain; a toolchain bump requires re-recording.",
  "artifact": "packages/parasma-core/src/wasm/parasma_wasm_bg.wasm",
  "sha256": "$CUR_SHA",
  "raw_bytes": $CUR_RAW,
  "gzip_bytes": $CUR_GZIP,
  "gzip_budget": $GZIP_BUDGET,
  "export_symbol_count": $SYMCOUNT,
  "export_symbols_sha256": "$SYMHASH",
  "git_rev": "$GIT_REV",
  "date": "$TODAY"
}
EOF
  echo ">> recorded baseline -> $BASELINE"
  echo "   sha256:     $CUR_SHA"
  echo "   raw bytes:  $CUR_RAW"
  echo "   gzip bytes: $CUR_GZIP  (budget $GZIP_BUDGET)"
  echo "   git rev:    $GIT_REV   date: $TODAY"
  exit 0
fi

# --- verify mode ------------------------------------------------------------

[ -f "$BASELINE" ] || { echo "!! no baseline at $BASELINE — run '$0 --record' first" >&2; exit 1; }

BASE_SHA="$(json_field sha256 "$BASELINE")"
BASE_RAW="$(json_field raw_bytes "$BASELINE")"
BASE_GZIP="$(json_field gzip_bytes "$BASELINE")"

FAIL=0

echo ">> verifying against baseline ($BASELINE, rev $(json_field git_rev "$BASELINE") $(json_field date "$BASELINE"))"

if [ "$CUR_SHA" = "$BASE_SHA" ]; then
  echo "PASS: byte-identical to baseline (sha256 $CUR_SHA)"
else
  echo "DIFF: wasm sha256 differs from baseline"
  echo "      baseline: $BASE_SHA"
  echo "      current:  $CUR_SHA"
  echo "      raw  bytes: baseline=$BASE_RAW current=$CUR_RAW (delta $((CUR_RAW - BASE_RAW)))"
  echo "      gzip bytes: baseline=$BASE_GZIP current=$CUR_GZIP (delta $((CUR_GZIP - BASE_GZIP)))"

  # Exported-symbol summary: the behavioral signal for a docx-only build.
  # The baseline records the export count + a hash of the sorted symbol set, so
  # this diagnostic needs no extra committed file. An UNCHANGED export set with a
  # differing sha points at a toolchain/codegen change (investigate, re-record if
  # the bump is intentional); a CHANGED set is a real red flag for docx-only.
  if command -v wasm-objdump >/dev/null 2>&1; then
    CUR_SYMCOUNT="$(export_symbols_of "$WASM" | wc -l | tr -d ' ')"
    CUR_SYMHASH="$(export_symbols_of "$WASM" | shasum -a 256 | awk '{print $1}')"
    BASE_SYMHASH="$(json_field export_symbols_sha256 "$BASELINE")"
    BASE_SYMCOUNT="$(json_field export_symbol_count "$BASELINE")"
    if [ -n "$BASE_SYMHASH" ] && [ "$CUR_SYMHASH" = "$BASE_SYMHASH" ]; then
      echo "      exported symbols: UNCHANGED ($CUR_SYMCOUNT exports) — codegen/toolchain change, not an API change"
    elif [ -n "$BASE_SYMHASH" ]; then
      echo "      exported symbols CHANGED (behavioral red flag for docx-only):"
      echo "        baseline: $BASE_SYMCOUNT exports (sha $BASE_SYMHASH)"
      echo "        current:  $CUR_SYMCOUNT exports (sha $CUR_SYMHASH)"
    else
      echo "      (baseline predates symbol recording; current: $CUR_SYMCOUNT exports, sha $CUR_SYMHASH)"
    fi
  else
    echo "      (wasm-objdump unavailable — install wabt for the exported-symbol summary)"
  fi
  FAIL=1
fi

# Gzip budget is enforced unconditionally.
if [ "$CUR_GZIP" -le "$GZIP_BUDGET" ]; then
  echo "PASS: gzip size $CUR_GZIP <= budget $GZIP_BUDGET"
else
  echo "FAIL: gzip size $CUR_GZIP EXCEEDS budget $GZIP_BUDGET (over by $((CUR_GZIP - GZIP_BUDGET)) bytes)"
  FAIL=1
fi

if [ "$FAIL" -ne 0 ]; then
  echo ">> RESULT: FAIL"
  exit 1
fi
echo ">> RESULT: PASS"
