//! Numbering / list support (§17.9) — layout-side marker resolution.
//!
//! Resolves a paragraph's numbering reference into a renderable marker:
//! counter advancement, `lvlText` `%n` substitution, and the decimal/roman/
//! letter number formatters. The load-side `word/numbering.xml` parse that
//! builds the `(num_id, ilvl)` → `NumberingLevelDef` lookup stays in `lib.rs`
//! as WML-loader plumbing (it walks the parsed package at load time); it
//! constructs `NumberingLevelDef` via the now-`pub(crate)` fields here.
//! Verbatim relocation from `lib.rs`.

use std::collections::HashMap;

use ooxml_wml::ext::{ParagraphExt, StyleContext};
use ooxml_wml::types::{STLevelSuffix, STNumberFormat};

use crate::collect_paragraph_style_chain;
use crate::engine::paint::Underline;

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
    pub(crate) start: i64,
    /// §17.9.6 `w:lvlRestart` — 1-based index of the earliest level whose use
    /// restarts this level's counter. `None` = the default (restart whenever
    /// any earlier level is used). `Some(0)` = never restart.
    pub(crate) lvl_restart: Option<i64>,
    pub(crate) indent_left_twips: i64,
    pub(crate) indent_hanging_twips: i64,
    /// §17.9.28 `w:suff` — content between the level's number and the paragraph
    /// text (tab [default] / space / nothing). Drives how a marker that is
    /// WIDER than its hanging-indent slot advances the following text so it
    /// never overlaps.
    pub(crate) suffix: STLevelSuffix,
    /// §17.9.24 — the level's own `<w:rPr>` digest (marker formatting).
    pub(crate) rpr: MarkerRunOverrides,
}

pub(crate) struct NumberingMarker {
    pub(crate) text: String,
    pub(crate) indent_left_twips: i64,
    pub(crate) indent_hanging_twips: i64,
    /// §17.9.28 `w:suff` separator type (see `NumberingLevelDef::suffix`).
    pub(crate) suffix: STLevelSuffix,
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

/// Resolve the list marker for a paragraph: its rendered label string (`"1."`,
/// `"a)"`, `"•"`, multi-level `"1.2.3"`) plus the marker's run overrides and
/// indent, or `None` when the paragraph is not a list item. Advances the
/// per-`(numId, ilvl)` counters as a side effect (so sequential list items
/// number correctly). Governing spec: §17.9 numbering — `numFmt`, `lvlText`
/// `%n` substitution (§17.9.11), and the level rPr digest (§17.9.24).
pub(crate) fn resolve_numbering_marker(
    paragraph: &ooxml_wml::types::Paragraph,
    levels: &HashMap<(i64, i64), NumberingLevelDef>,
    counters: &mut HashMap<(i64, i64), i64>,
    style_ctx: &StyleContext,
) -> Option<NumberingMarker> {
    let numbering = paragraph.numbering().or_else(|| {
        let style_pprs = collect_paragraph_style_chain(paragraph, style_ctx);
        for ppr in &style_pprs {
            if let Some(ref num_pr) = ppr.num_pr {
                let num_id = num_pr.num_id.as_deref()?.value;
                let ilvl = num_pr.ilvl.as_deref().map(|i| i.value).unwrap_or(0);
                return Some((num_id, ilvl));
            }
        }
        None
    });
    let (num_id, ilvl) = numbering?;
    if num_id == 0 { return None; }
    Some(resolve_marker_for(num_id, ilvl, levels, counters))
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
    counters: &mut HashMap<(i64, i64), i64>,
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
                start: 1,
                lvl_restart: None,
                indent_left_twips: 720 * (ilvl + 1),
                indent_hanging_twips: 360,
                suffix: STLevelSuffix::Tab,
                rpr: MarkerRunOverrides::default(),
            };
            &default_level
        }
    };

    // Advance this level's counter (§17.9.11: the % number increments for each
    // subsequent paragraph of that level until its restart level intervenes).
    let counter = counters.entry((num_id, ilvl)).or_insert(level.start - 1);
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
    let mut deeper: Vec<i64> = counters.keys()
        .filter(|(nid, l)| *nid == num_id && *l > ilvl)
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
            counters.remove(&(num_id, l));
        }
    }

    // The two "•" arms are kept distinct on purpose: a symbol-font PUA bullet
    // (normalised to •) and an empty lvlText (default •) are separate causes.
    #[allow(clippy::if_same_then_else)]
    let text = match level.format {
        STNumberFormat::Bullet => {
            // §17.9 the `lvlText` value can carry XML char references (e.g.
            // `&#8226;` for •). The parser hands them through verbatim, so decode
            // before measuring/rendering — otherwise the marker shapes as the
            // literal "&#8226;" string, which is ~6 glyphs wide and overflows its
            // hanging slot (this is what the marker-overflow advance would otherwise
            // expose; decoding keeps a real • narrow). Symbol-font private-use
            // bullets still normalise to • first.
            let raw = decode_lvl_text_entities(&level.lvl_text);
            if raw.contains('\u{f0b7}') || raw.contains('\u{f0a7}') {
                "\u{2022}".to_string()
            } else if raw.is_empty() {
                "\u{2022}".to_string()
            } else {
                raw
            }
        }
        _ => {
            // §17.9.11 multi-level substitution: each `%n` is the 1-based index
            // of an ancestor (or this) level's counter, formatted per THAT
            // level's numFmt. A `%n` referencing a level higher than this one is
            // ignored (left as-is). An ancestor never yet used shows its start
            // value (the counter HashMap has no entry → fall back to start).
            let mut result = decode_lvl_text_entities(&level.lvl_text);
            for lvl_ref in (0..=ilvl).rev() {
                let token = format!("%{}", lvl_ref + 1);
                if result.contains(&token) {
                    let ref_def = levels.get(&(num_id, lvl_ref));
                    let ref_counter = if lvl_ref == ilvl {
                        current_count
                    } else {
                        counters.get(&(num_id, lvl_ref)).copied().unwrap_or_else(|| {
                            ref_def.map(|d| d.start).unwrap_or(1)
                        })
                    };
                    let ref_format = ref_def
                        .map(|l| &l.format)
                        .unwrap_or(&level.format);
                    result = result.replace(&token, &format_number(ref_counter, ref_format));
                }
            }
            result
        }
    };

    NumberingMarker {
        text,
        indent_left_twips: level.indent_left_twips,
        indent_hanging_twips: level.indent_hanging_twips,
        suffix: level.suffix,
        // Capture the resolved list identity + live counter for copy.
        num_id,
        ilvl,
        count: current_count,
        fmt: level.format,
        rpr: level.rpr.clone(),
    }
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

