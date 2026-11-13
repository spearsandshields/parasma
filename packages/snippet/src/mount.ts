/// <reference types="vite/client" />
/**
 * mountViewer — the batteries-included default viewer. REBUILT ON the chrome-free
 * composition seam: it constructs the `.parasma-snippet` wrapper,
 * calls {@link createHeadlessViewer} (the DRY core: Viewer + kernel + 13 plugins +
 * the 6 DOM adapters + the interaction wiring + the load lifecycle), then LAYERS
 * the default chrome (toolbar / formula bar / sheet tabs / comments panel / search
 * box / explorer / dev shell) on the returned `dom` binding. It is NOT a parallel
 * copy of the core — the demo renders + behaves byte-identical because the same
 * core code runs; only the chrome is composed here.
 *
 * The chrome subscribes to the SAME replayable bus as the core (core subs
 * registered first inside createHeadlessViewer, chrome subs registered here
 * second); every split handler reads its datum from the event PAYLOAD, so replay +
 * live dispatch stay order-independent.
 */
import { type Kernel } from "@parasma/headless";
import { type DocumentKind } from "@parasma/core";
import { createHeadlessViewer } from "./headless-viewer";
import { installDebugFonts } from "./debug-fonts";
import { installCrispDiff } from "./crisp-diff";

import { createToolbar } from "./chrome/toolbar";
import { createFormulaBar } from "./chrome/formula-bar";
import { createSheetTabs } from "./chrome/sheet-tabs";
import { createCommentsPanel } from "./chrome/comments-panel";
import { createSearchBox } from "./chrome/search-box";
import { createExplorer, type ExplorerHandle } from "./chrome/explorer";
import { icon } from "./chrome/icons";
import { installSlots } from "./slots";
import { createDevShell, type DevShellHandle } from "./dev/dev-shell";
import { runTeardown } from "@parasma/headless";
import { el } from "./dom";
import type { MountOptions, Theme, ViewerHandle, ViewerState, DevHooks } from "./types";

import "@fontsource-variable/hanken-grotesk/index.css";
import "@fontsource-variable/jetbrains-mono/index.css";
import { startThemeAuto } from "./theme-auto";

const FMT_LABEL: Record<DocumentKind, string> = {
  word: "DOCX",
  excel: "XLSX",
  ppt: "PPTX",
};

