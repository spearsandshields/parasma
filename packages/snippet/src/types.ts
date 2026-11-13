/**
 * @parasma/snippet — public types for the drop-in viewer tier.
 */
import type {
  Kernel,
  LoaderStatus,
  FontsStatus,
  LinkDecision,
  ContextMenuSeam,
} from "@parasma/headless";
import type {
  Viewer,
  DocumentInfo,
  DocumentKind,
  FontUrlResolver,
  WorkerSelector,
} from "@parasma/core";
import type { RootScope } from "./root-scope";
import type { PrintOptions } from "./print";
import type { DomViewportAdapter } from "./adapters/dom-viewport-adapter";
import type { DomGridAdapter } from "./adapters/dom-grid-adapter";
import type { DomSelectionAdapter } from "./adapters/dom-selection-adapter";
import type { DomToolAdapter } from "./adapters/dom-tool-adapter";
import type { DomClipboardAdapter } from "./adapters/dom-clipboard-adapter";
import type { DomContextMenuAdapter } from "./adapters/dom-context-menu-adapter";

export type Theme = "light" | "dark";
export type ThemeOption = Theme | "auto";

export type ExplorerFormat = "docx" | "xlsx" | "pptx" | "csv";

/** A single openable document in the explorer panel. */
export interface ExplorerEntry {
  /** Opaque id handed back to `open()`. */
  id: string;
  /** Leaf label. */
  name: string;
  /** Optional 2nd-tier sub-group within a format. */
  group?: string;
  /** Overrides the extension-derived format of `name`. */
  format?: ExplorerFormat;
}

/**
 * Opt-in file-explorer config. This is an APP-SHELL affordance, not viewer
 * chrome — absent ⇒ NO panel (clean viewer). The CONSUMER supplies the entries
 * + an `open` resolver; the snippet ships no fixtures.
 */
export interface ExplorerConfig {
  entries: ExplorerEntry[];
  /** Resolve an entry id to bytes (Uint8Array/ArrayBuffer) or a URL string. */
  open: (
    id: string,
  ) =>
    | Uint8Array
    | ArrayBuffer
    | string
    | Promise<Uint8Array | ArrayBuffer | string>;
  /** Panel title. Default 'Documents'. */
  title?: string;
  /** Default true — show the filter input. */
  filter?: boolean;
  /** Default true — show the collapse toggle + allow ⌘B/Ctrl+B. */
  collapsible?: boolean;
  /** Optional initial active leaf. */
  selectedId?: string;
}

