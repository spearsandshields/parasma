/**
 * plugin-cursor — the SINGLE OWNER of the page-canvas cursor.
 *
 * A DOM-free, store-less PURE-DECISION plugin: it holds no slice (no mutable
 * UI state of its own) and never touches the DOM. It exposes ONE query command,
 * `cursor.resolve({page,x,y}) => string`, that maps a CONTENT coordinate to a
 * CSS cursor keyword by PRIORITY (first hit wins). The CONSUMER (e.g. @parasma/snippet)
 * owns the single `pointermove` applier: it maps the pointer event → page +
 * content-coord (dividing by the zoom scale — see plugin-zoom), calls
 * cursor.resolve, and writes `canvas.style.cursor`. Plugin decides; consumer
 * applies. This replaces the scattered `canvas.style.cursor =` writes that used
 * to fight over the canvas (no split-brain — one decision point).
 *
 * PRIORITY (first hit wins, content coords already unscaled by the caller):
 *   0. hand mode   — tool mode === "hand"                    → "grab"
 *   1. hyperlink   — viewer.linkRects(page) contains (x,y)   → "pointer"
 *   2. comment     — viewer.commentRects(page) contains (x,y) → "pointer"
 *   3. excel cell  — documentKind === "excel" AND cellAt hit  → "cell"
 *   4. else                                                    → "" (DOM default)
 *
 * Rule 0 (hand mode): when the tool mode is "hand" the whole page
 * cursor is grab — it OVERRIDES every per-hit affordance below. The mode is
 * cached off the `tool:mode-changed` bus signal (default "select" until
 * plugin-tool first emits). The transient "grabbing" during an
 * active pan is the DomToolAdapter's DOM concern (the plugin has no in-flight-drag
 * notion); this plugin only owns the grab/normal DECISION.
 *
 * SCOPE — cursors meaningful TODAY only. The TEXT cursor (I-beam) is DEFERRED
 * (pending plugin-selection): resolve() has no selectable-text hit-test, and an
 * I-beam over non-selectable text is a FALSE affordance. The seam is marked in
 * resolve() (before the default return) but ships nothing — "text" never appears.
 *
 * documentKind is read from plugin-loader, not a module global: initialize()
 * subscribes the replayable `loader:loaded` bus event (DocumentInfo, "latest"
 * replay so a late boot still catches the current document's kind) and caches
 * `documentKind` in a closure var, so the cell-branch kind check stays a
 * synchronous closure read (no cross-plugin round-trip).
 *
 * resolve() itself is ASYNC: the three Viewer hit-tests (linkRects,
 * commentRects, cellAt) are worker-ready async seams. They are awaited INTERNALLY
 * so cursor.resolve remains ONE command (one kernel.exec round-trip) — the
 * consumer's pointermove applier `await`s a single resolve, not three reads.
 *
 * deps: ["loader"] — two-phase boot guarantees loader.initialize() (which emits
 * loader:loaded) runs before cursor.initialize().
 */

import type { DocumentInfo, DocumentKind } from "@parasma/core";
import type { Plugin } from "../registry";
import type { KernelContext } from "../kernel-context";
import type { CursorViewer } from "./ports";

/**
 * Commands OWNED by this plugin. Declaration-merged into the open
 * {@link CommandMap} (targeting `../commands`, the barrel-re-exported kernel
 * module → program-wide) so `exec(id, …)` is arg/return type-checked and an
 * undeclared id fails to register. Signatures authored from the handler bodies.
 */
declare module "../commands" {
  interface CommandMap {
    "cursor.resolve": (q: CursorQuery) => Promise<string>;
  }
}

/** Content-coordinate query for `cursor.resolve` (layout px, already unscaled). */
export interface CursorQuery {
  /** 0-based page index. */
  page: number;
  /** X in layout px within the page (caller divides client offset by zoom scale). */
  x: number;
  /** Y in layout px within the page. */
  y: number;
}

/** Inclusive-bounds hit-test (both edges count as inside). */
function contains(r: { x: number; y: number; w: number; h: number }, x: number, y: number): boolean {
  return x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h;
}

/**
 * The cursor plugin. Construct with the SAME Viewer the consumer renders into
 * (mirrors viewportPlugin(viewer) / loaderPlugin(viewer)) so it can read the
 * live `viewer.linkRects/commentRects/cellAt` hit-tests on demand.
 */
