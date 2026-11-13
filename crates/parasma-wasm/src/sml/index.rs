//! Grid v2 — the lazy `SheetIndex`: a per-sheet index that lets the grid
//! view address any row without materializing the whole sheet.
//!
//! One cheap O(bytes) pre-scan per worksheet replaces every O(sheet) step the
//! materialized (v1) grid path pays:
//!
//!  - **Pre-scan**: a single linear pass over the raw worksheet XML's
//!    `<sheetData>` span recording, per PRESENT row (sparse — a stamped-1M but
//!    nearly-empty sheet indexes a handful of entries): the row number (`@r`,
//!    or the implied previous+1 when absent — §18.3.1.73 r-less .NET streams,
//!    the same rule `SheetModel::from_resolved` applies), the byte offset of
//!    the `<row` start tag into the retained XML, the byte end just past the
//!    row's close (the window-slice seam), the `@ht` height exception, and the
//!    row's populated-column bounds from its `<c r="…">` refs.
//!  - **Husk parse**: the `<sheetData>…</sheetData>` content is located
//!    quote-aware (the `repair::xml_scan` discipline: quoted attribute values,
//!    comments, CDATA, PIs are all honored) and spliced OUT; the remaining
//!    "husk" — the original `<worksheet>` start/end tags and every metadata
//!    sibling, with an empty `sheetData` — parses through the EXISTING fork
//!    `parse_worksheet` (ext.rs seam, the same entry the `<col>` shim trusts).
//!    NO fork changes. This yields cols/§18.3.1.13, sheetFormatPr/§18.3.1.81,
//!    sheetViews/§18.3.1.88, mergeCells/§18.3.1.55 and the stamped
//!    dimension/§18.3.1.35 — everything `SheetModel` reads except rows. A
//!    `MissingAttribute` husk routes through the same `repair::sml_col_attrs`
//!    rewrite as the shim's Layer 2.
//!  - **Closed-form mappings**: [`IndexGeom`] answers extent, row→y,
//!    y→row, col→x and x→col with NO row walk — default-height runs are
//!    arithmetic, `@ht` exceptions contribute via prefix sums at the
//!    exceptions (O(log #exceptions) per query).
//!
//! Status: LIVE. `grid_extent`, `paint_grid_viewport` and
//! `grid_cell_at` all consume this index via `ensure_geom` (`mod.rs`), with
//! row content materialized per-chunk through `window::ChunkStore` (LRU).
//! The native equivalence tests against the materialized v1
//! `continuous_page` truth are the correctness net: they assert that the
//! index reproduces the eager walk's geometry and row content for the same
//! workbook.
//!
//! FP note (documented design point): `IndexGeom` computes default-height runs
//! by MULTIPLICATION (the closed form), while v1's `paginate` accumulates
//! row-by-row. For heights that are not exactly representable in binary
//! (e.g. `ht="22"` → 88/3 px) the two can differ by ~1 ulp (≈1e-14 relative) —
//! a million times below the geometry hash's 0.01px quantum, and invisible to
//! the equivalence invariant. The index tests assert bit-exactness where the
//! heights are exact (every default-15pt fixture) and a 1e-6px bound otherwise.

use std::ops::Range;

use ooxml_sml::ext::{parse_worksheet, ResolvedSheet};
use ooxml_sml::parsers::ParseError;

use crate::engine::units::{emu_to_px, pt_to_px};
use crate::repair::xml_scan::find_tag_end;

use super::drawings::SheetDrawing;
use super::grid::{
    chars_to_px, row_gutter_width_for_max_row, PageImage, COL_GUTTER_H, EMPTY_SHEET_DEFAULT_COLS,
    EMPTY_SHEET_DEFAULT_ROWS,
};
use super::model::{
    collect_col_runs, collect_merges, frozen_pane, grid_lines_enabled, parse_merge_ref,
    sheet_defaults, ColRun, MergeRegion, MAX_ADDRESSABLE_ROW,
};

/// One PRESENT `<row>` in the sheetData span (sparse: absent rows have no
/// entry). Byte offsets address the retained worksheet XML.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RowEntry {
    /// 1-based row number: `@r`, or implied previous+1 (§18.3.1.73).
    pub(crate) row: u32,
    /// Offset of the `<row` start-tag byte.
    pub(crate) byte_off: u32,
    /// Offset just past the row's closing `>` (self-closing `/>` or `</row>`).
    /// `xml[byte_off..byte_end]` is the complete row element — the window slice.
    pub(crate) byte_end: u32,
    /// Whether `@r` was explicit (the window envelope builder stamps `r` on a
    /// slice's first row when implied numbering would desynchronize).
    pub(crate) explicit_r: bool,
    /// `@ht` in points (§18.3.1.73) — a height exception. `None` → default.
    pub(crate) ht_pt: Option<f64>,
    /// `@hidden` (§18.3.1.73), or the Excel equivalence `ht="0"` +
    /// `customHeight`. A hidden row joins the exception path with height 0.
    pub(crate) hidden: bool,
    /// Min/max 1-based column over the row's `<c r="…">` refs; 0 = none.
    /// (Cells without `@r` are skipped — mirroring the fork's
    /// `column_number()`-gated walk, which v1 geometry already follows.)
    pub(crate) min_col: u32,
    pub(crate) max_col: u32,
}

/// The per-sheet lazy index: retained XML + sheetData span + sparse row
/// entries + the husk-parsed metadata.
pub(crate) struct SheetIndex {
    pub(crate) name: String,
    /// The decompressed worksheet XML, retained (a deliberate memory trade —
    /// strictly smaller than the typed-struct materialization it replaces).
    pub(crate) xml: Vec<u8>,
    /// Byte span of the sheetData CONTENT (between the start tag's `>` and
    /// `</sheetData>`). Empty for `<sheetData/>` or a missing sheetData.
    pub(crate) sheet_data: Range<usize>,
    /// Present rows in document order.
    pub(crate) rows: Vec<RowEntry>,

