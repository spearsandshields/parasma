//! Font resolution, shaping, and metrics core.
//!
//! VERBATIM relocation from `lib.rs` — behavior changes are forbidden, enforced
//! by the oracle snapshot tests whose geometry hashes must not drift.

use std::collections::HashMap;

use super::color::extract_theme_color_hex;
use super::units::pt_to_px;

// Defaults when a run / paragraph doesn't override.
pub(crate) const DEFAULT_FONT_PT: f64 = 11.0;
pub(crate) const DEFAULT_FONT_FAMILY: &str = "Calibri";
/// The OpenType math face (a STIX fork carrying a real `MATH` table) that
/// `Cambria Math` / the OMML default math font resolve to. The
/// TS resolver seeds this family whenever a document contains an `oMath`, and
/// the engine routes every math glyph through it. When the face is ABSENT,
/// `FontCache::math_face` returns `None` and the math layout keeps its fallback
/// body-font proxies — nothing blanks or panics.
pub(crate) const MATH_FAMILY: &str = "XITS Math";
/// Fallback families for the East Asian / Complex Script font slots when the
/// run sets none (§17.3.2.26). Loaded on-demand (only when the doc actually
/// contains those scripts — see `collect_para_fonts`). The font-map substitutes
/// real CJK/Arabic font names (MS Mincho, SimSun, …) to these too.
pub(crate) const DEFAULT_EAST_ASIA_FAMILY: &str = "Noto Sans CJK";
pub(crate) const DEFAULT_COMPLEX_FAMILY: &str = "Noto Sans Arabic";

/// True if `text` contains any East Asian (CJK) character — used to load CJK
/// fonts on-demand only when needed.
pub(crate) fn text_has_east_asia(text: &str) -> bool {
    text.chars().any(|c| matches!(font_slot_class(c), FontSlot::EastAsia))
}
/// True if `text` contains any complex-script (Arabic/Hebrew/…) character.
pub(crate) fn text_has_complex(text: &str) -> bool {
    text.chars().any(|c| matches!(font_slot_class(c), FontSlot::ComplexScript))
}

// ─── Theme font resolution ──────────────────────────────────────────────────

pub(crate) struct ThemeFonts {
    pub(crate) major_latin: Option<String>,
    pub(crate) minor_latin: Option<String>,
    colors: HashMap<String, String>,
}

impl ThemeFonts {
    pub(crate) fn empty() -> Self {
        Self { major_latin: None, minor_latin: None, colors: HashMap::new() }
    }

    pub(crate) fn resolve_color(&self, theme_color: &ooxml_wml::types::STThemeColor) -> Option<&str> {
        use ooxml_wml::types::STThemeColor::*;
        let key = match theme_color {
            Dark1 | Text1 => "dk1",
            Light1 | Background1 => "lt1",
            Dark2 | Text2 => "dk2",
            Light2 | Background2 => "lt2",
            Accent1 => "accent1",
            Accent2 => "accent2",
            Accent3 => "accent3",
            Accent4 => "accent4",
            Accent5 => "accent5",
            Accent6 => "accent6",
            Hyperlink => "hlink",
            FollowedHyperlink => "folHlink",
            None => return Option::None,
        };
        self.colors.get(key).map(|s| s.as_str())
    }

    pub(crate) fn resolve(&self, theme: &ooxml_wml::types::STTheme) -> Option<&str> {
        use ooxml_wml::types::STTheme::*;
        match theme {
            MajorAscii | MajorHAnsi | MajorEastAsia | MajorBidi => {
                self.major_latin.as_deref()
            }
            MinorAscii | MinorHAnsi | MinorEastAsia | MinorBidi => {
                self.minor_latin.as_deref()
            }
        }
    }
}

pub(crate) fn parse_theme_fonts(xml: &str) -> ThemeFonts {
    let major = xml.find("<a:majorFont")
        .and_then(|start| {
            let chunk = &xml[start..];
            chunk.find("<a:latin").and_then(|lt| {
                let tag = &chunk[lt..];
                let end = tag.find("/>")?;
                let tag_str = &tag[..end];
                let ti = tag_str.find("typeface=\"")?;
                let val_start = ti + "typeface=\"".len();
                let val_end = tag_str[val_start..].find('"')? + val_start;
                Some(tag_str[val_start..val_end].to_string())
            })
        });
    let minor = xml.find("<a:minorFont")
        .and_then(|start| {
            let chunk = &xml[start..];
            chunk.find("<a:latin").and_then(|lt| {
                let tag = &chunk[lt..];
                let end = tag.find("/>")?;
                let tag_str = &tag[..end];
                let ti = tag_str.find("typeface=\"")?;
                let val_start = ti + "typeface=\"".len();
                let val_end = tag_str[val_start..].find('"')? + val_start;
                Some(tag_str[val_start..val_end].to_string())
            })
        });
    let mut colors = HashMap::new();
    let color_slots = ["dk1", "lt1", "dk2", "lt2",
        "accent1", "accent2", "accent3", "accent4", "accent5", "accent6",
        "hlink", "folHlink"];
    for slot in &color_slots {
        let tag = format!("<a:{}", slot);
        if let Some(start) = xml.find(&tag) {
            let chunk = &xml[start..];
            if let Some(end_tag) = chunk.find(&format!("</a:{}", slot)) {
                let inner = &chunk[..end_tag];
                if let Some(hex) = extract_theme_color_hex(inner) {
                    colors.insert(slot.to_string(), hex);
                }
            }
        }
    }
    ThemeFonts { major_latin: major, minor_latin: minor, colors }
}

// ─── Font storage ───────────────────────────────────────────────────────────

pub(crate) struct LoadedFont {
    data: Vec<u8>,
    index: u32,
    // head/hhea metrics in RAW font units, cached at insert time (the face is
    // already parsed there for validation). `font_metrics` is then pure
    // arithmetic instead of re-parsing the whole TrueType table directory per
    // item on every relayout (font toggle / resize). Raw units + the identical
    // casts keep `font_metrics` byte-identical to the old parse-on-call form.
    units_per_em: u16,
    ascender: i16,
    descender: i16,
    line_gap: i16,
    // Recorded from the face at load time; retained for face-selection debugging
    // and future weight/style matching (not read on the current shaping path).
    #[allow(dead_code)]
    weight: u16,
    #[allow(dead_code)]
    italic: bool,
}

