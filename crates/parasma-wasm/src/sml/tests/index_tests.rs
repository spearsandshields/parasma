//! Grid v2 — `SheetIndex` equivalence tests: the lazy index must agree with
//! the eager, materialized path it replaces.
//!
//! These tests prove, on every
//! existing SML fixture class, that it reproduces the materialized v1 truth:
//!
//!  - **husk metadata == resolved metadata**: col runs, sheetFormatPr
//!    defaults, gridlines flag, merges — byte-for-byte what `SheetModel`
//!    reads via the eager path.
//!  - **row entries == fork rows**: count, explicit/implied `@r` numbering
//!    (§18.3.1.73), `@ht` exceptions, per-row populated-column bounds; every
//!    byte offset resolves to a real `<row` start.
//!  - **closed-form extent == `grid_extent` EXACTLY** (the v1/v2 equivalence
//!    pre-check), including the picture extension and the empty-sheet
//!    synthesis. FP caveat (see sml/index.rs header): default-height runs are
//!    multiplied, not accumulated, so fixtures whose row heights are not
//!    binary-exact (`ht="22"` → 88/3px) match to ~1ulp instead of bit-exact —
//!    those assert at 1e-6px (a million times below the geometry-hash
//!    quantum); every binary-exact fixture asserts `==`.
//!  - **y↔row / x↔col closed-form == brute-force rails** of the continuous
//!    page, probed at every rail and band midpoint.
//!
//! The 1M-row pre-scan wall-clock bound runs `--ignored` (release-profiled,
//! like the perf harness in `perf.rs`):
//!   cargo test -p parasma-wasm --features sml --release -- --ignored perf_index

use std::time::Instant;

use ooxml_sml::ext::CellExt;

use crate::sml::index::SheetIndex;
use crate::sml::model::{self, SheetModel};
use crate::sml::SmlRenderer;

use super::perf::{stamped_xlsx, STAMPED_1M_ROWS, STAMPED_CI_ROWS};
use super::{
    build_xlsx_two_sheets, calibri_font_cache, drawing_tests, merge_tests, sst, xlsx_with,
    ORACLE_SHEET_XML, SPARSE_HEIGHTS_SHEET_XML,
};

