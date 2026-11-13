//! SML frozen panes (§18.3.1.66 `<pane>`) — GRID VIEW ONLY.
//!
//! [`PaneInfo`] is the parsed file fact (captured on the `SheetIndex` by the
//! husk parse, mirroring `grid_lines_enabled`); [`PaneBands`] is its derived
//! content-space geometry over an [`IndexGeom`]. Both the windowed grid PAINTER
//! and the grid HIT-TEST consume the SAME [`PaneBands`] mapping —
//! [`PaneBands::eff_scroll`] for a point, [`axis_segments`] for the paint
//! quadrants — so the view→cell mapping cannot drift between them (the two are
//! pinned consistent by `segments_and_eff_scroll_agree`).
//!
//! Spec grounding (ECMA-376 §18.3.1.66):
//!  - `state="frozen"` / `"frozenSplit"`: `xSplit`/`ySplit` are COLUMN/ROW
//!    COUNTS — the first `xSplit` columns and first `ySplit` rows are pinned.
//!  - `state="split"` (a draggable non-frozen splitter; 1/20-pt units) is
//!    deliberately IGNORED — no pane (a scope decision, not an oversight).
//!  - `topLeftCell` is the BODY pane's saved top-left — the sheet-open scroll
//!    restore (absent → A1, i.e. scroll 0).
//!
//! EDGE POLICY (faithful, NO clamping): a pathologically tall/wide frozen band
//! renders as-is — the body just gets the remaining viewport (possibly none).

use super::index::IndexGeom;

/// The parsed `<pane>` file facts of a FROZEN sheet view (state `frozen` /
/// `frozenSplit`; a `split` or absent pane never constructs one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneInfo {
    /// Frozen COLUMN count (`@xSplit`, §18.3.1.66). 0 = no frozen columns.
    pub(crate) x_split: u32,
    /// Frozen ROW count (`@ySplit`). 0 = no frozen rows.
    pub(crate) y_split: u32,
    /// The body pane's saved `(row, col)` top-left (`@topLeftCell`), if any.
    pub(crate) top_left: Option<(u32, u32)>,
}

/// Content-space frozen-band geometry: the freeze boundary offsets derived from
/// the SAME prefix sums every other grid mapping uses (`IndexGeom`). Zero along
/// an axis ⇔ that axis is not frozen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PaneBands {
    /// Content-space width of the pinned column band (left edge of the first
    /// unfrozen column). 0 = no frozen columns.
    pub(crate) band_w: f64,
    /// Content-space height of the pinned row band (top edge of the first
    /// unfrozen row). 0 = no frozen rows.
    pub(crate) band_h: f64,
    /// The last frozen row (`ySplit`), for the band window fetch. 0 when the
    /// y axis is not frozen.
    pub(crate) y_split: u32,
}

impl PaneBands {
    /// Derive the band geometry for a frozen pane over a sheet's closed-form
    /// geometry. `None` when nothing is effectively frozen (both splits 0, or
    /// the frozen span lies entirely above/left of the grid origin). Guards
    /// weird values by CLAMPING the rail lookups, never the band sizes (a
    /// frozen span past the grid extent pins the whole grid — faithful).
    pub(crate) fn build(pane: &PaneInfo, geom: &IndexGeom) -> Option<PaneBands> {
        let band_w = if pane.x_split > 0 {
            // Left edge of column `x_split + 1` (columns are 1-based counts from
            // A). `col_left` indexes the flat rails vec → clamp into its domain;
            // a split at/right of the last rail pins the whole grid width.
            let c = (pane.x_split.saturating_add(1)).clamp(geom.min_col, geom.max_col + 1);
            geom.col_left(c).max(0.0)
        } else {
            0.0
        };
        let band_h = if pane.y_split > 0 {
            // Top edge of row `y_split + 1` — `row_top` is closed-form over ANY
            // row (kill-the-void), so no upper clamp: NO band clamping.
            geom.row_top(pane.y_split.saturating_add(1)).max(0.0)
        } else {
            0.0
        };
        if band_w <= 0.0 && band_h <= 0.0 {
            return None;
        }
        PaneBands {
            band_w,
            band_h,
            y_split: if band_h > 0.0 { pane.y_split } else { 0 },
        }
        .into()
    }

