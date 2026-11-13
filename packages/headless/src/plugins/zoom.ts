/**
 * plugin-zoom — the document zoom level.
 *
 * A `@xstate/store` SLICE, not an XState machine: zoom is "a value with a clamp"
 * (the hybrid rule — machines are only for the loader/fonts async
 * lifecycles). Owns the scalar `scale` (0.25–32.0) + `fitMode` + a `gesturing`
 * flag, exposes set/in/out/reset/fit commands, and emits `zoom:changed`.
 *
 * The TWO-TIER render contract: the plugin owns only
 * the NUMBER. The consumer's viewport adapter does the rendering — applying a
 * cheap CSS `transform: scale` while `gesturing` (60fps, momentarily soft) and a
 * crisp per-canvas re-render at the new scale when the gesture settles. The
 * plugin signals the tier via the `gesturing` field on `zoom:changed`.
 */

import { createSliceStore, type StoreSlice } from "../store";
import type { Plugin } from "../registry";
import type { KernelContext } from "../kernel-context";

/**
 * Commands OWNED by this plugin. Declaration-merged into the open
 * {@link CommandMap} (targeting `../commands`, the barrel-re-exported kernel
 * module → program-wide) so `exec(id, …)` is arg/return type-checked and an
 * undeclared id fails to register. Signatures authored from the handler bodies.
 */
declare module "../commands" {
  interface CommandMap {
    "zoom.set": (scale: number) => void;
    "zoom.in": () => void;
    "zoom.out": () => void;
    "zoom.by": (factor: number) => void;
    "zoom.reset": () => void;
    "zoom.fit": (p: { scale: number; mode: FitMode }) => void;
    "zoom.setGesturing": (g: boolean) => void;
    "zoom.scale": () => number;
    "zoom.fitMode": () => FitMode;
  }
}

/**
 * Bus events OWNED by this plugin (declared here, not in the kernel's
 * bus.ts). Declaration-merged into the open {@link BusEventMap} so adding a
 * plugin event no longer edits the kernel. The `../bus` path and `../commands`
 * path below both resolve to the same kernel modules the barrel re-exports,
 * so the augmentation is program-wide for every consumer.
 */
declare module "../bus" {
  interface BusEventMap {
    /**
     * Zoom changed (plugin-zoom). `scale` is 0.25–32.0. `gesturing` is true during
     * an active pinch/drag (Tier A: the consumer applies a cheap CSS transform for
     * 60fps), false at rest (Tier B: re-render canvases crisp at the new scale).
     * Sticky ("latest") so a late-attached adapter knows the current zoom.
     */
    "zoom:changed": { scale: number; gesturing: boolean };
  }
}

export const ZOOM_MIN = 0.25;
export const ZOOM_MAX = 32.0;
/** Multiplicative step for zoom.in / zoom.out (≈ one notch). */
const ZOOM_STEP = 1.25;

export type FitMode = "none" | "width" | "page";

export interface ZoomState extends Record<string, unknown> {
  /** Current zoom, clamped to [ZOOM_MIN, ZOOM_MAX]. 1 = 100%. */
  scale: number;
  fitMode: FitMode;
  /** True during an active pinch/drag (Tier A); false at rest (Tier B crisp). */
  gesturing: boolean;
}

const clamp = (s: number): number =>
  Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, Number.isFinite(s) ? s : 1));

const initialState: ZoomState = { scale: 1, fitMode: "none", gesturing: false };

function makeSlice(): StoreSlice<ZoomState> {
  return {
    state: initialState,
    on: {
      // An explicit zoom clears any fit mode (the user took manual control).
      set: (s, scale: number) => ({ ...s, scale: clamp(scale), fitMode: "none" }),
      // Relative zoom (wheel/pinch): multiply by a per-event factor. Relative so
      // the consumer never reads `scale` back across the async kernel.exec
      // boundary to compute the next value — a race under a wheel burst.
      by: (s, factor: number) => ({ ...s, scale: clamp(s.scale * factor), fitMode: "none" }),
      // A fit command sets both the computed scale and the sticky fit mode (so a
      // container resize can re-fit — the consumer recomputes and re-issues).
      setFit: (s, p: { scale: number; fitMode: FitMode }) => ({
        ...s,
        scale: clamp(p.scale),
        fitMode: p.fitMode,
      }),
      setGesturing: (s, g: boolean) => ({ ...s, gesturing: g }),
      reset: () => ({ ...initialState }),
    },
  };
}

export function zoomPlugin(): Plugin {
  const store = createSliceStore(makeSlice());
  const unsubs: Array<() => void> = [];
  const state = (): ZoomState => store.get();
  let ctxRef: KernelContext | null = null;

  /** Emit zoom:changed if the scale or gesturing flag actually moved. */
  function emit(prev: ZoomState): void {
    const s = state();
    if (s.scale !== prev.scale || s.gesturing !== prev.gesturing) {
      ctxRef?.bus.emit("zoom:changed", { scale: s.scale, gesturing: s.gesturing });
    }
  }

  return {
    id: "zoom",
    deps: [],

    initialize(ctx: KernelContext) {
      ctxRef = ctx;

      unsubs.push(
        ctx.commands.register("zoom.set", (scale: number) => {
          const prev = state();
          store.dispatch("set", scale);
          emit(prev);
        }),
        ctx.commands.register("zoom.in", () => {
          const prev = state();
          store.dispatch("set", prev.scale * ZOOM_STEP);
          emit(prev);
        }),
        ctx.commands.register("zoom.out", () => {
          const prev = state();
          store.dispatch("set", prev.scale / ZOOM_STEP);
          emit(prev);
        }),
        ctx.commands.register("zoom.by", (factor: number) => {
          const prev = state();
          store.dispatch("by", factor);
          emit(prev);
        }),
        ctx.commands.register("zoom.reset", () => {
          const prev = state();
          store.dispatch("set", 1);
          emit(prev);
        }),
        // Fit-to-width / fit-to-page: the consumer computes the scale from its
        // container width vs the page width (it owns the DOM), and passes it in.
        ctx.commands.register("zoom.fit", (p: { scale: number; mode: FitMode }) => {
          const prev = state();
          store.dispatch("setFit", { scale: p.scale, fitMode: p.mode });
          emit(prev);
        }),
        // Mark gesture start/end. On END, re-emit so the adapter does the crisp
        // (Tier B) re-render even if the scale was already at its final value.
        ctx.commands.register("zoom.setGesturing", (g: boolean) => {
          store.dispatch("setGesturing", g);
          ctx.bus.emit("zoom:changed", { scale: state().scale, gesturing: g });
        }),
        // Queries.
        ctx.commands.register("zoom.scale", () => state().scale),
        ctx.commands.register("zoom.fitMode", () => state().fitMode),
      );
    },

    destroy() {
      for (const u of unsubs.splice(0)) u();
      store.dispatch("reset", undefined);
      ctxRef = null;
    },
  };
}
