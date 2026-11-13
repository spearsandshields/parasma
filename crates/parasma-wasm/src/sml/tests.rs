//! Native tests for the SML grid model + geometry + `<col>` containment shim.
//!
//! All tests are web-sys-free: they build minimal `.xlsx` packages in-test with
//! the `zip` dev-dependency (no binary fixtures in git; bytes reproducible from
//! this source) and drive `Workbook::from_reader(Cursor::new(bytes))`. Geometry
//! baselines are pinned after confirming determinism across two runs.
//!
//! Spec vectors asserted here:
//!  - §18.3.1.13  the literal 8.7109375 → 61px width vector (Calibri/Arimo MDW=7).
//!  - §18.3.1.35  dimension / used-range walk + sparse iteration.
//!  - §18.3.1.73  row@ht in points → px.
//!  - §18.3.1.81  sheetFormatPr defaults.
//!  - §18.3.1.88  showGridLines default true.

mod bounds_tests;
mod hidden_tests;
mod index_tests;
mod ops_golden_tests;
mod pane_tests;
mod perf;
mod window_tests;

use std::io::{Cursor, Write};

use ooxml_sml::workbook::Workbook;

use super::grid::{self, chars_to_file_width, file_width_to_px, PageBox};
use super::model::{measure_mdw, HAlign, SheetModel};
use super::shim;
use super::SmlRenderer;
use crate::engine::fonts::FontCache;

// ─── xlsx builder ────────────────────────────────────────────────────────────

/// Zip the given parts into a stored (uncompressed, deterministic) .xlsx blob.
fn build_xlsx(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, content) in parts {
        zw.start_file(*name, opts).expect("zip start_file");
        zw.write_all(content.as_bytes()).expect("zip write");
    }
    zw.finish().expect("zip finish").into_inner()
}

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>"#;

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;

const WORKBOOK: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;

const WORKBOOK_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

const STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>
  <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
  <borders count="1"><border/></borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  <cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs>
</styleSheet>"#;

/// Build an xlsx with the given worksheet XML body and shared-strings table.
fn xlsx_with(sheet_xml: &str, shared_strings: &str) -> Vec<u8> {
    build_xlsx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("xl/workbook.xml", WORKBOOK),
        ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS),
        ("xl/styles.xml", STYLES),
        ("xl/sharedStrings.xml", shared_strings),
        ("xl/worksheets/sheet1.xml", sheet_xml),
    ])
}

fn sst(strings: &[&str]) -> String {
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<sst xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" count=\"{}\" uniqueCount=\"{}\">",
        strings.len(),
        strings.len()
    );
    for st in strings {
        s.push_str(&format!("<si><t>{st}</t></si>"));
    }
    s.push_str("</sst>");
    s
}

/// Arimo's measured Maximum Digit Width at 11pt @96dpi, in whole pixels. Arimo
/// is the vendored Calibri *layout* substitute, but its DIGIT advance is wider
/// than Calibri's (which is the spec's MDW=7 reference). We measure whatever the
/// loaded font actually is — so the geometry tests assert against this measured
/// value, while the pure §18.3.1.13 formula vector (8.7109375→61px) is asserted
/// separately with the spec's literal MDW=7 in `width_formula_spec_vector_*`.
const ARIMO_MDW: f64 = 9.0;

/// A FontCache with Arimo loaded under the "Calibri" family name (the normal
/// style declares Calibri; Arimo is the vendored substitute). MDW is measured
/// from it — see [`ARIMO_MDW`].
fn calibri_font_cache() -> FontCache {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
    let bytes = std::fs::read(format!("{dir}/Arimo-Regular.ttf")).unwrap_or_else(|_| {
        panic!("test font missing: Arimo-Regular.ttf\n  run: ./scripts/fetch-test-fonts.sh")
    });
    let mut fc = FontCache::new();
    fc.insert("Calibri", bytes, 400, false).expect("insert Arimo as Calibri");
    fc
}

fn load_model(sheet_xml: &str, strings: &[&str], fc: &FontCache) -> SheetModel {
    let bytes = xlsx_with(sheet_xml, &sst(strings));
    let mut wb = Workbook::from_reader(Cursor::new(bytes)).expect("workbook open");
    let normal = super::model::normal_font_name(wb.styles());
    let stylesheet = wb.styles().clone();
    let (sheets, warnings) = shim::load_sheets_tolerant(&mut wb);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    let ls = sheets.into_iter().next().expect("one sheet");
    SheetModel::from_resolved(
        &ls.sheet,
        &normal,
        fc,
        ls.col_widths_degraded,
        &stylesheet,
        &super::model::ColorCtx::default(),
    )
}

// ─── §18.3.1.13 width formula — the spec worked vector ────────────────────────

#[test]
fn width_formula_spec_vector_8_chars_61px() {
    // §18.3.1.13 worked example: Calibri 11pt, MDW=7px, 8 chars.
    let mdw = 7.0;
    // chars → file width: Truncate([8*7+5]/7*256)/256 = 8.7109375.
    let file_w = chars_to_file_width(8.0, mdw);
    assert_eq!(file_w, 8.7109375, "file width must equal the spec example");
    // file width → pixels: Truncate(((256*8.7109375 + Truncate(128/7))/256)*7) = 61.
    let px = file_width_to_px(file_w, mdw);
    assert_eq!(px, 61.0, "pixel width must equal the spec example");
    // Composed round-trip.
    assert_eq!(grid::chars_to_px(8.0, mdw), 61.0);
}

// ─── MDW measurement from the Calibri substitute ─────────────────────────────

#[test]
fn mdw_measured_from_loaded_font() {
    let fc = calibri_font_cache();
    // The measurement shapes digits '0'..'9' and rounds the max advance UP to a
    // whole pixel (§18.3.1.13). Arimo @11pt measures to ARIMO_MDW; the spec's
    // Calibri MDW=7 vector is asserted in width_formula_spec_vector_8_chars_61px.
    let mdw = measure_mdw("Calibri", &fc);
    assert_eq!(mdw, ARIMO_MDW, "measured MDW of the loaded normal font");
}

#[test]
fn mdw_falls_back_to_7_when_font_absent() {
    let fc = FontCache::new(); // nothing loaded
    assert_eq!(measure_mdw("Calibri", &fc), super::model::FALLBACK_MDW);
}

// ─── A1 reference parse + dimensions used-range ──────────────────────────────

#[test]
fn dimensions_used_range() {
    let fc = calibri_font_cache();
    // B2:C3 populated → used range (2,2,3,3).
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="B2:C3"/>
  <sheetData>
    <row r="2"><c r="B2" t="n"><v>1</v></c><c r="C2" t="n"><v>2</v></c></row>
    <row r="3"><c r="B3" t="n"><v>3</v></c><c r="C3" t="n"><v>4</v></c></row>
  </sheetData>
</worksheet>"#;
    let model = load_model(sheet, &[], &fc);
    assert_eq!(model.dimensions, Some((2, 2, 3, 3)));
    assert_eq!(model.rows.len(), 2);
    assert_eq!(model.rows[0].cells.len(), 2);
    assert_eq!(model.rows[0].cells[0].col, 2); // B = column 2
}

// ─── shared-string + inline-string resolution + general alignment ────────────

#[test]
fn shared_and_inline_strings_and_alignment() {
    let fc = calibri_font_cache();
    // A1: shared string idx 0 ("Hello"); B1: inline string ("Inline");
    // C1: number 42; D1: boolean TRUE.
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:D1"/>
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1" t="inlineStr"><is><t>Inline</t></is></c>
      <c r="C1" t="n"><v>42</v></c>
      <c r="D1" t="b"><v>1</v></c>
    </row>
  </sheetData>
</worksheet>"#;
    let model = load_model(sheet, &["Hello"], &fc);
    let cells = &model.rows[0].cells;
    assert_eq!(cells.len(), 4);
    assert_eq!(cells[0].text, "Hello");
    assert_eq!(cells[0].align, HAlign::Left); // text → left
    assert_eq!(cells[1].text, "Inline"); // inline strings: read Cell.is directly
    assert_eq!(cells[1].align, HAlign::Left);
    assert_eq!(cells[2].text, "42");
    assert_eq!(cells[2].align, HAlign::Right); // number → right
    assert_eq!(cells[3].text, "TRUE");
    assert_eq!(cells[3].align, HAlign::Center); // boolean → center
}

#[test]
fn inline_string_rich_runs_concatenate() {
    let fc = calibri_font_cache();
    // Inline string built from two rich runs (<r><t>…) → concatenated text.
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A1"/>
  <sheetData>
    <row r="1">
      <c r="A1" t="inlineStr"><is><r><t>Foo</t></r><r><t>Bar</t></r></is></c>
    </row>
  </sheetData>
</worksheet>"#;
    let model = load_model(sheet, &[], &fc);
    assert_eq!(model.rows[0].cells[0].text, "FooBar");
}

// ─── default gridlines (§18.3.1.88) ──────────────────────────────────────────

#[test]
fn gridlines_default_true_and_suppressed() {
    let fc = calibri_font_cache();
    let base = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A1"/>
  SHEETVIEW
  <sheetData><row r="1"><c r="A1" t="n"><v>1</v></c></row></sheetData>
</worksheet>"#;
    // No sheetView → default true.
    let m1 = load_model(&base.replace("SHEETVIEW", ""), &[], &fc);
    assert!(m1.show_grid_lines);
    // Explicit showGridLines="0" → false.
    let m2 = load_model(
        &base.replace(
            "SHEETVIEW",
            r#"<sheetViews><sheetView workbookViewId="0" showGridLines="0"/></sheetViews>"#,
        ),
        &[],
        &fc,
    );
    assert!(!m2.show_grid_lines);
}

// ─── row height (§18.3.1.73) + sheetFormatPr defaults (§18.3.1.81) ───────────

#[test]
fn row_height_points_to_px_and_defaults() {
    let fc = calibri_font_cache();
    // sheetFormatPr defaultRowHeight=15pt; row 2 overrides ht=30pt.
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A2"/>
  <sheetFormatPr defaultRowHeight="15" baseColWidth="8"/>
  <sheetData>
    <row r="1"><c r="A1" t="n"><v>1</v></c></row>
    <row r="2" ht="30" customHeight="1"><c r="A2" t="n"><v>2</v></c></row>
  </sheetData>
</worksheet>"#;
    let model = load_model(sheet, &[], &fc);
    assert_eq!(model.default_row_height_pt, 15.0);
    assert_eq!(model.rows[1].height_pt, Some(30.0));

    let pages = grid::paginate(&model, PageBox::default());
    assert_eq!(pages.len(), 1);
    // pt→px: 15pt = 20px, 30pt = 40px (×4/3). Row rails: 0, 20, 60.
    let rails = &pages[0].row_rails;
    assert_eq!(rails, &vec![0.0, 20.0, 60.0]);
}

// ─── explicit col width → pixel geometry (§18.3.1.13) ────────────────────────

#[test]
fn explicit_col_width_geometry() {
    let fc = calibri_font_cache();
    // Column A width=8.7109375 chars → 61px (the spec vector applied end-to-end).
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A1"/>
  <cols><col min="1" max="1" width="8.7109375" customWidth="1"/></cols>
  <sheetData><row r="1"><c r="A1" t="n"><v>1</v></c></row></sheetData>
</worksheet>"#;
    let model = load_model(sheet, &[], &fc);
    assert_eq!(model.mdw, ARIMO_MDW);
    let pages = grid::paginate(&model, PageBox::default());
    // col_rails: 0, then chars_to_px(8.7109375, measured-mdw) px. Geometry uses
    // the MEASURED MDW of the loaded font, not the spec's Calibri-7.
    let w = grid::chars_to_px(8.7109375, model.mdw);
    assert_eq!(pages[0].col_rails, vec![0.0, w]);
    assert_eq!(pages[0].cells[0].w, w);
}

// ─── sparse iteration: a gap row contributes an empty rail, not a collapse ───

#[test]
fn sparse_rows_keep_used_range_rails() {
    let fc = calibri_font_cache();
    // Rows 1 and 3 populated; row 2 absent. Used range 1..=3 → 3 row rails of
    // default height each (a missing row is empty, not zero-height).
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A3"/>
  <sheetFormatPr defaultRowHeight="15"/>
  <sheetData>
    <row r="1"><c r="A1" t="n"><v>1</v></c></row>
    <row r="3"><c r="A3" t="n"><v>3</v></c></row>
  </sheetData>
</worksheet>"#;
    let model = load_model(sheet, &[], &fc);
    let pages = grid::paginate(&model, PageBox::default());
    // 3 rows × 20px → rails 0,20,40,60. The middle (empty) row still occupies a
    // rail so the grid stays aligned.
    assert_eq!(pages[0].row_rails, vec![0.0, 20.0, 40.0, 60.0]);
    // Only the two populated cells produce ink.
    assert_eq!(pages[0].cells.len(), 2);
}

// ─── pagination: a tall sheet splits across pages ────────────────────────────

#[test]
fn pagination_splits_by_page_height() {
    let fc = calibri_font_cache();
    // 100 rows × 20px = 2000px content; default page height 1056px → 2 pages.
    let mut rows = String::new();
    for r in 1..=100 {
        rows.push_str(&format!("<row r=\"{r}\"><c r=\"A{r}\" t=\"n\"><v>{r}</v></c></row>"));
    }
    let sheet = format!(
        "<?xml version=\"1.0\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"A1:A100\"/><sheetFormatPr defaultRowHeight=\"15\"/><sheetData>{rows}</sheetData></worksheet>"
    );
    let model = load_model(&sheet, &[], &fc);
    let pages = grid::paginate(&model, PageBox::default());
    // 1056 / 20 = 52.8 → 52 rows per page → ceil(100/52) = 2 pages.
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].row_rails.len(), 52 + 1);
    assert_eq!(pages[1].row_rails.len(), (100 - 52) + 1);
}

// ─── geometry-hash oracle: deterministic, pinned baseline ────────────────────

/// The oracle sheet: a mixed grid exercising shared/inline strings, numbers,
/// booleans, explicit + default col widths, and an explicit row height.
/// (Extracted to a const so the Grid-v2 index equivalence tests drive the
/// SAME bytes; content unchanged — the pinned hashes are untouched.)
const ORACLE_SHEET_XML: &str = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:C3"/>
  <sheetFormatPr defaultRowHeight="15" baseColWidth="8"/>
  <cols>
    <col min="1" max="1" width="8.7109375" customWidth="1"/>
    <col min="2" max="2" width="12" customWidth="1"/>
  </cols>
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1" t="inlineStr"><is><t>Inline</t></is></c>
      <c r="C1" t="n"><v>45292</v></c>
    </row>
    <row r="2" ht="24" customHeight="1">
      <c r="A2" t="n"><v>3.14</v></c>
      <c r="B2" t="b"><v>1</v></c>
    </row>
    <row r="3">
      <c r="C3" t="s"><v>1</v></c>
    </row>
  </sheetData>
</worksheet>"#;

fn oracle_model() -> SheetModel {
    let fc = calibri_font_cache();
    load_model(ORACLE_SHEET_XML, &["Header", "Footer"], &fc)
}

#[test]
fn oracle_geometry_hash_pinned() {
    let model = oracle_model();
    let pages = grid::paginate(&model, PageBox::default());
    assert_eq!(pages.len(), 1, "oracle fits one page");
    let h = grid::geometry_hash(&pages);
    if std::env::var("SML_DUMP").is_ok() {
        eprintln!("oracle pages={} geometry_hash=0x{h:016x}", pages.len());
        for (i, p) in pages.iter().enumerate() {
            eprintln!(
                "  page {i}: cols={:?} rows={:?} cells={}",
                p.col_rails, p.row_rails, p.cells.len()
            );
        }
    }
    // PINNED baseline (confirmed identical across two runs, 2026-06-06).
    assert_eq!(h, ORACLE_GEOMETRY_HASH, "oracle geometry drift (0x{h:016x})");
}

#[test]
fn oracle_geometry_hash_deterministic() {
    // Same model, two independent paginations → identical hash.
    let a = grid::geometry_hash(&grid::paginate(&oracle_model(), PageBox::default()));
    let b = grid::geometry_hash(&grid::paginate(&oracle_model(), PageBox::default()));
    assert_eq!(a, b);
}

// ─── header gutter label fields + cellAt origin shift ────────────────────────

#[test]
fn gutter_labels_first_col_and_row_numbers_on_sparse_sheet() {
    // A sparse sheet whose used range starts at C2 (not A1): the page's
    // `first_col` must be the used-range min_col (3 = "C"), and `row_numbers`
    // must list every row band on the page in order (2,3,4 here), so the gutter
    // painter labels columns C,D,E and rows 2,3,4 — NOT A,B,C / 1,2,3.
    let fc = calibri_font_cache();
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="C2:E4"/>
  <sheetData>
    <row r="2"><c r="C2" t="n"><v>1</v></c><c r="E2" t="n"><v>2</v></c></row>
    <row r="3"><c r="D3" t="n"><v>3</v></c></row>
    <row r="4"><c r="E4" t="n"><v>4</v></c></row>
  </sheetData>
</worksheet>"#;
    let model = load_model(sheet, &[], &fc);
    let pages = grid::paginate(&model, PageBox::default());
    assert_eq!(pages.len(), 1);
    let p = &pages[0];

    // first_col = min_col = column C = 3.
    assert_eq!(p.first_col, 3, "first_col is the used-range start column (C)");
    assert_eq!(grid::col_letters(p.first_col), "C");
    // Three rail spans → three column letters C, D, E.
    assert_eq!(p.col_rails.len(), 4, "3 columns + right edge");
    let letters: Vec<String> = (0..p.col_rails.len() - 1)
        .map(|k| grid::col_letters(p.first_col + k as u32))
        .collect();
    assert_eq!(letters, vec!["C", "D", "E"]);

    // row_numbers is one-per-band, parallel to the row_rails gaps.
    assert_eq!(p.row_numbers, vec![2, 3, 4]);
    assert_eq!(
        p.row_numbers.len(),
        p.row_rails.len() - 1,
        "one row number per row band"
    );
}

#[test]
fn gutter_dims_widen_with_row_digit_count() {
    // The left (row-number) gutter grows with the digit count of the largest
    // row number on the page; the top (column-letter) gutter is fixed at 20px.
    let mk = |max_row: u32| grid::SheetPage {
        sheet_name: "S".into(),
        col_rails: vec![0.0, 10.0],
        row_rails: vec![0.0, 20.0],
        cells: Vec::new(),
        show_grid_lines: true,
        content_w: 10.0,
        content_h: 20.0,
        merges: Vec::new(),
        first_col: 1,
        row_numbers: vec![max_row],
        images: Vec::new(),
        charts: Vec::new(),
    };
    // Minimal gutter (col band 13px): 1 digit → floor of 16px (4 + 1*6.5 = 10.5 < 16).
    assert_eq!(grid::gutter_dims(&mk(1)), (16.0, 13.0));
    // 3 digits → 4 + 3*6.5 = 23.5px.
    assert_eq!(grid::gutter_dims(&mk(100)), (23.5, 13.0));
    // 7 digits (1_000_000) → 4 + 7*6.5 = 49.5px (so it never clips).
    assert_eq!(grid::gutter_dims(&mk(1_000_000)), (49.5, 13.0));
}

