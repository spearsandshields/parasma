/**
 * plugin-selection — canvas-native SELECTION model + pointer pipeline
 * (text, cells and PML text arms). Owns the SelectionState, resolves
 * pointer coordinates through the ONE format-neutral `viewer.hitTest` /
 * `viewer.hitTestObject` pair (a {@link DocLoc} addresses a page, a grid
 * sheet, or a slide — the ENGINE discriminates on the loaded doc kind), drives
 * the in-engine highlight via fire-and-forget `viewer.setSelection` /
 * `viewer.setSelectedObject`, and emits `selection:changed`. Surfaced as ONE
 * kernel command family (`selection.begin/drag/extendTo/end/clear/…`) a DOM
 * adapter drives — there are no per-format `cell*`/`pml*` command families
 * (the adapters send a loc; nobody routes on document kind).
 *
 * The ONE per-format interaction semantic that remains is read off
 * `viewer.documentKind` (never used for verb routing): on a SLIDE, a
 * background click clears the selection and shift+click re-anchors (PML v1 has
 * no extend) — PowerPoint convention; on a page the selection is kept — Word
 * convention.
 *
 * BOUNDARY: the plugin OWNS the model/decision; the DOM is the
 * consumer's (the DomSelectionAdapter / DomGridAdapter). It NEVER touches the
 * DOM. It reaches the engine through the public unified seams — no new
 * accessor.
 *
 * STORE, NOT MACHINE: a `@xstate/store` slice, not an XState
 * machine — selection has NO cancellable background loop (unlike plugin-search's
 * chunked loop); each pointer command is ONE bounded `await viewer.hitTest`.
 * Mirrors plugin-cursor/plugin-grid being slices.
 *
 * ASYNC-READ SEAM (cursor.resolve pattern): `viewer.hitTest` is a worker-ready
 * async seam awaited INTERNALLY, so each command is one `kernel.exec`. Because a
 * newer pointer event can arrive while a hitTest is in flight, every pointer
 * command bumps a monotonic `gestureSeq` SYNCHRONOUSLY at the top, then — after
 * the await — drops its result if `seq !== gestureSeq` (a newer event superseded
 * it). Worker hitTest RPCs are FIFO; combined with the seq
 * guard, a STALE resolve landing after a newer one is dropped, never applied —
 * LATEST-WINS. `selection.end` and `selection.clear` also bump the seq so a
 * hitTest still in flight at pointer-up/ESC is dropped.
 *
 * REPAINT CONTRACT: `setSelection` sets the engine's selection state but does
 * NOT self-repaint (identical to `setSearchHighlight`). The plugin emits
 * `selection:changed` (decision only — it never touches the viewport); the
 * ADAPTER reacts by requesting `viewport.invalidateVisible`, and the engine's
 * additive selection paint pass draws on that repaint.
 *
 * MODE-AWARE: drag-select acts only when the effective tool mode is
 * 'select'. The mode is cached off a `tool:mode-changed` bus signal that
 * DEFAULTS to 'select'; the grid.ts mode-signal style (NOT a dep).
 *
 * Doc switch (loader status → idle/planning/loading) resets the slice to none,
 * clears the engine selection, and bumps the seq so an in-flight hitTest is
 * dropped (parity with plugin-search's reset).
 *
 * deps: ["viewport"] — viewport (→loader) bridges loadComplete onto the bus so
 * the page model hitTest reads is settled post-layout, and its
 * `loader:status-changed` (replay "latest") drives the doc-switch reset.
 */

import type { CellRange, DocHit, DocLoc, ObjectRef, PmlTargetRef, TextPos } from "@parasma/core";
import type { SelectionViewer } from "./ports";
import type { Plugin } from "../registry";
import type { KernelContext } from "../kernel-context";
import { createSliceStore, type StoreSlice } from "../store";

/**
 * Commands OWNED by this plugin. Declaration-merged into the open
 * {@link CommandMap} (targeting `../commands`, the barrel-re-exported kernel
 * module → program-wide) so `exec(id, …)` is arg/return type-checked and an
 * undeclared id fails to register. Signatures authored from the handler bodies.
 * ONE family: the query carries a {@link DocLoc}; the engine's hit
 * result — not the command name — decides text vs cell vs slide semantics.
 */
