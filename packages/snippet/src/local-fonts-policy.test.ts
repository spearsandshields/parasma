/**
 * One-click local-fonts policy — pins the rules the mount wiring consumes.
 * Red-conditions:
 *  - storage precedence: mutating resolveLocalFontsEnabled to storage-wins (or
 *    inverting the undefined check) fails the (false,"1")→false / (true,"0")→
 *    true cases;
 *  - each CTA conjunct: dropping `enabled`/`explicit`, the permission check,
 *    the coverage filter, or the probe conjunct fails its dedicated case.
 */
import { describe, it, expect } from "vitest";
import { ctaShouldShow, resolveLocalFontsEnabled } from "./local-fonts-policy";

describe("resolveLocalFontsEnabled — explicit option ALWAYS wins", () => {
  it("explicit true beats stored '0'", () => {
    expect(resolveLocalFontsEnabled(true, "0")).toBe(true);
  });
  it("explicit false beats stored '1' (integrator opt-out)", () => {
    expect(resolveLocalFontsEnabled(false, "1")).toBe(false);
  });
  it("undefined option falls back to the persisted gesture", () => {
    expect(resolveLocalFontsEnabled(undefined, "1")).toBe(true);
  });
  it("undefined option + no/other stored value stays off", () => {
    expect(resolveLocalFontsEnabled(undefined, null)).toBe(false);
    expect(resolveLocalFontsEnabled(undefined, "0")).toBe(false);
  });
});

describe("ctaShouldShow — all conjuncts required", () => {
  const improvable = { coverage: "substituted", localCandidates: ["Times New Roman"] };
  const base = {
    enabled: false,
    explicit: false,
    permission: "prompt",
    families: [improvable],
    probeInstalled: (f: string) => f === "Times New Roman",
  };

  it("shows when every conjunct holds", () => {
    expect(ctaShouldShow(base)).toBe(true);
  });
  it("hidden when already enabled", () => {
    expect(ctaShouldShow({ ...base, enabled: true })).toBe(false);
  });
  it("hidden when the integrator configured useLocalFonts EXPLICITLY (either way)", () => {
    expect(ctaShouldShow({ ...base, explicit: true })).toBe(false);
  });
  it("hidden outside the prompt state (granted is automatic, denied is dead)", () => {
    for (const p of ["granted", "denied", "unsupported"]) {
      expect(ctaShouldShow({ ...base, permission: p })).toBe(false);
    }
  });
  it("hidden when no family is improvable (all exact-local / cjk)", () => {
    expect(
      ctaShouldShow({
        ...base,
        families: [
          { coverage: "exact-local", localCandidates: ["Times New Roman"] },
          { coverage: "cjk (bundled Noto)", localCandidates: [] },
        ],
      }),
    ).toBe(false);
  });
  it("hidden when no candidate is probe-visible (Office-cache false-negative accepted)", () => {
    expect(ctaShouldShow({ ...base, probeInstalled: () => false })).toBe(false);
    expect(ctaShouldShow({ ...base, probeInstalled: () => null })).toBe(false);
  });
  it("substituted-local-available and missing also qualify as improvable", () => {
    for (const coverage of ["substituted-local-available", "missing"]) {
      expect(ctaShouldShow({ ...base, families: [{ ...improvable, coverage }] })).toBe(true);
    }
  });
});
