//! Line breaking & measurement (§17.3.1 paragraph layout — the UAX#14 line
//! breaker + glyph measurement half). Verbatim relocation from `lib.rs`:
//! the run→item collection state machine (`collect_items` with its complex-
//! field/tracked-change/hyperlink/bookmark handling), the shared line types
//! (`Item`/`LineSegment`/`LayoutLine`), and the line packer (`pack_lines`,
//! `justify_line`). Tab-stop *resolution* (`ResolvedTabStops`) and paragraph
//! layout (`layout_paragraph`) deliberately STAY in `lib.rs` for now (they go
//! to `wml/paragraph.rs` in a later round); this module imports
//! `ResolvedTabStops`/`TabAlignment`/`TabLeader` via `crate::`. The float-aware
//! packer (`pack_lines_around_floats`) lives in the sibling `wml/float.rs`,
//! which imports `Item`/`LineSegment`/`LayoutLine`/`justify_line` from here via
//! `crate::`. `bidi_reorder_line` is imported from `wml::bidi`.
//!
//! Purity is enforced by the oracle snapshot tests — their geometry hashes
//! must not drift on a pure move.

use smallvec::SmallVec;
use wasm_bindgen::JsValue;

use ooxml_wml::ext::{DrawingChartExt, DrawingExt, RunExt, StyleContext};
use ooxml_wml::types::CTTxbxContent;

use crate::engine::fonts::{
    font_metrics, font_slot_class, resolve_font_slot, FaceCache, FontCache, FontSlot, ShapedRun,
    ThemeFonts, DEFAULT_FONT_PT,
};
use crate::engine::paint::{RunStyle, Underline, VertAlign};
use crate::engine::units::{emu_to_px, pt_to_px};
use crate::wml::bidi::bidi_reorder_line;
use crate::wml::drawing::{
    extract_drawing_text_box, extract_pict_text_box, extract_vml_dimensions, inline_dist_px,
    is_layout_control, parse_anchor_position, WrapInfo,
};
use crate::wml::float::{AnchorHRelFrom, AnchorPos, AnchorVRelFrom};
use crate::{
    collect_paragraph_style_rprs, field, PagedLine,
    ResolvedTabStops, TabAlignment, TabLeader, DEFAULT_TEXT_COLOR, FIELD_DATES,
    HYPERLINK_RELS, RESERVE_TRAILING_BLANKS,
};

#[derive(Clone)]
pub(crate) struct TabInfo {
    // Retained alongside `leader`; the align is resolved upstream into the
    // packed tab X, so the per-item copy is currently unread.
    #[allow(dead_code)]
    alignment: TabAlignment,
    pub(crate) leader: TabLeader,
}

pub(crate) struct AnchoredTextBoxInfo {
    pub(crate) raw: CTTxbxContent,
    pub(crate) width_px: f64,
    pub(crate) height_px: f64,
    pub(crate) pos_h: AnchorPos,
    pub(crate) pos_v: AnchorPos,
    pub(crate) h_relative_from: AnchorHRelFrom,
    pub(crate) v_relative_from: AnchorVRelFrom,
    pub(crate) behind_text: bool,
    pub(crate) fill_color: Option<String>,
    pub(crate) border_color: Option<String>,
    pub(crate) border_width_px: f64,
    pub(crate) wrap: WrapInfo,
}

pub(crate) struct AnchoredImageInfo {
    pub(crate) rel_id: String,
    pub(crate) width_px: f64,
    pub(crate) height_px: f64,
    pub(crate) pos_h: AnchorPos,
    pub(crate) pos_v: AnchorPos,
    pub(crate) h_relative_from: AnchorHRelFrom,
    pub(crate) v_relative_from: AnchorVRelFrom,
    pub(crate) behind_text: bool,
    pub(crate) wrap: WrapInfo,
}

/// One run queued for the item-emit pass: the run plus its (field tag, hyperlink
/// target, text override, tracked-change tag, open-comment-id snapshot) context
/// (§17.16 fields / §17.13.4 comments). Named fields close the tuple hazard: the
/// two adjacent `Option<String>` slots (`link` vs `text_override`) were
/// position-swappable and silently miscarry if crossed.
struct RunEntry<'a> {
    run: &'a ooxml_wml::types::Run,
    field: Option<FieldCode>,
    link: Option<String>,
    text_override: Option<String>,
    track_change: Option<TrackChange>,
    comment_ids: CommentIds,
}

/// A queued paragraph-content atom, in document order: either a text run
/// (`RunEntry`) or an OMML math atom seated between runs by its parser child
/// position (see the interleave in `collect_items`). Math-free paragraphs
/// produce only `Run` variants, so the emit stream is byte-identical.
enum Pending<'a> {
    Run(RunEntry<'a>),
    Math(Box<crate::wml::math::MathBox>),
}

impl<'a> RunEntry<'a> {
    /// A plain queued run carrying only its comment-range snapshot; the four
    /// field/link/override/track-change slots default to `None`. Each call site
    /// then sets just the one slot it needs via struct-update syntax.
    fn new(run: &'a ooxml_wml::types::Run, comment_ids: CommentIds) -> Self {
        RunEntry {
            run,
            field: None,
            link: None,
            text_override: None,
            track_change: None,
            comment_ids,
        }
    }
}

/// `collect_items` output: (items, anchored images, anchored text boxes,
/// footnote ids, endnote ids) — one paragraph's shaped content stream.
type CollectedItems = (
    Vec<Item>,
    Vec<AnchoredImageInfo>,
    Vec<AnchoredTextBoxInfo>,
    Vec<i64>,
    Vec<i64>,
);

