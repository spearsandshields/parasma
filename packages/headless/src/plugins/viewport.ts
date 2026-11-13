/**
 * plugin-viewport — per-page windowing behind the kernel.
 *
 * This plugin OWNS the per-page model (page count + per-page size) and the
 * visible-window state (which page indices are currently visible). It is NOT one
 * of the two XState machines (those are loader + fonts only): it has
 * no async lifecycle with cancellation, so per the rule it is a lightweight
 * `@xstate/store` slice.
 *
 * BOUNDARY: the kernel emits PAINT INTENTS; the consumer's DOM adapter
 * (DomViewportAdapter, in @parasma/snippet) owns the canvases + IntersectionObserver.
 * This plugin emits `{ tag: 'page', pageIndex }` intents — one per page that should
 * be painted / placeholder-added — plus `'page-invalidate'` repaints of visible pages. It NEVER touches the DOM and
 * NEVER imports the engine's render path beyond reading public `viewer.pageSize()`.
 *
 * It is a THIN consumer of the bus: plugin-loader already bridges the Viewer's
 * native `pageReady`/`loadComplete` onto the replayable kernel bus (loader.ts
 * §initialize), so subscribing here is replay-safe even though `pageReady` can
 * fire synchronously inside the first `viewer.load()` — the bus's "accumulate"
 * replay for `pageReady` and "latest" for `loadComplete` catch this plugin up.
 *
 * deps: ["loader"] — two-phase boot guarantees loader.initialize() (the bridge)
 * runs before viewport.initialize().
 */

import type { PageSize } from "@parasma/core";
import type { ViewportViewer } from "./ports";
import type { Plugin } from "../registry";
import type { KernelContext } from "../kernel-context";
import { createSliceStore, type StoreSlice } from "../store";

/**
 * Commands OWNED by this plugin. Declaration-merged into the open
 * {@link CommandMap} (targeting `../commands`, the barrel-re-exported kernel
 * module → program-wide) so `exec(id, …)` is arg/return type-checked and an
 * undeclared id fails to register. Signatures authored from the handler bodies.
 */
declare module "../commands" {
  interface CommandMap {
    "viewport.setVisible": (indices: number[]) => void;
    "viewport.invalidateVisible": () => void;
    "viewport.pageModel": () => PageModel;
    "viewport.reset": () => void;
    /**
     * SENSING input (0-based): the consumer's DOM sensor reports the topmost
     * visible page. Clamped + deduped; a CHANGE emits `viewport:page-changed`.
     */
    "viewport.setCurrentPage": (index: number) => void;
    /**
     * NAVIGATION intent (0-based). Clamps, updates the model (emitting
     * `viewport:page-changed` on change), then ALWAYS emits
     * `viewport:scroll-to-page` — even unchanged, preserving re-align-page-top
     * on goTo(currentPage). If no `.page` placeholder exists yet for the target
     * (early streaming / beyond the built range) the consumer's scroll is a
     * silent no-op, but the model still updates.
     */
    "viewport.goToPage": (index: number) => void;
  }
}

/**
 * Bus events OWNED by this plugin (declaration-merged into the open
 * {@link BusEventMap}; replay policy rows live in bus.ts REPLAY_POLICY).
 */
declare module "../bus" {
  interface BusEventMap {
    /**
     * The current-page MODEL edge (0-based). Sticky ("latest") so a
     * late-mounted consumer (toolbar/BYO nav) catches the current page.
     * Cleared on `viewport.reset` — doc B must not replay doc A's page.
     */
    "viewport:page-changed": { index: number };
    /**
     * Imperative scroll request (0-based) the DOM adapter consumes — the
     * `search:navigate` mirror. Transient ("none"): a late subscriber must not
     * replay a stale navigation.
     */
    "viewport:scroll-to-page": { index: number };
  }
}

/** The viewport plugin's store-slice state: page model + visible window. */
export interface ViewportState extends Record<string, unknown> {
  /** Number of pages laid out so far (max idx+1; final = loadComplete total). */
  pageCount: number;
  /** Per-page CSS size, captured once from viewer.pageSize() on first pageReady. */
  pageSize: PageSize | null;
  /** Sorted, de-duplicated indices currently inside the visible window. */
  visible: number[];
  /** True once the engine signalled layout-complete for this document. */
  loadComplete: boolean;
  /** Topmost-visible page index (0-based), fed by the consumer's DOM sensor. */
  currentPage: number;
}

const initialState: ViewportState = {
  pageCount: 0,
  pageSize: null,
  visible: [],
  loadComplete: false,
  currentPage: 0,
};

