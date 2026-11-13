/**
 * DEV-ONLY crispness diagnostic — measures whether WORKER-backed page render is
 * pixel-identical to IN-PROCESS render for text, across zoom × device-pixel-ratio.
 *
 * This is a MEASUREMENT, not a product change. It is wired only under `dev`
 * (mount.ts, alongside installDebugFonts), no-ops without the `?crispdiff=<path>`
 * query param, constructs its OWN two Viewers + own offscreen canvases, and NEVER
 * touches the product render path or the default snippet viewer. It tree-shakes
 * out of prod (the mount guard is `dev`-gated and the param is absent).
 *
 * What it measures (and what it does NOT): both paths end at the browser's native
 * fillText, via different routes — in-process paints directly through the wasm
 * painter (LocalPaintEngine.renderPageWindow); worker paints via recordPage
 * (logical 1:1 display list) + replayDisplayList with JS ctx.scale(zoom·dpr). This
 * harness diffs whether those two PAINTER routes produce a bit-identical canvas
 * BACKING STORE (getImageData, read pre-composite). It does NOT directly measure
 * the fractional-DPI softness: that crispness fix is NOT in the painter —
 * it lives in the SHARED, mode-agnostic viewport adapter (device-pixels.ts
 * snapLenToDevice / snapElementToDeviceGrid, which sizes+positions the page canvas
 * ELEMENT onto whole physical pixels regardless of which engine filled it). So the
 * gate argument is transitive: identical painter backing store (proven here) + the
 * same downstream snapping adapter ⇒ identical on-screen composite, i.e. worker
 * mode inherits the crispness fix unchanged. A direct end-to-end confirmation would
 * run at real --force-device-scale-factor=1.5 with DOM-attached canvases (deferred,
 * low-risk). `dpr` is an explicit numeric arg exercised by BOTH paths at the
 * rasterization level — see the honesty note logged at the end.
 */
import { Viewer } from "@parasma/core";

type Verdict = "IDENTICAL" | "COSMETIC" | "REGRESSION" | "CONFOUND" | "INVALID";

interface CellResult {
  zoom: number;
  dpr: number;
  W: number;
  H: number;
  spawns: number;
  fellBack: boolean;
  rpcRoundTrips: number;
  identPct: number;
  diffPx: number;
  maxDelta: number;
  edgePct: number;
  interiorPct: number;
  gradRatio: number;
  centroidShiftPx: number;
  inkA: boolean;
  inkB: boolean;
  verdict: Verdict;
  note: string;
}

const ZOOMS = [1.0, 1.5, 2.0];
const DPRS = [1.0, 1.5, 2.0];

export async function installCrispDiff(): Promise<void> {
  const fixture = new URLSearchParams(location.search).get("crispdiff");
  if (!fixture) return; // no-op: default execution mode provably unchanged

  try {
    await run(fixture);
  } catch (e) {
    // eslint-disable-next-line no-console
    console.error("[crispdiff] harness failed:", e);
  }
}

