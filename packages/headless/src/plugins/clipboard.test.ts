/**
 * plugin-clipboard tests. Boots the plugin over a fake
 * Viewer (counting extractRange) + a stub "selection" dep, driving the flow via
 * `selection:changed` on the bus. Pins: memoized single extractRange across N
 * providers; `clipboard:copied` payload; collapsed/none → no copy; per-flavor
 * robustness (a throwing provider omits only its mime); all-null → `clipboard:error`;
 * registry drop-in; plan() synchronous (extractRange lazy); selection-seq stamping.
 */
import { describe, it, expect } from "vitest";
import { createKernel } from "..";
import type { Plugin } from "../registry";
import type { Viewer, DocFragment } from "@parasma/core";
import { clipboardPlugin, type CopyOutcome, type CopyPlan } from "./clipboard";
import type { FlavorProvider } from "./flavors/types";

const selectionStub: Plugin = { id: "selection", initialize() {}, destroy() {} };

const FRAG: DocFragment = {
  blocks: [
    {
      kind: "para",
      para: {
        runs: [
          {
            text: "Hello",
            bold: false,
            italic: false,
            underline: "none",
            strike: false,
            doubleStrike: false,
            fontFamily: "Calibri",
            fontSizePt: 11,
            color: "#000000",
            highlight: null,
            runShading: null,
            vertAlign: "baseline",
            link: null,
            image: null,
            footnoteMark: null,
          },
        ],
        alignment: "start",
        dir: "ltr",
        list: null,
      },
    },
  ],
};

function fakeViewer(opts: { reject?: boolean } = {}) {
  let extractCalls = 0;
  const viewer = {
    extractRange: async (): Promise<DocFragment | null> => {
      extractCalls++;
      if (opts.reject) throw new Error("boom");
      return FRAG;
    },
    extractRangeSyncInProcess: (): DocFragment | null => FRAG,
  } as unknown as Viewer;
  return { viewer, calls: () => extractCalls };
}

async function boot(viewer: Viewer) {
  const kernel = createKernel();
  kernel.use(selectionStub);
  kernel.use(clipboardPlugin(viewer));
  await kernel.boot();
  return kernel;
}

const textSel = (seq: number) => ({
  kind: "text" as const,
  collapsed: false,
  anchor: { para: 0, byte: 0 },
  focus: { para: 0, byte: 5 },
  seq,
});

