import { describe, it, expect, vi, beforeEach } from "vitest";
import { effectScope, ref, nextTick } from "vue";

/**
 * Node-env unit tests over a mocked `@parasma/snippet/headless` (the sibling
 * posture: hand-built fakes, no DOM shim that could mask a stray global read).
 * What's under test is the CONTRACT this package adds on top of the handle:
 * the machine-grade status transitions, supersede suppression, boot/teardown
 * ordering, and event→ref wiring.
 */

// ── the fake handle ──────────────────────────────────────────────────────────
type Handler = (...a: unknown[]) => void;
function makeFakeHandle() {
  const on = new Map<string, Set<Handler>>();
  const bus = new Map<string, Set<Handler>>();
  const cellCbs = new Set<Handler>();
  const calls: string[] = [];
  const loadResolvers: Array<{ res: () => void; rej: (e: unknown) => void }> = [];
  const h = {
    state: { documentKind: "word", currentPage: 1, pageCount: 0, zoom: 1 },
    on(ev: string, cb: Handler) {
      if (!on.has(ev)) on.set(ev, new Set());
      on.get(ev)!.add(cb);
      return () => {
        calls.push(`unsub:${ev}`);
        on.get(ev)!.delete(cb);
      };
    },
    kernel: {
      bus: {
        subscribe(ev: string, cb: Handler) {
          if (!bus.has(ev)) bus.set(ev, new Set());
          bus.get(ev)!.add(cb);
          return () => {
            calls.push(`unsub:${ev}`);
            bus.get(ev)!.delete(cb);
          };
        },
      },
      exec: (...a: unknown[]) => {
        calls.push(`exec:${a.filter((x) => typeof x === "string").join(":")}`);
        return Promise.resolve();
      },
    },
    dom: {
      onCellSelect(cb: Handler) {
        cellCbs.add(cb);
        return () => calls.push("unsub:cellSelect");
      },
    },
    zoom: {
      in: () => calls.push("zoom.in"),
      out: () => calls.push("zoom.out"),
      set: (s: number) => calls.push(`zoom.set:${s}`),
      fit: (m: string) => calls.push(`zoom.fit:${m}`),
    },
    print: vi.fn(async () => calls.push("print")),
    goToPage: (n: number) => calls.push(`goToPage:${n}`),
    selectSheet: (i: number) => calls.push(`selectSheet:${i}`),
    scrollToGridContent: (x: number, y: number) => calls.push(`scrollToGridContent:${x},${y}`),
    scrollToAnchorPos: (pg: number, y: number) => calls.push(`scrollToAnchorPos:${pg},${y}`),
    load: vi.fn(
      () =>
        new Promise<void>((res, rej) => {
          loadResolvers.push({ res, rej });
        }),
    ),
    unload: vi.fn(async () => {}),
    destroy: vi.fn(async () => {
      calls.push("destroy");
    }),
  };
  return {
    h,
    calls,
    emit: (ev: string, ...a: unknown[]) => on.get(ev)?.forEach((cb) => cb(...a)),
    busEmit: (ev: string, ...a: unknown[]) => bus.get(ev)?.forEach((cb) => cb(...a)),
    // The cell feed arrives via the promoted handle event.
    cellEmit: (c: unknown) => on.get("cellSelect")?.forEach((cb) => cb(c)),
    // FIFO: each call settles the OLDEST outstanding load — so "finish the
    // first" means the first (the initial fake aliased a mutable resolver and
    // silently settled the latest, breaking the supersede test's premise).
    finishLoad: () => loadResolvers.shift()?.res(),
    failLoad: (e: unknown) => loadResolvers.shift()?.rej(e),
  };
}

let fake = makeFakeHandle();
let bootGate: Promise<void> = Promise.resolve();
vi.mock("@parasma/snippet/headless", async () => ({
  // The real session (framework-neutral, imports only the mocked core).
  createViewerSession: (await import("../../snippet/src/viewer-session")).createViewerSession,
  createHeadlessViewer: vi.fn(async () => {
    await bootGate;
    return fake.h;
  }),
}));
vi.mock("@parasma/core", () => ({
  // Real-shaped sniff: ZIP magic = supported; anything else = null.
  sniffKind: (b: Uint8Array) => (b[0] === 0x50 && b[1] === 0x4b ? "word" : null),
  UnsupportedFormatError: class UnsupportedFormatError extends Error {
    constructor(name?: string) {
      super(`${name ?? "bytes"} is not a previewable format`);
      this.name = "UnsupportedFormatError";
    }
  },
}));

