/**
 * Regression test for the self-defeating `imageRefDiagnostics()` diagnostic.
 *
 * `imageRefDiagnostics()` used to query the live WASM `imageRefs()`. But once a
 * document's images finish decoding in the background, `#decodeImagesInBackground`
 * calls `clearImageBytes()`, which empties the WASM-side `self.images`. After
 * that, `imageRefs()` returns `[]`. So a caller reading the diagnostic AFTER the
 * decode settled (the common case) always saw `[]` — making a working image
 * pipeline look like an extraction failure (the blank-image misdiagnosis).
 *
 * The fix captures a snapshot of the refs at extraction time (before
 * `clearImageBytes()`), and `imageRefDiagnostics()` reports that snapshot. This
 * test models the WASM clear contract: `imageRefs()` yields one ref, then `[]`
 * after `clearImageBytes()` — and asserts the diagnostic still reports the ref
 * once the decode has settled and cleared.
 */
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { Viewer } from "./index";

const PNG_BYTES = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

/**
 * Fake wasm-bindgen Viewer modelling the image-extraction + clear contract.
 * `imageRefs()` returns a single PNG ref until `clearImageBytes()` runs, after
 * which it returns `[]` — exactly the live-WASM behaviour that defeated the old
 * diagnostic.
 */
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
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

describe("imageRefDiagnostics() snapshot (blank-image misdiagnosis regression)", () => {
  let origBlob: typeof globalThis.Blob;
  let origCreateImageBitmap: typeof globalThis.createImageBitmap;

  beforeEach(() => {
    origBlob = globalThis.Blob;
    origCreateImageBitmap = globalThis.createImageBitmap;
    // Minimal Blob stub — the real one only needs to carry the parts/type.
    globalThis.Blob = class {
      constructor(
        public parts: unknown[],
        public options?: { type?: string },
      ) {}
    } as unknown as typeof globalThis.Blob;
    // Decode resolves to a trivial bitmap so the background decode settles and
    // triggers clearImageBytes().
    globalThis.createImageBitmap = (async () => ({
      close() {},
    })) as unknown as typeof globalThis.createImageBitmap;
  });

  afterEach(() => {
    globalThis.Blob = origBlob;
    globalThis.createImageBitmap = origCreateImageBitmap;
  });

  it("reports the extracted ref even after the decode settles and clears WASM bytes", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);

    await viewer.load(PNG_BYTES);

    // The background decode has not necessarily settled yet, but the snapshot
    // was captured synchronously inside #decodeImagesInBackground.
    expect(await viewer.imageRefDiagnostics()).toEqual([
      { relId: "rId1", mime: "image/png", byteLength: PNG_BYTES.byteLength },
    ]);

    // Let the createImageBitmap microtasks settle so clearImageBytes() runs and
    // the live WASM imageRefs() now returns [].
    await new Promise((r) => setTimeout(r, 0));
    expect(wasm.imageRefs()).toEqual([]); // WASM-side bytes are gone…

    // …but the diagnostic still reports the captured snapshot. THIS is the line
    // that was red before the fix (it returned [] post-clear).
    expect(await viewer.imageRefDiagnostics()).toEqual([
      { relId: "rId1", mime: "image/png", byteLength: PNG_BYTES.byteLength },
    ]);
  });

  it("returns a fresh copy so callers cannot mutate the internal snapshot", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    await viewer.load(PNG_BYTES);

    const first = await viewer.imageRefDiagnostics();
    first[0]!.relId = "TAMPERED";
    expect((await viewer.imageRefDiagnostics())[0]!.relId).toBe("rId1");
  });
});
