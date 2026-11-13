/**
 * DomContextMenuAdapter — the snippet's host DOM for the canvas-native context
 * menu. The PLUGIN owns the model + decision: it
 * emits `selection:menu` with the item list and the OPAQUE host coordinates
 * (`clientX`/`clientY`) after applying its re-anchor. This adapter is the ONLY
 * DOM-touching piece: it renders a floating menu at those coordinates, wires each
 * item to its host action (Copy → the clipboard adapter's `copyNow()` OS write,
 * inside the click's user-activation window), and dismisses on Esc / outside-click
 * / scroll / another right-click.
 *
 * The emitted `clientX`/`clientY` are opaque host coords the plugin only
 * echoes. This adapter MAY clamp them to the viewport so the menu stays on-screen,
 * but that clamp is DISPLAY-ONLY and is never fed back into the plugin.
 *
 * DISMISS MODEL — an `items:[]` emit (plugin doc-switch / `selection.closeMenu`)
 * closes an open menu. Local dismissals (Esc, outside-click, scroll, re-open)
 * remove the DOM element directly AND call `selection.closeMenu` so the plugin
 * model and the DOM never drift.
 */
import type { ContextMenuItem, Kernel } from "@parasma/headless";
import type { RootScope } from "../root-scope";
import { el } from "../dom";

interface MenuModel {
  clientX: number;
  clientY: number;
  items: ContextMenuItem[];
}

/**
 * Invoke a menu item's action: a consumer `onSelect` wins (it runs INSIDE the
 * menu click's user-activation window — the same guarantee Copy's clipboard
 * write relies on, so a consumer replacing `{id:"copy"}` with an `onSelect`
 * gets their handler); else built-in id dispatch ("copy" → the host copy
 * action). A throwing `onSelect` is contained (warn, no rethrow) so the menu
 * still dismisses.
 */
export function invokeMenuItem(item: ContextMenuItem, hostActions: { copy: () => void }): void {
  if (item.onSelect) {
    try {
      item.onSelect();
    } catch (e) {
      console.warn("[parasma-snippet] context-menu onSelect threw:", e);
    }
    return;
  }
  if (item.id === "copy") hostActions.copy();
}

export interface DomContextMenuAdapterOptions {
  kernel: Kernel;
  /** The DOM seam (realm window/document for the dismiss listeners + clamp). */
  scope: RootScope;
  /** The mount root the menu element is appended to. */
  root: HTMLElement;
  /** The scroll root — a scroll on it dismisses an open menu. */
  scrollRoot: HTMLElement;
  /** Host action for the Copy item: perform the actual OS clipboard write. */
  onCopy: () => void;
}

export class DomContextMenuAdapter {
  private readonly kernel: Kernel;
  private readonly scope: RootScope;
  private readonly root: HTMLElement;
  private readonly scrollRoot: HTMLElement;
  private readonly onCopy: () => void;

  private menuEl: HTMLElement | null = null;

  private readonly unsubs: Array<() => void> = [];
  private readonly onDocPointerDown: (ev: PointerEvent) => void;
  private readonly onScroll: () => void;
  private readonly onKeyDown: (ev: KeyboardEvent) => void;

  constructor(opts: DomContextMenuAdapterOptions) {
    this.kernel = opts.kernel;
    this.scope = opts.scope;
    this.root = opts.root;
    this.scrollRoot = opts.scrollRoot;
    this.onCopy = opts.onCopy;

    this.onDocPointerDown = (ev) => this.handleOutside(ev);
    this.onScroll = () => this.dismiss();
    this.onKeyDown = (ev) => {
      if (ev.key === "Escape") this.dismiss();
    };

    this.unsubs.push(
      this.kernel.bus.subscribe("selection:menu", (m) => this.render(m as MenuModel)),
    );
  }