export interface MountOptions {
  /** URL to auto-loadUrl on mount. */
  src?: string;
  /** 'auto' (default) follows prefers-color-scheme. */
  theme?: ThemeOption;
  /** Default FALSE — toggles the developer instrument. */
  dev?: boolean;
  /** Default true; false = chromeless canvas only. */
  controls?: boolean;
  /** Opt-in file-explorer panel. ABSENT/undefined ⇒ NO panel (clean viewer). */
  explorer?: ExplorerConfig;
  /**
   * Opt in to matching Word's font substitution by reading the user's INSTALLED
   * fonts (Font Access API). Best-effort; falls back to bundled URLs. Default
   * FALSE. Element attribute: `local-fonts`.
   *
   * When PROVIDED it always wins; when ABSENT the persisted one-click choice
   * (see {@link MountOptions.localFontsStorage}) is read at mount.
   */
  useLocalFonts?: boolean;
  /**
   * Storage seam for the one-click "Use installed fonts" choice.
   * Only user GESTURES are persisted (the affordance click). Default:
   * localStorage under "parasma.snippet.localFonts". Inject for hosts with
   * their own persistence (or `{ get: () => null, set: () => {} }` to disable).
   */
  localFontsStorage?: { get(key: string): string | null; set(key: string, value: string): void };
  /**
   * Decision seam for the right-click context menu: called SYNCHRONOUSLY
   * once per menu request (never on dismiss)
   * with a data-only ctx (the default items + a minimal selection summary + the
   * 0-based page/slide index) and returns the full item list to show (`[]`
   * suppresses the menu). Consumer items may carry an `onSelect` invoked inside
   * the menu click's user-activation window. A throw or invalid return falls
   * back to the default items with one console.warn — the menu never fails to
   * open. No custom-element attribute (functions cannot ride attributes) —
   * JS property/config only. (Link activation deliberately goes the OTHER
   * way: it is an EVENT, not a function option, precisely so a plain-HTML
   * `<parasma-viewer>` consumer can intercept it with `addEventListener` +
   * `preventDefault()`. A function-valued option is unreachable from markup.)
   */
  contextMenu?: ContextMenuSeam;
  /**
   * Tier-B slots — BOUNDED injection points into the DEFAULT chrome (exactly
   * two, demand-driven; deeper restructuring belongs to the headless tier where
   * the consumer owns all chrome). Each accepts a plain element or a factory
   * called with the LIVE {@link ViewerHandle} (close over `goToPage` /
   * `on("pageChange")` / `state` in one phase). A throwing factory or
   * non-element return is contained (one console.warn, slot skipped) — mounting
   * never fails because of a bad slot. Slot nodes detach with the viewer on
   * destroy(); they are consumer-owned otherwise. JS property/config only (no
   * custom-element attribute), same as {@link MountOptions.contextMenu}.
   */
  slots?: {
    /** Extra toolbar content — its own group, inserted before the status token.
     *  Hidden with the rest of the toolbar under `controls:false`. */
    toolbarTrailing?: HTMLElement | ((handle: ViewerHandle) => HTMLElement);
    /** A full-stage pass-through overlay (`pointer-events:none`; your direct
     *  children re-enable) for floating chrome — e.g. a bottom page navigator
     *  on the kernel-truth page API. Spans the whole stage (explorer included);
     *  sits above the scroll surface, below drawers/menus. */
    viewportOverlay?: HTMLElement | ((handle: ViewerHandle) => HTMLElement);
  };
  /**
   * Default true. False removes the toolbar's Page N/N nav cluster — pair it
   * with a BYO navigator (e.g. `slots.viewportOverlay`) so the two never
   * duplicate the same affordance. Deliberately the ONLY per-control toolbar
   * flag (a bounded exception, not the start of per-control config).
   */
  toolbarPageNav?: boolean;
  /**
   * Full-text search tuning. `debounceMs` overrides the plugin's keystroke-
   * silence debounce before a search loop starts (default 150ms).
   */
  search?: { debounceMs?: number };
  /**
   * Base URL under which Parasma's assets (wasm + fonts) are self-hosted.
   * Per-asset overrides win over it. ABSENT ⇒ today's behavior (co-located
   * wasm, jsDelivr fonts). Threaded into {@link ViewerConfig.assetBaseUrl}.
   */
  assetBaseUrl?: string;
  /** The wasm binary source; wins over {@link assetBaseUrl}. ABSENT ⇒ co-located. */
  wasmUrl?: string | URL;
  /** Origin/base to fetch font files from; wins over {@link assetBaseUrl} for fonts. */
  fontBaseUrl?: string;
  /** Full per-face font URL resolver; wins over {@link fontBaseUrl}. */
  fontResolver?: FontUrlResolver;
  /**
   * Engine transport selector (see {@link ViewerConfig.worker}). ABSENT ⇒
   * mountViewer's batteries-included default `"auto"` (every format
   * off-thread, silent in-process fallback); pass `false` to force in-process,
   * or `true`/fn/{url} for an explicit worker.
   */
  worker?: WorkerSelector;
}

/**
 * Per-event payload tuples for {@link HeadlessViewerHandle.on} /
 * {@link HeadlessViewerHandle.emit}. A BYO-chrome
 * parent gets the exact argument type per event rather than `...unknown[]`.
 * Verified against every emit site: `load` carries the {@link DocumentInfo}
 * (headless-viewer.ts onLoaded); `error` is `unknown` at every site (the
 * `loader:error` bus payload + mount.ts catch bindings); `unload` is arg-less;
 * the four scalar edges carry a single number / {@link Theme}.
 *
 * {@link SnippetEvent} is derived from this map's keys (DRY-of-knowledge: the
 * event set and the payloads stay in lockstep — a new event is one entry here).
 */
