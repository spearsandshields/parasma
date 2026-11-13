/**
 * DomSelectionAdapter — the snippet's thin DOM adapter for canvas-native
 * SELECTION on page-shaped surfaces (the plugin-selection DOM half). This is
 * the ONLY DOM-touching piece of the page selection feature:
 * it captures pointer + Escape events, maps client coords to
 * (`loc:{surface}`, content-x, content-y) by dividing by the zoom scale (the
 * same `logical()` math the snippet's wirePageWindow uses), and calls the
 * plugin's unified `selection.begin/drag/extendTo/end/clear` commands via
 * `kernel.exec`. The adapter sends the SAME commands for a docx page and
 * a pptx slide — the engine's hit result routes the semantics, so the old
 * per-format `isPml()` command switching is gone. The PLUGIN owns all
 * model/decision; this owns no selection state.
 *
 * REPAINT CONTRACT: `viewer.setSelection`
 * (driven inside the plugin) sets the engine selection but does NOT self-repaint.
 * On every `selection:changed` edge this adapter requests
 * `viewport.invalidateVisible` — the SAME channel the search box uses
 * (`repaintVisible` in mount.ts) — so the engine's additive selection paint pass
 * draws.
 *
 * LATEST-WINS: pointermove is rAF-throttled with one-in-flight + keep-latest at
 * the DOM edge; the plugin's monotonic gesture-seq is the correctness backstop
 * that DROPS a stale async hitTest (so a coarse rAF is safe). Drag is enabled
 * SYNCHRONOUSLY on pointerdown (NOT gated on the awaited begin): in worker mode
 * begin's hitTest is a real RPC, so gating on it dropped every genuine drag whose
 * pointerup/moves outran the RPC — the anchor never engaged and nothing painted.
 * The plugin's store-`dragging` guard makes any pre-anchor drag a no-op (it
 * returns before touching the model or the gesture-seq), and worker hitTest RPCs
 * are FIFO, so begin's anchor still lands before any drag extension applies.
 *
 * MODE-GATED: pointerdown no-ops the selection path entirely — no
 * `setPointerCapture`, no `selection.begin` — when the effective tool mode is not
 * 'select' (cached off the `tool:mode-changed` bus signal, default 'select'). In
 * hand mode the DomToolAdapter owns the pointer for panning, so the two never
 * fight over capture. (The plugin also drops drags in hand mode; gating here
 * additionally stops the adapter from grabbing the pointer out from under the pan.)
 *
 * Escape is handled on `window` but NEVER preventDefault-ed, so it does not
 * swallow the snippet's search-box Escape (mount.ts onKey). Cross-page drags
 * resolve the page under the pointer's clientY (falling back to the down page).
 *
 * GESTURE COMPLETENESS (all mode-gated to 'select'):
 *  - dblclick → `selection.selectWord`; a `click` with `detail>=3` (triple) →
 *    `selection.selectParagraph`. The WORD/PARAGRAPH boundaries are the ENGINE's
 *    hitTest range — never recomputed here. (On a slide the PLUGIN no-ops
 *    them — PML word/paragraph is a declared v1 deferral — so the adapter sends
 *    them unconditionally.)
 *  - shift+pointerdown → `selection.extendTo` (KEEP the anchor, move the focus;
 *    on a slide the plugin re-anchors — PML v1 has no extend); a plain
 *    pointerdown → `selection.begin`. Both then extend via the same
 *    rAF-throttled `selection.drag` on move (shift+drag continues extending).
 *  - EDGE AUTOSCROLL: a rAF loop runs for the life of a drag; when the pointer is
 *    within 24px of the `.ps-viewer` scroll-root edge it scrolls toward it AND
 *    re-fires `selection.drag` at the current pointer so the selection keeps
 *    extending as content scrolls. Stops on pointerup/cancel.
 *  - `contextmenu` → preventDefault + `selection.menuAt` with the content hit
 *    point AND the OPAQUE host `clientX`/`clientY` (the plugin echoes them;
 *    the DomContextMenuAdapter renders the emitted model). Inert in hand mode.
 *    Deferred for PML (the one surviving `getDocumentKind` read): whether the
 *    NATIVE menu shows is a synchronous preventDefault decision the adapter must
 *    make before any engine round-trip, and the pptx custom menu is a declared
 *    v1 deferral — this is per-format interaction policy, not verb routing.
 */

