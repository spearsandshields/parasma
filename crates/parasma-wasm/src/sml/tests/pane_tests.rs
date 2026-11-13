//! SML frozen panes (grid view) — native tests.
//!
//! Three guarantees are pinned here:
//!  1. BYTE-IDENTITY for pane-less sheets: the windowed grid painter's exact
//!     display-list hash is pinned (captured BEFORE the quadrant refactor) —
//!     any pane-gated branch that leaks into the pane-less path moves it.
//!  2. The quadrant mapping (ONE shared helper, `sml::pane`) is consistent
//!     between hit-test (`eff_scroll`) and paint (`axis_segments`) by
//!     construction AND by test.
//!  3. Frozen-band behavior: pinned rows/cols paint at zero scroll along the
//!     frozen axis, the freeze boundary line is present, selection spanning
//!     the boundary tints in every quadrant it intersects, and hit-tests in a
//!     frozen band resolve the pinned row/col regardless of scroll.

use std::collections::HashMap;

use crate::engine::displaylist::{DrawCmd, RecordingPainter};
use crate::engine::fonts::FontCache;
use crate::sml::grid;
use crate::sml::render;
use crate::sml::SmlRenderer;

use super::{calibri_font_cache, merge_tests, sst, xlsx_with, ORACLE_SHEET_XML};

const NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

fn renderer(bytes: Vec<u8>) -> SmlRenderer {
    SmlRenderer::from_xlsx(bytes).expect("from_xlsx")
}

