//! Native tests for the PML renderer.
//!
//! Web-sys-free: builds a minimal `.pptx` package in-test with the `zip`
//! dev-dependency (no binary fixtures in git; bytes reproducible from this
//! source) and drives `PmlRenderer::from_pptx`. These tests verify-by-LOADING:
//! the synthetic deck parses through `Presentation::from_reader`'s hard
//! requirements (valid OPC zip + [Content_Types].xml + root officeDocument rel +
//! readable ppt/presentation.xml + p:sldIdLst/p:sldId@r:id), reports
//! `slide_count == 1`, the page box is the §19.2.1.39 4:3 size in px, AND the
//! core layout behaviour: the title placeholder (no slide a:xfrm) inherits the
//! LAYOUT ctrTitle box (§19.3.1.x), the body placeholder inherits its idx-keyed
//! box, the title run is bold/sized from `rPr`, and its `solidFill srgbClr`
//! resolves. A final test loads the fflate-DEFLATED generated fixture from disk
//! to prove the generator output is parser-valid end-to-end.
//!
//! The fixture uses a parser-minimal part set AND the fork writer's
//! proven-loadable master/layout templates (writer.rs:85/:113). The layout
//! carries title + body placeholders WITH a:xfrm so the placeholder
//! inheritance floor has geometry to inherit.

use std::io::{Cursor, Write};

use super::{shape, PmlRenderer};
use crate::engine::units::emu_to_px;

// ─── pptx builder ──────────────────────────────────────────────────────────

/// Zip the given parts into a stored (uncompressed, deterministic) .pptx blob.
fn build_pptx(parts: &[(&str, &str)]) -> Vec<u8> {
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
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
</Types>"#;

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#;

// p:presentation with one master + one slide in the id lists, plus p:sldSz
// (§19.2.1.39, 4:3 screen).
const PRESENTATION: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
  <p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst>
  <p:sldSz cx="9144000" cy="6858000" type="screen4x3"/>
  <p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"#;

const PRESENTATION_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
</Relationships>"#;

// Minimal master (writer.rs:85 template) — empty spTree + required clrMap + one
// layout in the id list.
const SLIDE_MASTER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
    </p:spTree>
  </p:cSld>
  <p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
  <p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
</p:sldMaster>"#;

const SLIDE_MASTER_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"#;

// Layout WITH title + body placeholders carrying a:xfrm — the geometry the
// placeholder inheritance floor resolves (§19.7.10 ph matching), so the
// fixture exercises the inheritance path.
const SLIDE_LAYOUT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="title" preserve="1">
  <p:cSld name="Title">
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="2" name="Title 1"/>
          <p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
          <p:nvPr><p:ph type="ctrTitle"/></p:nvPr>
        </p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="685800" y="457200"/><a:ext cx="7772400" cy="1143000"/></a:xfrm></p:spPr>
      </p:sp>
      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="3" name="Body 2"/>
          <p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
          <p:nvPr><p:ph type="body" idx="1"/></p:nvPr>
        </p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="685800" y="1828800"/><a:ext cx="7772400" cy="4351338"/></a:xfrm></p:spPr>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sldLayout>"#;

const SLIDE_LAYOUT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>"#;

// The test slide: a title placeholder (no a:xfrm — inherits from layout) with an
// explicit bold run, and a body placeholder with two bullet paragraphs.
const SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="2" name="Title 1"/>
          <p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
          <p:nvPr><p:ph type="ctrTitle"/></p:nvPr>
        </p:nvSpPr>
        <p:spPr/>
        <p:txBody>
          <a:bodyPr/><a:lstStyle/>
          <a:p><a:r><a:rPr lang="en-US" sz="4400" b="1"><a:solidFill><a:srgbClr val="1F2937"/></a:solidFill></a:rPr><a:t>Hello Title</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="3" name="Body 2"/>
          <p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
          <p:nvPr><p:ph type="body" idx="1"/></p:nvPr>
        </p:nvSpPr>
        <p:spPr/>
        <p:txBody>
          <a:bodyPr/><a:lstStyle/>
          <a:p><a:r><a:rPr lang="en-US"/><a:t>First bullet</a:t></a:r></a:p>
          <a:p><a:r><a:rPr lang="en-US"/><a:t>Second bullet</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

const SLIDE_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"#;

// Fidelity fixture: a slide exercising fills, colours, an outline, alignment,
// and char bullets. Three non-placeholder rects with
// distinct fills (one outlined), a centred title, and a body with a char-
// bulleted right-aligned coloured line. Mirrors the generated fills-colors.pptx.
const FC_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr>
        <p:spPr/>
        <p:txBody><a:bodyPr/><a:lstStyle/>
          <a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="en-US" sz="4000" b="1"><a:solidFill><a:srgbClr val="0F172A"/></a:solidFill></a:rPr><a:t>Fills and Colours</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="3" name="Body 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
        <p:spPr/>
        <p:txBody><a:bodyPr/><a:lstStyle/>
          <a:p><a:pPr><a:buChar char="&#8226;"/></a:pPr><a:r><a:rPr lang="en-US" sz="2000"/><a:t>Bulleted body line one</a:t></a:r></a:p>
          <a:p><a:pPr algn="r"><a:buChar char="&#8226;"/></a:pPr><a:r><a:rPr lang="en-US" sz="2000"><a:solidFill><a:srgbClr val="B91C1C"/></a:solidFill></a:rPr><a:t>Right-aligned red line</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="4" name="Blue Rect"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="685800" y="5181600"/><a:ext cx="2286000" cy="1143000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="2563EB"/></a:solidFill></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="en-US" sz="1400"><a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill></a:rPr><a:t>Blue</a:t></a:r></a:p></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="5" name="Green Outlined Rect"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="3429000" y="5181600"/><a:ext cx="2286000" cy="1143000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="16A34A"/></a:solidFill><a:ln w="38100"><a:solidFill><a:srgbClr val="14532D"/></a:solidFill></a:ln></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="en-US" sz="1400"><a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill></a:rPr><a:t>Green</a:t></a:r></a:p></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="6" name="Amber Rect"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="6172200" y="5181600"/><a:ext cx="2286000" cy="1143000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="F59E0B"/></a:solidFill></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="en-US" sz="1400"><a:solidFill><a:srgbClr val="0F172A"/></a:solidFill></a:rPr><a:t>Amber</a:t></a:r></a:p></p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

// Connector fixture: a slide carrying two `p:cxnSp` connection shapes — a
// solid horizontal `straightConnector1` (no arrowheads) and a dashed one with a
// `triangle` headEnd + `arrow` tailEnd (the common dashed-arrow
// form). Exercises the connector walk + dash + arrowhead geometry.
const CXN_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:cxnSp>
        <p:nvCxnSpPr><p:cNvPr id="10" name="Plain Connector"/><p:cNvCxnSpPr/><p:nvPr/></p:nvCxnSpPr>
        <p:spPr>
          <a:xfrm><a:off x="0" y="900000"/><a:ext cx="9144000" cy="0"/></a:xfrm>
          <a:prstGeom prst="line"><a:avLst/></a:prstGeom>
          <a:ln w="38100"><a:solidFill><a:srgbClr val="C00000"/></a:solidFill></a:ln>
        </p:spPr>
      </p:cxnSp>
      <p:cxnSp>
        <p:nvCxnSpPr><p:cNvPr id="11" name="Arrow Connector"/><p:cNvCxnSpPr/><p:nvPr/></p:nvCxnSpPr>
        <p:spPr>
          <a:xfrm><a:off x="100000" y="1800000"/><a:ext cx="8000000" cy="0"/></a:xfrm>
          <a:prstGeom prst="straightConnector1"><a:avLst/></a:prstGeom>
          <a:ln w="12700"><a:solidFill><a:srgbClr val="111111"/></a:solidFill><a:prstDash val="dash"/><a:headEnd type="triangle"/><a:tailEnd type="arrow" w="lg" len="lg"/></a:ln>
        </p:spPr>
      </p:cxnSp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

/// Build a one-slide deck whose slide is the [`CXN_SLIDE`] connector fixture.
fn connectors_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", CXN_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

// ─── Group-shape fixture ────────────────────────────────────────────────────

// A slide with ONE top-level `p:sp` plus ONE `p:grpSp` group (§19.3.1.6) that
// contains two child `p:sp` AND a NESTED `p:grpSp` with one further child. The
// group's xfrm scales 2× on both axes (ext 4M×2M over chExt 2M×1M) and
// translates by off (1M, 2M) over chOff (0,0). Each child's CHILD-space box must
// map to the absolute slide box the assertions pin (the flatten+transform path).
const GRP_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="TopLevel"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="100000" y="200000"/><a:ext cx="300000" cy="400000"/></a:xfrm>
          <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
          <a:solidFill><a:srgbClr val="111111"/></a:solidFill>
        </p:spPr>
        <p:txBody><a:bodyPr/><a:p><a:r><a:t>TopLevelText</a:t></a:r></a:p></p:txBody>
      </p:sp>
      <p:grpSp>
        <p:nvGrpSpPr><p:cNvPr id="3" name="Group"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
        <p:grpSpPr>
          <a:xfrm><a:off x="1000000" y="2000000"/><a:ext cx="4000000" cy="2000000"/><a:chOff x="0" y="0"/><a:chExt cx="2000000" cy="1000000"/></a:xfrm>
        </p:grpSpPr>
        <p:sp>
          <p:nvSpPr><p:cNvPr id="4" name="Child1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
          <p:spPr>
            <a:xfrm><a:off x="0" y="0"/><a:ext cx="500000" cy="500000"/></a:xfrm>
            <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
            <a:solidFill><a:srgbClr val="222222"/></a:solidFill>
          </p:spPr>
          <p:txBody><a:bodyPr/><a:p><a:r><a:t>Child1Text</a:t></a:r></a:p></p:txBody>
        </p:sp>
        <p:sp>
          <p:nvSpPr><p:cNvPr id="5" name="Child2"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
          <p:spPr>
            <a:xfrm><a:off x="1000000" y="500000"/><a:ext cx="1000000" cy="500000"/></a:xfrm>
            <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
            <a:solidFill><a:srgbClr val="333333"/></a:solidFill>
          </p:spPr>
          <p:txBody><a:bodyPr/><a:p><a:r><a:t>Child2Text</a:t></a:r></a:p></p:txBody>
        </p:sp>
        <p:grpSp>
          <p:nvGrpSpPr><p:cNvPr id="6" name="Nested"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
          <p:grpSpPr>
            <a:xfrm><a:off x="1000000" y="1000000"/><a:ext cx="1000000" cy="1000000"/><a:chOff x="0" y="0"/><a:chExt cx="500000" cy="500000"/></a:xfrm>
          </p:grpSpPr>
          <p:sp>
            <p:nvSpPr><p:cNvPr id="7" name="Nested1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
            <p:spPr>
              <a:xfrm><a:off x="0" y="0"/><a:ext cx="250000" cy="250000"/></a:xfrm>
              <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
              <a:solidFill><a:srgbClr val="444444"/></a:solidFill>
            </p:spPr>
            <p:txBody><a:bodyPr/><a:p><a:r><a:t>NestedText</a:t></a:r></a:p></p:txBody>
          </p:sp>
        </p:grpSp>
      </p:grpSp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

/// Build a one-slide deck whose slide is the [`GRP_SLIDE`] group-shape fixture.
fn groups_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", GRP_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

/// A minimal valid 1×1 red PNG (deterministic; produced by zlib-deflating a
/// single RGB pixel). Used by the picture fixture so the byte-extraction +
/// mime-detection path resolves a real `image/png` part.
const TINY_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2,
    0, 0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0, 0,
    3, 1, 1, 0, 201, 254, 146, 239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

// Content types for the picture deck: same as the base set plus a png Default
// extension so the media part is a recognized content type.
const CONTENT_TYPES_PIC: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
</Types>"#;

// A slide with one p:pic (§19.3.1.x): a blipFill referencing rId2 (the media
// part) with an explicit a:xfrm box. The slide rels map rId2 → the image part.
const PIC_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:pic>
        <p:nvPicPr><p:cNvPr id="4" name="Picture 3"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr>
        <p:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>
        <p:spPr><a:xfrm><a:off x="1828800" y="1371600"/><a:ext cx="3657600" cy="2743200"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
      </p:pic>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

const PIC_SLIDE_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>
</Relationships>"#;

/// Build the picture deck: the base master/layout/presentation plus a
/// slide carrying one `p:pic` and a real PNG media part. Mixes str + byte parts,
/// so it zips them directly (the str builder only takes UTF-8 content).
fn picture_pptx() -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let str_parts: &[(&str, &str)] = &[
        ("[Content_Types].xml", CONTENT_TYPES_PIC),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", PIC_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", PIC_SLIDE_RELS),
    ];
    for (name, content) in str_parts {
        zw.start_file(*name, opts).expect("zip start_file");
        zw.write_all(content.as_bytes()).expect("zip write");
    }
    zw.start_file("ppt/media/image1.png", opts).expect("zip start_file png");
    zw.write_all(TINY_PNG).expect("zip write png");
    zw.finish().expect("zip finish").into_inner()
}

#[test]
fn picture_object_hit_key_round_trips_through_the_image_store() {
    // The image-copy contract: the key pml_hit_test_object returns
    // MUST be a key the engine's image store (image_refs → the TS #imageBytes
    // retention) actually holds — else objectInfo returns null and the picture
    // copy silently no-ops. Guards the same silent cross-boundary seam class as
    // the WML image-paste bug (image ids dropped once the bytes are cleared).
    let mut renderer = PmlRenderer::from_pptx(picture_pptx()).expect("picture pptx loads");
    let fc = crate::engine::fonts::FontCache::new();
    let page = renderer.build_pages_for_test(&fc).into_iter().next().expect("one page");
    let pic = page.pictures.first().expect("a picture on the slide").clone();
    let hit = crate::pml::select::hit_test_object(&page, pic.x + pic.w / 2.0, pic.y + pic.h / 2.0)
        .expect("a picture object hit at the box centre");
    let keys: std::collections::HashSet<String> =
        renderer.take_images().into_iter().map(|(k, _)| k).collect();
    assert!(
        keys.contains(&hit.key),
        "object-hit key {:?} must be a retained image key {:?}",
        hit.key,
        keys
    );
}

/// Build the fills/colours/outline/alignment/bullets deck.
fn fills_colors_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", FC_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

/// Build the parser-minimal one-slide deck.
pub(crate) fn minimal_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

// ─── tests ─────────────────────────────────────────────────────────────────

#[test]
fn loads_minimal_pptx_and_counts_one_slide() {
    let renderer = PmlRenderer::from_pptx(minimal_pptx()).expect("minimal pptx must load");
    assert_eq!(
        renderer.slide_count(),
        1,
        "the deck has exactly one slide (p:sldIdLst/p:sldId)"
    );
}

#[test]
fn page_box_is_default_4x3_in_px() {
    // `p:sldSz` self-parses to the §19.2.1.39 default 9144000 × 6858000 EMU → px. The
    // page box is what the Viewer mirrors into the shared page_size.
    let renderer = PmlRenderer::from_pptx(minimal_pptx()).expect("minimal pptx must load");
    let (w, h) = renderer.page_box();
    assert_eq!(w, emu_to_px(9_144_000));
    assert_eq!(h, emu_to_px(6_858_000));
}

#[test]
fn title_placeholder_inherits_layout_xfrm() {
    // The slide's title placeholder carries NO a:xfrm; it must inherit the
    // layout's ctrTitle box (off 685800,457200 EMU → px). This pins the
    // layout-only inheritance floor (§19.3.1.x ph matching).
    use crate::engine::fonts::FontCache;
    use crate::engine::units::emu_to_px;
    let renderer = PmlRenderer::from_pptx(minimal_pptx()).expect("minimal pptx must load");
    let fc = FontCache::new();
    let pages = renderer.build_pages_for_test(&fc);
    assert_eq!(pages.len(), 1);
    let page = &pages[0];

    // Find the title shape by its run text.
    let title = page
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "Hello Title"))
        .expect("title shape with 'Hello Title' run");
    assert_eq!(title.x, emu_to_px(685_800), "title inherits layout off x");
    assert_eq!(title.y, emu_to_px(457_200), "title inherits layout off y");
    assert_eq!(title.w, emu_to_px(7_772_400), "title inherits layout ext cx");
    assert_eq!(title.h, emu_to_px(1_143_000), "title inherits layout ext cy");

    // The title run is bold and sized from rPr sz=4400 → 44pt.
    let run = title
        .paragraphs
        .iter()
        .flat_map(|p| &p.runs)
        .find(|r| r.text == "Hello Title")
        .unwrap();
    assert!(run.bold, "title run is bold (rPr b=1)");
    assert_eq!(run.font_pt, 44.0, "title run size from rPr sz=4400");
}

#[test]
fn body_placeholder_inherits_idx_keyed_xfrm() {
    // The body placeholder (ph type=body idx=1) carries NO a:xfrm; it inherits
    // the layout body box (off 685800,1828800). Exercises idx-keyed inheritance.
    use crate::engine::fonts::FontCache;
    use crate::engine::units::emu_to_px;
    let renderer = PmlRenderer::from_pptx(minimal_pptx()).expect("minimal pptx must load");
    let fc = FontCache::new();
    let pages = renderer.build_pages_for_test(&fc);
    let page = &pages[0];

    let body = page
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "First bullet"))
        .expect("body shape with 'First bullet' run");
    assert_eq!(body.x, emu_to_px(685_800), "body inherits layout off x");
    assert_eq!(body.y, emu_to_px(1_828_800), "body inherits layout off y");
    assert_eq!(body.w, emu_to_px(7_772_400));
    assert_eq!(body.h, emu_to_px(4_351_338));

    // Two body paragraphs, each one run; rPr-less runs get the body default pt.
    assert_eq!(body.paragraphs.len(), 2, "two bullet paragraphs");
    let texts: Vec<&str> = body
        .paragraphs
        .iter()
        .flat_map(|p| &p.runs)
        .map(|r| r.text.as_str())
        .collect();
    assert_eq!(texts, vec!["First bullet", "Second bullet"]);
}

#[test]
fn title_run_solid_fill_color_resolves() {
    // rPr/solidFill/srgbClr val="1F2937" → "#1f2937" (decoded byte triple
    // re-encoded as a CSS hex string — the dml-fills+dml-colors path).
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(minimal_pptx()).expect("minimal pptx must load");
    let fc = FontCache::new();
    let pages = renderer.build_pages_for_test(&fc);
    let run = pages[0]
        .shapes
        .iter()
        .flat_map(|s| &s.paragraphs)
        .flat_map(|p| &p.runs)
        .find(|r| r.text == "Hello Title")
        .unwrap();
    assert_eq!(run.color, "#1f2937", "explicit srgbClr run colour");
}

