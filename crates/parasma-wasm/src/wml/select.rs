//! Selection & copy substrate — the logical position encoding, paragraph
//! extraction metadata, and the intra-item x↔byte math that maps a painted
//! geometry back to a logical `TextPos { para, byte }` and forward again.
//! The flavor-neutral output schema (`DocFragment` + satellites) lives in
//! `engine::fragment`; this module keeps the WML-side metadata
//! (`ParaMeta`/`RunMeta`/`TableMeta`) and the builders that map it there.
//!
//! Everything here is ADDITIVE and layout-neutral (the oracle `geometry_dump`
//! reads only geometry + `items.len()`; these types are captured alongside, and
//! the hit/rects readers only observe the page model). The hard correctness
//! item is the RTL mirror: `byte_to_x_local` gates on
//! `ShapedRun.rtl` (actual glyph storage order), NEVER on `Item.rtl`.

use std::collections::HashMap;

use ooxml_wml::types::{STJc, STNumberFormat};

use crate::cull;
use crate::engine::fragment::{
    Block, DocFragment, FragBorder, FragCell, FragList, FragPara, FragRow, FragRun, FragTable,
    ImageRef, SelRect,
};
use crate::engine::paint::{Painter, RunStyle, Underline, VertAlign};
use crate::wml::linebreak::{Item, LineSegment, CHAR_START_SENTINEL};
use crate::wml::numbering::NumberingMarker;
use crate::wml::page::{AnchoredImageLayout, PageElement, PageLayout, PagedLine, PARA_SENTINEL};

/// The shared position currency. Logical, layout-independent,
/// stable across zoom AND re-layout. `para` = the document-order paragraph id
/// (the cache-build counter); `byte` = byte offset into that paragraph's
/// LOGICAL text (the `collect_items` concatenation), snapped to a shaped-cluster
/// boundary at consumption. Total order = lexicographic `(para, byte)` —
/// the derived `Ord` gives exactly that because the fields are declared in that
/// order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct TextPos {
    pub(crate) para: u32,
    pub(crate) byte: u32,
}

impl TextPos {
    pub(crate) fn new(para: u32, byte: u32) -> Self {
        TextPos { para, byte }
    }
}

/// Partial source-pointer support — the paragraph-granular node-path prefix,
/// an additive upgrade path toward full source pointers. Body paragraphs and
/// table-cell paragraphs are the two selectable WML streams.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum ParaLocation {
    Body { sec: usize, block: usize },
    TableCell { sec: usize, block: usize, row: usize, cell: usize },
}

/// Resolved numbering captured at `resolve_marker_for` time. The
/// live `count` survives mid-list `lvlRestart` (reverse-parsing "vii." is not
/// viable); `fmt` drives the HTML `<ol type>`; `marker_text` is the exact
/// rendered marker (the text/plain prefix source), no counter replay.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct NumInfo {
    pub(crate) num_id: i64,
    pub(crate) ilvl: i64,
    pub(crate) count: i64,
    pub(crate) fmt: STNumberFormat,
    pub(crate) marker_text: String,
}

impl NumInfo {
    /// Build from a resolved marker (drops the rendered `\u{00A0}` suffix that
    /// `layout_paragraph` bakes on for spacing — `marker.text` here is the raw
    /// `resolve_marker_for` text, so no stripping is needed).
    pub(crate) fn from_marker(m: &NumberingMarker) -> Self {
        NumInfo {
            num_id: m.num_id,
            ilvl: m.ilvl,
            count: m.count,
            fmt: m.fmt,
            marker_text: m.text.clone(),
        }
    }
}

/// The side-table row captured for each body/cell paragraph
/// during the cache-build walk. `logical_text` is the extraction source of truth
/// — the byte concatenation, in `collect_items` emit order, of every
/// collected `Item.text` (post-`to_uppercase`), captured AFTER
/// `collect_items` and BEFORE the numbering-marker insert. `alignment` is the
/// already-`rtl_swap_jc`-resolved `layout.alignment` (consumers account for
/// the swap).
#[derive(Clone, Debug)]
pub(crate) struct ParaMeta {
    #[allow(dead_code)] // stamped for capture keying; not read back off ParaMeta
    pub(crate) para_id: u32,
    pub(crate) logical_text: String,
    pub(crate) location: ParaLocation,
    pub(crate) alignment: STJc,
    pub(crate) base_rtl: bool,
    pub(crate) numbering: Option<NumInfo>,
    /// The paragraph's runs in LOGICAL order, each
    /// carrying its COMPUTED direct style + byte offsets into `logical_text`.
    /// Built from the pre-pack, logical-order `items` at the SAME fold site that
    /// captures `logical_text` (`paragraph.rs`), so `char_start`/`char_len` share
    /// the exact byte basis. `extractRange` slices run TEXT from `logical_text`
    /// (never `item.text` — the PAGE/NUMPAGES pass mutates that in place);
    /// `RunMeta` supplies STYLE + offsets only.
    pub(crate) runs: Vec<RunMeta>,
    /// Anchored-image copy — the ANCHORED (floating)
    /// images whose `<wp:anchor>` lives in this paragraph, in document order,
    /// as `{rel_id, w_px, h_px}` DISPLAY px (the same FragImage wPx/hPx
    /// semantics as inline images). Captured beside `runs` at the SAME
    /// fold site (`paragraph.rs`), chart-gated (see `anchored_image_refs`).
    /// `extract_range_impl` appends one image run per entry when this
    /// paragraph is inside the copied range, so a text copy sweeping over a
    /// float's anchor brings the image along (Word parity — never silently dropped).
    pub(crate) anchored_images: Vec<ImageRef>,
}

/// One logical-order run captured at layout time. Text
/// runs carry `char_start == item.char_start` and `char_len == item.char_len`
/// (> 0). Zero-width object runs (`char_len == 0`) — inline images and
/// footnote/endnote refs — sit at a running byte cursor and never advance it, so
/// `extractRange` includes them when the cursor falls inside the covered window.
#[derive(Clone)]
pub(crate) struct RunMeta {
    pub(crate) char_start: u32,
    pub(crate) char_len: u32,
    pub(crate) style: RunStyle,
    pub(crate) link: Option<String>,
    pub(crate) image: Option<ImageRef>,
    pub(crate) footnote_mark: Option<String>,
}

// `RunStyle` isn't `Debug`; skip it so `ParaMeta` can keep its derived `Debug`.
impl std::fmt::Debug for RunMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunMeta")
            .field("char_start", &self.char_start)
            .field("char_len", &self.char_len)
            .field("link", &self.link)
            .field("image", &self.image)
            .field("footnote_mark", &self.footnote_mark)
            .finish()
    }
}

/// Anchored-image copy — map a paragraph's collected
/// `AnchoredImageInfo`s to the `ImageRef`s recorded on its `ParaMeta`.
/// Dimensions are the anchor's resolved DISPLAY px (`emu_to_px(extent)`).
///
/// CHART GATE (pinned): anchored rel ids come exclusively from `<a:blip>`
/// embeds inside `<wp:anchor>` (`anchored_image_rel_ids`), while chart rels
/// enter the pipeline ONLY under the namespaced `chart::<relId>` key
/// (`all_chart_rel_ids` → inline items). The prefix check is the
/// invariant-matching defense — chart rels must NEVER be emitted from a text
/// copy (by design, charts copy as intrinsic PNG via object copy, not here).
/// The `image_rel_ids` set gate was considered and rejected: it is Viewer
/// state, unreachable at this capture site without threading it through the
/// whole paragraph-layout API.
pub(crate) fn anchored_image_refs(
    infos: &[crate::wml::linebreak::AnchoredImageInfo],
) -> Vec<ImageRef> {
    infos
        .iter()
        .filter(|a| !a.rel_id.starts_with("chart::"))
        .map(|a| ImageRef {
            rel_id: a.rel_id.clone(),
            w_px: a.width_px,
            h_px: a.height_px,
        })
        .collect()
}

impl RunMeta {
    /// Build a TEXT run from a non-sentinel collected item (the common case).
    /// STYLE is cloned; TEXT is sliced from `logical_text` at extract time.
    pub(crate) fn text_run(it: &Item) -> RunMeta {
        RunMeta {
            char_start: it.char_start,
            char_len: it.char_len,
            style: it.style.clone(),
            link: it.link.clone(),
            image: None,
            footnote_mark: None,
        }
    }
}

/// The explicit-parameter capture context threaded through the
/// cache-build walk ONLY (`run_layout_impl` block loop + `layout_table`
/// recursion). Every OTHER `layout_paragraph` call site (hdr/ftr, footnote band,
/// textbox) passes `None`, because those re-run nondeterministically at paint
/// time (and may run more than once) — a thread-local counter would drift. `next_id` and
/// `sink` are borrowed from locals declared beside `list_counters`; `location`
/// is set by the caller before each `layout_paragraph`/cell descent.
pub(crate) struct ParaCapture<'a> {
    pub(crate) next_id: &'a mut u32,
    pub(crate) sink: &'a mut Vec<ParaMeta>,
    pub(crate) location: ParaLocation,
    /// Nested-table fix — set while descending a NESTED table. A nested
    /// table's cells overwrite `location.row`/`cell` with their OWN inner
    /// coordinates (`layout_table` recursion), so routing them by those
    /// coordinates lands in the wrong OUTER cell or drops them entirely (silent
    /// copy data loss). When this is set, nested-cell paragraphs are captured as
    /// BODY fallback so their text still copies (documented v1 limitation:
    /// nested-table cell paras copy as body text, not in-grid).
    pub(crate) nested: bool,
}

impl<'a> ParaCapture<'a> {
    /// Register one paragraph and return its assigned id. Debug-asserts
    /// exactly-once, document-order registration per layout generation:
    /// the sink length BEFORE the push must equal the id being assigned, so a
    /// double-register or an out-of-walk call (which would desync the two)
    /// trips in tests and proves the invariant guards something real.
    pub(crate) fn register(&mut self, meta_builder: impl FnOnce(u32) -> ParaMeta) -> u32 {
        let para_id = *self.next_id;
        *self.next_id += 1;
        debug_assert!(
            self.sink.len() as u32 == para_id,
            "para capture: para id {} registered out of order (sink has {})",
            para_id,
            self.sink.len()
        );
        self.sink.push(meta_builder(para_id));
        para_id
    }
}

/// The paragraph LOGICAL text: the byte concatenation, in
/// `collect_items` emit order (= logical order), of every collected `Item.text`.
/// Captured AFTER `collect_items` and BEFORE the numbering-marker insert, so it
/// shares the exact basis of every item's `char_start`/`char_len`.
///
/// Sentinel items (footnote/endnote refs, images, page-breaks…) are EXCLUDED:
/// `collect_items` stamps them `CHAR_START_SENTINEL` and does NOT advance
/// `byte_cursor`, yet some carry non-empty `text` (e.g. a footnote ref's "1").
/// Concatenating their text would shift the basis so `logical_text[cs..cs+len]`
/// no longer equals a following item's `text` — corrupting every hit/rect on a
/// paragraph with a mid-text footnote. Filtering restores the invariant
/// `logical_text[char_start .. char_start+char_len] == item.text` for every
/// non-sentinel item.
pub(crate) fn logical_text_of(items: &[Item]) -> String {
    items
        .iter()
        .filter(|i| i.char_start != CHAR_START_SENTINEL)
        .map(|i| i.text.as_str())
        .collect()
}

/// The paragraph's logical-order runs WITH
/// style, built from the SAME pre-pack `items` that seed `logical_text` (shared
/// byte basis). Non-sentinel items become TEXT runs (`char_len > 0`, text sliced
/// from `logical_text` at extract time). Footnote/endnote and inline-image
/// sentinels become ZERO-WIDTH runs at the running byte cursor (the length of
/// `logical_text` emitted so far) and do NOT advance it — matching how
/// `logical_text_of` filters them. Other sentinels (page breaks, bookmarks)
/// contribute no run. Read-only; never mutates `items` (byte-identity).
pub(crate) fn build_para_runs(items: &[Item]) -> Vec<RunMeta> {
    let mut runs = Vec::new();
    let mut cursor: u32 = 0; // running byte offset into logical_text
    for it in items {
        if it.char_start != CHAR_START_SENTINEL {
            cursor = it.char_start + it.char_len;
            runs.push(RunMeta::text_run(it));
        } else if it.footnote_id.is_some() || it.endnote_id.is_some() {
            // The collect-time DISPLAY number lives in `item.text` ("1"); capture
            // it as the footnote mark (text is already transformed).
            runs.push(RunMeta {
                char_start: cursor,
                char_len: 0,
                style: it.style.clone(),
                link: it.link.clone(),
                image: None,
                footnote_mark: Some(it.text.clone()),
            });
        } else if let Some(img) = &it.image {
            runs.push(RunMeta {
                char_start: cursor,
                char_len: 0,
                style: it.style.clone(),
                link: it.link.clone(),
                image: Some(ImageRef {
                    rel_id: img.rel_id.clone(),
                    w_px: it.width,
                    h_px: img.height_px,
                }),
                footnote_mark: None,
            });
        }
    }
    runs
}

// ─── Intra-item x↔byte ──────────────────────────────────────────────────────

/// Forward map: local x offset (px, from the item's LEFT edge) of LOCAL byte
/// `byte` (an offset into `it.text`, i.e. `global_byte − char_start`). LTR walks
/// the shaped cluster advances (the same walk as lib.rs `item_width_to_byte`);
/// RTL (gated on `ShapedRun.rtl`) MIRRORS about the item width
/// because glyphs are stored right-to-left, so byte 0 sits at the RIGHT edge.
/// Unshaped items fall back to a proportional char-fraction split (accepted v1
/// approximation — `item.text` vs `ShapedRun.text` divergence on
/// stripped-control-char segments is not reconciled).
pub(crate) fn byte_to_x_local(it: &Item, byte: usize) -> f64 {
    let len = it.text.len();
    // v1 LIMITATION: a lone RTL glyph shapes as `rtl = false` (see the
    // `glyphs.len() < 2` gate in engine/fonts.rs — one glyph cannot be ordered).
    // The caret SIDE for such a single-char RTL word is therefore mapped LTR. The
    // cluster is atomic (fully-covered → whole box), so no wrong-region
    // highlight results; only the sub-glyph caret can be off-by-one on a
    // one-character RTL word. Reconciled by the RTL/float-bidi selection follow-on.
    let rtl = it.shaped.as_ref().is_some_and(|s| s.rtl);
    if byte == 0 {
        return if rtl { it.width } else { 0.0 };
    }
    if byte >= len {
        return if rtl { 0.0 } else { it.width };
    }
    if let Some(shaped) = &it.shaped {
        // Sum advances of glyphs whose cluster is logically BEFORE `byte`. For
        // RTL storage those glyphs sit to the RIGHT, so the pre-byte width is
        // measured from the right edge → mirror about `it.width`.
        let before: f64 = shaped
            .glyphs
            .iter()
            .filter(|g| (g.cluster as usize) < byte)
            .map(|g| g.x_advance)
            .sum();
        if rtl {
            it.width - before
        } else {
            before
        }
    } else {
        let total = it.text.chars().count().max(1) as f64;
        let before = it.text[..byte.min(len)].chars().count() as f64;
        it.width * (before / total)
    }
}

