import { describe, it, expect, vi } from "vitest";
import { LocalFontProvider } from "./local-font-provider";

const fakeFace = (postscriptName: string, style = "normal") => ({
  family: "Verdana",
  fullName: "Verdana",
  postscriptName,
  style,
  blob: async () => new Blob([new Uint8Array([1, 2, 3, 4])]),
});

describe("LocalFontProvider", () => {
  it("returns null when the Font Access API is absent", async () => {
    const p = new LocalFontProvider({});
    expect(await p.read(["Verdana"], "400", "normal")).toBeNull();
  });

  it("returns null without user activation", async () => {
    const p = new LocalFontProvider({
      queryLocalFonts: async () => [fakeFace("Verdana")],
      hasUserActivation: () => false,
    });
    expect(await p.read(["Verdana"], "400", "normal")).toBeNull();
  });

  it("returns bytes + postscriptName on the happy path", async () => {
    const p = new LocalFontProvider({
      queryLocalFonts: async () => [fakeFace("Verdana"), fakeFace("Verdana-Bold", "bold")],
      hasUserActivation: () => true,
    });
    const r = await p.read(["Verdana"], "400", "normal");
    expect(r).not.toBeNull();
    expect(r!.postscriptName).toBe("Verdana");
    expect(new Uint8Array(r!.bytes)).toEqual(new Uint8Array([1, 2, 3, 4]));
  });

  it("picks the bold face when bold weight is requested", async () => {
    const p = new LocalFontProvider({
      queryLocalFonts: async () => [fakeFace("Verdana", "normal"), fakeFace("Verdana-Bold", "bold")],
      hasUserActivation: () => true,
    });
    const r = await p.read(["Verdana"], "700", "normal");
    expect(r!.postscriptName).toBe("Verdana-Bold");
  });

  it("returns null when no candidate family is installed", async () => {
    const p = new LocalFontProvider({
      queryLocalFonts: async () => [fakeFace("Verdana")],
      hasUserActivation: () => true,
    });
    expect(await p.read(["NotInstalled"], "400", "normal")).toBeNull();
  });

  it("swallows queryLocalFonts errors (SecurityError/denied) and returns null", async () => {
    const p = new LocalFontProvider({
      queryLocalFonts: async () => {
        throw new DOMException("denied", "SecurityError");
      },
      hasUserActivation: () => true,
    });
    expect(await p.read(["Verdana"], "400", "normal")).toBeNull();
  });
});

// Dev/automation byte-source: an injected fetchLocalFont (e.g. a dev-server route that
// serves the OS-installed font) is used INSTEAD of the gesture-gated Font Access
// path — no user activation required. Same read() contract.
describe("LocalFontProvider — fetchLocalFont (dev/automation source)", () => {
  it("uses fetchLocalFont with NO user activation, returning bytes + family as psName", async () => {
    const p = new LocalFontProvider({
      fetchLocalFont: async () => new Uint8Array([5, 6, 7]).buffer,
      hasUserActivation: () => false, // would block the Font Access path
    });
    const r = await p.read(["Verdana"], "700", "italic");
    expect(r).not.toBeNull();
    expect(r!.postscriptName).toBe("Verdana");
    expect(new Uint8Array(r!.bytes)).toEqual(new Uint8Array([5, 6, 7]));
  });

  it("passes parsed weight + italic to fetchLocalFont", async () => {
    const calls: Array<[string, number, boolean]> = [];
    const p = new LocalFontProvider({
      fetchLocalFont: async (family, weight, italic) => {
        calls.push([family, weight, italic]);
        return new Uint8Array([1]).buffer;
      },
    });
    await p.read(["Verdana"], "700", "italic");
    expect(calls).toEqual([["Verdana", 700, true]]);
  });

  it("falls through candidates until one returns bytes", async () => {
    const p = new LocalFontProvider({
      fetchLocalFont: async (family) => (family === "Verdana" ? new Uint8Array([9]).buffer : null),
    });
    const r = await p.read(["NotInstalled", "Verdana"], "400", "normal");
    expect(new Uint8Array(r!.bytes)).toEqual(new Uint8Array([9]));
  });

  it("returns null when fetchLocalFont misses every candidate", async () => {
    const p = new LocalFontProvider({ fetchLocalFont: async () => null });
    expect(await p.read(["Verdana"], "400", "normal")).toBeNull();
  });

  it("swallows a throwing fetchLocalFont and tries the next candidate", async () => {
    const p = new LocalFontProvider({
      fetchLocalFont: async (family) => {
        if (family === "Boom") throw new Error("route 500");
        return new Uint8Array([2]).buffer;
      },
    });
    const r = await p.read(["Boom", "Verdana"], "400", "normal");
    expect(new Uint8Array(r!.bytes)).toEqual(new Uint8Array([2]));
  });

  it("reports supported=true when only fetchLocalFont is injected", () => {
    expect(new LocalFontProvider({ fetchLocalFont: async () => null }).supported).toBe(true);
    expect(new LocalFontProvider({}).supported).toBe(false);
  });
});
