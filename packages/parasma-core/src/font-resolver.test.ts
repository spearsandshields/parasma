import fontMap from "./font-map.json";
import { describe, it, expect } from "vitest";
import { resolveSubstitute, buildFontPlan, BUNDLED_FONT_FAMILIES } from "./font-resolver";

describe("resolveSubstitute", () => {
  it("maps calibri to Carlito (metric-compatible)", () => {
    expect(resolveSubstitute("calibri")).toBe("Carlito");
  });

  it("maps arial to Arimo (metric-compatible)", () => {
    expect(resolveSubstitute("arial")).toBe("Arimo");
  });

  it("maps timesnewroman to Tinos", () => {
    expect(resolveSubstitute("timesnewroman")).toBe("Tinos");
  });

  it("maps couriernew to Cousine", () => {
    expect(resolveSubstitute("couriernew")).toBe("Cousine");
  });

  // CJK font NAMES → Noto Sans CJK (§17.3.2.26). Docs that explicitly name a
  // Chinese/Japanese/Korean font (the norm) must trigger the CJK face, else the
  // text renders in a Latin fallback (overlapping) — regression: a document
  // naming 宋体/黑体/楷体.
  it("maps Chinese font names (宋体/黑体/楷体) to a Noto CJK face", () => {
    expect(resolveSubstitute("宋体")).toBe("Noto Serif CJK SC");
    expect(resolveSubstitute("黑体")).toBe("Noto Sans CJK");
    expect(resolveSubstitute("楷体_GB2312")).toBe("Noto Serif CJK SC");
    expect(resolveSubstitute("SimSun")).toBe("Noto Serif CJK SC");
    expect(resolveSubstitute("Microsoft YaHei")).toBe("Noto Sans CJK");
  });

  // Serif CJK families (Song/Ming/Mincho/Myeongjo) keep their class and take
  // their own region's face; sans names stay on the pan-CJK sans face.
  it("maps serif CJK names to the regional Noto Serif face", () => {
    expect(resolveSubstitute("MS Mincho")).toBe("Noto Serif CJK JP");
    expect(resolveSubstitute("游明朝")).toBe("Noto Serif CJK JP");
    expect(resolveSubstitute("PMingLiU")).toBe("Noto Serif CJK TC");
    expect(resolveSubstitute("新細明體")).toBe("Noto Serif CJK TC");
    expect(resolveSubstitute("Batang")).toBe("Noto Serif CJK KR");
    expect(resolveSubstitute("바탕")).toBe("Noto Serif CJK KR");
    expect(resolveSubstitute("MS Gothic")).toBe("Noto Sans CJK");
  });

  // Windows "* UI" variants (real-world xlsx files name "Microsoft YaHei UI" in a
  // <font> and several dxf parts; the normal font 等线/DengXian already maps).
  it("maps Windows '* UI' CJK variants to Noto Sans CJK", () => {
    expect(resolveSubstitute("Microsoft YaHei UI")).toBe("Noto Sans CJK");
    expect(resolveSubstitute("Microsoft JhengHei UI")).toBe("Noto Sans CJK");
    expect(resolveSubstitute("等线")).toBe("Noto Sans CJK");
  });

  // Weight-suffixed CJK cuts — a theme's MAJOR (heading) font is routinely the
  // Light cut ("等线 Light"/"游ゴシック Light"), the literal names seen in
  // real-world themes. Without suffix-stripping these miss the base-name set and
  // headings fall to a Latin fallback (overlapping CJK).
  it("maps weight-suffixed CJK cuts (等线 Light / Yu Gothic Light) to Noto Sans CJK", () => {
    expect(resolveSubstitute("等线 Light")).toBe("Noto Sans CJK");
    expect(resolveSubstitute("游ゴシック Light")).toBe("Noto Sans CJK");
    expect(resolveSubstitute("DengXian Light")).toBe("Noto Sans CJK");
    expect(resolveSubstitute("Yu Gothic Light")).toBe("Noto Sans CJK");
    expect(resolveSubstitute("SimSun Bold")).toBe("Noto Serif CJK SC");
  });

  // The weight-strip must NOT misfire on Latin families that end in a weight
  // word (no false Noto-CJK substitution).
  it("does NOT map weight-suffixed Latin families to Noto CJK", () => {
    expect(resolveSubstitute("Arial Black")).not.toBe("Noto Sans CJK");
    expect(resolveSubstitute("Helvetica Bold")).not.toBe("Noto Sans CJK");
    expect(resolveSubstitute("Light")).toBeNull();
    expect(resolveSubstitute("Bold")).toBeNull();
  });

  it("maps Japanese + Korean sans font names to Noto Sans CJK", () => {
    expect(resolveSubstitute("Meiryo")).toBe("Noto Sans CJK");
    expect(resolveSubstitute("Malgun Gothic")).toBe("Noto Sans CJK");
  });

  it("does NOT map Latin fonts to Noto CJK (no false positives)", () => {
    expect(resolveSubstitute("calibri")).not.toBe("Noto Sans CJK");
    expect(resolveSubstitute("arial")).not.toBe("Noto Sans CJK");
  });

  it("maps cambria to Caladea", () => {
    expect(resolveSubstitute("cambria")).toBe("Caladea");
  });

  it("maps helvetica to Arimo (same-class)", () => {
    expect(resolveSubstitute("helvetica")).toBe("Arimo");
  });

  it("maps georgia to Gelasio (metric-compatible)", () => {
    expect(resolveSubstitute("georgia")).toBe("Gelasio");
  });

  // Hand-written mappings for names LibreOffice's table does not list used to
  // be dropped by the generator, leaving them on the engine's default face.
  it("maps names absent from VCL.xcu (Consolas, Segoe UI, Aptos)", () => {
    expect(resolveSubstitute("Consolas")).toBe("Cousine");
    expect(resolveSubstitute("Segoe UI")).toBe("Arimo");
    expect(resolveSubstitute("Aptos")).toBe("Carlito");
    expect(resolveSubstitute("Palatino Linotype")).toBe("Tinos");
  });

  it("loads Google Fonts families as themselves, and Comic Sans as Comic Neue", () => {
    expect(resolveSubstitute("Lato")).toBe("Lato");
    expect(resolveSubstitute("Poppins")).toBe("Poppins");
    expect(resolveSubstitute("Comic Sans MS")).toBe("Comic Neue");
  });

  it("maps variable-only Google Fonts families to their class", () => {
    expect(resolveSubstitute("Roboto")).toBe("Arimo");
    expect(resolveSubstitute("Merriweather")).toBe("Tinos");
    expect(resolveSubstitute("Source Code Pro")).toBe("Cousine");
  });

  it("handles VCL.xcu chain entries like garamond", () => {
    expect(resolveSubstitute("garamond")).toBe("Tinos");
  });

  it("returns null for symbol fonts", () => {
    expect(resolveSubstitute("symbol")).toBeNull();
    expect(resolveSubstitute("wingdings")).toBeNull();
  });

  it("returns null for unknown fonts not in mapping", () => {
    expect(resolveSubstitute("NonExistentFont12345")).toBeNull();
  });

  it("is case-insensitive and ignores spaces/hyphens", () => {
    expect(resolveSubstitute("Calibri")).toBe("Carlito");
    expect(resolveSubstitute("ARIAL")).toBe("Arimo");
    expect(resolveSubstitute("Times New Roman")).toBe("Tinos");
  });
});

