//! Excel (SpreadsheetML) renderer.
//!
//! The web-sys-free foundation: an owned [`model::SheetModel`] from
//! `ooxml_sml`'s parsed `Worksheet` / `ResolvedSheet`, a [`grid`] geometry layer
//! that turns the used range into print-page [`grid::SheetPage`] structs with
//! deterministic geometry hashes, and a [`shim`] that contains the strict
//! `<col>` `@min`/`@max` parse failure re-armed by `sml-styling`.
//!
//! It is wired into the `Viewer` facade: [`SmlRenderer`] owns the workbook +
//! the tolerant-loaded sheets, builds [`model::SheetModel`]s (measuring MDW from
//! the loaded normal font), paginates them, and streams the pages into the
//! Viewer's SHARED `sml_pages` / `layout_gen` / page-ready callback (option-A
//! borrowed shared state — the renderer holds NO back-reference to `Viewer`,
//! avoiding the doc-switch RefCell aliasing failure mode). [`render`] paints a
//! page through the shared `engine::paint` / `engine::fonts` seam.
//!
//! Spec grounding (ECMA-376 Part 1, §18 SpreadsheetML):
//!  - §18.3.1.13  `col@width` — character units of the normal font's Maximum
//!    Digit Width; the width↔pixel formulas live in [`grid`].
//!  - §18.3.1.35  `dimension` — the used range; sparse iteration over it.
//!  - §18.3.1.73  `row@ht` — points.
//!  - §18.3.1.81  `sheetFormatPr` — defaultRowHeight / defaultColWidth.
//!  - §18.3.1.88  `sheetView@showGridLines` — default true.

#![cfg_attr(not(test), allow(dead_code))]

pub(crate) mod drawings;
pub(crate) mod grid;
pub(crate) mod index;
pub(crate) mod model;
pub(crate) mod numfmt;
pub(crate) mod pane;
pub(crate) mod render;
pub(crate) mod select;
pub(crate) mod shim;
pub(crate) mod window;

#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use wasm_bindgen::JsValue;

use crate::engine::fonts::FontCache;
use crate::engine::opc;

use grid::{paginate, PageBox, SheetPage};
use index::{IndexGeom, SheetIndex};
use model::SheetModel;
use shim::SheetWarning;

/// The Excel renderer's document-specific state — Grid v2 (the lazy-index
/// model).
///
/// Load builds one cheap O(bytes) [`SheetIndex`] per sheet (pre-scan + husk
/// parse through the EXISTING fork seams) — the eager full materialization
/// (`load_sheets_tolerant` → `resolved_sheet` → `from_resolved` over all
/// cells) is GONE from the load path. Everything
/// geometric (extent, scroll mapping, rails) answers closed-form off the
/// index; cell CONTENT materializes per scroll window via synthetic-envelope
/// slice parses cached in a chunk LRU (`window.rs`).
pub(crate) struct SmlRenderer {
    /// Per-sheet lazy indexes in workbook order (failed-and-unrecoverable
    /// sheets dropped — the shim's Layer-1 containment, now per index build).
    pub(crate) indexes: Vec<SheetIndex>,
    /// Workbook shared strings (§18.4), self-parsed at load via the fork's
    /// generated `SharedStrings` parser (workbook-global, eager by design:
    /// every window needs it). Window parses hand each slice's `ResolvedSheet` a copy.
    shared_strings: Vec<String>,
    /// Resolved normal-style font family (`fonts[0].name` or Calibri fallback,
    /// §18 normal style — a simplification of the full normal-style chain).
    normal_font: String,
    /// Owned copy of the workbook stylesheet (§18.8): the per-cell style
    /// resolver walks `cellXfs` → font/fill/border/numFmt from it at
    /// window-materialization time. Cloned because the `Workbook` is dropped.
    stylesheet: ooxml_sml::types::Stylesheet,
    /// Workbook-level colour resolution context: theme palette (self-parsed
    /// from `xl/theme/theme1.xml`) + effective indexed palette (§18.8.27).
    color_ctx: model::ColorCtx,
    /// Anchored pictures per SHEET NAME (§18.3.1.36 + §20.5), OPC-parsed
    /// at load through the shared package handle (no bytes are retained
    /// afterwards). Keyed by name (not index) because a
    /// dropped sheet shifts `indexes` off workbook order.
    pub(crate) drawings: HashMap<String, Vec<drawings::SheetDrawing>>,
    /// Anchored CHARTS per SHEET NAME (embedded charts; same anchor geometry
    /// as pictures, `image_key` = the sheet-namespaced CHART key
    /// `chart::sml::{sheetIdx}::{relId}`). Placed/painted like the drawing
    /// layer but rendered through the SHARED `crate::chart::render_chart`.
    pub(crate) charts: HashMap<String, Vec<drawings::SheetDrawing>>,
    /// Picture media bytes collected from the drawing rels at load, keyed by
    /// the sheet-namespaced image key (`sml::{sheetIdx}::{relId}`). The Viewer
    /// drains these into its shared `images` map so the JS decode pipeline
    /// (imageRefs → createImageBitmap → setImageBitmap) turns them into
    /// `ImageBitmap`s — the exact PML extract-at-load contract.
    images: Vec<(String, crate::engine::images::ImageBytes)>,
    /// Raw chart-part XML collected at load, keyed by the chart key. The
    /// Viewer drains these into its shared `chart_xml` map (the exact
    /// `PmlRenderer::take_charts` contract) — the paint path reads them back
    /// via the borrowed map, so charts survive the drain.
    chart_parts: Vec<(String, Vec<u8>)>,
    /// Per-sheet load warnings (e.g. `<col>` widths repaired / sheet dropped),
    /// for later surfacing via documentInfo.
    warnings: Vec<SheetWarning>,
    /// The print-page box every sheet paginates into (one shared print page;
    /// per-sheet page sizing is not implemented).
    page_box: PageBox,
    /// Per-sheet closed-form geometry (built on first grid use with the
    /// then-measured MDW — the same build-once semantics the old continuous
    /// page cache had). Replaces the materialized `continuous` cache.
    geoms: RefCell<HashMap<usize, Rc<GeomState>>>,
    /// The chunk LRU: materialized row slices keyed (sheet, chunk).
    /// SHARED between the grid view's windows and the lazy paged fills.
    chunks: RefCell<window::ChunkStore>,
    /// Per-sheet lazy print plans (metadata-only page math), built as
    /// `run_layout` streams. Indexed by plan order (skipped sheets absent).
    plans: RefCell<Vec<PlanEntry>>,
    /// Flat streamed-page index → its plan page + materialized flag, parallel
    /// to the Viewer's shared `sml_pages`. A page starts as a skeleton and is
    /// filled (cells via the chunk LRU) on first paint — the filled `SheetPage`
    /// in `sml_pages` IS the per-page geometry cache (repaints are free).
    page_map: RefCell<Vec<PageSlot>>,
}

/// One sheet's lazy pagination plan + the workbook sheet it belongs to.
struct PlanEntry {
    sheet_idx: usize,
    plan: Rc<grid::SheetPlan>,
}

/// Where a flat streamed page lives in the plans, and whether its cells have
/// been materialized into the shared `sml_pages` yet.
struct PageSlot {
    plan: usize,
    page: usize,
    filled: bool,
}

/// One matching cell from [`SmlRenderer::search_sheet`] (full-text
/// search): the FLAT page index it lives on (into `sml_pages` / the JS page
/// list), the sheet name (the Excel tab the TS layer selects when navigating
/// across the workbook), and the cell's PAGE-LOCAL pixel rect (the lib.rs facade
/// adds page margins + gutters for a viewport coordinate).
pub(crate) struct SheetSearchHit {
    pub(crate) flat_page: usize,
    pub(crate) sheet_name: String,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) w: f64,
    pub(crate) h: f64,
    /// The cell's A1 ref (e.g. `"C7"`) — the identity the painter uses to pick the
    /// ACTIVE match (windowing/merge-invariant; see `render::SearchHighlight`).
    pub(crate) ref_a1: String,
    /// CONTENT-space (gutter-exclusive, pre-zoom) grid coordinates of the cell's
    /// top-left — `IndexGeom::col_left(col)` / `row_top(row)`. These let the
    /// continuous-grid scroller jump to a match: the print-page `x`/`y` above are
    /// laid out on an off-to-the-side print page whose origin has NO relationship
    /// to the continuous grid's content origin, so the grid path must re-source
    /// the position from the row/col rails, not convert the print rect.
    pub(crate) grid_x: f64,
    pub(crate) grid_y: f64,
}

/// A sheet's closed-form geometry + the MDW it was measured with.
pub(crate) struct GeomState {
    pub(crate) geom: IndexGeom,
    pub(crate) mdw: f64,
}

