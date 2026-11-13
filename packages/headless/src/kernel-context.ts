/**
 * The surface a plugin receives during boot. Plugins reach each other only
 * through this (commands/bus/paint) — never by import. There is no
 * store here: each plugin owns its OWN single-slice store; the bus is the
 * cross-plugin read-model.
 */

import type { Bus } from "./bus";
import type { CommandBus } from "./commands";
import type { PaintSurface } from "./paint";

export interface KernelContext {
  /** Replayable event bus (page lifecycle + plugin lifecycle). */
  readonly bus: Bus;
  /** Command bus: register your own; exec others. */
  readonly commands: CommandBus;
  /** Paint-intent surface (plugins request; the DOM adapters drain). */
  readonly paint: PaintSurface;
  /**
   * @advanced ESCAPE HATCH — look up another booted plugin's public api by id.
   * Prefer {@link KernelContext.commands} + {@link KernelContext.bus}. Typed:
   * pass the api shape as `T` for a `T | undefined` (defaults to `unknown`).
   */
  getPlugin<T = unknown>(id: string): T | undefined;
}
