/**
 * OMML — the math-font routing in the resolver: "Cambria Math" (and
 * the engine-seeded "XITS Math") short-circuit to the XITS Math substitute
 * BEFORE the Latin font-map, and the plan serves the CORS-enabled, full-OTF
 * (MATH-table-bearing) URL for all four variants.
 */
import { describe, it, expect } from "vitest";
import { resolveSubstitute, buildFontPlan } from "./font-resolver";

describe("math font routing", () => {
  it("routes Cambria Math and named math faces to XITS Math", () => {
    expect(resolveSubstitute("Cambria Math")).toBe("XITS Math");
    expect(resolveSubstitute("XITS Math")).toBe("XITS Math");
    expect(resolveSubstitute("STIX Two Math")).toBe("XITS Math");
    expect(resolveSubstitute("Latin Modern Math")).toBe("XITS Math");
    // Normalisation: spacing/case irrelevant.
    expect(resolveSubstitute("cambriamath")).toBe("XITS Math");
  });

  it("does NOT route a plain body font to the math face", () => {
    expect(resolveSubstitute("Cambria")).not.toBe("XITS Math");
    expect(resolveSubstitute("Calibri")).not.toBe("XITS Math");
  });

  it("plans XITS Math with a full-OTF URL for all four variants", () => {
    // The engine seeds "XITS Math" into fontFamiliesUsed for any doc with math.
    const plan = buildFontPlan(["XITS Math"]);
    const entry = plan.get("XITS Math");
    expect(entry).toBeDefined();
    expect(entry!.family).toBe("XITS Math");
    expect(entry!.urls).toBeDefined();
    const u = entry!.urls!;
    // Full non-subset OTF (the MATH table survives) — never a subset woff2.
    expect(u.regular).toMatch(/XITSMath-Regular\.otf$/);
    expect(u.regular).not.toMatch(/woff2?$/);
    // A single upright master → all four variants map to Regular.
    expect(u.bold).toBe(u.regular);
    expect(u.italic).toBe(u.regular);
    expect(u.boldItalic).toBe(u.regular);
  });

  it("a doc declaring Cambria Math plans the XITS Math substitute", () => {
    const plan = buildFontPlan(["Cambria Math"]);
    const entry = plan.get("Cambria Math");
    expect(entry!.family).toBe("XITS Math");
    expect(entry!.urls!.regular).toMatch(/XITSMath-Regular\.otf$/);
  });

  it("excludes the math face from local-candidate tagging", () => {
    const plan = buildFontPlan(["XITS Math"]);
    expect(plan.get("XITS Math")!.localCandidates).toBeUndefined();
  });
});
