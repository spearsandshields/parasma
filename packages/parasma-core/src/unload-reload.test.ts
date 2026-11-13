/**
 * unload-reload.test.ts — the engine half of the viewer teardown contract.
 *
 * Viewer.unload() is the WARM release: it drops the loaded document (page state,
 * layout, retained images, this-doc FontFaces via #releaseDocument) but KEEPS the
 * wasm module + worker transport alive, so a subsequent load() re-shows cheaply.
 * Viewer.destroy() is the full teardown (frees the wasm once, idempotent). Driven
 * over the same FakeWasm pattern as image-object-copy.test.
 *
 * The FontFace/document.fonts deletion inside #releaseDocument is NOT node-
 * testable (no FontFace in env:node) — it is browser-verified; here we assert the
 * observable doc-state + wasm-warmth + idempotency + composition seams.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { Viewer } from "./index";

class FakeWasm {
  free = vi.fn();
  loadDocx(_bytes: Uint8Array): void {}
  abortLayout(): void {}
  async runLayout(cb: (idx: number) => void): Promise<number> {
    cb(0);
    cb(1);
    cb(2);
    return 3;
  }
  setNow(_n: number): void {}
  pageSize(): { widthPx: number; heightPx: number } {
    return { widthPx: 600, heightPx: 800 };
  }
  fontFamiliesUsed(): string[] {
    return [];
  }
  imageRefs(): Array<{ relId: string; mime: string; bytes: Uint8Array }> {
    return [];
  }
  setImageBitmap(_relId: string, _bitmap: unknown): void {}
  clearImageBytes(): void {}
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

const DOCX = new Uint8Array([1, 2, 3, 4]);
const settle = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

describe("Viewer teardown contract — unload / destroy", () => {
  // The engine's #releaseDocument guards document.fonts; keep a stub present so
  // the guard path executes without a real DOM.
  let savedDoc: unknown;
  beforeEach(() => {
    savedDoc = (globalThis as Record<string, unknown>).document;
    (globalThis as Record<string, unknown>).document = {
      fonts: { add: vi.fn(), delete: vi.fn() },
    };
  });
  afterEach(() => {
    (globalThis as Record<string, unknown>).document = savedDoc;
  });

  it("unload() clears the document state but keeps the wasm WARM", async () => {
    const wasm = new FakeWasm();
    const v = makeViewer(wasm);
    await v.load(DOCX);
    await settle();
    expect(await v.pageCount()).toBe(3);
    expect(await v.isLoadComplete()).toBe(true);

    v.unload();

    expect(await v.pageCount()).toBe(0);
    expect(await v.isLoadComplete()).toBe(false);
    await expect(v.pageSize()).rejects.toThrow(); // #pageSize null → no document
    expect(wasm.free).not.toHaveBeenCalled(); // WARM: wasm module retained
  });

  it("load() after unload() fully re-shows (the warm re-show tripwire)", async () => {
    const wasm = new FakeWasm();
    const v = makeViewer(wasm);
    await v.load(DOCX);
    await settle();
    v.unload();
    expect(await v.pageCount()).toBe(0);

    await v.load(DOCX);
    await settle();
    expect(await v.pageCount()).toBe(3);
    expect(await v.isLoadComplete()).toBe(true);
    expect(wasm.free).not.toHaveBeenCalled();
  });

  it("unload() is idempotent", async () => {
    const wasm = new FakeWasm();
    const v = makeViewer(wasm);
    await v.load(DOCX);
    await settle();
    v.unload();
    expect(() => v.unload()).not.toThrow();
    expect(wasm.free).not.toHaveBeenCalled();
  });

  it("destroy() frees the wasm exactly once and is idempotent", async () => {
    const wasm = new FakeWasm();
    const v = makeViewer(wasm);
    await v.load(DOCX);
    await settle();
    v.destroy();
    expect(wasm.free).toHaveBeenCalledOnce();
    v.destroy();
    expect(wasm.free).toHaveBeenCalledOnce();
  });

  it("destroy() with NO prior load() is safe (frees once, idempotent)", () => {
    // The parent may construct then tear down without ever showing a document
    // (a cancelled mount, a StrictMode double-invoke). #releaseDocument's clears
    // must no-op on empty ledgers and the destroy tail must optional-chain.
    const wasm = new FakeWasm();
    const v = makeViewer(wasm);
    expect(() => v.destroy()).not.toThrow();
    expect(wasm.free).toHaveBeenCalledOnce();
    expect(() => v.destroy()).not.toThrow();
    expect(wasm.free).toHaveBeenCalledOnce();
  });

  it("unload() with NO prior load() is a no-op (nothing to release, wasm warm)", () => {
    const wasm = new FakeWasm();
    const v = makeViewer(wasm);
    expect(() => v.unload()).not.toThrow();
    expect(wasm.free).not.toHaveBeenCalled();
  });

  it("composition: destroy() after unload() is safe (no double free)", async () => {
    const wasm = new FakeWasm();
    const v = makeViewer(wasm);
    await v.load(DOCX);
    await settle();
    v.unload();
    v.destroy();
    expect(wasm.free).toHaveBeenCalledOnce();
  });

  it("composition: unload() after destroy() is a guarded no-op", async () => {
    const wasm = new FakeWasm();
    const v = makeViewer(wasm);
    await v.load(DOCX);
    await settle();
    v.destroy();
    expect(() => v.unload()).not.toThrow();
    expect(wasm.free).toHaveBeenCalledOnce(); // destroy's single free, no more
  });
});

// ── unload() must not leave document.fonts short of the session cache ──

/** A wasm whose document declares a family, so the font pipeline actually runs. */
class FontWasm extends FakeWasm {
  override fontFamiliesUsed(): string[] {
    return ["DejaVu Sans"]; // → a real plan with CDN urls for 4 variants
  }
  addFontAlias(_d: string, _s: string): void {}
  loadFont(_f: string, _b: Uint8Array, _w: number, _i: boolean): void {}
}

