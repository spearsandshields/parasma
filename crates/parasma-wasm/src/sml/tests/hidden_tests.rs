//! SML HIDDEN ROWS/COLS — native tests.
//!
//! The MECHANISM under test: a hidden column/row contributes ZERO width/height
//! to the SAME IndexGeom prefix sums everything reads, so every downstream
//! consumer (windowed paint, docHitTest, frozen-pane bands, selection view
//! mapping, merges, print plans) inherits hiding with no per-consumer changes.
//! Cell INDICES are untouched: copy stays an index-range walk (hidden cells
//! are INCLUDED, as Excel copies them — proven here), and search skips hidden
//! engine-side (Excel Find's default). Byte-identity for hidden-less sheets is
//! pinned by the UNCHANGED pane/oracle pins in `pane_tests`/`window_tests`.

use std::collections::HashMap;

use crate::engine::displaylist::{DrawCmd, RecordingPainter};
use crate::engine::fonts::FontCache;
use crate::sml::grid;
use crate::sml::render;
use crate::sml::select::CellRange;
use crate::sml::SmlRenderer;
use crate::SearchOpts;

use super::window_tests::{eager_continuous_hash, full_window_hash};
use super::{calibri_font_cache, sst, xlsx_with};

const NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

/// The hidden fixture: col C hidden (`hidden="1"`), col E hidden via the Excel
/// equivalence `width="0" customWidth="1"`, rows 4+5 hidden (`hidden="1"` and
/// the `ht="0" customHeight="1"` equivalence respectively) — with VALUES inside
/// the hidden cells so the copy/search tests bite. sst: 0="vis", 1="ghost",
/// 2="cee".
fn hidden_sheet_xml() -> String {
    format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="{NS}">
  <dimension ref="A1:F8"/>
  <sheetFormatPr defaultRowHeight="15"/>
  <cols>
    <col min="3" max="3" width="10" hidden="1" customWidth="1"/>
    <col min="5" max="5" width="0" customWidth="1"/>
  </cols>
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="n"><v>11</v></c><c r="C1" t="s"><v>2</v></c><c r="D1" t="n"><v>12</v></c></row>
    <row r="2"><c r="B2" t="n"><v>22</v></c><c r="C2" t="n"><v>33</v></c><c r="D2" t="n"><v>44</v></c></row>
    <row r="3"><c r="C3" t="s"><v>2</v></c></row>
    <row r="4" hidden="1"><c r="A4" t="s"><v>1</v></c></row>
    <row r="5" ht="0" customHeight="1"><c r="A5" t="s"><v>1</v></c></row>
    <row r="8"><c r="A8" t="s"><v>0</v></c><c r="F8" t="n"><v>55</v></c></row>
  </sheetData>
</worksheet>"#
    )
}

const HIDDEN_STRINGS: &[&str] = &["vis", "ghost", "cee"];

fn hidden_renderer() -> SmlRenderer {
    SmlRenderer::from_xlsx(xlsx_with(&hidden_sheet_xml(), &sst(HIDDEN_STRINGS)))
        .expect("from_xlsx")
}

// ─── 1) parse: the hidden flags land on the index/model ─────────────────────

#[test]
fn hidden_flags_parse_with_the_excel_equivalences() {
    let r = hidden_renderer();
    let idx = &r.indexes[0];
    // Columns: @hidden and width-0+customWidth both read hidden.
    assert!(idx.col_hidden(3), "col C: hidden=\"1\"");
    assert!(idx.col_hidden(5), "col E: width=\"0\" customWidth — Excel-equivalent hidden");
    for c in [1u32, 2, 4, 6] {
        assert!(!idx.col_hidden(c), "col {c} is visible");
    }
    // Rows: @hidden and ht=0+customHeight both read hidden.
    let hidden_rows: Vec<u32> = idx.rows.iter().filter(|e| e.hidden).map(|e| e.row).collect();
    assert_eq!(hidden_rows, vec![4, 5]);
}

