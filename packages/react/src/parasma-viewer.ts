/**
 * <ParasmaViewer> — the sugar component, built ON useParasma.
 *
 * Everything it does is reachable without it (the layer-not-gate rule): it
 * owns a stage element, forwards options to the hook, and adds the
 * DECLARATIVE conveniences a component tree wants — a `source` prop (change
 * it to load, the hook itself stays imperative), `page` / `zoom` props with
 * change callbacks, lifecycle callbacks — and exposes the full hook result
 * through `ref`.
 *
 * BYO chrome: `children` render BESIDE the stage inside the component root —
 * sibling chrome OUTSIDE the component works identically through the hook;
 * children are a convenience, not the architecture.
 */
import {
  createElement,
  useEffect,
  useImperativeHandle,
  useRef,
  type CSSProperties,
  type ReactNode,
  type Ref,
} from "react";
import type { FormulaCell, LinkActivateEvent } from "@parasma/snippet/headless";
import { useParasma, type UseParasmaOptions, type UseParasmaResult } from "./use-parasma";

export type ParasmaSource = ArrayBuffer | Uint8Array | Blob;

export interface ParasmaViewerProps {
  /** Document source; set to load, change to switch. */
  source?: ParasmaSource | null;
  /** Display name (extension helps the unsupported-format message). */
  sourceName?: string;
  /** Everything useParasma accepts (theme/styles/search/worker/…). Read at
   *  boot; remount (a new `key`) to change. */
  options?: UseParasmaOptions;
  /** 1-based page to show; echoes back through `onPageChange`. */
  page?: number;
  /** Scale (1 = 100%); echoes back through `onZoomChange`. */
  zoom?: number;
  onReady?: () => void;
  onError?: (error: unknown) => void;
  onTruncated?: (pages: number) => void;
  /** Synchronous: call `e.preventDefault()` to suppress the default
   *  navigation, show your own confirmation, then open `e.href` yourself on
   *  confirm — the confirm click supplies fresh user activation, so your
   *  `window.open` is not popup-blocked. */
  onLinkActivate?: (e: LinkActivateEvent) => void;
  /** Formula-bar feed; null on clear. */
  onCellSelect?: (cell: FormulaCell | null) => void;
  /** In-document comment-highlight click. */
  onCommentFocus?: (id: number) => void;
  onPageChange?: (page: number) => void;
  onZoomChange?: (zoom: number) => void;
  className?: string;
  style?: CSSProperties;
  children?: ReactNode;
  ref?: Ref<UseParasmaResult>;
}

export function ParasmaViewer(props: ParasmaViewerProps): ReactNode {
  const p = useParasma(props.options);
  // Callbacks read through a ref, so a new function identity each render
  // never resubscribes.
  const latest = useRef(props);
  useEffect(() => {
    latest.current = props;
  });

  useImperativeHandle(props.ref, () => p, [p]);

  // source → imperative load (Blob resolved here; the hook stays bytes-only).
  const { source, sourceName } = props;
  useEffect(() => {
    if (!source) return;
    let current = true;
    void (async () => {
      const bytes = source instanceof Blob ? new Uint8Array(await source.arrayBuffer()) : source;
      if (!current) return;
      const name = sourceName ?? (source instanceof File ? source.name : undefined);
      // Failures surface through onError below — one error channel.
      await p.load(bytes, name).catch(() => {});
    })();
    return () => {
      current = false;
    };
    // p.load is the session's (stable), so only the source drives this.
  }, [source, sourceName]);

  const { status, error, truncatedAt, currentPage, zoom, handle } = p;
  useEffect(() => {
    if (status === "ready") latest.current.onReady?.();
    if (status === "error") latest.current.onError?.(error);
  }, [status, error]);
  useEffect(() => {
    if (truncatedAt !== null) latest.current.onTruncated?.(truncatedAt);
  }, [truncatedAt]);

  // Handle events forward once the handle exists; the handler runs
  // synchronously, so the payload's preventDefault() veto works as on the handle.
  useEffect(() => {
    if (!handle) return;
    const offs = [
      handle.on("linkActivate", (e) => latest.current.onLinkActivate?.(e)),
      handle.on("cellSelect", (c) => latest.current.onCellSelect?.(c)),
      handle.on("commentFocus", (id) => latest.current.onCommentFocus?.(id)),
    ];
    return () => offs.forEach((off) => off());
  }, [handle]);

  // page / zoom bridges — echo engine state up; push prop changes down
  // (guarded against echo loops by value comparison).
  useEffect(() => {
    if (currentPage !== latest.current.page) latest.current.onPageChange?.(currentPage);
  }, [currentPage]);
  useEffect(() => {
    if (zoom !== latest.current.zoom) latest.current.onZoomChange?.(zoom);
  }, [zoom]);
  const { page: pageProp, zoom: zoomProp } = props;
  useEffect(() => {
    // Only a prop change pushes down; engine-side moves echo up instead.
    if (pageProp !== undefined && pageProp !== currentPage) p.commands.goToPage(pageProp);
  }, [pageProp]);
  useEffect(() => {
    if (zoomProp !== undefined && zoomProp !== zoom) p.commands.zoomTo(zoomProp);
  }, [zoomProp]);

  return createElement(
    "div",
    { className: ["parasma-viewer-root", props.className].filter(Boolean).join(" "), style: props.style },
    // The stage MUST render with no children, ever: it is the engine's mount
    // root, and React reconciling inside it would clobber the injected DOM.
    createElement("div", { ref: p.stageRef, className: "parasma-snippet parasma-viewer-stage" }),
    props.children,
  );
}
