/**
 * Worker-mode validation for the selection/copy Viewer wrappers. These seams
 * were first built for the IN-PROCESS backend (`#remote === null`, covered by
 * selection-wrapper.test.ts); this file validates + hardens the OFF-THREAD
 * `#remote` branch WITHOUT a live browser.
 *
 * A {@link MockRemoteTransport} implements the same {@link Transport} interface
 * the real {@link WorkerTransport} does, but routes `request`/`notify` to an
 * in-process {@link FakeEngine} (the wasm-export stand-in). It models the two
 * behaviors that make the worker boundary distinct from the in-process call:
 *
 *  1. **structured clone** — every arg is cloned IN and every return value is
 *     cloned OUT (`structuredClone`, exactly what `postMessage` does). A
 *     non-cloneable return (a function, a class instance, a Map with methods)
 *     would THROW here, so these tests double as the structured-clone gate: all
 *     of the wasm selection/copy exports build plain `Object`/`Array` trees of
 *     numbers + strings (verified), so they survive.
 *  2. **not awaited** — `worker.ts` does `value = fn.apply(v, args)` with NO
 *     `await`; the mock mirrors that (a sync export only). A Promise-typed
 *     return would serialize as garbage; the FakeEngine returns values directly.
 *
 * Plus FIFO ordering under a serial latency queue, and the image-read
 * two-branch: image reads return `null` in worker mode WITHOUT touching the
 * transport (the bytes live on the main thread), while text extraction routes
 * through `#remote` unconditionally.
 *
 * Viewer injection uses the DI seam `Viewer.withTransport(transport, localWasm)`
 * (the batteries-included `createWorkerBacked` spawns a real module Worker, which
 * has no wasm in vitest); the local `#wasm` is never touched on the `#remote`
 * branch, so a bare stand-in suffices.
 */
import { describe, it, expect, afterEach } from "vitest";
import {
  Viewer,
  type Transport,
  type TransportEvent,
  type DocFragment,
  type DocHit,
  type DocLoc,
  type DocSelection,
  type Rect,
  type TextPos,
} from "./index";
import { collectTransferables } from "./image-transfer";

// A minimal, structured-clone-safe DocFragment (matches the schema the wasm
// `extractRange` builds via `frag_to_jsvalue`).
const FRAG: DocFragment = {
  blocks: [
    {
      kind: "para",
      para: {
        runs: [
          {
            text: "Hello",
            bold: false,
            italic: false,
            underline: "none",
            strike: false,
            doubleStrike: false,
            fontFamily: "Calibri",
            fontSizePt: 11,
            color: "#000000",
            highlight: null,
            runShading: null,
            vertAlign: "baseline",
            link: null,
            image: null,
            footnoteMark: null,
          },
        ],
        alignment: "start",
        dir: "ltr",
        list: null,
      },
    },
  ],
} as unknown as DocFragment;

/**
 * The wasm-export stand-in the mock transport dispatches into by method name
 * (worker.ts's generic reflection: `fn = viewer[method]; fn.apply(v, args)`).
 * Every method returns a plain, structured-clone-safe JS value and records its
 * (post-clone) args so a test can assert the wire faithfully carried them.
 * `objectHitX` gates whether a coordinate reports an image object.
 */