declare module "../commands" {
  interface CommandMap {
    "selection.begin": (q: SelectionQuery) => Promise<void>;
    "selection.drag": (q: SelectionQuery) => Promise<void>;
    "selection.selectWord": (q: SelectionQuery) => Promise<void>;
    "selection.selectParagraph": (q: SelectionQuery) => Promise<void>;
    "selection.extendTo": (q: SelectionQuery) => Promise<void>;
    "selection.menuAt": (q: MenuQuery) => Promise<void>;
    "selection.closeMenu": () => void;
    "selection.end": () => void;
    "selection.clear": () => void;
    "selection.get": () => SelectionState;
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
    /**
     * Selection model changed (plugin-selection). A CHEAP
     * summary (deliberately cheap — no rect payloads, no `dragging` flag): `kind` is
     * one of "none" | "text" | "object" | "cells" | "pmlText" (the union below —
     * it grew past text/none with the cells and PML selection kinds);
     * `collapsed` is true for a caret (anchor === focus);
     * `anchor`/`focus` are the logical endpoints when kind is "text". Emitted on
     * every pointer down/move/clear edge + doc-switch reset. Drives the DomSelection
     * Adapter to request a viewport repaint (the plugin never touches the DOM). The
     * DRAWN highlight lives in-engine via `viewer.setSelection`; this event carries
     * only the model summary. Sticky ("latest") so a late-attached adapter catches
     * the current selection (a doc-switch `{kind:"none"}` replay is correct).
     */
    "selection:changed": {
      kind: "none" | "text" | "object" | "cells" | "pmlText";
      collapsed: boolean;
      anchor?: { para: number; byte: number };
      focus?: { para: number; byte: number };
      /**
       * A PML text selection (`kind:"pmlText"`): the slide + the
       * clamped target (opaque `(targetKind, targetA, targetB)` triple) +
       * the logical endpoints. plugin-clipboard reads this to extract the shape/cell
       * DocFragment (reusing the WML text/plain + text/html flavors). Absent for
       * non-PML selections. A PML PICTURE object rides the existing `object` arm
       * with `ObjectRef.kind === "picture"`.
       */
      pml?: {
        slide: number;
        targetKind: number;
        targetA: number;
        targetB: number;
        anchor: { para: number; byte: number };
        focus: { para: number; byte: number };
      };
      /**
       * The full {@link ObjectRef} for an object
       * selection (`kind:"object"`). plugin-clipboard reads this directly to build
       * the image flavors (no re-resolve). Absent for text/none selections.
       */
      object?: ObjectRef;
      /**
       * The SML cells selection (`kind:"cells"`): the sheet + the
       * normalized (raw, pre-merge-expansion) rect. plugin-clipboard reads this to
       * extract the TSV / HTML `<table>` flavors. Absent for non-SML selections.
       */
      sheet?: number;
      range?: { r0: number; c0: number; r1: number; c1: number };
      /**
       * The selection mutation sequence, sourced from
       * plugin-selection's monotonic `gestureSeq`. The SINGLE staleness source:
       * plugin-clipboard stamps its pre-serialization cache with this seq and the
       * DomClipboardAdapter compares against it, so a drag-end→instant-Ctrl+C that
       * lands in the debounce window never writes the PREVIOUS selection. A scalar
       * (still cheap).
       */
      seq: number;
    };
    /**
     * A context menu was REQUESTED (plugin-selection).
     * The plugin emits the MODEL; the host renders the DOM menu (the plugin boundary —
     * mirrors the `links.linkAt` veto-seam: the plugin decides, the host draws).
     *
     * `clientX`/`clientY` are OPAQUE host-space pass-through coordinates: the
     * plugin ECHOES them from the originating `contextmenu` event and NEVER computes
     * with them (no hit-test "optimization" may import DOM coordinate semantics into
     * the plugin). The host MAY clamp them to the viewport for on-screen display, but
     * must never feed a derived value back into the plugin.
     *
     * `items` is the menu model ({@link ContextMenuItem}[] — the built-in Copy is
     * `enabled` iff the current selection is a non-collapsed text span OR a
     * selected object; a host `contextMenu` seam may replace/extend the list, and
     * consumer items may carry an `onSelect` the HOST ADAPTER invokes). An EMPTY
     * `items` array is the DISMISS signal — the plugin emits `{items:[]}` on
     * doc-switch and on `selection.closeMenu`, so an open host menu closes when
     * the model says so. Policy "none": a menu request is consumed once; a late
     * subscriber must not replay a stale open — and must NOT persist the payload
     * (stashing it would pin host `onSelect` closures).
     */
    "selection:menu": {
      clientX: number;
      clientY: number;
      items: ContextMenuItem[];
    };
  }
}

/** A 1-based cell coordinate (SML). */
export interface CellPos {
  row: number;
  col: number;
}

/** The cell arm of a {@link DocHit} (a grid hit). */
type CellHit = Extract<DocHit, { kind: "cell" }>;

/**
 * The selection model. `none` = nothing selected; `text` = a logical text span
 * from `anchor` (drag start) to `focus` (drag current/end). `dragging` is true
 * between pointer-down and pointer-up (internal — NOT surfaced on the bus event).
 */
export type SelectionState =
  | { kind: "none" }
  | { kind: "text"; anchor: TextPos; focus: TextPos; dragging: boolean }
  // A selected image object (outline-only, read-only, no
  // drag span). Object-first z-order: a pointerdown over an image selects THIS
  // instead of the text under it.
  | { kind: "object"; ref: ObjectRef }
  // An SML CELLS range (grid view only). A single rectangular
  // range from `anchor` to `focus`, with `active` the distinct active cell (the
  // anchor snapped to its merge). `sheet` scopes it (a sheet switch clears it).
  | {
      kind: "cells";
      sheet: number;
      anchor: CellPos;
      focus: CellPos;
      active: CellPos;
      dragging: boolean;
    }
  // A PML text span, CLAMPED to one shape/table-cell on one
  // slide. `target` is the opaque wire triple the engine round-trips.
  // A PML PICTURE object rides the shared `object` kind (ObjectRef.kind ===
  // "picture"). All kinds stay mutually exclusive.
  | {
      kind: "pmlText";
      slide: number;
      target: PmlTargetRef;
      anchor: TextPos;
      focus: TextPos;
      dragging: boolean;
    };

/** The slice wrapper (store keys must be Record-shaped). */
interface SelectionSlice extends Record<string, unknown> {
  selection: SelectionState;
}

/**
 * A pointer query in CONTENT coords (layout px, already unscaled by the
 * adapter). `loc` is the format-neutral surface address — a page/slide
 * index from the page adapter, or a sheet + content scroll offsets from the
 * grid adapter (the engine resolves frozen-pane quadrants from the offsets).
 */
