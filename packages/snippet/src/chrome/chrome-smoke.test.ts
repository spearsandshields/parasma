/**
 * Tier-C smoke — the factories CONSTRUCT when imported through the chrome
 * barrel, against the snippet's standard node-env fake-DOM pattern (see
 * toolbar.test.ts: el()/icon() reach for the GLOBAL document at CALL time, so a
 * runtime stub suffices — nothing runs at module scope, which is exactly what
 * chrome-barrel.test.ts proves in a shim-free file).
 *
 * Kept SEPARATE from chrome-barrel.test.ts on purpose: that file must stay
 * DOM-shim-free (the absence of shims is its proof); this one installs the stub.
 */
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import type { Kernel } from "@parasma/headless";
import type { RootScope } from "../root-scope";
import { createFormulaBar, createToolbar, icon } from "./index";

// ── minimal fake DOM (the toolbar.test.ts pattern, trimmed) ──────────────────
class FakeEl {
  tag: string;
  children: unknown[] = [];
  attrs = new Map<string, string>();
  style: Record<string, string> = {};
  dataset: Record<string, string> = {};
  textContent = "";
  value = "";
  title = "";
  offsetWidth = 0;
  classes = new Set<string>();
  classList = {
    add: (c: string) => this.classes.add(c),
    remove: (c: string) => this.classes.delete(c),
    contains: (c: string) => this.classes.has(c),
  };
  constructor(tag: string) {
    this.tag = tag;
  }
  get className(): string {
    return [...this.classes].join(" ");
  }
  set className(v: string) {
    this.classes = new Set(v.split(/\s+/).filter(Boolean));
  }
  setAttribute(k: string, v: string): void {
    this.attrs.set(k, v);
  }
  getAttribute(k: string): string | null {
    return this.attrs.get(k) ?? null;
  }
  removeAttribute(k: string): void {
    this.attrs.delete(k);
  }
  append(...nodes: unknown[]): void {
    this.children.push(...nodes);
  }
  appendChild<T>(c: T): T {
    this.children.push(c);
    return c;
  }
  replaceChildren(...nodes: unknown[]): void {
    this.children = [...nodes];
  }
  querySelector(): null {
    return null;
  }
  addEventListener(): void {}
  removeEventListener(): void {}
}

let savedDocument: unknown;
beforeEach(() => {
  const g = globalThis as { document?: unknown };
  savedDocument = g.document;
  g.document = {
    createElement: (t: string) => new FakeEl(t),
    createElementNS: (_ns: string, t: string) => new FakeEl(t),
    createTextNode: (s: string) => ({ nodeType: 3, textContent: s }),
  };
});
afterEach(() => {
  (globalThis as { document?: unknown }).document = savedDocument;
});

describe("chrome barrel — factories construct via the subpath", () => {
  it("createFormulaBar (zero deps) yields root + setActive/show/clear", () => {
    const bar = createFormulaBar();
    const root = bar.root as unknown as FakeEl;
    expect(root.classes.has("ps-formula")).toBe(true);
    // Workspace-fusion contract: visibility is KIND-owned (data-active);
    // show()/clear() only fill/empty content (data-empty).
    bar.setActive(true);
    expect(root.getAttribute("data-active")).toBe("1");
    bar.show({ ref: "B2", raw: "=1+1", display: "2" });
    expect(root.getAttribute("data-empty")).toBeNull();
    bar.clear();
    expect(root.getAttribute("data-empty")).toBe("1");
    expect(root.getAttribute("data-active")).toBe("1");
  });

  it("icon() builds the Lucide svg at call time", () => {
    const svg = icon("sun", 14) as unknown as FakeEl;
    expect(svg.tag).toBe("svg");
    expect(svg.getAttribute("width")).toBe("14");
    expect(svg.getAttribute("stroke")).toBe("currentColor");
    expect(svg.children.length).toBeGreaterThan(0);
  });

  it("createToolbar constructs against a fake kernel/scope and destroys cleanly", () => {
    let subs = 0;
    const kernel = {
      exec: async () => undefined,
      bus: {
        subscribe: () => {
          subs++;
          return () => {
            subs--;
          };
        },
      },
    } as unknown as Kernel;
    const doc = new FakeEl("#document");
    const scope = { doc, activeElement: () => null } as unknown as RootScope;
    const toolbar = createToolbar({
      kernel,
      scope,
      computeFit: () => 1,
      onToggleComments: () => {},
      onToggleTheme: () => {},
      onToggleFullscreen: () => {},
      onPrint: () => {},
    });
    expect((toolbar.root as unknown as FakeEl).classes.has("ps-toolbar")).toBe(true);
    expect(subs).toBe(1); // viewport:page-changed
    toolbar.destroy();
    expect(subs).toBe(0);
  });
});
