/**
 * DomGridAdapter — the snippet's thin DOM adapter for the continuous spreadsheet
 * GRID. On top of the plugin-grid wiring it provides:
 *   - the cell readout is delivered through an `onCellSelect` callback (the
 *     snippet's SINGLE selection source — the formula bar) instead of reaching
 *     for `document.getElementById('cellbar-*')`. Cleared on sheet/mode/doc
 *     switch by the snippet (it owns the formula bar).
 *   - a 2px accent SELECTION RECTANGLE is painted on the active cell after each
 *     viewport repaint, so the selection is visible (without it the grid
 *     selection was invisible). The rect is tracked in grid logical coords and re-derived each
 *     paint so it stays pinned through scroll.
 *
 * plugin-grid owns the grid DECISIONS/state (mode, extent, grid.cellAt); this
 * owns the windowed virtual scroller DOM: #grid-canvas pinned to the
 * scroll corner via translate(scrollX,scrollY), #grid-spacer sized to the full
 * extent (native-scrollbar trick), and the rAF-coalesced scroll/ResizeObserver.
 */

import type { Kernel } from "@parasma/headless";
import type { Viewer } from "@parasma/core";
import type { RootScope } from "../root-scope";
import { snapLenToDevice } from "./device-pixels";

interface GridCell {
  ref: string;
  raw: string;
  display: string;
}

export interface DomGridAdapterOptions {
  kernel: Kernel;
  /** The DOM seam (realm window for timers, device-pixel ratio). */
  scope: RootScope;
  viewer: Viewer;
  /** The #gridview scroll container (the native scrollbar host). */
  gridviewEl: HTMLElement;
  /** The single windowed #grid-canvas. */
  gridCanvasEl: HTMLCanvasElement;
  /** The full-extent #grid-spacer driving the native scrollbar. */
  gridSpacerEl: HTMLElement;
  /** The #viewer (paged) container — hidden while the grid is shown. */
  viewerEl: HTMLElement;
  /** Single selection source: a grid cell was selected (or null = cleared). */
  onCellSelect?: (cell: GridCell | null) => void;
}

export class DomGridAdapter {
  private readonly kernel: Kernel;
  private readonly scope: RootScope;
  private readonly viewer: Viewer;
  private readonly gridviewEl: HTMLElement;
  private readonly gridCanvasEl: HTMLCanvasElement;
  private readonly gridSpacerEl: HTMLElement;
  private readonly viewerEl: HTMLElement;
  private readonly onCellSelect?: (cell: GridCell | null) => void;

  /** The current mode (mirrored off 'grid:mode-changed'). */
  private mode: "grid" | "pages" = "pages";
  /** The effective tool mode (mirrored off 'tool:mode-changed'). Cell
   *  selection acts only in 'select'; 'hand' leaves the grid to pan. */
  private toolMode: "select" | "hand" = "select";
  /** True between a selection pointer-down and pointer-up. */
  private cellDragging = false;
  /** The sheet the grid is currently set up for (the last resetup sheet). */
  private sheet = 0;
  /**
   * Frozen-pane band sizes for the active sheet (§18.3.1.66, CONTENT px),
   * fetched per setup() via `grid.paneInfo`. Null = no frozen pane (the
   * overwhelmingly common case — every pane branch below is gated on it).
   * The ENGINE pins the bands during paint (scroll offsets are paint inputs);
   * the adapter only needs the band sizes for the search scroll-target math.
   */
  private pane: { bandW: number; bandH: number } | null = null;
  private rafPending = false;
  private destroyed = false;
  /** True while setup() runs — fences paint()'s scroll save (see paint). */
  private settingUp = false;

  /**
   * viewer.paintGridViewport() is async (the worker-ready
   * seam). paint() is async too, so a fresh scroll/intent that lands WHILE a paint
   * is in flight must not start a SECOND concurrent paint onto the same canvas
   * (overlapping draws / torn frames). These two flags preserve the rAF coalescing
   * guarantee across the async boundary:
   *   - `painting` is true for the whole duration of an in-flight paint(); a
   *     re-entrant paint() bails immediately and instead sets `repaintQueued`.
   *   - `repaintQueued` records that at least one repaint was requested while a
   *     paint was running; when the in-flight paint finishes it runs EXACTLY ONE
   *     more paint (coalescing every request that arrived during the flight into a
   *     single trailing frame with the latest scroll/zoom/selection state).
   * In-process paintGridViewport resolves synchronously, so `painting` is only
   * ever briefly true and this collapses to the old behavior; in WORKER mode
   * paintGridViewport is truly async (record RPC + replay per frame), and the
   * trailing-paint coalescing keeps a slow round-trip from piling up frames.
   */
  private painting = false;
  private repaintQueued = false;

