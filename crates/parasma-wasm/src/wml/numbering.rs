//! Numbering / list support (§17.9) — layout-side marker resolution.
//!
//! Resolves a paragraph's numbering reference into a renderable marker:
//! `numPr` resolution, the per-list counters, and `lvlText` `%n`
//! substitution; the numbers themselves are written by the shared
//! `engine::counter`. The load-side `word/numbering.xml` parse that builds
//! the `(num_id, ilvl)` → `NumberingLevelDef` lookup is
//! `wml::loader::build_numbering_lookup`.

use std::collections::{HashMap, HashSet};

use ooxml_wml::ext::StyleContext;
use ooxml_wml::types::{STLevelSuffix, STNumberFormat};

use crate::wml::paragraph::paragraph_style_chain;
use crate::engine::paint::Underline;
use crate::wml::symbol_font::{SymbolFace, SymbolFont};

/// §17.9.24 "Numbering Symbol Run Properties" — the digest of a `<w:lvl>`'s
/// own `<w:rPr>`, i.e. the formatting of the NUMBERING TEXT (marker) itself.
/// `None` = the level rPr does not speak to that property; the marker keeps
/// its run-derived base value. Overlaid onto the marker's `RunStyle` in
/// `layout_paragraph` AFTER the run-decoration strip (which keeps the run's
/// underline/strike off the marker), so the level's own u/strike win.
///
/// Deliberate floors (do NOT silently widen): overlay set =
/// rFonts(ascii→hAnsi)/sz/b/i/color(plain hex)/u/strike/dstrike ONLY. Not
/// overlaid: themeColor+tint/shade, asciiTheme/hAnsiTheme (would need theme
/// threading into the loader digest), szCs/bCs/iCs and eastAsia/cs slots (the
/// marker shapes single-slot via `style.family`), caps/smallCaps, vanish
/// (Word can hide numbers via lvl-rPr vanish — deliberately out), highlight,
/// shading, w:spacing, vertAlign, position, outline/shadow/emboss/imprint,
/// lang, kern.
#[derive(Clone, Default)]
pub(crate) struct MarkerRunOverrides {
    /// `w:rFonts` ascii, else hAnsi — the RAW document family name (resolved
    /// against the font cache only at overlay time, keep-base-on-miss).
    pub(crate) family: Option<String>,
    /// `w:sz` half-points / 2.
    pub(crate) font_pt: Option<f64>,
    /// Plain `check_toggle`, NOT the §17.7.3 style-chain toggle-XOR — the
    /// §17.9.24 level rPr is standalone direct formatting on the numbering
    /// text (bare `<w:b/>` → true, `w:val="0"` → false).
    pub(crate) bold: Option<bool>,
    pub(crate) italic: Option<bool>,
    /// `"#RRGGBB"`; non-hex values ("auto") are skipped (marker keeps the
    /// run-derived color); themeColor is a floor (see above).
    pub(crate) color: Option<String>,
    /// Only when the LEVEL rPr carries `<w:u>` WITH a `w:val`. Note
    /// `<w:u w:val="none"/>` → `Some(Underline::None)` IS applied — a no-op
    /// after the run-decoration strip today, but it keeps the §17.9.24
    /// semantics if the marker base ever changes.
    pub(crate) underline: Option<Underline>,
    pub(crate) strikethrough: Option<bool>,
    pub(crate) double_strike: Option<bool>,
}

impl MarkerRunOverrides {
    pub(crate) fn from_rpr(rpr: &ooxml_wml::types::RunProperties) -> Self {
        Self {
            family: rpr.fonts.as_ref().and_then(|f| f.ascii.clone().or_else(|| f.h_ansi.clone())),
            font_pt: rpr.size.as_ref().and_then(|s| s.value.parse::<f64>().ok()).map(|hp| hp / 2.0),
            bold: crate::check_toggle(&rpr.bold),
            italic: crate::check_toggle(&rpr.italic),
            // ST_HexColor is "auto" or exactly RRGGBB; anything else (corrupt
            // doc) is dropped rather than emitted as an invalid canvas
            // fillStyle, which set_fill_style_str would silently ignore.
            color: rpr.color.as_ref().map(|c| c.value.clone())
                .filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()))
                .map(|h| format!("#{h}")),
            underline: rpr.underline.as_ref().and_then(|u| u.value).map(crate::map_underline),
            strikethrough: crate::check_toggle(&rpr.strikethrough),
            double_strike: crate::check_toggle(&rpr.dstrike),
        }
    }
}