import type { Kernel } from "@parasma/headless";
import type { RootScope } from "../root-scope";

export interface DomSelectionAdapterOptions {
  kernel: Kernel;
  /** The DOM seam (realm window/document, shadow-correct hit-test). */
  scope: RootScope;
  /** The .page host container (delegation root for pointer events). */
  pagesContainer: HTMLElement;
  /** The `.ps-viewer` scroll ROOT — edge autoscroll scrolls this during a drag. */
  viewerEl: HTMLElement;
  /** Current zoom scale (client px → content px divisor). */
  getScale: () => number;
  /**
   * The current document format — read ONLY by the contextmenu deferral (the
   * pptx custom menu is a declared v1 deferral, and native-menu suppression is
   * a sync preventDefault decision). The pointer path no longer routes on
   * it — one command family covers pages and slides.
   */
  getDocumentKind?: () => "word" | "excel" | "ppt" | string | null;
}

/** Edge autoscroll band + speed clamp (client px). */
const AUTOSCROLL_BAND = 24;
const AUTOSCROLL_MAX = 24;
const AUTOSCROLL_MIN = 3;

/** Map a `.page` element to its 0-based index (data-page), or null. */
function pageIndexOf(el: Element | null): number | null {
  const pageEl = el?.closest<HTMLElement>(".page");
  if (!pageEl) return null;
  const n = Number(pageEl.dataset.page);
  return Number.isFinite(n) ? n : null;
}

export class DomSelectionAdapter {
  private readonly kernel: Kernel;
  private readonly scope: RootScope;
  private readonly pagesContainer: HTMLElement;
  private readonly viewerEl: HTMLElement;
  private readonly getScale: () => number;
  private readonly getDocumentKind: () => string | null;

  private dragging = false;
  /** Effective tool mode, cached off `tool:mode-changed` (default 'select'). */
  private toolMode: "select" | "hand" = "select";
  /** The page the gesture anchored on (fallback when the pointer leaves it). */
  private activePage = 0;
  private capturedEl: HTMLElement | null = null;
  private capturedPointerId: number | null = null;

  /** rAF-throttle state for pointermove (one-in-flight, keep-latest). */
  private moveRaf = 0;
  private latestMove: { clientX: number; clientY: number } | null = null;
  /** The most recent pointer client coords for the life of the drag (autoscroll
   *  keeps extending from here even when the pointer is held still at an edge). */
  private lastPointer: { clientX: number; clientY: number } | null = null;
  /** Edge-autoscroll rAF handle (0 = idle). */
  private autoscrollRaf = 0;
  /** A real pointermove happened during this gesture. Autoscroll is gated on it
   *  so a plain click that lands near a viewport edge never scrolls (a drag must
   *  actually move into the edge band first). */
  private hasMoved = false;

  private readonly unsubs: Array<() => void> = [];
  private readonly onPointerDown: (ev: PointerEvent) => void;
  private readonly onPointerMove: (ev: PointerEvent) => void;
  private readonly onPointerUp: (ev: PointerEvent) => void;
  private readonly onPointerCancel: (ev: PointerEvent) => void;
  private readonly onDblClick: (ev: MouseEvent) => void;
  private readonly onClick: (ev: MouseEvent) => void;
  private readonly onContextMenu: (ev: MouseEvent) => void;
  private readonly onKey: (ev: KeyboardEvent) => void;

