//! Grid geometry + print-page pagination.
//!
//! Turns a [`super::model::SheetModel`] (used range + column/row metadata) into
//! pixel geometry and paginates it into [`SheetPage`] structs, each with a
//! deterministic geometry hash (as the oracle tests do: plain structs +
//! page-count + geometry-hash snapshot against pinned baselines). No
//! wasm-bindgen / web-sys.
//!
//! Spec grounding (ECMA-376 §18):
//!  - §18.3.1.13  width (character units of MDW) → pixels, via the exact
//!    Truncate formulas (the 8.7109375→61px worked example is the
//!    unit-test vector).
//!  - §18.3.1.35  dimension → the used range walked here for pagination.
//!  - §18.3.1.73  row@ht in POINTS → px via `engine::units::pt_to_px`.
//!  - §18.3.1.81  sheetFormatPr defaults.

use crate::engine::units::{emu_to_px, pt_to_px};

use super::drawings::SheetDrawing;
use super::model::{CellModel, CellStyle, HAlign, MergeRegion, SheetModel};

/// §18.3.1.13: convert a column's CHARACTER-UNIT width to a file `@width`
/// double, given the Maximum Digit Width in pixels:
///   `width = Truncate((chars*MDW + 5)/MDW * 256)/256`
/// (5 = 4px margin + 1px gridline padding). This is the value Excel persists.
pub(crate) fn chars_to_file_width(chars: f64, mdw: f64) -> f64 {
    let raw = (chars * mdw + 5.0) / mdw * 256.0;
    raw.trunc() / 256.0
}

/// §18.3.1.13: convert a file `@width` double to a runtime PIXEL width:
///   `px = Truncate(((256*width + Truncate(128/MDW))/256) * MDW)`
/// Worked vector: width 8.7109375, MDW 7 → 61 pixels.
pub(crate) fn file_width_to_px(width: f64, mdw: f64) -> f64 {
    let inner = (128.0 / mdw).trunc();
    let raw = ((256.0 * width + inner) / 256.0) * mdw;
    raw.trunc()
}

/// Convenience: character-unit width → runtime pixels, composing the two
/// §18.3.1.13 formulas (chars → file width → pixels). This is how an Excel
/// column width round-trips to the on-screen pixel width.
pub(crate) fn chars_to_px(chars: f64, mdw: f64) -> f64 {
    file_width_to_px(chars_to_file_width(chars, mdw), mdw)
}

/// One painted cell on a page, in page-local pixel coordinates.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PageCell {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) w: f64,
    pub(crate) h: f64,
    pub(crate) text: String,
    pub(crate) align: HAlign,
    /// The cell's resolved per-cell style, flowed verbatim from the model;
    /// `None` keeps the default render path (normal font / no fill / no border).
    pub(crate) style: Option<CellStyle>,
    /// A1-style reference (formula-bar readout / future selection addressing).
    pub(crate) ref_a1: String,
    /// The raw (pre-numFmt) value (formula-bar readout).
    pub(crate) raw: String,
    /// `true` when this cell is the anchor of a merged region (§18.3.1.55) and
    /// its `w`/`h` have been expanded to span the full merged extent. Covered
    /// (non-anchor) cells are dropped entirely, so a page never carries them.
    pub(crate) is_merged_anchor: bool,
}

/// A merged region projected onto one page in page-local pixels (§18.3.1.55).
/// The renderer uses these to white-out the interior gridline rails so the
/// region reads as a single box (Excel suppresses the rails inside a merge).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MergeBox {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) w: f64,
    pub(crate) h: f64,
}

/// One anchored picture projected onto a page, in page-local pixels.
/// The box may overhang the page edges (an image spanning a page break keeps
/// its full box on every page it intersects, offset negative/oversized); the
/// painter clips to the page content rect. `key` addresses the shared
/// `image_bitmaps` map (`sml::{sheetIdx}::{relId}`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PageImage {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) w: f64,
    pub(crate) h: f64,
    pub(crate) key: String,
}

/// One print page of a sheet: the gridline column-x and row-y rails (page-local)
/// plus the cells that fall on it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SheetPage {
    pub(crate) sheet_name: String,
    /// Vertical gridline x-positions (page-local px), left-to-right, including
    /// the right edge.
    pub(crate) col_rails: Vec<f64>,
    /// Horizontal gridline y-positions (page-local px), top-to-bottom, including
    /// the bottom edge.
    pub(crate) row_rails: Vec<f64>,
    pub(crate) cells: Vec<PageCell>,
    pub(crate) show_grid_lines: bool,
    /// Total content width / height used on this page (px).
    pub(crate) content_w: f64,
    pub(crate) content_h: f64,
    /// 1-based column number of the page's FIRST column rail (header gutters).
    /// Under wide-sheet column tiling this is the band's start column (the
    /// used range's `min_col` for the first band). The painter
    /// derives each column-letter label as `col_letters(first_col + k)` for the
    /// k-th rail span.
    pub(crate) first_col: u32,
    /// 1-based row number for each row band on the page (header gutters), parallel
    /// to the gaps between `row_rails` (one entry per band, so `row_numbers.len()
    /// == row_rails.len() - 1`). The painter draws these in the left gutter.
    pub(crate) row_numbers: Vec<u32>,
    /// Merged regions on this page in page-local px (§18.3.1.55) — the renderer
    /// suppresses interior gridline rails inside each so the region reads as one
    /// box. Empty when the sheet has no merges that intersect this page.
    pub(crate) merges: Vec<MergeBox>,
    /// Anchored pictures intersecting this page, in page-local px. The
    /// painter draws them LAST (Excel's drawing layer floats above the grid).
    pub(crate) images: Vec<PageImage>,
    /// Anchored CHARTS intersecting this page, in page-local px (embedded
    /// charts). Same box math as `images` (`key` = the chart key into the
    /// Viewer's shared `chart_xml`); painted in the same drawing layer but via
    /// the SHARED `crate::chart::render_chart`.
    pub(crate) charts: Vec<PageImage>,
}