#[test]
fn zero_equivalence_guards_float_dust_and_plain_zero() {
    // width="0.0000001"+customWidth is dust → hidden; ht="0" WITHOUT
    // customHeight is NOT the Excel equivalence (stays a plain 0pt height).
    let xml = format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="{NS}">
  <dimension ref="A1:C3"/>
  <cols>
    <col min="2" max="2" width="0.0000001" customWidth="1"/>
    <col min="3" max="3" width="0"/>
  </cols>
  <sheetData>
    <row r="1" ht="0"><c r="A1" t="n"><v>1</v></c></row>
  </sheetData>
</worksheet>"#
    );
    let r = SmlRenderer::from_xlsx(xlsx_with(&xml, &sst(&[]))).expect("from_xlsx");
    let idx = &r.indexes[0];
    assert!(idx.col_hidden(2), "float-dust width + customWidth is hidden");
    assert!(!idx.col_hidden(3), "width=0 WITHOUT customWidth is not the equivalence");
    assert!(!idx.rows[0].hidden, "ht=0 WITHOUT customHeight is not the equivalence");
}

// ─── 2) geometry: zero contribution to the shared prefix sums ────────────────

#[test]
fn hidden_cols_and_rows_contribute_zero_to_the_rails() {
    let fc = calibri_font_cache();
    let r = hidden_renderer();
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let g = &gs.geom;
    // Hidden col C collapses: D's left edge == C's left edge.
    assert_eq!(g.col_left(4), g.col_left(3), "col C is zero-width");
    // Hidden col E (width-0 equivalence) collapses too.
    assert_eq!(g.col_left(6), g.col_left(5), "col E is zero-width");
    // Visible cols keep real widths.
    assert!(g.col_left(3) > g.col_left(2), "col B has width");
    // Hidden rows 4+5 collapse: row 6's top == row 4's top.
    assert_eq!(g.row_top(5), g.row_top(4), "row 4 is zero-height");
    assert_eq!(g.row_top(6), g.row_top(4), "row 5 is zero-height");
    assert!(g.row_top(4) > g.row_top(3), "row 3 keeps its height");
    // The whole extent shrinks by exactly the hidden contributions: 8 rows at
    // default height minus the two hidden rows.
    assert_eq!(g.content_h, 6.0 * g.default_row_px, "6 visible rows of 8");
}

#[test]
fn boundary_hits_resolve_the_next_visible_row_and_col() {
    let fc = calibri_font_cache();
    let r = hidden_renderer();
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let g = &gs.geom;
    // x exactly on the collapsed C boundary → the NEXT visible column D.
    assert_eq!(g.col_at_x(g.col_left(3)), 4, "click where hidden C sits lands on D");
    // y exactly on the collapsed rows-4/5 boundary → the next visible row 6.
    assert_eq!(g.row_at_y(g.row_top(4)), 6, "click where hidden 4-5 sit lands on row 6");
    // The full production hit-test path agrees (grid_hit_test → same sums).
    let gw = grid::row_gutter_width_for_max_row(g.max_row);
    let gh = grid::COL_GUTTER_H;
    let hit = r
        .grid_hit_test(0, gw + g.col_left(3), gh + g.row_top(4), 0.0, 0.0, &fc)
        .expect("hit");
    assert_eq!((hit.col, hit.row), (4, 6), "docHitTest inherits the zero-width sums");
}

// ─── 3) equivalence: v1 eager model path == v2 index path, hidden-aware ─────

#[test]
fn v2_window_matches_eager_continuous_on_the_hidden_fixture() {
    let fc = calibri_font_cache();
    let bytes = xlsx_with(&hidden_sheet_xml(), &sst(HIDDEN_STRINGS));
    let r = SmlRenderer::from_xlsx(bytes.clone()).expect("from_xlsx");
    assert_eq!(
        full_window_hash(&r, &fc),
        eager_continuous_hash(bytes, &r, &fc),
        "hidden geometry must be identical on the v1 model path and the v2 index path"
    );
}

// ─── 4) frozen-pane interplay: hidden inside a band shrinks it ───────────────

#[test]
fn hidden_col_inside_a_frozen_band_shrinks_the_band() {
    let fc = calibri_font_cache();
    // Freeze the first 3 columns; col 2 is hidden → the band spans only the
    // visible widths of cols 1 and 3 (the band derives from the same sums).
    let xml = format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="{NS}">
  <dimension ref="A1:F10"/>
  <sheetViews><sheetView workbookViewId="0"><pane xSplit="3" ySplit="2" topLeftCell="D3" state="frozen"/></sheetView></sheetViews>
  <cols><col min="2" max="2" width="12" hidden="1" customWidth="1"/></cols>
  <sheetData>
    <row r="1" hidden="1"><c r="A1" t="n"><v>1</v></c></row>
    <row r="10"><c r="F10" t="n"><v>2</v></c></row>
  </sheetData>
</worksheet>"#
    );
    let r = SmlRenderer::from_xlsx(xlsx_with(&xml, &sst(&[]))).expect("from_xlsx");
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let g = &gs.geom;
    let bands = r.pane_bands(0, &fc).expect("bands");
    // band_w = left of col 4 over the SAME rails — col 2 contributes zero.
    assert_eq!(bands.band_w, g.col_left(4));
    let visible_two_cols = (g.col_left(2) - g.col_left(1)) + (g.col_left(4) - g.col_left(3));
    assert!((bands.band_w - visible_two_cols).abs() < 1e-9, "band = the two VISIBLE widths");
    // band_h = top of row 3; hidden row 1 contributes zero → one visible row.
    assert_eq!(bands.band_h, g.row_top(3));
    assert_eq!(bands.band_h, g.default_row_px, "row 1 hidden → band is ONE row tall");
}

