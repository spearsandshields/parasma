#!/usr/bin/env python3
"""
Generate font-map.json from LibreOffice's VCL.xcu font substitution table.

Classifies 315+ proprietary/system font names into Google Fonts substitute
families with documented reasoning for each mapping.

Usage:
    python3 scripts/generate-font-map.py [/path/to/VCL.xcu]

If no path given, downloads VCL.xcu from LibreOffice's GitHub repo.
"""

import json
import re
import sys
import urllib.request

VCL_URL = "https://raw.githubusercontent.com/LibreOffice/core/master/officecfg/registry/data/org/openoffice/VCL.xcu"
OUTPUT = "packages/parasma-core/src/font-map.json"

# ─── Substitute families available on Google Fonts ───────────────────────────
#
# Quality tiers:
#   "metric-compatible"  — designed as drop-in replacement, identical glyph widths
#   "same-class"         — same typographic class (sans/serif/mono), similar proportions
#   "rough-fallback"     — visually different but functional; best available option
#   null                 — no web substitute available (symbol fonts, rare decorative)

SUBSTITUTES = {
    "Arimo":      {"css_name": "Arimo",      "weights": [400, 700], "styles": ["normal", "italic"]},
    "Tinos":      {"css_name": "Tinos",      "weights": [400, 700], "styles": ["normal", "italic"]},
    "Cousine":    {"css_name": "Cousine",     "weights": [400, 700], "styles": ["normal", "italic"]},
    "Carlito":    {"css_name": "Carlito",     "weights": [400, 700], "styles": ["normal", "italic"]},
    "Caladea":    {"css_name": "Caladea",     "weights": [400, 700], "styles": ["normal", "italic"]},
    "Noto Sans":  {"css_name": "Noto+Sans",   "weights": [400, 700], "styles": ["normal", "italic"]},
    "Noto Serif": {"css_name": "Noto+Serif",  "weights": [400, 700], "styles": ["normal", "italic"]},
}


# ─── Classification rules ───────────────────────────────────────────────────
#
# Each entry: (font_name_lowercase, substitute, quality, reason)
# Applied in order; first match wins. The VCL.xcu chain analysis below
# catches everything not explicitly listed here.