/// Print-page box the sheet paginates into (the shared print-page geometry the
/// Viewer will reuse). Page-local origin is the top-left of the content area.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageBox {
    // `width` drives column-band tiling: the usable width per page is
    // `width - row_gutter`, and a sheet wider than that splits into bands emitted
    // downThenOver (§18.3.1.63). `height` drives row pagination.
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl Default for PageBox {
    /// US Letter at 96 DPI (816×1056 px) — the default WML page box; SML reuses
    /// it as the print-page (per-sheet page sizing is not implemented).
    fn default() -> Self {
        PageBox { width: 816.0, height: 1056.0 }
    }
}

/// One column band of a wide sheet (column tiling): a contiguous run of
/// columns `band_min_col..=band_max_col` that fits one print-page width, with
/// band-LOCAL x rails restarting at 0 (so each band paints from its own left
/// edge). `plan_pages` walks the used range greedily filling bands to the
/// usable page width, then emits all row-chunks of one band before the next
/// (downThenOver, §18.3.1.63 default).
pub(crate) struct ColBand {
    pub(crate) min_col: u32,
    pub(crate) max_col: u32,
    /// Band-local x rails: `col_x[k]` is the left edge of the (`min_col`+k)-th
    /// column relative to the band's left, the last entry the band's right edge.
    pub(crate) col_x: Vec<f64>,
    /// Total band width in px (= `col_x.last()`).
    pub(crate) content_w: f64,
    /// The band's absolute left edge in sheet content space (cumulative width
    /// of the columns before the band — picture re-basing).
    pub(crate) x0: f64,
}

/// Greedily split `min_col..=max_col` into column bands that each fit the usable
/// page width. Mirrors the row loop's single-oversized guard: a column
/// wider than the whole usable width still gets its own band rather than
/// producing a zero-column band. `usable_w` is the page content width after the
/// row gutter is reserved (gutters consume width); margins are an origin shift
/// applied at paint time, so — exactly like the height path which uses the bare
/// `PageBox.height` — they are NOT subtracted here. `col_px` is the §18.3.1.13
/// pixel width of a 1-based column (`chars_to_px(width_chars, mdw)`).
fn split_col_bands(col_px: &dyn Fn(u32) -> f64, min_col: u32, max_col: u32, usable_w: f64) -> Vec<ColBand> {
    let mut bands: Vec<ColBand> = Vec::new();
    let mut band_min = min_col;
    let mut col_x: Vec<f64> = vec![0.0];
    let mut x = 0.0;
    for c in min_col..=max_col {
        let cw = col_px(c);
        // Start a new band when adding this column would overflow the usable
        // width AND the band already holds at least one column (a single
        // oversized column still gets its own band — the row-loop guard).
        if c > band_min && x + cw > usable_w {
            bands.push(ColBand {
                min_col: band_min,
                max_col: c - 1,
                col_x: col_x.clone(),
                content_w: x,
                x0: 0.0,
            });
            band_min = c;
            col_x = vec![0.0];
            x = 0.0;
        }
        x += cw;
        col_x.push(x);
    }
    bands.push(ColBand { min_col: band_min, max_col, col_x, content_w: x, x0: 0.0 });
    bands
}

/// Lay a sheet out into print pages.
///
/// Splits the used range (§18.3.1.35) into column BANDS that each fit one
/// print-page width (tiling), then walks each band column-by-column for x
/// rails and row-by-row for y rails, paginating whenever the next row would
/// overflow the page height. Page order is downThenOver (§18.3.1.63
/// `pageSetup@pageOrder`, ST_PageOrder §18.18.51, DEFAULT): ALL row-chunks of
/// band 1 (top-to-bottom) precede band 2's, etc. `pageOrder` itself is fixed to
/// the default for v1 (no `pageSetup` parse). Missing rows/cells are EMPTY — they
/// still occupy a default-height rail so the grid stays aligned (a sparse sheet
/// is not a collapsed sheet), but only across the used range.
/// Synthesized used range for an EMPTY sheet: enough rows/columns to read
/// as a blank spreadsheet grid while fitting one default print page (10 default
/// 8-char columns ≈ 610px wide, 25 default 15pt rows ≈ 500px tall).
/// (`pub(crate)`: the Grid-v2 index synthesizes the SAME default range so its
/// closed-form extent matches `paginate`'s synthesis exactly.)
pub(crate) const EMPTY_SHEET_DEFAULT_ROWS: u32 = 25;
pub(crate) const EMPTY_SHEET_DEFAULT_COLS: u32 = 10;