pub(crate) struct NumberingLevelDef {
    pub(crate) format: STNumberFormat,
    pub(crate) lvl_text: String,
    /// The abstract list whose counters this level advances: the num
    /// instance's own `abstractNumId` (§17.9.26: instances of one abstract
    /// list continue one sequence).
    pub(crate) list: i64,
    pub(crate) start: i64,
    /// §17.9.26 `startOverride`: the value the list's sequence is reset to
    /// the first time this num instance uses the level.
    pub(crate) start_override: Option<i64>,
    /// §17.9.23 `pStyle`: the paragraph style this level is bound to.
    pub(crate) p_style: Option<String>,
    /// §17.9.6 `w:lvlRestart` — 1-based index of the earliest level whose use
    /// restarts this level's counter. `None` = the default (restart whenever
    /// any earlier level is used). `Some(0)` = never restart.
    pub(crate) lvl_restart: Option<i64>,
    /// The level's own indents (§17.9.22 level pPr): left, and the first
    /// line's offset from it (positive `firstLine`, negative `hanging`).
    /// `None` where the level sets none.
    pub(crate) indent_left_twips: Option<i64>,
    pub(crate) indent_first_twips: Option<i64>,
    /// The level's own tab stops (§17.9.22 level pPr), typically the `num`
    /// stop after the marker, merged under the paragraph's own. A `num` stop
    /// past the hanging indent loses to the implicit stop at the hang, which
    /// comes first; Word was not measured there. Of the rest of the level pPr
    /// only `ind` is read; spacing, justification and right indent are not
    /// (FLOOR).
    pub(crate) tabs: Option<ooxml_wml::types::CTTabs>,
    /// §17.9.4 `isLgl`: every number in the label is written in decimal.
    pub(crate) legal: bool,
    /// §17.9.11 `lvlText@null`: the label is a null character: nothing is
    /// drawn, but the suffix still separates the text (an empty `lvlText`
    /// draws neither).
    pub(crate) null_text: bool,
    /// §17.9.28 `w:suff` — content between the level's number and the paragraph
    /// text (tab [default] / space / nothing). A tab follows tab-stop rules,
    /// so a marker wider than the hang moves the text to the next stop.
    pub(crate) suffix: STLevelSuffix,
    /// §17.9.24 — the level's own `<w:rPr>` digest (marker formatting).
    pub(crate) rpr: MarkerRunOverrides,
}

pub(crate) struct NumberingMarker {
    pub(crate) text: String,
    /// The face a marker set in a symbol font draws in, its text re-encoded
    /// (`wml::symbol_font`); `None` for a marker in any other font.
    pub(crate) symbol_face: Option<SymbolFace>,
    /// See `NumberingLevelDef`.
    pub(crate) indent_left_twips: Option<i64>,
    pub(crate) indent_first_twips: Option<i64>,
    pub(crate) tabs: Option<ooxml_wml::types::CTTabs>,
    /// The paragraph names its list itself (a direct `numPr`), rather than
    /// through its style: then the list's indents beat the style's.
    pub(crate) direct: bool,
    /// §17.9.28 `w:suff` separator type (see `NumberingLevelDef::suffix`).
    pub(crate) suffix: STLevelSuffix,
    /// The suffix applies although `text` is empty (`lvlText@null`).
    pub(crate) suffix_only: bool,
    /// The resolved list identity + counter captured at
    /// `resolve_marker_for` time (all in scope here). `count` is the live
    /// document-order counter value (survives mid-list `lvlRestart`, where
    /// reverse-parsing the rendered "vii." is not viable); `fmt` is this level's
    /// `numFmt`. Consumed only by `ParaMeta.numbering` for copy/extraction — the
    /// existing marker consumers ignore these fields (byte-identical).
    pub(crate) num_id: i64,
    pub(crate) ilvl: i64,
    pub(crate) count: i64,
    pub(crate) fmt: STNumberFormat,
    /// §17.9.24 — the level's own rPr digest, overlaid onto the marker's
    /// `RunStyle` in `layout_paragraph` (after the run-decoration strip).
    pub(crate) rpr: MarkerRunOverrides,
}

