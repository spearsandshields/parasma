//! `SheetPage` → canvas paint walk.
//!
//! Draws one paginated [`super::grid::SheetPage`] onto a
//! `CanvasRenderingContext2d` through the shared [`crate::engine::paint`] /
//! [`crate::engine::fonts`] seam: gridlines, the cell-box rails, and cell text
//! with §18 *general* alignment (text left / number right / boolean+error
//! centered). This is the one web-sys-touching SML file; everything it consumes
//! (geometry + text + alignment) was produced web-sys-free in [`super::grid`].
//!
//! Spec grounding (ECMA-376 §18):
//!  - §18.3.1.88  `showGridLines` — the conventional gridline color is a
//!    deliberate SIMPLIFICATION (the file may theme it).
//!  - §18 general alignment — the per-cell `HAlign` already encodes it; the
//!    painter just honors the horizontal placement + a small inset.

use std::collections::HashMap;

use wasm_bindgen::JsValue;
use web_sys::ImageBitmap;

use crate::engine::fonts::{self, FaceCache, FontCache, DEFAULT_FONT_PT};
use crate::engine::paint::{Painter, RunStyle, Underline, VertAlign};

use super::grid::{col_letters, gutter_dims, PageCell, SheetPage};
use super::model::{CellStyle, HAlign, DEFAULT_NORMAL_FONT};

/// Conventional Excel gridline gray (§18.3.1.88). SIMPLIFICATION: the file
/// may theme the gridline color (`sheetView@colorId` → palette); we paint the
/// canonical light gray.
const GRIDLINE_COLOR: &str = "#d4d4d4";

/// Header gutter (A/B/C column letters + 1/2/3 row numbers) fill — the Excel
/// light-gray heading band. The separators reuse [`GRIDLINE_COLOR`].
const GUTTER_FILL: &str = "#f0f0f0";

/// Header gutter label text color — near-black, like Excel's heading glyphs.
const GUTTER_TEXT_COLOR: &str = "#3c4043";

/// Header gutter label font size (px). Fixed small heading text.
const GUTTER_FONT_PX: f64 = 11.0;

/// Hidden-range boundary marker (§18.3.1.13/§18.3.1.73 hidden cols/rows): the
/// slightly HEAVIER rule Excel draws in the header gutter where a hidden range
/// sits. Purely geometry-driven — a zero-size rail span exists ONLY when a
/// hidden column/row collapsed it, so hidden-less sheets emit NOTHING
/// (byte-identity; distinct from the freeze line's `#8a8a8a`).
const HIDDEN_MARKER_COLOR: &str = "#9b9b9b";
const HIDDEN_MARKER_WIDTH_PX: f64 = 2.0;

/// Content-space positions of hidden-range boundaries over a rails vec: the
/// collapsed rail where one or more consecutive zero-size spans sit (deduped —
/// a multi-column/row hidden range yields ONE mark).
fn hidden_boundary_marks(rails: &[f64]) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    for k in 0..rails.len().saturating_sub(1) {
        if rails[k + 1] <= rails[k] && out.last() != Some(&rails[k]) {
            out.push(rails[k]);
        }
    }
    out
}

/// Default cell text color (§18 default): near-black, used when a cell's
/// resolved style (`cellXfs` → `xf` → `font@color`) carries no colour.
const CELL_TEXT_COLOR: &str = "#1f2328";

/// Horizontal inset of cell text from its box edge, in px. Excel insets text a
/// few px from the gridline; 3px matches the §18.3.1.13 padding intuition and
/// keeps text off the rail. SIMPLIFICATION: a fixed inset, not the exact
/// `cellXfs@indent`.
const CELL_TEXT_INSET_PX: f64 = 3.0;

/// White page background so the print-page reads like paper (matches the WML
/// `render_page` prologue).
const PAGE_BG: &str = "#ffffff";

/// Search-highlight descriptor for the SML page painter (full-text
/// search): paints a translucent `fill` rect (at `alpha`) over every cell on the
/// page whose text matches `needle`/`opts`. The single ACTIVE match (the cell
/// next/prev focuses) gets the hotter `active_fill`/`active` emphasis, mirroring
/// WML. The active cell is identified by its A1 ref (`active_ref`), which is
/// invariant under the grid's scroll-windowing and merge-covered suppression —
/// unlike a bare enumeration index, which would mis-emphasise after any vertical
/// scroll in the continuous grid view (the default SML view) or when an earlier
/// cell on the page is merge-covered. `active_ref` is `Some` ONLY when the active
/// cell lives on the page being painted (the caller narrows by flat-page index,
/// mirroring WML's `p == idx` page check).
pub(crate) struct SearchHighlight<'a> {
    pub needle: &'a str,
    pub opts: crate::SearchOpts,
    pub fill: &'a str,
    pub alpha: f64,
    /// Alpha for the focused match (`SEARCH_ACTIVE_ALPHA`).
    pub active: f64,
    /// Fill for the focused match (`SEARCH_ACTIVE_COLOR`).
    pub active_fill: &'a str,
    /// A1 ref of the focused cell, if it lives on THIS page; else `None`.
    pub active_ref: Option<&'a str>,
}

/// Paint one sheet page onto `p`. `origin_x`/`origin_y` is the page-local
/// top-left where the grid content begins (the WML body uses page margins; the
/// SML painter pins the grid to the top-left content origin the caller passes).
///
/// `page_w`/`page_h` is the full print-page box (used only for the background
/// fill). `normal_font` is the resolved normal-style family for cell text.
/// `bitmaps` is the shared decoded-image map for anchored pictures.
/// `charts` is the Viewer's shared chart-XML map (embedded charts; keyed by
/// the sheet-namespaced chart key) — fed to `crate::chart::render_chart`.
/// `search` (if any) paints a highlight over matching cells (full-text search).
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_sheet_page(
    p: &dyn Painter,
    page: &SheetPage,
    origin_x: f64,
    origin_y: f64,
    page_w: f64,
    page_h: f64,
    normal_font: &str,
    font_cache: &FontCache,
    bitmaps: &HashMap<String, ImageBitmap>,
    charts: &HashMap<String, Vec<u8>>,
    search: Option<&SearchHighlight>,
) -> Result<(), JsValue> {
    // Full-page paint = the windowed paint with NO window (`clip = None`): every
    // cell/merge/image is kept, so the draw sequence is byte-identical.
    paint_sheet_page_clipped(
        p, page, origin_x, origin_y, page_w, page_h, normal_font, font_cache, bitmaps, charts,
        None, search,
    )
}

