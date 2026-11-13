/**
 * page-change-fanout — kernel truth → the public snippet event.
 *
 * Bridges the viewport plugin's 0-based `viewport:page-changed` onto the
 * 1-based public `pageChange` (SnippetEventMap). Wired by headless-viewer as a
 * CORE subscription, so `handle.on("pageChange")` fires with NO toolbar
 * mounted — the BYO floating-nav guarantee. This is one of the four 0↔1
 * conversion sites (with headless-viewer's goToPage −1, the toolbar's
 * display/dispatch edges, and attach-dom's DomBinding.currentPage +1).
 *
 * Doc-switch contract: `viewport.reset` is deliberately SILENT (no emit, replay
 * cleared), so a BYO nav must re-sync from `state.currentPage` on "load".
 */
import type { Kernel } from "@parasma/headless";

/** Subscribe the fan-out; returns the unsubscribe (push onto coreUnsubs). */
export function wirePageChangeFanout(
  kernel: Kernel,
  emit: (ev: "pageChange", n: number) => void,
): () => void {
  return kernel.bus.subscribe("viewport:page-changed", ({ index }) => {
    emit("pageChange", index + 1);
  });
}
