/**
 * Toolbar as a PURE current-page consumer (kernel-truth current page): it holds
 * no observer machinery — it subscribes `viewport:page-changed` for display and
 * dispatches `viewport.goToPage` for nav. Node-env test with a minimal fake-DOM
 * shim (el()/flash() call `document` at RUNTIME, so a global stub suffices).
 *
 * THE PIN: self-initiated goTo writes the input optimistically, so the bus echo
 * (p1 === currentPage) must produce NO flash — while a scroll-driven change
 * (a genuinely new index) MUST flash. Matches the pre-refactor UX exactly.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import type { Kernel } from "@parasma/headless";
import type { RootScope } from "../root-scope";
import { createToolbar } from "./toolbar";

// ── minimal fake DOM ──────────────────────────────────────────────────────────

class FakeEl {
  tag: string;
  children: unknown[] = [];
  attrs = new Map<string, string>();
  listeners = new Map<string, Set<(e: unknown) => void>>();
  classes = new Set<string>();
  style: Record<string, string> = {};
  dataset: Record<string, string> = {};
  textContent = "";
  value = "";
  title = "";
  offsetWidth = 0;
  /** Count of classList.add("ps-flash") — the flash() detector. */
  flashCount = 0;
  classList = {
    add: (c: string) => {
      if (c === "ps-flash") this.flashCount++;
      this.classes.add(c);
    },
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
  insertBefore<T>(node: T, _ref: unknown): T {
    this.children.push(node);
    return node;
  }
  querySelector(): null {
    return null;
  }
  addEventListener(type: string, cb: (e: unknown) => void): void {
    let s = this.listeners.get(type);
    if (!s) {
      s = new Set();
      this.listeners.set(type, s);
    }
    s.add(cb);
  }
  removeEventListener(type: string, cb: (e: unknown) => void): void {
    this.listeners.get(type)?.delete(cb);
  }
  dispatch(type: string, ev: unknown = { type }): void {
    for (const cb of [...(this.listeners.get(type) ?? [])]) cb(ev);
  }
  blur(): void {}
}

/** Depth-first search of the fake tree. */
function find(root: FakeEl, pred: (n: FakeEl) => boolean): FakeEl | null {
  if (pred(root)) return root;
  for (const c of root.children) {
    if (c instanceof FakeEl) {
      const hit = find(c, pred);
      if (hit) return hit;
    }
  }
  return null;
}

// ── harness ───────────────────────────────────────────────────────────────────

function makeHarness(pageNav?: boolean) {
  const busHandlers = new Map<string, Set<(p: unknown) => void>>();
  const exec = vi.fn(async () => undefined);
  const kernel = {
    exec,
    bus: {
      subscribe: (event: string, cb: (p: unknown) => void) => {
        let s = busHandlers.get(event);
        if (!s) {
          s = new Set();
          busHandlers.set(event, s);
        }
        s.add(cb);
        return () => s!.delete(cb);
      },
      clearReplay: () => {},
      reset: () => {},
    },
  } as unknown as Kernel;
  const doc = new FakeEl("#document");
  let activeEl: unknown = null;
  const scope = {
    doc,
    activeElement: () => activeEl,
  } as unknown as RootScope;

  const toolbar = createToolbar({
    kernel,
    scope,
    computeFit: () => 1,
    pageNav,
    onToggleComments: () => {},
    onToggleTheme: () => {},
    onToggleFullscreen: () => {},
    onPrint: () => {},
  });
  const root = toolbar.root as unknown as FakeEl;
  const nInput = find(root, (n) => n.classes.has("ps-n"))!;
  const nextBtn = find(root, (n) => n.getAttribute("aria-label") === "next page")!;
  const echo = (index: number): void => {
    for (const cb of [...(busHandlers.get("viewport:page-changed") ?? [])]) cb({ index });
  };
  const liveSubs = (event: string): number => busHandlers.get(event)?.size ?? 0;
  const setActive = (el: unknown): void => {
    activeEl = el;
  };
  return { toolbar, exec, echo, nInput, nextBtn, liveSubs, setActive };
}

