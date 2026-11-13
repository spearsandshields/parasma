/**
 * Re-resolve pipeline (local-font fidelity): viewer.refreshFonts().
 *
 *  1. SEMANTIC NO-OP GATE — when the rebuilt plan + per-face intent keys match
 *     what is already loaded, refreshFonts returns { changed:false } WITHOUT
 *     aborting layout, re-fetching, or re-laying out (zero wasm/net calls).
 *     RED: method missing pre-implementation; gate itself proven by mutation
 *     (force `changed = true` → the abort/runLayout counters trip).
 *  2. TOGGLE-FLIP APPLIES — flipping useLocalFonts then refreshFonts() aborts
 *     the current layout, re-fetches exactly the intent-changed faces (session
 *     cache with intent keys), replaces the FontFaces (delete-old
 *     before add), re-runs layout, and emits fonts:refreshed { changed:true }.
 *     RED: viewer.refreshFonts is not a function.
 *  3. RACING LOAD WINS — a load() issued after refreshFonts() bumps the
 *     generation synchronously; the queued refresh sees itself stale at its
 *     chain turn and exits without touching the wasm (no extra abort, no extra
 *     runLayout). RED (mutation): drop the stale() check → abort/runLayout
 *     counts double.
 *  4. WORKER PATH — on a worker-backed viewer the same pipeline routes
 *     abortLayout / loadFont / runLayout over the transport RPC (all the seams
 *     #doLoad already uses). RED: method missing pre-implementation.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { Viewer, type Transport, type TransportEvent } from "./index";

class FakeWasm {
  loadFontCalls: Array<{ family: string; bytes: Uint8Array; weight: number; italic: boolean }> = [];
  abortCount = 0;
  runLayoutCount = 0;
  loadDocx(_b: Uint8Array): void {}
  abortLayout(): void {
    this.abortCount++;
  }
  async runLayout(cb: (i: number) => void): Promise<number> {
    this.runLayoutCount++;
    // Yield first (mirrors the real wasm-bindgen future: no synchronous
    // pageReady inside the call), then stream two pages.
    await Promise.resolve();
    cb(0);
    cb(1);
    return 2;
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
  free(): void {}
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

/** Flush enough microtasks for the fake runLayout stream + emitters to settle. */
async function settle(n = 12): Promise<void> {
  for (let i = 0; i < n; i++) await Promise.resolve();
}

