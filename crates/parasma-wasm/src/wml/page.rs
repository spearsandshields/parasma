//! Per-page layout — the streaming output home.
//!
//! Dependency-leaf data types for the paged document model: the page itself
//! (`PageLayout`), its elements (`PageElement` / `PagedLine` / `PagedTableRow` /
//! `PagedTableCell`), absolutely-positioned overlays (`AnchoredImageLayout` /
//! `PositionedTextBox`), the block-level layout caches that feed page assembly
//! (`BlockCache` / `CellBlock`), and the laid-out header/footer cache
//! (`HdrFtrCache`). Everything else (render, hdr/ftr layout, run_layout_impl,
//! dbg) references these types, so they move first.

use crate::wml::border::BorderEdge;
use crate::wml::linebreak::{Item, LineSegment};
use crate::wml::paragraph::ParagraphCache;
use crate::wml::table::{CellBorderEdge, TableCache};

// Table layout caches (§17.4) relocated to wml::table (TableCache /
// TableRowCache / TableCellCache + CellVAlign + content_height). CellBlock
// stays here — it bridges the paragraph cache (ParagraphCache) and the table
// cache (wml::table::TableCache).

// Paragraph/Table caches differ greatly in size; boxing the large variant is a
// deferred off-thread-enablement change (output-identical but ripples through
// every construct/match site).
#[allow(clippy::large_enum_variant)]
pub(crate) enum CellBlock {
    Paragraph(ParagraphCache),
    Table(TableCache),
}

#[allow(clippy::large_enum_variant)] // see CellBlock: boxing deferred
pub(crate) enum BlockCache {
    Paragraph(ParagraphCache),
    Table(TableCache),
}

// ─── Per-page layout (the streaming output) ──────────────────────────────────

// Clone: per-page layout checkpoints capture `current` as a WHOLE-struct clone
// (never a field list — carry-seeded pages start non-empty, and finish_page
// stamps header_rows/footer_rows beyond the obvious fields).
#[derive(Clone, Default)]
pub(crate) struct PageLayout {
    pub(crate) elements: Vec<PageElement>,
    pub(crate) header_lines: Vec<PagedLine>,
    pub(crate) footer_lines: Vec<PagedLine>,
    /// Header/footer tables (§17.10): block-level tables in the header/footer
    /// band are positioned into `PagedTableRow`s at the header/footer origin and
    /// painted in the same band as `header_lines`/`footer_lines`.
    pub(crate) header_rows: Vec<PagedTableRow>,
    pub(crate) footer_rows: Vec<PagedTableRow>,
    pub(crate) anchored_images: Vec<AnchoredImageLayout>,
    pub(crate) positioned_text_boxes: Vec<PositionedTextBox>,
    pub(crate) footnote_ids: Vec<i64>,
    pub(crate) endnote_ids: Vec<i64>,
    pub(crate) page_number: usize,
    /// Vertical column separator lines: (x, y_start, y_end) in absolute page px.
    pub(crate) col_separator_lines: Vec<(f64, f64, f64)>,
    /// Index of the `Section` this page belongs to (§17.6). Used to resolve the
    /// page's own `PageGeometry` (page size + margins) for paint and the
    /// `pageSize` API, so a mixed-size multi-section doc renders each section at
    /// its own dimensions. Defaults to 0 (the single-section / first-section
    /// case) — every single-section page stamps 0, preserving byte-identity.
    pub(crate) section_idx: usize,
}

#[derive(Clone)]
pub(crate) struct PositionedTextBox {
    pub(crate) lines: Vec<PagedLine>,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width_px: f64,
    pub(crate) height_px: f64,
    pub(crate) behind_text: bool,
    pub(crate) fill_color: Option<String>,
    pub(crate) border_color: Option<String>,
    pub(crate) border_width_px: f64,
}

