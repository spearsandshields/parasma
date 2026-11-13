/**
 * plugin-clipboard. Owns the copy MODEL + decision: it
 * caches the current text selection off `selection:changed`, resolves the
 * flavor-neutral DocFragment ONCE per copy (`viewer.extractRange`, memoized
 * across N providers), serializes each registered flavor, and is the SOLE
 * emitter of `clipboard:copied` / `clipboard:error`. The DOM adapter performs
 * the actual `navigator.clipboard` write inside the user-gesture window (the plugin/DOM
 * boundary) — this plugin never touches the DOM or clipboard.
 *
 * Staleness: the bus `seq` (plugin-selection's `gestureSeq`) is the
 * SINGLE source. A debounced pre-serialization cache is stamped with it and
 * dropped when superseded, so a drag-end→instant-Ctrl+C never serializes the
 * PREVIOUS selection. deps: ["selection"].
 */
import type { CellFragment, CellRange, ObjectRef, PmlTargetRef, TextPos } from "@parasma/core";
import type { ClipboardViewer } from "./ports";
import type { Plugin } from "../registry";
import type { KernelContext } from "../kernel-context";
import {
  DEFAULT_BUDGETS,
  type CopyBudgets,
  type CopyContext,
  type FlavorMime,
  type FlavorProvider,
  type PngEncoder,
  type SelectionKind,
} from "./flavors/types";
import { builtinFlavorProviders } from "./flavors/registry";
import { serializeTextPlain } from "./flavors/text";
import { serializeTextHtml } from "./flavors/html";
import { serializeCellsHtml, serializeCellsTsv } from "./flavors/sml";
import { defaultPngEncoder } from "./flavors/image";

/**
 * Commands OWNED by this plugin. Declaration-merged into the open
 * {@link CommandMap} (targeting `../commands`, the barrel-re-exported kernel
 * module → program-wide) so `exec(id, …)` is arg/return type-checked and an
 * undeclared id fails to register. Signatures authored from the handler bodies.
 */
declare module "../commands" {
  interface CommandMap {
    "clipboard.canCopy": () => boolean;
    "clipboard.currentSeq": () => number;
    "clipboard.cached": (seq: number) => CachedCopy | null;
    "clipboard.plan": () => CopyPlan | null;
    "clipboard.copy": () => Promise<CopyOutcome | null>;
    "clipboard.serialize": () => Promise<CachedCopy | null>;
    "clipboard.extractSyncInProcess": () => CachedCopy | null;
    "clipboard.registerFlavor": (p: FlavorProvider) => void;
    "clipboard.reportCopied": (arg: { seq: number; charCount: number }) => void;
    "clipboard.reportError": (arg: { reason: string }) => void;
  }
}

/**
 * Bus events OWNED by this plugin (declared here, not in the kernel's
 * bus.ts). Declaration-merged into the open {@link BusEventMap} so adding a
 * plugin event no longer edits the kernel. The `../bus` path and `../commands`
 * path below both resolve to the same kernel modules the barrel re-exports,
 * so the augmentation is program-wide for every consumer.
 */
declare module "../bus" {
  interface BusEventMap {
    /**
     * A copy landed on the clipboard (plugin-clipboard). The
     * plugin is the SOLE emitter (the DOM adapter performs the write but emits
     * nothing itself). `kind` is the selection kind ("wml-text"), `mimes` the
     * flavors actually written (a failing provider omits its mime),
     * `charCount` the text/plain length (for a "Copied N chars" toast). Policy
     * "none" — a copy fires once; a late subscriber must not replay a stale copy.
     */
    "clipboard:copied": {
      kind: string;
      mimes: string[];
      charCount: number;
      /**
       * The SML copy cap signal. `truncated` is true when
       * the copy hit the row/byte cap; `rowCount`/`totalRows` drive the "Copied
       * first N of M rows" flash. Absent (undefined) for non-truncated / non-SML
       * copies.
       */
      truncated?: boolean;
      rowCount?: number;
      totalRows?: number;
    };
    /**
     * A copy could not be completed (plugin-clipboard): every
     * flavor provider returned null/failed, or the tier-3 fallback could not
     * serialize (worker mode, stale cache — never write wrong content).
     * Policy "none".
     */
    "clipboard:error": { reason: string };
  }
}

