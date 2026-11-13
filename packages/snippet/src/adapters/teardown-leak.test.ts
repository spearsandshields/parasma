/**
 * teardown-leak.test.ts — the per-instance no-leak gate, node env.
 *
 * For each adapter: installGlobals(); construct over per-instance counting seams;
 * EXERCISE the real interaction (so an over-eager teardown that rips out a
 * still-needed rAF/listener fails the PRE-destroy assertion, not just the
 * post-destroy zero); snapshot; destroy(); assert the per-instance deltas are
 * ZERO — listeners removed, observers disconnected, rAF a post-destroy no-op,
 * timers cleared, subscriptions released — and destroy() is idempotent.
 *
 * RED (the leaks this gate guards against):
 *   - viewport: a post-destroy scheduleFlush rAF fires into the viewer.
 *   - grid: the 4 gridCanvasEl pointer listeners survive destroy; the
 *     zoom-anchor timer is not cleared.
 */
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import {
  installGlobals,
  countingScope,
  countingElement,
  fakeKernel,
  fakeViewer,
  type InstalledGlobals,
  type CountingScope,
  type CountingElement,
  type FakeKernel,
  type FakeViewer,
} from "./leak-harness";
import { DomViewportAdapter } from "./dom-viewport-adapter";
import { DomGridAdapter } from "./dom-grid-adapter";
import type { Kernel } from "@parasma/headless";
import type { Viewer } from "@parasma/core";

let g: InstalledGlobals;
let scope: CountingScope;
let kernel: FakeKernel;
let viewer: FakeViewer;

beforeEach(() => {
  g = installGlobals();
  scope = countingScope();
  kernel = fakeKernel();
  viewer = fakeViewer();
});
afterEach(() => g.restore());

const asKernel = (k: FakeKernel): Kernel => k as unknown as Kernel;
const asViewer = (v: FakeViewer): Viewer => v as unknown as Viewer;

describe("DomViewportAdapter teardown", () => {
  function make(pagesContainer: CountingElement, viewerEl: CountingElement) {
    return new DomViewportAdapter({
      kernel: asKernel(kernel),
      scope,
      viewer: asViewer(viewer),
      pagesContainer: pagesContainer as unknown as HTMLElement,
      viewerEl: viewerEl as unknown as HTMLElement,
    });
  }

  it("releases listeners, observer, rAF and subscriptions; post-destroy flush is a no-op", () => {
    const pages = countingElement();
    const viewerEl = countingElement();
    const adapter = make(pages, viewerEl);

    // Construction acquisitions.
    expect(scope.win.liveFor("resize")).toBe(1);
    expect(viewerEl.liveFor("scroll")).toBe(1);
    expect(g.liveObservers()).toBe(1);
    expect(kernel.liveSubs()).toBe(2); // onPaintIntent + loadComplete

    // EXERCISE: a 'page' paint intent schedules a flush rAF (the live path).
    kernel.emitPaint({ tag: "page", pageIndex: 0 });
    expect(g.raf.pending()).toBe(1);

    // Prove the live flush reaches the viewer (cold page-size cache → primePageSize).
    const before = viewer.pageSize.mock.calls.length;
    // (do NOT flush here — we flush AFTER destroy to prove the guard.)
    expect(before).toBe(0);

    adapter.destroy();

    // Listeners/observer/subscriptions all released.
    expect(scope.win.live()).toBe(0);
    expect(viewerEl.live()).toBe(0);
    expect(g.liveObservers()).toBe(0);
    expect(kernel.liveSubs()).toBe(0);

    // The in-flight rAF is still queued; flushing it MUST NOT touch the viewer
    // (RED without the `destroyed` guard: handlePageIntent → primePageSize →
    // viewer.pageSize()).
    expect(() => g.raf.flush()).not.toThrow();
    expect(viewer.pageSize).not.toHaveBeenCalled();
    expect(viewer.callsWhileDestroyed()).toBe(0);
  });

  it("is idempotent: double destroy is a safe no-op", () => {
    const adapter = make(countingElement(), countingElement());
    kernel.emitPaint({ tag: "page", pageIndex: 0 });
    adapter.destroy();
    expect(() => adapter.destroy()).not.toThrow();
    expect(scope.win.live()).toBe(0);
    expect(g.liveObservers()).toBe(0);
    expect(kernel.liveSubs()).toBe(0);
    g.raf.flush();
    expect(viewer.pageSize).not.toHaveBeenCalled();
  });

  it("live behavior intact: pre-destroy flush DOES paint (guards over-removal)", () => {
    make(countingElement(), countingElement());
    kernel.emitPaint({ tag: "page", pageIndex: 0 });
    g.raf.flush(); // live, not destroyed
    expect(viewer.pageSize).toHaveBeenCalled();
  });
});

