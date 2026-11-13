/**
 * @parasma/headless/internal — the @advanced escape-hatch tier, re-exported on a
 * dedicated subpath.
 *
 * This is a DISCOVERABILITY tier, NOT a hiding one: every symbol here is ALSO
 * exported from the main `.` barrel. The subpath exists so a power user assembling
 * a bespoke kernel (or unit-testing a plugin against a hand-built bus/store) can
 * import the raw primitives from one honest place — signalling "you are past the
 * blessed contract" — without any export being removed from `.`.
 *
 * A normal BYO-chrome consumer never needs these: createKernel() constructs the
 * bus / command bus / paint surface / registry internally, and the ports +
 * commands + bus are the blessed rails.
 */

export { createBus } from "./bus";
export { createCommandBus } from "./commands";
export { createSliceStore } from "./store";
export type { Store, StoreSlice } from "./store";
export { createPaintSurface } from "./paint";
export type { PaintSurface, PaintIntent, PaintIntentHandler } from "./paint";
export { createRegistry, topoSort } from "./registry";
export type { Registry, Plugin } from "./registry";
