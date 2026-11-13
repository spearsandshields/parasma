/**
 * plugin-comments — the Word COMMENT MODEL + highlight↔card linking.
 *
 * DISPLAY-ONLY + PRESENT-ONLY: comments
 * are read-only display + navigate — NO editing/replying (app/future). This
 * plugin OWNS the comment LIST (viewer.comments()) + the focused-comment id + the
 * highlight HIT-TEST decision (comments.at). Like plugin-sheets / plugin-viewport
 * it is NOT one of the two XState machines: no async cancellable
 * lifecycle, so it is a lightweight `@xstate/store` slice.
 *
 * BOUNDARY: the plugin holds the MODEL/DECISION; the DOM is the
 * consumer's. It NEVER touches the DOM. It emits a `comments:changed` bus event
 * ({ comments, focused }) and the consumer's thin card renderer applies it —
 * rebuilding the #comments-list cards, the present-only N=0 hide, and reflecting
 * the focused id. The page CLICK→focus applier maps client→content coords, calls
 * comments.at, and focuses the resolved card.
 *
 * BUILD timing: `viewer.comments()` positions (page/anchorY) only exist
 * POST-LAYOUT (the comment range-start lines must be laid out). The ONLY event
 * that guarantees that is the bridged `loadComplete` — the same seam plugin-sheets
 * uses. No documentKind gate is needed: `viewer.comments()` returns `[]` for the
 * docx-only wasm build that lacks comments, for non-word documents, and for a
 * comment-free word doc (core contract), so present-only falls out of the empty
 * event naturally.
 *
 * A doc switch (loader status → idle/planning/loading) resets the slice to empty
 * then re-emits so a stale card strip
 * can't outlive the previous document.
 *
 * The comment CURSOR ('pointer' over a commentRect) is NOT here — that is rule 2
 * of plugin-cursor (cursor.resolve). plugin-comments owns the CLICK→focus decision
 * (comments.at) only; both plugins independently read the shared public
 * `viewer.commentRects` hit-test (cursor for the cursor shape, comments for the
 * focus ids) — the outputs differ, so it is the shared hit-test, not duplicated
 * decision logic. Do NOT add cursor logic here.
 *
 * deps: ["viewport"] — viewport already deps:["loader"], so loadComplete is
 * bridged onto the bus transitively. Two-phase boot guarantees viewport
 * initialize() first.
 */

import type { Comment } from "@parasma/core";
import type { CommentsViewer } from "./ports";
import type { Plugin } from "../registry";
import type { KernelContext } from "../kernel-context";
import { createSliceStore, type StoreSlice } from "../store";

/**
 * Commands OWNED by this plugin. Declaration-merged into the open
 * {@link CommandMap} (targeting `../commands`, the barrel-re-exported kernel
 * module → program-wide) so `exec(id, …)` is arg/return type-checked and an
 * undeclared id fails to register. Signatures authored from the handler bodies.
 */
declare module "../commands" {
  interface CommandMap {
    "comments.list": () => Comment[];
    "comments.focused": () => number | null;
    "comments.at": (q: CommentAtQuery) => Promise<number[]>;
    "comments.focus": (id: number) => void;
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
     * Comment model changed (plugin-comments). Carries the current Word
     * comment list (display-only) + the focused comment id. Emitted on build
     * (loadComplete for a comment-bearing doc), on a doc-switch reset (empty array),
     * and on every `comments.focus`. Sticky ("latest") so a late-attached panel
     * renderer catches the current set. `comments` is empty when the active document
     * is not a Word doc, has zero comments, or a doc switch reset just fired — the
     * consumer's present-only N=0 hide falls out of the empty event. The Comment
     * shape is inlined (mirroring how sheets:changed inlines SheetInfo) to keep
     * bus typing free of a @parasma/core import.
     */
    "comments:changed": {
      comments: {
        id: number;
        author: string;
        date: string | null;
        initials: string | null;
        text: string;
        page: number;
        anchorY: number;
        color: string;
      }[];
      focused: number | null;
    };
  }
}

/** Content-coordinate query for `comments.at` (layout px, already unscaled). */
export interface CommentAtQuery {
  /** 0-based page index. */
  page: number;
  /** X in layout px within the page (caller divides client offset by zoom scale). */
  x: number;
  /** Y in layout px within the page. */
  y: number;
}

