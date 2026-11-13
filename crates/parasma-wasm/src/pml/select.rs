//! PML (PowerPoint) text + image selection + copy.
//!
//! PowerPoint's selection model CLAMPS to ONE target: a click/drag inside
//! a single shape (or table cell) selects that target's text; a drag that
//! leaves the target does NOT extend into another — it clamps to the target's
//! nearest edge. A cross-target drag / a click on a picture selects the OBJECT
//! instead (the plugin probes objects first). Multi-shape text is a deferred
//! follow-on.
//!
//! All geometry flows through the SHARED `super::walk_paragraph_stack` (the same
//! line walk the paint consumes), so hit-test / rects / extract can never drift
//! from what is painted. These are all NEW READS + an ADDITIVE paint pass that
//! fires ONLY when a selection is set, so a no-selection slide render stays
//! byte-identical (the PML render pins hold).
//!
//! The text position is `PmlPos { slide, target, para, byte }` — `byte` indexes
//! the target-paragraph's PAINTED logical text (the run-concatenation the walk
//! and the search path agree on). Extraction reuses the shared flavor-neutral
//! `DocFragment` (`engine::fragment` — a PML shape/cell is paragraphs+runs; NO
//! PML-specific text serializer), so the TS text/plain +
//! text/html flavors are shared.

use crate::engine::paint::Painter;

use super::shape::{ParaAlign, SlidePage};
use super::{
    cell_text_box, measure_run_advance, run_width_to_byte, shape_text_box, walk_paragraph_stack,
    WalkPara,
};
use crate::engine::fonts::{FaceCache, FontCache};
use crate::engine::fragment::{Block, DocFragment, FragPara, FragRun, SelRect};

/// The one target a PML text selection is clamped to (one shape or cell). A `Shape`
/// indexes `SlidePage.shapes`; a `TableCell` indexes `SlidePage.tables[table]`
/// then `.cells[cell]` (a flat cell index — merges are deferred in PML render,
/// so cell indices are stable within a loaded deck).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PmlTarget {
    Shape(usize),
    TableCell { table: usize, cell: usize },
}

impl PmlTarget {
    /// Wire encoding `(kind, a, b)` for the scalar wasm exports: kind 0 = shape
    /// (`a` = shape idx), 1 = tableCell (`a` = table idx, `b` = cell idx).
    pub(crate) fn to_wire(self) -> (i32, u32, u32) {
        match self {
            PmlTarget::Shape(s) => (0, s as u32, 0),
            PmlTarget::TableCell { table, cell } => (1, table as u32, cell as u32),
        }
    }

    /// Decode a `(kind, a, b)` wire triple. Returns `None` for an unknown kind.
    pub(crate) fn from_wire(kind: i32, a: u32, b: u32) -> Option<PmlTarget> {
        match kind {
            0 => Some(PmlTarget::Shape(a as usize)),
            1 => Some(PmlTarget::TableCell { table: a as usize, cell: b as usize }),
            _ => None,
        }
    }
}

/// A logical PML text position — the currency the unified `docHitTest` PML arm
/// produces and `docSelectionRects` / `docSetSelection` / `docExtractRange`
/// consume (via the wire `PmlSelection`). `byte`
/// indexes the target paragraph's PAINTED logical text, snapped to a char
/// boundary at consumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PmlPos {
    pub(crate) slide: usize,
    pub(crate) target: PmlTarget,
    pub(crate) para: usize,
    pub(crate) byte: usize,
}

/// A stored PML text selection (`Copy`, lives in a `Cell` on the Viewer beside
/// the WML/SML selections). Clamped to ONE `target` on ONE `slide`. The
/// range is normalized `start <= end` at set time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PmlSelection {
    pub(crate) slide: usize,
    pub(crate) target: PmlTarget,
    pub(crate) start_para: usize,
    pub(crate) start_byte: usize,
    pub(crate) end_para: usize,
    pub(crate) end_byte: usize,
}

