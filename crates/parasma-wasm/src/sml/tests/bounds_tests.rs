//! Document-supplied SpreadsheetML index/range values.
//!
//! ECMA-376 places NO magnitude bound on any of these values (see the
//! `MAX_ADDRESSABLE_*` doc comments in `sml::model` for the citations), so every
//! test here pins a REJECT/DEGRADE outcome, never a rewritten coordinate:
//! an unaddressable cell/merge/dimension/row is dropped, and every addressable
//! one — up to and including the `XFD1048576` corner — renders exactly as it
//! did before.

use std::time::Instant;

use crate::sml::grid;
use crate::sml::index::SheetIndex;
use crate::sml::model::{parse_a1, MAX_ADDRESSABLE_COL, MAX_ADDRESSABLE_ROW};
use crate::sml::window;
use crate::sml::SmlRenderer;

use super::{calibri_font_cache, sst, xlsx_with};

const NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

fn index_of(sheet_xml: &str) -> SheetIndex {
    SheetIndex::build("Sheet1".into(), sheet_xml.as_bytes().to_vec()).expect("index build")
}

// ─── (1) the RENDERED-cell column entry point ────────────────────────────────

/// `build_rows` (the path every PAINTED cell takes) sourced its column from the
/// fork's unbounded `Cell::column_number`, which accumulates `col * 26 + d` with
/// no check: an 8-letter run panics in debug and WRAPS in release, landing the
/// cell in an arbitrary in-grid column. Bounding the index pre-scan's `parse_a1`
/// alone does not reach it — this test drives the real materializer.
#[test]
fn unaddressable_cell_columns_are_dropped_not_wrapped() {
    // `AATYHWUU` overflows u32 outright; `ZZZZZZ` (321 272 406) does not, but is
    // still 19 600x wider than the addressable grid — one f64 rail per column is
    // ~2.5 GB. Both must simply not become cells.
    let sheet = format!(
        r#"<worksheet xmlns="{NS}"><dimension ref="A1:E1"/><sheetData><row r="1"><c r="A1" t="n"><v>1</v></c><c r="AATYHWUU1" t="n"><v>999</v></c><c r="ZZZZZZ1" t="n"><v>998</v></c></row></sheetData></worksheet>"#
    );
    let bytes = xlsx_with(&sheet, &sst(&[]));
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
    let idx = &r.indexes[0];

    // The pre-scan's own column bounds stay addressable...
    assert_eq!((idx.rows[0].min_col, idx.rows[0].max_col), (1, 1));
    // ...and so does every range derived from them.
    assert!(idx.effective_range().3 <= MAX_ADDRESSABLE_COL, "effective_range col off grid");

    // ...and the MATERIALIZED rows carry only the addressable cell (this is the
    // assertion `parse_a1`-only bounding does not satisfy).
    let rows = window::parse_rows_window(idx, 1, 1, &[], &r.stylesheet, r.color_ctx());
    let cols: Vec<u32> = rows.iter().flat_map(|rd| rd.cells.iter().map(|c| c.col)).collect();
    assert_eq!(cols, vec![1], "only the addressable cell survives; none is relocated");
}

// ─── (2) merge coverage is CLIPPED on both axes ──────────────────────────────

/// An Excel select-all merge (`A1:XFD1048576`) is a LEGITIMATE document, so the
/// fix for it cannot be to drop it — it has to cost the visible window, not the
/// merge. The row axis was already clipped; the column axis was not, so one
/// window build inserted `window_rows x 16 384` coverage entries per repaint
/// (and `fill_plan_page` the same per page).
///
/// Cost is the only observable here (nothing is dropped, so the output is
/// byte-identical either way) — hence a wall-clock bound, in the style of
/// `sml::tests::perf`. The unclipped build is ~16.4M `HashSet` inserts; the
/// clipped one is ~1000 x the sheet's 3 columns.
#[test]
fn a_select_all_merge_costs_the_window_not_the_merge() {
    let sheet = format!(
        r#"<worksheet xmlns="{NS}"><dimension ref="A1:C3"/><sheetData><row r="1"><c r="A1" t="n"><v>1</v></c><c r="B1" t="n"><v>2</v></c><c r="C1" t="n"><v>3</v></c></row><row r="2"><c r="A2" t="n"><v>4</v></c></row><row r="3"><c r="A3" t="n"><v>5</v></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:XFD1048576"/></mergeCells></worksheet>"#
    );
    let bytes = xlsx_with(&sheet, &sst(&[]));
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
    let fc = calibri_font_cache();

    // The merge itself is kept intact — it is a valid region.
    let m = &r.indexes[0].merges[0];
    assert_eq!(
        (m.min_row, m.min_col, m.max_row, m.max_col),
        (1, 1, MAX_ADDRESSABLE_ROW, MAX_ADDRESSABLE_COL),
        "the select-all merge is NOT trimmed"
    );

    let t = Instant::now();
    let p = r.window_page(0, &fc, 1, 1000, 0.0).expect("window page");
    let ms = t.elapsed().as_secs_f64() * 1e3;
    assert_eq!(p.row_numbers.len(), 1000);
    assert!(ms < 500.0, "windowed build walks the merge, not the window: {ms:.1}ms");
}