/// The running list counters of one layout pass, keyed by `(list, ilvl)`.
/// §17.9.26: every num instance of an abstract list continues one sequence,
/// and an instance's `startOverride` resets it once, on that instance's first
/// use of the level (measured in Word: 1, 2 · restart 1 · back on the first
/// instance 2).
#[derive(Default)]
pub(crate) struct ListCounters {
    values: HashMap<(i64, i64), i64>,
    /// `(num_id, ilvl)` pairs whose `startOverride` has applied.
    overridden: HashSet<(i64, i64)>,
}

/// Resolve the list marker for a paragraph: its rendered label string (`"1."`,
/// `"a)"`, `"•"`, multi-level `"1.2.3"`) plus the marker's run overrides and
/// indent, or `None` when the paragraph is not a list item. Advances the
/// list's counters as a side effect (so sequential list items
/// number correctly). Governing spec: §17.9 numbering — `numFmt`, `lvlText`
/// `%n` substitution (§17.9.11), and the level rPr digest (§17.9.24).
pub(crate) fn resolve_numbering_marker(
    paragraph: &ooxml_wml::types::Paragraph,
    levels: &HashMap<(i64, i64), NumberingLevelDef>,
    counters: &mut ListCounters,
    style_ctx: &StyleContext,
) -> Option<NumberingMarker> {
    let (num_id, ilvl, direct) = resolve_num_pr(paragraph, levels, style_ctx)?;
    let mut marker = resolve_marker_for(num_id, ilvl, levels, counters);
    marker.direct = direct;
    Some(marker)
}

/// A paragraph's list and level, and whether it names the list itself.
///
/// `numId` and `ilvl` resolve separately (§17.3.1.19), each from the
/// paragraph's own `numPr`, else the nearest style in its chain that sets
/// it. `numId` 0 removes numbering (§17.9.18).
///
/// DEVIATION (measured in Word, against §17.3.1.19 and §17.9.23): a level's
/// `pStyle` does not choose the level of a style-numbered paragraph. Word
/// uses the style's `ilvl` when one is set, even if the binding names
/// another level. With no `ilvl` anywhere, a direct `numId` numbers at level
/// 0; a style's `numId` numbers at level 0 only when some level of the list
/// is bound to that style, and otherwise not at all.
fn resolve_num_pr(
    paragraph: &ooxml_wml::types::Paragraph,
    levels: &HashMap<(i64, i64), NumberingLevelDef>,
    style_ctx: &StyleContext,
) -> Option<(i64, i64, bool)> {
    let own = paragraph.p_pr.as_deref().and_then(|p| p.num_pr.as_deref());
    let chain = paragraph_style_chain(paragraph, style_ctx);

    fn style_num_pr(style: &ooxml_wml::types::Style) -> Option<&ooxml_wml::types::NumberingProperties> {
        style.p_pr.as_deref().and_then(|p| p.num_pr.as_deref())
    }
    let (num_id, direct, supplier) = match own.and_then(|n| n.num_id.as_deref()) {
        Some(n) => (n.value, true, None),
        None => chain.iter().find_map(|s| {
            style_num_pr(s).and_then(|n| n.num_id.as_deref()).map(|n| (n.value, false, s.style_id.as_deref()))
        })?,
    };
    if num_id == 0 {
        return None;
    }
    let ilvl = own.and_then(|n| n.ilvl.as_deref())
        .or_else(|| chain.iter().find_map(|s| style_num_pr(s).and_then(|n| n.ilvl.as_deref())))
        .map(|i| i.value);
    let ilvl = match ilvl {
        Some(ilvl) => ilvl,
        None if direct => 0,
        None => {
            let bound = levels.iter()
                .any(|((n, _), l)| *n == num_id && l.p_style.is_some() && l.p_style.as_deref() == supplier);
            if !bound {
                return None;
            }
            0
        }
    };
    Some((num_id, ilvl, direct))
}