/**
 * The cell-selection payload `DomBinding.onCellSelect` delivers (and the
 * default formula bar renders): A1-style `ref`, the raw stored value, and the
 * display string. Lives HERE (the chrome-free types module) — consumers on
 * `@parasma/snippet/headless` build their own formula bars from this type, and
 * deriving it structurally from `DomBinding`'s callback signature was the
 * workaround this export retires.
 */
export interface FormulaCell {
  ref: string;
  raw: string;
  display: string;
}

export interface SnippetEventMap {
  load: [DocumentInfo];
  unload: [];
  error: [unknown];
  /**
   * The topmost-visible page changed (1-based). Core-emitted from the kernel's
   * current-page model — it fires WITHOUT any chrome mounted (BYO
   * floating navs just `on("pageChange")`). Smooth programmatic jumps emit the
   * intermediate pages they pass (matches the toolbar's live N).
   *
   * DOC-SWITCH CONTRACT: the current-page reset on a document switch is
   * deliberately SILENT (no `pageChange(1)` is emitted; the sticky replay is
   * cleared). A BYO nav listening only to `pageChange` would keep showing the
   * prior document's page — re-sync from {@link CoreViewerState.currentPage}
   * on the `load` / `unload` events.
   */
  pageChange: [number];
  /**
   * Layout stopped before the memory wall; payload = pages kept. The
   * view is a valid PREFIX of the document ("showing the first N pages").
   * Fires after `load`, at most once per document; never on complete layouts.
   * Sticky on the kernel bus (replay "latest") for late BYO subscribers.
   */
  layoutTruncated: [number];
  sheetChange: [number];
  zoomChange: [number];
  themeChange: [Theme];
  /**
   * A link was activated. Fires for BOTH kinds — `url` (external) and
   * `anchor` (in-document bookmark) — so a consumer can observe navigation it
   * has no intention of blocking. The viewer performs the default navigation
   * after the event UNLESS a listener called `preventDefault()`.
   *
   * The href has already passed the {@link sanitizeHref} allowlist; a target
   * outside {http,https,mailto,tel} never reaches a listener because it never
   * becomes a link at all.
   *
   * On `<parasma-viewer>` this fans out as the CANCELABLE DOM event
   * `parasma-link-activate`, so markup consumers use the native
   * `e.preventDefault()`.
   */
  linkActivate: [LinkActivateEvent];
  /**
   * A cell was selected (grid clicks + paged-sheet clicks) — the formula-bar
   * feed; `null` when the selection clears. Promoted from
   * `handle.dom.onCellSelect` (@advanced) into the ONE subscription idiom, so
   * markup consumers reach it as `parasma-cell-select` via the element
   * fan-out. STABLE.
   */
  cellSelect: [FormulaCell | null];
  /**
   * An in-document comment highlight was clicked — a BYO comments panel's
   * doc→panel path. Deliberately an EVENT, not state: re-clicking the same
   * comment must pulse the panel again (a ref holding an unchanged id fires
   * nothing — see `comments:changed` for the state-shaped feed).
   * Promoted from `handle.dom.onCommentFocus` (@advanced). STABLE.
   */
  commentFocus: [number];
  /**
   * Print-preparation progress: `done` of `total` pages rasterized (exact —
   * the pipeline is sequential over a known page count; `{0, total}` fires at
   * start). Drives a determinate progress bar; prep runs ~100–300 ms/page, so
   * long documents genuinely need one.
   */
  printProgress: [{ done: number; total: number }];
}

export type SnippetEvent = keyof SnippetEventMap;

/**
 * The chrome-free viewer state. Everything a headless consumer can
 * observe about the document + interaction, WITHOUT the default-chrome-only theme/
 * dev flags. {@link ViewerState} widens it for the mountViewer (default-chrome)
 * path.
 */
