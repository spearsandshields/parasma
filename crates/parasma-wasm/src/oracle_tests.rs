//! Native layout-regression harness.
//!
//! Drives the FULL pagination pipeline natively — `Viewer::new` → `load_font`
//! (sha-pinned fonts from `test-fonts/`, fetched by
//! `scripts/fetch-test-fonts.sh`) → `load_docx` → `run_layout_native` — over
//! synthetic oracle documents built in-test (no binary fixtures in git; the
//! doc bytes are reproducible from this source).
//!
//! Five assertions per oracle, deliberately split across THREE ORTHOGONAL
//! streams so that none can mask another:
//!  1. **Page count** — coarse pagination guard.
//!  2. **LINE stream** (`geometry_dump` → FNV-1a) — every line's
//!     baseline/extents/height and every segment's x/item-count, rounded to
//!     0.01px. Catches single-line sub-pixel drift (the `luctus` class of bug)
//!     that page counts miss. Answers "where does everything SIT".
//!  3. **Collateral counters** (`LayoutStats`) — the only guard that can catch a
//!     re-lay which CONVERGES to identical output. No dump, however strong, can
//!     express this, so it is asserted on every pin.
//!  4. **ITEM stream** (`content_dumps().0` → `{name}.items.txt`) — text, item
//!     order, the full `RunStyle`, `char_start`/`char_len`, links, bookmarks,
//!     comment ids, bidi parity and every payload. COORDINATE-FREE by
//!     construction. Answers "what IS everything".
//!  5. **GLYPH stream** (`content_dumps().1` → `{name}.glyphs.txt`) — per
//!     shaped run, the interned per-glyph gid / advance / positioning offset /
//!     cluster vector. (4) pins a run's glyph COUNT and TOTAL advance; this pins
//!     the distribution INSIDE it. Answers "which glyphs, where in the run".
//!
//! (2), (4) and (5) are pinned separately rather than merged into one wider
//! hash. That is what let (4), and later (5), be added with NO re-baseline of
//! what came before — every historical `geometry_dump` constant keeps its value
//! and goes on asserting, on every run, that nothing moved. It also makes a
//! failure self-diagnosing: LINE red with ITEM green means a pure repositioning;
//! ITEM red with LINE green means the content changed where it stood; GLYPH red
//! with both green means a run was redistributed, not moved or resized.
//!
//! On mismatch: `ORACLE_DUMP=1` prints the LINE dump for diffing. Re-baseline
//! ONLY after confirming the change is intentional — and note that the ITEM and
//! GLYPH streams have no paste-a-constant path at all. Re-recording is
//! per-fixture and by name (`ORACLE_BLESS=oracle-flow`, which covers BOTH of
//! that fixture's goldens); the resulting `git diff` is the review artifact. See
//! `assert_golden` for why that asymmetry exists.

use super::wml::page::{GlyphStream, PageElement, PageLayout, PagedLine, PagedTableCell, PagedTableRow, PositionedTextBox};
use super::{LayoutStats, Viewer};
use std::io::Write;

// ─── docx builder ────────────────────────────────────────────────────────────

/// Zip the given parts into a .docx byte vector (stored, deterministic).
/// Generic over the part payload (String for XML parts, Vec<u8> for binary
/// media) — produced bytes are identical either way, so the oracle hashes are
/// unaffected.
///
/// `pub(crate)` for `perf_tests.rs`, which builds its synthetic timing document
/// the same way — a second copy of the zip plumbing would be one rule spelled
/// twice.
pub(crate) fn build_docx<P: AsRef<[u8]>>(parts: &[(&str, P)]) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    for (name, content) in parts {
        zw.start_file(*name, opts).expect("zip start_file");
        zw.write_all(content.as_ref()).expect("zip write");
    }
    zw.finish().expect("zip finish").into_inner()
}

const W_NS: &str =
    r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
const R_NS: &str =
    r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

fn p(ppr: &str, runs: &str) -> String {
    let ppr = if ppr.is_empty() { String::new() } else { format!("<w:pPr>{ppr}</w:pPr>") };
    format!("<w:p>{ppr}{runs}</w:p>")
}

fn r(rpr: &str, text: &str) -> String {
    let rpr = if rpr.is_empty() { String::new() } else { format!("<w:rPr>{rpr}</w:rPr>") };
    format!(r#"<w:r>{rpr}<w:t xml:space="preserve">{text}</w:t></w:r>"#)
}

/// Deterministic self-authored filler — cycles five sentence shapes so
/// paragraph lengths (and thus line counts) vary predictably.
fn filler(i: usize, sentences: usize) -> String {
    const S: [&str; 5] = [
        "The layout engine measures every glyph before a single page exists. ",
        "Paragraphs flow across the column and break where the measurements say they must, never a pixel early. ",
        "A table that straddles a page boundary keeps its rows whole while the remainder carries to the next page. ",
        "Numbered items advance their counters even when interrupted by plain text between the runs of a list. ",
        "Headers and footers occupy their reserved bands and the body never intrudes into either of them. ",
    ];
    (0..sentences).map(|k| S[(i + k) % S.len()]).collect()
}

// ─── deterministic PNG generator ─────────────────────────────────────────────
// A structurally valid RGB PNG (8-bit, color type 2) so the oracle-objects doc
// also opens visually in a viewer. Layout only needs the drawing's
// declared <wp:extent>, but a real PNG keeps manual checks honest. No deps:
// IHDR + a single IDAT carrying one zlib stream of stored (uncompressed)
// deflate blocks, ending with adler32; every chunk CRC-32'd; IEND.

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    crc ^ 0xffff_ffff
}

fn adler32(bytes: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in bytes {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc_input);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// Build a `w`×`h` RGB PNG whose pixels are a deterministic gradient.
fn make_png(w: u32, h: u32) -> Vec<u8> {
    // Raw scanlines: each row prefixed with filter byte 0 (None), then RGB.
    let mut raw = Vec::with_capacity((h * (1 + w * 3)) as usize);
    for y in 0..h {
        raw.push(0u8);
        for x in 0..w {
            raw.push((x % 256) as u8);
            raw.push((y % 256) as u8);
            raw.push(((x + y) % 256) as u8);
        }
    }
    // zlib wrapper: CMF=0x78, FLG=0x01, then stored deflate blocks, then adler32.
    let mut zlib = vec![0x78u8, 0x01];
    let chunks = raw.chunks(0xffff);
    let n = chunks.len();
    for (i, block) in raw.chunks(0xffff).enumerate() {
        let bfinal = if i + 1 == n { 1u8 } else { 0u8 };
        zlib.push(bfinal); // BTYPE=00 (stored) in the low bits, BFINAL on/off
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // bit depth 8, color type 2 (RGB)
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &zlib);
    png_chunk(&mut png, b"IEND", &[]);
    png
}

// ─── oracle document: "flow" ────────────────────────────────────────────────
// Multi-page body of plain/justified/indented/spaced paragraphs, a numbered
// and a bulleted list, a 36-row table straddling pages, an explicit page
// break, and a header + footer with a live PAGE field. All text Arimo 11pt
// via docDefaults so the harness needs exactly one font family.

fn oracle_flow_docx() -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults>
    <w:rPrDefault><w:rPr>
      <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
      <w:sz w:val="22"/><w:szCs w:val="22"/>
    </w:rPr></w:rPrDefault>
    <w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault>
  </w:docDefaults>
  <w:style w:type="paragraph" w:styleId="Heading1">
    <w:name w:val="heading 1"/>
    <w:pPr><w:spacing w:before="240" w:after="120"/></w:pPr>
    <w:rPr><w:b/><w:sz w:val="32"/></w:rPr>
  </w:style>
</w:styles>"#
    );

    let numbering = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering {W_NS}>
  <w:abstractNum w:abstractNumId="0">
    <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/>
      <w:lvlText w:val="%1."/><w:lvlJc w:val="left"/>
      <w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr></w:lvl>
  </w:abstractNum>
  <w:abstractNum w:abstractNumId="1">
    <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="bullet"/>
      <w:lvlText w:val="&#8226;"/><w:lvlJc w:val="left"/>
      <w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr></w:lvl>
  </w:abstractNum>
  <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
  <w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>
</w:numbering>"#
    );

    let header = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:hdr {W_NS}><w:p>
  <w:pPr><w:jc w:val="right"/></w:pPr>
  <w:r><w:rPr><w:sz w:val="18"/><w:color w:val="666666"/></w:rPr>
    <w:t>Parasma Layout Oracle</w:t></w:r>
</w:p></w:hdr>"#
    );

    let footer = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:ftr {W_NS}><w:p>
  <w:pPr><w:jc w:val="center"/></w:pPr>
  <w:r><w:rPr><w:sz w:val="18"/></w:rPr><w:t xml:space="preserve">Page </w:t></w:r>
  <w:r><w:fldChar w:fldCharType="begin"/></w:r>
  <w:r><w:instrText xml:space="preserve"> PAGE </w:instrText></w:r>
  <w:r><w:fldChar w:fldCharType="separate"/></w:r>
  <w:r><w:rPr><w:sz w:val="18"/></w:rPr><w:t>1</w:t></w:r>
  <w:r><w:fldChar w:fldCharType="end"/></w:r>
</w:p></w:ftr>"#
    );

    let mut body = String::new();
    body.push_str(&p(
        r#"<w:pStyle w:val="Heading1"/><w:jc w:val="center"/>"#,
        &r("", "Layout Oracle One: Flowing Text"),
    ));
    for i in 0..10 {
        body.push_str(&p("", &r("", &filler(i, 3))));
    }
    for i in 0..3 {
        body.push_str(&p(r#"<w:jc w:val="both"/>"#, &r("", &filler(i + 1, 4))));
    }
    for i in 0..3 {
        body.push_str(&p(
            r#"<w:ind w:left="720"/><w:spacing w:line="360" w:lineRule="auto"/>"#,
            &r(r#"<w:i/>"#, &filler(i + 2, 3)),
        ));
    }
    for i in 0..8 {
        body.push_str(&p(
            r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr>"#,
            &r("", &filler(i, 1)),
        ));
    }
    for i in 0..5 {
        body.push_str(&p(
            r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="2"/></w:numPr>"#,
            &r("", &filler(i + 3, 1)),
        ));
    }
    // 36-row × 3-col bordered table — straddles at least one page boundary.
    body.push_str(r#"<w:tbl><w:tblPr><w:tblW w:w="9360" w:type="dxa"/><w:tblBorders><w:top w:val="single" w:sz="4"/><w:left w:val="single" w:sz="4"/><w:bottom w:val="single" w:sz="4"/><w:right w:val="single" w:sz="4"/><w:insideH w:val="single" w:sz="4"/><w:insideV w:val="single" w:sz="4"/></w:tblBorders></w:tblPr><w:tblGrid><w:gridCol w:w="3120"/><w:gridCol w:w="3120"/><w:gridCol w:w="3120"/></w:tblGrid>"#);
    for row in 0..36 {
        body.push_str("<w:tr>");
        for col in 0..3 {
            body.push_str(&format!(
                r#"<w:tc><w:tcPr><w:tcW w:w="3120" w:type="dxa"/></w:tcPr>{}</w:tc>"#,
                p("", &r("", &format!("Row {row} cell {col}: metric {}", row * 3 + col)))
            ));
        }
        body.push_str("</w:tr>");
    }
    body.push_str("</w:tbl>");
    // Explicit page break, then a second flow section.
    body.push_str(&p(
        r#"<w:pStyle w:val="Heading1"/><w:pageBreakBefore/>"#,
        &r("", "Part Two: After the Break"),
    ));
    for i in 0..10 {
        body.push_str(&p("", &r("", &filler(i + 4, 3))));
    }
    body.push_str(
        r#"<w:sectPr>
      <w:headerReference w:type="default" r:id="rId2"/>
      <w:footerReference w:type="default" r:id="rId3"/>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
    </w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
  <Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>
  <Override PartName="/word/header1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"/>
  <Override PartName="/word/footer1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"/>
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
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/>
  <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>
</Relationships>"#
        .to_string();

    build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
        ("word/numbering.xml", numbering),
        ("word/header1.xml", header),
        ("word/footer1.xml", footer),
    ])
}

// ─── oracle document: "objects" ──────────────────────────────────────────────
// Guards the paths oracle-flow does NOT touch: a wp:anchor'd floating image with
// wrapSquare text wrap, a continuous section break into a 2-column section that
// fills both columns with a separator line, a return to a 1-column section, and
// footnote references resolved from footnotes.xml. Targets 3-5 pages.

fn oracle_objects_docx() -> Vec<u8> {
    // Image extent: 2in × 2in = 1828800 EMU (914400 EMU/in).
    const IMG_EMU: i64 = 1_828_800;

    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults>
    <w:rPrDefault><w:rPr>
      <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
      <w:sz w:val="22"/><w:szCs w:val="22"/>
    </w:rPr></w:rPrDefault>
    <w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault>
  </w:docDefaults>
  <w:style w:type="paragraph" w:styleId="Heading1">
    <w:name w:val="heading 1"/>
    <w:pPr><w:spacing w:before="240" w:after="120"/></w:pPr>
    <w:rPr><w:b/><w:sz w:val="32"/></w:rPr>
  </w:style>
</w:styles>"#
    );

    // footnotes.xml — separator/continuation sentinels + three real notes.
    let footnotes = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:footnotes {W_NS} {R_NS}>
  <w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>
  <w:footnote w:type="continuationSeparator" w:id="0"><w:p><w:r><w:continuationSeparator/></w:r></w:p></w:footnote>
  <w:footnote w:id="1"><w:p><w:r><w:t xml:space="preserve">First note: glyph metrics are measured before pagination begins.</w:t></w:r></w:p></w:footnote>
  <w:footnote w:id="2"><w:p><w:r><w:t xml:space="preserve">Second note: the column separator is drawn at the gutter midpoint.</w:t></w:r></w:p></w:footnote>
  <w:footnote w:id="3"><w:p><w:r><w:t xml:space="preserve">Third note: a floating image excludes a rectangle the text flows around.</w:t></w:r></w:p></w:footnote>
</w:footnotes>"#
    );

    // The floating image anchor (wrapSquare, fixed posOffset, ~2x2in).
    // posOffset 0 horizontally (column-relative) / 0 vertically (paragraph).
    let anchor = format!(
        r#"<w:r><w:drawing><wp:anchor distT="114300" distB="114300" distL="114300" distR="114300" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="0">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV>
  <wp:extent cx="{IMG_EMU}" cy="{IMG_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapSquare wrapText="right"/>
  <wp:docPr id="1" name="FloatImg"/>
  <wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect="1"/></wp:cNvGraphicFramePr>
  <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">
    <pic:pic><pic:nvPicPr><pic:cNvPr id="1" name="FloatImg"/><pic:cNvPicPr/></pic:nvPicPr>
    <pic:blipFill><a:blip r:embed="rId10"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>
    <pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{IMG_EMU}" cy="{IMG_EMU}"/></a:xfrm>
    <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );

    // sectPr for the 2-column middle section (continuous start, balanced cols
    // with a visible separator). Carried in the LAST paragraph of section one
    // via <w:pPr><w:sectPr>…; that paragraph's sectPr defines the section that
    // ENDS at it (i.e. section one — single column), so the 2-column properties
    // go on the body-level sectPr? No: in WML, a paragraph-embedded sectPr
    // describes the section the paragraph terminates. The 2-column section is
    // bounded by the marker paragraph below and the final body sectPr. So the
    // 2-column props live on the SECOND marker (the one closing the 2-col run).
    let single_col_sect = r#"<w:pPr><w:sectPr>
      <w:type w:val="continuous"/>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
    </w:sectPr></w:pPr>"#;
    let two_col_sect = r#"<w:pPr><w:sectPr>
      <w:type w:val="continuous"/>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
      <w:cols w:num="2" w:space="720" w:sep="1"/>
    </w:sectPr></w:pPr>"#;

    let mut body = String::new();

    // ── Section one (single column): heading + paragraphs wrapping the float ──
    body.push_str(&p(
        r#"<w:pStyle w:val="Heading1"/>"#,
        &r("", "Layout Oracle Two: Floats, Columns, Footnotes"),
    ));
    // Paragraph that hosts the anchor + flows text around it (with a footnote).
    body.push_str(&format!(
        "<w:p>{anchor}{}{}</w:p>",
        r("", &filler(0, 6)),
        r#"<w:r><w:footnoteReference w:id="1"/></w:r>"#,
    ));
    for i in 0..3 {
        body.push_str(&p("", &r("", &filler(i + 1, 4))));
    }
    // Close section one — single column.
    body.push_str(&format!("<w:p>{single_col_sect}</w:p>"));

    // ── Section two (two columns): enough text to fill both columns + notes ──
    body.push_str(&p(
        r#"<w:pStyle w:val="Heading1"/>"#,
        &r("", "Two-Column Section"),
    ));
    for i in 0..34 {
        let runs = if i == 4 {
            format!("{}{}", r("", &filler(i, 3)), r#"<w:r><w:footnoteReference w:id="2"/></w:r>"#)
        } else {
            r("", &filler(i, 3))
        };
        body.push_str(&format!("<w:p>{runs}</w:p>"));
    }
    // Close section two — two columns. (sectPr here describes the 2-col section.)
    body.push_str(&format!("<w:p>{two_col_sect}</w:p>"));

    // ── Section three (back to single column): more text + a third footnote ──
    body.push_str(&p(
        r#"<w:pStyle w:val="Heading1"/>"#,
        &r("", "Back to One Column"),
    ));
    for i in 0..12 {
        let runs = if i == 2 {
            format!("{}{}", r("", &filler(i + 2, 3)), r#"<w:r><w:footnoteReference w:id="3"/></w:r>"#)
        } else {
            r("", &filler(i + 2, 3))
        };
        body.push_str(&format!("<w:p>{runs}</w:p>"));
    }
    // Final body sectPr — single column (closes section three).
    body.push_str(
        r#"<w:sectPr>
      <w:type w:val="continuous"/>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
    </w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"><w:body>{body}</w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
  <Override PartName="/word/footnotes.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml"/>
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
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/>
  <Relationship Id="rId10" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/oracle.png"/>
</Relationships>"#
        .to_string();

    let png = make_png(64, 64);

    build_docx(&[
        ("[Content_Types].xml", content_types.into_bytes()),
        ("_rels/.rels", root_rels.into_bytes()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.into_bytes()),
        ("word/styles.xml", styles.into_bytes()),
        ("word/footnotes.xml", footnotes.into_bytes()),
        ("word/media/oracle.png", png),
    ])
}

// ─── harness plumbing ───────────────────────────────────────────────────────

/// `pub(crate)` for `ground_truth.rs`, which must render calibration documents
/// on the SAME font basis the oracles use — a second copy of this ladder would
/// let the two drift, and the whole ground-truth question is which faces were
/// loaded.
pub(crate) fn load_test_fonts(viewer: &mut Viewer) {
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
    // OMML: the real OpenType math face (XITS Math, with its MATH
    // table). Hard-fail on missing — a font-absent load would silently re-arm the
    // body-font proxies and the pinned math geometry would NOT reflect ship. CI
    // must run ./scripts/fetch-test-fonts.sh first.
    let xits = format!("{dir}/XITSMath-Regular.otf");
    let bytes = std::fs::read(&xits).unwrap_or_else(|_| {
        panic!("test font missing: {xits}\n  run: ./scripts/fetch-test-fonts.sh")
    });
    if viewer.load_font("XITS Math", bytes, 400, false).is_err() {
        panic!("load_font failed for XITSMath-Regular.otf");
    }
}

fn layout(docx: Vec<u8>) -> Viewer {
    let mut viewer = match Viewer::new() {
        Ok(v) => v,
        Err(_) => panic!("Viewer::new failed"),
    };
    load_test_fonts(&mut viewer);
    if viewer.load_docx(docx).is_err() {
        panic!("load_docx failed");
    }
    viewer.run_layout_native().expect("run_layout_native");
    viewer
}

/// Serialize every page's line geometry, rounded to 0.01px. Lines carry
/// baseline/extents/height; segments carry x-start + item count (so a word
/// hopping lines, or any sub-pixel drift, changes the dump).
fn geometry_dump(viewer: &Viewer) -> String {
    fn line(out: &mut String, tag: &str, l: &PagedLine) {
        out.push_str(&format!(
            "{tag} b{:.2} l{:.2} r{:.2} h{:.2}",
            l.baseline_y, l.content_left, l.content_right, l.line_height
        ));
        for s in &l.segments {
            out.push_str(&format!(" s{:.2}n{}", s.x_start, s.items.len()));
        }
        // Math-GUARDED extension: emit an `M{w,a,d}` token per math atom on the
        // line. Only fires when a line carries an OMML atom, so math-free lines
        // (every existing oracle) stay byte-identical and the three pinned
        // hashes hold. New math fixtures get their own additive pins.
        for s in &l.segments {
            for it in &s.items {
                if let Some(m) = &it.math {
                    // Completeness guard — NO REST-DOTS. A new `MathBox`
                    // field cannot compile until someone decides here whether it
                    // affects rendered output. This is the exact shape that let
                    // `item.math` reach `LayoutLine::push` and not
                    // `accumulate_item_metrics`, shipping a mis-measure that
                    // went unnoticed for a long time.
                    let crate::wml::math::MathBox {
                        width, ascent, descent,
                        // `nodes` is the atom's flattened primitives — pinned by
                        // the GLYPH stream (each node's shaped run reaches
                        // `dump_body`), so re-dumping it here would duplicate a
                        // pin, not add one.
                        nodes: _,
                        // `is_display` changes geometry only via width/ascent/
                        // descent, which are dumped above; it has no independent
                        // rendered effect to observe.
                        is_display: _,
                    } = &**m;
                    out.push_str(&format!(" M{{w{width:.2}a{ascent:.2}d{descent:.2}}}"));
                }
            }
        }
        out.push('\n');
    }
    let pages = viewer.pages.borrow();
    let mut out = String::new();
    for (pi, page) in pages.iter().enumerate() {
        // Completeness guard — NO REST-DOTS. A new `PageLayout` field
        // cannot compile until a decision is recorded here about which stream
        // (if any) pins it. The `_`-bound fields each carry that
        // decision:
        //  - (`positioned_text_boxes` graduated from `_` to EMITTED once
        //    the LINE stream carried its box + interior line
        //    geometry below; the ITEM stream owns its content/style.)
        //  - `page_number`: the 1-based PAGE-field counter. Pinned indirectly:
        //    a PAGE/NUMPAGES field's rendered text enters the ITEM stream;
        //    positionally it is `pi + 1` by construction for every
        //    non-blank-inserted page.
        //  - `section_idx`: geometry-carrying (per-section page size/margins),
        //    pinned INDIRECTLY through every line box it moves; the index
        //    itself is checked by the section fixtures' targeted asserts.
        let PageLayout {
            elements,
            header_lines,
            footer_lines,
            header_rows,
            footer_rows,
            anchored_images,
            positioned_text_boxes,
            footnote_ids,
            endnote_ids,
            page_number: _,
            col_separator_lines,
            section_idx: _,
        } = page;
        out.push_str(&format!("== page {pi}\n"));
        // Anchored (floating) images + column separators are serialized ONLY
        // when present, so pages that have neither stay byte-identical to the
        // pre-existing dump (keeps the oracle-flow hash valid).
        for ai in anchored_images {
            out.push_str(&format!(
                "A x{:.2} y{:.2} w{:.2} h{:.2}\n",
                ai.x, ai.y, ai.width_px, ai.height_px
            ));
        }
        // Positioned text boxes: the box rect AND its interior line
        // boxes enter the LINE stream (they were previously invisible to it —
        // "moving a textbox is a silent change" was measured, not rhetorical).
        // Serialized only when present, so textbox-free pages keep their
        // pinned hashes byte-identical.
        for tb in positioned_text_boxes {
            // Completeness guard, same shape. The `_`-bound fields are owned
            // by the ITEM stream's `B{bi}` row (behind/fill/border are style,
            // not geometry) — adding a field forces the choice here.
            let PositionedTextBox {
                lines,
                x,
                y,
                width_px,
                height_px,
                behind_text: _,
                fill_color: _,
                border_color: _,
                border_width_px: _,
            } = tb;
            out.push_str(&format!("B x{x:.2} y{y:.2} w{width_px:.2} h{height_px:.2}\n"));
            for l in lines {
                line(&mut out, "  BL", l);
            }
        }
        for &(x, y_start, y_end) in col_separator_lines {
            out.push_str(&format!("S x{:.2} y0{:.2} y1{:.2}\n", x, y_start, y_end));
        }
        // Footnote/endnote ids resolved onto this page — serialized only when
        // present (footnote line geometry itself is a render-only path the
        // native harness does not run, but the per-page id set IS layout state).
        if !footnote_ids.is_empty() {
            out.push_str(&format!("FN {footnote_ids:?}\n"));
        }
        if !endnote_ids.is_empty() {
            out.push_str(&format!("EN {endnote_ids:?}\n"));
        }
        for l in header_lines {
            line(&mut out, "H", l);
        }
        // Header/footer tables (§17.10) — serialized ONLY when present so pages
        // without a header/footer table stay byte-identical to the prior dump
        // (keeps the oracle-flow/objects hashes valid; their headers are plain
        // paragraphs, no table).
        for row in header_rows {
            out.push_str(&format!("HT y{:.2} h{:.2}\n", row.y_top, row.height_px));
            for cell in &row.cells {
                out.push_str(&format!("HC x{:.2} w{:.2}\n", cell.x, cell.width_px));
                for l in &cell.lines {
                    line(&mut out, "  HCL", l);
                }
            }
        }
        for el in elements {
            match el {
                PageElement::Line(l) => line(&mut out, "L", l),
                PageElement::TableRow(row) => {
                    // Completeness guards for row and cell — NO
                    // REST-DOTS, installed at THIS walk (the canonical body
                    // one; the header/footer walks carry the same shapes and
                    // inherit the guarantee). Decisions for the `_` fields:
                    //  - row `border_color`/`has_borders`/`cell_margin_h/v`:
                    //    declared-unread aggregates (the painter reads the
                    //    per-cell resolved edges; see #[allow(dead_code)] at
                    //    the struct).
                    //  - cell `fill_color`/`v_merged_away`/`border_*`: pinned
                    //    by targeted table tests (row-split border repetition,
                    //    shading fixtures), not by a stream — a NEW field
                    //    still lands here for a stream decision.
                    let PagedTableRow {
                        cells,
                        y_top,
                        height_px,
                        border_color: _,
                        has_borders: _,
                        cell_margin_h: _,
                        cell_margin_v: _,
                    } = row;
                    out.push_str(&format!("T y{y_top:.2} h{height_px:.2}\n"));
                    for cell in cells {
                        let PagedTableCell {
                            lines,
                            x,
                            width_px,
                            fill_color: _,
                            v_merged_away: _,
                            border_top: _,
                            border_bottom: _,
                            border_left: _,
                            border_right: _,
                        } = cell;
                        out.push_str(&format!("C x{x:.2} w{width_px:.2}\n"));
                        for l in lines {
                            line(&mut out, "  CL", l);
                        }
                    }
                }
            }
        }
        for l in footer_lines {
            line(&mut out, "F", l);
        }
        for row in footer_rows {
            out.push_str(&format!("FT y{:.2} h{:.2}\n", row.y_top, row.height_px));
            for cell in &row.cells {
                out.push_str(&format!("FC x{:.2} w{:.2}\n", cell.x, cell.width_px));
                for l in &cell.lines {
                    line(&mut out, "  FCL", l);
                }
            }
        }
    }
    out
}

// ─── the ITEM stream ─────────────────────────────────────────────────────────
//
// `geometry_dump` above is the LINE stream: where every line box SITS. It is
// structurally blind to what is IN the lines — text, item order, run identity,
// `char_start` — which is measured, not theoretical: re-flagging every painted
// run Comic Sans, or hiding every run with `w:vanish`, or dropping every
// hyperlink target, every paragraph border and every shading, left the entire
// 720-test suite green. So did flipping every item's bidi parity.
//
// The ITEM stream is the orthogonal half, and it is deliberately COORDINATE-FREE.
// Line membership is a per-page `L0/L1/L2…` counter and segment membership an
// `S0/S1…` counter — never a baseline or an x. Two consequences, both wanted:
//
//  1. The LINE stream needs NO re-baseline to gain this coverage. Every one of
//     the 21 committed `geometry_dump` constants keeps its exact value, so the
//     "prove the old hash still matches before recording a new one" rule stops
//     being a one-time procedure in a commit message and becomes a PERMANENT
//     assertion that runs on every `cargo test`. There is no window in which a
//     re-record can bless a live bug, because there is no re-record.
//  2. A pure repositioning (margins, page size, baseline maths) reds the LINE
//     stream and leaves this one byte-identical — and "line RED / items GREEN"
//     is itself a diagnosis: nothing moved BETWEEN lines, only where they sit.
//
// It is pinned as a GOLDEN FILE rather than a hash, because the single most
// dangerous operation in this repo is a re-baseline and a hex constant makes it
// unreviewable. A golden file turns it into a git diff.

/// Serialize this viewer into the two content streams — `(ITEM, GLYPH)`.
///
/// ITEM: every page's item content — text, order, full `RunStyle`, char span,
/// per-item advances and every payload — plus the per-line paragraph id,
/// borders, shading and first/last flags. No coordinates (see above).
///
/// GLYPH: the per-glyph gid/advance/offset/cluster vector inside every
/// shaped run, interned. Produced by the SAME walk, because "which lines and
/// items exist, in what order" is one rule and must have one spelling; the two
/// go to separate golden files so either can be added or changed without
/// re-baselining the other.
///
/// The per-struct `dump_into` helpers this delegates to open with an exhaustive
/// destructure and NO rest-dots, so a new field on `Item`, `RunStyle` or
/// `PagedLine` cannot COMPILE until someone decides which stream owns it.
fn content_dumps(viewer: &Viewer) -> (String, String) {
    use std::fmt::Write;
    let pages = viewer.pages.borrow();
    let mut out = String::new();
    // First-appearance intern table of rendered RunStyles; printed as a header.
    let mut styles: Vec<String> = Vec::new();
    // The GLYPH stream rides this same walk (see `GlyphStream`), so the
    // two goldens can never disagree about which lines and items exist.
    let mut glyphs = GlyphStream::default();
    for (pi, page) in pages.iter().enumerate() {
        let _ = writeln!(out, "== page {pi} num{} sect{}", page.page_number, page.section_idx);
        // `geometry_dump` records an anchored image's BOX; only rel_id and
        // behind_text say WHICH bitmap and in what z-order.
        for ai in &page.anchored_images {
            let _ = writeln!(out, "A rel{:?} behind{} anchor{:?}",
                ai.rel_id, ai.behind_text as u8, ai.anchor_para);
        }
        if !page.footnote_ids.is_empty() {
            let _ = writeln!(out, "FN {:?}", page.footnote_ids);
        }
        if !page.endnote_ids.is_empty() {
            let _ = writeln!(out, "EN {:?}", page.endnote_ids);
        }
        for (i, l) in page.header_lines.iter().enumerate() {
            l.dump_into(&mut out, &format!("H{i}"), "", &mut styles, &mut glyphs);
        }
        for (ri, row) in page.header_rows.iter().enumerate() {
            let _ = writeln!(out, "HT{ri} cells{}", row.cells.len());
            for (ci, cell) in row.cells.iter().enumerate() {
                for (li, l) in cell.lines.iter().enumerate() {
                    l.dump_into(&mut out, &format!("HC{ci}L{li}"), "  ", &mut styles, &mut glyphs);
                }
            }
        }
        let mut li = 0usize;
        for el in &page.elements {
            match el {
                PageElement::Line(l) => {
                    l.dump_into(&mut out, &format!("L{li}"), "", &mut styles, &mut glyphs);
                    li += 1;
                }
                PageElement::TableRow(row) => {
                    let _ = writeln!(out, "T{li} cells{}", row.cells.len());
                    li += 1;
                    for (ci, cell) in row.cells.iter().enumerate() {
                        for (cl, l) in cell.lines.iter().enumerate() {
                            l.dump_into(&mut out, &format!("C{ci}L{cl}"), "  ", &mut styles, &mut glyphs);
                        }
                    }
                }
            }
        }
        for (i, l) in page.footer_lines.iter().enumerate() {
            l.dump_into(&mut out, &format!("F{i}"), "", &mut styles, &mut glyphs);
        }
        for (ri, row) in page.footer_rows.iter().enumerate() {
            let _ = writeln!(out, "FT{ri} cells{}", row.cells.len());
            for (ci, cell) in row.cells.iter().enumerate() {
                for (li, l) in cell.lines.iter().enumerate() {
                    l.dump_into(&mut out, &format!("FC{ci}L{li}"), "  ", &mut styles, &mut glyphs);
                }
            }
        }
        // Positioned (anchored) text boxes — a whole content stream that
        // `geometry_dump` drops ENTIRELY, interior lines and all.
        for (bi, tb) in page.positioned_text_boxes.iter().enumerate() {
            let _ = writeln!(out, "B{bi} {:.2}x{:.2} behind{} fill{} border{}/{:.2} lines{}",
                tb.width_px, tb.height_px, tb.behind_text as u8,
                tb.fill_color.as_deref().unwrap_or("-"),
                tb.border_color.as_deref().unwrap_or("-"), tb.border_width_px,
                tb.lines.len());
            for (li, l) in tb.lines.iter().enumerate() {
                l.dump_into(&mut out, &format!("BL{li}"), "  ", &mut styles, &mut glyphs);
            }
        }
    }
    // The style table goes at the HEAD so a reader meets the vocabulary before
    // the items that reference it. Built during the walk, prepended after.
    let mut head = String::new();
    for (i, sty) in styles.iter().enumerate() {
        let _ = writeln!(head, "y{i} {sty}");
    }
    head.push_str(&out);
    (head, glyphs.finish())
}

/// Directory holding the committed ITEM- and GLYPH-stream golden files.
const SNAPSHOT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/oracle-snapshots");

/// Assert this viewer's ITEM and GLYPH streams against their committed golden
/// files (`{name}.items.txt`, `{name}.glyphs.txt`).
///
/// Both come from ONE `content_dumps` walk and are pinned as SEPARATE files, for
/// the same reason the LINE hash and the ITEM golden are separate assertions:
/// the GLYPH stream could be added additively, with every `.items.txt` staying
/// byte-identical and going on asserting that nothing else moved.
///
/// ONE CAUTION, because the split is not total: `ORACLE_BLESS=oracle-flow`
/// re-records BOTH of that fixture's goldens. They are one fixture and one
/// layout, so re-recording half of it would leave the pair describing two
/// different documents — but it does mean a bless aimed at an intended ITEM
/// change silently accepts whatever the GLYPH stream now says. Read both halves
/// of the `git diff`.
fn assert_content_streams(name: &str, viewer: &Viewer) {
    let (items, glyphs) = content_dumps(viewer);
    assert_golden(name, "items", &items,
        "the LINE geometry hash is a SEPARATE assertion, so if it is still green nothing \
         MOVED; text, item order, run identity or the char_start contract did");
    assert_golden(name, "glyphs", &glyphs,
        "shaping changed INSIDE a run — a glyph's identity, its advance, its positioning \
         offset or its cluster. If the ITEM golden is still green the run's glyph COUNT and \
         TOTAL advance are unchanged, so the change redistributed a run rather than resizing it");
}

/// Compare one stream against `oracle-snapshots/{name}.{kind}.txt`.
///
/// On drift the message names the FIRST differing record with its line number
/// and both sides — a red pin must tell a human WHAT moved, which is the whole
/// reason these are files and not hashes.
///
/// RE-RECORDING IS DELIBERATELY ASYMMETRIC, because the two cases carry opposite
/// risk. Adding a NEW fixture cannot bless anything away — there is no prior
/// value — so a missing golden is written automatically. OVERWRITING an existing
/// golden is how a live regression gets frozen into the net, so it requires
/// naming that fixture: `ORACLE_BLESS=oracle-flow,math-fraction`. `ORACLE_BLESS=all`
/// exists for a deliberate wholesale re-record and should appear in a commit
/// message that says why.
///
/// The earlier form was `ORACLE_BLESS=1` → rewrite everything unconditionally,
/// which relocated the re-baseline hazard rather than removing it: recording one
/// new fixture silently overwrote all 23, including any that had drifted, as an
/// injected `char_start += 1` regression demonstrated.
///
/// The `git diff` is the review artifact and the thing that actually protects
/// you. The overwrite notice below is a courtesy only — a blessing run PASSES, so
/// `cargo test` captures its stderr unless you also pass `--nocapture`.
/// Every golden the suite asserts must EXIST on disk and be tracked.
///
/// `assert_golden` records a missing golden and passes — correct for a genuinely
/// new fixture, and the reason adding one is frictionless. But it makes the
/// far more likely mistake silent: committing a new stream and forgetting to
/// `git add` its files leaves a pin that re-records itself on every machine and
/// is vacuously green forever. This counts them instead, so the omission fails
/// once, loudly, with the count.
#[test]
fn every_golden_stream_is_on_disk() {
    let dir = std::path::Path::new(SNAPSHOT_DIR);
    let mut items = 0usize;
    let mut glyphs = 0usize;
    for e in std::fs::read_dir(dir).expect("oracle-snapshots/ must exist") {
        let n = e.expect("dir entry").file_name().to_string_lossy().into_owned();
        if n.ends_with(".items.txt") {
            items += 1;
        } else if n.ends_with(".glyphs.txt") {
            glyphs += 1;
        }
    }
    // A floor, not an exact count: adding a fixture must not red this test. It
    // catches the whole set going missing (an un-added directory, a bad clean).
    assert!(items >= 23, "only {items} .items.txt goldens on disk — expected >= 23");
    assert!(glyphs >= 23, "only {glyphs} .glyphs.txt goldens on disk — expected >= 23");
    assert_eq!(
        items, glyphs,
        "the ITEM and GLYPH streams cover different fixture sets ({items} vs {glyphs}) —          one of them was not committed"
    );

    // Same correction as ops_golden.rs, and this test had the identical flaw
    // from the day it was written: counting files on disk cannot detect a
    // forgotten `git add`, because the sibling tests re-record a missing golden
    // before this one runs. Tracking is the property the name promises.
    for suffix in [".items.txt", ".glyphs.txt"] {
        if let Some(untracked) = crate::engine::untracked_goldens(suffix) {
            assert!(
                untracked.is_empty(),
                "{suffix} goldens on disk but NOT tracked by git: {untracked:?} — they \
                 re-record themselves on every machine and the pin is vacuous. `git add` them."
            );
        }
    }
}

fn assert_golden(name: &str, kind: &str, got: &str, diagnosis: &str) {
    let path = std::path::Path::new(SNAPSHOT_DIR).join(format!("{name}.{kind}.txt"));
    let bless = std::env::var("ORACLE_BLESS").unwrap_or_default();
    let named = bless == "all" || bless.split(',').any(|f| f.trim() == name);
    if !path.exists() || named {
        // A missing golden is a NEW fixture — safe. An existing one is only
        // overwritten when this fixture was named (or `all`).
        std::fs::create_dir_all(SNAPSHOT_DIR).expect("create oracle-snapshots/");
        if path.exists() && named {
            let prev = std::fs::read_to_string(&path).unwrap_or_default();
            if prev != got {
                eprintln!(
                    "[ORACLE_BLESS] {name}.{kind}: OVERWRITING an existing golden \
                     ({} → {} records). Review the `git diff` before committing.",
                    prev.lines().count(),
                    got.lines().count(),
                );
            }
        }
        std::fs::write(&path, got.as_bytes()).expect("write golden");
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("{name}: cannot read {kind} golden {}: {e} — delete it to re-record", path.display())
    });
    if got == want {
        return;
    }
    let (mut gl, mut wl) = (got.lines(), want.lines());
    let mut n = 0usize;
    loop {
        n += 1;
        match (gl.next(), wl.next()) {
            (Some(a), Some(b)) if a == b => continue,
            (a, b) => panic!(
                "{name}: {kind} stream drifted at record {n} — {diagnosis}.\n  golden: {}\n  \
                 actual: {}\n({} records golden vs {} actual; if this change is INTENDED, \
                 re-record just this fixture with ORACLE_BLESS={name} and review the git diff)",
                b.unwrap_or("<end of file>"), a.unwrap_or("<end of file>"),
                want.lines().count(), got.lines().count(),
            ),
        }
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn assert_oracle(name: &str, docx: Vec<u8>, expect_pages: u32, expect_hash: u64) {
    assert_oracle_with(name, docx, expect_pages, expect_hash, &[]);
}

/// [`assert_oracle`] with EXTRA per-fixture faces. Deliberately per-fixture,
/// not in `load_test_fonts`: a face loaded globally enters every fixture's
/// missing-glyph fallback candidate set, so "just add it to the loader" could
/// silently move goldens that never mention the family. A fixture that needs
/// a face names it; nothing else can see it.
fn assert_oracle_with(
    name: &str,
    docx: Vec<u8>,
    expect_pages: u32,
    expect_hash: u64,
    extra_fonts: &[(&str, &str)],
) {
    let mut viewer = match Viewer::new() {
        Ok(v) => v,
        Err(_) => panic!("Viewer::new failed"),
    };
    load_test_fonts(&mut viewer);
    for (family, file) in extra_fonts {
        let path = format!("{}/test-fonts/{file}", env!("CARGO_MANIFEST_DIR"));
        let bytes = std::fs::read(&path).unwrap_or_else(|_| {
            panic!("test font missing: {path}\n  run: ./scripts/fetch-test-fonts.sh")
        });
        if viewer.load_font(family, bytes, 400, false).is_err() {
            panic!("load_font failed for {file}");
        }
    }
    if viewer.load_docx(docx).is_err() {
        panic!("load_docx failed");
    }
    viewer.run_layout_native().expect("run_layout_native");
    let pages = viewer.pages.borrow().len() as u32;
    let dump = geometry_dump(&viewer);
    let hash = fnv1a(dump.as_bytes());
    if std::env::var("ORACLE_DUMP").is_ok() {
        println!("---- {name} geometry dump ----\n{dump}");
    }
    assert_eq!(
        pages, expect_pages,
        "{name}: page count drifted (got {pages}, baseline {expect_pages}) — \
         if intentional, re-baseline in oracle_tests.rs"
    );
    assert_eq!(
        hash, expect_hash,
        "{name}: line-geometry hash drifted (got {hash:#018x}, baseline {expect_hash:#018x}) — \
         run with ORACLE_DUMP=1 to inspect; re-baseline ONLY for intentional layout changes"
    );
    // Fixpoint NO-COLLATERAL gate: the on-disk oracle snapshots carry NO
    // cursor-independent (Page|Margin) wrapping float, so the fixpoint must never
    // fire on them. A non-zero pages_relaid means the fixpoint touched a real doc
    // it should not have — a BUG (diagnose), never a re-baseline. Pairs with the
    // byte-identical hash above.
    //
    // This is the one gate NO dump can replace, however strong: it catches a
    // re-lay that CONVERGES to identical geometry AND identical items. Keep it on
    // every pin.
    assert_eq!(
        viewer.layout_stats().pages_relaid, 0,
        "{name}: the fixpoint re-laid a page on a float-free oracle (collateral) — diagnose, do NOT re-baseline"
    );
    // The ITEM stream — text, order, run identity, char_start. Orthogonal
    // to the hash above and pinned separately, so the hash keeps its historical
    // value and goes on proving "nothing else moved" for free.
    assert_content_streams(name, &viewer);
}

// ─── oracle document: "continuous-title" ────────────────────────────────────
// Reproduces the dropped-leading-section bug: a full-width title section
// (P1 title + P2 empty paragraph whose pPr holds a NO-w:type 1-column sectPr,
// which defaults the section break to NextPage) followed by a body-level
// CONTINUOUS 2-column sectPr whose short body does NOT fill both columns.
//
// Carry machinery places the title into `current`, carries it into the 2-col
// section, then the last-page column-balancing block must preserve it. Before
// the fix the balancing reset (`current = PageLayout::new(); y_in_page = 0`)
// discarded the carried title and floated the 2-col body to the page top —
// the whole title section vanished. After the fix the title stays full-width
// on top of page 0 with the balanced columns below it.

fn oracle_continuous_title_docx() -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults>
    <w:rPrDefault><w:rPr>
      <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
      <w:sz w:val="22"/><w:szCs w:val="22"/>
    </w:rPr></w:rPrDefault>
    <w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault>
  </w:docDefaults>
  <w:style w:type="paragraph" w:styleId="Heading1">
    <w:name w:val="heading 1"/>
    <w:pPr><w:spacing w:before="240" w:after="120"/></w:pPr>
    <w:rPr><w:b/><w:sz w:val="32"/></w:rPr>
  </w:style>
</w:styles>"#
    );

    let mut body = String::new();
    // P1: full-width title.
    body.push_str(&p(
        r#"<w:pStyle w:val="Heading1"/><w:jc w:val="center"/>"#,
        &r("", "THE GREAT TITLE ABOVE THE COLUMNS"),
    ));
    // P2: empty paragraph closing section 0 — a 1-column sectPr with NO w:type
    // (defaults to NextPage). This is the distinguishing structure.
    body.push_str(
        r#"<w:p><w:pPr><w:sectPr>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
      <w:cols w:num="1"/>
    </w:sectPr></w:pPr></w:p>"#,
    );
    // P3: short body — a couple sentences, NOT enough to fill two columns.
    body.push_str(&p("", &r("", &filler(0, 2))));
    // Body-level sectPr: CONTINUOUS 2-column.
    body.push_str(
        r#"<w:sectPr>
      <w:type w:val="continuous"/>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
      <w:cols w:num="2" w:space="720"/>
    </w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
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
</Relationships>"#
        .to_string();

    build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
    ])
}

/// Concatenate all text items of a line into a single string.
fn line_text(l: &PagedLine) -> String {
    let mut s = String::new();
    for seg in &l.segments {
        for it in &seg.items {
            s.push_str(&it.text);
        }
    }
    s
}

// ─── the oracles ────────────────────────────────────────────────────────────

#[test]
fn oracle_continuous_title_drop_regression() {
    // Regression for the dropped-leading-section bug (carry + last-page column
    // balancing). Asserts behavior directly (not just a pinned hash) so the
    // failing-before / passing-after intent is explicit.
    let viewer = layout(oracle_continuous_title_docx());
    let pages = viewer.pages.borrow();

    // 1. Single page.
    assert_eq!(pages.len(), 1, "expected the title + short 2-col body to share one page");
    let page = &pages[0];

    // Collect body lines (page-coordinate order).
    let lines: Vec<&PagedLine> = page
        .elements
        .iter()
        .filter_map(|e| match e {
            PageElement::Line(l) => Some(l),
            _ => None,
        })
        .collect();
    assert!(!lines.is_empty(), "page 0 has no lines");

    // 2. The title line exists, full-width (spans across both column bands),
    //    at the TOP of the page. Before the fix this line did not exist at all.
    let title = lines
        .iter()
        .find(|l| line_text(l).contains("THE GREAT TITLE"))
        .expect("title line 'THE GREAT TITLE' is missing — leading section was dropped");
    let content_width = title.content_right - title.content_left;
    // Full content width at 12240 twip page, 1440 twip margins each side =
    // 9360 twip = 6.5in = 624px @96dpi. A single 2-col column band is ~half that.
    assert!(
        content_width > 480.0,
        "title is not full-width (content span {content_width:.1}px) — it landed in a column band"
    );

    // 3. The body's first line is BELOW the title and lands in a (narrow)
    //    column band — its content span is roughly half the full width.
    let body = lines
        .iter()
        .find(|l| line_text(l).contains("The layout engine measures"))
        .expect("body line is missing");
    assert!(
        body.baseline_y > title.baseline_y,
        "body (baseline {:.1}) is not below title (baseline {:.1})",
        body.baseline_y, title.baseline_y
    );
    let body_span = body.content_right - body.content_left;
    assert!(
        body_span < 400.0,
        "body line is not in a column band (span {body_span:.1}px)"
    );
}

#[test]
fn oracle_flow_layout_snapshot() {
    // Baselines established 2026-06-06. Page count + FNV-1a geometry hash.
    // Geometry re-baselined 2026-06-24: these oracles are settings.xml-less, so
    // the compatibilityMode-absent→legacy default (lib.rs) now enables the
    // half-space trailing reserve. Page count held at 5; only line-fill geometry
    // shifted (intended).
    // Re-baselined 2026-06-25: page-top first-paragraph before-spacing now applied
    // (suppressTopSpacing absent) + tables consume the preceding paragraph's
    // after-spacing. Page count held at 5; verified vs Word on calibration doc C6
    // (page-1 top −16→−4pt, page-2 table −10.6→0pt).
    // Re-baselined after the hhea-lineGap fix: oracle-flow uses Arimo (lineGap
    // 67/2048em), so every line's height grew +3.27% to the font's true line
    // height (ascent+descent+lineGap, the Windows/Word model). Page count held at
    // 5; only line geometry shifted. gap=0 fonts (oracle-objects) are unchanged.
    assert_oracle("oracle-flow", oracle_flow_docx(), 5, 0xd476c421510f1870);
}

#[test]
fn oracle_objects_layout_snapshot() {
    // Guards floats (wrapSquare anchor), a continuous 2-column section + its
    // separator, a return to single column, and footnotes. Baselines pinned
    // 2026-06-06. Geometry re-baselined 2026-06-24 for the compatibilityMode-
    // absent→legacy reserve default (settings.xml-less oracle; page count held
    // at 5, line-fill geometry shifted).
    //
    // Re-baselined 2026-07-27, for the float-aware packer alone: it now
    // charges the SAME legacy trailing-blank reserve as the float-free packer
    // (`item_fits`) instead of none. Reviewed record by record — 3 of this
    // oracle's lines re-partitioned their words (n10→n9, n11→n10, n10→n12; 31
    // items in, 31 out) and NOTHING else moved: every baseline, line height,
    // segment x, line count and page count is byte-identical, and the ITEM and
    // GLYPH streams are byte-identical once the L#/S# framing is stripped. A
    // pure break-point move on the wrapSquare paragraph, which is the point.
    assert_oracle("oracle-objects", oracle_objects_docx(), 5, 0xbcfe64182d4c649f);
}

// ─── oracle document: "hdr-ftr-inherit" ─────────────────────────────────────
// §17.10.1/§17.10.2 — a section that omits header/footer references inherits
// them from the previous section. Section 1 defines a default header+footer;
// section 2 (body-level sectPr) declares ONLY pgSz+pgMar. A pageBreakBefore
// forces section 2 onto its own page so the inherited bands are observable in
// the geometry dump (header_lines "H" + footer_lines "F" on BOTH pages, and the
// section-2 body top shifted down by the reserved header band).
fn oracle_hdr_ftr_inherit_docx() -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/></w:rPr></w:rPrDefault></w:docDefaults></w:styles>"#
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>
  <w:p><w:pPr><w:sectPr>
    <w:headerReference w:type="default" r:id="rId2"/>
    <w:footerReference w:type="default" r:id="rId3"/>
    <w:pgSz w:w="12240" w:h="15840"/>
    <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
  </w:sectPr></w:pPr><w:r><w:t>Section one page.</w:t></w:r></w:p>
  <w:p><w:pPr><w:pageBreakBefore/></w:pPr><w:r><w:t>Section two page — no refs.</w:t></w:r></w:p>
  <w:sectPr>
    <w:pgSz w:w="12240" w:h="15840"/>
    <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
  </w:sectPr>
</w:body></w:document>"#
    );
    let header = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:hdr {W_NS}><w:p><w:pPr><w:jc w:val="right"/></w:pPr><w:r><w:rPr><w:sz w:val="18"/></w:rPr><w:t>Inherited Header</w:t></w:r></w:p></w:hdr>"#
    );
    let footer = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:ftr {W_NS}><w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:rPr><w:sz w:val="18"/></w:rPr><w:t>Inherited Footer</w:t></w:r></w:p></w:ftr>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
  <Override PartName="/word/header1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"/>
  <Override PartName="/word/footer1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"/>
</Types>"#.to_string();
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#.to_string();
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/>
</Relationships>"#.to_string();

    build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
        ("word/header1.xml", header),
        ("word/footer1.xml", footer),
    ])
}

#[test]
fn oracle_hdr_ftr_inherit() {
    // NEW pin (§17.10.1/§17.10.2). Section 2 omits all hdr/ftr refs → inherits
    // section 1's default header+footer bands; pre-fix it rendered blank bands.
    // Recorded from the first GREEN run with the inheritance post-pass live.
    assert_oracle("oracle-hdr-ftr-inherit", oracle_hdr_ftr_inherit_docx(), 2, 0x7d8d0b6aa73babdd);
}

// ─── selection setter lineage: the setter round-trip / normalize / clear pins
// (normalize, clear, collapsed-caret, sibling-kind exclusivity) live with the
// unified setter in `select_dispatch::tests` — same assertions, the
// unified `doc_set_selection_core` spelling.

// Integration wiring — proves the lib.rs `paint_wml_page` call site is
// INERT when the selection is empty (the byte-identity guarantee), LIVE when
// set, and back to ZERO on clear. This is the piece the synthetic select.rs
// tests can't cover: it drives the REAL page model through a RecordingPainter.
#[test]
fn paint_wml_page_selection_wiring_inert_then_live_then_clear() {
    use super::engine::displaylist::{DrawCmd, RecordingPainter};

    let v = layout(oracle_flow_docx());

    // Count selection FillRects, keyed off the distinctive selection fill color
    // "#3B82F6" (distinct from search "#FFE066"). We track state between a
    // SetFillStyle("#3B82F6") and the next state-changing command.
    fn selection_fill_rects(v: &Viewer) -> usize {
        let rec = RecordingPainter::new();
        v.paint_wml_page(0, &rec, None).expect("paint page 0");
        let cmds = rec.take();
        let mut count = 0usize;
        let mut in_selection = false;
        for c in &cmds {
            match c {
                DrawCmd::SetFillStyle(s) => in_selection = s == "#3B82F6",
                DrawCmd::FillRect { .. } if in_selection => count += 1,
                _ => {}
            }
        }
        count
    }

    // EMPTY: no selection set ⇒ zero selection draws (byte-identity guarantee).
    assert_eq!(selection_fill_rects(&v), 0, "empty selection must paint zero rects");

    // SET: a wide selection over page 0's first paragraphs ⇒ ≥1 selection rect.
    v.doc_set_selection_core(Some(crate::select_dispatch::DocSelectionArg::Text {
        start: super::wml::select::TextPos::new(0, 0),
        end: super::wml::select::TextPos::new(i32::MAX as u32, i32::MAX as u32),
    }));
    assert!(
        selection_fill_rects(&v) >= 1,
        "a set selection must paint at least one rect (wiring is live)"
    );

    // CLEAR: `None` (the unified clear — no sentinel) ⇒ back to zero draws.
    v.doc_set_selection_core(None);
    assert_eq!(selection_fill_rects(&v), 0, "cleared selection returns to zero draws");
}

// ─── selection repro: hitTest→selectionRects roundtrip on a REAL page ───────
// A single justified body paragraph "justified" (9 bytes, one page). In the
// browser, hitTest returned a valid TextPos but selectionRects returned [] for
// every range on the same page. This drives the REAL layout path (collect_items →
// layout_paragraph → paginate) and asserts a NON-EMPTY rect for [0,0)..(0,9).

/// The exact align-justify.docx fixture, rebuilt inline (no styles.xml, no
/// sectPr — pure defaults; a single `<w:jc w:val="both"/>` paragraph "justified").
fn single_justified_para_docx() -> Vec<u8> {
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body><w:p><w:pPr><w:jc w:val="both"/></w:pPr><w:r><w:t>justified</w:t></w:r></w:p></w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#
        .to_string();
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#
        .to_string();
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"></Relationships>"#
        .to_string();
    build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
    ])
}

#[test]
fn justified_para_hit_to_selection_rects_roundtrip() {
    use super::wml::select::{selection_rects_impl, TextPos};

    let viewer = layout(single_justified_para_docx());
    let pages = viewer.pages.borrow();
    assert_eq!(pages.len(), 1, "single para must be one page");
    let page = &pages[0];

    // The REAL body line must carry the document-order para id 0 (NOT
    // PARA_SENTINEL): the ParaCapture walk in `run_layout_impl` stamps it via
    // `cache.para_id`. This is the piece the synthetic select.rs tests set by
    // hand — proving the real layout path stamps it too.
    let body_paras: Vec<u32> = page
        .elements
        .iter()
        .filter_map(|e| match e {
            PageElement::Line(l) => Some(l.para),
            _ => None,
        })
        .collect();
    assert_eq!(body_paras, vec![0], "real body line must be stamped para 0");

    // The hit→rects roundtrip on the REAL page: a full-paragraph range must
    // yield a NON-EMPTY rect (this is what the browser found returning [] —
    // proving the divergence is NOT engine-side but the TS wrapper's
    // `selectionRects(page, TextPos, TextPos)` signature being mis-called with
    // raw scalars, which collapses every range to (0,0)-(0,0); see below).
    let rects = selection_rects_impl(page, TextPos::new(0, 0), TextPos::new(0, 9));
    assert!(
        !rects.is_empty(),
        "real-page selection_rects_impl(0,0..0,9) must be non-empty: {rects:?}"
    );
    // Geometry pin (not just liveness): the full-word range must produce ONE
    // rect that starts at the left content margin (the line's content_left) and
    // has positive width covering "justified" — so a future regression that
    // silently narrowed/mis-placed the rect is caught, not just an empty result.
    let line = page.elements.iter().find_map(|e| match e {
        PageElement::Line(l) => Some(l),
        _ => None,
    }).expect("one body line");
    assert_eq!(rects.len(), 1, "one contiguous LTR run → one rect: {rects:?}");
    let r = &rects[0];
    assert!(
        (r.x - line.content_left).abs() < 0.5,
        "rect must start at the line's content_left {}: got x={}", line.content_left, r.x
    );
    assert!(r.w > 10.0, "rect must span the 9-char word, not a sliver: w={}", r.w);
    assert!(
        r.y <= line.baseline_y + 0.5 && r.y >= line.baseline_y - line.line_height,
        "rect y must sit on the line band (baseline {}, lh {}): got y={}",
        line.baseline_y, line.line_height, r.y
    );

    // Mechanism pin: a COLLAPSED range (what `undefined.para → 0` coercion
    // produces when the 3-arg TS wrapper is called with 5 raw scalars) yields
    // ZERO rects — the exact [] the browser probe observed for EVERY range.
    let collapsed = selection_rects_impl(page, TextPos::new(0, 0), TextPos::new(0, 0));
    assert!(collapsed.is_empty(), "a collapsed range must yield [] (mis-call footprint)");
}

// ─── anchored text-box wrap (§20.4.2.17) ─────────────────────────────────────
// A wp:anchor'd text box (wps:wsp → wps:txbx → w:txbxContent) with wrapSquare
// must EXCLUDE a rectangle the body text flows around — text never draws under
// it (§20.4.2.17: only wrapNone permits text under the object). Before the fix
// the text box was drawn (PositionedTextBox) but registered NO float, so body
// lines in its vertical band kept the full column width and sat under the box.

fn textbox_wrap_docx() -> Vec<u8> {
    // Text box extent: 2in × 2in.
    const BOX_EMU: i64 = 1_828_800;

    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults>
    <w:rPrDefault><w:rPr>
      <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
      <w:sz w:val="22"/><w:szCs w:val="22"/>
    </w:rPr></w:rPrDefault>
    <w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault>
  </w:docDefaults>
</w:styles>"#
    );

    // Anchored wrapSquare text box at the column left (posOffset 0/0), text on
    // its right. A bare wps:wsp carrying a one-paragraph txbxContent. Wrap dist
    // is 0 (Parasma reads dist_* from the wrap element, not the anchor — a
    // pre-existing approximation shared with images), so the exclusion's right
    // edge is exactly the box's right edge.
    let textbox = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV>
  <wp:extent cx="{BOX_EMU}" cy="{BOX_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapSquare wrapText="right"/>
  <wp:docPr id="1" name="TextBox 1"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp>
      <wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_EMU}" cy="{BOX_EMU}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">Boxed.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/>
    </wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );

    let mut body = String::new();
    // Host paragraph: the anchor + plenty of text that must wrap to its right.
    body.push_str(&format!("<w:p>{textbox}{}</w:p>", r("", &filler(0, 6))));
    for i in 0..4 {
        body.push_str(&p("", &r("", &filler(i + 1, 4))));
    }
    body.push_str(
        r#"<w:sectPr>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
      <w:cols w:num="1"/>
    </w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}</w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;

    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;

    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

#[test]
fn textbox_wrap_excludes_text() {
    let viewer = layout(textbox_wrap_docx());
    let pages = viewer.pages.borrow();
    let page = &pages[0];
    // The text box parsed and was positioned.
    assert_eq!(
        page.positioned_text_boxes.len(),
        1,
        "the anchored text box should be placed"
    );
    let tb = &page.positioned_text_boxes[0];
    let box_right = tb.x + tb.width_px;
    let (band_top, band_bottom) = (tb.y, tb.y + tb.height_px);
    // Every body line whose baseline sits inside the box's vertical band must
    // start to the RIGHT of the box (wrapText="right"), never under it.
    let mut in_band = 0;
    for el in &page.elements {
        if let PageElement::Line(l) = el {
            if l.baseline_y > band_top && l.baseline_y < band_bottom {
                in_band += 1;
                let first_x = l.segments.first().map(|s| s.x_start).unwrap_or(l.content_left);
                assert!(
                    first_x >= box_right - 0.5,
                    "a line in the text-box band started at x{first_x:.2}, not right of \
                     the box (right edge x{box_right:.2}) — text is under the box (§20.4.2.17)"
                );
            }
        }
    }
    assert!(
        in_band >= 2,
        "expected ≥2 body lines beside the text box, got {in_band}"
    );
    // The exclusion must be vertically BOUNDED: at least one body line whose
    // baseline sits BELOW the box resumes at the column left (full width). This
    // pins the float's vertical extent — without it an over-tall / mis-y'd
    // exclusion that kept every line shifted right would pass the check above
    // for the wrong reason.
    let resumed_full_width = page.elements.iter().any(|el| {
        matches!(el, PageElement::Line(l)
            if l.baseline_y > band_bottom
                && l.segments.first().map(|s| s.x_start).unwrap_or(l.content_left) <= tb.x + 0.5)
    });
    assert!(
        resumed_full_width,
        "no body line below the text box resumed at the column left (x{:.2}) — \
         the exclusion is not vertically bounded",
        tb.x
    );
}

// ─── anchored text-box wrap INSIDE A TABLE CELL (§20.4.2.17) ─────────────────
// Same wrapSquare text box as the fixture above, but anchored in a single-cell table. The
// cell layout path packs float-blind and flatten_cell_lines drops the anchored
// content, so before the fix the box vanishes (not drawn) AND the cell text
// runs under where it should be. After: the box draws at the cell origin and
// the cell's text wraps to its right.

fn cell_textbox_wrap_docx() -> Vec<u8> {
    const BOX_EMU: i64 = 1_828_800; // 2in × 2in

    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults>
    <w:rPrDefault><w:rPr>
      <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
      <w:sz w:val="22"/><w:szCs w:val="22"/>
    </w:rPr></w:rPrDefault>
    <w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault>
  </w:docDefaults>
</w:styles>"#
    );

    let textbox = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV>
  <wp:extent cx="{BOX_EMU}" cy="{BOX_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapSquare wrapText="right"/>
  <wp:docPr id="1" name="TextBox 1"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp>
      <wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_EMU}" cy="{BOX_EMU}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">Boxed.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/>
    </wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );

    let mut body = String::new();
    body.push_str(
        r#"<w:tbl><w:tblPr><w:tblW w:w="9000" w:type="dxa"/></w:tblPr><w:tblGrid><w:gridCol w:w="9000"/></w:tblGrid><w:tr><w:tc><w:tcPr><w:tcW w:w="9000" w:type="dxa"/></w:tcPr>"#,
    );
    // Host paragraph inside the cell: anchor + plenty of text that must wrap.
    body.push_str(&format!("<w:p>{textbox}{}</w:p>", r("", &filler(0, 6))));
    for i in 0..4 {
        body.push_str(&p("", &r("", &filler(i + 1, 4))));
    }
    body.push_str("</w:tc></w:tr></w:tbl>");
    body.push_str(
        r#"<w:sectPr>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
      <w:cols w:num="1"/>
    </w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}</w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;

    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;

    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

#[test]
fn cell_textbox_wrap_excludes_text() {
    let viewer = layout(cell_textbox_wrap_docx());
    let pages = viewer.pages.borrow();
    let page = &pages[0];
    // The cell-anchored text box must be DRAWN (it currently vanishes).
    assert_eq!(
        page.positioned_text_boxes.len(),
        1,
        "cell-anchored text box must be drawn"
    );
    let tb = &page.positioned_text_boxes[0];
    let box_right = tb.x + tb.width_px;
    let (band_top, band_bottom) = (tb.y, tb.y + tb.height_px);
    // The box is drawn at the CELL origin, inside the cell, not the page origin.
    let row = page
        .elements
        .iter()
        .find_map(|e| if let PageElement::TableRow(r) = e { Some(r) } else { None })
        .expect("a table row");
    let cell = &row.cells[0];
    assert!(
        tb.x >= cell.x - 0.5 && tb.x < cell.x + cell.width_px,
        "box x{:.2} must sit inside the cell [x{:.2}, x{:.2}]",
        tb.x, cell.x, cell.x + cell.width_px
    );
    // Cell lines in the box's vertical band must start right of the box.
    let mut in_band = 0;
    for l in &cell.lines {
        if l.baseline_y > band_top && l.baseline_y < band_bottom {
            in_band += 1;
            let first_x = l.segments.first().map(|s| s.x_start).unwrap_or(l.content_left);
            assert!(
                first_x >= box_right - 0.5,
                "cell line in the box band started x{first_x:.2}, not right of the box \
                 (x{box_right:.2}) — text is under the cell float (§20.4.2.17)"
            );
        }
    }
    assert!(in_band >= 2, "expected ≥2 cell lines beside the box, got {in_band}");
    // Bounded below: a cell line under the box resumes at the cell content-left.
    let resumed = cell.lines.iter().any(|l| {
        l.baseline_y > band_bottom
            && l.segments.first().map(|s| s.x_start).unwrap_or(l.content_left) < box_right - 0.5
    });
    assert!(
        resumed,
        "no cell line below the box resumed full width — the cell exclusion is not bounded"
    );
    // The row grew to contain the box (size pass and emit pass agree).
    assert!(
        row.y_top + row.height_px >= band_bottom - 0.5,
        "row [y{:.2}, +h{:.2}] must contain the box bottom y{band_bottom:.2}",
        row.y_top, row.height_px
    );
}

// A cell whose only text is SHORTER than the box: the row must grow to the
// FLOAT's height (content_height = text_bottom.max(float_bottom) — the
// .max(float_bottom) term decides here). Without it the box would overflow the
// row border. cell_textbox_wrap_docx can't catch this (its filler dominates).

fn cell_textbox_tall_grows_docx() -> Vec<u8> {
    const BOX_EMU: i64 = 1_828_800; // 2in × 2in

    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults><w:rPrDefault><w:rPr>
    <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
    <w:sz w:val="22"/><w:szCs w:val="22"/>
  </w:rPr></w:rPrDefault></w:docDefaults>
</w:styles>"#
    );

    let textbox = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV>
  <wp:extent cx="{BOX_EMU}" cy="{BOX_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapSquare wrapText="right"/>
  <wp:docPr id="1" name="TextBox 1"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp>
      <wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_EMU}" cy="{BOX_EMU}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">Boxed.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/>
    </wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );

    // ONE short line of text beside the box — text_bottom << box bottom.
    let body = format!(
        r#"<w:tbl><w:tblPr><w:tblW w:w="9000" w:type="dxa"/></w:tblPr><w:tblGrid><w:gridCol w:w="9000"/></w:tblGrid><w:tr><w:tc><w:tcPr><w:tcW w:w="9000" w:type="dxa"/></w:tcPr><w:p>{textbox}{}</w:p></w:tc></w:tr></w:tbl><w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr>"#,
        r("", "Short caption.")
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}</w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

#[test]
fn cell_textbox_tall_grows_row() {
    let viewer = layout(cell_textbox_tall_grows_docx());
    let pages = viewer.pages.borrow();
    let page = &pages[0];
    assert_eq!(page.positioned_text_boxes.len(), 1, "box must be drawn");
    let tb = &page.positioned_text_boxes[0];
    let box_bottom = tb.y + tb.height_px;
    let row = page
        .elements
        .iter()
        .find_map(|e| if let PageElement::TableRow(r) = e { Some(r) } else { None })
        .expect("a table row");
    // The row is driven by the FLOAT, not the one short line: it must be at
    // least the box height tall and fully contain the box bottom. If
    // content_height ignored the float (text_bottom only), the row would be
    // ~one line tall and the box would overflow.
    assert!(
        row.height_px >= tb.height_px - 0.5,
        "row h{:.2} must be ≥ the box height {:.2} (float drives the row height)",
        row.height_px, tb.height_px
    );
    assert!(
        row.y_top + row.height_px >= box_bottom - 0.5,
        "row [y{:.2}, +h{:.2}] must contain the box bottom y{box_bottom:.2}",
        row.y_top, row.height_px
    );
}

// A wrap-float cell whose paragraph carries a RIGHT indent: the re-packed text
// band must honor the indent (the float-blind path applies it via x_start), so
// in-band lines stop short of the cell content-right by the indent. Guards the
// indent fix in build_cell_float_layout (without it text packs into — and
// renders at — the wrong band width, notably for list/indented cells).

fn cell_textbox_indent_docx() -> Vec<u8> {
    const BOX_EMU: i64 = 1_828_800; // 2in × 2in

    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults><w:rPrDefault><w:rPr>
    <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
    <w:sz w:val="22"/><w:szCs w:val="22"/>
  </w:rPr></w:rPrDefault></w:docDefaults>
</w:styles>"#
    );

    let textbox = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV>
  <wp:extent cx="{BOX_EMU}" cy="{BOX_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapSquare wrapText="right"/>
  <wp:docPr id="1" name="TextBox 1"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp>
      <wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_EMU}" cy="{BOX_EMU}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">Boxed.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/>
    </wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );

    // Paragraph carries a 1in (1440 twip) RIGHT indent.
    let body = format!(
        r#"<w:tbl><w:tblPr><w:tblW w:w="9000" w:type="dxa"/></w:tblPr><w:tblGrid><w:gridCol w:w="9000"/></w:tblGrid><w:tr><w:tc><w:tcPr><w:tcW w:w="9000" w:type="dxa"/></w:tcPr><w:p><w:pPr><w:ind w:right="1440"/></w:pPr>{textbox}{}</w:p></w:tc></w:tr></w:tbl><w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr>"#,
        r("", &filler(0, 6))
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}</w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

#[test]
fn cell_textbox_wrap_honors_indent() {
    let viewer = layout(cell_textbox_indent_docx());
    let pages = viewer.pages.borrow();
    let page = &pages[0];
    assert_eq!(page.positioned_text_boxes.len(), 1, "box must be drawn");
    let row = page
        .elements
        .iter()
        .find_map(|e| if let PageElement::TableRow(r) = e { Some(r) } else { None })
        .expect("a table row");
    let cell = &row.cells[0];
    // The 1in (~96px) right indent must pull every wrapped line's content_right
    // in by that much; without the fix content_right would sit at the cell edge.
    let cell_right = cell.x + cell.width_px;
    let indented = cell.lines.iter().any(|l| l.content_right <= cell_right - 90.0);
    assert!(
        indented,
        "no wrapped cell line honored the ~96px right indent (cell right edge x{cell_right:.2}); \
         content_rights: {:?}",
        cell.lines.iter().map(|l| l.content_right).collect::<Vec<_>>()
    );
    // Sanity: indent never pushes content_right past the cell edge.
    assert!(
        cell.lines.iter().all(|l| l.content_right <= cell_right + 0.5),
        "a line's content_right exceeded the cell edge"
    );
}

// ─── page/margin float anchored BELOW the text it overlaps ──────────────────
// An INSET (narrow) wrapping float with positionV relativeFrom="margin" has a
// cursor-independent Y pinned to the content top, but is registered only at its
// anchor paragraph. Text paragraphs laid BEFORE the anchor on the same page
// fall in the float's band and — before the bounded-reflow fix — render
// full-width UNDER the float. After: those preceding lines wrap beside it.

fn page_float_above_anchor_docx() -> Vec<u8> {
    const BOX_EMU: i64 = 1_828_800; // 2in (~31% of a 6.5in content column → inset)

    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults><w:rPrDefault><w:rPr>
    <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
    <w:sz w:val="22"/><w:szCs w:val="22"/>
  </w:rPr></w:rPrDefault>
  <w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults>
</w:styles>"#
    );

    // Margin-relative-V (pinned to content top), column-relative-H at the left,
    // wrapSquare wrapText="right". Anchored in a LATER paragraph than the text
    // it overlaps.
    let textbox = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="margin"><wp:posOffset>0</wp:posOffset></wp:positionV>
  <wp:extent cx="{BOX_EMU}" cy="{BOX_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapSquare wrapText="right"/>
  <wp:docPr id="1" name="TextBox 1"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp>
      <wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_EMU}" cy="{BOX_EMU}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">Boxed.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/>
    </wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );

    let mut body = String::new();
    // TWO real text paragraphs laid BEFORE the anchor — these overlap the float.
    body.push_str(&p("", &r("", &filler(0, 4))));
    body.push_str(&p("", &r("", &filler(1, 4))));
    // The anchor paragraph (registers the margin-pinned float).
    body.push_str(&format!("<w:p>{textbox}{}</w:p>", r("", &filler(2, 4))));
    body.push_str(
        r#"<w:sectPr>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
      <w:cols w:num="1"/>
    </w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}</w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

#[test]
fn page_float_excludes_text_above_anchor() {
    let viewer = layout(page_float_above_anchor_docx());
    let pages = viewer.pages.borrow();
    let page = &pages[0];
    assert_eq!(page.positioned_text_boxes.len(), 1, "the margin-pinned box must be drawn");
    let tb = &page.positioned_text_boxes[0];
    let box_right = tb.x + tb.width_px;
    let (band_top, band_bottom) = (tb.y, tb.y + tb.height_px);
    // EVERY body line in the float's band — including the two paragraphs laid
    // ABOVE the anchor — must start right of the box, never under it.
    let mut in_band = 0;
    for el in &page.elements {
        if let PageElement::Line(l) = el {
            if l.baseline_y > band_top && l.baseline_y < band_bottom {
                in_band += 1;
                let first_x = l.segments.first().map(|s| s.x_start).unwrap_or(l.content_left);
                assert!(
                    first_x >= box_right - 0.5,
                    "a line at baseline_y{:.2} (in the float band) started x{first_x:.2}, not right \
                     of the box (x{box_right:.2}) — text laid above the anchor is under the float (§20.4.2.17)",
                    l.baseline_y
                );
            }
        }
    }
    assert!(in_band >= 2, "expected ≥2 in-band lines (incl. above the anchor), got {in_band}");
}

// Fixpoint RE-BASELINE (was `fullwidth_float_above_anchor_aborts_to_inert`
// = inert/overlap). A FULL-WIDTH margin-pinned float above real text is now the
// per-page fixpoint: on re-lay the float is in scope from page start, so the
// preceding real paragraph wraps BELOW it (no room beside a full-width band) —
// the old push-down cascade is structurally impossible because push-down
// happens inside normal layout where overflow_col! is legal.
//
// BEFORE (inert, pre-fixpoint): box band [96..192]; para0's 5 lines at
//   baseline_y {109.28,126.88,144.48,162.08,179.68} — ALL INSIDE the band, at
//   x_start=96 (UNDER the full-width box = the overlap). para1 at {205.28..}.
//   pages_relaid=0.
// AFTER (fixpoint live): box band [96..192] UNCHANGED; para0 pushed to
//   {205.28,222.88,240.48,258.08,275.68} — ALL BELOW band_bottom=192; para1 to
//   {301.28..371.68}. Still 1 page, box drawn once. pages_relaid=1.
// The re-baselined assertion pins the wrap invariant (no body line inside the
// band; float drawn once; bounded pages), matching the inset fixtures.

fn fullwidth_float_above_anchor_docx() -> Vec<u8> {
    // Box spans the full ~6.5in content column → spans_full_width true.
    const BOX_W_EMU: i64 = 5_943_600; // 6.5in
    const BOX_H_EMU: i64 = 914_400; // 1in

    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#
    );

    let textbox = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="margin"><wp:posOffset>0</wp:posOffset></wp:positionV>
  <wp:extent cx="{BOX_W_EMU}" cy="{BOX_H_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapTopAndBottom/>
  <wp:docPr id="1" name="TextBox 1"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp><wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_W_EMU}" cy="{BOX_H_EMU}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">Banner.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/></wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );

    let mut body = String::new();
    body.push_str(&p("", &r("", &filler(0, 4))));
    body.push_str(&format!("<w:p>{textbox}{}</w:p>", r("", &filler(1, 4))));
    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}</w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

#[test]
fn fullwidth_float_above_anchor_wraps_below_fixpoint() {
    // Fixpoint behaviour (was `_aborts_to_inert`): the full-width float above real
    // text now WRAPS — the preceding paragraph is pushed BELOW the band (no room
    // beside a full-width float), same class of invariant as the inset fixtures:
    // (1) bounded page count, (2) box drawn exactly once, (3) NO body line inside
    // the float band (the former overlap). See the BEFORE/AFTER dump above.
    let viewer = layout(fullwidth_float_above_anchor_docx());
    let pages = viewer.pages.borrow();
    assert!(pages.len() <= 2, "bounded pages — got {}", pages.len());
    let boxes: usize = pages.iter().map(|p| p.positioned_text_boxes.len()).sum();
    assert_eq!(boxes, 1, "the full-width box is drawn exactly once (not orphaned/duplicated)");
    // No body line's VERTICAL EXTENT may overlap the float band any more (the
    // overlap is gone). Stronger than the old baseline-in-band filter: PagedLine
    // exposes only line_height, so use the conservative box [baseline-line_height,
    // baseline]; the only sound over-inclusion is a line whose baseline sits ≥¼
    // line-height BELOW the band (a genuinely-below full-width line the up-shifted
    // box over-claims). Real ascent ≫ ¼·lh, so no true below-band line false-fails,
    // yet an edge line whose top pokes into the band is still caught.
    for page in pages.iter() {
        for tb in &page.positioned_text_boxes {
            let (band_top, band_bottom) = (tb.y, tb.y + tb.height_px);
            for el in &page.elements {
                if let PageElement::Line(l) = el {
                    let box_top = l.baseline_y - l.line_height;
                    let overlaps = box_top < band_bottom && l.baseline_y > band_top;
                    assert!(
                        !overlaps || l.baseline_y >= band_bottom + 0.25 * l.line_height,
                        "a body line (baseline_y{:.2}, lh{:.2}) VERTICALLY OVERLAPS the full-width \
                         band [{band_top:.2}..{band_bottom:.2}] — the fixpoint failed to push it below",
                        l.baseline_y, l.line_height
                    );
                }
            }
        }
    }
    // The fixpoint fired (re-laid the page once) to achieve the wrap.
    assert_eq!(viewer.layout_stats().pages_relaid, 1, "one batched re-lay pushed the text below");
}

// CASCADE GUARD: a TALL, wide-ish INSET float (thin side strip) pinned near the
// content top, with a preceding paragraph whose first token is WIDER than the
// strip. The replay's clear_below_floats would push that text down to the float
// bottom (near the page bottom) and overflow — firing overflow_col! mid-replay
// (the cascade) unless the fits-guard models clear_below_floats. The fix makes
// the guard faithful, so this aborts-to-inert (or wraps within the page): either
// way ONE page, no cascade, the float still drawn on its page.

fn tall_inset_float_no_cascade_docx() -> Vec<u8> {
    const BOX_W_EMU: i64 = 5_029_200; // 5.5in of a 6.5in column → ~1in right strip
    const BOX_H_EMU: i64 = 7_772_400; // 8.5in of a 9in content area → near bottom

    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#
    );

    let textbox = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="margin"><wp:posOffset>0</wp:posOffset></wp:positionV>
  <wp:extent cx="{BOX_W_EMU}" cy="{BOX_H_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapSquare wrapText="right"/>
  <wp:docPr id="1" name="TextBox 1"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp><wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_W_EMU}" cy="{BOX_H_EMU}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">Tall.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/></wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );

    let mut body = String::new();
    // Preceding paragraph: a long single token (wider than the ~1in strip) + more
    // words → multiple lines that, pushed below an 8.5in float, overflow.
    body.push_str(&p("", &r("",
        "Supercalifragilisticexpialidociousantidisestablishmentarianism and then several \
         more words follow here to make this paragraph span multiple lines of body text.")));
    body.push_str(&format!("<w:p>{textbox}{}</w:p>", r("", &filler(1, 4))));
    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}</w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

#[test]
fn tall_inset_float_does_not_cascade() {
    // The clear_below_floats-fires regime (first token wider than the side strip).
    // The faithful fits-guard models that clear step, so it either aborts-to-inert
    // or wraps-within-page — never a mid-replay overflow_col! that clears
    // page_floats and ORPHANS/duplicates the float. A tall float legitimately
    // pushes the anchor's text to a 2nd page, so we assert the robust invariant:
    // no panic, the float drawn EXACTLY ONCE total, and a bounded page count
    // (a cascade explosion or orphan would break one of these).
    let viewer = layout(tall_inset_float_no_cascade_docx());
    let pages = viewer.pages.borrow();
    let boxes: usize = pages.iter().map(|p| p.positioned_text_boxes.len()).sum();
    assert_eq!(boxes, 1, "the float must be drawn exactly once (not orphaned/duplicated)");
    assert!(pages.len() <= 2, "no cascade explosion — got {} pages", pages.len());
}

// ─── fixpoint fire fixtures ─────────────────────────────────────────────────
// Shared builder: an anchored wrapping text box (wps) at (h_rel,h_off)/(v_rel,
// v_off) sized cx×cy with the given wrap element, plus a "TB" caption.
fn tbox(h_rel: &str, h_off: i64, v_rel: &str, v_off: i64, cx: i64, cy: i64, wrap: &str) -> String {
    format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="{h_rel}"><wp:posOffset>{h_off}</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="{v_rel}"><wp:posOffset>{v_off}</wp:posOffset></wp:positionV>
  <wp:extent cx="{cx}" cy="{cy}"/><wp:effectExtent l="0" t="0" r="0" b="0"/>
  {wrap}
  <wp:docPr id="1" name="TB"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp><wps:cNvSpPr txBox="1"/><wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm>
      <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">TB.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/></wps:wsp>
  </a:graphicData></a:graphic></wp:anchor></w:drawing></w:r>"#
    )
}

// Wrap a body (which supplies its OWN sectPr) in a full wps-namespaced docx.
fn wps_docx(body: &str) -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#);
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}</w:body></w:document>"#);
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

const SECT_1COL: &str = r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr>"#;

// (1) PAGE-relative (not just margin) inset float over two preceding paras.
// Proves the fixpoint's trigger class includes positionV relativeFrom="page".
fn page_relative_inset_wraps_docx() -> Vec<u8> {
    const BOX: i64 = 1_828_800; // 2in inset (of 6.5in column)
    const V: i64 = 1_600_200;   // ~1.75in from PAGE top → band overlaps content top (margin 1in)
    let tb = tbox("column", 0, "page", V, BOX, BOX, r#"<wp:wrapSquare wrapText="right"/>"#);
    let mut body = String::new();
    body.push_str(&p("", &r("", &filler(0, 4))));
    body.push_str(&p("", &r("", &filler(1, 4))));
    body.push_str(&format!("<w:p>{tb}{}</w:p>", r("", &filler(2, 4))));
    body.push_str(SECT_1COL);
    wps_docx(&body)
}

#[test]
fn page_relative_inset_wraps() {
    let viewer = layout(page_relative_inset_wraps_docx());
    let pages = viewer.pages.borrow();
    let page = &pages[0];
    assert_eq!(page.positioned_text_boxes.len(), 1, "the page-pinned box is drawn once");
    let tb = &page.positioned_text_boxes[0];
    let box_right = tb.x + tb.width_px;
    let (band_top, band_bottom) = (tb.y, tb.y + tb.height_px);
    let mut in_band = 0;
    for el in &page.elements {
        if let PageElement::Line(l) = el {
            // VERTICAL-EXTENT overlap (conservative [baseline-line_height, baseline]
            // box) instead of a baseline-in-band filter — closes the edge-line gap.
            let box_top = l.baseline_y - l.line_height;
            if box_top < band_bottom && l.baseline_y > band_top {
                let fx = l.segments.first().map(|s| s.x_start).unwrap_or(l.content_left);
                // Either the line cleared the box, or it is genuinely below the band
                // (baseline ≥ ¼·lh past band_bottom — the conservative box's only
                // sound over-inclusion). Edge lines just below band_bottom cannot
                // take the escape and are held to x_start ≥ box_right.
                let below_band = l.baseline_y >= band_bottom + 0.25 * l.line_height;
                assert!(fx >= box_right - 0.5 || below_band,
                    "line at y{:.2} (lh{:.2}) vertically overlaps the band but started x{fx:.2}, \
                     not right of the box (x{box_right:.2}) — a PAGE-relative float did not \
                     enter the fixpoint", l.baseline_y, l.line_height);
                if l.baseline_y < band_bottom { in_band += 1; }
            }
        }
    }
    assert!(in_band >= 2, "expected ≥2 in-band lines (page-V arms the fixpoint), got {in_band}");
    assert!(viewer.layout_stats().pages_relaid >= 1, "the page-relative float must re-lay the page");
}

// (2a) MULTI-COLUMN, DEEP col-0 → CASCADE-TO-DEMOTION. A page-relative inset
// float over column-0's x-band, anchored in a column-1 paragraph. Column 0 is
// FULL (5×6 + 3×2 sentences) so narrowing it around the box cascades the col-1
// anchor off the page — the fixpoint legitimately DEMOTES (follow-the-anchor):
// a full 2-column page with a col-1 anchor over a full col-0 cannot side-wrap
// without pushing its own anchor. This fixture PINS that cascade outcome; the
// CLEAN cross-column exclusion (no demotion) is the SHALLOW fixture (2b).
fn multicol_page_float_cascade_docx() -> Vec<u8> {
    const BOX: i64 = 1_371_600; // 1.5in wide, sits in column 0's left x-band
    const V: i64 = 1_600_200;   // 1.75in from PAGE top → over column-0 content
    // Float H at page offset 1in → inside column 0's x-band (left ~3.4in half).
    let tb = tbox("page", 914_400, "page", V, BOX, BOX, r#"<wp:wrapSquare wrapText="right"/>"#);
    let mut body = String::new();
    // Five 6-sentence paras fill column 0 top-to-bottom (its text spans the float's
    // 1.75in..3.25in band). Three small paras then sit atop column 1 so the anchor
    // lands DEEP in column 1 (content above it there) — the cross-column detection
    // the PAGE-scoped block_tops + page tier make visible.
    for i in 0..5 { body.push_str(&p("", &r("", &filler(i, 6)))); }
    for i in 5..8 { body.push_str(&p("", &r("", &filler(i, 2)))); }
    body.push_str(&format!("<w:p>{tb}{}</w:p>", r("", &filler(8, 2))));
    body.push_str(r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="2" w:space="720"/></w:sectPr>"#);
    wps_docx(&body)
}

#[test]
fn multicol_page_float_cascades_to_demotion() {
    // The CROSS-COLUMN FIRE, DEEP case: a page float anchored in column 1 whose
    // band sits over column-0 text (laid BEFORE the anchor, in the other column).
    // The fixpoint page tier (persists across the column advance) + PAGE-scoped
    // block_tops make that overlap VISIBLE to seal_page — it fires and re-lays.
    // Column 0 is FULL, so pre-registering the float cascades the col-1 anchor off
    // the page and the fixpoint DEMOTES (follow-the-anchor). We pin that exact
    // outcome: detected, ≥1 demotion, single draw, bounded re-lays.
    let viewer = layout(multicol_page_float_cascade_docx());
    let pages = viewer.pages.borrow();
    let boxes: usize = pages.iter().map(|p| p.positioned_text_boxes.len()).sum();
    assert_eq!(boxes, 1, "the page float is drawn exactly once across columns");
    let stats = viewer.layout_stats();
    assert!(stats.violations_detected >= 1,
        "the cross-column overlap must be DETECTED (page-scoped block_tops + page tier)");
    assert!(stats.pages_relaid >= 1, "the page float must re-lay its page");
    assert!(stats.floats_demoted >= 1,
        "a FULL col-0 cascades the col-1 anchor off-page → the float must demote");
    let pages_n = pages.len() as u32;
    assert!(stats.pages_relaid <= 3 * pages_n + 1,
        "re-lays stay within the per-page fixpoint bound (got {})", stats.pages_relaid);
}

// (2b) MULTI-COLUMN, SHALLOW col-0 → CLEAN CROSS-COLUMN EXCLUSION. Column 0 is
// filled just enough to (i) reach into the float's 1.75in..3.25in band and (ii)
// spill the anchor paragraph into column 1, but column 1 is otherwise EMPTY, so
// pre-registering the float never pushes the anchor off the page — the fixpoint
// EXCLUDES rather than demotes. This witnesses the headline feature: col-0 text
// laid BEFORE a col-1-anchored page float is pushed OUT of the float's x-band.
//
// (There is no column-break element in the engine, so col-0 must FILL for the
// anchor to reach col-1; "shallow" here means shallow relative to the deep
// cascade fixture — col-1 carries only the short anchor, giving ~9in of slack so
// the exclusion's extra lines cannot cascade the anchor off-page.)
fn multicol_page_float_shallow_docx() -> Vec<u8> {
    const BOX: i64 = 1_371_600; // 1.5in wide, in column 0's left x-band [1in..2.5in]
    const V: i64 = 1_600_200;   // 1.75in from PAGE top → band [1.75in..3.25in]
    let tb = tbox("page", 914_400, "page", V, BOX, BOX, r#"<wp:wrapSquare wrapText="right"/>"#);
    let mut body = String::new();
    // Fill column 0 (six 4-sentence paras ≈ one full 9in column) so the anchor
    // paragraph spills to column 1's TOP; nothing follows it, so column 1 keeps
    // ~9in of slack. Paras 0..1 span the float's band → in-band col-0 lines exist.
    for i in 0..6 { body.push_str(&p("", &r("", &filler(i, 4)))); }
    body.push_str(&format!("<w:p>{tb}{}</w:p>", r("", &filler(6, 2))));
    body.push_str(r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="2" w:space="720"/></w:sectPr>"#);
    wps_docx(&body)
}

#[test]
fn multicol_page_float_excludes_other_column() {
    // THE HEADLINE FEATURE: a page float anchored in column 1 whose band sits over
    // column-0 text pushes that col-0 text OUT of the float's x-band — a CLEAN
    // cross-column win with NO demotion (col-1 has slack, so the anchor stays on
    // the page). Only the page tier (persists across the column advance) can
    // deliver this.
    let viewer = layout(multicol_page_float_shallow_docx());
    let pages = viewer.pages.borrow();
    let stats = viewer.layout_stats();
    let boxes: usize = pages.iter().map(|p| p.positioned_text_boxes.len()).sum();
    assert_eq!(boxes, 1, "the page float is drawn exactly once across columns");
    assert_eq!(stats.floats_demoted, 0, "the CLEAN cross-column win must NOT demote");
    assert!(stats.pages_relaid >= 1, "the page float must re-lay its page to exclude col-0");
    // Locate the box and its x-band. Column 1 starts at 4.5in = 432px; a col-0
    // line has x_start well left of that. The box occupies [1in..2.5in] = [96..240].
    let box_tb = pages.iter().flat_map(|p| p.positioned_text_boxes.iter()).next().expect("box drawn");
    let box_right = box_tb.x + box_tb.width_px;
    let (band_top, band_bottom) = (box_tb.y, box_tb.y + box_tb.height_px);
    const COL1_LEFT_PX: f64 = 432.0; // 4.5in
    let mut pushed = 0;
    for page in pages.iter() {
        for el in &page.elements {
            if let PageElement::Line(l) = el {
                let fx = l.segments.first().map(|s| s.x_start).unwrap_or(l.content_left);
                let is_col0 = fx < COL1_LEFT_PX;
                // VERTICAL-EXTENT overlap (conservative [baseline-line_height,
                // baseline] box); ¼·lh below-band escape exempts genuinely-below
                // full-width lines (see the inset fixture).
                let box_top = l.baseline_y - l.line_height;
                let overlaps = box_top < band_bottom && l.baseline_y > band_top;
                if is_col0 && overlaps {
                    let below_band = l.baseline_y >= band_bottom + 0.25 * l.line_height;
                    assert!(fx >= box_right - 0.5 || below_band,
                        "col-0 line at y{:.2} (lh{:.2}) intersects the float band but started \
                         x{fx:.2}, not right of the box (x{box_right:.2}) — col-0 text was NOT \
                         pushed out of the float's x-band", l.baseline_y, l.line_height);
                    if fx >= box_right - 0.5 && l.baseline_y < band_bottom { pushed += 1; }
                }
            }
        }
    }
    assert!(pushed >= 1,
        "expected ≥1 col-0 line pushed out of the float's x-band (the cross-column win), got {pushed}");
}

// (3) TWO page floats on the SAME page: witnesses the BATCHED union (one bounded
// set of re-lays, not two independent rollback storms).
fn two_page_floats_docx() -> Vec<u8> {
    const BOX: i64 = 1_828_800; // 2in inset
    let tb_a = tbox("column", 0, "page", 1_600_200, BOX, BOX, r#"<wp:wrapSquare wrapText="right"/>"#);
    let tb_b = tbox("column", 0, "page", 3_886_200, BOX, BOX, r#"<wp:wrapSquare wrapText="right"/>"#); // ~4.25in
    let mut body = String::new();
    body.push_str(&p("", &r("", &filler(0, 4))));
    body.push_str(&format!("<w:p>{tb_a}{}</w:p>", r("", &filler(1, 4))));
    for i in 2..6 { body.push_str(&p("", &r("", &filler(i, 4)))); }
    body.push_str(&format!("<w:p>{tb_b}{}</w:p>", r("", &filler(6, 4))));
    body.push_str(SECT_1COL);
    wps_docx(&body)
}

#[test]
fn two_page_floats_same_page() {
    let viewer = layout(two_page_floats_docx());
    let pages = viewer.pages.borrow();
    let boxes: usize = pages.iter().map(|p| p.positioned_text_boxes.len()).sum();
    assert_eq!(boxes, 2, "both page floats drawn exactly once");
    // CO-RESIDENCY: both floats live on the SAME page, so this fixture actually
    // witnesses the BATCHED-union single-page relay (not two independent
    // single-float relays on two pages).
    assert_eq!(pages.len(), 1, "both page floats must be co-resident on one page");
    assert_eq!(pages[0].positioned_text_boxes.len(), 2, "both boxes on the same page");
    // Every band-overlapping line clears its own box (vertical-extent overlap via
    // the conservative [baseline-line_height, baseline] box; the ¼·lh below-band
    // escape exempts only genuinely-below full-width lines — see the inset fixture).
    for page in pages.iter() {
        for tb in &page.positioned_text_boxes {
            let br = tb.x + tb.width_px;
            let (bt, bb) = (tb.y, tb.y + tb.height_px);
            for el in &page.elements {
                if let PageElement::Line(l) = el {
                    let box_top = l.baseline_y - l.line_height;
                    if box_top < bb && l.baseline_y > bt {
                        let fx = l.segments.first().map(|s| s.x_start).unwrap_or(l.content_left);
                        let below_band = l.baseline_y >= bb + 0.25 * l.line_height;
                        assert!(fx >= br - 0.5 || below_band,
                            "band-overlapping line y{:.2} (lh{:.2}) x{fx:.2} under box (right {br:.2})",
                            l.baseline_y, l.line_height);
                    }
                }
            }
        }
    }
    let n = 2u32; // two floats
    let pages_n = pages.len() as u32;
    assert!(viewer.layout_stats().pages_relaid <= 2 * pages_n + n,
        "batched union: pages_relaid {} exceeds 2*{pages_n}+{n}", viewer.layout_stats().pages_relaid);
}

// (4) OVER-TALL page float: pre-registering it pushes its own anchor off the page,
// so the demotion path (follow the anchor) fires. Asserts floats_demoted and that
// the float lands on its anchor's page (final float page == final anchor page).
fn overtall_page_float_docx() -> Vec<u8> {
    const BOXW: i64 = 5_943_600; // 6.5in — FULL width: no room to wrap beside
    const BOXH: i64 = 7_772_400; // 8.5in — near the full content area
    const V: i64 = 457_200;      // 0.5in from PAGE top → band [0.5in..9in]
    // A full-width 8.5in float pinned near the top: on re-lay every block wraps
    // BELOW it (only ~0.5in left under the band), so pre-registering it pushes the
    // anchor's own paragraph off the page → the follow-the-anchor demotion fires.
    let tb = tbox("column", 0, "page", V, BOXW, BOXH, r#"<wp:wrapSquare wrapText="left"/>"#);
    let mut body = String::new();
    for i in 0..24 { body.push_str(&p("", &r("", &filler(i, 6)))); }
    body.push_str(&format!("<w:p>{tb}{}</w:p>", r("", &filler(24, 6))));
    body.push_str(SECT_1COL);
    wps_docx(&body)
}

#[test]
fn overtall_page_float_demotes() {
    let viewer = layout(overtall_page_float_docx());
    let stats = viewer.layout_stats();
    assert_eq!(stats.floats_demoted, 1, "the over-tall float must demote exactly once");
    // Float drawn exactly once, on the SAME page its anchor paragraph starts on.
    let pages = viewer.pages.borrow();
    let boxes: usize = pages.iter().map(|p| p.positioned_text_boxes.len()).sum();
    assert_eq!(boxes, 1, "demoted float drawn exactly once");
    let box_page = pages.iter().position(|p| !p.positioned_text_boxes.is_empty())
        .expect("box drawn somewhere");
    // PIN "final float page == final anchor page": the anchor paragraph is
    // filler(24, 6), whose text includes S[4] ("Headers and footers occupy their
    // reserved bands..."). The demoted float follows its anchor, so that exact
    // text must appear on the box's page — strictly stronger than "box page has
    // any body lines".
    let anchor_seg = "Headers and footers occupy their reserved bands";
    assert!(filler(24, 6).contains(anchor_seg), "sanity: anchor filler must contain the probe segment");
    let box_page_text: String = pages[box_page].elements.iter()
        .filter_map(|el| if let PageElement::Line(l) = el { Some(line_text(l)) } else { None })
        .collect::<Vec<_>>().join(" ");
    assert!(box_page_text.contains(anchor_seg),
        "the demoted float's page must carry its anchor paragraph's text \
         (final float page == anchor page)");
}

// ─── violation-detector precision/recall battery ─────────────────────────────
// The seal-time detector (lib.rs `seal_page`) must FIRE on exactly the
// cursor-independent floats whose frozen band overlaps content emitted above
// their anchor — the class the fixpoint rolls back — and stay SILENT
// on everything else. Precision/recall is validated against a LABELED battery
// below (EXPECT_FIRE with the expected FloatIds vs EXPECT_SILENT). A mismatch
// is a REAL detector bug (bad recall from reading post-repair pixels is the
// likely one) — fix the predicate, never relabel a fixture.

// STRONG precision witness: earlier content EXISTS (blocks 0,1) and the
// page-pinned float sits genuinely BELOW its anchor (band 5..7in, anchor top
// ~2.8in) — exercises the y-comparison step, not merely "anchor is first
// block". Arms the gate (page-relative wrapSquare ⇒ checkpoints_captured>0, a
// REAL negative) yet the frozen band is below anchor_top ⇒ overlaps_band false
// ⇒ SILENT.
fn page_float_below_anchor_docx() -> Vec<u8> {
    const BOX_EMU: i64 = 1_828_800; // 2in inset
    const PAGE_V_EMU: i64 = 4_572_000; // 5in from PAGE top → band 5..7in
    let textbox = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="page"><wp:posOffset>{PAGE_V_EMU}</wp:posOffset></wp:positionV>
  <wp:extent cx="{BOX_EMU}" cy="{BOX_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapSquare wrapText="right"/>
  <wp:docPr id="1" name="TextBox 1"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp><wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_EMU}" cy="{BOX_EMU}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">Below.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/></wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );
    let mut body = String::new();
    body.push_str(&p("", &r("", &filler(0, 4))));
    body.push_str(&p("", &r("", &filler(1, 4))));
    body.push_str(&format!("<w:p>{textbox}{}</w:p>", r("", &filler(2, 4))));
    perf_docx(&body)
}

// A page-relative box with <wp:wrapNone/> (a watermark-class float): excluded
// by doc_has_page_margin_wrap_float (the WrapMode::None filter) AND never
// registered into scope.column_floats ⇒ gate-false ⇒ SILENT.
fn wrapnone_page_float_docx() -> Vec<u8> {
    const BOX_EMU: i64 = 1_828_800;
    const PAGE_V_EMU: i64 = 2_743_200; // 3in from page top
    let textbox = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="1" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="page"><wp:posOffset>{PAGE_V_EMU}</wp:posOffset></wp:positionV>
  <wp:extent cx="{BOX_EMU}" cy="{BOX_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapNone/>
  <wp:docPr id="1" name="TextBox 1"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp><wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_EMU}" cy="{BOX_EMU}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">WM.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/></wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );
    let mut body = String::new();
    body.push_str(&p("", &r("", &filler(0, 4))));
    body.push_str(&format!("<w:p>{textbox}{}</w:p>", r("", &filler(1, 4))));
    perf_docx(&body)
}

#[test]
fn page_float_below_anchor_is_a_real_negative() {
    // The gate is ARMED (page-relative wrapSquare ⇒ checkpoints captured), so
    // SILENT here proves the y-comparison, not a gate-false accident.
    let viewer = layout(page_float_below_anchor_docx());
    assert!(viewer.violation_log.borrow().is_empty(),
        "a page float BELOW its anchor must not fire (band is below anchor_top)");
    assert!(viewer.layout_stats().checkpoints_captured > 0,
        "page-relative float must arm the gate — else this is a gate-false, not a real negative");
}

#[cfg(test)]
enum Expect { Fire(Vec<(usize, Vec<super::FloatId>)>), Silent }

fn fid(b: usize, k: super::AnchorKind, w: usize) -> super::FloatId {
    super::FloatId { section_idx: 0, block_idx: b, kind: k, within_idx: w }
}

#[test]
fn violation_detector_precision_recall() {
    use super::AnchorKind::TextBox;
    let battery: Vec<(&str, fn() -> Vec<u8>, Expect)> = vec![
        ("page_float_excludes_text_above_anchor", page_float_above_anchor_docx,
            Expect::Fire(vec![(0, vec![fid(2, TextBox, 0)])])),
        // THE CRUX: the detector MUST fire on the Block-4-abort full-width case
        // (post-repair pixels look clean; the structural predicate does not).
        ("fullwidth_float_above_anchor_aborts_to_inert", fullwidth_float_above_anchor_docx,
            Expect::Fire(vec![(0, vec![fid(1, TextBox, 0)])])),
        ("tall_inset_float_does_not_cascade", tall_inset_float_no_cascade_docx,
            Expect::Fire(vec![(0, vec![fid(1, TextBox, 0)])])),
        ("page_float_below_anchor", page_float_below_anchor_docx, Expect::Silent),
        ("wrapnone_page_float", wrapnone_page_float_docx, Expect::Silent),
        ("float_free", (|| perf_docx(&perf_float_free_body(40))) as fn() -> Vec<u8>, Expect::Silent),
        ("paragraph_float_heavy", (|| perf_docx(&perf_float_heavy_body(40))) as fn() -> Vec<u8>, Expect::Silent),
    ];
    for (name, build, expect) in battery {
        let viewer = layout(build());
        let mut got: Vec<(usize, Vec<super::FloatId>)> = viewer.violation_log.borrow().iter()
            .map(|(p, fs)| { let mut v = fs.clone(); v.sort(); (*p, v) }).collect();
        got.sort_by_key(|(p, _)| *p);
        let vd = viewer.layout_stats().violations_detected;
        match expect {
            Expect::Silent => assert!(got.is_empty() && vd == 0,
                "{name}: expected SILENT, got {got:?} (violations_detected={vd})"),
            Expect::Fire(mut want) => {
                for (_, v) in want.iter_mut() { v.sort(); }
                want.sort_by_key(|(p, _)| *p);
                assert_eq!(got, want, "{name}: violation set mismatch");
                assert_eq!(vd, want.len() as u32,
                    "{name}: violations_detected {vd} != {} firing pages", want.len());
            }
        }
    }
}

// ─── comments (Word comments: anchors and metadata) ──────────────────────────
// Builds a docx with two comment ranges (distinct authors) + a word/comments.xml
// part, lays it out natively, and asserts `resolve_comments()` joins each
// comment's metadata to its range-start {page, y}. Guards the wml-comments
// feature (date/initials), the CommentRangeStart → synthetic-anchor stamp, the
// bookmark_position-style page-walk, the multi-paragraph body join, and document
// order. Sibling of the wasm `comments()` accessor, which is the JS envelope
// over this exact `resolve_comments()`.

fn comments_oracle_docx() -> Vec<u8> {
    // Comment 0 (Ada) flags the opening sentence; comment 1 (Grace) flags a span
    // several paragraphs later and carries a two-paragraph body; comments 2 + 3
    // exercise OVERLAPPING shared-endpoint ranges (the find-all-hits /
    // stacking path); comment 4 is a TABLE-CELL comment (highlight parity via the
    // cell-line walk). The range spanned by comment 0 also CROSSES a paragraph
    // boundary (start in para 1, end in para 2) to exercise the cross-paragraph
    // open-id carrier.
    let mut body = String::new();
    // Comment 0 opens here and closes in the NEXT paragraph (cross-paragraph span).
    body.push_str(&format!(
        "<w:p>{}{}</w:p>",
        r#"<w:commentRangeStart w:id="0"/>"#,
        r("", "This opening sentence is under review."),
    ));
    body.push_str(&format!(
        "<w:p>{}{}{}</w:p>",
        r("", "It continues into a second paragraph "),
        r#"<w:commentRangeEnd w:id="0"/><w:r><w:commentReference w:id="0"/></w:r>"#,
        r("", "and then runs on uncommented."),
    ));
    body.push_str(&p("", &r("", &filler(1, 4))));
    body.push_str(&p("", &r("", &filler(2, 4))));
    body.push_str(&format!(
        "<w:p>{}{}{}{}</w:p>",
        r("", "Earlier text, then "),
        r#"<w:commentRangeStart w:id="1"/>"#,
        r("", "a span flagged by the second reviewer") + r#"<w:commentRangeEnd w:id="1"/><w:r><w:commentReference w:id="1"/></w:r>"#,
        r("", ", then trailing text."),
    ));
    // Overlapping shared-endpoint ranges: comment 2 covers "alpha beta"; comment 3
    // opens INSIDE it (before "beta") and extends past it through "gamma". The
    // "beta" run is covered by BOTH → its own {2,3} rect between the {2}-only and
    // {3}-only rects (the stacking case the accessor must split).
    body.push_str(&format!(
        "<w:p>{}{}{}{}{}{}{}{}</w:p>",
        r#"<w:commentRangeStart w:id="2"/>"#,
        r("", "alpha "),
        r#"<w:commentRangeStart w:id="3"/>"#,
        r("", "beta "),
        r#"<w:commentRangeEnd w:id="2"/><w:r><w:commentReference w:id="2"/></w:r>"#,
        r("", "gamma"),
        r#"<w:commentRangeEnd w:id="3"/><w:r><w:commentReference w:id="3"/></w:r>"#,
        r("", " delta."),
    ));
    // Table-cell comment (comment 4): the range lives entirely inside one cell.
    body.push_str(r#"<w:tbl><w:tblPr><w:tblW w:w="9360" w:type="dxa"/></w:tblPr><w:tblGrid><w:gridCol w:w="4680"/><w:gridCol w:w="4680"/></w:tblGrid>"#);
    body.push_str("<w:tr>");
    body.push_str(&format!(
        "<w:tc><w:tcPr><w:tcW w:w=\"4680\" w:type=\"dxa\"/></w:tcPr><w:p>{}{}{}</w:p></w:tc>",
        r#"<w:commentRangeStart w:id="4"/>"#,
        r("", "Commented cell text"),
        r#"<w:commentRangeEnd w:id="4"/><w:r><w:commentReference w:id="4"/></w:r>"#,
    ));
    body.push_str(&format!("<w:tc><w:tcPr><w:tcW w:w=\"4680\" w:type=\"dxa\"/></w:tcPr>{}</w:tc>", p("", &r("", "Plain neighbour cell"))));
    body.push_str("</w:tr></w:tbl>");
    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );

    let comments = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:comments {W_NS}>
  <w:comment w:id="0" w:author="Ada Lovelace" w:date="2026-06-01T10:15:00Z" w:initials="AL">
    <w:p><w:r><w:t xml:space="preserve">This opening sentence could be punchier.</w:t></w:r></w:p>
  </w:comment>
  <w:comment w:id="1" w:author="Grace Hopper" w:date="2026-06-02T14:30:00Z" w:initials="GH">
    <w:p><w:r><w:t xml:space="preserve">Good point in the middle here.</w:t></w:r></w:p>
    <w:p><w:r><w:t xml:space="preserve">Second paragraph of the same comment.</w:t></w:r></w:p>
  </w:comment>
  <w:comment w:id="2" w:author="Ada Lovelace" w:date="2026-06-03T09:00:00Z" w:initials="AL">
    <w:p><w:r><w:t xml:space="preserve">Outer of the overlapping pair.</w:t></w:r></w:p>
  </w:comment>
  <w:comment w:id="3" w:author="Grace Hopper" w:date="2026-06-03T09:05:00Z" w:initials="GH">
    <w:p><w:r><w:t xml:space="preserve">Inner of the overlapping pair.</w:t></w:r></w:p>
  </w:comment>
  <w:comment w:id="4" w:author="Ada Lovelace" w:date="2026-06-04T11:00:00Z" w:initials="AL">
    <w:p><w:r><w:t xml:space="preserve">A comment anchored inside a table cell.</w:t></w:r></w:p>
  </w:comment>
</w:comments>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/comments.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml"/>
</Types>"#
        .to_string();

    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#
        .to_string();

    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>
</Relationships>"#
        .to_string();

    build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/comments.xml", comments),
    ])
}

#[test]
fn comments_join_metadata_to_range_start_position() {
    let viewer = layout(comments_oracle_docx());
    let resolved = viewer.resolve_comments();

    assert_eq!(resolved.len(), 5, "all five comment ranges should resolve");

    // Document order: Ada's comment (id 0) first, Grace's (id 1) second.
    let (ada, ada_page, ada_y) = &resolved[0];
    assert_eq!(ada.id, 0);
    assert_eq!(ada.author, "Ada Lovelace");
    assert_eq!(ada.date.as_deref(), Some("2026-06-01T10:15:00Z"));
    assert_eq!(ada.initials.as_deref(), Some("AL"));
    assert_eq!(ada.text, "This opening sentence could be punchier.");
    assert_eq!(*ada_page, 0);

    let (grace, _grace_page, grace_y) = &resolved[1];
    assert_eq!(grace.id, 1);
    assert_eq!(grace.author, "Grace Hopper");
    assert_eq!(grace.initials.as_deref(), Some("GH"));
    // Multi-paragraph body joins with '\n'.
    assert_eq!(
        grace.text,
        "Good point in the middle here.\nSecond paragraph of the same comment."
    );

    // The two range starts are on different lines: Grace's is below Ada's.
    assert!(
        *grace_y > *ada_y,
        "second comment anchorY ({grace_y:.1}) should be below first ({ada_y:.1})"
    );
}

/// `collect_comment_rects` is the post-layout pairing/overlap accessor.
/// Walk every page's rects and assert: (1) id-pairing — each range yields rects
/// carrying exactly its id; (2) cross-paragraph span — comment 0's range yields
/// rects on >1 distinct line (the start is in one paragraph, the end in the
/// next); (3) overlap → distinct id-sets — the {2,3} region is its own rect,
/// flanked by {2}-only and {3}-only rects; (4) table-cell parity — comment 4
/// (entirely inside a cell) still yields a rect; (5) the per-author colour maps
/// (id 2/Ada vs id 3/Grace differ).
#[test]
fn comment_rects_pair_overlap_and_walk_table_cells() {
    let viewer = layout(comments_oracle_docx());
    let pages = viewer.pages.borrow();
    // Flatten every page's rects (page index carried for the cross-paragraph
    // assertion, though all content here fits one page).
    let mut rects: Vec<super::CommentRect> = Vec::new();
    for page in pages.iter() {
        rects.extend(viewer.collect_comment_rects(page));
    }

    let with_ids = |ids: &[i64]| rects.iter().filter(move |r| r.ids == ids).count();
    let any_with = |id: i64| rects.iter().filter(move |r| r.ids.contains(&id)).count();

    // Every range produced at least one rect.
    for id in 0..=4 {
        assert!(any_with(id) > 0, "comment {id} should yield >=1 highlight rect");
    }

    // (2) Cross-paragraph span: comment 0's rects sit on >1 distinct y.
    let ys_0: std::collections::BTreeSet<i64> = rects
        .iter()
        .filter(|r| r.ids == [0])
        .map(|r| (r.y * 100.0) as i64)
        .collect();
    assert!(
        ys_0.len() >= 2,
        "comment 0 spans two paragraphs → rects on >=2 lines, got {}",
        ys_0.len()
    );

    // (3) Overlap: the shared region carries BOTH ids as its own rect, and the
    // {2}-only and {3}-only flanks each exist.
    assert!(with_ids(&[2]) >= 1, "comment 2 should have a {{2}}-only rect (alpha)");
    assert!(with_ids(&[3]) >= 1, "comment 3 should have a {{3}}-only rect (gamma)");
    assert!(
        with_ids(&[2, 3]) >= 1,
        "the overlap region (beta) should be its own {{2,3}} rect — got rects: {:?}",
        rects.iter().map(|r| r.ids.clone()).collect::<Vec<_>>()
    );

    // (4) Table-cell parity: comment 4 lives entirely inside a cell line; the
    // cell-line walk must still surface it.
    assert!(with_ids(&[4]) >= 1, "table-cell comment 4 should yield a rect");

    // (5) Per-author colour suggestion differs across authors (Ada id 2 vs
    // Grace id 3) and is non-empty.
    let color_for = |ids: &[i64]| rects.iter().find(|r| r.ids == ids).map(|r| r.color);
    let c2 = color_for(&[2]).expect("id-2 rect");
    let c3 = color_for(&[3]).expect("id-3 rect");
    assert!(c2.starts_with('#') && c3.starts_with('#'), "colours are #rrggbb");
    assert_ne!(c2, c3, "different authors → different colour suggestion");

    // Every rect has positive extent (no zero-width/-height highlights leak out).
    for r in &rects {
        assert!(r.w > 0.0 && r.h > 0.0, "degenerate rect {:?}", (r.x, r.y, r.w, r.h));
    }
}

// ─── row split across page breaks (§17.4.6/§17.4.7) ──────────────────────────
// A single-section single-column doc with: a short control row; a 2-cell row
// whose first cell holds ~60 paragraphs (far taller than one Letter content
// area) so it MUST split across >2 pages; and a cantSplit row of ~25 lines that
// must be pushed WHOLE to a fresh page. Asserts the splittable row fragments
// across pages with deferred bottom / repeated top+side borders, and the
// cantSplit row is never fragmented.

fn row_split_docx() -> Vec<u8> {
    let borders = r#"<w:tblBorders><w:top w:val="single" w:sz="4"/><w:left w:val="single" w:sz="4"/><w:bottom w:val="single" w:sz="4"/><w:right w:val="single" w:sz="4"/><w:insideH w:val="single" w:sz="4"/><w:insideV w:val="single" w:sz="4"/></w:tblBorders>"#;
    let mut body = String::new();
    body.push_str(&format!(
        r#"<w:tbl><w:tblPr><w:tblW w:w="9360" w:type="dxa"/>{borders}</w:tblPr><w:tblGrid><w:gridCol w:w="4680"/><w:gridCol w:w="4680"/></w:tblGrid>"#
    ));
    // Row A — control (one line per cell).
    body.push_str(&format!(
        "<w:tr><w:tc><w:tcPr><w:tcW w:w=\"4680\" w:type=\"dxa\"/></w:tcPr>{}</w:tc><w:tc><w:tcPr><w:tcW w:w=\"4680\" w:type=\"dxa\"/></w:tcPr>{}</w:tc></w:tr>",
        p("", &r("", "Header L")), p("", &r("", "Header R")),
    ));
    // Row B — MUST split: ~120 short paragraphs in the (shaded) left cell, far
    // taller than two Letter content areas, so it spans >2 pages and exercises
    // the multi-page continuation loop.
    let mut tall = String::new();
    for k in 0..120 {
        tall.push_str(&p("", &r("", &format!("Line {k} of the tall splittable cell"))));
    }
    body.push_str(&format!(
        "<w:tr><w:tc><w:tcPr><w:tcW w:w=\"4680\" w:type=\"dxa\"/><w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"FDE9D9\"/></w:tcPr>{}</w:tc><w:tc><w:tcPr><w:tcW w:w=\"4680\" w:type=\"dxa\"/></w:tcPr>{}</w:tc></w:tr>",
        tall, p("", &r("", "right side of tall row")),
    ));
    // Row C — cantSplit, ~25 lines: tall enough to need a fresh page but never
    // fragmented.
    let mut cant = String::new();
    for k in 0..25 {
        cant.push_str(&p("", &r("", &format!("CantSplit line {k}"))));
    }
    body.push_str(&format!(
        "<w:tr><w:trPr><w:cantSplit/></w:trPr><w:tc><w:tcPr><w:tcW w:w=\"4680\" w:type=\"dxa\"/></w:tcPr>{}</w:tc><w:tc><w:tcPr><w:tcW w:w=\"4680\" w:type=\"dxa\"/></w:tcPr>{}</w:tc></w:tr>",
        cant, p("", &r("", "cantSplit right")),
    ));
    body.push_str("</w:tbl>");
    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );
    // docDefaults pin the run font to Arimo (a sha-pinned test font), so native
    // measurement never reaches a JS font-shaping import.
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults><w:rPrDefault><w:rPr>
    <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
    <w:sz w:val="22"/><w:szCs w:val="22"/>
  </w:rPr></w:rPrDefault></w:docDefaults>
</w:styles>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#.to_string();
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#.to_string();
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#.to_string();

    build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
    ])
}

#[test]
fn row_split_across_page_breaks() {
    use super::wml::page::PageElement;
    let viewer = layout(row_split_docx());
    let pages = viewer.pages.borrow();
    assert!(pages.len() >= 3, "tall split row should span >2 pages (got {})", pages.len());

    // Collect every table-row fragment in page order, tagging its page index.
    let mut frags: Vec<(usize, &super::wml::page::PagedTableRow)> = Vec::new();
    for (pi, page) in pages.iter().enumerate() {
        for el in &page.elements {
            if let PageElement::TableRow(r) = el {
                frags.push((pi, r));
            }
        }
    }
    assert!(frags.len() >= 4, "expected control + ≥2 split fragments + cantSplit (got {})", frags.len());

    // The tall splittable row's fragments are the ones whose left (shaded) cell
    // carries the FDE9D9 fill. They must span ≥2 distinct pages, repeat the fill
    // on every fragment, defer the bottom border (None) on all but the last
    // fragment, and drop the top border (None) on all but the first.
    let shaded: Vec<&(usize, &super::wml::page::PagedTableRow)> = frags.iter()
        .filter(|(_, r)| r.cells.first().and_then(|c| c.fill_color.as_deref()) == Some("#FDE9D9"))
        .collect();
    assert!(shaded.len() >= 2, "tall row did not split (got {} shaded fragments)", shaded.len());
    let split_pages: std::collections::BTreeSet<usize> = shaded.iter().map(|(pi, _)| *pi).collect();
    assert!(split_pages.len() >= 2, "split fragments share one page — no real break");

    let last = shaded.len() - 1;
    for (i, (_, r)) in shaded.iter().enumerate() {
        let lc = &r.cells[0];
        assert_eq!(lc.fill_color.as_deref(), Some("#FDE9D9"), "fill must repeat on every fragment");
        // Left/right side borders repeat on every fragment.
        assert!(lc.border_left.is_some(), "left border must repeat (fragment {i})");
        if i != last {
            assert!(lc.border_bottom.is_none(), "non-final fragment must defer bottom border (fragment {i})");
        } else {
            assert!(lc.border_bottom.is_some(), "final fragment must draw the real bottom border");
        }
        if i != 0 {
            assert!(lc.border_top.is_none(), "continuation fragment must drop the top border (fragment {i})");
        }
    }

    // The cantSplit row (its left cell's first line is "CantSplit line 0") is a
    // SINGLE fragment — never fragmented across pages.
    let cant_frags: Vec<&(usize, &super::wml::page::PagedTableRow)> = frags.iter()
        .filter(|(_, r)| r.cells.first().map_or(false, |c| {
            c.lines.iter().any(|l| line_text(l).contains("CantSplit line 0"))
        }))
        .collect();
    assert_eq!(cant_frags.len(), 1, "cantSplit row must NOT be fragmented (§17.4.7)");
    // And it carries its real top + bottom borders (a whole, intact row).
    let cc = &cant_frags[0].1.cells[0];
    assert!(cc.border_top.is_some() && cc.border_bottom.is_some(),
        "whole cantSplit row keeps both top and bottom borders");
}

// ─── header table (§17.10) ───────────────────────────────────────────────────
// A header part whose block content is a leading paragraph + a 2x2 bordered
// table. Before the fix, `layout_hdr_ftr_blocks` laid out only the paragraph and
// DROPPED the table; the header band rendered the paragraph but the table
// vanished. After the fix the table is laid out with the body table machinery
// and positioned into the header band as `PagedTableRow`s.

fn header_table_docx() -> Vec<u8> {
    let borders = r#"<w:tblBorders><w:top w:val="single" w:sz="8"/><w:left w:val="single" w:sz="8"/><w:bottom w:val="single" w:sz="8"/><w:right w:val="single" w:sz="8"/><w:insideH w:val="single" w:sz="8"/><w:insideV w:val="single" w:sz="8"/></w:tblBorders>"#;
    let cell = |txt: &str| format!(
        r#"<w:tc><w:tcPr><w:tcW w:w="2340" w:type="dxa"/></w:tcPr>{}</w:tc>"#,
        p("", &r("", txt))
    );
    let header = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:hdr {W_NS} {R_NS}>{para}<w:tbl><w:tblPr><w:tblW w:w="4680" w:type="dxa"/>{borders}</w:tblPr><w:tblGrid><w:gridCol w:w="2340"/><w:gridCol w:w="2340"/></w:tblGrid><w:tr>{c00}{c01}</w:tr><w:tr>{c10}{c11}</w:tr></w:tbl></w:hdr>"#,
        para = p("", &r("", "Header paragraph above the table")),
        c00 = cell("Doc No"), c01 = cell("ENG-001"),
        c10 = cell("Rev"), c11 = cell("B"),
    );

    let mut body = String::new();
    for i in 0..3 {
        body.push_str(&p("", &r("", &filler(i, 3))));
    }
    body.push_str(
        r#"<w:sectPr><w:headerReference w:type="default" r:id="rId2"/><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults><w:rPrDefault><w:rPr>
    <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
    <w:sz w:val="22"/><w:szCs w:val="22"/>
  </w:rPr></w:rPrDefault></w:docDefaults>
</w:styles>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
  <Override PartName="/word/header1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"/>
</Types>"#.to_string();
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#.to_string();
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
</Relationships>"#.to_string();

    build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
        ("word/header1.xml", header),
    ])
}

#[test]
fn header_table_renders_in_header_band() {
    let viewer = layout(header_table_docx());
    let pages = viewer.pages.borrow();
    assert!(!pages.is_empty(), "no pages");
    let page = &pages[0];

    // The leading header paragraph still lays out as a header line.
    assert!(
        page.header_lines.iter().any(|l| line_text(l).contains("Header paragraph")),
        "header paragraph above the table is missing"
    );

    // The header TABLE now produces two PagedTableRows in the header band — the
    // previously-dropped §17.10 case.
    assert_eq!(page.header_rows.len(), 2, "expected the 2-row header table in the header band");

    // Each row has two cells carrying the expected text.
    let texts: Vec<String> = page.header_rows.iter()
        .flat_map(|r| r.cells.iter())
        .flat_map(|c| c.lines.iter())
        .map(line_text)
        .collect();
    for want in ["Doc No", "ENG-001", "Rev", "B"] {
        assert!(texts.iter().any(|t| t.contains(want)), "header table cell text '{want}' missing");
    }

    // Borders resolved (the 2x2 table is bordered) and the table sits in the
    // header band — above the body content top.
    let row0 = &page.header_rows[0];
    assert!(row0.has_borders, "header table should carry borders");
    assert_eq!(row0.cells.len(), 2, "row 0 should have two cells");
    let body_top = page.elements.iter().find_map(|e| match e {
        PageElement::Line(l) => Some(l.baseline_y),
        _ => None,
    }).expect("body has at least one line");
    assert!(
        row0.y_top < body_top,
        "header table row (y_top {:.1}) must sit above the body content (first body baseline {:.1})",
        row0.y_top, body_top
    );
}

/// §17.3.1.32-.33 — `beforeAutospacing`/`afterAutospacing` emulate an HTML `<p>`
/// margin, which is `1em` of the paragraph's OWN font size, NOT a fixed pt value.
/// This SYNTHETIC docx builder stacks an anchor line + one more paragraph at a
/// chosen font `sz` (half-points) and `before` spacing. By laying out the SAME
/// font size with auto-before ON vs OFF, the second paragraph's baseline shift
/// isolates the pure auto-before value — which must equal one em of the font
/// (10pt → ~13.3px, 20pt → ~26.7px), NOT a single hardcoded 14pt/280-twip
/// constant (the old behaviour that over-spaced small-font auto paragraphs and
/// pushed dense-stack content off its page).
fn autospace_em_docx(sz: u32, second_before: &str) -> Vec<u8> {
    // line=240 lineRule=auto so each paragraph is a single natural line.
    let anchor = p(
        r#"<w:spacing w:before="0" w:after="0" w:beforeAutospacing="0" w:afterAutospacing="0" w:line="240" w:lineRule="auto"/>"#,
        &r(&format!(r#"<w:sz w:val="{sz}"/>"#), "anchor"),
    );
    // The auto-before margin is one em of the PARAGRAPH MARK's font (§17.3.1.29
    // — the pilcrow glyph governs paragraph-level metrics, and Word's HTML
    // autospacing emulation sizes the `<p>` margin from it). Set the mark rPr
    // size alongside the run size so this paragraph's em is unambiguous.
    let second = p(
        &format!(
            r#"<w:spacing {second_before} w:after="0" w:afterAutospacing="0" w:line="240" w:lineRule="auto"/><w:rPr><w:sz w:val="{sz}"/></w:rPr>"#
        ),
        &r(&format!(r#"<w:sz w:val="{sz}"/>"#), "second paragraph"),
    );
    let body = format!(
        "{anchor}{second}<w:sectPr><w:pgSz w:w=\"12240\" w:h=\"15840\"/><w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\" w:header=\"720\" w:footer=\"720\" w:gutter=\"0\"/></w:sectPr>"
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults><w:rPrDefault><w:rPr>
    <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
    <w:sz w:val="20"/><w:szCs w:val="20"/>
  </w:rPr></w:rPrDefault></w:docDefaults>
</w:styles>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#.to_string();
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#.to_string();
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#.to_string();
    build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
    ])
}

#[test]
fn autospacing_gap_scales_with_paragraph_font_size() {
    // The second paragraph's baseline with auto-before ON minus the same with
    // auto-before OFF (before="0") == the pure auto-before value at that em.
    fn second_baseline(sz: u32, second_before: &str) -> f64 {
        let viewer = layout(autospace_em_docx(sz, second_before));
        let pages = viewer.pages.borrow();
        let baselines: Vec<f64> = pages[0].elements.iter().filter_map(|el| match el {
            PageElement::Line(l) if !l.segments.iter().all(|s| s.items.is_empty()) => Some(l.baseline_y),
            _ => None,
        }).collect();
        assert_eq!(baselines.len(), 2, "expected anchor + second paragraph");
        baselines[1]
    }
    const OFF: &str = r#"w:before="0" w:beforeAutospacing="0""#;
    const ON: &str = r#"w:beforeAutospacing="1""#;

    // 10pt (sz=20): one em ≈ 10pt = 13.33px. Old constant was 14pt = 18.67px.
    let auto_10pt = second_baseline(20, ON) - second_baseline(20, OFF);
    // 20pt (sz=40): one em ≈ 20pt = 26.67px.
    let auto_20pt = second_baseline(40, ON) - second_baseline(40, OFF);

    // (a) The 10pt auto-before is one em of 10pt (~13.3px), NOT the old fixed
    // 14pt (~18.7px). Pin it near 13.3px with a tight tolerance that excludes
    // 18.7px — a regression to the constant fails here.
    assert!(
        (auto_10pt - 13.33).abs() < 2.0,
        "10pt auto-before must be ~1em (13.3px), got {auto_10pt:.1}px \
         (the old fixed 14pt/18.7px constant would fail this)"
    );
    // (b) Auto-before scales with the em: doubling the font ~doubles the gap.
    assert!(
        (auto_20pt - 2.0 * auto_10pt).abs() < 2.0,
        "auto-before must scale with font em (spec §17.3.1.32-.33): \
         10pt={auto_10pt:.1}px 20pt={auto_20pt:.1}px (20pt should ≈ 2×10pt)"
    );
}

/// One 1×1 table whose row uses the given `hRule`; the cell paragraph carries a
/// big literal before-spacing (50pt). Returns the cell's first line baseline_y
/// MINUS the row's top — how far the before-spacing pushed the text down inside
/// the row.
fn cell_first_line_offset(h_rule: &str) -> f64 {
    let cell_p = p(
        r#"<w:spacing w:before="1000" w:after="0" w:line="240" w:lineRule="auto"/>"#,
        &r("", "Cell text"),
    );
    let body = format!(
        r#"<w:tbl><w:tblPr><w:tblW w:w="9360" w:type="dxa"/></w:tblPr><w:tblGrid><w:gridCol w:w="9360"/></w:tblGrid><w:tr><w:trPr><w:trHeight w:hRule="{h_rule}" w:val="2000"/></w:trPr><w:tc><w:tcPr><w:tcW w:w="9360" w:type="dxa"/></w:tcPr>{cell_p}</w:tc></w:tr></w:tbl><w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr>"#
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#.to_string();
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.to_string();
    let bytes = build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
    ]);
    let viewer = layout(bytes);
    let pages = viewer.pages.borrow();
    for el in &pages[0].elements {
        if let PageElement::TableRow(row) = el {
            let line = &row.cells[0].lines[0];
            return line.baseline_y - row.y_top;
        }
    }
    panic!("no table row laid out");
}

#[test]
fn exact_row_suppresses_paragraph_before_spacing() {
    // §17.4.80 trHeight hRule="exact": a fixed-height row must not let paragraph
    // before-spacing push the cell text down/out of the box (calibration doc C1
    // p11: a NormalWeb title overlapped the next row before this). atLeast (the control)
    // GROWS to fit, so the 50pt before-spacing pushes the first line ~67px down.
    let exact = cell_first_line_offset("exact");
    let at_least = cell_first_line_offset("atLeast");
    assert!(
        at_least > exact + 40.0,
        "atLeast applies the 50pt before-spacing (offset {at_least:.1}px); \
         exact must suppress it (offset {exact:.1}px)"
    );
    assert!(
        exact < 30.0,
        "exact-row first line must NOT be pushed down by before-spacing; got {exact:.1}px"
    );
}

// ─── perf gate ───────────────────────────────────────────────────────────────
// ─── restartability self-test ───────────────────────────────────────────────
// For every fixture, force-capture a checkpoint at every page start, then
// re-enter the layout from EACH checkpoint and assert the produced page
// suffix is byte-identical to the recorded run. Ground truth is the per-push
// snapshot (PushRecord) taken INSIDE push_page — never final `self.pages`,
// which the two post-loop passes mutate (and a replay holds only a suffix).
// A divergence is a missed checkpoint field or a wrong replay recipe — fix
// the capture/replay code, NEVER the comparator or the fixture.

fn open_comment_ids_now() -> Vec<i64> {
    super::wml::linebreak::OPEN_COMMENT_IDS.with(|c| c.borrow().clone())
}

/// Exact-precision (`{:?}`) per-page dump for the restartability comparator —
/// replay is same-code-same-inputs, so ANY difference is a determinism bug to
/// fix, never a tolerance to widen. Includes per-item comment-id stamps in
/// body lines, table-cell lines and textbox lines (the geometry-invisible
/// drift channel) plus the push-time OPEN_COMMENT_IDS. Header/footer bands
/// are excluded: finish_page re-stamps them from the immutable laid_out_hf,
/// so drift there is impossible by construction — and `page_number`
/// below pins the global index that feeds them.
fn restart_dump(page: &super::wml::page::PageLayout, open_ids: &[i64], spi: usize) -> String {
    fn line(out: &mut String, tag: &str, l: &PagedLine) {
        out.push_str(&format!(
            "{tag} b{:?} l{:?} r{:?} h{:?} fl{:?} ll{:?}",
            l.baseline_y, l.content_left, l.content_right, l.line_height,
            l.is_first_line, l.is_last_line
        ));
        for s in &l.segments {
            out.push_str(&format!(" s{:?} n{}", s.x_start, s.items.len()));
            for it in &s.items {
                // Per-item widths: justify mutates item.width IN PLACE on
                // non-last lines (including the carried tail the
                // checkpoint-owned remainder preserves); content_width
                // pins the whitespace-drop channel. A count alone sees neither.
                out.push_str(&format!(" w{:?} cw{:?} t{}",
                    it.width, it.content_width, it.text.len()));
                if !it.comment_ids.is_empty() {
                    out.push_str(&format!(" c{:?}", it.comment_ids.as_slice()));
                }
            }
        }
        out.push('\n');
    }
    let mut out = String::new();
    // `spi`: section_page_idx at push time — the header/footer variant
    // selector (first/even/odd bands can share heights and differ only in
    // content).
    out.push_str(&format!("P n{} spi{} open{:?}\n", page.page_number, spi, open_ids));
    for ai in &page.anchored_images {
        out.push_str(&format!(
            "A {} x{:?} y{:?} w{:?} h{:?} bt{}\n",
            ai.rel_id, ai.x, ai.y, ai.width_px, ai.height_px, ai.behind_text
        ));
    }
    for tb in &page.positioned_text_boxes {
        out.push_str(&format!(
            "TB x{:?} y{:?} w{:?} h{:?}\n",
            tb.x, tb.y, tb.width_px, tb.height_px
        ));
        for l in &tb.lines {
            line(&mut out, "  TBL", l);
        }
    }
    for &(x, y0, y1) in &page.col_separator_lines {
        out.push_str(&format!("S x{:?} y0{:?} y1{:?}\n", x, y0, y1));
    }
    out.push_str(&format!("FN {:?} EN {:?}\n", page.footnote_ids, page.endnote_ids));
    for el in &page.elements {
        match el {
            PageElement::Line(l) => line(&mut out, "L", l),
            PageElement::TableRow(row) => {
                out.push_str(&format!("T y{:?} h{:?}\n", row.y_top, row.height_px));
                for cell in &row.cells {
                    // Border presence pins split-fragment identity
                    // (is_first/last_fragment derive from `consumed`).
                    out.push_str(&format!(
                        "C x{:?} w{:?} bt{} bb{}\n",
                        cell.x, cell.width_px,
                        cell.border_top.is_some(), cell.border_bottom.is_some()
                    ));
                    for l in &cell.lines {
                        line(&mut out, "  CL", l);
                    }
                }
            }
        }
    }
    out
}

fn first_diff_line(a: &str, b: &str) -> (usize, String, String) {
    for (i, (la, lb)) in a.lines().zip(b.lines()).enumerate() {
        if la != lb {
            return (i, la.to_string(), lb.to_string());
        }
    }
    let (na, nb) = (a.lines().count(), b.lines().count());
    (na.min(nb), format!("<{na} lines>"), format!("<{nb} lines>"))
}

/// Compare the recorded run's page suffix against a replay's full output.
/// Failure names the checkpoint ordinal, the absolute first-divergent page,
/// and the first divergent dump line.
fn compare_page_suffix(name: &str, ord: usize, from_page: usize, expected: &[String], got: &[String]) {
    assert_eq!(
        expected.len(),
        got.len(),
        "{name}: checkpoint #{ord} (page {from_page}) replay produced {} pages, expected {}",
        got.len(),
        expected.len()
    );
    for (off, (e, g)) in expected.iter().zip(got).enumerate() {
        if e != g {
            let (line_no, exp_line, got_line) = first_diff_line(e, g);
            panic!(
                "{name}: checkpoint #{ord} replay diverged at page {} (dump line {line_no}):\n  expected: {exp_line}\n  got:      {got_line}",
                from_page + off
            );
        }
    }
}

/// True when any VALID checkpoint's resume matches `pred` — battery tests pin
/// that their fixture actually exercises the intended ResumeAt variant.
fn log_has(log: &[(usize, super::PageCheckpoint)], pred: impl Fn(&super::ResumeAt) -> bool) -> bool {
    log.iter().any(|(_, ck)| ck.resume.as_ref().is_some_and(|r| pred(&r.at)))
}

/// The restartability self-test driver: returns (captured, skipped).
///  1. plain UNFORCED run — the production-shaped baseline;
///  2. forced re-run on the same viewer — recorded ground truth + log;
///  3. inertness: forced == unforced page-for-page (proves capture — incl.
///     the lazy remainder clone — perturbs nothing; keep captures
///     clone-only or this fires);
///  4. for each VALID checkpoint: set the directive, re-lay to document end,
///     compare the full page suffix + per-page and end-state open-comment
///     state. INVALID checkpoints (oversized/excluded) are counted as skips.
fn assert_restartable(name: &str, docx: Vec<u8>) -> (Vec<(usize, super::PageCheckpoint)>, usize) {
    let mut viewer = match Viewer::new() {
        Ok(v) => v,
        Err(_) => panic!("Viewer::new failed"),
    };
    load_test_fonts(&mut viewer);
    viewer.load_docx(docx).expect("load_docx failed");
    viewer.run_layout_native().expect("plain run");
    let unforced: Vec<String> = viewer.push_records.borrow().iter()
        .map(|r| restart_dump(&r.page, &r.open_comment_ids, r.section_page_idx))
        .collect();

    viewer.force_capture.set(true);
    viewer.run_layout_native().expect("forced run");
    let recorded: Vec<String> = viewer.push_records.borrow().iter()
        .map(|r| restart_dump(&r.page, &r.open_comment_ids, r.section_page_idx))
        .collect();
    let end_comments = open_comment_ids_now();
    let log: Vec<_> = viewer.checkpoint_log.borrow().clone();

    assert_eq!(
        unforced.len(),
        recorded.len(),
        "{name}: forced capture changed the page count"
    );
    for (i, (u, f)) in unforced.iter().zip(&recorded).enumerate() {
        if u != f {
            let (line_no, exp_line, got_line) = first_diff_line(u, f);
            panic!(
                "{name}: forced capture perturbed page {i} (dump line {line_no}):\n  unforced: {exp_line}\n  forced:   {got_line}"
            );
        }
    }
    assert!(!log.is_empty(), "{name}: no checkpoints captured under force");
    assert_eq!(log[0].0, 0, "{name}: first checkpoint must be the document's page 0");
    // Coverage: every pushed page must have a checkpoint logged at its start
    // (duplicates allowed — a carry-shared page legitimately holds two).
    // A missing index is a silently missing capture site — the failure class
    // this self-test exists for. If a fixture ever pushes a page with no
    // page-start capture (e.g. a parity blank — none in the current battery),
    // this fails loudly; exempt only provably-blank pages (empty PageLayout),
    // with a comment, once one actually exists.
    let covered: std::collections::HashSet<usize> = log.iter().map(|(pi, _)| *pi).collect();
    for pi in 0..recorded.len() {
        assert!(
            covered.contains(&pi),
            "{name}: page {pi} was pushed with no checkpoint captured at its start"
        );
    }

    let mut skipped = 0usize;
    for (ord, (page_idx, ck)) in log.iter().enumerate() {
        if ck.resume.is_none() {
            // Oversized remainder / documented exclusion: the
            // checkpoint is INVALID by design; the caller pins the count.
            skipped += 1;
            continue;
        }
        *viewer.resume_directive.borrow_mut() = Some(ck.clone());
        viewer.run_layout_native().expect("replay run");
        let replay: Vec<String> = viewer.push_records.borrow().iter()
            .map(|r| restart_dump(&r.page, &r.open_comment_ids, r.section_page_idx))
            .collect();
        compare_page_suffix(name, ord, *page_idx, &recorded[*page_idx..], &replay);
        assert_eq!(
            open_comment_ids_now(),
            end_comments,
            "{name}: checkpoint #{ord} replay drifted the open-comment end state"
        );
    }
    (log, skipped)
}

#[test]
fn restart_float_free_single_page() {
    let (_, skipped) = assert_restartable(
        "restart_float_free_single_page",
        perf_docx(&perf_float_free_body(3)),
    );
    assert_eq!(skipped, 0);
}

#[test]
fn restart_float_free_multi_page() {
    // Real BlockBoundary seams across several pages. (Sized for the debug
    // suite: the driver re-lays the whole doc once per checkpoint, so keep
    // fixtures at a handful of pages.)
    let (_, skipped) = assert_restartable(
        "restart_float_free_multi_page",
        perf_docx(&perf_float_free_body(48)),
    );
    assert_eq!(skipped, 0);
}

// ── mid-paragraph, page_break_before, keep-driven ParaTop ──

#[test]
fn restart_mid_paragraph_straddle() {
    // One huge paragraph spanning pages: ParaLines{OverflowBreak} seams.
    let (log, skipped) = assert_restartable(
        "restart_mid_paragraph_straddle",
        perf_docx(&p("", &r("", &filler(0, 60)))),
    );
    assert_eq!(skipped, 0);
    assert!(
        log_has(&log, |r| matches!(r,
            super::ResumeAt::ParaLines { src: super::ParaLineSrc::OverflowBreak, .. })),
        "fixture must exercise ParaLines/OverflowBreak"
    );
}

#[test]
fn restart_page_break_before() {
    // PageBreakBeforeTop: the page start that does NOT drain pending_floats.
    let body = format!(
        "{}{}",
        p("", &r("", &filler(0, 5))),
        p("<w:pageBreakBefore/>", &r("", &filler(1, 5)))
    );
    let (log, skipped) = assert_restartable("restart_page_break_before", perf_docx(&body));
    assert_eq!(skipped, 0);
    assert!(
        log_has(&log, |r| matches!(r, super::ResumeAt::PageBreakBeforeTop { .. })),
        "fixture must exercise PageBreakBeforeTop"
    );
}

#[test]
fn restart_keep_lines_para_top() {
    // keepLines paragraphs interleaved with filler: some instance overflows
    // the page remainder and trips need_break → ParaTop (spacing consumed
    // and discarded on the previous page — the replay must not re-add it).
    let mut body = String::new();
    for i in 0..12 {
        body.push_str(&p("", &r("", &filler(i, 3))));
        body.push_str(&p("<w:keepLines/>", &r("", &filler(i + 1, 9))));
    }
    let (log, skipped) = assert_restartable("restart_keep_lines_para_top", perf_docx(&body));
    assert_eq!(skipped, 0);
    assert!(
        log_has(&log, |r| matches!(r, super::ResumeAt::ParaTop { .. })),
        "fixture must exercise ParaTop"
    );
}

// ── tables (whole-row, split-row) + section_page_idx-driven bands ──

#[test]
fn restart_oracle_flow() {
    // The flow oracle: multi-page body, lists, a 36-row table straddling
    // pages (RowWhole), an explicit page break (ParaLines/ExplicitBreak),
    // header + footer with a live PAGE field.
    let (log, skipped) = assert_restartable("restart_oracle_flow", oracle_flow_docx());
    assert_eq!(skipped, 0);
    assert!(
        log_has(&log, |r| matches!(r, super::ResumeAt::RowWhole { .. })),
        "fixture must exercise RowWhole"
    );
    assert!(
        log_has(&log, |r| matches!(r, super::ResumeAt::PageBreakBeforeTop { .. })),
        "fixture must exercise PageBreakBeforeTop (Heading1 pageBreakBefore)"
    );
}

#[test]
fn restart_explicit_page_break() {
    // <w:br w:type="page"/> both MID-paragraph (next_line < len — the block
    // completes on the new page) and TRAILING (next_line == len — the block
    // completes immediately, epilogue on the new page): ExplicitBreak lands
    // at the iteration TOP, re-running should_break AND the widow branch.
    let brk = r#"<w:r><w:br w:type="page"/></w:r>"#;
    let body = format!(
        "{}{}{}",
        p("", &format!("{}{brk}{}", r("", &filler(0, 4)), r("", &filler(1, 4)))),
        p("", &format!("{}{brk}", r("", &filler(2, 4)))),
        p("", &r("", &filler(3, 4)))
    );
    let (log, skipped) = assert_restartable("restart_explicit_page_break", perf_docx(&body));
    assert_eq!(skipped, 0);
    assert!(
        log_has(&log, |r| matches!(r,
            super::ResumeAt::ParaLines { src: super::ParaLineSrc::ExplicitBreak, .. })),
        "fixture must exercise ParaLines/ExplicitBreak"
    );
}

#[test]
fn restart_split_table_row() {
    // RowSplit: mid-row page seams (cursors/consumed/entry_col replay).
    let (log, skipped) = assert_restartable("restart_split_table_row", row_split_docx());
    assert_eq!(skipped, 0);
    assert!(
        log_has(&log, |r| matches!(r, super::ResumeAt::RowSplit { .. })),
        "fixture must exercise RowSplit"
    );
}

/// Two EQUAL columns (the verified pre-packed path — NOT the unequal
/// re-pack) with filler pushing a tall splittable row's table ENTRY into
/// column 1: the mid-row page seam's RowSplit checkpoint records
/// entry_col == 1 while the page arm resets col_idx to 0. The continuation
/// fragment reuses the ENTRY column's x geometry verbatim (the split-row
/// quirk contract), so entry_col — not the checkpoint's col_idx — is what
/// replay must pin.
fn equal_cols_row_split_docx() -> Vec<u8> {
    let borders = r#"<w:tblBorders><w:top w:val="single" w:sz="4"/><w:left w:val="single" w:sz="4"/><w:bottom w:val="single" w:sz="4"/><w:right w:val="single" w:sz="4"/><w:insideH w:val="single" w:sz="4"/><w:insideV w:val="single" w:sz="4"/></w:tblBorders>"#;
    let mut body = String::new();
    // Fill column 0 and part of column 1 (288px columns, 864px tall).
    for i in 0..7 {
        body.push_str(&p("", &r("", &filler(i, 5))));
    }
    body.push_str(&format!(
        r#"<w:tbl><w:tblPr><w:tblW w:w="4000" w:type="dxa"/>{borders}</w:tblPr><w:tblGrid><w:gridCol w:w="4000"/></w:tblGrid>"#
    ));
    body.push_str(&format!(
        "<w:tr><w:tc><w:tcPr><w:tcW w:w=\"4000\" w:type=\"dxa\"/></w:tcPr>{}</w:tc></w:tr>",
        p("", &r("", "Control row"))
    ));
    // Tall splittable row: far taller than the column-1 remainder.
    let mut tall = String::new();
    for k in 0..60 {
        tall.push_str(&p("", &r("", &format!("Split line {k}"))));
    }
    body.push_str(&format!(
        "<w:tr><w:tc><w:tcPr><w:tcW w:w=\"4000\" w:type=\"dxa\"/></w:tcPr>{tall}</w:tc></w:tr>"
    ));
    body.push_str("</w:tbl>");
    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="2" w:space="720"/></w:sectPr>"#,
    );
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

#[test]
fn restart_row_split_entry_col1() {
    let (log, skipped) = assert_restartable(
        "restart_row_split_entry_col1",
        equal_cols_row_split_docx(),
    );
    assert_eq!(skipped, 0);
    assert!(
        log.iter().any(|(_, ck)| ck.col_idx == 0 && ck.resume.as_ref().is_some_and(|r|
            matches!(r.at, super::ResumeAt::RowSplit { entry_col: 1, .. }))),
        "fixture must checkpoint a RowSplit whose entry_col (1) differs from \
         the page-start col_idx (0)"
    );
}

#[test]
fn restart_header_table() {
    // Header-table doc: section_page_idx-dependent effective content bands.
    let (_, skipped) = assert_restartable("restart_header_table", header_table_docx());
    assert_eq!(skipped, 0);
}

// ── floats — deferral, gate-TRUE capture, reflow-block determinism ──

/// An anchored wrapSquare IMAGE whose anchor lands near the page bottom: the
/// wrap band overflows the anchor page while the image fits a fresh one, so
/// it takes the DeferredFloat path (pending_floats → apply_pending_floats on
/// the next page). Text after the anchor wraps around the re-emitted image.
fn deferred_image_docx() -> Vec<u8> {
    const IMG_EMU: i64 = 1_828_800; // 2in
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#
    );
    let anchor = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="0">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV>
  <wp:extent cx="{IMG_EMU}" cy="{IMG_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapSquare wrapText="right"/>
  <wp:docPr id="1" name="DeferImg"/>
  <wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect="1"/></wp:cNvGraphicFramePr>
  <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">
    <pic:pic><pic:nvPicPr><pic:cNvPr id="1" name="DeferImg"/><pic:cNvPicPr/></pic:nvPicPr>
    <pic:blipFill><a:blip r:embed="rId10"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>
    <pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{IMG_EMU}" cy="{IMG_EMU}"/></a:xfrm>
    <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );
    let mut body = String::new();
    for i in 0..6 {
        body.push_str(&p("", &r("", &filler(i, 5))));
    }
    body.push_str(&format!("<w:p>{anchor}{}</w:p>", r("", &filler(6, 6))));
    for i in 7..10 {
        body.push_str(&p("", &r("", &filler(i, 5))));
    }
    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr>"#,
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"><w:body>{body}</w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId10" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/defer.png"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
        ("word/media/defer.png", make_png(64, 64)),
    ])
}

#[test]
fn restart_deferred_pending_float() {
    // pending_floats capture (pre-drain clone) + drain replay at re-entry.
    let (log, skipped) =
        assert_restartable("restart_deferred_pending_float", deferred_image_docx());
    assert_eq!(skipped, 0);
    assert!(
        log.iter().any(|(_, ck)| !ck.pending_floats.is_empty()),
        "fixture must capture a checkpoint with a live deferred float"
    );
}

#[test]
fn restart_page_margin_float_heavy() {
    // Gate-TRUE doc (production capture path) with page-pinned inset boxes:
    // Cursor-dependent and inset float reflow + fits-guard replay determinism at scale.
    let (_, skipped) = assert_restartable(
        "restart_page_margin_float_heavy",
        perf_docx(&perf_page_margin_float_body(40)),
    );
    assert_eq!(skipped, 0);
}

#[test]
fn restart_fullwidth_inert() {
    // Full-width float above real text: the deliberately-inert deferred case.
    let (_, skipped) =
        assert_restartable("restart_fullwidth_inert", fullwidth_float_above_anchor_docx());
    assert_eq!(skipped, 0);
}

#[test]
fn restart_inset_bug_c() {
    // Bounded inset reflow replay determinism.
    let (_, skipped) =
        assert_restartable("restart_inset_bug_c", page_float_above_anchor_docx());
    assert_eq!(skipped, 0);
}

#[test]
fn restart_tall_inset_no_cascade() {
    // Former fits-guard abort path — now the fixpoint's demotion — replay determinism.
    let (_, skipped) = assert_restartable(
        "restart_tall_inset_no_cascade",
        tall_inset_float_no_cascade_docx(),
    );
    assert_eq!(skipped, 0);
}

// The fixpoint FIRE fixtures must ALSO be restartable — the
// checkpoint machinery now drives real rollbacks + demotions, and replaying the
// FINAL laid pages from any checkpoint must reproduce them byte-identically.
#[test]
fn restart_page_relative_inset() {
    let (_, skipped) = assert_restartable("restart_page_relative_inset", page_relative_inset_wraps_docx());
    assert_eq!(skipped, 0);
}

#[test]
fn restart_multicol_page_float() {
    let (_, skipped) = assert_restartable("restart_multicol_page_float", multicol_page_float_cascade_docx());
    assert_eq!(skipped, 0);
}

#[test]
fn restart_multicol_shallow() {
    // The CLEAN cross-column exclusion fixture must also replay byte-identically.
    let (_, skipped) = assert_restartable("restart_multicol_shallow", multicol_page_float_shallow_docx());
    assert_eq!(skipped, 0);
}

#[test]
fn restart_two_page_floats() {
    let (_, skipped) = assert_restartable("restart_two_page_floats", two_page_floats_docx());
    assert_eq!(skipped, 0);
}

#[test]
fn restart_overtall_demotes() {
    let (_, skipped) = assert_restartable("restart_overtall_demotes", overtall_page_float_docx());
    assert_eq!(skipped, 0);
}

/// Truly-empty paragraphs (items-free mark lines — the ONLY shape routed to
/// place_empty_para_around_floats!; a <w:br/>-only run still yields one item
/// per line and takes the fast path) against a FULL-WIDTH wrapping textbox
/// whose band bottom lands within one 48pt mark-line-height of the content
/// bottom: the first empty para overlaps the band (para_floated), clearance
/// drops it below the band, and the fit check fails → the EmptyPara ParaLines
/// variant (macro-entry content_height replay) fires at the page seam.
fn empty_para_reflow_docx() -> Vec<u8> {
    const BOX_W_EMU: i64 = 5_943_600; // 6.5in — spans_full_width
    // Geometry (11in page, 1in margins → 864px content; two 6-line fillers
    // put the bearer top at y≈227): band top ≈ 227+25(posOffset); height
    // 573px puts the band bottom ≈ 825 — inside (864−lh₄₈ₚₜ, 864], the ~75px
    // window where a 48pt mark line clears the band but no longer fits.
    const BOX_H_EMU: i64 = 5_457_825; // 573px
    const POS_V_EMU: i64 = 238_125; // 25px — bearer's own line stays above
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#
    );
    // Band pinned one line BELOW the bearer paragraph's top so the FOLLOWING
    // empty paragraph overlaps it (para_floated → the empty-para macro) while
    // the bearer's own short text stays above the band.
    let textbox = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="paragraph"><wp:posOffset>{POS_V_EMU}</wp:posOffset></wp:positionV>
  <wp:extent cx="{BOX_W_EMU}" cy="{BOX_H_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapTopAndBottom/>
  <wp:docPr id="1" name="Banner"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp><wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_W_EMU}" cy="{BOX_H_EMU}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">Banner.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/></wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );
    let mut body = String::new();
    for i in 0..2 {
        body.push_str(&p("", &r("", &filler(i, 5))));
    }
    // Bearer text must be ONE short line (17.6px < the 25px posOffset) so the
    // bearer itself stays above the band — otherwise the bearer goes
    // FloatAware and its own continuation consumes the band before the empty
    // paragraph reaches it.
    body.push_str(&format!("<w:p>{textbox}{}</w:p>", r("", "Bearer.")));
    // Truly-empty 48pt paragraphs (mark rPr sizes the mark line, §17.3.1.29)
    // overlapping the band — routed to place_empty_para_around_floats!; the
    // first one's mark line clears below the band and fails the fit check,
    // straddling the page (EmptyPara). The second lands float-free on the
    // fresh page (fast path) — suffix content beyond the seam.
    let big_empty = r#"<w:p><w:pPr><w:rPr><w:sz w:val="96"/></w:rPr></w:pPr></w:p>"#;
    body.push_str(big_empty);
    body.push_str(big_empty);
    body.push_str(&p("", &r("", &filler(4, 4))));
    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr>"#,
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}</w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

#[test]
fn restart_empty_para_reflow() {
    let (log, skipped) =
        assert_restartable("restart_empty_para_reflow", empty_para_reflow_docx());
    assert_eq!(skipped, 0);
    assert!(
        log_has(&log, |r| matches!(r,
            super::ResumeAt::ParaLines { src: super::ParaLineSrc::EmptyPara { .. }, .. })),
        "fixture must exercise ParaLines/EmptyPara"
    );
}

// ── continuous carry, footnotes/objects, comments, cell textboxes ──

#[test]
fn restart_continuous_carry() {
    // Carry-seeded section-entry checkpoint (capture site c with a NON-empty
    // `current` + live carried_in) feeding the column-balancing pass. The
    // carry-shared page legitimately holds TWO checkpoints: section 0's
    // never fires a page arm, section 1's entry restores the carried title.
    let (log, skipped) =
        assert_restartable("restart_continuous_carry", oracle_continuous_title_docx());
    assert_eq!(skipped, 0, "carry-seeded checkpoints must replay");
    assert!(
        log.iter().any(|(_, ck)| !ck.current.elements.is_empty()),
        "fixture must capture a carry-seeded (non-empty current) checkpoint"
    );
}

#[test]
fn restart_oracle_objects() {
    // 2-col continuous section, column separators, footnote ids, float image.
    let (_, skipped) = assert_restartable("restart_oracle_objects", oracle_objects_docx());
    assert_eq!(skipped, 0);
}

#[test]
fn restart_comments_body() {
    // Comment ranges across page seams: the per-item comment-id stamps and
    // the per-push OPEN_COMMENT_IDS comparison are the assertion surface.
    let (_, skipped) = assert_restartable("restart_comments_body", comments_oracle_docx());
    assert_eq!(skipped, 0);
}

#[test]
fn restart_textbox_in_cell() {
    // Cell-local float re-emission (table-cell anchored textbox wrap).
    let (_, skipped) = assert_restartable("restart_textbox_in_cell", cell_textbox_wrap_docx());
    assert_eq!(skipped, 0);
}

/// A wrapping (in-loop-laid) textbox that opens AND closes its own comment
/// range while a BODY comment range is already open across it — and that body
/// range stays open across the page seams AND the section boundary. This is
/// a geometry-invisible drift channel: layout_text_box
/// mutates the OPEN_COMMENT_IDS thread-local mid-loop, so a checkpoint that
/// missed the snapshot would replay identical geometry with wrong comment
/// stamps. Body ranges mutate the carrier at CACHE BUILD (section prologue),
/// so a single-section doc's page starts all see the section's final state —
/// the range must close in a SECOND section for section-1 checkpoints to
/// carry a live open id.
fn textbox_comment_docx() -> Vec<u8> {
    const BOX_EMU: i64 = 1_828_800; // 2in
    let box_xml = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1"><wp:simplePos x="0" y="0"/><wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH><wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV><wp:extent cx="{BOX_EMU}" cy="{BOX_EMU}"/><wp:effectExtent l="0" t="0" r="0" b="0"/><wp:wrapSquare wrapText="right"/><wp:docPr id="1" name="CBox"/><a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><wps:wsp><wps:cNvSpPr txBox="1"/><wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_EMU}" cy="{BOX_EMU}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr><wps:txbx><w:txbxContent><w:p><w:commentRangeStart w:id="1"/><w:r><w:t xml:space="preserve">Boxed span under review.</w:t></w:r><w:commentRangeEnd w:id="1"/><w:r><w:commentReference w:id="1"/></w:r></w:p></w:txbxContent></wps:txbx><wps:bodyPr/></wps:wsp></a:graphicData></a:graphic></wp:anchor></w:drawing></w:r>"#
    );
    let mut body = String::new();
    body.push_str(&p("", &r("", &filler(0, 5))));
    // Body comment 0 OPENS here and stays open across the textbox bearer and
    // the page seam.
    body.push_str(&format!(
        "<w:p>{}{}</w:p>",
        r#"<w:commentRangeStart w:id="0"/>"#,
        r("", "The long review span begins here "),
    ));
    // Bearer: the in-loop layout_text_box call runs while comment 0 is open
    // and pushes/pops comment 1 inside the box.
    body.push_str(&format!("<w:p>{box_xml}{}</w:p>", r("", &filler(1, 4))));
    for i in 2..10 {
        body.push_str(&p("", &r("", &filler(i, 5))));
    }
    // Section 1 ends here (NextPage default) with comment 0 STILL OPEN — its
    // cache build leaves the carrier non-empty, so every section-1 page-start
    // checkpoint (including the mid-section seam) snapshots a live open id.
    body.push_str(
        r#"<w:p><w:pPr><w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr></w:pPr></w:p>"#,
    );
    // Section 2: comment 0 CLOSES; its prologue pops the id.
    body.push_str(&format!(
        "<w:p>{}{}</w:p>",
        r("", "and the review span ends here."),
        r#"<w:commentRangeEnd w:id="0"/><w:r><w:commentReference w:id="0"/></w:r>"#,
    ));
    body.push_str(&p("", &r("", &filler(11, 4))));
    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr>"#,
    );
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}</w:body></w:document>"#
    );
    let comments = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:comments {W_NS}>
  <w:comment w:id="0" w:author="Ada Lovelace" w:date="2026-06-01T10:15:00Z" w:initials="AL">
    <w:p><w:r><w:t xml:space="preserve">Spans the page seam.</w:t></w:r></w:p>
  </w:comment>
  <w:comment w:id="1" w:author="Grace Hopper" w:date="2026-06-02T14:30:00Z" w:initials="GH">
    <w:p><w:r><w:t xml:space="preserve">Lives inside the anchored textbox.</w:t></w:r></w:p>
  </w:comment>
</w:comments>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
  <Override PartName="/word/comments.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
        ("word/comments.xml", comments.into_bytes()),
    ])
}

#[test]
fn restart_comments_in_anchored_textbox() {
    let (log, skipped) = assert_restartable(
        "restart_comments_in_anchored_textbox",
        textbox_comment_docx(),
    );
    assert_eq!(skipped, 0);
    // The seam must fall INSIDE the open body range — a checkpoint whose
    // open_comment_ids snapshot is non-empty proves it (otherwise the fixture
    // is not exercising the carrier).
    assert!(
        log.iter().any(|(_, ck)| !ck.open_comment_ids.is_empty()),
        "fixture must capture a page start with an OPEN body comment range"
    );
}

// ── remainder lossiness vectors (justify, whitespace, bidi/unequal-cols) ──

/// Two filler paragraphs, then a paragraph bearing a 2in wrapSquare textbox
/// (para_floated at entry → place_float_aware!) with `text` and `tail` (raw
/// run XML appended after the text run — e.g. a trailing page break), then a
/// trailing filler: the bearer straddles the page, so its seam checkpoints
/// are ParaItems{FloatAware} carrying the checkpoint-owned live remainder.
fn floated_para_docx(ppr: &str, text: &str, tail: &str) -> Vec<u8> {
    const BOX_EMU: i64 = 1_828_800; // 2in
    let box_xml = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1"><wp:simplePos x="0" y="0"/><wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH><wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV><wp:extent cx="{BOX_EMU}" cy="{BOX_EMU}"/><wp:effectExtent l="0" t="0" r="0" b="0"/><wp:wrapSquare wrapText="right"/><wp:docPr id="1" name="B"/><a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><wps:wsp><wps:cNvSpPr txBox="1"/><wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_EMU}" cy="{BOX_EMU}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr><wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">B.</w:t></w:r></w:p></w:txbxContent></wps:txbx><wps:bodyPr/></wps:wsp></a:graphicData></a:graphic></wp:anchor></w:drawing></w:r>"#
    );
    let ppr = if ppr.is_empty() { String::new() } else { format!("<w:pPr>{ppr}</w:pPr>") };
    let mut body = String::new();
    for i in 0..2 {
        body.push_str(&p("", &r("", &filler(i, 5))));
    }
    body.push_str(&format!("<w:p>{ppr}{box_xml}{}{tail}</w:p>", r("", text)));
    body.push_str(&p("", &r("", &filler(4, 4))));
    perf_docx(&body)
}

/// True when some VALID checkpoint is ParaItems{FloatAware} with a LIVE
/// (non-empty) remainder — the clone the lossiness fixtures exist to replay.
fn log_has_live_float_aware_remainder(log: &[(usize, super::PageCheckpoint)]) -> bool {
    log_has(log, |r| matches!(r,
        super::ResumeAt::ParaItems {
            src: super::ParaItemsSrc::FloatAware, items_remaining, ..
        } if *items_remaining > 0))
}

#[test]
fn restart_justified_straddle_near_float() {
    // Lossiness vector 2: justification mutates item widths IN PLACE on non-last
    // lines — including lines of the unplaced tail — and the carried tail
    // keeps those widths. Only the checkpoint-owned remainder clone can
    // replay them; a re-flatten from the pristine cache could not.
    let (log, skipped) = assert_restartable(
        "restart_justified_straddle_near_float",
        floated_para_docx(r#"<w:jc w:val="both"/>"#, &filler(7, 60), ""),
    );
    assert_eq!(skipped, 0);
    assert!(
        log_has_live_float_aware_remainder(&log),
        "fixture must checkpoint a live FloatAware remainder"
    );
}

#[test]
fn restart_whitespace_wrap_edge() {
    // Lossiness vector 1: whitespace-only items are dropped at wrap edges in the
    // UNPLACED tail too (the placed/unplaced cut happens after packing) —
    // multi-space xml:space="preserve" gaps make the drops observable in the
    // remainder the checkpoint owns.
    let words: String = (0..900)
        .map(|k| {
            let gap = ["  ", "   ", "    "][k % 3];
            format!("w{k:03}{gap}")
        })
        .collect();
    let (log, skipped) =
        assert_restartable("restart_whitespace_wrap_edge", floated_para_docx("", &words, ""));
    assert_eq!(skipped, 0);
    assert!(
        log_has_live_float_aware_remainder(&log),
        "fixture must checkpoint a live FloatAware remainder"
    );
}

#[test]
fn restart_floated_trailing_page_break() {
    // Trailing <w:br w:type="page"/> as the floated bearer's LAST placed
    // line: content is fully placed but the arm epilogue runs on the NEW
    // page, so the seam records ParaItems{items_remaining: 0, FloatAware} —
    // the empty-remainder arm re-entry (epilogue replay) recipe.
    let (log, skipped) = assert_restartable(
        "restart_floated_trailing_page_break",
        floated_para_docx("", &filler(3, 6), r#"<w:r><w:br w:type="page"/></w:r>"#),
    );
    assert_eq!(skipped, 0);
    assert!(
        log_has(&log, |r| matches!(r,
            super::ResumeAt::ParaItems {
                src: super::ParaItemsSrc::FloatAware, items_remaining: 0, ..
            })),
        "fixture must checkpoint an EMPTY FloatAware remainder (trailing page break)"
    );
}

/// Unequal-width 2-column section (equalWidth="0", 2800tw + 5840tw) whose one
/// long mixed Latin/Hebrew paragraph fills both columns of page 1 and spills:
/// the page seam is a ParaItems{UnequalCols} checkpoint whose remainder was
/// rebuilt from packed lines AFTER per-line bidi reordering (lossiness vector 3 —
/// visual order, not a logical-stream suffix; Arimo test font shapes Hebrew,
/// verified 27/27 base letters).
fn unequal_cols_docx() -> Vec<u8> {
    let mut text = String::new();
    for i in 0..40 {
        text.push_str(&filler(i, 2));
        text.push_str("שלום עולם ובדיקה נוספת "); // "hello world and another test"
    }
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#
    );
    let body = format!(
        r#"{}{}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="2" w:space="720" w:equalWidth="0"><w:col w:w="2800" w:space="720"/><w:col w:w="5840"/></w:cols></w:sectPr>"#,
        p("", &r("", &text)),
        p("", &r("", &filler(9, 3))),
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

#[test]
fn restart_unequal_columns() {
    let (log, skipped) = assert_restartable("restart_unequal_columns", unequal_cols_docx());
    assert_eq!(skipped, 0);
    assert!(
        log_has(&log, |r| matches!(r,
            super::ResumeAt::ParaItems {
                src: super::ParaItemsSrc::UnequalCols, items_remaining, ..
            } if *items_remaining > 0)),
        "fixture must exercise ParaItems/UnequalCols with a live remainder"
    );
}

// ── oversized-remainder INVALID checkpoints ──

#[test]
fn restart_giant_paragraph_invalid_checkpoint() {
    // One ~6700-word floated paragraph (the float routes it through
    // place_float_aware! — the fast path records ParaLines cursors, which
    // never carry a remainder and are always VALID). Early page seams hold
    // remainders above MAX_CHECKPOINT_REMAINDER_ITEMS → INVALID (resume:
    // None, skipped); as the tail shrinks below the cap, later seams become
    // VALID ParaItems checkpoints and must replay clean — the O(n²)
    // straddling-paragraph adversary pays O(1) per page and still restarts
    // where it legally can.
    let (log, skipped) = assert_restartable(
        "restart_giant_paragraph_invalid_checkpoint",
        floated_para_docx("", &filler(0, 450), ""),
    );
    assert!(skipped >= 1, "the giant paragraph must produce an INVALID (oversized-remainder) checkpoint");
    assert!(
        log.len() > skipped,
        "capture must keep producing VALID checkpoints around the giant ({} captured, {skipped} skipped)",
        log.len()
    );
    assert!(
        log.iter().all(|(_, ck)| ck.resume.as_ref().map_or(true, |r| match &r.at {
            super::ResumeAt::ParaItems { items_remaining, .. } =>
                *items_remaining <= super::MAX_CHECKPOINT_REMAINDER_ITEMS,
            _ => true,
        })),
        "every VALID ParaItems checkpoint must respect MAX_CHECKPOINT_REMAINDER_ITEMS"
    );
    assert!(
        log_has_live_float_aware_remainder(&log),
        "the shrunken tail must eventually checkpoint as a VALID ParaItems"
    );
}

// Opt-in scaling checks: lay out N and 2N blocks and assert the layout TIME
// ratio is ~linear (≈2.0), not super-linear. The ratio cancels machine speed
// and build profile, so it is stable across environments; min-of-K runs damps
// noise. #[ignore]d so the normal suite stays timing-free — run with:
//   cargo test -p parasma-wasm --features sml,pml --release -- --ignored perf_

fn perf_docx(body_inner: &str) -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#);
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body_inner}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr></w:body></w:document>"#);
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

fn perf_float_free_body(n: usize) -> String {
    (0..n).map(|i| p("", &r("", &filler(i, 5)))).collect()
}

fn perf_float_heavy_body(n: usize) -> String {
    // Every 8th paragraph anchors an INSET wrapSquare text box (paragraph-relative
    // — the common float-wrap path, not the bug-c reflow). Float count scales
    // with N, so genuinely-linear float work stays a ~2.0 ratio.
    const BOX_EMU: i64 = 1_828_800;
    let box_xml = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1"><wp:simplePos x="0" y="0"/><wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH><wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV><wp:extent cx="{BOX_EMU}" cy="{BOX_EMU}"/><wp:effectExtent l="0" t="0" r="0" b="0"/><wp:wrapSquare wrapText="right"/><wp:docPr id="1" name="B"/><a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><wps:wsp><wps:cNvSpPr txBox="1"/><wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_EMU}" cy="{BOX_EMU}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr><wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">B.</w:t></w:r></w:p></w:txbxContent></wps:txbx><wps:bodyPr/></wps:wsp></a:graphicData></a:graphic></wp:anchor></w:drawing></w:r>"#);
    (0..n).map(|i| {
        if i % 8 == 0 { format!("<w:p>{box_xml}{}</w:p>", r("", &filler(i, 5))) }
        else { p("", &r("", &filler(i, 5))) }
    }).collect()
}

fn min_layout_ms(docx: &[u8], runs: u32) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..runs {
        let t = std::time::Instant::now();
        let _ = layout(docx.to_vec());
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
    }
    best
}

fn scaling_ratio(body: impl Fn(usize) -> String, n: usize) -> (f64, f64, f64) {
    let t1 = min_layout_ms(&perf_docx(&body(n)), 5);
    let t2 = min_layout_ms(&perf_docx(&body(2 * n)), 5);
    (t2 / t1, t1, t2)
}

#[ignore = "perf gate — run with: cargo test --release -- --ignored perf_"]
#[test]
fn perf_float_free_scaling() {
    let (ratio, t1, t2) = scaling_ratio(perf_float_free_body, 500);
    println!("PERF float-free: N=500 {t1:.1}ms, 2N=1000 {t2:.1}ms → ratio {ratio:.2} (linear ≈ 2.0)");
    assert!(ratio < 2.5, "float-free layout scaling {ratio:.2} is super-linear (>2.5)");
}

#[ignore = "perf gate — run with: cargo test --release -- --ignored perf_"]
#[test]
fn perf_float_heavy_scaling() {
    let (ratio, t1, t2) = scaling_ratio(perf_float_heavy_body, 320);
    println!("PERF float-heavy: N=320 {t1:.1}ms, 2N=640 {t2:.1}ms → ratio {ratio:.2} (linear ≈ 2.0)");
    assert!(ratio < 2.7, "float-heavy layout scaling {ratio:.2} is super-linear (>2.7)");
}

// ─── fixpoint perf contract ──────────────────────────────────────────────────
// LayoutStats gates over the checkpoint, detector and fixpoint machinery,
// pinned by standing tests. The zero-asserts are
// non-ignored: they pin that float-free docs and the common paragraph-relative
// float docs never capture a checkpoint, never re-lay, never demote — the
// "float-free docs pay ~zero" contract. The scaling gate exercises the
// cursor-independent class the existing heavy gate never enters (its floats
// are paragraph-relative, so `doc_has_page_margin_wrap_float` stays false).

fn perf_page_margin_float_body(n: usize) -> String {
    // Every 8th paragraph anchors an INSET wrapSquare text box pinned to the
    // PAGE (cursor-independent V — the fixpoint trigger class),
    // mid-page so its band overlaps ordinarily-laid text. This arms
    // doc_has_page_margin_wrap_float and, later, checkpoint capture + the
    // fixpoint; float count scales with N so linear float work keeps a ~2.0
    // ratio and `pages_relaid` must stay within the per-float lifetime bound.
    const BOX_EMU: i64 = 1_828_800;
    const PAGE_V_EMU: i64 = 2_743_200; // 3in from page top: mid-content
    let box_xml = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1"><wp:simplePos x="0" y="0"/><wp:positionH relativeFrom="column"><wp:posOffset>3657600</wp:posOffset></wp:positionH><wp:positionV relativeFrom="page"><wp:posOffset>{PAGE_V_EMU}</wp:posOffset></wp:positionV><wp:extent cx="{BOX_EMU}" cy="{BOX_EMU}"/><wp:effectExtent l="0" t="0" r="0" b="0"/><wp:wrapSquare wrapText="left"/><wp:docPr id="1" name="PM"/><a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><wps:wsp><wps:cNvSpPr txBox="1"/><wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_EMU}" cy="{BOX_EMU}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr><wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">PM.</w:t></w:r></w:p></w:txbxContent></wps:txbx><wps:bodyPr/></wps:wsp></a:graphicData></a:graphic></wp:anchor></w:drawing></w:r>"#);
    (0..n).map(|i| {
        if i % 8 == 0 { format!("<w:p>{box_xml}{}</w:p>", r("", &filler(i, 5))) }
        else { p("", &r("", &filler(i, 5))) }
    }).collect()
}

#[test]
fn layout_stats_zero_for_float_free_doc() {
    let viewer = layout(perf_docx(&perf_float_free_body(40)));
    assert_eq!(
        viewer.layout_stats(), LayoutStats::default(),
        "float-free doc must not capture checkpoints, re-lay, demote, or detect violations"
    );
    assert!(viewer.violation_log.borrow().is_empty(),
        "float-free doc must fire no violations (no cursor-independent float ⇒ detector empty)");
}

#[test]
fn layout_stats_zero_for_paragraph_float_doc() {
    // Paragraph-relative floats are cursor-DEPENDENT: outside the fixpoint's
    // trigger class by construction. The hot path must stay stats-silent.
    let viewer = layout(perf_docx(&perf_float_heavy_body(40)));
    assert_eq!(
        viewer.layout_stats(), LayoutStats::default(),
        "paragraph-relative floats must never enter the fixpoint machinery"
    );
    assert!(viewer.violation_log.borrow().is_empty(),
        "paragraph-relative-float doc must fire no violations (cursor-dependent ⇒ detector filters them out)");
}

#[test]
fn checkpoint_capture_fires_when_gated() {
    // UNFORCED: a page/margin-float doc arms doc_has_page_margin_wrap_float,
    // so production capture fires on its own — every page start captures,
    // and every checkpoint of this plain doc is VALID (resume present).
    let viewer = layout(perf_docx(&perf_page_margin_float_body(24)));
    let stats = viewer.layout_stats();
    assert!(stats.checkpoints_captured > 0, "gated doc must capture checkpoints");
    let log = viewer.checkpoint_log.borrow();
    assert_eq!(
        stats.checkpoints_captured as usize,
        log.len(),
        "counter must count exactly the logged captures"
    );
    assert!(
        log.iter().all(|(_, ck)| ck.resume.is_some()),
        "plain-doc checkpoints must all be VALID (no documented exclusion expected)"
    );
    drop(log);

    // FORCED: a float-free doc captures only under the test override — first
    // capture is the document's page 0 (section entry, site c).
    let mut viewer = match Viewer::new() {
        Ok(v) => v,
        Err(_) => panic!("Viewer::new failed"),
    };
    load_test_fonts(&mut viewer);
    viewer.load_docx(perf_docx(&perf_float_free_body(40))).expect("load_docx");
    viewer.force_capture.set(true);
    viewer.run_layout_native().expect("run_layout_native");
    let log = viewer.checkpoint_log.borrow();
    assert!(!log.is_empty(), "forced capture must log checkpoints");
    assert_eq!(log[0].0, 0, "first checkpoint is the document's page 0");
}

#[ignore = "perf gate — run with: cargo test --release -- --ignored perf_"]
#[test]
fn perf_page_margin_float_heavy_scaling() {
    let (ratio, t1, t2) = scaling_ratio(perf_page_margin_float_body, 320);
    println!("PERF page-margin-float: N=320 {t1:.1}ms, 2N=640 {t2:.1}ms → ratio {ratio:.2} (linear ≈ 2.0)");
    assert!(ratio < 2.7, "page/margin-float layout scaling {ratio:.2} is super-linear (>2.7)");

    // Timing-noise-free linearity witness: re-lays are bounded per float
    // LIFETIME (≤3 lays/page batched + ≤1 demotion-lay/float), never per
    // iteration-count. The bound bites whenever the fixpoint re-lays a page.
    let n = 640;
    let viewer = layout(perf_docx(&perf_page_margin_float_body(n)));
    let stats = viewer.layout_stats();
    let pages = viewer.pages.borrow().len() as u32;
    let floats = (n as u32).div_ceil(8);
    assert!(
        stats.pages_relaid <= 3 * pages + floats,
        "pages_relaid {} exceeds the fixpoint bound 3*{pages}+{floats}",
        stats.pages_relaid
    );
}


// ─── object hit-test survives clearImageBytes (bug fix) ──────────────────────
//
// BUG 2 regression: `hit_test_object` used to build `is_image` from `self.images`
// (the raw bytes). The memory optimization frees those bytes via
// `clearImageBytes` after the main-thread decode, so in the post-decode steady
// state EVERY image `contains_key` was false ⇒ `is_image` false ⇒ NO image object
// could be selected (hit returned None). The fix retains the image-rel SET
// (`image_rel_ids`) independently of the bytes. This test proves the hit still
// lands AFTER `clearImageBytes` — the exact regression.
#[test]
fn object_hit_test_survives_clear_image_bytes() {
    use crate::wml::select::hit_test_object_impl;

    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/image/image-anchored.docx"
    );
    let docx = std::fs::read(path)
        .unwrap_or_else(|_| panic!("fixture missing: {path}"));
    let viewer = layout(docx);

    // The anchored image (rId1) is the only object; hit its painted centre.
    let pages = viewer.pages.borrow();
    let pg = pages.get(0).expect("page 0");
    let ai = pg
        .anchored_images
        .get(0)
        .expect("fixture has one anchored image");
    let (cx, cy) = (ai.x + ai.width_px / 2.0, ai.y + ai.height_px / 2.0);
    let rel = ai.rel_id.clone();

    let charts = viewer.chart_xml.borrow();
    let hit_at = || {
        let image_rels = viewer.image_rel_ids.borrow();
        let is_image = |r: &str| image_rels.contains(r) && !charts.contains_key(r);
        let is_chart = |r: &str| charts.contains_key(r);
        hit_test_object_impl(pg, cx, cy, &is_image, &is_chart)
    };

    // BEFORE the byte-clear: the raw bytes ARE present and the hit lands.
    assert!(
        viewer.images.borrow().contains_key(&rel),
        "precondition: image bytes present at load"
    );
    let before = hit_at().expect("hit before clearImageBytes");
    assert_eq!(before.rel_id, rel);

    // The memory optimization: free the raw bytes.
    viewer.clear_image_bytes();
    assert!(
        viewer.images.borrow().is_empty(),
        "clearImageBytes frees the raw bytes"
    );
    // …but the retained rel SET survives, so the object still hits (THE FIX).
    let after = hit_at().expect("hit AFTER clearImageBytes — bug 2 regression");
    assert_eq!(after.rel_id, rel);
    assert_eq!(after.kind, before.kind);
}

// ─── ANCHORED image rides along on text copy ─────────────────────────────────
//
// Word behaviour: copying a TEXT range that covers a floating image's
// anchor paragraph brings the image along on paste. Before this feature the
// float was silently DROPPED (worst failure mode: silent data loss). The
// linkage is captured on `ParaMeta.anchored_images` at the cache-build fold
// (paragraph.rs) and emitted by `extract_range_impl` as a trailing zero-width
// image run. v1 inclusion is PARA-LEVEL (anchor paragraph ∈ range ⇒ included);
// Word is char-position-precise — a documented deviation.
#[test]
fn extract_range_carries_anchored_image() {
    use crate::engine::fragment::Block;
    use crate::wml::select::{extract_range_impl, TextPos};

    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/image/image-anchored.docx"
    );
    let docx = std::fs::read(path).unwrap_or_else(|_| panic!("fixture missing: {path}"));
    let viewer = layout(docx);

    // The layout painted one anchored image; its carrier is stamped with the
    // stable id of its anchor paragraph (the inert tint linkage).
    let pages = viewer.pages.borrow();
    let ai = pages
        .iter()
        .flat_map(|p| p.anchored_images.iter())
        .next()
        .expect("fixture has one anchored image");
    let anchor_para = ai
        .anchor_para
        .expect("anchored image carrier must be stamped with its anchor para");

    // The side table recorded the SAME anchor with DISPLAY-px dims.
    let paras = viewer.para_meta.borrow();
    let meta = paras
        .get(anchor_para as usize)
        .expect("anchor para id indexes the ParaMeta side table");
    assert_eq!(meta.anchored_images.len(), 1);
    let rec = &meta.anchored_images[0];
    assert_eq!(rec.rel_id, ai.rel_id);
    assert!((rec.w_px - ai.width_px).abs() < 1e-9, "wPx = anchor DISPLAY px");
    assert!((rec.h_px - ai.height_px).abs() < 1e-9, "hPx = anchor DISPLAY px");

    // Extract a range covering the anchor paragraph → the image run is emitted
    // (trailing, zero-width) with the recorded dims.
    let tables = viewer.table_meta.borrow();
    let end_byte = meta.logical_text.len().max(1) as u32;
    let frag = extract_range_impl(
        &paras,
        &tables,
        TextPos::new(anchor_para, 0),
        TextPos::new(anchor_para, end_byte),
    );
    let img_runs: Vec<_> = frag
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Para(p) => Some(p),
            _ => None,
        })
        .flat_map(|p| p.runs.iter())
        .filter(|r| r.image.is_some())
        .collect();
    assert_eq!(img_runs.len(), 1, "the swept-in float emits exactly once");
    let img = img_runs[0].image.as_ref().unwrap();
    assert_eq!(img.rel_id, rec.rel_id);
    assert!(img.rel_id.starts_with("rId"), "a raw blip rel, never chart::");
    assert!((img.w_px - rec.w_px).abs() < 1e-9);
    assert!((img.h_px - rec.h_px).abs() < 1e-9);
    assert_eq!(img_runs[0].text, "", "anchored image run is zero-width");

    // A range on a DIFFERENT paragraph does not sweep the float in.
    let other = if anchor_para == 0 { 1 } else { 0 };
    if let Some(om) = paras.get(other as usize) {
        let oe = om.logical_text.len() as u32;
        let frag2 = extract_range_impl(
            &paras,
            &tables,
            TextPos::new(other, 0),
            TextPos::new(other, oe.max(1)),
        );
        let any_img = frag2.blocks.iter().any(|b| match b {
            Block::Para(p) => p.runs.iter().any(|r| r.image.is_some()),
            Block::Table(t) => t.rows.iter().any(|row| {
                row.cells.iter().any(|c| {
                    c.blocks.iter().any(|cb| match cb {
                        Block::Para(p) => p.runs.iter().any(|r| r.image.is_some()),
                        _ => false,
                    })
                })
            }),
        });
        assert!(!any_img, "a range NOT covering the anchor para emits no image");
    }
}

// ─── WML tabs cluster (bar tabs, hierarchy merge, RTL, unequal cols) ──
//
// Synthetic fixtures for the four tab fixes. Positions are exact by
// construction: twips_to_px(t) = t/15 at 96dpi (720tw = 48px), Letter page
// with 1" margins → body content spans x = 96..720.

/// Minimal docx: given styles.xml body (`<w:styles>` inner) + document body.
fn tabs_docx(styles_inner: &str, body: &str) -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>{styles_inner}</w:styles>"#
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

const TABS_DOC_DEFAULTS_FONT: &str = r#"<w:rPrDefault><w:rPr><w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="24"/></w:rPr></w:rPrDefault>"#;

/// Per-item `(text, absolute_x, width)` of one paged line, in VISUAL order.
fn line_item_positions(line: &PagedLine) -> Vec<(String, f64, f64)> {
    let mut out = Vec::new();
    for seg in &line.segments {
        let mut x = seg.x_start;
        for it in &seg.items {
            out.push((it.text.clone(), x, it.width));
            x += it.width;
        }
    }
    out
}

fn body_lines(viewer: &Viewer) -> Vec<PagedLine> {
    let pages = viewer.pages.borrow();
    pages.iter()
        .flat_map(|pg| pg.elements.iter())
        .filter_map(|el| match el {
            PageElement::Line(l) => Some(l.clone()),
            PageElement::TableRow(_) => None,
        })
        .collect()
}

/// Hierarchy merge — §17.3.1.38: "tab stops at each level in the style hierarchy are added
/// to each other to determine the full set of tab stops", keyed by position
/// (nearest definition wins per position), with §17.18.84 val="clear" removing
/// the inherited stop. Chain: docDefaults L@720 → Base L@2880 + L@3600 →
/// Derived (basedOn Base) C@720 (position override) + clear@2880 (removal) →
/// direct pPr L@4320 (accumulate). Merged: C@48px, L@240px, L@288px.
#[test]
fn tab_stops_merge_position_keyed_across_hierarchy() {
    let styles = format!(
        r#"<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}<w:pPrDefault><w:pPr><w:tabs><w:tab w:val="left" w:pos="720"/></w:tabs></w:pPr></w:pPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:styleId="Base"><w:name w:val="Base"/><w:pPr><w:tabs><w:tab w:val="left" w:pos="2880"/><w:tab w:val="left" w:pos="3600"/></w:tabs></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="Derived"><w:name w:val="Derived"/><w:basedOn w:val="Base"/><w:pPr><w:tabs><w:tab w:val="clear" w:pos="2880"/><w:tab w:val="center" w:pos="720"/></w:tabs></w:pPr></w:style>"#
    );
    let body = p(
        r#"<w:pStyle w:val="Derived"/><w:tabs><w:tab w:val="left" w:pos="4320"/></w:tabs>"#,
        &r("", "A\tB\tC\tD"),
    );
    let viewer = layout(tabs_docx(&styles, &body));
    let lines = body_lines(&viewer);
    assert!(!lines.is_empty(), "one body line expected");
    let items = line_item_positions(&lines[0]);
    let find = |t: &str| items.iter().find(|(txt, _, _)| txt == t)
        .unwrap_or_else(|| panic!("item {t:?} not found in {items:?}"));

    // Derived's center@720 OVERRIDES docDefaults' left@720 at the same
    // position: B is CENTERED on x=96+48=144.
    let (_, bx, bw) = find("B");
    assert!(((bx + bw / 2.0) - 144.0).abs() < 0.1, "B center {} != 144 (same-position override)", bx + bw / 2.0);
    // Derived's clear@2880 REMOVED Base's stop there: C lands on Base's
    // NEXT stop 3600tw → x=96+240=336 (not 96+192=288).
    let (_, cx, _) = find("C");
    assert!((cx - 336.0).abs() < 0.1, "C at {cx} != 336 (clear must remove the inherited 2880 stop)");
    // The direct-pPr stop ACCUMULATES with the style chain: D at 96+288=384.
    let (_, dx, _) = find("D");
    assert!((dx - 384.0).abs() < 0.1, "D at {dx} != 384 (direct pPr accumulates, not replaces)");
}

/// §17.3.1.37 — a custom tab-stop `pos` is measured from the PAGE (leading text)
/// MARGIN, NOT the paragraph indent. A TOC-shaped entry (left indent + a
/// right-aligned dot-leader tab near the text margin + a trailing page number)
/// must keep the number and its leader on the SAME line as the entry text.
/// Pre-fix the margin-relative right-tab is placed `indent` px too far right, so
/// the page number overflows `available` and drops to a second line.
#[test]
fn indented_right_tab_keeps_page_number_on_same_line() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    // 1in left indent (1440tw = 96px); a right tab w/ dot leader at 9000tw (600px
    // from the LEFT text margin). Explicit US-Letter section, 1in margins:
    //   content_width = 816 - 96 - 96 = 624px;  available = 624 - 96 = 528px.
    // The stop sits at 600px in the MARGIN frame → pre-fix it exceeds `available`
    // (528) and the "5" wraps; post-fix it maps to 600 - 96 = 504px < 528 and fits.
    let body = format!(
        "{}{}",
        p(
            r#"<w:ind w:left="1440"/><w:tabs><w:tab w:val="right" w:pos="9000" w:leader="dot"/></w:tabs>"#,
            &r("", "Chapter One\t5"),
        ),
        r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr>"#,
    );
    let viewer = layout(tabs_docx(&styles, &body));
    let lines = body_lines(&viewer);
    assert_eq!(
        lines.len(), 1,
        "TOC entry wrapped to {} lines — the indented right-tab overflowed by the \
         indent (§17.3.1.37: `pos` is margin-relative, not indent-relative)",
        lines.len()
    );
    // The page number sits on that one line and its right edge stays within the
    // content box (the right tab pulled it left to the stop, not past the margin).
    let items = line_item_positions(&lines[0]);
    let (_, nx, nw) = items.iter().find(|(t, _, _)| t.contains('5'))
        .unwrap_or_else(|| panic!("page number '5' missing from {items:?}"));
    assert!(
        *nx + *nw <= lines[0].content_right + 0.5,
        "page number right edge {} overflows content_right {}",
        nx + nw, lines[0].content_right
    );
}

/// Bar tabs — §17.18.84 "bar": "does not result in a custom tab stop in the parent
/// paragraph (this tab stop location shall be skipped when positioning custom
/// tab characters), but instead shall be used to draw a vertical line (or bar)
/// at this location". Advance skips the bar stop; every line of the paragraph
/// carries the rule position; other paragraphs are untouched.
#[test]
fn bar_tab_rules_on_every_line_without_advancing_text() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    let body = format!(
        "{}{}",
        p(
            r#"<w:tabs><w:tab w:val="bar" w:pos="2160"/><w:tab w:val="left" w:pos="2880"/></w:tabs>"#,
            &r("", &format!("X\tY {}", filler(0, 4))),
        ),
        p("", &r("", "control paragraph without tabs")),
    );
    let viewer = layout(tabs_docx(&styles, &body));
    let lines = body_lines(&viewer);
    assert!(lines.len() >= 3, "expected a wrapped tab paragraph + control, got {} lines", lines.len());

    // The tab advances to the LEFT stop at 2880tw (x=96+192=288) — the bar at
    // 2160tw is skipped for positioning (old behavior put Y at 96+144=240).
    let items = line_item_positions(&lines[0]);
    let (_, yx, _) = items.iter().find(|(t, _, _)| t.starts_with('Y'))
        .unwrap_or_else(|| panic!("item starting with Y not found in {items:?}"));
    assert!((yx - 288.0).abs() < 0.1, "Y at {yx} != 288 (bar must not act as an advance stop)");

    // Every line of the bar paragraph draws the rule at x=96+144=240.
    let bar_para = lines[0].para;
    let para_lines: Vec<_> = lines.iter().filter(|l| l.para == bar_para).collect();
    assert!(para_lines.len() >= 2, "bar paragraph should wrap to 2+ lines");
    for (i, l) in para_lines.iter().enumerate() {
        assert_eq!(l.bar_tabs.len(), 1, "line {i} of the bar paragraph must carry ONE bar rule");
        assert!((l.bar_tabs[0] - 240.0).abs() < 0.1, "line {i} bar rule at {} != 240", l.bar_tabs[0]);
    }
    // The control paragraph carries none.
    let control = lines.iter().find(|l| l.para != bar_para).expect("control line");
    assert!(control.bar_tabs.is_empty(), "tab-free paragraph must have no bar rules");
}

/// Bar-tab paint half — `render_line` strokes each bar rule as a vertical hairline
/// spanning the line box (same extent idiom as the paragraph side borders).
#[test]
fn bar_tab_paints_vertical_rule_spanning_line_box() {
    use super::engine::displaylist::{DrawCmd, RecordingPainter};
    let line = PagedLine {
        segments: Vec::new(),
        para: 0,
        baseline_y: 120.0,
        para_shading: None,
        line_height: 20.0,
        content_left: 96.0,
        content_right: 720.0,
        border_top: None, border_bottom: None, border_left: None, border_right: None,
        bar_tabs: vec![240.0],
        is_first_line: true,
        is_last_line: true,
    };
    let p = RecordingPainter::new();
    let bitmaps = std::collections::HashMap::new();
    let charts = std::collections::HashMap::new();
    super::wml::render::render_line(&p, &line, &bitmaps, &charts, false).expect("render_line");
    let cmds = p.cmds();
    // bdr_top = baseline - lh + 0.15*lh = 120 - 20 + 3 = 103; bottom = 123.
    let has_move = cmds.iter().any(|c| matches!(c, DrawCmd::MoveTo { x, y } if (*x - 240.0).abs() < 1e-9 && (*y - 103.0).abs() < 1e-9));
    let has_line = cmds.iter().any(|c| matches!(c, DrawCmd::LineTo { x, y } if (*x - 240.0).abs() < 1e-9 && (*y - 123.0).abs() < 1e-9));
    let has_stroke = cmds.iter().any(|c| matches!(c, DrawCmd::Stroke));
    let has_color = cmds.iter().any(|c| matches!(c, DrawCmd::SetStrokeStyle(s) if s == super::DEFAULT_TEXT_COLOR));
    assert!(has_move && has_line && has_stroke && has_color,
        "bar rule stroke missing: move={has_move} line={has_line} stroke={has_stroke} color={has_color}\n{cmds:?}");
}

/// Unequal columns — the unequal-columns reflow re-packs with the paragraph's RESOLVED tab
/// stops (carried on `ParagraphCache::tab_stops`), not an empty set. The stop
/// at 1200tw (80px) is OFF the 720tw default grid, so the old empty-custom
/// re-pack (default-interval stops only) would land B at 96+48=144.
#[test]
fn unequal_cols_reflow_keeps_custom_tab_stops() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    // Short page (content 4320−2880 = 1440tw = 96px ≈ 6 lines) plus filler so
    // column 1 OVERFLOWS into column 2: col_idx then reaches the last column
    // and last-page balancing (§17.6.3) is SKIPPED. That matters because the
    // balancing rebuild re-places the pre-packed `cache.lines` — which were
    // packed WITH the paragraph's stops on the main path — so a one-paragraph
    // fixture passes even with the UnequalCols-arm stops reverted to empty.
    // With balancing skipped, the landed geometry comes from the arm's own
    // pack_lines call, which is what this test pins.
    let filler: String = (0..10).map(|_| p("", &r("", "filler"))).collect();
    let body = format!(
        r#"{}{}<w:sectPr><w:pgSz w:w="12240" w:h="4320"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="2" w:space="720" w:equalWidth="0"><w:col w:w="2800" w:space="720"/><w:col w:w="5840"/></w:cols></w:sectPr>"#,
        p(
            r#"<w:tabs><w:tab w:val="left" w:pos="1200"/></w:tabs>"#,
            &r("", "A\tB"),
        ),
        filler,
    );
    let viewer = layout(tabs_docx(&styles, &body));
    let lines = body_lines(&viewer);
    let b_items = lines.iter()
        .map(line_item_positions)
        .find(|items| items.iter().any(|(t, _, _)| t == "B"))
        .expect("no line contains item B");
    let (_, bx, _) = b_items.iter().find(|(t, _, _)| t == "B").unwrap();
    assert!((bx - 176.0).abs() < 0.1, "B at {bx} != 176 (col1 left 96 + custom stop 80)");
    // Fixture guard: the filler really used column 2 (left edge 96 + 2800tw +
    // 720tw space ≈ 330.7px). If this fires, the fixture shrank and balancing
    // is masking the arm again — the pin above proves nothing.
    assert!(
        lines.iter().any(|l| l.segments.first().is_some_and(|s| s.x_start > 320.0)),
        "no line landed in column 2 — balancing not skipped, fixture is vacuous"
    );
}

/// RTL tabs — §17.3.1.37: "A tab stop location shall always be measured relative to
/// the leading edge of the paragraph in which it is used (that is … the right
/// edge for a right-to-left paragraph)", and §17.18.84 start/end alignment is
/// leading/trailing-edge relative. Our packer resolves tabs in LOGICAL advance
/// space — which per the spec IS leading-edge-relative — and the base-RTL
/// End-alignment (`rtl_swap_jc`) plus the UAX#9 L2 reorder map logical
/// distances onto the right page edge, so a start-tab at 2880tw places the
/// following text's leading (RIGHT) edge exactly 192px left of the content
/// right edge. This test pins the composition numerically.
#[test]
fn rtl_paragraph_tab_measures_from_right_edge() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    let body = p(
        r#"<w:bidi/><w:tabs><w:tab w:val="left" w:pos="2880"/></w:tabs>"#,
        &r("", "שלום\tעולם"),
    );
    let viewer = layout(tabs_docx(&styles, &body));
    let lines = body_lines(&viewer);
    assert!(!lines.is_empty(), "one body line expected");
    let items = line_item_positions(&lines[0]);
    assert!(items.len() >= 3, "expected [post-tab, tab, pre-tab] visual items, got {items:?}");

    // Visual order after the L2 reorder: the post-tab word paints FIRST
    // (leftmost), the pre-tab word LAST (rightmost, at the leading edge).
    assert_eq!(items.first().unwrap().0, "עולם", "post-tab word must be visually leftmost");
    assert_eq!(items.last().unwrap().0, "שלום", "pre-tab word must be visually rightmost");

    // The pre-tab word's trailing (visual RIGHT) edge sits at the content
    // right edge x=720 (base-RTL Start → End alignment).
    let (_, ax, aw) = items.last().unwrap();
    assert!((ax + aw - 720.0).abs() < 0.1, "שלום right edge {} != 720", ax + aw);

    // The post-tab word's LEADING (right) edge sits at the stop: 192px from
    // the right edge → x=720-192=528.
    let (_, bx, bw) = items.first().unwrap();
    assert!((bx + bw - 528.0).abs() < 0.1, "עולם leading edge {} != 528 (stop measured from the RIGHT edge)", bx + bw);
}

/// §17.3.1.37: "A tab stop location shall always be measured relative to
/// the leading edge of the paragraph", i.e. relative to the leading edge of the
/// LINE the tab sits on. A tab on line 2+ of a SOFT-wrapped paragraph must
/// therefore resolve against that line's own advance, not against the running
/// total since the paragraph began.
///
/// Tab widths used to be resolved in a pre-pass over the whole paragraph, before
/// any line breaking existed to consult — and that pass reset its cursor only at
/// a mandatory/page break, never at a soft wrap. So the tab below was measured
/// from ~line 1's full width, sailed past the 2880tw custom stop (`next_stop`
/// only returns stops ahead of the cursor) and fell through to the next default
/// interval, landing the post-tab word a few px after "AB" instead of at the
/// stop. Resolution now happens inside the packing loop, where the line is known.
#[test]
fn tab_on_a_soft_wrapped_line_measures_from_that_line() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    // Exactly one line's worth of filler (16 × 39.125px fills the 624px content
    // box under the legacy trailing-blank reserve), so "AB" opens line 1 and the
    // tab's line-relative x is just AB's own advance.
    let filler = "wrap ".repeat(16);
    let body = p(
        r#"<w:tabs><w:tab w:val="left" w:pos="2880"/></w:tabs>"#,
        &r("", &format!("{filler}AB\tCD")),
    );
    let viewer = layout(tabs_docx(&styles, &body));
    let lines = body_lines(&viewer);

    // Fixture guard: the tab must land on a SOFT-wrapped line, not line 0 —
    // on line 0 the old pre-pass was already correct and the test is vacuous.
    let tab_line = lines.iter().position(|l| {
        line_item_positions(l).iter().any(|(t, _, _)| t == "CD")
    }).expect("no line contains item CD");
    assert!(
        tab_line > 0,
        "the tab landed on line 0 — the fixture no longer exercises the soft-wrap reset"
    );

    let items = line_item_positions(&lines[tab_line]);
    // Fixture guard 2: "AB" must OPEN that line. If the filler drifts and AB
    // lands mid-line, its line-relative x is already past the 2880tw stop and
    // the default interval takes over — the expectation below would then be
    // wrong rather than the code.
    assert_eq!(
        items.first().map(|(t, _, _)| t.as_str()), Some("AB"),
        "AB must open the wrapped line for the stop to be reachable; got {items:?}"
    );
    let (_, cx, _) = items.iter().find(|(t, _, _)| t == "CD")
        .unwrap_or_else(|| panic!("item CD not found in {items:?}"));
    // Same stop, same expectation as the line-0 control below: content left 96
    // plus the 2880tw (192px) stop.
    assert!(
        (cx - 288.0).abs() < 0.1,
        "CD at {cx} != 288 — the tab on line {tab_line} was measured from the \
         PARAGRAPH's advance, not from its own line's leading edge (§17.3.1.37)"
    );
}

/// RTL tabs, LTR control — the same stop in a left-to-right paragraph measures from
/// the LEFT edge: the post-tab word STARTS at 96+192=288.
#[test]
fn ltr_paragraph_tab_measures_from_left_edge() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    let body = p(
        r#"<w:tabs><w:tab w:val="left" w:pos="2880"/></w:tabs>"#,
        &r("", "AB\tCD"),
    );
    let viewer = layout(tabs_docx(&styles, &body));
    let lines = body_lines(&viewer);
    let items = line_item_positions(&lines[0]);
    let (_, cx, _) = items.iter().find(|(t, _, _)| t == "CD")
        .unwrap_or_else(|| panic!("item CD not found in {items:?}"));
    assert!((cx - 288.0).abs() < 0.1, "CD at {cx} != 288");
}

// ─── numbering: numStyleLink resolution + level rPr ─────────────────────────
// §17.9.21 numStyleLink / §17.9.27 styleLink: an abstractNum that is a
// REFERENCE to a numbering style, whose pPr/numPr/numId names the num whose
// abstractNum (carrying the matching styleLink) holds the real levels.
// §17.9.24: the level's OWN rPr formats the numbering text (marker) only.
//
// VACUITY NOTE: a level-less abstractNum does NOT drop the marker — the
// engine synthesizes a default bullet ("•", left=720·(ilvl+1), hanging=360)
// (numbering.rs). Tests therefore assert lvlText CONTENT + level GEOMETRY,
// never mere marker presence.

/// Minimal docx with word/numbering.xml + word/styles.xml (Arimo 11pt
/// docDefaults) around the given body paragraphs.
fn numbering_docx(styles_extra: &str, numbering_inner: &str, body: &str) -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults>
    <w:rPrDefault><w:rPr><w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/></w:rPr></w:rPrDefault>
    <w:pPrDefault><w:pPr/></w:pPrDefault>
  </w:docDefaults>
  {styles_extra}
</w:styles>"#
    );
    let numbering = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering {W_NS}>{numbering_inner}</w:numbering>"#
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}<w:sectPr>
  <w:pgSz w:w="12240" w:h="15840"/>
  <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
</w:sectPr></w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
  <Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
        ("word/numbering.xml", numbering.into_bytes()),
    ])
}

/// A body paragraph carrying a direct `numPr` (ilvl 0) reference.
fn num_p(num_id: i64, text: &str) -> String {
    p(
        &format!(r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="{num_id}"/></w:numPr>"#),
        &r("", text),
    )
}

/// abstractNum 0 = pure numStyleLink reference → numbering style "ListNums" →
/// numId 2 → abstractNum 1 (the styleLink-carrying definer: decimal "%1)",
/// start=3, ind left=1440 hanging=360). numId 1 → abs 0 (the reference side).
fn numstylelink_docx() -> Vec<u8> {
    let numbering_inner = r#"
  <w:abstractNum w:abstractNumId="0">
    <w:numStyleLink w:val="ListNums"/>
  </w:abstractNum>
  <w:abstractNum w:abstractNumId="1">
    <w:styleLink w:val="ListNums"/>
    <w:lvl w:ilvl="0">
      <w:start w:val="3"/>
      <w:numFmt w:val="decimal"/>
      <w:lvlText w:val="%1)"/>
      <w:pPr><w:ind w:left="1440" w:hanging="360"/></w:pPr>
    </w:lvl>
  </w:abstractNum>
  <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
  <w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>"#;
    let styles_extra = r#"
  <w:style w:type="numbering" w:styleId="ListNums">
    <w:name w:val="List Nums"/>
    <w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr>
  </w:style>"#;
    let body = format!(
        "{}{}{}",
        num_p(1, "Alpha references through the style"),
        num_p(1, "Beta references through the style"),
        num_p(2, "Gamma uses the defining num directly"),
    );
    numbering_docx(styles_extra, numbering_inner, &body)
}

/// T1 (numStyleLink, §17.9.21/§17.9.27) — a paragraph on the REFERENCE num renders the
/// chased level's lvlText ("3)"/"4)", start=3) at the chased indent (1440tw),
/// not the synthesized default bullet at 720tw.
#[test]
fn num_style_link_resolves_through_numbering_style() {
    let viewer = layout(numstylelink_docx());
    let lines = body_lines(&viewer);
    assert!(lines.len() >= 3, "expected 3 numbered paragraphs, got {} lines", lines.len());
    let m0 = &lines[0].segments[0].items[0];
    let m1 = &lines[1].segments[0].items[0];
    assert_eq!(
        m0.text, "3)\u{a0}",
        "first marker must be the chased level's lvlText at start=3 (§17.9.21) — \
         a default bullet means the numStyleLink chase failed"
    );
    assert_eq!(m1.text, "4)\u{a0}", "second marker must advance the counter");
    // Chased level geometry: page margin 1440tw (96px) + ind left 1440tw (96px)
    // − hanging 360tw (24px) = 168px. The default-bullet fallback (left 720tw)
    // would start at 120px.
    let x0 = lines[0].segments[0].x_start;
    assert!(
        (x0 - 168.0).abs() < 0.5,
        "marker line starts at x{x0:.2}, expected 168 (chased ind left=1440 hanging=360); \
         120 = the default-bullet 720tw fallback (chase not applied)"
    );
}

/// T2 (numStyleLink floor) — counters stay keyed by the paragraph-facing num: the
/// DEFINING num (numId=2) is NOT advanced by the two preceding paragraphs on
/// the REFERENCING num (numId=1); it starts fresh at "3)".
///
/// Scope: this topology has two DISTINCT abstractNum ids (one linking), where
/// independent counters match Word. It says nothing about two nums sharing
/// the SAME abstract, where the §17.9.26 example shows Word CONTINUING the
/// count across instances — that continuity gap is #30/#31 counter territory,
/// untouched here. Note this test does not detect chase removal (numId=2
/// resolves directly either way) — that is T1's job; this pins counter
/// non-aliasing only.
#[test]
fn defining_num_keeps_independent_counter() {
    let viewer = layout(numstylelink_docx());
    let lines = body_lines(&viewer);
    let m2 = &lines[2].segments[0].items[0];
    assert_eq!(
        m2.text, "3)\u{a0}",
        "numId=2 must keep its own counter (no aliasing with the referencing num)"
    );
}

/// T3 (numStyleLink) — a numStyleLink CYCLE (abs 0 → style A → num 2 → abs 1 → style A
/// → num 2 → abs 1 → …) must terminate via the depth cap and fall back to the
/// synthesized default bullet (the final abstract has no levels).
///
/// Note this test does not detect chase removal (no-chase also yields the
/// default bullet) — that is T1's job; this pins TERMINATION only (with the
/// cap removed the layout hangs, verified by removing it).
#[test]
fn num_style_link_cycle_terminates() {
    let numbering_inner = r#"
  <w:abstractNum w:abstractNumId="0">
    <w:numStyleLink w:val="A"/>
  </w:abstractNum>
  <w:abstractNum w:abstractNumId="1">
    <w:numStyleLink w:val="A"/>
  </w:abstractNum>
  <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
  <w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>"#;
    let styles_extra = r#"
  <w:style w:type="numbering" w:styleId="A">
    <w:name w:val="A"/>
    <w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr>
  </w:style>"#;
    let body = num_p(1, "Cycle must not hang the layout");
    let viewer = layout(numbering_docx(styles_extra, &numbering_inner, &body));
    let lines = body_lines(&viewer);
    let marker = &lines[0].segments[0].items[0];
    assert_eq!(
        marker.text, "\u{2022}\u{a0}",
        "cyclic chase must terminate at the default synthesized bullet"
    );
}

/// One abstractNum, one level (ilvl 0) with the given numFmt/lvlText/rPr;
/// one body paragraph on numId=1 whose run carries `run_rpr`.
fn single_level_docx(num_fmt: &str, lvl_text: &str, lvl_rpr: &str, run_rpr: &str) -> Vec<u8> {
    let numbering_inner = format!(
        r#"
  <w:abstractNum w:abstractNumId="0">
    <w:lvl w:ilvl="0">
      <w:start w:val="1"/>
      <w:numFmt w:val="{num_fmt}"/>
      <w:lvlText w:val="{lvl_text}"/>
      <w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr>
      {lvl_rpr}
    </w:lvl>
  </w:abstractNum>
  <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#
    );
    let body = p(
        r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr>"#,
        &r(run_rpr, "Body text stays plain"),
    );
    numbering_docx("", &numbering_inner, &body)
}

/// T4 (level rPr, §17.9.24) — the level's OWN rPr (b + sz 32 + color FF0000) formats
/// the MARKER only; the first text run keeps its plain 11pt style. Pins the
/// "affects only the numbering text itself" boundary in BOTH directions.
#[test]
fn level_rpr_formats_marker_only() {
    let docx = single_level_docx(
        "bullet",
        "\u{2022}",
        r#"<w:rPr><w:b/><w:sz w:val="32"/><w:color w:val="FF0000"/></w:rPr>"#,
        "",
    );
    let viewer = layout(docx);
    let lines = body_lines(&viewer);
    let seg = &lines[0].segments[0];
    let marker = &seg.items[0];
    assert!(marker.style.bold, "level rPr <w:b/> must bold the marker (§17.9.24)");
    assert_eq!(marker.style.font_pt, 16.0, "level rPr sz=32 half-points → 16pt marker");
    assert_eq!(marker.style.color, "#FF0000", "level rPr color must apply to the marker");
    let text_item = &seg.items[1];
    assert!(!text_item.style.bold, "level rPr must NOT bleed into the text run");
    assert_eq!(text_item.style.font_pt, 11.0, "text run keeps the 11pt docDefault");
}

/// T5 (level rPr) — a level rPr naming an UNSHIPPED symbol font (Symbol) must keep
/// the run-derived family (the U+F0B7→• remap keeps today's face/advance) while
/// the SIZE still applies. Guards both the keep-base-on-miss family path and
/// the naive-apply variant (family would flip to the resolved default).
///
/// A SECOND family is registered under the default name ("Calibri" =
/// DEFAULT_FONT_FAMILY) so the fallback default is semantically distinct from
/// the run-derived family: in the single-font harness "keep the base" and
/// "apply the fallback default" would otherwise differ only by letter case
/// (default_family_name lowercases), and a font-cache case-normalization
/// cleanup would silently vacuate the family half of this test.
#[test]
fn symbol_family_kept_but_size_applies() {
    let docx = single_level_docx(
        "bullet",
        "&#xF0B7;",
        r#"<w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/><w:sz w:val="48"/></w:rPr>"#,
        "",
    );
    let mut viewer = match Viewer::new() {
        Ok(v) => v,
        Err(_) => panic!("Viewer::new failed"),
    };
    load_test_fonts(&mut viewer);
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
    let bytes = std::fs::read(format!("{dir}/Arimo-Regular.ttf")).expect("test font");
    viewer.load_font("Calibri", bytes, 400, false).expect("load default-name family");
    if viewer.load_docx(docx).is_err() {
        panic!("load_docx failed");
    }
    viewer.run_layout_native().expect("run_layout_native");
    let lines = body_lines(&viewer);
    let seg = &lines[0].segments[0];
    let marker = &seg.items[0];
    let text_item = &seg.items[1];
    assert_eq!(marker.text, "\u{2022}\u{a0}", "PUA bullet must remap to •");
    assert_eq!(
        marker.style.family, text_item.style.family,
        "Symbol is not shipped → try_resolve_font misses → marker keeps the run-derived family"
    );
    assert_ne!(
        marker.style.family.to_lowercase(), "calibri",
        "marker must NOT fall to the default family on miss (keep-base, not naive-apply)"
    );
    assert_eq!(marker.style.font_pt, 24.0, "level rPr sz=48 half-points → 24pt marker");
}

/// T6 (level rPr, §17.9.24) — the LEVEL's own <w:u> survives the run-decoration strip
/// (order is load-bearing: strip first, overlay after), and the decoration
/// extent clamps to the glyphs (content_width < width — the rule must not run
/// across the baked NBSP gap + hanging-slot pad, which are not lvlText).
#[test]
fn level_underline_composes_after_strip() {
    let docx = single_level_docx(
        "decimal",
        "%1.",
        r#"<w:rPr><w:u w:val="single"/></w:rPr>"#,
        "",
    );
    let viewer = layout(docx);
    let lines = body_lines(&viewer);
    let seg = &lines[0].segments[0];
    let marker = &seg.items[0];
    assert_eq!(
        marker.style.underline,
        super::engine::paint::Underline::Single,
        "the level's OWN underline must apply to the marker (§17.9.24 positive half)"
    );
    assert!(
        marker.content_width < marker.width - 0.1,
        "decorated marker extent must clamp to the glyphs: content_width {:.2} !< width {:.2}",
        marker.content_width,
        marker.width
    );
    let text_item = &seg.items[1];
    assert_eq!(
        text_item.style.underline,
        super::engine::paint::Underline::None,
        "the level rPr must not underline the text run"
    );
}

/// T7 (level rPr negative half, a regression guard for the marker decoration strip,
/// which originally shipped untested) — an underlined RUN must not bleed its decoration under the
/// marker: the run rPr "affects only the numbering text itself, not the
/// remainder of runs" cuts BOTH ways (§17.9.24).
#[test]
fn marker_strips_run_decorations() {
    let docx = single_level_docx("bullet", "\u{2022}", "", r#"<w:u w:val="single"/>"#);
    let viewer = layout(docx);
    let lines = body_lines(&viewer);
    let seg = &lines[0].segments[0];
    let marker = &seg.items[0];
    assert_eq!(
        marker.style.underline,
        super::engine::paint::Underline::None,
        "run underline must be stripped from the marker"
    );
    assert!(!marker.style.strikethrough, "run strike must be stripped from the marker");
    let text_item = &seg.items[1];
    assert_eq!(
        text_item.style.underline,
        super::engine::paint::Underline::Single,
        "the text run itself keeps its underline"
    );
}

/// T9 (level rPr) — a DECORATED marker (content_width < width after the §17.9.24
/// clamp) must NOT become a justification gap-carrier: `justify_line` treats
/// `width > content_width` as "has a stretchable trailing gap", and without
/// the sentinel exclusion a jc=both wrapped list paragraph would stretch the
/// marker slot, pushing first-line text past the hanging indent while
/// continuation lines stay put. Word never stretches the marker/suffix gap.
#[test]
fn justified_line_does_not_stretch_decorated_marker() {
    let numbering_inner = r#"
  <w:abstractNum w:abstractNumId="0">
    <w:lvl w:ilvl="0">
      <w:start w:val="1"/>
      <w:numFmt w:val="decimal"/>
      <w:lvlText w:val="%1."/>
      <w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr>
      <w:rPr><w:u w:val="single"/></w:rPr>
    </w:lvl>
  </w:abstractNum>
  <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#;
    // Long enough to wrap: jc=both justifies every line but the last, so the
    // FIRST line (the one carrying the marker) is a genuine stretch target.
    let body = p(
        r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr><w:jc w:val="both"/>"#,
        &r("", &"lorem ipsum dolor sit amet consectetur ".repeat(6)),
    );
    let viewer = layout(numbering_docx("", numbering_inner, &body));
    let lines = body_lines(&viewer);
    assert!(lines.len() >= 2, "fixture must wrap so the first line is justified");
    let seg = &lines[0].segments[0];
    let marker = &seg.items[0];
    assert!(
        marker.content_width < marker.width - 0.1,
        "precondition: decorated marker carries the clamp (else this test is vacuous)"
    );
    // Justification really fired: some non-marker item got stretched past its
    // shaped advance (word items bake one trailing space ≲3.1px at 11pt; a
    // stretched gap exceeds it).
    let stretched = seg.items.iter().skip(1).any(|it| it.width > it.content_width + 3.5);
    assert!(stretched, "fixture must produce a justified (stretched) first line");
    // The marker slot itself stays at the unstretched hanging width (360tw =
    // 24px > the "1. " advance at 11pt). With the sentinel exclusion reverted
    // it absorbs a per-gap share and exceeds 24.
    assert!(
        (marker.width - 24.0).abs() < 0.01,
        "marker slot must stay 24px (unstretched), got {:.3}",
        marker.width
    );
}

// ─── built-in Header/Footer style fallback (WORD-BEHAVIOR) ──────────────────
// Real documents reference <w:pStyle w:val="Header"/>/"Footer" on hdr/ftr
// paragraphs WITHOUT defining those styles in styles.xml; Word applies its
// Normal.dotm built-ins (tabs center@4680 + right@9360 twips, spacing after=0
// line=240 auto). `inject_builtin_hdr_ftr_styles` synthesizes them into the
// StyleContext at load, insert-only-when-absent. No ECMA definition exists —
// documented as Word-behavior (compatibilityMode-absent precedent).

/// Minimal docx with a default header + footer part around the given styles
/// and body. Letter page, 1440tw margins (content x ∈ [96, 720] px).
fn hf_docx(styles_inner: &str, header_inner: &str, footer_inner: &str, body: &str) -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>{styles_inner}</w:styles>"#
    );
    let header = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:hdr {W_NS}>{header_inner}</w:hdr>"#
    );
    let footer = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:ftr {W_NS}>{footer_inner}</w:ftr>"#
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}<w:sectPr>
  <w:headerReference w:type="default" r:id="rId2"/>
  <w:footerReference w:type="default" r:id="rId3"/>
  <w:pgSz w:w="12240" w:h="15840"/>
  <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
</w:sectPr></w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
  <Override PartName="/word/header1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"/>
  <Override PartName="/word/footer1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
        ("word/header1.xml", header.into_bytes()),
        ("word/footer1.xml", footer.into_bytes()),
    ])
}

/// docDefaults (Arimo 12pt) + a default "Normal" paragraph style — but NO
/// Header/Footer definition, the fallback's trigger condition.
const HF_STYLES_NO_HEADER: &str = r#"<w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="24"/></w:rPr></w:rPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style>"#;

/// T1 — a header paragraph referencing the UNDEFINED "Header" style gets
/// Word's built-in tab stops: center@4680tw (312px) + right@9360tw (624px).
/// RED (injection absent): the stops fall back to the 720tw default-interval
/// grid — C lands left-aligned at 96+48k px, R nowhere near the right margin.
#[test]
fn hf_builtin_fallback_header_tabs() {
    let header = p(r#"<w:pStyle w:val="Header"/>"#, &r("", "L\tC\tR"));
    let viewer = layout(hf_docx(HF_STYLES_NO_HEADER, &header, "<w:p/>", &p("", &r("", "body"))));
    let pages = viewer.pages.borrow();
    let hline = pages[0].header_lines.first().expect("header line");
    let items = line_item_positions(hline);
    let find = |t: &str| items.iter().find(|(txt, _, _)| txt == t)
        .unwrap_or_else(|| panic!("item {t:?} not found in {items:?}"));
    // Center of "C" at margin 96 + twips_to_px(4680)=312 → 408.
    let (_, cx, cw) = find("C");
    assert!(((cx + cw / 2.0) - 408.0).abs() < 0.5,
        "C center {} != 408 (built-in center tab @4680tw)", cx + cw / 2.0);
    // Right edge of "R" at margin 96 + twips_to_px(9360)=624 → 720.
    let (_, rx, rw) = find("R");
    assert!(((rx + rw) - 720.0).abs() < 0.5,
        "R right edge {} != 720 (built-in right tab @9360tw)", rx + rw);
}

/// T2 — the built-in Footer style's spacing after=0 overrides docDefaults'
/// pPrDefault after=120 at the style tier: two footer paragraphs stack with
/// NO inter-paragraph gap (baseline delta == first line height).
/// RED (injection absent): delta = lh + twips_to_px(120) = lh + 8px.
#[test]
fn hf_builtin_fallback_footer_spacing() {
    let styles = format!(
        "{HF_STYLES_NO_HEADER}"
    ).replace(
        "</w:docDefaults>",
        r#"<w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults>"#,
    );
    let footer = format!(
        "{}{}",
        p(r#"<w:pStyle w:val="Footer"/>"#, &r("", "first")),
        p(r#"<w:pStyle w:val="Footer"/>"#, &r("", "second")),
    );
    let viewer = layout(hf_docx(&styles, "<w:p/>", &footer, &p("", &r("", "body"))));
    let pages = viewer.pages.borrow();
    let flines = &pages[0].footer_lines;
    assert!(flines.len() >= 2, "expected 2 footer lines, got {}", flines.len());
    let delta = flines[1].baseline_y - flines[0].baseline_y;
    let lh = flines[0].line_height;
    assert!((delta - lh).abs() < 0.5,
        "footer paragraph gap {delta} != line height {lh} (built-in after=0 must cancel docDefaults after=120)");
}

/// T3 — a styles.xml definition of styleId "Header" MASKS the built-in:
/// its single center tab @3000tw (200px) wins, NOT the built-in 4680.
/// RED-condition proven by temporarily making the injection unconditional
/// (documents the insert-only-when-absent contract).
#[test]
fn hf_builtin_masked_by_doc_style() {
    let styles = format!(
        r#"{HF_STYLES_NO_HEADER}
<w:style w:type="paragraph" w:styleId="Header"><w:name w:val="header"/><w:pPr><w:tabs><w:tab w:val="center" w:pos="3000"/></w:tabs></w:pPr></w:style>"#
    );
    let header = p(r#"<w:pStyle w:val="Header"/>"#, &r("", "L\tC"));
    let viewer = layout(hf_docx(&styles, &header, "<w:p/>", &p("", &r("", "body"))));
    let pages = viewer.pages.borrow();
    let hline = pages[0].header_lines.first().expect("header line");
    let items = line_item_positions(hline);
    let (_, cx, cw) = items.iter().find(|(t, _, _)| t == "C")
        .unwrap_or_else(|| panic!("item C not found in {items:?}"));
    // Center of "C" at 96 + twips_to_px(3000)=200 → 296 (doc style, NOT 408).
    assert!(((cx + cw / 2.0) - 296.0).abs() < 0.5,
        "C center {} != 296 (doc-defined Header style must mask the built-in)", cx + cw / 2.0);
}

/// T4 — the synthesized style enters the §17.3.1.38 position-keyed tab-stop
/// merge at the styles tier: a direct-pPr left@2000tw ACCUMULATES with the
/// built-in center@4680 + right@9360 (all three fire).
/// RED (injection absent): only the 2000 stop resolves; C/D land on the
/// default 720tw grid.
#[test]
fn hf_builtin_merges_with_direct_ppr() {
    let header = p(
        r#"<w:pStyle w:val="Header"/><w:tabs><w:tab w:val="left" w:pos="2000"/></w:tabs>"#,
        &r("", "A\tB\tC\tD"),
    );
    let viewer = layout(hf_docx(HF_STYLES_NO_HEADER, &header, "<w:p/>", &p("", &r("", "body"))));
    let pages = viewer.pages.borrow();
    let hline = pages[0].header_lines.first().expect("header line");
    let items = line_item_positions(hline);
    let find = |t: &str| items.iter().find(|(txt, _, _)| txt == t)
        .unwrap_or_else(|| panic!("item {t:?} not found in {items:?}"));
    // Direct pPr left stop: B starts at 96 + 2000/15 = 229.33.
    let (_, bx, _) = find("B");
    assert!((bx - (96.0 + 2000.0 / 15.0)).abs() < 0.5, "B at {bx} != 229.33 (direct-pPr stop)");
    // Built-in center: C centered at 408.
    let (_, cx, cw) = find("C");
    assert!(((cx + cw / 2.0) - 408.0).abs() < 0.5,
        "C center {} != 408 (built-in stop must merge with direct pPr)", cx + cw / 2.0);
    // Built-in right: D right edge at 720.
    let (_, dx, dw) = find("D");
    assert!(((dx + dw) - 720.0).abs() < 0.5,
        "D right edge {} != 720 (built-in stop must merge with direct pPr)", dx + dw);
}

// ─── East Asian refinement toggles ──────────────────────────────────────────
// autoSpaceDE (§17.3.1.2) / autoSpaceDN (§17.3.1.3): automatic inter-run gap
// between East Asian and Latin text / digits — DEFAULT ON when absent ("assumed
// to be true"). The gap magnitude is WORD-OBSERVED ~1/8 em (ECMA defines only
// the toggle) — see AUTOSPACE_EM_FRACTION in linebreak.rs. kinsoku (§17.3.1.16)
// and wordWrap (§17.3.1.45) toggles gate extra break opportunities when
// explicitly OFF. Native-font caveat: only Arimo loads, so CJK widths are
// deterministic but not "real" — every assertion below is a toggle-twin DELTA,
// never an absolute width.

/// All body lines' items (first segment), for item-level width/char assertions.
fn line_items_of(viewer: &Viewer) -> Vec<Vec<super::Item>> {
    body_lines(viewer).iter()
        .map(|l| l.segments.iter().flat_map(|s| s.items.iter().cloned()).collect())
        .collect()
}

/// Expected autospace pad at the test font size: 1/8 em of 12pt = 2.0px.
const AUTOSPACE_PAD_12PT: f64 = 0.125 * 16.0;

/// T5 — autoSpaceDE defaults ON: "abc中文def" gets a 1/8-em pad folded into
/// the item width on each EA↔Latin joint ("abc" at abc|中, "文" at 文|def);
/// "中" (EA×EA joint) and "def" (last) are untouched. The explicit-off twin
/// (<w:autoSpaceDE w:val="0"/>) proves both the default-on resolution and the
/// explicit-off leg. Text-position currency guard: item count and char tiling are
/// IDENTICAL across twins (the fold never touches text/bytes).
/// RED (fold absent): all widths equal across twins.
#[test]
fn autospace_de_default_on() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    let on = layout(tabs_docx(&styles, &p("", &r("", "abc中文def"))));
    let off = layout(tabs_docx(&styles, &p(r#"<w:autoSpaceDE w:val="0"/>"#, &r("", "abc中文def"))));
    let on_items = &line_items_of(&on)[0];
    let off_items = &line_items_of(&off)[0];
    let texts: Vec<&str> = on_items.iter().map(|i| i.text.as_str()).collect();
    assert_eq!(texts, ["abc", "中", "文", "def"], "UAX#14 item premise");
    assert_eq!(on_items.len(), off_items.len(), "fold must not change item count");
    let w = |items: &[super::Item], t: &str| items.iter().find(|i| i.text == t)
        .unwrap_or_else(|| panic!("item {t:?} missing")).width;
    assert!((w(on_items, "abc") - w(off_items, "abc") - AUTOSPACE_PAD_12PT).abs() < 0.01,
        "abc|中 joint: default-on width {} != off width {} + 2.0 pad",
        w(on_items, "abc"), w(off_items, "abc"));
    assert!((w(on_items, "文") - w(off_items, "文") - AUTOSPACE_PAD_12PT).abs() < 0.01,
        "文|def joint: default-on width {} != off width {} + 2.0 pad",
        w(on_items, "文"), w(off_items, "文"));
    assert!((w(on_items, "中") - w(off_items, "中")).abs() < 0.01,
        "中 (EA×EA joint) must be unpadded");
    assert!((w(on_items, "def") - w(off_items, "def")).abs() < 0.01,
        "def (paragraph-final) must be unpadded");
    // Text-position currency: char_start/char_len still tile the logical text.
    for items in [on_items, off_items] {
        let mut cursor = 0u32;
        for it in items.iter() {
            assert_eq!(it.char_start, cursor, "char_start must tile contiguously");
            assert_eq!(it.char_len as usize, it.text.len(), "char_len == text byte len");
            cursor += it.char_len;
        }
        assert_eq!(cursor as usize, "abc中文def".len(), "tiling covers the paragraph");
    }
}

/// T6 — autoSpaceDN pads the EA↔digit joint ("中文123" → pad on 文) and the
/// Latin-only control "abc123" (one UAX#14 item, no EA neighbor) gets NO pad
/// anywhere (vacuity guard). RED (fold absent): no delta on 中文123.
#[test]
fn autospace_dn_digit_pair() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    let body_on = format!("{}{}", p("", &r("", "中文123")), p("", &r("", "abc123")));
    let body_off = format!("{}{}", p(r#"<w:autoSpaceDN w:val="0"/>"#, &r("", "中文123")), p("", &r("", "abc123")));
    let on = layout(tabs_docx(&styles, &body_on));
    let off = layout(tabs_docx(&styles, &body_off));
    let on_lines = line_items_of(&on);
    let off_lines = line_items_of(&off);
    let w = |items: &[super::Item], t: &str| items.iter().find(|i| i.text == t)
        .unwrap_or_else(|| panic!("item {t:?} missing")).width;
    assert!((w(&on_lines[0], "文") - w(&off_lines[0], "文") - AUTOSPACE_PAD_12PT).abs() < 0.01,
        "文|1 joint: DN-on width {} != DN-off width {} + 2.0 pad",
        w(&on_lines[0], "文"), w(&off_lines[0], "文"));
    assert!((w(&on_lines[0], "中") - w(&off_lines[0], "中")).abs() < 0.01, "中 unpadded");
    // Control: Latin+digit with no EA neighbor is ONE item, identical width.
    assert_eq!(on_lines[1].len(), 1, "abc123 must be a single UAX#14 item");
    assert!((w(&on_lines[1], "abc123") - w(&off_lines[1], "abc123")).abs() < 0.01,
        "abc123 (no EA neighbor) must get no pad");
}

/// T7 — an explicit space at the joint suppresses autospace ("abc 中文": the
/// regions are not adjoining). RED under a buggy fold that ignores whitespace:
/// a pad appears on the "abc " item and this equality breaks.
#[test]
fn autospace_space_suppression() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    let on = layout(tabs_docx(&styles, &p("", &r("", "abc 中文"))));
    let off = layout(tabs_docx(&styles, &p(r#"<w:autoSpaceDE w:val="0"/>"#, &r("", "abc 中文"))));
    let on_items = &line_items_of(&on)[0];
    let off_items = &line_items_of(&off)[0];
    assert_eq!(on_items.len(), off_items.len());
    for (a, b) in on_items.iter().zip(off_items.iter()) {
        assert_eq!(a.text, b.text);
        assert!((a.width - b.width).abs() < 0.01,
            "item {:?}: width {} != {} — whitespace joint must suppress the pad",
            a.text, a.width, b.width);
    }
}

/// T8 — autoSpaceDE w:val="0" on a referenced paragraph STYLE (not direct pPr)
/// also turns the pad off — proves the style-chain leg of the resolution.
/// RED (only direct pPr read): the style doc still pads and equals default.
#[test]
fn autospace_off_via_style() {
    let base = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    let styled = format!(
        r#"{base}<w:style w:type="paragraph" w:styleId="NoAuto"><w:name w:val="NoAuto"/><w:pPr><w:autoSpaceDE w:val="0"/></w:pPr></w:style>"#
    );
    let on = layout(tabs_docx(&base, &p("", &r("", "abc中文def"))));
    let via_style = layout(tabs_docx(&styled, &p(r#"<w:pStyle w:val="NoAuto"/>"#, &r("", "abc中文def"))));
    let w = |v: &Viewer, t: &str| line_items_of(v)[0].iter().find(|i| i.text == t)
        .unwrap_or_else(|| panic!("item {t:?} missing")).width;
    assert!((w(&on, "abc") - w(&via_style, "abc") - AUTOSPACE_PAD_12PT).abs() < 0.01,
        "style-level autoSpaceDE=0 must remove the pad (default {} vs styled {})",
        w(&on, "abc"), w(&via_style, "abc"));
}

/// T9 — <w:kinsoku w:val="0"/> (§17.3.1.16) disables the kinsoku line-break
/// protections: the EA×EA-glued "語。" (UAX#14 CL class forbids the break, so
/// the default collect emits it as ONE item — asserted in-test, making the
/// red-condition premise self-verifying) becomes splittable, yielding 4 items
/// with char_start 0/3/6/9 and char_len 3 each.
/// RED (split absent): item counts equal across twins.
#[test]
fn kinsoku_off_split() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    let on = layout(tabs_docx(&styles, &p("", &r("", "日本語。"))));
    let off = layout(tabs_docx(&styles, &p(r#"<w:kinsoku w:val="0"/>"#, &r("", "日本語。"))));
    let on_items = &line_items_of(&on)[0];
    let off_items = &line_items_of(&off)[0];
    // Premise: default kinsoku glues 。 to 語 (UAX#14 CL) — one "語。" item.
    assert!(on_items.iter().any(|i| i.text == "語。"),
        "premise: default items must glue 語。, got {:?}",
        on_items.iter().map(|i| &i.text).collect::<Vec<_>>());
    assert!(on_items.len() < off_items.len(),
        "kinsoku=0 must add break opportunities ({} vs {})", on_items.len(), off_items.len());
    let texts: Vec<&str> = off_items.iter().map(|i| i.text.as_str()).collect();
    assert_eq!(texts, ["日", "本", "語", "。"], "kinsoku=0 splits at EA×EA boundaries");
    for (idx, it) in off_items.iter().enumerate() {
        assert_eq!(it.char_start, 3 * idx as u32, "piece {idx} char_start");
        assert_eq!(it.char_len, 3, "piece {idx} char_len");
    }
}

/// T10 — the kinsoku=0 split reaches the packer: in a column too narrow for
/// two characters, the line break lands before 。 and 。 becomes LINE-INITIAL
/// (the §17.3.1.16 example: the toggle-off state permits 。 to start a line).
/// RED (split absent): 。 stays glued to 語 and is never line-initial.
#[test]
fn kinsoku_off_via_layout() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    // ind left 9300tw = 620px of the 624px content → ~4px available: every
    // item overflows, one item per line (width-independent — no absolute-width
    // assumptions on the CJK fallback path).
    let body = p(r#"<w:kinsoku w:val="0"/><w:ind w:left="9300"/>"#, &r("", "日本語。"));
    let viewer = layout(tabs_docx(&styles, &body));
    let lines = line_items_of(&viewer);
    assert!(
        lines.iter().any(|items| items.first().is_some_and(|i| i.text == "。")),
        "。 must start a line under kinsoku=0, lines: {:?}",
        lines.iter().map(|items| items.iter().map(|i| i.text.clone()).collect::<String>()).collect::<Vec<_>>()
    );
}

/// T11 — <w:wordWrap w:val="0"/> (§17.3.1.45): "break words across lines by
/// character" — a single long ASCII word wider than the column wraps mid-word,
/// line 1 carrying a strict prefix. RED (split absent): the unbreakable word
/// stays on ONE overflowing line.
#[test]
fn word_wrap_off_mid_word() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    let word = "M".repeat(120);
    let viewer = layout(tabs_docx(&styles, &p(r#"<w:wordWrap w:val="0"/>"#, &r("", &word))));
    let lines = line_items_of(&viewer);
    assert!(lines.len() >= 2, "wordWrap=0 must wrap the long word, got {} line(s)", lines.len());
    let line1: String = lines[0].iter().map(|i| i.text.as_str()).collect();
    assert!(!line1.is_empty() && line1.len() < word.len() && word.starts_with(&line1),
        "line 1 {:?} must be a strict prefix of the word", line1);
}

/// T12 — the wordWrap=0 per-char split must never detach a combining mark from
/// its base or cut inside a joining script (§17.3.1.45 note: breaks must not
/// "change the semantics or appearance of the content" — per-piece re-shaping
/// of Arabic would render isolated letterforms). The cluster/ComplexScript
/// guard suppresses those cuts while ordinary chars still split.
#[test]
fn word_wrap_off_preserves_clusters_and_joining_scripts() {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>");
    let arabic = "\u{0633}\u{0644}\u{0627}\u{0645}"; // سلام
    let viewer = layout(tabs_docx(&styles, &p(
        r#"<w:wordWrap w:val="0"/>"#,
        &r("", &format!("xe\u{301}y {arabic}")),
    )));
    let texts: Vec<String> = line_items_of(&viewer).into_iter().flatten()
        .map(|i| i.text).collect();
    // Ordinary chars still split (the guard must not disable wordWrap=0).
    assert!(texts.iter().any(|t| t == "x"), "per-char split missing: {texts:?}");
    // The combining acute must ride its base as one piece.
    assert!(texts.iter().any(|t| t == "e\u{301}"), "cluster split apart: {texts:?}");
    assert!(texts.iter().all(|t| t.chars().next() != Some('\u{301}')),
        "a piece starts with a combining mark: {texts:?}");
    // The Arabic word must survive as ONE piece (no isolated letterforms).
    assert!(texts.iter().any(|t| t.contains(arabic)),
        "Arabic word was split: {texts:?}");
}

// ─── §17.6 per-section page geometry (mixed page sizes) ──────────────────────
// A doc with sections of DIFFERENT page sizes (portrait then landscape) must
// render each section at ITS OWN geometry, not one global size. Synthetic,
// measured against Word on calibration doc C4. Verifies:
//   (a) each page carries the section it belongs to (`section_idx`);
//   (b) `page_geometry(section_idx)` returns portrait dims for section 0 and
//       landscape dims (width > height) for section 1;
//   (c) the portrait page is taller than wide; the landscape page wider than
//       tall — the bug (every page one global size) is impossible to pass.

/// Two next-page sections: section 0 portrait (12240×15840 tw), section 1
/// landscape (15840×12240 tw, `w:orient="landscape"`).
fn two_section_mixed_docx() -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults><w:rPrDefault><w:rPr>
    <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
    <w:sz w:val="22"/><w:szCs w:val="22"/>
  </w:rPr></w:rPrDefault></w:docDefaults>
</w:styles>"#
    );

    // Section 0 (portrait): a content paragraph, then a paragraph whose pPr
    // carries the SECTION-ENDING sectPr (portrait geometry).
    let mut body = String::new();
    body.push_str(&p("", &r("", "Portrait section one.")));
    body.push_str(&p(
        r#"<w:sectPr>
          <w:pgSz w:w="12240" w:h="15840"/>
          <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
          <w:type w:val="nextPage"/>
        </w:sectPr>"#,
        "",
    ));
    // Section 1 (landscape): a content paragraph, then the BODY-level sectPr
    // (landscape geometry — orient=landscape, swapped page dimensions).
    body.push_str(&p("", &r("", "Landscape section two.")));
    body.push_str(
        r#"<w:sectPr>
          <w:pgSz w:w="15840" w:h="12240" w:orient="landscape"/>
          <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
          <w:type w:val="nextPage"/>
        </w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
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
</Relationships>"#
        .to_string();

    build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
    ])
}

#[test]
fn mixed_sections_render_at_their_own_geometry() {
    let viewer = layout(two_section_mixed_docx());
    let pages = viewer.pages.borrow();
    assert!(pages.len() >= 2, "expected ≥2 pages (portrait + landscape), got {}", pages.len());

    // (a) the first page belongs to section 0, the last to section 1.
    let first = &pages[0];
    let last = pages.last().unwrap();
    assert_eq!(first.section_idx, 0, "page 0 must be stamped section 0");
    assert_eq!(last.section_idx, 1, "the landscape page must be stamped section 1");

    // (b) per-section geometry: section 0 portrait, section 1 landscape.
    let geo0 = viewer.page_geometry(first.section_idx);
    let geo1 = viewer.page_geometry(last.section_idx);
    assert!(geo0.width_px < geo0.height_px,
        "section 0 must be portrait (w {} < h {})", geo0.width_px, geo0.height_px);
    assert!(geo1.width_px > geo1.height_px,
        "section 1 must be landscape (w {} > h {})", geo1.width_px, geo1.height_px);
    // (c) the two sections resolve to DIFFERENT sizes (the bug rendered both same).
    assert!((geo0.width_px - geo1.width_px).abs() > 1.0,
        "landscape width {} must differ from portrait width {}", geo1.width_px, geo0.width_px);
    // The landscape page is wider than the portrait page and shorter.
    assert!(geo1.width_px > geo0.width_px, "landscape wider than portrait");
    assert!(geo1.height_px < geo0.height_px, "landscape shorter than portrait");
}

/// Guard the public `pageSize(idx)` path: the wasm-bindgen `page_size_js` is a
/// thin wrapper over `page_geometry_at`, which resolves a *page* index through
/// its stamped `section_idx` to the section geometry. Asserting at two page
/// indices that they report DIFFERENT sizes covers the index→section→geometry
/// composition the API reports to JS — the one path the section-level tests did
/// not exercise (page_size_js returns an opaque JsValue, not native-callable).
#[test]
fn page_geometry_at_index_differs_across_sections() {
    let viewer = layout(two_section_mixed_docx());
    let n = viewer.pages.borrow().len();
    assert!(n >= 2, "expected ≥2 pages, got {n}");

    let geo0 = viewer.page_geometry_at(0);
    let (w0, h0) = (geo0.width_px, geo0.height_px);
    let geo_last = viewer.page_geometry_at(n - 1);
    let (wl, hl) = (geo_last.width_px, geo_last.height_px);

    assert!(w0 < h0, "page 0 portrait via pageSize path (w {w0} < h {h0})");
    assert!(wl > hl, "last page landscape via pageSize path (w {wl} > h {hl})");
    assert!(
        (w0 - wl).abs() > 1.0,
        "pageSize(0) width {w0} must differ from pageSize(last) width {wl}"
    );

    // An out-of-range page index must fall back to section-0 geometry, not panic.
    let geo_oob = viewer.page_geometry_at(n + 100);
    assert_eq!(
        geo_oob.width_px, w0,
        "out-of-range page idx falls back to section-0 width"
    );
}

/// Guard: a SINGLE-section doc stamps every page section 0 and every page
/// resolves to the doc-level `page_size` — the per-section machinery is inert,
/// so the overwhelming common case stays byte-identical (see oracle hashes).
#[test]
fn single_section_geometry_is_inert() {
    let viewer = layout(oracle_flow_docx());
    let pages = viewer.pages.borrow();
    assert!(pages.len() >= 2, "oracle-flow should paginate to multiple pages");
    for (i, page) in pages.iter().enumerate() {
        assert_eq!(page.section_idx, 0, "single-section page {i} must stamp section 0");
        let geo = viewer.page_geometry(page.section_idx);
        assert_eq!(geo.width_px, viewer.page_size.width_px,
            "page {i} width must equal the doc-level page_size (fallback inert)");
        assert_eq!(geo.height_px, viewer.page_size.height_px,
            "page {i} height must equal the doc-level page_size (fallback inert)");
    }
}

// ─── §17.4.66 cell tcBorders nil suppresses the inherited style border ────────
// A cell's explicit `<w:tcBorders>` edge set to nil/none WINS over the table /
// table-style border (conflict rule 6: "the cell border always wins"). The
// table below references a table style ("Grid") whose tblBorders are full single
// (exercises extract_table_border_edges's style-fallback), and every cell sets
// ALL edges to nil via BOTH alias spellings (top/start/bottom/end on one cell,
// top/left/bottom/right on the other, across two rows so the interior insideH/
// insideV edges are covered too). Correct render = ZERO borders. Borders are NOT
// part of geometry_dump, so this cannot ride a hash pin — the test reads
// border_* directly.

const GRID_TABLE_STYLE: &str = r#"<w:style w:type="table" w:styleId="Grid"><w:name w:val="Table Grid"/><w:tblPr><w:tblBorders><w:top w:val="single" w:sz="4" w:color="000000"/><w:left w:val="single" w:sz="4" w:color="000000"/><w:bottom w:val="single" w:sz="4" w:color="000000"/><w:right w:val="single" w:sz="4" w:color="000000"/><w:insideH w:val="single" w:sz="4" w:color="000000"/><w:insideV w:val="single" w:sz="4" w:color="000000"/></w:tblBorders></w:tblPr></w:style>"#;

/// Table referencing style "Grid" (full single borders); every cell suppresses
/// all edges with nil, alternating the left/right vs start/end alias spellings.
fn cell_nil_over_grid_style_docx() -> Vec<u8> {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>{GRID_TABLE_STYLE}");
    // Alias variant A: top/start/bottom/end + insideH/insideV all nil.
    let cell_a = r#"<w:tc><w:tcPr><w:tcBorders><w:top w:val="nil"/><w:start w:val="nil"/><w:bottom w:val="nil"/><w:end w:val="nil"/><w:insideH w:val="nil"/><w:insideV w:val="nil"/></w:tcBorders></w:tcPr><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc>"#;
    // Alias variant B: top/left/bottom/right + insideH/insideV all nil.
    let cell_b = r#"<w:tc><w:tcPr><w:tcBorders><w:top w:val="nil"/><w:left w:val="nil"/><w:bottom w:val="nil"/><w:right w:val="nil"/><w:insideH w:val="nil"/><w:insideV w:val="nil"/></w:tcBorders></w:tcPr><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc>"#;
    // Row 1: (A|B). Row 2: (B|A) — so the interior insideV edge of cell[1] is
    // suppressed via BOTH the start (row 2 cell A) and left (row 1 cell B) spellings.
    let body = format!(
        r#"<w:tbl><w:tblPr><w:tblStyle w:val="Grid"/><w:tblW w:w="4000" w:type="dxa"/></w:tblPr><w:tblGrid><w:gridCol w:w="2000"/><w:gridCol w:w="2000"/></w:tblGrid><w:tr>{cell_a}{cell_b}</w:tr><w:tr>{cell_b}{cell_a}</w:tr></w:tbl>"#
    );
    tabs_docx(&styles, &body)
}

/// CONTROL — same "Grid" style, cells with NO tcBorders: the style borders
/// still DO draw (proves the suppression above is scoped to the explicit nil).
fn cell_grid_style_no_override_docx() -> Vec<u8> {
    let styles = format!("<w:docDefaults>{TABS_DOC_DEFAULTS_FONT}</w:docDefaults>{GRID_TABLE_STYLE}");
    let cell = |t: &str| format!(r#"<w:tc><w:p><w:r><w:t>{t}</w:t></w:r></w:p></w:tc>"#);
    let body = format!(
        r#"<w:tbl><w:tblPr><w:tblStyle w:val="Grid"/><w:tblW w:w="4000" w:type="dxa"/></w:tblPr><w:tblGrid><w:gridCol w:w="2000"/><w:gridCol w:w="2000"/></w:tblGrid><w:tr>{}{}</w:tr><w:tr>{}{}</w:tr></w:tbl>"#,
        cell("a"), cell("b"), cell("c"), cell("d")
    );
    tabs_docx(&styles, &body)
}

#[test]
fn cell_nil_borders_suppress_style_grid() {
    let v = layout(cell_nil_over_grid_style_docx());
    let pages = v.pages.borrow();
    let mut cells = 0;
    for page in pages.iter() {
        for el in &page.elements {
            if let PageElement::TableRow(row) = el {
                for c in &row.cells {
                    cells += 1;
                    assert!(
                        c.border_top.is_none() && c.border_bottom.is_none()
                            && c.border_left.is_none() && c.border_right.is_none(),
                        "explicit tcBorders nil must suppress the inherited style border",
                    );
                }
            }
        }
    }
    assert!(cells >= 4, "fixture must produce the 2x2 grid (got {cells} cells)");
    drop(pages);

    // Control: same style, cells WITHOUT tcBorders → the style borders draw.
    let vc = layout(cell_grid_style_no_override_docx());
    let some = vc
        .pages
        .borrow()
        .iter()
        .flat_map(|p| p.elements.iter())
        .filter_map(|e| if let PageElement::TableRow(r) = e { Some(r) } else { None })
        .flat_map(|r| r.cells.iter())
        .any(|c| c.border_top.is_some());
    assert!(some, "un-suppressed style Table Grid must still draw borders");
}


// ─── image-line pagination integration red-green ─────────────────────────────
// A landscape section whose content band is 624px tall. A keepNext Heading1
// ("Appendix E") is immediately followed by a paragraph carrying a wp:inline
// drawing (the figure) plus a 72pt text run — so the image line's
// content_height folds the image ascent AND a fat (~17px) font descent. With
// the pre-fix line_height, that folded descent inflates the image line by
// ~20px, tipping heading+figure onto two pages; the fix measures the drawing
// extent and keeps them together on ONE page. The 72pt descent gives a wide
// straddle window so the test does not hinge on a knife-edge metric.
fn cmp_landscape_figure_docx(cy_emu: i64) -> Vec<u8> {
    const CX_EMU: i64 = 4_572_000; // 5in wide figure

    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults>
    <w:rPrDefault><w:rPr>
      <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
      <w:sz w:val="22"/><w:szCs w:val="22"/>
    </w:rPr></w:rPrDefault>
    <w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault>
  </w:docDefaults>
  <w:style w:type="paragraph" w:styleId="Heading1">
    <w:name w:val="heading 1"/>
    <w:pPr><w:keepNext/><w:spacing w:before="240" w:after="120"/></w:pPr>
    <w:rPr><w:b/><w:sz w:val="32"/></w:rPr>
  </w:style>
</w:styles>"#
    );

    // Inline figure: wp:inline (in-flow), extent CX×CY, wired to rId10; followed
    // by a 72pt run so the line's descent below the image baseline is large.
    let figure = format!(
        r#"<w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0">
  <wp:extent cx="{CX_EMU}" cy="{cy_emu}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:docPr id="1" name="Fig"/>
  <wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect="1"/></wp:cNvGraphicFramePr>
  <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">
    <pic:pic><pic:nvPicPr><pic:cNvPr id="1" name="Fig"/><pic:cNvPicPr/></pic:nvPicPr>
    <pic:blipFill><a:blip r:embed="rId10"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>
    <pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{CX_EMU}" cy="{cy_emu}"/></a:xfrm>
    <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic>
  </a:graphicData></a:graphic>
</wp:inline></w:drawing></w:r><w:r><w:rPr><w:sz w:val="144"/></w:rPr><w:t xml:space="preserve">.</w:t></w:r>"#
    );

    let mut body = String::new();
    body.push_str(&p(r#"<w:pStyle w:val="Heading1"/>"#, &r("", "Appendix E")));
    body.push_str(&format!("<w:p>{figure}</w:p>"));
    // Landscape section geometry: 15840×12240 tw, margins 1440 → 624px band.
    body.push_str(
        r#"<w:sectPr>
      <w:pgSz w:w="15840" w:h="12240" w:orient="landscape"/>
      <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
    </w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"><w:body>{body}</w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
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
  <Relationship Id="rId10" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/oracle.png"/>
</Relationships>"#
        .to_string();

    build_docx(&[
        ("[Content_Types].xml", content_types.into_bytes()),
        ("_rels/.rels", root_rels.into_bytes()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.into_bytes()),
        ("word/styles.xml", styles.into_bytes()),
        ("word/media/oracle.png", make_png(64, 64)),
    ])
}

/// Integration red-green for image-line auto-multiplier inflation.
/// This mirrors calibration doc C4's Appendix-E boundary with SYNTHESIZED bytes: a
/// keepNext heading immediately followed by a near-full-band inline figure whose
/// 72pt companion run folds a ~20px descent into the line's content_height. The
/// figure (CY 5_380_000 EMU ≈ 564.8px) sits ~9.6px clear of the 624px landscape
/// band once the descent is NOT added; with the pre-fix `+ descent` inflation
/// (~20px) it overflows and the figure divorces its heading onto page 2. Because
/// the descent window is ~20px wide (72pt run), the pass/fail margin is ~10px on
/// each side — robust to small Arimo metric drift, unlike C4's 4px knife-edge.
/// The deterministic red-green lives in `paragraph.rs::line_height_tests`; this
/// proves the two decision sites (keepNext reservation + fastpath overflow) read
/// the corrected height end-to-end.
#[test]
fn cmp_landscape_figure_shares_one_page() {
    let v = layout(cmp_landscape_figure_docx(5_380_000));
    let pages = v.pages.borrow();
    assert_eq!(pages.len(), 1, "heading + figure must share ONE page (got {})", pages.len());
    // Both a heading line and the (much taller) image line live on page 0.
    let heights: Vec<f64> = pages[0].elements.iter()
        .filter_map(|el| if let PageElement::Line(l) = el { Some(l.line_height) } else { None })
        .collect();
    assert!(heights.iter().any(|&h| h < 40.0), "heading line missing on page 0: {heights:?}");
    assert!(heights.iter().any(|&h| h > 500.0), "figure line missing on page 0: {heights:?}");
}

// §17.3.1.15 keepNext binds a paragraph to the START of the FOLLOWING block —
// including the first row of a following TABLE. This SYNTHESIZED fixture models
// calibration doc C4's Appendix-D construct (a keepNext heading immediately
// followed by a table) WITHOUT depending on the document itself (not distributed): a
// Heading1 style carrying <w:keepNext/> ("Appendix D") is seated near the bottom
// of a 624px landscape band by filler, immediately followed by a single-column
// table whose FIRST row is deliberately tall (a 40pt run) so the straddle window
// (heading fits alone, heading+row0 does not) is wide (~110px) — robust to Arimo
// metric drift. Pre-fix the Table arm reserved 0px, so the heading placed on the
// earlier page and its table's first row orphaned to the next page; the fix
// reserves rows[0].height_px so the heading rides to the SAME page as its row.
fn keepnext_heading_rides_with_table_docx() -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults><w:rPrDefault><w:rPr>
    <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/>
    <w:sz w:val="22"/><w:szCs w:val="22"/>
  </w:rPr></w:rPrDefault>
  <w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults>
  <w:style w:type="paragraph" w:styleId="Heading1">
    <w:name w:val="heading 1"/>
    <w:pPr><w:keepNext/><w:spacing w:before="240" w:after="120"/></w:pPr>
    <w:rPr><w:b/><w:sz w:val="48"/></w:rPr>
  </w:style>
</w:styles>"#
    );

    let mut body = String::new();
    // Filler seats the heading near the band bottom so the following table's tall
    // first row would NOT fit the remainder (heading fits alone; heading+row0
    // does not). Count (22) tuned for the 624px landscape band + Arimo metrics.
    for i in 0..22 {
        body.push_str(&p("", &r("", &filler(i, 1))));
    }
    // keepNext heading immediately followed by the table (no intervening block).
    body.push_str(&p(r#"<w:pStyle w:val="Heading1"/>"#, &r("", "Appendix D")));
    body.push_str(
        r#"<w:tbl><w:tblPr><w:tblW w:w="9360" w:type="dxa"/><w:tblBorders><w:top w:val="single" w:sz="4"/><w:left w:val="single" w:sz="4"/><w:bottom w:val="single" w:sz="4"/><w:right w:val="single" w:sz="4"/><w:insideH w:val="single" w:sz="4"/><w:insideV w:val="single" w:sz="4"/></w:tblBorders></w:tblPr><w:tblGrid><w:gridCol w:w="9360"/></w:tblGrid>"#,
    );
    // Row 0: single tall cell (40pt run). Text is deliberately SHORT ("D1") so it
    // is unambiguously ONE line at 40pt across the 6.5in column — that keeps it on
    // the non-splittable whole-row path pre-fix (a two-line row would be split-
    // eligible and could vacuously pass the assertion).
    body.push_str(r#"<w:tr><w:tc><w:tcPr><w:tcW w:w="9360" w:type="dxa"/></w:tcPr>"#);
    body.push_str(&p("", &r(r#"<w:sz w:val="80"/>"#, "D1")));
    body.push_str("</w:tc></w:tr>");
    // A couple of normal rows so the table is more than one line total.
    for row in 1..4 {
        body.push_str(&format!(
            r#"<w:tr><w:tc><w:tcPr><w:tcW w:w="9360" w:type="dxa"/></w:tcPr>{}</w:tc></w:tr>"#,
            p("", &r("", &format!("data row {row}")))
        ));
    }
    body.push_str("</w:tbl>");
    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="15840" w:h="12240" w:orient="landscape"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
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
</Relationships>"#
        .to_string();

    build_docx(&[
        ("[Content_Types].xml", content_types.into_bytes()),
        ("_rels/.rels", root_rels.into_bytes()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.into_bytes()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

#[test]
fn keepnext_heading_rides_with_following_table() {
    let v = layout(keepnext_heading_rides_with_table_docx());
    let pages = v.pages.borrow();
    // Heading is the tallest TOP-LEVEL Line (24pt Heading1 vs 11pt filler); the
    // table's tall 40pt line lives INSIDE a PageElement::TableRow, never a top-
    // level Line, so it can't be confused with the heading.
    let max_lh = |pg: &super::wml::page::PageLayout| {
        pg.elements
            .iter()
            .filter_map(|el| if let PageElement::Line(l) = el { Some(l.line_height) } else { None })
            .fold(0.0_f64, f64::max)
    };
    let heading_page = (0..pages.len())
        .max_by(|&a, &b| max_lh(&pages[a]).partial_cmp(&max_lh(&pages[b])).unwrap())
        .expect("some page has a Line");
    let table_page = pages
        .iter()
        .position(|pg| pg.elements.iter().any(|el| matches!(el, PageElement::TableRow(_))))
        .expect("table row present");
    // GREEN (post-fix): the keepNext heading rides to the SAME page as its
    // table's first row. RED (pre-fix): heading orphans on the earlier page,
    // table's first row flows to the next page -> heading_page < table_page.
    assert_eq!(
        heading_page, table_page,
        "keepNext heading (p{heading_page}) must ride with its following table's first row (p{table_page}); \
         pre-fix it orphans one page earlier because the Table arm reserved 0px"
    );
    // Sanity: construction actually straddled a boundary (heading not on page 0),
    // so the assertion above is load-bearing rather than vacuous.
    assert!(
        heading_page >= 1,
        "fixture must seat the heading past page 0 (increase filler count); got heading on p0"
    );
    // And page 0 (the page the heading LEFT) ends with filler, proving the heading moved.
    let orphaned_heading_on_prev = pages[heading_page - 1].elements.iter().any(|el| {
        matches!(el, PageElement::Line(l) if (l.line_height - max_lh(&pages[heading_page])).abs() < 0.5)
    });
    assert!(
        !orphaned_heading_on_prev,
        "the heading line must NOT remain on the previous page (it orphaned pre-fix)"
    );
}

// ─── OMML math: core constructs ──────────────────────────────────────────────
// Synthesized in-tree fixtures (build_docx). Each declares the math namespace on
// the document root and embeds m:oMath / m:oMathPara with the target construct.
// The RED state before implementation is intrinsic: math lived only in
// extra_children (invisible to the run walk), so ZERO math Items existed — every
// `count >= 1` / seating / geometry assertion below failed.

const M_NS: &str =
    r#"xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math""#;

/// Minimal single-section docx (Arimo body, styles-only) whose body is `body`.
/// The document root carries the WML, relationship AND math namespaces.
fn math_docx(body: &str) -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault></w:docDefaults></w:styles>"#
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS} {M_NS}><w:body>{body}<w:sectPr>
  <w:pgSz w:w="12240" w:h="15840"/>
  <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
</w:sectPr></w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
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
</Relationships>"#
        .to_string();

    build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
    ])
}

/// One laid-out math glyph node (text + placement + point size + italic flag).
struct GlyphSnap {
    /// The LOGICAL character (math-alphabet code points reverse-mapped
    /// back to ASCII), so the per-construct sanity lookups (`dy("a")`, …) keep
    /// resolving a variable regardless of the styled plane it renders in.
    text: String,
    /// The ACTUAL painted text — the math-alphanumeric code point the run
    /// shapes+paints (e.g. U+1D465 for a math-italic 'x', U+211D for ℝ). Coverage
    /// tests assert on this.
    raw: String,
    dx: f64,
    dy: f64,
    font_pt: f64,
    italic: bool,
}

/// Reverse the math-alphabet map: a Mathematical Alphanumeric Symbols
/// code point (or a letterlike-symbol hole) back to its base ASCII letter/digit.
/// Built by inverting [`math_alpha_pure`] over every plane it emits, so it can
/// never drift from the forward map.
fn demath_char(c: char) -> char {
    use crate::wml::math::{math_alpha_pure, MathPlane::*};
    if (c as u32) < 0x80 {
        return c; // fast path: plain ASCII is never a mapped code point
    }
    for base in ('A'..='Z').chain('a'..='z').chain('0'..='9') {
        for plane in [
            Italic, Bold, BoldItalic, Script, BoldScript, Fraktur, BoldFraktur,
            DoubleStruck, SansSerif, SansBold, SansItalic, SansBoldItalic, Monospace,
        ] {
            if math_alpha_pure(base, plane) == c {
                return base;
            }
        }
    }
    c
}

fn demath_str(s: &str) -> String {
    s.chars().map(demath_char).collect()
}

/// One laid-out fraction/vinculum Rule node.
struct RuleSnap {
    x: f64,
    y: f64,
    w: f64,
}

/// One laid-out border-box VRule (vertical edge) node.
struct VRuleSnap {
    x: f64,
    y: f64,
    height: f64,
}

/// A math atom snapshot lifted from a laid-out page (in-crate access to the
/// pub(crate) MathBox/MathNode).
struct MathSnap {
    width: f64,
    ascent: f64,
    descent: f64,
    glyphs: Vec<GlyphSnap>,
    rules: Vec<RuleSnap>,
    vrules: Vec<VRuleSnap>,
}

impl MathSnap {
    /// dy of the first glyph whose text equals `t`.
    fn dy(&self, t: &str) -> Option<f64> {
        self.glyphs.iter().find(|g| g.text == t).map(|g| g.dy)
    }
    /// dx of the first glyph whose text equals `t`.
    fn dx(&self, t: &str) -> Option<f64> {
        self.glyphs.iter().find(|g| g.text == t).map(|g| g.dx)
    }
    /// point size of the first glyph whose text equals `t`.
    fn font_pt(&self, t: &str) -> Option<f64> {
        self.glyphs.iter().find(|g| g.text == t).map(|g| g.font_pt)
    }
    /// italic flag of the first glyph whose text equals `t`. (The math face routes
    /// slant through the code point, so most callers now assert `raw` instead;
    /// kept for the no-math-face fallback assertion.)
    #[allow(dead_code)]
    fn italic(&self, t: &str) -> Option<bool> {
        self.glyphs.iter().find(|g| g.text == t).map(|g| g.italic)
    }
}

/// One laid-out inline item's essentials: its text (empty for a math atom),
/// whether it is a math atom, its width, and the math snapshot if any.
struct ItemSnap {
    text: String,
    is_math: bool,
    math: Option<MathSnap>,
    /// The line's total height (so callers can compare line growth).
    line_height: f64,
    line_text: String,
    /// The item's segment `x_start` and the line's content edges (for centering).
    seg_x_start: f64,
    content_left: f64,
    content_right: f64,
}

/// Flatten page-0 lines into an in-order item stream with math details.
fn item_snaps(viewer: &Viewer) -> Vec<ItemSnap> {
    use crate::wml::math::MathNode;
    let pages = viewer.pages.borrow();
    let mut out = Vec::new();
    for page in pages.iter() {
        for el in &page.elements {
            if let PageElement::Line(l) = el {
                let ltext = line_text(l);
                for seg in &l.segments {
                    for it in &seg.items {
                        let math = it.math.as_ref().map(|m| {
                            let mut glyphs = Vec::new();
                            let mut rules = Vec::new();
                            let mut vrules = Vec::new();
                            for n in &m.nodes {
                                match n {
                                    MathNode::Glyph { dx, dy, text, style, .. } => {
                                        glyphs.push(GlyphSnap {
                                            text: demath_str(text),
                                            raw: text.clone(),
                                            dx: *dx,
                                            dy: *dy,
                                            font_pt: style.font_pt,
                                            italic: style.italic,
                                        });
                                    }
                                    MathNode::Rule { x, y, w, .. } => {
                                        rules.push(RuleSnap { x: *x, y: *y, w: *w });
                                    }
                                    MathNode::VRule { x, y, height, .. } => {
                                        vrules.push(VRuleSnap { x: *x, y: *y, height: *height });
                                    }
                                    // A MATH size-variant glyph painted by outline
                                    // (math face). Surface it as a glyph so the
                                    // stretchy-construct sanity lookups (a grown
                                    // '(' / '√' / '∑' by dx / font_pt) still work.
                                    MathNode::GlyphOutline { dx, dy, ch, pt_equiv, .. } => {
                                        glyphs.push(GlyphSnap {
                                            text: demath_str(ch),
                                            raw: ch.clone(),
                                            dx: *dx,
                                            dy: *dy,
                                            font_pt: *pt_equiv,
                                            italic: false,
                                        });
                                    }
                                }
                            }
                            MathSnap {
                                width: m.width,
                                ascent: m.ascent,
                                descent: m.descent,
                                glyphs,
                                rules,
                                vrules,
                            }
                        });
                        out.push(ItemSnap {
                            is_math: it.math.is_some(),
                            text: it.text.clone(),
                            math,
                            line_height: l.line_height,
                            line_text: ltext.clone(),
                            seg_x_start: seg.x_start,
                            content_left: l.content_left,
                            content_right: l.content_right,
                        });
                    }
                }
            }
        }
    }
    out
}

#[test]
fn math_fraction_stacks_and_seats_between_runs() {
    // "x=" | [a/b fraction] | "+1", in a paragraph WITH a pPr (so the parser's
    // child_idx is offset by +1 vs the naive content ordinal — the seating bug
    // this guards against). A separate plain paragraph gives a line-height
    // reference.
    let body = format!(
        "{plain}{frac}",
        plain = r#"<w:p><w:r><w:t xml:space="preserve">reference</w:t></w:r></w:p>"#,
        frac = concat!(
            r#"<w:p><w:pPr><w:spacing w:after="0"/></w:pPr>"#,
            r#"<w:r><w:t xml:space="preserve">x=</w:t></w:r>"#,
            r#"<m:oMath><m:f><m:num><m:r><m:t>a</m:t></m:r></m:num>"#,
            r#"<m:den><m:r><m:t>b</m:t></m:r></m:den></m:f></m:oMath>"#,
            r#"<w:r><w:t xml:space="preserve">+1</w:t></w:r></w:p>"#,
        ),
    );
    let viewer = layout(math_docx(&body));
    let items = item_snaps(&viewer);

    // 1. Exactly one math atom exists (RED: zero before impl).
    let math_count = items.iter().filter(|i| i.is_math).count();
    assert_eq!(math_count, 1, "expected exactly one math atom, got {math_count}");

    // 2. Horizontal seating: the atom sits strictly between the 'x=' and '+1'
    //    run items (catches the pPr child_idx off-by-one).
    let idx_xeq = items.iter().position(|i| i.text == "x=").expect("'x=' run item");
    let idx_math = items.iter().position(|i| i.is_math).expect("math item");
    let idx_plus = items.iter().position(|i| i.text == "+1").expect("'+1' run item");
    assert!(
        idx_xeq < idx_math && idx_math < idx_plus,
        "math atom mis-seated: x=@{idx_xeq}, math@{idx_math}, +1@{idx_plus} (expected x= < math < +1)"
    );

    // 3. The atom has positive width and stacked geometry.
    let m = items[idx_math].math.as_ref().unwrap();
    assert!(m.width > 0.0, "math width must be > 0, got {}", m.width);
    assert!(
        m.ascent > 0.0 && m.descent > 0.0,
        "fraction must reserve ascent+descent (got a{:.2} d{:.2})",
        m.ascent, m.descent
    );

    // 4. Numerator 'a' sits ABOVE the denominator 'b' (dy less = higher), with a
    //    Rule between them.
    let a_dy = m.dy("a").expect("numerator glyph 'a'");
    let b_dy = m.dy("b").expect("denominator glyph 'b'");
    assert!(a_dy < b_dy, "numerator 'a' (dy {a_dy:.2}) must be above denominator 'b' (dy {b_dy:.2})");
    assert!(!m.rules.is_empty(), "fraction must have a Rule (bar) node");
    let rule_y = m.rules[0].y;
    assert!(
        a_dy < rule_y && rule_y < b_dy,
        "bar (y {rule_y:.2}) must sit between numerator (dy {a_dy:.2}) and denominator (dy {b_dy:.2})"
    );

    // 5. The fraction line is TALLER than the plain reference line.
    let frac_lh = items.iter().find(|i| i.is_math).unwrap().line_height;
    let ref_lh = items.iter().find(|i| i.line_text.contains("reference")).unwrap().line_height;
    assert!(
        frac_lh > ref_lh + 0.5,
        "fraction line height ({frac_lh:.2}) must exceed the plain line ({ref_lh:.2})"
    );
}

/// Same as `math_docx`, but the root also declares the DrawingML / anchor
/// namespaces so the body can carry a `wp:anchor` alongside `m:oMath`. Kept
/// separate because every other math fixture is float-free and pays nothing
/// for these declarations.
fn math_float_docx(body: &str) -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault></w:docDefaults></w:styles>"#
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS} {M_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}<w:sectPr>
  <w:pgSz w:w="12240" w:h="15840"/>
  <w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
</w:sectPr></w:body></w:document>"#
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

/// A 2in-square anchored text box at the column left, wrapping text to its
/// right — the cheapest float there is (no image relationship needed). Shared by
/// the two float fixtures below so the anchor XML is spelled once.
fn wrap_square_textbox_anchor(box_emu: i64) -> String {
    format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV>
  <wp:extent cx="{box_emu}" cy="{box_emu}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapSquare wrapText="right"/>
  <wp:docPr id="1" name="TextBox 1"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp>
      <wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{box_emu}" cy="{box_emu}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">Boxed.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/>
    </wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    )
}

/// 2in in EMU — the float's extent in both float fixtures.
const TWO_INCH_EMU: i64 = 1_828_800;

/// Float packer, rule 2 — a tab on a line that wraps around a float must resolve against
/// the stop chain, in the float band.
///
/// The float-aware packer took no `ResolvedTabStops` at all and never called
/// `next_stop`. It re-packed items whose tab advances had already been resolved
/// by `pack_lines`, against the FLOAT-BLIND geometry: a line starting at the
/// column edge rather than 2in into it. Those stale advances then rode straight
/// through, so the post-tab word landed one float-width past its stop. No
/// fixture in the suite paired a `w:tab` with a `wp:anchor`, so nothing saw it.
#[test]
fn tab_inside_a_float_band_resolves_against_the_stop() {
    // Content box 96..720. The 2in float takes 96..288, so the text band is
    // 288..720. A left stop at 5760tw = 384px from the paragraph's leading edge
    // therefore lands the post-tab word at 96 + 384 = 480 — INSIDE the band, so
    // the stop is genuinely reachable and the clamp below is not what is pinned.
    let body = format!(
        r#"<w:p><w:pPr><w:spacing w:after="0"/><w:tabs><w:tab w:val="left" w:pos="5760"/></w:tabs></w:pPr>{anchor}<w:r><w:t xml:space="preserve">AB	CD</w:t></w:r></w:p>"#,
        anchor = wrap_square_textbox_anchor(TWO_INCH_EMU),
    ).replace(r"	", "	");

    let viewer = layout(math_float_docx(&body));
    let lines = body_lines(&viewer);
    let items = line_item_positions(&lines[0]);

    // Fixture guard: this line must have come from the FLOAT packer — it starts
    // in the band, not at the column edge.
    let x0 = lines[0].segments.first().map(|s| s.x_start).expect("a segment");
    assert!(
        x0 > 280.0,
        "the line did not wrap around the float (starts at x{x0:.2}) — the fixture          no longer exercises pack_lines_around_floats"
    );
    let (_, cx, _) = items.iter().find(|(t, _, _)| t == "CD")
        .unwrap_or_else(|| panic!("item CD not found in {items:?}"));
    assert!(
        (cx - 480.0).abs() < 0.5,
        "CD at {cx} != 480 — the tab in the float band kept the advance          `pack_lines` resolved for the float-BLIND line (§17.3.1.37)"
    );
}

/// Float packer, rule 3 — a base-RTL paragraph that wraps around a float must be PACKED
/// in logical order and reordered (UAX#9 L2) once, at placement.
///
/// The float-aware packer took no base direction and never reordered. Worse, the
/// items it was handed came from flattening `ParagraphCache::lines`, which
/// `pack_lines` has ALREADY permuted into visual order — so the float band
/// re-broke a visual sequence, and the words landed on lines in reverse. The
/// paragraph's last words appeared on its FIRST line. Nothing in the suite
/// paired `w:bidi` with a `wp:anchor` (the census in the attributes-oracle
/// header records RTL items 0 across 269 documents), so nothing saw it.
///
/// The fixture pins the property, not pixel positions: every line must hold a
/// CONTIGUOUS run of the logical word sequence, and the runs must advance down
/// the page — which is exactly what packing-in-logical-order means.
#[test]
fn rtl_paragraph_around_a_float_packs_in_logical_order() {
    // 18 distinct Hebrew words, in logical order. Four characters each so the
    // 2in float leaves a band too narrow to hold them all on one line — a
    // single-line paragraph is trivially "in order" and would prove nothing.
    const LETTERS: [&str; 18] = [
        "אאאא", "בבבב", "גגגג", "דדדד", "הההה", "וווו",
        "זזזז", "חחחח", "טטטט", "יייי", "כככך", "לללל",
        "מממם", "נננן", "סססס", "עעעע", "פפפף", "צצצץ",
    ];
    let text = LETTERS.join(" ");
    let body = format!(
        r#"<w:p><w:pPr><w:bidi/><w:spacing w:after="0"/></w:pPr>{anchor}<w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>"#,
        anchor = wrap_square_textbox_anchor(TWO_INCH_EMU),
    );
    let viewer = layout(math_float_docx(&body));
    let lines = body_lines(&viewer);

    // Logical index of each letter, by first appearance in the document text.
    let logical = |t: &str| LETTERS.iter().position(|l| *l == t.trim());

    // Fixture guard 1: the float must actually re-break the paragraph, otherwise
    // a single line is trivially "in order" and the test proves nothing.
    let word_lines: Vec<Vec<usize>> = lines.iter()
        .map(|l| line_item_positions(l).iter().filter_map(|(t, _, _)| logical(t)).collect::<Vec<_>>())
        .filter(|v: &Vec<usize>| !v.is_empty())
        .collect();
    assert!(
        word_lines.len() >= 2,
        "the float band must break this paragraph into ≥2 lines; got {word_lines:?}"
    );
    // Fixture guard 2: the text really did reorder — a visual line of base-RTL
    // items reads right-to-left, so indices DESCEND within a line.
    assert!(
        word_lines.iter().any(|v| v.len() >= 2 && v[0] > v[1]),
        "no line came back in visual (descending) order — the L2 reorder never \
         fired and this fixture is not testing bidi at all: {word_lines:?}"
    );

    // THE property: line N holds a contiguous logical run, and line N+1 holds the
    // run that follows it. Packing a VISUAL sequence puts the paragraph's LAST
    // words on its FIRST line, which fails here immediately.
    let mut next = 0usize;
    for (n, v) in word_lines.iter().enumerate() {
        let mut sorted = v.clone();
        sorted.sort_unstable();
        let want: Vec<usize> = (next..next + v.len()).collect();
        assert_eq!(
            sorted, want,
            "line {n} holds logical words {sorted:?}, expected the contiguous run \
             {want:?} — the float packer broke a VISUAL sequence, so the \
             paragraph's words landed on the wrong lines ({word_lines:?})"
        );
        next += v.len();
    }
    assert_eq!(next, LETTERS.len(), "every word must be placed; got {word_lines:?}");
}

/// Float packer, rule 3, the SIBLING CASE its base-RTL twin above cannot reach: an
/// LTR-base paragraph containing a complex-script run.
///
/// The two gates disagreed. `ParagraphCache::logical_items` is populated when
/// `bidi::may_reorder` holds — `base_rtl` OR any complex-script text — but the
/// float placer re-gated the reorder on `base_rtl` ALONE. So a `<w:bidi/>`-less
/// paragraph with Hebrew in it was carried in LOGICAL order and then never
/// reordered: the Hebrew rendered left-to-right, backwards, on every float path.
/// One predicate spelled two ways, and the base-RTL fixture above is blind to it
/// because `base_rtl` makes both spellings agree.
///
/// Pins the property, not positions: within a line, the complex-script run must
/// come back in VISUAL (descending-logical) order.
#[test]
fn ltr_paragraph_with_rtl_run_around_a_float_still_reorders() {
    // No <w:bidi/> — base direction is LTR. The Hebrew run is what makes
    // `may_reorder` true, and it is the only thing that does.
    const LETTERS: [&str; 18] = [
        "אאאא", "בבבב", "גגגג", "דדדד", "הההה", "וווו",
        "זזזז", "חחחח", "טטטט", "יייי", "כככך", "לללל",
        "מממם", "נננן", "סססס", "עעעע", "פפפף", "צצצץ",
    ];
    let text = LETTERS.join(" ");
    let body = format!(
        r#"<w:p><w:pPr><w:spacing w:after="0"/></w:pPr>{anchor}<w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>"#,
        anchor = wrap_square_textbox_anchor(TWO_INCH_EMU),
    );
    let viewer = layout(math_float_docx(&body));
    let lines = body_lines(&viewer);
    let logical = |t: &str| LETTERS.iter().position(|l| *l == t.trim());

    let word_lines: Vec<Vec<usize>> = lines.iter()
        .map(|l| line_item_positions(l).iter().filter_map(|(t, _, _)| logical(t)).collect::<Vec<_>>())
        .filter(|v: &Vec<usize>| !v.is_empty())
        .collect();

    // Fixture guard: the float must actually break the paragraph, or a single
    // line proves nothing.
    assert!(
        word_lines.len() >= 2,
        "the float band must break this paragraph into ≥2 lines; got {word_lines:?}"
    );

    // THE property. An RTL run inside an LTR paragraph is still displayed
    // right-to-left, so within a line the logical indices DESCEND. With the
    // reorder skipped they ascend — the run reads backwards on screen.
    assert!(
        word_lines.iter().any(|v| v.len() >= 2 && v[0] > v[1]),
        "no line came back in visual (descending) order — L2 was SKIPPED for an \
         LTR-base paragraph carrying a complex-script run, so the Hebrew renders \
         backwards: {word_lines:?}"
    );
}

/// Float packer, rule 4 — a math atom must reserve its OWN stacked height on a line that
/// wraps around a FLOAT, exactly as it already does on a float-free line.
///
/// The two WML packers each own a per-item vertical-metric chain. OMML support
/// first added the `math` arm to `LayoutLine::push` only; the float packer's
/// copy fell through to the *font* branch (a math atom has empty `text` and no
/// `image`/`text_box`), so a fraction beside a float measured as ordinary text
/// and the line box came out short — the equation overprinted the line above.
/// No fixture in the suite paired `m:oMath` with a `wp:anchor`, so nothing saw
/// it. This is that fixture.
#[test]
fn math_beside_a_float_reserves_its_stacked_height() {
    let anchor = wrap_square_textbox_anchor(TWO_INCH_EMU);
    let body = format!(
        concat!(
            r#"<w:p><w:pPr><w:spacing w:after="0"/></w:pPr>{anchor}"#,
            r#"<w:r><w:t xml:space="preserve">x=</w:t></w:r>"#,
            r#"<m:oMath><m:f><m:num><m:r><m:t>a</m:t></m:r></m:num>"#,
            r#"<m:den><m:r><m:t>b</m:t></m:r></m:den></m:f></m:oMath>"#,
            r#"<w:r><w:t xml:space="preserve">+1</w:t></w:r></w:p>"#,
        ),
        anchor = anchor,
    );

    let viewer = layout(math_float_docx(&body));
    let items = item_snaps(&viewer);
    let atom = items.iter().find(|i| i.is_math).expect("the math atom must be laid out");
    let m = atom.math.as_ref().unwrap();

    // Guard against a vacuous pass: this line must genuinely have come from the
    // FLOAT packer. Its segment starts inside the column, indented past the
    // 2in box — a float-blind line would start at content_left.
    assert!(
        atom.seg_x_start > atom.content_left + 1.0,
        "the math line did not wrap around the float (seg x{:.2} vs content_left x{:.2}); \
         the fixture no longer exercises pack_lines_around_floats",
        atom.seg_x_start, atom.content_left
    );
    // A fraction stacks well above a plain text line, so this is a real bound.
    assert!(
        m.ascent + m.descent > 20.0,
        "the fraction atom should stack taller than a text line; got a{:.2} d{:.2}",
        m.ascent, m.descent
    );
    assert!(
        atom.line_height + 0.5 >= m.ascent + m.descent,
        "line beside the float is {:.2}px tall but the math atom needs {:.2}px \
         (a{:.2}+d{:.2}) — the float packer measured the atom as ordinary text",
        atom.line_height, m.ascent + m.descent, m.ascent, m.descent
    );
}

#[test]
fn math_superscript_raises_the_script() {
    let body = concat!(
        r#"<w:p><w:r><w:t xml:space="preserve">y=</w:t></w:r>"#,
        r#"<m:oMath><m:sSup><m:e><m:r><m:t>x</m:t></m:r></m:e>"#,
        r#"<m:sup><m:r><m:t>2</m:t></m:r></m:sup></m:sSup></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("math atom").math.as_ref().unwrap();
    assert!(m.width > 0.0, "superscript width must be > 0");
    let x_dy = m.dy("x").expect("base 'x'");
    let two_dy = m.dy("2").expect("script '2'");
    assert!(
        two_dy < x_dy,
        "superscript '2' (dy {two_dy:.2}) must be RAISED above base 'x' (dy {x_dy:.2})"
    );
}

#[test]
fn math_subscript_lowers_the_script() {
    let body = concat!(
        r#"<w:p><m:oMath><m:sSub><m:e><m:r><m:t>x</m:t></m:r></m:e>"#,
        r#"<m:sub><m:r><m:t>i</m:t></m:r></m:sub></m:sSub></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("math atom").math.as_ref().unwrap();
    let x_dy = m.dy("x").expect("base 'x'");
    let i_dy = m.dy("i").expect("script 'i'");
    assert!(i_dy > 0.0, "subscript 'i' dy ({i_dy:.2}) must be LOWERED (> baseline)");
    assert!(i_dy > x_dy, "subscript 'i' (dy {i_dy:.2}) must sit below base 'x' (dy {x_dy:.2})");
}

#[test]
fn math_subsuperscript_stacks_both() {
    let body = concat!(
        r#"<w:p><m:oMath><m:sSubSup><m:e><m:r><m:t>x</m:t></m:r></m:e>"#,
        r#"<m:sub><m:r><m:t>i</m:t></m:r></m:sub>"#,
        r#"<m:sup><m:r><m:t>2</m:t></m:r></m:sup></m:sSubSup></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("math atom").math.as_ref().unwrap();
    let two_dy = m.dy("2").expect("sup '2'");
    let i_dy = m.dy("i").expect("sub 'i'");
    assert!(
        two_dy < 0.0 && i_dy > 0.0 && two_dy < i_dy,
        "sub/sup must straddle the baseline: sup '2' dy {two_dy:.2} (up) < sub 'i' dy {i_dy:.2} (down)"
    );
}

#[test]
fn math_display_paragraph_lays_out() {
    // m:oMathPara wrapping a fraction (display math). Word wraps it in a w:p, so
    // it lands in the paragraph path; the atom is flagged is_display and reserves
    // its full stacked height.
    let body = concat!(
        r#"<w:p><m:oMathPara><m:oMath>"#,
        r#"<m:f><m:num><m:r><m:t>a</m:t></m:r></m:num>"#,
        r#"<m:den><m:r><m:t>b</m:t></m:r></m:den></m:f>"#,
        r#"</m:oMath></m:oMathPara></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let count = items.iter().filter(|i| i.is_math).count();
    assert_eq!(count, 1, "display math must produce exactly one atom, got {count}");
    let m = items.iter().find(|i| i.is_math).unwrap().math.as_ref().unwrap();
    assert!(m.width > 0.0 && !m.rules.is_empty(), "display fraction must stack (width>0, has bar)");
}

#[test]
fn math_deferred_radical_degrades_without_vanishing() {
    // A radical must at worst degrade to a legible linear form
    // (√ + base) — never vanish, never panic.
    let body = concat!(
        r#"<w:p><w:r><w:t xml:space="preserve">z=</w:t></w:r>"#,
        r#"<m:oMath><m:rad><m:deg/><m:e><m:r><m:t>x</m:t></m:r></m:e></m:rad></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("radical atom (must not vanish)")
        .math.as_ref().unwrap();
    // The base 'x' survives, and a radical mark '√' is present (now real layout).
    let has_x = m.glyphs.iter().any(|g| g.text == "x");
    let has_root = m.glyphs.iter().any(|g| g.text == "\u{221A}");
    assert!(has_x, "radical base 'x' must render");
    assert!(has_root, "radical must render a '√' mark");
}

// ─── OMML math: n-ary, delimiters, radicals, matrices ────────────────────────
// Real 2D layout for n-ary operators, delimiters, radicals, matrices; plus
// display-equation centering. Each fixture is synthesized in-tree via math_docx.

#[test]
fn math_nary_sum_stacks_limits() {
    // ∑ with lower limit k=0 and upper limit n over an operand x. The ∑ operator
    // must be ENLARGED, the sup raised ABOVE and the sub lowered BELOW it (both
    // centered over the operator column), and the operand seated to the RIGHT.
    let body = concat!(
        r#"<w:p><m:oMath><m:nary>"#,
        r#"<m:naryPr><m:chr m:val="\u{2211}"/></m:naryPr>"#,
        r#"<m:sub><m:r><m:t>k=0</m:t></m:r></m:sub>"#,
        r#"<m:sup><m:r><m:t>n</m:t></m:r></m:sup>"#,
        r#"<m:e><m:r><m:t>x</m:t></m:r></m:e>"#,
        r#"</m:nary></m:oMath></w:p>"#,
    );
    // Substitute the literal ∑ char (concat! can't hold a \u escape in a string).
    let body = body.replace("\\u{2211}", "\u{2211}");
    let viewer = layout(math_docx(&body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("nary atom").math.as_ref().unwrap();

    // Enlarged operator: ∑ point size is above the base operand 'x'. (With the
    // math face, the real MATH display_operator_min_height enlarges to ~1.45em, less than the
    // old 1.8× proxy — still clearly enlarged.)
    let sigma_pt = m.font_pt("\u{2211}").expect("∑ operator glyph");
    let x_pt = m.font_pt("x").expect("operand 'x'");
    assert!(
        sigma_pt > x_pt * 1.2,
        "∑ must be enlarged (got {sigma_pt:.1}pt vs base {x_pt:.1}pt)"
    );

    // Upper limit 'n' raised above the operator; lower limit 'k' lowered below.
    let n_dy = m.dy("n").expect("upper limit 'n'");
    let k_dy = m.dy("k").expect("lower limit 'k'");
    assert!(n_dy < 0.0, "upper limit 'n' (dy {n_dy:.2}) must sit ABOVE the operator");
    assert!(k_dy > 0.0, "lower limit 'k' (dy {k_dy:.2}) must sit BELOW the operator");

    // Limits centered over the operator column; operand to the RIGHT of it.
    let sigma_dx = m.dx("\u{2211}").expect("∑ dx");
    let x_dx = m.dx("x").expect("operand dx");
    assert!(x_dx > sigma_dx, "operand 'x' (dx {x_dx:.2}) must sit right of ∑ (dx {sigma_dx:.2})");

    // The stacked atom grows ascent AND descent past the base glyph.
    assert!(m.ascent > 0.0 && m.descent > 0.0, "stacked n-ary must reserve asc+desc");
}

#[test]
fn math_delimiter_grows_to_content() {
    // Parens around a tall fraction: both brackets must appear and be GROWN
    // (point size above base) to straddle the content.
    let body = concat!(
        r#"<w:p><m:oMath><m:d>"#,
        r#"<m:e><m:f><m:num><m:r><m:t>a</m:t></m:r></m:num>"#,
        r#"<m:den><m:r><m:t>b</m:t></m:r></m:den></m:f></m:e>"#,
        r#"</m:d></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("delimiter atom").math.as_ref().unwrap();

    let lparen = m.glyphs.iter().find(|g| g.text == "(").expect("left paren '('");
    let rparen = m.glyphs.iter().find(|g| g.text == ")").expect("right paren ')'");
    // The fraction content 'a'/'b' sits BETWEEN the brackets.
    let a_dx = m.dx("a").expect("num 'a'");
    assert!(lparen.dx < a_dx && a_dx < rparen.dx, "content must sit between the brackets");
    // Brackets grown taller than the base run (proxy = enlarged point size).
    let base_pt = m.font_pt("a").expect("base 'a'");
    assert!(
        lparen.font_pt > base_pt && rparen.font_pt > base_pt,
        "brackets must be grown ({:.1}/{:.1}pt vs base {base_pt:.1}pt)",
        lparen.font_pt, rparen.font_pt
    );
}

#[test]
fn math_radical_has_vinculum() {
    // √ over a fraction: a '√' sign, plus a Rule (vinculum) that starts AFTER the
    // sign and spans the radicand width, sitting ABOVE the baseline (y negative).
    let body = concat!(
        r#"<w:p><m:oMath><m:rad><m:deg/>"#,
        r#"<m:e><m:f><m:num><m:r><m:t>a</m:t></m:r></m:num>"#,
        r#"<m:den><m:r><m:t>b</m:t></m:r></m:den></m:f></m:e>"#,
        r#"</m:rad></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("radical atom").math.as_ref().unwrap();

    let sign = m.glyphs.iter().find(|g| g.text == "\u{221A}").expect("√ sign");
    let base_pt = m.font_pt("a").expect("radicand 'a'");
    assert!(sign.font_pt > base_pt, "√ must be enlarged to the radicand height");
    // Vinculum: the fraction has its own bar (y ≈ -axis, small) plus the radical's
    // overbar high above; the highest (most negative y) Rule is the vinculum.
    let vinculum = m.rules.iter().min_by(|a, b| a.y.partial_cmp(&b.y).unwrap())
        .expect("at least one Rule");
    assert!(vinculum.y < 0.0, "vinculum must sit above the baseline (y {:.2})", vinculum.y);
    assert!(vinculum.x > sign.dx, "vinculum (x {:.2}) must start after the √ sign (dx {:.2})", vinculum.x, sign.dx);
    assert!(vinculum.w > 0.0, "vinculum must span the radicand width");
}

#[test]
fn math_matrix_grids_cells() {
    // 2×2 matrix: four cells on a non-overlapping grid (two distinct columns by
    // dx, two distinct rows by dy).
    let body = concat!(
        r#"<w:p><m:oMath><m:m>"#,
        r#"<m:mr><m:e><m:r><m:t>a</m:t></m:r></m:e><m:e><m:r><m:t>b</m:t></m:r></m:e></m:mr>"#,
        r#"<m:mr><m:e><m:r><m:t>c</m:t></m:r></m:e><m:e><m:r><m:t>d</m:t></m:r></m:e></m:mr>"#,
        r#"</m:m></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("matrix atom").math.as_ref().unwrap();

    let a = m.glyphs.iter().find(|g| g.text == "a").expect("cell a");
    let b = m.glyphs.iter().find(|g| g.text == "b").expect("cell b");
    let c = m.glyphs.iter().find(|g| g.text == "c").expect("cell c");
    let d = m.glyphs.iter().find(|g| g.text == "d").expect("cell d");
    // Columns: a,c on the left; b,d on the right.
    assert!(a.dx < b.dx && c.dx < d.dx, "columns must separate (a<b, c<d)");
    assert!((a.dx - c.dx).abs() < 2.0, "a,c share the left column");
    assert!((b.dx - d.dx).abs() < 2.0, "b,d share the right column");
    // Rows: a,b on top; c,d below.
    assert!(a.dy < c.dy && b.dy < d.dy, "rows must separate (top above bottom)");
    assert!((a.dy - b.dy).abs() < 2.0, "a,b share the top row");
    assert!((c.dy - d.dy).abs() < 2.0, "c,d share the bottom row");
    // No brackets on a bare matrix.
    assert!(!m.glyphs.iter().any(|g| g.text == "(" || g.text == ")"), "bare matrix has no delimiters");
}

#[test]
fn math_display_equation_centers_but_inline_stays_left() {
    // (a) A bare `<w:p><m:oMath>…` is a DISPLAY equation ⇒ its atom's segment
    //     x_start is CENTERED on the content width, not at the left margin.
    let display = concat!(
        r#"<w:p><m:oMath><m:sSup><m:e><m:r><m:t>x</m:t></m:r></m:e>"#,
        r#"<m:sup><m:r><m:t>2</m:t></m:r></m:sup></m:sSup></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(display));
    let items = item_snaps(&viewer);
    let it = items.iter().find(|i| i.is_math).expect("display math atom");
    let m = it.math.as_ref().unwrap();
    let expected = it.content_left + (it.content_right - it.content_left - m.width) / 2.0;
    assert!(
        (it.seg_x_start - expected).abs() < 0.5,
        "display eq must be centered: x_start {:.2} vs expected {:.2} (content {:.2}..{:.2}, w {:.2})",
        it.seg_x_start, expected, it.content_left, it.content_right, m.width
    );
    assert!(
        it.seg_x_start > it.content_left + 1.0,
        "display eq x_start ({:.2}) must not sit at the left margin ({:.2})",
        it.seg_x_start, it.content_left
    );

    // (b) The SAME math seated among real runs stays inline (left margin).
    let inline = concat!(
        r#"<w:p><w:r><w:t xml:space="preserve">y=</w:t></w:r>"#,
        r#"<m:oMath><m:sSup><m:e><m:r><m:t>x</m:t></m:r></m:e>"#,
        r#"<m:sup><m:r><m:t>2</m:t></m:r></m:sup></m:sSup></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(inline));
    let items = item_snaps(&viewer);
    let it = items.iter().find(|i| i.text == "y=").expect("'y=' run");
    assert!(
        (it.seg_x_start - it.content_left).abs() < 0.5,
        "inline math paragraph must stay left: x_start {:.2} vs content_left {:.2}",
        it.seg_x_start, it.content_left
    );
}

#[test]
fn math_binomial_display_stacks_and_centers() {
    // The authentic binomial (n over k): a delimiter (parens) wrapping an m:eqArr
    // of two rows [n; k], as a bare oMath owning its line. The eqArr now STACKS
    // the rows (real 2D), so the already-working delimiter grows the parens to the
    // full stacked height — the "finish the binomial" deliverable. Assert grown
    // parens, n above k, centered line, all composing with ZERO delimiter changes.
    let body = concat!(
        r#"<w:p><m:oMath><m:d><m:e><m:eqArr>"#,
        r#"<m:e><m:r><m:t>n</m:t></m:r></m:e>"#,
        r#"<m:e><m:r><m:t>k</m:t></m:r></m:e>"#,
        r#"</m:eqArr></m:e></m:d></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let it = items.iter().find(|i| i.is_math).expect("binomial atom");
    let m = it.math.as_ref().unwrap();

    // Grown parens present; n stacked ABOVE k.
    let lparen = m.glyphs.iter().find(|g| g.text == "(").expect("grown left paren");
    let rparen = m.glyphs.iter().find(|g| g.text == ")").expect("grown right paren");
    let base_pt = m.font_pt("n").expect("row 'n'");
    assert!(
        lparen.font_pt > base_pt && rparen.font_pt > base_pt,
        "parens must grow to the stacked height ({:.1}/{:.1}pt vs base {base_pt:.1}pt)",
        lparen.font_pt, rparen.font_pt
    );
    let n_dy = m.dy("n").expect("top row 'n'");
    let k_dy = m.dy("k").expect("bottom row 'k'");
    assert!(n_dy < k_dy, "binomial must stack: n (dy {n_dy:.2}) ABOVE k (dy {k_dy:.2})");
    // Content sits between the brackets.
    let n_dx = m.dx("n").expect("row 'n' dx");
    assert!(lparen.dx < n_dx && n_dx < rparen.dx, "rows must sit between the brackets");

    // Centered on its line.
    let expected = it.content_left + (it.content_right - it.content_left - m.width) / 2.0;
    assert!(
        (it.seg_x_start - expected).abs() < 0.5,
        "binomial display eq must be centered: x_start {:.2} vs expected {:.2}",
        it.seg_x_start, expected
    );
}

// ─── OMML: equation arrays, functions, accents, bars, group chars, limits,
//     pre-scripts, border boxes, phantoms. Each is a synthesized in-tree fixture.

#[test]
fn math_equation_array_stacks_rows() {
    // A 2-row eqArr [a; b] stacks a above b, both centered in the column.
    let body = concat!(
        r#"<w:p><m:oMath><m:eqArr>"#,
        r#"<m:e><m:r><m:t>a</m:t></m:r></m:e>"#,
        r#"<m:e><m:r><m:t>b</m:t></m:r></m:e>"#,
        r#"</m:eqArr></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("eqArr atom").math.as_ref().unwrap();

    let a_dy = m.dy("a").expect("row 'a'");
    let b_dy = m.dy("b").expect("row 'b'");
    assert!(a_dy < b_dy, "row 0 'a' (dy {a_dy:.2}) must sit ABOVE row 1 'b' (dy {b_dy:.2})");
    // Single-char rows of equal width ⇒ both centered at the same dx.
    let a_dx = m.dx("a").expect("a dx");
    let b_dx = m.dx("b").expect("b dx");
    assert!((a_dx - b_dx).abs() < 2.0, "rows must be centered in the column (a_dx≈b_dx)");
    // The stack reserves ascent AND descent past a single glyph.
    assert!(m.ascent > 0.0 && m.descent > 0.0, "stacked eqArr must reserve asc+desc");
}

#[test]
fn math_function_name_upright_with_argument() {
    // sin x: the function NAME renders upright (not italic), the argument to its
    // right with normal italic-variable classing.
    let body = concat!(
        r#"<w:p><m:oMath><m:func>"#,
        r#"<m:fName><m:r><m:t>sin</m:t></m:r></m:fName>"#,
        r#"<m:e><m:r><m:t>x</m:t></m:r></m:e>"#,
        r#"</m:func></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("func atom").math.as_ref().unwrap();

    // Name 'sin' upright (would otherwise be italic as three ascii letters) — it
    // stays plain ASCII (the upright/Normal plane is identity).
    let sin = m.glyphs.iter().find(|g| g.text == "sin").expect("name 'sin'");
    assert_eq!(sin.raw, "sin", "function name 'sin' must render upright (not a math-italic plane)");
    // Argument 'x' is a variable — with the math face its slant comes from the
    // math-italic code point (U+1D465), not a synthetic italic flag.
    let x = m.glyphs.iter().find(|g| g.text == "x").expect("argument 'x'");
    assert_eq!(
        x.raw, "\u{1D465}",
        "argument 'x' must render as the math-italic code point (got {:?})", x.raw
    );
    let name_dx = m.dx("sin").expect("name dx");
    let arg_dx = m.dx("x").expect("arg dx");
    assert!(arg_dx > name_dx, "argument 'x' (dx {arg_dx:.2}) must sit right of the name (dx {name_dx:.2})");
}

#[test]
fn math_accent_centered_above_without_widening() {
    // x̂ (hat over x): the accent overlays ABOVE the base and adds NO advance —
    // the atom width equals a bare 'x' atom's width. Compared against a reference
    // bare-x math atom in the same document.
    let body = concat!(
        r#"<w:p><m:oMath><m:acc><m:e><m:r><m:t>x</m:t></m:r></m:e></m:acc></m:oMath></w:p>"#,
        r#"<w:p><m:oMath><m:r><m:t>x</m:t></m:r></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let maths: Vec<_> = items.iter().filter(|i| i.is_math).collect();
    let acc = maths[0].math.as_ref().unwrap();
    let bare = maths[1].math.as_ref().unwrap();

    // Overlay: no widening.
    assert!((acc.width - bare.width).abs() < 0.01, "accent must not widen the base (overlay)");
    // Accent glyph above the baseline; ascent grows past the bare 'x'.
    let caret = acc.glyphs.iter().find(|g| g.text == "^").expect("accent '^' glyph");
    assert!(caret.dy < 0.0, "accent (dy {:.2}) must sit ABOVE the base", caret.dy);
    assert!(acc.ascent > bare.ascent, "accent must grow ascent past the bare base");
}

#[test]
fn math_bar_overline_spans_width() {
    // An overline over 'AB': a Rule above the baseline (y<0) spanning the base
    // width, and the atom ascent grows to reserve it.
    let body = concat!(
        r#"<w:p><m:oMath><m:bar><m:e><m:r><m:t>AB</m:t></m:r></m:e></m:bar></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("bar atom").math.as_ref().unwrap();

    let bar = m.rules.iter().min_by(|a, b| a.y.partial_cmp(&b.y).unwrap()).expect("overline Rule");
    assert!(bar.y < 0.0, "overline (y {:.2}) must sit above the baseline", bar.y);
    assert!((bar.x).abs() < 0.01, "overline must start at the base left edge");
    assert!(bar.w > 0.0 && (bar.w - m.width).abs() < 0.01, "overline must span the base width");
}

#[test]
fn math_group_char_stretched_above() {
    // An overbrace (U+23DE) over 'AB': the brace is grown (point size above base)
    // and placed ABOVE the group.
    let body = "<w:p><m:oMath><m:groupChr><m:groupChrPr><m:chr m:val=\"\u{23DE}\"/></m:groupChrPr>\
        <m:e><m:r><m:t>AB</m:t></m:r></m:e></m:groupChr></m:oMath></w:p>";
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("groupChr atom").math.as_ref().unwrap();

    let brace = m.glyphs.iter().find(|g| g.text == "\u{23DE}").expect("overbrace glyph");
    let base_pt = m.font_pt("AB").expect("base 'AB'");
    assert!(brace.font_pt > base_pt, "brace must be width-stretched (grown point size)");
    assert!(brace.dy < 0.0, "overbrace (dy {:.2}) must sit ABOVE the group", brace.dy);
}

#[test]
fn math_limits_stack_below_and_above() {
    // limLow: limit BELOW the base; limUpp: limit ABOVE. Both centered, smaller.
    let body = concat!(
        r#"<w:p><m:oMath><m:limLow>"#,
        r#"<m:e><m:r><m:t>L</m:t></m:r></m:e><m:lim><m:r><m:t>n</m:t></m:r></m:lim>"#,
        r#"</m:limLow></m:oMath></w:p>"#,
        r#"<w:p><m:oMath><m:limUpp>"#,
        r#"<m:e><m:r><m:t>L</m:t></m:r></m:e><m:lim><m:r><m:t>m</m:t></m:r></m:lim>"#,
        r#"</m:limUpp></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let maths: Vec<_> = items.iter().filter(|i| i.is_math).collect();
    let low = maths[0].math.as_ref().unwrap();
    let upp = maths[1].math.as_ref().unwrap();

    // Lower limit 'n' below the base 'L'; upper limit 'm' above.
    assert!(low.dy("n").expect("low limit 'n'") > 0.0, "limLow limit must sit BELOW the base");
    assert!(upp.dy("m").expect("upp limit 'm'") < 0.0, "limUpp limit must sit ABOVE the base");
    // Limits are down-scaled.
    let l_pt = low.font_pt("L").expect("base 'L'");
    assert!(low.font_pt("n").expect("limit 'n'") < l_pt, "the limit must be smaller than the base");
}

#[test]
fn math_pre_script_left_of_base() {
    // ₂³X: pre-sub '2' and pre-sup '3' seated to the LEFT of the base 'X'; sup
    // raised, sub lowered.
    let body = concat!(
        r#"<w:p><m:oMath><m:sPre>"#,
        r#"<m:sub><m:r><m:t>2</m:t></m:r></m:sub>"#,
        r#"<m:sup><m:r><m:t>3</m:t></m:r></m:sup>"#,
        r#"<m:e><m:r><m:t>X</m:t></m:r></m:e>"#,
        r#"</m:sPre></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("sPre atom").math.as_ref().unwrap();

    let base_dx = m.dx("X").expect("base 'X'");
    let sub_dx = m.dx("2").expect("pre-sub '2'");
    let sup_dx = m.dx("3").expect("pre-sup '3'");
    assert!(sub_dx < base_dx && sup_dx < base_dx, "pre-scripts must sit LEFT of the base");
    assert!(m.dy("3").expect("sup dy") < 0.0, "pre-superscript must be raised");
    assert!(m.dy("2").expect("sub dy") > 0.0, "pre-subscript must be lowered");
}

#[test]
fn math_border_box_draws_rectangle() {
    // A border box around 'x': two horizontal Rules (top/bottom) + two vertical
    // VRules (left/right), with the content nudged inside.
    let body = concat!(
        r#"<w:p><m:oMath><m:borderBox><m:e><m:r><m:t>x</m:t></m:r></m:e></m:borderBox></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("borderBox atom").math.as_ref().unwrap();

    assert!(m.rules.len() >= 2, "border box must draw >=2 horizontal edges (got {})", m.rules.len());
    assert!(m.vrules.len() >= 2, "border box must draw >=2 vertical edges (got {})", m.vrules.len());
    // Content sits INSIDE the box (shifted right by pad+thickness).
    let x_dx = m.dx("x").expect("content 'x'");
    assert!(x_dx > 0.0, "content must be inset inside the border (dx {x_dx:.2})");
    // A left edge at x≈0 and a right edge past the content.
    let left = m.vrules.iter().map(|v| v.x).fold(f64::INFINITY, f64::min);
    let right = m.vrules.iter().map(|v| v.x).fold(f64::NEG_INFINITY, f64::max);
    assert!(left < x_dx && x_dx < right, "content must sit between the vertical edges");
}

#[test]
fn math_phantom_reserves_but_hides() {
    // A phantom reserves the inner box dimensions but paints NOTHING: width > 0,
    // yet zero glyph/rule/vrule nodes.
    let body = concat!(
        r#"<w:p><m:oMath><m:phant><m:e><m:r><m:t>xyz</m:t></m:r></m:e></m:phant></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("phantom atom").math.as_ref().unwrap();

    assert!(m.width > 0.0, "phantom must RESERVE width (got {:.2})", m.width);
    assert!(m.glyphs.is_empty(), "phantom must paint NO glyphs (got {})", m.glyphs.len());
    assert!(m.rules.is_empty() && m.vrules.is_empty(), "phantom must paint no rules");
}

// ─── OMML math-face coverage: real MATH constants, size variants, alphabets, ──
//     and the no-math-face fallback. Each is a synthesized in-tree fixture.

/// Read the loaded XITS Math `MATH` axis_height, in px at `font_pt`.
fn xits_axis_height_px(font_pt: f64) -> f64 {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
    let bytes = std::fs::read(format!("{dir}/XITSMath-Regular.otf")).expect("XITS test font");
    let face = ttf_parser::Face::parse(&bytes, 0).expect("parse XITS");
    let upem = face.units_per_em() as f64;
    let axis = face.tables().math.unwrap().constants.unwrap().axis_height().value as f64;
    axis / upem * crate::engine::units::pt_to_px(font_pt)
}

#[test]
fn math_fraction_bar_sits_at_real_math_axis_height() {
    // With the real math face the fraction rule sits at the MATH-table
    // axis_height, which is DISTINCT from the old 0.26em proxy.
    let body = concat!(
        r#"<w:p><m:oMath><m:f><m:num><m:r><m:t>a</m:t></m:r></m:num>"#,
        r#"<m:den><m:r><m:t>b</m:t></m:r></m:den></m:f></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("fraction atom").math.as_ref().unwrap();
    assert!(!m.rules.is_empty(), "fraction must have a bar");
    let rule_y = m.rules[0].y; // negative = above baseline

    let font_pt = 11.0; // math_docx sets sz=22 half-points
    let real_axis = xits_axis_height_px(font_pt);
    let proxy_axis = 0.26 * crate::engine::units::pt_to_px(font_pt);
    assert!(
        (rule_y + real_axis).abs() < 0.05,
        "bar y ({rule_y:.3}) must sit at the real MATH axis_height (-{real_axis:.3})"
    );
    assert!(
        (real_axis - proxy_axis).abs() > 0.05,
        "the real axis ({real_axis:.3}) must differ from the 0.26em proxy ({proxy_axis:.3}) — \
         proving MATH constants drive the geometry, not the em-fraction"
    );
}

/// The first math atom's outline (size-variant) glyph nodes, as (source-char)
/// strings — walks the pub(crate) MathNode list directly.
fn outline_chars(viewer: &Viewer) -> Vec<String> {
    use crate::wml::math::MathNode;
    let pages = viewer.pages.borrow();
    for page in pages.iter() {
        for el in &page.elements {
            if let PageElement::Line(l) = el {
                for seg in &l.segments {
                    for it in &seg.items {
                        if let Some(m) = &it.math {
                            let mut out = Vec::new();
                            for n in &m.nodes {
                                if let MathNode::GlyphOutline { ch, segs, .. } = n {
                                    assert!(!segs.is_empty(), "outline glyph must carry path segs");
                                    out.push(ch.clone());
                                }
                            }
                            if !out.is_empty() {
                                return out;
                            }
                        }
                    }
                }
            }
        }
    }
    Vec::new()
}

#[test]
fn math_delimiter_snaps_to_a_math_size_variant() {
    // Parens around a tall fraction: the brackets must be painted as MATH size-
    // VARIANT glyph outlines (not a continuous point-scale of the base glyph), and
    // the bracket box must be at least as tall as the content.
    let body = concat!(
        r#"<w:p><m:oMath><m:d>"#,
        r#"<m:e><m:f><m:num><m:r><m:t>a</m:t></m:r></m:num>"#,
        r#"<m:den><m:r><m:t>b</m:t></m:r></m:den></m:f></m:e>"#,
        r#"</m:d></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let outlines = outline_chars(&viewer);
    assert!(
        outlines.iter().any(|c| c == "(") && outlines.iter().any(|c| c == ")"),
        "stretchy brackets must be painted via MATH size-variant outlines (got {outlines:?})"
    );

    // Bracket box height ≥ content height (a delimiter never shrinks the content).
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).unwrap().math.as_ref().unwrap();
    let lparen = m.glyphs.iter().find(|g| g.text == "(").expect("left paren snap");
    // The bracket's grown point size must exceed the base 'a'.
    let base_pt = m.font_pt("a").expect("base 'a'");
    assert!(lparen.font_pt > base_pt, "variant bracket must be grown past the base");
}

#[test]
fn math_alphabet_maps_planes_and_letterlike_holes() {
    // Pure map (m:sty/m:scr planes aren't parsed by the fork yet — documented
    // follow-on — so this exercises the alphabet map directly). Every mapped code
    // point must exist in the shipped XITS face (the tofu guard's premise).
    use crate::wml::math::{math_alpha_pure, MathPlane};
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
    let bytes = std::fs::read(format!("{dir}/XITSMath-Regular.otf")).expect("XITS test font");
    let face = ttf_parser::Face::parse(&bytes, 0).expect("parse XITS");

    // Double-struck 'R' → ℝ (a Letterlike-Symbols hole, NOT the 1D4xx arithmetic).
    assert_eq!(math_alpha_pure('R', MathPlane::DoubleStruck), '\u{211D}');
    // Script 'F' → ℱ; fraktur 'C' → ℭ; italic 'h' → ℎ (Planck); italic 'x' → 𝑥.
    assert_eq!(math_alpha_pure('F', MathPlane::Script), '\u{2131}');
    assert_eq!(math_alpha_pure('C', MathPlane::Fraktur), '\u{212D}');
    assert_eq!(math_alpha_pure('h', MathPlane::Italic), '\u{210E}');
    assert_eq!(math_alpha_pure('x', MathPlane::Italic), '\u{1D465}');
    // Non-hole arithmetic base offset (bold 'A' → 𝐀, double-struck digit 0 → 𝟘).
    assert_eq!(math_alpha_pure('A', MathPlane::Bold), '\u{1D400}');
    assert_eq!(math_alpha_pure('0', MathPlane::DoubleStruck), '\u{1D7D8}');
    // Digits stay upright in a serif/italic plane.
    assert_eq!(math_alpha_pure('5', MathPlane::Italic), '5');

    for (ch, plane) in [
        ('R', MathPlane::DoubleStruck), ('F', MathPlane::Script), ('C', MathPlane::Fraktur),
        ('x', MathPlane::Italic), ('A', MathPlane::Bold), ('0', MathPlane::DoubleStruck),
    ] {
        let mapped = math_alpha_pure(ch, plane);
        assert!(
            face.glyph_index(mapped).is_some(),
            "XITS must cover the mapped glyph U+{:04X} for {ch:?}", mapped as u32
        );
    }
}

#[test]
fn math_default_variable_renders_as_math_italic_codepoint() {
    // The OMML default single-letter variable renders via the math-italic PLANE
    // (slant from the code point, no synthetic italic flag).
    let body = concat!(
        r#"<w:p><m:oMath><m:r><m:t>x</m:t></m:r></m:oMath></w:p>"#,
    );
    let viewer = layout(math_docx(body));
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("math atom").math.as_ref().unwrap();
    let x = m.glyphs.iter().find(|g| g.text == "x").expect("variable 'x'");
    assert_eq!(x.raw, "\u{1D465}", "default variable must render math-italic (U+1D465)");
    assert!(!x.italic, "slant comes from the code point, not a synthetic italic flag");
}

/// Build a viewer with ONLY the body font (Arimo) loaded — NO math face — to
/// exercise the body-font proxy fallback.
fn layout_no_math_face(docx: Vec<u8>) -> Viewer {
    let mut viewer = Viewer::new().expect("Viewer::new");
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
    for (file, weight, italic) in [
        ("Arimo-Regular.ttf", 400u16, false),
        ("Arimo-Bold.ttf", 700, false),
        ("Arimo-Italic.ttf", 400, true),
        ("Arimo-BoldItalic.ttf", 700, true),
    ] {
        let bytes = std::fs::read(format!("{dir}/{file}")).expect("arimo test font");
        viewer.load_font("Arimo", bytes, weight, italic).expect("load arimo");
    }
    viewer.load_docx(docx).expect("load_docx");
    viewer.run_layout_native().expect("run_layout_native");
    viewer
}

#[test]
fn math_without_math_face_falls_back_to_proxies() {
    // RED-GREEN fallback: with NO math face loaded, math still lays out (non-zero
    // width, no panic), via the body font — variables are NOT alphabet-mapped
    // (they keep the ASCII code point + a synthetic italic flag) and stretchy
    // glyphs are the point-scale proxy (Glyph, never GlyphOutline).
    let body = concat!(
        r#"<w:p><m:oMath><m:d>"#,
        r#"<m:e><m:f><m:num><m:r><m:t>x</m:t></m:r></m:num>"#,
        r#"<m:den><m:r><m:t>y</m:t></m:r></m:den></m:f></m:e>"#,
        r#"</m:d></m:oMath></w:p>"#,
    );
    let viewer = layout_no_math_face(math_docx(body));
    // No panic reaching here is half the test. Now assert the fallback shape.
    assert!(outline_chars(&viewer).is_empty(), "no math face ⇒ no size-variant outlines");
    let items = item_snaps(&viewer);
    let m = items.iter().find(|i| i.is_math).expect("math atom must still exist").math.as_ref().unwrap();
    assert!(m.width > 0.0, "math must still reserve width via the body font");
    let x = m.glyphs.iter().find(|g| g.text == "x").expect("variable 'x'");
    assert_eq!(x.raw, "x", "without a math face the ASCII code point is kept (no alphabet map)");
    assert!(x.italic, "without a math face the synthetic italic flag drives the slant");
    // Brackets present as ordinary (point-scaled) glyphs.
    assert!(m.glyphs.iter().any(|g| g.text == "("), "proxy brackets still render as glyphs");
}

#[test]
fn math_fixtures_geometry_pinned() {
    // Additive geometry pins for the new math fixtures (the M{w,a,d} token is
    // math-guarded, so these hashes are self-contained and cannot perturb the
    // three math-free oracle pins). Justified NEW baselines.
    let cases: &[(&str, Vec<u8>, u64)] = &[
        ("math-fraction", math_docx(concat!(
            r#"<w:p><w:pPr><w:spacing w:after="0"/></w:pPr>"#,
            r#"<w:r><w:t xml:space="preserve">x=</w:t></w:r>"#,
            r#"<m:oMath><m:f><m:num><m:r><m:t>a</m:t></m:r></m:num>"#,
            r#"<m:den><m:r><m:t>b</m:t></m:r></m:den></m:f></m:oMath>"#,
            r#"<w:r><w:t xml:space="preserve">+1</w:t></w:r></w:p>"#,
        )), 0x0619b1b9379c4aee),
        // Bare `<w:p><m:oMath>…` = a DISPLAY equation ⇒ now CENTERED per Office's
        // centerGroup default (originally left-aligned). The M{w,a,d} token is
        // unchanged; only the segment s{x_start} shifts to the centered position
        // (verified by math_display_equation_centers_but_inline_stays_left).
        ("math-superscript", math_docx(concat!(
            r#"<w:p><m:oMath><m:sSup><m:e><m:r><m:t>x</m:t></m:r></m:e>"#,
            r#"<m:sup><m:r><m:t>2</m:t></m:r></m:sup></m:sSup></m:oMath></w:p>"#,
        )), 0x24557c091f2118ba),
        // N-ary/delimiter/radical/matrix pins (self-contained; math-guarded dump tokens).
        ("nary-sum", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">s=</w:t></w:r><m:oMath><m:nary>"#,
            "<m:naryPr><m:chr m:val=\"\u{2211}\"/></m:naryPr>",
            r#"<m:sub><m:r><m:t>k=0</m:t></m:r></m:sub>"#,
            r#"<m:sup><m:r><m:t>n</m:t></m:r></m:sup>"#,
            r#"<m:e><m:r><m:t>x</m:t></m:r></m:e>"#,
            r#"</m:nary></m:oMath></w:p>"#,
        )), 0x888ef183f5bbb985),
        ("delimiter-grow", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">d=</w:t></w:r><m:oMath><m:d>"#,
            r#"<m:e><m:f><m:num><m:r><m:t>a</m:t></m:r></m:num>"#,
            r#"<m:den><m:r><m:t>b</m:t></m:r></m:den></m:f></m:e>"#,
            r#"</m:d></m:oMath></w:p>"#,
        )), 0x78acb019523bbbc8),
        ("radical-sqrt", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">r=</w:t></w:r><m:oMath><m:rad><m:deg/>"#,
            r#"<m:e><m:f><m:num><m:r><m:t>a</m:t></m:r></m:num>"#,
            r#"<m:den><m:r><m:t>b</m:t></m:r></m:den></m:f></m:e>"#,
            r#"</m:rad></m:oMath></w:p>"#,
        )), 0x5ea97cf183ee716d),
        ("matrix-2x2", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">m=</w:t></w:r><m:oMath><m:m>"#,
            r#"<m:mr><m:e><m:r><m:t>a</m:t></m:r></m:e><m:e><m:r><m:t>b</m:t></m:r></m:e></m:mr>"#,
            r#"<m:mr><m:e><m:r><m:t>c</m:t></m:r></m:e><m:e><m:r><m:t>d</m:t></m:r></m:e></m:mr>"#,
            r#"</m:m></m:oMath></w:p>"#,
        )), 0x6705de3d83f26de0),
        // Display fixture (oMathPara) — now centered (guards display centering).
        ("display-centered", math_docx(concat!(
            r#"<w:p><m:oMathPara><m:oMath>"#,
            r#"<m:f><m:num><m:r><m:t>a</m:t></m:r></m:num>"#,
            r#"<m:den><m:r><m:t>b</m:t></m:r></m:den></m:f>"#,
            r#"</m:oMath></m:oMathPara></w:p>"#,
        )), 0x6835a7cc357d7ffe),
        // Binomial — REWRITTEN from an earlier noBar-fraction stand-in to the
        // authentic m:d(m:eqArr(n; k)). The eqArr now STACKS the two rows (real
        // 2D) and the already-working delimiter grows the parens to the full
        // stacked height, so this pin MOVES from 0x81358b70e5f831c1 (justified:
        // the eqArr path is now exercised and the binomial finally stacks n/k in
        // grown parens). See math_binomial_display_stacks_and_centers.
        ("binomial", math_docx(concat!(
            r#"<w:p><m:oMath><m:d><m:e><m:eqArr>"#,
            r#"<m:e><m:r><m:t>n</m:t></m:r></m:e>"#,
            r#"<m:e><m:r><m:t>k</m:t></m:r></m:e>"#,
            r#"</m:eqArr></m:e></m:d></m:oMath></w:p>"#,
        )), 0x2cc64febc114c576),
        // ─── eqArr/func/acc/bar/limits/etc. pins (self-contained; math-guarded) ───
        ("eqarr-stack", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">e=</w:t></w:r><m:oMath><m:eqArr>"#,
            r#"<m:e><m:r><m:t>a</m:t></m:r></m:e>"#,
            r#"<m:e><m:r><m:t>b</m:t></m:r></m:e>"#,
            r#"</m:eqArr></m:oMath></w:p>"#,
        )), 0xd5ea0bbd2b223879),
        ("function-upright", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">f=</w:t></w:r><m:oMath><m:func>"#,
            r#"<m:fName><m:r><m:t>sin</m:t></m:r></m:fName>"#,
            r#"<m:e><m:r><m:t>x</m:t></m:r></m:e>"#,
            r#"</m:func></m:oMath></w:p>"#,
        )), 0xab85ead9320a27c8),
        ("accent-hat", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">a=</w:t></w:r>"#,
            r#"<m:oMath><m:acc><m:e><m:r><m:t>x</m:t></m:r></m:e></m:acc></m:oMath></w:p>"#,
        )), 0x34b28a54e537b80b),
        ("bar-overline", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">b=</w:t></w:r>"#,
            r#"<m:oMath><m:bar><m:e><m:r><m:t>AB</m:t></m:r></m:e></m:bar></m:oMath></w:p>"#,
        )), 0xa554006e526ef900),
        ("groupchr-brace", math_docx(
            &("<w:p><w:r><w:t xml:space=\"preserve\">g=</w:t></w:r>\
              <m:oMath><m:groupChr><m:groupChrPr><m:chr m:val=\"\u{23DE}\"/></m:groupChrPr>\
              <m:e><m:r><m:t>AB</m:t></m:r></m:e></m:groupChr></m:oMath></w:p>".to_string()),
        ), 0x4e4abe7dd18be0e9),
        ("limlow", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">l=</w:t></w:r><m:oMath><m:limLow>"#,
            r#"<m:e><m:r><m:t>L</m:t></m:r></m:e><m:lim><m:r><m:t>n</m:t></m:r></m:lim>"#,
            r#"</m:limLow></m:oMath></w:p>"#,
        )), 0x0a58681b76f29be7),
        ("limupp", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">u=</w:t></w:r><m:oMath><m:limUpp>"#,
            r#"<m:e><m:r><m:t>L</m:t></m:r></m:e><m:lim><m:r><m:t>m</m:t></m:r></m:lim>"#,
            r#"</m:limUpp></m:oMath></w:p>"#,
        )), 0x161926aee0b53ce0),
        ("prescript", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">p=</w:t></w:r><m:oMath><m:sPre>"#,
            r#"<m:sub><m:r><m:t>2</m:t></m:r></m:sub>"#,
            r#"<m:sup><m:r><m:t>3</m:t></m:r></m:sup>"#,
            r#"<m:e><m:r><m:t>X</m:t></m:r></m:e>"#,
            r#"</m:sPre></m:oMath></w:p>"#,
        )), 0x2641b6f08c710787),
        ("borderbox", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">x=</w:t></w:r>"#,
            r#"<m:oMath><m:borderBox><m:e><m:r><m:t>x</m:t></m:r></m:e></m:borderBox></m:oMath></w:p>"#,
        )), 0x4b3f033c9e03e647),
        ("phantom", math_docx(concat!(
            r#"<w:p><w:r><w:t xml:space="preserve">h=</w:t></w:r>"#,
            r#"<m:oMath><m:phant><m:e><m:r><m:t>xyz</m:t></m:r></m:e></m:phant></m:oMath></w:p>"#,
        )), 0x9d6569511484e081),
    ];
    let dump_all = std::env::var("ORACLE_DUMP").is_ok();
    let mut drift = Vec::new();
    for (name, docx, expect) in cases {
        let viewer = layout(docx.clone());
        let dump = geometry_dump(&viewer);
        let hash = fnv1a(dump.as_bytes());
        if dump_all {
            println!("---- {name} geometry dump ----\n{dump}\n{name} hash = {hash:#018x}");
        }
        // The M-token must actually appear (proves the atom reached a paged line).
        assert!(dump.contains(" M{"), "{name}: dump has no math token — atom not seated");
        // These 18 pins were the ones WITHOUT `assert_oracle`'s collateral
        // gate. No math fixture carries a cursor-independent float, so the whole
        // LayoutStats tuple must stay zero — counters are the only place "the
        // engine did something different and landed in the same place" can show up.
        assert_eq!(
            viewer.layout_stats(), LayoutStats::default(),
            "{name}: layout counters moved on a float-free math fixture (collateral) — \
             diagnose, do NOT re-baseline"
        );
        // The ITEM stream also catches "the atom renders the wrong glyphs
        // at the right advance", which the M{w,a,d} token cannot express.
        assert_content_streams(name, &viewer);
        if hash != *expect {
            drift.push(format!("  {name}: got {hash:#018x}, baseline {expect:#018x}"));
        }
    }
    // This used to print a directly-pasteable replacement pin list, which
    // is a one-keystroke path to blessing a live bug — the exact opposite of
    // `assert_oracle`'s "diagnose, do NOT re-baseline". The new value is now
    // printed only behind an explicit opt-in.
    let reveal = if std::env::var("ORACLE_REBASELINE").is_ok() {
        drift.iter().map(|d| d.as_str()).collect::<Vec<_>>().join("\n")
    } else {
        drift.iter()
            .map(|d| d.split(':').next().unwrap_or(d).trim().to_string())
            .collect::<Vec<_>>().join("\n")
    };
    assert!(
        drift.is_empty(),
        "math LINE-geometry pins drifted — diagnose, do NOT re-baseline. Run with ORACLE_DUMP=1 \
         to diff the dump; the new hashes are printed only under ORACLE_REBASELINE=1, and only \
         once you have established the change is intentional. Drifted:\n{reveal}"
    );
}

/// Arbitrary-file probe — the DURABLE corpus/robustness harness (kept in-tree
/// so investigations reuse one tool instead of rebuilding a throwaway each time).
/// `#[ignore]`d: never part of the suite. Run one file:
///
///   PROBE_DOCX=/abs/path.docx cargo test -p parasma-wasm \
///       --features sml,pml,emf probe_open_arbitrary_docx -- --ignored --nocapture
///
/// Prints machine-parseable one-line markers a sweep driver can grep:
///   PROBE_PARSE_ERR <err>   — load_docx rejected the file
///   PROBE_LAYOUT_ERR <err>  — layout failed after a successful parse
///   PROBE_OK pages=N ms=T   — opened + laid out
/// A PANIC anywhere surfaces as the normal test failure (non-zero exit), which
/// the driver records as its own category. Fonts = the standard test set
/// (Arimo + XITS) — this probe screens "opens and renders at all", not fidelity.
#[test]
#[ignore]
fn probe_open_arbitrary_docx() {
    let path = std::env::var("PROBE_DOCX").expect("set PROBE_DOCX=/abs/path.docx");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    // Parse errors are probed via the tolerant loader DIRECTLY: load_docx's own
    // error arm formats a JsValue, whose wasm-bindgen shim PANICS off-wasm32 —
    // natively every parse failure would otherwise be an unreadable abort. The
    // double parse for OK files is fine (this is a probe, not a benchmark).
    let mut viewer = Viewer::new().expect("Viewer::new");
    load_test_fonts(&mut viewer);
    let t0 = std::time::Instant::now();
    // Same order as load_docx: the bomb guard runs before any parse.
    if let Err(e) = crate::package_limits::check(&bytes) {
        println!("PROBE_REFUSED {e} ms={}", t0.elapsed().as_millis());
        return;
    }
    let guard_ms = t0.elapsed().as_millis();
    if let Err(e) = crate::repair::load_docx_tolerant(bytes.clone()) {
        println!("PROBE_PARSE_ERR {e}");
        return;
    }
    // Stage gate for overflow/crash triage: stop after the parse succeeded.
    if std::env::var("PROBE_PARSE_ONLY").is_ok() {
        println!("PROBE_PARSED");
        return;
    }
    if viewer.load_docx(bytes).is_err() {
        println!("PROBE_PARSE_ERR load_docx failed after tolerant parse succeeded");
        return;
    }
    match viewer.run_layout_native() {
        Err(e) => println!("PROBE_LAYOUT_ERR {e:?}"),
        Ok(_) => {
            let pages = viewer.page_count();
            println!("PROBE_OK pages={pages} ms={} guard_ms={guard_ms}", t0.elapsed().as_millis());
        }
    }
}

/// MIN-ONE-PAGE: an EMPTY body (zero paragraphs/tables — corpus Bug60341.docx
/// is a bare sectPr) must still yield one blank page, as Word renders it.
#[test]
fn empty_body_yields_one_page() {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("[Content_Types].xml", opts).unwrap();
        w.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#).unwrap();
        w.start_file("_rels/.rels", opts).unwrap();
        w.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#).unwrap();
        w.start_file("word/document.xml", opts).unwrap();
        w.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:sectPr/></w:body></w:document>"#).unwrap();
        w.finish().unwrap();
    }
    let viewer = layout(buf.into_inner());
    assert_eq!(viewer.page_count(), 1, "an empty body renders ONE blank page (Word parity)");
}


// ─── coverage: a fully-blocked float band reached by the PACKER ──────────────
//
// `pack_lines_around_floats` JUMPS a fully-blocked band instead of stepping it
// one pitch at a time. When that was introduced, a check found the entire suite
// entered `next_open_band` ZERO times from a document — the only three entries
// came from the unit test that ships with the fix. Re-measured on this tree
// before adding this fixture: 3 entries, all from
// `wml::float::packer_conservation_tests::a_blocked_band_is_jumped_not_stepped`.
// "All oracle pins green" attested only that an unexercised path was unexercised.
//
// Real documents DO produce fully-blocked bands, but they all reach them through
// `clear_below_floats`, which descends the page cursor BEFORE packing starts. To
// reach the packer's jump the block must open MID-PARAGRAPH: a full-width float
// whose top sits below the paragraph's first line, so the first lines pack
// normally and a later one lands in a band with no free interval at all.
//
// `positionV relativeFrom="paragraph"` gives exactly that, and keeps the float
// cursor-DEPENDENT, so the per-page fixpoint stays inert (`pages_relaid == 0`)
// and the fixture is a clean pin rather than a float-machinery stress test.

fn blocked_band_midparagraph_docx() -> Vec<u8> {
    const BOX_W_EMU: i64 = 5_943_600; // 6.5in == the WHOLE content column → zero free interval
    const BOX_H_EMU: i64 = 1_371_600; // 1.5in ≈ 8 line pitches of blocked band
    const BOX_OFF_EMU: i64 = 914_400; // 1in below the paragraph top → lines 0..4 pack freely

    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#
    );

    let textbox = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="paragraph"><wp:posOffset>{BOX_OFF_EMU}</wp:posOffset></wp:positionV>
  <wp:extent cx="{BOX_W_EMU}" cy="{BOX_H_EMU}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapSquare wrapText="bothSides"/>
  <wp:docPr id="1" name="TextBox 1"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp><wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_W_EMU}" cy="{BOX_H_EMU}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">Blocking band.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/></wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    );

    let mut body = String::new();
    // ONE long paragraph: the float must block a band INSIDE it, not between it
    // and a neighbour (that is the clear_below_floats path the suite already had).
    body.push_str(&format!("<w:p>{textbox}{}</w:p>", r("", &filler(0, 14))));
    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}</w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

#[test]
fn blocked_band_midparagraph_is_jumped_by_the_packer() {
    let viewer = layout(blocked_band_midparagraph_docx());
    let stats = viewer.layout_stats();
    assert_eq!(stats, LayoutStats::default(),
        "paragraph-relative float ⇒ cursor-dependent ⇒ the fixpoint must stay inert, got {stats:?}");
    let pages = viewer.pages.borrow();
    assert_eq!(pages.len(), 1, "the fixture must fit one page");
    let page = &pages[0];
    assert_eq!(page.positioned_text_boxes.len(), 1, "the blocking box is drawn once");
    let tb = &page.positioned_text_boxes[0];
    let (band_top, band_bottom) = (tb.y, tb.y + tb.height_px);

    // (1) The band really is FULLY blocked — the box spans the whole content
    // column, so `free_intervals_for_line` returns nothing inside it. Without
    // this the fixture would be an ordinary side-wrap and prove nothing.
    assert!(tb.width_px >= 620.0, "the box must span the content column, got {:.2}", tb.width_px);

    // (2) The blocked run collapsed into a SINGLE item-less line carrying the
    // whole walk — "jump, don't step". Stepping would emit one empty line
    // per pitch instead; `walk_height` is what the jump feeds into `line_height`.
    let empties: Vec<&PagedLine> = page.elements.iter().filter_map(|el| match el {
        PageElement::Line(l) if l.segments.iter().all(|s| s.items.is_empty()) => Some(l),
        _ => None,
    }).collect();
    assert_eq!(empties.len(), 1,
        "the blocked run must collapse to ONE line (stepping would emit one per pitch), got {}",
        empties.len());
    let walk = empties[0];
    assert!(walk.line_height > band_bottom - band_top,
        "the collapsed line must carry the WHOLE walk ({:.2}) past the band ({:.2}) — a \
         one-pitch height means the packer stepped instead of jumping",
        walk.line_height, band_bottom - band_top);

    // (3) …and no line with content sits inside the band (§20.4.2.17).
    for el in &page.elements {
        if let PageElement::Line(l) = el {
            if l.segments.iter().all(|s| s.items.is_empty()) { continue; }
            let top = l.baseline_y - l.line_height;
            assert!(top >= band_bottom - 0.5 || l.baseline_y <= band_top + 0.5,
                "a line with text (baseline {:.2}, lh {:.2}) overlaps the fully-blocked band \
                 [{band_top:.2}..{band_bottom:.2}]", l.baseline_y, l.line_height);
        }
    }
}

#[test]
fn oracle_blocked_band_snapshot() {
    // NEW pin. Recorded from the first green run with the invariants above
    // asserted independently — no pre-existing constant is touched.
    //
    // Re-baselined 2026-07-27, same single cause as oracle-objects: the
    // float packer adopted the legacy trailing-blank reserve. 4 lines
    // re-partitioned (n16→n15, n18→n17, n13→n14, n10→n11; 57 items in, 57 out);
    // every baseline, line height, segment x and the blocked-band walk height
    // are byte-identical, as are the framing-stripped ITEM/GLYPH streams.
    //
    // Re-baselined 2026-07-28, ADDITIVE ONLY and proven so: the LINE
    // stream gained positioned-textbox geometry, and stripping exactly the two
    // added lines (`B x96.00 y192.00 w624.00 h144.00` + its interior `BL`)
    // reproduces the prior hash 0x6bc955d63b747506 byte-for-byte. This was the
    // only LINE-pinned fixture carrying a positioned textbox.
    assert_oracle("oracle-blocked-band", blocked_band_midparagraph_docx(), 1, 0x769303d4dc625162);
}

// ─── fixture debt: the "attributes" oracle ──────────────────────────────────
//
// A stronger net over a corpus that contains none of the things it now sees is
// still a net over nothing. Census of every WML layout in this suite before this
// fixture existed — 269 documents, 17,494 lines, 200,193 painted items:
//
//   hyperlink targets 0 · all-caps 0 · small-caps 0 · vanish 0 · outline 0 ·
//   shadow 0 · emboss 0 · imprint 0 · highlight 0 · run-shading 0 ·
//   strikethrough 0 · bordered lines 0 · shaded lines 0 · char-spacing 0 ·
//   baseline-shift 0 · endnote refs 0 · RTL items 0 (base_rtl fired ONCE in the
//   whole suite, in a tab test) · ~1.1 font families per document
//
// Those are the counts AT THAT MOMENT, kept as the record of why this fixture
// exists — do not read them as current. The RTL column in particular has moved:
// a later change added `rtl_paragraph_around_a_float_packs_in_logical_order`, a base-RTL
// paragraph of 18 Hebrew words wrapping an anchored float, because the bidi
// column being 0 is exactly what let the float-aware packer re-break a VISUAL
// item sequence undetected.
//
// The three PINNED oracles were the emptiest of all: one font family, and a zero
// in every single one of those columns. This fixture puts one of each on the
// page so the ITEM stream has something to be blind to.
//
// Only Arimo and XITS Math are loaded by `load_test_fonts`, so "Georgia" here is
// a deliberately UNRESOLVED family: it exercises the estimated-width fallback
// (`shaped: None`) alongside two real faces, and the family string is pinned
// either way — which is the axis that had no coverage.

fn oracle_attributes_docx() -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#
    );

    let mut body = String::new();

    // (1) Bordered + shaded paragraph, with a bar tab and a dotted leader tab.
    body.push_str(&format!(
        r#"<w:p><w:pPr>
  <w:pBdr>
    <w:top w:val="single" w:sz="12" w:space="4" w:color="1F4E79"/>
    <w:bottom w:val="single" w:sz="8" w:space="2" w:color="C00000"/>
    <w:left w:val="single" w:sz="6" w:space="3" w:color="00B050"/>
    <w:right w:val="single" w:sz="24" w:space="1" w:color="7030A0"/>
  </w:pBdr>
  <w:shd w:val="clear" w:color="auto" w:fill="FFF2CC"/>
  <w:tabs><w:tab w:val="bar" w:pos="2880"/><w:tab w:val="right" w:pos="8640" w:leader="dot"/></w:tabs>
</w:pPr>{}<w:r><w:tab/></w:r>{}</w:p>"#,
        r("", "Bordered and shaded, with a bar rule."),
        r("", "right-aligned after a dotted leader"),
    ));

    // (2) Every run flag the census scored zero on, one run each so a flipped
    // bit shows up as exactly one changed record in the golden.
    body.push_str(&format!(
        "<w:p>{}{}{}{}{}{}{}{}{}{}{}{}</w:p>",
        r(r#"<w:u w:val="wave"/>"#, "wavy "),
        r(r#"<w:strike/>"#, "struck "),
        r(r#"<w:dstrike/>"#, "double-struck "),
        r(r#"<w:caps/>"#, "all caps "),
        r(r#"<w:smallCaps/>"#, "small caps "),
        r(r#"<w:outline/>"#, "outline "),
        r(r#"<w:shadow/>"#, "shadow "),
        r(r#"<w:emboss/>"#, "emboss "),
        r(r#"<w:imprint/>"#, "imprint "),
        r(r#"<w:highlight w:val="yellow"/>"#, "highlit "),
        r(r#"<w:shd w:val="clear" w:fill="D9E2F3"/>"#, "run-shaded "),
        r(r#"<w:vanish/>"#, "hidden-should-not-paint"),
    ));

    // (3) Character spacing, baseline shift, super/subscript, colour, and the
    // two extra font families.
    body.push_str(&format!(
        "<w:p>{}{}{}{}{}{}</w:p>",
        r(r#"<w:spacing w:val="40"/>"#, "tracked-out "),
        r(r#"<w:position w:val="8"/>"#, "raised "),
        r(r#"<w:vertAlign w:val="superscript"/>"#, "super"),
        r(r#"<w:vertAlign w:val="subscript"/>"#, "sub "),
        r(r#"<w:color w:val="C00000"/><w:rFonts w:ascii="Georgia" w:hAnsi="Georgia"/>"#,
          "Georgia in red "),
        r(r#"<w:rFonts w:ascii="XITS Math" w:hAnsi="XITS Math"/>"#, "XITS Math face"),
    ));

    // (4) Hyperlinks — external (r:id) and internal (w:anchor) — plus a bookmark
    // for the internal one to land on. Zero occurrences suite-wide before this.
    body.push_str(&format!(
        r#"<w:p><w:bookmarkStart w:id="7" w:name="target"/>{}<w:bookmarkEnd w:id="7"/>
  <w:hyperlink r:id="rId5"><w:r><w:rPr><w:u w:val="single"/><w:color w:val="0563C1"/></w:rPr><w:t xml:space="preserve">external link</w:t></w:r></w:hyperlink>
  {}
  <w:hyperlink w:anchor="target"><w:r><w:rPr><w:u w:val="single"/></w:rPr><w:t xml:space="preserve">internal link</w:t></w:r></w:hyperlink>
</w:p>"#,
        r("", "Anchored here. "),
        r("", " and "),
    ));

    // (5) A commented range (§17.13.4) — the per-item `comment_ids` carrier.
    body.push_str(&format!(
        r#"<w:p><w:commentRangeStart w:id="0"/>{}<w:commentRangeEnd w:id="0"/>
  <w:r><w:commentReference w:id="0"/></w:r>{}</w:p>"#,
        r("", "This span carries a comment."),
        r("", " Uncommented tail."),
    ));

    // (6) An RTL paragraph (§17.3.1.6 bidi + §17.3.2.30 rtl). `base_rtl` fired
    // exactly ONCE in the entire suite before this, and never on a pinned doc, so
    // no pin reached the UAX#9 L2 reorder at all.
    body.push_str(&format!(
        r#"<w:p><w:pPr><w:bidi/></w:pPr>{}{}</w:p>"#,
        r(r#"<w:rtl/>"#, "\u{5E9}\u{5DC}\u{5D5}\u{5DD} \u{5E2}\u{5D5}\u{5DC}\u{5DD} "),
        r("", "latin tail 123"),
    ));

    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr>"#,
    );

    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>{body}</w:body></w:document>"#
    );
    let comments = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:comments {W_NS}><w:comment w:id="0" w:author="Ada Lovelace" w:initials="AL" w:date="2026-07-26T09:00:00Z">
  <w:p><w:r><w:t xml:space="preserve">One comment, one range.</w:t></w:r></w:p>
</w:comment></w:comments>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
  <Override PartName="/word/comments.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml"/>
</Types>"#.to_string();
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#.to_string();
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>
  <Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.invalid/spec" TargetMode="External"/>
</Relationships>"#.to_string();

    build_docx(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", styles),
        ("word/comments.xml", comments),
    ])
}

#[test]
fn oracle_attributes_snapshot() {
    // NEW pin — nothing pre-existing is re-recorded. Its whole job is to make the
    // ITEM stream non-vacuous: the axes below had ZERO occurrences across all 269
    // documents the suite laid out before it.
    assert_oracle("oracle-attributes", oracle_attributes_docx(), 1, 0xb0b46bc9a53d2147);
}

/// The fixture above is only worth its weight if it really does carry the axes
/// the census scored zero on. Assert that directly, so a future edit that guts
/// it fails loudly instead of quietly re-emptying the corpus.
#[test]
fn oracle_attributes_actually_covers_the_zero_census_axes() {
    let viewer = layout(oracle_attributes_docx());
    let pages = viewer.pages.borrow();
    let mut families = std::collections::BTreeSet::new();
    let (mut links, mut caps, mut small_caps, mut vanish, mut strike, mut dstrike) = (0, 0, 0, 0, 0, 0);
    let (mut effects, mut highlight, mut run_shading, mut spacing, mut shift) = (0, 0, 0, 0, 0);
    let (mut comments, mut bookmarks, mut rtl, mut tabs) = (0, 0, 0, 0);
    let (mut bordered, mut shaded, mut bartab_lines) = (0, 0, 0);
    for page in pages.iter() {
        for el in &page.elements {
            let PageElement::Line(l) = el else { continue };
            if l.border_top.is_some() || l.border_bottom.is_some()
                || l.border_left.is_some() || l.border_right.is_some() { bordered += 1; }
            if l.para_shading.is_some() { shaded += 1; }
            if !l.bar_tabs.is_empty() { bartab_lines += 1; }
            for s in &l.segments {
                for it in &s.items {
                    families.insert(it.style.family.clone());
                    if it.link.is_some() { links += 1; }
                    if it.bookmark.is_some() { bookmarks += 1; }
                    if !it.comment_ids.is_empty() { comments += 1; }
                    if it.tab.is_some() { tabs += 1; }
                    if it.rtl { rtl += 1; }
                    if it.style.all_caps { caps += 1; }
                    if it.style.small_caps { small_caps += 1; }
                    if it.style.vanish { vanish += 1; }
                    if it.style.strikethrough { strike += 1; }
                    if it.style.double_strike { dstrike += 1; }
                    if it.style.outline || it.style.shadow
                        || it.style.emboss || it.style.imprint { effects += 1; }
                    if it.style.highlight.is_some() { highlight += 1; }
                    if it.style.run_shading.is_some() { run_shading += 1; }
                    if it.style.char_spacing_px != 0.0 { spacing += 1; }
                    if it.style.baseline_shift_px != 0.0 { shift += 1; }
                }
            }
        }
    }
    for (what, n) in [
        ("hyperlink targets", links), ("bookmarks", bookmarks), ("comment ids", comments),
        ("tab items", tabs), ("all-caps", caps), ("small-caps", small_caps),
        ("strikethrough", strike), ("double-strike", dstrike),
        ("outline/shadow/emboss/imprint", effects), ("highlight", highlight),
        ("run shading", run_shading), ("char spacing", spacing), ("baseline shift", shift),
        ("RTL items", rtl), ("bordered lines", bordered), ("shaded lines", shaded),
        ("bar-tab lines", bartab_lines),
    ] {
        assert!(n > 0, "oracle-attributes stopped covering {what} — the ITEM stream is \
                        blind to an axis again (census had 0 of these suite-wide)");
    }
    assert!(families.len() >= 3,
        "oracle-attributes must carry ≥3 distinct font families (the whole suite averaged \
         ~1.1 per document), got {families:?}");
    // `w:vanish` is the one census zero that is NOT fixture debt: `collect_items`
    // (linebreak.rs) `continue`s on a hidden run, so a vanished run can never
    // reach a painted item in ANY document. The fixture carries one anyway (the
    // paragraph above), and this pins the drop — if hidden text ever starts
    // reaching layout, that is a real behaviour change and this fails first.
    // The `style.vanish` FIELD is still folded into the ITEM stream, which is
    // what catches the flag being flipped after layout.
    assert_eq!(vanish, 0,
        "a `w:vanish` run reached a painted item — hidden text must be dropped in collect_items");
}

// ─── hyperlink TargetMode faithfulness ───────────────────────────────────────

/// One paragraph, three hyperlinks, one bookmark:
///  - `rId5` → a rel with `TargetMode="External"` (a real URL),
///  - `rId6` → a rel with NO `TargetMode` attribute — per OPC (ECMA-376
///    Part 2; not in the s17–s21 extracts) an absent `TargetMode` is
///    `Internal`, i.e. a package part resolved relative to the source part,
///    NOT an external URL,
///  - a `w:anchor` link (§17.16.22) to the bookmark `target`.
fn targetmode_docx() -> Vec<u8> {
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}>
  <w:docDefaults>
    <w:rPrDefault><w:rPr><w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/></w:rPr></w:rPrDefault>
    <w:pPrDefault><w:pPr/></w:pPrDefault>
  </w:docDefaults>
</w:styles>"#
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}><w:body>
<w:p><w:bookmarkStart w:id="1" w:name="target"/>{}<w:bookmarkEnd w:id="1"/>
  <w:hyperlink r:id="rId5"><w:r><w:t xml:space="preserve">extlink</w:t></w:r></w:hyperlink>
  <w:hyperlink r:id="rId6"><w:r><w:t xml:space="preserve">intrel</w:t></w:r></w:hyperlink>
  <w:hyperlink w:anchor="target"><w:r><w:t xml:space="preserve">anchlink</w:t></w:r></w:hyperlink>
</w:p>
<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr>
</w:body></w:document>"#,
        r("", "Anchored. "),
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    // rId6 deliberately carries NO TargetMode attribute (⇒ Internal per OPC).
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.invalid/ext" TargetMode="External"/>
  <Relationship Id="rId6" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="attachedPart.xml"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

/// Only EXTERNAL hyperlink rels may reach consumers as
/// `url:` targets. An Internal-mode rel target is a package part path — the
/// engine used to report it verbatim as an external URL (`url:attachedPart.xml`),
/// which consumers then treated as a navigable href. Faithful transcription,
/// not policy: the Internal run now carries NO link (it can NOT fall back to
/// `w:anchor` — §17.16.22: when r:id is specified, anchor "shall be ignored";
/// here r:id is present but unresolvable, so the run is plain text). External
/// URL resolution and `w:anchor` bookmark navigation are pinned unchanged.
#[test]
fn internal_targetmode_rel_is_not_reported_as_url() {
    let viewer = layout(targetmode_docx());

    let mut stamped: Vec<(String, String)> = Vec::new();
    let mut intrel_items = 0usize;
    for l in body_lines(&viewer) {
        for s in &l.segments {
            for it in &s.items {
                if it.text.contains("intrel") { intrel_items += 1; }
                if let Some(ref link) = it.link {
                    stamped.push((it.text.clone(), link.clone()));
                }
            }
        }
    }

    // (a) The external rel still resolves to its URL.
    assert!(
        stamped.iter().any(|(_, l)| l == "url:https://example.invalid/ext"),
        "external (TargetMode=\"External\") hyperlink no longer resolves; stamped: {stamped:?}"
    );
    // (b) The Internal rel (absent TargetMode ⇒ Internal per OPC) must NOT be
    // reported as a url: target — and being an r:id link it must not morph
    // into an anchor: one either (§17.16.22 precedence).
    for (text, link) in &stamped {
        assert!(
            !text.contains("intrel"),
            "Internal-mode hyperlink rel leaked to a consumer-visible link \
             ({text:?} → {link:?}); an Internal target is a package part, not a URL"
        );
    }
    // ... while the run's text still renders as plain content.
    assert!(intrel_items > 0, "the Internal-rel hyperlink's text must still lay out");
    // (c) w:anchor bookmark links are untouched: the link stamps and the
    // bookmark itself resolves to a position (the navigation substrate behind
    // `bookmarkPosition`).
    assert!(
        stamped.iter().any(|(_, l)| l == "anchor:target"),
        "w:anchor hyperlink lost its anchor: target; stamped: {stamped:?}"
    );
    let pages = viewer.pages.borrow();
    assert!(
        pages.iter().any(|pg| super::bookmark_page_y(pg, "target").is_some()),
        "bookmark 'target' must resolve to a page position (anchor navigation)"
    );
}

// ─── fixpoint rollback coverage: one fixture per ResumeAt variant ────────────
//
// The 2026-07-28 rollback census (PARASMA_STATS_CENSUS over the full suite)
// measured every fixpoint rollback re-entering through a BlockBoundary (35
// events) or ParaLines (33 events) checkpoint. The other FIVE resume shapes —
// the ones threaded through the riskiest rollback call sites — were rolled back
// through ZERO times, so a control-flow refactor could break rollback on any of
// them with the whole suite green. The five fixtures below close that gap, one
// per variant, all sharing one shape:
//
//   shape: [page-2 start of the variant's own flavour] [short paragraph P]
//          [anchor A with a margin-pinned wrapSquare textbox taller than the
//           page-2 content above A]
//
// A's float band pins to the page-2 content top and overlaps [topmost .. A.top]
// (seal_page's detector sees only Paragraph block_tops — a mid-block carryover
// and a table row both record none, which is why P must exist), so page 2 seals
// with a violation and rolls back into its start checkpoint.
// `Viewer::rollback_log` pins the variant itself so a fixture that drifts to a
// different resume shape fails loudly instead of silently pinning nothing.
//
//   ParaItems           mid-block     straddling paragraph (place_float_aware!)
//   RowSplit            mid-block     straddling table row  (place_split_row!)
//   ParaTop             page-start    keepLines whole-paragraph push
//   RowWhole            page-start    cantSplit whole-row push
//   PageBreakBeforeTop  page-start    pageBreakBefore inline advance
//
// BlockBoundary and ParaLines are the two the census already covers; the seven
// pre-existing float-fixpoint tests listed above `can_rollback` (lib.rs) are
// their guard, so they get no fixture here.
//
// The PageBreakBeforeTop fixture found a live defect on first run: the page-
// preset injection makes `current` non-empty on a re-lay, so the variant's
// `!current.is_empty()` self-neutralisation did not hold and the break re-fired
// (debug_assert panic in debug, a second rollback in release). Fixed at the site
// — see the `resumed_page_break` guard in lib.rs.

/// An anchored inset wrapSquare textbox (`wrapText="right"`, column-left) with
/// the given vertical relative base ("paragraph" pre-anchor-band on page 1;
/// "margin" = cursor-independent, the fixpoint trigger class).
fn w62_anchor_textbox(id: u32, v_rel: &str, cx_emu: i64, cy_emu: i64) -> String {
    format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:simplePos x="0" y="0"/>
  <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
  <wp:positionV relativeFrom="{v_rel}"><wp:posOffset>0</wp:posOffset></wp:positionV>
  <wp:extent cx="{cx_emu}" cy="{cy_emu}"/>
  <wp:effectExtent l="0" t="0" r="0" b="0"/>
  <wp:wrapSquare wrapText="right"/>
  <wp:docPr id="{id}" name="TextBox {id}"/>
  <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
    <wps:wsp><wps:cNvSpPr txBox="1"/>
      <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx_emu}" cy="{cy_emu}"/></a:xfrm>
        <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>
      <wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">Pin {id}.</w:t></w:r></w:p></w:txbxContent></wps:txbx>
      <wps:bodyPr/></wps:wsp>
  </a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r>"#
    )
}

/// Shared OPC shell for the two coverage fixtures (Arimo 11pt docDefaults, no
/// paragraph after-spacing, Letter page, 1in margins → 624×864 px content).
fn w62_docx(body_pre_sectpr: &str) -> Vec<u8> {
    let body = format!(
        r#"{body_pre_sectpr}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:num="1"/></w:sectPr>"#
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document {W_NS} {R_NS}
  xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><w:body>{body}</w:body></w:document>"#
    );
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr>
  <w:rFonts w:ascii="Arimo" w:hAnsi="Arimo" w:cs="Arimo"/><w:sz w:val="22"/><w:szCs w:val="22"/>
</w:rPr></w:rPrDefault></w:docDefaults></w:styles>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
    build_docx(&[
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("_rels/.rels", root_rels.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes().to_vec()),
        ("word/styles.xml", styles.into_bytes()),
    ])
}

/// Every body-line item's text on one page, concatenated (completeness pin).
fn w62_page_text(page: &super::wml::page::PageLayout) -> String {
    use super::wml::page::PageElement;
    let mut s = String::new();
    for el in &page.elements {
        if let PageElement::Line(l) = el {
            for seg in &l.segments {
                for it in &seg.items {
                    s.push_str(&it.text);
                }
            }
        }
    }
    s
}

/// F1: one LONG paragraph (its own paragraph-relative inset float forces the
/// place_float_aware! route, so its page-1→2 overflow records a ParaItems
/// checkpoint) straddling onto page 2, then P, then the margin-pinned anchor.
fn w62_paraitems_docx(long_sentences: usize) -> Vec<u8> {
    const INSET_EMU: i64 = 1_828_800; // 2in — inset (≈31% of the 6.5in column)
    const PIN_H_EMU: i64 = 2_743_200; // 3in tall — taller than the carryover
    let para_float = w62_anchor_textbox(1, "paragraph", INSET_EMU, INSET_EMU);
    let margin_float = w62_anchor_textbox(2, "margin", INSET_EMU, PIN_H_EMU);
    let mut body = String::new();
    body.push_str(&format!(
        "<w:p>{para_float}{}</w:p>",
        r("", &format!("{} ENDLONGPARA.", filler(0, long_sentences)))
    ));
    body.push_str(&p("", &r("", "Between-marker paragraph on page two.")));
    body.push_str(&format!("<w:p>{margin_float}{}</w:p>", r("", "Anchor paragraph text.")));
    w62_docx(&body)
}

#[test]
fn rollback_reenters_through_paraitems_checkpoint() {
    let viewer = layout(w62_paraitems_docx(55));
    use super::wml::page::PageElement;
    let stats = viewer.layout_stats();
    let pages = viewer.pages.borrow();
    let log = viewer.rollback_log.borrow();

    // Fixture preconditions — drift here invalidates the coverage claim, so it
    // must fail loudly, not skew silently.
    assert_eq!(pages.len(), 2, "fixture must straddle exactly 2 pages (got {})", pages.len());
    assert!(stats.pages_relaid >= 1, "page 2 must roll back (pages_relaid = 0)");

    // THE coverage pin: the rollback re-entered through a ParaItems checkpoint.
    assert!(
        log.contains(&"ParaItems"),
        "no rollback re-entered through a ParaItems checkpoint; rollback_log = {log:?}"
    );

    // Wrap invariant (§20.4.2.17): after the fixpoint, no page-2 body line in
    // the margin-pinned band may start under the box.
    assert!(
        w62_assert_wrap_beside(&pages[1]) >= 1,
        "no page-2 line landed beside the pinned band — band never contested"
    );

    // Completeness: the straddling paragraph's tail survived the replay.
    let all_text = format!("{}{}", w62_page_text(&pages[0]), w62_page_text(&pages[1]));
    assert!(
        all_text.contains("ENDLONGPARA"),
        "the straddling paragraph's final items were dropped by the rollback replay"
    );
}

/// F2: one table row tall enough that it MUST split across the page break
/// (place_split_row! records a RowSplit checkpoint at the page-2 start), then
/// P, then the margin-pinned anchor.
fn w62_rowsplit_docx(cell_lines: usize) -> Vec<u8> {
    const INSET_EMU: i64 = 1_828_800; // 2in
    const PIN_H_EMU: i64 = 3_657_600; // 4in — taller than the row's carryover
    let margin_float = w62_anchor_textbox(2, "margin", INSET_EMU, PIN_H_EMU);
    let mut tall = String::new();
    for k in 0..cell_lines {
        tall.push_str(&p("", &r("", &format!("Tall cell line {k}"))));
    }
    let mut body = String::new();
    // The row-split trigger requires a non-empty page (`!current.is_empty()`,
    // lib.rs row-split trigger clause) — a page-top row whole-emits instead of
    // splitting. The intro paragraph makes the split reachable.
    body.push_str(&p("", &r("", "Intro paragraph above the table.")));
    body.push_str(&format!(
        r#"<w:tbl><w:tblPr><w:tblW w:w="9360" w:type="dxa"/></w:tblPr><w:tblGrid><w:gridCol w:w="9360"/></w:tblGrid><w:tr><w:tc><w:tcPr><w:tcW w:w="9360" w:type="dxa"/></w:tcPr>{tall}</w:tc></w:tr></w:tbl>"#
    ));
    body.push_str(&p("", &r("", "Between-marker paragraph on page two.")));
    body.push_str(&format!("<w:p>{margin_float}{}</w:p>", r("", "Anchor paragraph text.")));
    w62_docx(&body)
}

#[test]
fn rollback_reenters_through_rowsplit_checkpoint() {
    let viewer = layout(w62_rowsplit_docx(52));
    use super::wml::page::PageElement;
    let stats = viewer.layout_stats();
    let pages = viewer.pages.borrow();
    let log = viewer.rollback_log.borrow();

    assert_eq!(pages.len(), 2, "fixture must straddle exactly 2 pages (got {})", pages.len());
    assert!(stats.pages_relaid >= 1, "page 2 must roll back (pages_relaid = 0)");

    // THE coverage pin.
    assert!(
        log.contains(&"RowSplit"),
        "no rollback re-entered through a RowSplit checkpoint; rollback_log = {log:?}"
    );

    // The split row must actually fragment across both pages (a whole-row push
    // would have recorded RowWhole/BlockBoundary instead — belt to the pin).
    let frags_per_page: Vec<usize> = pages.iter()
        .map(|pg| pg.elements.iter().filter(|el| matches!(el, PageElement::TableRow(_))).count())
        .collect();
    assert!(
        frags_per_page.iter().filter(|&&n| n > 0).count() >= 2,
        "row fragments must land on both pages (got {frags_per_page:?})"
    );

    // Completeness: the cell's final paragraph survived the replay.
    let mut cell_text = String::new();
    for pg in pages.iter() {
        for el in &pg.elements {
            if let PageElement::TableRow(row) = el {
                for cell in &row.cells {
                    for l in &cell.lines {
                        for seg in &l.segments {
                            for it in &seg.items {
                                cell_text.push_str(&it.text);
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        cell_text.contains("Tall cell line 51"),
        "the split row's final cell paragraph was dropped by the rollback replay"
    );

    // Wrap invariant for the two body paragraphs on page 2 (table fragments are
    // invisible to the detector and may sit under the band — only Paragraph
    // lines are checked).
    assert!(
        w62_assert_wrap_beside(&pages[1]) >= 1,
        "no page-2 paragraph line landed beside the pinned band — band never contested"
    );
}

/// The §20.4.2.17 wrap invariant on one page, against its first positioned
/// textbox: no body LINE whose baseline falls inside the box's vertical band
/// may start left of the box's right edge. Returns how many lines were in the
/// band, so a caller can insist the band was actually contested (a fixture
/// whose float stopped overlapping anything would otherwise pass vacuously).
///
/// Lines only — table-row fragments do not wrap around floats and are invisible
/// to `seal_page`'s detector (it reads Paragraph block_tops alone), so a row may
/// legitimately sit under the band.
fn w62_assert_wrap_beside(page: &super::wml::page::PageLayout) -> usize {
    use super::wml::page::PageElement;
    let tb = page.positioned_text_boxes.first().expect("margin-pinned box drawn on this page");
    let box_right = tb.x + tb.width_px;
    let (band_top, band_bottom) = (tb.y, tb.y + tb.height_px);
    let mut in_band = 0;
    for el in &page.elements {
        if let PageElement::Line(l) = el {
            if l.baseline_y > band_top && l.baseline_y < band_bottom {
                in_band += 1;
                let first_x = l.segments.first().map(|s| s.x_start).unwrap_or(l.content_left);
                assert!(
                    first_x >= box_right - 0.5,
                    "line at baseline_y {:.2} starts x {first_x:.2}, under the box (right edge {box_right:.2})",
                    l.baseline_y
                );
            }
        }
    }
    in_band
}

/// F3: a `keepLines` paragraph K that does not fit the page-1 remainder is
/// pushed WHOLE to page 2 (§17.3.1.14 — the need_break test records a ParaTop
/// checkpoint), then P, then the margin-pinned anchor. K itself supplies the
/// detector's above-anchor block_top: a ParaTop re-entry skips spacing but NOT
/// registration, so K re-pushes block_tops on the re-lay.
fn w62_paratop_docx(fill_sentences: usize, keep_sentences: usize) -> Vec<u8> {
    const INSET_EMU: i64 = 1_828_800; // 2in — inset (≈31% of the 6.5in column)
    const PIN_H_EMU: i64 = 3_657_600; // 4in — spans K + P down past the anchor
    let margin_float = w62_anchor_textbox(2, "margin", INSET_EMU, PIN_H_EMU);
    let mut body = String::new();
    body.push_str(&p("", &r("", &filler(0, fill_sentences))));
    body.push_str(&p(
        "<w:keepLines/>",
        &r("", &format!("STARTKEEP. {} ENDKEEPPARA.", filler(1, keep_sentences))),
    ));
    body.push_str(&p("", &r("", "Between-marker paragraph on page two.")));
    body.push_str(&format!("<w:p>{margin_float}{}</w:p>", r("", "Anchor paragraph text.")));
    w62_docx(&body)
}

#[test]
fn rollback_reenters_through_paratop_checkpoint() {
    let viewer = layout(w62_paratop_docx(44, 8));
    let stats = viewer.layout_stats();
    let pages = viewer.pages.borrow();
    let log = viewer.rollback_log.borrow();

    // Fixture preconditions — drift here invalidates the coverage claim, so it
    // must fail loudly, not skew silently.
    assert_eq!(pages.len(), 2, "fixture must straddle exactly 2 pages (got {})", pages.len());
    assert!(stats.pages_relaid >= 1, "page 2 must roll back (pages_relaid = 0)");

    // THE coverage pin: the rollback re-entered through a ParaTop checkpoint.
    assert!(
        log.contains(&"ParaTop"),
        "no rollback re-entered through a ParaTop checkpoint; rollback_log = {log:?}"
    );

    // Shape belt: keepLines moved K WHOLE — not one line of it stayed on page 1,
    // and page 1 is non-empty (an empty page 1 would mean no push happened).
    let (p1, p2) = (w62_page_text(&pages[0]), w62_page_text(&pages[1]));
    assert!(!p1.is_empty(), "page 1 must carry the filler paragraph");
    assert!(!p1.contains("STARTKEEP"), "keepLines paragraph must not start on page 1");
    assert!(p2.contains("STARTKEEP"), "keepLines paragraph must start on page 2");

    // Behaviour, not just the counter: after the fixpoint, no page-2 line in the
    // margin-pinned band may start under the box (§20.4.2.17).
    assert!(
        w62_assert_wrap_beside(&pages[1]) >= 1,
        "no page-2 line landed beside the pinned band — band never contested"
    );

    // Completeness: K survived the replay intact.
    assert!(
        p2.contains("ENDKEEPPARA"),
        "the pushed paragraph's final items were dropped by the rollback replay"
    );
}

/// F4: a `pageBreakBefore` paragraph B opens page 2 through the inline advance
/// (§17.3.1.23 — the only page start that defers the pending-float drain, so it
/// carries its own PageBreakBeforeTop cursor), then P, then the anchor.
///
/// LIMIT, stated so nobody reads more coverage into this than it has: what makes
/// the variant unique is the DEFERRED `pending_floats` drain, and this document
/// has no deferred float, so the deferral itself is uncontested. Pairing an
/// over-tall deferred image with a pageBreakBefore page start is a strictly
/// harder tuning problem, left as a known residual.
fn w62_pagebreakbefore_docx(b_sentences: usize) -> Vec<u8> {
    const INSET_EMU: i64 = 1_828_800; // 2in
    const PIN_H_EMU: i64 = 3_657_600; // 4in
    let margin_float = w62_anchor_textbox(2, "margin", INSET_EMU, PIN_H_EMU);
    let mut body = String::new();
    // The break only fires on a non-empty page (`!current.is_empty()`), so page
    // one needs one paragraph — nothing more; this fixture needs no fill tuning.
    body.push_str(&p("", &r("", "Intro paragraph on page one.")));
    body.push_str(&p(
        "<w:pageBreakBefore/>",
        &r("", &format!("STARTBREAK. {} ENDBREAKPARA.", filler(2, b_sentences))),
    ));
    body.push_str(&p("", &r("", "Between-marker paragraph on page two.")));
    body.push_str(&format!("<w:p>{margin_float}{}</w:p>", r("", "Anchor paragraph text.")));
    w62_docx(&body)
}

#[test]
fn rollback_reenters_through_pagebreakbefore_checkpoint() {
    let viewer = layout(w62_pagebreakbefore_docx(4));
    let stats = viewer.layout_stats();
    let pages = viewer.pages.borrow();
    let log = viewer.rollback_log.borrow();

    assert_eq!(pages.len(), 2, "fixture must straddle exactly 2 pages (got {})", pages.len());
    assert!(stats.pages_relaid >= 1, "page 2 must roll back (pages_relaid = 0)");

    // THE coverage pin.
    assert!(
        log.contains(&"PageBreakBeforeTop"),
        "no rollback re-entered through a PageBreakBeforeTop checkpoint; rollback_log = {log:?}"
    );
    // The break must fire ONCE. It used to re-fire on the re-lay (the page-preset
    // injection makes `current` non-empty), emitting a spurious page and logging
    // a second rollback — this is the pin on that fix.
    assert_eq!(
        log.iter().filter(|v| **v == "PageBreakBeforeTop").count(), 1,
        "the pageBreakBefore advance re-fired on the re-lay; rollback_log = {log:?}"
    );

    // Shape belt: B is entirely on page 2, and page 1 kept only the intro.
    let (p1, p2) = (w62_page_text(&pages[0]), w62_page_text(&pages[1]));
    assert!(p1.contains("Intro paragraph on page one."), "page 1 must keep the intro");
    assert!(!p1.contains("STARTBREAK") && !p1.contains("ENDBREAKPARA"),
        "the break-bearing paragraph must be wholly on page 2");
    assert!(p2.contains("STARTBREAK"), "break-bearing paragraph must start page 2");

    assert!(
        w62_assert_wrap_beside(&pages[1]) >= 1,
        "no page-2 line landed beside the pinned band — band never contested"
    );

    assert!(
        p2.contains("ENDBREAKPARA"),
        "the break-bearing paragraph's final items were dropped by the rollback replay"
    );
}

/// F5: a `cantSplit` row (§17.4.7) taller than the page-1 remainder is pushed
/// WHOLE to page 2 (the whole-row overflow test records a RowWhole checkpoint).
/// cantSplit is what forces RowWhole instead of RowSplit — without it the tall
/// row takes place_split_row! and re-pins the already-covered variant.
///
/// LIMIT: `seal_page`'s detector records Paragraph block_tops only, so the row
/// itself is invisible to it and the between-marker paragraph P is the mandatory
/// donor of the above-anchor block_top. This fixture therefore pins that the
/// RowWhole CALL SITE propagates the rollback; it does NOT show that a float
/// overlapping table content is detected. That detector gap is pre-existing and
/// documented at `seal_page` — do not read this fixture as closing it.
fn w62_rowwhole_docx(fill_sentences: usize, cell_lines: usize) -> Vec<u8> {
    const INSET_EMU: i64 = 1_828_800; // 2in
    const PIN_H_EMU: i64 = 4_572_000; // 5in — must reach past the row down to P
    let margin_float = w62_anchor_textbox(2, "margin", INSET_EMU, PIN_H_EMU);
    let mut tall = String::new();
    for k in 0..cell_lines {
        tall.push_str(&p("", &r("", &format!("Tall cell line {k}"))));
    }
    let mut body = String::new();
    // The whole-row push needs a non-empty page (`!current.is_empty()`) AND a
    // remainder shorter than the row.
    body.push_str(&p("", &r("", &filler(0, fill_sentences))));
    body.push_str(&format!(
        r#"<w:tbl><w:tblPr><w:tblW w:w="9360" w:type="dxa"/></w:tblPr><w:tblGrid><w:gridCol w:w="9360"/></w:tblGrid><w:tr><w:trPr><w:cantSplit/></w:trPr><w:tc><w:tcPr><w:tcW w:w="9360" w:type="dxa"/></w:tcPr>{tall}</w:tc></w:tr></w:tbl>"#
    ));
    body.push_str(&p("", &r("", "Between-marker paragraph on page two.")));
    body.push_str(&format!("<w:p>{margin_float}{}</w:p>", r("", "Anchor paragraph text.")));
    w62_docx(&body)
}

#[test]
fn rollback_reenters_through_rowwhole_checkpoint() {
    let viewer = layout(w62_rowwhole_docx(44, 10));
    use super::wml::page::PageElement;
    let stats = viewer.layout_stats();
    let pages = viewer.pages.borrow();
    let log = viewer.rollback_log.borrow();

    assert_eq!(pages.len(), 2, "fixture must straddle exactly 2 pages (got {})", pages.len());
    assert!(stats.pages_relaid >= 1, "page 2 must roll back (pages_relaid = 0)");

    // THE coverage pin.
    assert!(
        log.contains(&"RowWhole"),
        "no rollback re-entered through a RowWhole checkpoint; rollback_log = {log:?}"
    );

    // Shape belt: cantSplit held — exactly ONE row fragment exists, on page 2.
    // A split would have fragmented it across both pages and logged RowSplit.
    let frags_per_page: Vec<usize> = pages.iter()
        .map(|pg| pg.elements.iter().filter(|el| matches!(el, PageElement::TableRow(_))).count())
        .collect();
    assert_eq!(frags_per_page, vec![0, 1], "cantSplit row must emit one whole fragment on page 2");

    assert!(
        w62_assert_wrap_beside(&pages[1]) >= 1,
        "no page-2 paragraph line landed beside the pinned band — band never contested"
    );

    // Completeness: the whole row survived the replay.
    let mut cell_text = String::new();
    for el in &pages[1].elements {
        if let PageElement::TableRow(row) = el {
            for cell in &row.cells {
                for l in &cell.lines {
                    for seg in &l.segments {
                        for it in &seg.items {
                            cell_text.push_str(&it.text);
                        }
                    }
                }
            }
        }
    }
    assert!(
        cell_text.contains("Tall cell line 0") && cell_text.contains("Tall cell line 9"),
        "the whole-pushed row lost cell content in the rollback replay"
    );
}

// ─── the rollback census, in-tree and self-baselining ────────────────────────
//
// The census that found the five blind resume variants was a shell pipeline
// (`PARASMA_STATS_CENSUS=1 cargo test … | grep -o 'resume=[A-Za-z]*'`) whose
// output lived in /tmp. Two reasons that cannot be the durable record:
//
//   * IT DOES NOT REPRODUCE ON A CLEAN CHECKOUT. The full-suite histogram sums
//     rows from tests that need calibration documents (resolved only through
//     the local calibration manifest) and from tests that need gitignored
//     fixtures. On a fresh clone those degrade or skip, so a committed
//     whole-suite baseline would be RED on exactly the machine that most needs
//     it — the failure class only a clean checkout catches, never the local gate.
//   * A HISTOGRAM IS A WEAK PIN ANYWAY. `BlockBoundary 35 → 36` is the normal
//     consequence of adding any float test; `BlockBoundary 35 → 0` is a
//     rollback path that stopped happening. A golden calls both red, so it gets
//     re-blessed reflexively and stops meaning anything.
//
// So the census is SELF-BASELINING and restricted BY CONSTRUCTION to
// machine-independent rows: every row below is an inline synthetic document
// built from XML string literals in this file — no external file, no env var,
// no /tmp, nothing under `fixtures/` or `.calibration/`. It runs in the normal
// suite on a clean checkout. What it pins is the invariant that actually
// matters and cannot drift into meaninglessness:
//
//     EVERY `ResumeAt` VARIANT IS ROLLED BACK THROUGH BY A NAMED IN-TREE
//     DOCUMENT — so no fixpoint refactor can drop a variant's rollback silently.
//
// The `PARASMA_STATS_CENSUS=1` instrument stays: it is how you discover which
// variants a NEW corpus reaches. This test is how the answer stays true.

/// The census row for one resume variant: its label, and a document builder
/// whose layout is REQUIRED to roll back through that variant.
///
/// EXHAUSTIVE BY CONSTRUCTION — adding a `ResumeAt` variant fails to compile
/// here until a census row is declared for it. The declared builder must then
/// actually produce that variant (asserted in the test), so a placeholder row
/// cannot be smuggled past: it goes red on the next run.
///
/// RESIDUAL, stated rather than papered over: the compile break lands on this
/// match, but the instance list in the test below is ordinary data. Extend both
/// in the same edit — the match is what drags you here, the list is what makes
/// the new variant actually get laid out.
fn w62_census_row(at: &super::ResumeAt) -> (&'static str, fn() -> Vec<u8>) {
    use super::ResumeAt;
    match at {
        // The seven float-fixpoint tests are the pre-existing guard for these
        // two; the census reuses two of their documents so every variant has a
        // row, rather than leaving a hole labelled "covered elsewhere".
        ResumeAt::BlockBoundary { .. } => ("BlockBoundary", page_relative_inset_wraps_docx),
        ResumeAt::ParaLines { .. } => ("ParaLines", overtall_page_float_docx),
        // The two mid-block shapes.
        ResumeAt::ParaItems { .. } => ("ParaItems", || w62_paraitems_docx(55)),
        ResumeAt::RowSplit { .. } => ("RowSplit", || w62_rowsplit_docx(52)),
        // The three page-start shapes that were blind.
        ResumeAt::ParaTop { .. } => ("ParaTop", || w62_paratop_docx(44, 8)),
        ResumeAt::RowWhole { .. } => ("RowWhole", || w62_rowwhole_docx(44, 10)),
        ResumeAt::PageBreakBeforeTop { .. } =>
            ("PageBreakBeforeTop", || w62_pagebreakbefore_docx(4)),
    }
}

#[test]
fn rollback_census_covers_every_resume_variant() {
    use super::{ParaItemsSrc, ParaLineSrc, ResumeAt};
    // One instance per variant. Field values are irrelevant — only the
    // discriminant is read (by `w62_census_row` and by `census_label`).
    let variants = [
        ResumeAt::BlockBoundary { next_block: 0 },
        ResumeAt::PageBreakBeforeTop { block: 0 },
        ResumeAt::ParaTop { block: 0 },
        ResumeAt::ParaLines { block: 0, next_line: 0, src: ParaLineSrc::OverflowBreak },
        ResumeAt::ParaItems {
            block: 0, items_remaining: 0, is_first_batch: false, src: ParaItemsSrc::FloatAware,
        },
        ResumeAt::RowWhole { block: 0, next_row: 0 },
        ResumeAt::RowSplit {
            block: 0, row: 0, entry_col: 0, cursors: Vec::new(), consumed: 0.0,
        },
    ];

    let mut seen: Vec<&'static str> = Vec::new();
    for at in &variants {
        let (label, build) = w62_census_row(at);
        // Cross-check the pairing against lib.rs's own labeller: a row wired to
        // the wrong arm (copy-paste) fails here rather than silently censusing
        // one variant twice and leaving another uncovered.
        assert_eq!(
            at.census_label(), label,
            "census row for {:?} declares label {label:?} but ResumeAt::census_label says {:?}",
            at, at.census_label()
        );
        assert!(!seen.contains(&label), "two census rows share the label {label:?}");
        seen.push(label);

        // THE CENSUS: lay the document out and require a rollback through this
        // variant. `rollback_log` is written only on the ROLLBACK branch of the
        // fixpoint verdict, so this cannot be satisfied by a document that
        // merely contains a float.
        let viewer = layout(build());
        let log = viewer.rollback_log.borrow().clone();
        assert!(
            log.contains(&label),
            "census row {label:?} does not roll back through that variant — its \
             document rolled back through {log:?}. Either the fixture drifted or \
             a fixpoint change dropped the rollback on this arm."
        );
    }

    // `seen` gains exactly one entry per loop iteration with no `continue` and
    // no early exit, so comparing its length to the loop bound cannot fail —
    // it restates the loop. Assert the CONTENT instead: every variant name the
    // engine can produce must appear, which is what the test's name promises
    // and what actually breaks if a fixpoint change drops an arm.
    let mut names: Vec<&str> = seen.clone();
    names.sort_unstable();
    names.dedup();
    let mut want: Vec<&str> = variants.iter().map(|v| v.census_label()).collect();
    want.sort_unstable();
    want.dedup();
    assert_eq!(
        names, want,
        "census variant coverage drifted — every ResumeAt variant needs its own row"
    );
}

// ── backpressure: layout stops BEFORE the memory wall ────────────────────────
//
// The three page-boundary yield points check `should_truncate_layout()`; these
// tests inject pressure through the native `test_memory_bytes` stand-in (real
// wasm reads `memory_size`). Pressure is raised FROM THE PAGE SINK so the run
// truncates mid-document, not at page one — the shape a real memory ramp has.

#[test]
fn layout_truncates_at_memory_budget() {
    let viewer = layout(perf_docx(&perf_float_free_body(48)));
    let full = viewer.page_count();
    assert!(full >= 3, "fixture must span several pages, got {full}");
    assert_eq!(viewer.layout_truncated_at_js(), None, "untruncated run must report None");

    viewer.set_memory_budget(1_000.0);
    let n = viewer
        .run_layout_native_with(|idx| {
            if idx == 1 {
                viewer.set_test_memory_bytes(2_000); // over budget after page 2 lands
            }
        })
        .expect("truncated layout still returns Ok");
    assert_eq!(n, 2, "layout must stop at the first page boundary after the budget trip");
    assert_eq!(viewer.page_count(), 2);
    assert_eq!(viewer.layout_truncated_at_js(), Some(2));
}

#[test]
fn truncated_layout_is_a_byte_identical_prefix() {
    // Truncation must be a STOP, not a different layout: the pages that were
    // emitted are exactly the full run's first pages. (Fixture has no
    // PAGE/NUMPAGES fields and no relocation, so the skipped post-loop passes
    // cannot differ — that skip is declared at the yield-site comment.)
    let viewer = layout(perf_docx(&perf_float_free_body(48)));
    let full_dump = geometry_dump(&viewer);

    viewer.set_memory_budget(1_000.0);
    viewer
        .run_layout_native_with(|idx| {
            if idx == 1 {
                viewer.set_test_memory_bytes(2_000);
            }
        })
        .expect("truncated layout");
    assert_eq!(viewer.layout_truncated_at_js(), Some(2), "precondition");
    let truncated_dump = geometry_dump(&viewer);
    assert!(
        full_dump.starts_with(&truncated_dump) && !truncated_dump.is_empty(),
        "truncated pages must be a byte-identical prefix of the full layout"
    );
}

#[test]
fn truncation_resets_on_relayout_and_zero_budget_disables() {
    let viewer = layout(perf_docx(&perf_float_free_body(48)));
    let full = viewer.page_count();

    // Trip it once…
    viewer.set_memory_budget(1_000.0);
    viewer.set_test_memory_bytes(2_000);
    viewer.run_layout_native().expect("truncated layout");
    assert!(viewer.layout_truncated_at_js().is_some(), "precondition: tripped");

    // …then a budget of 0 disables the check even under pressure, and the
    // fresh run resets the stamp: no stale "showing first N pages" banner.
    viewer.set_memory_budget(0.0);
    let n = viewer.run_layout_native().expect("full layout");
    assert_eq!(n, full);
    assert_eq!(viewer.layout_truncated_at_js(), None);
}

// ─── complex-script shaping coverage — Arabic ───────────────────────────────
//
// Before this fixture the GLYPH stream defended ~28,400 glyphs of LATIN
// advances: 1 glyph in the whole corpus had a non-zero positioning offset and
// 2 runs a non-trivial cluster vector. Arabic (Noto Sans Arabic, the SAME
// pinned face the browser's on-demand Complex Script slot loads) brings the
// three axes Latin cannot: contextual joining (every letter reshapes by
// position), the lam-alef ligature (two chars → one glyph, a many-to-one
// cluster), and mark attachment (tashkeel diacritics carry non-zero x/y
// offsets). A shaping regression on any of those axes now reds a golden
// instead of sailing through.
fn oracle_arabic_docx() -> Vec<u8> {
    // §17.3.2.26 cs slot + §17.3.1.6 bidi paragraph + §17.3.2.30 rtl runs.
    // Content chosen for shaping, not prose: "السَّلَامُ" carries shadda+fatha
    // stacks (mark offsets) AND a lam-alef inside "لَامُ"; the bare "لا" is the
    // canonical ligature; "OK" embeds an LTR word so the UAX#9 L2 reorder and
    // the shaping interact on one line.
    let rtl = |text: &str| {
        format!(
            r#"<w:r><w:rPr><w:rFonts w:cs="Noto Sans Arabic"/><w:rtl/></w:rPr><w:t xml:space="preserve">{text}</w:t></w:r>"#
        )
    };
    let body = format!(
        r#"<w:p><w:pPr><w:bidi/></w:pPr>{}{}{}</w:p>"#,
        rtl("السَّلَامُ عَلَيْكُمْ "),
        r("", "OK"),
        rtl(" لا شَيْءَ"),
    );
    perf_docx(&body)
}

#[test]
fn oracle_arabic_snapshot() {
    assert_oracle_with(
        "oracle-arabic",
        oracle_arabic_docx(),
        1,
        0x07357abe537ae984, // derived from the first run; content verified below
        &[("Noto Sans Arabic", "NotoSansArabic-Regular.ttf")],
    );
}

// ─── the last uncovered geometry axis — TWO-SEGMENT lines ───────────────────
//
// Census before this fixture: ZERO pinned lines had a second segment. Every
// float fixture wraps text on ONE side, so the multi-segment arm of the line
// model (and the `s…n… s…n…` geometry serialization, and the ITEM stream's
// S1 counter) had never once been exercised by a pin. A centred
// `wrapText="bothSides"` box splits each adjacent line into two segments.
fn oracle_both_sides_docx() -> Vec<u8> {
    // 1" box centred in the 6.5" column: content x ≈ [252px..348px] leaves
    // ~2.6" of text either side. Paragraph-relative (cursor-DEPENDENT) so the
    // The per-page fixpoint must stay silent (the no-collateral gate holds).
    const BOX_EMU: i64 = 914_400;
    const CENTER_EMU: i64 = 2_400_300; // (6.5" − 1")/2 · 914400
    let box_xml = format!(
        r#"<w:r><w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1"><wp:simplePos x="0" y="0"/><wp:positionH relativeFrom="column"><wp:posOffset>{CENTER_EMU}</wp:posOffset></wp:positionH><wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV><wp:extent cx="{BOX_EMU}" cy="{BOX_EMU}"/><wp:effectExtent l="0" t="0" r="0" b="0"/><wp:wrapSquare wrapText="bothSides"/><wp:docPr id="1" name="B"/><a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><wps:wsp><wps:cNvSpPr txBox="1"/><wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{BOX_EMU}" cy="{BOX_EMU}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr><wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">B.</w:t></w:r></w:p></w:txbxContent></wps:txbx><wps:bodyPr/></wps:wsp></a:graphicData></a:graphic></wp:anchor></w:drawing></w:r>"#
    );
    let body = format!(
        "<w:p>{box_xml}{}</w:p>",
        r("", &"both sides of the box carry text and every adjacent line \
            must split into two segments around it, exercising the second \
            segment arm end to end. ".repeat(4)),
    );
    perf_docx(&body)
}

#[test]
fn oracle_both_sides_snapshot() {
    assert_oracle("oracle-both-sides", oracle_both_sides_docx(), 1, 0xf6d8dc3b36fd94ba);
}

// ─── the memory census ───────────────────────────────────────────────────────
//
// MEASURE, THEN DECIDE. Three expensive decisions hang on these numbers:
// windowed retention (is `block_caches` or `pages` the whale?), text
// interning (how many bytes does Item.text/ShapedRun.text duplication waste?),
// and checkpoint-based eviction (are checkpoints actually small — median AND
// tail?). This test produces the numbers; it makes none of the
// decisions itself.
//
// Two instruments, cross-checking each other:
//  - the cfg(test) counting GLOBAL ALLOCATOR (lib.rs `alloc_census`) — exact
//    request-size totals, phase-bracketed (model / retained / transient peak);
//  - structure WALKERS below — attribute the retained side to text / glyphs /
//    struct overhead. Walkers count PAYLOAD (String::len, Vec len × elem), not
//    capacity or allocator slack, so walker ≤ allocator is expected; the gap
//    IS the slack estimate.
//
// Run ALONE (the allocator counters are process-global):
//   cargo test -p parasma-wasm --features sml,pml,emf memory_census \
//     -- --ignored --nocapture --test-threads=1

struct ItemBytes {
    total: usize,
    item_text: usize,
    shaped_text: usize,
    dup_text: usize,
    glyphs: usize,
}

fn item_bytes(it: &crate::wml::linebreak::Item) -> ItemBytes {
    use std::mem::size_of;
    let mut b = ItemBytes {
        total: size_of::<crate::wml::linebreak::Item>(),
        item_text: it.text.len(),
        shaped_text: 0,
        dup_text: 0,
        glyphs: 0,
    };
    b.total += it.text.len();
    if let Some(sh) = &it.shaped {
        b.shaped_text = sh.text.len();
        b.glyphs = sh.glyphs.len() * size_of::<crate::engine::fonts::ShapedGlyph>();
        b.total += b.shaped_text + b.glyphs;
        if sh.text == it.text {
            b.dup_text = sh.text.len();
        }
    }
    if let Some(tb) = &it.text_box {
        for l in &tb.lines {
            let lb = line_bytes(l);
            b.total += lb.total;
            b.item_text += lb.item_text;
            b.shaped_text += lb.shaped_text;
            b.dup_text += lb.dup_text;
            b.glyphs += lb.glyphs;
        }
    }
    // image/tab/field/math counted SHALLOW (inside size_of::<Item>'s options,
    // plus the MathBox box itself) — the census fixtures are text-heavy by
    // design, so deep math/image attribution is out of scope here.
    if it.math.is_some() {
        b.total += std::mem::size_of::<crate::wml::math::MathBox>();
    }
    b
}

fn line_bytes(l: &PagedLine) -> ItemBytes {
    use std::mem::size_of;
    let mut b = ItemBytes { total: size_of::<PagedLine>(), item_text: 0, shaped_text: 0, dup_text: 0, glyphs: 0 };
    b.total += l.bar_tabs.len() * size_of::<f64>();
    for seg in &l.segments {
        b.total += size_of::<crate::wml::linebreak::LineSegment>();
        for it in &seg.items {
            let ib = item_bytes(it);
            b.total += ib.total;
            b.item_text += ib.item_text;
            b.shaped_text += ib.shaped_text;
            b.dup_text += ib.dup_text;
            b.glyphs += ib.glyphs;
        }
    }
    b
}

fn page_bytes(p: &PageLayout) -> ItemBytes {
    use std::mem::size_of;
    let mut b = ItemBytes { total: size_of::<PageLayout>(), item_text: 0, shaped_text: 0, dup_text: 0, glyphs: 0 };
    let mut add = |x: ItemBytes, b: &mut ItemBytes| {
        b.total += x.total;
        b.item_text += x.item_text;
        b.shaped_text += x.shaped_text;
        b.dup_text += x.dup_text;
        b.glyphs += x.glyphs;
    };
    for el in &p.elements {
        match el {
            PageElement::Line(l) => add(line_bytes(l), &mut b),
            PageElement::TableRow(r) => {
                b.total += size_of::<PagedTableRow>();
                for c in &r.cells {
                    b.total += size_of::<PagedTableCell>();
                    for l in &c.lines {
                        add(line_bytes(l), &mut b);
                    }
                }
            }
        }
    }
    for l in p.header_lines.iter().chain(p.footer_lines.iter()) {
        add(line_bytes(l), &mut b);
    }
    for tb in &p.positioned_text_boxes {
        b.total += size_of::<PositionedTextBox>();
        for l in &tb.lines {
            add(line_bytes(l), &mut b);
        }
    }
    b.total += p.anchored_images.len() * size_of::<crate::wml::page::AnchoredImageLayout>();
    b.total += p.footnote_ids.len() * 8 + p.endnote_ids.len() * 8 + p.col_separator_lines.len() * 24;
    b
}

fn mb(x: usize) -> f64 {
    x as f64 / 1_048_576.0
}

#[test]
#[ignore] // process-global counters — run alone; see the header above
fn memory_census() {
    use crate::alloc_census::{reset_peak, CURRENT, PEAK};
    use std::sync::atomic::Ordering::Relaxed;
    println!("fixture       pages  model_MB  retained_MB  pages_MB(walk)  transient_peak_MB  text_MB  shaped_dup_MB  glyphs_MB  ckpt_med_KB  ckpt_max_KB  ckpt_total_MB");
    for (name, paras) in [("text-2k", 2000usize), ("text-10k", 10000), ("text-45k", 45000)] {
        let mut viewer = Viewer::new().expect("viewer");
        load_test_fonts(&mut viewer);
        let base = CURRENT.load(Relaxed);
        let body: String = (0..paras).map(|i| p("", &r("", &filler(i, 5)))).collect();
        let docx = perf_docx(&body);
        viewer.load_docx(docx).expect("load");
        let model = CURRENT.load(Relaxed).saturating_sub(base);
        viewer.force_capture.set(true); // checkpoint every page → real size distribution
        reset_peak();
        let before_layout = CURRENT.load(Relaxed);
        viewer.run_layout_native().expect("layout");
        let after = CURRENT.load(Relaxed);
        let peak = PEAK.load(Relaxed);
        let retained = after.saturating_sub(before_layout);
        let transient = peak.saturating_sub(after);

        let pages = viewer.pages.borrow();
        let mut pw = ItemBytes { total: 0, item_text: 0, shaped_text: 0, dup_text: 0, glyphs: 0 };
        for pg in pages.iter() {
            let b = page_bytes(pg);
            pw.total += b.total;
            pw.item_text += b.item_text;
            pw.shaped_text += b.shaped_text;
            pw.dup_text += b.dup_text;
            pw.glyphs += b.glyphs;
        }
        let n_pages = pages.len();
        drop(pages);

        let log = viewer.checkpoint_log.borrow();
        let mut sizes: Vec<usize> = log
            .iter()
            .map(|(_, ck)| {
                let mut t = std::mem::size_of_val(ck);
                t += page_bytes(&ck.current).total;
                t += ck.pending_floats.len() * std::mem::size_of::<crate::DeferredFloat>();
                t += ck.open_comment_ids.len() * 8;
                if let Some((els, _)) = &ck.carried_in {
                    for el in els {
                        if let PageElement::Line(l) = el {
                            t += line_bytes(l).total;
                        }
                    }
                }
                if let Some(res) = &ck.resume {
                    if let Some(rem) = &res.para_remainder {
                        for it in rem {
                            t += item_bytes(it).total;
                        }
                    }
                }
                t
            })
            .collect();
        sizes.sort_unstable();
        let (med, max, tot) = if sizes.is_empty() {
            (0, 0, 0usize)
        } else {
            (sizes[sizes.len() / 2], *sizes.last().unwrap(), sizes.iter().sum())
        };
        println!(
            "{name:<12} {n_pages:>6} {:>9.1} {:>12.1} {:>15.1} {:>18.1} {:>8.1} {:>14.1} {:>10.1} {:>12.1} {:>12.1} {:>13.2}",
            mb(model), mb(retained), mb(pw.total), mb(transient),
            mb(pw.item_text), mb(pw.dup_text), mb(pw.glyphs),
            med as f64 / 1024.0, max as f64 / 1024.0, mb(tot),
        );
    }
}

#[test]
#[ignore] // companion to memory_census: the fixed per-item cost, printed once
fn memory_census_type_sizes() {
    use std::mem::size_of;
    println!("size_of Item        = {:>4} B", size_of::<crate::wml::linebreak::Item>());
    println!("size_of RunStyle    = {:>4} B", size_of::<crate::engine::paint::RunStyle>());
    println!("size_of ShapedRun   = {:>4} B", size_of::<crate::engine::fonts::ShapedRun>());
    println!("size_of ShapedGlyph = {:>4} B", size_of::<crate::engine::fonts::ShapedGlyph>());
    println!("size_of PagedLine   = {:>4} B", size_of::<PagedLine>());
    println!("size_of PageLayout  = {:>4} B", size_of::<PageLayout>());
}