/// Counter-advancing core (§17.9): given the resolved `(num_id, ilvl)`, advance
/// this level's counter, apply lvlRestart to deeper levels (§17.9.6), and build
/// the `lvlText` marker with multi-level `%n` substitution (§17.9.11). Split out
/// from `resolve_numbering_marker` so the counter logic is unit-testable without
/// constructing a `Paragraph`/`StyleContext`.
pub(crate) fn resolve_marker_for(
    num_id: i64,
    ilvl: i64,
    levels: &HashMap<(i64, i64), NumberingLevelDef>,
    counters: &mut ListCounters,
) -> NumberingMarker {
    // A paragraph can reference a level (num_id, ilvl) the abstractNum never
    // defines (e.g. a nested item whose `<w:lvl ilvl="1">` is missing). Word
    // synthesizes a default rather than dropping the marker+indent (§17.9): a
    // bullet at left indent 720·(ilvl+1) twips with a 360 hanging outdent.
    let default_level;
    let level = match levels.get(&(num_id, ilvl)) {
        Some(l) => l,
        None => {
            default_level = NumberingLevelDef {
                format: STNumberFormat::Bullet,
                lvl_text: "\u{2022}".to_string(),
                // The instance's list, from any level it does define; a num
                // with no levels at all counts on a key no list uses.
                list: levels.iter().find(|((n, _), _)| *n == num_id).map_or(i64::MIN, |(_, l)| l.list),
                start: 1,
                start_override: None,
                p_style: None,
                lvl_restart: None,
                indent_left_twips: Some(720 * (ilvl + 1)),
                indent_first_twips: Some(-360),
                tabs: None,
                legal: false,
                null_text: false,
                suffix: STLevelSuffix::Tab,
                rpr: MarkerRunOverrides::default(),
            };
            &default_level
        }
    };

    // Advance this level's counter (§17.9.11: the % number increments for each
    // subsequent paragraph of that level until its restart level intervenes).
    let list = level.list;
    if let Some(start) = level.start_override {
        if counters.overridden.insert((num_id, ilvl)) {
            counters.values.insert((list, ilvl), start - 1);
        }
    }
    let counter = counters.values.entry((list, ilvl)).or_insert(level.start - 1);
    *counter += 1;
    let current_count = *counter;

    // §17.9.6 lvlRestart — advancing level `ilvl` resets every DEEPER level
    // `L > ilvl` whose restart rule fires. For a level L with lvlRestart = N
    // (1-based), L restarts when a level with 0-based index ≤ N-1 is used, i.e.
    // when `ilvl < N`. Omitted (None) → restart whenever any earlier level is
    // used → always (since ilvl < L). A value of 0 → never restart. "If the
    // specified level is higher than the current level [N-1 ≥ L] the element is
    // ignored" → falls back to the default (always restart). We reset by
    // removing the counter entry so its next use re-seeds from `start - 1`.
    let mut deeper: Vec<i64> = counters.values.keys()
        .filter(|(lst, l)| *lst == list && *l > ilvl)
        .map(|(_, l)| *l)
        .collect();
    deeper.sort_unstable();
    for l in deeper {
        let restarts = match levels.get(&(num_id, l)).and_then(|d| d.lvl_restart) {
            Some(0) => false,                 // never restart
            Some(n) if n - 1 < l => ilvl < n, // valid restart index (1-based)
            _ => true,                        // omitted, or specified ≥ L → default
        };
        if restarts {
            counters.values.remove(&(list, l));
        }
    }

    // The level's font (§17.9.24) — a symbol font gives its codes their own
    // meaning, so the marker text is re-encoded below.
    let symbol_font = level.rpr.family.as_deref().and_then(SymbolFont::from_family);
    let text = match level.format {
        STNumberFormat::Bullet => {
            // §17.9 the `lvlText` value can carry XML char references (e.g.
            // `&#8226;` for •). The parser hands them through verbatim, so decode
            // before measuring/rendering — otherwise the marker shapes as the
            // literal "&#8226;" string, which is ~6 glyphs wide and overflows its
            // hanging slot (this is what the marker-overflow advance would otherwise
            // expose; decoding keeps a real • narrow).
            // An empty lvlText draws no marker (§17.9.11): Word shows none.
            let raw = decode_lvl_text_entities(&level.lvl_text);
            if symbol_font.is_none() && (raw.contains('\u{f0b7}') || raw.contains('\u{f0a7}')) {
                "\u{2022}".to_string()
            } else {
                raw
            }
        }
        _ => {
            // §17.9.11 multi-level substitution: each `%n` is the 1-based index
            // of an ancestor (or this) level's counter, formatted per THAT
            // level's numFmt, or in decimal when this level is legal (§17.9.4
            // isLgl). A `%n` naming a deeper level than this one is dropped.
            // An ancestor never yet used shows its start value (the counter
            // HashMap has no entry → fall back to start).
            let mut result = decode_lvl_text_entities(&level.lvl_text);
            for deeper in (ilvl + 1)..9 {
                result = result.replace(&format!("%{}", deeper + 1), "");
            }
            for lvl_ref in (0..=ilvl).rev() {
                let token = format!("%{}", lvl_ref + 1);
                if result.contains(&token) {
                    let ref_def = levels.get(&(num_id, lvl_ref));
                    let ref_counter = if lvl_ref == ilvl {
                        current_count
                    } else {
                        counters.values.get(&(list, lvl_ref)).copied().unwrap_or_else(|| {
                            ref_def.map(|d| d.start).unwrap_or(1)
                        })
                    };
                    let ref_format = if level.legal {
                        &STNumberFormat::Decimal
                    } else {
                        ref_def.map(|l| &l.format).unwrap_or(&level.format)
                    };
                    result = result.replace(&token, &format_number(ref_counter, ref_format));
                }
            }
            result
        }
    };

    let (text, symbol_face) = match symbol_font {
        Some(font) => {
            let (text, face) = font.reencode(&text);
            (text, Some(face))
        }
        None => (text, None),
    };

    NumberingMarker {
        suffix_only: level.null_text && text.is_empty(),
        text,
        symbol_face,
        indent_left_twips: level.indent_left_twips,
        indent_first_twips: level.indent_first_twips,
        tabs: level.tabs.clone(),
        direct: false,
        suffix: level.suffix,
        // Capture the resolved list identity + live counter for copy.
        num_id,
        ilvl,
        count: current_count,
        fmt: level.format,
        rpr: level.rpr.clone(),
    }
}