// ─── 4b) selection view mapping: a range over hidden members paints the
//        visible geometry (cell_sel_paint reads the same sums) ────────────────

#[test]
fn selection_over_hidden_members_paints_the_visible_geometry() {
    let fc = calibri_font_cache();
    let r = hidden_renderer();
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let g = &gs.geom;
    let sel = crate::sml::select::CellSelection {
        sheet: 0,
        range: CellRange::new(1, 2, 6, 4), // B1:D6 — spans hidden col C + rows 4-5
        anchor_row: 1,
        anchor_col: 2,
    };
    let p = crate::sml::select::cell_sel_paint(g, &r.indexes[0].merges, &sel);
    // Width B..D = w(B) + w(D) only (C is zero); height rows 1..6 = 4 visible rows.
    let visible_w = (g.col_left(3) - g.col_left(2)) + (g.col_left(5) - g.col_left(4));
    assert!((p.x1 - p.x0 - visible_w).abs() < 1e-9, "selection width excludes hidden C");
    assert_eq!(p.y1 - p.y0, 4.0 * g.default_row_px, "selection height excludes hidden 4-5");
}

// ─── 5) merges spanning hidden members render at the visible span ───────────

#[test]
fn merge_spanning_a_hidden_col_renders_the_visible_span() {
    let fc = calibri_font_cache();
    let xml = format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="{NS}">
  <dimension ref="A1:D2"/>
  <cols><col min="3" max="3" width="10" hidden="1" customWidth="1"/></cols>
  <sheetData>
    <row r="1"><c r="B1" t="s"><v>0</v></c></row>
    <row r="2"><c r="B2" t="n"><v>7</v></c></row>
  </sheetData>
  <mergeCells count="1"><mergeCell ref="B1:D1"/></mergeCells>
</worksheet>"#
    );
    let r = SmlRenderer::from_xlsx(xlsx_with(&xml, &sst(&["Title"]))).expect("from_xlsx");
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let g = &gs.geom;
    let page = r.window_page(0, &fc, g.min_row, g.max_row, 0.0).expect("page");
    let anchor = page.cells.iter().find(|c| c.ref_a1 == "B1").expect("anchor");
    assert!(anchor.is_merged_anchor);
    // B1:D1 with C hidden → the box spans exactly w(B) + w(D).
    assert_eq!(anchor.w, g.col_left(5) - g.col_left(2));
    let visible_span = (g.col_left(3) - g.col_left(2)) + (g.col_left(5) - g.col_left(4));
    assert!((anchor.w - visible_span).abs() < 1e-9, "merge width = the visible members only");
}

// ─── 6) no degenerate boxes: hidden cells emit nothing ───────────────────────

#[test]
fn hidden_cells_emit_no_zero_size_boxes_on_grid_or_print_pages() {
    let fc = calibri_font_cache();
    let r = hidden_renderer();
    let gs = r.ensure_geom(0, &fc).expect("geom");
    // Grid window page (the continuous view's geometry).
    let page = r.window_page(0, &fc, gs.geom.min_row, gs.geom.max_row, 0.0).expect("page");
    assert!(
        page.cells.iter().all(|c| c.w > 0.0 && c.h > 0.0),
        "no degenerate cell boxes on the window page"
    );
    for hidden_ref in ["C1", "C2", "C3", "A4", "A5"] {
        assert!(
            !page.cells.iter().any(|c| c.ref_a1 == hidden_ref),
            "hidden cell {hidden_ref} must not be emitted"
        );
    }
    // Print pages via the eager model path — same rule.
    let m = r.materialize_model(0, &fc).expect("model");
    for p in grid::paginate(&m, grid::PageBox::default()) {
        assert!(p.cells.iter().all(|c| c.w > 0.0 && c.h > 0.0), "print page degenerate box");
        assert!(p.merges.iter().all(|m| m.w > 0.0 && m.h > 0.0), "print merge degenerate box");
    }
}

