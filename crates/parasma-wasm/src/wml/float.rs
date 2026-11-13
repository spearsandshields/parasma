//! Floating-object text wrap (§20.4.2 anchored drawings) — exclusion geometry
//! and the float-aware line packer. Verbatim relocation from `lib.rs`:
//! the self-contained float cluster (anchor position resolution, polygon/
//! interval clipping, `pack_lines_around_floats`). Parse-time wrap data
//! (`WrapInfo`/`WrapMode`/`WrapTextSide`) and the shared line types
//! (`Item`/`LineSegment`/`LayoutLine`/`justify_line`) deliberately STAY in
//! `lib.rs`; this module imports them via `crate::`.
//!
//! Purity is enforced by the oracle snapshot tests — their geometry hashes
//! must not drift on a pure move.

use crate::engine::fonts::{FontCache, DEFAULT_FONT_PT};
use crate::engine::units::pt_to_px;
use crate::wml::drawing::{WrapInfo, WrapMode, WrapTextSide};
use crate::wml::linebreak::{
    accumulate_item_metrics, item_fits, resolve_tab, tab_lookaheads, trailing_blank_reserve, BreakAfter,
};
use crate::wml::paragraph::ResolvedTabStops;
use crate::{justify_line, Item, LineSegment};
use ooxml_wml::types::STJc;

/// §20.4.2.13 positionH — horizontal position child: offset or alignment.
#[derive(Clone)]
pub(crate) enum AnchorPos {
    Offset(f64),
    Align(AnchorAlign),
}

#[derive(Clone, Copy)]
pub(crate) enum AnchorAlign { Left, Center, Right, Top, Bottom }

/// §20.4.2.13 relativeFrom (horizontal): page, margin, column, character, etc.
#[derive(Clone, Copy)]
pub(crate) enum AnchorHRelFrom { Page, Margin, Column, Character }

/// §20.4.2.14 relativeFrom (vertical): page, margin, paragraph, line, etc.
#[derive(Clone, Copy)]
pub(crate) enum AnchorVRelFrom { Page, Margin, Paragraph, Line }

// ─── Float exclusion geometry (pure; see polygon-clip unit tests) ───────────

pub(crate) const WRAP_EPS: f64 = 0.01;

/// Merge a set of [start,end) intervals in place: sort by start, coalesce any
/// that overlap or touch (within `WRAP_EPS`). Result is disjoint and sorted.
fn merge_intervals(intervals: &mut Vec<(f64, f64)>) {
    intervals.retain(|(a, b)| *b - *a > WRAP_EPS);
    intervals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged: Vec<(f64, f64)> = Vec::with_capacity(intervals.len());
    for &(s, e) in intervals.iter() {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 + WRAP_EPS {
                if e > last.1 { last.1 = e; }
                continue;
            }
        }
        merged.push((s, e));
    }
    *intervals = merged;
}

/// Subtract a set of blocked intervals from a single `domain` interval,
/// returning the free sub-intervals left over (sorted, disjoint).
fn subtract_intervals(domain: (f64, f64), blocked: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut blk: Vec<(f64, f64)> = blocked
        .iter()
        .map(|&(a, b)| (a.max(domain.0), b.min(domain.1)))
        .filter(|&(a, b)| b - a > WRAP_EPS)
        .collect();
    merge_intervals(&mut blk);

    let mut free = Vec::new();
    let mut cursor = domain.0;
    for &(s, e) in &blk {
        if s - cursor > WRAP_EPS {
            free.push((cursor, s));
        }
        cursor = cursor.max(e);
    }
    if domain.1 - cursor > WRAP_EPS {
        free.push((cursor, domain.1));
    }
    free
}