/// Shared print-page paint, threaded by `clip`. `None` = full page
/// (byte-identical to the original `paint_sheet_page`); `Some(win)` = windowed.
///
/// Cells are culled by ROW BAND (whole rows whose y-band intersects the window
/// survive), NOT per-cell — matching the proven `paint_grid_viewport` template:
/// keeping whole rows preserves the per-row spill/clip occupancy `paint_cells`
/// computes (a partial row would break Excel overflow semantics). Merges and
/// anchored images cull by their own boxes. Gridlines/gutters are O(rails over
/// the page's used range) chrome → painted unconditionally (the rail count is
/// bounded by the print page, not the whole sheet). The window arrives in
/// PAGE-LOGICAL px (canvas space); it is shifted into grid-content space by the
/// same gutter origin shift the cells use.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_sheet_page_clipped(
    p: &dyn Painter,
    page: &SheetPage,
    origin_x: f64,
    origin_y: f64,
    page_w: f64,
    page_h: f64,
    normal_font: &str,
    font_cache: &FontCache,
    bitmaps: &HashMap<String, ImageBitmap>,
    charts: &HashMap<String, Vec<u8>>,
    clip: Option<&crate::cull::Rect>,
    search: Option<&SearchHighlight>,
) -> Result<(), JsValue> {
    // ── Page background (paper) ──────────────────────────────────────────────
    // Reduced to the window ∩ page box under a windowed paint (`cull::bg_rect`) so
    // the ONE page-spanning fill stays device-bounded at high zoom (offset alone
    // leaves it page-sized and still overflows Skia). `clip == None` → full page.
    p.set_fill_style_str(PAGE_BG);
    if let Some((bx, by, bw, bh)) = crate::cull::bg_rect(clip, page_w, page_h) {
        p.fill_rect(bx, by, bw, bh);
    }

    // ── Header gutters: A/B/C column letters + 1/2/3 row numbers ─────────────
    // The gutters live INSIDE the page content box as an ORIGIN SHIFT: the grid
    // content begins at (margin + gutter), not the bare margin. `origin_x`/
    // `origin_y` as passed are the MARGIN origin (where the gutter bands start);
    // the grid below shifts right+down by the gutter widths. cellAt applies the
    // SAME shift via the shared `gutter_dims` so hit-testing stays aligned.
    let (row_gutter_w, col_gutter_h) = gutter_dims(page);
    let grid_origin_x = origin_x + row_gutter_w;
    let grid_origin_y = origin_y + col_gutter_h;
    paint_gutters(p, page, origin_x, origin_y, row_gutter_w, col_gutter_h);
    // From here on the grid paints at the gutter-shifted origin.
    let origin_x = grid_origin_x;
    let origin_y = grid_origin_y;

    // Cull helper in GRID-CONTENT space: a box at content (cx,cy,cw,ch)
    // paints at (origin + c), so it is visible iff that placed box intersects
    // the window. `clip == None` ⇒ always visible (full-page identity).
    let visible = |cx: f64, cy: f64, cw: f64, ch: f64| -> bool {
        crate::cull::keep(
            clip,
            origin_x + cx,
            origin_y + cy,
            origin_x + cx + cw,
            origin_y + cy + ch,
        )
    };

    // ── Gridlines (§18.3.1.88) ───────────────────────────────────────────────
    // Drawn first, behind cell text. Only across the content extent (the used
    // range), so a sparse sheet shows its grid without flooding the whole page.
    if page.show_grid_lines {
        p.set_stroke_style_str(GRIDLINE_COLOR);
        p.set_line_width(1.0);
        // Vertical rails: each col_rail x, from the top of the content to the
        // bottom of the content (the last row rail).
        let bottom = origin_y + page.content_h;
        for rx in &page.col_rails {
            // +0.5 aligns the 1px stroke to the device pixel grid (crisp lines).
            let x = (origin_x + rx).round() + 0.5;
            p.begin_path();
            p.move_to(x, origin_y.round() + 0.5);
            p.line_to(x, bottom.round() + 0.5);
            p.stroke();
        }
        // Horizontal rails: each row_rail y, across the content width.
        let right = origin_x + page.content_w;
        for ry in &page.row_rails {
            let y = (origin_y + ry).round() + 0.5;
            p.begin_path();
            p.move_to(origin_x.round() + 0.5, y);
            p.line_to(right.round() + 0.5, y);
            p.stroke();
        }

        // ── Merged-region rail suppression (§18.3.1.55) ──────────────────────
        // A merged region renders as ONE box: its interior gridline rails are
        // suppressed. White out each merge box interior (page paper), then
        // redraw only its OUTER perimeter so the region keeps a clean border on
        // the surrounding grid. (Fills/borders/text paint over this next.)
        for m in &page.merges {
            if !visible(m.x, m.y, m.w, m.h) {
                continue;
            }
            let bx = origin_x + m.x;
            let by = origin_y + m.y;
            // Erase the interior rails: inset 1px so the perimeter survives, then
            // repaint the perimeter as a single rectangle of gridline.
            p.set_fill_style_str(PAGE_BG);
            p.fill_rect(bx + 1.0, by + 1.0, (m.w - 2.0).max(0.0), (m.h - 2.0).max(0.0));
            p.set_stroke_style_str(GRIDLINE_COLOR);
            p.set_line_width(1.0);
            p.begin_path();
            p.rect(bx.round() + 0.5, by.round() + 0.5, m.w.round() - 1.0, m.h.round() - 1.0);
            p.stroke();
        }
    }

    // Fills (§18.8.20) over gridlines, borders (§18.8.4) over fills, then cell
    // text with §18 overflow semantics — shared with the grid-viewport path.
    //
    // Cull by ROW BAND (not per-cell): keep WHOLE rows whose y-band
    // intersects the window, so `paint_cells`'s per-row spill/clip occupancy
    // stays exact (a half-row would mis-compute Excel overflow neighbours). The
    // x-extent is left intact within a kept row — matching `paint_grid_viewport`.
    // `clip == None` ⇒ `windowed` is `&page.cells` borrowed verbatim (identity).
    let windowed: Vec<PageCell>;
    let cells: &[PageCell] = match clip {
        None => &page.cells,
        Some(_) => {
            windowed = page
                .cells
                .iter()
                .filter(|c| visible(0.0, c.y, page.content_w, c.h))
                .cloned()
                .collect();
            &windowed
        }
    };
    // Search highlights are painted inside paint_cells (shared print + grid).
    paint_cells(p, page, origin_x, origin_y, cells, normal_font, font_cache, search)?;

    // ── Anchored pictures — Excel's drawing layer floats ABOVE the grid ──────
    // Clipped to the page content rect: a picture spanning a page break keeps
    // its full box on every page it intersects, so the clip shows exactly the
    // slice belonging to this page. The image LIST is culled per-box;
    // the per-image content clip is unchanged.
    let images_windowed: Vec<super::grid::PageImage>;
    let images: &[super::grid::PageImage] = match clip {
        None => &page.images,
        Some(_) => {
            images_windowed = page
                .images
                .iter()
                .filter(|im| visible(im.x, im.y, im.w, im.h))
                .cloned()
                .collect();
            &images_windowed
        }
    };
    paint_page_images_slice(
        p, images, origin_x, origin_y, page.content_w, page.content_h, bitmaps,
    );

    // ── Anchored CHARTS (embedded charts) — the same drawing layer, rendered
    // via the SHARED `crate::chart::render_chart` at the anchored box, clipped
    // to the page content rect like the pictures. Chart-gated: a chart-less
    // page emits ZERO extra commands (byte-identical paint stream).
    let charts_windowed: Vec<super::grid::PageImage>;
    let page_charts: &[super::grid::PageImage] = match clip {
        None => &page.charts,
        Some(_) => {
            charts_windowed = page
                .charts
                .iter()
                .filter(|ch| visible(ch.x, ch.y, ch.w, ch.h))
                .cloned()
                .collect();
            &charts_windowed
        }
    };
    paint_page_charts_slice(
        p, page_charts, origin_x, origin_y, page.content_w, page.content_h, charts,
    );

    Ok(())
}