/// §17.18.59 ST_NumberFormat — maps a level's `numFmt` to the marker glyph
/// sequence (decimal / lowerLetter / upperLetter / lowerRoman / upperRoman).
/// FLOOR: lowerLetter/upperLetter fall back to DECIMAL past 26 (item 27 → "27"),
/// whereas §17.18.59 specifies repeated-letter cycling (aa, bb, cc). The PML
/// numbering path already implements that (`pml::shape::int_to_alpha`); WML
/// keeps the decimal fallback for now — folding the two onto one shared helper
/// is a known numbering follow-on (it would move pins on any >26 letter list,
/// so it needs its own red-green test).
fn format_number(n: i64, format: &STNumberFormat) -> String {
    match format {
        STNumberFormat::Decimal => n.to_string(),
        STNumberFormat::LowerLetter => {
            if (1..=26).contains(&n) {
                ((b'a' + (n - 1) as u8) as char).to_string()
            } else {
                n.to_string() // FLOOR: §17.18.59 wants "aa"; see banner
            }
        }
        STNumberFormat::UpperLetter => {
            if (1..=26).contains(&n) {
                ((b'A' + (n - 1) as u8) as char).to_string()
            } else {
                n.to_string() // FLOOR: §17.18.59 wants "AA"; see banner
            }
        }
        STNumberFormat::LowerRoman => to_roman(n).to_lowercase(),
        STNumberFormat::UpperRoman => to_roman(n),
        _ => n.to_string(),
    }
}

fn to_roman(mut n: i64) -> String {
    const TABLE: &[(i64, &str)] = &[
        (1000, "M"), (900, "CM"), (500, "D"), (400, "CD"),
        (100, "C"), (90, "XC"), (50, "L"), (40, "XL"),
        (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I"),
    ];
    let mut result = String::new();
    for &(value, numeral) in TABLE {
        while n >= value {
            result.push_str(numeral);
            n -= value;
        }
    }
    result
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
            start: 1,
            lvl_restart: restart,
            indent_left_twips: 720,
            indent_hanging_twips: 360,
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
        let mut c = HashMap::new();
        let mark = |il, c: &mut HashMap<(i64, i64), i64>| {
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

    /// §17.9.9 startOverride: the loader bakes the overridden start into the
    /// level def's `start`, so level 0 begins at 5 → 5, 5.1.
    #[test]
    fn start_override_offsets_first_counter() {
        let mut levels = HashMap::new();
        let mut l0 = def(STNumberFormat::Decimal, "%1", None);
        l0.start = 5;
        levels.insert((2, 0), l0);
        levels.insert((2, 1), def(STNumberFormat::Decimal, "%1.%2", None));
        let mut c = HashMap::new();
        assert_eq!(resolve_marker_for(2, 0, &levels, &mut c).text, "5");
        assert_eq!(resolve_marker_for(2, 1, &levels, &mut c).text, "5.1");
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
        let mut c = HashMap::new();
        let mark = |il, c: &mut HashMap<(i64, i64), i64>| {
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
        let mut c = HashMap::new();
        assert_eq!(resolve_marker_for(4, 1, &levels, &mut c).text, "3.1");
    }
}
