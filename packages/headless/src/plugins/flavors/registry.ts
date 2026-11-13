/**
 * Built-in flavor providers: text/plain + text/html (the RTE
 * ceiling) for text selections, plus the image, chart and SML cells flavors.
 * The text flavors are memoized over ONE `ctx.fragment()` extractRange. Further
 * providers drop in via `clipboard.registerFlavor` with NO change here.
 */
import type { CopyContext, FlavorProvider, SelectionKind } from "./types";
import { serializeTextPlain } from "./text";
import { serializeTextHtml } from "./html";
import { buildImageResolver, imageHtmlProvider, imagePngProvider } from "./image";
import { serializeCellsHtml, serializeCellsTsv } from "./sml";
import {
  buildChartResolver,
  chartHtmlProvider,
  chartPngProvider,
  chartTsvProvider,
} from "./chart";

/** text/plain for the WML/PML text selection kinds (PML shares it — a PML
 *  shape/cell is paragraphs+runs, the SAME DocFragment). */
export const textPlainProvider: FlavorProvider = {
  mime: "text/plain",
  appliesTo: (k: SelectionKind) => k === "wml-text" || k === "pml-text",
  provide: async (_k: SelectionKind, ctx: CopyContext): Promise<string> =>
    serializeTextPlain(await ctx.fragment()),
};

/** text/html RTE ceiling for the WML/PML text selection kinds (sanitized;
 *  PML shares it via the neutral DocFragment). Inline-image
 *  data-URLs are injected at COPY TIME: the resolver is built from THIS
 *  fragment's originals, never pre-serialized per selection change.
 *  Chart ride-along: `chart::` image runs resolve through the chart raster resolver
 *  (a 4× PNG rendered at copy time), composed FIRST so a chart rel never falls
 *  through to the image-bytes path. */
export const textHtmlProvider: FlavorProvider = {
  mime: "text/html",
  appliesTo: (k: SelectionKind) => k === "wml-text" || k === "pml-text",
  provide: async (_k: SelectionKind, ctx: CopyContext): Promise<string> => {
    const f = await ctx.fragment();
    // ONE budget ledger across both resolvers — their combined data-URLs
    // respect the single totalHtmlBytes cap.
    const ledger = { bytes: 0 };
    const images = await buildImageResolver(f, ctx, ledger);
    const charts = await buildChartResolver(f, ctx, ledger);
    return serializeTextHtml(f, ctx.budgets, (relId) => charts(relId) ?? images(relId));
  },
};

/** SML cells range → TSV (text/plain, Excel quoting). */
export const smlTsvProvider: FlavorProvider = {
  mime: "text/plain",
  appliesTo: (k: SelectionKind) => k === "sml-range",
  provide: async (_k: SelectionKind, ctx: CopyContext): Promise<string> =>
    serializeCellsTsv(await ctx.cellFragment()),
};

/** SML cells range → HTML `<table>` (text/html, colspan/rowspan + escaping). */
export const smlHtmlProvider: FlavorProvider = {
  mime: "text/html",
  appliesTo: (k: SelectionKind) => k === "sml-range",
  provide: async (_k: SelectionKind, ctx: CopyContext): Promise<string> =>
    serializeCellsHtml(await ctx.cellFragment()),
};

/** The built-in provider set: text (plain + html) + image object (png + html) +
 *  chart object (png + html + data-TSV) + SML cells range (TSV +
 *  HTML table). */
export function builtinFlavorProviders(): FlavorProvider[] {
  return [
    textPlainProvider,
    textHtmlProvider,
    imagePngProvider,
    imageHtmlProvider,
    chartPngProvider,
    chartHtmlProvider,
    chartTsvProvider,
    smlTsvProvider,
    smlHtmlProvider,
  ];
}
