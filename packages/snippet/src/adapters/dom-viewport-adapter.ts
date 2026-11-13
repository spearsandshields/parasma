/**
 * DomViewportAdapter — the snippet's thin DOM adapter for per-page windowing.
 *
 * plugin-viewport (in @parasma/headless) owns the page MODEL and the visible-
 * window STATE, and emits `{ tag: 'page', pageIndex }` paint intents. This class
 * owns the concrete browser DOM the kernel deliberately does NOT touch:
 *   - the `.page` divs (placeholders), sized from the plugin's page model,
 *   - the per-page canvases (a low-res `ps-base` preview under the crisp
 *     windowed `ps-slice`, DPR backing stores), painted via the public viewer
 *     render calls (`renderPage` / `renderPageWindow`),
 *   - the IntersectionObserver (root = #viewer, rootMargin "400px") that reports
 *     visible indices back into the kernel via `viewport.setVisible`.
 *
 * It DRAINS paint intents (coalesced per requestAnimationFrame,
 * deduped by pageIndex) and, for each 'page' intent, idempotently ensures a
 * placeholder+observer exists and paints the page if it is currently in view.
 *
 * The post-paint interaction wiring (excel cellAt, comment hit-tests, links) is
 * NOT this adapter's concern. The adapter exposes an `onPageWindowed(idx,
 * pageEl)` callback seam so attach-dom wires that to the STABLE .page element
 * (the slice canvas is rebuilt on every scroll).
 *
 * It does NOT touch the Excel GRID path (#gridview/#grid-canvas/#grid-spacer) —
 * that is DomGridAdapter's job.
 */

import type { Kernel } from "@parasma/headless";
import type { Viewer, PageSize } from "@parasma/core";
import type { RootScope } from "../root-scope";
import { snapLenToDevice, snapElementToDeviceGrid } from "./device-pixels";
import { shouldCommitPaint } from "./paint-seq";
import { sliceTrackTransform } from "./slice-track";

export interface DomViewportAdapterOptions {
  kernel: Kernel;
  /** The DOM seam (realm window/document, device-pixel ratio, resize). */
  scope: RootScope;
  viewer: Viewer;
  /** The #pages container the `.page` placeholders are appended to. */
  pagesContainer: HTMLElement;
  /** The #viewer scroll root the IntersectionObserver observes against. */
  viewerEl: HTMLElement;
  /**
   * Seam: called when a page's `.page` PLACEHOLDER is (re)painted, so the
   * composer can wire hit-test listeners to the STABLE .page element (the slice
   * canvas is recreated on every scroll, so listeners can't live on it).
   * Idempotent on the caller's side via a dedup guard on the .page element.
   */
  onPageWindowed?: (idx: number, pageEl: HTMLElement) => void;
}

export class DomViewportAdapter {
  private readonly kernel: Kernel;
  private readonly scope: RootScope;
  private readonly viewer: Viewer;
  private readonly pagesContainer: HTMLElement;
  private readonly viewerEl: HTMLElement;
  private readonly onPageWindowed?: (idx: number, pageEl: HTMLElement) => void;

  private observer: IntersectionObserver | null = null;
  private readonly unsubPaint: () => void;
  private readonly unsubLoadComplete: () => void;

  /** Page indices with a drained 'page' intent pending the next rAF flush. */
  private pendingPaints = new Set<number>();
  /**
   * Page indices with a drained 'page-invalidate' intent (force re-render of an
   * EXISTING canvas) pending the next rAF flush — the kernel-native re-render
   * channel for a late font face / image. Kept separate from `pendingPaints` so
   * the flush can bypass the "already painted → skip" guard for these only.
   */
  private pendingInvalidates = new Set<number>();
  private rafPending = false;

  /**
   * Set FIRST in destroy() (before the unsubscribes). Every rAF callback this
   * adapter schedules is a BARE rAF (a boolean-latch coalescer, not a stored
   * handle), so an in-flight frame can land AFTER destroy() — the exact failure
   * dom-grid-adapter learned (a post-destroy paint threw "Viewer was
   * destroyed"). Each callback bails on this flag; destroy() also early-returns
   * on it for idempotency. Mirrors the grid's proven `destroyed` guard.
   */
  private destroyed = false;

  /**
   * Index → placeholder element. A `querySelector` per intent was O(container)
   * — quadratic across a load, and the dominant main-thread cost once lazy
   * spreadsheet pagination made 20k+-page spreadsheets stream in milliseconds
   * (~20s of selector scans on the stamped-1M fixture). Entries are validated
   * with `isConnected` so an external container clear (doc switch) can never
   * serve a detached node.
   */
  private pageEls = new Map<number, HTMLElement>();
  /** Indices currently inside the visible window (mirrors data-visible). */
  private visibleSet = new Set<number>();

  /**
   * Current CRISP zoom scale (the scale the painted canvases were rendered at).
   * Tier B (gesture settle) re-renders every visible canvas at this scale so the
   * engine re-rasterises sharp; Tier A (during a gesture) applies a cheap CSS
   * transform RELATIVE to this baseline for 60fps, momentarily soft.
   */
  private currentScale = 1;

  /**
   * viewer.pageSize() is async (the worker-ready seam),
   * but the placeholder build / base-layer / settle paths read it synchronously
   * while constructing DOM. Page size is uniform per document, so we cache the
   * first resolved value and prime the cache asynchronously on the first paint
   * intent (preparePageIntent). A 'page' intent that arrives before the cache is
   * warm re-schedules itself once priming resolves — the same page paints a
   * microtask later with zero behavior change (the IntersectionObserver decides
   * visibility either way). Reset on clear() for a doc switch.
   */
  private cachedPageSize: PageSize | null = null;
  /**
   * Per-page size cache (§17.6). A mixed-size multi-section doc reports a
   * different geometry per section, so each page is sized from ITS OWN entry
   * (keyed by page index). A doc whose pages all share a size stores the same
   * value under every key — byte-identical to the single-cachedPageSize path.
   * {@link cachedPageSize} mirrors idx 0 as the synchronous anchor/fallback
   * (setScale loop + camera). Reset on clear() for a doc switch.
   */
  private pageSizes = new Map<number, PageSize>();
  /** Per-index in-flight guard: an idx being primed is not re-fetched. */
  private primingPageSizes = new Set<number>();

  /**
   * OVERSCAN: the slice canvas is rendered LARGER than the
   * viewport — viewport + this fraction of buffer on each side (the usual
   * virtual-scroller overscan). Native scroll translates the oversized slice (a child of .page) for
   * free; onScroll repaints ONLY when the viewport approaches the slice's edge,
   * not every scroll pixel. 0.5 ⇒ half a viewport of buffer each direction (~2×
   * viewport slice). Tunable.
   */
  private static readonly OVERSCAN = 0.5;
  /**
   * OVERSCAN: re-render when this fraction of the per-side overscan buffer is
   * still UNCONSUMED — i.e. repaint once ~ (1 − THRESHOLD_FRAC) of the buffer has
   * been scrolled past, re-centring the overscan on the new viewport. 0.4 ⇒
   * repaint when ~60% of the buffer is used up.
   */
  private static readonly OVERSCAN_THRESHOLD_FRAC = 0.4;
  /**
   * OVERSCAN: per page idx, the page-box-relative CSS rect the current
   * overscanned slice covers ([left, top, right, bottom] in the .page element's
   * own coords — stable as the page scrolls). onScroll dedups against this: if the
   * viewport (in the same basis) still sits inside the recorded rect with enough
   * buffer margin, the native-scroll-translated slice already shows the content and
   * the repaint is SKIPPED. Cleared when the slice is dropped (unpaint/off-screen)
   * so a stale rect can't suppress a needed repaint.
   */
  private sliceCoverage = new Map<number, { l: number; t: number; r: number; b: number }>();

