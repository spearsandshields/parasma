#!/usr/bin/env bun
/**
 * Generates test .docx fixtures for the demo.
 * Each fixture is a minimal OOXML package (zip with document.xml).
 *
 * Usage: bun run scripts/generate-test-fixtures.ts
 */

import { join } from "path";
import { mkdirSync, writeFileSync } from "fs";
import { zipSync, strToU8, zlibSync } from "fflate";

const DEST = join(import.meta.dir, "../fixtures");

// ─── PNG builder (deterministic) ───────────────────────────────────────────────
// Emit a valid solid-colour RGB PNG without an image library: an IHDR + a
// zlib-deflated raw image (one filter byte 0 per scanline) + IEND, each chunk
// CRC-32'd. Used by the pictures fixture so a real image/png media part
// flows through the slide picture pipeline (no binary asset committed to git).
function pngCrc32(bytes: Uint8Array): number {
  let crc = 0xffffffff;
  for (let i = 0; i < bytes.length; i++) {
    crc ^= bytes[i];
    for (let b = 0; b < 8; b++) {
      crc = crc & 1 ? (crc >>> 1) ^ 0xedb88320 : crc >>> 1;
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function pngChunk(type: string, data: Uint8Array): Uint8Array {
  const typeBytes = strToU8(type);
  const body = new Uint8Array(typeBytes.length + data.length);
  body.set(typeBytes, 0);
  body.set(data, typeBytes.length);
  const out = new Uint8Array(4 + body.length + 4);
  const dv = new DataView(out.buffer);
  dv.setUint32(0, data.length);
  out.set(body, 4);
  dv.setUint32(4 + body.length, pngCrc32(body));
  return out;
}

function makeSolidPng(w: number, h: number, r: number, g: number, b: number): Uint8Array {
  const sig = new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10]);
  const ihdr = new Uint8Array(13);
  const dv = new DataView(ihdr.buffer);
  dv.setUint32(0, w);
  dv.setUint32(4, h);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 2; // colour type 2 = truecolour RGB
  // 10,11,12 = compression/filter/interlace = 0
  // Raw image: per scanline a filter byte (0) then w RGB triples.
  const raw = new Uint8Array(h * (1 + w * 3));
  let p = 0;
  for (let y = 0; y < h; y++) {
    raw[p++] = 0;
    for (let x = 0; x < w; x++) {
      raw[p++] = r;
      raw[p++] = g;
      raw[p++] = b;
    }
  }
  const idat = zlibSync(raw);
  const ihdrChunk = pngChunk("IHDR", ihdr);
  const idatChunk = pngChunk("IDAT", idat);
  const iendChunk = pngChunk("IEND", new Uint8Array(0));
  const out = new Uint8Array(
    sig.length + ihdrChunk.length + idatChunk.length + iendChunk.length,
  );
  let o = 0;
  out.set(sig, o); o += sig.length;
  out.set(ihdrChunk, o); o += ihdrChunk.length;
  out.set(idatChunk, o); o += idatChunk.length;
  out.set(iendChunk, o);
  return out;
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

const RELS = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>`;

const DOC_RELS = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>`;

const CONTENT_TYPES = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>`;

function docXml(bodyXml: string): string {
  return `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <w:body>${bodyXml}</w:body>
</w:document>`;
}

function p(content: string, pPr = ""): string {
  return `<w:p>${pPr ? `<w:pPr>${pPr}</w:pPr>` : ""}${content}</w:p>`;
}

function r(text: string, rPr = ""): string {
  return `<w:r>${rPr ? `<w:rPr>${rPr}</w:rPr>` : ""}<w:t xml:space="preserve">${text}</w:t></w:r>`;
}

function makeDocx(bodyXml: string): Uint8Array {
  return zipSync({
    "word/document.xml": strToU8(docXml(bodyXml)),
    "_rels/.rels": strToU8(RELS),
    "word/_rels/document.xml.rels": strToU8(DOC_RELS),
    "[Content_Types].xml": strToU8(CONTENT_TYPES),
  });
}

// ─── Fixture definitions ──────────────────────────────────────────────────────

interface Fixture {
  path: string;
  category: string;
  name: string;
  body: string;
}

const fixtures: Fixture[] = [
  {
    path: "text/superscript-subscript.docx",
    category: "text",
    name: "superscript-subscript",
    body: [
      p(r("H") + r("2", `<w:vertAlign w:val="subscript"/>`) + r("O is water. CO") + r("2", `<w:vertAlign w:val="subscript"/>`) + r(" is carbon dioxide.")),
      p(r("E = mc") + r("2", `<w:vertAlign w:val="superscript"/>`) + r(" is Einstein's equation.")),
      p(r("The 1") + r("st", `<w:vertAlign w:val="superscript"/>`) + r(" and 2") + r("nd", `<w:vertAlign w:val="superscript"/>`) + r(" items. Also x") + r("i", `<w:vertAlign w:val="subscript"/>`) + r(" + y") + r("j", `<w:vertAlign w:val="subscript"/>`) + r(".")),
    ].join(""),
  },
  {
    path: "text/highlight.docx",
    category: "text",
    name: "highlight",
    body: [
      p(r("Normal text with ") + r("yellow highlight", `<w:highlight w:val="yellow"/>`) + r(" and ") + r("cyan highlight", `<w:highlight w:val="cyan"/>`) + r(" in a sentence.")),
      p(r("Green", `<w:highlight w:val="green"/>`) + r(" ") + r("Magenta", `<w:highlight w:val="magenta"/>`) + r(" ") + r("Red", `<w:highlight w:val="red"/><w:color w:val="FFFFFF"/>`) + r(" ") + r("Dark Blue", `<w:highlight w:val="darkBlue"/><w:color w:val="FFFFFF"/>`)),
    ].join(""),
  },
  {
    path: "text/char-spacing.docx",
    category: "text",
    name: "char-spacing",
    body: [
      p(r("Normal spacing")),
      p(r("Expanded spacing (+3pt)", `<w:spacing w:val="60"/>`)),
      p(r("Very expanded (+6pt)", `<w:spacing w:val="120"/>`)),
      p(r("Condensed spacing (-1pt)", `<w:spacing w:val="-20"/>`)),
      p(r("Mixed: ") + r("WIDE", `<w:spacing w:val="80"/>`) + r(" and ") + r("tight", `<w:spacing w:val="-10"/>`) + r(" in one line.")),
    ].join(""),
  },
  {
    path: "shapes/textbox.docx",
    category: "shapes",
    name: "textbox",
    body: [
      p(r("Paragraph before the text box.")),
      `<w:p><w:r><w:drawing xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><wp:inline distT="0" distB="0" distL="0" distR="0"><wp:extent cx="3657600" cy="914400"/><wp:docPr id="1" name="Text Box 1"/><a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><wps:wsp><wps:cNvSpPr txBox="1"/><wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="3657600" cy="914400"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="D6E4F0"/></a:solidFill><a:ln w="12700"><a:solidFill><a:srgbClr val="4472C4"/></a:solidFill></a:ln></wps:spPr><wps:txbx><w:txbxContent><w:p><w:r><w:t xml:space="preserve">This is text inside a DrawingML text box. It should wrap within the box boundaries and render correctly with the existing paragraph layout engine.</w:t></w:r></w:p><w:p><w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">Second paragraph — bold text.</w:t></w:r></w:p></w:txbxContent></wps:txbx><wps:bodyPr lIns="91440" tIns="45720" rIns="91440" bIns="45720" anchor="t"/></wps:wsp></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>`,
      p(r("Paragraph after the text box.")),
    ].join(""),
  },
  {
    path: "paragraph/shading.docx",
    category: "paragraph",
    name: "shading",
    body: [
      p(r("Normal paragraph with no shading.")),
      p(r("Yellow shaded paragraph — like a callout or warning box."), `<w:shd w:val="clear" w:color="auto" w:fill="FFFF00"/>`),
      p(r("Light green shaded paragraph — success or info box. This has enough text to wrap to a second line so we can verify multi-line shading works correctly."), `<w:shd w:val="clear" w:color="auto" w:fill="D9EAD3"/>`),
      p(r("Light red shaded paragraph — error or danger callout."), `<w:shd w:val="clear" w:color="auto" w:fill="F4CCCC"/>`),
      p(r("Normal paragraph after shaded blocks.")),
    ].join(""),
  },
  {
    // Decimal tab alignment (§17.3.1.x, ST_TabJc="decimal").
    // Each paragraph has ONE decimal tab stop at 2160 twips (1.5"). The text
    // before the tab is a label; the text after the tab is a number whose first
    // decimal separator '.' must sit AT the stop x — so the dots of 3.14 / 100.5
    // / 7.0 / 0.007 all line up vertically regardless of integer-part width. The
    // last row ("no dot") has no separator, so its end aligns at the stop (Word's
    // fallback = right-align at the decimal stop). A trailing left-tabbed column
    // is omitted to keep the fixture focused on the single decimal column.
    path: "paragraph/decimal-tab.docx",
    category: "paragraph",
    name: "decimal-tab",
    body: (() => {
      const decTab = `<w:tabs><w:tab w:val="decimal" w:pos="2160"/></w:tabs>`;
      const row = (label: string, num: string) =>
        p(r(label) + r("\t") + r(num), decTab);
      return [
        row("Pi", "3.14"),
        row("Hundred-ish", "100.5"),
        row("Seven", "7.0"),
        row("Tiny", "0.007"),
        row("No dot", "42"),
      ].join("");
    })(),
  },
  {
    // Regression for the dropped-leading-section bug: a full-width title section
    // (a paragraph-level sectPr with NO w:type — defaults to NextPage) followed
    // by a CONTINUOUS 2-column section whose short body does NOT fill both
    // columns. The title must stay full-width on top of page 0 with the balanced
    // 2 columns below it. Before the fix the column-balancing reset discarded the
    // carried-in title and floated the body to the page top.
    path: "section/continuous-title-2col.docx",
    category: "section",
    name: "continuous-title-2col",
    body: [
      p(r("THE GREAT TITLE ABOVE THE COLUMNS", `<w:b/><w:sz w:val="32"/>`), `<w:jc w:val="center"/>`),
      // Empty paragraph closing section 0 — 1-column sectPr, NO w:type.
      `<w:p><w:pPr><w:sectPr>` +
        `<w:pgSz w:w="12240" w:h="15840"/>` +
        `<w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>` +
        `<w:cols w:num="1"/>` +
        `</w:sectPr></w:pPr></w:p>`,
      // Short body — two sentences, not enough to fill two columns.
      p(r("The layout engine measures every glyph before a single page exists. Paragraphs flow across the column and break where the measurements say they must, never a pixel early.")),
      // Body-level CONTINUOUS 2-column sectPr.
      `<w:sectPr>` +
        `<w:type w:val="continuous"/>` +
        `<w:pgSz w:w="12240" w:h="15840"/>` +
        `<w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>` +
        `<w:cols w:num="2" w:space="720"/>` +
        `</w:sectPr>`,
    ].join(""),
  },
  {
    // Row-split across page breaks (§17.4.6/§17.4.7). A
    // single-section bordered table with three rows: a short control row; a
    // tall 2-cell row whose shaded (FDE9D9) left cell holds ~60 lines — far
    // more than one Letter content area — so it MUST split across >1 page
    // (repeated left/right/top borders on continuation fragments, deferred
    // bottom border until the last fragment, fill repeated per fragment); and a
    // cantSplit row (~25 lines) that MUST be pushed WHOLE to a fresh page and
    // never fragmented (§17.4.7). Open in the demo to verify the tall row
    // continues onto the next page and the cantSplit row stays intact.
    path: "table/row-split.docx",
    category: "table",
    name: "row-split",
    body: (() => {
      const borders =
        `<w:tblBorders>` +
        `<w:top w:val="single" w:sz="4"/><w:left w:val="single" w:sz="4"/>` +
        `<w:bottom w:val="single" w:sz="4"/><w:right w:val="single" w:sz="4"/>` +
        `<w:insideH w:val="single" w:sz="4"/><w:insideV w:val="single" w:sz="4"/>` +
        `</w:tblBorders>`;
      const cell = (inner: string, tcPr = `<w:tcW w:w="4680" w:type="dxa"/>`) =>
        `<w:tc><w:tcPr>${tcPr}</w:tcPr>${inner}</w:tc>`;
      // Row A — short control (untouched normal row).
      const rowA = `<w:tr>${cell(p(r("Header L")))}${cell(p(r("Header R")))}</w:tr>`;
      // Row B — tall, MUST split. ~60 short paragraphs, shaded left cell.
      const tall = Array.from({ length: 60 }, (_, k) =>
        p(r(`Line ${k} of the tall splittable cell`)),
      ).join("");
      const rowB =
        `<w:tr>` +
        cell(tall, `<w:tcW w:w="4680" w:type="dxa"/><w:shd w:val="clear" w:color="auto" w:fill="FDE9D9"/>`) +
        cell(p(r("right side of tall row"))) +
        `</w:tr>`;
      // Row C — cantSplit, MUST stay whole. ~25 lines.
      const cant = Array.from({ length: 25 }, (_, k) =>
        p(r(`CantSplit line ${k}`)),
      ).join("");
      const rowC =
        `<w:tr><w:trPr><w:cantSplit/></w:trPr>` +
        cell(cant) +
        cell(p(r("cantSplit right"))) +
        `</w:tr>`;
      return (
        `<w:tbl><w:tblPr><w:tblW w:w="9360" w:type="dxa"/>${borders}</w:tblPr>` +
        `<w:tblGrid><w:gridCol w:w="4680"/><w:gridCol w:w="4680"/></w:tblGrid>` +
        `${rowA}${rowB}${rowC}</w:tbl>`
      );
    })(),
  },
];

