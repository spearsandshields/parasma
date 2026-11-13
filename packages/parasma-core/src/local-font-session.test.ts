/**
 * Correctness of the session font pipeline (local-font fidelity):
 *
 *  1. INTENT-KEYED session cache — the #loadedFaces key encodes whether the
 *     load would TRY local bytes for the family, so flipping useLocalFonts
 *     mid-session re-fetches on the next load instead of skipping.
 *     RED (old code): key was `${family}|${weight}|${style}` → the second load
 *     session-cache-hits and the local source is never consulted.
 *  2. ORIGIN SURVIVES CACHE HITS — provenance ("local"|"cdn"|"failed") lives in
 *     a map that a session-cache hit does not overwrite; freshness
 *     ("fresh"|"cached") is reported separately.
 *     RED (old code): a hit overwrote the source with "cached", destroying it,
 *     and no freshness field existed.
 *  3. FONTFACE RETENTION + DELETE-OLD-BEFORE-ADD — re-fetching a descriptor
 *     replaces the registered FontFace (never two same-descriptor faces), and
 *     destroy() unregisters every retained face.
 *     RED (old code): the FontFace was a dropped local → re-fetch doubled the
 *     registrations and destroy() left them all behind.
 *  4. PROVENANCE AFTER SUCCESS — origin is recorded only once BOTH the wasm
 *     loadFont AND the FontFace registration succeed; a failure records
 *     "failed".
 *     RED (old code): source was recorded after the byte fetch but BEFORE
 *     loadFont, so a rejecting loadFont still reported "cdn".
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { Viewer } from "./index";

class FakeWasm {
  loadFontCalls: Array<{ family: string; bytes: Uint8Array; weight: number; italic: boolean }> = [];
  loadFontShouldThrow = false;
  loadDocx(_b: Uint8Array): void {}
  abortLayout(): void {}
  async runLayout(_cb: (i: number) => void): Promise<number> {
    return 1;
  }
  setNow(_n: number): void {}
  pageSize(): { widthPx: number; heightPx: number } {
    return { widthPx: 600, heightPx: 800 };
  }
  fontFamiliesUsed(): string[] {
    return ["DejaVu Sans"]; // → plan.localCandidates = ["Verdana"]
  }
  addFontAlias(_d: string, _s: string): void {}
  loadFont(family: string, bytes: Uint8Array, weight: number, italic: boolean): void {
    if (this.loadFontShouldThrow) throw new Error("ttf parse failed (synthetic)");
    this.loadFontCalls.push({ family, bytes, weight, italic });
  }
  imageRefs(): unknown[] {
    return [];
  }
  clearImageBytes(): void {}
  free(): void {}
  // debug-probe surface: empty faces table — enough for the source/freshness rows
  debugFonts(): unknown {
    return { faces: [] };
  }
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

type FontRow = {
  used: string;
  source: string | null;
  freshness: string | null;
  loadedVariants: string[];
};
type FontsDbg = { fonts: FontRow[] };

describe("session font pipeline — intent keying, provenance, FontFace retention", () => {
  let origFetch: unknown;
  let origFontFace: unknown;
  let origDocument: unknown;
  let origCaches: unknown;
  let cdnFetch: ReturnType<typeof vi.fn>;
  let registered: Set<unknown>;
  let fontsAdd: ReturnType<typeof vi.fn>;
  let fontsDelete: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    const g = globalThis as Record<string, unknown>;
    origFetch = g.fetch;
    origFontFace = g.FontFace;
    origDocument = g.document;
    origCaches = g.caches;

    cdnFetch = vi.fn(async () => {
      const resp = {
        ok: true,
        arrayBuffer: async () => new ArrayBuffer(8),
        clone: () => resp, // fetchWithCache stores a clone in the Cache API
      };
      return resp;
    });
    g.fetch = cdnFetch;
    g.FontFace = class {
      constructor(_f: string, _b: ArrayBuffer, _d: unknown) {}
      async load() {
        return this;
      }
    };
    registered = new Set<unknown>();
    fontsAdd = vi.fn((f: unknown) => registered.add(f));
    fontsDelete = vi.fn((f: unknown) => registered.delete(f));
    g.document = { fonts: { add: fontsAdd, delete: fontsDelete } };

    const store = new Map<string, ArrayBuffer>();
    g.caches = {
      open: async () => ({
        match: async (k: string) =>
          store.has(k) ? { arrayBuffer: async () => store.get(k)!, headers: { get: () => null } } : undefined,
        put: async (k: string, resp: { arrayBuffer: () => Promise<ArrayBuffer> }) => {
          store.set(k, await resp.arrayBuffer());
        },
      }),
    };
  });

  afterEach(() => {
    const g = globalThis as Record<string, unknown>;
    g.fetch = origFetch;
    g.FontFace = origFontFace;
    g.document = origDocument;
    g.caches = origCaches;
  });

  it("re-fetches when the useLocalFonts toggle flips (intent-keyed session cache)", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const localBytes = new Uint8Array([42, 42, 42, 42]);
    const fetchLocalFont = vi.fn(async () => localBytes.buffer);
    viewer.setLocalFontSource({ fetchLocalFont });

    // Load 1: local fonts OFF → CDN only.
    await viewer.load(new Uint8Array([1]));
    expect(fetchLocalFont).not.toHaveBeenCalled();
    expect(cdnFetch).toHaveBeenCalled();

    // Load 2: toggle ON → the intent bit changes the session-cache key, so the
    // faces must be re-fetched via the LOCAL source (old key would skip here).
    viewer.setLocalFontsEnabled(true);
    await viewer.load(new Uint8Array([1]));
    expect(fetchLocalFont).toHaveBeenCalled();
    const localLoads = wasm.loadFontCalls.filter((c) => Array.from(c.bytes)[0] === 42);
    expect(localLoads.length).toBeGreaterThan(0); // wasm actually got the local bytes

    // Load 3: toggle back OFF → the replaced faces are stale for the cdn intent
    // too, so the CDN bytes must be re-loaded (possibly via the Cache API —
    // the point is the faces are NOT skipped by the session set).
    wasm.loadFontCalls.length = 0;
    viewer.setLocalFontsEnabled(false);
    await viewer.load(new Uint8Array([1]));
    const cdnLoads = wasm.loadFontCalls.filter((c) => Array.from(c.bytes)[0] !== 42);
    expect(cdnLoads.length).toBe(4); // all 4 faces re-loaded with CDN bytes
  });

  it("origin survives a session-cache hit; freshness reported separately", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const fetchLocalFont = vi.fn(async () => new Uint8Array([7]).buffer);
    viewer.setLocalFontSource({ fetchLocalFont });
    viewer.setLocalFontsEnabled(true);

    await viewer.load(new Uint8Array([1]));
    let dbg = (await viewer.debugFonts()) as FontsDbg;
    expect(dbg.fonts[0]!.source).toBe("local");
    expect(dbg.fonts[0]!.freshness).toBe("fresh");

    // Same intent → session-cache hit: the hit must NOT destroy the origin.
    await viewer.load(new Uint8Array([1]));
    dbg = (await viewer.debugFonts()) as FontsDbg;
    expect(dbg.fonts[0]!.source).toBe("local"); // old code reported "cached" here
    expect(dbg.fonts[0]!.freshness).toBe("cached");
  });

  it("delete-old-before-add: exactly one registered face per descriptor; destroy() unregisters", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const fetchLocalFont = vi.fn(async () => new Uint8Array([9]).buffer);
    viewer.setLocalFontSource({ fetchLocalFont });

    await viewer.load(new Uint8Array([1]));
    expect(registered.size).toBe(4); // 4 variants, 4 descriptors

    // Flip the toggle → same 4 descriptors re-fetched → each replacement must
    // delete the retained old face first (old code left 8 registered).
    viewer.setLocalFontsEnabled(true);
    await viewer.load(new Uint8Array([1]));
    expect(fontsDelete).toHaveBeenCalledTimes(4);
    expect(registered.size).toBe(4);

    viewer.destroy();
    expect(registered.size).toBe(0); // teardown removed every retained face
  });

  it("records 'failed' when loadFont rejects the bytes (provenance after success)", async () => {
    const wasm = new FakeWasm();
    wasm.loadFontShouldThrow = true;
    const viewer = makeViewer(wasm);

    await viewer.load(new Uint8Array([1])); // CDN fetch OK, wasm rejects the ttf
    const dbg = (await viewer.debugFonts()) as FontsDbg;
    expect(dbg.fonts[0]!.source).toBe("failed"); // old code optimistically said "cdn"
    expect(registered.size).toBe(0); // nothing reached document.fonts
  });
});
