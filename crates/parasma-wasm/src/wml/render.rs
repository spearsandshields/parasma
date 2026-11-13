//! Render-walk leaves (§17 paint) — text-box layout/render, per-line canvas
//! painting, and page-field substitution.
//!
//! These are pure free fns over `&dyn Painter`; they are NOT routed through
//! `PaintCtx` (`render_line` calls `p.paint_glyph_run` directly for each glyph
//! run). The
//! `render_page` `#[wasm_bindgen]` method stays in `lib.rs`: its body has heavy
//! inline canvas drawing (page borders, watermark, table cell fill/border,
//! footnote/endnote layout+draw, column separators) that reads `self` fields and
//! is not a clean free-fn lift. `substitute_page_fields` mutates `PagedLine` for
//! PAGE/NUMPAGES fields and is called only from `render_page`; it moves with the
//! render free fns. `layout_text_box` is a layout helper (produces
//! `Vec<PagedLine>`) that pairs with `render_text_box`, so it lives here too.

use std::collections::HashMap;

use wasm_bindgen::JsValue;
use web_sys::ImageBitmap;

use ooxml_wml::ext::StyleContext;
use ooxml_wml::types::{BlockContent, CTTxbxContent};

use crate::chart;
use crate::engine::fonts::{FaceCache, FontCache, ThemeFonts};
use crate::engine::images::draw_image_placeholder;
use crate::engine::paint::{Painter, Underline, VertAlign};
use crate::engine::units::{emu_to_px, pt_to_px};
use crate::wml::drawing::is_layout_control;
use crate::wml::linebreak::{measure_item_text, FieldCode, Item, LineSegment, CHAR_START_SENTINEL};
use crate::wml::numbering::{resolve_numbering_marker, NumberingLevelDef};
use crate::wml::page::{PagedLine, PagedTableRow, PositionedTextBox, PARA_SENTINEL};
use crate::wml::paragraph::{layout_paragraph, TabLeader};
use crate::DEFAULT_TEXT_COLOR;

