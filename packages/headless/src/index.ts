/**
 * @parasma/headless — framework-agnostic plugin kernel + the blessed
 * headless contract.
 *
 * OFFICIALIZED SURFACE ("tier, do NOT remove"): every symbol below
 * is exported from `.`; nothing a consumer can reach today became unreachable.
 * The barrel is TIERED into two clearly-bannered sections:
 *
 *   • @public   — the blessed contract a BYO-chrome consumer depends on: the
 *                 Kernel, the 13 plugin factories + their state/query/option
 *                 types, KernelContext, the narrow plugin PORTS, the clipboard
 *                 flavor serializers, DocumentInfo, and the ordered-teardown
 *                 primitive.
 *   • @advanced — escape-hatch kernel PRIMITIVES a normal consumer never needs
 *                 but a power user CAN still reach (createBus/createCommandBus/
 *                 createSliceStore/createPaintSurface/createRegistry + their
 *                 types). ALSO reachable via the additive `@parasma/headless/
 *                 internal` subpath (a discoverability tier, not a hiding one).
 *
 * See docs/contract/headless-public-contract.md.
 */

// ===========================================================================
// @public — the blessed headless contract
// ===========================================================================

/**
 * @public The kernel — register plugins, boot, exec typed commands, subscribe the
 * replayable bus. `getPlugin<T>` stays as a typed @advanced escape hatch on it.
 */
export { createKernel } from "./kernel";
export type { Kernel } from "./kernel";

/** @public The surface a plugin factory authors against (commands/bus/paint + the typed getPlugin escape hatch). */
export type { KernelContext } from "./kernel-context";

/** @public The typed command bus contract (register/exec, CommandMap declaration-merge target). */
export type { CommandBus, Command, CommandMap, CommandId, DynamicCommandId } from "./commands";
/** @public `asDynamic` brands a genuinely-dynamic command id for the escape-hatch exec overload. */
export { asDynamic } from "./commands";

/** @public The replayable event bus contract — the cross-plugin read-model. */
export type { Bus, BusEventMap, BusHandler, ReplayPolicy } from "./bus";

/** @public The ordered, Promise-returning teardown primitive backing the viewer handle's destroy(). */
export { runTeardown } from "./teardown";
export type { TeardownStep } from "./teardown";

// ── The 13 plugin factories + their public state/query/option types ──────────
export { loaderPlugin } from "./plugins/loader";
export type { LoaderStatus } from "./plugins/loader";

export { viewportPlugin } from "./plugins/viewport";
export type { ViewportState, PageModel } from "./plugins/viewport";
export { zoomPlugin, ZOOM_MIN, ZOOM_MAX } from "./plugins/zoom";
export type { ZoomState, FitMode } from "./plugins/zoom";

export { cursorPlugin } from "./plugins/cursor";
export type { CursorQuery } from "./plugins/cursor";

export { linksPlugin } from "./plugins/links";
export type { LinkQuery, LinkDecision } from "./plugins/links";

export { sheetsPlugin } from "./plugins/sheets";
export type { SheetInfo, SheetsState } from "./plugins/sheets";

export { gridPlugin } from "./plugins/grid";
export type { GridState, GridMode, GridCellQuery } from "./plugins/grid";

export { commentsPlugin } from "./plugins/comments";
export type { CommentsState, CommentAtQuery } from "./plugins/comments";

export { searchPlugin } from "./plugins/search";
export type {
  SearchState,
  SearchScope,
  SearchStatus,
  SearchPluginOptions,
} from "./plugins/search";

export { selectionPlugin } from "./plugins/selection";
export type {
  SelectionState,
  SelectionQuery,
  MenuQuery,
  CellPos,
  ContextMenuItem,
  ContextMenuContext,
  ContextMenuSeam,
} from "./plugins/selection";

export { toolPlugin } from "./plugins/tool";
export type { ToolMode, ToolState } from "./plugins/tool";

export { clipboardPlugin } from "./plugins/clipboard";
export type { CopyOutcome, CopyPlan, CachedCopy } from "./plugins/clipboard";

export { fontsPlugin } from "./plugins/fonts";
export type { FontsStatus, FontsDebugReport, FontsDebugRow } from "./plugins/fonts";

/**
 * @public Narrow structural plugin PORTS. Each is a `Pick<Viewer,K>`
 * the concrete Viewer satisfies definitionally — the "author-your-own-driver"
 * contract + the testability seam. Adopted on loader/cursor/links; the recipe is
 * documented for the deferred rest (see ./plugins/ports.ts).
 */
export type { LoaderViewer, CursorViewer, LinksViewer } from "./plugins/ports";

// ── Clipboard flavor serializers (pure, DocFragment→string) ──────────────────
// @public LOAD-BEARING — the DOM clipboard adapter's tier-3 sync fallback (setData)
// serializes inside the copy event with these.
export { sanitizeHref } from "./plugins/flavors/sanitize";
export { serializeTextPlain } from "./plugins/flavors/text";
export { serializeTextHtml } from "./plugins/flavors/html";
export { serializeTable, WML_TABLE, SML_TSV } from "./plugins/flavors/table";
export { serializeCellsTsv, serializeCellsHtml, quoteTsvCell } from "./plugins/flavors/sml";
// The chart TSV serializer (pure, ChartCopyData→string) + the
// raster scale pin, exported for tests/hosts (the chart flavors themselves are
// built in; there is no sync tier for a chart copy — the TSV rides the async
// ClipboardItem write).
export { serializeChartTsv, CHART_RASTER_SCALE } from "./plugins/flavors/chart";
export type {
  FlavorMime,
  SelectionKind,
  FlavorProvider,
  CopyContext,
  CopyBudgets,
} from "./plugins/flavors/types";

// @public Re-export the load contract shape so consumers driving loader.open get
// the DocumentInfo type without reaching into @parasma/core directly.
export type { DocumentInfo } from "@parasma/core";

// ===========================================================================
// @advanced — escape-hatch kernel primitives
//
// A normal BYO-chrome consumer NEVER constructs these directly — createKernel()
// wires them internally. They are exported (and additionally re-exported from
// `@parasma/headless/internal`) so a power user assembling a bespoke kernel CAN
// reach them. Nothing here is removed or hidden: this is a discoverability tier.
// ===========================================================================

/** @advanced Construct a replayable event bus directly (createKernel does this for you). */
export { createBus } from "./bus";

/** @advanced Construct a typed command bus directly (createKernel does this for you). */
export { createCommandBus } from "./commands";

/** @advanced Per-plugin single-slice store helper + its types. */
export { createSliceStore } from "./store";
export type { Store, StoreSlice } from "./store";

/** @advanced Construct a paint-intent surface directly + its types. */
export { createPaintSurface } from "./paint";
export type { PaintSurface, PaintIntent, PaintIntentHandler } from "./paint";

/** @advanced The plugin registry (two-phase topo boot) + the Plugin contract + topoSort. */
export { createRegistry, topoSort } from "./registry";
export type { Registry, Plugin } from "./registry";
