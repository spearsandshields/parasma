//! Paragraph layout (§17.3.1 — the orchestration + paragraph-property half).
//! Verbatim relocation from `lib.rs`: the paragraph orchestrator
//! (`layout_paragraph`), paragraph-property resolution (`ParagraphLayout` +
//! `ParagraphLayout::from_resolved`/`x_start`/`available_width`/`line_height`),
//! the per-paragraph cache (`ParagraphCache`), tab-stop *types* and resolution
//! (`TabAlignment`/`TabLeader`/`ResolvedTabStops`/`resolve_paragraph_tab_stops`),
//! and the paragraph/character style-chain collectors
//! (`collect_paragraph_style_chain`/`collect_paragraph_style_rprs`).
//!
//! The UAX#14 line breaker + measurement half (`collect_items`/`pack_lines`/
//! `justify_line`) lives in the sibling `wml/linebreak.rs`, which imports
//! `ResolvedTabStops`/`TabAlignment`/`TabLeader`/`collect_paragraph_style_rprs`
//! from here via `crate::`; the float-aware packer lives in `wml/float.rs`.
//! `layout_paragraph` calls `collect_items`/`pack_lines` from `wml::linebreak`.
//!
//! `collect_char_style_rprs` deliberately STAYS in `lib.rs` (its only consumers
//! are `RunStyle::from_resolved` and the codepoint/font precompute, both in
//! lib) — moving it would only add a re-export with no cross-module benefit.
//! `resolve_run_option`/`check_toggle` STAY in `lib.rs` (they have lib-side
//! consumers too) and are imported here via `crate::`; the small private
//! helpers used solely by `from_resolved` (`resolve_ppr_option`/
//! `resolve_ppr_indent`/`is_on_off_true`) travel here with it.
//!
//! Purity is enforced by the oracle snapshot tests — their geometry hashes
//! must not drift on a pure move.

use wasm_bindgen::JsValue;

use ooxml_wml::ext::{ParagraphExt, StyleContext};
use ooxml_wml::types::{STJc, STLineSpacingRule};

use crate::engine::fonts::{
    font_metrics, resolve_font_with_fallbacks, try_resolve_font, FaceCache, FontCache,
    ThemeFonts, DEFAULT_FONT_FAMILY, DEFAULT_FONT_PT,
};
use crate::engine::color::ooxml_color_to_css;
use crate::engine::paint::{RunStyle, Underline};
use crate::engine::units::{pt_to_px, twips_to_px};
use crate::wml::bidi::rtl_swap_jc;
use crate::wml::border::{resolve_border_edge, BorderEdge};
use crate::wml::linebreak::{
    apply_east_asian_refinements, collect_items, pack_lines, AnchoredImageInfo,
    AnchoredTextBoxInfo, EaToggles, LayoutLine,
};
use crate::wml::numbering::NumberingMarker;
use crate::{check_toggle, resolve_run_option, Item};

// ─── Tab stop types ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabAlignment {
    Left,
    Center,
    Right,
    /// Decimal tab (§17.18.84, ST_TabJc="decimal"): the following text is placed
    /// so its FIRST decimal separator ('.'/locale ',') sits AT the stop x. Text
    /// with no separator right-aligns its end at the stop (Word's fallback).
    Decimal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabLeader {
    None,
    Dot,
    Hyphen,
    Underscore,
    Heavy,
    MiddleDot,
}

pub(crate) struct ResolvedTabStops {
    pub(crate) custom: Vec<(f64, TabAlignment, TabLeader)>,
    /// Bar-tab rule positions (§17.18.84 ST_TabJc "bar"), leading-edge-relative
    /// px like `custom`. A bar tab "does not result in a custom tab stop in the
    /// parent paragraph (this tab stop location shall be skipped when
    /// positioning custom tab characters), but instead shall be used to draw a
    /// vertical line (or bar) at this location" — so bars are EXCLUDED from
    /// `custom` (never advance text) and painted per line at these positions.
    pub(crate) bars: Vec<f64>,
    pub(crate) default_interval: f64,
}

impl ResolvedTabStops {
    /// Positions and `current_x` are in the paragraph's LOGICAL advance space,
    /// which per §17.3.1.37 is leading-edge-relative ("the left edge for a
    /// left-to-right paragraph, and the right edge for a right-to-left
    /// paragraph") — so the same walk serves both directions; for a base-RTL
    /// paragraph the End-alignment `x_start` plus the UAX#9 L2 reorder map the
    /// logical distances onto the right edge (see `pack_lines`/`bidi.rs`).
    pub(crate) fn next_stop(&self, current_x: f64) -> (f64, TabAlignment, TabLeader) {
        for &(pos, align, leader) in &self.custom {
            if pos > current_x + 1.0 {
                return (pos, align, leader);
            }
        }
        let n = (current_x / self.default_interval).floor() as i64 + 1;
        (n as f64 * self.default_interval, TabAlignment::Left, TabLeader::None)
    }

    /// Absolute page-x of each bar-tab rule for one placed line. §17.3.1.37:
    /// positions are leading-edge-relative, so LTR measures rightward from the
    /// leading (left indent) edge and RTL leftward from the right one.
    pub(crate) fn bar_xs(&self, leading_left: f64, leading_right: f64, base_rtl: bool) -> Vec<f64> {
        if self.bars.is_empty() {
            return Vec::new();
        }
        self.bars.iter()
            .map(|&pos| if base_rtl { leading_right - pos } else { leading_left + pos })
            .collect()
    }
}
// ─── Per-paragraph layout (cached for future incremental relayout) ───────────

pub(crate) struct ParagraphCache {
    pub(crate) layout: ParagraphLayout,
    pub(crate) lines: Vec<LayoutLine>,
    pub(crate) anchored_images: Vec<AnchoredImageInfo>,
    pub(crate) anchored_text_boxes: Vec<AnchoredTextBoxInfo>,
    pub(crate) footnote_ids: Vec<i64>,
    pub(crate) endnote_ids: Vec<i64>,
    /// The paragraph's fully resolved tab stops (§17.3.1.38 hierarchy merge).
    /// Carried so every re-pack site — the unequal-columns reflow
    /// (unequal-width column balancing) and the float-aware packer — reuses the
    /// SAME stops the pre-packed lines were built with, and so bar-tab rule
    /// positions (§17.18.84) reach the per-line paint.
    pub(crate) tab_stops: ResolvedTabStops,
    /// The document-order paragraph id assigned during
    /// the cache-build walk, or `None` for caches built OUTSIDE that walk
    /// (hdr/ftr, footnote band, textbox — `layout_paragraph` passed `capture:
    /// None`). Propagated to every `PagedLine.para` at pagination time.
    pub(crate) para_id: Option<u32>,
    /// The paragraph's items in LOGICAL order, before `pack_lines` permuted each
    /// line into UAX#9 visual order — `Some` only when `bidi::may_reorder` holds,
    /// i.e. for base-RTL or complex-script paragraphs. Every re-pack site must
    /// prefer this over flattening `lines`, which are visual.
    pub(crate) logical_items: Option<Vec<Item>>,
}

/// Width of the slot reserved for a numbering marker (§17.9, widened to the
/// marker when wider). The marker renders into this slot at the first-line outdent; the
/// following paragraph text begins at its end.
///
/// - An empty marker (`numFmt="none"`) occupies zero width so the text honours
///   the hanging outdent instead of being shoved back to `left`.
/// - With a hanging indent, the slot is normally the hanging width so the text
///   aligns at `left`. But a marker WIDER than that (e.g. "1.1.1", "viii)")
///   would otherwise overrun into the text ("1.1.1Deep" crowding); floor the
///   slot at the marker's own advance (which already carries one space of suffix
///   gap) so a wide marker pushes the text right instead of overlapping.
/// - With no hanging indent, the slot is exactly the marker advance.
fn marker_slot_width(marker_empty: bool, hanging_px: f64, marker_advance: f64) -> f64 {
    if marker_empty {
        0.0
    } else if hanging_px > 0.0 {
        hanging_px.max(marker_advance)
    } else {
        marker_advance
    }
}

