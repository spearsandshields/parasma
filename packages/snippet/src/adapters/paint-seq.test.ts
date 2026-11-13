/**
 * paint-seq.test.ts — the supersede-decision pure-function guard for the
 * back-buffer slice fix. This is the only correctness-critical piece of the fix
 * that is jsdom-safe (the blit/white-window pixel timing is a browser-only
 * harness assertion — jsdom has no real 2D raster). It pins the latest-wins
 * contract: a paint whose generation has been superseded mid-flight must NOT
 * commit, so it can never blit stale pixels over a newer landed frame.
 */
import { describe, it, expect } from "vitest";
import { shouldCommitPaint } from "./paint-seq";

describe("shouldCommitPaint", () => {
  it("commits when the generation token is unchanged (equal seq → true)", () => {
    expect(shouldCommitPaint(0, 0)).toBe(true);
    expect(shouldCommitPaint(7, 7)).toBe(true);
  });

  it("drops when a newer generation has bumped the token (currentSeq advanced → false)", () => {
    expect(shouldCommitPaint(3, 4)).toBe(false);
    expect(shouldCommitPaint(0, 1)).toBe(false);
    expect(shouldCommitPaint(10, 999)).toBe(false);
  });

  it("burst: a paint captured at N must NOT commit once a mid-flight bump advances to N+1 (latest-wins)", () => {
    // Simulate: paintWindow captures the live token, then a sibling/successor
    // generation bumps it while the RPC is in flight. The captured paint drops.
    let paintSeq = 5;
    const capturedByStalePaint = paintSeq; // captured at entry
    paintSeq++; // a newer generation (scroll rAF / zoom batch / invalidate) fires mid-RPC
    expect(shouldCommitPaint(capturedByStalePaint, paintSeq)).toBe(false);

    // The paint launched BY that newer generation captured the bumped value and
    // therefore still commits — exactly one generation is current at a time.
    const capturedByFreshPaint = paintSeq;
    expect(shouldCommitPaint(capturedByFreshPaint, paintSeq)).toBe(true);
  });

  it("table-driven latest-wins matrix", () => {
    const cases: Array<{ start: number; current: number; commit: boolean }> = [
      { start: 0, current: 0, commit: true },
      { start: 1, current: 1, commit: true },
      { start: 1, current: 2, commit: false },
      { start: 2, current: 5, commit: false },
      { start: 100, current: 100, commit: true },
    ];
    for (const c of cases) {
      expect(shouldCommitPaint(c.start, c.current)).toBe(c.commit);
    }
  });
});