#[derive(Clone)]
pub(crate) struct AnchoredImageLayout {
    pub(crate) rel_id: String,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width_px: f64,
    pub(crate) height_px: f64,
    pub(crate) behind_text: bool,
    /// Anchored-image copy — the document-order id of the
    /// paragraph this float is ANCHORED to, or `None` for carriers built
    /// outside the capture walk. INERT for normal render (never read on any
    /// layout/draw path — byte-identity holds); read ONLY by
    /// `paint_selection_pass` to tint a float whose anchor paragraph falls
    /// inside the active text selection.
    pub(crate) anchor_para: Option<u32>,
}

impl PageLayout {
    pub(crate) fn new() -> Self {
        Self {
            elements: Vec::new(),
            header_lines: Vec::new(),
            footer_lines: Vec::new(),
            header_rows: Vec::new(),
            footer_rows: Vec::new(),
            anchored_images: Vec::new(),
            positioned_text_boxes: Vec::new(),
            footnote_ids: Vec::new(),
            endnote_ids: Vec::new(),
            page_number: 0,
            col_separator_lines: Vec::new(),
            section_idx: 0,
        }
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.elements.is_empty() && self.anchored_images.is_empty() && self.positioned_text_boxes.is_empty()
    }
    pub(crate) fn push(&mut self, elem: PageElement) {
        self.elements.push(elem);
    }
}

/// One laid-out header/footer block, in document order (§17.10). Tables in the
/// header/footer band are kept ALONGSIDE paragraphs so the band's vertical
/// geometry interleaves correctly (a paragraph above the table, the table, then
/// a paragraph below it).
#[allow(clippy::large_enum_variant)] // see CellBlock: boxing deferred
pub(crate) enum HfBlock {
    Paragraph(ParagraphCache),
    Table(TableCache),
}

pub(crate) struct HdrFtrCache {
    pub(crate) blocks: Vec<HfBlock>,
    pub(crate) total_height: f64,
}

#[derive(Clone)]
pub(crate) enum PageElement {
    Line(PagedLine),
    TableRow(PagedTableRow),
}

/// `PagedLine::para` for lines that carry no captured paragraph
/// id (header/footer, footnote/endnote band, textbox interior — the streams
/// excluded from selection). Body/table-cell lines carry the real
/// document-order id propagated from `ParagraphCache::para_id`.
pub(crate) const PARA_SENTINEL: u32 = u32::MAX;

/// A line as positioned on a specific page. Coordinates are absolute within
/// the page's coordinate space (top-left origin).
#[derive(Clone, Default)]
pub(crate) struct PagedLine {
    /// Always ≥1. The non-float case is a single full-width segment.
    pub(crate) segments: Vec<LineSegment>,
    /// The document-order paragraph id this line belongs
    /// to (from `ParagraphCache::para_id`), or `PARA_SENTINEL` for non-captured
    /// streams (hdr/ftr, footnote band, textbox). Every painted line knows its
    /// paragraph so `hitTest` can return `TextPos.para` and `selectionRects` can
    /// filter lines by paragraph range.
    pub(crate) para: u32,
    pub(crate) baseline_y: f64,
    pub(crate) para_shading: Option<String>,
    pub(crate) line_height: f64,
    pub(crate) content_left: f64,
    pub(crate) content_right: f64,
    pub(crate) border_top: Option<BorderEdge>,
    pub(crate) border_bottom: Option<BorderEdge>,
    pub(crate) border_left: Option<BorderEdge>,
    pub(crate) border_right: Option<BorderEdge>,
    /// Absolute page-x of each bar-tab rule (§17.18.84 "bar") crossing this
    /// line: a vertical line spanning the line box on EVERY line of the
    /// paragraph, never a text-advance stop. Empty for the overwhelming
    /// common case (paint is skipped entirely). Computed per placement site
    /// from `ParagraphCache::tab_stops` (bar positions are leading-edge
    /// relative per §17.3.1.37, so the site's column/cell edges resolve them).
    pub(crate) bar_tabs: Vec<f64>,
    pub(crate) is_first_line: bool,
    pub(crate) is_last_line: bool,
}

impl PagedLine {
    /// Convenience for the common single-segment (no-float) line. Lets the
    /// many construction sites pass `items`/`x_start` as before.
    pub(crate) fn single(items: Vec<Item>, x_start: f64) -> Vec<LineSegment> {
        vec![LineSegment { x_start, items }]
    }
}

