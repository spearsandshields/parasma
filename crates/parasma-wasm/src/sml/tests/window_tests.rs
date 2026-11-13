//! Grid v2 — window-path tests (window materialization, the chunk LRU,
//! lazy print pagination, dimension-distrust and kill-the-void).
//!
//! The EQUIVALENCE INVARIANT, asserted directly: the v2 window path,
//! materialized over the FULL row range, reproduces the pinned v1
//! `continuous_page` geometry hashes EXACTLY (the constants below are the
//! SAME pins tests.rs asserts off the eager path — re-pinning = broken
//! equivalence). Plus the window edge cases: merges spanning a chunk/window
//! boundary (anchor outside the materialized window), synthetic-envelope
//! byte-boundary paranoia (chunk-edge rows, r-less at slice starts,
//! self-closing rows, the last row before `</sheetData>`, CDATA/quote fakes),
//! LRU eviction/reparse, the print clip, and dimension-distrust on
//! the grid extent.

use crate::engine::fonts::FontCache;
use crate::sml::grid;
use crate::sml::index::SheetIndex;
use crate::sml::model::{self, SheetModel};
use crate::sml::window::{self, ChunkStore, CHUNK_ROWS};
use crate::sml::SmlRenderer;

use super::perf::{stamped_xlsx, STAMPED_CI_ROWS};
use super::{
    build_xlsx_two_sheets, calibri_font_cache, drawing_tests, merge_tests, sst, xlsx_with,
    ORACLE_SHEET_XML, SPARSE_HEIGHTS_SHEET_XML,
};

const NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

fn renderer(bytes: Vec<u8>) -> SmlRenderer {
    SmlRenderer::from_xlsx(bytes).expect("from_xlsx")
}

/// Hash the v2 FULL-RANGE window of sheet 0 (the v1/v2 equivalence surface).
/// (`pub(super)`: hidden_tests reuses it for the hidden-rows/cols invariant.)
pub(super) fn full_window_hash(r: &SmlRenderer, fc: &FontCache) -> u64 {
    let gs = r.ensure_geom(0, fc).expect("geom");
    let (r0, r1) = (gs.geom.min_row, gs.geom.max_row);
    let p = r.window_page(0, fc, r0, r1, 0.0).expect("window page");
    grid::geometry_hash(std::slice::from_ref(&p))
}

/// Hash the EAGER v1 continuous page of the same workbook, built standalone
/// (shim load + from_resolved + continuous_page — the exact eager path).
/// (`pub(super)`: hidden_tests reuses it for the hidden-rows/cols invariant.)
pub(super) fn eager_continuous_hash(bytes: Vec<u8>, r: &SmlRenderer, fc: &FontCache) -> u64 {
    let mut wb = ooxml_sml::workbook::Workbook::from_reader(std::io::Cursor::new(bytes)).expect("open");
    let (sheets, _) = crate::sml::shim::load_sheets_tolerant(&mut wb);
    let ls = &sheets[0];
    let mut m = SheetModel::from_resolved(
        &ls.sheet,
        r.normal_font(),
        fc,
        ls.col_widths_degraded,
        &r.stylesheet,
        r.color_ctx(),
    );
    m.drawings = r.drawings.get(&m.name).cloned().unwrap_or_default();
    let p = grid::continuous_page(&m).expect("continuous page");
    grid::geometry_hash(std::slice::from_ref(&p))
}

// ─── the window path reproduces the PINNED continuous oracles ───────────────

#[test]
fn v2_window_reproduces_pinned_oracle_geometry() {
    let fc = calibri_font_cache();
    let r = renderer(xlsx_with(ORACLE_SHEET_XML, &sst(&["Header", "Footer"])));
    assert_eq!(
        full_window_hash(&r, &fc),
        0x5dba3a8136ccce88,
        "v2 window != pinned continuous oracle geometry"
    );
}

#[test]
fn v2_window_reproduces_pinned_merged_geometry() {
    // ht="22" → 88/3px (NOT binary-exact): the within-window SEQUENTIAL height
    // accumulation makes even this fixture hash-identical (the closed form is
    // only the window BASE, which is 0 for the full range).
    let fc = calibri_font_cache();
    let r = renderer(xlsx_with(merge_tests::MERGED_SHEET_XML, &sst(merge_tests::MERGED_STRINGS)));
    assert_eq!(
        full_window_hash(&r, &fc),
        0x834f4dd0cebb563c,
        "v2 window != pinned continuous merged geometry"
    );
}

