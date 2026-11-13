/// <reference types="vite/client" />
/**
 * createHeadlessViewer — the CHROME-FREE composition entry (the primary embed path).
 *
 * Composes @parasma/core (Viewer) + @parasma/headless (kernel + 13 plugins) + the
 * DOM adapters ({@link attachDom}) over a caller-supplied `root`, and returns a
 * {@link HeadlessViewerHandle} with the load → unload → destroy verbs + zoom/nav +
 * the kernel/viewer/scope/dom escape hatches. It renders the interactive document
 * canvas into the slot with NO default toolbar/formula-bar/sheet-tabs/dev-shell —
 * the parent brings its own chrome (or none).
 *
 * mountViewer (the batteries-included default) is REBUILT ON THIS (not a parallel
 * copy): it calls createHeadlessViewer and layers the default chrome on the
 * returned `dom` binding. This module is the DRY core.
 *
 * SIDE-EFFECT-FREE / BARREL-FREE (the load-bearing headless invariant): imports
 * only style-adopt (runtime delivery) + the adapters/root-scope/attach-dom — NO chrome/*, NO
 * @fontsource, NO ./element, NO ./index. Importing it registers NO custom element
 * (grep-verified: no `customElements.define` reachable from here).
 */
import {
  createKernel,
  loaderPlugin,
  viewportPlugin,
  fontsPlugin,
  zoomPlugin,
  cursorPlugin,
  linksPlugin,
  sheetsPlugin,
  gridPlugin,
  commentsPlugin,
  searchPlugin,
  selectionPlugin,
  toolPlugin,
  clipboardPlugin,
  runTeardown,
  type LoaderStatus,
  type FontsStatus,
} from "@parasma/headless";
import { Viewer, type DocumentInfo } from "@parasma/core";

import { attachDom } from "./attach-dom";
import { startThemeAuto } from "./theme-auto";
import { createPrintPipeline, domPrintPorts } from "./print";
import { wirePageChangeFanout } from "./page-change-fanout";
import { createRootScope } from "./root-scope";
import { createInstalledLocallyProbe } from "./local-font-probe";
import { ctaShouldShow, resolveLocalFontsEnabled } from "./local-fonts-policy";
import type {
  Theme,
  CoreViewerState,
  DevHooks,
  HeadlessViewerConfig,
  HeadlessViewerHandle,
  SnippetEvent,
} from "./types";

// No side-effect CSS import: styles are ADOPTED at create time into
// the scope's own root (document or the consumer's ShadowRoot), so shadow
// embeds work with zero consumer injection and no bundler css requirement.
import { ensureSnippetStyles } from "./style-adopt";

// The chrome-free composition surface, re-exported from this side-effect-free
// entry so `@parasma/snippet/headless` is the one blessed import for BYO-chrome.
export { attachDom } from "./attach-dom";
export { createRootScope, type RootScope } from "./root-scope";
export {
  createViewerSession,
  type ViewerSession,
  type ViewerSessionOptions,
  type ViewerSnapshot,
  type ParasmaStatus,
  type ParasmaViewState,
  type ParasmaSearchState,
  type ParasmaCommands,
} from "./viewer-session";
export type { SearchScope, ToolMode } from "@parasma/headless";
export type { PrintOptions } from "./print";
export type {
  HeadlessViewerConfig,
  HeadlessViewerHandle,
  CoreViewerState,
  DomBinding,
  AttachDomOptions,
  DevHooks,
  SnippetEvent,
  FormulaCell,
  Theme,
  ThemeOption,
  LinkActivateEvent,
} from "./types";

/** See mount.ts — a per-USER (not per-instance) font-access grant key. */
const LS_LOCAL_FONTS = "parasma.snippet.localFonts";

const NOOP_HOOKS: DevHooks = { log() {}, mark() {}, markLoadStart() {}, refresh() {} };

