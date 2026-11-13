//! SML cell selection + copy (GRID VIEW ONLY).
//!
//! Geometry-only hit-testing, a single rectangular selection with a
//! distinct active cell (Excel-parity), merge expansion, and an INDEX-RANGE
//! extraction walk with the per-content-type copy cap. All coordinates are
//! grid CONTENT space (gutter-exclusive) — the engine is zoom-blind; the DOM grid
//! adapter owns the zoom/scroll transform.
//!
//! Byte-identity: the paint helpers here fire ONLY when a selection is set (the
//! `render::paint_grid_viewport` pass takes `Option`), and the hit-test /
//! content-bounds / extract entry points are pure READS — so a no-selection grid
//! render is byte-identical and the SML equivalence pins hold unchanged.

use super::grid::{row_gutter_width_for_max_row, COL_GUTTER_H};
use super::index::IndexGeom;
use super::model::MergeRegion;

/// Copy caps (per-content-type — this SML TEXT budget is DISTINCT from the
/// image copy budget, never summed). Two knobs guard two failure modes:
/// `MAX_COPY_BYTES` is the dense-data OOM guard; `MAX_COPY_ROWS` catches the
/// sparse-tall case (400k empty TSV lines are tiny in bytes so the byte budget
/// never trips — the row cap does).
pub(crate) const MAX_COPY_ROWS: u32 = 50_000;
pub(crate) const MAX_COPY_BYTES: usize = 8 * 1024 * 1024;

/// A rectangular cell range — 1-based, inclusive, normalized (`r0 <= r1`,
/// `c0 <= c1`). The SML analogue of the WML `TextPos` range currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellRange {
    pub(crate) r0: u32,
    pub(crate) c0: u32,
    pub(crate) r1: u32,
    pub(crate) c1: u32,
}

impl CellRange {
    /// Normalize an anchor→focus pair into an inclusive rect.
    pub(crate) fn new(ar: u32, ac: u32, br: u32, bc: u32) -> Self {
        CellRange { r0: ar.min(br), c0: ac.min(bc), r1: ar.max(br), c1: ac.max(bc) }
    }

    /// True when this rect and a merge region share at least one cell.
    fn overlaps_merge(&self, m: &MergeRegion) -> bool {
        !(m.max_row < self.r0 || m.min_row > self.r1 || m.max_col < self.c0 || m.min_col > self.c1)
    }
}

/// A stored SML cell selection (`Copy` — lives in a `Cell` on the Viewer beside
/// the WML `selection`). `range` is the RAW drag/normalized rect; merge expansion
/// is applied at every CONSUMPTION (paint + extract), so expansion stays in one
/// place. `anchor_row`/`anchor_col` is the ACTIVE cell (rendered distinctly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellSelection {
    pub(crate) sheet: u32,
    pub(crate) range: CellRange,
    pub(crate) anchor_row: u32,
    pub(crate) anchor_col: u32,
}

/// The grid zone a point lands in (geometry-only). `Corner` is the top-left
/// select-all box — a NO-OP for selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GridZone {
    Cell,
    RowHeader,
    ColHeader,
    Corner,
}

impl GridZone {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            GridZone::Cell => "cell",
            GridZone::RowHeader => "rowHeader",
            GridZone::ColHeader => "colHeader",
            GridZone::Corner => "corner",
        }
    }
}

/// The result of [`grid_hit_test`]: the zone + a valid 1-based row/col (valid even
/// on an empty/model-less cell) + the merge anchor when the point lands
/// inside a merge (so the active cell can snap to it).
pub(crate) struct GridHit {
    pub(crate) zone: GridZone,
    pub(crate) row: u32,
    pub(crate) col: u32,
    pub(crate) merge_anchor: Option<(u32, u32)>,
}