/// Lay out ONE paragraph into a [`ParagraphCache`]: the shaped, wrapped lines
/// plus the resolved line-box metrics (indents, before/after spacing, line
/// height, shading, numbering marker) the page assembler needs to place and
/// paint it. This is the paragraph-level orchestrator — it resolves the style
/// chain, collects break-bounded items ([`crate::wml::linebreak::collect_items`]),
/// packs them into lines against the content width, then applies numbering /
/// tab-stops / justification. Governing spec: §17.3.1.x paragraph properties
/// (indent §17.3.1.12, spacing §17.3.1.33, justification §17.3.1.13).
// Positional orchestrator args; struct-ification deferred to a dedicated pass.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_paragraph(
    font_cache: &FontCache,
    paragraph: &ooxml_wml::types::Paragraph,
    content_width: f64,
    marker: Option<&NumberingMarker>,
    style_ctx: &StyleContext,
    default_ppr: Option<&ooxml_wml::types::CTPPrGeneral>,
    default_tab_stop_px: f64,
    theme_fonts: &ThemeFonts,
    table_style_id: Option<&str>,
    table_style_rprs: &[&ooxml_wml::types::RunProperties],
    table_cond_pprs: &[&ooxml_wml::types::CTPPrGeneral],
    grid_pitch_px: Option<f64>,
    exact_row: bool,
    // Selection capture — `Some` ONLY on the cache-build walk (body + table cells);
    // `None` on every re-runnable call site (hdr/ftr, footnote band, textbox).
    capture: Option<&mut crate::wml::select::ParaCapture>,
) -> Result<ParagraphCache, JsValue> {
    // This paragraph's shaping scope: the EA re-measure pass and the
    // numbering marker reuse whatever faces they build. `collect_items` holds
    // its own (its run walk is the bulk of the shaping).
    let faces = FaceCache::new();
    let mut layout = ParagraphLayout::from_resolved(paragraph, style_ctx, default_ppr, table_style_id, table_cond_pprs, grid_pitch_px, exact_row);

    if let Some(m) = marker {
        // Only inherit the numbering level's indent when the paragraph did not
        // set its own. A direct `<w:ind w:left="0"/>` (common on numbered
        // headings) explicitly cancels the list indent and must win.
        if !layout.indent_left_explicit {
            layout.indent_left_px = twips_to_px(m.indent_left_twips);
        }
        if layout.indent_first_line_px == 0.0 {
            layout.indent_first_line_px = twips_to_px(-m.indent_hanging_twips);
        }
    }

    let (mut items, anchored_images, anchored_text_boxes, footnote_ids, endnote_ids) = collect_items(font_cache, paragraph, style_ctx, theme_fonts, table_style_rprs)?;

    // East Asian refinements (§17.3.1.2/.3/.16/.45) — one post-pass, BEFORE
    // the ParaMeta capture: the splits preserve the concatenated logical text
    // and byte tiling, and the autospace fold touches no text/bytes, so the
    // capture's byte basis is intact.
    apply_east_asian_refinements(&mut items, layout.ea, font_cache, &faces);

    // Capture ParaMeta HERE: after collect_items, BEFORE the
    // numbering-marker insert, so `logical_text` shares the exact byte basis of
    // every item's `char_start`/`char_len`. Read-only fold gated entirely on
    // `Some`; never mutates `items` (byte-identity).
    let para_id = if let Some(cap) = capture {
        let logical_text = crate::wml::select::logical_text_of(&items);
        // Capture the logical-order runs WITH style HERE,
        // where `items` is live post-`collect_items`, pre-marker-insert, sharing
        // `logical_text`'s exact byte basis. Read-only fold; byte-identical.
        let runs = crate::wml::select::build_para_runs(&items);
        // Anchored-image copy — record the paragraph's ANCHORED
        // images (chart-gated, DISPLAY px) so extractRange can bring a float
        // along when its anchor paragraph is inside the copied range.
        let anchored = crate::wml::select::anchored_image_refs(&anchored_images);
        let numbering = marker.map(crate::wml::select::NumInfo::from_marker);
        let alignment = layout.alignment;
        let base_rtl = layout.base_rtl;
        let location = cap.location.clone();
        Some(cap.register(move |id| crate::wml::select::ParaMeta {
            para_id: id,
            logical_text,
            location,
            alignment,
            base_rtl,
            numbering,
            runs,
            anchored_images: anchored,
        }))
    } else {
        None
    };

    if let Some(m) = marker {
        // NOTE: unlike RunStyle::default() (empty font-slot families), this
        // fallback seeds the three family slots with DEFAULT_FONT_FAMILY — those
        // feed shaping/measurement of the empty-para-with-marker line, so they
        // MUST stay explicit before the spread.
        let mut style = items.first().map(|i| i.style.clone()).unwrap_or_else(|| RunStyle {
            family: DEFAULT_FONT_FAMILY.to_owned(),
            family_east_asia: DEFAULT_FONT_FAMILY.to_owned(),
            family_complex: DEFAULT_FONT_FAMILY.to_owned(),
            ..Default::default()
        });
        // The marker (bullet/number + separator) is padded to the hanging width;
        // carrying the run's underline/strike across that pad draws a rule under
        // the empty gap before the text — and past it. The marker is not part of
        // the underlined run, so strip text decorations from it.
        style.underline = Underline::None;
        style.strikethrough = false;
        style.double_strike = false;
        // §17.9.24 — the level's OWN rPr formats the numbering text (positive
        // half of the marker-formatting rule). Order is load-bearing: the run-derived
        // decorations were stripped above (the negative half); the
        // level's own u/strike win here. Placed BEFORE the font resolution +
        // shaping below so measurement and the marker slot-width fit all see the
        // overlaid face/size.
        //
        // FLOOR: the marker's BASE formatting remains the first text run's
        // RunStyle clone (above), not Word's paragraph-mark rPr chain —
        // §17.9.24 does not define the base, and changing it is unmeasured pin
        // risk across every numbered fixture for zero corpus evidence. The
        // overlay applies on top of whichever base.
        let ov = &m.rpr;
        if let Some(fam) = ov.family.as_deref() {
            // Resolve through the substitution chain; on total miss (Symbol/
            // OpenSymbol/Wingdings — never shipped; markers never enter
            // fontCodepointsUsed so marker-only faces are never fetched), KEEP
            // the run-derived family so the remapped • keeps today's face and
            // advance. (No hardcoded symbol-font name set: keep-base-on-miss
            // subsumes it.)
            if let Some(resolved) = try_resolve_font(fam, font_cache) {
                style.family = resolved;
            }
        }
        if let Some(pt) = ov.font_pt { style.font_pt = pt; }
        if let Some(b) = ov.bold { style.bold = b; }
        if let Some(i) = ov.italic { style.italic = i; }
        if let Some(c) = &ov.color { style.color = c.clone(); }
        if let Some(u) = ov.underline { style.underline = u; }
        if let Some(s) = ov.strikethrough { style.strikethrough = s; }
        if let Some(d) = ov.double_strike { style.double_strike = d; }
        // §17.9.28 w:suff — the separator between marker and text. `tab`
        // (default) / `space` append a trailing gap; `nothing` butts the text up
        // against the marker. We bake the gap into the marker's shaped text so
        // its advance carries the gap (used as the min-gap floor below).
        let suffix_gap = match m.suffix {
            ooxml_wml::types::STLevelSuffix::Nothing => "",
            _ => "\u{00A0}", // tab/space → at least one space's worth of gap
        };
        let marker_text = format!("{}{}", m.text, suffix_gap);
        let font = font_cache.resolve(
            &style.family,
            if style.bold { 700 } else { 400 },
            style.italic,
        );
        let hanging_px = twips_to_px(m.indent_hanging_twips);
        let (marker_advance, marker_cw, shaped) = if let Some(run) = font.and_then(|f| faces.shape(&marker_text, f, style.font_pt)) {
            // `trailing_ws_advance` counts the baked NBSP suffix gap
            // (char::is_whitespace includes U+00A0), so `cw` is the lvlText
            // glyphs alone.
            let cw = run.total_advance - run.trailing_ws_advance();
            (run.total_advance, cw, Some(run))
        } else {
            // FLOOR: in this no-shaping estimated-width fallback the
            // decoration-extent clamp below uses the estimate (which includes
            // the gap) — decorated-marker extent is only glyph-exact when the
            // font shapes.
            let estimated = marker_text.chars().count() as f64 * pt_to_px(style.font_pt) * 0.5;
            (estimated, estimated, None)
        };
        let marker_width = marker_slot_width(m.text.is_empty(), hanging_px, marker_advance);
        // §17.9.24 scopes the level rPr to the lvlText; the §17.9.28 suff
        // separator is not lvlText and the slot pad is not text at all. When
        // the OVERLAID style decorates the marker, clamp the decoration extent
        // (`content_width`, which the renderer rules over for zero-length
        // sentinel items) to the glyphs so the rule does not run across the
        // NBSP gap + hanging-slot pad. Undecorated markers keep
        // content_width == width (byte-identical to before).
        //
        // Known content_width consumers for a DECORATED marker (all intended):
        // `decoration_width` (render.rs) rules over the glyphs; the highlight/
        // run-shading rect (render.rs, filled at content_width) narrows to the
        // glyphs likewise — the marker inherits highlight from its base run
        // clone, and lvlText scoping is the faithful extent for it too;
        // `justify_line` excludes sentinel items entirely (a marker is never a
        // justification gap-carrier — Word does not stretch the marker slot).
        let marker_deco = style.underline != Underline::None
            || style.strikethrough
            || style.double_strike;
        let marker_content_width = if marker_deco { marker_cw } else { marker_width };
        items.insert(0, Item {
            text: marker_text,
            style,
            width: marker_width,
            content_width: marker_content_width,
            shaped,
            // The numbering marker is synthesized AFTER collect_items, so it
            // carries no logical byte span (default char_start=CHAR_START_SENTINEL);
            // hit snaps to para-start.
            ..Default::default()
        });
    }

    let mut tab_stops = resolve_paragraph_tab_stops(paragraph, style_ctx, default_ppr, default_tab_stop_px);
    // §17.3.1.37 — a custom tab-stop `pos` is measured from the current PAGE
    // (leading text) MARGIN, but the advance/fit walk in `pack_lines` runs in the
    // paragraph's LEADING-EDGE frame (x=0 at the indented text start; see
    // `next_stop`/`bar_xs` doc-comments, which already ASSERT this frame). For an
    // indented paragraph the two frames diverge by the leading indent, so a raw
    // `twips_to_px(pos)` right-tab lands `indent` px too far right and its trailing
    // text (a TOC page number + dot leader) overflows `available` and wraps to the
    // next line. Translate every margin-relative stop into the leading-edge frame
    // by subtracting the leading indent — LEFT for LTR base, RIGHT for RTL base
    // (§17.3.1.37 "left edge for LTR, right edge for RTL"). Stops at or left of the
    // leading edge are inert (`next_stop` needs pos > current_x, and x >= 0) so we
    // drop them. This also collapses the implicit hanging-indent stop (pushed at
    // the left indent in `resolve_paragraph_tab_stops`) to ~0 — the guard against
    // DOUBLE-SHIFTING a hanging-indent numbered list, whose marker is positioned by
    // `marker_slot_width`, not the tab advance: the now-inert implicit stop cannot
    // push the body text a second time. Bars are translated too — `bar_xs` re-adds
    // the leading edge, so a margin-frame bar on an indented paragraph would
    // otherwise double-count the indent. Un-indented paragraphs (LEADING == 0, incl.
    // every hdr/ftr default-tab and body tab in the oracle snapshots) are untouched:
    // the two frames coincide, so the render is byte-identical.
    let leading_indent = if layout.base_rtl { layout.indent_right_px } else { layout.indent_left_px };
    if leading_indent.abs() > 0.5 {
        tab_stops.custom.retain_mut(|(pos, _, _)| { *pos -= leading_indent; *pos > 0.5 });
        for pos in tab_stops.bars.iter_mut() { *pos -= leading_indent; }
        tab_stops.bars.retain(|&pos| pos > 0.5);
    }
    let available = content_width - layout.indent_left_px - layout.indent_right_px;
    // `pack_lines` reorders each finished line into UAX#9 VISUAL order, so
    // its output is no longer a faithful logical stream. Every re-pack site (the
    // float-aware packer, the unequal-columns reflow, float-aware table cells)
    // rebuilds its input by flattening these lines, and re-packing a visual
    // sequence is wrong twice over: the words are re-broken in the wrong order,
    // and applying L2 again does NOT restore anything (L2 is not idempotent on a
    // visual sequence — a second pass hands back logical order). Keep the
    // pristine logical items for those sites, but only for paragraphs L2 can
    // actually touch, so the Latin/CJK majority pays one scan and no clone.
    let logical_items = crate::wml::bidi::may_reorder(&items, layout.base_rtl)
        .then(|| items.clone());
    let mut lines = pack_lines(items, available, layout.indent_first_line_px, font_cache, &tab_stops, layout.base_rtl);
    if lines.is_empty() {
        // Empty paragraph: its height is the paragraph mark's natural line
        // height (§17.3.1.29 — the mark is a formattable character), not the em
        // fallback that a zero-metric blank line falls back to. For a FREE-BODY
        // paragraph, resolve the mark's size + family from the mark's OWN
        // `<w:pPr><w:rPr>` FIRST, then the paragraph style chain (style →
        // docDefaults) — the same order `autospace_pt` uses below. Earlier this
        // consulted the style chain ONLY, which mis-sized blank lines whose mark
        // rPr sets an explicit size/font: file-sample_500kB's DejaVu Sans 10.5pt
        // empties rendered as 12pt Liberation Serif (style default), ~8.7% too tall.
        //
        // EXCEPTION — modern docs (compatibilityMode ≥ 15): Word sizes a blank
        // line from the paragraph STYLE/docDefault, NOT the mark rPr. Honoring the
        // mark in a modern doc regresses real Word output (calibration doc C1,
        // compatMode=15: its Arial 9/10pt empty marks drop it 21→20pp vs Word's
        // 11pt default). Only LEGACY docs (compatMode < 15 or absent — the same
        // classification that gates the trailing-space reserve) size empties from
        // the mark; file-sample_500kB (written with no compatMode) is legacy, so
        // its DejaVu Sans 10.5pt marks correctly drive the blank-line height.
        // (Heuristic: verified against both docs; the legacy/modern axis is the
        // cleanest non-overfit separator found — see also the trailing reserve.)
        let mut line = LayoutLine::new();
        let no_rpr: Option<Box<ooxml_wml::types::RunProperties>> = None;
        let pm_rprs = collect_paragraph_style_rprs(paragraph, style_ctx);
        // §17.3.1.29: the paragraph-mark glyph is formatted by the mark's own rPr
        // (pPr/rPr), honored unconditionally (version-independent per spec). This
        // drives the empty-paragraph line height.
        let mark_rpr = paragraph.p_pr.as_ref().and_then(|p| p.r_pr.as_ref());
        // §17.3.1.29 mark SIZE — shared with the autospace 1em margin below.
        let font_pt = resolve_mark_size_pt(paragraph, style_ctx);
        let doc_family = mark_rpr
            .and_then(|rpr| rpr.fonts.as_ref())
            .and_then(|f| f.ascii.clone()
                .or_else(|| f.ascii_theme.as_ref()
                    .and_then(|t| theme_fonts.resolve(t)).map(str::to_owned)))
            .or_else(|| resolve_run_option(&no_rpr, &[], &pm_rprs, &[], style_ctx,
                |rpr| rpr.fonts.as_ref().and_then(|f| f.ascii.clone()
                    .or_else(|| f.ascii_theme.as_ref()
                        .and_then(|t| theme_fonts.resolve(t)).map(str::to_owned)))))
            .unwrap_or_else(|| DEFAULT_FONT_FAMILY.to_owned());
        let family = resolve_font_with_fallbacks(&doc_family, font_cache);
        if let Some(font) = font_cache.resolve(&family, 400, false) {
            let m = font_metrics(font, font_pt);
            line.max_ascent = m.ascent;
            line.max_descent = m.descent + crate::effective_line_gap(&m);
        }
        line.max_font_pt = font_pt;
        lines.push(line);
    }
    Ok(ParagraphCache { layout, lines, anchored_images, anchored_text_boxes, footnote_ids, endnote_ids, tab_stops, para_id, logical_items })
}