  /**
   * How many OFF-WINDOW `ps-base` previews to retain. The bound is
   * |window| + this: a base inside the IntersectionObserver window is NEVER
   * evictable (see {@link evictBaseLayers}), so the budget only ever spends on
   * pages the user has already scrolled past.
   *
   * A US-Letter base is 791×1024×4 B = 3.09 MiB (see {@link ensureBaseLayer}'s
   * 1024-px longest side), so 8 caps the off-window store at ~25 MiB — against
   * the unbounded 3.09 MiB PER PAGE EVER VISITED it replaces (618 MiB across a
   * 200-page read; measured in base-layer-bound.test.ts). Raising it trades
   * memory for fewer regenerations on scroll-back; the other dial is the 1024
   * itself, which is a preview-quality decision and deliberately NOT bundled here.
   */
  private static readonly BASE_RETAIN_OFF_WINDOW = 8;
  /**
   * Page indices carrying a live `ps-base` canvas, in LEAST-recently-used
   * order. `Set` iterates in insertion order and {@link touchBase} re-inserts, so
   * the front is the eviction candidate. Mirrors the DOM exactly: an entry is
   * added the instant the canvas is inserted and removed the instant it is (see
   * the ensureBaseLayer catch, the loadComplete prune, and clear()).
   */
  private baseOrder = new Set<number>();

  /**
   * Cursor anchor (client coords) for zoom-to-point. Set by setZoomAnchor() from
   * the wheel handler each tick; null ⇒ anchor to the viewport centre (button
   * zoom). Cleared after each settle.
   */
  private zoomAnchor: { x: number; y: number } | null = null;
  /**
   * The anchor LOCKED at the start of a zoom burst (cursor client point), used by
   * latchZoom() so every eased frame anchors the SAME point as the scale changes.
   */
  private gestureAnchor: { x: number; y: number } | null = null;
  /**
   * The cursor's content point + client position, latched ONCE per zoom burst
   * (first tick). The `off`-baked camera reads this + LIVE viewport geometry to
   * compute the scroll each frame — there is no transform, so live geometry is
   * always accurate. Null ⇒ no burst in flight (also the "zooming" flag onScroll
   * checks). contentX = (cursorX − page.left)/baseScale; contentY likewise from the
   * container's visible top. See latchZoom() + cameraAxis().
   */
  private zoomLatch: { cursorX: number; cursorY: number; contentX: number; contentY: number } | null = null;
  /** Coalescing guard for the per-frame crisp windowed repaint (grid pattern):
   * at most one in-flight batch + one trailing batch, whatever the tick rate. */
  private zoomPainting = false;
  private zoomRepaintQueued = false;
  /**
   * BACK-BUFFER SLICE FIX — monotonic paint-generation token (latest-wins).
   * Bumped ONCE per paint generation (a batch of paintWindow launches that
   * should supersede any older in-flight paints) via {@link launchPaintGeneration},
   * and READ (never bumped) inside paintWindow, which captures it at entry and,
   * after its async renderPageWindow resolves, gates every VISIBLE-canvas mutation
   * on shouldCommitPaint(captured, this.paintSeq). A slow in-flight paint whose
   * generation was superseded mid-RPC drops instead of blitting stale pixels over
   * a newer already-landed frame. Layered ON TOP of the zoomPainting/zoomRepaint
   * coalescer (which bounds concurrency), not a replacement for it.
   *
   * INVARIANT: every code path that (re)launches paintWindow MUST run inside a
   * launchPaintGeneration closure so the bump can't be forgotten (a missing bump
   * silently reintroduces the stale-blit window; the pure shouldCommitPaint test
   * cannot catch it) — and the bump is ONCE PER BATCH, never per page: siblings
   * launched by one batch share the token so they all commit (a per-page bump
   * drops every sibling but the last — the selection-highlight bug pinned by
   * invalidate-batch.test.ts). Enumerated launchers: onScroll, onResize,
   * scheduleFlush (page + invalidate drains), zoomPaint. Do NOT add a raw
   * `this.paintSeq++` or a bare paintWindow call.
   *
   * SECOND INVARIANT (same bug class, other direction): a generation must NEVER
   * be opened EMPTY. A bump with zero paintWindow launches supersedes every
   * in-flight paint and schedules NOTHING to redo them, so those repaints resolve
   * and are dropped forever. Every launcher therefore decides its to-repaint list
   * FIRST and skips launchPaintGeneration when the list is empty — the common case
   * for onScroll (the overscan dedup skips every page on most scroll frames) and
   * reachable for onResize / zoomPaint (every page filtered out as disconnected or
   * not visible).
   */
  private paintSeq = 0;
  /**
   * True only while a zoom gesture is EASING (mirrors setScale's `gesturing`).
   * Device snapping is a CRISPNESS-at-rest operation: snapping a fixed feature
   * to the device grid every eased frame — against a step (page size, slice
   * offset, page-left nudge) that changes each frame — re-quantizes it to a
   * different sub-pixel each frame, which reads as the gridline/glyph SHAKE the
   * user sees during zoom (worst at fractional dpr). So we skip all per-feature
   * device snaps mid-gesture (scale smoothly) and snap once on the settle frame.
   */
  private gesturing = false;
  /** Cached `.ps-viewer` top padding (var(--ps-s5)); the container sits this far below
   * the viewer top, so the Y camera origin must include it. */
  private viewerPadTop = -1;

  /** rAF guard for the resize-driven device-grid re-snap (coalesced). */
  private resizeRafPending = false;

  /** rAF guard for the scroll-driven window-slice repaint (coalesced). */
  private scrollRafPending = false;
  /**
   * Passive scroll listener on viewerEl: a scroll recomputes + repaints the slice
   * canvas of every visible page (mirrors dom-grid-adapter's scroll→repaint),
   * deduped by the overscan buffer (see below).
   */
  private readonly onScroll = (): void => {
    // Suppress during a zoom burst: setScale already repaints the visible pages at
    // the new scale (coalesced via zoomPaint), and the scrollTo it issues fires
    // this listener — handling it would double-paint. zoomLatch != null ⇔ a burst
    // is in flight. Normal navigation scrolls (latch null) repaint as usual.
    if (this.zoomLatch !== null) return;
    if (this.scrollRafPending) return;
    this.scrollRafPending = true;
    requestAnimationFrame(() => {
      this.scrollRafPending = false;
      if (this.destroyed) return; // a scroll rAF can land after destroy()
      // OVERSCAN dedup: the slice was rendered LARGER than the viewport (viewport +
      // OVERSCAN buffer each side). Native scroll translates that oversized slice
      // for free, so most scroll frames need NO repaint — only repaint a page when
      // the viewport approaches the edge of the buffer the current slice covers.
      const viewRect = this.viewerEl.getBoundingClientRect();
      const overscanX = viewRect.width * DomViewportAdapter.OVERSCAN;
      const overscanY = viewRect.height * DomViewportAdapter.OVERSCAN;
      // Repaint once the unconsumed margin on any side falls below this — i.e. when
      // ~ (1 − THRESHOLD_FRAC) of the buffer has been scrolled past on that side.
      const threshX = overscanX * DomViewportAdapter.OVERSCAN_THRESHOLD_FRAC;
      const threshY = overscanY * DomViewportAdapter.OVERSCAN_THRESHOLD_FRAC;
      // ONE generation for the whole scroll batch, opened AFTER the list is known
      // so all re-centred slices share it and commit, while any older in-flight
      // paint for those pages (e.g. a settle zoom paint mid-RPC) is superseded
      // rather than blitting stale pixels over the fresh scroll frame. The order
      // matters: on the MAJORITY
      // of scroll frames the dedup below skips every page, and opening an EMPTY
      // generation there would supersede any in-flight late-font/late-image repaint
      // with nothing queued to redo it (the crisp slice then keeps pre-asset pixels
      // indefinitely — only the blurry ps-base under it refreshes).
      const toRepaint: HTMLElement[] = [];
      // (bare block: this loop was the launchPaintGeneration closure body — kept at
      // its original indentation so the dedup rules stay blame-attributable)
      {
        for (const idx of this.visibleSet) {
          const el = this.pageEls.get(idx);
          if (!el?.isConnected) continue;
          const cov = this.sliceCoverage.get(idx);
          if (cov) {
            // Where the viewport now sits within THIS page's box, in the page-box's
            // own CSS coords (same basis the coverage rect is recorded in).
            const pageRect = el.getBoundingClientRect();
            const vL = Math.max(viewRect.left, pageRect.left) - pageRect.left;
            const vT = Math.max(viewRect.top, pageRect.top) - pageRect.top;
            const vR = Math.min(viewRect.right, pageRect.right) - pageRect.left;
            const vB = Math.min(viewRect.bottom, pageRect.bottom) - pageRect.top;
            // A coverage side PINNED to the page-box boundary (the overscan was
            // clamped to the page edge — no more page content exists beyond it) needs
            // NO buffer margin: nothing can scroll into view there. Otherwise the
            // side must retain >= threshold of unconsumed buffer. Still inside on
            // every side → the native-scroll-translated slice already covers the
            // viewport; SKIP the repaint. EPS absorbs sub-pixel snap rounding.
            const EPS = 0.5;
            const okL = cov.l <= EPS || vL - cov.l >= threshX;
            const okT = cov.t <= EPS || vT - cov.t >= threshY;
            const okR = cov.r >= pageRect.width - EPS || cov.r - vR >= threshX;
            const okB = cov.b >= pageRect.height - EPS || cov.b - vB >= threshY;
            if (okL && okT && okR && okB) {
              continue;
            }
          }
          // No slice yet, or the viewport has consumed the buffer → re-centre the
          // overscan on the new viewport (paintWindow updates the coverage rect).
          toRepaint.push(el);
        }
      }
      if (toRepaint.length === 0) return;
      this.launchPaintGeneration(() => {
        for (const el of toRepaint) void this.paintWindow(el);
      });
    });
  };

