/**
 * plugin-grid tests.
 *
 * plugin-grid owns the continuous spreadsheet GRID: the windowed virtual-scroll
 * state (extent), the grid-vs-pages MODE (excel-only), and the grid cell hit-test
 * DECISION (grid.cellAt). It is a `@xstate/store` slice (NOT a machine),
 * deps:["sheets","viewport"], caches documentKind off loader:loaded for the
 * excel-only clamp, and reacts to 'sheets:changed' (re-fetch extent + emit
 * 'grid:resetup' while in grid mode).
 *
 * We boot it alongside the REAL loader/viewport/sheets plugins against the same
 * FakeWasm/Viewer injection the sheets test uses. The fake adds the SML grid
 * surface plugin-grid reads: gridExtent + gridCellAt.
 */
import { describe, it, expect } from "vitest";
import { Viewer } from "@parasma/core";
import { createKernel } from "..";
import { loaderPlugin } from "./loader";
import { viewportPlugin } from "./viewport";
import { sheetsPlugin, type SheetInfo } from "./sheets";
import { gridPlugin, type GridMode } from "./grid";

class FakeWasm {
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
  // Per-sheet extent: width grows with the sheet index so we can assert the
  // resetup re-fetches the right sheet's extent.
  gridExtent(sheet: number): { width: number; height: number } | null {
    if (!this.isXlsx) return null;
    return { width: 1000 + sheet * 100, height: 2000 + sheet * 100 };
  }
  gridCellAt(
    sheet: number,
    x: number,
    _y: number,
    _scrollX: number,
    _scrollY: number,
  ): { ref: string; raw: string; display: string } | null {
    if (!this.isXlsx) return null;
    // A hit anywhere x>=0 returns a deterministic cell; x<0 misses (for cursor).
    if (x < 0) return null;
    return { ref: `S${sheet}!A1`, raw: "42", display: "$42" };
  }
  /** Frozen panes (§18.3.1.66): per-sheet pane info a test can arm; default none. */
  panes: Record<number, { xSplit: number; ySplit: number; bandW: number; bandH: number; scrollX: number; scrollY: number }> = {};
  gridPaneInfo(sheet: number): unknown {
    if (!this.isXlsx) return null;
    return this.panes[sheet] ?? null;
  }
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

function xlsxBytes(): Uint8Array {
  const head = [0x50, 0x4b, 0x03, 0x04];
  const needle = [...new TextEncoder().encode("xl/workbook.xml")];
  return new Uint8Array([...head, ...needle]);
}
function docxBytes(): Uint8Array {
  return new Uint8Array([1, 2, 3, 4]);
}

const SHEETS: SheetInfo[] = [
  { name: "Sheet1", firstPage: 0, pageCount: 2, maxRow: 0 },
  { name: "Sheet2", firstPage: 2, pageCount: 1, maxRow: 0 },
];

async function bootKernel(pages: number, sheets: SheetInfo[] = []) {
  const wasm = new FakeWasm(pages, sheets);
  const viewer = makeViewer(wasm);
  const kernel = createKernel();
  kernel.use(loaderPlugin(viewer));
  kernel.use(viewportPlugin(viewer));
  kernel.use(sheetsPlugin(viewer));
  kernel.use(gridPlugin(viewer));
  await kernel.boot();
  return { kernel, viewer, wasm };
}

describe("plugin-grid", () => {
  it("grid.mode defaults to 'pages' before any document", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    expect(await kernel.exec("grid.mode")).toBe("pages");
  });

  // The PLUGIN owns the default-mode decision: an excel load flips to the
  // continuous grid with NO chrome involvement. (The flip used to be a side
  // effect of the snippet's sheet-TAB rebuild, which silently no-opped when
  // the rebuild bailed — observed as an xlsx stuck in Pages view.)
  it("an excel load flips mode to 'grid' by itself (plugin-owned default)", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    expect(await kernel.exec("grid.mode")).toBe("grid");
  });