impl LoadedFont {
    /// Raw font bytes — exposed for the OMML `MATH`-table reader (constants,
    /// size variants, glyph outlines), which re-parses a `ttf_parser::Face` per
    /// call. The normal shaping/metrics paths never touch this.
    pub(crate) fn data(&self) -> &[u8] {
        &self.data
    }
    /// TrueType-collection face index (0 for the standalone math OTF).
    pub(crate) fn index(&self) -> u32 {
        self.index
    }
    /// Design units per em, cached at load.
    pub(crate) fn units_per_em(&self) -> u16 {
        self.units_per_em
    }
}

#[derive(Hash, Eq, PartialEq, Clone)]
struct FontKey {
    family: String,
    weight: u16,
    italic: bool,
}

impl FontKey {
    /// Deterministic face-preference rank WITHIN one family: the upright regular
    /// cut first, then the nearest weight to regular, then upright before italic.
    ///
    /// `fonts` is a `HashMap`, and `std`'s `RandomState` reseeds per process, so
    /// iteration order is arbitrary and differs run to run. Every fallback that
    /// picked a face with `find`/`min_by` on a PARTIAL key (family alone) was
    /// therefore a nondeterministic input to layout, not merely to a debug dump:
    /// the cuts of one family disagree on advance width, so the same document
    /// measured differently on different runs. This is the same hazard
    /// `wml/loader.rs` already names and fixes for style ids.
    ///
    /// The rank is TOTAL over a family, which is the whole point — a partial key
    /// leaves the tie to be broken by iteration order again. `(weight, italic)` is
    /// unique per family because it is the rest of the key, and the trailing
    /// `weight` separates the two cuts that can share an `abs_diff` (300 and 500
    /// both rank 100); without it the fix would be a narrower coin toss, not a fix.
    fn cut_rank(&self) -> (u16, bool, u16) {
        (self.weight.abs_diff(400), self.italic, self.weight)
    }
}

pub(crate) struct FontCache {
    fonts: HashMap<FontKey, LoadedFont>,
    family_aliases: HashMap<String, String>,
}

impl FontCache {
    pub(crate) fn new() -> Self {
        Self {
            fonts: HashMap::new(),
            family_aliases: HashMap::new(),
        }
    }

    pub(crate) fn insert(&mut self, family: &str, data: Vec<u8>, weight: u16, italic: bool) -> Result<(), String> {
        // Parse once here (validation) and cache the metrics; the scoped block
        // releases the face's borrow of `data` before it moves into LoadedFont.
        let (units_per_em, ascender, descender, line_gap) = {
            let face = ttf_parser::Face::parse(&data, 0)
                .map_err(|e| format!("invalid font: {e}"))?;
            (face.units_per_em(), face.ascender(), face.descender(), face.line_gap())
        };
        let key = FontKey {
            family: family.to_lowercase(),
            weight,
            italic,
        };
        self.fonts.insert(key, LoadedFont {
            data, index: 0,
            units_per_em, ascender, descender, line_gap,
            weight, italic,
        });
        Ok(())
    }

    pub(crate) fn add_alias(&mut self, from: &str, to: &str) {
        self.family_aliases.insert(from.to_lowercase(), to.to_lowercase());
    }

    pub(crate) fn resolve_family_name(&self, family: &str) -> Option<String> {
        let lower = family.to_lowercase();
        self.family_aliases.get(&lower).cloned()
    }

    pub(crate) fn resolve(&self, family: &str, weight: u16, italic: bool) -> Option<&LoadedFont> {
        let lower = family.to_lowercase();
        let effective = self.family_aliases.get(&lower).unwrap_or(&lower);

        let keys = [
            Some(FontKey { family: effective.clone(), weight, italic }),
            Some(FontKey { family: effective.clone(), weight, italic: !italic }),
            if weight != 400 { Some(FontKey { family: effective.clone(), weight: 400, italic }) } else { None },
        ];

        for key in keys.into_iter().flatten() {
            if let Some(font) = self.fonts.get(&key) {
                return Some(font);
            }
        }

        // Fallback: the closest-to-regular cut of this family (`cut_rank`), NOT
        // whichever cut the HashMap happened to yield first.
        self.closest_cut(effective)
            .or_else(|| self.resolve_default_font())
    }

    /// EXACT-family lookup (OMML math face): resolve `family` (honoring the
    /// declared→substitute alias hop) to a loaded face WITHOUT the `resolve()`
    /// default fallback ladder. `resolve()` always hands back *something*
    /// (Calibri, then any face), which would make it impossible to detect that
    /// the math face is genuinely absent. `resolve_exact` returns `None` when no
    /// face in that family is loaded, which is the single guard the math layout
    /// uses to fall back to its body-font proxies. Prefers the upright regular
    /// cut, else the nearest weight/style in the family — which is exactly
    /// [`FontKey::cut_rank`], so this is one lookup rather than a `get` for the
    /// regular cut plus a separate fallback spelling the same preference again.
    pub(crate) fn resolve_exact(&self, family: &str) -> Option<&LoadedFont> {
        let lower = family.to_lowercase();
        let effective = self.family_aliases.get(&lower).unwrap_or(&lower);
        self.closest_cut(effective)
    }

    /// The loaded OpenType math face (`XITS Math`), or `None` when no math font
    /// is present. Uses [`resolve_exact`] so absence is detectable — the sole
    /// fallback guard for OMML math layout.
    pub(crate) fn math_face(&self) -> Option<&LoadedFont> {
        self.resolve_exact(MATH_FAMILY)
    }

    pub(crate) fn has_family(&self, family: &str) -> bool {
        let lower = family.to_lowercase();
        let effective = self.family_aliases.get(&lower).unwrap_or(&lower);
        self.fonts.keys().any(|k| k.family == *effective)
    }

    /// EXEMPT from the [`FontKey::cut_rank`] treatment, deliberately: this
    /// `min_by` also compares family alone over a `HashMap`, but every key that
    /// can tie it shares the one thing it returns — the family string. The result
    /// is already a pure function of the loaded set. Do not "fix" it by symmetry.
    pub(crate) fn default_family_name(&self) -> Option<String> {
        let lower = DEFAULT_FONT_FAMILY.to_lowercase();
        let effective = self.family_aliases.get(&lower).unwrap_or(&lower);
        if self.fonts.keys().any(|k| k.family == *effective) {
            Some(effective.clone())
        } else {
            self.fonts.keys().min_by(|a, b| a.family.cmp(&b.family)).map(|k| k.family.clone())
        }
    }

