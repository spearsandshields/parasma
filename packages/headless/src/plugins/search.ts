/**
 * plugin-search — the full-text-search ENGINE LOGIC. Owns query
 * state, results, the active-match index, navigation, cancellation, and
 * debounce. Surfaced as kernel commands + a `search:changed`
 * event (+ `search:navigate` on next/prev/goto). Mirrors plugin-comments: a
 * consumer of the headless layer gets search with NO UI — the snippet provides
 * the headed box/counter/highlight.
 *
 * BOUNDARY: the plugin OWNS the model/decision; the DOM is the
 * consumer's. It NEVER touches the DOM. It drives the wasm highlight pass
 * indirectly through the public `viewer.setSearchHighlight()` seam (a state set
 * the next paint consumes) and emits `search:navigate` ({page,y,sheet} +
 * {cellRef,gridX,gridY} for SML grid scroll-to) for the consumer's scroll adapter.
 *
 * ORCHESTRATION (cancellation, debounce, streaming): the wasm Viewer is
 * single-threaded — a running synchronous wasm call cannot be interrupted — so
 * wasm stays a bounded read-only primitive (`viewer.searchPage(idx, q, opts)`,
 * one page = one bounded call) and TS owns the loop:
 *   - DEBOUNCE: a fresh query waits `debounceMs` (default 150, override via
 *     searchPlugin({ debounceMs })) of keystroke silence before starting a loop.
 *   - CHUNKED LOOP: awaits one searchPage per tick, yielding to the event loop
 *     between ticks, streaming partial results via `search:changed`.
 *   - GENERATION TOKEN: bumped on every new query / scope change / clear / doc
 *     switch. The in-flight loop captures its generation and checks the live one
 *     each tick, self-aborting when stale → wasm never burns cycles on a dead
 *     query, and a superseded loop's partial results never land.
 *
 * ITERATION BOUND: WML iterates page indices [0, pageCount)
 * — the whole document is one logical "sheet". SML iterates SHEETS
 * (viewer.sheetInfo()): the ACTIVE sheet in 'sheet' scope (default, Excel's
 * Within:Sheet), or every sheet in 'workbook' scope (Within:Workbook), querying
 * each sheet's representative page (firstPage) — the wasm resolves the sheet from
 * the page idx and (in workbook scope) widens past the active sheet. Flipping back
 * to 'sheet' cancels any in-flight workbook scan via the generation bump.
 *
 * Doc switch (loader status → idle/planning/loading) resets the slice to empty
 * and bumps the generation (parity with plugin-comments' reset), so a stale loop
 * or result set can't outlive the previous document.
 *
 * deps: ["viewport", "sheets"] — viewport (→loader) bridges loadComplete onto the
 * bus, so the page model the WML loop iterates is settled post-layout; sheets owns
 * the active-sheet index the SML 'sheet'-scope loop targets, surfaced here by
 * tracking the `sheets:changed` event (read synchronously inside the loop without
 * an async command round-trip). Two-phase boot guarantees both initialize() first.
 */

