/**
 * createPageSensor — the ONE DOM current-page tracker in the snippet layer
 * (kernel-truth current page). Node-env tests with injected raf/caf + counting
 * fakes (leak-harness style): the sensor senses the topmost `.page`, execs
 * `viewport.setCurrentPage`, mirrors the kernel echo, and tears down cleanly
 * (a tracker inside the toolbar once fired a rAF after destroy; the dispose
 * gate here pins that).
 */
import { describe, it, expect, vi } from "vitest";
import { createPageSensor, type PageSensorDeps } from "./page-sense";

interface FakePage {
  dataset: { page: string };
  style: { display: string };
  getBoundingClientRect(): { top: number };
  scrollIntoView: ReturnType<typeof vi.fn>;
  rectReads: number;
}

function fakePage(index: number, top: number, hidden = false): FakePage {
  const p: FakePage = {
    dataset: { page: String(index) },
    style: { display: hidden ? "none" : "" },
    rectReads: 0,
    getBoundingClientRect() {
      p.rectReads++;
      return { top };
    },
    scrollIntoView: vi.fn(),
  };
  return p;
}

function makeHarness(pages: FakePage[], opts: { suspended?: () => boolean } = {}) {
  // rAF fake: queue + flush + cancel.
  const queue: Array<{ id: number; cb: FrameRequestCallback; cancelled: boolean }> = [];
  let seq = 1;
  const raf = (cb: FrameRequestCallback): number => {
    const id = seq++;
    queue.push({ id, cb, cancelled: false });
    return id;
  };
  const caf = (h: number): void => {
    const s = queue.find((q) => q.id === h);
    if (s) s.cancelled = true;
  };
  const flush = (): void => {
    const batch = queue.splice(0);
    for (const s of batch) if (!s.cancelled) s.cb(0);
  };

  // Counting scroll-listener target + rect source.
  const scrollListeners = new Set<EventListener>();
  const viewerEl = {
    addEventListener: (_t: string, cb: EventListener) => scrollListeners.add(cb),
    removeEventListener: (_t: string, cb: EventListener) => scrollListeners.delete(cb),
    getBoundingClientRect: () => ({ top: 0 }),
  } as unknown as HTMLElement;

  const pagesContainer = {
    querySelectorAll: (_sel: string) => pages,
    querySelector: (sel: string) => {
      const m = /data-page="(\d+)"/.exec(sel);
      return m ? (pages.find((p) => p.dataset.page === m[1]) ?? null) : null;
    },
  } as unknown as HTMLElement;

  // Counting bus fake: live-sub count + handler dispatch.
  const handlers = new Map<string, Set<(p: unknown) => void>>();
  let liveSubs = 0;
  const subscribe = ((event: string, cb: (p: unknown) => void) => {
    let s = handlers.get(event);
    if (!s) {
      s = new Set();
      handlers.set(event, s);
    }
    s.add(cb);
    liveSubs++;
    let done = false;
    return () => {
      if (done) return;
      done = true;
      liveSubs--;
      s!.delete(cb);
    };
  }) as PageSensorDeps["subscribe"];
  const emitBus = (event: string, payload: unknown): void => {
    for (const h of [...(handlers.get(event) ?? [])]) h(payload);
  };

  const exec = vi.fn();
  const deps: PageSensorDeps = {
    viewerEl,
    pagesContainer,
    exec,
    subscribe,
    suspended: opts.suspended ?? (() => false),
    raf,
    caf,
  };
  const sensor = createPageSensor(deps);
  return {
    sensor,
    exec,
    flush,
    emitBus,
    pending: () => queue.filter((q) => !q.cancelled).length,
    scrollListenerCount: () => scrollListeners.size,
    liveSubs: () => liveSubs,
    fireScroll: () => {
      for (const cb of [...scrollListeners]) cb({ type: "scroll" } as Event);
    },
  };
}