describe("buildFontPlan", () => {
  // The Carlito commit is spelled out here as well as in font-resolver.ts on
  // purpose: a pin bump is a deliberate change of rendering, not
  // maintenance, so it should cost a red test and a second look. Whether the
  // pin is IMMUTABLE at all, and whether it agrees with the native harness, is
  // font-pinning.test.ts's job — not this test's.
  const CARLITO = "https://cdn.jsdelivr.net/gh/googlefonts/carlito@3a810cab78ebd6e2e4eed42af9e8453c4f9b850a/fonts/ttf";

  it("returns correct TTF URLs for known fonts", () => {
    const plans = buildFontPlan(["Calibri", "Arial", "Courier New"]);

    expect(plans.size).toBe(3);

    const calibri = plans.get("Calibri")!;
    expect(calibri.family).toBe("Carlito");
    expect(calibri.urls!.regular).toBe(`${CARLITO}/Carlito-Regular.ttf`);
    expect(calibri.urls!.bold).toBe(`${CARLITO}/Carlito-Bold.ttf`);
    expect(calibri.urls!.italic).toBe(`${CARLITO}/Carlito-Italic.ttf`);
    expect(calibri.urls!.boldItalic).toBe(`${CARLITO}/Carlito-BoldItalic.ttf`);

    const arial = plans.get("Arial")!;
    expect(arial.family).toBe("Arimo");
    expect(arial.urls!.regular).toContain("Arimo-Regular.ttf");

    const courier = plans.get("Courier New")!;
    expect(courier.family).toBe("Cousine");
    expect(courier.urls!.regular).toContain("Cousine-Regular.ttf");
  });

  // Caladea lives in its own upstream repo with its own pin — it used to be the
  // one family reached via `@master` rather than `@main`, and that difference is
  // what this test guarded. Both are gone (every family is now pinned to a
  // commit), so what is left worth pinning is the substitution itself.
  it("maps Cambria to the Caladea face", () => {
    const plans = buildFontPlan(["Cambria"]);
    const cambria = plans.get("Cambria")!;
    expect(cambria.family).toBe("Caladea");
    expect(cambria.urls!.regular).toContain("/caladea@");
    expect(cambria.urls!.regular).toContain("Caladea-Regular.ttf");
  });

  // A declared family with NO substitute is not dropped — it gets a
  // synthesized LOCAL-ONLY plan entry (family = declared, no urls) so the exact-match ladder can serve it when useLocalFonts is on.
  // A total local miss costs nothing (no urls → no fetch → engine default,
  // the same as having no plan). An earlier version `continue`-dropped these,
  // so plans.size was 0.
  it("synthesizes a local-only entry for unknown (brand) fonts", () => {
    const plans = buildFontPlan(["NonExistentFont12345"]);
    expect(plans.size).toBe(1);
    const p = plans.get("NonExistentFont12345")!;
    expect(p.family).toBe("NonExistentFont12345");
    expect(p.urls).toBeUndefined(); // no CDN fallback — engine default on miss
    expect(p.localCandidates).toEqual(["NonExistentFont12345"]);
  });

  it("synthesizes local-only entries for symbol fonts too (miss = engine default)", () => {
    const plans = buildFontPlan(["Symbol", "Wingdings"]);
    expect(plans.size).toBe(2);
    expect(plans.get("Symbol")!.urls).toBeUndefined();
  });
});