#[test]
fn cell_at_round_trips_through_gutter_shifted_grid() {
    // A click in the gutter-shifted grid must resolve the right A1 ref. The
    // painter shifts the grid origin right+down by `gutter_dims`; cellAt (in
    // mod.rs) subtracts the SAME shift, so a CONTENT-space coordinate (= page
    // coord minus margins, what lib.rs passes) lands on the correct cell.
    let fc = calibri_font_cache();
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B2"/>
  <sheetData>
    <row r="1"><c r="A1" t="n"><v>11</v></c><c r="B1" t="n"><v>12</v></c></row>
    <row r="2"><c r="A2" t="n"><v>21</v></c><c r="B2" t="n"><v>22</v></c></row>
  </sheetData>
</worksheet>"#;
    let model = load_model(sheet, &[], &fc);
    let pages = grid::paginate(&model, PageBox::default());
    let (row_gutter_w, col_gutter_h) = grid::gutter_dims(&pages[0]);

    // Center of B2 in page-local (grid) coordinates.
    let b2_cell = pages[0]
        .cells
        .iter()
        .find(|c| c.ref_a1 == "B2")
        .expect("B2 cell");
    let local_cx = b2_cell.x + b2_cell.w / 2.0;
    let local_cy = b2_cell.y + b2_cell.h / 2.0;
    // The content-space coordinate cellAt receives = grid-local + gutter shift.
    let content_x = local_cx + row_gutter_w;
    let content_y = local_cy + col_gutter_h;

    let cell = std::cell::RefCell::new(pages);
    let hit = super::SmlRenderer::cell_at(&cell, 0, content_x, content_y)
        .expect("a gutter-shifted click resolves a cell");
    assert_eq!(hit.0, "B2", "the shifted click resolves to B2");

    // A click INSIDE the gutter band (content coord below both gutter widths)
    // resolves nothing — no cell box has a negative page-local origin.
    assert!(
        super::SmlRenderer::cell_at(&cell, 0, 2.0, 2.0).is_none(),
        "a click inside the gutter bands hits no cell"
    );
}

// Pinned after confirming determinism across two independent runs.
// Re-baseline ONLY after confirming a geometry change is intentional (set
// SML_DUMP=1 to print the full page geometry for diffing).
// The hash folds, beyond rails and cell boxes:
//  - each cell's resolved CellStyle (numFmt/font/fill/border/h-align); this
//    oracle's cells all resolve to the default xf (style=None);
//  - each cell's `is_merged_anchor` flag and each page's merged-region boxes
//    (§18.3.1.55); this oracle has NO merges, so no covered-cell suppression
//    or anchor expansion is exercised here;
//  - each page's gutter label fields: `first_col` (1 here) and `row_numbers`
//    ([1,2,3] here) for the A/B/C + 1/2/3 header gutters (§18 heading band).
// The gutter ORIGIN SHIFT lives at paint time (render.rs) and in cellAt, not
// in the paginated page geometry, so it is not part of the hash.
// Each of these folds was introduced with a re-pin during which the rails and
// cell boxes stayed byte-identical: only the hash *format* changed, never the
// geometry. A re-pin that moves a rail or a cell box is a geometry change and
// needs its own justification.
//
const ORACLE_GEOMETRY_HASH: u64 = 0x5dba3a8136ccce88;

// ─── `<col>` shim: malformed <col> tolerance ────────────────────────────────

#[test]
fn malformed_col_missing_min_max_is_tolerated() {
    let fc = calibri_font_cache();
    // <col width="20"/> lacks @min/@max → strict sml-styling parse would fail
    // the whole sheet. The shim's render-only pre-repair injects
    // them so the sheet still renders.
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A1"/>
  <cols><col width="20"/></cols>
  <sheetData><row r="1"><c r="A1" t="n"><v>7</v></c></row></sheetData>
</worksheet>"#;
    let bytes = xlsx_with(sheet, &sst(&[]));
    let mut wb = Workbook::from_reader(Cursor::new(bytes)).expect("workbook open");

    // Sanity: the strict per-sheet path DOES fail on the malformed <col>.
    assert!(wb.resolved_sheet(0).is_err(), "strict parse must reject missing @min/@max");

    // The shim contains it: the sheet loads, flagged degraded, with a warning.
    let (sheets, warnings) = shim::load_sheets_tolerant(&mut wb);
    assert_eq!(sheets.len(), 1, "sheet must still render via the shim");
    assert!(sheets[0].col_widths_degraded);
    assert_eq!(warnings.len(), 1);
    assert!(matches!(
        warnings[0],
        shim::SheetWarning::ColWidthsRepaired { sheet: 0 }
    ));

    let normal = super::model::normal_font_name(wb.styles());
    let stylesheet = wb.styles().clone();
    let model = SheetModel::from_resolved(&sheets[0].sheet, &normal, &fc, sheets[0].col_widths_degraded, &stylesheet, &super::model::ColorCtx::default());
    assert!(model.col_widths_degraded);
    // The cell still renders.
    let pages = grid::paginate(&model, PageBox::default());
    assert_eq!(pages[0].cells[0].text, "7");
}

#[test]
fn repair_col_attrs_injects_only_missing() {
    // min present, max missing → only @max injected, byte-preserving otherwise.
    let xml = br#"<worksheet><cols><col min="3" width="20"/></cols></worksheet>"#;
    let out = shim::repair_col_attrs(xml);
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains(r#"min="3""#), "existing @min preserved");
    assert!(s.contains(r#"max="1""#), "missing @max injected (running index)");
    assert!(s.contains(r#"width="20""#), "other attrs preserved");

    // Both present → untouched (idempotent).
    let xml2 = br#"<col min="1" max="2" width="5"/>"#;
    assert_eq!(shim::repair_col_attrs(xml2), xml2.to_vec());

    // A non-col tag containing "min" is not mangled.
    let xml3 = br#"<dimension ref="A1:B2"/>"#;
    assert_eq!(shim::repair_col_attrs(xml3), xml3.to_vec());
}

// ─── SmlRenderer wiring (from_xlsx → paginate_all) ──────────────────────────
//
// These exercise the renderer the Viewer dispatches into. The wasm-only paths
// (loadXlsx / runLayout / renderPage / the JS callback) are not natively
// testable, but the renderer's pure core — workbook parse, tolerant sheet load,
// MDW measurement, model build, and pagination — is, and it is the load-bearing
// logic of the dispatch. Geometry baselines are pinned after confirming
// determinism across two runs (as the oracle tests do).

/// Build a two-sheet xlsx so the renderer's per-sheet concatenation is covered.
fn build_xlsx_two_sheets(sheet1: &str, sheet2: &str, strings: &str) -> Vec<u8> {
    const CT2: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>"#;
    const WB2: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="One" sheetId="1" r:id="rId1"/>
    <sheet name="Two" sheetId="2" r:id="rId4"/>
  </sheets>
</workbook>"#;
    const WB2_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
    build_xlsx(&[
        ("[Content_Types].xml", CT2),
        ("_rels/.rels", ROOT_RELS),
        ("xl/workbook.xml", WB2),
        ("xl/_rels/workbook.xml.rels", WB2_RELS),
        ("xl/styles.xml", STYLES),
        ("xl/sharedStrings.xml", strings),
        ("xl/worksheets/sheet1.xml", sheet1),
        ("xl/worksheets/sheet2.xml", sheet2),
    ])
}

#[test]
fn renderer_from_xlsx_renders_absolute_rels_targets() {
    // End-to-end pin for the fork's resolve_part_path fix (RULE 4 retired): a
    // package whose rels use ABSOLUTE OPC targets (Target="/xl/workbook.xml",
    // the .NET-exporter shape — legal per OPC Part 2 §9.3.1.1) parses and
    // renders through the STRAIGHT path — no repair involved.
    const ABS_ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="/xl/workbook.xml"/>
</Relationships>"#;
    const ABS_WORKBOOK_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="/xl/worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="/xl/sharedStrings.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="/xl/styles.xml"/>
</Relationships>"#;
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1"/>
  <sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData>
</worksheet>"#;
    let strings = sst(&["Recovered"]);
    let bytes = build_xlsx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ABS_ROOT_RELS),
        ("xl/workbook.xml", WORKBOOK),
        ("xl/_rels/workbook.xml.rels", ABS_WORKBOOK_RELS),
        ("xl/styles.xml", STYLES),
        ("xl/sharedStrings.xml", &strings),
        ("xl/worksheets/sheet1.xml", sheet),
    ]);

    // Sheet present, cell text resolved (shared strings reached through the
    // parser-resolved nested rels too).
    let fc = calibri_font_cache();
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx parses absolute targets");
    assert!(r.warnings().is_empty(), "no sheet-level warning: {:?}", r.warnings());
    let pages = r.paginate_all(&fc);
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].sheet_name, "Sheet1");
    assert!(pages[0].cells.iter().any(|c| c.text == "Recovered"));
}

#[test]
fn renderer_from_xlsx_renders_absolute_nested_rels_targets() {
    // End-to-end pin for the fork's resolve_part_path fix (RULE 4 retired),
    // a shape some generators write: RELATIVE root _rels/.rels but ABSOLUTE nested
    // sheet targets (Target="/xl/worksheets/sheet1.xml", legal per OPC Part 2
    // §9.3.1.1). The worksheet part resolves LAZILY at sheet_xml(i) — the case
    // that once failed there and was swallowed into SheetDropped → blank grid.
    // Inline-string cells + a frozen pane are included and asserted
    // NON-fatal (neither is implicated).
    const REL_ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
    const ABS_WORKBOOK_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="/xl/worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="/xl/sharedStrings.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="/xl/styles.xml"/>
</Relationships>"#;
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A2"/>
  <sheetViews>
    <sheetView tabSelected="1" workbookViewId="0">
      <pane ySplit="1" topLeftCell="A2" activePane="bottomLeft" state="frozen"/>
    </sheetView>
  </sheetViews>
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>Dependency</t></is></c></row>
    <row r="2"><c r="A2" t="inlineStr"><is><t>openpyxl</t></is></c></row>
  </sheetData>
</worksheet>"#;
    // The nested rels still reference a sharedStrings part (rId2); provide
    // one so the relativized SST target also resolves (multi-target rewrite).
    let strings = sst(&["unused"]);
    let bytes = build_xlsx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", REL_ROOT_RELS),
        ("xl/workbook.xml", WORKBOOK),
        ("xl/_rels/workbook.xml.rels", ABS_WORKBOOK_RELS),
        ("xl/styles.xml", STYLES),
        ("xl/sharedStrings.xml", &strings),
        ("xl/worksheets/sheet1.xml", sheet),
    ]);

    // The sheet SURVIVES the straight path with warnings() EMPTY (no
    // SheetDropped), inline text reachable through the parser-resolved rels.
    let fc = calibri_font_cache();
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx parses nested absolute rels");
    assert!(r.warnings().is_empty(), "no sheet-drop: {:?}", r.warnings());
    let pages = r.paginate_all(&fc);
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].sheet_name, "Sheet1");
    assert!(pages[0].cells.iter().any(|c| c.text == "Dependency"));
    assert!(pages[0].cells.iter().any(|c| c.text == "openpyxl"));
}

#[test]
fn renderer_from_xlsx_paginates_single_sheet() {
    let fc = calibri_font_cache();
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B2"/>
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="n"><v>42</v></c></row>
    <row r="2"><c r="A2" t="n"><v>1</v></c></row>
  </sheetData>
</worksheet>"#;
    let bytes = xlsx_with(sheet, &sst(&["Hello"]));
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
    assert!(r.warnings().is_empty(), "clean workbook → no warnings");
    assert_eq!(r.normal_font(), "Calibri");

    let pages = r.paginate_all(&fc);
    assert_eq!(pages.len(), 1, "two short rows fit one page");
    let p = &pages[0];
    assert_eq!(p.sheet_name, "Sheet1");
    // 3 populated cells: A1 shared-string "Hello", B1 number 42, A2 number 1.
    let texts: Vec<&str> = p.cells.iter().map(|c| c.text.as_str()).collect();
    assert!(texts.contains(&"Hello"));
    assert!(texts.contains(&"42"));
    assert!(texts.contains(&"1"));
    // General alignment: the shared string is left, the numbers right (§18).
    let hello = p.cells.iter().find(|c| c.text == "Hello").unwrap();
    let n42 = p.cells.iter().find(|c| c.text == "42").unwrap();
    assert_eq!(hello.align, HAlign::Left);
    assert_eq!(n42.align, HAlign::Right);
}

#[test]
fn renderer_concatenates_sheets_in_index_order() {
    let fc = calibri_font_cache();
    let s1 = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A1"/>
  <sheetData><row r="1"><c r="A1" t="n"><v>11</v></c></row></sheetData>
</worksheet>"#;
    let s2 = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A1"/>
  <sheetData><row r="1"><c r="A1" t="n"><v>22</v></c></row></sheetData>
</worksheet>"#;
    let bytes = build_xlsx_two_sheets(s1, s2, &sst(&[]));
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
    let pages = r.paginate_all(&fc);
    assert_eq!(pages.len(), 2, "one page per non-empty sheet");
    assert_eq!(pages[0].sheet_name, "One");
    assert_eq!(pages[1].sheet_name, "Two");
    assert_eq!(pages[0].cells[0].text, "11");
    assert_eq!(pages[1].cells[0].text, "22");
}

#[test]
fn sheet_spans_run_length_groups_contiguous_sheets() {
    // The sheetInfo() facade run-length-groups the flat page vec on
    // sheet_name. Pages stream sheets back-to-back (Summary, then Data's two
    // pages, then Notes), so consecutive-run grouping must yield exactly three
    // spans with correct firstPage / pageCount.
    let mk = |name: &str| grid::SheetPage {
        sheet_name: name.to_string(),
        col_rails: vec![0.0],
        row_rails: vec![0.0],
        cells: Vec::new(),
        show_grid_lines: true,
        content_w: 0.0,
        content_h: 0.0,
        merges: Vec::new(),
        first_col: 1,
        row_numbers: Vec::new(),
        images: Vec::new(),
        charts: Vec::new(),
    };
    let pages = vec![mk("Summary"), mk("Data"), mk("Data"), mk("Notes")];
    let spans = grid::sheet_spans(&pages);
    assert_eq!(spans.len(), 3, "three sheets → three spans");
    assert_eq!(spans[0], grid::SheetSpan { name: "Summary".into(), first_page: 0, page_count: 1 });
    assert_eq!(spans[1], grid::SheetSpan { name: "Data".into(), first_page: 1, page_count: 2 });
    assert_eq!(spans[2], grid::SheetSpan { name: "Notes".into(), first_page: 3, page_count: 1 });
    assert!(grid::sheet_spans(&[]).is_empty(), "no pages → no spans");
}

#[test]
fn renderer_recovers_malformed_col_with_warning() {
    let fc = calibri_font_cache();
    // <col width="20"/> lacks @min/@max → strict parse fails the sheet; the
    // renderer's shim path must recover it AND surface the degraded warning.
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A1"/>
  <cols><col width="20"/></cols>
  <sheetData><row r="1"><c r="A1" t="n"><v>7</v></c></row></sheetData>
</worksheet>"#;
    let bytes = xlsx_with(sheet, &sst(&[]));
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx must not fail the whole workbook");
    assert_eq!(r.warnings().len(), 1, "the repair records one warning");
    assert!(matches!(
        r.warnings()[0],
        shim::SheetWarning::ColWidthsRepaired { sheet: 0 }
    ));
    let pages = r.paginate_all(&fc);
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].cells[0].text, "7", "cell still renders after repair");
}

#[test]
fn renderer_geometry_hash_pinned() {
    // The renderer's end-to-end geometry (parse → tolerant load → model →
    // paginate) is pinned so the dispatch wiring can't silently drift it.
    let fc = calibri_font_cache();
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:C3"/>
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="n"><v>100</v></c><c r="C1" t="b"><v>1</v></c></row>
    <row r="2"><c r="A2" t="n"><v>2</v></c><c r="C2" t="s"><v>1</v></c></row>
    <row r="3"><c r="B3" t="n"><v>3</v></c></row>
  </sheetData>
</worksheet>"#;
    let bytes = xlsx_with(sheet, &sst(&["Name", "End"]));
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
    let pages = r.paginate_all(&fc);
    let h = grid::geometry_hash(&pages);
    if std::env::var("SML_DUMP").is_ok() {
        eprintln!("renderer pages={} geometry_hash=0x{h:016x}", pages.len());
    }
    // PINNED baseline (confirmed identical across two runs).
    assert_eq!(h, RENDERER_GEOMETRY_HASH, "renderer geometry drift (0x{h:016x})");
}

// Pinned after confirming determinism across two independent runs.
// The hash folds, beyond rails and cell boxes:
//  - per-cell CellStyle; this fixture's cells all resolve to the default xf
//    (style=None);
//  - is_merged_anchor + per-page merged-region boxes (§18.3.1.55); this
//    fixture has NO merges;
//  - the gutter label fields `first_col` (1) and `row_numbers` ([1,2,3]) for
//    the A/B/C + 1/2/3 header gutters.
// The gutter origin shift lives in render.rs + cellAt, not in the paginated
// page geometry. Each fold was introduced with a re-pin during which the
// rails and cell boxes stayed byte-identical (only the hash format changed).
//
const RENDERER_GEOMETRY_HASH: u64 = 0xcf4880dcd43c59e9;

// ─── numfmt interpreter (§18.8.30) ──────────────────────────────────────────
//
// Heavy table-driven tests. These cover the format-CODE interpreter in
// isolation (its wiring into the model is covered by the per-cell style tests
// below and leaves the geometry pins above untouched). Vectors mirror the
// acceptance fixtures' "Should display as" column plus the date-serial / 1900
// leap-year quirk REUSED from the fork helper.

#[cfg(test)]
mod numfmt_tests {
    use crate::sml::numfmt::{format_value, resolve_format_code, RawValue};
    use ooxml_sml::workbook::excel_date_to_ymd;

    /// Format a numeric value through a format code.
    fn fnum(n: f64, code: &str) -> String {
        format_value(&RawValue::Number(n), code)
    }

    // ── numFmtId → format-code resolution (custom-first, then built-in) ──────

    #[test]
    fn resolve_builtin_and_custom_codes() {
        // Built-in table (§18.8.30): no custom entries → built-in lookup.
        assert_eq!(resolve_format_code(0, []).as_deref(), Some("General"));
        assert_eq!(resolve_format_code(2, []).as_deref(), Some("0.00"));
        assert_eq!(resolve_format_code(3, []).as_deref(), Some("#,##0"));
        assert_eq!(resolve_format_code(10, []).as_deref(), Some("0.00%"));
        assert_eq!(resolve_format_code(49, []).as_deref(), Some("@"));
        // Unknown id with no custom entry → None (caller falls back to General).
        assert_eq!(resolve_format_code(164, []), None);
        // Custom id (§18.8.31, ≥164) resolves custom-first.
        let custom = [(164u32, "\"$\"#,##0.00")];
        assert_eq!(
            resolve_format_code(164, custom).as_deref(),
            Some("\"$\"#,##0.00")
        );
        // A custom entry shadowing a built-in id wins (custom-first).
        let shadow = [(2u32, "0.000")];
        assert_eq!(resolve_format_code(2, shadow).as_deref(), Some("0.000"));
        // Locale-sensitive date built-ins resolve to the en-US shape Excel
        // displays, NOT the fork's spec-literal placeholder (which would
        // render 14 as "01-01-24" instead of "1/1/2024").
        assert_eq!(resolve_format_code(14, []).as_deref(), Some("m/d/yyyy"));
        assert_eq!(resolve_format_code(22, []).as_deref(), Some("m/d/yyyy h:mm"));
    }

    #[test]
    fn builtin_14_is_us_short_date() {
        // The number-formats fixture's "Should display as" expectation.
        let code = resolve_format_code(14, []).unwrap();
        assert_eq!(fnum(45292.0, &code), "1/1/2024");
    }

    // ── date serials via the fork helper (1900 system + leap quirk) ──────────

    #[test]
    fn date_serial_45292_is_2024_01_01() {
        // The fixture vector. Routed through excel_date_to_ymd (REUSED).
        assert_eq!(excel_date_to_ymd(45292.0), Some((2024, 1, 1)));
        // mm-dd-yy (built-in 14) and a yyyy-mm-dd custom code.
        assert_eq!(fnum(45292.0, "mm-dd-yy"), "01-01-24");
        assert_eq!(fnum(45292.0, "yyyy-mm-dd"), "2024-01-01");
        // d-mmm-yy (built-in 15) and mmm-yy (17).
        assert_eq!(fnum(45292.0, "d-mmm-yy"), "1-Jan-24");
        assert_eq!(fnum(45292.0, "mmm-yy"), "Jan-24");
        // Long month / weekday tokens.
        assert_eq!(fnum(45292.0, "mmmm d, yyyy"), "January 1, 2024");
        assert_eq!(fnum(45292.0, "dddd"), "Monday"); // 2024-01-01 is a Monday
    }