/** The cached current selection: a text range, a selected image object, a
 *  selected CHART object (WML inline OR SML grid, split by the
 *  key namespace so `clipboard:copied` reports the honest kind), OR an SML
 *  cells range. */
type CopySel =
  | { kind: "wml-text"; start: TextPos; end: TextPos }
  | { kind: "wml-object-image"; ref: ObjectRef }
  | { kind: "wml-object-chart"; ref: ObjectRef }
  | { kind: "sml-object-chart"; ref: ObjectRef }
  | { kind: "sml-range"; sheet: number; range: CellRange }
  // A PML text span (one shape/cell) + a PML picture object.
  // The picture reuses the WML image flavors (bytes/bitmaps are keyed by the
  // image key for both); the text reuses the WML text/plain + text/html flavors.
  | { kind: "pml-text"; slide: number; target: PmlTargetRef; start: TextPos; end: TextPos }
  | { kind: "pml-object-image"; ref: ObjectRef };

/** An empty cells fragment (a non-SML selection / an unresolvable extract). */
const EMPTY_CELL_FRAG: CellFragment = {
  rows: [],
  rowCount: 0,
  totalRows: 0,
  truncated: false,
  reason: null,
};

/** The SML copy cap signal, captured when a cells fragment resolves. */
interface CellCap {
  truncated: boolean;
  rowCount: number;
  totalRows: number;
}

/**
 * A pre-serialized text snapshot stamped with the selection seq. Carries
 * BOTH text flavors (the copy budgets permit the text-sized, image-free HTML skeleton in the
 * sync cache) so the DOM adapter's `setData` tier can write text/html too.
 */
export interface CachedCopy {
  seq: number;
  plain: string;
  html: string;
}

/** The resolved outcome of a copy: the metadata PLUS the serialized flavors. */
export interface CopyOutcome {
  kind: SelectionKind;
  mimes: FlavorMime[];
  charCount: number;
  seq: number;
  /** The serialized flavor payloads (what the adapter writes to the OS). */
  flavors: Partial<Record<FlavorMime, string | Blob>>;
}

/**
 * A SYNCHRONOUS copy plan (memoized). `flavor(mime)` returns a lazy promise (the
 * memoized extractRange fires only when awaited); `settled` resolves once all
 * flavors land, emits the bus event, and returns the outcome. Building the plan
 * does NO async work (Safari-gesture safe: the adapter can build a ClipboardItem
 * synchronously from these promises).
 */
export interface CopyPlan {
  kind: SelectionKind;
  mimes: FlavorMime[];
  seq: number;
  flavor(mime: FlavorMime): Promise<string | Blob | null>;
  settled: Promise<CopyOutcome | null>;
}

/** Lexicographic (para, byte) order of two positions. */
function order(a: TextPos, b: TextPos): [TextPos, TextPos] {
  if (a.para < b.para || (a.para === b.para && a.byte <= b.byte)) return [a, b];
  return [b, a];
}