/** Best-effort display name from a URL's last path segment. */
function nameFromUrl(url: string): string | null {
  try {
    const seg = url.split(/[?#]/)[0].split("/").filter(Boolean).pop();
    return seg ? decodeURIComponent(seg) : null;
  } catch {
    return null;
  }
}

/**
 * Build a chrome-free interactive viewer into `config.root`. `root` is REQUIRED
 * (the embeddable path always mounts into a caller-owned slot).
 */
export async function createHeadlessViewer(
  config: HeadlessViewerConfig,
): Promise<HeadlessViewerHandle> {
  if (!config.root) throw new Error("createHeadlessViewer: `root` is required");
  const hooks = config.logger ?? NOOP_HOOKS;

  // The ONE DOM seam, built over the caller's slot.
  const scope = createRootScope(config.root);

  // Thread the (optional) asset config through to the core ViewerConfig.
  const viewer = await Viewer.create({
    assetBaseUrl: config.assetBaseUrl,
    wasmUrl: config.wasmUrl,
    fontBaseUrl: config.fontBaseUrl,
    fontResolver: config.fontResolver,
    // Default to the sniff-and-route "auto" engine (like mountViewer) so a
    // chrome-free embedded viewer gets off-threading + no-freeze by default;
    // NULLISH `??` (not `||`) preserves an explicit `worker:false` (in-process).
    // An explicit true/fn/{url} still passes through (fail-loud). Bare
    // @parasma/core Viewer.create() is unaffected (this is the snippet layer).
    worker: config.worker ?? "auto",
  });
  // DEV-ONLY: route the local-font byte-source at the dev server (tree-shaken in
  // prod). Inert until useLocalFonts is enabled.
  if (import.meta.env.DEV) {
    viewer.setLocalFontSource({
      fetchLocalFont: (family, weight, italic) =>
        fetch(`/__local-fonts/${encodeURIComponent(family)}?w=${weight}&i=${italic ? 1 : 0}`)
          .then((r) => (r.ok ? r.arrayBuffer() : null))
          .catch(() => null),
    });
  }

  const localFontsStorage = config.localFontsStorage ?? {
    get: (k: string) => {
      try {
        return localStorage.getItem(k);
      } catch {
        return null;
      }
    },
    set: (k: string, v: string) => {
      try {
        localStorage.setItem(k, v);
      } catch {
        /* ignore */
      }
    },
  };
  let localFontsEnabled = resolveLocalFontsEnabled(
    config.useLocalFonts,
    localFontsStorage.get(LS_LOCAL_FONTS),
  );

  const kernel = createKernel();
  kernel.use(loaderPlugin(viewer));
  kernel.use(viewportPlugin(viewer));
  kernel.use(fontsPlugin(viewer, { useLocalFonts: localFontsEnabled }));
  kernel.use(zoomPlugin());
  kernel.use(cursorPlugin(viewer));
  kernel.use(linksPlugin(viewer));
  kernel.use(sheetsPlugin(viewer));
  kernel.use(gridPlugin(viewer));
  kernel.use(commentsPlugin(viewer));
  kernel.use(searchPlugin(viewer, { debounceMs: config.search?.debounceMs }));
  kernel.use(toolPlugin());
  kernel.use(selectionPlugin(viewer, { contextMenu: config.contextMenu }));
  kernel.use(clipboardPlugin(viewer));
  await kernel.boot();

  if (config.styles !== "manual") ensureSnippetStyles(scope);
  const dom = attachDom(scope, { viewer, kernel }, {
    emitLinkActivate: (e) => emit("linkActivate", e),
    hooks,
    searchReveal: config.searchReveal,
  });
  config.root.append(dom.stage);


  // ── Lifecycle state machine (the teardown contract) ──────────────────
  let loaded = false;
  let destroyPromise: Promise<void> | null = null;
  let loaderStatus: LoaderStatus = "idle";
  let fontsStatus: FontsStatus = "idle";

  // ── Event emitter ───────────────────────────────────────────────────────────
  const listeners: Record<SnippetEvent, Set<(...a: unknown[]) => void>> = {
    load: new Set(),
    unload: new Set(),
    error: new Set(),
    pageChange: new Set(),
    layoutTruncated: new Set(),
    sheetChange: new Set(),
    zoomChange: new Set(),
    themeChange: new Set(),
    linkActivate: new Set(),
    cellSelect: new Set(),
    commentFocus: new Set(),
    printProgress: new Set(),
  };
  function emit(ev: SnippetEvent, ...args: unknown[]): void {
    for (const cb of listeners[ev]) {
      try {
        cb(...args);
      } catch (e) {
        console.error(`[parasma-snippet] listener for ${ev} threw:`, e);
      }
    }
  }

  const coreUnsubs: Array<() => void> = [];
  // Theme control on the consumer-owned wrapper. `root` may be a
  // ShadowRoot (attribute-less): attributes land on its HOST element then,
  // which is also where snippet.css's `[data-theme]` scoping applies.
  const themeTarget: Element = config.root instanceof Element ? config.root : config.root.host;
  const applyTheme = (t: Theme): void => {
    themeTarget.setAttribute("data-theme", t);
    emit("themeChange", t);
  };
  if (config.theme === "light" || config.theme === "dark") {
    applyTheme(config.theme);
  } else if (config.theme === "auto") {
    coreUnsubs.push(startThemeAuto(scope, applyTheme));
  }
  coreUnsubs.push(
    kernel.bus.subscribe("zoom:changed", ({ scale, gesturing }) => {
      if (!gesturing) emit("zoomChange", scale);
    }),
  );
  // Public pageChange is CORE-emitted from kernel truth (viewport:page-changed,
  // 0-based → 1-based) — it fires with NO toolbar mounted (the BYO guarantee).
  coreUnsubs.push(wirePageChangeFanout(kernel, emit));
  // Partial-layout signal, CORE-wired like pageChange (fires with no
  // chrome mounted; BYO consumers get the DOM event / handle.on both).
  coreUnsubs.push(kernel.bus.subscribe("layoutTruncated", (n) => emit("layoutTruncated", n)));
  // The two hit-notification events, promoted into the event map (one
  // subscription idiom; the element fan-out carries them to markup consumers).
  coreUnsubs.push(dom.onCellSelect((cell) => emit("cellSelect", cell)));
  coreUnsubs.push(dom.onCommentFocus((id) => emit("commentFocus", id)));
  // Print pipeline (print.ts owns the memory model; this wires the ports).
  const printer = createPrintPipeline({
    ...domPrintPorts(scope.win, scope.doc),
    pageCount: () => dom.pageCount(),
    pageSize: (idx) => viewer.pageSize(idx),
    renderPage: (idx, canvas) => viewer.renderPage(idx, canvas),
    onProgress: (done, total) => emit("printProgress", { done, total }),
  });
  /** Fonts still arriving would print fallback glyphs — wait (bounded). */
  const fontsSettled = (): Promise<void> =>
    new Promise((res) => {
      if (fontsStatus !== "planning" && fontsStatus !== "loading") return res();
      const un = kernel.bus.subscribe("fonts:status-changed", (st) => {
        if (st !== "planning" && st !== "loading") {
          un();
          res();
        }
      });
      scope.win.setTimeout(() => {
        un();
        res();
      }, 10_000);
    });
  coreUnsubs.push(
    kernel.bus.subscribe("loader:status-changed", (s) => {
      loaderStatus = s as LoaderStatus;
    }),
  );
  coreUnsubs.push(
    kernel.bus.subscribe("loader:error", (err) => {
      emit("error", err);
    }),
  );
  coreUnsubs.push(
    kernel.bus.subscribe("fonts:status-changed", (s) => {
      fontsStatus = s as FontsStatus;
      if (s === "ready") void evaluateLocalFontsCta();
    }),
  );
  coreUnsubs.push(kernel.bus.subscribe("fonts:refreshed", () => void evaluateLocalFontsCta()));
  coreUnsubs.push(viewer.on("fonts:permission-changed", () => void evaluateLocalFontsCta()));

  // ── One-click local fonts ───────────────────────────────────────────────────
  const probeInstalled = createInstalledLocallyProbe();
  const localFontsExplicit = config.useLocalFonts !== undefined;
  let ctaEvalToken = 0;
  async function evaluateLocalFontsCta(): Promise<void> {
    const token = ++ctaEvalToken;
    let show = false;
    try {
      show = ctaShouldShow({
        enabled: localFontsEnabled,
        explicit: localFontsExplicit,
        permission: await viewer.fontPermissionState(),
        families: viewer.fontProvenance().families,
        probeInstalled,
      });
    } catch {
      show = false;
    }
    if (token !== ctaEvalToken) return;
    dom.localFontsCta.hidden = !show;
  }
  const onCtaClick = () => {
    dom.localFontsCta.hidden = true;
    const warmed = viewer.warmLocalFontCatalog();
    hooks.log("FONT", "one-click local fonts: enumerating…", "info");
    void warmed
      .then((ok) => {
        if (!ok) {
          hooks.log("FONT", "local-font catalog unavailable (prompt dismissed/denied?)", "err");
          void evaluateLocalFontsCta();
          return undefined;
        }
        viewer.setLocalFontsEnabled(true);
        localFontsEnabled = true;
        localFontsStorage.set(LS_LOCAL_FONTS, "1");
        hooks.log("FONT", "local-font catalog ready; refreshFonts()", "ok");
        return viewer.refreshFonts();
      })
      .then((r) => {
        if (r) hooks.log("FONT", `one-click refresh: ${r.changed ? "applied" : "no change"}`, "ok");
      })
      .catch((e) => {
        hooks.log("FONT", `one-click local fonts failed: ${String((e as Error)?.message ?? e)}`, "err");
        void evaluateLocalFontsCta();
      });
  };
  dom.localFontsCta.addEventListener("click", onCtaClick);

  // ── Load path ───────────────────────────────────────────────────────────────
  function onLoaded(info: DocumentInfo, name: string | null): void {
    dom.setLoaded(info, name);
    if (info.documentKind === "ppt") {
      const token = dom.loadToken();
      requestAnimationFrame(() => {
        if (destroyPromise || token !== dom.loadToken() || dom.getKind() !== "ppt") return;
        const scale = dom.computeFit("page");
        void kernel.exec("zoom.setGesturing", false);
        void kernel.exec("zoom.fit", { scale, mode: "page" });
      });
    } else {
      void kernel.exec("zoom.setGesturing", false);
      void kernel.exec("zoom.set", 1);
    }
    loaded = true;
    emit("load", info);
    hooks.refresh();
  }

  async function load(bytes: ArrayBuffer | Uint8Array, name?: string): Promise<void> {
    hooks.markLoadStart();
    hooks.log("FETCH", name ? `load(${name})` : "load()", "info");
    const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    const info = (await kernel.exec("loader.open", u8))!;
    onLoaded(info, name ?? null);
  }

  async function loadUrl(url: string): Promise<void> {
    const name = nameFromUrl(url);
    hooks.markLoadStart();
    hooks.log("FETCH", url, "info");
    const info = (await kernel.exec("loader.openUrl", url))!;
    onLoaded(info, name);
  }

  const state: CoreViewerState = {
    get documentKind() {
      return dom.getKind();
    },
    get pageCount() {
      return dom.pageCount();
    },
    get currentPage() {
      return dom.currentPage();
    },
    get zoom() {
      return dom.getScale();
    },
    get loaderStatus() {
      return loaderStatus;
    },
    get fontsStatus() {
      return fontsStatus;
    },
  } as CoreViewerState;

  const handle: HeadlessViewerHandle = {
    load: (bytes, name) => load(bytes, name),
    loadUrl: (url) => loadUrl(url),
    async unload() {
      if (destroyPromise || !loaded) return;
      loaded = false;
      dom.bumpLoadToken();
      dom.resetForLoad();
      dom.clearForLoad();
      viewer.unload();
      emit("unload");
    },
    destroy() {
      if (destroyPromise) return destroyPromise;
      loaded = false;
      destroyPromise = runTeardown([
        () => {
          printer.cancel();
          dom.localFontsCta.removeEventListener("click", onCtaClick);
          for (const u of coreUnsubs.splice(0)) u();
          for (const k of Object.keys(listeners) as SnippetEvent[]) listeners[k].clear();
        },
        () => dom.destroy(),
        () => kernel.destroy(),
        () => {
          viewer.destroy();
          dom.stage.remove();
        },
      ]);
      return destroyPromise;
    },
    zoom: {
      set: (n) => {
        void kernel.exec("zoom.setGesturing", false);
        void kernel.exec("zoom.set", n);
      },
      in: () => void kernel.exec("zoom.in"),
      out: () => void kernel.exec("zoom.out"),
      fit: (mode) => {
        const scale = dom.computeFit(mode);
        void kernel.exec("zoom.fit", { scale, mode });
      },
    },
    // Kernel-truth navigation: the viewport plugin clamps + updates
    // the model (emitting pageChange on change) and requests the scroll; the
    // sensor's scroll-to-page sub performs the scrollIntoView.
    // BEHAVIOR CHANGE vs the old direct scrollIntoView: out-of-range n used to
    // be a silent no-op (querySelector miss); it now CLAMPS to [1, pageCount],
    // scrolls to the nearest bound, and fires pageChange.
    goToPage: (n) => {
      void kernel.exec("viewport.goToPage", n - 1);
    },
    selectSheet: (i) => void kernel.exec("sheets.select", i),
    // Reveal verbs — thin delegations (see the interface doc).
    scrollToGridContent: (x, y) => dom.adapters.grid.scrollToContent(x, y),
    scrollToAnchorPos: (page, y, x) => dom.scrollToAnchorPos(page, y, x),
    async print(opts) {
      if (!loaded) throw new Error("print: no document loaded");
      if (state.documentKind === "excel")
        throw new Error("print: spreadsheets are not supported yet");
      await fontsSettled();
      return printer.print(opts);
    },
    state,
    on(event, cb) {
      // Contravariance bridge at the shared `Set<(...a:unknown[])=>void>` storage
      // boundary: the generic `cb:(...args:SnippetEventMap[E])=>void` is not
      // assignable to the looser stored fn type, so cast ONCE. Zero runtime
      // effect; the shared `fn` also keeps add/delete referencing the same value.
      const fn = cb as (...a: unknown[]) => void;
      listeners[event].add(fn);
      return () => listeners[event].delete(fn);
    },
    // @advanced composition fan-in: a chrome layer built on this handle emits its
    // own snippet events (themeChange/sheetChange) into the same listener
    // registry so `handle.on(...)` stays the single subscription point.
    // pageChange is CORE-emitted from kernel truth (wirePageChangeFanout above);
    // chrome layers no longer fan it in.
    emit,
    kernel,
    viewer,
    scope,
    dom,
  };

  return handle;
}