    #[test]
    fn serial_60_is_1900_leap_quirk() {
        // The Lotus 1900-leap-year compatibility quirk: serial 60 = 1900-02-29
        // (a day that doesn't exist), serials > 60 shift back by one. Asserted
        // via the fork helper we REUSE, and through the formatter.
        assert_eq!(excel_date_to_ymd(60.0), Some((1900, 2, 29)));
        assert_eq!(excel_date_to_ymd(61.0), Some((1900, 3, 1)));
        assert_eq!(fnum(60.0, "yyyy-mm-dd"), "1900-02-29");
        assert_eq!(fnum(61.0, "yyyy-mm-dd"), "1900-03-01");
        assert_eq!(fnum(1.0, "yyyy-mm-dd"), "1900-01-01");
    }

    #[test]
    fn datetime_tokens() {
        // 45292.5 = 2024-01-01 12:00:00 (half a day).
        assert_eq!(fnum(45292.5, "yyyy-mm-dd hh:mm:ss"), "2024-01-01 12:00:00");
        // m disambiguation: between h and s → minutes.
        assert_eq!(fnum(45292.5, "h:mm"), "12:00");
        // AM/PM 12-hour clock (built-in 18 shape).
        assert_eq!(fnum(45292.5, "h:mm AM/PM"), "12:00 PM");
        // A quarter day = 06:00 → 6 AM.
        assert_eq!(fnum(45292.25, "h:mm AM/PM"), "6:00 AM");
    }

    #[test]
    fn time_only_serial_under_one_day() {
        // A serial < 1 is a pure time-of-day (the fork's date helper rejects it,
        // so the interpreter computes the clock from the day fraction). The
        // number-formats fixture B7 = 0.5625 of a day = 13:30:00 (built-in 21).
        assert_eq!(fnum(0.5625, "h:mm:ss"), "13:30:00");
        assert_eq!(fnum(0.5, "h:mm"), "12:00");
        assert_eq!(fnum(0.25, "h:mm AM/PM"), "6:00 AM");
        assert_eq!(fnum(0.75, "h:mm AM/PM"), "6:00 PM");
    }

    // ── percent (×100), built-in 9/10 ────────────────────────────────────────

    #[test]
    fn percent_scales_by_100() {
        // The fixture vector: 0.625 → 62.50% via "0.00%" (built-in 10).
        assert_eq!(fnum(0.625, "0.00%"), "62.50%");
        // Built-in 9 "0%": rounds to whole percent.
        assert_eq!(fnum(0.625, "0%"), "63%");
        assert_eq!(fnum(0.5, "0%"), "50%");
        assert_eq!(fnum(1.0, "0.00%"), "100.00%");
    }

    // ── thousands grouping, built-in 3 ───────────────────────────────────────

    #[test]
    fn thousands_grouping() {
        // The fixture vector: 1234567 → 1,234,567 via "#,##0" (built-in 3).
        assert_eq!(fnum(1234567.0, "#,##0"), "1,234,567");
        assert_eq!(fnum(1000.0, "#,##0"), "1,000");
        assert_eq!(fnum(999.0, "#,##0"), "999"); // no comma under 1000
        assert_eq!(fnum(1234567.0, "#,##0.00"), "1,234,567.00");
    }

    // ── decimals, built-in 2 ─────────────────────────────────────────────────

    #[test]
    fn decimal_places_and_rounding() {
        // The fixture vector: 3.14159 → 3.14 via "0.00" (built-in 2).
        assert_eq!(fnum(3.14159, "0.00"), "3.14");
        assert_eq!(fnum(3.14559, "0.00"), "3.15"); // rounds up
        assert_eq!(fnum(2.0, "0.00"), "2.00"); // zero-pads
        assert_eq!(fnum(0.0, "0.00"), "0.00");
        // Built-in 1 "0": integer, rounds.
        assert_eq!(fnum(3.7, "0"), "4");
        assert_eq!(fnum(3.2, "0"), "3");
    }

    // ── currency custom code (§18.8.31, id 164) ──────────────────────────────

    #[test]
    fn currency_quoted_literal() {
        // The fixture vector: 1299.99 → $1,299.99 via the custom 164 code
        // "$"#,##0.00 (the quoted-literal currency prefix + grouping + 2dp).
        assert_eq!(fnum(1299.99, "\"$\"#,##0.00"), "$1,299.99");
        assert_eq!(fnum(1000.0, "\"$\"#,##0.00"), "$1,000.00");
        // Backslash-escaped literal currency is equivalent.
        assert_eq!(fnum(5.5, "\\$0.00"), "$5.50");
    }

    // ── General fallback + text passthrough ──────────────────────────────────

    #[test]
    fn general_fallback() {
        assert_eq!(fnum(45292.0, "General"), "45292"); // raw serial, NOT a date
        assert_eq!(fnum(3.14, "General"), "3.14");
        assert_eq!(fnum(1000.0, ""), "1000"); // empty code → General
        assert_eq!(fnum(42.0, "general"), "42"); // case-insensitive
    }

    #[test]
    fn text_passthrough() {
        // `@` (built-in 49) and bare strings pass through verbatim; numeric
        // sections never apply to text.
        let t = RawValue::Text("hello");
        assert_eq!(format_value(&t, "@"), "hello");
        assert_eq!(format_value(&t, "General"), "hello");
        assert_eq!(format_value(&t, "0.00"), "hello"); // numeric code, text value
        // A text section with a literal wrapper.
        assert_eq!(format_value(&t, "0.00;0.00;0.00;\"<\"@\">\""), "<hello>");
    }

    // ── four-section split: negatives + zero ─────────────────────────────────

    #[test]
    fn negative_and_zero_sections() {
        // Two-section "pos;neg": negative reuses the neg section's literals.
        assert_eq!(fnum(-5.0, "0.00;(0.00)"), "(5.00)");
        assert_eq!(fnum(5.0, "0.00;(0.00)"), "5.00");
        // No negative section: synthesized leading '-' on the positive section.
        assert_eq!(fnum(-5.0, "0.00"), "-5.00");
        assert_eq!(fnum(-1234.5, "#,##0.00"), "-1,234.50");
        // Three-section "pos;neg;zero": explicit zero section.
        assert_eq!(fnum(0.0, "0.00;(0.00);\"zero\""), "zero");
    }

    #[test]
    fn section_split_is_quote_and_bracket_aware() {
        // A `;` inside a quoted literal must NOT split sections.
        // Single-section code whose literal contains a semicolon.
        assert_eq!(fnum(5.0, "\"a;b\"0"), "a;b5");
        // A `;` inside brackets is also protected.
        assert_eq!(fnum(5.0, "[$-409]0"), "5");
    }
}

// ─── per-cell style + numfmt wiring (§18.8.45 cellXfs resolution) ───────────
//
// Builds in-test xlsx packages with custom stylesheets and asserts the resolved
// CellStyle threaded onto each cell (font/fill/border/numFmt/alignment), the
// ARGB → #RRGGBB colour decoding, and numFmt applied to the displayed text.
// Mirrors the acceptance fixtures' style tables (styled.xlsx / number-formats).

#[cfg(test)]
mod style_tests {
    use super::*;
    use crate::sml::model::{BorderEdge, HAlign};

    /// Build an xlsx whose styles part is the caller-supplied `styles_xml`,
    /// returning the first sheet's model.
    fn load_styled(sheet_xml: &str, strings: &[&str], styles_xml: &str, fc: &FontCache) -> SheetModel {
        let bytes = build_xlsx(&[
            ("[Content_Types].xml", CONTENT_TYPES),
            ("_rels/.rels", ROOT_RELS),
            ("xl/workbook.xml", WORKBOOK),
            ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS),
            ("xl/styles.xml", styles_xml),
            ("xl/sharedStrings.xml", &sst(strings)),
            ("xl/worksheets/sheet1.xml", sheet_xml),
        ]);
        let mut wb = Workbook::from_reader(Cursor::new(bytes)).expect("workbook open");
        let normal = super::super::model::normal_font_name(wb.styles());
        let stylesheet = wb.styles().clone();
        let (sheets, _w) = shim::load_sheets_tolerant(&mut wb);
        let ls = sheets.into_iter().next().expect("one sheet");
        // ColorCtx::build (not ::default) so style tests exercise any
        // stylesheet <indexedColors> overlay too (theme slots stay empty —
        // the OPC theme self-parse is covered by the renderer-level tests).
        let colors = super::super::model::ColorCtx::build(Default::default(), &stylesheet);
        SheetModel::from_resolved(&ls.sheet, &normal, fc, ls.col_widths_degraded, &stylesheet, &colors)
    }

    /// The styled.xlsx acceptance stylesheet (blue header fill + white bold
    /// header font + zebra fill + boxed border + 16pt title).
    const STYLED_STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <fonts count="4">
    <font><sz val="11"/><name val="Calibri"/></font>
    <font><b/><sz val="12"/><color rgb="FFFFFFFF"/><name val="Calibri"/></font>
    <font><b/><sz val="16"/><color rgb="FF1F3864"/><name val="Calibri"/></font>
    <font><i/><sz val="10"/><color rgb="FF808080"/><name val="Calibri"/></font>
  </fonts>
  <fills count="4">
    <fill><patternFill patternType="none"/></fill>
    <fill><patternFill patternType="gray125"/></fill>
    <fill><patternFill patternType="solid"><fgColor rgb="FF4472C4"/></patternFill></fill>
    <fill><patternFill patternType="solid"><fgColor rgb="FFF2F2F2"/></patternFill></fill>
  </fills>
  <borders count="2">
    <border/>
    <border><left style="thin"/><right style="thin"/><top style="thin"/><bottom style="thin"/></border>
  </borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  <cellXfs count="6">
    <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>
    <xf numFmtId="0" fontId="1" fillId="2" borderId="1" xfId="0" applyAlignment="1"><alignment horizontal="center"/></xf>
    <xf numFmtId="0" fontId="0" fillId="3" borderId="1" xfId="0"/>
    <xf numFmtId="0" fontId="0" fillId="0" borderId="1" xfId="0"/>
    <xf numFmtId="0" fontId="2" fillId="0" borderId="0" xfId="0"/>
    <xf numFmtId="0" fontId="3" fillId="0" borderId="0" xfId="0"/>
  </cellXfs>
</styleSheet>"#;

    /// One styled cell per xf index 1..=5, so we can read the resolved CellStyle.
    fn styled_model() -> SheetModel {
        let fc = calibri_font_cache();
        // A1 default(0), B1 header(1), C1 zebra(2), D1 boxed(3), E1 title(4),
        // F1 footnote-italic(5).
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:F1"/>
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1" t="s" s="1"><v>1</v></c>
      <c r="C1" t="s" s="2"><v>2</v></c>
      <c r="D1" t="n" s="3"><v>5</v></c>
      <c r="E1" t="s" s="4"><v>3</v></c>
      <c r="F1" t="s" s="5"><v>4</v></c>
    </row>
  </sheetData>
</worksheet>"#;
        load_styled(sheet, &["A", "Header", "Zebra", "Title", "Foot"], STYLED_STYLES, &fc)
    }

    fn cell<'a>(m: &'a SheetModel, col: u32) -> &'a super::super::model::CellModel {
        m.rows[0].cells.iter().find(|c| c.col == col).expect("cell")
    }

    #[test]
    fn default_xf_resolves_to_no_style() {
        // style_index 0 (default xf, all ids 0, no alignment) → None (keeps the
        // default render path; no spurious geometry change for unstyled cells).
        let m = styled_model();
        assert_eq!(cell(&m, 1).style, None, "A1 default xf → no style");
    }

    #[test]
    fn header_xf_resolves_font_fill_border_alignment() {
        // xf 1: fontId=1 (bold 12pt white), fillId=2 (blue solid), borderId=1
        // (thin box), applyAlignment center.
        let m = styled_model();
        let st = cell(&m, 2).style.as_ref().expect("B1 styled");
        assert!(st.bold, "header is bold");
        assert!(!st.italic);
        assert_eq!(st.font_size_pt, Some(12.0));
        // White font (FFFFFFFF → #FFFFFF, alpha dropped).
        assert_eq!(st.font_color_hex.as_deref(), Some("#FFFFFF"));
        // Blue solid fill (FF4472C4 → #4472C4).
        assert_eq!(st.fill_hex.as_deref(), Some("#4472C4"));
        // Thin box on all four edges (left,right,top,bottom).
        for (i, edge) in st.borders.iter().enumerate() {
            let e = edge.as_ref().unwrap_or_else(|| panic!("edge {i} present"));
            assert_eq!(e.width_px, 1.0, "thin → 1px");
        }
        // Explicit center alignment overrides the value-kind general align.
        assert_eq!(st.h_align_override, Some(HAlign::Center));
    }

    #[test]
    fn zebra_xf_resolves_light_fill_no_font_override() {
        // xf 2: fontId=0 (default → no font override), fillId=3 (light gray),
        // borderId=1 (box), no alignment.
        let m = styled_model();
        let st = cell(&m, 3).style.as_ref().expect("C1 styled");
        assert!(!st.bold);
        assert_eq!(st.font_size_pt, None, "fontId 0 → no size override");
        assert_eq!(st.font_color_hex, None, "fontId 0 → no colour override");
        assert_eq!(st.fill_hex.as_deref(), Some("#F2F2F2"));
        assert_eq!(st.h_align_override, None, "no alignment → keep general");
    }

    /// §18.8.45 applyFont/applyFill/applyBorder over index 0. A cellXf that sets
    /// `fontId=0 fillId=0 borderId=0` but `applyFont="1" applyFill="1"
    /// applyBorder="1"` must APPLY the referenced (index-0) formatting — the
    /// "apply the default" case. Regression pin for the collapsed-boolean logic
    /// bug (`(id!=0 || apply) && id!=0` reduced to `id!=0`, so the apply flag was
    /// dead and index-0 formatting was silently dropped). Uses a synthetic
    /// stylesheet whose index-0 font/fill/border are non-default and visible, so
    /// the applied style is observably != CellStyle::default() (dodging the
    /// `style == default → None` early-out).
    #[test]
    fn apply_flags_force_index_zero_font_fill_border() {
        let fc = calibri_font_cache();
        const STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <fonts count="1">
    <font><b/><sz val="13"/><color rgb="FF112233"/><name val="Calibri"/></font>
  </fonts>
  <fills count="1">
    <fill><patternFill patternType="solid"><fgColor rgb="FFABCDEF"/></patternFill></fill>
  </fills>
  <borders count="1">
    <border><left style="thin"/><right style="thin"/><top style="thin"/><bottom style="thin"/></border>
  </borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  <cellXfs count="2">
    <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>
    <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0" applyFont="1" applyFill="1" applyBorder="1"/>
  </cellXfs>
</styleSheet>"#;
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A1"/>
  <sheetData>
    <row r="1"><c r="A1" t="n" s="1"><v>5</v></c></row>
  </sheetData>
</worksheet>"#;
        let m = load_styled(sheet, &[], STYLES, &fc);
        let st = cell(&m, 1)
            .style
            .as_ref()
            .expect("apply* flags must force index-0 formatting to apply");
        assert!(st.bold, "index-0 font (bold) applied via applyFont");
        assert_eq!(st.font_size_pt, Some(13.0));
        assert_eq!(st.font_color_hex.as_deref(), Some("#112233"));
        assert_eq!(st.fill_hex.as_deref(), Some("#ABCDEF"), "index-0 solid fill via applyFill");
        assert!(
            st.borders.iter().all(|b| b.is_some()),
            "index-0 four-edge border via applyBorder"
        );
    }

    #[test]
    fn boxed_xf_resolves_border_only() {
        // xf 3: only borderId=1 set; no fill, no font.
        let m = styled_model();
        let st = cell(&m, 4).style.as_ref().expect("D1 styled");
        assert_eq!(st.fill_hex, None);
        assert_eq!(st.font_color_hex, None);
        assert_eq!(
            st.borders,
            [
                Some(BorderEdge { width_px: 1.0, color: "#808080".into(), dash: None }),
                Some(BorderEdge { width_px: 1.0, color: "#808080".into(), dash: None }),
                Some(BorderEdge { width_px: 1.0, color: "#808080".into(), dash: None }),
                Some(BorderEdge { width_px: 1.0, color: "#808080".into(), dash: None }),
            ]
        );
    }

    #[test]
    fn title_xf_resolves_16pt_dark_blue() {
        // xf 4: fontId=2 (bold 16pt dark blue #1F3864), no fill/border.
        let m = styled_model();
        let st = cell(&m, 5).style.as_ref().expect("E1 styled");
        assert!(st.bold);
        assert_eq!(st.font_size_pt, Some(16.0));
        assert_eq!(st.font_color_hex.as_deref(), Some("#1F3864"));
        assert_eq!(st.fill_hex, None);
        assert!(st.borders.iter().all(|b| b.is_none()));
    }

    #[test]
    fn italic_footnote_xf() {
        // xf 5: fontId=3 (italic 10pt gray #808080).
        let m = styled_model();
        let st = cell(&m, 6).style.as_ref().expect("F1 styled");
        assert!(st.italic);
        assert!(!st.bold);
        assert_eq!(st.font_size_pt, Some(10.0));
        assert_eq!(st.font_color_hex.as_deref(), Some("#808080"));
    }

    // ── numFmt threaded onto the cell text (number-formats.xlsx vectors) ──────

    /// The number-formats.xlsx acceptance stylesheet (custom 164 currency +
    /// built-in date/percent/thousands/decimal/time xfs).
    const NUMFMT_STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <numFmts count="1"><numFmt numFmtId="164" formatCode="&quot;$&quot;#,##0.00"/></numFmts>
  <fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>
  <fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills>
  <borders count="1"><border/></borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  <cellXfs count="7">
    <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>
    <xf numFmtId="14" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/>
    <xf numFmtId="10" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/>
    <xf numFmtId="3" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/>
    <xf numFmtId="2" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/>
    <xf numFmtId="164" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/>
    <xf numFmtId="21" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/>
  </cellXfs>
</styleSheet>"#;

    #[test]
    fn numfmt_applied_to_cell_text() {
        let fc = calibri_font_cache();
        // B2 date(xf1)=45292, B3 percent(xf2)=0.625, B4 thousands(xf3)=1234567,
        // B5 decimal(xf4)=3.14159, B6 currency(xf5)=1299.99, B7 time(xf6)=0.5625.
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="B2:B7"/>
  <sheetData>
    <row r="2"><c r="B2" t="n" s="1"><v>45292</v></c></row>
    <row r="3"><c r="B3" t="n" s="2"><v>0.625</v></c></row>
    <row r="4"><c r="B4" t="n" s="3"><v>1234567</v></c></row>
    <row r="5"><c r="B5" t="n" s="4"><v>3.14159</v></c></row>
    <row r="6"><c r="B6" t="n" s="5"><v>1299.99</v></c></row>
    <row r="7"><c r="B7" t="n" s="6"><v>0.5625</v></c></row>
  </sheetData>
</worksheet>"#;
        let m = load_styled(sheet, &[], NUMFMT_STYLES, &fc);
        let text = |row: usize| m.rows[row].cells[0].text.as_str();
        // built-in 14 (US short date): 45292 → 1/1/2024 (locale-resolved, not
        // the fork's literal mm-dd-yy).
        assert_eq!(text(0), "1/1/2024");
        // 0.00% (built-in 10): 0.625 → 62.50%.
        assert_eq!(text(1), "62.50%");
        // #,##0 (built-in 3): 1234567 → 1,234,567.
        assert_eq!(text(2), "1,234,567");
        // 0.00 (built-in 2): 3.14159 → 3.14.
        assert_eq!(text(3), "3.14");
        // custom 164 "$"#,##0.00: 1299.99 → $1,299.99.
        assert_eq!(text(4), "$1,299.99");
        // h:mm:ss (built-in 21): 0.5625 of a day = 13:30:00.
        assert_eq!(text(5), "13:30:00");
        // All numeric → right-aligned (general), no explicit override.
        assert_eq!(m.rows[0].cells[0].align, HAlign::Right);
    }

    #[test]
    fn numfmt_id_zero_keeps_raw_general() {
        // A cell with the default xf (no numFmt) keeps the raw serial (General).
        let fc = calibri_font_cache();
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="B2:B2"/>
  <sheetData><row r="2"><c r="B2" t="n"><v>45292</v></c></row></sheetData>
</worksheet>"#;
        let m = load_styled(sheet, &[], NUMFMT_STYLES, &fc);
        assert_eq!(m.rows[0].cells[0].text, "45292", "no numFmt → raw serial");
    }
}