#[test]
fn an_entirely_hidden_range_degrades_to_an_empty_grid() {
    let fc = calibri_font_cache();
    // EVERY row and EVERY column of the used range hidden — no panic, an
    // empty (zero-extent) grid, no cells emitted.
    let xml = format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="{NS}">
  <dimension ref="A1:B2"/>
  <cols><col min="1" max="2" width="9" hidden="1" customWidth="1"/></cols>
  <sheetData>
    <row r="1" hidden="1"><c r="A1" t="n"><v>1</v></c><c r="B1" t="n"><v>2</v></c></row>
    <row r="2" hidden="1"><c r="A2" t="n"><v>3</v></c></row>
  </sheetData>
</worksheet>"#
    );
    let r = SmlRenderer::from_xlsx(xlsx_with(&xml, &sst(&[]))).expect("from_xlsx");
    let gs = r.ensure_geom(0, &fc).expect("geom");
    assert_eq!(gs.geom.content_w, 0.0);
    assert_eq!(gs.geom.content_h, 0.0);
    let page = r.window_page(0, &fc, gs.geom.min_row, gs.geom.max_row, 0.0).expect("page");
    assert!(page.cells.is_empty(), "all cells hidden → nothing emitted");
    // The grid extent stays sane (gutters only) and the paint does not panic.
    let (w, h) = r.grid_extent(0, &fc).expect("extent");
    assert!(w > 0.0 && h > 0.0, "gutter-only extent");
}

// ─── 7) search skips hidden (Excel Find default) — engine-side filter ────────

#[test]
fn search_skips_hidden_rows_and_cols() {
    let fc = calibri_font_cache();
    let r = hidden_renderer();
    let opts = SearchOpts::default();
    // "ghost" lives ONLY in hidden rows 4+5 → zero matches.
    assert!(r.search_sheet(0, "ghost", opts, &fc).is_empty(), "hidden-row text not found");
    // "cee" lives ONLY in hidden col C (C1, C3) → zero matches.
    assert!(r.search_sheet(0, "cee", opts, &fc).is_empty(), "hidden-col text not found");
    // "33" is C2 (hidden col) → zero; "44" is D2 (visible) → one.
    assert!(r.search_sheet(0, "33", opts, &fc).is_empty());
    assert_eq!(r.search_sheet(0, "44", opts, &fc).len(), 1);
    // Visible matches still surface — the count reflects VISIBLE cells only.
    let vis: Vec<String> =
        r.search_sheet(0, "vis", opts, &fc).into_iter().map(|h| h.ref_a1).collect();
    assert_eq!(vis, vec!["A1".to_string(), "A8".to_string()]);
}

// ─── 8) copy: the index-range walk INCLUDES hidden values (as Excel does) ───

#[test]
fn copy_across_hidden_members_includes_their_values() {
    let fc = calibri_font_cache();
    let r = hidden_renderer();
    // B1:D2 with col C hidden — the hidden C values PASTE (index-range walk,
    // Excel default; NO copy-path change).
    let frag = r.extract_cell_range(0, CellRange::new(1, 2, 2, 4), &fc).expect("fragment");
    let grid_out: Vec<Vec<&str>> =
        frag.rows.iter().map(|row| row.iter().map(|c| c.display.as_str()).collect()).collect();
    assert_eq!(
        grid_out,
        vec![vec!["11", "cee", "12"], vec!["22", "33", "44"]],
        "hidden column C's values are included"
    );
    // A3:A6 spans hidden rows 4+5 — their values are included too.
    let frag = r.extract_cell_range(0, CellRange::new(3, 1, 6, 1), &fc).expect("fragment");
    let col_out: Vec<&str> = frag.rows.iter().map(|row| row[0].display.as_str()).collect();
    assert_eq!(col_out, vec!["", "ghost", "ghost", ""], "hidden rows' values are included");
}

// ─── 9) header cue: letters/numbers skip + the heavier boundary marker ───────