/// §17.3.1.29 — the paragraph mark's resolved point size: the mark's direct
/// `pPr/rPr` size wins, then the paragraph's run-property style chain (first with
/// a size), then `ctx.default_run_properties`, then `DEFAULT_FONT_PT`. Half-points
/// → points. Shared by the empty-paragraph mark-line height and the 1em auto-space
/// margin (§17.3.1.32-.33), which format the same pilcrow glyph.
fn resolve_mark_size_pt(
    paragraph: &ooxml_wml::types::Paragraph,
    style_ctx: &StyleContext,
) -> f64 {
    let mark_rpr = paragraph.p_pr.as_ref().and_then(|p| p.r_pr.as_ref());
    let pm_rprs = collect_paragraph_style_rprs(paragraph, style_ctx);
    let no_rpr: Option<Box<ooxml_wml::types::RunProperties>> = None;
    mark_rpr
        .and_then(|rpr| rpr.size.as_ref())
        .and_then(|sz| sz.value.parse::<u32>().ok())
        .or_else(|| resolve_run_option(&no_rpr, &[], &pm_rprs, &[], style_ctx,
            |rpr| rpr.size.as_ref().and_then(|sz| sz.value.parse::<u32>().ok())))
        .map(|hp| hp as f64 / 2.0)
        .unwrap_or(DEFAULT_FONT_PT)
}