    /// The EFFECTIVE scroll offsets for a gutter-relative view point (`vx`/`vy`
    /// measured from the grid content origin, i.e. `view_x - gutter_w`): a point
    /// inside a frozen band sees ZERO scroll along the pinned axis; beyond the
    /// freeze boundary the body scroll applies. The hit-test half of the shared
    /// quadrant mapping — [`axis_segments`] is the paint half, and
    /// `segments_and_eff_scroll_agree` pins them together.
    pub(crate) fn eff_scroll(
        &self,
        vx: f64,
        vy: f64,
        scroll_x: f64,
        scroll_y: f64,
    ) -> (f64, f64) {
        (
            if vx < self.band_w { 0.0 } else { scroll_x },
            if vy < self.band_h { 0.0 } else { scroll_y },
        )
    }
}

/// One pinned-axis view segment of the quadrant paint: the frozen band (zero
/// scroll) and/or the scrolling remainder. All values are precomputed so the
/// painter consumes them verbatim — content culling windows and view clamps
/// use the exact expressions the pane-less painter used (byte-identity).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AxisSeg {
    /// View-space start of the segment's clip (the gutter edge, or the freeze
    /// boundary).
    pub(crate) view0: f64,
    /// View-space FAR edge for the painter's min-clamps (the viewport edge for
    /// the scrolling segment; `view0 + len` for the pinned band).
    pub(crate) view1: f64,
    /// Clip length (≥ 0; a zero-length segment is not emitted).
    pub(crate) len: f64,
    /// The scroll offset content painted in this segment is shifted by (0 for
    /// the pinned band).
    pub(crate) scroll: f64,
    /// Content-space start of the visible window (`content1 = content0 + len`).
    pub(crate) content0: f64,
    pub(crate) content1: f64,
    /// True for the frozen band (its rows come from the pinned band page).
    pub(crate) pinned: bool,
}