/// Paint a SLICE of a page's anchored CHARTS at the content origin, clipped to
/// the content window (the picture-slice contract, chart-painter routed). A
/// key missing from the chart-XML map (or an unsupported chart) paints nothing
/// — never a placeholder box over the grid.
fn paint_page_charts_slice(
    p: &dyn Painter,
    page_charts: &[super::grid::PageImage],
    origin_x: f64,
    origin_y: f64,
    clip_w: f64,
    clip_h: f64,
    charts: &HashMap<String, Vec<u8>>,
) {
    if page_charts.is_empty() {
        return;
    }
    p.save();
    p.begin_path();
    p.rect(origin_x, origin_y, clip_w, clip_h);
    p.clip();
    for ch in page_charts {
        if let Some(xml) = charts.get(&ch.key) {
            // White chart-area ground first (Excel's default chart fill —
            // gridlines must not show through the plot; the exact
            // `renderChartToCanvas` white-ground rationale).
            p.set_fill_style_str(PAGE_BG);
            p.fill_rect(origin_x + ch.x, origin_y + ch.y, ch.w, ch.h);
            let _ = crate::chart::render_chart(xml, p, origin_x + ch.x, origin_y + ch.y, ch.w, ch.h);
        }
    }
    p.restore();
}

/// Draw a page's anchored pictures at the given content origin, clipped to
/// the `clip_w`×`clip_h` content window. Decoded bitmaps draw scaled into their
/// anchor box; a not-yet-decoded (or undecodable) image draws the shared grey
/// placeholder (the WML/PML safety principle — never silently blank).
#[allow(dead_code)]
fn paint_page_images(
    p: &dyn Painter,
    page: &SheetPage,
    origin_x: f64,
    origin_y: f64,
    clip_w: f64,
    clip_h: f64,
    bitmaps: &HashMap<String, ImageBitmap>,
) {
    paint_page_images_slice(p, &page.images, origin_x, origin_y, clip_w, clip_h, bitmaps);
}

/// Paint a SLICE of a page's anchored pictures (the windowed image list,
/// or `&page.images` for the full page) at the content origin, clipped to the
/// content window. Same per-image content-clip behaviour as before.
fn paint_page_images_slice(
    p: &dyn Painter,
    images: &[super::grid::PageImage],
    origin_x: f64,
    origin_y: f64,
    clip_w: f64,
    clip_h: f64,
    _bitmaps: &HashMap<String, ImageBitmap>,
) {
    if images.is_empty() {
        return;
    }
    p.save();
    p.begin_path();
    p.rect(origin_x, origin_y, clip_w, clip_h);
    p.clip();
    for im in images {
        let (x, y) = (origin_x + im.x, origin_y + im.y);
        let _ = p.draw_image(&im.key, x, y, im.w, im.h);
    }
    p.restore();
}

