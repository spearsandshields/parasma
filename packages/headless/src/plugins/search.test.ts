/**
 * plugin-search tests.
 *
 * plugin-search owns the search ENGINE LOGIC: query state, results, active-match
 * index, navigation, cancellation, debounce. It is a `@xstate/store` slice with a
 * TS-owned chunked/cancellable loop (the GENERATION TOKEN — NOT one of the two
 * XState machines), deps:["viewport","sheets"], and emits `search:changed`
 * (+ `search:navigate` on next/prev/goto).
 *
 * We boot it alongside the REAL loader/viewport/sheets plugins against the same
 * FakeWasm/Viewer injection trick the sheets/comments tests use. The FakeWasm
 * exposes the wasm search seam (searchPage + setSearchHighlight) so the plugin reads
 * canned matches and we can assert the highlight push. searchPage is SYNCHRONOUS
 * (matching the real wasm primitive — core's Viewer.searchPage wraps it in the
 * async seam) and INSTRUMENTED to record (page, query) calls. The generation-token
 * cancellation is observable at the TS-loop level: a superseding query lands during
 * the per-page `yieldTick` gap, so the stale loop self-aborts before its next page.
 *
 * FAKE TIMERS drive the debounce + the per-tick yields; we advance them with
 * `vi.advanceTimersByTimeAsync` so the awaited setTimeout(0) yields resolve.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { Viewer, type SearchMatch } from "@parasma/core";
import { createKernel } from "..";
import { loaderPlugin } from "./loader";
import { viewportPlugin } from "./viewport";
import { sheetsPlugin, type SheetInfo } from "./sheets";
import { searchPlugin } from "./search";

type SearchChanged = {
  query: string;
  results: SearchMatch[];
  activeIdx: number | null;
  status: "idle" | "searching" | "done";
};
type SearchNavigate = {
  page: number;
  x: number;
  y: number;
  sheet: string | null;
  cellRef: string | null;
  gridX: number;
  gridY: number;
};

/**
 * A canned match on a page (one rect, y = page*100 for identity in asserts). For
 * SML matches (a `sheet` is given) we also stamp the grid-navigation fields:
 * `cellRef` = `A<page>` and `gridX`/`gridY` = page*10/page*100, so the highlight +
 * navigate plumbing of the new fields is observable. WML matches carry null/0.
 */
function match(page: number, sheet: string | null = null): SearchMatch {
  const sml = sheet !== null;
  return {
    page,
    rects: [{ x: 0, y: page * 100, w: 10, h: 10 }],
    y: page * 100,
    sheet,
    cellRef: sml ? `A${page}` : null,
    gridX: sml ? page * 10 : 0,
    gridY: sml ? page * 100 : 0,
  };
}

interface HighlightCall {
  query: string;
  activePage: number;
  activeIndex: number;
  activeCellRef: string | null;
}

/**
 * FakeWasm mirroring the sheets-test fake, plus the wasm search surface:
 * searchPage(idx, q, opts) returns canned per-page matches; setSearchHighlight
 * records the active-match push. `byPage` maps pageIdx → matches; a missing page
 * yields [].
 */