  /** Bound resize handler — re-snaps visible pages when the centring offset
   *  shifts with the viewport width (held so destroy() can remove it). */
  private readonly onResize = (): void => {
    if (this.resizeRafPending) return;
    this.resizeRafPending = true;
    requestAnimationFrame(() => {
      this.resizeRafPending = false;
      if (this.destroyed) return; // a resize rAF can land after destroy()
      // ONE generation for the whole resize batch, opened AFTER the list is known,
      // so a resize supersedes any older in-flight paint rendered at the pre-resize
      // geometry instead of letting it blit stale-geometry pixels. Collect FIRST:
      // if every visible page filtered out (all disconnected mid-churn) an empty
      // generation would supersede in-flight paints with nothing queued to redo
      // them — see the SECOND INVARIANT on paintSeq.
      const toRepaint: HTMLElement[] = [];
      for (const idx of this.visibleSet) {
        const el = this.pageEls.get(idx);
        if (!el?.isConnected) continue;
        snapElementToDeviceGrid(el, this.scope.dpr());
        // A resize changes both the centring offset (re-snapped above) AND the
        // viewport intersection, so the slice must be recomputed.
        toRepaint.push(el);
      }
      if (toRepaint.length === 0) return;
      this.launchPaintGeneration(() => {
        for (const el of toRepaint) void this.paintWindow(el);
      });
    });
  };

  constructor(opts: DomViewportAdapterOptions) {
    this.kernel = opts.kernel;
    this.scope = opts.scope;
    this.viewer = opts.viewer;
    this.pagesContainer = opts.pagesContainer;
    this.viewerEl = opts.viewerEl;
    this.onPageWindowed = opts.onPageWindowed;
    // DELIBERATE (root-scoping): keep a realm-window resize
    // listener, NOT a ResizeObserver(root). onResize re-snaps the device grid on
    // DPR/browser-zoom change, which a CSS-size ResizeObserver does not deliver;
    // an iframe binds its own window (no cross-fire). A ResizeObserver is deferred.
    this.scope.win.addEventListener("resize", this.onResize);
    // A scroll is no longer a free pan — it repaints the visible slice canvases
    // (deduped by the overscan buffer). Passive (no preventDefault) + rAF-coalesced.
    this.viewerEl.addEventListener("scroll", this.onScroll, { passive: true });

    this.observer = new IntersectionObserver((entries) => this.onIntersect(entries), {
      root: this.viewerEl,
      rootMargin: "400px",
    });

    // Drain paint intents from plugin-viewport. Only 'page' intents are ours
    // (a later plugin-grid shares the surface with a different tag).
    this.unsubPaint = this.kernel.paint.onPaintIntent((intent) => {
      if (intent.tag === "page") {
        this.pendingPaints.add(intent.pageIndex);
        this.scheduleFlush();
      } else if (intent.tag === "page-invalidate") {
        // Force-repaint an already-painted visible page (late font face / image).
        this.pendingInvalidates.add(intent.pageIndex);
        this.scheduleFlush();
      }
    });

    // SLOW-NETWORK doc-switch belt: if a prior document's trailing pageReady
    // stream repopulated a just-cleared container, prune any placeholder whose
    // index is beyond the NEW document's final total once its layout completes.
    this.unsubLoadComplete = this.kernel.bus.subscribe("loadComplete", (total) => {
      for (const [idx, pageEl] of this.pageEls) {
        if (idx >= total) {
          this.observer?.unobserve(pageEl);
          pageEl.remove();
          this.pageEls.delete(idx);
          this.visibleSet.delete(idx);
          this.sliceCoverage.delete(idx);
          // The base went with the removed `.page`; a stale ledger entry would
          // otherwise consume the base-retention budget forever.
          this.baseOrder.delete(idx);
        }
      }
    });
  }

  /** Coalesce drained intents per rAF. */
  private scheduleFlush(): void {
    if (this.rafPending) return;
    this.rafPending = true;
    requestAnimationFrame(() => {
      this.rafPending = false;
      if (this.destroyed) return; // a drained flush can land after destroy()
      const batch = [...this.pendingPaints];
      this.pendingPaints.clear();
      // Then drain force-repaints. A page that was just placeholder-painted above
      // and ALSO invalidated in the same frame re-renders once with the latest
      // asset — correct and idempotent.
      const invalidations = [...this.pendingInvalidates];
      this.pendingInvalidates.clear();
      // Decide the ACTUAL to-repaint set BEFORE opening a generation. The intent
      // lists are NOT that set: a 'page' intent paints only a page that is
      // currently visible, and an invalidate only one that already has a
      // placeholder. Guarding on the intent lists (as this did) still opened
      // EMPTY generations — an off-screen 'page' intent, which the pageReady
      // stream emits for every page below the fold while a document lays out,
      // bumped the token and launched nothing, superseding in-flight repaints
      // with nothing queued to redo them. Late fonts land during exactly that
      // window. The same defect was fixed in onScroll/onResize/zoomPaint; this
      // launcher kept it longer because its guard tested the wrong list.
      //
      // ensurePlaceholder is a DOM side effect, not a paint, so it still runs for
      // every intent — here, outside any generation.
      const toPaint: HTMLElement[] = [];
      for (const idx of batch) {
        const pageEl = this.preparePageIntent(idx);
        if (pageEl) toPaint.push(pageEl);
      }
      const toInvalidate = invalidations.filter((idx) => this.lookupPlaceholder(idx) !== null);
      if (toPaint.length === 0 && toInvalidate.length === 0) return;
      // ONE generation for the whole flush (mirrors onScroll/onResize): every
      // sibling paintWindow this drain launches shares the bumped token, so they
      // ALL commit. Per-page generations here were the selection-highlight bug:
      // with 2+ visible pages, each handleInvalidate bump superseded the previous
      // page's paint while it was suspended at `await renderPageWindow`, so only
      // the LAST visible page ever committed an invalidate repaint.
      this.launchPaintGeneration(() => {
        for (const pageEl of toPaint) void this.paint(pageEl);
        for (const idx of toInvalidate) this.handleInvalidate(idx);
      });
    });
  }

