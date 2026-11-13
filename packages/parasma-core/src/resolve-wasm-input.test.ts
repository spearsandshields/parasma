/**
 * resolveWasmInput precedence + absolutization + the
 * default-path identity invariant. Pure: no wasm boot (resolveWasmInput is a
 * plain function; importing ./index only DEFINES the glue's init, never runs it).
 */
import { describe, it, expect, afterEach, vi } from "vitest";
import { resolveWasmInput } from "./index";

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("resolveWasmInput — default-path identity (RED if a default leaks a source)", () => {
  it("returns undefined with no config", () => {
    expect(resolveWasmInput(undefined)).toBeUndefined();
  });
  it("returns undefined with an empty config", () => {
    expect(resolveWasmInput({})).toBeUndefined();
  });
});

describe("resolveWasmInput — precedence", () => {
  it("wasmUrl beats assetBaseUrl (both present)", () => {
    const out = resolveWasmInput({
      wasmUrl: "https://x/w.wasm",
      assetBaseUrl: "https://x/base",
    });
    expect(out).toBe("https://x/w.wasm");
  });
  it("assetBaseUrl derives <base>/parasma_wasm_bg.wasm (trailing slash tolerated)", () => {
    expect(resolveWasmInput({ assetBaseUrl: "https://x/base" })).toBe(
      "https://x/base/parasma_wasm_bg.wasm",
    );
    expect(resolveWasmInput({ assetBaseUrl: "https://x/base/" })).toBe(
      "https://x/base/parasma_wasm_bg.wasm",
    );
  });
});

describe("resolveWasmInput — absolutization", () => {
  it("makes a relative wasmUrl string absolute against document.baseURI", () => {
    vi.stubGlobal("document", { baseURI: "https://host.example/app/" });
    expect(resolveWasmInput({ wasmUrl: "wasm/x.wasm" })).toBe("https://host.example/app/wasm/x.wasm");
  });
  it("passes a URL instance through unchanged (already absolute)", () => {
    const u = new URL("https://x/w.wasm");
    expect(resolveWasmInput({ wasmUrl: u })).toBe(u);
  });
  it("passes a non-URL InitInput (Response/Module-like) through untouched", () => {
    const resp = { __kind: "response" } as unknown as Response;
    expect(resolveWasmInput({ wasmUrl: resp })).toBe(resp);
  });
});
