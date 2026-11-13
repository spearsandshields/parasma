/**
 * plugin-selection — SML CELLS arm. The ONE command family
 * (`selection.begin/drag/extendTo/end/clear`) drives the grid
 * through the unified `viewer.hitTest` (a scrollful {@link DocLoc} addresses
 * the grid; the CELL hit kind — not a command name — selects the cells
 * semantics) + `viewer.gridContentBounds` (header extent) +
 * `viewer.setSelection({kind:"cells"})` (fire-and-forget), emits
 * `selection:changed` {kind:"cells"}, and is mode-gated. Booted over a
 * FakeWasm the same way the text-arm tests boot, so the REAL Viewer wrappers
 * are exercised.
 */
import { describe, it, expect } from "vitest";
import { Viewer } from "@parasma/core";
import { createKernel } from "..";
import { loaderPlugin } from "./loader";
import { viewportPlugin } from "./viewport";
import { selectionPlugin, type SelectionState } from "./selection";

type CellsChanged = {
  kind: "none" | "text" | "object" | "cells";
  collapsed: boolean;
  sheet?: number;
  range?: { r0: number; c0: number; r1: number; c1: number };
  seq: number;
};

/** A recorded cells DocSelection (`null` = the unified clear). */
type RecordedSel = {
  kind: string;
  sheet: number;
  range: { r0: number; c0: number; r1: number; c1: number };
  anchor: { row: number; col: number };
} | null;

/** Shorthand for the expected recorded cells selection. */
const cells = (sheet: number, r0: number, c0: number, r1: number, c1: number, ar: number, ac: number): RecordedSel => ({
  kind: "cells",
  sheet,
  range: { r0, c0, r1, c1 },
  anchor: { row: ar, col: ac },
});

/**
 * FakeWasm: the loader/viewport boot surface + the unified selection exports.
 * `docHitTest` encodes the zone/row/col in the query: negative x = a row header
 * (row = y), negative y = a column header (col = x), both negative = the corner;
 * otherwise a cell at (row=y, col=x). A `(row,col)` in `merges` reports its anchor.
 */
class FakeWasm {
  readonly setCellCalls: RecordedSel[] = [];
  merges: Array<{ r: number; c: number; ar: number; ac: number }> = [];
  bounds = { r0: 1, c0: 1, r1: 20, c1: 8 };

  loadDocx(_b: Uint8Array): void {}
  loadXlsx(_b: Uint8Array): void {}
  loadPptx(_b: Uint8Array): void {}
  abortLayout(): void {}
  async runLayout(cb: (idx: number) => void): Promise<number> {
    cb(0);
    return 1;
  }
  setNow(_n: number): void {}
  pageSize(): { widthPx: number; heightPx: number } {
    return { widthPx: 612, heightPx: 792 };
  }
  fontFamiliesUsed(): string[] {
    return [];
  }
  addFontAlias(_d: string, _s: string): void {}
  loadFont(_f: string, _b: Uint8Array, _w: number, _i: boolean): void {}
  imageRefs(): unknown[] {
    return [];
  }
  clearImageBytes(): void {}
  sheetInfo(): unknown[] {
    return [];
  }
  /** Spy: records the outline key each docSetSelectedObject wire carries (null = clear). */
  selectedObjectCalls: Array<string | null> = [];
  docSetSelectedObject(obj: { surface: number; key: string } | null): void {
    this.selectedObjectCalls.push(obj ? obj.key : null);
  }