  /**
   * For a 'page' intent: (a) ensure a placeholder div+observer exists
   * (idempotent), and (b) paint it if it is currently inside the visible window.
   */
  private preparePageIntent(idx: number): HTMLElement | null {
    const pageEl = this.ensurePlaceholder(idx);
    if (!pageEl) {
      // Cold page-size cache (the async page-size seam): re-queue this intent and prime
      // the cache; primePageSize re-drains once it resolves so the page paints a
      // microtask later. No-op if the cache is warm but pageSize is genuinely
      // unavailable (no document) — then primePageSize's catch keeps it cold and
      // the next real intent retries.
      if (!this.pageSize(idx)) {
        this.pendingPaints.add(idx);
        this.primePageSize(idx);
      }
      return null;
    }
    // A placeholder-add intent (from pageReady) should not force-paint an
    // offscreen page; the IntersectionObserver decides visibility. But if the
    // page is already on screen (the intent came from setVisible, or it scrolled
    // into view before the placeholder existed), it paints.
    return pageEl.dataset.visible === "1" ? pageEl : null;
  }

  /**
   * Force-repaint an ALREADY-painted page in place (the invalidate-visible
   * channel). Unlike paint(), this re-renders onto the EXISTING canvas (DPR
   * transform reset + viewer.renderPage) so a font face / image that landed after
   * the first paint is picked up. No-op if the page has no canvas yet (the normal
   * 'page' paint path will render it with the asset already present). This is the
   * kernel-native re-render channel for late assets.
   */
  private handleInvalidate(idx: number): void {
    const pageEl = this.lookupPlaceholder(idx);
    if (!pageEl) return;
    // The slice canvas is rebuilt on each paint, so a late asset is picked up by
    // simply repainting the current window (re-renders the existing slice at the
    // same geometry). The low-res BASE layer is also re-rendered (cheap at ~1024px)
    // so the periphery preview picks up the late font/image too — drop the stale
    // base so ensureBaseLayer (idempotent on a LIVE base) actually re-renders it.
    this.dropBase(idx);
    void this.ensureBaseLayer(pageEl, idx);
    // Runs under the CALLING flush's generation (bumped once per drain batch in
    // scheduleFlush). That bump still supersedes any older in-flight paint for
    // this page (pre-asset frame can't blit over the fresh one) while letting
    // every sibling page in the SAME batch commit — a per-page bump here dropped
    // all but the last visible page's repaint (the selection-highlight bug).
    void this.paintWindow(pageEl);
  }

  /** The live placeholder for idx from the index map, or null. */
  private lookupPlaceholder(idx: number): HTMLElement | null {
    const el = this.pageEls.get(idx);
    if (el && el.isConnected) return el;
    if (el) {
      // Externally removed (doc-switch container clear) — drop the stale entry.
      this.pageEls.delete(idx);
      this.visibleSet.delete(idx);
    }
    return null;
  }

  /** Create + observe a `.page` placeholder for idx if missing. Returns it. */
  private ensurePlaceholder(idx: number): HTMLElement | null {
    const existing = this.lookupPlaceholder(idx);
    if (existing) return existing;

    const size = this.pageSize(idx);
    if (!size) return null;

    const dpr = this.scope.dpr();
    const page = this.scope.doc.createElement("div");
    page.className = "page";
    page.dataset.page = String(idx);
    // Box dimensions snapped to the device grid so the centred page (and the
    // canvas it holds) lands on whole physical pixels — see device-pixels.ts.
    page.style.width = `${snapLenToDevice(size.widthPx * this.currentScale, dpr)}px`;
    page.style.height = `${snapLenToDevice(size.heightPx * this.currentScale, dpr)}px`;
    const num = this.scope.doc.createElement("div");
    num.className = "page-num";
    num.textContent = `${idx + 1}`;
    page.appendChild(num);
    this.pagesContainer.appendChild(page);
    this.pageEls.set(idx, page);
    this.observer?.observe(page);
    return page;
  }

  /**
   * Per-page size for page `idx` (§17.6) — the cached per-index value, or null
   * until it is primed (see primePageSize). ensurePlaceholder treats null as
   * "not ready yet" and the re-scheduled intent paints once the entry warms.
   * A uniform doc stores the same size under every key.
   */
  private pageSize(idx: number): PageSize | null {
    return this.pageSizes.get(idx) ?? null;
  }

  /**
   * Warm the per-index page-size cache from the async
   * viewer.pageSize(idx) and re-drain pending paints so any 'page' intent that
   * arrived while the entry was cold (returned null from ensurePlaceholder)
   * gets a placeholder + paint. Idempotent per idx; one in-flight prime per idx.
   * idx 0 also seeds {@link cachedPageSize} (the synchronous anchor/fallback).
   */
  private primePageSize(idx: number): void {
    if (this.pageSizes.has(idx) || this.primingPageSizes.has(idx)) return;
    this.primingPageSizes.add(idx);
    void this.viewer
      .pageSize(idx)
      .then((size) => {
        this.primingPageSizes.delete(idx);
        if (this.pageSizes.has(idx)) return;
        this.pageSizes.set(idx, size);
        if (idx === 0) this.cachedPageSize = size;
        // Re-drain any paints that no-op'd on a cold entry. scheduleFlush
        // coalesces; re-add the still-pending indices so they paint now.
        if (this.pendingPaints.size || this.pendingInvalidates.size) this.scheduleFlush();
      })
      .catch(() => {
        // No document loaded yet (pageSize throws). Stay cold; the next paint
        // intent re-primes.
        this.primingPageSizes.delete(idx);
      });
  }

  /**
   * Open a NEW paint generation and run the launch closure that fires this
   * batch's paintWindow call(s). The single choke point that bumps {@link paintSeq}
   * — EVERY paintWindow launcher must go through here, BEFORE any paintWindow
   * begins, so all siblings in the batch share the bumped token (they all commit)
   * while any OLDER in-flight paint reads a stale token and drops. Centralised so a
   * future sixth launcher cannot silently reintroduce the stale-blit window; the
   * bump precedes the closure so the loop inside it launches under the new token.
   */
  private launchPaintGeneration<T>(launch: () => T): T {
    this.paintSeq++;
    return launch();
  }

  /** Paint a page for the FIRST time (scroll-in) — routes to the windowed slice.
   *  Runs under the CALLING flush's generation (scheduleFlush bumps once per
   *  drain batch) — no bump here, or siblings in the batch would supersede each
   *  other and drop all but the last page's commit. */
  private async paint(pageEl: HTMLElement): Promise<void> {
    await this.paintWindow(pageEl);
  }