EXPLICIT_MAPPINGS = {
    # === Metric-compatible substitutes (designed as drop-in replacements) ===
    "calibri":          ("Carlito",  "metric-compatible", "Carlito is Google's metric-compatible clone of Calibri"),
    "calibrilight":     ("Carlito",  "metric-compatible", "Carlito covers Calibri Light weight range"),
    "carlito":          ("Carlito",  "metric-compatible", "Carlito is itself the substitute"),
    "cambria":          ("Caladea",  "metric-compatible", "Caladea is the metric-compatible clone of Cambria"),
    "cambriamath":      ("Caladea",  "metric-compatible", "Caladea substitutes for Cambria family; math glyphs may be missing"),
    "caladea":          ("Caladea",  "metric-compatible", "Caladea is itself the substitute"),
    "arial":            ("Arimo",    "metric-compatible", "Arimo is Google's metric-compatible clone of Arial"),
    "arimo":            ("Arimo",    "metric-compatible", "Arimo is itself the substitute"),
    "timesnewroman":    ("Tinos",    "metric-compatible", "Tinos is Google's metric-compatible clone of Times New Roman"),
    "tinos":            ("Tinos",    "metric-compatible", "Tinos is itself the substitute"),
    "couriernew":       ("Cousine",  "metric-compatible", "Cousine is Google's metric-compatible clone of Courier New"),
    "cousine":          ("Cousine",  "metric-compatible", "Cousine is itself the substitute"),

    # === Same-class: sans-serif fonts → Arimo ===
    "helvetica":        ("Arimo",    "same-class", "Helvetica and Arial share nearly identical metrics; Arimo matches Arial"),
    "helveticanarrow":  ("Arimo",    "same-class", "Condensed sans; Arimo is closest available sans on Google Fonts"),
    "verdana":          ("Arimo",    "same-class", "Humanist sans-serif; Arimo is closer in weight than Noto Sans. Verdana has wider glyphs — not metric-compatible"),
    "tahoma":           ("Arimo",    "same-class", "Tahoma is narrower than Verdana but still a humanist sans; Arimo is reasonable"),
    "segoe":            ("Arimo",    "same-class", "Segoe UI is Microsoft's UI font; Arimo is closest sans match"),
    "trebuchet":        ("Arimo",    "same-class", "Geometric sans; Arimo is closest available. Trebuchet has more distinctive letterforms"),
    "geneva":           ("Arimo",    "same-class", "macOS system sans-serif; Arimo substitutes adequately"),
    "lucidagrande":     ("Arimo",    "same-class", "macOS system sans-serif; Arimo is reasonable substitute"),
    "lucidasans":       ("Arimo",    "same-class", "Humanist sans-serif; Arimo covers the role"),
    "frutiger":         ("Arimo",    "same-class", "Humanist sans-serif; Arimo is closest available"),
    "futura":           ("Arimo",    "same-class", "Geometric sans; no metric-compatible web font exists"),
    "optima":           ("Arimo",    "same-class", "Humanist sans with serif hints; no good web match, Arimo is functional fallback"),
    "centurygothic":    ("Arimo",    "same-class", "Geometric sans (ITC Avant Garde family); Arimo is closest sans on Google Fonts"),
    "avantgarde":       ("Arimo",    "same-class", "ITC Avant Garde Gothic; geometric sans — Arimo is functional substitute"),
    "din":              ("Arimo",    "same-class", "Industrial sans-serif; Arimo is functional substitute"),
    "chicago":          ("Arimo",    "same-class", "Classic Mac system font; Arimo covers the role"),
    "ocean":            ("Arimo",    "same-class", "Sans-serif; Arimo is functional substitute"),
    "omega":            ("Arimo",    "same-class", "Sans-serif; Arimo is functional substitute"),
    "antiqueolive":     ("Arimo",    "same-class", "Sans-serif; Arimo is functional substitute"),
    "arialnarrow":      ("Arimo",    "same-class", "Condensed sans; no condensed variant on Google Fonts, Arimo is width-incompatible but functional"),
    "arialblack":       ("Arimo",    "same-class", "Heavy sans; Arimo 700 is lighter than Arial Black but same class"),

    # === Same-class: serif fonts → Tinos ===
    "times":            ("Tinos",    "same-class", "Times is the original; Times New Roman is the digitization; Tinos matches TNR"),
    "georgia":          ("Tinos",    "same-class", "Slab-influenced serif designed for screen; Tinos is closest serif. Georgia has wider glyphs"),
    "garamond":         ("Tinos",    "same-class", "Old-style serif; Tinos is closest available. Different proportions than Garamond"),
    "palatino":         ("Tinos",    "same-class", "Old-style serif; wider than Times. Tinos is narrower but same class"),
    "palatinolinotype": ("Tinos",    "same-class", "Palatino Linotype = digital Palatino; Tinos substitutes as closest serif"),
    "bookantiqua":      ("Tinos",    "same-class", "Palatino clone; Tinos is closest available serif"),
    "bookman":          ("Tinos",    "same-class", "ITC Bookman is a wide serif; Tinos is narrower but same class"),
    "bookmanoldstyle":  ("Tinos",    "same-class", "Bookman Old Style = ITC Bookman; Tinos substitutes"),
    "newcenturyschlbk": ("Tinos",    "same-class", "New Century Schoolbook is a legibility serif; Tinos is closest"),
    "centuryschoolbook":("Tinos",    "same-class", "Century Schoolbook variant; Tinos substitutes"),
    "century":          ("Tinos",    "same-class", "Century family serif; Tinos is closest available"),
    "lucidabright":     ("Tinos",    "same-class", "Serif designed for screen; Tinos is closest"),
    "baskervilleoldface":("Tinos",   "same-class", "Transitional serif; Tinos is closest"),
    "bell":             ("Tinos",    "same-class", "Transitional serif; Tinos is closest available"),
    "calisto":          ("Tinos",    "same-class", "Calisto MT is a calligraphic serif; Tinos is closest"),
    "newyork":          ("Tinos",    "same-class", "Classic Mac serif; Tinos is closest"),

    # === Same-class: monospace fonts → Cousine ===
    "courier":          ("Cousine",  "same-class", "Courier is the typewriter original; Cousine matches Courier New"),
    "monaco":           ("Cousine",  "same-class", "macOS monospace; Cousine is closest mono on Google Fonts"),
    "consolas":         ("Cousine",  "same-class", "Modern monospace; Cousine is closest. Consolas has different proportions"),
    "lucidaconsole":    ("Cousine",  "same-class", "Monospace; Cousine substitutes"),
    "lucidasanstypewriter": ("Cousine", "same-class", "Monospace; Cousine substitutes"),
    "lettergothic":     ("Cousine",  "same-class", "Monospace typewriter font; Cousine substitutes"),
    "menlo":            ("Cousine",  "same-class", "macOS monospace (DejaVu Sans Mono derivative); Cousine is closest"),
    "andalemono":       ("Cousine",  "same-class", "Monospace; Cousine substitutes"),

    # === LibreOffice intermediate fonts → their proper substitute ===
    "albanyamt":        ("Arimo",    "same-class", "LibreOffice's Arial substitute; we use Arimo instead"),
    "albany":           ("Arimo",    "same-class", "Alias for Albany AMT"),
    "thorndaleamt":     ("Tinos",    "same-class", "LibreOffice's Times New Roman substitute; we use Tinos instead"),
    "thorndale":        ("Tinos",    "same-class", "Alias for Thorndale AMT"),
    "cumberlandamt":    ("Cousine",  "same-class", "LibreOffice's Courier New substitute; we use Cousine instead"),
    "cumberland":       ("Cousine",  "same-class", "Alias for Cumberland AMT"),
    "liberationsans":   ("Arimo",    "metric-compatible", "Liberation Sans is also metric-compatible with Arial, like Arimo"),
    "liberationserif":  ("Tinos",    "metric-compatible", "Liberation Serif is also metric-compatible with Times New Roman, like Tinos"),
    "liberationmono":   ("Cousine",  "metric-compatible", "Liberation Mono is also metric-compatible with Courier New, like Cousine"),
    "liberationsansnarrow": ("Arimo","same-class", "Condensed sans; no condensed variant available, using Arimo"),
    "dejavusans":       ("Arimo",    "same-class", "DejaVu Sans is a wide sans; Arimo is narrower but same class"),
    "dejavuserif":      ("Tinos",    "same-class", "DejaVu Serif → Tinos"),
    "dejavusansmono":   ("Cousine",  "same-class", "DejaVu Sans Mono → Cousine"),
    "dejavusanscondensed": ("Arimo", "same-class", "Condensed sans → Arimo"),
    "bitstreamverasans":("Arimo",    "same-class", "Bitstream Vera Sans (DejaVu ancestor) → Arimo"),
    "bitstreamveraserif":("Tinos",   "same-class", "Bitstream Vera Serif → Tinos"),
    "bitstreamverasansmono": ("Cousine", "same-class", "Bitstream Vera Sans Mono → Cousine"),
    "bitstreamcharter": ("Tinos",    "same-class", "Charter is a transitional serif; Tinos substitutes"),
    "charter":          ("Tinos",    "same-class", "Charter serif → Tinos"),
    "nimbussansl":      ("Arimo",    "same-class", "Nimbus Sans L is Helvetica clone; Arimo substitutes"),
    "nimbusromanno9l":  ("Tinos",    "same-class", "Nimbus Roman is Times clone; Tinos substitutes"),
    "nimbusromannol":   ("Tinos",    "same-class", "Nimbus Roman variant; Tinos substitutes"),
    "nimbusmonol":      ("Cousine",  "same-class", "Nimbus Mono L is Courier clone; Cousine substitutes"),
    "freesans":         ("Arimo",    "same-class", "GNU FreeFont Sans → Arimo"),
    "freeserif":        ("Tinos",    "same-class", "GNU FreeFont Serif → Tinos"),
    "freemono":         ("Cousine",  "same-class", "GNU FreeFont Mono → Cousine"),
    "cmusansserif":     ("Arimo",    "same-class", "Computer Modern Sans → Arimo"),
    "cmuserif":         ("Tinos",    "same-class", "Computer Modern Serif → Tinos"),
    "cmutypewritertext":("Cousine",  "same-class", "Computer Modern Typewriter → Cousine"),

    # === Generic family names ===
    "sansserif":        ("Arimo",    "same-class", "Generic sans-serif family → Arimo"),
    "sans":             ("Arimo",    "same-class", "Generic sans → Arimo"),
    "serif":            ("Tinos",    "same-class", "Generic serif family → Tinos"),
    "monospace":        ("Cousine",  "same-class", "Generic monospace family → Cousine"),
    "monospaced":       ("Cousine",  "same-class", "Generic monospace → Cousine"),
    "mono":             ("Cousine",  "same-class", "Generic mono → Cousine"),
    "cursive":          ("Arimo",    "rough-fallback", "CSS cursive generic; no script font on Google Fonts matches well"),
    "fantasy":          ("Arimo",    "rough-fallback", "CSS fantasy generic; no decorative font matches well"),
}

