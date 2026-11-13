#!/usr/bin/env bash
set -euo pipefail

# Fetches the test fonts used by the native layout-regression harness
# (crates/parasma-wasm — see the oracle snapshot tests) into the gitignored
# crates/parasma-wasm/test-fonts/ directory.
#
# Policy: NO binaries in git. Fonts are pinned to an exact upstream commit and
# sha256-verified, so the layout baselines they produce are reproducible on any
# machine. Arimo is SIL OFL licensed (https://github.com/googlefonts/arimo).
#
# Every family here is ALSO served to the browser by
# packages/parasma-core/src/font-resolver.ts (FONT_URLS). The two files must name
# the SAME commit — otherwise the browser renders against different bytes than
# these baselines were calibrated on. font-pinning.test.ts fails if they drift
# apart, and fails if any commit below stops being a 40-hex sha.
#
# Usage: ./scripts/fetch-test-fonts.sh

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$REPO_ROOT/crates/parasma-wasm/test-fonts"
mkdir -p "$DEST"

# googlefonts/arimo, upstream commit of 2026-04-27 — bump deliberately; baselines
# depend on exact glyph metrics, so a font bump requires re-baselining the
# snapshots.
ARIMO_COMMIT="4a6255f269916ae7ad3fc2706b0935e7621396b8"

# fetch_url FILE SHA URL — generic pinned + sha256-verified fetcher.
fetch_url() {
  local file="$1" sha="$2" url="$3"
  local out="$DEST/$file"

  if [ -f "$out" ] && echo "$sha  $out" | shasum -a 256 -c - >/dev/null 2>&1; then
    echo "ok (cached): $file"
    return
  fi

  echo "fetching: $file"
  curl -sSfL -o "$out" "$url"
  if ! echo "$sha  $out" | shasum -a 256 -c - >/dev/null 2>&1; then
    echo "ERROR: sha256 mismatch for $file — refusing to keep it" >&2
    rm -f "$out"
    exit 1
  fi
  echo "ok (fetched): $file"
}

fetch() {
  local file="$1" sha="$2"
  fetch_url "$file" "$sha" \
    "https://cdn.jsdelivr.net/gh/googlefonts/arimo@${ARIMO_COMMIT}/fonts/ttf/${file}"
}

fetch "Arimo-Regular.ttf"    "41b22bc8f0b51f932825d37bc55b5eb6ba67dfe599a626e4aff2b43b624f9f8c"
fetch "Arimo-Bold.ttf"       "d7a8b187cf8444d4cfee102e8eae9e3043682fd5106d5d33ed677fe268a0e2ba"
fetch "Arimo-Italic.ttf"     "ffdf690e4357a392e4edcc3df0b3adb0302920f75e54f2e6eec355afbe7edcba"
fetch "Arimo-BoldItalic.ttf" "77c967d49d4ef4a63c39fbcd1e27892102e503e35a20983cb058430c4f268b34"

# OMML: XITS Math (a STIX fork, SIL OFL) — the real OpenType math face
# carrying a MATH table. The native oracle harness registers this as the math
# family so the pinned geometry reflects the real MATH-table path (constants +
# size variants + alphabet coverage) that ships in the browser.
# The commit is tag v1.302; a tag is a MOVABLE ref, so name the commit.
XITS_COMMIT="81258e3318693741a50bd921d87c73d497103d37"
fetch_url "XITSMath-Regular.otf" \
  "3025792adb0b7072ace08bf5726d341de32f2254612ddec6dd98271a8dd29689" \
  "https://cdn.jsdelivr.net/gh/aliftype/xits@${XITS_COMMIT}/XITSMath-Regular.otf"

# ── complex-script shaping coverage ──────────────────────────────────────────
# Noto Sans Arabic (SIL OFL), SAME face + SAME pinned commit as
# font-resolver.ts's on-demand Arabic slot (font-pinning.test.ts enforces the
# agreement). The GLYPH stream defended ~28,400 Latin advances and 1 nonzero
# offset before this: Arabic brings real ligatures (lam-alef), mark
# attachment (nonzero x/y offsets) and many-to-one clusters.
NOTO_COMMIT="eaa1a5cf8cb83ea73941197e492d659e51bb11dd"
fetch_url "NotoSansArabic-Regular.ttf" \
  "bdff3e5659d67e67def05b33f749683b9376ae819d65d3dd62ac4640b3aaef48" \
  "https://cdn.jsdelivr.net/gh/notofonts/notofonts.github.io@${NOTO_COMMIT}/fonts/NotoSansArabic/hinted/ttf/NotoSansArabic-Regular.ttf"