impl SmlRenderer {
    /// Build a renderer from raw `.xlsx` bytes — Grid v2 lazy load. Parses the
    /// workbook envelope (`Workbook::from_reader` over a `Cursor`), resolves
    /// the normal-style font + stylesheet + theme + shared strings (all
    /// workbook-global, eager by design), then builds one O(bytes)
    /// [`SheetIndex`] per sheet from its raw XML (`sheet_xml`, the shim's seam).
    /// NO sheet is fully materialized at load.
    ///
    /// MDW measurement is deferred to first geometry use (fonts arrive after
    /// load via `loadFont`), so this constructor needs no `FontCache`.
    pub(crate) fn from_xlsx(bytes: Vec<u8>) -> Result<Self, JsValue> {
        // The Workbook cursor BORROWS the retained bytes (the old
        // `bytes.clone()` copy was pure waste; `Cursor<&[u8]>` is `Read+Seek`).
        // (RULE 4 RETIRED: absolute rels Targets are resolved by the parser
        // itself now — ooxml-opc resolve_part_path — so the straight open
        // handles absolute and relative rels Targets directly.)
        let mut wb = ooxml_sml::workbook::Workbook::from_reader(std::io::Cursor::new(&bytes[..]))
            .map_err(|e| JsValue::from_str(&format!("xlsx parse failed: {e}")))?;

        // Normal-style font name + owned stylesheet copy (per-cell style resolution).
        let normal_font = model::normal_font_name(wb.styles());
        let stylesheet = wb.styles().clone();

        // ONE shared OPC open for the whole load — the theme, SST
        // and drawings self-parses below all read through this
        // handle instead of each re-opening the archive over a fresh copy. An
        // unopenable package hands every site `None` → its own empty-value default
        // (the exact per-site posture the independent opens had).
        let mut opc_pkg = opc::OpcPackage::open(&bytes);

        // Workbook colour context — theme palette via the shared OPC
        // self-parse of the theme part, indexed palette from the stylesheet.
        let color_ctx = model::ColorCtx::build(parse_workbook_theme(opc_pkg.as_mut()), wb.styles());

        // Grid v2: shared strings via the shared OPC handle + the fork's
        // generated `SharedStrings` parser (the lazy path never calls
        // `resolved_sheet`, which was the only SST conduit).
        let shared_strings = parse_workbook_sst(opc_pkg.as_mut());

        // Per-sheet lazy index (pre-scan + husk parse). `<col>` containment: a
        // sheet whose husk cannot parse even after the <col> repair is dropped
        // with a warning, never failing the workbook (the shim's Layer 1).
        let names: Vec<String> = wb.sheet_names().iter().map(|s| s.to_string()).collect();
        let mut indexes = Vec::with_capacity(wb.sheet_count());
        let mut warnings = Vec::new();
        for i in 0..wb.sheet_count() {
            let name = names.get(i).cloned().unwrap_or_else(|| format!("Sheet{}", i + 1));
            match wb.sheet_xml(i) {
                Ok(xml) => match SheetIndex::build(name, xml) {
                    Ok(idx) => {
                        if idx.col_widths_degraded {
                            warnings.push(SheetWarning::ColWidthsRepaired { sheet: i });
                        }
                        indexes.push(idx);
                    }
                    Err(reason) => warnings.push(SheetWarning::SheetDropped { sheet: i, reason }),
                },
                Err(e) => warnings.push(SheetWarning::SheetDropped {
                    sheet: i,
                    reason: format!("{e}"),
                }),
            }
        }

        // Anchored pictures + embedded charts — the same shared OPC handle
        // (sheet rels → drawing part → xdr anchors → media / chart-part bytes).
        let wd = drawings::parse_workbook_drawings(opc_pkg.as_mut());

        // NOTE: the original bytes are NOT retained — every OPC self-parse
        // (theme, SST, drawings) happens above through the one
        // shared `OpcPackage`, so nothing reads the bytes after
        // load.
        Ok(SmlRenderer {
            indexes,
            shared_strings,
            drawings: wd.drawings,
            charts: wd.charts,
            images: wd.images,
            chart_parts: wd.chart_parts,
            normal_font,
            stylesheet,
            color_ctx,
            warnings,
            page_box: PageBox::default(),
            geoms: RefCell::new(HashMap::new()),
            chunks: RefCell::new(window::ChunkStore::new(window::CHUNK_CAPACITY)),
            plans: RefCell::new(Vec::new()),
            page_map: RefCell::new(Vec::new()),
        })
    }

    /// The print-page box (px). The Viewer mirrors this into its shared
    /// `page_size` so `pageSize()` returns the SML print-page geometry.
    pub(crate) fn page_box(&self) -> PageBox {
        self.page_box
    }

    /// Whether any sheet was loaded degraded (warnings recorded).
    pub(crate) fn warnings(&self) -> &[SheetWarning] {
        &self.warnings
    }

    /// The picture media bytes collected at load, keyed by sheet-namespaced
    /// image key. The Viewer drains these into its shared `images` map (then JS
    /// decodes them to `ImageBitmap`s). Returns owned pairs (the renderer keeps
    /// none) — mirrors `PmlRenderer::take_images` exactly.
    pub(crate) fn take_images(&mut self) -> Vec<(String, crate::engine::images::ImageBytes)> {
        std::mem::take(&mut self.images)
    }

    /// The raw chart-part XML collected at load, keyed by the sheet-namespaced
    /// chart key (`chart::sml::{sheetIdx}::{relId}`). The Viewer drains these
    /// into its shared `chart_xml` map so the grid/print paint and the copy
    /// seams (`chartData` / `renderChartToCanvas`) feed the SHARED
    /// `crate::chart` painter — mirrors `PmlRenderer::take_charts` exactly.
    pub(crate) fn take_charts(&mut self) -> Vec<(String, Vec<u8>)> {
        std::mem::take(&mut self.chart_parts)
    }

    /// Sheet `sheet_idx`'s anchored-chart placements (empty slice when none).
    pub(crate) fn chart_anchors(&self, sheet_idx: usize) -> &[drawings::SheetDrawing] {
        self.indexes
            .get(sheet_idx)
            .and_then(|idx| self.charts.get(&idx.name))
            .map_or(&[], |v| v.as_slice())
    }

    /// Resolved normal-style font family.
    /// Hit-test a page-content coordinate to its cell (formula-bar readout).
    /// `x`/`y` are CONTENT coordinates (caller subtracts the page margins).
    /// Returns (A1 ref, raw value, formatted display text). Static over the
    /// shared pages; a lazy-page caller (lib.rs `cellAt`) runs `ensure_page`
    /// first so a not-yet-painted page resolves through the same per-page
    /// materialization.
    pub(crate) fn cell_at(
        pages: &std::cell::RefCell<Vec<grid::SheetPage>>,
        page_idx: usize,
        x: f64,
        y: f64,
    ) -> Option<(String, String, String)> {
        let pages = pages.borrow();
        let page = pages.get(page_idx)?;
        // The grid content origin is shifted right+down by the header
        // gutters; the painter applied the SAME shift via `gutter_dims`, so
        // subtract it here to map the content coordinate back to a page-local
        // cell box. A click inside the gutter bands lands at negative x/y and
        // matches nothing (no cell box has a negative origin).
        let (row_gutter_w, col_gutter_h) = grid::gutter_dims(page);
        let x = x - row_gutter_w;
        let y = y - col_gutter_h;
        // Last match wins: merged anchors are pushed with expanded extents and
        // should win over any overlap ambiguity at the rails.
        let mut hit: Option<(String, String, String)> = None;
        for c in &page.cells {
            if x >= c.x && x <= c.x + c.w && y >= c.y && y <= c.y + c.h {
                hit = Some((c.ref_a1.clone(), c.raw.clone(), c.text.clone()));
            }
        }
        if hit.is_some() {
            return hit;
        }
        // No materialized cell box — but an in-rail coordinate is still a CELL,
        // just an empty one (same distinction as `grid_cell_at`: only gutter/
        // outside clicks mean "nothing"). Derive the ref from the page's rails:
        // band k between col_rails[k]..[k+1] is column `first_col + k`; row
        // bands map through `row_numbers` (merged regions never reach here —
        // their expanded anchor box wins the loop above).
        let in_cols = page.col_rails.first().is_some_and(|&l| x >= l)
            && page.col_rails.last().is_some_and(|&r| x <= r);
        let in_rows = page.row_rails.first().is_some_and(|&t| y >= t)
            && page.row_rails.last().is_some_and(|&b| y <= b);
        if in_cols && in_rows {
            let k = page.col_rails.partition_point(|&r| r <= x).saturating_sub(1);
            let j = page.row_rails.partition_point(|&r| r <= y).saturating_sub(1);
            if let Some(&row) = page.row_numbers.get(j) {
                let col = page.first_col + k as u32;
                return Some((
                    format!("{}{}", grid::col_letters(col), row),
                    String::new(),
                    String::new(),
                ));
            }
        }
        None
    }

    /// Ensure the closed-form geometry for `sheet_idx` (built on first use with
    /// the then-measured MDW — the same build-once semantics the materialized
    /// continuous cache had; scroll frames hit the cache).
    pub(crate) fn ensure_geom(&self, sheet_idx: usize, font_cache: &FontCache) -> Option<Rc<GeomState>> {
        if let Some(g) = self.geoms.borrow().get(&sheet_idx) {
            return Some(g.clone());
        }
        let idx = self.indexes.get(sheet_idx)?;
        let mdw = model::measure_mdw(&self.normal_font, font_cache);
        let empty = Vec::new();
        let drawings = self.drawings.get(&idx.name).unwrap_or(&empty);
        let charts = self.charts.get(&idx.name).unwrap_or(&empty);
        let g = Rc::new(GeomState { geom: idx.geom_v2(mdw, drawings, charts), mdw });
        self.geoms.borrow_mut().insert(sheet_idx, g.clone());
        Some(g)
    }

    /// One LRU chunk of materialized rows: cached, or a synthetic-
    /// envelope slice parse on miss. Misses are bounded per window (a viewport
    /// covers 1–2 chunks), so the fill phase stays ms-class.
    fn chunk(&self, sheet_idx: usize, chunk_id: u32) -> Option<Rc<Vec<model::RowData>>> {
        let idx = self.indexes.get(sheet_idx)?;
        Some(self.chunks.borrow_mut().get_or_build((sheet_idx, chunk_id), || {
            let (r0, r1) = window::chunk_rows(chunk_id);
            window::parse_rows_window(
                idx, r0, r1, &self.shared_strings, &self.stylesheet, &self.color_ctx,
            )
        }))
    }

