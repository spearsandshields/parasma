import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { createElement, StrictMode, act, useState, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";

/**
 * Tests over a mocked `@parasma/snippet/headless` (hand-built fake handles,
 * the real framework-neutral session). What's under test is what this
 * package adds: the boot/teardown lifecycle under React (StrictMode
 * included), snapshot → render wiring, and the component's props/callbacks.
 */

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

type Handler = (...a: unknown[]) => void;
function makeFakeHandle(log: string[], id: number) {
  const on = new Map<string, Set<Handler>>();
  const bus = new Map<string, Set<Handler>>();
  const loadResolvers: Array<{ res: () => void; rej: (e: unknown) => void }> = [];
  const sub = (m: Map<string, Set<Handler>>, ev: string, cb: Handler) => {
    if (!m.has(ev)) m.set(ev, new Set());
    m.get(ev)!.add(cb);
    return () => {
      log.push(`unsub:${id}:${ev}`);
      m.get(ev)!.delete(cb);
    };
  };
  const h = {
    state: { documentKind: "word", currentPage: 1, pageCount: 3, zoom: 1 },
    on: (ev: string, cb: Handler) => sub(on, ev, cb),
    kernel: {
      bus: { subscribe: (ev: string, cb: Handler) => sub(bus, ev, cb) },
      exec: (...a: unknown[]) => {
        log.push(`exec:${a.filter((x) => typeof x === "string").join(":")}`);
        return Promise.resolve();
      },
    },
    zoom: { in() {}, out() {}, set: (s: number) => log.push(`zoom.set:${s}`), fit() {} },
    goToPage: (n: number) => log.push(`goToPage:${n}`),
    selectSheet: (i: number) => log.push(`selectSheet:${i}`),
    scrollToGridContent() {},
    scrollToAnchorPos() {},
    print: async () => {},
    load: vi.fn(() => new Promise<void>((res, rej) => loadResolvers.push({ res, rej }))),
    unload: vi.fn(async () => {}),
    destroy: vi.fn(async () => {
      log.push(`destroy:${id}`);
    }),
  };
  return {
    h,
    emit: (ev: string, ...a: unknown[]) => on.get(ev)?.forEach((cb) => cb(...a)),
    busEmit: (ev: string, ...a: unknown[]) => bus.get(ev)?.forEach((cb) => cb(...a)),
    finishLoad: () => loadResolvers.shift()?.res(),
    failLoad: (e: unknown) => loadResolvers.shift()?.rej(e),
  };
}

let log: string[] = [];
let fakes: Array<ReturnType<typeof makeFakeHandle>> = [];
let live = 0;
let maxLive = 0;
vi.mock("@parasma/snippet/headless", async () => ({
  createViewerSession: (await import("../../snippet/src/viewer-session")).createViewerSession,
  createHeadlessViewer: vi.fn(async (config: { root: Element }) => {
    const f = makeFakeHandle(log, fakes.length);
    fakes.push(f);
    log.push(`boot:${fakes.length - 1}:${config.root.className}`);
    live++;
    maxLive = Math.max(maxLive, live);
    const destroy = f.h.destroy;
    f.h.destroy = vi.fn(async () => {
      live--;
      await destroy();
    });
    return f.h;
  }),
}));
vi.mock("@parasma/core", () => ({
  sniffKind: (b: Uint8Array) => (b[0] === 0x50 && b[1] === 0x4b ? "word" : null),
  UnsupportedFormatError: class UnsupportedFormatError extends Error {
    constructor(name?: string) {
      super(`${name ?? "bytes"} is not a previewable format`);
      this.name = "UnsupportedFormatError";
    }
  },
}));

import { useParasma, type UseParasmaResult } from "./use-parasma";
import { ParasmaViewer } from "./parasma-viewer";

const zipBytes = () => new Uint8Array([0x50, 0x4b, 0x03, 0x04, 1, 2, 3]);
const junkBytes = () => new Uint8Array([1, 2, 3, 4]);

let container: HTMLElement;
let root: Root;
async function render(node: ReactNode) {
  await act(async () => root.render(node));
}
async function settle() {
  await act(async () => {
    for (let i = 0; i < 6; i++) await Promise.resolve();
  });
}

/** Renders the hook into a stage and exposes the latest result. */
function Probe(props: { onResult: (p: UseParasmaResult) => void }) {
  const p = useParasma();
  props.onResult(p);
  return createElement("div", { ref: p.stageRef, className: "stage" });
}

beforeEach(() => {
  log = [];
  fakes = [];
  live = 0;
  maxLive = 0;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
});

describe("useParasma", () => {
  it("boots once the stage attaches and exposes the handle", async () => {
    let p!: UseParasmaResult;
    await render(createElement(Probe, { onResult: (r) => (p = r) }));
    await settle();
    expect(p.isReady).toBe(true);
    expect(p.handle).toBe(fakes[0].h);
    await expect(p.ready).resolves.toBe(fakes[0].h);
    expect(log[0]).toBe("boot:0:stage");
  });

  it("StrictMode: the second boot waits for the first viewer's teardown", async () => {
    let p!: UseParasmaResult;
    await render(createElement(StrictMode, null, createElement(Probe, { onResult: (r) => (p = r) })));
    await settle();
    expect(maxLive).toBe(1);
    expect(live).toBe(1);
    expect(p.handle).toBe(fakes[fakes.length - 1].h);
  });

  it("status: idle → loading → ready, and events re-render the snapshot", async () => {
    let p!: UseParasmaResult;
    await render(createElement(Probe, { onResult: (r) => (p = r) }));
    await settle();
    expect(p.status).toBe("idle");
    let loading!: Promise<void>;
    await act(async () => {
      loading = p.load(zipBytes(), "a.docx");
    });
    expect(p.status).toBe("loading");
    await act(async () => {
      fakes[0].finishLoad();
      await loading;
    });
    expect(p.status).toBe("ready");
    await act(async () => fakes[0].emit("pageChange", 2));
    expect(p.currentPage).toBe(2);
    await act(async () =>
      fakes[0].busEmit("sheets:changed", { sheets: [{ name: "S1", pageCount: 4 }], active: 0 }),
    );
    expect(p.sheets).toEqual([{ name: "S1", pageCount: 4 }]);
  });

  it("a superseded load never lands its tail", async () => {
    let p!: UseParasmaResult;
    await render(createElement(Probe, { onResult: (r) => (p = r) }));
    await settle();
    let first!: Promise<void>;
    let second!: Promise<void>;
    await act(async () => {
      first = p.load(zipBytes());
      second = p.load(zipBytes());
      await Promise.resolve();
    });
    await act(async () => {
      fakes[0].failLoad(new Error("first failed late"));
      await first.catch(() => {});
    });
    expect(p.status).toBe("loading");
    await act(async () => {
      fakes[0].finishLoad();
      await second;
    });
    expect(p.status).toBe("ready");
    expect(p.error).toBeNull();
  });

  it("unsupported bytes reject before the engine and set status error", async () => {
    let p!: UseParasmaResult;
    await render(createElement(Probe, { onResult: (r) => (p = r) }));
    await settle();
    await act(async () => {
      await expect(p.load(junkBytes(), "x.bin")).rejects.toMatchObject({ name: "UnsupportedFormatError" });
    });
    expect(p.status).toBe("error");
    expect(fakes[0].h.load).not.toHaveBeenCalled();
  });

  it("keeps the result's identity until the snapshot or handle changes", async () => {
    const seen: UseParasmaResult[] = [];
    let bump!: () => void;
    function Parent() {
      const [n, setN] = useState(0);
      bump = () => setN(n + 1);
      return createElement(Probe, { onResult: (r) => seen.push(r) });
    }
    await render(createElement(Parent));
    await settle();
    const before = seen[seen.length - 1];
    await act(async () => bump());
    expect(seen[seen.length - 1]).toBe(before);
    await act(async () => fakes[0].emit("pageChange", 2));
    expect(seen[seen.length - 1]).not.toBe(before);
  });

  it("unmount unhooks every subscription before destroying the viewer", async () => {
    await render(createElement(Probe, { onResult: () => {} }));
    await settle();
    await act(async () => root.unmount());
    await settle();
    const destroyAt = log.indexOf("destroy:0");
    expect(destroyAt).toBeGreaterThan(0);
    expect(log.slice(0, destroyAt).filter((l) => l.startsWith("unsub:0:")).length).toBeGreaterThan(10);
    expect(log.slice(destroyAt).some((l) => l.startsWith("unsub:"))).toBe(false);
    root = createRoot(container); // afterEach unmounts again
  });
});

describe("<ParasmaViewer>", () => {
  it("renders an empty stage, loads `source`, and reports ready", async () => {
    const onReady = vi.fn();
    let p: UseParasmaResult | null = null;
    await render(
      createElement(ParasmaViewer, { source: zipBytes(), onReady, ref: (r: UseParasmaResult | null) => void (p = r) }),
    );
    await settle();
    const stage = container.querySelector(".parasma-viewer-stage")!;
    expect(stage.childNodes.length).toBe(0);
    expect(fakes[0].h.load).toHaveBeenCalledTimes(1);
    await act(async () => fakes[0].finishLoad());
    await settle();
    expect(onReady).toHaveBeenCalledTimes(1);
    expect(p!.status).toBe("ready");
  });

  it("a ref that stores the result in state settles instead of looping", async () => {
    let renders = 0;
    let held: UseParasmaResult | null = null;
    function Host() {
      const [p, setP] = useState<UseParasmaResult | null>(null);
      renders++;
      held = p;
      return createElement(ParasmaViewer, { ref: setP });
    }
    await render(createElement(Host));
    await settle();
    expect(held!.isReady).toBe(true);
    expect(renders).toBeLessThan(10);
  });

  it("page prop pushes down; engine page changes echo through onPageChange", async () => {
    const onPageChange = vi.fn();
    await render(createElement(ParasmaViewer, { page: 1, onPageChange }));
    await settle();
    await render(createElement(ParasmaViewer, { page: 3, onPageChange }));
    expect(log).toContain("goToPage:3");
    await act(async () => fakes[0].emit("pageChange", 5));
    expect(onPageChange).toHaveBeenLastCalledWith(5);
  });

  it("forwards linkActivate synchronously so preventDefault() vetoes", async () => {
    const onLinkActivate = vi.fn((e: { preventDefault(): void }) => e.preventDefault());
    await render(createElement(ParasmaViewer, { onLinkActivate }));
    await settle();
    let vetoed = false;
    const ev = { href: "https://example.com", preventDefault: () => (vetoed = true) };
    await act(async () => fakes[0].emit("linkActivate", ev));
    expect(onLinkActivate).toHaveBeenCalledWith(ev);
    expect(vetoed).toBe(true);
  });
});
