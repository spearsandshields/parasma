/**
 * Plugin registry with two-phase boot.
 *
 * Boot is topo-sorted on declared `deps`:
 *   1. resolve a load order (deps before dependents; cycle → throw),
 *   2. `initialize()` each plugin in that order (it may register commands,
 *      bridge the Viewer into the bus, seed its store slice),
 *   3. once ALL have initialized, `ready()` each (cross-plugin wiring is now
 *      safe — every plugin's commands/slice exist).
 * Destroy runs in REVERSE order so a plugin tears down before its deps.
 */

import type { KernelContext } from "./kernel-context";

export interface Plugin {
  /** Unique plugin id, e.g. "loader". Commands/slices are namespaced under it. */
  readonly id: string;
  /** Ids this plugin must boot after. Default none. */
  readonly deps?: readonly string[];
  /** Phase 1: register commands, bridge the Viewer, seed state. */
  initialize?(ctx: KernelContext): void | Promise<void>;
  /** Phase 2: cross-plugin wiring — every plugin is initialized by now. */
  ready?(ctx: KernelContext): void | Promise<void>;
  /** Teardown. Runs in reverse boot order. */
  destroy?(ctx: KernelContext): void | Promise<void>;
}

/**
 * Topo-sort plugins so each plugin's `deps` precede it. Throws on an unknown
 * dep or a cycle (deterministic boot is a correctness property, not a nicety).
 */
export function topoSort(plugins: readonly Plugin[]): Plugin[] {
  const byId = new Map<string, Plugin>();
  for (const p of plugins) {
    if (byId.has(p.id)) throw new Error(`[parasma:headless] duplicate plugin id: ${p.id}`);
    byId.set(p.id, p);
  }

  const ordered: Plugin[] = [];
  const visited = new Set<string>(); // fully placed
  const onStack = new Set<string>(); // in current DFS path → cycle detector

  const visit = (p: Plugin): void => {
    if (visited.has(p.id)) return;
    if (onStack.has(p.id)) {
      throw new Error(`[parasma:headless] dependency cycle through plugin: ${p.id}`);
    }
    onStack.add(p.id);
    for (const depId of p.deps ?? []) {
      const dep = byId.get(depId);
      if (!dep) {
        throw new Error(`[parasma:headless] plugin "${p.id}" depends on unknown plugin: ${depId}`);
      }
      visit(dep);
    }
    onStack.delete(p.id);
    visited.add(p.id);
    ordered.push(p);
  };

  // Visit in registration order for a stable result among independent plugins.
  for (const p of plugins) visit(p);
  return ordered;
}

export interface Registry {
  add(plugin: Plugin): void;
  has(id: string): boolean;
  get(id: string): Plugin | undefined;
  /** The two-phase boot. Idempotent guard via the kernel. */
  boot(ctx: KernelContext): Promise<void>;
  /** Reverse-order teardown. */
  destroy(ctx: KernelContext): Promise<void>;
  /** Boot order (after boot), for tests/diagnostics. */
  readonly order: readonly Plugin[];
}

export function createRegistry(): Registry {
  const plugins: Plugin[] = [];
  let order: Plugin[] = [];

  return {
    add(plugin) {
      if (plugins.some((p) => p.id === plugin.id)) {
        throw new Error(`[parasma:headless] duplicate plugin id: ${plugin.id}`);
      }
      plugins.push(plugin);
    },
    has(id) {
      return plugins.some((p) => p.id === id);
    },
    get(id) {
      return plugins.find((p) => p.id === id);
    },
    async boot(ctx) {
      order = topoSort(plugins);
      // Phase 1 — initialize in dependency order.
      for (const p of order) await p.initialize?.(ctx);
      // Phase 2 — ready, only after ALL initialized.
      for (const p of order) await p.ready?.(ctx);
    },
    async destroy(ctx) {
      for (let i = order.length - 1; i >= 0; i--) await order[i]!.destroy?.(ctx);
      order = [];
    },
    get order() {
      return order;
    },
  };
}
