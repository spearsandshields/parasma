/**
 * Replayable event bus.
 *
 * The Viewer's own `on()` replays catch-up history so a subscriber attaching
 * mid-stream still sees every `pageReady` index and a sticky `loadComplete`
 * (see parasma-core/src/index.ts `on()`). A naive emitter would lose those
 * events for late subscribers and REGRESS streaming layout. This bus mirrors
 * that contract for the kernel's own lifecycle events.
 *
 * Per-event replay POLICY:
 *  - "accumulate": retain ALL payloads; replay each in order (e.g. pageReady —
 *    matches the per-index catch-up loop in the Viewer).
 *  - "latest": retain only the last payload; replay it once (e.g. loadComplete,
 *    loader:loaded, loader:status-changed — sticky lifecycle state).
 *  - "none": retain nothing; transient (e.g. loader:error).
 */

/**
 * The kernel's OWN lifecycle events. Plugins do NOT edit this map; they
 * AUGMENT it via `declare module "../bus" { interface BusEventMap { … } }`
 * (declaration-merging), co-located with the plugin that emits the event —
 * so adding a plugin event no longer couples the kernel to that plugin.
 * emit/subscribe/clearReplay are already keyof-generic over this open
 * interface and need no change. (The interface is intentionally NOT closed.)
 */
export interface BusEventMap {
  // Viewer-bridged page lifecycle (the loader plugin pumps these in).
  pageReady: number;
  loadComplete: number;
  /** Layout stopped early at the memory guardrail; payload = pages kept ("showing first N
   *  pages"). Replay "latest": a late subscriber (BYO chrome mounting after a
   *  fast truncated load) still learns the view is partial. */
  layoutTruncated: number;
  /**
   * An image finished decoding (payload = relId), bridged from the Viewer by the
   * loader plugin. plugin-viewport consumes it through the single
   * invalidate-visible channel (same as `fonts:variant-arrived`) so slow-decoding
   * images repaint the visible pages without waiting for a zoom/scroll. Transient
   * ("none"): a late subscriber must not replay it — the page was already painted
   * with the decoded image present.
   */
  imageReady: string;
  // Loader plugin lifecycle.
  "loader:status-changed": string;
  "loader:loaded": unknown;
  "loader:error": unknown;
  /**
   * Emitted ONCE per `loader.open()` with a per-open monotonic id, REGARDLESS of
   * prior machine status. Unlike `loader:status-changed`, this has no status-edge
   * dedup: the loader machine self-transitions loading→loading on a re-entrant
   * LOAD, which the `s !== prevStatus` mirror suppresses — so a 2nd open() while
   * loading would NOT re-emit `loader:status-changed`. plugin-fonts arms a fresh
   * per-document readiness off THIS event so every open() re-plans exactly once.
   */
  "loader:load-started": number;
}

export type ReplayPolicy = "accumulate" | "latest" | "none";