/// The substitute faces the levels' markers draw in — for the font plan, which
/// fetches them before layout. Every defined level counts, drawn or not: the
/// plan is built before the counters that decide which levels are drawn.
pub(crate) fn marker_face_families(
    levels: &HashMap<(i64, i64), NumberingLevelDef>,
) -> impl Iterator<Item = &'static str> + '_ {
    levels.values().filter_map(|level| {
        let font = level.rpr.family.as_deref().and_then(SymbolFont::from_family)?;
        font.reencode(&decode_lvl_text_entities(&level.lvl_text)).1.family()
    })
}

/// Decode the XML char references the parser leaves verbatim in a `lvlText`
/// value: the five named entities plus decimal (`&#8226;`) and hex (`&#x2022;`)
/// numeric references. Unknown/malformed references are left untouched. Scoped
/// to numbering markers (§17.9) so the marker's measured width reflects the real
/// glyph, not the literal entity string. Delegates to the ONE shared decoder,
/// keeping malformed references verbatim as this site always has.
fn decode_lvl_text_entities(s: &str) -> String {
    crate::engine::vml::decode_char_refs_verbatim(s)
}

/// A level's number in its `numFmt` (§17.18.59), through the shared
/// formatter. Declared: a custom format (`numFmt@format`) and the forms with
/// no formatter here (Vietnamese and Hindi counting, Thai counting, Hindi
/// vowels and consonants, Korean legal, Taiwanese digital, Chinese legal
/// simplified) are written in decimal, and letter formats always use the
/// Latin alphabet whatever the run's language.
pub(crate) fn format_number(n: i64, format: &STNumberFormat) -> String {
    crate::engine::counter::format(n, counter_style(format))
}

