/**
 * #applyConfig font sugar, exercised through Viewer.create().
 * Config is pure orchestration over the shipped setters
 * (setFontUrlResolver/setLocalFontSource/setLocalFontsEnabled); this locks the
 * precedence (fontResolver > fontBaseUrl > assetBaseUrl) and the {} invariant
 * (NONE of the three setters called with no font config).
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

const { initSpy } = vi.hoisted(() => ({
  initSpy: vi.fn((..._a: unknown[]) => Promise.resolve({})),
}));

vi.mock("./wasm/parasma_wasm.js", () => {
  class Viewer {
    free(): void {}
    debugLayout(): void {}
  }
  return { default: initSpy, Viewer, parserInfo: () => ({}) };
});

import { Viewer, type FontUrlInfo } from "./index";

function spyFontSetters() {
  const resolver = vi.spyOn(Viewer.prototype, "setFontUrlResolver").mockImplementation(() => {});
  const source = vi.spyOn(Viewer.prototype, "setLocalFontSource").mockImplementation(() => {});
  const enabled = vi.spyOn(Viewer.prototype, "setLocalFontsEnabled").mockImplementation(() => {});
  return { resolver, source, enabled };
}

const JSDELIVR_URL: FontUrlInfo = {
  family: "Carlito",
  variant: "regular",
  weight: 400,
  italic: false,
  defaultUrl: "https://cdn.jsdelivr.net/gh/googlefonts/carlito@main/fonts/ttf/Carlito-Regular.ttf",
  documentFamily: "Calibri",
};

beforeEach(() => {
  initSpy.mockClear();
});
afterEach(() => {
  vi.restoreAllMocks();
});

describe("#applyConfig font sugar", () => {
  it("{} calls NONE of the three font setters (RED-invariant)", async () => {
    const { resolver, source, enabled } = spyFontSetters();
    await Viewer.create({});
    expect(resolver).not.toHaveBeenCalled();
    expect(source).not.toHaveBeenCalled();
    expect(enabled).not.toHaveBeenCalled();
  });

  it("fontBaseUrl => an origin-swap resolver that rewrites the jsDelivr origin", async () => {
    const { resolver } = spyFontSetters();
    await Viewer.create({ fontBaseUrl: "https://cdn.me" });
    expect(resolver).toHaveBeenCalledTimes(1);
    const fn = resolver.mock.calls[0]![0] as (i: FontUrlInfo) => string;
    expect(fn(JSDELIVR_URL)).toBe(
      "https://cdn.me/gh/googlefonts/carlito@main/fonts/ttf/Carlito-Regular.ttf",
    );
  });

  it("fontResolver wins over fontBaseUrl and is passed verbatim", async () => {
    const { resolver } = spyFontSetters();
    const fn = (i: FontUrlInfo) => `custom://${i.family}`;
    await Viewer.create({ fontResolver: fn, fontBaseUrl: "https://cdn.me" });
    expect(resolver).toHaveBeenCalledTimes(1);
    expect(resolver.mock.calls[0]![0]).toBe(fn);
  });

  it("assetBaseUrl is used for fonts ONLY when neither fontResolver nor fontBaseUrl present", async () => {
    const { resolver } = spyFontSetters();
    await Viewer.create({ assetBaseUrl: "https://cdn.me/assets" });
    expect(resolver).toHaveBeenCalledTimes(1);
    const fn = resolver.mock.calls[0]![0] as (i: FontUrlInfo) => string;
    expect(fn(JSDELIVR_URL)).toBe(
      "https://cdn.me/assets/gh/googlefonts/carlito@main/fonts/ttf/Carlito-Regular.ttf",
    );
  });

  it("localFontSource + useLocalFonts thread to their setters", async () => {
    const { source, enabled } = spyFontSetters();
    const src = { fetchLocalFont: () => Promise.resolve(null) };
    await Viewer.create({ localFontSource: src, useLocalFonts: true });
    expect(source).toHaveBeenCalledTimes(1);
    expect(source.mock.calls[0]![0]).toBe(src);
    expect(enabled).toHaveBeenCalledTimes(1);
    expect(enabled.mock.calls[0]![0]).toBe(true);
  });
});