  /** Teardown captures: observer + bus/paint unsubs + the scroll listener. */
  private readonly resizeObserver: ResizeObserver;
  private readonly unsubs: Array<() => void> = [];
  private readonly onScroll = (): void => this.scheduleRepaint();

  /**
   * Active selection as the ABSOLUTE grid-content rect (logical px incl. the
   * sheet gutters), captured from the click's scroll-corrected coordinate. Kept
   * absolute so it re-pins correctly across scrolls — we subtract the live scroll
   * each paint. Null = no selection.
   */
  private selection: { x: number; y: number; w: number; h: number } | null = null;

  /**
   * Scroll-scale factors between SPACER (DOM) space and VISUAL space (the
   * spacer cap): browsers cap element heights (Firefox ≈ 17.9M px) below
   * Excel's 1,048,576-row ≈ 21M px extent, so the spacer clamps to SPACER_MAX
   * and scroll positions map linearly between the two spaces. 1.0 for every
   * sheet that fits (everything that scrolled before) — the capped path only
   * arms on monster extents. The ENGINE speaks content px only.
   */
  private scaleX = 1;
  private scaleY = 1;

  /**
   * Grid zoom, mirrored off plugin-zoom's 'zoom:changed'.
   * VISUAL space = CONTENT space × zoom: the spacer sizes to extent×zoom, the
   * engine paints through one uniform ctx scale, and DOM↔content mappings
   * divide/multiply by it. 1 = 100%.
   */
  private zoom = 1;

  /**
   * Zoom GESTURE tier. While > 0 a zoom gesture is easing and the canvas is
   * CSS-SCALED (a fixed crisp bitmap, GPU-scaled) instead of re-rasterized every
   * frame — re-rendering per eased frame re-snaps each 1px gridline to a new
   * device-pixel phase per frame, which flickers the line weight at fractional dpr
   * (the residual "flakiness"). Holds the scale the canvas was last crisply
   * painted at, so the live scale renders as `scale / gestureBaseScale`. 0 = not
   * gesturing. Reset on the settle frame, which repaints crisp + clears the scale.
   */
  private gestureBaseScale = 0;

  /** Spacer cap in CSS px — safely under every browser's element-size limit. */
  private static readonly SPACER_MAX = 15_000_000;

  /**
   * Map the live DOM scroll position to CONTENT-space scroll offsets (the
   * coordinates the engine paints/hit-tests with): DOM → visual via the spacer
   * scroll-scale (identity while uncapped), visual → content via ÷zoom.
   */
  private contentScroll(): { sx: number; sy: number } {
    return {
      sx: (this.gridviewEl.scrollLeft * this.scaleX) / this.zoom,
      sy: (this.gridviewEl.scrollTop * this.scaleY) / this.zoom,
    };
  }

  /**
   * Pending zoom-to-cursor anchor (grid): the canvas-relative cursor `(cx, cy)`
   * plus the CONTENT point under it, captured at the PRE-zoom scale on each grid
   * zoom-wheel tick. The next `setup()` re-pins that content point under the
   * cursor instead of restoring the remembered scroll — so the grid zooms toward
   * the pointer like the page viewer does (was: grid zoom ignored the cursor).
   */
  private pendingZoomAnchor: { cx: number; cy: number; contentX: number; contentY: number } | null =
    null;
  private zoomAnchorTimer = 0;

  /**
   * Capture a zoom-to-cursor anchor. LOCKED on the FIRST wheel tick of a gesture
   * (the content point under the cursor at the start scale) and re-pinned by
   * every subsequent `setup()` until the gesture settles — NOT re-captured or
   * consumed per tick. (Per-tick capture+consume races: zoom-driven `setup()` is
   * async, so a burst of ticks queues several setups; only the first would see
   * the anchor and the rest would restore the remembered scroll, dropping the
   * pin.) A settle timer clears it ~200ms after the last tick, so the next
   * gesture re-locks and a sheet-switch's remembered-scroll restore resumes.
   * `clientX`/`clientY` are page px; the content point uses the click hit-test
   * model (`cx / zoom + scroll`).
   */
  setZoomAnchor(clientX: number, clientY: number): void {
    if (!this.pendingZoomAnchor) {
      const r = this.gridCanvasEl.getBoundingClientRect();
      const cx = clientX - r.left;
      const cy = clientY - r.top;
      const { sx, sy } = this.contentScroll();
      this.pendingZoomAnchor = { cx, cy, contentX: cx / this.zoom + sx, contentY: cy / this.zoom + sy };
    }
    this.scope.win.clearTimeout(this.zoomAnchorTimer);
    this.zoomAnchorTimer = this.scope.win.setTimeout(() => {
      this.pendingZoomAnchor = null;
    }, 200);
  }