import { useParasma } from "./use-parasma";
import { UnsupportedFormatError } from "@parasma/core";

const zipBytes = () => new Uint8Array([0x50, 0x4b, 0x03, 0x04, 1, 2, 3]);
const junkBytes = () => new Uint8Array([1, 2, 3, 4]);
const fakeEl = () => ({ ownerDocument: {} }) as unknown as HTMLElement;

async function flushMicro(n = 4) {
  for (let i = 0; i < n; i++) await Promise.resolve();
  await nextTick();
}

beforeEach(() => {
  fake = makeFakeHandle();
  bootGate = Promise.resolve();
});

describe("useParasma", () => {
  it("boots in a bare effectScope (no component instance) and exposes the handle", async () => {
    const scope = effectScope();
    const p = scope.run(() => useParasma(ref(fakeEl())))!;
    expect(p.isReady.value).toBe(false);
    await flushMicro();
    expect(p.isReady.value).toBe(true);
    expect(p.handle.value).toBe(fake.h);
    await expect(p.ready).resolves.toBe(fake.h);
    scope.stop();
  });

  it("disposed-during-boot: reclaims the handle, never wires, handle stays null", async () => {
    let openGate!: () => void;
    bootGate = new Promise((res) => (openGate = res));
    const scope = effectScope();
    const p = scope.run(() => useParasma(ref(fakeEl())))!;
    scope.stop(); // unmount before boot resolves
    openGate();
    await flushMicro();
    expect(fake.h.destroy).toHaveBeenCalledTimes(1);
    expect(p.handle.value).toBe(null);
  });

  it("status machine: sniff-reject is a typed error BEFORE the engine; success walks idle→loading→ready", async () => {
    const scope = effectScope();
    const p = scope.run(() => useParasma(ref(fakeEl())))!;
    await flushMicro();

    await expect(p.load(junkBytes(), "x.bin")).rejects.toBeInstanceOf(UnsupportedFormatError);
    expect(p.status.value).toBe("error");
    expect(fake.h.load).not.toHaveBeenCalled();

    const done = p.load(zipBytes(), "a.docx");
    await flushMicro();
    expect(p.status.value).toBe("loading");
    expect(p.error.value).toBe(null); // a new load clears the old error
    fake.finishLoad();
    await done;
    expect(p.status.value).toBe("ready");
    scope.stop();
  });

  it("supersede: the FIRST load's late resolution must not flip status (no stale tail)", async () => {
    const scope = effectScope();
    const p = scope.run(() => useParasma(ref(fakeEl())))!;
    await flushMicro();

    void p.load(zipBytes()); // load #1
    await flushMicro();
    const second = p.load(zipBytes()); // load #2 supersedes
    await flushMicro();
    fake.finishLoad(); // FIFO: settles #1 — LATE, already superseded
    await flushMicro();
    expect(p.status.value).toBe("loading"); // #1's "ready" was suppressed
    fake.finishLoad();
    await second;
    expect(p.status.value).toBe("ready");
    scope.stop();
  });

  it("event→ref wiring: streaming pageCount, pageChange, truncation, cell feed", async () => {
    const scope = effectScope();
    const p = scope.run(() => useParasma(ref(fakeEl())))!;
    await flushMicro();

    fake.h.state.pageCount = 7;
    fake.busEmit("pageReady", 6);
    expect(p.pageCount.value).toBe(7); // streaming-aware (reads live state)
    fake.busEmit("loadComplete", 9);
    expect(p.pageCount.value).toBe(9);
    fake.emit("pageChange", 4);
    expect(p.currentPage.value).toBe(4);
    fake.emit("layoutTruncated", 812);
    expect(p.truncatedAt.value).toBe(812);
    fake.cellEmit({ ref: "B2", raw: "", display: "" });
    expect(p.cell.value).toEqual({ ref: "B2", raw: "", display: "" });
    fake.busEmit("sheets:changed", { sheets: [{ name: "S1", pageCount: 2 }], active: 0 });
    expect(p.cell.value).toBe(null); // sheet switch clears the cell feed
    expect(p.tool.value).toBe("select"); // the default base mode
    fake.busEmit("tool:mode-changed", { mode: "hand" });
    expect(p.tool.value).toBe("hand");
    fake.emit("printProgress", { done: 2, total: 5 });
    expect(p.printProgress.value).toEqual({ done: 2, total: 5 });
    fake.emit("printProgress", { done: 5, total: 5 }); // completion → idle
    expect(p.printProgress.value).toBe(null);
    scope.stop();
  });

  it("search: bus state condenses to the counter shape; commands reach the kernel", async () => {
    const scope = effectScope();
    const p = scope.run(() => useParasma(ref(fakeEl())))!;
    await flushMicro();

    expect(p.search.value).toEqual({ query: "", total: 0, activeIdx: null, status: "idle", scope: "sheet" });
    fake.busEmit("search:changed", {
      query: "foo",
      results: [{ page: 0 }, { page: 1 }, { page: 2 }],
      activeIdx: 1,
      status: "done",
      scope: "workbook",
    });
    expect(p.search.value).toEqual({ query: "foo", total: 3, activeIdx: 1, status: "done", scope: "workbook" });

    p.commands.search("bar");
    p.commands.searchNext();
    p.commands.searchPrev();
    p.commands.setSearchScope("workbook");
    p.commands.setTool("hand");
    p.commands.toggleTool();
    await p.commands.print({ to: 2 });
    expect(fake.calls).toEqual(
      expect.arrayContaining([
        "exec:search.query:bar",
        "exec:search.next",
        "exec:search.prev",
        "exec:search.setScope:workbook",
        "exec:tool.setMode:hand",
        "exec:tool.toggle",
        "print",
      ]),
    );
    expect(fake.h.print).toHaveBeenCalledWith({ to: 2 });

    await p.unload();
    expect(p.search.value).toEqual({ query: "", total: 0, activeIdx: null, status: "idle", scope: "sheet" });
    scope.stop();
  });

  it("search: equal-value streaming ticks do NOT retrigger reactivity (identity-stable)", async () => {
    const scope = effectScope();
    const p = scope.run(() => useParasma(ref(fakeEl())))!;
    await flushMicro();

    // The chunked loop emits once per page tick; ticks that found nothing new
    // re-emit equal values. The ref must keep the SAME object then — an
    // identity flip is exactly what would run every consumer effect per tick.
    const tick = () =>
      fake.busEmit("search:changed", {
        query: "q",
        results: [{ page: 0 }],
        activeIdx: 0,
        status: "searching",
        scope: "sheet",
      });
    tick();
    const first = p.search.value;
    expect(first.total).toBe(1);
    tick();
    tick();
    expect(p.search.value).toBe(first); // same object: no trigger happened
    fake.busEmit("search:changed", {
      query: "q",
      results: [{ page: 0 }, { page: 4 }],
      activeIdx: 0,
      status: "searching",
      scope: "sheet",
    });
    expect(p.search.value).not.toBe(first);
    expect(p.search.value.total).toBe(2);
    scope.stop();
  });

  it("teardown ORDER: every unsubscribe runs BEFORE destroy (unhook-while-alive)", async () => {
    const scope = effectScope();
    scope.run(() => useParasma(ref(fakeEl())));
    await flushMicro();
    scope.stop();
    await flushMicro();
    const di = fake.calls.indexOf("destroy");
    expect(di).toBeGreaterThan(0);
    for (let i = 0; i < di; i++) expect(fake.calls[i]).toMatch(/^unsub:/);
    expect(fake.calls.filter((c) => c.startsWith("unsub:")).length).toBeGreaterThanOrEqual(10);
  });

  it("commands + viewState round-trip through the handle", async () => {
    const scope = effectScope();
    const p = scope.run(() => useParasma(ref(fakeEl())))!;
    await flushMicro();
    p.commands.zoomTo(1.5);
    p.commands.goToPage(3);
    p.commands.scrollToGridContent(120, 4500);
    p.commands.scrollToAnchorPos(2, 300);
    p.applyViewState({ sheet: 2, zoom: 2, page: 5, gridMode: "pages" });
    expect(fake.calls).toEqual(
      expect.arrayContaining([
        "zoom.set:1.5",
        "goToPage:3",
        "scrollToGridContent:120,4500",
        "scrollToAnchorPos:2,300",
        "exec:grid.setMode",
        "selectSheet:2",
        "zoom.set:2",
        "goToPage:5",
      ]),
    );
    fake.emit("zoomChange", 2);
    fake.emit("pageChange", 5);
    fake.busEmit("grid:mode-changed", { mode: "grid" });
    expect(p.viewState()).toMatchObject({ zoom: 2, page: 5, gridMode: "grid" });
    scope.stop();
  });
});
