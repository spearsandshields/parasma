import { describe, it, expect, vi, beforeAll, beforeEach } from "vitest";

// mountViewer is mocked so element tests never boot a kernel/wasm. The mock is
// hoisted (vi.mock) and re-implemented per test via mountViewerMock.
const mountViewerMock = vi.hoisted(() => vi.fn());
vi.mock("./mount", () => ({ mountViewer: mountViewerMock }));

// The snippet runs under the `node` vitest env (no DOM). element.ts derives its
// base class SSR-safely from globalThis.HTMLElement, so we shim a rich-enough
// base BEFORE a dynamic import (a static import would eval the class before the
// shim runs): attribute map (driving attributeChangedCallback for observed
// attributes, like the real upgrade machinery), a mini EventTarget, and a
// settable isConnected.
class FakeHTMLElement {
  private attrs = new Map<string, string>();
  private listeners = new Map<string, Set<(e: Event) => void>>();
  isConnected = false;

  getAttribute(n: string): string | null {
    return this.attrs.has(n) ? (this.attrs.get(n) as string) : null;
  }
  hasAttribute(n: string): boolean {
    return this.attrs.has(n);
  }
  setAttribute(n: string, v: string): void {
    const old = this.getAttribute(n);
    this.attrs.set(n, v);
    this.notify(n, old, v);
  }
  removeAttribute(n: string): void {
    const old = this.getAttribute(n);
    this.attrs.delete(n);
    if (old !== null) this.notify(n, old, null);
  }
  private notify(n: string, oldV: string | null, newV: string | null): void {
    const observed =
      (this.constructor as { observedAttributes?: string[] }).observedAttributes ?? [];
    const cb = (this as unknown as {
      attributeChangedCallback?: (n: string, o: string | null, v: string | null) => void;
    }).attributeChangedCallback;
    if (observed.includes(n) && cb) cb.call(this, n, oldV, newV);
  }
  addEventListener(t: string, l: (e: Event) => void): void {
    let s = this.listeners.get(t);
    if (!s) this.listeners.set(t, (s = new Set()));
    s.add(l);
  }
  removeEventListener(t: string, l: (e: Event) => void): void {
    this.listeners.get(t)?.delete(l);
  }
  dispatchEvent(e: Event): boolean {
    for (const l of [...(this.listeners.get(e.type) ?? [])]) l.call(this, e);
    // Per the DOM spec: false iff a listener called preventDefault() on a
    // cancelable event. The fan-out relies on this to propagate a veto.
    return !e.defaultPrevented;
  }
}

let registerElement: typeof import("./element").registerElement;
let ParasmaViewerElement: typeof import("./element").ParasmaViewerElement;
let SNIPPET_DOM_EVENTS: typeof import("./element").SNIPPET_DOM_EVENTS;

beforeAll(async () => {
  (globalThis as { HTMLElement?: unknown }).HTMLElement = FakeHTMLElement;
  // Belt-and-braces: Node ships CustomEvent, but pin a shim shape if absent.
  (globalThis as { CustomEvent?: unknown }).CustomEvent ??= class extends Event {
    detail: unknown;
    constructor(type: string, init?: { detail?: unknown } & EventInit) {
      super(type, init);
      this.detail = init?.detail ?? null; // DOM default: detail is null
    }
  };
  const mod = await import("./element");
  registerElement = mod.registerElement;
  ParasmaViewerElement = mod.ParasmaViewerElement;
  SNIPPET_DOM_EVENTS = mod.SNIPPET_DOM_EVENTS;
});

/**
 * A controllable stand-in for the ViewerHandle mountViewer resolves.
 *
 * DELIBERATE: this mock destroy must NOT clear the listener registry. The REAL
 * HeadlessViewerHandle.destroy() clears it (headless-viewer.ts teardown), so if
 * this mock ever mirrored real destroy, the leak tests (emit-on-old-handle → 0
 * dispatches) would pass VACUOUSLY without detachFanout. The mock keeps the
 * registry alive precisely so those tests exercise the element's OWN
 * unsubscription.
 */
function makeHandle() {
  const registry = new Map<string, Set<(...a: unknown[]) => void>>();
  return {
    registry,
    on: vi.fn((ev: string, cb: (...a: unknown[]) => void) => {
      let s = registry.get(ev);
      if (!s) registry.set(ev, (s = new Set()));
      s.add(cb);
      return () => s.delete(cb);
    }),
    emit(ev: string, ...a: unknown[]) {
      for (const cb of [...(registry.get(ev) ?? [])]) cb(...a);
    },
    destroy: vi.fn(async () => {}),
    loadUrl: vi.fn(async () => {}),
    setTheme: vi.fn(),
    setDev: vi.fn(),
    state: { currentPage: 1 },
  };
}
type FakeHandle = ReturnType<typeof makeHandle>;