  constructor(opts: DomSelectionAdapterOptions) {
    this.kernel = opts.kernel;
    this.scope = opts.scope;
    this.pagesContainer = opts.pagesContainer;
    this.viewerEl = opts.viewerEl;
    this.getScale = opts.getScale;
    this.getDocumentKind = opts.getDocumentKind ?? (() => null);

    this.onPointerDown = (ev) => this.handleDown(ev);
    this.onPointerMove = (ev) => this.handleMove(ev);
    this.onPointerUp = (ev) => this.handleUp(ev);
    this.onPointerCancel = () => this.endGesture();
    this.onDblClick = (ev) => this.handleDblClick(ev);
    this.onClick = (ev) => this.handleClick(ev);
    this.onContextMenu = (ev) => this.handleContextMenu(ev);
    this.onKey = (ev) => this.handleKey(ev);

    this.pagesContainer.addEventListener("pointerdown", this.onPointerDown);
    this.pagesContainer.addEventListener("pointermove", this.onPointerMove);
    this.pagesContainer.addEventListener("pointerup", this.onPointerUp);
    // A gesture interruption (OS/touch takeover, some context-menu paths)
    // releases capture and fires NO pointerup — tear down here too, else
    // `dragging` sticks true and every later move is a phantom drag.
    this.pagesContainer.addEventListener("pointercancel", this.onPointerCancel);
    this.pagesContainer.addEventListener("lostpointercapture", this.onPointerCancel);
    // dbl/triple-click WORD/PARAGRAPH + right-click menu.
    this.pagesContainer.addEventListener("dblclick", this.onDblClick);
    this.pagesContainer.addEventListener("click", this.onClick);
    this.pagesContainer.addEventListener("contextmenu", this.onContextMenu);
    // Root-scoped (the realm's own window): an iframe binds ITS window, so a
    // second instance never cross-fires. Escape is never preventDefault-ed (see
    // handleKey), so it does not swallow the search box's Escape.
    this.scope.win.addEventListener("keydown", this.onKey);

    // REPAINT CONTRACT: drive the engine selection paint on every model edge.
    this.unsubs.push(
      this.kernel.bus.subscribe("selection:changed", () => {
        void this.kernel.exec("viewport.invalidateVisible");
      }),
      // Mode signal: in hand mode pointerdown no-ops so the pan owns the
      // pointer. "latest" replay catches the current mode on late attach.
      this.kernel.bus.subscribe("tool:mode-changed", (e) => {
        this.toolMode = (e as { mode: "select" | "hand" }).mode;
      }),
    );
  }

  /** Client coords → content px within `pageEl` (divide by the zoom scale). */
  private logical(ev: { clientX: number; clientY: number }, pageEl: HTMLElement): { x: number; y: number } {
    const r = pageEl.getBoundingClientRect();
    const scale = this.getScale();
    return { x: (ev.clientX - r.left) / scale, y: (ev.clientY - r.top) / scale };
  }

  /** Resolve an event to (page index, content x/y), or null if not over a page. */
  private resolve(
    ev: { clientX: number; clientY: number; target: EventTarget | null },
  ): { idx: number; x: number; y: number } | null {
    const target = ev.target as Element | null;
    const pageEl = target?.closest<HTMLElement>(".page") ?? null;
    const idx = pageIndexOf(target);
    if (!pageEl || idx === null) return null;
    const { x, y } = this.logical(ev, pageEl);
    return { idx, x, y };
  }

  private handleDown(ev: PointerEvent): void {
    if (ev.button !== 0) return; // primary (left) only
    if (this.toolMode !== "select") return; // hand mode: pan owns the pointer — no capture/begin
    const target = ev.target as Element;
    const pageEl = target.closest<HTMLElement>(".page");
    const idx = pageIndexOf(target);
    if (!pageEl || idx === null) return;

    const { x, y } = this.logical(ev, pageEl);
    this.activePage = idx;
    // Enable drag SYNCHRONOUSLY (do NOT gate on the awaited begin): a slow begin
    // RPC in worker mode otherwise let pointerup/moves outrun it, so `dragging`
    // never flipped and the drag silently collapsed to no paint. Any drag that
    // fires before begin's anchor lands is a harmless no-op — the plugin's
    // `selection.drag` returns early until the store has a `dragging` text span
    // (which `selection.begin` sets), and FIFO hitTest RPCs keep begin first.
    this.dragging = true;
    this.hasMoved = false;
    this.lastPointer = { clientX: ev.clientX, clientY: ev.clientY };
    // Capture so a drag that leaves the page still delivers move/up here.
    try {
      pageEl.setPointerCapture(ev.pointerId);
      this.capturedEl = pageEl;
      this.capturedPointerId = ev.pointerId;
    } catch {
      // Pointer capture unavailable (rare) — proceed without it.
    }

    // shift+pointerdown EXTENDS (keep the anchor, move the focus) instead of
    // anchoring a new span; a plain pointerdown begins. Both leave `dragging` true
    // so a subsequent move keeps extending via `selection.drag`. One command
    // family for pages AND slides — the plugin/engine discriminate.
    if (ev.shiftKey) {
      void this.kernel.exec("selection.extendTo", { loc: { surface: idx }, x, y });
    } else {
      void this.kernel.exec("selection.begin", { loc: { surface: idx }, x, y });
    }
    this.startAutoscroll();
  }