    /// The closest-to-regular cut of an ALREADY-ALIASED family, or `None` when
    /// no face in it is loaded. ONE spelling of the face-determinism rule: all
    /// three of `resolve`'s fallback, `resolve_exact` and `resolve_default_font`
    /// need it, and three copies of a determinism rule is precisely the shape
    /// that let the HashMap-order bug survive — the fix had to be applied three
    /// times and one of the three was at first left untested. Takes the
    /// EFFECTIVE (post-alias) name so a caller that already hopped does not hop
    /// twice.
    fn closest_cut(&self, effective: &str) -> Option<&LoadedFont> {
        self.fonts.iter()
            .filter(|(k, _)| k.family == *effective)
            .min_by_key(|(k, _)| k.cut_rank())
            .map(|(_, v)| v)
    }

    /// Last resort when the requested family is not loaded at all: the default
    /// family's best cut, else the alphabetically-first family's best cut.
    ///
    /// Both arms rank by [`FontKey::cut_rank`], so which FACE comes back is a pure
    /// function of the loaded set. Before this rank both broke their tie on the family
    /// alone and inherited `HashMap` order — with four Arimo cuts loaded, all four
    /// were reachable from one call, and they disagree on advance width.
    fn resolve_default_font(&self) -> Option<&LoadedFont> {
        let lower = DEFAULT_FONT_FAMILY.to_lowercase();
        let effective = self.family_aliases.get(&lower).unwrap_or(&lower);
        self.closest_cut(effective)
            .or_else(|| {
                self.fonts.iter()
                    .min_by(|(a, _), (b, _)| (&a.family, a.cut_rank()).cmp(&(&b.family, b.cut_rank())))
                    .map(|(_, v)| v)
            })
    }
}

#[cfg(test)]
mod face_selection_determinism {
    //! Face selection must be a pure function of the LOADED SET.
    //!
    //! These tests need no subprocesses, which is why they can live in the normal
    //! suite: each `HashMap` gets its OWN `RandomState` within a process, so
    //! building the cache N times here yields N iteration orders — the same
    //! variation the bug needed N processes to show. Without
    //! [`FontKey::cut_rank`] the first test reds on essentially every run
    //! (measured before the fix: 4 distinct faces from 40 caches, 15/4/12/9).
    use super::*;

    fn cache() -> FontCache {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
        let mut c = FontCache::new();
        for (file, weight, italic) in [
            ("Arimo-Regular.ttf", 400u16, false),
            ("Arimo-Bold.ttf", 700, false),
            ("Arimo-Italic.ttf", 400, true),
            ("Arimo-BoldItalic.ttf", 700, true),
        ] {
            let bytes = std::fs::read(format!("{dir}/{file}")).unwrap();
            c.insert("Arimo", bytes, weight, italic).unwrap();
        }
        c
    }

    /// A family with NO regular cut, which is what it takes to reach `resolve`'s
    /// own fallback: its third candidate key is `(400, italic)`, so any family
    /// carrying an upright and an italic regular — `cache()` above, and the
    /// shipping substitution set — answers every request before the fallback runs.
    /// The two cuts here are deliberately both off-400 and on opposite sides of
    /// it, so the winner is decided by `cut_rank`'s distance term.
    fn cache_without_a_regular_cut() -> FontCache {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
        let mut c = FontCache::new();
        for (file, weight, italic) in [("Arimo-Bold.ttf", 500u16, false), ("Arimo-BoldItalic.ttf", 900, true)] {
            let bytes = std::fs::read(format!("{dir}/{file}")).unwrap();
            c.insert("Oddcut", bytes, weight, italic).unwrap();
        }
        c
    }

    /// Every fallback that can pick a face must return the SAME face from
    /// independently-built caches holding identical contents.
    ///
    /// `resolve("Calibri", …)` is the live path: an item-less numbered paragraph
    /// seeds the raw `DEFAULT_FONT_FAMILY` (`wml/paragraph.rs`), so the marker
    /// asks for a family that is not loaded and lands in `resolve_default_font`.
    /// The other two arms are the same class, and are covered here so that a
    /// later edit cannot reintroduce the bug in the siblings of the site that
    /// happened to be the one a corpus sweep noticed.
    #[test]
    fn face_selection_is_independent_of_hashmap_order() {
        let ident = |f: &LoadedFont| (f.weight, f.italic);
        let mut resolved = std::collections::BTreeSet::new();
        let mut no_regular = std::collections::BTreeSet::new();
        let mut exact = std::collections::BTreeSet::new();
        let mut exact_fallback = std::collections::BTreeSet::new();
        for _ in 0..40 {
            let c = cache();
            // Not loaded, not aliased -> resolve_default_font's second arm.
            resolved.insert(ident(c.resolve("Calibri", 400, false).unwrap()));
            // "Arimo" HAS a regular cut, so the earlier spelling
            // (`get(regular).or_else(find)`) was already deterministic for it —
            // this arm alone was VACUOUS against the real prior code, catching
            // only a degenerate find()-only regression. The no-regular-cut
            // family is what actually exercises resolve_exact's fallback, which
            // is the arm that used to be HashMap-order dependent.
            exact.insert(ident(c.resolve_exact("Arimo").unwrap()));
            exact_fallback.insert(ident(
                cache_without_a_regular_cut().resolve_exact("Oddcut").unwrap(),
            ));
            // Family present but no candidate key matches -> resolve's fallback.
            no_regular.insert(ident(cache_without_a_regular_cut().resolve("Oddcut", 400, false).unwrap()));
        }
        assert_eq!(resolved.len(), 1, "resolve() picked {resolved:?} across 40 caches");
        assert_eq!(exact.len(), 1, "resolve_exact() picked {exact:?}");
        assert_eq!(
            exact_fallback.len(), 1,
            "resolve_exact()'s no-regular-cut fallback picked {exact_fallback:?}"
        );
        // Stable on the RIGHT cut: 500 is nearer 400 than 900 is.
        assert_eq!(*exact_fallback.iter().next().unwrap(), (500, false));
        assert_eq!(no_regular.len(), 1, "resolve() fallback picked {no_regular:?}");
        // Not merely stable — stable on the RIGHT cut. A rank that happened to
        // pin the bold-italic face would pass the assertions above.
        assert_eq!(*resolved.iter().next().unwrap(), (400, false));
        assert_eq!(*exact.iter().next().unwrap(), (400, false));
        assert_eq!(*no_regular.iter().next().unwrap(), (500, false), "nearest weight to regular");
    }