// ─── Watermarks (need a header part — and media for the image one) ─────────────
//
// Watermarks are VML WordArt/image shapes inside a header (not body content), so
// they don't fit the single-document `makeDocx` shape. Each is a full package:
// document.xml (with a headerReference) + header1.xml + part rels + content types.

function sectPrWithHeader(): string {
  return `<w:sectPr><w:headerReference w:type="default" r:id="rId1"/>` +
    `<w:pgSz w:w="12240" w:h="15840"/>` +
    `<w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>` +
    `</w:sectPr>`;
}

const WM_NS =
  `xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" ` +
  `xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" ` +
  `xmlns:v="urn:schemas-microsoft-com:vml" ` +
  `xmlns:o="urn:schemas-microsoft-com:office:office"`;

// Diagonal "CONFIDENTIAL" WordArt — silver, 50% opacity, rotated 315°.
const WM_TEXT_HEADER = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:hdr ${WM_NS}><w:p><w:r><w:pict>
  <v:shape id="PowerPlusWaterMarkObject1" o:spid="_x0000_s2049" type="#_x0000_t136"
    style="position:absolute;margin-left:0;margin-top:0;width:468pt;height:140pt;rotation:315;z-index:-251658240;mso-position-horizontal:center;mso-position-horizontal-relative:margin;mso-position-vertical:center;mso-position-vertical-relative:margin"
    o:allowincell="f" fillcolor="silver" stroked="f">
    <v:fill opacity=".5"/>
    <v:textpath style="font-family:&quot;Calibri&quot;;font-size:1pt" string="CONFIDENTIAL"/>
  </v:shape>
</w:pict></w:r></w:p></w:hdr>`;

// Washed-out image watermark (gain/blacklevel) referencing media/image1.png.
const WM_IMAGE_HEADER = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:hdr ${WM_NS}><w:p><w:r><w:pict>
  <v:shape id="WordPictureWatermark1" o:spid="_x0000_s2050" type="#_x0000_t75"
    style="position:absolute;margin-left:0;margin-top:0;width:300pt;height:200pt;z-index:-251657216;mso-position-horizontal:center;mso-position-horizontal-relative:margin;mso-position-vertical:center;mso-position-vertical-relative:margin"
    o:allowincell="f">
    <v:imagedata r:id="rId1" o:title="logo" gain="19661f" blacklevel="22938f"/>
  </v:shape>
</w:pict></w:r></w:p></w:hdr>`;

const WM_LOGO_PNG_B64 =
  "iVBORw0KGgoAAAANSUhEUgAAAHgAAABQCAIAAABd+SbeAAAAoElEQVR42u3QAQkAQAgEQZNcku+PrfwWgjDLJpgarVQIQIMWaNCgBRq0QIMGrRXoTs6d1+cGDRo0aNCgQYMGDRo0aNCgQYMGDRo0aNCgQYMGDRo0aNCgQYMGDRo0aNCgQYMGDRo0aNCgQYMGDRo0aNCgQYMGDRo0aNCgQYMGDRo0aNCgQYMGDRo0aNCgQYMGDRo0aNCgQYMGDRo0aNDnoT/F5Q++WsQCXQAAAABJRU5ErkJggg==";

function makeWatermarkDocx(headerXml: string, withImage: boolean): Uint8Array {
  const docBody = `${p(r("Body text under the watermark — the watermark sits behind this."))}${sectPrWithHeader()}`;
  const docRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
</Relationships>`;
  const contentTypes = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>${withImage ? `\n  <Default Extension="png" ContentType="image/png"/>` : ""}
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/header1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"/>
</Types>`;
  const parts: Record<string, Uint8Array> = {
    "word/document.xml": strToU8(docXml(docBody)),
    "word/header1.xml": strToU8(headerXml),
    "_rels/.rels": strToU8(RELS),
    "word/_rels/document.xml.rels": strToU8(docRels),
    "[Content_Types].xml": strToU8(contentTypes),
  };
  if (withImage) {
    parts["word/_rels/header1.xml.rels"] = strToU8(`<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>
</Relationships>`);
    parts["word/media/image1.png"] = Uint8Array.from(atob(WM_LOGO_PNG_B64), (c) => c.charCodeAt(0));
  }
  return zipSync(parts);
}

const watermarkFixtures: { path: string; docx: Uint8Array }[] = [
  { path: "watermark/watermark-text.docx", docx: makeWatermarkDocx(WM_TEXT_HEADER, false) },
  { path: "watermark/watermark-image.docx", docx: makeWatermarkDocx(WM_IMAGE_HEADER, true) },
];

// ─── Header table (§17.10) ─────────────────────────────────────────────────────
// A header whose block content is a paragraph PLUS a 2x2 bordered table — the
// previously-dropped case (BlockContent::Tbl in the header band). Verifies the
// header table renders (cells + borders) alongside the header paragraph. The
// table's two columns hold short labels; the header also leads with a plain
// paragraph so the interleave (para above the table) is exercised.
const HT_NS =
  `xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" ` +
  `xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"`;

function headerTableHeaderXml(): string {
  const borders =
    `<w:tblBorders>` +
    `<w:top w:val="single" w:sz="8" w:color="000000"/>` +
    `<w:left w:val="single" w:sz="8" w:color="000000"/>` +
    `<w:bottom w:val="single" w:sz="8" w:color="000000"/>` +
    `<w:right w:val="single" w:sz="8" w:color="000000"/>` +
    `<w:insideH w:val="single" w:sz="8" w:color="000000"/>` +
    `<w:insideV w:val="single" w:sz="8" w:color="000000"/>` +
    `</w:tblBorders>`;
  const cell = (txt: string) =>
    `<w:tc><w:tcPr><w:tcW w:w="2340" w:type="dxa"/></w:tcPr>${p(r(txt))}</w:tc>`;
  const table =
    `<w:tbl><w:tblPr><w:tblW w:w="4680" w:type="dxa"/>${borders}</w:tblPr>` +
    `<w:tblGrid><w:gridCol w:w="2340"/><w:gridCol w:w="2340"/></w:tblGrid>` +
    `<w:tr>${cell("Doc No")}${cell("ENG-001")}</w:tr>` +
    `<w:tr>${cell("Rev")}${cell("B")}</w:tr>` +
    `</w:tbl>`;
  return `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:hdr ${HT_NS}>${p(r("Header paragraph above the table"))}${table}${p(r(""))}</w:hdr>`;
}

function makeHeaderTableDocx(): Uint8Array {
  const docBody = `${p(r("Body text — the header above carries a 2x2 bordered table."))}${sectPrWithHeader()}`;
  const docRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
</Relationships>`;
  const contentTypes = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/header1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"/>
</Types>`;
  return zipSync({
    "word/document.xml": strToU8(docXml(docBody)),
    "word/header1.xml": strToU8(headerTableHeaderXml()),
    "_rels/.rels": strToU8(RELS),
    "word/_rels/document.xml.rels": strToU8(docRels),
    "[Content_Types].xml": strToU8(contentTypes),
  });
}

watermarkFixtures.push({ path: "header/header-table.docx", docx: makeHeaderTableDocx() });

// ─── Comments fixture (Word comments v1, §17.13.4) ─────────────────────────────
// A document body with `commentRangeStart`/`commentRangeEnd` markers + a
// `word/comments.xml` part wired through a comments relationship. Two authors,
// one with a multi-paragraph body, so the panel shows multiple per-author cards
// and the card→range jump can be exercised. The COMMENTED.docx WordprocessingML
// pattern: each range opens with `<w:commentRangeStart w:id="N"/>`, the run(s),
// `<w:commentRangeEnd w:id="N"/>`, then an empty run carrying
// `<w:commentReference w:id="N"/>`. The panel only needs the range *start* to
// resolve a scroll position; the reference run is included for spec fidelity.

function commentRangeStart(id: number): string {
  return `<w:commentRangeStart w:id="${id}"/>`;
}
function commentRangeEnd(id: number): string {
  return `<w:commentRangeEnd w:id="${id}"/><w:r><w:rPr><w:rStyle w:val="CommentReference"/></w:rPr><w:commentReference w:id="${id}"/></w:r>`;
}

interface CommentDef {
  id: number;
  author: string;
  initials: string;
  date: string;
  /** Body paragraphs (already wrapped text). */
  bodyParas: string[];
}

const COMMENT_DEFS: CommentDef[] = [
  {
    id: 0,
    author: "Ada Lovelace",
    initials: "AL",
    date: "2026-06-01T10:15:00Z",
    bodyParas: ["This opening sentence could be punchier."],
  },
  {
    id: 1,
    author: "Grace Hopper",
    initials: "GH",
    date: "2026-06-02T14:30:00Z",
    bodyParas: [
      "Good point in the middle here.",
      "Second paragraph of the same comment — confirms multi-paragraph bodies join correctly.",
    ],
  },
  // Overlapping shared-endpoint pair: ids 2 + 3 share a region, so the
  // highlight→card find-all-hits path and the {2,3} stacked rect are exercised.
  {
    id: 2,
    author: "Ada Lovelace",
    initials: "AL",
    date: "2026-06-03T09:00:00Z",
    bodyParas: ["Outer range of the overlapping pair."],
  },
  {
    id: 3,
    author: "Grace Hopper",
    initials: "GH",
    date: "2026-06-03T09:05:00Z",
    bodyParas: ["Inner range — overlaps the outer one on the shared word."],
  },
  // Table-cell comment: the range lives entirely inside one cell, so
  // the table-cell-line walk in commentRects gets coverage.
  {
    id: 4,
    author: "Ada Lovelace",
    initials: "AL",
    date: "2026-06-04T11:00:00Z",
    bodyParas: ["A comment anchored inside a table cell."],
  },
];

function commentXml(defs: CommentDef[]): string {
  const comments = defs
    .map((c) => {
      const body = c.bodyParas
        .map(
          (t) =>
            `<w:p><w:pPr><w:pStyle w:val="CommentText"/></w:pPr>` +
            `<w:r><w:rPr><w:rStyle w:val="CommentReference"/></w:rPr><w:annotationRef/></w:r>` +
            `<w:r><w:t xml:space="preserve">${t}</w:t></w:r></w:p>`,
        )
        .join("");
      return `<w:comment w:id="${c.id}" w:author="${c.author}" w:date="${c.date}" w:initials="${c.initials}">${body}</w:comment>`;
    })
    .join("");
  return `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:comments xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">${comments}</w:comments>`;
}

