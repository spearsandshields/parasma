/**
 * plugin-cursor tests.
 *
 * cursor.resolve is a PURE decision: given a content coordinate {page,x,y} it
 * returns a CSS cursor keyword by priority (link > comment > excel-cell >
 * default), reading the live Viewer hit-tests + the documentKind cached off the
 * loader:loaded bus event. These tests stub the Viewer hit-tests and a minimal
 * loader plugin (to emit loader:loaded with a documentKind) — no real engine.
 *
 * The KEY invariant: NO text cursor (I-beam) ships — plain text falls through to
 * "" (the OS default arrow), because the text-cursor hit-test is deferred
 * (no selectable-text hit-test yet). Asserted explicitly below.
 */
import { describe, it, expect } from "vitest";
import type { DocumentInfo, DocumentKind } from "@parasma/core";
import { createKernel } from "../kernel";
import type { Plugin } from "../registry";
import type { KernelContext } from "../kernel-context";
import { cursorPlugin } from "./cursor";
import type { CursorViewer } from "./ports";

type Rect = { x: number; y: number; w: number; h: number };

/** A stub Viewer exposing only the hit-tests cursor.resolve reads. */
interface StubConfig {
  links?: Rect[];
  comments?: Rect[];
  /** When set, cellAt returns a cell for any (x,y) inside this rect. */
  cellRect?: Rect | null;
}

/**
 * The port testability demo: the fake is typed as the narrow
 * {@link CursorViewer} port with NO `as unknown as Viewer` double cast — it
 * implements exactly the three hit-tests the port names, and the plugin factory
 * (now `cursorPlugin(v: CursorViewer)`) accepts it structurally. Removing a Pick
 * key from CursorViewer would make cursor.ts stop type-checking; dropping a
 * method here would make this fake stop satisfying the port. Zero-loss is
 * compile-locked.
 */
function stubViewer(cfg: StubConfig): CursorViewer {
  const inside = (r: Rect, x: number, y: number) =>
    x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h;
  return {
    linkRects: (_idx: number) =>
      Promise.resolve((cfg.links ?? []).map((r) => ({ ...r, target: "url:https://x" }))),
    commentRects: (_idx: number) =>
      Promise.resolve((cfg.comments ?? []).map((r) => ({ ...r, ids: [1], color: "#abcdef" }))),
    cellAt: (_p: number, x: number, y: number) =>
      Promise.resolve(
        cfg.cellRect && inside(cfg.cellRect, x, y)
          ? { ref: "A1", raw: "1", display: "1" }
          : null,
      ),
  };
}

/**
 * A minimal loader stand-in: satisfies cursor's deps:["loader"] and lets a test
 * push a documentKind onto the replayable bus exactly as the real loader does.
 */
function fakeLoaderPlugin(kind: DocumentKind | null): Plugin {
  return {
    id: "loader",
    initialize(ctx: KernelContext) {
      if (kind) {
        ctx.bus.emit("loader:loaded", { pageSize: { widthPx: 600, heightPx: 800 }, documentKind: kind } as DocumentInfo);
      }
    },
  };
}

async function boot(stub: StubConfig, kind: DocumentKind | null = "word") {
  const kernel = createKernel();
  kernel.use(fakeLoaderPlugin(kind));
  kernel.use(cursorPlugin(stubViewer(stub)));
  await kernel.boot();
  return kernel;
}

const resolve = (kernel: Awaited<ReturnType<typeof boot>>, x: number, y: number) =>
  kernel.exec("cursor.resolve", { page: 0, x, y });

/**
 * A {@link CursorViewer} fake that COUNTS each hit-test call and lets a test
 * swap the returned rects mid-life (to simulate a re-layout producing new rects).
 * The per-generation rect cache in cursor.ts is proven by asserting these counts:
 * a cache HIT costs ZERO extra linkRects/commentRects calls; an invalidation
 * forces a re-fetch (count bumps) and serves the swapped rects.
 */
interface CountingViewer {
  viewer: CursorViewer;
  counts: { link: number; comment: number; cell: number };
  setLinks: (r: Rect[]) => void;
  setComments: (r: Rect[]) => void;
}
function countingViewer(initial: StubConfig): CountingViewer {
  let links = initial.links ?? [];
  let comments = initial.comments ?? [];
  const cellRect = initial.cellRect ?? null;
  const counts = { link: 0, comment: 0, cell: 0 };
  const inside = (r: Rect, x: number, y: number) =>
    x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h;
  const viewer: CursorViewer = {
    linkRects: (_idx: number) => {
      counts.link++;
      return Promise.resolve(links.map((r) => ({ ...r, target: "url:https://x" })));
    },
    commentRects: (_idx: number) => {
      counts.comment++;
      return Promise.resolve(comments.map((r) => ({ ...r, ids: [1], color: "#abcdef" })));
    },
    cellAt: (_p: number, x: number, y: number) => {
      counts.cell++;
      return Promise.resolve(cellRect && inside(cellRect, x, y) ? { ref: "A1", raw: "1", display: "1" } : null);
    },
  };
  return { viewer, counts, setLinks: (r) => { links = r; }, setComments: (r) => { comments = r; } };
}

