/**
 * @parasma/core — the internal EngineFacade.
 *
 * The Viewer used to branch `if (this.#remote) <RPC> else <local #wasm>` at ~40
 * call sites (the in-process/worker "duality god-object"). This module collapses that
 * behind ONE internal facade so the Viewer calls `this.#engine.m(...)` once, and
 * TWO impls satisfy it:
 *
 *  - {@link InProcessEngine} wraps the local `WasmViewer` — every method body is
 *    the EXACT prior LOCAL branch (same `typeof` stale guards, same `?? null`).
 *  - {@link RemoteEngine} wraps a {@link Transport} — every method body is the
 *    EXACT prior RPC branch.
 *
 * TWO-TIER MODEL:
 *  - TIER 1 {@link EngineFacade} — always equivalent in-process AND worker
 *    (never silently empty in worker mode). Both impls satisfy it.
 *  - TIER 2 {@link LocalPaintEngine} — a SEPARATE, capability-DETECTED capability
 *    (direct-to-canvas paint needing a live non-transferable ctx/ImageBitmap).
 *    InProcess advertises it; Remote does NOT. The Viewer composes the fallback.
 *
 * INTERNAL to @parasma/core — the facade interfaces/classes are NOT re-exported
 * past the package. The SOLE public export is {@link EngineUnsupportedError}
 * (consumers need `instanceof` for the NOT-SUPPORTED policy).
 *
 * The Viewer OWNS all post-processing (`?? null` coalesce that stays at the call
 * site, result `.map`, generation guard, `stale()`/`destroyed` checks, the
 * `#documentKind` short-circuits) — the facade returns the RAW dual-branch value.
 */

import type { Viewer as WasmViewerType } from "./wasm/parasma_wasm.js";
import type { Transport, TransportEvent } from "./transport.js";
import type { EncodedDisplayList } from "./replay.js";
import type {
  PageSize,
  LinkRect,
  BookmarkPosition,
  Comment,
  CommentRect,
  SearchMatch,
  SearchOpts,
  Rect,
  ChartCopyData,
  DocFragment,
  CellFragment,
  GridPaneInfo,
  DocLoc,
  DocSelection,
  DocHit,
  DocObjectHit,
} from "./index.js";

/**
 * An embedded-image reference the engine extracted from the current document:
 * `relId` + `mime` + the RAW `bytes` (a detached JS-owned copy — wasm
 * `image_refs()` does `new_with_length` + `copy_from`, one FRESH whole-buffer
 * `Uint8Array` per image). In worker mode these ride the RPC response as
 * Transferable ArrayBuffers (see {@link ./image-transfer.js}). Shared with the
 * Viewer's `#decodeImagesInBackground` (main-thread `createImageBitmap` decode).
 */
export type ImageRef = {
  relId: string;
  mime: string;
  bytes: Uint8Array;
  /**
   * Intrinsic pixel dimensions, present ONLY for the raw-RGBA EMF-DIB variant
   * (mime `application/x-emf-dib-rgba`, emitted by the wasm `emf` feature). Raw
   * RGBA has no container header, so `createImageBitmap` needs the size to build
   * an `ImageData`. Absent for every normal encoded-image ref.
   */
  wPx?: number;
  hPx?: number;
};

/** The wasm debug-probe font table (Viewer merges it with its capture maps). */
export type WasmFontTable = {
  faces?: Array<{
    family: string;
    weight: number;
    italic: boolean;
    unitsPerEm: number;
    ascentEm: number;
    descentEm: number;
    lineGapEm: number;
  }>;
  families?: Array<{
    requested: string;
    alias: string | null;
    resolvesNow: string;
    fellBackToDefault: boolean;
  }>;
};

/** The raw sheet-info row (Viewer applies the `maxRow ?? 0` normalization). */
type RawSheetInfo = { name: string; firstPage: number; pageCount: number; maxRow?: number };
type CellReadout = { ref: string; raw: string; display: string };
type GridExtent = { width: number; height: number };
type SearchRectResolved = { page: number; x: number; y: number; w: number; h: number };

/**
 * The NOT-SUPPORTED policy — the LOUD failure for anything a partial impl
 * genuinely cannot serve. Public (`@public`) so a consumer can `instanceof` it.
 * The ONLY site that throws this today is {@link paintGridViewport} in worker
 * mode (no grid display-list record primitive exists to fall back to).
 */
export class EngineUnsupportedError extends Error {
  constructor(
    public readonly method: string,
    public readonly reason: string,
  ) {
    super(`Engine cannot serve "${method}": ${reason}`);
    this.name = "EngineUnsupportedError";
  }
}

