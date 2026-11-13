/**
 * DomToolAdapter — the snippet's DOM adapter for the TOOL MODE. It owns the concrete DOM for the hand tool: the toolbar toggle
 * click, the Space-hold transient-hand keys, the hand-mode drag→viewport-scroll
 * PAN, and the grab/grabbing cursor on the scroll root. The PLUGIN (plugin-tool)
 * owns all model/decision; this adapter only reflects it into the DOM.
 *
 * A SEPARATE adapter from DomSelectionAdapter (not folded in) because pan owns the
 * scroll ROOT `.ps-viewer` (so a drag over the page MARGINS pans too), while
 * selection owns the `.ps-pages` container. Both gate on the effective tool mode
 * — pan acts only in hand mode, selection no-ops its pointerdown in hand mode —
 * so the two never contend for the same pointer.
 *
 * PAN mechanism: absolute scroll from the pointerdown anchor
 * (`scrollTo({left,top})`), NOT incremental scrollBy, so no per-move drift
 * accumulates. `behavior:"instant"` is LOAD-BEARING: `.ps-viewer` has
 * `scroll-behavior:smooth` (snippet.css), and the instant override is the same one
 * the zoom camera uses (dom-viewport-adapter). Moves are rAF-throttled (latest-wins).
 *
 * SPACE-HOLD: keydown → tool.setTransientHand(true), keyup → false, but IGNORED
 * when focus is in an input/textarea/select/button/link/contenteditable (so Space
 * types a space in the find box and doesn't hijack a focused control). blur +
 * visibilitychange clear a stuck transient-hand (a Space held across a tab switch
 * never gets its keyup).
 *
 * CURSOR: the plugin decides grab (plugin-cursor rule 0); this adapter writes it
 * on the scroll root for the margins and swaps to the transient `grabbing` during
 * an active pan (the plugin has no in-flight-drag notion). The `.is-panning` class
 * on the root forces `grabbing` over the per-page inline cursor while dragging.
 */
import type { Kernel } from "@parasma/headless";
import type { RootScope } from "../root-scope";

export interface DomToolAdapterOptions {
  kernel: Kernel;
  /** The DOM seam (realm window/document, shadow-correct activeElement). */
  scope: RootScope;
  /** The pages scroll ROOT (.ps-viewer) — hand-drag pans its scroll in pages view. */
  viewerEl: HTMLElement;
  /** The grid scroll ROOT (.ps-gridview) — the active pan target in grid view. */
  gridviewEl: HTMLElement;
  /** The mount root (.parasma-snippet) — toggles the `is-panning` grabbing class. */
  root: HTMLElement;
  /** The two segments of the toolbar tool toggle (pointer | hand). The active one
   *  reflects the effective mode via aria-pressed; each SETS its mode on click. */
  selectButton: HTMLButtonElement;
  handButton: HTMLButtonElement;
}

export class DomToolAdapter {
  private readonly kernel: Kernel;
  private readonly scope: RootScope;
  private readonly viewerEl: HTMLElement;
  private readonly gridviewEl: HTMLElement;
  private readonly root: HTMLElement;
  private readonly selectButton: HTMLButtonElement;
  private readonly handButton: HTMLButtonElement;

  private mode: "select" | "hand" = "select";
  /** Grid vs pages view — decides which scroll root a hand-drag pans. */
  private gridMode = false;
  /** The scroll element captured for the CURRENT pan gesture (grid or pages). */
  private panEl: HTMLElement | null = null;
  private panning = false;
  private startX = 0; private startY = 0;
  private startScrollLeft = 0; private startScrollTop = 0;
  private capturedPointerId: number | null = null;
  private panRaf = 0;
  private latest: { x: number; y: number } | null = null;

  private readonly unsubs: Array<() => void> = [];
  private readonly onDown: (e: PointerEvent) => void;
  private readonly onMove: (e: PointerEvent) => void;
  private readonly onUp: (e: PointerEvent) => void;
  private readonly onCancel: () => void;
  private readonly onKeyDown: (e: KeyboardEvent) => void;
  private readonly onKeyUp: (e: KeyboardEvent) => void;
  private readonly onBlur: () => void;
  private readonly onVisibility: () => void;
  private readonly onSelectClick: () => void;
  private readonly onHandClick: () => void;

