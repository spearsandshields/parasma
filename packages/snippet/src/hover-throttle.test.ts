import { describe, it, expect } from "vitest";
import { createHoverThrottle, type HoverSample } from "./hover-throttle";

/**
 * Runs under the `node` vitest env (no DOM): createHoverThrottle injects rAF and
 * only writes `sample.pageEl.style.cursor`, so a hand-built fake page element and
 * a manual rAF queue drive it with zero DOM. These tests prove the three
 * guarantees the worker-mode hover smoothness rests on:
 *   - keep-latest: only the LAST sample of a burst is resolved (intermediate
 *     samples are coalesced, never resolved);
 *   - one-in-flight: at most ONE resolve runs at a time (no pile-up);
 *   - trailing flush: a sample arriving mid-resolve is ALWAYS resolved after, so
 *     the resting cursor reflects the final pointer position (never sticks stale).
 */

/** A minimal page-element stand-in — the throttle only writes `.style.cursor`. */
function fakePage(): HTMLElement {
  return { style: { cursor: "" } } as unknown as HTMLElement;
}

/** A manual rAF queue: `flushFrame()` runs exactly the callbacks queued so far. */
function fakeRaf() {
  let next = 1;
  const cbs = new Map<number, () => void>();
  const raf = (cb: () => void): number => {
    const h = next++;
    cbs.set(h, cb);
    return h;
  };
  const caf = (h: number): void => {
    cbs.delete(h);
  };
  const flushFrame = (): void => {
    const due = [...cbs.entries()];
    cbs.clear();
    for (const [, cb] of due) cb();
  };
  const pendingCount = () => cbs.size;
  return { raf, caf, flushFrame, pendingCount };
}

/** A resolve whose settle is DEFERRED under test control (simulates a slow RPC). */
function deferredResolve() {
  const calls: HoverSample[] = [];
  const queue: Array<(c: string) => void> = [];
  const resolve = (idx: number, x: number, y: number): Promise<string> => {
    calls.push({ pageEl: null as unknown as HTMLElement, idx, x, y });
    return new Promise<string>((res) => {
      queue.push(res);
    });
  };
  /** Settle the OLDEST pending resolve, then drain the .then/.finally microtasks. */
  const settle = async (cursor = "pointer"): Promise<void> => {
    queue.shift()?.(cursor);
    await Promise.resolve();
    await Promise.resolve();
  };
  return { resolve, calls, settle, inFlight: () => queue.length };
}

describe("createHoverThrottle", () => {
  it("keep-latest: a burst of samples coalesces to ONE resolve of the LAST position", async () => {
    const { raf, flushFrame } = fakeRaf();
    const dr = deferredResolve();
    const t = createHoverThrottle(dr.resolve, raf, () => {});

    const page = fakePage();
    t.schedule({ pageEl: page, idx: 0, x: 1, y: 1 });
    t.schedule({ pageEl: page, idx: 0, x: 2, y: 2 });
    t.schedule({ pageEl: page, idx: 0, x: 3, y: 3 }); // last

    // No resolve yet — only ONE frame was armed for the whole burst.
    expect(dr.calls.length).toBe(0);
    flushFrame();
    expect(dr.calls.length).toBe(1);
    expect(dr.calls[0]).toMatchObject({ x: 3, y: 3 }); // the LAST sample, not 1 or 2
    await dr.settle("pointer");
    expect(page.style.cursor).toBe("pointer");
  });

  it("one-in-flight: samples during a pending resolve do NOT start a second resolve", async () => {
    const { raf, flushFrame } = fakeRaf();
    const dr = deferredResolve();
    const t = createHoverThrottle(dr.resolve, raf, () => {});
    const page = fakePage();

    t.schedule({ pageEl: page, idx: 0, x: 1, y: 1 });
    flushFrame(); // starts resolve #1 (pending, not yet settled)
    expect(dr.calls.length).toBe(1);
    expect(dr.inFlight()).toBe(1);

    // More samples arrive while resolve #1 is in flight — must NOT fan out.
    t.schedule({ pageEl: page, idx: 0, x: 5, y: 5 });
    t.schedule({ pageEl: page, idx: 0, x: 6, y: 6 });
    flushFrame(); // one-in-flight guard: no new frame should have been armed either
    expect(dr.calls.length).toBe(1); // STILL just one resolve running
    expect(dr.inFlight()).toBe(1);
  });

  it("trailing flush: a sample arriving mid-resolve is ALWAYS resolved after (never sticks stale)", async () => {
    const { raf, flushFrame } = fakeRaf();
    const dr = deferredResolve();
    const t = createHoverThrottle(dr.resolve, raf, () => {});
    const page = fakePage();

    t.schedule({ pageEl: page, idx: 0, x: 1, y: 1 });
    flushFrame(); // resolve #1 (x=1) in flight
    // Newer sample lands while #1 is pending.
    t.schedule({ pageEl: page, idx: 0, x: 9, y: 9 });

    await dr.settle("pointer"); // settle #1 → its .finally arms a trailing frame
    expect(page.style.cursor).toBe("pointer");
    // The trailing flush re-armed a frame for the queued (x=9) sample.
    flushFrame();
    expect(dr.calls.length).toBe(2);
    expect(dr.calls[1]).toMatchObject({ x: 9, y: 9 }); // FINAL position resolved
    await dr.settle(""); // e.g. moved off a link
    expect(page.style.cursor).toBe(""); // resting cursor reflects the LAST sample
  });

  it("cancel: after teardown a queued sample never reschedules", async () => {
    const { raf, flushFrame, pendingCount } = fakeRaf();
    const dr = deferredResolve();
    let cancelled = 0;
    const t = createHoverThrottle(dr.resolve, raf, () => { cancelled++; });
    const page = fakePage();

    t.schedule({ pageEl: page, idx: 0, x: 1, y: 1 });
    t.cancel();
    expect(cancelled).toBe(1); // the armed frame was cancelled
    flushFrame(); // no callbacks remain
    expect(dr.calls.length).toBe(0);
    expect(pendingCount()).toBe(0);
  });
});