/**
 * Clamp a 0-based page index into [0, pageCount-1] (an empty doc clamps to 0).
 * The Number.isFinite guard lives in the command handlers, not here.
 */
const clampPage = (i: number, n: number): number => Math.max(0, Math.min(Math.max(0, n - 1), i | 0));

/**
 * The "viewport" store slice. Transitions mirror core's own page-model math
 * (pageCount = max(pageCount, idx+1) on pageReady; pageCount = total on
 * loadComplete — see parasma-core/src/index.ts).
 */
function makeSlice(): StoreSlice<ViewportState> {
  return {
    state: initialState,
    on: {
      pageReady: (s, payload: { idx: number; pageSize: PageSize | null }) => ({
        ...s,
        pageCount: Math.max(s.pageCount, payload.idx + 1),
        // Capture pageSize once (it is uniform per document for the paged path).
        pageSize: s.pageSize ?? payload.pageSize,
      }),
      loadComplete: (s, total: number) => ({
        ...s,
        pageCount: total,
        loadComplete: true,
        // A doc that completes SHORTER than the sensed page clamps here; the
        // plugin's loadComplete bus sub detects the edge and emits (a pure
        // store transition cannot emit).
        currentPage: Math.min(s.currentPage, Math.max(0, total - 1)),
      }),
      setVisible: (s, indices: number[]) => ({
        ...s,
        visible: [...indices].sort((a, b) => a - b),
      }),
      setCurrentPage: (s, idx: number) => ({ ...s, currentPage: clampPage(idx, s.pageCount) }),
      reset: () => ({ ...initialState, visible: [] }),
    },
  };
}

/** Public query shape returned by the `viewport.pageModel` command. */
export interface PageModel {
  pageCount: number;
  pageSize: PageSize | null;
  loadComplete: boolean;
  /** Topmost-visible page index (0-based) — the kernel-truth current page. */
  currentPage: number;
}

/**
 * The viewport plugin. Construct with the SAME Viewer the consumer renders into
 * (mirrors loaderPlugin(viewer)) so it can read `viewer.pageSize()` without a new
 * engine accessor — the per-page canvas model is preserved exactly; no engine
 * change.
 */
