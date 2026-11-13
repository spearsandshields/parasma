import { describe, it, expect } from "vitest";
import { createViewerSession } from "./viewer-session";
import type { HeadlessViewerHandle } from "./types";

/**
 * The session's own contract, below the framework adapters (whose suites
 * cover the status machine end to end): snapshot identity, reconnect and
 * abandon. Node env, hand-built fake handle.
 */

type Handler = (...a: unknown[]) => void;
function fakeHandle() {
  const on = new Map<string, Set<Handler>>();
  const bus = new Map<string, Set<Handler>>();
  const sub = (m: Map<string, Set<Handler>>) => (ev: string, cb: Handler) => {
    if (!m.has(ev)) m.set(ev, new Set());
    m.get(ev)!.add(cb);
    return () => void m.get(ev)!.delete(cb);
  };
  const h = {
    state: { documentKind: "word", currentPage: 1, pageCount: 2, zoom: 1 },
    on: sub(on),
    kernel: { bus: { subscribe: sub(bus) }, exec: async () => {} },
  } as unknown as HeadlessViewerHandle;
  const count = () => [...on.values(), ...bus.values()].reduce((n, s) => n + s.size, 0);
  return {
    h,
    count,
    emit: (ev: string, ...a: unknown[]) => on.get(ev)?.forEach((cb) => cb(...a)),
    busEmit: (ev: string, ...a: unknown[]) => bus.get(ev)?.forEach((cb) => cb(...a)),
  };
}
const el = {} as HTMLElement;

describe("createViewerSession", () => {
  it("publishes a new snapshot only on change, keeping unchanged fields' identity", () => {
    const s = createViewerSession();
    const f = fakeHandle();
    s.connect(f.h, el);
    let notified = 0;
    s.subscribe(() => notified++);
    const before = s.getSnapshot();
    f.emit("pageChange", 1); // same value
    expect(notified).toBe(0);
    expect(s.getSnapshot()).toBe(before);
    f.emit("pageChange", 2);
    const after = s.getSnapshot();
    expect(notified).toBe(1);
    expect(after).not.toBe(before);
    expect(after.currentPage).toBe(2);
    expect(after.search).toBe(before.search);
  });

  it("search ticks that change nothing a counter renders are dropped", () => {
    const s = createViewerSession();
    const f = fakeHandle();
    s.connect(f.h, el);
    const tick = { query: "a", results: [1, 2], activeIdx: 0, status: "searching", scope: "sheet" };
    f.busEmit("search:changed", tick);
    const first = s.getSnapshot().search;
    f.busEmit("search:changed", { ...tick, results: [3, 4] });
    expect(s.getSnapshot().search).toBe(first);
  });

  it("disconnect unwires everything and re-arms `ready` for the next connect", async () => {
    const s = createViewerSession();
    const a = fakeHandle();
    s.connect(a.h, el);
    await expect(s.ready).resolves.toBe(a.h);
    expect(a.count()).toBeGreaterThan(10);
    s.disconnect();
    expect(a.count()).toBe(0);
    expect(s.handle).toBeNull();
    const b = fakeHandle();
    const next = s.ready;
    s.connect(b.h, el);
    await expect(next).resolves.toBe(b.h);
  });

  it("abandon rejects `ready` without touching the snapshot", async () => {
    const s = createViewerSession();
    const before = s.getSnapshot();
    s.abandon(new Error("torn down"));
    await expect(s.ready).rejects.toThrow("torn down");
    expect(s.getSnapshot()).toBe(before);
  });

  it("fail sets status error and rejects `ready`", async () => {
    const s = createViewerSession();
    const e = new Error("boot failed");
    s.fail(e);
    expect(s.getSnapshot().status).toBe("error");
    expect(s.getSnapshot().error).toBe(e);
    await expect(s.ready).rejects.toBe(e);
  });
});
