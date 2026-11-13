/**
 * plugin-selection + plugin-clipboard — PML arm. The ONE
 * command family drives a slide through the unified `viewer.hitTestObject`
 * (object first) → `viewer.hitTest` (the TARGET on the hit — not a command
 * name — selects the PML semantics; a drag re-resolves on the ANCHOR slide with `loc.clamp`) → `viewer.setSelection` /
 * `viewer.setSelectedObject` (fire-and-forget), emits `selection:changed`
 * {kind:"pmlText"|"object"}, and is mode-gated. A background MISS on a
 * slide CLEARS (PowerPoint convention — the one `documentKind` semantic, so
 * boot() actually loads a pptx). The clipboard reuses the WML text flavors on
 * the unified `viewer.extractRange`. Booted over a FakeWasm so the REAL Viewer
 * wrappers are exercised.
 */
import { describe, it, expect } from "vitest";
import { Viewer } from "@parasma/core";
import { createKernel } from "..";
import { loaderPlugin } from "./loader";
import { viewportPlugin } from "./viewport";
import { selectionPlugin, type SelectionState } from "./selection";
import { clipboardPlugin, type CopyOutcome } from "./clipboard";

type PmlChanged = {
  kind: "none" | "text" | "object" | "cells" | "pmlText";
  collapsed: boolean;
  object?: { page: number; kind: string; key: string; index: number };
  pml?: {
    slide: number;
    targetKind: number;
    targetA: number;
    targetB: number;
    anchor: { para: number; byte: number };
    focus: { para: number; byte: number };
  };
  seq: number;
};

/** A recorded pmlText DocSelection (`null` = the unified clear). */
type RecordedSel = {
  kind: string;
  slide: number;
  target: { targetKind: number; targetA: number; targetB: number };
  start: { para: number; byte: number };
  end: { para: number; byte: number };
} | null;

/** Shorthand for the expected recorded pmlText selection. */
const pmlSpan = (
  slide: number,
  target: [number, number, number],
  sp: number,
  sb: number,
  ep: number,
  eb: number,
): RecordedSel => ({
  kind: "pmlText",
  slide,
  target: { targetKind: target[0], targetA: target[1], targetB: target[2] },
  start: { para: sp, byte: sb },
  end: { para: ep, byte: eb },
});

/**
 * FakeWasm: the loader/viewport boot surface + the unified selection exports.
 * `docHitTestObject` reports a picture when `x >= 500` (the "picture zone").
 * `docHitTest` returns a text hit in shape 2 (para 0, byte = ⌊x⌋) carrying the
 * PML target; a `loc.clamp` echoes that target; a negative x is a background
 * MISS. `docExtractRange` returns a one-run "Hello" paragraph fragment.
 */
class FakeWasm {
  readonly setSelCalls: RecordedSel[] = [];
  readonly setObjCalls: Array<{ surface: number; key: string } | null> = [];

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
    return { widthPx: 960, heightPx: 540 };
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

  // ── unified selection exports (the PML arms) ─────────────────────────────
  docHitTestObject(loc: { surface: number }, x: number, _y: number): unknown {
    if (x >= 500) return { surface: loc.surface, kind: "picture", key: "img1", rect: { x: 500, y: 10, w: 80, h: 60 } };
    return null;
  }
  docHitTest(
    loc: { surface: number; clamp?: { targetKind: number; targetA: number; targetB: number } },
    x: number,
    _y: number,
    _g: string,
  ): unknown {
    if (x < 0) return null; // background miss
    const target = loc.clamp ?? { targetKind: 0, targetA: 2, targetB: 0 };
    return { kind: "text", surface: loc.surface, target, pos: { para: 0, byte: Math.floor(x) } };
  }
  docSetSelection(sel: RecordedSel): void {
    this.setSelCalls.push(sel);
  }
  docSetSelectedObject(obj: { surface: number; key: string } | null): void {
    this.setObjCalls.push(obj);
  }
  docExtractRange(_sel: unknown): unknown {
    return {
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
                fontFamily: "Arial",
                fontSizePt: 18,
                color: "#000000",
                highlight: null,
                runShading: null,
                vertAlign: "baseline",
                link: null,
                image: null,
                footnoteMark: null,
              },
            ],
            alignment: "left",
            dir: "ltr",
            list: null,
          },
        },
      ],
    };
  }
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

// A ZIP header + the pptx part-name needle → the load sniff classifies as PPT
// (the miss-clears semantic reads `viewer.documentKind === "ppt"`).
const pptxBytes = (): Uint8Array =>
  new Uint8Array([0x50, 0x4b, 0x03, 0x04, ...new TextEncoder().encode("ppt/presentation.xml")]);

