/**
 * plugin-loader — the document load/abort lifecycle.
 *
 * This is one of the EXACTLY TWO plugins that get a real XState v5 machine
 * because it has a genuine async lifecycle WITH cancellation. The
 * machine's shape:
 *
 *     idle → planning → loading → (ready | error)
 *                          ↑__________|  (LOAD re-enters loading)
 *
 * `loading` invokes a `fromPromise` actor wrapping the REAL `Viewer.load(bytes)`.
 * A `LOAD` while `loading` RE-ENTERS `loading` — XState v5 stops the prior
 * invoked actor (its `AbortSignal` fires) and starts a fresh one.
 *
 * THIN CALLER: the loader does NOT reimplement load
 * serialization. Core's `#loadChain` / `#loadGeneration` / `abortLayout` stay in
 * @parasma/core; the plugin only calls the public `viewer.load()`. The key
 * mechanism is the COMPOSITION:
 *   - the machine guarantees only ONE actor is live (re-entry stops the prior),
 *   - each fresh `viewer.load()` re-enters core's `#loadChain`, which aborts the
 *     prior streaming layout and fences `loadDocx`+`runLayout`.
 * So two loads can never run `loadDocx`+`runLayout` concurrently, AND a
 * superseded actor's `onDone`/`onError` never fires (no stale ready/error).
 *
 * The `fromPromise` `AbortSignal` here is COOPERATIVE — it does NOT call engine
 * abort (core owns that). On re-enter the stale actor's signal aborts so its
 * settle never transitions the machine; the real layout abort is core's.
 */

import { assign, createActor, fromPromise, setup, type Actor } from "xstate";
import type { DocumentInfo } from "@parasma/core";
import type { Plugin } from "../registry";
import type { KernelContext } from "../kernel-context";
import type { LoaderViewer } from "./ports";

/**
 * Commands OWNED by this plugin. Declaration-merged into the open
 * {@link CommandMap} (targeting `../commands`, the barrel-re-exported kernel
 * module → program-wide) so `exec(id, …)` is arg/return type-checked and an
 * undeclared id fails to register. Signatures authored from the handler bodies.
 */
declare module "../commands" {
  interface CommandMap {
    "loader.open": (bytes: Uint8Array) => Promise<DocumentInfo>;
    "loader.openUrl": (url: string) => Promise<DocumentInfo>;
    "loader.status": () => LoaderStatus;
  }
}

/** Public status surface — the machine's state value, queried via loader.status. */
export type LoaderStatus = "idle" | "planning" | "loading" | "ready" | "error";

interface LoaderContext {
  info: DocumentInfo | null;
  error: unknown;
}

type LoaderEvent =
  | { type: "LOAD"; load: Promise<DocumentInfo> }
  // EFFECTIVE-FAILURE ESCALATION. Sent when a load reached `ready`
  // (parse OK) but layout completed with ZERO renderable pages (0 sheets / 0
  // slides). Drives `ready → error` so the SAME terminal the parse-error path
  // uses fires, instead of a silent `ready` over a stale render.
  | { type: "EMPTY" };

/**
 * The load actor: AWAITS an already-started `viewer.load()` promise (core's
 * serialized `#loadChain` call). `open()` starts exactly ONE `viewer.load()` per
 * call and hands the SAME promise to both the caller and this actor — so the
 * machine never double-loads the engine (the key test asserts core's single
 * loadDocx+runLayout per load).
 *
 * On re-enter (a fresh LOAD) the prior actor is stopped before it can settle, so
 * a superseded load's resolution never reaches `onDone`/`onError` — no stale
 * ready/error transition. We do NOT touch `abortLayout`; core's `#loadChain`
 * performs the real layout abort (thin caller).
 */
const loadActor = fromPromise<DocumentInfo, { load: Promise<DocumentInfo> }>(
  ({ input }) => input.load,
);

