/**
 * attachDom — the chrome-free DOM interaction PRIMITIVE.
 *
 * Given a {@link RootScope} + an already-built `{ viewer, kernel }`, this builds
 * the interactive STAGE DOM, constructs + root-scopes the SIX DOM adapters, wires
 * the interaction machinery (zoom-wheel + eased burst, refit-on-resize, per-page
 * hit-test, scroll-to-anchor, scroll-anchor capture/restore), and registers the
 * CORE half of the replayable-bus subscriptions (the ones that touch the stage /
 * adapters / core cache — never the toolbar/formula-bar/etc.). It returns a
 * {@link DomBinding} the composing chrome layer builds on, and is Disposable.
 *
 * This is the extraction FROM mount.ts (not a copy): the code here is the exact
 * core mountViewer used to inline; mountViewer now calls createHeadlessViewer,
 * which calls this, and layers chrome on the returned binding. A parent authoring
 * its OWN chrome imports attachDom directly (the author-your-own-chrome contract).
 *
 * SIDE-EFFECT-FREE / BARREL-FREE: imports no chrome, no snippet barrel, no
 * @fontsource, and registers no custom element.
 */
import { ZOOM_MIN, ZOOM_MAX, type Kernel } from "@parasma/headless";
import type { Viewer, DocumentKind, DocumentInfo } from "@parasma/core";

import { DomViewportAdapter } from "./adapters/dom-viewport-adapter";
import { DomGridAdapter } from "./adapters/dom-grid-adapter";
import { DomSelectionAdapter } from "./adapters/dom-selection-adapter";
import { DomToolAdapter } from "./adapters/dom-tool-adapter";
import { DomClipboardAdapter } from "./adapters/dom-clipboard-adapter";
import { DomContextMenuAdapter } from "./adapters/dom-context-menu-adapter";
import { el } from "./dom";
import { icon } from "./chrome/icons";
import { createFanout } from "./fanout";
import { createHoverThrottle } from "./hover-throttle";
import { createPageSensor } from "./page-sense";
import type { RootScope } from "./root-scope";
import type { AttachDomOptions, DevHooks, DomBinding, LinkActivateEvent } from "./types";
import type { FormulaCell } from "./types";

const NOOP_HOOKS: DevHooks = { log() {}, mark() {}, markLoadStart() {}, refresh() {} };

/**
 * Wire the DOM interaction adapters + core bus subs to `scope` over an already-
 * built `viewer` + `kernel`. Returns the {@link DomBinding} the composer layers
 * chrome on.
 */