import type { SearchMatch, SearchOpts } from "@parasma/core";
import type { SearchViewer } from "./ports";
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
    "search.query": (text: string) => void;
    "search.setScope": (scope: SearchScope) => void;
    "search.opts": (opts: { caseSensitive: boolean; wholeWord: boolean }) => void;
    "search.next": () => void;
    "search.prev": () => void;
    "search.goto": (i: number) => void;
    "search.clear": () => void;
    "search.results": () => SearchMatch[];
    "search.active": () => number | null;
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
     * Full-text-search state changed (plugin-search). Carries the live
     * query, the (possibly partial) results streamed in so far, the active-match
     * index, and the loop status. Emitted on every query/scope/clear edge, on each
     * streaming tick as partial results land, and on next/prev/goto. Sticky
     * ("latest") so a late-attached search box / counter catches the current query
     * + match count + active index without missing the build. `results` is empty
     * when the query is empty, the loop was cancelled, or a doc switch reset just
     * fired. The SearchMatch shape is inlined (mirroring how comments:changed
     * inlines Comment) to keep bus typing free of a @parasma/core import.
     */
    "search:changed": {
      query: string;
      results: {
        page: number;
        rects: { x: number; y: number; w: number; h: number }[];
        y: number;
        sheet: string | null;
        cellRef: string | null;
        gridX: number;
        gridY: number;
      }[];
      activeIdx: number | null;
      status: "idle" | "searching" | "done";
      /** The scope the results were produced under ('sheet' = active sheet,
       *  Excel's Within:Sheet — the default; 'workbook' = every sheet). Lets
       *  a consumer render a Within control reactively; always 'sheet' for
       *  non-SML documents (the scope simply has nothing to widen). */
      scope: SearchScope;
    };
    /**
     * A search navigation was requested (plugin-search, next/prev/goto). Carries
     * the active match's page + top-y (layout px) for scroll-to (the consumer routes
     * it through its scrollToPage adapter) and the originating sheet name for SML
     * (workbook-scope next/prev crosses sheet tabs → the consumer issues sheets.select
     * first), or null for WML. Transient ("none"): a late subscriber must NOT replay
     * a stale scroll — the navigation already happened.
     *
     * `cellRef`/`gridX`/`gridY` carry the SML match's identity + CONTENT-space
     * (gutter-exclusive, pre-zoom) position so a grid-mode consumer scrolls the GRID
     * to the cell (not scrollToPage). `null`/`0` for WML & PML, which scroll by
     * `page`/`y`.
     */
    "search:navigate": {
      page: number;
      x: number;
      y: number;
      sheet: string | null;
      cellRef: string | null;
      gridX: number;
      gridY: number;
    };
    /**
     * The SETTLED navigation target — final coordinates, safe to scroll to NOW.
     * Where `search:navigate` fires immediately with raw match data (and leaves
     * cross-sheet selection, settle-timing and deferred-rect resolution to the
     * consumer — the "settle dance" every integration used to re-derive), this
     * event fires AFTER the plugin has: selected the target sheet if needed,
     * awaited the adapter's settle (`grid:resetup` in grid mode /
     * `sheets:changed` in pages mode, sticky replays skipped), and resolved the
     * pages-mode deferred rect through `viewer.resolveSearchRect`. Transient
     * ("none"): a late subscriber must not replay a stale reveal.
     *
     * `target` is a discriminated union so a consumer scrolls the right
     * surface without re-deriving mode: `grid` targets are CONTENT-space
     * (pre-zoom, gutter-exclusive); `page` targets are page-space.
     */
    "search:navigate-resolved": {
      target:
        | { kind: "grid"; sheet: number; cellRef: string; gridX: number; gridY: number }
        | { kind: "page"; page: number; x: number; y: number };
    };
  }
}

/** SML search scope, mirroring Excel's Find Within: Sheet / Workbook. */
export type SearchScope = "sheet" | "workbook";

/** Loop status: idle (no/empty query), searching (loop in flight), done. */
export type SearchStatus = "idle" | "searching" | "done";

/** The search plugin's store-slice state. */
export interface SearchState extends Record<string, unknown> {
  /** The current query string (empty ⇒ no search; clears highlights). */
  query: string;
  /** Match semantics toggles. Default: case-insensitive substring. */
  opts: { caseSensitive: boolean; wholeWord: boolean };
  /** SML scope (Word is always whole-document; scope is inert for WML). */
  scope: SearchScope;
  /** Results streamed in so far (partial during a running loop). */
  results: SearchMatch[];
  /** The active match index into `results`, or null when there are none. */
  activeIdx: number | null;
  /** Loop status. */
  status: SearchStatus;
}

const initialState: SearchState = {
  query: "",
  opts: { caseSensitive: false, wholeWord: false },
  scope: "sheet",
  results: [],
  activeIdx: null,
  status: "idle",
};