/// Walk a paragraph's runs, find UAX #14 break opportunities, emit
/// break-bounded items with measured widths. Known limits: a token with no
/// break opportunity overflows the line; see also the ⚠️ gotcha about terminal
/// Mandatory markers.
pub(crate) fn collect_items(
    font_cache: &FontCache,
    paragraph: &ooxml_wml::types::Paragraph,
    style_ctx: &StyleContext,
    theme_fonts: &ThemeFonts,
    table_style_rprs: &[&ooxml_wml::types::RunProperties],
) -> Result<CollectedItems, JsValue> {
    let mut items = Vec::new();
    // Running byte offset into the paragraph LOGICAL text.
    // Advanced ONLY by the text-bearing pushes (the split `'\t'` synthesized
    // item and each UAX#14 segment), in emit = logical order. Every other push
    // (image/chart/textbox/footnote-ref/endnote-ref/page-break/bookmark) stamps
    // `CHAR_START_SENTINEL` and leaves the cursor untouched, so following text
    // items keep contiguous logical offsets.
    let mut byte_cursor: u32 = 0;
    let mut anchored_images = Vec::new();
    let mut anchored_text_boxes: Vec<AnchoredTextBoxInfo> = Vec::new();
    let mut footnote_ids: Vec<i64> = Vec::new();
    let mut endnote_ids: Vec<i64> = Vec::new();
    // Shaping scope — one built `rustybuzz::Face` per (font, paragraph) instead of one
    // per shaped segment. Scoped to this call, so it cannot outlive the
    // `font_cache` borrow its keys are addresses into (see `FaceCache`).
    let faces = FaceCache::new();
    let para_style_rprs = collect_paragraph_style_rprs(paragraph, style_ctx);

    // (run, page-field tag, link target, text override, tracked-change tag,
    // open-comment-id snapshot). link target: Some("url:…"|"anchor:…") for
    // hyperlink runs (also drives hyperlink styling); None otherwise. The last
    // element is the set of comment ranges open at this run's document position
    // — snapshotted during the content walk so the item-emit pass can
    // stamp `Item::comment_ids` without re-deriving range membership.
    let mut runs_to_process: Vec<Pending> = Vec::new();
    let doc_dates = FIELD_DATES.with(|d| d.get());
    // Complex-field state machine (§17.16.18). Complex fields span multiple
    // runs: fldChar(begin) → instrText… → fldChar(separate) → result runs →
    // fldChar(end). A stack handles nested fields. Marker and instruction runs
    // never render; the first result run of a recognized live field (PAGE/
    // NUMPAGES) is tagged with its FieldCode for render-time substitution, and
    // further result runs of that field are suppressed. Unrecognized fields
    // pass their cached result text through unchanged.
    // Collect runs from a tracked-change wrapper's RunContentChoice list,
    // tagging each with the change (insertion/deletion). Insertions nested in
    // deletions (moved text) keep the outer deletion tag — simplification.
    // `open` is the comment-range snapshot at the wrapper's position; every run
    // inside the tracked change inherits it (comment ranges that open/close
    // strictly inside an ins/del are not separately tracked — a v1 simplification
    // matching the moved-text tag inheritance above).
    fn collect_tc_runs<'a>(
        items: &'a [ooxml_wml::types::RunContentChoice],
        tag: &TrackChange,
        open: &CommentIds,
        out: &mut Vec<Pending<'a>>,
    ) {
        for it in items {
            match it {
                ooxml_wml::types::RunContentChoice::R(r) => {
                    out.push(Pending::Run(RunEntry {
                        track_change: Some(tag.clone()),
                        ..RunEntry::new(r, open.clone())
                    }));
                }
                ooxml_wml::types::RunContentChoice::Ins(c)
                | ooxml_wml::types::RunContentChoice::Del(c) => {
                    collect_tc_runs(&c.run_content, tag, open, out);
                }
                _ => {}
            }
        }
    }

    // Bookmark names declared in this paragraph (internal-link anchor targets).
    // Anchored at paragraph-first-line granularity (see Item::bookmark).
    let mut para_bookmarks: Vec<String> = Vec::new();
    // Comment ranges open at the layout cursor, seeded from the cross-paragraph
    // carrier so a range opened in an earlier paragraph is still active here
    // (§17.13.4 ranges span paragraphs). Mutated in document order by
    // `commentRangeStart`/`End` below and snapshotted onto each run; written
    // back to the thread-local at the end so the NEXT paragraph inherits it.
    let mut open_comments: CommentIds = OPEN_COMMENT_IDS.with(|c| c.borrow().iter().copied().collect());
    let snapshot = |open: &CommentIds| -> CommentIds { open.clone() };
    let mut field_stack: Vec<FieldFrame> = Vec::new();

    // ── OMML math interleave (§22.1) ──────────────────────────────────────────
    // Math atoms live in `paragraph.extra_children` as PositionedNodes keyed by
    // the parser's paragraph-child index (`child_idx`): it increments once for a
    // leading `w:pPr`, once per `paragraph_content` element AND once per
    // extra-child, stamping the math node's `.position`. To seat an atom between
    // the runs it fell between in document order we reconstruct each content
    // item's own `child_idx` by free-slot assignment (skipping the pPr slot and
    // every extra-child slot) — NOT the naive content-only ordinal, which
    // mis-seats by +1 whenever a pPr is present (the common case). Math-free
    // paragraphs have empty `extra_children` → zero atoms → byte-identical stream.
    let mut math_boxes: std::collections::VecDeque<(usize, Box<crate::wml::math::MathBox>)> =
        std::collections::VecDeque::new();
    let mut content_child_idx: Vec<usize> = Vec::new();
    if !paragraph.extra_children.is_empty() {
        let empty_run = ooxml_wml::types::Run::default();
        let base_math_style = RunStyle::from_resolved(
            &empty_run, style_ctx, &para_style_rprs, table_style_rprs, font_cache, theme_fonts,
        );
        let collected =
            crate::wml::math::collect_paragraph_math(paragraph, &base_math_style, font_cache);
        if !collected.is_empty() {
            math_boxes = collected.into_iter().collect();
            let extra_positions: std::collections::BTreeSet<usize> =
                paragraph.extra_children.iter().map(|c| c.position).collect();
            let has_ppr = paragraph.p_pr.is_some();
            let n_content = paragraph.paragraph_content.len();
            content_child_idx.reserve(n_content);
            let mut child = 0usize;
            let mut ppr_pending = has_ppr;
            while content_child_idx.len() < n_content {
                if extra_positions.contains(&child) {
                    child += 1;
                    continue;
                }
                if ppr_pending {
                    ppr_pending = false;
                    child += 1;
                    continue;
                }
                content_child_idx.push(child);
                child += 1;
            }
        }
    }

    for (pc_ord, content) in paragraph.paragraph_content.iter().enumerate() {
        // Drain any math atom seated BEFORE this content item (its parser
        // position precedes this content's reconstructed child index). Runs of a
        // multi-run content (hyperlink) share one child_idx, so the drain before
        // the first is idempotent for the rest.
        if !math_boxes.is_empty() {
            let limit = content_child_idx[pc_ord];
            while math_boxes.front().is_some_and(|(pos, _)| *pos < limit) {
                let (_, mb) = math_boxes.pop_front().unwrap();
                runs_to_process.push(Pending::Math(mb));
            }
        }
        match content {
            ooxml_wml::types::ParagraphContent::BookmarkStart(b)
                if !b.name.is_empty() && b.name != "_GoBack" => {
                    para_bookmarks.push(b.name.clone());
                }
            ooxml_wml::types::ParagraphContent::R(r) => {
                // fldChar marker run → drive the state machine, never render.
                if let Some(fc_type) = run_fld_char_type(r) {
                    match fc_type {
                        ooxml_wml::types::STFldCharType::Begin => {
                            field_stack.push(FieldFrame::default());
                        }
                        ooxml_wml::types::STFldCharType::Separate => {
                            if let Some(frame) = field_stack.last_mut() {
                                frame.in_result = true;
                                frame.code = resolve_field_code(&frame.instr);
                                if frame.code.is_none() {
                                    frame.static_value =
                                        field::resolve_doc_field(&frame.instr, &doc_dates);
                                }
                            }
                        }
                        ooxml_wml::types::STFldCharType::End => {
                            field_stack.pop();
                        }
                    }
                    continue;
                }
                // instrText run → accumulate instruction, never render.
                if let Some(instr) = run_instr_text(r) {
                    if let Some(frame) = field_stack.last_mut() {
                        if !frame.in_result {
                            frame.instr.push_str(&instr);
                        }
                    }
                    continue;
                }
                // Ordinary run: plain text, or a field's result region.
                if let Some(frame) = field_stack.last_mut() {
                    if frame.in_result {
                        if let Some(code) = frame.code {
                            if !frame.emitted {
                                frame.emitted = true;
                                runs_to_process.push(Pending::Run(RunEntry {
                                    field: Some(code),
                                    ..RunEntry::new(r, snapshot(&open_comments))
                                }));
                            }
                        } else if let Some(val) = frame.static_value.clone() {
                            if !frame.emitted {
                                frame.emitted = true;
                                runs_to_process.push(Pending::Run(RunEntry {
                                    text_override: Some(val),
                                    ..RunEntry::new(r, snapshot(&open_comments))
                                }));
                            }
                        } else {
                            runs_to_process.push(Pending::Run(RunEntry::new(r, snapshot(&open_comments))));
                        }
                    }
                    // else: instruction-region run without instrText — suppress
                    continue;
                }
                runs_to_process.push(Pending::Run(RunEntry::new(r, snapshot(&open_comments))));
            }
            // Tracked insertion / deletion (§17.13.5.18/.14): render the runs
            // with redline styling. Inserted text is part of the document and
            // MUST render; previously these fell through and were dropped.
            ooxml_wml::types::ParagraphContent::Ins(c) => {
                collect_tc_runs(&c.run_content, &TrackChange::Inserted(c.author.clone()), &open_comments, &mut runs_to_process);
            }
            ooxml_wml::types::ParagraphContent::Del(c) => {
                collect_tc_runs(&c.run_content, &TrackChange::Deleted(c.author.clone()), &open_comments, &mut runs_to_process);
            }
            ooxml_wml::types::ParagraphContent::FldSimple(fld) => {
                let field = resolve_field_code(&fld.instr);
                let static_value = if field.is_none() {
                    field::resolve_doc_field(&fld.instr, &doc_dates)
                } else {
                    None
                };
                let mut injected = false;
                for child in &fld.paragraph_content {
                    if let ooxml_wml::types::ParagraphContent::R(r) = child {
                        if let Some(ref v) = static_value {
                            if !injected {
                                injected = true;
                                runs_to_process.push(Pending::Run(RunEntry {
                                    text_override: Some(v.clone()),
                                    ..RunEntry::new(r, snapshot(&open_comments))
                                }));
                            }
                        } else {
                            runs_to_process.push(Pending::Run(RunEntry {
                                field,
                                ..RunEntry::new(r, snapshot(&open_comments))
                            }));
                        }
                    }
                }
            }
            ooxml_wml::types::ParagraphContent::Hyperlink(h) => {
                // Resolve the link target: external URL (r:id → relationship)
                // or internal anchor (w:anchor → bookmark name). Carried on each
                // run so the placement pass can emit clickable rects.
                let target = if let Some(ref rid) = h.id {
                    HYPERLINK_RELS.with(|m| m.borrow().get(rid).map(|u| format!("url:{u}")))
                } else {
                    h.anchor.as_ref().map(|a| format!("anchor:{a}"))
                };
                for child in &h.paragraph_content {
                    if let ooxml_wml::types::ParagraphContent::R(r) = child {
                        runs_to_process.push(Pending::Run(RunEntry {
                            link: target.clone(),
                            ..RunEntry::new(r, snapshot(&open_comments))
                        }));
                    }
                }
            }
            ooxml_wml::types::ParagraphContent::Sdt(sdt) => {
                if let Some(ref content) = sdt.sdt_content {
                    for child in &content.paragraph_content {
                        if let ooxml_wml::types::ParagraphContent::R(r) = child {
                            runs_to_process.push(Pending::Run(RunEntry::new(r, snapshot(&open_comments))));
                        }
                    }
                }
            }
            // Comment range start (§17.13.4.4). Stamp a synthetic scroll anchor
            // (`comment_anchor_name(id)`) onto this paragraph's first line via the
            // existing bookmark machinery, so `comments()` can resolve each
            // range's start to a `{page, y}` with the `bookmark_position` pattern
            // pattern. It also OPENS the range here: its id joins the
            // open set, so every subsequent run on this and following paragraphs
            // is stamped with it until the matching `commentRangeEnd`.
            ooxml_wml::types::ParagraphContent::CommentRangeStart(c) => {
                para_bookmarks.push(comment_anchor_name(c.id));
                if !open_comments.contains(&c.id) {
                    open_comments.push(c.id);
                }
            }
            // Comment range end (§17.13.4.3) closes the range: drop its id from
            // the open set so following runs are no longer stamped with it.
            ooxml_wml::types::ParagraphContent::CommentRangeEnd(c) => {
                open_comments.retain(|id| *id != c.id);
            }
            _ => {}
        }
    }

    // Trailing math atoms (position at/after the last content child, or a
    // math-only paragraph) — emit in document order after every run.
    while let Some((_, mb)) = math_boxes.pop_front() {
        runs_to_process.push(Pending::Math(mb));
    }

    // Carry any still-open comment ranges to the next paragraph (a range whose
    // `commentRangeEnd` is in a later paragraph stays open). Single-threaded
    // WASM, so the thread-local is the natural cross-call carrier.
    OPEN_COMMENT_IDS.with(|c| {
        let mut b = c.borrow_mut();
        b.clear();
        b.extend(open_comments.iter().copied());
    });

    for pending in &runs_to_process {
        // Math atom: one unbreakable inline Item mirroring the image/text_box
        // payload path. `text` empty; width = the atom's advance; break allowed
        // both sides; `char_start` stays the Default sentinel (not born of
        // paragraph text), so select/justify skip it cleanly.
        let entry = match pending {
            Pending::Run(e) => e,
            Pending::Math(mbox) => {
                items.push(Item {
                    math: Some(mbox.clone()),
                    width: mbox.width,
                    content_width: mbox.width,
                    break_after: BreakAfter::Allowed,
                    ..Default::default()
                });
                continue;
            }
        };
        let RunEntry {
            run,
            field: field_code,
            link,
            text_override,
            track_change,
            comment_ids: run_comment_ids,
        } = entry;
        let text = text_override.clone().unwrap_or_else(|| {
            // Deleted runs store their text in <w:delText>, which Run::text()
            // skips. In All-Markup view that text must still render (struck
            // through), so pull delText for deletions.
            if matches!(track_change, Some(TrackChange::Deleted(_))) {
                run_text_including_del(run)
            } else {
                run.text()
            }
        });
        let has_page_break = run.has_page_break();
        let drawings = run.drawings();

        let has_pict = run.run_content.iter().any(|rc| matches!(rc, ooxml_wml::types::RunContent::Pict(_)));
        let has_footnote_ref = run.footnote_ref().is_some();
        let has_endnote_ref = run.endnote_ref().is_some();
        let has_content = !text.is_empty() || has_page_break || !drawings.is_empty() || has_pict || has_footnote_ref || has_endnote_ref;
        if !has_content {
            continue;
        }

        // Index where this run's items begin; used to stamp the link target
        // onto every item produced for a hyperlink run (see end of loop body).
        let link_start = items.len();

        let mut style = RunStyle::from_resolved(run, style_ctx, &para_style_rprs, table_style_rprs, font_cache, theme_fonts);
        if link.is_some() {
            if style.color == DEFAULT_TEXT_COLOR {
                style.color = "#0563C1".to_owned();
            }
            if style.underline == Underline::None {
                style.underline = Underline::Single;
            }
        }
        // Tracked-change redline styling (§17.13.5): insertion → author color
        // + underline; deletion → author color + strikethrough. "All Markup".
        match track_change {
            Some(TrackChange::Inserted(author)) => {
                style.color = author_color(author).to_owned();
                if style.underline == Underline::None {
                    style.underline = Underline::Single;
                }
            }
            Some(TrackChange::Deleted(author)) => {
                style.color = author_color(author).to_owned();
                style.strikethrough = true;
            }
            None => {}
        }

        if style.vanish {
            continue;
        }

        if style.small_caps {
            style.font_pt -= 2.0_f64.max(0.0);
        }

        let font = font_cache.resolve(
            &style.family,
            if style.bold { 700 } else { 400 },
            style.italic,
        );

        let text = if style.all_caps || style.small_caps {
            text.to_uppercase()
        } else {
            text
        };
        let tab_chunks: Vec<&str> = text.split('\t').collect();
        for (chunk_idx, chunk) in tab_chunks.iter().enumerate() {
            if chunk_idx > 0 {
                items.push(Item {
                    text: "\t".to_string(),
                    style: style.clone(),
                    tab: Some(TabInfo {
                        alignment: TabAlignment::Left,
                        leader: TabLeader::None,
                    }),
                    // The synthesized tab is one logical byte ('\t') of the
                    // paragraph text (it was split out of `text` at a '\t').
                    char_start: byte_cursor,
                    char_len: 1,
                    ..Default::default()
                });
                byte_cursor += 1;
            }

            if chunk.is_empty() {
                continue;
            }

            let mut last_emit_idx = 0;
            for (idx, break_class) in unicode_linebreak::linebreaks(chunk) {
                let segment = &chunk[last_emit_idx..idx];
                let last_char = segment.chars().last();
                let raw_mandatory =
                    matches!(break_class, unicode_linebreak::BreakOpportunity::Mandatory);
                let ends_with_newline = matches!(last_char, Some('\n') | Some('\r'));
                let real_mandatory = raw_mandatory && ends_with_newline;

                // Emit an item at EVERY Unicode line-break opportunity (UAX #14),
                // not only whitespace ones — OOXML defers line breaking to the
                // Unicode algorithm. Previously a token only became breakable at a
                // space, so a hyphenated word ("Post-Vacancy") or unspaced CJK run
                // was one unbreakable item that overran its column. Each item keeps
                // break_after=Allowed, so the packer may wrap between them; justify
                // is unaffected (it only stretches trailing-whitespace items).
                if segment.is_empty() {
                    continue;
                }

                let visible: String = segment.chars().filter(|c| !is_layout_control(*c)).collect();
                // §17.3.2.26: choose this segment's font slot from its first
                // strongly-classified char (EastAsia/ComplexScript), defaulting
                // to the run's ascii/default family. Re-resolve the face only
                // when the slot differs — pure-Latin runs reuse `font` for free.
                let seg_slot = visible.chars()
                    .map(|c| resolve_font_slot(font_slot_class(c), style.run_is_complex))
                    .find(|s| matches!(s, FontSlot::EastAsia | FontSlot::ComplexScript))
                    .unwrap_or(FontSlot::Ascii);
                let seg_family: &str = match seg_slot {
                    FontSlot::EastAsia => &style.family_east_asia,
                    FontSlot::ComplexScript => &style.family_complex,
                    _ => &style.family,
                };
                let seg_font = if seg_family == style.family {
                    font
                } else {
                    font_cache.resolve(seg_family, if style.bold { 700 } else { 400 }, style.italic)
                };
                let mut seg_style = style.clone();
                seg_style.family = seg_family.to_string();
                let (width, content_width, shaped) = if visible.is_empty() {
                    (0.0, 0.0, None)
                } else if let Some(mut run) = seg_font.and_then(|f| faces.shape(&visible, f, seg_style.font_pt)) {
                    if seg_style.char_spacing_px.abs() > 0.001 {
                        let n = run.glyphs.len();
                        for g in &mut run.glyphs {
                            g.x_advance += seg_style.char_spacing_px;
                        }
                        run.total_advance += seg_style.char_spacing_px * n as f64;
                        run.is_simple = false;
                    }
                    let cw = run.total_advance - run.trailing_ws_advance();
                    (run.total_advance, cw, Some(run))
                } else {
                    let char_count = visible.chars().count() as f64;
                    let estimated = char_count * pt_to_px(seg_style.font_pt) * 0.5 + char_count * seg_style.char_spacing_px;
                    (estimated, estimated, None)
                };
                items.push(Item {
                    text: segment.to_string(),
                    style: seg_style,
                    width,
                    content_width,
                    break_after: if real_mandatory {
                        BreakAfter::Mandatory
                    } else {
                        BreakAfter::Allowed
                    },
                    shaped,
                    field: *field_code,
                    // Logical span = this segment's OWN byte length at stamp
                    // time (robust to later PAGE/NUMPAGES mutation).
                    char_start: byte_cursor,
                    char_len: segment.len() as u32,
                    ..Default::default()
                });
                byte_cursor += segment.len() as u32;
                last_emit_idx = idx;
            }
        }

        for drawing in &drawings {
            let inline_rel_ids = drawing.inline_image_rel_ids();
            if !inline_rel_ids.is_empty() {
                let (width_px, height_px) = match drawing.extent_emu() {
                    Some((cx, cy)) => (emu_to_px(cx), emu_to_px(cy)),
                    None => (200.0, 150.0),
                };
                // §20.4.2.8 distL/distT/distR/distB (EMU, ST_WrapDistance
                // §20.4.3.6): the text-distance padding Word maintains around
                // an inline image. distL+distR widen the item box so adjacent
                // text keeps its horizontal gap; distT/distB grow the line's
                // vertical extent (added below in `push`). Default 0.
                let (dist_l_px, dist_t_px, dist_r_px, dist_b_px) = inline_dist_px(drawing);
                let box_width = width_px + dist_l_px + dist_r_px;
                for rel_id in inline_rel_ids {
                    items.push(Item {
                        style: style.clone(),
                        width: box_width,
                        content_width: box_width,
                        image: Some(ImageItem {
                            rel_id: rel_id.to_string(),
                            height_px,
                            dist_l_px,
                            dist_t_px,
                            dist_r_px,
                            dist_b_px,
                        }),
                        // Not born of paragraph text — sentinel.
                        ..Default::default()
                    });
                }
            }

            let chart_rel_ids = drawing.all_chart_rel_ids();
            for chart_rel_id in &chart_rel_ids {
                let fallback_key = format!("chart::{}", chart_rel_id);
                let (cw, ch) = match drawing.extent_emu() {
                    Some((cx, cy)) => (emu_to_px(cx), emu_to_px(cy)),
                    None => (400.0, 300.0),
                };
                items.push(Item {
                    style: style.clone(),
                    width: cw,
                    content_width: cw,
                    image: Some(ImageItem {
                        rel_id: fallback_key,
                        height_px: ch,
                        dist_l_px: 0.0,
                        dist_t_px: 0.0,
                        dist_r_px: 0.0,
                        dist_b_px: 0.0,
                    }),
                    // Not born of paragraph text — sentinel.
                    ..Default::default()
                });
            }

            let anchor_rel_ids = drawing.anchored_image_rel_ids();
            if !anchor_rel_ids.is_empty() {
                let (aw, ah) = match drawing.extent_emu() {
                    Some((cx, cy)) => (emu_to_px(cx), emu_to_px(cy)),
                    None => (200.0, 150.0),
                };
                let (h_pos, v_pos, h_rel, v_rel, behind, wrap) = parse_anchor_position(drawing, aw, ah);
                for rel_id in anchor_rel_ids {
                    anchored_images.push(AnchoredImageInfo {
                        rel_id: rel_id.to_string(),
                        width_px: aw,
                        height_px: ah,
                        pos_h: h_pos.clone(),
                        pos_v: v_pos.clone(),
                        h_relative_from: h_rel,
                        v_relative_from: v_rel,
                        behind_text: behind,
                        wrap: wrap.clone(),
                    });
                }
            }

            // Text boxes: walk extra_children to find <wps:wsp> → <wps:txbx> → <w:txbxContent>
            if let Some(etb) = extract_drawing_text_box(drawing) {
                let (tw, th) = match drawing.extent_emu() {
                    Some((cx, cy)) => (emu_to_px(cx), emu_to_px(cy)),
                    None => (200.0, 100.0),
                };
                if etb.is_anchored {
                    let (h_pos, v_pos, h_rel, v_rel, behind, wrap) = parse_anchor_position(drawing, tw, th);
                    anchored_text_boxes.push(AnchoredTextBoxInfo {
                        raw: etb.content,
                        width_px: tw,
                        height_px: th,
                        pos_h: h_pos,
                        pos_v: v_pos,
                        h_relative_from: h_rel,
                        v_relative_from: v_rel,
                        behind_text: behind,
                        fill_color: etb.fill_color,
                        border_color: etb.border_color,
                        border_width_px: etb.border_width_px,
                        wrap,
                    });
                } else {
                    items.push(Item {
                        style: style.clone(),
                        width: tw,
                        content_width: tw,
                        text_box: Some(TextBoxContent {
                            raw: Some(etb.content),
                            lines: Vec::new(),
                            width_px: tw,
                            height_px: th,
                            fill_color: etb.fill_color,
                            border_color: etb.border_color,
                            border_width_px: etb.border_width_px,
                        }),
                        // Not born of paragraph text — sentinel.
                        ..Default::default()
                    });
                }
            }
        }

        // VML text boxes from RunContent::Pict
        for rc in &run.run_content {
            if let ooxml_wml::types::RunContent::Pict(pict) = rc {
                if let Some(etb) = extract_pict_text_box(pict) {
                    let (tw, th) = extract_vml_dimensions(pict).unwrap_or((200.0, 100.0));
                    items.push(Item {
                        style: style.clone(),
                        width: tw,
                        content_width: tw,
                        text_box: Some(TextBoxContent {
                            raw: Some(etb.content),
                            lines: Vec::new(),
                            width_px: tw,
                            height_px: th,
                            fill_color: etb.fill_color,
                            border_color: etb.border_color,
                            border_width_px: etb.border_width_px,
                        }),
                        // Not born of paragraph text — sentinel.
                        ..Default::default()
                    });
                }
            }
        }

        // Footnote references → superscript number
        //
        // ⚠️ DEVIATION (found by the oracle ITEM stream on its first run, NOT fixed
        // here): `footnote_ids` is local to `collect_items`, which runs ONCE PER
        // PARAGRAPH, so this counter restarts at 1 for every paragraph. A document
        // whose footnotes sit in separate paragraphs — the normal case — paints
        // the mark "1" for every one of them. §17.11.14 `w:numRestart` defaults to
        // `continuous`, i.e. Word numbers footnotes across the WHOLE document, so
        // the correct source is a document-scoped counter (or the resolved
        // footnote order), not a paragraph-local `len()`.
        //
        // Left as-is deliberately: the fix changes painted text AND (via
        // `fn_width`, which scales with `fn_num.len()`) line geometry, so it owes
        // its own change with its own re-baseline. The oracle-objects golden
        // records the wrong marks TODAY — three `fn1/fn2/fn3` items all carrying
        // text "1" — so the fix will show up as a reviewable `"1"→"2"` diff
        // instead of passing unnoticed, which is the whole point of the stream.
        if let Some(fnref) = run.footnote_ref() {
            let fn_id: i64 = fnref.id;
            if fn_id > 0 {
                footnote_ids.push(fn_id);
                let fn_num = footnote_ids.len().to_string();
                let mut fn_style = style.clone();
                fn_style.vert_align = VertAlign::Superscript;
                fn_style.font_pt = style.font_pt * 0.65;
                let fn_width = pt_to_px(fn_style.font_pt) * 0.5 * fn_num.len() as f64;
                items.push(Item {
                    text: fn_num,
                    style: fn_style,
                    width: fn_width,
                    content_width: fn_width,
                    footnote_id: Some(fn_id),
                    // Not born of paragraph text — sentinel.
                    ..Default::default()
                });
            }
        }

        // Endnote references → superscript number (roman numerals by convention, but simple numbers for now)
        if let Some(enref) = run.endnote_ref() {
            let en_id: i64 = enref.id;
            if en_id > 0 {
                endnote_ids.push(en_id);
                let en_num = endnote_ids.len().to_string();
                let mut en_style = style.clone();
                en_style.vert_align = VertAlign::Superscript;
                en_style.font_pt = style.font_pt * 0.65;
                let en_width = pt_to_px(en_style.font_pt) * 0.5 * en_num.len() as f64;
                items.push(Item {
                    text: en_num,
                    style: en_style,
                    width: en_width,
                    content_width: en_width,
                    endnote_id: Some(en_id),
                    // Not born of paragraph text — sentinel.
                    ..Default::default()
                });
            }
        }

        if has_page_break {
            items.push(Item {
                style: style.clone(),
                break_after: BreakAfter::PageBreak,
                // Not born of paragraph text — sentinel.
                ..Default::default()
            });
        }

        // Stamp the hyperlink target onto every item produced for this run.
        if let Some(target) = link {
            for it in &mut items[link_start..] {
                it.link = Some(target.clone());
            }
        }
        // Stamp the open comment-range ids onto every item produced for this run
        // — the same post-hoc `link_start..` stamp as the hyperlink target
        // — purely additive, no width/break change, so the geometry hot loop is
        // untouched. `comment_rects` pairs/overlaps these post-layout.
        if !run_comment_ids.is_empty() {
            for it in &mut items[link_start..] {
                it.comment_ids = run_comment_ids.clone();
            }
        }
    }

    // Anchor the paragraph's bookmark(s) to its first line. The first item
    // carries the first bookmark; extras (and the empty-paragraph case) get
    // zero-width marker items prepended at the paragraph start, so every anchor
    // name still resolves to a scroll position on the paragraph's first line.
    for (i, name) in para_bookmarks.into_iter().enumerate() {
        if i == 0 {
            if let Some(first) = items.first_mut() {
                first.bookmark = Some(name);
                continue;
            }
        }
        items.insert(0, Item {
            bookmark: Some(name),
            // Bookmark anchor marker — sentinel.
            ..Default::default()
        });
    }

    Ok((items, anchored_images, anchored_text_boxes, footnote_ids, endnote_ids))
}

