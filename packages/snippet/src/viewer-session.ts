/**
 * createViewerSession — the framework-neutral state layer the framework
 * adapters (@parasma/vue, @parasma/react) bind to.
 *
 * A session turns one headless handle's events into an immutable snapshot
 * (`getSnapshot` / `subscribe`, the external-store shape both Vue refs and
 * React's useSyncExternalStore consume), and owns the load status machine,
 * the search counter, the commands and the view-state round trip. Writing
 * these rules once is the point: each adapter is then only a binding, and a
 * new adapter cannot drift from the others.
 *
 * The session does NOT create the handle. The adapter boots one with
 * `createHeadlessViewer` and hands it over with `connect`; `disconnect`
 * unwires it before the adapter destroys it. The session survives a
 * disconnect, so a host that mounts twice (React StrictMode) reuses it.
 *
 * Snapshot identity: a new object only when a field actually changed, and
 * unchanged fields keep their identity. Engine objects are held, never
 * copied or proxied.
 */
import { sniffKind, UnsupportedFormatError, type DocumentKind } from "@parasma/core";
import type { SearchScope, ToolMode } from "@parasma/headless";
import type { PrintOptions } from "./print";
import type { FormulaCell, HeadlessViewerHandle } from "./types";

/** The machine-grade load status. Transitions (documented contract):
 *  idle → loading (load() called) → ready (load resolved; pages may still
 *  stream) | error (load rejected) ; ready|error → loading (next load — a
 *  SUPERSEDED in-flight load never fires its tail: no stale ready, no stale
 *  error) ; any → idle (unload()). */
export type ParasmaStatus = "idle" | "loading" | "ready" | "error";

export interface ParasmaViewState {
  page: number;
  zoom: number;
  sheet: number;
  /** Spreadsheets only: the Grid/Pages choice; absent for other kinds. */
  gridMode?: "grid" | "pages";
  theme?: "light" | "dark";
}

/** The counter shape (the "3 / 17" search box): `activeIdx` is 0-based and
 *  null while nothing is active. Full match geometry (rects, cell refs) stays
 *  on the bus — subscribe to `search:changed` on `handle.kernel.bus` for it. */
export interface ParasmaSearchState {
  query: string;
  total: number;
  activeIdx: number | null;
  status: "idle" | "searching" | "done";
  /** 'sheet' = active sheet (Excel's Within:Sheet — the default);
   *  'workbook' = every sheet. Always 'sheet' for non-spreadsheets. */
  scope: SearchScope;
}

export interface ParasmaCommands {
  zoomIn(): void;
  zoomOut(): void;
  zoomTo(scale: number): void;
  fitPage(): void;
  fitWidth(): void;
  goToPage(n: number): void;
  selectSheet(i: number): void;
  /** Debounced full-text search; `""` clears query and highlights. */
  search(text: string): void;
  searchNext(): void;
  searchPrev(): void;
  /** Spreadsheets: widen to 'workbook' or narrow back to 'sheet' (the
   *  default). Restarts the current query against the new scope. */
  setSearchScope(scope: SearchScope): void;
  /** Reveal verbs — consumer-initiated scrolls (your comments panel,
   *  your hit list, your deep links) the engine resolves to positions.
   *  Same names as the handle: no translation layer to memorize. */
  scrollToGridContent(contentX: number, contentY: number): void;
  scrollToAnchorPos(page: number, y: number, x?: number): void;
  /** The select↔hand tool pair (text selection vs drag-to-pan; space-hold
   *  pan works in either mode). */
  setTool(mode: ToolMode): void;
  toggleTool(): void;
  /** Print (word/pptx; spreadsheets reject). Returns the run's promise so a
   *  toolbar can disable its button / surface rejection; progress streams on
   *  `printProgress`. A second call supersedes the run in flight. */
  print(opts?: PrintOptions): Promise<void>;
  /** Fullscreen a custom target (e.g. a wrapper INCLUDING your chrome rows);
   *  defaults to the mount root. The reactive `isFullscreen` is deliberately
   *  not provided — pair with your framework's own fullscreen composable
   *  (any fullscreen composable) if you need the state. */
  toggleFullscreen(target?: HTMLElement): void;
}

