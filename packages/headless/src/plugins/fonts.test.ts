/**
 * plugin-fonts tests (the 2nd of the two genuine XState machines).
 *
 * The fonts plugin OBSERVES core's font load: it does NOT relocate the
 * orchestration. We exercise it against the SAME FakeWasm injection trick the
 * loader test uses (a fake wasm into the private Viewer ctor), driving core's
 * real `#loadFontsFor` so the bridge (viewer `fontsReady` / `fonts:variant-
 * arrived` → kernel bus + machine) is exercised end to end.
 *
 * Two FakeWasm flavours:
 *   - EmptyFontsWasm: fontFamiliesUsed() → [] → buildFontPlan empty → core emits
 *     a terminal fontsReady(0) after an empty Promise.all. Drives status
 *     transitions + per-document re-plan WITHOUT needing network/FontFace.
 *   - PlannedFontsWasm: fontFamiliesUsed() → ["Calibri"] → buildFontPlan maps to
 *     the Carlito substitute (4 variants). We stub the browser globals
 *     (fetch/FontFace/document.fonts) so core's #loadFontsFor runs to completion
 *     and emits the per-family `fonts:variant-arrived` — proving the per-face
 *     bridge + machine accumulation.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { Viewer, type FontUrlResolver } from "@parasma/core";
import { createKernel } from "../kernel";
import { loaderPlugin } from "./loader";
import { fontsPlugin } from "./fonts";

class FakeWasm {
  log: string[] = [];
  #resolveRun: ((total: number) => void) | null = null;

  loadDocx(_bytes: Uint8Array): void {
    this.log.push("loadDocx");
  }
  abortLayout(): void {
    this.log.push("abort");
    const r = this.#resolveRun;
    this.#resolveRun = null;
    r?.(0);
  }
  async runLayout(_cb: (idx: number) => void): Promise<number> {
    this.log.push("runLayout");
    return new Promise<number>((res) => {
      this.#resolveRun = res;
    });
  }
  setNow(_n: number): void {}
  pageSize(): { widthPx: number; heightPx: number } {
    return { widthPx: 600, heightPx: 800 };
  }
  fontFamiliesUsed(): string[] {
    return [];
  }
  addFontAlias(_doc: string, _sub: string): void {}
  loadFont(_f: string, _b: Uint8Array, _w: number, _i: boolean): void {}
  imageRefs(): unknown[] {
    return [];
  }
  clearImageBytes(): void {}
}

/** Drives core's per-family `fonts:variant-arrived` by reporting a real family. */
class PlannedFontsWasm extends FakeWasm {
  override fontFamiliesUsed(): string[] {
    return ["Calibri"]; // → Carlito substitute in buildFontPlan
  }
}

/** A DISTINCT non-empty plan → Arimo, so cross-doc accumulation is detectable. */
class PlannedFontsWasmB extends FakeWasm {
  override fontFamiliesUsed(): string[] {
    return ["Arial"]; // → Arimo substitute in buildFontPlan (distinct from Carlito)
  }
}

function makeViewer(wasm: FakeWasm): Viewer {
  return new (Viewer as unknown as { new (w: unknown): Viewer })(wasm);
}

async function bootKernelWith(viewer: Viewer) {
  const kernel = createKernel();
  kernel.use(loaderPlugin(viewer));
  kernel.use(fontsPlugin(viewer));
  await kernel.boot();
  return kernel;
}

