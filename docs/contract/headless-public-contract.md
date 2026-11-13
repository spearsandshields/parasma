# Headless public contract

The `@parasma/headless` surface is **tiered**, not narrowed. Every symbol that was
reachable before the tiering is still reachable — the tiering only makes the *intended* contract
first-class (blessed `@public` tier) while keeping the escape hatches reachable
(`@advanced` tier). This is the "officialize, lean, improve — ZERO capability /
flexibility loss" invariant.

## Subpaths

| Import path | Contents |
| --- | --- |
| `@parasma/headless` (`.`) | The **whole** surface — every `@public` symbol *and* every `@advanced` primitive. Nothing was dropped from here. |
| `@parasma/headless/internal` | A discoverability re-export of the `@advanced` tier only. Signals "you are past the blessed contract." Additive — everything here is also on `.`. |

## `@public` — the blessed contract

The surface a BYO-chrome consumer (e.g. a parent app that hosts another document
viewer on the same page) depends on:

- **Kernel** — `createKernel`, `Kernel`. Register plugins, `boot()`, `exec()` typed
  commands, subscribe the replayable `bus`.
- **KernelContext** — the surface a plugin factory authors against.
- **Command bus contract** — `CommandBus`, `Command`, `CommandMap`, `CommandId`,
  `DynamicCommandId`, `asDynamic`.
- **Event bus contract** — `Bus`, `BusEventMap`, `BusHandler`, `ReplayPolicy`.
- **Teardown** — `runTeardown`, `TeardownStep` (the ordered Promise-returning spine
  of a viewer handle's `destroy()`).
- **The 13 plugin factories** + their state/query/option types (`loaderPlugin`,
  `viewportPlugin`, `zoomPlugin`, `cursorPlugin`, `linksPlugin`, `sheetsPlugin`,
  `gridPlugin`, `commentsPlugin`, `searchPlugin`, `selectionPlugin`, `toolPlugin`,
  `clipboardPlugin`, `fontsPlugin`, plus `ZOOM_MIN`/`ZOOM_MAX`).
- **Narrow plugin PORTS** — `LoaderViewer`, `CursorViewer`, `LinksViewer`. Each is a
  `Pick<Viewer, K>` the concrete `Viewer` satisfies definitionally: the
  "author-your-own-driver" contract + the testability seam. Adopted on
  loader/cursor/links; the recipe is documented for the deferred rest.
- **Clipboard flavor serializers** (load-bearing for the DOM adapter's sync copy
  tier) — `serializeTextPlain`, `serializeTextHtml`, `serializeTable`, `WML_TABLE`,
  `SML_TSV`, `serializeCellsTsv`, `serializeCellsHtml`, `quoteTsvCell`,
  `serializeChartTsv`, `CHART_RASTER_SCALE`, and the flavor types.
- **`DocumentInfo`** — re-exported so a consumer driving `loader.open` needn't reach
  into `@parasma/core`.

### The escape hatch: `getPlugin<T>`

`kernel.getPlugin<T>(id)` stays as a **documented escape hatch** (it was *not*
removed). Its signature was improved from `Plugin`/`unknown` to a generic
`getPlugin<T = Plugin>(id): T | undefined`, so a power consumer can read a plugin's
api typed without a cast. The blessed cross-plugin rails remain `commands.exec`
(typed) + `bus` (replayable).

## `@advanced` — escape-hatch kernel primitives

A normal consumer never constructs these — `createKernel()` wires them internally —
but a power user assembling a bespoke kernel can reach them (on `.` or via
`/internal`):

- `createBus`, `createCommandBus`, `createSliceStore` (+ `Store`, `StoreSlice`),
  `createPaintSurface` (+ `PaintSurface`, `PaintIntent`, `PaintIntentHandler`),
  `createRegistry`, `topoSort` (+ `Registry`, `Plugin`).

## Composition seam (`@parasma/snippet/headless`)

The chrome-free path lives in `@parasma/snippet`, not here — see
`createHeadlessViewer` / `attachDom` on the `@parasma/snippet/headless` subpath. It
composes the `@public` headless kernel + plugins with the DOM adapters over a
`RootScope`, with no default chrome and no custom-element side effect.
