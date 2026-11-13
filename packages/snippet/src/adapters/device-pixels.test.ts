/**
 * device-pixels.test.ts — the grid scroll-PIN device-snap math (adapter-only
 * fractional-DPR gutter-shimmer fix). Pure, no DOM: asserts the invariants the
 * DomGridAdapter.paint() pin relies on so the sticky gutter + gridlines land on
 * the SAME physical-pixel grid every scroll frame.
 *
 * The pin computes domX = snapLenToDevice(scrollLeft, dpr) for the CSS translate
 * and derives the engine content offset sx = domX·scaleX/zoom from the SAME
 * value, so the translate and the paint agree on one device-rounded offset.
 */
import { describe, it, expect } from "vitest";
import { snapLenToDevice } from "./device-pixels";

const FRACTIONAL_DPRS = [1.25, 1.5];

describe("grid scroll-pin device snap", () => {
  it("pins the translate onto a whole device pixel", () => {
    for (const dpr of FRACTIONAL_DPRS) {
      for (let s = 0; s < 200; s += 0.13) {
        const domX = snapLenToDevice(s, dpr);
        const deviceUnits = domX * dpr;
        expect(Math.abs(deviceUnits - Math.round(deviceUnits))).toBeLessThan(1e-9);
      }
    }
  });

  it("derives a device-whole content offset when scaleX=1 (labels/gridlines align)", () => {
    const scaleX = 1;
    for (const dpr of FRACTIONAL_DPRS) {
      for (const zoom of [1, 1.5]) {
        for (let s = 0; s < 200; s += 0.13) {
          const domX = snapLenToDevice(s, dpr);
          const sx = (domX * scaleX) / zoom;
          // The wasm gutter labels/gridlines land on the device grid iff the
          // content offset scaled back to device space is an integer.
          const deviceUnits = sx * zoom * dpr;
          expect(Math.abs(deviceUnits - Math.round(deviceUnits))).toBeLessThan(1e-9);
        }
      }
    }
  });

  it("is monotonic with device-step deltas in {0,1} (no ±0.5 oscillation)", () => {
    for (const dpr of FRACTIONAL_DPRS) {
      let prevDevice: number | null = null;
      for (let s = 0; s < 200; s += 0.13) {
        const device = Math.round(snapLenToDevice(s, dpr) * dpr);
        if (prevDevice !== null) {
          const step = device - prevDevice;
          expect(step).toBeGreaterThanOrEqual(0); // non-decreasing
          expect([0, 1]).toContain(step); // whole device-px steps, never a flip
        }
        prevDevice = device;
      }
    }
  });

  it("is identity at integer DPR (guards in-process byte-identity)", () => {
    for (let s = 0; s < 50; s += 0.5) {
      // Already-whole-device inputs at dpr=2 are fixed points.
      const whole = Math.round(s * 2) / 2; // a value on the dpr=2 grid
      expect(snapLenToDevice(whole, 2)).toBeCloseTo(whole, 12);
    }
  });
});
