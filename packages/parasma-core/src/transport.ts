/**
 * Worker transport — the main↔worker RPC seam that
 * lets the wasm engine run OFF the main thread. The worker hosts the wasm Viewer
 * (parse / layout / shaping / display-list recording); the main thread stays the
 * orchestrator (fonts, image decode, generation guard, event emitter) and does
 * the cheap GPU replay. This module defines the wire protocol + the two transport
 * impls; the engine seam in index.ts routes low-level wasm calls through it.
 *
 * Protocol — three message shapes over one MessagePort:
 *   req (main→worker):  { kind:"req", id, method, args }            (+ transfer)
 *   res (worker→main):  { kind:"res", id, ok:true, value } | { …, ok:false, error }
 *   evt (worker→main):  { kind:"evt", event, gen, payload }         (unsolicited)
 * `id` is a main-minted correlation id; a Map settles the matching promise. `gen`
 * is the `#loadGeneration` token echoed on every event so the EXISTING stale-drop
 * guard (a superseded load's trailing pageReady events are ignored) works verbatim
 * with the worker as just another async event source.
 */

/** A worker→main unsolicited event (today: streamed `pageReady`). */
export interface TransportEvent {
  event: string;
  /** The `#loadGeneration` this event belongs to (stale-drop guard). */
  gen: number;
  payload: unknown;
}

/**
 * How the render worker is obtained (the single `worker` config field):
 *  - absent / `true` / `false` = the co-located default worker (byte-identical
 *    to today — see {@link createRenderWorker});
 *  - a FUNCTION = the consumer's bundler-emitted worker (the FIRST-CLASS and
 *    ONLY documented cross-origin path);
 *  - `{ url }` = a same-origin-only convenience;
 *  - `"auto"` = SNIFF-AND-ROUTE at each `load()`: construct in-process (default),
 *    then per document ensure the engine the format needs — docx (the
 *    worker-complete format) attaches a worker so parse+layout run off-thread;
 *    xlsx/pptx/csv stay in-process (the worker cannot yet paint the grid). A
 *    worker-boot failure degrades SILENTLY to in-process (batteries-included).
 *    {@link Viewer.create} is the ONLY sanctioned `"auto"` entry — it intercepts
 *    `"auto"` and never passes it to {@link createRenderWorker} /
 *    {@link createWorkerTransport} (the ensure step passes `worker: undefined`);
 *    a stray `"auto"` reaching {@link createRenderWorker} falls through its
 *    co-located default branch (harmless but off-intent).
 */
export type WorkerSelector = "auto" | boolean | (() => Worker) | { url: string | URL };

/** Boot config for {@link createWorkerTransport}. */
export interface RenderWorkerConfig {
  /** How to obtain the worker; see {@link WorkerSelector}. Absent => default. */
  worker?: WorkerSelector;
  /**
   * Resolved + ABSOLUTIZED wasm input threaded into the worker's `init()` via
   * the boot handshake. `string | URL` ONLY (clone-safe over postMessage;
   * Response/Module are in-process-only). `undefined` => worker `init()` no-arg
   * = co-located default, byte-identical to today.
   */
  wasmInput?: string | URL;
  /** Boot-handshake timeout in ms (default 10s). */
  timeoutMs?: number;
  /** See {@link WorkerTransportOptions.stallTimeoutMs}. */
  stallTimeoutMs?: number;
}

/** Options for {@link WorkerTransport}. */
export interface WorkerTransportOptions {
  /**
   * Stall watchdog, in ms (default 30 s; `0` disables). While a request is
   * outstanding, a worker that sends NOTHING (no reply, no `pageReady`) for this
   * long is treated as hung: it is terminated, every pending request rejects,
   * and the dead handlers fire — the same path as a crashed worker.
   *
   * Why silence and not elapsed time: layout streams one `pageReady` per page,
   * so a long document is never silent, while a hostile one stuck inside a
   * single synchronous engine call is. The engine cannot be interrupted from
   * outside; terminating its worker is the only way to stop it.
   */
  stallTimeoutMs?: number;
}