/** Everything a generic BYO toolbar renders, as one immutable value. */
export interface ViewerSnapshot {
  readonly status: ParasmaStatus;
  /** The load()/unload() rejection, if status === "error". */
  readonly error: unknown;
  readonly documentKind: DocumentKind | null;
  /**
   * The REAL page dimensions (CSS px), available the moment `load()` resolves
   * — i.e. right after PARSE, long before layout finishes. This is the number
   * a loading skeleton should use: the only window where dimensions are
   * genuinely unknown is the parse itself (tens of ms — an indeterminate
   * spinner's job).
   */
  readonly pageSize: { widthPx: number; heightPx: number } | null;
  /** Streaming-aware: grows with pageReady, settles on loadComplete. */
  readonly pageCount: number;
  /** 1-based. */
  readonly currentPage: number;
  /** Pages kept when layout stopped at the memory guardrail; null = complete. */
  readonly truncatedAt: number | null;
  readonly zoom: number;
  readonly theme: "light" | "dark" | null;
  readonly sheets: ReadonlyArray<{ name: string; pageCount: number }>;
  readonly activeSheet: number;
  /** Spreadsheets only; null for other kinds. */
  readonly gridMode: "grid" | "pages" | null;
  /** The formula-bar feed; null when no cell is selected. */
  readonly cell: FormulaCell | null;
  readonly comments: ReadonlyArray<{ id: number; page: number; anchorY: number }>;
  readonly focusedCommentId: number | null;
  /** Non-null while document fonts are still arriving. */
  readonly fontsLoading: { arrived: number } | null;
  /** The EFFECTIVE tool ("select" default): base mode + space-hold override. */
  readonly tool: ToolMode;
  /** Exact print-prep progress ({done, total}); null when no run is active.
   *  Resets to null when the run completes or a new document loads. */
  readonly printProgress: { done: number; total: number } | null;
  /** Streams while a search runs (`status: "searching"` → `"done"`); reveal,
   *  cross-sheet navigation and repaint are the engine's job. */
  readonly search: ParasmaSearchState;
}

export interface ViewerSessionOptions {
  /** Applied after each successful load. Storage stays yours. */
  initialViewState?: Partial<ParasmaViewState>;
}

export interface ViewerSession {
  getSnapshot(): ViewerSnapshot;
  /** Calls `listener` after every snapshot change; returns the unsubscribe. */
  subscribe(listener: () => void): () => void;
  /** The connected handle, or null before `connect` / after `disconnect`. */
  readonly handle: HeadlessViewerHandle | null;
  /** Resolves with the next (or current) connected handle. Rejected only by
   *  `fail`. A disconnect starts a new pending promise. */
  readonly ready: Promise<HeadlessViewerHandle>;
  /** Wire a booted handle. `root` is the element it was booted into. */
  connect(handle: HeadlessViewerHandle, root: HTMLElement): void;
  /** Unwire the connected handle. The caller destroys it afterwards. */
  disconnect(): void;
  /** Report a boot failure: status becomes "error" and `ready` rejects. */
  fail(error: unknown): void;
  /** The adapter will never connect (it was torn down mid-boot): `ready`, and
   *  any load awaiting it, rejects with `reason`. The snapshot is untouched. */
  abandon(reason: unknown): void;
  load(bytes: ArrayBuffer | Uint8Array, name?: string): Promise<void>;
  unload(): Promise<void>;
  readonly commands: ParasmaCommands;
  viewState(): ParasmaViewState;
  applyViewState(s: Partial<ParasmaViewState>): void;
}

const EMPTY_SEARCH: ParasmaSearchState = { query: "", total: 0, activeIdx: null, status: "idle", scope: "sheet" };

const INITIAL: ViewerSnapshot = {
  status: "idle",
  error: null,
  documentKind: null,
  pageSize: null,
  pageCount: 0,
  currentPage: 1,
  truncatedAt: null,
  zoom: 1,
  theme: null,
  sheets: [],
  activeSheet: 0,
  gridMode: null,
  cell: null,
  comments: [],
  focusedCommentId: null,
  fontsLoading: null,
  tool: "select",
  printProgress: null,
  search: EMPTY_SEARCH,
};