impl PmlSelection {
    /// True when the selection is a collapsed caret (paints nothing).
    fn is_collapsed(&self) -> bool {
        self.start_para == self.end_para && self.start_byte == self.end_byte
    }
}

// ─── Text-box geometry for a target ─────────────────────────────────────────

/// Walk the paragraph geometry for one target on a page (the SAME walk the paint
/// runs), plus the paragraphs slice. `None` when the target index is out of
/// range or carries no text.
fn target_walk<'a>(
    page: &'a SlidePage,
    target: PmlTarget,
    font_cache: &FontCache,
) -> Option<(Vec<WalkPara>, &'a [super::shape::SlideParagraph], ParaAlign)> {
    match target {
        PmlTarget::Shape(si) => {
            let sh = page.shapes.get(si)?;
            if sh.paragraphs.is_empty() {
                return None;
            }
            let (tx, ty, tw) = shape_text_box(sh, font_cache);
            let walks = walk_paragraph_stack(&sh.paragraphs, tx, ty, tw, sh.wrap, font_cache);
            Some((walks, &sh.paragraphs, sh.paragraphs[0].align))
        }
        PmlTarget::TableCell { table, cell } => {
            let cell = page.tables.get(table)?.cells.get(cell)?;
            if cell.paragraphs.is_empty() {
                return None;
            }
            let (tx, ty, tw) = cell_text_box(cell, font_cache);
            let walks = walk_paragraph_stack(
                &cell.paragraphs,
                tx,
                ty,
                tw,
                super::shape::WrapMode::Square,
                font_cache,
            );
            Some((walks, &cell.paragraphs, cell.paragraphs[0].align))
        }
    }
}

/// The (box_x, box_y, box_w, box_h) of a target's OUTER box — the pointer hit
/// region (a click anywhere in the shape/cell box selects its text). `None` for
/// an out-of-range index.
fn target_box(page: &SlidePage, target: PmlTarget) -> Option<(f64, f64, f64, f64)> {
    match target {
        PmlTarget::Shape(si) => page.shapes.get(si).map(|s| (s.x, s.y, s.w, s.h)),
        PmlTarget::TableCell { table, cell } => {
            page.tables.get(table)?.cells.get(cell).map(|c| (c.x, c.y, c.w, c.h))
        }
    }
}

/// True when `(x, y)` lies inside a box.
fn in_box(bx: (f64, f64, f64, f64), x: f64, y: f64) -> bool {
    x >= bx.0 && x < bx.0 + bx.2 && y >= bx.1 && y < bx.1 + bx.3
}

// ─── Byte ↔ x within a run ──────────────────────────────────────────────────

/// Map a local x (from a sub-run's left edge) to a byte offset within the run's
/// text, snapped to the nearest char boundary (snapping happens at consumption).
/// The runs aren't pre-shaped, so this measures the prefix width to each char
/// boundary and picks the nearest — O(n²) in run length, but slide runs are
/// short.
fn run_x_to_byte(run: &super::shape::SlideRun, x_local: f64, font_cache: &FontCache) -> usize {
    if x_local <= 0.0 || run.text.is_empty() {
        return 0;
    }
    // One built face for this run's O(n) prefix sweep.
    let faces = FaceCache::new();
    let full = measure_run_advance(run, font_cache, &faces);
    if x_local >= full {
        return run.text.len();
    }
    let mut best_b = 0usize;
    let mut best_d = x_local.abs(); // distance to byte 0 (x = 0)
    for (b, ch) in run.text.char_indices() {
        let end = b + ch.len_utf8();
        let w = run_width_to_byte(run, end, font_cache, &faces);
        let d = (x_local - w).abs();
        if d < best_d {
            best_d = d;
            best_b = end;
        }
    }
    best_b
}

// ─── Hit-test (text) ────────────────────────────────────────────────────────