export interface CoreViewerState {
  documentKind: DocumentKind | null;
  pageCount: number;
  currentPage: number;
  zoom: number;
  loaderStatus: LoaderStatus;
  fontsStatus: FontsStatus;
}

export interface ViewerState extends CoreViewerState {
  theme: Theme;
  dev: boolean;
}

/**
 * A dev-instrument hook the chrome-free core routes its timing/log calls through.
 * Default no-op in headless (the core stays chrome-free); the
 * mountViewer path passes an impl backed by the developer shell so the demo logs
 * identically. This is the ONE seam that keeps the extracted core from importing
 * the dev shell.
 */
export interface DevHooks {
  log(channel: string, message: string, level?: "info" | "ok" | "err"): void;
  mark(phase: "parse" | "layout" | "first-paint" | "fonts-ready"): void;
  markLoadStart(): void;
  refresh(): void;
}

/**
 * The DOM interaction binding {@link attachDom} returns — the composition
 * substrate a chrome layer (mountViewer's default chrome, or a parent's own)
 * builds on. Exposes the stage DOM, the six wired adapters, the interaction
 * helpers, and the fan-out sinks a toolbar/formula-bar subscribes to. Disposable.
 * @advanced (the raw wiring surface; most consumers use {@link HeadlessViewerHandle}).
 */
export interface DomBinding {
  /** The interactive stage element (canvas + grid + overlays). Core-owned. */
  readonly stage: HTMLElement;
  /**
   * The workspace COLUMN (`.ps-workspace`) inside the stage: the document views
   * (viewer + gridview) plus any chrome rows a chrome layer fuses to them —
   * mountViewer prepends its formula row and appends its sheet tabs here so
   * they belong to the spreadsheet workspace, not the app frame. BYO chrome
   * injects the same way (prepend/append). Chrome-free at this tier.
   */
  readonly workspace: HTMLElement;
  /** The paged scroll root (`.ps-viewer`). */
  readonly viewerEl: HTMLElement;
  /** The `.ps-pages` container the `.page` placeholders live in. */
  readonly pagesContainer: HTMLElement;
  /** The grid scroll container (`.ps-gridview`). */
  readonly gridviewEl: HTMLElement;
  /** The single windowed grid canvas. */
  readonly gridCanvasEl: HTMLCanvasElement;
  /** The full-extent grid spacer. */
  readonly gridSpacerEl: HTMLElement;
  /** The one-click "Use installed fonts" affordance (behavior wired by the composer). */
  readonly localFontsCta: HTMLButtonElement;
  /** The detached tool-toggle buttons (a chrome layer adopts them into its toolbar). */
  readonly toolButtons: { readonly select: HTMLButtonElement; readonly hand: HTMLButtonElement };
  /** The six wired DOM adapters (escape hatch). @advanced */
  readonly adapters: {
    readonly viewport: DomViewportAdapter;
    readonly grid: DomGridAdapter;
    readonly selection: DomSelectionAdapter;
    readonly tool: DomToolAdapter;
    readonly clipboard: DomClipboardAdapter;
    readonly contextMenu: DomContextMenuAdapter;
  };
  /** Sync fit computation off the cached page size (a chrome zoom-fit consumes it). */
  computeFit(mode: "width" | "page"): number;
  /** Smooth-scroll a page/y (+optional x) into view (search/comments/links reveal). */
  scrollToAnchorPos(page: number, y: number, x?: number): void;
  /**
   * The topmost visible page (1-based) — the KERNEL-TRUTH MIRROR of the
   * viewport plugin's current-page model, fed by the snippet layer's single
   * DOM sensor (page-sense.ts). Updated via scroll→rAF→exec→bus-echo, so a
   * read immediately after a programmatic scroll can be ~1 frame stale.
   * (Previously an on-demand live DOM read with a bottom>viewTop rule; the
   * heuristic is now the toolbar's top<=80 rule, so this value always agrees
   * with the toolbar's visible N.)
   */
  currentPage(): number;
  /** The cached page count (fed by the pageReady/loadComplete stream). */
  pageCount(): number;
  /** The live zoom scale. */
  getScale(): number;
  /** The current document kind (null when nothing is loaded). */
  getKind(): DocumentKind | null;
  /** The current document display name, if known. */
  currentName(): string | null;
  /** Subscribe to cell selections (grid + paged-excel click) — the formula-bar feed. */
  onCellSelect(cb: (cell: FormulaCell | null) => void): () => void;
  /** Subscribe to comment focus (a comment click) — the comments-panel feed. */
  onCommentFocus(cb: (id: number) => void): () => void;
  /** Capture the current scroll anchor (topmost page + in-page fraction). */
  captureScrollAnchor(): { page: number; frac: number } | null;
  /** Restore a captured scroll anchor. */
  restoreScrollAnchor(anchor: { page: number; frac: number } | null): void;
  /** The per-load fence token (bumped on every load / unload). */
  loadToken(): number;
  /** Bump the fence token (used by the warm unload path). */
  bumpLoadToken(): void;
  /** Reset the cache for a fresh load (does NOT emit). */
  resetForLoad(): void;
  /** Core doc-switch clearing: stage/adapters/grid (NOT chrome). */
  clearForLoad(): void;
  /** Stamp the loaded document into the cache (kind/pageSize/name). */
  setLoaded(info: DocumentInfo, name: string | null): void;
  /** Detach all adapters + core bus subs + wiring. */
  destroy(): void;
}

