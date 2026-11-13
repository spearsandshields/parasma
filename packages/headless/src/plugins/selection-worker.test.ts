/**
 * Worker-mode plugin validation — NO live browser.
 *
 * The other plugin-selection + plugin-clipboard tests drive the IN-PROCESS
 * Viewer (selection.test / clipboard.test, `#remote === null`). This file drives
 * the SAME plugins against a Viewer whose `#remote` is a latency+FIFO
 * {@link MockRemoteTransport} routing to an in-process backend (the wasm-export
 * stand-in), so the whole stack runs through the worker seam:
 *
 *     plugin → viewer.hitTest/setSelection/extractRange (unified verbs) → #remote → mock → backend
 *
 * It pins the two worker-distinct behaviors:
 *
 *  - **Gesture staleness under out-of-order replies.** A newer
 *    pointer bumps the plugin's monotonic `gestureSeq`; an older hitTest whose
 *    RPC settles LATER (the mock's `flushReverse`) is DROPPED, and the winning
 *    `setSelection` notify reaches the backend (proving the fire-and-forget wire
 *    lands in the worker).
 *  - **Two-branch clipboard copy.** In worker mode text/selection copy works
 *    UNCONDITIONALLY (extractRange routes through `#remote`), while image copy
 *    CLEANLY SKIPS the png/data-URL: `objectInfo`/`imageOriginal`/`imageBitmapFor`
 *    return null WITHOUT an RPC (the bytes live on the main thread and are empty
 *    when the doc is in the worker) — text still copies, NO throw, NO empty blob.
 *
 * The Viewer is built with `Viewer.withTransport` (the DI seam; the
 * batteries-included `createWorkerBacked` spawns a real module Worker, absent in
 * vitest). Boot triggers no viewer RPC (no load is issued), and `selection.begin`
 * is inherently the WML text path, so the mock only serves the selection/copy
 * methods.
 */
import { describe, it, expect } from "vitest";
import { Viewer, type Transport, type TransportEvent, type DocFragment } from "@parasma/core";
import { createKernel } from "..";
import type { Plugin } from "../registry";
import { loaderPlugin } from "./loader";
import { viewportPlugin } from "./viewport";
import { selectionPlugin, type SelectionState } from "./selection";
import { clipboardPlugin, type CopyOutcome } from "./clipboard";

type MockMode = "sync" | "manual";

/** The wasm-export stand-in the transport dispatches into by method name. */
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

class Backend {
  readonly setSelectionCalls: RecordedSel[] = [];
  readonly hitTestCalls: number[][] = [];
  objectHitX: number | null = null;
  fragment: DocFragment = { blocks: [] };

  docHitTest(loc: { surface: number }, x: number, _y: number, _g: string): unknown {
    this.hitTestCalls.push([loc.surface, x]);
    return { kind: "text", surface: loc.surface, pos: { para: 0, byte: x } };
  }
  docHitTestObject(loc: { surface: number }, x: number, _y: number): unknown {
    if (this.objectHitX !== null && x === this.objectHitX) {
      return { surface: loc.surface, kind: "inlineImage", key: "rIdImg", rect: { x: 0, y: 0, w: 50, h: 16 } };
    }
    return null;
  }
  docSetSelection(sel: RecordedSel): void {
    this.setSelectionCalls.push(sel);
  }
  docSetSelectedObject(_obj: { surface: number; key: string } | null): void {}
  docExtractRange(_sel: unknown): DocFragment {
    return this.fragment;
  }
}

/** A Transport routing to {@link Backend}, structured-cloning both ways (as
 *  postMessage does). `manual` mode parks requests in {@link pending} for
 *  hand-ordered release (models a late / out-of-order worker reply). Notifies
 *  apply synchronously (fire-and-forget terminal writes). */
class MockRemoteTransport implements Transport {
  readonly pending: Array<() => void> = [];
  mode: MockMode = "sync";
  #backend: Record<string, ((...a: unknown[]) => unknown) | undefined>;

