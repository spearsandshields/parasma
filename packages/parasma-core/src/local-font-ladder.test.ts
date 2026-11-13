/**
 * Exact-match ladder + session catalog + permission flow
 * (local-font fidelity; browser behaviour verified 2026-07-10).
 *
 * RED conditions (each stated per test):
 *  - The catalog/ladder exports (buildLocalFontCatalog / matchDeclaredFamily /
 *    selectVariantFace / normalizeFontName) do not exist pre-implementation —
 *    the whole file fails at import (proven red).
 *  - Behavior-level reds are stated inline; the never-[0] hazard is additionally
 *    mutation-provable (replace selectVariantFace's return with group[0] → the
 *    Avenir tests pick Avenir-Black).
 *
 * Fixture strings are CHROME's real FontData values as observed (NOT the
 * name-table ID 1 values a font tool reports — Chrome reports family "Avenir"
 * for every cut).
 */
import { describe, it, expect, vi } from "vitest";
import {
  LocalFontProvider,
  buildLocalFontCatalog,
  matchDeclaredFamily,
  selectVariantFace,
  normalizeFontName,
  readLocalFontCached,
  flushLocalFontCacheEntries,
  type LocalFaceLike,
} from "./local-font-provider";

const face = (
  family: string,
  fullName: string,
  postscriptName: string,
  style: string,
  byte = 1,
): LocalFaceLike => ({
  family,
  fullName,
  postscriptName,
  style,
  blob: async () => new Blob([new Uint8Array([byte])]),
});

/** Chrome 149 on macOS — alphabetical by postscriptName within family, so
 *  index 0 is Avenir-Black (the live [0]-hazard). */
const AVENIR: LocalFaceLike[] = [
  face("Avenir", "Avenir Black", "Avenir-Black", "Black"),
  face("Avenir", "Avenir Black Oblique", "Avenir-BlackOblique", "Black Oblique"),
  face("Avenir", "Avenir Book", "Avenir-Book", "Book", 42),
  face("Avenir", "Avenir Book Oblique", "Avenir-BookOblique", "Book Oblique"),
  face("Avenir", "Avenir Heavy", "Avenir-Heavy", "Heavy"),
  face("Avenir", "Avenir Heavy Oblique", "Avenir-HeavyOblique", "Heavy Oblique"),
  face("Avenir", "Avenir Light", "Avenir-Light", "Light"),
  face("Avenir", "Avenir Light Oblique", "Avenir-LightOblique", "Light Oblique"),
  face("Avenir", "Avenir Medium", "Avenir-Medium", "Medium"),
  face("Avenir", "Avenir Medium Oblique", "Avenir-MediumOblique", "Medium Oblique"),
  face("Avenir", "Avenir Oblique", "Avenir-Oblique", "Oblique"),
  face("Avenir", "Avenir Roman", "Avenir-Roman", "Roman", 7),
];

const TIMES: LocalFaceLike[] = [
  face("Times New Roman", "Times New Roman Bold", "TimesNewRomanPS-BoldMT", "Bold"),
  face("Times New Roman", "Times New Roman Bold Italic", "TimesNewRomanPS-BoldItalicMT", "Bold Italic"),
  face("Times New Roman", "Times New Roman Italic", "TimesNewRomanPS-ItalicMT", "Italic"),
  face("Times New Roman", "Times New Roman", "TimesNewRomanPSMT", "Regular", 11),
];

describe("normalizeFontName", () => {
  it("lowercases and strips non-alphanumerics (zero fuzz beyond that)", () => {
    expect(normalizeFontName("Times New Roman")).toBe("timesnewroman");
    expect(normalizeFontName("Avenir-Book")).toBe("avenirbook");
    expect(normalizeFontName("Segoe UI")).toBe("segoeui");
  });
});