function makeCommentDocx(): Uint8Array {
  // Body: a paragraph whose first sentence is commented (id 0), an intervening
  // paragraph, then a sentence later in the doc commented by id 1 — so the two
  // range starts resolve to distinct {page, y} positions.
  // Comment 0's range CROSSES a paragraph boundary (start in para 1, end in
  // para 2) to exercise the cross-paragraph open-id carrier. Comments 2 + 3
  // overlap on a shared word; comment 4 is anchored inside a table cell.
  const overlapPara = p(
    commentRangeStart(2) +
      r("alpha ") +
      commentRangeStart(3) +
      r("beta ") +
      commentRangeEnd(2) +
      r("gamma") +
      commentRangeEnd(3) +
      r(" delta."),
  );
  const commentedCell =
    `<w:tc><w:tcPr><w:tcW w:w="4680" w:type="dxa"/></w:tcPr>` +
    p(
      commentRangeStart(4) +
        r("Commented cell text") +
        commentRangeEnd(4),
    ) +
    `</w:tc>`;
  const plainCell =
    `<w:tc><w:tcPr><w:tcW w:w="4680" w:type="dxa"/></w:tcPr>${p(r("Plain neighbour cell"))}</w:tc>`;
  const table =
    `<w:tbl><w:tblPr><w:tblW w:w="9360" w:type="dxa"/></w:tblPr>` +
    `<w:tblGrid><w:gridCol w:w="4680"/><w:gridCol w:w="4680"/></w:tblGrid>` +
    `<w:tr>${commentedCell}${plainCell}</w:tr></w:tbl>`;
  const body = [
    p(commentRangeStart(0) + r("This opening sentence is under review.")),
    p(
      r("It continues into a second paragraph ") +
        commentRangeEnd(0) +
        r("and then runs on uncommented."),
    ),
    p(r("An intervening paragraph with no comments, to separate the two ranges so they land on different lines.")),
    p(r("This paragraph runs on for a while. ").repeat(3)),
    p(
      r("Earlier text, then ") +
        commentRangeStart(1) +
        r("a span flagged by the second reviewer") +
        commentRangeEnd(1) +
        r(", then trailing text."),
    ),
    overlapPara,
    table,
  ].join("");

  const docRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>
</Relationships>`;
  const contentTypes = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/comments.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml"/>
</Types>`;
  return zipSync({
    "word/document.xml": strToU8(docXml(body)),
    "word/comments.xml": strToU8(commentXml(COMMENT_DEFS)),
    "_rels/.rels": strToU8(RELS),
    "word/_rels/document.xml.rels": strToU8(docRels),
    "[Content_Types].xml": strToU8(contentTypes),
  });
}

const commentFixtures: { path: string; docx: Uint8Array }[] = [
  { path: "comments/comment-basic.docx", docx: makeCommentDocx() },
];

// ─── Numbering fixture (§17.9) — multi-level + restart + override ───────────────
// Exercises the numbering fidelity fixes (level restart, start overrides and
// multi-level %n substitution):
//   %n  multi-level %n substitution: lvlText "%1.%2.%3" must show "1.1.2", each
//       ancestor counter formatted per its own level's numFmt (§17.9.11).
//   -   lvlRestart (§17.9.6): a deeper level's counter resets when an earlier
//       level advances; lvlRestart="0" suppresses the reset (never restarts).
//   -   lvlOverride/startOverride (§17.9.8/9): a w:num instance overrides an
//       abstract level's start value (and/or whole level def).
//
// Three numbering definitions:
//   abstractNum 0 → outline decimal: %1 / %1.%2 / %1.%2.%3 (default restart).
//     num 1 renders 1, 1.1, 1.1.1, 1.1.2, 1.2, 2, 2.1.
//     num 2 reuses abstract 0 but startOverride=5 on level 0 → 5, 5.1, ...
//   abstractNum 1 → the §17.9.6 never-restart example: %1) decimal / %2)
//     upperLetter (default restart) / %3) lowerRoman with lvlRestart=0.
//     num 3 renders 1) / A) / i) / ii) / 2) / A) / iii)  (level-2 never resets).

function numPr(ilvl: number, numId: number): string {
  return `<w:numPr><w:ilvl w:val="${ilvl}"/><w:numId w:val="${numId}"/></w:numPr>`;
}

function lvl(ilvl: number, fmt: string, text: string, extra = ""): string {
  return (
    `<w:lvl w:ilvl="${ilvl}">` +
    `<w:start w:val="1"/>` +
    `<w:numFmt w:val="${fmt}"/>` +
    extra +
    `<w:lvlText w:val="${text}"/>` +
    `<w:lvlJc w:val="left"/>` +
    `<w:pPr><w:ind w:left="${720 * (ilvl + 1)}" w:hanging="360"/></w:pPr>` +
    `</w:lvl>`
  );
}

// §17.9 wide-marker crowding: a level whose hanging indent (180 twips ≈ 12px) is
// DELIBERATELY narrower than its marker renders ("1.1.1." / "viii)"). The marker
// overflows its slot; the fix must push the text right so marker and text never
// touch ("1.1.1.Deep" crowding). Counterpart to `lvl` (which uses a roomy 360).
function lvlNarrow(ilvl: number, fmt: string, text: string, extra = ""): string {
  return (
    `<w:lvl w:ilvl="${ilvl}">` +
    `<w:start w:val="1"/>` +
    `<w:numFmt w:val="${fmt}"/>` +
    extra +
    `<w:lvlText w:val="${text}"/>` +
    `<w:lvlJc w:val="left"/>` +
    `<w:pPr><w:ind w:left="${720 + 180 * ilvl}" w:hanging="180"/></w:pPr>` +
    `</w:lvl>`
  );
}

const NUMBERING_XML = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:abstractNum w:abstractNumId="0">
    ${lvl(0, "decimal", "%1")}
    ${lvl(1, "decimal", "%1.%2")}
    ${lvl(2, "decimal", "%1.%2.%3")}
  </w:abstractNum>
  <w:abstractNum w:abstractNumId="1">
    ${lvl(0, "decimal", "%1)")}
    ${lvl(1, "upperLetter", "%2)")}
    ${lvl(2, "lowerRoman", "%3)", `<w:lvlRestart w:val="0"/>`)}
  </w:abstractNum>
  <w:abstractNum w:abstractNumId="2">
    ${lvlNarrow(0, "decimal", "%1.")}
    ${lvlNarrow(1, "decimal", "%1.%2.")}
    ${lvlNarrow(2, "decimal", "%1.%2.%3")}
  </w:abstractNum>
  <w:abstractNum w:abstractNumId="3">
    ${lvlNarrow(0, "lowerRoman", "%1)", `<w:lvlRestart w:val="0"/>`)}
  </w:abstractNum>
  <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
  <w:num w:numId="2">
    <w:abstractNumId w:val="0"/>
    <w:lvlOverride w:ilvl="0"><w:startOverride w:val="5"/></w:lvlOverride>
  </w:num>
  <w:num w:numId="3"><w:abstractNumId w:val="1"/></w:num>
  <w:num w:numId="4"><w:abstractNumId w:val="2"/></w:num>
  <w:num w:numId="5"><w:abstractNumId w:val="3"/></w:num>
</w:numbering>`;

function makeNumberingDocx(): Uint8Array {
  // Section A — outline list (num 1): the marker text must read 1, 1.1, 1.1.1,
  // 1.1.2, 1.2, 2, 2.1 (multi-level substitution + default restart on deeper
  // levels when an ancestor advances).
  const outline = [
    p(r("First top item"), numPr(0, 1)),          // 1
    p(r("Nested under first"), numPr(1, 1)),       // 1.1
    p(r("Deep item A"), numPr(2, 1)),              // 1.1.1
    p(r("Deep item B"), numPr(2, 1)),              // 1.1.2
    p(r("Second nested"), numPr(1, 1)),            // 1.2
    p(r("Second top item"), numPr(0, 1)),          // 2  (restarts level 1+2)
    p(r("Nested under second"), numPr(1, 1)),      // 2.1
  ];
  // Section B — startOverride (num 2): level 0 begins at 5.
  const overridden = [
    p(r("Overridden top"), numPr(0, 2)),           // 5
    p(r("Overridden nested"), numPr(1, 2)),        // 5.1
  ];
  // Section C — never-restart (num 3): level 2 (lowerRoman) keeps counting.
  const noRestart = [
    p(r("Alpha"), numPr(0, 3)),                     // 1)
    p(r("Alpha-A"), numPr(1, 3)),                   // A)
    p(r("Alpha-A-i"), numPr(2, 3)),                 // i)
    p(r("Alpha-A-ii"), numPr(2, 3)),                // ii)
    p(r("Beta"), numPr(0, 3)),                      // 2)  (level 1 restarts → A)
    p(r("Beta-A"), numPr(1, 3)),                    // A)
    p(r("Beta-A-iii"), numPr(2, 3)),                // iii) (level 2 never resets)
  ];
  // Section D — wide-marker crowding (num 4 + 5): the
  // hanging indent is only 180 twips (≈12px) but the markers render much wider.
  // num 4 level 2 reads "1.1.1" / "1.1.2" (the canonical "1.1.1Deep" crowding);
  // num 5 is a single-level lowerRoman reaching "viii)". Both markers exceed the
  // 12px slot, so the fix must push the following text right — every line must
  // show a clear gap between marker and word (e.g. "1.1.1" ⎵ "Deep", "viii)" ⎵
  // "Eight"), never touching.
  const wide = [
    p(r("Top"), numPr(0, 4)),                        // 1.
    p(r("Nested"), numPr(1, 4)),                     // 1.1.
    p(r("Deep"), numPr(2, 4)),                       // 1.1.1  ← wide marker, narrow slot
    p(r("Deeper"), numPr(2, 4)),                     // 1.1.2
    p(r("One"), numPr(0, 5)),                         // i)
    p(r("Two"), numPr(0, 5)),                         // ii)
    p(r("Three"), numPr(0, 5)),                       // iii)
    p(r("Four"), numPr(0, 5)),                        // iv)
    p(r("Five"), numPr(0, 5)),                        // v)
    p(r("Six"), numPr(0, 5)),                         // vi)
    p(r("Seven"), numPr(0, 5)),                       // vii)
    p(r("Eight"), numPr(0, 5)),                       // viii)  ← wide roman marker
  ];
  const body = [...outline, ...overridden, ...noRestart, ...wide].join("");

  const docRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>
</Relationships>`;
  const contentTypes = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>
</Types>`;
  return zipSync({
    "word/document.xml": strToU8(docXml(body)),
    "word/numbering.xml": strToU8(NUMBERING_XML),
    "_rels/.rels": strToU8(RELS),
    "word/_rels/document.xml.rels": strToU8(docRels),
    "[Content_Types].xml": strToU8(contentTypes),
  });
}

const numberingFixtures: { path: string; docx: Uint8Array }[] = [
  { path: "numbering/multilevel-outline.docx", docx: makeNumberingDocx() },
];

// ─── Toggle-property XOR (§17.7.3) ──────────────────────────────────────────────
//
// Toggle properties (b, i, caps, ...) do NOT "last value wins" up the style
// chain — they XOR. A run/char-style that re-asserts a property already true in
// an ancestor style TOGGLES IT OFF (even count = off, odd = on). This fixture
// isolates that behaviour with a styles.xml that defines:
//   • BoldPara  — a *paragraph* style with <w:b/> (bold true)
//   • BoldChar  — a *character* style with <w:b/> (bold true)
// Rows (each a single short word so bold/non-bold is visually obvious):
//   1. "Solo"   plain run, no styles                     → NOT bold (baseline)
//   2. "Para"   paragraph BoldPara only                  → BOLD (single true)
//   3. "Char"   character BoldChar only                  → BOLD (single true)
//   4. "Both"   BoldPara paragraph + BoldChar run        → NOT bold (true XOR true)
//   5. "Direct" BoldPara + BoldChar + direct <w:b w:val="1"/> on the run
//               → BOLD (direct formatting wins as-is, no XOR)
// The decisive assertion is row 4 rendering non-bold while rows 2/3 are bold.
function makeToggleXorDocx(): Uint8Array {
  const STYLES_XML = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults>
  <w:style w:type="paragraph" w:styleId="BoldPara">
    <w:name w:val="Bold Para"/>
    <w:rPr><w:b/></w:rPr>
  </w:style>
  <w:style w:type="character" w:styleId="BoldChar">
    <w:name w:val="Bold Char"/>
    <w:rPr><w:b/></w:rPr>
  </w:style>
</w:styles>`;

  const charRun = (text: string, extra = "") =>
    `<w:r><w:rPr><w:rStyle w:val="BoldChar"/>${extra}</w:rPr><w:t xml:space="preserve">${text}</w:t></w:r>`;

  const body = [
    p(r("Solo")),
    p(r("Para"), `<w:pStyle w:val="BoldPara"/>`),
    p(charRun("Char")),
    p(charRun("Both"), `<w:pStyle w:val="BoldPara"/>`),
    p(charRun("Direct", `<w:b w:val="1"/>`), `<w:pStyle w:val="BoldPara"/>`),
  ].join("");

  const docRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>`;
  const contentTypes = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>`;
  return zipSync({
    "word/document.xml": strToU8(docXml(body)),
    "word/styles.xml": strToU8(STYLES_XML),
    "_rels/.rels": strToU8(RELS),
    "word/_rels/document.xml.rels": strToU8(docRels),
    "[Content_Types].xml": strToU8(contentTypes),
  });
}

