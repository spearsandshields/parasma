/**
 * Sheet tabs — workspace-fusion behavior: the tab row auto-HIDES for a
 * single-sheet workbook (a lone "CSV" tab adds nothing to the workspace);
 * multi-sheet shows the row with the active tab selected. There is NO view-mode
 * control — the grid IS the spreadsheet in the default chrome. Node-env test
 * with the toolbar.test.ts fake-DOM + fake-kernel pattern.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import type { Kernel } from "@parasma/headless";
import { createSheetTabs } from "./sheet-tabs";

class FakeEl {
  tag: string;
  children: FakeEl[] = [];
  attrs = new Map<string, string>();
  listeners = new Map<string, Set<(e: unknown) => void>>();
  style: Record<string, string> = {};
  textContent = "";
  title = "";
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
  addEventListener(t: string, cb: (e: unknown) => void): void {
    const set = this.listeners.get(t) ?? new Set();
    set.add(cb);
    this.listeners.set(t, set);
  }
  set innerHTML(v: string) {
    if (v === "") this.children = [];
  }
  querySelectorAll(sel: string): FakeEl[] {
    const cls = sel.replace(/^\./, "");
    return this.children.filter((c) => c.className.split(" ").includes(cls));
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

type BusCb = (payload: unknown) => void;

function fakeKernel(): { kernel: Kernel; emit: (id: string, payload: unknown) => void } {
  const subs = new Map<string, Set<BusCb>>();
  const kernel = {
    exec: vi.fn(() => Promise.resolve()),
    bus: {
      subscribe: (id: string, cb: BusCb) => {
        const set = subs.get(id) ?? new Set();
        set.add(cb);
        subs.set(id, set);
        return () => set.delete(cb);
      },
    },
  } as unknown as Kernel;
  return {
    kernel,
    emit: (id, payload) => {
      for (const cb of subs.get(id) ?? []) cb(payload);
    },
  };
}

const sheet = (name: string) => ({ name, firstPage: 0, pageCount: 1, maxRow: 10 });

describe("sheet tabs — single-sheet auto-hide (workspace fusion)", () => {
  it("a MULTI-sheet workbook shows the tab row", () => {
    const { kernel, emit } = fakeKernel();
    const h = createSheetTabs({ kernel, onSheetChange: vi.fn() });
    const root = h.root as unknown as FakeEl;
    emit("sheets:changed", { active: 1, sheets: [sheet("Q1"), sheet("Q2")] });
    expect(root.getAttribute("data-show")).toBe("1");
    const tabs = root.querySelectorAll(".ps-tab");
    expect(tabs.length).toBe(2);
    expect(tabs[1]!.getAttribute("aria-selected")).toBe("true");
    h.destroy();
  });

  it("a SINGLE-sheet workbook hides the tab row", () => {
    const { kernel, emit } = fakeKernel();
    const h = createSheetTabs({ kernel, onSheetChange: vi.fn() });
    const root = h.root as unknown as FakeEl;
    emit("sheets:changed", { active: 0, sheets: [sheet("CSV")] });
    expect(root.getAttribute("data-show")).toBeNull();
    h.destroy();
  });

  it("multi → single (doc switch) re-hides the row", () => {
    const { kernel, emit } = fakeKernel();
    const h = createSheetTabs({ kernel, onSheetChange: vi.fn() });
    const root = h.root as unknown as FakeEl;
    emit("sheets:changed", { active: 0, sheets: [sheet("A"), sheet("B")] });
    expect(root.getAttribute("data-show")).toBe("1");
    emit("sheets:changed", { active: 0, sheets: [sheet("CSV")] });
    expect(root.getAttribute("data-show")).toBeNull();
    h.destroy();
  });
});
