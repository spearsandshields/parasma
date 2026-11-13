/**
 * Chart flavor tests. Pure-TS over a mocked CopyContext:
 * the exact Excel source-data TSV layout, the 4× raster geometry through a
 * mocked OffscreenCanvas, the per-image cap → placeholder path, the
 * worker clean skip (raster seam false → png omitted / html placeholder while
 * the TSV survives), raster-failure isolation, and the ride-along
 * `buildChartResolver` (chart:: rels pre-rendered at copy time; non-chart rels
 * untouched).
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ChartCopyData, DocFragment } from "@parasma/core";
import {
  buildChartResolver,
  chartHtmlProvider,
  chartPngProvider,
  chartTsvProvider,
  CHART_RASTER_SCALE,
  serializeChartTsv,
} from "./chart";
import { DEFAULT_BUDGETS, type CopyBudgets, type CopyContext } from "./types";

// ── the 2-series × 3-category model (mirrors the native synthetic fixture) ───
const DATA: ChartCopyData = {
  categories: ["Q1", "Q2", "Q3"],
  series: [
    { name: "Alpha", values: [10, 24, 17] },
    { name: "Beta", values: [4.5, 6, 7.25] },
  ],
  wPx: 430,
  hPx: 216,
};

// ── mock CopyContext ─────────────────────────────────────────────────────────
interface MockOpts {
  chart?: ChartCopyData | null;
  chartKey?: string | null;
  /** What the raster seam reports (false = worker/unknown clean skip). */
  rasterOk?: boolean;
  /** Bytes the mocked OffscreenCanvas encodes to (sizes the cap tests). */
  pngBytes?: Uint8Array;
  budgets?: CopyBudgets;
  onRaster?: (key: string, w: number, h: number, scale: number) => void;
}
function mockCtx(o: MockOpts): CopyContext {
  return {
    fragment: async () => ({ blocks: [] }),
    cellFragment: async () => ({ rows: [], rowCount: 0, totalRows: 0, truncated: false, reason: null }),
    budgets: o.budgets ?? DEFAULT_BUDGETS,
    object: async () => null, // charts NEVER resolve objectInfo (by design)
    imageOriginal: async () => null,
    imageBitmap: async () => null,
    encodePng: async () => new Blob(["png"], { type: "image/png" }),
    chartKey: o.chartKey !== undefined ? o.chartKey : "chart::rId3",
    chart: async () => (o.chart !== undefined ? o.chart : DATA),
    renderChartToCanvas: async (key, _ctx, w, h, scale) => {
      o.onRaster?.(key, w, h, scale);
      return o.rasterOk ?? true;
    },
  };
}

/** Install a minimal OffscreenCanvas whose convertToBlob yields `bytes`. */
function stubOffscreenCanvas(bytes: Uint8Array, created?: Array<{ w: number; h: number }>) {
  class FakeOffscreenCanvas {
    constructor(
      public width: number,
      public height: number,
    ) {
      created?.push({ w: width, h: height });
    }
    getContext(_kind: string): object {
      return {};
    }
    async convertToBlob(_opts?: { type?: string }): Promise<Blob> {
      return new Blob([bytes as unknown as BlobPart], { type: "image/png" });
    }
  }
  vi.stubGlobal("OffscreenCanvas", FakeOffscreenCanvas);
}

afterEach(() => {
  vi.unstubAllGlobals();
});

