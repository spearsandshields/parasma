/**
 * Tier-C barrel guarantees (@parasma/snippet/chrome — src/chrome/index.ts):
 *
 *  (a) The barrel resolves in BARE node — this file deliberately installs NO
 *      DOM shims (no `document`, no `HTMLElement`, no `customElements`); the
 *      ABSENCE of shims IS the proof. Any future module-scope DOM call anywhere
 *      in the barrel's import graph throws here → red. Vitest's per-file module
 *      isolation keeps other test files' shims out.
 *  (b) Importing it defines no custom element / touches no global.
 *  (c) Exact-surface lock: the runtime export list is pinned, so barrel drift
 *      (accidentally re-exporting `el`, dropping a factory) is caught. Type-only
 *      exports are erased and correctly absent from Object.keys.
 *  (d) Import hygiene: the barrel's EXTRACTED import specifiers (not raw
 *      substrings — the header comment legitimately mentions "element" and the
 *      stylesheet) must never reach ../element, ../mount, ../headless-viewer,
 *      ../index, any .css, or @fontsource.
 */
/// <reference types="vite/client" />
import { describe, it, expect } from "vitest";
// Vite's ?raw gives the barrel SOURCE without node:fs (tsconfig "types":[] has
// no @types/node — the vite/client reference types this instead, the same
// pattern headless-viewer.ts uses).
import barrelSource from "./index.ts?raw";

describe("chrome barrel — side-effect freedom", () => {
  it("(a) imports cleanly in bare node with no DOM shims", async () => {
    const g = globalThis as { document?: unknown; HTMLElement?: unknown };
    // Preconditions: this test file really is DOM-less.
    expect(g.document).toBeUndefined();
    expect(g.HTMLElement).toBeUndefined();
    const mod = await import("./index");
    expect(mod).toBeTruthy();
  });

  it("(b) touches no global — customElements stays undefined after import", async () => {
    await import("./index");
    expect((globalThis as { customElements?: unknown }).customElements).toBeUndefined();
  });

  it("(c) exact runtime export surface", async () => {
    const mod = await import("./index");
    expect(Object.keys(mod).sort()).toEqual([
      "createCommentsPanel",
      "createExplorer",
      "createFormulaBar",
      "createSearchBox",
      "createSheetTabs",
      "createToolbar",
      "icon",
      "installSlots",
    ]);
  });

  it("(d) import hygiene — no banned specifier in the barrel source", () => {
    const specifiers = [...barrelSource.matchAll(/(?:from|import)\s+["']([^"']+)["']/g)].map(
      (m) => m[1],
    );
    // Sanity: the extraction found the real re-exports (guards against a refactor
    // that changes quoting style and silently blinds this test).
    expect(specifiers).toContain("./toolbar");
    expect(specifiers).toContain("../slots");
    const bannedExact = new Set(["../element", "../mount", "../headless-viewer", "../index"]);
    for (const spec of specifiers) {
      expect(bannedExact.has(spec), `banned specifier: ${spec}`).toBe(false);
      expect(/\.css$/.test(spec), `CSS import is a side effect: ${spec}`).toBe(false);
      expect(/^@fontsource/.test(spec), `@fontsource import is a side effect: ${spec}`).toBe(
        false,
      );
    }
  });
});
