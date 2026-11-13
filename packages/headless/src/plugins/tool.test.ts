/**
 * plugin-tool tests.
 *
 * plugin-tool owns the interaction TOOL MODE: a `@xstate/store` slice (NOT a
 * machine), deps:[] (a leaf — reads no other plugin/engine state), exposing
 * tool.get/toggle/setMode/setTransientHand and emitting `tool:mode-changed` with
 * the EFFECTIVE mode. effective = transientHand ? "hand" : baseMode; the emit is
 * DEDUP-ON-EFFECTIVE (fires only on a real effective-mode edge). Doc switch
 * (loader:status-changed → idle/planning/loading) resets to select.
 *
 * Boot pattern = zoom.test.ts (deps:[], no loader/viewer). `capture()` subscribes
 * `tool:mode-changed` and drops any retained replay so the array holds only edges
 * the interaction produced; `loader:status-changed` is driven via kernel.bus.emit
 * directly (as selection.test.ts does).
 */
import { describe, it, expect } from "vitest";
import { createKernel } from "../kernel";
import { toolPlugin, type ToolMode } from "./tool";

async function boot() {
  const kernel = createKernel();
  kernel.use(toolPlugin());
  await kernel.boot();
  return kernel;
}
function capture(kernel: Awaited<ReturnType<typeof boot>>) {
  const modes: Array<{ mode: ToolMode }> = [];
  kernel.bus.subscribe("tool:mode-changed", (e) => modes.push(e as { mode: ToolMode }));
  modes.length = 0; // drop any retained replay
  return modes;
}
const get = (k: Awaited<ReturnType<typeof boot>>) => k.exec("tool.get");

describe("plugin-tool", () => {
  it("defaults to 'select' before any command", async () => {
    expect(await get(await boot())).toBe("select");
  });

  it("tool.toggle flips base select↔hand and emits the effective mode", async () => {
    const k = await boot(); const m = capture(k);
    await k.exec("tool.toggle");
    expect(await get(k)).toBe("hand"); expect(m.at(-1)).toEqual({ mode: "hand" });
    await k.exec("tool.toggle");
    expect(await get(k)).toBe("select"); expect(m.at(-1)).toEqual({ mode: "select" });
  });

  it("tool.setMode sets the base mode + emits", async () => {
    const k = await boot(); const m = capture(k);
    await k.exec("tool.setMode", "hand");
    expect(await get(k)).toBe("hand"); expect(m.at(-1)).toEqual({ mode: "hand" });
  });

  it("setMode('select') when already select emits NOTHING (dedup)", async () => {
    const k = await boot(); const m = capture(k);
    await k.exec("tool.setMode", "select");
    expect(m.length).toBe(0);
  });

  it("transientHand overrides base → effective 'hand', reverts on release", async () => {
    const k = await boot(); const m = capture(k);
    await k.exec("tool.setTransientHand", true);
    expect(await get(k)).toBe("hand"); expect(m.at(-1)).toEqual({ mode: "hand" });
    await k.exec("tool.setTransientHand", false);
    expect(await get(k)).toBe("select"); expect(m.at(-1)).toEqual({ mode: "select" });
  });

  it("toggle under a held Space does NOT emit (effective pinned hand)", async () => {
    const k = await boot(); const m = capture(k);
    await k.exec("tool.setTransientHand", true); // emit hand
    const n = m.length;
    await k.exec("tool.toggle");                 // base select→hand, effective still hand
    expect(await get(k)).toBe("hand"); expect(m.length).toBe(n);
    await k.exec("tool.setTransientHand", false); // base is now hand → effective still hand
    expect(await get(k)).toBe("hand"); expect(m.length).toBe(n);
  });

  it("emit payload shape matches plugin-selection's subscription", async () => {
    const k = await boot(); const m = capture(k);
    await k.exec("tool.setMode", "hand");
    expect(m.at(-1)).toEqual({ mode: "hand" }); // deep-equal, no extra keys
  });

  it("doc-switch resets base+transient to select and emits when it was hand", async () => {
    const k = await boot();
    await k.exec("tool.setMode", "hand");
    const m = capture(k);
    k.bus.emit("loader:status-changed", "loading");
    expect(await get(k)).toBe("select"); expect(m.at(-1)).toEqual({ mode: "select" });
    // a reset while already 'select' emits nothing
    const m2 = capture(k);
    k.bus.emit("loader:status-changed", "planning");
    expect(m2.length).toBe(0);
  });

  it("doc-switch clears a held transientHand too", async () => {
    const k = await boot();
    await k.exec("tool.setTransientHand", true);
    k.bus.emit("loader:status-changed", "idle");
    expect(await get(k)).toBe("select");
  });
});