class FakeEngine {
  readonly calls: Array<{ m: string; args: unknown[] }> = [];
  objectHitX: number | null = null;
  #rec(m: string, args: unknown[]): void {
    this.calls.push({ m, args });
  }
  countOf(m: string): number {
    return this.calls.filter((c) => c.m === m).length;
  }

  /** When true the fake models a PML doc: page-space hits carry a target. */
  pmlMode = false;

  docHitTest(loc: DocLoc, x: number, _y: number, g: string): DocHit {
    this.#rec("docHitTest", [loc, x, _y, g]);
    // A scrollful loc addresses the grid (the wasm SML arm) → a cell hit.
    if (loc.scrollX != null && loc.scrollY != null) {
      return { kind: "cell", surface: loc.surface, zone: "cell", row: 1, col: 2 };
    }
    if (this.pmlMode) {
      return {
        kind: "text",
        surface: loc.surface,
        target: { targetKind: 0, targetA: 1, targetB: 0 },
        pos: { para: 0, byte: 7 },
      };
    }
    const pos = { para: 0, byte: x };
    if (g === "word") {
      return { kind: "text", surface: loc.surface, pos, range: { start: { para: 0, byte: x - 1 }, end: { para: 0, byte: x + 2 } } };
    }
    return { kind: "text", surface: loc.surface, pos };
  }
  docSelectionRects(loc: DocLoc, sel: DocSelection): Rect[] {
    this.#rec("docSelectionRects", [loc, sel]);
    if (sel.kind !== "text") return [];
    return [{ x: sel.start.byte, y: 0, w: sel.end.byte - sel.start.byte, h: 12 }];
  }
  docExtractRange(sel: DocSelection): unknown {
    this.#rec("docExtractRange", [sel]);
    if (sel.kind === "cells") {
      return { rows: [{ cells: [{ ref: "A1", display: "x", raw: "x", rowspan: 1, colspan: 1, style: null }] }], truncated: false };
    }
    return FRAG;
  }
  docSetSelection(sel: DocSelection | null): void {
    this.#rec("docSetSelection", [sel]);
  }
  docHitTestObject(loc: DocLoc, x: number, _y: number): unknown {
    this.#rec("docHitTestObject", [loc, x, _y]);
    if (this.objectHitX !== null && x === this.objectHitX) {
      return { surface: loc.surface, kind: "inlineImage", key: "rIdImg", rect: { x: 0, y: 0, w: 50, h: 16 } };
    }
    return null;
  }
  docSetSelectedObject(obj: { surface: number; key: string } | null): void {
    this.#rec("docSetSelectedObject", [obj]);
  }
  gridContentBounds(sheet: number): unknown {
    this.#rec("gridContentBounds", [sheet]);
    return { r0: 0, c0: 0, r1: 3, c1: 3 };
  }
  gridPaneInfo(sheet: number): unknown {
    this.#rec("gridPaneInfo", [sheet]);
    // Frozen panes (§18.3.1.66) — a structured-clone-safe plain object riding
    // the generic dispatch (sheet 0 has a pane; others are pane-less → null).
    if (sheet !== 0) return null;
    return { xSplit: 1, ySplit: 2, bandW: 64, bandH: 38, scrollX: 0, scrollY: 120 };
  }
  // The chart copy model (structured-clone-safe, so it
  // DOES route in worker mode; the raster seam, by contrast, never dispatches).
  chartData(key: string): unknown {
    this.#rec("chartData", [key]);
    return { categories: ["Q1"], series: [{ name: "Alpha", values: [1] }], wPx: 430, hPx: 216 };
  }
  // The worker's wasm HAS the doc, so imageRefs returns the
  // real embedded bytes here (the main thread's local wasm is empty). FRESH
  // Uint8Arrays minted per call (each a distinct whole-buffer alloc) so a
  // transfer of one buffer never neuters a sibling AND a repeat call still has
  // live buffers to transfer (the prior call's were neutered on the way out).
  imageRefs(): unknown[] {
    this.#rec("imageRefs", []);
    return [
      { relId: "rIdImg", mime: "image/png", bytes: new Uint8Array([137, 80, 78, 71]) },
      { relId: "rIdImg2", mime: "image/jpeg", bytes: new Uint8Array([255, 216, 255]) },
    ];
  }
  // ── the minimal load surface (so a worker load() can drive #decodeImagesInBackground) ──
  abortLayout(): void {
    this.#rec("abortLayout", []);
  }

  /** The recycle decision's input; settable per scenario. */
  memoryBytesValue = 0;
  memoryBytes(): number {
    this.#rec("memoryBytes", []);
    return this.memoryBytesValue;
  }

  loadDocx(_b: Uint8Array): void {
    this.#rec("loadDocx", []);
  }
  setNow(_n: number): void {
    this.#rec("setNow", []);
  }
  pageSize(): { widthPx: number; heightPx: number } {
    return { widthPx: 612, heightPx: 792 };
  }
  fontFamiliesUsed(): string[] {
    return []; // empty → buildFontPlan makes no fetches; load resolves offline
  }
  // The Viewer's worker arm RPCs runLayout with [gen] (a number) — worker.ts
  // supplies the real per-page callback, which the mock does not model — so guard
  // against a non-function arg and just resolve the total page count.
  runLayout(cbOrGen: unknown): Promise<number> {
    if (typeof cbOrGen === "function") (cbOrGen as (idx: number) => void)(0);
    return Promise.resolve(1);
  }
  // Debug-probe font table: in worker mode the wasm font_cache lives
  // HERE — the main thread's local wasm is a bare stand-in — so the probe must
  // route over the generic dispatch. Plain object tree → structured-clone-safe.
  debugFonts(): unknown {
    this.#rec("debugFonts", []);
    return {
      aliases: [{ declared: "dejavu sans", effective: "verdana" }],
      faces: [],
      families: [
        { requested: "DejaVu Sans", alias: "verdana", resolvesNow: "verdana", fellBackToDefault: false },
      ],
    };
  }
}

