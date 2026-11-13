/**
 * The render worker — hosts the wasm Viewer OFF the
 * main thread. It is a thin dispatcher: a `req` message names a `WasmViewer`
 * method, the worker calls it and replies with a `res`. Two special cases:
 *
 *  - `runLayout`: wasm streams per-page progress through a JS callback, which
 *    cannot cross the worker boundary. The worker supplies its own callback that
 *    reifies each call as a `pageReady` `evt` (stamped with the load `gen` the
 *    main thread passed), then resolves the `res` with the total page count when
 *    layout completes. postMessage is FIFO, so all `pageReady` events arrive
 *    before the `res` — the main thread's existing generation guard + emitter
 *    apply them in order.
 *  - `renderPageDisplayList`: the returned `{ opcodes: Float64Array, strings }` is
 *    replied with `opcodes.buffer` as a Transferable (zero-copy back to main).
 *
 * The wasm module loads via the SAME glue the main thread uses (`./wasm/…`); in
 * the worker's module scope that `init()` is the worker's own single wasm
 * instance (its own font cache + document state — no cross-thread sharing).
 */

import init, { Viewer as WasmViewer } from "./wasm/parasma_wasm.js";
import { collectTransferables } from "./image-transfer.js";

type ReqMessage = { kind: "req"; id: number; method: string; args: unknown[] };

let viewer: WasmViewer | null = null;
let ready: Promise<void> | null = null;

/**
 * EMF raster cache — rels whose DIB blits have already been decoded into THIS
 * worker's wasm bitmap map (`emf:{rel}:{n}` keys), so a re-raster skips the
 * decode. Cleared on every `load*` method: loadDocx repopulates `emf_pics`,
 * and a same-relId NEW doc must decode its NEW blits (yes, this also catches
 * `loadFont` — one redundant re-decode, harmless; do NOT tighten to an exact
 * match and miss a future load variant).
 */
const decodedEmfRels = new Set<string>();

/**
 * The worker half of `EngineFacade.rasterizeEmf`: lazily decode the rel's DIB
 * blits (awaited — worker rasters are complete by construction, MODULO
 * decode-failed blits: the catch-continue below memoizes past a permanently
 * corrupt DIB, freezing a partial raster with no retry. Deliberate parity with
 * the in-process path, whose permanently-failing decode never completes
 * either — a deterministic failure would fail every retry anyway), paint the EMF
 * program into a local OffscreenCanvas via the wasm `renderEmfToCanvas` export
 * (transparent ground), and hand back the ImageBitmap (transferred zero-copy
 * by the caller). `null` = cannot serve (stale wasm bundle, unknown key after
 * a doc switch) — the main thread keeps its placeholder / stale bitmap.
 */
async function rasterizeEmf(v: WasmViewer, key: string, pxW: number, pxH: number): Promise<ImageBitmap | null> {
  const w = v as unknown as Record<string, ((...a: unknown[]) => unknown) | undefined>;
  const render = w.renderEmfToCanvas;
  if (typeof render !== "function" || typeof OffscreenCanvas === "undefined") return null;
  // Rel ids are xsd:ID (no ':'), so stripping the "emf-pic:" prefix is safe.
  const rel = key.startsWith("emf-pic:") ? key.slice("emf-pic:".length) : key;
  const blitRefsFn = w.emfBlitRefs;
  if (!decodedEmfRels.has(rel) && typeof blitRefsFn === "function") {
    const refs = (blitRefsFn.apply(v, [key]) ?? []) as Array<{
      relId: string;
      wPx: number;
      hPx: number;
      bytes: Uint8Array;
    }>;
    for (const ref of refs) {
      try {
        // Raw RGBA has no container — ImageData decode (same as the main
        // thread's x-emf-dib-rgba ingest branch). createImageBitmap exists in
        // workers.
        const bm = await createImageBitmap(new ImageData(new Uint8ClampedArray(ref.bytes), ref.wPx, ref.hPx));
        v.setImageBitmap(ref.relId, bm);
      } catch (err) {
        // Continue past a single bad blit: a PARTIAL raster (grey placeholder
        // for the missing blit) beats a rejected RPC — that would null-degrade
        // through #tryReq and freeze the pic placeholder with no retry path.
        console.warn(`[parasma worker] EMF blit decode failed for ${ref.relId}:`, err);
      }
    }
    decodedEmfRels.add(rel);
  }
  const canvas = new OffscreenCanvas(pxW, pxH);
  // Default alpha:true is LOAD-BEARING (transparent ground; see the wasm hook).
  const ctx = canvas.getContext("2d");
  if (!ctx) return null;
  const ok = render.apply(v, [key, ctx, pxW, pxH, 1]) as boolean;
  // `false` (unknown key / degenerate dims): never transfer the transparent
  // canvas — registering it main-side would blank the EMF permanently.
  if (!ok) return null;
  return canvas.transferToImageBitmap();
}

