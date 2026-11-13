/**
 * The EngineFacade two-tier conformance + capability + fallback
 * suite (the in-process/worker duality collapse). Complements
 * worker-remote.test.ts (which validates the RPC branch of the Viewer wrappers)
 * by testing the FACADE itself: that InProcessEngine and RemoteEngine satisfy the
 * SAME tier-1 interface with equivalent results, that capabilities are detected
 * correctly, that the tier-2 fallbacks compose (renderPage→replay, chart→false),
 * and that a genuinely-unservable method throws the LOUD EngineUnsupportedError.
 *
 * Each test states its RED condition (what a bad collapse trips).
 */
import { describe, it, expect, vi } from "vitest";
import { Viewer, type Transport, type TransportEvent } from "./index";
import { InProcessEngine, RemoteEngine } from "./engine";

// ── A wasm-export stand-in exposing the tier-1 methods the facade collapses ──
class FakeWasm {
  readonly calls: string[] = [];
  #rec(m: string) {
    this.calls.push(m);
  }
  // loads / layout / time
  loadDocx(_b: Uint8Array): void {
    this.#rec("loadDocx");
  }
  loadXlsx(_b: Uint8Array): void {
    this.#rec("loadXlsx");
  }
  loadPptx(_b: Uint8Array): void {
    this.#rec("loadPptx");
  }
  setNow(_n: number): void {
    this.#rec("setNow");
  }
  pageSize(): { widthPx: number; heightPx: number } {
    return { widthPx: 612, heightPx: 792 };
  }
  abortLayout(): void {
    this.#rec("abortLayout");
  }
  // wasm-bindgen returns undefined for Option::None; the facade must
  // normalize to null in BOTH arms (in-process reads it directly, remote via
  // #tryReq's ?? fallback).
  layoutTruncatedAt(): number | undefined {
    this.#rec("layoutTruncatedAt");
    return undefined;
  }
  runLayout(cb: (idx: number) => void): Promise<number> {
    cb(0);
    return Promise.resolve(1);
  }
  renderPageDisplayList(_idx: number): { opcodes: Float64Array; strings: string[] } {
    this.#rec("renderPageDisplayList");
    return { opcodes: new Float64Array(0), strings: [] };
  }
  recordGridViewport(): { opcodes: Float64Array; strings: string[] } {
    this.#rec("recordGridViewport");
    return { opcodes: new Float64Array(0), strings: [] };
  }
  // latent reads (the ones the worker path used to read empty locally)
  linkRects(idx: number): Array<{ x: number; y: number; w: number; h: number; target: string }> {
    this.#rec("linkRects");
    return [{ x: idx, y: 0, w: 10, h: 10, target: "url:https://x" }];
  }
  bookmarkPosition(name: string): { page: number; y: number } | null {
    this.#rec("bookmarkPosition");
    return name === "top" ? { page: 0, y: 0 } : null;
  }
  comments(): Array<unknown> {
    this.#rec("comments");
    return [{ id: 1, author: "A", date: null, initials: null, text: "hi", page: 0, anchorY: 0, color: "#fff" }];
  }
  commentRects(_i: number): Array<unknown> {
    this.#rec("commentRects");
    return [];
  }
  cellAt(): null {
    return null;
  }
  sheetInfo(): Array<unknown> {
    return [];
  }
  // read (sync — worker.ts does `value = fn.apply(v, args)` with NO await, so the
  // wasm export is sync; the Viewer/InProcessEngine `await` is defensive).
  docHitTest(loc: { surface: number }, x: number): unknown {
    this.#rec("docHitTest");
    return { kind: "text", surface: loc.surface, pos: { para: loc.surface, byte: x } };
  }
  // notify
  docSetSelection(_sel: unknown): void {
    this.#rec("docSetSelection");
  }
  // font
  fontFamiliesUsed(): string[] {
    return ["Calibri"];
  }
  addFontAlias(_a: string, _b: string): void {}
  loadFont(): void {}
  // tier-2
  paintGridViewport(): void {
    this.#rec("paintGridViewport");
  }
  renderChartToCanvas(): boolean {
    this.#rec("renderChartToCanvas");
    return true;
  }
  setImageBitmap(): void {}
  // A sentinel image-refs payload so the facade conformance
  // test can assert verbatim forwarding (InProcess) + RPC round-trip (Remote).
  readonly imageRefsSentinel = [{ relId: "rId1", mime: "image/png", bytes: new Uint8Array([1, 2, 3, 4]) }];
  imageRefs(): unknown[] {
    return this.imageRefsSentinel;
  }
  clearImageBytes(): void {}
  free(): void {}
}