pub(crate) fn paginate(model: &SheetModel, page: PageBox) -> Vec<SheetPage> {
    // A HIDDEN row is a 0pt height exception (§18.3.1.73 — zero contribution
    // to the shared sums); hidden-less rows keep the exact old mapping.
    let heights: Vec<(u32, f64)> = model
        .rows
        .iter()
        .filter_map(|rd| {
            if rd.hidden {
                Some((rd.row, 0.0))
            } else {
                rd.height_pt.map(|h| (rd.row, h))
            }
        })
        .collect();
    let col_width_chars = |c: u32| model.col_width_chars(c);
    let col_hidden = |c: u32| model.col_hidden(c);
    let plan = plan_pages(
        PlanInput {
            name: &model.name,
            show_grid_lines: model.show_grid_lines,
            dims: model.dimensions,
            default_row_height_pt: model.default_row_height_pt,
            mdw: model.mdw,
            row_heights_pt: heights,
            col_width_chars: &col_width_chars,
            col_hidden: &col_hidden,
            drawings: &model.drawings,
            charts: &model.charts,
            merges: model.merges.clone(),
        },
        page,
    );
    // Map populated rows to their cells for O(1) lookup (last duplicate wins —
    // the HashMap-insert rule the original single-pass paginate applied).
    let mut row_text: std::collections::HashMap<u32, &Vec<CellModel>> =
        std::collections::HashMap::new();
    for rd in &model.rows {
        row_text.insert(rd.row, &rd.cells);
    }
    (0..plan.pages.len())
        .map(|i| fill_plan_page(&plan, i, |r| row_text.get(&r).map(|v| v.as_slice())))
        .collect()
}

// ─── Grid v2: lazy pagination — plan (metadata math) + per-page fill ─────────

/// The metadata a sheet pagination plan is computed from — NO cells. Both the
/// eager [`paginate`] (from a materialized `SheetModel`) and the lazy
/// path (straight off the `SheetIndex`) build one of these, so the page
/// boundaries are identical BY CONSTRUCTION (the same code runs the same
/// FP-accumulation over the same heights).
pub(crate) struct PlanInput<'a> {
    pub(crate) name: &'a str,
    pub(crate) show_grid_lines: bool,
    /// The pagination range (§18.3.1.35 used range / lazy print clip);
    /// `None` → the empty-sheet default-grid synthesis.
    pub(crate) dims: Option<(u32, u32, u32, u32)>,
    pub(crate) default_row_height_pt: f64,
    pub(crate) mdw: f64,
    /// Explicit `@ht` rows (§18.3.1.73, points) in DOCUMENT order — duplicate
    /// row numbers last-wins (the HashMap-insert rule the eager walk applied).
    pub(crate) row_heights_pt: Vec<(u32, f64)>,
    /// §18.3.1.13 width lookup (character units) for a 1-based column.
    pub(crate) col_width_chars: &'a dyn Fn(u32) -> f64,
    /// Whether a 1-based column is HIDDEN (§18.3.1.13 `@hidden` / width-0 +
    /// customWidth): a hidden column contributes EXACTLY 0px to the rails (the
    /// chars→px formula must not run — it adds a 5px padding floor).
    pub(crate) col_hidden: &'a dyn Fn(u32) -> bool,
    pub(crate) drawings: &'a [SheetDrawing],
    /// Anchored-chart placements (embedded charts) — same anchor geometry as
    /// `drawings`, kept separate so pages carry them as `SheetPage::charts`.
    pub(crate) charts: &'a [SheetDrawing],
    pub(crate) merges: Vec<MergeRegion>,
}

/// One planned print page: a column band + an inclusive row range. Pure
/// numbers — the cells materialize on first paint, through the chunk
/// path, when the page becomes visible.
pub(crate) struct PlanPage {
    /// Index into [`SheetPlan::bands`].
    pub(crate) band: usize,
    /// First/last (inclusive) row on the page.
    pub(crate) r0: u32,
    pub(crate) r1: u32,
    /// Absolute content-y of `r0`'s top edge (the flush `row_top` FP order:
    /// the same sequential height accumulation from `min_row`).
    pub(crate) y0: f64,
    /// Page content height (the sequential sum of the page's row heights).
    pub(crate) content_h: f64,
}

/// A sheet's full pagination plan: bands + page row-ranges + the height/image
/// metadata each page fill needs. O(#exceptions + #pages + #cols) storage.
pub(crate) struct SheetPlan {
    pub(crate) name: String,
    pub(crate) show_grid_lines: bool,
    pub(crate) min_row: u32,
    pub(crate) min_col: u32,
    /// Range bounds AFTER the drawing extension.
    pub(crate) max_row: u32,
    pub(crate) max_col: u32,
    pub(crate) default_row_px: f64,
    /// Height exceptions (row, px), sorted by row, duplicates last-wins.
    pub(crate) row_hx: Vec<(u32, f64)>,
    pub(crate) bands: Vec<ColBand>,
    /// Absolute (content-space) anchored-picture boxes.
    pub(crate) abs_images: Vec<PageImage>,
    /// Absolute (content-space) anchored-CHART boxes (embedded charts).
    pub(crate) abs_charts: Vec<PageImage>,
    pub(crate) merges: Vec<MergeRegion>,
    pub(crate) pages: Vec<PlanPage>,
}