describe("plugin-fonts", () => {
  it("(a) status transitions idle → planning → loading → ready on a load", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const kernel = await bootKernelWith(viewer);

    const statuses: string[] = [];
    kernel.bus.subscribe("fonts:status-changed", (s) => statuses.push(s as string));

    expect(await kernel.exec("fonts.status")).toBe("idle");

    await kernel.exec("loader.open", new Uint8Array([1]));
    // Let the readiness promise (resolved by core's fontsReady inside load) settle
    // and the invoked actor's onDone microtask flush.
    await Promise.resolve();
    await Promise.resolve();

    expect(await kernel.exec("fonts.status")).toBe("ready");
    // planning is transient (always → loading); we at least see loading + ready.
    expect(statuses).toContain("loading");
    expect(statuses).toContain("ready");
  });

  it("(b) per-document re-plan: a second load re-enters loading then ready", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const kernel = await bootKernelWith(viewer);

    await kernel.exec("loader.open", new Uint8Array([1]));
    await Promise.resolve();
    await Promise.resolve();
    expect(await kernel.exec("fonts.status")).toBe("ready");

    const statuses: string[] = [];
    kernel.bus.subscribe("fonts:status-changed", (s) => statuses.push(s as string));

    // Second document → re-plan: ready → planning → loading → ready.
    await kernel.exec("loader.open", new Uint8Array([2]));
    await Promise.resolve();
    await Promise.resolve();

    expect(statuses).toContain("loading");
    expect(statuses[statuses.length - 1]).toBe("ready");
    expect(await kernel.exec("fonts.status")).toBe("ready");
  });

  it("(c) ready emits a terminal fonts:ready on the bus", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const kernel = await bootKernelWith(viewer);

    let readyCount: number | null = null;
    kernel.bus.subscribe("fonts:ready", (c) => {
      readyCount = c as number;
    });

    await kernel.exec("loader.open", new Uint8Array([1]));
    await Promise.resolve();
    await Promise.resolve();

    // Empty plan → fontsReady(0); the machine's snapshot mirror re-emits it.
    expect(readyCount).toBe(0);
  });

  describe("per-face arrival (with stubbed browser font globals)", () => {
    let origFetch: unknown;
    let origFontFace: unknown;
    let origDocument: unknown;
    let origCaches: unknown;

    beforeEach(() => {
      const g = globalThis as Record<string, unknown>;
      origFetch = g.fetch;
      origFontFace = g.FontFace;
      origDocument = g.document;
      origCaches = g.caches;

      // No Cache API → core's fetchWithCache falls through to plain fetch.
      g.caches = undefined;
      g.fetch = vi.fn(async () => ({
        ok: true,
        arrayBuffer: async () => new ArrayBuffer(8),
      }));
      g.FontFace = class {
        constructor(_f: string, _b: ArrayBuffer, _d: unknown) {}
        async load() {
          return this;
        }
      };
      g.document = { fonts: { add: vi.fn() } };
    });

    afterEach(() => {
      const g = globalThis as Record<string, unknown>;
      g.fetch = origFetch;
      g.FontFace = origFontFace;
      g.document = origDocument;
      g.caches = origCaches;
    });

    it("(d) per-family fonts:variant-arrived is bridged onto the bus + recorded in context", async () => {
      const wasm = new PlannedFontsWasm();
      const viewer = makeViewer(wasm);
      const kernel = await bootKernelWith(viewer);

      const arrivals: string[] = [];
      kernel.bus.subscribe("fonts:variant-arrived", (p) =>
        arrivals.push((p as { family: string }).family),
      );

      await kernel.exec("loader.open", new Uint8Array([1]));
      // Let all 4 stubbed face fetch/load microtasks settle.
      for (let i = 0; i < 10; i++) await Promise.resolve();

      // Carlito is the single substitute family for Calibri → exactly one
      // per-family arrival, bridged onto the bus AND recorded in machine context.
      expect(arrivals).toContain("Carlito");
      const recorded = (await kernel.exec("fonts.arrived")) as string[];
      expect(recorded).toContain("Carlito");
      expect(await kernel.exec("fonts.status")).toBe("ready");
    });

    // ── Adversarial concurrency (the storm the sequential suite cannot catch) ──

    it("(i) a 2nd open() before the first resolves → exactly ONE fresh PLAN, correct final count, reset arrived", async () => {
      // Re-planning is keyed off `loader:load-started` (once per open, no status-edge
      // dedup). Two opens in flight must yield exactly ONE additional re-plan for
      // the 2nd doc (planning resets the arrived set), and the final readiness
      // count/status must be doc B's (Carlito), with no doc-A carryover.
      // Doc A → Carlito, doc B → Arimo (distinct), so doc-A carryover into doc B
      // would be directly observable as a stale "Carlito" in doc B's arrived set.
      const wasm = new PlannedFontsWasm();
      const viewer = makeViewer(wasm);
      const kernel = await bootKernelWith(viewer);

      // Count the re-plan DRIVER: `loader:load-started` fires once per open with
      // no status-edge dedup, so it is the ground truth that fonts re-plans per
      // open. (The transient `planning` macrostep collapses into `loading` in one
      // XState step, so the status mirror does not reliably surface "planning";
      // the old status-edge bug surfaced precisely because loading→loading was
      // suppressed — which is exactly what `loader:load-started` sidesteps.)
      let started = 0;
      kernel.bus.subscribe("loader:load-started", () => started++);

      // Fire both opens WITHOUT awaiting between — the 2nd lands while the 1st is
      // still loading (loading→loading on the loader machine). Doc B swaps to
      // Arial→Arimo, synchronously, before its open is sent.
      const p1 = kernel.exec("loader.open", new Uint8Array([1]));
      (wasm as unknown as { fontFamiliesUsed: () => string[] }).fontFamiliesUsed =
        () => ["Arial"];
      const p2 = kernel.exec("loader.open", new Uint8Array([2]));
      await Promise.all([p1.catch(() => {}), p2.catch(() => {})]);
      for (let i = 0; i < 12; i++) await Promise.resolve();

      // Exactly two opens → exactly two load-started signals (one re-plan each) —
      // NOT zero re-plans for the 2nd (the old status-edge bug suppressed the
      // loading→loading self-transition so doc B never re-planned).
      expect(started).toBe(2);
      expect(await kernel.exec("fonts.status")).toBe("ready");
      // arrived is doc B's clean set: Arimo only — the 2nd PLAN re-entered
      // `planning` (reset arrived) so doc A's Carlito did NOT carry over.
      const recorded = (await kernel.exec("fonts.arrived")) as string[];
      expect(recorded).toEqual(["Arimo"]);
      expect(recorded).not.toContain("Carlito");
    });

    it("(iv) deferred-fetch doc switch: a SUPERSEDED doc's late font terminal cannot resolve the NEW doc's readiness", async () => {
      // THE generation seam (fixed AT SOURCE in core): doc A suspends in
      // #loadFonts on hung Carlito fetches; doc B opens (re-arming the plugin's
      // readiness for B); A's fetches then release. Before the fix, core bumped
      // #loadGeneration only inside #doLoad* — A still looked "current" (B's
      // #doLoad hadn't had its serialized chain turn), so A's fontsReady fired
      // and, being the FIRST terminal, passed the observer's high-water guard:
      // premature 'ready' with A's count (clearing the spinner while B's fonts
      // never even started), A's Carlito contaminating B's arrived set, and B's
      // REAL terminal later dropped on a null arm. Now load() claims the
      // generation at ENTRY, so A's whole tail is suppressed at source.
      const pending = new Map<string, Array<() => void>>();
      const g = globalThis as Record<string, unknown>;
      g.fetch = vi.fn(
        (url: string) =>
          new Promise((res) => {
            const bucket = /carlito/i.test(url) ? "carlito" : "arimo";
            const list = pending.get(bucket) ?? [];
            list.push(() => res({ ok: true, arrayBuffer: async () => new ArrayBuffer(8) }));
            pending.set(bucket, list);
          }),
      );
      const release = (bucket: string) => {
        for (const r of pending.get(bucket) ?? []) r();
        pending.delete(bucket);
      };

      const wasm = new PlannedFontsWasm(); // doc A: Calibri → Carlito
      const viewer = makeViewer(wasm);
      const kernel = await bootKernelWith(viewer);

      const p1 = kernel.exec("loader.open", new Uint8Array([1]));
      // Let doc A reach its (hung) Carlito fetches.
      for (let i = 0; i < 8; i++) await Promise.resolve();

      // Doc B: Arial → Arimo. Open while A is suspended mid-#loadFonts.
      (wasm as unknown as { fontFamiliesUsed: () => string[] }).fontFamiliesUsed =
        () => ["Arial"];
      const p2 = kernel.exec("loader.open", new Uint8Array([2]));
      for (let i = 0; i < 8; i++) await Promise.resolve();
      expect(await kernel.exec("fonts.status")).toBe("loading");

      // Release the SUPERSEDED doc's fetches. Its terminal must change NOTHING:
      // status stays 'loading' until doc B's OWN terminal, and B's arrived set
      // excludes A's family.
      release("carlito");
      for (let i = 0; i < 16; i++) await Promise.resolve();
      expect(await kernel.exec("fonts.status")).toBe("loading");
      expect((await kernel.exec("fonts.arrived")) as string[]).not.toContain("Carlito");

      // Doc B's own faces land → ITS terminal flips ready.
      release("arimo");
      await Promise.all([p1, p2]);
      for (let i = 0; i < 16; i++) await Promise.resolve();
      expect(await kernel.exec("fonts.status")).toBe("ready");
      expect((await kernel.exec("fonts.arrived")) as string[]).toEqual(["Arimo"]);
    });

    it("(ii) two DISTINCT non-empty loads → fonts:ready count AND arrived set scoped to doc B", async () => {
      // Doc A uses Calibri→Carlito; doc B uses Arial→Arimo. After both settle
      // sequentially, the arrived set + ready count must reflect ONLY doc B —
      // no cross-doc accumulation (planning entry resets arrived/loaded; the
      // generation-tagged fontsReady scopes the count to doc B).
      const wasmA = new PlannedFontsWasm();
      const viewerA = makeViewer(wasmA);
      const kernel = await bootKernelWith(viewerA);

      const readyCounts: number[] = [];
      kernel.bus.subscribe("fonts:ready", (c) => readyCounts.push(c as number));

      await kernel.exec("loader.open", new Uint8Array([1]));
      for (let i = 0; i < 12; i++) await Promise.resolve();
      expect((await kernel.exec("fonts.arrived")) as string[]).toEqual(["Carlito"]);

      // Swap the family the SAME viewer reports for the next load (doc B).
      (wasmA as unknown as { fontFamiliesUsed: () => string[] }).fontFamiliesUsed =
        () => ["Arial"];
      await kernel.exec("loader.open", new Uint8Array([2]));
      for (let i = 0; i < 12; i++) await Promise.resolve();

      // arrived is scoped to doc B (Arimo only) — Carlito did NOT carry over.
      const recorded = (await kernel.exec("fonts.arrived")) as string[];
      expect(recorded).toEqual(["Arimo"]);
      expect(recorded).not.toContain("Carlito");
      // Each load resolved its OWN terminal count (4 faces each), not a merge.
      expect(readyCounts[readyCounts.length - 1]).toBe(4);
      expect(await kernel.exec("fonts.status")).toBe("ready");
    });
  });

  it("(iii) a load FAILURE → fonts reaches 'error', not a permanent 'loading' spinner", async () => {
    // Without error wiring the machine hung in `loading` forever. Now
    // loader:error rejects the in-flight arm → loading.onError → 'error'.
    class ThrowWasm extends FakeWasm {
      override loadDocx(_bytes: Uint8Array): void {
        throw new Error("boom");
      }
    }
    const wasm = new ThrowWasm();
    const viewer = makeViewer(wasm);
    const kernel = await bootKernelWith(viewer);

    await expect(kernel.exec("loader.open", new Uint8Array([1]))).rejects.toThrow();
    for (let i = 0; i < 6; i++) await Promise.resolve();

    const st = (await kernel.exec("fonts.status")) as string;
    expect(st).not.toBe("loading");
    expect(st).toBe("error");
  });

  it("(iii-b) TWO consecutive failures with the SAME raw-string rejection both reach 'error' (no stuck spinner)", async () => {
    // wasm rejects with raw strings ('Could not find EOCD'), so back-to-back
    // failures reject with === values. The loader's old payload-identity dedup
    // swallowed the 2nd loader:error → the re-armed fonts machine stranded in
    // 'loading' forever (REPRODUCED live: corrupt docx, then an empty file →
    // perpetual "Loading fonts…" spinner). Settles now emit per state-entry
    // edge, so the 2nd failure also rejects the arm.
    class EocdWasm extends FakeWasm {
      override loadDocx(_bytes: Uint8Array): void {
        // eslint-disable-next-line no-throw-literal
        throw "Could not find EOCD";
      }
    }
    const wasm = new EocdWasm();
    const viewer = makeViewer(wasm);
    const kernel = await bootKernelWith(viewer);

    await expect(kernel.exec("loader.open", new Uint8Array([1]))).rejects.toBe("Could not find EOCD");
    for (let i = 0; i < 6; i++) await Promise.resolve();
    expect(await kernel.exec("fonts.status")).toBe("error");

    await expect(kernel.exec("loader.open", new Uint8Array([2]))).rejects.toBe("Could not find EOCD");
    for (let i = 0; i < 6; i++) await Promise.resolve();
    const st = (await kernel.exec("fonts.status")) as string;
    expect(st).not.toBe("loading"); // the stuck-spinner state
    expect(st).toBe("error");
  });

  it("(e) error: a load whose engine throws drives fonts → error (no permanent spinner)", async () => {
    // Core's #doLoad throws BEFORE #loadFonts emits fontsReady, so the
    // readiness promise never resolves on its own. The loader machine reaches
    // `error` and emits `loader:error`; plugin-fonts now subscribes that and
    // rejects the in-flight arm → the invoked fontsActor settles rejected →
    // loading.onError → the fonts machine lands in `error` (NOT a permanent
    // `loading` spinner). It then recovers on the next successful load.
    class ThrowOnceWasm extends FakeWasm {
      threw = false;
      override loadDocx(bytes: Uint8Array): void {
        if (!this.threw) {
          this.threw = true;
          throw new Error("boom");
        }
        super.loadDocx(bytes);
      }
    }
    const wasm = new ThrowOnceWasm();
    const viewer = makeViewer(wasm);
    const kernel = await bootKernelWith(viewer);

    await expect(kernel.exec("loader.open", new Uint8Array([1]))).rejects.toThrow();
    await Promise.resolve();
    await Promise.resolve();
    // The failed load reaches a NON-loading terminal status — error, not a hang.
    expect(await kernel.exec("fonts.status")).toBe("error");

    // A fresh successful load re-plans → ready (recovery).
    await kernel.exec("loader.open", new Uint8Array([2]));
    await Promise.resolve();
    await Promise.resolve();
    expect(await kernel.exec("fonts.status")).toBe("ready");
  });

  // ── Consumer font-source override (their own CDN / CloudFront) ─────────────
  it("(f) fontSource string rewrites the default jsDelivr origin to the consumer CDN", () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    let captured: FontUrlResolver | null = null;
    viewer.setFontUrlResolver = (r) => { captured = r; }; // spy
    fontsPlugin(viewer, { fontSource: "https://d123.cloudfront.net/fonts/" });
    expect(typeof captured).toBe("function");
    const out = captured!({
      family: "Carlito", variant: "regular", weight: 400, italic: false,
      defaultUrl: "https://cdn.jsdelivr.net/gh/googlefonts/carlito@main/fonts/ttf/Carlito-Regular.ttf",
      documentFamily: "Calibri",
    });
    // Trailing slash on the base is normalised; the default path is preserved.
    expect(out).toBe("https://d123.cloudfront.net/fonts/gh/googlefonts/carlito@main/fonts/ttf/Carlito-Regular.ttf");
  });

  // ── Local-font fidelity opt-in (Font Access API) ──────────────────────────
  it("(h) useLocalFonts:true enables local fonts on the viewer; omitting it leaves them untouched", () => {
    const wasm = new FakeWasm();
    const v1 = makeViewer(wasm);
    const calls1: boolean[] = [];
    v1.setLocalFontsEnabled = (b) => { calls1.push(b); };
    fontsPlugin(v1, { useLocalFonts: true });
    expect(calls1).toEqual([true]);

    const v2 = makeViewer(wasm);
    let called = false;
    v2.setLocalFontsEnabled = () => { called = true; };
    fontsPlugin(v2); // no config → never enabled
    expect(called).toBe(false);
  });

  it("(h-b) destroy() clears local fonts only if this plugin enabled them", async () => {
    const wasm = new FakeWasm();
    const viewer = makeViewer(wasm);
    const calls: boolean[] = [];
    viewer.setLocalFontsEnabled = (b) => { calls.push(b); };

    const kernel = createKernel();
    kernel.use(loaderPlugin(viewer));
    kernel.use(fontsPlugin(viewer, { useLocalFonts: true }));
    await kernel.boot();
    expect(calls).toEqual([true]);

    await kernel.destroy();
    expect(calls).toEqual([true, false]); // re-disabled on teardown
  });

  // ── fonts.debug: the merged TS-provenance + wasm-probe report ────────────
  describe("fonts.debug (with stubbed browser font globals)", () => {
    let origFetch: unknown;
    let origFontFace: unknown;
    let origDocument: unknown;
    let origCaches: unknown;

    beforeEach(() => {
      const g = globalThis as Record<string, unknown>;
      origFetch = g.fetch;
      origFontFace = g.FontFace;
      origDocument = g.document;
      origCaches = g.caches;
      g.caches = undefined; // fetchWithCache falls through to plain fetch
      g.fetch = vi.fn(async () => ({
        ok: true,
        arrayBuffer: async () => new ArrayBuffer(8),
      }));
      g.FontFace = class {
        constructor(_f: string, _b: ArrayBuffer, _d: unknown) {}
        async load() {
          return this;
        }
      };
      g.document = { fonts: { add: vi.fn(), delete: vi.fn() } };
    });

    afterEach(() => {
      const g = globalThis as Record<string, unknown>;
      g.fetch = origFetch;
      g.FontFace = origFontFace;
      g.document = origDocument;
      g.caches = origCaches;
    });

    /** PlannedFontsWasm + the debug-probe surface (families report). */
    class ProbeWasm extends PlannedFontsWasm {
      debugFonts(): unknown {
        return {
          aliases: [{ declared: "calibri", effective: "carlito" }],
          faces: [
            {
              family: "carlito",
              weight: 400,
              italic: false,
              unitsPerEm: 2048,
              ascentEm: 0.952,
              descentEm: 0.269,
              lineGapEm: 0,
            },
          ],
          families: [
            { requested: "Calibri", alias: "carlito", resolvesNow: "carlito", fellBackToDefault: false },
          ],
        };
      }
    }

    it("(j) probe-present: merged rows carry provenance + probe fields + metrics", async () => {
      const wasm = new ProbeWasm();
      const viewer = makeViewer(wasm);
      const kernel = await bootKernelWith(viewer);

      await kernel.exec("loader.open", new Uint8Array([1]));
      for (let i = 0; i < 10; i++) await Promise.resolve();

      const dbg = (await kernel.exec("fonts.debug")) as {
        useLocalFonts: boolean;
        refreshing: boolean;
        probeAvailable: boolean;
        permissionState: string;
        catalogReady: boolean;
        fonts: Array<{
          declared: string;
          resolved: string | null;
          origin: string | null;
          freshness: string | null;
          matchTier: number | string | null;
          coverage: string;
          probe: { resolvesNow: string | null; fellBackToDefault: boolean | null; alias: string | null } | "unavailable";
          loadedVariants: string[];
          metrics: { unitsPerEm: number } | null;
        }>;
      };

      expect(dbg.useLocalFonts).toBe(false);
      expect(dbg.refreshing).toBe(false);
      expect(dbg.probeAvailable).toBe(true);
      // No Permissions API in the test env
      // → "unsupported"; the session catalog was never enumerated.
      expect(dbg.permissionState).toBe("unsupported");
      expect(dbg.catalogReady).toBe(false);
      const row = dbg.fonts.find((f) => f.declared === "Calibri")!;
      expect(row.resolved).toBe("Carlito");
      expect(row.origin).toBe("cdn");
      expect(row.freshness).toBe("fresh");
      expect(row.matchTier).toBeNull(); // CDN-sourced face — no local ladder hit
      // Coverage: CDN substitute, no catalog/probe knowledge → "substituted".
      expect(row.coverage).toBe("substituted");
      expect(row.probe).toEqual({ resolvesNow: "carlito", fellBackToDefault: false, alias: "carlito" });
      expect(row.loadedVariants).toContain("regular");
      expect(row.metrics).toMatchObject({ unitsPerEm: 2048 });
    });

    it("(j2) NON-DEFAULT local-font fields pass through: local hit → matchTier/coverage/catalogReady are live, not hardcoded defaults", async () => {
      // The (j)/(k) cases assert only DEFAULT values
      // (null/"substituted"/"unsupported"/false), so hardcoding every local-font
      // field would pass them. This case forces non-defaults end to end.
      const g = globalThis as Record<string, unknown>;
      // A working Cache API (the local path requires it).
      const store = new Map<string, { buf: ArrayBuffer; headers: Headers }>();
      g.caches = {
        open: async () => ({
          match: async (k: string) => {
            const e = store.get(k);
            return e ? { arrayBuffer: async () => e.buf, headers: e.headers } : undefined;
          },
          put: async (k: string, resp: Response) => {
            store.set(k, { buf: await resp.arrayBuffer(), headers: resp.headers });
          },
          delete: async (k: string) => store.delete(k),
        }),
      };

      class TnrWasm extends FakeWasm {
        override fontFamiliesUsed(): string[] {
          return ["Times New Roman"];
        }
      }
      const wasm = new TnrWasm();
      const viewer = makeViewer(wasm);
      // Font Access path with an injected catalog: TNR present → tier-1 hit.
      const face = (style: string, ps: string) => ({
        family: "Times New Roman",
        fullName: `Times New Roman ${style}`.trim(),
        postscriptName: ps,
        style: style || "Regular",
        blob: async () => new Blob([new Uint8Array([7])]),
      });
      viewer.setLocalFontSource({
        queryLocalFonts: async () => [
          face("", "TimesNewRomanPSMT"),
          face("Bold", "TimesNewRomanPS-BoldMT"),
        ],
        hasUserActivation: () => true,
      });
      viewer.setLocalFontsEnabled(true);
      const kernel = await bootKernelWith(viewer);

      await kernel.exec("loader.open", new Uint8Array([1]));
      for (let i = 0; i < 20; i++) await Promise.resolve();

      const dbg = (await kernel.exec("fonts.debug")) as {
        catalogReady: boolean;
        useLocalFonts: boolean;
        fonts: Array<{
          declared: string;
          origin: string | null;
          matchTier: number | string | null;
          coverage: string;
        }>;
      };
      expect(dbg.useLocalFonts).toBe(true);
      expect(dbg.catalogReady).toBe(true); // enumeration ran
      const row = dbg.fonts.find((f) => f.declared === "Times New Roman")!;
      expect(row.origin).toBe("local");
      expect(row.matchTier).toBe(1); // exact family match — not the null default
      expect(row.coverage).toBe("exact-local"); // not the "substituted" default
    });

    it("(k) probe-absent: rows still carry TS-side truth; probe marked unavailable", async () => {
      const wasm = new PlannedFontsWasm(); // no debugFonts export (probe OFF)
      const viewer = makeViewer(wasm);
      const kernel = await bootKernelWith(viewer);

      await kernel.exec("loader.open", new Uint8Array([1]));
      for (let i = 0; i < 10; i++) await Promise.resolve();

      const dbg = (await kernel.exec("fonts.debug")) as {
        probeAvailable: boolean;
        fonts: Array<{ declared: string; origin: string | null; probe: unknown; loadedVariants: string[]; metrics: unknown }>;
      };
      expect(dbg.probeAvailable).toBe(false);
      const row = dbg.fonts.find((f) => f.declared === "Calibri")!;
      expect(row.origin).toBe("cdn"); // byte-source provenance is probe-independent
      expect(row.probe).toBe("unavailable");
      expect(row.loadedVariants).toContain("regular"); // from the FontFace registry, not the probe
      expect(row.metrics).toBeNull();
    });

    /**
     * refreshFonts lifecycle. Layout N streams N+1 pages so the
     * pre-refresh and post-refresh page lifecycles are distinguishable on the
     * replayable bus (2 pages, then 3).
     */
    class RefreshWasm extends FakeWasm {
      layoutRuns = 0;
      override fontFamiliesUsed(): string[] {
        return ["DejaVu Sans"]; // → localCandidates ["Verdana"] (a real local intent)
      }
      override async runLayout(cb: (idx: number) => void): Promise<number> {
        const pages = 2 + this.layoutRuns++;
        await Promise.resolve(); // mirror the real wasm: no synchronous pageReady
        for (let i = 0; i < pages; i++) cb(i);
        return pages;
      }
    }

    it("(m) fonts:refreshed is bridged onto the bus and clears the STALE page-lifecycle replay", async () => {
      const wasm = new RefreshWasm();
      const viewer = makeViewer(wasm);
      // A supported local byte-source so the useLocalFonts intent is real.
      viewer.setLocalFontSource({ fetchLocalFont: async () => new ArrayBuffer(8) });
      const kernel = await bootKernelWith(viewer);

      await kernel.exec("loader.open", new Uint8Array([1]));
      for (let i = 0; i < 12; i++) await Promise.resolve();

      // The first layout streamed pages 0,1 onto the replayable bus.
      const before: number[] = [];
      kernel.bus.subscribe("pageReady", (i) => before.push(i as number));
      expect(before).toEqual([0, 1]);

      const refreshed: Array<{ changed: boolean }> = [];
      // At the refresh boundary the stale loadComplete replay must ALSO be
      // gone: a subscriber attaching here (the snippet's scroll-restore
      // once-handler) must see ONLY the refresh's own completion (3), never a
      // sticky replay of the old one (2).
      const completionsSeenAtBoundary: number[] = [];
      kernel.bus.subscribe("fonts:refreshed", (p) => {
        refreshed.push(p as { changed: boolean });
        kernel.bus.subscribe("loadComplete", (t) => completionsSeenAtBoundary.push(t as number));
      });

      viewer.setLocalFontsEnabled(true);
      const res = await viewer.refreshFonts();
      for (let i = 0; i < 16; i++) await Promise.resolve();

      expect(res.changed).toBe(true);
      expect(refreshed).toEqual([{ changed: true }]);
      // A LATE subscriber replays ONLY the refresh's own page stream (the new
      // 3-page layout) — the prior layout's entries were cleared, not appended
      // to ([0,1,0,1,2] would mean the stale log leaked through).
      const after: number[] = [];
      kernel.bus.subscribe("pageReady", (i) => after.push(i as number));
      expect(after).toEqual([0, 1, 2]);
      expect(completionsSeenAtBoundary).toEqual([3]); // no stale sticky 2
    });

    it("(m-b) a changed:false refresh re-emits on the bus but does NOT clear the page replay", async () => {
      const wasm = new RefreshWasm();
      const viewer = makeViewer(wasm);
      const kernel = await bootKernelWith(viewer);

      await kernel.exec("loader.open", new Uint8Array([1]));
      for (let i = 0; i < 12; i++) await Promise.resolve();

      const refreshed: Array<{ changed: boolean }> = [];
      kernel.bus.subscribe("fonts:refreshed", (p) => refreshed.push(p as { changed: boolean }));

      const res = await viewer.refreshFonts(); // toggle unchanged → no-op
      expect(res.changed).toBe(false);
      expect(refreshed).toEqual([{ changed: false }]);
      // The CURRENT document's replay log survives a no-op refresh.
      const late: number[] = [];
      kernel.bus.subscribe("pageReady", (i) => late.push(i as number));
      expect(late).toEqual([0, 1]);
    });

    it("(n) fonts.debug's refreshing field mirrors viewer.isRefreshingFonts() live", async () => {
      const wasm = new PlannedFontsWasm();
      const viewer = makeViewer(wasm);
      const kernel = await bootKernelWith(viewer);
      await kernel.exec("loader.open", new Uint8Array([1]));
      for (let i = 0; i < 10; i++) await Promise.resolve();

      let dbg = (await kernel.exec("fonts.debug")) as { refreshing: boolean };
      expect(dbg.refreshing).toBe(false);

      viewer.isRefreshingFonts = () => true; // an in-flight refresh, stubbed
      dbg = (await kernel.exec("fonts.debug")) as { refreshing: boolean };
      expect(dbg.refreshing).toBe(true); // RED: the old report hardcoded false
    });

    it("(l) fonts.debug is unregistered on destroy (alongside fonts.status)", async () => {
      const viewer = makeViewer(new FakeWasm());
      const kernel = await bootKernelWith(viewer);
      expect(kernel.commands.has("fonts.debug")).toBe(true);
      await kernel.destroy();
      expect(kernel.commands.has("fonts.debug")).toBe(false);
    });
  });

  it("(g) fontSource function is forwarded verbatim; no config leaves the resolver unset", () => {
    const wasm = new FakeWasm();
    const v1 = makeViewer(wasm);
    let r1: FontUrlResolver | null = null;
    v1.setFontUrlResolver = (r) => { r1 = r; };
    const fn: FontUrlResolver = (i) => `https://acme.example/${i.family}-${i.variant}.ttf`;
    fontsPlugin(v1, { fontSource: fn });
    expect(r1).toBe(fn);

    const v2 = makeViewer(wasm);
    let called = false;
    v2.setFontUrlResolver = () => { called = true; };
    fontsPlugin(v2); // no config → bundled defaults, resolver never set
    expect(called).toBe(false);
  });
});
