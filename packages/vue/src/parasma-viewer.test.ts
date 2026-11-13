// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createApp, defineCustomElement, h, nextTick, ref } from "vue";

/**
 * Component + custom-element behavior under happy-dom (this file only — the
 * rest of the suite stays node-env). The headless seam is mocked with the
 * same fake-handle pattern as use-parasma.test.ts; what's under test is the
 * SUGAR layer: stage rendering, reactive source→load, v-model echo, expose,
 * and the shadow-root boot that `defineCustomElement` implies.
 */

type Handler = (...a: unknown[]) => void;
function makeFakeHandle() {
  const on = new Map<string, Set<Handler>>();
  const h2 = {
    state: { documentKind: "word", currentPage: 1, pageCount: 3, zoom: 1 },
    on(ev: string, cb: Handler) {
      if (!on.has(ev)) on.set(ev, new Set());
      on.get(ev)!.add(cb);
      return () => on.get(ev)!.delete(cb);
    },
    kernel: { bus: { subscribe: () => () => {} } },
    dom: { onCellSelect: () => () => {} },
    zoom: { in() {}, out() {}, set: vi.fn(), fit() {} },
    goToPage: vi.fn(),
    selectSheet: vi.fn(),
    load: vi.fn(async () => {}),
    unload: vi.fn(async () => {}),
    destroy: vi.fn(async () => {}),
  };
  return { h: h2, emit: (ev: string, ...a: unknown[]) => on.get(ev)?.forEach((cb) => cb(...a)) };
}

let fake = makeFakeHandle();
const bootRoots: Element[] = [];
vi.mock("@parasma/snippet/headless", async () => ({
  // The real session (framework-neutral, imports only the mocked core).
  createViewerSession: (await import("../../snippet/src/viewer-session")).createViewerSession,
  createHeadlessViewer: vi.fn(async (config: { root: Element }) => {
    bootRoots.push(config.root);
    return fake.h;
  }),
}));
vi.mock("@parasma/core", () => ({
  sniffKind: (b: Uint8Array) => (b[0] === 0x50 ? "word" : null),
  UnsupportedFormatError: class extends Error {},
}));

import { ParasmaViewer } from "./parasma-viewer";
import type { UseParasmaReturn } from "./use-parasma";

const zip = () => new Uint8Array([0x50, 0x4b, 3, 4]);
const flush = async (n = 6) => {
  for (let i = 0; i < n; i++) await Promise.resolve();
  await nextTick();
};

beforeEach(() => {
  fake = makeFakeHandle();
  bootRoots.length = 0;
});

describe("<ParasmaViewer>", () => {
  it("renders the stage, boots into it, loads the reactive source, exposes the composable", async () => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    const exposed = ref<UseParasmaReturn | null>(null);
    const source = ref<Uint8Array | null>(null);
    const seen: unknown[] = [];
    const app = createApp({
      setup: () => () =>
        h(ParasmaViewer, {
          ref: (r: unknown) => (exposed.value = r as UseParasmaReturn),
          source: source.value,
          options: {},
          onCellSelect: (c: unknown) => seen.push(["cell", c]),
          onCommentFocus: (id: number) => seen.push(["comment", id]),
        }),
    });
    app.mount(host);
    await flush();

    const stage = host.querySelector(".parasma-viewer-stage");
    expect(stage).not.toBeNull();
    expect(bootRoots[0]).toBe(stage); // booted into OUR stage, not the host
    // Vue UNWRAPS refs on an exposed instance proxy — isReady arrives as a
    // plain boolean through a template ref (typed access is via the
    // composable itself; the template-ref proxy is a convenience view).
    expect((exposed.value as unknown as { isReady: boolean }).isReady).toBe(true);

    source.value = zip();
    await flush();
    expect(fake.h.load).toHaveBeenCalledTimes(1);

    // Handle events forward as component emits (template `@cell-select` etc.).
    fake.emit("cellSelect", { ref: "C3", raw: "", display: "7" });
    fake.emit("commentFocus", 12);
    await flush();

    expect(seen).toEqual([
      ["cell", { ref: "C3", raw: "", display: "7" }],
      ["comment", 12],
    ]);

    app.unmount();
    await flush();
    expect(fake.h.destroy).toHaveBeenCalledTimes(1);
  });

  it("v-model:page echoes engine changes up and pushes prop changes down", async () => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    const page = ref(1);
    const app = createApp({
      setup: () => () =>
        h(ParasmaViewer, {
          page: page.value,
          "onUpdate:page": (n: number) => (page.value = n),
        }),
    });
    app.mount(host);
    await flush();

    fake.emit("pageChange", 4); // engine → prop echo
    await flush();
    expect(page.value).toBe(4);

    page.value = 9; // prop → engine push
    await flush();
    expect(fake.h.goToPage).toHaveBeenCalledWith(9);
    app.unmount();
  });

  it("defineCustomElement: boots inside the element's SHADOW ROOT (the custom-element consumer path)", async () => {
    const El = defineCustomElement(ParasmaViewer);
    customElements.define("parasma-vue-test", El);
    const el = document.createElement("parasma-vue-test");
    document.body.appendChild(el);
    await flush();

    expect(el.shadowRoot).not.toBeNull();
    expect(bootRoots.length).toBe(1);
    // The boot root must live INSIDE the shadow tree — the exact topology
    // style adoption into the shadow root and token piercing of the boundary
    // (docs/theming.md) were built to serve.
    expect(bootRoots[0].getRootNode()).toBe(el.shadowRoot);
    el.remove();
    await flush();
    expect(fake.h.destroy).toHaveBeenCalledTimes(1);
  });
});