impl SheetPlan {
    /// The explicit height of `row` in px, else the sheet default — the same
    /// value the eager walk's `row_h.get(&r).copied().unwrap_or(default)` gave.
    fn height(&self, row: u32) -> f64 {
        let i = self.row_hx.partition_point(|&(er, _)| er < row);
        if i < self.row_hx.len() && self.row_hx[i].0 == row {
            self.row_hx[i].1
        } else {
            self.default_row_px
        }
    }

    /// A SKELETON `SheetPage` for streaming at load: correct identity
    /// (sheet name → `sheet_spans`), band geometry extents and flags, but NO
    /// rails/cells/merges — those fill on first paint via `fill_plan_page`.
    pub(crate) fn skeleton_page(&self, page_no: usize) -> SheetPage {
        let pp = &self.pages[page_no];
        let band = &self.bands[pp.band];
        SheetPage {
            sheet_name: self.name.clone(),
            col_rails: Vec::new(),
            row_rails: Vec::new(),
            cells: Vec::new(),
            show_grid_lines: self.show_grid_lines,
            content_w: band.content_w,
            content_h: pp.content_h,
            merges: Vec::new(),
            first_col: band.min_col,
            row_numbers: Vec::new(),
            images: Vec::new(),
            charts: Vec::new(),
        }
    }
}

