/**
 * plugin-selection tests (text arm).
 *
 * plugin-selection owns the SELECTION model + pointer pipeline: a `@xstate/store`
 * slice (NOT an XState machine — no cancellable loop), deps:["viewport"], driving
 * `viewer.hitTest` (async read, awaited internally) + `viewer.setSelection`
 * (fire-and-forget), emitting `selection:changed`. It is mode-aware (drag
 * only in tool mode 'select', default) and enforces LATEST-WINS via a monotonic
 * gesture-seq that DROPS a stale async hitTest.
 *
 * We boot it alongside the REAL loader/viewport plugins over the same
 * FakeWasm/Viewer injection the search/sheets tests use. The FakeWasm's unified
 * `docHitTest` derives a deterministic position from the query x (`byte = x`)
 * and can DEFER resolution (a controllable promise) so the out-of-order/
 * stale-drop path is exercised: the real `Viewer.hitTest` awaits the fake's
 * promise, so resolving two drags' hitTests in reverse order proves the newer
 * wins. `docSetSelection` records every DocSelection (a clear records `null`,
 * the unified clear).
 */
import { describe, it, expect, vi } from "vitest";
import { Viewer } from "@parasma/core";
import { createKernel } from "..";
import { loaderPlugin } from "./loader";
import { viewportPlugin } from "./viewport";
import { selectionPlugin, type SelectionState } from "./selection";

type SelectionChanged = {
  kind: "none" | "text" | "object";
  collapsed: boolean;
  anchor?: { para: number; byte: number };
  focus?: { para: number; byte: number };
  object?: { page: number; kind: string; key: string; index: number };
  seq: number;
};

/** A recorded text DocSelection (the only kind this file's gestures produce). */
type RecordedSel = {
  kind: string;
  start: { para: number; byte: number };
  end: { para: number; byte: number };
} | null;

/** Shorthand for the expected recorded text span (sp,sb) → (ep,eb). */
const span = (sp: number, sb: number, ep: number, eb: number): RecordedSel => ({
  kind: "text",
  start: { para: sp, byte: sb },
  end: { para: ep, byte: eb },
});

/**
 * FakeWasm: the loader/viewport boot surface + the unified selection exports.
 * `docHitTest` returns `{kind:"text", surface, pos:{para:0, byte:x}}`
 * (deterministic per query x). When `deferred` is set, it returns a promise
 * held in `pending` for manual, possibly out-of-order, resolution.
 * `docSetSelection` records every call (`null` = the unified clear).
 */
class FakeWasm {
  deferred = false;
  readonly pending: Array<() => void> = [];
  readonly setSelectionCalls: RecordedSel[] = [];
  readonly hitTestCalls: number[][] = [];

  constructor(private readonly pages: number = 1) {}

  // ── loader/viewport boot surface ──────────────────────────────────────────
  loadDocx(_bytes: Uint8Array): void {}
  loadXlsx(_bytes: Uint8Array): void {}
  loadPptx(_bytes: Uint8Array): void {}
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
  sheetInfo(): unknown[] {
    return [];
  }

  // ── unified selection exports ─────────────────────────────────────────────
  // Deterministic per query x: char → collapsed pos at byte x; word → a 3-byte
  // range [x-1, x+2] around x; paragraph → the whole-para range [0, 100]. This
  // lets the gesture tests assert the ENGINE range is applied verbatim.
  docHitTest(loc: { surface: number }, x: number, _y: number, g: string): unknown {
    this.hitTestCalls.push([loc.surface, x]);
    const pos = { para: 0, byte: x };
    let result: unknown = { kind: "text", surface: loc.surface, pos };
    if (g === "word") {
      result = { kind: "text", surface: loc.surface, pos, range: { start: { para: 0, byte: x - 1 }, end: { para: 0, byte: x + 2 } } };
    } else if (g === "paragraph") {
      result = { kind: "text", surface: loc.surface, pos, range: { start: { para: 0, byte: 0 }, end: { para: 0, byte: 100 } } };
    }
    if (!this.deferred) return result;
    return new Promise((resolve) => {
      this.pending.push(() => resolve(result));
    });
  }
  docSetSelection(sel: RecordedSel): void {
    this.setSelectionCalls.push(sel);
  }

