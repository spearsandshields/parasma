/**
 * device-pixels.ts — keep canvases aligned to the physical screen-pixel grid.
 *
 * A canvas whose size OR on-screen position lands on a FRACTIONAL device pixel
 * is bilinear-resampled by the browser compositor → the whole bitmap (text,
 * rules, images) goes soft. This bites at fractional devicePixelRatio — Windows
 * 125 % / 150 % display scaling gives dpr 1.25 / 1.5 — and is invisible at
 * integer DPR (a 2× Mac display never lands off-grid, which is why the softness
 * shows on Windows but not there). This is the usual cause of fuzzy canvas text
 * at 125 % scaling; the fix is to map the canvas exactly onto physical pixels.
 *
 * We render at the TRUE devicePixelRatio (no supersampling) and lean on these
 * two helpers for the alignment that actually drives crispness.
 */

/**
 * Round a CSS length to a whole number of DEVICE pixels. Sizing a backing store
 * at `result · dpr` (an integer) and the CSS box at `result` makes the canvas
 * map 1:1 onto the screen grid, so it is never scaled-resampled on composite.
 */
export function snapLenToDevice(cssLen: number, dpr: number): number {
  return Math.round(cssLen * dpr) / dpr;
}

/**
 * Nudge an element horizontally so its left edge sits on a whole device pixel —
 * killing the compositor resample that softens a centred canvas at fractional
 * DPR. No-op when already aligned (integer DPR, or a zero remainder).
 *
 * IDEMPOTENT: the target translateX is computed WITHOUT first clearing the
 * transform (we recover the natural left as `rect.left − appliedNudge`), and
 * `el.style.transform` is written ONLY when that target actually changes. So a
 * repaint that did NOT move the page — a content-only invalidate from a
 * selection drag or search highlight, which fans through the same paintWindow
 * snap every frame — is a true no-op: one layout read, zero DOM writes. That
 * kills the per-frame clear→reflow→reapply thrash (the "page-shake oscillator")
 * that jitters the page and the highlight riding on it at fractional DPR.
 * A genuine layout change (initial paint, zoom-settle, resize) moves the natural
 * left, so the identical corrective nudge is still written → crispness at rest
 * is preserved. Still forces one layout read, so prefer paint/zoom-settle/resize.
 */
// `dpr` is REQUIRED and comes from the caller's `RootScope.dpr()`, so this stays
// a pure free function with no global read. It used to default to
// `window.devicePixelRatio`, which was the single page-global read left in the
// adapters and a direct violation of the root-scoping rule: in an iframe or a second viewer
// instance that global is the wrong window. No in-tree caller ever used the
// default, so it was an escape hatch that only weakened the rule.
export function snapElementToDeviceGrid(el: HTMLElement, dpr: number): void {
  // Recover the natural (un-nudged) position WITHOUT clearing the transform:
  // read the translateX we last applied (this module is the ONLY writer of
  // `transform` on a `.page`), measure the CURRENT rect (which includes it),
  // then strip it. Avoids the unconditional `transform=""` → measure → re-apply
  // that wrote the DOM on every call.
  const applied = currentTranslateX(el);
  const rect = el.getBoundingClientRect(); // includes `applied` (translateX ⇏ width)
  // When the page is NARROWER than its container it is FLEX-centred (the
  // `.ps-pages` `align-items: safe center`) at a fractional offset. A snap
  // translateX can't move it onto the device grid — layout recomputes the centre
  // from the page width — so the nudge only FIGHTS the centring, and on zoom-out
  // (where the page crosses below the viewport width) the clear→measure→apply
  // cycle oscillates the page between centred and nudged: visible flicker. Skip
  // the snap for centred pages; leave the stable centred position — but DROP any
  // stale nudge left over from a prior wider layout (the old code cleared
  // unconditionally at the top), dedup'd so an already-clean page is a no-op.
  const parent = el.parentElement;
  if (parent && rect.width < parent.clientWidth - 0.5) {
    if (el.style.transform !== "") el.style.transform = "";
    lastNaturalLeft.delete(el); // centred: drop snap state so a re-widen re-snaps
    return;
  }
  const naturalLeft = rect.left - applied; // strip the applied nudge
  // Skip entirely when the page has NOT moved since the last snap. Recomputing
  // the nudge per frame re-rounds `naturalLeft·dpr`; at a half-device-pixel tie
  // (e.g. 30·1.25 = 37.5) `Math.round`'s tie-break flips on float noise, so the
  // target alternates ±0.5 device px → a per-frame ±translateX OSCILLATION (the
  // residual page-shake — string-dedup can't catch an alternating value). A
  // content-only repaint (selection drag / search highlight, which fan through
  // the same paintWindow snap every frame) leaves `naturalLeft` unchanged, so we
  // return without re-rounding or touching the transform. A genuine layout change
  // (zoom-settle, resize) MOVES `naturalLeft` → cache miss → we re-snap crisp.
  const prev = lastNaturalLeft.get(el);
  if (prev !== undefined && prev.dpr === dpr && Math.abs(prev.left - naturalLeft) < 1e-2) {
    return;
  }
  const deviceLeft = naturalLeft * dpr;
  const frac = deviceLeft - Math.round(deviceLeft); // sub-device-pixel remainder
  const next = Math.abs(frac) > 1e-3 ? `translateX(${(-frac / dpr).toFixed(4)}px)` : "";
  if (el.style.transform !== next) el.style.transform = next; // write only on change
  lastNaturalLeft.set(el, { left: naturalLeft, dpr });
}

/**
 * Per-element natural (un-nudged) left + dpr at the last actual snap. When an
 * invalidate leaves this unchanged (a content-only repaint), the snap is skipped
 * so it never re-rounds at a half-pixel tie. Keyed weakly so detached pages GC.
 */
const lastNaturalLeft = new WeakMap<HTMLElement, { left: number; dpr: number }>();

/**
 * The px value of the translateX(…) this module last wrote inline, or 0. We are
 * the sole writer of `transform` on a `.page`, so parsing our own inline string
 * is exact and avoids a getComputedStyle matrix round-trip (and its reflow).
 */
function currentTranslateX(el: HTMLElement): number {
  const m = /translateX\(\s*(-?[\d.]+)px\s*\)/.exec(el.style.transform);
  return m ? parseFloat(m[1]) : 0;
}