type TestEl = InstanceType<typeof import("./element").ParasmaViewerElement> & {
  isConnected: boolean;
};

const mounts: FakeHandle[] = [];

function makeEl(): TestEl {
  const el = new ParasmaViewerElement() as unknown as TestEl;
  el.isConnected = true;
  return el;
}

/** Flush the microtask queue (boot's snapshot defer + the mock mount). */
const flush = () => new Promise<void>((r) => setTimeout(r, 0));

/** The MountOptions object the Nth mountViewer call received. */
function mountOpts(n = 0): Record<string, unknown> {
  return mountViewerMock.mock.calls[n]?.[1] as Record<string, unknown>;
}

beforeEach(() => {
  mounts.length = 0;
  mountViewerMock.mockReset();
  mountViewerMock.mockImplementation(async () => {
    const h = makeHandle();
    mounts.push(h);
    return h;
  });
});

/** A minimal CustomElementRegistry stub tracking defines. */
function fakeRegistry(seed?: Record<string, unknown>) {
  const map = new Map<string, unknown>(Object.entries(seed ?? {}));
  return {
    map,
    define: vi.fn((tag: string, ctor: unknown) => {
      map.set(tag, ctor);
    }),
    get: (tag: string) => map.get(tag),
  };
}

describe("registerElement — coexistence", () => {
  it("(a) fresh tag → define() called, returns the tag", () => {
    const reg = fakeRegistry();
    const tag = registerElement("parasma-viewer", reg as unknown as CustomElementRegistry);
    expect(tag).toBe("parasma-viewer");
    expect(reg.define).toHaveBeenCalledTimes(1);
    expect(reg.define).toHaveBeenCalledWith("parasma-viewer", ParasmaViewerElement);
  });

  it("(b) SAME class re-registered → no warn, no redefine", () => {
    const reg = fakeRegistry({ "parasma-viewer": ParasmaViewerElement });
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const tag = registerElement("parasma-viewer", reg as unknown as CustomElementRegistry);
    expect(tag).toBe("parasma-viewer");
    expect(reg.define).not.toHaveBeenCalled();
    expect(warn).not.toHaveBeenCalled();
    warn.mockRestore();
  });

  it("(c) FOREIGN class under the same tag → warn, no define, no throw", () => {
    class OtherViewer {}
    const reg = fakeRegistry({ "parasma-viewer": OtherViewer });
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const tag = registerElement("parasma-viewer", reg as unknown as CustomElementRegistry);
    expect(tag).toBe("parasma-viewer");
    expect(reg.define).not.toHaveBeenCalled();
    expect(warn).toHaveBeenCalledTimes(1);
    warn.mockRestore();
  });

  it("(d) a custom tag → defines under it", () => {
    const reg = fakeRegistry();
    const tag = registerElement("acme-doc", reg as unknown as CustomElementRegistry);
    expect(tag).toBe("acme-doc");
    expect(reg.define).toHaveBeenCalledWith("acme-doc", ParasmaViewerElement);
  });

  it("no registry (SSR / undefined customElements) → returns tag, no throw", () => {
    expect(registerElement("parasma-viewer", undefined)).toBe("parasma-viewer");
  });
});

