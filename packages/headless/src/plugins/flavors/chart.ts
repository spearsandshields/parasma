/**
 * Chart flavors (the WML inline-chart copy; the SAME
 * providers also serve the `sml-object-chart` kind — an SML grid chart copies with IDENTICAL flavors/scale/TSV rules; the
 * providers only read `ctx.chartKey` + the format-agnostic ctx seams). Three
 * producers for the `wml-object-chart` / `sml-object-chart` selection kinds:
 *   - `image/png`  — a 4×-display-size raster (print-grade) of
 *     the chart painted by the SHARED engine vector painter
 *     (`crate::chart::render_chart` via the `renderChartToCanvas` seam).
 *   - `text/html`  — `<img src="data:image/png;base64,…">` of the SAME raster,
 *     with `width`/`height` = DISPLAY px so it pastes at layout size.
 *   - `text/plain` — the data-TSV in the Excel SOURCE-DATA layout: header row
 *     `["", series1, series2, …]`, then one `[category, v1, v2, …]` row per
 *     category; cells joined with `\t`, rows with `\r\n`, numbers formatted
 *     plainly (never locale-formatted). Sourced from `ctx.chart()` — the SAME
 *     parsed model the painter uses (NO new XML parsing).
 * Plus `buildChartResolver` — the RIDE-ALONG seam: a text-range copy sweeping a
 * chart paragraph carries `FragRun.image` with the `chart::` key; this
 * pre-renders a PNG data-URL per such rel AT COPY TIME (never cached per
 * selection change) so the chart rides along as a picture in text/html.
 *
 * Per-flavor isolation: every raster path catches → null/placeholder — a
 * failed raster never sinks the text flavors. Budgets: a raster blob over
 * the per-image cap takes the placeholder path. Worker mode: the raster
 * seam returns false (a live canvas context cannot cross the RPC boundary) →
 * png omitted + html placeholder, while the TSV (structured-clone-safe
 * `chartData` RPC) still copies. Base64 alphabet + the fixed image/png mime
 * carry no `"<>&`, so the data-URL is emitted unescaped (the image.ts
 * invariant).
 */
import type { ChartCopyData, DocFragment, FragBlock, FragImage } from "@parasma/core";
import type { CopyContext, FlavorProvider, ImageResolver, SelectionKind } from "./types";
import { base64FromBytes } from "./image";
import { imgDims } from "./html";
import { quoteTsvCell } from "./sml";

/** The raster scale: 4× the chart's DISPLAY px (print-grade). */
export const CHART_RASTER_SCALE = 4;

/** A number cell, formatted plainly (`String(v)` — no locale commas); absent /
 *  non-finite values emit an EMPTY cell (a ragged series keeps its columns). */
function fmtNum(v: number | undefined): string {
  return v == null || !Number.isFinite(v) ? "" : String(v);
}

/**
 * The data-TSV in the Excel source-data layout. Header row =
 * `["", …series names]`; one row per category `[category, …values]`. `\t`
 * cells, `\r\n` rows (no trailing newline — the sml.ts TSV discipline, whose
 * `quoteTsvCell` guards names/categories carrying tabs/quotes/newlines).
 */
export function serializeChartTsv(d: ChartCopyData): string {
  const rows: string[] = [];
  rows.push(["", ...d.series.map((s) => quoteTsvCell(s.name))].join("\t"));
  const n = d.series.reduce((m, s) => Math.max(m, s.values.length), d.categories.length);
  for (let i = 0; i < n; i++) {
    rows.push(
      [quoteTsvCell(d.categories[i] ?? ""), ...d.series.map((s) => fmtNum(s.values[i]))].join(
        "\t",
      ),
    );
  }
  return rows.join("\r\n");
}

/**
 * Rasterize chart `key` at `CHART_RASTER_SCALE` × its display px through the
 * engine's shared vector painter, onto an OffscreenCanvas → PNG Blob. `null`
 * is the CLEAN SKIP: no OffscreenCanvas, worker mode (seam returns
 * false), unknown key, unsupported chart, or zero display px.
 */
export async function renderChartPngBlob(
  ctx: CopyContext,
  key: string,
  wPx: number,
  hPx: number,
): Promise<Blob | null> {
  // Memoized PER COPY (the CopyContext is one object per copy gesture): the png
  // and html providers both need the same raster, and without this each copy
  // would paint + PNG-encode the 4× canvas twice. Keyed on
  // key+dims; the WeakMap dies with the context.
  let memo = rasterMemo.get(ctx);
  if (!memo) {
    memo = new Map();
    rasterMemo.set(ctx, memo);
  }
  const mk = `${key}@${wPx}x${hPx}`;
  let p = memo.get(mk);
  if (!p) {
    p = renderChartPngBlobUncached(ctx, key, wPx, hPx);
    memo.set(mk, p);
  }
  return p;
}

const rasterMemo = new WeakMap<CopyContext, Map<string, Promise<Blob | null>>>();

async function renderChartPngBlobUncached(
  ctx: CopyContext,
  key: string,
  wPx: number,
  hPx: number,
): Promise<Blob | null> {
  if (!(wPx > 0) || !(hPx > 0)) return null; // no painted placement
  const OC = (globalThis as { OffscreenCanvas?: typeof OffscreenCanvas }).OffscreenCanvas;
  if (!OC) return null;
  const w = Math.max(1, Math.round(wPx * CHART_RASTER_SCALE));
  const h = Math.max(1, Math.round(hPx * CHART_RASTER_SCALE));
  const canvas = new OC(w, h);
  const c2d = canvas.getContext("2d");
  if (!c2d) return null;
  const ok = await ctx.renderChartToCanvas(key, c2d, w, h, CHART_RASTER_SCALE);
  if (!ok) return null; // worker mode / unknown / unsupported → clean skip
  return canvas.convertToBlob({ type: "image/png" });
}