/** The comments plugin's store-slice state: comment list + focused id. */
export interface CommentsState extends Record<string, unknown> {
  /** The current document's comments (empty for non-word / comment-free / pre-build). */
  comments: Comment[];
  /** The focused comment id, or null. */
  focused: number | null;
}

const initialState: CommentsState = { comments: [], focused: null };

/** Inclusive-bounds hit-test — the SAME helper shape plugin-cursor / plugin-links use. */
function contains(r: { x: number; y: number; w: number; h: number }, x: number, y: number): boolean {
  return x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h;
}

function makeSlice(): StoreSlice<CommentsState> {
  return {
    state: initialState,
    on: {
      // Adopt a new comment set; clear focus (a new document has no active card).
      setComments: (s, list: Comment[]) => ({ ...s, comments: list, focused: null }),
      // Focus a comment id (the highlight→card link, or a programmatic focus).
      focus: (s, id: number) => ({ ...s, focused: id }),
      reset: () => ({ ...initialState }),
    },
  };
}

/**
 * The comments plugin. Construct with the SAME Viewer the consumer renders into
 * (mirrors sheetsPlugin(viewer) / cursorPlugin(viewer)) so it can read the live
 * `viewer.comments()` / `viewer.commentRects()` without a new engine accessor.
 */
export function commentsPlugin(viewer: CommentsViewer): Plugin {
  const store = createSliceStore(makeSlice());
  const unsubs: Array<() => void> = [];

  const state = (): CommentsState => store.get();

  const emitChanged = (ctx: KernelContext): void => {
    const s = state();
    ctx.bus.emit("comments:changed", { comments: s.comments, focused: s.focused });
  };

  return {
    id: "comments",
    deps: ["viewport"],

    initialize(ctx: KernelContext) {
      unsubs.push(
        // BUILD on loadComplete (the only post-layout seam — comment positions
        // exist only after layout). No kind gate: viewer.comments() is [] for
        // non-word / comment-free / docx-only build, so present-only (the empty
        // event) falls out naturally.
        ctx.bus.subscribe("loadComplete", () => {
          // viewer.comments() is a worker-ready async seam; the bus
          // handler can't be async, so await inside a void IIFE (fire-and-forget,
          // resolves immediately in-process). Order vs other loadComplete
          // subscribers is unaffected — the dispatch+emit just lands a microtask later.
          void (async () => {
            store.dispatch("setComments", await viewer.comments());
            emitChanged(ctx);
          })();
        }),
        // Doc switch: a reset to a non-ready status clears the slice, then
        // emits the now-empty set so a stale
        // card strip can't outlive the previous document.
        ctx.bus.subscribe("loader:status-changed", (s) => {
          if (s === "idle" || s === "planning" || s === "loading") {
            store.dispatch("reset", undefined);
            emitChanged(ctx);
          }
        }),
      );

      // Commands (the plugin surface).
      unsubs.push(
        ctx.commands.register("comments.list", (): Comment[] => state().comments),
        ctx.commands.register("comments.focused", (): number | null => state().focused),

        // The highlight HIT-TEST (the model move of the inline hitRects + id-flatten
        // from the page interaction wiring). Returns the distinct comment ids whose
        // commentRects cover (x,y). [] on non-word (commentRects []), miss, or empty.
        ctx.commands.register("comments.at", async (q: CommentAtQuery): Promise<number[]> => {
          const { page, x, y } = q;
          // commentRects is a worker-ready async seam — awaited internally.
          const hits = (await viewer.commentRects(page)).filter((r) => contains(r, x, y));
          return [...new Set(hits.flatMap((h) => h.ids))];
        }),

        // Focus a comment id (the consumer's click applier dispatches this after a
        // comments.at hit). Emits 'comments:changed' so the card renderer can reflect
        // the focused id — single source, no split-brain.
        ctx.commands.register("comments.focus", (id: number) => {
          store.dispatch("focus", id);
          emitChanged(ctx);
        }),
      );
    },

    destroy(_ctx: KernelContext) {
      for (const u of unsubs.splice(0)) u();
      store.dispatch("reset", undefined);
    },
  };
}