describe("options property — vue-shaped binding contract", () => {
  it("(1) options set pre-mount → mountViewer called once with those fields", async () => {
    const el = makeEl();
    el.options = { controls: true, dev: true };
    el.connectedCallback();
    await flush();
    expect(mountViewerMock).toHaveBeenCalledTimes(1);
    expect(mountOpts().controls).toBe(true);
    expect(mountOpts().dev).toBe(true);
  });

  it("(2) present attributes OVERRIDE their options fields (incl. explicit theme=auto)", async () => {
    const el = makeEl();
    el.options = { theme: "light", src: "a.docx" };
    el.setAttribute("theme", "dark");
    el.setAttribute("src", "b.docx");
    el.connectedCallback();
    await flush();
    expect(mountOpts().theme).toBe("dark");
    expect(mountOpts().src).toBe("b.docx");

    const el2 = makeEl();
    el2.options = { theme: "dark" };
    el2.setAttribute("theme", "auto"); // explicit auto is PRESENT → overrides back to auto
    el2.connectedCallback();
    await flush();
    expect(mountOpts(1).theme).toBe("auto");
  });

  it("(3) absent boolean attribute is UNSET (defers to options), present overrides to true", async () => {
    const el = makeEl();
    el.options = { controls: false };
    el.connectedCallback();
    await flush();
    expect(mountOpts().controls).toBe(false);

    const el2 = makeEl();
    el2.options = { controls: false };
    el2.setAttribute("controls", "");
    el2.connectedCallback();
    await flush();
    expect(mountOpts(1).controls).toBe(true);
  });

  it("(4) function-valued fields ride BY REFERENCE (contextMenu/slots)", async () => {
    const contextMenu = vi.fn();
    const overlay = vi.fn();
    const el = makeEl();
    el.options = {
      contextMenu: contextMenu as never,
      slots: { viewportOverlay: overlay as never },
    };
    el.connectedCallback();
    await flush();
    expect(mountOpts().contextMenu).toBe(contextMenu);
    expect((mountOpts().slots as { viewportOverlay: unknown }).viewportOverlay).toBe(overlay);
  });

  it("(5) live assignment remounts: destroy awaited, handle nulled during teardown, 2nd mount sees new fields", async () => {
    const el = makeEl();
    el.connectedCallback();
    await flush();
    const h0 = mounts[0];
    let handleDuringDestroy: unknown = "unread";
    h0.destroy.mockImplementation(async () => {
      handleDuringDestroy = el.viewerHandle; // capture-null-detach window
    });
    el.options = { controls: true };
    await flush();
    expect(h0.destroy).toHaveBeenCalledTimes(1);
    expect(handleDuringDestroy).toBeNull();
    expect(mountViewerMock).toHaveBeenCalledTimes(2);
    expect(mountOpts(1).controls).toBe(true);
    // destroy strictly precedes the successor mount
    expect(h0.destroy.mock.invocationCallOrder[0]).toBeLessThan(
      mountViewerMock.mock.invocationCallOrder[1],
    );
  });

  it("(6) post-snapshot mid-boot assignment → pendingRemount path, exactly 2 mounts total", async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    let first = true;
    mountViewerMock.mockImplementation(async () => {
      if (first) {
        first = false;
        await gate;
      }
      const h = makeHandle();
      mounts.push(h);
      return h;
    });
    const el = makeEl();
    el.connectedCallback();
    await flush(); // snapshot taken, mountViewer called, promise pending on the gate
    expect(mountViewerMock).toHaveBeenCalledTimes(1);
    el.options = { dev: true }; // lands AFTER the snapshot → must not be dropped
    release();
    await flush();
    await flush();
    expect(mountViewerMock).toHaveBeenCalledTimes(2);
    expect(mountOpts(1).dev).toBe(true);
  });

  it("(7) explorer precedence: explorerConfig property > options.explorer > bare-attr empty-state", async () => {
    const A = { entries: [{ id: "a", name: "a.docx" }], open: (id: string) => id };
    const B = { entries: [{ id: "b", name: "b.docx" }], open: (id: string) => id };

    const el = makeEl();
    el.explorerConfig = A;
    el.options = { explorer: B };
    el.connectedCallback();
    await flush();
    expect(mountOpts(0).explorer).toBe(A);

    const el2 = makeEl();
    el2.options = { explorer: B };
    el2.connectedCallback();
    await flush();
    expect(mountOpts(1).explorer).toBe(B);

    const el3 = makeEl();
    el3.setAttribute("explorer", "");
    el3.connectedCallback();
    await flush();
    const bare = mountOpts(2).explorer as { entries: unknown[]; open: (id: string) => string };
    expect(bare.entries).toEqual([]);
    expect(bare.open("x")).toBe("x");
  });

  it("(13) same-tick post-connect assignment is ABSORBED into one mount (property + attribute)", async () => {
    const el = makeEl();
    el.connectedCallback();
    el.options = { dev: true }; // same tick, before the microtask snapshot
    await flush();
    expect(mountViewerMock).toHaveBeenCalledTimes(1);
    expect(mountOpts().dev).toBe(true);

    const el2 = makeEl();
    el2.connectedCallback();
    el2.setAttribute("dev", ""); // same-tick attribute set also absorbed
    await flush();
    expect(mountViewerMock).toHaveBeenCalledTimes(2);
    expect(mountOpts(1).dev).toBe(true);
  });

  it("(14) upgrade-property dance rescues a pre-upgrade own-property shadow", async () => {
    const el = makeEl();
    // The pre-upgrade shape: an own DATA property shadowing the class accessor.
    Object.defineProperty(el, "options", {
      value: { controls: true },
      writable: true,
      configurable: true,
      enumerable: true,
    });
    el.connectedCallback();
    await flush();
    expect(mountViewerMock).toHaveBeenCalledTimes(1);
    expect(mountOpts().controls).toBe(true);
    // The accessor is restored: a later assignment routes through it → remount.
    el.options = { dev: true };
    await flush();
    expect(mountViewerMock).toHaveBeenCalledTimes(2);
    expect(mountOpts(1).dev).toBe(true);
  });

  it("(15) attribute REMOVAL restores the options base live (dev/theme)", async () => {
    const el = makeEl();
    el.options = { dev: true, theme: "light" };
    el.setAttribute("dev", "");
    el.setAttribute("theme", "dark");
    el.connectedCallback();
    await flush();
    const h = mounts[0];
    el.removeAttribute("dev");
    expect(h.setDev).toHaveBeenLastCalledWith(true); // options base, NOT false
    el.removeAttribute("theme");
    expect(h.setTheme).toHaveBeenLastCalledWith("light"); // concrete options.theme
  });

  it("bare element (no options) mounts with today's exact defaults", async () => {
    const el = makeEl();
    el.connectedCallback();
    await flush();
    const o = mountOpts();
    expect(o.src).toBeUndefined();
    expect(o.theme).toBe("auto");
    expect(o.controls).toBe(false); // element default (mountViewer alone defaults true)
    expect(o.dev).toBe(false);
    expect(o.useLocalFonts).toBeUndefined(); // absent → persisted one-click choice wins
    expect(o.explorer).toBeUndefined();
  });

  it("options getter returns the exact stored reference", () => {
    const el = makeEl();
    const o = { controls: true };
    el.options = o;
    expect(el.options).toBe(o);
  });
});