describe("viewer.refreshFonts() — the re-resolve pipeline", () => {
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
        clone: () => resp,
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

  it("semantic no-op: unchanged intent → { changed:false }, zero abort/fetch/layout calls", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    await viewer.load(new Uint8Array([1]));
    await settle();

    const refreshedEvents: Array<{ changed: boolean }> = [];
    viewer.on("fonts:refreshed", (p) => refreshedEvents.push(p));

    cdnFetch.mockClear();
    const loadFontsBefore = wasm.loadFontCalls.length;

    const res = await viewer.refreshFonts();
    await settle();

    expect(res).toEqual({ changed: false });
    expect(wasm.abortCount).toBe(0); // did NOT abort layout
    expect(wasm.runLayoutCount).toBe(1); // did NOT re-lay out
    expect(cdnFetch).not.toHaveBeenCalled(); // zero network
    expect(wasm.loadFontCalls.length).toBe(loadFontsBefore); // zero wasm font writes
    // The tab still gets a truth tick (header state) — changed:false.
    expect(refreshedEvents).toEqual([{ changed: false }]);
  });

  it("toggle-flip applies: abort → re-fetch intent-changed faces → replace → re-layout → fonts:refreshed", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const localBytes = new Uint8Array([42, 42, 42, 42]);
    const fetchLocalFont = vi.fn(async () => localBytes.buffer);
    viewer.setLocalFontSource({ fetchLocalFont });

    await viewer.load(new Uint8Array([1])); // CDN load
    await settle();
    expect(fetchLocalFont).not.toHaveBeenCalled();

    const refreshedEvents: Array<{ changed: boolean }> = [];
    viewer.on("fonts:refreshed", (p) => refreshedEvents.push(p));
    const pageReadyAfter: number[] = [];
    let loadCompletes = 0;
    viewer.on("loadComplete", () => loadCompletes++); // catch-up fires once now
    expect(loadCompletes).toBe(1);

    viewer.setLocalFontsEnabled(true);
    wasm.loadFontCalls.length = 0;
    const res = await viewer.refreshFonts();
    viewer.on("pageReady", (i) => pageReadyAfter.push(i)); // catch-up replay mid-stream ok
    await settle();

    expect(res).toEqual({ changed: true });
    expect(wasm.abortCount).toBe(1); // the doc-switch abort contract
    expect(wasm.runLayoutCount).toBe(2); // a REAL re-layout, not just re-render
    expect(fetchLocalFont).toHaveBeenCalled(); // local source consulted
    // All 4 faces re-loaded into the wasm with the LOCAL bytes.
    const localLoads = wasm.loadFontCalls.filter((c) => Array.from(c.bytes)[0] === 42);
    expect(localLoads.length).toBe(4);
    // delete-old-before-add: each replaced descriptor dropped its prior face.
    expect(fontsDelete).toHaveBeenCalledTimes(4);
    expect(registered.size).toBe(4);
    expect(refreshedEvents).toEqual([{ changed: true }]);
    // The re-layout completed and re-emitted the page lifecycle.
    expect(loadCompletes).toBe(2);
    expect(pageReadyAfter).toContain(0);
    expect(await viewer.pageCount()).toBe(2);
  });

  it("refresh back OFF re-fetches the cdn intent (last state wins across flips)", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const fetchLocalFont = vi.fn(async () => new Uint8Array([42]).buffer);
    viewer.setLocalFontSource({ fetchLocalFont });

    await viewer.load(new Uint8Array([1]));
    await settle();
    viewer.setLocalFontsEnabled(true);
    await viewer.refreshFonts();
    await settle();

    wasm.loadFontCalls.length = 0;
    viewer.setLocalFontsEnabled(false);
    const res = await viewer.refreshFonts();
    await settle();
    expect(res).toEqual({ changed: true });
    const cdnLoads = wasm.loadFontCalls.filter((c) => Array.from(c.bytes)[0] !== 42);
    expect(cdnLoads.length).toBe(4); // back to CDN bytes (Cache API or refetch)
    expect(wasm.runLayoutCount).toBe(3);

    // And a second refresh with no further flip is a no-op again.
    const res2 = await viewer.refreshFonts();
    expect(res2).toEqual({ changed: false });
    expect(wasm.runLayoutCount).toBe(3);
  });

  it("racing load wins: a load() issued after refreshFonts() makes the refresh exit untouched", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const fetchLocalFont = vi.fn(async () => new Uint8Array([42]).buffer);
    viewer.setLocalFontSource({ fetchLocalFont });

    await viewer.load(new Uint8Array([1]));
    await settle();

    // Flip ON so the refresh WOULD have work — then supersede it synchronously.
    viewer.setLocalFontsEnabled(true);
    const refresh = viewer.refreshFonts();
    const load2 = viewer.load(new Uint8Array([2])); // bumps the generation NOW

    expect(await refresh).toEqual({ changed: false }); // stale → gave way
    await load2;
    await settle();

    // Exactly ONE abort (load2's own doc-switch abort) and TWO layouts
    // (load1 + load2) — the stale refresh pipeline never ran its abort/layout.
    expect(wasm.abortCount).toBe(1);
    expect(wasm.runLayoutCount).toBe(2);
  });

  it("FontFace (OTS) rejection leaves the wasm font_cache untouched — no measure/paint divergence", async () => {
    // Chrome's OTS sanitizer is stricter than the engine's ttf-parser: bytes
    // the wasm would accept can be rejected by FontFace. The FontFace must
    // therefore validate FIRST — if it rejects, loadFont is never called and
    // both sides keep the same (old) state.
    const g = globalThis as Record<string, unknown>;
    g.FontFace = class {
      constructor(_f: string, _b: ArrayBuffer, _d: unknown) {}
      async load(): Promise<never> {
        throw new Error("OTS parsing error (simulated)");
      }
    };
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    await viewer.load(new Uint8Array([1]));
    await settle();

    expect(wasm.loadFontCalls.length).toBe(0); // wasm never touched
    expect(fontsAdd).not.toHaveBeenCalled(); // nothing registered either
    const prov = viewer.fontProvenance();
    expect(prov.families.length).toBeGreaterThan(0);
    expect(prov.families.every((f) => f.origin === "failed")).toBe(true);
  });

  it("aborting an in-flight layout emits NO spurious partial loadComplete", async () => {
    // An aborted wasm run_layout RESOLVES with its PARTIAL page count, and
    // refreshFonts deliberately does not bump #loadGeneration — so without the
    // #layoutSeq token the aborted layout's tail would emit loadComplete(1)
    // (partial) before the refresh's real loadComplete(2).
    class SlowWasm extends FakeWasm {
      releaseAbort: (() => void) | null = null;
      override abortLayout(): void {
        super.abortLayout();
        this.releaseAbort?.();
        this.releaseAbort = null;
      }
      override async runLayout(cb: (i: number) => void): Promise<number> {
        this.runLayoutCount++;
        await Promise.resolve();
        cb(0);
        if (this.runLayoutCount === 1) {
          // First layout: block mid-document until aborted, then resolve with
          // the partial count — exactly the wasm abort contract.
          await new Promise<void>((r) => {
            this.releaseAbort = r;
          });
          return 1; // partial
        }
        cb(1);
        return 2;
      }
    }
    const wasm = new SlowWasm();
    const viewer = makeViewer(wasm);
    const fetchLocalFont = vi.fn(async () => new Uint8Array([42]).buffer);
    viewer.setLocalFontSource({ fetchLocalFont });

    const completes: number[] = [];
    viewer.on("loadComplete", (n) => completes.push(n as number));

    await viewer.load(new Uint8Array([1]));
    await settle();
    expect(completes).toEqual([]); // first layout still blocked mid-document

    viewer.setLocalFontsEnabled(true);
    const res = await viewer.refreshFonts(); // aborts layout #1, runs layout #2
    await settle();

    expect(res).toEqual({ changed: true });
    expect(wasm.abortCount).toBe(1);
    // ONLY the refresh's final total — never the aborted layout's partial 1.
    expect(completes).toEqual([2]);
  });

  it("xlsx: aborting an in-flight layout emits NO spurious partial loadComplete", async () => {
    // #doLoadXlsx guarded its layout tail on #loadGeneration ONLY. refreshFonts
    // deliberately keeps the generation (same document) and an aborted wasm
    // run_layout RESOLVES with its PARTIAL count — so the xlsx tail published
    // loadComplete(partial). Browser-measured: a 1,154-page workbook emitted
    // loadComplete(18). Same SlowWasm shape as the docx case above.
    class SlowXlsxWasm extends FakeWasm {
      releaseAbort: (() => void) | null = null;
      loadXlsx(_b: Uint8Array): void {}
      override abortLayout(): void {
        super.abortLayout();
        this.releaseAbort?.();
        this.releaseAbort = null;
      }
      override async runLayout(cb: (i: number) => void): Promise<number> {
        this.runLayoutCount++;
        await Promise.resolve();
        cb(0);
        if (this.runLayoutCount === 1) {
          await new Promise<void>((r) => {
            this.releaseAbort = r;
          });
          return 1; // partial — the abort contract
        }
        cb(1);
        return 2;
      }
    }
    const wasm = new SlowXlsxWasm();
    const viewer = makeViewer(wasm);
    viewer.setLocalFontSource({ fetchLocalFont: async () => new Uint8Array([42]).buffer });

    const completes: number[] = [];
    viewer.on("loadComplete", (n) => completes.push(n as number));

    // CSV routes through csvToXlsx into the #doLoadXlsx path.
    await viewer.load(new TextEncoder().encode("a,b\n1,2\n"));
    await settle();
    expect(completes).toEqual([]); // still blocked mid-workbook

    viewer.setLocalFontsEnabled(true);
    const res = await viewer.refreshFonts();
    await settle();

    expect(res).toEqual({ changed: true });
    expect(wasm.abortCount).toBe(1);
    expect(completes).toEqual([2]); // NEVER the aborted layout's partial 1
  });

  it("pptx: aborting an in-flight layout emits NO spurious partial loadComplete", async () => {
    // #doLoadPptx's live() must carry the same #layoutSeq clause as the docx
    // and xlsx arms: refreshFonts deliberately keeps the generation (same
    // deck) and an aborted wasm run_layout RESOLVES with its PARTIAL slide
    // count — a generation-only guard publishes loadComplete(partial). Twin of
    // the xlsx case above; before this test the entire suite was blind to the
    // pptx arm's guard.
    class SlowPptxWasm extends FakeWasm {
      releaseAbort: (() => void) | null = null;
      loadPptx(_b: Uint8Array): void {}
      override abortLayout(): void {
        super.abortLayout();
        this.releaseAbort?.();
        this.releaseAbort = null;
      }
      override async runLayout(cb: (i: number) => void): Promise<number> {
        this.runLayoutCount++;
        await Promise.resolve();
        cb(0);
        if (this.runLayoutCount === 1) {
          await new Promise<void>((r) => {
            this.releaseAbort = r;
          });
          return 1; // partial — the abort contract
        }
        cb(1);
        return 2;
      }
    }
    const wasm = new SlowPptxWasm();
    const viewer = makeViewer(wasm);
    viewer.setLocalFontSource({ fetchLocalFont: async () => new Uint8Array([42]).buffer });

    const completes: number[] = [];
    viewer.on("loadComplete", (n) => completes.push(n as number));

    // ZIP magic + the verbatim part name sniffIsPptx scans for → #doLoadPptx.
    const pptxBytes = Uint8Array.of(
      0x50, 0x4b, 0x03, 0x04,
      ...new TextEncoder().encode("ppt/presentation.xml"),
    );
    await viewer.load(pptxBytes);
    await settle();
    expect(completes).toEqual([]); // still blocked mid-deck

    viewer.setLocalFontsEnabled(true);
    const res = await viewer.refreshFonts();
    await settle();

    expect(res).toEqual({ changed: true });
    expect(wasm.abortCount).toBe(1);
    expect(completes).toEqual([2]); // NEVER the aborted layout's partial 1
  });

  it("a settled local-only miss is RE-PROBED by an explicit refresh: a newly-installed brand font gets picked up", async () => {
    // A declared family with no substitute → url-less local-only plan. Its
    // first local read misses (font not installed) and settles at the engine
    // default. The user then installs the font; refreshFonts() must re-probe
    // and apply — an earlier no-op gate settled these forever.
    class BrandWasm extends FakeWasm {
      override fontFamiliesUsed(): string[] {
        return ["Brand Grotesque"];
      }
    }
    const wasm = new BrandWasm();
    const viewer = makeViewer(wasm);
    let installed: ArrayBuffer | null = null;
    viewer.setLocalFontSource({ fetchLocalFont: async () => installed });
    viewer.setLocalFontsEnabled(true);

    await viewer.load(new Uint8Array([1]));
    await settle();
    expect(wasm.loadFontCalls.length).toBe(0); // missed → engine default

    // Still not installed: explicit refresh re-probes but finds nothing.
    const r1 = await viewer.refreshFonts();
    await settle();
    expect(r1).toEqual({ changed: false });
    expect(wasm.loadFontCalls.length).toBe(0);

    // "Install" the font, then refresh again — must apply now.
    installed = new Uint8Array([77]).buffer;
    const r2 = await viewer.refreshFonts();
    await settle();
    expect(r2).toEqual({ changed: true });
    expect(wasm.loadFontCalls.some((c) => c.family === "Brand Grotesque")).toBe(true);
  });

  it("isRefreshingFonts() is true while the pipeline is in flight, false after", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    await viewer.load(new Uint8Array([1]));
    await settle();

    expect(viewer.isRefreshingFonts()).toBe(false);
    const p = viewer.refreshFonts();
    expect(viewer.isRefreshingFonts()).toBe(true); // set synchronously at entry
    await p;
    expect(viewer.isRefreshingFonts()).toBe(false);
  });
});

