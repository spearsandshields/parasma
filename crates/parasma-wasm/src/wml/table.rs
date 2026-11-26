//! Tables (ECMA-376 §17.4) — table layout, cell caches, conditional
//! formatting, and cell/table border resolution. Relocated VERBATIM from
//! `lib.rs` (chapter-aligned module boundaries).
//!
//! The *build-side* table caches live here (`TableCache`/`TableRowCache`/
//! `TableCellCache` produced by `layout_table`). The *page-model* types
//! (`PagedTableRow`/`PagedTableCell`) live in `wml::page` with the rest of
//! the page model — only the cell-border type `CellBorderEdge` they share is
//! table-domain and lives here.
//!
//! Cell borders (§17.4.42) stay table-domain (paragraph and page borders
//! live in `wml::border`; cell borders are §17.4).

use crate::wml::content_walk::{flatten_blocks, FlatBlock};
use std::collections::HashMap;

use wasm_bindgen::prelude::*;

use ooxml_wml::ext::{CellExt, StyleContext, TableExt};
use ooxml_wml::types::{STBorder, STJcTable, STTblWidth};

use crate::engine::color::ooxml_color_to_css;
use crate::engine::fonts::{FontCache, ThemeFonts, DEFAULT_FONT_PT};
use crate::engine::units::twips_to_px;
use crate::wml::numbering::{resolve_numbering_marker, ListCounters, NumberingLevelDef};
use crate::{
    layout_paragraph, CellBlock, Item, DEFAULT_CELL_MARGIN_H_TWIPS, DEFAULT_CELL_MARGIN_V_TWIPS,
};
use crate::wml::drawing::WrapMode;
use crate::wml::float::{
    build_active_float, clear_below_floats, pack_lines_around_floats, ActiveFloat, WRAP_EPS,
};
use crate::wml::float_resolve::{resolve_anchor_position, FrameCtx};
use crate::wml::page::{PagedLine, PARA_SENTINEL};
use crate::wml::loader::DEFAULT_TABLE_STYLE_KEY;
use crate::wml::paragraph::{gap_prev, paragraph_gap, style_chain, ParagraphLayout};
use crate::wml::render::layout_text_box;

// ─── Table layout cache ─────────────────────────────────────────────────────

pub(crate) struct TableCache {
    pub(crate) col_widths_px: Vec<f64>,
    pub(crate) rows: Vec<TableRowCache>,
    pub(crate) border_color: String,
    pub(crate) has_borders: bool,
    pub(crate) alignment: STJcTable,
    pub(crate) indent_px: f64,
}

pub(crate) struct TableRowCache {
    pub(crate) cells: Vec<TableCellCache>,
    pub(crate) height_px: f64,
    /// §17.4.6 `w:cantSplit` (trPr): if set, the row is not broken across a
    /// page boundary — it is placed whole, pushed to a fresh page if it does not
    /// fit the page remainder. When unset (the default here), a row taller than
    /// the remaining space splits across the break at line granularity. A row
    /// that fits on no page splits either way.
    pub(crate) cant_split: bool,
}

pub(crate) struct TableCellCache {
    pub(crate) blocks: Vec<CellBlock>,
    pub(crate) col_start: usize,
    pub(crate) col_span: usize,
    /// The cell's left edge, from the table's: the grid columns before it.
    pub(crate) x_px: f64,
    pub(crate) width_px: f64,
    pub(crate) fill_color: Option<String>,
    pub(crate) v_align: CellVAlign,
    pub(crate) v_merged_away: bool,
    pub(crate) border_top: Option<CellBorderEdge>,
    pub(crate) border_bottom: Option<CellBorderEdge>,
    pub(crate) border_left: Option<CellBorderEdge>,
    pub(crate) border_right: Option<CellBorderEdge>,
    /// §17.4.74 / §17.4.75 diagonal borders, top-left to bottom-right and
    /// top-right to bottom-left. Boxed: few cells have them.
    pub(crate) diagonals: Option<Box<CellDiagonals>>,
    // Effective per-cell margins in px (§17.4.42 tcMar overrides the table/row
    // default §17.4.41/§17.4.43 tblCellMar). Default to the table-level value.
    pub(crate) mar_top: f64,
    pub(crate) mar_bottom: f64,
    pub(crate) mar_left: f64,
    pub(crate) mar_right: f64,
    /// Pre-resolved layout for a cell hosting a WRAPPING anchored float
    /// (§20.4.2.17, in-cell float wrap). `None` for the overwhelmingly common
    /// float-free cell — that case keeps the verbatim float-blind emit path.
    pub(crate) float_layout: Option<CellFloatLayout>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellVAlign { Top, Center, Bottom }

/// A cell's diagonal borders.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CellDiagonals {
    pub(crate) down: Option<CellBorderEdge>,
    pub(crate) up: Option<CellBorderEdge>,
}

/// A cell's own diagonal borders, if it sets any.
fn cell_diagonals(theme: &ThemeFonts, cell: &ooxml_wml::types::TableCell) -> Option<Box<CellDiagonals>> {
    let b = cell.properties().and_then(|p| p.tc_borders.as_ref())?;
    let down = border_to_edge(theme, &b.tl2br);
    let up = border_to_edge(theme, &b.tr2bl);
    (down.is_some() || up.is_some()).then(|| Box::new(CellDiagonals { down, up }))
}

/// A table cell's float-aware layout, resolved once at cache-build time in
/// CELL-LOCAL coordinates (origin = cell content top-left, i.e. cell.mar_left /
/// cell content top). The page-assembly emit seam translates these by the
/// cell's page origin. Keeping this in the cache makes the size pass
/// (`content_height`) and the emit pass agree from a single pack.
pub(crate) struct CellFloatLayout {
    /// The cell's paragraph text, re-packed around the floats; cell-local
    /// `segments[].x_start` (rel. cell left) and `baseline_y` (rel. cell top).
    pub(crate) lines: Vec<PagedLine>,
    /// The floats to draw, cell-local top-left.
    pub(crate) draws: Vec<CellFloatDraw>,
    /// Float-aware cell content height (PRE v-margin; `content_height` adds it).
    pub(crate) content_height: f64,
}

pub(crate) enum CellFloatDraw {
    // `anchor_para` — the hosting paragraph's document-order id (anchored-image
    // copy), forwarded verbatim onto the emitted `AnchoredImageLayout`.
    Image { rel_id: String, x: f64, y: f64, width_px: f64, height_px: f64, behind_text: bool, anchor_para: Option<u32> },
    TextBox {
        lines: Vec<PagedLine>, // box-local (untranslated), as PositionedTextBox expects
        x: f64, y: f64, width_px: f64, height_px: f64, behind_text: bool,
        frame: crate::wml::drawing::TextBoxFrame,
    },
}

impl TableCellCache {
    pub(crate) fn content_height(&self, v_margin_total: f64) -> f64 {
        // Float cells carry a pre-computed, float-aware height (so the size pass
        // and the emit pass agree). Float-free cells take the verbatim sum.
        if let Some(fl) = &self.float_layout {
            return fl.content_height + v_margin_total;
        }
        let mut total = 0.0_f64;
        let mut prev_after = 0.0_f64;
        let mut prev_para: Option<&ParagraphLayout> = None;
        for block in &self.blocks {
            match block {
                CellBlock::Paragraph(pc) => {
                    total += paragraph_gap(Some(gap_prev(prev_para, prev_after)), &pc.layout);
                    total += pc.layout.border_reserve_top() + pc.layout.border_reserve_bottom();
                    total += pc.lines.iter().map(|l| pc.layout.line_height(l.max_font_pt, l.max_ascent + l.max_descent, l.max_image_height)).sum::<f64>();
                    prev_after = pc.layout.space_after_px;
                    prev_para = Some(&pc.layout);
                }
                CellBlock::Table(tc) => {
                    total += prev_after;
                    total += tc.rows.iter().map(|r| r.height_px).sum::<f64>();
                    prev_after = 0.0;
                    prev_para = None;
                }
            }
        }
        // Word includes the LAST paragraph's after-spacing in the cell (hence
        // row) height — e.g. a cell whose only paragraph is TextBody (after=225)
        // makes the row ~15px taller. Dropping it (only summing gaps *between*
        // paragraphs) made every such row ~15px short and the whole table
        // visibly shorter than Word's.
        total + prev_after + v_margin_total
    }
}

/// A single cell line flattened for row-splitting (§17.4.6, split at line
/// granularity). `rel_top` is the line's top offset from the cell's
/// *content* top (i.e. excluding the cell's top margin and any vertical-align
/// offset); `height` is its line height. `line` is the fully-built `PagedLine`
/// whose `baseline_y` is stored *relative* to the same content top — a fragment
/// places it by adding its content-top page position. Flattening lets the
/// splitter partition a cell's lines into the lines that fit on the current
/// page and the remainder carried to a continuation fragment.
pub(crate) struct CellLineProto {
    pub(crate) rel_top: f64,
    pub(crate) height: f64,
    pub(crate) line: PagedLine,
}