#[cfg(test)]
impl PagedLine {
    /// Serialize this line and every item on it into the oracle ITEM stream
    /// `tag` names the stream (body / header / cell / textbox …);
    /// `indent` is prefixed to every emitted line.
    ///
    /// The opening exhaustive destructure (NO rest-dots) is a COMPILE-TIME
    /// completeness guard, the pattern `CellStyle::hash_into` (sml/model.rs)
    /// established: adding a field to `PagedLine` fails to compile until it is
    /// bound here and a decision recorded about which stream owns it. Before this
    /// existed the oracle read 5 of 12 fields, so dropping every paragraph
    /// border, every shading and every bar-tab rule left all 21 pins green
    /// (measured).
    ///
    /// The four `_`-bound fields are the LINE stream's, deliberately: this stream
    /// is COORDINATE-FREE so that a pure repositioning reds `geometry_dump` alone
    /// and leaves the item snapshot byte-identical. `bar_tabs` contributes its
    /// COUNT only for the same reason (its values are absolute page-x).
    pub(crate) fn dump_into(
        &self, out: &mut String, tag: &str, indent: &str, styles: &mut Vec<String>,
        glyphs: &mut GlyphStream,
    ) {
        use std::fmt::Write;
        glyphs.begin_line(indent, tag);
        let PagedLine {
            segments, para, para_shading, border_top, border_bottom, border_left,
            border_right, bar_tabs, is_first_line, is_last_line,
            // ── owned by the LINE stream (`geometry_dump`) — see the doc above ──
            baseline_y: _, line_height: _, content_left: _, content_right: _,
        } = self;
        let _ = write!(out, "{indent}{tag} p{para} f{} l{} segs{}",
            *is_first_line as u8, *is_last_line as u8, segments.len());
        if let Some(s) = para_shading {
            let _ = write!(out, " shade{s:?}");
        }
        for (name, edge) in [("bt", border_top), ("bb", border_bottom),
                             ("bl", border_left), ("br", border_right)] {
            if let Some(e) = edge {
                let _ = write!(out, " {name}={}/{:.2}/{:.2}", e.color, e.width_px, e.space_px);
            }
        }
        if !bar_tabs.is_empty() {
            let _ = write!(out, " bars{}", bar_tabs.len());
        }
        out.push('\n');
        for (si, seg) in segments.iter().enumerate() {
            let _ = writeln!(out, "{indent}  S{si} n{}", seg.items.len());
            glyphs.begin_segment(si);
            for (ii, it) in seg.items.iter().enumerate() {
                let _ = write!(out, "{indent}    i{ii} ");
                it.dump_into(out, styles, glyphs);
                out.push('\n');
                // A text box riding on an item carries its own interior lines —
                // a whole nested stream `geometry_dump` drops entirely.
                if let Some(tb) = &it.text_box {
                    for (li, l) in tb.lines.iter().enumerate() {
                        l.dump_into(out, &format!("TBL{li}"), &format!("{indent}      "), styles, glyphs);
                    }
                }
            }
        }
        glyphs.end_line();
    }
}

