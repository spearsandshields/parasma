/**
 * @parasma/core — framework-agnostic viewer class.
 *
 * Headless: never imports a framework. Owns the document
 * model via a WASM handle. Renders one page at a time into a
 * consumer-supplied canvas; the consumer owns scrolling and
 * virtualization via IntersectionObserver or any other mechanism.
 */

import init, {
  Viewer as WasmViewer,
  parserInfo as wasmParserInfo,
  type InitInput,
} from "./wasm/parasma_wasm.js";
import { replayDisplayList, type EncodedDisplayList } from "./replay.js";
import {
  createWorkerTransport,
  type Transport,
  type TransportEvent,
  type WorkerSelector,
} from "./transport.js";
import {
  InProcessEngine,
  RemoteEngine,
  type EngineFacade,
  type LocalPaintEngine,
  type ImageRef,
} from "./engine.js";
import { planEmfRaster, type EmfRasterEntry } from "./emf-raster-policy.js";

export {
  createWorkerTransport,
  createRenderWorker,
  WorkerTransport,
  type Transport,
  type TransportEvent,
  type WorkerSelector,
  type WorkerTransportOptions,
} from "./transport.js";

// The ONLY engine-facade symbol re-exported past @parasma/core:
// consumers need `instanceof EngineUnsupportedError` for the NOT-SUPPORTED policy
// (a tier-2 method a partial impl genuinely cannot serve throws it, LOUD). The
// EngineFacade/LocalPaintEngine interfaces themselves stay INTERNAL.
export { EngineUnsupportedError } from "./engine.js";

/** Dev-only (`?gridperf`) worker-mode grid scroll-latency sample: `rpc` = the
 *  off-thread record round-trip (upper bound), `replay` = the sole main-thread
 *  cost, `total` = rpc+replay, `ops` = recorded opcode count, `sy` = content
 *  scrollY at record time. Emitted only when `globalThis.__gridPerf` is set. */
export type GridPerfSample = { rpc: number; replay: number; total: number; ops: number; sy: number };

/** Any input the wasm glue's `init()` accepts. string|URL are worker-threadable
 *  (clone-safe over postMessage); Response/Module/BufferSource are in-process. */
export type WasmInput = string | URL | InitInput;

/**
 * Where a relative wasm URL is absolutized against. The worker's base differs
 * from the main thread's, so a relative string MUST be made absolute BEFORE it
 * is threaded into either target.
 */
function docBaseUri(): string {
  return typeof document !== "undefined" ? document.baseURI : "http://localhost/";
}
function joinUrl(base: string, file: string): string {
  return `${base.replace(/\/$/, "")}/${file}`;
}
function absolutizeWasmInput(input: WasmInput): WasmInput {
  if (typeof input === "string") {
    try {
      return new URL(input, docBaseUri()).href;
    } catch {
      return input;
    }
  }
  return input; // URL already absolute; Response/Module/BufferSource pass through
}

/**
 * Resolve the wasm binary source from config, by precedence
 * (explicit override > assetBaseUrl-derived > built-in co-located). Returns
 * `undefined` when no config field selects a source — the caller then runs
 * `init()` no-arg (co-located), byte-identical to today. `@public`
 */
export function resolveWasmInput(config?: ViewerConfig): WasmInput | undefined {
  if (!config) return undefined;
  const raw =
    config.wasmUrl ?? (config.assetBaseUrl ? joinUrl(config.assetBaseUrl, "parasma_wasm_bg.wasm") : undefined);
  return raw === undefined ? undefined : absolutizeWasmInput(raw);
}

/** A stable string key for a resolved input, used for the conflict-warn token. */
function wasmInputKey(input: WasmInput | undefined): string {
  if (input === undefined) return "<default>";
  if (typeof input === "string") return input;
  if (input instanceof URL) return input.href;
  return "<non-url-input>";
}

// The wasm module is a PROCESS singleton (first-init-wins). The resolved input
// key is stashed on a globalThis Symbol (not this module local) so conflict
// detection survives DUPLICATE modules on a page.
const WASM_INPUT_TOKEN = Symbol.for("parasma.wasmInput");

let wasmReadyPromise: Promise<void> | null = null;
async function ensureWasmReady(input?: WasmInput): Promise<void> {
  if (!wasmReadyPromise) {
    (globalThis as Record<symbol, unknown>)[WASM_INPUT_TOKEN] = wasmInputKey(input);
    wasmReadyPromise = (
      input === undefined
        ? init() // no-config => init() ZERO args, byte-identical to the old seam
        : init({ module_or_path: input }) // object form => no glue deprecation warn
    ).then(() => undefined);
  } else {
    const prev = (globalThis as Record<symbol, unknown>)[WASM_INPUT_TOKEN] as string | undefined;
    const next = wasmInputKey(input);
    if (prev !== undefined && next !== "<default>" && prev !== next) {
      console.warn(
        `[parasma] wasm already initialized from "${prev}"; ignoring conflicting wasmUrl "${next}" (process singleton, first-init wins)`,
      );
    }
  }
  return wasmReadyPromise;
}

/**
 * The origin-swap font resolver: rewrites a jsDelivr default URL onto `base`,
 * mirroring headless `fonts.ts` exactly. Pure sugar over {@link
 * Viewer.setFontUrlResolver}.
 */
function originSwapResolver(base: string): FontUrlResolver {
  const origin = base.replace(/\/$/, "");
  return (info) => info.defaultUrl.replace("https://cdn.jsdelivr.net", origin);
}

/**
 * Down-select a resolved wasm input to what the worker can receive over
 * postMessage (string|URL). A Response/Module is in-process-only — warn and let
 * the worker init its co-located binary.
 */
function workerThreadableWasm(input?: WasmInput): string | URL | undefined {
  if (typeof input === "string" || input instanceof URL) return input;
  if (input !== undefined) {
    console.warn(
      "[parasma] wasmUrl is a Response/Module; the worker cannot receive it over postMessage and will init co-located wasm.",
    );
  }
  return undefined;
}

let debugProbeWarned = false;
/**
 * Warn once if the loaded WASM was built without `--features debug-probe`.
 * In that build `viewer.debugLayout()` silently returns `undefined`, which is
 * easy to mistake for a layout bug while debugging. The release artifact ships
 * probe-free on purpose, so this is informational, not an error.
 */
function warnIfDebugProbeAbsent(wasm: WasmViewer): void {
  if (debugProbeWarned) return;
  debugProbeWarned = true;
  const hasProbe = typeof (wasm as { debugLayout?: unknown }).debugLayout === "function";
  if (!hasProbe) {
    console.warn(
      "[parasma] debug-probe OFF — rebuild wasm with `--features debug-probe` to use viewer.debugLayout()",
    );
  }
}

export interface PageSize {
  widthPx: number;
  heightPx: number;
}

/** A clickable hyperlink region on a page, in layout px. */
export interface LinkRect {
  x: number;
  y: number;
  w: number;
  h: number;
  /** `"url:<href>"` for external links, `"anchor:<bookmark>"` for internal. */
  target: string;
}

export interface BookmarkPosition {
  /** 0-based page index containing the bookmark. */
  page: number;
  /** Top of the bookmark's line, in layout px within the page. */
  y: number;
}

/**
 * One comment-range highlight rectangle on a page (Word comments v1),
 * in layout px. The engine paints a persistent translucent fill of each rect in
 * the canvas; the consumer hit-tests pointer events against these to link a
 * highlight back to its comment card(s). `ids` is the set of comment ranges
 * covering this run — normally one, but multiple where ranges OVERLAP (the rect
 * for the shared region carries every covering id, so highlights stack). `color`
 * is the per-author colour suggestion for `ids[0]`; the consumer owns final
 * presentation (the engine→TS interface is data only).
 */
export interface CommentRect {
  x: number;
  y: number;
  w: number;
  h: number;
  /** Comment ids covering this rect (>1 only where ranges overlap). */
  ids: number[];
  /** Per-author colour suggestion `#rrggbb` for `ids[0]`. */
  color: string;
}

/**
 * One Word comment (display-only, v1), joined to its range-start position.
 * Engine data only — all presentation (panel, card markup, highlight colour,
 * formatting of `date`) is the consumer's responsibility (`color` is merely a
 * per-author suggestion). Word-only; other formats report no comments.
 */
export interface Comment {
  /** `w:id` — stable comment id. */
  id: number;
  author: string;
  /** `w:date` ISO-8601 string, or null when absent. */
  date: string | null;
  /** `w:initials`, or null when absent. */
  initials: string | null;
  /** Flattened body text (paragraphs joined by `\n`). */
  text: string;
  /** 0-based page index of the comment range's start. */
  page: number;
  /** Top of the range-start line, in layout px within the page. */
  anchorY: number;
  /** Per-author colour suggestion `#rrggbb`; the consumer decides final paint. */
  color: string;
}

/** One full-text-search highlight rectangle on a page, in layout px. */
export interface SearchRect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/**
 * Selection — a logical, layout-independent WML text position:
 * `para` is the document-order paragraph counter assigned in the block-cache
 * build; `byte` is the offset into that paragraph's LOGICAL text (pre-pack
 * concatenation), snapped to a shaped-cluster boundary. Stable across zoom AND
 * re-layout (unlike a page-derived position), so it is THE currency `hitTest`
 * produces and `selectionRects`/`setSelection` consume.
 */
export interface TextPos {
  /** Document-order paragraph counter (block-cache build walk). */
  para: number;
  /** Byte offset into the paragraph's logical text (cluster-snapped). */
  byte: number;
}

/**
 * Fail LOUD on a scalar mis-call. The selection verbs take
 * `TextPos {para,byte}` OBJECTS inside a {@link DocSelection}. A caller passing
 * raw scalars instead makes `start`/`end` plain numbers, so `.para`/`.byte`
 * read `undefined`, the wire coerces undefined→0 at the u32 boundary, and
 * EVERY range silently collapses to the (0,0)-(0,0) empty range → `[]` (the
 * exact symptom early browser testing hit). Throwing here converts that
 * silent collapse into a self-describing error at the call site.
 */
function assertTextPos(p: TextPos, arg: string): void {
  if (p == null || typeof p.para !== "number" || typeof p.byte !== "number") {
    throw new TypeError(
      `Viewer selection API: \`${arg}\` must be a TextPos {para,byte} object, not a raw scalar`,
    );
  }
}

/** The DocSelection-wide loud-fail: text-ish kinds must carry real
 *  TextPos endpoints (same silent-collapse trap {@link assertTextPos} guards). */
function assertSelection(sel: DocSelection): void {
  if (sel.kind === "text" || sel.kind === "pmlText") {
    assertTextPos(sel.start, "start");
    assertTextPos(sel.end, "end");
  }
}

/** Hit-test granularity (char/word/paragraph expansion). */
export type Granularity = "char" | "word" | "paragraph";

/**
 * An image OBJECT reference. `kind` is the paint stream;
 * `key` = relId; `index` = 0 for v1 (duplicate-relId disambiguation deferred).
 * Identical shape across engine hit result → plugin state → objectInfo arg → bus
 * event → clipboard.
 */
export interface ObjectRef {
  page: number;
  // `picture` is a PML slide picture (`page` = slide index,
  // `key` = the slide-namespaced image key). The clipboard image flavors treat
  // it exactly like a WML image object (the retained bytes/bitmaps are keyed by
  // `key` for all three).
  // `chart` is a WML inline chart (`key` = the
  // `chart::<rId>` chart-XML key). Charts stay OUT of the image-bytes paths
  // (objectInfo/imageOriginal/imageBitmapFor return null for chart keys); the
  // chart flavors use {@link Viewer.chartData} / {@link Viewer.renderChartToCanvas}.
  kind: "inlineImage" | "anchoredImage" | "picture" | "chart";
  key: string; // relId (WML) / slide-namespaced image key (PML picture) / chart:: key
  index: number;
}

/**
 * The chart COPY MODEL returned by {@link Viewer.chartData}:
 * the Excel source-data layout (series names + per-category values, the TSV
 * source) plus the chart's DISPLAY px (`wPx`/`hPx`, the first painted
 * placement's object box — the basis for the 4× raster and the pasted
 * `<img width/height>`). Structured-clone-safe (worker-dispatchable).
 */
export interface ChartCopyData {
  categories: string[];
  series: Array<{ name: string; values: number[] }>;
  wPx: number;
  hPx: number;
}

/**
 * A PML text position. Clamped to ONE target: a
 * shape or a table cell on one slide. The `(targetKind, targetA, targetB)`
 * triple is the opaque wire the engine round-trips (kind 0 = shape[A], 1 =
 * tableCell[table=A, cell=B]); TS never interprets it, only echoes it back.
 */
export interface PmlTargetRef {
  targetKind: number;
  targetA: number;
  targetB: number;
}

/**
 * The format-neutral surface address every unified selection verb takes.
 * `surface` is the 0-based PAGE (word) / SHEET (excel grid) / SLIDE (ppt); the
 * LOADED doc kind is the discriminant — no format tag crosses the wire.
 */
export interface DocLoc {
  surface: number;
  /**
   * Grid only — the content scroll offsets (BOTH present ⇒ `surface` addresses
   * the SML grid's content space). They must cross the boundary because frozen
   * panes make viewport→content NON-affine (§18.3.1.66 quadrant mapping). An
   * SML doc ignores a scroll-less loc (the grid is its sole selectable
   * surface); WML/PML ignore the offsets.
   */
  scrollX?: number;
  scrollY?: number;
  /** PML only — the drag clamp: resolve within this target (a drag never
   *  escapes its anchor shape/cell). */
  clamp?: PmlTargetRef;
}

/**
 * THE selection currency: the one cross-boundary "what is selected"
 * union (`setSelection`/`selectionRects`/`extractRange` all take it; `null`
 * clears — replacing the three legacy per-format clear sentinels).
 */
export type DocSelection =
  | { kind: "text"; start: TextPos; end: TextPos }
  | { kind: "cells"; sheet: number; range: CellRange; anchor?: { row: number; col: number } }
  | { kind: "pmlText"; slide: number; target: PmlTargetRef; start: TextPos; end: TextPos };

/** Object copy metadata. `wPx`/`hPx` are INTRINSIC (bitmap) px. */
export interface ObjectInfo {
  relId: string;
  mime: string;
  wPx: number;
  hPx: number;
}

/**
 * The unified hit-test result. `kind:"text"`: `pos` is the caret
 * position; `range` (word/paragraph granularity, WML only today) the expanded
 * span; `target` present ⇔ a PML hit (the shape/cell the point resolved in).
 * `kind:"cell"`: an SML grid hit — the zone + a valid 1-based row/col even on
 * a model-less cell, with `mergeAnchor` when the point lands inside a
 * merge (the active cell snaps to it). Objects are a SEPARATE probe
 * ({@link Viewer.hitTestObject}, object-first z-order).
 */
export type DocHit =
  | {
      kind: "text";
      surface: number;
      pos: TextPos;
      range?: { start: TextPos; end: TextPos };
      target?: PmlTargetRef;
    }
  | {
      kind: "cell";
      surface: number;
      zone: GridZone;
      row: number;
      col: number;
      mergeAnchor?: { row: number; col: number };
    };

/**
 * The unified object hit: the topmost image/chart/picture under the
 * point. `kind` is the {@link ObjectRef} kind and `key` the shared
 * relId/chart-key namespace (`chartData` / `renderChartToCanvas` /
 * `setSelectedObject` already treat WML relIds and SML/PML keys as one map).
 */
export interface DocObjectHit {
  surface: number;
  kind: ObjectRef["kind"];
  key: string;
  rect: Rect;
}

/**
 * A bare selection highlight rectangle in layout px. Distinct
 * from {@link SearchRect} (a different paint pass) though structurally identical;
 * kept separate so the two seams can diverge (per-rect direction is a follow-on).
 */
export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

// ─── DocFragment (the flavor-neutral extraction schema) ──────────────────────
//
// These MIRROR, byte-for-byte, the JS property names the wasm `frag_to_jsvalue`
// sets (crates/parasma-wasm/src/lib.rs). The engine delivers ALL fields even
// though text/plain only reads `text`/`list.marker`/`footnoteMark`, so the
// HTML flavor is a pure-TS consumer needing no engine change.

/** An inline image reference on a run (bytes never cross the wasm boundary). */
export interface FragImage {
  relId: string;
  wPx: number;
  hPx: number;
}

/** A resolved cell/table border edge (Parasma tracks solid-only; style="single"). */
export interface FragBorder {
  style: string;
  color: string;
  widthPx: number;
}

/** One logical-order run of styled text (or a zero-width object). */
export interface FragRun {
  text: string;
  bold: boolean;
  italic: boolean;
  underline: string;
  strike: boolean;
  doubleStrike: boolean;
  fontFamily: string;
  fontSizePt: number;
  color: string;
  highlight: string | null;
  runShading: string | null;
  vertAlign: string;
  link: string | null;
  image: FragImage | null;
  footnoteMark: string | null;
}

/** A resolved list marker on a fragment paragraph. */
export interface FragList {
  ordered: boolean;
  numId: number;
  level: number;
  value: number;
  fmt: string;
  marker: string;
}

/** A paragraph in a fragment. `runs` empty ⇒ §17.3.1.29 empty paragraph. */
export interface FragPara {
  runs: FragRun[];
  alignment: string;
  dir: "ltr" | "rtl";
  list: FragList | null;
}

/** One table cell. `borders` = [top, right, bottom, left]. */
export interface FragCell {
  rowspan: number;
  colspan: number;
  fill: string | null;
  borders: (FragBorder | null)[];
  vMergedAway: boolean;
  blocks: FragBlock[];
}

export interface FragRow {
  cells: FragCell[];
}

export interface FragTable {
  rows: FragRow[];
}

/** A top-level fragment block: a body paragraph or a table. */
export type FragBlock =
  | { kind: "para"; para: FragPara }
  | { kind: "table"; table: FragTable };

/** The extraction result. `blocks` in LOGICAL document order. */
export interface DocFragment {
  blocks: FragBlock[];
}

// ─── SML cells selection + copy (grid view only) ─────────────────────────────
//
// These MIRROR the JS property names the wasm `docHitTest` cell arm /
// `gridContentBounds` / `cell_fragment_to_jsvalue` set
// (crates/parasma-wasm/src/{select_dispatch.rs,sml/select.rs,lib.rs}).

/** A rectangular cell range — 1-based, inclusive, normalized (matches CellRange::new). */
export interface CellRange {
  r0: number;
  c0: number;
  r1: number;
  c1: number;
}

/** The grid zone a point lands in (geometry-only). `corner` is a no-op. */
export type GridZone = "cell" | "rowHeader" | "colHeader" | "corner";

/**
 * A sheet's FROZEN pane (§18.3.1.66, grid view only) — the wasm `gridPaneInfo`
 * read. `bandW`/`bandH` are the pinned-band sizes in CONTENT px (0 = that axis
 * is not frozen); `scrollX`/`scrollY` the `topLeftCell`-derived initial BODY
 * scroll (0 when absent → A1). `null` from the read = no frozen pane (incl.
 * `state="split"`, which is ignored by design).
 */
export interface GridPaneInfo {
  /** Frozen column count (`@xSplit`). */
  xSplit: number;
  /** Frozen row count (`@ySplit`). */
  ySplit: number;
  /** Content-space width of the pinned column band (0 = no frozen columns). */
  bandW: number;
  /** Content-space height of the pinned row band (0 = no frozen rows). */
  bandH: number;
  /** Initial body scroll from `topLeftCell` (content px; 0 when absent). */
  scrollX: number;
  scrollY: number;
}

/** One cell in an extracted range. `covered` marks a merge's non-anchor cell. */
export interface CellFragmentCell {
  /** Formatted `CellModel.text` (no raw value in v1); empty for model-less cells. */
  display: string;
  rowspan: number;
  colspan: number;
  covered: boolean;
}

/** The `extractRange` cells-arm result + the copy-cap signal. */
export interface CellFragment {
  rows: { cells: CellFragmentCell[] }[];
  /** Rows actually emitted (== rows.length). */
  rowCount: number;
  /** Rows in the full requested (merge-expanded) rect. */
  totalRows: number;
  /** True when the cap truncated the walk. */
  truncated: boolean;
  /** Which cap tripped: "rows" (sparse-tall) or "bytes" (dense); null when not truncated. */
  reason: "rows" | "bytes" | null;
}

/**
 * One full-text-search match, joined to its on-page position (the async-seam
 * typing over the wasm `searchPage` primitive). A match
 * is a logical hit (WML: a paragraph's logical text spanning soft-wrapped lines;
 * SML: a cell's model string), carrying one rect per display-line-portion so a
 * multi-line WML match paints as several rects — the same multi-rect shape the
 * comment-highlight core emits (see {@link CommentRect}). `y` is the match's
 * top in layout px within its page, for scroll-to (reuses the bookmark/anchor
 * navigation primitive). `sheet` is the originating sheet name for SML matches
 * (workbook-scope next/prev crosses sheet tabs), or `null` for WML.
 *
 * SML-only navigation fields (full-text search polish): `cellRef` is the matched
 * cell's A1 ref (`"C7"`) — the active-emphasis identity the wasm paint pass keys
 * on (invariant under the continuous grid's scroll-windowed cell slice, unlike a
 * bare enumeration index); `gridX`/`gridY` are the cell's top-left in CONTENT
 * space (gutter-exclusive, pre-zoom), for the grid scroller's scroll-to-match.
 * `null`/`0` for WML & PML, which navigate via `page`/`y` instead.
 */
export interface SearchMatch {
  /** 0-based page index containing the match. */
  page: number;
  /** Highlight rectangles, in layout px (>1 where a match spans display lines). */
  rects: SearchRect[];
  /** Top of the match in layout px within the page, for scroll-to. */
  y: number;
  /** Originating sheet name (SML), or null for Word documents. */
  sheet: string | null;
  /** Matched cell's A1 ref (SML active-emphasis identity), or null for WML/PML. */
  cellRef: string | null;
  /** Cell's content-space (gutter-exclusive, pre-zoom) left, for grid scroll-to. */
  gridX: number;
  /** Cell's content-space (gutter-exclusive, pre-zoom) top, for grid scroll-to. */
  gridY: number;
}

/**
 * Match semantics for {@link Viewer.searchPage} / {@link Viewer.setSearchHighlight}.
 * Absent fields default to case-insensitive substring on the active sheet. For
 * SML, `scope: "workbook"` widens past the active sheet (and `idx`/`page` is then
 * ignored as a sheet selector).
 */