# Fonts with no web substitute — documented for future reference
NO_SUBSTITUTE = {
    # Symbol/dingbat fonts — use special encoded characters, no text substitute
    "symbol":           "Symbol font uses private Unicode area; glyphs mapped to Unicode equivalents in code",
    "wingdings":        "Dingbat font; no web equivalent. We map common chars (bullets) in rendering code",
    "wingdings2":       "Dingbat font variant; no web equivalent",
    "wingdings3":       "Dingbat font variant; no web equivalent",
    "wingdings4":       "Dingbat font variant; no web equivalent",
    "webdings":         "Microsoft dingbat font; no web equivalent",
    "starsymbol":       "LibreOffice's symbol font; no web equivalent",
    "opensymbol":       "LibreOffice's symbol font; no web equivalent",
    "zapfdingbats":     "Dingbat font; no web equivalent",
    "marlett":          "Windows UI glyph font; no web equivalent",
    "dingbats":         "Generic dingbat; no web equivalent",

    # CJK fonts — future: will map to Noto Sans/Serif CJK variants
    "msgothic":         "Japanese Gothic (sans); future: Noto Sans CJK JP",
    "mspgothic":        "Japanese Gothic proportional; future: Noto Sans CJK JP",
    "msuigothic":       "Japanese Gothic UI; future: Noto Sans CJK JP",
    "msmincho":         "Japanese Mincho (serif); future: Noto Serif CJK JP",
    "mspmincho":        "Japanese Mincho proportional; future: Noto Serif CJK JP",
    "meiryo":           "Japanese UI font; future: Noto Sans CJK JP",
    "simsun":           "Chinese Song (serif); future: Noto Serif CJK SC",
    "nsimsun":          "Chinese Song variant; future: Noto Serif CJK SC",
    "simhei":           "Chinese Hei (sans); future: Noto Sans CJK SC",
    "simkai":           "Chinese Kai (brush script); future: Noto Sans CJK SC",
    "fangsong":         "Chinese FangSong (imitation Song); future: Noto Serif CJK SC",
    "gulim":            "Korean sans; future: Noto Sans CJK KR",
    "gulimche":         "Korean sans mono; future: Noto Sans CJK KR",
    "dotum":            "Korean sans; future: Noto Sans CJK KR",
    "dotumche":         "Korean sans mono; future: Noto Sans CJK KR",
    "batang":           "Korean serif; future: Noto Serif CJK KR",
    "batangche":        "Korean serif mono; future: Noto Serif CJK KR",
    "mingliu":          "Traditional Chinese serif; future: Noto Serif CJK TC",
    "pmingliu":         "Traditional Chinese serif proportional; future: Noto Serif CJK TC",

    # Thai fonts
    "angsananew":       "Thai serif font; future: Noto Serif Thai",
    "angsanaupc":       "Thai serif variant; future: Noto Serif Thai",
    "browallianew":     "Thai sans font; future: Noto Sans Thai",
    "browalliaupc":     "Thai sans variant; future: Noto Sans Thai",
    "cordianew":        "Thai font; future: Noto Sans Thai",
    "cordiaupc":        "Thai font; future: Noto Sans Thai",

    # Decorative/display — rare in OOXML, no good substitute
    "impact":           "Heavy display sans; no metric-compatible web font. Arimo 700 is lighter",
    "comicsans":        "Casual handwriting font; no web equivalent matches the style",
    "comicsansms":      "Comic Sans MS; no web equivalent matches the informal style",
    "broadway":         "Art Deco display font; no web equivalent",
    "algerian":         "Decorative shadowed font; no web equivalent",
    "castellar":        "All-caps engraved display; no web equivalent",
    "colonna":          "Decorative outline; no web equivalent",

    # Script/calligraphy — rare in OOXML
    "zapfchancery":     "Calligraphic script; no web equivalent on Google Fonts",
    "palacescript":     "Formal script; no web equivalent",
    "edwardianscript":  "Formal script; no web equivalent",
    "shelley":          "Formal script; no web equivalent",

    # Additional CJK fonts
    "fzsongti":         "Chinese Song style; future: Noto Serif CJK SC",
    "fzmingti":         "Chinese/Traditional Chinese Ming style; future: Noto Serif CJK TC",
    "fzhei":            "Chinese Hei (sans); future: Noto Sans CJK SC",
    "fzkai":            "Chinese Kai (brush); future: Noto Sans CJK SC",
    "cfangsong":        "Chinese FangSong variant; future: Noto Serif CJK SC",
    "song":             "Chinese Song style; future: Noto Serif CJK SC",
    "msong":            "Chinese Song variant; future: Noto Serif CJK SC",
    "shusong":          "Chinese Song variant; future: Noto Serif CJK SC",
    "simsong":          "Chinese Song variant; future: Noto Serif CJK SC",
    "hei":              "Chinese Hei (sans); future: Noto Sans CJK SC",
    "mhei":             "Chinese Hei variant; future: Noto Sans CJK SC",
    "mkai":             "Chinese Kai variant; future: Noto Sans CJK SC",
    "kai":              "Chinese Kai calligraphy style; future: Noto Sans CJK SC",
    "ming":             "Chinese Ming (serif); future: Noto Serif CJK TC",
    "mingli":           "Chinese Ming variant; future: Noto Serif CJK TC",
    "zycjkhei":         "Chinese Hei CJK variant; future: Noto Sans CJK SC",
    "zycjkkai":         "Chinese Kai CJK variant; future: Noto Sans CJK SC",
    "zycjksun":         "Chinese Song CJK variant; future: Noto Serif CJK SC",
    "msunglightsc":     "Chinese Song (simplified); future: Noto Serif CJK SC",
    "msunglighttc":     "Chinese Ming (traditional); future: Noto Serif CJK TC",
    "gothic":           "Japanese Gothic (sans); future: Noto Sans CJK JP",
    "gothicb":          "Japanese Gothic variant; future: Noto Sans CJK JP",
    "gothicl":          "Japanese Gothic variant; future: Noto Sans CJK JP",
    "pgothic":          "Japanese proportional Gothic; future: Noto Sans CJK JP",
    "mincho":           "Japanese Mincho (serif); future: Noto Serif CJK JP",
    "minchoj":          "Japanese Mincho variant; future: Noto Serif CJK JP",
    "minchol":          "Japanese Mincho variant; future: Noto Serif CJK JP",
    "minchou":          "Japanese Mincho variant; future: Noto Serif CJK JP",
    "pmincho":          "Japanese proportional Mincho; future: Noto Serif CJK JP",
    "heiseimin":        "Japanese Heisei Mincho; future: Noto Serif CJK JP",
    "hiraginokakugothicpronw3": "macOS Japanese Gothic; future: Noto Sans CJK JP",
    "hiraginokakugothicprow3":  "macOS Japanese Gothic; future: Noto Sans CJK JP",
    "hiraginomarugothicpronw4": "macOS Japanese rounded Gothic; future: Noto Sans CJK JP",
    "hiraginomarugothicprow4":  "macOS Japanese rounded Gothic; future: Noto Sans CJK JP",
    "hiraginominchopronw3":     "macOS Japanese Mincho; future: Noto Serif CJK JP",
    "hiraginominchoprow3":      "macOS Japanese Mincho; future: Noto Serif CJK JP",
    "gungsuh":          "Korean decorative serif; future: Noto Serif CJK KR",
    "gungsuhche":       "Korean decorative serif mono; future: Noto Serif CJK KR",
    "haansoftbatang":   "Korean serif (Hansoft); future: Noto Serif CJK KR",
    "haansoftdotum":    "Korean sans (Hansoft); future: Noto Sans CJK KR",
    "myeongjo":         "Korean Myeongjo (serif); future: Noto Serif CJK KR",

    # Thai fonts
    "garuda":           "Thai font (Garuda); future: Noto Sans Thai",
    "norasi":           "Thai font (Norasi); future: Noto Serif Thai",
    "nftnorasi":        "Thai font variant; future: Noto Serif Thai",
    "ooannop":          "Thai font; future: Noto Sans Thai",
    "oonaksit":         "Thai font; future: Noto Sans Thai",
    "ooratchada":       "Thai font; future: Noto Sans Thai",
    "oosawasdee":       "Thai font; future: Noto Sans Thai",
    "oothanakrit":      "Thai font; future: Noto Sans Thai",

    # Indic
    "gautami":          "Telugu font; future: Noto Sans Telugu",

    # Google Fonts ↔ MS bidirectional mappings (these are Google's own alternatives)
    "agdasima":         "Google Font; counterpart to Agency FB (display sans). No need to substitute — available directly on Google Fonts",
    "agencyfb":         "Microsoft display sans; Agdasima is Google's alternative but visually quite different",
    "bacasimeantique":  "Google Font; counterpart to Baskerville Old Face. Available directly on Google Fonts",
    "baskervilleoldface": "already classified elsewhere",
    "belanosima":       "Google Font; counterpart to Berlin Sans FB. Available directly on Google Fonts",
    "berlinsansfb":     "Microsoft display sans; Belanosima is Google's alternative",
    "caprasimo":        "Google Font; counterpart to Cooper Black. Available directly on Google Fonts",
    "cooperblack":      "Heavy decorative serif; Caprasimo is Google's alternative but visually different",

    # Lucida variants mapped to Google alternatives
    "lugrasimo":        "Google Font substitute for Lucida Calligraphy. Available on Google Fonts",
    "lunasima":         "Google Font substitute for Lucida Grande. Available on Google Fonts",
    "lumanosimo":       "Google Font substitute for Lucida Handwriting. Available on Google Fonts",

    # Condensed variants
    "helmetcondensed":  "Condensed sans; no condensed web font available",
    "fujiyama":         "Condensed sans; no condensed web font available",
    "sanscondensed":    "Generic condensed sans; no condensed web font available",
    "sanslcondensed":   "Generic condensed sans; no condensed web font available",
    "universcondensed": "Univers Condensed; no condensed web font available",
    "swissnarrow":      "Narrow serif; no narrow serif web font available",
}