/** Request/response RPC + a one-way event channel. */
export interface Transport {
  /** Call a worker method; resolves with its return value (or rejects on error). */
  request(method: string, args: unknown[], transfer?: Transferable[]): Promise<unknown>;
  /** Fire-and-forget (no response awaited) — e.g. setSearchHighlight. */
  notify(method: string, args: unknown[], transfer?: Transferable[]): void;
  /** Subscribe to worker→main events. Returns an unsubscribe fn. */
  onEvent(handler: (e: TransportEvent) => void): () => void;
  /** Subscribe to fatal transport death (worker errored). Returns an unsubscribe fn. */
  onDead(handler: (err: Error) => void): () => void;
  /** Tear down the worker. */
  dispose(): void;
}

/**
 * Spawn the render worker as a module Worker. The `new URL("./worker.js", …)`
 * pattern lets Vite emit the worker bundle and resolve the co-located wasm asset
 * (no 6 MB duplication). Kept here (in @parasma/core) so the worker
 * shares the same `./wasm/parasma_wasm.js` glue path as the main thread.
 *
 * `worker` selects the source ({@link WorkerSelector}). Absent / `true` / `false`
 * falls to the co-located default — the EXACT current body — so the no-config
 * path is byte-identical to today. A function is invoked (bundler-emitted,
 * cross-origin); `{ url }` is a same-origin convenience.
 */
export function createRenderWorker(worker?: WorkerSelector): Worker {
  if (typeof worker === "function") return worker(); // bundler-emitted (cross-origin)
  if (worker && typeof worker === "object") return new Worker(worker.url, { type: "module" }); // {url}
  return new Worker(new URL("./worker.js", import.meta.url), { type: "module" }); // absent/true/false
}

/**
 * Boot a {@link WorkerTransport}, resolving only after the worker confirms it
 * loaded + initialised wasm (an `__init`/`pong` handshake, carrying the resolved
 * wasm input, within `timeoutMs`). Rejects (so the caller can fall back to
 * in-process) if the worker can't be constructed or init fails. With no config
 * the handshake sends `[null]` and the worker runs `init()` no-arg — today's
 * behavior. `@public`
 */
export async function createWorkerTransport(config: RenderWorkerConfig = {}): Promise<WorkerTransport> {
  const { worker, wasmInput, timeoutMs = 10_000, stallTimeoutMs } = config;
  const w = createRenderWorker(worker);
  const transport = new WorkerTransport(w, { stallTimeoutMs });
  try {
    const pong = await Promise.race([
      transport.request("__init", [wasmInput ?? null]),
      new Promise((_, reject) => setTimeout(() => reject(new Error("worker boot timeout")), timeoutMs)),
    ]);
    if (pong !== "pong") throw new Error(`unexpected init reply: ${String(pong)}`);
    return transport;
  } catch (err) {
    transport.dispose();
    throw err;
  }
}

type ReqMessage = { kind: "req"; id: number; method: string; args: unknown[] };
type ResMessage =
  | { kind: "res"; id: number; ok: true; value: unknown }
  | { kind: "res"; id: number; ok: false; error: { message: string; stack?: string } };
type EvtMessage = { kind: "evt"; event: string; gen: number; payload: unknown };
type FromWorker = ResMessage | EvtMessage;

/**
 * Main-thread transport over a dedicated module Worker. Maintains the
 * correlation-id → promise map and fans worker events out to subscribers.
 */
export class WorkerTransport implements Transport {
  #worker: Worker;
  #seq = 0;
  #pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: unknown) => void }>();
  #eventHandlers = new Set<(e: TransportEvent) => void>();
  #disposed = false;
  #stallMs: number;
  /** When the worker was last heard from (or a request started after idle). */
  #lastHeard = 0;
  #watchdog: ReturnType<typeof setInterval> | null = null;
  /** When the watchdog last ran, to notice the main thread having been frozen. */
  #lastTick = 0;

  constructor(worker: Worker, options: WorkerTransportOptions = {}) {
    this.#worker = worker;
    this.#stallMs = options.stallTimeoutMs ?? 30_000;
    this.#worker.onmessage = (ev: MessageEvent<FromWorker>) => this.#onMessage(ev.data);
    // A worker-scope error (e.g. wasm init failure, or the worker dying
    // mid-session) is FATAL to this transport.
    this.#worker.onerror = (ev) => this.#die(new Error(`worker error: ${ev.message}`));
  }