    // ── husk-parsed metadata (everything SheetModel reads except rows) ──────
    /// The stamped `<dimension ref>` (§18.3.1.35) — the FILE's claim, kept
    /// distinct from the scanned truth (dimension-distrust).
    pub(crate) stamped_dimension: Option<(u32, u32, u32, u32)>,
    pub(crate) col_runs: Vec<ColRun>,
    pub(crate) default_row_height_pt: f64,
    pub(crate) default_col_width_chars: f64,
    pub(crate) show_grid_lines: bool,
    pub(crate) merges: Vec<MergeRegion>,
    /// The sheet's FROZEN pane (§18.3.1.66; grid view only). `None` for the
    /// overwhelmingly common pane-less sheet AND for `state="split"` — every
    /// pane consumer is gated on this, so pane-less paths are byte-identical.
    pub(crate) pane: Option<super::pane::PaneInfo>,
    /// Whether the husk needed the `<col>` repair (degraded widths).
    pub(crate) col_widths_degraded: bool,

    // ── scan-derived ranges ──────────────────────────────────────────────────
    /// EXACT mirror of the fork's present-rows-derived `dimensions()`
    /// (ext.rs): row bounds over EXPLICIT-`@r` rows only, col bounds over
    /// `@r`-carrying cells; `None` when no explicit row exists. This is what
    /// v1's `model.dimensions` feeds `paginate`, so it is the v1 equivalence
    /// range. (Its quirks — r-less rows and the stamp are IGNORED — are v1
    /// behavior; dimension-distrust supersedes it in v2 via `content_bounds`.)
    pub(crate) used_range_v1: Option<(u32, u32, u32, u32)>,
    /// Scanned content truth: bounds over rows that actually carry `@r`
    /// cells (row numbers explicit OR implied). `None` when no cells exist.
    /// The effective v2 extent is `max(stamped, content, kill-the-void)`.
    pub(crate) content_bounds: Option<(u32, u32, u32, u32)>,
}

impl SheetIndex {
    /// Build the index from a sheet's raw (decompressed, possibly
    /// package-repaired) worksheet XML. O(bytes): one span scan + one row
    /// scan + the husk parse (tiny — sheetData is spliced out).
    pub(crate) fn build(name: String, xml: Vec<u8>) -> Result<SheetIndex, String> {
        let span = find_sheet_data(&xml);
        let (content, husk) = match &span {
            Some(sd) if !sd.self_closing => (
                sd.content.clone(),
                // Husk = everything before the content + everything from the
                // end tag on: the ORIGINAL start/end tags survive byte-for-byte
                // (namespace prefixes and attributes preserved).
                [&xml[..sd.content.start], &xml[sd.content.end..]].concat(),
            ),
            // Self-closing or absent sheetData: the XML already IS the husk.
            _ => (0..0, xml.clone()),
        };

        // Husk parse through the EXISTING fork seam; a MissingAttribute routes
        // through the same render-only <col> repair as the shim's Layer 2.
        let (worksheet, col_widths_degraded) = match parse_worksheet(&husk) {
            Ok(ws) => (ws, false),
            Err(ParseError::MissingAttribute(_)) => {
                let repaired = crate::repair::sml_col_attrs(&husk);
                match parse_worksheet(&repaired) {
                    Ok(ws) => (ws, true),
                    Err(e) => return Err(format!("husk parse failed after repair: {e}")),
                }
            }
            Err(e) => return Err(format!("husk parse failed: {e}")),
        };

        let stamped_dimension = worksheet
            .dimension
            .as_deref()
            .and_then(|d| parse_merge_ref(&d.reference))
            .map(|r| (r.min_row, r.min_col, r.max_row, r.max_col));

        // Reuse the model's metadata extractors verbatim over a husk-backed
        // ResolvedSheet (no shared strings needed — the husk has no cells).
        let rs = ResolvedSheet::new(name.clone(), worksheet, Vec::new());
        let col_runs = collect_col_runs(&rs);
        let (default_row_height_pt, default_col_width_chars) = sheet_defaults(&rs);
        let show_grid_lines = grid_lines_enabled(&rs);
        let merges = collect_merges(&rs);
        let pane = frozen_pane(&rs);

        let rows = scan_rows(&xml, content.clone());

        // ── used_range_v1: EXACT mirror of fork dimensions() ────────────────
        // DEVIATION (deliberate): rows outside the addressable grid are
        // EXCLUDED from both range folds below. `<row @r>` (§18.3.1.73) is a bare
        // integer — the one worksheet coordinate that never passes through an A1
        // parse — and the fork folds it raw, so `<row r="4000000000">` propagates
        // into `v1_range`/`effective_range`/`print_range`/`paginate`, each of
        // which walks `min_row..=max_row` one row at a time. The row is DROPPED,
        // not snapped to the last grid row: snapping would invent a million rows
        // of pagination out of one bogus attribute. Its `RowEntry` is kept so the
        // byte offsets and the implied-row chain stay honest; it simply falls
        // outside every range, so nothing lays it out. See `MAX_ADDRESSABLE_ROW`.
        // The COLUMN axis needs no fold-site check: every column bound here is
        // cell-derived through `parse_a1`, which is already bounded.
        let used_range_v1 = if rows.is_empty() {
            None
        } else {
            let mut min_row = u32::MAX;
            let mut max_row = 0u32;
            let mut min_col = u32::MAX;
            let mut max_col = 0u32;
            for e in &rows {
                if e.explicit_r && e.row <= MAX_ADDRESSABLE_ROW {
                    min_row = min_row.min(e.row);
                    max_row = max_row.max(e.row);
                }
                if e.max_col > 0 {
                    min_col = min_col.min(e.min_col);
                    max_col = max_col.max(e.max_col);
                }
            }
            // The fork returns None only when NO explicit @r row exists; the
            // col bounds pass through unchecked (mirrored exactly).
            (min_row != u32::MAX).then_some((min_row, min_col, max_row, max_col))
        };

        // ── content bounds: scanned truth over cell-bearing rows ───────────
        let mut cb: Option<(u32, u32, u32, u32)> = None;
        for e in rows.iter().filter(|e| e.max_col > 0 && e.row <= MAX_ADDRESSABLE_ROW) {
            cb = Some(match cb {
                None => (e.row, e.min_col, e.row, e.max_col),
                Some((r0, c0, r1, c1)) => {
                    (r0.min(e.row), c0.min(e.min_col), r1.max(e.row), c1.max(e.max_col))
                }
            });
        }

        Ok(SheetIndex {
            name,
            xml,
            sheet_data: content,
            rows,
            stamped_dimension,
            col_runs,
            default_row_height_pt,
            default_col_width_chars,
            show_grid_lines,
            merges,
            pane,
            col_widths_degraded,
            used_range_v1,
            content_bounds: cb,
        })
    }