describe("DomGridAdapter teardown", () => {
  function make() {
    const gridviewEl = countingElement();
    const gridCanvasEl = countingElement();
    const gridSpacerEl = countingElement();
    const viewerEl = countingElement();
    const adapter = new DomGridAdapter({
      kernel: asKernel(kernel),
      scope,
      viewer: asViewer(viewer),
      gridviewEl: gridviewEl as unknown as HTMLElement,
      gridCanvasEl: gridCanvasEl as unknown as HTMLCanvasElement,
      gridSpacerEl: gridSpacerEl as unknown as HTMLElement,
      viewerEl: viewerEl as unknown as HTMLElement,
    });
    return { adapter, gridviewEl, gridCanvasEl };
  }

  it("removes the 4 gridCanvas pointer listeners + gridview scroll + observer", () => {
    const { adapter, gridviewEl, gridCanvasEl } = make();

    // Construction acquisitions.
    expect(gridviewEl.liveFor("scroll")).toBe(1);
    expect(gridCanvasEl.live()).toBe(4); // pointer down/move/up/cancel
    expect(g.liveObservers()).toBe(1); // ResizeObserver

    // EXERCISE: enter grid mode, then a pointer-down begins a cell selection
    // (proves the pointer wiring still fires — over-removal would break it).
    kernel.emitBus("grid:mode-changed", { mode: "grid" });
    gridCanvasEl.dispatch("pointerdown", { button: 0, clientX: 10, clientY: 10, pointerId: 1, shiftKey: false });
    expect(kernel.exec).toHaveBeenCalledWith("selection.begin", expect.anything());

    adapter.destroy();

    // RED without the fix: gridCanvasEl.live() === 4 (never removed).
    expect(gridCanvasEl.live()).toBe(0);
    expect(gridviewEl.live()).toBe(0);
    expect(g.liveObservers()).toBe(0);
    expect(kernel.liveSubs()).toBe(0);
  });

  it("clears the zoom-anchor timer on destroy", () => {
    const { adapter, gridCanvasEl } = make();
    kernel.emitBus("grid:mode-changed", { mode: "grid" });
    // Schedule the 200ms zoom-anchor debounce.
    adapter.setZoomAnchor(10, 10);
    void gridCanvasEl; // (anchor reads gridCanvasEl rect; keep it referenced)
    expect(scope.pendingTimers()).toBe(1);

    adapter.destroy();
    // RED without the fix: destroy() does not clear zoomAnchorTimer.
    expect(scope.pendingTimers()).toBe(0);
  });

  it("post-destroy scroll rAF is a no-op (reference guard held)", () => {
    const { adapter, gridviewEl } = make();
    kernel.emitBus("grid:mode-changed", { mode: "grid" });
    gridviewEl.dispatch("scroll");
    expect(g.raf.pending()).toBe(1);
    adapter.destroy();
    expect(() => g.raf.flush()).not.toThrow();
    expect(viewer.paintGridViewport).not.toHaveBeenCalled();
  });

  it("is idempotent: double destroy is a safe no-op", () => {
    const { adapter } = make();
    adapter.destroy();
    expect(() => adapter.destroy()).not.toThrow();
    expect(kernel.liveSubs()).toBe(0);
    expect(g.liveObservers()).toBe(0);
  });
});
