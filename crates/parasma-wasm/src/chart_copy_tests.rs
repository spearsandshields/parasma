//! WML inline-chart copy, native regression tests.
//!
//! Drives the FULL pipeline (`Viewer::new` → `load_docx` → `run_layout_native`)
//! over a SYNTHETIC docx carrying one inline chart (mirroring the pml/tests.rs
//! synthetic-pptx discipline — no binary fixtures, never `fixtures/user/`):
//! `document.xml` holds a `<wp:inline>` drawing whose `<a:graphicData>` is a
//! `<c:chart r:id="rId3"/>`, and `word/charts/chart1.xml` is a 2-series ×
//! 3-category bar `c:chartSpace`.
//!
//! Pins:
//!  1. the chart XML lands under the `chart::rId3` key + `chart::copy_data`
//!     reads the exact series/categories the painter paints (the TSV source),
//!  2. `hit_test_object_impl` reports the inline chart with the DISTINCT kind
//!     `"chart"` (excluded from the image path),
//!  3. `object_outline_rects` outlines the chart's painted box (the click-to-
//!     select outline + the `chartData` display-px source),
//!  4. the highlight-hug fix: a selection sweeping the chart paragraph emits the
//!     chart item's OBJECT BOX (216px tall bitmap box), never a 216px-tall
//!     line-band rect around the neighbouring text.
//!
//! Kept in a SEPARATE file (the image_extraction_tests.rs discipline) so
//! adding/removing cases never shifts `lib.rs` source line numbers (those feed
//! the release wasm's panic-location table).

use super::wml::page::PageElement;
use super::wml::select::{
    hit_test_object_impl, object_outline_rects, selection_rects_impl, TextPos,
};
use super::Viewer;
use std::io::Write;

// ─── synthetic chart docx ────────────────────────────────────────────────────

/// Zip the given parts into a .docx byte vector (stored, deterministic).
fn build_docx(parts: &[(&str, String)]) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    for (name, content) in parts {
        zw.start_file(*name, opts).expect("zip start_file");
        zw.write_all(content.as_bytes()).expect("zip write");
    }
    zw.finish().expect("zip finish").into_inner()
}

const W_NS: &str =
    r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
const R_NS: &str =
    r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

/// The chart's declared display size: 430 × 216 px (a typical inline chart
/// geometry). 1 px = 9525 EMU.
const CHART_W_PX: f64 = 430.0;
const CHART_H_PX: f64 = 216.0;
const CHART_CX_EMU: i64 = 4_095_750; // 430 × 9525
const CHART_CY_EMU: i64 = 2_057_400; // 216 × 9525

/// A 2-series × 3-category clustered-bar `c:chartSpace` (§21.2) — the same
/// schema the shared `crate::chart` painter parses for WML/PML/SML.
const CHART_PART: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <c:chart>
    <c:plotArea>
      <c:layout/>
      <c:barChart>
        <c:barDir val="col"/>
        <c:grouping val="clustered"/>
        <c:ser>
          <c:idx val="0"/>
          <c:order val="0"/>
          <c:tx><c:strRef><c:f>Sheet1!$B$1</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>Alpha</c:v></c:pt></c:strCache></c:strRef></c:tx>
          <c:cat><c:strRef><c:f>Sheet1!$A$2:$A$4</c:f><c:strCache><c:ptCount val="3"/><c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt><c:pt idx="2"><c:v>Q3</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:f>Sheet1!$B$2:$B$4</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val="3"/><c:pt idx="0"><c:v>10</c:v></c:pt><c:pt idx="1"><c:v>24</c:v></c:pt><c:pt idx="2"><c:v>17</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
        <c:ser>
          <c:idx val="1"/>
          <c:order val="1"/>
          <c:tx><c:strRef><c:f>Sheet1!$C$1</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>Beta</c:v></c:pt></c:strCache></c:strRef></c:tx>
          <c:cat><c:strRef><c:f>Sheet1!$A$2:$A$4</c:f><c:strCache><c:ptCount val="3"/><c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt><c:pt idx="2"><c:v>Q3</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:f>Sheet1!$C$2:$C$4</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val="3"/><c:pt idx="0"><c:v>4.5</c:v></c:pt><c:pt idx="1"><c:v>6</c:v></c:pt><c:pt idx="2"><c:v>7.25</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
        <c:axId val="1"/>
        <c:axId val="2"/>
      </c:barChart>
      <c:catAx><c:axId val="1"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/><c:crossAx val="2"/></c:catAx>
      <c:valAx><c:axId val="2"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="l"/><c:crossAx val="1"/></c:valAx>
    </c:plotArea>
    <c:plotVisOnly val="1"/>
  </c:chart>
