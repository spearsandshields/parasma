/**
 * plugin-grid — the continuous spreadsheet GRID: windowed virtual-scroll state +
 * the grid-vs-pages MODE + the grid cell hit-test DECISION.
 *
 * The grid is a SEPARATE windowed virtual scroller: ONE #grid-canvas
 * sized to the viewport + a full-extent #grid-spacer driving the native scrollbar,
 * painting only the visible window via viewer.paintGridViewport — NOT the per-page
 * `.page` canvases (those are plugin-viewport's). The grid is the DEFAULT xlsx view.
 *
 * BOUNDARY: the PLUGIN owns the DECISIONS/state (mode, extent, the
 * cell-at decision); the DomGridAdapter owns the concrete grid DOM (the canvas +
 * spacer + scroll listener + rAF coalescer + the DPR backing store). The plugin
 * NEVER touches the DOM. It emits a `grid-viewport` PAINT INTENT (so the adapter
 * repaints), `grid:mode-changed` (the adapter shows/hides #gridview vs #viewer),
 * and `grid:resetup` (the adapter re-sizes + repaints on a sheet switch).
 *
 * Like plugin-viewport / plugin-zoom / plugin-sheets it is NOT one of the two
 * XState machines (loader + fonts only) — no async lifecycle with
 * cancellation — so it is a lightweight `@xstate/store` slice.
 *
 * documentKind is cached off the replayable `loader:loaded` event (mirrors
 * plugin-cursor) so the excel-only mode clamp is a synchronous pure read: grid
 * mode is meaningful ONLY for spreadsheets, so grid.setMode(true) is ignored for
 * a word/ppt document (and a doc-switch reset drops mode back to 'pages').
 *
 * deps: ["sheets", "viewport"] — sheets owns the active-sheet model the grid
 * paints/sizes for; viewport for page-model ordering/kind parity. Two-phase boot
 * guarantees both initialize() before grid.initialize().
 */

import type { DocumentInfo, DocumentKind, GridPaneInfo } from "@parasma/core";
import type { GridViewer } from "./ports";
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
    "grid.mode": () => GridMode;
    "grid.setMode": (on: boolean) => void;
    "grid.toggle": () => void;
    "grid.extent": (sheet: number) => Promise<{ width: number; height: number } | null>;
    "grid.cellAt": (q: GridCellQuery) => Promise<{ ref: string; raw: string; display: string } | null>;
    "grid.paneInfo": (sheet: number) => Promise<GridPaneInfo | null>;
    "grid.paintWindow": (p: { sheet: number; scrollX: number; scrollY: number; viewW: number; viewH: number }) => void;
    "grid.saveScroll": (p: { sheet: number; x: number; y: number }) => void;
    "grid.scrollFor": (sheet: number) => Promise<GridScrollPos>;
  }
}

/**
 * Bus events OWNED by this plugin (declared here, not in the kernel's
 * bus.ts). Declaration-merged into the open {@link BusEventMap} so adding a
 * plugin event no longer edits the kernel. The `../bus` path and `../commands`
 * path below both resolve to the same kernel modules the barrel re-exports,
 * so the augmentation is program-wide for every consumer.
 */
declare module "../bus" {
  interface BusEventMap {
    /**
     * Grid view mode changed (plugin-grid). 'grid' = the continuous
     * windowed virtual scroller (the spreadsheet DEFAULT); 'pages' =
     * the print-page view. Sticky ("latest") so a late-attached grid adapter knows
     * the current mode (and to show/hide #gridview vs #viewer accordingly).
     */
    "grid:mode-changed": { mode: "grid" | "pages" };
    /**
     * Grid needs a full re-setup for `sheet` (plugin-grid). Emitted when the active
     * sheet changes while in grid mode (the adapter re-sizes the spacer + canvas and
     * repaints from the origin). Sticky
     * ("latest") so a late adapter re-setups to the current sheet.
     */
    "grid:resetup": { sheet: number };
  }
}

/** The grid view mode: the continuous virtual scroller, or the print-page view. */
export type GridMode = "grid" | "pages";

/** A remembered per-sheet scroll position (CONTENT-space px, zoom-independent). */
export interface GridScrollPos {
  x: number;
  y: number;
}