/// Compute a sheet's pagination plan from metadata alone — the §18.3.1.63
/// downThenOver walk over NUMBERS, not cells. This is the load-bearing
/// extraction behind both [`paginate`] (eager, model-fed) and the lazy
/// path (index-fed): page row-ranges fall out of one sequential walk over the
/// row heights (default runs + `@ht` exceptions — the EXACT `used + h >
/// page.height` accumulation the original single-pass paginate ran, so
/// boundaries are bit-identical); column bands from the §18.3.1.13 col runs;
/// the drawing extension from the anchor metadata. No cell is touched.
pub(crate) fn plan_pages(input: PlanInput, page: PageBox) -> SheetPlan {
    // An EMPTY sheet (no populated rows → `dims` None, §18.3.1.35)
    // synthesizes a default used range instead of yielding zero pages. Zero
    // pages would mean the sheet never appeared in `sheet_spans` — its tab would
    // vanish (a blank sheet would read as "missing"). A blank default grid
    // gives the sheet a page + tab, matching Excel's empty-grid presentation.
    // `continuous_page` routes through here too, so the grid view gets the same
    // synthesized extent.
    let (min_row, min_col, mut max_row, mut max_col) = match input.dims {
        Some(d) => d,
        None => (1, 1, EMPTY_SHEET_DEFAULT_ROWS, EMPTY_SHEET_DEFAULT_COLS),
    };

    // ── Row heights across the used range (§18.3.1.73 / §18.3.1.81) ──────────
    let default_row_px = pt_to_px(input.default_row_height_pt);
    let mut row_h: std::collections::HashMap<u32, f64> = std::collections::HashMap::new();
    for &(r, hpt) in &input.row_heights_pt {
        row_h.insert(r, pt_to_px(hpt));
    }

    // ── Anchored pictures → absolute pixel boxes (§20.5.2.33/.24) ────────────
    // Grid-absolute coordinates: x=0 at min_col's left edge, y=0 at min_row's
    // top (the rails' origin). `col_left`/`row_top` give a 1-based column/row's
    // leading edge — columns/rows outside the used range fall back to the sheet
    // defaults, exactly like the band/rail walk below, so an anchor beyond the
    // data range still lands where Excel puts it.
    // Hidden columns contribute EXACTLY 0px (hidden-gated: hidden-less sheets
    // run the identical chars→px conversion).
    let col_px = |c: u32| {
        if (input.col_hidden)(c) {
            0.0
        } else {
            chars_to_px((input.col_width_chars)(c), input.mdw)
        }
    };
    let col_left = |c: u32| -> f64 {
        if c >= min_col {
            (min_col..c).map(col_px).sum()
        } else {
            -(c..min_col).map(col_px).sum::<f64>()
        }
    };
    let row_top = |r: u32| -> f64 {
        if r >= min_row {
            (min_row..r).map(|rr| row_h.get(&rr).copied().unwrap_or(default_row_px)).sum()
        } else {
            -(r..min_row).map(|rr| row_h.get(&rr).copied().unwrap_or(default_row_px)).sum::<f64>()
        }
    };
    // xdr anchors are 0-based, so `from.col + 1` is the 1-based column the
    // picture starts in. twoCellAnchor size comes from the to-rail (from→to
    // span); oneCellAnchor (and a degenerate to-rail) uses the EMU extent.
    let to_box = |d: &SheetDrawing| -> Option<PageImage> {
        let x0 = col_left(d.from.col + 1) + emu_to_px(d.from.col_off_emu);
        let y0 = row_top(d.from.row + 1) + emu_to_px(d.from.row_off_emu);
        let from_to = d.to.map(|t| {
            (
                col_left(t.col + 1) + emu_to_px(t.col_off_emu) - x0,
                row_top(t.row + 1) + emu_to_px(t.row_off_emu) - y0,
            )
        });
        let (w, h) = match from_to {
            Some((w, h)) if w > 0.0 && h > 0.0 => (w, h),
            _ => {
                let (cx, cy) = d.ext_emu?;
                (emu_to_px(cx), emu_to_px(cy))
            }
        };
        Some(PageImage { x: x0, y: y0, w, h, key: d.image_key.clone() })
    };
    let abs_images: Vec<PageImage> = input.drawings.iter().filter_map(to_box).collect();
    // Anchored charts: the SAME box math (embedded charts share the picture
    // anchor geometry); kept separate so pages route them to the chart painter.
    let abs_charts: Vec<PageImage> = input.charts.iter().filter_map(to_box).collect();
    // Extend the used range so the grid (rails + pages) covers every picture
    // AND chart — Excel's grid extends past the data under a floating drawing.
    // Incremental right/bottom edges keep this O(extension), capped at the
    // sheet limits.
    {
        let mut right = col_left(max_col + 1);
        let mut bottom = row_top(max_row + 1);
        for im in abs_images.iter().chain(abs_charts.iter()) {
            let (x, y, w, h) = (im.x, im.y, im.w, im.h);
            while right < x + w - 0.01 && max_col < 16_384 {
                max_col += 1;
                right += col_px(max_col);
            }
            while bottom < y + h - 0.01 && max_row < 1_048_576 {
                max_row += 1;
                bottom += row_h.get(&max_row).copied().unwrap_or(default_row_px);
            }
        }
    }

    // ── Column bands (§18.3.1.13 widths, page-width tiling) ──────────────────
    // The row gutter (left band, holds row numbers) consumes page width, so the
    // usable width for columns is the page width minus the gutter. The gutter
    // width depends only on the digit count of the largest row number, which is
    // page-independent (max_row across the sheet), so it is computed once here.
    let row_gutter_w = row_gutter_width_for_max_row(max_row);
    let usable_w = (page.width - row_gutter_w).max(1.0);
    let mut bands = split_col_bands(&col_px, min_col, max_col, usable_w);
    for band in &mut bands {
        // The band's absolute left edge (picture re-basing): cumulative
        // width of the columns before the band's first column.
        band.x0 = col_left(band.min_col);
    }

    // ── The row walk (§18.3.1.63): page row-ranges from heights alone ────────
    // EXACT mirror of the original row loop: rows accumulate `used += h` and a
    // row that would overflow `page.height` (on a non-empty page) starts the
    // next page. The walk is over NUMBERS only — one pass, shared by every
    // band (heights are band-independent, so the ranges are too; the original
    // re-walked identically per band). Heights come from the sorted exception
    // list via a merge pointer (same values as the `row_h` map).
    let mut exc: Vec<(u32, f64)> = row_h.iter().map(|(&r, &h)| (r, h)).collect();
    exc.sort_unstable_by_key(|&(r, _)| r);
    let mut ranges: Vec<(u32, u32, f64, f64)> = Vec::new(); // (r0, r1, y0, content_h)
    {
        let mut ei = exc.partition_point(|&(r, _)| r < min_row);
        let mut start = min_row;
        let mut used = 0.0;
        let mut y_start = 0.0;
        // Running absolute y — sequential adds of the same heights row_top
        // sums, so `y_abs` at row r is bit-identical to `row_top(r)`.
        let mut y_abs = 0.0;
        for r in min_row..=max_row {
            let h = if ei < exc.len() && exc[ei].0 == r {
                let h = exc[ei].1;
                ei += 1;
                h
            } else {
                default_row_px
            };
            // Paginate: if adding this row overflows the page (and the page
            // already has at least one row), close it first. A single oversized
            // row still gets its own page rather than looping forever.
            if r > start && used + h > page.height {
                ranges.push((start, r - 1, y_start, used));
                start = r;
                y_start = y_abs;
                used = 0.0;
            }
            used += h;
            y_abs += h;
        }
        if min_row <= max_row {
            ranges.push((start, max_row, y_start, used));
        }
    }

    // downThenOver (§18.3.1.63 default): ALL row-chunks of band 1
    // (top-to-bottom) precede band 2's, etc.
    let mut pages_out = Vec::with_capacity(bands.len() * ranges.len());
    for bi in 0..bands.len() {
        for &(r0, r1, y0, content_h) in &ranges {
            pages_out.push(PlanPage { band: bi, r0, r1, y0, content_h });
        }
    }

    SheetPlan {
        name: input.name.to_string(),
        show_grid_lines: input.show_grid_lines,
        min_row,
        min_col,
        max_row,
        max_col,
        default_row_px,
        row_hx: exc,
        bands,
        abs_images,
        abs_charts,
        merges: input.merges,
        pages: pages_out,
    }
}

