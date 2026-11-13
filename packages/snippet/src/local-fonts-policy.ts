/**
 * Pure policy for the one-click local-fonts flow — extracted from mount.ts so
 * the rules are unit-testable (while inline, the three CTA conjuncts and the
 * storage precedence had no test, and dropping any one conjunct passed every
 * gate).
 *
 * Contract:
 *  - An EXPLICIT mount option always wins — including false (integrator
 *    opt-out). Persistence is consulted only when the option is undefined.
 *  - Persistence records user gestures only (the CTA click, after a
 *    successful grant) — never written on other paths.
 */

/** Coverage states that mean "local fonts could improve this family". */
const CTA_COVERAGE = new Set(["substituted", "substituted-local-available", "missing"]);

export interface CtaFamily {
  coverage: string;
  localCandidates: string[];
}

/**
 * Mount-time resolution of the effective useLocalFonts flag:
 * explicit option (any boolean) wins; else the persisted one-click choice.
 */
export function resolveLocalFontsEnabled(
  option: boolean | undefined,
  stored: string | null,
): boolean {
  if (option !== undefined) return option;
  return stored === "1";
}

/**
 * The CTA visibility rule — ALL conjuncts required:
 *  1. not already enabled, and NOT explicitly configured (either way) by the
 *     integrator (the CTA participates only in persistence-driven mode);
 *  2. permission state is exactly "prompt" (granted → automatic, denied /
 *     unsupported / policy-blocked → nothing actionable);
 *  3. some family's coverage is improvable AND one of its ladder/curated
 *     candidates is CSS-visible per the canvas probe (best effort —
 *     Office-cache faces are an accepted false-negative).
 */
export function ctaShouldShow(input: {
  enabled: boolean;
  explicit: boolean;
  permission: string;
  families: CtaFamily[];
  probeInstalled: (family: string) => boolean | null;
}): boolean {
  if (input.enabled || input.explicit) return false;
  if (input.permission !== "prompt") return false;
  return input.families.some(
    (f) =>
      CTA_COVERAGE.has(f.coverage) &&
      f.localCandidates.some((c) => input.probeInstalled(c) === true),
  );
}