</c:chartSpace>"#;

/// One body paragraph: "Before " + the inline chart drawing + " after".
fn chart_docx() -> Vec<u8> {
    let drawing = format!(
        r#"<w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0">
  <wp:extent cx="{CHART_CX_EMU}" cy="{CHART_CY_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:docPr id="2" name="Chart 1"/>
  <wp:cNvGraphicFramePr/>
  <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
    <c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="rId3"/>
  </a:graphicData></a:graphic>
</wp:inline></w:drawing></w:r>"#
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><w:body>
<w:p><w:r><w:t xml:space="preserve">Before </w:t></w:r>{drawing}<w:r><w:t xml:space="preserve"> after</w:t></w:r></w:p>
<w:sectPr>
  <w:pgSz w:w="12240" w:h="15840"/>
  <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
</w:sectPr>
</w:body></w:document>"#
    );

    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults>
    <w:rPrDefault><w:rPr>
      <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
      <w:sz w:val="22"/><w:szCs w:val="22"/>
    </w:rPr></w:rPrDefault>
  </w:docDefaults>
</w:styles>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
  <Override PartName="/word/charts/chart1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>
</Types>"#
        .to_string();

    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#
        .to_string();

    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="charts/chart1.xml"/>
</Relationships>"#
        .to_string();

    build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
        ("word/charts/chart1.xml", CHART_PART.to_string()),
    ])
}

// ─── harness plumbing (mirrors oracle_tests) ────────────────────────────────

fn load_test_fonts(viewer: &mut Viewer) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
    for (file, weight, italic) in [
        ("Arimo-Regular.ttf", 400u16, false),
        ("Arimo-Bold.ttf", 700, false),
        ("Arimo-Italic.ttf", 400, true),
        ("Arimo-BoldItalic.ttf", 700, true),
    ] {
        let path = format!("{dir}/{file}");
        let bytes = std::fs::read(&path).unwrap_or_else(|_| {
            panic!("test font missing: {path}\n  run: ./scripts/fetch-test-fonts.sh")
        });
        if viewer.load_font("Arimo", bytes, weight, italic).is_err() {
            panic!("load_font failed for {file}");
        }
    }
    // Same OMML math face as the oracle harness (see
    // oracle_tests::load_test_fonts). Hard-fail on missing.
    let xits = format!("{dir}/XITSMath-Regular.otf");
    let bytes = std::fs::read(&xits).unwrap_or_else(|_| {
        panic!("test font missing: {xits}\n  run: ./scripts/fetch-test-fonts.sh")
    });
    if viewer.load_font("XITS Math", bytes, 400, false).is_err() {
        panic!("load_font failed for XITSMath-Regular.otf");
    }
}

fn layout_chart_doc() -> Viewer {
    let mut viewer = match Viewer::new() {
        Ok(v) => v,
        Err(_) => panic!("Viewer::new failed"),
    };
    load_test_fonts(&mut viewer);
    if viewer.load_docx(chart_docx()).is_err() {
        panic!("load_docx failed");
    }
    viewer.run_layout_native().expect("run_layout_native");
    viewer
}

const CHART_KEY: &str = "chart::rId3";

// ─── tests ───────────────────────────────────────────────────────────────────

// Pin 1 — the chart part lands under `chart::rId3` and `chart::copy_data` reads
// the EXACT series/categories the painter would paint (the TSV source model).
#[test]
fn chart_xml_stored_and_copy_data_reads_painted_model() {
    let viewer = layout_chart_doc();
    let charts = viewer.chart_xml.borrow();
    let xml = charts.get(CHART_KEY).expect("chart XML stored under chart::rId3");
    let data = crate::chart::copy_data(xml).expect("copy_data parses the chart part");
    assert_eq!(data.categories, vec!["Q1", "Q2", "Q3"]);
    assert_eq!(data.series.len(), 2, "both series survive extraction");
    assert_eq!(data.series[0].0, "Alpha");
    assert_eq!(data.series[0].1, vec![10.0, 24.0, 17.0]);
    assert_eq!(data.series[1].0, "Beta");
    assert_eq!(data.series[1].1, vec![4.5, 6.0, 7.25]);
}

// Pin 3 — the chart's painted box outlines (click-to-select) and carries the
// declared 430×216 display px (the `chartData` wPx/hPx source).
#[test]
fn chart_outline_rect_is_the_painted_box() {
    let viewer = layout_chart_doc();
    let pages = viewer.pages.borrow();
    let rects = object_outline_rects(&pages[0], CHART_KEY);
    assert_eq!(rects.len(), 1, "one inline placement outlines once");
    let r = rects[0];
    assert!((r.w - CHART_W_PX).abs() < 0.5, "outline w ≈ 430px, got {}", r.w);
    assert!((r.h - CHART_H_PX).abs() < 0.5, "outline h ≈ 216px, got {}", r.h);
}

