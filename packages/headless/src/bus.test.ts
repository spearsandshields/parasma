import { describe, expect, it } from "vitest";
import { createBus } from "./bus";

describe("replayable bus — parity with Viewer.on() catch-up", () => {
  it("accumulate: a subscriber attaching after N pageReady emits gets all N in order", () => {
    const bus = createBus();
    bus.emit("pageReady", 0);
    bus.emit("pageReady", 1);
    bus.emit("pageReady", 2);

    const seen: number[] = [];
    bus.subscribe("pageReady", (i) => seen.push(i));
    // Late subscriber must catch up on every index, in order (the Viewer's
    // per-index catch-up loop). A naive emitter would deliver nothing here.
    expect(seen).toEqual([0, 1, 2]);

    // ...and still receive subsequent live emits.
    bus.emit("pageReady", 3);
    expect(seen).toEqual([0, 1, 2, 3]);
  });

  it("latest: a late loadComplete subscriber gets the sticky value exactly once", () => {
    const bus = createBus();
    bus.emit("loadComplete", 7);

    const seen: number[] = [];
    bus.subscribe("loadComplete", (n) => seen.push(n));
    expect(seen).toEqual([7]);

    // A second emit overwrites the sticky value; only the latest replays for
    // the NEXT late subscriber.
    bus.emit("loadComplete", 9);
    const seen2: number[] = [];
    bus.subscribe("loadComplete", (n) => seen2.push(n));
    expect(seen2).toEqual([9]);
    // First subscriber saw both (sticky replay 7, then live 9).
    expect(seen).toEqual([7, 9]);
  });

  it("none: transient events (loader:error) do not replay to late subscribers", () => {
    const bus = createBus();
    bus.emit("loader:error", new Error("boom"));
    const seen: unknown[] = [];
    bus.subscribe("loader:error", (e) => seen.push(e));
    expect(seen).toEqual([]);
  });

  it("reset() drops replay logs so a new doc's late subscriber sees no stale pages", () => {
    const bus = createBus();
    bus.emit("pageReady", 0);
    bus.emit("pageReady", 1);
    bus.reset();
    const seen: number[] = [];
    bus.subscribe("pageReady", (i) => seen.push(i));
    expect(seen).toEqual([]);
  });

  it("clearReplay drops one event's log only", () => {
    const bus = createBus();
    bus.emit("pageReady", 0);
    bus.emit("loadComplete", 3);
    bus.clearReplay("pageReady");

    const pages: number[] = [];
    bus.subscribe("pageReady", (i) => pages.push(i));
    expect(pages).toEqual([]); // pageReady log cleared

    const done: number[] = [];
    bus.subscribe("loadComplete", (n) => done.push(n));
    expect(done).toEqual([3]); // loadComplete log intact
  });

  it("one throwing handler does not break delivery to the rest (log-and-continue)", () => {
    const bus = createBus();
    const seen: number[] = [];
    bus.subscribe("pageReady", () => {
      throw new Error("bad handler");
    });
    bus.subscribe("pageReady", (i) => seen.push(i));
    bus.emit("pageReady", 5);
    expect(seen).toEqual([5]);
  });

  it("unsubscribe stops live delivery", () => {
    const bus = createBus();
    const seen: number[] = [];
    const off = bus.subscribe("pageReady", (i) => seen.push(i));
    bus.emit("pageReady", 0);
    off();
    bus.emit("pageReady", 1);
    expect(seen).toEqual([0]);
  });
});
