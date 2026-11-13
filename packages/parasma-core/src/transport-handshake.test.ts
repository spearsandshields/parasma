/**
 * The reshaped worker boot handshake carries the resolved wasm
 * input, and createRenderWorker's default path is byte-identical to today. A
 * fake Worker is injected via the worker-factory seam (no real worker in vitest).
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { createWorkerTransport, createRenderWorker } from "./transport";

/** Records postMessage and auto-replies a pong to the __init request. */
class FakeWorker {
  onmessage: ((e: MessageEvent) => void) | null = null;
  onerror: ((e: unknown) => void) | null = null;
  posted: Array<{ kind: string; id: number; method: string; args: unknown[] }> = [];
  postMessage(msg: { kind: string; id: number; method: string; args: unknown[] }): void {
    this.posted.push(msg);
    queueMicrotask(() => {
      this.onmessage?.({ data: { kind: "res", id: msg.id, ok: true, value: "pong" } } as MessageEvent);
    });
  }
  terminate(): void {}
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("createWorkerTransport — handshake carries the wasm input", () => {
  it("sends __init with the resolved URL as args[0]", async () => {
    const fake = new FakeWorker();
    const url = new URL("https://x/parasma_wasm_bg.wasm");
    await createWorkerTransport({ worker: () => fake as unknown as Worker, wasmInput: url });
    expect(fake.posted[0]).toEqual({ kind: "req", id: 1, method: "__init", args: [url] });
  });

  it("no-config handshake carries args:[null] (RED if a default leaks a wasm input)", async () => {
    const fake = new FakeWorker();
    await createWorkerTransport({ worker: () => fake as unknown as Worker });
    expect(fake.posted[0]).toEqual({ kind: "req", id: 1, method: "__init", args: [null] });
  });
});

describe("createRenderWorker — construction paths", () => {
  it("no-arg builds the co-located worker.js URL with {type:'module'} and nothing else", () => {
    const ctor = vi.fn();
    vi.stubGlobal("Worker", ctor);
    createRenderWorker();
    expect(ctor).toHaveBeenCalledTimes(1);
    const [urlArg, opts] = ctor.mock.calls[0] as [URL, unknown];
    // `.js`, not `.ts`: bundlers/tsdown don't rewrite the string literal, so the
    // published package must reference the built `worker.js` (dev resolves .js→.ts).
    expect(String(urlArg)).toMatch(/worker\.js$/);
    expect(opts).toEqual({ type: "module" });
  });

  it("{url} builds new Worker(url, {type:'module'})", () => {
    const ctor = vi.fn();
    vi.stubGlobal("Worker", ctor);
    createRenderWorker({ url: "https://x/w.js" });
    expect(ctor).toHaveBeenCalledTimes(1);
    expect(ctor.mock.calls[0]).toEqual(["https://x/w.js", { type: "module" }]);
  });

  it("a function is invoked and its worker returned", () => {
    const fake = new FakeWorker() as unknown as Worker;
    expect(createRenderWorker(() => fake)).toBe(fake);
  });
});

describe("handshake message is postMessage clone-safe", () => {
  // resolveWasmInput absolutizes a string wasmUrl to its `.href` STRING, so the
  // arg the transport actually posts for the common case is a string — the
  // unambiguously clone-safe form asserted here. (A URL instance passed by a
  // consumer is also clone-safe over real postMessage; Node's structuredClone
  // lacks URL support, so it is not asserted here.)
  it("a __init req with a string arg round-trips through structuredClone", () => {
    const msg = { kind: "req", id: 1, method: "__init", args: ["https://x/w.wasm"] };
    const clone = structuredClone(msg);
    expect(clone.args[0]).toBe("https://x/w.wasm");
    expect(clone).toEqual(msg);
  });
});
