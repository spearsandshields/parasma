// Red-green guards for the gesture-time slice tracking math (zoom-tremor fix).
// The correctness-critical property: content painted at paintedScale sitting at
// page-relative (left, top) must land at (left·r, top·r) under the live scale —
// i.e. the rescale happens about the PAGE origin, not the slice corner. A bare
// `scale(r)` (wrong origin) or a `paintedScale/liveScale` inversion fails these.
import { describe, expect, it } from "vitest";

import { sliceTrackTransform } from "./slice-track";

describe("sliceTrackTransform", () => {
  it("identity when the live scale equals the painted scale", () => {
    expect(sliceTrackTransform(2, 2, 100, 50)).toBe("");
  });

  it("zoom-in: rescales about the page origin (translate carries the origin shift)", () => {
    // r = 2: content at page-relative (100, 50) belongs at (200, 100) — the
    // element (origin 0 0) needs translate(+100, +50) then scale(2).
    expect(sliceTrackTransform(1, 2, 100, 50)).toBe("translate(100px, 50px) scale(2)");
  });

  it("zoom-out: negative origin shift", () => {
    // r = 0.5: content at (100, 50) belongs at (50, 25) → translate(-50, -25).
    expect(sliceTrackTransform(2, 1, 100, 50)).toBe("translate(-50px, -25px) scale(0.5)");
  });

  it("slice at the page origin needs no translate, only scale", () => {
    expect(sliceTrackTransform(1, 1.5, 0, 0)).toBe("translate(0px, 0px) scale(1.5)");
  });

  it("degenerate painted scale is inert (no NaN/Infinity transforms)", () => {
    expect(sliceTrackTransform(0, 2, 100, 50)).toBe("");
    expect(sliceTrackTransform(Number.NaN, 2, 100, 50)).toBe("");
  });

  it("near-identity ratios collapse to identity (no per-frame sub-epsilon churn)", () => {
    expect(sliceTrackTransform(2, 2 + 1e-9, 100, 50)).toBe("");
  });
});
