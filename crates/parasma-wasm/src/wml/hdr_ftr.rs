//! Headers & footers (§17.10) — layout and per-page positioning of the
//! header/footer bands.
//!
//! Depends on the header/footer cache (`HdrFtrCache`) and per-page line model
//! (`PagedLine`) from `wml::page`, the section header/footer model
//! (`SectionHdrFtr`) from `wml::loader`, plus paragraph layout. `run_layout_impl`
//! (which stays in `lib.rs`) drives section header/footer layout via
//! `layout_section_hdr_ftr` then positions the cached lines per page via
//! `position_hdr_ftr_lines`; `render_page` (also in `lib.rs`) only reads the
//! resulting `header_lines`/`footer_lines`.

use std::collections::HashMap;

use wasm_bindgen::JsValue;

use ooxml_wml::ext::StyleContext;
use ooxml_wml::types::{BlockContent, STJcTable};

use crate::engine::fonts::{FaceCache, FontCache, ThemeFonts};
use crate::wml::linebreak::{measure_item_text, FieldCode, Item};
use crate::wml::loader::SectionHdrFtr;
use crate::wml::page::{
    CellBlock, HdrFtrCache, HfBlock, PagedLine, PagedTableCell, PagedTableRow, PARA_SENTINEL,
};
use crate::wml::paragraph::layout_paragraph;
use crate::wml::table::{layout_table, CellVAlign, TableCache};

// ─── Per-page layout (the streaming output) ──────────────────────────────────

pub(crate) struct LaidOutHdrFtr {
    pub(crate) default_header: Option<HdrFtrCache>,
    pub(crate) first_header: Option<HdrFtrCache>,
    pub(crate) even_header: Option<HdrFtrCache>,
    pub(crate) default_footer: Option<HdrFtrCache>,
    pub(crate) first_footer: Option<HdrFtrCache>,
    pub(crate) even_footer: Option<HdrFtrCache>,
    pub(crate) title_pg: bool,
    pub(crate) even_odd: bool,
}

impl LaidOutHdrFtr {
    pub(crate) fn header_for_page(&self, section_page_idx: usize) -> Option<&HdrFtrCache> {
        if section_page_idx == 0 && self.title_pg {
            self.first_header.as_ref().or(self.default_header.as_ref())
        } else if section_page_idx % 2 == 1 && self.even_odd {
            self.even_header.as_ref().or(self.default_header.as_ref())
        } else {
            self.default_header.as_ref()
        }
    }
    pub(crate) fn footer_for_page(&self, section_page_idx: usize) -> Option<&HdrFtrCache> {
        if section_page_idx == 0 && self.title_pg {
            self.first_footer.as_ref().or(self.default_footer.as_ref())
        } else if section_page_idx % 2 == 1 && self.even_odd {
            self.even_footer.as_ref().or(self.default_footer.as_ref())
        } else {
            self.default_footer.as_ref()
        }
    }
}

/// A block resolved to its renderable form, with block-level content controls
/// (`<w:sdt>`) unwrapped. Word commonly wraps header/footer page-number fields
/// in an sdt; without unwrapping, the content is silently dropped.
pub(crate) enum FlatBlock<'a> {
    Para(&'a ooxml_wml::types::Paragraph),
    Table(&'a ooxml_wml::types::Table),
}

/// Flatten `block_content`, descending into block-level `<w:sdt>` content
/// controls so their inner paragraphs/tables are rendered. Other block markup
/// (bookmarks, ranges, customXml) is skipped, matching prior behavior.
pub(crate) fn flatten_blocks<'a>(blocks: &'a [BlockContent], out: &mut Vec<FlatBlock<'a>>) {
    for b in blocks {
        match b {
            BlockContent::P(p) => out.push(FlatBlock::Para(p)),
            BlockContent::Tbl(t) => out.push(FlatBlock::Table(t)),
            BlockContent::Sdt(sdt) => {
                if let Some(content) = &sdt.sdt_content {
                    flatten_block_choices(&content.block_content, out);
                }
            }
            _ => {}
        }
    }
}