  /**
   * Gesture tier render: CSS-scale the last crisp canvas by `zoom/gestureBaseScale`
   * around the cursor anchor (so the content point under the cursor stays put —
   * the SAME point setup() re-pins crisply at settle, so the hand-off is seamless).
   * A fixed bitmap scaled on the GPU glides smoothly without re-snapping any line,
   * so the gridlines don't flicker mid-zoom. The scroll/spacer are frozen during
   * the burst (setup() is skipped), so the pin translate reads the stable scroll.
   */
  private applyGestureScale(): void {
    const rel = this.zoom / this.gestureBaseScale;
    const domX = this.gridviewEl.scrollLeft;
    const domY = this.gridviewEl.scrollTop;
    const a = this.pendingZoomAnchor;
    const cx = a ? a.cx : this.gridviewEl.clientWidth / 2;
    const cy = a ? a.cy : this.gridviewEl.clientHeight / 2;
    // pin(scroll) ∘ scale(rel) about (cx,cy), with transform-origin at 0,0:
    // scale-about-point collapses to translate((1−rel)·c) ∘ scale(rel).
    const tx = domX + cx * (1 - rel);
    const ty = domY + cy * (1 - rel);
    this.gridCanvasEl.style.transformOrigin = "0 0";
    this.gridCanvasEl.style.transform = `translate(${tx}px, ${ty}px) scale(${rel})`;
  }

  constructor(opts: DomGridAdapterOptions) {
    this.kernel = opts.kernel;
    this.scope = opts.scope;
    this.viewer = opts.viewer;
    this.gridviewEl = opts.gridviewEl;
    this.gridCanvasEl = opts.gridCanvasEl;
    this.gridSpacerEl = opts.gridSpacerEl;
    this.viewerEl = opts.viewerEl;
    this.onCellSelect = opts.onCellSelect;

    this.gridviewEl.addEventListener("scroll", this.onScroll, {
      passive: true,
    });

    this.resizeObserver = new ResizeObserver(() => {
      if (this.mode === "grid") this.setup(this.sheet);
    });
    this.resizeObserver.observe(this.gridviewEl);

    this.unsubs.push(
      this.kernel.bus.subscribe("grid:mode-changed", ({ mode }) => {
        this.mode = mode;
        const showGrid = mode === "grid";
        // Zero the DOM scroll BEFORE hiding: browsers remember a hidden
        // element's scroll offsets and RESTORE them on re-show in a task that
        // can land AFTER setup()'s own restore and override it (observed live
        // as a stale-scroll leak across a doc switch: the prior doc's offset
        // resurfaced at row ~4 on the successor). The real per-sheet position
        // lives in plugin-grid's scroll map, not in the hidden DOM.
        if (!showGrid) this.gridviewEl.scrollTo(0, 0);
        this.gridviewEl.style.display = showGrid ? "block" : "none";
        this.viewerEl.style.display = showGrid ? "none" : "";
        if (!showGrid) this.clearSelection();
      }),
      this.kernel.bus.subscribe("grid:resetup", ({ sheet }) => {
        if (sheet !== this.sheet) this.clearSelection();
        this.sheet = sheet;
        if (this.mode === "grid") this.setup(sheet);
      }),
      this.kernel.paint.onPaintIntent((intent) => {
        if (intent.tag === "grid-viewport") this.scheduleRepaint();
      }),
      // Full-text search: the grid canvas is OURS (the page viewport's
      // invalidateVisible doesn't touch it), so repaint the visible window when
      // the query/active-match changes — the wasm grid painter draws the
      // highlight swipe over matching cells from the viewer's search state.
      this.kernel.bus.subscribe("search:changed", () => {
        if (this.mode === "grid") this.scheduleRepaint();
      }),
      // Late image decode: an anchored picture's ImageBitmap arrives AFTER the
      // first grid paint — ALWAYS in worker mode (the imageRefs RPC +
      // createImageBitmap resolve post-paint), so the picture stays blank until a
      // repaint. Repaint the window when a bitmap lands. In-process this is at
      // worst one idempotent repaint (the picture is usually already decoded).
      this.kernel.bus.subscribe("imageReady", () => {
        if (this.mode === "grid") this.scheduleRepaint();
      }),
      // The cell selection paints in-engine (paintGridViewport
      // reads the viewer's cell-selection state), so repaint the window on every
      // selection edge. "object" repaints too: a selected embedded CHART's focus
      // ring is an in-engine additive pass (xlsx embedded charts). Mode signal:
      // suppress cell-drag select in hand mode.
      this.kernel.bus.subscribe("selection:changed", (e) => {
        if (
          this.mode === "grid" &&
          (e.kind === "cells" || e.kind === "none" || e.kind === "object")
        ) {
          this.scheduleRepaint();
        }
      }),
      this.kernel.bus.subscribe("tool:mode-changed", ({ mode }) => {
        this.toolMode = mode;
      }),
      // Grid zoom: mirror plugin-zoom's scale and re-setup (spacer
      // ×zoom, scroll re-anchored to the same CONTENT point — the remembered
      // per-sheet position is zoom-independent). Gesture tiers don't apply:
      // a windowed grid paint is ms-class, so every tick paints crisp.
      this.kernel.bus.subscribe("zoom:changed", ({ scale, gesturing }) => {
        if (this.mode !== "grid") {
          this.zoom = scale;
          this.gestureBaseScale = 0;
          return;
        }
        if (gesturing) {
          if (scale > this.zoom) {
            // Zoom IN: CSS-scale the last crisp bitmap UP — it grows to cover the
            // viewport (no gap) and isn't re-rasterized (no gridline flicker).
            // Capture the base scale on the first such tick of the burst.
            if (this.gestureBaseScale === 0) this.gestureBaseScale = this.zoom;
            this.zoom = scale;
            this.applyGestureScale();
          } else if (scale < this.zoom) {
            // Zoom OUT: scaling the viewport-sized bitmap DOWN would reveal white
            // margins (it has no content beyond the viewport it rendered). Repaint
            // crisp instead — no gap, and the per-frame flicker is negligible at
            // the lower zoom a zoom-out heads toward. Clears any active CSS-scale.
            this.gestureBaseScale = 0;
            this.zoom = scale;
            void this.setup(this.sheet);
          }
          return;
        }
        // Settle (or a discrete zoom.set): repaint CRISP at the final scale and
        // reset the transform. Force the repaint even if `scale` equals the last
        // gesture tick (otherwise the grid would stay at the soft gesture bitmap).
        const wasGesturing = this.gestureBaseScale !== 0;
        this.gestureBaseScale = 0;
        if (scale === this.zoom && !wasGesturing) return;
        this.zoom = scale;
        void this.setup(this.sheet);
      }),
    );

    // ── cell selection pointer gestures (mode-gated) ───────
    // pointer-down anchors a range (or shift+click EXTENDS, or a header click
    // selects a whole row/col used extent); pointer-move drags the range; pointer-
    // up ends it. The plugin owns the decisions; the engine paints the tinted
    // range + distinct active cell (this adapter only forwards content-space
    // coords and repaints on selection:changed). The formula bar reads the cell
    // under the click. In HAND mode selection is suppressed (the grid pans).
    // Hoisted to bound fields (this.onCanvasDown/Move/End) so destroy() can
    // removeEventListener them — an inline arrow has no removable identity, which
    // was the 5-add/1-remove asymmetry (a slot-pooling parent that keeps the
    // viewer graph alive would double-fire these on remount). onCanvasEnd is the
    // SHARED identity for pointerup + pointercancel (the old local-const capture
    // semantics preserved).
    this.gridCanvasEl.addEventListener("pointerdown", this.onCanvasDown);
    this.gridCanvasEl.addEventListener("pointermove", this.onCanvasMove);
    this.gridCanvasEl.addEventListener("pointerup", this.onCanvasEnd);
    this.gridCanvasEl.addEventListener("pointercancel", this.onCanvasEnd);
  }