/** The grid plugin's store-slice state: view mode + the current sheet's extent. */
export interface GridState extends Record<string, unknown> {
  /** 'grid' (continuous virtual scroller, the excel default) or 'pages'. */
  mode: GridMode;
  /** Full pixel extent of the active sheet (spacer size); null pre-setup. */
  extent: { width: number; height: number } | null;
  /**
   * Per-sheet remembered scroll positions (sheet-switch scroll restore): keyed by sheet index, CONTENT-space px (zoom-independent), saved
   * by the adapter as the user scrolls and restored on tab return. Cleared on
   * doc switch with the rest of the slice (the loader-status reset).
   */
  scrolls: Record<number, GridScrollPos>;
}

// The grid is the DEFAULT xlsx view. The slice starts in 'pages'
// (no document yet); setMode(true) flips it to 'grid' once an excel doc loads.
const initialState: GridState = { mode: "pages", extent: null, scrolls: {} };

function makeSlice(): StoreSlice<GridState> {
  return {
    state: initialState,
    on: {
      setMode: (s, mode: GridMode) => ({ ...s, mode }),
      setExtent: (s, extent: { width: number; height: number } | null) => ({ ...s, extent }),
      saveScroll: (s, p: { sheet: number; x: number; y: number }) => ({
        ...s,
        scrolls: { ...s.scrolls, [p.sheet]: { x: p.x, y: p.y } },
      }),
      reset: () => ({ ...initialState }),
    },
  };
}

/** Query shape for the grid cell hit-test (content-space px + scroll offset). */
export interface GridCellQuery {
  sheet: number;
  /**
   * X in CONTENT-space canvas-relative px. Under grid zoom the ADAPTER
   * divides its raw CSS px by the zoom scale before querying — the coordinate
   * transform between DOM space and content space is the adapter's job
   * (mirrors its spacer scroll-scale mapping); the plugin/engine stay zoom-blind.
   */
  x: number;
  /** Y in CONTENT-space canvas-relative px (see `x`). */
  y: number;
  scrollX: number;
  scrollY: number;
}

/**
 * The grid plugin. Construct with the SAME Viewer the consumer renders into
 * (mirrors viewportPlugin(viewer) / sheetsPlugin(viewer)) so it can read
 * viewer.gridExtent / gridCellAt without a new engine accessor.
 */