    /// The chunks covering rows `[r0, r1]` plus the anchor chunks of any merge
    /// region intersecting the window from above (the anchor outside the
    /// materialized window must still paint).
    fn rows_view(&self, sheet_idx: usize, r0: u32, r1: u32) -> window::RowsView {
        let mut ids: Vec<u32> = (window::chunk_of(r0)..=window::chunk_of(r1)).collect();
        if let Some(idx) = self.indexes.get(sheet_idx) {
            for m in &idx.merges {
                if m.min_row < r0 && m.max_row >= r0 {
                    let id = window::chunk_of(m.min_row);
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
        }
        let chunks = ids.into_iter().filter_map(|id| self.chunk(sheet_idx, id)).collect();
        window::RowsView::new(chunks)
    }

    /// Build the windowed `SheetPage` for rows `[r0, r1]` (kill-the-void: rails
    /// extend past the sheet extent to `r1`/`extend_to_x`). The full-range,
    /// no-extension call is the v1 equivalence surface the oracles pin.
    pub(crate) fn window_page(
        &self,
        sheet_idx: usize,
        font_cache: &FontCache,
        r0: u32,
        r1: u32,
        extend_to_x: f64,
    ) -> Option<SheetPage> {
        let gs = self.ensure_geom(sheet_idx, font_cache)?;
        let idx = self.indexes.get(sheet_idx)?;
        let fetch_hi = r1.min(gs.geom.max_row);
        let rows = self.rows_view(sheet_idx, r0, fetch_hi.max(r0));
        Some(window::build_window_page(&window::WindowSpec {
            index: idx,
            geom: &gs.geom,
            mdw: gs.mdw,
            rows: &rows,
            r0,
            r1,
            extend_to_x,
        }))
    }

    /// Full extent of a sheet's grid INCLUDING the sticky gutters — the scroll
    /// container's spacer size. Closed-form off the index: the
    /// stamped extent is honored with NO row walk (extent fidelity), content
    /// beyond an understating stamp extends it (dimension-distrust).
    pub(crate) fn grid_extent(&self, sheet_idx: usize, font_cache: &FontCache) -> Option<(f64, f64)> {
        self.ensure_geom(sheet_idx, font_cache).map(|gs| gs.geom.extent_px())
    }

    /// Paint one scroll-window of a sheet's grid (virtual scroller) — Grid v2:
    /// the window's rows materialize from the chunk LRU (hit: same frame;
    /// miss: a bounded ms-class slice parse), geometry is closed-form,
    /// and the void beyond the extent paints as real (empty) grid.
    /// `bitmaps` is the Viewer's shared decoded-image map (pictures).
    ///
    /// `zoom` (zoom-in-grid): the painter works entirely in
    /// CONTENT-space CSS px; zoom is ONE uniform ctx transform around the whole
    /// frame+fill paint (gutters scale too — Excel's headers shrink/grow with
    /// zoom) with the viewport dims divided into content space. `scroll_x`/
    /// `scroll_y` arrive in content space already (the adapter divides), so the
    /// window math below is zoom-blind by construction ([`grid_window`], native-
    /// tested). Canvas text under a 2D scale transform re-rasterizes crisply.
    /// In-process SHIM: apply the zoom transform to the CONCRETE ctx
    /// exactly where it was before, wrap it in a `CanvasPainter`, and relay to
    /// the painter-generic [`Self::paint_grid_into`]. The net ctx mutation
    /// sequence (save → scale → the identical `render::paint_grid_viewport`
    /// calls → restore) is byte-identical to a direct paint on the concrete
    /// ctx, so the in-process grid renders pixel-for-pixel the same.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_grid_viewport(
        &self,
        sheet_idx: usize,
        ctx: &web_sys::CanvasRenderingContext2d,
        scroll_x: f64,
        scroll_y: f64,
        view_w: f64,
        view_h: f64,
        zoom: f64,
        font_cache: &FontCache,
        bitmaps: &RefCell<std::collections::HashMap<String, web_sys::ImageBitmap>>,
        charts: &RefCell<std::collections::HashMap<String, Vec<u8>>>,
        selected_object: Option<&str>,
        search: Option<&render::SearchHighlight>,
        cell_sel: Option<select::CellSelection>,
    ) -> Result<(), JsValue> {
        // Same defensive clamp the core applies to `zoom` (an undefined/0 from a
        // stale JS caller) — applied here so the concrete `ctx.scale` matches the
        // windowing math's effective zoom EXACTLY (byte-identity).
        let zoom_c = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
        ctx.save();
        let images = bitmaps.borrow();
        let res = ctx.scale(zoom_c, zoom_c).and_then(|_| {
            let painter = crate::engine::paint::CanvasPainter::new(ctx, &images);
            self.paint_grid_into(
                &painter,
                sheet_idx,
                scroll_x,
                scroll_y,
                view_w,
                view_h,
                zoom,
                font_cache,
                &charts.borrow(),
                selected_object,
                search,
                cell_sel,
            )
        });
        ctx.restore();
        res
    }

    /// Painter-generic grid viewport paint (transform-FREE core, so the worker
    /// can replay it). Runs the pure windowing prelude then issues the SINGLE
    /// `render::paint_grid_viewport` call against `p`. The zoom transform is
    /// applied by the CALLER to the surface `p` wraps (the in-process shim
    /// scales the concrete ctx; the worker replay scales the TS ctx) — NOT baked
    /// here, so the recorded stream is zoom-independent content-space CSS px
    /// (scroll baked, no OP_SCALE prefix). `charts` is the already-borrowed
    /// chart-XML map; the dead `_bitmaps` render arg (never read — the painter
    /// owns image resolution) gets an empty map.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_grid_into(
        &self,
        p: &dyn crate::engine::paint::Painter,
        sheet_idx: usize,
        scroll_x: f64,
        scroll_y: f64,
        view_w: f64,
        view_h: f64,
        zoom: f64,
        font_cache: &FontCache,
        charts: &HashMap<String, Vec<u8>>,
        selected_object: Option<&str>,
        search: Option<&render::SearchHighlight>,
        cell_sel: Option<select::CellSelection>,
    ) -> Result<(), JsValue> {
        let Some(gs) = self.ensure_geom(sheet_idx, font_cache) else {
            return Ok(());
        };
        let geom = &gs.geom;
        // Cell selection — build the content-space selection paint geometry
        // (byte-identical render when `cell_sel` is None). Merge expansion is
        // applied inside `cell_sel_paint`.
        let csp = cell_sel.and_then(|sel| {
            self.indexes.get(sheet_idx).map(|idx| select::cell_sel_paint(geom, &idx.merges, &sel))
        });
        // Defensive clamp (an undefined/0 from a stale JS caller must not wedge
        // the paint in an infinite-window state).
        let zoom = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
        // Viewport dims in CONTENT space: at 50% zoom an 800px canvas shows
        // 1600 content px (more rows/cols), at 150% it shows ~533 (fewer).
        let (view_cw, view_ch) = (view_w / zoom, view_h / zoom);
        // Gutters from the EFFECTIVE extent's max row (constant across scroll —
        // matches the extent the adapter sized its spacer with).
        let gut_w = grid::row_gutter_width_for_max_row(geom.max_row);
        let gut_h = grid::COL_GUTTER_H;
        let (r0, r1, win_x1) = grid_window(geom, scroll_x, scroll_y, view_cw, view_ch);
        let Some(page) = self.window_page(sheet_idx, font_cache, r0, r1, win_x1) else {
            return Ok(());
        };
        // Frozen panes (§18.3.1.66, grid view): the pinned-row band page —
        // rows 1..=ySplit clipped to what the viewport can show. Pane-gated:
        // every pane-less sheet takes `None` and paints byte-identically.
        let pane_parts = self.pane_paint_parts(sheet_idx, font_cache, scroll_x, view_cw, view_ch, win_x1);
        let pane_ctx = pane_parts
            .as_ref()
            .map(|(bands, band_page)| render::PaneCtx { bands: *bands, band_page: band_page.as_ref() });
        render::paint_grid_viewport(
            p,
            &page,
            (gut_w, gut_h),
            scroll_x,
            scroll_y,
            view_cw,
            view_ch,
            &self.normal_font,
            font_cache,
            &HashMap::new(),
            charts,
            selected_object,
            search,
            csp.as_ref(),
            pane_ctx.as_ref(),
        )
    }

    /// The frozen-pane PAINT inputs for one viewport frame: the band geometry +
    /// the pinned-row band page (rows `min_row..=ySplit`, clipped to the
    /// viewport's visible band height with the SAME kill-the-void window math
    /// the body uses at zero y scroll). `None` for every pane-less sheet.
    /// Extracted from `paint_grid_viewport` so the native pane paint tests
    /// drive the EXACT production wiring (no web-sys).
    pub(crate) fn pane_paint_parts(
        &self,
        sheet_idx: usize,
        font_cache: &FontCache,
        scroll_x: f64,
        view_cw: f64,
        view_ch: f64,
        win_x1: f64,
    ) -> Option<(pane::PaneBands, Option<SheetPage>)> {
        let bands = self.pane_bands(sheet_idx, font_cache)?;
        let gs = self.ensure_geom(sheet_idx, font_cache)?;
        let geom = &gs.geom;
        let band_page = if bands.y_split > 0 {
            let band_vis_h = (view_ch - grid::COL_GUTTER_H).max(0.0).min(bands.band_h);
            let (br0, br1, _) =
                grid_window(geom, scroll_x, 0.0, view_cw, band_vis_h + grid::COL_GUTTER_H);
            let band_r1 = br1.min(bands.y_split.max(geom.min_row));
            self.window_page(sheet_idx, font_cache, br0, band_r1, win_x1)
        } else {
            None
        };
        Some((bands, band_page))
    }