/// Materialize ONE planned page into a full `SheetPage` — the original
/// `paginate` flush body, fed by a row-content lookup instead of a whole-sheet
/// map. `row_cells(r)` returns row `r`'s cells (eager: the model's row map;
/// lazy: the chunk LRU via `RowsView::lookup`) or `None` for an
/// absent row (it still occupies its rail — a sparse sheet is not a collapsed
/// sheet).
pub(crate) fn fill_plan_page<'a>(
    plan: &SheetPlan,
    page_no: usize,
    mut row_cells: impl FnMut(u32) -> Option<&'a [CellModel]>,
) -> SheetPage {
    let pp = &plan.pages[page_no];
    let band = &plan.bands[pp.band];
    let col_x = &band.col_x;

    // ── Merge bookkeeping (§18.3.1.55), clipped to THIS page's rows ─────────
    // `covered` is every cell of every region EXCEPT its anchor — those cells
    // are suppressed (the region renders as one box). `anchor_of` maps a
    // region's top-left to its region so the anchor cell can expand to span the
    // full extent. Only regions intersecting the page contribute (the original
    // built these globally; membership is only ever tested for page rows, so
    // the clipped sets answer identically — and a million-row merge no longer
    // costs a million inserts per page).
    //
    // Clipped on the COLUMN axis too. Membership is only ever tested after
    // the `band.min_col..=band.max_col` guard in the cell loop below, so the
    // clipped set answers identically, and the LEGITIMATE `A1:XFD1048576` merge
    // (Excel's select-all) stops costing `page_rows x 16 384` inserts per page.
    // The region is untouched — only the bookkeeping is bounded by the band.
    let mut covered: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
    let mut anchor_of: std::collections::HashMap<(u32, u32), MergeRegion> =
        std::collections::HashMap::new();
    for region in &plan.merges {
        if region.max_row < pp.r0 || region.min_row > pp.r1 {
            continue;
        }
        anchor_of.insert((region.min_row, region.min_col), *region);
        for rr in region.min_row.max(pp.r0)..=region.max_row.min(pp.r1) {
            for cc in region.min_col.max(band.min_col)..=region.max_col.min(band.max_col) {
                if !region.is_anchor(rr, cc) {
                    covered.insert((rr, cc));
                }
            }
        }
    }

    // A merge crossing a page break truncates at the page bottom (§18.3.1.55
    // page-boundary DECISION: clamp to the start page, overflow rows are not
    // re-emitted; full cross-page spanning is DEFERRED).
    let last_row_on_page = pp.r1;
    let mut row_rails = Vec::with_capacity((pp.r1 - pp.r0 + 2) as usize);
    let mut cells = Vec::new();
    let mut merges: Vec<MergeBox> = Vec::new();
    let mut y = 0.0;
    row_rails.push(0.0);
    for row_num in pp.r0..=pp.r1 {
        let h = plan.height(row_num);
        if let Some(row) = row_cells(row_num) {
            for cm in row {
                // Only cells whose column falls in THIS band paint on its pages.
                if cm.col < band.min_col || cm.col > band.max_col {
                    continue;
                }
                // Covered (non-anchor) cells of a merge are suppressed entirely.
                if covered.contains(&(row_num, cm.col)) {
                    continue;
                }
                let k = (cm.col - band.min_col) as usize;
                let cx = col_x[k];
                let mut cw = col_x[k + 1] - col_x[k];
                let mut ch = h;
                let mut is_merged_anchor = false;

                if let Some(region) = anchor_of.get(&(row_num, cm.col)) {
                    is_merged_anchor = true;
                    // Width: span columns up to the merge's right edge, clamped to
                    // THIS band's right edge: a merge crossing a band
                    // boundary truncates here; its continuation in the next band
                    // stays blank (cross-band spanning DEFERRED — a simplification).
                    let span_max_col = region.max_col.min(band.max_col);
                    let k_end = (span_max_col - band.min_col + 1) as usize;
                    cw = col_x[k_end] - col_x[k];
                    // Height: span rows up to the last row on THIS page (the
                    // page-boundary clamp). Sum each spanned row's height.
                    let span_max_row = region.max_row.min(last_row_on_page);
                    let mut summed = 0.0;
                    for rr in row_num..=span_max_row {
                        // A spanned row's height: its explicit @ht, else default.
                        summed += plan.height(rr);
                    }
                    ch = summed;
                }
                // A cell whose box collapsed to zero (its row/col is HIDDEN and
                // no merge spans a visible member) emits NOTHING — no degenerate
                // draw calls, no spill occupancy (text spills ACROSS hidden
                // columns, like Excel). Zero boxes arise from HIDDEN members
                // OR an explicit ht="0" exception — Excel displays both as
                // hidden, so skipping either is faithful.
                if cw <= 0.0 || ch <= 0.0 {
                    continue;
                }
                if is_merged_anchor {
                    merges.push(MergeBox { x: cx, y, w: cw, h: ch });
                }

                cells.push(PageCell {
                    x: cx,
                    y,
                    w: cw,
                    h: ch,
                    text: cm.text.clone(),
                    align: cm.align,
                    style: cm.style.clone(),
                    ref_a1: format!("{}{}", col_letters(cm.col), row_num),
                    raw: cm.raw.clone(),
                    is_merged_anchor,
                });
            }
        }
        y += h;
        row_rails.push(y);
    }

    // Anchored pictures intersecting THIS page: the page's absolute
    // window is [band.x0, band.x0+content_w) × [page_y0, page_y0+height);
    // an intersecting picture keeps its FULL box, re-based to page-local
    // coordinates (the painter clips at the page content rect, so a
    // picture spanning a page break shows its correct slice on each page).
    let page_y0 = pp.y0;
    let page_y1 = page_y0 + y;
    let band_x1 = band.x0 + band.content_w;
    let clip_to_page = |abs: &[PageImage]| -> Vec<PageImage> {
        let mut out: Vec<PageImage> = Vec::new();
        for im in abs {
            if im.x < band_x1 && im.x + im.w > band.x0 && im.y < page_y1 && im.y + im.h > page_y0 {
                out.push(PageImage {
                    x: im.x - band.x0,
                    y: im.y - page_y0,
                    w: im.w,
                    h: im.h,
                    key: im.key.clone(),
                });
            }
        }
        out
    };
    let images = clip_to_page(&plan.abs_images);
    // Anchored charts intersecting THIS page — the same re-basing as pictures.
    let charts = clip_to_page(&plan.abs_charts);
    // Gutter labels: the page's first column number (= min_col for
    // the rows-only pagination; the band start under tiling) plus one row
    // number per row band, parallel to the `row_rails` gaps.
    let row_numbers: Vec<u32> = (pp.r0..=pp.r1).collect();
    SheetPage {
        sheet_name: plan.name.clone(),
        col_rails: col_x.clone(),
        row_rails,
        cells,
        show_grid_lines: plan.show_grid_lines,
        content_w: band.content_w,
        content_h: y,
        merges,
        first_col: band.min_col,
        row_numbers,
        images,
        charts,
    }
}