/// Paint the cell layer of a sheet page — solid fills (§18.8.20) over the
/// gridlines, per-edge borders (§18.8.4) over the fills, then per-cell styled
/// text with Excel overflow/clip semantics (§18 general alignment). Shared by
/// the paged [`paint_sheet_page`] and the virtualised [`paint_grid_viewport`];
/// the only per-caller difference is the cell slice (`&page.cells` vs the
/// `grid::visible_cells` window) and the content origin.
///
/// `origin_x`/`origin_y` is the grid content origin (gutter-shifted for the
/// paged path, scroll-shifted for the viewport). `cells` is the slice to paint;
/// `page` still supplies `content_w` (the spill clip extent). Excel paint order
/// is fills → borders → text, so a filled cell hides the grid beneath it and a
/// border overpaints the fill edge.
#[allow(clippy::too_many_arguments)]
fn paint_cells(
    p: &dyn Painter,
    page: &SheetPage,
    origin_x: f64,
    origin_y: f64,
    cells: &[PageCell],
    normal_font: &str,
    font_cache: &FontCache,
    search: Option<&SearchHighlight>,
) -> Result<(), JsValue> {
    // One built face per (font, painted slice) instead of one (or two)
    // per cell measured.
    let faces = FaceCache::new();
    // ── Cell FILLS (§18.8.20) — paint OVER the gridlines ─────────────────────
    // Excel paint order: a filled cell hides the grid beneath it, so fills go
    // down after the gridlines and before the borders/text. Only cells whose
    // resolved style carries a solid fill contribute.
    for cell in cells {
        if let Some(fill) = cell.style.as_ref().and_then(|s| s.fill_hex.as_deref()) {
            p.set_fill_style_str(fill);
            p.fill_rect(origin_x + cell.x, origin_y + cell.y, cell.w, cell.h);
        }
    }

    // ── Cell BORDERS (§18.8.4) — paint OVER the fills ────────────────────────
    // Per-edge solid strokes on the cell box perimeter (a simplification
    // collapses dash/double styles to a solid stroke of the class width).
    for cell in cells {
        let Some(borders) = cell.style.as_ref().map(|s| &s.borders) else {
            continue;
        };
        paint_cell_borders(p, cell, origin_x, origin_y, borders);
    }

    // ── Cell text (§18 general alignment) ────────────────────────────────────
    // Excel overflow semantics: text wider than its cell spills over EMPTY
    // neighbors (left-aligned spills right, right-aligned left, centered both),
    // is CLIPPED at the first non-empty neighbor, and the gridline segments it
    // crosses are suppressed (we white-out the spill band under the text before
    // drawing, preserving the horizontal rails above/below).
    //
    // Text is per-cell styled (font/size/bold/italic/colour from the
    // resolved `CellStyle`). The shared normal-style `RunStyle` is the fallback
    // for cells with no style.
    let mut default_style = cell_text_style(normal_font);
    // Resolve to the LOADED family (alias → substitute, e.g. Calibri→Carlito):
    // the canvas FontFace is registered under the substitute name, and shaping
    // measures the same face — using the raw file name here would paint a system
    // fallback that doesn't match the measured advances (collisions/overlap).
    default_style.family = fonts::resolve_font_with_fallbacks(&default_style.family, font_cache);
    p.set_text_baseline("alphabetic");

    // Per-row occupancy of NON-empty cells (geometric: cells sharing a y form a
    // row band) — the spill/clip limits for every cell in that row.
    let mut rows: HashMap<u64, Vec<(f64, f64)>> = HashMap::new();
    for cell in cells {
        if !cell.text.is_empty() {
            rows.entry(cell.y.to_bits()).or_default().push((cell.x, cell.x + cell.w));
        }
    }
    for spans in rows.values_mut() {
        spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    }

    for cell in cells {
        if cell.text.is_empty() {
            continue;
        }
        // Per-cell RunStyle: the resolved CellStyle's font/size/bold/italic/
        // colour layered onto the normal-style default. A styled cell sets
        // the canvas font from its own metrics; the metrics font resolves the
        // cell's family for shaping (alignment) measurement.
        let mut style = cell_run_style(&default_style, cell.style.as_ref());
        // Same resolved-family rule for per-cell fonts (cell styles carry the
        // raw file family, e.g. "Calibri"): measure and paint one real face.
        style.family = fonts::resolve_font_with_fallbacks(&style.family, font_cache);
        let metrics_font = font_cache.resolve(&style.family, if style.bold { 700 } else { 400 }, style.italic);
        p.set_font(&font_css(&style));
        p.set_fill_style_str(&style.color);

        // Explicit alignment (§18.8.1) supersedes the value-kind general align.
        let align = cell
            .style
            .as_ref()
            .and_then(|s| s.h_align_override)
            .unwrap_or(cell.align);

        let placed =
            measure_cell_text(cell, origin_x, origin_y, &style, align, metrics_font, font_cache, &faces);

        // Spill window: own box extended over empty neighbors to the adjacent
        // non-empty cell (or the content edge) on each side.
        let spans = &rows[&cell.y.to_bits()];
        let mut clip_x0 = 0.0_f64;
        let mut clip_x1 = page.content_w;
        for &(sx0, sx1) in spans {
            if (sx0 - cell.x).abs() < 0.01 {
                continue; // self
            }
            if sx1 <= cell.x + 0.01 {
                clip_x0 = clip_x0.max(sx1);
            } else if sx0 >= cell.x + cell.w - 0.01 {
                clip_x1 = clip_x1.min(sx0);
            }
        }

        let abs_x0 = origin_x + clip_x0;
        let abs_x1 = origin_x + clip_x1;
        let box_y = origin_y + cell.y;

        // The white-out colour matches the cell's own background (its fill, if
        // any, else the page paper) so suppressing crossed gridlines under
        // overflow text never punches a hole in a filled cell.
        let bg = cell
            .style
            .as_ref()
            .and_then(|s| s.fill_hex.as_deref())
            .unwrap_or(PAGE_BG);

        // Only OVERFLOWING text gets the white-out + clip treatment; text that
        // fits its box paints directly (clipping fitting text would expose the
        // shaping-vs-canvas font measurement drift as chopped glyph edges at
        // the column rails; such drift appears whenever a cell paints with a
        // family other than the one it was shaped with).
        let text_x1 = placed.text_x + placed.advance;
        let overflows = placed.text_x < origin_x + cell.x - 0.01
            || text_x1 > origin_x + cell.x + cell.w + 0.01;
        if overflows {
            // White-out crossed gridlines under the spill — inset 1.5px from
            // the row rails so the horizontal lines survive (Excel suppresses
            // the gridline segments overflow text crosses).
            let wx0 = placed.text_x.max(abs_x0);
            let wx1 = text_x1.min(abs_x1);
            if wx1 > wx0 {
                p.set_fill_style_str(bg);
                p.fill_rect(wx0, box_y + 1.5, wx1 - wx0, cell.h - 3.0);
                p.set_fill_style_str(&style.color);
            }
            // Clip to the spill window so text never overlaps a non-empty
            // neighbor (Excel clips at the first occupied cell).
            p.save();
            p.begin_path();
            p.rect(abs_x0, box_y, abs_x1 - abs_x0, cell.h);
            p.clip();
            let r = p.paint_glyph_run(&style, &cell.text, placed.text_x, placed.baseline_y);
            p.restore();
            r?;
        } else {
            p.paint_glyph_run(&style, &cell.text, placed.text_x, placed.baseline_y)?;
        }
    }

    // ── Search highlights (full-text search) ─────────────────────────────────
    // Additive translucent swipe over every cell (in this culled slice) whose
    // text matches the query — painted LAST so it sits over the cell text like a
    // marker. Lives here, in the SHARED cell painter, so BOTH the print page and
    // the continuous grid viewport get it from their own visible `cells` slice +
    // origin. Read-only over the page model; matching reuses `find_matches`.
    if let Some(sh) = search {
        if !sh.needle.is_empty() {
            p.save();
            // CELL-LEVEL highlight (Excel-style): one translucent rect over the WHOLE
            // matching cell — NOT per-occurrence substrings. A single common char in a
            // 13k-char cell would otherwise run find_matches (Vec of ~1k ranges) AND shape the
            // text prefix to every match (O(len²)/cell) → a main-thread freeze. Here
            // it's an early-exit contains-check (find_matches_any, no Vec, no shaping)
            // plus one fill_rect. The ACTIVE cell — identified by A1 ref, an identity
            // that survives the windowed slice + merge-covered suppression — paints
            // hotter (the SML analogue of the WML active swap).
            for cell in cells {
                if cell.text.is_empty() {
                    continue;
                }
                if !crate::find_matches_any(&cell.text, sh.needle, sh.opts) {
                    continue;
                }
                let is_active = sh.active_ref == Some(cell.ref_a1.as_str());
                p.set_global_alpha(if is_active { sh.active } else { sh.alpha });
                p.set_fill_style_str(if is_active { sh.active_fill } else { sh.fill });
                p.fill_rect(origin_x + cell.x, origin_y + cell.y, cell.w, cell.h);
            }
            p.restore();
        }
    }

    Ok(())
}

/// Paint the two Excel-gray header gutter bands plus their labels.
///
/// `gx`/`gy` is the gutter (margin) origin — the top-left of the LEFT row-number
/// band and the TOP column-letter band. The grid content sits at
/// `(gx + row_gutter_w, gy + col_gutter_h)`. Column letters (`col_letters(
/// first_col + k)`) are centered over each col-rail span; row numbers
/// (`page.row_numbers[i]`) are right-aligned in the left band over each row band.
/// The TOP-LEFT corner (the gutter intersection) is filled but unlabeled, like
/// Excel's select-all box.
fn paint_gutters(
    p: &dyn Painter,
    page: &SheetPage,
    gx: f64,
    gy: f64,
    row_gutter_w: f64,
    col_gutter_h: f64,
) {
    let grid_x = gx + row_gutter_w;
    let grid_y = gy + col_gutter_h;

    // ── Gutter band fills ────────────────────────────────────────────────────
    p.set_fill_style_str(GUTTER_FILL);
    // Top band (column letters): spans the corner + the grid content width.
    p.fill_rect(gx, gy, row_gutter_w + page.content_w, col_gutter_h);
    // Left band (row numbers): below the corner, down the grid content height.
    p.fill_rect(gx, grid_y, row_gutter_w, page.content_h);

    // ── Gutter separators (reuse the gridline gray) ──────────────────────────
    p.set_stroke_style_str(GRIDLINE_COLOR);
    p.set_line_width(1.0);
    // Bottom edge of the top band.
    let by = grid_y.round() + 0.5;
    p.begin_path();
    p.move_to(gx.round() + 0.5, by);
    p.line_to((grid_x + page.content_w).round() + 0.5, by);
    p.stroke();
    // Right edge of the left band.
    let rx = grid_x.round() + 0.5;
    p.begin_path();
    p.move_to(rx, gy.round() + 0.5);
    p.line_to(rx, (grid_y + page.content_h).round() + 0.5);
    p.stroke();

    // ── Hidden-range markers (the Excel heavier boundary rule) ──────────────
    // One heavier stroke in the header gutter at each hidden col/row range
    // boundary. Geometry-driven (zero-size rail spans) — hidden-less pages
    // emit NOTHING here, keeping the paint stream byte-identical.
    let col_marks = hidden_boundary_marks(&page.col_rails);
    let row_marks = hidden_boundary_marks(&page.row_rails);
    if !col_marks.is_empty() || !row_marks.is_empty() {
        p.set_stroke_style_str(HIDDEN_MARKER_COLOR);
        p.set_line_width(HIDDEN_MARKER_WIDTH_PX);
        for mx in col_marks {
            let x = (grid_x + mx).round();
            p.begin_path();
            p.move_to(x, gy);
            p.line_to(x, gy + col_gutter_h);
            p.stroke();
        }
        for my in row_marks {
            let y = (grid_y + my).round();
            p.begin_path();
            p.move_to(gx, y);
            p.line_to(gx + row_gutter_w, y);
            p.stroke();
        }
    }

    // ── Labels ───────────────────────────────────────────────────────────────
    p.set_fill_style_str(GUTTER_TEXT_COLOR);
    p.set_font(&format!(
        "{GUTTER_FONT_PX}px -apple-system, BlinkMacSystemFont, \"Segoe UI\", sans-serif"
    ));
    p.set_text_baseline("middle");

    // Column letters: centered over each rail span (rails are page-local, so add
    // the grid origin). `col_rails` has n+1 entries; n spans → n letters.
    p.set_text_align("center");
    let letter_y = gy + col_gutter_h / 2.0;
    for k in 0..page.col_rails.len().saturating_sub(1) {
        let x0 = grid_x + page.col_rails[k];
        let x1 = grid_x + page.col_rails[k + 1];
        // A HIDDEN column's span is zero-width — its letter skips (like Excel).
        if x1 <= x0 {
            continue;
        }
        let cx = (x0 + x1) / 2.0;
        let label = col_letters(page.first_col + k as u32);
        let _ = p.fill_text(&label, cx, letter_y);
    }

    // Row numbers: right-aligned in the left band, vertically centered in each
    // row band (parallel to the gaps between row_rails).
    p.set_text_align("right");
    let label_x = grid_x - 2.0;
    for (i, rn) in page.row_numbers.iter().enumerate() {
        if i + 1 >= page.row_rails.len() {
            break;
        }
        let y0 = grid_y + page.row_rails[i];
        let y1 = grid_y + page.row_rails[i + 1];
        // A HIDDEN row's band is zero-height — its number skips (like Excel).
        if y1 <= y0 {
            continue;
        }
        let cy = (y0 + y1) / 2.0;
        let _ = p.fill_text(&rn.to_string(), label_x, cy);
    }

    // Restore defaults the cell-text walk relies on (left align, alphabetic).
    p.set_text_align("left");
    p.set_text_baseline("alphabetic");
}

