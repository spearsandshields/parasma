/**
 * plugin-zoom tests — the zoom @xstate/store slice (hybrid rule: a
 * value-with-a-clamp, NOT a machine). Exercises clamp, multiplicative in/out,
 * the zoom:changed event + gesturing flag, and reset.
 */
import { describe, it, expect } from "vitest";
import { createKernel } from "../kernel";
import { zoomPlugin, ZOOM_MIN, ZOOM_MAX } from "./zoom";

async function boot() {
  const kernel = createKernel();
  kernel.use(zoomPlugin());
  await kernel.boot();
  return kernel;
}

describe("plugin-zoom", () => {
  it("starts at 100% and zoom.set updates the scale + emits zoom:changed", async () => {
    const kernel = await boot();
    expect(await kernel.exec("zoom.scale")).toBe(1);

    const changes: number[] = [];
    kernel.bus.subscribe("zoom:changed", (p) => changes.push((p as { scale: number }).scale));

    await kernel.exec("zoom.set", 2.5);
    expect(await kernel.exec("zoom.scale")).toBe(2.5);
    expect(changes.at(-1)).toBe(2.5);
  });

  it("clamps to [25%, 3200%]", async () => {
    const kernel = await boot();
    await kernel.exec("zoom.set", 99);
    expect(await kernel.exec("zoom.scale")).toBe(ZOOM_MAX); // 32.0
    await kernel.exec("zoom.set", 0.01);
    expect(await kernel.exec("zoom.scale")).toBe(ZOOM_MIN); // 0.25
    await kernel.exec("zoom.set", NaN);
    expect(Number.isFinite(await kernel.exec("zoom.scale"))).toBe(true);
  });

  it("zoom.by multiplies by a factor, clamps, and clears fitMode", async () => {
    const kernel = await boot();
    await kernel.exec("zoom.set", 2);
    await kernel.exec("zoom.by", 1.5);
    expect(await kernel.exec("zoom.scale")).toBeCloseTo(3, 5);
    await kernel.exec("zoom.by", 0.5);
    expect(await kernel.exec("zoom.scale")).toBeCloseTo(1.5, 5);
    // Clamps at the ceiling.
    await kernel.exec("zoom.by", 1000);
    expect(await kernel.exec("zoom.scale")).toBe(ZOOM_MAX);
    // A relative zoom is a manual override → fitMode cleared.
    await kernel.exec("zoom.fit", { scale: 1.2, mode: "width" });
    await kernel.exec("zoom.by", 1.1);
    expect(await kernel.exec("zoom.fitMode")).toBe("none");
  });

  it("zoom.in / zoom.out step multiplicatively and clamp at the bounds", async () => {
    const kernel = await boot();
    await kernel.exec("zoom.set", 1);
    await kernel.exec("zoom.in");
    expect(await kernel.exec("zoom.scale")).toBeCloseTo(1.25, 5);
    await kernel.exec("zoom.out");
    expect(await kernel.exec("zoom.scale")).toBeCloseTo(1, 5);
    // Out from the floor stays at the floor.
    await kernel.exec("zoom.set", ZOOM_MIN);
    await kernel.exec("zoom.out");
    expect(await kernel.exec("zoom.scale")).toBe(ZOOM_MIN);
  });

  it("setGesturing flips the gesturing flag on zoom:changed (Tier A vs Tier B)", async () => {
    const kernel = await boot();
    const gestures: boolean[] = [];
    kernel.bus.subscribe("zoom:changed", (p) => gestures.push((p as { gesturing: boolean }).gesturing));
    await kernel.exec("zoom.setGesturing", true);
    await kernel.exec("zoom.setGesturing", false);
    expect(gestures.at(-2)).toBe(true);
    expect(gestures.at(-1)).toBe(false);
  });

  it("zoom.reset returns to 100%", async () => {
    const kernel = await boot();
    await kernel.exec("zoom.set", 3);
    await kernel.exec("zoom.reset");
    expect(await kernel.exec("zoom.scale")).toBe(1);
  });
});