async function run(fixture: string): Promise<void> {
  // eslint-disable-next-line no-console
  const log = (...a: unknown[]): void => console.log("[crispdiff]", ...a);

  const url = `/fixtures/${fixture}`;
  const bytes = new Uint8Array(await (await fetch(url)).arrayBuffer());
  log(`fixture ${url} — ${bytes.byteLength} bytes`);

  // ── Prove-worker-ran layer 3: console.warn spy across the WHOLE run ──────────
  const realWarn = console.warn.bind(console);
  let fellBack = false;
  console.warn = (...args: unknown[]): void => {
    const s = args.map((x) => String(x)).join(" ");
    if (s.includes("falling back to in-process") || s.includes("render worker died")) {
      fellBack = true;
    }
    realWarn(...args);
  };

  // ── In-process viewer (path A) ──────────────────────────────────────────────
  const A = await Viewer.create();

  // ── Prove-worker-ran layer 2: Worker-constructor interceptor (install → build
  //    → restore immediately so the default snippet viewer is never perturbed) ──
  const RealWorker = window.Worker;
  let spawns = 0;
  let inst: Worker | null = null;
  window.Worker = class extends RealWorker {
    constructor(...a: ConstructorParameters<typeof Worker>) {
      super(...a);
      spawns++;
      inst = this;
    }
  } as typeof Worker;

  // ── Prove-worker-ran layer 1: EXPLICIT worker → fail-loud (re-throws on boot
  //    failure, can never silently return an in-process viewer) ────────────────
  let B: Viewer;
  try {
    B = await Viewer.createWorkerBacked({ worker: true });
  } finally {
    window.Worker = RealWorker;
  }
  log(`worker constructed — spawns=${spawns} instanceCaptured=${!!inst}`);

  // ── Prove-worker-ran layer 4: postMessage round-trip observation ────────────
  let rpcReq = 0;
  let rpcRes = 0; // messages received FROM worker (record→replay round-trips)
  if (inst) {
    const w = inst as Worker;
    const origPost = w.postMessage.bind(w) as Worker["postMessage"];
    w.postMessage = function (...args: unknown[]): void {
      rpcReq++;
      (origPost as (...a: unknown[]) => void)(...args);
    } as unknown as Worker["postMessage"];
    w.addEventListener("message", () => {
      rpcRes++;
    });
  }

  // ── Settle both loads (loadComplete replays late-subscribe → race-free) ──────
  // NOTE: on() may fire the handler SYNCHRONOUSLY during the on() call when the
  // load already completed (the core Viewer replays it), so the handler must not touch
  // `off` before it is assigned (temporal-dead-zone) — hence the null-guarded
  // late unsubscribe.
  const settled = (v: Viewer): Promise<void> =>
    new Promise<void>((res) => {
      let off: (() => void) | null = null;
      let fired = false;
      const handler = (): void => {
        fired = true;
        if (off) off();
        res();
      };
      off = v.on("loadComplete", handler);
      if (fired && off) off();
    });

  const infoA = await A.load(bytes.slice());
  const infoB = await B.load(bytes.slice());
  await settled(A);
  await settled(B);

  const { widthPx: pw, heightPx: ph } = infoA.pageSize;
  log(`loaded — page 0 size ${pw}×${ph} CSS px (worker pageSize ${infoB.pageSize.widthPx}×${infoB.pageSize.heightPx})`);

  // ── Overlay for screenshots ─────────────────────────────────────────────────
  const overlay = document.createElement("div");
  overlay.id = "crispdiff-overlay";
  overlay.setAttribute(
    "style",
    "position:fixed;inset:0;z-index:2147483647;overflow:auto;background:#101216;" +
      "color:#e6e4df;font:12px ui-monospace,monospace;padding:10px;",
  );
  const header = document.createElement("div");
  header.setAttribute("style", "padding:4px 0 10px;font-size:13px;");
  header.textContent = `crispdiff — ${fixture} — in-process | worker | diff×8`;
  overlay.appendChild(header);
  document.body.appendChild(overlay);

  const results: CellResult[] = [];

  for (const zoom of ZOOMS) {
    for (const dpr of DPRS) {
      const cell = await measureCell(A, B, pw, ph, zoom, dpr, spawns, () => fellBack, {
        before: rpcRes,
        get: () => rpcRes,
      });
      results.push(cell);

      log(
        `zoom=${zoom} dpr=${dpr} spawns=${cell.spawns} fellBack=${cell.fellBack} ` +
          `rpcRoundTrips=${cell.rpcRoundTrips} ink=${cell.inkA}/${cell.inkB} ` +
          `ident=${cell.identPct.toFixed(4)}% diffPx=${cell.diffPx} maxΔ=${cell.maxDelta} ` +
          `edge%=${cell.edgePct.toFixed(1)} interior%=${cell.interiorPct.toFixed(1)} ` +
          `gradRatio=${cell.gradRatio.toFixed(4)} centroidShiftPx=${cell.centroidShiftPx.toFixed(4)} ` +
          `verdict=${cell.verdict}${cell.note ? " (" + cell.note + ")" : ""}`,
      );

      mountCell(overlay, cell);
    }
  }

  // ── Summary + honesty note ──────────────────────────────────────────────────
  log("──── SUMMARY (zoom × dpr) ────");
  for (const r of results) {
    log(
      `  z${r.zoom}·d${r.dpr} → ${r.verdict}  ident=${r.identPct.toFixed(3)}%  ` +
        `diffPx=${r.diffPx}  maxΔ=${r.maxDelta}  gradRatio=${r.gradRatio.toFixed(3)}  ` +
        `centroidShift=${r.centroidShiftPx.toFixed(3)}px`,
    );
  }
  log(
    "DPR-COVERAGE NOTE: diffed numbers use the simulated dpr ARG (renderPageWindow's " +
      "explicit dpr param) and DO cover fractional 1.5; screenshots composite at the browser's " +
      "real window.devicePixelRatio (" +
      String(window.devicePixelRatio) +
      ") and are corroborating only — CDP deviceScaleFactor was NOT varied.",
  );
  log("DONE");

  // Restore the console.warn spy (leave the default viewer untouched).
  console.warn = realWarn;

  // Expose for the runner.
  (globalThis as { __crispdiff?: unknown }).__crispdiff = results;
}