/**
 * What an engine impl can do beyond the always-equivalent TIER-1 facade.
 *  - `localPaint` — a live non-transferable canvas/ctx is reachable (renderPage,
 *    renderPageWindow, paintGridViewport, setImageBitmap fast paths).
 *  - `localChartRaster` — a chart can be rasterized into a caller ctx.
 *  - `inProcessDoc` — the document + wasm live on THIS thread, gating the sync
 *    load HOT path (no added microtask) AND the sync-extract copy-event hatches.
 * InProcess = all true; Remote = all false.
 */
export interface EngineCapabilities {
  readonly localPaint: boolean;
  readonly localChartRaster: boolean;
  readonly inProcessDoc: boolean;
}

/**
 * TIER 1 — the portable engine both InProcessEngine and RemoteEngine implement
 * (conformance-tested; every arg/return is structured-clone-safe). Reads are
 * async (already async wrappers over sync wasm — no new mismatch); notifies are
 * sync-void fire-and-forget. Each method's body IS the prior dual-branch, minus
 * the Viewer-side orchestration which stays at the call site.
 */
export interface EngineFacade {
  readonly capabilities: EngineCapabilities;

  // ── loads / layout / time / record (HOT — the Viewer calls the in-process
  //    arm inline for zero added microtask; the worker arm routes here) ──
  loadDocx(bytes: Uint8Array): Promise<void>;
  loadXlsx(bytes: Uint8Array): Promise<void>;
  loadPptx(bytes: Uint8Array): Promise<void>;
  setNow(now: number): Promise<void>;
  /** Per-page size (§17.6): resolves page `idx`'s section geometry. A doc
   *  whose pages all share a size returns identical dims for every idx. */
  pageSize(idx: number): Promise<PageSize>;
  /** `n` iff the most recent layout stopped early (memory budget /
   *  allocation failure) after emitting `n` pages; null = complete. Read after
   *  `runLayout` resolves — the wasm stamps it before resolving. */
  layoutTruncatedAt(): Promise<number | null>;
  /** The engine's total wasm linear memory in bytes (its lifetime
   *  PEAK, since wasm memory never shrinks). Drives the worker-recycle
   *  decision on document switch. */
  memoryBytes(): Promise<number>;
  abortLayout(): Promise<void>;
  runLayout(onPageReady: (idx: number) => void, gen: number): Promise<number>;
  recordPage(idx: number): Promise<EncodedDisplayList>;
  // Record ONE grid scroll-window to a display list (worker-mode grid
  // paint primitive; the symmetric partner of `paintGridViewport`'s TIER-2
  // fast path). Both engines expose it: InProcess for conformance/fallback,
  // Remote to RPC the off-thread record. A non-SML wasm returns an empty list.
  recordGridViewport(sheet: number, scrollX: number, scrollY: number, viewW: number, viewH: number, zoom: number): Promise<EncodedDisplayList>;
  // Window-local fix: record ONE WML/PML page slice (window-local, culled,
  // background-reduced) as a display list — the worker-mode partner of the
  // in-process `renderPageWindow`. The recorded coords are already window-local,
  // so the main thread replays SCALE-ONLY (no giant CTM translate that overflows
  // Skia at high zoom). A non-WML/PML wasm returns an empty list.
  recordPageWindow(idx: number, clipX: number, clipY: number, viewW: number, viewH: number, zoom: number): Promise<EncodedDisplayList>;