  constructor(opts: DomToolAdapterOptions) {
    this.kernel = opts.kernel;
    this.scope = opts.scope;
    this.viewerEl = opts.viewerEl;
    this.gridviewEl = opts.gridviewEl;
    this.root = opts.root;
    this.selectButton = opts.selectButton;
    this.handButton = opts.handButton;

    this.onDown = (e) => this.handleDown(e);
    this.onMove = (e) => this.handleMove(e);
    this.onUp = () => this.endPan();
    this.onCancel = () => this.endPan();
    this.onKeyDown = (e) => this.handleKeyDown(e);
    this.onKeyUp = (e) => this.handleKeyUp(e);
    this.onBlur = () => void this.kernel.exec("tool.setTransientHand", false);
    this.onVisibility = () => {
      if (this.scope.doc.visibilityState === "hidden") void this.kernel.exec("tool.setTransientHand", false);
    };
    // Each segment SETS its mode directly (segmented control, not a flip).
    this.onSelectClick = () => void this.kernel.exec("tool.setMode", "select");
    this.onHandClick = () => void this.kernel.exec("tool.setMode", "hand");

    this.unsubs.push(
      this.kernel.bus.subscribe("tool:mode-changed", (e) =>
        this.applyMode((e as { mode: "select" | "hand" }).mode),
      ),
      // Which scroll root is live — grid view pans .ps-gridview, pages .ps-viewer.
      this.kernel.bus.subscribe("grid:mode-changed", (e) => {
        this.gridMode = (e as { mode: "grid" | "pages" }).mode === "grid";
        this.applyMode(this.mode); // refresh the cursor onto the active root
      }),
    );

    // Listen on BOTH scroll roots (only one is visible at a time); the pan targets
    // whichever is active (activeScroll()).
    for (const el of [this.viewerEl, this.gridviewEl]) {
      el.addEventListener("pointerdown", this.onDown);
      el.addEventListener("pointermove", this.onMove);
      el.addEventListener("pointerup", this.onUp);
      el.addEventListener("pointercancel", this.onCancel);
      el.addEventListener("lostpointercapture", this.onCancel);
    }
    this.scope.win.addEventListener("keydown", this.onKeyDown);
    this.scope.win.addEventListener("keyup", this.onKeyUp);
    this.scope.win.addEventListener("blur", this.onBlur);
    this.scope.doc.addEventListener("visibilitychange", this.onVisibility);
    this.selectButton.addEventListener("click", this.onSelectClick);
    this.handButton.addEventListener("click", this.onHandClick);

    this.applyMode("select"); // initial segment/cursor state
  }

  /** DOM write of the mode decision: cursor + toggle aria; end a pan if leaving hand. */
  private applyMode(mode: "select" | "hand"): void {
    this.mode = mode;
    this.selectButton.setAttribute("aria-pressed", mode === "select" ? "true" : "false");
    this.handButton.setAttribute("aria-pressed", mode === "hand" ? "true" : "false");
    if (mode === "hand") {
      // Grab cursor on the active scroll root; clear the inactive one so a stale
      // grab can't linger on a hidden container after a view switch.
      const active = this.activeScroll();
      const inactive = active === this.viewerEl ? this.gridviewEl : this.viewerEl;
      active.style.cursor = this.panning ? "grabbing" : "grab";
      inactive.style.cursor = "";
    } else {
      if (this.panning) this.endPan();
      this.viewerEl.style.cursor = "";
      this.gridviewEl.style.cursor = "";
    }
  }

  /** Space must not hijack typing in the find box or activate a focused control. */
  private isInteractiveFocus(): boolean {
    const a = this.scope.activeElement() as HTMLElement | null;
    if (!a) return false;
    // The hidden clipboard copy sink is contenteditable but is NOT
    // interactive chrome: a focused sink must still allow Space-hold pan.
    if (a.hasAttribute("data-clipboard-sink")) return false;
    if (a.isContentEditable) return true;
    return ["INPUT", "TEXTAREA", "SELECT", "BUTTON", "A"].includes(a.tagName);
  }

