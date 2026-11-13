/**
 * teardown.ts — the abstract ORDERED teardown primitive. The contract lives
 * in headless.
 *
 * A `Disposable` releases its resources in a defined ORDER, once, and returns a
 * Promise that settles only after the last (possibly async) release. This is the
 * shared spine of the viewer handle's `destroy()`: child adapters → chrome →
 * kernel (AWAITED, so plugins settle) → engine → root.
 *
 * `runTeardown` runs the steps sequentially, AWAITING each before the next (so a
 * step returning a pending promise blocks the successor — that is what fixes the
 * `void kernel.destroy(); viewer.destroy()` race: the engine cannot die before
 * the plugin chain settles). Every step is ATTEMPTED even if an earlier one
 * throws (a throwing adapter.destroy() must not strand the worker.terminate that
 * follows it); the first error is re-thrown after the whole chain has run.
 *
 * IDEMPOTENCY is the caller's concern: memoize the returned promise (a second
 * `destroy()` awaits the SAME completion, never a second teardown).
 */
export type TeardownStep = () => void | Promise<void>;

export async function runTeardown(steps: readonly TeardownStep[]): Promise<void> {
  let firstError: unknown;
  let threw = false;
  for (const step of steps) {
    try {
      await step();
    } catch (e) {
      if (!threw) {
        threw = true;
        firstError = e;
      }
    }
  }
  if (threw) throw firstError;
}