function resolveTheme(opt: MountOptions["theme"]): Theme {
  if (opt === "light" || opt === "dark") return opt;
  // BOOTSTRAP EXCEPTION (root scoping): a pre-root prefers-color-scheme read runs
  // before the RootScope exists (theme sizes the shell). Standalone-only.
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

export async function mountViewer(
  target: HTMLElement | string,
  options: MountOptions = {},
): Promise<ViewerHandle> {
  // BOOTSTRAP EXCEPTION (root scoping): a STRING target resolves against the page
  // document — standalone quickstart convenience; an embedded caller passes an
  // Element (or drives createHeadlessViewer directly).
  const host = typeof target === "string" ? document.querySelector<HTMLElement>(target) : target;
  if (!host) throw new Error(`mountViewer: target not found: ${String(target)}`);

  const controls = options.controls !== false;
  const dev = options.dev === true;
  let theme = resolveTheme(options.theme);

  // DEV-ONLY font redirect must patch fetch BEFORE the core loads any font.
  if (dev) await installDebugFonts();
  // DEV-ONLY crispness diagnostic (no-op without ?crispdiff=<fixture>).
  if (dev) await installCrispDiff();

  // The chrome wrapper — the RootScope + the interactive stage mount INTO it.
  const root = el("div", { class: "parasma-snippet is-revealing" });
  root.setAttribute("data-theme", theme);
  root.setAttribute("data-controls", controls ? "1" : "0");
  root.setAttribute("data-dev", dev ? "1" : "0");
  host.append(root);

  // Late-bound dev-instrument hook: createHeadlessViewer routes its timing/log
  // calls through this. The kernel boot INSIDE createHeadlessViewer (below) emits
  // the initial loader/fonts status BEFORE devShell exists, so hooks fired during
  // that window are BUFFERED and flushed in order once the shell is built — no
  // throw, no lost boot logs.
  let devShell: DevShellHandle | undefined;
  const bootHookQueue: Array<(d: DevShellHandle) => void> = [];
  const runHook = (fn: (d: DevShellHandle) => void): void => {
    if (devShell) fn(devShell);
    else bootHookQueue.push(fn);
  };
  const hooks: DevHooks = {
    log: (c, m, l) => runHook((d) => d.log(c, m, l)),
    mark: (p) => runHook((d) => d.mark(p)),
    markLoadStart: () => runHook((d) => d.markLoadStart()),
    refresh: () => runHook((d) => d.refresh()),
  };

  // ── The chrome-free core (the seam) ─────────────────────────────────────────
  const core = await createHeadlessViewer({
    root,
    assetBaseUrl: options.assetBaseUrl,
    wasmUrl: options.wasmUrl,
    fontBaseUrl: options.fontBaseUrl,
    fontResolver: options.fontResolver,
    // SCOPED DEFAULT: the batteries-included layer's job
    // is to keep a large docx off the main thread WITHOUT a user opt-in. Default
    // to "auto" (every format off-thread, silent in-process fallback).
    // NULLISH `??` (not `||`) so an explicit `worker:false` is PRESERVED (false is
    // not nullish). createHeadlessViewer defaults the same way.
    worker: options.worker ?? "auto",
    useLocalFonts: options.useLocalFonts,
    localFontsStorage: options.localFontsStorage,
    contextMenu: options.contextMenu,
    search: options.search,
    logger: hooks,
  });
  const { kernel, viewer, scope, dom } = core;
  const stage = dom.stage;

  // DEV-ONLY debug hook: expose the wasm Viewer for debugLayout().
  if (dev) (globalThis as { __pv?: unknown }).__pv = viewer;

  devShell = createDevShell({
    kernel,
    viewer,
    isDev: () => root.getAttribute("data-dev") === "1",
  });
  // Flush the hook calls that fired during createHeadlessViewer's boot window.
  for (const fn of bootHookQueue) fn(devShell);
  bootHookQueue.length = 0;

  // ── Comments panel + search box ─────────────────────────────────────────────
  const commentsPanel = createCommentsPanel({
    kernel,
    scrollToAnchor: (page, y) => dom.scrollToAnchorPos(page, y),
  });
  const searchBox = createSearchBox({
    kernel,
  });

  // ── Toolbar ─────────────────────────────────────────────────────────────────
  const formulaBar = createFormulaBar();
  const sheetTabs = createSheetTabs({
    kernel,
    onSheetChange: (i) => core.emit("sheetChange", i),
  });

  // The toolbar is a PURE current-page consumer: it subscribes the
  // kernel bus for display and execs viewport.goToPage for nav. pageChange is
  // core-emitted from kernel truth inside createHeadlessViewer — no fan-in here.
  const toolbar = createToolbar({
    kernel,
    scope,
    computeFit: dom.computeFit,
    pageNav: options.toolbarPageNav,
    onToggleComments: () => commentsPanel.toggle(),
    onToggleTheme: () => {
      stopThemeAuto?.();
      stopThemeAuto = null;
      setTheme(theme === "dark" ? "light" : "dark");
    },
    onToggleFullscreen: () => toggleFullscreen(),
    onPrint: () =>
      void core.print().catch((e) => hooks.log("PRINT", String(e), "err")),
  });


  // Adopt the core's detached tool-toggle buttons into the toolbar tool-seg.
  const toolSeg = el(
    "div",
    { class: "ps-seg ps-tool-seg", role: "group", "aria-label": "Tool" },
    [dom.toolButtons.select, dom.toolButtons.hand],
  );
  toolbar.root.insertBefore(toolSeg, toolbar.root.querySelector(".ps-spacer"));

  commentsPanel.onCountChange((n) => toolbar.setCommentCount(n));
  commentsPanel.onOpenChange((open) => toolbar.setCommentsOpen(open));

  // ── Explorer panel (OPT-IN) ─────────────────────────────────────────────────
  let explorer: ExplorerHandle | null = null;
  let panelToggleBtn: HTMLButtonElement | null = null;
  if (options.explorer) {
    explorer = createExplorer({
      config: options.explorer,
      onSelect: (id) => void openEntry(id),
    });
    root.setAttribute("data-explorer", "1");
    stage.prepend(explorer.root);

    panelToggleBtn = el("button", {
      class: "ps-btn ps-btn--ps-icon ps-panel-toggle",
      "aria-label": "toggle file explorer",
      title: "Toggle explorer (⌘B)",
    }, [icon("panel-left")]) as HTMLButtonElement;
    panelToggleBtn.addEventListener("click", () => explorer?.toggle());
    toolbar.root.insertBefore(panelToggleBtn, toolbar.root.firstChild);

    explorer.onCollapsedChange((collapsed) => {
      root.toggleAttribute("data-explorer-collapsed", collapsed);
      panelToggleBtn?.setAttribute("aria-pressed", collapsed ? "false" : "true");
    });
    panelToggleBtn.setAttribute("aria-pressed", "true");
  }

  // Assemble the chrome AROUND the core's already-appended stage. The two
  // SPREADSHEET rows fuse INTO the workspace column (formula row above the
  // views, sheet tabs below) so they read as part of the sheet workspace —
  // not app-wide strips sandwiching the stage (the old layout).
  root.insertBefore(toolbar.root, stage);
  dom.workspace.prepend(formulaBar.root);
  dom.workspace.append(sheetTabs.root);
  root.append(devShell.root);
  stage.append(commentsPanel.root, searchBox.root);

  // ── Chrome bus subscriptions (the CHROME half — core subs already ran) ───────
  const chromeUnsubs: Array<() => void> = [];
  // Memoized teardown flag; lifecycle rAFs/timers bail on it.
  let mountDestroyPromise: Promise<void> | null = null;

  // The truncation stamp is the status slot's RESTING text once set:
  // every writer that "returns to ready" must consult it, or a passing flash
  // (clipboard copy) silently erases "showing first N pages". Declared above
  // flashStatus because flashStatus's 1.2s revert is exactly such a writer —
  // the first version of this wiring missed it.
  let truncatedAt: number | null = null;
  const restingStatus = () => (truncatedAt === null ? "ready" : `showing first ${truncatedAt} pages`);

  // Clipboard status flash.
  let clipboardStatusTimer = 0;
  const flashStatus = (text: string) => {
    toolbar.setStatus("ready", text);
    if (clipboardStatusTimer) scope.win.clearTimeout(clipboardStatusTimer);
    clipboardStatusTimer = scope.win.setTimeout(() => toolbar.setStatus("ready", restingStatus()), 1200);
  };
  chromeUnsubs.push(
    kernel.bus.subscribe("clipboard:copied", ({ charCount }) => flashStatus(`Copied ${charCount} chars`)),
  );
  chromeUnsubs.push(kernel.bus.subscribe("clipboard:error", () => flashStatus("Copy failed")));

  // Zoom pill (reads scale from the payload → order-independent with the core sub).
  chromeUnsubs.push(kernel.bus.subscribe("zoom:changed", ({ scale }) => toolbar.setZoom(scale)));

  // Loader/fonts status → toolbar tokens + dev marks (dev marks are core hooks).
  // The truncation writers: the event arm and the loader arm race (bus
  // replay order is not contractual), so BOTH order outcomes must render the
  // truncation. Cleared on the next load's "loading". Wording/placement is
  // PROVISIONAL; the load-bearing surface is the `layoutTruncated` event,
  // not this status text.
  chromeUnsubs.push(
    kernel.bus.subscribe("layoutTruncated", (n) => {
      truncatedAt = n;
      toolbar.setStatus("ready", restingStatus());
    }),
  );
  chromeUnsubs.push(
    kernel.bus.subscribe("loader:status-changed", (s) => {
      if (s === "loading" || s === "planning") {
        truncatedAt = null;
        toolbar.setStatus("loading", "loading");
      } else if (s === "ready") toolbar.setStatus("ready", restingStatus());
      else if (s === "error") toolbar.setStatus("error", "load error");
    }),
  );
  chromeUnsubs.push(kernel.bus.subscribe("loader:error", () => toolbar.setStatus("error", "load error")));

  let fontsLoaded = 0;
  let fontsStatus: string = "idle";
  chromeUnsubs.push(
    kernel.bus.subscribe("fonts:status-changed", (s) => {
      fontsStatus = s as string;
      if (s === "planning" || s === "loading") toolbar.setFontStatus(true, fontsLoaded || null);
      else {
        fontsLoaded = 0;
        toolbar.setFontStatus(false);
      }
    }),
  );
  chromeUnsubs.push(
    kernel.bus.subscribe("fonts:variant-arrived", ({ loadedVariants }) => {
      fontsLoaded = loadedVariants;
      if (fontsStatus === "planning" || fontsStatus === "loading") toolbar.setFontStatus(true, fontsLoaded);
    }),
  );
  chromeUnsubs.push(
    kernel.bus.subscribe("fonts:ready", () => {
      fontsLoaded = 0;
      toolbar.setFontStatus(false);
    }),
  );

  chromeUnsubs.push(kernel.bus.subscribe("loadComplete", (total) => toolbar.setPageCount(total as number)));

  // rAF-coalesced page-count writes (a 20k-page sheet streams faster than DOM writes).
  let pendingPageCount = 0;
  let pageCountRaf = false;
  chromeUnsubs.push(
    kernel.bus.subscribe("pageReady", (idx) => {
      pendingPageCount = Math.max(pendingPageCount, (idx as number) + 1);
      if (!pageCountRaf) {
        pageCountRaf = true;
        requestAnimationFrame(() => {
          pageCountRaf = false;
          if (mountDestroyPromise) return;
          toolbar.setPageCount(pendingPageCount);
        });
      }
    }),
  );

  chromeUnsubs.push(
    kernel.bus.subscribe("grid:mode-changed", ({ mode }) => toolbar.setGridMode(mode === "grid")),
  );

  // ── Chrome cell/comment feeds (fan-out from the core hit-test) ───────────────
  chromeUnsubs.push(dom.onCellSelect((cell) => formulaBar.show(cell)));
  chromeUnsubs.push(dom.onCommentFocus((id) => commentsPanel.focusCard(id)));

  // ── Chrome doc-switch clearing (loads + warm unload) ─────────────────────────
  function clearChromeForLoad(): void {
    formulaBar.clear();
    // Hide the row until the NEXT load declares its kind (a doc-switch to a
    // spreadsheet re-activates it in the "load" handler; a warm unload or a
    // switch to word/ppt leaves it hidden).
    formulaBar.setActive(false);
    commentsPanel.close();
    searchBox.close();
    toolbar.resetForLoad();
    pendingPageCount = 0;
  }
  chromeUnsubs.push(kernel.bus.subscribe("loader:load-started", clearChromeForLoad));
  chromeUnsubs.push(core.on("unload", () => clearChromeForLoad()));
  chromeUnsubs.push(
    core.on("printProgress", (p) => toolbar.setPrintProgress(p.done >= p.total ? null : p)),
  );

  // ── Chrome per-document setup (label + scope) ────────────────────────────────
  chromeUnsubs.push(
    core.on("load", (info) => {
      const documentKind = (info as { documentKind: DocumentKind }).documentKind;
      searchBox.setScopeVisible(documentKind === "excel");
      // Workspace fusion: the formula row is a stable part of the spreadsheet
      // workspace (kind-owned visibility — no cell-click pop-in reflow).
      formulaBar.setActive(documentKind === "excel");
      const name = dom.currentName();
      const fmtLabel =
        documentKind === "excel" && name?.toLowerCase().endsWith(".csv")
          ? "CSV"
          : FMT_LABEL[documentKind];
      toolbar.setDocument(documentKind, dom.pageCount(), fmtLabel, name ?? undefined);
    }),
  );

  if (dev) devShell.attachGlobals();

  // ── Theme / fullscreen ───────────────────────────────────────────────────────
  function setTheme(t: Theme): void {
    theme = t;
    root.setAttribute("data-theme", t);
    toolbar.setTheme(t);
    core.emit("themeChange", t);
  }
  setTheme(theme);
  // `theme: "auto"` is LIVE: mirror the host's data-theme (shadow
  // host / documentElement) + the system preference until a manual toggle
  // takes over (an explicit user choice beats the mirror — pressing the theme
  // button under "auto" pins the theme and stops tracking).
  let stopThemeAuto: (() => void) | null = null;
  if (options.theme === "auto" || options.theme === undefined) {
    // undefined historically resolved as system-once; it now tracks live too —
    // same initial value, strictly better behavior on OS/app theme flips.
    stopThemeAuto = startThemeAuto(scope, setTheme);
  }

  function toggleFullscreen(): void {
    if (scope.doc.fullscreenElement) void scope.doc.exitFullscreen();
    else void root.requestFullscreen?.();
  }

  // ── Dev runtime toggle (⌥⇧D) + chrome keyboard ──────────────────────────────
  let devOn = dev;
  function setDev(on: boolean): void {
    devOn = on;
    root.setAttribute("data-dev", on ? "1" : "0");
    if (on) devShell?.attachGlobals();
    else devShell?.detachGlobals();
  }
  const explorerCollapsible = options.explorer?.collapsible !== false;
  const onKey = (e: KeyboardEvent) => {
    if (e.altKey && e.shiftKey && (e.key === "D" || e.key === "d" || e.code === "KeyD")) {
      e.preventDefault();
      setDev(!devOn);
    }
    if (explorer && explorerCollapsible && (e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && (e.key === "b" || e.key === "B")) {
      e.preventDefault();
      explorer.toggle();
    }
    if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && (e.key === "f" || e.key === "F")) {
      e.preventDefault();
      searchBox.open();
    } else if (e.key === "Escape" && searchBox.isOpen) {
      searchBox.close();
    }
  };
  scope.win.addEventListener("keydown", onKey);

  // Resolve an explorer entry through the host's open() — string ⇒ URL, else bytes.
  async function openEntry(id: string): Promise<void> {
    if (!options.explorer) return;
    try {
      const resolved = await options.explorer.open(id);
      if (typeof resolved === "string") await core.loadUrl(resolved);
      else await core.load(resolved);
      explorer?.markActive(id);
    } catch (e) {
      if (e instanceof DOMException && e.name === "AbortError") return;
      devShell?.log("FETCH", String((e as Error)?.message ?? e), "err");
      core.emit("error", e);
    }
  }

  // ── Reveal → settle ──────────────────────────────────────────────────────────
  let revealRaf = 0;
  let revealTimer = 0;
  revealRaf = requestAnimationFrame(() => {
    revealRaf = 0;
    if (mountDestroyPromise) return;
    revealTimer = scope.win.setTimeout(() => {
      revealTimer = 0;
      root.classList.remove("is-revealing");
    }, 600);
  });

  // ── Auto-load src ──────────────────────────────────────────────────────────────
  if (options.src) {
    // INVARIANT: stays fire-and-forget. <parasma-viewer>'s CustomEvent fan-out
    // subscribes after mountViewer resolves, so awaiting the src load here
    // would silently drop the initial parasma-load (pinned by element.test.ts
    // "(16) initial-load ordering").
    void core.loadUrl(options.src).catch((e) => {
      devShell?.log("FETCH", String((e as Error)?.message ?? e), "err");
      core.emit("error", e);
    });
  }

  // ── Handle ─────────────────────────────────────────────────────────────────────
  const state: ViewerState = {
    get documentKind() {
      return core.state.documentKind;
    },
    get pageCount() {
      return core.state.pageCount;
    },
    get currentPage() {
      // Kernel truth: the same single-tracker datum the toolbar shows.
      return core.state.currentPage;
    },
    get zoom() {
      return core.state.zoom;
    },
    get theme() {
      return theme;
    },
    get dev() {
      return devOn;
    },
    get loaderStatus() {
      return core.state.loaderStatus;
    },
    get fontsStatus() {
      return core.state.fontsStatus;
    },
  } as ViewerState;

  const handle: ViewerHandle = {
    ...core,
    // Full teardown: chrome widgets THEN the core (adapters → kernel awaited →
    // engine → stage) THEN the wrapper root. Idempotent + Promise-returning.
    destroy() {
      if (mountDestroyPromise) return mountDestroyPromise;
      mountDestroyPromise = runTeardown([
        () => {
          stopThemeAuto?.();
          scope.win.removeEventListener("keydown", onKey);
          if (revealRaf) cancelAnimationFrame(revealRaf);
          if (revealTimer) scope.win.clearTimeout(revealTimer);
          if (clipboardStatusTimer) scope.win.clearTimeout(clipboardStatusTimer);
          for (const u of chromeUnsubs.splice(0)) u();
        },
        () => toolbar.destroy(),
        () => sheetTabs.destroy(),
        () => commentsPanel.destroy(),
        () => searchBox.destroy(),
        () => explorer?.destroy(),
        () => devShell.destroy(),
        // The core: detach adapters + core subs, AWAIT kernel (plugins settle),
        // free the engine, remove the stage.
        () => core.destroy(),
        () => root.remove(),
      ]);
      return mountDestroyPromise;
    },
    setTheme: (t) => setTheme(t),
    setDev: (on) => setDev(on),
    setControls: (on: boolean) => root.setAttribute("data-controls", on ? "1" : "0"),
    state,
    explorer: explorer
      ? {
          setEntries: (e) => explorer!.setEntries(e),
          setActive: (id) => explorer!.markActive(id),
          isOpen: () => explorer!.isOpen(),
          toggle: (open) => explorer!.toggle(open),
        }
      : undefined,
  };

  // Tier-B slots — installed LAST so factories receive the fully-built handle
  // (a floating nav closes over goToPage/on("pageChange") in one phase). Slot
  // nodes live inside `root`, so destroy()'s root.remove() detaches them.
  installSlots({ slots: options.slots, handle, toolbarRoot: toolbar.root, stage });

  return handle;
}

// Re-export the Kernel type only for convenience in callers needing it.
export type { Kernel };