/// Sdt inner content is a sibling enum (`BlockContentChoice`); same flattening.
fn flatten_block_choices<'a>(
    blocks: &'a [ooxml_wml::types::BlockContentChoice],
    out: &mut Vec<FlatBlock<'a>>,
) {
    use ooxml_wml::types::BlockContentChoice as BC;
    for b in blocks {
        match b {
            BC::P(p) => out.push(FlatBlock::Para(p)),
            BC::Tbl(t) => out.push(FlatBlock::Table(t)),
            BC::Sdt(sdt) => {
                if let Some(content) = &sdt.sdt_content {
                    flatten_block_choices(&content.block_content, out);
                }
            }
            _ => {}
        }
    }
}

/// Total laid-out height of a header/footer table = sum of row heights
/// (§17.4.6). Header tables are not paginated inside the band (the band is
/// shallow and a header table is normally small), so the simple row-sum is the
/// band contribution.
pub(crate) fn hf_table_height(table: &TableCache) -> f64 {
    table.rows.iter().map(|r| r.height_px).sum()
}

fn layout_hdr_ftr_blocks(
    blocks: &[BlockContent],
    font_cache: &FontCache,
    content_width: f64,
    style_ctx: &StyleContext,
    default_ppr: Option<&ooxml_wml::types::CTPPrGeneral>,
    default_tab_stop_px: f64,
    theme_fonts: &ThemeFonts,
) -> Result<HdrFtrCache, JsValue> {
    let mut out_blocks = Vec::new();
    let mut total_height = 0.0;
    let mut hf_prev_after: f64 = 0.0;
    let mut flat = Vec::new();
    flatten_blocks(blocks, &mut flat);
    // Header/footer tables don't carry list markers in practice; pass empty
    // numbering so `layout_table` resolves no counters (§17.9 not exercised here).
    let numbering_levels = HashMap::new();
    let mut list_counters = HashMap::new();
    for block in &flat {
        match block {
            FlatBlock::Para(p) => {
                let cache = layout_paragraph(font_cache, p, content_width, None, style_ctx, default_ppr, default_tab_stop_px, theme_fonts, None, &[], &[], None, false, None)?;
                let gap = hf_prev_after.max(cache.layout.space_before_px);
                total_height += gap;
                for line in &cache.lines {
                    total_height += cache.layout.line_height(line.max_font_pt, line.max_ascent + line.max_descent, line.max_image_height);
                }
                // Paragraph borders (e.g. the footer's top rule) reserve their own
                // space + line width above/below the text, which counts toward the
                // header/footer band height (and thus the content area).
                if let Some(ref b) = cache.layout.border_top {
                    total_height += b.space_px + b.width_px;
                }
                if let Some(ref b) = cache.layout.border_bottom {
                    total_height += b.space_px + b.width_px;
                }
                hf_prev_after = cache.layout.space_after_px;
                out_blocks.push(HfBlock::Paragraph(cache));
            }
            FlatBlock::Table(t) => {
                // §17.10: lay the header/footer table out with the body table
                // machinery, constrained to the band width. It contributes its
                // row-sum height to the band; the after-spacing carry resets
                // (a table has no space_after of its own here).
                let cache = layout_table(font_cache, t, content_width, style_ctx, default_ppr, default_tab_stop_px, theme_fonts, &numbering_levels, &mut list_counters, None)?;
                total_height += hf_prev_after;
                total_height += hf_table_height(&cache);
                hf_prev_after = 0.0;
                out_blocks.push(HfBlock::Table(cache));
            }
        }
    }
    Ok(HdrFtrCache { blocks: out_blocks, total_height })
}