/// One sheet's contiguous run of pages in the flat streamed page vec.
/// `first_page` is the index of the sheet's first page; `page_count` the number
/// of pages it owns. Drives the viewer's sheet-tab strip.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SheetSpan {
    pub(crate) name: String,
    pub(crate) first_page: usize,
    pub(crate) page_count: usize,
}

/// Run-length group a flat page vec by `sheet_name` into per-sheet spans.
/// Pages stream in document order, sheets back-to-back (`paginate_all` extends
/// per sheet, no interleaving), so consecutive-run grouping is exact. Empty
/// sheets produce zero pages and never appear here. Pure (no wasm/web-sys) so
/// the grouping is natively unit-testable; the lib.rs `sheetInfo` facade reuses
/// this exact logic over the shared `sml_pages`.
pub(crate) fn sheet_spans(pages: &[SheetPage]) -> Vec<SheetSpan> {
    let mut out: Vec<SheetSpan> = Vec::new();
    let mut i = 0usize;
    while i < pages.len() {
        let name = &pages[i].sheet_name;
        let first = i;
        while i < pages.len() && &pages[i].sheet_name == name {
            i += 1;
        }
        out.push(SheetSpan {
            name: name.clone(),
            first_page: first,
            page_count: i - first,
        });
    }
    out
}

/// Deterministic FNV-1a geometry hash over a sheet's pages: every rail and every
/// cell box (rounded to 0.01px) plus the cell text. Mirrors the oracle
/// geometry-hash precedent so a pinned baseline catches sub-pixel drift.
pub(crate) fn geometry_hash(pages: &[SheetPage]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h = FNV_OFFSET;
    let byte = |h: &mut u64, b: u8| {
        *h ^= b as u64;
        *h = h.wrapping_mul(FNV_PRIME);
    };
    let feed_f = |h: &mut u64, v: f64| {
        // Round to 0.01px and hash the integer hundredths (stable across runs).
        let q = (v * 100.0).round() as i64;
        for b in q.to_le_bytes() {
            byte(h, b);
        }
    };
    let feed_s = |h: &mut u64, s: &str| {
        for b in s.as_bytes() {
            byte(h, *b);
        }
        byte(h, 0);
    };
    for (pi, p) in pages.iter().enumerate() {
        feed_f(&mut h, pi as f64);
        feed_s(&mut h, &p.sheet_name);
        byte(&mut h, p.show_grid_lines as u8);
        // Gutter label fields: the page's first column number and its
        // per-band row numbers, so a label-geometry change re-pins the baseline.
        feed_f(&mut h, p.first_col as f64);
        byte(&mut h, b'R');
        for rn in &p.row_numbers {
            feed_f(&mut h, *rn as f64);
        }
        byte(&mut h, b'|');
        for r in &p.col_rails {
            feed_f(&mut h, *r);
        }
        byte(&mut h, b'|');
        for r in &p.row_rails {
            feed_f(&mut h, *r);
        }
        byte(&mut h, b'|');
        for c in &p.cells {
            feed_f(&mut h, c.x);
            feed_f(&mut h, c.y);
            feed_f(&mut h, c.w);
            feed_f(&mut h, c.h);
            byte(&mut h, c.align as u8);
            byte(&mut h, c.is_merged_anchor as u8);
            feed_s(&mut h, &c.text);
            // Fold the resolved per-cell style so a style change is caught
            // by the pinned geometry hash (fill/border/font/numfmt drift).
            if let Some(st) = &c.style {
                byte(&mut h, 1);
                st.hash_into(&byte, &feed_f, &feed_s, &mut h);
            } else {
                byte(&mut h, 0);
            }
        }
        // Fold merged-region boxes (§18.3.1.55) so a merge geometry change is
        // caught by the pinned baseline (covered-cell emission + anchor span).
        byte(&mut h, b'M');
        for m in &p.merges {
            feed_f(&mut h, m.x);
            feed_f(&mut h, m.y);
            feed_f(&mut h, m.w);
            feed_f(&mut h, m.h);
        }
        // Fold anchored-picture boxes so image-anchor drift is caught.
        // GUARDED on non-empty: pages without drawings hash as if pictures did
        // not exist, keeping drawing-free pinned baselines (oracle + renderer) valid.
        if !p.images.is_empty() {
            byte(&mut h, b'I');
            for im in &p.images {
                feed_f(&mut h, im.x);
                feed_f(&mut h, im.y);
                feed_f(&mut h, im.w);
                feed_f(&mut h, im.h);
                feed_s(&mut h, &im.key);
            }
        }
        // Fold anchored-CHART boxes (embedded charts) — same non-empty guard,
        // so chart-less pages hash exactly as before (pins stay valid).
        if !p.charts.is_empty() {
            byte(&mut h, b'C');
            for im in &p.charts {
                feed_f(&mut h, im.x);
                feed_f(&mut h, im.y);
                feed_f(&mut h, im.w);
                feed_f(&mut h, im.h);
                feed_s(&mut h, &im.key);
            }
        }
        byte(&mut h, b'\n');
    }
    h
}

