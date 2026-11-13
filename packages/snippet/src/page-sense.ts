/**
 * page-sense — the ONE DOM current-page tracker (kernel-truth current page).
 *
 * Kernel truth: the viewport PLUGIN owns the current-page MODEL
 * (state + `viewport:page-changed` + `viewport.goToPage`); DOM sensing lives
 * HERE, in the snippet adapter layer, as the single tracker feeding the kernel
 * via `viewport.setCurrentPage`. The toolbar is a pure consumer; BYO chrome
 * receives the public `pageChange` with no toolbar mounted.
 *
 * Two exports:
 *  - {@link pickCurrentPage}: the PURE heuristic (the pre-extraction toolbar
 *    rule, verbatim) — node-testable.
 *  - {@link createPageSensor}: the DOM sensor factory — scroll-listener +
 *    rAF-coalesced sensing + the kernel-echo mirror + the scroll-to-page
 *    consumer. Extracted (rather than inlined in attach-dom) so its dispose()
 *    is unit-pinned (a tracker inside the toolbar once fired a rAF after destroy).
 *
 * CONVENTION: everything here is 0-BASED (kernel space). The 1-based tier is
 * only the public snippet surface (attach-dom's DomBinding.currentPage adds 1).
 */
import type { Kernel } from "@parasma/headless";

/** One page placeholder's sensing datum: index + top offset (CSS px) + hidden. */
export interface PageBand {
  /** 0-based page index (`.page[data-page]`). */
  index: number;
  /** pageRect.top − viewerRect.top in CSS px. */
  top: number;
  /** True when the placeholder is display:none (sheet page filter). */
  hidden: boolean;
}

/**
 * The topmost-visible-page heuristic (the toolbar tracker's rule, verbatim):
 * among non-hidden bands with `top <= 80`, pick the smallest |top|; if none
 * qualify, return `current` (sticky).
 *
 * PRECONDITION: bands are in document order (ascending top — the vertical page
 * stack), so a truncated prefix ending at the first band with top > 80 yields
 * the same pick as the full list. This backs the sensor's O(visible)
 * early-break; if a future layout breaks the monotonicity, drop the break in
 * the sensor — not this heuristic.
 */
export function pickCurrentPage(bands: PageBand[], current: number): number {
  let best = current;
  let bestDist = Infinity;
  for (const b of bands) {
    if (b.hidden) continue;
    const dist = Math.abs(b.top);
    // Prefer pages at or just below the top edge.
    if (b.top <= 80 && dist < bestDist) {
      bestDist = dist;
      best = b.index;
    }
  }
  return best;
}

export interface PageSensorDeps {
  /** The paged scroll root (`.ps-viewer`) — the rect origin + scroll source. */
  viewerEl: HTMLElement;
  /** The `.ps-pages` container the `.page[data-page]` placeholders live in. */
  pagesContainer: HTMLElement;
  /** Dispatch the sensed index into the kernel (wraps `void kernel.exec`). */
  exec: (id: "viewport.setCurrentPage", index: number) => void;
  subscribe: Kernel["bus"]["subscribe"];
  /** True suppresses sensing entirely (grid mode hides the paged surface). */
  suspended: () => boolean;
  /** Injectable for node tests; defaults to the global rAF pair. */
  raf?: (cb: FrameRequestCallback) => number;
  caf?: (h: number) => void;
}

export interface PageSensor {
  /** rAF-coalesced sense request (piggybacked on existing bus subs + scroll). */
  schedule(): void;
  /** Kernel-truth mirror, 0-based (updated ONLY by the bus echo / reset()). */
  current(): number;
  /** Zero the mirror (doc switch — `viewport.reset` is deliberately silent). */
  reset(): void;
  /**
   * Scroll listener off, pending rAF cancelled, both bus subs released;
   * post-dispose schedule() is a no-op.
   */
  dispose(): void;
}

export function createPageSensor(deps: PageSensorDeps): PageSensor {
  const raf = deps.raf ?? ((cb: FrameRequestCallback) => requestAnimationFrame(cb));
  const caf = deps.caf ?? ((h: number) => cancelAnimationFrame(h));

  // The kernel-truth mirror. The bus echo below is the ONLY writer besides
  // reset() — senseNow never sets it, so goToPage-driven changes (which also
  // echo) don't fight the plugin's dedup.
  let sensedPage = 0;
  let rafHandle = 0;
  let disposed = false;

  function senseNow(): void {
    // GRID GUARD FIRST: grid mode sets the paged surface display:none →
    // all-zero rects would elect band 0 and leak a phantom pageChange(1).
    if (deps.suspended()) return;
    const viewerTop = deps.viewerEl.getBoundingClientRect().top;
    const bands: PageBand[] = [];
    for (const pageEl of deps.pagesContainer.querySelectorAll<HTMLElement>(".page")) {
      // Hidden check BEFORE the rect read (hidden costs no layout).
      if (pageEl.style.display === "none") continue;
      const top = pageEl.getBoundingClientRect().top - viewerTop;
      bands.push({ index: Number(pageEl.dataset.page), top, hidden: false });
      // O(visible) early-break — result-identical per the pickCurrentPage
      // document-order precondition.
      if (top > 80) break;
    }
    const best = pickCurrentPage(bands, sensedPage);
    // The exec's synchronous bus echo updates the mirror same-tick (command
    // handlers run before exec's first await — verified kernel.ts/commands.ts).
    if (best !== sensedPage) deps.exec("viewport.setCurrentPage", best);
  }

  function schedule(): void {
    if (disposed || rafHandle) return;
    rafHandle = raf(() => {
      rafHandle = 0;
      if (disposed) return;
      senseNow();
    });
  }

  deps.viewerEl.addEventListener("scroll", schedule, { passive: true });
  const unsubChanged = deps.subscribe("viewport:page-changed", ({ index }) => {
    sensedPage = index;
  });
  const unsubScrollTo = deps.subscribe("viewport:scroll-to-page", ({ index }) => {
    // The toolbar's original goTo mechanics; a missing placeholder (early streaming /
    // beyond the built range) is a silent no-scroll, same as before.
    deps.pagesContainer
      .querySelector<HTMLElement>(`.page[data-page="${index}"]`)
      ?.scrollIntoView({ behavior: "smooth", block: "start" });
  });

  return {
    schedule,
    current: () => sensedPage,
    reset() {
      sensedPage = 0;
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      deps.viewerEl.removeEventListener("scroll", schedule);
      if (rafHandle) {
        caf(rafHandle);
        rafHandle = 0;
      }
      unsubChanged();
      unsubScrollTo();
    },
  };
}