pub(crate) fn layout_section_hdr_ftr(
    hdr_ftr: &SectionHdrFtr,
    font_cache: &FontCache,
    content_width: f64,
    style_ctx: &StyleContext,
    default_ppr: Option<&ooxml_wml::types::CTPPrGeneral>,
    default_tab_stop_px: f64,
    theme_fonts: &ThemeFonts,
) -> Result<LaidOutHdrFtr, JsValue> {
    let lay = |blocks: &Option<Vec<BlockContent>>| -> Result<Option<HdrFtrCache>, JsValue> {
        match blocks {
            Some(b) if !b.is_empty() => Ok(Some(layout_hdr_ftr_blocks(b, font_cache, content_width, style_ctx, default_ppr, default_tab_stop_px, theme_fonts)?)),
            _ => Ok(None),
        }
    };
    Ok(LaidOutHdrFtr {
        default_header: lay(&hdr_ftr.default_header)?,
        first_header: lay(&hdr_ftr.first_header)?,
        even_header: lay(&hdr_ftr.even_header)?,
        default_footer: lay(&hdr_ftr.default_footer)?,
        first_footer: lay(&hdr_ftr.first_footer)?,
        even_footer: lay(&hdr_ftr.even_footer)?,
        title_pg: hdr_ftr.title_pg,
        even_odd: hdr_ftr.even_odd,
    })
}

/// Position a header/footer cache into its band at `y_start`. Paragraphs become
/// `PagedLine`s; block-level tables (§17.10) become `PagedTableRow`s placed at
/// the band origin (no in-band pagination — see `hf_table_height`).
// Positional orchestrator args; struct-ification deferred to a dedicated pass.
#[allow(clippy::too_many_arguments)]
pub(crate) fn position_hdr_ftr_lines(
    cache: &HdrFtrCache,
    y_start: f64,
    page_w: f64,
    margin_left: f64,
    margin_right: f64,
    page_number: usize,
    total_pages: Option<usize>,
    font_cache: &FontCache,
) -> (Vec<PagedLine>, Vec<PagedTableRow>) {
    // Shaping scope — one built face per (font, band) for the PAGE/NUMPAGES re-measures.
    let faces = FaceCache::new();
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let mut y = y_start;
    let mut pos_prev_after: f64 = 0.0;
    // Body content area left edge (= page left margin); tables align relative to it.
    let band_left = margin_left;
    let band_width = page_w - margin_left - margin_right;
    for block in &cache.blocks {
        match block {
            HfBlock::Paragraph(pc) => {
                let gap = pos_prev_after.max(pc.layout.space_before_px);
                y += gap;
                for (li, line) in pc.lines.iter().enumerate() {
                    let indent = if li == 0 { pc.layout.indent_first_line_px } else { 0.0 };
                    let baseline_y = y + line.max_ascent;
                    let items: Vec<Item> = line.items.iter().map(|item| {
                        // Live PAGE/NUMPAGES fields: substitute the result AND re-measure
                        // it — the cached result is one digit ("1"), so without re-width
                        // a multi-digit page number overruns the following " | Page" text.
                        let new_text = match item.field {
                            Some(FieldCode::Page) => Some(page_number.to_string()),
                            Some(FieldCode::NumPages) => Some(total_pages.map_or("?".to_string(), |n| n.to_string())),
                            None => None,
                        };
                        match new_text {
                            Some(nt) if nt != item.text => {
                                let mut item = item.clone();
                                let (w, cw, shaped) = measure_item_text(&nt, &item.style, font_cache, &faces);
                                item.text = nt;
                                item.width = w;
                                item.content_width = cw;
                                item.shaped = shaped;
                                item
                            }
                            _ => item.clone(),
                        }
                    }).collect();
                    // Right/centre alignment must use the post-substitution line width, or
                    // a widened page number shifts the whole footer off its margin.
                    let line_w: f64 = items.iter().map(|i| i.width).sum();
                    let x_start = pc.layout.x_start(page_w, margin_left, margin_right, line_w, indent);
                    let lh = pc.layout.line_height(line.max_font_pt, line.max_ascent + line.max_descent, line.max_image_height);
                    lines.push(PagedLine { segments: PagedLine::single(items, x_start), para: PARA_SENTINEL, baseline_y, para_shading: None, line_height: lh, content_left: x_start, content_right: x_start + 200.0, border_top: None, border_bottom: None, border_left: None, border_right: None, bar_tabs: pc.tab_stops.bar_xs(margin_left + pc.layout.indent_left_px, page_w - margin_right - pc.layout.indent_right_px, pc.layout.base_rtl), is_first_line: false, is_last_line: false });
                    y += lh;
                }
                pos_prev_after = pc.layout.space_after_px;
            }
            HfBlock::Table(table) => {
                y += pos_prev_after;
                pos_prev_after = 0.0;
                position_hf_table(table, &mut rows, &mut y, band_left, band_width);
            }
        }
    }
    (lines, rows)
}