#[test]
fn v2_window_reproduces_pinned_sparse_heights_geometry() {
    let fc = calibri_font_cache();
    let r = renderer(xlsx_with(SPARSE_HEIGHTS_SHEET_XML, &sst(&["Header"])));
    assert_eq!(
        full_window_hash(&r, &fc),
        0xdfda69175e7a0e61,
        "v2 window != pinned continuous sparse geometry"
    );
}

#[test]
fn v2_window_matches_eager_continuous_on_drawing_and_d2_fixtures() {
    // No pinned constant for these two classes — assert window == eager
    // continuous DIRECTLY (picture extension + image boxes; empty-sheet
    // 25×10 synthesis).
    let fc = calibri_font_cache();

    let bytes = drawing_tests::xlsx_with_drawing(drawing_tests::DRAWING_TWO_CELL);
    let r = renderer(bytes.clone());
    assert_eq!(
        full_window_hash(&r, &fc),
        eager_continuous_hash(bytes, &r, &fc),
        "v2 window != eager continuous (drawing fixture)"
    );

    let sheet1 = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="{NS}"><dimension ref="A1:B2"/><sheetFormatPr defaultRowHeight="15"/><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row><row r="2" ht="30" customHeight="1"><c r="B2" t="n"><v>8</v></c></row></sheetData></worksheet>"#
    );
    let sheet2 =
        format!(r#"<?xml version="1.0"?><worksheet xmlns="{NS}"><sheetFormatPr defaultRowHeight="15"/><sheetData/></worksheet>"#);
    let r = renderer(build_xlsx_two_sheets(&sheet1, &sheet2, &sst(&["x"])));
    // Sheet 2 (empty): full-range window over the synthesized 25×10 grid.
    let gs = r.ensure_geom(1, &fc).expect("geom");
    let p = r
        .window_page(1, &fc, gs.geom.min_row, gs.geom.max_row, 0.0)
        .expect("window page");
    assert_eq!(p.row_rails.len(), 26, "25 default rows + bottom rail");
    assert_eq!(p.col_rails.len(), 11, "10 default cols + right rail");
    assert!(p.cells.is_empty(), "the synthesized empty-sheet grid is empty");
}

// ─── merges across the chunk/window boundary ─────────────────────────────────

#[test]
fn merge_anchor_above_window_still_paints() {
    // Stamped fixture: merge A255:A258 spans the 256|257 chunk boundary. A
    // window starting at row 257 (chunk 1) must still paint the anchor from
    // chunk 0 — merges are husk metadata; the anchor's chunk is fetched.
    let fc = calibri_font_cache();
    let r = renderer(stamped_xlsx(STAMPED_CI_ROWS));
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let p = r.window_page(0, &fc, 257, 400, 0.0).expect("window page");

    let anchor = p.cells.iter().find(|c| c.ref_a1 == "A255").expect("A255 anchor in window");
    assert!(anchor.is_merged_anchor, "anchor flagged");
    assert_eq!(anchor.text, "Beta", "anchor content materialized from chunk 0");
    assert!(anchor.h >= 4.0 * 20.0 - 0.01, "anchor spans the full 4-row region");
    assert!(
        p.merges.iter().any(|m| (m.y - gs.geom.row_top(255)).abs() < 0.01),
        "the region's MergeBox is present at the anchor's absolute y"
    );
    // The widen-for-anchors window query finds it from a y INSIDE the window.
    let y0 = gs.geom.row_top(257) + 1.0;
    let visible = grid::visible_cells(&p, y0, y0 + 200.0);
    assert!(visible.iter().any(|c| c.ref_a1 == "A255"), "visible_cells widens to the anchor");
    // Rails start at the window row, NOT the anchor.
    assert_eq!(p.row_numbers.first(), Some(&257));
}