/// The shared counter style of a WML number format.
pub(crate) fn counter_style(format: &STNumberFormat) -> crate::engine::counter::CounterStyle {
    use crate::engine::counter::CounterStyle as C;
    use STNumberFormat as F;
    match format {
        F::DecimalZero => C::DecimalZero,
        F::UpperRoman => C::UpperRoman,
        F::LowerRoman => C::LowerRoman,
        F::UpperLetter => C::UpperLetter,
        F::LowerLetter => C::LowerLetter,
        F::Ordinal => C::Ordinal,
        F::CardinalText => C::CardinalText,
        F::OrdinalText => C::OrdinalText,
        F::DollarText => C::DollarText,
        F::Hex => C::Hex,
        F::Chicago => C::Chicago,
        F::NumberInDash => C::NumberInDash,
        F::DecimalEnclosedCircle | F::DecimalEnclosedCircleChinese => C::DecimalEnclosedCircle,
        F::DecimalEnclosedFullstop => C::DecimalEnclosedFullstop,
        F::DecimalEnclosedParen => C::DecimalEnclosedParen,
        F::DecimalFullWidth | F::DecimalFullWidth2 => C::DecimalFullWidth,
        F::IdeographDigital | F::JapaneseDigitalTenThousand => C::IdeographDigital,
        F::JapaneseCounting | F::ChineseCounting | F::ChineseCountingThousand
        | F::TaiwaneseCounting | F::TaiwaneseCountingThousand | F::IdeographLegalTraditional
        | F::JapaneseLegal => C::IdeographCounting,
        F::IdeographTraditional => C::IdeographTraditional,
        F::IdeographZodiac | F::IdeographZodiacTraditional => C::IdeographZodiac,
        F::Aiueo => C::Aiueo,
        F::AiueoFullWidth => C::AiueoFullWidth,
        F::Iroha => C::Iroha,
        F::IrohaFullWidth => C::IrohaFullWidth,
        F::Ganada => C::Ganada,
        F::Chosung => C::Chosung,
        F::KoreanDigital | F::KoreanDigital2 => C::KoreanDigital,
        F::KoreanCounting => C::KoreanCounting,
        F::Hebrew1 => C::Hebrew1,
        F::Hebrew2 => C::Hebrew2,
        F::ArabicAlpha => C::ArabicAlpha,
        F::ArabicAbjad => C::ArabicAbjad,
        F::RussianLower => C::RussianLower,
        F::RussianUpper => C::RussianUpper,
        F::ThaiLetters => C::ThaiLetters,
        F::ThaiNumbers => C::ThaiNumbers,
        F::HindiNumbers => C::HindiNumbers,
        F::None => C::None,
        _ => C::Decimal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_numeric_and_named_lvl_text_entities() {
        // §17.9 char references the parser leaves verbatim. A bullet `&#8226;`
        // must become the single • glyph (so it measures ~6px, not the literal
        // 7-char string that would overflow its hanging slot — the overflow trigger).
        assert_eq!(decode_lvl_text_entities("&#8226;"), "\u{2022}");
        assert_eq!(decode_lvl_text_entities("&#x2022;"), "\u{2022}");
        assert_eq!(decode_lvl_text_entities("&#X2022;"), "\u{2022}");
        assert_eq!(decode_lvl_text_entities("a &amp; b"), "a & b");
        assert_eq!(decode_lvl_text_entities("%1)"), "%1)"); // no entity, untouched
        // Malformed / unknown refs are preserved verbatim rather than dropped.
        assert_eq!(decode_lvl_text_entities("R&D"), "R&D");
        assert_eq!(decode_lvl_text_entities("&unknown;"), "&unknown;");
        assert_eq!(decode_lvl_text_entities("&#zzz;"), "&#zzz;");
    }

    fn def(fmt: STNumberFormat, text: &str, restart: Option<i64>) -> NumberingLevelDef {
        NumberingLevelDef {
            format: fmt,
            lvl_text: text.to_string(),
            list: 0,
            start: 1,
            start_override: None,
            p_style: None,
            lvl_restart: restart,
            indent_left_twips: Some(720),
            indent_first_twips: Some(-360),
            tabs: None,
            legal: false,
            null_text: false,
            suffix: STLevelSuffix::Tab,
            rpr: MarkerRunOverrides::default(),
        }
    }

    /// 3-level decimal outline (§17.9.11 multi-level + §17.9.6 default restart):
    /// "%1" / "%1.%2" / "%1.%2.%3" must read 1, 1.1, 1.1.1, 1.1.2, 1.2, 2, 2.1 —
    /// each ancestor counter substituted and deeper levels restarting when an
    /// earlier level advances.
    #[test]
    fn multilevel_outline_with_default_restart() {
        let mut levels = HashMap::new();
        levels.insert((1, 0), def(STNumberFormat::Decimal, "%1", None));
        levels.insert((1, 1), def(STNumberFormat::Decimal, "%1.%2", None));
        levels.insert((1, 2), def(STNumberFormat::Decimal, "%1.%2.%3", None));
        let mut c = ListCounters::default();
        let mark = |il, c: &mut ListCounters| {
            resolve_marker_for(1, il, &levels, c).text
        };
        assert_eq!(mark(0, &mut c), "1");
        assert_eq!(mark(1, &mut c), "1.1");
        assert_eq!(mark(2, &mut c), "1.1.1");
        assert_eq!(mark(2, &mut c), "1.1.2");
        assert_eq!(mark(1, &mut c), "1.2");
        assert_eq!(mark(0, &mut c), "2"); // restarts levels 1 and 2
        assert_eq!(mark(1, &mut c), "2.1");
    }

    /// §17.9.26, the three sequences measured in Word: instances of one
    /// abstract list share its counters; a startOverride resets the shared
    /// sequence once, on its instance's first use, and every instance then
    /// continues from it.
    #[test]
    fn instances_share_the_abstract_list_sequence() {
        let on = |list, start_override| NumberingLevelDef {
            list,
            start_override,
            ..def(STNumberFormat::Decimal, "%1", None)
        };
        let mut levels = HashMap::new();
        levels.insert((5, 0), on(4, None));
        levels.insert((6, 0), on(4, Some(1)));
        levels.insert((7, 0), on(2, None));
        levels.insert((8, 0), on(2, None));
        levels.insert((9, 0), on(3, None));
        levels.insert((10, 0), on(3, Some(5)));
        let mut c = ListCounters::default();
        let mut seq = |nums: &[i64]| -> Vec<String> {
            nums.iter().map(|&n| resolve_marker_for(n, 0, &levels, &mut c).text).collect()
        };
        assert_eq!(seq(&[5, 5, 6, 5]), ["1", "2", "1", "2"], "the spec example");
        assert_eq!(seq(&[7, 8, 7]), ["1", "2", "3"], "plain instances continue");
        assert_eq!(seq(&[9, 10, 10, 9]), ["1", "5", "6", "7"], "an override resets once");
    }

    /// §17.9.6 lvlRestart=0 (the spec example): level 2 (lowerRoman) NEVER
    /// restarts, while level 1 (upperLetter, default restart) resets when level
    /// 0 advances → 1) A) i) ii) 2) A) iii).
    #[test]
    fn lvl_restart_zero_never_resets() {
        let mut levels = HashMap::new();
        levels.insert((3, 0), def(STNumberFormat::Decimal, "%1)", None));
        levels.insert((3, 1), def(STNumberFormat::UpperLetter, "%2)", None));
        levels.insert((3, 2), def(STNumberFormat::LowerRoman, "%3)", Some(0)));
        let mut c = ListCounters::default();
        let mark = |il, c: &mut ListCounters| {
            resolve_marker_for(3, il, &levels, c).text
        };
        assert_eq!(mark(0, &mut c), "1)");
        assert_eq!(mark(1, &mut c), "A)");
        assert_eq!(mark(2, &mut c), "i)");
        assert_eq!(mark(2, &mut c), "ii)");
        assert_eq!(mark(0, &mut c), "2)"); // restarts level 1, NOT level 2
        assert_eq!(mark(1, &mut c), "A)");
        assert_eq!(mark(2, &mut c), "iii)"); // continues — never restarts
    }

    /// §17.9.24 — the `MarkerRunOverrides::from_rpr` digest maps the
    /// level rPr's fields exactly: bare toggles, val="0" negation, plain-hex
    /// color kept / "auto" skipped, hAnsi fallback for rFonts, `w:u val="none"`
    /// as an EXPLICIT Some(None-underline), and half-point size halving.
    #[test]
    fn marker_run_overrides_digest() {
        use ooxml_wml::types::{
            CTColor, CTUnderline, Fonts, HpsMeasureElement, OnOffElement, RunProperties,
            STUnderline,
        };

        // bare <w:b/> → Some(true); <w:i w:val="0"/> → Some(false)
        let rpr = RunProperties {
            bold: Some(Box::new(OnOffElement::default())),
            italic: Some(Box::new(OnOffElement { value: Some("0".into()), ..Default::default() })),
            ..Default::default()
        };
        let ov = MarkerRunOverrides::from_rpr(&rpr);
        assert_eq!(ov.bold, Some(true), "bare <w:b/> is an assertion (§17.9.24 direct formatting)");
        assert_eq!(ov.italic, Some(false), "w:val=\"0\" negates");
        assert!(ov.family.is_none() && ov.font_pt.is_none() && ov.color.is_none());
        assert!(ov.underline.is_none() && ov.strikethrough.is_none() && ov.double_strike.is_none());

        // color "auto" (non-hex) skipped; "FF0000" → "#FF0000"
        let color = |v: &str| RunProperties {
            color: Some(Box::new(CTColor {
                value: v.into(),
                theme_color: None,
                theme_tint: None,
                theme_shade: None,
            })),
            ..Default::default()
        };
        assert_eq!(MarkerRunOverrides::from_rpr(&color("auto")).color, None);
        assert_eq!(
            MarkerRunOverrides::from_rpr(&color("FF0000")).color,
            Some("#FF0000".to_string())
        );

        // rFonts hAnsi-only → family from hAnsi (ascii wins when present)
        let rpr = RunProperties {
            fonts: Some(Box::new(Fonts { h_ansi: Some("Wingdings".into()), ..Default::default() })),
            ..Default::default()
        };
        assert_eq!(MarkerRunOverrides::from_rpr(&rpr).family, Some("Wingdings".to_string()));

        // <w:u w:val="none"/> → Some(Underline::None); bare <w:u/> → None
        let underline = |v: Option<STUnderline>| RunProperties {
            underline: Some(Box::new(CTUnderline { value: v, ..Default::default() })),
            ..Default::default()
        };
        assert_eq!(
            MarkerRunOverrides::from_rpr(&underline(Some(STUnderline::None))).underline,
            Some(Underline::None)
        );
        assert_eq!(MarkerRunOverrides::from_rpr(&underline(None)).underline, None);

        // sz "48" half-points → 24pt
        let rpr = RunProperties {
            size: Some(Box::new(HpsMeasureElement { value: "48".into() })),
            ..Default::default()
        };
        assert_eq!(MarkerRunOverrides::from_rpr(&rpr).font_pt, Some(24.0));
    }

    /// An ancestor never yet used must show its start value, not a stale 1, in
    /// "%1.%2" when level 1 is the first paragraph emitted (jumping straight to
    /// a nested item). With ancestor start=3 the marker reads "3.1".
    #[test]
    fn unused_ancestor_uses_its_start() {
        let mut levels = HashMap::new();
        let mut l0 = def(STNumberFormat::Decimal, "%1", None);
        l0.start = 3;
        levels.insert((4, 0), l0);
        levels.insert((4, 1), def(STNumberFormat::Decimal, "%1.%2", None));
        let mut c = ListCounters::default();
        assert_eq!(resolve_marker_for(4, 1, &levels, &mut c).text, "3.1");
    }
}
