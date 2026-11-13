//! Grid v2 — window materialization + the chunk LRU (the row-content half of
//! the lazy model; [`super::index`] is the geometry half).
//!
//! The painter never sees the whole sheet again: when it needs rows `[a, b]`,
//! the [`super::index::SheetIndex`] maps them to an exact byte span of the
//! retained worksheet XML, the span is wrapped in a **synthetic envelope**
//! (the sheet's OWN pre/post-sheetData bytes — original tags, prefixes and
//! namespace declarations — around the row slice) and parsed through the
//! EXISTING fork `parse_worksheet` (NO fork changes). The parsed rows build
//! [`RowData`]/[`CellModel`]s via the same [`super::model::build_rows`] the
//! eager path uses — the v1/v2 equivalence invariant holds by construction.
//!
//! Parsed slices are cached in an **LRU keyed `(sheet, chunk)`** with windows
//! rounded to fixed [`CHUNK_ROWS`]-row chunks so scrolling reuses chunks
//! instead of re-parsing per pixel. Chunk size is a TUNING KNOB:
//! 256 rows ≈ 4–8 viewport-heights of default rows — measured
//! sub-millisecond to parse on sparse sheets and low-single-digit ms on dense
//! ones, so a cold scroll stays far inside a 100 ms frame budget while
//! one chunk usually covers several frames of scrolling. Capacity
//! [`CHUNK_CAPACITY`] = 64 chunks ≈ "a few dozen"; eviction is
//! droppable-and-reparseable by construction.
//!
//! [`build_window_page`] then turns a row window + the closed-form
//! [`IndexGeom`] into a `SheetPage` whose rails/cells/merges are — over the
//! FULL row range — **hash-identical** to v1's `continuous_page` (the pinned
//! oracles): within-window row heights accumulate SEQUENTIALLY (the exact FP
//! order `paginate`'s flush used), with only the window's BASE offset coming
//! from the closed-form `row_top` (full range: base = 0, bit-exact).
//!
//! Kill-the-void: rails may extend BEYOND the sheet extent — rows past
//! `geom.max_row` at the default height, columns past `geom.max_col` at their
//! `<col>`-run/default widths — so any window paints real grid, never void.
//!
//! Merged cells across a window/chunk boundary: a region whose
//! ANCHOR lies above the window still paints — the merge list is husk metadata,
//! the anchor row's chunk is fetched explicitly, and the anchor cell is emitted
//! (expanded to the full region extent) ahead of the window rows so
//! `visible_cells`' widen-for-anchors finds it.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use ooxml_sml::ext::{parse_worksheet, ResolvedSheet};
use ooxml_sml::parsers::ParseError;
use ooxml_sml::types::Stylesheet;

use super::grid::{
    col_letters, MergeBox, PageCell, SheetPage, EMPTY_SHEET_DEFAULT_COLS,
    EMPTY_SHEET_DEFAULT_ROWS,
};
use super::index::{IndexGeom, SheetIndex};
use super::model::{
    build_rows, ColorCtx, MergeRegion, RowData, StyleResolver, MAX_ADDRESSABLE_ROW,
};

/// Rows per LRU chunk (the tuning knob — see the module header).
pub(crate) const CHUNK_ROWS: u32 = 256;
/// LRU capacity in chunks (global across sheets; "a few dozen per sheet").
pub(crate) const CHUNK_CAPACITY: usize = 64;

/// The 0-based chunk id of a 1-based row number.
pub(crate) fn chunk_of(row: u32) -> u32 {
    (row.max(1) - 1) / CHUNK_ROWS
}

/// First/last 1-based row of a chunk id.
pub(crate) fn chunk_rows(chunk: u32) -> (u32, u32) {
    let r0 = chunk * CHUNK_ROWS + 1;
    (r0, r0 + CHUNK_ROWS - 1)
}

// ─── the chunk LRU ────────────────────────────────────────────────────────────

/// LRU of materialized row chunks keyed `(sheet, chunk)`. Values
/// are `Rc` so a window can hold chunks across an eviction. Touch order is a
/// plain vec (capacity is small; O(cap) touch is noise next to a parse).
pub(crate) struct ChunkStore {
    map: HashMap<(usize, u32), Rc<Vec<RowData>>>,
    order: Vec<(usize, u32)>,
    cap: usize,
}

impl ChunkStore {
    pub(crate) fn new(cap: usize) -> Self {
        ChunkStore { map: HashMap::new(), order: Vec::new(), cap }
    }