  /**
   * PERSISTENT LOW-RES BASE LAYER: render the WHOLE page ONCE at low resolution
   * into a canvas that is CSS-stretched (width/height
   * 100%) to fill the full `.page` box at ANY zoom — blurry but ALWAYS covering.
   * It sits UNDER the crisp slice (zIndex 0 vs the slice's 1), so during a
   * transition (zoom-out reveal, fast scroll while the crisp slice is stale) the
   * area the slice doesn't cover shows a blurry preview instead of WHITE, in
   * every direction, sharpening when the crisp slice lands (the map-tile model).
   *
   * Idempotent: a `.page` that already carries a `ps-base` canvas returns
   * immediately. Re-rendered (cheaply, at low-res) on handleInvalidate for late
   * fonts/images; NEVER re-rendered on zoom/scroll (the CSS-stretch covers every
   * scale for free). Removed with the page (child of `.page`) on clear()/switch,
   * and evicted OFF-WINDOW once the retention budget is exceeded, which
   * is safe precisely because this method is a re-derivable pure function of
   * (idx, pageSizes, viewer): see {@link evictBaseLayers}.
   */
  private async ensureBaseLayer(pageEl: HTMLElement, idx: number): Promise<void> {
    // Idempotent: one base render per page. A live ps-base canvas → done (but it
    // was just USED, so it goes to the back of the LRU queue).
    if (pageEl.querySelector("canvas.ps-base")) {
      this.touchBase(idx);
      return;
    }

    // Per-page size (the async page-size seam). ensureBaseLayer only runs after a
    // paintWindow, which only runs once idx's entry is warm (ensurePlaceholder
    // gates on it), so it is populated here; bail defensively if not. Per-idx so
    // a landscape section's base layer gets the landscape aspect, not page 0's.
    const size = this.pageSizes.get(idx) ?? this.cachedPageSize;
    if (!size) return;
    const { widthPx, heightPx } = size;
    if (widthPx <= 0 || heightPx <= 0) return;
    // Longest side ~1024 device px — a quality/cost dial (one render, then it
    // CSS-scales for free at every zoom).
    const baseScale = 1024 / Math.max(widthPx, heightPx);
    const baseW = Math.round(widthPx * baseScale);
    const baseH = Math.round(heightPx * baseScale);
    if (baseW <= 0 || baseH <= 0) return;

    const canvas = this.scope.doc.createElement("canvas");
    canvas.className = "ps-base";
    canvas.dataset.psBase = "1";
    canvas.width = baseW;
    canvas.height = baseH;
    // width/height:100% CSS-stretch it to fill the FULL .page box at ANY zoom
    // (blurry but always covering). zIndex 0 → UNDER the crisp slice (zIndex 1);
    // ordering is by z-index, NOT DOM position (we still insert first for tidiness).
    canvas.style.position = "absolute";
    canvas.style.left = "0";
    canvas.style.top = "0";
    canvas.style.width = "100%";
    canvas.style.height = "100%";
    canvas.style.zIndex = "0";
    canvas.style.display = "block";
    pageEl.insertBefore(canvas, pageEl.firstChild);
    // The ledger tracks the CANVAS, not the render — record it the moment
    // it exists in the DOM (so a concurrent ensureBaseLayer that early-returns on
    // it cannot see an untracked base) and spend the budget straight away. A page
    // evicted here is by construction off-window; its in-flight renderPage below
    // then draws into a detached canvas and is dropped, which is the same benign
    // outcome as the superseded catch.
    this.touchBase(idx);
    this.evictBaseLayers();

    const ctx = canvas.getContext("2d", { alpha: false });
    if (ctx) {
      ctx.fillStyle = "#ffffff";
      ctx.fillRect(0, 0, baseW, baseH);
      // renderPage draws in layout px (widthPx units); scale so the whole page
      // fills the low-res backing exactly.
      ctx.scale(baseW / widthPx, baseH / heightPx);
    }
    try {
      await this.viewer.renderPage(idx, canvas);
    } catch {
      // SUPERSEDED (doc switched/shrank mid-render) — drop the base rather than
      // leave a half-rendered preview. Benign and timing-inherent.
      // Only OUR canvas, and only OUR ledger entry: if the retention budget evicted
      // this base mid-render the node is already detached (remove() is a no-op)
      // and the entry either belongs to a NEWER base for the same page or was
      // deleted with the eviction — deleting it here would strand that newer one.
      const wasLive = canvas.parentElement !== null;
      canvas.remove();
      if (wasLive) this.baseOrder.delete(idx); // keep the ledger == the DOM
      return;
    }
    // No onPageWindowed here: the base is a passive preview layer, never
    // hit-tested (the slice/.page own interaction).
  }

  /** Move idx to the MRU end of {@link baseOrder} (Set re-insert = move). */
  private touchBase(idx: number): void {
    this.baseOrder.delete(idx);
    this.baseOrder.add(idx);
  }

  /**
   * Drop a page's `ps-base` canvas AND its ledger entry. The two must never
   * drift — a canvas left out of the ledger is unevictable, a ledger entry left
   * without a canvas silently consumes the retention budget forever — so the rule
   * is spelled here once and every deliberate removal goes through it.
   */
  private dropBase(idx: number): void {
    this.pageEls.get(idx)?.querySelector("canvas.ps-base")?.remove();
    this.baseOrder.delete(idx);
  }

  /**
   * Hold the retained `ps-base` previews at |window| + BASE_RETAIN_OFF_WINDOW
   * by dropping the least-recently-used OFF-WINDOW ones.
   *
   * THE CORRECTNESS ARGUMENT (pinned by base-layer-bound.test.ts): a base inside
   * the window is never a candidate, and a page outside an IntersectionObserver
   * window with rootMargin "400px" is ≥400 px from the viewport — so removing its
   * base changes ZERO on-screen pixels, and unpaint() has already dropped its
   * crisp slice. Nothing in the tree reads `ps-base` (it is z-index 0 under the
   * slice, never hit-tested), and ensureBaseLayer regenerates it identically from
   * (idx, pageSizes, viewer), which handleInvalidate has relied on in production
   * since the late-font channel. The single observable difference is a transient:
   * a re-entering evicted page shows its preview one renderPage later — the same
   * window that already exists on the FIRST visit to any page.
   *
   * Called from exactly two places, which between them cover both ways the bound
   * can be breached: base CREATION grows the ledger, and onIntersect shrinks the
   * exempt (in-window) set. O(|baseOrder|), itself bounded by this method.
   */
  private evictBaseLayers(): void {
    let offWindow = 0;
    for (const idx of this.baseOrder) if (!this.visibleSet.has(idx)) offWindow++;
    let over = offWindow - DomViewportAdapter.BASE_RETAIN_OFF_WINDOW;
    if (over <= 0) return;
    // Insertion order ⇒ least-recently-used first. Deleting the current entry
    // mid-iteration is well-defined for Set.
    for (const idx of this.baseOrder) {
      if (over <= 0) break;
      if (this.visibleSet.has(idx)) continue; // ON SCREEN — never evictable
      this.dropBase(idx);
      over--;
    }
  }