#[test]
fn group_shapes_flatten_to_mapped_absolute_boxes() {
    // A `p:grpSp` group scaling 2× (ext 4M×2M / chExt 2M×1M) and translating
    // by off (1M,2M) over chOff (0,0). Each descendant `p:sp`'s CHILD-space
    // `a:xfrm` must map to the absolute slide box below; the top-level sp is
    // untouched; a NESTED group composes the two transforms (net 4× scale).
    use crate::engine::fonts::FontCache;
    use crate::engine::units::emu_to_px;
    let renderer = PmlRenderer::from_pptx(groups_pptx()).expect("groups pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let page = &pages[0];

    let by_text = |t: &str| {
        page.shapes
            .iter()
            .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == t))
            .unwrap_or_else(|| panic!("shape with run '{t}' must be present"))
    };

    // Top-level shape: identity transform — its own box, unchanged.
    let top = by_text("TopLevelText");
    assert_eq!(top.x, emu_to_px(100_000), "top-level sp keeps own off x");
    assert_eq!(top.y, emu_to_px(200_000), "top-level sp keeps own off y");
    assert_eq!(top.w, emu_to_px(300_000));
    assert_eq!(top.h, emu_to_px(400_000));

    // Child1 at child (0,0): parent = off + 0 = (1M, 2M); ext 500k×500k × 2 = 1M.
    let c1 = by_text("Child1Text");
    assert_eq!(c1.x, emu_to_px(1_000_000), "child1 maps off x = off.x");
    assert_eq!(c1.y, emu_to_px(2_000_000), "child1 maps off y = off.y");
    assert_eq!(c1.w, emu_to_px(1_000_000), "child1 ext scaled 2×");
    assert_eq!(c1.h, emu_to_px(1_000_000));

    // Child2 at child (1M, 500k): parent = (2·1M+1M, 2·500k+2M) = (3M, 3M);
    // ext (1M,500k)×2 = (2M, 1M).
    let c2 = by_text("Child2Text");
    assert_eq!(c2.x, emu_to_px(3_000_000), "child2 maps off x");
    assert_eq!(c2.y, emu_to_px(3_000_000), "child2 maps off y");
    assert_eq!(c2.w, emu_to_px(2_000_000));
    assert_eq!(c2.h, emu_to_px(1_000_000));

    // Nested child: inner group (2× over (1M,1M) in OUTER-CHILD space) composes
    // with the outer (2×, +off) → net 4× scale + composed translate. Child at
    // (0,0) → (3M, 4M); ext 250k×4 = 1M.
    let n1 = by_text("NestedText");
    assert_eq!(n1.x, emu_to_px(3_000_000), "nested child composed off x");
    assert_eq!(n1.y, emu_to_px(4_000_000), "nested child composed off y");
    assert_eq!(n1.w, emu_to_px(1_000_000), "nested child net 4× scale");
    assert_eq!(n1.h, emu_to_px(1_000_000));

    // All four shapes present (no group descendants dropped).
    assert_eq!(page.shapes.len(), 4, "1 top-level + 2 group + 1 nested = 4");
}

#[test]
fn font_families_reports_no_explicit_latin() {
    // The fixture runs set no rPr/latin typeface, so the deck reports none; the
    // JS font plan then loads only the engine default (mirrors SML's contract).
    let renderer = PmlRenderer::from_pptx(minimal_pptx()).expect("minimal pptx must load");
    assert!(renderer.font_families().is_empty());
}

#[test]
fn garbage_bytes_fail_to_parse() {
    // A non-pptx blob must fail the fork parse rather than load. We drive the
    // fork's `Presentation::from_reader` directly (NOT `from_pptx`, whose
    // error path constructs a `JsValue` that panics off-wasm32 — that wrapper is
    // covered on the wasm target) and assert it errors.
    use std::io::Cursor;
    let res = ooxml_pml::Presentation::from_reader(Cursor::new(vec![1u8, 2, 3, 4, 5]));
    assert!(res.is_err(), "garbage bytes must fail the pptx parse");
}

#[test]
fn generated_fixture_loads_from_disk() {
    // Verify the fflate-DEFLATED generated fixture (not the in-test STORED zip)
    // loads + resolves the same title geometry, proving the generator output is
    // parser-valid end-to-end. Path is relative to the crate manifest dir.
    use crate::engine::fonts::FontCache;
    use crate::engine::units::emu_to_px;
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/ppt/title-body.pptx"
    );
    // The fixture is gitignored + script-generated; skip (don't fail) when it's
    // absent so a clean CI checkout without `generate-test-fixtures` stays green.
    let Ok(bytes) = std::fs::read(path) else {
        eprintln!("skipping: generated fixture absent (run generate-test-fixtures.ts)");
        return;
    };
    let renderer = PmlRenderer::from_pptx(bytes).expect("generated pptx must load");
    assert_eq!(renderer.slide_count(), 1);
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let title = pages[0]
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "Hello Title"))
        .expect("title shape");
    assert_eq!(title.x, emu_to_px(685_800));
    assert_eq!(title.y, emu_to_px(457_200));
    // The non-placeholder boxed-caption rect uses its OWN xfrm + a solid fill.
    let boxed = pages[0]
        .shapes
        .iter()
        .find(|s| s.solid_fill_hex() == Some("#2563eb"))
        .expect("boxed rect with own fill");
    assert_eq!(boxed.x, emu_to_px(685_800));
    assert_eq!(boxed.y, emu_to_px(6_096_000));
}

// ─── Fills, colours, outline, alignment, bullets ────────────────────────────