/// Resolve `(para, byte)` for `(x, y)` within one target's walked geometry.
/// Finds the sub-run whose band contains `y` and whose x-span contains `x`;
/// otherwise clamps to the geometrically nearest sub-run edge (a drag
/// past the target snaps to its nearest text, never escapes the target). An
/// empty target resolves to `(0, 0)`.
fn resolve_in_target(walks: &[WalkPara], x: f64, y: f64, font_cache: &FontCache) -> (usize, usize) {
    let mut best: Option<(f64, usize, usize)> = None; // (distance², para, byte)
    let mut consider = |dist: f64, para: usize, byte: usize| {
        if best.is_none_or(|(d, _, _)| dist < d) {
            best = Some((dist, para, byte));
        }
    };
    for (pi, wp) in walks.iter().enumerate() {
        for wr in &wp.runs {
            let in_band = y >= wr.top && y < wr.top + wr.height;
            let in_x = x >= wr.x0 && x < wr.x1;
            if in_band && in_x {
                let byte = wr.byte_start + run_x_to_byte(&wr.run, x - wr.x0, font_cache);
                return (pi, byte);
            }
            // Nearest-edge candidates (both ends of the box), with a squared
            // distance in x and y so a point below the last line lands at the
            // last line's nearest end, not a far earlier line.
            let dy = if y < wr.top {
                wr.top - y
            } else if y >= wr.top + wr.height {
                y - (wr.top + wr.height)
            } else {
                0.0
            };
            let dl = ((x - wr.x0).powi(2) + dy.powi(2)).sqrt();
            let dr = ((x - wr.x1).powi(2) + dy.powi(2)).sqrt();
            consider(dl, pi, wr.byte_start);
            consider(dr, pi, wr.byte_end);
        }
    }
    match best {
        Some((_, para, byte)) => (para, byte),
        None => (0, 0),
    }
}

/// PML text hit-test. When `clamp` is `Some(target)` the point is
/// resolved WITHIN that target regardless of where it lands (the drag-clamp — a
/// drag started in a shape stays in it). When `clamp` is `None` the TOPMOST text
/// target under the point wins (tables paint above shapes, so tables are probed
/// first; each in reverse z-order). Returns `None` when no text target is hit
/// (a bare click on empty slide background — the plugin then clears).
pub(crate) fn hit_test(
    page: &SlidePage,
    slide: usize,
    x: f64,
    y: f64,
    clamp: Option<PmlTarget>,
    font_cache: &FontCache,
) -> Option<PmlPos> {
    let target = match clamp {
        Some(t) => t,
        None => topmost_text_target(page, x, y)?,
    };
    let (walks, _, _) = target_walk(page, target, font_cache)?;
    let (para, byte) = resolve_in_target(&walks, x, y, font_cache);
    Some(PmlPos { slide, target, para, byte })
}

/// The topmost text target under `(x, y)` in paint z-order (tables above shapes;
/// each format reverse-scanned so a later-drawn overlapping item wins). A target
/// qualifies only when its box contains the point AND it carries text.
fn topmost_text_target(page: &SlidePage, x: f64, y: f64) -> Option<PmlTarget> {
    for (ti, tbl) in page.tables.iter().enumerate().rev() {
        for (ci, cell) in tbl.cells.iter().enumerate() {
            if !cell.paragraphs.is_empty()
                && in_box((cell.x, cell.y, cell.w, cell.h), x, y)
            {
                return Some(PmlTarget::TableCell { table: ti, cell: ci });
            }
        }
    }
    for (si, sh) in page.shapes.iter().enumerate().rev() {
        if !sh.paragraphs.is_empty() && in_box((sh.x, sh.y, sh.w, sh.h), x, y) {
            return Some(PmlTarget::Shape(si));
        }
    }
    None
}

// ─── Selection rects ────────────────────────────────────────────────────────