/// Drive a whole workbook through BOTH paths and assert the index reproduces
/// the materialized truth on every sheet. `exact` → bit-exact px equality
/// (binary-exact row heights); otherwise 1e-6px (the documented ~1ulp FP
/// difference of multiplied default runs). Returns the number of sheets
/// checked (sanity: a helper that silently checks nothing proves nothing).
fn assert_index_equivalent(bytes: Vec<u8>, exact: bool) -> usize {
    let fc = calibri_font_cache();
    let assert_px = |a: f64, b: f64, what: &str| {
        if exact {
            assert!(a == b, "{what}: index {a} != materialized {b} (bit-exact expected)");
        } else {
            assert!((a - b).abs() < 1e-6, "{what}: index {a} vs materialized {b}");
        }
    };

    let r = SmlRenderer::from_xlsx(bytes.clone()).expect("from_xlsx");
    // The eager v1 truth, built STANDALONE (the renderer no longer holds
    // materialized sheets): `<col>`-tolerant shim load + from_resolved
    // + continuous_page, exactly the eager path.
    let mut wb = ooxml_sml::workbook::Workbook::from_reader(std::io::Cursor::new(bytes)).expect("tolerant open");
    let names: Vec<String> = wb.sheet_names().iter().map(|s| s.to_string()).collect();
    let (eager_sheets, _warnings) = crate::sml::shim::load_sheets_tolerant(&mut wb);
    let mdw = model::measure_mdw(r.normal_font(), &fc);

    let mut checked = 0usize;
    for (wi, name) in names.iter().enumerate() {
        // Match by NAME (a dropped sheet shifts indices off workbook order).
        let Some(ls) = eager_sheets.iter().find(|ls| ls.sheet.name() == name.as_str()) else {
            continue;
        };
        let xml = wb.sheet_xml(wi).expect("sheet_xml");
        let idx = SheetIndex::build(name.clone(), xml).expect("index build");

        // ── husk metadata == eager model metadata ────────────────────────────
        let mut model = SheetModel::from_resolved(
            &ls.sheet,
            r.normal_font(),
            &fc,
            ls.col_widths_degraded,
            &r.stylesheet,
            r.color_ctx(),
        );
        assert_eq!(idx.name, model.name, "sheet name");
        assert_eq!(idx.col_runs, model.col_runs, "{name}: husk col runs");
        assert_eq!(
            idx.default_row_height_pt, model.default_row_height_pt,
            "{name}: husk defaultRowHeight"
        );
        assert_eq!(
            idx.default_col_width_chars, model.default_col_width_chars,
            "{name}: husk defaultColWidth"
        );
        assert_eq!(idx.show_grid_lines, model.show_grid_lines, "{name}: husk gridlines");
        assert_eq!(idx.merges, model.merges, "{name}: husk merges");
        assert_eq!(idx.col_widths_degraded, ls.col_widths_degraded, "{name}: degraded flag");
        // The v1-mirror used range IS what paginate consumes as dimensions.
        assert_eq!(idx.used_range_v1, model.dimensions, "{name}: used range (v1 mirror)");

        // ── row entries == fork rows (offsets, numbering, ht, col bounds) ────
        let fork_rows: Vec<_> = ls.sheet.rows().collect();
        assert_eq!(idx.rows.len(), fork_rows.len(), "{name}: present-row count");
        let mut next_row = 1u32;
        for (e, row) in idx.rows.iter().zip(&fork_rows) {
            let expect = row.reference.unwrap_or(next_row);
            next_row = expect + 1;
            assert_eq!(e.row, expect, "{name}: row number (explicit or implied)");
            assert_eq!(e.explicit_r, row.reference.is_some(), "{name}: r{expect} explicit flag");
            assert_eq!(e.ht_pt, row.height, "{name}: r{expect} @ht");
            let slice = &idx.xml[e.byte_off as usize..e.byte_end as usize];
            assert!(slice.starts_with(b"<row"), "{name}: r{expect} offset resolves to <row");
            assert!(slice.ends_with(b">"), "{name}: r{expect} slice is a complete element");
            let (mut mn, mut mx) = (u32::MAX, 0u32);
            for c in &row.cells {
                if let Some(col) = c.column_number() {
                    mn = mn.min(col);
                    mx = mx.max(col);
                }
            }
            assert_eq!(e.max_col, mx, "{name}: r{expect} max populated col");
            assert_eq!(e.min_col, if mn == u32::MAX { 0 } else { mn }, "{name}: r{expect} min col");
        }

        // ── closed-form geometry == materialized continuous page ────────────
        let drawings = r.drawings.get(name).cloned().unwrap_or_default();
        let geom = idx.geom(mdw, &drawings);
        model.drawings = drawings;
        let p = crate::sml::grid::continuous_page(&model).expect("continuous page");
        let (gw, gh) = crate::sml::grid::gutter_dims(&p);
        let (ew, eh) = (p.content_w + gw, p.content_h + gh);
        let (iw, ih) = geom.extent_px();
        assert_px(iw, ew, &format!("{name}: extent width"));
        assert_px(ih, eh, &format!("{name}: extent height"));
        // The v2 renderer extent (geom_v2: v1 ∪ stamp ∪ content) must agree on
        // these truthful fixtures (stamp == content by construction).
        if let Some(ri) = r.indexes.iter().position(|ix| ix.name == *name) {
            let (vw, vh) = r.grid_extent(ri, &fc).expect("grid_extent");
            assert_px(vw, ew, &format!("{name}: v2 extent width"));
            assert_px(vh, eh, &format!("{name}: v2 extent height"));
        }

        {
            let p = &p;
            assert_px(geom.content_w, p.content_w, &format!("{name}: content_w"));
            assert_px(geom.content_h, p.content_h, &format!("{name}: content_h"));
            assert_eq!(p.first_col, geom.min_col, "{name}: first col");
            // Row rails: one band per row of the (possibly picture-extended)
            // range; closed-form row_top must hit every materialized rail.
            assert_eq!(
                p.row_rails.len() as u32,
                geom.max_row - geom.min_row + 2,
                "{name}: row rail count"
            );
            for (k, rail) in p.row_rails.iter().enumerate() {
                let row = geom.min_row + k as u32;
                assert_px(geom.row_top(row), *rail, &format!("{name}: row_top({row})"));
            }
            // Column rails (always integer px — §18.3.1.13 truncation).
            assert_eq!(
                p.col_rails.len() as u32,
                geom.max_col - geom.min_col + 2,
                "{name}: col rail count"
            );
            for (k, rail) in p.col_rails.iter().enumerate() {
                let col = geom.min_col + k as u32;
                assert!(geom.col_left(col) == *rail, "{name}: col_left({col})");
            }
            // y→row: every band, probed at the midpoint (and the exact top
            // rail when the fixture is binary-exact — at ~1ulp drift an exact
            // rail probe would be ambiguous by construction).
            for k in 0..p.row_rails.len() - 1 {
                let row = p.row_numbers[k];
                let (top, bot) = (p.row_rails[k], p.row_rails[k + 1]);
                if bot - top > 0.01 {
                    assert_eq!(geom.row_at_y((top + bot) / 2.0), row, "{name}: row_at_y mid r{row}");
                }
                if exact {
                    assert_eq!(geom.row_at_y(top), row, "{name}: row_at_y top r{row}");
                }
            }
            // x→col: every band, top rail + midpoint (rails exact).
            for k in 0..p.col_rails.len() - 1 {
                let col = geom.min_col + k as u32;
                let (l, rr) = (p.col_rails[k], p.col_rails[k + 1]);
                if rr - l > 0.01 {
                    assert_eq!(geom.col_at_x((l + rr) / 2.0), col, "{name}: col_at_x mid c{col}");
                    assert_eq!(geom.col_at_x(l), col, "{name}: col_at_x left c{col}");
                }
            }
        }
        checked += 1;
    }
    assert!(checked > 0, "helper checked no sheets");
    checked
}