type MockMode = "sync" | "serial" | "manual";

/**
 * A Transport that routes to an in-process FakeEngine. `structuredClone` on both
 * sides makes it a faithful stand-in for `postMessage` (and a structured-clone
 * gate). Requests honor the current {@link MockMode}: `sync` resolves in the
 * Promise executor; `serial` runs them through a single FIFO chain with a fixed
 * per-call latency (models the worker's serial dispatch); `manual` parks them in
 * {@link pending} for hand-ordered release (models a late/out-of-order reply).
 * Notifies always apply synchronously (fire-and-forget terminal writes).
 */
class MockRemoteTransport implements Transport {
  readonly log: Array<{ id: number; method: string }> = [];
  readonly executed: string[] = [];
  readonly resolvedOrder: string[] = [];
  readonly pending: Array<{ id: number; method: string; run: () => void }> = [];
  mode: MockMode;
  #seq = 0;
  #latencyMs: number;
  #chain: Promise<void> = Promise.resolve();
  #backend: Record<string, ((...a: unknown[]) => unknown) | undefined>;
  #eventHandlers = new Set<(e: TransportEvent) => void>();
  #deadHandlers = new Set<(e: Error) => void>();
  #dead = false;
  #disposed = false;

  constructor(backend: object, opts: { mode?: MockMode; latencyMs?: number } = {}) {
    this.#backend = backend as Record<string, ((...a: unknown[]) => unknown) | undefined>;
    this.mode = opts.mode ?? "sync";
    this.#latencyMs = opts.latencyMs ?? 0;
  }

