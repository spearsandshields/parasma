/**
 * @parasma/headless KERNEL.
 *
 * Framework-agnostic (NO framework imports). Ties the buses together:
 *  - a plugin REGISTRY (two-phase topo-sorted boot; reverse-order destroy),
 *  - a REPLAYABLE event bus — the CROSS-PLUGIN read-model,
 *  - a TYPED COMMAND bus (register/exec; plugins reach each other only here),
 *  - a paint-intent surface (plugins request paints; the DOM adapters drain).
 *
 * There is NO kernel-composed store: each stateful plugin owns its OWN
 * single-slice `createSliceStore` (store.ts) and reads only its own state; the
 * bus's replayable sticky events are the cross-plugin read-model.
 * The kernel holds no plugin state.
 */

import { createBus, type Bus } from "./bus";
import {
  createCommandBus,
  type CommandBus,
  type CommandId,
  type CommandMap,
  type DynamicCommandId,
} from "./commands";
import { createPaintSurface, type PaintSurface } from "./paint";
import { createRegistry, type Plugin } from "./registry";
import type { KernelContext } from "./kernel-context";

export interface Kernel {
  /** Register a plugin BEFORE boot. Returns the kernel for chaining. */
  use(plugin: Plugin): Kernel;
  /** Two-phase boot: topo-sort → initialize() all → ready() all. Idempotent. */
  boot(): Promise<void>;
  /** Reverse-order teardown; clears the bus replay logs. */
  destroy(): Promise<void>;
  /**
   * Invoke a declared command by id, e.g. `kernel.exec('loader.open', bytes)`.
   * Args + return are type-checked against {@link CommandMap}.
   */
  exec<Id extends CommandId>(
    id: Id,
    ...args: Parameters<CommandMap[Id]>
  ): Promise<Awaited<ReturnType<CommandMap[Id]>>>;
  /** Invoke a genuinely-dynamic id (the branded escape hatch, see `asDynamic`). */
  exec<R = unknown>(id: DynamicCommandId, ...args: unknown[]): Promise<R>;
  /** The replayable event bus. */
  readonly bus: Bus;
  /** The command bus. */
  readonly commands: CommandBus;
  /** The paint-intent surface. */
  readonly paint: PaintSurface;
  /**
   * @advanced ESCAPE HATCH — look up a booted plugin's raw record by id.
   *
   * The BLESSED cross-plugin rails stay {@link Kernel.exec} (typed commands) +
   * {@link Kernel.bus} (replayable events); reach for this only when a power
   * consumer must touch a plugin's public api directly. Typed: pass the plugin's
   * api shape as `T` (e.g. `kernel.getPlugin<LoaderApi>("loader")`) to get a
   * `T | undefined` instead of an opaque `Plugin`. Officialized (not removed):
   * every escape hatch stays reachable.
   */
  getPlugin<T = Plugin>(id: string): T | undefined;
}

export function createKernel(): Kernel {
  const bus = createBus();
  const commands = createCommandBus();
  const paint = createPaintSurface();
  const registry = createRegistry();

  let booted = false;

  const ctx: KernelContext = {
    bus,
    commands,
    paint,
    getPlugin: <T = unknown>(id: string): T | undefined =>
      registry.get(id) as unknown as T | undefined,
  };

  const kernel: Kernel = {
    use(plugin) {
      if (booted) throw new Error("[parasma:headless] cannot use() after boot()");
      registry.add(plugin);
      return kernel;
    },
    async boot() {
      if (booted) return;
      booted = true;
      await registry.boot(ctx);
    },
    async destroy() {
      if (!booted) return;
      await registry.destroy(ctx);
      bus.reset();
      booted = false;
    },
    exec(id: string, ...args: unknown[]): Promise<never> {
      // The typed overloads live on the interface; the delegation is one cast
      // (both buses share the same CommandMap-derived overload pair).
      return commands.exec(id as DynamicCommandId, ...args);
    },
    bus,
    commands,
    paint,
    getPlugin: <T = Plugin>(id: string): T | undefined =>
      registry.get(id) as unknown as T | undefined,
  };

  return kernel;
}