  it("a word load does NOT flip mode to 'grid'", async () => {
    const { kernel } = await bootKernel(2, []);
    await kernel.exec("loader.open", docxBytes());
    expect(await kernel.exec("grid.mode")).toBe("pages");
  });

  it("grid.setMode(true) enters 'grid' for an excel document", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    await kernel.exec("grid.setMode", true);
    expect(await kernel.exec("grid.mode")).toBe("grid");
  });

  it("grid.setMode(false) returns to 'pages'", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    await kernel.exec("grid.setMode", true);
    await kernel.exec("grid.setMode", false);
    expect(await kernel.exec("grid.mode")).toBe("pages");
  });

  it("grid.toggle flips grid ⇄ pages", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    await kernel.exec("grid.setMode", true);
    expect(await kernel.exec("grid.mode")).toBe("grid");
    await kernel.exec("grid.toggle");
    expect(await kernel.exec("grid.mode")).toBe("pages");
    await kernel.exec("grid.toggle");
    expect(await kernel.exec("grid.mode")).toBe("grid");
  });

  it("grid.setMode(true) is IGNORED / clamped for a non-excel (word) document", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", docxBytes());
    await kernel.exec("grid.setMode", true);
    expect(await kernel.exec("grid.mode")).toBe("pages");
  });

  it("emits 'grid:mode-changed' on the mode edge", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    const modes: GridMode[] = [];
    kernel.bus.subscribe("grid:mode-changed", ({ mode }) => modes.push(mode));
    await kernel.exec("grid.setMode", true);
    await kernel.exec("grid.setMode", false);
    expect(modes.at(-2)).toBe("grid");
    expect(modes.at(-1)).toBe("pages");
  });

  it("grid.extent is a thin pass-through to viewer.gridExtent", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    expect(await kernel.exec("grid.extent", 0)).toEqual({ width: 1000, height: 2000 });
    expect(await kernel.exec("grid.extent", 1)).toEqual({ width: 1100, height: 2100 });
  });

  it("grid.cellAt is a thin pass-through to viewer.gridCellAt (raw coords)", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    const cell = await kernel.exec("grid.cellAt", { sheet: 1, x: 10, y: 20, scrollX: 0, scrollY: 0 });
    expect(cell).toEqual({ ref: "S1!A1", raw: "42", display: "$42" });
    // A miss (x<0) returns null — the cursor gap relies on this null/non-null.
    const miss = await kernel.exec("grid.cellAt", { sheet: 0, x: -1, y: 5, scrollX: 0, scrollY: 0 });
    expect(miss).toBeNull();
  });

  it("emits 'grid:resetup' for the active sheet when a sheet changes in grid mode", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    await kernel.exec("grid.setMode", true);
    const resetups: number[] = [];
    kernel.bus.subscribe("grid:resetup", ({ sheet }) => resetups.push(sheet));
    // Subscribing replays the latest (from entering grid mode) → sheet 0.
    expect(resetups.at(-1)).toBe(0);
    await kernel.exec("sheets.select", 1);
    expect(resetups.at(-1)).toBe(1);
  });

  it("does NOT emit 'grid:resetup' on a sheet change while in pages mode", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    // An excel load now auto-flips to 'grid' (plugin-owned default) — opt back
    // into 'pages' explicitly. Switching sheets must then not drive a resetup.
    await kernel.exec("grid.setMode", false);
    const resetups: number[] = [];
    kernel.bus.subscribe("grid:resetup", ({ sheet }) => resetups.push(sheet));
    await kernel.exec("sheets.select", 1);
    expect(resetups).toEqual([]);
  });

  it("resets mode/extent on a doc-switch loader status (loading)", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    await kernel.exec("grid.setMode", true);
    expect(await kernel.exec("grid.mode")).toBe("grid");

    const modes: GridMode[] = [];
    kernel.bus.subscribe("grid:mode-changed", ({ mode }) => modes.push(mode));
    // A new load drives loader → loading (status edge) → plugin-grid resets.
    const p = kernel.exec("loader.open", docxBytes());
    expect(await kernel.exec("grid.mode")).toBe("pages");
    expect(modes.some((m) => m === "pages")).toBe(true);
    await p;
  });

  // ── per-sheet scroll memory (sheet-switch scroll restore) ──────────────────

  it("grid.scrollFor defaults to the origin for an unvisited sheet", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    expect(await kernel.exec("grid.scrollFor", 0)).toEqual({ x: 0, y: 0 });
  });

  it("grid.saveScroll remembers per-sheet positions independently", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    await kernel.exec("grid.saveScroll", { sheet: 0, x: 120, y: 4500 });
    await kernel.exec("grid.saveScroll", { sheet: 1, x: 0, y: 99 });
    await kernel.exec("grid.saveScroll", { sheet: 0, x: 130, y: 4600 }); // latest wins
    expect(await kernel.exec("grid.scrollFor", 0)).toEqual({ x: 130, y: 4600 });
    expect(await kernel.exec("grid.scrollFor", 1)).toEqual({ x: 0, y: 99 });
  });

  // ── Frozen panes (§18.3.1.66): pane info + topLeftCell scroll restore ──────

  it("grid.paneInfo is a thin pass-through (null for pane-less sheets)", async () => {
    const { kernel, wasm } = await bootKernel(2, SHEETS);
    wasm.panes[1] = { xSplit: 1, ySplit: 2, bandW: 64, bandH: 38, scrollX: 0, scrollY: 150 };
    await kernel.exec("loader.open", xlsxBytes());
    expect(await kernel.exec("grid.paneInfo", 0)).toBeNull();
    expect(await kernel.exec("grid.paneInfo", 1)).toEqual(
      { xSplit: 1, ySplit: 2, bandW: 64, bandH: 38, scrollX: 0, scrollY: 150 },
    );
  });

  it("grid.scrollFor restores the pane topLeftCell scroll on an UNVISITED sheet; a remembered position wins", async () => {
    const { kernel, wasm } = await bootKernel(2, SHEETS);
    wasm.panes[0] = { xSplit: 0, ySplit: 1, bandW: 0, bandH: 19, scrollX: 0, scrollY: 931 };
    await kernel.exec("loader.open", xlsxBytes());
    // Unvisited + pane with topLeftCell → the file-saved body scroll.
    expect(await kernel.exec("grid.scrollFor", 0)).toEqual({ x: 0, y: 931 });
    // Unvisited + pane-less sheet → the origin (old behavior, byte-neutral).
    expect(await kernel.exec("grid.scrollFor", 1)).toEqual({ x: 0, y: 0 });
    // Once the user scrolls, the remembered position wins over topLeftCell.
    await kernel.exec("grid.saveScroll", { sheet: 0, x: 12, y: 34 });
    expect(await kernel.exec("grid.scrollFor", 0)).toEqual({ x: 12, y: 34 });
  });

  it("scroll memory is CLEARED on a doc switch (loader reset)", async () => {
    const { kernel } = await bootKernel(2, SHEETS);
    await kernel.exec("loader.open", xlsxBytes());
    await kernel.exec("grid.saveScroll", { sheet: 1, x: 50, y: 777 });
    expect(await kernel.exec("grid.scrollFor", 1)).toEqual({ x: 50, y: 777 });
    // A new load drives loader → loading → plugin-grid resets the slice.
    await kernel.exec("loader.open", xlsxBytes());
    expect(await kernel.exec("grid.scrollFor", 1)).toEqual({ x: 0, y: 0 });
  });

  it("depends on sheets + viewport (boots after both)", async () => {
    const wasm = new FakeWasm(1);
    const viewer = makeViewer(wasm);
    const kernel = createKernel();
    kernel.use(loaderPlugin(viewer));
    kernel.use(viewportPlugin(viewer));
    kernel.use(gridPlugin(viewer)); // missing sheets
    await expect(kernel.boot()).rejects.toThrow(/unknown plugin: sheets/);
  });
});
