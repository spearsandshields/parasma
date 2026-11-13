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

use std::collections::HashMap;

use wasm_bindgen::prelude::*;

use ooxml_wml::ext::{CellExt, StyleContext, TableExt};
use ooxml_wml::types::{BlockContent, STBorder, STJcTable, STTblWidth};

use crate::engine::color::ooxml_color_to_css;
use crate::engine::fonts::{FontCache, ThemeFonts, DEFAULT_FONT_PT};
use crate::engine::units::twips_to_px;
use crate::wml::numbering::{resolve_numbering_marker, NumberingLevelDef};
use crate::{
    layout_paragraph, CellBlock, Item, DEFAULT_CELL_MARGIN_H_TWIPS, DEFAULT_CELL_MARGIN_V_TWIPS,
};
use crate::wml::drawing::WrapMode;
use crate::wml::float::{
    build_active_float, clear_below_floats, pack_lines_around_floats, ActiveFloat, WRAP_EPS,
};
use crate::wml::float_resolve::{resolve_anchor_position, FrameCtx};
use crate::wml::page::{PagedLine, PARA_SENTINEL};
use crate::wml::render::layout_text_box;

// ─── Table layout cache ─────────────────────────────────────────────────────

pub(crate) struct TableCache {
    pub(crate) col_widths_px: Vec<f64>,
    pub(crate) rows: Vec<TableRowCache>,
    pub(crate) border_color: String,
    pub(crate) has_borders: bool,
    pub(crate) alignment: STJcTable,
    pub(crate) cell_margin_h: f64,
    pub(crate) cell_margin_v: f64,
    pub(crate) indent_px: f64,
}

pub(crate) struct TableRowCache {
    pub(crate) cells: Vec<TableCellCache>,
    pub(crate) height_px: f64,
    /// §17.4.7 `w:cantSplit` (trPr): if set, the row must not be broken across a
    /// page boundary — it is placed whole, pushed to a fresh page if it does not
    /// fit the page remainder. When unset (the default here), a row taller than
    /// the remaining space splits across the break at line granularity.
    pub(crate) cant_split: bool,
}

