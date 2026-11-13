/**
 * plugin-fonts — the OBSERVABLE font-loading status + per-face re-render
 * (the 2nd of the EXACTLY TWO genuine XState v5 machines).
 *
 * The machine's shape (mirrors plugin-loader):
 *
 *     idle → planning → loading → (ready | error)
 *                          ↑__________|  (PLAN re-enters loading per document)
 *
 * OBSERVE-AND-AUGMENT. Core KEEPS ownership of font orchestration: #loadFonts /
 * buildFontPlan stay AWAITED mid-load inside core's #doLoad* (load{Docx,Xlsx,
 * Pptx} → pageSize → AWAIT #loadFonts → runLayout). That await-before-layout is
 * the alignment invariant — wasm.loadFont gives the rustybuzz shaper the metrics
 * it breaks lines / places glyphs with, and document.fonts.add gives canvas the
 * SAME face so fillText paints at those exact metrics (measure == paint). There
 * is no public seam to interpose font-loading between core's private loadDocx and
 * runLayout without split-brain font loading or a double-load race, so relocating
 * orchestration into the plugin is provably risky for ZERO extra shipped value. This plugin instead:
 *
 *   1. OBSERVES core's load via the bus (loader:loaded drives a per-document
 *      re-plan) and bridges core's `fontsReady` → DONE, so the machine surfaces a
 *      genuine idle → planning → loading → ready | error status, re-planned per doc.
 *   2. ADDS the per-FACE value: bridges core's `fonts:variant-arrived` (emitted as
 *      each family's faces land, BEFORE the slowest face's terminal fontsReady)
 *      onto the kernel bus, so plugin-viewport re-renders the currently-visible
 *      pages the instant a family (e.g. the slow Noto CJK OTF) is ready — CJK /
 *      substituted text stops showing fallback as soon as ITS face arrives,
 *      rather than waiting for everything.
 *
 * THIN CALLER: the plugin does NOT reimplement font
 * planning, fetching, aliasing, or cancellation. Core already owns per-document
 * re-plan (each #doLoad* re-runs buildFontPlan) AND cancellation (the
 * #loadGeneration stale() closure abandons a superseded doc's in-flight fetches).
 * The machine MIRRORS that: a new PLAN (next document's loader:loaded) re-enters
 * loading, stopping the prior invoked actor (same key mechanism as loader).
 */

import { assign, createActor, fromPromise, setup, type Actor } from "xstate";
import type {
  FontUrlResolver,
  MatchTier,
  FontCoverage,
  LocalFontPermission,
} from "@parasma/core";
import type { FontsViewer } from "./ports";
import type { Plugin } from "../registry";
import type { KernelContext } from "../kernel-context";

/**
 * Commands OWNED by this plugin. Declaration-merged into the open
 * {@link CommandMap} (targeting `../commands`, the barrel-re-exported kernel
 * module → program-wide) so `exec(id, …)` is arg/return type-checked and an
 * undeclared id fails to register. Signatures authored from the handler bodies.
 */
declare module "../commands" {
  interface CommandMap {
    "fonts.status": () => FontsStatus;
    "fonts.arrived": () => string[];
    "fonts.debug": () => Promise<FontsDebugReport>;
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
    // Fonts plugin lifecycle (the 2nd genuine machine).
    "fonts:status-changed": string;
    /** Terminal: all faces for the current document have loaded (count). */
    "fonts:ready": number;
    /** Per-family arrival, bridged from core's `fonts:variant-arrived`. */
    "fonts:variant-arrived": { family: string; loadedVariants: number };
    /**
     * A live font re-apply (viewer.refreshFonts)
     * finished, bridged from the Viewer by plugin-fonts. `changed: true` = faces
     * re-fetched under the new useLocalFonts intent AND layout re-run (the bridge
     * clears the stale pageReady/loadComplete replay logs BEFORE re-emitting, so
     * on this event the retained page lifecycle belongs to the refresh's own
     * layout); `changed: false` = the semantic no-op gate found nothing to apply
     * (replay logs untouched). Transient ("none" default): a late subscriber
     * re-reads fonts.debug anyway.
     */
    "fonts:refreshed": { changed: boolean };
  }
}

/** Public status surface — the machine's state value, queried via fonts.status. */
export type FontsStatus = "idle" | "planning" | "loading" | "ready" | "error";

/**
 * One row of the `fonts.debug` report: the
 * MERGED font truth for one declared family — TS-side provenance (plan
 * resolution, byte-source origin, per-load freshness, registered variants) +
 * the wasm probe's live FINAL-STATE resolution when available.
 */