async function measureCell(
  A: Viewer,
  B: Viewer,
  pw: number,
  ph: number,
  zoom: number,
  dpr: number,
  spawns: number,
  fellBack: () => boolean,
  rpc: { before: number; get: () => number },
): Promise<CellResult> {
  const W = Math.round(pw * zoom * dpr);
  const H = Math.round(ph * zoom * dpr);

  const base: CellResult = {
    zoom,
    dpr,
    W,
    H,
    spawns,
    fellBack: fellBack(),
    rpcRoundTrips: 0,
    identPct: 0,
    diffPx: 0,
    maxDelta: 0,
    edgePct: 0,
    interiorPct: 0,
    gradRatio: 0,
    centroidShiftPx: 0,
    inkA: false,
    inkB: false,
    verdict: "INVALID",
    note: "",
  };

  // Canvas-size guard.
  if (W > 16384 || H > 16384 || W * H > 120_000_000) {
    base.note = "canvas-cap exceeded";
    return base;
  }

  // ── IN-PROCESS render (path A) ──────────────────────────────────────────────
  const cA = document.createElement("canvas");
  cA.width = W;
  cA.height = H;
  const ctxA = cA.getContext("2d", { willReadFrequently: true });
  if (!ctxA) {
    base.note = "no 2d ctx (A)";
    return base;
  }
  whiteFill(ctxA, W, H);
  await A.renderPageWindow(0, cA, 0, 0, pw * zoom, ph * zoom, zoom, dpr);
  await afterPaint(cA);
  const imgA = ctxA.getImageData(0, 0, W, H);

  // ── WORKER render (path B) — measure RPC round-trips DURING this render ──────
  const rpcBefore = rpc.get();
  const cB = document.createElement("canvas");
  cB.width = W;
  cB.height = H;
  const ctxB = cB.getContext("2d", { willReadFrequently: true });
  if (!ctxB) {
    base.note = "no 2d ctx (B)";
    return base;
  }
  whiteFill(ctxB, W, H);
  await B.renderPageWindow(0, cB, 0, 0, pw * zoom, ph * zoom, zoom, dpr);
  await afterPaint(cB);
  const imgB = ctxB.getImageData(0, 0, W, H);
  const rpcRoundTrips = rpc.get() - rpcBefore;

  base.fellBack = fellBack();
  base.rpcRoundTrips = rpcRoundTrips;

  // ── Blank / ink guards ──────────────────────────────────────────────────────
  base.inkA = hasInk(imgA.data);
  base.inkB = hasInk(imgB.data);

  // ── Diff ────────────────────────────────────────────────────────────────────
  const d = diff(imgA, imgB, W, H);
  base.identPct = (100 * (W * H - d.diffPx)) / (W * H);
  base.diffPx = d.diffPx;
  base.maxDelta = d.maxDelta;
  base.edgePct = d.diffPx ? (100 * d.edgeDiff) / d.diffPx : 0;
  base.interiorPct = d.diffPx ? (100 * d.interiorDiff) / d.diffPx : 0;
  base.gradRatio = d.gradRatio;
  base.centroidShiftPx = d.centroidShift;

  // ── Verdict ─────────────────────────────────────────────────────────────────
  base.verdict = classify(base, d);

  // ── Build a small display triple + amplified heatmap; the big canvases fall
  //    out of scope after this call so they GC (bounds DOM memory across cells). ─
  attachDisplayTriple(base, cA, cB, d.heatmap, W, H);

  return base;
}

