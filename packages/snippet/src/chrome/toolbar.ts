/**
 * Toolbar — the format-aware top chrome ("a control exists iff its data exists").
 *
 * Composes the headless plugins' commands/events into the polished default UI:
 *   - wordmark + mono format tag
 *   - NAV cluster (DOCX page / PPTX slide): a PURE CONSUMER of the kernel-truth
 *     current page — live N via the `viewport:page-changed` bus sub,
 *     typed-N/prev/next jumps via `kernel.exec("viewport.goToPage")`. The DOM
 *     sensing lives in the snippet layer's ONE tracker (page-sense.ts, wired by
 *     attach-dom); the toolbar holds NO observer machinery.
 *   - ZOOM cluster: a hairline-segmented [− | editable% | +] pill + Fit▾ menu
 *   - fullscreen + theme toggle (net-new)
 *   - comments badge (present-only) + the clean-tier status token
 *
 * It owns ONLY toolbar DOM; document paint stays in DomViewportAdapter. The dev
 * toggle is deliberately NOT on the default face (a developer concern) —
 * mount.ts wires the ⌥⇧D chord + option.
 */

import type { Kernel } from "@parasma/headless";
import type { DocumentKind } from "@parasma/core";
import type { RootScope } from "../root-scope";
import { el, flash } from "../dom";
import { icon } from "./icons";

/**
 * @advanced Tier-C seam (`@parasma/snippet/chrome`). Wired by mount.ts as:
 * `{ kernel, scope, computeFit: dom.computeFit, onToggleComments: () =>
 * commentsPanel.toggle(), onToggleTheme/onToggleFullscreen: mount-owned }`.
 */
export interface ToolbarDeps {
  kernel: Kernel;
  /** The DOM seam (realm-safe type guards, activeElement, host document). */
  scope: RootScope;
  /** Compute a fit scale from the live page/stage geometry. */
  computeFit(mode: "width" | "page"): number;
  /**
   * Default true. False removes the Page N/N nav cluster — for a consumer who
   * supplies their OWN navigator (e.g. floating, via `slots.viewportOverlay`),
   * so the toolbar and the BYO nav never duplicate the same affordance.
   * Deliberately the ONLY per-control toolbar flag (a bounded exception, not
   * the start of per-control config — deeper toolbar changes go through the
   * `@parasma/snippet/chrome` factories).
   */
  pageNav?: boolean;
  onToggleComments(): void;
  /** Print button click (word/pptx only — the button hides for spreadsheets). */
  onPrint(): void;
  onToggleTheme(): void;
  onToggleFullscreen(): void;
}

export interface ToolbarHandle {
  root: HTMLElement;
  /** Update for a freshly-loaded document. `name` shows as the doc tooltip. */
  setDocument(kind: DocumentKind, pageCount: number, fmtLabel: string, name?: string): void;
  /** Reflect a streamed page count growing. */
  setPageCount(n: number): void;
  /** Reflect print-prep progress: disables the button and shows d/t in the
   *  tooltip while a run is live; null restores the idle affordance. */
  setPrintProgress(p: { done: number; total: number } | null): void;
  /**
   * Reset the page readout for a fresh load (wired to 'loader:load-started').
   * Without this, setPageCount's within-a-load monotonic max lets a 13-page
   * doc's "/13" flash over a 2-page successor until its pages stream in.
   */
  resetForLoad(): void;
  /**
   * Grid mode (the xlsx default) has REAL zoom (the engine's scale-aware grid
   * paint) — the zoom pill stays live; only the Fit menu hides while grid mode
   * is active (fit derives from the PRINT-page geometry, which is meaningless
   * against the continuous grid). An earlier version hid the pill too, because
   * zoom then had no effect on the grid; with real grid zoom it stays.
   */
  setGridMode(grid: boolean): void;
  /** Reflect the comments drawer's ACTUAL open state (panel ×/doc-switch close). */
  setCommentsOpen(open: boolean): void;
  setZoom(scale: number): void;
  setCommentCount(n: number): void;
  setStatus(state: "idle" | "loading" | "ready" | "error", text: string): void;
  /**
   * Reflect FONT readiness in the status token. While fonts load (especially the
   * multi-second ~16MB Noto CJK OTF) we show a spinner + "loading fonts…" with an
   * optional running count; `loaded`=null clears the font overlay back to idle.
   */
  setFontStatus(loading: boolean, loaded?: number | null): void;
  setTheme(theme: "light" | "dark"): void;
  /** The current top-most visible page (1-based), tracked live. */
  readonly currentPage: number;
  destroy(): void;
}

