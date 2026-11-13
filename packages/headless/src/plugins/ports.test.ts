/**
 * Plugin PORTS — the compile-time ZERO-CAPABILITY-LOSS self-test.
 *
 * These assertions are enforced by `tsc` (the file compiling IS the test). They
 * lock two invariants:
 *   1. the concrete `Viewer` is assignable to EVERY port (so the real object,
 *      in-process or worker-backed, keeps driving the ported factories — no
 *      call-site change, no capability lost);
 *   2. a minimal FAKE implementing only a port's Pick keys satisfies that port
 *      (the testability win — a plugin can be driven without the whole Viewer).
 *
 * RED conditions: remove a Pick key from
 * CursorViewer (e.g. "cellAt") → cursor.ts's `viewer.cellAt(...)` stops
 * type-checking against the narrowed param; drop a method from the fake below →
 * it stops satisfying the port. Re-adding restores green.
 */
import { describe, it, expect } from "vitest";
import type { Viewer } from "@parasma/core";
import type {
  LoaderViewer,
  CursorViewer,
  LinksViewer,
  ViewportViewer,
  SheetsViewer,
  GridViewer,
  CommentsViewer,
  SearchViewer,
  SelectionViewer,
  ClipboardViewer,
  FontsViewer,
} from "./ports";

// (1) The concrete Viewer satisfies every port — assignability is the lock (a
// port that named a non-Viewer member would fail HERE, not just in the plugin).
const _loaderFromViewer: LoaderViewer = null as unknown as Viewer;
const _cursorFromViewer: CursorViewer = null as unknown as Viewer;
const _linksFromViewer: LinksViewer = null as unknown as Viewer;
const _viewportFromViewer: ViewportViewer = null as unknown as Viewer;
const _sheetsFromViewer: SheetsViewer = null as unknown as Viewer;
const _gridFromViewer: GridViewer = null as unknown as Viewer;
const _commentsFromViewer: CommentsViewer = null as unknown as Viewer;
const _searchFromViewer: SearchViewer = null as unknown as Viewer;
const _selectionFromViewer: SelectionViewer = null as unknown as Viewer;
const _clipboardFromViewer: ClipboardViewer = null as unknown as Viewer;
const _fontsFromViewer: FontsViewer = null as unknown as Viewer;

// (2) A minimal fake implementing exactly the Pick keys satisfies the port.
const _cursorFake: CursorViewer = {
  linkRects: () => Promise.resolve([]),
  commentRects: () => Promise.resolve([]),
  cellAt: () => Promise.resolve(null),
};
const _linksFake: LinksViewer = {
  linkRects: () => Promise.resolve([]),
  bookmarkPosition: () => Promise.resolve(null),
};
const _loaderFake: LoaderViewer = {
  load: () => Promise.resolve({ pageSize: { widthPx: 1, heightPx: 1 }, documentKind: "word" }),
  on: () => () => {},
};

// The trivial 1-method port: a fake that is JUST `pageSize()`.
const _viewportFake: ViewportViewer = {
  pageSize: () => Promise.resolve({ widthPx: 1, heightPx: 1 }),
};

// The STRONGEST orthogonality proof: selectionPlugin drives on a fake that
// implements only its 6-member port (the unified verb set, not per-format
// members) — none of the Viewer's load / font / paint machinery. Each member
// is a bare stub (the plugin's shape, not its behavior, is what the port locks).
const _selectionFake: SelectionViewer = {
  hitTest: () => Promise.resolve(null),
  hitTestObject: () => Promise.resolve(null),
  setSelection: () => {},
  setSelectedObject: () => {},
  gridContentBounds: () => Promise.resolve(null),
  documentKind: null,
};

describe("plugin ports (zero-capability-loss)", () => {
  it("compiles: the concrete Viewer satisfies every port", () => {
    // The real assertions are the type bindings above (checked by tsc). A runtime
    // touch keeps vitest from reporting an empty suite.
    expect(
      [
        _loaderFromViewer,
        _cursorFromViewer,
        _linksFromViewer,
        _viewportFromViewer,
        _sheetsFromViewer,
        _gridFromViewer,
        _commentsFromViewer,
        _searchFromViewer,
        _selectionFromViewer,
        _clipboardFromViewer,
        _fontsFromViewer,
      ],
    ).toHaveLength(11);
  });

  it("compiles: minimal fakes implementing only a port's keys satisfy it", () => {
    expect([_cursorFake, _linksFake, _loaderFake, _viewportFake, _selectionFake]).toHaveLength(5);
  });
});