def parse_vcl(content: str) -> list[tuple[str, str | None, str | None]]:
    """Parse VCL.xcu XML and return (name, SubstFonts, SubstFontsMS) tuples."""
    entries = []
    lines = content.split("\n")
    current_name = None
    current_subst = None
    current_ms = None
    in_font_subst = False

    for i, line in enumerate(lines):
        if "FontSubstitutions" in line:
            in_font_subst = True
        if not in_font_subst:
            continue

        m = re.search(r'<node oor:name="([^"]+)"', line)
        if m:
            if current_name and (current_subst or current_ms):
                entries.append((current_name, current_subst, current_ms))
            current_name = m.group(1)
            current_subst = None
            current_ms = None

        if current_name:
            if "SubstFontsMS" in line or (i > 0 and "SubstFontsMS" in lines[i - 1]):
                vm = re.search(r"<value>([^<]*)</value>", line)
                if vm:
                    current_ms = vm.group(1)
            elif "SubstFonts" in line and "SubstFontsMS" not in line:
                vm = re.search(r"<value>([^<]*)</value>", line)
                if vm and current_subst is None:
                    current_subst = vm.group(1)
            elif current_subst is None:
                vm = re.search(r"<value>([^<]*)</value>", line)
                if vm and i > 0 and "SubstFonts" in lines[i - 1] and "SubstFontsMS" not in lines[i - 1]:
                    current_subst = vm.group(1)

    if current_name and (current_subst or current_ms):
        entries.append((current_name, current_subst, current_ms))

    return entries