    /// The frozen-pane band geometry for a sheet (§18.3.1.66) — `None` for the
    /// pane-less common case AND for `state="split"` (ignored by design). The
    /// SINGLE source both paint and hit-test consume (the shared quadrant
    /// mapping lives in [`pane::PaneBands`]).
    pub(crate) fn pane_bands(
        &self,
        sheet_idx: usize,
        font_cache: &FontCache,
    ) -> Option<pane::PaneBands> {
        let info = self.indexes.get(sheet_idx)?.pane?;
        let gs = self.ensure_geom(sheet_idx, font_cache)?;
        pane::PaneBands::build(&info, &gs.geom)
    }

    /// The grid pane info the TS adapter reads (frozen panes): band sizes in
    /// CONTENT px + the `topLeftCell`-derived initial BODY scroll (absent
    /// topLeftCell → A1 → zero scroll). `None` when the sheet has no frozen
    /// pane — the adapter then changes nothing.
    pub(crate) fn grid_pane_info(
        &self,
        sheet_idx: usize,
        font_cache: &FontCache,
    ) -> Option<(pane::PaneInfo, pane::PaneBands, f64, f64)> {
        let info = self.indexes.get(sheet_idx)?.pane?;
        let gs = self.ensure_geom(sheet_idx, font_cache)?;
        let bands = pane::PaneBands::build(&info, &gs.geom)?;
        // topLeftCell names the BODY pane's first visible cell: body content
        // starts at `scroll + band`, so scroll = offset(topLeft) − band (≥ 0).
        let (sx, sy) = match info.top_left {
            Some((row, col)) => {
                let geom = &gs.geom;
                let x = geom
                    .col_left(col.clamp(geom.min_col, geom.max_col + 1))
                    - bands.band_w;
                let y = geom.row_top(row) - bands.band_h;
                (x.max(0.0), y.max(0.0))
            }
            None => (0.0, 0.0),
        };
        Some((info, bands, sx, sy))
    }

    /// Formula-bar hit-test in the continuous grid: viewport coordinates +
    /// scroll offsets → cell. Grid v2: O(log) closed-form x/y→col/row
    /// mapping; content from the row's LRU chunk; a covered merge cell resolves
    /// to its anchor (the expanded-anchor-box behavior of the linear scan).
    ///
    /// All four coordinates are CONTENT-space px. Under grid zoom the
    /// adapter divides its canvas-relative CSS px by the zoom scale before
    /// calling — the engine stays zoom-blind here (the gutter check below
    /// composes: `view_x/z < gut_w  ⇔  view_x < gut_w·z`, the visual gutter).
    pub(crate) fn grid_cell_at(
        &self,
        sheet_idx: usize,
        view_x: f64,
        view_y: f64,
        scroll_x: f64,
        scroll_y: f64,
        font_cache: &FontCache,
    ) -> Option<(String, String, String)> {
        let gs = self.ensure_geom(sheet_idx, font_cache)?;
        let geom = &gs.geom;
        let gw = grid::row_gutter_width_for_max_row(geom.max_row);
        let gh = grid::COL_GUTTER_H;
        if view_x < gw || view_y < gh {
            return None;
        }
        // Frozen panes: a point inside a pinned band resolves with ZERO scroll
        // along the frozen axis (the shared quadrant mapping — the same
        // `eff_scroll` the selection hit-test uses; pane-less = identity).
        let (scroll_x, scroll_y) = match self.pane_bands(sheet_idx, font_cache) {
            Some(b) => b.eff_scroll(view_x - gw, view_y - gh, scroll_x, scroll_y),
            None => (scroll_x, scroll_y),
        };
        let x = view_x - gw + scroll_x;
        let y = view_y - gh + scroll_y;
        if x < 0.0 || y < 0.0 || x > geom.content_w || y > geom.content_h {
            return None;
        }
        let (mut row, mut col) = (geom.row_at_y(y), geom.col_at_x(x));
        // A covered cell of a merge resolves to the region's anchor (§18.3.1.55
        // — v1's expanded anchor box covered the whole region).
        let idx = self.indexes.get(sheet_idx)?;
        if let Some(region) = idx.merges.iter().find(|m| m.contains(row, col)) {
            row = region.min_row;
            col = region.min_col;
        }
        // An IN-BOUNDS coordinate always resolves to its ref. "Your selection
        // is A1 and it's empty" and "no meaningful selection" are different
        // statements — Excel's formula bar makes the distinction, and a
        // consumer formula bar can only make it if we do. The bounds checks
        // above still return None for gutter/outside clicks; from here down,
        // missing DATA means an empty cell, not a missing cell.
        let a1 = format!("{}{}", grid::col_letters(col), row);
        let Some(chunk) = self.chunk(sheet_idx, window::chunk_of(row)) else {
            return Some((a1, String::new(), String::new()));
        };
        let i = chunk.partition_point(|r| r.row < row);
        let Some(rd) = chunk.get(i).filter(|r| r.row == row) else {
            return Some((a1, String::new(), String::new()));
        };
        let Some(cm) = rd.cells.iter().find(|c| c.col == col) else {
            return Some((a1, String::new(), String::new()));
        };
        Some((a1, cm.raw.clone(), cm.text.clone()))
    }

    /// Geometry-only grid hit-test for cell selection.
    /// Content-space viewport coords + scroll offsets → zone/row/col (+ merge
    /// anchor). Resolves purely from the closed-form geometry (cell existence NOT
    /// required), so a drag anchored on an empty cell keeps a valid anchor.
    pub(crate) fn grid_hit_test(
        &self,
        sheet_idx: usize,
        view_x: f64,
        view_y: f64,
        scroll_x: f64,
        scroll_y: f64,
        font_cache: &FontCache,
    ) -> Option<select::GridHit> {
        let pane = self.pane_bands(sheet_idx, font_cache);
        let gs = self.ensure_geom(sheet_idx, font_cache)?;
        let idx = self.indexes.get(sheet_idx)?;
        Some(select::grid_hit_test(
            &gs.geom, &idx.merges, pane.as_ref(), view_x, view_y, scroll_x, scroll_y,
        ))
    }

    /// Chart-OBJECT hit-test in the continuous grid (xlsx embedded
    /// charts; the SML arm of `docHitTestObject` beside the `docHitTest` cell
    /// arm — the object/text split). Viewport
    /// CONTENT-space coords + content scroll offsets → the TOPMOST chart under
    /// the point (charts paint in document order, so the LAST hit wins) as
    /// `(chart key, content-space rect x/y/w/h)`. Frozen panes resolve through
    /// the SAME `eff_scroll` quadrant mapping the cell hit-test uses, so a
    /// chart pinned in a frozen band hits at its pinned position. `None` on a
    /// miss / a gutter point / a chart-less sheet (the common case — one empty
    /// slice iteration).
    pub(crate) fn grid_chart_hit(
        &self,
        sheet_idx: usize,
        view_x: f64,
        view_y: f64,
        scroll_x: f64,
        scroll_y: f64,
        font_cache: &FontCache,
    ) -> Option<(String, f64, f64, f64, f64)> {
        let gs = self.ensure_geom(sheet_idx, font_cache)?;
        let geom = &gs.geom;
        if geom.charts.is_empty() {
            return None;
        }
        let gw = grid::row_gutter_width_for_max_row(geom.max_row);
        let gh = grid::COL_GUTTER_H;
        if view_x < gw || view_y < gh {
            return None;
        }
        let (scroll_x, scroll_y) = match self.pane_bands(sheet_idx, font_cache) {
            Some(b) => b.eff_scroll(view_x - gw, view_y - gh, scroll_x, scroll_y),
            None => (scroll_x, scroll_y),
        };
        let x = view_x - gw + scroll_x;
        let y = view_y - gh + scroll_y;
        geom.charts
            .iter()
            .rev() // topmost = last painted
            .find(|ch| x >= ch.x && x <= ch.x + ch.w && y >= ch.y && y <= ch.y + ch.h)
            .map(|ch| (ch.key.clone(), ch.x, ch.y, ch.w, ch.h))
    }

    /// The DISPLAY px of chart `key` (its content-space anchor box) — the
    /// basis for the copy raster (`chartData`'s `wPx`/`hPx`, the exact WML
    /// `object_outline_rects` role). Scans each sheet's closed-form geometry;
    /// sheets are few and geoms cache, so this is cheap. `None` when no sheet
    /// places the key.
    pub(crate) fn chart_display_px(&self, key: &str, font_cache: &FontCache) -> Option<(f64, f64)> {
        for i in 0..self.indexes.len() {
            let Some(gs) = self.ensure_geom(i, font_cache) else { continue };
            if let Some(ch) = gs.geom.charts.iter().find(|c| c.key == key) {
                return Some((ch.w, ch.h));
            }
        }
        None
    }

    /// The USED-EXTENT box a header click selects. Keyed
    /// off `content_bounds` (the tight cell-bearing box), NOT the stamped/effective
    /// range (which honors `<dimension>` for scrollbar fidelity — the phantom "1M
    /// rows, 1% populated" case). An empty sheet falls back to the effective range
    /// (a real default grid), so a header click always yields a sane rect.
    pub(crate) fn content_bounds(&self, sheet_idx: usize) -> Option<(u32, u32, u32, u32)> {
        let idx = self.indexes.get(sheet_idx)?;
        Some(idx.content_bounds.unwrap_or_else(|| idx.effective_range()))
    }