/// Flatten a table cell's blocks into content-relative line protos, mirroring
/// the whole-row emit walk (paragraphs with before/after gaps + nested tables)
/// but recording each line's offset from the cell content top instead of an
/// absolute baseline. `cell_x` is the cell's left edge on the page (constant
/// across a split, since fragments stay in the same column). Used only on the
/// split path; the whole-row path keeps its original inline emit byte-for-byte.
pub(crate) fn flatten_cell_lines(cell: &TableCellCache, cell_x: f64) -> Vec<CellLineProto> {
    let mut protos = Vec::new();
    let mut cur: f64 = 0.0; // running offset from cell content top
    let mut prev_after: f64 = 0.0;
    let mut prev_para: Option<&ParagraphLayout> = None;
    for block in &cell.blocks {
        match block {
            CellBlock::Paragraph(pc) => {
                cur += paragraph_gap(Some(gap_prev(prev_para, prev_after)), &pc.layout);
                cur += pc.layout.border_reserve_top();
                let n = pc.lines.len();
                let (box_left, box_right) = (cell_x + cell.mar_left, cell_x + cell.width_px - cell.mar_right);
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
                    protos.push(CellLineProto {
                        rel_top: cur,
                        height: lh,
                        line: PagedLine {
                            segments: PagedLine::single(line.items.clone(), x_start),
                            para: pc.para_id.unwrap_or(PARA_SENTINEL),
                            baseline_y: cur + line.max_ascent,
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
                        }.decorated(&pc.layout, li, n, box_left, box_right),
                    });
                    cur += lh;
                }
                cur += pc.layout.border_reserve_bottom();
                prev_after = pc.layout.space_after_px;
                prev_para = Some(&pc.layout);
            }
            CellBlock::Table(nested) => {
                cur += prev_after;
                prev_para = None;
                let nested_x = cell_x + cell.mar_left;
                for nrow in &nested.rows {
                    let nrow_top = cur;
                    for ncell in &nrow.cells {
                        if ncell.v_merged_away { continue; }
                        let ncell_x = nested_x + ncell.x_px;
                        let mut ny = nrow_top + ncell.mar_top;
                        for nb in &ncell.blocks {
                            if let CellBlock::Paragraph(npc) = nb {
                                ny += npc.layout.border_reserve_top();
                                let nn = npc.lines.len();
                                let (nleft, nright) = (ncell_x + ncell.mar_left, ncell_x + ncell.width_px - ncell.mar_right);
                                for (nli, nline) in npc.lines.iter().enumerate() {
                                    let nlh = npc.layout.line_height(nline.max_font_pt, nline.max_ascent + nline.max_descent, nline.max_image_height);
                                    let nindent = if nli == 0 { npc.layout.indent_first_line_px } else { 0.0 };
                                    let nx = npc.layout.x_start(
                                        ncell_x + ncell.width_px,
                                        ncell_x + ncell.mar_left,
                                        ncell.mar_left,
                                        nline.width(), nindent,
                                    );
                                    protos.push(CellLineProto {
                                        rel_top: ny,
                                        height: nlh,
                                        line: PagedLine {
                                            segments: PagedLine::single(nline.items.clone(), nx),
                                            para: npc.para_id.unwrap_or(PARA_SENTINEL),
                                            baseline_y: ny + nline.max_ascent,
                                            para_shading: None,
                                            line_height: nlh,
                                            content_left: ncell_x + ncell.mar_left,
                                            content_right: ncell_x + ncell.width_px - ncell.mar_right,
                                            border_top: None, border_bottom: None,
                                            border_left: None, border_right: None,
                                            bar_tabs: npc.tab_stops.bar_xs(
                                                ncell_x + ncell.mar_left + npc.layout.indent_left_px,
                                                ncell_x + ncell.width_px - ncell.mar_right - npc.layout.indent_right_px,
                                                npc.layout.base_rtl),
                                            is_first_line: false, is_last_line: false,
                                        }.decorated(&npc.layout, nli, nn, nleft, nright),
                                    });
                                    ny += nlh;
                                }
                                ny += npc.layout.border_reserve_bottom();
                            }
                        }
                    }
                    cur += nrow.height_px;
                }
                prev_after = 0.0;
            }
        }
    }
    protos
}

/// A table cell's drawable edge (§17.4): the shared [`Edge`] (colour + pixel
/// width; `dash` unused by WML — cell borders stroke solid).
pub(crate) use crate::engine::border::Edge as CellBorderEdge;

/// Nested-table fix — the capture location for a cell paragraph. Nested
/// tables route to BODY fallback (their inner row/cell coordinates cannot be
/// keyed against the OUTER grid without silently mis-routing/dropping text);
/// top-level cells keep their in-grid `TableCell` location.
fn cell_para_location(
    nested: bool,
    sec: usize,
    block: usize,
    row: usize,
    cell: usize,
) -> crate::wml::select::ParaLocation {
    use crate::wml::select::ParaLocation;
    if nested {
        ParaLocation::Body { sec, block }
    } else {
        ParaLocation::TableCell { sec, block, row, cell }
    }
}

/// Capture a top-level table's STRUCTURE (spans /
/// borders / fill / v-merge) from its `TableCache` into the flavor-neutral
/// `TableMeta` side table. Pure read; `extract_range_impl` populates cell content
/// separately from `ParaMeta`. Rowspan is not stored on the cache, so it is
/// derived: a non-merged cell absorbs consecutive continuation (`v_merged_away`)
/// cells at the SAME grid column (`col_start`) in the rows below. Cells are kept
/// in cache `cell_idx` order so `ParaLocation::TableCell.cell` routes straight in.
pub(crate) fn build_table_meta(cache: &TableCache) -> crate::wml::select::TableMeta {
    use crate::engine::fragment::FragBorder;
    use crate::wml::select::{CellMeta, CellMetaRow, TableMeta};
    // Parasma tracks solid-only cell borders (color + width); style is "single".
    let edge = |e: &Option<CellBorderEdge>| -> Option<FragBorder> {
        e.as_ref().map(|b| FragBorder {
            style: "single".to_string(),
            color: b.color.clone(),
            width_px: b.width_px,
        })
    };
    let num_rows = cache.rows.len();
    let rows = cache
        .rows
        .iter()
        .enumerate()
        .map(|(r, row)| {
            let cells = row
                .cells
                .iter()
                .map(|c| {
                    let rowspan = if c.v_merged_away {
                        1
                    } else {
                        let mut span = 1u32;
                        for r2 in (r + 1)..num_rows {
                            let cont = cache.rows[r2]
                                .cells
                                .iter()
                                .any(|c2| c2.col_start == c.col_start && c2.v_merged_away);
                            if cont {
                                span += 1;
                            } else {
                                break;
                            }
                        }
                        span
                    };
                    CellMeta {
                        rowspan,
                        colspan: c.col_span as u32,
                        fill: c.fill_color.clone(),
                        borders: [
                            edge(&c.border_top),
                            edge(&c.border_right),
                            edge(&c.border_bottom),
                            edge(&c.border_left),
                        ],
                        v_merged_away: c.v_merged_away,
                    }
                })
                .collect();
            CellMetaRow { cells }
        })
        .collect();
    TableMeta { rows }
}

/// Resolve a cell's WRAPPING anchored floats and re-pack its paragraph text
/// around them, all in CELL-LOCAL coordinates (origin = cell content top-left;
/// the in-cell float-wrap path). Returns `None` (the zero-cost fast path) unless the
/// cell hosts ≥1 anchored object AND has no nested table (nested-cell floats are
/// a tracked follow-on). An anchor that does not wrap text (`wrapNone`, in front
/// of or behind the text) is drawn at its position but excludes no text and does
/// not grow the cell, as Word lays it over the content. Mirrors the body float seam — resolve_anchor_position →
/// build_active_float → clear_below_floats → pack_lines_around_floats — walked
/// in document order, which resolves the cell-internal anchor/text circularity
/// the same single-pass way the body does.
#[allow(clippy::too_many_arguments)]
fn build_cell_float_layout(
    blocks: &[CellBlock],
    cell_width: f64,
    mar_left: f64,
    mar_right: f64,
    font_cache: &FontCache,
    style_ctx: &StyleContext,
    default_ppr: Option<&ooxml_wml::types::CTPPrGeneral>,
    default_tab_stop_px: f64,
    theme_fonts: &ThemeFonts,
    numbering_levels: &HashMap<(i64, i64), NumberingLevelDef>,
) -> Option<CellFloatLayout> {
    // GATE: a nested table in the cell defers to the float-blind path; need ≥1
    // anchored object. An empty-Vec scan — zero cost for float-free cells.
    let mut has_anchor = false;
    for block in blocks {
        match block {
            CellBlock::Table(_) => return None,
            CellBlock::Paragraph(pc) => {
                if !pc.anchored_images.is_empty() || !pc.anchored_text_boxes.is_empty() {
                    has_anchor = true;
                }
            }
        }
    }
    if !has_anchor {
        return None;
    }

    let dom_left = mar_left;
    let dom_right = (cell_width - mar_right).max(dom_left);
    // Finite frame for ALIGN-positioned page/margin-relative anchors (a tracked
    // simplification — the common in-cell anchor is column/paragraph-relative
    // with an offset, where these extents are unused). Avoids INF/NaN.
    let frame_extent = (dom_right - dom_left).max(1.0);

    let mut floats: Vec<ActiveFloat> = Vec::new();
    let mut draws: Vec<CellFloatDraw> = Vec::new();
    let mut lines: Vec<PagedLine> = Vec::new();
    let mut cursor = 0.0_f64; // cell-local y, origin at cell content top
    let mut prev_after = 0.0_f64;
    let mut prev_para: Option<&ParagraphLayout> = None;
    // The lowest bottom of a float that wraps text: it grows the cell.
    let mut float_bottom = 0.0_f64;

    for block in blocks {
        let CellBlock::Paragraph(pc) = block else { continue }; // gate excluded Tables
        cursor += paragraph_gap(Some(gap_prev(prev_para, prev_after)), &pc.layout);
        let para_top = cursor;
        let fctx = FrameCtx {
            col_x_offset: dom_left,
            col_width: dom_right - dom_left,
            page_w: frame_extent,
            page_height: frame_extent,
            eff_margin_top: 0.0,
            y_in_page: para_top,
            content_height: frame_extent,
        };

        // (1) Resolve this paragraph's anchors at the paragraph top: record
        //     each one's draw (cell-local), and the exclusion and cell growth
        //     of each that wraps text.
        for ai in &pc.anchored_images {
            let (fx, fy) = resolve_anchor_position(
                &ai.pos_h, &ai.pos_v, ai.h_relative_from, ai.v_relative_from,
                ai.width_px, ai.height_px, &fctx,
            );
            if !matches!(ai.wrap.mode, WrapMode::None) {
                if let Some(af) = build_active_float(&ai.wrap, fx, fy, ai.width_px, ai.height_px, dom_left, dom_right) {
                    floats.push(af);
                }
                float_bottom = float_bottom.max(fy + ai.height_px);
            }
            draws.push(CellFloatDraw::Image {
                rel_id: ai.rel_id.clone(), x: fx, y: fy,
                width_px: ai.width_px, height_px: ai.height_px, behind_text: ai.behind_text,
                anchor_para: pc.para_id,
            });
        }
        for tb in &pc.anchored_text_boxes {
            let (tx, ty) = resolve_anchor_position(
                &tb.pos_h, &tb.pos_v, tb.h_relative_from, tb.v_relative_from,
                tb.width_px, tb.height_px, &fctx,
            );
            if !matches!(tb.wrap.mode, WrapMode::None) {
                if let Some(af) = build_active_float(&tb.wrap, tx, ty, tb.width_px, tb.height_px, dom_left, dom_right) {
                    floats.push(af);
                }
                float_bottom = float_bottom.max(ty + tb.height_px);
            }
            let tb_lines = layout_text_box(
                &tb.raw, tb.width_px, tb.height_px, &tb.frame, font_cache, style_ctx, default_ppr,
                default_tab_stop_px, theme_fonts, numbering_levels,
            );
            draws.push(CellFloatDraw::TextBox {
                lines: tb_lines, x: tx, y: ty,
                width_px: tb.width_px, height_px: tb.height_px, behind_text: tb.behind_text,
                frame: tb.frame.clone(),
            });
        }

        // (2) Re-pack this paragraph's items around ALL floats resolved so far.
        // The TEXT band honors the paragraph's L/R indent (the float-blind path
        // applies it via x_start — paragraph.rs); the float EXCLUSION frame
        // (build_active_float above) stays the full cell content box, matching
        // the body (floats anchor to the column, text indents within).
        let text_left = dom_left + pc.layout.indent_left_px;
        let text_right = (dom_right - pc.layout.indent_right_px).max(text_left);
        // LOGICAL order, not the visual order `pc.lines` hold — see
        // `logical_para_items` / `ParagraphCache::logical_items`.
        let items: Vec<Item> = match &pc.logical_items {
            Some(it) => it.clone(),
            None => pc.lines.iter().flat_map(|l| l.items.clone()).collect(),
        };
        let nominal_lh = pc.lines.first()
            .map(|l| pc.layout.line_height(l.max_font_pt, l.max_ascent + l.max_descent, l.max_image_height))
            .unwrap_or_else(|| pc.layout.line_height(DEFAULT_FONT_PT, 0.0, 0.0));
        // Drop the paragraph below a FULL-WIDTH cell float (no page-fill
        // semantics inside a cell → page_has_content=false so the off-page guard
        // never fires; the cell simply grows to contain it).
        let first_word_w = items.iter()
            .find(|it| !it.is_only_whitespace())
            .map(|it| it.content_width)
            .unwrap_or(WRAP_EPS * 2.0);
        clear_below_floats(
            &floats, &mut cursor, 0.0, nominal_lh, text_left, text_right,
            first_word_w, frame_extent, false,
        );
        let y_first_top = cursor;
        let wrapped = pack_lines_around_floats(
            items, &floats, text_left, text_right, pc.layout.indent_first_line_px,
            nominal_lh, y_first_top, pc.layout.alignment, font_cache, &pc.tab_stops,
        );
        for mut wl in wrapped {
            // UAX#9 L2 at placement — the packer keeps logical order so its
            // remainder can be re-packed; see `pack_lines_around_floats`. A cell
            // places every line it produces, so there is no remainder here.
            if pc.layout.base_rtl {
                for seg in &mut wl.segments {
                    crate::wml::bidi::bidi_reorder_line(&mut seg.items, pc.layout.base_rtl);
                }
            }
            let lh = pc.layout.line_height(wl.max_font_pt, wl.max_ascent + wl.max_descent, wl.max_image_height);
            lines.push(PagedLine {
                segments: wl.segments,
                para: pc.para_id.unwrap_or(PARA_SENTINEL),
                baseline_y: cursor + wl.max_ascent,
                para_shading: None,
                line_height: lh,
                content_left: text_left,
                content_right: text_right,
                border_top: None, border_bottom: None, border_left: None, border_right: None,
                // Cell-local x here — the emit site translates the whole line
                // (segments/content edges/bar_tabs) by the cell origin.
                bar_tabs: pc.tab_stops.bar_xs(text_left, text_right, pc.layout.base_rtl),
                is_first_line: false, is_last_line: false,
            });
            cursor += lh;
        }
        prev_after = pc.layout.space_after_px;
        prev_para = Some(&pc.layout);
    }

    // Float-aware cell content height (pre v-margin): the lower of text bottom
    // (incl. the last paragraph's after-spacing, matching content_height) and
    // the bottom of any float that wraps text must fit, so take the max.
    let text_bottom = cursor + prev_after;
    Some(CellFloatLayout { lines, draws, content_height: text_bottom.max(float_bottom) })
}