/// Per-line-merged highlight rects for a stored selection over its target
/// (page-absolute px). The covered byte window per paragraph is intersected with
/// each sub-run's `[byte_start, byte_end)`; partial coverage slices via
/// `run_width_to_byte`; rects sharing a visual line (same `top`) are merged so
/// translucent fills never double-darken at seams. A collapsed selection yields
/// no rects (byte-identical no-op).
pub(crate) fn selection_rects(
    page: &SlidePage,
    sel: &PmlSelection,
    font_cache: &FontCache,
) -> Vec<SelRect> {
    if sel.is_collapsed() {
        return Vec::new();
    }
    let Some((walks, _, _)) = target_walk(page, sel.target, font_cache) else {
        return Vec::new();
    };
    // One shaping scope for this selection's partial-coverage slices.
    let faces = FaceCache::new();
    let mut rects: Vec<SelRect> = Vec::new();
    for (pi, wp) in walks.iter().enumerate() {
        if pi < sel.start_para || pi > sel.end_para {
            continue;
        }
        let lo = if pi == sel.start_para { sel.start_byte } else { 0 };
        let hi = if pi == sel.end_para { sel.end_byte } else { usize::MAX };
        // (top, height, x0, x1) intervals, grouped and merged per visual line.
        let mut intervals: Vec<(f64, f64, f64, f64)> = Vec::new();
        for wr in &wp.runs {
            let cov_lo = wr.byte_start.max(lo);
            let cov_hi = wr.byte_end.min(hi);
            if cov_lo >= cov_hi {
                continue;
            }
            let (x0, x1) = if cov_lo == wr.byte_start && cov_hi == wr.byte_end {
                (wr.x0, wr.x1) // fully covered → whole box
            } else {
                let a = wr.x0 + run_width_to_byte(&wr.run, cov_lo - wr.byte_start, font_cache, &faces);
                let b = wr.x0 + run_width_to_byte(&wr.run, cov_hi - wr.byte_start, font_cache, &faces);
                (a.min(b), a.max(b))
            };
            intervals.push((wr.top, wr.height, x0, x1));
        }
        merge_line_intervals(&mut intervals, &mut rects);
    }
    rects
}

/// Merge covered x-intervals that share a visual line (same `top`), pushing the
/// merged rects into `out`. Intervals are grouped by `top`, sorted by left, then
/// coalesced when they touch/overlap (the WML `linkRects` idiom).
fn merge_line_intervals(intervals: &mut [(f64, f64, f64, f64)], out: &mut Vec<SelRect>) {
    // Sort by (top, x0) so same-line intervals are contiguous and left-ordered.
    intervals.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut i = 0;
    while i < intervals.len() {
        let (top, h, mut cx0, mut cx1) = intervals[i];
        let mut j = i + 1;
        while j < intervals.len() && (intervals[j].0 - top).abs() < 0.5 {
            let (_, _, x0, x1) = intervals[j];
            if x0 <= cx1 + 0.5 {
                cx1 = cx1.max(x1);
            } else {
                out.push(SelRect { x: cx0, y: top, w: cx1 - cx0, h });
                cx0 = x0;
                cx1 = x1;
            }
            j += 1;
        }
        out.push(SelRect { x: cx0, y: top, w: cx1 - cx0, h });
        i = j;
    }
}

// ─── Extraction (reuses the shared engine::fragment::DocFragment) ───────────

/// Map a PML paragraph alignment to the `DocFragment` alignment token the shared
/// TS flavors consume (`left`/`center`/`right`/`both`).
fn align_str(a: ParaAlign) -> String {
    match a {
        ParaAlign::Left => "left",
        ParaAlign::Center => "center",
        ParaAlign::Right => "right",
        ParaAlign::Justify => "both",
    }
    .to_string()
}