    /// Extract the cells in `sel` (INDEX-RANGE walk, hidden cells
    /// INCLUDED, as Excel copies them) into a flavor-neutral [`select::CellFragment`]
    /// with the copy cap. ONE synchronous pass over the AREA-CLAMPED rect,
    /// counting rows + bytes and early-exiting at `MAX_COPY_ROWS`/`MAX_COPY_BYTES`
    /// (TRUNCATE + signal — no silent clip). Merges expand; model-less cells
    /// emit empty strings; covered merge cells are marked so TSV keeps them (empty
    /// column) while the HTML `<table>` skips them under the anchor's span.
    pub(crate) fn extract_cell_range(
        &self,
        sheet_idx: usize,
        sel: select::CellRange,
        font_cache: &FontCache,
    ) -> Option<select::CellFragment> {
        self.extract_cell_range_capped(
            sheet_idx,
            sel,
            font_cache,
            select::MAX_COPY_ROWS,
            select::MAX_COPY_BYTES,
        )
    }

    /// The capped extraction walk. The public [`extract_cell_range`] passes the
    /// production ceilings; tests inject tiny caps to exercise the truncation
    /// branches (byte cap, rowspan clamp) without 8MB / 50k-row fixtures.
    pub(crate) fn extract_cell_range_capped(
        &self,
        sheet_idx: usize,
        sel: select::CellRange,
        font_cache: &FontCache,
        max_rows: u32,
        max_bytes: usize,
    ) -> Option<select::CellFragment> {
        use select::{CellFragment, CellOut};
        // ensure_geom is not strictly needed for the walk, but a missing sheet is
        // the same None as every other accessor (and keeps the LRU warm/shared).
        let _ = self.ensure_geom(sheet_idx, font_cache)?;
        let idx = self.indexes.get(sheet_idx)?;
        let rng = select::expand_for_merges(sel, &idx.merges);
        let (r0, c0, r1, c1) = (rng.r0, rng.c0, rng.r1, rng.c1);

        // Merge bookkeeping over the (expanded) rect — clipped to the rect so a
        // pathological giant merge can't blow the sets up beyond the copy area.
        let mut covered: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
        let mut anchor_span: std::collections::HashMap<(u32, u32), (u32, u32)> =
            std::collections::HashMap::new();
        for m in &idx.merges {
            if m.max_row < r0 || m.min_row > r1 || m.max_col < c0 || m.min_col > c1 {
                continue;
            }
            anchor_span.insert(
                (m.min_row, m.min_col),
                (m.max_row - m.min_row + 1, m.max_col - m.min_col + 1),
            );
            for rr in m.min_row.max(r0)..=m.max_row.min(r1) {
                for cc in m.min_col.max(c0)..=m.max_col.min(c1) {
                    if !(rr == m.min_row && cc == m.min_col) {
                        covered.insert((rr, cc));
                    }
                }
            }
        }

        let total_rows = r1 - r0 + 1;
        let mut rows_out: Vec<Vec<CellOut>> = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        let mut reason: Option<&'static str> = None;
        let mut emitted = 0u32;
        // Reuse the LRU chunk across consecutive rows in the same chunk.
        let mut cur_chunk_id: Option<u32> = None;
        let mut cur_chunk: Option<std::rc::Rc<Vec<model::RowData>>> = None;

        for r in r0..=r1 {
            if emitted >= max_rows {
                truncated = true;
                reason = Some("rows");
                break;
            }
            let cid = window::chunk_of(r);
            if cur_chunk_id != Some(cid) {
                cur_chunk = self.chunk(sheet_idx, cid);
                cur_chunk_id = Some(cid);
            }
            let row_cells: Option<&[model::CellModel]> = cur_chunk.as_ref().and_then(|ch| {
                let i = ch.partition_point(|x| x.row < r);
                ch.get(i).filter(|x| x.row == r).map(|rd| rd.cells.as_slice())
            });

            let mut cells_out: Vec<CellOut> = Vec::with_capacity((c1 - c0 + 1) as usize);
            for c in c0..=c1 {
                if covered.contains(&(r, c)) {
                    cells_out.push(CellOut {
                        display: String::new(),
                        rowspan: 1,
                        colspan: 1,
                        covered: true,
                    });
                    continue;
                }
                let text = row_cells
                    .and_then(|rc| rc.iter().find(|cm| cm.col == c))
                    .map(|cm| cm.text.clone())
                    .unwrap_or_default();
                let (rowspan, colspan) = anchor_span.get(&(r, c)).copied().unwrap_or((1, 1));
                bytes += text.len() + 1;
                cells_out.push(CellOut { display: text, rowspan, colspan, covered: false });
            }
            rows_out.push(cells_out);
            emitted += 1;
            if bytes > max_bytes {
                truncated = true;
                reason = Some("bytes");
                break;
            }
        }

        // On cap-truncation, clamp each emitted anchor's rowspan to the rows
        // actually emitted below it: a vertical merge straddling the truncation
        // boundary would otherwise emit an over-spanning <td> (rowspan reaching
        // past the last emitted row). No-op when not truncated — the rect was
        // merge-expanded, so it always contains whole merges.
        if truncated {
            let n = rows_out.len() as u32;
            for (li, row) in rows_out.iter_mut().enumerate() {
                let max_rs = n - li as u32; // rows remaining, this row inclusive
                for cell in row.iter_mut() {
                    if cell.rowspan > max_rs {
                        cell.rowspan = max_rs;
                    }
                }
            }
        }

        Some(CellFragment { rows: rows_out, row_count: emitted, total_rows, truncated, reason })
    }

    pub(crate) fn normal_font(&self) -> &str {
        &self.normal_font
    }

    /// Every font family the workbook can paint with: the normal-style font
    /// plus each stylesheet `<font><name>` (per-cell fonts). Feeds the JS
    /// font plan via `fontFamiliesUsed` so substitutes load BEFORE layout —
    /// without this the xlsx path shaped with estimates and painted a system
    /// fallback.
    pub(crate) fn font_families(&self) -> Vec<String> {
        let mut out = vec![self.normal_font.clone()];
        if let Some(fonts) = self.stylesheet.fonts.as_deref() {
            for f in &fonts.font {
                if let Some(name) = f.name.as_deref() {
                    let n = name.value.trim();
                    if !n.is_empty() && !out.iter().any(|e| e == n) {
                        out.push(n.to_string());
                    }
                }
            }
        }
        out
    }

    /// Compute sheet `i`'s lazy pagination PLAN — pure metadata math off the
    /// [`SheetIndex`] (heights prefix over default-runs + `@ht` exceptions,
    /// col runs, the §18.3.1.63 downThenOver walk over numbers): NO cell
    /// parsing. The plan's range is the value-bearing print clip; its
    /// page boundaries are identical to the eager `paginate`'s because BOTH
    /// run [`grid::plan_pages`] over the same heights (`print_range` folds
    /// every `@ht` row into the clip, so the exception set is the model's).
    pub(crate) fn sheet_plan(&self, i: usize, font_cache: &FontCache) -> Option<grid::SheetPlan> {
        let idx = self.indexes.get(i)?;
        let dims = window::print_range(idx);
        let mdw = model::measure_mdw(&self.normal_font, font_cache);
        let empty = Vec::new();
        let drawings = self.drawings.get(&idx.name).unwrap_or(&empty);
        let charts = self.charts.get(&idx.name).unwrap_or(&empty);
        // Hidden rows are 0pt height exceptions (zero contribution — the same
        // rule `paginate` applies to the model rows, so plans stay equivalent).
        let heights: Vec<(u32, f64)> = idx
            .rows
            .iter()
            .filter_map(|e| {
                if e.hidden {
                    Some((e.row, 0.0))
                } else {
                    e.ht_pt.map(|h| (e.row, h))
                }
            })
            .collect();
        let col_width_chars = |c: u32| idx.col_width_chars(c);
        let col_hidden = |c: u32| idx.col_hidden(c);
        Some(grid::plan_pages(
            grid::PlanInput {
                name: &idx.name,
                show_grid_lines: idx.show_grid_lines,
                dims,
                default_row_height_pt: idx.default_row_height_pt,
                mdw,
                row_heights_pt: heights,
                col_width_chars: &col_width_chars,
                col_hidden: &col_hidden,
                drawings,
                charts,
                merges: idx.merges.clone(),
            },
            self.page_box,
        ))
    }

    /// Materialize the cells of streamed page `idx` into the shared
    /// `sml_pages` slot, if it is still a skeleton (a page's cells
    /// materialize through the chunk path when it becomes visible). The page's row
    /// range resolves through the SAME chunk LRU the grid view uses, and the
    /// filled `SheetPage` replaces the skeleton in place — the per-page
    /// geometry cache, keyed by page. Idempotent and cheap on repaints.
    pub(crate) fn ensure_page(&self, idx: usize, sml_pages: &RefCell<Vec<SheetPage>>) {
        let (plan_idx, page_no) = {
            let map = self.page_map.borrow();
            match map.get(idx) {
                Some(s) if !s.filled => (s.plan, s.page),
                // Already filled, or no plan entry (a doc mid-switch): no-op.
                _ => return,
            }
        };
        let (sheet_idx, plan) = {
            let plans = self.plans.borrow();
            let e = &plans[plan_idx];
            (e.sheet_idx, e.plan.clone())
        };
        let pp = &plan.pages[page_no];
        let rows = self.rows_view(sheet_idx, pp.r0, pp.r1);
        let page = grid::fill_plan_page(&plan, page_no, |r| {
            rows.lookup(r).map(|rd| rd.cells.as_slice())
        });
        {
            let mut pages = sml_pages.borrow_mut();
            let Some(slot) = pages.get_mut(idx) else { return };
            *slot = page;
        }
        self.page_map.borrow_mut()[idx].filled = true;
    }