/// How many grid columns a cell occupies, from `<w:gridSpan w:val="…">`
/// (§17.4.17) and the number of grid columns still available at the cell's
/// start position.
///
/// **The spec does not bound this value.** `@w:val` is `ST_DecimalNumber`
/// (§17.18.10): *"This simple type specifies that its contents contain a whole
/// decimal number (positive or negative) … a restriction of the W3C XML Schema
/// integer datatype"* — signed and, being `xsd:integer`, arbitrary-precision.
/// So no numeric clamp is grounded in ECMA-376 and none is applied here. Two
/// document-supplied shapes still cannot be laid out as written, and each gets
/// the policy the clause itself supplies:
///
/// * **Non-positive (`0`, `-1`)** — schema-valid, but §17.4.17 gives it no
///   meaning as a span count. The clause states its own fallback for a cell
///   that declares nothing: *"If this element is omitted, then the number of
///   grid units spanned by this cell shall be assumed to be one."* A value the
///   spec cannot interpret is therefore treated exactly as an absent element —
///   one column. (`CT_DecimalNumber/@val` is `use="required"`, so the grounding
///   is the prose, not an attribute default.) Note this is NOT a saturating
///   clamp of `-1` to `1`: it is the documented omitted-element rule.
///   Before this, `gs.value as usize` turned `-1` into `usize::MAX` and the
///   caller's `col_idx + col_span` panicked on merely OPENING the document.
///
/// * **Larger than the grid** — this one is LEGAL and must not be rewritten:
///   *"If the number of grid units specified by the val attribute exceeds the
///   size of the table grid, then the table grid shall be augmented as needed
///   to create the number of grid columns required."* We do not augment the
///   grid (a standing floor: the augmented columns have no `CT_TblGridCol/@w`,
///   so they contribute no geometry), and we do not touch `w:val`. Instead the
///   DERIVED work is bounded by geometry: the cell spans the grid columns that
///   actually exist from where it starts. That is already what the width sum
///   does — the slice saturates at the grid width — so this only makes the
///   recorded `col_span` agree with the width, and it means the requested span
///   never reaches arithmetic that can overflow. The bound is applied to the
///   SIGNED value before any cast, because `i64 as usize` truncates to 32 bits
///   on wasm32: `w:val="4294967295"` would otherwise wrap `1 + span` to 0 and
///   invert the slice range.
fn grid_span_columns(val: Option<i64>, remaining_grid_columns: usize) -> usize {
    // §17.4.17 omitted-element rule, reused for values the spec gives no meaning.
    let requested = match val {
        Some(v) if v >= 1 => v,
        _ => return 1,
    };
    // Bound by the real grid. `try_from` (not `as`) so a value wider than
    // `usize` — schema-valid, and only 32 bits wide on wasm32 — saturates
    // upward into the ceiling instead of wrapping to something small.
    let ceiling = remaining_grid_columns.max(1);
    usize::try_from(requested).unwrap_or(usize::MAX).min(ceiling)
}

const PX_PER_TWIP: f64 = 96.0 / 1440.0;

/// A table measurement (§17.4.87 CT_TblWidth), read by its lexical form
/// first and its type second (§17.18.90).
enum TblWidth {
    /// A length, in px.
    Px(f64),
    /// A share of the extent the property is measured against.
    Fraction(f64),
    Auto,
    /// Zero, whatever the width says.
    Nil,
}

/// Read a table measurement. A value with a percent sign is a percentage
/// and a value with a unit is that length, whatever the type says; a bare
/// number is fiftieths of a percent for `pct` and twentieths of a point for
/// `dxa` or no type. A missing width is 0; an unreadable one is `Auto`.
fn tbl_width(w: &ooxml_wml::types::CTTblWidth) -> TblWidth {
    const TWIPS_PER_UNIT: [(&str, f64); 6] =
        [("mm", 1440.0 / 25.4), ("cm", 1440.0 / 2.54), ("in", 1440.0), ("pt", 20.0), ("pc", 240.0), ("pi", 240.0)];
    if matches!(w.r#type, Some(STTblWidth::Nil)) {
        return TblWidth::Nil;
    }
    let raw = w.width.as_deref().map_or("0", str::trim);
    let number = |s: &str| s.parse::<f64>().ok().filter(|n| n.is_finite());
    if let Some(pct) = raw.strip_suffix('%') {
        return number(pct).map_or(TblWidth::Auto, |p| TblWidth::Fraction(p / 100.0));
    }
    for (unit, twips) in TWIPS_PER_UNIT {
        if let Some(n) = raw.strip_suffix(unit) {
            return number(n).map_or(TblWidth::Auto, |n| TblWidth::Px(n * twips * PX_PER_TWIP));
        }
    }
    match (number(raw), w.r#type) {
        (Some(n), Some(STTblWidth::Pct)) => TblWidth::Fraction(n / 5000.0),
        (Some(n), Some(STTblWidth::Dxa) | None) => TblWidth::Px(n * PX_PER_TWIP),
        _ => TblWidth::Auto,
    }
}

/// A cell's four margins, in px.
#[derive(Clone, Copy)]
struct CellMargins {
    top: f64,
    bottom: f64,
    left: f64,
    right: f64,
}

impl CellMargins {
    /// With no margin anywhere in the style hierarchy. DEVIATION, declared:
    /// §17.4.34 and §17.4.11 give 115 twentieths for the sides; Word uses 108.
    const DEFAULT: Self = Self {
        top: DEFAULT_CELL_MARGIN_V_TWIPS as f64 * PX_PER_TWIP,
        bottom: DEFAULT_CELL_MARGIN_V_TWIPS as f64 * PX_PER_TWIP,
        left: DEFAULT_CELL_MARGIN_H_TWIPS as f64 * PX_PER_TWIP,
        right: DEFAULT_CELL_MARGIN_H_TWIPS as f64 * PX_PER_TWIP,
    };

    /// Take the edges a tblCellMar or tcMar names. A `pct` or `auto` margin
    /// is ignored (§17.4.5, the same for every edge) and `nil` is zero.
    /// Declared: start and end are left and right, also in a right-to-left
    /// table.
    fn overlay(
        &mut self,
        top: Option<&ooxml_wml::types::CTTblWidth>,
        bottom: Option<&ooxml_wml::types::CTTblWidth>,
        start: Option<&ooxml_wml::types::CTTblWidth>,
        end: Option<&ooxml_wml::types::CTTblWidth>,
    ) {
        for (edge, w) in [(&mut self.top, top), (&mut self.bottom, bottom), (&mut self.left, start), (&mut self.right, end)] {
            match w.map(tbl_width) {
                Some(TblWidth::Px(px)) => *edge = px.max(0.0),
                Some(TblWidth::Nil) => *edge = 0.0,
                Some(TblWidth::Fraction(_) | TblWidth::Auto) | None => {}
            }
        }
    }
}