/// Geometry-only grid hit-test. `view_x`/`view_y` are viewport-relative
/// CONTENT-space px (the adapter divides CSS px by zoom); `scroll_x`/`scroll_y`
/// are the content scroll offsets. Mirrors `SmlRenderer::grid_cell_at`'s gutter +
/// scroll math EXACTLY, but resolves zone/row/col purely from `IndexGeom`
/// (`row_at_y`/`col_at_x`, closed-form) — cell existence is NOT required, so a
/// drag anchored on an empty cell (extremely common) keeps a valid anchor.
///
/// `pane` (frozen panes, §18.3.1.66): a point inside a pinned band resolves
/// with ZERO scroll along the frozen axis, via the SAME
/// [`super::pane::PaneBands::eff_scroll`] quadrant mapping the painter's
/// segments derive from — hit and paint cannot diverge. `None` = identity.
pub(crate) fn grid_hit_test(
    geom: &IndexGeom,
    merges: &[MergeRegion],
    pane: Option<&super::pane::PaneBands>,
    view_x: f64,
    view_y: f64,
    scroll_x: f64,
    scroll_y: f64,
) -> GridHit {
    let gw = row_gutter_width_for_max_row(geom.max_row);
    let gh = COL_GUTTER_H;
    let in_left = view_x < gw;
    let in_top = view_y < gh;
    // Frozen panes: zero the scroll along a pinned axis (quadrant mapping).
    let (scroll_x, scroll_y) = match pane {
        Some(b) => b.eff_scroll(view_x - gw, view_y - gh, scroll_x, scroll_y),
        None => (scroll_x, scroll_y),
    };
    // Content coords, clamped ≥ 0 so a header click (negative in the other axis)
    // still maps to a valid row/col along its axis.
    let x = (view_x - gw + scroll_x).max(0.0);
    let y = (view_y - gh + scroll_y).max(0.0);
    let row = geom.row_at_y(y);
    let col = geom.col_at_x(x);
    let zone = match (in_left, in_top) {
        (true, true) => GridZone::Corner,
        (true, false) => GridZone::RowHeader,
        (false, true) => GridZone::ColHeader,
        (false, false) => GridZone::Cell,
    };
    let merge_anchor = if zone == GridZone::Cell {
        merges.iter().find(|m| m.contains(row, col)).map(|m| (m.min_row, m.min_col))
    } else {
        None
    };
    GridHit { zone, row, col, merge_anchor }
}

