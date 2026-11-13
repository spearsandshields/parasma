/**
 * The worker's __init boot branch initialises wasm from the
 * main-resolved input (object form, warn-free) and, with no config (null arg),
 * runs init() ZERO-args = today's behavior. worker.ts registers self.onmessage
 * at import time; we provide a fake `self` + a mocked glue and dispatch a
 * synthetic __init message. Each case uses a fresh module so the worker's
 * memoized `ready` singleton does not carry over.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

const { initSpy } = vi.hoisted(() => ({
  initSpy: vi.fn((..._a: unknown[]) => Promise.resolve({})),
}));

vi.mock("./wasm/parasma_wasm.js", () => {
  class Viewer {
    free(): void {}
  }
  return { default: initSpy, Viewer };
});

type WorkerOnMessage = (ev: { data: unknown }) => Promise<void> | void;

async function bootWorker(): Promise<{ dispatch: WorkerOnMessage; posted: unknown[] }> {
  vi.resetModules();
  initSpy.mockClear();
  const posted: unknown[] = [];
  const fakeSelf = {
    onmessage: null as WorkerOnMessage | null,
    postMessage: (m: unknown) => posted.push(m),
  };
  vi.stubGlobal("self", fakeSelf);
  await import("./worker");
  if (!fakeSelf.onmessage) throw new Error("worker did not register self.onmessage");
  return { dispatch: fakeSelf.onmessage, posted };
}

beforeEach(() => {
  initSpy.mockClear();
});
afterEach(() => {
  vi.unstubAllGlobals();
});

describe("worker __init boot branch", () => {
  it("inits wasm from a URL as { module_or_path } and replies pong", async () => {
    const { dispatch, posted } = await bootWorker();
    const url = new URL("https://x/w.wasm");
    await dispatch({ data: { kind: "req", id: 1, method: "__init", args: [url] } });
    expect(initSpy).toHaveBeenCalledTimes(1);
    expect(initSpy.mock.calls[0]![0]).toEqual({ module_or_path: url });
    expect(posted[0]).toEqual({ kind: "res", id: 1, ok: true, value: "pong" });
  });

  it("with a null arg (no-config) inits with ZERO args (RED if the default threads an input)", async () => {
    const { dispatch } = await bootWorker();
    await dispatch({ data: { kind: "req", id: 1, method: "__init", args: [null] } });
    expect(initSpy).toHaveBeenCalledTimes(1);
    expect(initSpy.mock.calls[0]!.length).toBe(0);
  });
});