  // ── object selection exports ──────────────────────────────────────────────
  // When `objectHitX` is set, a query at that x reports an image object hit
  // (unified wire shape: `{surface, kind, key, rect}`). `docSetSelectedObject`
  // records every `{surface, key}` (`null` = the clear).
  objectHitX: number | null = null;
  readonly setSelectedObjectCalls: Array<{ surface: number; key: string } | null> = [];
  docHitTestObject(loc: { surface: number }, x: number, _y: number): unknown {
    if (this.objectHitX !== null && x === this.objectHitX) {
      return { surface: loc.surface, kind: "inlineImage", key: "rIdImg", rect: { x: 0, y: 0, w: 50, h: 16 } };
    }
    return null;
  }
  docSetSelectedObject(obj: { surface: number; key: string } | null): void {
    this.setSelectedObjectCalls.push(obj);
  }
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

async function bootKernel(pages = 1, selOpts?: Parameters<typeof selectionPlugin>[1]) {
  const wasm = new FakeWasm(pages);
  const viewer = makeViewer(wasm);
  const kernel = createKernel();
  kernel.use(loaderPlugin(viewer));
  kernel.use(viewportPlugin(viewer));
  kernel.use(selectionPlugin(viewer, selOpts));
  await kernel.boot();
  // Boot drives the loader through its status edges, which the plugin's
  // doc-switch subscription treats as a reset (setSelection(null) — the
  // unified clear). That is correct behavior but noise for these assertions — baseline
  // the recorders so each test sees only its own interactions.
  wasm.setSelectionCalls.length = 0;
  wasm.hitTestCalls.length = 0;
  return { kernel, viewer, wasm };
}

/** Subscribe to selection:changed and collect events fired AFTER this call. */
function captureChanges(kernel: Awaited<ReturnType<typeof bootKernel>>["kernel"]) {
  const events: SelectionChanged[] = [];
  // "latest" replay delivers the current retained event first; drop it so the
  // array holds only edges produced by the subsequent interaction.
  kernel.bus.subscribe("selection:changed", (e) => events.push(e as SelectionChanged));
  events.length = 0;
  return events;
}

type MenuModel = {
  clientX: number;
  clientY: number;
  items: { id: string; label: string; enabled: boolean; onSelect?: () => void }[];
};

/** Subscribe to selection:menu and collect the emitted menu models. */
function captureMenus(kernel: Awaited<ReturnType<typeof bootKernel>>["kernel"]) {
  const events: MenuModel[] = [];
  kernel.bus.subscribe("selection:menu", (e) => events.push(e as MenuModel));
  events.length = 0;
  return events;
}

describe("plugin-selection", () => {
  it("down → move → up produces the selection:changed sequence + setSelection calls", async () => {
    const { kernel, wasm } = await bootKernel();
    const events = captureChanges(kernel);

    // down at x=5 → anchor === focus (collapsed caret).
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 5, y: 1 });
    // move to x=15 → focus extends.
    await kernel.exec("selection.drag", { loc: { surface: 0 }, x: 15, y: 1 });
    // up → drag ends, span held.
    await kernel.exec("selection.end");

    // setSelection: collapsed anchor, then the extended span. (No clear.)
    expect(wasm.setSelectionCalls).toEqual([
      span(0, 5, 0, 5),
      span(0, 5, 0, 15),
    ]);
    // selection:changed edges: text/collapsed (down), text (move), text (up).
    expect(events.map((e) => `${e.kind}:${e.collapsed}`)).toEqual([
      "text:true",
      "text:false",
      "text:false",
    ]);
    expect(events[1]?.anchor).toEqual({ para: 0, byte: 5 });
    expect(events[1]?.focus).toEqual({ para: 0, byte: 15 });

    // dragging transitioned true → false across the gesture.
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind).toBe("text");
    if (sel.kind === "text") {
      expect(sel.dragging).toBe(false);
      expect(sel.anchor).toEqual({ para: 0, byte: 5 });
      expect(sel.focus).toEqual({ para: 0, byte: 15 });
    }
  });

  it("LATEST-WINS: a stale hitTest resolving after a newer one is DROPPED", async () => {
    const { kernel, wasm } = await bootKernel();

    // Establish a live drag (anchor byte 10) with immediate resolution.
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 10, y: 1 });
    expect(wasm.setSelectionCalls).toEqual([span(0, 10, 0, 10)]);

    // Now DEFER hitTest resolution and fire two drags whose reads are in flight.
    wasm.deferred = true;
    const pA = kernel.exec("selection.drag", { loc: { surface: 0 }, x: 20, y: 1 }); // seq N   (older)
    const pB = kernel.exec("selection.drag", { loc: { surface: 0 }, x: 30, y: 1 }); // seq N+1 (newer)

    // Both hitTests are queued; resolve OUT OF ORDER — the newer (B) first, then
    // the older (A) last. A must be dropped (its seq is now stale).
    expect(wasm.pending.length).toBe(2);
    wasm.pending[1]!(); // resolve B (x=30)
    wasm.pending[0]!(); // resolve A (x=20) — stale
    await Promise.all([pA, pB]);

    // The newer drag won: the last setSelection is the byte-30 span; the stale
    // byte-20 span NEVER landed.
    expect(wasm.setSelectionCalls).toEqual([
      span(0, 10, 0, 10),
      span(0, 10, 0, 30),
    ]);
    expect(wasm.setSelectionCalls.some((c) => c?.end.byte === 20)).toBe(false);

    const sel = (await kernel.exec("selection.get")) as SelectionState;
    if (sel.kind === "text") expect(sel.focus.byte).toBe(30);
  });

  it("ESC (selection.clear) drops to none, clears the engine selection, emits none", async () => {
    const { kernel, wasm } = await bootKernel();
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 7, y: 1 });

    const events = captureChanges(kernel);
    await kernel.exec("selection.clear");

    expect((await kernel.exec("selection.get")) as SelectionState).toEqual({ kind: "none" });
    // setSelection(null) — the unified clear.
    expect(wasm.setSelectionCalls.at(-1)).toEqual(null);
    expect(events.at(-1)).toMatchObject({ kind: "none", collapsed: true });
  });

  it("doc-switch (loader status 'loading') resets to none + clears the engine selection", async () => {
    const { kernel, wasm } = await bootKernel();
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 9, y: 1 });
    await kernel.exec("selection.drag", { loc: { surface: 0 }, x: 21, y: 1 });
    expect(((await kernel.exec("selection.get")) as SelectionState).kind).toBe("text");

    const events = captureChanges(kernel);
    // A new load drives loader → loading; the plugin resets off the status edge.
    kernel.bus.emit("loader:status-changed", "loading");

    expect((await kernel.exec("selection.get")) as SelectionState).toEqual({ kind: "none" });
    expect(wasm.setSelectionCalls.at(-1)).toEqual(null);
    expect(events.at(-1)).toMatchObject({ kind: "none", collapsed: true });
  });

  it("mode-aware: in 'hand' mode a drag produces NO selection; flipping back restores it", async () => {
    const { kernel, wasm } = await bootKernel();

    // Enter hand mode (plugin-tool would emit this; here we drive the seam directly).
    kernel.bus.emit("tool:mode-changed", { mode: "hand" });
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 4, y: 1 });
    await kernel.exec("selection.drag", { loc: { surface: 0 }, x: 18, y: 1 });

    // Suppressed entirely: no setSelection, no text state, no hitTest even.
    expect(wasm.setSelectionCalls).toEqual([]);
    expect(wasm.hitTestCalls).toEqual([]);
    expect((await kernel.exec("selection.get")) as SelectionState).toEqual({ kind: "none" });

    // Flip back to select → selection works again.
    kernel.bus.emit("tool:mode-changed", { mode: "select" });
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 4, y: 1 });
    await kernel.exec("selection.drag", { loc: { surface: 0 }, x: 18, y: 1 });
    expect(wasm.setSelectionCalls).toEqual([
      span(0, 4, 0, 4),
      span(0, 4, 0, 18),
    ]);
    expect(((await kernel.exec("selection.get")) as SelectionState).kind).toBe("text");
  });

  it("collapsed: down then up (no move) = caret; setSelection has equal endpoints", async () => {
    const { kernel, wasm } = await bootKernel();
    const events = captureChanges(kernel);

    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 8, y: 1 });
    await kernel.exec("selection.end");

    // A single collapsed setSelection (equal endpoints) — the engine paints nothing.
    expect(wasm.setSelectionCalls).toEqual([span(0, 8, 0, 8)]);
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    if (sel.kind === "text") {
      expect(sel.anchor).toEqual(sel.focus);
      expect(sel.dragging).toBe(false);
    }
    // Every emitted edge reports collapsed:true (anchor === focus throughout).
    expect(events.every((e) => e.collapsed)).toBe(true);
    expect(events.some((e) => e.kind === "text")).toBe(true);
  });

  it("object-first: pointerdown over an image selects the OBJECT, not the text under it", async () => {
    const { kernel, wasm } = await bootKernel();
    wasm.objectHitX = 42; // a query at x=42 reports an image hit
    const events = captureChanges(kernel);

    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 42, y: 1 });

    // State is an object selection carrying the mapped ObjectRef.
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind).toBe("object");
    if (sel.kind === "object") {
      expect(sel.ref).toEqual({ page: 0, kind: "inlineImage", key: "rIdImg", index: 0 });
    }
    // The outline was pushed and the text highlight cleared (setSelection(null)).
    expect(wasm.setSelectedObjectCalls.at(-1)).toEqual({ surface: 0, key: "rIdImg" });
    expect(wasm.setSelectionCalls.at(-1)).toEqual(null);
    // No text hitTest was consumed (object won before the text probe).
    expect(wasm.hitTestCalls).toEqual([]);
    // The bus edge carries the full ObjectRef.
    const last = events.at(-1)!;
    expect(last.kind).toBe("object");
    expect(last.object).toEqual({ page: 0, kind: "inlineImage", key: "rIdImg", index: 0 });
  });

  it("a text drag after an object select is a no-op (object selection held)", async () => {
    const { kernel, wasm } = await bootKernel();
    wasm.objectHitX = 42;
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 42, y: 1 });
    const before = wasm.setSelectionCalls.length;

    // A drag over plain text does nothing — the state is object, not a text drag.
    await kernel.exec("selection.drag", { loc: { surface: 0 }, x: 60, y: 1 });

    expect(((await kernel.exec("selection.get")) as SelectionState).kind).toBe("object");
    expect(wasm.setSelectionCalls.length).toBe(before); // no new text span
  });

  it("selection.clear after an object select → none + clears the outline", async () => {
    const { kernel, wasm } = await bootKernel();
    wasm.objectHitX = 42;
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 42, y: 1 });

    const events = captureChanges(kernel);
    await kernel.exec("selection.clear");

    expect((await kernel.exec("selection.get")) as SelectionState).toEqual({ kind: "none" });
    // Outline cleared via setSelectedObject(null).
    expect(wasm.setSelectedObjectCalls.at(-1)).toEqual(null);
    expect(events.at(-1)).toMatchObject({ kind: "none", collapsed: true });
  });

  it("doc-switch after an object select resets to none + clears the outline", async () => {
    const { kernel, wasm } = await bootKernel();
    wasm.objectHitX = 42;
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 42, y: 1 });
    expect(((await kernel.exec("selection.get")) as SelectionState).kind).toBe("object");

    kernel.bus.emit("loader:status-changed", "loading");

    expect((await kernel.exec("selection.get")) as SelectionState).toEqual({ kind: "none" });
    expect(wasm.setSelectedObjectCalls.at(-1)).toEqual(null);
  });

  // ── gesture completeness (word/para/extend + context menu) ────────

  it("selectWord applies the ENGINE word range (anchor=range.start, focus=range.end)", async () => {
    const { kernel, wasm } = await bootKernel();
    const events = captureChanges(kernel);

    await kernel.exec("selection.selectWord", { loc: { surface: 0 }, x: 10, y: 1 });

    // The fake's word range for x=10 is [9, 12]; applied verbatim (no TS recompute).
    expect(wasm.setSelectionCalls.at(-1)).toEqual(span(0, 9, 0, 12));
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind).toBe("text");
    if (sel.kind === "text") {
      expect(sel.anchor).toEqual({ para: 0, byte: 9 });
      expect(sel.focus).toEqual({ para: 0, byte: 12 });
      expect(sel.dragging).toBe(false); // a click, not a drag
    }
    expect(events.at(-1)).toMatchObject({ kind: "text", collapsed: false });
  });

  it("selectParagraph applies the ENGINE paragraph range", async () => {
    const { kernel, wasm } = await bootKernel();
    await kernel.exec("selection.selectParagraph", { loc: { surface: 0 }, x: 7, y: 1 });
    // The fake's paragraph range is the whole para [0, 100].
    expect(wasm.setSelectionCalls.at(-1)).toEqual(span(0, 0, 0, 100));
  });

  it("extendTo KEEPS the existing anchor and moves the focus (shift+click)", async () => {
    const { kernel, wasm } = await bootKernel();
    // Anchor a span first (down at 5, drag to 15).
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 5, y: 1 });
    await kernel.exec("selection.drag", { loc: { surface: 0 }, x: 15, y: 1 });
    await kernel.exec("selection.end");

    // shift+click at x=40 → anchor 5 held, focus moves to 40.
    await kernel.exec("selection.extendTo", { loc: { surface: 0 }, x: 40, y: 1 });
    expect(wasm.setSelectionCalls.at(-1)).toEqual(span(0, 5, 0, 40));
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    if (sel.kind === "text") {
      expect(sel.anchor).toEqual({ para: 0, byte: 5 });
      expect(sel.focus).toEqual({ para: 0, byte: 40 });
    }
  });

  it("extendTo with no prior selection anchors at the click point", async () => {
    const { kernel, wasm } = await bootKernel();
    await kernel.exec("selection.extendTo", { loc: { surface: 0 }, x: 12, y: 1 });
    expect(wasm.setSelectionCalls.at(-1)).toEqual(span(0, 12, 0, 12));
  });

  it("word/para/extend are mode-gated to 'select' (inert in hand mode)", async () => {
    const { kernel, wasm } = await bootKernel();
    kernel.bus.emit("tool:mode-changed", { mode: "hand" });
    await kernel.exec("selection.selectWord", { loc: { surface: 0 }, x: 10, y: 1 });
    await kernel.exec("selection.selectParagraph", { loc: { surface: 0 }, x: 10, y: 1 });
    await kernel.exec("selection.extendTo", { loc: { surface: 0 }, x: 10, y: 1 });
    expect(wasm.setSelectionCalls).toEqual([]);
    expect(wasm.hitTestCalls).toEqual([]);
  });

  it("word/para selection is LATEST-WINS (a stale deferred hitTest is dropped)", async () => {
    const { kernel, wasm } = await bootKernel();
    wasm.deferred = true;
    const pA = kernel.exec("selection.selectWord", { loc: { surface: 0 }, x: 20, y: 1 }); // older
    const pB = kernel.exec("selection.selectParagraph", { loc: { surface: 0 }, x: 30, y: 1 }); // newer
    expect(wasm.pending.length).toBe(2);
    wasm.pending[1]!(); // resolve B (paragraph) first
    wasm.pending[0]!(); // resolve A (word) — stale
    await Promise.all([pA, pB]);
    // Paragraph (newer) won; the stale word range never landed.
    expect(wasm.setSelectionCalls.at(-1)).toEqual(span(0, 0, 0, 100));
  });

  it("menuAt on a NON-collapsed text span INSIDE it keeps the selection; Copy enabled", async () => {
    const { kernel, wasm } = await bootKernel();
    // Select [10, 30] via selectParagraph-ish span (use word then extend for a real span).
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 10, y: 1 });
    await kernel.exec("selection.drag", { loc: { surface: 0 }, x: 30, y: 1 });
    await kernel.exec("selection.end");
    const before = wasm.setSelectionCalls.length;
    const menus = captureMenus(kernel);

    // Right-click INSIDE (x=20 is within [10,30]) with opaque host coords.
    await kernel.exec("selection.menuAt", { loc: { surface: 0 }, x: 20, y: 1, clientX: 111, clientY: 222 });

    // Selection unchanged (no re-anchor setSelection).
    expect(wasm.setSelectionCalls.length).toBe(before);
    const m = menus.at(-1)!;
    // The opaque host coords are echoed verbatim.
    expect(m.clientX).toBe(111);
    expect(m.clientY).toBe(222);
    expect(m.items).toEqual([{ id: "copy", label: "Copy", enabled: true }]);
  });

  it("menuAt OUTSIDE a text span RE-ANCHORS (collapses to the click); Copy disabled", async () => {
    const { kernel, wasm } = await bootKernel();
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 10, y: 1 });
    await kernel.exec("selection.drag", { loc: { surface: 0 }, x: 30, y: 1 });
    await kernel.exec("selection.end");
    const menus = captureMenus(kernel);

    // Right-click OUTSIDE (x=80 is past [10,30]) → collapse to the caret at 80.
    await kernel.exec("selection.menuAt", { loc: { surface: 0 }, x: 80, y: 1, clientX: 5, clientY: 6 });

    // Re-anchored: a collapsed setSelection at the click point.
    expect(wasm.setSelectionCalls.at(-1)).toEqual(span(0, 80, 0, 80));
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind).toBe("text");
    if (sel.kind === "text") expect(sel.anchor).toEqual(sel.focus); // collapsed
    // Copy disabled on a collapsed caret.
    expect(menus.at(-1)!.items[0]!.enabled).toBe(false);
  });

  it("menuAt over an OBJECT selects it (re-anchor); Copy enabled", async () => {
    const { kernel, wasm } = await bootKernel();
    wasm.objectHitX = 42;
    const menus = captureMenus(kernel);

    await kernel.exec("selection.menuAt", { loc: { surface: 0 }, x: 42, y: 1, clientX: 9, clientY: 9 });

    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind).toBe("object");
    expect(wasm.setSelectedObjectCalls.at(-1)).toEqual({ surface: 0, key: "rIdImg" });
    expect(menus.at(-1)!.items[0]!.enabled).toBe(true);
  });

  it("closeMenu (and doc-switch) emit the empty-items dismiss model", async () => {
    const { kernel } = await bootKernel();
    const menus = captureMenus(kernel);

    await kernel.exec("selection.closeMenu");
    expect(menus.at(-1)!.items).toEqual([]);

    // A doc switch also dismisses any open menu.
    menus.length = 0;
    kernel.bus.emit("loader:status-changed", "loading");
    expect(menus.some((m) => m.items.length === 0)).toBe(true);
  });

  it("depends on viewport (boots after it; missing viewport fails boot)", async () => {
    const wasm = new FakeWasm(1);
    const viewer = makeViewer(wasm);
    const kernel = createKernel();
    kernel.use(selectionPlugin(viewer)); // no loader/viewport
    await expect(kernel.boot()).rejects.toThrow(/unknown plugin: viewport/);
  });
});