  /**
   * Mark the transport dead so any NEW request rejects synchronously instead of
   * postMessaging into the void and hanging forever (a hung load-path RPC would
   * wedge the Viewer's #loadChain and break ALL future loads). Reject everything
   * in flight, then fire the dead handlers so the Viewer can drop #remote and
   * fall back to in-process wasm.
   */
  #die(err: Error): void {
    if (this.#dead) return;
    this.#dead = true;
    this.#stopWatchdog();
    for (const { reject } of this.#pending.values()) reject(err);
    this.#pending.clear();
    for (const h of this.#deadHandlers) {
      try {
        h(err);
      } catch (e) {
        console.error("[parasma] transport dead handler threw:", e);
      }
    }
  }

  #startWatchdog(): void {
    if (this.#stallMs <= 0 || this.#watchdog !== null) return;
    const period = Math.min(1000, Math.max(1, Math.floor(this.#stallMs / 4)));
    this.#lastTick = Date.now();
    this.#watchdog = setInterval(() => {
      const now = Date.now();
      // A tick far later than scheduled means the main thread itself was not
      // running (a frozen background tab, a debugger pause). The worker's
      // replies may be queued behind it, so that gap is not the worker's
      // silence: restart the count instead of killing a healthy worker.
      if (now - this.#lastTick > period * 4) this.#lastHeard = now;
      this.#lastTick = now;
      if (this.#pending.size === 0) return this.#stopWatchdog();
      if (now - this.#lastHeard < this.#stallMs) return;
      this.#worker.terminate();
      this.#die(new Error(`worker stalled: no reply or progress for ${this.#stallMs} ms`));
    }, period);
  }

  #stopWatchdog(): void {
    if (this.#watchdog !== null) clearInterval(this.#watchdog);
    this.#watchdog = null;
  }

  #dead = false;
  #deadHandlers = new Set<(err: Error) => void>();

  /**
   * Subscribe to fatal transport death (the worker errored). The Viewer uses
   * this to reset to the in-process path so subsequent loads don't hang. Returns
   * an unsubscribe fn.
   */
  onDead(handler: (err: Error) => void): () => void {
    this.#deadHandlers.add(handler);
    return () => this.#deadHandlers.delete(handler);
  }

  #onMessage(msg: FromWorker): void {
    this.#lastHeard = Date.now();
    if (msg.kind === "evt") {
      const e: TransportEvent = { event: msg.event, gen: msg.gen, payload: msg.payload };
      for (const h of this.#eventHandlers) {
        try {
          h(e);
        } catch (err) {
          console.error("[parasma] transport event handler threw:", err);
        }
      }
      return;
    }
    // res
    const entry = this.#pending.get(msg.id);
    if (!entry) return;
    this.#pending.delete(msg.id);
    if (msg.ok) entry.resolve(msg.value);
    else entry.reject(new Error(msg.error.message));
  }

  request(method: string, args: unknown[], transfer: Transferable[] = []): Promise<unknown> {
    if (this.#disposed || this.#dead) return Promise.reject(new Error("transport unavailable"));
    const id = ++this.#seq;
    const msg: ReqMessage = { kind: "req", id, method, args };
    // Silence is measured from the start of the current busy period, not from
    // whenever the worker last spoke before going idle.
    if (this.#pending.size === 0) this.#lastHeard = Date.now();
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      this.#worker.postMessage(msg, transfer);
      this.#startWatchdog();
    });
  }

  notify(method: string, args: unknown[], transfer: Transferable[] = []): void {
    if (this.#disposed || this.#dead) return;
    // id 0 = fire-and-forget; the worker still runs it but sends no res we await.
    const msg: ReqMessage = { kind: "req", id: 0, method, args };
    this.#worker.postMessage(msg, transfer);
  }

  onEvent(handler: (e: TransportEvent) => void): () => void {
    this.#eventHandlers.add(handler);
    return () => this.#eventHandlers.delete(handler);
  }

  dispose(): void {
    this.#disposed = true;
    this.#stopWatchdog();
    for (const { reject } of this.#pending.values()) reject(new Error("transport disposed"));
    this.#pending.clear();
    this.#eventHandlers.clear();
    this.#worker.terminate();
  }
}