/// Inverse of `byte_to_x_local`: given a local x offset, return the
/// LOCAL byte at the nearest caret boundary. Candidate boundaries are the shaped
/// cluster starts ∪ `it.text.len()` (or char boundaries when unshaped); the one
/// whose `byte_to_x_local` is closest to `x_local` wins. Cluster snapping
/// is inherent — the returned byte is always a cluster/char boundary. Direction
/// is handled entirely by the (gated) forward map, so this stays direction-
/// agnostic.
pub(crate) fn x_to_byte_local(it: &Item, x_local: f64) -> usize {
    let len = it.text.len();
    if len == 0 {
        return 0;
    }
    let mut boundaries: Vec<usize> = Vec::new();
    if let Some(shaped) = &it.shaped {
        for g in &shaped.glyphs {
            let b = g.cluster as usize;
            if b <= len && !boundaries.contains(&b) {
                boundaries.push(b);
            }
        }
        if !boundaries.contains(&0) {
            boundaries.push(0);
        }
        if !boundaries.contains(&len) {
            boundaries.push(len);
        }
    } else {
        // Char boundaries of the unshaped text.
        boundaries.extend(it.text.char_indices().map(|(i, _)| i));
        boundaries.push(len);
    }
    boundaries.sort_unstable();
    let mut best = boundaries[0];
    let mut best_d = f64::INFINITY;
    for &b in &boundaries {
        let d = (byte_to_x_local(it, b) - x_local).abs();
        if d < best_d {
            best_d = d;
            best = b;
        }
    }
    best
}

/// Snap `byte` down to the nearest char boundary at or before it, clamped into
/// `text` (`str::floor_char_boundary` is still nightly-only). The stable stand-in
/// used wherever a byte offset must land on a valid slice edge.
fn floor_char_boundary(text: &str, byte: usize) -> usize {
    let mut b = byte.min(text.len());
    while b > 0 && !text.is_char_boundary(b) {
        b -= 1;
    }
    b
}

/// Snap a LOCAL byte offset (into `it.text`) to the nearest shaped-cluster
/// START. Candidate boundaries are the `ShapedGlyph.cluster` values ∪ {0, len}
/// (the same cluster set `x_to_byte_local` uses); unshaped items fall back to the
/// nearest char boundary at or before `byte`. Consumer-side snapping guard: a
/// `TextPos` recorded under fallback metrics can, once the real font arrives and
/// re-shapes, land in the interior of a cluster — snap it before measuring.
fn snap_local_to_cluster(it: &Item, byte: usize) -> usize {
    let len = it.text.len();
    let byte = byte.min(len);
    let mut boundaries: Vec<usize> = Vec::new();
    if let Some(shaped) = &it.shaped {
        for g in &shaped.glyphs {
            let b = g.cluster as usize;
            if b <= len && !boundaries.contains(&b) {
                boundaries.push(b);
            }
        }
    }
    if boundaries.is_empty() {
        // Unshaped → nearest char boundary at or before `byte` (already ≤ len).
        return floor_char_boundary(&it.text, byte);
    }
    if !boundaries.contains(&0) {
        boundaries.push(0);
    }
    if !boundaries.contains(&len) {
        boundaries.push(len);
    }
    *boundaries
        .iter()
        .min_by_key(|&&b| (b as isize - byte as isize).unsigned_abs())
        .unwrap()
}

// ─── Hit-test granularity + rect payload ────────────────────────────────────

/// Selection granularity requested by `hitTest`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Granularity {
    Char,
    Word,
    Paragraph,
}

impl Granularity {
    pub(crate) fn from_str(s: &str) -> Granularity {
        match s {
            "word" => Granularity::Word,
            "paragraph" | "para" => Granularity::Paragraph,
            _ => Granularity::Char,
        }
    }
}

/// Vertical band of a line: `[baseline_y − line_height·0.85, +line_height)` —
/// the `collect_comment_rects` idiom. `top` is the highlight/hit top edge.
fn line_band_top(line: &PagedLine) -> f64 {
    line.baseline_y - line.line_height * 0.85
}

/// Walk every selectable body/table-cell line on a page in paint order
/// (headers/footers/footnote bands/textboxes are NOT selectable and carry
/// `PARA_SENTINEL`, so they are excluded here by construction). Yields each
/// `&PagedLine`.
fn for_each_body_line<'a>(page: &'a PageLayout, mut f: impl FnMut(&'a PagedLine)) {
    for el in &page.elements {
        match el {
            PageElement::Line(line) => f(line),
            PageElement::TableRow(row) => {
                for cell in &row.cells {
                    for line in &cell.lines {
                        f(line);
                    }
                }
            }
        }
    }
}

/// Iterate a segment's items paired with each item's LEFT edge, accumulating the
/// running x-cursor from `seg.x_start` (`+= it.width` per item). Centralizes the
/// hand-rolled cursor walk shared by the hit-test / selection-rect / object passes
/// (right edge is `left + it.width` at the call site).
fn items_with_left(seg: &LineSegment) -> impl Iterator<Item = (f64, &Item)> {
    let mut cursor = seg.x_start;
    seg.items.iter().map(move |it| {
        let left = cursor;
        cursor += it.width;
        (left, it)
    })
}

/// Resolve `(para, byte)` for a horizontal position `x` on a specific line
/// (its items already laid out left-to-right per segment). Cluster-snapped at
/// consumption (inherent in `x_to_byte_local`). Outside all item boxes →
/// the geometrically-nearest item edge, never a logical-neighbour walk.
/// A sentinel item resolves to para-start (byte 0).
fn resolve_x_on_line(line: &PagedLine, x: f64) -> TextPos {
    let para = line.para;
    // Track the nearest item edge in case x falls in a gap / off the ends.
    let mut best: Option<(f64, u32)> = None; // (distance, byte)
    let mut consider = |dist: f64, byte: u32| {
        if best.is_none_or(|(d, _)| dist < d) {
            best = Some((dist, byte));
        }
    };
    for seg in &line.segments {
        for (left, it) in items_with_left(seg) {
            let right = left + it.width;
            let byte_at = |local_x: f64| -> u32 {
                if it.char_start == CHAR_START_SENTINEL {
                    0 // A hit on a synthesized item snaps to para-start.
                } else {
                    it.char_start + x_to_byte_local(it, local_x) as u32
                }
            };
            if x >= left && x < right {
                return TextPos::new(para, byte_at(x - left));
            }
            // Nearest-edge candidates (both ends of the box).
            consider((x - left).abs(), byte_at(0.0));
            consider((x - right).abs(), byte_at(it.width));
        }
    }
    match best {
        Some((_, byte)) => TextPos::new(para, byte),
        None => TextPos::new(para, 0), // empty line → [para, 0]
    }
}

/// The `hitTest` char-granularity core. Finds the line whose band contains
/// `y` (else the vertically-nearest line), then resolves the byte within
/// it. Returns `None` only when the page has no selectable line. Word/paragraph
/// expansion is applied by the caller against `ParaMeta.logical_text`.
pub(crate) fn hit_test_impl(page: &PageLayout, x: f64, y: f64) -> Option<TextPos> {
    let mut in_band: Option<&PagedLine> = None;
    let mut nearest: Option<(f64, &PagedLine)> = None;
    for_each_body_line(page, |line| {
        let top = line_band_top(line);
        let bottom = top + line.line_height;
        if y >= top && y < bottom && in_band.is_none() {
            in_band = Some(line);
        }
        let dist = if y < top { top - y } else if y >= bottom { y - bottom } else { 0.0 };
        if nearest.is_none_or(|(d, _)| dist < d) {
            nearest = Some((dist, line));
        }
    });
    let line = in_band.or(nearest.map(|(_, l)| l))?;
    Some(resolve_x_on_line(line, x))
}

/// The `selectionRects` core. Normalizes `start ≤ end` (lexicographic), then
/// per selectable line intersects each item's LOGICAL span `[char_start,
/// char_start+char_len)` (never the painted `text.len()`) with the
/// covered window, emitting DISJOINT visual rects. No line-contiguity
/// assumption: covered x-intervals are merged by geometric overlap, so a
/// permuted bidi/float line yields correct disjoint rects with zero special
/// casing.
pub(crate) fn selection_rects_impl(page: &PageLayout, start: TextPos, end: TextPos) -> Vec<SelRect> {
    let (start, end) = if end < start { (end, start) } else { (start, end) };
    let mut rects: Vec<SelRect> = Vec::new();
    for_each_body_line(page, |line| {
        let para = line.para;
        if para == PARA_SENTINEL || para < start.para || para > end.para {
            return;
        }
        let lo = if para == start.para { start.byte } else { 0 };
        let hi = if para == end.para { end.byte } else { u32::MAX };
        // Collect covered x-intervals on this line.
        let mut intervals: Vec<(f64, f64)> = Vec::new();
        for seg in &line.segments {
            for (left, it) in items_with_left(seg) {
                let width = it.width;
                if it.char_start == CHAR_START_SENTINEL {
                    // A synthesized item is painted when the covered range
                    // includes byte 0 of the paragraph.
                    if lo == 0 && hi > 0 {
                        // Highlight-hug fix — an INLINE
                        // OBJECT item (image or chart) highlights with its OBJECT
                        // BOX (`inline_image_rect`, the painted bitmap box), not
                        // the line-metrics band: on a tall object line the band
                        // paints an "offset square" beside/around the object.
                        // Emitted as its OWN rect (never merged into the text
                        // intervals, whose band stays the line box). Text-only
                        // lines are byte-identical.
                        if it.image.is_some() {
                            if let Some(r) = inline_image_rect(line, left, it) {
                                rects.push(r);
                            }
                            continue;
                        }
                        // Numbering markers keep the line band (unchanged).
                        intervals.push((left, left + width));
                    }
                    continue;
                }
                let cs = it.char_start;
                let ce = it.char_start + it.char_len;
                let cov_lo = cs.max(lo);
                let cov_hi = ce.min(hi);
                if cov_lo >= cov_hi {
                    continue; // no overlap
                }
                if cov_lo == cs && cov_hi == ce {
                    // Fully covered → whole box (robust to PAGE mutation).
                    intervals.push((left, left + width));
                } else {
                    // Partial: slice via the (direction-gated) forward map, and
                    // snap each local byte to the nearest shaped-cluster START at
                    // consumption (a TextPos stored under fallback metrics then
                    // re-shaped after font-arrival can arrive mid-cluster).
                    let lo_local = snap_local_to_cluster(it, (cov_lo - cs) as usize);
                    let hi_local = snap_local_to_cluster(it, (cov_hi - cs) as usize);
                    let x0 = byte_to_x_local(it, lo_local);
                    let x1 = byte_to_x_local(it, hi_local);
                    let (a, b) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
                    intervals.push((left + a, left + b));
                }
            }
        }
        if intervals.is_empty() {
            return;
        }
        // Merge overlapping/adjacent intervals → disjoint rects (linkRects idiom).
        intervals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let top = line_band_top(line);
        let h = line.line_height;
        let mut cur = intervals[0];
        for &(l, r) in &intervals[1..] {
            if l <= cur.1 + 0.5 {
                cur.1 = cur.1.max(r);
            } else {
                rects.push(SelRect { x: cur.0, y: top, w: cur.1 - cur.0, h });
                cur = (l, r);
            }
        }
        rects.push(SelRect { x: cur.0, y: top, w: cur.1 - cur.0, h });
    });
    rects
}

// ─── Selection paint pass ───────────────────────────────────────────────────

/// The WML selection highlight is a THIRD additive
/// translucent pass, mirroring the search swipe (lib.rs `SEARCH_FILL_*`). ~30%
/// alpha blue over the glyphs stays legible (the 40% search fill proves the
/// model), reading perceptually as a native selection BEHIND the text. Themable
/// by a later plugin. Kept beside `paint_selection_pass`, its sole
/// consumer; mirrors lib.rs SEARCH_FILL_* (kept private there). The color is
/// distinct from search (`#FFE066`) so tests can assert it unambiguously.
const SELECTION_FILL_ALPHA: f64 = 0.30;
const SELECTION_FILL_COLOR: &str = "#3B82F6";

/// The WML selection paint pass. Third additive
/// translucent pass, painted through `&dyn Painter` so the worker
/// `RecordingPainter` records it identically (the worker pixel-identity canary)
/// and slice culling is inherited via `cull::keep` — NO worker
/// special-casing. `sel` is the viewer-held `selection`, set fire-and-forget by
/// `docSetSelection`, so a drag repaints without touching layout. `None` (or a
/// collapsed range ⇒ zero rects) emits ZERO draw commands (not even a `save`),
/// so a no-selection render is BYTE-IDENTICAL. Alpha/color are set ONCE (no
/// active/inactive split, unlike search). Selection is doc-global;
/// `selection_rects_impl` filters by para, so a cross-page selection paints on
/// each page owning covered paras when this is invoked per page.
pub(crate) fn paint_selection_pass(
    p: &dyn Painter,
    page: &PageLayout,
    sel: Option<(TextPos, TextPos)>,
    clip: Option<&cull::Rect>,
) {
    let Some((start, end)) = sel else { return };
    let (start, end) = if end < start { (end, start) } else { (start, end) };
    let rects = selection_rects_impl(page, start, end);
    if rects.is_empty() {
        return; // collapsed caret / no covered line ⇒ byte-identical no-op.
    }
    p.save();
    p.set_global_alpha(SELECTION_FILL_ALPHA);
    p.set_fill_style_str(SELECTION_FILL_COLOR);
    for r in &rects {
        if !cull::keep(clip, r.x, r.y, r.x + r.w, r.y + r.h) {
            continue;
        }
        p.fill_rect(r.x, r.y, r.w, r.h);
    }
    p.restore();
}

