/**
 * plugin-comments tests.
 *
 * plugin-comments owns the Word COMMENT MODEL (comments[] from viewer.comments())
 * + the focused-comment id + the highlight HIT-TEST (comments.at), and emits a
 * `comments:changed` bus event ({comments, focused}). It is a `@xstate/store`
 * slice (NOT a machine), deps:["viewport"], and builds on the bridged loadComplete
 * (the post-layout seam) from viewer.comments() — which is [] for non-word /
 * comment-free docs, so no documentKind gate is needed (present-only falls out of
 * the empty event).
 *
 * We boot it alongside the REAL loaderPlugin + viewportPlugin against the same
 * FakeWasm/Viewer injection trick the sheets/viewport tests use. Comments live on
 * the docx path (a plain non-xlsx blob → core routes to #doLoadDocx). The
 * FakeWasm exposes comments() + commentRects() so the plugin reads the live model.
 */
import { describe, it, expect } from "vitest";
import { Viewer, type Comment, type CommentRect } from "@parasma/core";
import { createKernel } from "..";
import { loaderPlugin } from "./loader";
import { viewportPlugin } from "./viewport";
import { commentsPlugin } from "./comments";

type CommentsChanged = { comments: Comment[]; focused: number | null };

/**
 * FakeWasm mirroring the sheets-test fake, plus the comment surface
 * plugin-comments reads: comments() + commentRects(idx). isWord gates them so a
 * non-word (xlsx) load reports no comments (core contract parity).
 */