/**
 * Options for {@link attachDom} — the few things the DOM binding cannot own
 * itself (the link-activation policy + the dev-instrument hook).
 */
/**
 * Payload of the `linkActivate` event — a {@link LinkDecision} plus a DOM-style
 * veto. The viewer opens the link AFTER the event returns unless a listener
 * called `preventDefault()`.
 *
 * WHY A SYNCHRONOUS VETO, when a consumer's confirmation dialog is async:
 * `window.open` requires the browser's *transient user activation*, which the
 * click grants for only a few seconds. The viewer's own open therefore happens
 * inside the click, where it can never be popup-blocked. A consumer who wants
 * to ask its user calls `preventDefault()` FIRST (synchronously, which is all
 * the browser needs), then runs its own async flow and opens the link itself —
 * accepting, by choice, that a human-speed dialog outlives the activation
 * window and may need a same-tab navigation instead of a new tab.
 *
 * Apply {@link sanitizeHref} to any href you open yourself; the viewer already
 * refuses anything outside {http,https,mailto,tel} before it reaches you.
 */
export type LinkActivateEvent = NonNullable<LinkDecision> & {
  /** Suppress the viewer's default navigation. Must be called synchronously. */
  preventDefault(): void;
  /** True once any listener has called {@link preventDefault}. */
  readonly defaultPrevented: boolean;
};

export interface AttachDomOptions {
  /**
   * Emits the `linkActivate` event and returns the (possibly vetoed) payload.
   * Wired by the composition entry; absent means "no listeners", i.e. open.
   */
  emitLinkActivate?: (e: LinkActivateEvent) => void;
  /** Dev-instrument hook; default no-op (keeps the core chrome-free). */
  hooks?: DevHooks;
  /**
   * Who performs the default scroll when a search navigation settles
   * (`search:navigate-resolved`). `"auto"` (default): the DOM layer reveals the
   * hit — grid targets scroll the grid, page targets smooth-scroll the page —
   * with NO chrome required (the BYO guarantee). `"manual"`: the layer stays
   * hands-off; the consumer subscribes to the resolved event and reveals
   * however they like (offset scrolls, animations, veto). The event fires in
   * BOTH modes — `"auto"` only adds the default behavior on top.
   */
  searchReveal?: "auto" | "manual";
}

/**
 * Config for {@link createHeadlessViewer} — the chrome-free composition entry.
 * `root` is REQUIRED (the embeddable path always mounts into a
 * caller-owned slot). The asset/worker fields mirror the core `ViewerConfig`.
 */