// ─── merged cells (§18.3.1.55) ───────────────────────────────────────────────
//
// Mirrors merged.xlsx (A1:C1 spanning title + A3:A4 / A5:A6 vertical group
// cells). Asserts the parsed MergeRegion, covered-cell suppression (the merge's
// non-anchor cells are dropped), anchor box expansion (width = sum of spanned
// col widths, height = sum of spanned row heights), and interior-rail
// suppression via the per-page MergeBox list. Geometry re-pinned (merges change
// covered-cell emission + anchor span) — sml pins only.

#[cfg(test)]
mod merge_tests {
    use super::*;
    use crate::sml::grid::PageCell;
    use crate::sml::model::MergeRegion;

    /// merged.xlsx shape: A1:C1 title, A3:A4 + A5:A6 vertical group cells, with
    /// the surrounding sparse content (so covered-cell suppression is exercised
    /// against real neighbours, not an isolated merge). Shared (`pub(super)`)
    /// with the Grid-v2 index equivalence tests; content unchanged.
    pub(super) const MERGED_SHEET_XML: &str = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:C7"/>
  <sheetFormatPr defaultRowHeight="15"/>
  <sheetData>
    <row r="1" ht="22" customHeight="1"><c r="A1" t="s"><v>0</v></c></row>
    <row r="2"><c r="A2" t="s"><v>1</v></c><c r="B2" t="s"><v>2</v></c><c r="C2" t="s"><v>3</v></c></row>
    <row r="3"><c r="A3" t="s"><v>4</v></c><c r="B3" t="s"><v>5</v></c><c r="C3" t="n"><v>1899</v></c></row>
    <row r="4"><c r="B4" t="s"><v>6</v></c><c r="C4" t="n"><v>549</v></c></row>
    <row r="5"><c r="A5" t="s"><v>7</v></c><c r="B5" t="s"><v>8</v></c><c r="C5" t="n"><v>1200</v></c></row>
    <row r="6"><c r="B6" t="s"><v>9</v></c><c r="C6" t="n"><v>350</v></c></row>
    <row r="7"><c r="A7" t="s"><v>10</v></c><c r="C7" t="n"><v>3998</v></c></row>
  </sheetData>
  <mergeCells count="3"><mergeCell ref="A1:C1"/><mergeCell ref="A3:A4"/><mergeCell ref="A5:A6"/></mergeCells>
</worksheet>"#;

    pub(super) const MERGED_STRINGS: &[&str] = &[
        "Invoice", "Group", "Item", "Amount", "Hardware", "Laptop", "Monitor", "Software",
        "License", "Support", "Total",
    ];

    pub(super) fn merged_model() -> SheetModel {
        let fc = calibri_font_cache();
        load_model(MERGED_SHEET_XML, MERGED_STRINGS, &fc)
    }

    #[test]
    fn merge_ref_parses_into_normalised_region() {
        let m = merged_model();
        assert_eq!(
            m.merges,
            vec![
                MergeRegion { min_row: 1, min_col: 1, max_row: 1, max_col: 3 }, // A1:C1
                MergeRegion { min_row: 3, min_col: 1, max_row: 4, max_col: 1 }, // A3:A4
                MergeRegion { min_row: 5, min_col: 1, max_row: 6, max_col: 1 }, // A5:A6
            ]
        );
    }

    fn page_cell_at<'a>(cells: &'a [PageCell], x: f64, y: f64) -> Option<&'a PageCell> {
        cells.iter().find(|c| (c.x - x).abs() < 0.01 && (c.y - y).abs() < 0.01)
    }

    #[test]
    fn covered_cells_suppressed_anchor_spans_full_width() {
        // A1:C1: B1/C1 are covered → dropped (they have no content anyway, but
        // the anchor A1 must expand to span all three columns).
        let m = merged_model();
        let pages = grid::paginate(&m, PageBox::default());
        assert_eq!(pages.len(), 1);
        let p = &pages[0];

        // The full 3-column width (the sum of col_x spans, = content_w here).
        let full_w = p.col_rails[3] - p.col_rails[0];
        let anchor = page_cell_at(&p.cells, 0.0, 0.0).expect("A1 anchor at top-left");
        assert!(anchor.is_merged_anchor, "A1 is the merge anchor");
        assert_eq!(anchor.text, "Invoice");
        assert_eq!(anchor.w, full_w, "anchor width = sum of the 3 column widths");

        // No cell other than the anchor starts at row-1's y (B1/C1 suppressed).
        let row1_cells: Vec<&PageCell> = p.cells.iter().filter(|c| c.y == 0.0).collect();
        assert_eq!(row1_cells.len(), 1, "only the anchor remains on the merged row");

        // The page records exactly the three merge boxes for rail suppression.
        assert_eq!(p.merges.len(), 3, "A1:C1 + A3:A4 + A5:A6");
        let title_box = p.merges.iter().find(|mb| mb.y == 0.0).expect("title merge box");
        assert_eq!(title_box.w, full_w);
    }

    #[test]
    fn vertical_group_anchor_spans_two_rows() {
        // A3:A4: anchor A3 spans rows 3+4; A4 (covered, empty) is dropped. The
        // anchor height = row3 + row4 heights (both default 15pt → 20px each).
        let m = merged_model();
        let pages = grid::paginate(&m, PageBox::default());
        let p = &pages[0];

        // Row y-offsets: row1=22pt(29px... actually 22pt→29.33→pt_to_px). Compute
        // from the page's own row_rails to stay independent of the px formula.
        // row_rails index: rail[k] is the top of the (min_row+k)-th row.
        let y_row3 = p.row_rails[2]; // rows are 1-based from min_row=1; row3 → idx 2
        let y_row5 = p.row_rails[4];
        let h_row3 = p.row_rails[3] - p.row_rails[2];
        let h_row4 = p.row_rails[4] - p.row_rails[3];

        let a3 = page_cell_at(&p.cells, 0.0, y_row3).expect("A3 anchor");
        assert!(a3.is_merged_anchor);
        assert_eq!(a3.text, "Hardware");
        assert!(
            (a3.h - (h_row3 + h_row4)).abs() < 0.01,
            "A3 anchor height = rows 3+4 summed (got {}, rails {})",
            a3.h,
            h_row3 + h_row4
        );
        // The covered A4 cell is gone: no cell at column-0 / row-4 y.
        let y_row4 = p.row_rails[3];
        assert!(
            page_cell_at(&p.cells, 0.0, y_row4).is_none(),
            "A4 (covered) is suppressed"
        );

        // A5:A6 likewise.
        let a5 = page_cell_at(&p.cells, 0.0, y_row5).expect("A5 anchor");
        assert!(a5.is_merged_anchor);
        assert_eq!(a5.text, "Software");
        let h_row5 = p.row_rails[5] - p.row_rails[4];
        let h_row6 = p.row_rails[6] - p.row_rails[5];
        assert!((a5.h - (h_row5 + h_row6)).abs() < 0.01);

        // B3/C3 (NOT covered — only column A is merged in the vertical group)
        // still render normally at row-3 y.
        assert!(
            page_cell_at(&p.cells, p.col_rails[1], y_row3).is_some(),
            "B3 survives (not part of the A-only vertical merge)"
        );
    }

    #[test]
    fn single_cell_ref_is_one_by_one_region() {
        // A bare "B4" ref (no ':') parses to a 1×1 region covering only its
        // anchor — harmless (no cells suppressed beyond the anchor itself).
        let fc = calibri_font_cache();
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B4"/>
  <sheetData>
    <row r="4"><c r="B4" t="n"><v>9</v></c></row>
  </sheetData>
  <mergeCells count="1"><mergeCell ref="B4"/></mergeCells>
</worksheet>"#;
        let m = load_model(sheet, &[], &fc);
        assert_eq!(
            m.merges,
            vec![MergeRegion { min_row: 4, min_col: 2, max_row: 4, max_col: 2 }]
        );
        let pages = grid::paginate(&m, PageBox::default());
        // The cell still renders (anchor of a 1×1 region) and is flagged anchor.
        let b4 = pages[0].cells.iter().find(|c| c.text == "9").expect("B4");
        assert!(b4.is_merged_anchor);
    }
}

// ─── wide-sheet column tiling (§18.3.1.63 downThenOver) ─────────────────────
//
// `PageBox.width` drives tiling: a sheet wider than the usable page width splits
// into column BANDS, emitted downThenOver (all row-chunks of band 1, then band
// 2…). Asserts the band-split page count for a 24-column fixture, the
// downThenOver page ordering, the single-oversized-column guard (a column wider
// than the whole page still gets its own band), and the cross-band merge clamp
// (an anchor spanning a band boundary truncates at the band right edge; the
// continuation in the next band stays blank — a simplification).

#[cfg(test)]
mod tiling_tests {
    use super::*;
    use crate::sml::grid::PageCell;

    /// Build a single-sheet xlsx whose worksheet body + dimension are supplied,
    /// returning the first sheet's model (default 8-char columns unless the body
    /// declares `<cols>`).
    fn wide_model(dimension: &str, pre: &str, body: &str, fc: &FontCache) -> SheetModel {
        let sheet = format!(
            "<?xml version=\"1.0\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"{dimension}\"/><sheetFormatPr defaultRowHeight=\"15\" baseColWidth=\"8\"/>{pre}<sheetData>{body}</sheetData></worksheet>"
        );
        load_model(&sheet, &[], fc)
    }

    /// 1-based column number → A1 column letters (mirror of grid::col_letters,
    /// used to build cell refs for the wide body).
    fn col_letters(n: u32) -> String {
        grid::col_letters(n)
    }

    /// A 24-column × 3-row sheet of default-width columns. At the loaded font's
    /// MDW (Arimo, 9px → 77px per 8-char column) the usable page width (816 minus
    /// the ~26px row gutter = 790px) holds 10 columns per band, so 24 columns tile
    /// into 3 bands (10 + 10 + 4). Each cell carries its column number as its
    /// value so the band membership is checkable.
    fn twenty_four_col_model(fc: &FontCache) -> SheetModel {
        let mut body = String::new();
        for r in 1..=3u32 {
            body.push_str(&format!("<row r=\"{r}\">"));
            for c in 1..=24u32 {
                let reff = format!("{}{}", col_letters(c), r);
                // Value = column number (1..24), unique per column for assertions.
                body.push_str(&format!("<c r=\"{reff}\" t=\"n\"><v>{c}</v></c>"));
            }
            body.push_str("</row>");
        }
        wide_model("A1:X3", "", &body, fc)
    }

    #[test]
    fn wide_sheet_splits_into_column_bands() {
        let fc = calibri_font_cache();
        let model = twenty_four_col_model(&fc);
        let pages = grid::paginate(&model, PageBox::default());

        // Every page fits within one band of the usable page width.
        let row_gutter = grid::row_gutter_width_for_max_row(3);
        let usable = PageBox::default().width - row_gutter;
        for (i, p) in pages.iter().enumerate() {
            assert!(
                p.content_w <= usable + 0.01,
                "page {i} band width {} exceeds usable {usable}",
                p.content_w
            );
        }

        // 3 short rows fit one page vertically, so #pages == #bands. With the
        // loaded Arimo MDW (9 → 77px/col, 10 cols/band) → 3 bands for 24 columns.
        assert_eq!(pages.len(), 3, "24 cols at 77px/col → 3 bands (10+10+4)");

        // The union of every band's columns is exactly 1..=24, contiguous and
        // non-overlapping (no column dropped, none duplicated).
        let mut next_expected_first = 1u32;
        for p in &pages {
            assert_eq!(p.first_col, next_expected_first, "bands are contiguous");
            let band_cols = p.col_rails.len() as u32 - 1;
            next_expected_first += band_cols;
        }
        assert_eq!(next_expected_first, 25, "bands cover columns 1..=24 exactly");

        // Band 1 = cols 1..=10, band 2 = 11..=20, band 3 = 21..=24.
        assert_eq!(pages[0].first_col, 1);
        assert_eq!(pages[0].col_rails.len() - 1, 10);
        assert_eq!(pages[1].first_col, 11);
        assert_eq!(pages[1].col_rails.len() - 1, 10);
        assert_eq!(pages[2].first_col, 21);
        assert_eq!(pages[2].col_rails.len() - 1, 4);
    }

    #[test]
    fn band_cells_carry_only_their_band_columns() {
        // Each band's cells must be exactly the columns of that band — a cell's
        // value is its column number, so the min/max value per page bounds the
        // band, and band-local x rails restart at 0.
        let fc = calibri_font_cache();
        let model = twenty_four_col_model(&fc);
        let pages = grid::paginate(&model, PageBox::default());

        for p in &pages {
            // Band-local rails always start at 0 (each band paints from its own
            // left edge, not a global offset).
            assert_eq!(p.col_rails[0], 0.0, "band-local x rails restart at 0");
            let last_col = p.first_col + (p.col_rails.len() as u32 - 1) - 1;
            for c in &p.cells {
                let v: u32 = c.text.parse().expect("cell value is a column number");
                assert!(
                    v >= p.first_col && v <= last_col,
                    "cell value {v} outside band {}..={last_col}",
                    p.first_col
                );
            }
        }
    }

    #[test]
    fn downthenover_band_one_precedes_band_two() {
        // §18.3.1.63 default downThenOver: all row-chunks of band 1 are emitted
        // before band 2. Make the sheet TALL enough that each band spans two
        // pages, then assert the page order is [b1p1, b1p2, b2p1, b2p2, …] — i.e.
        // first_col is non-decreasing and every band-1 page precedes every band-2
        // page. 60 rows × 20px = 1200px > one 1056px page → 2 row-chunks/band.
        let fc = calibri_font_cache();
        let mut body = String::new();
        for r in 1..=60u32 {
            body.push_str(&format!("<row r=\"{r}\">"));
            for c in 1..=24u32 {
                let reff = format!("{}{}", col_letters(c), r);
                body.push_str(&format!("<c r=\"{reff}\" t=\"n\"><v>{c}</v></c>"));
            }
            body.push_str("</row>");
        }
        let model = wide_model("A1:X60", "", &body, &fc);
        let pages = grid::paginate(&model, PageBox::default());

        // 3 bands × 2 row-chunks = 6 pages.
        assert_eq!(pages.len(), 6, "3 bands × 2 row-chunks = 6 pages");

        // first_col must be non-decreasing across the page stream (downThenOver:
        // a band's pages are contiguous, bands ascend left-to-right).
        let firsts: Vec<u32> = pages.iter().map(|p| p.first_col).collect();
        assert_eq!(firsts, vec![1, 1, 11, 11, 21, 21], "band-1 pages precede band-2");

        // Within each band the two pages are the top then bottom row-chunk.
        assert_eq!(pages[0].row_numbers.first(), Some(&1), "band1 page1 starts row 1");
        assert!(
            pages[1].row_numbers[0] > pages[0].row_numbers[0],
            "band1 page2 is the lower row-chunk"
        );
        // Band 2 restarts at row 1 (the same row pagination repeats per band).
        assert_eq!(pages[2].row_numbers.first(), Some(&1), "band2 page1 restarts at row 1");
    }

    #[test]
    fn single_oversized_column_gets_its_own_band() {
        // A column WIDER than the whole usable page width still gets its own band
        // (the single-oversized guard mirrors the row loop's). Column A = 200
        // chars (way over a page), columns B,C normal → band 1 = [A], then B,C.
        let fc = calibri_font_cache();
        let pre = r#"<cols><col min="1" max="1" width="200" customWidth="1"/></cols>"#;
        let body = r#"<row r="1"><c r="A1" t="n"><v>1</v></c><c r="B1" t="n"><v>2</v></c><c r="C1" t="n"><v>3</v></c></row>"#;
        let model = wide_model("A1:C1", pre, body, &fc);
        let pages = grid::paginate(&model, PageBox::default());

        // The oversized A is alone in band 1; B,C share band 2.
        assert_eq!(pages.len(), 2, "oversized column gets its own band");
        assert_eq!(pages[0].first_col, 1);
        assert_eq!(pages[0].col_rails.len() - 1, 1, "band 1 holds only column A");
        assert!(
            pages[0].content_w > PageBox::default().width,
            "the oversized band exceeds the page width (not force-split mid-column)"
        );
        assert_eq!(pages[1].first_col, 2);
        assert_eq!(pages[1].col_rails.len() - 1, 2, "band 2 holds B + C");
    }

    fn page_cell<'a>(cells: &'a [PageCell], reff: &str) -> Option<&'a PageCell> {
        cells.iter().find(|c| c.ref_a1 == reff)
    }

    #[test]
    fn cross_band_merge_clamps_to_band_right_edge() {
        // A merge A1:X1 spans all 24 columns — across every band boundary.
        // Behaviour: the anchor's WIDTH is clamped to band 1's right edge;
        // the continuation in bands 2/3 stays BLANK (the covered cells are
        // suppressed globally). So band 1's anchor width == band 1's content_w,
        // and bands 2/3 have NO cell on row 1 (all their row-1 columns are
        // covered by the merge).
        let fc = calibri_font_cache();
        let mut body = String::new();
        for r in 1..=2u32 {
            body.push_str(&format!("<row r=\"{r}\">"));
            for c in 1..=24u32 {
                let reff = format!("{}{}", col_letters(c), r);
                body.push_str(&format!("<c r=\"{reff}\" t=\"n\"><v>{c}</v></c>"));
            }
            body.push_str("</row>");
        }
        let pre = "";
        let sheet = format!(
            "<?xml version=\"1.0\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"A1:X2\"/><sheetFormatPr defaultRowHeight=\"15\" baseColWidth=\"8\"/>{pre}<sheetData>{body}</sheetData><mergeCells count=\"1\"><mergeCell ref=\"A1:X1\"/></mergeCells></worksheet>"
        );
        let model = load_model(&sheet, &[], &fc);
        let pages = grid::paginate(&model, PageBox::default());
        assert_eq!(pages.len(), 3, "3 bands");

        // Band 1: the A1 anchor is present, clamped to band 1's right edge.
        let b1 = &pages[0];
        let anchor = page_cell(&b1.cells, "A1").expect("A1 anchor on band 1");
        assert!(anchor.is_merged_anchor);
        assert!(
            (anchor.w - b1.content_w).abs() < 0.01,
            "anchor width clamped to band 1 width ({} vs {})",
            anchor.w,
            b1.content_w
        );
        // Exactly one merge box on band 1, none on bands 2/3 (the continuation is
        // blank, not re-anchored).
        assert_eq!(b1.merges.len(), 1, "band 1 carries the clamped merge box");
        assert_eq!(pages[1].merges.len(), 0, "band 2 shows blank continuation");
        assert_eq!(pages[2].merges.len(), 0, "band 3 shows blank continuation");

        // Bands 2/3 have NO row-1 cells (every row-1 column is covered & dropped).
        for (i, p) in pages.iter().enumerate().skip(1) {
            let row1: Vec<&PageCell> = p.cells.iter().filter(|c| c.y == 0.0).collect();
            assert!(row1.is_empty(), "band {i} row-1 is blank under the cross-band merge");
        }

        // Row 2 (unmerged) still renders normally in every band.
        assert!(page_cell(&pages[0].cells, "A2").is_some(), "band 1 row 2 renders");
        assert!(page_cell(&pages[1].cells, "K2").is_some(), "band 2 row 2 renders");
        assert!(page_cell(&pages[2].cells, "U2").is_some(), "band 3 row 2 renders");
    }
}