    /// Materialize sheet `i`'s [`SheetModel`] over its PRINT range (the
    /// value-bearing clip): rows arrive via ONE synthetic-envelope window parse
    /// covering the clipped range; everything else is husk metadata. Where the
    /// clip equals the v1 used range (every truthful fixture), the model — and
    /// therefore `paginate`'s pages — is IDENTICAL to the old eager build (the
    /// pinned paged baselines verify this). Test oracle for the lazy plan+fill
    /// path (`run_layout` + `ensure_page` no longer route through it).
    pub(crate) fn materialize_model(&self, i: usize, font_cache: &FontCache) -> Option<SheetModel> {
        let idx = self.indexes.get(i)?;
        let dims = window::print_range(idx);
        let rows = match dims {
            Some((r0, _, r1, _)) => window::parse_rows_window(
                idx, r0, r1, &self.shared_strings, &self.stylesheet, &self.color_ctx,
            ),
            None => Vec::new(),
        };
        Some(SheetModel {
            name: idx.name.clone(),
            dimensions: dims,
            rows,
            col_runs: idx.col_runs.clone(),
            default_row_height_pt: idx.default_row_height_pt,
            default_col_width_chars: idx.default_col_width_chars,
            show_grid_lines: idx.show_grid_lines,
            mdw: model::measure_mdw(&self.normal_font, font_cache),
            col_widths_degraded: idx.col_widths_degraded,
            merges: idx.merges.clone(),
            drawings: self.drawings.get(&idx.name).cloned().unwrap_or_default(),
            charts: self.charts.get(&idx.name).cloned().unwrap_or_default(),
        })
    }

    /// Paginate every sheet into a flat `Vec<SheetPage>` (sheets concatenated in
    /// index order) — Grid v2: each sheet's model materializes lazily over its
    /// print range. Pure; the geometry the native tests pin.
    pub(crate) fn paginate_all(&self, font_cache: &FontCache) -> Vec<SheetPage> {
        let mut out = Vec::new();
        for i in 0..self.indexes.len() {
            if let Some(model) = self.materialize_model(i, font_cache) {
                out.extend(paginate(&model, self.page_box));
            }
        }
        out
    }