  private handleMove(ev: PointerEvent): void {
    if (!this.dragging) return;
    this.hasMoved = true;
    this.latestMove = { clientX: ev.clientX, clientY: ev.clientY };
    this.lastPointer = { clientX: ev.clientX, clientY: ev.clientY };
    if (this.moveRaf) return; // one-in-flight; the rAF picks up the latest sample
    this.moveRaf = requestAnimationFrame(() => {
      this.moveRaf = 0;
      const m = this.latestMove;
      this.latestMove = null;
      if (!m || !this.dragging) return;
      this.dispatchDrag(m);
    });
  }

  /** Double-click → select the WORD under the pointer (the engine's hitTest range). On a
   *  slide the PLUGIN no-ops (PML word/para deferral) — sent unconditionally. */
  private handleDblClick(ev: MouseEvent): void {
    if (this.toolMode !== "select") return;
    const r = this.resolve(ev);
    if (!r) return;
    void this.kernel.exec("selection.selectWord", { loc: { surface: r.idx }, x: r.x, y: r.y });
  }

  /** Triple(+)-click → select the PARAGRAPH (the engine's hitTest range). A single/double
   *  click's `detail` is 1/2 and no-ops here (dblclick handles words). */
  private handleClick(ev: MouseEvent): void {
    if (this.toolMode !== "select") return;
    if (ev.detail < 3) return;
    const r = this.resolve(ev);
    if (!r) return;
    void this.kernel.exec("selection.selectParagraph", { loc: { surface: r.idx }, x: r.x, y: r.y });
  }

  /** Right-click → suppress the native menu and ask the plugin to build ours
   *  (the menu re-anchor lives in the plugin). Inert in hand mode (native menu shows).
   *  Deferred for PML (the custom slide menu is a declared v1 deferral; the
   *  native menu must show, and preventDefault is a SYNC decision — the one
   *  place the adapter still reads the document kind). */
  private handleContextMenu(ev: MouseEvent): void {
    if (this.toolMode !== "select" || this.getDocumentKind() === "ppt") return;
    const r = this.resolve(ev);
    if (!r) return;
    ev.preventDefault();
    // clientX/clientY are OPAQUE host coords passed straight through; the
    // plugin echoes them into selection:menu and never computes with them.
    void this.kernel.exec("selection.menuAt", {
      loc: { surface: r.idx },
      x: r.x,
      y: r.y,
      clientX: ev.clientX,
      clientY: ev.clientY,
    });
  }

  /** Start the edge-autoscroll rAF loop for the life of the current drag. */
  private startAutoscroll(): void {
    if (this.autoscrollRaf) return;
    const tick = () => {
      if (!this.dragging) {
        this.autoscrollRaf = 0;
        return;
      }
      this.autoscrollStep();
      this.autoscrollRaf = requestAnimationFrame(tick);
    };
    this.autoscrollRaf = requestAnimationFrame(tick);
  }

  /** One autoscroll frame: if the pointer is near a scroll-root edge, scroll
   *  toward it and re-extend the selection to the current pointer. */
  private autoscrollStep(): void {
    if (!this.hasMoved) return; // a plain click near an edge must not scroll
    const m = this.lastPointer;
    if (!m) return;
    const rect = this.viewerEl.getBoundingClientRect();
    const speed = (overshoot: number): number =>
      Math.min(AUTOSCROLL_MAX, Math.max(AUTOSCROLL_MIN, overshoot * 0.5));
    let dx = 0;
    let dy = 0;
    if (m.clientY < rect.top + AUTOSCROLL_BAND) dy = -speed(rect.top + AUTOSCROLL_BAND - m.clientY);
    else if (m.clientY > rect.bottom - AUTOSCROLL_BAND) dy = speed(m.clientY - (rect.bottom - AUTOSCROLL_BAND));
    if (m.clientX < rect.left + AUTOSCROLL_BAND) dx = -speed(rect.left + AUTOSCROLL_BAND - m.clientX);
    else if (m.clientX > rect.right - AUTOSCROLL_BAND) dx = speed(m.clientX - (rect.right - AUTOSCROLL_BAND));
    if (dx === 0 && dy === 0) return;
    // behavior:"instant" — .ps-viewer has scroll-behavior:smooth (snippet.css);
    // the same override the pan/zoom camera uses.
    this.viewerEl.scrollBy({ left: dx, top: dy, behavior: "instant" as ScrollBehavior });
    // Keep extending to the (edge-held) pointer as the content scrolls under it.
    this.dispatchDrag(m);
  }