const toggleFixtures: { path: string; docx: Uint8Array }[] = [
  { path: "text/toggle-xor.docx", docx: makeToggleXorDocx() },
];

// ─── Inline image text-distance padding (§20.4.2.8) ────────────────────────────
//
// A real inline <wp:inline> image with NON-ZERO distL/distR/distT/distB. The
// dist* attributes are Word's "text-distance" padding (type ST_WrapDistance
// §20.4.3.6, EMU): the minimum gap Word keeps between the image edges and the
// surrounding text/lines. The fixture sandwiches the image between text on the
// SAME line ("BEFORE [img] AFTER") and a paragraph above and below it, so all
// four gaps are visible:
//   • distL  → space between "BEFORE" and the image's left edge
//   • distR  → space between the image's right edge and "AFTER"
//   • distT  → extra space above the image (line raised vs the para above)
//   • distB  → extra space below the image (image lifted off the baseline)
// A 60×40 px image is 60*9525=571500 × 40*9525=381000 EMU. Each dist is set to
// 457200 EMU = 0.5" = 48px so the gaps are unmistakable in a screenshot/probe.
// The decisive checks: no text touches the image; horizontal advance of the
// image item is 60+48+48=156px; the image-bearing line is ~40+48+48=136px tall.
const INLINE_IMG_PNG = makeSolidPng(60, 40, 0xe8, 0x4a, 0x2f); // vivid orange-red
const DIST_EMU = 457200; // 0.5 inch on every edge
function makeInlineImageDistDocx(): Uint8Array {
  const drawing =
    `<w:drawing ` +
    `xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" ` +
    `xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" ` +
    `xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">` +
    `<wp:inline distT="${DIST_EMU}" distB="${DIST_EMU}" distL="${DIST_EMU}" distR="${DIST_EMU}">` +
    `<wp:extent cx="571500" cy="381000"/>` +
    `<wp:docPr id="1" name="Picture 1"/>` +
    `<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">` +
    `<pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">` +
    `<pic:nvPicPr><pic:cNvPr id="1" name="Picture 1"/><pic:cNvPicPr/></pic:nvPicPr>` +
    `<pic:blipFill><a:blip r:embed="rId1"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>` +
    `<pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="571500" cy="381000"/></a:xfrm>` +
    `<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr>` +
    `</pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing>`;
  const body =
    p(r("Paragraph above the inline image with padding.")) +
    `<w:p><w:r><w:t xml:space="preserve">BEFORE</w:t></w:r><w:r>${drawing}</w:r>` +
    `<w:r><w:t xml:space="preserve">AFTER</w:t></w:r></w:p>` +
    p(r("Paragraph below the inline image with padding."));
  const docRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>
</Relationships>`;
  const contentTypes = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>`;
  return zipSync({
    "word/document.xml": strToU8(docXml(body)),
    "word/media/image1.png": INLINE_IMG_PNG,
    "_rels/.rels": strToU8(RELS),
    "word/_rels/document.xml.rels": strToU8(docRels),
    "[Content_Types].xml": strToU8(contentTypes),
  });
}

const inlineImageFixtures: { path: string; docx: Uint8Array }[] = [
  { path: "shapes/inline-image-dist.docx", docx: makeInlineImageDistDocx() },
];