/// The x-spans a polygon occupies at horizontal scanline `y`, via even-odd
/// edge crossings. Concave polygons yield multiple spans (e.g. a "C").
/// The vertex ring is treated as implicitly closed (last → first).
fn polygon_scanline_spans(polygon: &[(f64, f64)], y: f64) -> Vec<(f64, f64)> {
    if polygon.len() < 3 { return Vec::new(); }
    let mut xs: Vec<f64> = Vec::new();
    for i in 0..polygon.len() {
        let (x1, y1) = polygon[i];
        let (x2, y2) = polygon[(i + 1) % polygon.len()];
        let (lo, hi) = if y1 <= y2 { (y1, y2) } else { (y2, y1) };
        // Half-open [lo, hi) so a scanline through a shared vertex counts once.
        if y >= lo && y < hi {
            let t = (y - y1) / (y2 - y1);
            xs.push(x1 + t * (x2 - x1));
        }
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut spans = Vec::new();
    let mut i = 0;
    while i + 1 < xs.len() {
        spans.push((xs[i], xs[i + 1]));
        i += 2;
    }
    spans
}

/// The blocked x-intervals a polygon imposes across a line's vertical band
/// `[y_top, y_bottom]`. Samples the band edges, its midpoint, and every vertex
/// whose y falls inside the band, then unions the per-scanline spans — so text
/// avoids the polygon anywhere it touches the line's height (conservative and
/// correct: no glyph may overlap the image on any part of the line).
fn polygon_band_blocked(polygon: &[(f64, f64)], y_top: f64, y_bottom: f64) -> Vec<(f64, f64)> {
    if polygon.len() < 3 { return Vec::new(); }
    let y_lo = y_top.min(y_bottom);
    let y_hi = y_top.max(y_bottom);

    let mut sample_ys = vec![y_lo, (y_lo + y_hi) * 0.5, (y_hi - WRAP_EPS).max(y_lo)];
    for &(_, vy) in polygon {
        if vy > y_lo + WRAP_EPS && vy < y_hi - WRAP_EPS {
            sample_ys.push(vy);
            // Just past the vertex too, where span topology may change.
            sample_ys.push((vy + WRAP_EPS).min(y_hi));
        }
    }

    let mut blocked: Vec<(f64, f64)> = Vec::new();
    for &y in &sample_ys {
        blocked.extend(polygon_scanline_spans(polygon, y));
    }
    merge_intervals(&mut blocked);
    blocked
}

/// Restrict free intervals to the side(s) allowed by §20.4.3.7 `wrapText`.
/// `obj_left`/`obj_right` bound the float's horizontal extent on this line.
///  * BothSides — keep everything (incl. concavity gaps between spans).
///  * Left  — text only left of the float → drop intervals reaching its right.
///  * Right — text only right of the float → drop intervals reaching its left.
///  * Largest — keep only the side (left vs right of the float) with more total width.
pub(crate) fn apply_wrap_side(
    free: Vec<(f64, f64)>,
    obj_left: f64,
    obj_right: f64,
    side: WrapTextSide,
) -> Vec<(f64, f64)> {
    match side {
        WrapTextSide::BothSides => free,
        WrapTextSide::Left => free.into_iter().filter(|&(_, e)| e <= obj_left + WRAP_EPS).collect(),
        WrapTextSide::Right => free.into_iter().filter(|&(s, _)| s >= obj_right - WRAP_EPS).collect(),
        WrapTextSide::Largest => {
            let left_w: f64 = free.iter().filter(|&&(_, e)| e <= obj_left + WRAP_EPS)
                .map(|&(s, e)| e - s).sum();
            let right_w: f64 = free.iter().filter(|&&(s, _)| s >= obj_right - WRAP_EPS)
                .map(|&(s, e)| e - s).sum();
            if left_w >= right_w {
                free.into_iter().filter(|&(_, e)| e <= obj_left + WRAP_EPS).collect()
            } else {
                free.into_iter().filter(|&(s, _)| s >= obj_right - WRAP_EPS).collect()
            }
        }
    }
}

/// A floating object that excludes text on the current page, resolved to
/// absolute page coordinates. The exclusion shape is a polygon ring in page px
/// (for square/topAndBottom we synthesize the rectangle's 4 corners), already
/// expanded by distT/B/L/R. `wrap_text` constrains which side(s) text may flow.
#[derive(Clone)]
pub(crate) struct ActiveFloat {
    /// Closed polygon ring in absolute page px.
    polygon: Vec<(f64, f64)>,
    /// Bounding box (page px) — `[x0,x1] × [y0,y1]` — for fast overlap tests
    /// and for the wrap-side left/right reference edges.
    x0: f64,
    x1: f64,
    y0: f64,
    pub(crate) y1: f64,
    wrap_text: WrapTextSide,
}

impl ActiveFloat {
    pub(crate) fn overlaps_band(&self, y_top: f64, y_bottom: f64) -> bool {
        y_bottom > self.y0 + WRAP_EPS && y_top < self.y1 - WRAP_EPS
    }
    /// The float's exclusion top edge (page px).
    pub(crate) fn top(&self) -> f64 {
        self.y0
    }
    /// True when the float's exclusion spans the full content width — no usable
    /// text interval remains on either side (e.g. an image wider than the
    /// column, or wrapTopAndBottom). Used to decide whether preceding empty
    /// paragraphs must reflow below it.
    pub(crate) fn spans_full_width(&self, dom_left: f64, dom_right: f64) -> bool {
        self.x0 <= dom_left + WRAP_EPS && self.x1 >= dom_right - WRAP_EPS
    }
}

/// The minimum top (`f64::INFINITY` if none match) among `floats` satisfying
/// `pred` — the shared trigger of the two retroactive-reflow blocks (the
/// full-width and the inset cases pass opposite `spans_full_width` predicates
/// over the same newly-registered float slice). Pure filter→map→fold.
pub(crate) fn min_float_top(floats: &[ActiveFloat], pred: impl Fn(&ActiveFloat) -> bool) -> f64 {
    floats.iter().filter(|f| pred(f)).map(|f| f.top()).fold(f64::INFINITY, f64::min)
}

/// Given the active floats and a line's vertical band + horizontal domain
/// (`[dom_left, dom_right]`, the paragraph's content box in page px), return
/// the free x-intervals the line's text may occupy, left→right. With no
/// overlapping float this is just `[(dom_left, dom_right)]` (the no-float
/// case → single full-width segment → identical to the old behavior).
pub(crate) fn free_intervals_for_line(
    floats: &[ActiveFloat],
    y_top: f64,
    y_bottom: f64,
    dom_left: f64,
    dom_right: f64,
) -> Vec<(f64, f64)> {
    let overlapping: Vec<&ActiveFloat> =
        floats.iter().filter(|f| f.overlaps_band(y_top, y_bottom)).collect();
    if overlapping.is_empty() {
        return vec![(dom_left, dom_right)];
    }

    // Union every overlapping float's blocked x-intervals across the band.
    let mut blocked: Vec<(f64, f64)> = Vec::new();
    for f in &overlapping {
        blocked.extend(polygon_band_blocked(&f.polygon, y_top, y_bottom));
    }
    merge_intervals(&mut blocked);

    let mut free = subtract_intervals((dom_left, dom_right), &blocked);

    // Apply each float's wrap-side constraint (left/right/largest). For a
    // single overlapping float this is exact; with multiple, we apply each
    // in turn (intersection of constraints) — a rare case, conservative.
    for f in &overlapping {
        if !matches!(f.wrap_text, WrapTextSide::BothSides) {
            free = apply_wrap_side(free, f.x0, f.x1, f.wrap_text);
        }
    }
    free
}

/// Advance a page cursor (`y_in_page`) straight down past any float that leaves
/// no usable room for `min_room` on the line at the current cursor, so the next
/// content resumes BELOW the float (§20.4.2.17: distB is a minimum, so resuming
/// below a full-width object is the [WORD] line-grid behaviour). "No usable
/// room" = the widest free interval can't fit `min_room`; pass the first word's
/// width to wrap text beside partial floats, or `2.0 * WRAP_EPS` to drop a
/// content-less mark/empty line only past a *full-width* float (zero free
/// interval). This is the single primitive behind the three former clear loops
/// (now unified) — equivalence rests on `band_h` feeding BOTH the
/// interval query and the `overlaps_band` bottom-filter, so the
/// `widest == WRAP_EPS` boundary stays masked. `page_has_content` =
/// `!page.is_empty()` guards against an empty page never advancing past a float
/// that overflows it.
// Positional orchestrator args; struct-ification deferred to a dedicated pass.
#[allow(clippy::too_many_arguments)]
pub(crate) fn clear_below_floats(
    floats: &[ActiveFloat],
    y_in_page: &mut f64,
    margin_top: f64,
    band_h: f64,
    dleft: f64,
    dright: f64,
    min_room: f64,
    content_height: f64,
    page_has_content: bool,
) {
    loop {
        let y_top = margin_top + *y_in_page;
        let iv = free_intervals_for_line(floats, y_top, y_top + band_h, dleft, dright);
        let widest = iv.iter().map(|&(s, e)| e - s).fold(0.0_f64, f64::max);
        if widest + WRAP_EPS >= min_room { break; }
        let bottom = floats.iter()
            .filter(|f| f.overlaps_band(y_top, y_top + band_h))
            .map(|f| f.y1)
            .fold(f64::NEG_INFINITY, f64::max);
        // Stop if there's no blocking float, or dropping below it would run off
        // the page (let the page-fill overflow carry the remainder). Guard
        // against non-advancing loops.
        if !bottom.is_finite()
            || bottom <= y_top + WRAP_EPS
            || (bottom - margin_top > content_height && page_has_content)
        { break; }
        *y_in_page = bottom - margin_top;
    }
}

/// Pack already-measured items into multi-segment lines that flow around
/// floats. `line_height` is the paragraph's nominal line height (used to form
/// each line's vertical band — see the accepted simplification: bands use the
/// nominal height, not each line's exact post-pack height). `y_first_top` is the
/// page-y of the first line's top. Returns lines whose segments sit at
/// absolute page x; the caller positions them vertically. Mandatory breaks
/// (`\n`, page breaks) still force a new line.
/// One line produced by float-aware packing: its segments (absolute page-x)
/// plus the vertical metrics the caller needs to place its baseline and
/// advance the cursor. Decouples packing from page placement (no side-channel
/// values smuggled through `PagedLine.baseline_y`).
pub(crate) struct WrappedLine {
    pub(crate) segments: Vec<LineSegment>,
    pub(crate) max_ascent: f64,
    pub(crate) max_descent: f64,
    pub(crate) max_font_pt: f64,
    pub(crate) max_image_height: f64,
    pub(crate) ends_with_page_break: bool,
    /// Explicit vertical advance for a BAND-WALK line, overriding the height the
    /// caller would otherwise derive from this line's metrics.
    ///
    /// A line whose float band is fully blocked carries no segments; it exists
    /// only to descend the page cursor past the float. Emitting one such line per
    /// band pitch is O(float_height / pitch) — for a 12in float at
    /// `w:line="1" w:lineRule="exact"` (1 twip = 0.0667px) that is ~17,000 empty
    /// lines PER PARAGRAPH, each materialised into a `PagedLine` by the caller.
    /// Measured before this field existed: 100 such paragraphs cost 711 MB, versus
    /// 44.9 MB for the identical document without the `w:spacing`.
    ///
    /// So the walk now emits ONE line per blocked RUN and states its own height.
    /// `Some(h)` ⇒ advance the cursor by exactly `h`; the geometry is identical to
    /// the N lines it replaces (N·pitch == h) because an empty line draws nothing.
    /// `None` ⇒ ordinary line, caller derives the height from the metrics as before.
    pub(crate) walk_height: Option<f64>,
}

/// Compute the free intervals for line `idx` of a paragraph, applying the
/// first-line indent to the leftmost interval. Pulled out so packing and the
/// later justification pass agree exactly.
fn line_free_intervals(
    floats: &[ActiveFloat], idx: usize, y_first_top: f64, line_height: f64,
    dom_left: f64, dom_right: f64, first_line_indent: f64,
) -> Vec<(f64, f64)> {
    let y_t = y_first_top + idx as f64 * line_height;
    let mut iv = free_intervals_for_line(floats, y_t, y_t + line_height, dom_left, dom_right);
    if idx == 0 && first_line_indent > 0.0 {
        if let Some(first) = iv.first_mut() { first.0 += first_line_indent; }
        iv.retain(|&(s, e)| e - s > WRAP_EPS);
    }
    iv
}

/// Given that the band at `idx` is FULLY BLOCKED (no free interval), find the
/// first later band that is not — by jumping to float bottoms instead of stepping
/// one pitch at a time.
///
/// A blocked band can only open where a blocking float ENDS, so the next
/// candidate is the smallest `y1` among the floats overlapping it. Taking the
/// MINIMUM is conservative: it can never skip past an opening, and each iteration
/// clears at least the float that produced it, so the loop is bounded by
/// `floats.len()` rather than by `blocked_height / pitch`.
///
/// This is the same primitive `clear_below_floats` already uses to descend the
/// page cursor past a float; the packer was the one place still stepping.
fn next_open_band(
    floats: &[ActiveFloat], idx: usize, y_first_top: f64, band_pitch: f64,
    dom_left: f64, dom_right: f64, first_line_indent: f64,
) -> usize {
    let mut cur = idx + 1;
    // One iteration per float at worst: each jump clears the lowest-ending float
    // still covering the band. `..=` so the final check runs after the last jump.
    for _ in 0..=floats.len() {
        if !line_free_intervals(
            floats, cur, y_first_top, band_pitch, dom_left, dom_right, first_line_indent,
        )
        .is_empty()
        {
            return cur;
        }
        let band_top = y_first_top + cur as f64 * band_pitch;
        let band_bottom = band_top + band_pitch;
        // `overlaps_band` implies `y1 > band_top`, so a finite edge always advances.
        let next_edge = floats
            .iter()
            .filter(|f| f.overlaps_band(band_top, band_bottom))
            .map(|f| f.y1)
            .fold(f64::INFINITY, f64::min);
        if !next_edge.is_finite() {
            return cur; // nothing overlaps: open, or degenerately narrow domain
        }
        let jumped = ((next_edge - y_first_top) / band_pitch).ceil();
        // Guard the cast: a non-finite or non-advancing target must not wrap.
        if !jumped.is_finite() || jumped <= cur as f64 {
            return cur;
        }
        cur = jumped as usize;
    }
    cur
}

/// Pack already-measured items into multi-segment lines that flow around
/// `floats`. Vertical bands use the paragraph's nominal `line_height` (accepted
/// simplification: not each line's exact post-pack height). `y_first_top` is the
/// page-y of the first line's top. Segments carry absolute page-x; the caller
/// places baselines from the returned metrics. Mandatory/page breaks force a
/// new line. Greedy first-fit per interval, mirroring `pack_lines`.
///
/// Items go IN and come OUT in LOGICAL order. Unlike `pack_lines`, this
/// packer does not apply the UAX#9 L2 reorder, because the caller carries every
/// line it could not place back into the next batch's input — reordering here
/// would feed a VISUAL sequence into the next re-pack, and L2 is not idempotent
/// on one. The caller reorders each segment as it places it (`bidi_reorder_line`
/// at the `PagedLine` sites in `lib.rs`/`table.rs`), which is the last moment
/// before paint and the only one where the sequence is final.
// Positional orchestrator args; struct-ification deferred to a dedicated pass.
// `unused_assignments`: the `flush_segment!` macro resets `seg_w` after every
// flush; the reset on the FINAL flush is never read (end of function).
#[allow(clippy::too_many_arguments, unused_assignments)]
pub(crate) fn pack_lines_around_floats(
    items: Vec<Item>,
    floats: &[ActiveFloat],
    dom_left: f64,
    dom_right: f64,
    first_line_indent: f64,
    line_height: f64,
    y_first_top: f64,
    align: STJc,
    font_cache: &FontCache,
    tab_stops: &ResolvedTabStops,
) -> Vec<WrappedLine> {
    let mut out: Vec<WrappedLine> = Vec::new();

    // Conservation bookkeeping (paragraph resume cursor): a wrap-edge whitespace drop is the
    // ONLY path on which an input item does not reach an output segment.
    let n_in = items.len();
    let mut dropped_ws = 0usize;

    // BAND PITCH — used ONLY to advance the float band, never as a rendered line
    // height (the caller derives that from each line's own metrics). A zero or
    // negative `w:line` with `w:lineRule="exact"` is schema-valid and cannot
    // advance the band, so the walk below would never terminate.
    //
    // The floor is 2·WRAP_EPS, NOT WRAP_EPS: `ActiveFloat::overlaps_band` tests
    // `y_bottom > y0 + WRAP_EPS`, so a band exactly WRAP_EPS tall registers NO
    // overlap with a float starting at the cursor — the text would then be placed
    // straight over the image. The floor must exceed the epsilon the geometry
    // itself uses. (Caught by the regression test below, which asserts the walk
    // actually clears the float rather than merely terminating.)
    let band_pitch = if line_height > 2.0 * WRAP_EPS { line_height } else { 2.0 * WRAP_EPS };
    // Set when a blocked-band RUN is collapsed into one line — see WrappedLine::walk_height.
    let mut pending_walk: Option<f64> = None;

    // ONE fit predicate with the float-free packer — see `item_fits`.
    // Sampled once here: the thread-local is per-document, not per-item.
    let trail_reserve = trailing_blank_reserve();
    // Tab advances are re-resolved HERE rather than inherited: the items handed
    // to this packer were tab-resolved by `pack_lines` against the FLOAT-BLIND
    // geometry, so their stops describe a line that no longer exists. See
    // `resolve_tab`; re-resolution is idempotent (it overwrites the tab's own
    // widths, and the lookaheads sum only the items AFTER it).
    let looks = tab_lookaheads(&items);

    let mut line_idx = 0usize;
    let mut intervals = line_free_intervals(
        floats, line_idx, y_first_top, band_pitch, dom_left, dom_right, first_line_indent);
    let mut seg_idx = 0usize;
    let mut seg_x = intervals.first().map(|&(s, _)| s).unwrap_or(dom_left);
    let mut seg_w = 0.0_f64;
    let mut seg_items: Vec<Item> = Vec::new();
    let mut line_segs: Vec<LineSegment> = Vec::new();
    let mut asc = 0.0; let mut desc = 0.0; let mut fpt = DEFAULT_FONT_PT; let mut imgh = 0.0;

    macro_rules! flush_segment { () => {{
        if !seg_items.is_empty() {
            line_segs.push(LineSegment { x_start: seg_x, items: std::mem::take(&mut seg_items) });
        }
        seg_w = 0.0;
    }}; }

    macro_rules! finish_line { ($page_break:expr) => {{
        flush_segment!();
        out.push(WrappedLine {
            segments: std::mem::take(&mut line_segs),
            max_ascent: asc, max_descent: desc, max_font_pt: fpt, max_image_height: imgh,
            ends_with_page_break: $page_break,
            walk_height: pending_walk.take(),
        });
        line_idx += 1;
        asc = 0.0; desc = 0.0; fpt = DEFAULT_FONT_PT; imgh = 0.0;
        intervals = line_free_intervals(
            floats, line_idx, y_first_top, band_pitch, dom_left, dom_right, first_line_indent);
        seg_idx = 0;
        seg_x = intervals.first().map(|&(s, _)| s).unwrap_or(dom_left);
    }}; }

    for (i, mut item) in items.into_iter().enumerate() {
        let mandatory = item.break_after == BreakAfter::Mandatory;
        let page_break = item.break_after == BreakAfter::PageBreak;

        loop {
            match intervals.get(seg_idx).copied() {
                None => {
                    // FULLY BLOCKED band: no free interval at all, so this line can
                    // hold nothing and exists only to descend past the float. Jump
                    // straight to the first band that opens instead of emitting one
                    // empty line per pitch, and collapse the whole run into the
                    // single line `finish_line!` is about to push, carrying its true
                    // height. Partially-blocked bands (some interval, just no room
                    // left in THIS segment) fall through unchanged.
                    if intervals.is_empty() {
                        let next = next_open_band(
                            floats, line_idx, y_first_top, band_pitch,
                            dom_left, dom_right, first_line_indent,
                        );
                        // `finish_line!` advances by one, so pre-advance by the rest.
                        // The emitted line stands for bands [line_idx, next).
                        if next > line_idx + 1 {
                            pending_walk = Some((next - line_idx) as f64 * band_pitch);
                            line_idx = next - 1;
                        }
                    }
                    finish_line!(false)
                }
                Some((s, e)) => {
                    let avail = e - s;
                    if item.tab.is_some() {
                        // §17.3.1.37 measures the stop from the LINE's leading
                        // edge; inside a float band that edge is still the
                        // paragraph's, not the segment's, so the cursor is the
                        // absolute advance minus `dom_left`. On line 0 the first
                        // interval already carries the first-line indent, which
                        // is exactly what the float-blind packer adds.
                        let cursor = if seg_items.is_empty() { s.max(seg_x) } else { seg_x + seg_w };
                        resolve_tab(&mut item, cursor - dom_left, looks[i], tab_stops);
                        // DEVIATION, declared: §17.3.1.37 defines stops in the
                        // paragraph frame and says NOTHING about float
                        // exclusions, so a stop beyond this interval would send
                        // the tab — and the leader glyphs drawn in it — under the
                        // float, which §20.4.2.17 forbids for every wrap mode but
                        // wrapNone. Clamp the advance to the interval. The
                        // following text then misses its stop, which is the
                        // lesser of the two wrongs and is visible rather than
                        // overprinting.
                        let room = e - cursor;
                        if item.width > room {
                            item.width = room.max(pt_to_px(3.0));
                            item.content_width = item.width;
                        }
                    }
                    if seg_items.is_empty() {
                        // First item in a segment always placed (guarantees progress).
                        seg_x = s.max(seg_x);
                        accumulate_item_metrics(&item, &mut asc, &mut desc, &mut fpt, &mut imgh, font_cache);
                        seg_w += item.width;
                        seg_items.push(item);
                        break;
                    } else if item_fits(seg_w, &item, avail, trail_reserve) {
                        accumulate_item_metrics(&item, &mut asc, &mut desc, &mut fpt, &mut imgh, font_cache);
                        seg_w += item.width;
                        seg_items.push(item);
                        break;
                    } else {
                        // Doesn't fit; advance to next interval (new segment), or wrap.
                        flush_segment!();
                        seg_idx += 1;
                        match intervals.get(seg_idx).copied() {
                            Some((ns, _)) => {
                                seg_x = ns;
                                if item.is_only_whitespace() { dropped_ws += 1; break; } // drop ws at segment edge
                            }
                            None => {
                                finish_line!(false);
                                if item.is_only_whitespace() { dropped_ws += 1; break; } // drop ws at line edge
                            }
                        }
                    }
                }
            }
        }

        if mandatory || page_break {
            finish_line!(page_break);
        }
    }
    if !seg_items.is_empty() || !line_segs.is_empty() {
        flush_segment!();
        out.push(WrappedLine {
            segments: std::mem::take(&mut line_segs),
            max_ascent: asc, max_descent: desc, max_font_pt: fpt, max_image_height: imgh,
            ends_with_page_break: false,
            // The trailing flush closes a line that HELD items, so it is never a
            // band-walk line; any pending walk was consumed by finish_line! above.
            walk_height: pending_walk.take(),
        });
    }

    // Item-count conservation (the ResumeToken invariant): items pass through
    // segments unsplit and unduplicated; the ONLY loss is whitespace-only
    // items dropped at a wrap edge (the two `drop ws` sites above). NOTE
    // this is a GLOBAL count equation — it does NOT locate the drops
    // relative to a later placed/unplaced cut (drops occur at tail wrap
    // edges too), so an "N items remaining" cursor alone cannot reconstruct
    // the live remainder from the pristine stream; see the ResumeAt::
    // ParaItems contract-limit note in lib.rs. (Justification below mutates
    // widths in place on non-last lines — including unplaced ones — which
    // is lossiness vector (2) of that note.)
    debug_assert_eq!(
        out.iter().flat_map(|wl| &wl.segments).map(|s| s.items.len()).sum::<usize>() + dropped_ws,
        n_in,
        "pack_lines_around_floats: item-count conservation violated"
    );

    // Per-segment justification: each segment justifies to its own interval
    // width (skipping the last line, per normal justify rules).
    if align == STJc::Both {
        let n = out.len();
        for (i, wl) in out.iter_mut().enumerate() {
            if i + 1 == n { continue; }
            let ivs = line_free_intervals(
                floats, i, y_first_top, line_height, dom_left, dom_right, first_line_indent);
            for (seg, iv) in wl.segments.iter_mut().zip(ivs.iter()) {
                justify_line(&mut seg.items, iv.1 - iv.0);
            }
        }
    }

    out
}

/// Resolve an anchored object's top-left to absolute page px, given the
/// reference bases/extents already computed by the caller. Mirrors the inline
/// math in page assembly so the exclusion path and the draw path agree.
// Positional orchestrator args; struct-ification deferred to a dedicated pass.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_anchor_xy(
    pos_h: &AnchorPos, pos_v: &AnchorPos,
    w: f64, h: f64,
    h_base: f64, h_extent: f64, v_base: f64, v_extent: f64,
) -> (f64, f64) {
    let x = match pos_h {
        AnchorPos::Offset(off) => h_base + off,
        AnchorPos::Align(AnchorAlign::Center) => h_base + (h_extent - w) / 2.0,
        AnchorPos::Align(AnchorAlign::Right) => h_base + h_extent - w,
        _ => h_base,
    };
    let y = match pos_v {
        AnchorPos::Offset(off) => v_base + off,
        AnchorPos::Align(AnchorAlign::Center) => v_base + (v_extent - h) / 2.0,
        AnchorPos::Align(AnchorAlign::Bottom) => v_base + v_extent - h,
        _ => v_base,
    };
    (x, y)
}