  /**
   * Paint the VIEWPORT SLICE of a page rather than a full page canvas. The `.page`
   * PLACEHOLDER stays the full page box (scroll extent + coordinate anchor —
   * untouched). The CANVAS is sized to the intersection of the page box with the
   * #viewer viewport and positioned absolutely inside the page at that
   * intersection; everything outside the canvas bounds is clipped by the browser,
   * so only the visible slice rasterises and the backing dims stay viewport-bounded
   * at any zoom. Repainted on scroll + zoom-settle.
   *
   * The slice is rendered through the EXISTING viewer.renderPage by PRE-
   * TRANSFORMING the ctx: ctx is scaled by scale·dpr (page-logical → device px)
   * then translated by −clip so the page's visible region lands at the canvas
   * origin. renderPage draws the whole page in page-logical coords; the browser
   * clips the rest. (renderPage honours the incoming ctx transform — the full-
   * page path relies on the same ctx.scale today.)
   */
  private async paintWindow(pageEl: HTMLElement): Promise<void> {
    const idx = Number(pageEl.dataset.page);
    const dpr = this.scope.dpr();
    const scale = this.currentScale;
    // BACK-BUFFER FIX — capture the paint-generation token AND the gesture flag at
    // ENTRY. `seq` gates the post-await commit (a newer generation supersedes this
    // paint → it drops without touching the visible canvas). `gesturingAtStart` is
    // used for BOTH the slice-offset snap (below) and the page-element snap (in the
    // commit block) so an in-flight eased paint never blits geometry that mismatches
    // the scale it was rendered at, even if this.gesturing flips true→false mid-RPC;
    // a settle's own newer generation repaints device-snapped and supersedes this.
    // Do NOT read live this.gesturing anywhere after this point in the method.
    const seq = this.paintSeq;
    const gesturingAtStart = this.gesturing;

    // Intersection of the full page box with the #viewer viewport (CSS px),
    // EXPANDED by the OVERSCAN buffer. The overscan is in SCREEN px (a fraction of
    // the viewport), so the slice stays viewport-bounded (~ (1 + 2·OVERSCAN)×
    // viewport) and does NOT grow with zoom — the backing can't blow up at extreme
    // zoom. Native scroll then translates the oversized slice for free; onScroll
    // only repaints when the viewport nears the buffer edge (the dedup above).
    const pageRect = pageEl.getBoundingClientRect();
    const viewRect = this.viewerEl.getBoundingClientRect();
    const overscanX = viewRect.width * DomViewportAdapter.OVERSCAN;
    const overscanY = viewRect.height * DomViewportAdapter.OVERSCAN;
    const visLeft = Math.max(pageRect.left, viewRect.left - overscanX);
    const visTop = Math.max(pageRect.top, viewRect.top - overscanY);
    const visRight = Math.min(pageRect.right, viewRect.right + overscanX);
    const visBot = Math.min(pageRect.bottom, viewRect.bottom + overscanY);
    const visWCss = visRight - visLeft;
    const visHCss = visBot - visTop;
    // Off-screen (or zero-area) → no SLICE. Drop any stale slice and its coverage
    // record (so a stale rect can't suppress a needed repaint on re-entry). The
    // base layer (ps-base) is left in place — it keeps the periphery from flashing
    // white if the page re-enters; the retention budget (evictBaseLayers, run from
    // onIntersect) and clear()/doc-switch are what eventually remove it.
    if (visWCss <= 0 || visHCss <= 0) {
      pageEl.querySelector("canvas.ps-slice")?.remove();
      this.sliceCoverage.delete(idx);
      return;
    }

    // The slice's offset inside the page box (CSS px) → snapped to whole device
    // px so the canvas left/top edges land on the physical grid (no resample).
    // Mid-zoom-gesture: keep it RAW (smooth scale, no per-frame re-quantize =
    // no shake); the settle paint snaps it for crisp edges at rest.
    const visLeftInPageCss = visLeft - pageRect.left;
    const visTopInPageCss = visTop - pageRect.top;
    const leftCss = gesturingAtStart ? visLeftInPageCss : snapLenToDevice(visLeftInPageCss, dpr);
    const topCss = gesturingAtStart ? visTopInPageCss : snapLenToDevice(visTopInPageCss, dpr);
    // Page-logical clip origin (what region of the page the slice shows).
    const clipXLogical = leftCss / scale;
    const clipYLogical = topCss / scale;

    // Backing store at whole device px; CSS box = backing / dpr (1:1 physical).
    const bw = Math.round(visWCss * dpr);
    const bh = Math.round(visHCss * dpr);
    if (bw <= 0 || bh <= 0) {
      pageEl.querySelector("canvas.ps-slice")?.remove();
      this.sliceCoverage.delete(idx);
      return;
    }

    // PERSISTENT LOW-RES BASE LAYER: ensure a full-page blurry render exists
    // UNDER the crisp slice so the periphery the slice doesn't cover shows a
    // preview instead of white (zoom-out reveal / fast scroll). Idempotent +
    // independent of the slice (rendered once per page). Awaited NOT — it owns
    // its own superseded handling; the slice below proceeds either way.
    void this.ensureBaseLayer(pageEl, idx);

    // BACK-BUFFER FIX — render into a DETACHED, per-call scratch canvas, NEVER the
    // visible one. Per-call (not per-page reuse) because zoomPaint's Promise.all
    // paints multiple pages concurrently AND its trailing batch can re-paint the
    // SAME page while the first is mid-RPC; a per-idx scratch would race, whereas a
    // detached per-call buffer cannot be clobbered by any sibling or successor. It
    // is NOT inserted into the DOM → zero layout/paint cost; it is GC'd right after
    // the synchronous blit below, so there is no retained-memory growth.
    const back = this.scope.doc.createElement("canvas");
    back.width = bw;
    back.height = bh;
    // The white pre-fill MOVES off the visible canvas onto the back buffer (the
    // visible canvas is not touched until finished pixels exist). renderPageWindow's
    // own getContext("2d") returns this SAME alpha:false context.
    const bctx = back.getContext("2d", { alpha: false });
    if (bctx) {
      // Plain white pre-fill (page ground for any strip the window-reduced bg
      // fill doesn't cover at the page edge). The old content-improbable SENTINEL
      // pre-fill + 5×1×1 getImageData probes are GONE: even a 1×1 getImageData
      // forces a synchronous raster flush + GPU readback of everything pending on
      // the canvas — measured 277/104/54ms stalls on a cold zoom burst and
      // 3-16ms per committed paint warm, firing on EVERY gesture commit (that
      // judder was one cause of zoom shake). Emptiness now arrives OUT-OF-BAND:
      // renderPageWindow resolves `false` when the worker display list recorded
      // ZERO ops (see the `drew` gate below) — zero readbacks, same
      // degrade-to-blurry-base behavior.
      bctx.fillStyle = "#ffffff";
      bctx.fillRect(0, 0, bw, bh);
      // render_page_window sets its OWN SCALE-ONLY ctx transform (device = s·window
      // local, no giant translate) from the passed clip/zoom/dpr; we only own the
      // white pre-fill (on the back buffer, not the visible canvas).
    }
    let drew: boolean | undefined;
    try {
      // O(visible) windowed render into the BACK buffer — identical bw×bh
      // device dims, so it is a drop-in target. render_page_window owns the ctx
      // transform + clip; we pass page-logical clip origin, the CSS-px visible
      // slice, the scale and the dpr. Resolves false ⇔ the worker recorded an
      // EMPTY display list (drew nothing); void/true ⇔ content (in-process paints
      // same-task through the window-local path and is trusted).
      drew = await this.viewer.renderPageWindow(
        idx,
        back,
        clipXLogical,
        clipYLogical,
        visWCss,
        visHCss,
        scale,
        dpr,
      );
    } catch {
      // SUPERSEDED (doc switched/shrank mid-paint) — drop the BACK buffer (let it
      // GC) and return WITHOUT canvas.remove(): the visible ps-slice was never
      // touched and still shows valid old pixels, so removing it would be the
      // regression. Benign and timing-inherent.
      return;
    }

    // Defense-in-depth (window-local fix), now READBACK-FREE: an explicitly-empty
    // render (`false` — the recorded display list had zero ops) skips the blit so
    // the page degrades to the blurry full-page ps-base, never an opaque block.
    // `void`/`true` (in-process, older mocks) blits normally.
    if (drew === false) return;

    // ─── SYNCHRONOUS COMMIT — NO await, NO yield from here to the blit ───────────
    // THE INVARIANT: between the first mutation of the VISIBLE canvas (step 3) and
    // the drawImage blit (step 7) there is no await and no yield, so the compositor
    // never paints an intermediate cleared/white frame — the whole block runs in one
    // task and the next composited frame is the finished blit. Zero white, both modes.
    //
    // 1. Superseded by a NEWER paint generation → drop; never touch the visible
    //    canvas or coverage (a dropped paint must not blit stale pixels over a fresh
    //    frame, nor poison the onScroll dedup with a rect for pixels that never landed).
    if (!shouldCommitPaint(seq, this.paintSeq)) return;
    // 2. Page removed mid-await (visibleSet churn / doc switch): the detached back
    //    buffer is GC'd, nothing leaks.
    if (!pageEl.isConnected) return;
    // 3. Reuse the existing SLICE canvas if present (scroll repaint) so we don't
    //    churn DOM nodes every frame; otherwise create + insert one. Tagged
    //    `ps-slice` (zIndex 1) so it's never confused with the ps-base (zIndex 0).
    let canvas = pageEl.querySelector<HTMLCanvasElement>("canvas.ps-slice");
    if (!canvas) {
      canvas = this.scope.doc.createElement("canvas");
      canvas.className = "ps-slice";
      canvas.style.position = "absolute";
      canvas.style.zIndex = "1";
      pageEl.insertBefore(canvas, pageEl.firstChild);
    }
    // 4. Size the backing store. `canvas.width = bw` clears the backing store AND
    //    resets the 2D ctx transform to identity — this MUST precede the getContext
    //    /drawImage in step 7 so the blit is a 1:1 device copy (the back buffer
    //    already holds scale·dpr-baked pixels). Do NOT reorder the width assignment
    //    after the blit or the drawImage would resample.
    canvas.width = bw;
    canvas.height = bh;
    canvas.style.width = `${bw / dpr}px`;
    canvas.style.height = `${bh / dpr}px`;
    canvas.style.left = `${leftCss}px`;
    canvas.style.top = `${topCss}px`;
    // 5. OVERSCAN coverage — recorded ONLY on commit, with l/t EQUAL to the applied
    //    canvas.style.left/top (same sync block, they cannot diverge). onScroll
    //    dedups the viewport against this to skip repaints while the oversized slice
    //    still covers the (translated) viewport.
    this.sliceCoverage.set(idx, {
      l: leftCss,
      t: topCss,
      r: leftCss + visWCss,
      b: topCss + visHCss,
    });
    // 6. Snap the page's left edge onto a whole device pixel (settle-only, via the
    //    CAPTURED gesturingAtStart — a live this.gesturing read here could snap the
    //    page under a stale generation and mismatch the back buffer's rendered scale).
    if (!gesturingAtStart) snapElementToDeviceGrid(pageEl, this.scope.dpr());
    // 7. Blit finished pixels onto the visible canvas. Identity transform (fresh
    //    context after the step-4 width reset) → 1:1 device copy, no resample.
    const vctx = canvas.getContext("2d", { alpha: false });
    if (vctx) vctx.drawImage(back, 0, 0);
    // 8. Gesture-time content tracking (the zoom-tremor fix): stamp the scale
    //    + geometry these pixels were painted at, then apply the tracking
    //    transform for the CURRENT live scale in the SAME sync block as the blit.
    //    NOT a bare identity reset: if the gesture eased on during the RPC, the
    //    committed pixels are already one RPC stale — identity would compose them
    //    unscaled against the live box for a frame (a single hold-then-leap, the
    //    very tremor). sliceTrackTransform collapses to "" when live == painted
    //    (settle / no drift). setScale's gesture loop keeps the transform current
    //    on every eased frame between commits, so content ALWAYS tracks the box.
    canvas.dataset.psScale = String(scale);
    canvas.dataset.psLeft = String(leftCss);
    canvas.dataset.psTop = String(topCss);
    canvas.style.transformOrigin = "0 0";
    canvas.style.transform = sliceTrackTransform(scale, this.currentScale, leftCss, topCss);
    // ─── end synchronous commit block ──────────────────────────────────────────

    // Hit-test listeners live on the STABLE .page (the slice canvas is recreated on
    // scroll), wired once per .page via the seam's own dedup guard. Fires only for
    // COMMITTED (non-superseded) paints now, so wiring is not attached for dropped
    // paints. attach-dom wires excel/comments/links to the .page via onPageWindowed.
    this.onPageWindowed?.(idx, pageEl);
  }

