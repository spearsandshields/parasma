/**
 * EMF raster policy units — the pure decision core (no timers, paint-driven).
 * Each test states its RED condition: what a broken rule would let through
 * (blank-on-zoom, raster thrash, a 16MP re-raster loop at the wall).
 */
import { describe, it, expect } from "vitest";
import {
  planEmfRaster,
  budgetClamp,
  EMF_PIXEL_BUDGET,
  EMF_UPGRADE_FACTOR,
  type EmfRasterEntry,
} from "./emf-raster-policy";
import { replayDisplayList } from "./replay";

const entry = (over: Partial<EmfRasterEntry> = {}): EmfRasterEntry => ({
  pxW: 1000,
  pxH: 500,
  complete: true,
  inFlightPxW: 0,
  pendingReraster: false,
  ...over,
});

describe("planEmfRaster — first need", () => {
  it("no entry → plans req × 1.5 headroom (a settled zoom rarely needs a second pass)", () => {
    expect(planEmfRaster(null, 800, 400)).toEqual({ pxW: 1200, pxH: 600 });
    expect(planEmfRaster(undefined, 801, 401)).toEqual({ pxW: Math.ceil(801 * 1.5), pxH: Math.ceil(401 * 1.5) });
  });

  it("degenerate/NaN requests plan nothing (NaN-rejecting guards)", () => {
    expect(planEmfRaster(null, 0, 100)).toBeNull();
    expect(planEmfRaster(null, 100, 0)).toBeNull();
    expect(planEmfRaster(null, NaN, 100)).toBeNull();
    expect(planEmfRaster(entry(), NaN, NaN)).toBeNull();
  });
});

describe("planEmfRaster — downscale + hysteresis", () => {
  it("req ≤ cached → null (pure blit-scale-down, always crisp; cache never shrinks)", () => {
    expect(planEmfRaster(entry(), 1000, 500)).toBeNull();
    expect(planEmfRaster(entry(), 200, 100)).toBeNull();
  });

  it("req in (cached, cached×1.5] → null (the hysteresis band, no thrash)", () => {
    expect(planEmfRaster(entry(), 1001, 501)).toBeNull();
    expect(planEmfRaster(entry(), 1500, 750)).toBeNull(); // exactly ×1.5
  });

  it("req > cached×1.5 → plans req×1.5 (the upgrade)", () => {
    expect(planEmfRaster(entry(), 1501, 751)).toEqual({
      pxW: Math.ceil(1501 * EMF_UPGRADE_FACTOR),
      pxH: Math.ceil(751 * EMF_UPGRADE_FACTOR),
    });
  });
});

describe("planEmfRaster — 16MP budget wall", () => {
  it("budgetClamp is aspect-preserving and caps the area at the budget", () => {
    const { pxW, pxH } = budgetClamp(8000, 4000); // 32MP → clamp
    expect(pxW * pxH).toBeLessThanOrEqual(EMF_PIXEL_BUDGET);
    expect(pxW / pxH).toBeCloseTo(2, 1); // 2:1 aspect preserved
    // under budget passes through untouched
    expect(budgetClamp(100, 50)).toEqual({ pxW: 100, pxH: 50 });
  });

  it("a planned upgrade is clamped to the budget", () => {
    const plan = planEmfRaster(entry({ pxW: 100, pxH: 50 }), 8000, 4000);
    expect(plan).not.toBeNull();
    expect(plan!.pxW * plan!.pxH).toBeLessThanOrEqual(EMF_PIXEL_BUDGET);
  });

  it("entry already AT the clamped target → null (soft degrade, no re-raster loop at the wall)", () => {
    const wall = budgetClamp(Math.ceil(8000 * 1.5), Math.ceil(4000 * 1.5));
    const e = entry({ pxW: wall.pxW, pxH: wall.pxH });
    // Every further zoom-in enters the upgrade rule but must stay null forever.
    expect(planEmfRaster(e, 8000, 4000)).toBeNull();
    expect(planEmfRaster(e, 16000, 8000)).toBeNull();
  });

  it("±1px float jitter at the wall does not re-raster (slack)", () => {
    const wall = budgetClamp(Math.ceil(8000 * 1.5), Math.ceil(4000 * 1.5));
    // cached one px SHORT of the recomputed clamp target — jitter, not a real upgrade
    const e = entry({ pxW: wall.pxW - 1, pxH: wall.pxH });
    expect(planEmfRaster(e, 8000, 4000)).toBeNull();
  });
});

describe("planEmfRaster — in-flight dedup", () => {
  it("an equal-or-larger raster in flight → null (one raster per key)", () => {
    const e = entry({ inFlightPxW: 3000 });
    expect(planEmfRaster(e, 1600, 800)).toBeNull(); // target 2400 ≤ 3000 in flight
  });

  it("a LARGER target than the in-flight raster still plans (zoomed past it mid-flight)", () => {
    const e = entry({ inFlightPxW: 1800 });
    expect(planEmfRaster(e, 1600, 800)).toEqual({ pxW: 2400, pxH: 1200 });
  });
});

describe("replay onImageDraw — inert-ness for non-EMF documents", () => {
  const drawImageOps = (key: string): { opcodes: Float64Array; strings: string[] } => ({
    // [OP_DRAW_IMAGE=34, strIdx, dx, dy, dw, dh]
    opcodes: new Float64Array([34, 0, 1, 2, 30, 40]),
    strings: [key],
  });
  const noopCtx = () =>
    new Proxy({}, { get: () => () => undefined }) as unknown as CanvasRenderingContext2D;

  it("fires for 'emf-pic:' keys on a MISS (placeholder beat still feeds the upgrade check)", () => {
    const seen: Array<[string, number, number]> = [];
    const { opcodes, strings } = drawImageOps("emf-pic:rId7");
    replayDisplayList(noopCtx(), opcodes, strings, new Map(), (k, dw, dh) => seen.push([k, dw, dh]));
    expect(seen).toEqual([["emf-pic:rId7", 30, 40]]);
  });

  it("fires on a HIT too (hits must still feed the upgrade check)", () => {
    const seen: string[] = [];
    const { opcodes, strings } = drawImageOps("emf-pic:rId7");
    const images = new Map([["emf-pic:rId7", {} as ImageBitmap]]);
    replayDisplayList(noopCtx(), opcodes, strings, images, (k) => seen.push(k));
    expect(seen).toEqual(["emf-pic:rId7"]);
  });

  it("never fires for ordinary image keys (zero new code paths without EMF)", () => {
    const seen: string[] = [];
    const { opcodes, strings } = drawImageOps("rId3");
    replayDisplayList(noopCtx(), opcodes, strings, new Map(), (k) => seen.push(k));
    expect(seen).toEqual([]);
    // and the internal DIB blit prefix is NOT the pic prefix (colon discipline)
    const blit = drawImageOps("emf:rId3:0");
    replayDisplayList(noopCtx(), blit.opcodes, blit.strings, new Map(), (k) => seen.push(k));
    expect(seen).toEqual([]);
  });
});