  /** Render (or dismiss on empty) the emitted menu model. */
  private render(model: MenuModel): void {
    if (!model.items.length) {
      this.close();
      return;
    }
    // A re-open (another right-click) replaces any existing menu.
    this.close();

    const menu = el("div", { class: "ps-context-menu", role: "menu" });
    for (const item of model.items) {
      const btn = el(
        "button",
        {
          type: "button",
          class: "ps-context-menu__item",
          role: "menuitem",
          ...(item.enabled ? {} : { disabled: "", "aria-disabled": "true" }),
        },
        [item.label],
      );
      if (item.enabled) {
        btn.addEventListener("click", () => {
          // Consumer onSelect / built-in id dispatch, INSIDE the click's
          // user-activation window (clipboard writes depend on it).
          invokeMenuItem(item, { copy: this.onCopy });
          this.dismiss();
        });
      }
      menu.append(btn);
    }
    this.root.append(menu);
    this.menuEl = menu;

    // position:fixed BEFORE measuring: as a static flex child of .parasma-snippet
    // (display:flex, column), the menu would STRETCH to the container's full width,
    // so offsetWidth reads ~full-width and collapses the horizontal clamp to its 4px
    // floor. Fixed positioning shrink-wraps it to its real size first — THEN measure.
    menu.style.position = "fixed";
    menu.style.left = "0";
    menu.style.top = "0";

    // Position at the opaque host coords, CLAMPED to the viewport for display only
    // (never fed back to the plugin).
    const w = menu.offsetWidth;
    const h = menu.offsetHeight;
    // DELIBERATE (root-scoping): clamp against the REALM window's
    // inner size, NOT viewportRect() — the menu is positioned at clientX/clientY
    // (viewport coords), so the clamp boundary must be the viewport. In standalone
    // scope.win.inner* === window.inner* (byte-identical); in an iframe it is the
    // iframe's own viewport; viewportRect() would move the boundary and shift the
    // menu near the viewer edge.
    const left = Math.max(4, Math.min(model.clientX, this.scope.win.innerWidth - w - 4));
    const top = Math.max(4, Math.min(model.clientY, this.scope.win.innerHeight - h - 4));
    menu.style.left = `${left}px`;
    menu.style.top = `${top}px`;

    // Dismiss wiring (added only while open). Outside-pointerdown uses capture to
    // catch clicks anywhere; a click ON a menu item is excluded by containment
    // (handleOutside), so the item's own click handler still runs.
    // Outside-dismiss binds the HOST document (not the shadow root) so a click
    // ANYWHERE outside the shadow tree still dismisses the menu.
    this.scope.doc.addEventListener("pointerdown", this.onDocPointerDown, true);
    this.scrollRoot.addEventListener("scroll", this.onScroll, { passive: true });
    this.scope.win.addEventListener("scroll", this.onScroll, { passive: true });
    this.scope.win.addEventListener("keydown", this.onKeyDown, true);
  }

  /** Remove the menu element + its dismiss listeners (no plugin round-trip). */
  private close(): void {
    if (!this.menuEl) return;
    this.scope.doc.removeEventListener("pointerdown", this.onDocPointerDown, true);
    this.scrollRoot.removeEventListener("scroll", this.onScroll);
    this.scope.win.removeEventListener("scroll", this.onScroll);
    this.scope.win.removeEventListener("keydown", this.onKeyDown, true);
    this.menuEl.remove();
    this.menuEl = null;
  }

  /** A host-initiated dismiss: close the DOM AND tell the plugin (model parity). */
  private dismiss(): void {
    if (!this.menuEl) return;
    this.close();
    void this.kernel.exec("selection.closeMenu");
  }

  private handleOutside(ev: PointerEvent): void {
    if (!this.menuEl) return;
    if (this.menuEl.contains(ev.target as Node)) return; // a click inside the menu
    this.dismiss();
  }

  destroy(): void {
    this.close();
    for (const u of this.unsubs.splice(0)) u();
  }
}