// ─── Hyperlinks (§17.16.22): one external (r:id → relationship), one internal
// (w:anchor → bookmark). Eight paragraphs of filler push the bookmark target
// off the first page so that following the anchor has to scroll. Read by the
// native test fixture_yields_external_and_anchor_hyperlinks and the demo.
function makeHyperlinkDocx(): Uint8Array {
  const filler = p(Array.from({ length: 40 }, () => r("filler ")).join(""));
  const body =
    p(`${r("Visit ")}<w:hyperlink r:id="rId100">${r("the example site")}</w:hyperlink>${r(" for details.")}`) +
    p(`${r("Jump to ")}<w:hyperlink w:anchor="target1">${r("the marked section")}</w:hyperlink>${r(" below.")}`) +
    filler.repeat(8) +
    `<w:p><w:bookmarkStart w:id="1" w:name="target1"/>${r("=== TARGET SECTION (bookmark target1) ===")}<w:bookmarkEnd w:id="1"/></w:p>`;
  const docRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId100" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/" TargetMode="External"/>
</Relationships>`;
  return zipSync({
    "word/document.xml": strToU8(docXml(body)),
    "_rels/.rels": strToU8(RELS),
    "word/_rels/document.xml.rels": strToU8(docRels),
    "[Content_Types].xml": strToU8(CONTENT_TYPES),
  });
}

inlineImageFixtures.push({ path: "hyperlink/clickable-links.docx", docx: makeHyperlinkDocx() });

// ─── Excel (SpreadsheetML) demo fixture ────────────────────────────────────────
//
// A minimal .xlsx exercising the spreadsheet path's basics: shared strings,
// inline strings, numbers (right-aligned per §18 general), booleans (centered),
// explicit + default column widths (§18.3.1.13), an explicit row height
// (§18.3.1.73), and default gridlines (§18.3.1.88). Deterministic bytes; no
// binary in git (regenerated by this script, gitignored fixtures dir).

const XLSX_CONTENT_TYPES = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>`;

const XLSX_ROOT_RELS = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>`;

const XLSX_WORKBOOK = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Demo" sheetId="1" r:id="rId1"/></sheets>
</workbook>`;

const XLSX_WORKBOOK_RELS = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>`;

const XLSX_STYLES = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>
  <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
  <borders count="1"><border/></borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  <cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs>
</styleSheet>`;

const XLSX_SHARED_STRINGS = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="6" uniqueCount="6">
  <si><t>Item</t></si>
  <si><t>Qty</t></si>
  <si><t>In stock</t></si>
  <si><t>Widget</t></si>
  <si><t>Gadget</t></si>
  <si><t>Gizmo</t></si>
</sst>`;

const XLSX_SHEET = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:C4"/>
  <sheetFormatPr defaultRowHeight="15" baseColWidth="8"/>
  <cols>
    <col min="1" max="1" width="16" customWidth="1"/>
    <col min="3" max="3" width="12" customWidth="1"/>
  </cols>
  <sheetData>
    <row r="1" ht="20" customHeight="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1" t="s"><v>1</v></c>
      <c r="C1" t="s"><v>2</v></c>
    </row>
    <row r="2">
      <c r="A2" t="s"><v>3</v></c>
      <c r="B2" t="n"><v>12</v></c>
      <c r="C2" t="b"><v>1</v></c>
    </row>
    <row r="3">
      <c r="A3" t="s"><v>4</v></c>
      <c r="B3" t="n"><v>7</v></c>
      <c r="C3" t="b"><v>0</v></c>
    </row>
    <row r="4">
      <c r="A4" t="inlineStr"><is><t>Gizmo</t></is></c>
      <c r="B4" t="n"><v>45292</v></c>
      <c r="C4" t="b"><v>1</v></c>
    </row>
  </sheetData>
</worksheet>`;

function makeDemoXlsx(): Uint8Array {
  return zipSync({
    "[Content_Types].xml": strToU8(XLSX_CONTENT_TYPES),
    "_rels/.rels": strToU8(XLSX_ROOT_RELS),
    "xl/workbook.xml": strToU8(XLSX_WORKBOOK),
    "xl/_rels/workbook.xml.rels": strToU8(XLSX_WORKBOOK_RELS),
    "xl/styles.xml": strToU8(XLSX_STYLES),
    "xl/sharedStrings.xml": strToU8(XLSX_SHARED_STRINGS),
    "xl/worksheets/sheet1.xml": strToU8(XLSX_SHEET),
  });
}

// ─── Parameterized xlsx builder + the styling acceptance suite ────────────────
// Each fixture is a visible acceptance target for one styling area (number
// formats, styles, merges, panes).

const SML_NS = `xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"`;

function makeXlsx(opts: {
  sheets: { name: string; xml: string }[];
  sharedStrings?: string[];
  stylesXml?: string;
}): Uint8Array {
  const n = opts.sheets.length;
  const contentTypes = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
${opts.sheets.map((_, i) => `  <Override PartName="/xl/worksheets/sheet${i + 1}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>`).join("\n")}
  <Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>`;
  const workbook = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook ${SML_NS} xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>${opts.sheets.map((s, i) => `<sheet name="${s.name}" sheetId="${i + 1}" r:id="rId${i + 1}"/>`).join("")}</sheets>
</workbook>`;
  const wbRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
${opts.sheets.map((_, i) => `  <Relationship Id="rId${i + 1}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet${i + 1}.xml"/>`).join("\n")}
  <Relationship Id="rId${n + 1}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
  <Relationship Id="rId${n + 2}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>`;
  const sst = opts.sharedStrings ?? [];
  const sstXml = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst ${SML_NS} count="${sst.length}" uniqueCount="${sst.length}">
${sst.map((t) => `  <si><t xml:space="preserve">${t}</t></si>`).join("\n")}
</sst>`;
  const styles = opts.stylesXml ?? XLSX_STYLES;
  const parts: Record<string, Uint8Array> = {
    "[Content_Types].xml": strToU8(contentTypes),
    "_rels/.rels": strToU8(XLSX_ROOT_RELS),
    "xl/workbook.xml": strToU8(workbook),
    "xl/_rels/workbook.xml.rels": strToU8(wbRels),
    "xl/styles.xml": strToU8(styles),
    "xl/sharedStrings.xml": strToU8(sstXml),
  };
  opts.sheets.forEach((s, i) => {
    parts[`xl/worksheets/sheet${i + 1}.xml`] = strToU8(s.xml);
  });
  return zipSync(parts);
}

// Cell/row helpers — `s` is the cellXfs style index.
const sc = (ref: string, sstIdx: number, s = 0) => `<c r="${ref}" t="s"${s ? ` s="${s}"` : ""}><v>${sstIdx}</v></c>`;
const nc = (ref: string, v: number | string, s = 0) => `<c r="${ref}"${s ? ` s="${s}"` : ""}><v>${v}</v></c>`;
const bc = (ref: string, v: 0 | 1, s = 0) => `<c r="${ref}" t="b"${s ? ` s="${s}"` : ""}><v>${v}</v></c>`;
const rw = (r: number, cells: string, ht?: number) => `<row r="${r}"${ht ? ` ht="${ht}" customHeight="1"` : ""}>${cells}</row>`;
const ws = (dimension: string, body: string, pre = "") => `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet ${SML_NS}>
  <dimension ref="${dimension}"/>
  <sheetFormatPr defaultRowHeight="15" baseColWidth="8"/>
${pre}  <sheetData>
${body}
  </sheetData>
</worksheet>`;

// 1) number-formats.xlsx — every common numFmt class; the number-format target.
//    cellXfs: 1=date(14) 2=percent(10) 3=thousands(3) 4=decimal2(2) 5=currency(164) 6=time(21)
function makeFormatsXlsx(): Uint8Array {
  const styles = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet ${SML_NS}>
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
</styleSheet>`;
  const sst = ["Format", "Raw value", "Should display as", "Date (numFmt 14)", "Percent (10)",
    "Thousands (3)", "Decimal (2)", "Currency ($#,##0.00)", "Time (21)", "General",
    "1/1/2024", "62.50%", "1,234,567", "3.14", "$1,299.99", "1:30:00 PM", "as-is"];
  const body = [
    rw(1, sc("A1", 0) + sc("B1", 1) + sc("C1", 2), 20),
    rw(2, sc("A2", 3) + nc("B2", 45292, 1) + sc("C2", 10)),
    rw(3, sc("A3", 4) + nc("B3", 0.625, 2) + sc("C3", 11)),
    rw(4, sc("A4", 5) + nc("B4", 1234567, 3) + sc("C4", 12)),
    rw(5, sc("A5", 6) + nc("B5", 3.14159, 4) + sc("C5", 13)),
    rw(6, sc("A6", 7) + nc("B6", 1299.99, 5) + sc("C6", 14)),
    rw(7, sc("A7", 8) + nc("B7", 0.5625, 6) + sc("C7", 15)),
    rw(8, sc("A8", 9) + nc("B8", 42) + sc("C8", 16)),
  ].join("\n");
  const pre = `  <cols><col min="1" max="1" width="22" customWidth="1"/><col min="2" max="2" width="14" customWidth="1"/><col min="3" max="3" width="18" customWidth="1"/></cols>\n`;
  return makeXlsx({ sheets: [{ name: "Formats", xml: ws("A1:C8", body, pre) }], sharedStrings: sst, stylesXml: styles });
}

// 2) styled.xlsx — fonts/fills/borders/alignment; the styles target.
function makeStyledXlsx(): Uint8Array {
  const styles = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet ${SML_NS}>
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
</styleSheet>`;
  const sst = ["Quarterly Sales Report", "Region", "Q1", "Q2", "Q3", "Q4",
    "North", "South", "East", "West", "Styled: blue header, zebra rows, boxed cells"];
  const body = [
    rw(1, sc("A1", 0, 4), 24),
    rw(3, sc("A3", 1, 1) + sc("B3", 2, 1) + sc("C3", 3, 1) + sc("D3", 4, 1) + sc("E3", 5, 1), 18),
    rw(4, sc("A4", 6, 3) + nc("B4", 12500, 3) + nc("C4", 14200, 3) + nc("D4", 13100, 3) + nc("E4", 15800, 3)),
    rw(5, sc("A5", 7, 2) + nc("B5", 9800, 2) + nc("C5", 11400, 2) + nc("D5", 10900, 2) + nc("E5", 12300, 2)),
    rw(6, sc("A6", 8, 3) + nc("B6", 15200, 3) + nc("C6", 16100, 3) + nc("D6", 15900, 3) + nc("E6", 17400, 3)),
    rw(7, sc("A7", 9, 2) + nc("B7", 8700, 2) + nc("C7", 9200, 2) + nc("D7", 9800, 2) + nc("E7", 10100, 2)),
    rw(9, sc("A9", 10, 5)),
  ].join("\n");
  const pre = `  <cols><col min="1" max="1" width="14" customWidth="1"/><col min="2" max="5" width="10" customWidth="1"/></cols>\n`;
  return makeXlsx({ sheets: [{ name: "Styled", xml: ws("A1:E9", body, pre) }], sharedStrings: sst, stylesXml: styles });
}

// 3) merged.xlsx — mergeCells (horizontal title + vertical group); the merge target.
function makeMergedXlsx(): Uint8Array {
  const sst = ["Invoice #2024-001", "Group", "Item", "Amount", "Hardware", "Laptop", "Monitor",
    "Software", "License", "Support", "Total"];
  const body = [
    rw(1, sc("A1", 0), 22),
    rw(2, sc("A2", 1) + sc("B2", 2) + sc("C2", 3)),
    rw(3, sc("A3", 4) + sc("B3", 5) + nc("C3", 1899)),
    rw(4, sc("B4", 6) + nc("C4", 549)),
    rw(5, sc("A5", 7) + sc("B5", 8) + nc("C5", 1200)),
    rw(6, sc("B6", 9) + nc("C6", 350)),
    rw(7, sc("A7", 10) + nc("C7", 3998)),
  ].join("\n");
  const xml = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet ${SML_NS}>
  <dimension ref="A1:C7"/>
  <sheetFormatPr defaultRowHeight="15" baseColWidth="8"/>
  <cols><col min="1" max="1" width="12" customWidth="1"/><col min="2" max="2" width="14" customWidth="1"/><col min="3" max="3" width="12" customWidth="1"/></cols>
  <sheetData>
${body}
  </sheetData>
  <mergeCells count="3"><mergeCell ref="A1:C1"/><mergeCell ref="A3:A4"/><mergeCell ref="A5:A6"/></mergeCells>
</worksheet>`;
  return makeXlsx({ sheets: [{ name: "Invoice", xml }], sharedStrings: sst });
}

// 4) inventory-wide.xlsx — 8 cols × 60 rows, crosses print-page boundaries.
function makeInventoryXlsx(): Uint8Array {
  const sst = ["SKU", "Product", "Category", "Qty", "Unit price", "Reorder", "Discontinued", "Notes",
    "Widgets", "Gadgets", "Gizmos", "Fasteners",
    ...Array.from({ length: 60 }, (_, i) => `Product ${String.fromCharCode(65 + (i % 26))}-${100 + i}`)];
  const rows = [rw(1, ["A", "B", "C", "D", "E", "F", "G", "H"].map((c, j) => sc(`${c}1`, j)).join(""), 18)];
  for (let i = 0; i < 60; i++) {
    const r = i + 2;
    rows.push(rw(r,
      nc(`A${r}`, 10000 + i * 7) + sc(`B${r}`, 12 + i) + sc(`C${r}`, 8 + (i % 4)) +
      nc(`D${r}`, (i * 13) % 250) + nc(`E${r}`, ((i * 7919) % 9000) / 100 + 1) +
      nc(`F${r}`, (i % 5) * 10 + 5) + bc(`G${r}`, (i % 7 === 0 ? 1 : 0) as 0 | 1)));
  }
  const pre = `  <cols><col min="1" max="1" width="9" customWidth="1"/><col min="2" max="2" width="18" customWidth="1"/><col min="3" max="3" width="12" customWidth="1"/><col min="4" max="4" width="8" customWidth="1"/><col min="5" max="5" width="11" customWidth="1"/><col min="6" max="6" width="10" customWidth="1"/><col min="7" max="7" width="14" customWidth="1"/></cols>\n`;
  return makeXlsx({ sheets: [{ name: "Inventory", xml: ws("A1:H61", rows.join("\n"), pre) }], sharedStrings: sst });
}

// 4b) wide-grid.xlsx — 24 cols × 30 rows; the column-tiling target. Wider
//     than one print-page, so it tiles into column BANDS emitted downThenOver
//     (§18.3.1.63 default): all row-chunks of band 1, then band 2, etc. Carries
//     a banner merge spanning ALL 24 columns (crosses every band boundary) to
//     exercise the cross-band clamp (the anchor truncates at band 1's right edge;
//     the continuation in later bands renders blank — v1 simplification).
function colLetters(n: number): string {
  let s = "";
  while (n > 0) { const r = (n - 1) % 26; s = String.fromCharCode(65 + r) + s; n = Math.floor((n - 1) / 26); }
  return s;
}
function makeWideGridXlsx(): Uint8Array {
  const NCOL = 24, NROW = 30;
  const sst = ["Wide grid: 24 columns tile into bands (downThenOver)"];
  const rows: string[] = [];
  // Row 1: the banner merge anchor (A1:X1), a single shared string.
  rows.push(rw(1, sc("A1", 0), 20));
  // Row 2: column headers C1..C24 as numbers (the column index), so each band's
  // membership reads directly off the grid.
  rows.push(rw(2, Array.from({ length: NCOL }, (_, j) => nc(`${colLetters(j + 1)}2`, j + 1)).join("")));
  // Rows 3..NROW: a numeric grid (row*100 + col) so every cell is unique.
  for (let r = 3; r <= NROW; r++) {
    rows.push(rw(r, Array.from({ length: NCOL }, (_, j) => nc(`${colLetters(j + 1)}${r}`, r * 100 + (j + 1))).join("")));
  }
  const dim = `A1:${colLetters(NCOL)}${NROW}`;
  const xml = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet ${SML_NS}>
  <dimension ref="${dim}"/>
  <sheetFormatPr defaultRowHeight="15" baseColWidth="8"/>
  <sheetData>
${rows.join("\n")}
  </sheetData>
  <mergeCells count="1"><mergeCell ref="A1:${colLetters(NCOL)}1"/></mergeCells>
</worksheet>`;
  return makeXlsx({ sheets: [{ name: "Wide", xml }], sharedStrings: sst });
}

// 4c) frozen.xlsx — FROZEN PANES (§18.3.1.66); the grid-view frozen-pane demo.
//     Sheet 1 "Frozen": xSplit=1 + ySplit=1 (pinned corner + top band + left
//     band + scrolling body) over a 10×60 grid — row 1 and column A stay pinned
//     while both axes scroll, with the darker freeze boundary line at the edge.
//     Sheet 2 "DeepStart": ySplit=1 with topLeftCell="A50" — the sheet OPENS
//     scrolled to row 50 (the file-saved body position; Excel-restore parity).
function makeFrozenXlsx(): Uint8Array {
  const NCOL = 10, NROW = 60;
  const sst = [
    "Region", // A1 — the pinned corner
    ...Array.from({ length: NCOL - 1 }, (_, j) => `Q${j + 1}`), // B1..J1 headers
    ...Array.from({ length: NROW - 1 }, (_, i) => `Region ${String(i + 1).padStart(2, "0")}`), // A2..A60
    "Log", "Deep start: this sheet opens at row 50 (topLeftCell)", // DeepStart headers
  ];
  const frozenRows = [
    rw(1, Array.from({ length: NCOL }, (_, j) => sc(`${colLetters(j + 1)}1`, j)).join(""), 18),
  ];
  for (let r = 2; r <= NROW; r++) {
    frozenRows.push(rw(r,
      sc(`A${r}`, NCOL + r - 2) +
      Array.from({ length: NCOL - 1 }, (_, j) => nc(`${colLetters(j + 2)}${r}`, r * 100 + j + 1)).join("")));
  }
  // sheetViews sits between dimension and sheetFormatPr (schema order).
  const frozen = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet ${SML_NS}>
  <dimension ref="A1:${colLetters(NCOL)}${NROW}"/>
  <sheetViews><sheetView workbookViewId="0"><pane xSplit="1" ySplit="1" topLeftCell="B2" activePane="bottomRight" state="frozen"/></sheetView></sheetViews>
  <sheetFormatPr defaultRowHeight="15" baseColWidth="8"/>
  <cols><col min="1" max="1" width="14" customWidth="1"/></cols>
  <sheetData>
${frozenRows.join("\n")}
  </sheetData>
</worksheet>`;
  const deepBase = sst.length - 2;
  const deepRows = [rw(1, sc("A1", deepBase) + sc("B1", deepBase + 1), 18)];
  for (let r = 2; r <= 90; r++) deepRows.push(rw(r, nc(`A${r}`, r) + nc(`B${r}`, r * 1000)));
  const deep = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet ${SML_NS}>
  <dimension ref="A1:B90"/>
  <sheetViews><sheetView workbookViewId="0"><pane ySplit="1" topLeftCell="A50" activePane="bottomLeft" state="frozen"/></sheetView></sheetViews>
  <sheetFormatPr defaultRowHeight="15" baseColWidth="8"/>
  <cols><col min="2" max="2" width="44" customWidth="1"/></cols>
  <sheetData>
${deepRows.join("\n")}
  </sheetData>
</worksheet>`;
  return makeXlsx({
    sheets: [{ name: "Frozen", xml: frozen }, { name: "DeepStart", xml: deep }],
    sharedStrings: sst,
  });
}

// 4c-bis) hidden.xlsx — HIDDEN ROWS/COLS (§18.3.1.13/§18.3.1.73); the hidden
//     rows/cols demo. Col C is hidden via `hidden="1"`, col E via the
//     Excel equivalence `width="0" customWidth="1"`; rows 4-5 are hidden
//     (`hidden="1"` and the `ht="0" customHeight="1"` equivalence). Values
//     live INSIDE the hidden cells so copy proves the include-hidden
//     ruling (select B..D, copy → the hidden C values PASTE), and the hidden
//     rows carry text that search must SKIP (Excel Find default). Header cue:
//     letters/numbers skip C/E/4/5 with the heavier gutter boundary marker.
function makeHiddenXlsx(): Uint8Array {
  const sst = [
    "Item", "Jan", "Feb (hidden col)", "Mar", "Adj (hidden col)", "Total", // A1..F1
    "Widgets", "Gadgets", // A2, A3
    "SECRET: this row is hidden — search must not find it", // A4 (hidden row)
    "Also hidden (ht=0 + customHeight)", // A5 (hidden row)
    "Gizmos", // A6
    "Copy B1:D3 — the hidden Feb values paste; the hidden code word is never found by search.", // A8 (must NOT contain the needle itself!)
  ];
  const body = [
    rw(1, ["A", "B", "C", "D", "E", "F"].map((c, j) => sc(`${c}1`, j)).join(""), 18),
    rw(2, sc("A2", 6) + nc("B2", 120) + nc("C2", 135) + nc("D2", 150) + nc("E2", -5) + nc("F2", 400)),
    rw(3, sc("A3", 7) + nc("B3", 80) + nc("C3", 95) + nc("D3", 110) + nc("E3", 0) + nc("F3", 285)),
    `<row r="4" hidden="1">${sc("A4", 8) + nc("B4", 1) + nc("C4", 2) + nc("D4", 3)}</row>`,
    `<row r="5" ht="0" customHeight="1">${sc("A5", 9)}</row>`,
    rw(6, sc("A6", 10) + nc("B6", 60) + nc("C6", 70) + nc("D6", 85) + nc("E6", 5) + nc("F6", 220)),
    rw(8, sc("A8", 11)),
  ].join("\n");
  const pre = `  <cols><col min="1" max="1" width="14" customWidth="1"/><col min="3" max="3" width="12" hidden="1" customWidth="1"/><col min="5" max="5" width="0" customWidth="1"/></cols>\n`;
  return makeXlsx({ sheets: [{ name: "Hidden", xml: ws("A1:F8", body, pre) }], sharedStrings: sst });
}

// 4d) chart.xlsx — an EMBEDDED CHART: sheet
//     "Chart" carries the quarterly source data (A2:A5 categories, B/C series)
//     plus a worksheet drawing (§18.3.1.36) whose twoCellAnchor graphicFrame
//     references a DML chart part (`c:chart r:id` → xl/charts/chart1.xml, §21.2)
//     anchored E2→K17 — the SAME c:chartSpace schema WML/PML render, so the
//     shared crate::chart::render_chart draws it AS-IS in the grid. Two series
//     (Sales/Target, clustered columns) so the copy TSV exercises multi-series.
function makeChartXlsx(): Uint8Array {
  const sst = ["Quarter", "Sales", "Target", "Q1", "Q2", "Q3", "Q4",
    "Click the chart, then copy: numbers paste as TSV, picture as PNG."];
  const rows = [
    rw(1, sc("A1", 0) + sc("B1", 1) + sc("C1", 2), 18),
    rw(2, sc("A2", 3) + nc("B2", 18) + nc("C2", 20)),
    rw(3, sc("A3", 4) + nc("B3", 32) + nc("C3", 30)),
    rw(4, sc("A4", 5) + nc("B4", 25) + nc("C4", 30)),
    rw(5, sc("A5", 6) + nc("B5", 41) + nc("C5", 40)),
    rw(7, sc("A7", 7)),
  ];
  const sheet = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet ${SML_NS} xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <dimension ref="A1:C7"/>
  <sheetFormatPr defaultRowHeight="15" baseColWidth="8"/>
  <sheetData>
${rows.join("\n")}
  </sheetData>
  <drawing r:id="rId1"/>
</worksheet>`;
  const chartSer = (idx: number, nameRef: string, name: string, valRef: string, vals: number[]) => `<c:ser>
          <c:idx val="${idx}"/><c:order val="${idx}"/>
          <c:tx><c:strRef><c:f>${nameRef}</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>${name}</c:v></c:pt></c:strCache></c:strRef></c:tx>
          <c:cat><c:strRef><c:f>Chart!$A$2:$A$5</c:f><c:strCache><c:ptCount val="4"/><c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt><c:pt idx="2"><c:v>Q3</c:v></c:pt><c:pt idx="3"><c:v>Q4</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:f>${valRef}</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val="4"/>${vals.map((v, i) => `<c:pt idx="${i}"><c:v>${v}</c:v></c:pt>`).join("")}</c:numCache></c:numRef></c:val>
        </c:ser>`;
  const chartPart = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <c:chart>
    <c:plotArea>
      <c:layout/>
      <c:barChart>
        <c:barDir val="col"/>
        <c:grouping val="clustered"/>
        ${chartSer(0, "Chart!$B$1", "Sales", "Chart!$B$2:$B$5", [18, 32, 25, 41])}
        ${chartSer(1, "Chart!$C$1", "Target", "Chart!$C$2:$C$5", [20, 30, 30, 40])}
        <c:axId val="1"/><c:axId val="2"/>
      </c:barChart>
      <c:catAx><c:axId val="1"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/><c:crossAx val="2"/></c:catAx>
      <c:valAx><c:axId val="2"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="l"/><c:crossAx val="1"/></c:valAx>
    </c:plotArea>
    <c:legend><c:legendPos val="r"/></c:legend>
    <c:plotVisOnly val="1"/>
  </c:chart>
</c:chartSpace>`;
  const drawing = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <xdr:twoCellAnchor>
    <xdr:from><xdr:col>4</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
    <xdr:to><xdr:col>10</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>16</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
    <xdr:graphicFrame macro="">
      <xdr:nvGraphicFramePr><xdr:cNvPr id="2" name="Chart 1"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr>
      <xdr:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></xdr:xfrm>
      <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
        <c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="rId1"/>
      </a:graphicData></a:graphic>
    </xdr:graphicFrame>
    <xdr:clientData/>
  </xdr:twoCellAnchor>
</xdr:wsDr>`;
  const contentTypes = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
  <Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/>
  <Override PartName="/xl/charts/chart1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>
</Types>`;
  const workbook = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook ${SML_NS} xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Chart" sheetId="1" r:id="rId1"/></sheets>
</workbook>`;
  const wbRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>`;
  const sheetRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/>
</Relationships>`;
  const drawingRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/>
</Relationships>`;
  const sstXml = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst ${SML_NS} count="${sst.length}" uniqueCount="${sst.length}">
${sst.map((t) => `  <si><t xml:space="preserve">${t}</t></si>`).join("\n")}
</sst>`;
  return zipSync({
    "[Content_Types].xml": strToU8(contentTypes),
    "_rels/.rels": strToU8(XLSX_ROOT_RELS),
    "xl/workbook.xml": strToU8(workbook),
    "xl/_rels/workbook.xml.rels": strToU8(wbRels),
    "xl/styles.xml": strToU8(XLSX_STYLES),
    "xl/sharedStrings.xml": strToU8(sstXml),
    "xl/worksheets/sheet1.xml": strToU8(sheet),
    "xl/worksheets/_rels/sheet1.xml.rels": strToU8(sheetRels),
    "xl/drawings/drawing1.xml": strToU8(drawing),
    "xl/drawings/_rels/drawing1.xml.rels": strToU8(drawingRels),
    "xl/charts/chart1.xml": strToU8(chartPart),
  });
}

// 5) multi-sheet.xlsx — three sheets; the sheet-tabs target.
function makeMultiSheetXlsx(): Uint8Array {
  const sst = ["Summary", "Total widgets", "Total value", "Region", "Units", "North", "South",
    "Notes", "All figures are synthetic fixture data.", "Generated by scripts/generate-test-fixtures.ts"];
  const s1 = ws("A1:B3", [
    rw(1, sc("A1", 0), 20),
    rw(2, sc("A2", 1) + nc("B2", 1234)),
    rw(3, sc("A3", 2) + nc("B3", 98765.43)),
  ].join("\n"));
  const s2 = ws("A1:B3", [
    rw(1, sc("A1", 3) + sc("B1", 4), 18),
    rw(2, sc("A2", 5) + nc("B2", 640)),
    rw(3, sc("A3", 6) + nc("B3", 594)),
  ].join("\n"));
  const s3 = ws("A1:A3", [
    rw(1, sc("A1", 7), 18),
    rw(2, sc("A2", 8)),
    rw(3, sc("A3", 9)),
  ].join("\n"));
  return makeXlsx({
    sheets: [{ name: "Summary", xml: s1 }, { name: "Data", xml: s2 }, { name: "Notes", xml: s3 }],
    sharedStrings: sst,
  });
}

const xlsxFixtures: { path: string; xlsx: Uint8Array }[] = [
  { path: "spreadsheet/demo.xlsx", xlsx: makeDemoXlsx() },
  { path: "spreadsheet/number-formats.xlsx", xlsx: makeFormatsXlsx() },
  { path: "spreadsheet/styled.xlsx", xlsx: makeStyledXlsx() },
  { path: "spreadsheet/merged.xlsx", xlsx: makeMergedXlsx() },
  { path: "spreadsheet/inventory-wide.xlsx", xlsx: makeInventoryXlsx() },
  { path: "spreadsheet/wide-grid.xlsx", xlsx: makeWideGridXlsx() },
  { path: "spreadsheet/frozen.xlsx", xlsx: makeFrozenXlsx() },
  { path: "spreadsheet/hidden.xlsx", xlsx: makeHiddenXlsx() },
  { path: "spreadsheet/chart.xlsx", xlsx: makeChartXlsx() },
  { path: "spreadsheet/multi-sheet.xlsx", xlsx: makeMultiSheetXlsx() },
];

// ─── PowerPoint (.pptx) builder ───────────────────────────────────────────────
//
// A minimal, parser-valid one-slide deck exercising the basic slide paths: slide
// = print page sized from p:sldSz (§19.2.1.39); a title placeholder with NO
// a:xfrm that INHERITS the layout's ctrTitle box (§19.3.1.x); a body placeholder
// (ph idx=1) that inherits the layout body box; explicit run props (bold + sz +
// solidFill srgbClr). Mirrors the native pml fixture (src/pml/tests.rs) so the
// demo renders exactly what the unit tests pin. Parts are the
// parser-minimal set: Content_Types + root rels + presentation(+rels) + master(
// +rels) + layout-with-placeholders(+rels) + slide(+rels). No theme part (the
// fork never reads one). Stored/deflated bytes are deterministic.

const PPTX_CONTENT_TYPES = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
</Types>`;

const PPTX_ROOT_RELS = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>`;

const PPTX_PRESENTATION = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
  <p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst>
  <p:sldSz cx="9144000" cy="6858000" type="screen4x3"/>
  <p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>`;

const PPTX_PRESENTATION_RELS = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
</Relationships>`;

const PPTX_SLIDE_MASTER = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
    </p:spTree>
  </p:cSld>
  <p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
  <p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
</p:sldMaster>`;

const PPTX_SLIDE_MASTER_RELS = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>`;

const PPTX_SLIDE_LAYOUT = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
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
</p:sldLayout>`;

const PPTX_SLIDE_LAYOUT_RELS = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>`;

// Slide: a title placeholder (NO a:xfrm — inherits the layout ctrTitle box) with
// a bold 44pt explicit run, plus a body placeholder (ph idx=1, NO a:xfrm) with
// two bullet paragraphs + a non-placeholder rectangle WITH its own xfrm + a
// solidFill + a caption run (exercises the direct-xfrm + shape-fill path).
const PPTX_SLIDE = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
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
          <a:p><a:r><a:rPr lang="en-US"/><a:t>First bullet point on the slide</a:t></a:r></a:p>
          <a:p><a:r><a:rPr lang="en-US"/><a:t>Second bullet point on the slide</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="4" name="Rect 3"/>
          <p:cNvSpPr/>
          <p:nvPr/>
        </p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="685800" y="6096000"/><a:ext cx="3657600" cy="457200"/></a:xfrm>
          <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
          <a:solidFill><a:srgbClr val="2563EB"/></a:solidFill>
        </p:spPr>
        <p:txBody>
          <a:bodyPr/><a:lstStyle/>
          <a:p><a:r><a:rPr lang="en-US" sz="1600"><a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill></a:rPr><a:t>Boxed caption</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>`;

const PPTX_SLIDE_RELS = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>`;

function makePptx(): Uint8Array {
  return zipSync({
    "[Content_Types].xml": strToU8(PPTX_CONTENT_TYPES),
    "_rels/.rels": strToU8(PPTX_ROOT_RELS),
    "ppt/presentation.xml": strToU8(PPTX_PRESENTATION),
    "ppt/_rels/presentation.xml.rels": strToU8(PPTX_PRESENTATION_RELS),
    "ppt/slideMasters/slideMaster1.xml": strToU8(PPTX_SLIDE_MASTER),
    "ppt/slideMasters/_rels/slideMaster1.xml.rels": strToU8(PPTX_SLIDE_MASTER_RELS),
    "ppt/slideLayouts/slideLayout1.xml": strToU8(PPTX_SLIDE_LAYOUT),
    "ppt/slideLayouts/_rels/slideLayout1.xml.rels": strToU8(PPTX_SLIDE_LAYOUT_RELS),
    "ppt/slides/slide1.xml": strToU8(PPTX_SLIDE),
    "ppt/slides/_rels/slide1.xml.rels": strToU8(PPTX_SLIDE_RELS),
  });
}

// ─── Second PPTX fixture: fills, colours, outline, alignment, bullets ─────────
//
// A deck exercising the fidelity surface beyond the basic title-body deck:
// three non-placeholder rectangles with DISTINCT solid fills, one of them also
// carrying a solid OUTLINE (a:ln solidFill srgbClr), a centred title run, a
// right-aligned caption, and a body placeholder whose paragraphs carry char
// bullets (pPr/buChar). Reuses the same minimal master/layout/Content_Types as
// title-body.pptx (only the slide part differs). Verified by loading in the
// native pml tests (src/pml/tests.rs::fills_colors_*) + a demo __capture.

const PPTX_FC_SLIDE = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr>
        <p:spPr/>
        <p:txBody>
          <a:bodyPr/><a:lstStyle/>
          <a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="en-US" sz="4000" b="1"><a:solidFill><a:srgbClr val="0F172A"/></a:solidFill></a:rPr><a:t>Fills and Colours</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="3" name="Body 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
        <p:spPr/>
        <p:txBody>
          <a:bodyPr/><a:lstStyle/>
          <a:p><a:pPr><a:buChar char="&#8226;"/></a:pPr><a:r><a:rPr lang="en-US" sz="2000"/><a:t>Bulleted body line one</a:t></a:r></a:p>
          <a:p><a:pPr algn="r"><a:buChar char="&#8226;"/></a:pPr><a:r><a:rPr lang="en-US" sz="2000"><a:solidFill><a:srgbClr val="B91C1C"/></a:solidFill></a:rPr><a:t>Right-aligned red line</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="4" name="Blue Rect"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="685800" y="5181600"/><a:ext cx="2286000" cy="1143000"/></a:xfrm>
          <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
          <a:solidFill><a:srgbClr val="2563EB"/></a:solidFill>
        </p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="en-US" sz="1400"><a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill></a:rPr><a:t>Blue</a:t></a:r></a:p></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="5" name="Green Outlined Rect"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="3429000" y="5181600"/><a:ext cx="2286000" cy="1143000"/></a:xfrm>
          <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
          <a:solidFill><a:srgbClr val="16A34A"/></a:solidFill>
          <a:ln w="38100"><a:solidFill><a:srgbClr val="14532D"/></a:solidFill></a:ln>
        </p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="en-US" sz="1400"><a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill></a:rPr><a:t>Green</a:t></a:r></a:p></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="6" name="Amber Rect"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="6172200" y="5181600"/><a:ext cx="2286000" cy="1143000"/></a:xfrm>
          <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
          <a:solidFill><a:srgbClr val="F59E0B"/></a:solidFill>
        </p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="en-US" sz="1400"><a:solidFill><a:srgbClr val="0F172A"/></a:solidFill></a:rPr><a:t>Amber</a:t></a:r></a:p></p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>`;

function makeFillsColorsPptx(): Uint8Array {
  return zipSync({
    "[Content_Types].xml": strToU8(PPTX_CONTENT_TYPES),
    "_rels/.rels": strToU8(PPTX_ROOT_RELS),
    "ppt/presentation.xml": strToU8(PPTX_PRESENTATION),
    "ppt/_rels/presentation.xml.rels": strToU8(PPTX_PRESENTATION_RELS),
    "ppt/slideMasters/slideMaster1.xml": strToU8(PPTX_SLIDE_MASTER),
    "ppt/slideMasters/_rels/slideMaster1.xml.rels": strToU8(PPTX_SLIDE_MASTER_RELS),
    "ppt/slideLayouts/slideLayout1.xml": strToU8(PPTX_SLIDE_LAYOUT),
    "ppt/slideLayouts/_rels/slideLayout1.xml.rels": strToU8(PPTX_SLIDE_LAYOUT_RELS),
    "ppt/slides/slide1.xml": strToU8(PPTX_FC_SLIDE),
    "ppt/slides/_rels/slide1.xml.rels": strToU8(PPTX_SLIDE_RELS),
  });
}

// ─── pictures.pptx — slide pictures ───────────────────────────────────────────
//
// A slide carrying one p:pic (§19.3.1.x): a blipFill → a:blip@r:embed → a real
// PNG media part, positioned by an explicit a:xfrm. Exercises the picture
// pipeline end-to-end in the demo: wasm extracts the media bytes at load
// (OPC self-parse of the slide rels → embed rel → media part), JS decodes them
// to an ImageBitmap, and render_page draws the bitmap at the picture's EMU box.
// A title placeholder labels the slide so the capture is self-describing.

// A small solid-blue 64×48 PNG (deterministic; a single IDAT of one scanline
// pattern). Bytes are inlined so the fixture needs no binary asset in git.
const PPTX_PICTURE_PNG = makeSolidPng(64, 48, 0x25, 0x63, 0xeb);

const PPTX_PIC_CONTENT_TYPES = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
</Types>`;

const PPTX_PIC_SLIDE = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr>
        <p:spPr/>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="en-US" sz="3600" b="1"><a:solidFill><a:srgbClr val="0F172A"/></a:solidFill></a:rPr><a:t>Picture Slide</a:t></a:r></a:p></p:txBody>
      </p:sp>
      <p:pic>
        <p:nvPicPr><p:cNvPr id="4" name="Picture 3"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr>
        <p:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>
        <p:spPr><a:xfrm><a:off x="1828800" y="2286000"/><a:ext cx="5486400" cy="3429000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
      </p:pic>
    </p:spTree>
  </p:cSld>
</p:sld>`;

const PPTX_PIC_SLIDE_RELS = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>
</Relationships>`;

function makePicturesPptx(): Uint8Array {
  return zipSync({
    "[Content_Types].xml": strToU8(PPTX_PIC_CONTENT_TYPES),
    "_rels/.rels": strToU8(PPTX_ROOT_RELS),
    "ppt/presentation.xml": strToU8(PPTX_PRESENTATION),
    "ppt/_rels/presentation.xml.rels": strToU8(PPTX_PRESENTATION_RELS),
    "ppt/slideMasters/slideMaster1.xml": strToU8(PPTX_SLIDE_MASTER),
    "ppt/slideMasters/_rels/slideMaster1.xml.rels": strToU8(PPTX_SLIDE_MASTER_RELS),
    "ppt/slideLayouts/slideLayout1.xml": strToU8(PPTX_SLIDE_LAYOUT),
    "ppt/slideLayouts/_rels/slideLayout1.xml.rels": strToU8(PPTX_SLIDE_LAYOUT_RELS),
    "ppt/slides/slide1.xml": strToU8(PPTX_PIC_SLIDE),
    "ppt/slides/_rels/slide1.xml.rels": strToU8(PPTX_PIC_SLIDE_RELS),
    "ppt/media/image1.png": PPTX_PICTURE_PNG,
  });
}

// ─── tables.pptx — slide tables ──────────────────────────────────────────────
//
// A slide carrying one p:graphicFrame holding a DrawingML a:tbl (§21.1.3): a
// 3-column × 3-row table with a header row (solid fill + bold text), four solid
// borders per cell, and body rows of plain text. The frame xfrm positions the
// table on the slide; the gridCol @w / tr @h drive the cell rails. Exercises the
// table pipeline end-to-end in the demo: column/row rail prefix
// sums, per-cell solidFill + 4-edge borders, and cell text via the reused slide
// text painter. A title placeholder labels the slide so the capture is
// self-describing. Verified by the native pml tests (src/pml/tests.rs::table_*).

// A reusable cell with a 1px (12700 EMU) solid 333333 border on all four edges.
const TBL_BORDERS = `<a:lnL w="12700"><a:solidFill><a:srgbClr val="334155"/></a:solidFill></a:lnL><a:lnR w="12700"><a:solidFill><a:srgbClr val="334155"/></a:solidFill></a:lnR><a:lnT w="12700"><a:solidFill><a:srgbClr val="334155"/></a:solidFill></a:lnT><a:lnB w="12700"><a:solidFill><a:srgbClr val="334155"/></a:solidFill></a:lnB>`;

function tblCell(text: string, opts: { bold?: boolean; fill?: string } = {}): string {
  const b = opts.bold ? ` b="1"` : "";
  const color = opts.bold ? "FFFFFF" : "0F172A";
  const fill = opts.fill ? `<a:solidFill><a:srgbClr val="${opts.fill}"/></a:solidFill>` : "";
  return `<a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1600"${b}><a:solidFill><a:srgbClr val="${color}"/></a:solidFill></a:rPr><a:t>${text}</a:t></a:r></a:p></a:txBody><a:tcPr>${TBL_BORDERS}${fill}</a:tcPr></a:tc>`;
}

const HDR = "1E3A8A"; // header fill (deep blue)
const PPTX_TABLE_SLIDE = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr>
        <p:spPr/>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="en-US" sz="3600" b="1"><a:solidFill><a:srgbClr val="0F172A"/></a:solidFill></a:rPr><a:t>Quarterly Results</a:t></a:r></a:p></p:txBody>
      </p:sp>
      <p:graphicFrame>
        <p:nvGraphicFramePr><p:cNvPr id="5" name="Table 4"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>
        <p:xfrm><a:off x="914400" y="2133600"/><a:ext cx="7315200" cy="2057400"/></p:xfrm>
        <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
          <a:tbl>
            <a:tblPr firstRow="1" bandRow="1"/>
            <a:tblGrid><a:gridCol w="3657600"/><a:gridCol w="1828800"/><a:gridCol w="1828800"/></a:tblGrid>
            <a:tr h="685800">${tblCell("Region", { bold: true, fill: HDR })}${tblCell("Revenue", { bold: true, fill: HDR })}${tblCell("Growth", { bold: true, fill: HDR })}</a:tr>
            <a:tr h="685800">${tblCell("North America")}${tblCell("$4.2M")}${tblCell("+12%")}</a:tr>
            <a:tr h="685800">${tblCell("Europe")}${tblCell("$3.1M")}${tblCell("+8%")}</a:tr>
          </a:tbl>
        </a:graphicData></a:graphic>
      </p:graphicFrame>
    </p:spTree>
  </p:cSld>
</p:sld>`;

function makeTablesPptx(): Uint8Array {
  return zipSync({
    "[Content_Types].xml": strToU8(PPTX_CONTENT_TYPES),
    "_rels/.rels": strToU8(PPTX_ROOT_RELS),
    "ppt/presentation.xml": strToU8(PPTX_PRESENTATION),
    "ppt/_rels/presentation.xml.rels": strToU8(PPTX_PRESENTATION_RELS),
    "ppt/slideMasters/slideMaster1.xml": strToU8(PPTX_SLIDE_MASTER),
    "ppt/slideMasters/_rels/slideMaster1.xml.rels": strToU8(PPTX_SLIDE_MASTER_RELS),
    "ppt/slideLayouts/slideLayout1.xml": strToU8(PPTX_SLIDE_LAYOUT),
    "ppt/slideLayouts/_rels/slideLayout1.xml.rels": strToU8(PPTX_SLIDE_LAYOUT_RELS),
    "ppt/slides/slide1.xml": strToU8(PPTX_TABLE_SLIDE),
    "ppt/slides/_rels/slide1.xml.rels": strToU8(PPTX_SLIDE_RELS),
  });
}

// ─── chart.pptx — slide charts ───────────────────────────────────────────────
//
// A slide carrying one p:graphicFrame whose a:graphicData references a DML chart
// part (c:chart@r:id → ppt/charts/chart1.xml). The chart part is a standard
// c:chartSpace bar chart (§21.2) — the SAME schema WML/SML render, so the shared
// crate::chart::render_chart draws it AS-IS at the frame box. A title placeholder
// labels the slide. Verified by the native pml tests (src/pml/tests.rs::chart_*).

// A clustered-column bar chart: one "Sales" series over four quarters.
const PPTX_CHART_PART = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <c:chart>
    <c:plotArea>
      <c:layout/>
      <c:barChart>
        <c:barDir val="col"/>
        <c:grouping val="clustered"/>
        <c:ser>
          <c:idx val="0"/><c:order val="0"/>
          <c:tx><c:strRef><c:f>Sheet1!$B$1</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>Sales</c:v></c:pt></c:strCache></c:strRef></c:tx>
          <c:cat><c:strRef><c:f>Sheet1!$A$2:$A$5</c:f><c:strCache><c:ptCount val="4"/><c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt><c:pt idx="2"><c:v>Q3</c:v></c:pt><c:pt idx="3"><c:v>Q4</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:f>Sheet1!$B$2:$B$5</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val="4"/><c:pt idx="0"><c:v>18</c:v></c:pt><c:pt idx="1"><c:v>32</c:v></c:pt><c:pt idx="2"><c:v>25</c:v></c:pt><c:pt idx="3"><c:v>41</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
        <c:axId val="1"/><c:axId val="2"/>
      </c:barChart>
      <c:catAx><c:axId val="1"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/><c:crossAx val="2"/></c:catAx>
      <c:valAx><c:axId val="2"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="l"/><c:crossAx val="1"/></c:valAx>
    </c:plotArea>
    <c:legend><c:legendPos val="r"/></c:legend>
    <c:plotVisOnly val="1"/>
  </c:chart>
</c:chartSpace>`;

const PPTX_CHART_SLIDE = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr/>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr>
        <p:spPr/>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="en-US" sz="3600" b="1"><a:solidFill><a:srgbClr val="0F172A"/></a:solidFill></a:rPr><a:t>Sales by Quarter</a:t></a:r></a:p></p:txBody>
      </p:sp>
      <p:graphicFrame>
        <p:nvGraphicFramePr><p:cNvPr id="5" name="Chart 4"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>
        <p:xfrm><a:off x="914400" y="2133600"/><a:ext cx="7315200" cy="3886200"/></p:xfrm>
        <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
          <c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="rId2"/>
        </a:graphicData></a:graphic>
      </p:graphicFrame>
    </p:spTree>
  </p:cSld>
</p:sld>`;

const PPTX_CONTENT_TYPES_CHART = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
  <Override PartName="/ppt/charts/chart1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>
</Types>`;

const PPTX_CHART_SLIDE_RELS = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/>
</Relationships>`;

function makeChartPptx(): Uint8Array {
  return zipSync({
    "[Content_Types].xml": strToU8(PPTX_CONTENT_TYPES_CHART),
    "_rels/.rels": strToU8(PPTX_ROOT_RELS),
    "ppt/presentation.xml": strToU8(PPTX_PRESENTATION),
    "ppt/_rels/presentation.xml.rels": strToU8(PPTX_PRESENTATION_RELS),
    "ppt/slideMasters/slideMaster1.xml": strToU8(PPTX_SLIDE_MASTER),
    "ppt/slideMasters/_rels/slideMaster1.xml.rels": strToU8(PPTX_SLIDE_MASTER_RELS),
    "ppt/slideLayouts/slideLayout1.xml": strToU8(PPTX_SLIDE_LAYOUT),
    "ppt/slideLayouts/_rels/slideLayout1.xml.rels": strToU8(PPTX_SLIDE_LAYOUT_RELS),
    "ppt/slides/slide1.xml": strToU8(PPTX_CHART_SLIDE),
    "ppt/slides/_rels/slide1.xml.rels": strToU8(PPTX_CHART_SLIDE_RELS),
    "ppt/charts/chart1.xml": strToU8(PPTX_CHART_PART),
  });
}

// ─── Multi-slide deck: N slides streamed as N pages (one slide = one page) ────
//
// Every prior pptx fixture is a single slide, so the slide-iteration path (the
// p:sldIdLst → N slide parts → N streamed pages) was never exercised by a
// fixture. This deck has several distinct slides (different titles + bullets)
// so the dropdown/page-count and per-slide rendering are visibly testable.

/** Build a deck from N slide bodies, reusing the shared master/layout. Each
 *  slide is a title placeholder + bulleted body; `slides[i] = { title, body[] }`. */
function makeMultiSlidePptx(slides: { title: string; body: string[] }[]): Uint8Array {
  const slideXml = (s: { title: string; body: string[] }): string =>
    `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree>
    <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr>
      <p:spPr/>
      <p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="4000" b="1"><a:solidFill><a:srgbClr val="0F172A"/></a:solidFill></a:rPr><a:t>${s.title}</a:t></a:r></a:p></p:txBody>
    </p:sp>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="3" name="Body 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
      <p:spPr/>
      <p:txBody><a:bodyPr/><a:lstStyle/>${s.body
        .map((line) => `<a:p><a:r><a:rPr lang="en-US" sz="2000"/><a:t>${line}</a:t></a:r></a:p>`)
        .join("")}</p:txBody>
    </p:sp>
  </p:spTree></p:cSld>
</p:sld>`;

  const n = slides.length;
  // rId1 is the master; slides take rId2..rId(n+1). sldId numbering from 256.
  const presentation = PPTX_PRESENTATION.replace(
    /<p:sldIdLst>.*<\/p:sldIdLst>/,
    `<p:sldIdLst>${slides
      .map((_, i) => `<p:sldId id="${256 + i}" r:id="rId${i + 2}"/>`)
      .join("")}</p:sldIdLst>`,
  );
  const presRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
${slides
  .map(
    (_, i) =>
      `  <Relationship Id="rId${i + 2}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide${i + 1}.xml"/>`,
  )
  .join("\n")}
</Relationships>`;
  const contentTypes = PPTX_CONTENT_TYPES.replace(
    '<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>',
    slides
      .map(
        (_, i) =>
          `<Override PartName="/ppt/slides/slide${i + 1}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>`,
      )
      .join("\n  "),
  );

  const parts: Record<string, Uint8Array> = {
    "[Content_Types].xml": strToU8(contentTypes),
    "_rels/.rels": strToU8(PPTX_ROOT_RELS),
    "ppt/presentation.xml": strToU8(presentation),
    "ppt/_rels/presentation.xml.rels": strToU8(presRels),
    "ppt/slideMasters/slideMaster1.xml": strToU8(PPTX_SLIDE_MASTER),
    "ppt/slideMasters/_rels/slideMaster1.xml.rels": strToU8(PPTX_SLIDE_MASTER_RELS),
    "ppt/slideLayouts/slideLayout1.xml": strToU8(PPTX_SLIDE_LAYOUT),
    "ppt/slideLayouts/_rels/slideLayout1.xml.rels": strToU8(PPTX_SLIDE_LAYOUT_RELS),
  };
  slides.forEach((s, i) => {
    parts[`ppt/slides/slide${i + 1}.xml`] = strToU8(slideXml(s));
    parts[`ppt/slides/_rels/slide${i + 1}.xml.rels`] = strToU8(PPTX_SLIDE_RELS);
  });
  void n;
  return zipSync(parts);
}

