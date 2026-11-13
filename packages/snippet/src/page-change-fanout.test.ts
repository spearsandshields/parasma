/**
 * THE BYO ACCEPTANCE TEST (kernel-truth current page): a floating nav WITHOUT
 * the toolbar. A real kernel (createKernel + loaderPlugin(FakeWasm viewer) +
 * viewportPlugin) wired through the PRODUCTION wirePageChangeFanout delivers the
 * public 1-based `pageChange` to a plain emit spy — proving the event no longer
 * depends on any chrome. NO toolbar module is imported anywhere in this file.
 */
import { describe, it, expect, vi } from "vitest";
import { Viewer } from "@parasma/core";
import { createKernel, loaderPlugin, viewportPlugin } from "@parasma/headless";
import { wirePageChangeFanout } from "./page-change-fanout";

/** Mirrors the headless viewport.test.ts FakeWasm: streams N pageReady + total. */
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

describe("wirePageChangeFanout (BYO floating-nav-without-toolbar)", () => {
  it("delivers 1-based pageChange for kernel page edges with NO toolbar mounted", async () => {
    const kernel = createKernel();
    const viewer = makeViewer(new FakeWasm(5));
    kernel.use(loaderPlugin(viewer));
    kernel.use(viewportPlugin(viewer));
    await kernel.boot();

    const emitSpy = vi.fn();
    const unwire = wirePageChangeFanout(kernel, emitSpy);

    await kernel.exec("loader.open", new Uint8Array([1]));

    await kernel.exec("viewport.setCurrentPage", 3);
    expect(emitSpy).toHaveBeenLastCalledWith("pageChange", 4);

    await kernel.exec("viewport.goToPage", 1);
    expect(emitSpy).toHaveBeenLastCalledWith("pageChange", 2);
    expect(emitSpy).toHaveBeenCalledTimes(2);

    // The returned unsubscribe detaches the fan-out.
    unwire();
    await kernel.exec("viewport.setCurrentPage", 0);
    expect(emitSpy).toHaveBeenCalledTimes(2);
  });
});
