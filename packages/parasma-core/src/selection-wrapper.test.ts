/**
 * Selection Viewer-wrapper tests (the unified selection verbs).
 *
 * Surfaces `hitTest` / `selectionRects` (async worker-ready reads) and
 * `setSelection` / `setSelectedObject` (sync fire-and-forget notifies) on the
 * core Viewer over the unified `doc*` wasm exports. These are the seam
 * plugin-selection drives; here we assert the IN-PROCESS branch (`#remote`
 * stays null — injected FakeWasm) round-trips arguments and shapes faithfully,
 * and that a stale build (export absent) degrades to `null`/`[]`/no-op rather
 * than throwing. The worker (`#remote`) path is covered by worker-remote.test.
 *
 * Injection mirrors load-kind.test / search.test:
 * `new (Viewer as unknown as { new(w): Viewer })(fake)`.
 */
import { describe, it, expect } from "vitest";
import {
  Viewer,
  type DocFragment,
  type DocHit,
  type DocLoc,
  type DocSelection,
  type ObjectRef,
  type Rect,
  type TextPos,
} from "./index";

/** A FakeWasm exposing the unified selection exports + recording calls. */
class FakeWasm {
  readonly hitTestCalls: unknown[][] = [];
  readonly rectsCalls: unknown[][] = [];
  readonly setSelectionCalls: Array<DocSelection | null> = [];
  readonly extractCalls: Array<DocSelection> = [];

  constructor(
    private readonly hitResult: DocHit | null = null,
    private readonly rects: Rect[] = [],
    private readonly fragment: DocFragment | null = null,
  ) {}

  docHitTest(loc: DocLoc, x: number, y: number, granularity: string): DocHit | null {
    this.hitTestCalls.push([loc, x, y, granularity]);
    return this.hitResult;
  }
  docSelectionRects(loc: DocLoc, sel: DocSelection): Rect[] {
    this.rectsCalls.push([loc, sel]);
    return this.rects;
  }
  docSetSelection(sel: DocSelection | null): void {
    this.setSelectionCalls.push(sel);
  }
  docExtractRange(sel: DocSelection): DocFragment | null {
    this.extractCalls.push(sel);
    return this.fragment;
  }

  // ── object hit-test + outline setter ──────────────────────────────────────
  objectHitRaw: { surface: number; kind: string; key: string; rect: Rect } | null = null;
  readonly hitTestObjectCalls: unknown[][] = [];
  readonly setSelectedObjectCalls: Array<{ surface: number; key: string } | null> = [];
  docHitTestObject(loc: DocLoc, x: number, y: number): unknown {
    this.hitTestObjectCalls.push([loc, x, y]);
    return this.objectHitRaw;
  }
  docSetSelectedObject(obj: { surface: number; key: string } | null): void {
    this.setSelectedObjectCalls.push(obj);
  }
}

/** A FakeWasm WITHOUT the selection exports (stale build). */
class BareWasm {}

function makeViewer(wasm: unknown): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

const pos = (para: number, byte: number): TextPos => ({ para, byte });
const text = (start: TextPos, end: TextPos): DocSelection => ({ kind: "text", start, end });

