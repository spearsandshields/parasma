//! SML recorded-paint-stream goldens.
//!
//! WHY, measured: perturbing `measure_cell_text`'s advance by 5% (`sml/render.rs`,
//! downstream of the MDW rails so the column geometry does NOT move) reddens the
//! paint-stream pins and NOTHING else — every `grid::geometry_hash` pin stays
//! green, because that hash folds cell box rects and the cell `text` string but
//! never the measured advance. It is the SML analogue of the WML LINE stream and
//! it is blind on exactly the same axis.
//!
//! So the paint stream is the only SML instrument that sees shaping, and it was
//! pinned in two places only, both as hex digests, and only on the GRID path.
//! This module adds:
//!  1. `.ops.txt` goldens over the same three grid streams the digests cover — a
//!     drift here names the first differing draw command instead of printing two
//!     unequal 64-bit numbers (`oracle_tests.rs:822` argues at length that an
//!     unreviewable digest re-baseline is the hazard; SML carried exactly that
//!     form);
//!  2. the PRINT path (`render::paint_sheet_page`), which shares
//!     `measure_cell_text` with the grid and had NO byte-identity pin at all.
//!
//! DELIBERATE OVERLAP (declared): the two grid goldens pin the same streams as
//! `pane_tests`'s `ops_hash` assertions. They are kept side by side rather than
//! collapsed because the goldens were added strictly additively — removing a live
//! assertion to install a new one is exactly the move that lets a regression
//! through if the new one is wrong. The digests should be deleted in favour of the goldens once
//! these have proven themselves; they pin nothing the goldens do not.

use std::collections::HashMap;

use crate::engine::displaylist::RecordingPainter;
use crate::ops_golden::assert_ops_golden;
use crate::sml::grid;
use crate::sml::render;
use crate::sml::SmlRenderer;

use super::pane_tests::paint_ops;
use super::{calibri_font_cache, merge_tests, sst, xlsx_with, ORACLE_SHEET_XML};

/// What a drift in an SML paint stream means, for the panic message. The cell
/// RAILS are pinned separately by `grid::geometry_hash`, so if those are still
/// green the boxes did not move — the text, its measured advance, or the
/// alignment that consumes the advance did.
const SML_DIAGNOSIS: &str = "the grid geometry hashes are a SEPARATE assertion, so if they are \
     still green no cell BOX moved; the painted text, the shaped advance behind a \
     right/centre-aligned cell's pen x, or the draw ORDER did";

#[test]
fn grid_paint_stream_golden_oracle_fixture() {
    let fc = calibri_font_cache();
    let r = SmlRenderer::from_xlsx(xlsx_with(ORACLE_SHEET_XML, &sst(&["Header", "Footer"])))
        .expect("from_xlsx");
    // Two scroll positions, the same pair the digest pins use: unscrolled proves
    // the base stream, scrolled proves the window offset rides along with it.
    assert_ops_golden(
        "sml-grid-oracle",
        &paint_ops(&r, &fc, 0.0, 0.0, 640.0, 480.0, None),
        SML_DIAGNOSIS,
    );
    assert_ops_golden(
        "sml-grid-oracle-scrolled",
        &paint_ops(&r, &fc, 37.0, 61.5, 640.0, 480.0, None),
        SML_DIAGNOSIS,
    );
}

#[test]
fn grid_paint_stream_golden_merged_fixture_with_selection() {
    let fc = calibri_font_cache();
    let r = SmlRenderer::from_xlsx(xlsx_with(
        merge_tests::MERGED_SHEET_XML,
        &sst(merge_tests::MERGED_STRINGS),
    ))
    .expect("from_xlsx");
    let sel = crate::sml::select::CellSelection {
        sheet: 0,
        range: crate::sml::select::CellRange::new(1, 1, 3, 2),
        anchor_row: 1,
        anchor_col: 1,
    };
    assert_ops_golden(
        "sml-grid-merged-selection",
        &paint_ops(&r, &fc, 10.0, 20.0, 500.0, 400.0, Some(sel)),
        SML_DIAGNOSIS,
    );
}

#[test]
fn print_page_paint_stream_golden() {
    // The print path was structurally unpinned: `paint_sheet_page` shares
    // `measure_cell_text` with the grid painter, but only the grid stream was
    // hashed, so a print-only regression (gutters, page background, the
    // full-page cell walk that skips the grid's windowing) had no byte-identity
    // pin anywhere. Same fixture as the grid golden, so the two streams are
    // directly comparable when one moves and the other does not.
    let fc = calibri_font_cache();
    let r = SmlRenderer::from_xlsx(xlsx_with(ORACLE_SHEET_XML, &sst(&["Header", "Footer"])))
        .expect("from_xlsx");
    let model = r.materialize_model(0, &fc).expect("model");
    let pages = grid::paginate(&model, grid::PageBox::default());
    assert_eq!(pages.len(), 1, "the oracle fixture fits one print page");
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
    assert_ops_golden("sml-print-oracle", &rec.take(), SML_DIAGNOSIS);
}