    /// Stream-paginate the workbook into the Viewer's SHARED `sml_pages`, firing
    /// `on_page` per page and honoring `layout_gen` for abort (option-A borrowed
    /// shared state — the renderer never holds a back-reference to `Viewer`).
    ///
    /// Mirrors `WmlRenderer`'s streaming contract: clears the shared page vec,
    /// pushes each `SheetPage`, fires the page-ready callback, and yields to the
    /// event loop between pages. Re-checks `layout_gen` after each yield so a
    /// concurrent document switch (`abortLayout` bumps the gen) bails cleanly
    /// without holding a borrow across the await (which would alias the RefCell).
    pub(crate) async fn run_layout(
        &self,
        sml_pages: &RefCell<Vec<SheetPage>>,
        layout_gen: &Cell<u64>,
        font_cache: &FontCache,
        mut on_page: impl FnMut(u32) -> Result<(), JsValue>,
    ) -> Result<u32, JsValue> {
        let my_gen = layout_gen.get();
        sml_pages.borrow_mut().clear();
        self.plans.borrow_mut().clear();
        self.page_map.borrow_mut().clear();

        // Grid v2 LAZY pagination: page row-ranges and column bands are
        // pure metadata math off the SheetIndex (`sheet_plan` — the
        // §18.3.1.63 walk over numbers, not cells). NO cell parsing happens
        // here: the streamed pages are skeletons whose cells materialize via
        // the chunk path when a page is first painted (`ensure_page`).
        // The main thread never owns O(print-range) parse work at load — a
        // 1M-row print range streams ~20k metadata pages in milliseconds.
        let mut count: u32 = 0;
        'sheets: for i in 0..self.indexes.len() {
            let Some(plan) = self.sheet_plan(i, font_cache) else { continue };
            let plan = Rc::new(plan);
            let plan_idx = {
                let mut plans = self.plans.borrow_mut();
                plans.push(PlanEntry { sheet_idx: i, plan: plan.clone() });
                plans.len() - 1
            };
            // Yield cadence: pages are metadata-cheap, so the batch scales with
            // the sheet's page count to bound a huge workbook at ~64 yields per
            // sheet (the page-ready callback still fires per page).
            //
            // An earlier floor of 8 amortised a CLAMPED `setTimeout` yield
            // across 8 pages (~4.4 ms/page, an estimate, not re-measured). A
            // yield is now a MessageChannel round trip, which is not clamped,
            // so that floor would buy nothing and cost abortability — a small
            // sheet would be up to 8 pages less abortable than it needs to be.
            // Floor of 1: yield per page
            // below 64 pages, which is ≤ ~1 ms of yield for the whole sheet.
            let yield_batch = (plan.pages.len() as u32 / 64).max(1);
            for p in 0..plan.pages.len() {
                // Abort if a new document started loading (gen bumped). Never
                // hold a borrow across the await; we only read the Cell here.
                if layout_gen.get() != my_gen {
                    break 'sheets;
                }
                let idx = {
                    let mut pages = sml_pages.borrow_mut();
                    pages.push(plan.skeleton_page(p));
                    pages.len() as u32 - 1
                };
                self.page_map.borrow_mut().push(PageSlot { plan: plan_idx, page: p, filled: false });
                on_page(idx)?;
                count = idx + 1;
                if count.is_multiple_of(yield_batch) {
                    crate::yield_to_browser().await;
                    if layout_gen.get() != my_gen {
                        break 'sheets;
                    }
                }
            }
            // Yield at least once per sheet so a huge workbook stays abortable.
            crate::yield_to_browser().await;
            if layout_gen.get() != my_gen {
                break;
            }
        }
        Ok(count)
    }

    /// Full-text search over ONE sheet's CELL MODEL
    /// — the key SML perf lever: matching reads the resolved cell strings via the
    /// chunk LRU (the SAME cache paint uses, so already-scrolled chunks are free
    /// and matches are byte-identical to painted cells), NOT the laid-out pages.
    /// NO non-matching page is materialized: locating a hit's page uses the
    /// metadata-only plan (`sheet_plan`, a pure number walk), and only the
    /// matching cell's page is laid out (one off-to-the-side `fill_plan_page`) to
    /// resolve the page-local rect. `sml_pages`/`ensure_page` are never touched,
    /// so the streamed page vec stays as lazy as before the search.
    ///
    /// Returns one [`SheetSearchHit`] per matching cell: the FLAT page index
    /// (into the Viewer's `sml_pages` / the JS page list), the sheet name, and
    /// the cell's page-local pixel rect (origin relative to the band; the facade
    /// adds page margins + gutters for a viewport coordinate, exactly as
    /// `cell_at` does in reverse). Whole-cell granularity — a cell is one logical
    /// unit (no wrapping), so a match yields exactly one rect.
    /// Full-sheet search (the 4-arg shim — byte-identical to an unwindowed walk).
    /// Kept stable for the native tests + the per-sheet benchmark.
    pub(crate) fn search_sheet(
        &self,
        sheet_idx: usize,
        needle: &str,
        opts: crate::SearchOpts,
        font_cache: &FontCache,
    ) -> Vec<SheetSearchHit> {
        self.search_sheet_window(sheet_idx, needle, opts, font_cache, None)
    }

    /// Windowed search core. `row_window = Some((r0, r1))` searches the
    /// HALF-OPEN 1-based row range `[r0, r1)` only; `None` = the whole sheet.
    /// Disjoint windows partition the sheet with NO double-count: each emitted
    /// row falls in exactly one window (the `rd.row` trim below drops chunk-edge
    /// rows outside `[r_lo, r_hi]`). The TS plugin drives row windows per tick so a
    /// huge sheet streams + can abort mid-sheet between windows.
    pub(crate) fn search_sheet_window(
        &self,
        sheet_idx: usize,
        needle: &str,
        opts: crate::SearchOpts,
        font_cache: &FontCache,
        row_window: Option<(u32, u32)>,
    ) -> Vec<SheetSearchHit> {
        let mut out = Vec::new();
        if needle.is_empty() {
            return out;
        }
        let Some(idx) = self.indexes.get(sheet_idx) else { return out };
        let name = idx.name.clone();
        // The flat-page offset of this sheet = Σ earlier plans' page counts (pages
        // stream in plan order, sheets concatenated; a skipped sheet has no plan
        // and contributes nothing). Reuse the plans `run_layout` already built;
        // fall back to a fresh `sheet_plan` if search runs before layout.
        let plan = {
            let plans = self.plans.borrow();
            plans.iter().find(|e| e.sheet_idx == sheet_idx).map(|e| e.plan.clone())
        };
        let plan = match plan {
            Some(p) => p,
            None => match self.sheet_plan(sheet_idx, font_cache) {
                Some(p) => Rc::new(p),
                None => return out,
            },
        };
        let first_flat_page: usize = {
            let plans = self.plans.borrow();
            if plans.is_empty() {
                0
            } else {
                plans
                    .iter()
                    .take_while(|e| e.sheet_idx != sheet_idx)
                    .map(|e| e.plan.pages.len())
                    .sum()
            }
        };
        // Walk the sheet's chunks (the SAME LRU paint uses) and substring-match
        // each resolved cell string. Only chunks that actually contain rows are
        // parsed (sparse-safe); other sheets are never touched.
        let Some(gs) = self.ensure_geom(sheet_idx, font_cache) else { return out };
        let max_row = gs.geom.max_row;
        // Clamp the requested window to [1, max_row]; `r_hi` is INCLUSIVE (the
        // half-open `r1` exclusive end → `r1 - 1`).
        let r_lo = row_window.map(|(a, _)| a).unwrap_or(1).max(1);
        let r_hi = row_window.map(|(_, b)| b.saturating_sub(1)).unwrap_or(max_row).min(max_row);
        if r_lo > r_hi {
            return out;
        }
        let mut chunk_id = window::chunk_of(r_lo);
        let last_chunk = window::chunk_of(r_hi.max(1));
        while chunk_id <= last_chunk {
            if let Some(chunk) = self.chunk(sheet_idx, chunk_id) {
                for rd in chunk.iter() {
                    // Trim chunk-edge rows outside the window so disjoint windows
                    // never double-count a row a shared boundary chunk holds.
                    if rd.row < r_lo || rd.row > r_hi {
                        continue;
                    }
                    // HIDDEN rows are skipped (Excel Find's default searches
                    // visible cells only); the match count reflects it.
                    if rd.hidden {
                        continue;
                    }
                    for cm in &rd.cells {
                        // Cells in HIDDEN columns are skipped too.
                        if idx.col_hidden(cm.col) {
                            continue;
                        }
                        // Cell-level match: a contains-check, NOT the full ranges Vec.
                        // find_matches allocated O(occurrences) per cell just to test
                        // emptiness — pathological on the huge SQL-log cells.
                        if crate::find_matches_any(&cm.text, needle, opts) {
                            out.push(self.locate_cell(
                                &plan, first_flat_page, sheet_idx, &name, rd.row, cm.col, &gs.geom,
                            ));
                        }
                    }
                }
            }
            chunk_id += 1;
        }
        out
    }

    /// Build the search hit for a matching cell `(row, col)`. The CONTENT-space
    /// grid coords (`grid_x`/`grid_y`) come straight from `geom` and are always
    /// valid — that is what the continuous grid (the SML default view) + the count
    /// use, so a hit is ALWAYS produced. The print-page rect (`flat_page`/x/y/w/h)
    /// is BEST-EFFORT: laid out from the plan page covering this cell's row AND
    /// column band (one off-to-the-side `fill_plan_page` over the chunk LRU); when
    /// the cell has no print page (beyond the print range) those degrade to the
    /// page origin with a zero rect — harmless, since it can't appear in print view.
    #[allow(clippy::too_many_arguments)]
    fn locate_cell(
        &self,
        plan: &grid::SheetPlan,
        first_flat_page: usize,
        _sheet_idx: usize,
        sheet_name: &str,
        row: u32,
        col: u32,
        geom: &IndexGeom,
    ) -> SheetSearchHit {
        let target = format!("{}{}", grid::col_letters(col), row);
        // O(1) hit: the print rect is DEFERRED. The count + the grid view (the
        // SML default) need only grid_x/grid_y (straight from `geom`, below) — they
        // never read x/y/w/h. Laying out a whole print page
        // (`fill_plan_page`, cloning ~25-50 rows of cell strings) PER MATCH just to
        // fill that rect would be O(M·page) — a freeze on a huge dense sheet. Instead
        // each match only number-walks `plan.pages` for a correct `flat_page` (which
        // `pushHighlight` uses to narrow the wasm paint pass); the exact print rect
        // is resolved LAZILY by `locate_cell_rect` for the single match the user
        // navigates to in Pages view (`resolveSearchRect`). Band-aware page pick
        // (row AND column band) still applies.
        let flat_page = plan
            .pages
            .iter()
            .position(|pp| {
                let band = &plan.bands[pp.band];
                row >= pp.r0 && row <= pp.r1 && col >= band.min_col && col <= band.max_col
            })
            .map(|p| first_flat_page + p)
            .unwrap_or(first_flat_page);
        SheetSearchHit {
            flat_page,
            sheet_name: sheet_name.to_string(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            ref_a1: target,
            // CONTENT-space grid top-left from the row/col rails — the space the
            // continuous-grid scroller (and `grid_cell_at`) work in, NOT the print
            // page's local origin. Gutter-exclusive (the DOM grid adapter adds the
            // sticky gutter itself). Always valid, independent of the print page.
            grid_x: geom.col_left(col),
            grid_y: geom.row_top(row),
        }
    }

    /// LAZY print-rect resolver — the expensive half of cell location,
    /// split out so it runs ONCE for the single match the user navigates to in
    /// Pages view, not per match at search time. Resolves the plan + flat-page
    /// offset (like `search_sheet`), band-aware-picks the print page for (row,col),
    /// lays out THAT page once (`fill_plan_page`) and returns the gutter-shifted
    /// page-local rect + its flat page. `None` beyond the print range (the consumer
    /// falls back to the page origin — harmless, the cell can't be in print view).
    pub(crate) fn locate_cell_rect(
        &self,
        sheet_idx: usize,
        row: u32,
        col: u32,
        font_cache: &FontCache,
    ) -> Option<(usize, f64, f64, f64, f64)> {
        let plan = {
            let plans = self.plans.borrow();
            plans.iter().find(|e| e.sheet_idx == sheet_idx).map(|e| e.plan.clone())
        };
        let plan = match plan {
            Some(p) => p,
            None => Rc::new(self.sheet_plan(sheet_idx, font_cache)?),
        };
        let first_flat_page: usize = {
            let plans = self.plans.borrow();
            if plans.is_empty() {
                0
            } else {
                plans
                    .iter()
                    .take_while(|e| e.sheet_idx != sheet_idx)
                    .map(|e| e.plan.pages.len())
                    .sum()
            }
        };
        let target = format!("{}{}", grid::col_letters(col), row);
        let page_no = plan.pages.iter().position(|pp| {
            let band = &plan.bands[pp.band];
            row >= pp.r0 && row <= pp.r1 && col >= band.min_col && col <= band.max_col
        })?;
        let pp = &plan.pages[page_no];
        let rows = self.rows_view(sheet_idx, pp.r0, pp.r1);
        let page = grid::fill_plan_page(&plan, page_no, |r| {
            rows.lookup(r).map(|rd| rd.cells.as_slice())
        });
        let cell = page.cells.iter().find(|c| c.ref_a1 == target)?;
        let (gut_w, gut_h) = grid::gutter_dims(&page);
        Some((first_flat_page + page_no, gut_w + cell.x, gut_h + cell.y, cell.w, cell.h))
    }

    /// The sheet index that flat page `flat_page` belongs to, using the same
    /// plan-order flat-page accumulation `search_sheet`/the page stream use. Lets
    /// the continuous-grid paint pass decide whether the active search match (held
    /// by `(flat_page, A1 ref)`) is on the sheet it is painting — the grid is
    /// per-sheet & continuous, so it can't narrow by flat page directly. `None`
    /// when no plan covers the page (search ran before layout).
    pub(crate) fn sheet_of_flat_page(&self, flat_page: usize) -> Option<usize> {
        let plans = self.plans.borrow();
        let mut acc = 0usize;
        for e in plans.iter() {
            let n = e.plan.pages.len();
            if flat_page < acc + n {
                return Some(e.sheet_idx);
            }
            acc += n;
        }
        None
    }

    /// Workbook colour-resolution context (theme + indexed palettes) — exposed
    /// for the native tests.
    #[cfg(test)]
    pub(crate) fn color_ctx(&self) -> &model::ColorCtx {
        &self.color_ctx
    }

    /// Whether any chunk of `sheet_idx` has been parsed into the LRU — the
    /// model-not-layout search invariant test pins that searching one sheet
    /// never parses ANOTHER sheet's chunks (its cells are never materialized).
    #[cfg(test)]
    pub(crate) fn any_chunk_loaded(&self, sheet_idx: usize) -> bool {
        let max_row = self
            .indexes
            .get(sheet_idx)
            .and_then(window::print_range)
            .map(|(_, _, r1, _)| r1)
            .unwrap_or(1);
        let max = window::chunk_of(max_row.max(1));
        let store = self.chunks.borrow();
        (0..=max).any(|c| store.contains((sheet_idx, c)))
    }

    /// Number of workbook sheets that survived the lazy index build — the
    /// workbook-scope search loops `0..sheet_count` over these. (`indexes` is
    /// `pub(crate)`; this is the named accessor the lib.rs facade reads.)
    pub(crate) fn sheet_count(&self) -> usize {
        self.indexes.len()
    }

    /// Number of materialized chunks in the LRU — the native tests assert the
    /// lazy load path parses NOTHING (skeletons only) until a page is painted.
    #[cfg(test)]
    pub(crate) fn chunk_count(&self) -> usize {
        self.chunks.borrow().len()
    }

    /// Paint page `idx` of the streamed `sml_pages` onto `ctx`, at the given
    /// page-local content origin. Delegates to [`render::paint_sheet_page`].
    /// `bitmaps` is the Viewer's shared decoded-image map (pictures).
    /// First paint of a lazy skeleton page materializes its cells here
    /// (`ensure_page` — the chunk LRU shared with the grid view).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_page(
        &self,
        idx: usize,
        sml_pages: &RefCell<Vec<SheetPage>>,
        origin_x: f64,
        origin_y: f64,
        page_w: f64,
        page_h: f64,
        font_cache: &FontCache,
        bitmaps: &RefCell<std::collections::HashMap<String, web_sys::ImageBitmap>>,
        charts: &RefCell<std::collections::HashMap<String, Vec<u8>>>,
        ctx: &web_sys::CanvasRenderingContext2d,
        search: Option<&render::SearchHighlight>,
    ) -> Result<(), JsValue> {
        self.ensure_page(idx, sml_pages);
        let pages = sml_pages.borrow();
        let page = pages
            .get(idx)
            .ok_or_else(|| JsValue::from_str("page index out of range"))?;
        let images = bitmaps.borrow();
        let painter = crate::engine::paint::CanvasPainter::new(ctx, &images);
        render::paint_sheet_page(
            &painter,
            page,
            origin_x,
            origin_y,
            page_w,
            page_h,
            &self.normal_font,
            font_cache,
            &bitmaps.borrow(),
            &charts.borrow(),
            search,
        )
    }

    /// Windowed variant of [`Self::render_page`] for the SML PRINT page:
    /// paints only the cells/merges/images intersecting `clip` (page-logical
    /// px), so the item walk is O(visible). Mirrors `paint_sheet_page` exactly
    /// except for the clip (cells cull by ROW BAND, preserving spill occupancy;
    /// gridlines/gutters are bounded-page chrome painted in full). The
    /// continuous GRID view already had its own windowed painter
    /// (`paint_grid_viewport`); this is the print-page analogue.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_page_window(
        &self,
        idx: usize,
        sml_pages: &RefCell<Vec<SheetPage>>,
        origin_x: f64,
        origin_y: f64,
        page_w: f64,
        page_h: f64,
        font_cache: &FontCache,
        bitmaps: &RefCell<std::collections::HashMap<String, web_sys::ImageBitmap>>,
        charts: &RefCell<std::collections::HashMap<String, Vec<u8>>>,
        ctx: &web_sys::CanvasRenderingContext2d,
        clip: &crate::cull::Rect,
        search: Option<&render::SearchHighlight>,
    ) -> Result<(), JsValue> {
        self.ensure_page(idx, sml_pages);
        let pages = sml_pages.borrow();
        let page = pages
            .get(idx)
            .ok_or_else(|| JsValue::from_str("page index out of range"))?;
        let images = bitmaps.borrow();
        let painter = crate::engine::paint::CanvasPainter::new(ctx, &images);
        // Window-local via the offset decorator (offset = window origin). SML draws
        // every cell/gridline/gutter/sticky-header at ABSOLUTE page-logical coords
        // through the painter (no direct ctx, no translate), so leaf subtraction in
        // the decorator makes them all window-local automatically.
        let off = crate::engine::paint::OffsetPainter::new(&painter, clip.x0, clip.y0);
        render::paint_sheet_page_clipped(
            &off,
            page,
            origin_x,
            origin_y,
            page_w,
            page_h,
            &self.normal_font,
            font_cache,
            &bitmaps.borrow(),
            &charts.borrow(),
            Some(clip),
            search,
        )
    }
}

