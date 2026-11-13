/**
 * leak-harness.ts — TEST-ONLY per-instance teardown instrumentation.
 *
 * The snippet vitest env is `node` (no DOM, no rAF, no ResizeObserver /
 * IntersectionObserver / FontFace). The no-leak gate is written against
 * PER-INSTANCE deltas (the teardown contract) rather than global page state, so
 * this module supplies hand-built COUNTING doubles keyed to a single instance:
 *
 *   - installGlobals(): swaps globalThis rAF/cAF for a controllable fake
 *     scheduler (queue + flush + pending count) and IntersectionObserver /
 *     ResizeObserver for counting doubles (a module-level live count that
 *     increments on construct and decrements on disconnect). Returns restore().
 *   - countingTarget(): an EventTarget-shaped fake whose net add−remove count is
 *     keyed by (type, capture) — the capture flag is part of the key because the
 *     context-menu adapter registers with {capture:true}; a balanced add/remove
 *     pair must not read as a leak. `.dispatch()` fires the live listeners so the
 *     pre-destroy exercise proves the interaction still works.
 *   - countingScope(): a RootScope-shaped seam whose win/doc are countingTargets
 *     carrying net-pending setTimeout/clearTimeout counters — the per-instance
 *     window/document the gate measures against.
 *   - fakeKernel(): counting bus/paint subscriptions (liveSubs) + emit helpers.
 *   - fakeViewer(): async reads that record calls (and throw once `destroyed`,
 *     mirroring the engine's #assertAlive) so a post-destroy rAF firing into the
 *     viewer is OBSERVABLE.
 *
 * NONE of this ships — it is imported only by *.test.ts.
 */
import { vi } from "vitest";
import type { RootScope } from "../root-scope";

// ── rAF + observer globals ────────────────────────────────────────────────────

let liveObserverCount = 0;
/** Net live counting-observer count (construct − disconnect) for the process. */
export function liveObservers(): number {
  return liveObserverCount;
}

interface Scheduled {
  id: number;
  cb: FrameRequestCallback;
  cancelled: boolean;
  ran: boolean;
}

export interface FakeRaf {
  /** Scheduled-but-not-yet-run, uncancelled callbacks. */
  pending(): number;
  /** Total callbacks ever scheduled (proves the live path scheduled one). */
  scheduled(): number;
  /** Run every pending (uncancelled) callback exactly once. */
  flush(): void;
}

export interface InstalledGlobals {
  raf: FakeRaf;
  liveObservers(): number;
  restore(): void;
}

/**
 * Swap the process-shared rAF/observer globals for counting doubles. beforeEach
 * install / afterEach restore(): the globals are process-shared, so a leaked
 * restore cross-contaminates sibling tests.
 */
export function installGlobals(): InstalledGlobals {
  const g = globalThis as unknown as Record<string, unknown>;
  const saved = {
    raf: g.requestAnimationFrame,
    caf: g.cancelAnimationFrame,
    io: g.IntersectionObserver,
    ro: g.ResizeObserver,
  };
  const queue: Scheduled[] = [];
  let seq = 1;
  liveObserverCount = 0;

  g.requestAnimationFrame = (cb: FrameRequestCallback): number => {
    const id = seq++;
    queue.push({ id, cb, cancelled: false, ran: false });
    return id;
  };
  g.cancelAnimationFrame = (id: number): void => {
    const s = queue.find((q) => q.id === id);
    if (s) s.cancelled = true;
  };

  class CountingObserver {
    constructor() {
      liveObserverCount++;
    }
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {
      liveObserverCount--;
    }
    takeRecords(): unknown[] {
      return [];
    }
  }
  g.IntersectionObserver = CountingObserver as unknown;
  g.ResizeObserver = CountingObserver as unknown;

  const raf: FakeRaf = {
    pending: () => queue.filter((q) => !q.cancelled && !q.ran).length,
    scheduled: () => queue.length,
    flush: () => {
      for (const s of queue) {
        if (s.cancelled || s.ran) continue;
        s.ran = true;
        s.cb(0);
      }
    },
  };

  return {
    raf,
    liveObservers,
    restore: () => {
      g.requestAnimationFrame = saved.raf;
      g.cancelAnimationFrame = saved.caf;
      g.IntersectionObserver = saved.io;
      g.ResizeObserver = saved.ro;
    },
  };
}