export interface SearchOpts {
  /** Case-sensitive matching (default: case-insensitive). */
  caseSensitive?: boolean;
  /** Whole-word matching (default: substring). */
  wholeWord?: boolean;
  /** SML search scope (default: active sheet). */
  scope?: "sheet" | "workbook";
}

/** Which document format a {@link Viewer} loaded. */
export type DocumentKind = "word" | "excel" | "ppt";

export interface DocumentInfo {
  /** Page size in CSS pixels (uniform across pages in v1, per the document's default sectPr). */
  pageSize: PageSize;
  /** The detected document format. */
  documentKind: DocumentKind;
}

export type ViewerEventMap = {
  pageReady: number;
  loadComplete: number; // total page count
  /**
   * Layout stopped BEFORE the memory wall; payload = pages kept. The
   * document is partially laid out and fully usable up to that page ("showing
   * the first N pages"). Fires AFTER `loadComplete` (whose total is then the
   * truncated count), at most once per load. Not fired on complete layouts.
   */
  layoutTruncated: number;
  loadError: unknown;
  /**
   * The engine panicked and its wasm instance is now dead.
   * Payload is the tagged panic text, including the Rust source location —
   * e.g. `[parasma PANIC] panicked at wml/table.rs:588:48: slice index starts
   * at 1 but ends at 0`.
   *
   * TERMINAL: a wasm panic aborts the instance, so nothing further can be
   * rendered or read from it. This is the last event a Viewer will emit.
   * Fired for worker-mode panics, which are otherwise invisible to the host —
   * the worker's console is unreachable from the main thread, so before this
   * the only signal was `RuntimeError: unreachable`.
   */
  enginePanic: string;
  /** Fired when an embedded image finishes decoding. Payload is the rel id. */
  imageReady: string;
  /**
   * Terminal font-readiness for ONE document load. `count` is the total faces
   * loaded; `loadId` is the per-document monotonic `#loadGeneration`, assigned
   * SYNCHRONOUSLY at `load()` entry (BEFORE the call queues on `#loadChain`), so
   * a queued-but-superseded load is stale from the moment a newer `load()` is
   * called — not only once the newer `#doLoad*` gets its turn on the chain.
   * The `loadId` lets an OBSERVER (plugin-fonts) tell WHICH
   * document this readiness belongs to, so a stale doc-A `fontsReady` cannot
   * resolve doc-B's readiness with the wrong count under rapid doc switches —
   * the same generation guard core uses internally (`#loadGeneration`), surfaced
   * additively for the bridge. Widened from a bare `number` (additive: the only
   * in-repo consumer is plugin-fonts).
   */
  fontsReady: { count: number; loadId: number };
  /**
   * Fired as EACH font family's faces land (additive to the terminal
   * `fontsReady`). The single `fontsReady` only fires after Promise.all, so a
   * slow face (e.g. the ~16MB Noto CJK OTF) withholds it until the slowest
   * arrives — and a consumer that wants to re-render visible pages the instant a
   * specific family is ready has nothing to listen to. This per-family signal is
   * that hook (consumed by plugin-fonts). The await-before-layout ordering inside
   * `#doLoad*` is UNCHANGED — fonts still complete before the initial paint; this
   * is purely additive for incremental re-render.
   */
  "fonts:variant-arrived": { family: string; loadedVariants: number };
  /**
   * A {@link Viewer.refreshFonts} pipeline finished. `changed: true` = font sources were re-resolved AND a fresh
   * layout was kicked off (its `pageReady`/`loadComplete` stream follows);
   * `changed: false` = the semantic no-op gate found nothing to apply (still
   * emitted so an observer, e.g. the dev FONTS tab, re-reads the truth surface
   * — the useLocalFonts header may have flipped even when no face changed).
   * On the `changed: true` path this fires BEFORE the re-layout's first
   * `pageReady`, so a bridge can treat it as the "stale page lifecycle"
   * boundary (plugin-fonts clears the kernel bus replay logs on it).
   */
  "fonts:refreshed": { changed: boolean };
  /**
   * The local-fonts PERMISSION transitioned (PermissionStatus.onchange).
   * Observers (the dev FONTS tab / the snippet's one-click
   * affordance) re-read the truth surface. On a granted→denied/prompt
   * REVOCATION core KICKS the local-sourced Cache-API flush and drops the
   * session catalog; the flush is FIRE-AND-FORGET, so it may
   * still be in flight when this event is observed — the read path is the
   * backstop (readLocalFontCached refuses non-dev-route entries without a
   * live grant, deleting them on contact).
   */
  "fonts:permission-changed": { state: string; previous: string };
};

/** The {@link Viewer.refreshFonts} result. */
export interface RefreshFontsResult {
  /** True iff faces were re-fetched and the document was re-laid out. */
  changed: boolean;
}

// ─── Font resolver ──────────────────────────────────────────────────────────

import { BUNDLED_FONT_ORIGINS, buildFontPlan, type FontPlanEntry } from "./font-resolver";
import {
  LocalFontProvider,
  readLocalFontCached,
  flushLocalFontCacheEntries,
  type LocalFontDeps,
  type LocalFontPermission,
  type MatchTier,
} from "./local-font-provider";
import { computeFontCoverage, type FontCoverage } from "./font-coverage";
import { csvToXlsx, sniffIsCsv } from "./csv";
import { sniffIsPptxBytes, sniffIsXlsxBytes } from "./formats";
export { sniffKind, supportedFormats, UnsupportedFormatError } from "./formats";
export type { SupportedFormat } from "./formats";

// Local-font public types (headless fonts.debug + the snippet button consume them).
export type { LocalFontDeps, LocalFontPermission, MatchTier } from "./local-font-provider";
export { computeFontCoverage, type FontCoverage, type FontCoverageInput } from "./font-coverage";

/** A face about to be fetched, with the engine's default (bundled) URL. */
export interface FontUrlInfo {
  /** The resolved (substitute) family the face belongs to, e.g. "Carlito". */
  family: string;
  variant: "regular" | "bold" | "italic" | "boldItalic";
  weight: number;
  italic: boolean;
  /** Parasma's default source URL for this face (jsDelivr). */
  defaultUrl: string;
  /**
   * The ORIGINAL document font this face substitutes for, e.g. "Calibri" (whose
   * substitute `family` is "Carlito"). Lets a consumer resolver route by the
   * authored font, not just the substitute. Additive / non-breaking.
   */
  documentFamily: string;
}

/**
 * Override where font files are fetched from (consumer's own CDN / CloudFront).
 * Receives the default URL + face info, returns the URL to actually fetch. The
 * engine still decides WHICH faces are needed; the consumer decides WHERE they
 * come from. Configured via `plugin-fonts`' `fontSource`, or directly via
 * `viewer.setFontUrlResolver`. Default (none) = Parasma's bundled jsDelivr URLs.
 */
export type FontUrlResolver = (info: FontUrlInfo) => string;

/**
 * One declared family's TS-side provenance (see {@link Viewer.fontProvenance}).
 * Probe-INDEPENDENT: everything here is main-thread truth (the font plan +
 * origin/freshness + the retained FontFace registry) — no wasm read,
 * so it works identically in worker mode and on probe-OFF builds.
 */
export interface FontProvenanceFamily {
  /** The family name as the document declares it (a fontFamiliesUsed entry). */
  declared: string;
  /** The plan's substitute family actually fetched, or null (engine default). */
  resolved: string | null;
  /** The family Word itself substitutes (e.g. Verdana for DejaVu Sans), if any. */
  wordTarget: string | null;
  /** Regular-face byte-source provenance; survives session-cache hits. */
  origin: "local" | "cdn" | "failed" | null;
  /** Regular face: fetched this load ("fresh") vs session-cache hit ("cached"). */
  freshness: "fresh" | "cached" | null;
  /** Which faces are registered in document.fonts right now (retained refs). */
  loadedVariants: string[];
  /** The ladder tier the regular face's LOCAL bytes matched at, or null. */
  matchTier: MatchTier | null;
  /**
   * The LOCAL family that actually provided the bytes (e.g. "Times New Roman"
   * while `resolved` reads "Tinos" — local bytes register under the plan's
   * resolved name because RunStyle bakes it). null when the bytes are CDN.
   */
  matchedFamily: string | null;
  /** The coverage classification (catalog-fed when one exists). */
  coverage: FontCoverage;
  /** The plan's local search order ([declared, ...curated]; [] for CJK). */
  localCandidates: string[];
}

/** The {@link Viewer.fontProvenance} snapshot. */
export interface FontProvenance {
  useLocalFonts: boolean;
  /** Whether the session local-font catalog has been enumerated. */
  catalogReady: boolean;
  families: FontProvenanceFamily[];
}

// v2: local-font entries are keyed per RESOLVED face (matched family + weight +
// style, with identifying metadata headers) instead of by candidate list — see
// readLocalFontCached. Old-format keys are unreadable by the new reader, so the
// migration is this one-line name bump: the old "parasma-fonts" cache is simply
// orphaned (never opened again) and eventually evicted by the browser. CDN
// URL-keyed entries move too (one extra re-fetch per face, then cached again).
const FONT_CACHE_NAME = "parasma-fonts-v2";

// The OPC byte-sniffers live in ./formats (exported format knowledge); load() keeps its historical routing semantics:
// an unknown ZIP still falls through to the docx parser for the real error.
const sniffIsXlsx = sniffIsXlsxBytes;
const sniffIsPptx = sniffIsPptxBytes;


async function fetchWithCache(url: string): Promise<ArrayBuffer> {
  // The Cache API only admits http(s) requests — `cache.put` THROWS for other
  // schemes. A consumer {@link FontUrlResolver} may legitimately return a
  // blob:/data: URL (e.g. user-supplied font bytes for substitution testing);
  // those fetch directly, uncached — a blob URL's bytes are already local.
  const cacheable = /^https?:/i.test(url);
  if (cacheable && typeof caches !== "undefined") {
    const cache = await caches.open(FONT_CACHE_NAME);
    const cached = await cache.match(url);
    if (cached) return cached.arrayBuffer();

    const response = await fetch(url);
    if (response.ok) {
      await cache.put(url, response.clone());
      return response.arrayBuffer();
    }
    throw new Error(`font fetch failed: ${response.status} ${url}`);
  }
  const response = await fetch(url);
  if (!response.ok) throw new Error(`font fetch failed: ${response.status} ${url}`);
  return response.arrayBuffer();
}

/**
 * One flat per-asset config (`ViewerConfig`). Every field is OPTIONAL
 * and PURELY ADDITIVE: an absent/empty config resolves to today's behavior
 * exactly (co-located wasm, jsDelivr fonts, in-process engine). Resolution
 * precedence per asset: explicit override > `assetBaseUrl`-derived > built-in.
 */
export interface ViewerConfig {
  /** Base URL under which all assets live; per-asset overrides win over it. */
  assetBaseUrl?: string;
  /** The wasm binary source. Response/Module are in-process-only. */
  wasmUrl?: string | URL | InitInput;
  /**
   * Engine transport: absent/false = in-process; true = default same-origin
   * worker; a FUNCTION = the consumer's bundler-emitted worker (first-class
   * cross-origin path); `{ url }` = same-origin convenience; `"auto"` =
   * sniff-and-route at each load (all sniffed formats attach a worker and render
   * off-thread, silent in-process fallback on a worker-boot failure).
   */
  worker?: WorkerSelector;
  /**
   * Worker mode only: end a render worker that sends nothing for this many ms
   * while a request is outstanding (default 30 000; `0` disables). The load in
   * flight fails with a "worker stalled" error and the next load boots a fresh
   * worker. See {@link WorkerTransportOptions.stallTimeoutMs}.
   */
  workerStallTimeoutMs?: number;
  /** Origin/base to fetch font files from (rewrites the jsDelivr default). */
  fontBaseUrl?: string;
  /** Full per-face font URL resolver; wins over {@link fontBaseUrl}. */
  fontResolver?: FontUrlResolver;
  /** Local (installed) font byte-source seam; see {@link Viewer.setLocalFontSource}. */
  localFontSource?: LocalFontDeps | null;
  /** Opt in to matching Word's installed-font substitution. */
  useLocalFonts?: boolean;
  /** Reserved for root-scoping — accepted + stored, not wired here. */
  root?: HTMLElement | ShadowRoot;
  /** Reserved for the coexistence custom-element tag — accepted + stored. */
  elementTag?: string;
}

/** Kept for backward compatibility; {@link ViewerConfig} is the canonical shape. */
export interface ViewerOptions extends ViewerConfig {}

type Handler<E extends keyof ViewerEventMap> = (payload: ViewerEventMap[E]) => void;

/**
 * Framework-agnostic OOXML viewer. Built around the per-page rendering
 * pattern so consumers can virtualize visible pages however they like.
 *
 * INTERNAL NOTE: in worker mode the render + read implementation runs
 * off-thread (worker + display list), but the Viewer's public surface is the same.
 * Every public READ method is async (`Promise`-returning) so the transport
 * (sync in-process vs. worker RPC) is an implementation detail
 * under the seam — callers already `await`. Pure state SETTERS with no wasm read
 * ({@link setFontUrlResolver}, {@link setSearchHighlight}) and lifecycle methods
 * ({@link reset}, {@link destroy}, {@link on}) stay synchronous. The internal
 * `#loadGeneration` is the version guard the worker transport uses to discard stale
 * replies; it is never exposed as a public parameter.
 *
 * `@public`
 */