export function createViewerSession(options: ViewerSessionOptions = {}): ViewerSession {
  let snap: ViewerSnapshot = INITIAL;
  const listeners = new Set<() => void>();
  let handle: HeadlessViewerHandle | null = null;
  let root: HTMLElement | null = null;
  let unsubs: Array<() => void> = [];
  /** Supersede token for load tails, so a superseded load()'s status flips
   *  never land. */
  let loadSeq = 0;

  let resolveReady!: (h: HeadlessViewerHandle) => void;
  let rejectReady!: (e: unknown) => void;
  let ready!: Promise<HeadlessViewerHandle>;
  const armReady = (): void => {
    ready = new Promise<HeadlessViewerHandle>((res, rej) => {
      resolveReady = res;
      rejectReady = rej;
    });
    // A consumer may never await `ready` (it reads the snapshot); a boot
    // failure must not surface as an unhandled rejection.
    ready.catch(() => {});
  };
  armReady();

  /** Merge `patch`; publish only when some field's identity changed. */
  function set(patch: Partial<ViewerSnapshot>): void {
    let changed = false;
    for (const k in patch) {
      const key = k as keyof ViewerSnapshot;
      if (!Object.is(snap[key], patch[key])) {
        changed = true;
        break;
      }
    }
    if (!changed) return;
    snap = { ...snap, ...patch };
    for (const l of [...listeners]) l();
  }

  function wire(h: HeadlessViewerHandle): void {
    unsubs.push(
      h.on("load", (info) => {
        // Optional-chained on the same degrade posture as the engine facade:
        // a test double emitting a bare "load" must not throw.
        set({
          pageSize: info?.pageSize ?? null,
          documentKind: h.state.documentKind,
          currentPage: h.state.currentPage,
          pageCount: h.state.pageCount,
          zoom: h.state.zoom,
        });
      }),
      h.on("pageChange", (n) => set({ currentPage: n })),
      h.on("zoomChange", (z) => set({ zoom: z })),
      h.on("themeChange", (t) => set({ theme: t })),
      h.on("layoutTruncated", (n) => set({ truncatedAt: n })),
      h.kernel.bus.subscribe("pageReady", () => set({ pageCount: h.state.pageCount })),
      h.kernel.bus.subscribe("loadComplete", (n) => set({ pageCount: n })),
      h.kernel.bus.subscribe("sheets:changed", ({ sheets, active }) =>
        set({ sheets: sheets.map((s) => ({ name: s.name, pageCount: s.pageCount })), activeSheet: active, cell: null }),
      ),
      h.kernel.bus.subscribe("tool:mode-changed", ({ mode }) => set({ tool: mode })),
      h.kernel.bus.subscribe("grid:mode-changed", ({ mode }) => set({ gridMode: mode, cell: null })),
      h.kernel.bus.subscribe("comments:changed", ({ comments, focused }) =>
        set({ comments, focusedCommentId: focused }),
      ),
      h.kernel.bus.subscribe("fonts:status-changed", (s) => {
        const active = s === "planning" || s === "loading";
        if (active && snap.fontsLoading === null) set({ fontsLoading: { arrived: 0 } });
        else if (!active) set({ fontsLoading: null });
      }),
      h.kernel.bus.subscribe("fonts:variant-arrived", ({ loadedVariants }) => {
        if (snap.fontsLoading !== null) set({ fontsLoading: { arrived: loadedVariants } });
      }),
      // Sticky ("latest") on the bus: a late connect still catches the current
      // search state. Match geometry deliberately stays behind — counters
      // need the count, not the rects — and the streaming loop emits once per
      // page TICK, so equal-value emissions are dropped here: subscribers
      // run only when a field a counter can render actually moved.
      h.kernel.bus.subscribe("search:changed", ({ query, results, activeIdx, status, scope }) => {
        const prev = snap.search;
        if (
          prev.query === query &&
          prev.total === results.length &&
          prev.activeIdx === activeIdx &&
          prev.status === status &&
          prev.scope === scope
        )
          return;
        set({ search: { query, total: results.length, activeIdx, status, scope } });
      }),
      h.on("printProgress", (p) => set({ printProgress: p.done >= p.total ? null : p })),
      // The promoted event, not the @advanced dom member — one idiom.
      h.on("cellSelect", (c) => set({ cell: c })),
    );
  }

  function connect(h: HeadlessViewerHandle, el: HTMLElement): void {
    if (handle) disconnect();
    handle = h;
    root = el;
    wire(h);
    resolveReady(h);
  }

  function disconnect(): void {
    for (const u of unsubs.splice(0)) u();
    if (!handle) return;
    handle = null;
    root = null;
    loadSeq++; // an in-flight load against the old handle must not land
    armReady();
  }

  function fail(e: unknown): void {
    set({ error: e, status: "error" });
    rejectReady(e);
  }

  function abandon(reason: unknown): void {
    rejectReady(reason);
  }

  async function load(bytes: ArrayBuffer | Uint8Array, name?: string): Promise<void> {
    const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    // Early typed rejection: bytes with NO recognizable signature never reach
    // the engine — "unsupported" and "broken" become different states.
    if (sniffKind(u8) === null) {
      const e = new UnsupportedFormatError(name);
      set({ error: e, status: "error" });
      throw e;
    }
    const h = await ready;
    const seq = ++loadSeq;
    set({ status: "loading", error: null, truncatedAt: null });
    try {
      await h.load(u8, name);
      if (seq !== loadSeq) return; // superseded or disconnected: no stale tail
      set({ status: "ready" });
      if (options.initialViewState) applyViewState(options.initialViewState);
    } catch (e) {
      if (seq !== loadSeq) return;
      set({ error: e, status: "error" });
      throw e;
    }
  }

  async function unload(): Promise<void> {
    const h = handle;
    if (!h) return;
    loadSeq++;
    await h.unload();
    set({
      status: "idle",
      documentKind: null,
      pageSize: null,
      pageCount: 0,
      truncatedAt: null,
      sheets: [],
      comments: [],
      cell: null,
      search: EMPTY_SEARCH,
      printProgress: null,
    });
  }

  const commands: ParasmaCommands = {
    zoomIn: () => handle?.zoom.in(),
    zoomOut: () => handle?.zoom.out(),
    zoomTo: (s) => handle?.zoom.set(s),
    fitPage: () => handle?.zoom.fit("page"),
    fitWidth: () => handle?.zoom.fit("width"),
    goToPage: (n) => handle?.goToPage(n),
    selectSheet: (i) => handle?.selectSheet(i),
    search: (text) => void handle?.kernel.exec("search.query", text),
    searchNext: () => void handle?.kernel.exec("search.next"),
    searchPrev: () => void handle?.kernel.exec("search.prev"),
    setSearchScope: (scope) => void handle?.kernel.exec("search.setScope", scope),
    scrollToGridContent: (x, y) => handle?.scrollToGridContent(x, y),
    scrollToAnchorPos: (page, y, x) => handle?.scrollToAnchorPos(page, y, x),
    setTool: (m) => void handle?.kernel.exec("tool.setMode", m),
    toggleTool: () => void handle?.kernel.exec("tool.toggle"),
    print: (opts) => (handle ? handle.print(opts) : Promise.reject(new Error("print: not mounted"))),
    toggleFullscreen: (target) => {
      const el = target ?? root;
      if (!el) return;
      const doc = el.ownerDocument;
      if (doc.fullscreenElement) void doc.exitFullscreen();
      else void el.requestFullscreen?.();
    },
  };

  function viewState(): ParasmaViewState {
    return {
      page: snap.currentPage,
      zoom: snap.zoom,
      sheet: snap.activeSheet,
      gridMode: snap.gridMode ?? undefined,
      theme: snap.theme ?? undefined,
    };
  }

  function applyViewState(s: Partial<ParasmaViewState>): void {
    // Mode before page: page numbers only mean anything in pages mode.
    if (s.gridMode !== undefined) void handle?.kernel.exec("grid.setMode", s.gridMode === "grid");
    if (s.sheet !== undefined) commands.selectSheet(s.sheet);
    if (s.zoom !== undefined) commands.zoomTo(s.zoom);
    if (s.page !== undefined) commands.goToPage(s.page);
    // theme intentionally not applied here: it belongs to the `theme` option
    // (an explicit value or "auto"), not to per-document view state.
  }

  return {
    getSnapshot: () => snap,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    get handle() {
      return handle;
    },
    get ready() {
      return ready;
    },
    connect,
    disconnect,
    fail,
    abandon,
    load,
    unload,
    commands,
    viewState,
    applyViewState,
  };
}
