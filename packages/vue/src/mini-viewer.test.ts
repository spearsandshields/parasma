// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createApp, nextTick } from "vue";

/**
 * The acceptance artifact's proof: the mini viewer MOUNTS, WIRES and stays
 * SMALL. The line-count assertion is the package's measured target: a full
 * BYO-chrome integration within a 70-line ceiling.
 */

type Handler = (...a: unknown[]) => void;
function makeFakeHandle() {
  const on = new Map<string, Set<Handler>>();
  const bus = new Map<string, Set<Handler>>();
  const cellCbs = new Set<Handler>();
  const execs: unknown[][] = [];
  const h = {
    state: { documentKind: "excel", currentPage: 1, pageCount: 5, zoom: 1 },
    on(ev: string, cb: Handler) {
      if (!on.has(ev)) on.set(ev, new Set());
      on.get(ev)!.add(cb);
      return () => on.get(ev)!.delete(cb);
    },
    kernel: {
      bus: {
        subscribe(ev: string, cb: Handler) {
          if (!bus.has(ev)) bus.set(ev, new Set());
          bus.get(ev)!.add(cb);
          return () => bus.get(ev)!.delete(cb);
        },
      },
      exec: (...a: unknown[]) => {
        execs.push(a);
        return Promise.resolve();
      },
    },
    dom: { onCellSelect: (cb: Handler) => (cellCbs.add(cb), () => {}) },
    zoom: { in: vi.fn(), out: vi.fn(), set: vi.fn(), fit: vi.fn() },
    goToPage: vi.fn(),
    selectSheet: vi.fn(),
    load: vi.fn(async () => {}),
    unload: vi.fn(async () => {}),
    destroy: vi.fn(async () => {}),
  };
  return {
    h,
    execs,
    emit: (ev: string, ...a: unknown[]) => on.get(ev)?.forEach((cb) => cb(...a)),
    busEmit: (ev: string, ...a: unknown[]) => bus.get(ev)?.forEach((cb) => cb(...a)),
    cellEmit: (c: unknown) => on.get("cellSelect")?.forEach((cb) => cb(c)),
  };
}
let fake = makeFakeHandle();
vi.mock("@parasma/snippet/headless", async () => ({
  // The real session (framework-neutral, imports only the mocked core).
  createViewerSession: (await import("../../snippet/src/viewer-session")).createViewerSession,
  createHeadlessViewer: vi.fn(async () => fake.h),
}));
vi.mock("@parasma/core", () => ({
  sniffKind: () => "word",
  UnsupportedFormatError: class extends Error {},
}));

import { MiniViewer } from "../examples/mini-viewer";
import miniSource from "../examples/mini-viewer.ts?raw";

const flush = async (n = 6) => {
  for (let i = 0; i < n; i++) await Promise.resolve();
  await nextTick();
};

beforeEach(() => {
  fake = makeFakeHandle();
});

describe("mini-viewer (the acceptance artifact)", () => {
  it("stays within the measured burden target: ≤ 70 integration lines (comments/blanks excluded)", () => {
    const lines = miniSource
      .replace(/\/\*[\s\S]*?\*\//g, "") // block comments
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l.length > 0 && !l.startsWith("//") && !l.startsWith("*"));
    expect(lines.length).toBeLessThanOrEqual(70);
  });

  it("mounts, wires the sibling formula bar, commands, search and truncation notice", async () => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    const app = createApp(MiniViewer);
    app.mount(host);
    await flush();

    // Pre-load (non-excel): page navigation renders and drives p.commands.
    let buttons = [...host.querySelectorAll("button")];
    buttons.find((b) => b.textContent === "›")!.click();
    expect(fake.h.goToPage).toHaveBeenCalledWith(2);
    buttons.find((b) => b.textContent === "+")!.click();
    expect(fake.h.zoom.in).toHaveBeenCalled();

    // Sibling formula bar (excel kind + a cell selection) — OUTSIDE the stage.
    fake.emit("load", { pageSize: { widthPx: 816, heightPx: 1056 }, documentKind: "excel" });
    fake.cellEmit({ ref: "B2", raw: "", display: "42" });
    await flush();
    const bar = host.querySelector(".mini-formula-bar");
    expect(bar?.textContent).toContain("B2");
    expect(bar?.textContent).toContain("42");
    expect(host.querySelector(".mini-stage")?.contains(bar!)).toBe(false);

    // Excel: page nav is REPLACED by sheet tabs (the grid view is continuous
    // — pages don't exist there); a tab click drives selectSheet.
    fake.busEmit("sheets:changed", {
      sheets: [{ name: "Overview", pageCount: 1 }, { name: "Data", pageCount: 1 }],
      active: 0,
    });
    await flush();
    buttons = [...host.querySelectorAll("button")];
    expect(buttons.find((b) => b.textContent === "›")).toBeUndefined();
    const tabs = [...host.querySelectorAll(".mini-tab")];
    expect(tabs.map((t) => t.textContent)).toEqual(["Overview", "Data"]);
    expect(tabs[0].classList.contains("active")).toBe(true);
    (tabs[1] as HTMLElement).click();
    expect(fake.h.selectSheet).toHaveBeenCalledWith(1);

    // Search input → kernel command (default reveal handles the rest).
    const search = host.querySelector<HTMLInputElement>("input[placeholder=search]")!;
    search.value = "foo";
    search.dispatchEvent(new Event("input"));
    await flush();
    expect(fake.execs).toContainEqual(["search.query", "foo"]);

    // Counter renders from p.search; ↑/↓ drive next/prev through commands.
    fake.busEmit("search:changed", { query: "foo", results: [{}, {}], activeIdx: 0, status: "done" });
    await flush();
    const toolbar = host.querySelector(".mini-toolbar")!;
    expect(toolbar.textContent).toContain("1 / 2");
    [...host.querySelectorAll("button")].find((b) => b.textContent === "↓")!.click();
    expect(fake.execs).toContainEqual(["search.next"]);

    // Truncation notice appears from the guardrail event.
    fake.emit("layoutTruncated", 812);
    await flush();
    expect(host.querySelector(".mini-notice")?.textContent).toContain("812");
    app.unmount();
  });

  it("the ENGINE'S injected DOM survives re-renders (stage must never carry vnode children)", async () => {
    // The regression this guards: an idle hint rendered INSIDE the stage
    // ref; Vue reconciled the stage's children on the next status flip and
    // clobbered the engine's injected DOM — a loaded document went blank.
    const host = document.createElement("div");
    document.body.appendChild(host);
    const app = createApp(MiniViewer);
    app.mount(host);
    await flush();

    // Simulate the engine mounting its stage (what attachDom does at boot).
    const stage = host.querySelector(".mini-stage")!;
    const engineDom = document.createElement("div");
    engineDom.className = "ps-viewer-injected";
    stage.appendChild(engineDom);

    // Drive status through idle→loading→ready→truncated — each flips vnodes.
    fake.emit("load", { pageSize: { widthPx: 816, heightPx: 1056 }, documentKind: "word" });
    fake.emit("pageChange", 2);
    fake.emit("layoutTruncated", 5);
    await flush();
    expect(stage.contains(engineDom)).toBe(true); // RED with children-in-stage
    app.unmount();
  });
});
