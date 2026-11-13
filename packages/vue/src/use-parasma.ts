/**
 * useParasma — the Vue-native composition core.
 *
 * Composable-first by decision: state lives in this function's return value —
 * plain reactive refs any sibling, parent or store can consume, with no
 * component-tree constraints (consumer chrome is typically SIBLINGS of the
 * stage, where provide/inject cannot reach). The `<ParasmaViewer>` component
 * is sugar built ON this; nothing is reachable only through it.
 *
 * LAYER, NEVER A GATE: every member here is sugar over the same handle /
 * kernel / bus, and `handle` itself is exposed. Entry points only increase.
 *
 * Reactivity discipline: `shallowRef` for anything holding engine objects —
 * Vue's deep proxying must never wrap a wasm-bound object graph.
 *
 * Lifecycle (the ceremony this absorbs, otherwise ~100 lines of consumer code):
 * boots on mount (module-scope-pure — nothing here touches the DOM at import
 * time), guards the unmounted-while-booting race, splits teardown (unhook
 * subscriptions while the kernel is alive, then destroy), and suppresses
 * stale async tails against a disposed instance.
 */
import {
  computed,
  getCurrentInstance,
  onMounted,
  onScopeDispose,
  ref,
  shallowRef,
  type ComputedRef,
  type Ref,
  type ShallowRef,
} from "vue";
import {
  createHeadlessViewer,
  createViewerSession,
  type HeadlessViewerConfig,
  type HeadlessViewerHandle,
  type FormulaCell,
  type ToolMode,
  type ParasmaStatus,
  type ParasmaViewState,
  type ParasmaSearchState,
  type ParasmaCommands,
  type ViewerSnapshot,
} from "@parasma/snippet/headless";
import type { DocumentKind } from "@parasma/core";

// The state rules (event wiring, status machine, search counter, commands)
// live in the framework-neutral session; this file binds them to refs.
export type { ParasmaStatus, ParasmaViewState, ParasmaSearchState, ParasmaCommands };

export interface UseParasmaOptions
  extends Omit<HeadlessViewerConfig, "root"> {
  /** Applied after each successful load. Storage stays yours. */
  initialViewState?: Partial<ParasmaViewState>;
}

export interface UseParasmaReturn {
  // ── machine-grade lifecycle ──
  status: Ref<ParasmaStatus>;
  /** The load()/unload() rejection, if status === "error". */
  error: ShallowRef<unknown>;
  load(bytes: ArrayBuffer | Uint8Array, name?: string): Promise<void>;
  unload(): Promise<void>;
  // ── document ──
  documentKind: Ref<DocumentKind | null>;
  /**
   * The REAL page dimensions (CSS px), available the moment `load()` resolves
   * — i.e. right after PARSE, long before layout finishes. This is the number
   * a loading skeleton should use: the only window where dimensions are
   * genuinely unknown is the parse itself (tens of ms — an indeterminate
   * spinner's job); faking a page size after that is a choice, not a
   * necessity.
   */
  pageSize: ShallowRef<{ widthPx: number; heightPx: number } | null>;
  /** Streaming-aware: grows with pageReady, settles on loadComplete. */
  pageCount: Ref<number>;
  currentPage: Ref<number>;
  /** Pages kept when layout stopped at the memory guardrail; null = complete. */
  truncatedAt: Ref<number | null>;
  // ── view ──
  zoom: Ref<number>;
  theme: Ref<"light" | "dark" | null>;
  sheets: ShallowRef<ReadonlyArray<{ name: string; pageCount: number }>>;
  activeSheet: Ref<number>;
  gridMode: Ref<"grid" | "pages" | null>;
  // ── feeds ──
  cell: ShallowRef<FormulaCell | null>;
  comments: ShallowRef<ReadonlyArray<{ id: number; page: number; anchorY: number }>>;
  focusedCommentId: Ref<number | null>;
  fontsLoading: Ref<{ arrived: number } | null>;
  /** The EFFECTIVE tool ("select" default): base mode + space-hold override. */
  tool: Ref<ToolMode>;
  /** Exact print-prep progress ({done, total}); null when no run is active.
   *  Resets to null when the run completes or a new document loads. */
  printProgress: Ref<{ done: number; total: number } | null>;
  /** Streams while a search runs (`status: "searching"` → `"done"`); reveal,
   *  cross-sheet navigation and repaint are the engine's job. */
  search: ShallowRef<ParasmaSearchState>;
  // ── verbs ──
  commands: ParasmaCommands;
  viewState(): ParasmaViewState;
  applyViewState(s: Partial<ParasmaViewState>): void;
  // ── the escape hatch (the layer-not-gate guarantee) ──
  /** null until mounted+booted; everything above keeps working across the
   *  boot (refs just hold their initial values). */
  handle: ShallowRef<HeadlessViewerHandle | null>;
  /** Resolves when the handle exists (post-mount boot). */
  ready: Promise<HeadlessViewerHandle>;
  isReady: ComputedRef<boolean>;
}