  private handleKeyDown(e: KeyboardEvent): void {
    if (e.key !== " " && e.code !== "Space") return;
    if (e.repeat) return;
    if (this.isInteractiveFocus()) return;
    e.preventDefault(); // suppress the default space-scroll on a read-only viewer
    void this.kernel.exec("tool.setTransientHand", true);
  }
  private handleKeyUp(e: KeyboardEvent): void {
    if (e.key !== " " && e.code !== "Space") return;
    void this.kernel.exec("tool.setTransientHand", false);
  }

  /** The scroll root a hand-drag pans right now: the grid in grid view, else pages. */
  private activeScroll(): HTMLElement {
    return this.gridMode ? this.gridviewEl : this.viewerEl;
  }

  private handleDown(e: PointerEvent): void {
    if (e.button !== 0 || this.mode !== "hand") return;
    const el = this.activeScroll();
    this.panEl = el;
    this.panning = true;
    this.startX = e.clientX; this.startY = e.clientY;
    this.startScrollLeft = el.scrollLeft;
    this.startScrollTop = el.scrollTop;
    try {
      el.setPointerCapture(e.pointerId);
      this.capturedPointerId = e.pointerId;
    } catch { /* capture unavailable — proceed */ }
    this.root.classList.add("is-panning");
    el.style.cursor = "grabbing";
    e.preventDefault();
  }

  private handleMove(e: PointerEvent): void {
    if (!this.panning || !this.panEl) return;
    this.latest = { x: e.clientX, y: e.clientY };
    if (this.panRaf) return;
    this.panRaf = requestAnimationFrame(() => {
      this.panRaf = 0;
      const m = this.latest; this.latest = null;
      if (!m || !this.panning || !this.panEl) return;
      // Absolute from the pointerdown anchor = no incremental drift. Grab-pan:
      // content follows the hand. behavior "instant" is LOAD-BEARING — both scroll
      // roots have scroll-behavior:smooth (snippet.css); the grid's own scroll
      // listener repaints the windowed grid, so this reads as a natural scroll.
      this.panEl.scrollTo({
        left: this.startScrollLeft - (m.x - this.startX),
        top: this.startScrollTop - (m.y - this.startY),
        behavior: "instant",
      });
    });
  }

  private endPan(): void {
    if (!this.panning && this.capturedPointerId === null) return;
    this.panning = false;
    if (this.panRaf) { cancelAnimationFrame(this.panRaf); this.panRaf = 0; }
    this.latest = null;
    const el = this.panEl ?? this.activeScroll();
    if (this.capturedPointerId !== null) {
      try { el.releasePointerCapture(this.capturedPointerId); } catch { /* released */ }
      this.capturedPointerId = null;
    }
    this.root.classList.remove("is-panning");
    el.style.cursor = this.mode === "hand" ? "grab" : "";
    this.panEl = null;
  }

  destroy(): void {
    for (const el of [this.viewerEl, this.gridviewEl]) {
      el.removeEventListener("pointerdown", this.onDown);
      el.removeEventListener("pointermove", this.onMove);
      el.removeEventListener("pointerup", this.onUp);
      el.removeEventListener("pointercancel", this.onCancel);
      el.removeEventListener("lostpointercapture", this.onCancel);
    }
    this.scope.win.removeEventListener("keydown", this.onKeyDown);
    this.scope.win.removeEventListener("keyup", this.onKeyUp);
    this.scope.win.removeEventListener("blur", this.onBlur);
    this.scope.doc.removeEventListener("visibilitychange", this.onVisibility);
    this.selectButton.removeEventListener("click", this.onSelectClick);
    this.handButton.removeEventListener("click", this.onHandClick);
    for (const u of this.unsubs.splice(0)) u();
    this.endPan();
  }
}
