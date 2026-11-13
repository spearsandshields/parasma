import { it, expect } from "vitest";

/**
 * The module-scope-purity gate (a design decision: hygiene, not an SSR feature).
 * REAL imports, no mocks, node environment — if any module in the chain
 * (@parasma/vue → snippet/headless → headless → core → wasm glue) touches
 * window/document/navigator at import time, this throws where a browser-only
 * suite would never notice. All DOM work must start at mount, not import.
 */
it("the full package chain imports under node with no DOM globals", async () => {
  const mod = await import("./index");
  expect(typeof mod.useParasma).toBe("function");
  expect(typeof mod.sniffKind).toBe("function");
  expect(mod.supportedFormats.length).toBeGreaterThanOrEqual(3);
  // The typed error is constructible and carries its contract shape.
  const e = new mod.UnsupportedFormatError("x.bin");
  expect(e.name).toBe("UnsupportedFormatError");
});
