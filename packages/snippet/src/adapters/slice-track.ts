/**
 * Gesture-time slice content tracking (zoom-tremor fix).
 *
 * During a pinch, the `.page` box and the `ps-base` layer are CSS-rescaled to
 * the LIVE eased scale every frame, but crisp `ps-slice` commits land at worker
 * RPC cadence carrying pixels rendered at the ENTRY-time scale. When the RPC
 * exceeds a frame, the sharp content visibly holds-then-leaps (12-17% steps at
 * ~commit cadence, measured via +25ms latency injection) — perceived as tremor.
 * In-process mode structurally cannot desync (its render blocks the same main
 * thread that advances the scale), which is why the tremor is worker-specific.
 *
 * The fix: every eased frame, CSS-transform the existing slice by
 * r = liveScale / paintedScale so its stale-scale content tracks the live box
 * BETWEEN commits; a landing commit swaps in sharper pixels and resets the
 * transform to identity. Content can then never desync from the box.
 *
 * The transform must rescale the slice about the PAGE origin (the slice's
 * content is page-anchored), not the slice's own corner: the slice sits at
 * page-relative (leftCss, topCss) painted at `paintedScale`; at `liveScale`
 * that content belongs at (leftCss·r, topCss·r). With `transform-origin: 0 0`
 * the element scales about its own top-left, so the origin shift is supplied
 * by a translate of (leftCss·(r−1), topCss·(r−1)) BEFORE the scale.
 *
 * Pure (no DOM) so vitest can red-green the math — jsdom cannot see the
 * compositor, but a wrong origin (bare `scale(r)`) or an inverted ratio fails
 * these numbers immediately.
 *
 * @param paintedScale the scale the slice's pixels were rendered at (stamped
 *   on the canvas at commit time)
 * @param liveScale    the current eased gesture scale
 * @param leftCss      the slice's committed page-relative CSS left
 * @param topCss       the slice's committed page-relative CSS top
 * @returns a CSS transform string, or "" when tracking is the identity
 *   (r ≈ 1) or the inputs are degenerate (paintedScale ≤ 0 / non-finite).
 */
export function sliceTrackTransform(
  paintedScale: number,
  liveScale: number,
  leftCss: number,
  topCss: number,
): string {
  const r = liveScale / paintedScale;
  if (!Number.isFinite(r) || r <= 0) return "";
  if (Math.abs(r - 1) < 1e-6) return "";
  const tx = leftCss * (r - 1);
  const ty = topCss * (r - 1);
  return `translate(${tx}px, ${ty}px) scale(${r})`;
}