/// Expand a range to fully contain every merge it overlaps ("a range
/// containing a merge expands to include all merged regions it overlaps"). A
/// bounded fixpoint: each pass can only GROW the rect (monotone, capped at the
/// sheet limits), and growing may newly overlap another merge, so iterate until
/// stable. Real sheets have few small merges, so this settles in one or two
/// passes.
pub(crate) fn expand_for_merges(range: CellRange, merges: &[MergeRegion]) -> CellRange {
    let mut r = range;
    loop {
        let mut grew = false;
        for m in merges {
            if r.overlaps_merge(m) {
                let nr = CellRange {
                    r0: r.r0.min(m.min_row),
                    c0: r.c0.min(m.min_col),
                    r1: r.r1.max(m.max_row),
                    c1: r.c1.max(m.max_col),
                };
                if nr != r {
                    r = nr;
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    r
}

/// Content-space paint geometry for a cell selection, built where the `IndexGeom`
/// lives (`SmlRenderer::paint_grid_viewport`) so `render.rs` stays geometry-dumb.
/// All coords are grid CONTENT space (gutter-exclusive); the painter adds the
/// content origin shift.
pub(crate) struct CellSelPaint {
    /// The (merge-expanded) range outer box.
    pub(crate) x0: f64,
    pub(crate) y0: f64,
    pub(crate) x1: f64,
    pub(crate) y1: f64,
    /// The active cell box — the anchor, expanded through its own merge.
    pub(crate) ax0: f64,
    pub(crate) ay0: f64,
    pub(crate) ax1: f64,
    pub(crate) ay1: f64,
}

/// Build the content-space paint geometry for a stored selection over `geom`.
/// Columns clamp to the geom's rail extent (`col_left` indexes a flat rails vec);
/// rows use the closed-form `row_top` (valid for any row, incl. the kill-the-void
/// area past the extent).
pub(crate) fn cell_sel_paint(
    geom: &IndexGeom,
    merges: &[MergeRegion],
    sel: &CellSelection,
) -> CellSelPaint {
    let rng = expand_for_merges(sel.range, merges);
    // `col_left` requires `min_col <= c <= max_col + 1`; clamp defensively.
    let cx = |c: u32| geom.col_left(c.clamp(geom.min_col, geom.max_col + 1));

    // Active cell: the anchor, expanded through its own merge (Excel renders the
    // whole merge as the active box when a merged cell is active).
    let (mut ar0, mut ac0, mut ar1, mut ac1) =
        (sel.anchor_row, sel.anchor_col, sel.anchor_row, sel.anchor_col);
    if let Some(m) = merges.iter().find(|m| m.contains(sel.anchor_row, sel.anchor_col)) {
        ar0 = m.min_row;
        ac0 = m.min_col;
        ar1 = m.max_row;
        ac1 = m.max_col;
    }

    CellSelPaint {
        x0: cx(rng.c0),
        y0: geom.row_top(rng.r0),
        x1: cx(rng.c1 + 1),
        y1: geom.row_top(rng.r1 + 1),
        ax0: cx(ac0),
        ay0: geom.row_top(ar0),
        ax1: cx(ac1 + 1),
        ay1: geom.row_top(ar1 + 1),
    }
}

// ─── Extraction output (the flavor-neutral SML fragment + the cap flags) ─────

/// One cell in an [`CellFragment`] row. `display` is the formatted `CellModel.text`
/// (the displayed value; raw values are not copied); model-less cells emit an EMPTY string.
/// `covered` marks a merge's non-anchor cell (TSV emits it as an empty column so
/// alignment is preserved; the HTML `<table>` skips it under the anchor's span).
pub(crate) struct CellOut {
    pub(crate) display: String,
    pub(crate) rowspan: u32,
    pub(crate) colspan: u32,
    pub(crate) covered: bool,
}

/// The `docExtractRange` cells-arm result: the emitted cell grid + the cap signal.
/// `row_count` is what was emitted, `total_rows` the full requested rect height;
/// `truncated`/`reason` drive the "Copied first N of M rows" signal.
pub(crate) struct CellFragment {
    pub(crate) rows: Vec<Vec<CellOut>>,
    pub(crate) row_count: u32,
    pub(crate) total_rows: u32,
    pub(crate) truncated: bool,
    pub(crate) reason: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // A geom over a default 10-col × 25-row empty sheet (empty-sheet synthesis) built
    // through the real index path, so the gutter width + rails match production.
    fn empty_geom() -> IndexGeom {
        let idx = crate::sml::index::SheetIndex::build(
            "S".into(),
            b"<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData/></worksheet>".to_vec(),
        )
        .expect("index");
        idx.geom_v2(7.0, &[], &[])
    }

    // A geom over a small populated sheet with a couple of merges.
    fn merged_geom() -> (IndexGeom, Vec<MergeRegion>) {
        let xml = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:C7"/><sheetData><row r="1"><c r="A1"/></row><row r="7"><c r="C7"/></row></sheetData><mergeCells count="2"><mergeCell ref="A1:C1"/><mergeCell ref="B3:C4"/></mergeCells></worksheet>"#;
        let idx = crate::sml::index::SheetIndex::build("S".into(), xml.as_bytes().to_vec())
            .expect("index");
        let merges = idx.merges.clone();
        (idx.geom_v2(7.0, &[], &[]), merges)
    }

    #[test]
    fn zones_resolve_from_gutter_geometry() {
        let geom = empty_geom();
        let gw = row_gutter_width_for_max_row(geom.max_row);
        // Deep in the content: a cell.
        let h = grid_hit_test(&geom, &[], None, gw + 100.0, COL_GUTTER_H + 100.0, 0.0, 0.0);
        assert_eq!(h.zone, GridZone::Cell);
        assert!(h.row >= 1 && h.col >= 1);
        // Left gutter, below the corner: a row header.
        let h = grid_hit_test(&geom, &[], None, gw - 2.0, COL_GUTTER_H + 100.0, 0.0, 0.0);
        assert_eq!(h.zone, GridZone::RowHeader);
        // Top gutter, right of the corner: a column header.
        let h = grid_hit_test(&geom, &[], None, gw + 100.0, COL_GUTTER_H - 2.0, 0.0, 0.0);
        assert_eq!(h.zone, GridZone::ColHeader);
        // The corner box: a no-op zone.
        let h = grid_hit_test(&geom, &[], None, gw - 2.0, COL_GUTTER_H - 2.0, 0.0, 0.0);
        assert_eq!(h.zone, GridZone::Corner);
    }

    #[test]
    fn gutter_edge_is_the_first_content_column() {
        let geom = empty_geom();
        let gw = row_gutter_width_for_max_row(geom.max_row);
        // Exactly at the gutter edge is the FIRST content cell (not a header).
        let h = grid_hit_test(&geom, &[], None, gw, COL_GUTTER_H, 0.0, 0.0);
        assert_eq!(h.zone, GridZone::Cell);
        assert_eq!((h.row, h.col), (1, 1), "top-left content cell is A1");
    }

    #[test]
    fn empty_cell_still_yields_a_valid_anchor() {
        // A hit on a model-less cell (this sheet has no cells) must resolve a
        // real row/col so a drag anchor never dissolves.
        let geom = empty_geom();
        let gw = row_gutter_width_for_max_row(geom.max_row);
        let h = grid_hit_test(&geom, &[], None, gw + 200.0, COL_GUTTER_H + 60.0, 0.0, 0.0);
        assert_eq!(h.zone, GridZone::Cell);
        assert!(h.row >= 1 && h.col >= 1);
        assert!(h.merge_anchor.is_none());
    }

    #[test]
    fn scroll_offsets_shift_the_resolved_cell() {
        let geom = empty_geom();
        let gw = row_gutter_width_for_max_row(geom.max_row);
        let unscrolled = grid_hit_test(&geom, &[], None, gw + 10.0, COL_GUTTER_H + 10.0, 0.0, 0.0);
        // Scroll down by ~4 default rows and the same viewport point resolves lower.
        let scrolled =
            grid_hit_test(&geom, &[], None, gw + 10.0, COL_GUTTER_H + 10.0, 0.0, geom.default_row_px * 4.0);
        assert!(scrolled.row > unscrolled.row);
    }

    #[test]
    fn hit_inside_a_merge_reports_its_anchor() {
        let (geom, merges) = merged_geom();
        let gw = row_gutter_width_for_max_row(geom.max_row);
        // A1:C1 merge — click somewhere in the second column of row 1.
        let x = gw + geom.col_left(2) + 2.0;
        let h = grid_hit_test(&geom, &merges, None, x, COL_GUTTER_H + 2.0, 0.0, 0.0);
        assert_eq!(h.zone, GridZone::Cell);
        assert_eq!(h.row, 1);
        assert_eq!(h.merge_anchor, Some((1, 1)), "any cell in A1:C1 anchors at A1");
    }

    #[test]
    fn expand_for_merges_grows_to_cover_overlaps() {
        let merges = vec![
            MergeRegion { min_row: 1, min_col: 1, max_row: 1, max_col: 3 }, // A1:C1
            MergeRegion { min_row: 3, min_col: 2, max_row: 4, max_col: 3 }, // B3:C4
        ];
        // A single cell inside A1:C1 expands to the whole A1:C1.
        let r = expand_for_merges(CellRange::new(1, 2, 1, 2), &merges);
        assert_eq!(r, CellRange { r0: 1, c0: 1, r1: 1, c1: 3 });
        // A rect grazing B3 pulls in all of B3:C4.
        let r = expand_for_merges(CellRange::new(3, 1, 3, 2), &merges);
        assert_eq!(r, CellRange { r0: 3, c0: 1, r1: 4, c1: 3 });
    }

    #[test]
    fn expand_for_merges_is_a_noop_without_overlap() {
        let merges = vec![MergeRegion { min_row: 10, min_col: 10, max_row: 12, max_col: 12 }];
        let r = expand_for_merges(CellRange::new(1, 1, 2, 2), &merges);
        assert_eq!(r, CellRange { r0: 1, c0: 1, r1: 2, c1: 2 });
    }

    #[test]
    fn active_cell_expands_through_its_merge() {
        let (geom, merges) = merged_geom();
        let sel = CellSelection {
            sheet: 0,
            range: CellRange::new(1, 1, 4, 3),
            anchor_row: 1,
            anchor_col: 2, // inside A1:C1
        };
        let p = cell_sel_paint(&geom, &merges, &sel);
        // The active box spans A1:C1 (cols 1..=3), so its left is col 1's left and
        // its right is col 4's left.
        assert!((p.ax0 - geom.col_left(1)).abs() < 1e-9);
        assert!((p.ax1 - geom.col_left(4)).abs() < 1e-9);
        assert!((p.ay0 - geom.row_top(1)).abs() < 1e-9);
        assert!((p.ay1 - geom.row_top(2)).abs() < 1e-9);
    }
}