    /// File width (character units) of column `col` — first matching `<col>`
    /// run with a width wins, else the sheet default. EXACT mirror of
    /// `SheetModel::col_width_chars` (the v1 geometry's lookup).
    pub(crate) fn col_width_chars(&self, col: u32) -> f64 {
        for run in &self.col_runs {
            if col >= run.min && col <= run.max {
                if let Some(w) = run.width_chars {
                    return w;
                }
            }
        }
        self.default_col_width_chars
    }

    /// Whether column `col` is HIDDEN (§18.3.1.13 `@hidden` / width-0 +
    /// customWidth). EXACT mirror of `SheetModel::col_hidden`.
    pub(crate) fn col_hidden(&self, col: u32) -> bool {
        self.col_runs.iter().any(|r| col >= r.min && col <= r.max && r.hidden)
    }

    /// The PIXEL width column `col` contributes to the rails: EXACTLY 0.0 when
    /// hidden (the §18.3.1.13 formula must not run — it adds a 5px padding
    /// floor), else the usual chars→px conversion. Every rail-building site
    /// routes through this so hiding is inherited by all downstream consumers.
    pub(crate) fn col_px(&self, col: u32, mdw: f64) -> f64 {
        if self.col_hidden(col) {
            0.0
        } else {
            chars_to_px(self.col_width_chars(col), mdw)
        }
    }

    /// The v1-equivalent base range with the empty-sheet synthesis applied —
    /// exactly what `paginate` consumes as `model.dimensions` today.
    pub(crate) fn v1_range(&self) -> (u32, u32, u32, u32) {
        self.used_range_v1
            .unwrap_or((1, 1, EMPTY_SHEET_DEFAULT_ROWS, EMPTY_SHEET_DEFAULT_COLS))
    }

    /// The Grid-v2 EFFECTIVE range (dimension-distrust): the union of the
    /// v1-equivalent range, the stamped `<dimension>` (the scrollbar honors the
    /// stamp — extent fidelity, NO trimming) and the scanned content bounds
    /// (an understating stamp never cuts content off). Degenerate v1 col bounds
    /// (a sheet of cell-less rows: `min_col == u32::MAX`) are healed by the
    /// union; if NOTHING bounds the columns the empty-sheet default width applies.
    pub(crate) fn effective_range(&self) -> (u32, u32, u32, u32) {
        let mut r = self.v1_range();
        for ext in [self.stamped_dimension, self.content_bounds].into_iter().flatten() {
            r = (r.0.min(ext.0), r.1.min(ext.1), r.2.max(ext.2), r.3.max(ext.3));
        }
        // Heal a degenerate column span (explicit rows but zero cells anywhere
        // and no stamp): fall back to the empty-sheet default column count.
        if r.1 == u32::MAX || r.3 == 0 {
            r.1 = 1;
            r.3 = EMPTY_SHEET_DEFAULT_COLS;
        }
        r
    }

    /// Build the closed-form geometry over the v1-equivalent range.
    /// `mdw` is the measured Maximum Digit Width (§18.3.1.13);
    /// `drawings` are the sheet's OPC-parsed picture anchors — the used
    /// range extends under a floating picture exactly as `paginate` extends it.
    pub(crate) fn geom(&self, mdw: f64, drawings: &[SheetDrawing]) -> IndexGeom {
        self.geom_for(self.v1_range(), mdw, drawings, &[])
    }

    /// Build the closed-form geometry over the EFFECTIVE (distrust-union)
    /// range — the Grid-v2 grid path's geometry. `charts` are
    /// the sheet's anchored-chart placements (embedded charts) — same anchor
    /// geometry as pictures, kept SEPARATE so the painter routes them to the
    /// shared `crate::chart` painter instead of `draw_image`.
    pub(crate) fn geom_v2(
        &self,
        mdw: f64,
        drawings: &[SheetDrawing],
        charts: &[SheetDrawing],
    ) -> IndexGeom {
        self.geom_for(self.effective_range(), mdw, drawings, charts)
    }