class FakeWasm {
  private isWord = true;
  constructor(
    private readonly pages: number,
    private readonly cmts: Comment[] = [],
    private readonly rects: Record<number, CommentRect[]> = {},
  ) {}
  loadDocx(_bytes: Uint8Array): void {
    this.isWord = true;
  }
  loadXlsx(_bytes: Uint8Array): void {
    this.isWord = false;
  }
  loadPptx(_bytes: Uint8Array): void {
    this.isWord = false;
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
  sheetInfo(): unknown[] {
    return [];
  }
  comments(): Comment[] {
    return this.isWord ? this.cmts : [];
  }
  commentRects(idx: number): CommentRect[] {
    return this.isWord ? (this.rects[idx] ?? []) : [];
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

const COMMENTS: Comment[] = [
  { id: 1, author: "Alice", date: "2026-01-01", initials: "A", text: "first", page: 0, anchorY: 100, color: "#aabbcc" },
  { id: 2, author: "Bob", date: null, initials: "B", text: "second", page: 1, anchorY: 200, color: "#ddee00" },
];

// Two rects on page 0: a plain rect (id 1) and an overlap rect (ids 1+2 stacked).
const RECTS: Record<number, CommentRect[]> = {
  0: [
    { x: 10, y: 10, w: 20, h: 20, ids: [1], color: "#aabbcc" },
    { x: 50, y: 50, w: 30, h: 30, ids: [1, 2], color: "#aabbcc" },
  ],
};

async function bootKernel(pages: number, cmts: Comment[] = [], rects: Record<number, CommentRect[]> = {}) {
  const wasm = new FakeWasm(pages, cmts, rects);
  const viewer = makeViewer(wasm);
  const kernel = createKernel();
  kernel.use(loaderPlugin(viewer));
  kernel.use(viewportPlugin(viewer));
  kernel.use(commentsPlugin(viewer));
  await kernel.boot();
  return { kernel, viewer };
}

describe("plugin-comments", () => {
  it("builds the comment model from viewer.comments() on loadComplete (word)", async () => {
    const { kernel } = await bootKernel(2, COMMENTS, RECTS);
    await kernel.exec("loader.open", docxBytes());

    const list = (await kernel.exec("comments.list")) as Comment[];
    expect(list).toEqual(COMMENTS);
  });

  it("emits 'comments:changed' with the built set", async () => {
    const { kernel } = await bootKernel(2, COMMENTS, RECTS);
    await kernel.exec("loader.open", docxBytes());

    const events: CommentsChanged[] = [];
    kernel.bus.subscribe("comments:changed", (e) => events.push(e));
    // Subscribing replays the latest (the build event).
    expect(events.at(-1)?.comments).toEqual(COMMENTS);
    expect(events.at(-1)?.focused).toBeNull();
  });

  it("comments.at returns the distinct ids over a commentRect", async () => {
    const { kernel } = await bootKernel(2, COMMENTS, RECTS);
    await kernel.exec("loader.open", docxBytes());

    // Inside the plain rect → [1].
    expect(await kernel.exec("comments.at", { page: 0, x: 15, y: 15 })).toEqual([1]);
    // Inside the overlap rect → distinct [1,2].
    expect(await kernel.exec("comments.at", { page: 0, x: 60, y: 60 })).toEqual([1, 2]);
    // A miss → [].
    expect(await kernel.exec("comments.at", { page: 0, x: 500, y: 500 })).toEqual([]);
    // A page with no rects → [].
    expect(await kernel.exec("comments.at", { page: 1, x: 10, y: 10 })).toEqual([]);
  });

  it("comments.at returns [] for a non-word document", async () => {
    const { kernel } = await bootKernel(2, COMMENTS, RECTS);
    await kernel.exec("loader.open", xlsxBytes());

    expect(await kernel.exec("comments.list")).toEqual([]);
    expect(await kernel.exec("comments.at", { page: 0, x: 15, y: 15 })).toEqual([]);
  });

  it("comments.focus sets focused and emits 'comments:changed' with the focused id", async () => {
    const { kernel } = await bootKernel(2, COMMENTS, RECTS);
    await kernel.exec("loader.open", docxBytes());
    expect(await kernel.exec("comments.focused")).toBeNull();

    const events: CommentsChanged[] = [];
    kernel.bus.subscribe("comments:changed", (e) => events.push(e));

    await kernel.exec("comments.focus", 2);
    expect(await kernel.exec("comments.focused")).toBe(2);
    expect(events.at(-1)?.focused).toBe(2);
    expect(events.at(-1)?.comments).toEqual(COMMENTS);
  });

  it("'comments:changed' is empty for a non-word document (present-only)", async () => {
    const { kernel } = await bootKernel(2, COMMENTS, RECTS);
    await kernel.exec("loader.open", xlsxBytes());

    const events: CommentsChanged[] = [];
    kernel.bus.subscribe("comments:changed", (e) => events.push(e));
    expect(events.at(-1)?.comments).toEqual([]);
  });

  it("resets the slice on a doc-switch loader status (loading)", async () => {
    const { kernel, viewer } = await bootKernel(2, COMMENTS, RECTS);
    await kernel.exec("loader.open", docxBytes());
    await kernel.exec("comments.focus", 1);
    expect((await kernel.exec("comments.list") as Comment[]).length).toBe(2);

    const events: CommentsChanged[] = [];
    kernel.bus.subscribe("comments:changed", (e) => events.push(e));

    // A new load drives loader → loading (status-changed) → plugin-comments resets.
    const p = kernel.exec("loader.open", docxBytes());
    expect((await kernel.exec("comments.list") as Comment[]).length).toBe(0);
    expect(await kernel.exec("comments.focused")).toBeNull();
    // The reset emitted an empty set.
    expect(events.some((e) => e.comments.length === 0)).toBe(true);
    await p;
    void viewer;
  });

  it("depends on viewport (boots after it)", async () => {
    const wasm = new FakeWasm(1);
    const viewer = makeViewer(wasm);
    const kernel = createKernel();
    kernel.use(loaderPlugin(viewer));
    kernel.use(commentsPlugin(viewer)); // missing viewport
    await expect(kernel.boot()).rejects.toThrow(/unknown plugin: viewport/);
  });
});
