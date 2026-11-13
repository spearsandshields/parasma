/**
 * Image flavors. Two producers for the
 * `wml-object-image` selection kind:
 *   - `image/png`  — the decoded INTRINSIC-resolution bitmap re-encoded to PNG
 *     (never a canvas-zoom screenshot), via the injected `PngEncoder`.
 *   - `text/html`  — an `<img src="data:…">` of the UNTOUCHED original bytes.
 * Plus `buildImageResolver` — the COPY-TIME (never per-selection) relId →
 * data-URL map the inline-image html seam consumes, with the copy budgets
 * enforced (per-image + total) and the raster mime allowlist.
 *
 * PNG encodes at intrinsic resolution only; the over-budget PNG downscale is a
 * documented v1 follow-on (Blob size is unknown pre-encode). A validated mime +
 * the base64 alphabet carry no `"<>&`, so the assembled data-URL is emitted
 * UNescaped (the same documented invariant as html.ts's validated style values).
 */
import type { DocFragment, FragBlock } from "@parasma/core";
import type {
  CopyContext,
  FlavorProvider,
  ImageResolver,
  PngEncoder,
  SelectionKind,
} from "./types";
import { sanitizeImageMime } from "./sanitize";
import { imgDims } from "./html";

const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/** Dependency-free, chunk-safe base64 of raw bytes (no btoa string-length trap). */
export function base64FromBytes(bytes: Uint8Array): string {
  let out = "";
  const len = bytes.length;
  let i = 0;
  for (; i + 2 < len; i += 3) {
    const n = (bytes[i]! << 16) | (bytes[i + 1]! << 8) | bytes[i + 2]!;
    out += B64[(n >> 18) & 63]! + B64[(n >> 12) & 63]! + B64[(n >> 6) & 63]! + B64[n & 63]!;
  }
  const rem = len - i;
  if (rem === 1) {
    const n = bytes[i]! << 16;
    out += B64[(n >> 18) & 63]! + B64[(n >> 12) & 63]! + "==";
  } else if (rem === 2) {
    const n = (bytes[i]! << 16) | (bytes[i + 1]! << 8);
    out += B64[(n >> 18) & 63]! + B64[(n >> 12) & 63]! + B64[(n >> 6) & 63]! + "=";
  }
  return out;
}

/** Every distinct inline-image relId in the fragment, in DOC ORDER (dedup). */
export function collectImageRelIds(frag: DocFragment): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  const visit = (blocks: FragBlock[]): void => {
    for (const b of blocks) {
      if (b.kind === "para") {
        for (const r of b.para.runs) {
          if (r.image && !seen.has(r.image.relId)) {
            seen.add(r.image.relId);
            out.push(r.image.relId);
          }
        }
      } else {
        for (const row of b.table.rows) for (const c of row.cells) visit(c.blocks);
      }
    }
  };
  visit(frag.blocks);
  return out;
}

/**
 * Build the inline-image data-URL resolver at COPY TIME (NEVER cached per
 * selection change). Each relId in doc order is resolved from the retained
 * originals; a relId is SKIPPED (→ resolver returns null → `<img alt>`
 * placeholder) when it is absent / worker-mode-unreachable, over the per-image
 * byte cap, has a non-allowlisted mime, or would push assembled data-URLs past
 * the total HTML budget.
 */
export async function buildImageResolver(
  frag: DocFragment,
  ctx: CopyContext,
  // The total-HTML budget LEDGER, shared with buildChartResolver when both feed
  // one text/html copy (independent accumulators would let the pair emit
  // 2× totalHtmlBytes). Callers composing several resolvers pass ONE
  // ledger; the default keeps single-resolver callers/tests unchanged.
  ledger: { bytes: number } = { bytes: 0 },
): Promise<ImageResolver> {
  const map = new Map<string, string>();
  for (const relId of collectImageRelIds(frag)) {
    const orig = await ctx.imageOriginal(relId);
    if (!orig) continue; // absent / worker mode
    if (orig.bytes.length > ctx.budgets.perImageBytes) continue; // per-image cap
    const mime = sanitizeImageMime(orig.mime);
    if (!mime) continue;
    const url = `data:${mime};base64,${base64FromBytes(orig.bytes)}`;
    if (ledger.bytes + url.length > ctx.budgets.totalHtmlBytes) continue; // total budget
    map.set(relId, url);
    ledger.bytes += url.length;
  }
  return (relId: string) => map.get(relId) ?? null;
}

/** image/png for a selected image object — the intrinsic bitmap re-encoded. */
export const imagePngProvider: FlavorProvider = {
  mime: "image/png",
  appliesTo: (k: SelectionKind) => k === "wml-object-image" || k === "pml-object-image",
  provide: async (_k: SelectionKind, ctx: CopyContext): Promise<Blob | null> => {
    const obj = await ctx.object();
    if (!obj) return null;
    const bmp = await ctx.imageBitmap(obj.relId);
    if (!bmp) return null; // worker / not decoded → clean skip
    try {
      return await ctx.encodePng(bmp);
    } catch {
      return null; // a failed encode must not sink the html flavor
    }
  },
};

/** text/html `<img data-url>` for a selected image object (INTRINSIC dims). */
export const imageHtmlProvider: FlavorProvider = {
  mime: "text/html",
  appliesTo: (k: SelectionKind) => k === "wml-object-image" || k === "pml-object-image",
  provide: async (_k: SelectionKind, ctx: CopyContext): Promise<string | null> => {
    const obj = await ctx.object();
    if (!obj) return null;
    const dim = imgDims(obj.wPx, obj.hPx);
    const orig = await ctx.imageOriginal(obj.relId);
    if (!orig) return `<meta charset="utf-8"><img alt="image"${dim}>`;
    const mime = sanitizeImageMime(orig.mime);
    if (!mime || orig.bytes.length > ctx.budgets.perImageBytes) {
      return `<meta charset="utf-8"><img alt="image"${dim}>`; // over-budget / bad mime → placeholder
    }
    return `<meta charset="utf-8"><img src="data:${mime};base64,${base64FromBytes(orig.bytes)}"${dim}>`;
  },
};

/**
 * The default `PngEncoder`: draw the intrinsic bitmap onto an OffscreenCanvas and
 * `convertToBlob("image/png")`. Throws `no-offscreencanvas` when the API is
 * absent (the provider catches → clean skip); the DOM adapter may inject
 * a canvas-backed encoder where OffscreenCanvas is unavailable.
 */
export const defaultPngEncoder: PngEncoder = async (bmp: ImageBitmap): Promise<Blob> => {
  const OC = (globalThis as { OffscreenCanvas?: typeof OffscreenCanvas }).OffscreenCanvas;
  if (!OC) throw new Error("no-offscreencanvas");
  const canvas = new OC(bmp.width, bmp.height);
  const cx = canvas.getContext("2d");
  if (!cx) throw new Error("no-2d-context");
  cx.drawImage(bmp, 0, 0);
  return canvas.convertToBlob({ type: "image/png" });
};