/// Build the page-space exclusion for a wrapping float anchored at top-left
/// `(x, y)`. `dom_left`/`dom_right` bound the paragraph content box, used so a
/// `TopAndBottom` float blocks the *full* content width (no text beside it).
/// Returns `None` for `wrapNone` (no exclusion).
pub(crate) fn build_active_float(
    wrap: &WrapInfo, x: f64, y: f64, w: f64, h: f64,
    dom_left: f64, dom_right: f64,
) -> Option<ActiveFloat> {
    let (dt, db, dl, dr) = (wrap.dist_t, wrap.dist_b, wrap.dist_l, wrap.dist_r);
    let (poly, x0, x1, y0, y1) = match &wrap.mode {
        WrapMode::None => return None,
        WrapMode::Square => {
            let (a, b, c, d) = (x - dl, x + w + dr, y - dt, y + h + db);
            (vec![(a, c), (b, c), (b, d), (a, d)], a, b, c, d)
        }
        WrapMode::TopAndBottom => {
            // Full content width so no text sits beside the object.
            let (c, d) = (y - dt, y + h + db);
            let (a, b) = (dom_left - 1.0, dom_right + 1.0);
            (vec![(a, c), (b, c), (b, d), (a, d)], a, b, c, d)
        }
        WrapMode::Polygon { polygon } => match polygon {
            Some(pts) if pts.len() >= 3 => {
                // Polygon coords are px offsets from the object's upper-left.
                let abs: Vec<(f64, f64)> = pts.iter().map(|&(px, py)| (x + px, y + py)).collect();
                let x0 = abs.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
                let x1 = abs.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
                let y0 = abs.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
                let y1 = abs.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
                (abs, x0, x1, y0, y1)
            }
            // Degenerate polygon → fall back to the bounding rectangle (Square).
            _ => {
                let (a, b, c, d) = (x - dl, x + w + dr, y - dt, y + h + db);
                (vec![(a, c), (b, c), (b, d), (a, d)], a, b, c, d)
            }
        },
    };
    Some(ActiveFloat { polygon: poly, x0, x1, y0, y1, wrap_text: wrap.wrap_text })
}

