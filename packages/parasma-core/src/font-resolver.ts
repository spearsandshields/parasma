/**
 * Font resolution via VCL.xcu-based font-map.json classification + full TTF
 * files from googlefonts repos on jsDelivr CDN.
 *
 * Two layers:
 *   1. font-map.json classifies 315+ proprietary fonts -> substitute family
 *   2. Full TTF/OTF files are fetched for the 11 bundled families below — the
 *      five Word metric-clones (Arimo/Tinos/Cousine/Carlito/Caladea), Inter, the
 *      two libre faces we load for real (DejaVu Sans, Open Sans), the two
 *      on-demand script faces (Noto Sans Arabic, Noto Sans CJK) and XITS Math.
 *      Every URL is pinned to an exact upstream commit — see PINNED_COMMITS.
 */

import fontMap from "./font-map.json";

// ─── Types ─────────────────────────────────────────────────────────────────

/** The four cuts a bundled family resolves to. Families with no true italic
 *  (CJK, Arabic, the single-master math face) point italic at regular. */
export interface FontVariantUrls {
  regular: string;
  bold: string;
  italic: string;
  boldItalic: string;
}

export interface FontPlanEntry {
  family: string;
  /**
   * CDN fallback URLs. ABSENT on a synthesized LOCAL-ONLY entry (a declared
   * family with no substitute, e.g. a brand font): the exact-match ladder may
   * serve it locally when `useLocalFonts` is on; a total local miss fetches
   * nothing and the engine default renders.
   */
  urls?: FontVariantUrls;
  /**
   * Local-resolution candidates, in search order: the DECLARED family FIRST
   * (the exact-match ladder target), then the curated Word-substitute
   * (Verdana for DejaVu Sans, etc.) when one exists. Only consulted when
   * `useLocalFonts` is on AND a candidate is installed; otherwise inert —
   * `urls` stays the fallback. CJK-routed families carry NO candidates (a v1
   * exclusion, tracked as a follow-on).
   */
  localCandidates?: string[];
  /** The curated family Word itself substitutes for this doc font, if any. */
  wordTarget?: string;
}

// ─── Layer 1: Classification ───────────────────────────────────────────────

const mappings: Record<string, { substitute: string; quality: string; reason: string }> =
  (fontMap as any).mappings;

/**
 * CJK font NAMES → "Noto Sans CJK" (§17.3.2.26 eastAsia slot). font-map.json is
 * Latin-only, so a doc that explicitly NAMES a Chinese/Japanese/Korean font
 * (e.g. 宋体/SimSun) would otherwise resolve to null and render the CJK text in
 * a Latin fallback (badly overlapping). Real Office docs name these constantly;
 * an unset eastAsia slot already falls to the Noto-CJK default in the engine, so
 * this catches the EXPLICITLY-named case. Normalised: lowercased, spaces/hyphens/
 * underscores stripped — so "楷体_GB2312" and "Microsoft YaHei" both match.
 */
const CJK_FONT_NAMES = new Set<string>([
  // Simplified Chinese (native + romanised)
  "宋体", "新宋体", "黑体", "楷体", "楷体gb2312", "仿宋", "仿宋gb2312",
  "微软雅黑", "等线", "华文宋体", "华文楷体", "华文细黑", "华文黑体", "隶书", "幼圆",
  "simsun", "nsimsun", "simhei", "kaiti", "kaitigb2312", "fangsong", "fangsonggb2312",
  "microsoftyahei", "microsoftyaheiui", "msyh", "msyhui", "dengxian", "stsong", "stkaiti",
  "sthei", "lisu", "youyuan",
  // Traditional Chinese
  "新細明體", "細明體", "標楷體", "微軟正黑體",
  "pmingliu", "mingliu", "dfkaisb", "microsoftjhenghei", "microsoftjhengheiui", "msjh",
  "msjhui", "kaiu",
  // Japanese
  "ｍｓ明朝", "ｍｓゴシック", "メイリオ", "游ゴシック", "游明朝",
  "msmincho", "mspmincho", "msgothic", "mspgothic", "meiryo", "yugothic", "yumincho",
  // Korean
  "맑은고딕", "바탕", "돋움", "굴림", "궁서",
  "malgungothic", "batang", "dotum", "gulim", "gungsuh", "nanumgothic", "nanummyeongjo",
]);

function normalizeFontKey(name: string): string {
  return name.toLowerCase().replace(/[ \-_]/g, "");
}