// ─── the equivalence pre-check over every existing fixture class ─────────────

#[test]
fn index_matches_renderer_on_oracle_fixture() {
    assert_index_equivalent(xlsx_with(ORACLE_SHEET_XML, &sst(&["Header", "Footer"])), true);
}

#[test]
fn index_matches_renderer_on_sparse_heights_fixture() {
    // Sparse rows, EMPTY ht-only exception row, r-less implied row (§18.3.1.73),
    // explicit col runs — the grid-edge inventory fixture.
    assert_index_equivalent(xlsx_with(SPARSE_HEIGHTS_SHEET_XML, &sst(&["Header"])), true);
}

#[test]
fn index_matches_renderer_on_merged_fixture() {
    // ht="22" → 88/3 px is NOT binary-exact: this fixture exercises the
    // documented ~1ulp multiplied-run vs accumulated-run FP difference, so it
    // asserts at 1e-6px (see the sml/index.rs header note).
    assert_index_equivalent(
        xlsx_with(merge_tests::MERGED_SHEET_XML, &sst(merge_tests::MERGED_STRINGS)),
        false,
    );
}

#[test]
fn index_matches_renderer_on_wide_24_col_fixture() {
    // 24 default-width columns (the band-tiling fixture shape): the
    // continuous page never tiles, but the col rails + x→col mapping span all
    // 24 columns.
    let mut body = String::new();
    for r in 1..=3u32 {
        body.push_str(&format!("<row r=\"{r}\">"));
        for c in 1..=24u32 {
            body.push_str(&format!(
                "<c r=\"{}{r}\" t=\"n\"><v>{c}</v></c>",
                crate::sml::grid::col_letters(c)
            ));
        }
        body.push_str("</row>");
    }
    let sheet = format!(
        "<?xml version=\"1.0\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"A1:X3\"/><sheetFormatPr defaultRowHeight=\"15\" baseColWidth=\"8\"/><sheetData>{body}</sheetData></worksheet>"
    );
    assert_index_equivalent(xlsx_with(&sheet, &sst(&[])), true);
}

#[test]
fn index_matches_renderer_on_two_sheet_workbook_with_empty_sheet() {
    // Sheet "One" has data; sheet "Two" is EMPTY (self-closing sheetData) —
    // the empty-sheet synthesis: the index's geom must reproduce the 25×10 default grid.
    let sheet1 = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B2"/>
  <sheetFormatPr defaultRowHeight="15"/>
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="n"><v>7</v></c></row>
    <row r="2" ht="30" customHeight="1"><c r="B2" t="n"><v>8</v></c></row>
  </sheetData>
</worksheet>"#;
    let sheet2 = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetFormatPr defaultRowHeight="15"/>
  <sheetData/>
</worksheet>"#;
    let n = assert_index_equivalent(build_xlsx_two_sheets(sheet1, sheet2, &sst(&["x"])), true);
    assert_eq!(n, 2, "both sheets (incl. the empty one) checked");
}