// ─── East Asian refinement toggles ──────────────────────────────────────────

/// The paragraph's East Asian refinement toggles, resolved through the pPr →
/// style-chain → docDefaults cascade in `ParagraphLayout::from_resolved`.
/// Every toggle DEFAULTS TRUE when never specified: autoSpaceDE §17.3.1.2 /
/// autoSpaceDN §17.3.1.3 ("assumed to be true"), kinsoku §17.3.1.16, wordWrap
/// §17.3.1.45. FLOORED siblings (deliberate): overflowPunct §17.3.1.21 (see
/// the pack_lines fit test), topLinePunct §17.3.1.43, adjustRightInd §17.3.1.1,
/// custom kinsoku tables §17.15.1.58/.59 + strictFirstAndLastChars §17.15.1.82
/// (default UAX#14 tables only — a settings.xml table would SUPERSEDE the
/// defaults, not extend them). Base-RTL paragraphs: the autospace pad rides the
/// LOGICAL-left item's width through the UAX#9 L2 reorder, so the visual gap
/// side is unverified against Word — deferred with the existing bidi
/// follow-ons (RTL indent-swap, float-path bidi).
#[derive(Clone, Copy)]
pub(crate) struct EaToggles {
    pub(crate) auto_space_de: bool,
    pub(crate) auto_space_dn: bool,
    pub(crate) kinsoku: bool,
    pub(crate) word_wrap: bool,
}