// ─── The GLYPH stream ───────────────────────────────────────────────────────
//
// The third orthogonal stream. `geometry_dump` says where line boxes SIT and
// `content_dumps` says what IS on them — including, per shaped run, its glyph COUNT
// and its TOTAL advance (`sh{N}g adv{…}`). Everything INSIDE a run was unpinned,
// and measurably so: reversing the per-glyph advance vector while holding the
// total constant, shifting every glyph's `x_offset` by half a pixel, shifting
// every `y_offset` by half a pixel, and collapsing every interior cluster each
// left the entire suite green.
//
// This stream is emitted BY THE ITEM WALK rather than by a second traversal of
// `viewer.pages`, so "which lines and items exist, in what order" is one rule
// spelled once. It writes to its own buffer and its own golden file, so the ITEM
// goldens stay byte-identical — the same additive property that let the ITEM
// stream be added over the LINE hashes with no re-baseline.
//
// It is INTERNED: the body addresses one vocabulary row per DISTINCT glyph
// vector. That is not cosmetic — the corpus is overwhelmingly Latin text at a
// handful of (face, size) pairs, so `oracle-flow`'s 2,196 shaped runs collapse
// to 310 rows. A shaping regression then shows up as one changed table row plus
// the run references that migrated, which is exactly the reviewable `git diff` a
// hex digest per run would have destroyed.
//
// NO FIXTURE IS EXCLUDED, and that was checked rather than assumed. There is a
// known nondeterministic input to layout: `FontCache::resolve_default_font` breaks
// its cut tie with `min_by` over a `HashMap`, so a run whose family is neither
// loaded nor aliased gets an arbitrary cut per PROCESS. It cannot reach these
// fixtures — no golden carries the un-normalized `fam"Calibri"` that opens that
// door, the only unresolved family present (`fam""`, 18 occurrences) belongs to
// math ATOM items which shape nothing themselves, and the math glyphs hanging off
// them route through `XITS Math`, loaded in exactly one cut. Confirmed
// empirically: five fresh-process suite runs, byte-identical goldens each time.
// If that tie-break is fixed, this stream is the instrument that proves shaping was not
// to blame — the "LINE red / ITEM green is itself a diagnosis" split, one layer
// further down.

/// Accumulates the oracle GLYPH stream while `PagedLine::dump_into` walks a
/// document. One instance per dump; `finish` renders it.
///
/// Addressing MIRRORS the ITEM stream: a row is `{indent}{tag} tok tok …` under
/// the same `L3` / `T4C1L0` / `HC0L1` / `BL2` tags, and the Nth token is the Nth
/// ITEM of that line (`-` where an item shaped nothing, `|` between segments),
/// so a red row is looked up directly in `{name}.items.txt`. Inserting one item
/// therefore shifts ONE row, not every subsequent record.
///
/// Rows with no shaped run at all are dropped, and trailing placeholders trimmed
/// — a body that is mostly `-` teaches a reader nothing.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct GlyphStream {
    /// Distinct `ShapedRun::dump_body` strings in first-appearance order.
    vocab: Vec<String>,
    /// One reserved slot per `begin_line`, filled by its `end_line`. A slot is
    /// reserved on OPEN so a nested text-box line, which necessarily finishes
    /// first, still lands AFTER its parent — matching the ITEM stream's order.
    rows: Vec<String>,
    /// Open (nested) lines: `(slot, prefix, tokens)`.
    open: Vec<(usize, String, Vec<String>)>,
}

#[cfg(test)]
impl GlyphStream {
    pub(crate) fn begin_line(&mut self, indent: &str, tag: &str) {
        self.rows.push(String::new());
        self.open.push((self.rows.len() - 1, format!("{indent}{tag}"), Vec::new()));
    }

    /// Segment boundary within the open line. Emitted as `|` so a token's index
    /// resolves to (segment, item) exactly as the ITEM stream's `S{si}`/`i{ii}`.
    pub(crate) fn begin_segment(&mut self, si: usize) {
        if si > 0 {
            self.push_token("|".to_string());
        }
    }

    pub(crate) fn push_token(&mut self, tok: String) {
        if let Some((_, _, toks)) = self.open.last_mut() {
            toks.push(tok);
        }
    }

    /// Intern one shaped run, returning its vocabulary reference.
    pub(crate) fn intern(&mut self, run: &crate::engine::fonts::ShapedRun) -> String {
        let body = run.dump_body();
        let i = self.vocab.iter().position(|v| *v == body).unwrap_or_else(|| {
            self.vocab.push(body);
            self.vocab.len() - 1
        });
        format!("g{i}")
    }

    pub(crate) fn end_line(&mut self) {
        let Some((slot, prefix, mut toks)) = self.open.pop() else { return };
        while matches!(toks.last().map(String::as_str), Some("-") | Some("|")) {
            toks.pop();
        }
        if toks.iter().any(|t| t != "-" && t != "|") {
            self.rows[slot] = format!("{prefix} {}\n", toks.join(" "));
        }
    }