// ── counting EventTarget ──────────────────────────────────────────────────────

type Listener = ((ev: unknown) => void) | { handleEvent(ev: unknown): void };

function captureOf(opts: unknown): boolean {
  if (typeof opts === "boolean") return opts;
  return !!(opts && (opts as { capture?: boolean }).capture);
}

export interface CountingTarget {
  addEventListener(type: string, cb: Listener, opts?: unknown): void;
  removeEventListener(type: string, cb: Listener, opts?: unknown): void;
  /** Fire every live listener for `type` (both capture + bubble registrations). */
  dispatch(type: string, ev?: unknown): void;
  /** Net live listener count across all (type, capture) keys. */
  live(): number;
  /** Net live listener count for a single type. */
  liveFor(type: string): number;
}

function makeCountingTarget(): CountingTarget {
  const map = new Map<string, Set<Listener>>();
  const key = (t: string, c: boolean): string => `${t}|${c ? 1 : 0}`;
  const fire = (cb: Listener, ev: unknown): void => {
    if (typeof cb === "function") cb(ev);
    else cb.handleEvent(ev);
  };
  return {
    addEventListener(type, cb, opts) {
      const k = key(type, captureOf(opts));
      let s = map.get(k);
      if (!s) {
        s = new Set();
        map.set(k, s);
      }
      s.add(cb);
    },
    removeEventListener(type, cb, opts) {
      map.get(key(type, captureOf(opts)))?.delete(cb);
    },
    dispatch(type, ev) {
      const event = ev ?? { type };
      for (const cap of [true, false]) {
        const s = map.get(key(type, cap));
        if (s) for (const cb of [...s]) fire(cb, event);
      }
    },
    live() {
      let n = 0;
      for (const s of map.values()) n += s.size;
      return n;
    },
    liveFor(type) {
      return (map.get(key(type, false))?.size ?? 0) + (map.get(key(type, true))?.size ?? 0);
    },
  };
}

// ── counting element ──────────────────────────────────────────────────────────

const RECT: DOMRect = {
  x: 0,
  y: 0,
  left: 0,
  top: 0,
  right: 100,
  bottom: 100,
  width: 100,
  height: 100,
  toJSON: () => ({}),
};

export interface CountingElement extends CountingTarget {
  style: Record<string, string>;
  dataset: Record<string, string>;
  classList: { add(): void; remove(): void; contains(): boolean; toggle(): void };
  className: string;
  textContent: string;
  innerHTML: string;
  isConnected: boolean;
  scrollLeft: number;
  scrollTop: number;
  clientWidth: number;
  clientHeight: number;
  getBoundingClientRect(): DOMRect;
  scrollTo(x?: number, y?: number): void;
  appendChild<T>(c: T): T;
  removeChild<T>(c: T): T;
  remove(): void;
  querySelector(): null;
  querySelectorAll(): never[];
  setPointerCapture(): void;
  releasePointerCapture(): void;
}

/** A DOM-element-shaped counting fake — enough surface for the adapters. */
export function countingElement(): CountingElement {
  const base = makeCountingTarget();
  const el: CountingElement = {
    ...base,
    style: {},
    dataset: {},
    classList: { add() {}, remove() {}, contains: () => false, toggle() {} },
    className: "",
    textContent: "",
    innerHTML: "",
    isConnected: true,
    scrollLeft: 0,
    scrollTop: 0,
    clientWidth: 100,
    clientHeight: 100,
    getBoundingClientRect: () => RECT,
    scrollTo() {},
    appendChild: (c) => c,
    removeChild: (c) => c,
    remove() {},
    querySelector: () => null,
    querySelectorAll: () => [],
    setPointerCapture() {},
    releasePointerCapture() {},
  };
  return el;
}

