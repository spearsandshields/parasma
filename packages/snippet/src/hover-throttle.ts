/**
 * hover-throttle — rAF-coalesced, one-in-flight, keep-latest scheduler for the
 * per-page pointermove → cursor.resolve hover path (attach-dom).
 *
 * WHY: a raw `pointermove` can fire 100+/sec, and each fired a fresh
 * `cursor.resolve` — in WORKER mode every resolve is a postMessage round-trip
 * (up to three serial reads), so an unthrottled hover sweep piled up dozens of
 * redundant RPCs per second (the rects do not change while hovering one laid-out
 * page). This coalesces the burst to at most ONE resolve per animation frame AND
 * holds a single resolve in flight at a time, mirroring the VERIFIED selection
 * adapter idiom (dom-selection-adapter.ts: latestMove keep-latest + one-in-flight
 * `if (this.moveRaf) return`).
 *
 * A pure rAF gate is INSUFFICIENT: a worker RPC resolve can exceed one frame, so
 * cross-frame resolves would still stack. We combine rAF-coalescing (bursts →
 * frame boundary) with a promise IN-FLIGHT guard (no new resolve while one is
 * pending). The `.finally` TRAILING FLUSH is the keep-latest guarantee: if a
 * newer sample landed mid-resolve, resolve again so the RESTING cursor always
 * reflects the LAST pointer position (it never sticks on a stale sample).
 *
 * DOM-global-free: `requestAnimationFrame`/`cancelAnimationFrame` are INJECTED
 * (the caller passes the realm's own binding, matching the surrounding
 * attach-dom code) so this is unit-testable under the node vitest env with fake
 * rAF + a counting resolve. It only writes `sample.pageEl.style.cursor`.
 */

/** One pointer sample: the page element to write the cursor onto + its content coord. */
export interface HoverSample {
  pageEl: HTMLElement;
  idx: number;
  x: number;
  y: number;
}

export interface HoverThrottle {
  /** Record the latest pointer sample; schedule a coalesced resolve (keep-latest). */
  schedule(sample: HoverSample): void;
  /** Teardown: cancel any pending frame and drop the latest sample (no reschedule). */
  cancel(): void;
}

/**
 * @param resolve  Maps a sample to a CSS cursor keyword (wraps `cursor.resolve`).
 * @param raf      The realm's `requestAnimationFrame` (injected for testability).
 * @param caf      The realm's `cancelAnimationFrame`.
 */
export function createHoverThrottle(
  resolve: (idx: number, x: number, y: number) => Promise<string>,
  raf: (cb: () => void) => number,
  caf: (handle: number) => void,
): HoverThrottle {
  let hoverRaf = 0;
  let pending = false;
  let latest: HoverSample | null = null;

  function flush(): void {
    hoverRaf = 0;
    const s = latest;
    if (!s) return;
    latest = null; // consume — a newer sample arriving now re-arms via `pending`
    pending = true;
    void resolve(s.idx, s.x, s.y)
      .then((c) => {
        s.pageEl.style.cursor = c;
      })
      .finally(() => {
        pending = false;
        // Trailing flush (keep-latest): a sample that arrived while this resolve
        // was in flight is still queued — resolve it so the resting cursor is
        // always the LAST position, never a stale mid-sweep sample.
        if (latest && !hoverRaf) hoverRaf = raf(flush);
      });
  }

  return {
    schedule(sample: HoverSample): void {
      latest = sample; // keep-latest: always overwrite with the newest sample
      // One-per-frame AND one-in-flight: do not arm a second frame while a frame
      // is queued or a resolve is pending — the queued/settling flush picks up
      // the latest sample.
      if (hoverRaf || pending) return;
      hoverRaf = raf(flush);
    },
    cancel(): void {
      if (hoverRaf) caf(hoverRaf);
      hoverRaf = 0;
      latest = null; // prevent any post-teardown reschedule
    },
  };
}