/// The window a content-space viewport covers: `(r0, r1, win_x1)` — first/last
/// row to materialize rails for, and the content-x the kill-the-void column
/// extension must reach. Pure closed-form math off the index geometry (no
/// materialization, no web-sys), factored out of `paint_grid_viewport` so the
/// zoom-in-grid division (the adapter/painter pass CONTENT-space
/// viewport dims, i.e. CSS px ÷ zoom) is natively testable.
///
/// Window rows: closed-form y→row; past the extent continue at the default row
/// height (kill-the-void), capped at the §18 sheet limit (1,048,576).
pub(crate) fn grid_window(
    geom: &IndexGeom,
    scroll_x: f64,
    scroll_y: f64,
    view_w: f64,
    view_h: f64,
) -> (u32, u32, f64) {
    let gut_w = grid::row_gutter_width_for_max_row(geom.max_row);
    let gut_h = grid::COL_GUTTER_H;
    let win_y0 = scroll_y.max(0.0);
    let win_y1 = scroll_y + (view_h - gut_h).max(0.0);
    let win_x1 = scroll_x + (view_w - gut_w).max(0.0);
    let r0 = geom.row_at_y(win_y0);
    let r1 = if win_y1 <= geom.content_h || geom.default_row_px <= 0.0 {
        geom.row_at_y(win_y1)
    } else {
        let extra = ((win_y1 - geom.content_h) / geom.default_row_px).ceil() as u32;
        (geom.max_row.saturating_add(extra)).min(1_048_576)
    };
    (r0, r1, win_x1)
}

// ─── OPC self-parse: workbook theme palette ──────────────────────────────────

/// Relationship type of the shared-strings part (workbook rels →
/// `xl/sharedStrings.xml`). Not among the fork's `rel_type` consts, so spelled
/// here (the shared OFFICE_DOCUMENT/THEME types come from `opc::rel_type`).
const REL_SHARED_STRINGS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings";

/// Self-parse the workbook's shared strings (§18.4) through the load's SHARED
/// [`opc::OpcPackage`] (one archive open per document): walk root rels →
/// workbook part → sharedStrings rel, and parse the part through the fork's
/// GENERATED `SharedStrings::from_xml` (public via `ooxml_sml::parsers` — the
/// same FromXml machinery `parse_worksheet` drives; NO fork changes). String
/// extraction mirrors the fork's `extract_shared_strings` exactly (plain `<t>`
/// or concatenated rich-run `<r>/<t>` texts) so window-materialized cells
/// resolve identically to the eager path's. Any structural miss — including a
/// package that never opened (`None`) — returns an empty SST (shared-string
/// cells then render empty — the same failure shape the fork's own loader
/// has for an unreadable SST part).
fn parse_workbook_sst(pkg: Option<&mut opc::OpcPackage<'_>>) -> Vec<String> {
    use ooxml_sml::parsers::FromXml;
    use quick_xml::events::Event;

    let Some(pkg) = pkg else {
        return Vec::new();
    };
    // Root rels → workbook part path (fallback: the conventional location).
    let wb_path = pkg
        .office_document_target()
        .map(|t| t.trim_start_matches('/').to_string())
        .unwrap_or_else(|| "xl/workbook.xml".to_string());
    // Workbook rels → sharedStrings part path. NO conventional fallback here:
    // a workbook without the rel has no shared strings (the common case for
    // generated files), and probing would mask that.
    let Some(sst_path) = pkg
        .rels(&wb_path)
        .get_by_type(REL_SHARED_STRINGS)
        .map(|r| opc::resolve_target(&wb_path, &r.target))
    else {
        return Vec::new();
    };
    let Some(data) = pkg.read_part(&sst_path) else {
        return Vec::new();
    };

    // Drive the generated parser exactly like the fork's `bootstrap`: scan to
    // the first start tag, then `SharedStrings::from_xml`.
    let mut reader = quick_xml::Reader::from_reader(std::io::Cursor::new(data.as_slice()));
    let mut buf = Vec::new();
    let sst: Option<ooxml_sml::types::SharedStrings> = loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                break ooxml_sml::types::SharedStrings::from_xml(&mut reader, &e, false).ok()
            }
            Ok(Event::Empty(e)) => {
                break ooxml_sml::types::SharedStrings::from_xml(&mut reader, &e, true).ok()
            }
            Ok(Event::Eof) | Err(_) => break None,
            _ => {}
        }
        buf.clear();
    };
    let Some(sst) = sst else { return Vec::new() };
    // EXACT mirror of the fork's extract_shared_strings (workbook.rs).
    sst.si
        .iter()
        .map(|si| {
            if let Some(t) = &si.cell_type {
                t.clone()
            } else {
                si.reference.iter().map(|r| r.cell_type.as_str()).collect()
            }
        })
        .collect()
}

/// Self-parse the workbook's colour theme through the load's SHARED
/// [`opc::OpcPackage`] (the fork's `Workbook` exposes no theme-part accessor):
/// walk root rels → workbook part → theme rel, and scan the theme XML's
/// `<a:clrScheme>` (§20.1.6.2) via
/// [`crate::engine::color::extract_theme_color_hex`] (handles `srgbClr@val`
/// AND `sysClr@lastClr`).
///
/// Returns the 12 slots in Excel's `color@theme` INDEX order — i.e. with the
/// documented dk1/lt1 + dk2/lt2 SWAP applied (0→lt1, 1→dk1, 2→lt2, 3→dk2,
/// 4-9→accent1-6, 10→hlink, 11→folHlink): Excel writes white title text as
/// `<color theme="0"/>` = background 1 = lt1. Any structural miss (no theme
/// part, garbled rels) leaves the affected slots EMPTY — those colours then
/// resolve to `None` (no theme colour), graceful by construction.
fn parse_workbook_theme(pkg: Option<&mut opc::OpcPackage<'_>>) -> [String; 12] {
    use crate::engine::color::extract_theme_color_hex;

    let mut out: [String; 12] = Default::default();
    let Some(pkg) = pkg else {
        return out;
    };

    // Root rels → workbook part path (fallback: the conventional location).
    let wb_path = pkg
        .office_document_target()
        .map(|t| t.trim_start_matches('/').to_string())
        .unwrap_or_else(|| "xl/workbook.xml".to_string());

    // Workbook rels → theme part path (fallback: the conventional location).
    let theme_path = pkg
        .rels(&wb_path)
        .get_by_type(opc::rel_type::THEME)
        .map(|r| opc::resolve_target(&wb_path, &r.target))
        .unwrap_or_else(|| "xl/theme/theme1.xml".to_string());

    let Some(xml) = pkg.read_part_string(&theme_path) else {
        return out;
    };

    // clrScheme slot names in Excel `color@theme` index order (the swap).
    const SLOTS: [&str; 12] = [
        "lt1", "dk1", "lt2", "dk2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ];
    for (i, slot) in SLOTS.iter().enumerate() {
        let open = format!("<a:{slot}");
        if let Some(start) = xml.find(&open) {
            let chunk = &xml[start..];
            if let Some(end) = chunk.find(&format!("</a:{slot}")) {
                if let Some(hex) = extract_theme_color_hex(&chunk[..end]) {
                    out[i] = hex;
                }
            }
        }
    }
    out
}