export interface HeadlessViewerConfig {
  /** REQUIRED slot: an element or a ShadowRoot the interactive stage mounts into. */
  root: HTMLElement | ShadowRoot;
  /** Base URL for self-hosted assets (wasm + fonts). Per-asset overrides win. */
  assetBaseUrl?: string;
  /** The wasm binary source; wins over {@link assetBaseUrl}. */
  wasmUrl?: string | URL;
  /** Origin/base to fetch font files from; wins over {@link assetBaseUrl}. */
  fontBaseUrl?: string;
  /** Full per-face font URL resolver; wins over {@link fontBaseUrl}. */
  fontResolver?: FontUrlResolver;
  /**
   * Engine transport selector. ABSENT ⇒ `"auto"`, as in mountViewer (every
   * format off-thread, silent in-process fallback); pass `false` to force
   * in-process.
   */
  worker?: WorkerSelector;
  /** Opt in to Word-style installed-font substitution (Font Access API). */
  useLocalFonts?: boolean;
  /** Storage seam for the one-click local-fonts choice (see {@link MountOptions}). */
  localFontsStorage?: { get(key: string): string | null; set(key: string, value: string): void };
  /** Context-menu decision seam (see {@link MountOptions.contextMenu} — same contract). */
  contextMenu?: ContextMenuSeam;
  /** Full-text search tuning (keystroke debounce). */
  search?: { debounceMs?: number };
  /**
   * Theme control for the consumer-owned wrapper. ABSENT ⇒ today's
   * behavior: the consumer owns `data-theme` on `root` entirely (snippet.css
   * reads it; nothing is set or observed). `"light"`/`"dark"` ⇒ set once.
   * `"auto"` ⇒ LIVE-MIRROR the host: the `data-theme` attribute on the nearest
   * shadow host (custom-element embeds) or `document.documentElement`, falling
   * back to `prefers-color-scheme`, tracked via MutationObserver + media
   * listener until destroy. This absorbs the mirror every themed BYO consumer
   * previously hand-rolled.
   */
  theme?: ThemeOption;
  /** Default search-hit reveal; see {@link AttachDomOptions.searchReveal}. */
  searchReveal?: "auto" | "manual";
  /**
   * Style delivery. `"auto"` (default): the snippet's stylesheet is
   * ADOPTED into the scope's root at create time — `document`, or the
   * consumer's `ShadowRoot` when mounting inside one — via constructable
   * stylesheets (`<style>` fallback). Idempotent per root; shared by
   * co-located instances. `"manual"`: nothing is delivered — import the
   * `@parasma/snippet/snippet.css` subpath and own delivery (CSP-strict
   * pipelines, consumer build-time bundling).
   */
  styles?: "auto" | "manual";
  /** Dev-instrument hook; default no-op (the core stays chrome-free). */
  logger?: DevHooks;
}

/**
 * The chrome-free viewer handle. Same load→unload→destroy verbs +
 * zoom/nav as {@link ViewerHandle}, but with NO default toolbar/formula-bar/sheet-
 * tabs/dev-shell — the parent brings its own chrome. Carries MORE reach than the
 * default handle: alongside the `kernel`/`viewer` escape hatches it also exposes
 * `scope` (the RootScope) and `dom` (the DomBinding) so a parent can author its
 * own chrome. {@link ViewerHandle} is this widened with the default-chrome members.
 */