/// Paint a cell's border edges (§18.8.4) as solid strokes on the box perimeter.
/// Order [left, right, top, bottom]. Strokes are centred on the box edge; the
/// width comes from the resolved [`super::model::BorderEdge`] class width.
fn paint_cell_borders(
    p: &dyn Painter,
    cell: &PageCell,
    origin_x: f64,
    origin_y: f64,
    borders: &[Option<super::model::BorderEdge>; 4],
) {
    use crate::engine::border::{stroke_box_edges, Edge, EdgeSide};
    let x = origin_x + cell.x;
    let y = origin_y + cell.y;
    // `borders` is [left, right, top, bottom] (CellStyle order); SML strokes in
    // that order and snaps odd-width rules to the device-pixel centre (`snap`).
    stroke_box_edges(
        x,
        y,
        cell.w,
        cell.h,
        [
            (EdgeSide::Left, borders[0].as_ref()),
            (EdgeSide::Right, borders[1].as_ref()),
            (EdgeSide::Top, borders[2].as_ref()),
            (EdgeSide::Bottom, borders[3].as_ref()),
        ],
        true,
        |e: &Edge, x0, y0, x1, y1| {
            p.set_stroke_style_str(&e.color);
            p.set_line_width(e.width_px);
            p.begin_path();
            p.move_to(x0, y0);
            p.line_to(x1, y1);
            p.stroke();
        },
    );
}

/// Resolved draw position for one cell's text (absolute page coordinates are
/// produced by the caller adding the origin; here x/baseline already include it
/// via the passed origin in `paint_sheet_page`'s loop — see `measure_cell_text`
/// call site).
struct PlacedText {
    text_x: f64,
    advance: f64,
    baseline_y: f64,
}

/// Measure one cell's text: draw x per §18 general alignment (left-aligned
/// spills right, right-aligned spills left, centered both — clipping happens at
/// the caller's spill window), the shaped advance, and the vertically-centered
/// baseline. SIMPLIFICATION: Excel renders #### for numbers wider than
/// their column instead of spilling; we spill numbers like text.
// One more positional arg than clippy's default; the shaping scope rides
// alongside the font it resolves through and splitting them into a struct would
// only move the count.
#[allow(clippy::too_many_arguments)]
fn measure_cell_text<'f>(
    cell: &PageCell,
    origin_x: f64,
    origin_y: f64,
    style: &RunStyle,
    align: HAlign,
    metrics_font: Option<&'f crate::engine::fonts::LoadedFont>,
    font_cache: &'f FontCache,
    faces: &FaceCache<'f>,
) -> PlacedText {
    let box_x = origin_x + cell.x;
    let box_y = origin_y + cell.y;

    // Measure the text advance. When the font isn't loaded we fall back to a
    // rough per-char estimate so alignment still approximates (the painter must
    // not crash on a missing font).
    let advance = match metrics_font {
        Some(f) => faces.shape(&cell.text, f, style.font_pt)
            .map(|r| r.total_advance)
            .unwrap_or_else(|| estimate_advance(&cell.text, style.font_pt)),
        None => font_cache
            .resolve(&style.family, 400, false)
            .and_then(|f| faces.shape(&cell.text, f, style.font_pt))
            .map(|r| r.total_advance)
            .unwrap_or_else(|| estimate_advance(&cell.text, style.font_pt)),
    };

    let inset = CELL_TEXT_INSET_PX;
    let text_x = match align {
        HAlign::Left => box_x + inset,
        HAlign::Right => box_x + cell.w - inset - advance,
        HAlign::Center => box_x + (cell.w - advance) / 2.0,
    };

    // Vertical centering: place the alphabetic baseline so the text sits
    // visually centered in the cell box. Approx via font metrics when loaded.
    let baseline_y = match metrics_font {
        Some(f) => {
            let m = fonts::font_metrics(f, style.font_pt);
            let text_h = m.ascent + m.descent;
            box_y + (cell.h - text_h) / 2.0 + m.ascent
        }
        None => box_y + cell.h * 0.7, // rough center for ~11pt in a ~20px row.
    };

    PlacedText { text_x, advance, baseline_y }
}

/// The normal-style cell `RunStyle` (§18 general): the normal-style family at
/// the default 11pt, no bold/italic, default colour — the base that
/// [`cell_run_style`] layers each cell's resolved style onto.
fn cell_text_style(family: &str) -> RunStyle {
    let fam = if family.is_empty() {
        DEFAULT_NORMAL_FONT.to_string()
    } else {
        family.to_string()
    };
    RunStyle {
        bold: false,
        italic: false,
        font_pt: DEFAULT_FONT_PT,
        family: fam.clone(),
        family_east_asia: fam.clone(),
        family_complex: fam,
        run_is_complex: false,
        color: CELL_TEXT_COLOR.to_string(),
        underline: Underline::None,
        strikethrough: false,
        double_strike: false,
        vert_align: VertAlign::Baseline,
        highlight: None,
        run_shading: None,
        char_spacing_px: 0.0,
        all_caps: false,
        small_caps: false,
        vanish: false,
        outline: false,
        shadow: false,
        emboss: false,
        imprint: false,
        baseline_shift_px: 0.0,
    }
}

