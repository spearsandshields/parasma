/**
 * pickCurrentPage — the PURE topmost-page heuristic (kernel-truth current page).
 *
 * The rule is the toolbar tracker's, verbatim (toolbar.ts pre-extraction): among
 * non-hidden bands with top <= 80 CSS px (relative to the viewer top), pick the
 * one with the smallest |top|; if none qualify, stay sticky on `current`.
 */
import { describe, it, expect } from "vitest";
import { pickCurrentPage, type PageBand } from "./page-sense";

const band = (index: number, top: number, hidden = false): PageBand => ({ index, top, hidden });

describe("pickCurrentPage", () => {
  it("picks the closest band with top <= 80", () => {
    const bands = [band(0, -500), band(1, -20), band(2, 60), band(3, 900)];
    expect(pickCurrentPage(bands, 0)).toBe(1);
  });

  it("skips hidden bands", () => {
    const bands = [band(0, 10, true), band(1, 30)];
    expect(pickCurrentPage(bands, 0)).toBe(1);
  });

  it("falls back to `current` (sticky) when no band qualifies", () => {
    const bands = [band(0, 200), band(1, 1200)];
    expect(pickCurrentPage(bands, 7)).toBe(7);
  });

  it("returns `current` for an empty band list", () => {
    expect(pickCurrentPage([], 3)).toBe(3);
  });

  it("breaks |top| ties in favor of the earlier band (document order)", () => {
    // |-50| === |50|: strict < keeps the FIRST qualifying band, exactly like
    // the toolbar's `dist < bestDist`.
    const bands = [band(0, -50), band(1, 50)];
    expect(pickCurrentPage(bands, 1)).toBe(0);
  });

  it("truncated-prefix parity: a prefix ending at the first band with top > 80 picks the same page", () => {
    // PRECONDITION (documented on pickCurrentPage): bands are in document order
    // (ascending top). Bands past the first top>80 can only be FURTHER from the
    // viewport top, so the O(visible) early-break in the sensor is
    // result-identical to scanning the full list.
    const full = [band(0, -900), band(1, -300), band(2, 15), band(3, 120), band(4, 700), band(5, 1500)];
    const cut = full.findIndex((b) => b.top > 80);
    const prefix = full.slice(0, cut + 1); // include the first >80 band (the break point)
    expect(pickCurrentPage(prefix, 0)).toBe(pickCurrentPage(full, 0));
    expect(pickCurrentPage(full, 0)).toBe(2);
  });
});
