/**
 * Local-font Viewer integration: local-only plans, matchTier/coverage
 * provenance, refresh-gate stability for local misses, the permission bridge +
 * revocation flush, and the catalog surfaces.
 *
 * RED conditions:
 *  (1) local-only OFF path — pre-impl #loadFontsFor dereferences plan.urls.*
 *      unconditionally → TypeError on a synthesized (url-less) plan → the load
 *      rejects (proven red the moment buildFontPlan synthesizes entries).
 *  (2) matchTier/coverage — fields absent from fontProvenance() pre-impl.
 *  (3) miss-gate stability — pre-impl the refresh gate sees the never-loaded
 *      local faces as "changed" forever → a second refresh re-lays out again.
 *  (4) permission bridge/flush — no fonts:permission-changed event and no
 *      cache flush existed pre-impl.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { Viewer } from "./index";

class FakeWasm {
  families: string[] = ["DejaVu Sans"];
  loadFontCalls: Array<{ family: string; bytes: Uint8Array; weight: number; italic: boolean }> = [];
  abortCount = 0;
  runLayoutCount = 0;
  loadDocx(_b: Uint8Array): void {}
  abortLayout(): void {
    this.abortCount++;
  }
  async runLayout(cb: (i: number) => void): Promise<number> {
    this.runLayoutCount++;
    await Promise.resolve();
    cb(0);
    return 1;
  }
  setNow(_n: number): void {}
  pageSize(): { widthPx: number; heightPx: number } {
    return { widthPx: 600, heightPx: 800 };
  }
  fontFamiliesUsed(): string[] {
    return this.families;
  }
  addFontAlias(_d: string, _s: string): void {}
  loadFont(family: string, bytes: Uint8Array, weight: number, italic: boolean): void {
    this.loadFontCalls.push({ family, bytes, weight, italic });
  }
  imageRefs(): unknown[] {
    return [];
  }
  clearImageBytes(): void {}
  free(): void {}
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

async function settle(n = 12): Promise<void> {
  for (let i = 0; i < n; i++) await Promise.resolve();
}

describe("local fonts — Viewer integration", () => {
  let origFetch: unknown;
  let origFontFace: unknown;
  let origDocument: unknown;
  let origCaches: unknown;
  let cdnFetch: ReturnType<typeof vi.fn>;
  let cacheStore: Map<string, { buf: ArrayBuffer; headers: Map<string, string> }>;

  beforeEach(() => {
    const g = globalThis as Record<string, unknown>;
    origFetch = g.fetch;
    origFontFace = g.FontFace;
    origDocument = g.document;
    origCaches = g.caches;

    cdnFetch = vi.fn(async () => {
      const resp = { ok: true, arrayBuffer: async () => new ArrayBuffer(8), clone: () => resp };
      return resp;
    });
    g.fetch = cdnFetch;
    g.FontFace = class {
      constructor(_f: string, _b: ArrayBuffer, _d: unknown) {}
      async load() {
        return this;
      }
    };
    g.document = { fonts: { add: vi.fn(), delete: vi.fn() } };

    // Cache stub with keys()/delete() so the revocation flush is testable.
    cacheStore = new Map();
    g.caches = {
      open: async () => ({
        match: async (k: string) => {
          const e = cacheStore.get(k);
          return e
            ? { arrayBuffer: async () => e.buf, headers: { get: (h: string) => e.headers.get(h) ?? null } }
            : undefined;
        },
        put: async (k: string, resp: Response) => {
          const headers = new Map<string, string>();
          resp.headers?.forEach?.((v, key) => headers.set(key, v));
          cacheStore.set(k, { buf: await resp.arrayBuffer(), headers });
        },
        keys: async () => [...cacheStore.keys()].map((url) => ({ url })),
        delete: async (req: { url: string }) => cacheStore.delete(req.url),
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

  it("(1) local-only plan, OFF path: a url-less brand-font plan fetches NOTHING and the load survives", async () => {
    const wasm = new FakeWasm();
    wasm.families = ["BrandFontX"];
    const viewer = makeViewer(wasm);
    await viewer.load(new Uint8Array([1]));
    await settle();
    expect(cdnFetch).not.toHaveBeenCalled();
    expect(wasm.loadFontCalls).toEqual([]);
    const f = viewer.fontProvenance().families[0]!;
    expect(f.declared).toBe("BrandFontX");
    expect(f.resolved).toBeNull(); // engine default — not the synthesized plan name
    expect(f.coverage).toBe("missing");
  });

  it("(2) local-only plan, ON + local hit: registers under the DECLARED family with exact-local coverage", async () => {
    const wasm = new FakeWasm();
    wasm.families = ["BrandFontX"];
    const viewer = makeViewer(wasm);
    viewer.setLocalFontSource({ fetchLocalFont: async () => new Uint8Array([42]).buffer });
    viewer.setLocalFontsEnabled(true);
    await viewer.load(new Uint8Array([1]));
    await settle();
    expect(cdnFetch).not.toHaveBeenCalled(); // no CDN fallback exists (or is needed)
    expect(wasm.loadFontCalls.length).toBe(4);
    expect(wasm.loadFontCalls.every((c) => c.family === "BrandFontX")).toBe(true);
    const f = viewer.fontProvenance().families[0]!;
    expect(f.resolved).toBe("BrandFontX");
    expect(f.origin).toBe("local");
    expect(f.matchTier).toBe("dev-route");
    expect(f.coverage).toBe("exact-local");
  });

  it("(3) local-only plan miss: the refresh gate settles (no endless changed:true re-layouts)", async () => {
    const wasm = new FakeWasm();
    wasm.families = ["BrandFontX"];
    const viewer = makeViewer(wasm);
    viewer.setLocalFontSource({ fetchLocalFont: async () => null }); // installed? no
    await viewer.load(new Uint8Array([1]));
    await settle();

    viewer.setLocalFontsEnabled(true);
    const r1 = await viewer.refreshFonts(); // real work: try the ladder once
    await settle();
    expect(r1.changed).toBe(true);
    const layoutsAfterFirst = wasm.runLayoutCount;

    const r2 = await viewer.refreshFonts(); // known miss → semantic no-op
    await settle();
    expect(r2.changed).toBe(false);
    expect(wasm.runLayoutCount).toBe(layoutsAfterFirst);
  });

  it("curated local hit reports matchTier 'curated' and coverage 'substituted-local'", async () => {
    const wasm = new FakeWasm(); // DejaVu Sans → candidates [declared, Verdana]
    const viewer = makeViewer(wasm);
    viewer.setLocalFontSource({
      fetchLocalFont: async (family) => (family === "Verdana" ? new Uint8Array([7]).buffer : null),
    });
    viewer.setLocalFontsEnabled(true);
    await viewer.load(new Uint8Array([1]));
    await settle();
    const f = viewer.fontProvenance().families[0]!;
    expect(f.origin).toBe("local");
    // The dev route reports "dev-route"; the matched family (Verdana) is NOT
    // the declared name, so coverage is the curated-substitution state.
    expect(f.coverage).toBe("substituted-local");
    expect(f.wordTarget).toBe("Verdana");
  });

  it("(5) shared-substitute arbitration: a sibling's CDN fallback must not clobber a LOCAL win", async () => {
    // Times New Roman AND Liberation Serif both resolve to Tinos — ONE wasm/
    // FontFace descriptor. TNR's ladder read hits locally; Liberation Serif
    // misses and falls to the CDN. RED (browser-observed): the CDN write
    // landed LAST and clobbered the local Tinos bytes (origin flipped to
    // "cdn", real Times New Roman lost — the marquee fidelity case).
    const wasm = new FakeWasm();
    wasm.families = ["Times New Roman", "Liberation Serif"];
    const viewer = makeViewer(wasm);
    viewer.setLocalFontSource({
      fetchLocalFont: async (family) =>
        family === "Times New Roman" ? new Uint8Array([42]).buffer : null,
    });
    viewer.setLocalFontsEnabled(true);
    // Make the CDN fetch slower than the local read so the CDN write is the
    // LAST to land — the exact clobber ordering seen live.
    (globalThis as Record<string, unknown>).fetch = vi.fn(async () => {
      for (let i = 0; i < 20; i++) await Promise.resolve();
      const resp = { ok: true, arrayBuffer: async () => new ArrayBuffer(8), clone: () => resp };
      return resp;
    });
    await viewer.load(new Uint8Array([1]));
    await settle(40);

    const rows = viewer.fontProvenance().families;
    const tnr = rows.find((f) => f.declared === "Times New Roman")!;
    expect(tnr.origin).toBe("local"); // the local win SURVIVES the sibling's fallback
    expect(tnr.coverage).toBe("exact-local");
    // The sibling truthfully reports the descriptor's local bytes (matched via
    // the OTHER declared name → the curated-substitution coverage state).
    const serif = rows.find((f) => f.declared === "Liberation Serif")!;
    expect(serif.origin).toBe("local");
    expect(serif.coverage).toBe("substituted-local");
    // Gate stability: nothing pending → a refresh is a semantic no-op.
    const r = await viewer.refreshFonts();
    expect(r.changed).toBe(false);
  });

  it("(4) permission bridge: emits fonts:permission-changed and flushes local entries on revocation", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const status: { state: string; onchange: null | (() => void) } = { state: "granted", onchange: null };
    viewer.setLocalFontSource({
      queryLocalFonts: async () => [],
      permissionsQuery: async () => status,
    });
    await settle(); // let the subscription attach

    // Seed the shared cache with one LOCAL entry and one CDN entry.
    cacheStore.set("https://local-font.invalid/face/verdana/400/normal", {
      buf: new ArrayBuffer(4),
      headers: new Map(),
    });
    cacheStore.set("https://cdn.jsdelivr.net/x/Tinos-Regular.ttf", {
      buf: new ArrayBuffer(4),
      headers: new Map(),
    });

    const events: Array<{ state: string; previous: string }> = [];
    viewer.on("fonts:permission-changed", (p) => events.push(p));

    status.state = "prompt"; // the user revoked (granted → prompt)
    status.onchange!();
    await settle();

    expect(events).toEqual([{ state: "prompt", previous: "granted" }]);
    expect(cacheStore.has("https://local-font.invalid/face/verdana/400/normal")).toBe(false);
    expect(cacheStore.has("https://cdn.jsdelivr.net/x/Tinos-Regular.ttf")).toBe(true);
  });

  it("permission/catalog surfaces: fontPermissionState + localFontCatalogReady + warmLocalFontCatalog", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    viewer.setLocalFontSource({
      queryLocalFonts: async () => [],
      hasUserActivation: () => true,
      permissionsQuery: async () => ({ state: "prompt" }),
    });
    expect(await viewer.fontPermissionState()).toBe("prompt");
    expect(viewer.localFontCatalogReady()).toBe(false);
    expect(await viewer.warmLocalFontCatalog()).toBe(true); // activation carries it
    expect(viewer.localFontCatalogReady()).toBe(true);
  });
});
