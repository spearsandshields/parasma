#!/bin/bash
# The LayoutStats census, as a runnable protocol with in-tree baselines.
#
# Any refactor of `run_layout_impl` must leave both censuses byte-identical.
# Before this script, the command was written out by hand, and that version
# was wrong three ways, each failing silently green (measured 2026-07-28):
#
#   1. stderr must go TO A FILE. `2>&1 | grep` splices libtest's stdout
#      progress lines INTO census rows, so two identical runs differ; and
#      `2>&1 >/dev/null | grep` is NOT the fix under zsh (MULTIOS tees it
#      anyway). This script therefore never merges streams.
#   2. `--test-threads=4` emits rows in thread-completion order → LC_ALL=C sort.
#   3. without `--nocapture` libtest swallows the eprintln! and the census is
#      EMPTY — and diffing two empty files PASSES. This script fails on empty.
#
# NOT part of `bun run ci`, deliberately: it is a second full suite run (~25 s)
# and gates only `run_layout_impl` refactors, not every commit. Run it before and after
# any change to `run_layout_impl` or the macros inside it.
#
# Usage:
#   scripts/census.sh check    # diff against crates/parasma-wasm/census/
#   scripts/census.sh bless    # overwrite the baselines with this run
#
# Three tests read gitignored inputs (.calibration manifest, fixtures/) and
# degrade to skip on a clean checkout; their rows live in the *.machine-bound
# baselines and are compared only when present in the run. Everything else is
# clean-checkout-reproducible.
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-check}"
BASE=crates/parasma-wasm/census
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# The machine-bound test list. A new fixture-reading test will surface as an
# unexplained new row in the public diff — classify it here (or fix it) then.
MACHINE_BOUND='^STATS[A-Z]* test=(ground_truth::word_ground_truth|oracle_tests::extract_range_carries_anchored_image|oracle_tests::object_hit_test_survives_clear_image_bytes) '

echo "── census run (full suite, --nocapture, stderr to file) ──"
PARASMA_STATS_CENSUS=1 cargo test -p parasma-wasm --features sml,pml,emf --locked \
  -- --test-threads=4 --nocapture >"$TMP/stdout" 2>"$TMP/stderr" \
  || { echo "SUITE FAILED — census meaningless"; tail -20 "$TMP/stdout"; exit 1; }

for stream in STATSCENSUS STATSROLLBACK; do
  grep "^$stream " "$TMP/stderr" | LC_ALL=C sort >"$TMP/$stream.all" || true
  grep -Ev "$MACHINE_BOUND" "$TMP/$stream.all" >"$TMP/$stream.public" || true
  grep -E  "$MACHINE_BOUND" "$TMP/$stream.all" >"$TMP/$stream.mb" || true
done

# the vacuous-empty trap: an empty census means the instrument broke, not "no data"
[ -s "$TMP/STATSCENSUS.public" ] || { echo "EMPTY census — instrument or capture broke (see header, failure 3)"; exit 1; }

fail=0
compare () { # $1 = produced, $2 = baseline, $3 = label, $4 = machine-bound
  # A machine-bound stream with no rows is only "inputs absent" if the BASELINE
  # expects rows. An empty-vs-empty pair is a real, trivially-identical result —
  # saying "inputs absent" there would be false on any machine (measured on one
  # where the inputs exist and the stream is legitimately empty).
  if [ "${4:-}" = optional ] && [ ! -s "$1" ] && [ -s "$2" ]; then
    echo "  ~ $3: baseline has rows but this run produced none — gitignored inputs likely absent; skipped, not failed"; return
  fi
  if diff -u "$2" "$1" >"$TMP/diff.$3" 2>/dev/null; then
    echo "  ✓ $3: $(wc -l <"$1" | tr -d ' ') rows byte-identical"
  else
    echo "  ✗ $3 DIFFERS from baseline:"; sed -n '1,25p' "$TMP/diff.$3"; fail=1
  fi
}

case "$MODE" in
  bless)
    mkdir -p "$BASE"
    cp "$TMP/STATSCENSUS.public"   "$BASE/statscensus.txt"
    cp "$TMP/STATSCENSUS.mb"       "$BASE/statscensus.machine-bound.txt"
    cp "$TMP/STATSROLLBACK.public" "$BASE/statsrollback.txt"
    cp "$TMP/STATSROLLBACK.mb"     "$BASE/statsrollback.machine-bound.txt"
    wc -l "$BASE"/*.txt
    echo "BLESSED — review the diff before committing; a blessed regression is invisible forever"
    ;;
  check)
    compare "$TMP/STATSCENSUS.public"   "$BASE/statscensus.txt"                "statscensus"
    compare "$TMP/STATSROLLBACK.public" "$BASE/statsrollback.txt"              "statsrollback"
    compare "$TMP/STATSCENSUS.mb"       "$BASE/statscensus.machine-bound.txt"  "statscensus-mb"   optional
    compare "$TMP/STATSROLLBACK.mb"     "$BASE/statsrollback.machine-bound.txt" "statsrollback-mb" optional
    [ "$fail" = 0 ] && echo "CENSUS IDENTICAL" || { echo "CENSUS CHANGED — if intended, re-bless and say why in the commit"; exit 1; }
    ;;
  *) echo "usage: $0 [check|bless]"; exit 2;;
esac