  /** pointer-down: anchor a cell range (or shift-extend / header select). */
  private readonly onCanvasDown = (ev: PointerEvent): void => {
    if (this.mode !== "grid" || this.toolMode !== "select" || ev.button !== 0) return;
    const r = this.gridCanvasEl.getBoundingClientRect();
    const cx = ev.clientX - r.left;
    const cy = ev.clientY - r.top;
    const q = this.cellQuery(cx, cy);
    if (ev.shiftKey) {
      void this.kernel.exec("selection.extendTo", q);
    } else {
      this.cellDragging = true;
      try {
        this.gridCanvasEl.setPointerCapture(ev.pointerId);
      } catch {
        /* capture is best-effort */
      }
      void this.kernel.exec("selection.begin", q);
    }
    // Formula-bar readout for the clicked cell (independent of the range).
    void this.cellAt(cx, cy).then((cell) => this.onCellSelect?.(cell));
  };

  /** pointer-move: hover cursor + (while dragging) extend the range. */
  private readonly onCanvasMove = (ev: PointerEvent): void => {
    const r = this.gridCanvasEl.getBoundingClientRect();
    const cx = ev.clientX - r.left;
    const cy = ev.clientY - r.top;
    if (this.cellDragging && this.toolMode === "select") {
      void this.kernel.exec("selection.drag", this.cellQuery(cx, cy));
      return;
    }
    void this.cellAt(cx, cy).then((cell) => {
      this.gridCanvasEl.style.cursor = cell ? "cell" : "";
    });
  };

  /** pointer-up / pointer-cancel: end the drag (shared identity → removable). */
  private readonly onCanvasEnd = (ev: PointerEvent): void => {
    if (!this.cellDragging) return;
    this.cellDragging = false;
    try {
      this.gridCanvasEl.releasePointerCapture(ev.pointerId);
    } catch {
      /* capture may already be released */
    }
    void this.kernel.exec("selection.end");
  };

