/**
 * Paint-generation supersede decision (back-buffer slice repaint).
 *
 * A monotonic `paintSeq` token is bumped once per PAINT GENERATION (a batch of
 * paintWindow launches that should supersede any older in-flight paints). Each
 * paintWindow captures the token at entry (`seqAtStart`) and, AFTER its async
 * renderPageWindow resolves, gates every visible-canvas mutation on this
 * decision. A stale in-flight paint (whose generation has since been superseded)
 * must NOT blit its now-outdated pixels over a newer already-landed frame.
 *
 * Kept as a tiny pure module (no DOM imports) so vitest can exercise the
 * correctness-critical latest-wins logic without importing the DOM-heavy adapter
 * — jsdom has no real 2D raster, so the blit/white-window timing cannot be
 * asserted there; only this decision can.
 *
 * @param seqAtStart the paintSeq value captured at paintWindow entry
 * @param currentSeq the LIVE paintSeq at the moment of the commit gate
 * @returns true ⇒ this paint is still the current generation, commit (blit);
 *          false ⇒ a newer generation has superseded it, drop WITHOUT touching
 *          the visible canvas or sliceCoverage.
 */
export function shouldCommitPaint(seqAtStart: number, currentSeq: number): boolean {
  return seqAtStart === currentSeq;
}