// ─── (3) out-of-grid `<row @r>`: DROPPED, never snapped ──────────────────────

/// `<row @r>` is a bare integer that never passes through an A1 parse. An
/// unaddressable row must fall out of EVERY layout range — consistently, in one
/// direction. Snapping it to the last grid row instead would INFLATE the print
/// range to row 1 048 576 and invent a million blank rows of pagination.
#[test]
fn unaddressable_rows_drop_out_of_every_range() {
    let sheet = format!(
        r#"<worksheet xmlns="{NS}"><sheetData><row r="1"><c r="A1" t="n"><v>1</v></c></row><row r="3"><c r="B3" t="n"><v>2</v></c></row><row r="4000000000" ht="20"><c r="B4000000000" t="n"><v>3</v></c></row><row r="4294967295" ht="20"/></sheetData></worksheet>"#
    );
    let idx = index_of(&sheet);

    assert_eq!(idx.used_range_v1, Some((1, 1, 3, 2)), "v1 used range excludes the off-grid row");
    assert_eq!(idx.content_bounds, Some((1, 1, 3, 2)), "content bounds exclude it too");
    assert_eq!(idx.v1_range(), (1, 1, 3, 2));
    assert_eq!(idx.effective_range(), (1, 1, 3, 2));
    // The `@ht` fold in print_range is the one site that folds a RAW scanned row
    // number: DROP (max_row stays 3), never snap (which would give 1 048 576).
    assert_eq!(window::print_range(&idx), Some((1, 1, 3, 2)), "print range drops, does not snap");

    // And the materializer agrees with the index.
    let bytes = xlsx_with(&sheet, &sst(&[]));
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
    let rows = window::parse_rows_window(
        &r.indexes[0],
        1,
        MAX_ADDRESSABLE_ROW,
        &[],
        &r.stylesheet,
        r.color_ctx(),
    );
    assert_eq!(rows.iter().map(|rd| rd.row).collect::<Vec<_>>(), vec![1, 3]);
}

// ─── (4) the stamped `<dimension>` is a HINT, and is DEGRADED not clamped ────