describe("unload() → load() must re-register the session-cached FontFaces", () => {
  let saved: Record<string, unknown>;
  let registered: Set<unknown>;
  let cdnFetch: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    const g = globalThis as Record<string, unknown>;
    saved = { fetch: g.fetch, FontFace: g.FontFace, document: g.document, caches: g.caches };
    g.caches = undefined; // no Cache API → the plain CDN fetch path
    cdnFetch = vi.fn(async () => {
      const resp = { ok: true, arrayBuffer: async () => new ArrayBuffer(8), clone: () => resp };
      return resp;
    });
    g.fetch = cdnFetch;
    g.FontFace = class {
      constructor(
        readonly family: string,
        _b: ArrayBuffer,
        _d: unknown,
      ) {}
      async load(): Promise<unknown> {
        return this;
      }
    };
    registered = new Set<unknown>();
    g.document = {
      fonts: {
        add: vi.fn((f: unknown) => registered.add(f)),
        delete: vi.fn((f: unknown) => registered.delete(f)),
      },
    };
  });

  afterEach(() => {
    Object.assign(globalThis as Record<string, unknown>, saved);
  });

  it("the re-shown document's faces are in document.fonts again (measure==paint)", async () => {
    const wasm = new FontWasm();
    const v = makeViewer(wasm);

    await v.load(DOCX);
    await settle();
    const afterFirstLoad = registered.size;
    expect(afterFirstLoad).toBe(4); // 4 variants registered

    v.unload();
    expect(registered.size).toBe(0); // #releaseDocument deleted them all

    // The session cache (#loadedFaces) is deliberately NOT cleared by unload(),
    // so this load HITS it. Before the fix the hit skipped document.fonts.add
    // entirely: the wasm font_cache still measured with the metrics while the
    // canvas painted with a fallback.
    await v.load(DOCX);
    await settle();
    expect(registered.size).toBe(afterFirstLoad);
  });

  it("re-registers WITHOUT re-fetching — the warm-re-show optimisation is kept", async () => {
    const wasm = new FontWasm();
    const v = makeViewer(wasm);
    await v.load(DOCX);
    await settle();
    const fetchCalls = cdnFetch.mock.calls.length;
    expect(fetchCalls).toBeGreaterThan(0);

    v.unload();
    await v.load(DOCX);
    await settle();

    expect(cdnFetch.mock.calls.length).toBe(fetchCalls);
    expect(registered.size).toBe(4);
  });
});

