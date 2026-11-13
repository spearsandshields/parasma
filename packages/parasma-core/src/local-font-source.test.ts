/**
 * End-to-end coverage of the local-font LOAD path: with `useLocalFonts` on and an injected dev byte-source
 * (`setLocalFontSource`), a doc whose plan carries localCandidates (DejaVu Sans
 * → Verdana) must pull bytes from the local source and NOT touch the CDN; with
 * the source returning null it must fall through to the CDN fetch.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { Viewer } from "./index";

class FakeWasm {
  loadFontCalls: Array<{ family: string; bytes: Uint8Array; weight: number; italic: boolean }> = [];
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
    this.loadFontCalls.push({ family, bytes, weight, italic });
  }
  imageRefs(): unknown[] {
    return [];
  }
  clearImageBytes(): void {}
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

describe("local-font dev source — load path", () => {
  let origFetch: unknown;
  let origFontFace: unknown;
  let origDocument: unknown;
  let origCaches: unknown;
  let cdnFetch: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    const g = globalThis as Record<string, unknown>;
    origFetch = g.fetch;
    origFontFace = g.FontFace;
    origDocument = g.document;
    origCaches = g.caches;

    cdnFetch = vi.fn(async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(8) }));
    g.fetch = cdnFetch;
    g.FontFace = class {
      constructor(_f: string, _b: ArrayBuffer, _d: unknown) {}
      async load() {
        return this;
      }
    };
    g.document = { fonts: { add: vi.fn() } };

    const store = new Map<string, ArrayBuffer>();
    g.caches = {
      open: async () => ({
        match: async (k: string) =>
          store.has(k) ? { arrayBuffer: async () => store.get(k)! } : undefined,
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

  it("prefers the injected local source and does NOT hit the CDN", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const localBytes = new Uint8Array([42, 42, 42, 42]);
    const fetchLocalFont = vi.fn(async (_f: string, _w: number, _i: boolean) => localBytes.buffer);

    viewer.setLocalFontSource({ fetchLocalFont });
    viewer.setLocalFontsEnabled(true);
    await viewer.load(new Uint8Array([1]));

    // The local source supplied every face → the DejaVu CDN file was never
    // fetched. The DECLARED family leads the candidate list (the
    // exact-name ladder), so the source is asked for "DejaVu Sans" first and —
    // since it answers — the curated "Verdana" hint is never needed.
    expect(fetchLocalFont).toHaveBeenCalled();
    expect(fetchLocalFont.mock.calls.map((c) => c[0])).toContain("DejaVu Sans");
    expect(cdnFetch).not.toHaveBeenCalled();
    // loadFont received the LOCAL bytes (Verdana), not a CDN blob.
    expect(wasm.loadFontCalls.length).toBeGreaterThan(0);
    expect(Array.from(wasm.loadFontCalls[0]!.bytes)).toEqual([42, 42, 42, 42]);
  });

  it("falls through to the CDN when the local source misses", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const fetchLocalFont = vi.fn(async () => null); // not installed

    viewer.setLocalFontSource({ fetchLocalFont });
    viewer.setLocalFontsEnabled(true);
    await viewer.load(new Uint8Array([1]));

    expect(fetchLocalFont).toHaveBeenCalled();
    expect(cdnFetch).toHaveBeenCalled(); // fell through to the bundled URL
  });

  it("does not consult the local source when useLocalFonts is off (default)", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const fetchLocalFont = vi.fn(async () => new Uint8Array([1]).buffer);

    viewer.setLocalFontSource({ fetchLocalFont });
    // useLocalFonts left OFF
    await viewer.load(new Uint8Array([1]));

    expect(fetchLocalFont).not.toHaveBeenCalled();
    expect(cdnFetch).toHaveBeenCalled();
  });
});