describe("CustomEvent fan-out", () => {
  it("(8) pageChange fans out as 'parasma-page-change' with detail + bubbles + composed", async () => {
    const el = makeEl();
    el.connectedCallback();
    await flush();
    const spy = vi.fn();
    el.addEventListener("parasma-page-change", spy as never);
    mounts[0].emit("pageChange", 3);
    expect(spy).toHaveBeenCalledTimes(1);
    const e = spy.mock.calls[0][0] as CustomEvent<number>;
    expect(e.detail).toBe(3);
    expect(e.bubbles).toBe(true);
    expect(e.composed).toBe(true);
    expect(e.cancelable).toBe(false);
  });

  it("(8b) linkActivate is CANCELABLE and preventDefault() feeds back to the viewer", async () => {
    // The reason link activation is an EVENT and not a function option: a
    // function cannot ride an HTML attribute, so a plain-markup
    // <parasma-viewer> consumer could never reach it. This proves the markup
    // path — addEventListener + the native preventDefault() — actually vetoes.
    const el = makeEl();
    el.connectedCallback();
    await flush();

    let prevented = false;
    const payload = {
      kind: "url" as const,
      href: "https://example.com",
      preventDefault() {
        prevented = true;
      },
      get defaultPrevented() {
        return prevented;
      },
    };

    // (a) nobody listening → not vetoed → the viewer opens.
    mounts[0].emit("linkActivate", payload);
    expect(prevented).toBe(false);

    // (b) a listener calling the NATIVE preventDefault vetoes the viewer.
    el.addEventListener("parasma-link-activate", ((e: Event) => e.preventDefault()) as never);
    mounts[0].emit("linkActivate", payload);
    expect(prevented).toBe(true);
  });

  it("(8c) a listener that does NOT prevent leaves the link openable", async () => {
    const el = makeEl();
    el.connectedCallback();
    await flush();
    let prevented = false;
    const payload = {
      kind: "anchor" as const,
      name: "Heading1",
      page: 3,
      y: 240,
      preventDefault() {
        prevented = true;
      },
      get defaultPrevented() {
        return prevented;
      },
    };
    const spy = vi.fn();
    el.addEventListener("parasma-link-activate", spy as never);
    mounts[0].emit("linkActivate", payload);
    expect(spy).toHaveBeenCalledTimes(1);
    expect((spy.mock.calls[0][0] as CustomEvent).cancelable).toBe(true);
    expect(prevented).toBe(false); // observed, not blocked
  });

  it("(9) exhaustive map: every SnippetEventMap event reaches its DOM name; exact-strings name-lock", async () => {
    // NAME LOCK — the public DOM contract. A rename (esp. back to a native-
    // colliding "load"/"error") must fail here first.
    expect(SNIPPET_DOM_EVENTS).toEqual({
      load: "parasma-load",
      unload: "parasma-unload",
      error: "parasma-error",
      pageChange: "parasma-page-change",
      layoutTruncated: "parasma-layout-truncated",
      sheetChange: "parasma-sheet-change",
      zoomChange: "parasma-zoom-change",
      themeChange: "parasma-theme-change",
      linkActivate: "parasma-link-activate",
      cellSelect: "parasma-cell-select",
      commentFocus: "parasma-comment-focus",
      printProgress: "parasma-print-progress",
    });

    const el = makeEl();
    el.connectedCallback();
    await flush();
    const h = mounts[0];
    const sentinels: Record<string, unknown> = {
      load: { kind: "word" },
      // Arg-less event: the DOM spec defaults CustomEventInit.detail to NULL
      // (browsers + Node normalize undefined → null) — the platform-faithful
      // contract, pinned here.
      unload: null,
      error: new Error("boom"),
      pageChange: 7,
      layoutTruncated: 42,
      sheetChange: 2,
      zoomChange: 1.5,
      themeChange: "dark",
      // Carries a veto payload; the fan-out reads `preventDefault` off it, so a
      // bare sentinel would not exercise the cancelable path (test 8b does).
      linkActivate: { kind: "url", href: "https://example.com", preventDefault() {} },
      cellSelect: { ref: "B2", raw: "=1+1", display: "2" },
      commentFocus: 3,
      printProgress: { done: 1, total: 3 },
    };
    for (const [ev, domName] of Object.entries(SNIPPET_DOM_EVENTS)) {
      const spy = vi.fn();
      el.addEventListener(domName as never, spy as never);
      if (ev === "unload") h.emit(ev);
      else h.emit(ev, sentinels[ev]);
      expect(spy, `event ${ev} → ${domName}`).toHaveBeenCalledTimes(1);
      const e = spy.mock.calls[0][0] as CustomEvent<unknown>;
      expect(e.detail, `detail of ${ev}`).toBe(sentinels[ev]);
    }
  });

  it("(16) initial-load ordering pin: subscription exists on the handle before any load emit", async () => {
    const el = makeEl();
    const spy = vi.fn();
    el.addEventListener("parasma-load", spy as never);
    el.connectedCallback();
    await flush();
    // mountViewer's src load is fire-and-forget (mount.ts invariant), so the
    // fan-out subscription — attached synchronously in boot()'s post-await
    // continuation — is in place BEFORE any network-bound load settles.
    expect(mounts[0].on).toHaveBeenCalledWith("load", expect.any(Function));
    const info = { kind: "word", pageCount: 3 };
    mounts[0].emit("load", info);
    expect(spy).toHaveBeenCalledTimes(1);
    expect((spy.mock.calls[0][0] as CustomEvent<unknown>).detail).toBe(info);
  });
});

