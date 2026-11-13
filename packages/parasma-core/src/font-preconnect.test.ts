/**
 * Font-origin `preconnect` warm-up.
 *
 * `#doLoad` awaits every font byte before `runLayout`, so the DNS + TCP + TLS
 * handshake to the font host sits directly in front of first paint. Emitting a
 * `<link rel="preconnect">` at CONSTRUCTION moves that handshake into whatever
 * the app does before `load()`. Browser-measured against jsDelivr (Chrome 149,
 * fresh profile per sample, N=3 cold/warm pairs): connection setup 68-86 ms → 0,
 * first-font TTFB 114-137 ms → 37-47 ms.
 *
 * The hint transfers no bytes and cannot change which faces load, so nothing
 * here is fidelity-bearing. What IS load-bearing is the origin it names: a
 * consumer serving fonts themselves must be warmed at THEIR host, and a
 * consumer with an arbitrary resolver must be warmed nowhere at all rather than
 * handed a handshake to a CDN they never touch. Those are the tests below.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

const H = vi.hoisted(() => ({ initSpy: vi.fn(() => Promise.resolve({})) }));

// Minimal wasm stand-in: `create()` must reach `#applyConfig` without a real
// engine. No load happens in this file, so only construction is modelled.
vi.mock("./wasm/parasma_wasm.js", () => {
  class Viewer {
    free(): void {}
    debugLayout(): void {}
  }
  return { default: H.initSpy, Viewer, parserInfo: () => ({}) };
});

import { Viewer } from "./index";
import { BUNDLED_FONT_ORIGINS } from "./font-resolver";

interface FakeLink {
  rel: string;
  href: string;
  crossOrigin: string | null;
}

let links: FakeLink[];

/** A `document` with just the surface `#warmFontOrigins` touches. */
function fakeDocument(): unknown {
  return {
    baseURI: "https://app.example/viewer/",
    createElement: (): FakeLink => ({ rel: "", href: "", crossOrigin: null }),
    head: {
      appendChild: (l: FakeLink) => links.push(l),
      querySelectorAll: (sel: string) =>
        sel === 'link[rel="preconnect"]' ? links.filter((l) => l.rel === "preconnect") : [],
    },
  };
}

const origins = (): string[] => links.filter((l) => l.rel === "preconnect").map((l) => l.href);

beforeEach(() => {
  links = [];
  vi.stubGlobal("document", fakeDocument());
});
afterEach(() => {
  vi.unstubAllGlobals();
});

describe("font-origin preconnect", () => {
  it("warms the BUNDLED origin when no config is supplied", async () => {
    await Viewer.create();

    // Derived from FONT_URLS, not restated — one origin (jsDelivr) today.
    expect(origins()).toEqual([...BUNDLED_FONT_ORIGINS]);
    expect(links.map((l) => l.crossOrigin)).toEqual(links.map(() => "anonymous"));
  });

  it("warms the CONSUMER's origin — not jsDelivr — when fontBaseUrl is set", async () => {
    await Viewer.create({ fontBaseUrl: "https://fonts.acme.test/parasma" });

    expect(origins()).toEqual(["https://fonts.acme.test"]);
    expect(origins()).not.toContain("https://cdn.jsdelivr.net");
  });

  it("follows assetBaseUrl when no fontBaseUrl is given", async () => {
    await Viewer.create({ assetBaseUrl: "https://assets.acme.test/v2/" });

    expect(origins()).toEqual(["https://assets.acme.test"]);
  });

  it("warms NOTHING when a full fontResolver is installed (origin unknowable)", async () => {
    await Viewer.create({ fontResolver: (i) => `https://who.knows/${i.family}` });

    expect(origins()).toEqual([]);
  });

  it("skips a SAME-ORIGIN base — that connection is already open", async () => {
    vi.stubGlobal("location", { origin: "https://app.example" });

    await Viewer.create({ assetBaseUrl: "/assets/" });

    expect(origins()).toEqual([]);
  });

  it("emits ONE node per origin however many viewers mount", async () => {
    await Viewer.create();
    await Viewer.create();
    await Viewer.create({ fontBaseUrl: "https://fonts.acme.test" });

    expect(origins()).toEqual([...BUNDLED_FONT_ORIGINS, "https://fonts.acme.test"]);
  });

  it("is inert without a document (SSR / worker scope)", async () => {
    vi.stubGlobal("document", undefined);

    await expect(Viewer.create()).resolves.toBeInstanceOf(Viewer);
    expect(links).toEqual([]);
  });
});