describe("createPageSensor", () => {
  it("senses the topmost page and execs viewport.setCurrentPage with the picked index", () => {
    const h = makeHarness([fakePage(0, -900), fakePage(1, -20), fakePage(2, 800)]);
    h.sensor.schedule();
    h.flush();
    expect(h.exec).toHaveBeenCalledExactlyOnceWith("viewport.setCurrentPage", 1);
  });

  it("coalesces schedule() calls into one rAF sense", () => {
    const h = makeHarness([fakePage(0, -900), fakePage(1, -20)]);
    h.sensor.schedule();
    h.sensor.schedule();
    h.sensor.schedule();
    expect(h.pending()).toBe(1);
    h.flush();
    expect(h.exec).toHaveBeenCalledTimes(1);
  });

  it("a scroll event on the viewer schedules a sense", () => {
    const h = makeHarness([fakePage(0, -900), fakePage(1, -20)]);
    h.fireScroll();
    h.flush();
    expect(h.exec).toHaveBeenCalledExactlyOnceWith("viewport.setCurrentPage", 1);
  });

  it("does NOT exec when the pick equals the mirror (kernel dedup preserved sensor-side)", () => {
    const h = makeHarness([fakePage(0, 10), fakePage(1, 900)]);
    // sensedPage starts at 0 and the topmost page IS 0 → nothing to report.
    h.sensor.schedule();
    h.flush();
    expect(h.exec).not.toHaveBeenCalled();
  });

  it("suspended()=true suppresses sensing entirely (grid-mode phantom-sense pin)", () => {
    // Grid mode hides the paged surface → all-zero rects would elect band 0 and
    // leak a phantom pageChange to BYO consumers. The guard runs FIRST.
    const h = makeHarness([fakePage(0, 0), fakePage(1, 0)], { suspended: () => true });
    h.sensor.schedule();
    h.flush();
    expect(h.exec).not.toHaveBeenCalled();
  });

  it("skips hidden pages BEFORE the rect read and early-breaks past top > 80", () => {
    const hidden = fakePage(1, 5, true);
    const far = fakePage(3, 900);
    const h = makeHarness([fakePage(0, -400), hidden, fakePage(2, 30), far]);
    h.sensor.schedule();
    h.flush();
    expect(h.exec).toHaveBeenCalledExactlyOnceWith("viewport.setCurrentPage", 2);
    // Hidden costs no layout read.
    expect(hidden.rectReads).toBe(0);
    // The band AFTER the first top>80 one is never reached… there is none here;
    // `far` itself IS the break point so it reads exactly once.
    expect(far.rectReads).toBe(1);
  });

  it("the viewport:page-changed echo is the ONLY writer of current()", () => {
    const h = makeHarness([fakePage(0, -900), fakePage(1, -20)]);
    expect(h.sensor.current()).toBe(0);
    h.sensor.schedule();
    h.flush();
    // senseNow never writes the mirror directly — the exec's bus echo does.
    expect(h.sensor.current()).toBe(0);
    h.emitBus("viewport:page-changed", { index: 1 });
    expect(h.sensor.current()).toBe(1);
  });

  it("viewport:scroll-to-page scrolls the target placeholder into view (missing target = silent no-op)", () => {
    const p1 = fakePage(1, 500);
    const h = makeHarness([fakePage(0, 0), p1]);
    h.emitBus("viewport:scroll-to-page", { index: 1 });
    expect(p1.scrollIntoView).toHaveBeenCalledExactlyOnceWith({ behavior: "smooth", block: "start" });
    // Missing placeholder (early streaming): no throw.
    h.emitBus("viewport:scroll-to-page", { index: 42 });
  });

  it("reset() zeroes the mirror (doc switch — viewport.reset is deliberately silent)", () => {
    const h = makeHarness([fakePage(0, 0)]);
    h.emitBus("viewport:page-changed", { index: 5 });
    expect(h.sensor.current()).toBe(5);
    h.sensor.reset();
    expect(h.sensor.current()).toBe(0);
  });

  it("dispose(): scroll listener off, pending rAF cancelled, both subs released, post-dispose schedule() no-op", () => {
    const h = makeHarness([fakePage(0, -900), fakePage(1, -20)]);
    expect(h.scrollListenerCount()).toBe(1);
    expect(h.liveSubs()).toBe(2);

    h.sensor.schedule(); // pending rAF at dispose time
    h.sensor.dispose();

    expect(h.scrollListenerCount()).toBe(0);
    expect(h.liveSubs()).toBe(0);
    h.flush(); // the pending rAF must have been cancelled — no sense fires
    expect(h.exec).not.toHaveBeenCalled();

    h.sensor.schedule(); // post-dispose schedule is a no-op
    expect(h.pending()).toBe(0);
    h.flush();
    expect(h.exec).not.toHaveBeenCalled();
  });
});