/// A position's merged outcome while walking the style hierarchy: a text
/// advance stop, or a bar-tab rule (§17.18.84 — draws a line, never advances).
enum MergedStop {
    Stop(TabAlignment, TabLeader),
    Bar,
}

/// Return the paragraph's effective tab-stop chain: the direct `w:tabs` merged
/// over the style-chain tabs, position-keyed so a nearer definition overrides a
/// farther one and a `w:val="clear"` deletes that position. Solves "which stop
/// does a `\t` advance to" for this paragraph, feeding both the custom advance
/// stops and the bar-tab rule positions. Governing spec: §17.3.1.37 tab-stop
/// definition + position-keyed merge/clear (§17.3.1.38).
fn resolve_paragraph_tab_stops(
    paragraph: &ooxml_wml::types::Paragraph,
    style_ctx: &StyleContext,
    default_ppr: Option<&ooxml_wml::types::CTPPrGeneral>,
    default_tab_stop_px: f64,
) -> ResolvedTabStops {
    let style_pprs = collect_paragraph_style_chain(paragraph, style_ctx);

    // §17.3.1.38: tab stops are ADDITIVE across the style hierarchy — "tab
    // stops at each level in the style hierarchy are added to each other to
    // determine the full set of tab stops for the paragraph" — keyed by
    // POSITION. Walk base→derived (docDefaults → basedOn ancestors → the
    // paragraph's own style → direct pPr): a stop at an already-seen position
    // OVERRIDES it, a different position ACCUMULATES, and val="clear" REMOVES
    // the inherited stop at its position without rendering itself (§17.18.84:
    // "cleared and shall be removed and ignored when processing").
    // `collect_paragraph_style_chain` returns derived-first, hence `.rev()`.
    let mut merged: std::collections::BTreeMap<i64, MergedStop> = std::collections::BTreeMap::new();
    let sources = default_ppr.and_then(|d| d.tabs.as_ref()).into_iter()
        .chain(style_pprs.iter().rev().filter_map(|ppr| ppr.tabs.as_ref()))
        .chain(paragraph.p_pr.as_ref().and_then(|ppr| ppr.tabs.as_ref()));
    for tab_list in sources {
        for tab in &tab_list.tab {
            let pos_twips: i64 = tab.pos.parse().unwrap_or(0);
            match tab.value {
                ooxml_wml::types::STTabJc::Clear => {
                    merged.remove(&pos_twips);
                }
                ooxml_wml::types::STTabJc::Bar => {
                    merged.insert(pos_twips, MergedStop::Bar);
                }
                _ => {
                    let alignment = match tab.value {
                        ooxml_wml::types::STTabJc::Right | ooxml_wml::types::STTabJc::End => TabAlignment::Right,
                        ooxml_wml::types::STTabJc::Center => TabAlignment::Center,
                        ooxml_wml::types::STTabJc::Decimal => TabAlignment::Decimal,
                        _ => TabAlignment::Left,
                    };
                    let leader = tab.leader.as_ref().map_or(TabLeader::None, |l| match l {
                        ooxml_wml::types::STTabTlc::Dot => TabLeader::Dot,
                        ooxml_wml::types::STTabTlc::Hyphen => TabLeader::Hyphen,
                        ooxml_wml::types::STTabTlc::Underscore => TabLeader::Underscore,
                        ooxml_wml::types::STTabTlc::Heavy => TabLeader::Heavy,
                        ooxml_wml::types::STTabTlc::MiddleDot => TabLeader::MiddleDot,
                        _ => TabLeader::None,
                    });
                    merged.insert(pos_twips, MergedStop::Stop(alignment, leader));
                }
            }
        }
    }

    let mut custom = Vec::new();

    // Implicit tab stop at the left indent position when hanging indent is present
    // (per §17.3.1.12: content after a tab in hanging indent area aligns at left indent)
    let ind = paragraph.p_pr.as_ref()
        .and_then(|ppr| ppr.indentation.as_ref())
        .or_else(|| {
            for ppr in &style_pprs {
                if ppr.indentation.is_some() {
                    return ppr.indentation.as_ref();
                }
            }
            None
        });
    if let Some(ind) = ind {
        let has_hanging = ind.hanging.as_deref()
            .and_then(|h| h.parse::<i64>().ok())
            .unwrap_or(0) > 0;
        if has_hanging {
            let left_twips = ind.left.as_deref()
                .or(ind.start.as_deref())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            if left_twips > 0 {
                custom.push((twips_to_px(left_twips), TabAlignment::Left, TabLeader::None));
            }
        }
    }

    let mut bars = Vec::new();
    for (&pos_twips, stop) in &merged {
        match stop {
            MergedStop::Stop(alignment, leader) => custom.push((twips_to_px(pos_twips), *alignment, *leader)),
            MergedStop::Bar => bars.push(twips_to_px(pos_twips)),
        }
    }
    custom.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    custom.dedup_by(|a, b| (a.0 - b.0).abs() < 0.5);

    ResolvedTabStops {
        custom,
        bars,
        default_interval: default_tab_stop_px,
    }
}

