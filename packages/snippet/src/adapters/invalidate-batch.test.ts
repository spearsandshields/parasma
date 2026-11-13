/**
 * invalidate-batch.test.ts — the shared-generation contract for scheduleFlush's
 * intent drains (node env).
 *
 * paintSeq is a GLOBAL latest-wins token: paintWindow captures it at entry and
 * drops its commit if any newer generation opened mid-flight. That makes the
 * batch contract load-bearing: all sibling paints launched by ONE flush must
 * share ONE generation (launchPaintGeneration's own doc: "so all siblings in
 * the batch share the bumped token (they all commit)"). onScroll/onResize honor
 * it by bumping once around their loop.
 *
 * RED (the bug this pins): the flush drains called handleInvalidate / paint()
 * PER PAGE, each opening its OWN generation — so with 2+ visible pages every
 * page but the LAST was superseded while suspended at `await renderPageWindow`
 * and dropped its freshly rendered buffer. User-visible: the selection
 * highlight (plugin-selection repaints via viewport.invalidateVisible) never
 * painted on the earlier visible page — selection worked (copy OK) with no UI
 * feedback. Same channel ⇒ same loss for late-font/late-image repaints.
 *
 * SECOND RED (the other direction of the same token): a launcher that
 * decides to paint NOTHING must not open a generation at all. launchPaintGeneration
 * bumps BEFORE its closure filters, so onScroll (dedup skips every page — the
 * majority of scroll frames) and onResize (every page filtered out) used to open an
 * EMPTY generation that superseded every in-flight paint with nothing queued to redo
 * them. The suspended late-font/late-image repaint then resolved and was dropped
 * forever; the crisp ps-slice kept its pre-asset pixels while only the blurry
 * ps-base under it refreshed.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
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
  // Swap the harness's countingElement factory for the structural fake the
  // paintWindow commit path needs (insertBefore/firstChild/querySelector).
  (scope.doc as unknown as { createElement: (t: string) => StructuralEl }).createElement = (t: string) => structuralElement(t);
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

/**
 * Prime placeholders for pages 0..n-1 (shared driver) and assert the count, so a
 * primer regression surfaces here rather than as a confusing downstream failure.
 */
async function primePlaceholders(pagesContainer: StructuralEl, n: number): Promise<StructuralEl[]> {
  const pages = await primeViewportPages(kernel, g.raf, pagesContainer, n);
  expect(pages).toHaveLength(n);
  return pages;
}

describe("scheduleFlush batch generations (shared-token contract)", () => {
  it("a page-invalidate batch over 2 visible pages commits BOTH slice repaints (selection-highlight bug)", async () => {
    const viewerEl = structuralElement();
    const pagesContainer = structuralElement();
    makeAdapter(viewerEl, pagesContainer);
    const [page0, page1] = await primePlaceholders(pagesContainer, 2);

    // The plugin-selection repaint contract: viewport.invalidateVisible emits
    // one 'page-invalidate' intent per visible page — ONE flush batch.
    kernel.emitPaint({ tag: "page-invalidate", pageIndex: 0 });
    kernel.emitPaint({ tag: "page-invalidate", pageIndex: 1 });
    g.raf.flush();
    await settleTasks();

    // Both suspended paintWindows must resume under the SAME generation and
    // commit. RED: per-page generations → page 0 superseded by page 1's bump
    // mid-await → its ps-slice never lands (the highlight never paints).
    expect(viewer.renderPageWindow).toHaveBeenCalledTimes(2);
    expect(page1.querySelector("canvas.ps-slice")).not.toBeNull();
    expect(page0.querySelector("canvas.ps-slice")).not.toBeNull();
  });

  it("a 'page' intent batch over 2 visible pages commits BOTH first paints", async () => {
    const viewerEl = structuralElement();
    const pagesContainer = structuralElement();
    makeAdapter(viewerEl, pagesContainer);
    const [page0, page1] = await primePlaceholders(pagesContainer, 2);

    // First-paint path only fires for on-screen pages; the IO double never
    // delivers entries, so mark visibility the way onIntersect would.
    page0.dataset.visible = "1";
    page1.dataset.visible = "1";
    kernel.emitPaint({ tag: "page", pageIndex: 0 });
    kernel.emitPaint({ tag: "page", pageIndex: 1 });
    g.raf.flush();
    await settleTasks();

    expect(page1.querySelector("canvas.ps-slice")).not.toBeNull();
    expect(page0.querySelector("canvas.ps-slice")).not.toBeNull();
  });
});