// Libre fonts (DejaVu Sans, Open Sans) must load their REAL wide face, NOT the
// narrower Arial-metric Arimo — substituting Arimo shifts every line break and
// page-fill on documents from some generators (e.g. file-sample_500kB). DejaVu Sans
// reaches its real face via a libre SELF-substitute in font-map.json
// ("dejavusans" → "DejaVuSans" → FONT_URLS.DejaVuSans); Open Sans via a direct
// quoted FONT_URLS key. These guard either path against an accidental
// Arimo fallback (a removed font-map entry / FONT_URLS key would regress to it).
describe("buildFontPlan — libre fonts load their real face (not Arimo)", () => {
  it("loads the real DejaVu Sans face for 'DejaVu Sans'", () => {
    const plans = buildFontPlan(["DejaVu Sans"]);
    expect(plans.size).toBe(1);
    const dejavu = plans.get("DejaVu Sans");
    expect(dejavu).toBeDefined();
    expect(dejavu!.urls!.regular).toContain("dejavu-fonts-ttf"); // the REAL face…
    expect(dejavu!.urls!.regular).toContain("DejaVuSans.ttf");
    expect(dejavu!.urls!.regular).not.toContain("Arimo"); // …never the narrow clone
  });

  it("loads the real Open Sans face for 'Open Sans'", () => {
    const openSans = buildFontPlan(["Open Sans"]).get("Open Sans");
    expect(openSans).toBeDefined();
    expect(openSans!.family).toBe("Open Sans");
    expect(openSans!.urls!.regular).toContain("OpenSans-Regular.ttf");
    expect(openSans!.urls!.regular).not.toContain("Arimo");
  });
});