/// How many leader glyphs (§17.3.1.37 `w:leader`) to paint into a tab `width`
/// px wide, set in a run of `font_pt` points.
///
/// The malformed input this guards is `<w:sz w:val="0"/>`. §17.3.2.38 requires
/// the opposite — *"Specifies a positive measurement specified in half-points
/// (1/144 of an inch)"* — so a zero (or negative) font size is NOT conformant
/// and carries no size a leader can be measured in. But nothing rejects it on
/// the way in, and the old expression `(width / char_width).floor() as usize`
/// then evaluated `width / 0.0` = `+inf`, whose `as usize` cast SATURATES to
/// `usize::MAX`, and the caller's `leader_ch.repeat(count)` aborted the process
/// trying to allocate it. (Float→int saturation is not an overflow check, so
/// this bit identically in debug and release, on the PAINT path.)
///
/// Two bounds, neither of them a clamp on a document value:
///
/// * **Zero-or-negative glyph advance ⇒ no leader.** This is arithmetic, not a
///   chosen limit: a glyph that advances 0 px never reaches the end of the tab,
///   so no count fills it. Painting nothing is also exactly what the rest of a
///   0pt run paints.
/// * **At most one glyph per pixel of the tab.** ECMA-376 bounds neither the
///   leader run nor the tab that holds it (`w:tab/@w:pos` is
///   `ST_SignedTwipsMeasure`, §17.3.1.37 states no maximum), so there is no
///   spec number to cite and none is invented. The bound is the geometry the
///   glyphs are painted into: the run fills a box `width` px wide, and two
///   glyphs cannot occupy the same pixel of it, so beyond `width` glyphs the
///   extra ones add nothing that resolves on the surface. This can only bind
///   when the leader glyph advance is under 1 px — i.e. a font below ~2.1 pt —
///   because for any larger font `floor(width / char_width) ≤ width` already.
///   Every leader in a normally-sized run is therefore untouched, and the
///   allocation is bounded by the tab's own width rather than by the font size.
///
/// Residual, stated rather than papered over: the tab's `width` itself comes
/// from an unvalidated `w:tab/@w:pos`, so an absurd tab position still asks for
/// a large (though now font-size-independent, one-byte-per-pixel) string. That
/// exposure exists today at any font size and is a different document value —
/// out of scope for this guard, which neither creates nor amplifies it.
fn tab_leader_count(width: f64, font_pt: f64) -> usize {
    let char_width = pt_to_px(font_pt) * 0.35;
    // A non-finite tab width or glyph advance refuses outright rather than
    // saturating the `as usize` cast, which is what produced `usize::MAX`.
    if !char_width.is_finite() || char_width <= 0.0 || !width.is_finite() || width <= 0.0 {
        return 0;
    }
    // `width.ceil()` is finite, so the `min` is finite whatever the division did.
    //
    // DECLARED DEVIATION from §17.3.1.37, which says a leader "shall be repeated
    // as required to completely fill the tab spacing". The one-glyph-per-pixel
    // ceiling binds only when the glyph advance is BELOW 1px — i.e. below ~2.14pt
    // (`w:sz` ≤ 4, since sz is integer half-points), where a leader would need
    // more dots than the tab has pixels to render them in. There the leader stops
    // short of its stop: at `w:sz w:val="4"` with `w:pos="9000"`, 482 dots become
    // 450, a ~30px shortfall on sub-2pt text that is already unreadable.
    // Accepted deliberately: the alternative is an allocation whose size is set by
    // the ratio of two document-controlled values rather than by page geometry.
    // At every real body size the ceiling cannot bind — `floor(width/advance)` is
    // already ≤ width once the advance is ≥ 1px — which the tests pin.
    let count = (width / char_width).floor().min(width.ceil());
    if count >= 1.0 {
        count as usize
    } else {
        0
    }
}