#[test]
fn index_matches_renderer_on_drawing_fixture() {
    // The twoCellAnchor picture: the closed-form extent must include the
    // same used-range extension paginate applies under a floating drawing.
    assert_index_equivalent(drawing_tests::xlsx_with_drawing(drawing_tests::DRAWING_TWO_CELL), true);
}

#[test]
fn index_matches_renderer_on_stamped_ci_fixture() {
    // The generated stamped fixture (`perf.rs`): ~2048 physically-present
    // rows, r-less run, ht exceptions (incl. an EMPTY ht-only row), merges
    // incl. the chunk-boundary one, SENTINEL at J2048.
    let n = assert_index_equivalent(stamped_xlsx(STAMPED_CI_ROWS), true);
    assert_eq!(n, 1);
}

// ─── stamped-fixture inventory: the index records the pre-scan facts ─────────

#[test]
fn stamped_ci_index_records_rless_heights_merges_and_distrust_fields() {
    let mut wb = ooxml_sml::workbook::Workbook::from_reader(std::io::Cursor::new(
        stamped_xlsx(STAMPED_CI_ROWS),
    ))
    .expect("open");
    let xml = wb.sheet_xml(0).expect("sheet_xml");
    let idx = SheetIndex::build("Stamped".into(), xml).expect("build");
    let rows = STAMPED_CI_ROWS;
    let half = rows / 2;

    // Every stamped row is physically present (the .NET-exporter class).
    assert_eq!(idx.rows.len(), rows as usize);
    // The r-less run 1000..=1004 implied from explicit row 999 (§18.3.1.73).
    for r in 1000..=1004u32 {
        let e = &idx.rows[(r - 1) as usize];
        assert_eq!(e.row, r, "implied row number");
        assert!(!e.explicit_r, "r-less row {r}");
        assert_eq!(e.min_col, 1, "r-less row {r} carries its A-col cell");
    }
    // Height exceptions: row 10 (30pt), half (45pt), rows-6 (EMPTY ht-only 60pt).
    assert_eq!(idx.rows[9].ht_pt, Some(30.0));
    assert_eq!(idx.rows[(half - 1) as usize].ht_pt, Some(45.0));
    let empty_ht = &idx.rows[(rows - 7) as usize];
    assert_eq!(empty_ht.ht_pt, Some(60.0), "EMPTY custom-height row indexed");
    assert_eq!(empty_ht.max_col, 0, "…with no cells");
    // The chunk-boundary merge A255:A258 arrives via the husk.
    assert!(
        idx.merges.iter().any(|m| m.min_row == 255 && m.max_row == 258 && m.min_col == 1),
        "A255:A258 merge from the husk parse"
    );
    // Dimension-distrust bookkeeping: stamp and scanned truth recorded SEPARATELY
    // (here they agree — the fixture's stamp is truthful).
    assert_eq!(idx.stamped_dimension, Some((1, 1, rows, 10)));
    assert_eq!(idx.content_bounds, Some((1, 1, rows, 10)));
}

#[test]
fn understating_stamp_keeps_scanned_truth_distinct() {
    // Dimension-distrust raw material: the stamp claims A1:B2 but content
    // reaches E10. The index RECORDS both sides (used_range_v1 stays the fork
    // mirror; the effective extent is `effective_range`, tested in window_tests).
    let xml = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B2"/>
  <sheetFormatPr defaultRowHeight="15"/>
  <sheetData>
    <row r="1"><c r="A1" t="n"><v>1</v></c></row>
    <row r="10"><c r="E10" t="n"><v>5</v></c></row>
  </sheetData>
</worksheet>"#;
    let idx = SheetIndex::build("S".into(), xml.as_bytes().to_vec()).expect("build");
    assert_eq!(idx.stamped_dimension, Some((1, 1, 2, 2)), "the file's (lying) claim");
    assert_eq!(idx.content_bounds, Some((1, 1, 10, 5)), "the scanned truth");
    assert_eq!(idx.used_range_v1, Some((1, 1, 10, 5)), "fork-mirror range (rows-derived)");
}