    /// Vocabulary table first (a reader meets the glyph vectors before the rows
    /// that cite them), then the body — the same head/body shape `content_dumps`
    /// uses for its `RunStyle` intern table.
    pub(crate) fn finish(self) -> String {
        let mut out = String::new();
        for (i, body) in self.vocab.iter().enumerate() {
            out.push_str(&format!("g{i} {body}\n"));
        }
        for row in &self.rows {
            out.push_str(row);
        }
        out
    }
}

#[derive(Clone)]
pub(crate) struct PagedTableRow {
    pub(crate) cells: Vec<PagedTableCell>,
    pub(crate) y_top: f64,
    pub(crate) height_px: f64,
    // Row-level border/margin summary retained on the paged row; per-cell
    // resolved edges (`PagedTableCell::border_*`) are what the painter reads,
    // so these row aggregates are currently unread.
    #[allow(dead_code)]
    pub(crate) border_color: String,
    #[allow(dead_code)]
    pub(crate) has_borders: bool,
    #[allow(dead_code)]
    pub(crate) cell_margin_h: f64,
    #[allow(dead_code)]
    pub(crate) cell_margin_v: f64,
}

#[derive(Clone)]
pub(crate) struct PagedTableCell {
    pub(crate) lines: Vec<PagedLine>,
    pub(crate) x: f64,
    pub(crate) width_px: f64,
    pub(crate) fill_color: Option<String>,
    pub(crate) v_merged_away: bool,
    pub(crate) border_top: Option<CellBorderEdge>,
    pub(crate) border_bottom: Option<CellBorderEdge>,
    pub(crate) border_left: Option<CellBorderEdge>,
    pub(crate) border_right: Option<CellBorderEdge>,
}

/// How many whole content heights an anchored object at `y` must move down for
/// its top to land inside a page's content area, capped at `max_steps`.
///
/// Computed in closed form, never by walking: `y` derives from a
/// document-supplied 64-bit EMU offset, so a walk is as long as the author
/// wants it to be. A non-finite `y` or a degenerate content height moves
/// nothing.
pub(crate) fn relocation_steps(y: f64, content_bottom: f64, content_h: f64, max_steps: usize) -> usize {
    if !(y.is_finite() && content_h > 1.0) || y < content_bottom {
        return 0;
    }
    // `as usize` saturates, so an astronomically large quotient is still capped.
    let needed = ((y - content_bottom) / content_h).floor() as usize;
    needed.saturating_add(1).min(max_steps)
}

#[cfg(test)]
mod relocation_tests {
    use super::relocation_steps;

    #[test]
    fn an_object_inside_the_content_area_does_not_move() {
        assert_eq!(relocation_steps(500.0, 1000.0, 900.0, 10), 0);
    }

    #[test]
    fn an_object_below_the_content_area_moves_to_the_page_it_lands_on() {
        // Exactly at the bottom edge belongs to the next page.
        assert_eq!(relocation_steps(1000.0, 1000.0, 900.0, 10), 1);
        assert_eq!(relocation_steps(1899.0, 1000.0, 900.0, 10), 1);
        assert_eq!(relocation_steps(1900.0, 1000.0, 900.0, 10), 2);
    }

    #[test]
    fn the_move_is_capped_and_never_walks() {
        // The posOffset that used to add 1,093,614 blank pages, and the larger
        // one that never returned: both are answered immediately.
        assert_eq!(relocation_steps(1.0e9, 1000.0, 900.0, 3), 3);
        assert_eq!(relocation_steps(f64::MAX, 1000.0, 900.0, 3), 3);
    }

    #[test]
    fn degenerate_inputs_move_nothing() {
        assert_eq!(relocation_steps(f64::NAN, 1000.0, 900.0, 3), 0);
        assert_eq!(relocation_steps(f64::INFINITY, 1000.0, 900.0, 3), 0);
        assert_eq!(relocation_steps(5000.0, 1000.0, 0.5, 3), 0);
    }
}