export function cursorPlugin(viewer: CursorViewer): Plugin {
  const unsubs: Array<() => void> = [];
  // Cached document kind, sourced from plugin-loader's `loader:loaded` event
  // (replay-safe). null until the first document loads. Only the cell branch
  // needs it — linkRects/commentRects already guard kind internally in core.
  let documentKind: DocumentKind | null = null;
  // Effective tool mode, cached off `tool:mode-changed` (default "select"
  // until plugin-tool first emits). Hand mode overrides the
  // per-hit cursor with "grab". Read synchronously in resolve().
  let toolMode: "select" | "hand" = "select";

  // ── Per-page rect cache, keyed by a LOAD GENERATION ─────────────────────────
  // linkRects/commentRects return LAYOUT-coordinate rects (CursorQuery x/y are
  // already-unscaled layout px), so a page's rects are STABLE across zoom and
  // change ONLY on a re-layout. During a hover the pointer sweeps one laid-out
  // page whose rects do not move, yet each raw resolve re-issued a fresh read
  // (an RPC round-trip in worker mode). We cache the per-page result on the first
  // hover and reuse it for every later hover on that page in the SAME generation
  // — so after the first hover a worker-mode hover costs ZERO RPC. The cache is
  // invalidated (generation bumped, map cleared) on the load/layout bus signals
  // wired in initialize(). cellAt stays UNcached — it is position-dependent
  // (a different (x,y) is a genuinely different query) and excel-only.
  //
  // DECISION-UNCHANGED: on a miss we store exactly the array the Viewer returns;
  // on a hit we return that same array; contains() is unchanged. So for any
  // (page,x,y) the same rects yield the same `.some(contains)` → the SAME cursor
  // keyword in in-process, worker, and pre-cache. This is a pure read-through.
  function pageRectCache<T>(fetch: (p: number) => Promise<T[]>) {
    let gen = 0;
    const byPage = new Map<number, T[]>();
    return {
      async get(p: number): Promise<T[]> {
        const hit = byPage.get(p);
        if (hit) return hit;
        const g = gen;
        const r = await fetch(p);
        // STALE-GUARD: if an invalidation bumped the generation while this fetch
        // was in flight, DROP the result — NEVER populate a new generation with
        // prior-layout rects. This in-flight call still returns `r` (harmless —
        // the keep-latest hover flush re-resolves the final position fresh), but
        // the next get() re-fetches under the current generation.
        if (g === gen) byPage.set(p, r);
        return r;
      },
      invalidate(): void {
        gen++;
        byPage.clear();
      },
    };
  }
  const linkCache = pageRectCache((p: number) => viewer.linkRects(p));
  const commentCache = pageRectCache((p: number) => viewer.commentRects(p));
  const invalidateCaches = (): void => {
    linkCache.invalidate();
    commentCache.invalidate();
  };

  return {
    id: "cursor",
    deps: ["loader"],

    initialize(ctx: KernelContext) {
      // Cache documentKind off the replayable bus so resolve() is a synchronous
      // pure read. "latest" replay catches a late subscriber up to the current
      // document's kind; a reload re-emits with the new kind.
      unsubs.push(
        ctx.bus.subscribe("loader:loaded", (info) => {
          documentKind = (info as DocumentInfo).documentKind;
        }),
        // Cache the tool mode for the hand-mode grab override. "latest"
        // replay catches a late boot up to the current mode.
        ctx.bus.subscribe("tool:mode-changed", (e) => {
          toolMode = (e as { mode: "select" | "hand" }).mode;
        }),
        // A reset to a non-ready status clears the cached kind (parity with a
        // doc switch / viewer.reset) so a stale "excel" never leaks a cell
        // cursor into the next document before its loader:loaded lands. Also drop
        // hand mode (belt — plugin-tool re-emits "select" on the same reset).
        ctx.bus.subscribe("loader:status-changed", (s) => {
          if (s === "idle" || s === "planning" || s === "loading") {
            documentKind = null;
            toolMode = "select";
            // BELT: covers viewer.reset paths that transition status without a
            // distinct load-started/loadComplete pair.
            invalidateCaches();
          }
        }),
        // Rect-cache invalidation (the crux). Bump the generation + clear on:
        //  (1) loader:load-started — emitted synchronously at every open() BEFORE
        //      the new layout, so no prior-doc rect is served during the loading
        //      window (early clear on a doc switch).
        //  (2) loadComplete — the AUTHORITATIVE post-layout clear. It fires ONLY
        //      at layout completion, and the in-place refreshFonts re-layout
        //      RE-EMITS its own fresh loadComplete — so this catches BOTH a doc
        //      switch AND the refreshFonts re-layout that moves rects without a
        //      new load-started. (fonts:refreshed is deliberately NOT used: it is
        //      emitted BEFORE the fresh layout's first pageReady, so invalidating
        //      there would re-cache STALE rects under the new generation.)
        ctx.bus.subscribe("loader:load-started", () => invalidateCaches()),
        ctx.bus.subscribe("loadComplete", () => invalidateCaches()),
      );

      // The single PURE decision. The three Viewer hit-test reads are async
      // seams (in worker mode the RPC transport goes UNDER each read, leaving
      // this command shape unchanged). They are
      // awaited INTERNALLY here so cursor.resolve stays ONE command (one
      // kernel.exec round-trip), not three: a single async command, not a fan-out.
      unsubs.push(
        ctx.commands.register("cursor.resolve", async (q: CursorQuery): Promise<string> => {
        const { page, x, y } = q;

        // 0. Hand mode → grab, overriding every per-hit affordance below.
        if (toolMode === "hand") return "grab";

        // 1. Hyperlink → pointer. (linkRects guards kind internally.) Read via the
        //    per-generation cache — first hover fetches, later hovers reuse.
        if ((await linkCache.get(page)).some((r) => contains(r, x, y))) return "pointer";

        // 2. Comment range → pointer. (commentRects returns [] for non-word.)
        if ((await commentCache.get(page)).some((r) => contains(r, x, y))) return "pointer";

        // 3. Excel cell → cell. Needs documentKind (cellAt is the docx-only-build
        // / non-spreadsheet no-op otherwise, but the kind guard keeps it cheap).
        if (documentKind === "excel" && (await viewer.cellAt(page, x, y)) != null) return "cell";

        // SEAM (plugin-selection): when a selectable-text hover
        // hit-test is added, it goes HERE → return "text". Deferred —
        // an I-beam over non-selectable text is a false affordance, so we fall
        // through to the default. (No viewer.textAt/hasText call exists yet.)

        // 4. Default — "" = the DOM default cursor (the OS arrow), never an I-beam.
        return "";
      }),
      );
    },

    destroy(_ctx: KernelContext) {
      for (const u of unsubs.splice(0)) u();
      documentKind = null;
      // Drop any cached rects so a re-attached plugin never serves prior-doc rects.
      invalidateCaches();
    },
  };
}