// ─── closed-form extent at full row-space scale (no walk, no eager path) ─────

#[test]
fn sparse_stamp_indexes_present_rows_only_with_closed_form_extent() {
    // A 1M-row-space sheet with THREE present rows: the index stays sparse
    // (3 entries, not a million) and extent / row↔y resolve closed-form.
    let total = 1_048_576u32;
    let xml = format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:J{total}"/>
  <sheetFormatPr defaultRowHeight="15"/>
  <sheetData>
    <row r="1"><c r="A1" t="n"><v>1</v></c></row>
    <row r="500000" ht="45" customHeight="1"/>
    <row r="{total}"><c r="J{total}" t="n"><v>9</v></c></row>
  </sheetData>
</worksheet>"#
    );
    let idx = SheetIndex::build("S".into(), xml.into_bytes()).expect("build");
    assert_eq!(idx.rows.len(), 3, "sparse: present rows only");
    assert_eq!(idx.used_range_v1, Some((1, 1, total, 10)));

    let geom = idx.geom(9.0, &[]);
    // Closed-form height: (total − 1 exception) × 20px + 60px.
    let expected_h = (total as f64 - 1.0) * 20.0 + 60.0;
    assert_eq!(geom.content_h, expected_h);
    assert_eq!(geom.extent_px().1, expected_h + 13.0, "+ the column-letter gutter");
    // row↔y closed-form around the deep exception and the extremes.
    let top_exc = geom.row_top(500_000);
    assert_eq!(top_exc, 499_999.0 * 20.0);
    assert_eq!(geom.row_at_y(top_exc + 59.0), 500_000, "inside the 60px exception band");
    assert_eq!(geom.row_at_y(top_exc + 60.0), 500_001, "first default row after it");
    assert_eq!(geom.row_at_y(0.0), 1);
    assert_eq!(geom.row_at_y(expected_h - 1.0), total, "bottom row");
    assert_eq!(geom.row_at_y(expected_h + 999.0), total, "clamped past the extent");
}

// ─── 1M-row pre-scan: wall-clock bound + full-scale equivalence (--ignored) ──

#[test]
#[ignore = "1M-row index pre-scan — run explicitly with --release"]
fn perf_index_stamped_1m() {
    let fc = calibri_font_cache();
    let bytes = stamped_xlsx(STAMPED_1M_ROWS);

    let mut wb = ooxml_sml::workbook::Workbook::from_reader(std::io::Cursor::new(bytes.clone()))
        .expect("open");
    let t = Instant::now();
    let xml = wb.sheet_xml(0).expect("sheet_xml");
    let inflate_ms = t.elapsed().as_secs_f64() * 1e3;
    let xml_len = xml.len();

    let t = Instant::now();
    let idx = SheetIndex::build("Stamped".into(), xml).expect("build");
    let build_ms = t.elapsed().as_secs_f64() * 1e3;

    assert_eq!(idx.rows.len(), STAMPED_1M_ROWS as usize, "all physically-present rows indexed");

    // Full-scale equivalence: closed-form extent == the eager path's, exactly
    // (all stamped heights are binary-exact px).
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
    let t = Instant::now();
    let geom = idx.geom(model::measure_mdw(r.normal_font(), &fc), &[]);
    let geom_ms = t.elapsed().as_secs_f64() * 1e3;
    let eager = r.grid_extent(0, &fc).expect("grid_extent");
    assert_eq!(geom.extent_px(), eager, "closed-form extent == eager extent at 1M rows");

    eprintln!(
        "[perf] sheet_xml(inflate): {inflate_ms:.1} ms ({xml_len} bytes) | index build: {build_ms:.1} ms | geom: {geom_ms:.1} ms | extent {:.0}×{:.0}px",
        geom.extent_px().0,
        geom.extent_px().1
    );
    // Wall-clock bound (release): the pre-scan is the ONLY O(bytes) step
    // v2 keeps at load — it must stay well inside a <2s interactive
    // budget. 1.5s on ~17MB of worksheet XML is a generous ceiling (measured
    // ~2 orders below; the bound guards regression class, not the mean).
    assert!(build_ms < 1500.0, "1M pre-scan too slow: {build_ms:.1} ms");
}