    /// Why the flip was a rendering bug and not a dump artefact: the cuts a tie
    /// could hand back disagree on advance width, and that width becomes an
    /// `Item`'s `width`/`content_width` and its per-cluster paint x.
    ///
    /// The ascent/descent half of the same assertion records WHY page geometry
    /// never drifted in the corpus — Arimo's four cuts share hhea metrics, so
    /// line height was accidentally immune. A substitute family whose cuts
    /// disagreed there would have moved pagination too.
    #[test]
    fn cuts_of_one_family_disagree_on_advance_but_not_on_height() {
        let c = cache();
        let faces = FaceCache::new();
        let cut = |w, i| c.fonts.get(&FontKey { family: "arimo".into(), weight: w, italic: i }).unwrap();
        let adv = |w, i| faces.shape("A.\u{a0}", cut(w, i), DEFAULT_FONT_PT).unwrap().total_advance;
        assert_ne!(adv(400, false), adv(700, false), "cuts must differ in width for this bug to bite");
        let m = |w, i| { let m = font_metrics(cut(w, i), DEFAULT_FONT_PT); (m.ascent, m.descent) };
        assert_eq!(m(400, false), m(700, true), "Arimo cuts share hhea metrics — see the docstring");
    }
}

// ─── Debug-probe introspection (font-loading diagnostics) ────────────────────

/// One loaded face's identity + hhea metrics (normalised per-em so the consumer
/// can compare faces at any size). Debug-only.
#[cfg(feature = "debug-probe")]
pub(crate) struct DbgFontFace {
    pub(crate) family: String,
    pub(crate) weight: u16,
    pub(crate) italic: bool,
    pub(crate) units_per_em: u16,
    pub(crate) ascent_em: f64,
    pub(crate) descent_em: f64,
    pub(crate) line_gap_em: f64,
}

/// A read-only snapshot of the resolved font table: declared→effective family
/// aliases and every loaded face. Debug-only.
#[cfg(feature = "debug-probe")]
pub(crate) struct DbgFontResolutions {
    /// (declared doc family, effective/loaded family) — both lowercased.
    pub(crate) aliases: Vec<(String, String)>,
    pub(crate) faces: Vec<DbgFontFace>,
}

#[cfg(feature = "debug-probe")]
impl FontCache {
    /// Snapshot the resolved font table for the font-resolution probe. The TS
    /// `debugFonts()` wrapper overlays each face's byte SOURCE (local / cached /
    /// CDN) — the engine only knows the loaded bytes, never where they came from.
    pub(crate) fn dbg_resolutions(&self) -> DbgFontResolutions {
        let mut aliases: Vec<(String, String)> = self
            .family_aliases
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        aliases.sort();
        let mut faces: Vec<DbgFontFace> = self
            .fonts
            .iter()
            .map(|(k, font)| {
                let face = ttf_parser::Face::parse(&font.data, font.index)
                    .expect("font was validated on insert");
                let upem = face.units_per_em() as f64;
                DbgFontFace {
                    family: k.family.clone(),
                    weight: k.weight,
                    italic: k.italic,
                    units_per_em: face.units_per_em(),
                    ascent_em: face.ascender() as f64 / upem,
                    descent_em: -(face.descender() as f64) / upem,
                    line_gap_em: face.line_gap() as f64 / upem,
                }
            })
            .collect();
        faces.sort_by(|a, b| {
            a.family
                .cmp(&b.family)
                .then(a.weight.cmp(&b.weight))
                .then(a.italic.cmp(&b.italic))
        });
        DbgFontResolutions { aliases, faces }
    }
}

// ─── Shaping types ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct ShapedGlyph {
    pub(crate) cluster: u32,
    pub(crate) x_advance: f64,
    pub(crate) x_offset: f64,
    pub(crate) y_offset: f64,
    /// The face-relative glyph index rustybuzz selected. RETAINED FOR THE ORACLE
    /// ONLY — nothing paints from it, because paint is `fill_text` on the
    /// run's *text*, not a gid run. It is here because glyph identity is the one
    /// axis no other pin can express: the GLYPH stream can then red on "same
    /// advance, wrong glyph", the failure `FaceCache` reuse could produce.
    ///
    /// Measured cost of carrying it: ZERO bytes — `{u32,f64,f64,f64}` was already
    /// padded to 32, and `glyph_gid_is_free_of_charge` asserts that so a future
    /// field cannot silently make this expensive.
    ///
    /// It earns its keep, measured. Forcing every `FontCache::resolve` to the
    /// italic cut reds `oracle-hdr-ftr-inherit`'s GLYPH golden on gid ALONE, with
    /// every advance byte-identical:
    ///     golden: 44/3.33 81/6.67 75/6.67 72/6.67 85/4.00 …
    ///     actual: 10/3.33 41/6.67 35/6.67 32/6.67 45/4.00 …
    /// The ITEM and LINE pins are green on that fixture. Arimo's cuts are
    /// metric-compatible, so nothing but glyph identity could have caught it.
    ///
    /// DEVIATION, declared: a gid is face-RELATIVE, so what it actually pins is
    /// the face's glyph ORDER. Two faces that agree on order — a subset and its
    /// source, a re-hinted rebuild — would swap invisibly. Closing THAT needs
    /// resolved-face identity on `ShapedRun`, not a wider glyph tuple.
    ///
    /// Read ONLY by the `#[cfg(test)]` GLYPH-stream dump, so a shipping build
    /// sees it as dead — the same `cfg_attr` shape `decode_char_refs` uses in
    /// this file. Kept on the shipping struct rather than behind `cfg(test)`
    /// because the pin must observe the value the SHIPPING shaper produced, not
    /// one a test-only field could diverge from.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) gid: u32,
}

