/**
 * A tiny multi-subscriber fan-out. The DOM binding uses one per
 * cross-cut a chrome layer subscribes to (cell-select → formula bar, comment-focus
 * → comments panel): the core fires INTO it from the hit-test path; the composing
 * chrome subscribes OUT of it, so the exact call chain stays byte-identical whether
 * the demo chrome or a parent's own chrome is on the other end.
 *
 * Extracted so the fan-out wiring is unit-testable without a DOM (the full attachDom
 * path is browser-verified).
 */
export interface Fanout<T> {
  /** Register a sink; returns an unsubscribe. */
  subscribe(cb: (value: T) => void): () => void;
  /** Deliver a value to every current sink. */
  fire(value: T): void;
  /** Drop every sink (teardown). */
  clear(): void;
}

export function createFanout<T>(): Fanout<T> {
  const sinks = new Set<(value: T) => void>();
  return {
    subscribe(cb) {
      sinks.add(cb);
      return () => sinks.delete(cb);
    },
    fire(value) {
      for (const cb of sinks) cb(value);
    },
    clear() {
      sinks.clear();
    },
  };
}
