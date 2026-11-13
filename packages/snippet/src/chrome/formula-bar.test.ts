/**
 * Formula bar — workspace-fusion behavior: the row's VISIBILITY is owned by the
 * document kind (setActive, wired off mount's kind site), NOT by cell selection
 * — killing the old pop-in reflow where the first cell click grew a 34px strip
 * and shifted the whole grid. show()/clear() only fill/empty the CONTENT
 * (data-empty), the row itself stays.
 */
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { createFormulaBar } from "./formula-bar";

class FakeEl {
  tag: string;
  children: FakeEl[] = [];
  attrs = new Map<string, string>();
  textContent = "";
  className = "";
  constructor(tag: string) {
    this.tag = tag;
  }
  setAttribute(k: string, v: string): void {
    if (k === "class") this.className = v;
    this.attrs.set(k, v);
  }
  removeAttribute(k: string): void {
    this.attrs.delete(k);
  }
  getAttribute(k: string): string | null {
    return this.attrs.get(k) ?? null;
  }
  append(...nodes: (FakeEl | string)[]): void {
    for (const n of nodes) if (n instanceof FakeEl) this.children.push(n);
  }
}

beforeEach(() => {
  (globalThis as { document?: unknown }).document = {
    createElement: (tag: string) => new FakeEl(tag),
    createTextNode: (t: string) => {
      const n = new FakeEl("#text");
      n.textContent = t;
      return n;
    },
  };
});
afterEach(() => {
  delete (globalThis as { document?: unknown }).document;
});

describe("formula bar — kind-owned visibility (no pop-in)", () => {
  it("setActive(true) shows the row EMPTY; a later show(cell) only fills content", () => {
    const h = createFormulaBar();
    const root = h.root as unknown as FakeEl;
    // Pre-activation: hidden entirely (non-spreadsheet docs).
    expect(root.getAttribute("data-active")).toBeNull();
    h.setActive(true);
    // Active but no selection: the row is PRESENT (reserved height) and empty.
    expect(root.getAttribute("data-active")).toBe("1");
    expect(root.getAttribute("data-empty")).toBe("1");
    h.show({ ref: "B7", raw: "=SUM(B2:B6)", display: "42" });
    expect(root.getAttribute("data-active")).toBe("1");
    expect(root.getAttribute("data-empty")).toBeNull();
    h.clear();
    // Clearing a cell keeps the row (no reflow) — only the content empties.
    expect(root.getAttribute("data-active")).toBe("1");
    expect(root.getAttribute("data-empty")).toBe("1");
    h.setActive(false);
    expect(root.getAttribute("data-active")).toBeNull();
  });
});