pub(crate) fn substitute_page_fields(line: &mut PagedLine, page_num: usize, total: usize, font_cache: &FontCache) {
    // Shaping scope — one built face per (font, line) for the re-measures below.
    let faces = FaceCache::new();
    let content_left = line.content_left;
    let content_right = line.content_right;
    for seg in &mut line.segments {
        let old_total: f64 = seg.items.iter().map(|i| i.width).sum();
        let mut changed = false;
        for item in &mut seg.items {
            let new_text = match item.field {
                Some(FieldCode::Page) => Some(page_num.to_string()),
                Some(FieldCode::NumPages) => Some(total.to_string()),
                None => None,
            };
            if let Some(nt) = new_text {
                if nt != item.text {
                    // A wider result (e.g. "11" vs the cached "1") must update the
                    // item width, or following items (the " | Page" separator) get
                    // positioned at the stale one-digit advance and overlap it.
                    let (w, cw, shaped) = measure_item_text(&nt, &item.style, font_cache, &faces);
                    item.text = nt;
                    item.width = w;
                    item.content_width = cw;
                    item.shaped = shaped;
                    changed = true;
                }
            }
        }
        if changed {
            // Re-align the segment for the width delta so a right-aligned (or
            // centered) footer keeps its edge instead of overflowing the margin.
            let delta = seg.items.iter().map(|i| i.width).sum::<f64>() - old_total;
            if delta.abs() > 0.001 {
                let old_right = seg.x_start + old_total;
                let near = |a: f64, b: f64| (a - b).abs() < 1.0;
                if near(old_right, content_right) {
                    seg.x_start -= delta; // right-aligned
                } else if near(seg.x_start + old_total / 2.0, (content_left + content_right) / 2.0) {
                    seg.x_start -= delta / 2.0; // centered
                }
            }
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

// Positional orchestrator args; struct-ification deferred to a dedicated pass.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_text_box(
    raw: &CTTxbxContent,
    width_px: f64,
    font_cache: &FontCache,
    style_ctx: &StyleContext,
    default_ppr: Option<&ooxml_wml::types::CTPPrGeneral>,
    default_tab_stop_px: f64,
    theme_fonts: &ThemeFonts,
    numbering_levels: &HashMap<(i64, i64), NumberingLevelDef>,
) -> Vec<PagedLine> {
    let mut result = Vec::new();
    let mut y = 0.0_f64;
    let mut prev_space_after = 0.0_f64;
    let mut list_counters: HashMap<(i64, i64), i64> = HashMap::new();
    let inset = emu_to_px(91440); // ~0.1" default inset

    let inner_width = (width_px - inset * 2.0).max(10.0);

    for block in &raw.block_content {
        if let BlockContent::P(p) = block {
            let marker = resolve_numbering_marker(
                p, numbering_levels, &mut list_counters, style_ctx,
            );
            let cache = match layout_paragraph(
                font_cache, p, inner_width, marker.as_ref(),
                style_ctx, default_ppr, default_tab_stop_px,
                theme_fonts, None, &[], &[],
                None,
                false,
                None, // R28 — textbox interior is a non-captured stream (R22)
            ) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let gap = prev_space_after.max(cache.layout.space_before_px);
            if y > 0.0 { y += gap; }

            for line in &cache.lines {
                let lh = cache.layout.line_height(
                    line.max_font_pt, line.max_ascent + line.max_descent, line.max_image_height,
                );
                let baseline = y + line.max_ascent;

                let x_start = inset + cache.layout.indent_left_px;

                result.push(PagedLine {
                    segments: PagedLine::single(line.items.clone(), x_start),
                    // Textbox interior: para_id is None → sentinel (non-captured, R22).
                    para: cache.para_id.unwrap_or(PARA_SENTINEL),
                    baseline_y: baseline,
                    para_shading: cache.layout.shading_color.clone(),
                    line_height: lh,
                    content_left: inset,
                    content_right: width_px - inset,
                    border_top: None,
                    border_bottom: None,
                    border_left: None,
                    border_right: None,
                    bar_tabs: cache.tab_stops.bar_xs(
                        inset + cache.layout.indent_left_px,
                        width_px - inset - cache.layout.indent_right_px,
                        cache.layout.base_rtl,
                    ),
                    is_first_line: false,
                    is_last_line: false,
                });
                y += lh;
            }
            prev_space_after = cache.layout.space_after_px;
        }
    }
    result
}

pub(crate) fn render_text_box(
    p: &dyn Painter,
    tb: &PositionedTextBox,
    bitmaps: &HashMap<String, ImageBitmap>,
    charts: &HashMap<String, Vec<u8>>,
    ul_trail_space: bool,
) -> Result<(), JsValue> {
    p.save();

    // Fill
    if let Some(ref color) = tb.fill_color {
        p.set_fill_style_str(color);
        p.fill_rect(tb.x, tb.y, tb.width_px, tb.height_px);
    }

    // Clip content to box bounds
    p.save();
    p.begin_path();
    p.rect(tb.x, tb.y, tb.width_px, tb.height_px);
    p.clip();

    // Content
    for line in &tb.lines {
        let offset_line = PagedLine {
            para: line.para, // clone-from-line preserves the source line's para.
            segments: line.segments.iter().map(|s| LineSegment {
                x_start: tb.x + s.x_start,
                items: s.items.clone(),
            }).collect(),
            baseline_y: tb.y + line.baseline_y,
            para_shading: line.para_shading.clone(),
            line_height: line.line_height,
            content_left: tb.x + line.content_left,
            content_right: tb.x + line.content_right,
            border_top: None,
            border_bottom: None,
            border_left: None,
            border_right: None,
            bar_tabs: line.bar_tabs.iter().map(|&x| tb.x + x).collect(),
            is_first_line: false,
            is_last_line: false,
        };
        render_line(p, &offset_line, bitmaps, charts, ul_trail_space)?;
    }
    p.restore(); // remove clip

    // Border (drawn outside clip so it's crisp on the edge)
    if let Some(ref color) = tb.border_color {
        if tb.border_width_px > 0.0 {
            p.set_stroke_style_str(color);
            p.set_line_width(tb.border_width_px);
            p.stroke_rect(tb.x, tb.y, tb.width_px, tb.height_px);
        }
    }

    p.restore();
    Ok(())
}

/// Paint one positioned table row (§17.4): cell fills, then per-cell borders,
/// then clipped cell text lines. Shared by the body content loop and the
/// header/footer band (§17.10 header/footer tables).
pub(crate) fn render_table_row(
    p: &dyn Painter,
    row: &PagedTableRow,
    bitmaps: &HashMap<String, ImageBitmap>,
    charts: &HashMap<String, Vec<u8>>,
    ul_trail_space: bool,
) -> Result<(), JsValue> {
    for cell in &row.cells {
        if cell.v_merged_away { continue; }
        if let Some(fill) = &cell.fill_color {
            p.set_fill_style_str(fill);
            p.fill_rect(cell.x, row.y_top, cell.width_px, row.height_px);
        }
    }
    for cell in &row.cells {
        if cell.v_merged_away { continue; }
        // Cell borders stroke corner-to-corner (no device-pixel snap), in the
        // order top, bottom, left, right (unchanged from the pre-shared loop).
        crate::engine::border::stroke_box_edges(
            cell.x, row.y_top, cell.width_px, row.height_px,
            [
                (crate::engine::border::EdgeSide::Top, cell.border_top.as_ref()),
                (crate::engine::border::EdgeSide::Bottom, cell.border_bottom.as_ref()),
                (crate::engine::border::EdgeSide::Left, cell.border_left.as_ref()),
                (crate::engine::border::EdgeSide::Right, cell.border_right.as_ref()),
            ],
            false,
            |b: &crate::engine::border::Edge, x0, y0, x1, y1| {
                p.set_stroke_style_str(&b.color);
                p.set_line_width(b.width_px);
                p.begin_path(); p.move_to(x0, y0); p.line_to(x1, y1); p.stroke();
            },
        );
    }
    p.set_text_baseline("alphabetic");
    for cell in &row.cells {
        p.save();
        p.begin_path();
        p.rect(cell.x, row.y_top, cell.width_px, row.height_px);
        p.clip();
        for line in &cell.lines {
            render_line(p, line, bitmaps, charts, ul_trail_space)?;
        }
        p.restore();
    }
    Ok(())
}

pub(crate) fn render_line(
    p: &dyn Painter,
    line: &PagedLine,
    bitmaps: &HashMap<String, ImageBitmap>,
    charts: &HashMap<String, Vec<u8>>,
    ul_trail_space: bool,
) -> Result<(), JsValue> {
    let bdr_top = line.baseline_y - line.line_height + (line.line_height * 0.15);
    let bdr_width = line.content_right - line.content_left;

    if let Some(ref shading) = line.para_shading {
        p.set_fill_style_str(shading);
        p.fill_rect(line.content_left, bdr_top, bdr_width, line.line_height);
    }

    if line.is_first_line {
        if let Some(ref b) = line.border_top {
            p.set_stroke_style_str(&b.color);
            p.set_line_width(b.width_px);
            let y = bdr_top - b.space_px;
            p.begin_path(); p.move_to(line.content_left, y); p.line_to(line.content_right, y); p.stroke();
        }
    }
    if line.is_last_line {
        if let Some(ref b) = line.border_bottom {
            p.set_stroke_style_str(&b.color);
            p.set_line_width(b.width_px);
            let y = bdr_top + line.line_height + b.space_px;
            p.begin_path(); p.move_to(line.content_left, y); p.line_to(line.content_right, y); p.stroke();
        }
    }
    if let Some(ref b) = line.border_left {
        p.set_stroke_style_str(&b.color);
        p.set_line_width(b.width_px);
        let x = line.content_left - b.space_px;
        p.begin_path(); p.move_to(x, bdr_top); p.line_to(x, bdr_top + line.line_height); p.stroke();
    }
    if let Some(ref b) = line.border_right {
        p.set_stroke_style_str(&b.color);
        p.set_line_width(b.width_px);
        let x = line.content_right + b.space_px;
        p.begin_path(); p.move_to(x, bdr_top); p.line_to(x, bdr_top + line.line_height); p.stroke();
    }

    // §17.18.84 bar tab: a vertical rule at the stop position, spanning the
    // line box of every line of the paragraph. Purely decorative — never a
    // text-advance stop (excluded from `ResolvedTabStops::custom`). Auto/black
    // hairline, same vertical extent idiom as the paragraph side borders.
    for &x in &line.bar_tabs {
        p.set_stroke_style_str(DEFAULT_TEXT_COLOR);
        p.set_line_width(1.0);
        p.begin_path(); p.move_to(x, bdr_top); p.line_to(x, bdr_top + line.line_height); p.stroke();
    }

    for seg in &line.segments {
      let mut x = seg.x_start;
      for item in &seg.items {
        if let Some(m) = &item.math {
            // OMML math atom: paint the flattened box at the item's
            // origin on the text baseline; the atom's own ascent/descent already
            // grew the line box (LayoutLine::push).
            crate::wml::math::render_math_box(p, m, x, line.baseline_y)?;
            x += item.width;
            continue;
        }
        if let Some(ref tb) = item.text_box {
            let top = line.baseline_y - line.line_height;
            let ptb = PositionedTextBox {
                lines: tb.lines.clone(),
                x,
                y: top,
                width_px: tb.width_px,
                height_px: tb.height_px,
                behind_text: false,
                fill_color: tb.fill_color.clone(),
                border_color: tb.border_color.clone(),
                border_width_px: tb.border_width_px,
            };
            render_text_box(p, &ptb, bitmaps, charts, ul_trail_space)?;
            x += item.width;
            continue;
        }
        if let Some(img) = &item.image {
            // §20.4.2.8 text-distance padding: the item box is widened by
            // distL+distR and the line raised by distT+distB. Draw the bitmap
            // inset distL from the box's left edge, at its intrinsic width
            // (box width minus the L/R padding), and lift it distB above the
            // baseline so distB of clear space sits below it (distT clears the
            // line above via the grown ascent).
            let (img_x, top, img_w, _img_h) = img.painted_box(x, item.width, line.baseline_y);
            // EMF dispatch: an `image/x-emf` rel no longer inlines the vector
            // program (a ~5900-record file re-emitted 1000+ ops per paint /
            // per recorded display list). It draws ONE image op under the
            // synthetic `emf-pic:{rel}` key and logs the drawn footprint; the
            // TS adapter rasterizes off the hot path and registers the bitmap
            // under the same key (miss ⇒ the standard grey placeholder, the
            // async-image first-paint behavior). `is_active` is false for
            // every non-EMF rel, so the chart/bitmap fallback is unchanged.
            // (Feature-off: this whole cfg branch is absent and the `not(emf)`
            // block below is byte-identical to today.)
            #[cfg(feature = "emf")]
            {
                if crate::emf::is_active(&img.rel_id) {
                    crate::emf::note_request(&img.rel_id, img_w, img.height_px);
                    p.draw_image(&crate::emf::pic_key(&img.rel_id), img_x, top, img_w, img.height_px)?;
                } else if let Some(chart_bytes) = charts.get(&img.rel_id) {
                    if !chart::render_chart(chart_bytes, p, img_x, top, img_w, img.height_px) {
                        draw_image_placeholder(p, img_x, top, img_w, img.height_px)?;
                    }
                } else {
                    p.draw_image(&img.rel_id, img_x, top, img_w, img.height_px)?;
                }
            }
            #[cfg(not(feature = "emf"))]
            if let Some(chart_bytes) = charts.get(&img.rel_id) {
                if !chart::render_chart(chart_bytes, p, img_x, top, img_w, img.height_px) {
                    draw_image_placeholder(p, img_x, top, img_w, img.height_px)?;
                }
            } else {
                p.draw_image(&img.rel_id, img_x, top, img_w, img.height_px)?;
            }
        } else {
            let bg = item.style.highlight.as_ref().or(item.style.run_shading.as_ref());
            if let Some(bg_color) = bg {
                p.set_fill_style_str(bg_color);
                let font_px = pt_to_px(item.style.font_pt);
                let hl_top = line.baseline_y - font_px * 0.85;
                let hl_height = font_px * 1.15;
                p.fill_rect(x, hl_top, item.content_width, hl_height);
            }

            p.set_font(&item.style.font_css());
            p.set_fill_style_str(&item.style.color);

            let text_y = match item.style.vert_align {
                VertAlign::Superscript => line.baseline_y - pt_to_px(item.style.font_pt) * 0.5,
                VertAlign::Subscript => line.baseline_y + pt_to_px(item.style.font_pt) * 0.25,
                VertAlign::Baseline => line.baseline_y,
            } - item.style.baseline_shift_px;

            match &item.shaped {
                Some(shaped) if shaped.is_simple => {
                    let visible = item.visible_text();
                    if !visible.is_empty() {
                        p.paint_glyph_run(&item.style, &visible, x, text_y)?;
                    }
                }
                Some(shaped) => {
                    let mut gx = x;
                    let mut cluster_start = 0;
                    while cluster_start < shaped.glyphs.len() {
                        let cluster_byte = shaped.glyphs[cluster_start].cluster as usize;
                        let mut cluster_end = cluster_start + 1;
                        while cluster_end < shaped.glyphs.len()
                            && shaped.glyphs[cluster_end].cluster == shaped.glyphs[cluster_start].cluster
                        {
                            cluster_end += 1;
                        }
                        let next_byte = if cluster_end < shaped.glyphs.len() {
                            shaped.glyphs[cluster_end].cluster as usize
                        } else {
                            shaped.text.len()
                        };
                        if cluster_byte < shaped.text.len() && next_byte <= shaped.text.len() {
                            let cluster_text: String = shaped.text[cluster_byte..next_byte]
                                .chars()
                                .filter(|c| !is_layout_control(*c))
                                .collect();

                            if !cluster_text.is_empty() {
                                let glyph = &shaped.glyphs[cluster_start];
                                p.paint_glyph_run(
                                    &item.style,
                                    &cluster_text,
                                    gx + glyph.x_offset,
                                    text_y - glyph.y_offset,
                                )?;
                            }
                        }
                        for gi in cluster_start..cluster_end {
                            gx += shaped.glyphs[gi].x_advance;
                        }
                        cluster_start = cluster_end;
                    }
                }
                None => {
                    let visible = item.visible_text();
                    if !visible.is_empty() {
                        p.paint_glyph_run(&item.style, &visible, x, text_y)?;
                    }
                }
            }
        }
        // Decoration extent rules: see `decoration_width` (trailing-whitespace
        // suppression + §17.9.24 decorated-marker lvlText-only scope).
        let is_line_final = std::ptr::eq(seg, line.segments.last().unwrap())
            && std::ptr::eq(item, seg.items.last().unwrap());
        let deco_w = decoration_width(item, is_line_final, ul_trail_space);

        // Draw underline
        if item.style.underline != Underline::None && item.image.is_none() && item.tab.is_none() {
            p.set_stroke_style_str(&item.style.color);
            let font_px = pt_to_px(item.style.font_pt);
            let underline_y = line.baseline_y + font_px * 0.15;
            let line_w = match item.style.underline {
                Underline::Thick => 1.5,
                Underline::Double => 0.7,
                _ => 1.0,
            };
            p.set_line_width(line_w);
            p.begin_path();
            p.move_to(x, underline_y);
            p.line_to(x + deco_w, underline_y);
            p.stroke();
            if item.style.underline == Underline::Double {
                let y2 = underline_y + 2.0;
                p.begin_path();
                p.move_to(x, y2);
                p.line_to(x + deco_w, y2);
                p.stroke();
            }
        }

        // Draw strikethrough
        if (item.style.strikethrough || item.style.double_strike) && item.image.is_none() && item.tab.is_none() {
            p.set_stroke_style_str(&item.style.color);
            let font_px = pt_to_px(item.style.font_pt);
            let strike_y = line.baseline_y - font_px * 0.3;
            p.set_line_width(1.0);
            p.begin_path();
            p.move_to(x, strike_y);
            p.line_to(x + deco_w, strike_y);
            p.stroke();
            if item.style.double_strike {
                let y2 = strike_y + 2.0;
                p.begin_path();
                p.move_to(x, y2);
                p.line_to(x + deco_w, y2);
                p.stroke();
            }
        }

        if let Some(ref tab) = item.tab {
            if tab.leader != TabLeader::None && item.width > 0.0 {
                let leader_ch = match tab.leader {
                    TabLeader::Dot => ".",
                    TabLeader::Hyphen => "-",
                    TabLeader::Underscore => "_",
                    TabLeader::MiddleDot => "\u{00B7}",
                    TabLeader::Heavy => "\u{2500}",
                    TabLeader::None => "",
                };
                if !leader_ch.is_empty() {
                    p.set_font(&item.style.font_css());
                    p.set_fill_style_str(&item.style.color);
                    let count = tab_leader_count(item.width, item.style.font_pt);
                    if count > 0 {
                        let leader_str: String = leader_ch.repeat(count);
                        p.fill_text(&leader_str, x, line.baseline_y)?;
                    }
                }
            }
        }
        x += item.width;
      }
    }
    Ok(())
}

/// Horizontal extent of an item's text decorations (underline/strike).
/// Line-final items rule over `content_width` so the rule never runs under
/// end-of-line trailing whitespace (Word behavior; `ul_trail_space`
/// §17.15.3.8 restores full width). Synthesized zero-length-span items
/// (numbering markers — §17.9.24: the level rPr formats the lvlText only,
/// never the suff gap or the hanging-slot pad) also rule over
/// `content_width` MID-line. Every other sentinel item construction sets
/// `content_width == width`, so the sentinel arm only diverges for a
/// decorated marker.
pub(crate) fn decoration_width(item: &Item, is_line_final: bool, ul_trail_space: bool) -> f64 {
    if (is_line_final && !ul_trail_space)
        || (item.char_start == CHAR_START_SENTINEL && item.char_len == 0)
    {
        item.content_width
    } else {
        item.width
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunStyle;

    /// Minimal text item: `width` 30 with `content_width` 24 (as if the last
    /// 6px were trailing whitespace / the marker's suffix gap + slot pad).
    fn item(char_start: u32, char_len: u32) -> Item {
        Item {
            text: "1.".into(),
            style: RunStyle::marker(),
            width: 30.0,
            content_width: 24.0,
            char_start,
            char_len,
            ..Default::default()
        }
    }

    /// The §17.9.24 decorated-marker branch: a sentinel (marker) item rules
    /// over `content_width` even MID-line, so a level-rPr underline never
    /// runs across the suff gap or the hanging-slot pad. This is the branch
    /// the layout-level assertions cannot reach (native tests do not paint).
    #[test]
    fn decoration_width_sentinel_marker_clamps_mid_line() {
        let marker = item(CHAR_START_SENTINEL, 0);
        assert_eq!(decoration_width(&marker, false, false), 24.0);
        assert_eq!(decoration_width(&marker, false, true), 24.0);
        // Line-final too — the sentinel arm dominates.
        assert_eq!(decoration_width(&marker, true, false), 24.0);
        assert_eq!(decoration_width(&marker, true, true), 24.0);
    }

    /// Regular text items keep the pre-existing rules: full width mid-line
    /// (continuous underline across inter-word spaces), `content_width` when
    /// line-final (no rule under trailing whitespace) unless §17.15.3.8
    /// `ulTrailSpace` restores it.
    #[test]
    fn decoration_width_regular_item_rules() {
        let text = item(0, 2);
        assert_eq!(decoration_width(&text, false, false), 30.0);
        assert_eq!(decoration_width(&text, true, false), 24.0);
        assert_eq!(decoration_width(&text, true, true), 30.0);
    }

    /// A sentinel with a nonzero span (defensive: no such item exists today)
    /// is NOT treated as a marker — both sentinel conditions must hold.
    #[test]
    fn decoration_width_requires_zero_len_sentinel() {
        let odd = item(CHAR_START_SENTINEL, 2);
        assert_eq!(decoration_width(&odd, false, false), 30.0);
    }

    /// `<w:sz w:val="0"/>` on a run carrying a leader tab (§17.3.1.37), a
    /// verified crash vector. §17.3.2.38 requires a POSITIVE half-point
    /// measurement, but nothing rejects zero on the way in: `item.width / 0.0`
    /// was `+inf`, the `as usize` cast saturated to `usize::MAX`, and
    /// `leader_ch.repeat(count)` aborted the process on the PAINT path (which the
    /// open-the-document probe never reaches, which is why this one hid). A glyph
    /// with no advance never reaches the end of the tab, so no count fills it.
    #[test]
    fn tab_leader_count_is_zero_when_the_glyph_has_no_advance() {
        assert_eq!(tab_leader_count(500.0, 0.0), 0);
        assert_eq!(tab_leader_count(500.0, -12.0), 0);
        // A tab with no width has nothing to fill either.
        assert_eq!(tab_leader_count(0.0, 11.0), 0);
        // An infinite tab width refuses instead of saturating the cast.
        assert_eq!(tab_leader_count(f64::INFINITY, 11.0), 0);
        assert_eq!(tab_leader_count(f64::NAN, 11.0), 0);
    }

    /// Ordinary leaders are untouched — the one-glyph-per-pixel geometric bound
    /// cannot bind while the glyph advance is ≥ 1 px (any font above ~2.1 pt),
    /// because `floor(width / char_width) ≤ width` there already.
    #[test]
    fn tab_leader_count_leaves_real_leaders_exactly_as_they_were() {
        // A 9000-twip (450px) dotted TOC leader in an 11pt run:
        // char_width = 11 * 96/72 * 0.35 = 5.1333px → 87 dots, as before.
        assert_eq!(tab_leader_count(450.0, 11.0), 87);
        for &(w, pt) in &[(450.0, 11.0), (96.0, 12.0), (2112.0, 8.0), (33.0, 24.0)] {
            let unbounded = (w / (pt_to_px(pt) * 0.35)).floor() as usize;
            assert_eq!(tab_leader_count(w, pt), unbounded, "width={w} pt={pt}");
        }
    }

    /// A sub-pixel glyph advance (a ~0.5pt run, schema-valid under
    /// ST_HpsMeasure) is where the geometric bound bites: the count is the tab's
    /// own width in pixels, so the allocation depends on the tab, never on how
    /// small the font is.
    #[test]
    fn tab_leader_count_is_bounded_by_the_tab_it_paints_into() {
        assert_eq!(tab_leader_count(624.0, 0.5), 624);
        assert_eq!(tab_leader_count(624.0, 0.001), 624);
        // …and the bound is the tab width, not a fixed constant.
        assert_eq!(tab_leader_count(80.0, 0.001), 80);
    }
}