/// Layer a resolved [`CellStyle`] over the normal-style default to produce
/// the per-cell paint `RunStyle`: bold/italic/strikethrough, the explicit font
/// size/family/colour when set, else the normal-style defaults. Cells with no
/// style return a clone of `default_style` unchanged.
fn cell_run_style(default_style: &RunStyle, cell_style: Option<&CellStyle>) -> RunStyle {
    let mut style = default_style.clone();
    let Some(cs) = cell_style else { return style };
    style.bold = cs.bold;
    style.italic = cs.italic;
    style.strikethrough = cs.strikethrough;
    if let Some(pt) = cs.font_size_pt {
        style.font_pt = pt;
    }
    if let Some(fam) = &cs.font_family {
        if !fam.is_empty() {
            style.family = fam.clone();
            style.family_east_asia = fam.clone();
            style.family_complex = fam.clone();
        }
    }
    if let Some(c) = &cs.font_color_hex {
        style.color = c.clone();
    }
    style
}

/// CSS `font` shorthand for a cell `RunStyle` (mirrors lib.rs `RunStyle::font_css`
/// — duplicated locally to keep that WML-side impl untouched; the shape is the
/// same so the canvas renders cell text identically to WML body text).
fn font_css(style: &RunStyle) -> String {
    let s = if style.italic { "italic" } else { "normal" };
    let w = if style.bold { "bold" } else { "normal" };
    format!(
        "{s} {w} {size}pt \"{family}\", -apple-system, BlinkMacSystemFont, sans-serif",
        size = style.font_pt,
        family = style.family,
    )
}

/// Rough text-advance estimate when no font is loaded (~0.5em per char). Only
/// used so right/center alignment doesn't collapse to 0 before fonts arrive.
fn estimate_advance(text: &str, font_pt: f64) -> f64 {
    let em = crate::engine::units::pt_to_px(font_pt);
    text.chars().count() as f64 * em * 0.5
}

// ─── Continuous grid view (virtual scroller) ─────────────────────────────────
// Paints ONE viewport window of an unbounded continuous SheetPage (see
// grid::continuous_page): the TS grid adapter owns a native scroll container whose
// spacer spans the full sheet extent (real draggable scrollbar), and calls this
// on every scroll frame with the current offsets. Only the visible row band's
// cells are painted (grid::visible_cells — binary-searched), so cost per frame
// is O(visible), not O(sheet). Gutters are painted LAST at viewport-fixed
// positions (sticky headers). Cell-loop logic mirrors paint_sheet_page over the
// visible slice — deliberate duplication over refactoring the proven paged
// path.

/// Excel-style freeze boundary line — darker than the gridline gray so the
/// pinned bands read as pinned (§18.3.1.66 frozen panes, grid view).
const FREEZE_LINE_COLOR: &str = "#8a8a8a";

/// Frozen-pane paint context (§18.3.1.66, grid view only). `bands` is the
/// shared quadrant mapping ([`super::pane::PaneBands`] — the SAME helper the
/// hit-test consumes); `band_page` is the windowed page over the pinned rows
/// (rows `1..=ySplit`, view-clipped), present iff rows are frozen — the corner
/// and top-band quadrants read their cells from it while the left band and the
/// body read the scrolled window page.
pub(crate) struct PaneCtx<'a> {
    pub(crate) bands: super::pane::PaneBands,
    pub(crate) band_page: Option<&'a SheetPage>,
}