/**
 * OMML math font NAMES → "XITS Math" (a STIX fork carrying a real OpenType MATH
 * table). font-map.json is Latin-body-only, so a document naming "Cambria Math"
 * (or the engine-seeded "XITS Math") must short-circuit BEFORE the mappings
 * lookup — exactly like the CJK case — so equations fetch the math face with its
 * constants / size-variants / math-alphabet coverage. A doc that names NO math
 * font still gets it because the engine seeds "XITS Math" whenever the WML
 * contains an oMath/oMathPara. Normalised: lowercased, spaces/hyphens/
 * underscores stripped.
 */
const MATH_FONT_NAMES = new Set<string>([
  "cambriamath",
  "xitsmath",
  "stixtwomath",
  "stixmath",
  "latinmodernmath",
  "asanamath",
]);

/**
 * Weight/style tokens a CJK family carries as a suffix on the named face: a
 * theme's MAJOR font is routinely the Light cut ("等线 Light", "游ゴシック Light",
 * "DengXian Light", "Yu Gothic Light"). These normalise to "dengxianlight" etc.
 * and miss the base-name set, so the heading text falls to a Latin fallback. We
 * strip ONE trailing weight token and re-test against the CJK set — never a bare
 * weight word (so "Light"/"Bold" alone stay unmapped), and only when the
 * remaining base is itself a known CJK family (no Latin false positives:
 * "Arial Bold" → base "arial" ∉ CJK set → still null).
 */
const CJK_WEIGHT_SUFFIXES = [
  "light", "semilight", "extralight", "ultralight", "demilight",
  "bold", "semibold", "demibold", "extrabold", "ultrabold",
  "medium", "regular", "black", "heavy", "thin",
];

function isCjkFontName(fontName: string): boolean {
  const key = normalizeFontKey(fontName);
  if (CJK_FONT_NAMES.has(key)) return true;
  for (const w of CJK_WEIGHT_SUFFIXES) {
    if (key.length > w.length && key.endsWith(w)) {
      const base = key.slice(0, key.length - w.length);
      if (CJK_FONT_NAMES.has(base)) return true;
    }
  }
  return false;
}

export function resolveSubstitute(fontName: string): string | null {
  const lower = fontName.toLowerCase().replace(/ /g, "").replace(/-/g, "");

  // CJK names first — they're never in the Latin font-map (§17.3.2.26).
  if (isCjkFontName(fontName)) return "Noto Sans CJK";

  // OMML math faces next — never in the Latin font-map.
  if (MATH_FONT_NAMES.has(normalizeFontKey(fontName))) return "XITS Math";

  const entry = mappings[lower];
  if (entry) return entry.substitute;

  for (const [key, value] of Object.entries(mappings)) {
    if (key.toLowerCase().replace(/ /g, "").replace(/-/g, "") === lower) {
      return value.substitute;
    }
  }
  return null;
}

// ─── Layer 2: Full TTF/OTF URLs, all commit-pinned ─────────────────────────

/**
 * The exact upstream commit every bundled font is fetched from.
 *
 * A BRANCH ref (`@main`, `@master`) is a moving target: jsDelivr serves whatever
 * that branch points at today, so an upstream rebuild changes the metrics this
 * engine measures with no commit of ours and nothing going red. That is not
 * hypothetical — while Cousine sat on `@main`, its 2026-06-09 rebuild left
 * `glyf` and `hmtx` byte-identical (advances held) but moved OS/2
 * sTypoAscender 1255→1705 and sTypoDescender −386→−615. Cousine sets
 * USE_TYPO_METRICS, so `ttf_parser::Face::ascender()` follows the typographic
 * pair and the natural line height of every monospace line went
 * 0.8013em → 1.1328em, +41%, silently.
 *
 * Each sha below is what the branch/tag resolved to when the pin was taken, and
 * all 26 affected artefacts were sha256-compared between the old ref and the
 * commit ref first — adopting them moved no bytes. Bumping one is therefore
 * never routine maintenance: it is a deliberate change of rendering, and the
 * native baselines must be re-run behind it.
 *
 * Arimo, Carlito, Tinos, Open Sans, XITS Math and DejaVu Sans are ALSO fetched
 * by `scripts/fetch-test-fonts.sh` for the native layout harness. Those two
 * files must name the same version, or the browser renders against different
 * bytes than every pinned baseline was calibrated on. `font-pinning.test.ts`
 * enforces both halves — immutability here, and agreement with that script.
 */