  // ── reads (the Viewer awaits; coalesce/map stays at the call site) ──
  //
  // The return type is `T | Promise<T>`: InProcessEngine returns SYNCHRONOUSLY for
  // the exports the old Viewer called without `await` (so the Viewer's own async
  // wrapper resolves in the SAME single microtask as before — byte-identical
  // in-process timing; a fire-and-forget consumer that relies on immediate
  // resolution, e.g. plugin-grid's `sheets:changed` resetup IIFE, is preserved).
  // The wasm-async exports (the ones the old Viewer awaited) stay Promise-typed.
  // RemoteEngine is always async (an RPC).
  cellAt(page: number, x: number, y: number): CellReadout | null | Promise<CellReadout | null>;
  sheetInfo(): RawSheetInfo[] | Promise<RawSheetInfo[]>;
  gridExtent(sheet: number): GridExtent | null | Promise<GridExtent | null>;
  gridCellAt(sheet: number, x: number, y: number, scrollX: number, scrollY: number): CellReadout | null | Promise<CellReadout | null>;
  gridContentBounds(sheet: number): import("./index.js").CellRange | null | Promise<import("./index.js").CellRange | null>;
  gridPaneInfo(sheet: number): GridPaneInfo | null | Promise<GridPaneInfo | null>;
  linkRects(idx: number): LinkRect[] | Promise<LinkRect[]>;
  bookmarkPosition(name: string): BookmarkPosition | null | Promise<BookmarkPosition | null>;
  comments(): Comment[] | Promise<Comment[]>;
  commentRects(idx: number): CommentRect[] | Promise<CommentRect[]>;
  searchPage(idx: number, query: string, opts: SearchOpts, rowStart?: number, rowEnd?: number): SearchMatch[] | Promise<SearchMatch[]>;
  resolveSearchRect(sheet: number, cellRef: string): SearchRectResolved | null | Promise<SearchRectResolved | null>;
  // ── The unified selection verb set (ONE spelling per concept; the
  //    loaded doc kind is the engine-side discriminant; every arg/return is a
  //    structured-clone-safe plain object riding the generic worker dispatch) ──
  docHitTest(loc: DocLoc, x: number, y: number, granularity: string): Promise<DocHit | null>;
  docHitTestObject(loc: DocLoc, x: number, y: number): Promise<DocObjectHit | null>;
  docSelectionRects(loc: DocLoc, sel: DocSelection): Promise<Rect[]>;
  docExtractRange(sel: DocSelection): Promise<DocFragment | CellFragment | null>;
  chartData(key: string): Promise<ChartCopyData | null>;
  // EMF raster cache — rasterize the cached EMF drawing program for the
  // synthetic key `"emf-pic:{rel}"` at (pxW, pxH) DEVICE px into an
  // ImageBitmap (structured-clone/Transferable-safe, so it fits the tier-1
  // contract). `null` = cannot serve (feature-off/stale wasm, no
  // OffscreenCanvas, unknown key after a doc switch) — the caller keeps the
  // placeholder / stale bitmap and MUST NOT register anything.
  rasterizeEmf(key: string, pxW: number, pxH: number): Promise<ImageBitmap | null>;
  debugFonts(): WasmFontTable | undefined | Promise<WasmFontTable | undefined>;
  // The embedded-image bytes for the CURRENT doc. InProcess
  // reads the LOCAL wasm SYNC (byte-identical timing); Remote RPCs the worker
  // whose wasm HAS the doc (bytes ride back as Transferable ArrayBuffers). The
  // Viewer's `#decodeImagesInBackground` decodes these on the main thread.
  imageRefs(): ImageRef[] | Promise<ImageRef[]>;

  // ── notifies (fire-and-forget, sync-void) ──
  // `null` clears (ONE convention; the three legacy sentinels are gone).
  docSetSelection(sel: DocSelection | null): void;
  docSetSelectedObject(obj: { surface: number; key: string } | null): void;
  setSearchHighlight(query: string, opts: SearchOpts, activePage: number, activeIndex: number, activeCellRef: string | null): void;

  // ── font writes ──
  fontFamiliesUsed(): Promise<string[]>;
  addFontAlias(fromFamily: string, toFamily: string): Promise<void>;
  loadFont(family: string, bytes: Uint8Array, weight: number, italic: boolean): Promise<void>;
}

/**
 * TIER 2 — direct-to-canvas paint that needs a live, non-transferable
 * canvas/ctx/ImageBitmap. Capability-DETECTED (`capabilities.localPaint`), NOT
 * on the facade. InProcessEngine implements it; RemoteEngine does NOT (the
 * Viewer composes a documented fallback for each — see the Viewer render seams).
 */
export interface LocalPaintEngine {
  renderPage(idx: number, canvas: HTMLCanvasElement): void;
  renderPageWindow(idx: number, canvas: HTMLCanvasElement, clipX: number, clipY: number, viewW: number, viewH: number, zoom: number, dpr: number): void;
  paintGridViewport(sheet: number, canvas: HTMLCanvasElement, scrollX: number, scrollY: number, viewW: number, viewH: number, zoom: number): void;
  renderChartToCanvas(key: string, ctx: CanvasRenderingContext2D | OffscreenCanvasRenderingContext2D, wPx: number, hPx: number, scale: number): boolean;
  setImageBitmap(relId: string, bitmap: ImageBitmap): void;
  /** Drain the wasm-side per-paint EMF paint-request log (`[{ key:
   *  "emf-pic:{rel}", dw, dh }]`, PAGE-LOGICAL px). In-process only — the
   *  Viewer drains synchronously after each local paint (worker mode detects
   *  draws replay-side instead). Feature-off/stale wasm → `[]`. */
  takeEmfPaintRequests(): Array<{ key: string; dw: number; dh: number }>;
}

const IN_PROCESS_CAPS: EngineCapabilities = { localPaint: true, localChartRaster: true, inProcessDoc: true };
const REMOTE_CAPS: EngineCapabilities = { localPaint: false, localChartRaster: false, inProcessDoc: false };

