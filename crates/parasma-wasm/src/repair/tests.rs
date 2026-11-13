//! Tests for the consolidated parser-repair module.
//!
//! RULE 1 (sml col) is also covered by the pre-existing `sml::tests`
//! (`repair_col_attrs_injects_only_missing` + the tolerant-load warnings tests),
//! which still call `shim::repair_col_attrs` (now a delegating wrapper) — those
//! prove the migration kept behavior identical. The tests here add direct
//! coverage of the repair fns + the WML/PML tolerant-load dispatch.

use std::io::{Cursor, Write};

// ─── shared zip helpers ──────────────────────────────────────────────────────

/// Build a deterministic stored-zip from named parts (the oracle docx builder
/// shape, local so the repair tests are self-contained).
fn build_zip(parts: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, content) in parts {
        zw.start_file(*name, opts).expect("start_file");
        zw.write_all(content).expect("write");
    }
    zw.finish().expect("finish").into_inner()
}

fn read_zip_part(bytes: &[u8], name: &str) -> Vec<u8> {
    use std::io::Read;
    let mut a = zip::ZipArchive::new(Cursor::new(bytes)).expect("open");
    let mut e = a.by_name(name).expect("by_name");
    let mut buf = Vec::new();
    e.read_to_end(&mut buf).expect("read");
    buf
}

fn zip_part_names(bytes: &[u8]) -> Vec<String> {
    let mut a = zip::ZipArchive::new(Cursor::new(bytes)).expect("open");
    (0..a.len())
        .map(|i| a.by_index(i).unwrap().name().unwrap().into_owned())
        .collect()
}

// ─── zip_rewrite ─────────────────────────────────────────────────────────────

/// A zip entry's declared size is author-controlled. Reserving it up front
/// turned a 200-byte package claiming a 2^62-byte entry into a process abort.
#[test]
fn a_lying_declared_size_does_not_drive_the_allocation() {
    let got = super::zip_rewrite::read_entry_bytes(&b"abc"[..], u64::MAX).expect("reads");
    assert_eq!(got, b"abc");
}

// ─── hostile-input termination ───────────────────────────────────────────────

