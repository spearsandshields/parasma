/**
 * fanout — the attachDom fan-out wiring contract.
 *
 * attachDom exposes onCellSelect / onCommentFocus as fan-out sinks a chrome layer
 * subscribes to; the core fires INTO the same fan-out from the hit-test path. This
 * unit-tests that seam (the full DOM path is browser-verified — the demo's formula
 * bar + comment focus route through these). RED: break `fire` to skip iteration →
 * a subscribed sink never fires; break `subscribe`'s unsubscribe → a released sink
 * keeps firing.
 */
import { describe, it, expect, vi } from "vitest";
import { createFanout } from "./fanout";

describe("createFanout", () => {
  it("delivers a fired value to every subscribed sink", () => {
    const f = createFanout<number>();
    const a = vi.fn();
    const b = vi.fn();
    f.subscribe(a);
    f.subscribe(b);
    f.fire(7);
    expect(a).toHaveBeenCalledWith(7);
    expect(b).toHaveBeenCalledWith(7);
  });

  it("stops delivering to a sink after its unsubscribe runs", () => {
    const f = createFanout<string>();
    const sink = vi.fn();
    const off = f.subscribe(sink);
    f.fire("first");
    off();
    f.fire("second");
    expect(sink).toHaveBeenCalledTimes(1);
    expect(sink).toHaveBeenCalledWith("first");
  });

  it("clear() drops every sink (teardown)", () => {
    const f = createFanout<null>();
    const sink = vi.fn();
    f.subscribe(sink);
    f.clear();
    f.fire(null);
    expect(sink).not.toHaveBeenCalled();
  });

  it("delivers null (the cleared-cell / no-selection signal)", () => {
    const f = createFanout<{ ref: string } | null>();
    const sink = vi.fn();
    f.subscribe(sink);
    f.fire(null);
    f.fire({ ref: "A1" });
    expect(sink).toHaveBeenNthCalledWith(1, null);
    expect(sink).toHaveBeenNthCalledWith(2, { ref: "A1" });
  });
});
