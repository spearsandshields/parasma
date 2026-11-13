/**
 * plugin-viewport tests.
 *
 * plugin-viewport owns the per-page MODEL (page count + per-page size) and the
 * VISIBLE-WINDOW state, and emits `{ tag: 'page', pageIndex }` paint intents. It
 * is a `@xstate/store` slice (NOT a machine), deps:["loader"], and consumes the
 * loader-bridged pageReady/loadComplete off the replayable kernel bus.
 *
 * We boot it alongside the real loaderPlugin against the SAME FakeWasm/Viewer
 * injection trick the loader tests use, so the page lifecycle flows exactly as in
 * production: loader bridges viewer events → bus → viewport.
 */
import { describe, it, expect } from "vitest";
import { Viewer } from "@parasma/core";
import { createKernel, type PaintIntent } from "..";
import { loaderPlugin } from "./loader";
import { viewportPlugin } from "./viewport";

/**
 * FakeWasm whose `runLayout` emits a configurable burst of pageReady indices and
 * then resolves with the total — mirrors core's streaming layout so the loader's
 * bridge feeds real pageReady/loadComplete onto the bus.
 */
class FakeWasm {
  constructor(private readonly pages: number) {}
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
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

async function bootKernel(pages: number) {
  const wasm = new FakeWasm(pages);
  const viewer = makeViewer(wasm);
  const kernel = createKernel();
  kernel.use(loaderPlugin(viewer));
  kernel.use(viewportPlugin(viewer));
  await kernel.boot();
  return { kernel, viewer };
}

describe("plugin-viewport", () => {
  it("builds the page model from replayed pageReady (count = max idx+1, size captured)", async () => {
    const { kernel } = await bootKernel(3);
    await kernel.exec("loader.open", new Uint8Array([1]));

    const model = (await kernel.exec("viewport.pageModel")) as {
      pageCount: number;
      pageSize: { widthPx: number; heightPx: number } | null;
      loadComplete: boolean;
    };
    expect(model.pageCount).toBe(3);
    expect(model.pageSize).toEqual({ widthPx: 612, heightPx: 792 });
    expect(model.loadComplete).toBe(true);
  });

  it("loadComplete sets the final count + loadComplete flag", async () => {
    const { kernel } = await bootKernel(5);
    await kernel.exec("loader.open", new Uint8Array([1]));
    const model = (await kernel.exec("viewport.pageModel")) as { pageCount: number; loadComplete: boolean };
    expect(model.pageCount).toBe(5);
    expect(model.loadComplete).toBe(true);
  });

  it("emits one 'page' paint intent per pageReady", async () => {
    const { kernel } = await bootKernel(4);
    const intents: PaintIntent[] = [];
    kernel.paint.onPaintIntent((i) => intents.push(i));
    await kernel.exec("loader.open", new Uint8Array([1]));

    const pageIntents = intents.filter((i) => i.tag === "page");
    expect(pageIntents.map((i) => (i as { pageIndex: number }).pageIndex)).toEqual([0, 1, 2, 3]);
  });

  it("setVisible updates the window and emits a 'page' intent only for newly-visible indices", async () => {
    const { kernel } = await bootKernel(6);
    await kernel.exec("loader.open", new Uint8Array([1]));

    const intents: PaintIntent[] = [];
    kernel.paint.onPaintIntent((i) => intents.push(i));

    await kernel.exec("viewport.setVisible", [0, 1, 2]);
    expect(intents.map((i) => (i as { pageIndex: number }).pageIndex)).toEqual([0, 1, 2]);

    // Re-setVisible with an overlap: only the NEW index (3) emits an intent.
    intents.length = 0;
    await kernel.exec("viewport.setVisible", [1, 2, 3]);
    expect(intents.map((i) => (i as { pageIndex: number }).pageIndex)).toEqual([3]);
  });

  it("imageReady emits exactly one 'page-invalidate' intent per visible page", async () => {
    const { kernel } = await bootKernel(4);
    await kernel.exec("loader.open", new Uint8Array([1]));
    await kernel.exec("viewport.setVisible", [1, 2]);

    const intents: PaintIntent[] = [];
    kernel.paint.onPaintIntent((i) => intents.push(i));

    // The loader bridges viewer's `imageReady` onto the kernel bus; emit the
    // bridged event directly (the FakeWasm has no image pipeline).
    kernel.bus.emit("imageReady", "rId7");

    const invalidates = intents.filter((i) => i.tag === "page-invalidate");
    expect(invalidates.map((i) => (i as { pageIndex: number }).pageIndex)).toEqual([1, 2]);
    // EXACTLY one per visible page — the duplicate-listener bug class (one
    // invalidate per image, never doubled by a second subscription).
    expect(intents.length).toBe(invalidates.length);
  });

  it("imageReady with nothing visible emits no intents", async () => {
    const { kernel } = await bootKernel(2);
    await kernel.exec("loader.open", new Uint8Array([1]));

    const intents: PaintIntent[] = [];
    kernel.paint.onPaintIntent((i) => intents.push(i));
    kernel.bus.emit("imageReady", "rId1");
    expect(intents).toEqual([]);
  });

  it("reset clears the slice (doc switch parity)", async () => {
    const { kernel } = await bootKernel(3);
    await kernel.exec("loader.open", new Uint8Array([1]));
    await kernel.exec("viewport.setVisible", [0, 1]);

    await kernel.exec("viewport.reset");
    const model = (await kernel.exec("viewport.pageModel")) as {
      pageCount: number;
      pageSize: unknown;
      loadComplete: boolean;
    };
    expect(model.pageCount).toBe(0);
    expect(model.pageSize).toBeNull();
    expect(model.loadComplete).toBe(false);
  });

  it("depends on loader (boots after the bridge)", async () => {
    // viewportPlugin without loaderPlugin must fail to boot (unknown dep).
    const wasm = new FakeWasm(1);
    const viewer = makeViewer(wasm);
    const kernel = createKernel();
    kernel.use(viewportPlugin(viewer));
    await expect(kernel.boot()).rejects.toThrow(/unknown plugin: loader/);
  });
});

describe("plugin-viewport current page (kernel-truth model)", () => {
  async function pageModel(kernel: Awaited<ReturnType<typeof bootKernel>>["kernel"]) {
    return (await kernel.exec("viewport.pageModel")) as { currentPage: number; pageCount: number };
  }

  it("setCurrentPage updates pageModel().currentPage and emits page-changed exactly once per CHANGE", async () => {
    const { kernel } = await bootKernel(5);
    await kernel.exec("loader.open", new Uint8Array([1]));
    const seen: number[] = [];
    kernel.bus.subscribe("viewport:page-changed", ({ index }) => seen.push(index));

    await kernel.exec("viewport.setCurrentPage", 2);
    expect((await pageModel(kernel)).currentPage).toBe(2);
    expect(seen).toEqual([2]);

    // Repeat exec to the same index: deduped — NO extra emit.
    await kernel.exec("viewport.setCurrentPage", 2);
    expect(seen).toEqual([2]);
  });

  it("clamps: 99 on a 3-page doc → 2; −1 → 0; NaN ignored", async () => {
    const { kernel } = await bootKernel(3);
    await kernel.exec("loader.open", new Uint8Array([1]));
    const seen: number[] = [];
    kernel.bus.subscribe("viewport:page-changed", ({ index }) => seen.push(index));

    await kernel.exec("viewport.setCurrentPage", 99);
    expect((await pageModel(kernel)).currentPage).toBe(2);
    expect(seen).toEqual([2]);

    await kernel.exec("viewport.setCurrentPage", -1);
    expect((await pageModel(kernel)).currentPage).toBe(0);
    expect(seen).toEqual([2, 0]);

    await kernel.exec("viewport.setCurrentPage", NaN);
    expect((await pageModel(kernel)).currentPage).toBe(0);
    expect(seen).toEqual([2, 0]);
  });

  it("goToPage emits BOTH events on change; repeat goToPage emits ONLY scroll-to-page", async () => {
    const { kernel } = await bootKernel(3);
    await kernel.exec("loader.open", new Uint8Array([1]));
    const changed: number[] = [];
    const scrolls: number[] = [];
    kernel.bus.subscribe("viewport:page-changed", ({ index }) => changed.push(index));
    kernel.bus.subscribe("viewport:scroll-to-page", ({ index }) => scrolls.push(index));

    await kernel.exec("viewport.goToPage", 1);
    expect(changed).toEqual([1]);
    expect(scrolls).toEqual([1]);

    // Same target: still scrolls (re-align page top) but no model edge.
    await kernel.exec("viewport.goToPage", 1);
    expect(changed).toEqual([1]);
    expect(scrolls).toEqual([1, 1]);
  });

  it("replay: late page-changed subscriber gets the latest index; late scroll-to-page gets nothing", async () => {
    const { kernel } = await bootKernel(5);
    await kernel.exec("loader.open", new Uint8Array([1]));
    await kernel.exec("viewport.setCurrentPage", 3);
    await kernel.exec("viewport.goToPage", 2);

    const changed: number[] = [];
    const scrolls: number[] = [];
    kernel.bus.subscribe("viewport:page-changed", ({ index }) => changed.push(index));
    kernel.bus.subscribe("viewport:scroll-to-page", ({ index }) => scrolls.push(index));
    expect(changed).toEqual([2]); // "latest" only
    expect(scrolls).toEqual([]); // transient
  });

  it("viewport.reset → currentPage 0 AND a fresh subscriber gets NO replay (doc B must not see doc A's page)", async () => {
    const { kernel } = await bootKernel(5);
    await kernel.exec("loader.open", new Uint8Array([1]));
    await kernel.exec("viewport.setCurrentPage", 3);

    await kernel.exec("viewport.reset");
    expect((await pageModel(kernel)).currentPage).toBe(0);

    const changed: number[] = [];
    kernel.bus.subscribe("viewport:page-changed", ({ index }) => changed.push(index));
    expect(changed).toEqual([]);
  });

  it("a CLAMPING loadComplete EMITS page-changed with the clamped index (mirror-deadlock pin); replay holds it", async () => {
    const { kernel } = await bootKernel(10);
    await kernel.exec("loader.open", new Uint8Array([1]));
    await kernel.exec("viewport.setCurrentPage", 9);

    const changed: number[] = [];
    kernel.bus.subscribe("viewport:page-changed", ({ index }) => changed.push(index));
    expect(changed).toEqual([9]); // sticky replay of the set

    // The document completes SHORTER than the sensed page (streaming shrink):
    // the clamp must EMIT — a silent clamp deadlocks the consumer mirror (it
    // updates only from the bus echo, and the plugin dedups the next exec).
    kernel.bus.emit("loadComplete", 4);
    expect((await pageModel(kernel)).currentPage).toBe(3);
    expect(changed).toEqual([9, 3]);

    // A LATE subscriber replays the CLAMPED index, not the pre-clamp one.
    const late: number[] = [];
    kernel.bus.subscribe("viewport:page-changed", ({ index }) => late.push(index));
    expect(late).toEqual([3]);
  });

  it("a NON-clamping loadComplete emits nothing extra", async () => {
    const { kernel } = await bootKernel(5);
    await kernel.exec("loader.open", new Uint8Array([1]));
    const changed: number[] = [];
    kernel.bus.subscribe("viewport:page-changed", ({ index }) => changed.push(index));

    kernel.bus.emit("loadComplete", 5);
    expect(changed).toEqual([]);
  });
});