// ── TSV (the Excel source-data layout, pinned byte-exact) ────────────────────
describe("serializeChartTsv", () => {
  it("emits the exact Excel source-data layout", () => {
    expect(serializeChartTsv(DATA)).toBe(
      "\tAlpha\tBeta\r\nQ1\t10\t4.5\r\nQ2\t24\t6\r\nQ3\t17\t7.25",
    );
  });
  it("numbers are plain (no locale commas)", () => {
    const d: ChartCopyData = {
      categories: ["A"],
      series: [{ name: "S", values: [1234567.5] }],
      wPx: 0,
      hPx: 0,
    };
    expect(serializeChartTsv(d)).toBe("\tS\r\nA\t1234567.5");
  });
  it("quotes names/categories carrying TSV metacharacters", () => {
    const d: ChartCopyData = {
      categories: ['Q"1'],
      series: [{ name: "A\tB", values: [1] }],
      wPx: 0,
      hPx: 0,
    };
    expect(serializeChartTsv(d)).toBe('\t"A\tB"\r\n"Q""1"\t1');
  });
  it("ragged series pad with empty cells; missing categories stay empty", () => {
    const d: ChartCopyData = {
      categories: ["Q1"],
      series: [
        { name: "A", values: [1, 2] },
        { name: "B", values: [3] },
      ],
      wPx: 0,
      hPx: 0,
    };
    expect(serializeChartTsv(d)).toBe("\tA\tB\r\nQ1\t1\t3\r\n\t2\t");
  });
});

describe("chartTsvProvider", () => {
  it("applies to BOTH chart kinds (wml inline + sml grid) and nothing else", () => {
    expect(chartTsvProvider.appliesTo("wml-object-chart")).toBe(true);
    expect(chartTsvProvider.appliesTo("sml-object-chart")).toBe(true);
    expect(chartTsvProvider.appliesTo("wml-object-image")).toBe(false);
    expect(chartTsvProvider.appliesTo("sml-range")).toBe(false);
    expect(chartTsvProvider.appliesTo("wml-text")).toBe(false);
  });
  it("serializes ctx.chart(); null model → null (flavor omitted)", async () => {
    expect(await chartTsvProvider.provide("wml-object-chart", mockCtx({}))).toBe(
      serializeChartTsv(DATA),
    );
    expect(await chartTsvProvider.provide("wml-object-chart", mockCtx({ chart: null }))).toBeNull();
  });
  it("copies even when the raster seam is dead (worker mode)", async () => {
    // No OffscreenCanvas stub + rasterOk:false — the TSV never touches either.
    expect(
      await chartTsvProvider.provide("wml-object-chart", mockCtx({ rasterOk: false })),
    ).toBe(serializeChartTsv(DATA));
  });
});

// ── the SML grid chart kind (xlsx embedded charts) — the SAME providers fire ──
describe("sml-object-chart reuses the WML chart flavors verbatim", () => {
  const SML_KEY = "chart::sml::0::rId1";
  it("all three providers apply to sml-object-chart", () => {
    expect(chartPngProvider.appliesTo("sml-object-chart")).toBe(true);
    expect(chartHtmlProvider.appliesTo("sml-object-chart")).toBe(true);
    expect(chartTsvProvider.appliesTo("sml-object-chart")).toBe(true);
  });
  it("TSV serializes the shared chartData model for an sml key", async () => {
    expect(
      await chartTsvProvider.provide("sml-object-chart", mockCtx({ chartKey: SML_KEY })),
    ).toBe(serializeChartTsv(DATA));
  });
  it("PNG rasters at the SAME 4× scale through the sheet-namespaced key", async () => {
    stubOffscreenCanvas(new Uint8Array([1, 2, 3]));
    const calls: Array<{ key: string; w: number; h: number; scale: number }> = [];
    const ctx = mockCtx({
      chartKey: SML_KEY,
      onRaster: (key, w, h, scale) => calls.push({ key, w, h, scale }),
    });
    const blob = await chartPngProvider.provide("sml-object-chart", ctx);
    expect(blob).toBeInstanceOf(Blob);
    expect(calls).toEqual([
      { key: SML_KEY, w: 430 * CHART_RASTER_SCALE, h: 216 * CHART_RASTER_SCALE, scale: CHART_RASTER_SCALE },
    ]);
  });
  it("html emits the <img data-url> at DISPLAY px; worker mode degrades to the placeholder while the TSV survives", async () => {
    stubOffscreenCanvas(new Uint8Array([1, 2, 3]));
    const html = await chartHtmlProvider.provide(
      "sml-object-chart",
      mockCtx({ chartKey: SML_KEY }),
    );
    expect(html).toBe(
      '<meta charset="utf-8"><img src="data:image/png;base64,AQID" width="430" height="216">',
    );
    const worker = mockCtx({ chartKey: SML_KEY, rasterOk: false });
    expect(await chartHtmlProvider.provide("sml-object-chart", worker)).toBe(
      '<meta charset="utf-8"><img alt="chart" width="430" height="216">',
    );
    expect(await chartPngProvider.provide("sml-object-chart", worker)).toBeNull();
    expect(await chartTsvProvider.provide("sml-object-chart", worker)).toBe(
      serializeChartTsv(DATA),
    );
  });
});