// ─── theme/tint/indexed colour resolution + empty-sheet tab ──────────────────
//
// Colour: a workbook styled ONLY with theme=N(+tint) / indexed colours
// (zero rgb fills, a pattern found in the wild) must resolve every fill,
// font and border colour through the ColorCtx (rgb → theme(+tint §18.8.19) →
// indexed §18.8.27 → auto). Resolving rgb alone leaves every such colour None
// and the whole sheet renders white. Empty sheet: an EMPTY sheet still produces a
// (blank) page so its tab appears in sheet_spans.

#[cfg(test)]
mod color_ctx_tests {
    use super::*;
    use crate::engine::color::apply_excel_tint;
    use crate::sml::model::ColorCtx;

    /// Minimal Office-default theme part (§20.1.6.2 clrScheme): dk1/lt1 via
    /// sysClr@lastClr, the rest srgbClr — both extract paths covered.
    const THEME1: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office">
 <a:themeElements><a:clrScheme name="Office">
  <a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
  <a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
  <a:dk2><a:srgbClr val="44546A"/></a:dk2>
  <a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
  <a:accent1><a:srgbClr val="4472C4"/></a:accent1>
  <a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
  <a:accent3><a:srgbClr val="A5A5A5"/></a:accent3>
  <a:accent4><a:srgbClr val="FFC000"/></a:accent4>
  <a:accent5><a:srgbClr val="5B9BD5"/></a:accent5>
  <a:accent6><a:srgbClr val="70AD47"/></a:accent6>
  <a:hlink><a:srgbClr val="0563C1"/></a:hlink>
  <a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
 </a:clrScheme></a:themeElements>
</a:theme>"#;

    /// Styles using ONLY theme(+tint)/indexed/auto colours — no rgb anywhere
    /// (the pattern that renders white when only rgb resolves).
    const THEMED_STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <fonts count="3">
    <font><sz val="11"/><name val="Calibri"/></font>
    <font><color theme="0"/><name val="Calibri"/></font>
    <font><color indexed="60"/><name val="Calibri"/></font>
  </fonts>
  <fills count="4">
    <fill><patternFill patternType="none"/></fill>
    <fill><patternFill patternType="gray125"/></fill>
    <fill><patternFill patternType="solid"><fgColor theme="4" tint="0.6"/></patternFill></fill>
    <fill><patternFill patternType="solid"><fgColor theme="3"/></patternFill></fill>
  </fills>
  <borders count="2">
    <border/>
    <border>
      <left style="thin"><color theme="9" tint="-0.25"/></left>
      <right style="thin"><color indexed="10"/></right>
      <top style="thin"/>
      <bottom style="thin"><color auto="1"/></bottom>
    </border>
  </borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  <cellXfs count="3">
    <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>
    <xf numFmtId="0" fontId="1" fillId="2" borderId="0" xfId="0"/>
    <xf numFmtId="0" fontId="2" fillId="3" borderId="1" xfId="0"/>
  </cellXfs>
</styleSheet>"#;

    /// Two-sheet workbook (sheet 2 EMPTY — the blank-sheet case) with a theme part
    /// wired through workbook rels (the OPC theme self-parse route).
    fn build_themed_xlsx(styles: &str) -> Vec<u8> {
        const CT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
  <Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>"#;
        const WB: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="Colors" sheetId="1" r:id="rId1"/>
    <sheet name="空白" sheetId="2" r:id="rId4"/>
  </sheets>
</workbook>"#;
        const WB_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/>
</Relationships>"#;
        // A1 themed-fill+white-font (xf 1), B1 indexed-font + dk2 fill + themed
        // borders (xf 2). Sheet 2 is EMPTY (no rows → dimensions None).
        const SHEET1: &str = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B1"/>
  <sheetData>
    <row r="1"><c r="A1" t="s" s="1"><v>0</v></c><c r="B1" t="n" s="2"><v>7</v></c></row>
  </sheetData>
</worksheet>"#;
        const SHEET2: &str = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData/>
</worksheet>"#;
        build_xlsx(&[
            ("[Content_Types].xml", CT),
            ("_rels/.rels", ROOT_RELS),
            ("xl/workbook.xml", WB),
            ("xl/_rels/workbook.xml.rels", WB_RELS),
            ("xl/theme/theme1.xml", THEME1),
            ("xl/styles.xml", styles),
            ("xl/sharedStrings.xml", &sst(&["Title"])),
            ("xl/worksheets/sheet1.xml", SHEET1),
            ("xl/worksheets/sheet2.xml", SHEET2),
        ])
    }

    #[test]
    fn theme_self_parse_applies_excel_slot_swap() {
        // The OPC self-parse must order slots in Excel `color@theme` INDEX
        // order: 0→lt1, 1→dk1 (SWAPPED vs the XML's dk1-first order), 2→lt2,
        // 3→dk2, 4-9→accent1-6, 10→hlink, 11→folHlink.
        let r = SmlRenderer::from_xlsx(build_themed_xlsx(THEMED_STYLES)).expect("from_xlsx");
        let ctx = r.color_ctx();
        assert_eq!(ctx.theme[0], "FFFFFF", "slot 0 = lt1 (sysClr lastClr)");
        assert_eq!(ctx.theme[1], "000000", "slot 1 = dk1 (the swap)");
        assert_eq!(ctx.theme[2], "E7E6E6", "slot 2 = lt2");
        assert_eq!(ctx.theme[3], "44546A", "slot 3 = dk2 (the swap)");
        assert_eq!(ctx.theme[4], "4472C4", "slot 4 = accent1");
        assert_eq!(ctx.theme[9], "70AD47", "slot 9 = accent6");
        assert_eq!(ctx.theme[11], "954F72", "slot 11 = folHlink");
    }

    #[test]
    fn themed_styles_resolve_fill_font_border() {
        let fc = calibri_font_cache();
        let r = SmlRenderer::from_xlsx(build_themed_xlsx(THEMED_STYLES)).expect("from_xlsx");
        let pages = r.paginate_all(&fc);
        let cell = |ref_a1: &str| {
            pages[0]
                .cells
                .iter()
                .find(|c| c.ref_a1 == ref_a1)
                .unwrap_or_else(|| panic!("{ref_a1} missing"))
        };

        // A1 (xf 1): font theme=0 → lt1 white; fill theme=4 tint=0.6 → accent1
        // 4472C4 lightened 60 % = #B4C7E7 (Excel's exact swatch value).
        let a1 = cell("A1").style.as_ref().expect("A1 styled");
        assert_eq!(a1.font_color_hex.as_deref(), Some("#FFFFFF"));
        assert_eq!(a1.fill_hex.as_deref(), Some("#B4C7E7"));

        // B1 (xf 2): font indexed=60 → §18.8.27 palette #993300; fill theme=3 →
        // dk2 #44546A (no tint). Borders: left theme=9 tint=-0.25 → accent6
        // 70AD47 darkened 25 % = #548235 (Excel's exact swatch value); right
        // indexed=10 → #FF0000; top no colour + bottom auto → #808080 fallback.
        let b1 = cell("B1").style.as_ref().expect("B1 styled");
        assert_eq!(b1.font_color_hex.as_deref(), Some("#993300"));
        assert_eq!(b1.fill_hex.as_deref(), Some("#44546A"));
        let [left, right, top, bottom] = &b1.borders;
        assert_eq!(left.as_ref().unwrap().color, "#548235");
        assert_eq!(
            left.as_ref().unwrap().color,
            apply_excel_tint("70AD47", -0.25),
            "border tint must go through the §18.8.19 HSL-luminance math"
        );
        assert_eq!(right.as_ref().unwrap().color, "#FF0000");
        assert_eq!(top.as_ref().unwrap().color, "#808080");
        assert_eq!(bottom.as_ref().unwrap().color, "#808080");
    }

    #[test]
    fn swatch_tint_steps_differ_per_row() {
        // A swatch board: one theme colour across graduated tints must
        // yield DISTINCT fills per step (rgb-only resolution gives None → white).
        let r = SmlRenderer::from_xlsx(build_themed_xlsx(THEMED_STYLES)).expect("from_xlsx");
        let ctx = r.color_ctx();
        let mut c = ooxml_sml::types::Color::default();
        c.theme = Some(4);
        let mut seen = Vec::new();
        for tint in [0.8, 0.6, 0.4, -0.25, -0.5] {
            c.tint = Some(tint);
            let hex = ctx.resolve(&c).expect("themed colour resolves");
            assert!(!seen.contains(&hex), "tint {tint} must differ: {seen:?}");
            seen.push(hex);
        }
    }

    #[test]
    fn resolve_precedence_rgb_theme_indexed_auto() {
        let ctx = ColorCtx::build(
            {
                let mut t: [String; 12] = Default::default();
                t[4] = "4472C4".to_string();
                t
            },
            &ooxml_sml::types::Stylesheet::default(),
        );
        // rgb wins over theme+indexed.
        let mut c = ooxml_sml::types::Color::default();
        c.rgb = Some(vec![0xFF, 0x12, 0x34, 0x56]);
        c.theme = Some(4);
        c.indexed = Some(10);
        assert_eq!(ctx.resolve(&c).as_deref(), Some("#123456"));
        // theme (no tint) → raw slot.
        let mut c = ooxml_sml::types::Color::default();
        c.theme = Some(4);
        assert_eq!(ctx.resolve(&c).as_deref(), Some("#4472C4"));
        // theme pointing at an EMPTY slot (no theme part) → None, graceful.
        c.theme = Some(7);
        assert_eq!(ctx.resolve(&c), None);
        // indexed → §18.8.27 default palette.
        let mut c = ooxml_sml::types::Color::default();
        c.indexed = Some(2);
        assert_eq!(ctx.resolve(&c).as_deref(), Some("#FF0000"));
        // indexed 64/65 (system fore/background) → None (auto).
        c.indexed = Some(64);
        assert_eq!(ctx.resolve(&c), None);
        // auto/none → None.
        let mut c = ooxml_sml::types::Color::default();
        c.auto = Some(true);
        assert_eq!(ctx.resolve(&c), None);
    }

    #[test]
    fn stylesheet_indexed_colors_overlay_replaces_palette_by_position() {
        // §18.8.27: a stylesheet <colors><indexedColors> REPLACES the default
        // palette entry-by-position.
        let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>
  <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
  <borders count="1"><border/></borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  <cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs>
  <colors><indexedColors><rgbColor rgb="FF112233"/><rgbColor rgb="FFAABBCC"/></indexedColors></colors>
</styleSheet>"#;
        let r = SmlRenderer::from_xlsx(build_themed_xlsx(styles)).expect("from_xlsx");
        let ctx = r.color_ctx();
        assert_eq!(ctx.indexed[0], "#112233", "entry 0 overlaid");
        assert_eq!(ctx.indexed[1], "#AABBCC", "entry 1 overlaid");
        assert_eq!(ctx.indexed[2], "#FF0000", "entry 2 keeps the default");
    }

    #[test]
    fn value_less_styled_cells_keep_their_fill_ink() {
        // A swatch board can be built ENTIRELY of value-less styled cells
        // (`<c r="A12" s="26"/>` — fill, no <v>). The empty-cell skip rule must
        // keep a cell whose style paints ink (fill/border); a value-less cell
        // with only a FONT override still drops (no visible ink).
        let fc = calibri_font_cache();
        let styles = r##"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <fonts count="2">
    <font><sz val="11"/><name val="Calibri"/></font>
    <font><b/><name val="Calibri"/></font>
  </fonts>
  <fills count="3">
    <fill><patternFill patternType="none"/></fill>
    <fill><patternFill patternType="gray125"/></fill>
    <fill><patternFill patternType="solid"><fgColor rgb="FFB4C7E7"/></patternFill></fill>
  </fills>
  <borders count="1"><border/></borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  <cellXfs count="3">
    <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>
    <xf numFmtId="0" fontId="0" fillId="2" borderId="0" xfId="0"/>
    <xf numFmtId="0" fontId="1" fillId="0" borderId="0" xfId="0"/>
  </cellXfs>
</styleSheet>"##;
        // Swap the styles part into the themed workbook builder's part list by
        // building directly: A1 fill-only (kept), B1 font-only (dropped).
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B1"/>
  <sheetData><row r="1"><c r="A1" s="1"/><c r="B1" s="2"/></row></sheetData>
</worksheet>"#;
        let bytes = build_xlsx(&[
            ("[Content_Types].xml", CONTENT_TYPES),
            ("_rels/.rels", ROOT_RELS),
            ("xl/workbook.xml", WORKBOOK),
            ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS),
            ("xl/styles.xml", styles),
            ("xl/sharedStrings.xml", &sst(&[])),
            ("xl/worksheets/sheet1.xml", sheet),
        ]);
        let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
        let pages = r.paginate_all(&fc);
        assert_eq!(pages.len(), 1);
        let a1 = pages[0].cells.iter().find(|c| c.ref_a1 == "A1").expect("fill-only A1 kept");
        assert!(a1.text.is_empty(), "no text, just ink");
        assert_eq!(
            a1.style.as_ref().and_then(|st| st.fill_hex.as_deref()),
            Some("#B4C7E7"),
            "the fill rides onto the page cell"
        );
        assert!(
            pages[0].cells.iter().all(|c| c.ref_a1 != "B1"),
            "font-only value-less cell paints nothing → dropped"
        );
    }

    #[test]
    fn empty_sheet_synthesizes_blank_page_and_tab() {
        // The EMPTY sheet must still paginate (blank default grid) so
        // sheet_spans surfaces its tab. Zero pages would make the tab vanish.
        let fc = calibri_font_cache();
        let r = SmlRenderer::from_xlsx(build_themed_xlsx(THEMED_STYLES)).expect("from_xlsx");
        let pages = r.paginate_all(&fc);
        let spans = grid::sheet_spans(&pages);
        assert_eq!(spans.len(), 2, "BOTH sheets get a span (empty included)");
        assert_eq!(spans[0].name, "Colors");
        assert_eq!(spans[1].name, "空白");
        assert_eq!(spans[1].page_count, 1, "empty sheet → exactly one blank page");

        let blank = &pages[spans[1].first_page];
        assert!(blank.cells.is_empty(), "blank page has no cells");
        // Synthesized default range: 25 rows × 10 cols → 26 row rails, 11 col
        // rails, row numbers 1..=25, first col A.
        assert_eq!(blank.row_rails.len(), 26);
        assert_eq!(blank.col_rails.len(), 11);
        assert_eq!(blank.row_numbers.first(), Some(&1));
        assert_eq!(blank.row_numbers.last(), Some(&25));
        assert_eq!(blank.first_col, 1);
    }
}

// ─── embedded drawings (anchored pictures) ───────────────────────────────────
// Synthetic xlsx with a real drawing part chain (sheet rels → xdr:wsDr →
// drawing rels → media png), driven through the full SmlRenderer:
//  - §18.3.1.36  <drawing r:id> on the worksheet
//  - §20.5.2.33  twoCellAnchor (from/to col+colOff/row+rowOff in EMU)
//  - §20.5.2.24  oneCellAnchor (from + xdr:ext cx/cy)
//  - §20.1.8.13  a:blip r:embed → media part
mod drawing_tests {
    use std::io::{Cursor, Write};

    use super::super::grid;
    use super::{calibri_font_cache, sst, ARIMO_MDW, ROOT_RELS, STYLES, WORKBOOK, WORKBOOK_RELS};
    use crate::engine::units::pt_to_px;

    /// 1×1 px PNG (the pml::tests::TINY_PNG bytes — duplicated; that const is
    /// `pml`-feature-gated and module-private).
    const TINY_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8,
        2, 0, 0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192,
        0, 0, 3, 1, 1, 0, 201, 254, 146, 239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    const CONTENT_TYPES_DRAWING: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
  <Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/>
</Types>"#;

    const SHEET_WITH_DRAWING: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <dimension ref="A1:C3"/>
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c></row>
    <row r="3"><c r="C3" t="s"><v>0</v></c></row>
  </sheetData>
  <drawing r:id="rId1"/>
</worksheet>"#;

    const SHEET_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/>
</Relationships>"#;

    const DRAWING_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>
</Relationships>"#;

    /// twoCellAnchor: from B2+(1px,2px) to C3+(0,0) — EMU offsets are 9525/px.
    /// The pic carries a realistic spPr/xfrm whose a:ext must NOT be mistaken
    /// for the anchor extent (the rails win for a twoCellAnchor).
    pub(super) const DRAWING_TWO_CELL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <xdr:twoCellAnchor editAs="oneCell">
    <xdr:from><xdr:col>1</xdr:col><xdr:colOff>9525</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>19050</xdr:rowOff></xdr:from>
    <xdr:to><xdr:col>2</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
    <xdr:pic>
      <xdr:nvPicPr><xdr:cNvPr id="2" name="pic"/><xdr:cNvPicPr/></xdr:nvPicPr>
      <xdr:blipFill><a:blip xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:embed="rId1"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill>
      <xdr:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="999999" cy="999999"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr>
    </xdr:pic>
    <xdr:clientData/>
  </xdr:twoCellAnchor>
</xdr:wsDr>"#;

    /// oneCellAnchor at A1+(0,0), xdr:ext 10×20px. The spPr a:ext is larger —
    /// the FIRST positive ext (xdr:ext, document order) must win.
    const DRAWING_ONE_CELL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <xdr:oneCellAnchor>
    <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
    <xdr:ext cx="95250" cy="190500"/>
    <xdr:pic>
      <xdr:nvPicPr><xdr:cNvPr id="2" name="pic"/><xdr:cNvPicPr/></xdr:nvPicPr>
      <xdr:blipFill><a:blip xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:embed="rId1"/></xdr:blipFill>
      <xdr:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="999999" cy="999999"/></a:xfrm></xdr:spPr>
    </xdr:pic>
    <xdr:clientData/>
  </xdr:oneCellAnchor>
</xdr:wsDr>"#;