  /**
   * Single-tier zoom (driven by plugin-zoom's `zoom:changed`, eased per-frame by
   * attach-dom). There is ONE layout at every scale: resize the real `.page` boxes,
   * position via scroll from the `off`-baked camera, and crisp-repaint the visible
   * pages — coalesced during a burst, run to completion at the final settle.
   *
   * There is NO container transform. The inter-page gap, `.ps-viewer` padding,
   * `.page-num` labels and shadows are therefore NEVER in a scaled state — they
   * render at fixed CSS size at every frame, exactly as at rest. With no second,
   * transform-scaled layout to reconcile against, the entire settle-SNAP class —
   * page-N gap jumps, the `--ps-s5` padding lurch, the scroll-overflow clamp/kick — is
   * structurally impossible (there is nothing to snap from). `gesturing` no longer
   * selects a layout; it only chooses COALESCE (cheap, mid-burst) vs paint to
   * completion (final settle). The crisp re-raster is O(visible) per the grid's
   * proven per-tick model; the persistent `ps-base` layer covers any sub-frame gap.
   */
  setScale(scale: number, gesturing: boolean): void {
    const size = this.cachedPageSize;
    if (!size) {
      this.zoomLatch = this.zoomAnchor = this.gestureAnchor = null;
      return;
    }
    // Latch the cursor's content point ONCE per burst (first tick), so the anchor
    // is stable as the scale eases. A discrete zoom.set (gesturing=false, no prior
    // latch) latches → uses → clears in this single call.
    if (this.zoomLatch === null) this.latchZoom();
    this.gesturing = gesturing;
    const same = scale === this.currentScale;
    this.currentScale = scale;
    const dpr = this.scope.dpr();
    // ONE layout: resize the real page boxes. The gap/padding/labels are fixed CSS,
    // scaled by NOBODY, so they cannot desync from the boxes. Mid-gesture the box
    // size is RAW (smooth scale); the settle frame snaps to whole device px (crisp).
    // Each page is sized from ITS OWN geometry (§17.6) so a landscape section page
    // stays landscape at every zoom; a uniform doc uses one repeated size (page 0).
    for (const pageEl of this.pagesContainer.querySelectorAll<HTMLElement>(".page")) {
      const ps = this.pageSizes.get(Number(pageEl.dataset.page)) ?? size;
      const { widthPx, heightPx } = ps;
      pageEl.style.width = `${gesturing ? widthPx * scale : snapLenToDevice(widthPx * scale, dpr)}px`;
      pageEl.style.height = `${gesturing ? heightPx * scale : snapLenToDevice(heightPx * scale, dpr)}px`;
      // Gesture-time content tracking (the zoom-tremor fix): the box + ps-base
      // rescale to the live eased scale every frame, but crisp slice commits land
      // at worker-RPC cadence carrying paintedScale pixels. When the RPC exceeds a
      // frame the sharp content holds-then-leaps 12-17% at ~commit cadence over the
      // smoothly-scaling base — the measured pinch tremor. Rescale the slice by
      // live/painted (about the PAGE origin — see sliceTrackTransform) every eased
      // frame so its content tracks the box between commits; a landing commit
      // swaps in sharper pixels and re-derives the transform (see paintWindow
      // step 8). Settle needs nothing here: the settle repaint commits at the
      // final scale, where the transform collapses to identity.
      if (gesturing) {
        const slice = pageEl.querySelector<HTMLCanvasElement>("canvas.ps-slice");
        const painted = Number(slice?.dataset.psScale);
        if (slice && Number.isFinite(painted) && painted > 0) {
          slice.style.transformOrigin = "0 0";
          slice.style.transform = sliceTrackTransform(
            painted,
            scale,
            Number(slice.dataset.psLeft ?? "0"),
            Number(slice.dataset.psTop ?? "0"),
          );
        }
      }
    }
    // Position via the off-baked camera (now scroll-only). The layout is REAL at
    // every frame, so the browser's scroll-overflow is correct every frame — no
    // clamp to fight, no scrollLeft pin. `behavior:"instant"` is LOAD-BEARING:
    // `.ps-viewer` has `scroll-behavior: smooth` (TOC/comment nav) which would
    // otherwise animate this into a glide.
    this.viewerEl.scrollTo({
      left: Math.max(0, this.cameraAxis(scale, "x")),
      top: Math.max(0, this.cameraAxis(scale, "y")),
      behavior: "instant",
    });
    // Crisp windowed repaint of the visible pages, coalesced so a fast eased burst
    // never queues N concurrent paints. Geometry above is synchronous, so the view
    // tracks at 60fps even if crisp pixels lag a frame (ps-base covers the gap).
    // Repaint when the scale moved, AND always on the settle frame (!gesturing) so
    // the final crisp pixels are the device-SNAPPED ones even if the settle scale
    // equals the last eased value.
    if (!same || !gesturing) this.requestZoomPaint();
    if (!gesturing) this.zoomLatch = this.zoomAnchor = this.gestureAnchor = null;
  }

  /**
   * Latch the zoom anchor for a burst: the cursor's client point + the unzoomed
   * content coord under it (X from the page's left edge, Y from the container's
   * visible top), at the current scale. Measured in live untransformed geometry.
   */
  private latchZoom(): void {
    this.gestureAnchor = this.zoomAnchor;
    const a = this.gestureAnchor;
    const vRect = this.viewerEl.getBoundingClientRect();
    const cursorX = a ? a.x : vRect.left + vRect.width / 2;
    const cursorY = a ? a.y : vRect.top + vRect.height / 2;
    const pEl = this.pagesContainer.querySelector<HTMLElement>(".page");
    const pRect = (pEl ?? this.pagesContainer).getBoundingClientRect();
    const cTop = this.pagesContainer.getBoundingClientRect().top;
    const base = this.currentScale || 1;
    this.zoomLatch = {
      cursorX,
      cursorY,
      contentX: (cursorX - pRect.left) / base,
      contentY: (cursorY - cTop) / base,
    };
  }

  /** Request a crisp repaint of the visible pages; coalesced (grid pattern). */
  private requestZoomPaint(): void {
    if (this.zoomPainting) {
      this.zoomRepaintQueued = true;
      return;
    }
    void this.zoomPaint();
  }