pub(crate) struct TableCellCache {
    pub(crate) blocks: Vec<CellBlock>,
    pub(crate) col_start: usize,
    pub(crate) col_span: usize,
    pub(crate) width_px: f64,
    pub(crate) fill_color: Option<String>,
    pub(crate) v_align: CellVAlign,
    pub(crate) v_merged_away: bool,
    pub(crate) border_top: Option<CellBorderEdge>,
    pub(crate) border_bottom: Option<CellBorderEdge>,
    pub(crate) border_left: Option<CellBorderEdge>,
    pub(crate) border_right: Option<CellBorderEdge>,
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
        fill_color: Option<String>, border_color: Option<String>, border_width_px: f64,
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
        for block in &self.blocks {
            match block {
                CellBlock::Paragraph(pc) => {
                    let gap = prev_after.max(pc.layout.space_before_px);
                    total += gap;
                    total += pc.lines.iter().map(|l| pc.layout.line_height(l.max_font_pt, l.max_ascent + l.max_descent, l.max_image_height)).sum::<f64>();
                    prev_after = pc.layout.space_after_px;
                }
                CellBlock::Table(tc) => {
                    total += prev_after;
                    total += tc.rows.iter().map(|r| r.height_px).sum::<f64>();
                    prev_after = 0.0;
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
/// cell hosts ≥1 wrapping anchor AND has no nested table (nested-cell floats are
/// a tracked follow-on). Mirrors the body float seam — resolve_anchor_position →
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
    // wrapping anchor. An empty-Vec scan — zero cost for float-free cells.
    let mut has_wrapping = false;
    for block in blocks {
        match block {
            CellBlock::Table(_) => return None,
            CellBlock::Paragraph(pc) => {
                if pc.anchored_images.iter().any(|a| !matches!(a.wrap.mode, WrapMode::None))
                    || pc.anchored_text_boxes.iter().any(|t| !matches!(t.wrap.mode, WrapMode::None))
                {
                    has_wrapping = true;
                }
            }
        }
    }
    if !has_wrapping {
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

    for block in blocks {
        let CellBlock::Paragraph(pc) = block else { continue }; // gate excluded Tables
        let gap = prev_after.max(pc.layout.space_before_px);
        cursor += gap;
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

        // (1) Resolve this paragraph's wrapping anchors at the paragraph top and
        //     register their exclusions + draw records (cell-local).
        for ai in &pc.anchored_images {
            if matches!(ai.wrap.mode, WrapMode::None) {
                continue;
            }
            let (fx, fy) = resolve_anchor_position(
                &ai.pos_h, &ai.pos_v, ai.h_relative_from, ai.v_relative_from,
                ai.width_px, ai.height_px, &fctx,
            );
            if let Some(af) = build_active_float(&ai.wrap, fx, fy, ai.width_px, ai.height_px, dom_left, dom_right) {
                floats.push(af);
            }
            draws.push(CellFloatDraw::Image {
                rel_id: ai.rel_id.clone(), x: fx, y: fy,
                width_px: ai.width_px, height_px: ai.height_px, behind_text: ai.behind_text,
                anchor_para: pc.para_id,
            });
        }
        for tb in &pc.anchored_text_boxes {
            if matches!(tb.wrap.mode, WrapMode::None) {
                continue;
            }
            let (tx, ty) = resolve_anchor_position(
                &tb.pos_h, &tb.pos_v, tb.h_relative_from, tb.v_relative_from,
                tb.width_px, tb.height_px, &fctx,
            );
            if let Some(af) = build_active_float(&tb.wrap, tx, ty, tb.width_px, tb.height_px, dom_left, dom_right) {
                floats.push(af);
            }
            let tb_lines = layout_text_box(
                &tb.raw, tb.width_px, font_cache, style_ctx, default_ppr,
                default_tab_stop_px, theme_fonts, numbering_levels,
            );
            draws.push(CellFloatDraw::TextBox {
                lines: tb_lines, x: tx, y: ty,
                width_px: tb.width_px, height_px: tb.height_px, behind_text: tb.behind_text,
                fill_color: tb.fill_color.clone(), border_color: tb.border_color.clone(),
                border_width_px: tb.border_width_px,
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
    }

    // Float-aware cell content height (pre v-margin): the lower of text bottom
    // (incl. the last paragraph's after-spacing, matching content_height) and
    // the bottom of any float must fit, so take the max.
    let text_bottom = cursor + prev_after;
    let float_bottom = draws.iter().fold(0.0_f64, |m, d| {
        let b = match d {
            CellFloatDraw::Image { y, height_px, .. } => y + height_px,
            CellFloatDraw::TextBox { y, height_px, .. } => y + height_px,
        };
        m.max(b)
    });
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
    list_counters: &mut HashMap<(i64, i64), i64>,
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
    let tbl_style_id = props.tbl_style.as_ref().map(|s| s.value.as_str());

    let resolve_cell_mar = |mar: &ooxml_wml::types::CTTblCellMar| -> (i64, i64) {
        let h = mar.start.as_ref().or(mar.left.as_ref())
            .and_then(|w| w.width.as_deref()?.parse().ok())
            .unwrap_or(DEFAULT_CELL_MARGIN_H_TWIPS);
        let v = mar.top.as_ref()
            .and_then(|w| w.width.as_deref()?.parse().ok())
            .unwrap_or(DEFAULT_CELL_MARGIN_V_TWIPS);
        (h, v)
    };

    let (cell_margin_h_twips, cell_margin_v_twips) = props.tbl_cell_mar.as_deref()
        .map(resolve_cell_mar)
        .or_else(|| {
            tbl_style_id
                .and_then(|id| style_ctx.style(id))
                .and_then(|s| s.table_properties.as_ref())
                .and_then(|tp| tp.tbl_cell_mar.as_deref())
                .map(resolve_cell_mar)
        })
        .unwrap_or((DEFAULT_CELL_MARGIN_H_TWIPS, DEFAULT_CELL_MARGIN_V_TWIPS));

    let cell_margin_h = twips_to_px(cell_margin_h_twips);
    let cell_margin_v = twips_to_px(cell_margin_v_twips);

    // Per-cell margins (§17.4.42 tcMar) override the table/row default. Returns
    // (top, bottom, left, right) px; each edge falls back to the table value.
    let resolve_tc_mar = |tcm: Option<&ooxml_wml::types::CTTcMar>| -> (f64, f64, f64, f64) {
        let edge = |w: Option<&Box<ooxml_wml::types::CTTblWidth>>, dflt: i64| -> f64 {
            twips_to_px(w.and_then(|w| w.width.as_deref()?.parse().ok()).unwrap_or(dflt))
        };
        match tcm {
            Some(m) => (
                edge(m.top.as_ref(), cell_margin_v_twips),
                edge(m.bottom.as_ref(), cell_margin_v_twips),
                edge(m.start.as_ref().or(m.left.as_ref()), cell_margin_h_twips),
                edge(m.end.as_ref().or(m.right.as_ref()), cell_margin_h_twips),
            ),
            None => (cell_margin_v, cell_margin_v, cell_margin_h, cell_margin_h),
        }
    };

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
        // (auto-fit), NOT a literal zero-width table. Only a positive Pct/Dxa
        // value is an explicit target; otherwise fall through to grid-based
        // sizing below (down-scaling the grid if it overflows the content box).
        let target_width = table.properties().tbl_w.as_ref().and_then(|tw| {
            let val = tw.width.as_deref()?.parse::<f64>().ok()?;
            match tw.r#type? {
                STTblWidth::Pct if val > 0.0 => Some((val / 5000.0) * content_width),
                STTblWidth::Dxa if val > 0.0 => Some(twips_to_px(val as i64)),
                _ => None,
            }
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
    let table_border_edges = extract_table_border_edges(props, tbl_style_id, style_ctx);
    let alignment = props.justification.as_ref()
        .map(|j| j.value)
        .unwrap_or(STJcTable::Start);

    let tbl_look = props.tbl_look.as_deref();
    let on_off = |v: &Option<String>| v.as_deref().map(|s| s == "1" || s == "true").unwrap_or(false);
    let look_first_row = tbl_look.map(|l| on_off(&l.first_row)).unwrap_or(false);
    let look_last_row = tbl_look.map(|l| on_off(&l.last_row)).unwrap_or(false);
    let look_first_col = tbl_look.map(|l| on_off(&l.first_column)).unwrap_or(false);
    let look_last_col = tbl_look.map(|l| on_off(&l.last_column)).unwrap_or(false);
    let look_no_h_band = tbl_look.map(|l| on_off(&l.no_h_band)).unwrap_or(false);

    let style_overrides: &[ooxml_wml::types::TableStyleProperties] = tbl_style_id
        .and_then(|id| style_ctx.style(id))
        .map(|s| s.tbl_style_pr.as_slice())
        .unwrap_or(&[]);

    let table_rows = table.rows();
    let num_rows = table_rows.len();

    let mut rows = Vec::new();
    for (row_idx, row) in table_rows.iter().enumerate() {
        let mut cells = Vec::new();
        let mut col_idx: usize = 0;
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
            let (cmar_top, cmar_bottom, cmar_left, cmar_right) =
                resolve_tc_mar(cell.properties().and_then(|p| p.tc_mar.as_deref()));
            let cell_content_width = (cell_width - cmar_left - cmar_right).max(0.0);

            let table_base_rpr = tbl_style_id
                .and_then(|id| style_ctx.style(id))
                .and_then(|s| s.r_pr.as_deref());
            let cond_rprs = collect_conditional_run_properties(
                row_idx, cell_idx, num_rows, num_cols,
                look_first_row, look_last_row, look_first_col, look_last_col, look_no_h_band,
                style_overrides, table_base_rpr,
            );

            let cond_pprs = collect_conditional_para_properties(
                row_idx, cell_idx, num_rows, num_cols,
                look_first_row, look_last_row, look_first_col, look_last_col, look_no_h_band,
                style_overrides,
            );

            let mut cell_blocks: Vec<CellBlock> = Vec::new();
            if !v_merged_away {
                for block in &cell.block_content {
                    match block {
                        BlockContent::P(p) => {
                            let marker = resolve_numbering_marker(p, numbering_levels, list_counters, style_ctx);
                            if let Some(cap) = capture.as_deref_mut() {
                                cap.location = cell_para_location(cap.nested, cap_sec, cap_block, row_idx, cell_idx);
                            }
                            let cache = layout_paragraph(font_cache, p, cell_content_width, marker.as_ref(), style_ctx, default_ppr, default_tab_stop_px, theme_fonts, tbl_style_id, &cond_rprs, &cond_pprs, None, is_exact_row, capture.as_deref_mut())?;
                            cell_blocks.push(CellBlock::Paragraph(cache));
                        }
                        BlockContent::Tbl(nested) => {
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
                        BlockContent::Sdt(sdt) => {
                            if let Some(ref content) = sdt.sdt_content {
                                for inner in &content.block_content {
                                    if let ooxml_wml::types::BlockContentChoice::P(p) = inner {
                                        let marker = resolve_numbering_marker(p, numbering_levels, list_counters, style_ctx);
                                        if let Some(cap) = capture.as_deref_mut() {
                                            cap.location = cell_para_location(cap.nested, cap_sec, cap_block, row_idx, cell_idx);
                                        }
                                        let cache = layout_paragraph(font_cache, p, cell_content_width, marker.as_ref(), style_ctx, default_ppr, default_tab_stop_px, theme_fonts, tbl_style_id, &cond_rprs, &cond_pprs, None, is_exact_row, capture.as_deref_mut())?;
                                        cell_blocks.push(CellBlock::Paragraph(cache));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            let direct_fill = cell.properties()
                .and_then(|p| p.shading.as_ref())
                .and_then(|s| s.fill.as_deref())
                .and_then(ooxml_color_to_css);

            let fill_color = direct_fill.or_else(|| {
                resolve_conditional_cell_fill(
                    row_idx, cell_idx, num_rows, num_cols,
                    look_first_row, look_last_row, look_first_col, look_last_col, look_no_h_band,
                    style_overrides,
                )
            });

            let direct_valign = cell.properties()
                .and_then(|p| p.v_align.as_ref())
                .map(|va| va.value);
            let v_align = match direct_valign {
                Some(ooxml_wml::types::STVerticalJc::Center) => CellVAlign::Center,
                Some(ooxml_wml::types::STVerticalJc::Bottom) => CellVAlign::Bottom,
                Some(_) => CellVAlign::Top,
                None => resolve_conditional_cell_valign(
                    row_idx, cell_idx, num_rows, num_cols,
                    look_first_row, look_last_row, look_first_col, look_last_col, look_no_h_band,
                    style_overrides,
                ),
            };

            let cond_borders = resolve_conditional_cell_borders(
                row_idx, cell_idx, num_rows, num_cols,
                look_first_row, look_last_row, look_first_col, look_last_col, look_no_h_band,
                style_overrides,
            );
            let (cell_bdr_top, cell_bdr_bottom, cell_bdr_left, cell_bdr_right) =
                extract_cell_borders(cell, &table_border_edges, &cond_borders, row_idx, cell_idx, num_rows, col_count);

            // Pre-resolve a cell-hosted wrapping float (None = fast path). Built
            // before `cell_blocks` is moved into the cache.
            let float_layout = if v_merged_away {
                None
            } else {
                build_cell_float_layout(
                    &cell_blocks, cell_width, cmar_left, cmar_right,
                    font_cache, style_ctx, default_ppr, default_tab_stop_px,
                    theme_fonts, numbering_levels,
                )
            };
            cells.push(TableCellCache {
                blocks: cell_blocks,
                col_start: col_idx,
                col_span,
                width_px: cell_width,
                fill_color: if v_merged_away { None } else { fill_color },
                v_align,
                v_merged_away,
                border_top: cell_bdr_top,
                border_bottom: cell_bdr_bottom,
                border_left: cell_bdr_left,
                border_right: cell_bdr_right,
                mar_top: cmar_top,
                mar_bottom: cmar_bottom,
                mar_left: cmar_left,
                mar_right: cmar_right,
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

        // §17.4.7 cantSplit (common boolean, §17.17.4): present + value not
        // explicitly off ("0"/"false"/"off") = on. An empty `<w:cantSplit/>`
        // (value None) is therefore ON — it is the canonical "on" form.
        let cant_split = row.row_properties.as_ref()
            .and_then(|p| p.cant_split.as_ref())
            .map(|o| !matches!(o.value.as_deref(), Some("0" | "false" | "off")))
            .unwrap_or(false);

        rows.push(TableRowCache { cells, height_px: row_height, cant_split });
    }

    Ok(TableCache { col_widths_px, rows, border_color, has_borders, alignment, cell_margin_h, cell_margin_v, indent_px })
}

/// The `tblStylePr` conditional regions that apply to a cell, ordered
/// least→most specific (banding first, then first/last col, then first/last
/// row). Callers iterate `.rev()` so the most specific region wins. Mirrors
/// the OOXML §17.7.6 conditional-formatting precedence.
// Positional orchestrator args; struct-ification deferred to a dedicated pass.
#[allow(clippy::too_many_arguments)]
fn conditional_regions(
    row_idx: usize, col_idx: usize, num_rows: usize, num_cols: usize,
    first_row: bool, last_row: bool, first_col: bool, last_col: bool, no_h_band: bool,
) -> Vec<ooxml_wml::types::STTblStyleOverrideType> {
    use ooxml_wml::types::STTblStyleOverrideType::*;
    let mut regions = Vec::new();
    if !no_h_band {
        let band_row = if first_row { row_idx.saturating_sub(1) } else { row_idx };
        if band_row % 2 == 0 { regions.push(Band1Horz); } else { regions.push(Band2Horz); }
    }
    if first_col && col_idx == 0 { regions.push(FirstCol); }
    if last_col && col_idx + 1 >= num_cols { regions.push(LastCol); }
    if first_row && row_idx == 0 { regions.push(FirstRow); }
    if last_row && row_idx + 1 >= num_rows { regions.push(LastRow); }
    regions
}

#[allow(clippy::too_many_arguments)] // positional orchestrator args (deferred)
fn resolve_conditional_cell_fill(
    row_idx: usize, col_idx: usize, num_rows: usize, num_cols: usize,
    first_row: bool, last_row: bool, first_col: bool, last_col: bool, no_h_band: bool,
    overrides: &[ooxml_wml::types::TableStyleProperties],
) -> Option<String> {
    let regions = conditional_regions(
        row_idx, col_idx, num_rows, num_cols,
        first_row, last_row, first_col, last_col, no_h_band);
    for region in regions.iter().rev() {
        for ov in overrides {
            if ov.r#type == *region {
                if let Some(ref tcp) = ov.cell_properties {
                    if let Some(ref shd) = tcp.shading {
                        let fill = shd.fill.as_deref().and_then(ooxml_color_to_css)?;
                        return Some(fill);
                    }
                }
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)] // positional orchestrator args (deferred)
fn resolve_conditional_cell_valign(
    row_idx: usize, col_idx: usize, num_rows: usize, num_cols: usize,
    first_row: bool, last_row: bool, first_col: bool, last_col: bool, no_h_band: bool,
    overrides: &[ooxml_wml::types::TableStyleProperties],
) -> CellVAlign {
    let regions = conditional_regions(
        row_idx, col_idx, num_rows, num_cols,
        first_row, last_row, first_col, last_col, no_h_band);
    for region in regions.iter().rev() {
        for ov in overrides {
            if ov.r#type == *region {
                if let Some(ref tcp) = ov.cell_properties {
                    if let Some(ref va) = tcp.v_align {
                        return match va.value {
                            ooxml_wml::types::STVerticalJc::Center => CellVAlign::Center,
                            ooxml_wml::types::STVerticalJc::Bottom => CellVAlign::Bottom,
                            _ => CellVAlign::Top,
                        };
                    }
                }
            }
        }
    }
    CellVAlign::Top
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

#[allow(clippy::too_many_arguments)] // positional orchestrator args (deferred)
fn resolve_conditional_cell_borders(
    row_idx: usize, col_idx: usize, num_rows: usize, num_cols: usize,
    first_row: bool, last_row: bool, first_col: bool, last_col: bool, no_h_band: bool,
    overrides: &[ooxml_wml::types::TableStyleProperties],
) -> EdgeOverrides {
    let regions = conditional_regions(
        row_idx, col_idx, num_rows, num_cols,
        first_row, last_row, first_col, last_col, no_h_band);
    let is_top = row_idx == 0;
    let is_bottom = row_idx + 1 >= num_rows;
    let is_left = col_idx == 0;
    let is_right = col_idx + 1 >= num_cols;

    let (mut top, mut bottom, mut left, mut right): EdgeOverrides = (None, None, None, None);
    // Iterate least→most specific so later (more specific) regions overwrite.
    // A region that specifies an edge — drawn OR explicit-nil — records
    // Some(..) so the cell-resolution short-circuits the table default (a
    // firstRow region can thus explicitly REMOVE the header separator).
    for region in regions.iter() {
        for ov in overrides {
            if ov.r#type != *region { continue; }
            // Region cell tcBorders → outer edges of cells in the region.
            if let Some(ref tcp) = ov.cell_properties {
                if let Some(ref b) = tcp.tc_borders {
                    if let Some(v) = edge_override(&b.top) { top = Some(v); }
                    if let Some(v) = edge_override(&b.bottom) { bottom = Some(v); }
                    if let Some(v) = edge_override(&b.left).or_else(|| edge_override(&b.start)) { left = Some(v); }
                    if let Some(v) = edge_override(&b.right).or_else(|| edge_override(&b.end)) { right = Some(v); }
                }
            }
            // Region tblPr tblBorders → outer edges + insideH/V for the region.
            if let Some(ref tp) = ov.table_properties {
                if let Some(ref b) = tp.tbl_borders {
                    if is_top { if let Some(v) = edge_override(&b.top) { top = Some(v); } }
                    else if let Some(v) = edge_override(&b.inside_h) { top = Some(v); }
                    if is_bottom { if let Some(v) = edge_override(&b.bottom) { bottom = Some(v); } }
                    else if let Some(v) = edge_override(&b.inside_h) { bottom = Some(v); }
                    if is_left { if let Some(v) = edge_override(&b.left).or_else(|| edge_override(&b.start)) { left = Some(v); } }
                    else if let Some(v) = edge_override(&b.inside_v) { left = Some(v); }
                    if is_right { if let Some(v) = edge_override(&b.right).or_else(|| edge_override(&b.end)) { right = Some(v); } }
                    else if let Some(v) = edge_override(&b.inside_v) { right = Some(v); }
                }
            }
        }
    }
    (top, bottom, left, right)
}

#[allow(clippy::too_many_arguments)] // positional orchestrator args (deferred)
fn collect_conditional_para_properties(
    row_idx: usize, col_idx: usize, num_rows: usize, num_cols: usize,
    first_row: bool, last_row: bool, first_col: bool, last_col: bool, no_h_band: bool,
    overrides: &[ooxml_wml::types::TableStyleProperties],
) -> Vec<&ooxml_wml::types::CTPPrGeneral> {
    use ooxml_wml::types::STTblStyleOverrideType::*;

    let mut regions: Vec<ooxml_wml::types::STTblStyleOverrideType> = Vec::new();
    if !no_h_band {
        let band_row = if first_row { row_idx.saturating_sub(1) } else { row_idx };
        if band_row % 2 == 0 { regions.push(Band1Horz); }
        else { regions.push(Band2Horz); }
    }
    if first_col && col_idx == 0 { regions.push(FirstCol); }
    if last_col && col_idx + 1 >= num_cols { regions.push(LastCol); }
    if first_row && row_idx == 0 { regions.push(FirstRow); }
    if last_row && row_idx + 1 >= num_rows { regions.push(LastRow); }

    let mut result = Vec::new();
    for region in regions.iter().rev() {
        for ov in overrides {
            if ov.r#type == *region {
                if let Some(ref ppr) = ov.p_pr {
                    result.push(ppr.as_ref());
                }
            }
        }
    }
    result
}

#[allow(clippy::too_many_arguments)] // positional orchestrator args (deferred)
fn collect_conditional_run_properties<'a>(
    row_idx: usize, col_idx: usize, num_rows: usize, num_cols: usize,
    first_row: bool, last_row: bool, first_col: bool, last_col: bool, no_h_band: bool,
    overrides: &'a [ooxml_wml::types::TableStyleProperties],
    table_style_rpr: Option<&'a ooxml_wml::types::RunProperties>,
) -> Vec<&'a ooxml_wml::types::RunProperties> {
    use ooxml_wml::types::STTblStyleOverrideType::*;

    let mut regions: Vec<ooxml_wml::types::STTblStyleOverrideType> = Vec::new();
    if !no_h_band {
        let band_row = if first_row { row_idx.saturating_sub(1) } else { row_idx };
        if band_row % 2 == 0 { regions.push(Band1Horz); }
        else { regions.push(Band2Horz); }
    }
    if first_col && col_idx == 0 { regions.push(FirstCol); }
    if last_col && col_idx + 1 >= num_cols { regions.push(LastCol); }
    if first_row && row_idx == 0 { regions.push(FirstRow); }
    if last_row && row_idx + 1 >= num_rows { regions.push(LastRow); }

    let mut result = Vec::new();
    for region in regions.iter().rev() {
        for ov in overrides {
            if ov.r#type == *region {
                if let Some(ref rpr) = ov.r_pr {
                    result.push(rpr.as_ref());
                }
            }
        }
    }
    if let Some(rpr) = table_style_rpr {
        result.push(rpr);
    }
    result
}

pub(crate) fn collect_row_cells(row: &ooxml_wml::types::CTRow) -> Vec<&ooxml_wml::types::TableCell> {
    let mut cells = Vec::new();
    for c in &row.cells {
        match c {
            ooxml_wml::types::CellContent::Tc(cell) => cells.push(cell.as_ref()),
            ooxml_wml::types::CellContent::Sdt(sdt) => {
                if let Some(ref content) = sdt.sdt_content {
                    for inner in &content.cells {
                        if let ooxml_wml::types::CellContent::Tc(cell) = inner {
                            cells.push(cell.as_ref());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    cells
}

pub(crate) fn collect_cell_paragraphs(cell: &ooxml_wml::types::TableCell) -> Vec<&ooxml_wml::types::Paragraph> {
    let mut paras = Vec::new();
    for block in &cell.block_content {
        match block {
            BlockContent::P(p) => paras.push(p.as_ref()),
            BlockContent::Sdt(sdt) => {
                if let Some(ref content) = sdt.sdt_content {
                    for inner in &content.block_content {
                        if let ooxml_wml::types::BlockContentChoice::P(p) = inner {
                            paras.push(p.as_ref());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    paras
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
fn border_value(b: &ooxml_wml::types::CTBorder) -> Option<CellBorderEdge> {
    if matches!(b.value, STBorder::None | STBorder::Nil) { return None; }
    let color = b.color.as_deref()
        .and_then(ooxml_color_to_css)
        .unwrap_or_else(|| "#000000".to_string());
    let sz = b.size.unwrap_or(4) as f64;
    // `sz` is in eighths of a point (ST_EighthPointMeasure). Clamp the rendered
    // width to a 1px floor so very thin borders (e.g. sz=1, ~0.125px) don't
    // vanish into anti-aliasing — Word draws them as a 1px device hairline.
    Some(CellBorderEdge { color, width_px: (sz / 8.0).max(1.0), dash: None })
}

// Two-state (absent OR nil BOTH collapse to None). Correct for TABLE-level
// edges, where there is no further per-edge fallback below them.
fn border_to_edge(b: &Option<Box<ooxml_wml::types::CTBorder>>) -> Option<CellBorderEdge> {
    b.as_ref().and_then(|bd| border_value(bd))
}

// Three-state edge resolution keyed on element PRESENCE (§17.4.66 tcBorders
// conflict rule 6 "the cell border always wins" over §17.4.39/§17.4.38
// tblBorders): a cell/region-specified edge — INCLUDING explicit nil/none —
// overrides the inherited table/style edge.
//   None            → element absent      : inherit
//   Some(None)      → present but nil/none : explicit suppression wins
//   Some(Some(edge))→ a real drawn border
fn edge_override(b: &Option<Box<ooxml_wml::types::CTBorder>>) -> Option<Option<CellBorderEdge>> {
    b.as_ref().map(|bd| border_value(bd))
}

fn extract_table_border_edges(
    props: &ooxml_wml::types::TableProperties,
    tbl_style_id: Option<&str>,
    style_ctx: &StyleContext,
) -> TableBorderEdges {
    let borders = props.tbl_borders.as_ref()
        .or_else(|| {
            tbl_style_id
                .and_then(|id| style_ctx.style(id))
                .and_then(|s| s.table_properties.as_ref())
                .and_then(|tp| tp.tbl_borders.as_ref())
        });
    match borders {
        Some(b) => TableBorderEdges {
            top: border_to_edge(&b.top),
            bottom: border_to_edge(&b.bottom),
            left: border_to_edge(&b.left).or_else(|| border_to_edge(&b.start)),
            right: border_to_edge(&b.right).or_else(|| border_to_edge(&b.end)),
            inside_h: border_to_edge(&b.inside_h),
            inside_v: border_to_edge(&b.inside_v),
        },
        None => TableBorderEdges {
            top: None, bottom: None, left: None, right: None,
            inside_h: None, inside_v: None,
        },
    }
}

fn extract_cell_borders(
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
        tcb.and_then(|b| edge_override(&b.top)), &cond.0,
        if row_idx == 0 { table_borders.top.clone() } else { table_borders.inside_h.clone() });
    // saturating_sub: a malformed table can declare ZERO grid columns while
    // still carrying cells (corpus: tdf104713_undefinedStyles.docx ships an
    // empty tblGrid) — `0 - 1` underflow-panics in debug and wraps in release.
    // The degenerate read "index 0 is also last" yields the OUTER border,
    // which is the sane rendering for a sole cell.
    let bottom = resolve(
        tcb.and_then(|b| edge_override(&b.bottom)), &cond.1,
        if row_idx == num_rows.saturating_sub(1) { table_borders.bottom.clone() } else { table_borders.inside_h.clone() });
    let left = resolve(
        tcb.and_then(|b| edge_override(&b.left).or_else(|| edge_override(&b.start))), &cond.2,
        if cell_idx == 0 { table_borders.left.clone() } else { table_borders.inside_v.clone() });
    let right = resolve(
        tcb.and_then(|b| edge_override(&b.right).or_else(|| edge_override(&b.end))), &cond.3,
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
        let mut list_counters = HashMap::new();

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
        let border = CellBorderEdge { color: "#123456".to_string(), width_px: 2.0, dash: None };
        let cell = |v_merged_away: bool, fill: Option<&str>| TableCellCache {
            blocks: Vec::new(),
            col_start: 0,
            col_span: 1,
            width_px: 100.0,
            fill_color: fill.map(str::to_string),
            v_align: CellVAlign::Top,
            v_merged_away,
            border_top: Some(border.clone()),
            border_bottom: Some(border.clone()),
            border_left: Some(border.clone()),
            border_right: Some(border.clone()),
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
            cell_margin_h: 0.0,
            cell_margin_v: 0.0,
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
        let mut list_counters = HashMap::new();
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
        let mut list_counters = HashMap::new();
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
