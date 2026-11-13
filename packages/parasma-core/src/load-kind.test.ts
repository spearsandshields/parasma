/**
 * Load-kind dispatch test.
 *
 * `Viewer.load(bytes)` must branch on the document format sniffed from the ZIP
 * package: an xlsx (whose central directory names `xl/workbook.xml`) routes to
 * the WASM `loadXlsx` entry and reports `documentKind: 'excel'`; everything else
 * (docx, or any non-ZIP blob) routes to `loadDocx` and reports `'word'`. The
 * `pageSize`/`pageReady` streaming contracts are identical across both kinds —
 * only the load entry and the reported kind differ.
 *
 * We inject a fake wasm-bindgen handle (same trick as load-serialization.test)
 * and record which load entry fired. The xlsx path requests the normal-style
 * font ("Calibri") through `#loadFontsFor`; we stub `fetch` to reject so those
 * font tasks fail fast (they are caught + warned, never thrown) and the load
 * resolves deterministically without hitting the network.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { Viewer } from "./index";

/**
 * Fake wasm-bindgen Viewer recording which load entry was called. Models both
 * the docx (`loadDocx`/`setNow`) and xlsx (`loadXlsx`) entries plus the shared
 * streaming surface (`runLayout`/`pageSize`/`abortLayout`). Font/image hooks are
 * present so `#doLoad`/`#doLoadXlsx` run their full setup; they record nothing
 * load-bearing for this test.
 */
class FakeWasm {
  log: string[] = [];
  #resolveRun: ((total: number) => void) | null = null;

  loadDocx(_bytes: Uint8Array): void {
    this.log.push("loadDocx");
  }
  loadXlsx(_bytes: Uint8Array): void {
    this.log.push("loadXlsx");
  }
  loadPptx(_bytes: Uint8Array): void {
    this.log.push("loadPptx");
  }
  abortLayout(): void {
    this.log.push("abort");
    const r = this.#resolveRun;
    this.#resolveRun = null;
    r?.(0);
  }
  async runLayout(_cb: (idx: number) => void): Promise<number> {
    this.log.push("runLayout");
    return new Promise<number>((res) => {
      this.#resolveRun = res;
    });
  }
  setNow(_n: number): void {}
  pageSize(): { widthPx: number; heightPx: number } {
    return { widthPx: 800, heightPx: 600 };
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
  // Constructor is `private` at compile time only; inject the fake at runtime.
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

/**
 * Build a tiny ZIP whose stored part names include `partName`, so the byte
 * substring sniff classifies it. Real OPC packages store the part name verbatim
 * (uncompressed) in the local-file header; a stored (method 0) entry reproduces
 * that for the sniffer without pulling in a zip lib.
 */
function makeZipNaming(partName: string): Uint8Array {
  const name = new TextEncoder().encode(partName);
  const header = new Uint8Array(30 + name.length);
  // Local file header signature "PK\x03\x04".
  header[0] = 0x50;
  header[1] = 0x4b;
  header[2] = 0x03;
  header[3] = 0x04;
  // Filename length at offset 26 (little-endian); 0 bytes of file data.
  header[26] = name.length & 0xff;
  header[27] = (name.length >> 8) & 0xff;
  header.set(name, 30);
  return header;
}

describe("load() document-kind dispatch", () => {
  const realFetch = globalThis.fetch;

  beforeEach(() => {
    // Font fetches in the xlsx path go through the network; reject fast so they
    // fail (caught + warned) instead of stalling the test.
    globalThis.fetch = vi.fn(() =>
      Promise.reject(new Error("network disabled in test")),
    ) as unknown as typeof fetch;
  });
  afterEach(() => {
    globalThis.fetch = realFetch;
    vi.restoreAllMocks();
  });

  it("routes an xlsx package to loadXlsx and reports documentKind 'excel'", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);

    const info = await viewer.load(makeZipNaming("xl/workbook.xml"));

    expect(info.documentKind).toBe("excel");
    expect(wasm.log).toContain("loadXlsx");
    expect(wasm.log).not.toContain("loadDocx");
    // Shared streaming contract preserved: pageSize flows through unchanged.
    expect(info.pageSize).toEqual({ widthPx: 800, heightPx: 600 });
  });

  it("routes a pptx package to loadPptx and reports documentKind 'ppt'", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);

    const info = await viewer.load(makeZipNaming("ppt/presentation.xml"));

    expect(info.documentKind).toBe("ppt");
    expect(wasm.log).toContain("loadPptx");
    expect(wasm.log).not.toContain("loadDocx");
    expect(wasm.log).not.toContain("loadXlsx");
    // Shared streaming contract preserved: pageSize flows through unchanged.
    expect(info.pageSize).toEqual({ widthPx: 800, heightPx: 600 });
  });

  it("routes a docx package to loadDocx and reports documentKind 'word'", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);

    const info = await viewer.load(makeZipNaming("word/document.xml"));

    expect(info.documentKind).toBe("word");
    expect(wasm.log).toContain("loadDocx");
    expect(wasm.log).not.toContain("loadXlsx");
    expect(info.pageSize).toEqual({ widthPx: 800, heightPx: 600 });
  });

  it("treats a non-ZIP blob as docx (parser surfaces any real error)", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);

    const info = await viewer.load(new Uint8Array([1, 2, 3, 4, 5]));

    expect(info.documentKind).toBe("word");
    expect(wasm.log).toContain("loadDocx");
    expect(wasm.log).not.toContain("loadXlsx");
  });
});
