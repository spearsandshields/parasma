/**
 * base-layer-bound.test.ts — the `ps-base` retention bound (node env).
 *
 * `ps-base` is the persistent low-res full-page preview that sits UNDER the
 * crisp `ps-slice`. `unpaint()` deliberately keeps it when a page leaves the
 * IntersectionObserver window ("it's cheap"), so without a bound one base existed for
 * EVERY page that ever entered the window and was released only by clear() on a
 * doc switch. At the adapter's own arithmetic (longest side 1024 device px,
 * alpha:false ⇒ 4 B/px) a US-Letter page costs 791×1024×4 = 3 239 936 B, so
 * reading a 200-page docx end-to-end retained ~618 MiB of canvas backing store —
 * an unbounded per-document accumulator — the same memory/eviction problem
 * that was deferred for the tile cache.
 *
 * WHY EVICTION CANNOT CHANGE RENDERED OUTPUT (the constraint this file exists to
 * defend):
 *   1. `ps-base` is a pure, idempotent, re-derivable function of
 *      (pageSizes.get(idx), viewer) — ensureBaseLayer regenerates it on demand.
 *   2. NOTHING reads it. It is z-index 0 under the slice's z-index 1, is never
 *      hit-tested, and no other module in the tree so much as names it.
 *   3. Only OFF-WINDOW pages are evictable. A page outside an IntersectionObserver
 *      window with rootMargin "400px" is ≥400 px from the viewport, so removing
 *      its base changes zero on-screen pixels at the moment of eviction, and
 *      unpaint() has already dropped its crisp slice.
 *   4. handleInvalidate has done remove-then-regenerate on this exact layer in
 *      production since the late-font channel landed — the mechanism is proven.
 * The only observable difference is a transient: a re-entering evicted page shows
 * its blurry preview one renderPage later, the same window that already exists on
 * the FIRST visit to any page.
 *
 * So the tests below pin the two halves of the contract:
 *   - the retained backing store is BOUNDED however many pages are read, and
 *   - a base inside the window is NEVER evicted (that would be the pixel change),
 *     and eviction is non-destructive (re-entry restores an identical base).
 */
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import {
  installGlobals,
  countingScope,
  fakeKernel,
  fakeViewer,
  structuralElement,
  installDrivableObserver,
  primeViewportPages,
  settleTasks,
  type InstalledGlobals,
  type CountingScope,
  type FakeKernel,
  type FakeViewer,
  type StructuralEl,
} from "./leak-harness";
import { DomViewportAdapter } from "./dom-viewport-adapter";
import type { Kernel } from "@parasma/headless";
import type { Viewer } from "@parasma/core";

let g: InstalledGlobals;
let scope: CountingScope;
let kernel: FakeKernel;
let viewer: FakeViewer;

beforeEach(() => {
  g = installGlobals();
  scope = countingScope();
  // The paintWindow/ensureBaseLayer path needs a real child tree, not the
  // teardown gate's inert counting element.
  (scope.doc as unknown as { createElement: (t: string) => StructuralEl }).createElement = (t) =>
    structuralElement(t);
  kernel = fakeKernel();
  viewer = fakeViewer();
});
afterEach(() => g.restore());

function makeAdapter(viewerEl: StructuralEl, pagesContainer: StructuralEl): DomViewportAdapter {
  return new DomViewportAdapter({
    kernel: kernel as unknown as Kernel,
    scope,
    viewer: viewer as unknown as Viewer,
    pagesContainer: pagesContainer as unknown as HTMLElement,
    viewerEl: viewerEl as unknown as HTMLElement,
  });
}

/** Page indices that currently carry a live `ps-base` canvas. */
function basePages(pagesContainer: StructuralEl): number[] {
  return pagesContainer.children
    .filter((c) => c.className === "page" && c.children.some((k) => k.className === "ps-base"))
    .map((p) => Number(p.dataset.page));
}

/** Retained `ps-base` backing store in bytes (4 B/px, alpha:false). */
function baseBytes(pagesContainer: StructuralEl): number {
  let total = 0;
  for (const page of pagesContainer.children) {
    for (const kid of page.children) {
      if (kid.className === "ps-base") total += kid.width * kid.height * 4;
    }
  }
  return total;
}

const MIB = 1024 * 1024;

/** Bring page `idx` into the window and let its first paint (and base) land. */
async function visit(
  io: ReturnType<typeof installDrivableObserver>,
  page: StructuralEl,
  idx: number,
): Promise<void> {
  io.intersect([page], true);
  kernel.emitPaint({ tag: "page", pageIndex: idx });
  g.raf.flush();
  await settleTasks();
}