impl Default for EaToggles {
    fn default() -> Self {
        Self { auto_space_de: true, auto_space_dn: true, kinsoku: true, word_wrap: true }
    }
}

/// WORD-BEHAVIOR: ECMA §17.3.1.2/.3 define only the toggle (default true); the
/// gap magnitude is not specified. Word renders ~1/8 em (JLREQ prefers 1/4 em;
/// Word does not follow it). Single named const so recalibration is one line.
const AUTOSPACE_EM_FRACTION: f64 = 0.125;

/// A text item the East Asian post-pass may split or pad: born in `collect_items`
/// with a real byte span, carrying no object payload.
fn is_ea_plain_text(item: &Item) -> bool {
    item.image.is_none()
        && item.tab.is_none()
        && item.field.is_none()
        && item.text_box.is_none()
        && item.footnote_id.is_none()
        && item.endnote_id.is_none()
        && item.char_start != CHAR_START_SENTINEL
        && !item.text.is_empty()
}

/// autoSpaceDE "Latin side": explicit ASCII/Latin-ext/Greek/Cyrillic letter
/// ranges. Deliberately NOT "slot ∈ {Ascii, HighAnsi}": `font_slot_class`
/// classifies Arabic/Hebrew via the ComplexScript/Ascii route (and HighAnsi is
/// a catch-all), so a slot-based predicate would wrongly autospace EA↔Arabic
/// pairs — Word does not.
fn is_autospace_latin(c: char) -> bool {
    c.is_alphabetic()
        && matches!(c as u32,
            0x0041..=0x005A | 0x0061..=0x007A          // ASCII letters
            | 0x00C0..=0x024F                           // Latin-1 sup + Latin ext A/B
            | 0x1E00..=0x1EFF                           // Latin extended additional
            | 0x2C60..=0x2C7F                           // Latin extended C
            | 0xA720..=0xA7FF                           // Latin extended D
            | 0xAB30..=0xAB6F                           // Latin extended E
            | 0x0370..=0x03FF                           // Greek
            | 0x1F00..=0x1FFF                           // Greek extended (polytonic)
            | 0x0400..=0x04FF)                          // Cyrillic
}

/// autoSpaceDE/DN "East Asian side": an EA-slot LETTER. `is_alphabetic`
/// excludes CJK punctuation (。、「」 — Word does not autospace punctuation)
/// and full-width digits (which set solid with CJK).
fn is_autospace_ea(c: char) -> bool {
    font_slot_class(c) == FontSlot::EastAsia && c.is_alphabetic()
}

/// Combining mark (common blocks): a char that must stay attached to its base —
/// never a split point for the kinsoku-off/wordWrap-off cuts. Covers the
/// combining-diacritical blocks, Cyrillic combining, kana voicing marks
/// (U+3099/309A — EA-slot, so the both-EA kinsoku cut would otherwise detach
/// them), variation selectors, and half marks. Complex-script combining marks
/// (Arabic harakat, Indic matras) are already excluded wholesale by the
/// ComplexScript slot guard at the cut site.
fn is_combining_mark(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F          // combining diacritical marks
        | 0x0483..=0x0489        // Cyrillic combining
        | 0x1AB0..=0x1AFF        // combining diacritical marks extended
        | 0x1DC0..=0x1DFF        // combining diacritical marks supplement
        | 0x20D0..=0x20FF        // combining marks for symbols
        | 0x3099..=0x309A        // kana voicing marks (combining)
        | 0xFE00..=0xFE0F        // variation selectors
        | 0xFE20..=0xFE2F)       // combining half marks
}

/// East Asian post-pass over the collected items — one O(items) walk, run by
/// `layout_paragraph` immediately after `collect_items` and BEFORE the
/// selection ParaMeta capture (the splits preserve the concatenated logical
/// text and byte tiling; the autospace fold touches no text/bytes, so the
/// capture's byte basis is intact either way).
///
/// Pass order: (a) kinsoku-off / wordWrap-off splits (wordWrap-off supersedes
/// kinsoku-off), then (b) the autoSpaceDE/DN width fold. The splits only run
/// on an explicit `w:val="0"` — never on defaults — so the default path is
/// structurally a no-op for split logic, and the fold is a no-op on any
/// paragraph without an EA-letter↔Latin/digit item joint.
pub(crate) fn apply_east_asian_refinements<'f>(
    items: &mut Vec<Item>,
    ea: EaToggles,
    font_cache: &'f FontCache,
    faces: &FaceCache<'f>,
) {
    // (a) Extra break opportunities from explicitly-disabled protections.
    //  - §17.3.1.45 wordWrap off: "break words across lines by character" —
    //    split at EVERY char boundary; the greedy packer then breaks at the
    //    exact overflowing character (no pack_lines change).
    //  - §17.3.1.16 kinsoku off: the kinsoku line-break protections are
    //    disabled — approximated as "both-neighbors-EastAsia-slot become
    //    breakable" (so 語|。 splits and 。 may start a line, per the spec's
    //    own example). Both-EA (not either-EA) avoids splitting before
    //    combining marks and leaves embedded Latin words whole; the Latin×EA-
    //    punct corner (a。) stays glued, and the EA-slot test also covers
    //    Hangul/half-width forms beyond the spec's zh/ja scope — documented
    //    deviations. Per-char shaping loses cross-char kerning —
    //    rides on the project-wide kerning floor.
    //  - NEVER cut where it would change rendering (§17.3.1.45 note: breaks
    //    must not "change the semantics or appearance of the content"): a
    //    joining-script neighbor (ComplexScript slot — Arabic/Hebrew/Indic
    //    shape across char boundaries, and per-piece re-shaping would produce
    //    isolated letterforms) or a following combining mark (must stay with
    //    its base) suppresses the cut. Word does not break inside such
    //    clusters even with wordWrap off.
    if !ea.word_wrap || !ea.kinsoku {
        let old = std::mem::take(items);
        for item in old {
            if !is_ea_plain_text(&item) {
                items.push(item);
                continue;
            }
            // Byte offsets where a new piece starts (piece 0 starts at 0).
            let mut starts: Vec<usize> = vec![0];
            let mut prev: Option<char> = None;
            for (off, c) in item.text.char_indices() {
                if off > 0 {
                    let cluster_glue = font_slot_class(c) == FontSlot::ComplexScript
                        || prev.is_some_and(|p| font_slot_class(p) == FontSlot::ComplexScript)
                        || is_combining_mark(c);
                    let cut = if cluster_glue {
                        false
                    } else if !ea.word_wrap {
                        true
                    } else {
                        prev.is_some_and(|p| font_slot_class(p) == FontSlot::EastAsia)
                            && font_slot_class(c) == FontSlot::EastAsia
                    };
                    if cut {
                        starts.push(off);
                    }
                }
                prev = Some(c);
            }
            if starts.len() == 1 {
                items.push(item);
                continue;
            }
            let n = starts.len();
            for (pi, &s) in starts.iter().enumerate() {
                let e = if pi + 1 < n { starts[pi + 1] } else { item.text.len() };
                let piece = &item.text[s..e];
                // `item.style.family` was already slot-rewritten to the EA face
                // at emit time, so the right face resolves here.
                let (width, content_width, shaped) =
                    measure_item_text(piece, &item.style, font_cache, faces);
                items.push(Item {
                    text: piece.to_string(),
                    style: item.style.clone(),
                    width,
                    content_width,
                    // Interior pieces become break opportunities; the last
                    // piece keeps the original flag (Mandatory/PageBreak).
                    break_after: if pi + 1 < n { BreakAfter::Allowed } else { item.break_after },
                    shaped,
                    // Inherited oracle-invisible carriers — MUST stay explicit
                    // (a dropped `link`/`comment_ids` passes the geometry pins but
                    // breaks link/comment hit-test; only the select suite catches).
                    link: item.link.clone(),
                    // The paragraph bookmark anchors at the piece that starts
                    // the original item only.
                    bookmark: if pi == 0 { item.bookmark.clone() } else { None },
                    comment_ids: item.comment_ids.clone(),
                    // Same stamping discipline as the collect-time emit: the
                    // piece's LOGICAL span is its own bytes at its own offset.
                    char_start: item.char_start + s as u32,
                    char_len: piece.len() as u32,
                    ..Default::default()
                });
            }
        }
    }

    // (b) autoSpaceDE/DN width fold — NO new items, so the selection position currency
    // (char_start/char_len tiling, item count, byte_to_x glyph advances) is
    // preserved by construction. The pad is folded into the LEFT item's
    // `width` only (`content_width` untouched): exact trailing-space semantics
    // through pack_lines' fit test — the gap occupies space mid-line, is
    // unpainted when the joint lands at a line break (Word drops autospace at
    // wraps), and never forces a wrap by itself. Zero-length sentinel items
    // (bookmark/comment anchors) between an EA/Latin pair act as separators
    // and suppress the gap — a deliberate v1 floor. UAX#14 guarantees the
    // EA↔Latin/digit transitions of the default classes are item boundaries
    // (ID×AL / AL×ID / ID×NU / NU×ID are all break opportunities), so
    // item-boundary granularity is complete for them.
    if ea.auto_space_de || ea.auto_space_dn {
        for i in 0..items.len().saturating_sub(1) {
            if !is_ea_plain_text(&items[i]) || !is_ea_plain_text(&items[i + 1]) {
                continue;
            }
            let Some(l) = items[i].text.chars().next_back() else { continue };
            let Some(f) = items[i + 1].text.chars().next() else { continue };
            // An explicit space suppresses autospace — the regions are not
            // "adjoining" (§17.3.1.2). (The letter/digit predicates below also
            // reject whitespace; this guard additionally pins the "adjoining"
            // semantics against a future look-past-whitespace regression —
            // see autospace_space_suppression.)
            if l.is_whitespace() || f.is_whitespace() {
                continue;
            }
            let de = ea.auto_space_de
                && ((is_autospace_ea(l) && is_autospace_latin(f))
                    || (is_autospace_latin(l) && is_autospace_ea(f)));
            let dn = ea.auto_space_dn
                && ((is_autospace_ea(l) && f.is_ascii_digit())
                    || (l.is_ascii_digit() && is_autospace_ea(f)));
            if de || dn {
                // The gap is measured off the EA side's font size (equal to the
                // Latin side's in the single-run common case).
                let ea_pt = if is_autospace_ea(l) {
                    items[i].style.font_pt
                } else {
                    items[i + 1].style.font_pt
                };
                items[i].width += AUTOSPACE_EM_FRACTION * pt_to_px(ea_pt);
            }
        }
    }
}