export function useParasma(
  root: Ref<HTMLElement | null | undefined>,
  options: UseParasmaOptions = {},
): UseParasmaReturn {
  const { initialViewState, ...config } = options;
  const session = createViewerSession({ initialViewState });
  const s0 = session.getSnapshot();
  const status = ref<ParasmaStatus>(s0.status);
  const error = shallowRef<unknown>(s0.error);
  const documentKind = ref<DocumentKind | null>(s0.documentKind);
  const pageSize = shallowRef(s0.pageSize);
  const pageCount = ref(s0.pageCount);
  const currentPage = ref(s0.currentPage);
  const truncatedAt = ref(s0.truncatedAt);
  const zoom = ref(s0.zoom);
  const theme = ref(s0.theme);
  const sheets = shallowRef(s0.sheets);
  const activeSheet = ref(s0.activeSheet);
  const gridMode = ref(s0.gridMode);
  const cell = shallowRef(s0.cell);
  const comments = shallowRef(s0.comments);
  const focusedCommentId = ref(s0.focusedCommentId);
  const fontsLoading = ref(s0.fontsLoading);
  const tool = ref<ToolMode>(s0.tool);
  const printProgress = ref(s0.printProgress);
  const search = shallowRef<ParasmaSearchState>(s0.search);
  const handle = shallowRef<HeadlessViewerHandle | null>(null);
  const refs: { [K in keyof ViewerSnapshot]: Ref<ViewerSnapshot[K]> } = {
    status, error, documentKind, pageSize, pageCount, currentPage, truncatedAt, zoom, theme,
    sheets, activeSheet, gridMode, cell, comments, focusedCommentId, fontsLoading, tool,
    printProgress, search,
  };

  // Copy only the fields whose identity changed: the session keeps unchanged
  // fields' identity, so every other ref stays quiet.
  let prev = s0;
  const unsubscribe = session.subscribe(() => {
    const next = session.getSnapshot();
    for (const k in refs) {
      const key = k as keyof ViewerSnapshot;
      if (!Object.is(prev[key], next[key])) (refs[key] as Ref<unknown>).value = next[key];
    }
    prev = next;
  });

  let disposed = false;
  const ready = session.ready;
  const isReady = computed(() => handle.value !== null);

  const boot = async (): Promise<void> => {
    const el = root.value;
    if (!el) {
      session.abandon(new Error("useParasma: root ref resolved to nothing at mount"));
      return;
    }
    try {
      const h = await createHeadlessViewer({ ...config, root: el });
      if (disposed) {
        // Unmounted while booting: nothing was wired — reclaim and stop.
        await h.destroy();
        session.abandon(new Error("useParasma: disposed during boot"));
        return;
      }
      handle.value = h;
      session.connect(h, el);
    } catch (e) {
      if (disposed) session.abandon(e);
      else session.fail(e);
    }
  };
  // Inside a component: boot at mount (the ref resolves then). Outside one
  // (a store, a bare effectScope, tests): boot on a microtask — the
  // caller hands us an already-resolved element ref in those contexts.
  if (getCurrentInstance()) onMounted(boot);
  else void Promise.resolve().then(boot);

  // Teardown: unhook while alive, THEN destroy (the split every consumer
  // previously had to discover). onScopeDispose covers both component unmount
  // and effectScope disposal.
  onScopeDispose(() => {
    disposed = true;
    session.disconnect();
    unsubscribe();
    const h = handle.value;
    handle.value = null;
    if (h) void h.destroy();
  });

  return {
    status,
    error,
    load: session.load,
    unload: session.unload,
    documentKind,
    pageSize,
    pageCount,
    currentPage,
    truncatedAt,
    zoom,
    theme,
    sheets,
    activeSheet,
    gridMode,
    cell,
    comments,
    focusedCommentId,
    fontsLoading,
    tool,
    printProgress,
    search,
    commands: session.commands,
    viewState: session.viewState,
    applyViewState: session.applyViewState,
    handle,
    ready,
    isReady,
  };
}