/** The over-cap / unrasterizable html fallback (mirrors the image `<img alt>`). */
function chartPlaceholder(dim: string): string {
  return `<meta charset="utf-8"><img alt="chart"${dim}>`;
}

/** image/png for a selected chart object — the 4× raster. */
export const chartPngProvider: FlavorProvider = {
  mime: "image/png",
  appliesTo: (k: SelectionKind) => k === "wml-object-chart" || k === "sml-object-chart",
  provide: async (_k: SelectionKind, ctx: CopyContext): Promise<Blob | null> => {
    const key = ctx.chartKey;
    if (!key) return null;
    try {
      const data = await ctx.chart();
      if (!data) return null;
      const blob = await renderChartPngBlob(ctx, key, data.wPx, data.hPx);
      if (!blob) return null;
      if (blob.size > ctx.budgets.perImageBytes) return null; // per-image cap → flavor omitted
      return blob;
    } catch {
      return null; // a failed raster must not sink the text flavors
    }
  },
};

/** text/html `<img data-url>` of the SAME 4× raster, sized at DISPLAY px. */
export const chartHtmlProvider: FlavorProvider = {
  mime: "text/html",
  appliesTo: (k: SelectionKind) => k === "wml-object-chart" || k === "sml-object-chart",
  provide: async (_k: SelectionKind, ctx: CopyContext): Promise<string | null> => {
    const key = ctx.chartKey;
    if (!key) return null;
    const data = await ctx.chart();
    if (!data) return null;
    const dim = imgDims(data.wPx, data.hPx); // DISPLAY px (pastes at layout size)
    try {
      const blob = await renderChartPngBlob(ctx, key, data.wPx, data.hPx);
      if (!blob || blob.size > ctx.budgets.perImageBytes) {
        return chartPlaceholder(dim); // worker / over-cap → placeholder path
      }
      const bytes = new Uint8Array(await blob.arrayBuffer());
      return `<meta charset="utf-8"><img src="data:image/png;base64,${base64FromBytes(bytes)}"${dim}>`;
    } catch {
      return chartPlaceholder(dim); // a failed raster degrades to the placeholder
    }
  },
};

/** text/plain — the data-TSV of the chart series (Excel source-data layout). */
export const chartTsvProvider: FlavorProvider = {
  mime: "text/plain",
  appliesTo: (k: SelectionKind) => k === "wml-object-chart" || k === "sml-object-chart",
  provide: async (_k: SelectionKind, ctx: CopyContext): Promise<string | null> => {
    const data = await ctx.chart();
    return data ? serializeChartTsv(data) : null;
  },
};

/** Every distinct `chart::` image ref in the fragment, in DOC ORDER, with its
 *  DISPLAY px (the FragImage wPx/hPx the layout stamped on the run). */
function collectChartRefs(frag: DocFragment): FragImage[] {
  const seen = new Set<string>();
  const out: FragImage[] = [];
  const visit = (blocks: FragBlock[]): void => {
    for (const b of blocks) {
      if (b.kind === "para") {
        for (const r of b.para.runs) {
          if (r.image && r.image.relId.startsWith("chart::") && !seen.has(r.image.relId)) {
            seen.add(r.image.relId);
            out.push(r.image);
          }
        }
      } else {
        for (const row of b.table.rows) for (const c of row.cells) visit(c.blocks);
      }
    }
  };
  visit(frag.blocks);
  return out;
}

/**
 * The RIDE-ALONG resolver: pre-render a PNG data-URL for each
 * `chart::` rel present in the fragment, AT COPY TIME only (never in the
 * per-selection cache), so a text copy sweeping a chart paragraph pastes the
 * chart as a picture. Budgets mirror `buildImageResolver`: per-image cap on
 * the raster blob, total-HTML cap on accumulated data-URLs. A rel is SKIPPED
 * (→ `<img alt>` placeholder) in worker mode / over-cap / on any raster
 * failure (the text flavors always survive). text/plain for a swept-in
 * chart run stays EMPTY (image runs carry no text; the TSV is exclusively the
 * direct chart-object copy's).
 */
export async function buildChartResolver(
  frag: DocFragment,
  ctx: CopyContext,
  // Shared total-HTML budget ledger (see buildImageResolver): one text/html
  // copy threads ONE ledger through both resolvers so their combined data-URLs
  // respect the single totalHtmlBytes cap.
  ledger: { bytes: number } = { bytes: 0 },
): Promise<ImageResolver> {
  const map = new Map<string, string>();
  for (const ref of collectChartRefs(frag)) {
    try {
      const blob = await renderChartPngBlob(ctx, ref.relId, ref.wPx, ref.hPx);
      if (!blob) continue; // worker mode / unsupported
      if (blob.size > ctx.budgets.perImageBytes) continue; // per-image cap
      const bytes = new Uint8Array(await blob.arrayBuffer());
      const url = `data:image/png;base64,${base64FromBytes(bytes)}`;
      if (ledger.bytes + url.length > ctx.budgets.totalHtmlBytes) continue; // total budget
      map.set(ref.relId, url);
      ledger.bytes += url.length;
    } catch {
      // A failed chart raster must not sink the surrounding html copy.
    }
  }
  return (relId: string) => map.get(relId) ?? null;
}