#[cfg(test)]
mod wrap_geometry_tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool { (a - b).abs() < 0.1 }

    #[test]
    fn subtract_middle_block_splits_domain() {
        let free = subtract_intervals((0.0, 100.0), &[(40.0, 60.0)]);
        assert_eq!(free.len(), 2);
        assert!(approx(free[0].0, 0.0) && approx(free[0].1, 40.0));
        assert!(approx(free[1].0, 60.0) && approx(free[1].1, 100.0));
    }

    #[test]
    fn subtract_left_block_leaves_right() {
        let free = subtract_intervals((0.0, 100.0), &[(0.0, 30.0)]);
        assert_eq!(free.len(), 1);
        assert!(approx(free[0].0, 30.0) && approx(free[0].1, 100.0));
    }

    #[test]
    fn subtract_overlapping_blocks_merge() {
        let free = subtract_intervals((0.0, 100.0), &[(10.0, 40.0), (30.0, 55.0)]);
        assert_eq!(free.len(), 2);
        assert!(approx(free[0].1, 10.0));
        assert!(approx(free[1].0, 55.0));
    }

    #[test]
    fn rectangle_polygon_blocks_its_x_extent() {
        // Square 20..70 horizontally, 0..100 vertically.
        let sq = vec![(20.0, 0.0), (70.0, 0.0), (70.0, 100.0), (20.0, 100.0)];
        let blocked = polygon_band_blocked(&sq, 40.0, 52.0);
        assert_eq!(blocked.len(), 1);
        assert!(approx(blocked[0].0, 20.0) && approx(blocked[0].1, 70.0));
    }

    #[test]
    fn diamond_narrows_with_height() {
        // Diamond centered at x=50, spanning y 0..100, widest (0..100) at y=50.
        let diamond = vec![(50.0, 0.0), (100.0, 50.0), (50.0, 100.0), (0.0, 50.0)];
        // Band near the top tip → narrow blocked span around x=50.
        let near_top = polygon_band_blocked(&diamond, 8.0, 12.0);
        assert_eq!(near_top.len(), 1);
        let top_w = near_top[0].1 - near_top[0].0;
        // Band at the middle → near-full-width blocked span.
        let middle = polygon_band_blocked(&diamond, 48.0, 52.0);
        let mid_w = middle[0].1 - middle[0].0;
        assert!(mid_w > top_w, "diamond should block more at its waist ({mid_w}) than its tip ({top_w})");
    }

    #[test]
    fn concave_pi_shape_yields_two_spans_in_the_gap() {
        // A "∏" / tuning-fork: two legs joined across the top, open at the
        // bottom. A scanline through the legs hits two separated filled spans
        // (this is the both-sides-of-a-cutout case true polygon wrap must keep).
        let pi_shape = vec![
            (0.0, 0.0), (60.0, 0.0), (60.0, 60.0), (40.0, 60.0),
            (40.0, 20.0), (20.0, 20.0), (20.0, 60.0), (0.0, 60.0),
        ];
        let spans = polygon_scanline_spans(&pi_shape, 30.0);
        assert_eq!(spans.len(), 2, "leg row should split into two filled spans");
        assert!(approx(spans[0].0, 0.0) && approx(spans[0].1, 20.0));
        assert!(approx(spans[1].0, 40.0) && approx(spans[1].1, 60.0));
    }

    #[test]
    fn wrap_side_left_drops_right_intervals() {
        let free = vec![(0.0, 30.0), (70.0, 100.0)];
        let kept = apply_wrap_side(free, 30.0, 70.0, WrapTextSide::Left);
        assert_eq!(kept.len(), 1);
        assert!(approx(kept[0].1, 30.0));
    }

    #[test]
    fn wrap_side_largest_keeps_wider_side() {
        // Left side width 20, right side width 40 → keep right only.
        let free = vec![(0.0, 20.0), (80.0, 120.0)];
        let kept = apply_wrap_side(free, 20.0, 80.0, WrapTextSide::Largest);
        assert_eq!(kept.len(), 1);
        assert!(approx(kept[0].0, 80.0));
    }

    #[test]
    fn full_width_centered_largest_float_leaves_no_room_beside() {
        // Regression case: a 642.5px image centered in a 642.5px
        // content column (spans it exactly) with wrapText="largest". No text may
        // sit beside it on any line within its vertical band — every in-band line
        // must get ZERO free intervals (so the packer pushes text below it).
        let wrap = WrapInfo {
            mode: WrapMode::Square, wrap_text: WrapTextSide::Largest,
            dist_t: 0.0, dist_b: 0.0, dist_l: 0.0, dist_r: 0.0,
        };
        let dom_left = 75.6; let dom_right = 718.1;
        let af = build_active_float(&wrap, 75.6, 75.6, 642.5, 428.5, dom_left, dom_right).unwrap();
        let floats = [af];
        // line ~17, y≈374 — squarely inside the band [75.6, 504]
        let iv = free_intervals_for_line(&floats, 374.0, 374.0 + 17.6, dom_left, dom_right);
        assert!(iv.is_empty(), "in-band line must have no free interval, got {iv:?}");
    }

    // ─── Geometry-asserting safety net for the float kernels ─────────────────
    // These call the kernel functions the bounded-iteration refactor extracts
    // verbatim (`build_active_float` → float_resolve.rs; `free_intervals_for_line`
    // → clear_below_floats / the band reformat; `resolve_anchor_xy` → the two-phase
    // anchor resolvers). They assert resolved rects and per-line free-interval
    // x-ranges DIRECTLY (not FNV hashes), so extracting these kernels cannot
    // silently change float geometry. They pass against current behavior.
    // The orchestration-level guards (textbox/cell registration, anchor
    // look-ahead, reflow, cross-page wrap deferral) live with the code paths,
    // co-located with the production logic they protect.

    fn wrap(mode: WrapMode, side: WrapTextSide) -> WrapInfo {
        WrapInfo { mode, wrap_text: side, dist_t: 0.0, dist_b: 0.0, dist_l: 0.0, dist_r: 0.0 }
    }

    #[test]
    fn square_float_leaves_room_on_both_sides() {
        // A 50px-wide square at x=80 in a 0..200 content box, wrapping both
        // sides. A line crossing its band keeps the left strip [0,80] and the
        // right strip [130,200]; the object's x-extent [80,130] is blocked.
        let af = build_active_float(&wrap(WrapMode::Square, WrapTextSide::BothSides),
            80.0, 40.0, 50.0, 100.0, 0.0, 200.0).unwrap();
        let iv = free_intervals_for_line(&[af], 60.0, 76.0, 0.0, 200.0);
        assert_eq!(iv.len(), 2, "both sides should keep room: {iv:?}");
        assert!(approx(iv[0].0, 0.0) && approx(iv[0].1, 80.0));
        assert!(approx(iv[1].0, 130.0) && approx(iv[1].1, 200.0));
    }

    #[test]
    fn topandbottom_blocks_full_width_then_clears_below() {
        // A TopAndBottom float occupies y[40,140]. In-band lines get NO room
        // (text stops above); a line BELOW the float (y>140) gets the full
        // content width back (text resumes below). This is the kernel contract
        // the Phase-B `clear_below_floats` primitive depends on.
        let af = build_active_float(&wrap(WrapMode::TopAndBottom, WrapTextSide::BothSides),
            80.0, 40.0, 50.0, 100.0, 0.0, 200.0).unwrap();
        assert!(approx(af.top(), 40.0));
        assert!(approx(af.y1, 140.0));
        assert!(af.spans_full_width(0.0, 200.0), "TopAndBottom must span the content width");
        let in_band = free_intervals_for_line(&[af.clone()], 60.0, 76.0, 0.0, 200.0);
        assert!(in_band.is_empty(), "in-band line must be fully blocked: {in_band:?}");
        let below = free_intervals_for_line(&[af], 150.0, 166.0, 0.0, 200.0);
        assert_eq!(below.len(), 1);
        assert!(approx(below[0].0, 0.0) && approx(below[0].1, 200.0));
    }

    #[test]
    fn square_float_above_a_line_does_not_block_it() {
        // A line whose band sits ENTIRELY below the float's y-extent sees the
        // full width — guards the "a cleared line resumes at full width" path
        // that today's three clear loops (and tomorrow's one) all rely on.
        let af = build_active_float(&wrap(WrapMode::Square, WrapTextSide::BothSides),
            80.0, 40.0, 50.0, 60.0, 0.0, 200.0).unwrap(); // y-extent [40,100]
        let iv = free_intervals_for_line(&[af], 120.0, 136.0, 0.0, 200.0);
        assert_eq!(iv.len(), 1);
        assert!(approx(iv[0].0, 0.0) && approx(iv[0].1, 200.0));
    }

    #[test]
    fn dist_margins_expand_the_blocked_extent() {
        // distL/R/T/B (§20.4.2.* wrap distances) widen the exclusion: a 50px
        // object with distL=10, distR=15 blocks [80-10, 130+15] = [70,145].
        let w = WrapInfo { mode: WrapMode::Square, wrap_text: WrapTextSide::BothSides,
            dist_t: 5.0, dist_b: 5.0, dist_l: 10.0, dist_r: 15.0 };
        let af = build_active_float(&w, 80.0, 40.0, 50.0, 100.0, 0.0, 200.0).unwrap();
        // band must overlap the distT-expanded top (y0 = 40-5 = 35)
        assert!(approx(af.top(), 35.0));
        let iv = free_intervals_for_line(&[af], 60.0, 76.0, 0.0, 200.0);
        assert_eq!(iv.len(), 2, "{iv:?}");
        assert!(approx(iv[0].1, 70.0), "left strip ends at x-distL: {iv:?}");
        assert!(approx(iv[1].0, 145.0), "right strip starts at x+w+distR: {iv:?}");
    }

    #[test]
    fn wrap_text_left_keeps_only_the_left_strip() {
        // wrapText="left": text only on the left of the object, none on the
        // right — the right strip is dropped even though geometrically free.
        let af = build_active_float(&wrap(WrapMode::Square, WrapTextSide::Left),
            80.0, 40.0, 50.0, 100.0, 0.0, 200.0).unwrap();
        let iv = free_intervals_for_line(&[af], 60.0, 76.0, 0.0, 200.0);
        assert_eq!(iv.len(), 1, "only the left side survives: {iv:?}");
        assert!(approx(iv[0].0, 0.0) && approx(iv[0].1, 80.0));
    }

    #[test]
    fn two_floats_leave_only_the_central_gap() {
        // A left-anchored float [0,60] and a right-anchored float [150,200]
        // sharing a band leave exactly the middle channel [60,150] for text.
        let left = build_active_float(&wrap(WrapMode::Square, WrapTextSide::BothSides),
            0.0, 40.0, 60.0, 100.0, 0.0, 200.0).unwrap();
        let right = build_active_float(&wrap(WrapMode::Square, WrapTextSide::BothSides),
            150.0, 40.0, 50.0, 100.0, 0.0, 200.0).unwrap();
        let iv = free_intervals_for_line(&[left, right], 60.0, 76.0, 0.0, 200.0);
        assert_eq!(iv.len(), 1, "only the central channel survives: {iv:?}");
        assert!(approx(iv[0].0, 60.0) && approx(iv[0].1, 150.0));
    }

    #[test]
    fn tight_concave_polygon_keeps_both_legs_of_the_gap() {
        // wrapTight with a ∏ outline: a scanline through the legs leaves the
        // central cutout as free space between two blocked legs — the concave
        // case build_active_float must preserve end-to-end (not collapse to a
        // bounding box). Object at (100,200); legs block, gap is free.
        let ring = vec![
            (0.0, 0.0), (60.0, 0.0), (60.0, 60.0), (40.0, 60.0),
            (40.0, 20.0), (20.0, 20.0), (20.0, 60.0), (0.0, 60.0),
        ];
        let w = wrap(WrapMode::Polygon { polygon: Some(ring) }, WrapTextSide::BothSides);
        // place at object origin (100, 200), 60x60 box, in a wide content box
        let af = build_active_float(&w, 100.0, 200.0, 60.0, 60.0, 0.0, 400.0).unwrap();
        // a line through the legs (object-local y=30 → page y=230)
        let iv = free_intervals_for_line(&[af], 229.0, 231.0, 0.0, 400.0);
        // free = everything except the two legs [100,120] and [140,160]
        assert_eq!(iv.len(), 3, "left margin + gap + right margin: {iv:?}");
        assert!(approx(iv[0].1, 100.0), "blocked starts at left leg: {iv:?}");
        assert!(approx(iv[1].0, 120.0) && approx(iv[1].1, 140.0), "central gap free: {iv:?}");
        assert!(approx(iv[2].0, 160.0), "blocked ends at right leg: {iv:?}");
    }

    #[test]
    fn spans_full_width_distinguishes_inset_from_full_bleed() {
        // The reflow/Phase-B trigger fires only for floats that span the
        // content width. An inset object must report false; a content-spanning
        // one true (within WRAP_EPS at both edges).
        let inset = build_active_float(&wrap(WrapMode::Square, WrapTextSide::BothSides),
            80.0, 40.0, 50.0, 100.0, 0.0, 200.0).unwrap();
        assert!(!inset.spans_full_width(0.0, 200.0), "inset float is not full-width");
        let full = build_active_float(&wrap(WrapMode::Square, WrapTextSide::Largest),
            0.0, 40.0, 200.0, 100.0, 0.0, 200.0).unwrap();
        assert!(full.spans_full_width(0.0, 200.0), "content-spanning float is full-width");
    }

    #[test]
    fn resolve_anchor_xy_offset_align_center_right_bottom() {
        // The position arithmetic both resolver phases extract verbatim into float_resolve.
        // Object 40x30; H base=100 extent=300; V base=50 extent=200.
        let off = resolve_anchor_xy(&AnchorPos::Offset(12.0), &AnchorPos::Offset(8.0),
            40.0, 30.0, 100.0, 300.0, 50.0, 200.0);
        assert!(approx(off.0, 112.0) && approx(off.1, 58.0), "offset adds to base: {off:?}");
        let ctr = resolve_anchor_xy(&AnchorPos::Align(AnchorAlign::Center),
            &AnchorPos::Align(AnchorAlign::Center), 40.0, 30.0, 100.0, 300.0, 50.0, 200.0);
        // x = 100 + (300-40)/2 = 230 ; y = 50 + (200-30)/2 = 135
        assert!(approx(ctr.0, 230.0) && approx(ctr.1, 135.0), "centered: {ctr:?}");
        let far = resolve_anchor_xy(&AnchorPos::Align(AnchorAlign::Right),
            &AnchorPos::Align(AnchorAlign::Bottom), 40.0, 30.0, 100.0, 300.0, 50.0, 200.0);
        // x = 100 + 300 - 40 = 360 ; y = 50 + 200 - 30 = 220
        assert!(approx(far.0, 360.0) && approx(far.1, 220.0), "right/bottom aligned: {far:?}");
    }
}