/// §18.3.1.35: "This is optional and is not required." An unaddressable stamp is
/// therefore ignored outright and the extent derives from `sheetData` — the same
/// behaviour as the (conformant) no-`<dimension>` sheet. It is NOT rewritten to
/// `XFD1048576`: a stamp that disagrees with the content buys nothing.
#[test]
fn an_unaddressable_dimension_stamp_degrades_to_the_scanned_extent() {
    let cells = r#"<sheetData><row r="1"><c r="A1" t="n"><v>1</v></c><c r="B1" t="n"><v>2</v></c></row></sheetData>"#;
    let stamped =
        format!(r#"<worksheet xmlns="{NS}"><dimension ref="A1:ZZZZZZ1048577"/>{cells}</worksheet>"#);
    let bare = format!(r#"<worksheet xmlns="{NS}">{cells}</worksheet>"#);

    let a = index_of(&stamped);
    let b = index_of(&bare);
    assert_eq!(a.stamped_dimension, None, "the unusable hint is dropped, not clamped");
    assert_eq!(a.effective_range(), b.effective_range(), "identical to the no-dimension sheet");
    assert_eq!(a.effective_range(), (1, 1, 1, 2));
}

// ─── (5) OVER-REACH GUARD: the addressable corner is untouched ───────────────

/// Everything the bound admits must be admitted UNCHANGED. `XFD` / row
/// 1 048 576 is the corner Excel itself writes (select-all + merge), so a sheet
/// built entirely out of it must survive every range, every fold and the parser.
#[test]
fn the_addressable_corner_is_admitted_unchanged() {
    let sheet = format!(
        r#"<worksheet xmlns="{NS}"><dimension ref="A1:XFD1048576"/><sheetData><row r="1"><c r="XFD1" t="n"><v>1</v></c></row><row r="1048576" ht="20"><c r="A1048576" t="n"><v>2</v></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:XFD1048576"/></mergeCells></worksheet>"#
    );
    let idx = index_of(&sheet);
    let full = (1, 1, MAX_ADDRESSABLE_ROW, MAX_ADDRESSABLE_COL);
    assert_eq!(idx.stamped_dimension, Some(full));
    assert_eq!(idx.merges.len(), 1);
    let m = idx.merges[0];
    assert_eq!((m.min_row, m.min_col, m.max_row, m.max_col), full);
    assert_eq!((idx.rows[0].min_col, idx.rows[0].max_col), (MAX_ADDRESSABLE_COL, MAX_ADDRESSABLE_COL));
    assert_eq!(idx.rows[1].row, MAX_ADDRESSABLE_ROW);
    assert_eq!(idx.used_range_v1, Some(full));
    assert_eq!(idx.content_bounds, Some(full));
    assert_eq!(idx.v1_range(), full);
    assert_eq!(idx.effective_range(), full);
    assert_eq!(window::print_range(&idx), Some(full));

    // The parser: the corner in, one past it out (the `$` row marker the old
    // parser already stripped still strips).
    assert_eq!(parse_a1("XFD1048576"), Some((MAX_ADDRESSABLE_COL, MAX_ADDRESSABLE_ROW)));
    assert_eq!(parse_a1("XFD$1048576"), Some((MAX_ADDRESSABLE_COL, MAX_ADDRESSABLE_ROW)));
    assert_eq!(parse_a1("XFE1"), None, "one column past the addressable grid");
    assert_eq!(parse_a1("A1048577"), None, "one row past the addressable grid");
    assert_eq!(parse_a1("A0"), None, "§18.17.2.3.1: the row digit-sequence shall be non-zero");
    assert_eq!(parse_a1("A99999999999999999999"), None, "not representable");
}

/// The FULL RENDER of a merged workbook is unchanged by the merge clip, for BOTH
/// shapes the clip touches: the legitimate input that motivates it (Excel's
/// select-all merge, which spans far past the window on the column axis) and an
/// ordinary merge that fits inside it. Pinned by the emitted cells/merge boxes
/// AND the geometry hash, both measured before the clip existed, so a regression
/// shows up as a moved number rather than a silent reflow.
#[test]
fn merged_workbooks_render_identically() {
    let body = r#"<sheetData><row r="1"><c r="A1" t="n"><v>1</v></c><c r="B1" t="n"><v>2</v></c><c r="C1" t="n"><v>3</v></c></row><row r="2"><c r="A2" t="n"><v>4</v></c></row><row r="3"><c r="A3" t="n"><v>5</v></c></row></sheetData>"#;
    let render = |merge_ref: &str| {
        let sheet = format!(
            r#"<worksheet xmlns="{NS}"><dimension ref="A1:C3"/>{body}<mergeCells count="1"><mergeCell ref="{merge_ref}"/></mergeCells></worksheet>"#
        );
        let r = SmlRenderer::from_xlsx(xlsx_with(&sheet, &sst(&[]))).expect("from_xlsx");
        let p = r.window_page(0, &calibri_font_cache(), 1, 3, 0.0).expect("window page");
        let cells: Vec<(String, String, bool)> =
            p.cells.iter().map(|c| (c.ref_a1.clone(), c.text.clone(), c.is_merged_anchor)).collect();
        (cells, p.merges.len(), grid::geometry_hash(std::slice::from_ref(&p)))
    };

    // Excel's select-all merge: every visible cell is covered by it, so the only
    // survivor is the anchor. Nothing about that may change.
    assert_eq!(
        render("A1:XFD1048576"),
        (vec![("A1".to_string(), "1".to_string(), true)], 1, SELECT_ALL_MERGE_GEOM_HASH),
        "the select-all merge renders exactly as it did before the clip"
    );
    // An ordinary A1:C2 merge: B1/C1/A2 suppressed, A3 still painted.
    assert_eq!(
        render("A1:C2"),
        (
            vec![
                ("A1".to_string(), "1".to_string(), true),
                ("A3".to_string(), "5".to_string(), false),
            ],
            1,
            ORDINARY_MERGE_GEOM_HASH
        ),
        "an in-window merge renders exactly as it did before the clip"
    );
}

/// Geometry hashes measured on the engine without merge clipping and
/// asserted unchanged with it — the valid-document guard for the
/// merge clipping, which may only make the bookkeeping cheaper.
const SELECT_ALL_MERGE_GEOM_HASH: u64 = 3_053_905_167_861_565_682;
const ORDINARY_MERGE_GEOM_HASH: u64 = 10_284_817_456_822_257_108;