function makeMachine() {
  return setup({
    types: {
      context: {} as LoaderContext,
      events: {} as LoaderEvent,
    },
    actors: { loadActor },
    actions: {
      // Effective-failure escalation — assign the effective-failure Error into context. The
      // subscribe-mirror re-emits context.error as loader:error on entry to
      // `error`, so the consumer's existing data-error path renders a
      // meaningful message (never a silent `ready` over a stale render).
      escalateEmpty: assign({
        error: () => new Error("This document has no viewable content"),
      }),
    },
  }).createMachine({
    id: "loader",
    initial: "idle",
    context: { info: null, error: null },
    states: {
      idle: {
        on: { LOAD: "loading" },
      },
      // Transient seam: sniff/normalize already live in core's load(); kept as an
      // extension point. Immediately advance to loading.
      planning: {
        always: "loading",
      },
      loading: {
        invoke: {
          src: "loadActor",
          input: ({ event }) => ({
            // The LOAD that entered/re-entered this state carries the already-
            // started core load promise (started in open(), serialized by core).
            // Only a LOAD event enters `loading` (EMPTY is accepted solely in
            // `ready`), so narrowing to the LOAD variant is sound.
            load: (event as Extract<LoaderEvent, { type: "LOAD" }>).load,
          }),
          onDone: {
            target: "ready",
            actions: assign({
              info: ({ event }) => event.output,
              error: () => null,
            }),
          },
          onError: {
            target: "error",
            actions: assign({
              error: ({ event }) => event.error,
            }),
          },
        },
        // SELF / RE-ENTER: a new LOAD mid-flight stops the prior actor and starts
        // a fresh one. This is the key mechanism (see file header).
        //
        // `reenter: true` is LOAD-BEARING: XState v5 self-transitions are
        // internal by default — a bare `LOAD: "loading"` here would NOT exit/
        // re-enter the state, the prior invoked actor would stay live awaiting
        // load A's promise, and A's settle would drive `ready` with A's info
        // (REPRODUCED: docx+xlsx opened same-tick → loader:loaded said "word",
        // the xlsx opened in Pages view instead of the grid default).
        on: {
          LOAD: { target: "loading", reenter: true },
          // Effective-failure escalation — EMPTY can arrive in `loading` (not just `ready`): a doc that
          // lays out 0 pages FAST (e.g. an all-sheets-failed xlsx — nothing to
          // paginate) emits loadComplete(0) BEFORE the load promise resolves the
          // machine to `ready`. Escalate here too, stopping the in-flight load
          // actor. Safe against supersede: core generation-guards the
          // loadComplete emit, so ONLY the current load's completion reaches the
          // bridge — a superseded load never emits, never sends EMPTY.
          EMPTY: { target: "error", actions: "escalateEmpty" },
        },
      },
      ready: {
        on: {
          // Effective-failure escalation — a parse-OK load that laid out ZERO renderable pages is an
          // EFFECTIVE FAILURE. Re-route through the SAME `error` terminal the
          // parse-error path uses so the consumer's loader:error / data-error
          // handling fires (never a silent `ready` over a stale render). Handled
          // in both `ready` and `loading` because the 0-page loadComplete can
          // land in either state depending on layout speed (see `loading` above).
          EMPTY: { target: "error", actions: "escalateEmpty" },
          LOAD: { target: "loading", reenter: true },
        },
      },
      error: {
        on: { LOAD: { target: "loading", reenter: true } },
      },
    },
  });
}

/**
 * The loader plugin. Construct with the same Viewer the consumer renders into;
 * the kernel boots it and the consumer drives loads via `loader.open`.
 */