#[test]
fn fills_colors_distinct_shape_fills_resolve() {
    // The three non-placeholder rects carry distinct srgbClr solid fills; each
    // resolves to its own CSS hex (the dml-fills + dml-colors path).
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(fills_colors_pptx()).expect("fills-colors pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let fills: std::collections::HashSet<&str> = pages[0]
        .shapes
        .iter()
        .filter_map(|s| s.solid_fill_hex())
        .collect();
    assert!(fills.contains("#2563eb"), "blue rect fill");
    assert!(fills.contains("#16a34a"), "green rect fill");
    assert!(fills.contains("#f59e0b"), "amber rect fill");
}

#[test]
fn fills_colors_shape_outline_resolves() {
    // The green rect carries an a:ln solidFill srgbClr 14532D, width 38100 EMU.
    // Only that one rect has an outline; the others have None.
    use crate::engine::fonts::FontCache;
    use crate::engine::units::emu_to_px;
    let renderer = PmlRenderer::from_pptx(fills_colors_pptx()).expect("fills-colors pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let outlined: Vec<_> = pages[0]
        .shapes
        .iter()
        .filter(|s| s.outline.is_some())
        .collect();
    assert_eq!(outlined.len(), 1, "only the green rect is outlined");
    let out = outlined[0].outline.as_ref().unwrap();
    assert_eq!(out.color, "#14532d", "outline colour from ln/solidFill/srgbClr");
    assert_eq!(out.width_px, emu_to_px(38_100), "outline width ln@w EMU → px");
    // The outlined rect is also the green-filled one.
    assert_eq!(outlined[0].solid_fill_hex(), Some("#16a34a"));
}

#[test]
fn fills_colors_title_is_center_aligned() {
    // The title paragraph has pPr@algn="ctr".
    use crate::engine::fonts::FontCache;
    use crate::pml::shape::ParaAlign;
    let renderer = PmlRenderer::from_pptx(fills_colors_pptx()).expect("fills-colors pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let title_para = pages[0]
        .shapes
        .iter()
        .flat_map(|s| &s.paragraphs)
        .find(|p| p.runs.iter().any(|r| r.text == "Fills and Colours"))
        .expect("title paragraph");
    assert_eq!(title_para.align, ParaAlign::Center);
}

#[test]
fn fills_colors_body_bullets_and_alignment() {
    // Body line one: char bullet, left-aligned. Line two: char bullet, right-
    // aligned, red run colour.
    use crate::engine::fonts::FontCache;
    use crate::pml::shape::ParaAlign;
    let renderer = PmlRenderer::from_pptx(fills_colors_pptx()).expect("fills-colors pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let body = pages[0]
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text.contains("Bulleted")))
        .expect("body shape");
    assert_eq!(body.paragraphs.len(), 2, "two bulleted body lines");

    let p0 = &body.paragraphs[0];
    assert_eq!(p0.bullet.as_deref(), Some("\u{2022}"), "char bullet •");
    assert_eq!(p0.align, ParaAlign::Left, "first line left-aligned (no algn)");

    let p1 = &body.paragraphs[1];
    assert_eq!(p1.bullet.as_deref(), Some("\u{2022}"));
    assert_eq!(p1.align, ParaAlign::Right, "second line right-aligned");
    assert_eq!(p1.runs[0].color, "#b91c1c", "right line red run colour");
}

#[test]
fn fills_colors_font_families_empty() {
    // No explicit latin typeface anywhere → JS loads only the engine default.
    let renderer = PmlRenderer::from_pptx(fills_colors_pptx()).expect("fills-colors pptx loads");
    assert!(renderer.font_families().is_empty());
}

// ─── bodyPr@wrap text wrapping (§21.1.2.1.1) ────────────────────────────────

/// Build a single-run paragraph for the wrap-breaker tests. No font is loaded in
/// these tests, so measurement uses the deterministic per-char estimate in
/// `measure_segment_advance`: ~1.0 em/char for East-Asian glyphs, ~0.5 em/char
/// for Latin. With `font_pt = 10`, one em (`pt_to_px(10)`) ≈ 13.333 px.
fn wrap_run(text: &str, font_pt: f64) -> shape::SlideRun {
    shape::SlideRun {
        text: text.to_string(),
        font_pt,
        bold: false,
        italic: false,
        family: "Test".into(),
        family_east_asia: "Test".into(),
        family_complex: "Test".into(),
        color: "#000000".into(),
    }
}

fn wrap_para(text: &str, font_pt: f64) -> shape::SlideParagraph {
    shape::SlideParagraph {
        runs: vec![wrap_run(text, font_pt)],
        align: shape::ParaAlign::Left,
        bullet: None,
        indent_first_px: 0.0,
        indent_rest_px: 0.0,
    }
}

#[test]
fn wrap_cjk_run_breaks_into_multiple_lines() {
    // A long CJK run in a narrow box wraps (CJK breaks between any two glyphs).
    // 6 ideographs at ~1 em each (~13.33 px) in a 30 px box → 2 px/line: 3 lines.
    use crate::engine::fonts::FontCache;
    let fc = FontCache::new();
    let para = wrap_para("文字内容测试", 10.0);
    let avail = 30.0_f64;
    let lines = super::layout_paragraph_lines(&para, avail, shape::WrapMode::Square, &fc, &crate::engine::fonts::FaceCache::new());
    assert!(lines.len() > 1, "narrow CJK caption must wrap to >1 line, got {}", lines.len());
    // Every glyph is preserved across the wrapped lines, in order.
    let joined: String =
        lines.iter().flat_map(|l| l.runs.iter()).map(|r| r.text.clone()).collect();
    assert_eq!(joined, "文字内容测试", "no glyphs lost/reordered by wrapping");
    // The wrap-aware block height is ~ n_lines * the single line height.
    let line_h = shape::run_line_metrics(&wrap_run("文", 10.0), &fc).1;
    let block_h = super::table_block_height(&[para], avail, shape::WrapMode::Square, &fc);
    let expected = lines.len() as f64 * line_h;
    assert!((block_h - expected).abs() < 1e-6, "block_h {block_h} ≈ n_lines*line_h {expected}");
}

#[test]
fn wrapped_paragraph_preserves_text_for_search_match_count() {
    // The PML active-match index numbers occurrences from the RAW paragraph runs
    // (search_pml_slide / count_paragraph_matches), while the paint pass matches
    // the LAID-OUT sub-runs after Square wrap. If wrapping changed the text (e.g.
    // dropped a soft-break space) the two match counts would diverge and the
    // active emphasis would light the wrong occurrence. Pin that the laid-out
    // sub-runs concatenate back to the EXACT original text — so the count is
    // identical for ANY needle, including multi-word ones straddling a wrap.
    use crate::engine::fonts::FontCache;
    let fc = FontCache::new();
    let text = "foo bar foo baz foo qux foo";
    let para = wrap_para(text, 10.0);
    let lines = super::layout_paragraph_lines(&para, 60.0, shape::WrapMode::Square, &fc, &crate::engine::fonts::FaceCache::new());
    assert!(lines.len() >= 2, "must wrap to >1 line, got {}", lines.len());
    let painted: String =
        lines.iter().flat_map(|l| l.runs.iter()).map(|r| r.text.as_str()).collect();
    assert_eq!(painted, text, "Square-wrap must preserve the exact text (active-index invariant)");
}

#[test]
fn wrap_none_keeps_single_line() {
    // wrap=None lays the whole CJK run on ONE line regardless of box width.
    use crate::engine::fonts::FontCache;
    let fc = FontCache::new();
    let para = wrap_para("文字内容测试", 10.0);
    let lines = super::layout_paragraph_lines(&para, 30.0, shape::WrapMode::None, &fc, &crate::engine::fonts::FaceCache::new());
    assert_eq!(lines.len(), 1, "wrap=none → exactly one line");
    assert_eq!(lines[0].runs.iter().map(|r| r.text.clone()).collect::<String>(), "文字内容测试");
}

#[test]
fn wrap_latin_breaks_at_space_not_mid_word() {
    // A multi-word Latin run wraps at a SPACE, never mid-word. "alpha beta gamma"
    // at ~0.5 em/char (~6.67 px) — "alpha " is ~6 chars ≈ 40 px. A 60 px box fits
    // "alpha " but not "alpha beta", so the break lands before "beta".
    use crate::engine::fonts::FontCache;
    let fc = FontCache::new();
    let para = wrap_para("alpha beta gamma", 10.0);
    let lines = super::layout_paragraph_lines(&para, 60.0, shape::WrapMode::Square, &fc, &crate::engine::fonts::FaceCache::new());
    assert!(lines.len() > 1, "multi-word Latin must wrap, got {}", lines.len());
    // No line's text splits a word: each whitespace-trimmed line is whole words.
    for line in &lines {
        let txt: String = line.runs.iter().map(|r| r.text.clone()).collect();
        let t = txt.trim();
        assert!(
            t.is_empty() || ["alpha", "beta", "gamma"].iter().any(|w| t.split_whitespace().all(|p| ["alpha", "beta", "gamma"].contains(&p))),
            "line '{t}' must be whole words, never a mid-word split"
        );
    }
    // First line starts the document; it must contain "alpha" (the first word).
    let first: String = lines[0].runs.iter().map(|r| r.text.clone()).collect();
    assert!(first.contains("alpha"), "first wrapped line starts with 'alpha'");
    assert!(!first.contains("gamma"), "'gamma' must NOT fit on the first 60px line");
}

#[test]
fn wrap_block_is_vertically_centered_for_ctr_anchor() {
    // For anchor=ctr the wrapped block centres in the box: the first baseline sits
    // below the box top by ~ (h - block_h)/2 + ascent. Mirror the painter's math.
    use crate::engine::fonts::FontCache;
    let fc = FontCache::new();
    let para = wrap_para("文字内容测试", 10.0);
    let avail = 30.0_f64;
    let box_h = 200.0_f64;
    let block_h = super::table_block_height(&[para.clone()], avail, shape::WrapMode::Square, &fc);
    let lines = super::layout_paragraph_lines(&para, avail, shape::WrapMode::Square, &fc, &crate::engine::fonts::FaceCache::new());
    let ascent = lines[0].ascent;
    // The painter offsets text_y by (box_h - block_h)/2 then adds the first
    // line's ascent to reach the baseline.
    let text_y = ((box_h - block_h) / 2.0).max(0.0);
    let first_baseline = text_y + ascent;
    let expected = (box_h - block_h) / 2.0 + ascent;
    assert!(block_h < box_h, "multi-line block fits within the tall box");
    assert!((first_baseline - expected).abs() < 1e-6, "first baseline centred");
}

// ─── Slide pictures ─────────────────────────────────────────────────────────

#[test]
fn picture_xfrm_box_and_namespaced_key() {
    // The slide's single p:pic carries an explicit a:xfrm (off 1828800,1371600;
    // ext 3657600×2743200 EMU). Pin the EMU→px draw box AND the slide-namespaced
    // image key (`pml::0::rId2`) the renderer stores its bytes under.
    use crate::engine::fonts::FontCache;
    use crate::engine::units::emu_to_px;
    let renderer = PmlRenderer::from_pptx(picture_pptx()).expect("picture pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].pictures.len(), 1, "one p:pic on the slide");
    let pic = &pages[0].pictures[0];
    assert_eq!(pic.x, emu_to_px(1_828_800), "picture off x EMU → px");
    assert_eq!(pic.y, emu_to_px(1_371_600), "picture off y EMU → px");
    assert_eq!(pic.w, emu_to_px(3_657_600), "picture ext cx EMU → px");
    assert_eq!(pic.h, emu_to_px(2_743_200), "picture ext cy EMU → px");
    assert_eq!(pic.image_key, "pml::0::rId2", "slide-namespaced image key");
}

#[test]
fn picture_media_bytes_extracted_with_png_mime() {
    // The renderer collects the picture's media bytes at load (OPC self-parse of
    // the slide rels → embed rel → media part), keyed by the same namespaced key
    // the draw item carries. Mime is derived from the .png extension.
    let mut renderer = PmlRenderer::from_pptx(picture_pptx()).expect("picture pptx loads");
    let images = renderer.take_images();
    assert_eq!(images.len(), 1, "one picture's bytes collected");
    let (key, img) = &images[0];
    assert_eq!(key, "pml::0::rId2", "bytes keyed by the namespaced image key");
    assert_eq!(img.mime, "image/png", "mime from .png extension");
    assert_eq!(img.data, TINY_PNG, "the exact media part bytes");
    // take_images drains, so a second call is empty (Viewer drains once at load).
    assert!(renderer.take_images().is_empty(), "images drained after take");
}

#[test]
fn picture_without_xfrm_or_embed_is_dropped() {
    // A p:pic with no a:xfrm (no box to draw into) contributes no draw item —
    // honest graceful drop, no panic. Build a slide whose pic omits the xfrm.
    use crate::engine::fonts::FontCache;
    const NOXFRM_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree>
    <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
    <p:pic><p:nvPicPr><p:cNvPr id="4" name="P"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr>
      <p:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>
      <p:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>
  </p:spTree></p:cSld>
</p:sld>"#;
    let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let str_parts: &[(&str, &str)] = &[
        ("[Content_Types].xml", CONTENT_TYPES_PIC),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", NOXFRM_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", PIC_SLIDE_RELS),
    ];
    for (name, content) in str_parts {
        zw.start_file(*name, opts).unwrap();
        zw.write_all(content.as_bytes()).unwrap();
    }
    zw.start_file("ppt/media/image1.png", opts).unwrap();
    zw.write_all(TINY_PNG).unwrap();
    let bytes = zw.finish().unwrap().into_inner();

    let renderer = PmlRenderer::from_pptx(bytes).expect("noxfrm picture pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    assert!(pages[0].pictures.is_empty(), "pic with no xfrm box is dropped");
}

// ─── Slide tables (DML a:tbl in a p:graphicFrame) ───────────────────────────

// A slide with one p:graphicFrame holding a 2×2 DML table (§21.1.3). The frame
// xfrm positions the table at (914400, 1828800); the grid has two 2438400-EMU
// columns and two 685800-EMU rows. The header row's first cell carries a solid
// fill (DDEEFF) + four solid borders (333333, w=12700) + bold text; the other
// cells carry plain text. Exercises the rail prefix sums, the cell box geometry,
// the per-cell fill/borders, and the cell-text reuse of the slide text painter.
const TABLE_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:graphicFrame>
        <p:nvGraphicFramePr><p:cNvPr id="5" name="Table 4"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>
        <p:xfrm><a:off x="914400" y="1828800"/><a:ext cx="4876800" cy="1371600"/></p:xfrm>
        <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
          <a:tbl>
            <a:tblPr firstRow="1" bandRow="1"/>
            <a:tblGrid>
              <a:gridCol w="2438400"/>
              <a:gridCol w="2438400"/>
            </a:tblGrid>
            <a:tr h="685800">
              <a:tc>
                <a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1800" b="1"/><a:t>Name</a:t></a:r></a:p></a:txBody>
                <a:tcPr>
                  <a:lnL w="12700"><a:solidFill><a:srgbClr val="333333"/></a:solidFill></a:lnL>
                  <a:lnR w="12700"><a:solidFill><a:srgbClr val="333333"/></a:solidFill></a:lnR>
                  <a:lnT w="12700"><a:solidFill><a:srgbClr val="333333"/></a:solidFill></a:lnT>
                  <a:lnB w="12700"><a:solidFill><a:srgbClr val="333333"/></a:solidFill></a:lnB>
                  <a:solidFill><a:srgbClr val="DDEEFF"/></a:solidFill>
                </a:tcPr>
              </a:tc>
              <a:tc>
                <a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1800" b="1"/><a:t>Score</a:t></a:r></a:p></a:txBody>
                <a:tcPr/>
              </a:tc>
            </a:tr>
            <a:tr h="685800">
              <a:tc>
                <a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1600"/><a:t>Alice</a:t></a:r></a:p></a:txBody>
                <a:tcPr/>
              </a:tc>
              <a:tc>
                <a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1600"/><a:t>42</a:t></a:r></a:p></a:txBody>
                <a:tcPr/>
              </a:tc>
            </a:tr>
          </a:tbl>
        </a:graphicData></a:graphic>
      </p:graphicFrame>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

/// Build the table deck: base master/layout/presentation + the table
/// slide. The slide rels are the base layout-only rels (no media/charts).
fn table_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", TABLE_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

#[test]
fn table_grid_rails_and_cell_boxes() {
    // The graphicFrame xfrm origin is (914400, 1828800); two 2438400-EMU columns,
    // two 685800-EMU rows. Pin the rail prefix sums via the resolved cell boxes.
    use crate::engine::fonts::FontCache;
    use crate::engine::units::emu_to_px;
    let renderer = PmlRenderer::from_pptx(table_pptx()).expect("table pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].tables.len(), 1, "one table on the slide");
    let cells = &pages[0].tables[0].cells;
    assert_eq!(cells.len(), 4, "2×2 un-merged grid → 4 cell boxes");

    // Cell (0,0): origin = frame off, w = col0, h = row0.
    let c00 = &cells[0];
    assert_eq!(c00.x, emu_to_px(914_400), "cell(0,0) x = frame off x");
    assert_eq!(c00.y, emu_to_px(1_828_800), "cell(0,0) y = frame off y");
    assert_eq!(c00.w, emu_to_px(2_438_400), "cell(0,0) w = gridCol0 @w");
    assert_eq!(c00.h, emu_to_px(685_800), "cell(0,0) h = tr0 @h");

    // Cell (1,1) (4th, row-major): x = off+col0, y = off+row0.
    let c11 = &cells[3];
    assert_eq!(c11.x, emu_to_px(914_400 + 2_438_400), "cell(1,1) x = off+col0 rail");
    assert_eq!(c11.y, emu_to_px(1_828_800 + 685_800), "cell(1,1) y = off+row0 rail");
}

#[test]
fn table_cell_fill_borders_and_text() {
    // The header cell (0,0) carries a DDEEFF solid fill, four 333333 borders, and
    // bold "Name" text; the other cells carry plain text. Pin the fill, borders,
    // and the reused cell-text painter (bold flag from rPr b=1).
    use crate::engine::fonts::FontCache;
    use crate::engine::units::emu_to_px;
    let renderer = PmlRenderer::from_pptx(table_pptx()).expect("table pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let cells = &pages[0].tables[0].cells;

    let header = &cells[0];
    assert_eq!(header.fill_hex.as_deref(), Some("#ddeeff"), "header cell solid fill");
    let b = header.border_t.as_ref().expect("header top border");
    assert_eq!(b.color, "#333333", "border colour from lnT solidFill srgbClr");
    assert_eq!(b.width_px, emu_to_px(12_700), "border width lnT@w EMU → px");
    assert!(header.border_l.is_some() && header.border_r.is_some() && header.border_b.is_some());

    // Cell text reuses the slide text painter: one paragraph, one bold run.
    assert_eq!(header.paragraphs.len(), 1, "header has one paragraph");
    let run = &header.paragraphs[0].runs[0];
    assert_eq!(run.text, "Name");
    assert!(run.bold, "header run bold (rPr b=1)");
    assert_eq!(run.font_pt, 18.0, "header run size rPr sz=1800");

    // A non-header cell has no fill / no borders but does carry text.
    let plain = &cells[1];
    assert!(plain.fill_hex.is_none(), "Score cell no explicit fill");
    assert!(plain.border_t.is_none(), "Score cell no explicit border");
    assert_eq!(plain.paragraphs[0].runs[0].text, "Score");

    // Body cells carry the data text.
    let texts: Vec<&str> = cells
        .iter()
        .flat_map(|c| &c.paragraphs)
        .flat_map(|p| &p.runs)
        .map(|r| r.text.as_str())
        .collect();
    assert_eq!(texts, vec!["Name", "Score", "Alice", "42"]);
}

#[test]
fn dashed_cell_border_carries_the_dash_array() {
    // A `tcPr/lnB/prstDash val="dash"` cell border carries the canonical
    // dash array on its ShapeOutline (§20.1.10.48) — the SAME `dash` field the
    // shape-outline/connector painters feed `apply_line_dash`, which
    // `paint_cell_edges` applies (an earlier version captured it but painted solid).
    const DASHED_TABLE_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree>
    <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
    <p:graphicFrame>
      <p:nvGraphicFramePr><p:cNvPr id="5" name="Dashed Table"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>
      <p:xfrm><a:off x="914400" y="1828800"/><a:ext cx="2438400" cy="685800"/></p:xfrm>
      <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
        <a:tbl>
          <a:tblGrid><a:gridCol w="2438400"/></a:tblGrid>
          <a:tr h="685800">
            <a:tc>
              <a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>D</a:t></a:r></a:p></a:txBody>
              <a:tcPr>
                <a:lnB w="12700"><a:solidFill><a:srgbClr val="333333"/></a:solidFill><a:prstDash val="dash"/></a:lnB>
              </a:tcPr>
            </a:tc>
          </a:tr>
        </a:tbl>
      </a:graphicData></a:graphic>
    </p:graphicFrame>
  </p:spTree></p:cSld>
</p:sld>"#;
    use crate::engine::fonts::FontCache;
    let bytes = build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", DASHED_TABLE_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ]);
    let renderer = PmlRenderer::from_pptx(bytes).expect("dashed table pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let cell = &pages[0].tables[0].cells[0];
    let b = cell.border_b.as_ref().expect("bottom border present");
    let dash = b.dash.as_ref().expect("prstDash val=\"dash\" → a dash array");
    assert_eq!(
        dash.as_slice(),
        crate::engine::pattern::preset_dash("dash").unwrap().as_slice(),
        "the canonical preset dash array (line-width multiples)"
    );
}

#[test]
fn combined_gridspan_rowspan_dash_deck() {
    // ONE deck combining gridSpan=2 + rowSpan=2 + a dashed
    // border, THROUGH the real XML parse path (the unit rowspan test built the
    // typed cell directly). The anchor grows over both rails on BOTH axes; the
    // three continuation cells are skipped; the dash array survives.
    const MERGED_DASH_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree>
    <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
    <p:graphicFrame>
      <p:nvGraphicFramePr><p:cNvPr id="7" name="Merged Dash"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>
      <p:xfrm><a:off x="914400" y="914400"/><a:ext cx="2438400" cy="685800"/></p:xfrm>
      <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
        <a:tbl>
          <a:tblGrid><a:gridCol w="1219200"/><a:gridCol w="1219200"/></a:tblGrid>
          <a:tr h="342900">
            <a:tc gridSpan="2" rowSpan="2">
              <a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>M</a:t></a:r></a:p></a:txBody>
              <a:tcPr>
                <a:lnB w="12700"><a:solidFill><a:srgbClr val="333333"/></a:solidFill><a:prstDash val="dash"/></a:lnB>
              </a:tcPr>
            </a:tc>
            <a:tc hMerge="1"><a:txBody><a:bodyPr/><a:lstStyle/><a:p/></a:txBody><a:tcPr/></a:tc>
          </a:tr>
          <a:tr h="342900">
            <a:tc vMerge="1" gridSpan="2"><a:txBody><a:bodyPr/><a:lstStyle/><a:p/></a:txBody><a:tcPr/></a:tc>
            <a:tc hMerge="1" vMerge="1"><a:txBody><a:bodyPr/><a:lstStyle/><a:p/></a:txBody><a:tcPr/></a:tc>
          </a:tr>
        </a:tbl>
      </a:graphicData></a:graphic>
    </p:graphicFrame>
  </p:spTree></p:cSld>
</p:sld>"#;
    use crate::engine::fonts::FontCache;
    let bytes = build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", MERGED_DASH_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ]);
    let renderer = PmlRenderer::from_pptx(bytes).expect("merged dash pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let cells = &pages[0].tables[0].cells;
    assert_eq!(cells.len(), 1, "only the anchor survives (3 continuations skipped)");
    let c = &cells[0];
    // 2438400 EMU / 9525 = 256 px wide (both cols); 685800 / 9525 = 72 px tall (both rows).
    assert!((c.w - 256.0).abs() < 0.5, "gridSpan=2 spans both column rails: {}", c.w);
    assert!((c.h - 72.0).abs() < 0.5, "rowSpan=2 spans both row rails: {}", c.h);
    let b = c.border_b.as_ref().expect("bottom border present");
    assert!(b.dash.is_some(), "the dashed border survives on the grown cell");
}

#[test]
fn malformed_table_missing_grid_w_is_dropped() {
    // #56/#63 tolerance: a gridCol missing the strict-required @w fails the
    // per-table parse_as::<CTTable>() in the fork extractor, so the whole table
    // degrades to "no table on the slide" (dropped, not repaired). No panic.
    const BAD_TABLE_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree>
    <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
    <p:graphicFrame>
      <p:nvGraphicFramePr><p:cNvPr id="5" name="Bad Table"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>
      <p:xfrm><a:off x="914400" y="1828800"/><a:ext cx="2438400" cy="685800"/></p:xfrm>
      <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
        <a:tbl>
          <a:tblGrid><a:gridCol/></a:tblGrid>
          <a:tr h="685800"><a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>X</a:t></a:r></a:p></a:txBody><a:tcPr/></a:tc></a:tr>
        </a:tbl>
      </a:graphicData></a:graphic>
    </p:graphicFrame>
  </p:spTree></p:cSld>
</p:sld>"#;
    use crate::engine::fonts::FontCache;
    let bytes = build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", BAD_TABLE_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ]);
    let renderer = PmlRenderer::from_pptx(bytes).expect("deck with malformed table still loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    assert!(
        pages[0].tables.is_empty(),
        "a table missing the strict gridCol @w is dropped whole, not repaired"
    );
}

#[test]
fn generated_tables_fixture_loads_from_disk() {
    // The fflate-DEFLATED generated fixture (tables.pptx) loads end-to-end and
    // resolves the 3×3 grid — proving the generator output is parser-valid (the
    // graphicFrame extra-children → CTTable path). Skipped (not failed) when the
    // gitignored fixture is absent on a clean checkout.
    use crate::engine::fonts::FontCache;
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/ppt/tables.pptx"
    );
    let Ok(bytes) = std::fs::read(path) else {
        eprintln!("skipping: generated tables fixture absent (run generate-test-fixtures.ts)");
        return;
    };
    let renderer = PmlRenderer::from_pptx(bytes).expect("generated tables pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    assert_eq!(pages[0].tables.len(), 1, "one table on the slide");
    let cells = &pages[0].tables[0].cells;
    assert_eq!(cells.len(), 9, "3×3 grid → 9 cell boxes");
    // The header row carries the deep-blue fill (#1e3a8a) + bold white text.
    let header = &cells[0];
    assert_eq!(header.fill_hex.as_deref(), Some("#1e3a8a"));
    assert_eq!(header.paragraphs[0].runs[0].text, "Region");
    assert!(header.paragraphs[0].runs[0].bold);
    // The body data cells carry their text.
    let texts: Vec<&str> = cells
        .iter()
        .flat_map(|c| &c.paragraphs)
        .flat_map(|p| &p.runs)
        .map(|r| r.text.as_str())
        .collect();
    assert!(texts.contains(&"North America") && texts.contains(&"$4.2M"));
}

// ─── Table STYLES (§21.1.3 — green header/first-col/banding) ────────────────

// The table-style part: ONE `<a:tblStyle>` modelled on PowerPoint's built-in
// "Medium Style 2 - Accent 6" (accent6 green). firstRow/firstCol/lastRow fill accent6 SOLID + white
// (lt1) bold text; band1H = accent6 tint40 (light); band2H = no fill; wholeTbl =
// accent6 tint20 + dk1 text + lt1 borders. accent6 = 70AD47 in THEME1.
const STYLED_TABLE_STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:tblStyleLst xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" def="{93296810-A885-4BE3-A3E7-6D5BEEA58F35}"><a:tblStyle styleId="{93296810-A885-4BE3-A3E7-6D5BEEA58F35}" styleName="Styled"><a:wholeTbl><a:tcTxStyle><a:fontRef idx="minor"><a:prstClr val="black"/></a:fontRef><a:schemeClr val="dk1"/></a:tcTxStyle><a:tcStyle><a:tcBdr><a:left><a:ln w="12700" cmpd="sng"><a:solidFill><a:schemeClr val="lt1"/></a:solidFill></a:ln></a:left><a:right><a:ln w="12700" cmpd="sng"><a:solidFill><a:schemeClr val="lt1"/></a:solidFill></a:ln></a:right><a:top><a:ln w="12700" cmpd="sng"><a:solidFill><a:schemeClr val="lt1"/></a:solidFill></a:ln></a:top><a:bottom><a:ln w="12700" cmpd="sng"><a:solidFill><a:schemeClr val="lt1"/></a:solidFill></a:ln></a:bottom></a:tcBdr><a:fill><a:solidFill><a:schemeClr val="accent6"><a:tint val="20000"/></a:schemeClr></a:solidFill></a:fill></a:tcStyle></a:wholeTbl><a:band1H><a:tcStyle><a:tcBdr/><a:fill><a:solidFill><a:schemeClr val="accent6"><a:tint val="40000"/></a:schemeClr></a:solidFill></a:fill></a:tcStyle></a:band1H><a:band2H><a:tcStyle><a:tcBdr/></a:tcStyle></a:band2H><a:firstCol><a:tcTxStyle b="on"><a:fontRef idx="minor"><a:prstClr val="black"/></a:fontRef><a:schemeClr val="lt1"/></a:tcTxStyle><a:tcStyle><a:tcBdr/><a:fill><a:solidFill><a:schemeClr val="accent6"/></a:solidFill></a:fill></a:tcStyle></a:firstCol><a:lastRow><a:tcTxStyle b="on"><a:fontRef idx="minor"><a:prstClr val="black"/></a:fontRef><a:schemeClr val="lt1"/></a:tcTxStyle><a:tcStyle><a:tcBdr/><a:fill><a:solidFill><a:schemeClr val="accent6"/></a:solidFill></a:fill></a:tcStyle></a:lastRow><a:firstRow><a:tcTxStyle b="on"><a:fontRef idx="minor"><a:prstClr val="black"/></a:fontRef><a:schemeClr val="lt1"/></a:tcTxStyle><a:tcStyle><a:tcBdr/><a:fill><a:solidFill><a:schemeClr val="accent6"/></a:solidFill></a:fill></a:tcStyle></a:firstRow></a:tblStyle></a:tblStyleLst>"#;

// Content types incl. the theme + tableStyles overrides (the OPC self-parse
// resolves both via the master/presentation rels).
const STYLED_TABLE_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
  <Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
  <Override PartName="/ppt/tableStyles.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml"/>
</Types>"#;

// Presentation rels with the tableStyles rel (the OPC walk resolves the part
// via `relationships/tableStyles`).
const STYLED_TABLE_PRESENTATION_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/tableStyles" Target="tableStyles.xml"/>
</Relationships>"#;

// A 2-col × 3-row table with firstRow/firstCol/lastRow/bandRow/bandCol set + the
// tableStyleId. Row0 = header; row1 = a body band1H row; row2 = lastRow. The
// row1/col1 cell carries an EXPLICIT orange (F0A030) tcPr solidFill (must STAY
// orange, overriding the band fill). Header text is bare (no rPr colour/bold) →
// it must resolve white+bold from the style.
const STYLED_TABLE_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:graphicFrame>
        <p:nvGraphicFramePr><p:cNvPr id="5" name="Styled Table"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>
        <p:xfrm><a:off x="914400" y="1828800"/><a:ext cx="4876800" cy="2057400"/></p:xfrm>
        <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
          <a:tbl>
            <a:tblPr firstRow="1" firstCol="1" lastRow="1" bandRow="1" bandCol="1"><a:tableStyleId>{93296810-A885-4BE3-A3E7-6D5BEEA58F35}</a:tableStyleId></a:tblPr>
            <a:tblGrid>
              <a:gridCol w="2438400"/>
              <a:gridCol w="2438400"/>
            </a:tblGrid>
            <a:tr h="685800">
              <a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1800"/><a:t>序号</a:t></a:r></a:p></a:txBody><a:tcPr/></a:tc>
              <a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1800"/><a:t>姓名</a:t></a:r></a:p></a:txBody><a:tcPr/></a:tc>
            </a:tr>
            <a:tr h="685800">
              <a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1600"/><a:t>1</a:t></a:r></a:p></a:txBody><a:tcPr/></a:tc>
              <a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1600"/><a:t>备注</a:t></a:r></a:p></a:txBody><a:tcPr><a:solidFill><a:srgbClr val="F0A030"/></a:solidFill></a:tcPr></a:tc>
            </a:tr>
            <a:tr h="685800">
              <a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1600"/><a:t>汇总行</a:t></a:r></a:p></a:txBody><a:tcPr/></a:tc>
              <a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1600"/><a:t></a:t></a:r></a:p></a:txBody><a:tcPr/></a:tc>
            </a:tr>
          </a:tbl>
        </a:graphicData></a:graphic>
      </p:graphicFrame>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

fn styled_table_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", STYLED_TABLE_CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", STYLED_TABLE_PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", THEME_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/theme/theme1.xml", THEME1),
        ("ppt/tableStyles.xml", STYLED_TABLE_STYLES),
        ("ppt/slides/slide1.xml", STYLED_TABLE_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

#[test]
fn table_style_resolves_green_header_first_col_banding_and_white_bold_text() {
    // The expected PowerPoint rendering: firstRow/firstCol/lastRow fill accent6
    // GREEN solid + white bold text; a body band1H row tints light green; an
    // explicit-orange cell keeps its own fill. accent6 = 70AD47, lt1 = white.
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(styled_table_pptx()).expect("styled table pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let cells = &pages[0].tables[0].cells;
    assert_eq!(cells.len(), 6, "2×3 grid → 6 cells");

    // cells are row-major: [0]=(0,0) [1]=(0,1) [2]=(1,0) [3]=(1,1) [4]=(2,0) [5]=(2,1).
    let h0 = &cells[0]; // header (firstRow ∩ firstCol → green)
    let h1 = &cells[1]; // header (firstRow → green)
    assert_eq!(h0.fill_hex.as_deref(), Some("#70ad47"), "header firstRow/firstCol → accent6 green");
    assert_eq!(h1.fill_hex.as_deref(), Some("#70ad47"), "header firstRow → accent6 green");
    // Header text is bare — it must resolve WHITE + bold from the firstRow style.
    let r0 = &h0.paragraphs[0].runs[0];
    assert_eq!(r0.color, "#ffffff", "header text resolves white (lt1) from tcTxStyle");
    assert!(r0.bold, "header text resolves bold from tcTxStyle b=on");

    // Body row1: col0 is firstCol → solid green; col1 is the EXPLICIT orange cell.
    let b_c0 = &cells[2]; // (1,0) firstCol
    let b_c1 = &cells[3]; // (1,1) explicit orange
    assert_eq!(b_c0.fill_hex.as_deref(), Some("#70ad47"), "firstCol body cell → accent6 green");
    assert_eq!(
        b_c1.fill_hex.as_deref(),
        Some("#f0a030"),
        "explicit tcPr solidFill overrides the band fill (stays orange)"
    );
    // The firstCol body cell text resolves white+bold too.
    assert_eq!(b_c0.paragraphs[0].runs[0].color, "#ffffff", "firstCol text white");
    assert!(b_c0.paragraphs[0].runs[0].bold, "firstCol text bold");

    // Last row: firstCol corner (2,0) is green (firstCol+lastRow both green); the
    // (2,1) cell is lastRow → green.
    assert_eq!(cells[4].fill_hex.as_deref(), Some("#70ad47"), "lastRow firstCol corner green");
    assert_eq!(cells[5].fill_hex.as_deref(), Some("#70ad47"), "lastRow cell green");
}

#[test]
fn table_with_no_style_part_renders_unstyled() {
    // The base `table_pptx` deck has NO tableStyles part — every cell renders
    // unstyled (its explicit tcPr only). The header keeps its explicit DDEEFF
    // fill; the plain cells stay fill-less (the style would have added a fill).
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(table_pptx()).expect("table pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let cells = &pages[0].tables[0].cells;
    assert_eq!(cells[0].fill_hex.as_deref(), Some("#ddeeff"), "explicit fill unchanged");
    assert!(cells[1].fill_hex.is_none(), "no style part → plain cell stays fill-less");
}

// ─── Default body bullets (#62-extension) ───────────────────────────────────

#[test]
fn body_placeholder_paragraphs_get_default_bullet() {
    // The minimal deck's body placeholder paragraphs carry NO explicit buChar.
    // Under the #62-extension floor, a body-placeholder paragraph with no bullet
    // inherits the flat default '•'; the title placeholder is never bulleted.
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(minimal_pptx()).expect("minimal pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());

    // Body placeholder paragraphs → default '•'.
    let body = pages[0]
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "First bullet"))
        .expect("body shape");
    assert!(
        body.paragraphs.iter().all(|p| p.bullet.as_deref() == Some("\u{2022}")),
        "every body-placeholder paragraph gets the default round bullet"
    );

    // Title placeholder paragraphs → no bullet.
    let title = pages[0]
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "Hello Title"))
        .expect("title shape");
    assert!(
        title.paragraphs.iter().all(|p| p.bullet.is_none()),
        "the title placeholder is never bulleted"
    );
}

#[test]
fn explicit_bunone_suppresses_default_bullet() {
    // A body-placeholder paragraph with an explicit <a:buNone/> suppresses the
    // default bullet; an explicit buChar still wins; a bare paragraph defaults.
    use crate::engine::fonts::FontCache;
    const BULLET_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree>
    <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="3" name="Body 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
      <p:spPr/>
      <p:txBody><a:bodyPr/><a:lstStyle/>
        <a:p><a:pPr><a:buNone/></a:pPr><a:r><a:rPr lang="en-US"/><a:t>No bullet here</a:t></a:r></a:p>
        <a:p><a:pPr><a:buChar char="&#9658;"/></a:pPr><a:r><a:rPr lang="en-US"/><a:t>Arrow bullet</a:t></a:r></a:p>
        <a:p><a:r><a:rPr lang="en-US"/><a:t>Default bullet</a:t></a:r></a:p>
      </p:txBody>
    </p:sp>
  </p:spTree></p:cSld>
</p:sld>"#;
    let bytes = build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", BULLET_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ]);
    let renderer = PmlRenderer::from_pptx(bytes).expect("bullet pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let body = &pages[0].shapes[0];
    assert_eq!(body.paragraphs.len(), 3);
    assert!(body.paragraphs[0].bullet.is_none(), "buNone suppresses the bullet");
    assert_eq!(body.paragraphs[1].bullet.as_deref(), Some("\u{25BA}"), "explicit buChar wins");
    assert_eq!(
        body.paragraphs[2].bullet.as_deref(),
        Some("\u{2022}"),
        "bare body paragraph gets the default bullet"
    );
}

// ─── PML fidelity cluster: master geometry + txStyles/bullets ──────────────

// A master carrying REAL placeholder geometry (title + body `a:xfrm`) AND a
// `<p:txStyles>` block (§19.3.1.52): titleStyle = 36pt bold, buNone;
// bodyStyle lvl1 = 28pt, marL 342900 / indent −342900, '•'; lvl2 = 24pt,
// marL 742950 / indent −285750, '–'. The master-geometry test inherits the title box from
// HERE (the layout below omits the title); the txStyles tests inherit the sizes/
// bullets/indents for rPr-less runs.
const MASTER_GEOM_TXSTYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="2" name="Title Placeholder 1"/>
          <p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
          <p:nvPr><p:ph type="title"/></p:nvPr>
        </p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="500000" y="300000"/><a:ext cx="8000000" cy="1000000"/></a:xfrm></p:spPr>
      </p:sp>
      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="3" name="Body Placeholder 2"/>
          <p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
          <p:nvPr><p:ph type="body" idx="1"/></p:nvPr>
        </p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="600000" y="1500000"/><a:ext cx="7900000" cy="4000000"/></a:xfrm></p:spPr>
      </p:sp>
    </p:spTree>
  </p:cSld>
  <p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
  <p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
  <p:txStyles>
    <p:titleStyle>
      <a:lvl1pPr><a:buNone/><a:defRPr sz="3600" b="1"/></a:lvl1pPr>
    </p:titleStyle>
    <p:bodyStyle>
      <a:lvl1pPr marL="342900" indent="-342900"><a:buChar char="&#8226;"/><a:defRPr sz="2800"/></a:lvl1pPr>
      <a:lvl2pPr marL="742950" indent="-285750"><a:buChar char="&#8211;"/><a:defRPr sz="2400"/></a:lvl2pPr>
    </p:bodyStyle>
    <p:otherStyle/>
  </p:txStyles>
</p:sldMaster>"#;

// A layout restating ONLY the body placeholder's geometry (no title, no
// lstStyle) — so the slide's ctrTitle must hop past the layout onto the
// MASTER's `title` entry (§19.7.10 tier 3 + the ctrTitle→title alias), while
// the body inherits the LAYOUT box (tier 2 beats the master).
const LAYOUT_BODY_ONLY: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="title" preserve="1">
  <p:cSld name="BodyOnly">
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="2" name="Body 1"/>
          <p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
          <p:nvPr><p:ph type="body" idx="1"/></p:nvPr>
        </p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="685800" y="1828800"/><a:ext cx="7772400" cy="4351338"/></a:xfrm></p:spPr>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sldLayout>"#;

// The test slide: a ctrTitle with NO xfrm and an rPr-less run (geometry from
// the MASTER via the alias; size/bold from titleStyle) + a body (idx 1, no
// xfrm) with an rPr-less lvl-0 paragraph and a lvl-1 paragraph (sizes/bullets/
// indents from bodyStyle lvl1pPr/lvl2pPr).
const MASTER_INHERIT_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="2" name="Title 1"/>
          <p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
          <p:nvPr><p:ph type="ctrTitle"/></p:nvPr>
        </p:nvSpPr>
        <p:spPr/>
        <p:txBody>
          <a:bodyPr/><a:lstStyle/>
          <a:p><a:r><a:rPr lang="en-US"/><a:t>Master Title</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="3" name="Body 2"/>
          <p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
          <p:nvPr><p:ph type="body" idx="1"/></p:nvPr>
        </p:nvSpPr>
        <p:spPr/>
        <p:txBody>
          <a:bodyPr/><a:lstStyle/>
          <a:p><a:r><a:rPr lang="en-US"/><a:t>Level one</a:t></a:r></a:p>
          <a:p><a:pPr lvl="1"/><a:r><a:rPr lang="en-US"/><a:t>Level two</a:t></a:r></a:p>
          <a:p><a:r><a:rPr lang="en-US" sz="1200" b="0"/><a:t>Explicit small</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

/// Build the #61/#62 master-inheritance deck: geometry+txStyles master, a
/// body-only layout, and the rPr-less slide above.
fn master_inherit_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", MASTER_GEOM_TXSTYLES),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", LAYOUT_BODY_ONLY),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", MASTER_INHERIT_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

#[test]
fn ctr_title_inherits_master_xfrm_when_layout_has_no_match() {
    // The slide ctrTitle has no xfrm and the LAYOUT restates no title
    // placeholder — the box must come from the MASTER's `title` entry
    // (§19.7.10 tier 3, ctrTitle→title alias), NOT the sldSz-derived default.
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(master_inherit_pptx()).expect("deck loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let title = pages[0]
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "Master Title"))
        .expect("title shape");
    assert_eq!(title.x, emu_to_px(500_000), "title inherits MASTER off x");
    assert_eq!(title.y, emu_to_px(300_000), "title inherits MASTER off y");
    assert_eq!(title.w, emu_to_px(8_000_000), "title inherits MASTER ext cx");
    assert_eq!(title.h, emu_to_px(1_000_000), "title inherits MASTER ext cy");

    // The LAYOUT still beats the master where it matches (§19.7.10 order):
    // the body placeholder takes the layout box, not the master's.
    let body = pages[0]
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "Level one"))
        .expect("body shape");
    assert_eq!(body.x, emu_to_px(685_800), "body keeps the LAYOUT box");
    assert_eq!(body.y, emu_to_px(1_828_800));
}

#[test]
fn txstyles_sizes_bold_and_bullets_inherit_by_level() {
    // rPr-less runs resolve size/bold through the master txStyles
    // (§19.3.1.52) instead of the type-based defaults: titleStyle 36pt bold;
    // bodyStyle lvl1 28pt '•'; lvl2 24pt '–'. An EXPLICIT rPr still wins.
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(master_inherit_pptx()).expect("deck loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());

    let title_run = pages[0]
        .shapes
        .iter()
        .flat_map(|s| &s.paragraphs)
        .flat_map(|p| &p.runs)
        .find(|r| r.text == "Master Title")
        .expect("title run");
    assert_eq!(title_run.font_pt, 36.0, "titleStyle defRPr sz=3600 → 36pt");
    assert!(title_run.bold, "titleStyle defRPr b=1 → bold");

    let body = pages[0]
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "Level one"))
        .expect("body shape");
    let p0 = &body.paragraphs[0];
    assert_eq!(p0.runs[0].font_pt, 28.0, "bodyStyle lvl1pPr defRPr sz=2800");
    assert_eq!(p0.bullet.as_deref(), Some("\u{2022}"), "lvl1pPr buChar '•'");
    let p1 = &body.paragraphs[1];
    assert_eq!(p1.runs[0].font_pt, 24.0, "bodyStyle lvl2pPr defRPr sz=2400");
    assert_eq!(p1.bullet.as_deref(), Some("\u{2013}"), "lvl2pPr buChar '–'");
    let p2 = &body.paragraphs[2];
    assert_eq!(p2.runs[0].font_pt, 12.0, "explicit rPr sz=1200 beats the chain");
    assert!(!p2.runs[0].bold, "explicit rPr b=0 beats the chain");

    // The title's chain bullet is buNone → no bullet (not the '•' default).
    let title = pages[0]
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "Master Title"))
        .unwrap();
    assert!(title.paragraphs[0].bullet.is_none(), "titleStyle buNone suppresses");
}

