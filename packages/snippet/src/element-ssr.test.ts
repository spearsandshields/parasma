import { describe, it, expect, vi } from "vitest";

// element.ts imports ./mount → ./attach-dom → CSS etc.; mock it out so this
// file exercises ONLY element.ts's module-scope evaluation.
vi.mock("./mount", () => ({ mountViewer: vi.fn() }));

describe("element.ts under SSR (no DOM globals)", () => {
  it("imports without HTMLElement (Nuxt/SSR server pass) — no ReferenceError at class eval", async () => {
    // This file installs NO HTMLElement shim; make the precondition explicit so
    // the test can never pass vacuously if another file's shim leaks in.
    delete (globalThis as { HTMLElement?: unknown }).HTMLElement;
    expect(typeof (globalThis as { HTMLElement?: unknown }).HTMLElement).toBe("undefined");
    const mod = await import("./element");
    expect(typeof mod.ParasmaViewerElement).toBe("function");
    // registerElement with no registry (SSR) is a no-op returning the tag.
    expect(mod.registerElement("parasma-viewer", undefined)).toBe("parasma-viewer");
  });
});