export function loaderPlugin(viewer: LoaderViewer): Plugin {
  let actor: Actor<ReturnType<typeof makeMachine>> | null = null;
  let prevStatus: LoaderStatus | null = null;
  // The kernel bus, captured at initialize() so open()/openUrl() can emit the
  // per-open `loader:load-started` event.
  let bus: KernelContext["bus"] | null = null;
  // Per-open monotonic id. Each open() bumps this and emits it so a re-entrant
  // open mid-load (loading→loading, which `loader:status-changed` dedups away)
  // still surfaces a distinct "a new load started" signal for plugin-fonts.
  let openCounter = 0;
  // Per-open settle routing: each loader.open awaits the actor reaching a
  // terminal-ish state (ready/error) FOR THAT LOAD. A superseded load (A when B
  // re-enters) is resolved by core's `viewer.load` promise directly, so callers
  // never hang — we resolve open() from the underlying viewer.load() promise and
  // mirror lifecycle onto the bus from the machine snapshot.
  const unsubs: Array<() => void> = [];

  const status = (): LoaderStatus =>
    (actor?.getSnapshot().value as LoaderStatus) ?? "idle";

  return {
    id: "loader",

    initialize(ctx: KernelContext) {
      actor = createActor(makeMachine());
      bus = ctx.bus;

      // Mirror machine state transitions onto the replayable kernel bus.
      //
      // loader:loaded / loader:error are emitted on the STATE-ENTRY EDGE (the
      // `s !== prevStatus` transition into ready/error), NOT on payload
      // identity. Every settle-to-settle path passes through `loading` first
      // (LOAD re-enters loading), so the edge fires exactly once per settle.
      // The previous `lastSettled?.value !== payload` identity dedup was a
      // latent bug: wasm rejects with RAW STRINGS, so two consecutive loads
      // failing with the SAME message (e.g. 'Could not find EOCD') compared
      // identical — the second loader:error was swallowed and plugin-fonts'
      // readiness arm was never rejected, stranding the fonts machine in
      // 'loading' (a perpetual "Loading fonts…" spinner).
      actor.subscribe((snapshot) => {
        const s = snapshot.value as LoaderStatus;
        if (s === prevStatus) return;
        prevStatus = s;
        ctx.bus.emit("loader:status-changed", s);
        if (s === "ready") {
          const info = snapshot.context.info;
          if (info) ctx.bus.emit("loader:loaded", info);
        } else if (s === "error") {
          ctx.bus.emit("loader:error", snapshot.context.error);
        }
      });

      actor.start();

      // BRIDGE the Viewer's native streaming events into the kernel's replayable
      // bus (bridge, don't own). The Viewer stays source-of-truth for
      // page-level catch-up; on() replays laid-out indices + sticky loadComplete,
      // so a late kernel-bus subscriber composes both replay layers.
      unsubs.push(
        viewer.on("pageReady", (idx) => ctx.bus.emit("pageReady", idx)),
        // Partial-layout signal (memory guardrail), bridged 1:1 (replay "latest" on the
        // bus gives late subscribers the sticky version).
        viewer.on("layoutTruncated", (n) => ctx.bus.emit("layoutTruncated", n)),
        viewer.on("loadComplete", (total) => {
          ctx.bus.emit("loadComplete", total);
          // Effective-failure escalation — a load that reached `ready` (parse OK) but laid out ZERO
          // pages is an EFFECTIVE FAILURE. `loadComplete` fires ONLY at layout
          // COMPLETION (core sets #loadComplete=true, then #emit), never
          // mid-stream, so a doc still streaming pages cannot trip this. Core
          // generation-guards the emit (#loadGeneration !== myGeneration → no
          // emit), so a SUPERSEDED load never reaches here. EMPTY is accepted in
          // BOTH `loading` and `ready` (a fast 0-page layout can emit
          // loadComplete(0) before the load promise resolves the machine to
          // `ready`); the generation guard — not the state — is what prevents a
          // stale failure.
          if (total === 0) actor?.send({ type: "EMPTY" });
        }),
        // Late image decode → the kernel bus. plugin-viewport drives the single
        // invalidate-visible channel off this (same path as a late font face),
        // so a slow-decoding image appears without a zoom/scroll nudge.
        viewer.on("imageReady", (relId) => ctx.bus.emit("imageReady", relId)),
      );

      // Commands (the plugin surface). loader.open is the public load entry;
      // it sends LOAD into the machine (re-entry auto-aborts a prior load) AND
      // returns the underlying viewer.load() DocumentInfo so callers can
      // destructure `{ pageSize, documentKind }` from the result.
      unsubs.push(
        ctx.commands.register("loader.open", (bytes: Uint8Array) => open(bytes)),
        ctx.commands.register("loader.openUrl", (url: string) => openUrl(url)),
        ctx.commands.register("loader.status", () => status()),
      );
    },

    destroy(_ctx: KernelContext) {
      for (const u of unsubs.splice(0)) u();
      actor?.stop();
      actor = null;
      prevStatus = null;
      bus = null;
    },
  };

  /**
   * Drive a load. Sending LOAD re-enters `loading` (stopping any prior actor);
   * core's `#loadChain` serializes the actual work + aborts the prior layout.
   * We resolve the caller's promise from the SAME `viewer.load()` core returns
   * (its serialized chain), so the caller gets the right DocumentInfo even when a
   * later open() supersedes this one's machine actor.
   */
  function open(bytes: Uint8Array): Promise<DocumentInfo> {
    // Start core's load ONCE (queues onto core's #loadChain). This single promise
    // is the caller's result AND the actor's input — the engine is loaded exactly
    // once per open, never doubly. `.catch(()=>{})` is intentionally absent: the
    // caller observes rejection; the machine observes it via onError.
    const corePromise = viewer.load(bytes);
    // Announce this open() BEFORE driving the machine — emitted ONCE per open
    // regardless of the machine's prior status (no status-edge dedup), so a 2nd
    // open while loading still re-plans fonts exactly once. This fires
    // synchronously, before core's #doLoad microtask runs #loadFonts (and thus
    // before any `fontsReady`), so plugin-fonts arms a fresh readiness in time.
    bus?.emit("loader:load-started", ++openCounter);
    // Drive the machine in lockstep so status/lifecycle reflect this load. The
    // LOAD re-enters `loading`, stopping any prior actor; core's #loadChain then
    // aborts the prior layout before swapping in this document.
    actor?.send({ type: "LOAD", load: corePromise });
    return corePromise;
  }

  /**
   * Fetch a document by URL, then load it. The FETCH stage is part of the load
   * lifecycle, not a detached prelude:
   *
   *  - GENERATION-GUARDED: concurrent openUrl calls used to enter core's
   *    serialized chain in FETCH-COMPLETION order, so the LAST click lost
   *    (REPRODUCED: click 'arabic' [slow fetch] then 'bold' [fast] → bold loads,
   *    then arabic's late fetch completes and clobbers it). We stamp a ticket
   *    from the SHARED open counter at entry; a fetch that completes after a
   *    newer open()/openUrl() drops its bytes and rejects (AbortError).
   *  - MACHINE-DRIVEN: the machine enters `loading` for the fetch itself, so a
   *    slow fetch shows a loading state (not the prior doc's stale 'ready'),
   *    and a FAILED fetch settles the actor → `error` → `loader:error` (it used
   *    to throw before ever touching the machine: blank stage, green 'ready'
   *    token, and plugin-fonts never released).
   *
   * On fetch success, open() takes over: it sends a fresh LOAD (re-entering
   * `loading`, superseding the fetch-stage actor) whose actor settles from
   * core's serialized viewer.load() — exactly the byte-path lifecycle.
   */
  function openUrl(url: string): Promise<DocumentInfo> {
    const ticket = ++openCounter;
    // Announce the open NOW (not after the fetch) so plugin-fonts re-arms and
    // observers see the load begin with the user's click.
    bus?.emit("loader:load-started", ticket);
    const run = (async () => {
      const res = await fetch(url);
      if (!res.ok) throw new Error(`[parasma:loader] fetch failed: ${res.status} ${url}`);
      const buf = new Uint8Array(await res.arrayBuffer());
      // Superseded mid-fetch: a newer open()/openUrl() bumped the shared
      // counter. Do NOT hand these stale bytes to core — they would queue onto
      // the chain AFTER the newer load and win. The machine actor for this LOAD
      // was already stopped by the newer LOAD; reject so the CALLER knows this
      // open lost (AbortError by convention — consumers treat it as benign).
      if (openCounter !== ticket) {
        throw new DOMException(`[parasma:loader] open superseded mid-fetch: ${url}`, "AbortError");
      }
      return open(buf);
    })();
    // Drive the machine through the fetch stage too (loading during the fetch;
    // error on fetch failure). On success the nested open() re-enters loading
    // with core's load promise and this actor is stopped before `run` settles.
    actor?.send({ type: "LOAD", load: run });
    return run;
  }
}