function classify(c: CellResult, d: DiffResult): Verdict {
  if (!c.inkA || !c.inkB) return "INVALID";
  if (c.spawns < 1 || c.fellBack || c.rpcRoundTrips < 1) return "INVALID";
  if (c.diffPx === 0) return "IDENTICAL";

  // CONFOUND triage FIRST: a global uniform channel offset (gamma/color-space
  // readback quirk) — nearly every differing px shares one signed offset — or a
  // whole-feature miss (a large block of interior differences forming a solid
  // region rather than glyph AA).
  if (d.uniformOffset) return "CONFOUND";

  // REGRESSION (the fractional-DPI softening class).
  const interiorFrac = c.diffPx ? d.interiorDiff / c.diffPx : 0;
  if (
    d.gradRatio < 0.95 ||
    Math.abs(c.centroidShiftPx) >= 0.25 ||
    interiorFrac > 0.02 // interior stroke px differ beyond negligible
  ) {
    return "REGRESSION";
  }

  // COSMETIC (benign AA jitter at glyph edges).
  if (
    c.maxDelta <= 24 &&
    d.gradRatio >= 0.98 &&
    d.gradRatio <= 1.02 &&
    Math.abs(c.centroidShiftPx) < 0.1 &&
    d.edgeDiff === c.diffPx
  ) {
    return "COSMETIC";
  }

  // Between cosmetic and regression: not benign-clean but no hard softening
  // signal — report COSMETIC only if edges dominate & deltas modest, else
  // REGRESSION.
  if (d.edgeDiff / c.diffPx > 0.9 && c.maxDelta <= 64 && d.gradRatio >= 0.95) {
    return "COSMETIC";
  }
  return "REGRESSION";
}

interface DiffResult {
  diffPx: number;
  maxDelta: number;
  edgeDiff: number;
  interiorDiff: number;
  gradRatio: number;
  centroidShift: number;
  uniformOffset: boolean;
  heatmap: ImageData;
}

function diff(imgA: ImageData, imgB: ImageData, W: number, H: number): DiffResult {
  const a = imgA.data;
  const b = imgB.data;
  const n = W * H;

  const lumA = new Float32Array(n);
  const lumB = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    const j = i * 4;
    lumA[i] = 0.299 * a[j] + 0.587 * a[j + 1] + 0.114 * a[j + 2];
    lumB[i] = 0.299 * b[j] + 0.587 * b[j + 1] + 0.114 * b[j + 2];
  }

  const heat = new Uint8ClampedArray(n * 4);
  let diffPx = 0;
  let maxDelta = 0;
  let edgeDiff = 0;
  let interiorDiff = 0;

  // For uniform-offset detection: track the signed per-channel delta histogram
  // (only over differing px). If one signed value dominates >90%, it's a global
  // offset (CONFOUND), not glyph AA.
  const signHist = new Map<number, number>();

  for (let i = 0; i < n; i++) {
    const j = i * 4;
    const dR = a[j] - b[j];
    const dG = a[j + 1] - b[j + 1];
    const dB = a[j + 2] - b[j + 2];
    const m = Math.max(Math.abs(dR), Math.abs(dG), Math.abs(dB));
    if (m > 0) {
      diffPx++;
      if (m > maxDelta) maxDelta = m;
      // classify edge vs interior via 3×3 luminance span in either image
      const span = neighborSpan(lumA, W, H, i) > 32 || neighborSpan(lumB, W, H, i) > 32;
      if (span) edgeDiff++;
      else interiorDiff++;
      const key = dR; // representative channel
      signHist.set(key, (signHist.get(key) ?? 0) + 1);
      // heatmap: amplify ×8, red
      const amp = Math.min(255, m * 8);
      heat[j] = 255;
      heat[j + 1] = 255 - amp;
      heat[j + 2] = 255 - amp;
      heat[j + 3] = 255;
    } else {
      heat[j] = 255;
      heat[j + 1] = 255;
      heat[j + 2] = 255;
      heat[j + 3] = 255;
    }
  }

  // uniform offset: one nonzero signed delta dominates the differing set
  let uniformOffset = false;
  if (diffPx > 1000) {
    let topKey = 0;
    let topCount = 0;
    for (const [k, ct] of signHist) {
      if (k !== 0 && ct > topCount) {
        topCount = ct;
        topKey = k;
      }
    }
    if (topKey !== 0 && topCount / diffPx > 0.9) uniformOffset = true;
  }

  // Sobel mean-magnitude ratio (softness discriminator).
  const gradA = sobelMean(lumA, W, H);
  const gradB = sobelMean(lumB, W, H);
  const gradRatio = gradA > 0 ? gradB / gradA : 1;

  // Column-centroid shift on the inkiest text row (sub-pixel misalignment).
  const centroidShift = rowCentroidShift(lumA, lumB, W, H);

  return {
    diffPx,
    maxDelta,
    edgeDiff,
    interiorDiff,
    gradRatio,
    centroidShift,
    uniformOffset,
    heatmap: new ImageData(heat, W, H),
  };
}

