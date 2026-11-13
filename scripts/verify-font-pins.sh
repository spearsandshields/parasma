#!/usr/bin/env bash
set -uo pipefail

# verify-font-pins.sh — the NETWORKED half of the font-pinning check.
#
# font-pinning.test.ts proves, offline and in the gate, that every bundled font
# URL names an immutable ref and that the browser and the native harness name the
# same one. What it cannot prove is that the CDN still serves the same BYTES for
# that ref — an upstream force-push or a jsDelivr cache poisoning would slip
# past it. This script is that check, and it is deliberately NOT in the gate:
# it is network-bound, and a flaky gate is a disabled gate.
#
# Run it when you change a pin in either file, and after any upstream incident.
#
#   PASS/FAIL is asserted only for the families the native harness also fetches
#   (scripts/fetch-test-fonts.sh carries their expected sha256). For the
#   browser-only families there is no stored expectation, so the sha256 is
#   PRINTED — diff it against a previous run, or against the values recorded in
#   the commit that introduced the pin.
#
# Usage: ./scripts/verify-font-pins.sh

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# The URLs come from the shipping module, never a restatement of it.
URLS="$(cd "$ROOT/packages/parasma-core" && bun -e \
  'import { BUNDLED_FONT_URLS } from "./src/font-resolver"; console.log(BUNDLED_FONT_URLS.join("\n"))')"
if [ -z "$URLS" ]; then
  echo "ERROR: could not read BUNDLED_FONT_URLS from @parasma/core" >&2
  exit 1
fi

# The native harness is the only place an expected sha256 is written down. Its
# fetch calls wrap over line continuations, so join those before matching.
EXPECTED="$TMP/fetch-test-fonts.joined"
awk '{ while (sub(/\\$/, "")) { if ((getline nxt) <= 0) break; $0 = $0 nxt } print }' \
  "$ROOT/scripts/fetch-test-fonts.sh" > "$EXPECTED"

CHECKED=0; PRINTED=0; FAILED=0

while IFS= read -r url; do
  [ -z "$url" ] && continue
  file="$(basename "${url%%\?*}")"
  file="$(printf '%b' "${file//%/\\x}")"   # Inter%5Bopsz%2Cwght%5D.ttf → decoded

  if ! curl -sSfL -o "$TMP/f" "$url"; then
    printf 'FAIL  %-26s fetch failed: %s\n' "$file" "$url"
    FAILED=$((FAILED + 1)); continue
  fi
  got="$(shasum -a 256 "$TMP/f" | cut -d' ' -f1)"

  # `fetch* "<file>" "<sha256>"` — absent for the browser-only families.
  want="$(grep -oE "\"${file//./\\.}\"[[:space:]]+\"[0-9a-f]{64}\"" "$EXPECTED" \
            | grep -oE '[0-9a-f]{64}' | head -1)"

  if [ -n "$want" ]; then
    if [ "$got" = "$want" ]; then
      printf 'ok    %-26s %s\n' "$file" "$got"
      CHECKED=$((CHECKED + 1))
    else
      printf 'FAIL  %-26s got %s\n      %-26s want %s (fetch-test-fonts.sh)\n' \
        "$file" "$got" "" "$want"
      FAILED=$((FAILED + 1))
    fi
  else
    printf '  ?   %-26s %s  (browser-only — no stored expectation)\n' "$file" "$got"
    PRINTED=$((PRINTED + 1))
  fi
done <<< "$URLS"

echo
if [ "$CHECKED" -eq 0 ]; then
  echo "ERROR: no artefact matched an expected sha256 in fetch-test-fonts.sh." >&2
  echo "That is a broken check, not a pass — the two files should overlap." >&2
  exit 1
fi
if [ "$FAILED" -gt 0 ]; then
  echo "FONT PINS CHANGED — $FAILED mismatch(es). The CDN is serving different bytes"
  echo "than the baselines were calibrated on. Do NOT re-baseline before finding out why."
  exit 1
fi
echo "font pins verified: $CHECKED asserted against fetch-test-fonts.sh, $PRINTED printed"