describe("matchDeclaredFamily — the 4-tier exact ladder", () => {
  const catalog = buildLocalFontCatalog([...AVENIR, ...TIMES]);

  it("tier 1: FontData.family == declared", () => {
    // RED: pre-impl there is no ladder at all.
    expect(matchDeclaredFamily(catalog, "Times New Roman")).toMatchObject({ tier: 1 });
    expect(matchDeclaredFamily(catalog, "Avenir")).toMatchObject({ tier: 1 });
    expect(matchDeclaredFamily(catalog, "Avenir")!.faces).toHaveLength(12);
  });

  it("tier 2: fullName == declared ('Avenir Book' — family is 'Avenir' in Chrome)", () => {
    // RED: the ID1-based expectation (family == "Avenir Book") is empirically
    // WRONG in Chrome; only the fullName tier can serve this declared name.
    const m = matchDeclaredFamily(catalog, "Avenir Book")!;
    expect(m.tier).toBe(2);
    expect(m.faces.map((f) => f.postscriptName)).toEqual(["Avenir-Book"]);
  });

  it("tier 3: postscriptName == declared (normalized: 'Avenir-BookOblique')", () => {
    // A declared string that misses family+fullName but IS a psName (fullName
    // here is the bare family, so only the psName tier can serve it).
    const c = buildLocalFontCatalog([
      face("Nimbus Sans", "Nimbus Sans", "NimbusSans-Regular", "Regular"),
    ]);
    const m = matchDeclaredFamily(c, "NimbusSans-Regular")!;
    expect(m.tier).toBe(3);
  });

  it("tier 4: family + style concat == declared", () => {
    // fullName intentionally DIFFERS from family+style so tier 2 misses.
    const c = buildLocalFontCatalog([
      face("Foo", "Foo Text Regular", "Foo-Reg", "Display"),
    ]);
    const m = matchDeclaredFamily(c, "Foo Display")!;
    expect(m.tier).toBe(4);
    expect(m.faces[0]!.postscriptName).toBe("Foo-Reg");
  });

  it("misses a localized / unknown name with NO fuzz", () => {
    expect(matchDeclaredFamily(catalog, "Times Nouveau Romain")).toBeNull();
    expect(matchDeclaredFamily(catalog, "TimesNewRoman2")).toBeNull();
  });

  it("never matches on an empty normalization (CJK names don't collide)", () => {
    // RED: a naive Map keyed by "" would return 宋体's face for 黑体.
    const c = buildLocalFontCatalog([face("宋体", "宋体", "SimSun", "宋体")]);
    expect(matchDeclaredFamily(c, "黑体")).toBeNull();
  });
});

describe("selectVariantFace — metadata variant selection, never [0]", () => {
  it("regular request on Avenir picks Roman (regular-synonym; [0] is Black)", () => {
    // RED (mutation): return group[0] → Avenir-Black.
    const f = selectVariantFace(AVENIR, false, false)!;
    expect(f.postscriptName).toBe("Avenir-Roman");
  });

  it("italic request on Avenir picks the bare Oblique cut", () => {
    const f = selectVariantFace(AVENIR, false, true)!;
    expect(f.postscriptName).toBe("Avenir-Oblique");
  });

  it("bold request with no bold-styled face falls back to the group regular", () => {
    // Avenir has Black/Heavy but no "Bold" — the fallback is Roman, not [0].
    const f = selectVariantFace(AVENIR, true, false)!;
    expect(f.postscriptName).toBe("Avenir-Roman");
  });

  it("classic 4-cut family resolves all four variants exactly", () => {
    expect(selectVariantFace(TIMES, false, false)!.postscriptName).toBe("TimesNewRomanPSMT");
    expect(selectVariantFace(TIMES, true, false)!.postscriptName).toBe("TimesNewRomanPS-BoldMT");
    expect(selectVariantFace(TIMES, false, true)!.postscriptName).toBe("TimesNewRomanPS-ItalicMT");
    expect(selectVariantFace(TIMES, true, true)!.postscriptName).toBe("TimesNewRomanPS-BoldItalicMT");
  });

  it("bold prefers the exact 'Bold' cut over 'Semibold' (shortest-loose tiebreak)", () => {
    const g = [
      face("X", "X Semibold", "X-Semibold", "Semibold"),
      face("X", "X Bold", "X-Bold", "Bold"),
    ];
    expect(selectVariantFace(g, true, false)!.postscriptName).toBe("X-Bold");
  });

  it("a single-face group (tier 2/3/4 direct hit) serves any request", () => {
    const g = [face("Avenir", "Avenir Book", "Avenir-Book", "Book")];
    expect(selectVariantFace(g, false, false)!.postscriptName).toBe("Avenir-Book");
  });

  it("returns null for a multi-face group with no regular synonym on a regular request", () => {
    const g = [
      face("Y", "Y Light", "Y-Light", "Light"),
      face("Y", "Y Black", "Y-Black", "Black"),
    ];
    expect(selectVariantFace(g, false, false)).toBeNull();
  });
});

