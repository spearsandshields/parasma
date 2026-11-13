/**
 * The TS replay decoder for the five PML style opcodes (display-list
 * fidelity): OP_SET_LINEAR_GRADIENT(35), OP_SET_RADIAL_GRADIENT(36),
 * OP_SET_PATTERN_FILL(37), OP_SET_LINE_DASH(38), OP_CLEAR_LINE_DASH(39). This
 * pins the decoder to the SAME op→arity table the Rust reference decoder walks
 * (engine/displaylist.rs), and to the pattern-tile rasterisation + cache
 * behaviour the in-process `CanvasPainter` produces — so a worker-recorded slide
 * replays with REAL gradients/patterns/dashes, not solid approximations.
 *
 * The vitest env is `node` (no DOM), so we feed a recording-ctx stub that
 * captures the exact call sequence + args, and a `document.createElement("canvas")`
 * stub whose 2D context records the tile stamping. Each test states its RED
 * condition (what a broken decoder would do).
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { replayDisplayList } from "./replay";

// ── Opcode + tile-tag mirror (engine/displaylist.rs contract) ────────────────-
const OP_SET_LINEAR_GRADIENT = 35;
const OP_SET_RADIAL_GRADIENT = 36;
const OP_SET_PATTERN_FILL = 37;
const OP_SET_LINE_DASH = 38;
const OP_CLEAR_LINE_DASH = 39;
const TILE_LINE = 0;
const TILE_DOT = 1;

/** A recorded call: the method/property name + its args (or the assigned value). */
type Call = { op: string; args: unknown[] };

/** A stub CanvasGradient that records its addColorStop calls. */
function makeGradient(kind: string, args: number[], log: Call[]): unknown {
  return {
    __kind: kind,
    __args: args,
    addColorStop: (pos: number, color: string) => log.push({ op: "addColorStop", args: [pos, color] }),
  };
}

/** A stub 2D context recording every op we exercise. `fillStyle` is a setter. */
function makeCtx(log: Call[]): CanvasRenderingContext2D {
  let fillStyle: unknown = "";
  const ctx = {
    createLinearGradient: (x0: number, y0: number, x1: number, y1: number) => {
      log.push({ op: "createLinearGradient", args: [x0, y0, x1, y1] });
      return makeGradient("linear", [x0, y0, x1, y1], log);
    },
    createRadialGradient: (x0: number, y0: number, r0: number, x1: number, y1: number, r1: number) => {
      log.push({ op: "createRadialGradient", args: [x0, y0, r0, x1, y1, r1] });
      return makeGradient("radial", [x0, y0, r0, x1, y1, r1], log);
    },
    createPattern: (image: unknown, repetition: string) => {
      log.push({ op: "createPattern", args: [image, repetition] });
      return { __pattern: true };
    },
    setLineDash: (segs: number[]) => log.push({ op: "setLineDash", args: [segs] }),
    set fillStyle(v: unknown) {
      fillStyle = v;
      log.push({ op: "set fillStyle", args: [v] });
    },
    get fillStyle() {
      return fillStyle;
    },
  };
  return ctx as unknown as CanvasRenderingContext2D;
}

/** Count how many offscreen tile canvases got created (createElement calls). */
let tileCanvasCreations = 0;