/// Build a `FragRun` from a PML sub-run + its text slice. PML runs carry no
/// underline/strike/highlight/link/vertAlign in v1, so those default off — the
/// shared serializers already handle the neutral values.
fn frag_run(run: &super::shape::SlideRun, text: String) -> FragRun {
    FragRun {
        text,
        bold: run.bold,
        italic: run.italic,
        underline: "none",
        strike: false,
        double_strike: false,
        font_family: run.family.clone(),
        font_size_pt: run.font_pt,
        color: run.color.clone(),
        highlight: None,
        run_shading: None,
        vert_align: "baseline",
        link: None,
        image: None,
        footnote_mark: None,
    }
}

/// Extract the LOGICAL-order `DocFragment` for a stored selection over its
/// target (one shape/cell). Each covered paragraph becomes a
/// `Block::Para`; runs are sliced from the walked sub-runs (which are already in
/// logical/line order — PML has no bidi reorder in this path). An empty covered
/// paragraph emits a runs-empty `FragPara`. Reuses the shared `DocFragment`
/// (NO PML-specific text serializer), so text/plain + text/html are shared.
pub(crate) fn extract_range(
    page: &SlidePage,
    sel: &PmlSelection,
    font_cache: &FontCache,
) -> DocFragment {
    let Some((walks, paras, _)) = target_walk(page, sel.target, font_cache) else {
        return DocFragment { blocks: Vec::new() };
    };
    let mut blocks: Vec<Block> = Vec::new();
    for (pi, wp) in walks.iter().enumerate() {
        if pi < sel.start_para || pi > sel.end_para {
            continue;
        }
        let lo = if pi == sel.start_para { sel.start_byte } else { 0 };
        let hi = if pi == sel.end_para { sel.end_byte } else { usize::MAX };
        let mut runs: Vec<FragRun> = Vec::new();
        for wr in &wp.runs {
            let cov_lo = wr.byte_start.max(lo);
            let cov_hi = wr.byte_end.min(hi);
            if cov_lo >= cov_hi {
                continue;
            }
            // Slice the painted logical text (char-boundary safe: the walk's byte
            // ranges are pushed whole run texts, and lo/hi originate from
            // char-boundary hit-tests).
            let slice = safe_slice(&wp.logical, cov_lo, cov_hi);
            if !slice.is_empty() {
                runs.push(frag_run(&wr.run, slice));
            }
        }
        let align = paras.get(pi).map(|p| p.align).unwrap_or(ParaAlign::Left);
        blocks.push(Block::Para(FragPara {
            runs,
            alignment: align_str(align),
            dir: "ltr",
            list: None,
        }));
    }
    DocFragment { blocks }
}

/// Slice `s[lo..hi]` snapped inward to char boundaries (defensive — the callers
/// pass char-boundary offsets, but a stored position under fallback metrics can
/// arrive mid-cluster after a font-arrival reshape).
fn safe_slice(s: &str, lo: usize, hi: usize) -> String {
    let mut a = lo.min(s.len());
    while a > 0 && !s.is_char_boundary(a) {
        a -= 1;
    }
    let mut b = hi.min(s.len());
    while b < s.len() && !s.is_char_boundary(b) {
        b += 1;
    }
    if a >= b {
        return String::new();
    }
    s[a..b].to_string()
}

// ─── Object (picture) hit-test + outline ────────────────────────────────────

/// A picture object hit: the slide-namespaced image key + its page-absolute box.
pub(crate) struct PmlObjectHit {
    pub(crate) key: String,
    pub(crate) rect: SelRect,
}

/// Topmost picture under `(x, y)` (pictures paint behind shapes; reverse
/// insertion so a later-drawn overlapping picture wins). A NEW READ. `None` when
/// no picture is hit.
pub(crate) fn hit_test_object(page: &SlidePage, x: f64, y: f64) -> Option<PmlObjectHit> {
    for pic in page.pictures.iter().rev() {
        if in_box((pic.x, pic.y, pic.w, pic.h), x, y) {
            return Some(PmlObjectHit {
                key: pic.image_key.clone(),
                rect: SelRect { x: pic.x, y: pic.y, w: pic.w, h: pic.h },
            });
        }
    }
    None
}