describe("ps-base retention is bounded", () => {
  it("reading 200 pages with a 3-page window retains a bounded backing store", async () => {
    const io = installDrivableObserver();
    const viewerEl = structuralElement();
    const pagesContainer = structuralElement();
    makeAdapter(viewerEl, pagesContainer);

    const N = 200;
    const WINDOW = 3;
    const pages = await primeViewportPages(kernel, g.raf, pagesContainer, N);
    expect(pages).toHaveLength(N);

    // Sequential read: each page enters the window, paints, and leaves WINDOW
    // pages later — the ordinary scroll-through-a-document trajectory.
    for (let i = 0; i < N; i++) {
      await visit(io, pages[i], i);
      if (i >= WINDOW) io.intersect([pages[i - WINDOW]], false);
    }

    const live = basePages(pagesContainer);
    const bytes = baseBytes(pagesContainer);
    // Every page really did get a base on its visit (the fixture is honest — if
    // ensureBaseLayer never ran, "bounded" would be vacuous).
    expect(pages[N - 1].children.some((k) => k.className === "ps-base")).toBe(true);
    // Per-page cost is the adapter's own arithmetic; unbounded retention would be
    // N × that. MEASURED on this fixture — RED (no bound): 200 bases,
    // 647 987 200 B = 618.0 MiB. GREEN: 11 bases (the 3-page window + the 8-page
    // off-window budget), 35 639 296 B = 34.0 MiB — an 18.2× reduction.
    const perPage = 791 * 1024 * 4;
    expect(bytes).toBeLessThan(64 * MIB);
    expect(bytes).toBeLessThan(N * perPage);
    expect(live.length).toBeLessThan(24);
    // The retained bases are the RECENT ones (LRU), i.e. the tail of the read.
    expect(Math.min(...live)).toBeGreaterThan(N - 24);
  });

  it("never evicts an ON-SCREEN base while over budget (pins the guard, not the early return)", async () => {
    // WHY THIS EXISTS: the sibling test below drives the ALL-VISIBLE case, where
    // `offWindow` is 0 and evictBaseLayers early-returns before the loop. The
    // `if (this.visibleSet.has(idx)) continue` guard is therefore never reached
    // there — deleting it left every test in this file green. This test puts
    // VISIBLE pages at the FRONT of the LRU order (the eviction candidates) with
    // enough off-window pages to push the budget over, so the loop MUST walk
    // past on-screen pages to do its job. Without the guard it evicts them, and
    // evicting an on-screen base changes PIXELS.
    const io = installDrivableObserver();
    const viewerEl = structuralElement();
    const pagesContainer = structuralElement();
    makeAdapter(viewerEl, pagesContainer);

    const N = 20;
    const pages = await primeViewportPages(kernel, g.raf, pagesContainer, N);
    io.intersect(pages, true);
    for (let i = 0; i < N; i++) kernel.emitPaint({ tag: "page", pageIndex: i });
    g.raf.flush();
    await settleTasks();
    expect(basePages(pagesContainer)).toHaveLength(N);

    // Pages 0-4 stay ON SCREEN and are the OLDEST entries (LRU front, i.e. first
    // in line to be evicted). Pages 5-19 go off-window: 15 off-window against a
    // budget of 8 ⇒ 7 must go, and the loop reaches the visible ones first.
    const onScreen = pages.slice(0, 5);
    io.intersect(pages.slice(5), false);
    g.raf.flush();
    await settleTasks();

    const live = basePages(pagesContainer);
    for (let i = 0; i < 5; i++) {
      expect(live, `page ${i} is on screen and must keep its base`).toContain(i);
    }
    // …and the budget was still enforced against the off-window set.
    expect(live.length).toBeLessThan(N);
  });

  it("never evicts a base that is inside the window, however many pages are visible", async () => {
    const io = installDrivableObserver();
    const viewerEl = structuralElement();
    const pagesContainer = structuralElement();
    makeAdapter(viewerEl, pagesContainer);

    // A deep zoom-out puts far more pages in the window at once than the
    // off-window retention budget. Every one of them is on screen, so every one
    // must keep its base — evicting here WOULD change pixels.
    const N = 40;
    const pages = await primeViewportPages(kernel, g.raf, pagesContainer, N);
    io.intersect(pages, true);
    for (let i = 0; i < N; i++) kernel.emitPaint({ tag: "page", pageIndex: i });
    g.raf.flush();
    await settleTasks();
    expect(basePages(pagesContainer)).toHaveLength(N);

    // Zoom back in / scroll away: they all leave the window in one batch, and the
    // budget applies from that moment.
    // MEASURED: 40 bases (123.6 MiB) while visible → 8 (24.7 MiB) once off-window.
    io.intersect(pages, false);
    const after = basePages(pagesContainer);
    expect(after.length).toBeLessThan(N);
    expect(after.length).toBeGreaterThan(0); // a retention floor, not a purge
  });

  it("eviction is non-destructive: a re-entering page gets an identical base back", async () => {
    const io = installDrivableObserver();
    const viewerEl = structuralElement();
    const pagesContainer = structuralElement();
    makeAdapter(viewerEl, pagesContainer);

    const N = 60;
    const pages = await primeViewportPages(kernel, g.raf, pagesContainer, N);
    for (let i = 0; i < N; i++) {
      await visit(io, pages[i], i);
      io.intersect([pages[i]], false);
    }
    // Page 0 is long gone from the budget.
    const evicted = pages[0];
    expect(evicted.children.some((k) => k.className === "ps-base")).toBe(false);

    // Scroll back to the top. ensureBaseLayer is idempotent and re-derivable, so
    // the preview returns at the SAME backing dimensions it had the first time.
    await visit(io, evicted, 0);
    const base = evicted.children.find((k) => k.className === "ps-base");
    expect(base).toBeDefined();
    expect(base?.width).toBe(791);
    expect(base?.height).toBe(1024);
  });
});
