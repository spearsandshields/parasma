/**
 * Tier-B slots — BOUNDED chrome injection points for the default (mountViewer)
 * tier. Exactly two, demand-driven (deliberately: no
 * declarative toolbar-config god-object; deep restructuring belongs to the
 * headless tier where the consumer owns ALL chrome):
 *
 *   - `toolbarTrailing`: extra toolbar content, mounted as its OWN `.ps-group`
 *     right before the status group (the status token stays terminal). Invisible
 *     in chromeless mode (`controls:false` hides the whole toolbar).
 *   - `viewportOverlay`: a full-stage pass-through layer (`pointer-events:none`;
 *     direct children re-enable) for floating chrome — the canonical use is a
 *     bottom-floating page navigator built on the public kernel-truth API
 *     (`on("pageChange")` + `goToPage` + `state.currentPage`).
 *
 * Each slot accepts a plain HTMLElement or a FACTORY called with the LIVE
 * ViewerHandle, so consumer chrome closes over the handle in one phase. A
 * throwing factory (or a non-element return) is CONTAINED — one console.warn,
 * that slot skipped — mounting never fails because of a bad slot. Slot nodes are
 * consumer-owned: they detach with the viewer root on destroy(); consumers with
 * their own listeners on foreign targets clean those up around destroy().
 *
 * HEADLESS TIER: deliberately NO slots on HeadlessViewerConfig — BYO-chrome
 * already owns every element around the stage.
 */
import { el } from "./dom";
import type { MountOptions, ViewerHandle } from "./types";

/**
 * @advanced Also a Tier-C seam (`@parasma/snippet/chrome`). Wired by mount.ts
 * as: `{ slots: options.slots, handle, toolbarRoot: toolbar.root, stage }`.
 */
export interface InstallSlotsDeps {
  slots: MountOptions["slots"];
  /** The LIVE handle — factories may close over goToPage/on()/state. */
  handle: ViewerHandle;
  /** The default toolbar root (`.ps-toolbar`). */
  toolbarRoot: HTMLElement;
  /** The stage (`.ps-stage`) the overlay layer spans. */
  stage: HTMLElement;
}

export interface InstalledSlots {
  /** The `.ps-group.ps-slot` wrapper, when toolbarTrailing installed. */
  toolbarSlot?: HTMLElement;
  /** The `.ps-slot-overlay` layer, when viewportOverlay installed. */
  overlaySlot?: HTMLElement;
}

/** Resolve an element-or-factory source; null = skipped (warned). */
function resolveSlot(
  name: string,
  src: HTMLElement | ((handle: ViewerHandle) => HTMLElement),
  handle: ViewerHandle,
): HTMLElement | null {
  try {
    const node = typeof src === "function" ? src(handle) : src;
    // Duck-typed (realm-safe: a cross-realm element fails `instanceof
    // HTMLElement` here): anything append-able with a tag works.
    if (!node || typeof (node as { tagName?: unknown }).tagName !== "string") {
      console.warn(`[parasma-snippet] slots.${name} did not yield an element — slot skipped`);
      return null;
    }
    return node;
  } catch (err) {
    console.warn(`[parasma-snippet] slots.${name} factory threw — slot skipped`, err);
    return null;
  }
}

export function installSlots(deps: InstallSlotsDeps): InstalledSlots {
  const out: InstalledSlots = {};
  const { slots, handle, toolbarRoot, stage } = deps;
  if (!slots) return out;

  if (slots.toolbarTrailing) {
    const node = resolveSlot("toolbarTrailing", slots.toolbarTrailing, handle);
    if (node) {
      const wrap = el("div", { class: "ps-group ps-slot ps-slot--toolbar" }, [node]);
      // Before the status group so the status token stays the terminal element;
      // the `.ps-status` lookup is our own toolbar DOM (same package, stable).
      const statusGroup = toolbarRoot.querySelector(".ps-status")?.parentElement ?? null;
      toolbarRoot.insertBefore(wrap, statusGroup);
      out.toolbarSlot = wrap;
    }
  }

  if (slots.viewportOverlay) {
    const node = resolveSlot("viewportOverlay", slots.viewportOverlay, handle);
    if (node) {
      const overlay = el("div", { class: "ps-slot-overlay" }, [node]);
      stage.appendChild(overlay);
      out.overlaySlot = overlay;
    }
  }

  return out;
}
