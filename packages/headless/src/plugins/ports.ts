/**
 * Narrow structural plugin PORTS (the headless contract).
 *
 * A port is a `Pick<Viewer, K>` — the minimal slice of the composed Viewer a
 * plugin factory actually reads. Because `Pick<Viewer, K>` is a definitional
 * SUPERTYPE of `Viewer`, the concrete `Viewer` (in-process OR worker-backed)
 * stays assignable to every ported factory: ZERO capability loss, no call-site
 * churn (mount.ts passes the same `viewer`). What narrows is only the TYPE'S
 * REQUIREMENT — a plugin can now be driven by a fake that implements just the 2-3
 * members the port names (the cursor.test.ts double-cast disappears), which is
 * the orthogonality / testability win.
 *
 * These are TYPES ONLY — no runtime, no adapter change. The ports narrow the
 * VIEWER (tiers 1+2+3 of the engine model), which is DISTINCT from the internal
 * tier-1 EngineFacade the worker satisfies.
 *
 * ADOPTED — every Viewer-taking plugin names its port:
 *   - {@link LoaderViewer}    → loaderPlugin    (load + events archetype)
 *   - {@link CursorViewer}    → cursorPlugin    (flagship query/hit-test testability)
 *   - {@link LinksViewer}     → linksPlugin     (second query archetype)
 *   - {@link ViewportViewer}  → viewportPlugin  (single page-size read)
 *   - {@link SheetsViewer}    → sheetsPlugin    (the sheet model read)
 *   - {@link GridViewer}      → gridPlugin      (extent + cell/pane hit-tests)
 *   - {@link CommentsViewer}  → commentsPlugin  (comment model + rect hit-test)
 *   - {@link SearchViewer}    → searchPlugin    (page/sheet iteration + highlight)
 *   - {@link SelectionViewer} → selectionPlugin (the full hit-test/set/clear surface)
 *   - {@link ClipboardViewer} → clipboardPlugin (extract + image/chart seams)
 *   - {@link FontsViewer}     → fontsPlugin     (events + font control surface)
 *
 * The selection port has 6 members and clipboard 7: the unified
 * `hitTest`/`setSelection`/`extractRange` verbs replaced earlier per-format
 * triplicates, so the ports name ONE spelling per concept.
 *
 * PORTLESS — the two leaf plugins whose factories take `()` and read NO Viewer:
 *   - toolPlugin / zoomPlugin — pure interaction/value state, nothing to port.
 */

import type { Viewer } from "@parasma/core";

/**
 * @public The Viewer surface {@link loaderPlugin} needs: drive a load and bridge
 * the Viewer's native streaming events (pageReady/loadComplete/imageReady) onto
 * the kernel bus.
 */
export type LoaderViewer = Pick<Viewer, "load" | "on">;

/**
 * @public The Viewer surface {@link cursorPlugin} needs: the three hit-tests it
 * resolves a cursor keyword from. A fake implementing exactly these three drives
 * the plugin in a unit test (no `as unknown as Viewer`).
 */
export type CursorViewer = Pick<Viewer, "linkRects" | "commentRects" | "cellAt">;

/**
 * @public The Viewer surface {@link linksPlugin} needs: the link hit-test plus
 * the bookmark resolve for an internal-anchor decision.
 */
export type LinksViewer = Pick<Viewer, "linkRects" | "bookmarkPosition">;

/**
 * @public The Viewer surface {@link viewportPlugin} needs: the single
 * `pageSize()` read it captures once on the first pageReady.
 */
export type ViewportViewer = Pick<Viewer, "pageSize">;

/**
 * @public The Viewer surface {@link sheetsPlugin} needs: the post-layout sheet
 * model read (`sheetInfo()`).
 */
export type SheetsViewer = Pick<Viewer, "sheetInfo">;

/**
 * @public The Viewer surface {@link gridPlugin} needs: the sheet extent + the
 * grid cell / pane hit-tests it sizes + queries with.
 */
export type GridViewer = Pick<Viewer, "gridExtent" | "gridCellAt" | "gridPaneInfo">;

/**
 * @public The Viewer surface {@link commentsPlugin} needs: the comment model
 * read + the per-page comment-rect hit-test.
 */
export type CommentsViewer = Pick<Viewer, "comments" | "commentRects">;

/**
 * @public The Viewer surface {@link searchPlugin} needs: the SML/WML iteration
 * bounds (`sheetInfo`/`pageCount`), the bounded per-page search primitive, and
 * the highlight state set the next paint consumes.
 */
export type SearchViewer = Pick<
  Viewer,
  "sheetInfo" | "pageCount" | "searchPage" | "setSearchHighlight" | "resolveSearchRect"
>;

/**
 * @public The Viewer surface {@link selectionPlugin} needs: the
 * two format-neutral hit-tests, the two format-neutral setters, the
 * used-extent read behind a grid header click, and `documentKind` (read ONLY
 * for the one per-format interaction semantic — a background click clears on a
 * slide, keeps on a page — never for verb routing). A fake implementing just
 * these 6 drives the plugin with none of the Viewer's load/font/paint
 * machinery.
 */
export type SelectionViewer = Pick<
  Viewer,
  | "hitTest"
  | "hitTestObject"
  | "setSelection"
  | "setSelectedObject"
  | "gridContentBounds"
  | "documentKind"
>;

/**
 * @public The Viewer surface {@link clipboardPlugin} needs: the ONE
 * flavor-neutral extract (async + the sync-in-process copy-event hatch) plus
 * the object / image / chart seams the flavor providers read.
 */
export type ClipboardViewer = Pick<
  Viewer,
  | "extractRange"
  | "extractRangeSyncInProcess"
  | "objectInfo"
  | "imageOriginal"
  | "imageBitmapFor"
  | "chartData"
  | "renderChartToCanvas"
>;

/**
 * @public The Viewer surface {@link fontsPlugin} needs: the native font events
 * (bridged via the generic `on`), the resolver / local-fonts controls, and the
 * provenance / debug-probe reads for the `fonts.debug` report.
 */
export type FontsViewer = Pick<
  Viewer,
  | "on"
  | "setFontUrlResolver"
  | "setLocalFontsEnabled"
  | "fontProvenance"
  | "debugFonts"
  | "isRefreshingFonts"
  | "fontPermissionState"
>;