export interface FontsDebugRow {
  declared: string;
  /** The plan's substitute family, or null (engine default). */
  resolved: string | null;
  /** Byte-source provenance of the regular face (survives cache hits). */
  origin: "local" | "cdn" | "failed" | null;
  /** "fresh" (fetched this load) vs "cached" (session-cache hit). */
  freshness: "fresh" | "cached" | null;
  /**
   * Which rung of the local-font resolution ladder served the LOCAL bytes
   * (1–4 = exact ladder tiers on the declared name, "curated" = the Word
   * substitution hint, "dev-route" = the injected dev byte-source), or null
   * when the face is not local-sourced.
   */
  matchTier: MatchTier | null;
  /** The LOCAL family that actually provided the bytes (e.g. "Times New
   * Roman" while `resolved` reads "Tinos" — local bytes register under the
   * plan's resolved name). null when the bytes came from the CDN. */
  matchedFamily: string | null;
  /** The coverage classification for this declared family. */
  coverage: FontCoverage;
  /**
   * The wasm probe's live per-family report ("what would this family paint
   * with RIGHT NOW"), or the literal "unavailable" when the build has no
   * debug-probe / the worker bundle is stale. Fields are null on a probe that
   * predates the families report.
   */
  probe:
    | { resolvesNow: string | null; fellBackToDefault: boolean | null; alias: string | null }
    | "unavailable";
  /** Faces registered in document.fonts right now (TS truth, probe-independent). */
  loadedVariants: string[];
  /** The resolved regular face's hhea metrics (per-em), probe-only → null without it. */
  metrics: { unitsPerEm: number; ascentEm: number; descentEm: number; lineGapEm: number } | null;
}

/** The `fonts.debug` command's return shape. */
export interface FontsDebugReport {
  useLocalFonts: boolean;
  /** True while a viewer.refreshFonts() pipeline is queued/in flight. */
  refreshing: boolean;
  probeAvailable: boolean;
  /** The local-fonts permission ("unsupported" where the API is absent). */
  permissionState: LocalFontPermission;
  /** Whether the session local-font catalog has been enumerated. */
  catalogReady: boolean;
  fonts: FontsDebugRow[];
}

interface FontsContext {
  /** Families whose faces have arrived so far this document (per-face accumulation). */
  arrived: Set<string>;
  /** Count from core's terminal `fontsReady` (total faces loaded), if reached. */
  loaded: number;
  error: unknown;
}

type FontsEvent =
  // Per document: a fresh load. Carries a promise that resolves when THIS
  // document's fonts finish (bridged from core's `fontsReady`), so the invoked
  // actor's onDone → ready maps to the real terminal font readiness.
  | { type: "PLAN"; ready: Promise<number> }
  // Per family: a face arrived (bridged from core's `fonts:variant-arrived`).
  // Accumulates into context.arrived; does NOT change the top-level state.
  | { type: "FACE_ARRIVED"; family: string };

/**
 * The fonts actor: AWAITS the per-document font-readiness promise. Because
 * core owns the orchestration, so this promise resolves off core's terminal
 * `fontsReady` event (wired in initialize). On re-enter (next document's PLAN)
 * the prior actor is stopped before it can settle — a superseded document's
 * fontsReady never drives a stale ready/error transition (mirrors loader).
 */
const fontsActor = fromPromise<number, { ready: Promise<number> }>(
  ({ input }) => input.ready,
);