#[test]
fn txstyles_indents_apply_per_level() {
    // bodyStyle lvl1 marL=342900/indent=−342900 → hanging bullet at the
    // box edge (first-line 0) with continuation text at marL; lvl2 marL=742950/
    // indent=−285750 → first line at 457200 EMU, continuations at 742950.
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(master_inherit_pptx()).expect("deck loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let body = pages[0]
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "Level one"))
        .expect("body shape");
    let p0 = &body.paragraphs[0];
    assert_eq!(p0.indent_first_px, 0.0, "lvl1: marL+indent = 0 (hanging bullet at edge)");
    assert_eq!(p0.indent_rest_px, emu_to_px(342_900), "lvl1: continuations at marL");
    let p1 = &body.paragraphs[1];
    assert_eq!(p1.indent_first_px, emu_to_px(457_200), "lvl2: marL+indent");
    assert_eq!(p1.indent_rest_px, emu_to_px(742_950), "lvl2: marL");
}

// A slide exercising buAutoNum (§21.1.2.4.1): a body placeholder with two
// arabicPeriod paragraphs, a lvl-1 alphaLcParenR, a third arabicPeriod (the
// lvl-0 counter continues past the sub-level), and an exotic scheme (falls back
// to arabicPeriod, continuing the lvl-0 counter); PLUS a second (non-
// placeholder) text box whose numbering RESTARTS with startAt=5 and a lvl-1
// romanLcPeriod.
const AUTONUM_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree>
    <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="2" name="Body 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
      <p:spPr/>
      <p:txBody><a:bodyPr/><a:lstStyle/>
        <a:p><a:pPr><a:buAutoNum type="arabicPeriod"/></a:pPr><a:r><a:rPr lang="en-US"/><a:t>One</a:t></a:r></a:p>
        <a:p><a:pPr><a:buAutoNum type="arabicPeriod"/></a:pPr><a:r><a:rPr lang="en-US"/><a:t>Two</a:t></a:r></a:p>
        <a:p><a:pPr lvl="1"><a:buAutoNum type="alphaLcParenR"/></a:pPr><a:r><a:rPr lang="en-US"/><a:t>Sub</a:t></a:r></a:p>
        <a:p><a:pPr><a:buAutoNum type="arabicPeriod"/></a:pPr><a:r><a:rPr lang="en-US"/><a:t>Three</a:t></a:r></a:p>
        <a:p><a:pPr><a:buAutoNum type="circleNumDbPlain"/></a:pPr><a:r><a:rPr lang="en-US"/><a:t>Exotic</a:t></a:r></a:p>
      </p:txBody>
    </p:sp>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="3" name="TextBox 2"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
      <p:spPr><a:xfrm><a:off x="685800" y="5181600"/><a:ext cx="2286000" cy="1143000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
      <p:txBody><a:bodyPr/><a:lstStyle/>
        <a:p><a:pPr><a:buAutoNum type="arabicPeriod" startAt="5"/></a:pPr><a:r><a:rPr lang="en-US"/><a:t>Five</a:t></a:r></a:p>
        <a:p><a:pPr lvl="1"><a:buAutoNum type="romanLcPeriod"/></a:pPr><a:r><a:rPr lang="en-US"/><a:t>Roman</a:t></a:r></a:p>
      </p:txBody>
    </p:sp>
  </p:spTree></p:cSld>
</p:sld>"#;

#[test]
fn autonum_bullets_number_restart_and_start_at() {
    // Bullets: buAutoNum renders REAL formatted ordinals (was the flat '•'):
    // per-level counters within one txBody, numbering restarts per shape text
    // body, @startAt honored, exotic schemes fall back to arabicPeriod.
    use crate::engine::fonts::FontCache;
    let bytes = build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", AUTONUM_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ]);
    let renderer = PmlRenderer::from_pptx(bytes).expect("autonum pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());

    let body = pages[0]
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "One"))
        .expect("body shape");
    let bullets: Vec<&str> =
        body.paragraphs.iter().map(|p| p.bullet.as_deref().unwrap_or("")).collect();
    // Lvl-0 counter: 1, 2, (lvl-1 counter: a), 3, then the exotic scheme takes
    // 4 from the SAME lvl-0 counter but formats via the arabicPeriod fallback.
    assert_eq!(bullets, vec!["1.", "2.", "a)", "3.", "4."]);

    let boxed = pages[0]
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "Five"))
        .expect("second text box");
    let bullets2: Vec<&str> =
        boxed.paragraphs.iter().map(|p| p.bullet.as_deref().unwrap_or("")).collect();
    // A NEW text body restarts numbering; startAt=5 seeds the lvl-0 counter;
    // the lvl-1 romanLcPeriod starts its own counter at 1 → "i.".
    assert_eq!(bullets2, vec!["5.", "i."]);
}

// ─── Slide charts (DML chart part via p:graphicFrame c:chart) ───────────────

// A minimal `c:chartSpace` bar chart part (§21.2): one series ("Sales") over
// three categories with three numeric values. Same schema WML/SML render, so the
// shared `crate::chart::render_chart` parses it AS-IS.
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
          <c:tx><c:strRef><c:f>Sheet1!$B$1</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>Sales</c:v></c:pt></c:strCache></c:strRef></c:tx>
          <c:cat><c:strRef><c:f>Sheet1!$A$2:$A$4</c:f><c:strCache><c:ptCount val="3"/><c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt><c:pt idx="2"><c:v>Q3</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:f>Sheet1!$B$2:$B$4</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val="3"/><c:pt idx="0"><c:v>10</c:v></c:pt><c:pt idx="1"><c:v>24</c:v></c:pt><c:pt idx="2"><c:v>17</c:v></c:pt></c:numCache></c:numRef></c:val>
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

// A slide whose p:graphicFrame holds a c:chart referencing the chart part via
// rId2 (the slide rels map rId2 → the chart part). The frame xfrm positions the
// chart at (914400, 914400), sized 5486400 × 3200400 EMU.
const CHART_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:graphicFrame>
        <p:nvGraphicFramePr><p:cNvPr id="5" name="Chart 4"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>
        <p:xfrm><a:off x="914400" y="914400"/><a:ext cx="5486400" cy="3200400"/></p:xfrm>
        <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
          <c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="rId2"/>
        </a:graphicData></a:graphic>
      </p:graphicFrame>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

// Content types for the chart deck: the base set plus the chart part override.
const CONTENT_TYPES_CHART: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
  <Override PartName="/ppt/charts/chart1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>
</Types>"#;

const CHART_SLIDE_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/>
</Relationships>"#;

/// Build the chart deck: base master/layout/presentation + a slide with a
/// chart graphicFrame + the chart part.
fn chart_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES_CHART),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", CHART_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", CHART_SLIDE_RELS),
        ("ppt/charts/chart1.xml", CHART_PART),
    ])
}

