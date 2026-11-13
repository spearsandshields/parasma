/**
 * teardown-contract.test.ts — the ORDERED / IDEMPOTENT / PROMISE contract of the
 * abstract teardown primitive. This is the headless
 * home of the sequencing guarantee the viewer handle's destroy() relies on:
 * kernel.destroy() must SETTLE before the engine goes away.
 */
import { describe, it, expect, vi } from "vitest";
import { runTeardown } from "./teardown";

describe("runTeardown", () => {
  it("runs steps in order", async () => {
    const order: number[] = [];
    await runTeardown([() => void order.push(1), () => void order.push(2), () => void order.push(3)]);
    expect(order).toEqual([1, 2, 3]);
  });

  it("AWAITS each async step before the next (the void-kernel.destroy race fix)", async () => {
    const order: string[] = [];
    let releaseKernel!: () => void;
    const kernelDone = new Promise<void>((r) => (releaseKernel = r));

    const p = runTeardown([
      async () => {
        order.push("kernel:start");
        await kernelDone;
        order.push("kernel:settled");
      },
      () => void order.push("engine:destroy"),
    ]);

    // The engine step must NOT have run while the kernel step is still pending.
    await Promise.resolve();
    expect(order).toEqual(["kernel:start"]);

    releaseKernel();
    await p;
    expect(order).toEqual(["kernel:start", "kernel:settled", "engine:destroy"]);
  });

  it("resolves only after the last async step settles (PROMISE)", async () => {
    let settled = false;
    const p = runTeardown([
      () => new Promise<void>((r) => setTimeout(() => ((settled = true), r()), 5)),
    ]);
    expect(settled).toBe(false);
    await p;
    expect(settled).toBe(true);
  });

  it("attempts EVERY step even if one throws, then rethrows the first error", async () => {
    const worker = vi.fn();
    await expect(
      runTeardown([
        () => {
          throw new Error("adapter blew up");
        },
        () => worker(), // must still run — the worker.terminate after a bad adapter
      ]),
    ).rejects.toThrow("adapter blew up");
    expect(worker).toHaveBeenCalledOnce();
  });

  it("supports the caller's memoized-idempotency pattern (destroy twice = one teardown)", async () => {
    const step = vi.fn(async () => {});
    let promise: Promise<void> | null = null;
    const destroy = (): Promise<void> => {
      if (promise) return promise; // 2nd call awaits the SAME completion
      promise = runTeardown([step]);
      return promise;
    };
    await Promise.all([destroy(), destroy(), destroy()]);
    expect(step).toHaveBeenCalledOnce();
  });
});