// Pin 2 — hit-testing inside the chart box yields kind "chart" under the
// lib.rs classifier pair (charts EXCLUDED from is_image, ADMITTED by is_chart);
// a point outside every object yields None.
#[test]
fn chart_hit_has_distinct_kind_and_stays_off_the_image_path() {
    let viewer = layout_chart_doc();
    let pages = viewer.pages.borrow();
    let image_rels = viewer.image_rel_ids.borrow();
    let charts = viewer.chart_xml.borrow();
    // The exact closures `hit_test_object` (lib.rs) builds.
    let is_image = |rel: &str| image_rels.contains(rel) && !charts.contains_key(rel);
    let is_chart = |rel: &str| charts.contains_key(rel);
    assert!(!is_image(CHART_KEY), "the chart key must NOT classify as an image");

    let r = object_outline_rects(&pages[0], CHART_KEY)[0];
    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    let hit = hit_test_object_impl(&pages[0], cx, cy, &is_image, &is_chart)
        .expect("chart box center hits");
    assert_eq!(hit.kind, "chart", "distinct kind for the chart object");
    assert_eq!(hit.rel_id, CHART_KEY);
    assert!((hit.rect.w - CHART_W_PX).abs() < 0.5 && (hit.rect.h - CHART_H_PX).abs() < 0.5);

    // Outside every box (page top-left margin corner) → no object hit.
    assert!(hit_test_object_impl(&pages[0], 1.0, 1.0, &is_image, &is_chart).is_none());
}

// Pin 4 — the highlight-hug fix: selecting the whole chart paragraph emits the
// chart's OBJECT BOX as its own rect (430 × 216 exactly), and NO other rect
// covers the chart's x-span (pre-fix, the chart's interval merged into the
// line-band pass, painting one line-metrics band ACROSS the chart, visible as
// an offset square). Text segments keep their LINE band (which on the chart's
// own line is legitimately tall — by design the object box applies to the
// OBJECT segment only).
#[test]
fn selection_hugs_the_chart_object_box() {
    let viewer = layout_chart_doc();
    let pages = viewer.pages.borrow();
    let len = viewer.para_meta.borrow()[0].logical_text.len() as u32;
    let rects = selection_rects_impl(&pages[0], TextPos::new(0, 0), TextPos::new(0, len));
    let hugged: Vec<_> = rects
        .iter()
        .filter(|r| (r.w - CHART_W_PX).abs() < 0.5 && (r.h - CHART_H_PX).abs() < 0.5)
        .collect();
    assert_eq!(
        hugged.len(),
        1,
        "exactly one rect is the chart OBJECT BOX (430×216): {rects:?}"
    );
    let chart_rect = *hugged[0];
    for r in &rects {
        if (r.x - chart_rect.x).abs() < 1e-9 && (r.y - chart_rect.y).abs() < 1e-9 {
            continue; // the object box itself
        }
        // No text rect intrudes into the chart's x-span (they abut, never cover).
        let overlap =
            (r.x + r.w).min(chart_rect.x + chart_rect.w) - r.x.max(chart_rect.x);
        assert!(
            overlap < 0.5,
            "text rects must not cover the chart x-span (pre-fix merge): {r:?}"
        );
    }
    // And the object box matches the outline box exactly (same geometry source).
    let outline = object_outline_rects(&pages[0], CHART_KEY)[0];
    assert_eq!(chart_rect, outline, "hug rect == painted chart box");
}

// The chart line also LAYS OUT: sanity that the page holds exactly one body
// line and that line carries exactly one chart item — guards against the chart
// item silently dropping out of the flow.
#[test]
fn chart_line_carries_the_chart_item() {
    let viewer = layout_chart_doc();
    let pages = viewer.pages.borrow();
    let mut lines = 0usize;
    let mut chart_items = 0usize;
    for el in &pages[0].elements {
        if let PageElement::Line(l) = el {
            lines += 1;
            for seg in &l.segments {
                for it in &seg.items {
                    if it.image.as_ref().map_or(false, |i| i.rel_id == CHART_KEY) {
                        chart_items += 1;
                    }
                }
            }
        }
    }
    assert_eq!(lines, 1, "everything fits one body line");
    assert_eq!(chart_items, 1, "the inline chart item is in the flow");
}