/**
 * The in-process engine: the local `WasmViewer` with the EXACT prior local-branch
 * bodies (same `typeof` stale guards, same `?? null`/`?? []`). Implements BOTH
 * the tier-1 facade and the tier-2 local-paint capability. The wasm handle is the
 * SAME instance the Viewer keeps as `#wasm` (the hot load path + replay helpers
 * read it directly); this class is a second reference for the collapsed calls.
 */
export class InProcessEngine implements EngineFacade, LocalPaintEngine {
  readonly capabilities = IN_PROCESS_CAPS;
  #wasm: WasmViewerType;
  constructor(wasm: WasmViewerType) {
    this.#wasm = wasm;
  }
  /** Loosely-typed view of the wasm handle for exports absent on the docx-only .d.ts. */
  #w(): Record<string, ((...a: unknown[]) => unknown) | undefined> {
    return this.#wasm as unknown as Record<string, ((...a: unknown[]) => unknown) | undefined>;
  }

  // ── loads / layout / time / record ──
  async loadDocx(bytes: Uint8Array): Promise<void> {
    this.#wasm.loadDocx(bytes);
  }
  async loadXlsx(bytes: Uint8Array): Promise<void> {
    this.#wasm.loadXlsx(bytes);
  }
  async loadPptx(bytes: Uint8Array): Promise<void> {
    this.#wasm.loadPptx(bytes);
  }
  async setNow(now: number): Promise<void> {
    this.#wasm.setNow(now);
  }
  async pageSize(idx: number): Promise<PageSize> {
    return this.#wasm.pageSize(idx) as PageSize;
  }
  async layoutTruncatedAt(): Promise<number | null> {
    // wasm-bindgen maps Option<u32>::None to undefined; normalize to null so
    // both engine arms present one shape. typeof-guarded for stale artifacts
    // and consumer test doubles — same degrade the Remote arm gets via #tryReq.
    if (typeof this.#wasm.layoutTruncatedAt !== "function") return null;
    return (this.#wasm.layoutTruncatedAt() as number | undefined) ?? null;
  }
  async memoryBytes(): Promise<number> {
    if (typeof this.#wasm.memoryBytes !== "function") return 0;
    return this.#wasm.memoryBytes() as number;
  }
  async abortLayout(): Promise<void> {
    this.#wasm.abortLayout();
  }
  runLayout(onPageReady: (idx: number) => void, _gen: number): Promise<number> {
    return this.#wasm.runLayout(onPageReady);
  }
  async recordPage(idx: number): Promise<EncodedDisplayList> {
    return this.#wasm.renderPageDisplayList(idx) as EncodedDisplayList;
  }
  async recordGridViewport(sheet: number, scrollX: number, scrollY: number, viewW: number, viewH: number, zoom: number): Promise<EncodedDisplayList> {
    // typeof-guarded so a docx/pptx-only wasm returns an empty list rather than
    // throwing a conformance failure (mirrors the read guards below).
    const fn = this.#w().recordGridViewport;
    if (typeof fn !== "function") return { opcodes: new Float64Array(0), strings: [] };
    return fn.apply(this.#wasm, [sheet, scrollX, scrollY, viewW, viewH, zoom]) as EncodedDisplayList;
  }
  async recordPageWindow(idx: number, clipX: number, clipY: number, viewW: number, viewH: number, zoom: number): Promise<EncodedDisplayList> {
    // typeof-guarded so a stale/SML-only wasm returns an empty list rather than
    // throwing (mirrors recordGridViewport). SML falls through to an empty list.
    const fn = this.#w().renderPageWindowDisplayList;
    if (typeof fn !== "function") return { opcodes: new Float64Array(0), strings: [] };
    return fn.apply(this.#wasm, [idx, clipX, clipY, viewW, viewH, zoom]) as EncodedDisplayList;
  }

  // ── async reads (verbatim local branches) ──
  // NB: every guarded call is invoked bound to `this.#wasm` (via `.apply`) —
  // wasm-bindgen methods carry the instance pointer on `this`, so an UNBOUND
  // `const fn = w.m; fn()` would break them (and the test fakes' private fields).
  cellAt(page: number, x: number, y: number): CellReadout | null {
    const fn = this.#w().cellAt;
    if (typeof fn !== "function") return null; // docx-only wasm build
    return (fn.apply(this.#wasm, [page, x, y]) as CellReadout | null) ?? null;
  }
  sheetInfo(): RawSheetInfo[] {
    const fn = this.#w().sheetInfo;
    if (typeof fn !== "function") return []; // docx-only wasm build
    return (fn.apply(this.#wasm, []) as RawSheetInfo[]) ?? [];
  }
  gridExtent(sheet: number): GridExtent | null {
    const fn = this.#w().gridExtent;
    if (typeof fn !== "function") return null;
    return (fn.apply(this.#wasm, [sheet]) as GridExtent | null) ?? null;
  }
  gridCellAt(sheet: number, x: number, y: number, scrollX: number, scrollY: number): CellReadout | null {
    const fn = this.#w().gridCellAt;
    if (typeof fn !== "function") return null;
    return (fn.apply(this.#wasm, [sheet, x, y, scrollX, scrollY]) as CellReadout | null) ?? null;
  }
  gridContentBounds(sheet: number): import("./index.js").CellRange | null {
    const fn = this.#w().gridContentBounds;
    if (typeof fn !== "function") return null;
    return (fn.apply(this.#wasm, [sheet]) as import("./index.js").CellRange | null) ?? null;
  }
  gridPaneInfo(sheet: number): GridPaneInfo | null {
    const fn = this.#w().gridPaneInfo;
    if (typeof fn !== "function") return null;
    return (fn.apply(this.#wasm, [sheet]) as GridPaneInfo | null) ?? null;
  }
  linkRects(idx: number): LinkRect[] {
    return this.#wasm.linkRects(idx) as LinkRect[];
  }
  bookmarkPosition(name: string): BookmarkPosition | null {
    return (this.#wasm.bookmarkPosition(name) as BookmarkPosition | null) ?? null;
  }
  comments(): Comment[] {
    return this.#wasm.comments() as Comment[];
  }
  commentRects(idx: number): CommentRect[] {
    const fn = this.#w().commentRects;
    if (typeof fn !== "function") return []; // docx-only build
    return (fn.apply(this.#wasm, [idx]) as CommentRect[]) ?? [];
  }
  searchPage(idx: number, query: string, opts: SearchOpts, rowStart?: number, rowEnd?: number): SearchMatch[] {
    const fn = this.#w().searchPage;
    if (typeof fn !== "function") return []; // build without search
    return (fn.apply(this.#wasm, [idx, query, opts ?? {}, rowStart, rowEnd]) as SearchMatch[]) ?? [];
  }
  resolveSearchRect(sheet: number, cellRef: string): SearchRectResolved | null {
    const fn = this.#w().resolveSearchRect;
    if (typeof fn !== "function") return null;
    return (fn.apply(this.#wasm, [sheet, cellRef]) as SearchRectResolved | null) ?? null;
  }
  // The unified verbs (typeof-guarded like every read: a stale wasm
  // artifact without the `doc*` exports degrades to null/[], never throws).
  async docHitTest(loc: DocLoc, x: number, y: number, granularity: string): Promise<DocHit | null> {
    const fn = this.#w().docHitTest;
    if (typeof fn !== "function") return null; // stale build
    return ((await fn.apply(this.#wasm, [loc, x, y, granularity])) as DocHit | null) ?? null;
  }
  async docHitTestObject(loc: DocLoc, x: number, y: number): Promise<DocObjectHit | null> {
    const fn = this.#w().docHitTestObject;
    if (typeof fn !== "function") return null; // stale build
    return ((await fn.apply(this.#wasm, [loc, x, y])) as DocObjectHit | null) ?? null;
  }
  async docSelectionRects(loc: DocLoc, sel: DocSelection): Promise<Rect[]> {
    const fn = this.#w().docSelectionRects;
    if (typeof fn !== "function") return []; // stale build
    return ((await fn.apply(this.#wasm, [loc, sel])) as Rect[]) ?? [];
  }
  async docExtractRange(sel: DocSelection): Promise<DocFragment | CellFragment | null> {
    const fn = this.#w().docExtractRange;
    if (typeof fn !== "function") return null; // stale build
    return ((await fn.apply(this.#wasm, [sel])) as DocFragment | CellFragment | null) ?? null;
  }
  async chartData(key: string): Promise<ChartCopyData | null> {
    const fn = this.#w().chartData;
    if (typeof fn !== "function") return null; // stale build
    return ((await fn.apply(this.#wasm, [key])) as ChartCopyData | null) ?? null;
  }
  async rasterizeEmf(key: string, pxW: number, pxH: number): Promise<ImageBitmap | null> {
    const fn = this.#w().renderEmfToCanvas;
    if (typeof fn !== "function") return null; // feature-off / stale wasm build
    // Older Safari (<16.4) main thread: no OffscreenCanvas — degrade to null
    // (the placeholder persists), never crash.
    if (typeof OffscreenCanvas === "undefined") return null;
    const canvas = new OffscreenCanvas(pxW, pxH);
    // Default alpha:true is LOAD-BEARING: the raster's transparent ground is
    // the whole point (an opaque context would re-introduce a fill behind
    // behind-text EMFs).
    const ctx = canvas.getContext("2d");
    if (!ctx) return null;
    const ok = fn.apply(this.#wasm, [key, ctx, pxW, pxH, 1]) as boolean;
    // `false` = unknown key (doc switched mid-flight) / degenerate dims — do
    // NOT hand back the fully-transparent canvas (registering it would make
    // the EMF vanish and rule 3 would never re-raster at that size).
    if (!ok) return null;
    return canvas.transferToImageBitmap();
  }
  debugFonts(): WasmFontTable | undefined {
    const fn = (this.#wasm as { debugFonts?: () => unknown }).debugFonts;
    if (typeof fn !== "function") return undefined; // probe OFF in this build
    return fn.call(this.#wasm) as WasmFontTable;
  }
  // VERBATIM local read over the SAME `#wasm` the Viewer
  // holds, SYNC-returning (mirrors cellAt/sheetInfo sync arms). The Viewer's
  // in-process CALL SITE does NOT route here (it reads `this.#wasm` directly for
  // byte-identical timing); this arm defines the Remote symmetry + is exercised
  // by conformance tests.
  imageRefs(): ImageRef[] {
    return (this.#wasm.imageRefs() as ImageRef[]) ?? [];
  }

  // ── notifies (verbatim local branches) ──
  docSetSelection(sel: DocSelection | null): void {
    const fn = this.#w().docSetSelection;
    if (typeof fn === "function") fn.apply(this.#wasm, [sel]);
  }
  docSetSelectedObject(obj: { surface: number; key: string } | null): void {
    const fn = this.#w().docSetSelectedObject;
    if (typeof fn === "function") fn.apply(this.#wasm, [obj]);
  }
  setSearchHighlight(query: string, opts: SearchOpts, activePage: number, activeIndex: number, activeCellRef: string | null): void {
    const fn = this.#w().setSearchHighlight;
    if (typeof fn === "function") fn.apply(this.#wasm, [query, opts ?? {}, activePage, activeIndex, activeCellRef]);
  }

  // ── font writes ──
  async fontFamiliesUsed(): Promise<string[]> {
    return this.#wasm.fontFamiliesUsed() as string[];
  }
  async addFontAlias(fromFamily: string, toFamily: string): Promise<void> {
    this.#wasm.addFontAlias(fromFamily, toFamily);
  }
  async loadFont(family: string, bytes: Uint8Array, weight: number, italic: boolean): Promise<void> {
    this.#wasm.loadFont(family, bytes, weight, italic);
  }

  // ── TIER 2 — local paint (verbatim local branches) ──
  renderPage(idx: number, canvas: HTMLCanvasElement): void {
    this.#wasm.renderPage(idx, canvas);
  }
  renderPageWindow(idx: number, canvas: HTMLCanvasElement, clipX: number, clipY: number, viewW: number, viewH: number, zoom: number, dpr: number): void {
    this.#wasm.renderPageWindow(idx, canvas, clipX, clipY, viewW, viewH, zoom, dpr);
  }
  paintGridViewport(sheet: number, canvas: HTMLCanvasElement, scrollX: number, scrollY: number, viewW: number, viewH: number, zoom: number): void {
    const fn = this.#w().paintGridViewport;
    if (typeof fn !== "function") return;
    fn.apply(this.#wasm, [sheet, canvas, scrollX, scrollY, viewW, viewH, zoom]);
  }
  renderChartToCanvas(key: string, ctx: CanvasRenderingContext2D | OffscreenCanvasRenderingContext2D, wPx: number, hPx: number, scale: number): boolean {
    const fn = this.#wasm as unknown as {
      renderChartToCanvas?: (k: string, c: unknown, w: number, h: number, s: number) => boolean;
    };
    if (typeof fn.renderChartToCanvas !== "function") return false; // stale build
    return fn.renderChartToCanvas(key, ctx, wPx, hPx, scale);
  }
  setImageBitmap(relId: string, bitmap: ImageBitmap): void {
    this.#wasm.setImageBitmap(relId, bitmap);
  }
  takeEmfPaintRequests(): Array<{ key: string; dw: number; dh: number }> {
    const fn = this.#w().takeEmfPaintRequests;
    if (typeof fn !== "function") return []; // feature-off / stale wasm build
    return (fn.apply(this.#wasm, []) as Array<{ key: string; dw: number; dh: number }>) ?? [];
  }
}

/**
 * The worker-backed engine: every method is the EXACT prior `#remote` RPC branch
 * (`request` for reads, `notify` for fire-and-forget). Advertises NO capabilities
 * (localPaint/localChartRaster/inProcessDoc all false), so the Viewer composes
 * the documented fallbacks (renderPage→replay, chart→false, sync-extract→null,
 * paintGridViewport→EngineUnsupportedError). The transport lifecycle (pageReady
 * forwarding, onDead fallback, dispose) stays in the Viewer's tier-3 state; this
 * class only issues request/notify.
 */
export class RemoteEngine implements EngineFacade {
  readonly capabilities = REMOTE_CAPS;
  #t: Transport;
  constructor(transport: Transport) {
    this.#t = transport;
  }
  /** Degrade to `fallback` when a STALE worker bundle throws "unknown worker method". */
  async #tryReq<T>(method: string, args: unknown[], fallback: T): Promise<T> {
    try {
      return ((await this.#t.request(method, args)) as T | null | undefined) ?? fallback;
    } catch {
      return fallback;
    }
  }

  // ── loads / layout / time / record ──
  async loadDocx(bytes: Uint8Array): Promise<void> {
    await this.#t.request("loadDocx", [bytes]);
  }
  async loadXlsx(bytes: Uint8Array): Promise<void> {
    await this.#t.request("loadXlsx", [bytes]);
  }
  async loadPptx(bytes: Uint8Array): Promise<void> {
    await this.#t.request("loadPptx", [bytes]);
  }
  async setNow(now: number): Promise<void> {
    await this.#t.request("setNow", [now]);
  }
  async pageSize(idx: number): Promise<PageSize> {
    return (await this.#t.request("pageSize", [idx])) as PageSize;
  }
  async layoutTruncatedAt(): Promise<number | null> {
    // #tryReq: a stale worker bundle without the export degrades to null
    // ("layout complete"), never throws — same posture as the doc* verbs.
    return this.#tryReq<number | null>("layoutTruncatedAt", [], null);
  }
  async memoryBytes(): Promise<number> {
    // Degrade 0 = "never worth recycling" on a stale worker bundle — the safe
    // direction (keep the warm worker; recycling is hygiene, not correctness).
    return this.#tryReq<number>("memoryBytes", [], 0);
  }
  async abortLayout(): Promise<void> {
    await this.#t.request("abortLayout", []);
  }
  async runLayout(_onPageReady: (idx: number) => void, gen: number): Promise<number> {
    // The per-page callback cannot cross the worker boundary; the Viewer wires
    // the transport's pageReady events into it (tier-3 generation guard). Here we
    // only kick the RPC with the load generation the worker stamps its events by.
    return (await this.#t.request("runLayout", [gen])) as number;
  }
  async recordPage(idx: number): Promise<EncodedDisplayList> {
    return (await this.#t.request("renderPageDisplayList", [idx])) as EncodedDisplayList;
  }
  async recordGridViewport(sheet: number, scrollX: number, scrollY: number, viewW: number, viewH: number, zoom: number): Promise<EncodedDisplayList> {
    return (await this.#t.request("recordGridViewport", [sheet, scrollX, scrollY, viewW, viewH, zoom])) as EncodedDisplayList;
  }
  async recordPageWindow(idx: number, clipX: number, clipY: number, viewW: number, viewH: number, zoom: number): Promise<EncodedDisplayList> {
    return (await this.#t.request("renderPageWindowDisplayList", [idx, clipX, clipY, viewW, viewH, zoom])) as EncodedDisplayList;
  }

  // ── async reads ──
  async cellAt(page: number, x: number, y: number): Promise<CellReadout | null> {
    return ((await this.#t.request("cellAt", [page, x, y])) as CellReadout | null) ?? null;
  }
  async sheetInfo(): Promise<RawSheetInfo[]> {
    return ((await this.#t.request("sheetInfo", [])) as RawSheetInfo[]) ?? [];
  }
  async gridExtent(sheet: number): Promise<GridExtent | null> {
    return ((await this.#t.request("gridExtent", [sheet])) as GridExtent | null) ?? null;
  }
  async gridCellAt(sheet: number, x: number, y: number, scrollX: number, scrollY: number): Promise<CellReadout | null> {
    return ((await this.#t.request("gridCellAt", [sheet, x, y, scrollX, scrollY])) as CellReadout | null) ?? null;
  }
  async gridContentBounds(sheet: number): Promise<import("./index.js").CellRange | null> {
    return ((await this.#t.request("gridContentBounds", [sheet])) as import("./index.js").CellRange | null) ?? null;
  }
  async gridPaneInfo(sheet: number): Promise<GridPaneInfo | null> {
    // Stale worker bundle → null (grid setup never breaks on a version skew).
    return this.#tryReq<GridPaneInfo | null>("gridPaneInfo", [sheet], null);
  }
  async linkRects(idx: number): Promise<LinkRect[]> {
    return ((await this.#t.request("linkRects", [idx])) as LinkRect[]) ?? [];
  }
  async bookmarkPosition(name: string): Promise<BookmarkPosition | null> {
    return ((await this.#t.request("bookmarkPosition", [name])) as BookmarkPosition | null) ?? null;
  }
  async comments(): Promise<Comment[]> {
    return ((await this.#t.request("comments", [])) as Comment[]) ?? [];
  }
  async commentRects(idx: number): Promise<CommentRect[]> {
    return ((await this.#t.request("commentRects", [idx])) as CommentRect[]) ?? [];
  }
  async searchPage(idx: number, query: string, opts: SearchOpts, rowStart?: number, rowEnd?: number): Promise<SearchMatch[]> {
    return ((await this.#t.request("searchPage", [idx, query, opts ?? {}, rowStart, rowEnd])) as SearchMatch[]) ?? [];
  }
  async resolveSearchRect(sheet: number, cellRef: string): Promise<SearchRectResolved | null> {
    return ((await this.#t.request("resolveSearchRect", [sheet, cellRef])) as SearchRectResolved | null) ?? null;
  }
  // The unified verbs. `#tryReq` degrade: a STALE worker bundle whose
  // wasm predates the `doc*` exports throws "unknown method" → null/[] (the
  // established version-skew posture; selection degrades, never breaks a load).
  async docHitTest(loc: DocLoc, x: number, y: number, granularity: string): Promise<DocHit | null> {
    return this.#tryReq<DocHit | null>("docHitTest", [loc, x, y, granularity], null);
  }
  async docHitTestObject(loc: DocLoc, x: number, y: number): Promise<DocObjectHit | null> {
    return this.#tryReq<DocObjectHit | null>("docHitTestObject", [loc, x, y], null);
  }
  async docSelectionRects(loc: DocLoc, sel: DocSelection): Promise<Rect[]> {
    return this.#tryReq<Rect[]>("docSelectionRects", [loc, sel], []);
  }
  async docExtractRange(sel: DocSelection): Promise<DocFragment | CellFragment | null> {
    return this.#tryReq<DocFragment | CellFragment | null>("docExtractRange", [sel], null);
  }
  async chartData(key: string): Promise<ChartCopyData | null> {
    return ((await this.#t.request("chartData", [key])) as ChartCopyData | null) ?? null;
  }
  async rasterizeEmf(key: string, pxW: number, pxH: number): Promise<ImageBitmap | null> {
    // Stale worker bundle (no rasterizeEmf special case) → null degrade; the
    // ImageBitmap rides the response as a Transferable (zero-copy).
    return this.#tryReq<ImageBitmap | null>("rasterizeEmf", [key, pxW, pxH], null);
  }
  async debugFonts(): Promise<WasmFontTable | undefined> {
    // Stale worker bundle / probe OFF in the worker's build → undefined.
    try {
      const table = (await this.#t.request("debugFonts", [])) as WasmFontTable | null | undefined;
      return table ?? undefined;
    } catch {
      return undefined;
    }
  }
  // RPC to the worker whose wasm HAS the doc. NO transfer arg
  // on the REQUEST (empty args); the image byte buffers ride the RESPONSE as
  // Transferables (worker.ts collectTransferables). `?? []` mirrors the sibling
  // read fallbacks.
  async imageRefs(): Promise<ImageRef[]> {
    return ((await this.#t.request("imageRefs", [])) as ImageRef[]) ?? [];
  }

  // ── notifies ──
  docSetSelection(sel: DocSelection | null): void {
    this.#t.notify("docSetSelection", [sel]);
  }
  docSetSelectedObject(obj: { surface: number; key: string } | null): void {
    this.#t.notify("docSetSelectedObject", [obj]);
  }
  setSearchHighlight(query: string, opts: SearchOpts, activePage: number, activeIndex: number, activeCellRef: string | null): void {
    this.#t.notify("setSearchHighlight", [query, opts ?? {}, activePage, activeIndex, activeCellRef]);
  }

  // ── font writes ──
  async fontFamiliesUsed(): Promise<string[]> {
    return (await this.#t.request("fontFamiliesUsed", [])) as string[];
  }
  async addFontAlias(fromFamily: string, toFamily: string): Promise<void> {
    await this.#t.request("addFontAlias", [fromFamily, toFamily]);
  }
  async loadFont(family: string, bytes: Uint8Array, weight: number, italic: boolean): Promise<void> {
    await this.#t.request("loadFont", [family, bytes, weight, italic]);
  }
}

export type { Transport, TransportEvent };