/// Record one production-wired grid viewport paint of sheet 0.
fn grid_paint_ops(r: &SmlRenderer, fc: &FontCache) -> Vec<DrawCmd> {
    let gs = r.ensure_geom(0, fc).expect("geom");
    let geom = &gs.geom;
    let gut_w = grid::row_gutter_width_for_max_row(geom.max_row);
    let gut_h = grid::COL_GUTTER_H;
    let (r0, r1, win_x1) = crate::sml::grid_window(geom, 0.0, 0.0, 640.0, 480.0);
    let page = r.window_page(0, fc, r0, r1, win_x1).expect("window page");
    let rec = RecordingPainter::new();
    render::paint_grid_viewport(
        &rec,
        &page,
        (gut_w, gut_h),
        0.0,
        0.0,
        640.0,
        480.0,
        r.normal_font(),
        fc,
        &HashMap::new(),
        &HashMap::new(),
        None,
        None,
        None,
        None,
    )
    .expect("paint");
    rec.take()
}

#[test]
fn gutters_skip_hidden_labels_and_paint_the_boundary_marker() {
    let fc = calibri_font_cache();
    let r = hidden_renderer();
    let cmds = grid_paint_ops(&r, &fc);
    let labels: Vec<&str> = cmds
        .iter()
        .filter_map(|c| match c {
            DrawCmd::FillText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    // Column letters skip C and E (hidden) but keep A/B/D/F.
    for l in ["A", "B", "D", "F"] {
        assert!(labels.contains(&l), "visible column letter {l} painted");
    }
    for l in ["C", "E"] {
        assert!(!labels.contains(&l), "hidden column letter {l} must skip");
    }
    // Row numbers skip 4 and 5 but keep 3 and 6.
    for n in ["3", "6"] {
        assert!(labels.contains(&n), "visible row number {n} painted");
    }
    for n in ["4", "5"] {
        assert!(!labels.contains(&n), "hidden row number {n} must skip");
    }
    // Hidden cell text never paints.
    for t in ["ghost", "cee"] {
        assert!(!labels.contains(&t), "hidden cell text {t} must not paint");
    }
    // The heavier hidden-range marker strokes are present: rows 4+5 share ONE
    // deduped mark, cols C and E get one each → 3 marker strokes.
    let marker_strokes = cmds
        .iter()
        .filter(|c| matches!(c, DrawCmd::SetStrokeStyle(s) if s == "#9b9b9b"))
        .count();
    assert_eq!(marker_strokes, 3, "one col mark ×2 + one deduped row mark");
}

#[test]
fn hidden_less_grid_paint_has_no_marker_and_all_labels() {
    let fc = calibri_font_cache();
    // The same fixture WITHOUT any hidden attributes.
    let xml = format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="{NS}">
  <dimension ref="A1:F8"/>
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c></row>
    <row r="8"><c r="A8" t="s"><v>0</v></c></row>
  </sheetData>
</worksheet>"#
    );
    let r = SmlRenderer::from_xlsx(xlsx_with(&xml, &sst(&["vis"]))).expect("from_xlsx");
    let cmds = grid_paint_ops(&r, &fc);
    assert!(
        !cmds.iter().any(|c| matches!(c, DrawCmd::SetStrokeStyle(s) if s == "#9b9b9b")),
        "no hidden range → no marker"
    );
    let labels: Vec<&str> = cmds
        .iter()
        .filter_map(|c| match c {
            DrawCmd::FillText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    for l in ["C", "E", "4", "5"] {
        assert!(labels.contains(&l), "hidden-less sheet keeps label {l}");
    }
}

#[test]
fn print_page_paint_skips_labels_and_marks_hidden_ranges_too() {
    let fc = calibri_font_cache();
    let r = hidden_renderer();
    let m = r.materialize_model(0, &fc).expect("model");
    let pages = grid::paginate(&m, grid::PageBox::default());
    let rec = RecordingPainter::new();
    render::paint_sheet_page(
        &rec,
        &pages[0],
        24.0,
        24.0,
        816.0,
        1056.0,
        r.normal_font(),
        &fc,
        &HashMap::new(),
        &HashMap::new(),
        None,
    )
    .expect("paint");
    let cmds = rec.take();
    let labels: Vec<&str> = cmds
        .iter()
        .filter_map(|c| match c {
            DrawCmd::FillText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    for l in ["C", "E", "4", "5"] {
        assert!(!labels.contains(&l), "print gutter must skip hidden label {l}");
    }
    assert!(
        cmds.iter().any(|c| matches!(c, DrawCmd::SetStrokeStyle(s) if s == "#9b9b9b")),
        "print gutter marks the hidden ranges"
    );
}
