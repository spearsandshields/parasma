/**
 * Regression test for the doc-switch page-duplication bug.
 *
 * `load()` has an interior `await` (font loading). A rapid document switch
 * fires a second `load()` while the first is suspended there. Before the fix,
 * both calls reached `loadDocx` + `runLayout` against the shared page list and
 * raced to append pages — a 4-page document switched-to mid-stream rendered as
 * 7+ pages with adjacent duplicates.
 *
 * The fix serializes loads: each one's critical section (abort prior layout →
 * loadDocx → kick off runLayout) runs to completion before the next begins.
 * We assert that ordering by recording the WASM call sequence: two overlapping
 * loads must NOT produce back-to-back `loadDocx` calls — the prior layout must
 * be aborted and swapped out between them.
 */
import { describe, it, expect } from "vitest";
import { Viewer } from "./index";

/**
 * Minimal stand-in for the wasm-bindgen `Viewer`. Records the order of the
 * state-mutating calls `#doLoad` makes, and models the real abort contract:
 * `abortLayout()` resolves the pending `runLayout` future (the real WASM bails
 * at its next page boundary). Empty font/image lists keep `#doLoad`'s async
 * setup fast while still yielding at its `await` points.
 */
class FakeWasm {
  log: string[] = [];
  #resolveRun: ((total: number) => void) | null = null;

  loadDocx(_bytes: Uint8Array): void {
    this.log.push("loadDocx");
  }
  abortLayout(): void {
    this.log.push("abort");
    const r = this.#resolveRun;
    this.#resolveRun = null;
    r?.(0);
  }
  async runLayout(_cb: (idx: number) => void): Promise<number> {
    this.log.push("runLayout");
    return new Promise<number>((res) => {
      this.#resolveRun = res;
    });
  }
  setNow(_n: number): void {}
  pageSize(): { widthPx: number; heightPx: number } {
    return { widthPx: 600, heightPx: 800 };
  }
  fontFamiliesUsed(): string[] {
    return [];
  }
  imageRefs(): unknown[] {
    return [];
  }
  clearImageBytes(): void {}
}

function makeViewer(wasm: FakeWasm): Viewer {
  // The constructor is `private` (compile-time only); at runtime we can inject
  // a fake to exercise `load()` without the real WASM + DOM stack.
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

describe("load() serialization (doc-switch duplication regression)", () => {
  it("does not interleave two overlapping loads", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);

    // Fire two loads in the same tick — a rapid document switch.
    const a = viewer.load(new Uint8Array([1]));
    const b = viewer.load(new Uint8Array([2]));
    await Promise.all([a, b]);

    // Each load runs its full critical section before the next starts. The
    // first lays out, then the second aborts it and swaps in its own document.
    // The broken (interleaved) sequence was ["loadDocx","loadDocx","runLayout",
    // "runLayout"] — both layouts on the same doc, racing to append pages.
    expect(wasm.log).toEqual(["loadDocx", "runLayout", "abort", "loadDocx", "runLayout"]);
  });

  it("serializes a storm of rapid switches without back-to-back loadDocx", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);

    const loads = [1, 2, 3, 4].map((n) => viewer.load(new Uint8Array([n])));
    await Promise.all(loads);

    // No two `loadDocx` calls are adjacent: every document swap is fenced by a
    // `runLayout` kickoff (and, after the first, an `abort` of the prior layout).
    for (let i = 1; i < wasm.log.length; i++) {
      if (wasm.log[i] === "loadDocx") {
        expect(wasm.log[i - 1]).not.toBe("loadDocx");
      }
    }
    // Exactly one loadDocx + runLayout per load.
    expect(wasm.log.filter((e) => e === "loadDocx")).toHaveLength(4);
    expect(wasm.log.filter((e) => e === "runLayout")).toHaveLength(4);
  });
});