export function createToolbar(deps: ToolbarDeps): ToolbarHandle {
  const { kernel, scope } = deps;

  // ── Wordmark + format tag ─────────────────────────────────────────────────
  const fmtTag = el("span", { class: "ps-fmt" });
  const brand = el("div", { class: "ps-group", style: "--ps-d:0ms" }, [
    el("span", { class: "ps-wordmark" }, ["PARASMA"]),
    fmtTag,
  ]);

  // ── Nav cluster (DOCX page / PPTX slide) ──────────────────────────────────
  const noun = el("span", { class: "ps-noun" }, ["Page"]);
  const nInput = el("input", {
    class: "ps-n",
    type: "text",
    inputmode: "numeric",
    value: "1",
    "aria-label": "current page",
  });
  const totalEl = el("span", { class: "ps-total" }, ["1"]);
  const readout = el("span", { class: "ps-readout" }, [
    noun,
    nInput,
    el("span", { class: "ps-sep" }, ["/"]),
    totalEl,
  ]);
  const prevBtn = el("button", { class: "ps-btn ps-btn--ps-icon", "aria-label": "previous page" }, [icon("chevron-left")]);
  const nextBtn = el("button", { class: "ps-btn ps-btn--ps-icon", "aria-label": "next page" }, [icon("chevron-right")]);
  const navGroup = el("div", { class: "ps-group ps-nav", style: "--ps-d:40ms" }, [
    prevBtn,
    readout,
    nextBtn,
  ]);
  // BYO-navigator mode (pageNav:false): the cluster never renders.
  const pageNavOn = deps.pageNav !== false;
  if (!pageNavOn) navGroup.style.display = "none";

  // ── Zoom cluster ──────────────────────────────────────────────────────────
  const zoomOut = el("button", { class: "ps-btn ps-btn--ps-icon", "aria-label": "zoom out" }, [icon("minus")]);
  const zoomVal = el("input", {
    class: "ps-zoomval",
    type: "text",
    inputmode: "numeric",
    value: "100%",
    "aria-label": "zoom level",
  });
  const zoomIn = el("button", { class: "ps-btn ps-btn--ps-icon", "aria-label": "zoom in" }, [icon("plus")]);
  const zoomSeg = el("div", { class: "ps-seg" }, [zoomOut, zoomVal, zoomIn]);

  const fitMenu = el("div", { class: "ps-menu" }, [
    el("button", { class: "ps-btn", "data-fit": "width" }, ["Fit width"]),
    el("button", { class: "ps-btn", "data-fit": "page" }, ["Fit page"]),
    el("button", { class: "ps-btn", "data-fit": "100" }, ["100%"]),
  ]);
  const fitBtn = el("button", { class: "ps-btn", "aria-haspopup": "true" }, ["Fit", icon("chevron-down", 14)]);
  const fitWrap = el("div", { class: "ps-fit" }, [fitBtn, fitMenu]);
  const zoomGroup = el("div", { class: "ps-group", style: "--ps-d:80ms" }, [zoomSeg, fitWrap]);

  // ── Right cluster: print, fullscreen, theme, comments, status ─────────────
  const printBtn = el("button", { class: "ps-btn ps-btn--ps-icon", "aria-label": "print", title: "Print" }, [icon("printer")]) as HTMLButtonElement;
  const fullBtn = el("button", { class: "ps-btn ps-btn--ps-icon", "aria-label": "fullscreen", title: "Fullscreen" }, [icon("maximize")]);
  // Shows the TARGET mode, i.e. what a click does: sun = switch to light,
  // moon = switch to dark. setTheme keeps it in sync (default theme is dark →
  // the button offers the sun).
  const themeBtn = el("button", { class: "ps-btn ps-btn--ps-icon", "aria-label": "toggle theme", title: "Toggle theme" }, [icon("sun")]);
  const rightGroup = el("div", { class: "ps-group", style: "--ps-d:120ms" }, [printBtn, fullBtn, themeBtn]);

  const commentsBadge = el("span", { class: "ps-badge" }, ["0"]);
  const commentsBtn = el("button", { class: "ps-btn", "aria-pressed": "false", title: "Comments" }, [
    "Comments ",
    commentsBadge,
  ]);
  const commentsGroup = el("div", { class: "ps-group", style: "--ps-d:120ms" }, [commentsBtn]);
  commentsGroup.style.display = "none";

  const statusDot = el("span", { class: "ps-dot" });
  // The spinner (reduced-motion-aware via CSS) sits where the dot is and is
  // revealed only while fonts load (data-fonts="loading"); the dot hides then.
  const statusSpinner = el("span", { class: "ps-spinner", "aria-hidden": "true" });
  const statusText = el("span", { class: "ps-status-text" }, ["ready"]);
  const status = el("div", { class: "ps-status", "data-state": "idle", style: "--ps-d:160ms" }, [statusDot, statusSpinner, statusText]);
  const statusGroup = el("div", { class: "ps-group", style: "--ps-d:160ms" }, [status]);

  const root = el("div", { class: "ps-toolbar", role: "toolbar" }, [
    brand,
    navGroup,
    zoomGroup,
    el("div", { class: "ps-spacer" }),
    commentsGroup,
    rightGroup,
    statusGroup,
  ]);

  // ── State ─────────────────────────────────────────────────────────────────
  let currentPage = 1;
  let pageCount = 1;
  let kind: DocumentKind | null = null;
  // Last scale from setZoom — restores the pill after a non-numeric entry.
  let lastScale = 1;
  // Size the N input to the digit count of the total (2-digit floor) so 3+
  // digit page numbers don't clip at the old fixed 2.4ch.
  function sizeNInput(): void {
    nInput.style.width = `${Math.max(2, String(pageCount).length) + 0.4}ch`;
  }
  // Loader-set base status, restored when the font-loading overlay clears.
  let lastStatusState: "idle" | "loading" | "ready" | "error" = "idle";
  let lastStatusText = "ready";

  // ── Live page display (pure consumer of the kernel-truth model) ───────────
  // The ONE DOM sensor (page-sense.ts, wired by attach-dom) feeds the viewport
  // plugin; this sub only DISPLAYS. Sticky "latest" replay catches a
  // late-mounted toolbar up to the current page. The `p1 === currentPage` guard
  // also means self-initiated nav (the optimistic write in goTo below) produces
  // NO flash on its own echo — matching the pre-kernel-truth UX (pinned by
  // toolbar.test.ts).
  const unsubPage = kernel.bus.subscribe("viewport:page-changed", ({ index }) => {
    const p1 = index + 1;
    if (p1 === currentPage) return;
    currentPage = p1;
    if (scope.activeElement() !== nInput) nInput.value = String(p1);
    flash(nInput);
  });

  function goTo(page1: number): void {
    // Non-numeric entry ("abc") restores the current page instead of coercing
    // to NaN|0 === 0 and jumping to page 1.
    if (!Number.isFinite(page1)) {
      nInput.value = String(currentPage);
      return;
    }
    // Local clamp for the OPTIMISTIC nInput write (the plugin re-clamps; the
    // bus echo confirms, deduped by the guard above).
    const clamped = Math.max(1, Math.min(pageCount, page1 | 0));
    currentPage = clamped;
    nInput.value = String(clamped);
    void kernel.exec("viewport.goToPage", clamped - 1);
  }

  // ── Events ────────────────────────────────────────────────────────────────
  prevBtn.addEventListener("click", () => goTo(currentPage - 1));
  nextBtn.addEventListener("click", () => goTo(currentPage + 1));
  nInput.addEventListener("change", () => goTo(Number(nInput.value)));
  nInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") goTo(Number(nInput.value));
  });

  zoomOut.addEventListener("click", () => {
    void kernel.exec("zoom.setGesturing", false);
    void kernel.exec("zoom.out");
  });
  zoomIn.addEventListener("click", () => {
    void kernel.exec("zoom.setGesturing", false);
    void kernel.exec("zoom.in");
  });
  const commitZoom = () => {
    const pct = parseInt(zoomVal.value.replace(/[^0-9]/g, ""), 10);
    if (!Number.isNaN(pct) && pct > 0) {
      void kernel.exec("zoom.setGesturing", false);
      void kernel.exec("zoom.set", pct / 100);
    } else {
      // Non-numeric entry ("abc") restores the live value instead of sticking.
      zoomVal.value = `${Math.round(lastScale * 100)}%`;
    }
    zoomVal.blur();
  };
  zoomVal.addEventListener("change", commitZoom);
  zoomVal.addEventListener("keydown", (e) => {
    if (e.key === "Enter") commitZoom();
  });

  const closeMenu = () => fitMenu.removeAttribute("data-open");
  fitBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    if (fitMenu.getAttribute("data-open") === "1") closeMenu();
    else fitMenu.setAttribute("data-open", "1");
  });
  fitMenu.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest<HTMLElement>("[data-fit]");
    if (!btn) return;
    const f = btn.dataset.fit!;
    if (f === "100") {
      void kernel.exec("zoom.setGesturing", false);
      void kernel.exec("zoom.set", 1);
    } else {
      const mode = f as "width" | "page";
      const scale = deps.computeFit(mode);
      void kernel.exec("zoom.setGesturing", false);
      void kernel.exec("zoom.fit", { scale, mode });
    }
    closeMenu();
  });
  scope.doc.addEventListener("click", closeMenu);

  fullBtn.addEventListener("click", () => deps.onToggleFullscreen());
  printBtn.addEventListener("click", () => deps.onPrint());
  themeBtn.addEventListener("click", () => deps.onToggleTheme());
  // aria-pressed is NOT toggled here: the comments panel owns its open state
  // (the × button / doc-switch close used to desync the button); the panel's
  // onOpenChange drives setCommentsOpen below.
  commentsBtn.addEventListener("click", () => deps.onToggleComments());

  return {
    root,
    get currentPage() {
      return currentPage;
    },
    setDocument(k, count, fmtLabel, name) {
      kind = k;
      pageCount = Math.max(1, count);
      currentPage = 1;
      fmtTag.textContent = fmtLabel;
      fmtTag.title = name ?? "";
      // Format-aware: nav shown for word/ppt (paged); xlsx hides it (sheets
      // nav) — and pageNav:false (BYO navigator) keeps it off for every kind.
      const isExcel = k === "excel";
      navGroup.style.display = isExcel || !pageNavOn ? "none" : "";
      printBtn.style.display = isExcel ? "none" : ""; // print: paged kinds only
      // Belt: a stale grid-mode Fit hiding must never survive into a non-excel
      // doc (plugin-grid re-emits 'pages' on reset, but keep the invariant local).
      if (!isExcel) fitWrap.style.display = "";
      noun.textContent = k === "ppt" ? "Slide" : "Page";
      nInput.value = "1";
      totalEl.textContent = String(pageCount);
      sizeNInput();
      // Comments only for word (badge set later via setCommentCount).
      commentsGroup.style.display = "none";
    },
    setPrintProgress(p) {
      printBtn.disabled = p !== null;
      printBtn.title = p ? `Printing… ${p.done}/${p.total}` : "Print";
    },
    setPageCount(n) {
      pageCount = Math.max(pageCount, n);
      totalEl.textContent = String(pageCount);
      sizeNInput();
    },
    resetForLoad() {
      pageCount = 1;
      currentPage = 1;
      nInput.value = "1";
      totalEl.textContent = "1";
      sizeNInput();
    },
    setGridMode(grid) {
      // Grid zoom is real: keep the [− % +] pill; hide only the Fit
      // menu (print-page fit math doesn't apply to the continuous grid).
      fitWrap.style.display = grid ? "none" : "";
    },
    setCommentsOpen(open) {
      commentsBtn.setAttribute("aria-pressed", open ? "true" : "false");
    },
    setZoom(scale) {
      lastScale = scale;
      const pct = Math.round(scale * 100);
      if (scope.activeElement() !== zoomVal) zoomVal.value = `${pct}%`;
      flash(zoomVal);
    },
    setCommentCount(n) {
      if (kind === "word" && n > 0) {
        commentsGroup.style.display = "";
        commentsBadge.textContent = String(n);
      } else {
        commentsGroup.style.display = "none";
        commentsBtn.setAttribute("aria-pressed", "false");
      }
    },
    setStatus(state, text) {
      // The loader status is the BASE; remember it so clearing the font overlay
      // restores it. While the font overlay is up it owns the visible text.
      lastStatusState = state;
      lastStatusText = text;
      if (status.getAttribute("data-fonts") === "loading") return;
      status.setAttribute("data-state", state);
      statusText.textContent = text;
    },
    setFontStatus(loading, loaded) {
      if (loading) {
        status.setAttribute("data-fonts", "loading");
        // Spinner + "loading fonts…" with a running count of arrived faces when
        // we have one (≥1). Progress comes from `fonts:variant-arrived`.
        const n = typeof loaded === "number" && loaded > 0 ? ` ${loaded}` : "";
        statusText.textContent = `loading fonts…${n}`;
      } else {
        status.removeAttribute("data-fonts");
        // Restore the loader's base status (e.g. green "ready").
        status.setAttribute("data-state", lastStatusState);
        statusText.textContent = lastStatusText;
      }
    },
    setTheme(theme) {
      // Target-mode icon: in dark show the sun (click → light), and vice-versa.
      themeBtn.replaceChildren(icon(theme === "dark" ? "sun" : "moon"));
    },
    destroy() {
      unsubPage();
      scope.doc.removeEventListener("click", closeMenu);
    },
  };
}