// ─── Paragraph-level properties ──────────────────────────────────────────────

pub(crate) struct ParagraphLayout {
    pub(crate) alignment: STJc,
    pub(crate) indent_left_px: f64,
    /// True when the paragraph (or its style chain) explicitly set a left
    /// indent — even to 0. A numbered paragraph's direct `<w:ind w:left>` must
    /// override the numbering level's indent: an explicit 0 *cancels* the list
    /// indent (§17.3.1.12), whereas a value that merely defaulted to 0 still
    /// inherits the marker's indent.
    indent_left_explicit: bool,
    pub(crate) indent_right_px: f64,
    pub(crate) indent_first_line_px: f64,
    pub(crate) space_before_px: f64,
    /// True when `space_before` came from `beforeAutospacing` (§17.3.1.33 HTML
    /// auto-margin) rather than an explicit `w:before`. Page-top suppression
    /// (CSS first-child margin-collapse) applies only to the auto margin.
    pub(crate) before_auto: bool,
    pub(crate) space_after_px: f64,
    line_spacing_twips: Option<i64>,
    line_spacing_rule: STLineSpacingRule,
    /// Document line-grid pitch in px this paragraph snaps its lines up to
    /// (§17.6.5). `None` when the section has no grid, the document uses
    /// Far-East layout, `snapToGrid` is off (§17.3.1.32), or it is a table cell.
    grid_pitch_px: Option<f64>,
    pub(crate) contextual_spacing: bool,
    pub(crate) style_id: Option<String>,
    pub(crate) shading_color: Option<String>,
    pub(crate) border_top: Option<BorderEdge>,
    pub(crate) border_bottom: Option<BorderEdge>,
    pub(crate) border_left: Option<BorderEdge>,
    pub(crate) border_right: Option<BorderEdge>,
    pub(crate) page_break_before: bool,
    pub(crate) keep_next: bool,
    /// §17.3.1.6 bidi — RTL paragraph base direction for UAX#9 reordering.
    pub(crate) base_rtl: bool,
    pub(crate) keep_lines: bool,
    pub(crate) widow_control: bool,
    /// East Asian refinement toggles (§17.3.1.2/.3/.16/.45, all default
    /// true), consumed by `apply_east_asian_refinements` post-`collect_items`.
    pub(crate) ea: EaToggles,
}