    /// Zip a drawing-carrying xlsx (str parts + the binary png media part).
    /// (`pub(super)`: the Grid-v2 index equivalence tests reuse it so the
    /// closed-form extent is checked against the picture extension.)
    pub(super) fn xlsx_with_drawing(drawing_xml: &str) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let strings = sst(&["x"]);
        let str_parts: &[(&str, &str)] = &[
            ("[Content_Types].xml", CONTENT_TYPES_DRAWING),
            ("_rels/.rels", ROOT_RELS),
            ("xl/workbook.xml", WORKBOOK),
            ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS),
            ("xl/styles.xml", STYLES),
            ("xl/sharedStrings.xml", &strings),
            ("xl/worksheets/sheet1.xml", SHEET_WITH_DRAWING),
            ("xl/worksheets/_rels/sheet1.xml.rels", SHEET_RELS),
            ("xl/drawings/drawing1.xml", drawing_xml),
            ("xl/drawings/_rels/drawing1.xml.rels", DRAWING_RELS),
        ];
        for (name, content) in str_parts {
            zw.start_file(*name, opts).expect("zip start_file");
            zw.write_all(content.as_bytes()).expect("zip write");
        }
        zw.start_file("xl/media/image1.png", opts).expect("zip start png");
        zw.write_all(TINY_PNG).expect("zip write png");
        zw.finish().expect("zip finish").into_inner()
    }

    #[test]
    fn two_cell_anchor_lands_on_the_grid_rails() {
        let fc = calibri_font_cache();
        let mut r = super::SmlRenderer::from_xlsx(xlsx_with_drawing(DRAWING_TWO_CELL))
            .expect("from_xlsx");

        // Media bytes extracted at load, keyed by the sheet-namespaced key.
        let images = r.take_images();
        assert_eq!(images.len(), 1, "one media part collected");
        let (key, img) = &images[0];
        assert_eq!(key, "sml::0::rId1", "sheet-namespaced image key");
        assert_eq!(img.mime, "image/png");
        assert_eq!(img.data, TINY_PNG, "the exact media part bytes");
        assert!(r.take_images().is_empty(), "images drained after take");

        // Anchor box on the rails: default col width (8 chars at the measured
        // Arimo MDW) and default row height (15pt → 20px), EMU/9525 offsets.
        let pages = r.paginate_all(&fc);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].images.len(), 1, "one anchored picture on the page");
        let im = &pages[0].images[0];
        assert_eq!(im.key, "sml::0::rId1");
        let col_w = grid::chars_to_px(8.0, ARIMO_MDW);
        let row_h = pt_to_px(15.0);
        let (x0, y0) = (col_w + 1.0, row_h + 2.0); // B2 + (9525, 19050) EMU
        assert!((im.x - x0).abs() < 0.01, "x {} ≠ {x0}", im.x);
        assert!((im.y - y0).abs() < 0.01, "y {} ≠ {y0}", im.y);
        // to = C3+(0,0): width spans to col C's left rail, height to row 3's top.
        assert!((im.w - (2.0 * col_w - x0)).abs() < 0.01, "w {} (col_w {col_w})", im.w);
        assert!((im.h - (2.0 * row_h - y0)).abs() < 0.01, "h {}", im.h);

        // The geometry hash now folds the image box (drift detection).
        let h1 = grid::geometry_hash(&pages);
        let mut no_img = pages.clone();
        no_img[0].images.clear();
        assert_ne!(h1, grid::geometry_hash(&no_img), "image box folds into the hash");
    }

    #[test]
    fn one_cell_anchor_uses_xdr_ext_not_the_xfrm_ext() {
        let fc = calibri_font_cache();
        let r = super::SmlRenderer::from_xlsx(xlsx_with_drawing(DRAWING_ONE_CELL))
            .expect("from_xlsx");
        let pages = r.paginate_all(&fc);
        assert_eq!(pages[0].images.len(), 1);
        let im = &pages[0].images[0];
        assert!((im.x).abs() < 0.01 && (im.y).abs() < 0.01, "anchored at A1 origin");
        assert!((im.w - 10.0).abs() < 0.01, "xdr:ext cx=95250 → 10px, got {}", im.w);
        assert!((im.h - 20.0).abs() < 0.01, "xdr:ext cy=190500 → 20px, got {}", im.h);
    }

    // ─── xdr anchor coordinates are bounded at parse ─────────────────────────
    //
    // `<xdr:col>`/`<xdr:row>` are `ST_ColID`/`ST_RowID` (§20.5.3.1/§20.5.3.3):
    // `xsd:int` restricted ONLY below (min 0) — no spec maximum — and both
    // consumers (`grid::paginate`'s `col_left`, `index::geom_for`'s
    // `col_left_any`/`row_top`) walk per-column/per-row rails up to the anchor.
    // See `drawings::anchor_point` for the bound and its policy.

    /// `DRAWING_TWO_CELL` with its FROM rail rewritten (col 1 → `col`,
    /// row 1 → `row`; the to rail is col/row 2, so the replaced strings are
    /// unique). Asserts the substitution happened so template drift cannot
    /// silently turn the callers vacuous.
    fn drawing_with_from(col: &str, row: &str) -> String {
        let s = DRAWING_TWO_CELL
            .replace("<xdr:col>1</xdr:col>", &format!("<xdr:col>{col}</xdr:col>"))
            .replace("<xdr:row>1</xdr:row>", &format!("<xdr:row>{row}</xdr:row>"));
        assert_ne!(s, DRAWING_TWO_CELL, "from-rail substitution must take");
        s
    }

    /// In-u32 coordinates far past the grid: without the parse bound each one
    /// is a ~2×10⁹-iteration rail walk (measured 3.0 s release / 78 s debug
    /// for ONE anchor) on BOTH the paged and the continuous-grid paths. The
    /// anchor must simply not exist.
    #[test]
    fn out_of_grid_anchor_walks_are_rejected_at_parse() {
        let fc = calibri_font_cache();
        for (what, col, row) in
            [("col walk", "2000000000", "1"), ("row walk", "1", "2000000000")]
        {
            let r = super::SmlRenderer::from_xlsx(xlsx_with_drawing(&drawing_with_from(col, row)))
                .expect("from_xlsx");
            let pages = r.paginate_all(&fc);
            assert!(pages[0].images.is_empty(), "{what}: paged anchor dropped");
            let gs = r.ensure_geom(0, &fc).expect("geom");
            assert!(gs.geom.images.is_empty(), "{what}: continuous-grid anchor dropped");
            assert!(gs.geom.max_col <= 16_384, "{what}: grid not extended toward the anchor");
        }
    }

    /// `u32::MAX` parses into `AnchorPoint::col`/`row`, and every consumer's
    /// 0-based→1-based `+ 1` then overflows (debug panic at `index::geom_for`'s
    /// `to_box`, wrap in release). The parse bound rejects it long before.
    #[test]
    fn out_of_grid_anchor_coordinates_do_not_overflow() {
        let fc = calibri_font_cache();
        for (what, col, row) in
            [("col u32::MAX", "4294967295", "1"), ("row u32::MAX", "1", "4294967295")]
        {
            let r = super::SmlRenderer::from_xlsx(xlsx_with_drawing(&drawing_with_from(col, row)))
                .expect("from_xlsx");
            assert!(r.paginate_all(&fc)[0].images.is_empty(), "{what}: paged anchor dropped");
            let gs = r.ensure_geom(0, &fc).expect("geom");
            assert!(gs.geom.images.is_empty(), "{what}: continuous-grid anchor dropped");
        }
    }

    /// A hostile TO rail alone does not take the picture with it: the from
    /// rail is fine, so the drawing degrades to its ext fallback (here the
    /// pic's `a:xfrm` `a:ext`, 999 999 EMU ≈ 105 px) instead of a rail span
    /// walked two billion columns wide.
    #[test]
    fn hostile_to_rail_degrades_to_the_ext_fallback() {
        let fc = calibri_font_cache();
        let hostile = DRAWING_TWO_CELL.replace("<xdr:col>2</xdr:col>", "<xdr:col>2000000000</xdr:col>");
        assert_ne!(hostile, DRAWING_TWO_CELL, "to-rail substitution must take");
        let r = super::SmlRenderer::from_xlsx(xlsx_with_drawing(&hostile)).expect("from_xlsx");
        let pages = r.paginate_all(&fc);
        assert_eq!(pages[0].images.len(), 1, "picture kept on its valid from rail");
        let ext_px = 999_999.0 / 9525.0;
        assert!((pages[0].images[0].w - ext_px).abs() < 0.01, "w {} ≠ ext {ext_px}", pages[0].images[0].w);
    }

    /// The bound's edge, kept: xdr coordinates are 0-based, so col 16 383 /
    /// row 1 048 575 is the LAST cell (XFD1048576) and col 16 384 /
    /// row 1 048 576 is the one-past-the-edge boundary a `to` rail names for a
    /// picture flush against the grid corner. Neither is dropped.
    #[test]
    fn grid_edge_anchors_survive_the_bound() {
        let fc = calibri_font_cache();
        let drawing = drawing_with_from("16383", "1048575")
            .replace("<xdr:col>2</xdr:col>", "<xdr:col>16384</xdr:col>")
            .replace("<xdr:row>2</xdr:row>", "<xdr:row>1048576</xdr:row>");
        assert!(drawing.contains("<xdr:col>16384</xdr:col>"), "to-rail substitution must take");
        let r = super::SmlRenderer::from_xlsx(xlsx_with_drawing(&drawing)).expect("from_xlsx");
        let gs = r.ensure_geom(0, &fc).expect("geom");
        assert_eq!(gs.geom.images.len(), 1, "the grid-corner anchor renders");
        assert!(gs.geom.images[0].w > 0.0 && gs.geom.images[0].h > 0.0, "spans its last cell");
        // NOT-VACUOUS GUARD. The two assertions above cannot see an off-by-one
        // (`>=` for `>`) regression of the very boundary this test names: a
        // rejected `to` rail still yields ONE image with positive extent — it
        // just degrades to the `ext` fallback (see
        // `hostile_to_rail_degrades_to_the_ext_fallback`). So pin the thing that
        // actually distinguishes the two paths: a `to`-rail picture spans real
        // grid cells and is therefore WIDER than the raw `ext`.
        let ext_px = 999_999.0 / 9525.0;
        assert!(
            (gs.geom.images[0].w - ext_px).abs() > 0.01,
            "w {} collapsed to the ext fallback {ext_px} — the `to` rail was REJECTED, \
             i.e. the bound is off by one at its own documented edge",
            gs.geom.images[0].w
        );
    }
}

// ─── xlsx embedded charts (grid placement + shared painter + object hit) ─────
mod chart_tests {
    use std::collections::HashMap;
    use std::io::{Cursor, Write};

    use super::super::{grid, render};
    use super::{calibri_font_cache, ARIMO_MDW, ROOT_RELS, STYLES, WORKBOOK, WORKBOOK_RELS};
    use crate::engine::displaylist::{DrawCmd, RecordingPainter};
    use crate::engine::units::pt_to_px;

    const CONTENT_TYPES_CHART: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
  <Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/>
  <Override PartName="/xl/charts/chart1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>
</Types>"#;

    const DRAWING_RELS_CHART: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/>
</Relationships>"#;

    /// twoCellAnchor graphicFrame from B2+(0,0) to E10+(0,0) referencing the
    /// chart part via `<c:chart r:id="rId1"/>` (§20.5.2.13 + §21.2).
    const DRAWING_GRAPHIC_FRAME: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <xdr:twoCellAnchor>
    <xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
    <xdr:to><xdr:col>4</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>9</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
    <xdr:graphicFrame macro="">
      <xdr:nvGraphicFramePr><xdr:cNvPr id="2" name="Chart 1"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr>
      <xdr:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></xdr:xfrm>
      <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
        <c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="rId1"/>
      </a:graphicData></a:graphic>
    </xdr:graphicFrame>
    <xdr:clientData/>
  </xdr:twoCellAnchor>
</xdr:wsDr>"#;

    /// A TWO-series clustered bar chart (§21.2 c:chartSpace) — the fixture the
    /// SHARED `crate::chart` painter parses AS-IS (mirrors the pml CHART_PART).
    pub(crate) const CHART_PART: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <c:chart>
    <c:plotArea>
      <c:layout/>
      <c:barChart>
        <c:barDir val="col"/>
        <c:grouping val="clustered"/>
        <c:ser>
          <c:idx val="0"/><c:order val="0"/>
          <c:tx><c:strRef><c:f>Chart!$B$1</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>Sales</c:v></c:pt></c:strCache></c:strRef></c:tx>
          <c:cat><c:strRef><c:f>Chart!$A$2:$A$5</c:f><c:strCache><c:ptCount val="4"/><c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt><c:pt idx="2"><c:v>Q3</c:v></c:pt><c:pt idx="3"><c:v>Q4</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:f>Chart!$B$2:$B$5</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val="4"/><c:pt idx="0"><c:v>18</c:v></c:pt><c:pt idx="1"><c:v>32</c:v></c:pt><c:pt idx="2"><c:v>25</c:v></c:pt><c:pt idx="3"><c:v>41</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
        <c:ser>
          <c:idx val="1"/><c:order val="1"/>
          <c:tx><c:strRef><c:f>Chart!$C$1</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>Target</c:v></c:pt></c:strCache></c:strRef></c:tx>
          <c:cat><c:strRef><c:f>Chart!$A$2:$A$5</c:f><c:strCache><c:ptCount val="4"/><c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt><c:pt idx="2"><c:v>Q3</c:v></c:pt><c:pt idx="3"><c:v>Q4</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:f>Chart!$C$2:$C$5</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val="4"/><c:pt idx="0"><c:v>20</c:v></c:pt><c:pt idx="1"><c:v>30</c:v></c:pt><c:pt idx="2"><c:v>30</c:v></c:pt><c:pt idx="3"><c:v>40</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
        <c:axId val="1"/><c:axId val="2"/>
      </c:barChart>
      <c:catAx><c:axId val="1"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/><c:crossAx val="2"/></c:catAx>
      <c:valAx><c:axId val="2"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="l"/><c:crossAx val="1"/></c:valAx>
    </c:plotArea>
    <c:legend><c:legendPos val="r"/></c:legend>
    <c:plotVisOnly val="1"/>
  </c:chart>
</c:chartSpace>"#;