class FakeWasm {
  private isXlsx = false;
  readonly searchCalls: { page: number; query: string; scope: unknown }[] = [];
  readonly highlightCalls: HighlightCall[] = [];
  constructor(
    private readonly pages: number,
    private readonly sheets: SheetInfo[] = [],
    private readonly byPage: Record<number, SearchMatch[]> = {},
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
  /** Settable deferred-rect answer + call recorder. */
  searchRect: { page: number; x: number; y: number; w: number; h: number } | null = null;
  readonly rectCalls: { sheet: number; cellRef: string }[] = [];
  resolveSearchRect(sheet: number, cellRef: string) {
    this.rectCalls.push({ sheet, cellRef });
    return this.searchRect;
  }

  searchPage(idx: number, query: string, opts: { scope?: string }): SearchMatch[] {
    this.searchCalls.push({ page: idx, query, scope: opts?.scope });
    if (query === "") return [];
    return this.byPage[idx] ?? [];
  }
  setSearchHighlight(
    query: string,
    _opts: unknown,
    activePage: number,
    activeIndex: number,
    activeCellRef: string | null = null,
  ): void {
    this.highlightCalls.push({ query, activePage, activeIndex, activeCellRef });
  }
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

/** Bytes that core's sniff classifies as XLSX: ZIP magic + "xl/workbook.xml". */
function xlsxBytes(): Uint8Array {
  const head = [0x50, 0x4b, 0x03, 0x04];
  const needle = [...new TextEncoder().encode("xl/workbook.xml")];
  return new Uint8Array([...head, ...needle]);
}

/** A plain (non-xlsx) blob → core routes to the docx path (documentKind 'word'). */
function docxBytes(): Uint8Array {
  return new Uint8Array([1, 2, 3, 4]);
}

const DEBOUNCE = 150;

async function bootKernel(
  pages: number,
  sheets: SheetInfo[] = [],
  byPage: Record<number, SearchMatch[]> = {},
  debounceMs = DEBOUNCE,
) {
  const wasm = new FakeWasm(pages, sheets, byPage);
  const viewer = makeViewer(wasm);
  const kernel = createKernel();
  kernel.use(loaderPlugin(viewer));
  kernel.use(viewportPlugin(viewer));
  kernel.use(sheetsPlugin(viewer));
  kernel.use(searchPlugin(viewer, { debounceMs }));
  await kernel.boot();
  return { kernel, viewer, wasm };
}

const SHEETS: SheetInfo[] = [
  { name: "Sheet1", firstPage: 0, pageCount: 2, maxRow: 0 },
  { name: "Sheet2", firstPage: 2, pageCount: 1, maxRow: 0 },
  { name: "Sheet3", firstPage: 3, pageCount: 3, maxRow: 0 },
];

/** Run the debounce + the whole chunked loop to completion under fake timers. */
async function flush(): Promise<void> {
  // Comfortably past the debounce + every per-tick setTimeout(0) yield.
  await vi.advanceTimersByTimeAsync(DEBOUNCE + 50);
}

describe("plugin-search", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("debounces then streams WML matches across page indices via search:changed", async () => {
    // 3 WML pages; matches on pages 0 and 2.
    const { kernel, wasm } = await bootKernel(3, [], { 0: [match(0)], 2: [match(2)] });
    await kernel.exec("loader.open", docxBytes());

    const events: SearchChanged[] = [];
    kernel.bus.subscribe("search:changed", (e) => events.push(e));

    await kernel.exec("search.query", "foo");
    // BEFORE the debounce fires, no searchPage call has run.
    expect(wasm.searchCalls.length).toBe(0);

    await flush();

    // Iterated WML page indices [0,3): one searchPage per page.
    expect(wasm.searchCalls.map((c) => c.page)).toEqual([0, 1, 2]);
    const results = (await kernel.exec("search.results")) as SearchMatch[];
    expect(results).toEqual([match(0), match(2)]);
    // Streaming: a 'searching' event preceded the terminal 'done'.
    expect(events.some((e) => e.status === "searching")).toBe(true);
    expect(events.at(-1)?.status).toBe("done");
    // The first match seeded the active index to 0.
    expect(await kernel.exec("search.active")).toBe(0);
  });

  it("pushes the active-match highlight to the wasm after the loop", async () => {
    const { kernel, wasm } = await bootKernel(2, [], { 1: [match(1)] });
    await kernel.exec("loader.open", docxBytes());

    await kernel.exec("search.query", "foo");
    await flush();

    // Last highlight push reflects the query + active match (page 1, idx 0). WML
    // emphasises via the index, so activeCellRef is null (the SML-only identity).
    const last = wasm.highlightCalls.at(-1);
    expect(last?.query).toBe("foo");
    expect(last?.activePage).toBe(1);
    expect(last?.activeIndex).toBe(0);
    expect(last?.activeCellRef).toBeNull();
  });

  it("pushes the SML active cell's A1 ref (cellRef) as the highlight identity", async () => {
    // SML: match on Sheet1's first page carries cellRef "A0".
    const { kernel, wasm } = await bootKernel(2, SHEETS, { 0: [match(0, "Sheet1")] });
    await kernel.exec("loader.open", xlsxBytes());

    await kernel.exec("search.query", "foo");
    await flush();

    // The active SML match emphasises by A1 ref, not the per-page index.
    const last = wasm.highlightCalls.at(-1);
    expect(last?.query).toBe("foo");
    expect(last?.activePage).toBe(0);
    expect(last?.activeCellRef).toBe("A0");
  });

  it("a superseding query bumps the generation; the STALE loop's results never land", async () => {
    // 4 WML pages; the loop yields a tick BETWEEN pages, so a query landing during
    // that gap supersedes the stale loop before it runs its remaining pages.
    const { kernel, wasm } = await bootKernel(
      4,
      [],
      { 0: [match(0)], 1: [match(1)], 2: [match(2)], 3: [match(3)] },
    );
    await kernel.exec("loader.open", docxBytes());

    // Query A: fire the debounce and let the loop run ONE page, then pause inside
    // the per-page yield (advance just past the debounce, not the whole loop).
    await kernel.exec("search.query", "AAA");
    await vi.advanceTimersByTimeAsync(DEBOUNCE); // debounce fires; page 0 runs, then yields
    const aaaPagesBeforeSupersede = wasm.searchCalls.filter((c) => c.query === "AAA").length;
    expect(aaaPagesBeforeSupersede).toBeGreaterThan(0);
    expect(aaaPagesBeforeSupersede).toBeLessThan(4); // the loop has NOT finished A

    // Query B supersedes A mid-loop (during the yield gap). The stale A loop must
    // self-abort on its next generation check and run NO further pages.
    await kernel.exec("search.query", "BBB");
    await flush();

    // A stopped early: it never searched all 4 pages.
    expect(wasm.searchCalls.filter((c) => c.query === "AAA").length).toBe(aaaPagesBeforeSupersede);
    // B ran cleanly over all 4 pages; the result set is purely B's (no A leakage).
    expect(wasm.searchCalls.filter((c) => c.query === "BBB").map((c) => c.page)).toEqual([0, 1, 2, 3]);
    const results = (await kernel.exec("search.results")) as SearchMatch[];
    expect(results).toEqual([match(0), match(1), match(2), match(3)]);
    // The live (sticky) search:changed reflects B, status done.
    let latest: SearchChanged | undefined;
    kernel.bus.subscribe("search:changed", (e) => {
      latest = e;
    });
    expect(latest?.query).toBe("BBB");
    expect(latest?.status).toBe("done");
  });

  it("scope switch cancels an in-flight workbook scan and re-runs for the new scope", async () => {
    // SML: 3 sheets; a match on each sheet's first page (0,2,3).
    const { kernel, wasm } = await bootKernel(
      6,
      SHEETS,
      { 0: [match(0, "Sheet1")], 2: [match(2, "Sheet2")], 3: [match(3, "Sheet3")] },
    );
    await kernel.exec("loader.open", xlsxBytes());

    // Default scope = 'sheet': only the active sheet (Sheet1, firstPage 0).
    await kernel.exec("search.query", "foo");
    await flush();
    let results = (await kernel.exec("search.results")) as SearchMatch[];
    expect(results).toEqual([match(0, "Sheet1")]);
    expect(wasm.searchCalls.map((c) => c.page)).toEqual([0]);

    // Flip to 'workbook': cancels + re-runs over every sheet's firstPage.
    // The changed event must CARRY the scope (a consumer's "Within" control
    // renders from it — sticky replay hands it to late subscribers too).
    let lastScope: string | undefined;
    const un = kernel.bus.subscribe("search:changed", (e) => {
      lastScope = e.scope;
    });
    wasm.searchCalls.length = 0;
    await kernel.exec("search.setScope", "workbook");
    await flush();
    results = (await kernel.exec("search.results")) as SearchMatch[];
    expect(results).toEqual([match(0, "Sheet1"), match(2, "Sheet2"), match(3, "Sheet3")]);
    expect(wasm.searchCalls.map((c) => c.page)).toEqual([0, 2, 3]);
    expect(wasm.searchCalls.every((c) => c.scope === "workbook")).toBe(true);
    expect(lastScope).toBe("workbook");
    un();
  });

  it("next/prev wrap around the result set", async () => {
    const { kernel } = await bootKernel(3, [], { 0: [match(0)], 1: [match(1)], 2: [match(2)] });
    await kernel.exec("loader.open", docxBytes());
    await kernel.exec("search.query", "foo");
    await flush();
    expect(await kernel.exec("search.active")).toBe(0);

    await kernel.exec("search.next");
    expect(await kernel.exec("search.active")).toBe(1);
    await kernel.exec("search.next");
    expect(await kernel.exec("search.active")).toBe(2);
    // next past the last wraps to the first.
    await kernel.exec("search.next");
    expect(await kernel.exec("search.active")).toBe(0);
    // prev before the first wraps to the last.
    await kernel.exec("search.prev");
    expect(await kernel.exec("search.active")).toBe(2);
  });

  it("goto sets the active index and emits search:navigate with {page,y,sheet} + grid coord", async () => {
    const { kernel } = await bootKernel(
      4,
      SHEETS,
      { 0: [match(0, "Sheet1")], 2: [match(2, "Sheet2")], 3: [match(3, "Sheet3")] },
    );
    await kernel.exec("loader.open", xlsxBytes());
    await kernel.exec("search.setScope", "workbook");
    await kernel.exec("search.query", "foo");
    await flush();

    const navs: SearchNavigate[] = [];
    kernel.bus.subscribe("search:navigate", (e) => navs.push(e));

    await kernel.exec("search.goto", 2);
    expect(await kernel.exec("search.active")).toBe(2);
    // The navigate carries the 3rd match's page/x/y/sheet (Sheet3 firstPage 3) plus
    // the SML grid coord (cellRef "A3", gridX 30, gridY 300) for the grid scroller.
    expect(navs.at(-1)).toEqual({
      page: 3,
      x: 0,
      y: 300,
      sheet: "Sheet3",
      cellRef: "A3",
      gridX: 30,
      gridY: 300,
    });
  });

  it("clear drops the query + results and clears the wasm highlight", async () => {
    const { kernel, wasm } = await bootKernel(2, [], { 0: [match(0)], 1: [match(1)] });
    await kernel.exec("loader.open", docxBytes());
    await kernel.exec("search.query", "foo");
    await flush();
    expect(((await kernel.exec("search.results")) as SearchMatch[]).length).toBe(2);

    await kernel.exec("search.clear");
    expect(await kernel.exec("search.results")).toEqual([]);
    expect(await kernel.exec("search.active")).toBeNull();
    // The clear pushed an empty-query highlight (clears all highlights).
    expect(wasm.highlightCalls.at(-1)?.query).toBe("");
  });

  it("resets on a doc-switch loader status (loading), aborting any in-flight loop", async () => {
    const { kernel } = await bootKernel(3, [], { 0: [match(0)], 1: [match(1)], 2: [match(2)] });
    await kernel.exec("loader.open", docxBytes());
    await kernel.exec("search.query", "foo");
    await flush();
    expect(((await kernel.exec("search.results")) as SearchMatch[]).length).toBe(3);

    const events: SearchChanged[] = [];
    kernel.bus.subscribe("search:changed", (e) => events.push(e));

    // A new load drives loader → loading (status-changed) → plugin-search resets.
    const p = kernel.exec("loader.open", docxBytes());
    expect(await kernel.exec("search.results")).toEqual([]);
    expect(await kernel.exec("search.active")).toBeNull();
    // The reset emitted an empty set.
    expect(events.some((e) => e.results.length === 0 && e.query === "")).toBe(true);
    await p;
  });

  it("an empty query clears highlights without running a loop", async () => {
    const { kernel, wasm } = await bootKernel(3, [], { 0: [match(0)] });
    await kernel.exec("loader.open", docxBytes());

    await kernel.exec("search.query", "");
    await flush();

    // No searchPage call for an empty query; highlight cleared (empty query).
    expect(wasm.searchCalls.length).toBe(0);
    expect(wasm.highlightCalls.at(-1)?.query).toBe("");
    expect(await kernel.exec("search.results")).toEqual([]);
  });

  it("depends on viewport and sheets (boots after them)", async () => {
    const wasm = new FakeWasm(1);
    const viewer = makeViewer(wasm);
    const kernel = createKernel();
    kernel.use(loaderPlugin(viewer));
    kernel.use(viewportPlugin(viewer));
    kernel.use(searchPlugin(viewer)); // missing sheets
    await expect(kernel.boot()).rejects.toThrow(/unknown plugin: sheets/);
  });
  it("owns its repaint: search.query alone emits page-invalidate paint intents (no chrome, no unwritten rule)", async () => {
    // The retired contract gap: highlights only painted if the CONSUMER knew to
    // exec viewport.invalidateVisible after every search:changed — a rule
    // discoverable only from plugin source. The plugin owns it now.
    const { kernel } = await bootKernel(3, [], { 0: [match(0)] });
    await kernel.exec("loader.open", docxBytes());
    await kernel.exec("viewport.setVisible", [0]);

    const intents: string[] = [];
    const un = kernel.paint.onPaintIntent((i) => {
      if (i.tag === "page-invalidate") intents.push(`p${(i as { pageIndex: number }).pageIndex}`);
    });
    await kernel.exec("search.query", "foo");
    await flush();
    un();
    expect(intents.length).toBeGreaterThanOrEqual(1); // RED before the fix: zero
    expect(intents).toContain("p0");
  });

  // ── the settled-navigation contract (the "settle dance", owned by the plugin) ──
  //
  // These pins replicate what every integration previously had to get right by
  // hand: sticky-replay arming, target-sheet verification, deferred-rect
  // resolution, and supersede. They are written against the RESOLVED event —
  // the plugin's product — so the dance itself can never leak back downstream.

  it("resolved: a WML navigate settles immediately with page coordinates", async () => {
    const { kernel } = await bootKernel(3, [], { 2: [match(2)] });
    await kernel.exec("loader.open", docxBytes());
    const resolved: unknown[] = [];
    kernel.bus.subscribe("search:navigate-resolved", (e) => resolved.push(e.target));
    await kernel.exec("search.setScope", "workbook");
    await kernel.exec("search.query", "foo");
    await flush();
    await kernel.exec("search.next");
    await flush();
    expect(resolved).toEqual([{ kind: "page", page: 2, x: 0, y: 200 }]);
  });

  it("resolved: a cross-sheet grid navigate selects the sheet and settles ONLY on the target resetup — sticky replay must not satisfy it", async () => {
    // Match lives on Sheet2 (page 2); active sheet starts 0; grid mode.
    const { kernel } = await bootKernel(4, SHEETS, { 2: [match(2, "Sheet2")] });
    await kernel.exec("loader.open", xlsxBytes());
    kernel.bus.emit("grid:mode-changed", { mode: "grid" });
    // ARM THE TRAP with the consumer's REAL bug shape: a stale sticky resetup
    // for the TARGET sheet itself (from an earlier visit). The settle
    // subscription replays it synchronously; the payload check cannot deflect
    // it (right sheet!) — only the replay-window skip can.
    kernel.bus.emit("grid:resetup", { sheet: 1 });

    const selects: number[] = [];
    kernel.bus.subscribe("sheets:changed", (e) => selects.push(e.active));
    const resolved: unknown[] = [];
    kernel.bus.subscribe("search:navigate-resolved", (e) => resolved.push(e.target));

    await kernel.exec("search.setScope", "workbook");
    await kernel.exec("search.query", "foo");
    await flush();
    await kernel.exec("search.next");
    await flush();
    // The plugin selected the target sheet…
    expect(selects[selects.length - 1]).toBe(1);
    // …but MUST NOT have resolved yet: no post-switch resetup has arrived
    // (the stale sheet-0 replay is the only thing it has seen).
    expect(resolved).toEqual([]);
    // The adapter finishes its re-setup for the TARGET sheet → now it resolves.
    kernel.bus.emit("grid:resetup", { sheet: 1 });
    await flush();
    expect(resolved).toEqual([
      { kind: "grid", sheet: 1, cellRef: "A2", gridX: 20, gridY: 200 },
    ]);
  });

  it("resolved: pages-mode cross-sheet resolves the deferred rect through the viewer (page-origin fallback on null)", async () => {
    const { kernel, wasm } = await bootKernel(4, SHEETS, { 2: [match(2, "Sheet2")] });
    await kernel.exec("loader.open", xlsxBytes());
    kernel.bus.emit("grid:mode-changed", { mode: "pages" });
    wasm.searchRect = { page: 2, x: 33, y: 44, w: 10, h: 10 };
    const resolved: unknown[] = [];
    kernel.bus.subscribe("search:navigate-resolved", (e) => resolved.push(e.target));
    await kernel.exec("search.setScope", "workbook");
    await kernel.exec("search.query", "foo");
    await flush();
    await kernel.exec("search.next");
    await flush();
    // sheets:changed (sticky, emitted by sheets.select) is the pages-mode
    // settle; the select above already fired it, so resolution completes.
    expect(resolved).toEqual([{ kind: "page", page: 2, x: 33, y: 44 }]);
    expect(wasm.rectCalls).toEqual([{ sheet: 1, cellRef: "A2" }]);
  });

  it("resolved: a supersede mid-settle drops the pending resolution", async () => {
    const { kernel } = await bootKernel(4, SHEETS, { 2: [match(2, "Sheet2")] });
    await kernel.exec("loader.open", xlsxBytes());
    kernel.bus.emit("grid:mode-changed", { mode: "grid" });
    const resolved: unknown[] = [];
    kernel.bus.subscribe("search:navigate-resolved", (e) => resolved.push(e.target));
    await kernel.exec("search.setScope", "workbook");
    await kernel.exec("search.query", "foo");
    await flush();
    await kernel.exec("search.next"); // settle now pending on sheet 1's resetup
    await flush();
    await kernel.exec("search.clear"); // supersede while pending
    kernel.bus.emit("grid:resetup", { sheet: 1 }); // the settle arrives too late
    await flush();
    expect(resolved).toEqual([]); // RED if the pending resolution leaked through
  });

});