    /// Cached chunk or `build()` + insert (evicting the least-recently-used
    /// entry past capacity). Empty chunks are cached too — scrolling the void
    /// must not re-scan the index per frame.
    pub(crate) fn get_or_build(
        &mut self,
        key: (usize, u32),
        build: impl FnOnce() -> Vec<RowData>,
    ) -> Rc<Vec<RowData>> {
        if let Some(v) = self.map.get(&key) {
            let v = v.clone();
            if let Some(pos) = self.order.iter().position(|k| *k == key) {
                self.order.remove(pos);
            }
            self.order.push(key);
            return v;
        }
        let v = Rc::new(build());
        self.map.insert(key, v.clone());
        self.order.push(key);
        while self.order.len() > self.cap {
            let old = self.order.remove(0);
            self.map.remove(&old);
        }
        v
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, key: (usize, u32)) -> bool {
        self.map.contains_key(&key)
    }
}

// ─── the synthetic-envelope slice parse ──────────────────────────────────────

/// Materialize rows `[r0, r1]` of an indexed sheet: index → byte span →
/// synthetic envelope → the EXISTING fork `parse_worksheet` → the SAME
/// `build_rows` the eager model used. A `MissingAttribute` failure routes
/// through the render-only `<col>` repair (the husk/envelope carries the
/// sheet's `<cols>`, so a deficient `<col>` resurfaces per window and is
/// repaired per window); an unrecoverable parse yields no rows (render-tolerant:
/// the frame phase already painted the grid).
pub(crate) fn parse_rows_window(
    index: &SheetIndex,
    r0: u32,
    r1: u32,
    shared_strings: &[String],
    stylesheet: &Stylesheet,
    colors: &ColorCtx,
) -> Vec<RowData> {
    let (lo, hi) = index.entry_range(r0, r1);
    if lo >= hi {
        return Vec::new();
    }
    let env = index.envelope(lo, hi);
    let worksheet = match parse_worksheet(&env) {
        Ok(ws) => ws,
        Err(ParseError::MissingAttribute(_)) => {
            let repaired = crate::repair::sml_col_attrs(&env);
            match parse_worksheet(&repaired) {
                Ok(ws) => ws,
                Err(_) => return Vec::new(),
            }
        }
        Err(_) => return Vec::new(),
    };
    let rs = ResolvedSheet::new(index.name.clone(), worksheet, shared_strings.to_vec());
    let mut styles = StyleResolver::new(stylesheet, colors);
    build_rows(&rs, &mut styles)
}

// ─── a window's row view over fetched chunks ─────────────────────────────────

/// Row lookup over the chunks a window fetched (plus any merge-anchor chunks).
/// Chunks are few per window; lookup is a linear chunk scan + binary search.
pub(crate) struct RowsView {
    chunks: Vec<Rc<Vec<RowData>>>,
}

impl RowsView {
    pub(crate) fn new(chunks: Vec<Rc<Vec<RowData>>>) -> Self {
        RowsView { chunks }
    }

    pub(crate) fn lookup(&self, row: u32) -> Option<&RowData> {
        for c in &self.chunks {
            let i = c.partition_point(|r| r.row < row);
            if i < c.len() && c[i].row == row {
                return Some(&c[i]);
            }
        }
        None
    }
}

// ─── the windowed SheetPage builder ──────────────────────────────────────────

/// Inputs for one window build. `r1` may exceed `geom.max_row` (and
/// `extend_to_x` may exceed `geom.content_w`) — the kill-the-void extension:
/// rails continue at default height / `<col>`-run widths, cells stay absent.
pub(crate) struct WindowSpec<'a> {
    pub(crate) index: &'a SheetIndex,
    pub(crate) geom: &'a IndexGeom,
    pub(crate) mdw: f64,
    pub(crate) rows: &'a RowsView,
    /// First/last (inclusive) row to emit rails for.
    pub(crate) r0: u32,
    pub(crate) r1: u32,
    /// Extend the column rails (void columns) until the right edge reaches at
    /// least this content-x (0.0 → no extension).
    pub(crate) extend_to_x: f64,
}