  /** A CONTENT-space grid pointer query (the unified command shape):
   *  canvas CSS px ÷ zoom + a scrollful `loc` addressing the grid's content
   *  space (the engine resolves frozen-pane quadrants from the offsets and
   *  stays zoom-blind). */
  private cellQuery(cx: number, cy: number): {
    loc: { surface: number; scrollX: number; scrollY: number };
    x: number;
    y: number;
  } {
    const { sx, sy } = this.contentScroll();
    return {
      loc: { surface: this.sheet, scrollX: sx, scrollY: sy },
      x: cx / this.zoom,
      y: cy / this.zoom,
    };
  }

  /**
   * Map canvas-relative px → grid.cellAt (the plugin's hit-test decision).
   * The query speaks CONTENT space: raw CSS px ÷ zoom (the engine stays
   * zoom-blind — the GridCellQuery contract).
   */
  private cellAt(cx: number, cy: number): Promise<GridCell | null> {
    const { sx, sy } = this.contentScroll();
    return this.kernel.exec("grid.cellAt", {
      sheet: this.sheet,
      x: cx / this.zoom,
      y: cy / this.zoom,
      scrollX: sx,
      scrollY: sy,
    });
  }

  clearSelection(): void {
    this.selection = null;
    this.cellDragging = false;
    this.onCellSelect?.(null);
    // Drop the engine cell selection too (sheet switch /
    // grid-mode off / doc switch). The plugin fire-and-forgets the engine clear +
    // emits selection:changed{none}, which repaints the now-clean grid.
    void this.kernel.exec("selection.clear");
    if (this.mode === "grid") void this.paint();
  }

  /**
   * Never show a STALE grid on a failed/empty load. Wipe the grid
   * surface on load-START (called from the snippet's clearForLoad, alongside
   * viewportAdapter.clear()): drop the selection/gesture state AND zero the
   * backing store so the prior sheet's bitmap and its scrollbar extent are gone.
   * Without this, an xlsx→xlsx switch where the successor has 0 sheets (extent
   * null → setupInner bails, no repaint) would leave the PRIOR sheet visible
   * under the error overlay.
   *
   * Safe against an in-flight async paint(): clearForLoad also drives
   * grid.setMode(false), so `mode` flips to 'pages' and paint() bails on its
   * post-await `mode !== 'grid'` re-check. Leaves `this.zoom` untouched — the
   * viewer-wide scale persists across doc switches (mirrors the page viewport
   * keeping its currentScale).
   */
  clear(): void {
    this.selection = null;
    this.cellDragging = false;
    this.onCellSelect?.(null);
    this.repaintQueued = false;
    this.gestureBaseScale = 0;
    this.pendingZoomAnchor = null;
    this.scope.win.clearTimeout(this.zoomAnchorTimer);
    this.sheet = 0;
    this.pane = null;
    this.scaleX = 1;
    this.scaleY = 1;
    // Zero the backing store + collapse the spacer: removes the stale bitmap AND
    // the stale native-scrollbar extent. The next grid:resetup → setup() rebuilds
    // size/transform/ctx from scratch.
    this.gridSpacerEl.style.width = "0px";
    this.gridSpacerEl.style.height = "0px";
    this.gridCanvasEl.width = 0;
    this.gridCanvasEl.height = 0;
    this.gridCanvasEl.style.transform = "translate(0px, 0px)";
    this.gridviewEl.scrollTo(0, 0);
  }