describe("fan-out listener hygiene (no leaks)", () => {
  it("(10) remount detaches the old handle's subs: old emit → 0, new emit → 1", async () => {
    const el = makeEl();
    el.connectedCallback();
    await flush();
    const spy = vi.fn();
    el.addEventListener("parasma-page-change", spy as never);
    el.options = { dev: true }; // remount
    await flush();
    expect(mounts.length).toBe(2);
    mounts[0].emit("pageChange", 5); // OLD handle — mock destroy deliberately kept its registry
    expect(spy).toHaveBeenCalledTimes(0);
    mounts[1].emit("pageChange", 5);
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("(11) disconnect unsubscribes: old emit → 0, fanoutUnsubs emptied", async () => {
    const el = makeEl();
    el.connectedCallback();
    await flush();
    const spy = vi.fn();
    el.addEventListener("parasma-page-change", spy as never);
    el.isConnected = false;
    el.disconnectedCallback();
    mounts[0].emit("pageChange", 4);
    expect(spy).toHaveBeenCalledTimes(0);
    expect((el as unknown as { fanoutUnsubs: unknown[] }).fanoutUnsubs).toEqual([]);
  });

  it("(12) a superseded in-flight boot's handle is destroyed and NEVER subscribed", async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    mountViewerMock.mockImplementation(async () => {
      await gate;
      const h = makeHandle();
      mounts.push(h);
      return h;
    });
    const el = makeEl();
    el.connectedCallback();
    await flush(); // mount in flight
    el.isConnected = false;
    el.disconnectedCallback(); // supersedes the boot
    release();
    await flush();
    expect(mounts.length).toBe(1);
    expect(mounts[0].destroy).toHaveBeenCalledTimes(1);
    expect(mounts[0].on).not.toHaveBeenCalled();
    const spy = vi.fn();
    el.addEventListener("parasma-page-change", spy as never);
    mounts[0].emit("pageChange", 9);
    expect(spy).toHaveBeenCalledTimes(0);
  });
});