/// Build one windowed `SheetPage` whose geometry — over the FULL row range with
/// no void extension — is hash-identical to v1's `continuous_page` (the v1/v2
/// equivalence invariant; see the module header for the FP discipline).
pub(crate) fn build_window_page(w: &WindowSpec) -> SheetPage {
    let geom = w.geom;
    let index = w.index;
    let (min_col, max_col) = (geom.min_col, geom.max_col);
    let default_h = geom.default_row_px;
    let height = |r: u32| geom.exc_height(r).unwrap_or(default_h);

    // Column rails: the geom's flat rails + the kill-the-void extension.
    let mut col_x: Vec<f64> = geom.col_rails().to_vec();
    let mut ext_max_col = max_col;
    while *col_x.last().unwrap() < w.extend_to_x && ext_max_col < 16_384 {
        ext_max_col += 1;
        // Hidden columns extend at 0px (col_px), same as the geom rails.
        let cw = index.col_px(ext_max_col, w.mdw);
        col_x.push(col_x.last().unwrap() + cw);
    }

    // ── Merge bookkeeping (§18.3.1.55), windowed ─────────────────────────────
    // `covered`/`anchor_of` mirror paginate's construction but only over rows
    // AND COLUMNS intersecting the window (a merge spanning a million rows must
    // not cost a million inserts per frame).
    //
    // Clipped on the COLUMN axis too: `covered` is only ever probed for
    // cells inside `[min_col, max_col]` (the guard in the row walk below), so
    // clipping answers identically while `A1:XFD1048576` — the LEGITIMATE region
    // Excel writes for select-all + merge, which must not be dropped or trimmed —
    // stops costing `window_rows x 16 384` `HashSet` inserts every repaint. This
    // is BOUND_BY_GEOMETRY, not a limit on the document: the merge keeps its
    // full extent, only the bookkeeping shrinks to what is on screen.
    let mut covered: HashSet<(u32, u32)> = HashSet::new();
    let mut anchor_of: HashMap<(u32, u32), MergeRegion> = HashMap::new();
    // Regions whose anchor lies ABOVE the window but which intersect it — the
    // merge-across-chunk/window-boundary case. Sorted by anchor
    // position so the prepended anchor cells stay in row order (visible_cells'
    // widen-for-anchors walks consecutive anchors backwards).
    let mut above: Vec<MergeRegion> = Vec::new();
    for region in &index.merges {
        if region.max_row < w.r0 || region.min_row > w.r1 {
            continue;
        }
        anchor_of.insert((region.min_row, region.min_col), *region);
        for rr in region.min_row.max(w.r0)..=region.max_row.min(w.r1) {
            for cc in region.min_col.max(min_col)..=region.max_col.min(max_col) {
                if !region.is_anchor(rr, cc) {
                    covered.insert((rr, cc));
                }
            }
        }
        if region.min_row < w.r0 {
            above.push(*region);
        }
    }
    above.sort_unstable_by_key(|m| (m.min_row, m.min_col));

    // Shared cell emitter — mirrors paginate's flush loop body EXACTLY.
    let emit = |row: u32,
                y: f64,
                h: f64,
                cm: &super::model::CellModel,
                anchor_of: &HashMap<(u32, u32), MergeRegion>,
                cells: &mut Vec<PageCell>,
                merges: &mut Vec<MergeBox>| {
        let k = (cm.col - min_col) as usize;
        let cx = col_x[k];
        let mut cw = col_x[k + 1] - col_x[k];
        let mut ch = h;
        let mut is_merged_anchor = false;
        if let Some(region) = anchor_of.get(&(row, cm.col)) {
            is_merged_anchor = true;
            // Width: span to the merge's right edge, clamped to the sheet band
            // (the continuous page is one band — same clamp paginate applies).
            let span_max_col = region.max_col.min(max_col);
            let k_end = (span_max_col - min_col + 1) as usize;
            cw = col_x[k_end] - col_x[k];
            // Height: sum each spanned row's height SEQUENTIALLY (flush's exact
            // FP order), clamped to the sheet's last row.
            let span_max_row = region.max_row.min(geom.max_row);
            let mut summed = 0.0;
            for rr in row..=span_max_row {
                summed += height(rr);
            }
            ch = summed;
        }
        // Zero box → emit nothing: no degenerate draws, no spill occupancy
        // (text spills ACROSS hidden columns, like Excel). Zero boxes arise
        // from HIDDEN members OR an explicit ht="0" exception — Excel displays
        // both as hidden, so skipping either is faithful.
        // Mirrors `fill_plan_page`.
        if cw <= 0.0 || ch <= 0.0 {
            return;
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
            ref_a1: format!("{}{}", col_letters(cm.col), row),
            raw: cm.raw.clone(),
            is_merged_anchor,
        });
    };

    let mut cells: Vec<PageCell> = Vec::new();
    let mut merges: Vec<MergeBox> = Vec::new();

    // ── Anchors above the window (merge-across-boundary) ────────────────────
    for region in &above {
        let Some(rd) = w.rows.lookup(region.min_row) else { continue };
        let Some(cm) = rd.cells.iter().find(|c| c.col == region.min_col) else { continue };
        if cm.col < min_col || cm.col > max_col {
            continue;
        }
        let y = geom.row_top(region.min_row);
        emit(region.min_row, y, height(region.min_row), cm, &anchor_of, &mut cells, &mut merges);
    }

    // ── Window rows: rails + cells ───────────────────────────────────────────
    // Base offset closed-form (full range: row_top(min_row) == 0, bit-exact);
    // within-window heights accumulate sequentially (flush's FP order).
    let mut row_rails = Vec::with_capacity((w.r1 - w.r0 + 2) as usize);
    let mut row_numbers = Vec::with_capacity((w.r1 - w.r0 + 1) as usize);
    let mut y = geom.row_top(w.r0);
    row_rails.push(y);
    for r in w.r0..=w.r1 {
        let h = height(r);
        if r <= geom.max_row {
            if let Some(rd) = w.rows.lookup(r) {
                for cm in &rd.cells {
                    if cm.col < min_col || cm.col > max_col {
                        continue;
                    }
                    if covered.contains(&(r, cm.col)) {
                        continue;
                    }
                    emit(r, y, h, cm, &anchor_of, &mut cells, &mut merges);
                }
            }
        }
        y += h;
        row_rails.push(y);
        row_numbers.push(r);
    }

    SheetPage {
        sheet_name: index.name.clone(),
        col_rails: col_x,
        row_rails,
        cells,
        show_grid_lines: index.show_grid_lines,
        // Full content extents (incl. any void extension) — the painter's
        // spill-clip horizon and edge clamps; NOT part of the geometry hash.
        content_w: geom.content_w.max(w.extend_to_x.min(1e15)),
        content_h: geom.content_h.max(y),
        first_col: min_col,
        row_numbers,
        merges,
        // Pictures: absolute boxes off the closed-form geom (the painter
        // culls to the visible window itself).
        images: geom.images.clone(),
        // Embedded charts: same absolute-box contract as the pictures.
        charts: geom.charts.clone(),
    }
}