async function bootWith(viewer: CursorViewer, kind: DocumentKind | null = "word") {
  const kernel = createKernel();
  kernel.use(fakeLoaderPlugin(kind));
  kernel.use(cursorPlugin(viewer));
  await kernel.boot();
  return kernel;
}

describe("plugin-cursor per-generation rect cache", () => {
  it("HIT: a repeat resolve on the same page reuses cached rects (ZERO extra RPC)", async () => {
    const cv = countingViewer({ links: [{ x: 10, y: 10, w: 50, h: 20 }] });
    const kernel = await bootWith(cv.viewer);
    expect(await resolve(kernel, 20, 15)).toBe("pointer");
    expect(await resolve(kernel, 20, 15)).toBe("pointer");
    expect(cv.counts.link).toBe(1); // second resolve is a cache hit
  });

  it("MISS-per-page: each page fetches once, repeats hit (page0 x2, page1 x2 → 2 fetches)", async () => {
    const cv = countingViewer({ links: [{ x: 10, y: 10, w: 50, h: 20 }] });
    const kernel = await bootWith(cv.viewer);
    await kernel.exec("cursor.resolve", { page: 0, x: 20, y: 15 });
    await kernel.exec("cursor.resolve", { page: 0, x: 20, y: 15 });
    await kernel.exec("cursor.resolve", { page: 1, x: 20, y: 15 });
    await kernel.exec("cursor.resolve", { page: 1, x: 20, y: 15 });
    expect(cv.counts.link).toBe(2); // one fetch per distinct page, not per resolve
  });

  it("INVALIDATE on loader:load-started re-fetches and serves the NEW rects", async () => {
    const cv = countingViewer({ links: [{ x: 10, y: 10, w: 50, h: 20 }] });
    const kernel = await bootWith(cv.viewer);
    expect(await resolve(kernel, 20, 15)).toBe("pointer"); // cache fills (doc A)
    cv.setLinks([]); // doc B has no link here
    kernel.bus.emit("loader:load-started", 1);
    expect(await resolve(kernel, 20, 15)).toBe(""); // re-fetched → new (empty) rects
    expect(cv.counts.link).toBe(2);
  });

  it("INVALIDATE on loadComplete re-fetches (covers the refreshFonts re-layout path)", async () => {
    const cv = countingViewer({ links: [{ x: 10, y: 10, w: 50, h: 20 }] });
    const kernel = await bootWith(cv.viewer);
    expect(await resolve(kernel, 20, 15)).toBe("pointer");
    cv.setLinks([{ x: 200, y: 200, w: 50, h: 20 }]); // rects moved by the re-layout
    kernel.bus.emit("loadComplete", 2);
    expect(await resolve(kernel, 20, 15)).toBe(""); // old spot no longer a link
    expect(await resolve(kernel, 210, 205)).toBe("pointer"); // new spot is
    expect(cv.counts.link).toBe(2); // one re-fetch after the invalidation
  });

  it("is NOT invalidated by fonts:refreshed alone (loadComplete is the authoritative signal)", async () => {
    const cv = countingViewer({ links: [{ x: 10, y: 10, w: 50, h: 20 }] });
    const kernel = await bootWith(cv.viewer);
    expect(await resolve(kernel, 20, 15)).toBe("pointer"); // cache fills
    cv.setLinks([]); // would change IF re-fetched
    kernel.bus.emit("fonts:refreshed", { changed: true });
    expect(await resolve(kernel, 20, 15)).toBe("pointer"); // STILL cached — no re-fetch
    expect(cv.counts.link).toBe(1);
  });

  it("STALE-GUARD: a fetch resolving AFTER an invalidation never populates the new generation", async () => {
    let release!: (rs: Rect[]) => void;
    const gate = new Promise<Rect[]>((r) => { release = r; });
    let calls = 0;
    const viewer: CursorViewer = {
      linkRects: () => {
        calls++;
        if (calls === 1) return gate.then((rs) => rs.map((r) => ({ ...r, target: "url:x" })));
        return Promise.resolve([]);
      },
      commentRects: () => Promise.resolve([]),
      cellAt: () => Promise.resolve(null),
    };
    const kernel = await bootWith(viewer);
    // Resolve #1's linkRects fetch is now pending on `gate`.
    const p1 = kernel.exec("cursor.resolve", { page: 0, x: 20, y: 15 });
    // Invalidate mid-flight, THEN release the stale fetch with a HIT rect.
    kernel.bus.emit("loadComplete", 1);
    release([{ x: 10, y: 10, w: 50, h: 20 }]);
    await p1; // this in-flight call may use the fetched rects — acceptable
    // The stale result must NOT have been cached under the new generation:
    // a fresh resolve re-fetches (calls → 2) and returns [] → "".
    expect(await kernel.exec("cursor.resolve", { page: 0, x: 20, y: 15 })).toBe("");
    expect(calls).toBe(2);
  });

  it("cellAt STAYS UNCACHED — every resolve re-queries (position-dependent)", async () => {
    const cv = countingViewer({ cellRect: { x: 0, y: 0, w: 200, h: 200 } });
    const kernel = await bootWith(cv.viewer, "excel");
    await kernel.exec("cursor.resolve", { page: 0, x: 50, y: 50 });
    await kernel.exec("cursor.resolve", { page: 0, x: 60, y: 60 });
    expect(cv.counts.cell).toBe(2); // never cached
  });

  it("DECISION-UNCHANGED: cached (hit) resolve returns the IDENTICAL cursor as the uncached (miss)", async () => {
    const cv = countingViewer({
      links: [{ x: 10, y: 10, w: 50, h: 20 }],
      comments: [{ x: 100, y: 100, w: 40, h: 12 }],
    });
    const kernel = await bootWith(cv.viewer, "word");
    const uncached = await resolve(kernel, 20, 15); // miss (fetches)
    const cached = await resolve(kernel, 20, 15); // hit (reuses)
    expect(cached).toBe(uncached);
    expect(cached).toBe("pointer");
    expect(await resolve(kernel, 110, 105)).toBe("pointer"); // comment
    expect(await resolve(kernel, 300, 300)).toBe(""); // blank
  });
});