/// Lay out ONE table into a [`TableCache`]: resolved column widths, per-row
/// cell geometry (x / width / margins / spans), each cell's laid-out block
/// content, and the resolved borders/fills — everything the page assembler
/// needs to place rows (with cross-page row splitting handled by the caller).
/// The table-level orchestrator: resolves the grid + `tblStyle` conditional
/// formatting, then lays out each cell's paragraphs/nested tables. Governing
/// spec: §17.4 tables (grid §17.4.48, cell margins §17.4.42, borders §17.4.66).
// Positional orchestrator args; struct-ification deferred to a dedicated pass.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_table(
    font_cache: &FontCache,
    table: &ooxml_wml::types::Table,
    content_width: f64,
    style_ctx: &StyleContext,
    default_ppr: Option<&ooxml_wml::types::CTPPrGeneral>,
    default_tab_stop_px: f64,
    theme_fonts: &ThemeFonts,
    numbering_levels: &HashMap<(i64, i64), NumberingLevelDef>,
    list_counters: &mut ListCounters,
    // Selection capture — `Some` on the body cache-build walk (threaded from
    // `run_layout_impl`); cell paragraphs are captured with a `TableCell`
    // location. `None` on the hdr/ftr build. The incoming location's `sec`/
    // `block` are the node-path prefix; per-cell `row`/`cell` are stamped below.
    mut capture: Option<&mut crate::wml::select::ParaCapture>,
) -> Result<TableCache, JsValue> {
    use crate::wml::select::ParaLocation;
    // Extract the sec/block prefix the caller seeded (Body or TableCell both
    // carry it); cells rewrite `row`/`cell` onto this prefix.
    //
    // v1 LIMITATION (nested tables, selection capture): only the `sec`/`block` prefix
    // survives the descent — a nested table's cells would overwrite `row`/`cell`
    // with their OWN inner coordinates, so routing them into the OUTER grid lands
    // in the wrong cell or drops the text (silent copy data loss). extractRange
    // DOES consume `location` to route cell paragraphs, so this is NOT harmless.
    // Guard: while `capture.nested` is set (below, across the `Tbl` recursion),
    // nested-cell paragraphs are captured as BODY fallback so their text still
    // copies (as body text, not in-grid). A full node-path lands with the
    // full node-path addressing.
    let (cap_sec, cap_block) = match capture.as_deref() {
        Some(cap) => match cap.location {
            ParaLocation::Body { sec, block }
            | ParaLocation::TableCell { sec, block, .. } => (sec, block),
        },
        None => (0, 0),
    };
    let props = table.properties();
    let style = TableStyle::of(props, style_ctx);

    // §17.4.42: each edge from the table's own tblCellMar, else from its
    // style hierarchy's, else the default.
    let mut table_mar = CellMargins::DEFAULT;
    for m in style.tbl_prs().filter_map(|tp| tp.tbl_cell_mar.as_deref()).chain(props.tbl_cell_mar.as_deref()) {
        table_mar.overlay(m.top.as_deref(), m.bottom.as_deref(), m.start.as_deref().or(m.left.as_deref()), m.end.as_deref().or(m.right.as_deref()));
    }

    let indent_px = props.tbl_ind.as_ref()
        .and_then(|ind| ind.width.as_deref())
        .and_then(|w| w.parse::<i64>().ok())
        .map(twips_to_px)
        .unwrap_or(0.0);

    let col_count = table.tbl_grid.grid_col.len();
    let col_widths_px: Vec<f64> = if col_count == 0 {
        vec![content_width]
    } else {
        let widths: Vec<f64> = table.tbl_grid.grid_col.iter()
            .map(|gc| {
                gc.width.as_deref()
                    .and_then(|w| w.parse::<i64>().ok())
                    .map(twips_to_px)
                    .unwrap_or(content_width / col_count as f64)
            })
            .collect();
        let total: f64 = widths.iter().sum();

        // `tblW` is a *preferred* table width. A value of 0 — or type Auto/Nil —
        // means "no preferred width": the table sizes from its grid columns
        // (auto-fit), NOT a literal zero-width table. Only a positive share or length
        // value is an explicit target; otherwise fall through to grid-based
        // sizing below (down-scaling the grid if it overflows the content box).
        let target_width = props.tbl_w.as_deref().and_then(|tw| match tbl_width(tw) {
            TblWidth::Fraction(f) if f > 0.0 => Some(f * content_width),
            TblWidth::Px(px) if px > 0.0 => Some(px),
            _ => None,
        });

        if let Some(target) = target_width {
            let scale = target / total;
            widths.iter().map(|w| w * scale).collect()
        } else if total > content_width {
            let scale = content_width / total;
            widths.iter().map(|w| w * scale).collect()
        } else {
            widths
        }
    };

    let (border_color, has_borders) = extract_table_border_info(props);
    let table_border_edges = extract_table_border_edges(theme_fonts, props, &style);
    let alignment = props.justification.as_ref()
        .map(|j| j.value)
        .unwrap_or(STJcTable::Start);

    let table_rows = table_rows(table);
    let num_rows = table_rows.len();
    let regions = TableRegions::of(TableLook::read(props.tbl_look.as_deref()), &style, props, &table_rows);

    let mut rows = Vec::new();
    // Which of each cell's edges its own tcBorders set: a cell border beats
    // a table border in a conflict (§17.4.66).
    let mut direct_edges: Vec<Vec<[bool; 4]>> = Vec::new();
    for (row_idx, row) in table_rows.iter().enumerate() {
        let mut cells = Vec::new();
        let mut row_direct = Vec::new();
        // §17.4.15 gridBefore: the grid columns the row leaves empty before
        // its first cell. A count that leaves no column for the cells is
        // ignored. The columns a row leaves empty after its last cell
        // (§17.4.14 gridAfter) need no reading: the cells simply end early.
        // wBefore and wAfter are not read: the grid already sizes the gap.
        let mut col_idx: usize = row.row_properties.as_ref()
            .and_then(|p| p.grid_before.as_ref())
            .and_then(|g| usize::try_from(g.value).ok())
            .filter(|skipped| *skipped < col_widths_px.len())
            .unwrap_or(0);
        let row_cells = collect_row_cells(row);
        let num_cols = row_cells.len();

        // §17.4.80: an hRule="exact" row is a fixed-height box — paragraph spacing
        // must not push cell content out of it (see paragraph.rs). Known up-front
        // from the row props, before the cells lay out.
        let is_exact_row = row.row_properties.as_ref()
            .and_then(|trpr| trpr.tr_height.as_ref())
            .map(|trh| matches!(trh.h_rule, Some(ooxml_wml::types::STHeightRule::Exact)))
            .unwrap_or(false);

        for (cell_idx, cell) in row_cells.iter().enumerate() {
            let at = CellPlace { row_idx, col_idx: cell_idx, num_rows, num_cols };
            // §17.4.17 gridSpan. `col_widths_px.len()` is the real grid; `col_idx`
            // never passes it (it is only ever assigned `end`, which is `.min(len)`).
            let col_span = grid_span_columns(
                cell.properties().and_then(|p| p.grid_span.as_ref()).map(|gs| gs.value),
                col_widths_px.len() - col_idx,
            );

            let v_merge = cell.properties()
                .and_then(|p| p.vertical_merge.as_ref());
            let v_merged_away = v_merge.is_some()
                && !matches!(v_merge.and_then(|vm| vm.value.as_ref()), Some(ooxml_wml::types::STMerge::Restart));

            // `grid_span_columns` already bounded the span by the columns left in
            // the grid, so this add cannot overflow; `saturating_add` keeps that
            // true no matter what a future caller passes.
            let end = col_idx.saturating_add(col_span).min(col_widths_px.len());
            let cell_width: f64 = col_widths_px[col_idx..end].iter().sum();
            // §17.4.68: a cell's tcMar overrides the table's margins edge by edge.
            let mut mar = table_mar;
            if let Some(m) = cell.properties().and_then(|p| p.tc_mar.as_deref()) {
                mar.overlay(m.top.as_deref(), m.bottom.as_deref(), m.start.as_deref().or(m.left.as_deref()), m.end.as_deref().or(m.right.as_deref()));
            }
            let cell_content_width = (cell_width - mar.left - mar.right).max(0.0);

            let region_props = style.region_props(&regions.at(at));
            // Run and paragraph properties from the style, nearest first:
            // the cell's regions, then the style itself.
            let style_rprs: Vec<&ooxml_wml::types::RunProperties> = region_props.iter()
                .filter_map(|(o, _)| o.r_pr.as_deref())
                .chain(style.chain.iter().filter_map(|s| s.r_pr.as_deref()))
                .collect();
            let style_pprs: Vec<&ooxml_wml::types::CTPPrGeneral> = region_props.iter()
                .filter_map(|(o, _)| o.p_pr.as_deref())
                .chain(style.chain.iter().filter_map(|s| s.p_pr.as_deref()))
                .collect();

            let mut cell_blocks: Vec<CellBlock> = Vec::new();
            if !v_merged_away {
                // Content controls and custom XML around a cell's blocks are
                // unwrapped at any depth, tables included.
                let mut blocks = Vec::new();
                flatten_blocks(&cell.block_content, &mut blocks);
                for block in blocks {
                    match block {
                        FlatBlock::Para(p) => {
                            let marker = resolve_numbering_marker(p, numbering_levels, list_counters, style_ctx);
                            if let Some(cap) = capture.as_deref_mut() {
                                cap.location = cell_para_location(cap.nested, cap_sec, cap_block, row_idx, cell_idx);
                            }
                            let cache = layout_paragraph(font_cache, p, cell_content_width, marker.as_ref(), style_ctx, default_ppr, default_tab_stop_px, theme_fonts, &style_rprs, &style_pprs, None, is_exact_row, capture.as_deref_mut())?;
                            cell_blocks.push(CellBlock::Paragraph(cache));
                        }
                        FlatBlock::Table(nested) => {
                            // Nested-table fix — descend the nested table with `nested` set so its
                            // cell paragraphs capture as BODY fallback (see the v1
                            // LIMITATION note above). Restore the prior flag for the
                            // outer cell's remaining sibling blocks.
                            let prev_nested = capture.as_deref().map(|c| c.nested).unwrap_or(false);
                            if let Some(cap) = capture.as_deref_mut() { cap.nested = true; }
                            let nested_cache = layout_table(font_cache, nested, cell_content_width, style_ctx, default_ppr, default_tab_stop_px, theme_fonts, numbering_levels, list_counters, capture.as_deref_mut())?;
                            if let Some(cap) = capture.as_deref_mut() { cap.nested = prev_nested; }
                            cell_blocks.push(CellBlock::Table(nested_cache));
                        }
                    }
                }
            }

            // Identically bordered paragraphs in the cell share one box.
            crate::wml::paragraph::join_border_boxes(cell_blocks.iter_mut().map(|b| match b {
                CellBlock::Paragraph(pc) => Some(&mut pc.layout),
                CellBlock::Table(_) => None,
            }).collect());

            // A cell's own shading decides, including one that clears.
            let direct_fill = cell.properties()
                .and_then(|p| p.shading.as_ref())
                .map(|s| theme_fonts.wml_shading(s));

            let fill_color = direct_fill.unwrap_or_else(|| {
                region_props.iter()
                    .find_map(|(o, _)| o.cell_properties.as_ref()?.shading.as_ref())
                    .and_then(|shd| theme_fonts.wml_shading(shd))
            });

            let v_align = cell.properties()
                .and_then(|p| p.v_align.as_ref())
                .or_else(|| region_props.iter().find_map(|(o, _)| o.cell_properties.as_ref()?.v_align.as_ref()))
                .map_or(CellVAlign::Top, |va| match va.value {
                    ooxml_wml::types::STVerticalJc::Center => CellVAlign::Center,
                    ooxml_wml::types::STVerticalJc::Bottom => CellVAlign::Bottom,
                    _ => CellVAlign::Top,
                });

            let cond_borders = resolve_conditional_cell_borders(theme_fonts, at, &region_props);
            let (cell_bdr_top, cell_bdr_bottom, cell_bdr_left, cell_bdr_right) =
                extract_cell_borders(theme_fonts, cell, &table_border_edges, &cond_borders, row_idx, cell_idx, num_rows, num_cols);
            row_direct.push(direct_cell_edges(cell));

            // Pre-resolve a cell-hosted wrapping float (None = fast path). Built
            // before `cell_blocks` is moved into the cache.
            let float_layout = if v_merged_away {
                None
            } else {
                build_cell_float_layout(
                    &cell_blocks, cell_width, mar.left, mar.right,
                    font_cache, style_ctx, default_ppr, default_tab_stop_px,
                    theme_fonts, numbering_levels,
                )
            };
            cells.push(TableCellCache {
                blocks: cell_blocks,
                col_start: col_idx,
                col_span,
                x_px: col_widths_px[..col_idx].iter().sum(),
                width_px: cell_width,
                fill_color: if v_merged_away { None } else { fill_color },
                v_align,
                v_merged_away,
                border_top: cell_bdr_top,
                border_bottom: cell_bdr_bottom,
                border_left: cell_bdr_left,
                border_right: cell_bdr_right,
                diagonals: if v_merged_away { None } else { cell_diagonals(theme_fonts, cell) },
                mar_top: mar.top,
                mar_bottom: mar.bottom,
                mar_left: mar.left,
                mar_right: mar.right,
                float_layout,
            });
            col_idx = end;
        }

        let content_height = cells.iter()
            .map(|c| c.content_height(c.mar_top + c.mar_bottom))
            .fold(0.0_f64, f64::max);

        let row_height = if let Some(ref trpr) = row.row_properties {
            if let Some(ref trh) = trpr.tr_height {
                let specified_px = trh.value.as_deref()
                    .and_then(|v| v.parse::<i64>().ok())
                    .map(twips_to_px)
                    .unwrap_or(0.0);
                match trh.h_rule {
                    Some(ooxml_wml::types::STHeightRule::Exact) => specified_px,
                    Some(ooxml_wml::types::STHeightRule::AtLeast) | None => content_height.max(specified_px),
                    Some(ooxml_wml::types::STHeightRule::Auto) => content_height,
                }
            } else {
                content_height
            }
        } else {
            content_height
        };

        // §17.4.6 cantSplit.
        let cant_split = row.row_properties.as_ref()
            .and_then(|p| crate::check_toggle(&p.cant_split))
            .unwrap_or(false);

        rows.push(TableRowCache { cells, height_px: row_height, cant_split });
        direct_edges.push(row_direct);
    }
    resolve_border_conflicts(&mut rows, &direct_edges);

    Ok(TableCache { col_widths_px, rows, border_color, has_borders, alignment, indent_px })
}