// ── Worker-backed refresh (the #remote branch) ────────────────────────────────

/** Wasm-export stand-in for the worker side (records RPC-dispatched calls). */
class FakeRemoteEngine {
  calls: Array<{ m: string; args: unknown[] }> = [];
  #rec(m: string, args: unknown[]): void {
    this.calls.push({ m, args });
  }
  countOf(m: string): number {
    return this.calls.filter((c) => c.m === m).length;
  }
  loadDocx(b: Uint8Array): void {
    this.#rec("loadDocx", [b]);
  }
  setNow(n: number): void {
    this.#rec("setNow", [n]);
  }
  pageSize(): { widthPx: number; heightPx: number } {
    this.#rec("pageSize", []);
    return { widthPx: 600, heightPx: 800 };
  }
  fontFamiliesUsed(): string[] {
    this.#rec("fontFamiliesUsed", []);
    return ["DejaVu Sans"];
  }
  addFontAlias(d: string, s: string): void {
    this.#rec("addFontAlias", [d, s]);
  }
  loadFont(family: string, bytes: Uint8Array, weight: number, italic: boolean): void {
    this.#rec("loadFont", [family, bytes, weight, italic]);
  }
  abortLayout(): void {
    this.#rec("abortLayout", []);
  }
  runLayout(gen: number): number {
    this.#rec("runLayout", [gen]);
    return 2;
  }
}