#[cfg(test)]
mod packer_conservation_tests {
    /// The default stop chain: no custom stops, Word's 0.5in default interval.
    /// These unit tests carry no tab items, so it is never consulted.
    fn no_tabs() -> ResolvedTabStops {
        ResolvedTabStops { custom: Vec::new(), bars: Vec::new(), default_interval: 48.0 }
    }

    // ResumeToken precondition: `pack_lines_around_floats` conserves
    // item count except for whitespace-only items dropped at a wrap edge —
    // the invariant that makes "N items consumed" a valid paragraph resume
    // cursor. These exercise the two tricky paths (mid-batch page break,
    // wrap-edge ws drop); the in-function debug_assert fires here too.
    use super::*;
    use crate::engine::paint::RunStyle;

    fn item(text: &str, width: f64, break_after: BreakAfter) -> Item {
        Item {
            text: text.to_string(),
            style: RunStyle::marker(),
            width,
            content_width: width,
            break_after,
            char_start: 0,
            char_len: text.len() as u32,
            ..Default::default()
        }
    }

    fn placed_count(out: &[WrappedLine]) -> usize {
        out.iter().flat_map(|wl| &wl.segments).map(|s| s.items.len()).sum()
    }

    /// A fully-blocked band is walked by JUMPING to the float's bottom, not by
    /// stepping one `line_height` at a time.
    ///
    /// `<w:spacing w:line="1" w:lineRule="exact"/>` is a legal 1-twip (0.0667px)
    /// line height. Against a 12in `wrapTopAndBottom` float that is ~17,000 fully
    /// blocked bands, and the old packer emitted ONE `WrappedLine` for each — every
    /// one materialised into a `PagedLine` by the caller. Measured end to end: 100
    /// such paragraphs cost 711 MB, versus 44.9 MB for the identical document
    /// without the `w:spacing`. `w:line="0"` was worse still: the band could not
    /// advance at all and the loop never terminated.
    ///
    /// Both are now O(#floats). This test pins the LINE COUNT, which is the thing
    /// that grew — asserting only "it terminates" would also pass an earlier
    /// fix that turned the hang into a 3.6 GB allocation.
    #[test]
    fn a_blocked_band_is_jumped_not_stepped() {
        let wrap = WrapInfo {
            mode: WrapMode::TopAndBottom,
            wrap_text: WrapTextSide::BothSides,
            dist_t: 0.0, dist_b: 0.0, dist_l: 0.0, dist_r: 0.0,
        };
        // A 12in-tall full-width float starting at the cursor: 1152px of blockage.
        let float = build_active_float(&wrap, 0.0, 0.0, 600.0, 1152.0, 0.0, 600.0)
            .expect("wrapTopAndBottom yields an exclusion band");
        let floats = [float];

        for pitch in [1.0 / 15.0, 0.0, -4.0] {
            let items = vec![item("word", 40.0, BreakAfter::Allowed)];
            let out = pack_lines_around_floats(
                items, &floats, 0.0, 600.0, 0.0, pitch, 0.0,
                STJc::Left, &FontCache::new(), &no_tabs(),
            );
            // One walk line + one line holding the word. Stepping would have emitted
            // ~17,000 at 1 twip, and never returned at all at 0.
            assert!(
                out.len() <= 4,
                "pitch {pitch}: expected an O(#floats) walk, got {} lines",
                out.len()
            );
            // The word still lands, below the float — conservation is not traded away.
            assert_eq!(placed_count(&out), 1, "pitch {pitch}: the item must still be placed");
            // The walk states its own advance, and it must actually clear the float.
            let walked: f64 = out.iter().filter_map(|l| l.walk_height).sum();
            assert!(
                walked >= 1152.0,
                "pitch {pitch}: walk of {walked}px does not clear the 1152px float"
            );
        }
    }