// ── image/png (the 4× raster) ────────────────────────────────────────────────
describe("chartPngProvider", () => {
  it("rasters at 4× display px through the engine seam", async () => {
    const created: Array<{ w: number; h: number }> = [];
    stubOffscreenCanvas(new Uint8Array([1, 2, 3]), created);
    const calls: Array<{ key: string; w: number; h: number; scale: number }> = [];
    const ctx = mockCtx({ onRaster: (key, w, h, scale) => calls.push({ key, w, h, scale }) });
    const blob = await chartPngProvider.provide("wml-object-chart", ctx);
    expect(blob).toBeInstanceOf(Blob);
    // Canvas + seam geometry: 4 × (430 × 216), scale 4.
    expect(created).toEqual([{ w: 430 * CHART_RASTER_SCALE, h: 216 * CHART_RASTER_SCALE }]);
    expect(calls).toEqual([
      { key: "chart::rId3", w: 430 * CHART_RASTER_SCALE, h: 216 * CHART_RASTER_SCALE, scale: CHART_RASTER_SCALE },
    ]);
  });
  it("worker mode (seam false) → null, no throw (clean skip)", async () => {
    stubOffscreenCanvas(new Uint8Array(8));
    expect(await chartPngProvider.provide("wml-object-chart", mockCtx({ rasterOk: false }))).toBeNull();
  });
  it("no OffscreenCanvas → null (clean skip)", async () => {
    vi.stubGlobal("OffscreenCanvas", undefined);
    expect(await chartPngProvider.provide("wml-object-chart", mockCtx({}))).toBeNull();
  });
  it("over the per-image cap → null (placeholder path)", async () => {
    stubOffscreenCanvas(new Uint8Array(64));
    const budgets: CopyBudgets = { perImageBytes: 16, totalHtmlBytes: 1024 };
    expect(await chartPngProvider.provide("wml-object-chart", mockCtx({ budgets }))).toBeNull();
  });
  it("zero display px (no painted placement) → null", async () => {
    stubOffscreenCanvas(new Uint8Array(8));
    const ctx = mockCtx({ chart: { ...DATA, wPx: 0, hPx: 0 } });
    expect(await chartPngProvider.provide("wml-object-chart", ctx)).toBeNull();
  });
  it("a throwing raster is caught → null (isolation)", async () => {
    stubOffscreenCanvas(new Uint8Array(8));
    const ctx = mockCtx({});
    ctx.renderChartToCanvas = async () => {
      throw new Error("boom");
    };
    expect(await chartPngProvider.provide("wml-object-chart", ctx)).toBeNull();
  });
});