describe("LocalFontProvider — session catalog + ladder read + matchTier", () => {
  it("reads via the ladder with tier reporting; catalog queried ONCE per session", async () => {
    // RED: pre-impl read() has no catalog (queries every read) and no matchTier.
    const query = vi.fn(async () => [...TIMES, ...AVENIR]);
    const p = new LocalFontProvider({ queryLocalFonts: query, hasUserActivation: () => true });

    const a = await p.read(["Times New Roman"], "400", "normal");
    expect(a).not.toBeNull();
    expect(a!.matchTier).toBe(1);
    expect(a!.family).toBe("Times New Roman");
    expect(a!.postscriptName).toBe("TimesNewRomanPSMT");
    expect(new Uint8Array(a!.bytes)).toEqual(new Uint8Array([11]));

    const b = await p.read(["Avenir Book"], "400", "normal");
    expect(b!.matchTier).toBe(2);
    expect(new Uint8Array(b!.bytes)).toEqual(new Uint8Array([42]));

    expect(query).toHaveBeenCalledTimes(1); // session catalog, not per-read
    expect(p.catalogReady).toBe(true);
  });

  it("reports 'curated' for any non-first candidate (declared is index 0)", async () => {
    const p = new LocalFontProvider({
      queryLocalFonts: async () => TIMES,
      hasUserActivation: () => true,
    });
    const r = await p.read(["DejaVu Sans", "Times New Roman"], "400", "normal");
    expect(r!.matchTier).toBe("curated");
    expect(r!.family).toBe("Times New Roman");
  });

  it("reports 'dev-route' on the injected fetchLocalFont path", async () => {
    const p = new LocalFontProvider({ fetchLocalFont: async () => new Uint8Array([5]).buffer });
    const r = await p.read(["Times New Roman"], "400", "normal");
    expect(r!.matchTier).toBe("dev-route");
  });

  it("granted permission enables SILENT enumeration (no activation)", async () => {
    // RED: old code required hasUserActivation() unconditionally.
    const p = new LocalFontProvider({
      queryLocalFonts: async () => TIMES,
      hasUserActivation: () => false,
      permissionsQuery: async () => ({ state: "granted" }),
    });
    const r = await p.read(["Times New Roman"], "400", "normal");
    expect(r).not.toBeNull();
  });

  it("prompt state without activation never calls queryLocalFonts (SecurityError avoided)", async () => {
    const query = vi.fn(async () => TIMES);
    const p = new LocalFontProvider({
      queryLocalFonts: query,
      hasUserActivation: () => false,
      permissionsQuery: async () => ({ state: "prompt" }),
    });
    expect(await p.read(["Times New Roman"], "400", "normal")).toBeNull();
    expect(query).not.toHaveBeenCalled();
  });

  it("invalidateCatalog() forces a re-enumeration on the next read", async () => {
    const query = vi.fn(async () => TIMES);
    const p = new LocalFontProvider({ queryLocalFonts: query, hasUserActivation: () => true });
    await p.read(["Times New Roman"], "400", "normal");
    p.invalidateCatalog();
    expect(p.catalogReady).toBe(false);
    await p.read(["Times New Roman"], "400", "normal");
    expect(query).toHaveBeenCalledTimes(2);
  });

  it("installedInCatalog: true/false with a catalog, null before one exists", async () => {
    const p = new LocalFontProvider({ queryLocalFonts: async () => TIMES, hasUserActivation: () => true });
    expect(p.installedInCatalog("Times New Roman")).toBeNull(); // lazy — not built yet
    await p.ensureCatalog();
    expect(p.installedInCatalog("Times New Roman")).toBe(true);
    expect(p.installedInCatalog("Comic Sans MS")).toBe(false);
  });
});

describe("LocalFontProvider — permission state + change subscription", () => {
  it("permissionState(): granted / prompt / denied pass through; TypeError → 'unsupported'", async () => {
    expect(
      await new LocalFontProvider({ permissionsQuery: async () => ({ state: "granted" }) }).permissionState(),
    ).toBe("granted");
    expect(
      await new LocalFontProvider({
        permissionsQuery: async () => {
          throw new TypeError("local-fonts is not a valid PermissionName");
        },
      }).permissionState(),
    ).toBe("unsupported");
    expect(await new LocalFontProvider({}).permissionState()).toBe("unsupported");
  });

  it("onPermissionChange fires (state, previous) on each transition and detaches on unsub", async () => {
    // RED: no subscription surface existed pre-impl.
    const status: { state: string; onchange: null | (() => void) } = { state: "granted", onchange: null };
    const p = new LocalFontProvider({ permissionsQuery: async () => status });
    const seen: Array<[string, string]> = [];
    const unsub = p.onPermissionChange((s, prev) => seen.push([s, prev]));
    await Promise.resolve();
    await Promise.resolve();
    expect(status.onchange).toBeTypeOf("function");

    status.state = "denied";
    status.onchange!();
    expect(seen).toEqual([["denied", "granted"]]);

    unsub();
    status.state = "granted";
    expect(status.onchange).toBeNull(); // detached
  });
});