// ── unload() must abort a running layout and stale-ify async tails ──
//
// The in-process runLayout future MACROTASK-yields between pages (the wasm's
// yield_to_browser is a MessageChannel post, not a bare microtask), so a click
// handler's unload() can run mid-layout in BOTH modes — this is not a
// worker-only hazard. Without the abort block, the still-running layout keeps
// streaming pageReady and finally publishes loadComplete(total), resurrecting
// the #pageCount/#loadComplete state that #releaseDocument just zeroed.

/**
 * A wasm whose FIRST layout parks mid-document (after page 0) until either
 * abortLayout() releases it — resolving with the PARTIAL count, the wasm abort
 * contract — or the test's release() lets it run to completion, which is what
 * an un-aborted layout does. Later layouts complete normally (3 pages).
 * loadDocx models the wasm-bindgen borrow: calling it while the runLayout
 * future is alive panics ("recursive use of an object") — this is why unload()
 * must KEEP #layoutPromise for the next load's unwind-await.
 */
class SlowLayoutWasm extends FakeWasm {
  abortCount = 0;
  aborted = false;
  runCount = 0;
  layoutInFlight = false;
  parked: (() => void) | null = null;
  override loadDocx(bytes: Uint8Array): void {
    if (this.layoutInFlight) throw new Error("recursive use of an object");
    super.loadDocx(bytes);
  }
  override abortLayout(): void {
    this.abortCount++;
    this.aborted = true;
    // The real wasm honours the abort at its next page-boundary yield — a
    // MACROTASK. Deferring the release keeps the unwind pending across the
    // next load's synchronous entry, so a lost unwind-await is catchable.
    setTimeout(() => {
      this.parked?.();
      this.parked = null;
    }, 0);
  }
  /** Un-aborted-mode driver: no-op when abortLayout() already emptied the park. */
  release(): void {
    this.parked?.();
    this.parked = null;
  }
  override async runLayout(cb: (idx: number) => void): Promise<number> {
    this.runCount++;
    this.layoutInFlight = true;
    try {
      await Promise.resolve();
      cb(0);
      if (this.runCount === 1) {
        await new Promise<void>((r) => {
          this.parked = r;
        });
        if (this.aborted) return 1; // abort contract: RESOLVE with the partial count
        cb(1);
        return 2; // nobody stopped it — ran to completion
      }
      cb(1);
      cb(2);
      return 3;
    } finally {
      this.layoutInFlight = false;
    }
  }
}