/// Header-gutter dimensions for a page: `(row_gutter_w, col_gutter_h)`
/// in px. The LEFT gutter holds row numbers; its width grows with the digit
/// count of the largest row number on the page so 1_000_000 never clips. The
/// TOP gutter holds A/B/C column letters at a fixed Excel-like height. Pure
/// (no web-sys) so BOTH `render::paint_sheet_page` and `SmlRenderer::cell_at`
/// derive the same origin shift from the borrowed page — there is exactly one
/// place the gutter geometry is defined.
pub(crate) fn gutter_dims(page: &SheetPage) -> (f64, f64) {
    let max_row = page.row_numbers.iter().copied().max().unwrap_or(1);
    (row_gutter_width_for_max_row(max_row), COL_GUTTER_H)
}

/// Fixed TOP gutter height (column-letter band) in px (Excel-like at 96dpi).
/// MINIMAL: hugs the 11px label with ~1px each side — no dead vertical space
/// (it scales with the cells under zoom, so every excess px is amplified).
/// (`pub(crate)`: the Grid-v2 index's closed-form `extent_px` adds the same
/// gutter the materialized `grid_extent` adds via `gutter_dims`.)
pub(crate) const COL_GUTTER_H: f64 = 13.0;

/// LEFT (row-number) gutter width in px for a given largest row number: grows
/// with the digit count so 1_000_000 never clips. Shared by both
/// `gutter_dims` (paint + cellAt, per-page max row) and `paginate`'s column-band
/// width budget (the gutter consumes page width; computed once from the sheet's
/// max_row so every band's usable width is consistent). Single source of
/// truth: change the formula here and both call sites follow. MINIMAL: just the
/// 11px digits (~6.5px each) + a 4px total margin — no dead horizontal space.
pub(crate) fn row_gutter_width_for_max_row(max_row: u32) -> f64 {
    let digits = max_row.max(1).to_string().len() as f64;
    (4.0 + digits * 6.5).max(16.0)
}

/// 1-based column number → A1 letters (inverse of `model::parse_a1`'s column).
pub(crate) fn col_letters(mut n: u32) -> String {
    let mut s = String::new();
    while n > 0 {
        let r = ((n - 1) % 26) as u8;
        s.insert(0, (b'A' + r) as char);
        n = (n - 1) / 26;
    }
    s
}

/// Build ONE unbounded page spanning the whole sheet — the continuous
/// (virtual-scroll) grid view's geometry. Same code path as print pagination
/// with an effectively-infinite page box, so rails/cells/merges/gutter data
/// are identical to the paged view's first band, just never split.
pub(crate) fn continuous_page(model: &super::model::SheetModel) -> Option<SheetPage> {
    paginate(model, PageBox { width: 1.0e12, height: 1.0e12 }).into_iter().next()
}

/// Index range of row bands intersecting the vertical window
/// `[y0, y1)` (content-local px). Bands are the gaps between `row_rails`.
pub(crate) fn visible_row_bands(page: &SheetPage, y0: f64, y1: f64) -> (usize, usize) {
    let rails = &page.row_rails;
    if rails.len() < 2 {
        return (0, 0);
    }
    // First band whose bottom rail is past y0; last band whose top rail is before y1.
    let start = rails.partition_point(|&r| r <= y0).saturating_sub(1);
    let end = rails.partition_point(|&r| r < y1).min(rails.len() - 1);
    (start.min(end), end)
}

/// Cells intersecting the row-band window — cells are pushed in row order, so a
/// y-window maps to a contiguous slice found by binary search on `cell.y`.
pub(crate) fn visible_cells(page: &SheetPage, y0: f64, y1: f64) -> &[PageCell] {
    let cells = &page.cells;
    // First cell whose bottom edge is past y0 may start earlier than its y due
    // to merged anchors spanning down — widen the start by scanning back over
    // anchors. Start from the binary-search lower bound on y.
    let mut start = cells.partition_point(|c| c.y + c.h <= y0);
    while start > 0 {
        let p = &cells[start - 1];
        if p.is_merged_anchor && p.y + p.h > y0 {
            start -= 1;
        } else {
            break;
        }
    }
    let end = cells.partition_point(|c| c.y < y1);
    &cells[start.min(end)..end]
}