// The localCandidates contract: EVERY planned family leads with its
// DECLARED name (the exact-match ladder target), followed by the curated
// WORD_SUBSTITUTIONS candidate when one exists — without disturbing the
// real-face CDN fallback. CJK-routed families are EXCLUDED (a follow-on).
// An earlier post-pass tagged ONLY curated families, with just [curated].
describe("buildFontPlan — localCandidates = [declared, ...curated]", () => {
  it("tags DejaVu Sans with [declared, Verdana], keeping the real-DejaVu URL fallback", () => {
    const plan = buildFontPlan(["DejaVu Sans"]).get("DejaVu Sans");
    expect(plan).toBeDefined();
    expect(plan!.urls!.regular).toContain("dejavu-fonts-ttf"); // fallback unchanged
    expect(plan!.localCandidates).toEqual(["DejaVu Sans", "Verdana"]);
    expect(plan!.wordTarget).toBe("Verdana");
  });

  it("tags Open Sans with [declared, Segoe UI], keeping the real-Open-Sans URL fallback", () => {
    const plan = buildFontPlan(["Open Sans"]).get("Open Sans");
    expect(plan).toBeDefined();
    expect(plan!.urls!.regular).toContain("OpenSans-Regular.ttf"); // fallback unchanged
    expect(plan!.localCandidates).toEqual(["Open Sans", "Segoe UI"]);
    expect(plan!.wordTarget).toBe("Segoe UI");
  });

  it("fonts with no Word-sub still get the declared-name ladder candidate", () => {
    const plan = buildFontPlan(["Calibri"]).get("Calibri");
    expect(plan).toBeDefined();
    expect(plan!.localCandidates).toEqual(["Calibri"]);
    expect(plan!.wordTarget).toBeUndefined();
  });

  it("CJK-routed families get NO localCandidates (excluded from the ladder v1)", () => {
    for (const [name, family] of [["宋体", "Noto Serif CJK SC"], ["黑体", "Noto Sans CJK"]]) {
      const plan = buildFontPlan([name!]).get(name!);
      expect(plan).toBeDefined();
      expect(plan!.family).toBe(family);
      expect(plan!.localCandidates).toBeUndefined();
    }
  });

  it("every substitute font-map.json names is a bundled family", () => {
    const subs = new Set(Object.values((fontMap as { mappings: Record<string, { substitute: string | null }> }).mappings)
      .map((m) => m.substitute)
      .filter((x): x is string => x !== null));
    for (const sub of subs) expect(BUNDLED_FONT_FAMILIES[sub], sub).toBeDefined();
  });

  it("every mapped substitute has fetchable URLs (no map entry points at a missing face)", () => {
    for (const name of ["Georgia", "Lato", "Poppins", "Comic Sans MS", "MS Mincho", "SimSun", "PMingLiU", "Batang"]) {
      const plan = buildFontPlan([name]).get(name);
      expect(plan?.urls?.regular, name).toMatch(/^https:\/\/cdn\.jsdelivr\.net\//);
    }
  });
});