  // ── unified selection exports (the grid arms) ────────────────────────────
  docHitTest(loc: { surface: number }, x: number, y: number, _g: string): unknown {
    const surface = loc.surface;
    if (x < 0 && y < 0) return { kind: "cell", surface, zone: "corner", row: 1, col: 1 };
    if (x < 0) return { kind: "cell", surface, zone: "rowHeader", row: y, col: 1 };
    if (y < 0) return { kind: "cell", surface, zone: "colHeader", row: 1, col: x };
    const m = this.merges.find((mm) => mm.r === y && mm.c === x);
    const hit: Record<string, unknown> = { kind: "cell", surface, zone: "cell", row: y, col: x };
    if (m) hit.mergeAnchor = { row: m.ar, col: m.ac };
    return hit;
  }
  /** xlsx-charts slice: a chart occupies view x ∈ [500, 600] (any y ≥ 0). */
  docHitTestObject(loc: { surface: number }, x: number, y: number): unknown {
    if (y >= 0 && x >= 500 && x <= 600) {
      return { surface: loc.surface, kind: "chart", key: "chart::sml::0::rId9", rect: { x: 500, y: 100, w: 100, h: 50 } };
    }
    return null;
  }
  gridContentBounds(_sheet: number): unknown {
    return { ...this.bounds };
  }
  docSetSelection(sel: RecordedSel): void {
    this.setCellCalls.push(sel);
  }
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

async function boot() {
  const wasm = new FakeWasm();
  const viewer = makeViewer(wasm);
  const kernel = createKernel();
  kernel.use(loaderPlugin(viewer));
  kernel.use(viewportPlugin(viewer));
  kernel.use(selectionPlugin(viewer));
  await kernel.boot();
  wasm.setCellCalls.length = 0; // drop the boot-time clear
  return { kernel, viewer, wasm };
}

function captureChanges(kernel: Awaited<ReturnType<typeof boot>>["kernel"]) {
  const events: CellsChanged[] = [];
  kernel.bus.subscribe("selection:changed", (e) => events.push(e as CellsChanged));
  events.length = 0;
  return events;
}

/** A grid pointer query: a scrollful loc addresses the grid content space. */
const q = (x: number, y: number) => ({ loc: { surface: 0, scrollX: 0, scrollY: 0 }, x, y });

describe("plugin-selection — SML cells arm", () => {
  it("begin on a cell anchors a collapsed range + pushes the cells selection", async () => {
    const { kernel, wasm } = await boot();
    const events = captureChanges(kernel);
    // Click cell (row=3, col=2).
    await kernel.exec("selection.begin", q(2, 3));
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind).toBe("cells");
    if (sel.kind === "cells") {
      expect(sel.anchor).toEqual({ row: 3, col: 2 });
      expect(sel.focus).toEqual({ row: 3, col: 2 });
      expect(sel.active).toEqual({ row: 3, col: 2 });
    }
    // Engine push: collapsed rect, active = the cell.
    expect(wasm.setCellCalls.at(-1)).toEqual(cells(0, 3, 2, 3, 2, 3, 2));
    // Event: kind cells, collapsed, the raw normalized rect.
    const last = events.at(-1)!;
    expect(last.kind).toBe("cells");
    expect(last.collapsed).toBe(true);
    expect(last.range).toEqual({ r0: 3, c0: 2, r1: 3, c1: 2 });
  });

  it("drag extends the focus into a rectangular range", async () => {
    const { kernel, wasm } = await boot();
    await kernel.exec("selection.begin", q(2, 2)); // anchor (2,2)
    await kernel.exec("selection.drag", q(5, 4)); // focus (4,5)
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    if (sel.kind === "cells") {
      expect(sel.anchor).toEqual({ row: 2, col: 2 });
      expect(sel.focus).toEqual({ row: 4, col: 5 });
    }
    // Normalized rect pushed to the engine (anchor stays the active cell).
    expect(wasm.setCellCalls.at(-1)).toEqual(cells(0, 2, 2, 4, 5, 2, 2));
  });

  it("shift-extend keeps the anchor and moves the focus", async () => {
    const { kernel, wasm } = await boot();
    await kernel.exec("selection.begin", q(3, 3)); // anchor (3,3)
    await kernel.exec("selection.end");
    await kernel.exec("selection.extendTo", q(6, 7)); // shift+click (7,6)
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    if (sel.kind === "cells") {
      expect(sel.anchor).toEqual({ row: 3, col: 3 });
      expect(sel.focus).toEqual({ row: 7, col: 6 });
    }
    expect(wasm.setCellCalls.at(-1)).toEqual(cells(0, 3, 3, 7, 6, 3, 3));
  });

  it("column-header click selects the whole used-extent column", async () => {
    const { kernel, wasm } = await boot();
    // colHeader hit: y<0 → colHeader at col = x (=5).
    await kernel.exec("selection.begin", q(5, -1));
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    if (sel.kind === "cells") {
      // Whole column 5 over the content-bounds rows 1..20.
      expect(sel.anchor).toEqual({ row: 1, col: 5 });
      expect(sel.focus).toEqual({ row: 20, col: 5 });
    }
    expect(wasm.setCellCalls.at(-1)).toEqual(cells(0, 1, 5, 20, 5, 1, 5));
  });

  it("shift+click a second column header extends to a multi-column band", async () => {
    const { kernel, wasm } = await boot();
    await kernel.exec("selection.begin", q(3, -1)); // click col 3 header
    await kernel.exec("selection.extendTo", q(6, -1)); // shift+click col 6 header
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    if (sel.kind === "cells") {
      expect(sel.anchor).toEqual({ row: 1, col: 3 }); // anchor col held, full height
      expect(sel.focus).toEqual({ row: 20, col: 6 }); // extends to col 6, full extent
    }
    // cols 3..6 × the used-extent rows 1..20 (NOT collapsed to the header row).
    expect(wasm.setCellCalls.at(-1)).toEqual(cells(0, 1, 3, 20, 6, 1, 3));
  });

  it("shift+click a second row header extends to a multi-row band", async () => {
    const { kernel, wasm } = await boot();
    await kernel.exec("selection.begin", q(-1, 2)); // click row 2 header
    await kernel.exec("selection.extendTo", q(-1, 5)); // shift+click row 5 header
    // rows 2..5 × the used-extent cols 1..8.
    expect(wasm.setCellCalls.at(-1)).toEqual(cells(0, 2, 1, 5, 8, 2, 1));
  });

  it("row-header click selects the whole used-extent row", async () => {
    const { kernel, wasm } = await boot();
    // rowHeader hit: x<0 → rowHeader at row = y (=4).
    await kernel.exec("selection.begin", q(-1, 4));
    expect(wasm.setCellCalls.at(-1)).toEqual(cells(0, 4, 1, 4, 8, 4, 1));
  });

  it("corner click is a no-op", async () => {
    const { kernel, wasm } = await boot();
    await kernel.exec("selection.begin", q(-1, -1));
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind).toBe("none");
    expect(wasm.setCellCalls.length).toBe(0);
  });

