/**
 * Flavor-registry types. A FlavorProvider turns a
 * `DocFragment` into ONE clipboard flavor (a string or Blob); the registry is
 * the extension seam, so a new flavor (text/html, image/png, …) drops in with
 * no plugin change. The plugin owns the model/decision; the DOM adapter performs
 * the actual `navigator.clipboard` write (the plugin/DOM boundary).
 *
 * `CopyContext.fragment()` is MEMOIZED per copy so N providers share ONE
 * `viewer.extractRange` round-trip. Budgets are inert for text/plain
 * (no image payload) and enforced by the html/image flavors.
 */
import type { CellFragment, ChartCopyData, DocFragment } from "@parasma/core";

/** A writable clipboard MIME. */
export type FlavorMime = "text/plain" | "text/html" | "image/png";

/** The kind of selection being copied. WML text + image objects; the SML cells
 *  range (TSV + HTML `<table>`); PML shape/cell text (shares the WML text
 *  flavors) + PML pictures (share the image flavors); the WML inline-chart
 *  object (PNG + html data-URL + data-TSV); and the SML
 *  grid chart object, which REUSES the WML chart flavors verbatim (the providers
 *  only read `ctx.chartKey` + the format-agnostic ctx seams). */
export type SelectionKind =
  | "wml-text"
  | "wml-object-image"
  | "wml-object-chart"
  | "sml-range"
  | "sml-object-chart"
  | "pml-text"
  | "pml-object-image";

/** The retained ORIGINAL bytes of an embedded image. */
export interface ImageOriginal {
  mime: string;
  bytes: Uint8Array;
}

/** A selected image object's copy metadata (INTRINSIC dims). */
export interface ObjectImageInfo {
  relId: string;
  mime: string;
  wPx: number;
  hPx: number;
}

/** relId → a ready `data:` URL (or null when unresolved/over-budget). */
export type ImageResolver = (relId: string) => string | null;

/** Encode a decoded bitmap to a PNG Blob (mockable OffscreenCanvas seam). */
export type PngEncoder = (bitmap: ImageBitmap) => Promise<Blob>;

/**
 * Clipboard payload budgets. Inert for text/plain (no per-image inflation);
 * threaded so the HTML flavor can enforce the data-URL + total caps.
 */
export interface CopyBudgets {
  /** Max original bytes per embedded image before an `<img alt>` placeholder. */
  perImageBytes: number;
  /** Max assembled HTML bytes before over-budget images degrade. */
  totalHtmlBytes: number;
}

/** Sensible v1 budgets. */
export const DEFAULT_BUDGETS: CopyBudgets = {
  perImageBytes: 4 * 1024 * 1024, // ~4MB
  totalHtmlBytes: 32 * 1024 * 1024, // ~32MB
};

/**
 * Per-copy context: a MEMOIZED fragment accessor (one extractRange across all
 * providers) plus the budgets and the image/chart seams.
 */
export interface CopyContext {
  /** The selection's DocFragment — resolved at most ONCE per copy (memoized). */
  fragment(): Promise<DocFragment>;
  /** The SML cells fragment (TSV + HTML `<table>`), memoized
   *  per copy. Empty for a non-SML selection. */
  cellFragment(): Promise<CellFragment>;
  budgets: CopyBudgets;
  /** The selected image object's metadata (null for text). */
  object(): Promise<ObjectImageInfo | null>;
  /** The retained ORIGINAL bytes for a relId (null in worker mode). */
  imageOriginal(relId: string): Promise<ImageOriginal | null>;
  /** The decoded intrinsic bitmap for a relId (null in worker mode). */
  imageBitmap(relId: string): Promise<ImageBitmap | null>;
  /** Encode a bitmap → PNG Blob (the injected `PngEncoder`). */
  encodePng(bitmap: ImageBitmap): Promise<Blob>;
  /** The selected chart object's `chart::<rId>` key
   *  (null for every other selection kind). */
  chartKey: string | null;
  /** The selected chart's copy model (series/categories
   *  + display px), resolved at most ONCE per copy (memoized). Null for a
   *  non-chart selection / an unsupported chart. */
  chart(): Promise<ChartCopyData | null>;
  /** The engine chart raster seam
   *  ({@link Viewer.renderChartToCanvas}): paint chart `key` into `ctx` at
   *  `(wPx, hPx)` canvas px with a uniform `scale`. `false` = CLEAN SKIP
   *  (worker mode / unknown key / unsupported chart). */
  renderChartToCanvas(
    key: string,
    ctx: OffscreenCanvasRenderingContext2D | CanvasRenderingContext2D,
    wPx: number,
    hPx: number,
    scale: number,
  ): Promise<boolean>;
}

/**
 * A single clipboard-flavor producer. `appliesTo` gates by selection kind;
 * `provide` returns the serialized flavor or `null` to OMIT it (a null/failed
 * provider must never kill sibling flavors).
 */
export interface FlavorProvider {
  mime: FlavorMime;
  appliesTo(kind: SelectionKind): boolean;
  provide(kind: SelectionKind, ctx: CopyContext): Promise<string | Blob | null>;
}