// el()/flash() reach for the GLOBAL document at call time (node env has none).
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

describe("toolbar — pure current-page consumer", () => {
  it("nav dispatches kernel.exec('viewport.goToPage', n-1) with the local clamp", () => {
    const { toolbar, exec, nextBtn } = makeHarness();
    toolbar.setDocument("word", 5, "DOCX");
    nextBtn.dispatch("click");
    expect(exec).toHaveBeenCalledWith("viewport.goToPage", 1); // page 2, 0-based
  });

  it("typed-N jump: clamps and dispatches; NaN entry restores the input", () => {
    const { toolbar, exec, nInput } = makeHarness();
    toolbar.setDocument("word", 3, "DOCX");
    nInput.value = "99";
    nInput.dispatch("change");
    expect(exec).toHaveBeenCalledWith("viewport.goToPage", 2); // clamped to page 3
    expect(nInput.value).toBe("3"); // optimistic write
    expect(toolbar.currentPage).toBe(3);

    exec.mockClear();
    nInput.value = "abc";
    nInput.dispatch("change");
    expect(exec).not.toHaveBeenCalled();
    expect(nInput.value).toBe("3"); // NaN restore
  });

  it("self-initiated goTo produces NO flash on the bus echo; a scroll-driven change DOES flash", () => {
    const { toolbar, echo, nInput, nextBtn } = makeHarness();
    toolbar.setDocument("word", 5, "DOCX");

    // Self-nav: optimistic write to 2, then the kernel echoes {index:1}.
    nextBtn.dispatch("click");
    expect(nInput.value).toBe("2");
    const flashesBefore = nInput.flashCount;
    echo(1); // p1 === currentPage → guarded, no update, no flash
    expect(nInput.flashCount).toBe(flashesBefore);
    expect(toolbar.currentPage).toBe(2);

    // Scroll-driven: a genuinely new index updates N and flashes.
    echo(3);
    expect(nInput.value).toBe("4");
    expect(nInput.flashCount).toBe(flashesBefore + 1);
    expect(toolbar.currentPage).toBe(4);
  });

  it("live-N respects the focused-input guard (no value overwrite while typing) but still flashes + tracks", () => {
    const { toolbar, echo, nInput, setActive } = makeHarness();
    toolbar.setDocument("word", 9, "DOCX");

    echo(4);
    expect(nInput.value).toBe("5"); // unfocused → live update

    setActive(nInput); // user is typing in the N input
    nInput.value = "7?"; // half-typed entry must not be clobbered
    echo(2);
    expect(nInput.value).toBe("7?");
    expect(toolbar.currentPage).toBe(3); // internal tracking still moves
  });

  it("destroy releases the viewport:page-changed subscription", () => {
    const { toolbar, liveSubs } = makeHarness();
    expect(liveSubs("viewport:page-changed")).toBe(1);
    toolbar.destroy();
    expect(liveSubs("viewport:page-changed")).toBe(0);
  });

  it("pageNav:false removes the nav cluster (BYO navigator) even for word docs", () => {
    const { toolbar } = makeHarness(false);
    const root = toolbar.root as unknown as FakeEl;
    const nav = find(root, (n) => n.classes.has("ps-nav"))!;
    expect(nav.style.display).toBe("none");
    // setDocument for a PAGED kind must not resurrect it.
    toolbar.setDocument("word", 5, "DOCX");
    expect(nav.style.display).toBe("none");
    // Default (pageNav undefined) keeps it for word.
    const def = makeHarness();
    const defRoot = def.toolbar.root as unknown as FakeEl;
    def.toolbar.setDocument("word", 5, "DOCX");
    expect(find(defRoot, (n) => n.classes.has("ps-nav"))!.style.display).toBe("");
  });
});