/** Per-event replay policy. Unlisted events default to "none" (transient). */
const REPLAY_POLICY: Partial<Record<keyof BusEventMap, ReplayPolicy>> = {
  pageReady: "accumulate",
  loadComplete: "latest",
  layoutTruncated: "latest",
  imageReady: "none",
  "loader:loaded": "latest",
  "loader:status-changed": "latest",
  "loader:error": "none",
  // Sticky: a late subscriber catches the CURRENT arm (so it can wire readiness
  // for the in-flight document rather than missing the open it arrived after).
  "loader:load-started": "latest",
  // Fonts: sticky status + terminal ready (a late subscriber catches up to the
  // current font state). Per-family arrivals are transient invalidate signals —
  // a late subscriber must not replay them (the page was already (re)painted).
  "fonts:status-changed": "latest",
  "fonts:ready": "latest",
  "fonts:variant-arrived": "none",
  "zoom:changed": "latest",
  // Sticky: a late-mounted page nav (toolbar or BYO chrome) catches the CURRENT
  // page. Cleared by the viewport.reset handler on doc switch.
  "viewport:page-changed": "latest",
  // Transient: an imperative scroll request consumed once (search:navigate
  // mirror). A late subscriber must not replay a stale navigation.
  "viewport:scroll-to-page": "none",
  // Sticky: a late-attached tab renderer (or plugin-grid) catches
  // the CURRENT sheet set + active index without missing the build.
  "sheets:changed": "latest",
  // Sticky: a late-attached grid adapter catches the CURRENT mode + the sheet it
  // must (re-)setup for, so it shows the right surface without missing the edge.
  "grid:mode-changed": "latest",
  "grid:resetup": "latest",
  // Sticky: a late-attached comment-panel renderer catches the CURRENT comment
  // set + focused id (so the present-only N=0 hide and the cards reflect the
  // active document without missing the build).
  "comments:changed": "latest",
  // Sticky: a late-attached search box / match counter catches the CURRENT query,
  // result set, active index, and loop status without missing the streaming build.
  "search:changed": "latest",
  // Transient: a scroll request consumed once. A late subscriber must not replay a
  // stale navigation (the active match already scrolled into view).
  "search:navigate": "none",
  // Transient for the same reason: a settled reveal consumed once.
  "search:navigate-resolved": "none",
  // Sticky: a late subscriber (the DomSelectionAdapter, plugin-cursor) catches
  // the CURRENT tool mode / selection without missing the edge. Until plugin-tool
  // first emits, the effective mode is the default 'select';
  // selection:changed replays the current selection (incl. a doc-switch 'none').
  "tool:mode-changed": "latest",
  // Transient: an explicit-toggle edge that clears the selection. A late
  // subscriber must NOT retroactively clear (the switch already happened).
  "tool:base-mode-changed": "none",
  "selection:changed": "latest",
  // Transient: a context-menu request (or its dismiss `{items:[]}`) is consumed
  // once; a late subscriber must not replay a stale open (the menu already
  // opened/closed). Same discipline as clipboard:copied/error.
  "selection:menu": "none",
};

function policyFor(event: keyof BusEventMap): ReplayPolicy {
  return REPLAY_POLICY[event] ?? "none";
}

export type BusHandler<E extends keyof BusEventMap> = (payload: BusEventMap[E]) => void;

export interface Bus {
  /** Append to the retained log per policy, then deliver to live subscribers. */
  emit<E extends keyof BusEventMap>(event: E, payload: BusEventMap[E]): void;
  /**
   * Subscribe. If the event is replayable, the retained history is drained into
   * the handler in order BEFORE it joins the live set. Returns an unsubscribe.
   */
  subscribe<E extends keyof BusEventMap>(event: E, handler: BusHandler<E>): () => void;
  /** Drop the retained replay log for one event (e.g. on doc switch). */
  clearReplay<E extends keyof BusEventMap>(event: E): void;
  /** Drop ALL retained logs (parity with viewer.reset() on doc switch). */
  reset(): void;
}

export function createBus(): Bus {
  const handlers = new Map<keyof BusEventMap, Set<BusHandler<keyof BusEventMap>>>();
  // Retained payloads per event (in emission order). For "latest" we keep at
  // most one; for "accumulate" we keep all; "none" never appends.
  const retained = new Map<keyof BusEventMap, unknown[]>();

  function handlersFor<E extends keyof BusEventMap>(event: E): Set<BusHandler<E>> {
    let set = handlers.get(event);
    if (!set) {
      set = new Set();
      handlers.set(event, set);
    }
    return set as Set<BusHandler<E>>;
  }

  function deliver<E extends keyof BusEventMap>(handler: BusHandler<E>, payload: BusEventMap[E]): void {
    // Log-and-continue PER handler, exactly like Viewer.#emit, so one bad
    // subscriber can't break streaming for the rest.
    try {
      handler(payload);
    } catch (e) {
      console.error("[parasma:headless] bus handler threw:", e);
    }
  }

  return {
    emit(event, payload) {
      const policy = policyFor(event);
      if (policy === "accumulate") {
        let log = retained.get(event);
        if (!log) {
          log = [];
          retained.set(event, log);
        }
        log.push(payload);
      } else if (policy === "latest") {
        retained.set(event, [payload]);
      }
      for (const h of handlersFor(event)) deliver(h, payload);
    },

    subscribe(event, handler) {
      const log = retained.get(event);
      if (log) {
        for (const payload of log) deliver(handler, payload as BusEventMap[typeof event]);
      }
      const set = handlersFor(event);
      set.add(handler);
      return () => {
        set.delete(handler);
      };
    },

    clearReplay(event) {
      retained.delete(event);
    },

    reset() {
      retained.clear();
    },
  };
}