function neighborSpan(lum: Float32Array, W: number, H: number, i: number): number {
  const x = i % W;
  const y = (i - x) / W;
  let mn = Infinity;
  let mx = -Infinity;
  for (let dy = -1; dy <= 1; dy++) {
    const yy = y + dy;
    if (yy < 0 || yy >= H) continue;
    for (let dx = -1; dx <= 1; dx++) {
      const xx = x + dx;
      if (xx < 0 || xx >= W) continue;
      const v = lum[yy * W + xx];
      if (v < mn) mn = v;
      if (v > mx) mx = v;
    }
  }
  return mx - mn;
}

function sobelMean(lum: Float32Array, W: number, H: number): number {
  // Sample stride to bound cost on big canvases; still statistically faithful.
  const stride = W * H > 4_000_000 ? 2 : 1;
  let sum = 0;
  let count = 0;
  for (let y = 1; y < H - 1; y += stride) {
    for (let x = 1; x < W - 1; x += stride) {
      const i = y * W + x;
      const tl = lum[i - W - 1];
      const tc = lum[i - W];
      const tr = lum[i - W + 1];
      const ml = lum[i - 1];
      const mr = lum[i + 1];
      const bl = lum[i + W - 1];
      const bc = lum[i + W];
      const br = lum[i + W + 1];
      const gx = tl + 2 * ml + bl - (tr + 2 * mr + br);
      const gy = tl + 2 * tc + tr - (bl + 2 * bc + br);
      sum += Math.sqrt(gx * gx + gy * gy);
      count++;
    }
  }
  return count ? sum / count : 0;
}

function rowCentroidShift(lumA: Float32Array, lumB: Float32Array, W: number, H: number): number {
  // Pick the inkiest row (max total darkness) in A, then compare the
  // darkness-weighted column centroid between A and B on that row.
  let bestRow = -1;
  let bestInk = 0;
  for (let y = 0; y < H; y++) {
    let ink = 0;
    const base = y * W;
    for (let x = 0; x < W; x++) ink += 255 - lumA[base + x];
    if (ink > bestInk) {
      bestInk = ink;
      bestRow = y;
    }
  }
  if (bestRow < 0) return 0;
  const base = bestRow * W;
  let sa = 0;
  let wa = 0;
  let sb = 0;
  let wb = 0;
  for (let x = 0; x < W; x++) {
    const da = 255 - lumA[base + x];
    const db = 255 - lumB[base + x];
    sa += da * x;
    wa += da;
    sb += db * x;
    wb += db;
  }
  const ca = wa ? sa / wa : 0;
  const cb = wb ? sb / wb : 0;
  return cb - ca;
}

function whiteFill(ctx: CanvasRenderingContext2D, W: number, H: number): void {
  ctx.fillStyle = "#ffffff";
  ctx.fillRect(0, 0, W, H);
}

function hasInk(data: Uint8ClampedArray): boolean {
  // any pixel meaningfully darker than white
  for (let j = 0; j < data.length; j += 4) {
    if (data[j] < 240 || data[j + 1] < 240 || data[j + 2] < 240) return true;
  }
  return false;
}