impl ParagraphLayout {
    fn from_resolved(
        paragraph: &ooxml_wml::types::Paragraph,
        style_ctx: &StyleContext,
        default_ppr: Option<&ooxml_wml::types::CTPPrGeneral>,
        table_style_id: Option<&str>,
        table_cond_pprs: &[&ooxml_wml::types::CTPPrGeneral],
        section_grid_pitch_px: Option<f64>,
        exact_row: bool,
    ) -> Self {
        let mut style_pprs = collect_paragraph_style_chain(paragraph, style_ctx);
        // Conditional pPr (firstRow, firstCol, etc.) overrides base table style pPr
        for cond_ppr in table_cond_pprs {
            style_pprs.push(cond_ppr);
        }
        if let Some(tbl_id) = table_style_id {
            if let Some(tbl_style) = style_ctx.style(tbl_id) {
                if let Some(ref ppr) = tbl_style.p_pr {
                    style_pprs.push(ppr.as_ref());
                }
            }
        }

        // East Asian refinement toggles: direct pPr → style chain (first
        // hit, derived-first) → docDefaults → spec default TRUE (§17.3.1.2/.3
        // "assumed to be true"; §17.3.1.16/.45 default on). `check_toggle`
        // handles present-without-val as Some(true).
        let resolve_ea = |direct: Option<bool>,
                          field: fn(
            &ooxml_wml::types::CTPPrGeneral,
        )
            -> &Option<Box<ooxml_wml::types::OnOffElement>>|
         -> bool {
            direct
                .or_else(|| style_pprs.iter().find_map(|ppr| check_toggle(field(ppr))))
                .or_else(|| default_ppr.and_then(|d| check_toggle(field(d))))
                .unwrap_or(true)
        };
        // General on/off toggle (§17.3.1): direct pPr → style chain (first hit,
        // derived-first) → per-site spec default. Unlike `resolve_ea` this does
        // NOT consult docDefaults (`default_ppr`) — these seven toggles never did.
        let resolve_ppr_toggle = |direct: Option<bool>,
                                  field: fn(
            &ooxml_wml::types::CTPPrGeneral,
        )
            -> &Option<Box<ooxml_wml::types::OnOffElement>>,
                                  default: bool|
         -> bool {
            direct
                .or_else(|| style_pprs.iter().find_map(|ppr| check_toggle(field(ppr))))
                .unwrap_or(default)
        };
        let direct_ppr = paragraph.p_pr.as_ref();
        let ea = EaToggles {
            auto_space_de: resolve_ea(
                direct_ppr.and_then(|p| check_toggle(&p.auto_space_d_e)),
                |p| &p.auto_space_d_e,
            ),
            auto_space_dn: resolve_ea(
                direct_ppr.and_then(|p| check_toggle(&p.auto_space_d_n)),
                |p| &p.auto_space_d_n,
            ),
            kinsoku: resolve_ea(direct_ppr.and_then(|p| check_toggle(&p.kinsoku)), |p| &p.kinsoku),
            word_wrap: resolve_ea(
                direct_ppr.and_then(|p| check_toggle(&p.word_wrap)),
                |p| &p.word_wrap,
            ),
        };

        // §17.3.1.6 bidi: RTL paragraph base direction (UAX#9 base level).
        let base_rtl = resolve_ppr_toggle(
            direct_ppr.and_then(|p| check_toggle(&p.bidi)),
            |p| &p.bidi,
            false,
        );
        // The explicit justification (direct pPr, then style/default chain)
        // BEFORE the Start fallback — `None` means the paragraph carries no jc.
        let explicit_jc = paragraph.alignment()
            .or_else(|| resolve_ppr_option(&style_pprs, default_ppr, |ppr| {
                ppr.justification.as_deref().map(|j| j.value)
            }));
        let alignment = {
            let a = explicit_jc.unwrap_or(STJc::Start);
            // §17.3.1.6: under RTL, the start edge is the right margin.
            if base_rtl { rtl_swap_jc(a) } else { a }
        };
        // OMML display-equation centering: a paragraph whose only significant
        // content is a bare `<m:oMath>` / `<m:oMathPara>` is a display equation.
        // Word centers it via the built-in `centerGroup` default math
        // justification even with NO settings.xml mathPr, so an ABSENT jc ⇒
        // center. An explicit w:jc is honored (left/right/center) instead. The
        // centering rides the existing STJc::Center `x_start` branch: the display
        // atom owns its line, so line_width == mathbox.width and the equation box
        // centers on the content width. Gated on `is_display_math_paragraph`, so
        // every non-math paragraph resolves alignment exactly as before.
        let alignment = if explicit_jc.is_none()
            && crate::wml::math::is_display_math_paragraph(paragraph)
        {
            STJc::Center
        } else {
            alignment
        };

        let indent_left_opt = paragraph.indent_left()
            .or_else(|| resolve_ppr_indent(&style_pprs, default_ppr, |ind| {
                ind.start.as_deref().or(ind.left.as_deref()).and_then(|s| s.parse().ok())
            }));
        let indent_left = indent_left_opt.unwrap_or(0);

        let indent_right = paragraph.indent_right()
            .or_else(|| resolve_ppr_indent(&style_pprs, default_ppr, |ind| {
                ind.end.as_deref().or(ind.right.as_deref()).and_then(|s| s.parse().ok())
            }))
            .unwrap_or(0);

        let first_line = paragraph.indent_first_line()
            .or_else(|| resolve_ppr_indent(&style_pprs, default_ppr, |ind| {
                ind.first_line.as_deref().and_then(|s| s.parse().ok())
            }))
            .unwrap_or(0);

        let hanging = paragraph.indent_hanging()
            .or_else(|| resolve_ppr_indent(&style_pprs, default_ppr, |ind| {
                ind.hanging.as_deref().and_then(|s| s.parse().ok())
            }))
            .unwrap_or(0);

        // Each w:spacing attribute inherits *independently*: a direct pPr that
        // sets `after`/`afterAutospacing` but omits `before`/`beforeAutospacing`
        // must still pick up the latter from the style chain. (Earlier this
        // short-circuited on "direct spacing element present" and lost the
        // style's beforeAutospacing, crushing the gap above NormalWeb headings.)
        let direct_spacing = paragraph.p_pr.as_ref().and_then(|ppr| ppr.spacing.as_deref());
        let space_before = direct_spacing
            .and_then(|s| s.before.as_deref()).and_then(|v| v.parse::<i64>().ok())
            .or_else(|| resolve_ppr_option(&style_pprs, default_ppr, |ppr| {
                ppr.spacing.as_deref().and_then(|s| s.before.as_deref()).and_then(|v| v.parse().ok())
            }))
            .unwrap_or(0);
        let before_auto = direct_spacing
            .and_then(|s| s.before_autospacing.as_deref()).map(|v| is_on_off_true(Some(v)))
            .or_else(|| resolve_ppr_option(&style_pprs, default_ppr, |ppr| {
                ppr.spacing.as_deref().and_then(|s| s.before_autospacing.as_deref()).map(|v| is_on_off_true(Some(v)))
            }))
            .unwrap_or(false);
        let space_after = direct_spacing
            .and_then(|s| s.after.as_deref()).and_then(|v| v.parse::<i64>().ok())
            .or_else(|| resolve_ppr_option(&style_pprs, default_ppr, |ppr| {
                ppr.spacing.as_deref().and_then(|s| s.after.as_deref()).and_then(|v| v.parse().ok())
            }))
            .unwrap_or(0);
        let after_auto = direct_spacing
            .and_then(|s| s.after_autospacing.as_deref()).map(|v| is_on_off_true(Some(v)))
            .or_else(|| resolve_ppr_option(&style_pprs, default_ppr, |ppr| {
                ppr.spacing.as_deref().and_then(|s| s.after_autospacing.as_deref()).map(|v| is_on_off_true(Some(v)))
            }))
            .unwrap_or(false);

        // before/afterAutospacing (§17.3.1.32-.33): the literal before/after
        // value is ignored and Word auto-computes the gap to "match the spacing
        // which would be applied to the paragraph in an HTML document where no
        // explicit spacing before/after is specified". In HTML the default `<p>`
        // top/bottom margin is `1em` — i.e. ONE em of the paragraph's OWN font
        // size (CSS `margin: 1em 0`), not a fixed pt value. A 10pt heading gets
        // 10pt of auto-margin; a 14pt heading gets 14pt. Hardcoding a single
        // 14pt (280-twip) constant over-spaced every small-font auto paragraph
        // (a dense stack of 10pt NormalWeb headings each got
        // ~4pt too much before-margin), pushing later rows off the page; using
        // the em of the paragraph's actual font size is the spec-correct HTML
        // emulation and removes that excess where small-font auto paragraphs
        // cluster. The font size is the paragraph mark's resolved size
        // (§17.3.1.29) — the mark rPr, then the paragraph style chain, then the
        // run defaults — matching the glyph HTML would lay the margin around.
        // The auto-margin em uses the paragraph mark's resolved size (§17.3.1.29):
        // mark rPr → paragraph style chain → docDefaults → default — the same rule
        // that sizes the empty-paragraph mark line above.
        let autospace_pt = resolve_mark_size_pt(paragraph, style_ctx);
        // twips = pt * 20; round to the nearest twip.
        let autospace_twips = (autospace_pt * 20.0).round() as i64;
        let space_before = if before_auto { autospace_twips } else { space_before };
        let space_after = if after_auto { autospace_twips } else { space_after };
        // §17.4.80 trHeight hRule="exact": the row is a FIXED-height box that does
        // not grow to fit content; Word positions the cell content within that box
        // by vertical alignment and does NOT let the paragraph's before-spacing
        // push it down/out (verified against Word on calibration doc C1: a
        // NormalWeb title with 1em auto-before sits at the same level as the plain
        // date cells in an hRule=exact header row, not pushed below them). Drop the
        // before-spacing here so it can't shove the text out of the fixed row into
        // the next one. Targeted to exact rows only — auto/atLeast rows keep their
        // spacing, so pagination is unchanged (an exact row's height is fixed
        // regardless of this).
        let space_before = if exact_row { 0 } else { space_before };

        let line_spacing_twips = paragraph.line_spacing()
            .or_else(|| resolve_ppr_option(&style_pprs, default_ppr, |ppr| {
                ppr.spacing.as_deref().and_then(|s| s.line.as_deref()).and_then(|v| v.parse().ok())
            }));


        let line_spacing_rule = paragraph.line_spacing_rule()
            .or_else(|| resolve_ppr_option(&style_pprs, default_ppr, |ppr| {
                ppr.spacing.as_deref().and_then(|s| s.line_rule)
            }))
            .unwrap_or(STLineSpacingRule::Auto);

        let contextual_spacing = resolve_ppr_toggle(
            direct_ppr.and_then(|p| check_toggle(&p.contextual_spacing)),
            |p| &p.contextual_spacing,
            false,
        );

        // §17.3.1.32: snapToGrid defaults on; a paragraph (or its style) can turn
        // it off to opt this paragraph out of the section's document line grid.
        let snaps_to_grid = resolve_ppr_toggle(
            direct_ppr.and_then(|p| check_toggle(&p.snap_to_grid)),
            |p| &p.snap_to_grid,
            true,
        );
        let grid_pitch_px = if snaps_to_grid { section_grid_pitch_px } else { None };

        let style_id = paragraph.p_pr.as_ref()
            .and_then(|ppr| ppr.paragraph_style.as_ref())
            .map(|ps| ps.value.clone());

        let shading_color = paragraph.p_pr.as_ref()
            .and_then(|ppr| ppr.shading.as_ref())
            .and_then(|shd| shd.fill.as_deref())
            .and_then(ooxml_color_to_css)
            .or_else(|| {
                resolve_ppr_option(&style_pprs, default_ppr, |ppr| {
                    ppr.shading.as_ref()
                        .and_then(|shd| shd.fill.as_deref())
                        .and_then(ooxml_color_to_css)
                })
            });

        let pbdr = paragraph.p_pr.as_ref()
            .and_then(|ppr| ppr.paragraph_border.as_ref())
            .or_else(|| {
                for ppr in &style_pprs {
                    if ppr.paragraph_border.is_some() {
                        return ppr.paragraph_border.as_ref();
                    }
                }
                None
            });
        let (border_top, border_bottom, border_left, border_right) = if let Some(bdr) = pbdr {
            (
                resolve_border_edge(bdr.top.as_deref()),
                resolve_border_edge(bdr.bottom.as_deref()),
                resolve_border_edge(bdr.left.as_deref()),
                resolve_border_edge(bdr.right.as_deref()),
            )
        } else {
            (None, None, None, None)
        };

        Self {
            alignment,
            indent_left_px: twips_to_px(indent_left),
            indent_left_explicit: indent_left_opt.is_some(),
            indent_right_px: twips_to_px(indent_right),
            indent_first_line_px: twips_to_px(first_line - hanging),
            space_before_px: twips_to_px(space_before),
            before_auto,
            space_after_px: twips_to_px(space_after),
            line_spacing_twips,
            line_spacing_rule,
            grid_pitch_px,
            contextual_spacing,
            style_id,
            shading_color,
            border_top,
            border_bottom,
            border_left,
            border_right,
            page_break_before: resolve_ppr_toggle(
                direct_ppr.and_then(|p| check_toggle(&p.page_break_before)),
                |p| &p.page_break_before,
                false,
            ),
            keep_next: resolve_ppr_toggle(
                direct_ppr.and_then(|p| check_toggle(&p.keep_next)),
                |p| &p.keep_next,
                false,
            ),
            base_rtl,
            keep_lines: resolve_ppr_toggle(
                direct_ppr.and_then(|p| check_toggle(&p.keep_lines)),
                |p| &p.keep_lines,
                false,
            ),
            widow_control: resolve_ppr_toggle(
                direct_ppr.and_then(|p| check_toggle(&p.widow_control)),
                |p| &p.widow_control,
                true,
            ),
            ea,
        }
    }