const PINNED_COMMITS = {
  "google/fonts": "7ff85c87f93ea6cca5f41c69f2e4edcb90240f26",
  "googlefonts/arimo": "4a6255f269916ae7ad3fc2706b0935e7621396b8",
  "googlefonts/tinos": "3b4482a99b80ea5fc75f187b1be3120a3f5905b3",
  "googlefonts/cousine": "c0fbdb438443968c884a5c13f5f9bee916a7f89b",
  "googlefonts/carlito": "3a810cab78ebd6e2e4eed42af9e8453c4f9b850a",
  "googlefonts/caladea": "336a529cfad3d103d6527752686f8331d13e820a",
  "googlefonts/opensans": "bd7e37632246368c60fdcbd374dbf9bad11969b6",
  "notofonts/notofonts.github.io": "eaa1a5cf8cb83ea73941197e492d659e51bb11dd",
  "notofonts/noto-cjk": "f8d157532fbfaeda587e826d4cd5b21a49186f7c",
  "aliftype/xits": "81258e3318693741a50bd921d87c73d497103d37", // = tag v1.302
} as const;

/** jsDelivr GitHub URL for `path` in a pinned repo. */
const gh = (repo: keyof typeof PINNED_COMMITS, path: string) =>
  `https://cdn.jsdelivr.net/gh/${repo}@${PINNED_COMMITS[repo]}/${path}`;

/**
 * npm packages carry an immutable EXACT version rather than a commit — a
 * published version can never be re-pointed, so `@2.37.3` is already a pin (a
 * range like `^2.37.3` would NOT be). Same version the native harness fetches.
 */
const DEJAVU_VERSION = "2.37.3";
const npmDejaVu = (file: string) =>
  `https://cdn.jsdelivr.net/npm/dejavu-fonts-ttf@${DEJAVU_VERSION}/ttf/${file}`;

const INTER_VF = gh("google/fonts", "ofl/inter/Inter%5Bopsz%2Cwght%5D.ttf");

/** One upright master serves all four XITS Math variants — see the map entry. */
const XITS_MATH = gh("aliftype/xits", "XITSMath-Regular.otf");

