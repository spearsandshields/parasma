/**
 * Paint-intent surface.
 *
 * The kernel emits paint intent; the consumer's DOM adapter owns
 * canvases + IntersectionObserver. This is the seam for that split.
 * `plugin-viewport` DRIVES `requestPaint` with the page variants and
 * `plugin-grid` with `'grid-viewport'`; the DOM adapter subscribes via
 * `onPaintIntent` and coalesces per rAF.
 */

/**
 * A coalescable paint request — a tagged union so a new producer adds a variant
 * without a breaking change. plugin-viewport emits the `'page'` and
 * `'page-invalidate'` variants (the per-page windowing concern); plugin-grid
 * emits `'grid-viewport'`. Each consumer adapter drains only the tags it owns.
 */
export type PaintIntent =
  | { tag: "page"; pageIndex: number }
  // INVALIDATE an already-painted page: force a re-render in place (DPR reset +
  // viewer.renderPage onto the EXISTING canvas), bypassing the adapter's
  // "already has a canvas → skip" guard. This is the kernel-native, ONE
  // invalidate-visible channel: both font-variant arrival
  // (plugin-fonts) and image arrival route through it so a face/image that lands
  // AFTER the first paint re-renders the currently-visible pages. Unlike `page`
  // (which only paints unpainted/visible placeholders), `page-invalidate` repaints
  // an existing canvas.
  | { tag: "page-invalidate"; pageIndex: number }
  // GRID WINDOW repaint (plugin-grid): paint the visible window of the
  // continuous spreadsheet grid (the windowed virtual scroller), a
  // SEPARATE canvas from the per-page `.page` canvases. The DOM grid adapter
  // (DomGridAdapter) drains this tag, pins #grid-canvas via a translate(scrollX,
  // scrollY) transform, and calls viewer.paintGridViewport(sheet, canvas, …) for
  // O(visible-rows) painting. The DomViewportAdapter ignores it (its drain only
  // handles 'page'/'page-invalidate'); the grid adapter ignores 'page'.
  | {
      tag: "grid-viewport";
      sheet: number;
      scrollX: number;
      scrollY: number;
      viewW: number;
      viewH: number;
    };

export type PaintIntentHandler = (intent: PaintIntent) => void;

export interface PaintSurface {
  /** Emit a paint intent to every subscribed adapter. */
  requestPaint(intent: PaintIntent): void;
  /** Subscribe to paint intents (the DOM adapter). Returns an unsubscribe. */
  onPaintIntent(handler: PaintIntentHandler): () => void;
}

export function createPaintSurface(): PaintSurface {
  const handlers = new Set<PaintIntentHandler>();
  return {
    requestPaint(intent) {
      for (const h of handlers) {
        try {
          h(intent);
        } catch (e) {
          console.error("[parasma:headless] paint-intent handler threw:", e);
        }
      }
    },
    onPaintIntent(handler) {
      handlers.add(handler);
      return () => {
        handlers.delete(handler);
      };
    },
  };
}