#[derive(Clone)]
pub(crate) struct ShapedRun {
    pub(crate) glyphs: Vec<ShapedGlyph>,
    pub(crate) total_advance: f64,
    pub(crate) text: String,
    pub(crate) is_simple: bool,
    /// Actual glyph STORAGE order is RTL (visual, right-to-left).
    /// rustybuzz's `GlyphBuffer` exposes no `direction()` accessor, so this is
    /// derived by DESCENDING-cluster detection: an RTL run stores its glyphs
    /// right-to-left, so the first glyph's cluster is GREATER than the last's.
    /// A single-glyph (or single-cluster) run is `false` — direction is
    /// immaterial there and the x↔byte inverse treats it trivially. This gates
    /// the mirrored x↔byte variant (NOT `Item.rtl`, which is bidi-level parity
    /// and can disagree: a neutral-only item under an RTL base shapes LTR).
    pub(crate) rtl: bool,
}

impl ShapedRun {
    pub(crate) fn trailing_ws_advance(&self) -> f64 {
        let trailing_ws_bytes: usize = self.text
            .chars()
            .rev()
            .take_while(|c| c.is_whitespace())
            .map(|c| c.len_utf8())
            .sum();
        if trailing_ws_bytes == 0 {
            return 0.0;
        }
        let ws_start_byte = (self.text.len() - trailing_ws_bytes) as u32;
        self.glyphs.iter()
            .filter(|g| g.cluster >= ws_start_byte)
            .map(|g| g.x_advance)
            .sum()
    }
}

#[cfg(test)]
impl ShapedRun {
    /// Serialize this run's glyph vector for the oracle GLYPH stream.
    ///
    /// The ITEM stream already pins a run's glyph COUNT and its TOTAL advance
    /// (`sh{N}g adv{…}`); everything INSIDE the run was unpinned, which is not a
    /// theoretical hole — reversing the per-glyph advance vector while holding
    /// the total constant left the whole suite green except where a trailing
    /// space happened to make `content_width` notice.
    ///
    /// SPARSE BY CONSTRUCTION, so the common case costs four characters a glyph:
    ///  * `x_offset`/`y_offset` are emitted only when NOT exactly zero (measured:
    ///    1 glyph of 28,451 across the 23 fixtures);
    ///  * the cluster vector is emitted only when it differs from the derivable
    ///    default — one cluster per glyph at ascending char-byte offsets, i.e.
    ///    exactly what `shape_with_face` checks for `is_simple` (measured: 2 runs
    ///    of 5,049).
    ///
    /// Advances/offsets are rounded to 0.01px, matching every other dump in the
    /// harness. A drift below that is invisible here too, deliberately — the
    /// oracle's unit of truth is the hundredth of a pixel everywhere.
    ///
    /// The returned string is what the caller interns, so two runs that shaped
    /// identically share one vocabulary row. That is what makes this stream an
    /// order of magnitude smaller than the ITEM stream it rides alongside.
    ///
    /// The opening exhaustive destructure (NO rest-dots) is a COMPILE-TIME
    /// completeness guard, the pattern `CellStyle::hash_into` (sml/model.rs)
    /// established: adding a field to `ShapedRun` fails to compile HERE until a
    /// decision is recorded about which stream owns it.
    ///
    /// It is owed on this struct specifically because no single site reads all
    /// five fields — this one emits the glyph vector, and `Item::dump_into`
    /// emits the other three as `sh{N}g adv{…} r{} s{}`. A reader split in two
    /// with nothing forcing the halves to stay in step is exactly how
    /// `Item::math` reached `LayoutLine::push` but not `accumulate_item_metrics`
    /// and shipped a real mis-measure. The next field is already named:
    /// `ShapedGlyph::gid`'s declared deviation says closing face-order ambiguity
    /// "needs resolved-face identity on `ShapedRun`" — a field that must reach a
    /// pin, and would otherwise reach only the shaper.
    ///
    /// `total_advance`/`is_simple`/`rtl` are `_`-bound, not dropped: re-emitting
    /// them here would spell one rule twice AND would move 23 committed goldens
    /// for no new coverage.
    pub(crate) fn dump_body(&self) -> String {
        use std::fmt::Write;
        let ShapedRun {
            glyphs, text,
            // ── owned by the ITEM stream (`Item::dump_into`) — see the doc above ──
            total_advance: _, is_simple: _, rtl: _,
        } = self;
        let mut out = String::new();
        for (i, g) in glyphs.iter().enumerate() {
            // Completeness guard — NO REST-DOTS. All five fields reach
            // this stream (gid/advance always; offsets and cluster when they
            // carry information), so a new `ShapedGlyph` field cannot compile
            // until someone decides its emission here.
            let ShapedGlyph { cluster: _, x_advance, x_offset, y_offset, gid } = g;
            if i > 0 {
                out.push(' ');
            }
            let _ = write!(out, "{gid}/{x_advance:.2}");
            if *x_offset != 0.0 || *y_offset != 0.0 {
                let _ = write!(out, "@{x_offset:.2},{y_offset:.2}");
            }
        }
        let default_clusters = glyphs.len() == text.chars().count()
            && glyphs.iter().zip(text.char_indices())
                .all(|(g, (b, _))| g.cluster == b as u32);
        if !default_clusters {
            out.push_str(" c=");
            for (i, g) in glyphs.iter().enumerate() {
                let _ = write!(out, "{}{}", if i > 0 { "," } else { "" }, g.cluster);
            }
        }
        out
    }
}

pub(crate) struct FontMetrics {
    pub(crate) ascent: f64,
    pub(crate) descent: f64,
    /// hhea lineGap (external leading), scaled to px. Word includes this in the
    /// single line height (Arial=67/2048em, Times=87/2048em); gap=0 for
    /// Calibri/DejaVu/Verdana so they are unaffected.
    pub(crate) line_gap: f64,
}

/// Face-construction measurement probe. Counts [`build_face`] calls — i.e. how many times a
/// whole `rustybuzz::Face` (the OpenType table directory) was parsed — and
/// accumulates the wall time spent inside `Face::from_slice`, so the "N% of
/// layout time" question is answered by measurement rather than by assertion.
/// With face reuse the call count is also the direct read of [`FaceCache`]'s hit rate:
/// misses are all that remain.
///
/// `#[cfg(test)]` — nothing here exists in the wasm binary. Compiled in and
/// DISABLED (the default, so the ordinary suite is unaffected) it costs one
/// relaxed atomic load per shaped run. The counters are process-global, so the
/// harness that reads them must run single-threaded (`--test-threads=1`); see
/// `perf_tests.rs`.
#[cfg(test)]
pub(crate) mod face_probe {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    use std::time::Instant;