/// One tblStylePr that formats a cell, with the cell's sides (top, bottom,
/// left, right) that lie on its region's outline.
type RegionProps<'a> = (&'a ooxml_wml::types::TableStyleProperties, [bool; 4]);

/// A table's style and the styles it is based on, nearest first (§17.7.6).
/// A table that names no style uses the default table style (§17.7.4.17).
struct TableStyle<'a> {
    chain: Vec<&'a ooxml_wml::types::Style>,
}

impl<'a> TableStyle<'a> {
    fn of(props: &ooxml_wml::types::TableProperties, ctx: &'a StyleContext) -> Self {
        let id = props.tbl_style.as_ref().map_or(DEFAULT_TABLE_STYLE_KEY, |s| s.value.as_str());
        Self { chain: style_chain(ctx, id) }
    }

    /// The hierarchy's table-wide tblPr, base first, so a later one overrides.
    fn tbl_prs(&self) -> impl Iterator<Item = &'a ooxml_wml::types::CTTblPrBase> + '_ {
        self.chain.iter().rev().filter_map(|s| s.table_properties.as_deref())
    }

    /// The tblStylePr that format a cell in `regions` (least specific first,
    /// as `TableRegions::at` gives them), most specific first: a more
    /// specific region before a less specific one, and within a region the
    /// nearest style before the ones it is based on. Each comes with the
    /// cell's sides on its region's outline.
    fn region_props(&self, regions: &[(ooxml_wml::types::STTblStyleOverrideType, [bool; 4])]) -> Vec<RegionProps<'a>> {
        let mut out = Vec::new();
        for (region, outline) in regions.iter().rev() {
            for style in &self.chain {
                out.extend(style.tbl_style_pr.iter().filter(|o| o.r#type == *region).map(|o| (o, *outline)));
            }
        }
        out
    }
}

/// §17.4.55 tblLook: which of its style's conditional regions a table uses.
#[derive(Clone, Copy)]
struct TableLook {
    first_row: bool,
    last_row: bool,
    first_col: bool,
    last_col: bool,
    no_h_band: bool,
    no_v_band: bool,
}

impl TableLook {
    /// Each flag from its named attribute, else from its bit in the hex
    /// `w:val`: the only form Word 2007 writes ([MS-OI29500] §17.4.55:
    /// 0x0020 first row, 0x0040 last row, 0x0080 first column, 0x0100 last
    /// column, 0x0200 no row bands, 0x0400 no column bands).
    /// DEVIATION, declared: §17.4.55 reads a missing tblLook as all zero;
    /// Word reads it as 0x04A0 (a header row and a first column, no column
    /// bands), measured in Word.
    fn read(look: Option<&ooxml_wml::types::CTTblLook>) -> Self {
        // The parser hands the hex digits over as bytes, most significant first.
        let bits = match look {
            Some(l) => l.value.as_deref().unwrap_or(&[]).iter().fold(0u16, |acc, b| acc << 8 | u16::from(*b)),
            None => 0x04A0,
        };
        let flag = |attr: fn(&ooxml_wml::types::CTTblLook) -> &Option<String>, bit: u16| {
            match look.and_then(|l| attr(l).as_deref()) {
                Some(v) => crate::on_off(v),
                None => bits & bit != 0,
            }
        };
        Self {
            first_row: flag(|l| &l.first_row, 0x0020),
            last_row: flag(|l| &l.last_row, 0x0040),
            first_col: flag(|l| &l.first_column, 0x0080),
            last_col: flag(|l| &l.last_column, 0x0100),
            no_h_band: flag(|l| &l.no_h_band, 0x0200),
            no_v_band: flag(|l| &l.no_v_band, 0x0400),
        }
    }
}

/// A cell's place in its table, by row and by cell within the row.
#[derive(Clone, Copy)]
struct CellPlace {
    row_idx: usize,
    col_idx: usize,
    num_rows: usize,
    num_cols: usize,
}

/// One axis of a table's banding: the band size, and how many leading and
/// trailing rows (or columns) stand outside the bands.
#[derive(Clone, Copy)]
struct BandAxis {
    /// Rows or columns per band; 0 when the axis is not banded.
    size: usize,
    skip_start: usize,
    skip_end: usize,
}

impl BandAxis {
    /// The band `idx` falls in, and its place in that band: `None` outside
    /// the bands. `Some((band, first, last))`.
    fn band(&self, idx: usize, count: usize) -> Option<(usize, bool, bool)> {
        let end = count.saturating_sub(self.skip_end);
        if self.size == 0 || idx < self.skip_start || idx >= end {
            return None;
        }
        let k = idx - self.skip_start;
        Some((k / self.size, k.is_multiple_of(self.size), (k + 1).is_multiple_of(self.size) || idx + 1 == end))
    }
}

/// Which `tblStylePr` conditional regions each cell of a table is in, as
/// Word applies them. Measured in Word; DEVIATIONS from §17.7.6, declared:
/// - row regions lie over column regions (the spec has it the other way);
/// - column bands lie over row bands;
/// - a `wholeTable` region formats nothing;
/// - an axis is banded only if the style hierarchy gives it a band size
///   (§17.7.6.7, §17.7.6.5 default the size to 1);
/// - a band skips the header and total rows (or first and last columns)
///   only where the style formats them, and band 1 starts after them;
/// - every leading "repeat as header row" row is a header row.
///
/// Declared: columns are counted by cell, not by grid column, so a
/// spanning cell shifts the column bands after it.
struct TableRegions {
    look: TableLook,
    /// The leading rows the firstRow region formats.
    header_rows: usize,
    rows: BandAxis,
    cols: BandAxis,
}

impl TableRegions {
    fn of(
        look: TableLook,
        style: &TableStyle,
        props: &ooxml_wml::types::TableProperties,
        rows: &[&ooxml_wml::types::CTRow],
    ) -> Self {
        use ooxml_wml::types::STTblStyleOverrideType as R;
        // §17.4.49 tblHeader: the run of header rows starts at the first row.
        let header_rows = rows.iter()
            .take_while(|r| r.row_properties.as_ref().and_then(|p| crate::check_toggle(&p.tbl_header)).unwrap_or(false))
            .count()
            .max(1);
        let formats = |region: R| style.chain.iter().any(|s| s.tbl_style_pr.iter().any(|o| o.r#type == region));
        // The nearest band size: the table's own, else its style hierarchy's.
        let size = |direct: &Option<Box<ooxml_wml::types::CTDecimalNumber>>, of: fn(&ooxml_wml::types::CTTblPrBase) -> &Option<Box<ooxml_wml::types::CTDecimalNumber>>| {
            direct.as_deref()
                .or_else(|| style.chain.iter().find_map(|s| of(s.table_properties.as_deref()?).as_deref()))
                .and_then(|n| usize::try_from(n.value).ok())
                .unwrap_or(0)
        };
        Self {
            look,
            header_rows,
            rows: BandAxis {
                size: if look.no_h_band { 0 } else { size(&props.tbl_style_row_band_size, |tp| &tp.tbl_style_row_band_size) },
                skip_start: if look.first_row && formats(R::FirstRow) { header_rows } else { 0 },
                skip_end: usize::from(look.last_row && formats(R::LastRow)),
            },
            cols: BandAxis {
                size: if look.no_v_band { 0 } else { size(&props.tbl_style_col_band_size, |tp| &tp.tbl_style_col_band_size) },
                skip_start: usize::from(look.first_col && formats(R::FirstCol)),
                skip_end: usize::from(look.last_col && formats(R::LastCol)),
            },
        }
    }

    /// The regions a cell is in, least specific first, each with the sides
    /// of the cell that lie on the region's outline (top, bottom, left,
    /// right); the other sides are edges between the region's cells.
    fn at(&self, at: CellPlace) -> Vec<(ooxml_wml::types::STTblStyleOverrideType, [bool; 4])> {
        use ooxml_wml::types::STTblStyleOverrideType::*;
        let (top, bottom) = (at.row_idx == 0, at.row_idx + 1 >= at.num_rows);
        let (left, right) = (at.col_idx == 0, at.col_idx + 1 >= at.num_cols);
        let header = self.look.first_row && at.row_idx < self.header_rows;
        let total = self.look.last_row && bottom && !header;
        let first_col = self.look.first_col && left;
        let last_col = self.look.last_col && right && !first_col;

        let mut regions = Vec::new();
        if let Some((band, first, last)) = self.rows.band(at.row_idx, at.num_rows) {
            regions.push((if band.is_multiple_of(2) { Band1Horz } else { Band2Horz }, [first, last, left, right]));
        }
        if let Some((band, first, last)) = self.cols.band(at.col_idx, at.num_cols) {
            regions.push((if band.is_multiple_of(2) { Band1Vert } else { Band2Vert }, [top, bottom, first, last]));
        }
        if first_col { regions.push((FirstCol, [top, bottom, true, true])); }
        if last_col { regions.push((LastCol, [top, bottom, true, true])); }
        if header { regions.push((FirstRow, [top, at.row_idx + 1 >= self.header_rows.min(at.num_rows), left, right])); }
        if total { regions.push((LastRow, [true, true, left, right])); }
        for (corner, here) in [(NwCell, header && first_col), (NeCell, header && last_col), (SwCell, total && first_col), (SeCell, total && last_col)] {
            if here { regions.push((corner, [true; 4])); }
        }
        regions
    }
}

/// Conditional border edges for a cell from `tblStylePr` regions.
/// Returns `(top, bottom, left, right)`. For each edge, the most-specific
/// region wins (matching fill/valign). A region's own `tcBorders` provides the
/// cell's outer edges; the region's `tblPr/tblBorders` provides insideH/insideV
/// for interior cells — this is how a firstRow region draws the header
/// separator line under the header row.
/// Three-state per-edge overrides `(top, bottom, left, right)` keyed on element
/// PRESENCE: `None` = edge absent (inherit), `Some(None)` = explicit nil/none
/// (suppress), `Some(Some(edge))` = a drawn border. Produced by `edge_override`
/// / `resolve_conditional_cell_borders`, consumed by `extract_cell_borders`.
type EdgeOverrides = (
    Option<Option<CellBorderEdge>>,
    Option<Option<CellBorderEdge>>,
    Option<Option<CellBorderEdge>>,
    Option<Option<CellBorderEdge>>,
);

fn resolve_conditional_cell_borders(
    theme: &ThemeFonts,
    at: CellPlace,
    region_props: &[RegionProps],
) -> EdgeOverrides {
    let is_top = at.row_idx == 0;
    let is_bottom = at.row_idx + 1 >= at.num_rows;
    let is_left = at.col_idx == 0;
    let is_right = at.col_idx + 1 >= at.num_cols;

    let (mut top, mut bottom, mut left, mut right): EdgeOverrides = (None, None, None, None);
    // Iterate least→most specific so later (more specific) regions overwrite.
    // A region that specifies an edge — drawn OR explicit-nil — records
    // Some(..) so the cell-resolution short-circuits the table default (a
    // firstRow region can thus explicitly REMOVE the header separator).
    for (ov, [on_top, on_bottom, on_left, on_right]) in region_props.iter().rev().copied() {
        // Region cell tcBorders: its top, bottom, left and right draw the
        // region's outline, its insideH and insideV the edges between the
        // region's cells (§17.7.6).
        if let Some(ref tcp) = ov.cell_properties {
            if let Some(ref b) = tcp.tc_borders {
                let start = || edge_override(theme, &b.left).or_else(|| edge_override(theme, &b.start));
                let end = || edge_override(theme, &b.right).or_else(|| edge_override(theme, &b.end));
                let pick = |outline: bool, edge: Option<Option<CellBorderEdge>>, inside: &Option<Box<ooxml_wml::types::CTBorder>>| {
                    if outline { edge } else { edge_override(theme, inside) }
                };
                if let Some(v) = pick(on_top, edge_override(theme, &b.top), &b.inside_h) { top = Some(v); }
                if let Some(v) = pick(on_bottom, edge_override(theme, &b.bottom), &b.inside_h) { bottom = Some(v); }
                if let Some(v) = pick(on_left, start(), &b.inside_v) { left = Some(v); }
                if let Some(v) = pick(on_right, end(), &b.inside_v) { right = Some(v); }
            }
        }
        // Region tblPr tblBorders → outer edges + insideH/V for the region.
        if let Some(ref tp) = ov.table_properties {
            if let Some(ref b) = tp.tbl_borders {
                if is_top { if let Some(v) = edge_override(theme, &b.top) { top = Some(v); } }
                else if let Some(v) = edge_override(theme, &b.inside_h) { top = Some(v); }
                if is_bottom { if let Some(v) = edge_override(theme, &b.bottom) { bottom = Some(v); } }
                else if let Some(v) = edge_override(theme, &b.inside_h) { bottom = Some(v); }
                if is_left { if let Some(v) = edge_override(theme, &b.left).or_else(|| edge_override(theme, &b.start)) { left = Some(v); } }
                else if let Some(v) = edge_override(theme, &b.inside_v) { left = Some(v); }
                if is_right { if let Some(v) = edge_override(theme, &b.right).or_else(|| edge_override(theme, &b.end)) { right = Some(v); } }
                else if let Some(v) = edge_override(theme, &b.inside_v) { right = Some(v); }
            }
        }
    }
    (top, bottom, left, right)
}

/// A table's rows, at any depth inside row-level content controls
/// (§17.5.2.30) and custom XML (§17.5.1.5). The parser's `rows()` returns only
/// direct rows, so rows wrapped in a control were not laid out.
pub(crate) fn table_rows(table: &ooxml_wml::types::Table) -> Vec<&ooxml_wml::types::CTRow> {
    fn collect<'a>(rows: &'a [ooxml_wml::types::RowContent], out: &mut Vec<&'a ooxml_wml::types::CTRow>) {
        use ooxml_wml::types::RowContent as R;
        for row in rows {
            match row {
                R::Tr(tr) => out.push(tr),
                R::Sdt(sdt) => {
                    if let Some(content) = &sdt.sdt_content {
                        collect(&content.rows, out);
                    }
                }
                R::CustomXml(c) => collect(&c.rows, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    collect(&table.rows, &mut out);
    out
}

/// A row's cells, at any depth inside cell-level content controls
/// (§17.5.2.32) and custom XML (§17.5.1.4).
pub(crate) fn collect_row_cells(row: &ooxml_wml::types::CTRow) -> Vec<&ooxml_wml::types::TableCell> {
    fn collect<'a>(cells: &'a [ooxml_wml::types::CellContent], out: &mut Vec<&'a ooxml_wml::types::TableCell>) {
        use ooxml_wml::types::CellContent as C;
        for cell in cells {
            match cell {
                C::Tc(tc) => out.push(tc),
                C::Sdt(sdt) => {
                    if let Some(content) = &sdt.sdt_content {
                        collect(&content.cells, out);
                    }
                }
                C::CustomXml(c) => collect(&c.cells, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    collect(&row.cells, &mut out);
    out
}

fn extract_table_border_info(props: &ooxml_wml::types::TableProperties) -> (String, bool) {
    let borders = props.tbl_borders.as_ref();
    let has_borders = borders.is_none_or(|b| {
        let not_none = |border: &Option<Box<ooxml_wml::types::CTBorder>>| {
            border.as_ref().is_some_and(|b| !matches!(b.value, STBorder::None | STBorder::Nil))
        };
        not_none(&b.top) || not_none(&b.bottom)
            || not_none(&b.left) || not_none(&b.right)
            || not_none(&b.inside_h) || not_none(&b.inside_v)
    });
    let color = borders
        .and_then(|b| b.top.as_ref())
        .and_then(|b| b.color.as_deref())
        .and_then(ooxml_color_to_css)
        .unwrap_or_else(|| "#000000".to_string());
    (color, has_borders)
}

struct TableBorderEdges {
    top: Option<CellBorderEdge>,
    bottom: Option<CellBorderEdge>,
    left: Option<CellBorderEdge>,
    right: Option<CellBorderEdge>,
    inside_h: Option<CellBorderEdge>,
    inside_v: Option<CellBorderEdge>,
}

// `Some(edge)` for a real border, `None` for an explicit nil/none. Operates
// on a PRESENT element — the caller decides presence.
fn border_value(theme: &ThemeFonts, b: &ooxml_wml::types::CTBorder) -> Option<CellBorderEdge> {
    crate::wml::border::border_element_line(b, theme)
}

/// Which of a cell's edges (top, bottom, left, right) its own tcBorders set.
fn direct_cell_edges(cell: &ooxml_wml::types::TableCell) -> [bool; 4] {
    let Some(b) = cell.properties().and_then(|p| p.tc_borders.as_ref()) else { return [false; 4] };
    [
        b.top.is_some(),
        b.bottom.is_some(),
        b.left.is_some() || b.start.is_some(),
        b.right.is_some() || b.end.is_some(),
    ]
}

/// The border that shows where two cells' borders meet, as Office resolves
/// it ([MS-OI29500] §17.4.66; cell spacing is not modelled, so every shared
/// edge conflicts): a missing border yields to the other; a cell's own
/// border beats a table border; then the heavier border (width in eighths
/// of a point times its style's border number; dotted and dashed weigh 1);
/// then the style earlier on the precedence list; then the darker colour;
/// then the first in reading order (`a`). DEVIATION, declared: `nil` and
/// `none` both yield to the other border (Office shows nothing for `nil`).
fn stronger_border(a: Option<&CellBorderEdge>, a_direct: bool, b: Option<&CellBorderEdge>, b_direct: bool) -> Option<CellBorderEdge> {
    use crate::engine::border::LineStyle as S;
    let (a, b) = match (a, b) {
        (None, other) | (other, None) => return other.cloned(),
        (Some(a), Some(b)) => (a, b),
    };
    if a_direct != b_direct {
        return Some(if a_direct { a } else { b }.clone());
    }
    let weight = |e: &CellBorderEdge| match e.style {
        S::Dotted | S::Dashed => 1.0,
        // px → eighths of a point is ×6.
        style => (e.width_px * 6.0).round() * style.number(),
    };
    let rgb = |e: &CellBorderEdge| {
        let h = e.color.trim_start_matches('#');
        let ch = |i: usize| h.get(i..i + 2).and_then(|x| u8::from_str_radix(x, 16).ok()).unwrap_or(0) as i64;
        (ch(0), ch(2), ch(4))
    };
    let brightness = |e: &CellBorderEdge| {
        let (r, g, b) = rgb(e);
        (r + b + 2 * g, b + 2 * g, g)
    };
    let a_wins = weight(a).total_cmp(&weight(b))
        .then_with(|| (b.style as u8).cmp(&(a.style as u8)))
        .then_with(|| brightness(b).cmp(&brightness(a)))
        .is_ge();
    Some(if a_wins { a } else { b }.clone())
}

/// Resolve every shared cell edge to the one border that shows
/// (`stronger_border`): between neighbours in a row, and between a cell and
/// the cell below it when both cover the same grid columns. Both cells keep
/// the winner, so a row that starts a new page still draws its top. Declared:
/// cells whose grid columns only partly overlap keep their own edges, and
/// merged-away cells are not compared.
fn resolve_border_conflicts(rows: &mut [TableRowCache], direct: &[Vec<[bool; 4]>]) {
    const TOP: usize = 0;
    const BOTTOM: usize = 1;
    const LEFT: usize = 2;
    const RIGHT: usize = 3;
    for (r, row) in rows.iter_mut().enumerate() {
        for i in 1..row.cells.len() {
            let (l, rt) = (&row.cells[i - 1], &row.cells[i]);
            if l.v_merged_away || rt.v_merged_away { continue; }
            let won = stronger_border(
                l.border_right.as_ref(), direct[r][i - 1][RIGHT],
                rt.border_left.as_ref(), direct[r][i][LEFT],
            );
            row.cells[i - 1].border_right = won.clone();
            row.cells[i].border_left = won;
        }
    }
    for r in 1..rows.len() {
        let (upper, lower) = rows.split_at_mut(r);
        let upper = &mut upper[r - 1];
        let lower = &mut lower[0];
        for li in 0..lower.cells.len() {
            let l = &lower.cells[li];
            if l.v_merged_away { continue; }
            let Some(ui) = upper.cells.iter().position(|u| u.col_start == l.col_start && u.col_span == l.col_span) else { continue };
            if upper.cells[ui].v_merged_away { continue; }
            let won = stronger_border(
                upper.cells[ui].border_bottom.as_ref(), direct[r - 1][ui][BOTTOM],
                l.border_top.as_ref(), direct[r][li][TOP],
            );
            upper.cells[ui].border_bottom = won.clone();
            lower.cells[li].border_top = won;
        }
    }
}

// Two-state (absent OR nil BOTH collapse to None). Correct for TABLE-level
// edges, where there is no further per-edge fallback below them.
fn border_to_edge(theme: &ThemeFonts, b: &Option<Box<ooxml_wml::types::CTBorder>>) -> Option<CellBorderEdge> {
    b.as_ref().and_then(|bd| border_value(theme, bd))
}

// Three-state edge resolution keyed on element PRESENCE (§17.4.66 tcBorders
// conflict rule 6 "the cell border always wins" over §17.4.39/§17.4.38
// tblBorders): a cell/region-specified edge — INCLUDING explicit nil/none —
// overrides the inherited table/style edge.
//   None            → element absent      : inherit
//   Some(None)      → present but nil/none : explicit suppression wins
//   Some(Some(edge))→ a real drawn border
fn edge_override(theme: &ThemeFonts, b: &Option<Box<ooxml_wml::types::CTBorder>>) -> Option<Option<CellBorderEdge>> {
    b.as_ref().map(|bd| border_value(theme, bd))
}

/// The table's own borders: each edge from the table's tblBorders, else
/// from its style hierarchy's (§17.4.38); the nearest that names the edge
/// decides, a nil there removing it.
fn extract_table_border_edges(
    theme: &ThemeFonts,
    props: &ooxml_wml::types::TableProperties,
    style: &TableStyle,
) -> TableBorderEdges {
    let mut e = TableBorderEdges { top: None, bottom: None, left: None, right: None, inside_h: None, inside_v: None };
    for b in style.tbl_prs().filter_map(|tp| tp.tbl_borders.as_deref()).chain(props.tbl_borders.as_deref()) {
        if let Some(v) = edge_override(theme, &b.top) { e.top = v; }
        if let Some(v) = edge_override(theme, &b.bottom) { e.bottom = v; }
        if let Some(v) = edge_override(theme, &b.left).or_else(|| edge_override(theme, &b.start)) { e.left = v; }
        if let Some(v) = edge_override(theme, &b.right).or_else(|| edge_override(theme, &b.end)) { e.right = v; }
        if let Some(v) = edge_override(theme, &b.inside_h) { e.inside_h = v; }
        if let Some(v) = edge_override(theme, &b.inside_v) { e.inside_v = v; }
    }
    e
}

#[allow(clippy::too_many_arguments)] // positional orchestrator args (deferred)
fn extract_cell_borders(
    theme: &ThemeFonts,
    cell: &ooxml_wml::types::TableCell,
    table_borders: &TableBorderEdges,
    cond: &EdgeOverrides,
    row_idx: usize,
    cell_idx: usize,
    num_rows: usize,
    num_cols: usize,
) -> (Option<CellBorderEdge>, Option<CellBorderEdge>, Option<CellBorderEdge>, Option<CellBorderEdge>) {
    let tcb = cell.properties()
        .and_then(|p| p.tc_borders.as_ref());

    // Precedence per §17.4.66/§17.4.39 rules 5-6: a direct cell edge — drawn OR
    // explicit-nil — always wins; then the conditional tblStylePr region edge;
    // then the table/style default. `resolve` collapses the three-state cell &
    // region overrides against the two-state table default.
    let resolve = |cell_ov: Option<Option<CellBorderEdge>>,
                   region_ov: &Option<Option<CellBorderEdge>>,
                   table: Option<CellBorderEdge>| -> Option<CellBorderEdge> {
        match cell_ov {
            Some(v) => v,                                  // cell present: drawn or nil-suppressed
            None => match region_ov {
                Some(v) => v.clone(),                      // region present: drawn or nil-suppressed
                None => table,                             // both absent: table/style default
            },
        }
    };

    let top = resolve(
        tcb.and_then(|b| edge_override(theme, &b.top)), &cond.0,
        if row_idx == 0 { table_borders.top.clone() } else { table_borders.inside_h.clone() });
    // saturating_sub: a malformed table can declare ZERO grid columns while
    // still carrying cells (corpus: tdf104713_undefinedStyles.docx ships an
    // empty tblGrid) — `0 - 1` underflow-panics in debug and wraps in release.
    // The degenerate read "index 0 is also last" yields the OUTER border,
    // which is the sane rendering for a sole cell.
    let bottom = resolve(
        tcb.and_then(|b| edge_override(theme, &b.bottom)), &cond.1,
        if row_idx == num_rows.saturating_sub(1) { table_borders.bottom.clone() } else { table_borders.inside_h.clone() });
    let left = resolve(
        tcb.and_then(|b| edge_override(theme, &b.left).or_else(|| edge_override(theme, &b.start))), &cond.2,
        if cell_idx == 0 { table_borders.left.clone() } else { table_borders.inside_v.clone() });
    let right = resolve(
        tcb.and_then(|b| edge_override(theme, &b.right).or_else(|| edge_override(theme, &b.end))), &cond.3,
        if cell_idx == num_cols.saturating_sub(1) { table_borders.right.clone() } else { table_borders.inside_v.clone() });

    (top, bottom, left, right)
}

#[cfg(test)]
mod tests {
    use super::{
        build_table_meta, layout_table, CellBorderEdge, CellVAlign, TableCache, TableCellCache,
        TableRowCache,
    };
    use crate::engine::fonts::{FontCache, ThemeFonts};
    use crate::engine::fragment::Block;
    use crate::wml::numbering::ListCounters;
    use crate::wml::select::{extract_range_impl, ParaCapture, ParaLocation, ParaMeta, TextPos};
    use ooxml_wml::ext::StyleContext;
    use ooxml_wml::types::{
        BlockContent, CTBorder, CTRow, CTTblBorders, CTTcBorders, CellContent, Paragraph,
        ParagraphContent, RowContent, Run, RunContent, STBorder, STJcTable, Table, TableCell,
        TableCellProperties, TableGrid, TableGridColumn, TableProperties, Text,
    };
    use std::collections::HashMap;

    fn text_para(s: &str) -> Paragraph {
        Paragraph {
            paragraph_content: vec![ParagraphContent::R(Box::new(Run {
                run_content: vec![RunContent::T(Box::new(Text {
                    text: Some(s.to_string()),
                    ..Default::default()
                }))],
                ..Default::default()
            }))],
            ..Default::default()
        }
    }

    fn cell_with(blocks: Vec<BlockContent>) -> CellContent {
        CellContent::Tc(Box::new(TableCell {
            block_content: blocks,
            ..Default::default()
        }))
    }

    fn one_row(cells: Vec<CellContent>) -> RowContent {
        RowContent::Tr(Box::new(CTRow {
            cells,
            ..Default::default()
        }))
    }

    fn table_of(cols: usize, rows: Vec<RowContent>) -> Table {
        Table {
            range_markup: Vec::new(),
            table_properties: Box::new(TableProperties::default()),
            tbl_grid: Box::new(TableGrid {
                grid_col: (0..cols).map(|_| TableGridColumn::default()).collect(),
                ..Default::default()
            }),
            rows,
            extra_children: Vec::new(),
        }
    }

    /// Recursively gather every run's text from a fragment block tree.
    fn collect_all_text(blocks: &[Block]) -> String {
        let mut out = String::new();
        for b in blocks {
            match b {
                Block::Para(p) => {
                    for r in &p.runs {
                        out.push_str(&r.text);
                    }
                }
                Block::Table(t) => {
                    for row in &t.rows {
                        for c in &row.cells {
                            out.push_str(&collect_all_text(&c.blocks));
                        }
                    }
                }
            }
        }
        out
    }

    // Nested-table fix — a paragraph inside a NESTED table must NOT be silently dropped on
    // copy. The outer table is 1×1; its single cell holds a body paragraph
    // ("OUTER") followed by a nested 1×2 table ("NA" | "NB"). The nested cells'
    // inner (row, cell) coordinates would overwrite the outer coordinates —
    // mis-routing "NA" into the outer cell and DROPPING "NB" (out of the 1-cell
    // outer grid). The fix captures nested-cell paragraphs as BODY fallback so
    // their text still copies (as body text, not in-grid — a documented v1 limit).
    #[test]
    fn nested_table_cell_text_is_not_dropped() {
        let inner = table_of(
            2,
            vec![one_row(vec![
                cell_with(vec![BlockContent::P(Box::new(text_para("NA")))]),
                cell_with(vec![BlockContent::P(Box::new(text_para("NB")))]),
            ])],
        );
        let outer = table_of(
            1,
            vec![one_row(vec![cell_with(vec![
                BlockContent::P(Box::new(text_para("OUTER"))),
                BlockContent::Tbl(Box::new(inner)),
            ])])],
        );

        let font_cache = FontCache::new();
        let style_ctx = StyleContext::default();
        let theme_fonts = ThemeFonts::empty();
        let numbering_levels = HashMap::new();
        let mut list_counters = ListCounters::default();

        let mut next_id = 0u32;
        let mut sink: Vec<ParaMeta> = Vec::new();
        let mut cap = ParaCapture {
            next_id: &mut next_id,
            sink: &mut sink,
            location: ParaLocation::TableCell { sec: 0, block: 0, row: 0, cell: 0 },
            nested: false,
        };
        let cache = layout_table(
            &font_cache,
            &outer,
            400.0,
            &style_ctx,
            None,
            48.0,
            &theme_fonts,
            &numbering_levels,
            &mut list_counters,
            Some(&mut cap),
        )
        .expect("layout_table");

        // The nested-cell paragraphs are captured as BODY fallback (not TableCell).
        let nested_text: String = sink
            .iter()
            .filter(|m| matches!(m.location, ParaLocation::Body { .. }))
            .map(|m| m.logical_text.clone())
            .collect();
        assert!(
            nested_text.contains("NA") && nested_text.contains("NB"),
            "nested-cell text must survive as body fallback: {:?}",
            sink.iter()
                .map(|m| (m.logical_text.clone(), format!("{:?}", m.location)))
                .collect::<Vec<_>>()
        );

        // End-to-end: extraction over the full range emits every text (nothing lost).
        let mut tables = HashMap::new();
        tables.insert((0usize, 0usize), build_table_meta(&cache));
        let last = sink.len() as u32 - 1;
        let end_byte = sink.last().map(|m| m.logical_text.len() as u32).unwrap_or(0);
        let frag = extract_range_impl(&sink, &tables, TextPos::new(0, 0), TextPos::new(last, end_byte));
        let all = collect_all_text(&frag.blocks);
        assert!(
            all.contains("OUTER") && all.contains("NA") && all.contains("NB"),
            "extraction dropped nested-table text: {:?}",
            all
        );
    }

    // `build_table_meta` derives rowspan from `v_merged_away` continuation
    // cells and captures fill + border. Row 0 is the merge anchor (restart); row 1
    // is the continuation at the same grid column.
    #[test]
    fn build_table_meta_derives_vmerge_rowspan_and_captures_borders() {
        let border = CellBorderEdge { color: "#123456".to_string(), width_px: 2.0, ..Default::default() };
        let cell = |v_merged_away: bool, fill: Option<&str>| TableCellCache {
            blocks: Vec::new(),
            col_start: 0,
            col_span: 1,
            x_px: 0.0,
            width_px: 100.0,
            fill_color: fill.map(str::to_string),
            v_align: CellVAlign::Top,
            v_merged_away,
            border_top: Some(border.clone()),
            border_bottom: Some(border.clone()),
            border_left: Some(border.clone()),
            border_right: Some(border.clone()),
            diagonals: None,
            mar_top: 0.0,
            mar_bottom: 0.0,
            mar_left: 0.0,
            mar_right: 0.0,
            float_layout: None,
        };
        let cache = TableCache {
            col_widths_px: vec![100.0],
            rows: vec![
                TableRowCache {
                    cells: vec![cell(false, Some("#FF0000"))],
                    height_px: 20.0,
                    cant_split: false,
                },
                TableRowCache {
                    cells: vec![cell(true, None)],
                    height_px: 20.0,
                    cant_split: false,
                },
            ],
            border_color: "#000000".to_string(),
            has_borders: true,
            alignment: STJcTable::Start,
            indent_px: 0.0,
        };

        let meta = build_table_meta(&cache);
        assert_eq!(meta.rows.len(), 2);

        let anchor = &meta.rows[0].cells[0];
        assert_eq!(anchor.rowspan, 2, "anchor absorbs the continuation cell");
        assert!(!anchor.v_merged_away);
        assert_eq!(anchor.fill.as_deref(), Some("#FF0000"));
        // borders = [top, right, bottom, left]; each edge captured.
        let top = anchor.borders[0].as_ref().expect("top border captured");
        assert_eq!(top.style, "single");
        assert_eq!(top.color, "#123456");
        assert_eq!(top.width_px, 2.0);

        let cont = &meta.rows[1].cells[0];
        assert_eq!(cont.rowspan, 1, "a v_merged_away continuation is rowspan 1");
        assert!(cont.v_merged_away);
    }

    // §17.4.66 conflict rule 6 "the cell border
    // always wins" — an explicit `<w:tcBorders>` edge set to nil/none on a cell
    // SUPPRESSES the inherited table border rather than falling through to it.
    // cell[0] sets top=nil over a fully single-bordered table → its resolved top
    // must be None; the adjacent cell[1] with NO tcBorders still inherits the
    // table's top. Pre-fix `border_to_edge(nil)` returned None and the `.or_else`
    // chain re-inherited the table single, so cell[0].border_top was Some(single)
    // and this `is_none()` assert panicked.
    #[test]
    fn cell_nil_tcborder_suppresses_inherited_table_border() {
        // CTBorder has no Default derive (its `value` is a non-Default enum), so
        // build one explicitly. Fields are the wml-styling set the suite builds with.
        fn ctb(value: STBorder) -> Box<CTBorder> {
            Box::new(CTBorder {
                value,
                color: Some("000000".to_string()),
                theme_color: None,
                theme_tint: None,
                theme_shade: None,
                size: Some(4),
                space: None,
                shadow: None,
                frame: None,
            })
        }
        let single = || Some(ctb(STBorder::Single));

        // Direct table borders: full single on every edge (no style needed).
        let props = TableProperties {
            tbl_borders: Some(Box::new(CTTblBorders {
                top: single(), bottom: single(), left: single(), right: single(),
                inside_h: single(), inside_v: single(),
                ..Default::default()
            })),
            ..Default::default()
        };

        // cell[0]: explicit top=nil (suppress). cell[1]: no tcBorders (inherit).
        let suppressed = CellContent::Tc(Box::new(TableCell {
            cell_properties: Some(Box::new(TableCellProperties {
                tc_borders: Some(Box::new(CTTcBorders {
                    top: Some(ctb(STBorder::Nil)),
                    ..Default::default()
                })),
                ..Default::default()
            })),
            block_content: vec![BlockContent::P(Box::new(text_para("a")))],
            ..Default::default()
        }));
        let inherit = cell_with(vec![BlockContent::P(Box::new(text_para("b")))]);

        let table = Table {
            range_markup: Vec::new(),
            table_properties: Box::new(props),
            tbl_grid: Box::new(TableGrid {
                grid_col: (0..2).map(|_| TableGridColumn::default()).collect(),
                ..Default::default()
            }),
            rows: vec![one_row(vec![suppressed, inherit])],
            extra_children: Vec::new(),
        };

        let font_cache = FontCache::new();
        let style_ctx = StyleContext::default();
        let theme_fonts = ThemeFonts::empty();
        let numbering_levels = HashMap::new();
        let mut list_counters = ListCounters::default();
        let cache = layout_table(
            &font_cache, &table, 400.0, &style_ctx, None, 48.0,
            &theme_fonts, &numbering_levels, &mut list_counters, None,
        )
        .expect("layout_table");

        let c0 = &cache.rows[0].cells[0];
        assert!(
            c0.border_top.is_none(),
            "explicit tcBorders top=nil must suppress the inherited table top border",
        );
        // The un-overridden edges of cell[0] still inherit the table single…
        assert!(c0.border_left.is_some(), "cell[0]'s non-nil left edge still inherits the table border");
        // …and the adjacent cell WITHOUT any tcBorders keeps the table top (control).
        let c1 = &cache.rows[0].cells[1];
        assert!(
            c1.border_top.is_some(),
            "adjacent cell without tcBorders must keep the inherited table top border",
        );
    }

    /// Build a one-row table whose second cell carries `<w:gridSpan w:val=…>`.
    fn table_with_grid_span(grid_cols: usize, span: i64) -> Table {
        let spanned = CellContent::Tc(Box::new(TableCell {
            block_content: vec![BlockContent::P(Box::new(text_para("B")))],
            cell_properties: Some(Box::new(TableCellProperties {
                grid_span: Some(Box::new(ooxml_wml::types::CTDecimalNumber { value: span })),
                ..Default::default()
            })),
            ..Default::default()
        }));
        let first = cell_with(vec![BlockContent::P(Box::new(text_para("A")))]);
        table_of(grid_cols, vec![one_row(vec![first, spanned])])
    }

    fn lay_out(table: &Table) -> TableCache {
        let font_cache = FontCache::new();
        let style_ctx = StyleContext::default();
        let theme_fonts = ThemeFonts::empty();
        let numbering_levels = HashMap::new();
        let mut list_counters = ListCounters::default();
        layout_table(
            &font_cache, table, 400.0, &style_ctx, None, 48.0,
            &theme_fonts, &numbering_levels, &mut list_counters, None,
        )
        .expect("layout_table must not fail")
    }

    /// `<w:gridSpan w:val="-1"/>` (§17.4.17), a verified crash vector.
    /// `gs.value as usize` turned `-1` into
    /// `usize::MAX`, and `col_idx + col_span` panicked with "attempt to add with
    /// overflow" (debug) / inverted the slice to `[1..0]` (release) on merely
    /// OPENING the document. §17.4.17's own omitted-element rule supplies the
    /// answer: a value the spec gives no meaning is treated as an absent
    /// element, i.e. one grid column.
    #[test]
    fn non_positive_grid_span_falls_back_to_the_omitted_element_default() {
        for span in [-1i64, 0, i64::MIN] {
            let cache = lay_out(&table_with_grid_span(2, span));
            let cells = &cache.rows[0].cells;
            assert_eq!(cells.len(), 2, "gridSpan={span}: both cells must survive");
            assert_eq!(cells[1].col_span, 1, "gridSpan={span} spans one column (§17.4.17 default)");
            assert_eq!(cells[1].col_start, 1);
            assert!(
                (cells[1].width_px - cells[0].width_px).abs() < 1e-9,
                "gridSpan={span}: {} must match the un-spanned control {}",
                cells[1].width_px, cells[0].width_px,
            );
        }
    }

    /// The wasm32 half of the same bug. A span wider than `usize` is
    /// schema-valid (ST_DecimalNumber is `xsd:integer`), and `usize` is 32 BITS
    /// on the shipping target, so `i64 as usize` truncated: `w:val="4294967295"`
    /// wrapped `1 + span` to 0 and inverted the slice. The bound is applied to
    /// the signed value BEFORE any cast, so the result is the remaining grid
    /// width on every target width.
    #[test]
    fn grid_span_wider_than_usize_is_bounded_before_the_cast() {
        // 2-column grid, cell 1 starts at column 1 → 1 column remains.
        for span in [4_294_967_295i64, 4_294_967_296, i64::MAX] {
            assert_eq!(super::grid_span_columns(Some(span), 1), 1, "span={span}");
            let cache = lay_out(&table_with_grid_span(2, span));
            let cells = &cache.rows[0].cells;
            assert_eq!(cells[1].col_span, 1, "span={span} bounded to the real grid");
            assert!(cells[1].width_px.is_finite() && cells[1].width_px > 0.0);
        }
    }

    /// The §17.4.17 case that is LEGAL and must not change: *"If the number of
    /// grid units specified by the val attribute exceeds the size of the table
    /// grid, then the table grid shall be augmented…"*. A cell asking for more
    /// columns than the row has left still covers every remaining real column —
    /// same painted width as before this guard, which is the sum of the columns
    /// the slice already saturated to.
    #[test]
    fn over_wide_grid_span_still_spans_the_rest_of_the_row() {
        let cache = lay_out(&table_with_grid_span(4, 9));
        let cells = &cache.rows[0].cells;
        let grid: f64 = cache.col_widths_px.iter().sum();
        let rest: f64 = cache.col_widths_px[1..].iter().sum();
        assert_eq!(cells[1].col_start, 1);
        assert_eq!(cells[1].col_span, 3, "spans the 3 grid columns that exist");
        assert!(
            (cells[1].width_px - rest).abs() < 1e-9,
            "spanned width {} must be the remaining grid {rest} (of {grid})",
            cells[1].width_px,
        );
    }

    /// Ordinary spans are untouched: an absent element and every in-grid value
    /// resolve to exactly what the document asked for.
    #[test]
    fn ordinary_grid_spans_pass_through_unbounded() {
        assert_eq!(super::grid_span_columns(None, 4), 1, "absent ⇒ one (§17.4.17)");
        for span in 1i64..=4 {
            assert_eq!(super::grid_span_columns(Some(span), 4), span as usize);
        }
        // Even a zero-column remainder yields a non-empty (if zero-width) span
        // rather than an inverted range.
        assert_eq!(super::grid_span_columns(Some(3), 0), 1);
    }
}