export function gridPlugin(viewer: GridViewer): Plugin {
  const store = createSliceStore(makeSlice());
  const unsubs: Array<() => void> = [];
  const state = (): GridState => store.get();
  // Cached document kind (mirrors plugin-cursor): the mode clamp is excel-only.
  let documentKind: DocumentKind | null = null;
  // The active sheet, mirrored off 'sheets:changed' so the resetup reaction and
  // the (rare) extent fetch don't need a cross-plugin command round-trip.
  let activeSheet = 0;

  return {
    id: "grid",
    deps: ["sheets", "viewport"],

    initialize(ctx: KernelContext) {
      unsubs.push(
        // Cache documentKind for the excel-only mode clamp (replay-safe) — and
        // OWN the default-mode decision: a spreadsheet flips to the continuous
        // grid HERE, in the plugin, the moment the doc loads. Previously the
        // flip was a side effect of the sheet-TAB strip re-rendering (chrome),
        // which silently no-opped whenever the tab rebuild bailed (empty
        // sheetInfo, subscriber-order shifts — observed as an xlsx stuck in
        // Pages). Mode is engine state; the plugin decides it.
        ctx.bus.subscribe("loader:loaded", (info) => {
          documentKind = (info as DocumentInfo).documentKind;
          if (documentKind === "excel" && state().mode !== "grid") {
            store.dispatch("setMode", "grid");
            ctx.bus.emit("grid:mode-changed", { mode: "grid" });
          }
        }),
        // Doc switch: a reset to a non-ready status clears mode/extent (back to
        // 'pages') so a stale grid can't outlive the previous document.
        ctx.bus.subscribe("loader:status-changed", (s) => {
          if (s === "idle" || s === "planning" || s === "loading") {
            documentKind = null;
            activeSheet = 0;
            store.dispatch("reset", undefined);
            ctx.bus.emit("grid:mode-changed", { mode: "pages" });
          }
        }),
        // A sheet switch (or build) re-fetches the extent and, IF in grid mode,
        // asks the adapter to re-setup (re-size spacer/canvas + repaint from the
        // origin).
        ctx.bus.subscribe("sheets:changed", ({ active }) => {
          activeSheet = active;
          // viewer.gridExtent() is a worker-ready async seam; await inside
          // a void IIFE (resolves immediately in-process). The resetup emit
          // moves in with the extent dispatch so the adapter sizes off the fresh
          // extent — both land together a microtask later.
          void (async () => {
            store.dispatch("setExtent", (await viewer.gridExtent(active)) ?? null);
            if (state().mode === "grid") {
              ctx.bus.emit("grid:resetup", { sheet: active });
            }
          })();
        }),
      );

      // ── Commands (the plugin surface) ───────────────────────────────────────

      unsubs.push(
        ctx.commands.register("grid.mode", (): GridMode => state().mode),

        // Set grid mode. Clamped to excel-only (the grid is meaningless for a
        // word/ppt doc) — a non-excel setMode(true) is ignored. Emits the mode edge.
        ctx.commands.register("grid.setMode", (on: boolean) => {
          const mode: GridMode = on && documentKind === "excel" ? "grid" : "pages";
          if (mode === state().mode) {
            // Re-emit on a no-op too so a freshly-attached adapter is driven into
            // the right surface (idempotent).
            ctx.bus.emit("grid:mode-changed", { mode });
            if (mode === "grid") ctx.bus.emit("grid:resetup", { sheet: activeSheet });
            return;
          }
          store.dispatch("setMode", mode);
          ctx.bus.emit("grid:mode-changed", { mode });
          // Entering grid mode → the adapter must size + paint for the active sheet.
          if (mode === "grid") ctx.bus.emit("grid:resetup", { sheet: activeSheet });
        }),

        // Toggle grid ⇄ pages (the #view-toggle button). Flip, then re-use the
        // setMode clamp so a flip into 'grid' on a non-excel doc is still ignored.
        ctx.commands.register("grid.toggle", () => {
          ctx.commands.exec("grid.setMode", state().mode !== "grid");
        }),

        // Thin pass-throughs so the adapter sizes/hit-tests without importing core.
        // Both gridExtent/gridCellAt are worker-ready async seams; the
        // commands stay one-call pass-throughs that await + forward the result.
        ctx.commands.register(
          "grid.extent",
          async (sheet: number): Promise<{ width: number; height: number } | null> =>
            (await viewer.gridExtent(sheet)) ?? null,
        ),
        ctx.commands.register(
          "grid.cellAt",
          async (q: GridCellQuery): Promise<{ ref: string; raw: string; display: string } | null> =>
            (await viewer.gridCellAt(q.sheet, q.x, q.y, q.scrollX, q.scrollY)) ?? null,
        ),
        // Frozen panes (§18.3.1.66, grid view): the sheet's pane info — band sizes
        // in CONTENT px + the topLeftCell initial body scroll. Null for pane-less
        // sheets (the adapter then changes nothing). Optional-chained so a stale
        // core build degrades to "no pane".
        ctx.commands.register(
          "grid.paneInfo",
          async (sheet: number): Promise<GridPaneInfo | null> =>
            (await viewer.gridPaneInfo?.(sheet)) ?? null,
        ),

        // Optional thin paint trigger: emit the 'grid-viewport' intent the adapter
        // drains. The adapter normally paints directly on scroll (it owns the
        // pixels), but this lets the plugin force a repaint through the same seam.
        ctx.commands.register(
          "grid.paintWindow",
          (p: { sheet: number; scrollX: number; scrollY: number; viewW: number; viewH: number }) => {
            ctx.paint.requestPaint({ tag: "grid-viewport", ...p });
          },
        ),

        // ── Per-sheet scroll memory (sheet-switch scroll restore) ──────────────
        // The PLUGIN owns the remembered positions (engine-adjacent view state,
        // owned here, not by the DOM); the adapter reports content-space positions as the user
        // scrolls and asks for the remembered one on each sheet re-setup. The
        // doc-switch loader reset above clears the map with the slice.
        ctx.commands.register("grid.saveScroll", (p: { sheet: number; x: number; y: number }) => {
          store.dispatch("saveScroll", p);
        }),
        // The remembered position wins; an UNVISITED sheet restores the pane's
        // saved topLeftCell body scroll (§18.3.1.66 — Excel reopens where the
        // file left off; absent → A1 → the origin).
        ctx.commands.register(
          "grid.scrollFor",
          async (sheet: number): Promise<GridScrollPos> => {
            const remembered = state().scrolls[sheet];
            if (remembered) return remembered;
            const pane = (await viewer.gridPaneInfo?.(sheet)) ?? null;
            return pane ? { x: pane.scrollX, y: pane.scrollY } : { x: 0, y: 0 };
          },
        ),
      );
    },

    destroy(_ctx: KernelContext) {
      for (const u of unsubs.splice(0)) u();
      documentKind = null;
      store.dispatch("reset", undefined);
    },
  };
}