async function ensureViewer(wasmInput?: string | URL): Promise<WasmViewer> {
  if (!ready) {
    // `wasmInput` arrives from the main thread's resolved config via the __init
    // boot handshake. The OBJECT form avoids the glue's deprecation warn; the
    // no-arg branch stays a LITERAL init() so no-config = byte-identical to today.
    ready = (
      wasmInput != null
        ? init({ module_or_path: wasmInput }) // main-resolved binary
        : init() // no-config => co-located, byte-identical to today
    ).then(() => undefined);
  }
  await ready;
  if (!viewer) viewer = new WasmViewer();
  return viewer;
}

function post(msg: unknown, transfer: Transferable[] = []): void {
  (self as unknown as Worker).postMessage(msg, transfer);
}

/**
 * Forward engine panics to the main thread.
 *
 * The wasm panic hook (`install_panic_hook`, lib.rs) writes the panic message +
 * source location to `console.error` before the instance aborts. In a WORKER
 * that output lands on the worker's console, which the main thread cannot read —
 * so the only thing the host ever saw was the opaque
 * `worker error: Uncaught RuntimeError: unreachable`. Since worker mode is the
 * DEFAULT, that meant the panic hook was invisible on the path that matters.
 *
 * Verified end-to-end against a document with a negative table `gridSpan`:
 * in-process reported
 * `panicked at wml/table.rs:588:48: slice index starts at 1 but ends at 0`,
 * worker mode reported nothing.
 *
 * The `[parasma PANIC]` tag is the contract with the Rust hook — keep both sides
 * in step. We re-emit through the ORIGINAL console.error too, so the worker's own
 * devtools output is unchanged.
 */
{
  const original = console.error.bind(console);
  console.error = (...args: unknown[]): void => {
    original(...args);
    const first = args.length > 0 ? String(args[0]) : "";
    if (first.startsWith("[parasma PANIC]")) {
      // gen -1: a panic is not tied to a load generation and must never be
      // stale-dropped by the main thread's generation guard.
      try {
        post({ kind: "evt", event: "enginePanic", gen: -1, payload: first });
      } catch {
        // The instance is already aborting; a failed post must not mask the panic.
      }
    }
  };
}

self.onmessage = async (ev: MessageEvent<ReqMessage>) => {
  const msg = ev.data;
  if (!msg || msg.kind !== "req") return;
  const { id, method, args } = msg;
  try {
    if (method === "__init") {
      // Boot handshake — MUST precede any other ensureViewer() so the worker's
      // single wasm instance initialises the main-resolved binary. args[0]
      // nullish (no-config) => ensureViewer(undefined) => init() no-arg => today.
      await ensureViewer((args[0] as string | URL | null | undefined) ?? undefined);
      if (id !== 0) post({ kind: "res", id, ok: true, value: "pong" });
      return;
    }

    const v = await ensureViewer();
    let value: unknown;
    let transfer: Transferable[] = [];

    // A new document invalidates the per-rel EMF blit-decode memo (the wasm
    // `emf_pics` is repopulated; a same-relId new doc must decode NEW blits).
    if (method.startsWith("load")) decodedEmfRels.clear();

    if (method === "rasterizeEmf") {
      // Special case BEFORE the generic dispatcher: "rasterizeEmf" is not a
      // wasm export (the generic typeof-lookup would throw "unknown worker
      // method", which is exactly how a STALE worker bundle degrades to null
      // main-side via #tryReq). The generic arm also never awaits returned
      // promises — this one must await the decode+raster AND set the transfer
      // list itself (ImageBitmap is a Transferable; zero-copy back to main).
      const [key, pxW, pxH] = args as [string, number, number];
      const bitmap = await rasterizeEmf(v, key, pxW, pxH);
      value = bitmap;
      transfer = bitmap ? [bitmap] : [];
    } else if (method === "runLayout") {
      // args = [gen]. Stream pageReady events for that generation, then resolve
      // with the total. The wasm callback fires synchronously per page from
      // inside runLayout's future; each becomes one FIFO-ordered evt.
      const gen = args[0] as number;
      const total = await (v as unknown as { runLayout(cb: (idx: number) => void): Promise<number> })
        .runLayout((idx: number) => post({ kind: "evt", event: "pageReady", gen, payload: idx }));
      value = total;
    } else {
      const fn = (v as unknown as Record<string, ((...a: unknown[]) => unknown) | undefined>)[method];
      if (typeof fn !== "function") throw new Error(`unknown worker method: ${method}`);
      value = fn.apply(v, args);
      // Build the zero-copy transfer list: renderPageDisplayList's opcode buffer
      // OR imageRefs' per-image byte buffers. Extracted to a pure helper so
      // it's unit-testable without running this worker's top-level onmessage
      // boot.
      transfer = collectTransferables(method, value);
    }

    // id 0 = fire-and-forget notify; no response expected.
    if (id !== 0) post({ kind: "res", id, ok: true, value }, transfer);
  } catch (err) {
    const e = err as { message?: string; stack?: string };
    if (id !== 0) {
      post({ kind: "res", id, ok: false, error: { message: String(e?.message ?? err), stack: e?.stack } });
    } else {
      console.error("[parasma worker] notify failed:", err);
    }
  }
};