// ─── print clip: the value-bearing range ─────────────────────────────────────

/// The PRINT-mode pagination range (the default, with no toggle):
/// the v1 used range CLIPPED to the value-bearing extent — content cells ∪
/// custom-height rows ∪ merge regions (drawings extend inside `paginate`
/// itself, exactly as v1). The top-left keeps v1's corner (leading empty rows
/// print as in v1 — pin safety); only trailing void is clipped. Dimension-distrust: content
/// beyond the explicit-row range (r-less tails) EXTENDS the range — an
/// understating stamp never cuts content. A sheet with present-but-valueless
/// rows clips to a default blank grid; a fully-empty sheet returns `None`
/// (paginate's empty-sheet synthesis).
pub(crate) fn print_range(index: &SheetIndex) -> Option<(u32, u32, u32, u32)> {
    let v1 = index.used_range_v1?;

    // Value-bearing row/col bounds: content cells, @ht rows, merges.
    let mut rows_b: Option<(u32, u32)> = None;
    let mut cols_b: Option<(u32, u32)> = None;
    let mut fold_rows = |a: u32, b: u32| {
        rows_b = Some(match rows_b {
            None => (a, b),
            Some((lo, hi)) => (lo.min(a), hi.max(b)),
        });
    };
    let mut fold_cols = |a: u32, b: u32| {
        cols_b = Some(match cols_b {
            None => (a, b),
            Some((lo, hi)) => (lo.min(a), hi.max(b)),
        });
    };
    if let Some((r0, c0, r1, c1)) = index.content_bounds {
        fold_rows(r0, r1);
        fold_cols(c0, c1);
    }
    // A custom-height row is the ONE fold whose row number comes straight
    // off the scan rather than out of an already-bounded range, so an
    // unaddressable `<row r="4000000000" ht="20"/>` would re-inflate `max_row`
    // here (`paginate` then walks four billion rows). Such a row is DROPPED —
    // the same direction `SheetIndex::build` and `build_rows` take, so the three
    // never disagree. Snapping it to the last grid row instead would inflate the
    // print range all the way to 1 048 576, i.e. invent the pagination the drop
    // exists to avoid. See `model::MAX_ADDRESSABLE_ROW`.
    for e in index.rows.iter().filter(|e| e.ht_pt.is_some() && e.row <= MAX_ADDRESSABLE_ROW) {
        fold_rows(e.row, e.row);
    }
    for m in &index.merges {
        fold_rows(m.min_row, m.max_row);
        fold_cols(m.min_col, m.max_col);
    }

    // Nothing value-bearing: a default blank grid anchored at v1's corner.
    let (vr0, vr1) = rows_b
        .unwrap_or((v1.0, v1.2.min(v1.0 + EMPTY_SHEET_DEFAULT_ROWS - 1)));
    let (vc0, vc1) = cols_b.unwrap_or((1, EMPTY_SHEET_DEFAULT_COLS));

    let min_row = v1.0.min(vr0);
    let max_row = vr1.max(min_row);
    // v1 col bounds are cell-derived (may be degenerate MAX/0 on cell-less
    // sheets); the value bounds subsume them, healing the degenerate case.
    let min_col = if v1.1 == u32::MAX { vc0 } else { v1.1.min(vc0) };
    let max_col = vc1.max(min_col);
    Some((min_row, min_col, max_row, max_col))
}