def normalize(name: str) -> str:
    return name.lower().replace(" ", "").replace("-", "").replace("_", "")


# Patterns for chain-based classification (when not in EXPLICIT_MAPPINGS)
CHAIN_PATTERNS = {
    # If the chain leads to these, classify accordingly
    "albanyamt": ("Arimo", "same-class", "VCL.xcu chain leads to sans-serif bucket (Albany AMT)"),
    "albany": ("Arimo", "same-class", "VCL.xcu chain leads to sans-serif bucket"),
    "arial": ("Arimo", "same-class", "VCL.xcu chain leads to Arial → Arimo"),
    "liberationsans": ("Arimo", "same-class", "VCL.xcu chain leads to Liberation Sans → Arimo"),

    "thorndaleamt": ("Tinos", "same-class", "VCL.xcu chain leads to serif bucket (Thorndale AMT)"),
    "thorndale": ("Tinos", "same-class", "VCL.xcu chain leads to serif bucket"),
    "timesnewroman": ("Tinos", "same-class", "VCL.xcu chain leads to Times New Roman → Tinos"),
    "liberationserif": ("Tinos", "same-class", "VCL.xcu chain leads to Liberation Serif → Tinos"),
    "times": ("Tinos", "same-class", "VCL.xcu chain leads to Times → Tinos"),

    "cumberlandamt": ("Cousine", "same-class", "VCL.xcu chain leads to mono bucket (Cumberland AMT)"),
    "cumberland": ("Cousine", "same-class", "VCL.xcu chain leads to mono bucket"),
    "liberationmono": ("Cousine", "same-class", "VCL.xcu chain leads to Liberation Mono → Cousine"),

    "carlito": ("Carlito", "metric-compatible", "VCL.xcu chain leads to Carlito"),
    "calibri": ("Carlito", "metric-compatible", "VCL.xcu chain leads to Calibri → Carlito"),
    "caladea": ("Caladea", "metric-compatible", "VCL.xcu chain leads to Caladea"),
    "cambria": ("Caladea", "metric-compatible", "VCL.xcu chain leads to Cambria → Caladea"),

    "starsymbol": None,   # Symbol — skip
    "opensymbol": None,
    "andymt": None,       # Handwriting — skip
    "zapfchancery": None, # Script — skip
    "palacescript": None,
    "imprintmtshadow": None,  # Decorative — skip
    "falstaff": None,
    "sheffield": None,
    "losangeles": None,
    "onyx": None,
}