/// Anchored-image copy — tint the floating images whose ANCHOR
/// paragraph falls inside the selection (they are included by extractRange,
/// para-level v1, so they must LOOK included). A SEPARATE pass from
/// `paint_selection_pass` because in-front anchored images draw AFTER the body
/// content: a tint painted with the text pass gets buried under the opaque
/// image draw (the z-order bug the split fixes) — this pass runs after the
/// in-front image draw, right before the object outline. `anchor_para` is the
/// inert linkage stamped at layout; carriers without one (None) never tint.
/// Gated on a non-collapsed range, mirroring the extraction guard. No
/// selection ⇒ zero draw commands ⇒ byte-identical render.
pub(crate) fn paint_anchored_selection_tint(
    p: &dyn Painter,
    page: &PageLayout,
    sel: Option<(TextPos, TextPos)>,
    clip: Option<&cull::Rect>,
) {
    let Some((start, end)) = sel else { return };
    let (start, end) = if end < start { (end, start) } else { (start, end) };
    if start >= end {
        return; // collapsed caret ⇒ nothing swept in.
    }
    let rects: Vec<SelRect> = page
        .anchored_images
        .iter()
        .filter_map(|ai| {
            let ap = ai.anchor_para?;
            (ap >= start.para && ap <= end.para).then(|| anchored_rect(ai))
        })
        .collect();
    if rects.is_empty() {
        return;
    }
    p.save();
    p.set_global_alpha(SELECTION_FILL_ALPHA);
    p.set_fill_style_str(SELECTION_FILL_COLOR);
    for r in &rects {
        if !cull::keep(clip, r.x, r.y, r.x + r.w, r.y + r.h) {
            continue;
        }
        p.fill_rect(r.x, r.y, r.w, r.h);
    }
    p.restore();
}

// ─── Object hit-test + outline ──────────────────────────────────────────────

/// An image object hit result: the topmost image object
/// under a point, in object-first z-order. `rect` is page-absolute display px.
/// Charts — `kind: "chart"` marks an inline CHART hit (a distinct
/// kind so TS routes it to the chart flavors, never the image-bytes paths).
pub(crate) struct ObjectHit {
    pub(crate) rel_id: String,
    pub(crate) kind: &'static str, // "anchoredImage" | "inlineImage" | "chart"
    pub(crate) rect: SelRect,
}

fn anchored_rect(ai: &AnchoredImageLayout) -> SelRect {
    SelRect { x: ai.x, y: ai.y, w: ai.width_px, h: ai.height_px }
}

/// Painted bitmap box for one inline-image `Item`, via the shared
/// `ImageItem::painted_box` (the SAME arithmetic the renderer draws with).
/// `x_cursor` is the item's left edge.
fn inline_image_rect(line: &PagedLine, x_cursor: f64, it: &Item) -> Option<SelRect> {
    let img = it.image.as_ref()?;
    let (x, y, w, h) = img.painted_box(x_cursor, it.width, line.baseline_y);
    Some(SelRect { x, y, w, h })
}

fn point_in(r: &SelRect, x: f64, y: f64) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

/// Object-first z-order: the topmost PAINTED image wins. Paint
/// order is behind-text anchored → body/inline flow → in-front anchored, so the
/// hit search reverses it: in-front anchored (reverse insertion) first, then
/// inline images in flow, then behind-text anchored (reverse). `is_image`
/// (threaded from `lib.rs`) excludes charts; `is_chart` admits an
/// inline CHART as a hit with the DISTINCT kind `"chart"` — charts stay OUT of
/// the image-bytes paths (their PNG is rendered on demand). Anchored charts
/// never reach `page.anchored_images` (`anchored_image_rel_ids` is `<a:blip>`-
/// only), so the anchored arms stay `is_image`-gated. Returns the object under
/// `(x, y)` or `None`. A NEW READ — never mutates the page model, so it is
/// layout/render byte-neutral.
pub(crate) fn hit_test_object_impl(
    page: &PageLayout,
    x: f64,
    y: f64,
    is_image: &dyn Fn(&str) -> bool,
    is_chart: &dyn Fn(&str) -> bool,
) -> Option<ObjectHit> {
    // In-front anchored images are painted last ⇒ topmost. Reverse insertion so
    // a later-drawn overlapping image wins.
    for ai in page.anchored_images.iter().rev() {
        if !ai.behind_text && is_image(&ai.rel_id) {
            let r = anchored_rect(ai);
            if point_in(&r, x, y) {
                return Some(ObjectHit { rel_id: ai.rel_id.clone(), kind: "anchoredImage", rect: r });
            }
        }
    }
    // Inline images + charts in text flow (drawn above behind-text anchors,
    // below in-front). A chart item is the same `Item.image` seam under its
    // `chart::<relId>` key; the kind alone diverges.
    let mut hit: Option<ObjectHit> = None;
    for_each_body_line(page, |line| {
        if hit.is_some() {
            return;
        }
        for seg in &line.segments {
            for (left, it) in items_with_left(seg) {
                if let Some(img) = it.image.as_ref() {
                    let kind = if is_image(&img.rel_id) {
                        Some("inlineImage")
                    } else if is_chart(&img.rel_id) {
                        Some("chart")
                    } else {
                        None
                    };
                    if let Some(kind) = kind {
                        if let Some(r) = inline_image_rect(line, left, it) {
                            if point_in(&r, x, y) {
                                hit = Some(ObjectHit {
                                    rel_id: img.rel_id.clone(),
                                    kind,
                                    rect: r,
                                });
                                return;
                            }
                        }
                    }
                }
            }
        }
    });
    if hit.is_some() {
        return hit;
    }
    // Behind-text anchored images are painted first ⇒ bottommost.
    for ai in page.anchored_images.iter().rev() {
        if ai.behind_text && is_image(&ai.rel_id) {
            let r = anchored_rect(ai);
            if point_in(&r, x, y) {
                return Some(ObjectHit { rel_id: ai.rel_id.clone(), kind: "anchoredImage", rect: r });
            }
        }
    }
    None
}

/// The selected-object OUTLINE (read-only, no handles). The
/// same blue as the selection fill; a 2px stroke reads as a focus ring.
const OBJECT_OUTLINE_COLOR: &str = "#3B82F6";
const OBJECT_OUTLINE_WIDTH_PX: f64 = 2.0;

/// Every painted box for `rel_id` on this page (an anchored image can appear
/// once; an inline image once per placement — outline strokes ALL matching
/// placements, the `index:0` disambiguation is a v1 deferral). Matches by KEY
/// only, so a selected inline CHART (`chart::<relId>`) outlines exactly like an
/// image. `pub(crate)` so `lib.rs` can derive a chart's display
/// box for the `chartData` export.
pub(crate) fn object_outline_rects(page: &PageLayout, rel_id: &str) -> Vec<SelRect> {
    let mut rects = Vec::new();
    for ai in &page.anchored_images {
        if ai.rel_id == rel_id {
            rects.push(anchored_rect(ai));
        }
    }
    for_each_body_line(page, |line| {
        for seg in &line.segments {
            for (left, it) in items_with_left(seg) {
                if it.image.as_ref().is_some_and(|i| i.rel_id == rel_id) {
                    if let Some(r) = inline_image_rect(line, left, it) {
                        rects.push(r);
                    }
                }
            }
        }
    });
    rects
}

/// The object-outline paint pass (additive, like
/// `paint_selection_pass`). `selected == None` (or an unknown/off-page relId)
/// ⇒ ZERO draw commands (not even a `save`), so a no-object-selected render is
/// BYTE-IDENTICAL. Painted through `&dyn Painter` so the worker RecordingPainter
/// records it identically and slice-culling is inherited via `cull::keep`.
pub(crate) fn paint_object_outline_pass(
    p: &dyn Painter,
    page: &PageLayout,
    selected: Option<&str>,
    clip: Option<&cull::Rect>,
) {
    let Some(rel_id) = selected else {
        return;
    };
    let rects = object_outline_rects(page, rel_id);
    if rects.is_empty() {
        return; // unknown/off-page relId ⇒ byte-identical no-op.
    }
    p.save();
    p.set_stroke_style_str(OBJECT_OUTLINE_COLOR);
    p.set_line_width(OBJECT_OUTLINE_WIDTH_PX);
    for r in &rects {
        if !cull::keep(clip, r.x, r.y, r.x + r.w, r.y + r.h) {
            continue;
        }
        p.stroke_rect(r.x, r.y, r.w, r.h);
    }
    p.restore();
}

// ─── Word / paragraph boundaries ────────────────────────────────────────────

/// True for a CJK ideograph that forms its own single-character word (each
/// ideograph is its own word; no `unicode-segmentation` dep in v1).
///
/// v1 LIMITATION: only the BMP ranges below are treated as ideographs. CJK
/// Extensions B–F (≥ U+20000) and Hangul syllables fall to the alnum path in
/// `word_bounds` (they group as words rather than per-ideograph). Acceptable for
/// v1; a full UAX #29 segmenter is deferred.
fn is_cjk_ideograph(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF   // CJK Unified Ideographs
        | 0x3400..=0x4DBF // Extension A
        | 0xF900..=0xFAFF // Compatibility Ideographs
        | 0x3040..=0x30FF // Hiragana + Katakana
    )
}

/// Word bounds around `byte` in `text` (a char offset; snapped to a char
/// boundary first). An alnum run expands over `char::is_alphanumeric`; a CJK
/// ideograph is a single-char word; whitespace/punctuation is a zero-width
/// `[byte, byte]`. Returns `(start, end)` byte offsets.
pub(crate) fn word_bounds(text: &str, byte: usize) -> (usize, usize) {
    if text.is_empty() {
        return (0, 0);
    }
    // Snap to a char boundary at or before `byte`.
    let b = floor_char_boundary(text, byte);
    // The char starting at `b` (if any); if `b == len`, look at the previous.
    let here = text[b..].chars().next();
    let target = match here {
        Some(c) => c,
        None => return (text.len(), text.len()),
    };
    if is_cjk_ideograph(target) {
        return (b, b + target.len_utf8());
    }
    if !target.is_alphanumeric() {
        return (b, b); // whitespace/punct → zero-width caret
    }
    // Expand left over alphanumerics.
    let mut start = b;
    for (i, c) in text[..b].char_indices().rev() {
        if c.is_alphanumeric() && !is_cjk_ideograph(c) {
            start = i;
        } else {
            break;
        }
    }
    // Expand right over alphanumerics.
    let mut end = b;
    for (i, c) in text[b..].char_indices() {
        if c.is_alphanumeric() && !is_cjk_ideograph(c) {
            end = b + i + c.len_utf8();
        } else {
            break;
        }
    }
    (start, end)
}

// ── Table structure side table — captured from TableCache in lib.rs ─────────