  /**
   * Repaint the crisp windowed slice of every currently-visible page at
   * `this.currentScale`. Serialised like dom-grid-adapter's paint(): at most one
   * in-flight batch + one trailing batch, so an eased burst that fires many frames
   * collapses to bounded work; intermediate frames are covered by the resized box +
   * the `ps-base` low-res layer (blurry-but-correctly-positioned), sharpening when
   * the trailing batch lands.
   */
  private async zoomPaint(): Promise<void> {
    this.zoomPainting = true;
    try {
      // ONE generation for the whole visible-set batch: bump once, THEN launch all
      // siblings so they share the token and every page commits (a per-page bump
      // would make each later sibling supersede the earlier one and multi-page
      // zoom paints would never commit). A NEWER generation (scroll / invalidate /
      // trailing zoom batch) is the only thing that supersedes them. Collect the
      // pages FIRST: if the filter admits none (visibleSet churn / a burst that
      // outlives the pages) an EMPTY generation would supersede every in-flight
      // paint with nothing queued to redo it — see the SECOND INVARIANT on paintSeq.
      const els: HTMLElement[] = [];
      for (const idx of this.visibleSet) {
        const el = this.pageEls.get(idx);
        if (el?.isConnected && el.dataset.visible === "1") els.push(el);
      }
      if (els.length > 0) {
        const work = this.launchPaintGeneration(() => els.map((el) => this.paintWindow(el)));
        await Promise.all(work);
      }
    } finally {
      this.zoomPainting = false;
      if (this.zoomRepaintQueued) {
        this.zoomRepaintQueued = false;
        void this.zoomPaint();
      }
    }
  }

  /**
   * Scroll for one axis — the continuous `off`-baked camera form, the single
   * positioning model (it once fed a transform too; now it only feeds scrollTo).
   * Reads the latched cursor/content point + LIVE viewport geometry (accurate
   * because there is no transform). For target `scale`:
   *   len       = content extent (page width·scale on X; the REAL stack height,
   *               gaps included, from scrollHeight, on Y — exact, no approximation)
   *   off       = max(0, (V − len)/2) on X (CSS `safe center` realises it); 0 on Y
   *   maxScroll = max(0, len − (V − 2·pad))   // pad = viewer padding on this axis
   *   scroll    = clamp(content·scale + off − (cursor − viewerStart), 0, maxScroll)
   * As content fills the viewport both off and maxScroll → 0, so scroll → 0 from
   * either side: the anchor↔fit crossover is C0 (no kick). Wide → zoom-to-cursor;
   * fits → scroll pins to 0 and CSS centres (X) / top-aligns (Y).
   */
  private cameraAxis(scale: number, axis: "x" | "y"): number {
    const latch = this.zoomLatch;
    if (!latch) return axis === "x" ? this.viewerEl.scrollLeft : this.viewerEl.scrollTop;
    const x = axis === "x";
    const size = this.cachedPageSize;
    const vRect = this.viewerEl.getBoundingClientRect();
    if (this.viewerPadTop < 0) {
      this.viewerPadTop = parseFloat(getComputedStyle(this.viewerEl).paddingTop) || 0;
    }
    const len = x
      ? size
        ? size.widthPx * scale
        : Number.POSITIVE_INFINITY
      : this.pagesContainer.scrollHeight; // real height incl. fixed gaps (post-resize)
    const V = x ? this.viewerEl.clientWidth : this.viewerEl.clientHeight;
    const pad = x ? 0 : this.viewerPadTop;
    const viewerStart = x ? vRect.left : vRect.top + pad;
    const cursor = x ? latch.cursorX : latch.cursorY;
    const content = x ? latch.contentX : latch.contentY;
    const off = x && len < V ? (V - len) / 2 : 0;
    const maxScroll = Math.max(0, len - (V - 2 * pad));
    return Math.min(maxScroll, Math.max(0, content * scale + off - (cursor - viewerStart)));
  }

  /**
   * Set the cursor anchor (client coords) for the NEXT zoom step, so it zooms
   * toward the pointer instead of the centre. Call from the wheel handler before
   * dispatching zoom.in/out. Cleared automatically after each settle.
   */
  setZoomAnchor(clientX: number, clientY: number): void {
    this.zoomAnchor = { x: clientX, y: clientY };
  }

  /** Remove a page's crisp slice when it leaves the window. */
  private unpaint(pageEl: HTMLElement): void {
    // The page carries TWO canvases — drop only the crisp SLICE (ps-slice) and
    // KEEP the low-res base (ps-base) so a re-entry shows a blurry preview
    // immediately instead of white. The base is not free, though: a budget caps how
    // many OFF-WINDOW ones survive, so the caller (onIntersect) runs the budget
    // after the batch. clear()/doc-switch drops the rest (they are .page children).
    pageEl.querySelector("canvas.ps-slice")?.remove();
    // OVERSCAN: drop the coverage record with the slice so a stale rect can't
    // suppress a needed repaint when the page re-enters the window.
    this.sliceCoverage.delete(Number(pageEl.dataset.page));
  }

  /**
   * IntersectionObserver callback: track per-page visibility, then report the
   * full current visible set to plugin-viewport via `viewport.setVisible`. The
   * plugin emits a 'page' intent for each newly-visible index, which drains back
   * here and paints. Pages leaving the window are unpainted directly.
   */
  private onIntersect(entries: IntersectionObserverEntry[]): void {
    for (const entry of entries) {
      const pageEl = entry.target as HTMLElement;
      const idx = Number(pageEl.dataset.page);
      if (entry.isIntersecting) {
        pageEl.dataset.visible = "1";
        this.visibleSet.add(idx);
      } else {
        delete pageEl.dataset.visible;
        this.visibleSet.delete(idx);
        this.unpaint(pageEl);
      }
    }
    // Pages that just left the window became EVICTABLE base layers. Run the
    // budget once per batch (not per page) — the other half of the pair that keeps
    // the retained preview store bounded; see evictBaseLayers.
    this.evictBaseLayers();
    // The tracked set replaces a full-container '[data-visible]' selector scan
    // (O(pages) per intersection batch — pathological at 20k+ lazy pages).
    void this.kernel.exec("viewport.setVisible", [...this.visibleSet]);
  }

  /** Doc switch: disconnect observer, empty the container, reset the plugin slice. */
  clear(): void {
    this.observer?.disconnect();
    this.observer = null;
    this.pendingPaints.clear();
    this.pendingInvalidates.clear();
    this.rafPending = false;
    this.pagesContainer.innerHTML = "";
    this.pageEls.clear();
    this.visibleSet.clear();
    this.sliceCoverage.clear();
    // innerHTML="" took every ps-base with it (they are children of `.page`).
    this.baseOrder.clear();
    // Drop the cached page sizes so a new document (possibly different sizes)
    // re-primes from its own viewer.pageSize(idx) on the first paint intent.
    this.cachedPageSize = null;
    this.pageSizes.clear();
    this.primingPageSizes.clear();
    void this.kernel.exec("viewport.reset");
    // Replay parity: a fresh document's pageReady/loadComplete should not catch
    // up on the prior document's stale page lifecycle.
    this.kernel.bus.clearReplay("pageReady");
    this.kernel.bus.clearReplay("loadComplete");
    // Re-arm the observer for the next document.
    this.observer = new IntersectionObserver((entries) => this.onIntersect(entries), {
      root: this.viewerEl,
      rootMargin: "400px",
    });
  }

  /** Full teardown (unsubscribe the paint drain + bus subscriptions). */
  destroy(): void {
    if (this.destroyed) return; // idempotent
    // Set BEFORE the unsubscribes so any rAF that drains between here and the
    // last removeEventListener bails on the flag instead of touching the viewer.
    this.destroyed = true;
    this.unsubPaint();
    this.unsubLoadComplete();
    this.scope.win.removeEventListener("resize", this.onResize);
    this.viewerEl.removeEventListener("scroll", this.onScroll);
    this.observer?.disconnect();
    this.observer = null;
  }
}