describe("plugin-cursor", () => {
  it("returns 'pointer' over a hyperlink rect", async () => {
    const kernel = await boot({ links: [{ x: 10, y: 10, w: 50, h: 20 }] });
    expect(await resolve(kernel, 20, 15)).toBe("pointer");
  });

  it("returns 'pointer' over a comment range (word doc)", async () => {
    const kernel = await boot({ comments: [{ x: 100, y: 100, w: 40, h: 12 }] }, "word");
    expect(await resolve(kernel, 110, 105)).toBe("pointer");
  });

  it("returns 'cell' over an excel cell (documentKind === 'excel')", async () => {
    const kernel = await boot({ cellRect: { x: 0, y: 0, w: 200, h: 200 } }, "excel");
    expect(await resolve(kernel, 50, 50)).toBe("cell");
  });

  it("returns '' (default) over an excel cell-less spot — no rects", async () => {
    const kernel = await boot({ cellRect: { x: 0, y: 0, w: 10, h: 10 } }, "excel");
    expect(await resolve(kernel, 500, 500)).toBe("");
  });

  it("does NOT show 'cell' in a word doc even if cellAt would hit (kind guard)", async () => {
    // Word doc: the excel-cell branch is gated on documentKind === "excel".
    const kernel = await boot({ cellRect: { x: 0, y: 0, w: 200, h: 200 } }, "word");
    expect(await resolve(kernel, 50, 50)).toBe("");
  });

  it("returns '' (NEVER an I-beam) over plain text — selection deferred", async () => {
    const kernel = await boot({}, "word");
    const c = await resolve(kernel, 300, 300);
    expect(c).toBe("");
    expect(c).not.toBe("text"); // explicit: no text-cursor false affordance ships
  });

  it("priority: link wins over a coincident comment AND excel cell", async () => {
    const kernel = await boot(
      {
        links: [{ x: 0, y: 0, w: 100, h: 100 }],
        comments: [{ x: 0, y: 0, w: 100, h: 100 }],
        cellRect: { x: 0, y: 0, w: 100, h: 100 },
      },
      "excel",
    );
    expect(await resolve(kernel, 50, 50)).toBe("pointer");
  });

  it("priority: comment wins over a coincident excel cell", async () => {
    const kernel = await boot(
      {
        comments: [{ x: 0, y: 0, w: 100, h: 100 }],
        cellRect: { x: 0, y: 0, w: 100, h: 100 },
      },
      "excel",
    );
    // comments only resolve for word in core; here the stub returns them and the
    // plugin's priority puts comment (rule 2) before excel-cell (rule 3).
    expect(await resolve(kernel, 50, 50)).toBe("pointer");
  });

  it("hand mode returns 'grab', overriding a coincident link", async () => {
    const kernel = await boot({ links: [{ x: 0, y: 0, w: 100, h: 100 }] }, "word");
    // Sanity: select mode still resolves the link to pointer.
    expect(await resolve(kernel, 50, 50)).toBe("pointer");
    // Enter hand mode → grab everywhere (over the link AND over blank).
    kernel.bus.emit("tool:mode-changed", { mode: "hand" });
    expect(await resolve(kernel, 50, 50)).toBe("grab");
    expect(await resolve(kernel, 500, 500)).toBe("grab");
    // Flip back to select → the select-mode results restore.
    kernel.bus.emit("tool:mode-changed", { mode: "select" });
    expect(await resolve(kernel, 50, 50)).toBe("pointer");
    expect(await resolve(kernel, 500, 500)).toBe("");
  });
});