/// Every painted box for `key` on this slide (a picture reused under several
/// placements outlines each).
fn object_outline_rects(page: &SlidePage, key: &str) -> Vec<SelRect> {
    page.pictures
        .iter()
        .filter(|p| p.image_key == key)
        .map(|p| SelRect { x: p.x, y: p.y, w: p.w, h: p.h })
        .collect()
}

// ─── Additive paint passes (fire only when a selection is set) ───────────────

const SELECTION_FILL_ALPHA: f64 = 0.30;
const SELECTION_FILL_COLOR: &str = "#3B82F6";
const OBJECT_OUTLINE_COLOR: &str = "#3B82F6";
const OBJECT_OUTLINE_WIDTH_PX: f64 = 2.0;

/// The PML text selection highlight — a translucent blue fill over the covered
/// glyphs, mirroring the WML/search additive pass. Fires ONLY when the selection
/// belongs to THIS slide AND covers a range (collapsed ⇒ zero rects ⇒
/// byte-identical no-op). Painted through the raw `ctx` (PML has no display-list
/// export), consistent with the PML search highlight.
pub(crate) fn paint_selection_pass(
    p: &dyn Painter,
    page: &SlidePage,
    slide: usize,
    sel: Option<&PmlSelection>,
    font_cache: &FontCache,
) {
    let Some(sel) = sel else { return };
    if sel.slide != slide {
        return;
    }
    let rects = selection_rects(page, sel, font_cache);
    if rects.is_empty() {
        return;
    }
    p.save();
    p.set_global_alpha(SELECTION_FILL_ALPHA);
    p.set_fill_style_str(SELECTION_FILL_COLOR);
    for r in &rects {
        p.fill_rect(r.x, r.y, r.w, r.h);
    }
    p.restore();
}

