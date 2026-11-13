/**
 * EMF raster resolution policy — the PURE decision core of the EMF raster
 * cache (no timers, no debounce: paint-driven only). Each paint of an EMF
 * reports its requested device-pixel footprint (drawn CSS px × zoom × dpr);
 * {@link planEmfRaster} decides whether that paint warrants a (re-)raster.
 *
 * Invariants the rules encode:
 *  - downscale never re-rasters (blit-scale-down is always crisp; the cache
 *    never shrinks);
 *  - upgrades carry ×{@link EMF_UPGRADE_FACTOR} headroom so a settled zoom
 *    rarely needs a second pass, and the SAME factor is the hysteresis band
 *    (≤1.5× soft upscale tolerated — no thrash);
 *  - per-raster pixel budget {@link EMF_PIXEL_BUDGET} (aspect-preserving
 *    clamp); at the wall the policy degrades to soft gracefully and NEVER
 *    re-rasters (with float-jitter slack, see {@link EMF_WALL_SLACK_PX});
 *  - one raster in flight per key (`inFlightPxW` dedup).
 *
 * Runs once per EMF draw per replayed frame — keep it allocation-free on the
 * common (entry-exists, no-upgrade) path: pure comparisons over the mutable
 * sidecar entry; the `{pxW,pxH}` result allocates only when a raster is
 * actually planned.
 */

/** Per-raster pixel budget (~16MP). Per-PIC, not global — a doc with many
 *  distinct EMFs can sum above this (global-budget eviction is a follow-on). */
export const EMF_PIXEL_BUDGET = 16 * 1024 * 1024;

/** Raster headroom AND hysteresis band (fixed decision: 1.5×). */
export const EMF_UPGRADE_FACTOR = 1.5;

/**
 * Slack (device px) for the at-the-budget-wall comparison: `ceil(dw·zoom·dpr·
 * 1.5)` jitters ±1px across paints from float noise, and at the wall every
 * paint enters the upgrade rule — an EXACT `>=` would re-raster a 16MP bitmap
 * repeatedly. Two px of slack absorbs the jitter.
 */
export const EMF_WALL_SLACK_PX = 2;

/**
 * The Viewer's per-key policy sidecar entry (`#emfRaster`), keyed by the
 * synthetic `"emf-pic:{rel}"`. MUTATED in place by the raster executor (never
 * re-created per plan — {@link planEmfRaster} runs on the hot replay loop).
 */
export interface EmfRasterEntry {
  /** Device-pixel size of the bitmap currently registered under the key. */
  pxW: number;
  pxH: number;
  /** Whether that bitmap was rastered with every DIB blit decoded (an
   *  incomplete raster baked grey blit placeholders; re-rastered on blit
   *  arrival via `imageReady`, in-process only — worker rasters are complete
   *  by construction). */
  complete: boolean;
  /** Width of the raster currently in flight, 0 when idle (dedup guard). */
  inFlightPxW: number;
  /** Set when a DIB blit arrived WHILE a raster was in flight — the landing
   *  consumes it with one follow-up re-raster (the arrival itself cannot,
   *  the in-flight dedup would swallow it). */
  pendingReraster: boolean;
}

/** Aspect-preserving clamp of a planned raster to {@link EMF_PIXEL_BUDGET}. */
export function budgetClamp(pxW: number, pxH: number): { pxW: number; pxH: number } {
  const area = pxW * pxH;
  if (area <= EMF_PIXEL_BUDGET) return { pxW, pxH };
  const s = Math.sqrt(EMF_PIXEL_BUDGET / area);
  return { pxW: Math.max(1, Math.floor(pxW * s)), pxH: Math.max(1, Math.floor(pxH * s)) };
}

/**
 * Decide whether one painted request `(reqPxW, reqPxH)` (device px) warrants
 * a raster for `entry` (null/undefined = never rastered). Returns the planned
 * raster size, or `null` for "keep blitting what's registered" (which may be
 * nothing yet if a first raster is in flight — the replay draws the standard
 * placeholder for that beat).
 */
export function planEmfRaster(
  entry: EmfRasterEntry | null | undefined,
  reqPxW: number,
  reqPxH: number,
): { pxW: number; pxH: number } | null {
  // NaN/degenerate rejection (`!(x > 0)` admits-NaN-as-false, the engine-side
  // guard pattern): a zero/NaN request plans nothing.
  if (!(reqPxW > 0) || !(reqPxH > 0)) return null;
  if (entry) {
    // (3) pure blit-scale-down — always crisp; the cache never shrinks.
    if (reqPxW <= entry.pxW) return null;
    // (4) hysteresis band — tolerate ≤1.5× soft upscale, no thrash.
    if (reqPxW <= entry.pxW * EMF_UPGRADE_FACTOR) return null;
  }
  // (1) target = req × 1.5 headroom (ALSO on the first raster), budget-clamped.
  const target = budgetClamp(Math.ceil(reqPxW * EMF_UPGRADE_FACTOR), Math.ceil(reqPxH * EMF_UPGRADE_FACTOR));
  if (!entry) return target; // (2) first need
  // (5a) at the wall: the cached bitmap already IS the clamped target (± float
  // jitter slack) — degrade to soft gracefully, never re-raster in a loop.
  if (entry.pxW >= target.pxW - EMF_WALL_SLACK_PX) return null;
  // (5b) dedup — an equal-or-larger raster is already in flight.
  if (entry.inFlightPxW >= target.pxW) return null;
  return target;
}
