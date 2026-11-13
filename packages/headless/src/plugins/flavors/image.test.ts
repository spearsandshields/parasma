/**
 * Image flavor tests. Pure-TS over a mocked CopyContext —
 * base64, the mime allowlist, the budgeted resolver, and the png/html
 * providers incl. the worker skip + encode-failure isolation.
 */
import { describe, it, expect } from "vitest";
import type { DocFragment } from "@parasma/core";
import {
  base64FromBytes,
  buildImageResolver,
  collectImageRelIds,
  imageHtmlProvider,
  imagePngProvider,
} from "./image";
import { sanitizeImageMime } from "./sanitize";
import { DEFAULT_BUDGETS, type CopyBudgets, type CopyContext, type ObjectImageInfo } from "./types";

// ── a mock CopyContext ────────────────────────────────────────────────────────
interface MockOpts {
  originals?: Record<string, { mime: string; bytes: Uint8Array }>;
  object?: ObjectImageInfo | null;
  bitmap?: ImageBitmap | null;
  encodePng?: (b: ImageBitmap) => Promise<Blob>;
  budgets?: CopyBudgets;
}
function mockCtx(o: MockOpts): CopyContext {
  return {
    fragment: async () => ({ blocks: [] }),
    cellFragment: async () => ({ rows: [], rowCount: 0, totalRows: 0, truncated: false, reason: null }),
    budgets: o.budgets ?? DEFAULT_BUDGETS,
    object: async () => o.object ?? null,
    imageOriginal: async (relId) => o.originals?.[relId] ?? null,
    imageBitmap: async () => o.bitmap ?? null,
    encodePng: o.encodePng ?? (async () => new Blob(["png"], { type: "image/png" })),
    // Chart seams — inert for the image flavors under test.
    chartKey: null,
    chart: async () => null,
    renderChartToCanvas: async () => false,
  };
}

const imgFrag = (relIds: string[]): DocFragment => ({
  blocks: [
    {
      kind: "para",
      para: {
        runs: relIds.map((relId) => ({
          text: "",
          bold: false, italic: false, underline: "none", strike: false, doubleStrike: false,
          fontFamily: "Calibri", fontSizePt: 11, color: "#000000", highlight: null,
          runShading: null, vertAlign: "baseline", link: null,
          image: { relId, wPx: 10, hPx: 10 }, footnoteMark: null,
        })),
        alignment: "start", dir: "ltr", list: null,
      },
    },
  ],
});

describe("base64FromBytes", () => {
  it("encodes 3-byte groups exactly", () => {
    expect(base64FromBytes(new Uint8Array([0, 1, 2]))).toBe("AAEC");
  });
  it("empty → empty string", () => {
    expect(base64FromBytes(new Uint8Array([]))).toBe("");
  });
  it("pads 1 and 2 trailing bytes", () => {
    expect(base64FromBytes(new Uint8Array([0]))).toBe("AA==");
    expect(base64FromBytes(new Uint8Array([0, 0]))).toBe("AAA=");
    // 4-byte input exercises a full group + 1-byte remainder.
    expect(base64FromBytes(new Uint8Array([77, 97, 110, 77]))).toBe("TWFuTQ==");
  });
});

describe("sanitizeImageMime", () => {
  it("passes the raster allowlist and folds jpg→jpeg", () => {
    for (const m of ["image/png", "image/jpeg", "image/gif", "image/bmp", "image/webp"]) {
      expect(sanitizeImageMime(m)).toBe(m);
    }
    expect(sanitizeImageMime("image/jpg")).toBe("image/jpeg");
    expect(sanitizeImageMime("IMAGE/PNG")).toBe("image/png");
  });
  it("rejects svg / html / empty / junk", () => {
    for (const m of ["image/svg+xml", "text/html", "", "notamime", null, undefined]) {
      expect(sanitizeImageMime(m as string)).toBeNull();
    }
  });
});

describe("collectImageRelIds", () => {
  it("returns distinct relIds in doc order", () => {
    expect(collectImageRelIds(imgFrag(["a", "b", "a", "c"]))).toEqual(["a", "b", "c"]);
  });
});