    #[test]
    fn page_break_mid_batch_conserves_items() {
        // A PageBreak item forces finish_line but must itself stay in a segment.
        let items = vec![
            item("one", 30.0, BreakAfter::PageBreak),
            item("two", 30.0, BreakAfter::Allowed),
            item("three", 30.0, BreakAfter::Allowed),
        ];
        let fc = FontCache::new();
        let out = pack_lines_around_floats(
            items, &[], 0.0, 100.0, 0.0, 16.0, 0.0, STJc::Start, &fc, &no_tabs());
        assert_eq!(out.len(), 2);
        assert!(out[0].ends_with_page_break, "break carrier line must flag the page break");
        assert_eq!(placed_count(&out), 3, "all 3 items must reach segments");
    }

    #[test]
    fn wrap_edge_whitespace_drop_is_the_only_loss() {
        // "AAAAA"(90) fills the 100px line; " "(20) fails to fit at the line
        // edge and is dropped; "BBBB"(60) starts line 2. 3 in → 2 placed.
        let items = vec![
            item("AAAAA", 90.0, BreakAfter::Allowed),
            item(" ", 20.0, BreakAfter::Allowed),
            item("BBBB", 60.0, BreakAfter::Allowed),
        ];
        let fc = FontCache::new();
        let out = pack_lines_around_floats(
            items, &[], 0.0, 100.0, 0.0, 16.0, 0.0, STJc::Start, &fc, &no_tabs());
        assert_eq!(out.len(), 2);
        assert_eq!(placed_count(&out), 2, "only the ws item may be lost");
        let texts: Vec<&str> = out.iter().flat_map(|wl| &wl.segments)
            .flat_map(|s| &s.items).map(|it| it.text.as_str()).collect();
        assert_eq!(texts, ["AAAAA", "BBBB"], "surviving items keep input order");
    }
}
