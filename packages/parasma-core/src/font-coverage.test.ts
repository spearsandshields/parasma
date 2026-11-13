/**
 * Coverage detector — a PURE function over provenance + catalog/probe
 * facts.
 */
import { describe, it, expect } from "vitest";
import { computeFontCoverage } from "./font-coverage";

const base = {
  declared: "Times New Roman",
  resolved: "Tinos" as string | null,
  origin: null as "local" | "cdn" | "failed" | null,
  matchTier: null as import("./local-font-provider").MatchTier | null,
  matchedFamily: null as string | null,
  isCjk: false,
  candidateInstalled: null as boolean | null,
};

describe("computeFontCoverage", () => {
  it("exact-local: a ladder tier hit rendering local bytes", () => {
    expect(
      computeFontCoverage({ ...base, origin: "local", matchTier: 1, matchedFamily: "Times New Roman" }),
    ).toBe("exact-local");
    expect(
      computeFontCoverage({
        ...base,
        declared: "Avenir Book",
        origin: "local",
        matchTier: 2,
        matchedFamily: "Avenir Book",
      }),
    ).toBe("exact-local");
  });

  it("exact-local via the dev route only when the matched family IS the declared name", () => {
    expect(
      computeFontCoverage({
        ...base,
        origin: "local",
        matchTier: "dev-route",
        matchedFamily: "Times New Roman",
      }),
    ).toBe("exact-local");
    // Dev route served the CURATED candidate (e.g. Verdana for DejaVu Sans).
    expect(
      computeFontCoverage({
        ...base,
        declared: "DejaVu Sans",
        origin: "local",
        matchTier: "dev-route",
        matchedFamily: "Verdana",
      }),
    ).toBe("substituted-local");
  });

  it("substituted-local: curated candidate's local bytes rendering", () => {
    expect(
      computeFontCoverage({
        ...base,
        declared: "DejaVu Sans",
        origin: "local",
        matchTier: "curated",
        matchedFamily: "Verdana",
      }),
    ).toBe("substituted-local");
  });

  it("substituted-local-available: candidate installed but NOT rendering (the benefit case)", () => {
    expect(computeFontCoverage({ ...base, origin: "cdn", candidateInstalled: true })).toBe(
      "substituted-local-available",
    );
    // Even an engine-default family counts when a local candidate exists.
    expect(
      computeFontCoverage({ ...base, resolved: null, origin: null, candidateInstalled: true }),
    ).toBe("substituted-local-available");
  });

  it("substituted: CDN substitute, nothing local detected (or unknown)", () => {
    expect(computeFontCoverage({ ...base, origin: "cdn", candidateInstalled: false })).toBe("substituted");
    expect(computeFontCoverage({ ...base, origin: "cdn", candidateInstalled: null })).toBe("substituted");
  });

  it("missing: engine default (no substitute / failed load), nothing local", () => {
    expect(computeFontCoverage({ ...base, resolved: null, candidateInstalled: false })).toBe("missing");
    expect(computeFontCoverage({ ...base, origin: "failed", candidateInstalled: false })).toBe("missing");
  });

  it("cjk exclusion wins over everything (v1 — a follow-on)", () => {
    expect(
      computeFontCoverage({ ...base, isCjk: true, origin: "cdn", candidateInstalled: true }),
    ).toBe("cjk (bundled Noto)");
  });
});