describe("unload() aborts a running layout", () => {
  let savedDoc: unknown;
  beforeEach(() => {
    savedDoc = (globalThis as Record<string, unknown>).document;
    (globalThis as Record<string, unknown>).document = {
      fonts: { add: vi.fn(), delete: vi.fn() },
    };
  });
  afterEach(() => {
    (globalThis as Record<string, unknown>).document = savedDoc;
  });

  it("mid-layout unload() signals the abort; the tail cannot resurrect the released document", async () => {
    const wasm = new SlowLayoutWasm();
    const v = makeViewer(wasm);
    const completes: number[] = [];
    v.on("loadComplete", (n) => completes.push(n as number));

    await v.load(DOCX);
    await settle();
    expect(await v.pageCount()).toBe(1); // streamed page 0, parked mid-document
    expect(completes).toEqual([]);

    v.unload();
    expect(wasm.abortCount).toBe(1); // unload signalled the wasm to stop

    // Subscribed AFTER unload (pageCount is 0 → no catch-up replay): anything
    // arriving below is the leak.
    const pagesAfter: number[] = [];
    v.on("pageReady", (i) => pagesAfter.push(i as number));

    // If unload did NOT abort, the layout keeps streaming and publishes
    // loadComplete(2) — this drives that path. No-op when the abort already
    // released the park.
    wasm.release();
    await settle();

    expect(pagesAfter).toEqual([]);
    expect(completes).toEqual([]); // no loadComplete for a released document
    expect(await v.pageCount()).toBe(0);
    expect(await v.isLoadComplete()).toBe(false);
  });

  it("load() after a mid-layout unload() awaits the unwind and re-shows cleanly", async () => {
    // unload() KEEPS #layoutPromise so the next load's abort block still awaits
    // the aborted future's unwind before calling loadDocx (&mut self) — the
    // fake's loadDocx throws the wasm-bindgen "recursive use of an object" if
    // that ordering is ever lost.
    const wasm = new SlowLayoutWasm();
    const v = makeViewer(wasm);
    const completes: number[] = [];
    v.on("loadComplete", (n) => completes.push(n as number));

    await v.load(DOCX);
    await settle();
    v.unload();

    await v.load(DOCX); // would reject if the unwind-await were skipped
    await settle();
    expect(await v.pageCount()).toBe(3);
    expect(await v.isLoadComplete()).toBe(true);
    expect(completes).toEqual([3]); // ONLY the re-show's total — no partials
  });
});

describe("unload() stale-ifies the in-flight image-decode tail", () => {
  let savedDoc: unknown;
  let origBlob: typeof globalThis.Blob;
  let origCreateImageBitmap: typeof globalThis.createImageBitmap;
  beforeEach(() => {
    savedDoc = (globalThis as Record<string, unknown>).document;
    (globalThis as Record<string, unknown>).document = {
      fonts: { add: vi.fn(), delete: vi.fn() },
    };
    origBlob = globalThis.Blob;
    origCreateImageBitmap = globalThis.createImageBitmap;
    globalThis.Blob = class {
      constructor(
        public parts: unknown[],
        public options?: { type?: string },
      ) {}
    } as unknown as typeof globalThis.Blob;
  });
  afterEach(() => {
    (globalThis as Record<string, unknown>).document = savedDoc;
    globalThis.Blob = origBlob;
    globalThis.createImageBitmap = origCreateImageBitmap;
  });

  it("a decode resolving AFTER unload() is dropped and closed — not repopulated", async () => {
    // #decodeImagesInBackground's stale() is generation-keyed; unload() must
    // bump #loadGeneration or a late createImageBitmap resolution re-fills
    // #imageBitmaps / re-emits imageReady for a document that was released.
    let resolveDecode: ((b: unknown) => void) | null = null;
    globalThis.createImageBitmap = (() =>
      new Promise((r) => {
        resolveDecode = r;
      })) as unknown as typeof globalThis.createImageBitmap;

    class ImageWasm extends FakeWasm {
      setImageBitmapCalls: string[] = [];
      override imageRefs(): Array<{ relId: string; mime: string; bytes: Uint8Array }> {
        return [{ relId: "rId1", mime: "image/png", bytes: new Uint8Array([1, 2, 3]) }];
      }
      override setImageBitmap(relId: string): void {
        this.setImageBitmapCalls.push(relId);
      }
    }
    const wasm = new ImageWasm();
    const v = makeViewer(wasm);
    const imageReadys: string[] = [];
    v.on("imageReady", (id) => imageReadys.push(id as string));

    await v.load(DOCX);
    await settle();
    expect(imageReadys).toEqual([]); // decode still parked in flight

    v.unload();
    const bitmap = { close: vi.fn() };
    resolveDecode!(bitmap);
    await settle();

    expect(imageReadys).toEqual([]); // late decode dropped…
    expect(bitmap.close).toHaveBeenCalled(); // …its GPU handle closed…
    expect(wasm.setImageBitmapCalls).toEqual([]); // …and never fed to the painter
  });
});