const pptxFixtures: { path: string; pptx: Uint8Array }[] = [
  { path: "ppt/title-body.pptx", pptx: makePptx() },
  { path: "ppt/fills-colors.pptx", pptx: makeFillsColorsPptx() },
  { path: "ppt/pictures.pptx", pptx: makePicturesPptx() },
  { path: "ppt/tables.pptx", pptx: makeTablesPptx() },
  { path: "ppt/chart.pptx", pptx: makeChartPptx() },
  {
    path: "ppt/multi-slide.pptx",
    pptx: makeMultiSlidePptx([
      { title: "Agenda", body: ["Project overview", "Quarterly results", "Next steps"] },
      { title: "Project Overview", body: ["Three Office formats render", "One feature-gated binary"] },
      { title: "Quarterly Results", body: ["Revenue up across regions", "Margins held steady"] },
      { title: "Next Steps", body: ["Real-file testing", "Master-tier fidelity"] },
    ]),
  },
];

// ─── Main ─────────────────────────────────────────────────────────────────────

let count = 0;
for (const f of fixtures) {
  const fullPath = join(DEST, f.path);
  mkdirSync(join(fullPath, ".."), { recursive: true });
  writeFileSync(fullPath, makeDocx(f.body));
  count++;
}

for (const f of watermarkFixtures) {
  const fullPath = join(DEST, f.path);
  mkdirSync(join(fullPath, ".."), { recursive: true });
  writeFileSync(fullPath, f.docx);
  count++;
}

