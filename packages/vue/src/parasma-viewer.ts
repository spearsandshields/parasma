/**
 * <ParasmaViewer> — the sugar component, built ON useParasma.
 *
 * Everything it does is reachable without it (the layer-not-gate rule): it
 * owns a stage element, forwards options to the composable, adds the
 * DECLARATIVE conveniences a template wants — a reactive `source` prop
 * (watch → load, the composable itself stays imperative by decision),
 * `v-model:page` / `v-model:zoom`, lifecycle emits — and exposes the full
 * composable return for template refs.
 *
 * Plain render function on purpose (no SFC): the package compiles with tsc
 * alone, and `defineCustomElement(ParasmaViewer)` works — the component boots
 * correctly inside the shadow root Vue gives custom elements (style delivery
 * adopts into that root; the --ps-* tokens pierce it, see docs/theming.md).
 *
 * BYO chrome: the default slot renders BESIDE the stage inside the component
 * root — but sibling chrome OUTSIDE the component works identically via the
 * exposed composable; the slot is a convenience, not the architecture.
 */
import {
  defineComponent,
  h,
  ref,
  watch,
  type PropType,
  type SlotsType,
  type VNode,
} from "vue";
import { useParasma, type UseParasmaOptions, type UseParasmaReturn } from "./use-parasma";

export type ParasmaSource = ArrayBuffer | Uint8Array | Blob;

export const ParasmaViewer = defineComponent({
  name: "ParasmaViewer",
  props: {
    /** Reactive document source; assign to load, reassign to switch. */
    source: { type: [Object, ArrayBuffer] as unknown as PropType<ParasmaSource | null>, default: null },
    /** Display name (extension helps the unsupported-format message). */
    sourceName: { type: String, default: undefined },
    /** Everything useParasma accepts (theme/styles/search/worker/…). */
    options: { type: Object as PropType<UseParasmaOptions>, default: () => ({}) },
    /** v-model:page (1-based). */
    page: { type: Number, default: undefined },
    /** v-model:zoom (scale, 1 = 100%). */
    zoom: { type: Number, default: undefined },
  },
  emits: {
    ready: () => true,
    error: (_e: unknown) => true,
    truncated: (_pages: number) => true,
    /** Cancelable (synchronously): call `e.preventDefault()` in the handler to
     *  suppress the default navigation, show your own confirmation dialog,
     *  then open `e.href` yourself on confirm — the confirm click supplies
     *  fresh user activation, so your `window.open` is not popup-blocked. */
    linkActivate: (_e: unknown) => true,
    /** Formula-bar feed (`@cell-select`); null on clear. */
    cellSelect: (_c: unknown) => true,
    /** In-document comment-highlight click (`@comment-focus`). */
    commentFocus: (_id: number) => true,
    "update:page": (_n: number) => true,
    "update:zoom": (_z: number) => true,
  },
  slots: Object as SlotsType<{ default?: () => VNode[] }>,
  setup(props, { emit, expose, slots }) {
    const stage = ref<HTMLElement | null>(null);
    const p: UseParasmaReturn = useParasma(stage, props.options);

    // source prop → imperative load (Blob resolved here; the composable's
    // surface stays bytes-only by decision).
    watch(
      () => props.source,
      async (src) => {
        if (!src) return;
        try {
          const bytes =
            src instanceof Blob ? new Uint8Array(await src.arrayBuffer()) : src;
          await p.load(bytes, props.sourceName ?? (src instanceof File ? src.name : undefined));
        } catch {
          /* surfaced below via the status watcher — one error channel */
        }
      },
      { immediate: true },
    );

    watch(p.status, (s) => {
      if (s === "ready") emit("ready");
      if (s === "error") emit("error", p.error.value);
    });
    watch(p.truncatedAt, (n) => {
      if (n !== null) emit("truncated", n);
    });
    // linkActivate forwards once the handle exists; Vue emits are synchronous,
    // so the payload's preventDefault() veto works exactly as on the handle.
    void p.ready.then((h) => {
      if (p.handle.value !== h) return;
      h.on("linkActivate", (e) => emit("linkActivate", e));
      h.on("cellSelect", (c) => emit("cellSelect", c));
      h.on("commentFocus", (id) => emit("commentFocus", id));
    });

    // v-model bridges — echo engine state up; push prop changes down (guarded
    // against echo loops by value comparison).
    watch(p.currentPage, (n) => {
      if (n !== props.page) emit("update:page", n);
    });
    watch(p.zoom, (z) => {
      if (z !== props.zoom) emit("update:zoom", z);
    });
    watch(
      () => props.page,
      (n) => {
        if (n !== undefined && n !== p.currentPage.value) p.commands.goToPage(n);
      },
    );
    watch(
      () => props.zoom,
      (z) => {
        if (z !== undefined && z !== p.zoom.value) p.commands.zoomTo(z);
      },
    );

    expose(p);

    return () =>
      h("div", { class: "parasma-viewer-root" }, [
        // The stage MUST render with no vnode children, ever: it is the
        // engine's mount root, and Vue reconciling children inside it would
        // clobber the injected DOM (slot content renders as SIBLINGS below).
        h("div", { ref: stage, class: "parasma-snippet parasma-viewer-stage" }),
        ...(slots.default ? slots.default() : []),
      ]);
  },
});