    pub(crate) fn x_start(
        &self,
        page_width_px: f64,
        margin_left_px: f64,
        margin_right_px: f64,
        line_width: f64,
        line_indent_extra: f64,
    ) -> f64 {
        let left_edge = margin_left_px + self.indent_left_px + line_indent_extra;
        let right_edge = page_width_px - margin_right_px - self.indent_right_px;
        let content_width = right_edge - left_edge;
        match self.alignment {
            STJc::Center => left_edge + (content_width - line_width) / 2.0,
            STJc::End | STJc::Right => right_edge - line_width,
            _ => left_edge,
        }
    }

    pub(crate) fn available_width(
        &self,
        page_width_px: f64,
        margin_left_px: f64,
        margin_right_px: f64,
        line_indent_extra: f64,
    ) -> f64 {
        let left_edge = margin_left_px + self.indent_left_px + line_indent_extra;
        let right_edge = page_width_px - margin_right_px - self.indent_right_px;
        right_edge - left_edge
    }

    pub(crate) fn line_height(&self, max_font_pt: f64, content_height: f64, image_height: f64) -> f64 {
        let font_px = pt_to_px(max_font_pt);
        // Document line grid (§17.6.5): when this paragraph snaps to a line grid,
        // each line occupies at least the grid pitch, so a fixed number of
        // single-spaced lines fits the page (the §17.15.3.1 figure shows the
        // wider spacing). We snap UP to the pitch (`max`), NOT to a whole
        // multiple (`ceil`) — ceil over-inflates lines taller than the pitch
        // (headings) and was empirically wrong. `Exact` line spacing pins the
        // height and overrides the grid, so it is never snapped. `grid_pitch_px`
        // is None when there is no grid, the section uses Far-East layout (Word
        // leaves Latin text at natural spacing then), the paragraph set
        // snapToGrid off, or the line is in a table cell.
        let snap = |lh: f64| match self.grid_pitch_px {
            Some(p) if p > 0.0 => lh.max(p),
            _ => lh,
        };
        let Some(spacing_twips) = self.line_spacing_twips else {
            // No explicit line rule → single spacing. An image line's
            // content_height folds the image ascent AND the run's font descent;
            // measure the drawing extent as a FLOOR, do not inflate by the
            // descent. Text-only (image_height == 0): text_ch == content_height,
            // so this is byte-identical to (font_px * 1.2).max(content_height).
            let text_ch = (content_height - image_height).max(0.0);
            return snap((font_px * 1.2).max(text_ch).max(image_height));
        };
        match self.line_spacing_rule {
            STLineSpacingRule::Exact => twips_to_px(spacing_twips),
            STLineSpacingRule::AtLeast => snap(twips_to_px(spacing_twips).max(content_height)),
            STLineSpacingRule::Auto => {
                let multiplier = (spacing_twips as f64) / 240.0;
                // §17.3.1.33: `line` is 240ths of a *single line height*, and the
                // single line height is the font's NATURAL line height (the
                // tallest run's ascent+descent), not the em square. `content_height`
                // is exactly that (max_ascent+max_descent); fall back to the em
                // only when no font metrics were available (content_height==0).
                // Previously we multiplied the em, making every auto-spaced line
                // ~13% short and accumulating a large vertical shortfall per page.
                let single = if content_height > 0.0 { content_height } else { font_px };
                // Auto spacing scales the *text* only. An inline image keeps its
                // natural height — Word never stretches an image to e.g. 1.15×
                // via auto line spacing. `content_height` folds the image into
                // the line ascent (for baseline placement), so strip it back out
                // before multiplying and re-add it un-scaled. For text-only lines
                // (image_height == 0) this is identical to `single * multiplier`.
                let text_single = (single - image_height).max(0.0);
                // The drawing extent is a FLOOR the scaled text competes with,
                // NOT an additive base. Word measures an image line at the
                // drawing extent (§17.3.1.33); it never re-adds the auto-scaled
                // residual descent on top of the full image. For text-only lines
                // (image_height == 0) this is byte-identical to `single * multiplier`.
                snap((text_single * multiplier).max(image_height))
            }
        }
    }
}

#[cfg(test)]
mod marker_slot_tests {
    use super::marker_slot_width;

    #[test]
    fn narrow_marker_keeps_hanging_slot() {
        // "1." at ~16px fits a 24px hanging slot → text still aligns at `left`.
        assert_eq!(marker_slot_width(false, 24.0, 16.3), 24.0);
    }

    #[test]
    fn wide_marker_pushes_text_right() {
        // §17.9: "1.1.1" at ~34px exceeds a 12px hanging slot → the slot
        // widens to the marker advance so the text never overlaps the marker.
        assert_eq!(marker_slot_width(false, 12.0, 34.0), 34.0);
    }

    #[test]
    fn empty_marker_takes_no_width() {
        assert_eq!(marker_slot_width(true, 24.0, 16.3), 0.0);
    }

    #[test]
    fn no_hanging_uses_marker_advance() {
        assert_eq!(marker_slot_width(false, 0.0, 16.3), 16.3);
    }
}

#[cfg(test)]
mod line_height_tests {
    use super::*;

    /// Build a `ParagraphLayout` with the given line spacing and inert defaults
    /// for everything else, so we can exercise `line_height` in isolation.
    fn para(line_spacing_twips: Option<i64>, rule: STLineSpacingRule) -> ParagraphLayout {
        ParagraphLayout {
            alignment: STJc::Start,
            base_rtl: false,
            indent_left_px: 0.0,
            indent_left_explicit: false,
            indent_right_px: 0.0,
            indent_first_line_px: 0.0,
            space_before_px: 0.0,
            before_auto: false,
            space_after_px: 0.0,
            line_spacing_twips,
            line_spacing_rule: rule,
            grid_pitch_px: None,
            contextual_spacing: false,
            style_id: None,
            shading_color: None,
            border_top: None,
            border_bottom: None,
            border_left: None,
            border_right: None,
            page_break_before: false,
            keep_next: false,
            keep_lines: false,
            widow_control: true,
            ea: EaToggles::default(),
        }
    }