#[test]
fn grid_cell_at_resolves_covered_cell_to_its_anchor() {
    let fc = calibri_font_cache();
    let r = renderer(stamped_xlsx(STAMPED_CI_ROWS));
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let gut_w = grid::row_gutter_width_for_max_row(gs.geom.max_row);
    let gut_h = grid::COL_GUTTER_H;
    // A point inside covered cell A257 (row 257, col A) — across the chunk
    // boundary from its anchor.
    let hit = r
        .grid_cell_at(
            0,
            gut_w + gs.geom.col_left(1) + 2.0,
            gut_h + 2.0,
            0.0,
            gs.geom.row_top(257) + 1.0,
            &fc,
        )
        .expect("covered cell resolves");
    assert_eq!(hit.0, "A255", "covered cell resolves to the region anchor");
    assert_eq!(hit.2, "Beta");
    // A plain cell hit (B257 carries the row number value).
    let hit = r
        .grid_cell_at(
            0,
            gut_w + gs.geom.col_left(2) + 2.0,
            gut_h + 2.0,
            0.0,
            gs.geom.row_top(257) + 1.0,
            &fc,
        )
        .expect("plain cell");
    assert_eq!(hit.0, "B257");
    assert_eq!(hit.2, "257");
    // Beyond the extent → None (v1: no cell box matched).
    assert!(r
        .grid_cell_at(0, gut_w + gs.geom.content_w + 50.0, gut_h + 2.0, 0.0, 0.0, &fc)
        .is_none());
}

// ─── envelope byte-boundary paranoia ─────────────────────────────────────────