  /**
   * Scroll the grid so a CONTENT-space (gutter-exclusive, pre-zoom) point lands
   * in view — the search scroll-to-match seam (full-text search polish). The
   * grid is a windowed virtual scroller, so we set the native DOM scroll (the
   * scroll listener's rAF then repaints the window): content → visual (×zoom) →
   * spacer (÷scroll-scale), mirroring setupInner's restore math. The target is
   * brought ~a quarter-viewport in from the top-left so the matched cell has
   * context around it rather than hugging the gutter edge. No-op outside grid
   * mode (Pages mode scrolls via scrollToAnchorPos instead). The active match is
   * marked by the wasm's active-emphasis fill (no separate ring — see below).
   */
  scrollToContent(contentX: number, contentY: number): void {
    if (this.destroyed || this.mode !== "grid") return;
    const z = this.zoom;
    const viewW = this.gridviewEl.clientWidth / z;
    const viewH = this.gridviewEl.clientHeight / z;
    // Current visible content window (inverse of the scroll readout: content =
    // scroll·scale/zoom).
    const curX = (this.gridviewEl.scrollLeft * this.scaleX) / z;
    const curY = (this.gridviewEl.scrollTop * this.scaleY) / z;
    // Frozen panes (§18.3.1.66): the body region only shows content BEYOND the
    // pinned bands — its visible window along an axis is [scroll + band,
    // scroll + view]. A target INSIDE a band is always visible on that axis
    // (pinned — no scroll needed); a body target's scroll must land it in the
    // body region (right/below the bands). band = 0 (pane-less) collapses every
    // expression to the original math.
    const bandW = this.pane?.bandW ?? 0;
    const bandH = this.pane?.bandH ?? 0;
    // Per-axis scroll target, or null when the axis needs no scroll. The 15%
    // inset (of the BODY region) skips the smooth scroll when the cell already
    // sits comfortably inside the window — avoids a jarring jump when stepping
    // between matches that are both on screen — and is generous enough to cover
    // the sticky gutters. The quarter-body lead-in keeps the cell off the edge.
    const axisTarget = (content: number, cur: number, view: number, band: number): number | null => {
      if (content < band) return null; // pinned band — always visible
      const body = Math.max(view - band, 0);
      const inset = body * 0.15;
      if (content >= cur + band + inset && content <= cur + view - inset) return null;
      return Math.max(0, content - band - body / 4);
    };
    const tx = axisTarget(contentX, curX, viewW, bandW);
    const ty = axisTarget(contentY, curY, viewH, bandH);
    if (tx !== null || ty !== null) {
      // content → visual (×zoom) → spacer (÷scroll-scale); the browser clamps.
      this.gridviewEl.scrollTo({
        left: ((tx ?? curX) * z) / this.scaleX,
        top: ((ty ?? curY) * z) / this.scaleY,
        behavior: "smooth",
      });
    }
    // NOTE: no selection ring here. The wasm active-match emphasis (the brighter
    // fill on the active cell) already marks the navigated match EXACTLY, and the
    // ring's paintSelection transform is gutter-exclusive — it would land offset
    // from the cell by the sticky-gutter width (a crisp engine-bounds outline is a
    // separate follow-up). Repaint so the active emphasis updates even with no
    // scroll (a same-page navigate where the cell is already on screen).
    this.scheduleRepaint();
  }

  private scheduleRepaint(): void {
    if (this.rafPending) return;
    this.rafPending = true;
    requestAnimationFrame(() => {
      this.rafPending = false;
      void this.paint();
    });
  }

  /**
   * Paint the visible window: pin the canvas, paint it, then overlay selection.
   *
   * Async: awaits viewer.paintGridViewport. The rAF in
   * scheduleRepaint still coalesces per frame; the `painting`/`repaintQueued`
   * guard (above) additionally serialises overlapping paints across the await —
   * a repaint requested mid-flight is collapsed into ONE trailing paint when the
   * current one resolves, so no two paints ever draw the same canvas at once.
   */
  private async paint(): Promise<void> {
    // A scroll rAF can land AFTER destroy() (handle.destroy mid-scroll):
    // paintGridViewport on a destroyed Viewer throws "Viewer was destroyed".
    if (this.destroyed || this.mode !== "grid") return;
    // During a zoom gesture the canvas is CSS-scaled (gesture tier); skip the crisp
    // repaint so a stray scroll/resize rAF doesn't reset the transform mid-gesture.
    // The settle frame's setup() does the crisp repaint + clears the scale.
    if (this.gestureBaseScale !== 0) return;
    // In-flight guard: a paint requested while another is running doesn't start a
    // second concurrent draw — it flags a single trailing repaint instead.
    if (this.painting) {
      this.repaintQueued = true;
      return;
    }
    this.painting = true;
    try {
      // Canvas pinning is DOM(spacer)-space; the ENGINE paints in content space
      // ×zoom (identical until zoom≠1 or the spacer cap arms on a monster
      // extent).
      const dpr = this.scope.dpr();
      // Device-pixel snap the scroll PIN (the snapLenToDevice discipline).
      // A fractional scrollLeft/Top at fractional DPR leaves the engine-pinned
      // sticky gutter + gridlines on a shifting device-pixel phase = shimmer.
      // Round the DOM scroll to whole device px and derive BOTH the translate pin
      // AND the engine paint offset from that ONE value so they agree on the same
      // physical-pixel grid every frame. ≤0.5 device px off true scroll = no
      // perceptible content jump; the GPU translate still glides.
      const domX = snapLenToDevice(this.gridviewEl.scrollLeft, dpr);
      const domY = snapLenToDevice(this.gridviewEl.scrollTop, dpr);
      const sx = (domX * this.scaleX) / this.zoom;
      const sy = (domY * this.scaleY) / this.zoom;
      const vw = this.gridviewEl.clientWidth;
      const vh = this.gridviewEl.clientHeight;
      this.gridCanvasEl.style.transform = `translate(${domX}px, ${domY}px)`;
      await this.viewer.paintGridViewport(this.sheet, this.gridCanvasEl, sx, sy, vw, vh, this.zoom);
      // Re-check after the await: a doc switch / mode flip / destroy can land
      // mid-paint (the worker RPC is a real suspension point).
      if (this.destroyed || this.mode !== "grid") return;
      this.paintSelection(sx, sy);
      // Remember the active sheet's position (CONTENT space, so it
      // restores correctly at any later zoom). Riding the rAF-coalesced paint
      // keeps this one store write per frame, not per scroll event. FENCED
      // while a setup is in flight: mid-setup scroll/clamp events must not
      // overwrite the remembered position before setup restores it.
      if (!this.settingUp) {
        void this.kernel.exec("grid.saveScroll", { sheet: this.sheet, x: sx, y: sy });
      }
    } finally {
      this.painting = false;
      // Coalesced trailing paint: if any repaint was requested during the flight,
      // run EXACTLY ONE more now with the latest scroll/zoom/selection state.
      if (this.repaintQueued && !this.destroyed && this.mode === "grid") {
        this.repaintQueued = false;
        void this.paint();
      } else {
        this.repaintQueued = false;
      }
    }
  }

