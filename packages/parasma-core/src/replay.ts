/**
 * Display-list REPLAY — the main-thread half of
 * the off-main-thread render hybrid. The worker runs the engine against a
 * RecordingPainter and ships back a flat display list; this module replays that
 * list onto the real (GPU-backed) `<canvas>` 2D context via `ctx.*` calls — so
 * GPU compositing and `document.fonts` stay on the main thread (no font fork).
 *
 * The wire form is `{ opcodes: Float64Array, strings: string[] }` (see the Rust
 * encoder in crates/parasma-wasm/src/engine/displaylist.rs). `replayDisplayList`
 * walks the f64 opcode stream with an index-based switch — no per-command object
 * allocation. The OPCODE NUMBERS + per-opcode arg order below MUST stay identical
 * to the Rust `OP_*` constants; they are the cross-language contract.
 */

// ── Opcode contract — MUST match engine/displaylist.rs OP_* constants ────────-
const OP_SAVE = 0;
const OP_RESTORE = 1;
const OP_BEGIN_PATH = 2;
const OP_CLOSE_PATH = 3;
const OP_MOVE_TO = 4;
const OP_LINE_TO = 5;
const OP_RECT = 6;
const OP_ARC = 7;
const OP_ARC_ACW = 8;
const OP_ELLIPSE = 9;
const OP_QUAD_TO = 10;
const OP_BEZIER_TO = 11;
const OP_FILL = 12;
const OP_STROKE = 13;
const OP_CLIP = 14;
const OP_FILL_RECT = 15;
const OP_STROKE_RECT = 16;
const OP_FILL_TEXT = 17;
const OP_STROKE_TEXT = 18;
const OP_SET_FONT = 19;
const OP_SET_TEXT_ALIGN = 20;
const OP_SET_TEXT_BASELINE = 21;
const OP_SET_FILL_STYLE = 22;
const OP_SET_STROKE_STYLE = 23;
const OP_SET_LINE_WIDTH = 24;
const OP_SET_GLOBAL_ALPHA = 25;
const OP_SET_SHADOW_COLOR = 26;
const OP_SET_SHADOW_OFFSET_X = 27;
const OP_SET_SHADOW_OFFSET_Y = 28;
const OP_SET_SHADOW_BLUR = 29;
const OP_TRANSLATE = 30;
const OP_ROTATE = 31;
const OP_SCALE = 32;
const OP_SET_TRANSFORM = 33;
const OP_DRAW_IMAGE = 34;
// PML style primitives — the FIRST variable-length, count-prefixed opcodes.
const OP_SET_LINEAR_GRADIENT = 35;
const OP_SET_RADIAL_GRADIENT = 36;
const OP_SET_PATTERN_FILL = 37;
const OP_SET_LINE_DASH = 38;
const OP_CLEAR_LINE_DASH = 39;
// pattern-tile sub-tags (mirror engine/displaylist.rs TILE_LINE / TILE_DOT).
const TILE_LINE = 0;
const TILE_DOT = 1;

/** One rasterisable pattern-tile primitive (mirrors the Rust `TilePrim`). */
type TilePrim =
  | { tag: typeof TILE_LINE; x1: number; y1: number; x2: number; y2: number; lw: number }
  | { tag: typeof TILE_DOT; x: number; y: number; s: number };

/**
 * 50/50 blend of two `#rrggbb` colours — the pattern surface-miss fallback,
 * mirroring the Rust `blend_hex_50_str` (pml/mod.rs) so an off-thread
 * `createPattern` miss degrades to the SAME solid the in-process painter paints.
 * A bad hex returns the foreground (`a`), matching the Rust `_ => a`.
 */