describe("plugin-clipboard", () => {
  it("copy() returns the outcome WITHOUT emitting; reportCopied is the sole copied signal", async () => {
    const { viewer } = fakeViewer();
    const kernel = await boot(viewer);
    const copied: Array<{ kind: string; mimes: string[]; charCount: number }> = [];
    kernel.bus.subscribe("clipboard:copied", (e) => copied.push(e));

    kernel.bus.emit("selection:changed", textSel(1));
    expect(await kernel.exec("clipboard.canCopy")).toBe(true);

    const out = (await kernel.exec("clipboard.copy")) as CopyOutcome | null;
    expect(out?.kind).toBe("wml-text");
    // text/html is a built-in alongside text/plain.
    expect(out?.mimes).toEqual(["text/plain", "text/html"]);
    expect(out?.flavors["text/plain"]).toBe("Hello");
    expect(out?.flavors["text/html"]).toContain(`<meta charset="utf-8">`);
    // Serialize does not emit; the copied signal reflects the ACTUAL
    // OS write, which the DOM adapter reports via clipboard.reportCopied.
    expect(copied).toEqual([]);

    await kernel.exec("clipboard.reportCopied", { seq: 1, charCount: out!.charCount });
    expect(copied).toEqual([
      { kind: "wml-text", mimes: ["text/plain", "text/html"], charCount: 5 },
    ]);
  });

  it("extractRange is called EXACTLY ONCE across all providers (memoization)", async () => {
    const { viewer, calls } = fakeViewer();
    const kernel = await boot(viewer);
    // A third provider (beyond the built-in plain+html) that ALSO reads the fragment.
    const pngProvider: FlavorProvider = {
      mime: "image/png",
      appliesTo: (k) => k === "wml-text",
      provide: async (_k, ctx) => {
        await ctx.fragment();
        return "PNGDATA";
      },
    };
    await kernel.exec("clipboard.registerFlavor", pngProvider);
    kernel.bus.emit("selection:changed", textSel(1));

    const out = (await kernel.exec("clipboard.copy")) as CopyOutcome;
    expect(out.mimes.sort()).toEqual(["image/png", "text/html", "text/plain"]);
    expect(calls()).toBe(1); // ONE extractRange shared across all three providers
  });

  it("a collapsed / none selection is not copyable; copy() returns null and emits nothing", async () => {
    const { viewer } = fakeViewer();
    const kernel = await boot(viewer);
    const copied: unknown[] = [];
    kernel.bus.subscribe("clipboard:copied", (e) => copied.push(e));

    kernel.bus.emit("selection:changed", { kind: "none", collapsed: true, seq: 2 });
    expect(await kernel.exec("clipboard.canCopy")).toBe(false);
    expect(await kernel.exec("clipboard.copy")).toBeNull();
    expect(copied).toEqual([]);
  });

  it("a throwing provider is skipped; text/plain still lands, mimes excludes it", async () => {
    const { viewer } = fakeViewer();
    const kernel = await boot(viewer);
    const boom: FlavorProvider = {
      mime: "image/png",
      appliesTo: () => true,
      provide: async () => {
        throw new Error("provider failed");
      },
    };
    await kernel.exec("clipboard.registerFlavor", boom);
    kernel.bus.emit("selection:changed", textSel(1));

    const out = (await kernel.exec("clipboard.copy")) as CopyOutcome;
    // The built-in text flavors still land; the throwing png is omitted.
    expect(out.mimes).toEqual(["text/plain", "text/html"]);
    expect(out.flavors["image/png"]).toBeUndefined();
  });

  it("all-null flavors → copy() null with NO auto-emit; the adapter reports the error", async () => {
    const { viewer } = fakeViewer({ reject: true });
    const kernel = await boot(viewer);
    const errors: Array<{ reason: string }> = [];
    kernel.bus.subscribe("clipboard:error", (e) => errors.push(e));

    kernel.bus.emit("selection:changed", textSel(1));
    const out = await kernel.exec("clipboard.copy");
    expect(out).toBeNull();
    // Serialize does not emit; the DOM adapter reports error after it
    // sees the null outcome (or a rejected OS write).
    expect(errors).toEqual([]);

    await kernel.exec("clipboard.reportError", { reason: "no-flavors" });
    expect(errors).toEqual([{ reason: "no-flavors" }]);
  });

  it("registerFlavor drop-in: a dummy image/png appears in plan().mimes + copy().flavors", async () => {
    const { viewer } = fakeViewer();
    const kernel = await boot(viewer);
    const png: FlavorProvider = {
      mime: "image/png",
      appliesTo: (k) => k === "wml-text",
      provide: async () => "PNGDATA",
    };
    await kernel.exec("clipboard.registerFlavor", png);
    kernel.bus.emit("selection:changed", textSel(1));

    const plan = (await kernel.exec("clipboard.plan")) as CopyPlan;
    expect(plan.mimes.sort()).toEqual(["image/png", "text/html", "text/plain"]);
    const out = (await kernel.exec("clipboard.copy")) as CopyOutcome;
    expect(out.flavors["image/png"]).toBe("PNGDATA");
  });

  it("plan() is synchronous: building it triggers NO extractRange until a flavor is used", async () => {
    const { viewer, calls } = fakeViewer();
    const kernel = await boot(viewer);
    kernel.bus.emit("selection:changed", textSel(7));

    const plan = (await kernel.exec("clipboard.plan")) as CopyPlan;
    expect(plan.seq).toBe(7);
    expect(calls()).toBe(0); // building the plan did NO extraction

    await plan.flavor("text/plain");
    expect(calls()).toBe(1); // extraction happened only when the flavor was used
  });

  it("clipboard.serialize returns {seq, plain, html} — the sync cache carries both text flavors", async () => {
    const { viewer } = fakeViewer();
    const kernel = await boot(viewer);
    kernel.bus.emit("selection:changed", textSel(9));

    const cached = (await kernel.exec("clipboard.serialize")) as {
      seq: number;
      plain: string;
      html: string;
    } | null;
    expect(cached?.seq).toBe(9);
    expect(cached?.plain).toBe("Hello");
    expect(cached?.html).toMatch(/^<meta charset="utf-8">/);
    expect(cached?.html).toContain("Hello");
  });

  it("clipboard.extractSyncInProcess also carries html (tier-3 setData path)", async () => {
    const { viewer } = fakeViewer();
    const kernel = await boot(viewer);
    kernel.bus.emit("selection:changed", textSel(1));

    const cached = (await kernel.exec("clipboard.extractSyncInProcess")) as {
      seq: number;
      plain: string;
      html: string;
    } | null;
    expect(cached?.plain).toBe("Hello");
    expect(cached?.html).toContain(`<meta charset="utf-8">`);
  });

  it("plan().seq tracks the latest selection seq; a newer selection bumps it", async () => {
    const { viewer } = fakeViewer();
    const kernel = await boot(viewer);
    kernel.bus.emit("selection:changed", textSel(3));
    expect(((await kernel.exec("clipboard.plan")) as CopyPlan).seq).toBe(3);
    kernel.bus.emit("selection:changed", textSel(4));
    expect(((await kernel.exec("clipboard.plan")) as CopyPlan).seq).toBe(4);
    expect(await kernel.exec("clipboard.currentSeq")).toBe(4);
  });

  // ── image object copy ─────────────────────────────────────────────────────
  function fakeObjectViewer(opts: { bitmap?: boolean; original?: boolean } = {}): Viewer {
    return {
      extractRange: async (): Promise<DocFragment | null> => ({ blocks: [] }),
      extractRangeSyncInProcess: (): DocFragment | null => null,
      objectInfo: async () => ({ relId: "rIdImg", mime: "image/png", wPx: 20, hPx: 16 }),
      imageOriginal: async () =>
        opts.original === false ? null : { mime: "image/png", bytes: new Uint8Array([0, 1, 2]) },
      imageBitmapFor: async () =>
        opts.bitmap === false ? null : ({ width: 20, height: 16, close() {} } as unknown as ImageBitmap),
    } as unknown as Viewer;
  }

  async function bootObj(viewer: Viewer) {
    const kernel = createKernel();
    kernel.use(selectionStub);
    kernel.use(clipboardPlugin(viewer, { encodePng: async () => new Blob(["png"], { type: "image/png" }) }));
    await kernel.boot();
    return kernel;
  }

  const objSel = (seq: number) => ({
    kind: "object" as const,
    collapsed: false,
    object: { page: 0, kind: "inlineImage" as const, key: "rIdImg", index: 0 },
    seq,
  });

  it("object selection → plan includes image/png + text/html; copy yields a png Blob + html string", async () => {
    const kernel = await bootObj(fakeObjectViewer());
    kernel.bus.emit("selection:changed", objSel(1));

    const plan = (await kernel.exec("clipboard.plan")) as CopyPlan;
    expect(plan.kind).toBe("wml-object-image");
    expect(plan.mimes.sort()).toEqual(["image/png", "text/html"]);

    const out = (await kernel.exec("clipboard.copy")) as CopyOutcome;
    expect(out.flavors["image/png"]).toBeInstanceOf(Blob);
    expect(typeof out.flavors["text/html"]).toBe("string");
    expect(out.flavors["text/html"]).toContain("<img src=");
  });

  it("worker mode (no bitmap) → image/png OMITTED, text/html present as a placeholder", async () => {
    const kernel = await bootObj(fakeObjectViewer({ bitmap: false, original: false }));
    kernel.bus.emit("selection:changed", objSel(1));

    const out = (await kernel.exec("clipboard.copy")) as CopyOutcome;
    expect(out.flavors["image/png"]).toBeUndefined(); // clean skip, not a failure
    expect(out.flavors["text/html"]).toContain(`<img alt="image"`);
  });

  it("an object selection has no sync text flavor: serialize / extractSyncInProcess return null", async () => {
    const kernel = await bootObj(fakeObjectViewer());
    kernel.bus.emit("selection:changed", objSel(1));
    expect(await kernel.exec("clipboard.serialize")).toBeNull();
    expect(await kernel.exec("clipboard.extractSyncInProcess")).toBeNull();
  });

  it("reportCopied after an object copy carries kind=wml-object-image", async () => {
    const kernel = await bootObj(fakeObjectViewer());
    const copied: Array<{ kind: string; mimes: string[] }> = [];
    kernel.bus.subscribe("clipboard:copied", (e) => copied.push(e));
    kernel.bus.emit("selection:changed", objSel(1));
    await kernel.exec("clipboard.reportCopied", { seq: 1, charCount: 0 });
    expect(copied[0]?.kind).toBe("wml-object-image");
    expect(copied[0]?.mimes.sort()).toEqual(["image/png", "text/html"]);
  });

  // ── CHART object copy routing ────────────────────────────────────────────
  function fakeChartViewer(opts: { data?: boolean } = {}): Viewer {
    return {
      extractRange: async (): Promise<DocFragment | null> => ({ blocks: [] }),
      extractRangeSyncInProcess: (): DocFragment | null => null,
      // Charts stay OUT of the image-bytes paths (by design): all three null.
      objectInfo: async () => null,
      imageOriginal: async () => null,
      imageBitmapFor: async () => null,
      chartData: async () =>
        opts.data === false
          ? null
          : {
              categories: ["Q1", "Q2"],
              series: [{ name: "Alpha", values: [1, 2] }],
              wPx: 430,
              hPx: 216,
            },
      renderChartToCanvas: async () => false, // worker-shaped: raster clean-skips
    } as unknown as Viewer;
  }

  const chartSel = (seq: number) => ({
    kind: "object" as const,
    collapsed: false,
    object: { page: 0, kind: "chart" as const, key: "chart::rId3", index: 0 },
    seq,
  });

  it("a chart object routes to wml-object-chart with png+html+TSV mimes", async () => {
    const kernel = await bootObj(fakeChartViewer());
    kernel.bus.emit("selection:changed", chartSel(1));
    const plan = (await kernel.exec("clipboard.plan")) as CopyPlan;
    expect(plan.kind).toBe("wml-object-chart");
    expect(plan.mimes.sort()).toEqual(["image/png", "text/html", "text/plain"]);
  });

  it("an SML grid chart (chart::sml:: key) routes to sml-object-chart with the SAME flavors", async () => {
    // xlsx embedded charts: the key namespace is the ONLY routing input — the
    // providers/flavors are the shared chart ones, unforked.
    const kernel = await bootObj(fakeChartViewer());
    kernel.bus.emit("selection:changed", {
      kind: "object" as const,
      collapsed: false,
      object: { page: 0, kind: "chart" as const, key: "chart::sml::0::rId1", index: 0 },
      seq: 1,
    });
    const plan = (await kernel.exec("clipboard.plan")) as CopyPlan;
    expect(plan.kind).toBe("sml-object-chart");
    expect(plan.mimes.sort()).toEqual(["image/png", "text/html", "text/plain"]);
    const out = (await kernel.exec("clipboard.copy")) as CopyOutcome;
    expect(out.flavors["text/plain"]).toBe("\tAlpha\r\nQ1\t1\r\nQ2\t2");
  });

  it("chart copy with a dead raster seam still yields the TSV + html placeholder", async () => {
    const kernel = await bootObj(fakeChartViewer());
    kernel.bus.emit("selection:changed", chartSel(1));
    const out = (await kernel.exec("clipboard.copy")) as CopyOutcome;
    expect(out.flavors["image/png"]).toBeUndefined(); // clean skip, not a failure
    expect(out.flavors["text/html"]).toContain(`<img alt="chart"`);
    expect(out.flavors["text/plain"]).toBe("\tAlpha\r\nQ1\t1\r\nQ2\t2");
    expect(out.charCount).toBe((out.flavors["text/plain"] as string).length);
  });

  it("an unresolvable chart model omits every chart flavor (copy → null)", async () => {
    const kernel = await bootObj(fakeChartViewer({ data: false }));
    kernel.bus.emit("selection:changed", chartSel(1));
    expect(await kernel.exec("clipboard.copy")).toBeNull();
  });

  it("a chart selection has no sync text flavor (the TSV rides the async tier)", async () => {
    const kernel = await bootObj(fakeChartViewer());
    kernel.bus.emit("selection:changed", chartSel(1));
    expect(await kernel.exec("clipboard.serialize")).toBeNull();
    expect(await kernel.exec("clipboard.extractSyncInProcess")).toBeNull();
  });
});