// ── structural element fake (tree + querySelector) ───────────────────────────

/**
 * Element fake rich enough for the viewport adapter's placeholder + paintWindow
 * COMMIT path — a real child list, insertBefore/firstChild, and the canvas-class
 * querySelector the adapter uses to find its `ps-base` / `ps-slice` layers.
 * Distinct from {@link countingElement}, which counts listener balance for the
 * teardown gate but has an inert (always-null) DOM.
 *
 * getContext() returns null on purpose: every raster call in the adapter is
 * null-guarded, so a paint commit is observable STRUCTURALLY (the canvas node
 * appearing under the `.page`) without a 2D context.
 */
export interface StructuralEl {
  tagName: string;
  className: string;
  style: Record<string, string>;
  dataset: Record<string, string>;
  isConnected: boolean;
  parentElement: StructuralEl | null;
  children: StructuralEl[];
  width: number;
  height: number;
  clientWidth: number;
  clientHeight: number;
  readonly firstChild: StructuralEl | null;
  appendChild(c: StructuralEl): StructuralEl;
  insertBefore(c: StructuralEl, ref: StructuralEl | null): StructuralEl;
  removeChild(c: StructuralEl): StructuralEl;
  remove(): void;
  querySelector(sel: string): StructuralEl | null;
  querySelectorAll(sel: string): StructuralEl[];
  getBoundingClientRect(): DOMRect;
  getContext(): null;
  addEventListener(type: string, cb: (ev: unknown) => void): void;
  removeEventListener(type: string, cb: (ev: unknown) => void): void;
  /** Fire the live listeners for `type` (e.g. the adapter's scroll handler). */
  dispatch(type: string): void;
}

function domRect(left: number, top: number, width: number, height: number): DOMRect {
  return {
    x: left,
    y: top,
    left,
    top,
    width,
    height,
    right: left + width,
    bottom: top + height,
    toJSON: () => ({}),
  } as DOMRect;
}

/** A US-Letter-at-100% structural element fake. See {@link StructuralEl}. */
export function structuralElement(tag = "div"): StructuralEl {
  const children: StructuralEl[] = [];
  const listeners = new Map<string, Set<(ev: unknown) => void>>();
  const el: StructuralEl = {
    tagName: tag.toUpperCase(),
    className: "",
    style: {},
    dataset: {},
    isConnected: true,
    parentElement: null,
    children,
    width: 0,
    height: 0,
    clientWidth: 816,
    clientHeight: 1056,
    get firstChild() {
      return children[0] ?? null;
    },
    appendChild(c) {
      children.push(c);
      c.parentElement = el;
      return c;
    },
    insertBefore(c, ref) {
      const i = ref ? children.indexOf(ref) : -1;
      if (i >= 0) children.splice(i, 0, c);
      else children.push(c);
      c.parentElement = el;
      return c;
    },
    removeChild(c) {
      const i = children.indexOf(c);
      if (i >= 0) children.splice(i, 1);
      c.parentElement = null;
      return c;
    },
    remove() {
      el.parentElement?.removeChild(el);
    },
    // The adapter only queries "canvas.ps-base" / "canvas.ps-slice" (and the
    // scroll-anchor path queries `.page[data-page="N"]`, unused here).
    querySelector(sel) {
      const cls = sel.startsWith("canvas.") ? sel.slice("canvas.".length) : null;
      if (!cls) return null;
      return children.find((c) => c.className === cls) ?? null;
    },
    querySelectorAll() {
      return [];
    },
    getBoundingClientRect: () => domRect(0, 0, 816, 1056),
    getContext: () => null,
    addEventListener(type, cb) {
      let s = listeners.get(type);
      if (!s) {
        s = new Set();
        listeners.set(type, s);
      }
      s.add(cb);
    },
    removeEventListener(type, cb) {
      listeners.get(type)?.delete(cb);
    },
    dispatch(type) {
      for (const cb of [...(listeners.get(type) ?? [])]) cb({ type });
    },
  };
  return el;
}