/// Lay a header/footer table's rows out as `PagedTableRow`s at the running band
/// `y`. Mirrors the body's whole-row placement (§17.4): table alignment offset,
/// per-cell margins, vertical alignment, and cell text lines. No page break —
/// the band is shallow and a header table is small; if a row would overrun, it
/// simply renders clipped (clamps gracefully).
fn position_hf_table(
    table: &TableCache,
    rows: &mut Vec<PagedTableRow>,
    y: &mut f64,
    band_left: f64,
    band_width: f64,
) {
    let table_width: f64 = table.col_widths_px.iter().sum();
    let table_offset = match table.alignment {
        STJcTable::Center => (band_width - table_width) / 2.0,
        STJcTable::End | STJcTable::Right => band_width - table_width,
        _ => table.indent_px,
    };
    let cell_x_start = band_left + table_offset;
    let tbl_cm_h = table.cell_margin_h;
    let tbl_cm_v = table.cell_margin_v;

    for row in &table.rows {
        let row_y_top = *y;
        let mut cell_x = cell_x_start;
        let mut paged_cells = Vec::with_capacity(row.cells.len());
        for cell in &row.cells {
            let mut cell_lines = Vec::new();
            let cell_vmar = cell.mar_top + cell.mar_bottom;
            let v_offset = match cell.v_align {
                CellVAlign::Center => {
                    let ch = cell.content_height(0.0);
                    ((row.height_px - cell_vmar) - ch).max(0.0) / 2.0
                }
                CellVAlign::Bottom => {
                    let ch = cell.content_height(0.0);
                    ((row.height_px - cell_vmar) - ch).max(0.0)
                }
                CellVAlign::Top => 0.0,
            };
            let mut cell_y = row_y_top + cell.mar_top + v_offset;
            let mut cell_prev_after: f64 = 0.0;
            for block in &cell.blocks {
                if let CellBlock::Paragraph(pc) = block {
                    let gap = cell_prev_after.max(pc.layout.space_before_px);
                    cell_y += gap;
                    for (li, line) in pc.lines.iter().enumerate() {
                        let lh = pc.layout.line_height(line.max_font_pt, line.max_ascent + line.max_descent, line.max_image_height);
                        let indent_extra = if li == 0 { pc.layout.indent_first_line_px } else { 0.0 };
                        let x_start = pc.layout.x_start(
                            cell_x + cell.width_px,
                            cell_x + cell.mar_left,
                            cell.mar_left,
                            line.width(),
                            indent_extra,
                        );
                        cell_lines.push(PagedLine {
                            segments: PagedLine::single(line.items.clone(), x_start),
                            para: PARA_SENTINEL,
                            baseline_y: cell_y + line.max_ascent,
                            para_shading: None,
                            line_height: lh,
                            content_left: x_start,
                            content_right: cell_x + cell.width_px - cell.mar_right,
                            border_top: None, border_bottom: None,
                            border_left: None, border_right: None,
                            bar_tabs: pc.tab_stops.bar_xs(
                                cell_x + cell.mar_left + pc.layout.indent_left_px,
                                cell_x + cell.width_px - cell.mar_right - pc.layout.indent_right_px,
                                pc.layout.base_rtl),
                            is_first_line: false, is_last_line: false,
                        });
                        cell_y += lh;
                    }
                    cell_prev_after = pc.layout.space_after_px;
                }
                // Nested tables inside a header/footer cell are not expanded
                // (out of scope — a header table is a flat grid in practice).
            }
            paged_cells.push(PagedTableCell {
                lines: cell_lines,
                x: cell_x,
                width_px: cell.width_px,
                fill_color: cell.fill_color.clone(),
                v_merged_away: cell.v_merged_away,
                border_top: cell.border_top.clone(),
                border_bottom: cell.border_bottom.clone(),
                border_left: cell.border_left.clone(),
                border_right: cell.border_right.clone(),
            });
            cell_x += cell.width_px;
        }
        rows.push(PagedTableRow {
            cells: paged_cells,
            y_top: row_y_top,
            height_px: row.height_px,
            border_color: table.border_color.clone(),
            has_borders: table.has_borders,
            cell_margin_h: tbl_cm_h,
            cell_margin_v: tbl_cm_v,
        });
        *y += row.height_px;
    }
}

