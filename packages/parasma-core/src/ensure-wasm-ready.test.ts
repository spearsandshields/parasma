/**
 * The module-level wasm-ready seam, exercised through
 * Viewer.create(). Locks the default-path identity (no-config => init() ZERO
 * args) and the object-form threading (no glue deprecation warn), plus the
 * process-singleton conflict warn.
 *
 * The glue is mocked so `init` is a spy that resolves immediately (no real
 * wasm). Cases that assert the FIRST init call use vi.resetModules() to get a
 * fresh module-memoized wasmReadyPromise; the conflict cases stay within ONE
 * module instance.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

const { initSpy } = vi.hoisted(() => ({
  initSpy: vi.fn((..._a: unknown[]) => Promise.resolve({})),
}));

vi.mock("./wasm/parasma_wasm.js", () => {
  class Viewer {
    free(): void {}
    // Present so warnIfDebugProbeAbsent() stays quiet — keeps the console.warn
    // spy scoped to the wasm-conflict warning under test.
    debugLayout(): void {}
  }
  return { default: initSpy, Viewer, parserInfo: () => ({}) };
});

beforeEach(() => {
  initSpy.mockClear();
});
afterEach(() => {
  vi.restoreAllMocks();
});

async function freshModule() {
  vi.resetModules();
  initSpy.mockClear();
  return import("./index");
}

describe("ensureWasmReady — default-path identity", () => {
  it("no-config create() calls init() with ZERO args (RED if object-form leaks into the default path)", async () => {
    const { Viewer } = await freshModule();
    await Viewer.create();
    expect(initSpy).toHaveBeenCalledTimes(1);
    expect(initSpy.mock.calls[0]!.length).toBe(0);
  });

  it("empty config create({}) also calls init() with ZERO args", async () => {
    const { Viewer } = await freshModule();
    await Viewer.create({});
    expect(initSpy).toHaveBeenCalledTimes(1);
    expect(initSpy.mock.calls[0]!.length).toBe(0);
  });
});

describe("ensureWasmReady — object-form threading", () => {
  it("a wasmUrl string is passed as { module_or_path } (RED if a bare arg fires the glue deprecation warn)", async () => {
    const { Viewer } = await freshModule();
    await Viewer.create({ wasmUrl: "https://x/w.wasm" });
    expect(initSpy).toHaveBeenCalledTimes(1);
    expect(initSpy.mock.calls[0]!.length).toBe(1);
    expect(initSpy.mock.calls[0]![0]).toEqual({ module_or_path: "https://x/w.wasm" });
  });
});

describe("ensureWasmReady — process singleton + conflict warn", () => {
  it("memoizes (second create does NOT re-init) and warns only on a DIVERGENT wasmUrl", async () => {
    const { Viewer } = await freshModule();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});

    await Viewer.create({ wasmUrl: "https://x/a.wasm" });
    expect(initSpy).toHaveBeenCalledTimes(1);

    // Divergent B: no re-init, one warn.
    await Viewer.create({ wasmUrl: "https://x/b.wasm" });
    expect(initSpy).toHaveBeenCalledTimes(1);
    expect(warn).toHaveBeenCalledTimes(1);

    // undefined (no-config) after a set input: does NOT warn.
    await Viewer.create();
    expect(warn).toHaveBeenCalledTimes(1);

    // Same A again: does NOT warn.
    await Viewer.create({ wasmUrl: "https://x/a.wasm" });
    expect(warn).toHaveBeenCalledTimes(1);
  });
});
