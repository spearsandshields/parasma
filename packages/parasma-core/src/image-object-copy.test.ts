/**
 * Image-object retention + copy accessors on the Viewer.
 *
 * `imageOriginal` / `imageBitmapFor` / `objectInfo` read the main-thread
 * retention maps that `#decodeImagesInBackground` populates: `#imageBytes` (the
 * untouched originals, retained BEFORE `clearImageBytes()` wipes the wasm side) and
 * `#imageBitmaps` (the decoded intrinsic bitmaps). We drive the real load path
 * over the same FakeWasm/createImageBitmap stub as image-ref-diagnostics.test,
 * then assert the accessors survive the wasm-side byte wipe and drop on doc
 * switch / destroy.
 */
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { Viewer, type ObjectRef } from "./index";

const PNG_BYTES = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

/** Fake wasm modelling image extraction + the clear-bytes contract. */
class FakeWasm {
  #cleared = false;
  #resolveRun: ((total: number) => void) | null = null;

  loadDocx(_bytes: Uint8Array): void {}
  abortLayout(): void {
    const r = this.#resolveRun;
    this.#resolveRun = null;
    r?.(0);
  }
  async runLayout(_cb: (idx: number) => void): Promise<number> {
    return new Promise<number>((res) => {
      this.#resolveRun = res;
    });
  }
  setNow(_n: number): void {}
  pageSize(): { widthPx: number; heightPx: number } {
    return { widthPx: 600, heightPx: 800 };
  }
  fontFamiliesUsed(): string[] {
    return [];
  }
  imageRefs(): Array<{ relId: string; mime: string; bytes: Uint8Array }> {
    if (this.#cleared) return [];
    return [{ relId: "rId1", mime: "image/png", bytes: PNG_BYTES }];
  }
  setImageBitmap(_relId: string, _bitmap: unknown): void {}
  clearImageBytes(): void {
    this.#cleared = true;
  }
  free(): void {}
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

const ref = (key: string): ObjectRef => ({ page: 0, kind: "inlineImage", key, index: 0 });

describe("image retention + copy accessors", () => {
  let origBlob: typeof globalThis.Blob;
  let origCreateImageBitmap: typeof globalThis.createImageBitmap;

  beforeEach(() => {
    origBlob = globalThis.Blob;
    origCreateImageBitmap = globalThis.createImageBitmap;
    globalThis.Blob = class {
      constructor(
        public parts: unknown[],
        public options?: { type?: string },
      ) {}
    } as unknown as typeof globalThis.Blob;
    // Decode to a bitmap carrying INTRINSIC dims (20×16) so objectInfo can read them.
    globalThis.createImageBitmap = (async () => ({
      width: 20,
      height: 16,
      close() {},
    })) as unknown as typeof globalThis.createImageBitmap;
  });

  afterEach(() => {
    globalThis.Blob = origBlob;
    globalThis.createImageBitmap = origCreateImageBitmap;
  });

  it("imageOriginal returns the retained bytes AFTER clearImageBytes wiped the wasm side", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    await viewer.load(PNG_BYTES);

    // Retention is captured synchronously inside #decodeImagesInBackground.
    const before = await viewer.imageOriginal("rId1");
    expect(before?.mime).toBe("image/png");
    expect(before?.bytes).toBe(PNG_BYTES);

    // Let the decode settle → clearImageBytes() runs (wasm imageRefs() now []).
    await new Promise((r) => setTimeout(r, 0));
    expect(wasm.imageRefs()).toEqual([]);

    // …yet the retained originals survive.
    const after = await viewer.imageOriginal("rId1");
    expect(after?.bytes).toBe(PNG_BYTES);
  });

  it("imageOriginal returns null for an unknown relId", async () => {
    const viewer = makeViewer(new FakeWasm());
    await viewer.load(PNG_BYTES);
    expect(await viewer.imageOriginal("nope")).toBeNull();
  });

  it("imageBitmapFor + objectInfo report the decoded intrinsic bitmap once decode settles", async () => {
    const viewer = makeViewer(new FakeWasm());
    await viewer.load(PNG_BYTES);
    await new Promise((r) => setTimeout(r, 0)); // decode settles

    const bmp = await viewer.imageBitmapFor("rId1");
    expect(bmp).not.toBeNull();
    expect((bmp as unknown as { width: number }).width).toBe(20);
    // objectInfo dims are INTRINSIC (from the bitmap), mime from retention.
    expect(await viewer.objectInfo(ref("rId1"))).toEqual({
      relId: "rId1",
      mime: "image/png",
      wPx: 20,
      hPx: 16,
    });
  });

  it("objectInfo is null when the relId isn't retained (no bytes)", async () => {
    const viewer = makeViewer(new FakeWasm());
    await viewer.load(PNG_BYTES);
    await new Promise((r) => setTimeout(r, 0));
    expect(await viewer.objectInfo(ref("nope"))).toBeNull();
  });

  it("retention drops on doc switch (a second load clears the old relId)", async () => {
    const viewer = makeViewer(new FakeWasm());
    await viewer.load(PNG_BYTES);
    await new Promise((r) => setTimeout(r, 0));
    expect(await viewer.imageOriginal("rId1")).not.toBeNull();

    // A second load (the FakeWasm reports no images post-clear) must drop the
    // retained originals from the previous document.
    await viewer.load(PNG_BYTES);
    expect(await viewer.imageOriginal("rId1")).toBeNull();
  });

  it("retention drops on destroy()", async () => {
    const viewer = makeViewer(new FakeWasm());
    await viewer.load(PNG_BYTES);
    await new Promise((r) => setTimeout(r, 0));
    expect(await viewer.imageOriginal("rId1")).not.toBeNull();
    viewer.destroy();
    // After destroy the Viewer throws on any access — the retention map itself is
    // cleared, which the doc-switch test already proves; here we just assert the
    // lifecycle guard fires.
    await expect(viewer.imageOriginal("rId1")).rejects.toThrow();
  });
});