/** A Transport that routes request/notify to a backend via structuredClone. */
class MockTransport implements Transport {
  readonly requests: string[] = [];
  readonly notifies: string[] = [];
  #backend: Record<string, ((...a: unknown[]) => unknown) | undefined>;
  #evt = new Set<(e: TransportEvent) => void>();
  #dead = new Set<(e: Error) => void>();
  constructor(backend: object) {
    this.#backend = backend as Record<string, ((...a: unknown[]) => unknown) | undefined>;
  }
  request(method: string, args: unknown[]): Promise<unknown> {
    this.requests.push(method);
    const fn = this.#backend[method];
    if (typeof fn !== "function") return Promise.reject(new Error(`unknown worker method: ${method}`));
    if (method === "runLayout") {
      // stream one pageReady then resolve, mirroring worker.ts
      for (const h of this.#evt) h({ event: "pageReady", gen: args[0] as number, payload: 0 });
    }
    const value = fn.apply(this.#backend, args.map((a) => structuredClone(a)));
    return Promise.resolve(structuredClone(value));
  }
  notify(method: string, args: unknown[]): void {
    this.notifies.push(method);
    const fn = this.#backend[method];
    if (typeof fn === "function") fn.apply(this.#backend, args.map((a) => structuredClone(a)));
  }
  onEvent(h: (e: TransportEvent) => void): () => void {
    this.#evt.add(h);
    return () => this.#evt.delete(h);
  }
  onDead(h: (e: Error) => void): () => void {
    this.#dead.add(h);
    return () => this.#dead.delete(h);
  }
  dispose(): void {}
}

const inProcViewer = (w: FakeWasm): Viewer => new (Viewer as unknown as { new (w: unknown): Viewer })(w);
// The local #wasm is the main-thread replay/decode-side helper even in worker
// mode (createWorkerBacked constructs a real WasmViewer for it); a bare stand-in
// suffices for pure-RPC reads, but load() runs #decodeImagesInBackground against
// it, so pass a FakeWasm (imageRefs/clearImageBytes present) for load tests.
const workerViewer = (t: Transport, localWasm: object = {}): Viewer =>
  Viewer.withTransport(t, localWasm as never);

// A canvas stub whose getContext returns a no-op 2D context (empty display list
// replays to nothing — the fallback path must not throw).
function fakeCanvas(): HTMLCanvasElement {
  const ctx = new Proxy({}, { get: () => () => undefined }) as unknown as CanvasRenderingContext2D;
  return { getContext: () => ctx } as unknown as HTMLCanvasElement;
}

describe("layoutTruncatedAt normalization", () => {
  it("undefined (wasm None) becomes null through both engine arms", async () => {
    const inproc = new InProcessEngine(new FakeWasm() as never);
    const remote = new RemoteEngine(new MockTransport(new FakeWasm()));
    expect(await inproc.layoutTruncatedAt()).toBeNull();
    expect(await remote.layoutTruncatedAt()).toBeNull();
    // RED: a raw forward would surface undefined, and a consumer's
    // `=== null` "complete" check would misread a truncated layout.
  });
});

describe("capability detection", () => {
  it("InProcessEngine advertises all three capabilities; RemoteEngine none", () => {
    const inproc = new InProcessEngine(new FakeWasm() as never);
    const remote = new RemoteEngine(new MockTransport(new FakeWasm()));
    expect(inproc.capabilities).toEqual({ localPaint: true, localChartRaster: true, inProcessDoc: true });
    expect(remote.capabilities).toEqual({ localPaint: false, localChartRaster: false, inProcessDoc: false });
    // RED: if Remote advertised localPaint it would take the fast path and crash
    // on a non-transferable ctx; if InProcess lacked it, it would needlessly replay.
  });
});

describe("tier-1 interface conformance (both impls satisfy the facade)", () => {
  it("linkRects / hitTest resolve equivalently through InProcess and Remote", async () => {
    const beLocal = new FakeWasm();
    const beRemote = new FakeWasm();
    const inproc = new InProcessEngine(beLocal as never);
    const remote = new RemoteEngine(new MockTransport(beRemote));

    expect(await inproc.linkRects(3)).toEqual(await remote.linkRects(3));
    expect(await inproc.docHitTest({ surface: 1 }, 7, 0, "char")).toEqual(
      await remote.docHitTest({ surface: 1 }, 7, 0, "char"),
    );
    expect(await inproc.bookmarkPosition("top")).toEqual(await remote.bookmarkPosition("top"));
    // RED: a dropped/inverted branch makes Remote return a different shape.
  });

  it("imageRefs — InProcess forwards the local wasm array verbatim; Remote round-trips it", async () => {
    const beLocal = new FakeWasm();
    const beRemote = new FakeWasm();
    const inproc = new InProcessEngine(beLocal as never);
    const remote = new RemoteEngine(new MockTransport(beRemote));

    // InProcess forwards the SAME array the local wasm returns (verbatim, ===).
    const local = inproc.imageRefs();
    expect(local).toBe(beLocal.imageRefsSentinel); // RED: a copy/wrap breaks byte-identity
    expect(Array.isArray(local) ? local[0] : null).toEqual({ relId: "rId1", mime: "image/png", bytes: new Uint8Array([1, 2, 3, 4]) });

    // Remote round-trips relId/mime/bytes through the clone boundary (RPC).
    const viaRpc = await remote.imageRefs();
    expect(viaRpc).toEqual(local); // RED before RemoteEngine.imageRefs exists → "not a function"
    expect(viaRpc).not.toBe(local); // crossed a clone boundary — distinct graph
  });
});

describe("mode-conformance (Viewer in-process vs worker, tier-1 equivalence)", () => {
  it("linkRects / hitTest are equal in both modes (the latent-read correctness gain)", async () => {
    const local = inProcViewer(new FakeWasm());
    const worker = workerViewer(new MockTransport(new FakeWasm()));

    expect(await worker.linkRects(2)).toEqual(await local.linkRects(2));
    expect(await worker.hitTest({ surface: 1 }, 5, 0, "char")).toEqual(
      await local.hitTest({ surface: 1 }, 5, 0, "char"),
    );
    // RED (the silent-empty bug): if the worker read the empty LOCAL wasm instead
    // of RPCing, worker.linkRects would be [] while in-process returns data.
  });

  it("worker linkRects ROUTES over the transport (not a local read)", async () => {
    const t = new MockTransport(new FakeWasm());
    const worker = workerViewer(t);
    await worker.linkRects(0);
    expect(t.requests).toContain("linkRects"); // RED: bypass persists → no RPC
  });
});

describe("worker-mode grid paint (record + replay, NOT unsupported)", () => {
  it("worker paintGridViewport RESOLVES and ROUTES recordGridViewport over the transport", async () => {
    const t = new MockTransport(new FakeWasm());
    const worker = workerViewer(t);
    // Was a LOUD EngineUnsupportedError throw (no grid record primitive); worker
    // mode now uses the compose-fallback (record off-thread + replay), mirroring
    // renderPage. It must resolve and drive the record RPC per frame.
    await expect(worker.paintGridViewport(0, fakeCanvas(), 0, 0, 100, 100, 1)).resolves.toBeUndefined();
    expect(t.requests).toContain("recordGridViewport");
    // RED: reverting to the throw, or bypassing the RPC (no off-thread record).
  });

  it("in-process paintGridViewport does NOT throw and takes the local-paint fast path (NO record)", async () => {
    const w = new FakeWasm();
    const local = inProcViewer(w);
    await expect(local.paintGridViewport(0, fakeCanvas(), 0, 0, 100, 100, 1)).resolves.toBeUndefined();
    expect(w.calls).toContain("paintGridViewport"); // direct-to-canvas fast path
    expect(w.calls).not.toContain("recordGridViewport"); // never records in-process
  });

  it("recordGridViewport round-trips both arms; a non-SML wasm returns an empty list", async () => {
    // InProcess arm forwards to the wasm export.
    const w = new FakeWasm();
    const inproc = new InProcessEngine(w as never);
    expect((await inproc.recordGridViewport(0, 0, 0, 100, 100, 1)).opcodes).toBeInstanceOf(Float64Array);
    expect(w.calls).toContain("recordGridViewport");
    // Remote arm RPCs the worker.
    const t = new MockTransport(new FakeWasm());
    const remote = new RemoteEngine(t as never);
    await remote.recordGridViewport(0, 0, 0, 100, 100, 1);
    expect(t.requests).toContain("recordGridViewport");
    // typeof-guard: a docx/pptx-only wasm (no recordGridViewport export) yields
    // an empty list rather than throwing a conformance failure.
    const bare = new InProcessEngine({} as never);
    const enc = await bare.recordGridViewport(0, 0, 0, 100, 100, 1);
    expect(enc.opcodes.length).toBe(0);
    expect(enc.strings).toEqual([]);
  });
});

describe("tier-2 transparent fallback", () => {
  it("worker renderPage does NOT throw and drives recordPage + replay", async () => {
    const t = new MockTransport(new FakeWasm());
    const worker = workerViewer(t);
    await expect(worker.renderPage(0, fakeCanvas())).resolves.toBeUndefined();
    // The fallback recorded the page as a display list over the transport.
    expect(t.requests).toContain("renderPageDisplayList");
    // RED: renderPage throwing/blanking in worker mode without invoking replay.
  });

  it("worker renderChartToCanvas returns false (degraded clean-skip, not a throw)", async () => {
    const worker = workerViewer(new MockTransport(new FakeWasm()));
    const ctx = new Proxy({}, { get: () => () => undefined }) as unknown as CanvasRenderingContext2D;
    await expect(worker.renderChartToCanvas("chart::rId1", ctx, 10, 10, 4)).resolves.toBe(false);
    // RED: throwing or attempting to ship a live ctx over RPC.
  });

  it("in-process renderPage uses the local-paint fast path (no display-list record)", async () => {
    const w = new FakeWasm();
    const local = inProcViewer(w);
    // FakeWasm has no renderPage → the fast path calls it; add a spy method.
    (w as unknown as { renderPage: () => void }).renderPage = () => w.calls.push("renderPage");
    await local.renderPage(0, fakeCanvas());
    expect(w.calls).toContain("renderPage");
    expect(w.calls).not.toContain("renderPageDisplayList"); // RED: needless replay in-process
  });
});

describe("EMF raster cache — rasterizeEmf facade conformance", () => {
  it("InProcess returns null when the wasm export is absent (feature-off / stale wasm build)", async () => {
    const inproc = new InProcessEngine(new FakeWasm() as never);
    await expect(inproc.rasterizeEmf("emf-pic:rId7", 100, 50)).resolves.toBeNull();
    // RED: throwing (or fabricating a bitmap) instead of the typeof-guard null
    // degrade — the caller keeps the placeholder, never crashes.
  });

  it("InProcess returns null when renderEmfToCanvas reports false; the bitmap only on true", async () => {
    const sentinel = { width: 100, height: 50 } as unknown as ImageBitmap;
    class FakeOffscreenCanvas {
      getContext(): unknown {
        return new Proxy({}, { get: () => () => undefined });
      }
      transferToImageBitmap(): ImageBitmap {
        return sentinel;
      }
    }
    vi.stubGlobal("OffscreenCanvas", FakeOffscreenCanvas);
    try {
      const w = new FakeWasm() as unknown as Record<string, unknown>;
      // `false` = unknown key (doc switched mid-flight) / degenerate dims.
      w.renderEmfToCanvas = () => false;
      await expect(new InProcessEngine(w as never).rasterizeEmf("emf-pic:gone", 100, 50)).resolves.toBeNull();
      // RED: handing back the fully-TRANSPARENT canvas — registering it would
      // make the EMF vanish and the policy would never re-raster at that size.
      w.renderEmfToCanvas = () => true;
      await expect(new InProcessEngine(w as never).rasterizeEmf("emf-pic:rId7", 100, 50)).resolves.toBe(sentinel);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("Remote issues the 'rasterizeEmf' RPC and returns the (Transferable) result", async () => {
    const be = new FakeWasm() as unknown as Record<string, unknown>;
    be.rasterizeEmf = (_key: string, pxW: number, pxH: number) => ({ width: pxW, height: pxH });
    const t = new MockTransport(be as object);
    const remote = new RemoteEngine(t);
    const bm = await remote.rasterizeEmf("emf-pic:rId7", 120, 60);
    expect(t.requests).toContain("rasterizeEmf"); // RED: a local read / no RPC
    expect(bm).toEqual({ width: 120, height: 60 });
  });

  it("Remote degrades to null on a STALE worker bundle (unknown-method rejection)", async () => {
    // FakeWasm backend has no rasterizeEmf → MockTransport rejects like the
    // real worker's generic dispatcher would on an old bundle.
    const remote = new RemoteEngine(new MockTransport(new FakeWasm()));
    await expect(remote.rasterizeEmf("emf-pic:rId7", 100, 50)).resolves.toBeNull();
    // RED: the rejection propagating — the Viewer's raster executor would
    // reject unhandled instead of keeping the placeholder.
  });
});

describe("load routing (xlsx/pptx worker-ready, kills the bypass)", () => {
  it("worker load(xlsx) routes loadXlsx over the transport, not the local wasm", async () => {
    const t = new MockTransport(new FakeWasm());
    const worker = workerViewer(t, new FakeWasm());
    // xlsx sniff: a ZIP header + the xl/workbook.xml part-name needle.
    const bytes = new Uint8Array([0x50, 0x4b, 0x03, 0x04, ...new TextEncoder().encode("xl/workbook.xml")]);
    const info = await worker.load(bytes);
    expect(info.documentKind).toBe("excel");
    expect(t.requests).toContain("loadXlsx"); // RED: the #wasm.loadXlsx bypass persists
  });
});