/// A package that looks Strict (its root rels carry the Strict stem) but whose
/// only Strict URI has no family segment, so the downgrade maps nothing. The
/// tolerant loader used to rebuild it unchanged and recurse until the stack
/// overflowed. It must return the original error.
#[test]
fn an_unmappable_strict_uri_returns_the_error_instead_of_recursing() {
    // The stem must be the last thing in the part: any later `/` would be read
    // as the end of a family segment and the URI would be (wrongly) mapped.
    let rels = br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"></Relationships><!--http://purl.oclc.org/ooxml/-->"#;
    let pkg = build_zip(&[
        ("[Content_Types].xml", br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#.to_vec()),
        ("_rels/.rels", rels.to_vec()),
        ("word/document.xml", b"<w:document/>".to_vec()),
    ]);
    assert!(super::wml_strict::looks_strict(&pkg), "fixture must take the Strict branch");
    assert!(super::wml_strict::downgrade_strict_package(&pkg).is_none(), "nothing mappable => no rewrite");
    assert!(super::wml_pgmar::load_docx_tolerant(pkg).is_err());
}

/// An unterminated start tag ends the scan: the remainder is copied through
/// unchanged. Stepping one character and retrying re-read the whole tail for
/// every `<`, which made 400,000 unterminated tags take minutes; this input is
/// that size and has to come back promptly and byte-identical.
#[test]
fn unterminated_tags_are_copied_through_in_one_pass() {
    let n = 400_000;
    let pgmar = "<w:pgMar w:top=\"".repeat(n);
    assert_eq!(super::wml_pgmar::repair_pgmar_attrs(pgmar.as_bytes()), pgmar.as_bytes());
    let col = "<col min=\"".repeat(n);
    assert_eq!(super::sml_col::repair_col_attrs(col.as_bytes()), col.as_bytes());
    let hdr = "<w:headerReference w:type=\"".repeat(n);
    assert_eq!(super::wml_malformed::normalize_malformed_wml(hdr.as_bytes()), hdr.as_bytes());
    let mv = "<w:moveFromRangeStart w:id=\"".repeat(n);
    assert_eq!(super::wml_malformed::normalize_malformed_wml(mv.as_bytes()), mv.as_bytes());
}

/// A terminated tag before an unterminated one is still repaired.
#[test]
fn a_good_tag_before_an_unterminated_one_is_still_repaired() {
    let xml = br#"<w:pgMar w:top="1" w:right="1" w:bottom="1" w:left="1"/><w:pgMar w:top=""#;
    let out = String::from_utf8(super::wml_pgmar::repair_pgmar_attrs(xml)).unwrap();
    assert!(out.contains("w:gutter="), "first tag repaired: {out}");
    assert!(out.ends_with("<w:pgMar w:top=\""), "tail untouched: {out}");
}

#[test]
fn rebuild_zip_replaces_only_named_part_keeps_rest() {
    let original = build_zip(&[
        ("[Content_Types].xml", b"<types/>".to_vec()),
        ("word/document.xml", b"<old/>".to_vec()),
        ("word/media/image1.png", vec![0x89, b'P', b'N', b'G']),
    ]);
    let rebuilt = super::rebuild_zip_replacing_part(
        &original,
        &[("word/document.xml".to_string(), b"<new/>".to_vec())],
    )
    .expect("rebuild");

    // The named part is replaced; every other part is byte-preserved; entry set
    // (and order) is preserved.
    assert_eq!(read_zip_part(&rebuilt, "word/document.xml"), b"<new/>");
    assert_eq!(read_zip_part(&rebuilt, "[Content_Types].xml"), b"<types/>");
    assert_eq!(
        read_zip_part(&rebuilt, "word/media/image1.png"),
        vec![0x89, b'P', b'N', b'G']
    );
    assert_eq!(
        zip_part_names(&rebuilt),
        vec![
            "[Content_Types].xml",
            "word/document.xml",
            "word/media/image1.png"
        ]
    );
}

// ─── RULE 1: sml col ─────────────────────────────────────────────────────────

#[cfg(feature = "sml")]
#[test]
fn sml_col_attrs_injects_only_missing() {
    // min present, max missing → only @max injected, byte-preserving otherwise.
    let xml = br#"<worksheet><cols><col min="3" width="20"/></cols></worksheet>"#;
    let s = String::from_utf8(super::sml_col_attrs(xml)).unwrap();
    assert!(s.contains(r#"min="3""#));
    assert!(s.contains(r#"max="1""#));
    assert!(s.contains(r#"width="20""#));

    // Both present → idempotent.
    let xml2 = br#"<col min="1" max="2" width="5"/>"#;
    assert_eq!(super::sml_col_attrs(xml2), xml2.to_vec());

    // A non-col tag containing "min" is untouched.
    let xml3 = br#"<dimension ref="A1:B2"/>"#;
    assert_eq!(super::sml_col_attrs(xml3), xml3.to_vec());
}

// ─── RULE 2: wml pgMar ───────────────────────────────────────────────────────

#[test]
fn pgmar_injects_missing_gutter_as_zero() {
    // Gutter-less pgMar (a shape several generators emit) → gutter
    // injected as 0; the present attrs are preserved.
    let xml = br#"<w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720"/>"#;
    let s = String::from_utf8(super::wml_pgmar_attrs(xml)).unwrap();
    assert!(s.contains(r#"w:gutter="0""#), "gutter injected: {s}");
    assert!(s.contains(r#"w:top="1440""#), "existing attr preserved");
    // Only gutter was missing → exactly one injection.
    assert_eq!(s.matches("w:gutter=").count(), 1);
}

#[test]
fn pgmar_injects_all_missing_attrs() {
    // A pgMar with ONLY top → the other 6 required attrs all injected as 0.
    let xml = br#"<w:pgMar w:top="720"/>"#;
    let s = String::from_utf8(super::wml_pgmar_attrs(xml)).unwrap();
    for a in ["w:right", "w:bottom", "w:left", "w:header", "w:footer", "w:gutter"] {
        assert!(s.contains(&format!(r#"{a}="0""#)), "{a} injected: {s}");
    }
    assert!(s.contains(r#"w:top="720""#));
}

#[test]
fn pgmar_fully_specified_is_untouched() {
    let xml = br#"<w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>"#;
    assert_eq!(super::wml_pgmar_attrs(xml), xml.to_vec());
    // No pgMar at all → untouched.
    let none = br#"<w:body><w:p/></w:body>"#;
    assert_eq!(super::wml_pgmar_attrs(none), none.to_vec());
}

/// Build a minimal single-page docx whose final-section `<w:pgMar>` optionally
/// omits `w:gutter`. With `gutter=false` the file is the missing-gutter repro (schema-
/// invalid, strict parse fails). With `gutter=true` it is fully valid.
fn minimal_docx(gutter: bool) -> Vec<u8> {
    let g = if gutter { r#" w:gutter="0""# } else { "" };
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>Hello repair</w:t></w:r></w:p>
    <w:sectPr>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720"{g}/>
    </w:sectPr>
  </w:body>
</w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    build_zip(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
    ])
}

#[test]
fn gutterless_docx_fails_strict_but_loads_tolerant() {
    use ooxml_wml::Document;

    // Sanity: the gutter-less file genuinely fails the STRICT fork parse (the
    // missing-gutter repro) — if this ever stops failing, the fork became lenient and
    // RULE 2 can be retired.
    let bad = minimal_docx(false);
    assert!(
        Document::from_reader(Cursor::new(bad.clone())).is_err(),
        "gutter-less docx should fail the strict parse (the missing-gutter repro)"
    );

    // The tolerant loader recovers it (injects gutter=0, re-parses).
    let doc = super::load_docx_tolerant(bad).expect("tolerant load recovers gutter-less docx");
    // The recovered document has a body (the parse fully succeeded).
    assert!(doc.gen_doc().body.is_some());
}

#[test]
fn valid_docx_loads_unchanged_via_tolerant_path() {
    // A fully-valid docx parses on the FIRST attempt — the happy path is never
    // touched (the repair would be a no-op anyway, but it is never reached).
    let good = minimal_docx(true);
    let doc = super::load_docx_tolerant(good).expect("valid docx loads");
    assert!(doc.gen_doc().body.is_some());
}

// ─── RULE 3: pml custGeom ────────────────────────────────────────────────────

#[cfg(feature = "pml")]
#[test]
fn strip_custgeom_replaces_paired_span_with_rect() {
    let xml = br#"<p:sp><a:custGeom><a:pathLst><a:path><a:moveTo><a:pt x="1" y="2"/></a:moveTo></a:path></a:pathLst></a:custGeom></p:sp>"#;
    let s = String::from_utf8(super::pml_strip_custgeom(xml)).unwrap();
    assert!(!s.contains("custGeom"), "custGeom removed: {s}");
    assert!(!s.contains("moveTo"), "moveTo subtree removed: {s}");
    assert!(s.contains(r#"<a:prstGeom prst="rect"><a:avLst/></a:prstGeom>"#));
    // The surrounding element is preserved.
    assert!(s.starts_with("<p:sp>") && s.ends_with("</p:sp>"));
}

#[cfg(feature = "pml")]
#[test]
fn strip_custgeom_handles_multiple_and_self_closing() {
    let xml = br#"<root><a:custGeom/><x/><a:custGeom><a:pt x="0"/></a:custGeom></root>"#;
    let s = String::from_utf8(super::pml_strip_custgeom(xml)).unwrap();
    assert_eq!(s.matches(r#"prst="rect""#).count(), 2, "both stripped: {s}");
    assert!(!s.contains("custGeom"));
    assert!(s.contains("<x/>"), "interleaved element preserved");
}

#[cfg(feature = "pml")]
#[test]
fn strip_custgeom_no_custgeom_is_untouched() {
    let xml = br#"<p:sp><a:prstGeom prst="ellipse"/></p:sp>"#;
    assert_eq!(super::pml_strip_custgeom(xml), xml.to_vec());
}

// ─── RULE 4: OPC absolute rels Targets ───────────────────────────────────────

#[cfg(feature = "sml")]
mod opc_abs_target {
    //! RULE 4 RETIRED — absolute rels Targets are resolved by the PARSER now
    //! (ooxml-opc `resolve_part_path`, the fork durable fix this module's old
    //! sanity checks pointed at: "if this ever stops failing … RULE 4 can be
    //! retired"). These tests re-pin the two real-world shapes as STRAIGHT
    //! parses so a fork regression resurfaces here immediately.
    use super::build_zip;

    /// Minimal single-sheet xlsx whose rels use ABSOLUTE OPC targets when
    /// `abs_targets` (a shape some exporters write; legal per OPC Part 2 §9.3.1.1)
    /// and conventional relative targets otherwise.
    fn minimal_xlsx(abs_targets: bool) -> Vec<u8> {
        let (wb_target, sheet_target) = if abs_targets {
            ("/xl/workbook.xml", "/xl/worksheets/sheet1.xml")
        } else {
            ("xl/workbook.xml", "worksheets/sheet1.xml")
        };
        let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;
        let root_rels = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{wb_target}"/>
</Relationships>"#
        );
        let workbook = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;
        let workbook_rels = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="{sheet_target}"/>
</Relationships>"#
        );
        let sheet = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1"/>
  <sheetData><row r="1"><c r="A1"><v>42</v></c></row></sheetData>
</worksheet>"#;
        build_zip(&[
            ("[Content_Types].xml", content_types.as_bytes().to_vec()),
            ("_rels/.rels", root_rels.into_bytes()),
            ("xl/workbook.xml", workbook.as_bytes().to_vec()),
            ("xl/_rels/workbook.xml.rels", workbook_rels.into_bytes()),
            ("xl/worksheets/sheet1.xml", sheet.as_bytes().to_vec()),
        ])
    }

    /// Minimal single-sheet xlsx with a RELATIVE working root `_rels/.rels`
    /// (Target="xl/workbook.xml") but an ABSOLUTE nested
    /// `xl/_rels/workbook.xml.rels` sheet target (Target="/xl/worksheets/…") —
    /// a shape found in documents in the wild. The initial open SUCCEEDS (the root
    /// rels is fine); before the fork resolved absolute targets, the nested one
    /// failed LAZILY at `sheet_xml`, which `from_xlsx` swallowed into a dropped sheet.
    fn nested_abs_xlsx() -> Vec<u8> {
        let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;
        // RELATIVE working root rels — as such generators write it (initial open OK).
        let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
        let workbook = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;
        // ABSOLUTE nested sheet target — the defect (fails lazily at sheet_xml).
        let workbook_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="/xl/worksheets/sheet1.xml"/>
</Relationships>"#;
        let sheet = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1"/>
  <sheetData><row r="1"><c r="A1"><v>42</v></c></row></sheetData>
</worksheet>"#;
        build_zip(&[
            ("[Content_Types].xml", content_types.as_bytes().to_vec()),
            ("_rels/.rels", root_rels.as_bytes().to_vec()),
            ("xl/workbook.xml", workbook.as_bytes().to_vec()),
            ("xl/_rels/workbook.xml.rels", workbook_rels.as_bytes().to_vec()),
            ("xl/worksheets/sheet1.xml", sheet.as_bytes().to_vec()),
        ])
    }

    /// Fully-absolute shape: root `_rels/.rels` + nested rels both ABSOLUTE.
    /// Straight strict parse now succeeds end-to-end (no repair involved).
    #[test]
    fn abs_target_xlsx_parses_strict() {
        use std::io::Cursor;
        let bytes = minimal_xlsx(true);
        let mut wb = ooxml_sml::workbook::Workbook::from_reader(Cursor::new(bytes))
            .expect("absolute-target xlsx parses strict (fork resolve_part_path)");
        assert_eq!(wb.sheet_count(), 1);
        assert!(wb.sheet_xml(0).is_ok(), "sheet part resolves");
    }

    /// Mixed shape: RELATIVE root rels, ABSOLUTE nested sheet target — the
    /// case that used to fail LAZILY at sheet_xml and silently drop every sheet.
    #[test]
    fn nested_only_abs_target_parses_strict() {
        use std::io::Cursor;
        let bytes = nested_abs_xlsx();
        let mut wb = ooxml_sml::workbook::Workbook::from_reader(Cursor::new(bytes))
            .expect("initial open succeeds");
        assert_eq!(wb.sheet_count(), 1);
        assert!(wb.sheet_xml(0).is_ok(), "nested-absolute sheet part resolves lazily");
    }
}

mod wml_malformed {
    //! RULE 5 unit pins: each normalizer fixes its observed corpus shape,
    //! leaves valid input BYTE-IDENTICAL, and is idempotent.
    use crate::repair::wml_malformed::normalize_malformed_wml;

    fn norm(s: &str) -> String {
        String::from_utf8(normalize_malformed_wml(s.as_bytes())).unwrap()
    }

    #[test]
    fn move_bookmark_missing_date_gets_placeholder() {
        // tdf104797 shape: moveFromRangeStart without the required w:date.
        let got = norm(r#"<w:moveFromRangeStart w:id="2" w:author="A" w:name="m1"/>"#);
        assert!(got.contains(r#"w:date="1970-01-01T00:00:00Z""#), "{got}");
        assert!(got.ends_with("/>"), "self-closing preserved: {got}");
        // With a date present: byte-identical.
        let ok = r#"<w:moveToRangeStart w:id="1" w:author="A" w:date="2020-01-01T00:00:00Z" w:name="m"/>"#;
        assert_eq!(norm(ok), ok);
        // Range ENDS carry no date and must never be touched.
        let end = r#"<w:moveFromRangeEnd w:id="2"/>"#;
        assert_eq!(norm(end), end);
    }

    #[test]
    fn empty_pprchange_gains_required_ppr() {
        // UnknownStyleInRedline shape: open-empty AND a complete record.
        let got = norm(r#"<w:pPrChange w:id="0" w:author="K"></w:pPrChange>"#);
        assert_eq!(got, r#"<w:pPrChange w:id="0" w:author="K"><w:pPr/></w:pPrChange>"#);
        let got = norm(r#"<w:pPrChange w:id="0" w:author="K"/>"#);
        assert_eq!(got, r#"<w:pPrChange w:id="0" w:author="K"><w:pPr/></w:pPrChange>"#);
        // A complete pPrChange is byte-identical.
        let ok = r#"<w:pPrChange w:id="0" w:author="K"><w:pPr><w:pStyle w:val="X"/></w:pPr></w:pPrChange>"#;
        assert_eq!(norm(ok), ok);
    }

    #[test]
    fn hdrftr_type_odd_becomes_default() {
        // 60293 shape; "even" stays untouched.
        let got = norm(r#"<w:headerReference w:type="odd" r:id="rId7"/><w:footerReference w:type="even" r:id="rId8"/>"#);
        assert!(got.contains(r#"<w:headerReference w:type="default" r:id="rId7"/>"#), "{got}");
        assert!(got.contains(r#"w:type="even""#), "{got}");
        // "odd" text OUTSIDE those tags is never rewritten.
        let ok = r#"<w:t>odd</w:t>"#;
        assert_eq!(norm(ok), ok);
    }

    #[test]
    fn table_missing_children_injected_in_order() {
        // fdo53985/tdf131203 shape: tbl straight into rows.
        let got = norm(r#"<w:tbl><w:tr><w:tc/></w:tr></w:tbl>"#);
        assert_eq!(got, r#"<w:tbl><w:tblPr/><w:tblGrid/><w:tr><w:tc/></w:tr></w:tbl>"#);
        // tblPr present, tblGrid missing → only the grid, AFTER tblPr.
        let got = norm(r#"<w:tbl><w:tblPr><w:tblW w:w="0" w:type="auto"/></w:tblPr><w:tr/></w:tbl>"#);
        assert_eq!(
            got,
            r#"<w:tbl><w:tblPr><w:tblW w:w="0" w:type="auto"/></w:tblPr><w:tblGrid/><w:tr/></w:tbl>"#
        );
        // A complete table is byte-identical; NESTED complete table too.
        let ok = r#"<w:tbl><w:tblPr/><w:tblGrid><w:gridCol w:w="1"/></w:tblGrid><w:tr><w:tc><w:tbl><w:tblPr/><w:tblGrid/><w:tr/></w:tbl></w:tc></w:tr></w:tbl>"#;
        assert_eq!(norm(ok), ok);
    }

    #[test]
    fn idempotent_over_all_passes() {
        let broken = r#"<w:tbl><w:tr/></w:tbl><w:pPrChange w:id="0"/><w:moveFromRangeStart w:id="1" w:author="A"/><w:headerReference w:type="odd"/>"#;
        let once = norm(broken);
        assert_eq!(norm(&once), once, "second pass is a no-op");
    }
}

mod wml_strict_e2e {
    //! RULE 6 end-to-end pins: a minimal ISO Strict package loads through
    //! load_docx_tolerant (namespace downgrade), including CHAINED with a
    //! RULE-5 deficiency (the depth-one recursion), and a transitional file
    //! never triggers the downgrade path.
    use super::build_zip;
    use crate::repair::load_docx_tolerant;

    const STRICT_CT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;
    const STRICT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;

    fn strict_docx(body: &str) -> Vec<u8> {
        let doc = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://purl.oclc.org/ooxml/wordprocessingml/main"><w:body>{body}</w:body></w:document>"#
        );
        build_zip(&[
            ("[Content_Types].xml", STRICT_CT.as_bytes().to_vec()),
            ("_rels/.rels", STRICT_RELS.as_bytes().to_vec()),
            ("word/document.xml", doc.into_bytes()),
        ])
    }

    #[test]
    fn strict_package_loads_via_downgrade() {
        let bytes = strict_docx("<w:p><w:r><w:t>Strict body</w:t></w:r></w:p>");
        assert!(
            load_docx_tolerant(bytes).is_ok(),
            "a valid ISO Strict package must load through the RULE-6 downgrade"
        );
    }

    #[test]
    fn strict_chained_with_rule5_deficiency_loads() {
        // Strict AND a table missing tblPr/tblGrid: the downgraded bytes
        // re-enter the pipeline once and RULE 5 finishes the job.
        let bytes = strict_docx("<w:tbl><w:tr><w:tc><w:p/></w:tc></w:tr></w:tbl><w:p/>");
        assert!(
            load_docx_tolerant(bytes).is_ok(),
            "strict + malformed table must load (RULE 6 then RULE 5)"
        );
    }

    #[test]
    fn genuinely_missing_relationship_stays_an_error() {
        // No officeDocument relationship AT ALL (and nothing Strict): the
        // original error must surface unchanged — never masked.
        let empty_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"></Relationships>"#;
        let bytes = build_zip(&[
            ("[Content_Types].xml", STRICT_CT.as_bytes().to_vec()),
            ("_rels/.rels", empty_rels.as_bytes().to_vec()),
            ("word/document.xml", b"<w:document/>".to_vec()),
        ]);
        assert!(load_docx_tolerant(bytes).is_err());
    }
}

mod wml_nesting {
    //! RULE 7 pins: within-cap byte-identity, beyond-cap truncation with the
    //! marker, idempotence, and the end-to-end deep-docx load.
    use super::build_zip;
    use crate::repair::wml_nesting::{cap_table_nesting, MAX_TBL_DEPTH};

    fn nested_tbl(depth: usize, innermost: &str) -> String {
        let mut s = String::new();
        for _ in 0..depth {
            s.push_str("<w:tbl><w:tblPr/><w:tblGrid/><w:tr><w:tc>");
        }
        s.push_str(innermost);
        for _ in 0..depth {
            s.push_str("</w:tc></w:tr></w:tbl>");
        }
        s
    }

    #[test]
    fn within_cap_is_byte_identical() {
        let doc = nested_tbl(MAX_TBL_DEPTH, "<w:p/>");
        assert_eq!(cap_table_nesting(&doc), doc);
        let shallow = nested_tbl(3, "<w:p/>");
        assert_eq!(cap_table_nesting(&shallow), shallow);
    }

    #[test]
    fn beyond_cap_truncates_with_marker() {
        let doc = nested_tbl(MAX_TBL_DEPTH + 5, "<w:p><w:r><w:t>deep</w:t></w:r></w:p>");
        let capped = cap_table_nesting(&doc);
        assert!(capped.contains("[nested table truncated]"), "marker present");
        assert!(!capped.contains("deep"), "beyond-cap content gone");
        // Depth of the result is exactly the cap.
        let mut depth = 0usize;
        let mut max = 0usize;
        let mut rest = capped.as_str();
        loop {
            let o = rest.find("<w:tbl>");
            let c = rest.find("</w:tbl>");
            match (o, c) {
                (Some(o), Some(c)) if o < c => { depth += 1; max = max.max(depth); rest = &rest[o + 7..]; }
                (_, Some(c)) => { depth -= 1; rest = &rest[c + 8..]; }
                _ => break,
            }
        }
        assert_eq!(max, MAX_TBL_DEPTH);
        // Idempotent.
        assert_eq!(cap_table_nesting(&capped), capped);
    }

    #[test]
    fn deep_docx_loads_end_to_end() {
        let body = nested_tbl(MAX_TBL_DEPTH + 20, "<w:p/>");
        let doc = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}<w:p/></w:body></w:document>"#
        );
        let ct = r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;
        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let bytes = build_zip(&[
            ("[Content_Types].xml", ct.as_bytes().to_vec()),
            ("_rels/.rels", rels.as_bytes().to_vec()),
            ("word/document.xml", doc.into_bytes()),
        ]);
        assert!(
            crate::repair::load_docx_tolerant(bytes).is_ok(),
            "a beyond-cap nested docx must load via RULE 7 truncation"
        );
    }
}