export class Viewer {
  #wasm: WasmViewer;
  #pageSize: PageSize | null = null;
  #pageCount = 0;
  #loadComplete = false;
  /** Format of the currently-loaded document, or null before first load. */
  #documentKind: DocumentKind | null = null;
  #handlers: { [K in keyof ViewerEventMap]: Set<Handler<K>> } = {
    pageReady: new Set(),
    loadComplete: new Set(),
    layoutTruncated: new Set(),
    loadError: new Set(),
    enginePanic: new Set(),
    imageReady: new Set(),
    fontsReady: new Set(),
    "fonts:variant-arrived": new Set(),
    "fonts:refreshed": new Set(),
    "fonts:permission-changed": new Set(),
  };
  #destroyed = false;
  /** The construction-time config; stored for its root/worker/elementTag
   *  consumers. null = no config = today's defaults. */
  #config: ViewerConfig | null = null;
  /** Consumer font-source override (e.g. their own CDN); null = bundled URLs. */
  #fontUrlResolver: FontUrlResolver | null = null;
  /**
   * Origins this viewer is KNOWN to fetch fonts from — the `preconnect` targets.
   * Bundled defaults by default; the consumer's host when they supply a
   * base URL; EMPTY when a full {@link FontUrlResolver} is installed, because an
   * arbitrary function's output is unknowable and a guessed hint would make a
   * self-hosting consumer pay a handshake to a CDN they never use.
   */
  #fontOrigins: readonly string[] = BUNDLED_FONT_ORIGINS;
  /** Opt-in: read the user's installed fonts to match Word's substitution.
   *  Off by default; toggled via {@link setLocalFontsEnabled}. */
  #useLocalFonts = false;
  #localFontProvider = new LocalFontProvider();
  // Introspection capture for the debugFonts() probe: the CURRENT document's
  // declared font list + its resolved plan + each face's byte source, captured
  // at load so the probe can tell the full declared→resolved→loaded→source
  // story per font the doc actually uses. Cleared/replaced every load.
  #lastFamiliesUsed: string[] = [];
  #lastFontPlan = new Map<string, FontPlanEntry>();
  /**
   * Per-face byte-source PROVENANCE, keyed `${family.toLowerCase()}|${weight}|
   * ${style}` (no intent bit): where the currently-registered bytes came from
   * ("local" installed font / "cdn"), or "failed" when the last attempt could
   * not register the face. Session-lived like {@link #loadedFaces} — a session-
   * cache hit must NOT destroy it (origin and freshness are SEPARATE; see
   * {@link #faceFreshness}). Written only AFTER a load fully succeeds (wasm
   * `loadFont` + FontFace registration), never optimistically.
   */
  #faceOrigin = new Map<string, "local" | "cdn" | "failed">();
  /**
   * Per-LOAD freshness for the same keys: "fresh" = bytes fetched + registered
   * during THIS document's load; "cached" = served by the session cache
   * ({@link #loadedFaces} hit). Reset every load; complements (never replaces)
   * the origin above.
   */
  #faceFreshness = new Map<string, "fresh" | "cached">();
  /**
   * WHICH ladder rung + candidate family the currently-registered
   * LOCAL bytes matched, per descriptor key. Session-lived like
   * {@link #faceOrigin} (a cache hit must not destroy it); written only after
   * a fully-successful local load, deleted when the face goes CDN/failed.
   */
  #faceMatch = new Map<string, { tier: MatchTier; family: string }>();
  /**
   * Intent keys (`local|family|weight|style`) of
   * URL-LESS (local-only) faces whose local read MISSED. A miss leaves the
   * engine default rendering — the correct, settled outcome — so the refresh
   * no-op gate treats these as satisfied; without this a brand-font doc would
   * report changed:true (and abort + re-lay out) on EVERY refresh forever.
   * Cleared on each APPLYING refresh (alongside the catalog invalidation) so a
   * newly-installed font is retried against the fresh catalog.
   */
  #localAttempted = new Set<string>();
  /** Detach for the provider's PermissionStatus.onchange subscription. */
  #permissionUnsub: (() => void) | null = null;
  /**
   * The registered `FontFace` per descriptor `${family.toLowerCase()}|${weight}|
   * ${style}`. Retained so a replacement load (e.g. a useLocalFonts flip) can
   * `document.fonts.delete` the OLD face before adding the new one — two
   * same-descriptor faces must never coexist (the browser would pick one
   * nondeterministically, so paint could diverge from the wasm measure side).
   * Cleared (with deletes) on {@link destroy}.
   */
  #registeredFaces = new Map<string, FontFace>();
  /**
   * SESSION-lived twin of {@link #registeredFaces}, same descriptor key, NOT
   * cleared by {@link #releaseDocument}. `unload()` deletes every registered
   * FontFace from `document.fonts` but must not clear {@link #loadedFaces} (that
   * cache is what makes a warm re-show cheap) — so the next load's session-cache
   * HIT would skip both the fetch and the `document.fonts.add`, leaving the wasm
   * font_cache measuring with metrics the canvas can no longer paint with. Keeping
   * the decoded FontFace here lets the hit path re-`add` the SAME face instead of
   * re-fetching it. Dropped in {@link destroy}.
   */
  #sessionFaces = new Map<string, FontFace>();
  #imageBitmaps = new Map<string, ImageBitmap>();
  /**
   * EMF raster-cache policy sidecar, keyed by the synthetic `"emf-pic:{rel}"`
   * the page paint draws under. The BITMAP itself is NOT stored here — it is
   * registered into {@link #imageBitmaps} (worker replay resolution) AND the
   * wasm map via `#localPaint.setImageBitmap` (in-process CanvasPainter
   * resolution) under the same key, exactly like ordinary raster images, so
   * both paint paths blit it with zero paint-path changes. This map holds only
   * the per-key resolution/lifecycle state {@link planEmfRaster} consults
   * (entries mutated in place — the plan runs per draw per frame). Cleared
   * wherever {@link #imageBitmaps} resets (the three per-format load arms +
   * {@link #releaseDocument}); the bitmaps close with that same reset loop.
   */
  #emfRaster = new Map<string, EmfRasterEntry>();
  /**
   * Retained ORIGINAL embedded image bytes keyed by
   * relId, for the clipboard `<img>` data-URL (untouched originals). `bytes` is
   * already a detached JS-owned copy (wasm `imageRefs()` does
   * `new_with_length` + `copy_from`), so it survives `clearImageBytes()` + wasm
   * memory growth with NO extra copy/slice. Captured at the SAME point as the
   * diagnostics snapshot (BEFORE `clearImageBytes()`); dropped on doc switch +
   * destroy. Unconditional retention in v1. Populated in BOTH modes:
   * in-process from the local wasm, in worker mode from the imageRefs RPC bytes.
   */
  #imageBytes = new Map<string, { mime: string; bytes: Uint8Array }>();
  /**
   * Snapshot of the image references WASM extracted for the current document,
   * captured at extraction time (before `clearImageBytes()` wipes the WASM-side
   * bytes). `imageRefDiagnostics()` reports this so it stays accurate after the
   * background decode settles and clears WASM bytes — querying WASM directly
   * would otherwise always return `[]` once decode finishes.
   */
  #imageRefsSnapshot: Array<{ relId: string; mime: string; byteLength: number }> = [];
  #loadGeneration = 0;
  /**
   * Session-level set of font faces already fully loaded into BOTH the WASM
   * `font_cache` (shaper metrics) AND `document.fonts` (the browser decode) this
   * session, keyed `${family}|${weight}|${style}`. Once a face is in here, a
   * later document that reuses the same family must NOT re-fetch / re-`loadFont`
   * / re-decode it — that redundant re-work is the dominant cost of a same-session
   * doc-switch onto a heavy family (measured ~400ms of FontFace.load + 4 Cache
   * reads for the 16MB Noto CJK faces). Skipping is sound because BOTH sides
   * (measure: wasm font_cache, paint: document.fonts) are already satisfied from
   * the prior load, so the measure==paint invariant still holds. Never cleared
   * (a registered FontFace lives for the page lifetime).
   */
  #loadedFaces = new Set<string>();
  /**
   * Recreate recipe for the CURRENT worker transport, set wherever WE
   * created the worker (createWorkerBacked's selector, the worker:"auto" boot,
   * or the withTransport `recreate` param). null = this viewer cannot rebuild
   * its transport (an injected one with no recipe) → recycling is disabled.
   */
  #workerRecipe: (() => Promise<Transport>) | null = null;
  /**
   * Set when a worker this viewer can rebuild has died: the next load boots a
   * fresh worker from it instead of running in-process ({@link #reviveWorker}).
   */
  #reviveRecipe: (() => Promise<Transport>) | null = null;
  /** In-flight `runLayout` promise from the current document, if any. */
  #layoutPromise: Promise<void> | null = null;
  /**
   * Monotonic layout-kickoff token. `#loadGeneration` alone cannot silence an
   * ABORTED layout's tail when the aborter is `refreshFonts()` (which
   * deliberately does NOT bump the generation): an aborted wasm `run_layout`
   * RESOLVES with its partial page count, and a same-generation tail would emit
   * a spurious `loadComplete(partial)`. Every aborter bumps this BEFORE
   * aborting; every kickoff captures `++#layoutSeq` and its tail/pageReady
   * handlers additionally require `#layoutSeq === mySeq`.
   */
  #layoutSeq = 0;
  /**
   * Tail of the load-serialization chain. Each `load()` queues its work behind
   * the previous one so two loads never interleave their `loadDocx` + layout
   * kickoff. Without this, a rapid document switch re-enters `load()` while the
   * prior call is suspended at `await #loadFonts()` — both then kick off
   * `runLayout` against the same page list and race to append, duplicating
   * pages (see `#doLoad` for why interleaving is unsafe).
   */
  #loadChain: Promise<unknown> = Promise.resolve();
  /**
   * Count of {@link refreshFonts} pipelines currently queued/in flight.
   * >0 ⇒ {@link isRefreshingFonts} is true — surfaced through the headless
   * `fonts.debug` command's top-level `refreshing` field. A counter (not a
   * boolean) so overlapping calls (rapid toggle flips) stay truthful.
   */
  #fontsRefreshing = 0;
  /**
   * The off-thread transport. `null` = in-process (the
   * default + fallback): every engine call runs the local `#wasm` synchronously,
   * byte-identical to before. Non-null = worker-backed: the heavy engine calls
   * (loadDocx/setNow/pageSize/loadFont/runLayout/renderPageDisplayList/searchPage)
   * are RPC'd to the worker's wasm; the main thread keeps orchestration (fonts,
   * image decode, generation guard, emitter) + the cheap GPU replay. The local
   * `#wasm` is unused while `#remote` is set (the doc lives in the worker).
   */
  #remote: Transport | null = null;
  /** Current load's per-page callback, fed by the worker's `pageReady` events. */
  #workerPageReady: ((idx: number) => void) | null = null;

  /**
   * `worker:"auto"` sniff-and-route. `true` IFF constructed
   * via `create({worker:"auto"})`; each `load()` then ensures the worker engine —
   * ALL sniffed formats attach a worker and render off-thread (docx parse+layout,
   * pptx slides, xlsx grid + overlays now record in worker mode). `false` (the
   * default, and for absent/false/true/fn/{url} `worker`)
   * makes load()'s route step a NO-OP so the in-process path is byte-identical to
   * today.
   */
  #autoRoute = false;
  /**
   * Worker-threadable resolved wasm input captured at `create({worker:"auto"})`,
   * threaded into the auto worker boot's `init()` handshake (string | URL only;
   * `undefined` => co-located default, byte-identical to today).
   */
  #autoWasmInput?: string | URL;
  /**
   * Give-up latch: once an auto worker boot has FAILED (CSP/no-worker host, an
   * init hang), don't re-attempt it on every subsequent docx load — that would
   * re-pay the createWorkerTransport boot (up to its timeout) per load. The first
   * failure sticks the viewer to in-process for its lifetime; the doc always
   * renders, just on the main thread.
   */
  #autoWorkerFailed = false;

  /**
   * THE internal engine facade (tier 1). Collapses the former
   * `if(this.#remote) <RPC> else <local #wasm>` dual-branch at ~40 call sites
   * into ONE `this.#engine.m(...)` call. In-process default is an
   * {@link InProcessEngine} over `#wasm`; {@link RemoteEngine} over the transport
   * on `#attachTransport`; swapped back to in-process on worker death. The HOT
   * load path still calls `#wasm.*` INLINE in-process (predicate flips to
   * `capabilities.inProcessDoc`) so it adds ZERO microtask — only the worker arm
   * routes through the facade.
   */
  #engine: EngineFacade;
  /**
   * TIER 2 — the local-paint capability, non-null IFF
   * `#engine.capabilities.localPaint`. In-process this is the same
   * {@link InProcessEngine} instance (it implements both tiers); worker mode is
   * `null` (the Viewer composes the documented fallback — renderPage→replay,
   * chart→false, paintGridViewport→{@link EngineUnsupportedError}).
   */
  #localPaint: LocalPaintEngine | null;

  private constructor(wasm: WasmViewer) {
    this.#wasm = wasm;
    const engine = new InProcessEngine(wasm);
    this.#engine = engine;
    this.#localPaint = engine; // InProcessEngine implements both tiers
    this.#wirePermissionBridge();
  }

  /**
   * Bridge the provider's permission transitions to the viewer event +
   * revocation hygiene: leaving "granted" flushes every
   * LOCAL-sourced Cache-API entry (the bytes are no longer authorized) and
   * drops the session catalog. Re-wired whenever the provider is replaced
   * ({@link setLocalFontSource}); detached on {@link destroy}.
   */
  #wirePermissionBridge(): void {
    this.#permissionUnsub?.();
    this.#permissionUnsub = this.#localFontProvider.onPermissionChange((state, previous) => {
      if (this.#destroyed) return;
      if (previous === "granted" && (state === "denied" || state === "prompt")) {
        this.#localFontProvider.invalidateCatalog();
        if (typeof caches !== "undefined") {
          void caches
            .open(FONT_CACHE_NAME)
            .then((c) => flushLocalFontCacheEntries(c))
            .catch(() => {});
        }
      }
      this.#emit("fonts:permission-changed", { state, previous });
    });
  }

  /**
   * Create a Viewer. Initialises the WASM module on first call. Accepts an
   * OPTIONAL {@link ViewerConfig}: with no config (or an empty one) the resolved
   * wasm input is `undefined` => `init()` no-arg (co-located) and no font sugar
   * applies — byte-identical to today. A truthy `config.worker` delegates to
   * {@link createWorkerBacked}. `@public`
   */
  static async create(config?: ViewerConfig): Promise<Viewer> {
    // `worker:"auto"` is a TRUTHY string, so it must be
    // intercepted BEFORE the `config?.worker` gate — otherwise the truthy gate
    // would wrongly eager-boot it via createWorkerBacked. "auto" constructs
    // IN-PROCESS (the default constructor path, never eager-booting) and defers
    // the worker to the per-load sniff-and-route step (see load() / #ensureEngineFor).
    if (config?.worker === "auto") {
      const resolved = resolveWasmInput(config);
      await ensureWasmReady(resolved);
      const wasm = new WasmViewer();
      warnIfDebugProbeAbsent(wasm);
      const viewer = new Viewer(wasm);
      viewer.#applyConfig(config);
      viewer.#autoRoute = true;
      viewer.#autoWasmInput = workerThreadableWasm(resolved);
      return viewer;
    }
    if (config?.worker) return Viewer.createWorkerBacked(config);
    await ensureWasmReady(resolveWasmInput(config));
    const wasm = new WasmViewer();
    warnIfDebugProbeAbsent(wasm);
    const viewer = new Viewer(wasm);
    viewer.#applyConfig(config);
    return viewer;
  }

  /**
   * Apply the additive config sugar. A null/undefined config leaves every
   * default intact; each field only alters behavior when EXPLICITLY present.
   * Pure orchestration over shipped setters — no engine plumbing.
   *
   * The font-origin warm-up runs on BOTH paths (it needs no config — the
   * bundled origins are the default), so it sits outside the config guard.
   */
  #applyConfig(config?: ViewerConfig): void {
    if (config) {
      this.#config = config; // retain root/elementTag/worker for later consumers
      if (config.fontResolver) this.setFontUrlResolver(config.fontResolver);
      else if (config.fontBaseUrl !== undefined) this.#useFontBase(config.fontBaseUrl);
      else if (config.assetBaseUrl !== undefined) this.#useFontBase(config.assetBaseUrl);
      if (config.localFontSource !== undefined) this.setLocalFontSource(config.localFontSource);
      if (config.useLocalFonts !== undefined) this.setLocalFontsEnabled(config.useLocalFonts);
    }
    this.#warmFontOrigins();
  }

  /**
   * Point font fetches at `base` (the `fontBaseUrl` / `assetBaseUrl` sugar) via
   * the origin-swap resolver, AND record that origin as the `preconnect` target
   * — the two must move together, or a self-hosting consumer would fetch from
   * their host while the hint warmed jsDelivr. A relative or unparseable base
   * leaves the target list empty rather than guessing.
   */
  #useFontBase(base: string): void {
    this.setFontUrlResolver(originSwapResolver(base));
    const docBase = typeof document !== "undefined" ? document.baseURI : undefined;
    try {
      this.#fontOrigins = [new URL(base, docBase).origin];
    } catch {
      this.#fontOrigins = [];
    }
  }

  /**
   * Emit `<link rel="preconnect">` for the origin(s) this viewer will fetch
   * fonts from, at CONSTRUCTION — so DNS + TCP + TLS overlap whatever the app
   * does before `load()` instead of sitting in front of the first font byte on
   * the blocking path to first paint (`#doLoad` awaits `#loadFonts` before
   * `runLayout`). Browser-measured against jsDelivr, Chrome 149, fresh profile
   * per sample, N=3 cold/warm pairs: connection setup 68-86 ms → 0 ms and the
   * first font's TTFB 114-137 ms → 37-47 ms.
   *
   * Purely a network hint: it transfers no bytes and cannot change which faces
   * load, so it is invisible to layout.
   *
   * CAVEAT, deliberate: because it fires at construction, a Viewer that is
   * created and NEVER loads a document still opens one connection to the font
   * origin. That is the cost of overlapping the handshake with app startup —
   * a consumer who cannot accept it should pass `fontBaseUrl` (warms their own
   * host) or a `fontResolver` (warms nothing).
   *
   * Self-hosting is respected rather than broken — see {@link #fontOrigins}: a
   * consumer-supplied base URL is warmed instead of jsDelivr, and a full
   * {@link FontUrlResolver} warms nothing. CAVEAT: the origins are read at
   * construction, so installing a resolver LATER via
   * {@link setFontUrlResolver} leaves one idle connection to the bundled
   * origin. Pass `fontResolver` / `fontBaseUrl` in the {@link ViewerConfig}
   * (the config path) and no such hint is emitted.
   *
   * Idempotent against the LIVE DOM rather than module state, so it also holds
   * across duplicate module instances: at most one node per origin per
   * document, however many viewers are mounted. Same-origin targets are skipped
   * (that connection is already open). The node outlives `destroy()` by design
   * — it carries no per-viewer state and a sibling viewer may still need it.
   */
  #warmFontOrigins(): void {
    if (typeof document === "undefined" || !document.head) return;
    const here = typeof location !== "undefined" ? location.origin : null;
    const existing = new Set(
      [...document.head.querySelectorAll('link[rel="preconnect"]')].map((l) => {
        try {
          return new URL((l as HTMLLinkElement).href).origin;
        } catch {
          return "";
        }
      }),
    );
    for (const origin of this.#fontOrigins) {
      if (origin === here || origin === "null" || existing.has(origin)) continue;
      const link = document.createElement("link");
      link.rel = "preconnect";
      link.href = origin;
      // Fonts are fetched with fetch()'s default (anonymous) credentials mode.
      // An un-flagged preconnect opens a CREDENTIALED socket those requests
      // cannot reuse, so the handshake would simply be paid twice.
      link.crossOrigin = "anonymous";
      document.head.appendChild(link);
      existing.add(origin);
    }
  }

  /**
   * Create a WORKER-BACKED Viewer: the wasm engine runs in a
   * Web Worker so parse / layout / shaping / display-list recording happen OFF
   * the main thread; the main thread orchestrates + replays the display list onto
   * the GPU canvas. Falls back to an in-process {@link create} if the worker can't
   * boot (no module-worker support, init failure) — the in-process path is always
   * the safe default. Worker-backed RENDER is WML display-list only today; use
   * {@link renderPageViaDisplayList}. `@public`
   */
  static async createWorkerBacked(config?: ViewerConfig): Promise<Viewer> {
    // The main thread still holds a local wasm (replay-side helpers, fallback);
    // the worker holds the authoritative engine instance. The resolved wasm
    // input is threaded to BOTH (main here, worker via the boot handshake); with
    // no config it is undefined => init() no-arg on both, byte-identical to today.
    const wasmInput = resolveWasmInput(config);
    await ensureWasmReady(wasmInput);
    const viewer = new Viewer(new WasmViewer());
    viewer.#applyConfig(config);
    try {
      const transport = await createWorkerTransport({
        worker: config?.worker,
        wasmInput: workerThreadableWasm(wasmInput),
        stallTimeoutMs: config?.workerStallTimeoutMs,
      });
      viewer.#attachTransport(transport);
      // Every selector form (true / factory / {url} / default) is
      // re-invocable, so the recycle recipe is just "boot the same way again".
      viewer.#workerRecipe = () =>
        createWorkerTransport({
          worker: config?.worker,
          wasmInput: workerThreadableWasm(wasmInput),
          stallTimeoutMs: config?.workerStallTimeoutMs,
        });
      return viewer;
    } catch (err) {
      // An EXPLICIT worker source (worker: true | () => Worker | { url })
      // that fails to boot FAILS LOUD, so a CSP/CDN/cross-origin misconfig surfaces
      // instead of silently degrading to in-process. Only the batteries-included
      // default path — createWorkerBacked() with no worker selector — keeps the
      // historical silent fallback (byte-identical to pre-config behavior).
      if (config?.worker) throw err;
      console.warn("[parasma] worker boot failed; falling back to in-process:", err);
      return viewer; // #remote stays null → in-process
    }
  }

  /**
   * Build a worker-backed Viewer over an ALREADY-BOOTED {@link Transport}
   * (dependency injection). {@link createWorkerBacked} is the batteries-included
   * form (spawns + `__ping`-handshakes the module Worker, with an in-process
   * fallback); this is the seam for a caller-supplied transport — a custom bridge
   * (SharedWorker, cross-frame) OR a mock-remote transport in worker-mode
   * tests. `localWasm` is the main-thread replay-side helper
   * instance; it is UNUSED for the engine reads (the transport serves those while
   * `#remote` is set), so a test may inject any local stand-in. When omitted a
   * fresh local `WasmViewer` is constructed, so wasm must be ready (see
   * {@link create}). Behaviorally identical to the `#attachTransport` step inside
   * {@link createWorkerBacked}. `@public`
   */
  static withTransport(transport: Transport, localWasm?: WasmViewer, recreate?: () => Promise<Transport>): Viewer {
    const viewer = new Viewer(localWasm ?? new WasmViewer());
    viewer.#attachTransport(transport);
    // A custom bridge MAY supply its own recreation; without one,
    // worker recycling is disabled for this viewer (an injected transport is
    // not ours to rebuild).
    viewer.#workerRecipe = recreate ?? null;
    return viewer;
  }

  /** Wire a transport: route its `pageReady` events into the current load's callback. */
  #attachTransport(transport: Transport): void {
    this.#remote = transport;
    // Swap the tier-1 facade to the worker-backed RemoteEngine and drop the
    // local-paint capability (worker mode composes the replay fallback). The
    // transport LIFECYCLE (pageReady forward, onDead fallback, dispose) stays here
    // in the Viewer's tier-3 state; the facade only issues request/notify.
    this.#engine = new RemoteEngine(transport);
    this.#localPaint = null;
    transport.onEvent((e: TransportEvent) => {
      // Stale-drop with the EXISTING generation guard: a superseded load's
      // trailing pageReady events (e.gen older than the active load) are ignored.
      if (e.event === "pageReady" && e.gen === this.#loadGeneration) {
        this.#workerPageReady?.(e.payload as number);
      }
      // ENGINE PANIC — the wasm panic hook's message, forwarded
      // out of the worker where its console output is unreachable. NOT
      // generation-guarded (gen -1 by construction): a trap is not tied to a
      // load, and dropping it as stale would restore the exact blindness the
      // hook exists to remove. Re-emitted on the main thread so it reaches
      // devtools, and surfaced on the bus so a host can show it — the instance
      // is aborting, so this is the last thing it will ever say.
      if (e.event === "enginePanic") {
        const text = String(e.payload);
        console.error(text);
        this.#emit("enginePanic", text);
      }
    });
    // Worker death (boot OK, then crashed or stalled): drop back to the
    // in-process #wasm so nothing hangs on a dead worker. The in-flight load
    // already rejected (transport rejected its pending RPCs), so #loadChain
    // advances. If this viewer can rebuild its worker, the next load boots a
    // fresh one (#reviveWorker): a document that stalled a worker is likely
    // hostile, and running the next document on the main thread would let the
    // next hostile one freeze the page. worker:"auto" re-boots on its own path.
    transport.onDead(() => {
      if (this.#remote === transport) {
        const recipe = this.#workerRecipe;
        console.warn(
          recipe
            ? "[parasma] render worker died; the next load boots a fresh worker"
            : "[parasma] render worker died; falling back to in-process for future loads",
        );
        // The worker is ALREADY gone (it errored, or the stall watchdog
        // terminated it) — do NOT dispose(); its RPCs are already rejected.
        this.#restoreInProcess(false);
        if (!this.#autoRoute) this.#reviveRecipe = recipe;
      }
    });
  }

  /**
   * Restore the in-process facade + local-paint capability over the local `#wasm`
   * (byte-identical to the default constructor state). Shared by the worker-DEATH
   * path (`dispose=false` — the worker already died, nothing to terminate) and the
   * `worker:"auto"` format-switch DETACH (`dispose=true` — a deliberate detach
   * terminates the live worker, mirroring destroy() :2659).
   */
  #restoreInProcess(dispose: boolean): void {
    if (dispose) this.#remote?.dispose();
    this.#remote = null;
    this.#workerPageReady = null;
    this.#workerRecipe = null; // a recipe without a worker is a stale pointer
    // Restore the in-process facade + local-paint capability so future
    // load()s run against the local #wasm (byte-identical to the default).
    const engine = new InProcessEngine(this.#wasm);
    this.#engine = engine;
    this.#localPaint = engine;
  }

  /**
   * Replace a memory-bloated worker with a fresh one at the document
   * switch, the one seam where wasm memory can actually be RETURNED
   * (`memory.grow` has no inverse; a worker's peak is resident until the
   * worker dies). Runs INSIDE the load chain, before the format dispatch.
   *
   * Policy: recycle when the worker's linear memory ≥ 1 GiB — half the default
   * layout budget, the point where a prior document's peak starts eating
   * meaningfully into the next document's headroom. Below it, the warm worker
   * (compiled module, warm caches) wins. A stale worker bundle reports 0 via
   * the facade degrade → never recycles, the safe direction.
   *
   * IN-PROCESS HAS NO EQUIVALENT, and that is a documented platform limit,
   * not an omission: the in-process wasm memory is module-level, shared by
   * every handle on the page — it can only be returned with a full page-level
   * teardown. docs/memory-architecture.md carries this.
   *
   * The one cross-document cache that lives IN the worker's wasm is the font
   * store: `#loadedFaces` promises "never re-loadFont this session", which
   * becomes a lie the moment the worker is replaced — so it is cleared here,
   * making the next load re-push font BYTES (Cache-API read + memcpy; the
   * expensive FontFace re-decode is still skipped via `#sessionFaces`).
   */
  async #maybeRecycleWorker(): Promise<void> {
    if (!this.#remote || !this.#workerRecipe) return;
    let bytes = 0;
    try {
      bytes = await this.#engine.memoryBytes();
    } catch {
      return; // an unreadable worker is the death path's problem, not ours
    }
    if (bytes < Viewer.#WORKER_RECYCLE_BYTES) return;
    await this.#abortInFlightLayout();
    const recipe = this.#workerRecipe;
    this.#remote.dispose();
    this.#remote = null;
    this.#workerPageReady = null;
    this.#loadedFaces.clear(); // the new worker's font_cache is empty — see doc
    try {
      this.#attachTransport(await recipe());
      this.#workerRecipe = recipe; // attach does not know the recipe; re-arm it
    } catch (err) {
      console.warn("[parasma] worker recycle failed; falling back to in-process:", err);
      this.#restoreInProcess(false); // already disposed — nothing to terminate
      this.#autoWorkerFailed = true; // don't re-attempt a boot that just failed
    }
  }

  /**
   * Boot a replacement for a worker that died (see the onDead handler in
   * {@link #attachTransport}). Runs inside the load chain, before dispatch. A
   * failed boot leaves the viewer in-process, as before this existed.
   */
  async #reviveWorker(): Promise<void> {
    const recipe = this.#reviveRecipe;
    this.#reviveRecipe = null;
    if (!recipe || this.#remote) return;
    try {
      this.#attachTransport(await recipe());
      this.#workerRecipe = recipe; // attach does not know the recipe; re-arm it
      this.#loadedFaces.clear(); // the new worker's font_cache is empty
    } catch (err) {
      console.warn("[parasma] worker re-boot failed; staying in-process:", err);
    }
  }

  /** Recycle threshold: 1 GiB (see {@link #maybeRecycleWorker}). */
  static #WORKER_RECYCLE_BYTES = 1 << 30;

  // ── Engine seam ────────────────────────────────────────────────────────────
  // The load path branches INLINE on `#remote` at each call site rather than
  // through async helpers: an `await` on a resolved promise still costs a
  // microtask, and the in-process default MUST stay tick-for-tick identical (the
  // generation-guard tests pin the load path's microtask timing). So in-process
  // calls `this.#wasm.*` synchronously; only the worker path awaits an RPC. The
  // render/search seams below are off the load-timing path, so they can be async
  // helpers.
  /** Worker-mode runLayout: register the per-page callback (fed by the transport's
   *  pageReady events under the tier-3 generation guard), then RPC via the facade. */
  async #workerRunLayout(onPageReady: (idx: number) => void, gen: number): Promise<number> {
    this.#workerPageReady = onPageReady;
    return this.#engine.runLayout(onPageReady, gen);
  }

  // ── worker:"auto" sniff-and-route ───────────────────────────────────────────
  /**
   * Drain any in-flight layout on the CURRENT engine, used ONLY by
   * {@link #ensureEngineFor} before an engine swap. A DELIBERATE PARALLEL copy of
   * the inline drain block in {@link #doLoad} (and #doLoadXlsx / #doLoadPptx): the
   * three #doLoad* keep their inline drains VERBATIM so the in-process default
   * stays tick-for-tick identical (sharing this helper would add an
   * `await helper()` microtask on a resolved promise and perturb the pinned
   * generation-guard / load-serialization timing). Candidate for an oracle-pinned
   * consolidation once those timing pins relax — keep the two in lock-step.
   */
  async #abortInFlightLayout(): Promise<void> {
    if (this.#layoutPromise) {
      this.#layoutSeq++; // silence the aborted layout's tail (see #layoutSeq)
      if (this.#engine.capabilities.inProcessDoc) this.#wasm.abortLayout();
      else await this.#engine.abortLayout();
      await this.#layoutPromise.catch(() => {});
      this.#layoutPromise = null;
    }
  }

  /**
   * Ensure the desired engine for the current (auto) load. IDEMPOTENT: a no-op
   * when already in the desired mode (zero attach/detach), so a steady-state
   * viewer pays the worker attach at most once (all sniffed formats now attach a
   * worker; the "inprocess" arm survives only as the worker-death degrade path).
   *
   * DRAIN-BEFORE-SWAP (the deep correctness point): the prior layout is aborted on
   * the CURRENT engine BEFORE the swap and `#layoutPromise` nulled, so the
   * following #doLoad*'s own `if (this.#layoutPromise)` drain is a no-op. If we
   * swapped first, #doLoad*'s drain would run against the FRESH engine (which has
   * no layout to abort) while the OLD layout on the other instance was never
   * signalled → `await #layoutPromise` would stall until it finished naturally.
   *
   * A worker BOOT FAILURE degrades SILENTLY to in-process (batteries-included):
   * the catch only warns, leaving `#remote` null → the docx STILL renders via the
   * in-process engine. (Distinct from the EXPLICIT worker:true/fn/{url} path,
   * which fails LOUD in {@link createWorkerBacked}.)
   */
  async #ensureEngineFor(desired: "worker" | "inprocess"): Promise<void> {
    const current: "worker" | "inprocess" = this.#remote ? "worker" : "inprocess";
    if (current === desired) return; // IDEMPOTENT — zero attach/detach
    // Give-up latch: a previously-failed auto boot never re-attempts (no per-load
    // re-timeout on a CSP/no-worker host); the doc renders in-process.
    if (desired === "worker" && this.#autoWorkerFailed) return;
    await this.#abortInFlightLayout(); // drain on the CURRENT engine BEFORE the swap
    if (desired === "worker") {
      try {
        const boot = () =>
          createWorkerTransport({
            worker: undefined,
            wasmInput: this.#autoWasmInput,
            stallTimeoutMs: this.#config?.workerStallTimeoutMs,
          });
        this.#attachTransport(await boot()); // reuse the shipped attach seam
        this.#workerRecipe = boot;
      } catch (err) {
        // SILENT degrade — leave #remote null so the docx renders in-process, and
        // latch so later docx loads skip the doomed re-boot.
        this.#autoWorkerFailed = true;
        console.warn("[parasma] auto worker boot failed; staying in-process:", err);
      }
    } else {
      this.#restoreInProcess(true); // detach + dispose (terminate the worker)
    }
  }

  /**
   * Auto-only load wrapper: ensure the worker engine (all sniffed formats attach
   * a worker), then dispatch the format's #doLoad* through it. Runs INSIDE the
   * #loadChain turn, so the ensure's attach/detach is serialized with the load
   * like every other switch.
   */
  async #routeThenLoad(isXlsx: boolean, isPptx: boolean, bytes: Uint8Array, gen: number): Promise<DocumentInfo> {
    // ALL formats render off-thread now — docx, pptx, AND xlsx (grid
    // overlays recorded in worker mode). Route every load to the worker.
    // isXlsx/isPptx no longer drive ROUTING — they only select the #doLoad* below.
    await this.#ensureEngineFor("worker");
    await this.#maybeRecycleWorker(); // the doc switch is the seam
    return isXlsx ? this.#doLoadXlsx(bytes, gen) : isPptx ? this.#doLoadPptx(bytes, gen) : this.#doLoad(bytes, gen);
  }

  /**
   * Parse + start laying out a DOCX. Resolves as soon as parsing is done
   * (typically tens of milliseconds). Layout and image decoding both stream
   * in the background; subscribe to `pageReady` / `loadComplete` events to
   * observe progress.
   *
   * Accepts `ArrayBuffer` (from `fetch` / `<input type=file>`) or a
   * `Uint8Array`. The bytes are copied into WASM memory.
   *
   * `@public`
   */
  async load(data: ArrayBuffer | Uint8Array): Promise<DocumentInfo> {
    this.#assertAlive();
    const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);

    // Serialize against any in-flight load. `#doLoad` has interior `await`
    // points (font fetching); if a second `load()` ran while the first was
    // suspended there, both would call `loadDocx` + `runLayout` against the
    // shared page list and race to append — duplicating pages on a rapid
    // document switch. Chaining keeps each load's critical section atomic.
    // We swallow the previous load's error on the chain (its own caller already
    // received the rejection) so one failed load doesn't poison later switches.
    // Branch on the detected format. docx and xlsx are
    // both ZIP packages; the distinguishing part name (`xl/workbook.xml` vs
    // `word/document.xml`) appears verbatim in the ZIP's (uncompressed) file
    // headers, so a substring sniff classifies without unzipping.
    // CSV routes through the browser-side converter into the same xlsx path
    // (Excel-style import: the engine needs zero CSV knowledge).
    // Claim this load's generation SYNCHRONOUSLY, before queueing on the chain.
    // Loads are SERIALIZED but a superseded load still RUNS (its #doLoad* gets
    // its chain turn before the newer one aborts it) — if the generation were
    // bumped inside #doLoad*, a superseded load would look "current" for its
    // whole turn and its tail emissions (fontsReady / fonts:variant-arrived /
    // pageReady) would leak into the NEWER document's observers (plugin-fonts
    // saw a stale doc's fontsReady resolve the new doc's readiness arm).
    // Claiming at entry makes stale() true for the superseded load the moment
    // the newer load() is CALLED.
    const gen = ++this.#loadGeneration;
    let loadBytes = bytes;
    let isXlsx = sniffIsXlsx(loadBytes);
    const isPptx = !isXlsx && sniffIsPptx(loadBytes);
    if (!isXlsx && !isPptx && sniffIsCsv(loadBytes)) {
      loadBytes = csvToXlsx(new TextDecoder().decode(loadBytes));
      isXlsx = true;
    }
    // worker:"auto" — a SYNCHRONOUS `#autoRoute` gate INSIDE
    // the chain callback. When false (the default, absent/false/true/fn/{url}
    // worker), the else arm is the EXACT current dispatch expression — no route,
    // no await, no added microtask — so the in-process default is byte-identical.
    // The worker arm may recycle a bloated worker before dispatch;
    // the IN-PROCESS arm calls dispatch() synchronously inside the chain
    // callback, adding zero microtasks — the pinned load-timing contract.
    const dispatch = () =>
      isXlsx
        ? this.#doLoadXlsx(loadBytes, gen)
        : isPptx
          ? this.#doLoadPptx(loadBytes, gen)
          : this.#doLoad(loadBytes, gen);
    const run = this.#loadChain.then(() =>
      this.#autoRoute
        ? this.#routeThenLoad(isXlsx, isPptx, loadBytes, gen)
        : this.#remote
          ? this.#maybeRecycleWorker().then(dispatch)
          : this.#reviveRecipe
            ? this.#reviveWorker().then(dispatch)
            : dispatch(),
    );
    this.#loadChain = run.catch(() => {});
    return run;
  }

  /**
   * One spreadsheet's load: abort any prior layout, swap in the
   * workbook via `loadXlsx`, ensure the normal-style font (Calibri) is loaded so
   * cell text shapes, and kick off streaming grid pagination. Mirrors `#doLoad`'s
   * abort/generation/streaming contract; the WML-only steps (field dates, image
   * decode, per-run font planning) are skipped — the SML tracer paints cell text
   * in the single normal font.
   */
  async #doLoadXlsx(bytes: Uint8Array, gen: number): Promise<DocumentInfo> {
    this.#assertAlive();

    for (const bitmap of this.#imageBitmaps.values()) bitmap.close?.();
    this.#imageBitmaps.clear();
    // EMF raster sidecar: entries are stale for the new doc/teardown; their
    // bitmaps live in #imageBitmaps under "emf-pic:" keys and were closed by
    // the loop above. In-flight rasterizeEmf resolutions drop via the
    // generation guard in #rasterizeEmf.
    this.#emfRaster.clear();
    this.#imageBytes.clear(); // drop retained originals on doc switch/destroy
    this.#imageRefsSnapshot = [];

    if (this.#layoutPromise) {
      this.#layoutSeq++; // silence the aborted layout's tail (see #layoutSeq)
      if (this.#engine.capabilities.inProcessDoc) this.#wasm.abortLayout();
      else await this.#engine.abortLayout();
      await this.#layoutPromise.catch(() => {});
      this.#layoutPromise = null;
    }

    // Route the workbook load through the facade so worker mode CAN load it
    // (kills the historical #wasm.loadXlsx bypass). In-process stays synchronous.
    if (this.#engine.capabilities.inProcessDoc) this.#wasm.loadXlsx(bytes);
    else await this.#engine.loadXlsx(bytes);
    this.#pageCount = 0;
    this.#loadComplete = false;
    this.#pageSize = this.#engine.capabilities.inProcessDoc
      ? (this.#wasm.pageSize(0) as PageSize)
      : await this.#engine.pageSize(0);

    // Cell text uses the workbook's normal-style font (e.g. Calibri). Load it
    // through the same font-plan + alias machinery the WML path uses so the
    // vendored substitute (Carlito/Arimo) shapes and renders the digits/text.
    // fontFamiliesUsed now has an SML arm (workbook stylesheet fonts), so the
    // generic plan path applies — no more hardcoded normal font.
    await this.#loadFonts(gen);

    // Decode anchored-picture bytes: the wasm collected each drawing's
    // media bytes at load (imageRefs → createImageBitmap → setImageBitmap).
    // Without this kickoff the bytes never become ImageBitmaps and sheet
    // pictures paint the placeholder forever. Mirrors #doLoadPptx exactly.
    this.#decodeImagesInBackground(gen);

    const myGeneration = gen;
    // Generation ALONE cannot silence this tail: refreshFonts() aborts the
    // layout WITHOUT bumping #loadGeneration (same document), and an aborted
    // wasm run_layout RESOLVES with its partial page count — so a
    // generation-only guard publishes loadComplete(partial). Same #layoutSeq
    // token #doLoad uses.
    const mySeq = ++this.#layoutSeq;
    const live = () => this.#loadGeneration === myGeneration && this.#layoutSeq === mySeq;
    const onPageReady = (idx: number) => {
      if (!live()) return;
      this.#pageCount = Math.max(this.#pageCount, idx + 1);
      this.#emit("pageReady", idx);
    };
    this.#layoutPromise = (
      this.#engine.capabilities.inProcessDoc
        ? this.#wasm.runLayout(onPageReady)
        : this.#workerRunLayout(onPageReady, myGeneration)
    )
      .then((total: number) => {
        if (!live()) return;
        this.#pageCount = total;
        this.#loadComplete = true;
        this.#emit("loadComplete", total);
      })
      .catch((err: unknown) => {
        if (!live()) return;
        this.#emit("loadError", err);
        console.error("[parasma] layout failed:", err);
      });

    this.#documentKind = "excel";
    return { pageSize: this.#pageSize, documentKind: "excel" };
  }

  /**
   * One presentation's load: abort any prior layout, swap in the
   * deck via `loadPptx`, plan fonts, and kick off streaming slide pagination
   * (one slide = one print page). Mirrors `#doLoadXlsx`'s abort/generation/
   * streaming contract exactly — only the wasm load entry and the reported kind
   * differ.
   */
  async #doLoadPptx(bytes: Uint8Array, gen: number): Promise<DocumentInfo> {
    this.#assertAlive();

    for (const bitmap of this.#imageBitmaps.values()) bitmap.close?.();
    this.#imageBitmaps.clear();
    // EMF raster sidecar: entries are stale for the new doc/teardown; their
    // bitmaps live in #imageBitmaps under "emf-pic:" keys and were closed by
    // the loop above. In-flight rasterizeEmf resolutions drop via the
    // generation guard in #rasterizeEmf.
    this.#emfRaster.clear();
    this.#imageBytes.clear(); // drop retained originals on doc switch/destroy
    this.#imageRefsSnapshot = [];

    if (this.#layoutPromise) {
      this.#layoutSeq++; // silence the aborted layout's tail (see #layoutSeq)
      if (this.#engine.capabilities.inProcessDoc) this.#wasm.abortLayout();
      else await this.#engine.abortLayout();
      await this.#layoutPromise.catch(() => {});
      this.#layoutPromise = null;
    }

    // Route the deck load through the facade so worker mode CAN load it
    // (kills the historical #wasm.loadPptx bypass). In-process stays synchronous.
    if (this.#engine.capabilities.inProcessDoc) this.#wasm.loadPptx(bytes);
    else await this.#engine.loadPptx(bytes);
    this.#pageCount = 0;
    this.#loadComplete = false;
    this.#pageSize = this.#engine.capabilities.inProcessDoc
      ? (this.#wasm.pageSize(0) as PageSize)
      : await this.#engine.pageSize(0);

    // Slide text uses the deck's run fonts; the generic font plan applies
    // via the PML arm of fontFamiliesUsed.
    await this.#loadFonts(gen);

    // Decode slide picture bytes: the wasm collected each picture's media
    // bytes at load (imageRefs → createImageBitmap → setImageBitmap). Without this
    // call the bytes never become ImageBitmaps and pictures never paint. Mirrors
    // #doLoad's decode kickoff.
    this.#decodeImagesInBackground(gen);

    const myGeneration = gen;
    // See #doLoadXlsx — the same #layoutSeq token, for the same reason: an
    // aborted run_layout resolves with a PARTIAL count and refreshFonts()
    // deliberately keeps the generation.
    const mySeq = ++this.#layoutSeq;
    const live = () => this.#loadGeneration === myGeneration && this.#layoutSeq === mySeq;
    const onPageReady = (idx: number) => {
      if (!live()) return;
      this.#pageCount = Math.max(this.#pageCount, idx + 1);
      this.#emit("pageReady", idx);
    };
    this.#layoutPromise = (
      this.#engine.capabilities.inProcessDoc
        ? this.#wasm.runLayout(onPageReady)
        : this.#workerRunLayout(onPageReady, myGeneration)
    )
      .then((total: number) => {
        if (!live()) return;
        this.#pageCount = total;
        this.#loadComplete = true;
        this.#emit("loadComplete", total);
      })
      .catch((err: unknown) => {
        if (!live()) return;
        this.#emit("loadError", err);
        console.error("[parasma] layout failed:", err);
      });

    this.#documentKind = "ppt";
    return { pageSize: this.#pageSize, documentKind: "ppt" };
  }

  /**
   * One document's load: abort the previous layout, swap in the new document,
   * load its fonts, and kick off streaming layout. Must run to completion
   * before the next `#doLoad` begins — `load()` enforces that ordering.
   */
  async #doLoad(bytes: Uint8Array, gen: number): Promise<DocumentInfo> {
    this.#assertAlive();

    // `gen` was claimed at load() entry (see load()); prior loads' in-flight
    // decode/font callbacks are already stale. Close GPU-backed bitmaps from
    // the previous doc.
    for (const bitmap of this.#imageBitmaps.values()) bitmap.close?.();
    this.#imageBitmaps.clear();
    // EMF raster sidecar: entries are stale for the new doc/teardown; their
    // bitmaps live in #imageBitmaps under "emf-pic:" keys and were closed by
    // the loop above. In-flight rasterizeEmf resolutions drop via the
    // generation guard in #rasterizeEmf.
    this.#emfRaster.clear();
    this.#imageBytes.clear(); // drop retained originals on doc switch/destroy
    this.#imageRefsSnapshot = [];

    // A prior document's `runLayout` future holds the Viewer's wasm-bindgen
    // borrow across its `await` yields; calling `loadDocx` (&mut self) while it
    // is alive panics ("recursive use of an object"). Signal it to abort at its
    // next page boundary and wait for it to unwind before touching WASM state.
    if (this.#layoutPromise) {
      this.#layoutSeq++; // silence the aborted layout's tail (see #layoutSeq)
      // The predicate is the EXPLICIT capability; the in-process arm stays
      // the SYNCHRONOUS #wasm.* call (zero added microtask — the generation-guard
      // tests pin this ordering); only the worker arm awaits the facade RPC.
      if (this.#engine.capabilities.inProcessDoc) this.#wasm.abortLayout();
      else await this.#engine.abortLayout();
      await this.#layoutPromise.catch(() => {});
      this.#layoutPromise = null;
    }

    if (this.#engine.capabilities.inProcessDoc) this.#wasm.loadDocx(bytes);
    else await this.#engine.loadDocx(bytes);
    // Supply the current time for DATE/TIME fields — a viewer behaves like Word
    // opening the doc: DATE reflects "now". CREATEDATE/SAVEDATE use the doc's own
    // core-property dates (handled in WASM).
    const now = Date.now();
    if (this.#engine.capabilities.inProcessDoc) this.#wasm.setNow(now);
    else await this.#engine.setNow(now);
    this.#pageCount = 0;
    this.#loadComplete = false;
    this.#pageSize = this.#engine.capabilities.inProcessDoc
      ? (this.#wasm.pageSize(0) as PageSize)
      : await this.#engine.pageSize(0);

    await this.#loadFonts(gen);

    this.#decodeImagesInBackground(gen);

    // Kick off background layout. Returns control to the caller immediately
    // (we do NOT await this Promise). The runLayout async fn yields between
    // pages so the browser stays responsive even for huge documents.
    const myGeneration = gen;
    const mySeq = ++this.#layoutSeq;
    const live = () => this.#loadGeneration === myGeneration && this.#layoutSeq === mySeq;
    const onPageReady = (idx: number) => {
      // Ignore stray callbacks from a prior, now-superseded layout that is
      // still unwinding after abortLayout() — without this guard its trailing
      // pageReady calls corrupt the new document's page count.
      if (!live()) return;
      this.#pageCount = Math.max(this.#pageCount, idx + 1);
      this.#emit("pageReady", idx);
    };
    this.#layoutPromise = (
      this.#engine.capabilities.inProcessDoc
        ? this.#wasm.runLayout(onPageReady)
        : this.#workerRunLayout(onPageReady, myGeneration)
    )
      .then(async (total: number) => {
        // Ignore the result of a layout that was superseded by a newer load OR
        // aborted by a same-generation refreshFonts (an aborted run_layout
        // RESOLVES with its partial page count — see #layoutSeq).
        if (!live()) return;
        this.#pageCount = total;
        this.#loadComplete = true;
        this.#emit("loadComplete", total);
        // Read the truncation stamp AFTER completion (the wasm sets it
        // before resolving). In-process reads inline like runLayout above;
        // worker mode RPCs with a stale-bundle degrade to null. A doc-switch
        // abort also resolves this promise, but its stamp is None (truncation
        // stamps only at the budget/alloc checks) and live() has gone false
        // anyway — both guards would have to fail for a false banner.
        // typeof-guarded like the worker arm's #tryReq degrade: a stale wasm
        // artifact OR a consumer's test double without the export must read as
        // "layout complete", never throw inside the load tail (an unguarded
        // version of this line broke a test double).
        const truncatedAt = this.#engine.capabilities.inProcessDoc
          ? typeof this.#wasm.layoutTruncatedAt === "function"
            ? ((this.#wasm.layoutTruncatedAt() as number | undefined) ?? null)
            : null
          : await this.#engine.layoutTruncatedAt();
        if (!live() || truncatedAt === null) return;
        this.#emit("layoutTruncated", truncatedAt);
      })
      .catch((err: unknown) => {
        if (!live()) return;
        this.#emit("loadError", err);
        console.error("[parasma] layout failed:", err);
      });

    this.#documentKind = "word";
    return { pageSize: this.#pageSize, documentKind: "word" };
  }

  /**
   * Subscribe to a viewer event. Returns an unsubscribe function.
   *
   * Catch-up emission: if `pageReady` subscribers attach *after* pages have
   * already been laid out, the handler fires immediately for each existing
   * page (in order). Same for `loadComplete` if layout is already done.
   * This makes subscription-order race conditions impossible.
   *
   * Stays SYNC: a DOM-style subscription (returns the unsubscribe
   * function), not a wasm read. `@public`
   */
  on<E extends keyof ViewerEventMap>(event: E, handler: Handler<E>): () => void {
    (this.#handlers[event] as Set<Handler<E>>).add(handler);

    if (event === "pageReady") {
      const h = handler as Handler<"pageReady">;
      for (let i = 0; i < this.#pageCount; i++) {
        try {
          h(i);
        } catch (e) {
          console.error("[parasma] catch-up pageReady handler threw:", e);
        }
      }
    } else if (event === "loadComplete" && this.#loadComplete) {
      try {
        (handler as Handler<"loadComplete">)(this.#pageCount);
      } catch (e) {
        console.error("[parasma] catch-up loadComplete handler threw:", e);
      }
    }

    return () => {
      (this.#handlers[event] as Set<Handler<E>>).delete(handler);
    };
  }

  #emit<E extends keyof ViewerEventMap>(event: E, payload: ViewerEventMap[E]): void {
    for (const h of this.#handlers[event] as Set<Handler<E>>) {
      try {
        h(payload);
      } catch (e) {
        console.error(`[parasma] handler for ${event} threw:`, e);
      }
    }
  }

  /**
   * Total page count laid out so far. Grows during streaming layout.
   *
   * Worker-ready async seam: an async method rather than a getter, so the read
   * is the same in worker mode — the value is cached state and resolves
   * immediately. `@public`
   */
  async pageCount(): Promise<number> {
    return this.#pageCount;
  }

  /**
   * The loaded document's kind ("word" | "excel" | "ppt"), or `null` before a
   * load settles. SYNC (cached load state — no engine read). The ONE
   * per-format interaction semantic plugin-selection keeps (a background
   * click clears the selection on a SLIDE but preserves it on a page — Word
   * vs PowerPoint convention) reads this; verb routing never does. `@public`
   */
  get documentKind(): DocumentKind | null {
    return this.#documentKind;
  }

  /**
   * True once `loadComplete` has fired.
   *
   * Worker-ready async seam: an async method rather than a getter; reads cached
   * state and resolves immediately. `@public`
   */
  async isLoadComplete(): Promise<boolean> {
    return this.#loadComplete;
  }

  /**
   * Formula-bar readout (spreadsheets): the cell at a page coordinate, or null.
   * `x`/`y` are page logical px (the same space `renderPage` paints).
   *
   * Worker-ready async seam: wraps the synchronous wasm primitive and
   * resolves immediately in-process; in worker mode the transport sits
   * UNDER this seam, leaving callers (which already `await`) unchanged. Returns
   * null for the docx-only wasm build (no `cellAt` export). `@public`
   */
  async cellAt(page: number, x: number, y: number): Promise<{ ref: string; raw: string; display: string } | null> {
    this.#assertAlive();
    return this.#engine.cellAt(page, x, y);
  }

  /**
   * Per-sheet → page mapping for spreadsheet sheet-tab UI. Returns one
   * entry per non-empty sheet, in document order: `{ name, firstPage,
   * pageCount }` where pages `[firstPage, firstPage + pageCount)` of the
   * streamed page list belong to that sheet. Empty `[]` for the docx-only wasm
   * build (no `sheetInfo` export) or a non-spreadsheet document.
   *
   * Worker-ready async seam: wraps the synchronous wasm primitive, resolving
   * immediately in-process; the worker transport sits UNDER this seam without
   * touching callers. `@public`
   */
  async sheetInfo(): Promise<{ name: string; firstPage: number; pageCount: number; maxRow: number }[]> {
    this.#assertAlive();
    // Do NOT `await` a SYNC in-process result — awaiting even a resolved value
    // costs a microtask, and plugin-grid's `sheets:changed` resetup race pins the
    // pre-collapse tick count. In-process the facade returns the array directly
    // (the single async wrapper = 1 tick, byte-identical); worker mode awaits RPC.
    const r = this.#engine.sheetInfo();
    const raw = r instanceof Promise ? await r : r;
    // maxRow defaults to 0 for an older wasm build (→ the plugin treats the
    // sheet as a single chunk, i.e. the older per-sheet behaviour).
    return raw.map((s) => ({ ...s, maxRow: s.maxRow ?? 0 }));
  }

  /**
   * Continuous grid view: a sheet's full extent (incl. gutters) for the scroll
   * spacer, or null for non-spreadsheets / docx-only builds.
   *
   * Worker-ready async seam: wraps the synchronous wasm primitive,
   * resolving immediately in-process. `@public`
   */
  async gridExtent(sheet: number): Promise<{ width: number; height: number } | null> {
    this.#assertAlive();
    return this.#engine.gridExtent(sheet);
  }

  /**
   * Paint one scroll-window of a sheet's continuous grid (virtual scroller).
   * `zoom`: `scrollX`/`scrollY` are CONTENT-space px,
   * `viewW`/`viewH` CSS px; the engine paints through one uniform scale.
   *
   * Worker-ready async seam: the paint runs synchronously in-process (the
   * Promise resolves immediately); in worker mode the off-thread paint goes
   * UNDER this seam, leaving the rAF-coalesced caller — which `await`s — the
   * same. `@public`
   */
  async paintGridViewport(sheet: number, canvas: HTMLCanvasElement, scrollX: number, scrollY: number, viewW: number, viewH: number, zoom = 1): Promise<void> {
    this.#assertAlive();
    // In-process: the TIER-2 fast path — direct-to-canvas wasm paint, byte-
    // identical to before, keeping the docx-only typeof guard inside.
    if (this.#localPaint) {
      this.#localPaint.paintGridViewport(sheet, canvas, scrollX, scrollY, viewW, viewH, zoom);
      return;
    }
    // Worker mode: RECORD the viewport off-thread + REPLAY it here, the
    // compose fallback mirroring `renderPageWindow`. The recorded stream is
    // content-space CSS px (scroll baked, zoom-INDEPENDENT); the ambient
    // scale(dpr) the grid adapter set once on this ctx PERSISTS across scroll
    // paints, so net = ambient(dpr)·ctx.scale(zoom)·content — IDENTICAL to the
    // in-process shim. Do NOT bake zoom into the Rust stream or it double-applies.
    // Dev-only scroll-latency probe (?gridperf): splits the frame cost into the
    // off-thread RECORD RPC (t1−t0, an UPPER bound — the await also yields to the
    // microtask/rAF queue) and the SOLE main-thread cost, REPLAY (t2−t1). Behind
    // a global hook, worker-branch only → in-process + release are byte-neutral.
    const probe = (globalThis as { __gridPerf?: (s: GridPerfSample) => void }).__gridPerf;
    const t0 = probe ? performance.now() : 0;
    const enc = await this.#engine.recordGridViewport(sheet, scrollX, scrollY, viewW, viewH, zoom);
    const t1 = probe ? performance.now() : 0;
    const ctx = canvas.getContext("2d", { alpha: false });
    if (!ctx) throw new Error("paintGridViewport: no 2d context");
    ctx.save();
    ctx.scale(zoom, zoom);
    replayDisplayList(ctx, enc.opcodes, enc.strings, this.#imageBitmaps);
    ctx.restore();
    if (probe) {
      const t2 = performance.now();
      probe({ rpc: t1 - t0, replay: t2 - t1, total: t2 - t0, ops: enc.opcodes.length, sy: scrollY });
    }
  }

  /**
   * Formula-bar hit-test in the continuous grid view.
   *
   * Worker-ready async seam: wraps the synchronous wasm primitive,
   * resolving immediately in-process. `@public`
   */
  async gridCellAt(sheet: number, x: number, y: number, scrollX: number, scrollY: number): Promise<{ ref: string; raw: string; display: string } | null> {
    this.#assertAlive();
    return this.#engine.gridCellAt(sheet, x, y, scrollX, scrollY);
  }

  /**
   * The USED-EXTENT box a header click selects (keyed off
   * `content_bounds`, the tight cell-bearing box — NOT the phantom stamped extent).
   * The grid adapter turns a column-header click at col C into
   * `{r0: extent.r0, c0: C, r1: extent.r1, c1: C}` (and the row-header dual). `null`
   * for a non-SML / stale build. `@public`
   */
  async gridContentBounds(sheet: number): Promise<CellRange | null> {
    this.#assertAlive();
    return this.#engine.gridContentBounds(sheet);
  }

  /**
   * Frozen panes (§18.3.1.66, grid view) — a sheet's pane info for the grid
   * adapter: pinned-band sizes in CONTENT px + the `topLeftCell`-derived initial
   * body scroll. `null` for pane-less sheets, `state="split"` (ignored by
   * design), non-SML docs, and stale builds. WORKER-READY async seam: `#remote`
   * RPC (a structured-clone-safe plain object riding the generic dispatch) else
   * the sync wasm export. `@public`
   */
  async gridPaneInfo(sheet: number): Promise<GridPaneInfo | null> {
    this.#assertAlive();
    return this.#engine.gridPaneInfo(sheet);
  }

  /**
   * Page dimensions for page `idx` (§17.6). Per-page: a mixed-size
   * multi-section doc reports each section's own geometry (a landscape section
   * is wider than tall). A doc whose pages all share a size returns the same
   * dims for every idx. `idx` defaults to 0, whose size is also cached on
   * {@link DocumentInfo.pageSize}. Throws if no document is loaded.
   *
   * Worker-ready async seam: page 0 resolves from cached state;
   * other indices route through the engine/wasm (the adapter caches per-idx).
   * `@public`
   */
  async pageSize(idx = 0): Promise<PageSize> {
    if (!this.#pageSize) throw new Error("no document loaded");
    if (idx === 0) return this.#pageSize;
    return this.#engine.capabilities.inProcessDoc
      ? (this.#wasm.pageSize(idx) as PageSize)
      : await this.#engine.pageSize(idx);
  }

  /**
   * Render page `idx` into the supplied canvas. The canvas's *backing store*
   * dimensions should be (pageSize × devicePixelRatio); the consumer is
   * responsible for the DPR setup.
   *
   * Synchronous in-process — the async wrapper keeps the contract the same for
   * lazy / streamed page layout (the worker transport goes under this seam).
   *
   * `@public`
   */
  async renderPage(idx: number, canvas: HTMLCanvasElement): Promise<void> {
    this.#assertAlive();
    // TIER 2 — transparent fallback (NEVER throws): the local-paint fast path
    // when available (in-process: the verbatim #wasm.renderPage), else record the
    // page as a display list and replay it (exactly how worker mode paints today).
    if (this.#localPaint) {
      this.#localPaint.renderPage(idx, canvas);
      // EMF raster cache — drain the paint's requests SYNCHRONOUSLY (the wasm
      // request log is thread-shared; the sync set_active(clear)→paint→drain
      // window is the only interleaving guarantee — no await before this).
      // renderPage applies NO Viewer-side transform (the ADAPTER pre-scales the
      // ctx: base layer ctx.scale, dev-shell dpr scale), so the page-logical →
      // device-px scale is read back off the ctx transform. Identity/unreadable
      // transform ⇒ scale 1 (a bare consumer painting logical px 1:1).
      this.#drainEmfPaintRequests(this.#ctxScale(canvas));
      return;
    }
    await this.renderPageViaDisplayList(idx, canvas);
  }

  /**
   * Render a page via the DISPLAY-LIST path: ask the engine
   * to RECORD the page as a flat draw-command list, then REPLAY it onto the
   * canvas's 2D context on this (main) thread. Pixel-equivalent to
   * {@link renderPage} — it runs the identical paint, only the draw calls travel
   * as data and are replayed here instead of issued in-engine. This is the seam
   * the worker slots under: the worker produces the list, the main thread
   * replays it (GPU + `document.fonts` stay here, no font fork). WML-only today
   * (the engine export rejects SML/PML); paints page-logical px at 1:1 (the
   * caller owns any device-pixel pre-transform, mirroring {@link renderPage}).
   *
   * `@public`
   */
  async renderPageViaDisplayList(idx: number, canvas: HTMLCanvasElement): Promise<void> {
    this.#assertAlive();
    const enc = await this.#engine.recordPage(idx);
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("renderPageViaDisplayList: no 2d context");
    // EMF raster cache — the full-page replay arm (worker-mode renderPage and
    // any headless consumer that never windowed-paints) must also feed the
    // raster funnel, or such a consumer would keep the placeholder forever.
    // The caller owns the ctx pre-transform, so the page-logical → device-px
    // scale is an APPROXIMATE readback of that transform (identity ⇒ 1) — a
    // wrong-but-recent scale self-corrects on the next paint.
    const scale = this.#ctxScale(canvas);
    replayDisplayList(ctx, enc.opcodes, enc.strings, this.#imageBitmaps, (key, dw, dh) =>
      this.#noteEmfDraw(key, dw * scale, dh * scale),
    );
  }

  /**
   * Windowed render: paint ONLY the items intersecting the visible
   * viewport, so render cost is O(visible) instead of O(whole page). Mirrors
   * {@link renderPage} but bounds the item walk by a clip rect.
   *
   * `clipX`/`clipY` are the page-logical-px origin of the visible slice;
   * `viewW`/`viewH` are the visible slice in CSS px; `zoom` is the scale
   * (page-logical → CSS px) and `dpr` the device-pixel-ratio. Unlike
   * `renderPage`, render_page_window sets its OWN ctx transform (scale·dpr,
   * then translate by −clip) and clips to the window rect — the caller must NOT
   * pre-transform the ctx (only the white pre-fill is the caller's job).
   *
   * The window in page-logical px is `{ x: clipX, y: clipY, w: viewW/zoom,
   * h: viewH/zoom }`. Output inside the clip is byte-identical to renderPage;
   * items outside the window are culled (bbox ∩ window) and never painted.
   *
   * Resolves `true` when the render produced content, `false` when the worker
   * recorded an EMPTY display list (drew nothing) — the caller's out-of-band
   * emptiness signal, replacing the adapter's old getImageData sentinel probes
   * (a 1×1 readback forces a full synchronous raster flush: 277ms cold /
   * 3-16ms warm per commit — the zoom-shake judder). The in-process path
   * paints same-task through the window-local coordinates and always reports
   * `true`.
   *
   * `@public`
   */
  async renderPageWindow(
    idx: number,
    canvas: HTMLCanvasElement,
    clipX: number,
    clipY: number,
    viewW: number,
    viewH: number,
    zoom: number,
    dpr: number,
  ): Promise<boolean> {
    this.#assertAlive();
    // TIER 2 — local-paint fast path (in-process: the verbatim
    // #wasm.renderPageWindow). Worker mode has no live canvas in the engine, so
    // fall back to record + a windowed replay: set the SAME ctx transform the
    // engine's render_page_window applies (scale·zoom·dpr, translate −clip) and
    // clip to the window, then replay the full page's display list (best-effort;
    // browser-verify deferred with worker-xlsx/pptx). NEVER throws.
    if (this.#localPaint) {
      this.#localPaint.renderPageWindow(idx, canvas, clipX, clipY, viewW, viewH, zoom, dpr);
      // EMF raster cache — drain SYNCHRONOUSLY after the sync paint (no await
      // in between; see renderPage). Requested device px = page-logical dims ×
      // zoom·dpr, both known here as args.
      this.#drainEmfPaintRequests(zoom * dpr);
      return true;
    }
    // Worker mode (#localPaint == null): record a WINDOW-LOCAL slice and replay
    // SCALE-ONLY. The old fallback recorded the FULL page then applied the SAME
    // giant CTM translate the in-process path used (`scale(zoom·dpr); rect(clip…);
    // clip; translate(-clip)`) — at high zoom that pushed device coords past the
    // Chrome/Skia ~16384 px limit and Skia SILENTLY dropped the draws (the growing
    // white block). Now the wasm records culled + window-local + background-reduced
    // ops (mirroring the in-process render_page_window), so the replay is:
    //   setTransform(s,0,0,s,0,0); rect(0,0,viewW·dpr,viewH·dpr); clip();  — NO translate.
    const s = zoom * dpr;
    const enc = await this.#engine.recordPageWindow(idx, clipX, clipY, viewW, viewH, zoom);
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("renderPageWindow: no 2d context");
    ctx.save();
    // Scale-only CTM; the clip rect is authored WINDOW-LOCAL at (0,0), device
    // (0,0)…(s·viewW/zoom, …) — bounded by the slice at any zoom.
    ctx.setTransform(s, 0, 0, s, 0, 0);
    ctx.beginPath();
    ctx.rect(0, 0, viewW / zoom, viewH / zoom);
    ctx.clip();
    // EMF raster cache — the worker-mode paint funnel: replay reports each
    // "emf-pic:" draw (hit AND miss) with its page-logical dims; zoom·dpr is
    // exactly known here. A windowed draw reports the FULL img box even when
    // mostly clipped — a full-size raster for a sliver in view is correct (a
    // pan reveals the rest) and is the pixel budget's job to bound.
    replayDisplayList(ctx, enc.opcodes, enc.strings, this.#imageBitmaps, (key, dw, dh) =>
      this.#noteEmfDraw(key, dw * zoom * dpr, dh * zoom * dpr),
    );
    ctx.restore();
    // Out-of-band emptiness: the display list is already in hand, so its op
    // stream length is a FREE "drew anything" signal (no pixel readback). Zero
    // opcodes ⇔ the windowed record painted nothing.
    return enc.opcodes.length > 0;
  }

  // ── EMF raster cache (paint-driven, no timers) ─────────────────────────────
  //
  // The WML page paint emits ONE `draw_image` op per EMF under the synthetic
  // key `"emf-pic:{rel}"` instead of inlining the ~1000+-op vector program.
  // Every paint reports the EMF's drawn footprint (in-process: the wasm
  // paint-request drain; worker: replay's `onImageDraw`); the pure policy in
  // emf-raster-policy.ts decides when a (re-)raster is warranted; the executor
  // below rasterizes via the mode-agnostic `engine.rasterizeEmf` and registers
  // the ImageBitmap into the SAME two registries ordinary raster images use,
  // so both paint paths blit it with zero paint-path changes. Repaint rides
  // the existing `imageReady` machinery unchanged.

  /**
   * The page-logical → DEVICE-px scale factor read back off a canvas's current
   * 2D transform. `renderPage`/`renderPageViaDisplayList` apply NO Viewer-side
   * transform — the ADAPTER pre-scales the ctx (base layer `ctx.scale`,
   * dev-shell dpr scale) — so the readback of `.a` is the only scale source
   * there. Identity / unreadable transform (test stubs, bare consumers
   * painting logical px 1:1) ⇒ 1. Approximate by design: a wrong-but-recent
   * scale self-corrects on the next paint.
   */
  #ctxScale(canvas: HTMLCanvasElement): number {
    const ctx = canvas.getContext("2d");
    const a = ctx?.getTransform?.()?.a;
    return typeof a === "number" && Number.isFinite(a) && a > 0 ? a : 1;
  }

  /**
   * Drain the wasm-side per-paint EMF request log (in-process only) and feed
   * the resolution policy. MUST run synchronously after the sync local paint —
   * the wasm request log is a thread_local shared by every in-process Viewer
   * on the page, and `set_active(clear)→paint→drain` with no interleaving
   * `await` is the only isolation guarantee. `scale` is page-logical →
   * device px (zoom·dpr for windowed paints; ctx-transform readback for
   * full-page paints).
   */
  #drainEmfPaintRequests(scale: number): void {
    if (!this.#localPaint) return;
    for (const r of this.#localPaint.takeEmfPaintRequests()) {
      this.#noteEmfDraw(r.key, r.dw * scale, r.dh * scale);
    }
  }

  /**
   * One painted EMF draw (both funnels): consult the pure policy and kick the
   * raster executor when it plans one. Allocation-light on the common
   * (no-upgrade) path — `planEmfRaster` is pure comparisons over the mutable
   * sidecar entry (this runs per EMF draw per replayed frame).
   */
  #noteEmfDraw(key: string, reqPxW: number, reqPxH: number): void {
    const plan = planEmfRaster(this.#emfRaster.get(key), reqPxW, reqPxH);
    if (plan) void this.#rasterizeEmf(key, plan.pxW, plan.pxH);
  }

  /**
   * Whether every DIB blit (`"emf:{rel}:{n}"`) of `rel` has decoded into
   * `#imageBitmaps`. Valid as a proxy for the wasm bitmap map the raster
   * paints with ONLY because `#ingestAndDecodeImages` feeds both in the same
   * `.then` — if that feed order ever changes, scan the map the raster uses.
   * (`"emf-pic:…".startsWith("emf:")` is false — hyphen vs colon — so pic keys
   * never pollute this scan; header/footer EMFs never emit blit refs.)
   */
  #emfBlitsComplete(rel: string): boolean {
    const prefix = `emf:${rel}:`;
    for (const r of this.#imageRefsSnapshot) {
      if (r.relId.startsWith(prefix) && !this.#imageBitmaps.has(r.relId)) return false;
    }
    return true;
  }

  /**
   * The raster executor: rasterize `key` at `(pxW, pxH)` device px via the
   * mode-agnostic engine seam and land the bitmap. Landing mirrors
   * `#ingestAndDecodeImages`' resolve branch verbatim: generation guard
   * (stale ⇒ close + drop), close the PREVIOUS bitmap under the key (page
   * paints are synchronous, so post-swap close is safe), register into
   * `#imageBitmaps` + the wasm map, `emit('imageReady', key)`. Extra rules the
   * ordinary image path doesn't need:
   *  - MONOTONIC swap — drop a landing whose `pxW` is below the currently
   *    registered width (two different-sized draws in one paint can put two
   *    rasters in flight; FIFO usually orders landings, this closes the
   *    reordering hole);
   *  - `null` from the engine (feature-off/stale wasm, unknown key after a doc
   *    switch, no OffscreenCanvas) registers NOTHING — the placeholder / stale
   *    bitmap persists (degrade, never a transparent blank);
   *  - landing-time completeness recheck — a DIB blit that decoded WHILE this
   *    raster was in flight (or arrived via the `pendingReraster` flag)
   *    triggers exactly ONE follow-up re-raster at the same size, so an
   *    incomplete raster can never freeze its baked grey blit placeholders
   *    once the blits exist. Worker rasters are complete by construction (the
   *    worker handler awaits blit decode before painting).
   */
  async #rasterizeEmf(key: string, pxW: number, pxH: number): Promise<void> {
    const gen = this.#loadGeneration;
    let entry = this.#emfRaster.get(key);
    if (!entry) {
      entry = { pxW: 0, pxH: 0, complete: false, inFlightPxW: 0, pendingReraster: false };
      this.#emfRaster.set(key, entry);
    }
    entry.inFlightPxW = Math.max(entry.inFlightPxW, pxW);
    // Rel ids are xsd:ID (no ':'), so the prefix strip is unambiguous.
    const rel = key.startsWith("emf-pic:") ? key.slice("emf-pic:".length) : key;
    const completeAtStart = this.#localPaint ? this.#emfBlitsComplete(rel) : true;

    const bm = await this.#engine.rasterizeEmf(key, pxW, pxH);

    if (this.#destroyed || this.#loadGeneration !== gen) {
      // Stale landing (doc switched / viewer destroyed mid-flight): the sidecar
      // was already cleared for the new generation — just release the bitmap.
      bm?.close?.();
      return;
    }
    // Clear the in-flight marker unless a LARGER raster is still airborne.
    if (entry.inFlightPxW <= pxW) entry.inFlightPxW = 0;
    if (!bm) return; // cannot serve — keep placeholder / stale bitmap
    if (pxW < entry.pxW) {
      // Monotonic guard: a smaller raster landing after a larger one already
      // registered would DOWNGRADE the cache (rule 3 would then never restore
      // it at that size). Drop it.
      bm.close?.();
      return;
    }
    this.#imageBitmaps.get(key)?.close?.();
    this.#imageBitmaps.set(key, bm);
    if (this.#localPaint) this.#localPaint.setImageBitmap(key, bm);
    entry.pxW = pxW;
    entry.pxH = pxH;
    entry.complete = completeAtStart;
    this.#emit("imageReady", key);

    // Landing-time completeness recheck (belt) + arrival flag (suspenders): a
    // blit that landed during the flight re-rasters ONCE at the same size.
    const followUp =
      entry.pendingReraster || (!completeAtStart && (!this.#localPaint || this.#emfBlitsComplete(rel)));
    entry.pendingReraster = false;
    if (followUp && entry.inFlightPxW === 0) {
      void this.#rasterizeEmf(key, pxW, pxH);
    }
  }

  /**
   * Clickable hyperlink rectangles on page `idx`, in layout px. Each is
   * `{ x, y, w, h, target }` where target is `"url:<href>"` (external) or
   * `"anchor:<bookmark>"` (internal). Consumers hit-test pointer events
   * against these (translating from CSS px) and open / scroll accordingly.
   *
   * Worker-ready async seam: wraps the synchronous wasm primitive,
   * resolving immediately in-process. `@public`
   */
  async linkRects(idx: number): Promise<LinkRect[]> {
    this.#assertAlive();
    return this.#engine.linkRects(idx);
  }

  /**
   * Resolve an internal anchor (bookmark name) to its on-screen position:
   * `{ page, y }` where `page` is the 0-based page index and `y` is the
   * bookmark line's top in layout px. Returns `null` for an unknown name.
   * Consumers use this to scroll a clicked `anchor:` link into view.
   *
   * Worker-ready async seam: wraps the synchronous wasm primitive,
   * resolving immediately in-process. `@public`
   */
  async bookmarkPosition(name: string): Promise<BookmarkPosition | null> {
    this.#assertAlive();
    return this.#engine.bookmarkPosition(name);
  }

  /**
   * Word comments (display-only, v1), each joined to its range-start position
   * in document order. Valid only after `loadComplete` (positions exist only
   * after layout). Returns `[]` for non-Word documents and for Word documents
   * with no comments part. The consumer owns all presentation — see
   * {@link Comment}.
   *
   * Worker-ready async seam: wraps the synchronous wasm primitive,
   * resolving immediately in-process. `@public`
   */
  async comments(): Promise<Comment[]> {
    this.#assertAlive();
    if (this.#documentKind !== "word") return [];
    return this.#engine.comments();
  }

  /**
   * Comment-range highlight rectangles on page `idx`, in layout px (Word
   * comments v1). The engine paints a persistent translucent fill for
   * each; the consumer hit-tests pointer events against them to link a highlight
   * back to its comment card(s) — collecting ALL hits (a click in an overlap
   * region matches multiple rects). Returns `[]` for non-Word documents, for the
   * docx-only wasm build (no `commentRects` export), and for pages with no
   * comment ranges. See {@link CommentRect}.
   *
   * Worker-ready async seam: wraps the synchronous wasm primitive,
   * resolving immediately in-process. `@public`
   */
  async commentRects(idx: number): Promise<CommentRect[]> {
    this.#assertAlive();
    if (this.#documentKind !== "word") return [];
    return this.#engine.commentRects(idx);
  }

  /**
   * Full-text-search matches on page `idx` for `query`. Read-only: matches + highlight rects are computed without mutating the
   * model or re-laying-out, so a per-keystroke query never invalidates layout.
   * Returns one {@link SearchMatch} per logical hit (WML: a paragraph's logical
   * text, which may span soft-wrapped display lines → multiple rects; SML: a
   * matching cell's model string). `opts` defaults to case-insensitive substring
   * on the active sheet; `scope: "workbook"` (SML only) widens past the active
   * sheet, in which case `idx` is ignored as a sheet selector.
   *
   * This is the WORKER-READY async seam: it wraps the synchronous wasm
   * `searchPage` and resolves immediately in-process, but callers `await` it, so
   * worker mode changes only what sits under this seam — the
   * chunked search loop, generation token, debounce, plugin, and UI are all
   * transport-agnostic. Returns `[]` for the docx-only / search-free wasm build
   * (no `searchPage` export) and for an empty `query`.
   */
  async searchPage(
    idx: number,
    query: string,
    opts?: SearchOpts,
    /** SML row window: chunk a huge sheet across ticks. Half-open [rowStart,
     * rowEnd), 1-based. Omitted ⇒ whole page/sheet (WML/PML ignore it). */
    window?: { rowStart: number; rowEnd: number },
  ): Promise<SearchMatch[]> {
    this.#assertAlive();
    const rs = window?.rowStart;
    const re = window?.rowEnd;
    // No-await-when-sync (see sheetInfo): preserve the pre-collapse in-process
    // tick count; worker mode awaits the RPC.
    const sr = this.#engine.searchPage(idx, query, opts ?? {}, rs, re);
    const raw = sr instanceof Promise ? await sr : sr;
    return raw.map((m) => ({
      page: m.page,
      rects: (m.rects ?? []).map((r) => ({ x: r.x, y: r.y, w: r.w, h: r.h })),
      y: m.y,
      sheet: m.sheet ?? null,
      // SML grid-navigation fields (wasm emits `cellRef`/`gridX`/`gridY`; WML/PML
      // get `null`/`0`). Normalised so the shape is uniform across formats.
      cellRef: m.cellRef ?? null,
      gridX: m.gridX ?? 0,
      gridY: m.gridY ?? 0,
    }));
  }

  /**
   * Lazily resolve a SML match's PAINT-space print rect for Pages-view
   * scroll-to-cell — the search loop now DEFERS this (it returns a zero rect for
   * every SML hit), so the per-match `fill_plan_page` runs at most once, here, for
   * the cell the user navigates to. `sheet` is the match's flat-page index.
   * Returns null for non-SML / a cell beyond the print range (caller falls back to
   * the page origin).
   */
  async resolveSearchRect(
    sheet: number,
    cellRef: string,
  ): Promise<{ page: number; x: number; y: number; w: number; h: number } | null> {
    this.#assertAlive();
    return this.#engine.resolveSearchRect(sheet, cellRef);
  }

  /**
   * Set the active full-text-search highlight state.
   * Thin fire-and-forget pass-through to the wasm setter — it records the query +
   * active match so the NEXT render (the windowed paint pass inside
   * `render_page_window`) paints all matches with the base highlight and the
   * active match with a distinct emphasis. No re-layout occurs and nothing is
   * returned; this is a state set consumed by the next paint, not a query (use
   * {@link searchPage} to enumerate matches). An empty `query` clears all
   * highlights; `activePage < 0` (or no active match) emphasises no rect. No-op
   * for the docx-only / search-free wasm build (no `setSearchHighlight` export).
   *
   * `activeCellRef` is the SML active-match identity — the focused cell's A1 ref
   * (e.g. `"C7"`), paired with `activePage` so the SML paint pass narrows to the
   * page it paints. Pass `null` for WML & PML, which emphasise via `activeIndex`.
   *
   * Stays SYNC: a fire-and-forget state set with no wasm READ and no
   * return value, so there is nothing to await — it does not sit on the
   * worker-read seam. `@public`
   */
  setSearchHighlight(
    query: string,
    opts: SearchOpts,
    activePage: number,
    activeIndex: number,
    activeCellRef: string | null = null,
  ): void {
    this.#assertAlive();
    this.#engine.setSearchHighlight(query, opts ?? {}, activePage, activeIndex, activeCellRef);
  }

  /**
   * The unified hit-test: map a CONTENT-space point on the
   * {@link DocLoc}-addressed surface to a {@link DocHit} (a text position on a
   * page/slide, or a grid cell), or `null` when the point misses everything
   * selectable. `granularity` folds char/word/paragraph in ONE round-trip
   * (word/paragraph carry an expanded `range`; WML only today — PML
   * word/paragraph is a declared v1 deferral, the grid ignores it). The caller
   * has already divided client coords by the zoom scale (content px).
   *
   * WORKER-READY async seam: the RPC and the sync wasm export are
   * both awaited, so a worker round-trip changes nothing above this line.
   * Because the wasm reads are synchronous and FIFO on the worker,
   * plugin-selection's monotonic gesture-seq is the sole ordering
   * guard. `null` for a stale wasm build with no `docHitTest` export. `@public`
   */
  async hitTest(
    loc: DocLoc,
    x: number,
    y: number,
    granularity: Granularity = "char",
  ): Promise<DocHit | null> {
    this.#assertAlive();
    return this.#engine.docHitTest(loc, x, y, granularity);
  }

  /**
   * The highlight rectangles (layout px) a {@link DocSelection} covers on the
   * {@link DocLoc}-addressed surface. Normalised
   * in-engine (start ≤ end); disjoint bidi rects fall out per line. `[]` for a
   * cells selection — the grid paints its selection fully in-engine. Exposed
   * as a tested seam (the DRAWN highlight stays in-engine via
   * {@link setSelection}, so the drag hot path does NOT call this per frame).
   *
   * WORKER-READY async seam; `[]` for a stale wasm build. `@public`
   */
  async selectionRects(loc: DocLoc, sel: DocSelection): Promise<Rect[]> {
    this.#assertAlive();
    assertSelection(sel);
    return this.#engine.docSelectionRects(loc, sel);
  }

  /**
   * Set (or clear) THE selection so the NEXT render paints its
   * highlight (the additive paint pass after search, mirroring
   * {@link setSearchHighlight}). ONE verb for every format: `sel` is a
   * {@link DocSelection}; `null` CLEARS every selection kind (replacing the
   * three legacy per-format clear sentinels), and setting one kind clears the
   * siblings in-engine (the kinds are mutually exclusive). Fire-and-forget and
   * SYNC: worker mode sends an id-0 notify (painted on the next record,
   * ordered before subsequent paint requests). Does NOT self-repaint —
   * the adapter drives a repaint on the `selection:changed` edge. No-op for a
   * stale wasm build. `@public`
   */
  setSelection(sel: DocSelection | null): void {
    this.#assertAlive();
    if (sel) assertSelection(sel);
    this.#engine.docSetSelection(sel);
  }

  /**
   * The retained ORIGINAL bytes for an embedded image
   * (untouched, for the clipboard `<img>` data-URL). Served in BOTH modes once
   * decode settles: in-process from the local wasm, in worker mode from the
   * imageRefs RPC bytes. `null` for an
   * unknown/dropped relId or before the (worker) RPC ingest lands. `@public`
   */
  async imageOriginal(relId: string): Promise<{ mime: string; bytes: Uint8Array } | null> {
    this.#assertAlive();
    // Read the main-thread retention map. It is populated in worker mode too now
    // (the imageRefs RPC bytes are retained by #ingestAndDecodeImages), so a miss
    // is a legitimate null (a missing image / pre-decode), NOT an error.
    // A chart has NO original bytes (its PNG is rendered
    // on demand); a `chart::` key may alias a retained FALLBACK thumbnail, which
    // must never leak into the copy path. Charts stay OUT of the image-bytes paths.
    if (relId.startsWith("chart::")) return null;
    const e = this.#imageBytes.get(relId);
    return e ? { mime: e.mime, bytes: e.bytes } : null;
  }

  /**
   * The decoded intrinsic-resolution `ImageBitmap` for an
   * embedded image, for the `image/png` clipboard flavor (drawn onto an
   * OffscreenCanvas) and `objectInfo`'s intrinsic dims. Served in BOTH modes once
   * decode settles; `null` before decode
   * settles or for an unknown relId. `@public`
   */
  async imageBitmapFor(relId: string): Promise<ImageBitmap | null> {
    this.#assertAlive();
    // Map read — populated in worker mode too now (RPC-sourced decode); a miss is
    // a legitimate null (pre-decode / unknown), no #remote branch needed.
    // Chart keys stay OUT of the image paths (a decoded
    // `chart::` bitmap is the render FALLBACK thumbnail, never the copy source).
    if (relId.startsWith("chart::")) return null;
    return this.#imageBitmaps.get(relId) ?? null;
  }

  /**
   * Object copy metadata for an {@link ObjectRef}. Dims
   * are INTRINSIC (from the decoded `ImageBitmap.width/height`), mime from the
   * retained originals. TS-side derivation (no engine export). `null` when the
   * relId isn't retained / the bitmap isn't decoded (both modes now serve once
   * the decode settles). `@public`
   */
  async objectInfo(ref: ObjectRef): Promise<ObjectInfo | null> {
    this.#assertAlive();
    // Map read — populated in worker mode too now (RPC-sourced decode); no #remote
    // branch. A miss (pre-decode / unknown) is a legitimate null.
    // Null for chart objects (charts have no original
    // bytes/intrinsic bitmap; the chart flavors read {@link chartData} instead).
    if (ref.kind === "chart" || ref.key.startsWith("chart::")) return null;
    const bytes = this.#imageBytes.get(ref.key);
    const bmp = this.#imageBitmaps.get(ref.key);
    if (!bytes || !bmp) return null;
    return { relId: ref.key, mime: bytes.mime, wPx: bmp.width, hPx: bmp.height };
  }

  /**
   * The chart COPY MODEL for a `chart::<rId>` key: the
   * SAME parsed series/categories the painter paints (the TSV source; NO new
   * XML parsing) + the chart's DISPLAY px. WORKER-READY async seam: the value
   * is structured-clone-safe, so worker mode dispatches the RPC (the TSV flavor
   * copies in BOTH modes); in-process awaits the sync wasm export. Doc-kind-
   * agnostic (WML `chart::rIdN` + SML `chart::sml::…` keys). `null` for an
   * unknown key / unsupported chart / stale build. `@public`
   */
  async chartData(key: string): Promise<ChartCopyData | null> {
    this.#assertAlive();
    return this.#engine.chartData(key);
  }

  /**
   * The chart RASTER seam: paint chart `key` into a
   * caller-provided 2D context at `(wPx, hPx)` CANVAS px with a uniform `scale`
   * (the copy flavors pass canvas = 4 × display px, scale = 4). Returns `false`
   * as the CLEAN SKIP (same posture as the image reads): worker mode (a live context cannot cross
   * the RPC boundary — no throw, no empty blob; the text flavors still copy),
   * a stale build, an unknown key, or an unsupported chart. Draws only on the
   * provided context — the page render stays byte-identical. `@public`
   */
  async renderChartToCanvas(
    key: string,
    ctx: CanvasRenderingContext2D | OffscreenCanvasRenderingContext2D,
    wPx: number,
    hPx: number,
    scale: number,
  ): Promise<boolean> {
    this.#assertAlive();
    // TIER 2 — DEGRADED clean-skip gated on an EXPLICIT capability (a live ctx
    // cannot cross the RPC boundary). false, not a throw or empty blob — the TSV/
    // HTML text flavors still copy. In-process localChartRaster is true.
    if (!this.#engine.capabilities.localChartRaster || !this.#localPaint) return false;
    return this.#localPaint.renderChartToCanvas(key, ctx, wPx, hPx, scale);
  }

  /**
   * The unified object hit-test at `(x, y)` on the {@link DocLoc}-
   * addressed surface (object-first z-order): a WML inline/anchored image
   * or chart, an SML grid chart, or a PML picture. Maps the engine
   * {@link DocObjectHit} to `{object: ObjectRef, rect}` (`page` = surface,
   * `index: 0`) uniformly here so worker mode is never silently unremapped.
   * `null` on a miss / a stale build. WORKER-READY async seam. `@public`
   */
  async hitTestObject(
    loc: DocLoc,
    x: number,
    y: number,
  ): Promise<{ object: ObjectRef; rect: Rect } | null> {
    this.#assertAlive();
    const raw = await this.#engine.docHitTestObject(loc, x, y);
    if (!raw) return null;
    return {
      object: { page: raw.surface, kind: raw.kind, key: raw.key, index: 0 },
      rect: raw.rect,
    };
  }

  /**
   * Set (or clear) the selected OBJECT so the NEXT render paints its
   * OUTLINE (fire-and-forget, sync, mirroring {@link setSelection}). ONE verb
   * for every format: the engine routes a PML picture to its slide-scoped cell
   * and a WML image / WML-or-SML chart to the shared key-scoped cell. `null`
   * clears BOTH cells. Worker mode sends an id-0 notify. No-op for a stale
   * build. `@public`
   */
  setSelectedObject(ref: ObjectRef | null): void {
    this.#assertAlive();
    this.#engine.docSetSelectedObject(ref ? { surface: ref.page, key: ref.key } : null);
  }

  /**
   * The unified extract: the flavor-neutral fragment for a
   * {@link DocSelection}. A text selection yields the
   * {@link DocFragment} schema (document-scoped for WML, target-scoped for PML
   * — the selection carries its own scope, so there is NO loc argument); a
   * cells selection yields the {@link CellFragment} schema (merges expand,
   * copy-capped). ONE call per copy; the plugin serializes each clipboard
   * flavor from the returned JSON (no wasm string work).
   *
   * WORKER-READY async seam (the worker dispatcher ships the sync wasm value
   * directly). `null` for a selection kind the loaded doc cannot serve, or a
   * stale build. `@public`
   */
  async extractRange(sel: Extract<DocSelection, { kind: "cells" }>): Promise<CellFragment | null>;
  async extractRange(sel: Extract<DocSelection, { kind: "text" | "pmlText" }>): Promise<DocFragment | null>;
  async extractRange(sel: DocSelection): Promise<DocFragment | CellFragment | null>;
  async extractRange(sel: DocSelection): Promise<DocFragment | CellFragment | null> {
    this.#assertAlive();
    assertSelection(sel);
    return this.#engine.docExtractRange(sel);
  }

  /**
   * Tier-3 escape hatch: the SYNCHRONOUS in-process extract,
   * for the fallback clipboard write path that must serialize INSIDE the copy
   * event (no await). ONE hatch for every {@link DocSelection} kind (it wraps
   * the same `docExtractRange` export). Returns `null` when worker-backed —
   * the doc lives in the worker and cannot be read synchronously, so the
   * adapter treats `null` as "cannot serialize now" and emits `clipboard:error`
   * rather than write stale content. Also `null` for a stale build. `@public`
   */
  extractRangeSyncInProcess(sel: Extract<DocSelection, { kind: "cells" }>): CellFragment | null;
  extractRangeSyncInProcess(sel: Extract<DocSelection, { kind: "text" | "pmlText" }>): DocFragment | null;
  extractRangeSyncInProcess(sel: DocSelection): DocFragment | CellFragment | null;
  extractRangeSyncInProcess(sel: DocSelection): DocFragment | CellFragment | null {
    this.#assertAlive();
    assertSelection(sel);
    // The sync copy-event hatch is gated on an EXPLICIT capability (the
    // doc + wasm must live on THIS thread to read synchronously), not a raw
    // #remote branch.
    if (!this.#engine.capabilities.inProcessDoc) return null; // worker mode: no synchronous read
    const wasm = this.#wasm as unknown as { docExtractRange?: (sel: DocSelection) => unknown };
    if (typeof wasm.docExtractRange !== "function") return null;
    return (wasm.docExtractRange(sel) as DocFragment | CellFragment | null) ?? null;
  }

  /**
   * Debug-only layout-introspection probe (layer a). Returns the
   * positioned-layout tree for a page — Tier 1 (cheap shallow index) when
   * `node` is omitted, Tier 2 (full detail for one element) when given.
   * Only present when the WASM was built with `--features debug-probe`;
   * returns `undefined` otherwise.
   *
   * Worker-ready async seam: wraps the synchronous wasm primitive,
   * resolving immediately in-process. `@public`
   */
  async debugLayout(page: number, node?: number): Promise<unknown> {
    this.#assertAlive();
    const fn = (this.#wasm as { debugLayout?: (p: number, n?: number) => unknown }).debugLayout;
    return fn ? fn.call(this.#wasm, page, node) : undefined;
  }

  /**
   * Debug-only font-resolution probe (layer b). Returns ONE row per
   * font the current document USES, telling the full resolution story:
   *   `used`          — the declared/expected family the document names
   *   `resolved`      — the family Parasma actually loaded for it, or null when
   *                     no substitute was found (engine renders with its default)
   *   `wordTarget`    — the family Word substitutes (Verdana for DejaVu Sans),
   *                     surfaced whether or not `useLocalFonts` acted on it
   *   `source`        — the regular face's byte-source PROVENANCE: 'local'
   *                     (installed font), 'cdn', 'failed' (last attempt could
   *                     not register the face), or null. Survives session-cache
   *                     hits — freshness is reported separately.
   *   `freshness`     — 'fresh' (fetched + registered during this document's
   *                     load) vs 'cached' (served by the session cache), or null
   *   `loadedVariants`— which of regular/bold/italic/boldItalic registered
   *   `metrics`       — the resolved regular face's hhea metrics (per-em)
   *   `resolvesNow`   — what the engine's resolve_font_with_fallbacks returns
   *                     for this family RIGHT NOW (the wasm probe's live
   *                     re-resolution — what a run declaring this family would
   *                     paint with), or null on a pre-families probe build
   *   `fellBackToDefault` — true iff that live resolution landed on the
   *                     document default because nothing declared is loaded
   *   `alias`         — the engine's family_aliases hop for this family, if any
   *
   * Only present when the WASM was built with `--features debug-probe`; returns
   * `undefined` otherwise. In WORKER mode the document (and its font cache)
   * lives in the worker's wasm — the local #wasm cache is EMPTY — so the probe
   * routes over the generic dispatcher; a stale worker bundle / probe-OFF build
   * throws in dispatch and degrades to `undefined` (the gridPaneInfo precedent).
   *
   * Worker-ready async seam: wraps the synchronous wasm primitive. `@public`
   */
  async debugFonts(): Promise<unknown> {
    this.#assertAlive();

    type WasmFace = {
      family: string; // effective family, lowercased
      weight: number;
      italic: boolean;
      unitsPerEm: number;
      ascentEm: number;
      descentEm: number;
      lineGapEm: number;
    };
    type WasmFamilyReport = {
      requested: string;
      alias: string | null;
      resolvesNow: string;
      fellBackToDefault: boolean;
    };
    type WasmFontTable = { faces?: WasmFace[]; families?: WasmFamilyReport[] };

    // The facade does the raw dual-branch probe call (in-process typeof
    // guard / worker RPC with the stale-bundle try-catch), returning undefined
    // when the probe is unavailable; the Viewer keeps the merge orchestration.
    const table = (await this.#engine.debugFonts()) as WasmFontTable | undefined;
    if (!table) return undefined;
    const faces = table.faces ?? [];
    // The per-requested-family FINAL-STATE report. Absent on an older
    // probe build → the merged fields degrade to null rather than lying.
    const familyReports = new Map((table.families ?? []).map((f) => [f.requested, f]));
    const variants: Array<{ name: string; weight: number; italic: boolean }> = [
      { name: "regular", weight: 400, italic: false },
      { name: "bold", weight: 700, italic: false },
      { name: "italic", weight: 400, italic: true },
      { name: "boldItalic", weight: 700, italic: true },
    ];

    const fonts = this.#lastFamiliesUsed.map((declared) => {
      const plan = this.#lastFontPlan.get(declared);
      const resolvedLower = plan ? plan.family.toLowerCase() : null;
      const facesForFamily = resolvedLower ? faces.filter((f) => f.family === resolvedLower) : [];
      const regular =
        facesForFamily.find((f) => f.weight === 400 && !f.italic) ?? facesForFamily[0] ?? null;
      const loadedVariants = variants
        .filter((v) => facesForFamily.some((f) => f.weight === v.weight && f.italic === v.italic))
        .map((v) => v.name);
      // Live per-family resolution from the probe (families are keyed by the
      // SAME requested strings fontFamiliesUsed reported, so used === requested).
      const report = familyReports.get(declared) ?? null;
      const source = resolvedLower ? this.#faceOrigin.get(`${resolvedLower}|400|normal`) ?? null : null;
      return {
        used: declared,
        // null ⇒ engine default. A LOCAL-ONLY plan (no urls) only
        // counts as resolved when its local bytes actually loaded — a miss IS
        // the engine default, exactly as when no plan existed.
        resolved: plan && (plan.urls || source === "local") ? plan.family : null,
        wordTarget: plan?.wordTarget ?? null,
        source,
        freshness: resolvedLower
          ? this.#faceFreshness.get(`${resolvedLower}|400|normal`) ?? null
          : null,
        resolvesNow: report ? report.resolvesNow : null,
        fellBackToDefault: report ? report.fellBackToDefault : null,
        alias: report ? report.alias : null,
        loadedVariants,
        metrics: regular
          ? {
              unitsPerEm: regular.unitsPerEm,
              ascentEm: regular.ascentEm,
              descentEm: regular.descentEm,
              lineGapEm: regular.lineGapEm,
            }
          : null,
      };
    });

    return { useLocalFonts: this.#useLocalFonts, fonts };
  }

  /**
   * The TS-side font-provenance snapshot: per DECLARED family (the current
   * document's fontFamiliesUsed), the plan resolution + byte-source origin +
   * per-load freshness + which faces are registered in document.fonts. This is
   * the probe-INDEPENDENT half of the font truth surface — no wasm read, so it
   * is identical in worker mode and on probe-OFF builds. The headless
   * `fonts.debug` command merges it with {@link debugFonts}' wasm probe.
   *
   * Stays SYNC: pure reads of main-thread capture fields. `@public`
   */
  fontProvenance(): FontProvenance {
    this.#assertAlive();
    const variants = [
      { name: "regular", weight: 400, style: "normal" },
      { name: "bold", weight: 700, style: "normal" },
      { name: "italic", weight: 400, style: "italic" },
      { name: "boldItalic", weight: 700, style: "italic" },
    ] as const;
    const families = this.#lastFamiliesUsed.map((declared): FontProvenanceFamily => {
      const plan = this.#lastFontPlan.get(declared);
      const resolvedLower = plan ? plan.family.toLowerCase() : null;
      const regularKey = resolvedLower ? `${resolvedLower}|400|normal` : null;
      const origin = regularKey ? this.#faceOrigin.get(regularKey) ?? null : null;
      const match = regularKey ? this.#faceMatch.get(regularKey) ?? null : null;
      const localCandidates = plan?.localCandidates ?? [];
      // Candidate availability from the session catalog (null when no
      // catalog exists — the dev shell may then fall back to the canvas probe).
      let candidateInstalled: boolean | null = null;
      for (const c of localCandidates) {
        const r = this.#localFontProvider.installedInCatalog(c);
        if (r === true) {
          candidateInstalled = true;
          break;
        }
        if (r === false) candidateInstalled = false;
      }
      // A LOCAL-ONLY plan (no urls) resolves only via local bytes;
      // a miss is the engine default (same rule as debugFonts).
      const resolved = plan && (plan.urls || origin === "local") ? plan.family : null;
      return {
        declared,
        resolved,
        wordTarget: plan?.wordTarget ?? null,
        origin,
        freshness: regularKey ? this.#faceFreshness.get(regularKey) ?? null : null,
        // Truthful "what is registered": the RETAINED FontFace registry,
        // which session-cache hits keep and replacements swap in place.
        loadedVariants: resolvedLower
          ? variants
              .filter((v) => this.#registeredFaces.has(`${resolvedLower}|${v.weight}|${v.style}`))
              .map((v) => v.name)
          : [],
        matchTier: origin === "local" ? match?.tier ?? null : null,
        matchedFamily: origin === "local" ? match?.family ?? null : null,
        coverage: computeFontCoverage({
          declared,
          resolved,
          origin,
          matchTier: origin === "local" ? match?.tier ?? null : null,
          matchedFamily: origin === "local" ? match?.family ?? null : null,
          isCjk: plan?.family === "Noto Sans CJK",
          candidateInstalled,
        }),
        localCandidates,
      };
    });
    return {
      useLocalFonts: this.#useLocalFonts,
      catalogReady: this.#localFontProvider.catalogReady,
      families,
    };
  }

  /**
   * Clear loaded-document state so the next `load()` (or fresh subscription)
   * starts from a clean slate. Use this before loading a different document.
   *
   * Without this, late subscribers receive catch-up `pageReady` events for
   * the *previous* document's pages — those page indices are stale once a
   * new document is loaded.
   *
   * Stays SYNC: clears cached state fields only, no wasm read. `@public`
   */
  reset(): void {
    this.#pageCount = 0;
    this.#loadComplete = false;
    this.#pageSize = null;
  }

  /**
   * Release the WASM handle and GPU-backed image bitmaps. Idempotent.
   *
   * Stays SYNC: teardown / cleanup, not a wasm read. `@public`
   */
  /**
   * Release everything scoped to the LOADED DOCUMENT: the GPU-backed image
   * bitmaps, the retained original bytes, the FontFaces this viewer registered
   * for the doc, and the cached page state. Keeps the wasm module, worker
   * transport, permission bridge, and event handlers ALIVE. Shared by `unload()`
   * (warm re-show) and `destroy()` (full teardown) so both release the doc in
   * ONE place, byte-identically. Idempotent (all clears are no-ops when empty).
   *
   * Stays SYNC: teardown / cleanup, not a wasm read. `@internal`
   */
  #releaseDocument(): void {
    for (const bitmap of this.#imageBitmaps.values()) bitmap.close?.();
    this.#imageBitmaps.clear();
    // EMF raster sidecar: entries are stale for the new doc/teardown; their
    // bitmaps live in #imageBitmaps under "emf-pic:" keys and were closed by
    // the loop above. In-flight rasterizeEmf resolutions drop via the
    // generation guard in #rasterizeEmf.
    this.#emfRaster.clear();
    this.#imageBytes.clear(); // drop retained originals on doc switch/destroy
    // Unregister every retained FontFace so a released doc leaves no orphan
    // faces in the page-global document.fonts (guarded: tests/workers may have
    // no document by teardown time). Per-instance (#registeredFaces), never the
    // sibling-confounded global document.fonts.size.
    if (typeof document !== "undefined" && document.fonts) {
      for (const face of this.#registeredFaces.values()) document.fonts.delete?.(face);
    }
    this.#registeredFaces.clear();
    this.reset(); // #pageCount / #loadComplete / #pageSize back to the empty state
  }

  /**
   * WARM release (the teardown contract): drop the loaded document + its
   * layout + retained images + this-doc FontFaces, but KEEP the wasm module,
   * worker transport (`#remote`), permission bridge, and handlers so a
   * subsequent `loadDocx`/`loadXlsx`/`loadPptx` of the same format re-shows
   * cheaply (no worker-terminate + wasm re-boot). The wasm heap still holds the
   * prior doc's bytes until the next load replaces them — that is what "warm"
   * means; only `destroy()` frees the wasm. A still-RUNNING layout is aborted
   * and its tail silenced; in-flight font/decode tails are stale-ified
   * by the generation bump. Idempotent.
   *
   * Stays SYNC: teardown / cleanup, not a wasm read. The worker-mode
   * abort is fire-and-forget — the document is released logically here and
   * quiescence is enforced by the next load's unwind-await. `@public`
   */
  unload(): void {
    if (this.#destroyed) return;
    // Stale-ify every generation-guarded async tail: an in-flight
    // #decodeImagesInBackground / #loadFonts continuation would otherwise
    // repopulate #imageBitmaps / re-emit imageReady AFTER #releaseDocument
    // cleared them — their stale() checks compare against #loadGeneration.
    this.#loadGeneration++;
    // Abort a still-running layout. Reachable in BOTH modes — the in-process
    // runLayout future macrotask-yields between pages (yield_to_browser is a
    // MessageChannel post), so a click handler's unload() can run mid-layout.
    // Without this the tail keeps streaming pageReady and finally publishes
    // loadComplete(total), resurrecting the state reset() just zeroed. Mirrors
    // the #doLoad* abort blocks; the generation bump above already kills the
    // tail's live() — the #layoutSeq bump keeps the documented "every aborter
    // bumps BEFORE aborting" contract (see #layoutSeq), and the abort itself
    // stops the wasted work.
    if (this.#layoutPromise) {
      this.#layoutSeq++;
      if (this.#engine.capabilities.inProcessDoc) this.#wasm.abortLayout();
      else void this.#engine.abortLayout(); // fire-and-forget: unload stays sync
      // KEEP #layoutPromise non-null: the next load's abort block must still
      // await the aborted future's unwind before touching wasm state (loadDocx
      // during a live runLayout future = "recursive use of an object" panic —
      // see #doLoad).
    }
    this.#releaseDocument();
  }

  destroy(): void {
    if (this.#destroyed) return;
    this.#destroyed = true;
    this.#releaseDocument();
    // The session FontFace retention is for the NEXT load of THIS viewer; a
    // destroyed viewer has none, so drop the decoded-font references (unload()
    // deliberately keeps them — see #sessionFaces).
    this.#sessionFaces.clear();
    this.#permissionUnsub?.();
    this.#permissionUnsub = null;
    this.#wasm.free();
    // Worker-backed viewers own a WorkerTransport (one thread + its own ~6MB
    // wasm instance); without this it leaked per Viewer lifecycle across the
    // documented multi-doc use case. dispose() terminates the worker.
    this.#remote?.dispose();
    this.#remote = null;
    for (const set of Object.values(this.#handlers)) set.clear();
  }

  #assertAlive(): void {
    if (this.#destroyed) throw new Error("Viewer was destroyed");
  }

  /**
   * Route font fetches through a consumer-supplied resolver (their own CDN /
   * CloudFront). The engine still picks WHICH faces; this decides WHERE they
   * load from. Pass `null` to restore the bundled jsDelivr URLs. Takes effect on
   * the next document load (fonts are fetched during load).
   *
   * Stays SYNC: a pure field assignment with no wasm read. `@public`
   */
  setFontUrlResolver(resolver: FontUrlResolver | null): void {
    this.#fontUrlResolver = resolver;
    // A resolver can return ANY origin, so the preconnect target is no longer
    // knowable; clearing it is what keeps a self-hosting consumer from being
    // handed a hint to a CDN they do not use. Clearing back to null restores
    // the bundled origins. `#useFontBase` overwrites this with the base's own
    // origin immediately after calling in. See {@link #warmFontOrigins}.
    this.#fontOrigins = resolver ? [] : BUNDLED_FONT_ORIGINS;
  }

  /**
   * Enable using the user's INSTALLED fonts (Font Access API) to match Word's
   * own substitution (e.g. DejaVu Sans → Verdana). Best-effort and gesture-gated:
   * with no local face available it falls back to the bundled URLs, so the
   * default-off path is unchanged. Takes effect on the next document load — or
   * apply it to the CURRENT document via {@link refreshFonts}, the SOLE live
   * apply trigger.
   *
   * DELIBERATELY INERT: this setter must never implicitly
   * re-fetch/re-layout. plugin-fonts' destroy() calls
   * `setLocalFontsEnabled(false)` during kernel teardown — an implicit apply
   * here would fire a re-fetch + re-layout racing `wasm.free()`.
   *
   * Stays SYNC: a pure field assignment with no wasm read. `@public`
   */
  setLocalFontsEnabled(enabled: boolean): void {
    this.#useLocalFonts = enabled;
  }

  /**
   * True while a {@link refreshFonts} pipeline is queued or in flight. Sync
   * (pure counter read) so the headless `fonts.debug` report can surface a
   * live `refreshing` flag. `@public`
   */
  isRefreshingFonts(): boolean {
    return this.#fontsRefreshing > 0;
  }

  /**
   * Re-resolve font sources for the CURRENT document and APPLY them — the live
   * counterpart of {@link setLocalFontsEnabled}'s next-load contract. FINAL
   * STATE, not path: it diffs the rebuilt font plan's per-face INTENT keys
   * (`local|`/`cdn|` prefixed) against the session-loaded set and
   *
   *  - if NOTHING would change: resolves `{ changed:false }` without aborting
   *    layout, fetching, or re-laying out (the semantic no-op gate);
   *  - otherwise: aborts the in-flight layout (the doc-switch contract),
   *    re-fetches exactly the intent-changed faces (the intent-keyed session
   *    cache skips the rest), overwrites the wasm-side faces via `loadFont`,
   *    replaces the registered FontFaces (delete-old-before-add via the
   *    retention map), and re-runs layout — resolution is BAKED at layout
   *    (`resolve_font_with_fallbacks` → `RunStyle.family`), so a re-render
   *    alone could never apply a new source. The re-layout streams the normal
   *    `pageReady`/`loadComplete` lifecycle.
   *
   * Serialized on the load chain; a racing `load()` wins (it claims the
   * generation synchronously at entry, so a queued refresh sees itself stale
   * and exits without touching the wasm). Worker-backed viewers are fully
   * supported — abort/loadFont/runLayout all ride the existing RPC seams.
   *
   * SELECTION is explicitly DROPPED on a changed refresh (v1 posture): the
   * fresh `run_layout` clears the engine-side selection with the pages, and no
   * TS-side re-anchor is attempted (the old positions are meaningless against
   * the re-broken lines).
   *
   * Emits `fonts:refreshed { changed }` in BOTH cases (observers re-read the
   * truth surface); on the changed path the emission precedes the re-layout's
   * first `pageReady`. `@public`
   */
  async refreshFonts(): Promise<RefreshFontsResult> {
    this.#assertAlive();
    // The refresh token: the generation of the document this refresh belongs
    // to, captured synchronously at entry. Any later load() bumps the counter
    // synchronously at ITS entry, so this refresh is stale from that moment.
    const gen = this.#loadGeneration;
    this.#fontsRefreshing++;
    const run = this.#loadChain
      .then(() => this.#doRefreshFonts(gen))
      .finally(() => {
        this.#fontsRefreshing--;
      });
    this.#loadChain = run.catch(() => {});
    return run;
  }

  /**
   * Cheap "would a local read hit NOW?" probe for the refresh no-op gate
   * (settled local-only misses). Goes through readLocalFontCached so a HIT is
   * simultaneously cached — the apply path then serves it from the Cache API
   * instead of reading twice. Never throws; false on any failure.
   */
  async #probeLocalFace(candidates: string[], weight: string, style: string): Promise<boolean> {
    if (typeof caches === "undefined") return false;
    try {
      const cache = await caches.open(FONT_CACHE_NAME);
      const r = await readLocalFontCached(cache, this.#localFontProvider, candidates, weight, style);
      return r !== null;
    } catch {
      return false;
    }
  }

  /** One refresh's serialized turn — see {@link refreshFonts} for semantics. */
  async #doRefreshFonts(gen: number): Promise<RefreshFontsResult> {
    const stale = () => this.#destroyed || this.#loadGeneration !== gen;
    if (stale()) return { changed: false };

    // ── SEMANTIC NO-OP GATE ──────────────────────────────────────────────────
    // Rebuild the plan for the CURRENT document's declared families and compute
    // each face's intent key EXACTLY as #loadFontsFor would (intent = would this
    // pass TRY the local path — never the observed outcome). Every key already
    // present in the session-loaded set means that face is already satisfied
    // for this intent (wasm font_cache + document.fonts); a missing key means
    // the refresh has real work (a flipped toggle, or a face whose last attempt
    // failed and never registered).
    const plans = buildFontPlan(this.#lastFamiliesUsed);
    const variantKeys = [
      ["400", "normal"],
      ["700", "normal"],
      ["400", "italic"],
      ["700", "italic"],
    ] as const;
    let changed = false;
    let probeInvalidated = false;
    outer: for (const [, plan] of plans) {
      const localCandidates = plan.localCandidates ?? [];
      const wantLocal =
        this.#useLocalFonts && localCandidates.length > 0 && this.#localFontProvider.supported;
      // A LOCAL-ONLY plan (no urls) without local intent is inert:
      // there is nothing this intent would fetch, so it can never mean work.
      if (!plan.urls && !wantLocal) continue;
      for (const [weight, style] of variantKeys) {
        const key = `${wantLocal ? "local" : "cdn"}|${plan.family}|${weight}|${style}`;
        if (this.#loadedFaces.has(key)) continue;
        // A url-less face whose last local read MISSED is settled at the
        // engine default for the THEN-installed set only: an EXPLICIT refresh
        // re-probes it, so a newly-installed brand font is picked up (an
        // unconditional settle would contradict the apply path and make
        // refreshFonts a permanent no-op for these docs). The probe
        // is cheap (dev route: one route call; Font Access: catalog/ladder,
        // invalidated once per refresh so mid-session installs are visible)
        // and a HIT lands in the Cache API, so the apply path cache-hits.
        if (!plan.urls && this.#localAttempted.has(key)) {
          if (!probeInvalidated) {
            this.#localFontProvider.invalidateCatalog();
            probeInvalidated = true;
          }
          const hitNow = await this.#probeLocalFace(localCandidates, weight, style);
          if (stale()) return { changed: false };
          if (!hitNow) continue;
        }
        changed = true;
        break outer;
      }
    }
    if (!changed) {
      this.#emit("fonts:refreshed", { changed: false });
      return { changed: false };
    }

    // ── APPLY ────────────────────────────────────────────────────────────────
    // Abort the in-flight layout and wait for it to unwind before touching the
    // wasm (the doc-switch contract — see #doLoad).
    if (this.#layoutPromise) {
      this.#layoutSeq++; // silence the aborted layout's tail (see #layoutSeq)
      if (this.#engine.capabilities.inProcessDoc) this.#wasm.abortLayout();
      else await this.#engine.abortLayout();
      await this.#layoutPromise.catch(() => {});
      this.#layoutPromise = null;
    }
    if (stale()) return { changed: false };

    // Fresh look at the installed set: the session catalog is
    // rebuilt on an applying refresh, and prior local-only misses are retried
    // against it (a newly-installed font gets picked up here).
    this.#localFontProvider.invalidateCatalog();
    this.#localAttempted.clear();

    // Re-fetch under the current intent. The intent-keyed session cache makes
    // this surgical: unchanged faces session-hit (no bytes move), changed faces
    // re-fetch → loadFont overwrites the wasm-side face (FontCache.insert
    // replaces same-key faces) → the retained FontFace is deleted-then-replaced.
    await this.#loadFontsFor(this.#lastFamiliesUsed, gen);
    if (stale()) return { changed: false };

    // Fresh layout over the SAME loaded document (run_layout clears pages at
    // entry). Announce the refresh BEFORE kicking layout so the fonts:refreshed
    // bridge (which clears the kernel bus's stale pageReady replay log) runs
    // before the first new pageReady can land.
    this.#pageCount = 0;
    this.#loadComplete = false;
    this.#emit("fonts:refreshed", { changed: true });
    const myGeneration = gen;
    const mySeq = ++this.#layoutSeq;
    const live = () => this.#loadGeneration === myGeneration && this.#layoutSeq === mySeq;
    const onPageReady = (idx: number) => {
      if (!live()) return;
      this.#pageCount = Math.max(this.#pageCount, idx + 1);
      this.#emit("pageReady", idx);
    };
    this.#layoutPromise = (
      this.#engine.capabilities.inProcessDoc
        ? this.#wasm.runLayout(onPageReady)
        : this.#workerRunLayout(onPageReady, myGeneration)
    )
      .then((total: number) => {
        // Superseded by a newer load (generation) OR by a newer refresh/abort
        // (seq — an aborted run_layout resolves with a partial count).
        if (!live()) return;
        this.#pageCount = total;
        this.#loadComplete = true;
        this.#emit("loadComplete", total);
      })
      .catch((err: unknown) => {
        if (!live()) return;
        this.#emit("loadError", err);
        console.error("[parasma] refresh re-layout failed:", err);
      });
    return { changed: true };
  }

  /**
   * Swap the BYTE SOURCE behind the local-font path. Production leaves this
   * unset → the gesture-gated Font Access API. Dev and automation harnesses inject a
   * `fetchLocalFont` (e.g. a dev-server route serving the OS-installed fonts) so
   * `useLocalFonts` resolves WITHOUT a gesture, on the same resolution path.
   * Pass `null` to restore the default Font Access provider. Takes effect on the
   * next document load.
   *
   * Stays SYNC: reconstructs the provider, no wasm read. `@public`
   */
  setLocalFontSource(deps: LocalFontDeps | null): void {
    this.#localFontProvider = new LocalFontProvider(deps ?? {});
    this.#wirePermissionBridge(); // the subscription follows the live provider
  }

  /**
   * The local-fonts PERMISSION state ("granted" | "prompt" | "denied" |
   * "unsupported") — drives the snippet's one-click affordance and the dev
   * FONTS tab's permission line. Async: the Permissions API query.
   * `@public`
   */
  async fontPermissionState(): Promise<LocalFontPermission> {
    return this.#localFontProvider.permissionState();
  }

  /** True once the session local-font catalog has been enumerated. `@public` */
  localFontCatalogReady(): boolean {
    return this.#localFontProvider.catalogReady;
  }

  /**
   * Build the local-font session catalog NOW if permitted. Call this
   * synchronously inside a user gesture (the one-click flow) so the
   * enumeration — and, in "prompt" state, the permission prompt itself —
   * rides the transient activation window; in "granted" state it also runs
   * silently. Resolves whether a catalog is available. `@public`
   */
  async warmLocalFontCatalog(): Promise<boolean> {
    return this.#localFontProvider.ensureCatalog();
  }

  async #loadFonts(gen: number): Promise<void> {
    // Read the family list from whichever engine holds the doc: in worker mode
    // the local #wasm has no document, so its families would be empty → wrong
    // fonts → the worker lays out with fallback metrics.
    const families = this.#engine.capabilities.inProcessDoc
      ? (this.#wasm.fontFamiliesUsed() as string[])
      : await this.#engine.fontFamiliesUsed();
    await this.#loadFontsFor(families, gen);
  }

  /**
   * Fetch + register the font variants for an explicit family list. Factored out
   * of `#loadFonts` so the SML path can request the normal-style
   * font directly — `fontFamiliesUsed()` is WML-only and returns empty for an
   * xlsx. Behavior for the WML path is unchanged (it passes `fontFamiliesUsed()`).
   *
   * `gen` is the OWNING load's generation, claimed at `load()` entry — NOT read
   * here from `#loadGeneration`. If a newer `load()` was called while this load
   * sat queued on `#loadChain`, `stale()` is true from the first check and the
   * superseded load's font work (fetches, `fontsReady`, `fonts:variant-arrived`)
   * is suppressed at source — it can never contaminate the newer document's
   * observers (plugin-fonts readiness arm / arrived set).
   */
  async #loadFontsFor(familiesUsed: string[], gen: number): Promise<void> {
    const stale = () => this.#destroyed || this.#loadGeneration !== gen;

    const plans = buildFontPlan(familiesUsed);
    if (stale()) return;

    // Capture the declared→resolved mapping for debugFonts() (reference assigns;
    // negligible cost). A fresh per-face FRESHNESS map for this load; the origin
    // map (#faceOrigin) is session-lived and deliberately NOT reset here.
    this.#lastFamiliesUsed = familiesUsed;
    this.#lastFontPlan = plans;
    this.#faceFreshness = new Map();

    let loadedCount = 0;
    const fetchTasks: Promise<void>[] = [];
    /**
     * Per-load arbitration for SHARED substitute descriptors. Two
     * declared families can resolve to the SAME plan family (Times New Roman
     * and Liberation Serif → Tinos), i.e. ONE wasm/FontFace descriptor — and
     * with declared-name ladder candidates on every plan, one sibling's LOCAL
     * hit can race the other's CDN fallback on that descriptor. Last-writer-
     * wins is nondeterministic and, when the CDN lands last, clobbers the
     * local fidelity win (browser-observed). Rule: LOCAL BEATS CDN within a
     * load — a CDN write is skipped once a sibling landed local bytes (the
     * reverse order self-heals: a later local write replaces the CDN face).
     * Today all siblings of a shared family carry the same wantLocal intent
     * (candidates are declared-name-universal; CJK-routed families have none),
     * so the skipped task's intent key is the same "local|…" key the winner
     * already recorded — the session cache and refresh gate stay consistent.
     */
    const localWonThisLoad = new Set<string>();

    /**
     * In-flight de-duplication for THIS LOAD, keyed by URL.
     *
     * MUST stay outside the per-family loop below. Several document families
     * routinely map to ONE substitute face — Calibri and Calibri Light both to
     * Carlito, Arial/Arial Nova/Tahoma all to Arimo — so a per-family map dedups
     * nothing across them and every family re-fetches the same four files.
     * Measured on calibration doc C4 before this was hoisted: 24 requests for 12 distinct
     * files, 6.3 MB transferred where 3.18 MB was needed. That is 3.1 MB of pure
     * duplicate on the BLOCKING path to first paint, because `runLayout` waits on
     * `Promise.all(fetchTasks)`.
     *
     * The Cache API does not save us here: `fetchWithCache` is
     * match → miss → fetch → put, and the check runs at task-PUSH time before any
     * task has awaited, so concurrent callers for one URL all miss and all fetch.
     * This map is what makes the second caller await the first's promise.
     */
    const bufByUrl = new Map<string, Promise<ArrayBuffer>>();
    const fetchOnce = (url: string): Promise<ArrayBuffer> => {
      let p = bufByUrl.get(url);
      if (p === undefined) {
        p = fetchWithCache(url);
        bufByUrl.set(url, p);
      }
      return p;
    };

    for (const [docFamily, plan] of plans) {
      // The local-fonts INTENT for this family: whether THIS load would TRY the
      // installed-font path (not whether it would succeed — the observed outcome
      // must not feed the cache key, or a transient miss would poison it). Part
      // of the session-cache key below so flipping `setLocalFontsEnabled`
      // changes the key → the next load re-fetches instead of skipping.
      const localCandidates = plan.localCandidates ?? [];
      const wantLocal =
        this.#useLocalFonts && localCandidates.length > 0 && this.#localFontProvider.supported;

      // A LOCAL-ONLY plan (no urls) without local intent is inert — nothing to
      // fetch, engine default renders (the behavior without local fonts).
      if (!plan.urls && !wantLocal) continue;

      if (plan.family.toLowerCase() !== docFamily.toLowerCase()) {
        if (this.#engine.capabilities.inProcessDoc) this.#wasm.addFontAlias(docFamily, plan.family);
        else await this.#engine.addFontAlias(docFamily, plan.family);
      }

      // `url` is undefined on a local-only plan: the face loads
      // from the ladder or not at all (a miss records #localAttempted below).
      const variantDefs = [
        { variant: "regular" as const, url: plan.urls?.regular, weight: "400", style: "normal" },
        { variant: "bold" as const, url: plan.urls?.bold, weight: "700", style: "normal" },
        { variant: "italic" as const, url: plan.urls?.italic, weight: "400", style: "italic" },
        { variant: "boldItalic" as const, url: plan.urls?.boldItalic, weight: "700", style: "italic" },
      ];
      // Apply the consumer font-source override (their CDN) per face, falling
      // back to the bundled default URL. The engine chose the faces; the
      // consumer chooses the origin.
      const resolver = this.#fontUrlResolver;
      const variants: Array<{ url?: string; weight: string; style: string }> = variantDefs.map((v) => {
        let url = v.url;
        if (resolver && v.url !== undefined) {
          // A throwing consumer resolver must NOT crash the whole load: this map
          // runs OUTSIDE the per-variant fetch try/catch below, so a throw here
          // would abort `#loadFonts` and strand the load. Fall back to the
          // bundled default URL and warn.
          try {
            url = resolver({
              family: plan.family,
              variant: v.variant,
              weight: parseInt(v.weight),
              italic: v.style === "italic",
              defaultUrl: v.url,
              documentFamily: docFamily,
            });
          } catch (err) {
            console.warn(
              `[parasma] font-source resolver threw for ${plan.family} ${v.variant}; using default URL:`,
              err,
            );
            url = v.url;
          }
        }
        return { url, weight: v.weight, style: v.style };
      });

      // Per-family completion bookkeeping: once all 4 of THIS family's variants
      // settle (with ≥1 loaded), emit `fonts:variant-arrived` so a consumer can
      // re-render visible pages the instant this family is ready — the slow CJK
      // OTF no longer hides behind the terminal `fontsReady`. Additive only; the
      // single `fontsReady` after Promise.all is unchanged.
      let familySettled = 0;
      let familyLoaded = 0;
      const familyTotal = variants.length;

      // Within-load fetch dedup: a font plan can map several FACES to the same
      // FILE (heavy CJK has regular==italic URL and bold==boldItalic URL — only
      // 2 unique files for 4 faces). Fetching+decoding the same 16MB blob twice
      // is pure waste, so share one fetchWithCache promise per resolved URL.
      // The faces are still distinct registrations (separate loadFont + FontFace
      // per weight/style); only the byte transfer is shared.
      for (const variant of variants) {
        // Session-cache key: this exact (intent, family, weight, style) face. A
        // HIT means the face THIS INTENT would produce is already in BOTH the
        // wasm font_cache and document.fonts from an earlier load this session,
        // so re-fetch/re-loadFont/re-decode is pure redundant work — skip it but
        // still count it as available so the family is reported ready
        // (measure==paint already satisfied for this face). The intent prefix
        // means a useLocalFonts flip changes the key and forces a re-fetch.
        const faceKey = `${wantLocal ? "local" : "cdn"}|${plan.family}|${variant.weight}|${variant.style}`;
        const descriptorKey = `${plan.family.toLowerCase()}|${variant.weight}|${variant.style}`;
        if (this.#loadedFaces.has(faceKey)) {
          // The hit asserts BOTH sides are satisfied — but `unload()` deletes the
          // FontFace from document.fonts while (deliberately) leaving this cache
          // intact, so the PAINT side can be gone even though the wasm font_cache
          // still measures. Re-register the retained face; without this the doc is
          // laid out to one font's widths and drawn with a fallback's glyphs.
          const retained = this.#sessionFaces.get(descriptorKey);
          if (retained && !this.#registeredFaces.has(descriptorKey) && typeof document !== "undefined" && document.fonts) {
            document.fonts.add(retained);
            this.#registeredFaces.set(descriptorKey, retained);
          }
          // Freshness only — #faceOrigin keeps whatever the load that actually
          // fetched the bytes recorded (a hit must not destroy provenance).
          this.#faceFreshness.set(descriptorKey, "cached");
          loadedCount++;
          familyLoaded++;
          familySettled++;
          if (familySettled === familyTotal && familyLoaded > 0 && !stale()) {
            this.#emit("fonts:variant-arrived", {
              family: plan.family,
              loadedVariants: familyLoaded,
            });
          }
          continue;
        }

        fetchTasks.push(
          (async () => {
            try {
              // Opt-in: try the user's INSTALLED font (the declared name via the
              // exact-match ladder, then Word's curated substitute; cached) before
              // the CDN. Any miss/denial falls through to fetchOnce, so the
              // default-off path is byte-identical to before.
              let buf: ArrayBuffer | null = null;
              let localMatch: { tier: MatchTier; family: string } | null = null;
              if (wantLocal && typeof caches !== "undefined") {
                const cache = await caches.open(FONT_CACHE_NAME);
                const local = await readLocalFontCached(
                  cache,
                  this.#localFontProvider,
                  localCandidates,
                  variant.weight,
                  variant.style,
                );
                if (local) {
                  buf = local.bytes;
                  localMatch = { tier: local.matchTier, family: local.family };
                }
              }
              const src: "local" | "cdn" = buf !== null ? "local" : "cdn";
              if (buf === null) {
                if (variant.url === undefined) {
                  // Total local miss: NO CDN fallback exists for a
                  // synthesized local-only plan — the engine default renders.
                  // Recorded as an ATTEMPT (not a
                  // failure) so the refresh no-op gate treats it as settled.
                  this.#localAttempted.add(faceKey);
                  return;
                }
                buf = await fetchOnce(variant.url);
              }
              if (stale()) return;

              const bytes = new Uint8Array(buf);
              // FONTFACE VALIDATES FIRST: Chrome's OTS sanitizer is stricter
              // than the engine's ttf-parser, so bytes the wasm accepts can
              // still be rejected by FontFace. Decode BEFORE touching the wasm
              // font_cache — if OTS rejects, the catch records "failed" and
              // NEITHER side changed (no measure/paint divergence window). The
              // reverse failure (wasm rejects after OTS passed) also leaves both
              // sides consistent: the face is not yet in document.fonts and
              // loadFont throws before inserting.
              const face = new FontFace(plan.family, bytes.buffer as ArrayBuffer, {
                weight: variant.weight,
                style: variant.style,
              });
              await face.load();
              // Recheck after EVERY await: destroy() mid-flight has already
              // deleted the retained faces — adding now would orphan a face on
              // the page-global document.fonts forever.
              if (stale() || this.#destroyed) return;

              // SHARED-DESCRIPTOR ARBITRATION (see localWonThisLoad): a CDN
              // fallback must not clobber a sibling's LOCAL bytes. Checked
              // after the last await before the writes, so the in-process
              // loadFont+register block is atomic with the check. The face IS
              // available (the sibling's), so count it for readiness. (Worker
              // path: the loadFont RPC below re-opens a tiny race window — a
              // late local write still replaces both sides consistently.)
              if (src === "cdn" && localWonThisLoad.has(descriptorKey)) {
                loadedCount++;
                familyLoaded++;
                return;
              }

              // Structured-clone the bytes to the worker (NO transfer) — the same
              // buffer also built the main-thread FontFace above for replay. The
              // arm-specific post-await stale()/destroyed check only fires on the
              // worker arm (in-process loadFont is SYNC — no stale check injected).
              if (this.#engine.capabilities.inProcessDoc) {
                this.#wasm.loadFont(plan.family, bytes, parseInt(variant.weight), variant.style === "italic");
              } else {
                await this.#engine.loadFont(plan.family, bytes, parseInt(variant.weight), variant.style === "italic");
                if (stale() || this.#destroyed) return;
              }

              // Delete-old-before-add: when this descriptor was registered by an
              // earlier load (e.g. the other useLocalFonts intent), remove that
              // face FIRST — two same-descriptor faces must never coexist.
              const prev = this.#registeredFaces.get(descriptorKey);
              if (prev) document.fonts.delete(prev);
              document.fonts.add(face);
              this.#registeredFaces.set(descriptorKey, face);
              this.#sessionFaces.set(descriptorKey, face); // survives unload() — see #sessionFaces
              // Record the face only once BOTH sides are satisfied (wasm
              // font_cache via loadFont AND document.fonts via the decoded
              // FontFace), so a later same-session load can safely skip it. The
              // OPPOSITE intent's entry is now stale (its face was just replaced
              // in both the wasm font_cache and document.fonts), so drop it —
              // flipping the toggle back must re-fetch, not skip.
              this.#loadedFaces.add(faceKey);
              this.#loadedFaces.delete(
                `${wantLocal ? "cdn" : "local"}|${plan.family}|${variant.weight}|${variant.style}`,
              );
              // PROVENANCE ONLY AFTER SUCCESS: recording before loadFont /
              // FontFace registration would overstate (loadFont can reject the
              // bytes — the wasm ttf-parse Err path).
              this.#faceOrigin.set(descriptorKey, src);
              this.#faceFreshness.set(descriptorKey, "fresh");
              // Which ladder rung/candidate the local bytes matched;
              // a CDN (re)load clears the stale local match for the descriptor.
              if (localMatch) this.#faceMatch.set(descriptorKey, localMatch);
              else this.#faceMatch.delete(descriptorKey);
              this.#localAttempted.delete(faceKey);
              if (src === "local") localWonThisLoad.add(descriptorKey);

              loadedCount++;
              familyLoaded++;
            } catch (err) {
              this.#faceOrigin.set(descriptorKey, "failed");
              this.#faceFreshness.delete(descriptorKey);
              this.#faceMatch.delete(descriptorKey);
              console.warn(`[parasma] font fetch failed: ${plan.family} ${variant.url ?? "(local-only)"}:`, err);
            } finally {
              familySettled++;
              if (familySettled === familyTotal && familyLoaded > 0 && !stale()) {
                this.#emit("fonts:variant-arrived", {
                  family: plan.family,
                  loadedVariants: familyLoaded,
                });
              }
            }
          })(),
        );
      }
    }

    await Promise.all(fetchTasks);
    if (!stale()) {
      // Tag the terminal readiness with the per-document load generation so an
      // observer can scope it to THIS document (stale doc-A readiness is
      // distinguishable from doc-B's). `gen` is the owning load's generation,
      // claimed synchronously at load() entry.
      this.#emit("fontsReady", { count: loadedCount, loadId: gen });
    }
  }

  /**
   * Decode every image embedded in the current document, in parallel, and
   * hand each resulting `ImageBitmap` to WASM. Runs in the background after
   * `load()` resolves; never throws — failed decodes log a warning and the
   * renderer falls back to a grey placeholder.
   */
  #decodeImagesInBackground(gen: number): void {
    if (this.#engine.capabilities.inProcessDoc) {
      // VERBATIM today — synchronous, reads the LOCAL wasm in the SAME tick (no
      // added microtask), then runs the SAME ingest.
      this.#ingestAndDecodeImages(this.#wasm.imageRefs() as ImageRef[], gen);
    } else {
      // WORKER mode: the doc lives in the worker's wasm, so
      // its local `#wasm.imageRefs()` is EMPTY. Fetch the real bytes over the
      // facade RPC (Transferable ArrayBuffers), then run the SAME ingest. The
      // ingest populates `#imageBitmaps` + emits `imageReady`, and the existing
      // mode-agnostic invalidate→replay swaps each placeholder for the real image.
      void Promise.resolve(this.#engine.imageRefs())
        .then((refs: ImageRef[]) => {
          // STALE guard AFTER the await (load-bearing): a superseded load must
          // not ingest late-arriving bytes into the new document's caches.
          if (this.#destroyed || this.#loadGeneration !== gen) return;
          this.#ingestAndDecodeImages(refs, gen);
        })
        .catch((err: unknown) => console.warn("[parasma] worker imageRefs RPC failed:", err));
    }
  }

  /**
   * Decode a set of extracted image references into `#imageBitmaps` (and retain
   * the originals in `#imageBytes`), then repaint via `imageReady`. The EXTRACTED
   * body shared by BOTH the in-process and worker arms of
   * {@link #decodeImagesInBackground} — identical decode/cache/repaint regardless
   * of where the bytes came from.
   */
  #ingestAndDecodeImages(refs: ImageRef[], gen: number): void {
    // Capture a diagnostic snapshot NOW, before any decode settles and triggers
    // `clearImageBytes()` (which empties the WASM-side `self.images`). Without
    // this snapshot, `imageRefDiagnostics()` could only ever report a non-empty
    // result in the brief window before the background decode finished.
    // Populate the original-bytes retention map in the SAME pass
    // (BEFORE `clearImageBytes()`). `r.bytes` is the detached JS copy; the
    // `new Blob([...])` below copies again for decode, so retaining the same
    // reference is aliasing-safe (no `.slice()` needed).
    this.#imageBytes.clear();
    this.#imageRefsSnapshot = (refs ?? []).map((r) => {
      if (r.bytes && r.bytes.byteLength > 0) {
        this.#imageBytes.set(r.relId, { mime: r.mime, bytes: r.bytes });
      }
      return {
        relId: r.relId,
        mime: r.mime,
        byteLength: r.bytes?.byteLength ?? 0,
      };
    });
    if (!refs || refs.length === 0) return;

    // `gen` is the owning load's generation (claimed at load() entry) — reading
    // `#loadGeneration` here would mis-tag a superseded load's decodes with the
    // NEWER load's generation (the newer load() may already have bumped it
    // while this load held its serialized #loadChain turn).
    let settled = 0;
    const total = refs.length;
    const stale = () => this.#destroyed || this.#loadGeneration !== gen;

    for (const ref of refs) {
      // Raw-RGBA EMF-DIB variant (wasm `emf` feature): raw pixels have no
      // container, so a `Blob` would not decode — build an `ImageData` of the
      // declared size and decode THAT. This branch is inert for every normal
      // build (the mime is never emitted without the feature).
      const decode =
        ref.mime === "application/x-emf-dib-rgba" && ref.wPx && ref.hPx
          ? createImageBitmap(
              new ImageData(new Uint8ClampedArray(ref.bytes), ref.wPx, ref.hPx),
            )
          : createImageBitmap(
              new Blob([ref.bytes as BlobPart], { type: ref.mime }),
            );
      decode
        .then((bitmap) => {
          if (stale()) {
            bitmap.close?.();
            return;
          }
          // Retain the bitmap UNCONDITIONALLY (replay + clipboard read it); feed
          // the wasm painter only when local paint exists (worker mode has no
          // local wasm to paint into).
          this.#imageBitmaps.set(ref.relId, bitmap);
          if (this.#localPaint) this.#localPaint.setImageBitmap(ref.relId, bitmap);
          this.#emit("imageReady", ref.relId);
          // EMF raster cache — a DIB blit ("emf:{rel}:{n}"; the pic keys are
          // "emf-pic:" and never match this colon-disciplined prefix) arriving
          // after an INCOMPLETE raster upgrades it exactly once, driven by the
          // same event, no polling. Arrival DURING a flight can't re-raster
          // here (the in-flight dedup would swallow it) — it sets the
          // pendingReraster flag the landing consumes instead. `rel` derives
          // by stripping "emf:" and the trailing ":{n}" (rel ids are xsd:ID,
          // no ':', so the rsplit is safe).
          if (ref.relId.startsWith("emf:")) {
            const rel = ref.relId.slice(4, ref.relId.lastIndexOf(":"));
            const entry = this.#emfRaster.get(`emf-pic:${rel}`);
            if (entry) {
              if (entry.inFlightPxW > 0) entry.pendingReraster = true;
              else if (!entry.complete && entry.pxW > 0) {
                void this.#rasterizeEmf(`emf-pic:${rel}`, entry.pxW, entry.pxH);
              }
            }
          }
        })
        .catch((err) => {
          console.warn(
            `[parasma] image decode failed for ${ref.relId} (${ref.mime}):`,
            err,
          );
        })
        .finally(() => {
          settled++;
          // Gate the settle clear to in-process: worker mode now REACHES this
          // line (refs are no longer empty), but the local `#wasm` there is a
          // real-but-empty WasmViewer (harmless no-op) or a bare stand-in in
          // tests (clearImageBytes would THROW). The worker's own resident bytes
          // are a documented memory follow-on (a clearImageBytes-over-RPC).
          if (settled === total && !stale() && this.#engine.capabilities.inProcessDoc) {
            this.#wasm.clearImageBytes();
          }
        });
    }
  }

  /**
   * Diagnostic: list embedded image references (relId, mime, byteLength)
   * the WASM side extracted from the package. Useful when no images render
   * to disambiguate extraction failure vs. decode failure vs. layout failure.
   *
   * Worker-ready async seam: returns a snapshot and resolves
   * immediately; async for parity with worker mode. `@public`
   */
  async imageRefDiagnostics(): Promise<Array<{ relId: string; mime: string; byteLength: number }>> {
    this.#assertAlive();
    // Report the snapshot captured at extraction time (in
    // `#decodeImagesInBackground`, before `clearImageBytes()`). Querying WASM
    // directly is self-defeating: once the background decode settles it calls
    // `clearImageBytes()`, after which `imageRefs()` always returns [] — so a
    // live query can only ever be non-empty in the brief pre-decode window.
    return this.#imageRefsSnapshot.map((r) => ({ ...r }));
  }
}

/**
 * Debug helper — reports the bundled parser/wasm
 * build string. Diagnostic only; not part of the Viewer surface. `@public`
 */
export async function parserInfo(): Promise<string> {
  await ensureWasmReady();
  return wasmParserInfo();
}
