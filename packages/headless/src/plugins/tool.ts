/**
 * plugin-tool — the interaction TOOL MODE: the model +
 * DECISION for whether a left-drag SELECTS text ("select") or PANS the viewport
 * ("hand"). It owns the mode; plugin-selection consults it (drops drags in hand
 * mode), the DomSelectionAdapter mode-gates its pointerdown, and the tool/cursor
 * adapters render grab/grabbing — all off the ONE `tool:mode-changed` signal this
 * plugin emits (the BUS is the seam; the plugin NEVER touches the DOM).
 *
 * TWO INPUTS, ONE EFFECTIVE MODE: `baseMode` is the persistent choice (the
 * toolbar toggle / tool.setMode); `transientHand` is the SPACE-HOLD override
 * (down→true, up→false) reverting on release. EFFECTIVE = transientHand ? "hand"
 * : baseMode — the value carried on every emit and returned by tool.get.
 *
 * BOUNDARY: a `@xstate/store` slice, NOT a machine (no cancellable
 * loop — mirrors plugin-zoom/plugin-grid). A LEAF plugin, deps:[] like
 * plugin-zoom: it reads NO other plugin/engine state. Its only cross-plugin tie
 * is CONSUMING `loader:status-changed` for the doc-switch reset — a replay
 * ("latest") event, so the subscription is order-independent and needs no dep.
 *
 * EMIT = DEDUP-ON-EFFECTIVE: `tool:mode-changed` fires ONLY when the effective
 * mode actually changes (unlike grid's re-emit-on-noop — replay "latest" already
 * catches late adapters). So toggling baseMode UNDER a held Space (effective
 * pinned "hand") emits nothing; the payload {mode} always matches what
 * plugin-selection subscribes to (bus.ts `{mode:"select"|"hand"}`).
 *
 * Doc switch (loader status → idle/planning/loading) resets baseMode→"select",
 * transientHand→false (mirrors grid.ts) so a stale hand mode can't outlive the
 * previous document.
 */
import type { Plugin } from "../registry";
import type { KernelContext } from "../kernel-context";
import { createSliceStore, type StoreSlice } from "../store";

/**
 * Commands OWNED by this plugin. Declaration-merged into the open
 * {@link CommandMap} (targeting `../commands`, the barrel-re-exported kernel
 * module → program-wide) so `exec(id, …)` is arg/return type-checked and an
 * undeclared id fails to register. Signatures authored from the handler bodies.
 */
declare module "../commands" {
  interface CommandMap {
    "tool.get": () => ToolMode;
    "tool.toggle": () => void;
    "tool.setMode": (m: ToolMode) => void;
    "tool.setTransientHand": (b: boolean) => void;
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
     * Tool mode changed (plugin-tool — plugin-tool is the SOLE
     * emitter). 'select' = left-drag selects text; 'hand' = left-drag pans.
     * plugin-selection consults this to suppress drag-select in hand mode. Sticky
     * ("latest") so a late subscriber catches the current mode; until the first
     * emit, the effective mode is the default 'select'.
     */
    "tool:mode-changed": { mode: "select" | "hand" };
    /**
     * Base tool mode changed via the EXPLICIT toggle (plugin-tool `tool.setMode` /
     * `tool.toggle`) — NOT the Space-hold transient. plugin-selection clears the
     * current selection on this (switching tools drops the selection), while a
     * transient Space-pan preserves it. Transient ("none"): a late subscriber must
     * not retroactively clear a selection.
     */
    "tool:base-mode-changed": { mode: "select" | "hand" };
  }
}

/** The interaction tool mode: 'select' left-drag selects; 'hand' pans. */
export type ToolMode = "select" | "hand";

/** plugin-tool store slice: the persistent base mode + the Space-hold override. */
export interface ToolState extends Record<string, unknown> {
  /** The persistent mode (toolbar toggle / tool.setMode). Default 'select'. */
  baseMode: ToolMode;
  /** Space-hold transient override → forces effective 'hand' until released. */
  transientHand: boolean;
}

const initialState: ToolState = { baseMode: "select", transientHand: false };

function makeSlice(): StoreSlice<ToolState> {
  return {
    state: initialState,
    on: {
      setBaseMode: (s, m: ToolMode) => ({ ...s, baseMode: m }),
      toggleBase: (s) => ({ ...s, baseMode: s.baseMode === "select" ? "hand" : "select" }),
      setTransientHand: (s, b: boolean) => ({ ...s, transientHand: b }),
      reset: () => ({ ...initialState }),
    },
  };
}

/** The tool plugin. Leaf (no Viewer, deps:[]) — pure interaction-mode state. */
export function toolPlugin(): Plugin {
  const store = createSliceStore(makeSlice());
  const unsubs: Array<() => void> = [];
  const state = (): ToolState => store.get();
  /** The one value every consumer sees: Space-hold overrides the base mode. */
  const effective = (): ToolMode => (state().transientHand ? "hand" : state().baseMode);

  return {
    id: "tool",
    deps: [],

    initialize(ctx: KernelContext) {
      // Mutate the slice, then emit tool:mode-changed ONLY if the effective mode
      // actually moved (dedup — replay "latest" covers late adapters). Payload
      // {mode} matches plugin-selection / bus.ts.
      const applyAndEmit = (mutate: () => void): void => {
        const before = effective();
        mutate();
        const after = effective();
        if (after !== before) ctx.bus.emit("tool:mode-changed", { mode: after });
      };

      // Explicit toggle path (tool.setMode / tool.toggle): also emit a BASE-mode
      // edge so plugin-selection clears the selection on a tool switch — but NOT
      // on the Space-hold transient (which uses applyAndEmit directly), so a quick
      // space-pan preserves the selection.
      const applyBase = (mutate: () => void): void => {
        const before = state().baseMode;
        applyAndEmit(mutate);
        const after = state().baseMode;
        if (after !== before) ctx.bus.emit("tool:base-mode-changed", { mode: after });
      };

      unsubs.push(
        // Doc switch: reset to select + no transient hand (mirrors grid.ts) so a
        // stale hand mode can't outlive the previous document. Emit only on a real
        // edge (e.g. a reset while in hand mode).
        ctx.bus.subscribe("loader:status-changed", (s) => {
          if (s === "idle" || s === "planning" || s === "loading") {
            applyAndEmit(() => store.dispatch("reset", undefined));
          }
        }),
      );

      // ── Commands (the plugin surface) ───────────────────────────────────────
      unsubs.push(
        // tool.get → the EFFECTIVE mode (the analog of grid.mode) — the value
        // consumers gate selection/pan/cursor on.
        ctx.commands.register("tool.get", (): ToolMode => effective()),

        // Flip the persistent base mode (the toolbar toggle). Under a held Space
        // (transientHand) the effective mode is pinned 'hand', so this silently
        // changes base with NO emit — correct (dedup).
        ctx.commands.register("tool.toggle", () => {
          applyBase(() => store.dispatch("toggleBase", undefined));
        }),

        // Set the persistent base mode explicitly.
        ctx.commands.register("tool.setMode", (m: ToolMode) => {
          applyBase(() => store.dispatch("setBaseMode", m));
        }),

        // Space-hold override (adapter: keydown→true, keyup/blur→false).
        ctx.commands.register("tool.setTransientHand", (b: boolean) => {
          applyAndEmit(() => store.dispatch("setTransientHand", b));
        }),
      );
    },

    destroy(_ctx: KernelContext) {
      for (const u of unsubs.splice(0)) u();
      store.dispatch("reset", undefined);
    },
  };
}