  /**
   * Overlay a 2px accent ring at the active cell (without it the grid
   * selection was invisible). The selection is stored in absolute CONTENT
   * coords; subtract the live scroll and scale by zoom to place it in the
   * viewport-pinned canvas. The ctx is already DPR-scaled by setup() (CSS px).
   */
  private paintSelection(sx: number, sy: number): void {
    if (!this.selection) return;
    const ctx = this.gridCanvasEl.getContext("2d", { alpha: false });
    if (!ctx) return;
    const accent = this.accentColor();
    const z = this.zoom;
    const x = (this.selection.x - sx) * z;
    const y = (this.selection.y - sy) * z;
    // A cell-sized ring centred on the hit point (engine cell-bounds export is a
    // follow-up; this marker makes the selection unambiguously visible). Sized
    // in content px so it tracks the cell through zoom.
    const w = (this.selection.w || 64) * z;
    const h = (this.selection.h || 20) * z;
    ctx.save();
    ctx.strokeStyle = accent;
    ctx.lineWidth = 2;
    ctx.strokeRect(Math.round(x - w / 2) + 1, Math.round(y - h / 2) + 1, w - 2, h - 2);
    ctx.restore();
  }

  private accentColor(): string {
    const root = this.gridviewEl.closest(".parasma-snippet") as HTMLElement | null;
    return root
      ? getComputedStyle(root).getPropertyValue("--ps-accent").trim() || "#e0a33e"
      : "#e0a33e";
  }

  /**
   * Size spacer + canvas for a sheet, restore its remembered scroll, paint.
   *
   * The spacer derives from the FULL engine extent (the
   * stamped 1M-row scrollbar with its tiny thumb) scaled by the grid ZOOM
   * (visual space = content × zoom), extended by
   *  - the kill-the-void minimum (≥ the viewport — a small sheet still fills
   *    its viewport with real grid; the engine paints empty cells there), and
   *  - a modest Excel-like over-scroll margin (half a viewport per axis),
   * then CLAMPED to the browser-safe SPACER_MAX with a linear scroll-scale
   * between spacer space and visual space (scale 1.0 whenever it fits).
   *
   * Sheet-switch scroll restore: instead of the old scrollTo(0,0),
   * the sheet's remembered CONTENT-space position (plugin-grid's per-sheet
   * map, cleared on doc switch) maps back to DOM space and is restored — a
   * tab return, a resize re-setup, and a zoom re-anchor all land on the same
   * content point.
   */
  private async setup(sheet: number): Promise<void> {
    this.sheet = sheet;
    this.settingUp = true;
    try {
      await this.setupInner(sheet);
    } finally {
      this.settingUp = false;
    }
    // The post-restore paint (un-fenced) records the restored position. Awaited so
    // setup() resolves after the paint is dispatched. NOTE: if a paint is
    // already in flight, this awaited paint() coalesces into the trailing repaint
    // and returns early — so the guarantee is "a paint is queued/running", not
    // "pixels are on screen". In-process paintGridViewport is synchronous so it's
    // moot; in WORKER mode paint is truly async (record RPC + replay), so a
    // caller needing paint-settled must await the trailing paint, not just setup.
    await this.paint();
  }