  /** Resolve the page under the pointer (cross-page drag), then drag there. */
  private dispatchDrag(m: { clientX: number; clientY: number }): void {
    // Prefer the .page under the pointer (cross-page drag); fall back to the page
    // the gesture anchored on when the pointer is off any page (e.g. margins).
    const under = this.scope.elementFromPoint(m.clientX, m.clientY);
    const overIdx = pageIndexOf(under);
    const idx = overIdx ?? this.activePage;
    const pageEl = this.pagesContainer.querySelector<HTMLElement>(`.page[data-page="${idx}"]`);
    if (!pageEl) return;
    const { x, y } = this.logical(m, pageEl);
    // ONE drag command. For a slide drag the plugin resolves on the
    // ANCHOR slide/target (the drag clamps to it) — `surface: idx` is the page under the
    // pointer, ignored for the clamp resolve.
    void this.kernel.exec("selection.drag", { loc: { surface: idx }, x, y });
  }

  private handleUp(_ev: PointerEvent): void {
    this.endGesture();
  }

  /**
   * Tear down the active gesture: release capture, stop the drag, end the
   * selection. Shared by pointerup AND the interruption paths (pointercancel /
   * lostpointercapture). Idempotent — guards on there being a live gesture, so a
   * real pointerup followed by our own releasePointerCapture (which re-fires
   * lostpointercapture), or a cancel with no active drag, is a safe no-op.
   */
  private endGesture(): void {
    if (!this.dragging && this.capturedPointerId === null) return;
    this.dragging = false;
    if (this.moveRaf) {
      cancelAnimationFrame(this.moveRaf);
      this.moveRaf = 0;
    }
    if (this.autoscrollRaf) {
      cancelAnimationFrame(this.autoscrollRaf);
      this.autoscrollRaf = 0;
    }
    this.latestMove = null;
    this.lastPointer = null;
    if (this.capturedEl && this.capturedPointerId !== null) {
      try {
        this.capturedEl.releasePointerCapture(this.capturedPointerId);
      } catch {
        // already released
      }
    }
    this.capturedEl = null;
    this.capturedPointerId = null;
    void this.kernel.exec("selection.end");
  }

  private handleKey(ev: KeyboardEvent): void {
    // Clear on Escape. Do NOT preventDefault — the snippet's search box also
    // listens for Escape on window (mount.ts onKey); swallowing it would break
    // closing the find bar.
    if (ev.key === "Escape") {
      void this.kernel.exec("selection.clear");
    }
  }

  destroy(): void {
    this.pagesContainer.removeEventListener("pointerdown", this.onPointerDown);
    this.pagesContainer.removeEventListener("pointermove", this.onPointerMove);
    this.pagesContainer.removeEventListener("pointerup", this.onPointerUp);
    this.pagesContainer.removeEventListener("pointercancel", this.onPointerCancel);
    this.pagesContainer.removeEventListener("lostpointercapture", this.onPointerCancel);
    this.pagesContainer.removeEventListener("dblclick", this.onDblClick);
    this.pagesContainer.removeEventListener("click", this.onClick);
    this.pagesContainer.removeEventListener("contextmenu", this.onContextMenu);
    this.scope.win.removeEventListener("keydown", this.onKey);
    if (this.moveRaf) cancelAnimationFrame(this.moveRaf);
    if (this.autoscrollRaf) cancelAnimationFrame(this.autoscrollRaf);
    for (const u of this.unsubs.splice(0)) u();
  }
}