const FONT_URLS: Record<string, FontVariantUrls> = {
  Inter: {
    regular: INTER_VF,
    bold: INTER_VF,
    italic: INTER_VF,
    boldItalic: INTER_VF,
  },
  Arimo: {
    regular: gh("googlefonts/arimo", "fonts/ttf/Arimo-Regular.ttf"),
    bold: gh("googlefonts/arimo", "fonts/ttf/Arimo-Bold.ttf"),
    italic: gh("googlefonts/arimo", "fonts/ttf/Arimo-Italic.ttf"),
    boldItalic: gh("googlefonts/arimo", "fonts/ttf/Arimo-BoldItalic.ttf"),
  },
  Tinos: {
    regular: gh("googlefonts/tinos", "fonts/ttf/Tinos-Regular.ttf"),
    bold: gh("googlefonts/tinos", "fonts/ttf/Tinos-Bold.ttf"),
    italic: gh("googlefonts/tinos", "fonts/ttf/Tinos-Italic.ttf"),
    boldItalic: gh("googlefonts/tinos", "fonts/ttf/Tinos-BoldItalic.ttf"),
  },
  Cousine: {
    regular: gh("googlefonts/cousine", "fonts/ttf/Cousine-Regular.ttf"),
    bold: gh("googlefonts/cousine", "fonts/ttf/Cousine-Bold.ttf"),
    italic: gh("googlefonts/cousine", "fonts/ttf/Cousine-Italic.ttf"),
    boldItalic: gh("googlefonts/cousine", "fonts/ttf/Cousine-BoldItalic.ttf"),
  },
  Carlito: {
    regular: gh("googlefonts/carlito", "fonts/ttf/Carlito-Regular.ttf"),
    bold: gh("googlefonts/carlito", "fonts/ttf/Carlito-Bold.ttf"),
    italic: gh("googlefonts/carlito", "fonts/ttf/Carlito-Italic.ttf"),
    boldItalic: gh("googlefonts/carlito", "fonts/ttf/Carlito-BoldItalic.ttf"),
  },
  Caladea: {
    regular: gh("googlefonts/caladea", "fonts/ttf/Caladea-Regular.ttf"),
    bold: gh("googlefonts/caladea", "fonts/ttf/Caladea-Bold.ttf"),
    italic: gh("googlefonts/caladea", "fonts/ttf/Caladea-Italic.ttf"),
    boldItalic: gh("googlefonts/caladea", "fonts/ttf/Caladea-BoldItalic.ttf"),
  },
  // DejaVu Sans is itself a libre font — load the REAL face rather than
  // substituting it with the (narrower) Arial-metric Arimo. DejaVu is notably
  // wide, so the wrong substitute shifts every line break and page-fill. Common
  // in documents from some non-Microsoft generators.
  DejaVuSans: {
    regular: npmDejaVu("DejaVuSans.ttf"),
    bold: npmDejaVu("DejaVuSans-Bold.ttf"),
    italic: npmDejaVu("DejaVuSans-Oblique.ttf"),
    boldItalic: npmDejaVu("DejaVuSans-BoldOblique.ttf"),
  },
  // Open Sans is libre — load the REAL face (same rationale as DejaVu Sans above).
  // It has a TALL line box (~1.36em); substituting the shorter Arial-metric Arimo
  // compresses every line (~2.7pt/line at 10.5pt) and pulls the whole page-fill
  // in, ending the document a page early vs Word. Common in documents from some
  // generators, named "Open Sans;Arial" (the `;`-fallback is split upstream).
  "Open Sans": {
    regular: gh("googlefonts/opensans", "fonts/ttf/OpenSans-Regular.ttf"),
    bold: gh("googlefonts/opensans", "fonts/ttf/OpenSans-Bold.ttf"),
    italic: gh("googlefonts/opensans", "fonts/ttf/OpenSans-Italic.ttf"),
    boldItalic: gh("googlefonts/opensans", "fonts/ttf/OpenSans-BoldItalic.ttf"),
  },
  // East Asian (CJK) and Complex Script (Arabic) font slots — §17.3.2.26.
  // Loaded on-demand only when the doc contains those scripts (the WASM reports
  // the family via fontFamiliesUsed only then). TTF/OTF (not woff2) so the Rust
  // ttf-parser can measure them. CJK/Arabic have no italic; italic→regular.
  "Noto Sans Arabic": {
    regular: gh("notofonts/notofonts.github.io", "fonts/NotoSansArabic/hinted/ttf/NotoSansArabic-Regular.ttf"),
    bold: gh("notofonts/notofonts.github.io", "fonts/NotoSansArabic/hinted/ttf/NotoSansArabic-Bold.ttf"),
    italic: gh("notofonts/notofonts.github.io", "fonts/NotoSansArabic/hinted/ttf/NotoSansArabic-Regular.ttf"),
    boldItalic: gh("notofonts/notofonts.github.io", "fonts/NotoSansArabic/hinted/ttf/NotoSansArabic-Bold.ttf"),
  },
  // Full pan-CJK (Chinese + Japanese + Korean) in one face — ~16 MB, on-demand.
  "Noto Sans CJK": {
    regular: gh("notofonts/noto-cjk", "Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Regular.otf"),
    bold: gh("notofonts/noto-cjk", "Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Bold.otf"),
    italic: gh("notofonts/noto-cjk", "Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Regular.otf"),
    boldItalic: gh("notofonts/noto-cjk", "Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Bold.otf"),
  },
  // OMML math face — XITS Math (a STIX fork, SIL OFL). jsDelivr's
  // GitHub mirror serves the FULL, non-subset OTF (so the MATH table survives)
  // with CORS enabled. A single upright Regular master carries the whole math
  // alphabet + size variants; there is no separate bold/italic cut, so all four
  // variants map to Regular (slant/weight come from the Mathematical
  // Alphanumeric code points the engine emits). On-demand: only fetched when the
  // doc contains an oMath (the engine seeds "XITS Math" into fontFamiliesUsed).
  // NOTE: do NOT swap in a subset woff2 math webfont — those strip
  // the MATH table and the engine's None-guard would silently re-arm proxies.
  "XITS Math": {
    regular: XITS_MATH,
    bold: XITS_MATH,
    italic: XITS_MATH,
    boldItalic: XITS_MATH,
  },
};