#[test]
fn chart_xfrm_box_and_namespaced_key() {
    // The slide's chart graphicFrame carries an explicit xfrm (off 914400,914400;
    // ext 5486400×3200400 EMU). Pin the EMU→px draw box AND the slide-namespaced
    // chart key (`chart::pml::0::rId2`) the renderer stores the chart XML under.
    use crate::engine::fonts::FontCache;
    use crate::engine::units::emu_to_px;
    let renderer = PmlRenderer::from_pptx(chart_pptx()).expect("chart pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].charts.len(), 1, "one chart on the slide");
    let ch = &pages[0].charts[0];
    assert_eq!(ch.x, emu_to_px(914_400), "chart off x EMU → px");
    assert_eq!(ch.y, emu_to_px(914_400), "chart off y EMU → px");
    assert_eq!(ch.w, emu_to_px(5_486_400), "chart ext cx EMU → px");
    assert_eq!(ch.h, emu_to_px(3_200_400), "chart ext cy EMU → px");
    assert_eq!(ch.chart_key, "chart::pml::0::rId2", "slide-namespaced chart key");
}

#[test]
fn chart_part_xml_extracted_and_parses_with_series() {
    // The renderer collects the chart part XML at load (OPC self-parse: slide
    // rels → chart rel → chart part), keyed by the namespaced chart key. Verify
    // the bytes resolve AND that the SHARED chart parser reads a non-empty series
    // from them (proving a pptx chart part is the standard DML c:chartSpace).
    let mut renderer = PmlRenderer::from_pptx(chart_pptx()).expect("chart pptx loads");
    let charts = renderer.take_charts();
    assert_eq!(charts.len(), 1, "one chart's XML collected");
    let (key, xml) = &charts[0];
    assert_eq!(key, "chart::pml::0::rId2", "bytes keyed by the namespaced chart key");
    // The shared DML chart parser must read the bar series out of the part.
    let cs = ooxml_dml::parse_chart_space(xml).expect("pptx chart part parses as c:chartSpace");
    use ooxml_dml::ext::{ChartExt, ChartSpaceExt};
    let bar = cs
        .chart()
        .plot_area()
        .bar_chart
        .first()
        .expect("bar chart present in the pptx chart part");
    assert!(!bar.ser.is_empty(), "chart part carries a non-empty series");
    // take_charts drains (the Viewer drains once at load).
    assert!(renderer.take_charts().is_empty(), "charts drained after take");
}

#[test]
fn generated_chart_fixture_loads_from_disk() {
    // The fflate-DEFLATED generated fixture (chart.pptx) loads end-to-end and
    // resolves the chart box + a parseable chart part. Skipped (not failed) when
    // the gitignored fixture is absent on a clean checkout.
    use crate::engine::fonts::FontCache;
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/ppt/chart.pptx"
    );
    let Ok(bytes) = std::fs::read(path) else {
        eprintln!("skipping: generated chart fixture absent (run generate-test-fixtures.ts)");
        return;
    };
    let mut renderer = PmlRenderer::from_pptx(bytes).expect("generated chart pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    assert_eq!(pages[0].charts.len(), 1, "one chart on the slide");
    let charts = renderer.take_charts();
    assert_eq!(charts.len(), 1, "chart part XML collected");
    assert!(
        ooxml_dml::parse_chart_space(&charts[0].1).is_ok(),
        "generated chart part parses as c:chartSpace"
    );
}

// ─── Theme scheme-colour resolution ─────────────────────────────────────────

// Content types for a deck WITH a theme part (the base CONTENT_TYPES omits it).
const THEME_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
  <Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
</Types>"#;

// Master rels: layout (rId1) AND theme (rId2). The theme-aware OPC walk reads
// rId2 for the clrScheme; the base SLIDE_MASTER_RELS has only the layout.
const THEME_MASTER_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>"#;

// The Office "Office" theme clrScheme: dk1/lt1 via sysClr lastClr (000000/FFFFFF),
// accent1 = 4472C4 (the canonical themed blue).
const THEME1: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office">
  <a:themeElements>
    <a:clrScheme name="Office">
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
    </a:clrScheme>
  </a:themeElements>
</a:theme>"#;

// A slide with a schemeClr-themed background (accent1), a non-placeholder rect
// filled with a bare schemeClr accent1, a second rect filled with accent1 +
// lumMod 50% (the dominant transform), and a run coloured tx1 (→ dk1 via clrMap).
const THEME_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:bg><p:bgPr><a:solidFill><a:schemeClr val="accent1"/></a:solidFill></p:bgPr></p:bg>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="R1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="100000" y="100000"/><a:ext cx="500000" cy="500000"/></a:xfrm>
          <a:solidFill><a:schemeClr val="accent1"/></a:solidFill></p:spPr>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="3" name="R2"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="700000" y="100000"/><a:ext cx="500000" cy="500000"/></a:xfrm>
          <a:solidFill><a:schemeClr val="accent1"><a:lumMod val="50000"/></a:schemeClr></a:solidFill></p:spPr>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="4" name="T1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="100000" y="700000"/><a:ext cx="2000000" cy="500000"/></a:xfrm></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/>
          <a:p><a:r><a:rPr lang="en-US"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill></a:rPr><a:t>Themed</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

fn theme_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", THEME_CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", THEME_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/theme/theme1.xml", THEME1),
        ("ppt/slides/slide1.xml", THEME_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

#[test]
fn schemeclr_shape_fill_resolves_to_theme_color() {
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(theme_pptx()).expect("theme pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    // accent1 (4472C4) on the first rect — None here would paint it invisible.
    let r1 = pages[0].shapes.iter().find(|s| s.solid_fill_hex() == Some("#4472c4"));
    assert!(r1.is_some(), "bare schemeClr accent1 fill resolves to #4472c4; got {:?}",
        pages[0].shapes.iter().map(|s| s.fill.clone()).collect::<Vec<_>>());
}

#[test]
fn schemeclr_lummod_darkens_the_fill() {
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(theme_pptx()).expect("theme pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    // accent1 + lumMod 50% → the naive-HSL darkened blue #203864 (matches the
    // engine::color unit test).
    let r2 = pages[0].shapes.iter().find(|s| s.solid_fill_hex() == Some("#203864"));
    assert!(r2.is_some(), "schemeClr accent1 + lumMod 50% resolves to #203864; got {:?}",
        pages[0].shapes.iter().map(|s| s.fill.clone()).collect::<Vec<_>>());
}

#[test]
fn schemeclr_run_color_resolves_via_clrmap() {
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(theme_pptx()).expect("theme pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    // tx1 → clrMap tx1=dk1 → sysClr windowText lastClr 000000 → #000000.
    let run = pages[0]
        .shapes
        .iter()
        .flat_map(|s| s.paragraphs.iter())
        .flat_map(|p| p.runs.iter())
        .find(|r| r.text == "Themed")
        .expect("themed run present");
    assert_eq!(run.color, "#000000", "tx1 resolves via clrMap to dk1 #000000");
}

#[test]
fn schemeclr_background_resolves_to_theme_color() {
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(theme_pptx()).expect("theme pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    // The slide bg is solidFill/schemeClr accent1 → #4472c4 (not the white default).
    assert_eq!(
        pages[0].bg,
        Some(shape::SlideBg::Solid("#4472c4".to_string())),
        "themed slide bg resolves"
    );
}

// ---------------------------------------------------------------------------
// Slide background gradients (§19.3.1.2 bgPr + §20.1.8.33 gradFill)
// ---------------------------------------------------------------------------

// A slide with a 2-stop srgbClr gradFill bg + explicit linear angle
// (002060 → 7030A0, ang = 5400000 → 90°, top → bottom).
const GRAD_BG_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:bg><p:bgPr><a:gradFill rotWithShape="1">
      <a:gsLst>
        <a:gs pos="0"><a:srgbClr val="002060"/></a:gs>
        <a:gs pos="100000"><a:srgbClr val="7030A0"/></a:gs>
      </a:gsLst>
      <a:lin ang="5400000" scaled="0"/>
    </a:gradFill></p:bgPr></p:bg>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="T1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="100000" y="100000"/><a:ext cx="2000000" cy="500000"/></a:xfrm></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/>
          <a:p><a:r><a:rPr lang="en-US"><a:solidFill><a:schemeClr val="bg1"/></a:solidFill></a:rPr><a:t>White title</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

// A slide whose gradFill stops are schemeClr (theme-resolved, one with lumMod)
// and which carries NO <a:lin> → the 45° default angle.
const GRAD_BG_SCHEME_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:bg><p:bgPr><a:gradFill>
      <a:gsLst>
        <a:gs pos="30000"><a:schemeClr val="accent1"><a:lumMod val="50000"/></a:schemeClr></a:gs>
        <a:gs pos="0"><a:schemeClr val="accent1"/></a:gs>
      </a:gsLst>
    </a:gradFill></p:bgPr></p:bg>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

fn grad_bg_pptx(slide_xml: &str) -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", THEME_CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", THEME_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/theme/theme1.xml", THEME1),
        ("ppt/slides/slide1.xml", slide_xml),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

#[test]
fn gradfill_background_resolves_stops_and_angle() {
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(grad_bg_pptx(GRAD_BG_SLIDE)).expect("grad pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    // 002060 → 7030A0, ang 5400000/60000 = 90° (top → bottom). None here would
    // leave a white bg under the white title text.
    assert_eq!(
        pages[0].bg,
        Some(shape::SlideBg::Gradient {
            stops: vec![(0.0, "#002060".to_string()), (1.0, "#7030a0".to_string())],
            angle_deg: 90.0,
        }),
        "2-stop srgbClr gradFill bg resolves with its linear angle"
    );
    // The white (bg1 → lt1) title run is untouched by the gradient — it still resolves.
    let run = pages[0]
        .shapes
        .iter()
        .flat_map(|s| s.paragraphs.iter())
        .flat_map(|p| p.runs.iter())
        .find(|r| r.text == "White title")
        .expect("title run present");
    assert_eq!(run.color, "#ffffff", "white title text stays white");
}

// A real-world slide background shape: a PATH gradient (§20.1.8.46) with the
// fillToRect collapsed to the bottom-right corner (l=t=100000). PowerPoint
// renders this radially — dark blue AT the corner, purple radiating outward.
// The old 45°-linear approximation put the colours at OPPOSITE corners.
const GRAD_BG_PATH_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:bg><p:bgPr><a:gradFill>
      <a:gsLst>
        <a:gs pos="0"><a:srgbClr val="002060"/></a:gs>
        <a:gs pos="100000"><a:srgbClr val="7030A0"/></a:gs>
      </a:gsLst>
      <a:path path="rect"><a:fillToRect l="100000" t="100000"/></a:path>
    </a:gradFill><a:effectLst/></p:bgPr></p:bg>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

#[test]
fn gradfill_path_background_resolves_radial_with_corner_focus() {
    use crate::engine::fonts::FontCache;
    let renderer =
        PmlRenderer::from_pptx(grad_bg_pptx(GRAD_BG_PATH_SLIDE)).expect("path-grad pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    // fillToRect l=t=100000 (r/b absent → 0) → focus centre ((1+1)/2, (1+1)/2)
    // = (1.0, 1.0): the bottom-right corner. Stop 0 (002060) lands AT the
    // focus, the last stop (7030A0) at the farthest corner — matching
    // PowerPoint's own render of this gradient.
    assert_eq!(
        pages[0].bg,
        Some(shape::SlideBg::Radial {
            stops: vec![(0.0, "#002060".to_string()), (1.0, "#7030a0".to_string())],
            focus: (1.0, 1.0),
        }),
        "path gradFill resolves to a radial bg focused at the fillToRect corner"
    );
}

#[test]
fn gradfill_schemeclr_stops_resolve_sorted_with_default_angle() {
    use crate::engine::fonts::FontCache;
    let renderer =
        PmlRenderer::from_pptx(grad_bg_pptx(GRAD_BG_SCHEME_SLIDE)).expect("grad pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    // Stops arrive out of order in the XML → sorted by pos. accent1 = #4472c4;
    // accent1 + lumMod 50% = #203864 (the colour-transform pin). No <a:lin> → 45°.
    assert_eq!(
        pages[0].bg,
        Some(shape::SlideBg::Gradient {
            stops: vec![(0.0, "#4472c4".to_string()), (0.3, "#203864".to_string())],
            angle_deg: 45.0,
        }),
        "schemeClr stops resolve via the theme, sorted, with the 45° default"
    );
}

// ─── Preset-geometry fixture ────────────────────────────────────────────────

// A slide exercising the preset-path builder end-to-end: a filled PIE, a
// filled rightArrow WITH an explicit avLst adj, a roundRect, and a shape with
// a preset OUTSIDE tier 1 (mathPlus → rect fallback + unknown_preset signal).
// All shapes carry explicit fills so the empty-shape skip keeps them (fillRef
// styling is covered by the style-matrix fixture below).
const GEOM_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Pie 1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="914400" y="914400"/><a:ext cx="914400" cy="914400"/></a:xfrm><a:prstGeom prst="pie"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="7030A0"/></a:solidFill></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="3" name="Right Arrow 2"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="2743200" y="914400"/><a:ext cx="1828800" cy="914400"/></a:xfrm><a:prstGeom prst="rightArrow"><a:avLst><a:gd name="adj1" fmla="val 100000"/><a:gd name="adj2" fmla="val 50000"/></a:avLst></a:prstGeom><a:solidFill><a:srgbClr val="4472C4"/></a:solidFill></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="4" name="Rounded Rect 3"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="914400" y="2743200"/><a:ext cx="1828800" cy="914400"/></a:xfrm><a:prstGeom prst="roundRect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="ED7D31"/></a:solidFill></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="5" name="Math Plus 4"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="3657600" y="2743200"/><a:ext cx="914400" cy="914400"/></a:xfrm><a:prstGeom prst="mathPlus"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="70AD47"/></a:solidFill></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

fn geom_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", GEOM_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

/// Preset geometry end-to-end: the fork-parsed `prstGeom` (preset + avLst) flows through
/// `shape_geometry` into a SHAPE-LOCAL PathCmd list on every SlideShape, and
/// the paths cover non-rect pixels (the native point-in-fill proxy for the
/// canvas painter — same nonzero rule). The unknown preset stays rect (None)
/// and reports its name for the debug-probe field-gap log.
#[test]
fn preset_geometry_resolves_pie_arrow_roundrect_and_flags_unknown() {
    use crate::engine::fonts::FontCache;
    use crate::engine::geom::testutil::point_in_path;
    let renderer = PmlRenderer::from_pptx(geom_pptx()).expect("geom pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    assert_eq!(pages.len(), 1);
    let find = |fill: &str| {
        pages[0]
            .shapes
            .iter()
            .find(|s| s.solid_fill_hex() == Some(fill))
            .unwrap_or_else(|| panic!("shape with fill {fill} present"))
    };

    // PIE (914400 EMU square = 96×96 px): default 0°→270° wedge — the corner
    // pixel a rect would fill (top-right quadrant) is OUTSIDE the path.
    let pie = find("#7030a0");
    let pie_path = pie.geometry.as_ref().expect("pie geometry resolved");
    assert_eq!(pie.unknown_preset, None);
    assert!((pie.w - emu_to_px(914_400)).abs() < 1e-9);
    assert!(
        !point_in_path(pie_path, pie.w * 0.9, pie.h * 0.1),
        "pie leaves the top-right quadrant empty where the rect was filled"
    );
    assert!(point_in_path(pie_path, pie.w * 0.3, pie.h * 0.7), "wedge body filled");

    // rightArrow with avLst adj1=100000 (full-height shaft): the parsed gd
    // values must reach preset_path — tail corner (0,0) IS in the path (dy =
    // h/2), while the head corner stays empty.
    let arrow = find("#4472c4");
    let arrow_path = arrow.geometry.as_ref().expect("arrow geometry resolved");
    assert!(
        point_in_path(arrow_path, 1.0, 1.0),
        "adj1=100000 widens the shaft to the full height (avLst parsed)"
    );
    assert!(
        !point_in_path(arrow_path, arrow.w - 1.0, 1.0),
        "head corner above the arrow tip is empty"
    );

    // roundRect: corner pixel shaved by the default-radius arc.
    let rr = find("#ed7d31");
    let rr_path = rr.geometry.as_ref().expect("roundRect geometry resolved");
    assert!(!point_in_path(rr_path, 1.0, 1.0), "rounded corner shaved");
    assert!(point_in_path(rr_path, rr.w / 2.0, rr.h / 2.0));

    // Unknown preset (mathPlus, outside tier 1): no path, name surfaced.
    let unk = find("#70ad47");
    assert!(unk.geometry.is_none(), "unknown preset paints the rect fallback");
    assert_eq!(unk.unknown_preset.as_deref(), Some("mathPlus"));
}

// ─── Style-matrix fills + gradient/pattern shape fills ──────────────────────

// A slide exercising style fillRef/lnRef and gradFill/pattFill:
//  - sp2 "Styled Arrow": NO explicit fill/line/text — a skip rule blind to
//    `<p:style>` would drop it. Its `<p:style>` fillRef idx=1 (accent1) + lnRef
//    idx=2 (accent2) must fill+outline it (a common styled-arrow shape).
//  - sp3 "Grad": explicit 2-stop gradFill (srgbClr + schemeClr stops, out of
//    order, lin ang 90°).
//  - sp4 "Patt": pattFill prst=pct5 fg FF0000 / bg 0000FF → a real repeating
//    Pattern (tier-1 preset); an unmapped preset would fall back to the blend.
//  - sp5 "NoFill": explicit noFill + a fillRef idx=1 style — explicit wins,
//    no fill (but lnRef still outlines it).
const STYLE_FILL_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Styled Arrow"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="914400" y="914400"/><a:ext cx="1828800" cy="914400"/></a:xfrm><a:prstGeom prst="rightArrow"><a:avLst/></a:prstGeom></p:spPr>
        <p:style><a:lnRef idx="2"><a:schemeClr val="accent2"/></a:lnRef><a:fillRef idx="1"><a:schemeClr val="accent1"/></a:fillRef><a:effectRef idx="0"><a:schemeClr val="accent1"/></a:effectRef><a:fontRef idx="minor"><a:schemeClr val="lt1"/></a:fontRef></p:style>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="3" name="Grad"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="914400" y="2743200"/><a:ext cx="914400" cy="914400"/></a:xfrm><a:prstGeom prst="ellipse"><a:avLst/></a:prstGeom><a:gradFill><a:gsLst><a:gs pos="100000"><a:schemeClr val="accent1"/></a:gs><a:gs pos="0"><a:srgbClr val="FF0000"/></a:gs></a:gsLst><a:lin ang="5400000" scaled="1"/></a:gradFill></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="4" name="Patt"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="2743200" y="2743200"/><a:ext cx="914400" cy="914400"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:pattFill prst="pct5"><a:fgClr><a:srgbClr val="FF0000"/></a:fgClr><a:bgClr><a:srgbClr val="0000FF"/></a:bgClr></a:pattFill></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="5" name="NoFill"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="4572000" y="2743200"/><a:ext cx="914400" cy="914400"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/></p:spPr>
        <p:style><a:lnRef idx="1"><a:schemeClr val="accent6"/></a:lnRef><a:fillRef idx="1"><a:schemeClr val="accent1"/></a:fillRef><a:effectRef idx="0"><a:schemeClr val="accent1"/></a:effectRef><a:fontRef idx="minor"><a:schemeClr val="lt1"/></a:fontRef></p:style>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="6" name="PattUnknown"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="6400800" y="2743200"/><a:ext cx="914400" cy="914400"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:pattFill prst="sphere"><a:fgClr><a:srgbClr val="FF0000"/></a:fgClr><a:bgClr><a:srgbClr val="0000FF"/></a:bgClr></a:pattFill></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="7" name="Dashed"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="914400" y="4572000"/><a:ext cx="914400" cy="914400"/></a:xfrm><a:prstGeom prst="roundRect"><a:avLst/></a:prstGeom><a:ln w="28575"><a:solidFill><a:schemeClr val="accent1"/></a:solidFill><a:prstDash val="dashDot"/></a:ln></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

fn style_fill_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", THEME_CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        // THEME_MASTER_RELS (not the base SLIDE_MASTER_RELS): the schemeClr
        // refs need the master→theme rel for the clrScheme walk.
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", THEME_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/theme/theme1.xml", THEME1),
        ("ppt/slides/slide1.xml", STYLE_FILL_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

/// A shape styled ONLY via `<p:style>` (no explicit fill/line/text) is
/// kept (the relaxed skip rule), filled with the fillRef colour (accent1) and
/// outlined with the lnRef colour (accent2) — plus its preset geometry resolves.
#[test]
fn style_ref_only_shape_renders_with_accent_fill_and_outline() {
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(style_fill_pptx()).expect("style pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    assert_eq!(pages.len(), 1);
    let arrow = pages[0]
        .shapes
        .iter()
        .find(|s| s.solid_fill_hex() == Some("#4472c4"))
        .expect("style-ref-only shape kept and filled with fillRef accent1");
    assert!(arrow.geometry.is_some(), "rightArrow geometry resolved");
    let out = arrow.outline.as_ref().expect("lnRef outlines the shape");
    assert_eq!(out.color, "#ed7d31", "lnRef idx=2 accent2 line colour");
}

/// An explicit `gradFill` resolves to a 2-stop linear gradient (stops
/// sorted by pos, srgbClr + theme-resolved schemeClr, `a:lin@ang` honoured).
#[test]
fn gradfill_shape_resolves_sorted_stops_and_angle() {
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(style_fill_pptx()).expect("style pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let grad = pages[0]
        .shapes
        .iter()
        .find(|s| matches!(s.fill, Some(shape::ShapeFill::Gradient { .. })))
        .expect("gradFill shape present");
    assert_eq!(
        grad.fill,
        Some(shape::ShapeFill::Gradient {
            stops: vec![(0.0, "#ff0000".to_string()), (1.0, "#4472c4".to_string())],
            angle_deg: 90.0,
        }),
        "stops sorted by pos; schemeClr stop theme-resolved; ang 5400000 → 90°"
    );
    assert!(grad.geometry.is_some(), "ellipse geometry resolved");
}

/// pattFill: a tier-1 preset (`pct5`) now resolves to a real repeating
/// [`shape::ShapeFill::Pattern`] (the painter rasterises a tile), carrying the
/// resolved fg/bg — NOT the 50/50 blend (that's only the fallback for
/// unmapped presets). And an explicit `noFill` beats the style fillRef (no
/// fill), while the lnRef still outlines.
#[test]
fn pattfill_tier1_makes_a_pattern_and_nofill_beats_style_fillref() {
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(style_fill_pptx()).expect("style pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    assert!(
        pages[0].shapes.iter().any(|s| matches!(
            &s.fill,
            Some(shape::ShapeFill::Pattern { prst, fg, bg })
                if prst == "pct5" && fg == "#ff0000" && bg == "#0000ff"
        )),
        "pct5 pattFill → a real Pattern with resolved fg/bg, got fills {:?}",
        pages[0].shapes.iter().map(|s| s.fill.clone()).collect::<Vec<_>>()
    );
    // An UNMAPPED preset (sphere) still falls back to the 50/50 blend
    // (#FF0000 + #0000FF → #7f007f) so the shape never silently vanishes.
    assert!(
        pages[0].shapes.iter().any(|s| s.solid_fill_hex() == Some("#7f007f")),
        "unmapped 'sphere' pattFill → 50/50 blend fallback"
    );
    let nofill = pages[0]
        .shapes
        .iter()
        .find(|s| s.outline.as_ref().is_some_and(|o| o.color == "#70ad47"))
        .expect("noFill shape kept via its lnRef outline (accent6)");
    assert!(nofill.fill.is_none(), "explicit noFill beats the style fillRef");
}

/// An `<a:ln>` with `<a:prstDash val="dashDot"/>` threads the canonical
/// dash-dot on/off array (line-width multiples) onto the shape outline; the
/// painter scales it by the pen width before `setLineDash`.
#[test]
fn prstdash_threads_a_dash_array_onto_the_outline() {
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(style_fill_pptx()).expect("style pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let dashed = pages[0]
        .shapes
        .iter()
        .find(|s| s.outline.as_ref().and_then(|o| o.dash.as_ref()).is_some())
        .expect("the dashDot-outlined shape carries a dash array");
    assert_eq!(
        dashed.outline.as_ref().unwrap().dash.as_deref(),
        Some([4.0, 3.0, 1.0, 3.0].as_slice()),
        "dashDot → 4-on/3-off/1-on/3-off line-width multiples"
    );
    // a shape WITHOUT a prstDash stays solid (dash == None).
    assert!(
        pages[0]
            .shapes
            .iter()
            .any(|s| s.outline.as_ref().is_some_and(|o| o.dash.is_none())),
        "non-dashed outlines stay solid (dash None)"
    );
}

/// #56-class audit gate for the newly-armed `pml-styling` (+ dml-shapes/
/// dml-themes) parsers: every generated ppt fixture still parses — a
/// `<p:style>` missing one of its 4 strict-required refs (or a ref missing
/// `@idx`) would fail the whole slide parse. Skips when the generated
/// fixtures are absent (clean CI checkout without generate-test-fixtures.ts).
#[test]
fn playground_ppt_fixtures_still_parse_with_pml_styling() {
    let dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/ppt"
    );
    let Ok(entries) = std::fs::read_dir(dir) else {
        eprintln!("skipping: generated fixtures absent (run generate-test-fixtures.ts)");
        return;
    };
    let mut checked = 0;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("pptx") {
            continue;
        }
        let bytes = std::fs::read(&p).expect("fixture readable");
        let renderer = PmlRenderer::from_pptx(bytes)
            .unwrap_or_else(|err| panic!("{} failed to parse: {err:?}", p.display()));
        assert!(renderer.slide_count() > 0, "{} has slides", p.display());
        checked += 1;
    }
    assert!(checked > 0, "fixture dir present but empty");
}

// ─── Theme fonts + East Asian font slot ─────────────────────────────────────
//
// Mirrors a common CJK deck shape: the theme's minorFont latin IS
// the CJK-capable face (等线/DengXian) with an EMPTY <a:ea typeface=""/>; runs
// either carry no rPr fonts at all or the +mn-lt/+mn-ea theme refs. Without
// theme-font resolution every run fell to Calibri, font_families() exported
// nothing, and CJK advances were estimated at 0.5 em while the canvas painted
// 1 em → horizontal run-on-run overlap.

const CONTENT_TYPES_CJK: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
  <Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
</Types>"#;

// Master rels including the theme part (the font-scheme source).
const SLIDE_MASTER_RELS_THEMED: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>"#;

// The CJK theme's fontScheme: latin slots are DengXian (the CJK family), ea/cs
// EMPTY — exactly how PowerPoint writes zh-CN themes (the script-supplemental
// <a:font script="Hans"…> table carries the real EA mapping, which we
// approximate by delegating EA to the latin face).
const CJK_THEME: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office">
  <a:themeElements>
    <a:clrScheme name="Office"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:accent1><a:srgbClr val="5B9BD5"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="4472C4"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme>
    <a:fontScheme name="Office">
      <a:majorFont><a:latin typeface="等线 Light"/><a:ea typeface=""/><a:cs typeface=""/><a:font script="Hans" typeface="等线 Light"/></a:majorFont>
      <a:minorFont><a:latin typeface="等线"/><a:ea typeface=""/><a:cs typeface=""/><a:font script="Hans" typeface="等线"/></a:minorFont>
    </a:fontScheme>
  </a:themeElements>
</a:theme>"#;

// Three paragraphs, one per resolution path:
//  p1: NO rPr fonts, mixed zh/latin text (theme minor fallback);
//  p2: +mn-lt/+mn-ea theme refs (ref resolution, empty-ea → latin);
//  p3: an explicit literal ea typeface (the straightforward rPr/ea read).
const CJK_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="TextBox 1"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="685800" y="457200"/><a:ext cx="7772400" cy="2143000"/></a:xfrm></p:spPr>
        <p:txBody><a:bodyPr wrap="none"/><a:lstStyle/>
          <a:p><a:r><a:rPr lang="zh-CN" altLang="en-US" sz="1400" dirty="0"/><a:t>支持npm下载</a:t></a:r></a:p>
          <a:p><a:r><a:rPr lang="zh-CN" altLang="en-US" sz="1400"><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/></a:rPr><a:t>第二级别标题</a:t></a:r></a:p>
          <a:p><a:r><a:rPr lang="zh-CN" altLang="en-US" sz="1400"><a:ea typeface="SimSun"/></a:rPr><a:t>副标题</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

/// Build the CJK deck: base presentation/layout + a THEMED master (master
/// rels → theme1.xml carrying the DengXian fontScheme) + the three-path slide.
fn cjk_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES_CJK),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS_THEMED),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/theme/theme1.xml", CJK_THEME),
        ("ppt/slides/slide1.xml", CJK_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

/// A FontCache with the Arimo test face inserted under the deck's CJK family
/// names (resolution is name-based; glyph coverage is irrelevant here).
fn cjk_font_cache() -> crate::engine::fonts::FontCache {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
    let bytes = std::fs::read(format!("{dir}/Arimo-Regular.ttf")).unwrap_or_else(|_| {
        panic!("test font missing: Arimo-Regular.ttf\n  run: ./scripts/fetch-test-fonts.sh")
    });
    let mut fc = crate::engine::fonts::FontCache::new();
    fc.insert("等线", bytes.clone(), 400, false).expect("insert 等线");
    fc.insert("SimSun", bytes, 400, false).expect("insert SimSun");
    fc
}

#[test]
fn cjk_runs_resolve_theme_and_ea_families() {
    // Theme-font pin, all three resolution paths:
    //  p1 (no rPr fonts): latin → theme MINOR latin (等线, not Calibri); ea →
    //     theme ea is EMPTY → delegates to the latin face (等线);
    //  p2 (+mn-lt/+mn-ea): theme refs resolve (not passed through literally);
    //  p3 (explicit ea): the literal rPr/ea typeface lands in family_east_asia.
    let renderer = PmlRenderer::from_pptx(cjk_pptx()).expect("cjk pptx loads");
    let fc = cjk_font_cache();
    let pages = renderer.build_pages_for_test(&fc);
    let shape = pages[0]
        .shapes
        .iter()
        .find(|s| !s.paragraphs.is_empty())
        .expect("text box present");
    let runs: Vec<&shape::SlideRun> =
        shape.paragraphs.iter().flat_map(|p| &p.runs).collect();
    assert_eq!(runs.len(), 3);

    assert_eq!(runs[0].family, "等线", "un-fonted run falls to the theme minor latin");
    assert_eq!(runs[0].family_east_asia, "等线", "empty theme ea delegates to the latin face");
    assert_eq!(runs[1].family, "等线", "+mn-lt theme ref resolves (never the literal string)");
    assert_eq!(runs[1].family_east_asia, "等线", "+mn-ea (empty slot) → latin face");
    assert_eq!(runs[2].family_east_asia, "SimSun", "explicit rPr/ea typeface surfaces");
}

#[test]
fn cjk_font_families_export_theme_and_ea_names() {
    // The JS font plan must SEE the CJK names (等线 → "Noto Sans CJK" in
    // font-resolver.ts) or the CJK face is never fetched (the observed symptom
    // was that only Carlito loaded). Theme major+minor latin are seeded;
    // CJK-bearing runs add their resolved ea family; the literal "+mn-lt" ref
    // string must NOT leak out.
    let renderer = PmlRenderer::from_pptx(cjk_pptx()).expect("cjk pptx loads");
    let families = renderer.font_families();
    assert!(families.iter().any(|f| f == "等线"), "theme minor latin exported: {families:?}");
    assert!(families.iter().any(|f| f == "等线 Light"), "theme major latin exported: {families:?}");
    assert!(families.iter().any(|f| f == "SimSun"), "explicit ea family exported: {families:?}");
    assert!(
        !families.iter().any(|f| f.starts_with('+')),
        "theme refs must resolve, not leak: {families:?}"
    );
}

#[test]
fn run_slot_segments_split_cjk_from_latin() {
    // The per-slot pen loop's segmentation (the wml/linebreak.rs pattern): a
    // mixed run splits at every CJK↔latin boundary, CJK segments carrying the
    // EA family, latin segments the default family.
    let run = shape::SlideRun {
        text: "支持npm下载".to_string(),
        font_pt: 14.0,
        bold: false,
        italic: false,
        family: "carlito".to_string(),
        family_east_asia: "noto sans cjk".to_string(),
        family_complex: "carlito".to_string(),
        color: "#000000".to_string(),
    };
    let segs = super::run_slot_segments(&run);
    assert_eq!(
        segs,
        vec![
            ("支持".to_string(), "noto sans cjk".to_string()),
            ("npm".to_string(), "carlito".to_string()),
            ("下载".to_string(), "noto sans cjk".to_string()),
        ]
    );
}

#[test]
fn cjk_estimate_is_full_width_per_char() {
    // With NO face loaded (shaping unavailable), the measured advance must
    // budget ~1.0 em per CJK char (the canvas system-font fallback paints CJK
    // full-width) and ~0.5 em per latin char — a uniform 0.5 em
    // estimate would start the next run INSIDE the painted CJK glyphs.
    use crate::engine::fonts::FontCache;
    use crate::engine::units::pt_to_px;
    let run = shape::SlideRun {
        text: "支持npm下载".to_string(),
        font_pt: 18.0,
        bold: false,
        italic: false,
        family: "carlito".to_string(),
        family_east_asia: "noto sans cjk".to_string(),
        family_complex: "carlito".to_string(),
        color: "#000000".to_string(),
    };
    let adv = super::measure_run_advance(&run, &FontCache::new(), &crate::engine::fonts::FaceCache::new());
    let em = pt_to_px(18.0);
    let expected = 4.0 * em * 1.0 + 3.0 * em * 0.5; // 4 CJK chars + "npm"
    assert!(
        (adv - expected).abs() < 0.01,
        "estimate budgets 1 em/CJK char: got {adv}, want {expected}"
    );
}

// ─── custGeom freeform overlay ──────────────────────────────────────────────

// A slide whose only content shape carries an `<a:custGeom>` freeform path
// (moveTo + lnTo + cubicBezTo + close) — the markup that trips the fork's
// `<a:moveTo>`/`<a:pt>` parse bug, so loading this deck MUST go through RULE 3's
// custGeom strip, after which the overlay raw-scans the ORIGINAL path back. The shape box
// is 100×100 px (914400 EMU = 96 px ≈ 100; we use 952500 EMU = 100 px exactly).
// Path-local space is 100×100 too (sx=sy=1), so PathCmds equal the literal pts.
const CUSTGEOM_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Freeform 1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="0" y="0"/><a:ext cx="952500" cy="952500"/></a:xfrm>
          <a:custGeom>
            <a:avLst/><a:gdLst/><a:ahLst/><a:rect l="0" t="0" r="100" b="100"/>
            <a:pathLst>
              <a:path w="100" h="100">
                <a:moveTo><a:pt x="0" y="0"/></a:moveTo>
                <a:lnTo><a:pt x="100" y="0"/></a:lnTo>
                <a:cubicBezTo><a:pt x="100" y="50"/><a:pt x="50" y="100"/><a:pt x="0" y="100"/></a:cubicBezTo>
                <a:close/>
              </a:path>
            </a:pathLst>
          </a:custGeom>
          <a:solidFill><a:srgbClr val="2563EB"/></a:solidFill>
        </p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:endParaRPr lang="en-US"/></a:p></p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

fn custgeom_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", CUSTGEOM_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

#[test]
fn custgeom_overlay_yields_the_real_freeform_path_not_a_rect() {
    use crate::engine::fonts::FontCache;
    use crate::engine::geom::PathCmd::*;
    // Loading this deck exercises RULE 3 (the custGeom strip) then the overlay
    // (raw-scanning the original path back). If either fails, from_pptx errors or
    // the shape stays a rect.
    let renderer = PmlRenderer::from_pptx(custgeom_pptx()).expect("custGeom pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let page = &pages[0];
    // The freeform shape (id=2) is the only content shape.
    let sh = page
        .shapes
        .iter()
        .find(|s| s.geometry.is_some())
        .expect("the freeform shape must carry overlaid geometry, not a rect fallback");
    // Exact PathCmd sequence: 100×100 path → 100×100 px box (1:1 scale).
    assert_eq!(
        sh.geometry.as_deref().unwrap(),
        &[
            MoveTo { x: 0.0, y: 0.0 },
            LineTo { x: 100.0, y: 0.0 },
            Cubic { x1: 100.0, y1: 50.0, x2: 50.0, y2: 100.0, x: 0.0, y: 100.0 },
            Close,
        ],
        "the custGeom overlay must apply the real moveTo/lnTo/cubicBezTo/close path"
    );
    // unknown_preset must be clear (a successful overlay, not the rect signal).
    assert!(sh.unknown_preset.is_none());
}

#[test]
fn custgeom_overlay_paints_non_rect_pixels() {
    use crate::engine::fonts::FontCache;
    use crate::engine::geom::testutil::point_in_path;
    let renderer = PmlRenderer::from_pptx(custgeom_pptx()).expect("custGeom pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let sh = pages[0]
        .shapes
        .iter()
        .find(|s| s.geometry.is_some())
        .expect("overlaid geometry");
    let cmds = sh.geometry.as_deref().unwrap();
    // The path covers the top-left bulk; the FAR top-right corner is outside the
    // curved hypotenuse — a rect fallback would have filled it (proves the
    // freeform actually replaced the placeholder rect).
    assert!(point_in_path(cmds, 5.0, 5.0), "interior near TL filled");
    assert!(
        !point_in_path(cmds, 99.0, 95.0),
        "bottom-right past the bezier is empty (a rect would fill it)"
    );
}

#[test]
fn connectors_produce_dashed_line_with_arrowheads() {
    // The slide's two `p:cxnSp` connectors walk into the page's connector
    // list. The plain one strokes a solid red line (no arrowheads); the second
    // is a dashed black line carrying a head + tail arrowhead triangle.
    use crate::engine::fonts::FontCache;
    use crate::engine::geom::PathCmd;
    use crate::engine::units::emu_to_px;
    let renderer = PmlRenderer::from_pptx(connectors_pptx()).expect("connector pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let page = &pages[0];

    assert_eq!(page.connectors.len(), 2, "both cxnSp walked into the page");

    // Plain connector: solid red, full-width horizontal line, no arrowheads.
    let plain = &page.connectors[0];
    assert_eq!(plain.x, emu_to_px(0));
    assert_eq!(plain.y, emu_to_px(900_000));
    assert_eq!(plain.outline.color, "#c00000");
    assert!(plain.outline.dash.is_none(), "plain connector strokes solid");
    assert!(plain.head_arrow.is_none() && plain.tail_arrow.is_none());
    // The path is the box-spanning horizontal segment (cy=0 → h=0).
    assert_eq!(
        plain.path,
        vec![PathCmd::MoveTo { x: 0.0, y: 0.0 }, PathCmd::LineTo { x: emu_to_px(9_144_000), y: 0.0 }]
    );

    // Arrow connector: dashed black, BOTH a head (triangle) and a tail (arrow)
    // arrowhead. Each arrowhead is a closed triangle (3 corners + Close).
    let arrow = &page.connectors[1];
    assert_eq!(arrow.outline.color, "#111111");
    assert!(arrow.outline.dash.is_some(), "prstDash=dash → a dash pattern");
    let head = arrow.head_arrow.as_deref().expect("headEnd triangle");
    let tail = arrow.tail_arrow.as_deref().expect("tailEnd triangle");
    let tip_count = |p: &[PathCmd]| p.iter().filter(|c| matches!(c, PathCmd::MoveTo { .. } | PathCmd::LineTo { .. })).count();
    assert_eq!(tip_count(head), 3, "head is a 3-point triangle");
    assert_eq!(tip_count(tail), 3, "tail is a 3-point triangle");
    // The head sits at the line START (local 0,0); the tail at the END (w,0).
    assert!(head.iter().any(|c| matches!(c, PathCmd::MoveTo { x, y } if *x == 0.0 && *y == 0.0)),
        "head triangle apex at the line start");
    assert!(tail.iter().any(|c| matches!(c, PathCmd::MoveTo { x, .. } if (*x - emu_to_px(8_000_000)).abs() < 1e-6)),
        "tail triangle apex at the line end");
}

#[test]
fn render_page_clipped_records_style_opcodes_for_worker_replay() {
    // Worker-render guard: painting a PML slide through a RecordingPainter (the
    // off-thread renderPageDisplayList path) must emit the NEW style opcodes for
    // real, so worker mode replays the true slide (dashes/gradients/patterns), not
    // a solid approximation. The connector slide carries an `<a:prstDash val="dash"/>`
    // outline, so the record must contain a `SetLineDash`. If the ctx->Painter
    // migration ever stops driving an op through the trait (or the PML record path
    // regresses to a reject), this goes RED.
    use crate::engine::displaylist::{DrawCmd, RecordingPainter};
    use crate::engine::fonts::FontCache;
    use std::cell::RefCell;
    use std::collections::HashMap;
    let renderer = PmlRenderer::from_pptx(connectors_pptx()).expect("connector pptx must load");
    let fc = FontCache::new();
    let pml_pages = RefCell::new(renderer.build_pages_for_test(&fc));
    let chart_xml: RefCell<HashMap<String, Vec<u8>>> = RefCell::new(HashMap::new());
    let rec = RecordingPainter::new();
    renderer
        .render_page_clipped(0, &pml_pages, &fc, &chart_xml, &rec, None, None, None, None)
        .expect("PML slide records through the RecordingPainter");
    let list = rec.take();
    assert!(!list.is_empty(), "the slide must record draw commands off-thread");
    assert!(
        list.iter().any(|c| matches!(c, DrawCmd::SetLineDash(segs) if !segs.is_empty())),
        "the connector's prstDash must record a real SetLineDash opcode, not a solid line"
    );
}

// ───────────────────────── SmartArt diagram fallback ────────────────────────
//
// A slide carrying ONE `p:graphicFrame` that references a SmartArt diagram (the
// diagram graphicData uri + `dgm:relIds@r:dm="rId2"`), plus the pre-rendered
// DrawingML fallback chain: slide rels rId2 → data1.xml (whose
// `dsp:dataModelExt@relId="rId6"`) and rId6 → drawing1.xml. The drawing's
// `dsp:spTree` holds TWO `dsp:sp`: a roundRect (no text) and an ellipse with a
// text run. The frame box is off (1M,2M) ext 4M×2M; the drawing's two shapes
// span (0,0)…(4M,2M) so drawing-space == frame-ext (pure translate by the off,
// scale 1×). Each `dsp:sp` must become a positioned `SlideShape`.

const DGM_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:graphicFrame>
        <p:nvGraphicFramePr><p:cNvPr id="5" name="Diagram 4"><a:extLst><a:ext uri="{FF2B5EF4-FFF2-40B4-BE49-F238E27FC236}"/></a:extLst></p:cNvPr><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>
        <p:xfrm><a:off x="1000000" y="2000000"/><a:ext cx="4000000" cy="2000000"/></p:xfrm>
        <a:graphic>
          <a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/diagram">
            <dgm:relIds xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:dm="rId2" r:lo="rId3" r:qs="rId4" r:cs="rId5"/>
          </a:graphicData>
        </a:graphic>
      </p:graphicFrame>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

const DGM_SLIDE_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/diagramData" Target="../diagrams/data1.xml"/>
  <Relationship Id="rId6" Type="http://schemas.microsoft.com/office/2007/relationships/diagramDrawing" Target="../diagrams/drawing1.xml"/>
</Relationships>"#;

// The diagram DATA part: only the `dsp:dataModelExt` matters (its relId names the
// drawing fallback rel on the SLIDE). The rest is elided (we never run the live
// layout algorithm).
const DGM_DATA: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<dgm:dataModel xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram">
  <dgm:extLst>
    <a:ext xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" uri="http://schemas.microsoft.com/office/drawing/2008/diagram">
      <dsp:dataModelExt xmlns:dsp="http://schemas.microsoft.com/office/drawing/2008/diagram" relId="rId6" minVer="http://schemas.openxmlformats.org/drawingml/2006/diagram"/>
    </a:ext>
  </dgm:extLst>
</dgm:dataModel>"#;

// The pre-rendered drawing fallback: a `dsp:spTree` with a roundRect (fill-only,
// no text) at (0,0) 2M×2M and an ellipse with a green fill + a text run at
// (2M,0) 2M×2M. schemeClr on the ellipse fill resolves through the theme
// (accent1 → the master/theme). srgbClr on the roundRect is theme-free.
const DGM_DRAWING: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<dsp:drawing xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" xmlns:dsp="http://schemas.microsoft.com/office/drawing/2008/diagram" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <dsp:spTree>
    <dsp:nvGrpSpPr><dsp:cNvPr id="0" name=""/><dsp:cNvGrpSpPr/></dsp:nvGrpSpPr>
    <dsp:grpSpPr/>
    <dsp:sp modelId="{AAAA}">
      <dsp:nvSpPr><dsp:cNvPr id="0" name=""/><dsp:cNvSpPr/></dsp:nvSpPr>
      <dsp:spPr>
        <a:xfrm><a:off x="0" y="0"/><a:ext cx="2000000" cy="2000000"/></a:xfrm>
        <a:prstGeom prst="roundRect"><a:avLst/></a:prstGeom>
        <a:solidFill><a:srgbClr val="C0504D"/></a:solidFill>
      </dsp:spPr>
      <dsp:txBody><a:bodyPr/><a:lstStyle/><a:p><a:endParaRPr lang="en-US"/></a:p></dsp:txBody>
    </dsp:sp>
    <dsp:sp modelId="{BBBB}">
      <dsp:nvSpPr><dsp:cNvPr id="0" name=""/><dsp:cNvSpPr/></dsp:nvSpPr>
      <dsp:spPr>
        <a:xfrm><a:off x="2000000" y="0"/><a:ext cx="2000000" cy="2000000"/></a:xfrm>
        <a:prstGeom prst="ellipse"><a:avLst/></a:prstGeom>
        <a:solidFill><a:srgbClr val="00B050"/></a:solidFill>
      </dsp:spPr>
      <dsp:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>NodeText</a:t></a:r></a:p></dsp:txBody>
    </dsp:sp>
  </dsp:spTree>
</dsp:drawing>"#;

/// Build a deck whose single slide carries a SmartArt diagram graphicFrame + its
/// pre-rendered drawing fallback parts. Reuses the base master/layout/presentation
/// (the diagram parts need no content-type Override — they are read as raw bytes).
fn diagram_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", DGM_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", DGM_SLIDE_RELS),
        ("ppt/diagrams/data1.xml", DGM_DATA),
        ("ppt/diagrams/drawing1.xml", DGM_DRAWING),
    ])
}

#[test]
fn smartart_diagram_renders_fallback_dsp_shapes() {
    // The diagram graphicFrame resolves its drawing fallback and builds one
    // `SlideShape` per `dsp:sp` — a roundRect (fill #c0504d, no text) at the
    // frame off, and an ellipse (fill #00b050, run "NodeText") translated by the
    // frame off. Drawing space (0,0)…(4M,2M) == frame ext 4M×2M → scale 1×,
    // translate by off (1M,2M).
    use crate::engine::fonts::FontCache;
    use crate::engine::geom::PathCmd;
    use crate::engine::units::emu_to_px;
    let renderer = PmlRenderer::from_pptx(diagram_pptx()).expect("diagram pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let page = &pages[0];

    // Exactly the two dsp:sp became shapes (the frame itself contributes none).
    assert_eq!(page.shapes.len(), 2, "two dsp:sp → two slide shapes");

    // The roundRect: fill-only, positioned at the frame off, box 2M×2M, and its
    // geometry is the tier-1 roundRect path (not the rect fallback).
    let rr = page
        .shapes
        .iter()
        .find(|s| s.solid_fill_hex() == Some("#c0504d"))
        .expect("roundRect dsp:sp present with its srgb fill");
    assert_eq!(rr.x, emu_to_px(1_000_000), "roundRect off x = frame off + child 0");
    assert_eq!(rr.y, emu_to_px(2_000_000), "roundRect off y = frame off + child 0");
    assert_eq!(rr.w, emu_to_px(2_000_000), "roundRect box width (scale 1×)");
    assert_eq!(rr.h, emu_to_px(2_000_000));
    let geo = rr.geometry.as_ref().expect("roundRect built a tier-1 geometry");
    // A roundRect path carries arc-rounded corners (the bare rect fallback would
    // be straight MoveTo/LineTo only) — assert it has a curve, proving the dsp:sp
    // prstGeom resolved through the same preset builder.
    assert!(
        geo.iter().any(|c| matches!(c, PathCmd::Arc { .. } | PathCmd::Cubic { .. } | PathCmd::Quad { .. })),
        "roundRect geometry has a rounded corner curve, proving prstGeom resolved"
    );

    // The ellipse: green fill, the "NodeText" run, translated by the frame off.
    let el = page
        .shapes
        .iter()
        .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == "NodeText"))
        .expect("ellipse dsp:sp present with its text run");
    assert_eq!(el.solid_fill_hex(), Some("#00b050"), "ellipse green fill resolved");
    assert_eq!(el.x, emu_to_px(3_000_000), "ellipse off x = frame off 1M + child 2M");
    assert_eq!(el.y, emu_to_px(2_000_000), "ellipse off y = frame off 2M + child 0");
    assert_eq!(el.w, emu_to_px(2_000_000));
    assert_eq!(el.h, emu_to_px(2_000_000));
}

// Three rect shapes, each a 2M×2M box, differing only in bodyPr@anchor: ctr / b
// / absent. Pins the vertical-anchor parse (an earlier version top-pinned
// shape text regardless of anchor="ctr", misplacing captions).
const ANCHOR_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree>
    <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
    <p:grpSpPr/>
    <p:sp><p:nvSpPr><p:cNvPr id="2" name="Ctr"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
      <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="2000000" cy="2000000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
      <p:txBody><a:bodyPr anchor="ctr"/><a:p><a:r><a:rPr lang="en-US" sz="1400"/><a:t>CenteredCap</a:t></a:r></a:p></p:txBody>
    </p:sp>
    <p:sp><p:nvSpPr><p:cNvPr id="3" name="Bot"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
      <p:spPr><a:xfrm><a:off x="2200000" y="0"/><a:ext cx="2000000" cy="2000000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
      <p:txBody><a:bodyPr anchor="b"/><a:p><a:r><a:rPr lang="en-US" sz="1400"/><a:t>BottomCap</a:t></a:r></a:p></p:txBody>
    </p:sp>
    <p:sp><p:nvSpPr><p:cNvPr id="4" name="Top"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
      <p:spPr><a:xfrm><a:off x="4400000" y="0"/><a:ext cx="2000000" cy="2000000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
      <p:txBody><a:bodyPr/><a:p><a:r><a:rPr lang="en-US" sz="1400"/><a:t>TopCap</a:t></a:r></a:p></p:txBody>
    </p:sp>
  </p:spTree></p:cSld>
</p:sld>"#;

fn anchor_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", ANCHOR_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

#[test]
fn shape_text_honors_bodypr_vertical_anchor() {
    use crate::engine::fonts::FontCache;
    use crate::pml::shape::VAnchor;
    let renderer = PmlRenderer::from_pptx(anchor_pptx()).expect("anchor pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let find = |t: &str| {
        pages[0]
            .shapes
            .iter()
            .find(|s| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text == t))
            .unwrap_or_else(|| panic!("shape with run {t}"))
    };
    assert_eq!(find("CenteredCap").anchor, VAnchor::Center, "bodyPr anchor=ctr → Center");
    assert_eq!(find("BottomCap").anchor, VAnchor::Bottom, "bodyPr anchor=b → Bottom");
    assert_eq!(find("TopCap").anchor, VAnchor::Top, "absent bodyPr anchor → Top default");
}

// ─── Full-text search (PML arm of `Viewer::search_page`) ────────────────────
//
// These exercise the READ-ONLY run-walk the PML `search_page` arm performs:
// shapes (+ table cells) → paragraphs → runs → `crate::find_matches`, one match
// per `(start,end)` so the per-slide count is right. They drive the same crate
// `find_matches`/`SearchOpts` the live arm uses; the arm itself lives on
// `Viewer` (web-sys), so we assert its core over the built page model here.

/// Count matches the PML `search_page` arm would emit for `query`/`opts` over a
/// built page — the exact shape/table run-walk `Viewer::search_pml_slide` does.
fn pml_slide_match_count(
    page: &shape::SlidePage,
    query: &str,
    opts: crate::SearchOpts,
) -> usize {
    let mut n = 0;
    // Per-PARAGRAPH logical text (runs concatenated) — the granularity the live
    // arm + the paint pass both use, so a multi-word phrase spanning runs is found
    // and count == paint.
    for sh in &page.shapes {
        for para in &sh.paragraphs {
            let ptext: String = para.runs.iter().map(|r| r.text.as_str()).collect();
            n += crate::find_matches(&ptext, query, opts).len();
        }
    }
    for tbl in &page.tables {
        for cell in &tbl.cells {
            for para in &cell.paragraphs {
                let ptext: String = para.runs.iter().map(|r| r.text.as_str()).collect();
                n += crate::find_matches(&ptext, query, opts).len();
            }
        }
    }
    n
}

#[test]
fn search_hits_known_shape_text() {
    // "Hello Title" + "First/Second bullet" live in the minimal deck's shapes.
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(minimal_pptx()).expect("minimal pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let page = &pages[0];
    let opts = crate::SearchOpts::default();

    assert!(pml_slide_match_count(page, "Title", opts) > 0, "title text is found");
    // Multi-word phrase spanning the paragraph's logical text (per-run matching
    // would miss it once Square wrap tokenizes into per-word sub-runs).
    assert!(
        pml_slide_match_count(page, "Hello Title", opts) > 0,
        "multi-word phrase is found via per-paragraph logical text"
    );
    // "bullet" occurs in both "First bullet" and "Second bullet" → two matches.
    assert_eq!(pml_slide_match_count(page, "bullet", opts), 2, "one match per occurrence");
}

#[test]
fn search_is_case_insensitive_by_default() {
    // Default opts are case-insensitive: a lowercased query still hits the
    // mixed-case run text.
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(minimal_pptx()).expect("minimal pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let page = &pages[0];

    let ci = crate::SearchOpts::default();
    assert!(pml_slide_match_count(page, "hello title", ci) > 0, "lowercased query hits");

    // Case-sensitive opts: the lowercased query must NOT hit "Hello Title".
    let cs = crate::SearchOpts { case_sensitive: true, ..crate::SearchOpts::default() };
    assert_eq!(pml_slide_match_count(page, "hello title", cs), 0, "case-sensitive misses");
    assert!(pml_slide_match_count(page, "Hello Title", cs) > 0, "exact case hits");
}

#[test]
fn search_no_match_and_empty_query_yield_zero() {
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(minimal_pptx()).expect("minimal pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let page = &pages[0];
    let opts = crate::SearchOpts::default();

    assert_eq!(pml_slide_match_count(page, "Nonexistent", opts), 0, "absent text → no match");
    assert_eq!(pml_slide_match_count(page, "", opts), 0, "empty query → no match");
}

#[test]
fn search_hits_table_cell_text() {
    // Table cells carry the same SlideParagraph/SlideRun model as shapes, so the
    // run-walk hits cell text too. The table fixture's cells carry "R1C1"-style
    // text (asserted by `table_cell_fill_borders_and_text`).
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(table_pptx()).expect("table pptx loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let page = &pages[0];
    let opts = crate::SearchOpts::default();

    // Whatever the first cell's first run text is, searching it must hit ≥1.
    let needle = page.tables[0].cells[0].paragraphs[0].runs[0].text.clone();
    assert!(!needle.is_empty(), "fixture cell has text to search");
    assert!(pml_slide_match_count(page, &needle, opts) > 0, "cell text is searchable");
}

#[test]
fn search_does_not_mutate_the_page_model() {
    // The read-only invariant: searching never touches geometry/text. Snapshot a
    // shape's box + run texts, run several searches, assert nothing moved.
    use crate::engine::fonts::FontCache;
    let renderer = PmlRenderer::from_pptx(minimal_pptx()).expect("minimal pptx must load");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    let page = &pages[0];
    let opts = crate::SearchOpts::default();

    let before_boxes: Vec<(f64, f64, f64, f64)> =
        page.shapes.iter().map(|s| (s.x, s.y, s.w, s.h)).collect();
    let before_texts: Vec<String> = page
        .shapes
        .iter()
        .flat_map(|s| s.paragraphs.iter().flat_map(|p| p.runs.iter().map(|r| r.text.clone())))
        .collect();

    let _ = pml_slide_match_count(page, "Title", opts);
    let _ = pml_slide_match_count(page, "bullet", opts);
    let _ = pml_slide_match_count(page, "missing", opts);

    let after_boxes: Vec<(f64, f64, f64, f64)> =
        page.shapes.iter().map(|s| (s.x, s.y, s.w, s.h)).collect();
    let after_texts: Vec<String> = page
        .shapes
        .iter()
        .flat_map(|s| s.paragraphs.iter().flat_map(|p| p.runs.iter().map(|r| r.text.clone())))
        .collect();

    assert_eq!(before_boxes, after_boxes, "search mutated no shape box");
    assert_eq!(before_texts, after_texts, "search mutated no run text");
}

/// The ACTIVE-match ordering guarantee. `search_pml_slide` numbers a slide's
/// matches as ONE flat list (all shapes' paragraphs, THEN all table cells'); the
/// paint walk visits shapes/cells one at a time and threads a running counter
/// (`count_paragraph_matches` for each shape/cell, in that exact order) to
/// recover each match's slide-global index. This test pins that the paint-order
/// accumulation lands the focused match at the SAME index the flat search list
/// assigns — i.e. paint match order == search match order. If they diverged, the
/// hotter active swipe would land on the WRONG occurrence.
#[test]
fn pml_active_index_paint_order_matches_search_order() {
    use crate::engine::fonts::FontCache;
    // "bullet" occurs twice (First bullet, Second bullet) — two distinct global
    // indices. The minimal deck lays them out as separate paragraphs in one shape.
    let renderer = PmlRenderer::from_pptx(minimal_pptx()).expect("minimal pptx must load");
    let page = &renderer.build_pages_for_test(&FontCache::new())[0];
    let opts = crate::SearchOpts::default();
    let needle = "bullet";

    let total = pml_slide_match_count(page, needle, opts);
    assert_eq!(total, 2, "fixture has two 'bullet' matches");

    // Rebuild the paint-walk's slide-global numbering from `count_paragraph_matches`
    // in the SAME shapes-then-cells order the paint walk uses, and record, for each
    // global index, the (container, paragraph, in-paragraph ordinal) it maps to.
    let sh = super::PmlSearchHighlight {
        needle,
        opts,
        fill: "#FFE066",
        alpha: 0.40,
        active: None,
    };
    // Per-shape match counts via the paint-walk counter.
    let shape_counts: Vec<usize> =
        page.shapes.iter().map(|s| super::count_paragraph_matches(&s.paragraphs, Some(&sh))).collect();
    let cell_counts: Vec<usize> = page
        .tables
        .iter()
        .flat_map(|t| t.cells.iter())
        .map(|c| super::count_paragraph_matches(&c.paragraphs, Some(&sh)))
        .collect();

    // The paint-walk accumulation must sum to the same total as the flat search.
    let walk_total: usize = shape_counts.iter().chain(cell_counts.iter()).sum();
    assert_eq!(
        walk_total, total,
        "paint-walk per-shape/cell counter sums to the same total as search_pml_slide"
    );

    // And the running prefix (the `match_base` each shape/cell would receive) is
    // monotone and contiguous — so each global index belongs to exactly one
    // container, with no gap or overlap. This is the invariant that makes
    // `sh.active == Some(global_mi)` select the right occurrence.
    let mut base = 0usize;
    let mut bases = Vec::new();
    for c in shape_counts.iter().chain(cell_counts.iter()) {
        bases.push(base);
        base += c;
    }
    assert!(bases.windows(2).all(|w| w[0] <= w[1]), "match bases are monotone: {bases:?}");
    assert_eq!(base, total, "final running base == total match count (contiguous numbering)");

    // The two matches occupy global indices 0 and 1 (both in the body shape's two
    // bullet paragraphs) — exactly the indices `search_pml_slide` emits.
    assert_eq!(walk_total, 2, "global indices 0 and 1 are both reachable");
}

/// Bullet lockstep guard: the walked logical is BULLET-PREFIXED ("1. …"), but
/// `search_pml_slide`/`count_paragraph_matches` number matches over the RAW
/// runs. The paint pass strips the prefix before matching (offset-shifted) —
/// pin that the stripped walked logical is byte-identical to the raw text, so
/// a numeric needle ("1") cannot match the "1." ordinal and desync the
/// slide-global active index.
#[test]
fn paint_search_text_excludes_the_bullet_prefix() {
    use crate::engine::fonts::FontCache;
    let fc = FontCache::new();
    let para = shape::SlideParagraph {
        runs: vec![wrap_run("1 apple and 1 pear", 10.0)],
        align: shape::ParaAlign::Left,
        bullet: Some("1.".to_string()),
        indent_first_px: 0.0,
        indent_rest_px: 0.0,
    };
    let walks =
        super::walk_paragraph_stack(&[para.clone()], 0.0, 0.0, 10_000.0, shape::WrapMode::Square, &fc);
    assert_eq!(walks[0].logical, "1. 1 apple and 1 pear", "walk logical is bullet-prefixed");

    // The paint pass's searchable slice (prefix "{bullet} " stripped).
    let prefix_len = para.bullet.as_ref().unwrap().len() + 1;
    let searchable = &walks[0].logical[prefix_len..];
    assert_eq!(searchable, "1 apple and 1 pear", "stripped slice == the raw runs");

    // Match parity with the search/count side (raw runs): the "1." ordinal must
    // not add a third "1" match.
    let opts = crate::SearchOpts::default();
    let raw: String = para.runs.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(
        crate::find_matches(searchable, "1", opts).len(),
        crate::find_matches(&raw, "1", opts).len(),
        "paint-side and search-side match counts agree for a numeric needle"
    );
    assert_eq!(crate::find_matches(searchable, "1", opts).len(), 2);
}



/// `blend_hex_50_str` (the painter's pattFill surface-miss fallback)
/// once gated on BYTE length before slicing `&h[0..2]`, so a document-controlled
/// `<a:srgbClr val="aé字"/>` — 6 bytes, 3 chars — panicked on the char
/// boundary. It must fall back to the foreground instead.
#[test]
fn blend_hex_50_str_multibyte_six_byte_hex_does_not_panic() {
    assert_eq!(super::blend_hex_50_str("#000000", "#FFFFFF"), "#7f7f7f");
    assert_eq!(super::blend_hex_50_str("aé字", "#FFFFFF"), "aé字");
    assert_eq!(super::blend_hex_50_str("#FFFFFF", "aé字"), "#FFFFFF");
}

/// O(1)-opens pin (synthetic deck, always runs): one `from_pptx` load of
/// an unrepaired deck performs exactly ONE OPC archive open, shared by the
/// geometry / theme / tableStyles / custGeom / diagram passes. (There is no
/// connector pass — connectors come off the already-parsed slide.)
/// An earlier version of the same load opened the archive 5 + N_slides times (plus 2 more
/// when pictures/charts were present), each over a fresh `bytes.to_vec()`.
#[test]
fn pptx_load_opens_one_opc_package_per_byte_image() {
    let bytes = minimal_pptx();
    let before = crate::engine::opc::OPEN_COUNT.with(|c| c.get());
    let r = PmlRenderer::from_pptx(bytes).expect("from_pptx");
    let opens = crate::engine::opc::OPEN_COUNT.with(|c| c.get()) - before;
    assert_eq!(opens, 1, "one shared OpcPackage per unrepaired pptx load, got {opens}");
    assert_eq!(r.slide_count(), 1);
}

/// The same pin over a REAL generated deck from `fixtures/ppt` (skip-if-absent,
/// the `title-body.pptx` precedent): multi-slide, so the retired per-slide
/// diagram open would alone have pushed the count past 5 + N.
#[test]
fn real_multi_slide_pptx_load_opens_one_opc_package() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/ppt/multi-slide.pptx"
    );
    let Ok(bytes) = std::fs::read(path) else {
        eprintln!("skipping: generated fixture absent (run generate-test-fixtures.ts)");
        return;
    };
    let before = crate::engine::opc::OPEN_COUNT.with(|c| c.get());
    let r = PmlRenderer::from_pptx(bytes).expect("from_pptx");
    let opens = crate::engine::opc::OPEN_COUNT.with(|c| c.get()) - before;
    assert_eq!(opens, 1, "one shared OpcPackage per real pptx load, got {opens}");
    assert!(r.slide_count() > 1, "multi-slide fixture");
}

/// Parse-once pin: one `from_pptx` load of an unrepaired deck calls
/// `Presentation::slides()` exactly ONCE.
///
/// `slides()` is uncached — the fork's `load_slide` re-reads and re-parses the
/// part on every call — and an earlier tolerant path called it twice: a probe
/// that drained every slide purely to surface a missing-attribute error, then a
/// second `Presentation` the caller drained for real. On a 200-slide synthetic
/// deck that pair measured 20.2 ms against 9.3 ms for a single parse: 10.9 ms,
/// 24% of the entire load, spent building a value that was dropped. It also kept
/// three copies of the deck bytes live at once instead of two.
#[test]
fn pptx_load_parses_each_slide_exactly_once() {
    let bytes = multi_slide_connectors_pptx();
    let before = super::SLIDE_PARSE_COUNT.with(|c| c.get());
    let r = PmlRenderer::from_pptx(bytes).expect("from_pptx");
    let parses = super::SLIDE_PARSE_COUNT.with(|c| c.get()) - before;
    assert_eq!(parses, 1, "an unrepaired pptx load parses the slide set once, got {parses}");
    // A multi-slide deck, so a per-slide re-parse would show up as N, not 2.
    assert_eq!(r.slide_count(), 3);
}

/// Connectors come off the ALREADY-PARSED slide (`Slide::inner()`), not
/// from a second typed parse of the slide part through the OPC package. The
/// swapped source must keep each slide's `p:cxnSp` on its OWN slide — the
/// retired pass produced an index-aligned `Vec<Vec<Connector>>` paired by
/// `slide_paths` order, the new one reads the slide in hand, and a misalignment
/// between the two orderings would silently move connectors between slides.
///
/// The fixture makes every slide's connector set DISTINCT (0 / 2 / 1, with a
/// slide-3-only colour), so a shifted or shared list cannot satisfy all three.
#[test]
fn multi_slide_connectors_stay_with_their_own_slide() {
    use crate::engine::fonts::FontCache;
    let renderer =
        PmlRenderer::from_pptx(multi_slide_connectors_pptx()).expect("multi-slide deck loads");
    let pages = renderer.build_pages_for_test(&FontCache::new());
    assert_eq!(pages.len(), 3, "three slides");

    assert!(pages[0].connectors.is_empty(), "slide 1 authors no cxnSp");

    assert_eq!(pages[1].connectors.len(), 2, "slide 2 authors both cxnSp");
    assert_eq!(
        pages[1].connectors[0].outline.color,
        "#c00000",
        "slide 2's plain connector keeps its own red stroke"
    );
    assert!(
        pages[1].connectors[1].outline.dash.is_some(),
        "slide 2's arrow connector keeps its prstDash"
    );

    assert_eq!(pages[2].connectors.len(), 1, "slide 3 authors one cxnSp");
    assert_eq!(
        pages[2].connectors[0].outline.color,
        "#00b0f0",
        "slide 3's connector keeps the colour authored on SLIDE 3"
    );
}

/// Slide 3 of [`multi_slide_connectors_pptx`]: exactly one connector, in a
/// colour no other slide uses, so it cannot be confused with slide 2's pair.
const CXN_SLIDE_3: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:cxnSp>
        <p:nvCxnSpPr><p:cNvPr id="12" name="Slide 3 Connector"/><p:cNvCxnSpPr/><p:nvPr/></p:nvCxnSpPr>
        <p:spPr>
          <a:xfrm><a:off x="200000" y="2700000"/><a:ext cx="7000000" cy="0"/></a:xfrm>
          <a:prstGeom prst="line"><a:avLst/></a:prstGeom>
          <a:ln w="25400"><a:solidFill><a:srgbClr val="00B0F0"/></a:solidFill></a:ln>
        </p:spPr>
      </p:cxnSp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

/// A three-slide deck: [`SLIDE`] (no connectors), [`CXN_SLIDE`] (two), and
/// [`CXN_SLIDE_3`] (one). The only synthetic MULTI-slide deck in this module —
/// every other fixture is single-slide, where per-slide alignment is vacuous.
fn multi_slide_connectors_pptx() -> Vec<u8> {
    const CT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slides/slide2.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slides/slide3.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
</Types>"#;
    const PRES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
  <p:sldIdLst><p:sldId id="256" r:id="rId2"/><p:sldId id="257" r:id="rId3"/><p:sldId id="258" r:id="rId4"/></p:sldIdLst>
  <p:sldSz cx="9144000" cy="6858000" type="screen4x3"/>
  <p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"#;
    const PRES_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/>
  <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide3.xml"/>
</Relationships>"#;
    build_pptx(&[
        ("[Content_Types].xml", CT),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRES),
        ("ppt/_rels/presentation.xml.rels", PRES_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
        ("ppt/slides/slide2.xml", CXN_SLIDE),
        ("ppt/slides/_rels/slide2.xml.rels", SLIDE_RELS),
        ("ppt/slides/slide3.xml", CXN_SLIDE_3),
        ("ppt/slides/_rels/slide3.xml.rels", SLIDE_RELS),
    ])
}

// ─── The PML recorded-paint-stream golden ───────────────────────────────────
//
// WHY, measured: with this pin removed, inflating what
// `measure_segment_advance` returns from `faces.shape` by 5% — and again by 4× —
// left ALL 96 other PML tests green, and so did rewriting a painted run's text.
// PML's text tests assert SHAPE, not value (`lines.len() > 1`, "contains
// alpha"), so no value the shaper produced was pinned anywhere. Compounding it:
// `FontCache::new()` — an EMPTY cache — appears 63 times in this file, and with
// no face loaded `measure_segment_advance` never reaches `faces.shape` at all;
// it falls to the 0.5/1.0-em estimate. The shaping site was reached by tests and
// asserted by none.
//
// This deck is built to make the advance load-bearing in three independent ways,
// so no single alignment path carries the pin alone:
//   * a CENTRED paragraph of two runs — `pen_x` starts at
//     `line_x + (line_avail - line_w)/2`, so the whole line slides on any width
//     change, and run 2's origin is run 1's measured end;
//   * a RIGHT-aligned paragraph — pen is `line_x + (line_avail - line_w)`;
//   * a LEFT-aligned paragraph long enough to WRAP inside the box, so the
//     greedy fill itself is pinned: change the advance and the break moves,
//     which changes the painted strings, not just their coordinates.
// And it loads a REAL face under the runs' declared family, so `faces.shape` is
// the branch under test rather than the estimate fallback.

const OPS_TEXT_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="TextBox 1"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="685800" y="457200"/><a:ext cx="4114800" cy="3429000"/></a:xfrm></p:spPr>
        <p:txBody><a:bodyPr wrap="square"/><a:lstStyle/>
          <a:p>
            <a:pPr algn="ctr"/>
            <a:r><a:rPr lang="en-US" sz="1800" b="1"><a:latin typeface="Calibri"/></a:rPr><a:t>Byte</a:t></a:r>
            <a:r><a:rPr lang="en-US" sz="1800"><a:latin typeface="Calibri"/></a:rPr><a:t>-identity</a:t></a:r>
          </a:p>
          <a:p>
            <a:pPr algn="r"/>
            <a:r><a:rPr lang="en-US" sz="1400"><a:latin typeface="Calibri"/></a:rPr><a:t>right edge</a:t></a:r>
          </a:p>
          <a:p>
            <a:r><a:rPr lang="en-US" sz="1400"><a:latin typeface="Calibri"/></a:rPr><a:t>The quick brown fox jumps over the lazy dog and keeps going well past the edge of this box</a:t></a:r>
          </a:p>
        </p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

fn ops_text_pptx() -> Vec<u8> {
    build_pptx(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("ppt/presentation.xml", PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", SLIDE_MASTER_RELS),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", SLIDE_LAYOUT_RELS),
        ("ppt/slides/slide1.xml", OPS_TEXT_SLIDE),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS),
    ])
}

/// A FontCache with the Arimo test face under "Calibri" — the family the deck's
/// runs declare AND `DEFAULT_FONT_FAMILY`, so BOTH the named resolve and the
/// fallback resolve inside `measure_segment_advance` hit a real face. Mirrors
/// `sml::tests::calibri_font_cache`; PML's own `cjk_font_cache` loads the same
/// file under the CJK family names.
fn calibri_font_cache() -> crate::engine::fonts::FontCache {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
    let bytes = std::fs::read(format!("{dir}/Arimo-Regular.ttf")).unwrap_or_else(|_| {
        panic!("test font missing: Arimo-Regular.ttf\n  run: ./scripts/fetch-test-fonts.sh")
    });
    let mut fc = crate::engine::fonts::FontCache::new();
    fc.insert("Calibri", bytes, 400, false).expect("insert Arimo as Calibri");
    fc
}

#[test]
fn slide_paint_stream_golden_shaped_text() {
    use crate::engine::displaylist::{DrawCmd, RecordingPainter};
    use crate::ops_golden::assert_ops_golden;
    use std::cell::RefCell;
    use std::collections::HashMap;
    let renderer = PmlRenderer::from_pptx(ops_text_pptx()).expect("ops-text pptx must load");
    let fc = calibri_font_cache();
    let pml_pages = RefCell::new(renderer.build_pages_for_test(&fc));
    let chart_xml: RefCell<HashMap<String, Vec<u8>>> = RefCell::new(HashMap::new());
    let rec = RecordingPainter::new();
    renderer
        .render_page_clipped(0, &pml_pages, &fc, &chart_xml, &rec, None, None, None, None)
        .expect("PML slide records through the RecordingPainter");
    let list = rec.take();
    // Guard the guard: a golden over an EMPTY stream is the vacuous-pin shape
    // this whole net exists to stop, and it would still be byte-stable forever.
    let texts = list.iter().filter(|c| matches!(c, DrawCmd::FillText { .. })).count();
    assert!(texts >= 4, "the deck must paint wrapped+aligned text, got {texts} FillText ops");
    assert_ops_golden(
        "pml-slide-text",
        &list,
        "the painted TEXT or the pen it was painted at moved. PML has no geometry pin \
         behind this one, so nothing else will say so: a shaped advance surfaces here as \
         the centred/right-aligned pen origin and as the greedy-fill break point, and a \
         change to the run text, the font-slot segmentation or the draw order surfaces as \
         the string itself",
    );
}

// ── the worker-record search-highlight regression ────────────────────────────
//
// Shipped bug (reproduced): PML search returned correct
// match counts but painted NO highlight rects in worker mode, while docx
// worked through the identical pipeline. Cause: both Viewer-level display-list
// record paths passed `None` for search/selection/object — an early floor that
// predates PML search. The state now threads through `Viewer::record_pml_page`
// (one spelling, both sites); this test drives the REAL record path with an
// armed query and asserts highlight paint reaches the recorded stream.
#[test]
fn viewer_record_path_carries_search_highlights() {
    use crate::engine::displaylist::{DrawCmd, RecordingPainter};
    let mut viewer = crate::Viewer::new().expect("Viewer::new");
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
    let bytes = std::fs::read(format!("{dir}/Arimo-Regular.ttf")).unwrap_or_else(|_| {
        panic!("test font missing — run: ./scripts/fetch-test-fonts.sh")
    });
    viewer.load_font("Calibri", bytes, 400, false).expect("font");
    viewer.load_pptx(ops_text_pptx()).expect("pptx");
    // run_layout_native is the WML entry (its guard builds a JsValue, which
    // panics off-wasm); PML pages build through the same per-slide constructor
    // the async layout uses — the pattern every test in this file follows.
    if let crate::LoadedDoc::Pml(p) = &viewer.loaded {
        *viewer.pml_pages.borrow_mut() = p.build_pages_for_test(&viewer.font_cache);
    } else {
        panic!("fixture must load as PML");
    }

    // Arm the query DIRECTLY on the state the paint pass reads (bypassing the
    // JsValue-taking setter, which is wasm-facing): "quick" appears in slide 1.
    *viewer.search_query.borrow_mut() = "quick".to_string();

    let rec = RecordingPainter::new();
    let handled = viewer.record_pml_page(0, &rec, None).expect("record");
    assert!(handled, "a loaded pptx must take the PML record arm");
    let ops = rec.take();
    let highlight_fills = ops
        .iter()
        .filter(|c| matches!(c, DrawCmd::SetFillStyle(v) if v == "#FFE066"))
        .count();
    assert!(
        highlight_fills >= 1,
        "an armed search query must paint highlight fills into the RECORDED \
         stream (got {highlight_fills}) — a zero here is the worker-mode \
         no-visible-highlights bug"
    );

    // And the inverse: no query, no highlight ops — the fix must not leak
    // highlight paint into search-free records (the ops goldens depend on it).
    viewer.search_query.borrow_mut().clear();
    let rec2 = RecordingPainter::new();
    viewer.record_pml_page(0, &rec2, None).expect("record");
    let stray = rec2
        .take()
        .iter()
        .filter(|c| matches!(c, DrawCmd::SetFillStyle(v) if v == "#FFE066"))
        .count();
    assert_eq!(stray, 0, "no query must mean no highlight fills");
}