export interface HeadlessViewerHandle {
  /**
   * Load a document from bytes. Resolves once the document is parsed + laid out;
   * the page canvas paints asynchronously AFTER this resolves (windowed/rAF
   * paint). A BYO-chrome integrator must not assume `.page canvas` exists
   * synchronously on resolution — await a `pageChange`/`load` event or the next
   * frame before measuring the canvas.
   */
  load(bytes: ArrayBuffer | Uint8Array, name?: string): Promise<void>;
  /** Load from a URL. Same async-paint contract as {@link HeadlessViewerHandle.load}. */
  loadUrl(url: string): Promise<void>;
  /** WARM release — drop the document but keep engine/kernel/adapters warm. */
  unload(): Promise<void>;
  /** Full ordered idempotent Promise-returning teardown. */
  destroy(): Promise<void>;
  zoom: {
    set(n: number): void;
    in(): void;
    out(): void;
    fit(mode: "width" | "page"): void;
  };
  /**
   * Navigate to page `n` (1-based) through the kernel's current-page model.
   * `n` is CLAMPED to [1, pageCount] and a change fires `pageChange` (a
   * behavior change: an out-of-range `n` used to be a silent no-op; it now
   * scrolls to the nearest bound). If the target page's placeholder does not
   * exist yet (early streaming) the model still updates + `pageChange` fires,
   * but no scroll happens — the pre-existing toolbar limitation, unchanged.
   */
  goToPage(n: number): void;
  selectSheet(i: number): void;
  // ── Reveal verbs, promoted from `handle.dom` (@advanced) to STABLE ───────
  // The consumer-initiated reveals (their comments panel, their hit list,
  // their deep links) that only the engine can resolve to scroll positions —
  // the irreducible tax of a canvas-native viewer. Thin delegations to the
  // SAME dom-binding members, so existing funnels over `handle.dom` and these
  // stay behaviorally identical.
  /** Scroll the continuous grid to a content-space point (grid mode only —
   *  no-op elsewhere). Content space is the engine's virtual surface,
   *  frozen-pane offsets included. STABLE. */
  scrollToGridContent(contentX: number, contentY: number): void;
  /** Smooth-scroll a page-space anchor into view (paged modes). STABLE. */
  scrollToAnchorPos(page: number, y: number, x?: number): void;
  /**
   * Print the document (word/pptx; spreadsheets reject — print ranges are
   * deferred). Rasterizes pages sequentially (one live canvas — the memory
   * model), presents them in a hidden same-origin iframe and opens the print
   * dialog; the consumer's app needs no `@media print` rules. Emits
   * {@link SnippetEventMap.printProgress}. A second call supersedes an
   * in-flight run. Waits for in-flight font loads first (a print with
   * fallback glyphs is a defect, not a draft). STABLE.
   */
  print(opts?: PrintOptions): Promise<void>;
  readonly state: CoreViewerState;
  /**
   * Subscribe a typed snippet event (see {@link SnippetEventMap} for payloads).
   * NOTE `pageChange`: the doc-switch page reset is deliberately silent — do
   * NOT expect a `pageChange(1)` on load; re-sync from `state.currentPage` in
   * your `load`/`unload` handlers (contract documented on the map entry).
   */
  on<E extends SnippetEvent>(event: E, cb: (...args: SnippetEventMap[E]) => void): () => void;
  /**
   * @advanced Composition fan-in — emit a snippet event into the same listener
   * registry `on(...)` reads. Used by a chrome layer built on this handle (e.g.
   * mountViewer's default chrome) to surface its themeChange/pageChange/
   * sheetChange through the single subscription point. A plain headless consumer
   * rarely needs it; the core lifecycle events (load/unload/error/zoomChange) are
   * emitted for you.
   */
  emit<E extends SnippetEvent>(event: E, ...args: SnippetEventMap[E]): void;
  /** Escape hatch — the plugin kernel. */
  kernel: Kernel;
  /** Escape hatch — the core Viewer. */
  viewer: Viewer;
  /** Escape hatch — the RootScope (author-your-own-adapter contract). @advanced */
  scope: RootScope;
  /** Escape hatch — the DOM binding (author-your-own-chrome contract). @advanced */
  dom: DomBinding;
}

export interface ViewerHandle extends HeadlessViewerHandle {
  setTheme(t: Theme): void;
  setDev(on: boolean): void;
  /**
   * Show/hide the viewer chrome (toolbar / formula bar / sheet tabs) at
   * runtime — the chromeless `controls:false` tier as a toggle.
   */
  setControls(on: boolean): void;
  readonly state: ViewerState;
  /**
   * Present ONLY when mounted with an `explorer` config — drives the panel
   * imperatively.
   */
  explorer?: {
    setEntries(entries: ExplorerEntry[]): void;
    setActive(id: string): void;
    isOpen(): boolean;
    toggle(open?: boolean): void;
  };
}