describe("Viewer selection wrapper (in-process)", () => {
  it("hitTest forwards (loc,x,y,granularity) and maps the result through", async () => {
    const hit: DocHit = { kind: "text", surface: 2, pos: pos(3, 12) };
    const wasm = new FakeWasm(hit);
    const viewer = makeViewer(wasm);

    const out = await viewer.hitTest({ surface: 2 }, 40, 88, "char");
    expect(wasm.hitTestCalls).toEqual([[{ surface: 2 }, 40, 88, "char"]]);
    expect(out).toEqual(hit);
  });

  it("hitTest defaults granularity to 'char' and passes word/paragraph + range through", async () => {
    const hit: DocHit = {
      kind: "text",
      surface: 0,
      pos: pos(1, 4),
      range: { start: pos(1, 0), end: pos(1, 9) },
    };
    const wasm = new FakeWasm(hit);
    const viewer = makeViewer(wasm);

    // Default granularity.
    await viewer.hitTest({ surface: 0 }, 1, 2);
    expect(wasm.hitTestCalls.at(-1)).toEqual([{ surface: 0 }, 1, 2, "char"]);

    // Explicit word granularity: range passthrough intact.
    const out = await viewer.hitTest({ surface: 0 }, 1, 2, "word");
    expect(wasm.hitTestCalls.at(-1)).toEqual([{ surface: 0 }, 1, 2, "word"]);
    expect(out?.kind === "text" ? out.range : undefined).toEqual({ start: pos(1, 0), end: pos(1, 9) });
  });

  it("hitTest forwards a grid loc (scroll offsets) untouched", async () => {
    const hit: DocHit = { kind: "cell", surface: 1, zone: "cell", row: 3, col: 2 };
    const wasm = new FakeWasm(hit);
    const viewer = makeViewer(wasm);

    const loc: DocLoc = { surface: 1, scrollX: 100, scrollY: 250 };
    const out = await viewer.hitTest(loc, 40, 88);
    expect(wasm.hitTestCalls).toEqual([[loc, 40, 88, "char"]]);
    expect(out).toEqual(hit);
  });

  it("hitTest returns null when the point misses (wasm returns null)", async () => {
    const viewer = makeViewer(new FakeWasm(null));
    expect(await viewer.hitTest({ surface: 0 }, 0, 0, "char")).toBeNull();
  });

  it("hitTest returns null (no throw) on a stale build without the export", async () => {
    const viewer = makeViewer(new BareWasm());
    expect(await viewer.hitTest({ surface: 0 }, 0, 0, "char")).toBeNull();
  });

  it("selectionRects forwards (loc, sel) and returns Rect[]", async () => {
    const rects: Rect[] = [
      { x: 1, y: 2, w: 3, h: 4 },
      { x: 5, y: 6, w: 7, h: 8 },
    ];
    const wasm = new FakeWasm(null, rects);
    const viewer = makeViewer(wasm);

    const sel = text(pos(1, 5), pos(2, 9));
    const out = await viewer.selectionRects({ surface: 4 }, sel);
    expect(wasm.rectsCalls).toEqual([[{ surface: 4 }, sel]]);
    expect(out).toEqual(rects);
  });

  it("selectionRects returns [] on a stale build without the export", async () => {
    const viewer = makeViewer(new BareWasm());
    expect(await viewer.selectionRects({ surface: 0 }, text(pos(0, 0), pos(0, 0)))).toEqual([]);
  });

  // Regression: raw scalars where TextPos objects belong
  // used to coerce undefined→0 and collapse EVERY range to [] (masqueraded as
  // an engine bug). It throws LOUD at the call site instead.
  it("selectionRects throws (not silently []) when start/end are raw scalars", async () => {
    const wasm = new FakeWasm(null, [{ x: 1, y: 2, w: 3, h: 4 }]);
    const viewer = makeViewer(wasm);
    await expect(
      viewer.selectionRects(
        { surface: 0 },
        text(0 as unknown as TextPos, 9 as unknown as TextPos),
      ),
    ).rejects.toThrow(TypeError);
    expect(wasm.rectsCalls).toEqual([]); // never reached the wasm boundary
  });

  it("setSelection throws when start/end are raw scalars", () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    expect(() =>
      viewer.setSelection(text(0 as unknown as TextPos, 9 as unknown as TextPos)),
    ).toThrow(TypeError);
    expect(wasm.setSelectionCalls).toEqual([]);
  });

  it("setSelection forwards the DocSelection once and returns undefined synchronously", () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);

    const sel = text(pos(2, 3), pos(5, 7));
    const ret = viewer.setSelection(sel);
    expect(ret).toBeUndefined();
    expect(wasm.setSelectionCalls).toEqual([sel]);
  });

  it("setSelection(null) forwards the unified clear (no sentinel)", () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);

    viewer.setSelection(null);
    expect(wasm.setSelectionCalls).toEqual([null]);
  });

  it("setSelection set/clear are no-ops (no throw) on a bare build", () => {
    const viewer = makeViewer(new BareWasm());
    expect(() => viewer.setSelection(text(pos(0, 0), pos(0, 1)))).not.toThrow();
    expect(() => viewer.setSelection(null)).not.toThrow();
  });

  // ── extractRange (the selection carries its own scope — no loc arg) ────────
  const frag = (): DocFragment => ({
    blocks: [{ kind: "para", para: { runs: [], alignment: "start", dir: "ltr", list: null } }],
  });

  it("extractRange forwards the DocSelection — no loc — and maps through", async () => {
    const f = frag();
    const wasm = new FakeWasm(null, [], f);
    const viewer = makeViewer(wasm);

    const sel = text(pos(1, 5), pos(2, 9));
    const out = await viewer.extractRange(sel);
    expect(wasm.extractCalls).toEqual([sel]);
    expect(out).toEqual(f);
  });

  it("extractRange returns null on a stale build without the export", async () => {
    const viewer = makeViewer(new BareWasm());
    expect(await viewer.extractRange(text(pos(0, 0), pos(0, 1)))).toBeNull();
  });

  it("extractRange returns null when the export returns null (unservable kind)", async () => {
    const viewer = makeViewer(new FakeWasm(null, [], null));
    expect(await viewer.extractRange(text(pos(0, 0), pos(0, 1)))).toBeNull();
  });

  it("extractRange throws (not silent) when start/end are raw scalars", async () => {
    const wasm = new FakeWasm(null, [], frag());
    const viewer = makeViewer(wasm);
    await expect(
      viewer.extractRange(text(0 as unknown as TextPos, 9 as unknown as TextPos)),
    ).rejects.toThrow(TypeError);
    expect(wasm.extractCalls).toEqual([]); // never reached the wasm boundary
  });

  it("extractRangeSyncInProcess returns the fragment in-process (no await)", () => {
    const f = frag();
    const wasm = new FakeWasm(null, [], f);
    const viewer = makeViewer(wasm);
    const sel = text(pos(0, 0), pos(0, 4));
    expect(viewer.extractRangeSyncInProcess(sel)).toEqual(f);
    expect(wasm.extractCalls).toEqual([sel]);
  });

  it("extractRangeSyncInProcess returns null on a bare build (no throw)", () => {
    const viewer = makeViewer(new BareWasm());
    expect(viewer.extractRangeSyncInProcess(text(pos(0, 0), pos(0, 1)))).toBeNull();
  });

  // ── object hit-test mapping + outline setter ──────────────────────────────
  it("hitTestObject maps engine {surface,kind,key,rect} → {object:{page,kind,key,index:0},rect}", async () => {
    const wasm = new FakeWasm();
    wasm.objectHitRaw = {
      surface: 2,
      kind: "inlineImage",
      key: "rIdImg",
      rect: { x: 1, y: 2, w: 30, h: 16 },
    };
    const viewer = makeViewer(wasm);

    const out = await viewer.hitTestObject({ surface: 2 }, 40, 88);
    expect(wasm.hitTestObjectCalls).toEqual([[{ surface: 2 }, 40, 88]]);
    expect(out).toEqual({
      object: { page: 2, kind: "inlineImage", key: "rIdImg", index: 0 },
      rect: { x: 1, y: 2, w: 30, h: 16 },
    });
  });

  it("hitTestObject returns null when the point misses (wasm returns null)", async () => {
    const viewer = makeViewer(new FakeWasm());
    expect(await viewer.hitTestObject({ surface: 0 }, 0, 0)).toBeNull();
  });

  it("hitTestObject returns null (no throw) on a stale build without the export", async () => {
    const viewer = makeViewer(new BareWasm());
    expect(await viewer.hitTestObject({ surface: 0 }, 0, 0)).toBeNull();
  });

  it("setSelectedObject forwards {surface,key}; (null) forwards null", () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const ref: ObjectRef = { page: 3, kind: "anchoredImage", key: "rIdA", index: 0 };

    viewer.setSelectedObject(ref);
    viewer.setSelectedObject(null);
    expect(wasm.setSelectedObjectCalls).toEqual([{ surface: 3, key: "rIdA" }, null]);
  });

  it("setSelectedObject is a no-op (no throw) on a stale build", () => {
    const viewer = makeViewer(new BareWasm());
    expect(() =>
      viewer.setSelectedObject({ page: 0, kind: "inlineImage", key: "x", index: 0 }),
    ).not.toThrow();
  });
});