beforeEach(() => {
  tileCanvasCreations = 0;
  const doc = {
    createElement: (tag: string) => {
      if (tag !== "canvas") throw new Error(`unexpected createElement(${tag})`);
      tileCanvasCreations++;
      let width = 0;
      let height = 0;
      const tctx = {
        set fillStyle(_v: unknown) {},
        get fillStyle() {
          return "";
        },
        set strokeStyle(_v: unknown) {},
        set lineCap(_v: unknown) {},
        set lineWidth(_v: unknown) {},
        fillRect: () => {},
        beginPath: () => {},
        moveTo: () => {},
        lineTo: () => {},
        stroke: () => {},
      };
      return {
        set width(v: number) {
          width = v;
        },
        get width() {
          return width;
        },
        set height(v: number) {
          height = v;
        },
        get height() {
          return height;
        },
        getContext: (kind: string) => (kind === "2d" ? tctx : null),
      };
    },
  };
  vi.stubGlobal("document", doc);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

/** Build a `{ opcodes, strings }` frame from a builder that appends ops + interns strings. */
function frame(build: (push: (...n: number[]) => void, intern: (s: string) => number) => void): {
  opcodes: Float64Array;
  strings: string[];
} {
  const ops: number[] = [];
  const strings: string[] = [];
  const table = new Map<string, number>();
  const intern = (s: string): number => {
    let idx = table.get(s);
    if (idx === undefined) {
      idx = strings.length;
      strings.push(s);
      table.set(s, idx);
    }
    return idx;
  };
  build((...n) => ops.push(...n), intern);
  return { opcodes: new Float64Array(ops), strings };
}

describe("replay — PML style opcodes", () => {
  it("35 linear gradient: createLinearGradient(endpoints) + addColorStop×n + set fillStyle", () => {
    // RED: a decoder that mis-reads the count or arg order builds the wrong axis /
    // stops, or forgets to assign the gradient to fillStyle.
    const log: Call[] = [];
    const ctx = makeCtx(log);
    const { opcodes, strings } = frame((push, s) => {
      push(OP_SET_LINEAR_GRADIENT, 0, 0, 100, 50, 2);
      push(0.0, s("#ff0000"));
      push(1.0, s("#0000ff"));
    });
    replayDisplayList(ctx, opcodes, strings, new Map());
    expect(log).toEqual([
      { op: "createLinearGradient", args: [0, 0, 100, 50] },
      { op: "addColorStop", args: [0.0, "#ff0000"] },
      { op: "addColorStop", args: [1.0, "#0000ff"] },
      { op: "set fillStyle", args: [expect.objectContaining({ __kind: "linear" })] },
    ]);
  });

  it("35 linear gradient with n=0 stops: no addColorStop, still assigns fillStyle", () => {
    // RED: an off-by-one count guard reads a phantom stop past the frame.
    const log: Call[] = [];
    const ctx = makeCtx(log);
    const { opcodes, strings } = frame((push) => {
      push(OP_SET_LINEAR_GRADIENT, 1, 2, 3, 4, 0);
    });
    replayDisplayList(ctx, opcodes, strings, new Map());
    expect(log.map((c) => c.op)).toEqual(["createLinearGradient", "set fillStyle"]);
    expect(log[0]!.args).toEqual([1, 2, 3, 4]);
  });

  it("36 radial gradient: createRadialGradient(6 args) + addColorStop×n + set fillStyle", () => {
    const log: Call[] = [];
    const ctx = makeCtx(log);
    const { opcodes, strings } = frame((push, s) => {
      push(OP_SET_RADIAL_GRADIENT, 10, 10, 0, 10, 10, 40, 3);
      push(0.0, s("#ffffff"));
      push(0.5, s("#808080"));
      push(1.0, s("#000000"));
    });
    replayDisplayList(ctx, opcodes, strings, new Map());
    expect(log).toEqual([
      { op: "createRadialGradient", args: [10, 10, 0, 10, 10, 40] },
      { op: "addColorStop", args: [0.0, "#ffffff"] },
      { op: "addColorStop", args: [0.5, "#808080"] },
      { op: "addColorStop", args: [1.0, "#000000"] },
      { op: "set fillStyle", args: [expect.objectContaining({ __kind: "radial" })] },
    ]);
  });

  it("37 pattern fill: rasterises the tile ONCE per key + sets the pattern; second key reuses cache", () => {
    // RED: no cache → a second createElement per identical key (the crux
    // regression); or the pattern never reaches fillStyle.
    const log: Call[] = [];
    const ctx = makeCtx(log);
    const emitPattern = (push: (...n: number[]) => void, s: (v: string) => number): void => {
      push(OP_SET_PATTERN_FILL, s("horz|#123456|#abcdef"), s("#123456"), s("#abcdef"), 8, 2);
      push(TILE_LINE, 0, 4, 8, 4, 1);
      push(TILE_DOT, 2, 2, 1);
    };
    const { opcodes, strings } = frame((push, s) => {
      emitPattern(push, s);
      emitPattern(push, s); // SAME key again → must reuse the cache
    });
    replayDisplayList(ctx, opcodes, strings, new Map());
    // Exactly one tile canvas + one createPattern for the two identical-key ops.
    expect(tileCanvasCreations).toBe(1);
    expect(log.filter((c) => c.op === "createPattern")).toHaveLength(1);
    // Both ops assign the SAME pattern object to fillStyle.
    const fills = log.filter((c) => c.op === "set fillStyle");
    expect(fills).toHaveLength(2);
    expect(fills[0]!.args[0]).toEqual({ __pattern: true });
    expect(fills[1]!.args[0]).toBe(fills[0]!.args[0]);
  });

  it("38 line dash: reads the count then n segments → setLineDash(segs)", () => {
    const log: Call[] = [];
    const ctx = makeCtx(log);
    const { opcodes, strings } = frame((push) => {
      push(OP_SET_LINE_DASH, 4, 4, 3, 1, 3);
    });
    replayDisplayList(ctx, opcodes, strings, new Map());
    expect(log).toEqual([{ op: "setLineDash", args: [[4, 3, 1, 3]] }]);
  });

  it("39 clear line dash → setLineDash([])", () => {
    // RED: a mis-decode would leave a stale dash on following strokes.
    const log: Call[] = [];
    const ctx = makeCtx(log);
    const { opcodes, strings } = frame((push) => {
      push(OP_CLEAR_LINE_DASH);
    });
    replayDisplayList(ctx, opcodes, strings, new Map());
    expect(log).toEqual([{ op: "setLineDash", args: [[]] }]);
  });
});