    /// Zip a chart-carrying xlsx: sheet(+rels) → drawing(+rels) → chart part.
    /// `pane_xml` (optional `<pane…/>`) arms frozen panes for the straddle
    /// tests; `""` = pane-less.
    fn xlsx_with_chart(pane_xml: &str) -> Vec<u8> {
        let views = if pane_xml.is_empty() {
            String::new()
        } else {
            format!(
                r#"<sheetViews><sheetView workbookViewId="0">{pane_xml}</sheetView></sheetViews>"#
            )
        };
        let sheet = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <dimension ref="A1:C5"/>
  {views}
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c></row>
    <row r="5"><c r="C5" t="s"><v>0</v></c></row>
  </sheetData>
  <drawing r:id="rId1"/>
</worksheet>"#
        );
        let sheet_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/>
</Relationships>"#;
        let strings = super::sst(&["x"]);
        let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let parts: &[(&str, &str)] = &[
            ("[Content_Types].xml", CONTENT_TYPES_CHART),
            ("_rels/.rels", ROOT_RELS),
            ("xl/workbook.xml", WORKBOOK),
            ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS),
            ("xl/styles.xml", STYLES),
            ("xl/sharedStrings.xml", &strings),
            ("xl/worksheets/sheet1.xml", &sheet),
            ("xl/worksheets/_rels/sheet1.xml.rels", sheet_rels),
            ("xl/drawings/drawing1.xml", DRAWING_GRAPHIC_FRAME),
            ("xl/drawings/_rels/drawing1.xml.rels", DRAWING_RELS_CHART),
            ("xl/charts/chart1.xml", CHART_PART),
        ];
        for (name, content) in parts {
            zw.start_file(*name, opts).expect("zip start_file");
            zw.write_all(content.as_bytes()).expect("zip write");
        }
        zw.finish().expect("zip finish").into_inner()
    }

    fn renderer(pane_xml: &str) -> super::SmlRenderer {
        super::SmlRenderer::from_xlsx(xlsx_with_chart(pane_xml)).expect("from_xlsx")
    }

    /// The chart-XML map the Viewer holds after the loadXlsx drain.
    fn chart_map(r: &mut super::SmlRenderer) -> HashMap<String, Vec<u8>> {
        r.take_charts().into_iter().collect()
    }

    /// The expected anchor box: B2 → E10 on default rails (8-char cols at the
    /// measured Arimo MDW, 15pt rows).
    fn expected_box() -> (f64, f64, f64, f64) {
        let col_w = grid::chars_to_px(8.0, ARIMO_MDW);
        let row_h = pt_to_px(15.0);
        (col_w, row_h, 3.0 * col_w, 8.0 * row_h)
    }

    #[test]
    fn chart_anchor_and_part_bytes_are_captured_under_the_namespaced_key() {
        let fc = calibri_font_cache();
        let mut r = renderer("");

        // Part bytes drained under the sheet-namespaced chart key (the
        // loadXlsx → chart_xml contract), NOT into the image map.
        assert!(r.take_images().is_empty(), "a chart is not an image");
        let charts = chart_map(&mut r);
        assert_eq!(charts.len(), 1);
        assert_eq!(
            charts["chart::sml::0::rId1"], CHART_PART.as_bytes(),
            "the exact chart part bytes under chart::sml::{{sheetIdx}}::{{relId}}"
        );
        assert!(r.take_charts().is_empty(), "charts drained after take");

        // Placement on the print page: rails box B2→E10, routed to
        // `SheetPage::charts` (never `images`).
        let pages = r.paginate_all(&fc);
        assert_eq!(pages.len(), 1);
        assert!(pages[0].images.is_empty());
        assert_eq!(pages[0].charts.len(), 1);
        let ch = &pages[0].charts[0];
        assert_eq!(ch.key, "chart::sml::0::rId1");
        let (x, y, w, h) = expected_box();
        assert!((ch.x - x).abs() < 0.01, "x {} ≠ {x}", ch.x);
        assert!((ch.y - y).abs() < 0.01, "y {} ≠ {y}", ch.y);
        assert!((ch.w - w).abs() < 0.01, "w {} ≠ {w}", ch.w);
        assert!((ch.h - h).abs() < 0.01, "h {} ≠ {h}", ch.h);

        // The geometry hash folds the chart box (drift detection), guarded on
        // non-empty (chart-less pages hash as before — the pinned baselines).
        let h1 = grid::geometry_hash(&pages);
        let mut no_chart = pages.clone();
        no_chart[0].charts.clear();
        assert_ne!(h1, grid::geometry_hash(&no_chart), "chart box folds into the hash");
    }

    #[test]
    fn shared_copy_pipeline_parses_the_sml_chart_part() {
        // chartData's engine half: the SHARED crate::chart::copy_data over the
        // stored part bytes — series/categories are the painted numbers.
        let data = crate::chart::copy_data(CHART_PART.as_bytes()).expect("copy data");
        assert_eq!(data.categories, vec!["Q1", "Q2", "Q3", "Q4"]);
        assert_eq!(data.series.len(), 2, "two series");
        assert_eq!(data.series[0].0, "Sales");
        assert_eq!(data.series[0].1, vec![18.0, 32.0, 25.0, 41.0]);
        assert_eq!(data.series[1].0, "Target");
        assert_eq!(data.series[1].1, vec![20.0, 30.0, 30.0, 40.0]);
    }

    /// Record one production-wired grid viewport paint (the pane_tests
    /// template + the chart map / selected key).
    fn paint_ops(
        r: &super::SmlRenderer,
        fc: &crate::engine::fonts::FontCache,
        charts: &HashMap<String, Vec<u8>>,
        selected: Option<&str>,
        scroll_x: f64,
        scroll_y: f64,
    ) -> Vec<DrawCmd> {
        let gs = r.ensure_geom(0, fc).expect("geom");
        let geom = &gs.geom;
        let gut_w = grid::row_gutter_width_for_max_row(geom.max_row);
        let gut_h = grid::COL_GUTTER_H;
        let (r0, r1, win_x1) = crate::sml::grid_window(geom, scroll_x, scroll_y, 640.0, 480.0);
        let page = r.window_page(0, fc, r0, r1, win_x1).expect("window page");
        let parts = r.pane_paint_parts(0, fc, scroll_x, 640.0, 480.0, win_x1);
        let pane_ctx = parts
            .as_ref()
            .map(|(b, bp)| render::PaneCtx { bands: *b, band_page: bp.as_ref() });
        let rec = RecordingPainter::new();
        render::paint_grid_viewport(
            &rec,
            &page,
            (gut_w, gut_h),
            scroll_x,
            scroll_y,
            640.0,
            480.0,
            r.normal_font(),
            fc,
            &HashMap::new(),
            charts,
            selected,
            None,
            None,
            pane_ctx.as_ref(),
        )
        .expect("paint");
        rec.take()
    }

    /// The first series' bar fill ("#4472c4", chart.rs DEFAULT_COLORS[0]) —
    /// the chart-specific marker no grid chrome ever emits.
    fn chart_marker_count(cmds: &[DrawCmd]) -> usize {
        cmds.iter()
            .filter(|c| matches!(c, DrawCmd::SetFillStyle(s) if s == "#4472c4"))
            .count()
    }

    #[test]
    fn grid_viewport_paints_the_chart_at_its_anchor() {
        let fc = calibri_font_cache();
        let mut r = renderer("");
        let charts = chart_map(&mut r);
        let cmds = paint_ops(&r, &fc, &charts, None, 0.0, 0.0);
        assert!(chart_marker_count(&cmds) >= 1, "the shared painter drew series bars");
        // The white chart-area ground sits at the anchored box (gutter-shifted).
        let gs = r.ensure_geom(0, &fc).expect("geom");
        let gut_w = grid::row_gutter_width_for_max_row(gs.geom.max_row);
        let gut_h = grid::COL_GUTTER_H;
        let (x, y, w, h) = expected_box();
        let ground = cmds.iter().any(|c| {
            matches!(c, DrawCmd::FillRect { x: fx, y: fy, w: fw, h: fh }
                if (fx - (gut_w + x)).abs() < 0.01
                && (fy - (gut_h + y)).abs() < 0.01
                && (fw - w).abs() < 0.01
                && (fh - h).abs() < 0.01)
        });
        assert!(ground, "white chart ground at the anchored rect");
        // Scrolled: the chart moves with content (its ground shifts by −scroll).
        let scrolled = paint_ops(&r, &fc, &charts, None, 20.0, 30.0);
        let moved = scrolled.iter().any(|c| {
            matches!(c, DrawCmd::FillRect { x: fx, y: fy, .. }
                if (fx - (gut_w + x - 20.0)).abs() < 0.01 && (fy - (gut_h + y - 30.0)).abs() < 0.01)
        });
        assert!(moved, "chart scrolls with the grid content");
        // An EMPTY chart map (key unresolvable) paints no chart marks at all.
        assert_eq!(chart_marker_count(&paint_ops(&r, &fc, &HashMap::new(), None, 0.0, 0.0)), 0);
    }

    #[test]
    fn paint_grid_into_records_a_viewport_for_worker_replay() {
        // Worker-render guard: the paint_grid_into seam (which record_grid_
        // viewport drives off-thread) must run the full grid prelude and emit real
        // cell/gridline draw commands, so the worker replays the true grid. paint_ops
        // covers render::paint_grid_viewport directly; this covers the paint_grid_into
        // seam the record export uses. Reverting the seam (empty/reject) goes RED.
        // (Zoom bakes into the content-space viewport dims + a caller-side ctx.scale,
        // not a recorded Scale — the record is content 1:1.)
        let fc = calibri_font_cache();
        let r = renderer("");
        let rec = RecordingPainter::new();
        r.paint_grid_into(&rec, 0, 0.0, 0.0, 640.0, 480.0, 1.0, &fc, &HashMap::new(), None, None, None)
            .expect("grid viewport records through the RecordingPainter");
        let cmds = rec.take();
        assert!(!cmds.is_empty(), "the grid viewport must record draw commands off-thread");
        // Real grid content: cells/gridlines paint filled rects.
        assert!(
            cmds.iter().any(|c| matches!(c, DrawCmd::FillRect { .. })),
            "the grid must record cell/gridline fills, not an empty list"
        );
    }

    #[test]
    fn paint_grid_into_records_the_cell_selection_overlay() {
        // Worker-overlay guard: record_grid_viewport sources the live cell
        // selection from engine state on `self` and threads it through the SAME
        // paint_grid_into seam. A base viewport (None cell_sel) must record NO
        // Excel-green selection stroke; a SEL viewport (Some cell_sel) MUST record
        // it. Reverting record_grid_viewport to None×3 blanks the worker overlay
        // and turns this RED. `#217346` is the SEL_TINT accent (render.rs).
        use super::super::select::{CellRange, CellSelection};
        let fc = calibri_font_cache();
        let r = renderer("");

        let base = RecordingPainter::new();
        r.paint_grid_into(&base, 0, 0.0, 0.0, 640.0, 480.0, 1.0, &fc, &HashMap::new(), None, None, None)
            .expect("base grid records");
        let base_cmds = base.take();
        assert!(
            !base_cmds
                .iter()
                .any(|c| matches!(c, DrawCmd::SetStrokeStyle(s) if s == "#217346")),
            "no selection: the base grid records NO SEL_TINT stroke (byte-identical to today's None call)"
        );

        let sel = CellSelection {
            sheet: 0,
            range: CellRange::new(1, 1, 2, 2),
            anchor_row: 1,
            anchor_col: 1,
        };
        let sel_rec = RecordingPainter::new();
        r.paint_grid_into(&sel_rec, 0, 0.0, 0.0, 640.0, 480.0, 1.0, &fc, &HashMap::new(), None, None, Some(sel))
            .expect("selection grid records");
        let sel_cmds = sel_rec.take();
        assert!(
            sel_cmds
                .iter()
                .any(|c| matches!(c, DrawCmd::SetStrokeStyle(s) if s == "#217346")),
            "a Some(cell_sel) records the SEL_TINT selection overlay off-thread"
        );
    }

    #[test]
    fn selected_chart_outline_is_additive_and_key_gated() {
        let fc = calibri_font_cache();
        let mut r = renderer("");
        let charts = chart_map(&mut r);
        let base = paint_ops(&r, &fc, &charts, None, 0.0, 0.0);
        // An unknown/unrelated key paints byte-identically to no selection.
        let other = paint_ops(&r, &fc, &charts, Some("chart::rId9"), 0.0, 0.0);
        assert_eq!(format!("{base:?}"), format!("{other:?}"), "unknown key = no outline");
        // The matching key adds exactly one focus-ring stroke pass.
        let sel = paint_ops(&r, &fc, &charts, Some("chart::sml::0::rId1"), 0.0, 0.0);
        let rings = sel
            .iter()
            .filter(|c| matches!(c, DrawCmd::SetStrokeStyle(s) if s == "#3B82F6"))
            .count();
        assert_eq!(rings, 1, "one outline pass for the selected chart");
        assert!(sel.len() > base.len(), "outline is additive");
    }

    #[test]
    fn chart_straddling_the_freeze_boundary_paints_in_every_quadrant() {
        // ySplit=3 puts the freeze boundary through the B2→E10 chart: the
        // pinned band quadrant AND the body quadrant must BOTH paint it
        // (clipped per quadrant by the region path).
        let fc = calibri_font_cache();
        let mut r = renderer(r#"<pane ySplit="3" topLeftCell="A4" state="frozen"/>"#);
        let charts = chart_map(&mut r);
        let cmds = paint_ops(&r, &fc, &charts, None, 0.0, 40.0);
        assert!(
            chart_marker_count(&cmds) >= 2,
            "chart painted in both the pinned band and the body quadrant"
        );
    }

    #[test]
    fn grid_chart_hit_resolves_the_key_and_rect() {
        let fc = calibri_font_cache();
        let r = renderer("");
        let gs = r.ensure_geom(0, &fc).expect("geom");
        let gut_w = grid::row_gutter_width_for_max_row(gs.geom.max_row);
        let gut_h = grid::COL_GUTTER_H;
        let (x, y, w, h) = expected_box();

        // Center hit, unscrolled.
        let (key, rx, ry, rw, rh) = r
            .grid_chart_hit(0, gut_w + x + w / 2.0, gut_h + y + h / 2.0, 0.0, 0.0, &fc)
            .expect("hit");
        assert_eq!(key, "chart::sml::0::rId1");
        assert!((rx - x).abs() < 0.01 && (ry - y).abs() < 0.01);
        assert!((rw - w).abs() < 0.01 && (rh - h).abs() < 0.01);

        // Under scroll the same CONTENT point sits at (view − scroll).
        let hit = r.grid_chart_hit(0, gut_w + x + 5.0 - 15.0, gut_h + y + 5.0 - 10.0, 15.0, 10.0, &fc);
        assert!(hit.is_some(), "hit under scroll");

        // A miss (above/left of the anchor) and a gutter point return None.
        assert!(r.grid_chart_hit(0, gut_w + x - 2.0, gut_h + y - 2.0, 0.0, 0.0, &fc).is_none());
        assert!(r.grid_chart_hit(0, 2.0, 2.0, 0.0, 0.0, &fc).is_none());
    }

    #[test]
    fn grid_chart_hit_resolves_inside_a_frozen_band_regardless_of_scroll() {
        let fc = calibri_font_cache();
        let r = renderer(r#"<pane ySplit="3" topLeftCell="A4" state="frozen"/>"#);
        let gs = r.ensure_geom(0, &fc).expect("geom");
        let gut_w = grid::row_gutter_width_for_max_row(gs.geom.max_row);
        let gut_h = grid::COL_GUTTER_H;
        let (x, y, ..) = expected_box();
        // A point in the pinned band over the chart's top slice: the frozen
        // axis zeroes the scroll (eff_scroll), so a deep y-scroll still hits.
        let hit = r.grid_chart_hit(0, gut_w + x + 5.0, gut_h + y + 5.0, 0.0, 500.0, &fc);
        assert_eq!(hit.map(|(k, ..)| k).as_deref(), Some("chart::sml::0::rId1"));
    }

    #[test]
    fn chart_display_px_reads_the_anchor_box() {
        let fc = calibri_font_cache();
        let r = renderer("");
        let (_, _, w, h) = expected_box();
        let (dw, dh) = r.chart_display_px("chart::sml::0::rId1", &fc).expect("display px");
        assert!((dw - w).abs() < 0.01 && (dh - h).abs() < 0.01);
        assert!(r.chart_display_px("chart::sml::0::rId9", &fc).is_none());
    }

    #[test]
    fn generated_chart_fixture_loads_places_and_parses() {
        // fixtures/spreadsheet/chart.xlsx (scripts/generate-test-fixtures.ts
        // makeChartXlsx): one two-series bar chart anchored E2→K17. End-to-end:
        // load → part bytes under the namespaced key → grid placement → the
        // shared copy parse. Skipped when the fixture set is not fetched
        // (mirrors the docx/pptx fixture-test convention).
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/spreadsheet/chart.xlsx");
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("chart.xlsx fixture not present — run bun scripts/fetch-fixtures.ts");
            return;
        };
        let fc = calibri_font_cache();
        let mut r = super::SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
        let charts = chart_map(&mut r);
        assert!(charts.contains_key("chart::sml::0::rId1"), "part bytes captured");
        let data = crate::chart::copy_data(&charts["chart::sml::0::rId1"]).expect("copy data");
        assert_eq!(data.series.len(), 2);
        assert_eq!(data.series[0].0, "Sales");
        assert_eq!(data.categories, vec!["Q1", "Q2", "Q3", "Q4"]);
        // Placement: E2 (col 4, row 1 zero-based) → K17, on default rails.
        let gs = r.ensure_geom(0, &fc).expect("geom");
        assert_eq!(gs.geom.charts.len(), 1);
        let ch = &gs.geom.charts[0];
        let col_w = grid::chars_to_px(8.0, ARIMO_MDW);
        let row_h = pt_to_px(15.0);
        assert!((ch.x - 4.0 * col_w).abs() < 0.01, "x {} ≠ {}", ch.x, 4.0 * col_w);
        // Row 1 carries ht="18" (the header row), so the chart's top is 18pt.
        assert!((ch.y - pt_to_px(18.0)).abs() < 0.01, "y {}", ch.y);
        assert!((ch.w - 6.0 * col_w).abs() < 0.01);
        assert!((ch.h - 15.0 * row_h).abs() < 0.01, "h {}", ch.h);
    }

    #[test]
    fn print_page_paint_draws_the_chart_too() {
        // paint_sheet_page symmetry: the print page paints the chart at its
        // page-local box through the same shared painter.
        let fc = calibri_font_cache();
        let mut r = renderer("");
        let charts = chart_map(&mut r);
        let pages = r.paginate_all(&fc);
        let rec = RecordingPainter::new();
        render::paint_sheet_page(
            &rec, &pages[0], 0.0, 0.0, 816.0, 1056.0, r.normal_font(), &fc,
            &HashMap::new(), &charts, None,
        )
        .expect("paint");
        assert!(chart_marker_count(&rec.take()) >= 1, "print view draws the chart");
    }
}

// ─── §18.3.1.73 r-less rows (implied sequential) ─────────────────────────────

#[test]
fn rows_without_r_are_implied_sequential() {
    // §18.3.1.73: `@r` is OPTIONAL on `<row>` — when omitted, the row follows
    // the previous one. .NET exporters write whole r-less `<row>` streams with
    // inlineStr cells (documents in the wild are shaped exactly so); a model
    // that skips every r-less row renders such a sheet blank.
    let fc = calibri_font_cache();
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A6"/>
  <sheetData>
    <row><c r="A1" t="inlineStr"><is><t>first</t></is></c></row>
    <row><c r="A2" t="inlineStr"><is><t>second</t></is></c></row>
    <row r="5"><c r="A5" t="inlineStr"><is><t>fifth</t></is></c></row>
    <row><c r="A6" t="inlineStr"><is><t>sixth</t></is></c></row>
  </sheetData>
</worksheet>"#;
    let model = load_model(sheet, &[], &fc);
    let nums: Vec<u32> = model.rows.iter().map(|r| r.row).collect();
    assert_eq!(nums, vec![1, 2, 5, 6], "implied rows 1,2; explicit 5; implied 6");
    assert_eq!(model.rows[0].cells[0].text, "first");
    assert_eq!(model.rows[3].cells[0].text, "sixth");
}

// ─── Grid v2 — continuous-page equivalence-oracle baselines ──────────────────
//
// THE EQUIVALENCE INVARIANT: Grid v2's window path, materialized over the FULL
// row range, must reproduce the v1 `continuous_page` geometry HASH-IDENTICALLY
// on every fixture. These pins are the baseline that invariant is checked
// against — if a v2 change needs one re-pinned, equivalence is broken: fix the
// change, never the pin. (The print-`paginate` oracle above pins the paged
// geometry; these pin the CONTINUOUS grid geometry the windowed painter reads.)
// A fourth continuous pin — over the generated stamped fixture, custom heights
// + r-less rows + chunk-boundary merges at scale — lives in tests/perf.rs.

/// Grid-relevant edge fixture: sparse rows, custom heights (incl. an EMPTY
/// custom-height row §18.3.1.73 — a height exception with no cells), an r-less
/// implied MID-RANGE row, and explicit col-width runs. The dimension stamp is
/// TRUTHFUL (A1:E11) and the r-less row sits below the explicit max row on
/// purpose: `model.dimensions` is the fork's present-rows-derived range
/// (ext.rs `dimensions()` ignores both the `<dimension>` stamp and r-less
/// rows), and v2's dimension-distrust deliberately changes behavior when
/// stamp ≠ content — an equivalence PIN must not depend on that. (Distrust
/// coverage has its own non-pinned tests.)
const SPARSE_HEIGHTS_SHEET_XML: &str = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:E11"/>
  <sheetFormatPr defaultRowHeight="15"/>
  <cols><col min="2" max="3" width="14" customWidth="1"/></cols>
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c><c r="E1" t="n"><v>1</v></c></row>
    <row r="4" ht="33" customHeight="1"><c r="B4" t="n"><v>4</v></c></row>
    <row r="7" ht="24" customHeight="1"/>
    <row r="8"><c r="A8" t="n"><v>8</v></c></row>
    <row><c r="D9" t="inlineStr"><is><t>implied nine</t></is></c></row>
    <row r="11"><c r="C11" t="inlineStr"><is><t>eleven</t></is></c></row>
  </sheetData>
</worksheet>"#;

fn sparse_heights_model() -> SheetModel {
    let fc = calibri_font_cache();
    load_model(SPARSE_HEIGHTS_SHEET_XML, &["Header"], &fc)
}

#[test]
fn grid_v2_oracle_continuous_oracle_model_pinned() {
    let hash = |m: &SheetModel| {
        let p = grid::continuous_page(m).expect("continuous page");
        grid::geometry_hash(std::slice::from_ref(&p))
    };
    let a = hash(&oracle_model());
    assert_eq!(a, hash(&oracle_model()), "continuous geometry must be deterministic");
    if std::env::var("SML_DUMP").is_ok() {
        eprintln!("continuous oracle_model geometry_hash=0x{a:016x}");
    }
    // PINNED baseline (confirmed identical across two runs, 2026-06-10).
    assert_eq!(a, 0x5dba3a8136ccce88, "continuous oracle geometry drift (0x{a:016x})");
}

#[test]
fn grid_v2_oracle_continuous_merged_pinned() {
    let hash = |m: &SheetModel| {
        let p = grid::continuous_page(m).expect("continuous page");
        grid::geometry_hash(std::slice::from_ref(&p))
    };
    let a = hash(&merge_tests::merged_model());
    assert_eq!(a, hash(&merge_tests::merged_model()), "deterministic");
    if std::env::var("SML_DUMP").is_ok() {
        eprintln!("continuous merged_model geometry_hash=0x{a:016x}");
    }
    // PINNED baseline (confirmed identical across two runs, 2026-06-10).
    assert_eq!(a, 0x834f4dd0cebb563c, "continuous merged geometry drift (0x{a:016x})");
}

#[test]
fn grid_v2_oracle_continuous_sparse_heights_pinned() {
    let hash = |m: &SheetModel| {
        let p = grid::continuous_page(m).expect("continuous page");
        grid::geometry_hash(std::slice::from_ref(&p))
    };
    let a = hash(&sparse_heights_model());
    assert_eq!(a, hash(&sparse_heights_model()), "deterministic");
    if std::env::var("SML_DUMP").is_ok() {
        eprintln!("continuous sparse_heights geometry_hash=0x{a:016x}");
    }
    // Structural guards: bands span the derived range (to row 11) and the
    // r-less implied row 9 contributed its cell to the continuous geometry.
    let p = grid::continuous_page(&sparse_heights_model()).expect("page");
    assert_eq!(*p.row_numbers.last().expect("bands"), 11, "bands to the derived max row");
    assert!(p.cells.iter().any(|c| c.ref_a1 == "D9"), "implied row 9 cell present");
    // PINNED baseline (confirmed identical across two runs, 2026-06-10).
    assert_eq!(a, 0xdfda69175e7a0e61, "continuous sparse geometry drift (0x{a:016x})");
}

// ─── Full-text search: cell-model match + page resolution ───────────────────
//
// `SmlRenderer::search_sheet` is the SML search core: substring-match the
// resolved cell strings via the chunk LRU (the same cache paint uses), then lay
// out ONLY a matching cell's page to resolve the page-local rect. These pin the
// model-not-layout invariant the design hinges on for giant fixtures.
#[cfg(test)]
mod search_tests {
    use super::*;
    use crate::SearchOpts;

    /// Default opts: case-insensitive substring, active-sheet.
    fn opts() -> SearchOpts {
        SearchOpts::default()
    }

    /// A single-sheet workbook with three shared-string cells.
    fn three_cell_renderer() -> (SmlRenderer, FontCache) {
        let fc = calibri_font_cache();
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A3"/>
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c></row>
    <row r="2"><c r="A2" t="s"><v>1</v></c></row>
    <row r="3"><c r="A3" t="s"><v>2</v></c></row>
  </sheetData>
</worksheet>"#;
        let bytes = xlsx_with(sheet, &sst(&["Hello World", "Invoice Total", "apple PIE"]));
        let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
        (r, fc)
    }

    #[test]
    fn sml_cell_model_match_case_insensitive() {
        let (r, fc) = three_cell_renderer();
        // "invoice" (lowercase) matches "Invoice Total" (cell A2) case-insensitively.
        let hits = r.search_sheet(0, "invoice", opts(), &fc);
        assert_eq!(hits.len(), 1, "exactly one cell matches");
        let h = &hits[0];
        assert_eq!(h.flat_page, 0, "single short sheet → one page");
        assert_eq!(h.ref_a1, "A2", "the matched cell");
        // The print rect is DEFERRED at search time — the hit carries grid
        // coords (always valid). The rect is resolved LAZILY for Pages-view nav.
        assert!(h.grid_y >= 0.0, "valid grid coord: {:?}", (h.grid_x, h.grid_y));
        let rect = r.locate_cell_rect(0, 2, 1, &fc);
        assert!(
            matches!(rect, Some((_, _, _, w, hgt)) if w > 0.0 && hgt > 0.0),
            "lazy resolver returns a real cell rect: {rect:?}"
        );
    }

    #[test]
    fn sml_case_sensitive_toggle() {
        let (r, fc) = three_cell_renderer();
        let cs = SearchOpts { case_sensitive: true, ..SearchOpts::default() };
        // "PIE" (uppercase) matches "apple PIE"; "pie" (lowercase) does not.
        assert_eq!(r.search_sheet(0, "PIE", cs, &fc).len(), 1, "exact case hits");
        assert_eq!(r.search_sheet(0, "pie", cs, &fc).len(), 0, "wrong case misses");
    }