export function viewportPlugin(viewer: ViewportViewer): Plugin {
  const store = createSliceStore(makeSlice());
  const unsubs: Array<() => void> = [];

  const state = (): ViewportState => store.get();

  return {
    id: "viewport",
    deps: ["loader"],

    initialize(ctx: KernelContext) {
      // Consume the loader-bridged page lifecycle off the replayable bus. The
      // bus replays the per-index pageReady log + sticky loadComplete, so this
      // catches up even if loader's synchronous first-page burst already fired.
      unsubs.push(
        ctx.bus.subscribe("pageReady", (idx) => {
          // viewer.pageSize() is a worker-ready async seam; the bus
          // handler can't be async. Capture it once: if we already have a
          // pageSize, stay fully synchronous (the common case — pageSize is
          // uniform per document, fetched on the FIRST pageReady only); otherwise
          // await it inside a void IIFE (resolves immediately in-process).
          const known = state().pageSize;
          if (known) {
            store.dispatch("pageReady", { idx, pageSize: known });
            ctx.paint.requestPaint({ tag: "page", pageIndex: idx });
            return;
          }
          void (async () => {
            const pageSize = state().pageSize ?? (await viewer.pageSize());
            store.dispatch("pageReady", { idx, pageSize });
            // Emit a 'page' intent: the adapter idempotently creates the
            // placeholder (and paints if the page is in the visible window).
            ctx.paint.requestPaint({ tag: "page", pageIndex: idx });
          })();
        }),
        ctx.bus.subscribe("loadComplete", (total) => {
          // MIRROR-DEADLOCK GUARD: if the final total clamps a dangling
          // currentPage (sensed 9 while streaming, complete at 4 → 3), the edge
          // MUST emit — the consumer's mirror updates only from the bus echo,
          // and a silent clamp would leave it stale while the plugin dedups the
          // next setCurrentPage exec (never converging). The sticky replay then
          // holds the clamped index for late subscribers.
          const prev = state().currentPage;
          store.dispatch("loadComplete", total);
          const now = state().currentPage;
          if (now !== prev) ctx.bus.emit("viewport:page-changed", { index: now });
        }),
      );

      // Commands (the plugin surface). The DOM adapter drives these.

      // The adapter's IntersectionObserver reports the current visible indices;
      // we update the window and emit a 'page' intent for each NEWLY-visible
      // index (no intent for indices that were already visible — idempotent).
      unsubs.push(
        ctx.commands.register("viewport.setVisible", (indices: number[]) => {
          const prev = new Set(state().visible);
          store.dispatch("setVisible", indices);
          for (const idx of indices) {
            if (!prev.has(idx)) ctx.paint.requestPaint({ tag: "page", pageIndex: idx });
          }
        }),
      );

      // INVALIDATE-VISIBLE channel (driven by plugin-fonts): re-render every
      // currently-visible painted page in place. Driven when a late asset lands
      // after the first paint — a font face (plugin-fonts 'fonts:variant-arrived')
      // or a decoded image — so the visible canvases pick it up. Emits a
      // 'page-invalidate' intent per visible index; the DOM adapter force-repaints
      // the EXISTING canvas. No-op when nothing is visible.
      unsubs.push(
        ctx.commands.register("viewport.invalidateVisible", () => {
          for (const idx of state().visible) {
            ctx.paint.requestPaint({ tag: "page-invalidate", pageIndex: idx });
          }
        }),
      );

      // plugin-fonts re-renders visible pages as each face lands. We subscribe the
      // bridged 'fonts:variant-arrived' here (rather than coupling plugin-fonts to
      // a viewport command) so the single invalidate-visible channel lives with the
      // page-model owner. deps:['loader'] guarantees the loader bridge exists; the
      // fonts plugin bridges core's event onto this bus.
      unsubs.push(
        ctx.bus.subscribe("fonts:variant-arrived", () => {
          for (const idx of state().visible) {
            ctx.paint.requestPaint({ tag: "page-invalidate", pageIndex: idx });
          }
        }),
        // A late image decode re-renders the visible pages through the SAME
        // invalidate-visible channel (loader bridges viewer's `imageReady` onto
        // the bus). Without this, a slow-decoding image never appeared until the
        // user zoomed or scrolled (the snippet never wired viewer.on directly).
        ctx.bus.subscribe("imageReady", () => {
          for (const idx of state().visible) {
            ctx.paint.requestPaint({ tag: "page-invalidate", pageIndex: idx });
          }
        }),
      );

      // CURRENT-PAGE model (kernel-truth): the consumer's ONE DOM sensor feeds
      // setCurrentPage; navigation intents go through goToPage. All indices are
      // 0-based (the public snippet tier converts to 1-based at its edge).
      unsubs.push(
        ctx.commands.register("viewport.setCurrentPage", (index: number) => {
          if (!Number.isFinite(index)) return;
          const clamped = clampPage(index, state().pageCount);
          if (clamped === state().currentPage) return; // dedup — no emit
          store.dispatch("setCurrentPage", clamped);
          ctx.bus.emit("viewport:page-changed", { index: state().currentPage });
        }),
        ctx.commands.register("viewport.goToPage", (index: number) => {
          if (!Number.isFinite(index)) return;
          const clamped = clampPage(index, state().pageCount);
          if (clamped !== state().currentPage) {
            store.dispatch("setCurrentPage", clamped);
            ctx.bus.emit("viewport:page-changed", { index: state().currentPage });
          }
          // ALWAYS scroll (even unchanged) — preserves today's re-align-page-top
          // behavior on goTo(currentPage).
          ctx.bus.emit("viewport:scroll-to-page", { index: clamped });
        }),
      );

      // Query for the adapter to size placeholders without reaching into core.
      unsubs.push(
        ctx.commands.register("viewport.pageModel", (): PageModel => {
          const s = state();
          return {
            pageCount: s.pageCount,
            pageSize: s.pageSize,
            loadComplete: s.loadComplete,
            currentPage: s.currentPage,
          };
        }),
        // Doc switch: clear the slice (parity with viewer.reset / bus.clearReplay).
        ctx.commands.register("viewport.reset", () => {
          store.dispatch("reset", undefined);
          // Doc-switch page reset is deliberately SILENT (no page-changed emit —
          // parity with today), but the sticky replay must not hand doc A's page
          // to a late-mounted consumer on doc B. SYNC-DISPATCH NOTE (verified):
          // kernel.exec → commands.exec invokes this handler BEFORE its first
          // await, so DomViewportAdapter.clear()'s `void kernel.exec(...)` runs
          // dispatch+clearReplay synchronously alongside its own
          // clearReplay("pageReady"/"loadComplete") — no deferred window.
          ctx.bus.clearReplay("viewport:page-changed");
        }),
      );
    },

    destroy(_ctx: KernelContext) {
      for (const u of unsubs.splice(0)) u();
      store.dispatch("reset", undefined);
    },
  };
}
