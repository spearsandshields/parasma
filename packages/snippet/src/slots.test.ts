/**
 * Tier-B slots (bounded chrome injection): `slots.toolbarTrailing` mounts as its
 * own .ps-group right before the toolbar's status group; `slots.viewportOverlay`
 * mounts as a full-stage pass-through layer. Both accept a plain element or a
 * factory called with the LIVE ViewerHandle (so a floating nav can close over
 * goToPage/on("pageChange") in one phase). A throwing factory is CONTAINED:
 * one console.warn, that slot skipped, the other still installs — the chrome
 * never fails to mount because of a bad slot.
 *
 * Node-env test with the toolbar.test.ts fake-DOM pattern (el() resolves
 * `document` at RUNTIME, so a global stub suffices).
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { installSlots } from "./slots";
import type { ViewerHandle } from "./types";

// ── minimal fake DOM ──────────────────────────────────────────────────────────

class FakeEl {
  tag: string;
  /** Real elements expose tagName — installSlots duck-types on it. */
  get tagName(): string {
    return this.tag.toUpperCase();
  }
  className = "";
  children: FakeEl[] = [];
  parentElement: FakeEl | null = null;
  attrs = new Map<string, string>();
  constructor(tag: string) {
    this.tag = tag;
  }
  setAttribute(k: string, v: string): void {
    this.attrs.set(k, v);
  }
  append(...nodes: FakeEl[]): void {
    for (const n of nodes) {
      n.parentElement = this;
      this.children.push(n);
    }
  }
  appendChild(n: FakeEl): FakeEl {
    this.append(n);
    return n;
  }
  insertBefore(n: FakeEl, ref: FakeEl | null): FakeEl {
    n.parentElement = this;
    const i = ref ? this.children.indexOf(ref) : -1;
    if (i === -1) this.children.push(n);
    else this.children.splice(i, 0, n);
    return n;
  }
  querySelector(sel: string): FakeEl | null {
    const cls = sel.replace(/^\./, "");
    for (const c of this.children) {
      if (c.className.split(" ").includes(cls)) return c;
      const hit = c.querySelector(sel);
      if (hit) return hit;
    }
    return null;
  }
}

beforeEach(() => {
  (globalThis as { document?: unknown }).document = {
    createElement: (tag: string) => new FakeEl(tag),
    createTextNode: (t: string) => {
      const n = new FakeEl("#text");
      n.className = t;
      return n;
    },
  };
});
afterEach(() => {
  delete (globalThis as { document?: unknown }).document;
  vi.restoreAllMocks();
});

/** A toolbar shaped like createToolbar's: groups, with the status group LAST. */
function makeToolbar(): { root: FakeEl; statusGroup: FakeEl } {
  const root = new FakeEl("div");
  root.className = "ps-toolbar";
  const brand = new FakeEl("div");
  brand.className = "ps-group";
  const statusGroup = new FakeEl("div");
  statusGroup.className = "ps-group";
  const status = new FakeEl("div");
  status.className = "ps-status";
  statusGroup.append(status);
  root.append(brand, statusGroup);
  return { root, statusGroup };
}

function fakeHandle(): ViewerHandle {
  return { goToPage: vi.fn() } as unknown as ViewerHandle;
}

describe("installSlots (tier B)", () => {
  it("no slots config → no-op, nothing inserted", () => {
    const { root } = makeToolbar();
    const stage = new FakeEl("div");
    const out = installSlots({
      slots: undefined,
      handle: fakeHandle(),
      toolbarRoot: root as unknown as HTMLElement,
      stage: stage as unknown as HTMLElement,
    });
    expect(out.toolbarSlot).toBeUndefined();
    expect(out.overlaySlot).toBeUndefined();
    expect(root.children.length).toBe(2);
    expect(stage.children.length).toBe(0);
  });

  it("toolbarTrailing (plain element) mounts as a .ps-slot group BEFORE the status group", () => {
    const { root, statusGroup } = makeToolbar();
    const stage = new FakeEl("div");
    const mine = new FakeEl("button");
    const out = installSlots({
      slots: { toolbarTrailing: mine as unknown as HTMLElement },
      handle: fakeHandle(),
      toolbarRoot: root as unknown as HTMLElement,
      stage: stage as unknown as HTMLElement,
    });
    const wrap = out.toolbarSlot as unknown as FakeEl;
    expect(wrap.className).toContain("ps-slot");
    expect(wrap.className).toContain("ps-group");
    expect(wrap.children[0]).toBe(mine);
    // Inserted before the status group (status token stays terminal).
    const idx = root.children.indexOf(wrap);
    expect(idx).toBe(root.children.indexOf(statusGroup) - 1);
  });

  it("viewportOverlay (factory) receives the LIVE handle and mounts a pass-through layer on the stage", () => {
    const { root } = makeToolbar();
    const stage = new FakeEl("div");
    const handle = fakeHandle();
    const nav = new FakeEl("div");
    const factory = vi.fn(() => nav as unknown as HTMLElement);
    const out = installSlots({
      slots: { viewportOverlay: factory },
      handle,
      toolbarRoot: root as unknown as HTMLElement,
      stage: stage as unknown as HTMLElement,
    });
    expect(factory).toHaveBeenCalledWith(handle);
    const overlay = out.overlaySlot as unknown as FakeEl;
    expect(overlay.className).toContain("ps-slot-overlay");
    expect(overlay.children[0]).toBe(nav);
    expect(stage.children[stage.children.length - 1]).toBe(overlay);
  });

  it("a THROWING factory is contained: one warn, that slot skipped, the other still installs", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { root } = makeToolbar();
    const stage = new FakeEl("div");
    const mine = new FakeEl("button");
    const out = installSlots({
      slots: {
        viewportOverlay: () => {
          throw new Error("bad slot");
        },
        toolbarTrailing: mine as unknown as HTMLElement,
      },
      handle: fakeHandle(),
      toolbarRoot: root as unknown as HTMLElement,
      stage: stage as unknown as HTMLElement,
    });
    expect(out.overlaySlot).toBeUndefined();
    expect(stage.children.length).toBe(0);
    expect(out.toolbarSlot).toBeDefined();
    expect(warn).toHaveBeenCalledTimes(1);
  });

  it("a factory returning a non-element is contained the same way", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { root } = makeToolbar();
    const stage = new FakeEl("div");
    const out = installSlots({
      slots: { toolbarTrailing: (() => null) as unknown as HTMLElement },
      handle: fakeHandle(),
      toolbarRoot: root as unknown as HTMLElement,
      stage: stage as unknown as HTMLElement,
    });
    expect(out.toolbarSlot).toBeUndefined();
    expect(warn).toHaveBeenCalledTimes(1);
  });
});