for (const f of commentFixtures) {
  const fullPath = join(DEST, f.path);
  mkdirSync(join(fullPath, ".."), { recursive: true });
  writeFileSync(fullPath, f.docx);
  count++;
}

for (const f of numberingFixtures) {
  const fullPath = join(DEST, f.path);
  mkdirSync(join(fullPath, ".."), { recursive: true });
  writeFileSync(fullPath, f.docx);
  count++;
}

for (const f of toggleFixtures) {
  const fullPath = join(DEST, f.path);
  mkdirSync(join(fullPath, ".."), { recursive: true });
  writeFileSync(fullPath, f.docx);
  count++;
}

for (const f of inlineImageFixtures) {
  const fullPath = join(DEST, f.path);
  mkdirSync(join(fullPath, ".."), { recursive: true });
  writeFileSync(fullPath, f.docx);
  count++;
}

for (const f of xlsxFixtures) {
  const fullPath = join(DEST, f.path);
  mkdirSync(join(fullPath, ".."), { recursive: true });
  writeFileSync(fullPath, f.xlsx);
  count++;
}

for (const f of pptxFixtures) {
  const fullPath = join(DEST, f.path);
  mkdirSync(join(fullPath, ".."), { recursive: true });
  writeFileSync(fullPath, f.pptx);
  count++;
}

// CSV fixture — routed through the browser-side csvToXlsx converter at load.
const SAMPLE_CSV = [
  "Order,Customer,Region,Amount,Paid,Notes",
  '1001,"Acme, Inc.",North,1299.99,TRUE,"Said ""rush order"" on the phone"',
  "1002,Globex,South,549,FALSE,Net 30",
  "1003,Initech,East,12500,TRUE,",
  "1004,Umbrella,West,89.5,TRUE,Partial shipment",
  "1005,Hooli,North,3998,FALSE,Awaiting PO",
].join("\r\n");
{
  const fullPath = join(DEST, "spreadsheet/sample.csv");
  mkdirSync(join(fullPath, ".."), { recursive: true });
  writeFileSync(fullPath, SAMPLE_CSV);
  count++;
}

console.log(`generated ${count} test fixtures into ${DEST}`);
