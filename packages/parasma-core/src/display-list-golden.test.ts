/**
 * The display-list wire contract, checked from the TS side.
 *
 * In worker mode the Rust `RecordingPainter` records draw commands,
 * `encode_display_list` packs them into a flat f64 stream plus a string table,
 * and `replayDisplayList` decodes them here. The opcode numbers and argument
 * order in replay.ts are a hand-kept copy of the Rust ones, and a wrong argument
 * count desyncs the stream without an error.
 *
 * `display-list-golden.json` is written by the Rust test
 * `wire_golden_matches_the_fixture` (engine/displaylist.rs). It holds every
 * draw-command variant, encoded by the real encoder, and each command's
 * arguments in wire order. This file replays that stream into a context that
 * logs every call, and checks the log against the decoded list one command at a
 * time. The calls each opcode must produce are stated below, independently of
 * replay.ts.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as replay from "./replay";
import { replayDisplayList } from "./replay";
import goldenJson from "./display-list-golden.json";

type Arg = number | string | boolean;
interface Golden {
  opcodes: Record<string, number>;
  tileTags: Record<"TILE_LINE" | "TILE_DOT", number>;
  strings: string[];
  stream: number[];
  decoded: { op: string; args: Arg[] }[];
}
const golden = goldenJson as Golden;

/** One logged call: a method name and its arguments, or `set <prop>` and the value. */
type Call = { op: string; args: unknown[] };

/** A context stand-in that logs every method call and property write it sees. */
function loggingTarget(prefix: string, log: Call[], returns: Record<string, (...args: unknown[]) => unknown> = {}) {
  return new Proxy(
    {},
    {
      get: (_target, prop) =>
        typeof prop !== "string"
          ? undefined
          : (...args: unknown[]) => {
              log.push({ op: `${prefix}${prop}`, args });
              return returns[prop]?.(...args);
            },
      set: (_target, prop, value) => {
        log.push({ op: `${prefix}set ${String(prop)}`, args: [value] });
        return true;
      },
    },
  );
}

const PATTERN = { pattern: "tile" };
const BITMAP = { bitmap: "rId7" } as unknown as ImageBitmap;
/** The replay's bitmap cache: `rId7` hits, every other key misses. */
const IMAGES = new Map([["rId7", BITMAP]]);

let log: Call[];

function makeCtx(): CanvasRenderingContext2D {
  const gradient = (kind: string) => ({
    kind,
    addColorStop: (pos: number, color: string) => log.push({ op: "addColorStop", args: [pos, color] }),
  });
  return loggingTarget("", log, {
    createLinearGradient: () => gradient("linear"),
    createRadialGradient: () => gradient("radial"),
    createPattern: () => PATTERN,
  }) as CanvasRenderingContext2D;
}