function makeMachine() {
  return setup({
    types: {
      context: {} as FontsContext,
      events: {} as FontsEvent,
    },
    actors: { fontsActor },
  }).createMachine({
    id: "fonts",
    initial: "idle",
    context: { arrived: new Set<string>(), loaded: 0, error: null },
    // FACE_ARRIVED accumulates regardless of state (it can land while loading OR
    // after ready — the slow OTF may arrive just after the terminal fontsReady on
    // a re-plan boundary). It never changes the top-level state.
    on: {
      FACE_ARRIVED: {
        actions: assign({
          arrived: ({ context, event }) => {
            const next = new Set(context.arrived);
            next.add((event as { family: string }).family);
            return next;
          },
        }),
      },
    },
    states: {
      idle: {
        on: { PLAN: "planning" },
      },
      // Transient seam: per-document re-plan (buildFontPlan re-runs in core).
      // Reset the per-document accumulation, then advance to loading.
      planning: {
        entry: assign({ arrived: () => new Set<string>(), loaded: () => 0, error: () => null }),
        always: "loading",
      },
      loading: {
        invoke: {
          src: "fontsActor",
          input: ({ event }) => ({
            // The PLAN that entered/re-entered carries the per-document readiness
            // promise (resolved by core's terminal fontsReady for THIS doc).
            ready: (event as { ready: Promise<number> }).ready,
          }),
          onDone: {
            target: "ready",
            actions: assign({
              loaded: ({ event }) => event.output,
              error: () => null,
            }),
          },
          onError: {
            target: "error",
            actions: assign({ error: ({ event }) => event.error }),
          },
        },
        // RE-ENTER on the next document: PLAN re-routes through planning (resetting
        // accumulation) into a fresh loading actor, stopping the prior one.
        on: { PLAN: "planning" },
      },
      ready: {
        on: { PLAN: "planning" },
      },
      error: {
        on: { PLAN: "planning" },
      },
    },
  });
}

/**
 * The fonts plugin. Construct with the SAME Viewer the loader/viewport plugins
 * use (mirrors loaderPlugin(viewer) / viewportPlugin(viewer)) so it can bridge
 * the Viewer's native `fontsReady` / `fonts:variant-arrived` onto the kernel bus.
 *
 * deps: ["loader"] — the per-document re-plan trigger is the bus 'loader:loaded'
 * event (one per document), and two-phase boot guarantees loader.initialize()
 * (which emits it) runs first.
 */
/** Configuration for `plugin-fonts`. */
export interface FontsConfig {
  /**
   * Where to load font files from. The engine still decides WHICH faces are
   * needed; this decides WHERE they come from (your own CDN / CloudFront).
   * - a **function** (`FontUrlResolver`): full control — receives the default
   *   URL + face info, returns the URL to fetch.
   * - a **string**: a base origin that replaces Parasma's default
   *   `https://cdn.jsdelivr.net` (host the default file paths under your origin).
   * Omit for the bundled jsDelivr defaults.
   */
  fontSource?: FontUrlResolver | string;
  /**
   * Opt in to reading the user's INSTALLED fonts (Font Access API) to match
   * Word's own substitution (e.g. DejaVu Sans → Verdana). Best-effort and
   * gesture-gated; falls back to the bundled URLs when unavailable. Off by
   * default.
   */
  useLocalFonts?: boolean;
}