# ── ground-truth substitutes ─────────────────────────────────────────────────
# The calibration documents (docs/calibration-ids.md) declare Calibri, Times New
# Roman, DejaVu Sans and Open Sans. The ground-truth harness loads these four
# families IN ADDITION to Arimo so its font basis matches what actually ships
# in the browser (font-resolver.ts FONT_URLS + font-map.json): Carlito is the
# Calibri metric-clone, Tinos the Times New Roman metric-clone, and DejaVu
# Sans / Open Sans are libre so the REAL faces load. Only ground_truth.rs
# consumes them — `load_test_fonts` (the oracle basis) is deliberately
# unchanged, so every pinned baseline keeps its meaning.
# Licences: Carlito/Tinos/Open Sans SIL OFL; DejaVu is the free Bitstream
# Vera licence.
CARLITO_COMMIT="3a810cab78ebd6e2e4eed42af9e8453c4f9b850a"
TINOS_COMMIT="3b4482a99b80ea5fc75f187b1be3120a3f5905b3"
OPENSANS_COMMIT="bd7e37632246368c60fdcbd374dbf9bad11969b6"

fetch_gh() { # FILE SHA REPO COMMIT
  fetch_url "$1" "$2" "https://cdn.jsdelivr.net/gh/googlefonts/$3@$4/fonts/ttf/$1"
}

fetch_gh "Carlito-Regular.ttf"    "f6418f708baede9789daef5d458c0f53d2a888af9820e8062934e504fedc6595" carlito "$CARLITO_COMMIT"
fetch_gh "Carlito-Bold.ttf"       "bb5d20f79b82599ec72983597437373a80f2d2085fa91fc144fd74e876a594db" carlito "$CARLITO_COMMIT"
fetch_gh "Carlito-Italic.ttf"     "0b019225e58d702bfedcbd35c21696769f8ee115cb6343f84c2f240312450d1c" carlito "$CARLITO_COMMIT"
fetch_gh "Carlito-BoldItalic.ttf" "b32928186c119599e03ca6a1ffc680fdcb7fac95772f4b95d989cf6cd3861517" carlito "$CARLITO_COMMIT"

fetch_gh "Tinos-Regular.ttf"      "60a0e8ef0c04dd5dd69ffe91025fa2ae5836cbd35600a82ba031977557e2cb61" tinos "$TINOS_COMMIT"
fetch_gh "Tinos-Bold.ttf"         "393269dbab8899f938db19783eca5eac92eb431f7ae0ab45b8349ca895f1a06b" tinos "$TINOS_COMMIT"
fetch_gh "Tinos-Italic.ttf"       "5942266ed398b155d7dc23e36833e7ec6be988f2439bdbeb8ef1bede808eaa91" tinos "$TINOS_COMMIT"
fetch_gh "Tinos-BoldItalic.ttf"   "a5de79f0fe863ea0954757acb3d47b3ccd0a930ce3dd5b97230cd3866790a06e" tinos "$TINOS_COMMIT"

fetch_gh "OpenSans-Regular.ttf"    "c53aceea2dcf5b4098099c0c4d0a061d17e178a049317b42a422b1a9f7f8eb59" opensans "$OPENSANS_COMMIT"
fetch_gh "OpenSans-Bold.ttf"       "27da758f4dcac9a65abe914c13b463b42982b9909bc65713424099f4810bd1e6" opensans "$OPENSANS_COMMIT"
fetch_gh "OpenSans-Italic.ttf"     "93bc1bb6abf4e6b7c75d7131714061d5b57cc478abcabe4cb3519bb38fb917aa" opensans "$OPENSANS_COMMIT"
fetch_gh "OpenSans-BoldItalic.ttf" "d672a770037104b6af45e1336b3d3c1729c8aea940f81e010f5a8a7319c29a21" opensans "$OPENSANS_COMMIT"

# DejaVu: npm package. An EXACT published version is immutable, so it is already
# a pin in the sense the header describes — a range (^2.37.3) would not be.
fetch_dejavu() { # FILE SHA
  fetch_url "$1" "$2" "https://cdn.jsdelivr.net/npm/dejavu-fonts-ttf@2.37.3/ttf/$1"
}
fetch_dejavu "DejaVuSans.ttf"             "7da195a74c55bef988d0d48f9508bd5d849425c1770dba5d7bfc6ce9ed848954"
fetch_dejavu "DejaVuSans-Bold.ttf"        "e6476c1b80502924294eed40894c5b18e06c181444ca953e5334262df9c27724"
fetch_dejavu "DejaVuSans-Oblique.ttf"     "4af75fa16ee6d3ad43e1ecec41862c24954af26a55c6bb1ebb27bd486a50f5f4"
fetch_dejavu "DejaVuSans-BoldOblique.ttf" "eb436dca0c2594b73d8b603b892e374fdfd8d885d25ffb4f18df4c4c0b49e50f"

echo "test fonts ready in $DEST"