/// One cell's structure (no paragraph content — that arrives from `ParaMeta`).
#[derive(Clone, Debug)]
pub(crate) struct CellMeta {
    pub(crate) rowspan: u32,
    pub(crate) colspan: u32,
    pub(crate) fill: Option<String>,
    pub(crate) borders: [Option<FragBorder>; 4],
    pub(crate) v_merged_away: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CellMetaRow {
    pub(crate) cells: Vec<CellMeta>,
}

/// A top-level table's structure, keyed `(sec, block)` in the `table_meta` side
/// table. Cells are indexed the SAME way `ParaLocation::TableCell.cell` is (the
/// cache-row `cell_idx`), so an extracted cell paragraph routes straight in.
#[derive(Clone, Debug)]
pub(crate) struct TableMeta {
    pub(crate) rows: Vec<CellMetaRow>,
}

// `FragTable` is engine-owned (`engine::fragment`); this builder lives HERE,
// beside the WML-side `TableMeta` it reads, so engine/ stays format-free
// (layering RULE 3, `scripts/check-layering.mjs`). Same-crate inherent impls on the engine type are fine.
impl FragTable {
    /// Build the FULL grid from a captured `TableMeta`, all cells empty.
    fn from_meta(m: &TableMeta) -> FragTable {
        FragTable {
            rows: m
                .rows
                .iter()
                .map(|r| FragRow {
                    cells: r
                        .cells
                        .iter()
                        .map(|c| FragCell {
                            rowspan: c.rowspan,
                            colspan: c.colspan,
                            fill: c.fill.clone(),
                            borders: c.borders.clone(),
                            v_merged_away: c.v_merged_away,
                            blocks: Vec::new(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

fn underline_str(u: Underline) -> &'static str {
    match u {
        Underline::None => "none",
        Underline::Single => "single",
        Underline::Double => "double",
        Underline::Thick => "thick",
        Underline::Dotted => "dotted",
        Underline::Dash => "dash",
        Underline::Wave => "wave",
    }
}

fn vert_align_str(v: VertAlign) -> &'static str {
    match v {
        VertAlign::Baseline => "baseline",
        VertAlign::Superscript => "superscript",
        VertAlign::Subscript => "subscript",
    }
}

/// Snap a byte offset into `text` down to the nearest char boundary at or before
/// it. `ParaMeta` carries no shaped clusters, so char-boundary is the
/// available snap (the same fallback `snap_local_to_cluster` uses when unshaped);
/// it guarantees `logical_text` slicing never panics mid-char.
fn snap_byte_to_char(text: &str, byte: u32) -> u32 {
    floor_char_boundary(text, byte as usize) as u32
}

fn frag_run(rm: &RunMeta, text: String) -> FragRun {
    let s = &rm.style;
    FragRun {
        text,
        bold: s.bold,
        italic: s.italic,
        underline: underline_str(s.underline),
        strike: s.strikethrough,
        double_strike: s.double_strike,
        font_family: s.family.clone(),
        font_size_pt: s.font_pt,
        color: s.color.clone(),
        highlight: s.highlight.clone(),
        run_shading: s.run_shading.clone(),
        vert_align: vert_align_str(s.vert_align),
        link: rm.link.clone(),
        image: rm.image.clone(),
        footnote_mark: rm.footnote_mark.clone(),
    }
}

/// Anchored-image copy — the FragRun emitted for an ANCHORED image
/// swept into the copied range: empty text + a neutral style (the html image
/// arm reads only `image`; text/plain reads only `text` = "" — verified by the
/// flavor tests), mirroring how inline zero-width image runs serialize.
fn anchored_image_run(img: &ImageRef) -> FragRun {
    FragRun {
        image: Some(img.clone()),
        ..FragRun::neutral()
    }
}

fn frag_list(n: &NumInfo) -> FragList {
    FragList {
        ordered: !matches!(n.fmt, STNumberFormat::Bullet),
        num_id: n.num_id,
        level: n.ilvl,
        value: n.count,
        fmt: n.fmt.to_string(),
        marker: n.marker_text.clone(),
    }
}

/// Build one fragment paragraph over the byte window `[lo, hi)` of its logical
/// text. `lo`/`hi` are pre-snapped to char boundaries. Runs intersecting the
/// window contribute a text slice; zero-width object runs are included when their
/// byte cursor sits inside the window. The list marker is emitted ONLY when the
/// window includes the paragraph start (`lo == 0`, matching Word's
/// mid-paragraph-omits-number behavior and the sentinel-paints-at-byte-0 rule).
fn build_frag_para(meta: &ParaMeta, lo: u32, hi: u32, sel_nonempty: bool) -> FragPara {
    let mut runs: Vec<FragRun> = Vec::new();
    for rm in &meta.runs {
        let cs = rm.char_start;
        if rm.char_len == 0 {
            // Zero-width object/footnote run — included when its byte ∈ [lo, hi]
            // and the OVERALL selection is non-collapsed. Keying the guard on the
            // per-paragraph window (`lo < hi`) dropped the run for an OBJECT-ONLY
            // paragraph (Word emits a chart/image in its own text-less paragraph
            // → logical_text empty → window [0,0]) — a real ride-along
            // regression. The collapsed-caret protection lives in `sel_nonempty`.
            if sel_nonempty && cs >= lo && cs <= hi {
                runs.push(frag_run(rm, String::new()));
            }
            continue;
        }
        let ce = cs + rm.char_len;
        let clip_lo = cs.max(lo);
        let clip_hi = ce.min(hi);
        if clip_lo >= clip_hi {
            continue; // no overlap with the window
        }
        let text = meta.logical_text[clip_lo as usize..clip_hi as usize].to_string();
        runs.push(frag_run(rm, text));
    }
    let list = if lo == 0 {
        meta.numbering.as_ref().map(frag_list)
    } else {
        None
    };
    FragPara {
        runs,
        alignment: meta.alignment.to_string(),
        dir: if meta.base_rtl { "rtl" } else { "ltr" },
        list,
    }
}

fn close_open_table(open: &mut Option<(usize, usize, FragTable)>, blocks: &mut Vec<Block>) {
    if let Some((_, _, t)) = open.take() {
        blocks.push(Block::Table(t));
    }
}

/// Extract the logical-order `DocFragment` for the byte range
/// `[start, end]`. Pure read over the layout-time `ParaMeta` + `table_meta` side
/// tables (NEVER the paged/visual output). Normalizes `start ≤ end`, walks every
/// paragraph id in range, slices each run's text from `logical_text` in LOGICAL
/// order, and routes body paras vs. table-cell paras (opening/closing a full-grid
/// `FragTable` on the `(sec, block)` key).
pub(crate) fn extract_range_impl(
    paras: &[ParaMeta],
    tables: &HashMap<(usize, usize), TableMeta>,
    start: TextPos,
    end: TextPos,
) -> DocFragment {
    let (start, end) = if end < start { (end, start) } else { (start, end) };
    let mut blocks: Vec<Block> = Vec::new();
    let mut open_table: Option<(usize, usize, FragTable)> = None;

    for para_id in start.para..=end.para {
        let Some(meta) = paras.get(para_id as usize) else {
            continue;
        };
        let text_len = meta.logical_text.len() as u32;
        let raw_lo = if para_id == start.para { start.byte } else { 0 };
        let raw_hi = if para_id == end.para { end.byte } else { text_len };
        let lo = snap_byte_to_char(&meta.logical_text, raw_lo);
        let hi = snap_byte_to_char(&meta.logical_text, raw_hi);
        let mut para = build_frag_para(meta, lo, hi, start != end);

        // Anchored-image copy — Word parity: copying a
        // TEXT range that covers a floating image's anchor brings the image
        // along. v1 inclusion is PARA-LEVEL: the anchor paragraph being inside
        // [start.para, end.para] includes the image, even when the byte window
        // only partially covers that paragraph. Word is char-position-precise
        // about the anchor run; this is a deliberate deviation.
        // A collapsed caret (start == end) emits nothing. One emission per
        // anchored occurrence — a rel anchored in two paragraphs emits twice,
        // matching Word pasting two. Appended AFTER the text runs.
        if start < end {
            for img in &meta.anchored_images {
                para.runs.push(anchored_image_run(img));
            }
        }

        match &meta.location {
            ParaLocation::Body { .. } => {
                close_open_table(&mut open_table, &mut blocks);
                blocks.push(Block::Para(para));
            }
            ParaLocation::TableCell {
                sec,
                block,
                row,
                cell,
            } => {
                let same = matches!(&open_table, Some((s, b, _)) if s == sec && b == block);
                if !same {
                    close_open_table(&mut open_table, &mut blocks);
                    let t = tables
                        .get(&(*sec, *block))
                        .map(FragTable::from_meta)
                        .unwrap_or(FragTable { rows: Vec::new() });
                    open_table = Some((*sec, *block, t));
                }
                if let Some((_, _, t)) = open_table.as_mut() {
                    if let Some(r) = t.rows.get_mut(*row) {
                        if let Some(c) = r.cells.get_mut(*cell) {
                            c.blocks.push(Block::Para(para));
                        }
                    }
                }
            }
        }
    }
    close_open_table(&mut open_table, &mut blocks);
    DocFragment { blocks }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::fonts::{ShapedGlyph, ShapedRun};
    use crate::engine::paint::RunStyle;

    /// Build a text item with an explicit shaped run. `glyphs` = (cluster,
    /// advance) pairs in STORAGE order; `rtl` sets the `ShapedRun.rtl` gate.
    fn mk_item(text: &str, width: f64, glyphs: &[(u32, f64)], rtl: bool, char_start: u32) -> Item {
        let shaped = if glyphs.is_empty() {
            None
        } else {
            Some(ShapedRun {
                glyphs: glyphs
                    .iter()
                    // `gid` is oracle-only; the x↔byte maps under test read
                    // cluster + advance, so a constant is honest here.
                    .map(|&(c, a)| ShapedGlyph { cluster: c, x_advance: a, x_offset: 0.0, y_offset: 0.0, gid: 0 })
                    .collect(),
                total_advance: glyphs.iter().map(|g| g.1).sum(),
                text: text.to_string(),
                is_simple: false,
                rtl,
            })
        };
        Item {
            text: text.to_string(),
            style: RunStyle::marker(),
            width,
            content_width: width,
            shaped,
            char_start,
            char_len: text.len() as u32,
            rtl,
            ..Default::default()
        }
    }

    // ── Trace pin: the exact RTL forward-map worked example ──────────────────
    // "שלום": 8 bytes, 4 glyphs adv 10, width 40, stored RTL (clusters 6,4,2,0).
    #[test]
    fn rtl_forward_map_trace_pin() {
        let it = mk_item("שלום", 40.0, &[(6, 10.0), (4, 10.0), (2, 10.0), (0, 10.0)], true, 0);
        assert!(it.shaped.as_ref().unwrap().rtl, "descending clusters ⇒ rtl");
        // byte 0 sits at the RIGHT edge (local 40), byte 2 at 30, byte 8 at 0.
        assert!((byte_to_x_local(&it, 0) - 40.0).abs() < 1e-9);
        assert!((byte_to_x_local(&it, 2) - 30.0).abs() < 1e-9);
        assert!((byte_to_x_local(&it, 8) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn ltr_forward_map_is_left_to_right() {
        let it = mk_item("abcd", 40.0, &[(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0)], false, 0);
        assert!(!it.shaped.as_ref().unwrap().rtl);
        assert!((byte_to_x_local(&it, 0) - 0.0).abs() < 1e-9);
        assert!((byte_to_x_local(&it, 2) - 20.0).abs() < 1e-9);
        assert!((byte_to_x_local(&it, 4) - 40.0).abs() < 1e-9);
    }

    #[test]
    fn x_to_byte_is_inverse_and_cluster_snaps() {
        // LTR: an x near the middle snaps to the nearest cluster boundary.
        let it = mk_item("abcd", 40.0, &[(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0)], false, 0);
        assert_eq!(x_to_byte_local(&it, 0.0), 0);
        assert_eq!(x_to_byte_local(&it, 19.0), 2); // nearest boundary to 19 is byte 2 (x=20)
        assert_eq!(x_to_byte_local(&it, 40.0), 4);
        // RTL inverse: x at the right edge is byte 0, left edge is byte 8.
        let r = mk_item("שלום", 40.0, &[(6, 10.0), (4, 10.0), (2, 10.0), (0, 10.0)], true, 0);
        assert_eq!(x_to_byte_local(&r, 40.0), 0);
        assert_eq!(x_to_byte_local(&r, 0.0), 8);
        assert_eq!(x_to_byte_local(&r, 30.0), 2);
    }

    #[test]
    fn word_bounds_alnum_cjk_and_punct() {
        // Alnum run expands both directions.
        assert_eq!(word_bounds("Hello World", 2), (0, 5));
        assert_eq!(word_bounds("Hello World", 8), (6, 11));
        // Whitespace/punct is a zero-width caret.
        assert_eq!(word_bounds("Hello World", 5), (5, 5)); // the space
        assert_eq!(word_bounds("a,b", 1), (1, 1)); // the comma
        // CJK ideograph is its own single-char word.
        let cjk = "你好";
        assert_eq!(word_bounds(cjk, 0), (0, 3));
        assert_eq!(word_bounds(cjk, 3), (3, 6));
        // Empty text.
        assert_eq!(word_bounds("", 0), (0, 0));
    }

    #[test]
    fn textpos_total_order_is_lexicographic() {
        assert!(TextPos::new(0, 5) < TextPos::new(1, 0));
        assert!(TextPos::new(1, 2) < TextPos::new(1, 3));
        assert_eq!(TextPos::new(2, 4), TextPos::new(2, 4));
    }

    use crate::wml::linebreak::LineSegment;

    /// A single body line at baseline 20, line_height 20, one segment at x=0.
    fn line_with(items: Vec<Item>, para: u32) -> PagedLine {
        PagedLine {
            segments: vec![LineSegment { x_start: 0.0, items }],
            para,
            baseline_y: 20.0,
            para_shading: None,
            line_height: 20.0,
            content_left: 0.0,
            content_right: 200.0,
            border_top: None,
            border_bottom: None,
            border_left: None,
            border_right: None,
            bar_tabs: Vec::new(),
            is_first_line: true,
            is_last_line: true,
        }
    }

    fn page_with(line: PagedLine) -> PageLayout {
        let mut pg = PageLayout::new();
        pg.push(PageElement::Line(line));
        pg
    }

    // A selection landing INSIDE a bidi run yields DISJOINT
    // per-line rects. Visual line "abc [שלום] xyz"; logical [1,5) covers abc's
    // tail (left rect) and the RTL word's FIRST logical char, which paints at the
    // RIGHT end of the RTL box — leaving the RTL box's left half unselected → a
    // gap → two disjoint rects.
    #[test]
    fn mixed_ltr_rtl_disjoint_rects() {
        let abc = mk_item("abc", 30.0, &[(0, 10.0), (1, 10.0), (2, 10.0)], false, 0);
        // RTL word: 8 bytes, char_start 3, glyphs stored right-to-left.
        let heb = mk_item(
            "שלום",
            40.0,
            &[(6, 10.0), (4, 10.0), (2, 10.0), (0, 10.0)],
            true,
            3,
        );
        let xyz = mk_item("xyz", 30.0, &[(0, 10.0), (1, 10.0), (2, 10.0)], false, 11);
        let page = page_with(line_with(vec![abc, heb, xyz], 0));
        let rects = selection_rects_impl(&page, TextPos::new(0, 1), TextPos::new(0, 5));
        assert_eq!(rects.len(), 2, "bidi selection must be disjoint: {:?}", rects);
        // Rects are x-disjoint (a real gap between them).
        let (mut r0, mut r1) = (rects[0], rects[1]);
        if r1.x < r0.x {
            std::mem::swap(&mut r0, &mut r1);
        }
        assert!(r0.x + r0.w < r1.x, "rects overlap: {:?} {:?}", r0, r1);
        // Expected geometry from the worked example: (10..30) and (60..70).
        assert!((r0.x - 10.0).abs() < 1e-6 && (r0.w - 20.0).abs() < 1e-6);
        assert!((r1.x - 60.0).abs() < 1e-6 && (r1.w - 10.0).abs() < 1e-6);
    }

    // A neutral-only item under an RTL base: Item.rtl (level parity) can be
    // true while ShapedRun.rtl (glyph storage) is false; the forward map must
    // gate on the SHAPED direction, so it stays LTR.
    #[test]
    fn neutral_punct_gates_on_shaped_not_item_rtl() {
        let mut it = mk_item("()", 20.0, &[(0, 10.0), (1, 10.0)], false, 0);
        it.rtl = true; // bidi level says RTL…
        assert!(!it.shaped.as_ref().unwrap().rtl); // …but glyphs shaped LTR.
        // Forward map uses shaped.rtl=false ⇒ left-to-right.
        assert!((byte_to_x_local(&it, 0) - 0.0).abs() < 1e-9);
        assert!((byte_to_x_local(&it, 1) - 10.0).abs() < 1e-9);
    }

    // An empty paragraph resolves to [para, 0].
    #[test]
    fn empty_line_hits_para_zero() {
        let page = page_with(line_with(vec![], 7));
        let pos = hit_test_impl(&page, 5.0, 20.0).expect("hit");
        assert_eq!(pos, TextPos::new(7, 0));
    }

    // A sentinel (numbering-marker) item: hit snaps to para-start, and
    // selectionRects paints it whenever the covered range includes byte 0.
    #[test]
    fn sentinel_marker_hit_and_paint() {
        let marker = mk_item("1.", 20.0, &[(0, 10.0), (1, 10.0)], false, CHAR_START_SENTINEL);
        let text = mk_item("Item", 40.0, &[(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0)], false, 0);
        let page = page_with(line_with(vec![marker, text], 0));
        // Hit on the marker (x in [0,20)) snaps to para-start byte 0.
        let pos = hit_test_impl(&page, 5.0, 20.0).expect("hit");
        assert_eq!(pos, TextPos::new(0, 0));
        // Selecting the whole para (byte 0..) paints the marker box + text.
        let rects = selection_rects_impl(&page, TextPos::new(0, 0), TextPos::new(0, 4));
        // Marker (0..20) merges with text (20..60) → one rect 0..60.
        assert_eq!(rects.len(), 1);
        assert!((rects[0].x - 0.0).abs() < 1e-6 && (rects[0].w - 60.0).abs() < 1e-6);
    }

    // Round-trip: hit an LTR word, resolve its bounds, get one enclosing rect.
    #[test]
    fn ltr_hit_to_rects_roundtrip() {
        let hello = mk_item("Hello", 50.0, &[(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0), (4, 10.0)], false, 0);
        let page = page_with(line_with(vec![hello], 0));
        // Hit near the 'l' (x≈25) → byte within [0,5].
        let pos = hit_test_impl(&page, 25.0, 20.0).expect("hit");
        assert_eq!(pos.para, 0);
        let (s, e) = word_bounds("Hello", pos.byte as usize);
        assert_eq!((s, e), (0, 5));
        let rects = selection_rects_impl(&page, TextPos::new(0, s as u32), TextPos::new(0, e as u32));
        assert_eq!(rects.len(), 1);
        assert!((rects[0].w - 50.0).abs() < 1e-6);
    }

    // The guard is a `debug_assert!` — it compiles OUT under `--release`,
    // so this should_panic test is only valid in debug builds. Gating it keeps the
    // release suite green without promoting the guard to a hard `assert!`.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "registered out of order")]
    fn exactly_once_guard_fires_on_desync() {
        // The debug-assert guards a REAL invariant: a `next_id` ahead of
        // the sink length (what an out-of-walk / double-register would produce)
        // trips it. next_id=3 but sink empty ⇒ 0 != 3 ⇒ panic.
        let mut next = 3u32;
        let mut sink: Vec<ParaMeta> = Vec::new();
        let mut cap = ParaCapture {
            next_id: &mut next,
            sink: &mut sink,
            location: ParaLocation::Body { sec: 0, block: 0 },
            nested: false,
        };
        cap.register(|id| ParaMeta {
            para_id: id,
            logical_text: String::new(),
            location: ParaLocation::Body { sec: 0, block: 0 },
            alignment: STJc::Start,
            base_rtl: false,
            numbering: None,
            runs: Vec::new(),
            anchored_images: Vec::new(),
        });
    }

    #[test]
    fn unshaped_item_falls_back_to_char_fraction() {
        let it = mk_item("abcd", 40.0, &[], false, 0);
        assert!(it.shaped.is_none());
        assert!((byte_to_x_local(&it, 2) - 20.0).abs() < 1e-9);
        assert_eq!(x_to_byte_local(&it, 21.0), 2);
    }

    // The load-bearing invariant, exercised through the
    // REAL `collect_items` path (not the synthetic `mk_item`). The paragraph mixes
    // a mid-text FOOTNOTE reference (a sentinel item that carries NON-empty text
    // "1" yet does NOT advance the byte cursor), a TAB, and an all-caps run
    // containing "ß" (`to_uppercase` → "SS"). It asserts the shared basis —
    // `logical_text[char_start .. char_start+char_len] == item.text` — for every
    // NON-sentinel item. FAILS without the sentinel filter in `logical_text_of`
    // (the concatenated "1" shifts the basis so "DE" no longer indexes to "DE").
    #[test]
    fn logical_text_invariant_holds_with_real_collect_items() {
        use crate::engine::fonts::{FontCache, ThemeFonts};
        use crate::wml::linebreak::collect_items;
        use ooxml_wml::ext::StyleContext;
        use ooxml_wml::types::{
            FootnoteEndnoteRef, OnOffElement, Paragraph, ParagraphContent, Run, RunContent,
            RunProperties, Text,
        };

        fn text_run(s: &str) -> Run {
            Run {
                run_content: vec![RunContent::T(Box::new(Text {
                    text: Some(s.to_string()),
                    ..Default::default()
                }))],
                ..Default::default()
            }
        }

        // Run 2 emits the sentinel footnote item (text = "1", the ref number).
        let footnote_run = Run {
            run_content: vec![RunContent::FootnoteReference(Box::new(FootnoteEndnoteRef {
                custom_mark_follows: None,
                id: 1,
            }))],
            ..Default::default()
        };
        // Run 3: all-caps "de\tß" → uppercased "DE\tSS" → split on the tab into
        // "DE" | <tab> | "SS".
        let mut caps_run = text_run("de\tß");
        caps_run.r_pr = Some(Box::new(RunProperties {
            caps: Some(Box::new(OnOffElement {
                value: Some("true".to_string()),
                ..Default::default()
            })),
            ..Default::default()
        }));

        let para = Paragraph {
            paragraph_content: vec![
                ParagraphContent::R(Box::new(text_run("abc"))),
                ParagraphContent::R(Box::new(footnote_run)),
                ParagraphContent::R(Box::new(caps_run)),
            ],
            ..Default::default()
        };

        let font_cache = FontCache::new();
        let style_ctx = StyleContext::default();
        let theme_fonts = ThemeFonts::empty();
        let (items, _, _, footnote_ids, _) =
            collect_items(&font_cache, &para, &style_ctx, &theme_fonts, &[])
                .expect("collect_items");

        // Sanity: the footnote ref was collected AND produced a sentinel item that
        // carries non-empty text — the exact shape that corrupts the basis.
        assert_eq!(footnote_ids, vec![1], "footnote ref must be collected");
        assert!(
            items
                .iter()
                .any(|i| i.char_start == CHAR_START_SENTINEL && i.text == "1"),
            "expected a sentinel footnote item carrying text \"1\": {:?}",
            items
                .iter()
                .map(|i| (i.text.clone(), i.char_start))
                .collect::<Vec<_>>()
        );
        // Sanity: the all-caps to_uppercase path ran ("ß" → "SS").
        assert!(
            items.iter().any(|i| i.text == "SS"),
            "expected an uppercased \"SS\" item: {:?}",
            items.iter().map(|i| i.text.clone()).collect::<Vec<_>>()
        );

        // The invariant: every NON-sentinel item's logical span indexes back to
        // its own text in the captured logical_text (the shared basis).
        let logical = logical_text_of(&items);
        for it in &items {
            if it.char_start == CHAR_START_SENTINEL {
                continue;
            }
            let cs = it.char_start as usize;
            let ce = cs + it.char_len as usize;
            assert!(
                ce <= logical.len(),
                "item span {cs}..{ce} out of bounds (logical.len()={}): {:?}",
                logical.len(),
                it.text
            );
            assert_eq!(
                &logical[cs..ce],
                it.text,
                "logical-text invariant violated: logical_text[{cs}..{ce}] != item.text"
            );
        }
    }

    // ── The selection paint pass ────────────────────────────────────────────
    // These drive `paint_selection_pass` through a `RecordingPainter` (the
    // worker backend), asserting the exact display-list command stream. Because
    // there is exactly ONE paint function that both backends drive, the recorded
    // stream IS what the in-process `CanvasPainter` issues → the worker
    // pixel-identity canary holds by construction (see the canary test below).
    use crate::engine::displaylist::{DrawCmd, RecordingPainter};

    /// A 4-glyph LTR item "abcd" (width 40, char_start 0) on one body line.
    fn ltr_page() -> PageLayout {
        let abcd = mk_item("abcd", 40.0, &[(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0)], false, 0);
        page_with(line_with(vec![abcd], 0))
    }

    // The byte-identity unit: None emits ZERO commands (not even a save).
    #[test]
    fn empty_selection_paints_nothing() {
        let page = ltr_page();
        let rec = RecordingPainter::new();
        paint_selection_pass(&rec, &page, None, None);
        assert!(rec.cmds().is_empty(), "None selection must emit zero commands: {:?}", *rec.cmds());
    }

    // A set selection records the exact translucent-rect sequence, and
    // every FillRect matches `selection_rects_impl` element-wise.
    #[test]
    fn selection_records_translucent_rect_sequence() {
        let page = ltr_page();
        let start = TextPos::new(0, 0);
        let end = TextPos::new(0, 4);
        let expect_rects = selection_rects_impl(&page, start, end);
        assert_eq!(expect_rects.len(), 1, "abcd fully covered → one rect");

        let rec = RecordingPainter::new();
        paint_selection_pass(&rec, &page, Some((start, end)), None);
        let cmds = rec.take();

        // Exact stream: Save, alpha, fill-style, one FillRect per rect, Restore.
        let mut expect: Vec<DrawCmd> = vec![
            DrawCmd::Save,
            DrawCmd::SetGlobalAlpha(SELECTION_FILL_ALPHA),
            DrawCmd::SetFillStyle(SELECTION_FILL_COLOR.into()),
        ];
        for r in &expect_rects {
            expect.push(DrawCmd::FillRect { x: r.x, y: r.y, w: r.w, h: r.h });
        }
        expect.push(DrawCmd::Restore);
        assert_eq!(cmds, expect, "selection paint must equal selection_rects_impl");
    }

    // A collapsed caret (zero-width) yields zero rects ⇒ zero commands.
    #[test]
    fn collapsed_selection_paints_nothing() {
        let page = ltr_page();
        let rec = RecordingPainter::new();
        paint_selection_pass(&rec, &page, Some((TextPos::new(0, 3), TextPos::new(0, 3))), None);
        assert!(rec.cmds().is_empty(), "collapsed caret must emit zero commands");
    }

    // Clear-back-to-zero at the pass level: Some(range) draws, then None
    // draws nothing.
    #[test]
    fn selection_then_none_returns_zero() {
        let page = ltr_page();
        let with = RecordingPainter::new();
        paint_selection_pass(&with, &page, Some((TextPos::new(0, 0), TextPos::new(0, 4))), None);
        assert!(!with.cmds().is_empty(), "a set selection must draw");
        let without = RecordingPainter::new();
        paint_selection_pass(&without, &page, None, None);
        assert!(without.cmds().is_empty(), "clearing must return to zero draws");
    }

    // A two-page (per-page) selection paints on BOTH pages. Each
    // page is painted with its own model; the pass filters by para.
    #[test]
    fn two_page_selection_paints_each_page() {
        let items0 = || vec![mk_item("abcd", 40.0, &[(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0)], false, 0)];
        let items1 = || vec![mk_item("wxyz", 40.0, &[(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0)], false, 0)];
        let page0 = page_with(line_with(items0(), 0));
        let page1 = page_with(line_with(items1(), 1));
        let sel = Some((TextPos::new(0, 0), TextPos::new(1, 999)));

        let r0 = RecordingPainter::new();
        paint_selection_pass(&r0, &page0, sel, None);
        assert!(
            r0.cmds().iter().any(|c| matches!(c, DrawCmd::FillRect { .. })),
            "page 0 (start para) must paint at least one rect"
        );
        let r1 = RecordingPainter::new();
        paint_selection_pass(&r1, &page1, sel, None);
        assert!(
            r1.cmds().iter().any(|c| matches!(c, DrawCmd::FillRect { .. })),
            "page 1 (end para) must paint at least one rect"
        );
    }

    // Slice-cull inheritance: a clip window covering only the first line
    // drops the second line's FillRect (same `cull::keep` path as search).
    #[test]
    fn selection_respects_clip_window() {
        // Two lines on one page, same para, at separated y (baseline 20 and 100).
        let g = [(0u32, 10.0f64), (1, 10.0), (2, 10.0), (3, 10.0)];
        let mut top_line = line_with(vec![mk_item("abcd", 40.0, &g, false, 0)], 0);
        top_line.baseline_y = 20.0; // band top = 20 - 20*0.85 = 3.0
        let mut bot_line = line_with(vec![mk_item("abcd", 40.0, &g, false, 0)], 0);
        bot_line.baseline_y = 100.0; // band top = 83.0
        let mut page = PageLayout::new();
        page.push(PageElement::Line(top_line));
        page.push(PageElement::Line(bot_line));

        // Clip window over ONLY the first line's band (y ~3..23).
        let clip = cull::Rect { x0: 0.0, y0: 0.0, x1: 200.0, y1: 40.0 };
        let rec = RecordingPainter::new();
        paint_selection_pass(&rec, &page, Some((TextPos::new(0, 0), TextPos::new(0, 4))), Some(&clip));
        let fills: Vec<_> = rec
            .cmds()
            .iter()
            .filter_map(|c| match c {
                DrawCmd::FillRect { y, .. } => Some(*y),
                _ => None,
            })
            .collect();
        assert_eq!(fills.len(), 1, "only the in-window line's rect survives cull: {:?}", fills);
        assert!((fills[0] - 3.0).abs() < 1e-6, "the surviving rect is the TOP line (y≈3): {:?}", fills);
    }

    // Pixel-identity canary, native form — worker == in-process by
    // construction. `paint_selection_pass` emits ONLY backend-invariant
    // primitives (save/set_global_alpha/set_fill_style_str/fill_rect/restore),
    // none of the two backend-divergent methods (`draw_image` key-vs-bitmap,
    // `measure_text_width` 0.0-vs-real). Both `render_wml_page` (CanvasPainter)
    // and `render_page_display_list` (RecordingPainter) drive the SAME
    // `paint_wml_page`, so the recorded selection subsequence IS what the canvas
    // backend issues. This test pins that the recorded stream contains only
    // those invariant primitives and matches `selection_rects_impl`.
    #[test]
    fn selection_uses_only_backend_invariant_primitives() {
        let page = ltr_page();
        let start = TextPos::new(0, 0);
        let end = TextPos::new(0, 4);
        let rec = RecordingPainter::new();
        paint_selection_pass(&rec, &page, Some((start, end)), None);
        let cmds = rec.take();
        let fills: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                DrawCmd::FillRect { x, y, w, h } => Some(SelRect { x: *x, y: *y, w: *w, h: *h }),
                _ => None,
            })
            .collect();
        // Every recorded command is a backend-invariant primitive.
        for c in &cmds {
            assert!(
                matches!(
                    c,
                    DrawCmd::Save
                        | DrawCmd::Restore
                        | DrawCmd::SetGlobalAlpha(_)
                        | DrawCmd::SetFillStyle(_)
                        | DrawCmd::FillRect { .. }
                ),
                "selection emitted a backend-divergent primitive: {:?}",
                c
            );
        }
        // The recorded rects match the geometry source of truth.
        assert_eq!(fills, selection_rects_impl(&page, start, end));
    }

    // ── extractRange (DocFragment) ───────────────────────────────────────────
    use crate::engine::paint::{Underline, VertAlign};
    use std::collections::HashMap as StdHashMap;

    /// Concatenate a paragraph's run text (the logical-order recovery proof).
    fn para_text(p: &FragPara) -> String {
        p.runs.iter().map(|r| r.text.as_str()).collect()
    }

    fn text_run_meta(cs: u32, text: &str) -> RunMeta {
        RunMeta {
            char_start: cs,
            char_len: text.len() as u32,
            style: RunStyle::marker(),
            link: None,
            image: None,
            footnote_mark: None,
        }
    }

    /// A minimal body ParaMeta with the given logical text + runs.
    fn body_para(id: u32, logical: &str, runs: Vec<RunMeta>) -> ParaMeta {
        ParaMeta {
            para_id: id,
            logical_text: logical.to_string(),
            location: ParaLocation::Body { sec: 0, block: 0 },
            alignment: STJc::Start,
            base_rtl: false,
            numbering: None,
            runs,
            anchored_images: Vec::new(),
        }
    }

    fn no_tables() -> StdHashMap<(usize, usize), TableMeta> {
        StdHashMap::new()
    }

    fn as_para(b: &Block) -> &FragPara {
        match b {
            Block::Para(p) => p,
            Block::Table(_) => panic!("expected a Para block, got a Table"),
        }
    }

    fn as_table(b: &Block) -> &FragTable {
        match b {
            Block::Table(t) => t,
            Block::Para(_) => panic!("expected a Table block, got a Para"),
        }
    }

    // A full single-para extraction: runs cover the logical text exactly.
    #[test]
    fn extract_single_body_para_full() {
        let p = body_para(0, "Hello World", vec![text_run_meta(0, "Hello World")]);
        let frag = extract_range_impl(&[p], &no_tables(), TextPos::new(0, 0), TextPos::new(0, 11));
        assert_eq!(frag.blocks.len(), 1);
        assert_eq!(para_text(as_para(&frag.blocks[0])), "Hello World");
    }

    // A sub-paragraph window slices runs at the byte boundaries.
    #[test]
    fn extract_partial_first_last_para() {
        let p0 = body_para(0, "Hello", vec![text_run_meta(0, "Hello")]);
        let p1 = body_para(1, "World", vec![text_run_meta(0, "World")]);
        let frag = extract_range_impl(
            &[p0, p1],
            &no_tables(),
            TextPos::new(0, 2),
            TextPos::new(1, 3),
        );
        assert_eq!(frag.blocks.len(), 2);
        assert_eq!(para_text(as_para(&frag.blocks[0])), "llo"); // "He|llo"
        assert_eq!(para_text(as_para(&frag.blocks[1])), "Wor"); // "Wor|ld"
    }

    // The contract fixture: EVERY pinned FragRun field maps from
    // RunStyle to the exact wire value/string. Synthetic RunStyle pins the
    // enum→string mappings (underline/vertAlign) unambiguously.
    #[test]
    fn extract_style_rich_roundtrip() {
        let style = RunStyle {
            bold: true,
            italic: true,
            underline: Underline::Double,
            strikethrough: true,
            double_strike: true,
            vert_align: VertAlign::Superscript,
            family: "Georgia".to_string(),
            font_pt: 18.0,
            color: "#FF0000".to_string(),
            highlight: Some("yellow".to_string()),
            run_shading: Some("#EEEEEE".to_string()),
            ..RunStyle::marker()
        };
        let run = RunMeta {
            char_start: 0,
            char_len: 4,
            style,
            link: Some("url:https://example.com".to_string()),
            image: None,
            footnote_mark: None,
        };
        let p = body_para(0, "Word", vec![run]);
        let frag = extract_range_impl(&[p], &no_tables(), TextPos::new(0, 0), TextPos::new(0, 4));
        let fp = as_para(&frag.blocks[0]);
        let r = &fp.runs[0];
        assert_eq!(r.text, "Word");
        assert!(r.bold && r.italic && r.strike && r.double_strike);
        assert_eq!(r.underline, "double");
        assert_eq!(r.vert_align, "superscript");
        assert_eq!(r.font_family, "Georgia");
        assert_eq!(r.font_size_pt, 18.0);
        assert_eq!(r.color, "#FF0000");
        assert_eq!(r.highlight.as_deref(), Some("yellow"));
        assert_eq!(r.run_shading.as_deref(), Some("#EEEEEE"));
        assert_eq!(r.link.as_deref(), Some("url:https://example.com"));
        assert_eq!(fp.dir, "ltr");
        assert_eq!(fp.alignment, "start");
    }

    // RTL LOGICAL-order proof: build_para_runs walks collect_items emit
    // order (LOGICAL, pre-bidi-reorder), so concat(runs) == logical_text and the
    // Hebrew segment lands AFTER "abc" — never the visual/paged permutation.
    #[test]
    fn extract_rtl_logical_order() {
        use crate::engine::fonts::{FontCache, ThemeFonts};
        use crate::wml::linebreak::collect_items;
        use ooxml_wml::ext::StyleContext;
        use ooxml_wml::types::{Paragraph, ParagraphContent, Run, RunContent, Text};

        let run = Run {
            run_content: vec![RunContent::T(Box::new(Text {
                text: Some("abcשלום".to_string()),
                ..Default::default()
            }))],
            ..Default::default()
        };
        let para = Paragraph {
            paragraph_content: vec![ParagraphContent::R(Box::new(run))],
            ..Default::default()
        };
        let (items, _, _, _, _) =
            collect_items(&FontCache::new(), &para, &StyleContext::default(), &ThemeFonts::empty(), &[])
                .expect("collect_items");
        let logical = logical_text_of(&items);
        let runs = build_para_runs(&items);
        let mut meta = body_para(0, &logical, runs);
        meta.base_rtl = true;
        let len = logical.len() as u32;
        let frag = extract_range_impl(&[meta], &no_tables(), TextPos::new(0, 0), TextPos::new(0, len));
        let fp = as_para(&frag.blocks[0]);
        // Concatenated runs equal the LOGICAL text (not a visual reorder).
        assert_eq!(para_text(fp), logical);
        // "abc" precedes the Hebrew in run order (logical, not visual).
        assert!(para_text(fp).starts_with("abc"));
        assert!(logical.contains("שלום"));
        assert_eq!(fp.dir, "rtl");
    }

    // Cross-page join: extraction reads ParaMeta, not pages, so a paragraph
    // paginated across a break is ONE Para with its full logical text.
    #[test]
    fn extract_cross_page_join() {
        let long = "The quick brown fox jumps over the lazy dog again and again";
        let p = body_para(0, long, vec![text_run_meta(0, long)]);
        let len = long.len() as u32;
        let frag = extract_range_impl(&[p], &no_tables(), TextPos::new(0, 0), TextPos::new(0, len));
        assert_eq!(frag.blocks.len(), 1);
        assert_eq!(para_text(as_para(&frag.blocks[0])), long);
    }

    // An empty paragraph extracts as a Para with NO runs.
    #[test]
    fn extract_empty_para_r8() {
        let p = body_para(0, "", vec![]);
        let frag = extract_range_impl(&[p], &no_tables(), TextPos::new(0, 0), TextPos::new(0, 0));
        assert_eq!(frag.blocks.len(), 1);
        assert!(as_para(&frag.blocks[0]).runs.is_empty());
    }

    // The list marker is emitted only when the window includes byte 0.
    #[test]
    fn extract_numbered_list_r34() {
        let mut p = body_para(0, "First item", vec![text_run_meta(0, "First item")]);
        p.numbering = Some(NumInfo {
            num_id: 3,
            ilvl: 0,
            count: 7,
            fmt: STNumberFormat::LowerRoman,
            marker_text: "vii.".to_string(),
        });
        let len = 10u32;
        // Whole para (lo == 0) → list present.
        let f_full = extract_range_impl(&[p.clone()], &no_tables(), TextPos::new(0, 0), TextPos::new(0, len));
        let list = as_para(&f_full.blocks[0]).list.as_ref().expect("list present at lo==0");
        assert!(list.ordered);
        assert_eq!(list.num_id, 3);
        assert_eq!(list.level, 0);
        assert_eq!(list.value, 7);
        assert_eq!(list.fmt, "lowerRoman");
        assert_eq!(list.marker, "vii.");
        // Mid-paragraph (lo > 0) → no marker (Word omits it).
        let f_mid = extract_range_impl(&[p], &no_tables(), TextPos::new(0, 2), TextPos::new(0, len));
        assert!(as_para(&f_mid.blocks[0]).list.is_none());
    }

    /// Build a 2×2 TableMeta with distinct fill/borders and the given v-merge.
    fn table_meta_2x2(v_merged_11: bool) -> TableMeta {
        let border = || {
            Some(FragBorder {
                style: "single".to_string(),
                color: "#000000".to_string(),
                width_px: 1.0,
            })
        };
        let cell = |fill: Option<&str>, v: bool, rowspan: u32| CellMeta {
            rowspan,
            colspan: 1,
            fill: fill.map(str::to_string),
            borders: [border(), border(), border(), border()],
            v_merged_away: v,
        };
        TableMeta {
            rows: vec![
                CellMetaRow {
                    cells: vec![cell(Some("#FF0000"), false, 1), cell(None, false, 1)],
                },
                CellMetaRow {
                    cells: vec![cell(None, false, 1), cell(None, v_merged_11, 1)],
                },
            ],
        }
    }

    fn cell_para(id: u32, logical: &str, block: usize, row: usize, cell: usize) -> ParaMeta {
        ParaMeta {
            para_id: id,
            logical_text: logical.to_string(),
            location: ParaLocation::TableCell { sec: 0, block, row, cell },
            alignment: STJc::Start,
            base_rtl: false,
            numbering: None,
            runs: vec![text_run_meta(0, logical)],
            anchored_images: Vec::new(),
        }
    }

    // A table range emits the FULL grid with spans/fill/borders,
    // covered paras populated. Cell content routes by (sec,block,row,cell).
    #[test]
    fn extract_table_r13() {
        let paras = vec![
            cell_para(0, "A", 0, 0, 0),
            cell_para(1, "B", 0, 0, 1),
            cell_para(2, "C", 0, 1, 0),
            cell_para(3, "D", 0, 1, 1),
        ];
        let mut tables = no_tables();
        tables.insert((0, 0), table_meta_2x2(false));
        let frag = extract_range_impl(&paras, &tables, TextPos::new(0, 0), TextPos::new(3, 1));
        assert_eq!(frag.blocks.len(), 1);
        let t = as_table(&frag.blocks[0]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0].cells.len(), 2);
        assert_eq!(para_text(as_para(&t.rows[0].cells[0].blocks[0])), "A");
        assert_eq!(para_text(as_para(&t.rows[0].cells[1].blocks[0])), "B");
        assert_eq!(para_text(as_para(&t.rows[1].cells[0].blocks[0])), "C");
        assert_eq!(para_text(as_para(&t.rows[1].cells[1].blocks[0])), "D");
        assert_eq!(t.rows[0].cells[0].fill.as_deref(), Some("#FF0000"));
        assert!(t.rows[0].cells[0].borders[0].is_some());

        // A PARTIAL selection (first two cells) still emits the full grid;
        // the uncovered cells are present-but-empty.
        let partial = extract_range_impl(&paras, &tables, TextPos::new(0, 0), TextPos::new(1, 1));
        let pt = as_table(&partial.blocks[0]);
        assert_eq!(pt.rows.len(), 2);
        assert_eq!(para_text(as_para(&pt.rows[0].cells[0].blocks[0])), "A");
        assert!(pt.rows[1].cells[0].blocks.is_empty()); // uncovered → empty
    }

    // A vMergedAway continuation cell is present-but-empty.
    #[test]
    fn extract_table_merged_away() {
        let paras = vec![
            cell_para(0, "A", 0, 0, 0),
            cell_para(1, "B", 0, 0, 1),
            cell_para(2, "C", 0, 1, 0),
            // cell (1,1) is v_merged_away → no paragraph captured there.
        ];
        let mut tables = no_tables();
        tables.insert((0, 0), table_meta_2x2(true));
        let frag = extract_range_impl(&paras, &tables, TextPos::new(0, 0), TextPos::new(2, 1));
        let t = as_table(&frag.blocks[0]);
        let merged = &t.rows[1].cells[1];
        assert!(merged.v_merged_away);
        assert!(merged.blocks.is_empty());
    }

    // A byte landing mid-multibyte-char snaps to a char boundary, no
    // panic. "áé" = 4 bytes (2 per char); start.byte=1 is inside 'á'.
    #[test]
    fn extract_cluster_snap_r31() {
        let s = "áé"; // 4 bytes
        assert_eq!(s.len(), 4);
        let p = body_para(0, s, vec![text_run_meta(0, s)]);
        // start.byte=1 (mid 'á') snaps down to 0; end.byte=3 (mid 'é') snaps to 2.
        let frag = extract_range_impl(&[p], &no_tables(), TextPos::new(0, 1), TextPos::new(0, 3));
        let fp = as_para(&frag.blocks[0]);
        assert_eq!(para_text(fp), "á"); // [0,2) after snapping
    }

    // A footnote ref becomes a ZERO-WIDTH run carrying its display number.
    // Driven through the REAL collect_items path (the sentinel item carries "1").
    #[test]
    fn extract_footnote_mark() {
        use crate::engine::fonts::{FontCache, ThemeFonts};
        use crate::wml::linebreak::collect_items;
        use ooxml_wml::ext::StyleContext;
        use ooxml_wml::types::{
            FootnoteEndnoteRef, Paragraph, ParagraphContent, Run, RunContent, Text,
        };

        let text_run = Run {
            run_content: vec![RunContent::T(Box::new(Text {
                text: Some("Hi".to_string()),
                ..Default::default()
            }))],
            ..Default::default()
        };
        let fn_run = Run {
            run_content: vec![RunContent::FootnoteReference(Box::new(FootnoteEndnoteRef {
                custom_mark_follows: None,
                id: 1,
            }))],
            ..Default::default()
        };
        let para = Paragraph {
            paragraph_content: vec![
                ParagraphContent::R(Box::new(text_run)),
                ParagraphContent::R(Box::new(fn_run)),
            ],
            ..Default::default()
        };
        let (items, _, _, _, _) =
            collect_items(&FontCache::new(), &para, &StyleContext::default(), &ThemeFonts::empty(), &[])
                .expect("collect_items");
        let logical = logical_text_of(&items);
        let runs = build_para_runs(&items);
        let meta = body_para(0, &logical, runs);
        let len = logical.len() as u32;
        let frag = extract_range_impl(&[meta], &no_tables(), TextPos::new(0, 0), TextPos::new(0, len));
        let fp = as_para(&frag.blocks[0]);
        let fn_run = fp
            .runs
            .iter()
            .find(|r| r.footnote_mark.is_some())
            .expect("a footnote run");
        assert_eq!(fn_run.footnote_mark.as_deref(), Some("1"));
        assert_eq!(fn_run.text, ""); // zero-width
        // The visible text is unaffected.
        assert!(para_text(fp).contains("Hi"));
    }

    // Run text comes from the CAPTURED logical_text (immune to the
    // later PAGE/NUMPAGES paged-item mutation, which extract never touches).
    #[test]
    fn extract_page_field_uses_captured_text() {
        // "Page 1 of 1": the field results ("1") were captured at collect time;
        // extract slices logical_text, never a mutated paged item.text.
        let logical = "Page 1 of 1";
        let p = body_para(0, logical, vec![text_run_meta(0, logical)]);
        let len = logical.len() as u32;
        let frag = extract_range_impl(&[p], &no_tables(), TextPos::new(0, 0), TextPos::new(0, len));
        assert_eq!(para_text(as_para(&frag.blocks[0])), logical);
    }

    // SmallCaps text is already UPPERCASE in logical_text; the run
    // carries the verbatim transform, no caps flag on FragRun (documented
    // divergence). Driven through collect_items (which applies to_uppercase).
    #[test]
    fn extract_caps_r30() {
        use crate::engine::fonts::{FontCache, ThemeFonts};
        use crate::wml::linebreak::collect_items;
        use ooxml_wml::ext::StyleContext;
        use ooxml_wml::types::{
            OnOffElement, Paragraph, ParagraphContent, Run, RunContent, RunProperties, Text,
        };

        let mut caps_run = Run {
            run_content: vec![RunContent::T(Box::new(Text {
                text: Some("hello".to_string()),
                ..Default::default()
            }))],
            ..Default::default()
        };
        caps_run.r_pr = Some(Box::new(RunProperties {
            caps: Some(Box::new(OnOffElement {
                value: Some("true".to_string()),
                ..Default::default()
            })),
            ..Default::default()
        }));
        let para = Paragraph {
            paragraph_content: vec![ParagraphContent::R(Box::new(caps_run))],
            ..Default::default()
        };
        let (items, _, _, _, _) =
            collect_items(&FontCache::new(), &para, &StyleContext::default(), &ThemeFonts::empty(), &[])
                .expect("collect_items");
        let logical = logical_text_of(&items);
        let runs = build_para_runs(&items);
        let meta = body_para(0, &logical, runs);
        let len = logical.len() as u32;
        let frag = extract_range_impl(&[meta], &no_tables(), TextPos::new(0, 0), TextPos::new(0, len));
        assert_eq!(para_text(as_para(&frag.blocks[0])), "HELLO");
    }

    // build_para_runs stamps the item's link onto its run (link capture).
    #[test]
    fn build_para_runs_captures_link() {
        let mut it = mk_item("link", 40.0, &[(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0)], false, 0);
        it.link = Some("url:https://a.example".to_string());
        let runs = build_para_runs(&[it]);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].link.as_deref(), Some("url:https://a.example"));
    }

    // Normalization: a reversed (end < start) range extracts the same as forward.
    #[test]
    fn extract_normalizes_reversed_range() {
        let p = body_para(0, "abcdef", vec![text_run_meta(0, "abcdef")]);
        let fwd = extract_range_impl(&[p.clone()], &no_tables(), TextPos::new(0, 1), TextPos::new(0, 4));
        let rev = extract_range_impl(&[p], &no_tables(), TextPos::new(0, 4), TextPos::new(0, 1));
        assert_eq!(fwd, rev);
        assert_eq!(para_text(as_para(&fwd.blocks[0])), "bcd");
    }

    /// A RunMeta for a zero-width inline OBJECT (image or chart) at byte `cs`.
    fn image_run_meta(cs: u32, rel: &str) -> RunMeta {
        RunMeta {
            char_start: cs,
            char_len: 0,
            style: RunStyle::marker(),
            link: None,
            image: Some(ImageRef { rel_id: rel.to_string(), w_px: 430.0, h_px: 216.0 }),
            footnote_mark: None,
        }
    }

    // The ride-along regression (reported on file-sample_1MB): Word emits a
    // chart/image in a paragraph of its OWN with no text. That paragraph's byte
    // window is [0,0], and the old `lo < hi` guard (meant for the collapsed
    // caret) dropped the object run — a text-range copy sweeping the chart
    // paragraph silently lost the chart. The guard must key on the OVERALL
    // selection being non-collapsed, not the per-paragraph window.
    #[test]
    fn extract_emits_inline_object_in_object_only_paragraph() {
        let paras = vec![
            body_para(0, "above", vec![text_run_meta(0, "above")]),
            body_para(1, "", vec![image_run_meta(0, "chart::rId3")]), // chart alone
            body_para(2, "below", vec![text_run_meta(0, "below")]),
        ];
        let frag = extract_range_impl(&paras, &no_tables(), TextPos::new(0, 0), TextPos::new(2, 5));
        assert_eq!(frag.blocks.len(), 3);
        let mid = as_para(&frag.blocks[1]);
        assert_eq!(
            mid.runs.iter().filter_map(|r| r.image.as_ref()).map(|i| i.rel_id.as_str()).collect::<Vec<_>>(),
            vec!["chart::rId3"],
            "the object-only paragraph must emit its inline object run"
        );
        // The collapsed caret still emits NOTHING (the guard's original purpose).
        let caret = extract_range_impl(
            &[body_para(1, "", vec![image_run_meta(0, "chart::rId3")])],
            &no_tables(),
            TextPos::new(1, 0),
            TextPos::new(1, 0),
        );
        assert!(
            caret.blocks.iter().all(|b| matches!(b, Block::Para(p) if p.runs.is_empty())),
            "a collapsed caret must not emit the object run"
        );
    }

    // A text-BEARING paragraph's inline object emits when the window covers its
    // byte position, and is EXCLUDED when the selection starts past it.
    #[test]
    fn extract_inline_object_respects_byte_window_in_text_paragraph() {
        let p = body_para(
            0,
            "before  after",
            vec![text_run_meta(0, "before "), image_run_meta(7, "chart::rId3"), text_run_meta(7, " after")],
        );
        let full = extract_range_impl(&[p.clone()], &no_tables(), TextPos::new(0, 0), TextPos::new(0, 13));
        assert!(as_para(&full.blocks[0]).runs.iter().any(|r| r.image.is_some()), "full window includes the object");
        let past = extract_range_impl(&[p], &no_tables(), TextPos::new(0, 8), TextPos::new(0, 13));
        assert!(
            as_para(&past.blocks[0]).runs.iter().all(|r| r.image.is_none()),
            "a selection starting past the object excludes it"
        );
    }

    // ── Object hit-test + outline pass ───────────────────────────────────────
    use crate::wml::linebreak::ImageItem;

    /// A body-line item carrying an inline image. `width` is the box width
    /// (incl. distL/distR); `height_px` is the intrinsic bitmap height. No
    /// shaped run (an image item is unshaped) and a NON-sentinel char_start so it
    /// is a normal flow item.
    fn mk_image_item(rel_id: &str, width: f64, height_px: f64) -> Item {
        let mut it = mk_item("", width, &[], false, 0);
        it.char_len = 0;
        it.image = Some(ImageItem {
            rel_id: rel_id.to_string(),
            height_px,
            dist_l_px: 0.0,
            dist_t_px: 0.0,
            dist_r_px: 0.0,
            dist_b_px: 0.0,
        });
        it
    }

    /// A page with one anchored image at (x,y,w,h).
    fn anchored_page(rel_id: &str, x: f64, y: f64, w: f64, h: f64, behind: bool) -> PageLayout {
        let mut pg = PageLayout::new();
        pg.anchored_images.push(AnchoredImageLayout {
            rel_id: rel_id.to_string(),
            x,
            y,
            width_px: w,
            height_px: h,
            behind_text: behind,
            anchor_para: None,
        });
        pg
    }

    fn all_images(_: &str) -> bool {
        true
    }

    /// The default chart classifier for image-only tests.
    fn no_charts(_: &str) -> bool {
        false
    }

    #[test]
    fn object_hit_over_anchored_image() {
        let page = anchored_page("rId1", 50.0, 50.0, 100.0, 80.0, false);
        let hit = hit_test_object_impl(&page, 60.0, 60.0, &all_images, &no_charts).expect("hit");
        assert_eq!(hit.rel_id, "rId1");
        assert_eq!(hit.kind, "anchoredImage");
        assert!((hit.rect.x - 50.0).abs() < 1e-9 && (hit.rect.w - 100.0).abs() < 1e-9);
        // A point outside the image box misses.
        assert!(hit_test_object_impl(&page, 0.0, 0.0, &all_images, &no_charts).is_none());
    }

    #[test]
    fn object_first_z_order_over_text() {
        // A body text line AND an in-front anchored image overlapping it: a point
        // inside the image returns the OBJECT (object-first z-order).
        let mut page = anchored_page("rId1", 0.0, 0.0, 60.0, 40.0, false);
        let abcd = mk_item("abcd", 40.0, &[(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0)], false, 0);
        page.push(PageElement::Line(line_with(vec![abcd], 0)));
        let hit = hit_test_object_impl(&page, 20.0, 20.0, &all_images, &no_charts).expect("hit");
        assert_eq!(hit.kind, "anchoredImage");
        assert_eq!(hit.rel_id, "rId1");
    }

    #[test]
    fn inline_image_hit() {
        // A line with one inline-image item (box 0..50, baseline 20, height 16).
        // Painted box (render.rs geometry): x=0, y=baseline-0-16=4, w=50, h=16.
        let img = mk_image_item("rIdImg", 50.0, 16.0);
        let page = page_with(line_with(vec![img], 0));
        let hit = hit_test_object_impl(&page, 25.0, 10.0, &all_images, &no_charts).expect("hit");
        assert_eq!(hit.kind, "inlineImage");
        assert_eq!(hit.rel_id, "rIdImg");
        assert!((hit.rect.x - 0.0).abs() < 1e-9);
        assert!((hit.rect.y - 4.0).abs() < 1e-9);
        assert!((hit.rect.w - 50.0).abs() < 1e-9);
        assert!((hit.rect.h - 16.0).abs() < 1e-9);
        // Above the painted box (y=2 < 4) misses.
        assert!(hit_test_object_impl(&page, 25.0, 2.0, &all_images, &no_charts).is_none());
    }

    #[test]
    fn chart_excluded_by_filter() {
        // The same anchored page, but `is_image` reports rId1 is NOT an image and
        // the chart classifier does not admit it either → no object hit. (An
        // anchored chart never renders today — the anchored hit-test keeps its
        // arms image-only.)
        let page = anchored_page("rId1", 50.0, 50.0, 100.0, 80.0, false);
        let not_image = |rel: &str| rel != "rId1";
        assert!(hit_test_object_impl(&page, 60.0, 60.0, &not_image, &no_charts).is_none());
    }

    // An INLINE chart item (the `chart::<rId>` key on the same
    // `Item.image` seam) hits with the DISTINCT kind "chart" when the chart
    // classifier admits it, while `is_image` keeps excluding it.
    #[test]
    fn inline_chart_hit_has_chart_kind() {
        let chart = mk_image_item("chart::rId3", 430.0, 216.0);
        let page = page_with(line_with(vec![chart], 0));
        let is_image = |rel: &str| !rel.starts_with("chart::"); // the lib.rs shape
        let is_chart = |rel: &str| rel.starts_with("chart::");
        let hit =
            hit_test_object_impl(&page, 200.0, 10.0, &is_image, &is_chart).expect("chart hit");
        assert_eq!(hit.kind, "chart");
        assert_eq!(hit.rel_id, "chart::rId3");
        assert!((hit.rect.w - 430.0).abs() < 1e-9 && (hit.rect.h - 216.0).abs() < 1e-9);
        // With the chart classifier OFF, the same point misses (images-only behavior).
        assert!(hit_test_object_impl(&page, 200.0, 10.0, &is_image, &no_charts).is_none());
    }

    // ── The highlight-hug fix ────────────────────────────────────────────────
    // Real inline objects (images AND charts) are SENTINEL items. Pre-fix, a
    // covered object item pushed its x-interval into the LINE-band merge, so a
    // tall object line painted one object-height band across the text too (the
    // "offset square" artifact). Post-fix the object emits its OBJECT BOX
    // (`inline_image_rect`) as its own rect and text keeps the line band.

    /// A SENTINEL inline-object item (the real `collect_items` shape).
    fn mk_sentinel_object_item(rel_id: &str, width: f64, height_px: f64) -> Item {
        let mut it = mk_image_item(rel_id, width, height_px);
        it.char_start = CHAR_START_SENTINEL;
        it
    }

    // DELIBERATE geometry change (documented): before the hug fix this line
    // yielded ONE merged rect (0..100 at the line band, h=20). Now it yields the
    // text rect (line band) + the object's painted box, disjoint by construction.
    #[test]
    fn selection_hug_uses_object_box_for_inline_object() {
        let abcd = mk_item("abcd", 40.0, &[(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0)], false, 0);
        let obj = mk_sentinel_object_item("chart::rId3", 60.0, 216.0);
        let page = page_with(line_with(vec![abcd, obj], 0)); // baseline 20, lh 20
        let rects = selection_rects_impl(&page, TextPos::new(0, 0), TextPos::new(0, 4));
        assert_eq!(rects.len(), 2, "text band + object box, never merged: {rects:?}");
        // The object box: x=40 (after abcd), y=baseline−216=−196, w=60, h=216.
        let ob = rects
            .iter()
            .find(|r| (r.h - 216.0).abs() < 1e-9)
            .expect("object-box rect present");
        assert!((ob.x - 40.0).abs() < 1e-9 && (ob.y - -196.0).abs() < 1e-9 && (ob.w - 60.0).abs() < 1e-9);
        // The text rect keeps the LINE band (top = 20 − 20·0.85 = 3, h = 20).
        let tr = rects
            .iter()
            .find(|r| (r.h - 20.0).abs() < 1e-9)
            .expect("text line-band rect present");
        assert!((tr.x - 0.0).abs() < 1e-9 && (tr.y - 3.0).abs() < 1e-9 && (tr.w - 40.0).abs() < 1e-9);
    }

    // The sentinel coverage gate is unchanged: a range NOT including byte 0 paints no
    // object box (sentinels paint only when the covered window includes para
    // byte 0) — and a pure-TEXT line stays byte-identical (pinned above).
    #[test]
    fn selection_hug_respects_sentinel_coverage_gate() {
        let abcd = mk_item("abcd", 40.0, &[(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0)], false, 0);
        let obj = mk_sentinel_object_item("chart::rId3", 60.0, 216.0);
        let page = page_with(line_with(vec![abcd, obj], 0));
        let rects = selection_rects_impl(&page, TextPos::new(0, 1), TextPos::new(0, 4));
        assert_eq!(rects.len(), 1, "mid-para range: text band only: {rects:?}");
        assert!((rects[0].h - 20.0).abs() < 1e-9, "no object-height rect");
    }

    // The numbering-marker sentinel (NO image) keeps the pre-fix line-band merge.
    #[test]
    fn selection_marker_sentinel_keeps_line_band() {
        let marker = mk_item("1.", 20.0, &[(0, 10.0), (1, 10.0)], false, CHAR_START_SENTINEL);
        let text = mk_item("Item", 40.0, &[(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0)], false, 0);
        let page = page_with(line_with(vec![marker, text], 0));
        let rects = selection_rects_impl(&page, TextPos::new(0, 0), TextPos::new(0, 4));
        assert_eq!(rects.len(), 1, "marker still merges into the line band");
        assert!((rects[0].x - 0.0).abs() < 1e-6 && (rects[0].w - 60.0).abs() < 1e-6);
    }

    // The object-outline pass matches by KEY, so a selected chart
    // key outlines its painted box exactly like an image.
    #[test]
    fn outline_strokes_selected_chart_key() {
        let chart = mk_image_item("chart::rId3", 430.0, 216.0);
        let page = page_with(line_with(vec![chart], 0));
        let rec = RecordingPainter::new();
        paint_object_outline_pass(&rec, &page, Some("chart::rId3"), None);
        let cmds = rec.take();
        // Painted box (render.rs geometry, dist 0): x=0, y=baseline(20)−216=−196.
        assert_eq!(
            cmds,
            vec![
                DrawCmd::Save,
                DrawCmd::SetStrokeStyle(OBJECT_OUTLINE_COLOR.into()),
                DrawCmd::SetLineWidth(OBJECT_OUTLINE_WIDTH_PX),
                DrawCmd::StrokeRect { x: 0.0, y: -196.0, w: 430.0, h: 216.0 },
                DrawCmd::Restore,
            ]
        );
    }

    #[test]
    fn outline_paints_nothing_when_none() {
        let page = anchored_page("rId1", 50.0, 50.0, 100.0, 80.0, false);
        let rec = RecordingPainter::new();
        paint_object_outline_pass(&rec, &page, None, None);
        assert!(rec.cmds().is_empty(), "None selection must emit zero commands: {:?}", *rec.cmds());
    }

    #[test]
    fn outline_records_stroke_when_selected() {
        let page = anchored_page("rId1", 50.0, 50.0, 100.0, 80.0, false);
        let rec = RecordingPainter::new();
        paint_object_outline_pass(&rec, &page, Some("rId1"), None);
        let cmds = rec.take();
        assert_eq!(
            cmds,
            vec![
                DrawCmd::Save,
                DrawCmd::SetStrokeStyle(OBJECT_OUTLINE_COLOR.into()),
                DrawCmd::SetLineWidth(OBJECT_OUTLINE_WIDTH_PX),
                DrawCmd::StrokeRect { x: 50.0, y: 50.0, w: 100.0, h: 80.0 },
                DrawCmd::Restore,
            ]
        );
    }

    #[test]
    fn outline_unknown_relid_paints_nothing() {
        let page = anchored_page("rId1", 50.0, 50.0, 100.0, 80.0, false);
        let rec = RecordingPainter::new();
        paint_object_outline_pass(&rec, &page, Some("nope"), None);
        assert!(rec.cmds().is_empty(), "unknown relId must emit zero commands");
    }

    // ── Anchored (floating) images ride along on text copy ──────────────────
    use crate::wml::drawing::{WrapInfo, WrapMode, WrapTextSide};
    use crate::wml::float::{AnchorHRelFrom, AnchorPos, AnchorVRelFrom};
    use crate::wml::linebreak::AnchoredImageInfo;

    fn anchored_info(rel: &str, w: f64, h: f64) -> AnchoredImageInfo {
        AnchoredImageInfo {
            rel_id: rel.to_string(),
            width_px: w,
            height_px: h,
            pos_h: AnchorPos::Offset(0.0),
            pos_v: AnchorPos::Offset(0.0),
            h_relative_from: AnchorHRelFrom::Column,
            v_relative_from: AnchorVRelFrom::Paragraph,
            behind_text: false,
            wrap: WrapInfo {
                mode: WrapMode::Square,
                wrap_text: WrapTextSide::BothSides,
                dist_t: 0.0,
                dist_b: 0.0,
                dist_l: 0.0,
                dist_r: 0.0,
            },
        }
    }

    // The pinned chart gate: a `chart::`-namespaced rel never becomes an
    // ImageRef; a plain blip rel maps with its DISPLAY-px dims.
    #[test]
    fn anchored_image_refs_excludes_charts() {
        let infos = vec![anchored_info("rId2", 120.0, 80.0), anchored_info("chart::rId7", 400.0, 300.0)];
        let refs = anchored_image_refs(&infos);
        assert_eq!(refs.len(), 1, "chart rel must be gated out");
        assert_eq!(refs[0], ImageRef { rel_id: "rId2".into(), w_px: 120.0, h_px: 80.0 });
    }

    /// `body_para` + recorded anchored images (the capture-site shape).
    fn body_para_with_anchor(id: u32, logical: &str, imgs: Vec<ImageRef>) -> ParaMeta {
        let mut p = body_para(id, logical, vec![text_run_meta(0, logical)]);
        p.anchored_images = imgs;
        p
    }

    // In-range anchor para ⇒ one appended image run (empty text, dims carried).
    // Para-LEVEL inclusion (v1): a PARTIAL byte window over the anchor para
    // still includes the image (Word is char-position-precise; documented
    // deviation).
    #[test]
    fn extract_emits_anchored_image_in_range() {
        let img = ImageRef { rel_id: "rId9".into(), w_px: 200.0, h_px: 150.0 };
        let p = body_para_with_anchor(0, "abcdef", vec![img.clone()]);
        // Full para.
        let frag = extract_range_impl(&[p.clone()], &no_tables(), TextPos::new(0, 0), TextPos::new(0, 6));
        let runs = &as_para(&frag.blocks[0]).runs;
        assert_eq!(runs.last().unwrap().image.as_ref(), Some(&img));
        assert_eq!(runs.last().unwrap().text, "", "anchored image run is zero-width");
        // Partial window over the anchor para (para-level inclusion).
        let part = extract_range_impl(&[p], &no_tables(), TextPos::new(0, 2), TextPos::new(0, 4));
        let pruns = &as_para(&part.blocks[0]).runs;
        assert_eq!(para_text(as_para(&part.blocks[0])), "cd");
        assert_eq!(pruns.last().unwrap().image.as_ref(), Some(&img));
    }

    // Outside range ⇒ NOT emitted; collapsed caret ⇒ NOT emitted.
    #[test]
    fn extract_skips_anchored_image_outside_range_and_collapsed() {
        let img = ImageRef { rel_id: "rId9".into(), w_px: 200.0, h_px: 150.0 };
        let p0 = body_para(0, "first", vec![text_run_meta(0, "first")]);
        let p1 = body_para_with_anchor(1, "second", vec![img.clone()]);
        // Range covers ONLY para 0 → the para-1 anchor is not swept in.
        let frag = extract_range_impl(
            &[p0, p1.clone()],
            &no_tables(),
            TextPos::new(0, 0),
            TextPos::new(0, 5),
        );
        assert_eq!(frag.blocks.len(), 1);
        assert!(as_para(&frag.blocks[0]).runs.iter().all(|r| r.image.is_none()));
        // Collapsed caret ON the anchor para → nothing (guard mirrors the tint).
        let anchored0 = body_para_with_anchor(0, "second", vec![img]);
        let caret = extract_range_impl(&[anchored0], &no_tables(), TextPos::new(0, 2), TextPos::new(0, 2));
        assert!(
            as_para(&caret.blocks[0]).runs.iter().all(|r| r.image.is_none()),
            "collapsed caret must not emit anchored images"
        );
    }

    // A rel anchored in TWO covered paragraphs emits twice (matches Word
    // pasting two instances).
    #[test]
    fn extract_emits_anchored_image_per_occurrence() {
        let img = ImageRef { rel_id: "rId9".into(), w_px: 10.0, h_px: 10.0 };
        let p0 = body_para_with_anchor(0, "a", vec![img.clone()]);
        let p1 = body_para_with_anchor(1, "b", vec![img.clone()]);
        let frag = extract_range_impl(&[p0, p1], &no_tables(), TextPos::new(0, 0), TextPos::new(1, 1));
        let count: usize = frag
            .blocks
            .iter()
            .map(|b| as_para(b).runs.iter().filter(|r| r.image.is_some()).count())
            .sum();
        assert_eq!(count, 2, "one emission per anchored occurrence");
    }

    // ── the selection tint over swept-in floats ──────────────────────────────

    /// `ltr_page` + one anchored image linked (or not) to a paragraph.
    fn ltr_page_with_float(anchor_para: Option<u32>) -> PageLayout {
        let mut page = ltr_page();
        page.anchored_images.push(AnchoredImageLayout {
            rel_id: "rId9".into(),
            x: 200.0,
            y: 300.0,
            width_px: 100.0,
            height_px: 80.0,
            behind_text: false,
            anchor_para,
        });
        page
    }

    // A selection covering the anchor para tints the float (one extra FillRect
    // at the image rect); no selection ⇒ ZERO commands (byte-identity).
    #[test]
    fn selection_tints_anchored_image_of_covered_para() {
        let page = ltr_page_with_float(Some(0));
        let rec = RecordingPainter::new();
        paint_anchored_selection_tint(
            &rec,
            &page,
            Some((TextPos::new(0, 0), TextPos::new(0, 4))),
            None,
        );
        let fills: Vec<_> = rec
            .cmds()
            .iter()
            .filter_map(|c| match c {
                DrawCmd::FillRect { x, y, w, h } => Some((*x, *y, *w, *h)),
                _ => None,
            })
            .collect();
        assert!(
            fills.contains(&(200.0, 300.0, 100.0, 80.0)),
            "the swept-in float must be tinted: {fills:?}"
        );
        // The TEXT pass must NOT tint the float (z-order: its fill would be
        // buried under the in-front image draw — the split's whole point).
        let text_pass = RecordingPainter::new();
        paint_selection_pass(
            &text_pass,
            &page,
            Some((TextPos::new(0, 0), TextPos::new(0, 4))),
            None,
        );
        assert!(
            !text_pass
                .cmds()
                .iter()
                .any(|c| matches!(c, DrawCmd::FillRect { x, .. } if *x == 200.0)),
            "the text pass must not paint the float tint (it would be overdrawn)"
        );
        // Byte-identity: no selection ⇒ zero commands even with the linkage set.
        let none = RecordingPainter::new();
        paint_anchored_selection_tint(&none, &page, None, None);
        assert!(none.cmds().is_empty(), "no selection must stay byte-identical");
    }

    // An anchor para OUTSIDE the selection (or an unlinked None carrier) does
    // not tint; a collapsed caret does not tint.
    #[test]
    fn selection_tint_respects_anchor_para_and_collapse() {
        let sel = Some((TextPos::new(0, 0), TextPos::new(0, 4)));
        let outside = ltr_page_with_float(Some(5));
        let rec = RecordingPainter::new();
        paint_anchored_selection_tint(&rec, &outside, sel, None);
        let img_fill = |r: &RecordingPainter| {
            r.cmds().iter().any(|c| matches!(c, DrawCmd::FillRect { x, .. } if *x == 200.0))
        };
        assert!(!img_fill(&rec), "anchor para outside the range must not tint");

        let unlinked = ltr_page_with_float(None);
        let rec2 = RecordingPainter::new();
        paint_anchored_selection_tint(&rec2, &unlinked, sel, None);
        assert!(!img_fill(&rec2), "a None-linked carrier never tints");

        let linked = ltr_page_with_float(Some(0));
        let rec3 = RecordingPainter::new();
        paint_anchored_selection_tint(
            &rec3,
            &linked,
            Some((TextPos::new(0, 2), TextPos::new(0, 2))),
            None,
        );
        assert!(rec3.cmds().is_empty(), "collapsed caret must not tint the float");
    }
}