async function boot() {
  const wasm = new FakeWasm();
  const viewer = makeViewer(wasm);
  const kernel = createKernel();
  kernel.use(loaderPlugin(viewer));
  kernel.use(viewportPlugin(viewer));
  kernel.use(selectionPlugin(viewer));
  kernel.use(clipboardPlugin(viewer));
  await kernel.boot();
  // Load a (fake) pptx so `viewer.documentKind` reports "ppt" — the slide
  // background-miss-clears semantic keys off it.
  await viewer.load(pptxBytes());
  wasm.setSelCalls.length = 0;
  wasm.setObjCalls.length = 0;
  const changed: PmlChanged[] = [];
  kernel.bus.subscribe("selection:changed", (e) => changed.push(e as PmlChanged));
  return { kernel, viewer, wasm, changed };
}

const sel = (k: Awaited<ReturnType<typeof boot>>["kernel"]): Promise<SelectionState> =>
  k.exec("selection.get") as Promise<SelectionState>;

describe("plugin-selection — PML text arm (anchor clamp)", () => {
  it("begin on slide text anchors the topmost shape/cell", async () => {
    const { kernel, wasm, changed } = await boot();
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 100, y: 50 });
    const s = await sel(kernel);
    expect(s.kind).toBe("pmlText");
    if (s.kind === "pmlText") {
      expect(s.target).toEqual({ targetKind: 0, targetA: 2, targetB: 0 });
      expect(s.anchor).toEqual({ para: 0, byte: 100 });
      expect(s.focus).toEqual({ para: 0, byte: 100 });
      expect(s.dragging).toBe(true);
    }
    // Engine got a collapsed pmlText set (slide + target + range).
    expect(wasm.setSelCalls.at(-1)).toEqual(pmlSpan(0, [0, 2, 0], 0, 100, 0, 100));
    expect(changed.at(-1)?.kind).toBe("pmlText");
  });

  it("drag clamps to the anchor target and extends the focus", async () => {
    const { kernel, wasm } = await boot();
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 100, y: 50 });
    // Drag far to a different slide index — the plugin resolves on the ANCHOR
    // slide (0) with the anchor target as the clamp, so the target never changes.
    await kernel.exec("selection.drag", { loc: { surface: 9 }, x: 260, y: 50 });
    const s = await sel(kernel);
    expect(s.kind).toBe("pmlText");
    if (s.kind === "pmlText") {
      expect(s.target).toEqual({ targetKind: 0, targetA: 2, targetB: 0 });
      expect(s.focus).toEqual({ para: 0, byte: 260 });
    }
    // The set uses the anchor slide (0) + anchor target, anchor 100 → focus 260.
    expect(wasm.setSelCalls.at(-1)).toEqual(pmlSpan(0, [0, 2, 0], 0, 100, 0, 260));
  });

  it("end ends the drag but keeps the span", async () => {
    const { kernel } = await boot();
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 100, y: 50 });
    await kernel.exec("selection.end");
    const s = await sel(kernel);
    expect(s.kind === "pmlText" && s.dragging).toBe(false);
  });

  it("begin on a picture selects the OBJECT (object-first z-order)", async () => {
    const { kernel, wasm, changed } = await boot();
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 600, y: 40 });
    const s = await sel(kernel);
    expect(s.kind).toBe("object");
    if (s.kind === "object") {
      expect(s.ref).toEqual({ page: 0, kind: "picture", key: "img1", index: 0 });
    }
    // The unified object seam carried the slide + key.
    expect(wasm.setObjCalls.at(-1)).toEqual({ surface: 0, key: "img1" });
    expect(changed.at(-1)?.kind).toBe("object");
  });

  it("begin on empty slide background clears the selection (ppt convention)", async () => {
    const { kernel } = await boot();
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 100, y: 50 });
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: -5, y: 50 });
    expect((await sel(kernel)).kind).toBe("none");
  });

  it("is inert in hand mode", async () => {
    const { kernel, wasm } = await boot();
    kernel.bus.emit("tool:mode-changed", { mode: "hand" });
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 100, y: 50 });
    expect((await sel(kernel)).kind).toBe("none");
    expect(wasm.setSelCalls.length).toBe(0);
  });
});

describe("plugin-clipboard — PML text copy (reuses the WML flavors)", () => {
  it("copies a PML text span as text/plain + text/html", async () => {
    const { kernel } = await boot();
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 0, y: 50 });
    await kernel.exec("selection.drag", { loc: { surface: 0 }, x: 200, y: 50 }); // non-collapsed
    const outcome = (await kernel.exec("clipboard.copy")) as CopyOutcome | null;
    expect(outcome).not.toBeNull();
    expect(outcome?.kind).toBe("pml-text");
    expect(outcome?.flavors["text/plain"]).toContain("Hello");
    expect(String(outcome?.flavors["text/html"])).toContain("Hello");
  });
});