describe("flushLocalFontCacheEntries — revocation flush", () => {
  it("deletes ONLY local-font.invalid entries, leaving CDN URL keys", async () => {
    // RED: helper absent pre-impl.
    const deleted: string[] = [];
    const cache = {
      keys: async () => [
        { url: "https://local-font.invalid/face/verdana/400/normal" },
        { url: "https://cdn.jsdelivr.net/gh/googlefonts/tinos@main/fonts/ttf/Tinos-Regular.ttf" },
        { url: "https://local-font.invalid/face/segoe%20ui/700/normal" },
      ],
      delete: async (req: { url: string }) => {
        deleted.push(req.url);
        return true;
      },
    };
    const n = await flushLocalFontCacheEntries(cache as unknown as Cache);
    expect(n).toBe(2);
    expect(deleted).toEqual([
      "https://local-font.invalid/face/verdana/400/normal",
      "https://local-font.invalid/face/segoe%20ui/700/normal",
    ]);
  });
});

describe("readLocalFontCached — matchTier survives the cache round-trip", () => {
  function fakeCache() {
    const store = new Map<string, { buf: ArrayBuffer; headers: Headers }>();
    return {
      match: async (k: string) => {
        const e = store.get(k);
        return e ? { arrayBuffer: async () => e.buf, headers: e.headers } : undefined;
      },
      put: async (k: string, resp: Response) => {
        store.set(k, { buf: await resp.arrayBuffer(), headers: resp.headers });
      },
    };
  }

  it("a cache hit reports the tier the original read matched at", async () => {
    // RED: pre-impl the entry carries no tier header, so a hit loses the tier.
    const cache = fakeCache();
    const provider = {
      permissionState: async () => "granted" as const,
      read: vi.fn(async () => ({
        bytes: new Uint8Array([1]).buffer,
        postscriptName: "Avenir-Book",
        family: "Avenir Book",
        matchTier: 2 as const,
      })),
    };
    await readLocalFontCached(cache as unknown as Cache, provider, ["Avenir Book"], "400", "normal");
    const hit = await readLocalFontCached(
      cache as unknown as Cache,
      { permissionState: async () => "granted" as const, read: vi.fn(async () => null) },
      ["Avenir Book"],
      "400",
      "normal",
    );
    expect(hit).not.toBeNull();
    expect(hit!.matchTier).toBe(2);
  });
});

describe("empty-normalization guards — each pinned INDIVIDUALLY (the two guards would otherwise mask each other)", () => {
  const cjkFace = (family: string, ps: string) => ({
    family,
    fullName: family,
    postscriptName: ps,
    style: "Regular",
    blob: async () => new Blob([new Uint8Array([1])]),
  });

  it("BUILD guard: fully non-alphanumeric names produce NO catalog keys (inspected directly)", () => {
    // Kills removal of buildLocalFontCatalog's `if (!key) return;` on its own —
    // no match call involved, so the match-side guard cannot mask it.
    const catalog = buildLocalFontCatalog([cjkFace("中文字体", "中文-PS体")]);
    expect(catalog.byFamily.has("")).toBe(false);
    expect(catalog.byFullName.has("")).toBe(false);
    expect(catalog.byFamilyStyle.has("")).toBe(false);
    // postscript "中文-PS体" normalizes to "ps" (non-empty) — allowed; only the
    // EMPTY key is forbidden.
    expect(catalog.byPostscript.has("")).toBe(false);
  });

  it("MATCH guard: an empty-normalizing declared name returns null even against a POISONED catalog", () => {
    // Kills removal of matchDeclaredFamily's `if (!key) return null;` on its
    // own: the catalog is hand-poisoned with an "" key (bypassing the build
    // guard), so only the match-side guard stands between two unrelated CJK
    // names "matching" each other.
    const poisoned = buildLocalFontCatalog([]);
    poisoned.byFamily.set("", [cjkFace("宋体", "SimSun")]);
    expect(matchDeclaredFamily(poisoned, "明朝体")).toBeNull();
  });
});