export function clipboardPlugin(viewer: ClipboardViewer, opts?: { encodePng?: PngEncoder }): Plugin {
  const unsubs: Array<() => void> = [];
  const providers: FlavorProvider[] = builtinFlavorProviders();
  const budgets: CopyBudgets = DEFAULT_BUDGETS;
  const encodePng: PngEncoder = opts?.encodePng ?? defaultPngEncoder;

  let sel: CopySel | null = null;
  let lastSeq = 0;
  let cache: CachedCopy | null = null;
  // The cap of the last-resolved cells fragment, set
  // whenever `cellFragment()`/`serializeNow` resolves an SML extract. Read by
  // `reportCopied` to carry "Copied first N of M rows" on `clipboard:copied`.
  let lastCellCap: CellCap | null = null;
  let primeTimer: ReturnType<typeof setTimeout> | null = null;
  const PRIME_MS = 150;

  const canCopy = (): boolean => sel !== null;

  /** A per-copy context: MEMOIZED fragment + object (one read each) + the
   *  image seams. `fragment()` is empty for an object selection; `object()` is
   *  null for a text selection. */
  const makeContext = (s: CopySel): CopyContext => {
    let frag: Promise<import("@parasma/core").DocFragment> | undefined;
    let cellFrag: Promise<CellFragment> | undefined;
    let obj: Promise<import("./flavors/types").ObjectImageInfo | null> | undefined;
    let chart: Promise<import("@parasma/core").ChartCopyData | null> | undefined;
    return {
      // Every extract is the ONE `viewer.extractRange(DocSelection)`
      // verb; the CopySel kind only picks which DocSelection to build (and
      // which fragment schema comes back).
      fragment: () =>
        (frag ??=
          s.kind === "wml-text"
            ? viewer
                .extractRange({ kind: "text", start: s.start, end: s.end })
                .then((f) => f ?? { blocks: [] })
            : s.kind === "pml-text"
              ? viewer
                  .extractRange({ kind: "pmlText", slide: s.slide, target: s.target, start: s.start, end: s.end })
                  .then((f) => f ?? { blocks: [] })
              : Promise.resolve({ blocks: [] })),
      cellFragment: () =>
        (cellFrag ??=
          s.kind === "sml-range"
            ? viewer.extractRange({ kind: "cells", sheet: s.sheet, range: s.range }).then((f) => {
                const ff = f ?? EMPTY_CELL_FRAG;
                lastCellCap = {
                  truncated: ff.truncated,
                  rowCount: ff.rowCount,
                  totalRows: ff.totalRows,
                };
                return ff;
              })
            : Promise.resolve(EMPTY_CELL_FRAG)),
      budgets,
      object: () =>
        (obj ??=
          s.kind === "wml-object-image" || s.kind === "pml-object-image"
            ? viewer.objectInfo(s.ref)
            : Promise.resolve(null)),
      imageOriginal: (relId: string) => viewer.imageOriginal(relId),
      imageBitmap: (relId: string) => viewer.imageBitmapFor(relId),
      encodePng: (bmp: ImageBitmap) => encodePng(bmp),
      // The chart seams (WML inline + SML grid charts:
      // the SAME seams, keyed by the format-namespaced chart key). `chart()`
      // is memoized like `object()` (one chartData read per copy); the raster
      // seam is a direct pass-through (worker mode returns false → clean
      // skip).
      chartKey:
        s.kind === "wml-object-chart" || s.kind === "sml-object-chart" ? s.ref.key : null,
      chart: () =>
        (chart ??=
          s.kind === "wml-object-chart" || s.kind === "sml-object-chart"
            ? viewer.chartData(s.ref.key)
            : Promise.resolve(null)),
      renderChartToCanvas: (key, c2d, wPx, hPx, scale) =>
        viewer.renderChartToCanvas(key, c2d, wPx, hPx, scale),
    };
  };

  /** Serialize the text flavors for a TEXT or SML-CELLS selection WITHOUT emitting
   *  (priming). Returns null for an object selection (no image base64 in the
   *  cache). SML cells cache the TSV + HTML `<table>` (text-only). */
  const serializeNow = async (): Promise<CachedCopy | null> => {
    if (!sel) return null;
    const seq = lastSeq;
    if (sel.kind === "wml-text") {
      const frag = (await viewer.extractRange({ kind: "text", start: sel.start, end: sel.end })) ?? { blocks: [] };
      return { seq, plain: serializeTextPlain(frag), html: serializeTextHtml(frag, budgets) };
    }
    if (sel.kind === "sml-range") {
      const f =
        (await viewer.extractRange({ kind: "cells", sheet: sel.sheet, range: sel.range })) ?? EMPTY_CELL_FRAG;
      lastCellCap = { truncated: f.truncated, rowCount: f.rowCount, totalRows: f.totalRows };
      return { seq, plain: serializeCellsTsv(f), html: serializeCellsHtml(f) };
    }
    if (sel.kind === "pml-text") {
      const frag =
        (await viewer.extractRange({ kind: "pmlText", slide: sel.slide, target: sel.target, start: sel.start, end: sel.end })) ??
        { blocks: [] };
      return { seq, plain: serializeTextPlain(frag), html: serializeTextHtml(frag, budgets) };
    }
    return null; // object selection
  };

  /** Debounced pre-serialization; drops the result if superseded. */
  const armPrime = () => {
    if (primeTimer) clearTimeout(primeTimer);
    primeTimer = setTimeout(() => {
      primeTimer = null;
      if (!canCopy()) return;
      void serializeNow().then((c) => {
        if (c && c.seq === lastSeq) cache = c; // fresh only
      });
    }, PRIME_MS);
  };

  return {
    id: "clipboard",
    deps: ["selection"],

    initialize(ctx: KernelContext) {
      /** Build the plan; each flavor promise wrapped so one failure omits only
       * its mime, and the memoized context shares ONE extractRange. */
      const buildPlan = (): CopyPlan | null => {
        if (!sel) return null;
        const kind = sel.kind;
        const seq = lastSeq;
        const c = makeContext(sel);
        const active = providers.filter((p) => p.appliesTo(kind));
        const memo = new Map<FlavorMime, Promise<string | Blob | null>>();
        const flavor = (mime: FlavorMime): Promise<string | Blob | null> => {
          let pr = memo.get(mime);
          if (!pr) {
            const prov = active.find((p) => p.mime === mime);
            pr = prov ? prov.provide(kind, c).catch(() => null) : Promise.resolve(null);
            memo.set(mime, pr);
          }
          return pr;
        };
        const finish = (entries: Array<readonly [FlavorMime, string | Blob | null]>): CopyOutcome | null => {
          const flavors: Partial<Record<FlavorMime, string | Blob>> = {};
          let charCount = 0;
          for (const [mime, v] of entries) {
            if (v == null) continue;
            flavors[mime] = v;
            if (mime === "text/plain" && typeof v === "string") charCount = v.length;
          }
          const mimes = Object.keys(flavors) as FlavorMime[];
          // The plugin does NOT emit clipboard:copied/error at SERIALIZE
          // time. "Copied"/"error" must reflect the ACTUAL OS write
          // outcome, which only the DOM adapter observes; it drives the
          // clipboard.reportCopied / clipboard.reportError commands (registered
          // below) AFTER the write resolves. finish() just resolves the plan;
          // an all-null serialization returns null (the adapter reports error).
          if (mimes.length === 0) return null;
          return { kind, mimes, charCount, seq, flavors };
        };
        // `settled` is LAZY: building a plan does NO async work (no extractRange)
        // until someone awaits a flavor or reads `.settled` — so `clipboard.plan`
        // is synchronous and side-effect-free (the Safari-gesture contract).
        let settledPromise: Promise<CopyOutcome | null> | undefined;
        return {
          kind,
          mimes: active.map((p) => p.mime),
          seq,
          flavor,
          get settled(): Promise<CopyOutcome | null> {
            return (settledPromise ??= Promise.all(
              active.map((p) => flavor(p.mime).then((v) => [p.mime, v] as const)),
            ).then(finish));
          },
        };
      };

      unsubs.push(
        ctx.bus.subscribe("selection:changed", (e) => {
          lastSeq = e.seq;
          if (e.kind === "text" && !e.collapsed && e.anchor && e.focus) {
            const [start, end] = order(e.anchor, e.focus);
            sel = { kind: "wml-text", start, end };
            armPrime();
          } else if (e.kind === "cells" && e.sheet != null && e.range) {
            // An SML cells range. Prime the TSV/HTML text cache
            // (text-only); the cap is captured when the extract resolves.
            sel = { kind: "sml-range", sheet: e.sheet, range: e.range };
            lastCellCap = null;
            armPrime();
          } else if (e.kind === "pmlText" && !e.collapsed && e.pml) {
            // A PML text span (one shape/cell). Prime the text
            // cache (reuses the WML text flavors on the shared DocFragment).
            const p = e.pml;
            const [start, end] = order(p.anchor, p.focus);
            sel = {
              kind: "pml-text",
              slide: p.slide,
              target: { targetKind: p.targetKind, targetA: p.targetA, targetB: p.targetB },
              start,
              end,
            };
            armPrime();
          } else if (e.kind === "object" && e.object) {
            // NEVER prime/cache image base64; the data-URL is built only at
            // actual copy time on the primary path. A PML picture (kind:"picture")
            // routes to the pml-object-image kind (same image flavors); a chart
            // (kind:"chart") routes to the chart flavors (png raster
            // + html data-URL + data-TSV — all copy-time), split WML/SML on the
            // key namespace (`chart::sml::…` = an xlsx grid chart) so the copied
            // kind is honest — the flavors themselves are IDENTICAL.
            sel =
              e.object.kind === "picture"
                ? { kind: "pml-object-image", ref: e.object }
                : e.object.kind === "chart"
                  ? e.object.key.startsWith("chart::sml::")
                    ? { kind: "sml-object-chart", ref: e.object }
                    : { kind: "wml-object-chart", ref: e.object }
                  : { kind: "wml-object-image", ref: e.object };
            cache = null;
          } else {
            sel = null;
            cache = null;
            lastCellCap = null;
          }
        }),
      );

      // Sync reads (the adapter tracks its own copies of these off the bus; these
      // exist for tests + non-adapter consumers).
      unsubs.push(
        ctx.commands.register("clipboard.canCopy", (): boolean => canCopy()),
        ctx.commands.register("clipboard.currentSeq", (): number => lastSeq),
        ctx.commands.register("clipboard.cached", (seq: number): CachedCopy | null =>
          cache && cache.seq === seq ? cache : null,
        ),
        ctx.commands.register("clipboard.plan", (): CopyPlan | null => buildPlan()),

        // The primary copy: serialize every flavor, emit, return the outcome (the
        // adapter derives its OS-write blob from `flavors`).
        ctx.commands.register("clipboard.copy", async (): Promise<CopyOutcome | null> => {
          const plan = buildPlan();
          return plan ? plan.settled : null;
        }),

        // Non-emitting serialize (primes the adapter's sync fallback cache).
        ctx.commands.register("clipboard.serialize", (): Promise<CachedCopy | null> => serializeNow()),

        // Tier-3 sync escape hatch (in-process only; null in worker mode).
        // TEXT-only — an object selection has no sync text flavor (image/png rides
        // the async ClipboardItem tier).
        ctx.commands.register("clipboard.extractSyncInProcess", (): CachedCopy | null => {
        if (!sel) return null;
        if (sel.kind === "wml-text") {
          const f = viewer.extractRangeSyncInProcess({ kind: "text", start: sel.start, end: sel.end });
          return f
            ? { seq: lastSeq, plain: serializeTextPlain(f), html: serializeTextHtml(f, budgets) }
            : null;
        }
        if (sel.kind === "sml-range") {
          const f = viewer.extractRangeSyncInProcess({ kind: "cells", sheet: sel.sheet, range: sel.range });
          if (!f) return null;
          lastCellCap = { truncated: f.truncated, rowCount: f.rowCount, totalRows: f.totalRows };
          return { seq: lastSeq, plain: serializeCellsTsv(f), html: serializeCellsHtml(f) };
        }
        if (sel.kind === "pml-text") {
          const f = viewer.extractRangeSyncInProcess({
            kind: "pmlText",
            slide: sel.slide,
            target: sel.target,
            start: sel.start,
            end: sel.end,
          });
          return f
            ? { seq: lastSeq, plain: serializeTextPlain(f), html: serializeTextHtml(f, budgets) }
            : null;
        }
          return null; // object selection
        }),

        // Registry drop-in for additional flavor providers.
        ctx.commands.register("clipboard.registerFlavor", (p: FlavorProvider): void => {
          providers.push(p);
        }),

      // Actual-write-outcome signal. The plugin stays the SOLE
      // bus emitter, but the DOM adapter is the only piece that knows
      // whether the OS write LANDED (on ALL tiers, incl. tier-3 setData). It
      // calls these fire-and-forget commands AFTER the write resolves / rejects,
      // so the "Copied N chars" flash reflects reality. `kind`/`mimes` are the
      // current provider set (informational — the flash consumer reads charCount).
        ctx.commands.register("clipboard.reportCopied", (arg: { seq: number; charCount: number }): void => {
          const kind: SelectionKind = sel?.kind ?? "wml-text";
          const mimes = providers.filter((p) => p.appliesTo(kind)).map((p) => p.mime);
          // Carry the SML copy cap so a truncated copy can
          // flash "Copied first N of M rows" instead of a silent clip.
          const cap =
            kind === "sml-range" && lastCellCap
              ? {
                  truncated: lastCellCap.truncated,
                  rowCount: lastCellCap.rowCount,
                  totalRows: lastCellCap.totalRows,
                }
              : {};
          ctx.bus.emit("clipboard:copied", { kind, mimes, charCount: arg.charCount, ...cap });
        }),
        ctx.commands.register("clipboard.reportError", (arg: { reason: string }): void => {
          ctx.bus.emit("clipboard:error", { reason: arg.reason });
        }),
      );
    },

    destroy(_ctx: KernelContext) {
      if (primeTimer) clearTimeout(primeTimer);
      primeTimer = null;
      for (const u of unsubs.splice(0)) u();
      sel = null;
      cache = null;
    },
  };
}
