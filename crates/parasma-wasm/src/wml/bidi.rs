//! Bidirectional text (§17.3.1.6 `w:bidi`, §17.3.2.30 `w:rtl`) — UAX#9 rule-L2
//! reordering of a line's items into visual order, and the start/end alignment
//! swap under an RTL base direction. Relocated verbatim from `lib.rs`.

use crate::engine::fonts::text_has_complex;
use crate::Item;
use ooxml_wml::types::STJc;

/// UAX#9 rule L2 over a per-item embedding-level array → the visual order
/// (item indices). Reverse contiguous item runs from the highest level down to
/// the lowest odd level. Items are uniform-level (items are segmented by script),
/// so we reorder at item granularity rather than per character.
pub(crate) fn bidi_l2_order(levels: &[u8]) -> Vec<usize> {
    let n = levels.len();
    let mut order: Vec<usize> = (0..n).collect();
    if n < 2 { return order; }
    let highest = *levels.iter().max().unwrap();
    let lowest_odd = levels.iter().copied().filter(|l| l % 2 == 1).min().unwrap_or(highest + 1);
    let mut level = highest;
    while level >= lowest_odd && level >= 1 {
        let mut i = 0;
        while i < n {
            if levels[order[i]] >= level {
                let start = i;
                while i < n && levels[order[i]] >= level { i += 1; }
                order[start..i].reverse();
            } else { i += 1; }
        }
        level -= 1;
    }
    order
}

/// Could rule L2 permute this run of items?
///
/// The negation of `bidi_reorder_line`'s fast path, hoisted so the two cannot
/// drift: an LTR paragraph with no complex-script character is left in logical
/// order, and only paragraphs for which this is TRUE need their pristine logical
/// item order preserved for the re-pack sites (`ParagraphCache::logical_items`).
/// Over-approximate by design — a paragraph can satisfy this and still not
/// permute — because the cost of a false positive is one `Vec<Item>` clone on a
/// bidi document, and the cost of a false negative is silent corruption.
pub(crate) fn may_reorder(items: &[Item], base_rtl: bool) -> bool {
    base_rtl || items.iter().any(|it| text_has_complex(&it.text))
}

/// Reorder a line's items into UAX#9 visual order (rule L2) for display, using
/// the paragraph base direction from `<w:bidi/>` (§17.3.1.6, "HL1 in UAX#9").
/// Item-granular: each item's level is that of its first character (uniform
/// after script segmentation); empty/non-text items take the base
/// level. Run `<w:rtl/>` (§17.3.2.30) already routed those runs to the cs slot
/// at font-slot selection; here the Arabic/Hebrew characters' strong-R class drives
/// the reorder. No-op for lines with no complex-script characters (the common
/// Latin/CJK case pays only a cheap scan). v1 simplifications (documented):
/// per-line level computation (not full paragraph isolating-run-sequence), and
/// `<w:dir>` embedding (§17.3.2.8) / explicit `<w:rtl/>` neutral-override are
/// approximated by UAX#9's standard resolution.
pub(crate) fn bidi_reorder_line(items: &mut [Item], base_rtl: bool) {
    // Stamp each item's bidi-level parity (`Item.rtl`) for
    // reorder/paint semantics, BEFORE any early return and BEFORE the L2 permute
    // so it cannot perturb the permutation. A degenerate line (0/1 item) takes
    // the base direction; the no-complex LTR fast path leaves the collect-time
    // `rtl = false` (all levels even). This is distinct from `ShapedRun.rtl`
    // (actual glyph storage), which the x↔byte mirror gates on — the two can
    // disagree (a neutral-only item under an RTL base carries an odd level yet
    // shapes LTR). Not load-bearing for selection (selectionRects gates on
    // `ShapedRun.rtl`); it is a cheap correctness nicety for future consumers.
    if items.len() < 2 {
        if let Some(it) = items.first_mut() { it.rtl = base_rtl; }
        return;
    }
    if !may_reorder(items, base_rtl) { return; }
    let mut text = String::new();
    let mut offsets = Vec::with_capacity(items.len());
    for it in items.iter() { offsets.push(text.len()); text.push_str(&it.text); }
    let base = if base_rtl { unicode_bidi::Level::rtl() } else { unicode_bidi::Level::ltr() };
    let info = unicode_bidi::BidiInfo::new(&text, Some(base));
    let base_num = base.number();
    let levels: Vec<u8> = items.iter().enumerate()
        .map(|(i, it)| if it.text.is_empty() { base_num } else { info.levels[offsets[i]].number() })
        .collect();
    for (i, lvl) in levels.iter().enumerate() { items[i].rtl = lvl % 2 == 1; }
    let order = bidi_l2_order(&levels);
    if order.iter().enumerate().all(|(i, &o)| i == o) { return; } // already in order
    let reordered: Vec<Item> = order.iter().map(|&i| items[i].clone()).collect();
    items.clone_from_slice(&reordered);
}

/// §17.3.1.6 bidi: `jc` start/end is relative to the paragraph base direction,
/// so under RTL the start edge is the right margin. Swap Start↔End; Left/Right
/// are absolute and Center/justify unchanged.
pub(crate) fn rtl_swap_jc(jc: STJc) -> STJc {
    match jc {
        STJc::Start => STJc::End,
        STJc::End => STJc::Start,
        other => other,
    }
}