/**
 * Replace {@link installGlobals}'s inert IntersectionObserver with one the test
 * can DRIVE: the adapter's visibleSet (which onScroll/onResize/zoomPaint iterate,
 * and which the base-layer bound treats as unevictable) is only ever populated
 * from onIntersect, so without delivering entries every launcher is vacuously
 * empty. Install AFTER installGlobals() and BEFORE the adapter is constructed;
 * g.restore() still puts the real global back (it saved the original).
 */
export function installDrivableObserver(): {
  intersect(els: StructuralEl[], isIntersecting?: boolean): void;
} {
  const cbs: Array<(entries: unknown[]) => void> = [];
  class DrivableObserver {
    constructor(cb: (entries: unknown[]) => void) {
      cbs.push(cb);
    }
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
    takeRecords(): unknown[] {
      return [];
    }
  }
  (globalThis as unknown as Record<string, unknown>).IntersectionObserver = DrivableObserver;
  return {
    intersect(els, isIntersecting = true) {
      const entries = els.map((target) => ({ target, isIntersecting }));
      for (const cb of cbs) cb(entries);
    },
  };
}

// ── viewport-adapter driving ─────────────────────────────────────────────────

/** Drain microtasks + one macrotask boundary so suspended awaits resume. */
export const settleTasks = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

/**
 * Drive DomViewportAdapter to `n` `.page` placeholders: emit one 'page' intent
 * per index and flush the rAF-coalesced drains around the ASYNC page-size prime
 * (the first drain finds a cold size cache and only kicks primePageSize; the
 * placeholders appear on the re-drain it schedules). Returns the placeholders in
 * container order — the caller asserts the count.
 */
export async function primeViewportPages(
  kernel: FakeKernel,
  raf: FakeRaf,
  pagesContainer: StructuralEl,
  n: number,
): Promise<StructuralEl[]> {
  for (let i = 0; i < n; i++) kernel.emitPaint({ tag: "page", pageIndex: i });
  raf.flush(); // drain 1: cold cache → prime
  await settleTasks(); // pageSize resolves → scheduleFlush re-adds
  raf.flush(); // drain 2: placeholders created
  await settleTasks();
  return pagesContainer.children.filter((c) => c.className === "page");
}

// ── counting scope ────────────────────────────────────────────────────────────

interface TimerHost extends CountingTarget {
  devicePixelRatio: number;
  visibilityState: string;
  setTimeout(fn: () => void, ms?: number): number;
  clearTimeout(id: number): void;
  getSelection(): null;
  pendingTimers(): number;
}

function makeTimerHost(): TimerHost {
  const base = makeCountingTarget();
  let seq = 1;
  const timers = new Map<number, () => void>();
  return {
    ...base,
    devicePixelRatio: 1,
    visibilityState: "visible",
    setTimeout(_fn, _ms) {
      const id = seq++;
      timers.set(id, _fn);
      return id;
    },
    clearTimeout(id) {
      timers.delete(id);
    },
    getSelection: () => null,
    pendingTimers: () => timers.size,
  };
}

export interface CountingScope extends RootScope {
  win: RootScope["win"] & CountingTarget & { pendingTimers(): number };
  doc: RootScope["doc"] & CountingTarget & { pendingTimers(): number };
  /** Combined net-pending timers across win + doc. */
  pendingTimers(): number;
}

/** A RootScope whose win/doc are per-instance counting seams. */
export function countingScope(overrides: Partial<RootScope> = {}): CountingScope {
  const win = makeTimerHost();
  const doc = Object.assign(makeTimerHost(), {
    createElement: () => countingElement(),
    createTextNode: (s: string) => ({ textContent: s }),
  });
  const host = countingElement();
  const scope = {
    host,
    rootNode: doc,
    doc,
    win,
    instanceId: "leak-test",
    dpr: () => win.devicePixelRatio || 1,
    elementFromPoint: () => null,
    activeElement: () => null,
    getSelection: () => null,
    createRange: () => ({}) as Range,
    viewportRect: () => RECT,
    isElement: (x: unknown): x is Element => !!x,
    isHTMLElement: (x: unknown): x is HTMLElement => !!x,
    isCanvas: (x: unknown): x is HTMLCanvasElement => !!x,
    pendingTimers: () => win.pendingTimers() + doc.pendingTimers(),
    ...overrides,
  };
  return scope as unknown as CountingScope;
}

