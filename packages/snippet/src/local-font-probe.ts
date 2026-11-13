/**
 * Permissionless installed-locally probe (canvas measure): a family exists
 * when BOTH "family, monospace" and "family, serif" stacks resolve to the
 * same width AND at least one differs from its bare fallback. NOTE:
 * document.fonts.check() is NON-discriminating in Chrome (true for fake
 * families — empirically refuted 2026-07-10) — do not swap this for it.
 * Sees CSS-visible fonts only: Office FontCache faces (Segoe UI, Calibri on
 * mac) read false here even though the dev /__local-fonts route finds them —
 * a known, accepted false-negative (it only gates the one-click affordance).
 *
 * Shared by the dev FONTS tab (the "installed?" column / no-catalog coverage
 * fallback) and the one-click affordance's visibility rule in mount.ts.
 * Results are cached per family for the probe's lifetime.
 */
export function createInstalledLocallyProbe(): (family: string) => boolean | null {
  const ctx = document.createElement("canvas").getContext("2d");
  const cache = new Map<string, boolean>();
  return (family: string): boolean | null => {
    if (!ctx) return null;
    const hit = cache.get(family);
    if (hit !== undefined) return hit;
    const S = "mmmmmmmmmmlliIL";
    const w = (f: string) => {
      ctx.font = `64px ${f}`;
      return ctx.measureText(S).width;
    };
    const q = `"${family.replace(/"/g, "")}"`;
    const out =
      (w(`${q}, monospace`) !== w("monospace") || w(`${q}, serif`) !== w("serif")) &&
      w(`${q}, monospace`) === w(`${q}, serif`);
    cache.set(family, out);
    return out;
  };
}