export function fontsPlugin(viewer: FontsViewer, config: FontsConfig = {}): Plugin {
  let actor: Actor<ReturnType<typeof makeMachine>> | null = null;
  let prevStatus: FontsStatus | null = null;
  const unsubs: Array<() => void> = [];
  // True iff THIS plugin called viewer.setFontUrlResolver — so destroy() clears
  // ONLY a resolver it set, never one a sibling plugin / the consumer installed.
  let didSetResolver = false;

  // Resolve the consumer's font-source override (their CDN) into a resolver and
  // hand it to core, which applies it per face on the next load. A string is a
  // convenience base-origin swap of the default jsDelivr origin.
  if (config.fontSource) {
    const src = config.fontSource;
    const resolver: FontUrlResolver =
      typeof src === "function"
        ? src
        : (info) => info.defaultUrl.replace("https://cdn.jsdelivr.net", src.replace(/\/$/, ""));
    viewer.setFontUrlResolver(resolver);
    didSetResolver = true;
  }

  // Opt-in: tell core to try the user's installed Word-substitute fonts on the
  // next load. Tracked so destroy() only clears a flag THIS plugin set.
  let didEnableLocalFonts = false;
  if (config.useLocalFonts) {
    viewer.setLocalFontsEnabled(true);
    didEnableLocalFonts = true;
  }

  // Per-document readiness wiring (fixed AT SOURCE in core). Core
  // claims each load's generation SYNCHRONOUSLY at `load()` entry (before the
  // call queues on its serialized #loadChain), so a superseded load's whole
  // emission tail (`fontsReady` / `fonts:variant-arrived` / `pageReady`) is
  // suppressed by core's own stale() guard the moment a newer load() is called —
  // a stale doc-A terminal can no longer reach this plugin at all. (The old
  // observer-side guard alone was porous: the FIRST stale terminal always
  // passed the `loadId < latestReadyLoadId` high-water because nothing had
  // advanced it yet, so doc A's fontsReady resolved doc B's arm — premature
  // 'ready' with the wrong count while doc B's 16MB CJK OTF still downloaded.)
  //
  // We still arm ONE promise per open() and keep the high-water below as
  // belt-and-braces against any future emission-path regression in core.
  interface ReadyArm {
    resolve: (count: number) => void;
    reject: (err: unknown) => void;
  }
  let arm: ReadyArm | null = null;
  // High-water of resolved loadIds — a fontsReady whose loadId is BELOW this is a
  // superseded document's late terminal; drop it. (Belt-and-braces: core already
  // suppresses these at source via the load()-entry generation claim.)
  let latestReadyLoadId = -1;
  function freshReadyPromise(): Promise<number> {
    return new Promise<number>((resolve, reject) => {
      arm = { resolve, reject };
    });
  }

  const status = (): FontsStatus =>
    (actor?.getSnapshot().value as FontsStatus) ?? "idle";
  const arrivedFamilies = (): string[] =>
    [...(actor?.getSnapshot().context.arrived ?? [])];

  return {
    id: "fonts",
    deps: ["loader"],

    initialize(ctx: KernelContext) {
      actor = createActor(makeMachine());

      // Mirror machine state onto the replayable kernel bus ('latest' replay), so
      // a late subscriber (e.g. a status pill) catches up — exactly like
      // loader:status-changed.
      actor.subscribe((snapshot) => {
        const s = snapshot.value as FontsStatus;
        if (s !== prevStatus) {
          prevStatus = s;
          ctx.bus.emit("fonts:status-changed", s);
          if (s === "ready") {
            ctx.bus.emit("fonts:ready", snapshot.context.loaded);
          }
        }
      });

      actor.start();

      // RE-PLAN per document — driven off `loader:load-started`.
      //
      // We previously re-planned on `loader:status-changed === "loading"`, but
      // the loader machine SELF-transitions loading→loading on a re-entrant open
      // and the status mirror dedups (`s !== prevStatus`) — so a 2nd open() while
      // the first is still loading emitted NO status change and fonts never
      // re-planned doc B (stayed armed on doc A). `loader:load-started` fires ONCE
      // per open() with no edge-dedup, so EVERY open re-plans exactly once.
      //
      // TIMING (critical, unchanged): the loader emits load-started synchronously
      // inside open(), BEFORE core's #doLoad microtask runs #loadFonts — so the
      // readiness promise is armed before `fontsReady` fires. The PLAN routes
      // through `planning`, whose entry resets arrived/loaded/error, so doc B
      // starts with a clean (no doc-A carryover) arrived set.
      unsubs.push(
        ctx.bus.subscribe("loader:load-started", () => {
          const ready = freshReadyPromise();
          actor?.send({ type: "PLAN", ready });
        }),
      );

      // BRIDGE the Viewer's native font events onto the kernel bus + machine
      // (bridge, don't own). Core stays source-of-truth for font orchestration;
      // we only observe.
      //
      // fontsReady (terminal, per document): resolve the CURRENT document's arm
      // → the invoked actor's onDone → 'ready'. Core suppresses a superseded
      // document's terminal AT SOURCE (generation claimed at load() entry — see
      // the ReadyArm comment above), so by construction only the CURRENT doc's
      // fontsReady reaches us; the loadId high-water below is belt-and-braces.
      // (Empty-plan loads still emit fontsReady({count:0,loadId}), so a doc
      // with no loadable fonts still reaches ready.)
      unsubs.push(
        viewer.on("fontsReady", ({ count, loadId }) => {
          if (loadId < latestReadyLoadId) return; // superseded doc's late terminal
          latestReadyLoadId = loadId;
          arm?.resolve(count);
          arm = null;
        }),
      );

      // loader:error: a failed load (#doLoad throws before #loadFonts
      // ever emits) means `fontsReady` never fires for this doc — the arm would
      // hang in `loading` FOREVER (permanent spinner). Reject the in-flight arm so
      // the invoked fontsActor settles rejected → the machine's loading.onError
      // fires → 'error'. (loader:error is "none" replay, so a late subscriber
      // doesn't re-trigger a stale error.)
      unsubs.push(
        ctx.bus.subscribe("loader:error", (err) => {
          arm?.reject(err);
          arm = null;
        }),
      );

      // fonts:refreshed (viewer.refreshFonts, the live re-apply):
      // bridge onto the kernel bus in BOTH cases (observers re-read fonts.debug),
      // and on the CHANGED path clear the stale page-lifecycle replay logs FIRST.
      // Core emits changed:true BEFORE the fresh layout's first pageReady (a
      // documented ordering guarantee), so clearing here drops exactly the
      // superseded layout's entries: a late bus subscriber then replays only the
      // refresh's own stream, and a boundary subscriber (the snippet's scroll
      // restore) sees only the refresh's own loadComplete — never the sticky
      // stale one. A changed:false refresh re-laid out nothing, so the CURRENT
      // document's replay log must survive it untouched.
      unsubs.push(
        viewer.on("fonts:refreshed", (payload) => {
          if (payload.changed) {
            ctx.bus.clearReplay("pageReady");
            ctx.bus.clearReplay("loadComplete");
          }
          ctx.bus.emit("fonts:refreshed", payload);
        }),
      );

      // fonts:variant-arrived (per family): record into the machine AND re-emit on
      // the kernel bus. plugin-viewport subscribes the bus event and re-renders the
      // currently-visible painted pages (the single invalidate-visible channel), so
      // CJK / substituted text picks up its face the instant THAT family lands —
      // not after the slowest face's terminal fontsReady.
      unsubs.push(
        viewer.on("fonts:variant-arrived", (payload) => {
          actor?.send({ type: "FACE_ARRIVED", family: payload.family });
          ctx.bus.emit("fonts:variant-arrived", payload);
        }),
      );

      // Commands (the plugin surface).
      unsubs.push(
        ctx.commands.register("fonts.status", () => status()),
        ctx.commands.register("fonts.arrived", () => arrivedFamilies()),
        // fonts.debug: the merged live font-truth report. THIN CALLER —
        // both halves come from core: fontProvenance() is the probe-independent
        // TS truth (works in worker mode + probe-OFF builds), debugFonts() is the
        // wasm probe (core routes it over RPC in worker mode and degrades to
        // undefined on probe-OFF / stale bundles).
        ctx.commands.register("fonts.debug", async (): Promise<FontsDebugReport> => {
        const snap = viewer.fontProvenance();
        type ProbeRow = {
          used: string;
          resolvesNow?: string | null;
          fellBackToDefault?: boolean | null;
          alias?: string | null;
          metrics?: FontsDebugRow["metrics"];
        };
        let probe: { fonts?: ProbeRow[] } | undefined;
        try {
          probe = (await viewer.debugFonts()) as { fonts?: ProbeRow[] } | undefined;
        } catch {
          probe = undefined; // belt-and-braces: core already degrades internally
        }
        const probeAvailable = probe != null;
        const probeRows = new Map((probe?.fonts ?? []).map((r) => [r.used, r]));
        const fonts = snap.families.map((f): FontsDebugRow => {
          const row = probeRows.get(f.declared);
          return {
            declared: f.declared,
            resolved: f.resolved,
            origin: f.origin,
            freshness: f.freshness,
            matchTier: f.matchTier, // the resolution-ladder rung (core truth)
            matchedFamily: f.matchedFamily, // the LOCAL family behind the bytes (null when cdn)
            coverage: f.coverage,
            probe: probeAvailable
              ? {
                  resolvesNow: row?.resolvesNow ?? null,
                  fellBackToDefault: row?.fellBackToDefault ?? null,
                  alias: row?.alias ?? null,
                }
              : "unavailable",
            loadedVariants: f.loadedVariants,
            metrics: row?.metrics ?? null,
          };
        });
        return {
          useLocalFonts: snap.useLocalFonts,
          // Live: true while a viewer.refreshFonts() pipeline is queued or in
          // flight (the FONTS tab re-renders on fonts:variant-arrived during a
          // refresh, so the flag is observable mid-flight).
          refreshing: viewer.isRefreshingFonts(),
          probeAvailable,
          // The permission line + catalog readiness (core truth; the
          // FONTS tab shows them and the one-click affordance keys off them).
          permissionState: await viewer.fontPermissionState(),
          catalogReady: snap.catalogReady,
          fonts,
        };
      }),
      );
    },

    destroy(_ctx: KernelContext) {
      for (const u of unsubs.splice(0)) u();
      // Clear the resolver ONLY if this plugin installed it, so we don't clobber
      // a resolver a sibling plugin or the consumer set on the shared Viewer
      // (core supports null = restore bundled defaults).
      if (didSetResolver) viewer.setFontUrlResolver(null);
      if (didEnableLocalFonts) viewer.setLocalFontsEnabled(false);
      actor?.stop();
      actor = null;
      prevStatus = null;
      arm = null;
    },
  };
}