describe("buildImageResolver (budgets)", () => {
  const bytes = (n: number) => new Uint8Array(n).fill(1);

  it("resolves a single image to a data:URL", async () => {
    const ctx = mockCtx({ originals: { a: { mime: "image/png", bytes: new Uint8Array([0, 1, 2]) } } });
    const r = await buildImageResolver(imgFrag(["a"]), ctx);
    expect(r("a")).toBe("data:image/png;base64,AAEC");
  });

  it("skips an image over the per-image cap → null (placeholder)", async () => {
    const ctx = mockCtx({
      originals: { a: { mime: "image/png", bytes: bytes(100) } },
      budgets: { perImageBytes: 10, totalHtmlBytes: 1_000_000 },
    });
    expect((await buildImageResolver(imgFrag(["a"]), ctx))("a")).toBeNull();
  });

  it("enforces the total budget in doc order: first resolves, second null", async () => {
    const ctx = mockCtx({
      originals: {
        a: { mime: "image/png", bytes: new Uint8Array([0, 1, 2]) },
        b: { mime: "image/png", bytes: new Uint8Array([0, 1, 2]) },
      },
      // Budget fits exactly one data:URL (len 27) but not two.
      budgets: { perImageBytes: 1_000_000, totalHtmlBytes: 30 },
    });
    const r = await buildImageResolver(imgFrag(["a", "b"]), ctx);
    expect(r("a")).not.toBeNull();
    expect(r("b")).toBeNull();
  });

  it("worker mode (imageOriginal null) → resolver returns null", async () => {
    const ctx = mockCtx({ originals: {} }); // nothing retained
    expect((await buildImageResolver(imgFrag(["a"]), ctx))("a")).toBeNull();
  });

  it("a non-allowlisted mime → null", async () => {
    const ctx = mockCtx({ originals: { a: { mime: "image/svg+xml", bytes: new Uint8Array([0]) } } });
    expect((await buildImageResolver(imgFrag(["a"]), ctx))("a")).toBeNull();
  });
});

describe("imagePngProvider", () => {
  const bmp = { width: 20, height: 16, close() {} } as unknown as ImageBitmap;

  it("encodes the bitmap to a PNG Blob", async () => {
    const ctx = mockCtx({ object: { relId: "a", mime: "image/png", wPx: 20, hPx: 16 }, bitmap: bmp });
    const out = await imagePngProvider.provide("wml-object-image", ctx);
    expect(out).toBeInstanceOf(Blob);
  });

  it("worker/not-decoded (bitmap null) → null (clean skip)", async () => {
    const ctx = mockCtx({ object: { relId: "a", mime: "image/png", wPx: 20, hPx: 16 }, bitmap: null });
    expect(await imagePngProvider.provide("wml-object-image", ctx)).toBeNull();
  });

  it("a failing encoder → null, never throws", async () => {
    const ctx = mockCtx({
      object: { relId: "a", mime: "image/png", wPx: 20, hPx: 16 },
      bitmap: bmp,
      encodePng: async () => { throw new Error("boom"); },
    });
    expect(await imagePngProvider.provide("wml-object-image", ctx)).toBeNull();
  });

  it("no object selected → null", async () => {
    expect(await imagePngProvider.provide("wml-object-image", mockCtx({ object: null }))).toBeNull();
  });
});

describe("imageHtmlProvider", () => {
  it("emits <img src=data:…> at INTRINSIC dims", async () => {
    const ctx = mockCtx({
      object: { relId: "a", mime: "image/png", wPx: 20, hPx: 16 },
      originals: { a: { mime: "image/png", bytes: new Uint8Array([0, 1, 2]) } },
    });
    expect(await imageHtmlProvider.provide("wml-object-image", ctx)).toBe(
      `<meta charset="utf-8"><img src="data:image/png;base64,AAEC" width="20" height="16">`,
    );
  });

  it("over-cap → <img alt> placeholder", async () => {
    const ctx = mockCtx({
      object: { relId: "a", mime: "image/png", wPx: 20, hPx: 16 },
      originals: { a: { mime: "image/png", bytes: new Uint8Array(100).fill(1) } },
      budgets: { perImageBytes: 10, totalHtmlBytes: 1_000_000 },
    });
    expect(await imageHtmlProvider.provide("wml-object-image", ctx)).toBe(
      `<meta charset="utf-8"><img alt="image" width="20" height="16">`,
    );
  });

  it("worker mode (no retained original) → <img alt> placeholder", async () => {
    const ctx = mockCtx({ object: { relId: "a", mime: "image/png", wPx: 20, hPx: 16 }, originals: {} });
    expect(await imageHtmlProvider.provide("wml-object-image", ctx)).toBe(
      `<meta charset="utf-8"><img alt="image" width="20" height="16">`,
    );
  });

  it("a bad mime → <img alt> placeholder", async () => {
    const ctx = mockCtx({
      object: { relId: "a", mime: "image/svg+xml", wPx: 20, hPx: 16 },
      originals: { a: { mime: "image/svg+xml", bytes: new Uint8Array([0, 1, 2]) } },
    });
    expect(await imageHtmlProvider.provide("wml-object-image", ctx)).toBe(
      `<meta charset="utf-8"><img alt="image" width="20" height="16">`,
    );
  });

  it("no object selected → null", async () => {
    expect(await imageHtmlProvider.provide("wml-object-image", mockCtx({ object: null }))).toBeNull();
  });
});