async function afterPaint(canvas: HTMLCanvasElement): Promise<void> {
  // belt-and-suspenders: 2×rAF then poll until non-blank (worker replay is
  // synchronous on promise-resolve, but this guards a half-painted readback).
  await raf();
  await raf();
  const ctx = canvas.getContext("2d", { willReadFrequently: true });
  if (!ctx) return;
  for (let t = 0; t < 30; t++) {
    const w = canvas.width;
    const h = canvas.height;
    // sample a central strip
    const sy = Math.max(0, Math.floor(h * 0.3));
    const sh = Math.max(1, Math.min(h - sy, Math.floor(h * 0.4)));
    const strip = ctx.getImageData(0, sy, w, sh).data;
    if (hasInk(strip)) return;
    await raf();
  }
}

function raf(): Promise<void> {
  return new Promise<void>((res) => requestAnimationFrame(() => res()));
}

// ── DOM (screenshot corroboration) ────────────────────────────────────────────

function attachDisplayTriple(
  cell: CellResult,
  cA: HTMLCanvasElement,
  cB: HTMLCanvasElement,
  heat: ImageData,
  W: number,
  H: number,
): void {
  // Downscale to a bounded display size to keep DOM memory small.
  const DISP = 300;
  const scale = Math.min(1, DISP / W);
  const dw = Math.max(1, Math.round(W * scale));
  const dh = Math.max(1, Math.round(H * scale));

  const small = (src: CanvasImageSource): HTMLCanvasElement => {
    const c = document.createElement("canvas");
    c.width = dw;
    c.height = dh;
    const ctx = c.getContext("2d");
    if (ctx) ctx.drawImage(src, 0, 0, dw, dh);
    return c;
  };

  // heatmap → temp big canvas → downscale
  const heatBig = document.createElement("canvas");
  heatBig.width = W;
  heatBig.height = H;
  heatBig.getContext("2d")?.putImageData(heat, 0, 0);

  cell.dispA = small(cA);
  cell.dispB = small(cB);
  cell.dispHeat = small(heatBig);
}

interface CellResult {
  dispA?: HTMLCanvasElement;
  dispB?: HTMLCanvasElement;
  dispHeat?: HTMLCanvasElement;
}

function mountCell(overlay: HTMLElement, cell: CellResult): void {
  const row = document.createElement("div");
  row.setAttribute("style", "margin:0 0 14px;border-top:1px solid #2a2c33;padding-top:8px;");

  const cap = document.createElement("div");
  const color =
    cell.verdict === "REGRESSION" || cell.verdict === "CONFOUND" || cell.verdict === "INVALID"
      ? "#ff8a8a"
      : cell.verdict === "COSMETIC"
        ? "#ffd27a"
        : "#8affa0";
  cap.setAttribute("style", `color:${color};font-weight:600;margin-bottom:6px;`);
  cap.textContent =
    `z${cell.zoom}·d${cell.dpr} ${cell.verdict} — ident=${cell.identPct.toFixed(3)}% ` +
    `diffPx=${cell.diffPx} maxΔ=${cell.maxDelta} edge%=${cell.edgePct.toFixed(0)} ` +
    `gradRatio=${cell.gradRatio.toFixed(3)} centroidΔ=${cell.centroidShiftPx.toFixed(3)}px ` +
    `[spawns=${cell.spawns} fellBack=${cell.fellBack} rpc=${cell.rpcRoundTrips}]`;
  row.appendChild(cap);

  const strip = document.createElement("div");
  strip.setAttribute("style", "display:flex;gap:8px;align-items:flex-start;");
  const labels = ["in-process", "worker", "diff×8"];
  const canvases = [cell.dispA, cell.dispB, cell.dispHeat];
  for (let k = 0; k < 3; k++) {
    const wrap = document.createElement("div");
    wrap.setAttribute("style", "display:flex;flex-direction:column;gap:2px;");
    const lab = document.createElement("div");
    lab.setAttribute("style", "color:#9aa0aa;font-size:10px;");
    lab.textContent = labels[k];
    wrap.appendChild(lab);
    const cv = canvases[k];
    if (cv) {
      cv.setAttribute("style", "border:1px solid #34373f;image-rendering:pixelated;");
      wrap.appendChild(cv);
    }
    strip.appendChild(wrap);
  }
  row.appendChild(strip);
  overlay.appendChild(row);
}