export interface SelectionQuery {
  loc: DocLoc;
  /** X in content px within the surface (adapter divides client px by zoom). */
  x: number;
  /** Y in content px within the surface. */
  y: number;
}

/**
 * A context-menu query. Carries BOTH the CONTENT
 * coords (`loc`/`x`/`y`, for the inside/outside hit-test) AND the OPAQUE
 * host-space `clientX`/`clientY` the plugin only ECHOES into `selection:menu`
 * (never computes with them).
 */
export interface MenuQuery extends SelectionQuery {
  /** Opaque host-space X (echoed, never computed with). */
  clientX: number;
  /** Opaque host-space Y (echoed, never computed with). */
  clientY: number;
}

/**
 * One context-menu entry (the `selection:menu` item model). Open by design: the
 * built-in Copy item uses the stable id `"copy"`; a host `contextMenu` seam adds
 * items under its own ids.
 */
export interface ContextMenuItem {
  /** Built-in ids are stable strings ("copy"); consumer items use their own ids. */
  id: string;
  label: string;
  enabled: boolean;
  /** Consumer action, invoked by the HOST ADAPTER inside the menu click's
   *  user-activation window. Built-in items omit it (the adapter id-dispatches
   *  "copy" to its onCopy host action). Never invoked by the plugin. */
  onSelect?: () => void;
}

/**
 * The DATA-ONLY context handed to a {@link ContextMenuSeam} — plain data, no DOM
 * nodes, no kernel internals. It deliberately does NOT carry `clientX`/`clientY`
 * (placement is the host adapter's concern).
 */
export interface ContextMenuContext {
  /** The default items the viewer would show (defensive copies — mutating them is inert). */
  items: ContextMenuItem[];
  /** Minimal selection summary — plain data, no DOM nodes, no kernel internals.
   *  `kind` MAY GROW as new surfaces gain selection — do not exhaustively
   *  switch without a default arm. */
  selection: { kind: SelectionState["kind"]; collapsed: boolean };
  /** 0-based surface index the right-click landed on: a PAGE index for docx,
   *  a SLIDE index for pptx. The "cells" kind cannot reach the seam today
   *  (no contextmenu wiring on the grid adapter); this field's meaning is
   *  fixed now so a future grid menu stays compatible. */
  page: number;
}

/** SYNC decision seam: return the full item list to show ([] suppresses the menu).
 *  Called once per menu REQUEST, never on dismiss. A throw, non-array return, or
 *  malformed item falls back to the unmodified default items with one
 *  console.warn — the menu never fails to open because of a bad seam. Needs the
 *  selection CONTENT (e.g. a Translate item)? Use the kernel escape hatch from
 *  onSelect:
 *  `onSelect: () => void handle.kernel.exec("clipboard.serialize").then(c => translate(c?.plain))`
 *  (clipboard.serialize exists today; it resolves `{ seq, plain, html } | null`).
 *  ctx stays minimal on purpose. */
export type ContextMenuSeam = (ctx: ContextMenuContext) => ContextMenuItem[];

/** Normalize an anchor→focus cell pair into an inclusive rect (matches CellRange::new). */
function cellRect(a: CellPos, b: CellPos): CellRange {
  return {
    r0: Math.min(a.row, b.row),
    c0: Math.min(a.col, b.col),
    r1: Math.max(a.row, b.row),
    c1: Math.max(a.col, b.col),
  };
}

/** Effective tool mode. Defaults to 'select'; 'hand' suppresses selection. */
type ToolMode = "select" | "hand";

const initialState: SelectionSlice = { selection: { kind: "none" } };