/// FNV-1a over the debug form of a recorded display list — the paint-stream
/// hash the byte-identity pins assert on.
fn ops_hash(cmds: &[DrawCmd]) -> u64 {
    let s = format!("{cmds:?}");
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Record one windowed grid paint of sheet 0 (the production `paint_grid_viewport`
/// wiring: `grid_window` → `window_page` → `render::paint_grid_viewport`).
pub(super) fn paint_ops(
    r: &SmlRenderer,
    fc: &FontCache,
    scroll_x: f64,
    scroll_y: f64,
    view_w: f64,
    view_h: f64,
    cell_sel: Option<crate::sml::select::CellSelection>,
) -> Vec<DrawCmd> {
    let gs = r.ensure_geom(0, fc).expect("geom");
    let geom = &gs.geom;
    let gut_w = grid::row_gutter_width_for_max_row(geom.max_row);
    let gut_h = grid::COL_GUTTER_H;
    let (r0, r1, win_x1) = crate::sml::grid_window(geom, scroll_x, scroll_y, view_w, view_h);
    let page = r.window_page(0, fc, r0, r1, win_x1).expect("window page");
    let csp = cell_sel.map(|sel| {
        crate::sml::select::cell_sel_paint(geom, &r.indexes[0].merges, &sel)
    });
    let rec = RecordingPainter::new();
    render::paint_grid_viewport(
        &rec,
        &page,
        (gut_w, gut_h),
        scroll_x,
        scroll_y,
        view_w,
        view_h,
        r.normal_font(),
        fc,
        &HashMap::new(),
        &HashMap::new(),
        None,
        None,
        csp.as_ref(),
        None,
    )
    .expect("paint");
    rec.take()
}

// ─── 1) pane-less byte-identity pins (captured pre-refactor) ─────────────────

#[test]
fn paneless_paint_stream_pinned_oracle_fixture() {
    let fc = calibri_font_cache();
    let r = renderer(xlsx_with(ORACLE_SHEET_XML, &sst(&["Header", "Footer"])));
    let unscrolled = ops_hash(&paint_ops(&r, &fc, 0.0, 0.0, 640.0, 480.0, None));
    let scrolled = ops_hash(&paint_ops(&r, &fc, 37.0, 61.5, 640.0, 480.0, None));
    if option_env!("PIN").is_some() {
        eprintln!("oracle unscrolled=0x{unscrolled:016x} scrolled=0x{scrolled:016x}");
    }
    assert_eq!(unscrolled, 0xc90b7b5bf52bcb16, "pane-less paint stream moved (unscrolled)");
    assert_eq!(scrolled, 0x481d62fa758c1038, "pane-less paint stream moved (scrolled)");
}

// ─── 2) pane parsing onto the SheetIndex ─────────────────────────────────────

/// A 6×30 sheet with text in A1/A2/F1 and the given `<pane…/>` XML (empty =
/// no pane element).
fn frozen_sheet_xml(pane_xml: &str) -> String {
    let views = if pane_xml.is_empty() {
        String::new()
    } else {
        format!(r#"<sheetViews><sheetView workbookViewId="0">{pane_xml}</sheetView></sheetViews>"#)
    };
    let mut rows = String::new();
    for r in 1..=30 {
        let extra = if r == 1 { r#"<c r="F1" t="s"><v>2</v></c>"# } else { "" };
        let s = if r == 1 { 0 } else { 1 };
        rows.push_str(&format!(r#"<row r="{r}"><c r="A{r}" t="s"><v>{s}</v></c>{extra}</row>"#));
    }
    format!(
        r#"<?xml version="1.0"?><worksheet xmlns="{NS}"><dimension ref="A1:F30"/>{views}<sheetFormatPr defaultRowHeight="15"/><sheetData>{rows}</sheetData></worksheet>"#
    )
}

const FROZEN_STRINGS: &[&str] = &["Frozen", "Body", "Wide"];

fn frozen_renderer(pane_xml: &str) -> SmlRenderer {
    renderer(xlsx_with(&frozen_sheet_xml(pane_xml), &sst(FROZEN_STRINGS)))
}

#[test]
fn frozen_pane_parsed_onto_the_index() {
    let r = frozen_renderer(
        r#"<pane xSplit="2" ySplit="1" topLeftCell="C2" activePane="bottomRight" state="frozen"/>"#,
    );
    assert_eq!(
        r.indexes[0].pane,
        Some(crate::sml::pane::PaneInfo { x_split: 2, y_split: 1, top_left: Some((2, 3)) })
    );
}

#[test]
fn frozen_split_state_honors_the_frozen_part() {
    let r = frozen_renderer(r#"<pane ySplit="3" topLeftCell="A4" state="frozenSplit"/>"#);
    assert_eq!(
        r.indexes[0].pane,
        Some(crate::sml::pane::PaneInfo { x_split: 0, y_split: 3, top_left: Some((4, 1)) })
    );
}

#[test]
fn split_state_and_absent_pane_are_ignored() {
    // state="split": xSplit/ySplit are 1/20-pt units of a draggable splitter —
    // deliberately NO pane.
    let r = frozen_renderer(r#"<pane xSplit="2000" ySplit="600" state="split"/>"#);
    assert_eq!(r.indexes[0].pane, None);
    // No state attribute at all → split semantics → ignored.
    let r = frozen_renderer(r#"<pane xSplit="1" ySplit="1"/>"#);
    assert_eq!(r.indexes[0].pane, None);
    // No pane element.
    let r = frozen_renderer("");
    assert_eq!(r.indexes[0].pane, None);
}

#[test]
fn weird_split_values_are_guarded() {
    // Negative → 0; both-zero → None; fractional truncates.
    let r = frozen_renderer(r#"<pane xSplit="-3" ySplit="-1" state="frozen"/>"#);
    assert_eq!(r.indexes[0].pane, None);
    let r = frozen_renderer(r#"<pane xSplit="2.7" ySplit="0" state="frozen"/>"#);
    assert_eq!(
        r.indexes[0].pane,
        Some(crate::sml::pane::PaneInfo { x_split: 2, y_split: 0, top_left: None })
    );
}

// ─── 3) quadrant hit-tests (pinned axes under scroll) ────────────────────────

#[test]
fn hit_in_the_frozen_band_resolves_the_pinned_row_regardless_of_scroll() {
    let fc = calibri_font_cache();
    let r = frozen_renderer(r#"<pane ySplit="1" topLeftCell="A2" state="frozen"/>"#);
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let gw = grid::row_gutter_width_for_max_row(gs.geom.max_row);
    let gh = grid::COL_GUTTER_H;
    let band_h = gs.geom.row_top(2); // one default row
    let scroll_y = 10.0 * gs.geom.default_row_px;

    // Inside the pinned band: row 1, whatever the scroll.
    let hit = r.grid_hit_test(0, gw + 5.0, gh + band_h / 2.0, 0.0, scroll_y, &fc).unwrap();
    assert_eq!((hit.zone, hit.row), (crate::sml::select::GridZone::Cell, 1));

    // 1px above the freeze boundary: still pinned.
    let hit = r.grid_hit_test(0, gw + 5.0, gh + band_h - 1.0, 0.0, scroll_y, &fc).unwrap();
    assert_eq!(hit.row, 1, "just above the boundary is the pinned band");

    // 1px below the boundary: the BODY — the scrolled row beyond the band.
    let hit = r.grid_hit_test(0, gw + 5.0, gh + band_h + 1.0, 0.0, scroll_y, &fc).unwrap();
    let expected = gs.geom.row_at_y(band_h + 1.0 + scroll_y);
    assert_eq!(hit.row, expected, "just below the boundary scrolls");
    assert!(hit.row > 1);

    // Unscrolled body point right below the band = row 2 (the seam is seamless).
    let hit = r.grid_hit_test(0, gw + 5.0, gh + band_h + 1.0, 0.0, 0.0, &fc).unwrap();
    assert_eq!(hit.row, 2);
}

#[test]
fn hit_in_the_frozen_col_band_resolves_the_pinned_col() {
    let fc = calibri_font_cache();
    let r = frozen_renderer(r#"<pane xSplit="1" topLeftCell="B1" state="frozen"/>"#);
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let gw = grid::row_gutter_width_for_max_row(gs.geom.max_row);
    let gh = grid::COL_GUTTER_H;
    let band_w = gs.geom.col_left(2);
    let scroll_x = 3.0 * band_w;
    let hit = r.grid_hit_test(0, gw + band_w / 2.0, gh + 5.0, scroll_x, 0.0, &fc).unwrap();
    assert_eq!(hit.col, 1, "pinned column A under x scroll");
    let hit = r.grid_hit_test(0, gw + band_w + 1.0, gh + 5.0, scroll_x, 0.0, &fc).unwrap();
    assert_eq!(hit.col, gs.geom.col_at_x(band_w + 1.0 + scroll_x), "body col scrolls");
}

#[test]
fn grid_cell_at_reads_the_pinned_cell_under_scroll() {
    let fc = calibri_font_cache();
    let r = frozen_renderer(r#"<pane ySplit="1" topLeftCell="A2" state="frozen"/>"#);
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let gw = grid::row_gutter_width_for_max_row(gs.geom.max_row);
    let gh = grid::COL_GUTTER_H;
    let scroll_y = 8.0 * gs.geom.default_row_px;
    // A viewport point over the pinned band returns A1 even when scrolled deep.
    let (a1, _, text) = r.grid_cell_at(0, gw + 3.0, gh + 3.0, 0.0, scroll_y, &fc).expect("cell");
    assert_eq!(a1, "A1");
    assert_eq!(text, "Frozen");
}

// ─── 4) frozen paint: pinned rows, boundary line, cross-boundary selection ──

/// Record a paint WITH the production pane wiring (`pane_paint_parts`).
fn paint_ops_frozen(
    r: &SmlRenderer,
    fc: &FontCache,
    scroll_x: f64,
    scroll_y: f64,
    view_w: f64,
    view_h: f64,
    cell_sel: Option<crate::sml::select::CellSelection>,
) -> Vec<DrawCmd> {
    let gs = r.ensure_geom(0, fc).expect("geom");
    let geom = &gs.geom;
    let gut_w = grid::row_gutter_width_for_max_row(geom.max_row);
    let gut_h = grid::COL_GUTTER_H;
    let (r0, r1, win_x1) = crate::sml::grid_window(geom, scroll_x, scroll_y, view_w, view_h);
    let page = r.window_page(0, fc, r0, r1, win_x1).expect("window page");
    let parts = r.pane_paint_parts(0, fc, scroll_x, view_w, view_h, win_x1);
    let pane_ctx = parts
        .as_ref()
        .map(|(b, bp)| render::PaneCtx { bands: *b, band_page: bp.as_ref() });
    let csp = cell_sel
        .map(|sel| crate::sml::select::cell_sel_paint(geom, &r.indexes[0].merges, &sel));
    let rec = RecordingPainter::new();
    render::paint_grid_viewport(
        &rec,
        &page,
        (gut_w, gut_h),
        scroll_x,
        scroll_y,
        view_w,
        view_h,
        r.normal_font(),
        fc,
        &HashMap::new(),
        &HashMap::new(),
        None,
        None,
        csp.as_ref(),
        pane_ctx.as_ref(),
    )
    .expect("paint");
    rec.take()
}

#[test]
fn frozen_row_cells_paint_pinned_while_the_body_scrolls() {
    let fc = calibri_font_cache();
    let r = frozen_renderer(r#"<pane ySplit="1" topLeftCell="A2" state="frozen"/>"#);
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let gut_h = grid::COL_GUTTER_H;
    let band_h = gs.geom.row_top(2);
    let scroll_y = 10.0 * gs.geom.default_row_px;

    let cmds = paint_ops_frozen(&r, &fc, 0.0, scroll_y, 640.0, 480.0, None);
    // The pinned row's text ("Frozen") paints INSIDE the band despite the
    // scroll (its unpinned position would be far above the viewport).
    let frozen_y: Vec<f64> = cmds
        .iter()
        .filter_map(|c| match c {
            DrawCmd::FillText { text, y, .. } if text == "Frozen" => Some(*y),
            _ => None,
        })
        .collect();
    assert!(!frozen_y.is_empty(), "the frozen row's cell text must paint");
    assert!(
        frozen_y.iter().all(|y| *y >= gut_h && *y <= gut_h + band_h),
        "frozen row text pinned inside the band: {frozen_y:?}"
    );
    // Body rows paint too (the scrolled window).
    assert!(
        cmds.iter().any(|c| matches!(c, DrawCmd::FillText { text, .. } if text == "Body")),
        "scrolled body rows still paint"
    );
    // The freeze boundary line is present (the darker rule).
    assert!(
        cmds.iter().any(|c| matches!(c, DrawCmd::SetStrokeStyle(s) if s == "#8a8a8a")),
        "freeze boundary line stroked"
    );
}

#[test]
fn paneless_sheet_never_paints_a_freeze_line() {
    let fc = calibri_font_cache();
    let r = frozen_renderer("");
    let cmds = paint_ops_frozen(&r, &fc, 0.0, 100.0, 640.0, 480.0, None);
    assert!(
        !cmds.iter().any(|c| matches!(c, DrawCmd::SetStrokeStyle(s) if s == "#8a8a8a")),
        "no pane → no freeze line"
    );
}

#[test]
fn selection_across_the_freeze_boundary_tints_every_quadrant() {
    let fc = calibri_font_cache();
    let r = frozen_renderer(r#"<pane xSplit="1" ySplit="1" topLeftCell="B2" state="frozen"/>"#);
    // A range spanning the boundary on BOTH axes (A1:C4, anchor A1).
    let sel = crate::sml::select::CellSelection {
        sheet: 0,
        range: crate::sml::select::CellRange::new(1, 1, 4, 3),
        anchor_row: 1,
        anchor_col: 1,
    };
    let cmds = paint_ops_frozen(&r, &fc, 30.0, 45.0, 640.0, 480.0, Some(sel));
    // paint_cell_selection opens with SetGlobalAlpha(SEL_TINT_ALPHA = 0.15):
    // one per quadrant → 4 with both axes frozen (corner, top, left, body).
    let tint_passes = cmds
        .iter()
        .filter(|c| matches!(c, DrawCmd::SetGlobalAlpha(a) if *a == 0.15))
        .count();
    assert_eq!(tint_passes, 4, "selection painted in all four quadrants");
}

// ─── 5) topLeftCell → initial body scroll (grid_pane_info) ───────────────────

#[test]
fn pane_info_derives_the_top_left_cell_body_scroll() {
    let fc = calibri_font_cache();
    let r = frozen_renderer(r#"<pane ySplit="1" topLeftCell="A50" state="frozen"/>"#);
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let band_h = gs.geom.row_top(2);
    let (info, bands, sx, sy) = r.grid_pane_info(0, &fc).expect("pane info");
    assert_eq!((info.x_split, info.y_split), (0, 1));
    assert_eq!(bands.band_h, band_h);
    assert_eq!(sx, 0.0);
    assert_eq!(sy, gs.geom.row_top(50) - band_h, "body scroll puts row 50 at the boundary");

    // Absent topLeftCell → A1 → zero scroll (current behavior preserved).
    let r = frozen_renderer(r#"<pane ySplit="1" state="frozen"/>"#);
    let (_, _, sx, sy) = r.grid_pane_info(0, &fc).expect("pane info");
    assert_eq!((sx, sy), (0.0, 0.0));

    // Pane-less sheet → None.
    let r = frozen_renderer("");
    assert!(r.grid_pane_info(0, &fc).is_none());
}

#[test]
fn paneless_paint_stream_pinned_merged_fixture_with_selection() {
    // Merges + an active cell selection: pins the merge suppression AND the
    // selection/header-tint passes of the pane-less painter.
    let fc = calibri_font_cache();
    let r = renderer(xlsx_with(merge_tests::MERGED_SHEET_XML, &sst(merge_tests::MERGED_STRINGS)));
    let sel = crate::sml::select::CellSelection {
        sheet: 0,
        range: crate::sml::select::CellRange::new(1, 1, 3, 2),
        anchor_row: 1,
        anchor_col: 1,
    };
    let h = ops_hash(&paint_ops(&r, &fc, 10.0, 20.0, 500.0, 400.0, Some(sel)));
    if option_env!("PIN").is_some() {
        eprintln!("merged+sel=0x{h:016x}");
    }
    assert_eq!(h, 0xbe50370e282821f1, "pane-less paint stream moved (merged + selection)");
}