def classify_by_chain(chain: list[str]) -> tuple[str, str, str] | None:
    """Follow VCL.xcu chain to classify a font."""
    for item in chain[:5]:
        key = normalize(item)
        if key in CHAIN_PATTERNS:
            return CHAIN_PATTERNS[key]
    return None


def main():
    if len(sys.argv) > 1:
        with open(sys.argv[1]) as f:
            content = f.read()
    else:
        print("Downloading VCL.xcu...", file=sys.stderr)
        content = urllib.request.urlopen(VCL_URL).read().decode("utf-8")

    entries = parse_vcl(content)
    print(f"Parsed {len(entries)} font entries from VCL.xcu", file=sys.stderr)

    mappings = {}
    no_substitute_entries = {}
    unclassified = []

    for name, subst, ms in entries:
        norm = normalize(name)

        # Check explicit mapping first
        if norm in EXPLICIT_MAPPINGS:
            sub, quality, reason = EXPLICIT_MAPPINGS[norm]
            mappings[name] = {
                "substitute": sub,
                "quality": quality,
                "reason": reason,
            }
            continue

        # Check no-substitute list
        if norm in NO_SUBSTITUTE:
            no_substitute_entries[name] = {
                "substitute": None,
                "quality": "none",
                "reason": NO_SUBSTITUTE[norm],
            }
            continue

        # Try chain-based classification
        if subst:
            chain = [c.strip() for c in subst.split(";")]
            result = classify_by_chain(chain)
            if result is not None:
                sub, quality, reason = result
                mappings[name] = {
                    "substitute": sub,
                    "quality": quality,
                    "reason": reason,
                }
                continue
            elif result is None and any(normalize(c) in CHAIN_PATTERNS and CHAIN_PATTERNS[normalize(c)] is None for c in chain[:5]):
                # Chain leads to a skipped category
                first_skip = next(c for c in chain[:5] if normalize(c) in CHAIN_PATTERNS and CHAIN_PATTERNS[normalize(c)] is None)
                no_substitute_entries[name] = {
                    "substitute": None,
                    "quality": "none",
                    "reason": f"VCL.xcu chain leads to {first_skip} (no web equivalent)",
                }
                continue

        # Unclassified
        chain_str = subst.split(";")[0].strip() if subst else "(no chain)"
        unclassified.append((name, chain_str))

    # Add explicitly known fonts not in VCL.xcu but common in OOXML docs
    extra_mappings = {
        "segoeui":          ("Arimo",  "same-class", "Segoe UI is Microsoft's UI font; Arimo is closest sans"),
        "microsoftsansserif":("Arimo", "same-class", "Microsoft Sans Serif → Arimo"),
        "trebuchetms":      ("Arimo",  "same-class", "Trebuchet MS is a geometric sans; Arimo is closest"),
        "franklingothicmedium":("Arimo","same-class","Franklin Gothic Medium is a grotesque sans; Arimo substitutes"),
        "aptos":            ("Carlito","same-class", "Aptos is Microsoft's new default (replaces Calibri in M365); Carlito is closest available"),
        "aptosnarrow":      ("Carlito","same-class", "Aptos Narrow → Carlito (width incompatible but functional)"),
        "candara":          ("Arimo",  "same-class", "Candara is a humanist sans from MS ClearType collection; Arimo substitutes"),
        "corbel":           ("Arimo",  "same-class", "Corbel is a humanist sans from MS ClearType collection; Arimo substitutes"),
        "constantia":       ("Tinos",  "same-class", "Constantia is a serif from MS ClearType collection; Tinos substitutes"),
    }
    for norm_name, (sub, quality, reason) in extra_mappings.items():
        if norm_name not in {normalize(k) for k in mappings}:
            display_name = norm_name  # These won't have a display name from VCL.xcu
            mappings[display_name] = {
                "substitute": sub,
                "quality": quality,
                "reason": reason,
            }

    if unclassified:
        print(f"\nUnclassified ({len(unclassified)}):", file=sys.stderr)
        for name, first in unclassified:
            print(f"  {name} → {first}", file=sys.stderr)

    output = {
        "_generated": "Auto-generated from LibreOffice VCL.xcu. Run: python3 scripts/generate-font-map.py",
        "_source": "https://github.com/LibreOffice/core/blob/master/officecfg/registry/data/org/openoffice/VCL.xcu",
        "substitutes": SUBSTITUTES,
        "mappings": {k: v for k, v in sorted(mappings.items())},
        "no_substitute": {k: v for k, v in sorted(no_substitute_entries.items())},
    }

    with open(OUTPUT, "w") as f:
        json.dump(output, f, indent=2, ensure_ascii=False)

    classified = len(mappings)
    skipped = len(no_substitute_entries)
    total = classified + skipped + len(unclassified)
    print(f"\nResults:", file=sys.stderr)
    print(f"  Mapped to Google Fonts: {classified}", file=sys.stderr)
    print(f"  No substitute (documented): {skipped}", file=sys.stderr)
    print(f"  Unclassified: {len(unclassified)}", file=sys.stderr)
    print(f"  Total: {total}", file=sys.stderr)
    print(f"\nSubstitute families used: {sorted(set(m['substitute'] for m in mappings.values()))}", file=sys.stderr)
    print(f"\nWrote to {OUTPUT}", file=sys.stderr)


if __name__ == "__main__":
    main()