  it("clicking a merged cell makes the merge anchor the active cell", async () => {
    const { kernel, wasm } = await boot();
    wasm.merges = [{ r: 1, c: 3, ar: 1, ac: 1 }]; // C1 is covered by A1:C1 → anchor A1
    await kernel.exec("selection.begin", q(3, 1)); // click C1
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    if (sel.kind === "cells") {
      expect(sel.anchor).toEqual({ row: 1, col: 3 }); // raw clicked cell
      expect(sel.active).toEqual({ row: 1, col: 1 }); // active snaps to the merge anchor
    }
    // Active cell (anchor of the merge) is pushed as the active coords.
    expect(wasm.setCellCalls.at(-1)).toEqual(cells(0, 1, 3, 1, 3, 1, 1));
  });

  it("selection.clear drops the selection and clears the engine", async () => {
    const { kernel, wasm } = await boot();
    await kernel.exec("selection.begin", q(2, 2));
    await kernel.exec("selection.clear");
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind).toBe("none");
    // The unified clear (null — no sheet sentinel) reached the engine.
    expect(wasm.setCellCalls.at(-1)).toBeNull();
  });

  it("is inert in hand mode", async () => {
    const { kernel, wasm } = await boot();
    kernel.bus.emit("tool:mode-changed", { mode: "hand" });
    await kernel.exec("selection.begin", q(2, 2));
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind).toBe("none");
    expect(wasm.setCellCalls.length).toBe(0);
  });

  it("an explicit tool switch (base-mode-changed) clears the cells selection", async () => {
    const { kernel, wasm } = await boot();
    const events = captureChanges(kernel);
    await kernel.exec("selection.begin", q(2, 3)); // a live cells selection
    expect(((await kernel.exec("selection.get")) as SelectionState).kind).toBe("cells");
    wasm.setCellCalls.length = 0;
    // The toolbar toggle emits tool:base-mode-changed (NOT the Space-hold transient).
    kernel.bus.emit("tool:base-mode-changed", { mode: "hand" });
    expect(((await kernel.exec("selection.get")) as SelectionState).kind).toBe("none");
    expect(wasm.setCellCalls.at(-1)).toBeNull(); // engine cell paint cleared (unified null)
    expect(events.at(-1)?.kind).toBe("none"); // empty summary emitted
  });

  it("a transient Space-hold (mode-changed only) PRESERVES the selection", async () => {
    const { kernel } = await boot();
    await kernel.exec("selection.begin", q(2, 3));
    // A transient hand emits tool:mode-changed but NOT base-mode-changed → kept.
    kernel.bus.emit("tool:mode-changed", { mode: "hand" });
    expect(((await kernel.exec("selection.get")) as SelectionState).kind).toBe("cells");
  });

  // ── xlsx charts: the pointerdown OBJECT-FIRST branch ─────────────────────
  it("begin over a chart selects the OBJECT (outline wired), not a cell", async () => {
    const { kernel, wasm } = await boot();
    await kernel.exec("selection.begin", q(550, 10)); // inside the chart zone
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind).toBe("object");
    if (sel.kind === "object") {
      expect(sel.ref).toEqual({ page: 0, kind: "chart", key: "chart::sml::0::rId9", index: 0 });
    }
    // The outline wire carried the chart key; NO cell selection was SET (the
    // exclusivity clear sends setSelection(null) — allowed).
    expect(wasm.selectedObjectCalls.at(-1)).toBe("chart::sml::0::rId9");
    expect(wasm.setCellCalls.every((c) => c === null)).toBe(true);
    // A subsequent drag is a no-op (no live cells drag).
    await kernel.exec("selection.drag", q(4, 4));
    expect(((await kernel.exec("selection.get")) as SelectionState).kind).toBe("object");
  });

  it("a cell click after a chart selection clears the object (exclusivity)", async () => {
    const { kernel, wasm } = await boot();
    await kernel.exec("selection.begin", q(550, 10)); // chart selected
    await kernel.exec("selection.begin", q(2, 3)); // plain cell click
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind).toBe("cells");
    // The engine outline was cleared (null wire) before the cell selection landed.
    expect(wasm.selectedObjectCalls).toContain(null);
    expect(wasm.setCellCalls.some((c) => c !== null)).toBe(true);
  });
});