function blendHex50(a: string, b: string): string {
  const parse = (h: string): [number, number, number] | null => {
    const s = h.replace(/^#/, "");
    if (s.length !== 6) return null;
    const r = parseInt(s.slice(0, 2), 16);
    const g = parseInt(s.slice(2, 4), 16);
    const bl = parseInt(s.slice(4, 6), 16);
    if (Number.isNaN(r) || Number.isNaN(g) || Number.isNaN(bl)) return null;
    return [r, g, bl];
  };
  const pa = parse(a);
  const pb = parse(b);
  if (!pa || !pb) return a;
  const hx = (n: number): string => (n & 0xff).toString(16).padStart(2, "0");
  // `>> 1` = floor(sum/2), matching the Rust integer `(r1 + r2) / 2`.
  return `#${hx((pa[0] + pb[0]) >> 1)}${hx((pa[1] + pb[1]) >> 1)}${hx((pa[2] + pb[2]) >> 1)}`;
}

/**
 * Re-rasterise a pattern tile offscreen and wrap it in `createPattern(…,
 * "repeat")` — a DIRECT PORT of the Rust `CanvasPainter::set_pattern_fill`
 * stamping loop (paint.rs): a real `<canvas>` (NOT `OffscreenCanvas`, so the
 * tile matches the in-process offscreen tile exactly), `size×size`, bg flood,
 * then fg strokes/dots with `lineCap = "butt"`. NOT device-scaled (a DPR scale
 * would diverge from the in-process tile). Returns `null` on a surface miss.
 */
function rasterizePatternTile(
  ctx: CanvasRenderingContext2D,
  size: number,
  fg: string,
  bg: string,
  prims: TilePrim[],
): CanvasPattern | null {
  const c = document.createElement("canvas");
  c.width = size;
  c.height = size;
  const t = c.getContext("2d");
  if (!t) return null;
  t.fillStyle = bg;
  t.fillRect(0, 0, size, size);
  t.strokeStyle = fg;
  t.fillStyle = fg;
  t.lineCap = "butt";
  for (const prim of prims) {
    if (prim.tag === TILE_LINE) {
      t.lineWidth = prim.lw;
      t.beginPath();
      t.moveTo(prim.x1, prim.y1);
      t.lineTo(prim.x2, prim.y2);
      t.stroke();
    } else {
      t.fillRect(prim.x, prim.y, prim.s, prim.s);
    }
  }
  return ctx.createPattern(c, "repeat");
}

/** The flat wire form a display-list wasm export returns. */
export interface EncodedDisplayList {
  /** Flat opcode + arg stream (see the OP_* contract). */
  opcodes: Float64Array;
  /** Deduplicated string table; string args are f64 indices into this. */
  strings: string[];
}

/**
 * The `[image]` placeholder for a missing/undecoded image — byte-identical to
 * the Rust `draw_image_placeholder` (engine/images.rs), so a key that resolves on
 * one path and misses on the other still produces the same pixels. The worker's
 * RecordingPainter records the key unconditionally; THIS side decides bitmap vs
 * placeholder (it owns the bitmap cache).
 */
function drawImagePlaceholder(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
): void {
  ctx.fillStyle = "#e6edf3";
  ctx.fillRect(x, y, w, h);
  ctx.strokeStyle = "#7d8590";
  ctx.lineWidth = 1.0;
  ctx.strokeRect(x, y, w, h);
  ctx.fillStyle = "#7d8590";
  ctx.font = "11px -apple-system, sans-serif";
  ctx.textBaseline = "alphabetic";
  ctx.fillText("[image]", x + 8.0, y + 18.0);
}

/**
 * Replay an encoded display list onto a 2D context. Pure plumbing: each opcode
 * maps to the identical `ctx.*` call the Rust `CanvasPainter` would have made.
 * `images` resolves `DrawImage` keys (relId → ImageBitmap); a miss draws the
 * placeholder. The caller owns the context's pre-transform (the list is in the
 * same coordinate space the engine painted in).
 *
 * `onImageDraw` (EMF raster cache): invoked for every `DrawImage` whose key
 * starts with `"emf-pic:"` — the worker-mode paint-request funnel feeding the
 * resolution policy. Fired on HIT **and** MISS (a hit at a higher zoom must
 * still trigger the upgrade check; a miss is the first-paint placeholder
 * beat). `dw`/`dh` are the list's page-logical px; the caller multiplies by
 * its known zoom·dpr. Inert for documents without EMF keys (prefix filter).
 */
export function replayDisplayList(
  ctx: CanvasRenderingContext2D,
  opcodes: Float64Array,
  strings: string[],
  images: Map<string, ImageBitmap>,
  onImageDraw?: (key: string, dw: number, dh: number) => void,
): void {
  const n = opcodes.length;
  let i = 0;
  // The encoder guarantees well-formed [opcode, ...args] frames, so reads are
  // in-bounds; the `!` satisfies `noUncheckedIndexedAccess` (Float64Array
  // indexing is typed `number | undefined` under it) without per-read branching.
  const next = (): number => opcodes[i++]!;
  const str = (): string => strings[opcodes[i++]!] ?? "";
  // Per-replay pattern cache (fresh each call, matching the per-render
  // `CanvasPainter.pattern_cache` lifetime): one rasterised tile per `prst|fg|bg`
  // key, so a second identical key reuses it (no second `createElement`).
  const patternCache = new Map<string, CanvasPattern | null>();
  while (i < n) {
    const op = opcodes[i++];
    switch (op) {
      case OP_SAVE:
        ctx.save();
        break;
      case OP_RESTORE:
        ctx.restore();
        break;
      case OP_BEGIN_PATH:
        ctx.beginPath();
        break;
      case OP_CLOSE_PATH:
        ctx.closePath();
        break;
      case OP_MOVE_TO:
        ctx.moveTo(next(), next());
        break;
      case OP_LINE_TO:
        ctx.lineTo(next(), next());
        break;
      case OP_RECT:
        ctx.rect(next(), next(), next(), next());
        break;
      case OP_ARC:
        ctx.arc(next(), next(), next(), next(), next());
        break;
      case OP_ARC_ACW:
        ctx.arc(next(), next(), next(), next(), next(), next() !== 0);
        break;
      case OP_ELLIPSE:
        ctx.ellipse(next(), next(), next(), next(), next(), next(), next(), next() !== 0);
        break;
      case OP_QUAD_TO:
        ctx.quadraticCurveTo(next(), next(), next(), next());
        break;
      case OP_BEZIER_TO:
        ctx.bezierCurveTo(next(), next(), next(), next(), next(), next());
        break;
      case OP_FILL:
        ctx.fill();
        break;
      case OP_STROKE:
        ctx.stroke();
        break;
      case OP_CLIP:
        ctx.clip();
        break;
      case OP_FILL_RECT:
        ctx.fillRect(next(), next(), next(), next());
        break;
      case OP_STROKE_RECT:
        ctx.strokeRect(next(), next(), next(), next());
        break;
      case OP_FILL_TEXT: {
        const t = str();
        ctx.fillText(t, next(), next());
        break;
      }
      case OP_STROKE_TEXT: {
        const t = str();
        ctx.strokeText(t, next(), next());
        break;
      }
      case OP_SET_FONT:
        ctx.font = str();
        break;
      case OP_SET_TEXT_ALIGN:
        ctx.textAlign = str() as CanvasTextAlign;
        break;
      case OP_SET_TEXT_BASELINE:
        ctx.textBaseline = str() as CanvasTextBaseline;
        break;
      case OP_SET_FILL_STYLE:
        ctx.fillStyle = str();
        break;
      case OP_SET_STROKE_STYLE:
        ctx.strokeStyle = str();
        break;
      case OP_SET_LINE_WIDTH:
        ctx.lineWidth = next();
        break;
      case OP_SET_GLOBAL_ALPHA:
        ctx.globalAlpha = next();
        break;
      case OP_SET_SHADOW_COLOR:
        ctx.shadowColor = str();
        break;
      case OP_SET_SHADOW_OFFSET_X:
        ctx.shadowOffsetX = next();
        break;
      case OP_SET_SHADOW_OFFSET_Y:
        ctx.shadowOffsetY = next();
        break;
      case OP_SET_SHADOW_BLUR:
        ctx.shadowBlur = next();
        break;
      case OP_TRANSLATE:
        ctx.translate(next(), next());
        break;
      case OP_ROTATE:
        ctx.rotate(next());
        break;
      case OP_SCALE:
        ctx.scale(next(), next());
        break;
      case OP_SET_TRANSFORM:
        ctx.setTransform(next(), next(), next(), next(), next(), next());
        break;
      case OP_DRAW_IMAGE: {
        const key = str();
        const dx = next();
        const dy = next();
        const dw = next();
        const dh = next();
        // EMF raster-cache funnel: report the drawn footprint on hit AND miss
        // (see the doc comment). "emf-pic:" (hyphen+colon) never matches the
        // internal DIB blit keys "emf:{rel}:{n}" or ordinary rels.
        if (onImageDraw && key.startsWith("emf-pic:")) onImageDraw(key, dw, dh);
        const bm = images.get(key);
        if (bm) ctx.drawImage(bm, dx, dy, dw, dh);
        else drawImagePlaceholder(ctx, dx, dy, dw, dh);
        break;
      }
      case OP_SET_LINEAR_GRADIENT: {
        const x0 = next();
        const y0 = next();
        const x1 = next();
        const y1 = next();
        const cnt = next(); // read count BEFORE the stop payload
        const g = ctx.createLinearGradient(x0, y0, x1, y1);
        for (let k = 0; k < cnt; k++) g.addColorStop(next(), str());
        ctx.fillStyle = g;
        break;
      }
      case OP_SET_RADIAL_GRADIENT: {
        const x0 = next();
        const y0 = next();
        const r0 = next();
        const x1 = next();
        const y1 = next();
        const r1 = next();
        const cnt = next();
        const g = ctx.createRadialGradient(x0, y0, r0, x1, y1, r1);
        for (let k = 0; k < cnt; k++) g.addColorStop(next(), str());
        ctx.fillStyle = g;
        break;
      }
      case OP_SET_PATTERN_FILL: {
        const key = str();
        const fg = str();
        const bg = str();
        const size = next();
        const pc = next();
        const prims: TilePrim[] = [];
        for (let k = 0; k < pc; k++) {
          const tag = next();
          if (tag === TILE_LINE) {
            prims.push({ tag: TILE_LINE, x1: next(), y1: next(), x2: next(), y2: next(), lw: next() });
          } else {
            prims.push({ tag: TILE_DOT, x: next(), y: next(), s: next() });
          }
        }
        let pat = patternCache.get(key);
        if (pat === undefined) {
          pat = rasterizePatternTile(ctx, size, fg, bg, prims);
          patternCache.set(key, pat);
        }
        // `pat ?? blend` — an off-thread surface miss degrades to the SAME blend
        // the in-process painter paints (byte-identical fallback parity).
        ctx.fillStyle = pat ?? blendHex50(fg, bg);
        break;
      }
      case OP_SET_LINE_DASH: {
        const cnt = next();
        const segs: number[] = [];
        for (let k = 0; k < cnt; k++) segs.push(next());
        ctx.setLineDash(segs);
        break;
      }
      case OP_CLEAR_LINE_DASH:
        ctx.setLineDash([]);
        break;
      default:
        throw new Error(`replayDisplayList: unknown opcode ${op} at ${i - 1}`);
    }
  }
}