export function attachDom(
  scope: RootScope,
  deps: { viewer: Viewer; kernel: Kernel },
  opts: AttachDomOptions = {},
): DomBinding {
  const { viewer, kernel } = deps;
  const hooks = opts.hooks ?? NOOP_HOOKS;

  // ── Stage DOM ───────────────────────────────────────────────────────────────
  const viewerEl = el("div", { class: "ps-viewer" });
  const pagesContainer = el("div", { class: "ps-pages" });
  viewerEl.append(pagesContainer);

  const gridCanvasEl = el("canvas", { class: "ps-grid-canvas" });
  const gridSpacerEl = el("div", { class: "ps-grid-spacer" });
  const gridviewEl = el("div", { class: "ps-gridview" }, [gridSpacerEl, gridCanvasEl]);

  const skelLabel = el("span", { class: "ps-skel-label" }, ["Loading…"]);
  const skeleton = el("div", { class: "ps-skeleton" }, [
    el("div", { class: "ps-skel-paper" }),
    el("div", { class: "ps-skel-status" }, [el("div", { class: "ps-skel-spinner" }), skelLabel]),
  ]);

  const errorMsgEl = el("div", { class: "ps-error-msg" });
  const errorEl = el("div", { class: "ps-error", role: "alert" }, [
    el("div", { class: "ps-error-icon", "aria-hidden": "true" }, ["⚠"]),
    el("div", { class: "ps-error-title" }, ["Couldn't open this document"]),
    errorMsgEl,
  ]);

  const localFontsCta = el(
    "button",
    {
      class: "ps-localfonts-cta",
      type: "button",
      hidden: "",
      title: "Render with the fonts installed on this device (asks for permission once)",
    },
    ["Use installed fonts"],
  ) as HTMLButtonElement;

  // The WORKSPACE column: the document views plus any chrome rows a chrome
  // layer fuses to them (mountViewer prepends the formula row and appends the
  // sheet tabs INSIDE this column, so they read as part of the spreadsheet
  // workspace rather than app-wide strips). Chrome-free here — headless
  // consumers get the bare column. Sits right of any leading panel (the
  // explorer prepends to the STAGE, not the workspace).
  const workspaceEl = el("div", { class: "ps-workspace" }, [viewerEl, gridviewEl]);

  const stage = el("div", { class: "ps-stage" }, [
    workspaceEl,
    skeleton,
    errorEl,
    localFontsCta,
  ]);

  // The tool toggle (built detached; a chrome layer adopts these into its toolbar).
  const selectSegBtn = el("button", {
    class: "ps-btn ps-btn--ps-icon",
    type: "button",
    "aria-label": "Select tool",
    "aria-pressed": "true",
    title: "Select — click & drag to select",
  }) as HTMLButtonElement;
  selectSegBtn.append(icon("mouse-pointer-2"));
  const handSegBtn = el("button", {
    class: "ps-btn ps-btn--ps-icon",
    type: "button",
    "aria-label": "Hand tool",
    "aria-pressed": "false",
    title: "Hand — drag to pan (Space)",
  }) as HTMLButtonElement;
  handSegBtn.append(icon("hand"));

  // ── Core cache ──────────────────────────────────────────────────────────────
  let currentScale = 1;
  let currentKind: DocumentKind | null = null;
  let currentName: string | null = null;
  let lastPageSize: { widthPx: number; heightPx: number } | null = null;
  let lastPageCount = 0;
  let currentSheets: Array<{ name: string; firstPage: number; pageCount: number }> = [];
  let activeSheet = 0;
  let gridMode: "grid" | "pages" = "pages";
  let firstPaintMarked = false;
  let loadToken = 0;

  // ── Fan-out sinks (the formula-bar + comments-panel feeds) ──────────────────
  const cellSelect = createFanout<FormulaCell | null>();
  const commentFocus = createFanout<number>();
  const fanCell = (cell: FormulaCell | null) => cellSelect.fire(cell);
  const fanComment = (id: number) => commentFocus.fire(id);

  // ── Fit computation ─────────────────────────────────────────────────────────
  function computeFit(mode: "width" | "page"): number {
    const size = lastPageSize;
    if (!size) return currentScale;
    const stageW = viewerEl.clientWidth - 48;
    const stageH = stage.clientHeight - 48;
    if (mode === "width") return Math.max(0.1, stageW / size.widthPx);
    return Math.max(0.1, Math.min(stageW / size.widthPx, stageH / size.heightPx));
  }

  // ── Adapters ────────────────────────────────────────────────────────────────
  const viewportAdapter = new DomViewportAdapter({
    kernel,
    scope,
    viewer,
    pagesContainer,
    viewerEl,
    onPageWindowed: (idx, pageEl) => {
      wirePageWindow(idx, pageEl);
      if (!firstPaintMarked) {
        firstPaintMarked = true;
        hooks.mark("first-paint");
      }
    },
  });

  const gridAdapter = new DomGridAdapter({
    kernel,
    scope,
    viewer,
    gridviewEl,
    gridCanvasEl,
    gridSpacerEl,
    viewerEl,
    onCellSelect: (cell) => fanCell(cell),
  });

  const selectionAdapter = new DomSelectionAdapter({
    kernel,
    scope,
    pagesContainer,
    viewerEl,
    getScale: () => currentScale,
    getDocumentKind: () => currentKind,
  });

  const toolAdapter = new DomToolAdapter({
    kernel,
    scope,
    viewerEl,
    gridviewEl,
    root: scope.root as HTMLElement,
    selectButton: selectSegBtn,
    handButton: handSegBtn,
  });

  const clipboardAdapter = new DomClipboardAdapter({
    kernel,
    scope,
    viewer,
    root: scope.root as HTMLElement,
    isInteractiveFocus: () => {
      const a = scope.activeElement() as HTMLElement | null;
      if (!a) return false;
      if (a.hasAttribute("data-clipboard-sink")) return false;
      if (a.isContentEditable) return true;
      return ["INPUT", "TEXTAREA", "SELECT"].includes(a.tagName);
    },
  });

  const contextMenuAdapter = new DomContextMenuAdapter({
    kernel,
    scope,
    root: scope.root as HTMLElement,
    scrollRoot: viewerEl,
    onCopy: () => clipboardAdapter.copyNow(),
  });

  // ── Current-page sensor (kernel-truth current page) ─────────────────────────
  // The ONE DOM tracker: senses the topmost `.page` and feeds the viewport
  // plugin (viewport.setCurrentPage); consumes viewport:scroll-to-page; mirrors
  // the kernel echo for DomBinding.currentPage(). Sensing deliberately stays
  // OUT of DomViewportAdapter (teardown-leak.test.ts pins that adapter's exact
  // acquisition counts). Triggers all piggyback EXISTING subs below — no new
  // IntersectionObserver/MutationObserver.
  const sensor = createPageSensor({
    viewerEl,
    pagesContainer,
    exec: (id, i) => void kernel.exec(id, i),
    subscribe: kernel.bus.subscribe,
    // Grid mode hides the paged surface (all-zero rects) — suppress sensing.
    suspended: () => gridMode === "grid",
  });

  const busUnsubs: Array<() => void> = [];

  // ── The DEFAULT search reveal (core-level, chrome-free) ─────────────────────
  // The search plugin now settles cross-sheet navigation itself and emits
  // `search:navigate-resolved` with final coordinates; the ONLY job left on
  // the DOM side is the actual scroll. Wired HERE (not in the search box) so a
  // BYO-chrome consumer gets hit-reveal with zero wiring; `searchReveal:
  // "manual"` opts out for custom reveal behavior without double-scrolling.
  if ((opts.searchReveal ?? "auto") === "auto") {
    busUnsubs.push(
      kernel.bus.subscribe("search:navigate-resolved", ({ target }) => {
        if (target.kind === "grid") {
          gridAdapter.scrollToContent(target.gridX, target.gridY);
        } else {
          scrollToAnchorPos(target.page, target.y, target.x);
        }
      }),
    );
  }

  // ── Zoom ────────────────────────────────────────────────────────────────────
  busUnsubs.push(
    kernel.bus.subscribe("zoom:changed", ({ scale, gesturing }) => {
      currentScale = scale;
      viewportAdapter.setScale(scale, gesturing);
      // Zoom belt: a zoom-about-anchor at scrollTop=0 can change the topmost
      // page with NO scroll event — re-sense.
      sensor.schedule();
    }),
  );
  const ZOOM_K_PINCH = 0.01;
  const ZOOM_K_WHEEL = 0.0025;
  const ZOOM_MAX_STEP = 1.25;
  const ZOOM_EASE_K = 0.35;
  const ZOOM_SETTLE_MS = 80;
  let zoomTarget: number | null = null;
  let zoomEasing = 1;
  let zoomRaf = 0;
  let lastWheelTs = 0;
  const stepZoom = (): void => {
    zoomRaf = 0;
    if (zoomTarget === null) return;
    const diff = zoomTarget - zoomEasing;
    if (Math.abs(diff) > zoomTarget * 0.005) {
      zoomEasing += diff * ZOOM_EASE_K;
      void kernel.exec("zoom.set", zoomEasing);
      zoomRaf = requestAnimationFrame(stepZoom);
      return;
    }
    if (zoomEasing !== zoomTarget) {
      zoomEasing = zoomTarget;
      void kernel.exec("zoom.set", zoomEasing);
    }
    if (performance.now() - lastWheelTs >= ZOOM_SETTLE_MS) {
      void kernel.exec("zoom.setGesturing", false);
      zoomTarget = null;
    } else {
      zoomRaf = requestAnimationFrame(stepZoom);
    }
  };
  const onZoomWheel = (e: WheelEvent, setAnchor?: (x: number, y: number) => void): void => {
    if (!e.ctrlKey && !e.metaKey) return;
    e.preventDefault();
    const unit = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? viewerEl.clientHeight : 1;
    const dy = e.deltaY * unit;
    const wheelish = e.deltaMode !== 0 || Math.abs(dy) >= 50;
    let factor = Math.exp(-dy * (wheelish ? ZOOM_K_WHEEL : ZOOM_K_PINCH));
    factor = Math.min(ZOOM_MAX_STEP, Math.max(1 / ZOOM_MAX_STEP, factor));
    if (setAnchor) setAnchor(e.clientX, e.clientY);
    if (zoomTarget === null) {
      zoomEasing = currentScale;
      zoomTarget = currentScale;
      void kernel.exec("zoom.setGesturing", true);
    }
    zoomTarget = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, zoomTarget * factor));
    lastWheelTs = performance.now();
    if (!zoomRaf) zoomRaf = requestAnimationFrame(stepZoom);
  };
  const onViewerWheel = (e: WheelEvent) =>
    onZoomWheel(e, (x, y) => viewportAdapter.setZoomAnchor(x, y));
  const onGridWheel = (e: WheelEvent) => onZoomWheel(e, (x, y) => gridAdapter.setZoomAnchor(x, y));
  viewerEl.addEventListener("wheel", onViewerWheel, { passive: false });
  gridviewEl.addEventListener("wheel", onGridWheel, { passive: false });

  // ── Refit on resize ─────────────────────────────────────────────────────────
  let refitRaf = 0;
  const refitObserver = new ResizeObserver(() => {
    // A container resize can change which page is topmost without any scroll or
    // zoom event (the old toolbar IntersectionObserver re-fired on ratio change;
    // this keeps that parity). schedule() is itself rAF-coalesced + grid-guarded.
    sensor.schedule();
    if (refitRaf) return;
    refitRaf = requestAnimationFrame(() => {
      refitRaf = 0;
      void kernel.exec("zoom.fitMode").then((mode) => {
        if (mode !== "width" && mode !== "page") return;
        const scale = computeFit(mode);
        void kernel.exec("zoom.fit", { scale, mode });
      });
    });
  });
  refitObserver.observe(viewerEl);

  // ── Loader/fonts status → stage tier + dev hook (CORE half) ──────────────────
  busUnsubs.push(
    kernel.bus.subscribe("loader:status-changed", (s) => {
      hooks.log("LOADER", s as string, "info");
      if (s === "loading" || s === "planning") {
        stage.setAttribute("data-loading", "1");
        skelLabel.textContent = "Loading…";
      } else if (s === "ready") {
        stage.removeAttribute("data-loading");
        hooks.mark("parse");
      } else if (s === "error") {
        stage.removeAttribute("data-loading");
        stage.setAttribute("data-error", "1");
      }
    }),
  );
  busUnsubs.push(
    kernel.bus.subscribe("loader:error", (err) => {
      stage.removeAttribute("data-loading");
      stage.setAttribute("data-error", "1");
      errorMsgEl.textContent = String((err as Error)?.message ?? err);
      hooks.log("LOADER", String((err as Error)?.message ?? err), "err");
    }),
  );
  // ENGINE PANIC — terminal. The wasm instance has aborted, so
  // nothing more will render; say so instead of leaving a half-drawn page and a
  // spinner that never resolves. Subscribed directly on the Viewer rather than
  // the kernel bus because the panic is a CORE-level fact (the engine handle is
  // dead) and no plugin owns it. The full tagged text, with the Rust source
  // location, goes to the DEV log; the user-facing panel stays plain.
  const offPanic = viewer.on("enginePanic", (text) => {
    stage.removeAttribute("data-loading");
    stage.setAttribute("data-error", "1");
    errorMsgEl.textContent = "The rendering engine stopped unexpectedly.";
    hooks.log("ENGINE", text, "err");
  });

  busUnsubs.push(
    kernel.bus.subscribe("fonts:status-changed", (s) => {
      hooks.log("FONT", s as string, s === "ready" ? "ok" : "info");
      if (s === "planning" || s === "loading") {
        skelLabel.textContent = "Loading fonts…";
      }
      if (s === "ready") hooks.mark("fonts-ready");
    }),
  );
  busUnsubs.push(
    kernel.bus.subscribe("fonts:variant-arrived", ({ family, loadedVariants }) => {
      hooks.log("FONT", `${family} arrived (${loadedVariants})`, "ok");
    }),
  );
  busUnsubs.push(
    kernel.bus.subscribe("loadComplete", (total) => {
      lastPageCount = total as number;
      hooks.mark("layout");
      hooks.log("LAYOUT", `${total} page(s)`, "ok");
      hooks.refresh();
      sensor.schedule();
    }),
  );
  busUnsubs.push(
    kernel.bus.subscribe("pageReady", (idx) => {
      lastPageCount = Math.max(lastPageCount, (idx as number) + 1);
      // Streaming placeholder growth can change the topmost page (rAF-coalesced).
      sensor.schedule();
    }),
  );

  // ── Sheet-aware Pages mode ──────────────────────────────────────────────────
  function applySheetPageFilter(): void {
    const tab = currentSheets[activeSheet];
    if (!tab) return;
    const lo = tab.firstPage;
    const hi = tab.firstPage + tab.pageCount;
    for (const pageEl of pagesContainer.querySelectorAll<HTMLElement>(".page")) {
      const p = Number(pageEl.dataset.page);
      pageEl.style.display = p >= lo && p < hi ? "" : "none";
    }
  }
  function clearSheetPageFilter(): void {
    for (const pageEl of pagesContainer.querySelectorAll<HTMLElement>(".page")) {
      pageEl.style.display = "";
    }
  }
  function scrollToSheetStart(): void {
    const tab = currentSheets[activeSheet];
    if (!tab) return;
    const pageEl = pagesContainer.querySelector<HTMLElement>(`.page[data-page="${tab.firstPage}"]`);
    if (!pageEl) return;
    const delta = pageEl.getBoundingClientRect().top - viewerEl.getBoundingClientRect().top;
    viewerEl.scrollBy({ top: delta, behavior: "smooth" });
  }
  busUnsubs.push(
    kernel.bus.subscribe("sheets:changed", ({ active, sheets }) => {
      currentSheets = sheets;
      activeSheet = active;
      if (sheets.length === 0) {
        clearSheetPageFilter();
        sensor.schedule();
        return;
      }
      if (gridMode === "pages") {
        applySheetPageFilter();
        scrollToSheetStart();
      }
      // Re-sense after the filter applied (hidden pages changed the bands).
      sensor.schedule();
    }),
  );
  busUnsubs.push(
    kernel.bus.subscribe("grid:mode-changed", ({ mode }) => {
      gridMode = mode;
      if (mode === "pages" && currentSheets.length > 0) {
        applySheetPageFilter();
        scrollToSheetStart();
      }
      // Enter-grid is a no-op via the suspended guard (no phantom pageChange);
      // return-to-pages re-senses the restored surface.
      sensor.schedule();
    }),
  );

  // ── Doc-switch clearing (CORE half) ─────────────────────────────────────────
  function clearForLoad(): void {
    viewportAdapter.clear();
    gridAdapter.clear();
    void kernel.exec("grid.setMode", false);
    stage.removeAttribute("data-error");
    errorMsgEl.textContent = "";
    currentSheets = [];
    firstPaintMarked = false;
    localFontsCta.hidden = true;
    // Doc switch: zero the current-page mirror. viewport.reset (inside
    // viewportAdapter.clear above) is deliberately SILENT (no pageChange emit +
    // replay cleared), so the mirror resets here rather than via a bus echo.
    // Covers both the loader:load-started sub and the warm unload() path.
    sensor.reset();
  }
  busUnsubs.push(
    kernel.bus.subscribe("loader:load-started", () => {
      loadToken++;
      lastPageCount = 0;
      clearForLoad();
    }),
  );

  // ── Scroll-anchor currency across a live font refresh ────────────────────────
  function captureScrollAnchor(): { page: number; frac: number } | null {
    const viewTop = viewerEl.getBoundingClientRect().top;
    for (const pageEl of pagesContainer.querySelectorAll<HTMLElement>(".page")) {
      const r = pageEl.getBoundingClientRect();
      if (r.bottom <= viewTop) continue;
      return {
        page: Number(pageEl.dataset.page),
        frac: Math.max(0, (viewTop - r.top) / Math.max(1, r.height)),
      };
    }
    return null;
  }
  function restoreScrollAnchor(anchor: { page: number; frac: number } | null): void {
    if (!anchor) return;
    const pages = pagesContainer.querySelectorAll<HTMLElement>(".page");
    if (pages.length === 0) return;
    let target = pagesContainer.querySelector<HTMLElement>(`.page[data-page="${anchor.page}"]`);
    let frac = anchor.frac;
    if (!target) {
      target = pages[pages.length - 1];
      frac = 1;
    }
    viewerEl.scrollTo({ top: target.offsetTop + frac * target.offsetHeight, behavior: "instant" });
  }
  busUnsubs.push(
    kernel.bus.subscribe("fonts:refreshed", ({ changed }) => {
      if (!changed) return;
      const token = loadToken;
      const anchor = captureScrollAnchor();
      const unsub = kernel.bus.subscribe("loadComplete", () => {
        unsub();
        const i = busUnsubs.indexOf(unsub);
        if (i >= 0) busUnsubs.splice(i, 1);
        if (token !== loadToken) return;
        restoreScrollAnchor(anchor);
        void kernel.exec("viewport.invalidateVisible");
      });
      busUnsubs.push(unsub);
    }),
  );

  // ── Hover throttle (shared across ALL pages) ────────────────────────────────
  // The pointer is over exactly ONE page at a time, so ONE keep-latest throttle
  // serves every page (each sample carries its own pageEl). This coalesces the
  // raw pointermove burst (100+/sec) to at most one cursor.resolve per frame with
  // a single resolve in flight — in WORKER mode that collapses a hover sweep from
  // dozens of serial RPCs to ~frame-rate, and (with the cursor plugin's rect
  // cache) to ZERO RPC after the first hover on a page. In-process the added
  // latency is one frame (<=~16.7ms), below the human perceptual floor. UNIVERSAL
  // (no worker gate) — mirrors the selection adapter, which throttles pointermove
  // unconditionally.
  const hover = createHoverThrottle(
    (idx, x, y) => kernel.exec("cursor.resolve", { page: idx, x, y }),
    requestAnimationFrame,
    cancelAnimationFrame,
  );

  // ── Per-page hit-test wiring ────────────────────────────────────────────────
  function wirePageWindow(idx: number, pageEl: HTMLElement): void {
    if (pageEl.dataset.psWired) return;
    pageEl.dataset.psWired = "1";
    const logical = (ev: { clientX: number; clientY: number }): { x: number; y: number } => {
      const r = pageEl.getBoundingClientRect();
      return { x: (ev.clientX - r.left) / currentScale, y: (ev.clientY - r.top) / currentScale };
    };
    if (currentKind === "excel") {
      pageEl.addEventListener("click", (ev) => {
        const { x, y } = logical(ev);
        void viewer.cellAt(idx, x, y).then((cell) => fanCell(cell));
      });
    }
    pageEl.addEventListener("click", (ev) => {
      const { x, y } = logical(ev);
      void kernel.exec("comments.at", { page: idx, x, y }).then((ids) => {
        if (!ids.length) return;
        void kernel.exec("comments.focus", ids[0]);
        fanComment(ids[0]);
      });
    });
    pageEl.addEventListener("click", (ev) => {
      const { x, y } = logical(ev);
      void kernel.exec("links.linkAt", { page: idx, x, y }).then((d) => {
        if (!d) return;
        // Announce, then navigate unless a listener vetoed. The veto is
        // synchronous because `window.open` needs the click's transient user
        // activation — see LinkActivateEvent. `linkAt` is async, but a fast
        // command resolves well inside the activation window; a human-speed
        // dialog does not, which is why a consumer that wants one prevents
        // first and opens the link itself.
        let prevented = false;
        const ev: LinkActivateEvent = {
          ...d,
          preventDefault() {
            prevented = true;
          },
          get defaultPrevented() {
            return prevented;
          },
        };
        opts.emitLinkActivate?.(ev);
        if (prevented) return;
        // `noreferrer` as well as `noopener`: without it the embedder's URL leaks
        // to the destination in the Referer header.
        if (d.kind === "url") scope.win.open(d.href, "_blank", "noopener,noreferrer");
        else scrollToAnchorPos(d.page, d.y);
      });
    });
    pageEl.addEventListener("pointermove", (ev) => {
      const { x, y } = logical(ev);
      hover.schedule({ pageEl, idx, x, y });
    });
  }

  function scrollToAnchorPos(page: number, y: number, x?: number): void {
    const pageEl = pagesContainer.querySelector<HTMLElement>(`.page[data-page="${page}"]`);
    if (!pageEl) return;
    const top = pageEl.offsetTop + y * currentScale;
    let left: number | undefined;
    if (x !== undefined) {
      const targetX = pageEl.offsetLeft + x * currentScale;
      const viewW = viewerEl.clientWidth;
      const curLeft = viewerEl.scrollLeft;
      if (targetX < curLeft + 24 || targetX > curLeft + viewW - 24) {
        left = Math.max(0, targetX - viewW * 0.25);
      }
    }
    viewerEl.scrollTo({ top: top - 24, left, behavior: "smooth" });
  }

  // ── currentPage — the kernel-truth mirror (1-based public tier) ─────────────
  // SEMANTICS CHANGE: previously an on-demand live DOM read with the
  // bottom>viewTop rule; now the sensor's kernel mirror, updated via
  // scroll→rAF→exec→bus-echo — a read immediately after a programmatic scroll
  // can be ~1 frame stale, and the heuristic is the toolbar's top<=80 rule (the
  // value now always agrees with the toolbar's visible N).
  const currentPage = (): number => sensor.current() + 1;

  return {
    stage,
    workspace: workspaceEl,
    viewerEl,
    pagesContainer,
    gridviewEl,
    gridCanvasEl,
    gridSpacerEl,
    localFontsCta,
    toolButtons: { select: selectSegBtn, hand: handSegBtn },
    adapters: {
      viewport: viewportAdapter,
      grid: gridAdapter,
      selection: selectionAdapter,
      tool: toolAdapter,
      clipboard: clipboardAdapter,
      contextMenu: contextMenuAdapter,
    },
    computeFit,
    scrollToAnchorPos,
    currentPage,
    pageCount: () => lastPageCount,
    getScale: () => currentScale,
    getKind: () => currentKind,
    currentName: () => currentName,
    onCellSelect: (cb) => cellSelect.subscribe(cb),
    onCommentFocus: (cb) => commentFocus.subscribe(cb),
    captureScrollAnchor,
    restoreScrollAnchor,
    loadToken: () => loadToken,
    bumpLoadToken() {
      loadToken++;
    },
    resetForLoad() {
      lastPageCount = 0;
      currentKind = null;
      currentName = null;
      lastPageSize = null;
    },
    clearForLoad,
    setLoaded(info: DocumentInfo, name: string | null) {
      currentKind = info.documentKind;
      currentName = name;
      lastPageSize = info.pageSize;
    },
    destroy() {
      viewerEl.removeEventListener("wheel", onViewerWheel);
      gridviewEl.removeEventListener("wheel", onGridWheel);
      if (zoomRaf) cancelAnimationFrame(zoomRaf);
      refitObserver.disconnect();
      if (refitRaf) cancelAnimationFrame(refitRaf);
      hover.cancel();
      sensor.dispose();
      for (const u of busUnsubs.splice(0)) u();
      offPanic();
      cellSelect.clear();
      commentFocus.clear();
      viewportAdapter.destroy();
      gridAdapter.destroy();
      selectionAdapter.destroy();
      toolAdapter.destroy();
      clipboardAdapter.destroy();
      contextMenuAdapter.destroy();
    },
  };
}