fn idx_of(body: &str) -> SheetIndex {
    let xml =
        format!(r#"<worksheet xmlns="{NS}"><sheetData>{body}</sheetData></worksheet>"#);
    SheetIndex::build("T".into(), xml.into_bytes()).expect("index build")
}

/// Parse rows via N chunk-aligned envelope slices and via ONE full-range
/// envelope; both must equal the same rows (numbering, heights, cells).
fn assert_sliced_equals_whole(idx: &SheetIndex, max_row: u32) {
    let ss: Vec<String> = vec!["S0".into(), "S1".into()];
    let st = ooxml_sml::types::Stylesheet::default();
    let cc = model::ColorCtx::default();
    let whole = window::parse_rows_window(idx, 1, max_row, &ss, &st, &cc);
    let mut sliced = Vec::new();
    let mut c = 0;
    while c * CHUNK_ROWS < max_row {
        let (r0, r1) = window::chunk_rows(c);
        sliced.extend(window::parse_rows_window(idx, r0, r1.min(max_row), &ss, &st, &cc));
        c += 1;
    }
    assert!(!whole.is_empty(), "paranoia fixture must parse rows");
    assert_eq!(sliced, whole, "chunked slice parses != whole-range parse");
}

#[test]
fn envelope_chunk_edge_rows_and_last_row_before_close() {
    // Rows exactly AT the chunk edges (256, 257, 512) + the very last row
    // abutting </sheetData>; self-closing rows at edges.
    let mut body = String::new();
    for r in [1u32, 255, 256, 257, 511, 512, 513, 600] {
        if r % 2 == 0 {
            body.push_str(&format!(r#"<row r="{r}"/>"#)); // self-closing at edges
        } else {
            body.push_str(&format!(r#"<row r="{r}"><c r="B{r}" t="n"><v>{r}</v></c></row>"#));
        }
    }
    let idx = idx_of(&body);
    // Byte ends are exact: the last row's end abuts </sheetData>.
    assert_eq!(idx.rows.last().unwrap().byte_end as usize, idx.sheet_data.end);
    assert_sliced_equals_whole(&idx, 600);
}

#[test]
fn envelope_rless_rows_at_slice_starts_resync_numbering() {
    // An r-less RUN crossing the 256-boundary: rows 250..=260 written with ONE
    // explicit r=250 then bare <row>. The chunk-1 slice starts at an r-less
    // row (implied 257) — the envelope must stamp r="257" or the slice would
    // renumber from 1.
    let mut body = String::new();
    body.push_str(r#"<row r="250"><c r="A250" t="n"><v>250</v></c></row>"#);
    for i in 251..=260u32 {
        body.push_str(&format!(r#"<row><c r="A{i}" t="n"><v>{i}</v></c></row>"#));
    }
    let idx = idx_of(&body);
    let ss: Vec<String> = vec![];
    let st = ooxml_sml::types::Stylesheet::default();
    let cc = model::ColorCtx::default();
    // Chunk 1 alone: rows 257..=260, numbered correctly via the stamped r.
    let (r0, r1) = window::chunk_rows(1);
    let rows = window::parse_rows_window(&idx, r0, r1, &ss, &st, &cc);
    let nums: Vec<u32> = rows.iter().map(|r| r.row).collect();
    assert_eq!(nums, vec![257, 258, 259, 260], "slice-start r-less row resynced");
    assert_eq!(rows[0].cells[0].raw, "257", "the right cell content followed");
    assert_sliced_equals_whole(&idx, 260);
}

#[test]
fn envelope_cdata_quotes_and_prefixed_streams_survive_slicing() {
    // CDATA containing fake </sheetData>/<row> markup, single-quoted attrs,
    // '>' inside attribute values — sliced at a chunk boundary they must parse
    // to the same rows as the whole (quote-aware scanning discipline).
    let mut body = String::new();
    body.push_str(
        r#"<row r="1"><c r="A1" t="inlineStr"><is><t><![CDATA[</sheetData><row r="9"><c r="Z9"/>]]></t></is></c></row>"#,
    );
    body.push_str(r#"<row r='2' ht='30.75' x="a>b"><c r='C2' t='s'><v>1</v></c></row>"#);
    body.push_str(&format!(r#"<row r="300"><c r="D300" t="s"><v>0</v></c></row>"#));
    let idx = idx_of(&body);
    let ss: Vec<String> = vec!["S0".into(), "S1".into()];
    let st = ooxml_sml::types::Stylesheet::default();
    let cc = model::ColorCtx::default();
    let rows = window::parse_rows_window(&idx, 1, 2, &ss, &st, &cc);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].cells[0].text.contains("</sheetData>"), "CDATA text intact through the slice");
    assert_eq!(rows[1].height_pt, Some(30.75), "single-quoted @ht");
    assert_eq!(rows[1].cells[0].text, "S1", "shared string resolved in the slice");
    assert_sliced_equals_whole(&idx, 300);

    // Prefixed .NET stream: the envelope reuses the ORIGINAL husk bytes, so
    // <x:row> slices keep their namespace declaration.
    let xml = format!(
        r#"<x:worksheet xmlns:x="{NS}"><x:sheetData><x:row r="1"><x:c r="A1" t="n"><x:v>7</x:v></x:c></x:row><x:row><x:c r="A2" t="n"><x:v>8</x:v></x:c></x:row></x:sheetData></x:worksheet>"#
    );
    let idx = SheetIndex::build("X".into(), xml.into_bytes()).expect("index");
    let rows = window::parse_rows_window(&idx, 2, 2, &[], &st, &cc);
    assert_eq!(rows.len(), 1, "r-less prefixed row sliced alone");
    assert_eq!(rows[0].row, 2, "stamped r on a PREFIXED r-less row");
    assert_eq!(rows[0].cells[0].raw, "8");
}

// ─── LRU eviction / reparse ──────────────────────────────────────────────────

#[test]
fn chunk_lru_evicts_oldest_and_rebuilds_identically() {
    let mut store = ChunkStore::new(2);
    let mut builds = 0usize;
    let mut get = |store: &mut ChunkStore, key: (usize, u32), builds: &mut usize| {
        store.get_or_build(key, || {
            *builds += 1;
            vec![]
        })
    };
    get(&mut store, (0, 0), &mut builds);
    get(&mut store, (0, 1), &mut builds);
    get(&mut store, (0, 0), &mut builds); // touch (0,0) → (0,1) becomes LRU
    get(&mut store, (0, 2), &mut builds); // evicts (0,1)
    assert_eq!(store.len(), 2, "capacity bound holds");
    assert!(store.contains((0, 0)) && store.contains((0, 2)));
    assert!(!store.contains((0, 1)), "least-recently-used chunk evicted");
    assert_eq!(builds, 3, "hits don't rebuild");
    get(&mut store, (0, 1), &mut builds);
    assert_eq!(builds, 4, "evicted chunk reparses on demand");
}

#[test]
fn evicted_chunks_rematerialize_identical_windows() {
    // Scroll far past the LRU capacity, then return: the re-windowed page must
    // equal the original EXACTLY (closed-form base + deterministic parse).
    let fc = calibri_font_cache();
    let r = renderer(stamped_xlsx(STAMPED_CI_ROWS));
    let first = r.window_page(0, &fc, 250, 300, 0.0).expect("window");
    // Touch enough distinct chunks to evict chunk 0/1 (capacity 64 → use the
    // whole 2048-row fixture's 8 chunks repeatedly won't evict; force eviction
    // through a dedicated store instead is covered above — here we just assert
    // determinism across repeated windows).
    for c in 0..(STAMPED_CI_ROWS / CHUNK_ROWS) {
        let (r0, r1) = window::chunk_rows(c);
        let _ = r.window_page(0, &fc, r0, r1.min(STAMPED_CI_ROWS), 0.0);
    }
    let again = r.window_page(0, &fc, 250, 300, 0.0).expect("window");
    assert_eq!(first, again, "re-materialized window drifted");
}

// ─── kill-the-void: rails extend past the extent ─────────────────────────────

#[test]
fn window_extends_rails_beyond_extent_with_no_cells() {
    let fc = calibri_font_cache();
    let r = renderer(xlsx_with(ORACLE_SHEET_XML, &sst(&["Header", "Footer"])));
    let gs = r.ensure_geom(0, &fc).expect("geom");
    assert_eq!(gs.geom.max_row, 3, "oracle content is 3 rows");
    // Ask for rows 1..50 and 2000px of width — way past the A1:C3 content.
    let p = r.window_page(0, &fc, 1, 50, 2000.0).expect("window");
    assert_eq!(p.row_rails.len(), 51, "void rows get rails");
    assert_eq!(*p.row_numbers.last().unwrap(), 50);
    assert!(*p.col_rails.last().unwrap() >= 2000.0, "void columns extend to the requested x");
    // Void rows beyond row 3 contribute NO cells.
    assert!(p.cells.iter().all(|c| {
        let row: u32 = c.ref_a1.trim_start_matches(|ch: char| ch.is_ascii_alphabetic()).parse().unwrap();
        row <= 3
    }));
    // Default-height void rows: uniform 20px bands beyond the content.
    let k = 10;
    assert!((p.row_rails[k + 1] - p.row_rails[k] - 20.0).abs() < 1e-9);
}

// ─── dimension-distrust on the grid extent ───────────────────────────────────

#[test]
fn overstating_stamp_drives_grid_extent_but_print_clips_to_content() {
    // Stamp claims A1:J1048576; content is 3 rows. Grid: the FULL stamped
    // extent (Excel-faithful scrollbar). Print: clipped to content.
    let total = 1_048_576u32;
    let sheet = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="{NS}"><dimension ref="A1:J{total}"/><sheetFormatPr defaultRowHeight="15"/><sheetData><row r="1"><c r="A1" t="n"><v>1</v></c></row><row r="2"><c r="B2" t="n"><v>2</v></c></row><row r="3"><c r="C3" t="n"><v>3</v></c></row></sheetData></worksheet>"#
    );
    let fc = calibri_font_cache();
    let r = renderer(xlsx_with(&sheet, &sst(&[])));
    let (_w, h) = r.grid_extent(0, &fc).expect("extent");
    assert!(
        h >= total as f64 * 20.0,
        "grid extent must honor the stamped 1M rows (got {h}px)"
    );
    // Print clips to the value-bearing range → ONE page, 3 row bands.
    let pages = r.paginate_all(&fc);
    assert_eq!(pages.len(), 1, "print clipped to content, not 14k+ stamped pages");
    assert_eq!(pages[0].row_numbers, vec![1, 2, 3]);
}

#[test]
fn understating_stamp_extends_grid_extent_to_content() {
    // Stamp claims A1:B2 but content reaches E10 → nothing is cut off.
    let sheet = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="{NS}"><dimension ref="A1:B2"/><sheetFormatPr defaultRowHeight="15"/><sheetData><row r="1"><c r="A1" t="n"><v>1</v></c></row><row r="10"><c r="E10" t="n"><v>5</v></c></row></sheetData></worksheet>"#
    );
    let fc = calibri_font_cache();
    let r = renderer(xlsx_with(&sheet, &sst(&[])));
    let gs = r.ensure_geom(0, &fc).expect("geom");
    assert_eq!((gs.geom.max_row, gs.geom.max_col), (10, 5), "extent covers the scanned content");
    // And the E10 cell is reachable in a window at its closed-form position.
    let p = r.window_page(0, &fc, 8, 10, 0.0).expect("window");
    assert!(p.cells.iter().any(|c| c.ref_a1 == "E10"));
}

// ─── print clip on trailing valueless rows (a shape real workbooks have) ────

#[test]
fn print_clips_trailing_physically_present_empty_rows() {
    // Content in rows 1..=3, then hundreds of physically-present EMPTY stamped
    // rows (the .NET-exporter padding class). v1 paginates the void; v2 print
    // clips to the value-bearing range. The GRID extent keeps every row.
    let mut body = String::new();
    for r in 1..=3u32 {
        body.push_str(&format!(r#"<row r="{r}"><c r="A{r}" t="n"><v>{r}</v></c></row>"#));
    }
    for r in 4..=600u32 {
        body.push_str(&format!(r#"<row r="{r}" customFormat="1"/>"#));
    }
    let sheet = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="{NS}"><dimension ref="A1:A600"/><sheetFormatPr defaultRowHeight="15"/><sheetData>{body}</sheetData></worksheet>"#
    );
    let fc = calibri_font_cache();
    let r = renderer(xlsx_with(&sheet, &sst(&[])));
    let pages = r.paginate_all(&fc);
    assert_eq!(pages.len(), 1, "trailing valueless rows don't paginate");
    assert_eq!(*pages[0].row_numbers.last().unwrap(), 3);
    // Grid: all 600 rows stay on the scrollbar.
    let (_w, h) = r.grid_extent(0, &fc).expect("extent");
    assert!((h - (600.0 * 20.0 + grid::COL_GUTTER_H)).abs() < 0.01);
}

#[test]
fn print_range_equals_v1_range_on_truthful_fixtures() {
    // Pin safety: where nothing overstates, the print clip IS the v1 range —
    // the pinned paged baselines hold by construction.
    for (bytes, what) in [
        (xlsx_with(ORACLE_SHEET_XML, &sst(&["Header", "Footer"])), "oracle"),
        (xlsx_with(SPARSE_HEIGHTS_SHEET_XML, &sst(&["Header"])), "sparse"),
        (
            xlsx_with(merge_tests::MERGED_SHEET_XML, &sst(merge_tests::MERGED_STRINGS)),
            "merged",
        ),
        (stamped_xlsx(STAMPED_CI_ROWS), "stamped-ci"),
    ] {
        let r = renderer(bytes);
        let idx = &r.indexes[0];
        assert_eq!(
            window::print_range(idx),
            idx.used_range_v1,
            "{what}: print clip must equal the v1 range on truthful content"
        );
    }
}

// ─── ht-only rows are value-bearing for print (formatting prints) ────────────

#[test]
fn print_keeps_trailing_custom_height_rows() {
    let sheet = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="{NS}"><sheetFormatPr defaultRowHeight="15"/><sheetData><row r="1"><c r="A1" t="n"><v>1</v></c></row><row r="40" ht="60" customHeight="1"/></sheetData></worksheet>"#
    );
    let fc = calibri_font_cache();
    let r = renderer(xlsx_with(&sheet, &sst(&[])));
    let pages = r.paginate_all(&fc);
    assert_eq!(*pages[0].row_numbers.last().unwrap(), 40, "the ht row prints");
}

// ─── lazy pagination ≡ the eager pagination (normal fixtures) ──────────────

#[test]
fn lazy_pagination_matches_eager_on_normal_fixtures() {
    // The pagination equivalence check: on every truthful (non-overstating) fixture,
    // the index-driven lazy pagination (materialize_model over the print
    // range → paginate) reproduces the eager pagination (shim full load →
    // from_resolved → paginate) — same page COUNT, hash-identical GEOMETRY.
    let fc = calibri_font_cache();
    for (bytes, what) in [
        (xlsx_with(ORACLE_SHEET_XML, &sst(&["Header", "Footer"])), "oracle"),
        (xlsx_with(SPARSE_HEIGHTS_SHEET_XML, &sst(&["Header"])), "sparse"),
        (
            xlsx_with(merge_tests::MERGED_SHEET_XML, &sst(merge_tests::MERGED_STRINGS)),
            "merged",
        ),
        (drawing_tests::xlsx_with_drawing(drawing_tests::DRAWING_TWO_CELL), "drawing"),
        (stamped_xlsx(STAMPED_CI_ROWS), "stamped-ci"),
    ] {
        let r = renderer(bytes.clone());
        let lazy = r.paginate_all(&fc);

        // The eager path, built standalone: full materialization of every
        // sheet, then the same paginate over the same page box.
        let mut wb = ooxml_sml::workbook::Workbook::from_reader(std::io::Cursor::new(bytes)).expect("open");
        let (sheets, _) = crate::sml::shim::load_sheets_tolerant(&mut wb);
        let mut eager = Vec::new();
        for ls in &sheets {
            let mut m = SheetModel::from_resolved(
                &ls.sheet,
                r.normal_font(),
                &fc,
                ls.col_widths_degraded,
                &r.stylesheet,
                r.color_ctx(),
            );
            m.drawings = r.drawings.get(&m.name).cloned().unwrap_or_default();
            eager.extend(grid::paginate(&m, r.page_box()));
        }

        assert_eq!(lazy.len(), eager.len(), "{what}: lazy vs eager page count");
        assert_eq!(
            grid::geometry_hash(&lazy),
            grid::geometry_hash(&eager),
            "{what}: lazy pagination geometry != eager pagination geometry"
        );
    }
}

// ─── LAZY pagination runtime path: skeletons at load, fill on paint ─────────

/// Poll an async future to completion with a no-op waker — the native
/// `yield_to_browser` is immediately ready, so `run_layout` never suspends
/// (the lib.rs `run_layout_native` precedent).
fn poll_ready<F: std::future::Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn raw() -> RawWaker {
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            raw()
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    let waker = unsafe { Waker::from_raw(raw()) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = Box::pin(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(out) => out,
        Poll::Pending => unreachable!("native layout future must not suspend"),
    }
}

/// Drive the RUNTIME lazy-pagination path natively: `run_layout` (skeleton stream) into a
/// shared page vec, returning `(page_count, pages_cell)`. (`pub(super)`: the
/// perf harness drives the same path over the 1M fixture.)
pub(super) fn run_layout_native(
    r: &SmlRenderer,
    fc: &FontCache,
) -> (u32, std::cell::RefCell<Vec<grid::SheetPage>>) {
    let pages = std::cell::RefCell::new(Vec::new());
    let layout_gen = std::cell::Cell::new(0u64);
    let count = poll_ready(r.run_layout(&pages, &layout_gen, fc, |_| Ok(())))
        .expect("run_layout");
    (count, pages)
}

/// THE lazy-pagination runtime pin: `run_layout` streams metadata-only skeletons (page
/// count + sheet identity intact, ZERO cell parsing at load), and first-paint
/// materialization (`ensure_page`, the chunk path) reproduces the eager
/// `paginate_all` geometry hash-identically on every fixture class — page
/// boundaries are shared `plan_pages` math, cells are the v1-equivalent
/// window content.
#[test]
fn lazy_run_layout_skeletons_fill_to_eager_geometry() {
    let fc = calibri_font_cache();
    let two_sheets = {
        let s1 = format!(
            r#"<?xml version="1.0"?><worksheet xmlns="{NS}"><dimension ref="A1:B2"/><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row><row r="2"><c r="B2" t="n"><v>7</v></c></row></sheetData></worksheet>"#
        );
        let s2 = format!(
            r#"<?xml version="1.0"?><worksheet xmlns="{NS}"><dimension ref="A1:A1"/><sheetData><row r="1"><c r="A1" t="n"><v>1</v></c></row></sheetData></worksheet>"#
        );
        build_xlsx_two_sheets(&s1, &s2, &sst(&["x"]))
    };
    for (bytes, what) in [
        (xlsx_with(ORACLE_SHEET_XML, &sst(&["Header", "Footer"])), "oracle"),
        (xlsx_with(SPARSE_HEIGHTS_SHEET_XML, &sst(&["Header"])), "sparse"),
        (
            xlsx_with(merge_tests::MERGED_SHEET_XML, &sst(merge_tests::MERGED_STRINGS)),
            "merged",
        ),
        (drawing_tests::xlsx_with_drawing(drawing_tests::DRAWING_TWO_CELL), "drawing"),
        (stamped_xlsx(STAMPED_CI_ROWS), "stamped-ci"),
        (two_sheets, "two-sheets"),
    ] {
        let r = renderer(bytes);
        let eager = r.paginate_all(&fc);
        let (count, pages_cell) = run_layout_native(&r, &fc);

        assert_eq!(count as usize, eager.len(), "{what}: streamed page count");
        // Load streamed SKELETONS only — no chunk parse happened (the
        // key invariant: the main thread owns no O(print-range) work).
        assert_eq!(r.chunk_count(), 0, "{what}: cells were parsed at load");
        // Sheet identity is already correct on skeletons (sheet_spans/tabs).
        {
            let pages = pages_cell.borrow();
            let skel_spans = grid::sheet_spans(&pages);
            let eager_spans = grid::sheet_spans(&eager);
            assert_eq!(skel_spans.len(), eager_spans.len(), "{what}: span count");
            for (a, b) in skel_spans.iter().zip(&eager_spans) {
                assert_eq!((a.name.as_str(), a.first_page, a.page_count),
                           (b.name.as_str(), b.first_page, b.page_count), "{what}: spans");
            }
        }
        // First paint of each page materializes it; the filled pages must be
        // hash-identical to the eager pagination (count + geometry + content).
        for i in 0..count as usize {
            r.ensure_page(i, &pages_cell);
        }
        let filled = pages_cell.borrow();
        assert_eq!(
            grid::geometry_hash(&filled),
            grid::geometry_hash(&eager),
            "{what}: lazily-filled pages != eager pagination geometry"
        );
        // Idempotence: re-ensuring a filled page is a no-op (the per-page cache).
        drop(filled);
        r.ensure_page(0, &pages_cell);
        assert_eq!(
            grid::geometry_hash(&pages_cell.borrow()),
            grid::geometry_hash(&eager),
            "{what}: ensure_page must be idempotent"
        );
    }
}

/// A single mid-document page materializes alone (scrolling Print pages paints
/// just the visible ones), resolving through the SAME chunk LRU as the grid.
#[test]
fn lazy_page_fill_is_per_page_and_shares_the_chunk_lru() {
    let fc = calibri_font_cache();
    let r = renderer(stamped_xlsx(STAMPED_CI_ROWS));
    let eager = r.paginate_all(&fc);
    let (count, pages_cell) = run_layout_native(&r, &fc);
    assert_eq!(count as usize, eager.len());
    // Fill ONLY the page holding the chunk-boundary merge (rows ~255..258).
    let target = eager
        .iter()
        .position(|p| p.row_numbers.contains(&255))
        .expect("page containing row 255");
    r.ensure_page(target, &pages_cell);
    {
        let pages = pages_cell.borrow();
        assert_eq!(
            grid::geometry_hash(std::slice::from_ref(&pages[target])),
            grid::geometry_hash(std::slice::from_ref(&eager[target])),
            "single-page fill must match the eager page"
        );
        // Other pages are still skeletons (per-page, not per-sheet, fill).
        assert!(pages[target + 1].row_rails.is_empty(), "neighbor stayed a skeleton");
    }
    let after_fill = r.chunk_count();
    assert!(after_fill >= 1, "the fill parsed through the chunk LRU");
    // A grid window over the same rows REUSES the chunks (no new parse).
    let _ = r.window_page(0, &fc, 255, 258, 0.0).expect("grid window");
    assert_eq!(r.chunk_count(), after_fill, "grid view shares the paged fill's chunks");
}

// ─── zoom-in-grid window math (content-space viewport division) ─────────────

#[test]
fn grid_window_scales_with_zoom_division() {
    // The adapter/painter divide the CSS-px viewport by the zoom scale before
    // the window math (the engine is otherwise zoom-blind — one ctx transform).
    let fc = calibri_font_cache();
    let r = renderer(stamped_xlsx(STAMPED_CI_ROWS));
    let gs = r.ensure_geom(0, &fc).expect("geom");
    let geom = &gs.geom;

    let (vw, vh) = (800.0, 600.0);
    let (r0_1, r1_1, x1_1) = crate::sml::grid_window(geom, 0.0, 0.0, vw, vh);
    // 50% zoom → content viewport ×2 → MORE rows + a wider void extension.
    let (r0_h, r1_h, x1_h) = crate::sml::grid_window(geom, 0.0, 0.0, vw / 0.5, vh / 0.5);
    // 150% zoom → content viewport ÷1.5 → FEWER rows, narrower.
    let (_, r1_x, x1_x) = crate::sml::grid_window(geom, 0.0, 0.0, vw / 1.5, vh / 1.5);

    assert_eq!((r0_1, r0_h), (1, 1), "window top row is scroll-driven, not zoom-driven");
    assert!(r1_h > r1_1 && r1_1 > r1_x, "rows covered: 50% > 100% > 150%");
    assert!(x1_h > x1_1 && x1_1 > x1_x, "void right edge follows the content viewport");
    // Exact: default 20px rows, 20px col gutter → 100% covers (600−20)/20 = 29
    // full bands; row_at_y(580) is the band containing that bottom edge.
    assert_eq!(r1_1, geom.row_at_y(580.0));
    assert_eq!(r1_h, geom.row_at_y(1180.0));

    // Kill-the-void cap: a zoomed-out window deep past the extent still clamps
    // at the §18 sheet limit.
    let deep_y = geom.content_h + 1e9;
    let (_, r1_cap, _) = crate::sml::grid_window(geom, 0.0, deep_y, vw / 0.25, 1e9);
    assert_eq!(r1_cap, 1_048_576, "void rows cap at the sheet limit");
}