/// Paint the `[scroll_x, scroll_y, +view_w, +view_h]` window of a continuous
/// sheet page onto a viewport-sized canvas.
///
/// `gutters` is the sticky `(row_gutter_w, col_gutter_h)` — passed explicitly
/// (Grid v2) because the page is now a WINDOW: deriving the row-gutter width
/// from the window's own `row_numbers` (the old `gutter_dims(page)`) would
/// make the gutter widen as you scroll down a tall sheet; the renderer derives
/// it once from the sheet's effective max row, matching `grid_extent`.
///
/// `pane` (frozen panes): the grid paints as up to FOUR quadrants — the pinned
/// corner, the pinned top row band, the pinned left column band, and the
/// scrolling body — each the SAME region paint under its own clip with the
/// frozen axis' scroll zeroed ([`super::pane::axis_segments`], the shared
/// quadrant mapping). `None` (every pane-less sheet) collapses to ONE segment
/// per axis whose values are the exact pane-less expressions — the paint
/// stream is byte-identical (pinned by `pane_tests`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_grid_viewport(
    p: &dyn Painter,
    page: &SheetPage,
    gutters: (f64, f64),
    scroll_x: f64,
    scroll_y: f64,
    view_w: f64,
    view_h: f64,
    normal_font: &str,
    font_cache: &FontCache,
    _bitmaps: &HashMap<String, ImageBitmap>,
    charts: &HashMap<String, Vec<u8>>,
    selected_object: Option<&str>,
    search: Option<&SearchHighlight>,
    cell_sel: Option<&super::select::CellSelPaint>,
    pane: Option<&PaneCtx>,
) -> Result<(), JsValue> {
    let (gut_w, gut_h) = gutters;
    let (band_w, band_h) = pane.map_or((0.0, 0.0), |pc| (pc.bands.band_w, pc.bands.band_h));
    // The pinned-axis view segments (ONE per axis when pane-less — the exact
    // pane-less window math; band + body when frozen).
    let segs_x = super::pane::axis_segments(gut_w, view_w, band_w, scroll_x);
    let segs_y = super::pane::axis_segments(gut_h, view_h, band_h, scroll_y);

    p.set_fill_style_str(PAGE_BG);
    p.fill_rect(0.0, 0.0, view_w, view_h);

    // ── Content quadrants (each clipped beyond the sticky gutters) ──────────
    // Pinned-row quadrants (corner + top band) read the band page; the left
    // band + body read the scrolled window page.
    for sy in &segs_y {
        let qpage = if sy.pinned {
            pane.and_then(|pc| pc.band_page).unwrap_or(page)
        } else {
            page
        };
        for sx in &segs_x {
            paint_grid_region(
                p, qpage, sx, sy, gut_w, gut_h, normal_font, font_cache, charts,
                selected_object, search, cell_sel,
            )?;
        }
    }

    // ── Sticky gutters (viewport-fixed) ──────────────────────────────────────
    let gutter_bg = "#f0f0f0";
    // Top band: column letters at the visible rail spans.
    p.set_fill_style_str(gutter_bg);
    p.fill_rect(0.0, 0.0, view_w, gut_h);
    // Left band: row numbers for the visible bands.
    p.fill_rect(0.0, 0.0, gut_w, view_h);
    p.set_stroke_style_str(GRIDLINE_COLOR);
    p.set_line_width(1.0);
    p.begin_path();
    p.move_to(0.5, gut_h.round() + 0.5);
    p.line_to(view_w, gut_h.round() + 0.5);
    p.move_to(gut_w.round() + 0.5, 0.5);
    p.line_to(gut_w.round() + 0.5, view_h);
    p.stroke();

    let label_style = cell_text_style(normal_font);
    let label_family = fonts::resolve_font_with_fallbacks(&label_style.family, font_cache);
    p.set_font(&format!("10px {label_family}"));
    p.set_fill_style_str("#444444");
    p.set_text_baseline("middle");

    // Column letters (clip to the area right of the corner box), one pass per
    // x segment so frozen-column letters stay pinned with their cells.
    for sx in &segs_x {
        let ox = gut_w - sx.scroll;
        p.save();
        p.begin_path();
        p.rect(sx.view0, 0.0, sx.len, gut_h);
        p.clip();
        p.set_text_align("center");
        // Hidden-range marker dedupe (consecutive hidden cols share one rail).
        let mut last_mark: Option<u64> = None;
        for k in 0..page.col_rails.len().saturating_sub(1) {
            let x0 = page.col_rails[k];
            let x1 = page.col_rails[k + 1];
            if x1 < sx.content0 || x0 > sx.content1 {
                continue;
            }
            // A HIDDEN column's span is zero-width: its letter SKIPS and the
            // Excel heavier boundary rule marks the gutter instead. Hidden-
            // gated — visible spans never collapse, so hidden-less sheets
            // paint byte-identically.
            if x1 <= x0 {
                if last_mark != Some(x0.to_bits()) {
                    last_mark = Some(x0.to_bits());
                    p.set_stroke_style_str(HIDDEN_MARKER_COLOR);
                    p.set_line_width(HIDDEN_MARKER_WIDTH_PX);
                    p.begin_path();
                    let x = (ox + x0).round();
                    p.move_to(x, 0.0);
                    p.line_to(x, gut_h);
                    p.stroke();
                }
                continue;
            }
            let label = super::grid::col_letters(page.first_col + k as u32);
            let _ = p.fill_text(&label, ox + (x0 + x1) / 2.0, gut_h / 2.0);
        }
        p.restore();
    }

    // Row numbers (clip to below the corner box), one pass per y segment (the
    // pinned segment labels the band page's frozen rows).
    for sy in &segs_y {
        let qpage = if sy.pinned {
            pane.and_then(|pc| pc.band_page).unwrap_or(page)
        } else {
            page
        };
        let oy = gut_h - sy.scroll;
        let (band0, band1) = super::grid::visible_row_bands(qpage, sy.content0, sy.content1);
        p.save();
        p.begin_path();
        p.rect(0.0, sy.view0, gut_w, sy.len);
        p.clip();
        p.set_text_align("right");
        // Hidden-range marker dedupe (consecutive hidden rows share one rail).
        let mut last_mark: Option<u64> = None;
        for b in band0..=band1 {
            if b >= qpage.row_numbers.len() || b + 1 >= qpage.row_rails.len() {
                break;
            }
            let y0 = qpage.row_rails[b];
            let y1 = qpage.row_rails[b + 1];
            // A HIDDEN row's band is zero-height: its number SKIPS and the
            // Excel heavier boundary rule marks the gutter instead.
            if y1 <= y0 {
                if last_mark != Some(y0.to_bits()) {
                    last_mark = Some(y0.to_bits());
                    p.set_stroke_style_str(HIDDEN_MARKER_COLOR);
                    p.set_line_width(HIDDEN_MARKER_WIDTH_PX);
                    p.begin_path();
                    let y = (oy + y0).round();
                    p.move_to(0.0, y);
                    p.line_to(gut_w, y);
                    p.stroke();
                }
                continue;
            }
            let _ = p.fill_text(
                &qpage.row_numbers[b].to_string(),
                gut_w - 4.0,
                oy + (y0 + y1) / 2.0,
            );
        }
        p.restore();
    }
    p.set_text_align("left");

    // ── Selected header tint (cell selection) — a stronger swipe over the
    // selection's column span (top gutter) + row span (left gutter), clipped to
    // each band so it never bleeds into the corner or the content. Byte-identical
    // when no selection is set. Per pane segment, so a selection spanning the
    // freeze boundary highlights the header in every segment it crosses.
    if let Some(cs) = cell_sel {
        p.save();
        p.set_global_alpha(SEL_HEADER_ALPHA);
        p.set_fill_style_str(SEL_TINT);
        // Column header band.
        for sx in &segs_x {
            p.save();
            p.begin_path();
            p.rect(sx.view0, 0.0, sx.len, gut_h);
            p.clip();
            p.fill_rect(gut_w - sx.scroll + cs.x0, 0.0, cs.x1 - cs.x0, gut_h);
            p.restore();
        }
        // Row header band.
        for sy in &segs_y {
            p.save();
            p.begin_path();
            p.rect(0.0, sy.view0, gut_w, sy.len);
            p.clip();
            p.fill_rect(0.0, gut_h - sy.scroll + cs.y0, gut_w, cs.y1 - cs.y0);
            p.restore();
        }
        p.restore();
    }

    // ── Freeze boundary lines (§18.3.1.66) — the Excel-style darker rule at
    // the freeze edge, drawn LAST so it crosses the header gutters like Excel's.
    // Pane-gated: a pane-less paint never reaches here (byte-identity).
    if pane.is_some() {
        p.set_stroke_style_str(FREEZE_LINE_COLOR);
        p.set_line_width(1.0);
        if band_w > 0.0 {
            let x = (gut_w + band_w).round() + 0.5;
            p.begin_path();
            p.move_to(x, 0.0);
            p.line_to(x, view_h);
            p.stroke();
        }
        if band_h > 0.0 {
            let y = (gut_h + band_h).round() + 0.5;
            p.begin_path();
            p.move_to(0.0, y);
            p.line_to(view_w, y);
            p.stroke();
        }
    }

    Ok(())
}