// ── fake kernel ───────────────────────────────────────────────────────────────

export interface FakeKernel {
  exec: ReturnType<typeof vi.fn>;
  bus: {
    subscribe(event: string, cb: (payload: unknown) => void): () => void;
    clearReplay(event: string): void;
    reset(): void;
  };
  paint: { onPaintIntent(cb: (intent: unknown) => void): () => void };
  commands: unknown;
  /** Net live subscriptions (subscribe/onPaintIntent − their unsub). */
  liveSubs(): number;
  /** Drive a paint intent into the adapter's onPaintIntent handlers. */
  emitPaint(intent: unknown): void;
  /** Drive a bus event into the adapter's subscribe handlers. */
  emitBus(event: string, payload: unknown): void;
}

export function fakeKernel(): FakeKernel {
  let subs = 0;
  const busHandlers = new Map<string, Set<(p: unknown) => void>>();
  const paintHandlers = new Set<(i: unknown) => void>();
  // Counts the acquisition; the returned unsub releases exactly once (a
  // double-unsub is absorbed — mirrors the real Set.delete-based unsub, so the
  // net stays honest under an over-eager double destroy).
  const countingUnsub = (release: () => void): (() => void) => {
    subs++;
    let done = false;
    return () => {
      if (done) return;
      done = true;
      subs--;
      release();
    };
  };
  return {
    exec: vi.fn(async () => undefined),
    bus: {
      subscribe(event, cb) {
        let s = busHandlers.get(event);
        if (!s) {
          s = new Set();
          busHandlers.set(event, s);
        }
        s.add(cb);
        return countingUnsub(() => s!.delete(cb));
      },
      clearReplay() {},
      reset() {},
    },
    paint: {
      onPaintIntent(cb) {
        paintHandlers.add(cb);
        return countingUnsub(() => paintHandlers.delete(cb));
      },
    },
    commands: {},
    liveSubs: () => subs,
    emitPaint: (intent) => {
      for (const h of [...paintHandlers]) h(intent);
    },
    emitBus: (event, payload) => {
      for (const h of [...(busHandlers.get(event) ?? [])]) h(payload);
    },
  };
}

// ── fake viewer ───────────────────────────────────────────────────────────────

export interface FakeViewer {
  destroyed: boolean;
  pageSize: ReturnType<typeof vi.fn>;
  renderPage: ReturnType<typeof vi.fn>;
  renderPageWindow: ReturnType<typeof vi.fn>;
  paintGridViewport: ReturnType<typeof vi.fn>;
  gridPaneInfo: ReturnType<typeof vi.fn>;
  gridExtent: ReturnType<typeof vi.fn>;
  /** Total post-`destroyed` calls into any read — the leak signal. */
  callsWhileDestroyed(): number;
}

/**
 * A Viewer stand-in whose reads record a call and THROW once `destroyed` is set
 * (mirrors the core Viewer's #assertAlive guard), so a rAF that fires into
 * the viewer AFTER teardown is observable as either a recorded call or a throw.
 */
export function fakeViewer(): FakeViewer {
  let deadCalls = 0;
  const v = {
    destroyed: false,
    callsWhileDestroyed: () => deadCalls,
  } as FakeViewer;
  const read = (result: unknown): ReturnType<typeof vi.fn> =>
    vi.fn(async () => {
      if (v.destroyed) {
        deadCalls++;
        throw new Error("Viewer was destroyed");
      }
      return result;
    });
  v.pageSize = read({ widthPx: 816, heightPx: 1056 });
  v.renderPage = read(undefined);
  v.renderPageWindow = read(true);
  v.paintGridViewport = read(undefined);
  v.gridPaneInfo = read(null);
  v.gridExtent = read({ cols: 10, rows: 10, width: 1000, height: 1000 });
  return v;
}