// ── text/html (data-URL of the same raster at DISPLAY px) ───────────────────
describe("chartHtmlProvider", () => {
  it("emits an <img data-url> sized at DISPLAY px", async () => {
    stubOffscreenCanvas(new Uint8Array([1, 2, 3]));
    const html = await chartHtmlProvider.provide("wml-object-chart", mockCtx({}));
    // base64([1,2,3]) = "AQID"; width/height are the DISPLAY px (430×216).
    expect(html).toBe(
      '<meta charset="utf-8"><img src="data:image/png;base64,AQID" width="430" height="216">',
    );
  });
  it("worker mode (seam false) → placeholder with DISPLAY dims (text still copies)", async () => {
    stubOffscreenCanvas(new Uint8Array(8));
    const html = await chartHtmlProvider.provide("wml-object-chart", mockCtx({ rasterOk: false }));
    expect(html).toBe('<meta charset="utf-8"><img alt="chart" width="430" height="216">');
  });
  it("over the cap → placeholder", async () => {
    stubOffscreenCanvas(new Uint8Array(64));
    const budgets: CopyBudgets = { perImageBytes: 16, totalHtmlBytes: 1024 };
    const html = await chartHtmlProvider.provide("wml-object-chart", mockCtx({ budgets }));
    expect(html).toBe('<meta charset="utf-8"><img alt="chart" width="430" height="216">');
  });
  it("null model → null (flavor omitted)", async () => {
    expect(await chartHtmlProvider.provide("wml-object-chart", mockCtx({ chart: null }))).toBeNull();
  });
});

// ── ride-along resolver (text copy sweeping a chart paragraph) ───────────────
const rideAlongFrag = (relIds: string[]): DocFragment => ({
  blocks: [
    {
      kind: "para",
      para: {
        runs: relIds.map((relId) => ({
          text: "",
          bold: false, italic: false, underline: "none", strike: false, doubleStrike: false,
          fontFamily: "Calibri", fontSizePt: 11, color: "#000000", highlight: null,
          runShading: null, vertAlign: "baseline", link: null,
          image: { relId, wPx: 430, hPx: 216 }, footnoteMark: null,
        })),
        alignment: "start", dir: "ltr", list: null,
      },
    },
  ],
});

describe("buildChartResolver (ride-along)", () => {
  it("pre-renders a data-URL for each chart:: rel; non-chart rels resolve null", async () => {
    stubOffscreenCanvas(new Uint8Array([1, 2, 3]));
    const frag = rideAlongFrag(["chart::rId3", "rId7"]);
    const resolve = await buildChartResolver(frag, mockCtx({}));
    expect(resolve("chart::rId3")).toBe("data:image/png;base64,AQID");
    expect(resolve("rId7")).toBeNull(); // image rels belong to buildImageResolver
  });
  it("worker mode skips cleanly (resolver null → <img alt> placeholder)", async () => {
    stubOffscreenCanvas(new Uint8Array(8));
    const frag = rideAlongFrag(["chart::rId3"]);
    const resolve = await buildChartResolver(frag, mockCtx({ rasterOk: false }));
    expect(resolve("chart::rId3")).toBeNull();
  });
  it("caps apply: per-image + total budget", async () => {
    stubOffscreenCanvas(new Uint8Array(64));
    const frag = rideAlongFrag(["chart::rId3"]);
    const perImage = await buildChartResolver(
      frag,
      mockCtx({ budgets: { perImageBytes: 16, totalHtmlBytes: 1024 } }),
    );
    expect(perImage("chart::rId3")).toBeNull();
    const total = await buildChartResolver(
      frag,
      mockCtx({ budgets: { perImageBytes: 1024, totalHtmlBytes: 8 } }),
    );
    expect(total("chart::rId3")).toBeNull();
  });
  it("a throwing raster skips that rel only", async () => {
    stubOffscreenCanvas(new Uint8Array([1, 2, 3]));
    const frag = rideAlongFrag(["chart::rBoom", "chart::rId3"]);
    const ctx = mockCtx({});
    const inner = ctx.renderChartToCanvas;
    ctx.renderChartToCanvas = async (key, c2d, w, h, s) => {
      if (key === "chart::rBoom") throw new Error("boom");
      return inner(key, c2d, w, h, s);
    };
    const resolve = await buildChartResolver(frag, ctx);
    expect(resolve("chart::rBoom")).toBeNull();
    expect(resolve("chart::rId3")).toBe("data:image/png;base64,AQID");
  });
});