    static ON: AtomicBool = AtomicBool::new(false);
    static CALLS: AtomicU64 = AtomicU64::new(0);
    static NANOS: AtomicU64 = AtomicU64::new(0);

    /// Arm/disarm the probe and zero the counters.
    pub(crate) fn arm(on: bool) {
        CALLS.store(0, Relaxed);
        NANOS.store(0, Relaxed);
        ON.store(on, Relaxed);
    }
    /// (`Face::from_slice` calls, nanoseconds inside them) since the last `arm`.
    pub(crate) fn read() -> (u64, u64) {
        (CALLS.load(Relaxed), NANOS.load(Relaxed))
    }

    /// RAII-free deliberately: `stop` consumes the timer at the exact point the
    /// face is constructed, so nothing else on the shaping path is attributed.
    pub(crate) struct Timer(Option<Instant>);
    impl Timer {
        pub(crate) fn start() -> Self {
            Self(ON.load(Relaxed).then(Instant::now))
        }
        pub(crate) fn stop(self) {
            if let Some(t0) = self.0 {
                NANOS.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
                CALLS.fetch_add(1, Relaxed);
            }
        }
    }
}

/// The one place a `rustybuzz::Face` is built. Parsing the OpenType table
/// directory costs ~4.1 µs on a 478 kB face (measured — `PERF_FACE_CTOR` in
/// `perf_tests`), which was 38–51 % of layout wall time back when it happened
/// once per shaped run. Everything that shapes now goes through [`FaceCache`],
/// so this runs once per (font, scope).
fn build_face(font: &LoadedFont) -> rustybuzz::Face<'_> {
    #[cfg(test)]
    let probe = face_probe::Timer::start();
    let face = rustybuzz::Face::from_slice(&font.data, font.index)
        .expect("font was validated on insert");
    #[cfg(test)]
    probe.stop();
    face
}

/// Scoped reuse of built faces, keyed by the identity of the `LoadedFont` they
/// were built from.
///
/// `Face<'f>` borrows `LoadedFont::data`, so it cannot be stored beside the font
/// it came from: that would be a self-referential struct, needing either a new
/// dependency (`self_cell`/`ouroboros`) or a lifetime `transmute`. Both are
/// barred here. What a face CAN outlive is one shaping call — up to the end
/// of the `&'f FontCache` borrow it was built from. This holds faces for exactly
/// that long.
///
/// ⚠️ The key is the `LoadedFont`'s address, and that is sound **only because
/// `'f` is a live shared borrow of the cache that owns it**: no `LoadedFont` can
/// be dropped, moved or replaced while this cache exists, so no address can be
/// reused by a different font. (Taking a reference's address is safe; only
/// dereferencing a raw pointer would not be.) A face cache that outlives its
/// borrow — a `thread_local`, a field on `Viewer` — loses that guarantee, and
/// the failure mode is a silently wrong font in the output, not a crash. Keep
/// this scoped.
///
/// Reuse is byte-identical by construction: `rustybuzz::shape` takes `&Face` and
/// a face carries no shaping state, so the Nth shape against a face is the same
/// as the first.
pub(crate) struct FaceCache<'f> {
    faces: std::cell::RefCell<HashMap<usize, rustybuzz::Face<'f>>>,
}

impl<'f> FaceCache<'f> {
    pub(crate) fn new() -> Self {
        Self { faces: std::cell::RefCell::new(HashMap::new()) }
    }

    /// Shape `text` with `font`, reusing this scope's face for it if one was
    /// already built. Replaces an earlier free `shape_text` function.
    pub(crate) fn shape(&self, text: &str, font: &'f LoadedFont, font_pt: f64) -> Option<ShapedRun> {
        let mut faces = self.faces.borrow_mut();
        let face = faces
            .entry(font as *const LoadedFont as usize)
            .or_insert_with(|| build_face(font));
        shape_with_face(face, text, font_pt)
    }
}

fn shape_with_face(
    face: &rustybuzz::Face<'_>,
    text: &str,
    font_pt: f64,
) -> Option<ShapedRun> {
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);

    let glyph_buffer = rustybuzz::shape(face, &[], buffer);
    let infos = glyph_buffer.glyph_infos();
    let positions = glyph_buffer.glyph_positions();

    if infos.iter().any(|info| info.glyph_id == 0) {
        return None;
    }

    let upem = face.units_per_em() as f64;
    let scale = pt_to_px(font_pt) / upem;

    let mut glyphs = Vec::with_capacity(infos.len());
    let mut total_advance = 0.0;
    let mut is_simple = infos.len() == text.chars().count();

    let char_byte_offsets: Vec<u32> = if is_simple {
        text.char_indices().map(|(i, _)| i as u32).collect()
    } else {
        Vec::new()
    };

    for (i, (info, pos)) in infos.iter().zip(positions.iter()).enumerate() {
        let x_advance = pos.x_advance as f64 * scale;
        let x_offset = pos.x_offset as f64 * scale;
        let y_offset = pos.y_offset as f64 * scale;

        if is_simple && (info.cluster != char_byte_offsets[i]
            || x_offset.abs() > 0.001
            || y_offset.abs() > 0.001)
        {
            is_simple = false;
        }

        glyphs.push(ShapedGlyph {
            cluster: info.cluster,
            x_advance,
            x_offset,
            y_offset,
            gid: info.glyph_id,
        });
        total_advance += x_advance;
    }

    // Descending-cluster detection of RTL glyph storage order (see field).
    // v1 LIMITATION: a single-glyph run (`glyphs.len() < 2`) cannot express an
    // order and reports `rtl = false`. The selection forward map (select.rs
    // `byte_to_x_local`) therefore treats a lone RTL glyph's caret side as LTR —
    // an off-by-one sub-glyph caret only, never a wrong-region highlight (the
    // cluster is atomic). Reconciled by the RTL/float-bidi selection follow-on.
    let rtl = glyphs.len() >= 2
        && glyphs.first().is_some_and(|f| f.cluster > glyphs.last().unwrap().cluster);

    Some(ShapedRun {
        glyphs,
        total_advance,
        text: text.to_string(),
        is_simple,
        rtl,
    })
}

pub(crate) fn font_metrics(font: &LoadedFont, font_pt: f64) -> FontMetrics {
    // Metrics were cached (raw font units) at insert time — see LoadedFont. This
    // is byte-identical to the previous per-call `ttf_parser::Face::parse`: same
    // `as f64` casts, same `* scale`, only the whole-face reparse per item per
    // relayout is gone.
    let upem = font.units_per_em as f64;
    let scale = pt_to_px(font_pt) / upem;
    FontMetrics {
        ascent: font.ascender as f64 * scale,
        descent: -(font.descender as f64 * scale),
        line_gap: font.line_gap as f64 * scale,
    }
}