// ── contextMenu seam (open menu model) ────────────────────────────────────────
// The seam is CONFIG (host policy) on the factory: selectionPlugin(viewer,
// { contextMenu }). It runs once per menu REQUEST (never on dismiss), receives a
// DATA-ONLY ctx (defensive item copies + a minimal selection summary + the
// 0-based surface index), and returns the FULL item list to show. Any throw /
// invalid return falls back to the unmodified defaults with one console.warn.
describe("plugin-selection contextMenu seam", () => {
  /** Select the span [10,30] so the seam sees a non-collapsed text selection. */
  async function selectSpan(kernel: Awaited<ReturnType<typeof bootKernel>>["kernel"]) {
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 10, y: 1 });
    await kernel.exec("selection.drag", { loc: { surface: 0 }, x: 30, y: 1 });
    await kernel.exec("selection.end");
  }

  it("appends a custom item; onSelect survives the bus by reference; ctx carries defaults + kind + page", async () => {
    const onSelect = vi.fn();
    let seenCtx: {
      items: { id: string; label: string; enabled: boolean }[];
      selection: { kind: string; collapsed: boolean };
      page: number;
    } | null = null;
    const { kernel } = await bootKernel(3, {
      contextMenu: (c) => {
        seenCtx = c;
        return [...c.items, { id: "host.annotate", label: "Annotate", enabled: true, onSelect }];
      },
    });
    await selectSpan(kernel);
    const menus = captureMenus(kernel);

    // Right-click INSIDE the span on page 2 (x=20 within [10,30]).
    await kernel.exec("selection.menuAt", { loc: { surface: 2 }, x: 20, y: 1, clientX: 7, clientY: 8 });

    const m = menus.at(-1)!;
    expect(m.items.map((i) => i.id)).toEqual(["copy", "host.annotate"]);
    expect(m.items[1]!.label).toBe("Annotate");
    // Function identity — the callback rides the bus unchanged.
    expect(m.items[1]!.onSelect).toBe(onSelect);
    // The seam ctx: default copy item + selection summary + the 0-based surface index.
    expect(seenCtx!.items.map((i) => i.id)).toEqual(["copy"]);
    expect(seenCtx!.selection.kind).toBe("text");
    expect(seenCtx!.selection.collapsed).toBe(false);
    expect(seenCtx!.page).toBe(2);
  });

  it("ctx.items are defensive copies; an undefined return falls back to pristine defaults + one warn", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    try {
      const { kernel } = await bootKernel(1, {
        contextMenu: (c) => {
          c.items[0]!.label = "HACKED";
          return undefined as never;
        },
      });
      await selectSpan(kernel);
      const menus = captureMenus(kernel);
      await kernel.exec("selection.menuAt", { loc: { surface: 0 }, x: 20, y: 1, clientX: 1, clientY: 2 });
      expect(menus.at(-1)!.items).toEqual([{ id: "copy", label: "Copy", enabled: true }]);
      expect(warn).toHaveBeenCalledTimes(1);
      expect(warn.mock.calls[0]![0]).toContain("[parasma:headless]");
    } finally {
      warn.mockRestore();
    }
  });

  it("a THROWING seam falls back to defaults with exactly one warn; the menu still opens", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    try {
      const { kernel } = await bootKernel(1, {
        contextMenu: () => {
          throw new Error("host bug");
        },
      });
      await selectSpan(kernel);
      const menus = captureMenus(kernel);
      await kernel.exec("selection.menuAt", { loc: { surface: 0 }, x: 20, y: 1, clientX: 3, clientY: 4 });
      const m = menus.at(-1)!;
      expect(m.items).toEqual([{ id: "copy", label: "Copy", enabled: true }]);
      expect(m.clientX).toBe(3);
      expect(warn).toHaveBeenCalledTimes(1);
      expect(warn.mock.calls[0]![0]).toContain("[parasma:headless]");
    } finally {
      warn.mockRestore();
    }
  });

  it("invalid returns (non-array / malformed item / Promise) fall back to defaults + warn", async () => {
    const badSeams: Array<(c: { items: unknown[] }) => unknown> = [
      () => ({ not: "an array" }), // non-array
      () => [{ id: "x", label: "X" }], // missing `enabled`
      () => [{ id: "x", label: "X", enabled: true, onSelect: "nope" }], // onSelect not a fn
      () => Promise.resolve([]), // async seam mistake — a Promise is not an array
    ];
    for (const seam of badSeams) {
      const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
      try {
        const { kernel } = await bootKernel(1, { contextMenu: seam as never });
        await selectSpan(kernel);
        const menus = captureMenus(kernel);
        await kernel.exec("selection.menuAt", { loc: { surface: 0 }, x: 20, y: 1, clientX: 1, clientY: 1 });
        expect(menus.at(-1)!.items).toEqual([{ id: "copy", label: "Copy", enabled: true }]);
        expect(warn).toHaveBeenCalledTimes(1);
      } finally {
        warn.mockRestore();
      }
    }
  });

  it("a seam returning [] suppresses the menu (empty-items emit)", async () => {
    const { kernel } = await bootKernel(1, { contextMenu: () => [] });
    await selectSpan(kernel);
    const menus = captureMenus(kernel);
    await kernel.exec("selection.menuAt", { loc: { surface: 0 }, x: 20, y: 1, clientX: 1, clientY: 1 });
    expect(menus.at(-1)!.items).toEqual([]);
  });

  it("dismiss NEVER runs the seam: closeMenu + doc-switch emit {items:[]} seam-free", async () => {
    const seam = vi.fn((c: { items: { id: string; label: string; enabled: boolean }[] }) => c.items);
    const { kernel } = await bootKernel(1, { contextMenu: seam as never });
    await selectSpan(kernel);
    const menus = captureMenus(kernel);

    await kernel.exec("selection.menuAt", { loc: { surface: 0 }, x: 20, y: 1, clientX: 1, clientY: 1 });
    expect(seam).toHaveBeenCalledTimes(1);

    await kernel.exec("selection.closeMenu");
    expect(seam).toHaveBeenCalledTimes(1); // dismiss did not run the seam
    expect(menus.at(-1)!.items).toEqual([]);

    kernel.bus.emit("loader:status-changed", "loading"); // doc switch
    expect(seam).toHaveBeenCalledTimes(1); // still not run
    expect(menus.at(-1)!.items).toEqual([]);
  });

  it("ctx.selection.collapsed uses the shared collapsedOf: caret → true, span → false", async () => {
    const kinds: Array<{ kind: string; collapsed: boolean }> = [];
    const { kernel } = await bootKernel(1, {
      contextMenu: (c) => {
        kinds.push({ ...c.selection });
        return c.items;
      },
    });
    await selectSpan(kernel);
    // Inside the span → non-collapsed text.
    await kernel.exec("selection.menuAt", { loc: { surface: 0 }, x: 20, y: 1, clientX: 1, clientY: 1 });
    // Outside the span → re-anchor collapses to a caret at x=80.
    await kernel.exec("selection.menuAt", { loc: { surface: 0 }, x: 80, y: 1, clientX: 1, clientY: 1 });
    expect(kinds).toEqual([
      { kind: "text", collapsed: false },
      { kind: "text", collapsed: true },
    ]);
  });
});