/**
 * The bundled families and the four variant URLs each resolves to — read-only
 * view of {@link FONT_URLS}.
 *
 * Exported so tooling can ASK the module instead of scraping it:
 * `scripts/debug-fonts.py` used to regex out `regular: "https://…"` string
 * literals, which stopped existing the moment the URLs were built from
 * PINNED_COMMITS. Evaluating the real thing cannot drift.
 */
export const BUNDLED_FONT_FAMILIES: Readonly<Record<string, Readonly<FontVariantUrls>>> =
  FONT_URLS;

/**
 * Every distinct URL the bundled families can fetch. Exported so the pinning
 * invariant is asserted over the SHIPPING values rather than over a restatement
 * of them (`font-pinning.test.ts`).
 */
export const BUNDLED_FONT_URLS: readonly string[] = [
  ...new Set(Object.values(FONT_URLS).flatMap((v) => Object.values(v))),
];

/**
 * The distinct origins the bundled fonts fetch from — one today (jsDelivr), but
 * DERIVED from the URLs rather than restated, so adding a family hosted
 * elsewhere can never leave the `preconnect` hint pointing at the wrong host.
 * Consumed by the viewer's font-origin warm-up; a consumer who overrides
 * the origin (`fontBaseUrl` / `assetBaseUrl`) supplies their own.
 */
export const BUNDLED_FONT_ORIGINS: readonly string[] = [
  ...new Set(BUNDLED_FONT_URLS.map((u) => new URL(u).origin)),
];

// ─── High-level API ─────────────────────────────────────────────────────────

/**
 * Hand-curated "what Word substitutes" map, keyed by normalizeFontKey(name).
 * Grown one entry at a time, each from a measured case. NOT auto-PANOSE; NOT the generated
 * font-map.json (a regen there would clobber this). Only actionable when
 * useLocalFonts is on AND the target is installed — otherwise inert (the plan's
 * own `urls` remain the fallback).
 */
const WORD_SUBSTITUTIONS: Record<string, string> = {
  dejavusans: "Verdana", // DejaVu forks Bitstream Vera ≈ Verdana (PANOSE 2-11-6-3-3 vs 2-11-6-4-3)
  // Open Sans isn't OS-installed, so Word substitutes it → Segoe UI (confirmed
  // via `pdffonts` on Word's PDF of file-sample_500kB: no Open Sans embedded, the
  // body text is SegoeUI). Segoe UI's hhea (1.3301em) is ~2.4% tighter than real
  // Open Sans (1.3618em), which is the whole p3 line-height residual — NOT a
  // line-height-model bug. Segoe UI ships inside Office (FontCache/CloudFonts),
  // which the dev /__local-fonts route now finds via name-table matching.
  opensans: "Segoe UI",
};

/**
 * Build a font plan from the list of font families used in the document.
 * Returns a Map from document font name to { family, urls } for each
 * substitute font that needs to be fetched.
 */
export function buildFontPlan(
  fontFamiliesUsed: string[],
): Map<string, FontPlanEntry> {
  const plans = new Map<string, FontPlanEntry>();

  for (const docFamily of fontFamiliesUsed) {
    const directUrls = FONT_URLS[docFamily];
    if (directUrls) {
      plans.set(docFamily, { family: docFamily, urls: directUrls });
      continue;
    }

    const substitute = resolveSubstitute(docFamily);
    const urls = substitute ? FONT_URLS[substitute] : undefined;
    if (!substitute || !urls) {
      // No substitute → a synthesized LOCAL-ONLY entry
      // (family = declared, NO urls). The ladder can serve it when local fonts
      // are on; a miss costs nothing (no fetch — engine default, as before).
      plans.set(docFamily, { family: docFamily });
      continue;
    }

    plans.set(docFamily, { family: substitute, urls });
  }

  // Local-candidate tagging: every planned family leads with its
  // DECLARED name (the exact-match ladder target), then the curated Word
  // substitute when one exists. Post-pass (not inline at each set) so the
  // direct, substitute, and synthesized paths are covered in one DRY place.
  // CJK-routed families are EXCLUDED v1 (label-only in coverage; a follow-on).
  for (const [docFamily, entry] of plans) {
    if (entry.family === "Noto Sans CJK" || entry.family === "XITS Math") continue;
    const wordTarget = WORD_SUBSTITUTIONS[normalizeFontKey(docFamily)];
    entry.localCandidates = wordTarget ? [docFamily, wordTarget] : [docFamily];
    if (wordTarget) entry.wordTarget = wordTarget;
  }

  return plans;
}