function makeSlice(): StoreSlice<SelectionSlice> {
  return {
    state: initialState,
    on: {
      // Pointer-down landed on text: anchor === focus, dragging begins.
      begin: (_s, at: TextPos): SelectionSlice => ({
        selection: { kind: "text", anchor: at, focus: at, dragging: true },
      }),
      // Drag moved: extend the focus (anchor held). No-op if not a live text drag.
      setFocus: (s, f: TextPos): SelectionSlice =>
        s.selection.kind === "text"
          ? { selection: { ...s.selection, focus: f } }
          : s,
      // Set a whole text span (double-click WORD / triple-click
      // PARAGRAPH, or a right-click re-anchor collapse). NOT a live drag.
      selectSpan: (_s, span: { anchor: TextPos; focus: TextPos }): SelectionSlice => ({
        selection: { kind: "text", anchor: span.anchor, focus: span.focus, dragging: false },
      }),
      // shift+click/shift+drag EXTEND: keep the existing anchor
      // (when a text span is live), move the focus to `pos`; a fresh extend with no
      // prior text selection anchors at `pos`. `dragging:true` so a subsequent
      // pointer-move continues extending via `setFocus` (shift+drag).
      extendTo: (s, pos: TextPos): SelectionSlice => ({
        selection: {
          kind: "text",
          anchor: s.selection.kind === "text" ? s.selection.anchor : pos,
          focus: pos,
          dragging: true,
        },
      }),
      // Pointer-up: end the drag, keep the span/range — whichever kind is live
      // (ONE event for every kind; non-dragging kinds no-op).
      end: (s): SelectionSlice =>
        s.selection.kind === "text" || s.selection.kind === "cells" || s.selection.kind === "pmlText"
          ? { selection: { ...s.selection, dragging: false } }
          : s,
      // Select an image object (no drag span).
      selectObject: (_s, ref: ObjectRef): SelectionSlice => ({
        selection: { kind: "object", ref },
      }),
      // Pointer-down on an SML cell: anchor === focus, the
      // active cell snapped to its merge, dragging begins.
      cellBegin: (
        _s,
        at: { sheet: number; anchor: CellPos; active: CellPos },
      ): SelectionSlice => ({
        selection: {
          kind: "cells",
          sheet: at.sheet,
          anchor: at.anchor,
          focus: at.anchor,
          active: at.active,
          dragging: true,
        },
      }),
      // Drag / shift-extend moved the focus (anchor + active held). No-op unless a
      // cells selection on the SAME sheet is live.
      cellSetFocus: (s, f: { sheet: number; focus: CellPos }): SelectionSlice =>
        s.selection.kind === "cells" && s.selection.sheet === f.sheet
          ? { selection: { ...s.selection, focus: f.focus } }
          : s,
      // Header click → a whole-row/col used-extent rect. The active cell is
      // the range's first cell (Excel highlights the extent's corner).
      cellSelectRange: (
        _s,
        r: { sheet: number; anchor: CellPos; focus: CellPos; active: CellPos },
      ): SelectionSlice => ({
        selection: {
          kind: "cells",
          sheet: r.sheet,
          anchor: r.anchor,
          focus: r.focus,
          active: r.active,
          dragging: false,
        },
      }),
      // Pointer-down inside a PML shape/cell: anchor === focus,
      // clamped to `target` on `slide`, dragging begins.
      pmlBegin: (
        _s,
        at: { slide: number; target: PmlTargetRef; at: TextPos },
      ): SelectionSlice => ({
        selection: {
          kind: "pmlText",
          slide: at.slide,
          target: at.target,
          anchor: at.at,
          focus: at.at,
          dragging: true,
        },
      }),
      // PML drag / clamp moved the focus (anchor + target held). No-op unless a
      // live pmlText drag.
      pmlSetFocus: (s, f: TextPos): SelectionSlice =>
        s.selection.kind === "pmlText"
          ? { selection: { ...s.selection, focus: f } }
          : s,
      // Explicit clear (ESC / empty pointer-down): back to none.
      setNone: (): SelectionSlice => ({ selection: { kind: "none" } }),
      // Doc switch: full reset to none.
      reset: (): SelectionSlice => ({ selection: { kind: "none" } }),
    },
  };
}

/** Lexicographic `a <= b` over (para, byte). */
function posLe(a: TextPos, b: TextPos): boolean {
  return a.para < b.para || (a.para === b.para && a.byte <= b.byte);
}

/**
 * The SINGLE collapsed-ness definition, shared by `emitChanged` and the
 * contextMenu seam ctx builder so the two can never drift: none → true (nothing
 * spans); text/pmlText → anchor === focus (a caret); object → false (an object
 * select always "contains" something); cells → a single-cell anchor === focus.
 */
function collapsedOf(sel: SelectionState): boolean {
  switch (sel.kind) {
    case "none":
      return true;
    case "text":
    case "pmlText":
      return sel.anchor.para === sel.focus.para && sel.anchor.byte === sel.focus.byte;
    case "object":
      return false;
    case "cells":
      return sel.anchor.row === sel.focus.row && sel.anchor.col === sel.focus.col;
  }
}

/** True when a text selection is a collapsed caret (anchor === focus). */
function isCollapsedText(sel: SelectionState): boolean {
  return sel.kind === "text" && collapsedOf(sel);
}

/** True when `pos` lies within the (normalized) text span [anchor, focus]. */
function withinText(sel: Extract<SelectionState, { kind: "text" }>, pos: TextPos): boolean {
  const [start, end] = posLe(sel.anchor, sel.focus)
    ? [sel.anchor, sel.focus]
    : [sel.focus, sel.anchor];
  return posLe(start, pos) && posLe(pos, end);
}

/** Structural ObjectRef equality (page/kind/key/index). */
function sameRef(a: ObjectRef, b: ObjectRef): boolean {
  return a.page === b.page && a.kind === b.kind && a.key === b.key && a.index === b.index;
}

/** Runtime shape-check for a seam-returned item (a bad seam must not break the menu). */
function isValidMenuItem(i: unknown): i is ContextMenuItem {
  if (!i || typeof i !== "object") return false;
  const o = i as Record<string, unknown>;
  return (
    typeof o.id === "string" &&
    typeof o.label === "string" &&
    typeof o.enabled === "boolean" &&
    (o.onSelect === undefined || typeof o.onSelect === "function")
  );
}

/**
 * The selection plugin. Construct with the SAME Viewer the consumer renders into
 * (mirrors cursorPlugin(viewer) / searchPlugin(viewer)) so it reads the live
 * unified `viewer.hitTest` and drives `viewer.setSelection` without a new
 * accessor.
 *
 * `opts.contextMenu` is the host's {@link ContextMenuSeam} — CONFIG (host
 * policy), not a Viewer capability (the searchPlugin `{debounceMs}` precedent).
 */