/// Width of the post-decimal-tab text up to (but not including) its FIRST
/// decimal separator ('.' or ','), used to place that separator AT the tab stop
/// (§17.18.84 ST_TabJc="decimal"). `span` is the run of items following the tab,
/// in logical order, up to the next tab. The separator may sit anywhere inside
/// the span — labels/spaces before the number are counted, then the integer part
/// up to the dot. If NO separator is present the whole span width is returned, so
/// the caller right-aligns the span's END at the stop (Word's no-separator
/// fallback). Per-item partial widths come from the shaped glyph advances
/// (`cluster` = byte offset into the visible text); the no-font estimate path
/// falls back to a proportional char-count split.
fn decimal_pre_separator_width(span: &[Item]) -> f64 {
    let mut acc = 0.0;
    for it in span {
        // Images/tabs/boxes have no separable text; count their full width.
        let sep_byte = it.visible_text()
            .char_indices()
            .find(|&(_, c)| c == '.' || c == ',')
            .map(|(b, _)| b);
        let Some(sep_byte) = sep_byte else {
            acc += it.width;
            continue;
        };
        // Width within this item up to the separator byte.
        let before = if let Some(shaped) = &it.shaped {
            shaped.glyphs.iter()
                .filter(|g| (g.cluster as usize) < sep_byte)
                .map(|g| g.x_advance)
                .sum()
        } else {
            // No shaping (estimate path): item width was char_count * half-em;
            // split proportionally by the number of chars before the separator.
            let total_chars = it.visible_text().chars().count().max(1) as f64;
            let before_chars = it.visible_text()[..sep_byte].chars().count() as f64;
            it.width * (before_chars / total_chars)
        };
        return acc + before;
    }
    // No separator anywhere in the span → align the span's end at the stop.
    acc
}

/// Distribute a justified line's slack across its inter-word gaps in place:
/// widen each stretchable item (trailing-space / autospace gaps, excluding
/// sentinel markers and tabs/images) by an equal share of `available_width -
/// line_width`, so the line's last glyph reaches the right margin. No-op when
/// the line already fills (or overflows) the width. Governing spec: §17.3.1.13
/// `w:jc="both"` justification.
pub(crate) fn justify_line(items: &mut [Item], available_width: f64) {
    let line_width: f64 = items.iter().map(|it| it.width).sum();
    let extra = available_width - line_width;
    if extra <= 0.0 {
        return;
    }
    // Sentinel items (numbering markers, bookmark anchors) are excluded: Word
    // never stretches the marker/suffix gap on justified lines, and a decorated
    // marker legitimately carries content_width < width (the §17.9.24 clamp).
    //
    // Accepted deviation: an autoSpaceDE/DN-padded item (the §17.3.1.2/.3
    // fold in `apply_east_asian_refinements`) also satisfies
    // `width > content_width` with a non-sentinel char_start and therefore
    // becomes a justify stretch target, indistinguishable from a trailing-space
    // item — accepted over adding an Item field (every construction site would
    // change). Word can flex the autospace gap under justification anyway.
    let gap_count = items.iter()
        .filter(|it| it.tab.is_none() && it.image.is_none()
            && it.char_start != CHAR_START_SENTINEL && it.width > it.content_width)
        .count();
    if gap_count == 0 {
        return;
    }
    let per_gap = extra / gap_count as f64;
    for item in items.iter_mut() {
        if item.tab.is_none() && item.image.is_none()
            && item.char_start != CHAR_START_SENTINEL && item.width > item.content_width {
            item.width += per_gap;
        }
    }
}

/// The lookahead a tab's alignment needs: the total advance of the run of items
/// that FOLLOW it up to the next tab, and the part of that run preceding the
/// first decimal separator.
///
/// Neither depends on where the tab lands, so both are computed once, before
/// packing — which is precisely what lets the resolution itself move INSIDE the
/// packing loop, where the line's true leading edge is finally known.
#[derive(Clone, Copy, Default)]
pub(crate) struct TabLookahead {
    following_width: f64,
    pre_sep: f64,
}

/// One [`TabLookahead`] per item, indexed in parallel with `items` (the entries
/// for non-tab items are unread). Returns empty when the paragraph has no tab,
/// so the overwhelmingly common case allocates nothing.
pub(crate) fn tab_lookaheads(items: &[Item]) -> Vec<TabLookahead> {
    if !items.iter().any(|it| it.tab.is_some()) {
        return Vec::new();
    }
    items.iter().enumerate().map(|(i, it)| {
        if it.tab.is_none() {
            return TabLookahead::default();
        }
        let following = &items[i + 1..];
        let span_end = following.iter().take_while(|f| f.tab.is_none()).count();
        let span = &following[..span_end];
        TabLookahead {
            following_width: span.iter().map(|f| f.width).sum(),
            pre_sep: decimal_pre_separator_width(span),
        }
    }).collect()
}

/// Resolve one tab item's advance against the stop chain and return the logical
/// x it leaves the cursor at.
///
/// `x` is the advance from the LINE's leading edge — §17.3.1.37 defines stop
/// positions in exactly that space ("the left edge for a left-to-right
/// paragraph, and the right edge for a right-to-left paragraph"), so
/// `next_stop` serves both directions unchanged; the End alignment plus the L2
/// reorder place RTL lines. Bar tabs (§17.18.84) never appear in
/// `tab_stops.custom` — they are draw-only rules, excluded upstream.
///
/// THE single spelling for both WML packers. It used to be a PRE-PASS
/// over the whole paragraph inside `pack_lines`, which had two consequences:
/// the float-aware packer could not do it at all (it re-packs against different
/// geometry, so the widths it inherited were resolved for the wrong line), and
/// the pre-pass itself reset `x` only at a mandatory/page break, never at a
/// SOFT WRAP — so a tab on line 2+ of a wrapped paragraph was measured from the
/// paragraph's start including every preceding line's advance. Resolving inside
/// the packing loop is what makes `x` line-relative, as the spec requires.
///
/// Known RTL floors: leader glyphs are not painted inside base-RTL tab gaps, and
/// explicit jc left/right (as opposed to start/end) in a base-RTL paragraph is
/// not distinguished from its logical counterpart.
pub(crate) fn resolve_tab(
    item: &mut Item, x: f64, look: TabLookahead, tab_stops: &ResolvedTabStops,
) -> f64 {
    let (stop_pos, alignment, leader) = tab_stops.next_stop(x);
    // How far LEFT of the stop the following text must begin, per alignment.
    let back = match alignment {
        TabAlignment::Left => 0.0,
        TabAlignment::Right => look.following_width,
        TabAlignment::Center => look.following_width / 2.0,
        // §17.18.84 ST_TabJc="decimal": align the FIRST decimal separator
        // ('.'/',') of the post-tab text AT the stop. The text up to (but not
        // including) the separator ends at the stop; the separator and
        // fractional part extend right. Text with NO separator right-aligns its
        // end at the stop (Word's fallback) — `decimal_pre_separator_width`
        // returns the whole span width in that case, so this arm degenerates to
        // the Right one, which is the intended behavior.
        TabAlignment::Decimal => look.pre_sep,
    };
    let tab_width = (stop_pos - x - back).max(pt_to_px(3.0));
    item.width = tab_width;
    item.content_width = tab_width;
    item.tab = Some(TabInfo { alignment, leader });
    // The cursor lands where the FOLLOWING text starts, not at the stop: a
    // right/center/decimal tab pulls that text back off the stop by `back`.
    //
    // DEVIATION, pre-existing and deliberately preserved here: when the stop is
    // so close that `tab_width` hits the 3pt floor, the painted advance is the
    // floor but the LOGICAL cursor still reports `stop_pos - back`, which can be
    // behind `x + tab_width`. The next stop is then chosen from a cursor the
    // text does not actually sit at. Left as-is so this refactor carries ONE
    // behavioral change (the line-relative `x`, below) rather than two.
    stop_pos - back
}

/// The fraction of a word's TRAILING WHITESPACE the fit test charges against
/// the line, under Word's legacy line-fitter (compatibilityMode < 15).
///
/// This is the `wrapTrailSpaces` / "Word-compatible trailing blanks" behavior, a
/// Word-2007-era compat trait dropped from ECMA-376 5th ed (so the spec proper
/// defines no line-measurement here; the modern mode-15 engine and standard
/// hanging-space layout don't do it). Counting the FULL trailing space
/// over-wraps (a word with <1 space-width of slack that Word keeps); counting
/// NONE under-wraps (a word with ~0 slack that Word breaks). Half a space is
/// calibrated against Word's exact L1–L9 on calibration doc C5 (the diverging
/// word had 0.69px slack; the next kept word had 3.26px — a half-space ~2.2px
/// sits in that window) and holds every page-count fixture. The calibration is
/// recorded, with its source and confidence, in the ground-truth manifest
/// (`ground_truth.rs`, entry C5 — see `docs/calibration-ids.md` for how
/// calibration IDs are assigned).
///
/// Gated on the compat mode so non-legacy docs keep standard line breaking.
pub(crate) fn trailing_blank_reserve() -> f64 {
    if RESERVE_TRAILING_BLANKS.with(|c| c.get()) { 0.5 } else { 0.0 }
}

/// Does `item` still fit on a line (or float segment) that has already consumed
/// `used` px of `avail`?
///
/// THE single fit predicate for both WML packers. It was spelled twice:
/// the float-aware packer tested `used + content_width <= avail + 1e-2` and so
/// charged NO trailing-blank reserve and granted an epsilon of slack the
/// float-free packer does not — two different answers to one question, on a
/// rule calibrated against Word. The reserve is passed in rather than read here
/// so the thread-local is sampled once per paragraph, not once per item.
///
/// FLOOR — overflowPunct (§17.3.1.21, default true): Word lets ONE trailing
/// punctuation character (Latin quotes included, per the spec's own example)
/// hang past the text extent instead of wrapping; the engine behaves as val="0"
/// for ALL paragraphs. Honoring it means loosening this test for
/// punct-terminated boundary words, which would move every punct-terminated
/// boundary line in the Word-calibrated 500kB / C1 fits — revisit only with
/// Word-side evidence of a diverging line.
#[inline]
pub(crate) fn item_fits(used: f64, item: &Item, avail: f64, trail_reserve: f64) -> bool {
    used + item.content_width + (item.width - item.content_width) * trail_reserve <= avail
}