function makeSlice(): StoreSlice<SearchState> {
  return {
    state: initialState,
    on: {
      // Begin a new query: adopt the query string, drop prior results, mark the
      // loop as about-to-run. Active resets to null until a first match lands.
      beginQuery: (s, query: string) => ({
        ...s,
        query,
        results: [],
        activeIdx: null,
        status: query === "" ? "idle" : "searching",
      }),
      // Stream a batch of partial results in. If this is the FIRST match of a
      // fresh query, seed the active index to 0 (so the first hit is emphasised).
      appendResults: (s, batch: SearchMatch[]) => {
        const results = [...s.results, ...batch];
        return {
          ...s,
          results,
          activeIdx: s.activeIdx === null && results.length > 0 ? 0 : s.activeIdx,
        };
      },
      // The loop finished (or was a no-op for an empty query).
      finish: (s) => ({ ...s, status: "done" as SearchStatus }),
      // Flip the SML scope (cancels any in-flight loop via the generation bump,
      // performed by the command, not the transition).
      setScope: (s, scope: SearchScope) => ({ ...s, scope }),
      // Flip the match-semantics toggles (case-sensitive / whole-word). Like
      // setScope, the in-flight cancel + loop restart is done by the command.
      setOpts: (s, opts: { caseSensitive: boolean; wholeWord: boolean }) => ({ ...s, opts }),
      // Toggle the active match index (next/prev/goto). Caller pre-clamps/wraps.
      setActive: (s, idx: number | null) => ({ ...s, activeIdx: idx }),
      // Clear everything but keep the scope/opts (Esc / clear button).
      clear: (s) => ({ ...s, query: "", results: [], activeIdx: null, status: "idle" as SearchStatus }),
      // Doc switch: full reset to defaults.
      reset: () => ({ ...initialState }),
    },
  };
}

/** Plugin options. */
export interface SearchPluginOptions {
  /** Keystroke-silence debounce before a loop starts. Default 150ms. */
  debounceMs?: number;
}

const DEFAULT_DEBOUNCE_MS = 150;

/** One event-loop yield between ticks so the loop is cancellable + non-blocking. */
const yieldTick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

/** SML rows scanned per tick. A huge sheet is split into half-open row
 * windows of this size so the loop yields (cancellable, progressive count) WITHIN
 * a sheet instead of scanning the whole sheet in one blocking call. Big enough
 * that a normal sheet is one window (no overhead), small enough that a worst-case
 * dense window stays a sub-frame task. */
const SML_ROWS_PER_TICK = 2048;

/**
 * The search plugin. Construct with the SAME Viewer the consumer renders into
 * (mirrors commentsPlugin(viewer) / sheetsPlugin(viewer)) so it reads the live
 * `viewer.pageCount()` / `viewer.sheetInfo()` (worker-ready async seams — awaited
 * in the loop) and drives `viewer.searchPage` / `viewer.setSearchHighlight` without
 * a new engine accessor.
 */
