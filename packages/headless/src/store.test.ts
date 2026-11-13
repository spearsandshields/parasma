import { describe, expect, it } from "vitest";
import { createSliceStore, type StoreSlice } from "./store";

interface ZoomState extends Record<string, unknown> {
  scale: number;
  fitMode: string;
}

function makeSlice(): StoreSlice<ZoomState> {
  return {
    state: { scale: 1, fitMode: "none" },
    on: {
      set: (ctx, scale: number) => ({ ...ctx, scale, fitMode: "none" }),
      setFit: (ctx, mode: string) => ({ ...ctx, fitMode: mode }),
    },
  };
}

describe("per-plugin single-slice store", () => {
  it("get() returns the RAW slice state (no {sliceId: state} wrapper)", () => {
    const store = createSliceStore(makeSlice());
    // A top-level field, not store.get().zoom.scale — the composition wrapper is gone.
    expect(store.get()).toEqual({ scale: 1, fitMode: "none" });
    expect(store.get().scale).toBe(1);
  });

  it("dispatch(event, payload) routes to the bare-named transition immutably", () => {
    const store = createSliceStore(makeSlice());
    const before = store.get();
    store.dispatch("set", 2);
    expect(store.get()).toEqual({ scale: 2, fitMode: "none" });
    // Immutable: the prior snapshot object was not mutated.
    expect(before).toEqual({ scale: 1, fitMode: "none" });

    store.dispatch("setFit", "width");
    expect(store.get()).toEqual({ scale: 2, fitMode: "width" });
  });

  it("watch fires on change and its returned thunk unsubscribes", () => {
    const store = createSliceStore(makeSlice());
    const seen: ZoomState[] = [];
    const off = store.watch((s) => seen.push(s));

    store.dispatch("set", 3);
    expect(seen.at(-1)).toEqual({ scale: 3, fitMode: "none" });

    off();
    const n = seen.length;
    store.dispatch("set", 4);
    expect(seen.length).toBe(n); // no further deliveries after unsubscribe
  });
});