  private async setupInner(sheet: number): Promise<void> {
    const extent = await this.kernel.exec(
      "grid.extent",
      sheet,
    );
    if (!extent) return;
    // Frozen panes: cache the band sizes for the search scroll-target math
    // (null = pane-less, the common case). Fetched per setup so a sheet switch
    // always reflects the active sheet's pane.
    this.pane =
      (await this.kernel.exec(
        "grid.paneInfo",
        sheet,
      )) ?? null;
    // grid.scrollFor: the remembered position, or — on an unvisited sheet — the
    // pane's saved topLeftCell body scroll (§18.3.1.66; absent → A1 → origin).
    const pos =
      (await this.kernel.exec("grid.scrollFor", sheet)) ??
      { x: 0, y: 0 };
    const dpr = this.scope.dpr();
    const z = this.zoom;
    const vw = this.gridviewEl.clientWidth;
    const vh = this.gridviewEl.clientHeight;
    // Kill-the-void + over-scroll: logical (VISUAL-space) scroll extent.
    const logicalW = Math.max(extent.width * z, vw) + Math.ceil(vw / 2);
    const logicalH = Math.max(extent.height * z, vh) + Math.ceil(vh / 2);
    // Browser spacer cap + scroll-scale (identity while uncapped).
    const spacerW = Math.min(Math.ceil(logicalW), DomGridAdapter.SPACER_MAX);
    const spacerH = Math.min(Math.ceil(logicalH), DomGridAdapter.SPACER_MAX);
    this.scaleX = spacerW < logicalW && spacerW > vw ? (logicalW - vw) / (spacerW - vw) : 1;
    this.scaleY = spacerH < logicalH && spacerH > vh ? (logicalH - vh) / (spacerH - vh) : 1;
    this.gridSpacerEl.style.width = `${spacerW}px`;
    this.gridSpacerEl.style.height = `${spacerH}px`;
    // Size the viewport-window backing store on the device-pixel grid (round, so
    // backing = whole device px and CSS box = backing / dpr) → the canvas maps
    // 1:1 onto physical pixels, no compositor resample at fractional DPR. Zoom is
    // baked into the content paint, not the canvas size, so this stays vw×vh.
    const bw = Math.round(vw * dpr);
    const bh = Math.round(vh * dpr);
    this.gridCanvasEl.width = bw;
    this.gridCanvasEl.height = bh;
    this.gridCanvasEl.style.width = `${bw / dpr}px`;
    this.gridCanvasEl.style.height = `${bh / dpr}px`;
    // Opaque context (alpha:false) → eligible for ClearType on Windows; pre-fill
    // white so the resized opaque canvas can't flash black before the grid paints.
    const ctx = this.gridCanvasEl.getContext("2d", { alpha: false });
    if (ctx) {
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.fillStyle = "#ffffff";
      ctx.fillRect(0, 0, bw, bh);
      ctx.scale(dpr, dpr);
    }
    if (this.pendingZoomAnchor) {
      // Zoom-to-cursor: re-pin the LOCKED content point under the cursor at the
      // NEW scale. content scroll sx' = contentX − cx/zoom; DOM scroll = sx'·zoom/
      // scaleX = (contentX·zoom − cx)/scaleX. The browser clamps to range (an edge
      // anchor near 0/max just pins to the edge). NOT consumed here — the settle
      // timer in setZoomAnchor clears it, so every setup in the gesture re-pins
      // the SAME point (a burst of async setups all stay anchored).
      const a = this.pendingZoomAnchor;
      this.gridviewEl.scrollTo(
        (a.contentX * z - a.cx) / this.scaleX,
        (a.contentY * z - a.cy) / this.scaleY,
      );
    } else {
      // Content → DOM: ×zoom into visual space, ÷scroll-scale into spacer space.
      // The browser clamps to the scrollable range (a shrunken zoom re-clamps).
      this.gridviewEl.scrollTo((pos.x * z) / this.scaleX, (pos.y * z) / this.scaleY);
    }
  }

  /**
   * Full teardown: disconnect the ResizeObserver, drop the bus/paint
   * subscriptions and the scroll listener, and fence any already-queued scroll
   * rAF (paint() bails on `destroyed`). Without this the adapter survived
   * handle.destroy() via its closures and a post-destroy scroll threw
   * "Viewer was destroyed".
   */
  destroy(): void {
    if (this.destroyed) return;
    this.destroyed = true;
    this.resizeObserver.disconnect();
    for (const u of this.unsubs.splice(0)) u();
    this.gridviewEl.removeEventListener("scroll", this.onScroll);
    // The 4 gridCanvas pointer listeners (onCanvasEnd registered for BOTH
    // pointerup + pointercancel — one identity, two removes).
    this.gridCanvasEl.removeEventListener("pointerdown", this.onCanvasDown);
    this.gridCanvasEl.removeEventListener("pointermove", this.onCanvasMove);
    this.gridCanvasEl.removeEventListener("pointerup", this.onCanvasEnd);
    this.gridCanvasEl.removeEventListener("pointercancel", this.onCanvasEnd);
    // The 200ms zoom-anchor debounce (clear() drops it too).
    this.scope.win.clearTimeout(this.zoomAnchorTimer);
  }
}