  constructor(backend: object) {
    this.#backend = backend as Record<string, ((...a: unknown[]) => unknown) | undefined>;
  }
  request(method: string, args: unknown[]): Promise<unknown> {
    const clonedArgs = args.map((a) => structuredClone(a));
    return new Promise((resolve, reject) => {
      const run = (): void => {
        try {
          const fn = this.#backend[method];
          if (typeof fn !== "function") throw new Error(`unknown worker method: ${method}`);
          resolve(structuredClone(fn.apply(this.#backend, clonedArgs)));
        } catch (e) {
          reject(e);
        }
      };
      if (this.mode === "manual") this.pending.push(run);
      else run();
    });
  }
  notify(method: string, args: unknown[]): void {
    const clonedArgs = args.map((a) => structuredClone(a));
    const fn = this.#backend[method];
    if (typeof fn === "function") fn.apply(this.#backend, clonedArgs);
  }
  /** Release parked requests newest-first (worst-case out-of-order delivery). */
  flushReverse(): void {
    while (this.pending.length) this.pending.pop()!();
  }
  onEvent(_h: (e: TransportEvent) => void): () => void {
    return () => {};
  }
  onDead(_h: (e: Error) => void): () => void {
    return () => {};
  }
  dispose(): void {}
}

function workerViewer(backend: Backend): { viewer: Viewer; mock: MockRemoteTransport } {
  const mock = new MockRemoteTransport(backend);
  const viewer = Viewer.withTransport(mock, {} as never);
  return { viewer, mock };
}

async function bootSelection() {
  const backend = new Backend();
  const { viewer, mock } = workerViewer(backend);
  const kernel = createKernel();
  kernel.use(loaderPlugin(viewer));
  kernel.use(viewportPlugin(viewer));
  kernel.use(selectionPlugin(viewer));
  await kernel.boot();
  backend.setSelectionCalls.length = 0;
  backend.hitTestCalls.length = 0;
  return { kernel, viewer, mock, backend };
}

describe("plugin-selection over #remote (worker mode)", () => {
  it("down → drag → up routes through #remote and setSelection reaches the worker", async () => {
    const { kernel, backend } = await bootSelection();

    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 5, y: 1 });
    await kernel.exec("selection.drag", { loc: { surface: 0 }, x: 15, y: 1 });
    await kernel.exec("selection.end");

    // The fire-and-forget setSelection notify landed in the backend (the worker).
    expect(backend.setSelectionCalls).toEqual([
      span(0, 5, 0, 5),
      span(0, 5, 0, 15),
    ]);
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind === "text" && sel.focus).toEqual({ para: 0, byte: 15 });
  });

  it("a stale hitTest whose RPC settles LATER is DROPPED (latest-wins)", async () => {
    const { kernel, mock, backend } = await bootSelection();

    // A live drag anchored at byte 5 (sync-resolved).
    await kernel.exec("selection.begin", { loc: { surface: 0 }, x: 5, y: 1 });

    // Two in-flight drags: the newer (x=30) issued AFTER the older (x=15). Both
    // bump gestureSeq synchronously before either RPC settles.
    mock.mode = "manual";
    const pOld = kernel.exec("selection.drag", { loc: { surface: 0 }, x: 15, y: 1 });
    const pNew = kernel.exec("selection.drag", { loc: { surface: 0 }, x: 30, y: 1 });
    expect(mock.pending.length).toBe(2); // both parked

    // Worst case: the OLDER reply arrives LAST (flushReverse resolves 30 then 15).
    mock.flushReverse();
    await Promise.all([pOld, pNew]);

    // The stale x=15 result was dropped: setSelection carries the NEWER focus only.
    expect(backend.setSelectionCalls).toEqual([
      span(0, 5, 0, 5), // begin (collapsed)
      span(0, 5, 0, 30), // newer drag wins
    ]);
    const sel = (await kernel.exec("selection.get")) as SelectionState;
    expect(sel.kind === "text" && sel.focus).toEqual({ para: 0, byte: 30 });
  });
});

// ── Two-branch clipboard copy in worker mode ─────────────────────────────────

const selectionStub: Plugin = { id: "selection", initialize() {}, destroy() {} };

/** A run carrying every run field (matches the wasm DocFragment schema). */
function textRun(text: string): Record<string, unknown> {
  return {
    text,
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
  };
}

/** A DocFragment with a text run AND an inline image run (relId → main-thread
 *  bytes that are unreachable in worker mode). */
const FRAG_WITH_IMAGE: DocFragment = {
  blocks: [
    {
      kind: "para",
      para: {
        runs: [
          textRun("Hello"),
          { ...textRun(""), image: { relId: "rId9", wPx: 100, hPx: 50 } },
        ],
        alignment: "start",
        dir: "ltr",
        list: null,
      },
    },
  ],
} as unknown as DocFragment;

async function bootClipboard(backend: Backend) {
  const { viewer, mock } = workerViewer(backend);
  const kernel = createKernel();
  kernel.use(selectionStub);
  kernel.use(clipboardPlugin(viewer));
  await kernel.boot();
  return { kernel, viewer, mock };
}

describe("plugin-clipboard over #remote (worker mode)", () => {
  it("worker TEXT copy works unconditionally; the inline image cleanly skips to <img alt> (no data-URL, no png)", async () => {
    const backend = new Backend();
    backend.fragment = FRAG_WITH_IMAGE;
    const { kernel } = await bootClipboard(backend);

    kernel.bus.emit("selection:changed", {
      kind: "text",
      collapsed: false,
      anchor: { para: 0, byte: 0 },
      focus: { para: 0, byte: 5 },
      seq: 1,
    });

    const out = (await kernel.exec("clipboard.copy")) as CopyOutcome | null;
    expect(out).not.toBeNull();
    expect(out?.kind).toBe("wml-text");
    // Text flavors copy — png is NOT one of them.
    expect(out?.mimes).toEqual(["text/plain", "text/html"]);
    expect(out?.mimes).not.toContain("image/png");
    // The inline image degraded to a placeholder (originals unreachable in worker
    // mode) — NO base64 data-URL leaked, and the text is intact.
    const html = out?.flavors["text/html"] as string;
    expect(html).toContain('<img alt="image"');
    expect(html).not.toContain("data:");
    expect(out?.flavors["text/plain"]).toBe("Hello");
  });

  it("worker IMAGE-OBJECT copy cleanly skips png (all flavors null → no throw, no empty blob)", async () => {
    const backend = new Backend();
    const { kernel } = await bootClipboard(backend);

    // A pure image-object selection: objectInfo/imageOriginal/imageBitmapFor all
    // return null in worker mode → png + data-URL html both cleanly skip.
    kernel.bus.emit("selection:changed", {
      kind: "object",
      collapsed: true,
      object: { page: 0, kind: "inlineImage", key: "rIdImg", index: 0 },
      seq: 2,
    });

    let out: CopyOutcome | null = null;
    // No throw — the copy resolves to null (nothing copyable in worker mode),
    // which the adapter reports as clipboard:error rather than writing an empty blob.
    await expect(
      (async () => {
        out = (await kernel.exec("clipboard.copy")) as CopyOutcome | null;
      })(),
    ).resolves.toBeUndefined();
    expect(out).toBeNull();
  });
});