    fn geom_for(
        &self,
        range: (u32, u32, u32, u32),
        mdw: f64,
        drawings: &[SheetDrawing],
        charts: &[SheetDrawing],
    ) -> IndexGeom {
        let (min_row, min_col, mut max_row, mut max_col) = range;

        let default_row_px = pt_to_px(self.default_row_height_pt);

        // Height exceptions: every present row with @ht, LAST duplicate wins
        // (paginate's HashMap-insert order), sorted by row + prefix sums.
        // A HIDDEN row joins the override path with height 0 (§18.3.1.73 —
        // zero contribution to the shared prefix sums, indices untouched).
        let mut by_row: std::collections::HashMap<u32, f64> = std::collections::HashMap::new();
        for e in &self.rows {
            if e.hidden {
                by_row.insert(e.row, 0.0);
            } else if let Some(h) = e.ht_pt {
                by_row.insert(e.row, pt_to_px(h));
            }
        }
        let mut exc: Vec<(u32, f64)> = by_row.into_iter().collect();
        exc.sort_unstable_by_key(|&(r, _)| r);
        let mut exc_prefix = Vec::with_capacity(exc.len() + 1);
        exc_prefix.push(0.0);
        for &(_, h) in &exc {
            exc_prefix.push(exc_prefix.last().unwrap() + h);
        }

        let mut geom = IndexGeom {
            min_row,
            max_row,
            min_col,
            max_col,
            default_row_px,
            exc,
            exc_prefix,
            col_x: vec![0.0],
            content_w: 0.0,
            content_h: 0.0,
            images: Vec::new(),
            charts: Vec::new(),
        };

        // Column rails: flat prefix over the (≤16,384-wide) range — a flat
        // rails vec is cheap at this width. Sequential accumulation
        // in the same order as `split_col_bands`, so the sums are FP-identical.
        if min_col != u32::MAX && min_col <= max_col {
            let mut x = 0.0;
            for c in min_col..=max_col {
                x += self.col_px(c, mdw);
                geom.col_x.push(x);
            }
        }

        // ── Picture/chart extension — mirrors paginate's loop exactly ────
        if !drawings.is_empty() || !charts.is_empty() {
            let col_left_any = |g: &IndexGeom, c: u32| -> f64 {
                if c >= g.min_col {
                    if c <= g.max_col + 1 {
                        g.col_x[(c - g.min_col) as usize]
                    } else {
                        // Beyond the rails: continue the same sequential sum.
                        let mut x = *g.col_x.last().unwrap();
                        for cc in g.max_col + 1..c {
                            x += self.col_px(cc, mdw);
                        }
                        x
                    }
                } else {
                    -(c..g.min_col).map(|cc| self.col_px(cc, mdw)).sum::<f64>()
                }
            };
            // Absolute anchor boxes (with keys) — mirrors paginate's
            // `abs_images`; retained on the geom for the windowed painter.
            let to_box = |g: &IndexGeom, d: &SheetDrawing| -> Option<PageImage> {
                let x0 = col_left_any(g, d.from.col + 1) + emu_to_px(d.from.col_off_emu);
                let y0 = g.row_top(d.from.row + 1) + emu_to_px(d.from.row_off_emu);
                let from_to = d.to.map(|t| {
                    (
                        col_left_any(g, t.col + 1) + emu_to_px(t.col_off_emu) - x0,
                        g.row_top(t.row + 1) + emu_to_px(t.row_off_emu) - y0,
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
            let abs_boxes: Vec<PageImage> =
                drawings.iter().filter_map(|d| to_box(&geom, d)).collect();
            let abs_charts: Vec<PageImage> =
                charts.iter().filter_map(|d| to_box(&geom, d)).collect();
            let mut right = col_left_any(&geom, max_col + 1);
            let mut bottom = geom.row_top(max_row + 1);
            for &PageImage { x, y, w, h, .. } in abs_boxes.iter().chain(abs_charts.iter()) {
                while right < x + w - 0.01 && max_col < 16_384 {
                    max_col += 1;
                    right += self.col_px(max_col, mdw);
                    geom.col_x.push(*geom.col_x.last().unwrap() + self.col_px(max_col, mdw));
                }
                while bottom < y + h - 0.01 && max_row < 1_048_576 {
                    max_row += 1;
                    bottom += geom.exc_height(max_row).unwrap_or(default_row_px);
                }
            }
            geom.max_col = max_col;
            geom.max_row = max_row;
            geom.images = abs_boxes;
            geom.charts = abs_charts;
        }

        geom.content_w = *geom.col_x.last().unwrap();
        geom.content_h = geom.row_top(geom.max_row + 1);
        geom
    }

    // ── Grid v2: the synthetic-envelope window slice ─────────────────────────

    /// Index range `[lo, hi)` of the present-row entries whose row numbers fall
    /// in `[r0, r1]` (entries are in document order; well-formed sheets are
    /// row-ascending, so `partition_point` is exact — a malformed out-of-order
    /// stream degrades to a slightly-wider window, never a wrong parse).
    pub(crate) fn entry_range(&self, r0: u32, r1: u32) -> (usize, usize) {
        let lo = self.rows.partition_point(|e| e.row < r0);
        let hi = self.rows.partition_point(|e| e.row <= r1);
        (lo, hi)
    }

    /// Build the synthetic envelope for entries `[lo, hi)`: the
    /// retained XML's own pre-sheetData bytes (the ORIGINAL `<worksheet>` start
    /// tag, namespace/prefix declarations, metadata siblings and the original
    /// `<sheetData>` start tag — so prefixed `<x:row>` streams parse with their
    /// declarations intact) + the exact row-element byte slice + the original
    /// post-sheetData bytes. When the slice's FIRST row is r-less, its true row
    /// number (which the index knows) is stamped as an explicit `r` attribute so
    /// implied previous+1 numbering (§18.3.1.73) re-synchronizes; subsequent
    /// r-less rows then imply correctly from it.
    ///
    /// The result parses through the EXISTING fork `parse_worksheet` — the same
    /// entry point the full sheet uses. NO fork changes.
    pub(crate) fn envelope(&self, lo: usize, hi: usize) -> Vec<u8> {
        let pre = &self.xml[..self.sheet_data.start];
        let post = &self.xml[self.sheet_data.end..];
        if lo >= hi {
            // Empty window: the husk itself (pre+post) is a valid worksheet.
            return [pre, post].concat();
        }
        let first = &self.rows[lo];
        let span = first.byte_off as usize..self.rows[hi - 1].byte_end as usize;
        let mut out = Vec::with_capacity(pre.len() + post.len() + (span.end - span.start) + 16);
        out.extend_from_slice(pre);
        if first.explicit_r {
            out.extend_from_slice(&self.xml[span]);
        } else {
            // Stamp `r` right after the tag name: `<row` / `<x:row` → `… r="N"`.
            let b = &self.xml[span.clone()];
            let nl = 1 + name_len(&b[1..]); // length of "<name"
            out.extend_from_slice(&b[..nl]);
            out.extend_from_slice(format!(" r=\"{}\"", first.row).as_bytes());
            out.extend_from_slice(&b[nl..]);
        }
        out.extend_from_slice(post);
        out
    }
}

/// Closed-form grid geometry derived from a [`SheetIndex`] + the measured
/// MDW: extent, row↔y in O(log #exceptions), col↔x via the flat rails.
pub(crate) struct IndexGeom {
    pub(crate) min_row: u32,
    pub(crate) max_row: u32,
    pub(crate) min_col: u32,
    pub(crate) max_col: u32,
    pub(crate) default_row_px: f64,
    /// Height exceptions (row, px), sorted by row, full row space.
    exc: Vec<(u32, f64)>,
    /// `exc_prefix[i]` = Σ exc[..i] heights.
    exc_prefix: Vec<f64>,
    /// Flat column rails relative to `min_col`'s left edge:
    /// `col_x[k]` = left of column `min_col + k`; last entry = right edge.
    col_x: Vec<f64>,
    pub(crate) content_w: f64,
    pub(crate) content_h: f64,
    /// Absolute (content-space) anchored-picture boxes with image keys —
    /// the windowed painter's `abs_images` equivalent.
    pub(crate) images: Vec<PageImage>,
    /// Absolute (content-space) anchored-CHART boxes with chart keys (embedded
    /// charts) — same box math as `images`, painted via `crate::chart`.
    pub(crate) charts: Vec<PageImage>,
}

impl IndexGeom {
    /// The explicit-`@ht` height of `row` in px, if it is an exception.
    pub(crate) fn exc_height(&self, row: u32) -> Option<f64> {
        let i = self.exc.partition_point(|&(r, _)| r < row);
        (i < self.exc.len() && self.exc[i].0 == row).then(|| self.exc[i].1)
    }

    /// y of row `r`'s TOP edge relative to `min_row`'s top (the rails origin).
    /// Defined for any row (a row below `min_row` maps negative — the same
    /// contract as paginate's `row_top` closure, used by drawing anchors).
    /// Closed-form: `(span − #exceptions) × default + Σ exception heights`,
    /// exceptions found in O(log #exceptions).
    pub(crate) fn row_top(&self, r: u32) -> f64 {
        if r >= self.min_row {
            let n = (r - self.min_row) as f64;
            let lo = self.exc.partition_point(|&(er, _)| er < self.min_row);
            let hi = self.exc.partition_point(|&(er, _)| er < r);
            let k = (hi - lo) as f64;
            let s = self.exc_prefix[hi] - self.exc_prefix[lo];
            (n - k) * self.default_row_px + s
        } else {
            let n = (self.min_row - r) as f64;
            let lo = self.exc.partition_point(|&(er, _)| er < r);
            let hi = self.exc.partition_point(|&(er, _)| er < self.min_row);
            let k = (hi - lo) as f64;
            let s = self.exc_prefix[hi] - self.exc_prefix[lo];
            -((n - k) * self.default_row_px + s)
        }
    }

    /// The row whose band contains content-y `y` (bands are `[top, top+h)`),
    /// clamped to `[min_row, max_row]`. O(log #exceptions): binary-search the
    /// last exception whose top is at/above `y`, then arithmetic.
    pub(crate) fn row_at_y(&self, y: f64) -> u32 {
        if y <= 0.0 || self.default_row_px <= 0.0 {
            return self.min_row;
        }
        let lo = self.exc.partition_point(|&(er, _)| er < self.min_row);
        let hi = self.exc.partition_point(|&(er, _)| er <= self.max_row);
        // Count in-range exceptions whose TOP is <= y.
        let (mut a, mut b) = (lo, hi);
        while a < b {
            let m = (a + b) / 2;
            if self.row_top(self.exc[m].0) <= y {
                a = m + 1;
            } else {
                b = m;
            }
        }
        let r = if a == lo {
            // Pure default-height run from min_row.
            self.min_row + (y / self.default_row_px).floor() as u32
        } else {
            let (erow, eh) = self.exc[a - 1];
            let etop = self.row_top(erow);
            if y < etop + eh {
                erow
            } else {
                erow + 1 + ((y - etop - eh) / self.default_row_px).floor() as u32
            }
        };
        r.min(self.max_row)
    }

    /// x of column `c`'s LEFT edge relative to `min_col`'s left
    /// (`min_col ≤ c ≤ max_col + 1`; `max_col + 1` = the right edge).
    pub(crate) fn col_left(&self, c: u32) -> f64 {
        self.col_x[(c - self.min_col) as usize]
    }

    /// The flat column rails (`col_x[k]` = left of `min_col + k`; last entry =
    /// the right edge) — the windowed page builder's column geometry.
    pub(crate) fn col_rails(&self) -> &[f64] {
        &self.col_x
    }

    /// The column whose band contains content-x `x`, clamped to
    /// `[min_col, max_col]`. O(log #cols) over the flat rails.
    pub(crate) fn col_at_x(&self, x: f64) -> u32 {
        if x <= 0.0 || self.col_x.len() < 2 {
            return self.min_col;
        }
        let k = self.col_x.partition_point(|&r| r <= x);
        self.min_col + (k.saturating_sub(1) as u32).min(self.max_col - self.min_col)
    }

    /// Full grid extent INCLUDING the sticky gutters — the v1 `grid_extent`
    /// contract (`content + gutter_dims`), closed-form.
    pub(crate) fn extent_px(&self) -> (f64, f64) {
        (
            self.content_w + row_gutter_width_for_max_row(self.max_row),
            self.content_h + COL_GUTTER_H,
        )
    }
}

// ─── raw-XML scanning (the repair::xml_scan discipline) ──────────────────────

/// The located sheetData element: content span + whether it was self-closing.
struct SheetDataSpan {
    /// CONTENT byte range (between the start tag's `>` and `</sheetData>`).
    content: Range<usize>,
    self_closing: bool,
}

/// The local part of an XML name (after the `prefix:`, if any) — .NET
/// exporters write `<x:row>`/`<x:sheetData>` streams.
fn local_name(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// Byte length of the element name starting at `b[0]` (after `<` / `</`).
fn name_len(b: &[u8]) -> usize {
    b.iter()
        .position(|&c| matches!(c, b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>'))
        .unwrap_or(b.len())
}

/// Skip a non-element markup construct starting at `b[i] == b'<'`:
/// comment, CDATA section, DOCTYPE, or processing instruction. Returns the
/// offset just past it, or `None` if `b[i..]` is not such a construct.
fn skip_special(b: &[u8], i: usize) -> Option<usize> {
    let rest = &b[i..];
    if rest.starts_with(b"<!--") {
        return Some(find_sub(b, i + 4, b"-->").map_or(b.len(), |p| p + 3));
    }
    if rest.starts_with(b"<![CDATA[") {
        return Some(find_sub(b, i + 9, b"]]>").map_or(b.len(), |p| p + 3));
    }
    if rest.starts_with(b"<!") {
        return Some(find_tag_end(rest).map_or(b.len(), |e| i + e + 1));
    }
    if rest.starts_with(b"<?") {
        return Some(find_sub(b, i + 2, b"?>").map_or(b.len(), |p| p + 2));
    }
    None
}

/// First occurrence of `needle` at/after `from`.
fn find_sub(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= b.len() {
        return None;
    }
    b[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

/// Locate the `<sheetData>` element's content span (quote-/comment-/CDATA-
/// aware). Returns `None` when the document has no sheetData element.
fn find_sheet_data(b: &[u8]) -> Option<SheetDataSpan> {
    let mut i = 0usize;
    let mut inside: Option<usize> = None; // content start once inside sheetData
    while i < b.len() {
        // Jump to the next markup start.
        match b[i..].iter().position(|&c| c == b'<') {
            Some(p) => i += p,
            None => break,
        }
        if let Some(next) = skip_special(b, i) {
            i = next;
            continue;
        }
        if b[i..].starts_with(b"</") {
            let nl = name_len(&b[i + 2..]);
            let is_sd = local_name(&b[i + 2..i + 2 + nl]) == b"sheetData";
            let after = find_tag_end(&b[i..]).map_or(b.len(), |e| i + e + 1);
            if is_sd {
                if let Some(start) = inside {
                    return Some(SheetDataSpan { content: start..i, self_closing: false });
                }
            }
            i = after;
            continue;
        }
        // Start tag.
        let nl = name_len(&b[i + 1..]);
        let is_sd = local_name(&b[i + 1..i + 1 + nl]) == b"sheetData";
        let end = match find_tag_end(&b[i..]) {
            Some(e) => i + e, // offset of '>'
            None => break,    // unterminated tag — bail
        };
        let self_closing = end > i && b[end - 1] == b'/';
        if is_sd && inside.is_none() {
            if self_closing {
                return Some(SheetDataSpan { content: end + 1..end + 1, self_closing: true });
            }
            inside = Some(end + 1);
        }
        i = end + 1;
    }
    // Unterminated sheetData (malformed): treat the rest as content.
    inside.map(|start| SheetDataSpan { content: start..b.len(), self_closing: false })
}

/// The pre-scan row pass: walk the sheetData content span and
/// record one [`RowEntry`] per `<row>` start tag — `@r` or implied numbering,
/// byte offsets, `@ht`, and the populated-column bounds from `<c r>` refs.
fn scan_rows(b: &[u8], span: Range<usize>) -> Vec<RowEntry> {
    let mut out: Vec<RowEntry> = Vec::new();
    let mut next_row: u32 = 1;
    let mut i = span.start;
    let end = span.end;
    while i < end {
        match b[i..end].iter().position(|&c| c == b'<') {
            Some(p) => i += p,
            None => break,
        }
        if let Some(next) = skip_special(b, i) {
            i = next;
            continue;
        }
        if b[i..].starts_with(b"</") {
            let nl = name_len(&b[i + 2..]);
            let after = find_tag_end(&b[i..]).map_or(end, |e| i + e + 1);
            if local_name(&b[i + 2..i + 2 + nl]) == b"row" {
                if let Some(e) = out.last_mut() {
                    if e.byte_end == 0 {
                        e.byte_end = after.min(end) as u32;
                    }
                }
            }
            i = after;
            continue;
        }
        let nl = name_len(&b[i + 1..]);
        let local = local_name(&b[i + 1..i + 1 + nl]);
        let tag_gt = match find_tag_end(&b[i..]) {
            Some(e) => i + e,
            None => break,
        };
        let self_closing = tag_gt > i && b[tag_gt - 1] == b'/';
        if local == b"row" {
            // A previous row never closed (malformed) ends where this one starts.
            if let Some(e) = out.last_mut() {
                if e.byte_end == 0 {
                    e.byte_end = i as u32;
                }
            }
            let tag = std::str::from_utf8(&b[i..=tag_gt]).unwrap_or("");
            let r_attr = attr_val(tag, "r").and_then(|v| v.parse::<u32>().ok());
            let ht_pt = attr_val(tag, "ht").and_then(|v| v.parse::<f64>().ok());
            // Hidden (§18.3.1.73): @hidden, or the Excel equivalence ht="0" +
            // customHeight (float-dust guarded) — mirrors model::row_hidden.
            let hidden = attr_bool_on(tag, "hidden")
                || (attr_bool_on(tag, "customHeight")
                    && ht_pt.is_some_and(|h| h.abs() < super::model::HIDDEN_ZERO_EPS));
            let row = r_attr.unwrap_or(next_row);
            // `saturating_add` mirrors `build_rows`: `<row r="4294967295">` would
            // otherwise overflow the implied-row counter (debug panic / release
            // wrap). Out-of-grid rows are dropped from the RANGES, not from the
            // scan, so the entry itself is still pushed (byte offsets stay honest
            // and following cells attach to the right row).
            next_row = row.saturating_add(1);
            out.push(RowEntry {
                row,
                byte_off: i as u32,
                byte_end: if self_closing { (tag_gt + 1) as u32 } else { 0 },
                explicit_r: r_attr.is_some(),
                ht_pt,
                hidden,
                min_col: 0,
                max_col: 0,
            });
        } else if local == b"c" {
            if let Some(e) = out.last_mut() {
                if e.byte_end == 0 {
                    // Only cells INSIDE the open row count toward its bounds.
                    let tag = std::str::from_utf8(&b[i..=tag_gt]).unwrap_or("");
                    if let Some((col, _)) =
                        attr_val(tag, "r").and_then(super::model::parse_a1)
                    {
                        if e.max_col == 0 {
                            e.min_col = col;
                            e.max_col = col;
                        } else {
                            e.min_col = e.min_col.min(col);
                            e.max_col = e.max_col.max(col);
                        }
                    }
                }
            }
        }
        i = tag_gt + 1;
    }
    if let Some(e) = out.last_mut() {
        if e.byte_end == 0 {
            e.byte_end = end as u32;
        }
    }
    out
}

/// Whether boolean attribute `name` is ON in a start tag (`ST_Xstring bool`:
/// `"1"`/`"true"`; absent or `"0"`/`"false"` → off — the xml_scan discipline,
/// mirroring `ooxml_xml::ooxml_bool`).
fn attr_bool_on(tag: &str, name: &str) -> bool {
    matches!(attr_val(tag, name), Some("1") | Some("true"))
}

/// The value of attribute `name` in a start tag, walking attributes properly
/// (whole-name match, prefix-tolerant, single- or double-quoted values).
/// Returns the RAW value text (entities undecoded — the targeted attrs are
/// numeric, the xml_scan discipline).
fn attr_val<'t>(tag: &'t str, name: &str) -> Option<&'t str> {
    let b = tag.as_bytes();
    // Skip "<name" (or "</name").
    let mut i = 1 + name_len(&b[1..]);
    while i < b.len() {
        // Skip whitespace.
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
        }
        if i >= b.len() || matches!(b[i], b'>' | b'/') {
            return None;
        }
        // Attribute name.
        let ns = i;
        while i < b.len() && !matches!(b[i], b'=' | b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/') {
            i += 1;
        }
        let aname = &tag[ns..i];
        // Skip whitespace, expect '='.
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            // Valueless (malformed) attribute — keep walking.
            continue;
        }
        i += 1;
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
        }
        if i >= b.len() || !matches!(b[i], b'"' | b'\'') {
            return None;
        }
        let quote = b[i];
        i += 1;
        let vs = i;
        while i < b.len() && b[i] != quote {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        let val = &tag[vs..i];
        i += 1;
        if local_name(aname.as_bytes()) == name.as_bytes() {
            return Some(val);
        }
    }
    None
}

// ─── scanner paranoia tests (the quote-aware discipline) ─────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

    fn ws(body: &str) -> Vec<u8> {
        format!("<worksheet xmlns=\"{NS}\"><sheetData>{body}</sheetData></worksheet>").into_bytes()
    }

    fn build(body: &str) -> SheetIndex {
        SheetIndex::build("T".into(), ws(body)).expect("index build")
    }

    #[test]
    fn self_closing_rows_and_the_last_row_before_sheetdata_end() {
        let idx = build(r#"<row r="1"/><row r="2"><c r="B2" t="n"><v>5</v></c></row>"#);
        assert_eq!(idx.rows.len(), 2);
        let s0 = &idx.xml[idx.rows[0].byte_off as usize..idx.rows[0].byte_end as usize];
        assert_eq!(s0, br#"<row r="1"/>"#, "self-closing slice is the exact element");
        let s1 = &idx.xml[idx.rows[1].byte_off as usize..idx.rows[1].byte_end as usize];
        assert!(s1.starts_with(b"<row r=\"2\"") && s1.ends_with(b"</row>"));
        // The last row's end abuts </sheetData> exactly.
        assert_eq!(idx.rows[1].byte_end as usize, idx.sheet_data.end);
        assert_eq!((idx.rows[1].min_col, idx.rows[1].max_col), (2, 2));
    }

    #[test]
    fn r_less_rows_imply_previous_plus_one_from_the_start() {
        // First row r-less → implied 1 (§18.3.1.73); the run continues; an
        // explicit r resyncs; a trailing bare <row/> follows it.
        let idx = build(r#"<row><c r="A1"/></row><row><c r="A2"/></row><row r="7"/><row/>"#);
        let nums: Vec<(u32, bool)> = idx.rows.iter().map(|e| (e.row, e.explicit_r)).collect();
        assert_eq!(nums, vec![(1, false), (2, false), (7, true), (8, false)]);
    }

    #[test]
    fn cdata_comments_and_quotes_do_not_fool_the_scanner() {
        // CDATA containing a fake </sheetData> AND a fake <row>; a comment
        // containing a fake row; single-quoted attributes; an attribute value
        // containing '>'.
        let body = concat!(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t><![CDATA[</sheetData> <row r="999"><c r="Z999"/>]]></t></is></c></row>"#,
            r#"<!-- <row r="777"><c r="Y777"/> --> "#,
            r#"<row r='2' ht='30.75' x="a>b"><c r='C2'/></row>"#
        );
        let idx = build(body);
        assert_eq!(idx.rows.len(), 2, "fake rows in CDATA/comments not indexed");
        assert_eq!(idx.rows[0].row, 1);
        assert_eq!((idx.rows[1].row, idx.rows[1].ht_pt), (2, Some(30.75)));
        assert_eq!((idx.rows[1].min_col, idx.rows[1].max_col), (3, 3), "single-quoted @r");
        assert_eq!(idx.content_bounds, Some((1, 1, 2, 3)));
        assert_eq!(idx.stamped_dimension, None, "no <dimension> in this husk");
    }

    #[test]
    fn prefixed_dotnet_stream_scans_by_local_name() {
        // .NET exporters write <x:worksheet>/<x:sheetData>/<x:row> streams;
        // the scanner matches LOCAL names (the husk keeps the original tags).
        let xml = format!(
            r#"<x:worksheet xmlns:x="{NS}"><x:sheetData><x:row><x:c r="A1"/></x:row><x:row r="5" ht="24"/></x:sheetData></x:worksheet>"#
        );
        let sd = find_sheet_data(xml.as_bytes()).expect("sheetData located");
        assert!(!sd.self_closing);
        let rows = scan_rows(xml.as_bytes(), sd.content.clone());
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].row, rows[0].explicit_r, rows[0].max_col), (1, false, 1));
        assert_eq!((rows[1].row, rows[1].ht_pt), (5, Some(24.0)));
        assert!(xml.as_bytes()[rows[0].byte_off as usize..].starts_with(b"<x:row"));
    }

    #[test]
    fn self_closing_sheetdata_yields_no_rows_and_no_range() {
        let xml = format!(
            r#"<worksheet xmlns="{NS}"><dimension ref="A1"/><sheetData/></worksheet>"#
        );
        let idx = SheetIndex::build("T".into(), xml.into_bytes()).expect("build");
        assert!(idx.rows.is_empty());
        assert!(idx.sheet_data.is_empty());
        assert_eq!(idx.used_range_v1, None, "the fork-mirror range of an empty sheet");
        assert_eq!(idx.content_bounds, None);
        assert_eq!(idx.stamped_dimension, Some((1, 1, 1, 1)), "single-cell dimension ref");
    }

    #[test]
    fn attr_extraction_is_whole_name_and_order_independent() {
        // `r` must not match inside `spans`/`customFormat`/`thickBot`, and
        // attribute order must not matter.
        let tag = r#"<row spans="1:10" customFormat="1" thickBot="1" ht="12.75" r="42">"#;
        assert_eq!(attr_val(tag, "r"), Some("42"));
        assert_eq!(attr_val(tag, "ht"), Some("12.75"));
        assert_eq!(attr_val(tag, "missing"), None);
        assert_eq!(attr_val(r#"<c thick="1" r="B7">"#, "r"), Some("B7"));
        // No attributes at all.
        assert_eq!(attr_val("<row>", "r"), None);
        assert_eq!(attr_val("<row/>", "r"), None);
    }

    #[test]
    fn husk_carries_metadata_while_rows_stay_out_of_it() {
        // cols / sheetFormatPr / mergeCells / sheetView survive the husk parse;
        // the (spliced-out) rows arrive ONLY via the scan.
        let xml = format!(
            r#"<worksheet xmlns="{NS}"><dimension ref="B2:D9"/><sheetViews><sheetView workbookViewId="0" showGridLines="0"/></sheetViews><sheetFormatPr defaultRowHeight="12" defaultColWidth="11"/><cols><col min="2" max="3" width="14" customWidth="1"/></cols><sheetData><row r="2" ht="20"><c r="B2" t="n"><v>1</v></c></row></sheetData><mergeCells count="1"><mergeCell ref="B2:C3"/></mergeCells></worksheet>"#
        );
        let idx = SheetIndex::build("T".into(), xml.into_bytes()).expect("build");
        assert_eq!(idx.default_row_height_pt, 12.0);
        assert_eq!(idx.default_col_width_chars, 11.0);
        assert!(!idx.show_grid_lines);
        assert_eq!(idx.col_runs.len(), 1);
        assert_eq!((idx.col_runs[0].min, idx.col_runs[0].max), (2, 3));
        assert_eq!(idx.merges.len(), 1);
        assert_eq!(idx.stamped_dimension, Some((2, 2, 9, 4)));
        assert_eq!(idx.rows.len(), 1);
        assert_eq!(idx.rows[0].ht_pt, Some(20.0));
    }
}