beforeEach(() => {
  log = [];
  // The pattern opcode stamps its tile onto an offscreen <canvas>; log that too.
  const tile = loggingTarget("tile.", log);
  const canvas = loggingTarget("canvas.", log, { getContext: () => tile });
  vi.stubGlobal("document", {
    createElement: (tag: string) => {
      log.push({ op: "document.createElement", args: [tag] });
      return canvas;
    },
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

// ── The calls each opcode must produce, from its wire arguments ──────────────

const num = (a: Arg | undefined): number => {
  if (typeof a !== "number") throw new Error(`expected a number argument, got ${String(a)}`);
  return a;
};
const str = (a: Arg | undefined): string => {
  if (typeof a !== "string") throw new Error(`expected a string argument, got ${String(a)}`);
  return a;
};
const bool = (a: Arg | undefined): boolean => {
  if (typeof a !== "boolean") throw new Error(`expected a flag argument, got ${String(a)}`);
  return a;
};
const call = (op: string, ...args: unknown[]): Call => ({ op, args });
/** A command that becomes one call with its arguments passed through unchanged. */
const direct =
  (op: string) =>
  (args: Arg[]): Call[] => [call(op, ...args)];
/** A command that becomes one property write. */
const setter =
  (prop: string) =>
  (args: Arg[]): Call[] => [call(`set ${prop}`, args[0])];

/** The `[image]` placeholder a cache miss draws: the same calls as the Rust `draw_image_placeholder`. */
const placeholder = (x: number, y: number, w: number, h: number): Call[] => [
  call("set fillStyle", "#e6edf3"),
  call("fillRect", x, y, w, h),
  call("set strokeStyle", "#7d8590"),
  call("set lineWidth", 1),
  call("strokeRect", x, y, w, h),
  call("set fillStyle", "#7d8590"),
  call("set font", "11px -apple-system, sans-serif"),
  call("set textBaseline", "alphabetic"),
  call("fillText", "[image]", x + 8, y + 18),
];

/** `count, (pos, colour) × count` → one `addColorStop` per stop. */
const colorStops = (rest: Arg[]): Call[] => {
  const count = num(rest[0]);
  expect(rest).toHaveLength(1 + 2 * count);
  return Array.from({ length: count }, (_, k) => call("addColorStop", num(rest[1 + 2 * k]), str(rest[2 + 2 * k])));
};

/** Pattern keys already rasterised in this replay; a repeat reuses the tile. */
let seenPatterns: Set<string>;

const EXPECT: Record<string, (args: Arg[]) => Call[]> = {
  OP_SAVE: direct("save"),
  OP_RESTORE: direct("restore"),
  OP_BEGIN_PATH: direct("beginPath"),
  OP_CLOSE_PATH: direct("closePath"),
  OP_MOVE_TO: direct("moveTo"),
  OP_LINE_TO: direct("lineTo"),
  OP_RECT: direct("rect"),
  OP_ARC: direct("arc"),
  OP_ARC_ACW: direct("arc"),
  OP_ELLIPSE: direct("ellipse"),
  OP_QUAD_TO: direct("quadraticCurveTo"),
  OP_BEZIER_TO: direct("bezierCurveTo"),
  OP_FILL: direct("fill"),
  OP_STROKE: direct("stroke"),
  OP_CLIP: direct("clip"),
  OP_FILL_RECT: direct("fillRect"),
  OP_STROKE_RECT: direct("strokeRect"),
  OP_FILL_TEXT: direct("fillText"),
  OP_STROKE_TEXT: direct("strokeText"),
  OP_SET_FONT: setter("font"),
  OP_SET_TEXT_ALIGN: setter("textAlign"),
  OP_SET_TEXT_BASELINE: setter("textBaseline"),
  OP_SET_FILL_STYLE: setter("fillStyle"),
  OP_SET_STROKE_STYLE: setter("strokeStyle"),
  OP_SET_LINE_WIDTH: setter("lineWidth"),
  OP_SET_GLOBAL_ALPHA: setter("globalAlpha"),
  OP_SET_SHADOW_COLOR: setter("shadowColor"),
  OP_SET_SHADOW_OFFSET_X: setter("shadowOffsetX"),
  OP_SET_SHADOW_OFFSET_Y: setter("shadowOffsetY"),
  OP_SET_SHADOW_BLUR: setter("shadowBlur"),
  OP_TRANSLATE: direct("translate"),
  OP_ROTATE: direct("rotate"),
  OP_SCALE: direct("scale"),
  OP_SET_TRANSFORM: direct("setTransform"),
  OP_DRAW_IMAGE: ([key, dx, dy, dw, dh]) => {
    const k = str(key);
    const [x, y, w, h] = [num(dx), num(dy), num(dw), num(dh)];
    // The EMF raster cache hears about every `emf-pic:` draw, hit or miss, first.
    const notify = k.startsWith("emf-pic:") ? [call("onImageDraw", k, w, h)] : [];
    const bitmap = IMAGES.get(k);
    return [...notify, ...(bitmap ? [call("drawImage", bitmap, x, y, w, h)] : placeholder(x, y, w, h))];
  },
  OP_SET_LINEAR_GRADIENT: (args) => [
    call("createLinearGradient", ...args.slice(0, 4).map(num)),
    ...colorStops(args.slice(4)),
    call("set fillStyle", expect.objectContaining({ kind: "linear" })),
  ],
  OP_SET_RADIAL_GRADIENT: (args) => [
    call("createRadialGradient", ...args.slice(0, 6).map(num)),
    ...colorStops(args.slice(6)),
    call("set fillStyle", expect.objectContaining({ kind: "radial" })),
  ],
  OP_SET_PATTERN_FILL: ([key, fg, bg, size, count, ...prims]) => {
    const [k, f, b, s] = [str(key), str(fg), str(bg), num(size)];
    if (seenPatterns.has(k)) return [call("set fillStyle", PATTERN)];
    seenPatterns.add(k);
    // Re-rasterise the tile as the Rust `CanvasPainter::set_pattern_fill` does:
    // flood the background, then stamp each primitive in the foreground.
    const stamps: Call[] = [];
    let at = 0;
    for (let p = 0; p < num(count); p++) {
      const tag = num(prims[at++]);
      const next = () => num(prims[at++]);
      if (tag === golden.tileTags.TILE_LINE) {
        const [x1, y1, x2, y2, lw] = [next(), next(), next(), next(), next()];
        stamps.push(
          call("tile.set lineWidth", lw),
          call("tile.beginPath"),
          call("tile.moveTo", x1, y1),
          call("tile.lineTo", x2, y2),
          call("tile.stroke"),
        );
      } else {
        expect(tag).toBe(golden.tileTags.TILE_DOT);
        const [x, y, side] = [next(), next(), next()];
        stamps.push(call("tile.fillRect", x, y, side, side));
      }
    }
    expect(at).toBe(prims.length);
    return [
      call("document.createElement", "canvas"),
      call("canvas.set width", s),
      call("canvas.set height", s),
      call("canvas.getContext", "2d"),
      call("tile.set fillStyle", b),
      call("tile.fillRect", 0, 0, s, s),
      call("tile.set strokeStyle", f),
      call("tile.set fillStyle", f),
      call("tile.set lineCap", "butt"),
      ...stamps,
      call("createPattern", expect.anything(), "repeat"),
      call("set fillStyle", PATTERN),
    ];
  },
  OP_SET_LINE_DASH: ([count, ...segs]) => {
    expect(segs).toHaveLength(num(count));
    return [call("setLineDash", segs.map(num))];
  },
  OP_CLEAR_LINE_DASH: () => [call("setLineDash", [])],
  OP_SET_FONT_KERNING: ([on]) => [call("set fontKerning", bool(on) ? "normal" : "none")],
};

const runReplay = (stream: number[]): void =>
  replayDisplayList(makeCtx(), new Float64Array(stream), golden.strings, IMAGES, (key, dw, dh) =>
    log.push(call("onImageDraw", key, dw, dh)),
  );

/** Exported constants of replay.ts whose names start with `prefix`. */
const exported = (prefix: string): Record<string, unknown> =>
  Object.fromEntries(Object.entries(replay).filter(([name]) => name.startsWith(prefix)));

describe("display-list wire golden", () => {
  it("replay.ts opcode and tile-tag constants equal the Rust encoder's, with none missing on either side", () => {
    expect(exported("OP_")).toEqual(golden.opcodes);
    expect(exported("TILE_")).toEqual(golden.tileTags);
  });

  it("this test states the calls for every opcode in the golden", () => {
    expect(Object.keys(EXPECT).sort()).toEqual(Object.keys(golden.opcodes).sort());
  });

  it("replays the golden stream into exactly the calls its decoded list implies, command by command", () => {
    seenPatterns = new Set();
    const expected: Call[] = [];
    const owner: number[] = [];
    golden.decoded.forEach((cmd, n) => {
      const expectFor = EXPECT[cmd.op];
      if (!expectFor) throw new Error(`no expected calls stated for ${cmd.op}`);
      for (const c of expectFor(cmd.args)) {
        expected.push(c);
        owner.push(n);
      }
    });

    runReplay(golden.stream);

    for (let i = 0; i < Math.max(log.length, expected.length); i++) {
      const n = owner[Math.min(i, owner.length - 1)]!;
      expect(log[i], `call ${i}, replaying command #${n} ${JSON.stringify(golden.decoded[n])}`).toEqual(expected[i]);
    }
  });

  it("ends on a frame boundary exactly at the end of the stream", () => {
    // An opcode the contract lacks, placed right after the last frame. A decoder
    // that read one argument too many swallows it and returns quietly; one that
    // read too few throws earlier. Only an exact decoder reaches it as an opcode.
    const end = golden.stream.length;
    expect(() => runReplay([...golden.stream, -1])).toThrow(`unknown opcode -1 at ${end}`);
  });
});
