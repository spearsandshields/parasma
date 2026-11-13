/**
 * plugin-sheets tests.
 *
 * plugin-sheets owns the Excel SHEET MODEL (sheets[] from viewer.sheetInfo()) +
 * the active sheet index + the switching DECISION, and emits a `sheets:changed`
 * bus event ({active, sheets}). It is a `@xstate/store` slice (NOT a machine),
 * deps:["loader","viewport"], and builds the strip on the bridged loadComplete
 * (the post-layout seam) from viewer.sheetInfo() — which is [] for non-spreadsheet
 * docs, so no documentKind gate is needed.
 *
 * We boot it alongside the REAL loaderPlugin + viewportPlugin against the same
 * FakeWasm/Viewer injection trick the loader/viewport tests use. To exercise the
 * excel path, load()'s sniff must classify the bytes as XLSX — the bytes carry
 * the ZIP magic + the "xl/workbook.xml" part-name needle so core routes to
 * #doLoadXlsx (which calls FakeWasm.loadXlsx → sheetInfo() returns the sheets);
 * a plain (non-xlsx) blob routes to the docx path (loadDocx → sheetInfo() []).
 */
import { describe, it, expect } from "vitest";
import { Viewer } from "@parasma/core";
import { createKernel } from "..";
import { loaderPlugin } from "./loader";
import { viewportPlugin } from "./viewport";
import { sheetsPlugin, type SheetInfo } from "./sheets";

type SheetsChanged = { active: number; sheets: SheetInfo[] };

/**
 * FakeWasm mirroring the viewport-test fake, plus the SML surface plugin-sheets
 * reads: loadXlsx + sheetInfo. runLayout streams a configurable page burst.
 */
class FakeWasm {
  // Mirrors core's contract: sheetInfo() is [] unless a spreadsheet is loaded.
  private isXlsx = false;
  constructor(
    private readonly pages: number,
    private readonly sheets: SheetInfo[] = [],
  ) {}
  loadDocx(_bytes: Uint8Array): void {
    this.isXlsx = false;
  }
  loadXlsx(_bytes: Uint8Array): void {
    this.isXlsx = true;
  }
  loadPptx(_bytes: Uint8Array): void {
    this.isXlsx = false;
  }
  abortLayout(): void {}
  async runLayout(cb: (idx: number) => void): Promise<number> {
    for (let i = 0; i < this.pages; i++) cb(i);
    return this.pages;
  }
  setNow(_n: number): void {}
  pageSize(): { widthPx: number; heightPx: number } {
    return { widthPx: 612, heightPx: 792 };
  }
  fontFamiliesUsed(): string[] {
    return [];
  }
  addFontAlias(_doc: string, _sub: string): void {}
  loadFont(_f: string, _b: Uint8Array, _w: number, _i: boolean): void {}
  imageRefs(): unknown[] {
    return [];
  }
  clearImageBytes(): void {}
  sheetInfo(): SheetInfo[] {
    return this.isXlsx ? this.sheets : [];
  }
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

/** Bytes that core's sniff classifies as XLSX: ZIP magic + "xl/workbook.xml". */
function xlsxBytes(): Uint8Array {
  const head = [0x50, 0x4b, 0x03, 0x04]; // "PK\x03\x04"
  const needle = [...new TextEncoder().encode("xl/workbook.xml")];
  return new Uint8Array([...head, ...needle]);
}

/** A plain (non-xlsx) blob → core routes to the docx path (documentKind 'word'). */
function docxBytes(): Uint8Array {
  return new Uint8Array([1, 2, 3, 4]);
}

async function bootKernel(pages: number, sheets: SheetInfo[] = []) {
  const wasm = new FakeWasm(pages, sheets);
  const viewer = makeViewer(wasm);
  const kernel = createKernel();
  kernel.use(loaderPlugin(viewer));
  kernel.use(viewportPlugin(viewer));
  kernel.use(sheetsPlugin(viewer));
  await kernel.boot();
  return { kernel, viewer };
}

const SHEETS: SheetInfo[] = [
  { name: "Sheet1", firstPage: 0, pageCount: 2, maxRow: 0 },
  { name: "Sheet2", firstPage: 2, pageCount: 1, maxRow: 0 },
  { name: "Sheet3", firstPage: 3, pageCount: 3, maxRow: 0 },
];

describe("plugin-sheets", () => {
  it("builds the sheet model from viewer.sheetInfo() on loadComplete (excel)", async () => {
    const { kernel } = await bootKernel(6, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());

    const list = (await kernel.exec("sheets.list")) as SheetInfo[];
    expect(list).toEqual(SHEETS);
  });

  it("sheets.active defaults to 0", async () => {
    const { kernel } = await bootKernel(6, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    expect(await kernel.exec("sheets.active")).toBe(0);
  });

  it("sheets.select updates active and emits 'sheets:changed' with {active,sheets}", async () => {
    const { kernel } = await bootKernel(6, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());

    const events: SheetsChanged[] = [];
    kernel.bus.subscribe("sheets:changed", (e) => events.push(e));
    // Subscribing replays the latest (the build event) first.
    expect(events.at(-1)?.active).toBe(0);

    await kernel.exec("sheets.select", 2);
    expect(await kernel.exec("sheets.active")).toBe(2);
    const last = events.at(-1)!;
    expect(last.active).toBe(2);
    expect(last.sheets).toEqual(SHEETS);
  });

  it("sheets.select clamps out-of-range indices", async () => {
    const { kernel } = await bootKernel(6, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());

    await kernel.exec("sheets.select", 99);
    expect(await kernel.exec("sheets.active")).toBe(SHEETS.length - 1);

    await kernel.exec("sheets.select", -5);
    expect(await kernel.exec("sheets.active")).toBe(0);
  });

  it("no strip for a word document (sheetInfo gated on excel kind)", async () => {
    const { kernel } = await bootKernel(3, SHEETS);
    await kernel.exec("loader.open", docxBytes());

    const list = (await kernel.exec("sheets.list")) as SheetInfo[];
    expect(list).toEqual([]);
  });

  it("resets the slice on a doc-switch loader status (loading)", async () => {
    const { kernel, viewer } = await bootKernel(6, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    await kernel.exec("sheets.select", 1);
    expect((await kernel.exec("sheets.list") as SheetInfo[]).length).toBe(3);

    const events: SheetsChanged[] = [];
    kernel.bus.subscribe("sheets:changed", (e) => events.push(e));

    // A new load drives loader → loading (status-changed) → plugin-sheets resets.
    // We don't await here so we observe the reset emitted on the status edge.
    const p = kernel.exec("loader.open", docxBytes());
    expect((await kernel.exec("sheets.list") as SheetInfo[]).length).toBe(0);
    expect((await kernel.exec("sheets.active"))).toBe(0);
    // The reset emitted an empty set.
    expect(events.some((e) => e.sheets.length === 0)).toBe(true);
    await p;
    void viewer;
  });

  it("depends on loader + viewport (boots after both)", async () => {
    const wasm = new FakeWasm(1);
    const viewer = makeViewer(wasm);
    const kernel = createKernel();
    kernel.use(loaderPlugin(viewer));
    kernel.use(sheetsPlugin(viewer)); // missing viewport
    await expect(kernel.boot()).rejects.toThrow(/unknown plugin: viewport/);
  });
});