/** Minimal in-process Transport over the fake engine (structured-clone faithful). */
class MiniTransport implements Transport {
  readonly log: string[] = [];
  #backend: Record<string, ((...a: unknown[]) => unknown) | undefined>;
  constructor(backend: object) {
    this.#backend = backend as Record<string, (...a: unknown[]) => unknown>;
  }
  request(method: string, args: unknown[]): Promise<unknown> {
    this.log.push(method);
    const fn = this.#backend[method];
    if (typeof fn !== "function") return Promise.reject(new Error(`unknown worker method: ${method}`));
    try {
      return Promise.resolve(structuredClone(fn.apply(this.#backend, args.map((a) => structuredClone(a)))));
    } catch (e) {
      return Promise.reject(e);
    }
  }
  notify(method: string, args: unknown[]): void {
    this.log.push(method);
    const fn = this.#backend[method];
    if (typeof fn === "function") fn.apply(this.#backend, args.map((a) => structuredClone(a)));
  }
  onEvent(_h: (e: TransportEvent) => void): () => void {
    return () => {};
  }
  onDead(_h: (e: Error) => void): () => void {
    return () => {};
  }
  dispose(): void {}
}

describe("viewer.refreshFonts() — worker-backed (#remote)", () => {
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
    g.caches = undefined; // no Cache API → local read skipped, CDN path fetches
    g.fetch = vi.fn(async () => {
      const resp = { ok: true, arrayBuffer: async () => new ArrayBuffer(8), clone: () => resp };
      return resp;
    });
    g.FontFace = class {
      constructor(_f: string, _b: ArrayBuffer, _d: unknown) {}
      async load() {
        return this;
      }
    };
    g.document = { fonts: { add: vi.fn(), delete: vi.fn() } };
  });

  afterEach(() => {
    const g = globalThis as Record<string, unknown>;
    g.fetch = origFetch;
    g.FontFace = origFontFace;
    g.document = origDocument;
    g.caches = origCaches;
  });

  it("routes abortLayout / loadFont / runLayout over the transport RPC", async () => {
    const be = new FakeRemoteEngine();
    const transport = new MiniTransport(be);
    // localWasm stand-in: only the main-thread-local calls (#decodeImagesInBackground).
    const localWasm = { imageRefs: () => [], clearImageBytes: () => {}, free: () => {} };
    const viewer = Viewer.withTransport(transport, localWasm as never);
    // Local intent needs a supported provider (dev byte-source injection).
    viewer.setLocalFontSource({ fetchLocalFont: async () => new ArrayBuffer(8) });

    await viewer.load(new Uint8Array([1]));
    for (let i = 0; i < 12; i++) await Promise.resolve();
    expect(be.countOf("runLayout")).toBe(1);
    expect(be.countOf("loadFont")).toBe(4);

    viewer.setLocalFontsEnabled(true);
    const res = await viewer.refreshFonts();
    for (let i = 0; i < 12; i++) await Promise.resolve();

    expect(res).toEqual({ changed: true });
    // The whole pipeline crossed the wire: abort, the 4 face replacements, and
    // the fresh layout — the worker's wasm is the one being updated.
    expect(be.countOf("abortLayout")).toBe(1);
    expect(be.countOf("loadFont")).toBe(8); // 4 initial + 4 replaced
    expect(be.countOf("runLayout")).toBe(2);
  });

  it("worker no-op: unchanged intent dispatches no abort/loadFont/runLayout RPCs", async () => {
    const be = new FakeRemoteEngine();
    const transport = new MiniTransport(be);
    const localWasm = { imageRefs: () => [], clearImageBytes: () => {}, free: () => {} };
    const viewer = Viewer.withTransport(transport, localWasm as never);

    await viewer.load(new Uint8Array([1]));
    for (let i = 0; i < 12; i++) await Promise.resolve();
    const before = transport.log.length;

    const res = await viewer.refreshFonts();
    expect(res).toEqual({ changed: false });
    expect(transport.log.length).toBe(before); // ZERO RPCs
  });
});
