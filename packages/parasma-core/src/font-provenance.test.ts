/**
 * Visibility surfaces (local-font fidelity):
 *
 *  1. fontProvenance() — the TS-side, probe-INDEPENDENT snapshot the headless
 *     fonts.debug command merges from: declared families + plan resolution +
 *     origin/freshness + loadedVariants (from the retained
 *     #registeredFaces, so it is truthful about what document.fonts holds).
 *  2. debugFonts() row merge of the wasm probe's per-family FINAL-STATE report
 *     (`families: [{requested, alias, resolvesNow, fellBackToDefault}]`) — each
 *     row gains resolvesNow/fellBackToDefault/alias by `used === requested`.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { Viewer } from "./index";

class FakeWasm {
  loadFontCalls: Array<{ family: string; weight: number; italic: boolean }> = [];
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
  loadFont(family: string, _bytes: Uint8Array, weight: number, italic: boolean): void {
    this.loadFontCalls.push({ family, weight, italic });
  }
  imageRefs(): unknown[] {
    return [];
  }
  clearImageBytes(): void {}
  free(): void {}
  // The extended debug-probe shape: faces + per-requested-family report.
  debugFonts(): unknown {
    return {
      aliases: [],
      faces: [],
      families: [
        {
          requested: "DejaVu Sans",
          alias: "dejavu sans",
          resolvesNow: "dejavu sans",
          fellBackToDefault: false,
        },
      ],
    };
  }
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

describe("font provenance snapshot + probe merge", () => {
  let origFetch: unknown;
  let origFontFace: unknown;
  let origDocument: unknown;
  let origCaches: unknown;

  beforeEach(() => {
    const g = globalThis as Record<string, unknown>;
    origFetch = g.fetch;
    origFontFace = g.FontFace;
    origDocument = g.document;
    origCaches = g.caches;

    const resp = {
      ok: true,
      arrayBuffer: async () => new ArrayBuffer(8),
      clone: () => resp,
    };
    g.fetch = vi.fn(async () => resp);
    g.FontFace = class {
      constructor(_f: string, _b: ArrayBuffer, _d: unknown) {}
      async load() {
        return this;
      }
    };
    g.document = { fonts: { add: vi.fn(), delete: vi.fn() } };
    const store = new Map<string, ArrayBuffer>();
    g.caches = {
      open: async () => ({
        match: async (k: string) =>
          store.has(k) ? { arrayBuffer: async () => store.get(k)!, headers: { get: () => null } } : undefined,
        put: async (k: string, r: { arrayBuffer: () => Promise<ArrayBuffer> }) => {
          store.set(k, await r.arrayBuffer());
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

  it("fontProvenance() reports declared/resolved/wordTarget/origin/freshness/loadedVariants sans probe", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);

    await viewer.load(new Uint8Array([1]));
    const snap = viewer.fontProvenance();

    expect(snap.useLocalFonts).toBe(false);
    expect(snap.families).toHaveLength(1);
    const f = snap.families[0]!;
    expect(f.declared).toBe("DejaVu Sans");
    expect(typeof f.resolved).toBe("string"); // the CDN substitute family
    expect(f.wordTarget).toBe("Verdana");
    expect(f.origin).toBe("cdn");
    expect(f.freshness).toBe("fresh");
    // All 4 faces registered this load → truthful loadedVariants from the
    // retained FontFace map (not the probe).
    expect(f.loadedVariants).toEqual(
      expect.arrayContaining(["regular", "bold", "italic", "boldItalic"]),
    );
    expect(f.loadedVariants).toHaveLength(4);
  });

  it("fontProvenance() before any load returns an empty snapshot (no throw)", () => {
    const viewer = makeViewer(new FakeWasm());
    const snap = viewer.fontProvenance();
    expect(snap.families).toEqual([]);
  });

  it("debugFonts() merges the probe's per-family report into each row", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);

    await viewer.load(new Uint8Array([1]));
    const dbg = (await viewer.debugFonts()) as {
      fonts: Array<{
        used: string;
        resolvesNow: string | null;
        fellBackToDefault: boolean | null;
        alias: string | null;
      }>;
    };
    const row = dbg.fonts.find((r) => r.used === "DejaVu Sans")!;
    expect(row.resolvesNow).toBe("dejavu sans");
    expect(row.fellBackToDefault).toBe(false);
    expect(row.alias).toBe("dejavu sans");
  });

  it("debugFonts() degrades row probe fields to null on a stale probe (no families array)", async () => {
    const wasm = new FakeWasm();
    wasm.debugFonts = () => ({ aliases: [], faces: [] }); // older probe build
    const viewer = makeViewer(wasm);

    await viewer.load(new Uint8Array([1]));
    const dbg = (await viewer.debugFonts()) as {
      fonts: Array<{ resolvesNow: string | null; fellBackToDefault: boolean | null }>;
    };
    expect(dbg.fonts[0]!.resolvesNow).toBeNull();
    expect(dbg.fonts[0]!.fellBackToDefault).toBeNull();
  });
});