export function selectionPlugin(
  viewer: SelectionViewer,
  opts?: { contextMenu?: ContextMenuSeam },
): Plugin {
  const store = createSliceStore(makeSlice());
  const unsubs: Array<() => void> = [];
  const state = (): SelectionState => store.get().selection;

  // Monotonic gesture sequence: bumped SYNCHRONOUSLY at the top of every pointer
  // command (and on end/clear/doc-switch). A command drops its awaited hitTest
  // when `seq !== gestureSeq` — a newer event superseded it (LATEST-WINS).
  let gestureSeq = 0;
  // Effective tool mode, cached off `tool:mode-changed` (default 'select').
  // Read synchronously in begin/drag to suppress hand-mode select.
  let toolMode: ToolMode = "select";

  /** Emit the cheap selection summary (no rects, no dragging flag). */
  const emitChanged = (ctx: KernelContext): void => {
    const sel = state();
    if (sel.kind === "text") {
      ctx.bus.emit("selection:changed", {
        kind: "text",
        collapsed: collapsedOf(sel),
        anchor: { para: sel.anchor.para, byte: sel.anchor.byte },
        focus: { para: sel.focus.para, byte: sel.focus.byte },
        // Surface the mutation sequence so plugin-clipboard can stamp
        // its cache and the adapter can reject a stale copy-event write.
        seq: gestureSeq,
      });
    } else if (sel.kind === "object") {
      // Carry the FULL ObjectRef so plugin-clipboard builds image flavors
      // without re-resolving. An object select is never "collapsed".
      ctx.bus.emit("selection:changed", {
        kind: "object",
        collapsed: false,
        object: sel.ref,
        seq: gestureSeq,
      });
    } else if (sel.kind === "cells") {
      // Carry the sheet + the RAW normalized rect; the engine
      // expands merges at extract time, so plugin-clipboard passes this through.
      ctx.bus.emit("selection:changed", {
        kind: "cells",
        collapsed: collapsedOf(sel),
        sheet: sel.sheet,
        range: cellRect(sel.anchor, sel.focus),
        seq: gestureSeq,
      });
    } else if (sel.kind === "pmlText") {
      // A PML text span. Carry the slide + opaque target +
      // endpoints so plugin-clipboard extracts the shape/cell DocFragment.
      ctx.bus.emit("selection:changed", {
        kind: "pmlText",
        collapsed: collapsedOf(sel),
        pml: {
          slide: sel.slide,
          targetKind: sel.target.targetKind,
          targetA: sel.target.targetA,
          targetB: sel.target.targetB,
          anchor: { para: sel.anchor.para, byte: sel.anchor.byte },
          focus: { para: sel.focus.para, byte: sel.focus.byte },
        },
        seq: gestureSeq,
      });
    } else {
      ctx.bus.emit("selection:changed", { kind: "none", collapsed: true, seq: gestureSeq });
    }
  };

  /** Clear EVERY engine selection surface: `setSelection(null)` clears all
   *  three selection kinds and `setSelectedObject(null)` both object cells —
   *  two calls. */
  const clearAllEngine = (): void => {
    viewer.setSelection(null);
    viewer.setSelectedObject(null);
  };

  /**
   * Apply an expanded text span (word/paragraph) and drive the engine highlight.
   * `range` (when present) is the engine's word/paragraph boundary — NEVER
   * recomputed in TS; a granularity that returns no range (defensive) falls back
   * to the collapsed caret at `pos`.
   */
  const applyHit = (
    ctx: KernelContext,
    hit: { pos: TextPos; range?: { start: TextPos; end: TextPos } },
  ): void => {
    const anchor = hit.range ? hit.range.start : hit.pos;
    const focus = hit.range ? hit.range.end : hit.pos;
    viewer.setSelectedObject(null);
    store.dispatch("selectSpan", { anchor, focus });
    viewer.setSelection({ kind: "text", start: anchor, end: focus });
    emitChanged(ctx);
  };

  /**
   * Emit the context-menu MODEL. Built from the CURRENT (post-reanchor)
   * selection: Copy is enabled iff a non-collapsed text span OR a selected object.
   * `clientX`/`clientY` are echoed opaque host coords. An `items:[]` emit is
   * the dismiss signal.
   *
   * When a `contextMenu` seam is configured it decides the final item list from a
   * DATA-ONLY ctx (defensive default-item copies + the shared-collapsedOf
   * selection summary + the 0-based `page` surface index). A throw / non-array /
   * malformed-item return falls back to the unmodified defaults with one
   * console.warn — the menu never fails to open because of a bad seam. The seam
   * is NEVER run on dismiss ({@link closeMenu} emits `items:[]` directly).
   */
  const emitMenu = (ctx: KernelContext, clientX: number, clientY: number, page: number): void => {
    const sel = state();
    const copyEnabled =
      (sel.kind === "text" && !isCollapsedText(sel)) ||
      sel.kind === "object" ||
      (sel.kind === "pmlText" && !collapsedOf(sel));
    const defaults: ContextMenuItem[] = [{ id: "copy", label: "Copy", enabled: copyEnabled }];
    let items = defaults;
    const seam = opts?.contextMenu;
    if (seam) {
      try {
        const res = seam({
          items: defaults.map((i) => ({ ...i })), // defensive copy — DATA-ONLY ctx
          selection: { kind: sel.kind, collapsed: collapsedOf(sel) },
          page,
        });
        if (Array.isArray(res) && res.every(isValidMenuItem)) items = res;
        else
          console.warn(
            "[parasma:headless] contextMenu seam returned an invalid item list; showing defaults",
          );
      } catch (e) {
        console.warn("[parasma:headless] contextMenu seam threw; showing defaults:", e);
      }
    }
    ctx.bus.emit("selection:menu", { clientX, clientY, items });
  };

  /** Emit the dismiss (empty-items) menu model. */
  const closeMenu = (ctx: KernelContext): void => {
    ctx.bus.emit("selection:menu", { clientX: 0, clientY: 0, items: [] });
  };

  /** Push the current CELLS selection to the engine (fire-and-forget) so the next
   *  grid paint draws the tinted range + distinct active cell, then emit. The
   *  engine's kind-routed setter drops any text/pml selection; the object outline
   *  is cleared explicitly so the kinds stay mutually exclusive. */
  const pushCells = (ctx: KernelContext): void => {
    const s = state();
    if (s.kind !== "cells") return;
    viewer.setSelectedObject(null);
    viewer.setSelection({
      kind: "cells",
      sheet: s.sheet,
      range: cellRect(s.anchor, s.focus),
      anchor: { row: s.active.row, col: s.active.col },
    });
    emitChanged(ctx);
  };

  /** The active cell for a grid hit: the merge anchor when the point lands in a
   *  merge (Excel makes the whole merge active), else the hit cell. */
  const activeOf = (hit: CellHit): CellPos =>
    hit.mergeAnchor ? { row: hit.mergeAnchor.row, col: hit.mergeAnchor.col } : { row: hit.row, col: hit.col };

  /** Select an object (outline-only, no drag span): the object-first z-order hit
   *  shared by every format. Drops any live selection first. */
  const selectObject = (ctx: KernelContext, ref: ObjectRef): void => {
    viewer.setSelection(null); // an object supersedes any text/cells span
    store.dispatch("selectObject", ref);
    viewer.setSelectedObject(ref); // paint the outline
    emitChanged(ctx);
  };

  /** PML v1: a click on empty slide BACKGROUND clears (PowerPoint convention;
   *  a page keeps its selection — Word convention). The ONE documentKind read
   *  on the miss path; verb routing never consults it. */
  const clearOnSlideMiss = (ctx: KernelContext): void => {
    if (viewer.documentKind !== "ppt") return;
    clearAllEngine();
    store.dispatch("setNone", undefined);
    emitChanged(ctx);
  };

  /** Grid header/cell pointer-down + shift-extend share this used-extent logic:
   *  a header click selects the row/col USED EXTENT keyed off
   *  `gridContentBounds`; a corner click is a no-op. */
  const cellBeginAt = async (
    ctx: KernelContext,
    q: SelectionQuery,
    hit: CellHit,
    seq: number,
  ): Promise<void> => {
    if (hit.zone === "corner") return; // corner = no-op
    const sheet = q.loc.surface;
    if (hit.zone === "rowHeader" || hit.zone === "colHeader") {
      const ext = await viewer.gridContentBounds(sheet);
      if (seq !== gestureSeq) return;
      if (!ext) return;
      const [anchor, focus] =
        hit.zone === "rowHeader"
          ? [{ row: hit.row, col: ext.c0 }, { row: hit.row, col: ext.c1 }]
          : [{ row: ext.r0, col: hit.col }, { row: ext.r1, col: hit.col }];
      store.dispatch("cellSelectRange", { sheet, anchor, focus, active: anchor });
      pushCells(ctx);
      return;
    }
    // A cell: collapsed range, active cell = merge anchor (or the cell).
    store.dispatch("cellBegin", {
      sheet,
      anchor: { row: hit.row, col: hit.col },
      active: activeOf(hit),
    });
    pushCells(ctx);
  };

  /**
   * The shared pointer-down body (`selection.begin`, and `selection.extendTo`
   * on a slide — PML v1 has no extend, so shift+click re-anchors like a plain
   * click). Object-first z-order: an
   * image/chart/picture under the point wins over the text/cell beneath it —
   * a SEPARATE engine probe, not folded into the text hitTest.
   */
  const beginBody = async (ctx: KernelContext, q: SelectionQuery): Promise<void> => {
    const seq = ++gestureSeq;
    const obj = await viewer.hitTestObject(q.loc, q.x, q.y);
    if (seq !== gestureSeq) return; // superseded → DROP
    if (obj) {
      selectObject(ctx, obj.object);
      return; // no dragging span ⇒ a subsequent drag auto-no-ops
    }
    const hit = await viewer.hitTest(q.loc, q.x, q.y, "char");
    if (seq !== gestureSeq) return; // superseded → DROP
    if (!hit) {
      clearOnSlideMiss(ctx); // slide background clears; page/grid miss keeps
      return;
    }
    if (hit.kind === "cell") {
      await cellBeginAt(ctx, q, hit, seq);
      return;
    }
    if (hit.target) {
      // A slide shape/cell: anchor clamped to that one target.
      viewer.setSelectedObject(null);
      store.dispatch("pmlBegin", { slide: hit.surface, target: hit.target, at: hit.pos });
      viewer.setSelection({
        kind: "pmlText",
        slide: hit.surface,
        target: hit.target,
        start: hit.pos,
        end: hit.pos,
      }); // collapsed → engine paints nothing
      emitChanged(ctx);
      return;
    }
    // Page text: selecting text supersedes any prior object outline.
    viewer.setSelectedObject(null);
    store.dispatch("begin", hit.pos);
    viewer.setSelection({ kind: "text", start: hit.pos, end: hit.pos }); // collapsed → paints nothing
    emitChanged(ctx);
  };

  return {
    id: "selection",
    deps: ["viewport"],

    initialize(ctx: KernelContext) {
      unsubs.push(
        // Mode signal. "latest" replay: a late boot catches the current mode.
        ctx.bus.subscribe("tool:mode-changed", (e) => {
          toolMode = e.mode;
        }),
        // Switching the tool (explicit toggle) drops the current selection —
        // text, object, OR cells. Fires ONLY on a base-mode edge, so a transient
        // Space-hold pan preserves the selection. Guarded on there being one.
        ctx.bus.subscribe("tool:base-mode-changed", () => {
          if (state().kind === "none") return;
          gestureSeq += 1;
          store.dispatch("setNone", undefined);
          clearAllEngine();
          emitChanged(ctx);
          closeMenu(ctx);
        }),
        // Doc switch: reset to none, clear the engine selection, bump the seq so
        // an in-flight hitTest is dropped. Emit the now-empty summary.
        ctx.bus.subscribe("loader:status-changed", (s) => {
          if (s === "idle" || s === "planning" || s === "loading") {
            gestureSeq += 1;
            store.dispatch("reset", undefined);
            clearAllEngine();
            emitChanged(ctx);
            closeMenu(ctx); // dismiss any open context menu on doc switch
          }
        }),
      );

      // selection.begin(query) — pointer-down on ANY surface. Bump the seq,
      // object-probe then hit-test the point, drop if superseded; the HIT KIND
      // (not the command) decides text vs cell vs slide semantics. Awaited by
      // the adapter BEFORE it enables drag, so the anchor's hitTest always lands
      // before any drag seq exists (never dropped by a later drag).
      unsubs.push(
      ctx.commands.register("selection.begin", async (q: SelectionQuery): Promise<void> => {
        if (toolMode !== "select") return;
        await beginBody(ctx, q);
      }),

      // selection.drag(query) — pointer-move during a drag. Dispatch on the LIVE
      // selection kind (only a dragging kind accepts moves): a text drag extends
      // on the pointer surface, a cells drag is same-sheet-gated, a slide drag
      // resolves on the ANCHOR slide clamped to the anchor target (the
      // drag never escapes the shape/cell). Seq-guarded, LATEST-WINS.
      ctx.commands.register("selection.drag", async (q: SelectionQuery): Promise<void> => {
        if (toolMode !== "select") return;
        const cur = state();
        if (cur.kind === "text" && cur.dragging) {
          const seq = ++gestureSeq;
          const hit = await viewer.hitTest(q.loc, q.x, q.y, "char");
          if (seq !== gestureSeq) return; // stale / out-of-order → DROP (newer wins)
          const s = state();
          if (s.kind !== "text" || !s.dragging || !hit || hit.kind !== "text" || hit.target) return;
          store.dispatch("setFocus", hit.pos);
          viewer.setSelection({ kind: "text", start: s.anchor, end: hit.pos });
          emitChanged(ctx);
        } else if (cur.kind === "cells" && cur.dragging && cur.sheet === q.loc.surface) {
          const seq = ++gestureSeq;
          const hit = await viewer.hitTest(q.loc, q.x, q.y, "char");
          if (seq !== gestureSeq) return; // stale / out-of-order → DROP
          const s = state();
          if (s.kind !== "cells" || !s.dragging || !hit || hit.kind !== "cell") return;
          store.dispatch("cellSetFocus", {
            sheet: q.loc.surface,
            focus: { row: hit.row, col: hit.col },
          });
          pushCells(ctx);
        } else if (cur.kind === "pmlText" && cur.dragging) {
          const seq = ++gestureSeq;
          // Resolve on the ANCHOR slide, clamped to the anchor target.
          const hit = await viewer.hitTest(
            { surface: cur.slide, clamp: cur.target },
            q.x,
            q.y,
            "char",
          );
          if (seq !== gestureSeq) return; // stale / out-of-order → DROP
          const s = state();
          if (s.kind !== "pmlText" || !s.dragging || !hit || hit.kind !== "text") return;
          store.dispatch("pmlSetFocus", hit.pos);
          viewer.setSelection({
            kind: "pmlText",
            slide: s.slide,
            target: s.target,
            start: s.anchor,
            end: hit.pos,
          });
          emitChanged(ctx);
        }
      }),

      // selection.selectWord(query) — double-click. Bump the seq, hit-test at WORD
      // granularity (boundaries live in the ENGINE, never recomputed in TS),
      // drop if superseded, else apply the returned range as the span. A slide
      // hit (target present) no-ops — PML word/paragraph selection is the
      // declared v1 deferral; a grid hit has no word to select.
      ctx.commands.register("selection.selectWord", async (q: SelectionQuery): Promise<void> => {
        if (toolMode !== "select") return;
        const seq = ++gestureSeq;
        const hit = await viewer.hitTest(q.loc, q.x, q.y, "word");
        if (seq !== gestureSeq) return; // superseded → DROP
        if (!hit || hit.kind !== "text" || hit.target) return;
        applyHit(ctx, hit);
      }),

      // selection.selectParagraph(query) — triple-click. Same as selectWord but at
      // PARAGRAPH granularity (the engine's whole-logical-text range).
      ctx.commands.register("selection.selectParagraph", async (q: SelectionQuery): Promise<void> => {
        if (toolMode !== "select") return;
        const seq = ++gestureSeq;
        const hit = await viewer.hitTest(q.loc, q.x, q.y, "paragraph");
        if (seq !== gestureSeq) return; // superseded → DROP
        if (!hit || hit.kind !== "text" || hit.target) return;
        applyHit(ctx, hit);
      }),

      // selection.extendTo(query) — shift+click / shift+drag. On a PAGE: keep the
      // existing text anchor, move the focus (char granularity — no word-snap on
      // extend, v1). On the GRID: extend the range (a header shift+click extends
      // the row/col band to the used extent — Excel convention). On a
      // SLIDE: re-anchor like a plain pointer-down (PML v1 has no extend).
      ctx.commands.register("selection.extendTo", async (q: SelectionQuery): Promise<void> => {
        if (toolMode !== "select") return;
        if (viewer.documentKind === "ppt") {
          // PML v1 extend deferral — shift+click behaves as begin (object-first
          // probe included).
          await beginBody(ctx, q);
          return;
        }
        const seq = ++gestureSeq;
        const hit = await viewer.hitTest(q.loc, q.x, q.y, "char");
        if (seq !== gestureSeq) return; // superseded → DROP
        if (!hit) return;

        if (hit.kind === "cell") {
          if (hit.zone === "corner") return;
          const sheet = q.loc.surface;
          const cur = state();
          const onSheet = cur.kind === "cells" && cur.sheet === sheet;

          // Header shift+click extends the row/col BAND to the full used extent
          // (Excel: click col A header, shift+click col C → cols A:C). Keep
          // the anchor's axis index (from the current selection, else the click);
          // the perpendicular axis spans content_bounds. Mirrors the begin header
          // branch — without this, the raw header hit.row/col collapsed the band.
          if (hit.zone === "rowHeader" || hit.zone === "colHeader") {
            const ext = await viewer.gridContentBounds(sheet);
            if (seq !== gestureSeq) return;
            if (!ext) return;
            const [anchor, focus] =
              hit.zone === "rowHeader"
                ? [
                    { row: onSheet ? cur.anchor.row : hit.row, col: ext.c0 },
                    { row: hit.row, col: ext.c1 },
                  ]
                : [
                    { row: ext.r0, col: onSheet ? cur.anchor.col : hit.col },
                    { row: ext.r1, col: hit.col },
                  ];
            store.dispatch("cellSelectRange", { sheet, anchor, focus, active: anchor });
            pushCells(ctx);
            return;
          }

          const focus = { row: hit.row, col: hit.col };
          if (onSheet) {
            store.dispatch("cellSelectRange", {
              sheet,
              anchor: cur.anchor,
              focus,
              active: cur.active,
            });
          } else {
            store.dispatch("cellBegin", { sheet, anchor: focus, active: activeOf(hit) });
          }
          pushCells(ctx);
          return;
        }

        // Page text extend.
        viewer.setSelectedObject(null); // extending text supersedes any object outline
        store.dispatch("extendTo", hit.pos);
        const s = state();
        if (s.kind === "text") viewer.setSelection({ kind: "text", start: s.anchor, end: s.focus });
        emitChanged(ctx);
      }),

      // selection.menuAt(query) — right-click. If the click point is OUTSIDE
      // the current selection, RE-ANCHOR (select the object under it, or collapse
      // the text selection to the click point / clear on a text miss) BEFORE
      // building the menu; INSIDE keeps the selection. Then emit the menu MODEL with
      // the ECHOED opaque host coords. Object-first z-order: probe the
      // object seam first. Async → seq-guarded. Slide/grid text hits build
      // no caret here (the menu wiring is a page feature today — the adapters
      // only send page locs).
      ctx.commands.register("selection.menuAt", async (q: MenuQuery): Promise<void> => {
        if (toolMode !== "select") return; // inert in hand mode
        const seq = ++gestureSeq;
        const cur = state();
        const obj = await viewer.hitTestObject(q.loc, q.x, q.y);
        if (seq !== gestureSeq) return; // superseded → DROP
        if (obj) {
          // Clicked an object: INSIDE iff it is the already-selected one.
          const inside = cur.kind === "object" && sameRef(cur.ref, obj.object);
          if (!inside) {
            selectObject(ctx, obj.object);
          }
        } else {
          const hit = await viewer.hitTest(q.loc, q.x, q.y, "char");
          if (seq !== gestureSeq) return; // superseded → DROP
          const pos = hit && hit.kind === "text" && !hit.target ? hit.pos : null;
          const inside =
            cur.kind === "text" && !isCollapsedText(cur) && pos != null && withinText(cur, pos);
          if (!inside) {
            viewer.setSelectedObject(null);
            if (pos) {
              // Collapse to the click point (Word: right-click places the caret).
              store.dispatch("selectSpan", { anchor: pos, focus: pos });
              viewer.setSelection({ kind: "text", start: pos, end: pos });
            } else {
              store.dispatch("setNone", undefined);
              viewer.setSelection(null);
            }
            emitChanged(ctx);
          }
        }
        emitMenu(ctx, q.clientX, q.clientY, q.loc.surface);
      }),

      // selection.closeMenu() — dismiss the context menu model (ESC / a menu action
      // taken / host-driven outside-click). Emits the empty-items signal.
      ctx.commands.register("selection.closeMenu", (): void => {
        closeMenu(ctx);
      }),

      // selection.end() — pointer-up on ANY surface. Bump the seq (drop any
      // in-flight drag hitTest), end whichever drag is live (the store's `end`
      // no-ops for object/none). No re-emit strictly needed (the span is
      // unchanged) but we emit for adapter symmetry.
      ctx.commands.register("selection.end", (): void => {
        gestureSeq += 1;
        const kind = state().kind;
        if (kind !== "text" && kind !== "cells" && kind !== "pmlText") return;
        store.dispatch("end", undefined);
        emitChanged(ctx);
      }),

      // selection.clear() — ESC / sheet switch / grid-mode off / doc-level
      // clear. Bump the seq, drop to none, clear every engine selection surface,
      // emit the empty summary.
      ctx.commands.register("selection.clear", (): void => {
        gestureSeq += 1;
        store.dispatch("setNone", undefined);
        clearAllEngine();
        emitChanged(ctx);
      }),

      // selection.get() — read the current model (tests + consumers).
      ctx.commands.register("selection.get", (): SelectionState => state()),
      );
    },

    destroy(_ctx: KernelContext) {
      gestureSeq += 1;
      for (const u of unsubs.splice(0)) u();
      clearAllEngine(); // every selection surface, both object cells
      store.dispatch("reset", undefined);
    },
  };
}