/// Greedily pack shaped [`Item`]s into lines that fit the content width: fill
/// the current line until the next item would overflow, then start a new one
/// (honoring mandatory/page breaks and resolving tab advances against the line
/// each tab lands on — `resolve_tab`). Returns the
/// per-line item groups the caller turns into `PagedLine`s. Carries the legacy
/// trailing-space half-reserve (compatMode 12) so a line's final blank does not
/// force an early break. Governing spec: §17.3 line breaking; UAX #14 supplies
/// the break opportunities upstream in `collect_items`.
pub(crate) fn pack_lines(items: Vec<Item>, available_width: f64, first_line_indent: f64, font_cache: &FontCache, tab_stops: &ResolvedTabStops, base_rtl: bool) -> Vec<LayoutLine> {
    let looks = tab_lookaheads(&items);

    let mut lines: Vec<LayoutLine> = Vec::new();
    let mut current = LayoutLine::new();
    let line_width = |idx: usize| -> f64 {
        available_width - if idx == 0 { first_line_indent } else { 0.0 }
    };
    // A line's leading edge in the paragraph's logical advance space: only the
    // FIRST line carries the first-line indent (§17.3.1.12), and only a positive
    // one — a hanging indent starts the line at the leading edge.
    let leading = |idx: usize| -> f64 {
        if idx == 0 { first_line_indent.max(0.0) } else { 0.0 }
    };

    let trail_reserve = trailing_blank_reserve();

    // The empty-line and fits-on-line arms both push the item but are kept
    // distinct on purpose (always-accept-first vs accept-when-it-fits).
    #[allow(clippy::if_same_then_else)]
    for (i, mut item) in items.into_iter().enumerate() {
        let mandatory_after = item.break_after == BreakAfter::Mandatory;
        let page_break_after = item.break_after == BreakAfter::PageBreak;

        // Tab advances resolve HERE, against the line being filled, because
        // §17.3.1.37 measures stops from the line's leading edge — see
        // `resolve_tab`.
        //
        // A tab CANNOT reach the wrap branch below: every tab item carries
        // `text: "\t"`, so `is_only_whitespace()` is true and the `continue`
        // fires first. An earlier version added a re-resolve there with a comment
        // claiming a wrapped tab is re-measured against the fresh line; that is
        // unreachable, and the comment described behaviour the code cannot
        // produce. Removed rather than made reachable — making it fire would
        // change where tabs land, which is a behaviour change needing its own
        // fixture and re-baseline, not a tidy-up.
        if item.tab.is_some() {
            resolve_tab(&mut item, leading(lines.len()) + current.width(), looks[i], tab_stops);
        }

        if current.is_empty() {
            current.push(item, font_cache);
        } else if item_fits(current.width(), &item, line_width(lines.len()), trail_reserve) {
            current.push(item, font_cache);
        } else {
            lines.push(std::mem::replace(&mut current, LayoutLine::new()));
            // ⚠️ DECLARED, pre-existing and wider than it looks: this drops any
            // item whose `text` is whitespace-only at a wrap edge — which is
            // correct for a trailing space, but `is_only_whitespace()` is also
            // vacuously true for an inline TEXT BOX and a math atom, both of
            // which carry empty text and their content elsewhere. A text box at
            // a wrap edge is therefore swallowed. Pre-existing and not fixed
            // here (it needs its own fixture and re-baseline); recorded
            // so the next reader does not assume "whitespace" means whitespace.
            if item.is_only_whitespace() {
                continue;
            }
            current.push(item, font_cache);
        }

        if mandatory_after || page_break_after {
            if page_break_after {
                current.ends_with_page_break = true;
            }
            lines.push(std::mem::replace(&mut current, LayoutLine::new()));
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    // UAX#9 L2: reorder each finalized line's items into visual order (after
    // line-breaking, per the algorithm). No-op for non-RTL lines.
    for line in &mut lines {
        bidi_reorder_line(&mut line.items, base_rtl);
    }
    lines
}

/// Fold one item's vertical extent into a line box's running metrics.
///
/// THE single spelling of the chain, for both WML packers: `LayoutLine::push`
/// (float-free) and `pack_lines_around_floats` (float-aware). It was written
/// twice, and the copies diverged — the OMML commit added the `math` arm to
/// `push` alone, so a math atom on a line wrapping a float fell through to the
/// *font* branch (an atom has empty `text` and no `image`/`text_box`) and
/// measured as ordinary text: the line box came out short by the atom's stacked
/// height and the equation overprinted the line above. Shipped, undetected,
/// because no fixture paired `m:oMath` with a `wp:anchor`. One function now, so
/// the compiler keeps the two readers in step.
///
/// Arm order is significant: `math`/`text_box`/`image` items all carry empty
/// `text`, so each must be tested before the font fallback.
pub(crate) fn accumulate_item_metrics(
    item: &Item,
    asc: &mut f64, desc: &mut f64, fpt: &mut f64, imgh: &mut f64,
    font_cache: &FontCache,
) {
    *fpt = fpt.max(item.style.font_pt);
    if let Some(m) = &item.math {
        // A math atom reserves its OWN stacked height so a fraction/script
        // grows the line box (ascent above + descent below the text baseline).
        *asc = asc.max(m.ascent);
        *desc = desc.max(m.descent);
    } else if let Some(ref tb) = item.text_box {
        *asc = asc.max(tb.height_px);
        *imgh = imgh.max(tb.height_px);
    } else if let Some(img) = &item.image {
        // §20.4.2.8: distT/distB pad the image vertically — the line's extent
        // must grow by distT+distB so the lines above/below keep their gap. The
        // bitmap is still drawn at its intrinsic height_px, lifted distB above
        // the baseline (see render.rs).
        let vextent = img.height_px + img.dist_t_px + img.dist_b_px;
        *asc = asc.max(vextent);
        *imgh = imgh.max(vextent);
    } else if let Some(font) = font_cache.resolve(
        &item.style.family,
        if item.style.bold { 700 } else { 400 },
        item.style.italic,
    ) {
        let m = font_metrics(font, item.style.font_pt);
        *asc = asc.max(m.ascent);
        *desc = desc.max(m.descent + crate::effective_line_gap(&m));
    } else {
        let approx = pt_to_px(item.style.font_pt);
        *asc = asc.max(approx * 0.8);
        *desc = desc.max(approx * 0.2);
    }
}

/// One horizontal run of items within a line, starting at `x_start`. A line
/// with no float exclusions has exactly one segment spanning its full width;
/// a line wrapping around a float has one segment per free interval. Making
/// this the universal model keeps the renderer/justifier float-agnostic.
#[derive(Clone)]
pub(crate) struct LineSegment {
    pub(crate) x_start: f64,
    pub(crate) items: Vec<Item>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BreakAfter {
    Allowed,
    /// `<w:br/>` or `<w:cr/>` — force a new line, paragraph continues.
    Mandatory,
    /// `<w:br w:type="page"/>` — force a new line *and* close the page.
    PageBreak,
}

#[derive(Clone)]
pub(crate) struct Item {
    pub(crate) text: String,
    pub(crate) style: RunStyle,
    pub(crate) width: f64,
    /// Width excluding trailing whitespace. Used by the line packer to decide
    /// whether an item fits — trailing whitespace hangs off the line edge.
    pub(crate) content_width: f64,
    pub(crate) break_after: BreakAfter,
    /// Shaped glyph data from rustybuzz. `None` for image items or
    /// when no font was loaded (fallback to estimated widths).
    pub(crate) shaped: Option<ShapedRun>,
    /// `Some` when this item represents an inline image (rendered via
    /// `drawImage` instead of `fillText`). `text` is empty in this case;
    /// `width` is the image's display width in CSS pixels.
    pub(crate) image: Option<ImageItem>,
    pub(crate) field: Option<FieldCode>,
    pub(crate) tab: Option<TabInfo>,
    pub(crate) text_box: Option<TextBoxContent>,
    pub(crate) footnote_id: Option<i64>,
    pub(crate) endnote_id: Option<i64>,
    /// `Some` when this item is a laid-out OMML math atom (`<m:oMath>` /
    /// `<m:oMathPara>`). Boxed to keep `Item` small (the payload is a whole
    /// flattened box-and-glue tree). Mirrors `image`/`text_box`: `text` is empty,
    /// `width`/`content_width` = the atom's advance, `break_after` Allowed on both
    /// sides (unbreakable), `char_start` a sentinel (born of `extra_children`, not
    /// paragraph text). Every math-free doc leaves this `None`, so all
    /// cloning/packing stays byte-identical.
    pub(crate) math: Option<Box<crate::wml::math::MathBox>>,
    /// Hyperlink target for this item, if it sits inside a `<w:hyperlink>`.
    /// `"url:<href>"` for external links, `"anchor:<bookmark>"` for internal.
    /// Used to build clickable link rectangles for the JS hit-test layer.
    pub(crate) link: Option<String>,
    /// Name of a `<w:bookmarkStart>` anchored at this item's position, if any.
    /// Used as the scroll target for internal anchor links. Paragraph-first
    /// granularity: the paragraph's first item carries its bookmark(s).
    pub(crate) bookmark: Option<String>,
    /// Comment-range ids covering this item (§17.13.4 — `commentRangeStart`/
    /// `commentRangeEnd`). The per-item highlight carrier: a flat marker scan in
    /// `collect_items` stamps every item with the set of comment ranges open at
    /// its position, with open-id state carried ACROSS paragraphs by the
    /// `OPEN_COMMENT_IDS` thread-local (comment ranges routinely span
    /// paragraphs). Mirrors the `link`/`bookmark` post-hoc stamp — it never
    /// changes widths, so the geometry hot loop (`pack_lines`/`justify_line`) is
    /// untouched. `SmallVec<[i64; 1]>` is allocation-free in the common 0/1-id
    /// case; only overlapping ranges spill to the heap. Read by `comment_rects`
    /// (the post-layout pairing/overlap accessor) — kept generic (no `kind`
    /// enum) so a future search-highlight / track-change carrier can reuse it.
    pub(crate) comment_ids: CommentIds,
    /// Byte offset of this item's `text` into the paragraph's
    /// LOGICAL text (the `collect_items` emit-order, post-`to_uppercase`,
    /// pre-`pack_lines`, pre-marker-insert concatenation of every collected
    /// `Item.text`). Stamped in `collect_items` BEFORE packing, so it rides
    /// through `pack_lines` AND `bidi_reorder_line` (whole-`Item` permutation,
    /// not a split) — a painted item in VISUAL order still carries its
    /// LOGICAL start. `CHAR_START_SENTINEL` for items NOT born in `collect_items`
    /// (numbering markers, images, footnote refs, page breaks): hit-test on a
    /// sentinel snaps to para-start (byte 0), `selectionRects` paints it when the
    /// covered range includes byte 0. Additive like `comment_ids` — never touches
    /// geometry, invisible to the oracle `geometry_dump`.
    pub(crate) char_start: u32,
    /// Byte length of this item's LOGICAL span, stamped as
    /// `text.len()` at collect time. The single span-length source for
    /// `selectionRects`: the PAGE/NUMPAGES post-layout pass mutates a
    /// stored item's `text`/`width`/`shaped` in place ('11' painted vs '1'
    /// collected), so the logical span must NOT be re-derived from the possibly-
    /// mutated `text.len()`. `0` for sentinel items.
    pub(crate) char_len: u32,
    /// This item's bidi embedding-level parity (`level % 2 == 1`),
    /// stamped in `bidi_reorder_line`. Kept for reorder/paint SEMANTICS only; the
    /// x↔byte mirror gates on `ShapedRun.rtl` (actual glyph storage order), which
    /// can DISAGREE (a neutral-only item under an RTL base carries an odd level
    /// yet shapes LTR). `false` at collect time; assigned before the L2 permute.
    pub(crate) rtl: bool,
}

impl Default for Item {
    /// A neutral sentinel item: empty text, the default run style, zero widths,
    /// no payload. Cannot be `#[derive]`d — a derived `char_start` would be `0`,
    /// but the sentinel convention needs `CHAR_START_SENTINEL` (`u32::MAX`), and
    /// `char_start` is oracle-INVISIBLE so a wrong `0` passes the geometry pins
    /// while silently corrupting hit-test/selection (only the select suite catches
    /// it). Every collapsed sentinel literal starts from this and sets its own
    /// non-default fields (payload / width / style / char span) before the spread.
    fn default() -> Self {
        Item {
            text: String::new(),
            style: RunStyle::default(),
            width: 0.0,
            content_width: 0.0,
            break_after: BreakAfter::Allowed,
            shaped: None,
            image: None,
            field: None,
            tab: None,
            text_box: None,
            footnote_id: None,
            endnote_id: None,
            math: None,
            link: None,
            bookmark: None,
            comment_ids: CommentIds::new(),
            char_start: CHAR_START_SENTINEL,
            char_len: 0,
            rtl: false,
        }
    }
}

#[cfg(test)]
impl Item {
    /// Serialize every field into the oracle ITEM stream. One line,
    /// no trailing newline; the caller owns indentation and the line terminator.
    ///
    /// The opening exhaustive destructure (NO rest-dots) is a COMPILE-TIME
    /// completeness guard, the pattern `CellStyle::hash_into` (sml/model.rs)
    /// established: adding a 20th field to `Item` fails to compile until it is
    /// bound and emitted here. That guard is owed — `math` reached
    /// `LayoutLine::push` but not `accumulate_item_metrics`, shipping a real
    /// mis-measure, because nothing forced the two readers to stay in step.
    ///
    /// NO COORDINATES: `width`/`content_width` are per-item ADVANCES, not page
    /// positions, so a pure repositioning (margins, page size, baseline maths)
    /// moves the LINE stream and leaves this byte-identical — and "line RED /
    /// items GREEN" is itself the diagnosis.
    /// `styles` is a first-appearance intern table of rendered `RunStyle`s: the
    /// item emits `y{index}` and the table is printed once at the head of the
    /// dump. Inlining all 24 style fields per item cost ~180 bytes each and made
    /// a one-field change a whole-file diff; interning makes it ONE changed table
    /// row plus the items that moved to it, and shrinks the goldens ~4x.
    pub(crate) fn dump_into(
        &self, out: &mut String, styles: &mut Vec<String>,
        glyphs: &mut crate::wml::page::GlyphStream,
    ) {
        use std::fmt::Write;
        let Item {
            text, style, width, content_width, break_after, shaped, image, field,
            tab, text_box, footnote_id, endnote_id, math, link, bookmark,
            comment_ids, char_start, char_len, rtl,
        } = self;
        // This item's contribution to the GLYPH stream, emitted here (not
        // in a second walk) so `shaped` and `math` are read by the SAME
        // exhaustive destructure that guards the ITEM stream. One token per item:
        // the run's vocabulary reference, `-` when the item shaped nothing, and a
        // bracketed list for a math atom's per-node runs (an atom's item shapes
        // nothing itself — its glyphs hang off `MathNode::Glyph`).
        {
            let mut tok = match shaped {
                Some(s) => glyphs.intern(s),
                None => "-".to_string(),
            };
            if let Some(m) = math {
                let inner: Vec<String> = m.nodes.iter().filter_map(|n| match n {
                    crate::wml::math::MathNode::Glyph { shaped: Some(s), .. } => Some(glyphs.intern(s)),
                    crate::wml::math::MathNode::Glyph { shaped: None, .. } => Some("-".to_string()),
                    // `Rule`/`VRule` are filled rects and `GlyphOutline` is a
                    // MATH size-variant painted from its path — none of the three
                    // owns a `ShapedRun`, so none is expressible here. Named
                    // rather than caught by a `_` arm so that a FIFTH variant
                    // reds this site too: the chars walk below would red on it
                    // anyway, and an author who satisfied only that one would
                    // leave a glyph-bearing variant silently absent from this
                    // stream — the two-readers-one-enforced shape again.
                    crate::wml::math::MathNode::Rule { .. }
                    | crate::wml::math::MathNode::VRule { .. }
                    | crate::wml::math::MathNode::GlyphOutline { .. } => None,
                }).collect();
                if !inner.is_empty() {
                    tok.push_str(&format!("[{}]", inner.join(" ")));
                }
            }
            glyphs.push_token(tok);
        }
        let mut rendered = String::new();
        style.dump_into(&mut rendered);
        let y = styles.iter().position(|s| *s == rendered).unwrap_or_else(|| {
            styles.push(rendered);
            styles.len() - 1
        });
        let _ = write!(out, "y{y} w{width:.2} cw{content_width:.2} ");
        if *char_start == CHAR_START_SENTINEL {
            let _ = write!(out, "c- ");
        } else {
            let _ = write!(out, "c{char_start}+{char_len} ");
        }
        let _ = write!(out, "{text:?}");
        match break_after {
            BreakAfter::Allowed => {}
            BreakAfter::Mandatory => out.push_str(" brk=M"),
            BreakAfter::PageBreak => out.push_str(" brk=P"),
        }
        if *rtl {
            out.push_str(" rtl");
        }
        if let Some(s) = shaped {
            // `rtl` here is glyph STORAGE order (≠ `Item::rtl`, bidi parity) and
            // gates the x↔byte mirror, so both are folded. `text` is emitted only
            // when it disagrees with the item's — the PAGE/NUMPAGES post-pass
            // rewrites one and not the other.
            let _ = write!(out, " sh{}g adv{:.2} r{} s{}",
                s.glyphs.len(), s.total_advance, s.rtl as u8, s.is_simple as u8);
            if s.text != *text {
                let _ = write!(out, " shtext{:?}", s.text);
            }
        }
        // The payload destructures below are the same completeness guard as the
        // `Item` one above, applied to the structs that HANG off an item: both
        // are complete today, so what they buy is that the NEXT field (an image
        // crop rect, a text-box inset) cannot reach the painter while staying
        // invisible to this stream. Every field of both affects what is drawn.
        if let Some(ImageItem {
            rel_id, height_px, dist_l_px, dist_t_px, dist_r_px, dist_b_px,
        }) = image {
            let _ = write!(out, " img{rel_id:?} h{height_px:.2} dist{dist_l_px:.2},{dist_t_px:.2},{dist_r_px:.2},{dist_b_px:.2}");
        }
        if let Some(f) = field {
            let _ = write!(out, " fld={f:?}");
        }
        if let Some(t) = tab {
            let _ = write!(out, " tab={:?}", t.leader);
        }
        // `lines` is emitted as a COUNT here and walked in full by
        // `PagedLine::dump_into`, which recurses into the interior stream.
        if let Some(TextBoxContent {
            raw, lines, width_px, height_px, fill_color, border_color, border_width_px,
        }) = text_box {
            let _ = write!(out, " tbx{width_px:.2}x{height_px:.2} raw{} lines{} fill{} border{}/{border_width_px:.2}",
                raw.is_some() as u8, lines.len(),
                fill_color.as_deref().unwrap_or("-"),
                border_color.as_deref().unwrap_or("-"));
        }
        if let Some(id) = footnote_id {
            let _ = write!(out, " fn{id}");
        }
        if let Some(id) = endnote_id {
            let _ = write!(out, " en{id}");
        }
        if let Some(m) = math {
            // The atom's `w/a/d` are already in the LINE stream's `M{…}` token;
            // what only this stream sees is WHICH CHARACTERS the atom renders —
            // the class of bug an all-geometry oracle cannot express.
            let mut chars = String::new();
            // Exhaustive over every variant's PAYLOAD (NO rest-dots) — the
            // `CellStyle::hash_into` completeness guard one level down, and the
            // deepest hole this dump had: adding a field to any `MathNode`
            // variant fails to compile HERE until a decision is recorded about
            // which stream owns it. The precedent is `Item::math` itself, which
            // reached `LayoutLine::push` but not `accumulate_item_metrics` and
            // shipped a real mis-measure because nothing forced its two readers
            // to stay in step.
            //
            // Where the `_`-bound fields actually go, one class at a time:
            //  * `shaped` IS read — by the GLYPH-stream walk at the top of this
            //    same function, which interns each math run's glyph vector;
            //  * `dx`/`dy`/`x`/`y`/`w`/`thickness`/`height`/`segs` are atom
            //    INTERIOR COORDINATES and this stream is coordinate-free by
            //    construction (see the doc above), so it cannot adopt them;
            //  * `style` and `color` are neither — they are content this stream
            //    could express, and does NOT. A math glyph's weight, size and
            //    colour are pinned by nothing today.
            // Both holes are declared, not hypothetical. The atom's OUTER box
            // rides `M{w,a,d}` here and `geometry_dump`, but the interior is
            // pinned by neither: +5px on every `Rule` x-shift reds ONE bespoke
            // test (`math_bar_overline_spans_width`) and +3px on every `Glyph`
            // dx/dy reds five, with all 18 math goldens and
            // `math_fixtures_geometry_pinned` green either way (measured).
            // Closing them means a math-geometry stream and a math-style token —
            // both re-record goldens, so neither belongs in a compile-time guard.
            for n in &m.nodes {
                match n {
                    crate::wml::math::MathNode::Glyph { text, dx: _, dy: _, style: _, shaped: _ } =>
                        chars.push_str(text),
                    crate::wml::math::MathNode::GlyphOutline {
                        ch, dx: _, dy: _, pt_equiv: _, color: _, segs: _,
                    } => chars.push_str(ch),
                    crate::wml::math::MathNode::Rule { x: _, y: _, w: _, thickness: _ } =>
                        chars.push('\u{2500}'),
                    crate::wml::math::MathNode::VRule { x: _, y: _, thickness: _, height: _ } =>
                        chars.push('\u{2502}'),
                }
            }
            let _ = write!(out, " math w{:.2} a{:.2} d{:.2} n{} disp{} {chars:?}",
                m.width, m.ascent, m.descent, m.nodes.len(), m.is_display as u8);
        }
        if let Some(l) = link {
            let _ = write!(out, " link{l:?}");
        }
        if let Some(b) = bookmark {
            let _ = write!(out, " bm{b:?}");
        }
        if !comment_ids.is_empty() {
            let _ = write!(out, " cmt{comment_ids:?}");
        }
    }
}

/// Per-item comment-range id set (see `Item::comment_ids`). Inline up to one id.
pub(crate) type CommentIds = SmallVec<[i64; 1]>;

/// The `Item::char_start`/`char_len` sentinel for items NOT born in
/// `collect_items` (numbering markers, inline images, footnote/endnote refs,
/// page breaks, bookmarks). Hit-test snaps a sentinel to paragraph-start.
pub(crate) const CHAR_START_SENTINEL: u32 = u32::MAX;

#[derive(Clone)]
pub(crate) struct TextBoxContent {
    pub(crate) raw: Option<CTTxbxContent>,
    pub(crate) lines: Vec<PagedLine>,
    pub(crate) width_px: f64,
    pub(crate) height_px: f64,
    pub(crate) fill_color: Option<String>,
    pub(crate) border_color: Option<String>,
    pub(crate) border_width_px: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FieldCode {
    Page,
    NumPages,
}

/// A run's tracked-change state (§17.13.5). `author` drives the redline color.
#[derive(Clone)]
enum TrackChange {
    Inserted(String),
    Deleted(String),
}

/// Synthetic scroll-anchor name for a comment's range start, stamped onto the
/// paragraph's first line so `comments()` can resolve it through the same
/// `bookmark_position` page-walk used for real bookmarks. Internal, never user
/// visible; the `\u{1}` prefix cannot collide with a real `w:bookmarkStart`
/// name (which is plain text) or with `_GoBack`.
pub(crate) fn comment_anchor_name(id: i64) -> String {
    format!("\u{1}comment\u{1}{id}")
}

thread_local! {
    /// The comment-range ids currently OPEN at the layout cursor.
    /// `collect_items` is called once per paragraph in document order; a comment
    /// range opened by `<w:commentRangeStart>` in one paragraph stays open until
    /// its `<w:commentRangeEnd>` — which may be paragraphs later — so the open
    /// set must persist ACROSS `collect_items` calls. This thread-local is that
    /// cross-paragraph carrier (single-threaded WASM), mirroring `HYPERLINK_RELS`
    /// / `FIELD_DATES` and avoiding threading a `&mut Vec` through
    /// `layout_paragraph`'s many call sites. Published (cleared) at `run_layout`
    /// start so a re-layout / document switch starts with no open ranges.
    /// Insertion order is preserved (a Vec, not a set) for stable highlight
    /// stacking; ids are deduped on push.
    pub(crate) static OPEN_COMMENT_IDS: std::cell::RefCell<Vec<i64>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Stable redline color for an author name (Word assigns per-author colors;
/// we use a small fixed palette keyed by a hash of the author string).
pub(crate) fn author_color(author: &str) -> &'static str {
    const PALETTE: &[&str] = &[
        "#C00000", // red
        "#0070C0", // blue
        "#00B050", // green
        "#7030A0", // purple
        "#E36C09", // orange
        "#1F9CA8", // teal
    ];
    let h = author.bytes().fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize));
    PALETTE[h % PALETTE.len()]
}

/// One frame of the complex-field state machine (§17.16.18).
#[derive(Default)]
struct FieldFrame {
    /// Accumulated instruction text (from `instrText` runs before `separate`).
    instr: String,
    /// True once the `fldChar(separate)` marker has been seen.
    in_result: bool,
    /// Resolved live field code (PAGE/NUMPAGES), set at `separate`.
    code: Option<FieldCode>,
    /// Document-level field resolved to a string at collect time
    /// (DATE/TIME/CREATEDATE/SAVEDATE).
    static_value: Option<String>,
    /// True once the live value has been emitted for one result run.
    emitted: bool,
}

/// The `fldChar` type carried by a run, if any (begin/separate/end marker).
fn run_fld_char_type(run: &ooxml_wml::types::Run) -> Option<ooxml_wml::types::STFldCharType> {
    run.run_content.iter().find_map(|rc| match rc {
        ooxml_wml::types::RunContent::FldChar(fc) => Some(fc.fld_char_type),
        _ => None,
    })
}

/// Like `Run::text()` but also includes `<w:delText>` content — used for
/// deleted runs so struck-through text renders in All-Markup view. Mirrors the
/// parser's text() (T, Tab→\t, Cr/Br→\n) plus DelText.
fn run_text_including_del(run: &ooxml_wml::types::Run) -> String {
    let mut out = String::new();
    for rc in &run.run_content {
        match rc {
            ooxml_wml::types::RunContent::T(t)
            | ooxml_wml::types::RunContent::DelText(t) => {
                if let Some(ref s) = t.text { out.push_str(s); }
            }
            ooxml_wml::types::RunContent::Tab(_) => out.push('\t'),
            ooxml_wml::types::RunContent::Cr(_) => out.push('\n'),
            _ => {}
        }
    }
    out
}

/// Concatenated `instrText` content of a run, or None if it has none.
fn run_instr_text(run: &ooxml_wml::types::Run) -> Option<String> {
    let mut out = String::new();
    let mut found = false;
    for rc in &run.run_content {
        if let ooxml_wml::types::RunContent::InstrText(t) = rc {
            if let Some(ref text) = t.text {
                out.push_str(text);
            }
            found = true;
        }
    }
    found.then_some(out)
}

/// Re-measure an item's text the same way `collect_items` did, so a field whose
/// result changed (PAGE/NUMPAGES) gets a correct width — not the stale width of
/// its cached one-digit result. `faces` is the caller's shaping scope;
/// a caller with only one item to measure passes a fresh `FaceCache`.
pub(crate) fn measure_item_text<'f>(text: &str, style: &RunStyle, font_cache: &'f FontCache, faces: &FaceCache<'f>) -> (f64, f64, Option<ShapedRun>) {
    if text.is_empty() {
        return (0.0, 0.0, None);
    }
    let font = font_cache.resolve(&style.family, if style.bold { 700 } else { 400 }, style.italic);
    if let Some(mut run) = font.and_then(|f| faces.shape(text, f, style.font_pt)) {
        if style.char_spacing_px.abs() > 0.001 {
            let n = run.glyphs.len();
            for g in &mut run.glyphs {
                g.x_advance += style.char_spacing_px;
            }
            run.total_advance += style.char_spacing_px * n as f64;
            run.is_simple = false;
        }
        let cw = run.total_advance - run.trailing_ws_advance();
        (run.total_advance, cw, Some(run))
    } else {
        let cc = text.chars().count() as f64;
        let est = cc * pt_to_px(style.font_pt) * 0.5 + cc * style.char_spacing_px;
        (est, est, None)
    }
}

/// Resolve a field instruction string to a renderer-substituted live field
/// code, or None for fields whose cached result we render as-is.
/// (Other fields go to `field::resolve_doc_field` at collect time.)
fn resolve_field_code(instr: &str) -> Option<FieldCode> {
    let instr = instr.trim().to_uppercase();
    if instr == "PAGE" || instr.starts_with("PAGE ") {
        Some(FieldCode::Page)
    } else if instr == "NUMPAGES" || instr.starts_with("NUMPAGES ") {
        Some(FieldCode::NumPages)
    } else {
        None
    }
}

#[derive(Clone)]
pub(crate) struct ImageItem {
    pub(crate) rel_id: String,
    /// Intrinsic display height in CSS pixels (from `<wp:extent cy="...">`),
    /// i.e. the height the bitmap is actually drawn at — NOT including the
    /// distT/distB text-distance padding (§20.4.2.8). The padding is added
    /// separately to the line's vertical extent so adjacent lines keep clear.
    pub(crate) height_px: f64,
    /// Text-distance padding in CSS pixels (§20.4.2.8 distL/distT/distR/distB,
    /// type ST_WrapDistance §20.4.3.6). distL/distR widen the item box so
    /// surrounding text keeps its horizontal gap; distT/distB grow the line's
    /// vertical extent so the lines above/below keep their gap. Default 0.
    pub(crate) dist_l_px: f64,
    pub(crate) dist_t_px: f64,
    pub(crate) dist_r_px: f64,
    pub(crate) dist_b_px: f64,
}

impl ImageItem {
    /// The painted bitmap box `(x, y, w, h)` for this inline image, given its
    /// item's left edge `x_left`, the item box width `item_width`, and the line
    /// `baseline_y` (§20.4.2.8): inset distL from the left, intrinsic width (box
    /// minus L/R padding), lifted distB + height above the baseline. Single source
    /// for the renderer's draw box and `select.rs`'s hit/outline rect.
    pub(crate) fn painted_box(&self, x_left: f64, item_width: f64, baseline_y: f64) -> (f64, f64, f64, f64) {
        (
            x_left + self.dist_l_px,
            baseline_y - self.dist_b_px - self.height_px,
            item_width - self.dist_l_px - self.dist_r_px,
            self.height_px,
        )
    }
}

impl Item {
    pub(crate) fn visible_text(&self) -> String {
        self.text.chars().filter(|c| !is_layout_control(*c)).collect()
    }
    pub(crate) fn is_only_whitespace(&self) -> bool {
        self.image.is_none() && self.text.chars().all(char::is_whitespace)
    }
}

pub(crate) struct LayoutLine {
    pub(crate) items: Vec<Item>,
    width: f64,
    pub(crate) max_ascent: f64,
    pub(crate) max_descent: f64,
    pub(crate) max_font_pt: f64,
    /// Height of the tallest inline image on this line (0 if text-only).
    /// Kept separate from text metrics so Auto line spacing can scale text
    /// without shrinking images.
    pub(crate) max_image_height: f64,
    /// Set when the line ends with a `<w:br w:type="page"/>` — pagination
    /// closes the current page after placing this line.
    pub(crate) ends_with_page_break: bool,
}

impl LayoutLine {
    pub(crate) fn new() -> Self {
        Self {
            items: Vec::new(),
            width: 0.0,
            max_ascent: 0.0,
            max_descent: 0.0,
            max_font_pt: DEFAULT_FONT_PT,
            max_image_height: 0.0,
            ends_with_page_break: false,
        }
    }
    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    fn push(&mut self, item: Item, font_cache: &FontCache) {
        self.width += item.width;
        accumulate_item_metrics(
            &item, &mut self.max_ascent, &mut self.max_descent,
            &mut self.max_font_pt, &mut self.max_image_height, font_cache,
        );
        self.items.push(item);
    }
    pub(crate) fn width(&self) -> f64 {
        self.width
    }
}

#[cfg(test)]
mod decimal_tab_tests {
    use super::*;
    use crate::engine::paint::RunStyle;

    /// A text item with no shaped run, so `decimal_pre_separator_width` takes the
    /// estimate path (proportional split by char count). `width` stands in for
    /// the item's measured advance; chars are assumed uniform here for the test.
    fn item(text: &str, width: f64) -> Item {
        Item {
            text: text.to_string(),
            style: RunStyle::marker(),
            width,
            content_width: width,
            char_start: 0,
            char_len: text.len() as u32,
            ..Default::default()
        }
    }

    #[test]
    fn separator_in_single_item_splits_at_dot() {
        // "3.14": 4 chars, width 40 → 1 char before '.' → 10px pre-separator.
        let pre = decimal_pre_separator_width(&[item("3.14", 40.0)]);
        assert!((pre - 10.0).abs() < 1e-9, "pre={pre}");
    }

    #[test]
    fn longer_integer_part_widens_pre_separator() {
        // "100.5": 5 chars, width 50 → 3 chars before '.' → 30px.
        let pre = decimal_pre_separator_width(&[item("100.5", 50.0)]);
        assert!((pre - 30.0).abs() < 1e-9, "pre={pre}");
    }

    #[test]
    fn no_separator_returns_full_span_width() {
        // "42" has no '.'/',' → whole span width (right-align fallback).
        let pre = decimal_pre_separator_width(&[item("42", 20.0)]);
        assert!((pre - 20.0).abs() < 1e-9, "pre={pre}");
    }

    #[test]
    fn comma_separator_is_recognized() {
        // Locale ',' decimal: "3,5" → 1 char before ',' of 3 → width/3.
        let pre = decimal_pre_separator_width(&[item("3,5", 30.0)]);
        assert!((pre - 10.0).abs() < 1e-9, "pre={pre}");
    }

    #[test]
    fn separator_spanning_multiple_items_sums_prior_widths() {
        // Span split across items (e.g. label + number after a break): the '.'
        // sits in the 2nd item; prior item widths are fully counted, then the
        // partial up to the dot.
        let span = [item("100", 30.0), item("0.5", 30.0)];
        // 2nd item "0.5": 3 chars, width 30 → 1 char before '.' → 10. Plus 30.
        let pre = decimal_pre_separator_width(&span);
        assert!((pre - 40.0).abs() < 1e-9, "pre={pre}");
    }

    #[test]
    fn first_separator_wins() {
        // "1.2.3" must align on the FIRST dot, not the last.
        let pre = decimal_pre_separator_width(&[item("1.2.3", 50.0)]);
        // 5 chars, width 50 → 1 char before first '.' → 10.
        assert!((pre - 10.0).abs() < 1e-9, "pre={pre}");
    }
}
