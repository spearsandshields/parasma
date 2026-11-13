/**
 * plugin-sheets — the Excel SHEET MODEL + active-sheet switching.
 *
 * This plugin OWNS the sheet model (the list of {name, firstPage, pageCount}
 * runs from `viewer.sheetInfo()`), the active sheet index, and the SWITCHING
 * DECISIONS (clamp + which sheet is active + which pages belong to it). Like
 * plugin-viewport / plugin-zoom it is NOT one of the two XState machines
 * because it has no async lifecycle with cancellation, so it is a
 * lightweight `@xstate/store` slice.
 *
 * BOUNDARY: the plugin holds the DECISION; the DOM is the consumer's.
 * It NEVER touches the DOM. It emits a `sheets:changed` bus event ({ active,
 * sheets }) and the consumer's thin tab renderer applies it — rebuilding the
 * #sheet-tabs buttons, toggling .selected, and applying the PAGED-VIEW page
 * filter (display:none on pages outside the active sheet's [firstPage,
 * firstPage+pageCount) run). Sheet filtering is plain show/hide, not a canvas
 * paint, so it needs no paint-intent variant.
 *
 * BUILD timing: `viewer.sheetInfo()` is only valid POST-LAYOUT (every sheet's
 * pages must be streamed before the per-sheet page runs exist). The ONLY event
 * that guarantees that is the bridged `loadComplete` (runLayout's `.then`) — NOT
 * `loader:loaded`, which fires when `viewer.load()` RESOLVES, and `#doLoadXlsx`
 * returns its DocumentInfo immediately after KICKING OFF streaming layout (it
 * does not await `#layoutPromise`). So the plugin builds on `loadComplete`.
 *
 * No documentKind gate is needed: `viewer.sheetInfo()` returns `[]` for the
 * docx-only wasm build AND for any non-spreadsheet document (core contract), so a
 * word/ppt load yields an empty set ⇒ empty strip. This also dodges the
 * event-ordering fragility of caching kind off `loader:loaded` (whose order vs
 * `loadComplete` differs between the streaming engine and a synchronous fake).
 *
 * A doc switch (loader status → idle/planning/loading) resets the slice.
 *
 * deps: ["loader", "viewport"] — loader bridges loadComplete/loader:loaded;
 * viewport owns the per-page model the paged-view filter operates over. Two-phase
 * boot guarantees both initialize() before sheets.initialize().
 */

import type { SheetsViewer } from "./ports";
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
    "sheets.list": () => SheetInfo[];
    "sheets.active": () => number;
    "sheets.select": (idx: number) => void;
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
     * Sheet model changed (plugin-sheets). Carries the current sheet set
     * + active index. Emitted on build (loadComplete for an excel doc) and on every
     * `sheets.select`. Sticky ("latest") so a late-attached tab renderer catches the
     * current sheet set / active sheet. `sheets` is empty when the active document
     * is not a spreadsheet (or a doc switch reset just fired).
     */
    "sheets:changed": {
      active: number;
      sheets: { name: string; firstPage: number; pageCount: number; maxRow: number }[];
    };
  }
}

/** One sheet's contiguous run of pages (= viewer.sheetInfo() element). */
export interface SheetInfo {
  name: string;
  firstPage: number;
  pageCount: number;
  /** Highest 1-based row with content (0 when empty / older wasm); drives the
   *  SML continuous-grid search row-window walk. Mirrors core's sheetInfo(). */
  maxRow: number;
}

/** The sheets plugin's store-slice state: sheet model + active index. */
export interface SheetsState extends Record<string, unknown> {
  /** The current document's sheets (empty for non-excel / pre-layout). */
  sheets: SheetInfo[];
  /** Active sheet index, clamped to [0, sheets.length). */
  active: number;
}

const initialState: SheetsState = { sheets: [], active: 0 };

function makeSlice(): StoreSlice<SheetsState> {
  return {
    state: initialState,
    on: {
      // Adopt a new sheet set; reset active to the first sheet.
      setSheets: (s, sheets: SheetInfo[]) => ({ ...s, sheets, active: 0 }),
      // Select a sheet, clamped to the valid range.
      select: (s, idx: number) => ({
        ...s,
        active: s.sheets.length === 0 ? 0 : Math.max(0, Math.min(idx, s.sheets.length - 1)),
      }),
      reset: () => ({ ...initialState }),
    },
  };
}

/**
 * The sheets plugin. Construct with the SAME Viewer the consumer renders into
 * (mirrors viewportPlugin(viewer) / cursorPlugin(viewer)) so it can read the live
 * `viewer.sheetInfo()` without a new engine accessor.
 */
export function sheetsPlugin(viewer: SheetsViewer): Plugin {
  const store = createSliceStore(makeSlice());
  const unsubs: Array<() => void> = [];

  const state = (): SheetsState => store.get();

  const emitChanged = (ctx: KernelContext): void => {
    const s = state();
    ctx.bus.emit("sheets:changed", { active: s.active, sheets: s.sheets });
  };

  return {
    id: "sheets",
    deps: ["loader", "viewport"],

    initialize(ctx: KernelContext) {
      unsubs.push(
        // BUILD on loadComplete (the only post-layout seam — see header). No kind
        // gate: viewer.sheetInfo() is [] for non-spreadsheets, so a word/ppt load
        // yields an empty strip. A spreadsheet with no non-empty sheets is [] too.
        ctx.bus.subscribe("loadComplete", () => {
          // viewer.sheetInfo() is a worker-ready async seam; the bus
          // handler can't be async, so await inside a void IIFE (resolves
          // immediately in-process). The setSheets dispatch + sheets:changed
          // emit just land a microtask later — order vs other subscribers is fine.
          void (async () => {
            store.dispatch("setSheets", await viewer.sheetInfo());
            emitChanged(ctx);
          })();
        }),
        // Doc switch: a reset to a non-ready status clears the sheet slice, then
        // emits the now-empty set so a stale
        // strip can't outlive the previous document.
        ctx.bus.subscribe("loader:status-changed", (s) => {
          if (s === "idle" || s === "planning" || s === "loading") {
            store.dispatch("reset", undefined);
            emitChanged(ctx);
          }
        }),
      );

      // Commands (the plugin surface). The thin tab renderer drives select.
      unsubs.push(
        ctx.commands.register("sheets.list", (): SheetInfo[] => state().sheets),
        ctx.commands.register("sheets.active", (): number => state().active),
        ctx.commands.register("sheets.select", (idx: number) => {
          store.dispatch("select", idx);
          // The DECISION (which sheet is active) lives here; the consumer applies
          // the DOM (tab .selected + paged-view page filter) off this event;
          // plugin-grid also reacts (re-setup the grid for the sheet).
          emitChanged(ctx);
        }),
      );
    },

    destroy(_ctx: KernelContext) {
      for (const u of unsubs.splice(0)) u();
      store.dispatch("reset", undefined);
    },
  };
}