describe("empty paint generations (a launcher that paints nothing must not supersede)", () => {
  /**
   * Bring both pages to a COMMITTED first paint with the IO driven, so that
   *  - visibleSet holds {0,1} (what onScroll/onResize iterate), and
   *  - sliceCoverage holds a full-page-box rect for each (l/t at 0, r/b at the
   *    page edges), which is exactly the "every side pinned to the page boundary"
   *    case onScroll's overscan dedup skips.
   * Then hold renderPageWindow open and remove the committed slices so that a
   * LATER commit is structurally observable.
   */
  async function armSuspendedInvalidate(): Promise<{
    viewerEl: StructuralEl;
    page0: StructuralEl;
    page1: StructuralEl;
    releases: Array<() => void>;
    io: ReturnType<typeof installDrivableObserver>;
  }> {
    const io = installDrivableObserver();
    const viewerEl = structuralElement();
    const pagesContainer = structuralElement();
    makeAdapter(viewerEl, pagesContainer);
    const [page0, page1] = await primePlaceholders(pagesContainer, 2);
    io.intersect([page0, page1]);

    kernel.emitPaint({ tag: "page", pageIndex: 0 });
    kernel.emitPaint({ tag: "page", pageIndex: 1 });
    g.raf.flush();
    await settleTasks();
    // Precondition: both first paints committed (coverage now recorded).
    expect(page0.querySelector("canvas.ps-slice")).not.toBeNull();
    expect(page1.querySelector("canvas.ps-slice")).not.toBeNull();

    // From here renderPageWindow HANGS until the test releases it — the worker-mode
    // RPC that a scroll can land inside.
    const releases: Array<() => void> = [];
    viewer.renderPageWindow = vi.fn(
      () => new Promise<boolean>((res) => releases.push(() => res(true))),
    );
    page0.querySelector("canvas.ps-slice")?.remove();
    page1.querySelector("canvas.ps-slice")?.remove();

    // The late-font / late-image / selection channel: ONE flush batch, both
    // paintWindows suspend at `await renderPageWindow`.
    kernel.emitPaint({ tag: "page-invalidate", pageIndex: 0 });
    kernel.emitPaint({ tag: "page-invalidate", pageIndex: 1 });
    g.raf.flush();
    await settleTasks();
    expect(releases).toHaveLength(2);
    return { viewerEl, page0, page1, releases, io };
  }

  it("a scroll frame the overscan dedup skips entirely does NOT drop the in-flight repaint", async () => {
    const { viewerEl, page0, page1, releases } = await armSuspendedInvalidate();

    // A scroll while the repaint is mid-RPC. The recorded coverage spans the whole
    // page box on every side, so the dedup `continue`s past BOTH pages — the
    // majority-of-frames case. RED: launchPaintGeneration bumped anyway.
    viewerEl.dispatch("scroll");
    g.raf.flush();
    await settleTasks();
    expect(releases).toHaveLength(2); // the scroll launched ZERO paints

    for (const r of releases) r();
    await settleTasks();
    // RED: both resolve superseded by the empty generation and drop, with nothing
    // scheduled to retry — the crisp slice never comes back.
    expect(page0.querySelector("canvas.ps-slice")).not.toBeNull();
    expect(page1.querySelector("canvas.ps-slice")).not.toBeNull();
  });

  it("a resize frame that filters out every page does NOT drop the in-flight repaint", async () => {
    const { page0, page1, releases } = await armSuspendedInvalidate();

    // Pages detached mid-flight (visibleSet churn / doc switch): onResize's
    // `!el?.isConnected` filter admits none, so its batch is empty.
    page0.isConnected = false;
    page1.isConnected = false;
    (scope.win as unknown as { dispatch(t: string): void }).dispatch("resize");
    g.raf.flush();
    await settleTasks();
    expect(releases).toHaveLength(2); // the resize launched ZERO paints

    // Re-attach before the release so the commit's own isConnected gate passes and
    // the ONLY thing under test is the generation token.
    page0.isConnected = true;
    page1.isConnected = true;
    for (const r of releases) r();
    await settleTasks();
    expect(page0.querySelector("canvas.ps-slice")).not.toBeNull();
    expect(page1.querySelector("canvas.ps-slice")).not.toBeNull();
  });
});