    #[test]
    fn auto_spacing_does_not_scale_inline_image() {
        // 1.15× auto spacing (line=276) on a line whose height is an inline image.
        // The image (618.8px) folds into content_height via the ascent; it must
        // NOT be stretched to 618.8 × 1.15 = 711.7 (the C2 Figure-1 over-count).
        let p = para(Some(276), STLineSpacingRule::Auto);
        let img = 618.8;
        let lh = p.line_height(18.0, img, img); // content_height == image_height for a pure image line
        assert!((lh - img).abs() < 0.5, "image line should stay at natural height, got {lh}");
    }

    #[test]
    fn auto_spacing_still_scales_text_only_lines() {
        // Regression guard: with no image, behaviour is unchanged (single × multiplier).
        let p = para(Some(276), STLineSpacingRule::Auto);
        let content = 20.0;
        let lh = p.line_height(12.0, content, 0.0);
        assert!((lh - content * 276.0 / 240.0).abs() < 1e-6, "text line height changed: {lh}");
    }

    #[test]
    fn auto_spacing_image_line_does_not_add_text_descent() {
        // Figure-line shape: an inline image line carries a real text run,
        // so content_height folds the image ascent AND the run's font descent.
        // Word measures the DRAWING EXTENT (563.45); Parasma must not add the
        // auto-multiplied descent (→568.66) which tips a full page.
        // Unlike `auto_spacing_does_not_scale_inline_image` above (which passes
        // content_height == image_height, masking the `+ image_height` term with
        // a zero text residual), this drives the REAL C4 condition where the
        // text descent below the image baseline is non-zero.
        let p = para(Some(276), STLineSpacingRule::Auto); // 1.15x
        let img = 563.45;
        let descent = 4.53; // text descent below the image baseline
        let lh = p.line_height(11.0, img + descent, img);
        assert!((lh - img).abs() < 0.5, "image line inflated to {lh}, want {img}");
        // pre-fix: (4.53*1.15 + 563.45)=568.66 → |568.66-563.45|=5.21 > 0.5 → FAILS
        // post-fix: (4.53*1.15).max(563.45)=563.45 → PASSES
    }

    #[test]
    fn no_explicit_spacing_image_line_measures_extent() {
        // None-arm sibling (docDefault w/ no w:spacing line rule): content_height
        // folds the image ascent AND the run's font descent; the line must
        // measure the drawing extent, not inflate by the folded descent.
        let p = para(None, STLineSpacingRule::Auto); // twips None → None arm
        let img = 563.45;
        let lh = p.line_height(11.0, img + 4.53, img);
        assert!((lh - img).abs() < 0.5, "None-arm image line inflated to {lh}");
        // pre-fix: (font*1.2).max(567.98)=567.98 → FAILS; post-fix: max(…,563.45)=563.45 → PASSES
    }

    #[test]
    fn no_explicit_spacing_text_only_unchanged() {
        // None-arm byte-identity guard: image_height==0 must remain
        // (font_px*1.2).max(content_height), unchanged pre AND post fix.
        let p = para(None, STLineSpacingRule::Auto);
        let lh = p.line_height(12.0, 20.0, 0.0);
        let expected = (pt_to_px(12.0) * 1.2).max(20.0);
        assert!((lh - expected).abs() < 1e-6, "None text-only changed: {lh} vs {expected}");
    }

    #[test]
    fn doc_grid_snaps_short_lines_up_to_pitch_but_not_taller_ones() {
        // §17.6.5: a snapped line occupies at least the grid pitch. A short body
        // line (18.8px) snaps up to the 24px (18pt) pitch; a heading (28.8px,
        // already taller) is left alone — NOT ceil'd to 48px.
        let mut p = para(Some(288), STLineSpacingRule::Auto);
        p.grid_pitch_px = Some(24.0);
        let body = p.line_height(10.5, 15.6, 0.0); // 15.6 × 1.2 = 18.72 → snap to 24
        assert!((body - 24.0).abs() < 0.5, "short body line should snap to pitch, got {body}");
        let heading = p.line_height(18.0, 24.0, 0.0); // 24 × 1.2 = 28.8, taller than pitch
        assert!((heading - 28.8).abs() < 0.5, "heading should keep its height, got {heading}");
    }

    #[test]
    fn doc_grid_does_not_override_exact_line_spacing() {
        // Exact spacing pins the height; the grid must not touch it (§17.6.5).
        let mut p = para(Some(200), STLineSpacingRule::Exact); // 10pt = 13.33px
        p.grid_pitch_px = Some(24.0);
        let lh = p.line_height(10.0, 13.0, 0.0);
        assert!((lh - twips_to_px(200)).abs() < 1e-6, "exact spacing must ignore grid: {lh}");
    }
}

pub(crate) fn collect_paragraph_style_chain<'a>(
    paragraph: &ooxml_wml::types::Paragraph,
    ctx: &'a StyleContext,
) -> Vec<&'a ooxml_wml::types::CTPPrGeneral> {
    let style_id = paragraph.p_pr.as_ref()
        .and_then(|ppr| ppr.paragraph_style.as_ref())
        .map(|ps| ps.value.as_str());

    let Some(mut current_id) = style_id.map(str::to_owned) else {
        return Vec::new();
    };

    let mut result = Vec::new();
    let mut depth = 0;
    loop {
        if depth >= 20 { break; }
        let Some(style) = ctx.style(&current_id) else { break };
        if let Some(ref ppr) = style.p_pr {
            result.push(ppr.as_ref());
        }
        match style.based_on.as_ref() {
            Some(base) => current_id = base.value.clone(),
            None => break,
        }
        depth += 1;
    }
    result
}

pub(crate) fn collect_paragraph_style_rprs<'a>(
    paragraph: &ooxml_wml::types::Paragraph,
    ctx: &'a StyleContext,
) -> Vec<&'a ooxml_wml::types::RunProperties> {
    let style_id = paragraph.p_pr.as_ref()
        .and_then(|ppr| ppr.paragraph_style.as_ref())
        .map(|ps| ps.value.as_str());

    let Some(mut current_id) = style_id.map(str::to_owned) else {
        return Vec::new();
    };

    let mut result = Vec::new();
    let mut depth = 0;
    loop {
        if depth >= 20 { break; }
        let Some(style) = ctx.style(&current_id) else { break };
        if let Some(ref rpr) = style.r_pr {
            result.push(rpr.as_ref());
        }
        match style.based_on.as_ref() {
            Some(base) => current_id = base.value.clone(),
            None => break,
        }
        depth += 1;
    }
    result
}

fn is_on_off_true(val: Option<&str>) -> bool {
    matches!(val, Some("1" | "true" | "on"))
}

fn resolve_ppr_option<T>(
    style_pprs: &[&ooxml_wml::types::CTPPrGeneral],
    default_ppr: Option<&ooxml_wml::types::CTPPrGeneral>,
    accessor: impl Fn(&ooxml_wml::types::CTPPrGeneral) -> Option<T>,
) -> Option<T> {
    for ppr in style_pprs {
        if let val @ Some(_) = accessor(ppr) {
            return val;
        }
    }
    default_ppr.and_then(accessor)
}

fn resolve_ppr_indent<T>(
    style_pprs: &[&ooxml_wml::types::CTPPrGeneral],
    default_ppr: Option<&ooxml_wml::types::CTPPrGeneral>,
    accessor: impl Fn(&ooxml_wml::types::CTInd) -> Option<T>,
) -> Option<T> {
    for ppr in style_pprs {
        if let Some(ind) = ppr.indentation.as_deref() {
            if let val @ Some(_) = accessor(ind) {
                return val;
            }
        }
    }
    if let Some(ind) = default_ppr.and_then(|d| d.indentation.as_deref()) {
        return accessor(ind);
    }
    None
}