    /// Disjoint HALF-OPEN row windows partition the full result with NO
    /// double-count — the invariant the streamed per-sheet chunking relies on
    /// (`search_sheet_window` trims chunk-edge rows outside its window).
    #[test]
    fn sml_search_row_window_partitions_without_double_count() {
        use std::collections::BTreeSet;
        let fc = calibri_font_cache();
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A6"/>
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>sale 1</t></is></c></row>
    <row r="2"><c r="A2" t="inlineStr"><is><t>sale 2</t></is></c></row>
    <row r="3"><c r="A3" t="inlineStr"><is><t>sale 3</t></is></c></row>
    <row r="4"><c r="A4" t="inlineStr"><is><t>sale 4</t></is></c></row>
    <row r="5"><c r="A5" t="inlineStr"><is><t>sale 5</t></is></c></row>
    <row r="6"><c r="A6" t="inlineStr"><is><t>sale 6</t></is></c></row>
  </sheetData>
</worksheet>"#;
        let r = SmlRenderer::from_xlsx(xlsx_with(sheet, &sst(&[]))).expect("from_xlsx");
        let collect = |w| -> BTreeSet<String> {
            r.search_sheet_window(0, "s", opts(), &fc, w).into_iter().map(|h| h.ref_a1).collect()
        };
        let full = collect(None);
        assert_eq!(full.len(), 6, "all 6 rows contain 's'");
        // Half-open windows [1,4) (rows 1-3) and [4,7) (rows 4-6) partition exactly.
        let lo = collect(Some((1, 4)));
        let hi = collect(Some((4, 7)));
        assert_eq!(lo.len(), 3, "[1,4) → rows 1-3");
        assert_eq!(hi.len(), 3, "[4,7) → rows 4-6");
        assert!(lo.is_disjoint(&hi), "windows overlap → DOUBLE COUNT");
        let union: BTreeSet<String> = lo.union(&hi).cloned().collect();
        assert_eq!(union, full, "disjoint windows partition the full result");
    }

    #[test]
    fn sml_no_match_and_empty_query() {
        let (r, fc) = three_cell_renderer();
        assert!(r.search_sheet(0, "zzz-nope", opts(), &fc).is_empty(), "no match → empty");
        assert!(r.search_sheet(0, "", opts(), &fc).is_empty(), "empty query → empty");
    }

    /// The load-bearing invariant: searching one sheet NEVER materializes a
    /// DIFFERENT sheet's cells. The active-sheet scope touches only sheet 0's
    /// chunk LRU; sheet 1 is never parsed.
    #[test]
    fn sml_search_does_not_materialize_other_sheets() {
        let fc = calibri_font_cache();
        let s1 = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1"/>
  <sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData>
</worksheet>"#;
        let s2 = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1"/>
  <sheetData><row r="1"><c r="A1" t="s"><v>1</v></c></row></sheetData>
</worksheet>"#;
        let bytes = build_xlsx_two_sheets(s1, s2, &sst(&["needle here", "needle there"]));
        let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
        assert!(!r.any_chunk_loaded(1), "sheet 1 unparsed before search");
        // Search ONLY sheet 0 (active-sheet scope).
        let hits = r.search_sheet(0, "needle", opts(), &fc);
        assert_eq!(hits.len(), 1, "sheet 0 has one match");
        assert!(r.any_chunk_loaded(0), "sheet 0 WAS parsed (it was searched)");
        assert!(
            !r.any_chunk_loaded(1),
            "sheet 1 must stay UNMATERIALIZED — active-sheet search never parses other sheets"
        );
    }

    /// A search hit carries the matched cell's A1 ref (the active-emphasis
    /// identity) and its CONTENT-space grid top-left (what the continuous-grid
    /// scroller jumps to). The ref must equal the A1 the match lives in.
    #[test]
    fn sml_hit_exposes_a1_ref_and_grid_coord() {
        let (r, fc) = three_cell_renderer();
        // "invoice" → cell A2 (row 2, col A).
        let hits = r.search_sheet(0, "invoice", opts(), &fc);
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.ref_a1, "A2", "hit carries the matched cell's A1 ref");
        // Column A is the FIRST column → its content-space left is 0.0.
        assert_eq!(h.grid_x, 0.0, "col A left = 0 in content space, got {}", h.grid_x);
        // Row 2 is below row 1 → strictly positive content-space top.
        assert!(h.grid_y > 0.0, "row 2 sits below the top → grid_y > 0, got {}", h.grid_y);
    }

    /// The grid-content coordinate is re-sourced from the row/col RAILS, so it is
    /// monotone in row order — a deeper match has a larger `grid_y`. (The print
    /// page-local `y` is NOT a usable scroll target for the continuous grid.)
    #[test]
    fn sml_grid_y_is_monotone_in_row() {
        let fc = calibri_font_cache();
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A3"/>
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c></row>
    <row r="2"><c r="A2" t="s"><v>0</v></c></row>
    <row r="3"><c r="A3" t="s"><v>0</v></c></row>
  </sheetData>
</worksheet>"#;
        let bytes = xlsx_with(sheet, &sst(&["needle"]));
        let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
        let hits = r.search_sheet(0, "needle", opts(), &fc);
        assert_eq!(hits.len(), 3, "all three rows match");
        // search_sheet walks row-major, so hits are A1, A2, A3 in order.
        let ys: Vec<f64> = hits.iter().map(|h| h.grid_y).collect();
        let refs: Vec<&str> = hits.iter().map(|h| h.ref_a1.as_str()).collect();
        assert_eq!(refs, ["A1", "A2", "A3"], "row-major emission order");
        assert!(ys[0] < ys[1] && ys[1] < ys[2], "grid_y strictly increases with row: {ys:?}");
        assert_eq!(ys[0], 0.0, "row 1 top = 0 in content space");
    }

    /// The active-emphasis guarantee: a hit's `ref_a1` selects EXACTLY ONE
    /// paintable cell on its page. This is what makes the identity-based active
    /// pick (paint_cells: `sh.active_ref == Some(cell.ref_a1)`) unambiguous — and
    /// robust to the windowed `visible_cells` slice (the grid view paints a
    /// scroll-shifted sub-slice, so a bare index would drift; identity does not).
    #[test]
    fn sml_active_ref_selects_unique_paintable_cell() {
        let fc = calibri_font_cache();
        // Two matching cells on one page (A1 + B2). Each must map to a distinct,
        // unique paintable cell so the active swap targets the right one.
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B2"/>
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c></row>
    <row r="2"><c r="B2" t="s"><v>0</v></c></row>
  </sheetData>
</worksheet>"#;
        let bytes = xlsx_with(sheet, &sst(&["needle"]));
        let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
        let model = load_model(sheet, &["needle"], &fc);
        let pages = grid::paginate(&model, PageBox::default());
        let hits = r.search_sheet(0, "needle", opts(), &fc);
        assert_eq!(hits.len(), 2, "both cells match");
        for h in &hits {
            let n = pages[0].cells.iter().filter(|c| c.ref_a1 == h.ref_a1).count();
            assert_eq!(
                n, 1,
                "hit {} must select exactly one paintable cell (identity is unambiguous)",
                h.ref_a1
            );
        }
        // The two hits target DIFFERENT cells (so the active swap never bleeds).
        assert_ne!(hits[0].ref_a1, hits[1].ref_a1, "distinct active identities");
    }

    /// Workbook scope crosses sheet tabs: both sheets' matches surface, each
    /// carrying its own sheet name.
    #[test]
    fn sml_workbook_scope_spans_sheets() {
        let fc = calibri_font_cache();
        let s1 = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1"/>
  <sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData>
</worksheet>"#;
        let s2 = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1"/>
  <sheetData><row r="1"><c r="A1" t="s"><v>1</v></c></row></sheetData>
</worksheet>"#;
        let bytes = build_xlsx_two_sheets(s1, s2, &sst(&["needle one", "needle two"]));
        let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
        let mut hits = Vec::new();
        for si in 0..r.sheet_count() {
            hits.extend(r.search_sheet(si, "needle", opts(), &fc));
        }
        assert_eq!(hits.len(), 2, "both sheets match under workbook scope");
        let names: Vec<&str> = hits.iter().map(|h| h.sheet_name.as_str()).collect();
        assert!(names.contains(&"One") && names.contains(&"Two"), "names: {names:?}");
    }
}

// ─── docExtractRange, cells (INDEX-RANGE walk + copy cap) ───────────────────
//
// The extraction is a pure INDEX-RANGE walk (include-hidden / forward-compatible,
// geometry-only) over the AREA-CLAMPED rect with the per-content-type copy cap.
// Model-less cells emit empty strings; merges expand and covered cells are
// flagged so TSV keeps them (empty column) while HTML skips them.
#[cfg(test)]
mod extract_tests {
    use super::*;
    use crate::sml::select::{CellRange, MAX_COPY_BYTES, MAX_COPY_ROWS};

    /// A small populated sheet: A1="Hi", B1=42, A2=7 (B2 is model-less/empty).
    fn small_renderer() -> SmlRenderer {
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B2"/>
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="n"><v>42</v></c></row>
    <row r="2"><c r="A2" t="n"><v>7</v></c></row>
  </sheetData>
</worksheet>"#;
        SmlRenderer::from_xlsx(xlsx_with(sheet, &sst(&["Hi"]))).expect("from_xlsx")
    }

    #[test]
    fn extract_rect_reads_display_values_and_empty_for_model_less() {
        let fc = calibri_font_cache();
        let r = small_renderer();
        let frag = r
            .extract_cell_range(0, CellRange::new(1, 1, 2, 2), &fc)
            .expect("fragment");
        assert_eq!(frag.total_rows, 2);
        assert_eq!(frag.row_count, 2);
        assert!(!frag.truncated);
        // Row 1: "Hi", "42".
        assert_eq!(frag.rows[0][0].display, "Hi");
        assert_eq!(frag.rows[0][1].display, "42");
        // Row 2: "7", "" (B2 has no model — empty string, still emitted).
        assert_eq!(frag.rows[1][0].display, "7");
        assert_eq!(frag.rows[1][1].display, "", "model-less cell emits empty");
        assert!(!frag.rows[1][1].covered);
        assert_eq!(frag.rows[1][1].colspan, 1);
    }

    #[test]
    fn extract_expands_a_grazed_merge_and_flags_covered_cells() {
        let fc = calibri_font_cache();
        // A1:C1 merged title "Invoice"; a click/drag over just A1 must copy the
        // whole merge, and the covered B1/C1 must be flagged (empty display).
        let r = SmlRenderer::from_xlsx(xlsx_with(
            merge_tests::MERGED_SHEET_XML,
            &sst(merge_tests::MERGED_STRINGS),
        ))
        .expect("from_xlsx");
        // Select just A1 (inside A1:C1) — expansion must pull in B1, C1.
        let frag = r
            .extract_cell_range(0, CellRange::new(1, 1, 1, 1), &fc)
            .expect("fragment");
        assert_eq!(frag.rows[0].len(), 3, "A1:C1 expansion → 3 columns");
        // Anchor A1 carries the value + a colspan of 3.
        assert_eq!(frag.rows[0][0].display, "Invoice");
        assert_eq!(frag.rows[0][0].colspan, 3);
        assert!(!frag.rows[0][0].covered);
        // B1, C1 are covered (empty display, flagged).
        assert!(frag.rows[0][1].covered && frag.rows[0][1].display.is_empty());
        assert!(frag.rows[0][2].covered && frag.rows[0][2].display.is_empty());
    }

    #[test]
    fn extract_vertical_merge_carries_rowspan_on_the_anchor() {
        let fc = calibri_font_cache();
        let r = SmlRenderer::from_xlsx(xlsx_with(
            merge_tests::MERGED_SHEET_XML,
            &sst(merge_tests::MERGED_STRINGS),
        ))
        .expect("from_xlsx");
        // A3:A4 is a vertical merge ("Hardware"). Select A3 alone → expands to A3:A4.
        let frag = r
            .extract_cell_range(0, CellRange::new(3, 1, 3, 1), &fc)
            .expect("fragment");
        assert_eq!(frag.total_rows, 2, "A3:A4 expansion → 2 rows");
        assert_eq!(frag.rows[0][0].rowspan, 2);
        assert_eq!(frag.rows[0][0].display, "Hardware");
        // Row 2 (A4) is covered by the vertical merge.
        assert!(frag.rows[1][0].covered && frag.rows[1][0].display.is_empty());
    }

    #[test]
    fn csv_baseline_extract_matches_the_cell_values() {
        // The SAME one-sheet shape `csv.ts::csvToXlsx` produces for
        // "name,qty / Apple,5 / Pie, \"special\",2" — inline strings for text, <v>
        // for numbers, no merges/styles (the clean CSV baseline). A drag-select of
        // the full rect must extract exactly the CSV cell values (the engine half
        // of the CSV round-trip; the TS TSV join is covered in sml.test.ts).
        let fc = calibri_font_cache();
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B3"/>
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>name</t></is></c><c r="B1" t="inlineStr"><is><t>qty</t></is></c></row>
    <row r="2"><c r="A2" t="inlineStr"><is><t>Apple</t></is></c><c r="B2"><v>5</v></c></row>
    <row r="3"><c r="A3" t="inlineStr"><is><t xml:space="preserve">Pie, "special"</t></is></c><c r="B3"><v>2</v></c></row>
  </sheetData>
</worksheet>"#;
        let r = SmlRenderer::from_xlsx(xlsx_with(sheet, &sst(&[]))).expect("from_xlsx");
        let frag = r
            .extract_cell_range(0, CellRange::new(1, 1, 3, 2), &fc)
            .expect("fragment");
        let grid: Vec<Vec<&str>> = frag
            .rows
            .iter()
            .map(|row| row.iter().map(|c| c.display.as_str()).collect())
            .collect();
        assert_eq!(
            grid,
            vec![
                vec!["name", "qty"],
                vec!["Apple", "5"],
                vec!["Pie, \"special\"", "2"],
            ]
        );
    }

    #[test]
    fn extract_row_cap_truncates_with_the_rows_signal() {
        let fc = calibri_font_cache();
        let r = small_renderer();
        // A tall sparse selection (mostly the kill-the-void area): the byte budget
        // never trips (empty cells cost ~1 byte), so the ROW cap must catch it.
        let tall = MAX_COPY_ROWS + 10_000;
        let frag = r
            .extract_cell_range(0, CellRange::new(1, 1, tall, 1), &fc)
            .expect("fragment");
        assert_eq!(frag.total_rows, tall);
        assert_eq!(frag.row_count, MAX_COPY_ROWS, "emitted rows clamp at the cap");
        assert!(frag.truncated);
        assert_eq!(frag.reason, Some("rows"));
    }

    #[test]
    fn extract_byte_cap_truncates_with_the_bytes_signal() {
        // The dense-data OOM guard: bytes trip BEFORE the row cap. A tiny injected
        // byte budget over a dense sheet exercises the `reason:"bytes"` branch
        // without an 8MB fixture. small_renderer's sheet has content-bearing rows;
        // a 1-byte budget trips after the first emitted row (bytes > 1).
        let fc = calibri_font_cache();
        let r = small_renderer();
        let frag = r
            .extract_cell_range_capped(0, CellRange::new(1, 1, 5, 3), &fc, MAX_COPY_ROWS, 1)
            .expect("fragment");
        assert!(frag.truncated, "a 1-byte budget must truncate a content-bearing rect");
        assert_eq!(frag.reason, Some("bytes"), "the byte cap, not the row cap, trips");
        assert!(
            frag.row_count < frag.total_rows,
            "fewer rows emitted than the full rect ({} < {})",
            frag.row_count,
            frag.total_rows
        );
    }

    #[test]
    fn extract_truncation_clamps_a_straddling_merge_rowspan() {
        // A vertical merge whose span reaches PAST the truncation boundary must
        // not emit an over-spanning <td>: its rowspan is clamped to the rows
        // actually emitted. A3:A4 merge ("Hardware"); select A1:B4 but cap at 3
        // rows → row 3 (the anchor, rowspan 2 covering rows 3-4) clamps to 1
        // because only row 3 of the merge is emitted (row 4 is past the cap).
        let fc = calibri_font_cache();
        let r = SmlRenderer::from_xlsx(xlsx_with(
            merge_tests::MERGED_SHEET_XML,
            &sst(merge_tests::MERGED_STRINGS),
        ))
        .expect("from_xlsx");
        let frag = r
            .extract_cell_range_capped(0, CellRange::new(1, 1, 4, 2), &fc, 3, MAX_COPY_BYTES)
            .expect("fragment");
        assert!(frag.truncated && frag.reason == Some("rows"));
        assert_eq!(frag.row_count, 3, "cap at 3 rows (A1:A3 of the A1:A4 rect)");
        // The A3:A4 merge anchor sits in emitted row index 2 (A3); only 1 row of
        // the merge is emitted, so its rowspan must clamp 2 → 1 (no over-span).
        assert_eq!(
            frag.rows[2][0].rowspan, 1,
            "straddling merge rowspan clamped to emitted rows"
        );
    }
}

/// O(1)-opens pin: one `from_xlsx` load performs exactly ONE
/// `engine::opc::OpcPackage` open (central-directory + content-types parse),
/// shared by the theme, SST and drawings self-parses (rather than opening the
/// archive three times over three fresh `bytes.to_vec()` copies).
/// SCOPE: `OPEN_COUNT` instruments OUR shared handle only — the fork's own
/// internal parse and `repair/`'s failure-path `ZipArchive` opens are separate
/// and deliberately uncounted (repair only runs when a load has already
/// failed).
#[test]
fn xlsx_load_opens_the_opc_package_exactly_once() {
    let bytes = xlsx_with(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B2"/>
  <sheetData>
    <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"><v>42</v></c></row>
  </sheetData>
</worksheet>"#,
        &sst(&["hello"]),
    );
    let before = crate::engine::opc::OPEN_COUNT.with(|c| c.get());
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
    let opens = crate::engine::opc::OPEN_COUNT.with(|c| c.get()) - before;
    assert_eq!(opens, 1, "one shared OpcPackage per xlsx load, got {opens}");
    assert_eq!(r.indexes.len(), 1, "the load still resolves the sheet");
}

#[test]
fn cell_at_reports_empty_cells_by_ref_not_null() {
    // Regression pin: clicking an EMPTY cell must not resolve to None —
    // indistinguishable from clicking nothing. Excel's formula bar shows
    // "A1, empty"; a downstream formula bar can only make that distinction if
    // the engine does. In-rail empty coordinate → Some((ref, "", "")); gutter
    // and outside-rails clicks stay None.
    let fc = calibri_font_cache();
    // B2 deliberately ABSENT: dimension spans A1:C3, data only at A1 + C3.
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:C3"/>
  <sheetData>
    <row r="1"><c r="A1" t="n"><v>11</v></c></row>
    <row r="3"><c r="C3" t="n"><v>33</v></c></row>
  </sheetData>
</worksheet>"#;
    let model = load_model(sheet, &[], &fc);
    let pages = grid::paginate(&model, PageBox::default());
    let (row_gutter_w, col_gutter_h) = grid::gutter_dims(&pages[0]);

    // Locate B2's band center from the rails (no cell box exists for it).
    let p0 = &pages[0];
    let bx = (p0.col_rails[1] + p0.col_rails[2]) / 2.0; // band k=1 → column B
    let by = (p0.row_rails[1] + p0.row_rails[2]) / 2.0; // band j=1 → row 2
    let cell = std::cell::RefCell::new(pages);
    let hit = super::SmlRenderer::cell_at(&cell, 0, bx + row_gutter_w, by + col_gutter_h)
        .expect("an in-rail empty coordinate must still resolve to its cell");
    assert_eq!(hit, ("B2".to_string(), String::new(), String::new()));

    // The distinction survives: gutter clicks still mean "nothing".
    assert!(super::SmlRenderer::cell_at(&cell, 0, 2.0, 2.0).is_none());

    // Grid path, same contract: an empty in-bounds coordinate reports its ref.
    let r = SmlRenderer::from_xlsx(xlsx_with(sheet, &sst(&[]))).expect("from_xlsx");
    let gw = grid::row_gutter_width_for_max_row(3);
    let gh = grid::COL_GUTTER_H;
    let (a1, raw, disp) = r
        .grid_cell_at(0, gw + bx, gh + by, 0.0, 0.0, &fc)
        .expect("grid: in-bounds empty coordinate resolves");
    assert_eq!((a1.as_str(), raw.as_str(), disp.as_str()), ("B2", "", ""));
    assert!(r.grid_cell_at(0, 2.0, 2.0, 0.0, 0.0, &fc).is_none(), "grid gutter click stays None");
}