#[cfg(test)]
mod footer_parse_tests {
    use super::*;
    use ooxml_wml::ext::{BodyExt, ParagraphExt};
    use std::io::Cursor;

    // A SYNTHETIC minimum-repro docx (built in-test, no external fixture) whose
    // footer wraps its "N | Page" field in a block-level <w:sdt> content control
    // — the structure some apps emit that originally surfaced as a blank footer.
    // flatten_blocks must descend into the sdt so the page-number paragraph is
    // laid out (previously the sdt was dropped and only a trailing empty <w:p>
    // remained → blank footer).
    fn footer_sdt_docx() -> Vec<u8> {
        use std::io::Write;
        const CT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/footer1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"/>
</Types>"#;
        const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <w:body>
    <w:p><w:r><w:t>Body</w:t></w:r></w:p>
    <w:sectPr>
      <w:footerReference w:type="default" r:id="rIdF1"/>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="720" w:right="720" w:bottom="720" w:left="720" w:header="432" w:footer="576" w:gutter="0"/>
    </w:sectPr>
  </w:body>
</w:document>"#;
        const DOC_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rIdF1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/>
</Relationships>"#;
        // The page-number paragraph lives INSIDE the sdt; a trailing empty <w:p>
        // is the only DIRECT block (mirrors the real-world blank-footer case).
        const FOOTER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:ftr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:sdt><w:sdtPr/><w:sdtContent>
    <w:p><w:r><w:t xml:space="preserve">1 | Page</w:t></w:r></w:p>
  </w:sdtContent></w:sdt>
  <w:p/>
</w:ftr>"#;
        let parts: &[(&str, &str)] = &[
            ("[Content_Types].xml", CT),
            ("_rels/.rels", ROOT_RELS),
            ("word/document.xml", DOC),
            ("word/_rels/document.xml.rels", DOC_RELS),
            ("word/footer1.xml", FOOTER),
        ];
        let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts: zip::write::SimpleFileOptions =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, content) in parts {
            zw.start_file(*name, opts).expect("zip start_file");
            zw.write_all(content.as_bytes()).expect("zip write");
        }
        zw.finish().expect("zip finish").into_inner()
    }

    #[test]
    fn footer_sdt_is_unwrapped_to_page_number() {
        let mut doc = ooxml_wml::Document::from_reader(Cursor::new(footer_sdt_docx())).expect("parse");
        let footer_id = doc.body().section_properties().and_then(|sp| {
            sp.header_footer_refs.iter().find_map(|r| match r {
                ooxml_wml::types::HeaderFooterRef::FooterReference(f) => Some(f.id.clone()),
                _ => None,
            })
        }).expect("footer reference");
        let ftr = doc.get_footer(&footer_id).expect("get_footer");

        // Without unwrapping, no direct paragraph carries the field/text.
        let direct_text: String = ftr.block_content.iter().filter_map(|b| match b {
            BlockContent::P(p) => Some(p.text()),
            _ => None,
        }).collect();
        assert!(!direct_text.contains("Page"), "test premise: field is inside the sdt, not a direct paragraph");

        // After flattening, the page-number paragraph (with the " | Page" run)
        // is reachable.
        let mut flat = Vec::new();
        flatten_blocks(&ftr.block_content, &mut flat);
        let has_page_text = flat.iter().any(|b| matches!(b, FlatBlock::Para(p) if p.text().contains("Page")));
        assert!(has_page_text, "flatten_blocks should surface the sdt-wrapped 'Page' paragraph");
    }
}