/// One of the four OOXML font slots (§17.3.2.26 rFonts). Each character in a
/// run is mapped to a slot whose font shapes and renders it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FontSlot { Ascii, HighAnsi, EastAsia, ComplexScript }

/// Step 1 of the §17.3.2.26 two-step methodology: classify a character by its
/// Unicode code point, BEFORE the cs/rtl override (step 2 — see
/// `resolve_font_slot`). Complex-script blocks (Arabic/Hebrew/…) classify as
/// ASCII per the spec table; step 2 reroutes them to ComplexScript when the run
/// is marked. Deliberate v1 simplifications (each per §17.3.2.26): the
/// `hint="eastAsia"` promotion of the U+2000–27BF symbol ranges and the
/// zh/Big5/GB2312 lang exceptions take the non-hint branch; High ANSI is folded
/// into the default (ASCII) family since we don't carry a separate hAnsi face.
pub(crate) fn font_slot_class(c: char) -> FontSlot {
    match c as u32 {
        0x0000..=0x007F => FontSlot::Ascii, // Basic Latin
        // East Asian blocks (CJK punctuation/Hiragana/Katakana/ideographs,
        // Hangul, half/full-width forms, CJK compatibility, SIP ideographs).
        0x1100..=0x11FF | 0x2E80..=0x2EFF | 0x3000..=0x9FFF | 0xA000..=0xA4CF
        | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFFEF
        | 0x20000..=0x2FA1F => FontSlot::EastAsia,
        // Complex-script blocks (Hebrew/Arabic/Syriac/Thaana/Arabic ext +
        // Indic; presentation forms). Step-1 classification is ASCII per the
        // spec; we tag them ComplexScript so step 2 can route to the cs slot
        // when the run carries <w:cs/> or <w:rtl/>, else fall back to ASCII.
        0x0590..=0x05FF | 0x0600..=0x06FF | 0x0700..=0x074F | 0x0750..=0x077F
        | 0x0780..=0x07BF | 0x08A0..=0x08FF | 0x0900..=0x0DFF
        | 0xFB1D..=0xFB4F | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF => FontSlot::ComplexScript,
        _ => FontSlot::HighAnsi, // Latin-1, Latin Ext, Greek, Cyrillic, symbols…
    }
}

/// Step 2 of §17.3.2.26: resolve the final slot given whether the run carries
/// complex-script formatting (`<w:cs/>` or `<w:rtl/>`). An East-Asian character
/// in an rtl run becomes Complex Script; a step-1 complex-script character uses
/// the cs slot only when the run is complex (else ASCII, per the spec table).
pub(crate) fn resolve_font_slot(class: FontSlot, run_is_complex: bool) -> FontSlot {
    match class {
        FontSlot::ComplexScript if !run_is_complex => FontSlot::Ascii,
        FontSlot::EastAsia if run_is_complex => FontSlot::ComplexScript,
        other => other,
    }
}

/// Fallible half of `resolve_font_with_fallbacks` (mechanical split — the two
/// together must stay byte-identical to the pre-split fn for every existing
/// caller): try each ';'-separated name through the substitution chain and the
/// cache; `None` when NO candidate is loaded. Used directly by the §17.9.24
/// numbering-marker rPr overlay, which on a total miss KEEPS the run-derived
/// family instead of falling to the document default (Symbol/Wingdings et al.
/// are never shipped and marker-only faces are never fetched, so the remapped
/// • must keep today's face and advance).
pub(crate) fn try_resolve_font(doc_family: &str, font_cache: &FontCache) -> Option<String> {
    let parts: Vec<&str> = doc_family.split(';').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    for part in &parts {
        let resolved = font_cache.resolve_family_name(part).unwrap_or_else(|| part.to_string());
        if font_cache.has_family(&resolved) {
            return Some(resolved);
        }
    }
    None
}

/// Resolve a font family name that may carry a semicolon-separated fallback
/// list ("Open Sans;Arial"). Tries each name in order against the font cache,
/// returns the first that's loaded, or the first name if none are loaded.
pub(crate) fn resolve_font_with_fallbacks(doc_family: &str, font_cache: &FontCache) -> String {
    try_resolve_font(doc_family, font_cache)
        .unwrap_or_else(|| font_cache.default_family_name().unwrap_or_else(|| DEFAULT_FONT_FAMILY.to_owned()))
}

/// Debug-probe: the FINAL-STATE resolution story for one REQUESTED family (a
/// name the document actually uses). Answers "what would a run declaring this
/// family paint with RIGHT NOW" — the same `resolve_font_with_fallbacks` the
/// layout bakes into `RunStyle.family` (wml/paragraph.rs), re-asked live, so a
/// post-load font-cache change (e.g. a local-font re-fetch) is visible without
/// trusting any snapshot taken at layout time.
#[cfg(feature = "debug-probe")]
pub(crate) struct DbgFamilyReport {
    /// The family name as the document declares it (verbatim).
    pub(crate) requested: String,
    /// The `family_aliases` hop for the requested name (lowercased target), or
    /// `None` when no alias is registered.
    pub(crate) alias: Option<String>,
    /// What `resolve_font_with_fallbacks` returns right now — the exact string
    /// layout bakes into `RunStyle.family`: the declared candidate VERBATIM on a
    /// direct hit, the (lowercased) alias target on an alias hop, the
    /// (lowercased) default family on a total miss. Case-preserving on purpose:
    /// the probe reports what paint uses, not a normalization of it.
    pub(crate) resolves_now: String,
    /// True iff the resolution LANDED ON THE DOCUMENT DEFAULT because no
    /// declared candidate has a loaded face (resolvesNow == the default family
    /// AND the requested name resolves to nothing loaded). False when the
    /// requested family merely IS the default — that's a direct hit, not a
    /// fallback.
    pub(crate) fell_back_to_default: bool,
}