/// Paint ONE grid quadrant: the content window `[sx.content0, sx.content1] ×
/// [sy.content0, sy.content1]` of `page`, clipped to the segment rects, with
/// the segments' effective scrolls (gridlines → merges → cells → pictures →
/// cell selection — the exact pane-less content pass, extracted verbatim so a
/// single-segment call is byte-identical to the pre-pane painter).
#[allow(clippy::too_many_arguments)]
fn paint_grid_region(
    p: &dyn Painter,
    page: &SheetPage,
    sx: &super::pane::AxisSeg,
    sy: &super::pane::AxisSeg,
    gut_w: f64,
    gut_h: f64,
    normal_font: &str,
    font_cache: &FontCache,
    charts: &HashMap<String, Vec<u8>>,
    selected_object: Option<&str>,
    search: Option<&SearchHighlight>,
    cell_sel: Option<&super::select::CellSelPaint>,
) -> Result<(), JsValue> {
    // Content origin in viewport coordinates (per the segment's eff. scroll).
    let ox = gut_w - sx.scroll;
    let oy = gut_h - sy.scroll;
    // Visible content window in content-local coordinates.
    let win_y0 = sy.content0;
    let win_y1 = sy.content1;
    let win_x0 = sx.content0;
    let win_x1 = sx.content1;

    p.save();
    p.begin_path();
    p.rect(sx.view0, sy.view0, sx.len, sy.len);
    p.clip();

    let (band0, band1) = super::grid::visible_row_bands(page, win_y0, win_y1);

    if page.show_grid_lines && page.row_rails.len() >= 2 {
        p.set_stroke_style_str(GRIDLINE_COLOR);
        p.set_line_width(1.0);
        let y_top = (oy + page.row_rails[band0]).max(sy.view0);
        let y_bot = (oy + page.row_rails[band1]).min(sy.view1);
        for rx in &page.col_rails {
            if *rx < win_x0 - 1.0 || *rx > win_x1 + 1.0 {
                continue;
            }
            let x = (ox + rx).round() + 0.5;
            p.begin_path();
            p.move_to(x, y_top.round() + 0.5);
            p.line_to(x, y_bot.round() + 0.5);
            p.stroke();
        }
        let x_left = (ox + page.col_rails.first().copied().unwrap_or(0.0)).max(sx.view0);
        let x_right = (ox + page.col_rails.last().copied().unwrap_or(page.content_w)).min(sx.view1);
        for ry in &page.row_rails[band0..=band1] {
            let y = (oy + ry).round() + 0.5;
            p.begin_path();
            p.move_to(x_left.round() + 0.5, y);
            p.line_to(x_right.round() + 0.5, y);
            p.stroke();
        }
    }

    // Merged regions intersecting the window (§18.3.1.55).
    for m in &page.merges {
        if m.y + m.h < win_y0 || m.y > win_y1 {
            continue;
        }
        let bx = ox + m.x;
        let by = oy + m.y;
        p.set_fill_style_str(PAGE_BG);
        p.fill_rect(bx + 1.0, by + 1.0, (m.w - 2.0).max(0.0), (m.h - 2.0).max(0.0));
        p.set_stroke_style_str(GRIDLINE_COLOR);
        p.set_line_width(1.0);
        p.begin_path();
        p.rect(bx.round() + 0.5, by.round() + 0.5, m.w.round() - 1.0, m.h.round() - 1.0);
        p.stroke();
    }

    let cells = super::grid::visible_cells(page, win_y0, win_y1);

    // Fills (§18.8.20), borders (§18.8.4), then overflow-aware cell text —
    // shared with the paged path. Full rows are present in the visible slice,
    // so per-row occupancy (spill/clip) is exact.
    paint_cells(p, page, ox, oy, cells, normal_font, font_cache, search)?;

    // ── Anchored pictures — drawn ABOVE the cells, inside the content
    // clip (so they scroll with the grid and never cover the sticky gutters).
    // Cull to the visible window; the continuous page is one band, so the
    // page-local image box + scroll offset is the viewport position directly.
    for im in &page.images {
        if im.y + im.h < win_y0 || im.y > win_y1 || im.x + im.w < win_x0 || im.x > win_x1 {
            continue;
        }
        let (x, y) = (ox + im.x, oy + im.y);
        let _ = p.draw_image(&im.key, x, y, im.w, im.h);
    }

    // ── Anchored CHARTS (embedded charts) — the same drawing layer, rendered
    // via the SHARED `crate::chart::render_chart` inside this quadrant's clip
    // (a chart straddling the freeze boundary paints per quadrant, clipped —
    // it falls out of the region path). Chart-gated: chart-less sheets emit
    // ZERO extra commands (the paint-stream pins hold byte-identically).
    for ch in &page.charts {
        if ch.y + ch.h < win_y0 || ch.y > win_y1 || ch.x + ch.w < win_x0 || ch.x > win_x1 {
            continue;
        }
        if let Some(xml) = charts.get(&ch.key) {
            let (x, y) = (ox + ch.x, oy + ch.y);
            // White chart-area ground (Excel's default chart fill — the grid
            // must not show through; the renderChartToCanvas rationale).
            p.set_fill_style_str(PAGE_BG);
            p.fill_rect(x, y, ch.w, ch.h);
            let _ = crate::chart::render_chart(xml, p, x, y, ch.w, ch.h);
        }
    }

    // ── Selected chart-object OUTLINE (read-only object style) — additive,
    // per-quadrant clipped like everything above; `None` (or a key painted on
    // no chart of this page) emits nothing ⇒ byte-identical render.
    if let Some(key) = selected_object {
        for ch in &page.charts {
            if ch.key != key
                || ch.y + ch.h < win_y0
                || ch.y > win_y1
                || ch.x + ch.w < win_x0
                || ch.x > win_x1
            {
                continue;
            }
            p.save();
            p.set_stroke_style_str(OBJECT_OUTLINE_COLOR);
            p.set_line_width(OBJECT_OUTLINE_WIDTH_PX);
            p.stroke_rect(ox + ch.x, oy + ch.y, ch.w, ch.h);
            p.restore();
        }
    }

    // ── Cell selection — additive, inside the content clip.
    // Fires ONLY when a selection is set, so a no-selection render is byte-
    // identical (the SML equivalence pins hold). Painted per quadrant: a range
    // spanning the freeze boundary tints in every quadrant it intersects.
    if let Some(cs) = cell_sel {
        paint_cell_selection(p, cs, ox, oy);
    }
    p.restore(); // content clip

    Ok(())
}

/// The selected-object outline (read-only, no handles).
/// Locally duplicated from `wml::select` (module-private consts there; the
/// render.rs local-duplication precedent, see `font_css`) so a selected SML
/// chart outlines in the SAME blue focus ring as a WML image/chart.
const OBJECT_OUTLINE_COLOR: &str = "#3B82F6";
const OBJECT_OUTLINE_WIDTH_PX: f64 = 2.0;

/// Excel-green selection accent (cell selection). The range fill is translucent
/// so cell text shows through; the active cell is left UN-tinted (Excel white
/// active cell) via a range-minus-active band fill, then both get a border (thin
/// range, thick active).
const SEL_TINT: &str = "#217346";
const SEL_TINT_ALPHA: f64 = 0.15;
const SEL_HEADER_ALPHA: f64 = 0.30;

/// Paint the cell-selection accent in content space (`ox`/`oy` = content origin).
fn paint_cell_selection(p: &dyn Painter, cs: &super::select::CellSelPaint, ox: f64, oy: f64) {
    let (rx, ry) = (ox + cs.x0, oy + cs.y0);
    let (rw, rh) = (cs.x1 - cs.x0, cs.y1 - cs.y0);
    let (ax, ay) = (ox + cs.ax0, oy + cs.ay0);
    let (aw, ah) = (cs.ax1 - cs.ax0, cs.ay1 - cs.ay0);

    p.save();
    // Range tint MINUS the active cell (four bands), so the active cell reads
    // white/distinct inside the range (a collapsed single-cell selection yields
    // zero bands — just the borders below).
    p.set_global_alpha(SEL_TINT_ALPHA);
    p.set_fill_style_str(SEL_TINT);
    if ay > ry {
        p.fill_rect(rx, ry, rw, ay - ry); // above the active cell
    }
    let ab = ay + ah;
    if ab < ry + rh {
        p.fill_rect(rx, ab, rw, (ry + rh) - ab); // below
    }
    if ax > rx {
        p.fill_rect(rx, ay, ax - rx, ah); // left, level with the active cell
    }
    let ar = ax + aw;
    if ar < rx + rw {
        p.fill_rect(ar, ay, (rx + rw) - ar, ah); // right
    }
    p.set_global_alpha(1.0);

    // Range border (thin).
    p.set_stroke_style_str(SEL_TINT);
    p.set_line_width(1.0);
    p.begin_path();
    p.rect(rx.round() + 0.5, ry.round() + 0.5, (rw.round() - 1.0).max(0.0), (rh.round() - 1.0).max(0.0));
    p.stroke();
    // Active cell border (thick — the distinct active cell).
    p.set_line_width(2.0);
    p.begin_path();
    p.rect(ax.round() + 0.5, ay.round() + 0.5, (aw.round() - 1.0).max(0.0), (ah.round() - 1.0).max(0.0));
    p.stroke();
    p.restore();
}