/// Split one axis of the grid viewport into its pane segments — the paint half
/// of the shared quadrant mapping. `origin` is the gutter edge (view space),
/// `view_end` the viewport extent along the axis, `band` the frozen-band size
/// (0 = unfrozen → ONE segment identical to the pane-less window math), and
/// `scroll` the axis scroll offset.
pub(crate) fn axis_segments(origin: f64, view_end: f64, band: f64, scroll: f64) -> Vec<AxisSeg> {
    let mut out = Vec::with_capacity(2);
    if band > 0.0 {
        let len = band.min((view_end - origin).max(0.0));
        if len > 0.0 {
            out.push(AxisSeg {
                view0: origin,
                view1: origin + len,
                len,
                scroll: 0.0,
                content0: 0.0,
                content1: len,
                pinned: true,
            });
        }
        let len = (view_end - (origin + band)).max(0.0);
        if len > 0.0 {
            out.push(AxisSeg {
                view0: origin + band,
                view1: view_end,
                len,
                scroll,
                content0: band + scroll,
                content1: band + scroll + len,
                pinned: false,
            });
        }
    } else {
        // The pane-less window: EXACT pane-less painter expressions.
        let len = (view_end - origin).max(0.0);
        out.push(AxisSeg {
            view0: origin,
            view1: view_end,
            len,
            scroll,
            content0: scroll,
            content1: scroll + len,
            pinned: false,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // A geom over a simple populated sheet (default 15pt rows, MDW 7).
    fn geom() -> IndexGeom {
        let xml = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:H40"/><sheetData><row r="1"><c r="A1"/></row><row r="40"><c r="H40"/></row></sheetData></worksheet>"#;
        let idx = crate::sml::index::SheetIndex::build("S".into(), xml.as_bytes().to_vec())
            .expect("index");
        idx.geom_v2(7.0, &[], &[])
    }

    #[test]
    fn bands_derive_from_the_shared_prefix_sums() {
        let g = geom();
        let p = PaneInfo { x_split: 2, y_split: 3, top_left: None };
        let b = PaneBands::build(&p, &g).expect("bands");
        assert_eq!(b.band_w, g.col_left(3), "band right edge = left of col xSplit+1");
        assert_eq!(b.band_h, g.row_top(4), "band bottom edge = top of row ySplit+1");
        assert_eq!(b.y_split, 3);
    }

    #[test]
    fn zero_splits_yield_no_bands() {
        let g = geom();
        assert!(PaneBands::build(&PaneInfo { x_split: 0, y_split: 0, top_left: None }, &g).is_none());
    }

    #[test]
    fn single_axis_freezes() {
        let g = geom();
        let rows_only =
            PaneBands::build(&PaneInfo { x_split: 0, y_split: 1, top_left: None }, &g).unwrap();
        assert_eq!(rows_only.band_w, 0.0);
        assert_eq!(rows_only.band_h, g.row_top(2));
        let cols_only =
            PaneBands::build(&PaneInfo { x_split: 1, y_split: 0, top_left: None }, &g).unwrap();
        assert_eq!(cols_only.band_h, 0.0);
        assert_eq!(cols_only.band_w, g.col_left(2));
    }

    #[test]
    fn oversized_splits_are_not_clamped() {
        // Faithful edge policy: ySplit beyond the extent pins past the content
        // (kill-the-void closed form), xSplit clamps only the RAIL LOOKUP.
        let g = geom();
        let b =
            PaneBands::build(&PaneInfo { x_split: 500, y_split: 100, top_left: None }, &g).unwrap();
        assert_eq!(b.band_w, g.col_left(g.max_col + 1), "whole grid width pinned");
        assert_eq!(b.band_h, g.row_top(101), "band extends past max_row at default height");
    }

    #[test]
    fn eff_scroll_pins_inside_bands_only() {
        let g = geom();
        let b = PaneBands::build(&PaneInfo { x_split: 2, y_split: 3, top_left: None }, &g).unwrap();
        let (sx, sy) = (120.0, 340.0);
        // Corner: both pinned.
        assert_eq!(b.eff_scroll(1.0, 1.0, sx, sy), (0.0, 0.0));
        // Top band: y pinned, x scrolls.
        assert_eq!(b.eff_scroll(b.band_w + 5.0, 1.0, sx, sy), (sx, 0.0));
        // Left band: x pinned, y scrolls.
        assert_eq!(b.eff_scroll(1.0, b.band_h + 5.0, sx, sy), (0.0, sy));
        // Body: both scroll.
        assert_eq!(b.eff_scroll(b.band_w + 5.0, b.band_h + 5.0, sx, sy), (sx, sy));
        // Boundary is the FIRST body pixel (bands are [0, band)).
        assert_eq!(b.eff_scroll(b.band_w, b.band_h, sx, sy), (sx, sy));
    }

    #[test]
    fn segments_and_eff_scroll_agree() {
        // The consistency pin: for sampled view points, the segment CONTAINING
        // the point (paint mapping) applies the same scroll `eff_scroll` (hit
        // mapping) resolves — the two halves of the shared helper cannot drift.
        let g = geom();
        let b = PaneBands::build(&PaneInfo { x_split: 2, y_split: 3, top_left: None }, &g).unwrap();
        let (gut, view_end, scroll) = (25.0, 480.0, 137.5);
        for (band, pick) in [
            (b.band_w, 0usize), // x axis
            (b.band_h, 1usize), // y axis
        ] {
            let segs = axis_segments(gut, view_end, band, scroll);
            assert_eq!(segs.len(), 2, "band + body");
            for v in [0.0, band / 2.0, band - 0.01, band, band + 0.01, view_end - gut] {
                let seg = segs
                    .iter()
                    .find(|s| gut + v >= s.view0 && gut + v < s.view0 + s.len)
                    .or(segs.last());
                let eff = b.eff_scroll(
                    if pick == 0 { v } else { f64::MAX },
                    if pick == 1 { v } else { f64::MAX },
                    scroll,
                    scroll,
                );
                let eff_axis = if pick == 0 { eff.0 } else { eff.1 };
                assert_eq!(seg.unwrap().scroll, eff_axis, "axis point {v}");
            }
        }
    }

    #[test]
    fn paneless_axis_is_one_segment_with_the_exact_window_math() {
        let segs = axis_segments(25.0, 480.0, 0.0, 60.0);
        assert_eq!(
            segs,
            vec![AxisSeg {
                view0: 25.0,
                view1: 480.0,
                len: (480.0f64 - 25.0).max(0.0),
                scroll: 60.0,
                content0: 60.0,
                content1: 60.0 + (480.0f64 - 25.0).max(0.0),
                pinned: false,
            }]
        );
    }

    #[test]
    fn band_wider_than_the_viewport_leaves_no_body_segment() {
        let segs = axis_segments(25.0, 100.0, 200.0, 60.0);
        assert_eq!(segs.len(), 1);
        assert!(segs[0].pinned);
        assert_eq!(segs[0].len, 75.0, "band clip stops at the viewport");
    }
}