#[cfg(feature = "debug-probe")]
pub(crate) fn dbg_family_report(requested: &str, font_cache: &FontCache) -> DbgFamilyReport {
    let resolves_now = resolve_font_with_fallbacks(requested, font_cache);
    // The same default-name expression resolve_font_with_fallbacks falls to, so
    // the comparison can never drift from the real fallback target.
    let default_name = font_cache
        .default_family_name()
        .unwrap_or_else(|| DEFAULT_FONT_FAMILY.to_owned());
    let fell_back_to_default =
        try_resolve_font(requested, font_cache).is_none() && resolves_now == default_name;
    DbgFamilyReport {
        requested: requested.to_owned(),
        alias: font_cache.resolve_family_name(requested),
        resolves_now,
        fell_back_to_default,
    }
}

/// Native probe tests (run with `--features debug-probe`; the feature is
/// additive-only so the default suite is unaffected).
/// RED-CONDITION (proven before implementing): `dbg_family_report` did not
/// exist → this module failed to compile under `--features sml,pml,debug-probe`.
#[cfg(all(test, feature = "debug-probe"))]
mod dbg_family_report_tests {
    use super::{dbg_family_report, FontCache};

    fn arimo_bytes() -> Vec<u8> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
        std::fs::read(format!("{dir}/Arimo-Regular.ttf")).unwrap_or_else(|_| {
            panic!("test font missing: Arimo-Regular.ttf\n  run: ./scripts/fetch-test-fonts.sh")
        })
    }

    #[test]
    fn loaded_family_reports_alias_hop_and_no_fallback() {
        // The #loadFontsFor shape: bytes land under the SUBSTITUTE name and the
        // declared name aliases to it.
        let mut fc = FontCache::new();
        fc.insert("Verdana", arimo_bytes(), 400, false).unwrap();
        fc.add_alias("DejaVu Sans", "Verdana");

        let r = dbg_family_report("DejaVu Sans", &fc);
        assert_eq!(r.requested, "DejaVu Sans");
        assert_eq!(r.alias.as_deref(), Some("verdana"));
        assert_eq!(r.resolves_now, "verdana");
        assert!(!r.fell_back_to_default, "aliased+loaded family did not fall back");
    }

    #[test]
    fn missing_family_falls_back_to_default() {
        // Only the document default (Calibri) is loaded; an unknown family must
        // report the default AND flag the fallback.
        let mut fc = FontCache::new();
        fc.insert("Calibri", arimo_bytes(), 400, false).unwrap();

        let r = dbg_family_report("Ghost Grotesk", &fc);
        assert_eq!(r.alias, None);
        assert_eq!(r.resolves_now, "calibri");
        assert!(r.fell_back_to_default, "unloaded family must flag the default fallback");
    }

    #[test]
    fn requested_default_itself_is_not_a_fallback() {
        // resolvesNow == default alone must NOT flag: the requested family HAS a
        // loaded face — it IS the default (the AND in the definition).
        let mut fc = FontCache::new();
        fc.insert("Calibri", arimo_bytes(), 400, false).unwrap();

        let r = dbg_family_report("Calibri", &fc);
        // A direct (alias-less) hit keeps the DECLARED case — try_resolve_font
        // returns the candidate verbatim; only alias/default targets lowercase.
        // This is exactly what layout bakes into RunStyle.family.
        assert_eq!(r.resolves_now, "Calibri");
        assert!(!r.fell_back_to_default, "the default family itself is not a fallback");
    }

    #[test]
    fn semicolon_fallback_list_resolves_second_candidate_without_default_flag() {
        // A fallback list "Open Sans;Arial": Arial loaded → resolves there,
        // even when a distinct default exists.
        let mut fc = FontCache::new();
        fc.insert("Calibri", arimo_bytes(), 400, false).unwrap();
        fc.insert("Arial", arimo_bytes(), 400, false).unwrap();

        let r = dbg_family_report("Open Sans;Arial", &fc);
        assert_eq!(r.resolves_now, "Arial"); // direct hit → verbatim (see above)
        assert!(!r.fell_back_to_default, "declared-list resolution is not a default fallback");
    }
}

#[cfg(test)]
mod font_slot_tests {
    use super::{font_slot_class, resolve_font_slot, FontSlot};
    #[test]
    fn step1_classifies_per_spec_ranges() {
        assert_eq!(font_slot_class('A'), FontSlot::Ascii);        // U+0041 Basic Latin
        assert_eq!(font_slot_class('中'), FontSlot::EastAsia);    // U+4E2D CJK ideograph
        assert_eq!(font_slot_class('あ'), FontSlot::EastAsia);    // U+3042 Hiragana
        assert_eq!(font_slot_class('한'), FontSlot::EastAsia);    // U+D55C Hangul syllable
        assert_eq!(font_slot_class('ｱ'), FontSlot::EastAsia);     // U+FF71 halfwidth Katakana
        assert_eq!(font_slot_class('م'), FontSlot::ComplexScript);// U+0645 Arabic
        assert_eq!(font_slot_class('א'), FontSlot::ComplexScript);// U+05D0 Hebrew
        assert_eq!(font_slot_class('é'), FontSlot::HighAnsi);     // U+00E9 Latin-1 Supplement
    }
    #[test]
    fn step2_applies_cs_rtl_override() {
        // Arabic: cs slot only when the run is complex, else ASCII (spec table).
        assert_eq!(resolve_font_slot(FontSlot::ComplexScript, false), FontSlot::Ascii);
        assert_eq!(resolve_font_slot(FontSlot::ComplexScript, true), FontSlot::ComplexScript);
        // East-Asian char in an rtl run → Complex Script (§17.3.2.26 step 2).
        assert_eq!(resolve_font_slot(FontSlot::EastAsia, true), FontSlot::ComplexScript);
        assert_eq!(resolve_font_slot(FontSlot::EastAsia, false), FontSlot::EastAsia);
    }
}

#[cfg(test)]
mod shaped_glyph_tests {
    use super::ShapedGlyph;

    /// `ShapedGlyph::gid` is retained for the oracle and read by nothing
    /// else, so the only cost that could justify dropping it is memory. There is
    /// none: the `u32` lands in the tail padding `{u32,f64,f64,f64}` already had.
    /// Pinned rather than asserted in prose, so that a future field which DOES
    /// grow the struct has to come past this test and re-argue the trade.
    #[test]
    fn glyph_gid_is_free_of_charge() {
        assert_eq!(std::mem::size_of::<ShapedGlyph>(), 32);
        assert_eq!(std::mem::size_of::<(u32, f64, f64, f64)>(), 32);
    }
}