/// The selected-picture OUTLINE (read-only focus ring, no handles). Fires only
/// when a picture is selected on THIS slide; an unknown/off-slide key strokes
/// nothing (byte-identical no-op).
pub(crate) fn paint_object_outline_pass(
    p: &dyn Painter,
    page: &SlidePage,
    slide: usize,
    selected: Option<&(usize, String)>,
) {
    let Some((sel_slide, key)) = selected else { return };
    if *sel_slide != slide {
        return;
    }
    let rects = object_outline_rects(page, key);
    if rects.is_empty() {
        return;
    }
    p.save();
    p.set_stroke_style_str(OBJECT_OUTLINE_COLOR);
    p.set_line_width(OBJECT_OUTLINE_WIDTH_PX);
    for r in &rects {
        p.stroke_rect(r.x, r.y, r.w, r.h);
    }
    p.restore();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::fonts::FontCache;
    use crate::pml::PmlRenderer;

    // A one-slide deck with a title ("Hello Title") + a two-bullet body.
    fn minimal_page() -> (SlidePage, FontCache) {
        let renderer = PmlRenderer::from_pptx(crate::pml::tests::minimal_pptx())
            .expect("minimal pptx must load");
        let fc = FontCache::new();
        let pages = renderer.build_pages_for_test(&fc);
        (pages.into_iter().next().expect("one page"), fc)
    }

    fn shape_with_text<'a>(page: &'a SlidePage, needle: &str) -> (usize, &'a super::super::shape::SlideShape) {
        page.shapes
            .iter()
            .enumerate()
            .find(|(_, s)| s.paragraphs.iter().flat_map(|p| &p.runs).any(|r| r.text.contains(needle)))
            .expect("shape with needle")
    }

    #[test]
    fn hit_inside_a_shape_resolves_that_shape() {
        let (page, fc) = minimal_page();
        let (si, sh) = shape_with_text(&page, "Hello Title");
        // A point comfortably inside the title box.
        let hit = hit_test(&page, 0, sh.x + 20.0, sh.y + sh.h / 2.0, None, &fc)
            .expect("a text hit inside the title");
        assert_eq!(hit.target, PmlTarget::Shape(si));
        assert_eq!(hit.slide, 0);
    }

    #[test]
    fn hit_on_empty_background_is_none() {
        let (page, fc) = minimal_page();
        // Far outside every shape box (slides are ~960×540 px; go well past).
        assert!(hit_test(&page, 0, page.width + 500.0, page.height + 500.0, None, &fc).is_none());
    }

    #[test]
    fn drag_clamps_to_the_anchor_target() {
        let (page, fc) = minimal_page();
        let (si, sh) = shape_with_text(&page, "Hello Title");
        let clamp = Some(PmlTarget::Shape(si));
        // A point FAR below/right of the title still resolves within the title
        // (clamped), never escaping into the body shape below it.
        let hit = hit_test(&page, 0, sh.x + 5000.0, sh.y + 5000.0, clamp, &fc)
            .expect("clamped hit");
        assert_eq!(hit.target, PmlTarget::Shape(si));
        // Clamped to the end of the title text.
        assert!(hit.byte > 0, "a far drag lands past the first byte");
    }

    #[test]
    fn full_paragraph_rects_and_extract_round_trip() {
        let (page, fc) = minimal_page();
        let (si, sh) = shape_with_text(&page, "Hello Title");
        // Select the whole first paragraph of the title: byte 0 .. logical end.
        let (walks, _, _) = target_walk(&page, PmlTarget::Shape(si), &fc).unwrap();
        let end_byte = walks[0].logical.len();
        assert!(end_byte > 0);
        let sel = PmlSelection {
            slide: 0,
            target: PmlTarget::Shape(si),
            start_para: 0,
            start_byte: 0,
            end_para: 0,
            end_byte,
        };
        let rects = selection_rects(&page, &sel, &fc);
        assert!(!rects.is_empty(), "a covered paragraph paints ≥1 rect");
        // The rect band sits inside the shape box.
        assert!(rects[0].y >= sh.y - 1.0);
        // Extraction yields the painted logical text of the paragraph.
        let frag = extract_range(&page, &sel, &fc);
        let text: String = frag
            .blocks
            .iter()
            .flat_map(|b| match b {
                Block::Para(p) => p.runs.iter().map(|r| r.text.clone()).collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        assert!(text.contains("Hello Title"), "extract yields the title text, got {text:?}");
    }

    #[test]
    fn collapsed_selection_paints_nothing() {
        let (page, fc) = minimal_page();
        let (si, _) = shape_with_text(&page, "Hello Title");
        let sel = PmlSelection {
            slide: 0,
            target: PmlTarget::Shape(si),
            start_para: 0,
            start_byte: 3,
            end_para: 0,
            end_byte: 3,
        };
        assert!(selection_rects(&page, &sel, &fc).is_empty());
    }

    #[test]
    fn partial_range_extracts_only_covered_bytes() {
        let (page, fc) = minimal_page();
        let (si, _) = shape_with_text(&page, "Hello Title");
        // "Hello Title" — bytes 0..5 = "Hello".
        let sel = PmlSelection {
            slide: 0,
            target: PmlTarget::Shape(si),
            start_para: 0,
            start_byte: 0,
            end_para: 0,
            end_byte: 5,
        };
        let frag = extract_range(&page, &sel, &fc);
        let text: String = frag
            .blocks
            .iter()
            .flat_map(|b| match b {
                Block::Para(p) => p.runs.iter().map(|r| r.text.clone()).collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(text, "Hello");
    }

    #[test]
    fn wire_round_trips_targets() {
        for t in [PmlTarget::Shape(3), PmlTarget::TableCell { table: 1, cell: 4 }] {
            let (k, a, b) = t.to_wire();
            assert_eq!(PmlTarget::from_wire(k, a, b), Some(t));
        }
        assert_eq!(PmlTarget::from_wire(9, 0, 0), None);
    }
}