  #clone<T>(v: T): T {
    return structuredClone(v);
  }

  #invoke(method: string, clonedArgs: unknown[]): unknown {
    const fn = this.#backend[method];
    if (typeof fn !== "function") throw new Error(`unknown worker method: ${method}`);
    // worker.ts does NOT await — sync wasm exports only.
    const value = fn.apply(this.#backend, clonedArgs);
    this.executed.push(method);
    // Clone OUT — postMessage back to main. For imageRefs, model the REAL wire:
    // the worker TRANSFERS the per-image buffers (worker.ts collectTransferables),
    // which NEUTERS the worker's own copy — a plain structuredClone(value) would
    // silently COPY and make the neuter/transfer behavior untestable (vacuous).
    if (method === "imageRefs") {
      return structuredClone(value, { transfer: collectTransferables("imageRefs", value) });
    }
    return this.#clone(value); // clone OUT — postMessage back to main
  }

  request(method: string, args: unknown[]): Promise<unknown> {
    if (this.#dead || this.#disposed) return Promise.reject(new Error("transport unavailable"));
    const id = ++this.#seq;
    this.log.push({ id, method });
    const clonedArgs = args.map((a) => this.#clone(a)); // clone IN — postMessage to worker
    return new Promise((resolve, reject) => {
      const run = (): void => {
        try {
          const v = this.#invoke(method, clonedArgs);
          this.resolvedOrder.push(method);
          resolve(v);
        } catch (e) {
          reject(e);
        }
      };
      if (this.mode === "manual") this.pending.push({ id, method, run });
      else if (this.mode === "serial") this.#chain = this.#chain.then(() => new Promise<void>((r) => setTimeout(() => { run(); r(); }, this.#latencyMs)));
      else run();
    });
  }

  notify(method: string, args: unknown[]): void {
    if (this.#dead || this.#disposed) return;
    this.log.push({ id: 0, method });
    const clonedArgs = args.map((a) => this.#clone(a));
    const fn = this.#backend[method];
    if (typeof fn === "function") {
      fn.apply(this.#backend, clonedArgs);
      this.executed.push(method);
    }
  }

  /** Resolve all parked (manual-mode) requests oldest-first (FIFO delivery). */
  flushInOrder(): void {
    while (this.pending.length) this.pending.shift()!.run();
  }
  /** Resolve all parked requests newest-first (worst-case out-of-order delivery). */
  flushReverse(): void {
    while (this.pending.length) this.pending.pop()!.run();
  }

  onEvent(h: (e: TransportEvent) => void): () => void {
    this.#eventHandlers.add(h);
    return () => this.#eventHandlers.delete(h);
  }
  onDead(h: (e: Error) => void): () => void {
    this.#deadHandlers.add(h);
    return () => this.#deadHandlers.delete(h);
  }
  dispose(): void {
    this.#disposed = true;
  }
  /** The worker died (crashed, or the stall watchdog ended it). */
  die(): void {
    this.#dead = true;
    for (const h of this.#deadHandlers) h(new Error("worker stalled"));
  }
}

const pos = (para: number, byte: number): TextPos => ({ para, byte });

// ── Image harness: node lacks createImageBitmap; stub it with a
//    fake bitmap carrying dims (objectInfo reads width/height) + a decode counter.
let bitmapDecodes = 0;
const origCreateImageBitmap = (globalThis as { createImageBitmap?: unknown }).createImageBitmap;
function installFakeDecode(): void {
  bitmapDecodes = 0;
  (globalThis as { createImageBitmap: (b: Blob) => Promise<unknown> }).createImageBitmap = async (blob: Blob) => {
    bitmapDecodes++;
    // Fake intrinsic dims by mime so objectInfo has deterministic wPx/hPx.
    const png = blob.type === "image/png";
    return { width: png ? 4 : 3, height: png ? 3 : 2, close() {} };
  };
}
afterEach(() => {
  if (origCreateImageBitmap === undefined) delete (globalThis as { createImageBitmap?: unknown }).createImageBitmap;
  else (globalThis as { createImageBitmap?: unknown }).createImageBitmap = origCreateImageBitmap;
});

// A ZIP header WITHOUT the xlsx (`xl/workbook.xml`) or pptx (`ppt/presentation.xml`)
// part-name needles → the load sniff classifies it as DOCX.
const docxBytes = (): Uint8Array => new Uint8Array([0x50, 0x4b, 0x03, 0x04, ...new TextEncoder().encode("word/document.xml")]);

function workerViewer(backend: FakeEngine, opts?: { mode?: MockMode; latencyMs?: number }): { viewer: Viewer; mock: MockRemoteTransport } {
  const mock = new MockRemoteTransport(backend, opts);
  // localWasm is unused on the #remote READ branch (a bare object suffices), but
  // destroy() calls #wasm.free() and the in-process settle-clear calls
  // clearImageBytes() — provide harmless no-op stubs so a worker-load test can
  // tear down cleanly (never touched on the pure-RPC read tests).
  const localWasm = { free() {}, clearImageBytes() {} };
  const viewer = Viewer.withTransport(mock, localWasm as never);
  return { viewer, mock };
}

describe("worker-mode selection/copy wrappers (#remote)", () => {
  it("hitTest routes through #remote, clones args in + result out, maps the shape", async () => {
    const be = new FakeEngine();
    const { viewer, mock } = workerViewer(be);

    const out = await viewer.hitTest({ surface: 2 }, 40, 88, "word");
    expect(mock.log).toEqual([{ id: 1, method: "docHitTest" }]);
    expect(be.calls).toEqual([{ m: "docHitTest", args: [{ surface: 2 }, 40, 88, "word"] }]);
    expect(out).toEqual({
      kind: "text",
      surface: 2,
      pos: { para: 0, byte: 40 },
      range: { start: { para: 0, byte: 39 }, end: { para: 0, byte: 42 } },
    });
  });

  it("worker hitTest result matches the in-process wrapper byte-for-byte (parity)", async () => {
    const beRemote = new FakeEngine();
    const beLocal = new FakeEngine();
    const { viewer: remote } = workerViewer(beRemote);
    // In-process viewer over the SAME FakeEngine shape (private-constructor cast,
    // the selection-wrapper.test idiom).
    const local = new (Viewer as unknown as { new (w: unknown): Viewer })(beLocal);

    const a = await remote.hitTest({ surface: 1 }, 12, 5, "char");
    const b = await local.hitTest({ surface: 1 }, 12, 5, "char");
    expect(a).toEqual(b);

    const sel: DocSelection = { kind: "text", start: pos(0, 2), end: pos(0, 9) };
    const ra = await remote.selectionRects({ surface: 3 }, sel);
    const rb = await local.selectionRects({ surface: 3 }, sel);
    expect(ra).toEqual(rb);
  });

  it("selectionRects / extractRange route + carry the DocSelection arg shape", async () => {
    const be = new FakeEngine();
    const { viewer, mock } = workerViewer(be);

    const sel: DocSelection = { kind: "text", start: pos(1, 5), end: pos(2, 9) };
    const rects = await viewer.selectionRects({ surface: 4 }, sel);
    expect(be.calls.at(-1)).toEqual({ m: "docSelectionRects", args: [{ surface: 4 }, sel] });
    expect(rects).toEqual([{ x: 5, y: 0, w: 4, h: 12 }]);

    const frag = await viewer.extractRange(sel);
    expect(be.calls.at(-1)).toEqual({ m: "docExtractRange", args: [sel] });
    expect(mock.log.map((l) => l.method)).toEqual(["docSelectionRects", "docExtractRange"]);
    expect(frag).toEqual(FRAG);
  });

  it("a DocFragment survives the structured-clone boundary (deep-equal, distinct ref)", async () => {
    const be = new FakeEngine();
    const { viewer } = workerViewer(be);

    const frag = await viewer.extractRange({ kind: "text", start: pos(0, 0), end: pos(0, 5) });
    // Deep-equal to the source…
    expect(frag).toEqual(FRAG);
    // …but a distinct object graph (it crossed a clone boundary, not shared).
    expect(frag).not.toBe(FRAG);
    expect(frag?.blocks).not.toBe(FRAG.blocks);
    // A structurally identical, mutation-independent copy.
    (frag as unknown as { blocks: unknown[] }).blocks.push({} as never);
    expect(FRAG.blocks.length).toBe(1);
  });

  it("setters route via notify (id 0) — text / clear / object / cells (ONE verb each)", () => {
    const be = new FakeEngine();
    const { viewer, mock } = workerViewer(be);

    const textSel: DocSelection = { kind: "text", start: pos(2, 3), end: pos(5, 7) };
    const cellsSel: DocSelection = { kind: "cells", sheet: 0, range: { r0: 1, c0: 1, r1: 4, c1: 4 }, anchor: { row: 1, col: 1 } };
    viewer.setSelection(textSel);
    viewer.setSelection(null); // the unified clear (no sentinel)
    viewer.setSelectedObject({ page: 3, kind: "anchoredImage", key: "rIdA", index: 0 });
    viewer.setSelectedObject(null);
    viewer.setSelection(cellsSel);
    viewer.setSelectedObject({ page: 1, kind: "picture", key: "rIdP", index: 0 });

    // Every setter is a fire-and-forget id-0 notify (no request ids minted).
    expect(mock.log.every((l) => l.id === 0)).toBe(true);
    expect(be.calls).toEqual([
      { m: "docSetSelection", args: [textSel] },
      { m: "docSetSelection", args: [null] },
      { m: "docSetSelectedObject", args: [{ surface: 3, key: "rIdA" }] },
      { m: "docSetSelectedObject", args: [null] },
      { m: "docSetSelection", args: [cellsSel] },
      { m: "docSetSelectedObject", args: [{ surface: 1, key: "rIdP" }] },
    ]);
  });

  it("SML + PML reads route through #remote with the right arg shape", async () => {
    const be = new FakeEngine();
    const { viewer } = workerViewer(be);

    // A scrollful loc addresses the grid content space (the SML arm).
    expect(await viewer.hitTest({ surface: 0, scrollX: 5, scrollY: 6 }, 30, 40)).toEqual({
      kind: "cell",
      surface: 0,
      zone: "cell",
      row: 1,
      col: 2,
    });
    expect(be.calls.at(-1)).toEqual({ m: "docHitTest", args: [{ surface: 0, scrollX: 5, scrollY: 6 }, 30, 40, "char"] });

    expect(await viewer.gridContentBounds(0)).toEqual({ r0: 0, c0: 0, r1: 3, c1: 3 });

    // Frozen panes (§18.3.1.66): the pane read rides the generic dispatch; a
    // pane-less sheet's null round-trips as null.
    expect(await viewer.gridPaneInfo(0)).toEqual({ xSplit: 1, ySplit: 2, bandW: 64, bandH: 38, scrollX: 0, scrollY: 120 });
    expect(be.calls.at(-1)).toEqual({ m: "gridPaneInfo", args: [0] });
    expect(await viewer.gridPaneInfo(1)).toBeNull();

    const cellsSel: DocSelection = { kind: "cells", sheet: 0, range: { r0: 1, c0: 1, r1: 2, c1: 2 } };
    const cf = await viewer.extractRange(cellsSel);
    expect(be.calls.at(-1)).toEqual({ m: "docExtractRange", args: [cellsSel] });
    expect(cf).toEqual({ rows: [{ cells: [{ ref: "A1", display: "x", raw: "x", rowspan: 1, colspan: 1, style: null }] }], truncated: false });

    // A PML doc's page-space hit carries the clamped target.
    be.pmlMode = true;
    const ph = await viewer.hitTest({ surface: 0 }, 10, 20);
    expect(ph).toEqual({ kind: "text", surface: 0, target: { targetKind: 0, targetA: 1, targetB: 0 }, pos: { para: 0, byte: 7 } });
    const pr = await viewer.extractRange({
      kind: "pmlText",
      slide: 0,
      target: { targetKind: 0, targetA: 1, targetB: 0 },
      start: pos(0, 0),
      end: pos(0, 3),
    });
    expect(pr).toEqual(FRAG);
  });

  // ── debugFonts probe routes over RPC ────────────────────────────────────────
  it("debugFonts routes through #remote (the local wasm's font cache is empty in worker mode)", async () => {
    const be = new FakeEngine();
    const { viewer, mock } = workerViewer(be);

    const dbg = (await viewer.debugFonts()) as { useLocalFonts: boolean; fonts: unknown[] };
    // The probe MUST come from the worker's wasm, not the bare local stand-in.
    expect(mock.log).toEqual([{ id: 1, method: "debugFonts" }]);
    expect(be.calls.at(-1)).toEqual({ m: "debugFonts", args: [] });
    // No document loaded through this viewer → no declared families to merge,
    // but the probe surface itself is present (an object, not undefined).
    expect(dbg).toMatchObject({ useLocalFonts: false, fonts: [] });
  });

  it("debugFonts degrades to undefined on a stale worker bundle (no export → dispatch throws)", async () => {
    // A backend WITHOUT debugFonts models a probe-OFF / older worker
    // bundle: the generic dispatch throws "unknown worker method".
    const be = { free: () => undefined } as unknown as FakeEngine;
    const { viewer } = workerViewer(be);
    await expect(viewer.debugFonts()).resolves.toBeUndefined();
  });

  // ── image reads clean-skip BEFORE a load; serve AFTER ───────────────────────
  it("no-doc: image reads return null WITHOUT hitting the transport; text still routes", async () => {
    const be = new FakeEngine();
    const { viewer, mock } = workerViewer(be);

    // With no doc loaded, #decodeImagesInBackground never ran, so the main-thread
    // maps (#imageBytes/#imageBitmaps) are empty → null, no throw, no RPC. (The
    // reads themselves are map lookups, never RPCs — that is unchanged by the imageRefs transfer;
    // only a LOADED worker doc now populates those maps via the imageRefs RPC.)
    expect(await viewer.imageOriginal("rId5")).toBeNull();
    expect(await viewer.imageBitmapFor("rId5")).toBeNull();
    expect(await viewer.objectInfo({ page: 0, kind: "inlineImage", key: "rId5", index: 0 })).toBeNull();

    // …and NOT one of those became an RPC (no imageOriginal/imageBitmapFor/objectInfo
    // request/notify was minted — they short-circuit before the #remote branch).
    expect(mock.log).toEqual([]);
    expect(be.calls).toEqual([]);

    // Text extraction, by contrast, DOES route through the worker unconditionally.
    const frag = await viewer.extractRange({ kind: "text", start: pos(0, 0), end: pos(0, 5) });
    expect(frag).toEqual(FRAG);
    expect(mock.log).toEqual([{ id: 1, method: "docExtractRange" }]);
  });

  // ── a loaded worker doc decodes real images into the cache ─────────────────
  it("worker load: imageRefs RPC → decode-into-cache → imageReady → imageOriginal/objectInfo serve REAL data", async () => {
    installFakeDecode();
    const be = new FakeEngine();
    const { viewer, mock } = workerViewer(be);

    // Wait for BOTH images to decode (imageReady fires once per relId).
    const seen: string[] = [];
    const bothReady = new Promise<void>((resolve) => {
      viewer.on("imageReady", (relId) => {
        seen.push(relId);
        if (seen.length === 2) resolve();
      });
    });

    // A ZIP header WITHOUT the xlsx/pptx needles sniffs as DOCX → loadDocx routes
    // over the worker; #decodeImagesInBackground then RPCs imageRefs.
    const info = await viewer.load(docxBytes());
    expect(info.documentKind).toBe("word");
    expect(mock.log.map((l) => l.method)).toContain("imageRefs"); // the RPC fired

    await bothReady;
    expect(seen.sort()).toEqual(["rIdImg", "rIdImg2"]);

    // The maps are now populated FROM THE WORKER BYTES → the reads serve REAL data
    // (before the imageRefs transfer these returned null in worker mode).
    const orig = await viewer.imageOriginal("rIdImg");
    expect(orig?.mime).toBe("image/png");
    expect(Array.from(orig!.bytes)).toEqual([137, 80, 78, 71]);

    const oi = await viewer.objectInfo({ page: 0, kind: "inlineImage", key: "rIdImg2", index: 0 });
    expect(oi).toEqual({ relId: "rIdImg2", mime: "image/jpeg", wPx: 3, hPx: 2 });

    const bmp = await viewer.imageBitmapFor("rIdImg");
    expect(bmp).not.toBeNull();
  });

  it("worker load: destroy mid-flight drops the late imageRefs ingest (post-await stale guard)", async () => {
    installFakeDecode();
    const be = new FakeEngine();
    const { viewer } = workerViewer(be);

    let readyFired = 0;
    viewer.on("imageReady", () => {
      readyFired++;
    });

    // Kick load, then destroy BEFORE the imageRefs RPC settles: the ingest's
    // post-await guard (`#destroyed || #loadGeneration !== gen`) must drop it, so
    // NO decode runs and NO imageReady fires into a torn-down viewer.
    const load1 = viewer.load(docxBytes()).catch(() => {}); // may reject post-destroy
    viewer.destroy();
    await load1;
    // Let any (incorrectly) surviving RPC/decode microtasks run.
    await new Promise((r) => setTimeout(r, 0));

    expect(readyFired).toBe(0); // RED: a missing stale guard would ingest + emit
    expect(bitmapDecodes).toBe(0); // RED: a dropped guard would decode into a dead viewer
  });

  it("the SyncInProcess escape hatch returns null in worker mode for EVERY kind", () => {
    const be = new FakeEngine();
    const { viewer, mock } = workerViewer(be);
    expect(viewer.extractRangeSyncInProcess({ kind: "text", start: pos(0, 0), end: pos(0, 4) })).toBeNull();
    expect(viewer.extractRangeSyncInProcess({ kind: "cells", sheet: 0, range: { r0: 0, c0: 0, r1: 1, c1: 1 } })).toBeNull();
    expect(
      viewer.extractRangeSyncInProcess({
        kind: "pmlText",
        slide: 0,
        target: { targetKind: 0, targetA: 0, targetB: 0 },
        start: pos(0, 0),
        end: pos(0, 1),
      }),
    ).toBeNull();
    // None of these touched the transport (worker mode has no synchronous read).
    expect(mock.log).toEqual([]);
  });

  // ── FIFO ordering under simulated latency ─────────────────────────────────
  it("FIFO: serial-latency requests execute AND resolve in request order", async () => {
    const be = new FakeEngine();
    const { viewer, mock } = workerViewer(be, { mode: "serial", latencyMs: 5 });

    // Fire four reads without awaiting — they must run + settle oldest-first.
    const ps = [
      viewer.hitTest({ surface: 0 }, 1, 0, "char"),
      viewer.hitTest({ surface: 0 }, 2, 0, "char"),
      viewer.selectionRects({ surface: 0 }, { kind: "text", start: pos(0, 0), end: pos(0, 1) }),
      viewer.extractRange({ kind: "text", start: pos(0, 0), end: pos(0, 2) }),
    ];
    await Promise.all(ps);

    const order = ["docHitTest", "docHitTest", "docSelectionRects", "docExtractRange"];
    expect(mock.log.map((l) => l.method)).toEqual(order); // enqueued FIFO
    expect(mock.executed).toEqual(order); // executed FIFO (serial chain)
    expect(mock.resolvedOrder).toEqual(order); // resolved FIFO
  });
});

describe("worker recycle on document switch", () => {
  const localStub = { free() {}, clearImageBytes() {} };

  it("a lean worker is KEPT; a bloated one is disposed and rebuilt via the recreate recipe", async () => {
    const be1 = new FakeEngine();
    const mock1 = new MockRemoteTransport(be1);
    const be2 = new FakeEngine();
    const mock2 = new MockRemoteTransport(be2);
    let recreated = 0;
    let disposed1 = false;
    const origDispose = mock1.dispose.bind(mock1);
    mock1.dispose = () => {
      disposed1 = true;
      origDispose();
    };
    const viewer = Viewer.withTransport(mock1, localStub as never, async () => {
      recreated++;
      return mock2;
    });

    // Below the 1 GiB threshold: the warm worker wins — no recycle.
    be1.memoryBytesValue = 100 * 1024 * 1024;
    await viewer.load(docxBytes());
    expect(recreated).toBe(0);
    expect(disposed1).toBe(false);
    expect(be1.countOf("loadDocx")).toBe(1);

    // Peak grew past the threshold: the NEXT switch replaces the worker BEFORE
    // dispatch, so the new engine takes the load and the old one is dead.
    be1.memoryBytesValue = 2 * 1024 * 1024 * 1024;
    await viewer.load(docxBytes());
    expect(recreated).toBe(1);
    expect(disposed1).toBe(true);
    expect(be2.countOf("loadDocx")).toBe(1);
    expect(be1.countOf("loadDocx")).toBe(1); // old worker got nothing more
    viewer.destroy();
  });

  it("an injected transport WITHOUT a recipe never recycles, however bloated", async () => {
    const be = new FakeEngine();
    const mock = new MockRemoteTransport(be);
    let disposed = false;
    const origDispose = mock.dispose.bind(mock);
    mock.dispose = () => {
      disposed = true;
      origDispose();
    };
    const viewer = Viewer.withTransport(mock, localStub as never); // no recreate
    be.memoryBytesValue = 3 * 1024 * 1024 * 1024;
    await viewer.load(docxBytes());
    // RED if #maybeRecycleWorker ignored the null recipe: the transport would
    // be disposed with no way to rebuild it, bricking the viewer.
    expect(disposed).toBe(false);
    expect(be.countOf("loadDocx")).toBe(1);
    viewer.destroy();
  });

  it("a worker that dies is replaced by a fresh one at the next load", async () => {
    const be1 = new FakeEngine();
    const mock1 = new MockRemoteTransport(be1);
    const be2 = new FakeEngine();
    const mock2 = new MockRemoteTransport(be2);
    let recreated = 0;
    const viewer = Viewer.withTransport(mock1, localStub as never, async () => {
      recreated++;
      return mock2;
    });
    await viewer.load(docxBytes());
    mock1.die();
    expect(recreated).toBe(0); // the replacement boots lazily, inside the next load
    // RED if death fell back to in-process for good: localStub has no loadDocx,
    // so the load would run (and fail) on the main thread instead of mock2.
    await viewer.load(docxBytes());
    expect(recreated).toBe(1);
    expect(be2.countOf("loadDocx")).toBe(1);
    expect(be1.countOf("loadDocx")).toBe(1);
    viewer.destroy();
  });

  it("a failed recreate falls back to in-process instead of bricking the load", async () => {
    const be = new FakeEngine();
    const mock = new MockRemoteTransport(be);
    const viewer = Viewer.withTransport(mock, localStub as never, async () => {
      throw new Error("boot refused");
    });
    be.memoryBytesValue = 2 * 1024 * 1024 * 1024;
    // The load itself must still resolve — dispatched against the in-process
    // arm after the fallback. localStub lacks the wasm surface, so just assert
    // the promise SETTLES rather than hangs (rejection = in-process dispatch
    // reached a stub method; a hang = the chain died inside the recycle).
    await expect(viewer.load(docxBytes()).catch(() => "settled")).resolves.toBe("settled");
    viewer.destroy();
  });
});