export function searchPlugin(viewer: SearchViewer, options: SearchPluginOptions = {}): Plugin {
  const debounceMs = options.debounceMs ?? DEFAULT_DEBOUNCE_MS;
  const store = createSliceStore(makeSlice());
  const unsubs: Array<() => void> = [];

  const state = (): SearchState => store.get();

  // The GENERATION TOKEN. Bumped on every new query / scope change / clear / doc
  // switch. A running loop captures the value at start and checks it each tick;
  // a mismatch means it was superseded → it stops without emitting.
  let generation = 0;
  // The pending debounce timer for the in-flight query (cleared on supersede).
  let debounceTimer: ReturnType<typeof setTimeout> | null = null;
  // Grid-vs-pages, tracked off `grid:mode-changed`: the settle event and the
  // resolved-target SHAPE both depend on it. Default 'pages' (the pre-grid
  // surface; WML/PML never read it).
  let gridMode: "grid" | "pages" = "pages";
  // The settled-navigation machinery. One pending resolution at a time:
  // a token guards the async tail, and the settle subscription is torn down on
  // supersede so a stale settle can never resolve a dead navigation.
  let navToken = 0;
  let pendingSettleUnsub: (() => void) | null = null;

  // The active sheet index, tracked off `sheets:changed` so the SML 'sheet'-scope
  // loop knows which sheet to target without an async `sheets.active` round-trip.
  let activeSheet = 0;

  const opaqueOpts = (): SearchOpts => {
    const s = state();
    return {
      caseSensitive: s.opts.caseSensitive,
      wholeWord: s.opts.wholeWord,
      scope: s.scope,
    };
  };

  const emitChanged = (ctx: KernelContext): void => {
    const s = state();
    ctx.bus.emit("search:changed", {
      query: s.query,
      results: s.results,
      activeIdx: s.activeIdx,
      status: s.status,
      scope: s.scope,
    });
  };

  /** Push the active-match state to the wasm so the next paint highlights it —
   * AND schedule that paint. The plugin owning its own repaint retires an
   * unwritten rule consumers previously had to discover from this file's
   * source: "after every search:changed you must exec
   * viewport.invalidateVisible or highlights never appear". Guarded by
   * `has()` so a kernel composed WITHOUT the viewport plugin still runs; no
   * opt-out flag on purpose — a second invalidate from a consumer who still
   * repaints manually coalesces in the paint layer, and no known consumer
   * batches paints (add the flag when one asks, not before). */
  const pushHighlight = (ctx: KernelContext): void => {
    const s = state();
    const active = s.activeIdx !== null ? s.results[s.activeIdx] : undefined;
    // The wasm paint pass interprets `activeIndex` as the active match's index
    // WITHIN ITS PAGE (collect_search_rects' per-page order), but `s.activeIdx`
    // is a GLOBAL index into the flattened `results`. Convert by counting earlier
    // results that share the active match's page — results are appended in page
    // order, so that count is the per-page rank the renderer enumerates.
    let perPageIdx = -1;
    if (active && s.activeIdx !== null) {
      perPageIdx = 0;
      for (let k = 0; k < s.activeIdx; k++) {
        if (s.results[k]?.page === active.page) perPageIdx += 1;
      }
    }
    // SML active-match identity: the wasm SML paint pass
    // emphasises by A1 ref, not the per-page index (the index drifts under the
    // continuous grid's scroll-windowed cell slice + covered-cell skips). Pass the
    // active match's `cellRef` for SML; `null` for WML/PML (they key on perPageIdx).
    // This groups by page the SAME way for all three: the active match's `page`
    // narrows the paint pass, then SML uses `cellRef` / WML+PML use `perPageIdx`.
    viewer.setSearchHighlight(
      s.query,
      opaqueOpts(),
      active ? active.page : -1,
      perPageIdx,
      active ? active.cellRef : null,
    );
    if (ctx.commands.has("viewport.invalidateVisible")) {
      void ctx.commands.exec("viewport.invalidateVisible");
    }
  };

  // Invalidate any in-flight loop/debounce: bump the generation and cancel the
  // pending debounce timer. Every state-changing command funnels through here.
  const supersede = (): number => {
    navToken += 1;
    pendingSettleUnsub?.();
    pendingSettleUnsub = null;
    generation += 1;
    if (debounceTimer !== null) {
      clearTimeout(debounceTimer);
      debounceTimer = null;
    }
    return generation;
  };

  /**
   * The chunked, cancellable loop. Captures `gen` at start; after each
   * (awaited) searchPage it re-checks the live generation and bails if stale —
   * so a superseded query's partial results NEVER land. Streams each page's
   * matches via `search:changed`, then yields a tick before the next page.
   */
  const runLoop = async (ctx: KernelContext, gen: number): Promise<void> => {
    // Build the iteration plan from the document shape. SML → sheet page indices
    // (active sheet, or all sheets in workbook scope); WML → [0, pageCount).
    // sheetInfo() is a worker-ready async seam — awaited (we're already in
    // an async loop). Re-check the generation after the await: building the plan
    // is the first step, and a newer query could have superseded us meanwhile.
    const sheets = await viewer.sheetInfo();
    if (gen !== generation) return;
    const isSml = sheets.length > 0;
    // A unit of work per tick. SML: a (page, row-window) so a huge sheet streams
    // WITHIN itself; WML/PML: a whole page (no window). One chunk per tiny sheet
    // (maxRow ≤ window, or maxRow 0 from an older wasm) is one whole-sheet call.
    type Chunk = { pageIdx: number; rowStart?: number; rowEnd?: number };
    let chunks: Chunk[];
    if (isSml) {
      const scope = state().scope;
      const targets =
        scope === "workbook"
          ? sheets
          : // Active-sheet only (Excel's Within:Sheet); clamp the tracked index.
            (() => {
              const sh = sheets[Math.max(0, Math.min(activeSheet, sheets.length - 1))];
              return sh ? [sh] : [];
            })();
      chunks = targets.flatMap((sh) => {
        const out: Chunk[] = [];
        for (let r = 1; r <= sh.maxRow; r += SML_ROWS_PER_TICK) {
          out.push({
            pageIdx: sh.firstPage,
            rowStart: r,
            rowEnd: Math.min(r + SML_ROWS_PER_TICK, sh.maxRow + 1), // half-open
          });
        }
        // maxRow 0 (empty sheet / older wasm) → one whole-sheet chunk.
        return out.length > 0 ? out : [{ pageIdx: sh.firstPage }];
      });
    } else {
      // viewer.pageCount() is an async method (worker-ready seam), not a getter.
      const pageCount = await viewer.pageCount();
      if (gen !== generation) return;
      chunks = Array.from({ length: pageCount }, (_, i) => ({ pageIdx: i }));
    }

    const query = state().query;
    const opts = opaqueOpts();

    for (const c of chunks) {
      // Cancellation check BEFORE the bounded wasm call: a stale loop must not
      // even invoke searchPage. The token now also aborts MID-sheet (between row
      // windows), not just between sheets.
      if (gen !== generation) return;
      const win = c.rowStart !== undefined ? { rowStart: c.rowStart, rowEnd: c.rowEnd! } : undefined;
      const batch = await viewer.searchPage(c.pageIdx, query, opts, win);
      // Re-check AFTER the await: the generation may have bumped while the
      // (async-seam) call was in flight. A superseded loop drops its batch.
      if (gen !== generation) return;
      if (batch.length > 0) {
        store.dispatch("appendResults", batch);
        // Seed the highlight on the first matches so the active emphasis paints.
        pushHighlight(ctx);
        emitChanged(ctx);
      }
      // Yield so a newer keystroke/scope can supersede us between windows.
      await yieldTick();
      if (gen !== generation) return;
    }

    // Done — mark status and emit a terminal changed event + final highlight push.
    store.dispatch("finish", undefined);
    pushHighlight(ctx);
    emitChanged(ctx);
  };

  // Start (or restart) a debounced search for the current query.
  const startDebounced = (ctx: KernelContext): void => {
    const gen = generation; // captured AFTER the supersede bump by the caller
    if (state().query === "") {
      // Empty query: nothing to search; clear highlights immediately.
      store.dispatch("finish", undefined);
      pushHighlight(ctx);
      emitChanged(ctx);
      return;
    }
    debounceTimer = setTimeout(() => {
      debounceTimer = null;
      // The debounce may have been outlived by a newer query/scope/clear.
      if (gen !== generation) return;
      void runLoop(ctx, gen);
    }, debounceMs);
  };

  return {
    id: "search",
    deps: ["viewport", "sheets"],

    initialize(ctx: KernelContext) {
      unsubs.push(
        // Track the active sheet (SML 'sheet'-scope iteration target). Sticky
        // ("latest") so this catches the current active index on (re)subscribe.
        ctx.bus.subscribe("grid:mode-changed", (e) => {
          gridMode = e.mode;
        }),
        ctx.bus.subscribe("sheets:changed", (e) => {
          activeSheet = e.active;
        }),
        // Doc switch: reset the slice + bump the generation so an in-flight loop
        // (or stale result set) can't outlive the previous document. Emit the now
        // empty set + clear the wasm highlight.
        ctx.bus.subscribe("loader:status-changed", (s) => {
          if (s === "idle" || s === "planning" || s === "loading") {
            supersede();
            store.dispatch("reset", undefined);
            pushHighlight(ctx);
            emitChanged(ctx);
          }
        }),
      );

      unsubs.push(
        // search.query(text) — the keystroke entry. Bump the generation (cancel any
        // in-flight loop/debounce), adopt the query, then start a debounced loop.
        ctx.commands.register("search.query", (text: string) => {
          supersede();
          store.dispatch("beginQuery", text);
          emitChanged(ctx);
          startDebounced(ctx);
        }),

        // search.setScope(scope) — flip SML Within. Cancels any in-flight scan and
        // (if there is a live query) restarts the loop for the new scope.
        ctx.commands.register("search.setScope", (scope: SearchScope) => {
          supersede();
          store.dispatch("setScope", scope);
          // Restart against the new scope: re-begin the same query (clears stale
          // results), then debounce-search again.
          const q = state().query;
          store.dispatch("beginQuery", q);
          emitChanged(ctx);
          startDebounced(ctx);
        }),

        // search.opts(opts) — flip the case-sensitive / whole-word toggles. Mirrors
        // setScope: cancel any in-flight loop, adopt the new opts, then (if there is
        // a live query) re-begin the SAME query so the loop re-runs with them.
        ctx.commands.register(
          "search.opts",
          (opts: { caseSensitive: boolean; wholeWord: boolean }) => {
            supersede();
            store.dispatch("setOpts", opts);
            const q = state().query;
            store.dispatch("beginQuery", q);
            emitChanged(ctx);
            startDebounced(ctx);
          },
        ),

        // search.next() — advance the active match, wrapping past the last to the
        // first. Emits search:navigate for the consumer's scroll adapter.
        ctx.commands.register("search.next", () => {
          const s = state();
          if (s.results.length === 0) return;
          const cur = s.activeIdx ?? -1;
          const next = (cur + 1) % s.results.length;
          gotoInternal(ctx, next);
        }),

        // search.prev() — step back, wrapping past the first to the last.
        ctx.commands.register("search.prev", () => {
          const s = state();
          if (s.results.length === 0) return;
          const cur = s.activeIdx ?? 0;
          const prev = (cur - 1 + s.results.length) % s.results.length;
          gotoInternal(ctx, prev);
        }),

        // search.goto(i) — jump to a specific match (clamped). Emits navigate.
        ctx.commands.register("search.goto", (i: number) => {
          const s = state();
          if (s.results.length === 0) return;
          const idx = Math.max(0, Math.min(i, s.results.length - 1));
          gotoInternal(ctx, idx);
        }),

        // search.clear() — Esc / clear. Cancel loop, drop query+results, clear the
        // wasm highlight, emit the empty set.
        ctx.commands.register("search.clear", () => {
          supersede();
          store.dispatch("clear", undefined);
          pushHighlight(ctx);
          emitChanged(ctx);
        }),

        // Read-only query commands (plugin surface).
        ctx.commands.register("search.results", (): SearchMatch[] => state().results),
        ctx.commands.register("search.active", (): number | null => state().activeIdx),
      );
    },

    destroy(_ctx: KernelContext) {
      supersede();
      for (const u of unsubs.splice(0)) u();
      store.dispatch("reset", undefined);
    },
  };

  // Set the active match, push the highlight, emit changed + a navigate request.
  function gotoInternal(ctx: KernelContext, idx: number): void {
    store.dispatch("setActive", idx);
    pushHighlight(ctx);
    emitChanged(ctx);
    const m = state().results[idx];
    if (m) {
      // x = the match's first rect left edge (layout px) so the consumer can
      // reveal it HORIZONTALLY too — at high zoom the page is wider than the
      // viewport and a vertical-only scroll leaves right-column matches off-screen.
      const x = m.rects[0]?.x ?? 0;
      // SML grid coord: a grid-mode consumer scrolls the
      // GRID to (cellRef, gridX, gridY) — content-space, pre-zoom — instead of
      // scrollToPage. `cellRef` is null for WML/PML, which scroll by page/y.
      ctx.bus.emit("search:navigate", {
        page: m.page,
        x,
        y: m.y,
        sheet: m.sheet,
        cellRef: m.cellRef,
        gridX: m.gridX,
        gridY: m.gridY,
      });
      void resolveNavigate(ctx, m, x);
    }
  }

  /**
   * Settle the navigation and emit `search:navigate-resolved` with
   * final, scroll-safe coordinates. This is the "settle dance" every
   * integration previously re-derived by hand, moved behind the plugin:
   *
   *  - WML/PML (`m.sheet == null`): nothing to settle — resolve immediately
   *    as a page target.
   *  - SML cross-sheet: select the target sheet, then await the adapter's
   *    settle — `grid:resetup` for the TARGET sheet in grid mode (the adapter
   *    restores the sheet's saved scroll during re-setup; scrolling earlier
   *    gets overwritten), `sheets:changed` for the target in pages mode. Both
   *    are STICKY events: `subscribe()` replays the pre-switch payload
   *    synchronously, so the replay window is skipped by construction (the
   *    classic downstream bug was resolving against that stale replay).
   *  - SML pages mode: the match rect was DEFERRED at search time — resolve
   *    the one cell now via `viewer.resolveSearchRect`; page-origin fallback
   *    when the cell is beyond the print range.
   *
   * Guarded by `navToken` at every await edge: a supersede (new query, clear,
   * another navigate) drops the pending resolution instead of emitting stale.
   */
  async function resolveNavigate(ctx: KernelContext, m: SearchMatch, x: number): Promise<void> {
    navToken += 1;
    const token = navToken;
    pendingSettleUnsub?.();
    pendingSettleUnsub = null;

    if (m.sheet === null) {
      ctx.bus.emit("search:navigate-resolved", {
        target: { kind: "page", page: m.page, x, y: m.y },
      });
      return;
    }

    const sheets = await viewer.sheetInfo();
    if (token !== navToken) return;
    const targetIdx = sheets.findIndex((sh) => sh.name === m.sheet);
    if (targetIdx < 0) return; // sheet vanished (doc switch mid-flight)

    if (targetIdx !== activeSheet) {
      // Arm the settle FIRST (mode decides the event), skipping the
      // synchronous sticky replay, then select. Payload must name the TARGET:
      // an intermediate emission for another sheet keeps waiting.
      const settled = new Promise<void>((resolve) => {
        let replaying = true;
        const want = targetIdx;
        // `un` is pre-declared: the sticky replay delivers SYNCHRONOUSLY inside
        // subscribe(), i.e. before the const-assignment form would have bound
        // it — a handler reaching the unsubscribe in that window must not TDZ-
        // throw (the bus's log-and-continue would silently swallow it, which is
        // exactly how this bug class hides).
        let un: () => void = () => {};
        const fire = (): void => {
          if (replaying) return;
          un();
          pendingSettleUnsub = null;
          resolve();
        };
        un =
          gridMode === "grid"
            ? ctx.bus.subscribe("grid:resetup", (e) => {
                if (e.sheet === want) fire();
              })
            : ctx.bus.subscribe("sheets:changed", (e) => {
                if (e.active === want) fire();
              });
        replaying = false;
        pendingSettleUnsub = un;
      });
      void ctx.commands.exec("sheets.select", targetIdx);
      await settled;
      if (token !== navToken) return;
    }

    if (gridMode === "grid") {
      if (m.cellRef !== null) {
        ctx.bus.emit("search:navigate-resolved", {
          target: {
            kind: "grid",
            sheet: targetIdx,
            cellRef: m.cellRef,
            gridX: m.gridX,
            gridY: m.gridY,
          },
        });
      }
      return;
    }

    // Pages mode: resolve the deferred rect; page origin on null.
    let target = { kind: "page" as const, page: m.page, x: 0, y: 0 };
    if (m.cellRef !== null) {
      const rect = await viewer.resolveSearchRect(targetIdx, m.cellRef);
      if (token !== navToken) return;
      if (rect) target = { kind: "page", page: rect.page, x: rect.x, y: rect.y };
    }
    ctx.bus.emit("search:navigate-resolved", { target });
  }
}
