//! OMML (Office Math Markup Language) — box-and-glue layout.
//!
//! A `<m:oMath>` / `<m:oMathPara>` subtree is laid out into ONE flat inline
//! atom: a [`MathBox`] carrying its own width/ascent/descent plus a flat
//! `Vec<MathNode>` of absolutely-positioned glyph and rule primitives (offsets
//! measured from the box origin = its left edge on the text baseline). Nested
//! constructs are flattened at build time — a child box's nodes are re-emitted
//! into the parent with `(dx, dy)` added — so the renderer is a simple two-node
//! walk and hit-test treats the whole equation as a single unbreakable Item.
//!
//! The layout covers ALL 20 OMML constructs with real 2D geometry:
//! styled text runs (with the OMML italic-variable convention), super/sub/
//! sub-super scripts, fractions, n-ary operators (∑/∫ with stacked or
//! script-placed limits), stretchy delimiters, radicals (sign + vinculum +
//! degree), matrices (a real grid), equation arrays
//! (a stacked column, so a delimiter around one grows its brackets to the full
//! stack = the binomial), functions (upright name + thin space + argument),
//! accents (overlay glyph centred above the base, no advance), bars
//! (over/under-line rule), group characters (a width-stretched brace above/
//! below), lower/upper limits (a shared over/under stacker), pre-scripts (a
//! script column hugging the LEFT of the base), boxes (pass-through), border
//! boxes (per-edge rules via the horizontal [`MathNode::Rule`] + vertical
//! [`MathNode::VRule`]) and phantoms (dimensions reserved, nothing painted).
//! [`layout_element`] matches `MathElement` exhaustively with a real 2D arm for
//! every variant; [`linearize`] survives only as the ultimate empty-subtree
//! degrade (a construct that produced zero primitives shows its plain text) —
//! nothing ever vanishes and nothing panics.
//!
//! Parser follow-ons (deferred to the fork): `barPr/m:pos`, `groupChrPr` pos/
//! vertJc, `borderBoxPr` per-edge hide + strikes, and `phantPr`
//! zeroWid/zeroAsc/zeroDesc are not yet parsed, so bar is overline-only, brace
//! position is glyph-class-driven, border boxes draw all four edges, and
//! phantoms are always a full invisible spacer. The layout already HONORS those
//! fields when present, so honoring them is a pure parser extension.
//!
//! Font handling: with the math face (`XITS Math`, fetched on demand) loaded,
//! every math glyph routes through it, the OpenType MATH constants table drives
//! gaps, shifts and rule thicknesses, letters map onto the Unicode Mathematical
//! Alphanumeric planes, and stretchy glyphs snap to MATH size variants painted
//! from their outlines. Without a math face, math renders in the run/body font
//! and stretchy glyphs are PROXIED by enlarging the point size (large operator
//! ≈ 1.8×), which distorts width with height. Multi-part glyph assembly
//! (top/ext/bottom pieces) is not implemented. Caladea is metric-compatible with
//! Cambria but is NOT Cambria Math and must not be substituted for the math face.
//!
//! Display math centering: a paragraph whose only significant content is a bare
//! `<m:oMath>` (or an `<m:oMathPara>`) is a DISPLAY equation — Word centers it
//! via the built-in `centerGroup` default math justification even with no
//! settings.xml mathPr. [`is_display_math_paragraph`] detects that case and
//! `ParagraphLayout::resolve` centers the line (honoring an explicit `w:jc`);
//! the centering rides the existing `STJc::Center` `x_start` branch because the
//! display atom owns its line. Inline `<m:oMath>` seated among real runs stays
//! inline (left).

use ooxml_omml::{
    Accent, Bar, BorderBox, Delimiter, EquationArray, Fraction, Function, GroupChar, Limit,
    LimitLocation, MathBox as MathBoxElem, MathElement, MathRun, MathScript, MathStyle, MathZone,
    Matrix, Nary, Phantom, PreScript, Radical, Script, SubSuperscript, VerticalPosition,
};
use ooxml_wml::ext::{ParagraphExt, RunExt};
use ttf_parser::GlyphId;
use wasm_bindgen::JsValue;

use crate::engine::fonts::{font_metrics, FaceCache, FontCache, LoadedFont, ShapedRun, MATH_FAMILY};
use crate::engine::paint::{Painter, RunStyle};
use crate::engine::units::pt_to_px;

// ─── Real OpenType math font + MATH table ─────────────────────────────────────

/// A parsed handle onto the loaded math face (`XITS Math`). Holds only the raw
/// bytes + index + upem (no borrowed `ttf_parser::Face`, so it is trivially
/// `Copy`-able through the layout tree); every `MATH`-table read re-parses the
/// face on demand (cheap: a table-directory scan, done a handful of times per
/// equation).
#[derive(Clone, Copy)]
pub(crate) struct MathFace<'a> {
    data: &'a [u8],
    index: u32,
    upem: f64,
}

impl<'a> MathFace<'a> {
    fn from_loaded(font: &'a LoadedFont) -> Self {
        MathFace { data: font.data(), index: font.index(), upem: font.units_per_em() as f64 }
    }

    fn face(&self) -> Option<ttf_parser::Face<'a>> {
        ttf_parser::Face::parse(self.data, self.index).ok()
    }

    /// Does this face carry a usable `MATH` constants table?
    fn has_constants(&self) -> bool {
        self.face().and_then(|f| f.tables().math.and_then(|m| m.constants)).is_some()
    }

    /// Is `ch` present in the face's cmap? Gates the alphabet map's tofu guard.
    fn has_glyph(&self, ch: char) -> bool {
        self.face().and_then(|f| f.glyph_index(ch)).is_some()
    }

    /// Select the smallest MATH size-variant of `ch` whose advance ≥ `target_px`
    /// (else the largest available), in the given axis. Returns the variant glyph
    /// id and its TRUE advance in px (the box extent — no aspect distortion). The
    /// multi-part assembly for glyphs taller than the largest variant is a
    /// documented follow-on (not implemented); the largest variant is the floor.
    fn size_variant(
        &self,
        ch: char,
        target_px: f64,
        vertical: bool,
        font_pt: f64,
    ) -> Option<(GlyphId, f64)> {
        let face = self.face()?;
        let base = face.glyph_index(ch)?;
        let variants = face.tables().math?.variants?;
        let constructions =
            if vertical { variants.vertical_constructions } else { variants.horizontal_constructions };
        let cons = constructions.get(base)?;
        let scale = pt_to_px(font_pt) / self.upem;
        let mut best: Option<(GlyphId, f64)> = None;
        let mut largest: Option<(GlyphId, f64)> = None;
        for v in cons.variants {
            let adv = v.advance_measurement as f64 * scale;
            // Track the largest as the fallback floor.
            if largest.map(|(_, a)| adv > a).unwrap_or(true) {
                largest = Some((v.variant_glyph, adv));
            }
            // Smallest variant that reaches the target.
            if adv + 0.01 >= target_px && best.map(|(_, a)| adv < a).unwrap_or(true) {
                best = Some((v.variant_glyph, adv));
            }
        }
        best.or(largest)
    }

    /// Horizontal advance of a glyph id, in px.
    fn advance_px(&self, gid: GlyphId, font_pt: f64) -> f64 {
        self.face()
            .and_then(|f| f.glyph_hor_advance(gid))
            .map(|a| a as f64 * pt_to_px(font_pt) / self.upem)
            .unwrap_or(0.0)
    }

    /// Outline `gid` into device-px path segments relative to the glyph pen
    /// origin on the baseline (font y-up flipped to screen y-down). Returns the
    /// segments plus the vertical extents (ascent above baseline, descent below).
    fn glyph_outline_px(&self, gid: GlyphId, font_pt: f64) -> Option<(Vec<PathSeg>, f64, f64)> {
        let face = self.face()?;
        let scale = pt_to_px(font_pt) / self.upem;
        let mut b = OutlineToSegs { segs: Vec::new(), scale, min_y: 0.0, max_y: 0.0 };
        face.outline_glyph(gid, &mut b)?;
        if b.segs.is_empty() {
            return None;
        }
        let ascent = (-b.min_y).max(0.0);
        let descent = b.max_y.max(0.0);
        Some((b.segs, ascent, descent))
    }
}

/// One command of a flattened glyph outline (device px, box-relative). Replayed
/// at paint time through the EXISTING Painter path vocabulary (begin_path/
/// move_to/line_to/quadratic_curve_to/bezier_curve_to/fill) — so it rides the
/// DevicePainter offset wrapper, the NullPainter no-op, and the worker/replay
/// display-list with ZERO new opcodes and no Path2D FFI primitive. This is how a
/// MATH size-variant glyph (which has no cmap entry, so `fill_text` cannot reach
/// it) is painted.
#[derive(Clone)]
pub(crate) enum PathSeg {
    Move(f64, f64),
    Line(f64, f64),
    Quad(f64, f64, f64, f64),
    Cubic(f64, f64, f64, f64, f64, f64),
    Close,
}

/// `ttf_parser::OutlineBuilder` that records scaled + Y-flipped path segments and
/// tracks the vertical bounding box (in screen coords, so `min_y` is the top).
struct OutlineToSegs {
    segs: Vec<PathSeg>,
    scale: f64,
    min_y: f64,
    max_y: f64,
}

impl OutlineToSegs {
    #[inline]
    fn track(&mut self, y: f64) {
        if y < self.min_y {
            self.min_y = y;
        }
        if y > self.max_y {
            self.max_y = y;
        }
    }
}

impl ttf_parser::OutlineBuilder for OutlineToSegs {
    fn move_to(&mut self, x: f32, y: f32) {
        let (px, py) = (x as f64 * self.scale, -(y as f64) * self.scale);
        self.track(py);
        self.segs.push(PathSeg::Move(px, py));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (px, py) = (x as f64 * self.scale, -(y as f64) * self.scale);
        self.track(py);
        self.segs.push(PathSeg::Line(px, py));
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (cx, cy) = (x1 as f64 * self.scale, -(y1 as f64) * self.scale);
        let (px, py) = (x as f64 * self.scale, -(y as f64) * self.scale);
        self.track(cy);
        self.track(py);
        self.segs.push(PathSeg::Quad(cx, cy, px, py));
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let s = self.scale;
        let p = (
            x1 as f64 * s, -(y1 as f64) * s,
            x2 as f64 * s, -(y2 as f64) * s,
            x as f64 * s, -(y as f64) * s,
        );
        self.track(p.1);
        self.track(p.3);
        self.track(p.5);
        self.segs.push(PathSeg::Cubic(p.0, p.1, p.2, p.3, p.4, p.5));
    }
    fn close(&mut self) {
        self.segs.push(PathSeg::Close);
    }
}

/// The threaded math layout context: the font cache, the resolved math family
/// (Some iff the math face is loaded), the pre-read MATH constants (real values
/// with the math face, em-fraction proxies without), and the math face handle
/// for variant / outline / cmap reads. Built once per zone.
pub(crate) struct MathContext<'a> {
    fc: &'a FontCache,
    /// The zone's shaping scope. Math measures glyph-at-a-time, so
    /// without this every single math glyph re-parsed a whole OpenType face.
    faces: FaceCache<'a>,
    /// The family every math glyph shapes+paints through, or `None` to keep the
    /// run/body font (the proxy degrade when no math face is loaded).
    family: Option<String>,
    consts: MathConstantsPx,
    face: Option<MathFace<'a>>,
}

impl<'a> MathContext<'a> {
    fn new(fc: &'a FontCache) -> Self {
        match fc.math_face() {
            Some(font) => {
                let face = MathFace::from_loaded(font);
                if face.has_constants() {
                    MathContext {
                        fc,
                        faces: FaceCache::new(),
                        family: Some(MATH_FAMILY.to_string()),
                        consts: MathConstantsPx::from_face(&face),
                        face: Some(face),
                    }
                } else {
                    // Loaded but MATH-table-less (a subset that stripped it) —
                    // degrade to proxies so nothing blanks. Logged once at load.
                    MathContext { fc, faces: FaceCache::new(), family: None, consts: MathConstantsPx::proxy(), face: None }
                }
            }
            None => MathContext { fc, faces: FaceCache::new(), family: None, consts: MathConstantsPx::proxy(), face: None },
        }
    }

    /// Route `style` onto the math family when one is loaded (else leave the
    /// body font). Math glyphs shape + paint through this.
    fn route(&self, style: &RunStyle) -> RunStyle {
        let mut s = style.clone();
        if let Some(fam) = &self.family {
            s.family = fam.clone();
        }
        s
    }

    /// A vertically-stretched glyph (delimiter, radical sign, n-ary operator)
    /// sized to `target_px`. With a math face, snaps to the smallest MATH size
    /// variant ≥ target and paints its real outline (true metrics, no aspect
    /// distortion). Without one, degrades to the point-scale proxy
    /// ([`grown_glyph`]) — byte-identical to before.
    fn vstretch(&self, ch: &str, target_px: f64, style: &RunStyle) -> MathBox {
        self.stretch(ch, target_px, true, style)
    }

    /// The horizontal companion (group-char brace stretched to a width).
    fn hstretch(&self, ch: &str, target_px: f64, style: &RunStyle) -> MathBox {
        self.stretch(ch, target_px, false, style)
    }

    fn stretch(&self, ch: &str, target_px: f64, vertical: bool, style: &RunStyle) -> MathBox {
        let routed = self.route(style);
        if let Some(face) = &self.face {
            if let Some(c) = ch.chars().next() {
                if let Some((gid, adv_px)) = face.size_variant(c, target_px, vertical, routed.font_pt)
                {
                    if let Some((segs, out_asc, out_desc)) =
                        face.glyph_outline_px(gid, routed.font_pt)
                    {
                        // Box extents: horizontal advance for width; the outline
                        // bbox (vertical) or the variant advance (horizontal) for
                        // the growth axis.
                        let (width, ascent, descent) = if vertical {
                            (face.advance_px(gid, routed.font_pt), out_asc, out_desc)
                        } else {
                            // Horizontal brace: advance is the WIDTH; keep the
                            // outline's own vertical extents.
                            (adv_px, out_asc, out_desc)
                        };
                        // Synthetic "grown point size" so the geometry snapshot /
                        // per-construct sanity tests still see an enlarged glyph —
                        // the growth ratio along the STRETCH axis (height for a
                        // vertical variant, width for a horizontal brace).
                        let (_, base_w, base_a, base_d) = measure(ch, &routed, self);
                        let ratio = if vertical {
                            (ascent + descent) / (base_a + base_d).max(1.0)
                        } else {
                            width / base_w.max(1.0)
                        };
                        let pt_equiv = routed.font_pt * ratio.max(1.0);
                        return MathBox {
                            width,
                            ascent,
                            descent,
                            nodes: vec![MathNode::GlyphOutline {
                                dx: 0.0,
                                dy: 0.0,
                                ch: ch.to_string(),
                                pt_equiv,
                                color: routed.color.clone(),
                                segs,
                            }],
                            is_display: false,
                        };
                    }
                }
            }
        }
        // Fallback: the point-scale proxy.
        if vertical {
            grown_glyph(ch, target_px, style, self)
        } else {
            grown_glyph_w(ch, target_px, style, self)
        }
    }
}

/// The MATH `Constants` table, pre-scaled to em-fractions (lengths) and ratios
/// (percents), so every accessor is a single multiply by `pt_to_px(font_pt)` and
/// the per-construct scaling lives in exactly one place. When no math face is
/// loaded [`proxy`] returns the literal em-fraction proxies, making the
/// no-math-face path byte-identical.
struct MathConstantsPx {
    axis_height: f64,
    fraction_rule_thickness: f64,
    fraction_numerator_gap: f64,
    fraction_denominator_gap: f64,
    superscript_shift_up: f64,
    subscript_shift_down: f64,
    sub_superscript_gap: f64,
    script_percent: f64,
    upper_limit_gap: f64,
    lower_limit_gap: f64,
    stack_gap: f64,
    radical_vertical_gap: f64,
    radical_rule_thickness: f64,
    radical_extra_ascender: f64,
    overbar_gap: f64,
    overbar_thickness: f64,
    overbar_extra_ascender: f64,
    underbar_gap: f64,
    underbar_thickness: f64,
    display_operator_min_height: f64,
    accent_gap: f64,
}

impl MathConstantsPx {
    /// The proxy em-fraction / ratio literals — the guard-default so a
    /// missing math face reproduces prior geometry exactly.
    fn proxy() -> Self {
        MathConstantsPx {
            axis_height: 0.26,
            fraction_rule_thickness: 0.06,
            fraction_numerator_gap: 0.15,
            fraction_denominator_gap: 0.15,
            superscript_shift_up: 0.45,
            subscript_shift_down: 0.20,
            sub_superscript_gap: 0.0,
            script_percent: 0.70,
            upper_limit_gap: 0.12,
            lower_limit_gap: 0.12,
            stack_gap: 0.35,
            radical_vertical_gap: 0.12,
            radical_rule_thickness: 0.06,
            radical_extra_ascender: 0.0,
            overbar_gap: 0.08,
            overbar_thickness: 0.06,
            overbar_extra_ascender: 0.0,
            underbar_gap: 0.08,
            underbar_thickness: 0.06,
            display_operator_min_height: 1.8, // proxy: NARY_OP_SCALE natural-height target
            accent_gap: 0.05,
        }
    }

    fn from_face(face: &MathFace) -> Self {
        let Some(c) = face.face().and_then(|f| f.tables().math.and_then(|m| m.constants)) else {
            return Self::proxy();
        };
        let upem = face.upem;
        // MathValue records carry design units → em-fraction.
        let em = |v: ttf_parser::math::MathValue| v.value as f64 / upem;
        // Bare u16 design-unit heights → em-fraction (NOT percents).
        let em_u16 = |v: u16| v as f64 / upem;
        // Bare i16 percents → ratio.
        let pct = |v: i16| v as f64 / 100.0;
        MathConstantsPx {
            axis_height: em(c.axis_height()),
            fraction_rule_thickness: em(c.fraction_rule_thickness()),
            fraction_numerator_gap: em(c.fraction_numerator_gap_min()),
            fraction_denominator_gap: em(c.fraction_denominator_gap_min()),
            superscript_shift_up: em(c.superscript_shift_up()),
            subscript_shift_down: em(c.subscript_shift_down()),
            sub_superscript_gap: em(c.sub_superscript_gap_min()),
            script_percent: pct(c.script_percent_scale_down()),
            upper_limit_gap: em(c.upper_limit_gap_min()),
            lower_limit_gap: em(c.lower_limit_gap_min()),
            stack_gap: em(c.stack_gap_min()),
            radical_vertical_gap: em(c.radical_vertical_gap()),
            radical_rule_thickness: em(c.radical_rule_thickness()),
            radical_extra_ascender: em(c.radical_extra_ascender()),
            overbar_gap: em(c.overbar_vertical_gap()),
            overbar_thickness: em(c.overbar_rule_thickness()),
            overbar_extra_ascender: em(c.overbar_extra_ascender()),
            underbar_gap: em(c.underbar_vertical_gap()),
            underbar_thickness: em(c.underbar_rule_thickness()),
            display_operator_min_height: em_u16(c.display_operator_min_height()),
            // No direct MATH "accent gap" constant (accent_base_height is a
            // placement GATE, not a clearance) — keep the 0.05em proxy.
            accent_gap: 0.05,
        }
    }

    #[inline]
    fn axis(&self, pt: f64) -> f64 {
        self.axis_height * pt_to_px(pt)
    }
}

// ─── math alphabets (Unicode Mathematical Alphanumeric Symbols) ───────────────

/// The alphabet plane a run selects (m:scr × m:sty). Resolved per group in
/// [`layout_run_with`] and applied per character by [`math_alpha_pure`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MathPlane {
    /// Serif upright — identity for letters (the ASCII glyph).
    Normal,
    /// Serif math-italic (the OMML default for a single-letter variable).
    Italic,
    Bold,
    BoldItalic,
    Script,
    BoldScript,
    Fraktur,
    BoldFraktur,
    DoubleStruck,
    SansSerif,
    SansBold,
    SansItalic,
    SansBoldItalic,
    Monospace,
}

/// Map an ASCII letter / digit (and, as a documented floor, leave Greek and
/// everything else unchanged) to its Unicode Mathematical Alphanumeric Symbols
/// code point for `plane`, honoring the reserved letterlike-symbol holes. PURE:
/// no cmap guard (the caller applies the tofu guard). Non-letter/digit input is
/// returned unchanged.
pub(crate) fn math_alpha_pure(ch: char, plane: MathPlane) -> char {
    use MathPlane::*;
    // Uppercase / lowercase / digit plane bases.
    let (upper, lower, digit): (u32, u32, Option<u32>) = match plane {
        Normal => return ch, // identity
        Italic => (0x1D434, 0x1D44E, None),
        Bold => (0x1D400, 0x1D41A, Some(0x1D7CE)),
        BoldItalic => (0x1D468, 0x1D482, None),
        Script => (0x1D49C, 0x1D4B6, None),
        BoldScript => (0x1D4D0, 0x1D4EA, None),
        Fraktur => (0x1D504, 0x1D51E, None),
        BoldFraktur => (0x1D56C, 0x1D586, None),
        DoubleStruck => (0x1D538, 0x1D552, Some(0x1D7D8)),
        SansSerif => (0x1D5A0, 0x1D5BA, Some(0x1D7E2)),
        SansBold => (0x1D5D4, 0x1D5EE, Some(0x1D7EC)),
        SansItalic => (0x1D608, 0x1D622, None),
        SansBoldItalic => (0x1D63C, 0x1D656, None),
        Monospace => (0x1D670, 0x1D68A, Some(0x1D7F6)),
    };
    // Reserved letterlike-symbol holes (a slot in the 1D4xx block is RESERVED and
    // the real glyph lives in the Letterlike Symbols block).
    if let Some(h) = alpha_hole(ch, plane) {
        return char::from_u32(h).unwrap_or(ch);
    }
    let mapped = if ch.is_ascii_uppercase() {
        upper + (ch as u32 - 'A' as u32)
    } else if ch.is_ascii_lowercase() {
        lower + (ch as u32 - 'a' as u32)
    } else if ch.is_ascii_digit() {
        match digit {
            Some(d) => d + (ch as u32 - '0' as u32),
            None => return ch, // digits stay upright unless a digit-plane is set
        }
    } else {
        return ch; // Greek / operators / punctuation: identity (documented floor)
    };
    char::from_u32(mapped).unwrap_or(ch)
}

/// The Letterlike-Symbols overrides for the reserved 1D4xx slots.
fn alpha_hole(ch: char, plane: MathPlane) -> Option<u32> {
    use MathPlane::*;
    match plane {
        Italic | SansItalic | BoldItalic | SansBoldItalic => match ch {
            'h' => Some(0x210E), // Planck constant (reserved math-italic h)
            _ => None,
        },
        Script | BoldScript => match ch {
            'B' => Some(0x212C), 'E' => Some(0x2130), 'F' => Some(0x2131), 'H' => Some(0x210B),
            'I' => Some(0x2110), 'L' => Some(0x2112), 'M' => Some(0x2133), 'R' => Some(0x211B),
            'e' => Some(0x212F), 'g' => Some(0x210A), 'o' => Some(0x2134),
            _ => None,
        },
        Fraktur | BoldFraktur => match ch {
            'C' => Some(0x212D), 'H' => Some(0x210C), 'I' => Some(0x2111), 'R' => Some(0x211C),
            'Z' => Some(0x2128),
            _ => None,
        },
        DoubleStruck => match ch {
            'C' => Some(0x2102), 'H' => Some(0x210D), 'N' => Some(0x2115), 'P' => Some(0x2119),
            'Q' => Some(0x211A), 'R' => Some(0x211D), 'Z' => Some(0x2124),
            _ => None,
        },
        _ => None,
    }
}

/// One laid-out inline math atom (see module docs). All offsets in CSS px.
#[derive(Clone)]
pub(crate) struct MathBox {
    /// Total advance width of the atom.
    pub(crate) width: f64,
    /// Height above the atom's baseline.
    pub(crate) ascent: f64,
    /// Depth below the atom's baseline.
    pub(crate) descent: f64,
    /// Flattened primitives, positioned relative to the box origin
    /// (left edge, baseline).
    pub(crate) nodes: Vec<MathNode>,
    /// `true` for `<m:oMathPara>` display math (reserves full stacked height).
    pub(crate) is_display: bool,
}

/// A single flattened math primitive.
///
/// The `Glyph` variant is intentionally the larger one (it carries the shaped
/// run + style). A math atom holds only a handful of nodes, so the size skew is
/// immaterial; boxing the payload would obscure the `shifted` flatten path for
/// no measurable win.
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum MathNode {
    /// A shaped (or estimated) glyph run at `(dx, dy)` from the box origin;
    /// `dy` is the baseline offset (negative = raised, positive = lowered).
    Glyph {
        dx: f64,
        dy: f64,
        style: RunStyle,
        text: String,
        shaped: Option<ShapedRun>,
    },
    /// A filled horizontal bar (fraction rule), painted as a crisp rect.
    Rule {
        x: f64,
        y: f64,
        w: f64,
        thickness: f64,
    },
    /// A filled vertical bar (border-box left/right edge), painted as a crisp
    /// rect. The vertical companion to [`MathNode::Rule`]: `x`/`y` is the top of
    /// the stroke centre-line, `height` runs downward. Needed because a wide-thin
    /// horizontal `Rule` cannot express a vertical edge.
    VRule {
        x: f64,
        y: f64,
        thickness: f64,
        height: f64,
    },
    /// A MATH size-variant glyph painted by its OUTLINE, because a
    /// variant glyph has no cmap entry so `fill_text` cannot reach it. `segs` are
    /// device-px path commands relative to the glyph origin `(dx, dy)`; `ch` and
    /// `pt_equiv` are carried only for the geometry snapshot / introspection (the
    /// source char and a synthetic grown point size). Replayed through the
    /// existing Painter path vocabulary — no new opcode, no Path2D FFI.
    GlyphOutline {
        dx: f64,
        dy: f64,
        ch: String,
        pt_equiv: f64,
        color: String,
        segs: Vec<PathSeg>,
    },
}

impl MathNode {
    /// Re-seat this node by `(dx, dy)` when flattening a child box into a parent.
    fn shifted(self, dx: f64, dy: f64) -> MathNode {
        match self {
            MathNode::Glyph { dx: x, dy: y, style, text, shaped } => MathNode::Glyph {
                dx: x + dx,
                dy: y + dy,
                style,
                text,
                shaped,
            },
            MathNode::Rule { x, y, w, thickness } => MathNode::Rule {
                x: x + dx,
                y: y + dy,
                w,
                thickness,
            },
            MathNode::VRule { x, y, thickness, height } => MathNode::VRule {
                x: x + dx,
                y: y + dy,
                thickness,
                height,
            },
            MathNode::GlyphOutline { dx: x, dy: y, ch, pt_equiv, color, segs } => {
                MathNode::GlyphOutline { dx: x + dx, dy: y + dy, ch, pt_equiv, color, segs }
            }
        }
    }
}

impl MathBox {
    fn empty() -> Self {
        MathBox { width: 0.0, ascent: 0.0, descent: 0.0, nodes: Vec::new(), is_display: false }
    }

    /// Absorb `child`'s nodes at `(dx, dy)`, extending our extents to cover it.
    fn absorb(&mut self, child: MathBox, dx: f64, dy: f64) {
        for n in child.nodes {
            self.nodes.push(n.shifted(dx, dy));
        }
    }
}

// ─── entry points ────────────────────────────────────────────────────────────

/// Lay out one math zone into a flat atom. `base` carries the body font family +
/// size + color; `is_display` marks `<m:oMathPara>`. Never panics; an
/// empty/parse-empty zone degrades to its plain-text glyph run.
pub(crate) fn layout_math_zone(
    zone: &MathZone,
    base: &RunStyle,
    fc: &FontCache,
    is_display: bool,
) -> MathBox {
    let ctx = MathContext::new(fc);
    let mut b = layout_zone(zone, base, &ctx);
    // Ultimate safety net: a zone that produced NOTHING AT ALL — no primitives
    // and no reserved extent — still shows its text (e.g. an all-unknown subtree
    // whose text() is non-empty). A zone that reserved extent but drew nothing is
    // a deliberate `m:phant` (invisible spacer) and must stay blank, so the net
    // is gated on zero width as well as zero nodes.
    if b.nodes.is_empty() && b.width == 0.0 {
        let text = zone.text();
        if !text.is_empty() {
            b = glyph_box(&text, &ctx.route(base), &ctx);
        }
    }
    b.is_display = is_display;
    b
}

/// Walk a paragraph's `extra_children`, parse every `<m:oMath>` /
/// `<m:oMathPara>` subtree and lay it out, tagging each with the child
/// `position` recorded by the parser (the paragraph-child index — see
/// `collect_items`'s interleave). Mirrors `ooxml_wml::ext::collect_math_from_raw`
/// (descends `oMathPara` to its inner `oMath`, recurses through unknown
/// wrappers) but PRESERVES the position that `math_expressions()` drops.
///
/// One extra-child node can yield zero or several atoms (an unknown wrapper, or
/// an `oMathPara` with multiple inner `oMath`s); all share that node's position.
pub(crate) fn collect_paragraph_math(
    paragraph: &ooxml_wml::types::Paragraph,
    base: &RunStyle,
    fc: &FontCache,
) -> Vec<(usize, Box<MathBox>)> {
    let mut out: Vec<(usize, Box<MathBox>)> = Vec::new();
    for child in &paragraph.extra_children {
        if let ooxml_wml::RawXmlNode::Element(elem) = &child.node {
            collect_from_elem(elem, child.position, base, fc, &mut out);
        }
    }
    // Stable sort by position so same-position atoms keep document order.
    out.sort_by_key(|(pos, _)| *pos);
    out
}

fn collect_from_elem(
    elem: &ooxml_wml::RawXmlElement,
    position: usize,
    base: &RunStyle,
    fc: &FontCache,
    out: &mut Vec<(usize, Box<MathBox>)>,
) {
    match math_local_name(&elem.name) {
        "oMathPara" => {
            // Display math: lay out each inner oMath child.
            for c in &elem.children {
                if let ooxml_wml::RawXmlNode::Element(e) = c {
                    if math_local_name(&e.name) == "oMath" {
                        let zone = e.parse_as::<MathZone>().unwrap_or_default();
                        out.push((position, Box::new(layout_math_zone(&zone, base, fc, true))));
                    }
                }
            }
        }
        "oMath" => {
            let zone = elem.parse_as::<MathZone>().unwrap_or_default();
            out.push((position, Box::new(layout_math_zone(&zone, base, fc, false))));
        }
        _ => {
            // Unknown wrapper — recurse so nested math is still found.
            for c in &elem.children {
                if let ooxml_wml::RawXmlNode::Element(e) = c {
                    collect_from_elem(e, position, base, fc, out);
                }
            }
        }
    }
}

/// Local name of a possibly-namespaced element name (`m:oMath` -> `oMath`).
#[inline]
fn math_local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// Does this paragraph carry any OMML zone (`<m:oMath>` / `<m:oMathPara>`,
/// possibly under unknown wrappers)? Drives the [`MATH_FAMILY`] font-plan seed
/// in `lib.rs::families_used_native` — only a doc that actually contains math
/// pays the ~548 KB XITS Math fetch.
pub(crate) fn paragraph_has_math(paragraph: &ooxml_wml::types::Paragraph) -> bool {
    fn scan(elem: &ooxml_wml::RawXmlElement) -> bool {
        match math_local_name(&elem.name) {
            "oMath" | "oMathPara" => true,
            _ => elem.children.iter().any(|c| match c {
                ooxml_wml::RawXmlNode::Element(e) => scan(e),
                _ => false,
            }),
        }
    }
    paragraph.extra_children.iter().any(|child| match &child.node {
        ooxml_wml::RawXmlNode::Element(elem) => scan(elem),
        _ => false,
    })
}

/// Is `paragraph` a DISPLAY equation whose own line is just one equation?
///
/// TRUE when any `extra_children` node is an `<m:oMathPara>` (always display),
/// OR the paragraph carries exactly ONE bare `<m:oMath>` (walking unknown
/// wrappers like [`collect_from_elem`]) AND has no significant run text AND no
/// inline drawing/picture — the "bare oMath owns its line" case that Word
/// centers via its built-in `centerGroup` default math justification.
///
/// FALSE when the `<m:oMath>` is seated among real runs (`text().trim()` is
/// non-empty) — inline math stays inline. `Paragraph::text()` walks only
/// `paragraph_content`, never `extra_children`, so math glyph text never leaks
/// into this gate.
pub(crate) fn is_display_math_paragraph(paragraph: &ooxml_wml::types::Paragraph) -> bool {
    let mut has_math_para = false;
    let mut bare_omath = 0usize;
    for child in &paragraph.extra_children {
        if let ooxml_wml::RawXmlNode::Element(elem) = &child.node {
            count_display_math(elem, &mut has_math_para, &mut bare_omath);
        }
    }
    if has_math_para {
        return true;
    }
    if bare_omath != 1 {
        return false;
    }
    // A lone bare oMath is display only when it OWNS the line: no run text and
    // no inline image.
    if !paragraph.text().trim().is_empty() {
        return false;
    }
    !paragraph.runs().iter().any(|r| r.has_images())
}

fn count_display_math(
    elem: &ooxml_wml::RawXmlElement,
    has_math_para: &mut bool,
    bare_omath: &mut usize,
) {
    match math_local_name(&elem.name) {
        "oMathPara" => *has_math_para = true,
        "oMath" => *bare_omath += 1,
        _ => {
            for c in &elem.children {
                if let ooxml_wml::RawXmlNode::Element(e) = c {
                    count_display_math(e, has_math_para, bare_omath);
                }
            }
        }
    }
}

// ─── layout ──────────────────────────────────────────────────────────────────

fn layout_zone(zone: &MathZone, style: &RunStyle, ctx: &MathContext) -> MathBox {
    row(zone.elements.iter().map(|e| layout_element(e, style, ctx)))
}

fn layout_element(el: &MathElement, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let laid = match el {
        MathElement::Run(r) => layout_run(r, style, ctx),
        MathElement::Fraction(f) => layout_fraction(f, style, ctx),
        MathElement::Superscript(s) => layout_script(s, style, ctx, ScriptKind::Super),
        MathElement::Subscript(s) => layout_script(s, style, ctx, ScriptKind::Sub),
        MathElement::SubSuperscript(s) => layout_subsup(s, style, ctx),
        MathElement::Nary(n) => layout_nary(n, style, ctx),
        MathElement::Radical(r) => layout_radical(r, style, ctx),
        MathElement::Delimiter(d) => layout_delimiter(d, style, ctx),
        MathElement::Matrix(m) => layout_matrix(m, style, ctx),
        MathElement::EquationArray(e) => layout_equation_array(e, style, ctx),
        MathElement::Function(f) => layout_function(f, style, ctx),
        MathElement::Accent(a) => layout_accent(a, style, ctx),
        MathElement::Bar(b) => layout_bar(b, style, ctx),
        MathElement::GroupChar(g) => layout_group_char(g, style, ctx),
        MathElement::LowerLimit(l) => layout_lower_limit(l, style, ctx),
        MathElement::UpperLimit(l) => layout_upper_limit(l, style, ctx),
        MathElement::PreScript(p) => layout_pre_script(p, style, ctx),
        MathElement::Box(b) => layout_box(b, style, ctx),
        MathElement::BorderBox(b) => layout_border_box(b, style, ctx),
        MathElement::Phantom(p) => layout_phantom(p, style, ctx),
    };
    // Ultimate degrade: a construct whose armed layout produced NO primitives
    // (an all-empty child subtree) still shows its plain text via `linearize`'s
    // glyph fallback — nothing ever vanishes. A `Phantom` is EXEMPT: its empty
    // node list is intentional (it reserves width while painting nothing), so it
    // must not be re-linearized back into visible glyphs. Every populated
    // construct (all real fixtures) returns unchanged.
    if laid.nodes.is_empty() && !matches!(el, MathElement::Phantom(_)) {
        return linearize(el, style, ctx);
    }
    laid
}

/// Horizontally concatenate boxes on a shared baseline (dy = 0).
fn row<I: IntoIterator<Item = MathBox>>(boxes: I) -> MathBox {
    let mut out = MathBox::empty();
    let mut dx = 0.0;
    for b in boxes {
        out.ascent = out.ascent.max(b.ascent);
        out.descent = out.descent.max(b.descent);
        let w = b.width;
        out.absorb(b, dx, 0.0);
        dx += w;
    }
    out.width = dx;
    out
}

/// A style clone scaled to `factor` of the current size (scripts, limits).
fn scaled(style: &RunStyle, factor: f64) -> RunStyle {
    let mut s = style.clone();
    s.font_pt *= factor;
    s
}

// Script down-scale (script_percent_scale_down), super/sub baseline shifts
// (superscript_shift_up / subscript_shift_down) now come from
// [`MathConstantsPx`] — real MATH-table values with a math face, the
// em-fraction proxies (0.70 / 0.45 / 0.20) without one.

// ─── text runs (leaf) ────────────────────────────────────────────────────────

/// Is `c` a math *variable* letter (rendered italic by the OMML convention)?
/// ASCII letters and the Greek letter blocks; digits/operators/space stay
/// upright even inside one run (Word splits letters↔digits).
fn is_math_variable(c: char) -> bool {
    c.is_ascii_alphabetic()
        || matches!(
            c as u32,
            0x0391..=0x03A9 | 0x03B1..=0x03C9 | 0x03D1..=0x03D6 | 0x03F0..=0x03F6
        )
}

fn layout_run(r: &MathRun, base: &RunStyle, ctx: &MathContext) -> MathBox {
    layout_run_with(r, base, ctx, None)
}

/// Resolve the alphabet plane from the run's `m:scr` script, its bold flag, and
/// the per-group slant decision.
fn resolve_plane(script: Option<MathScript>, bold: bool, slant: bool) -> MathPlane {
    use MathPlane as P;
    match script {
        Some(MathScript::DoubleStruck) => P::DoubleStruck,
        Some(MathScript::Script) => if bold { P::BoldScript } else { P::Script },
        Some(MathScript::Fraktur) => if bold { P::BoldFraktur } else { P::Fraktur },
        Some(MathScript::SansSerif) => match (bold, slant) {
            (true, true) => P::SansBoldItalic,
            (true, false) => P::SansBold,
            (false, true) => P::SansItalic,
            (false, false) => P::SansSerif,
        },
        Some(MathScript::Monospace) => P::Monospace,
        Some(MathScript::Roman) | None => match (bold, slant) {
            (true, true) => P::BoldItalic,
            (true, false) => P::Bold,
            // The OMML default single-letter VARIABLE renders math-italic; the
            // slant comes from the code point (there is no separate italic face).
            (false, true) => P::Italic,
            (false, false) => P::Normal,
        },
    }
}

/// Lay out a math run, optionally forcing the italic decision for every
/// sub-run. `upright_override` is the FORCED italic flag: `Some(false)` renders
/// every character UPRIGHT (used for function names like `sin`/`log`, which are
/// operators not variables), `Some(true)` forces italic, and `None` falls back
/// to the run's `m:nor`/`m:sty` properties and then the per-char
/// [`is_math_variable`] classing.
fn layout_run_with(
    r: &MathRun,
    base: &RunStyle,
    ctx: &MathContext,
    upright_override: Option<bool>,
) -> MathBox {
    let props = r.properties.as_ref();
    // Explicit override wins; else m:nor (normal text) or m:sty=Plain forces
    // upright; Italic/BoldItalic force italic; otherwise per-char classing
    // decides. m:sty Bold/BoldItalic => bold.
    let force_italic: Option<bool> = if let Some(ital) = upright_override {
        Some(ital)
    } else if props.map(|p| p.normal).unwrap_or(false) {
        Some(false)
    } else {
        match props.and_then(|p| p.style) {
            Some(MathStyle::Plain) => Some(false),
            Some(MathStyle::Italic) | Some(MathStyle::BoldItalic) => Some(true),
            _ => None,
        }
    };
    let bold = matches!(
        props.and_then(|p| p.style),
        Some(MathStyle::Bold) | Some(MathStyle::BoldItalic)
    );
    let script = props.and_then(|p| p.script);
    let math = ctx.family.is_some();

    // Group contiguous same-slant characters into sub-runs (so per-letter
    // introspection / snapshot lookups still resolve a single variable).
    let mut groups: Vec<(bool, String)> = Vec::new();
    for ch in r.text.chars() {
        let slant = force_italic.unwrap_or_else(|| is_math_variable(ch));
        match groups.last_mut() {
            Some(last) if last.0 == slant => last.1.push(ch),
            _ => groups.push((slant, ch.to_string())),
        }
    }

    let mut out = MathBox::empty();
    let mut dx = 0.0;
    for (slant, text) in groups {
        let plane = resolve_plane(script, bold, slant);
        // Per-character alphabet mapping — ONLY when a math face is routed (a
        // body-font miss leaves the ASCII char unchanged, as on the proxy path),
        // and only when the mapped code point actually has a glyph (tofu guard).
        let text: String = if math {
            text.chars()
                .map(|c| {
                    let m = math_alpha_pure(c, plane);
                    if m == c {
                        c
                    } else if ctx.face.map(|f| f.has_glyph(m)).unwrap_or(false) {
                        m
                    } else {
                        c
                    }
                })
                .collect()
        } else {
            text
        };
        let mut style = ctx.route(base);
        // With a math face the slant/weight is baked into the code point (the
        // single Regular master carries no italic/bold cut, so a synthetic flag
        // would double-slant); without one, keep the synthetic emphasis.
        style.italic = if math { false } else { slant };
        style.bold = if math { false } else { bold };
        let (shaped, w, a, d) = measure(&text, &style, ctx);
        out.ascent = out.ascent.max(a);
        out.descent = out.descent.max(d);
        out.nodes.push(MathNode::Glyph { dx, dy: 0.0, style, text, shaped });
        dx += w;
    }
    out.width = dx;
    out
}

/// Shape `text` in `style`'s font (or estimate). Returns
/// `(shaped, width, ascent, descent)` in px.
fn measure(text: &str, style: &RunStyle, ctx: &MathContext<'_>) -> (Option<ShapedRun>, f64, f64, f64) {
    let weight = if style.bold { 700 } else { 400 };
    if let Some(font) = ctx.fc.resolve(&style.family, weight, style.italic) {
        let shaped = ctx.faces.shape(text, font, style.font_pt);
        let m = font_metrics(font, style.font_pt);
        let width = shaped
            .as_ref()
            .map(|s| s.total_advance)
            .unwrap_or_else(|| estimate_width(text, style));
        (shaped, width, m.ascent, m.descent)
    } else {
        let em = pt_to_px(style.font_pt);
        (None, estimate_width(text, style), em * 0.8, em * 0.2)
    }
}

fn estimate_width(text: &str, style: &RunStyle) -> f64 {
    text.chars().count() as f64 * pt_to_px(style.font_pt) * 0.5
}

/// A single-run box carrying `text` in `style` (used by `linearize` and the
/// empty-zone safety net).
fn glyph_box(text: &str, style: &RunStyle, ctx: &MathContext<'_>) -> MathBox {
    let (shaped, w, a, d) = measure(text, style, ctx);
    MathBox {
        width: w,
        ascent: a,
        descent: d,
        nodes: vec![MathNode::Glyph { dx: 0.0, dy: 0.0, style: style.clone(), text: text.to_string(), shaped }],
        is_display: false,
    }
}

// ─── scripts ─────────────────────────────────────────────────────────────────

enum ScriptKind {
    Super,
    Sub,
}

fn layout_script(s: &Script, style: &RunStyle, ctx: &MathContext, kind: ScriptKind) -> MathBox {
    let base = layout_zone(&s.base, style, ctx);
    let script = layout_zone(&s.script, &scaled(style, ctx.consts.script_percent), ctx);
    let em = pt_to_px(style.font_pt);
    let mut out = MathBox::empty();
    let base_w = base.width;
    let script_w = script.width;
    let base_asc = base.ascent;
    let base_desc = base.descent;
    let script_asc = script.ascent;
    let script_desc = script.descent;

    out.absorb(base, 0.0, 0.0);
    match kind {
        ScriptKind::Super => {
            let shift = ctx.consts.superscript_shift_up * em; // raise
            out.absorb(script, base_w, -shift);
            out.ascent = base_asc.max(shift + script_asc);
            out.descent = base_desc;
        }
        ScriptKind::Sub => {
            let shift = ctx.consts.subscript_shift_down * em; // lower
            out.absorb(script, base_w, shift);
            out.ascent = base_asc;
            out.descent = base_desc.max(shift + script_desc);
        }
    }
    out.width = base_w + script_w;
    out
}

fn layout_subsup(s: &SubSuperscript, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let base = layout_zone(&s.base, style, ctx);
    let sub = layout_zone(&s.subscript, &scaled(style, ctx.consts.script_percent), ctx);
    let sup = layout_zone(&s.superscript, &scaled(style, ctx.consts.script_percent), ctx);
    let em = pt_to_px(style.font_pt);
    let mut sup_shift = ctx.consts.superscript_shift_up * em;
    let mut sub_shift = ctx.consts.subscript_shift_down * em;

    let base_w = base.width;
    let scripts_w = sub.width.max(sup.width);
    let base_asc = base.ascent;
    let base_desc = base.descent;
    let sup_asc = sup.ascent;
    let sup_desc = sup.descent;
    let sub_asc = sub.ascent;
    let sub_desc = sub.descent;

    // Enforce the MATH sub_superscript_gap_min clearance between the sup's bottom
    // and the sub's top (0 in the proxy → no effect, byte-identical).
    let clearance = sub_shift + sup_shift - sub_asc - sup_desc;
    let deficit = ctx.consts.sub_superscript_gap * em - clearance;
    if deficit > 0.0 {
        sup_shift += deficit / 2.0;
        sub_shift += deficit / 2.0;
    }

    let mut out = MathBox::empty();
    out.absorb(base, 0.0, 0.0);
    out.absorb(sup, base_w, -sup_shift);
    out.absorb(sub, base_w, sub_shift);
    out.width = base_w + scripts_w;
    out.ascent = base_asc.max(sup_shift + sup_asc);
    out.descent = base_desc.max(sub_shift + sub_desc);
    out
}

// ─── fractions ───────────────────────────────────────────────────────────────

fn layout_fraction(f: &Fraction, style: &RunStyle, ctx: &MathContext) -> MathBox {
    use ooxml_omml::FractionType;

    let num = layout_zone(&f.numerator, style, ctx);
    let den = layout_zone(&f.denominator, style, ctx);
    let em = pt_to_px(style.font_pt);
    let axis = ctx.consts.axis(style.font_pt); // real MATH axis_height (else proxy)
    let pad = 0.1 * em; // numerator/denominator horizontal inset (no MATH equivalent)
    // Numerator gap above the bar / denominator gap below it, from the MATH table
    // (fraction_numerator_gap_min); the proxy is 0.15em → byte-identical without
    // a math face.
    let gap = ctx.consts.fraction_numerator_gap * em;
    let den_gap = ctx.consts.fraction_denominator_gap * em;
    let thickness = (ctx.consts.fraction_rule_thickness * em).max(1.0);
    let no_bar = matches!(f.fraction_type, Some(FractionType::NoBar));

    let w = num.width.max(den.width) + 2.0 * pad;
    let num_w = num.width;
    let den_w = den.width;
    let num_asc = num.ascent;
    let num_desc = num.descent;
    let den_asc = den.ascent;
    let den_desc = den.descent;

    let mut out = MathBox::empty();
    // Numerator: centered, sitting `gap` above the bar (which is `axis` above
    // the baseline). Its baseline is placed so its descent-bottom clears the gap.
    out.absorb(num, (w - num_w) / 2.0, -(axis + gap + num_desc));
    // Denominator: centered, hanging below the bar by `thickness + den_gap`.
    out.absorb(den, (w - den_w) / 2.0, -axis + thickness + den_gap + den_asc);
    // The fraction rule (omitted for NoBar / Linear-degrade with no bar).
    if !no_bar {
        out.nodes.push(MathNode::Rule { x: 0.0, y: -axis, w, thickness });
    }
    out.width = w;
    out.ascent = axis + gap + num_asc + num_desc;
    out.descent = -axis + thickness + den_gap + den_asc + den_desc;
    out
}

// ─── n-ary operators (∑ ∫ ∏) ─────────────────────────────────────────────────

/// Large-operator enlargement factor (Word renders ∑/∫ well above run size).
const NARY_OP_SCALE: f64 = 1.8;

/// Natural (ascent+descent) height of `text` rendered at `style`'s size.
fn natural_height(text: &str, style: &RunStyle, ctx: &MathContext<'_>) -> f64 {
    let (_, _, a, d) = measure(text, style, ctx);
    a + d
}

/// A single glyph grown so its natural height ≈ `target_h`, by scaling the font
/// point size (clamped 1.0–3.0×). Aspect distortion (width grows with height) is
/// the accepted proxy for true stretchy glyph assembly. The natural
/// height is guarded to `>= 1.0` so the ratio can never divide by zero.
fn grown_glyph(ch: &str, target_h: f64, style: &RunStyle, ctx: &MathContext<'_>) -> MathBox {
    let natural = natural_height(ch, style, ctx).max(1.0);
    let scale = (target_h / natural).clamp(1.0, 3.0);
    glyph_box(ch, &scaled(style, scale), ctx)
}

/// The dy (down-positive baseline offset) that seats box `b`'s vertical centre on
/// the math axis `center_y` above the baseline (positive = up).
fn center_on(b: &MathBox, center_y: f64) -> f64 {
    -center_y + (b.ascent - b.descent) / 2.0
}

/// N-ary operators whose limits STACK over/under (∑ ∏ ∐ ⋃ ⋂ ⋁ ⋀) vs. integrals
/// (∫ …) whose limits sit as right-hand scripts. Drives the undOvr-vs-subSup
/// choice when the parser leaves `limit_location` unset.
fn is_big_operator(op: &str) -> bool {
    matches!(
        op.chars().next().map(|c| c as u32),
        Some(0x2211 | 0x220F | 0x2210 | 0x22C3 | 0x22C2 | 0x22C1 | 0x22C0)
    )
}

fn layout_nary(n: &Nary, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let em = pt_to_px(style.font_pt);
    let axis = ctx.consts.axis(style.font_pt);
    let gap = 0.12 * em; // limit clearance (upper/lower_limit_gap folded below)

    let op_char = n.operator.as_deref().unwrap_or("\u{222B}"); // default ∫ (ECMA)
    // The large operator: a MATH vertical size variant sized to
    // display_operator_min_height with a math face; the 1.8× point-scale
    // proxy without one (byte-identical).
    let op = if ctx.face.is_some() {
        let target = ctx.consts.display_operator_min_height * em;
        ctx.vstretch(op_char, target, style)
    } else {
        glyph_box(op_char, &scaled(style, NARY_OP_SCALE), ctx)
    };
    let small = scaled(style, ctx.consts.script_percent);
    let sub = layout_zone(&n.subscript, &small, ctx);
    let sup = layout_zone(&n.superscript, &small, ctx);
    let operand = layout_zone(&n.base, style, ctx);

    // Honor an explicit m:limLoc; otherwise heuristic on the operator class.
    let stacked = match n.limit_location {
        Some(LimitLocation::UnderOver) => true,
        Some(LimitLocation::SubSup) => false,
        None => is_big_operator(op_char),
    };

    if stacked {
        layout_nary_undovr(op, sub, sup, operand, gap, axis)
    } else {
        layout_nary_subsup(op, sub, sup, operand, style, ctx)
    }
}

/// Stacked (under/over) limits — the ∑ case. Sup centred above the operator,
/// sub centred below, operand to the right on the baseline.
fn layout_nary_undovr(
    op: MathBox,
    sub: MathBox,
    sup: MathBox,
    operand: MathBox,
    gap: f64,
    axis: f64,
) -> MathBox {
    let op_w = op.width;
    let op_asc = op.ascent;
    let op_desc = op.descent;
    let sub_w = sub.width;
    let sup_w = sup.width;
    let sub_asc = sub.ascent;
    let sub_desc = sub.descent;
    let sup_asc = sup.ascent;
    let sup_desc = sup.descent;
    let has_sub = !sub.nodes.is_empty();
    let has_sup = !sup.nodes.is_empty();
    let operand_w = operand.width;
    let operand_asc = operand.ascent;
    let operand_desc = operand.descent;

    let center_w = op_w.max(sub_w).max(sup_w);

    let mut out = MathBox::empty();
    // Operator lifted onto the math axis; its visual extents follow the lift.
    out.absorb(op, (center_w - op_w) / 2.0, -axis);
    let op_top = op_asc + axis; // above baseline
    let op_bottom = op_desc - axis; // below baseline

    let mut ascent = op_top;
    let mut descent = op_bottom.max(0.0);

    if has_sup {
        let dy = -(op_top + gap + sup_desc);
        out.absorb(sup, (center_w - sup_w) / 2.0, dy);
        ascent = ascent.max(op_top + gap + sup_desc + sup_asc);
    }
    if has_sub {
        // Hug the operator's visual bottom (the axis lift is folded into op_bottom).
        let dy = op_bottom + gap + sub_asc;
        out.absorb(sub, (center_w - sub_w) / 2.0, dy);
        descent = descent.max(op_bottom + gap + sub_asc + sub_desc);
    }

    out.absorb(operand, center_w + gap, 0.0);
    ascent = ascent.max(operand_asc);
    descent = descent.max(operand_desc);

    out.width = center_w + gap + operand_w;
    out.ascent = ascent;
    out.descent = descent;
    out
}

/// Script-placed limits — the ∫ case. Sup/sub sit as right-hand scripts on the
/// enlarged operator (the layout_subsup shift math, replicated against the
/// pre-laid operator box), operand to the right.
fn layout_nary_subsup(
    op: MathBox,
    sub: MathBox,
    sup: MathBox,
    operand: MathBox,
    style: &RunStyle,
    ctx: &MathContext,
) -> MathBox {
    let em = pt_to_px(style.font_pt);
    let sup_shift = ctx.consts.superscript_shift_up * em;
    let sub_shift = ctx.consts.subscript_shift_down * em;

    let op_w = op.width;
    let op_asc = op.ascent;
    let op_desc = op.descent;
    let scripts_w = sub.width.max(sup.width);
    let sup_asc = sup.ascent;
    let sub_desc = sub.descent;
    let has_sub = !sub.nodes.is_empty();
    let has_sup = !sup.nodes.is_empty();
    let operand_w = operand.width;
    let operand_asc = operand.ascent;
    let operand_desc = operand.descent;

    let mut out = MathBox::empty();
    out.absorb(op, 0.0, 0.0);
    if has_sup {
        out.absorb(sup, op_w, -sup_shift);
    }
    if has_sub {
        out.absorb(sub, op_w, sub_shift);
    }
    let script_col = op_w + scripts_w;
    out.absorb(operand, script_col, 0.0);

    out.width = script_col + operand_w;
    out.ascent = op_asc.max(sup_shift + sup_asc).max(operand_asc);
    out.descent = op_desc.max(sub_shift + sub_desc).max(operand_desc);
    out
}

// ─── delimiters (stretchy brackets) ──────────────────────────────────────────

fn layout_delimiter(d: &Delimiter, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let sep = d.separator_char.as_deref().unwrap_or(",");
    let mut inner: Vec<MathBox> = Vec::new();
    for (i, child) in d.elements.iter().enumerate() {
        if i > 0 {
            inner.push(glyph_box(sep, &ctx.route(style), ctx));
        }
        inner.push(layout_zone(child, style, ctx));
    }
    let content = row(inner);
    let content_asc = content.ascent;
    let content_desc = content.descent;
    let content_w = content.width;
    let content_h = content_asc + content_desc;
    let center_y = (content_asc - content_desc) / 2.0; // math axis of the content

    let beg = d.begin_char.as_deref().unwrap_or("(");
    let end = d.end_char.as_deref().unwrap_or(")");
    // Stretchy brackets: MATH vertical size variants (true metrics) with a math
    // face; the point-scale proxy without one.
    let left = ctx.vstretch(beg, content_h, style);
    let right = ctx.vstretch(end, content_h, style);
    let left_w = left.width;
    let right_w = right.width;
    let left_asc = left.ascent;
    let left_desc = left.descent;
    let right_asc = right.ascent;
    let right_desc = right.descent;
    let ldy = center_on(&left, center_y);
    let rdy = center_on(&right, center_y);
    // Visual extents of each bracket after the centring shift.
    let l_top = left_asc - ldy;
    let l_bot = left_desc + ldy;
    let r_top = right_asc - rdy;
    let r_bot = right_desc + rdy;

    let mut out = MathBox::empty();
    out.absorb(left, 0.0, ldy);
    out.absorb(content, left_w, 0.0);
    out.absorb(right, left_w + content_w, rdy);
    out.width = left_w + content_w + right_w;
    out.ascent = content_asc.max(l_top).max(r_top);
    out.descent = content_desc.max(l_bot).max(r_bot);
    out
}

// ─── radicals (√ + vinculum) ─────────────────────────────────────────────────

fn layout_radical(r: &Radical, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let em = pt_to_px(style.font_pt);
    let gap = ctx.consts.radical_vertical_gap * em;
    let thickness = (ctx.consts.radical_rule_thickness * em).max(1.0);
    let extra = ctx.consts.radical_extra_ascender * em;

    let radicand = layout_zone(&r.base, style, ctx);
    let rad_asc = radicand.ascent;
    let rad_desc = radicand.descent;
    let rad_w = radicand.width;
    let rad_h = rad_asc + rad_desc;

    // √ sign: a MATH vertical size variant sized to the radicand height with a
    // math face; the point-scale proxy without one.
    let sign = ctx.vstretch("\u{221A}", rad_h + gap, style);
    let sign_w = sign.width;
    let sign_asc = sign.ascent;

    // Degree (nth root) at ~0.5 scale, upper-left. The parser leaves an empty
    // degree zone for a plain √, so empty-zone == no degree.
    let has_degree = !r.degree.elements.is_empty() && !r.hide_degree;
    let degree = if has_degree {
        Some(layout_zone(&r.degree, &scaled(style, 0.5), ctx))
    } else {
        None
    };
    let deg_w = degree.as_ref().map(|d| d.width).unwrap_or(0.0);

    let mut out = MathBox::empty();
    // radical_extra_ascender adds clearance above the vinculum (0 in the proxy).
    let mut ascent = (rad_asc + gap + thickness + extra).max(sign_asc);
    let descent = rad_desc;

    // Radical sign, baseline-aligned, to the right of any degree.
    out.absorb(sign, deg_w, 0.0);

    if let Some(deg) = degree {
        let deg_asc = deg.ascent;
        let deg_desc = deg.descent;
        // Raise the degree so it hugs the vinculum's upper-left.
        let dy = -(rad_asc + gap - deg_desc);
        out.absorb(deg, 0.0, dy);
        ascent = ascent.max(deg_asc - dy);
    }

    // Radicand, to the right of the sign.
    out.absorb(radicand, deg_w + sign_w, 0.0);
    // Vinculum (overbar): spans the radicand width, `rad_asc + gap` above the
    // baseline (negative y = up), matching the fraction Rule convention.
    out.nodes.push(MathNode::Rule {
        x: deg_w + sign_w,
        y: -(rad_asc + gap),
        w: rad_w,
        thickness,
    });

    out.width = deg_w + sign_w + rad_w;
    out.ascent = ascent;
    out.descent = descent;
    out
}

// ─── matrices (grid) ─────────────────────────────────────────────────────────

fn layout_matrix(m: &Matrix, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let em = pt_to_px(style.font_pt);
    let axis = ctx.consts.axis(style.font_pt);
    let col_gap = 0.6 * em;
    let row_gap = 0.35 * em;

    let grid: Vec<Vec<MathBox>> = m
        .rows
        .iter()
        .map(|row| row.iter().map(|c| layout_zone(c, style, ctx)).collect())
        .collect();
    let nrows = grid.len();
    let ncols = grid.iter().map(|r| r.len()).max().unwrap_or(0);
    if nrows == 0 || ncols == 0 {
        return MathBox::empty();
    }

    // Per-column max width; per-row max ascent/descent.
    let mut col_w = vec![0.0f64; ncols];
    for row in &grid {
        for (ci, cell) in row.iter().enumerate() {
            col_w[ci] = col_w[ci].max(cell.width);
        }
    }
    let row_asc: Vec<f64> = grid
        .iter()
        .map(|r| r.iter().map(|c| c.ascent).fold(0.0, f64::max))
        .collect();
    let row_desc: Vec<f64> = grid
        .iter()
        .map(|r| r.iter().map(|c| c.descent).fold(0.0, f64::max))
        .collect();

    let total_h: f64 = row_asc.iter().sum::<f64>()
        + row_desc.iter().sum::<f64>()
        + row_gap * nrows.saturating_sub(1) as f64;
    let total_w: f64 =
        col_w.iter().sum::<f64>() + col_gap * ncols.saturating_sub(1) as f64;

    // Centre the whole grid on the math axis (`axis` above the baseline).
    let ascent = total_h / 2.0 + axis;
    let descent = total_h / 2.0 - axis;

    let mut out = MathBox::empty();
    let mut y_top = -ascent; // dy of the top edge (down-positive)
    for (ri, row) in grid.into_iter().enumerate() {
        let baseline_dy = y_top + row_asc[ri];
        let mut x = 0.0;
        for (ci, cell) in row.into_iter().enumerate() {
            let cw = col_w[ci];
            let dx = x + (cw - cell.width) / 2.0; // centre in the column (mcJc default)
            out.absorb(cell, dx, baseline_dy);
            x += cw + col_gap;
        }
        y_top += row_asc[ri] + row_desc[ri] + row_gap;
    }

    out.width = total_w;
    out.ascent = ascent;
    out.descent = descent;
    out
}

// ─── equation arrays, functions, accents, bars, groups, limits, ──────────────
//     pre-scripts, boxes, border boxes, phantoms

/// An advance-only spacer (no glyph, no vertical extent) used to seat a thin
/// space between a function name and its argument.
fn spacer(width: f64) -> MathBox {
    MathBox { width, ascent: 0.0, descent: 0.0, nodes: Vec::new(), is_display: false }
}

/// A single glyph grown so its natural WIDTH ≈ `target_w`, by scaling the font
/// point size (clamped 1.0–4.0×). The width-driven companion to [`grown_glyph`]
/// — used to stretch a group-char brace across the group it spans. Aspect
/// distortion (height grows with width) is the accepted proxy for true
/// stretchy glyph-part assembly. Natural width is guarded `>= 1.0`.
fn grown_glyph_w(ch: &str, target_w: f64, style: &RunStyle, ctx: &MathContext<'_>) -> MathBox {
    let (_, natural_w, _, _) = measure(ch, style, ctx);
    let scale = (target_w / natural_w.max(1.0)).clamp(1.0, 4.0);
    glyph_box(ch, &scaled(style, scale), ctx)
}

/// Which side a limit/label stacks on relative to its base.
enum StackSide {
    /// Above the base (upper limit).
    Over,
    /// Below the base (lower limit).
    Under,
}

/// Stack `lim` centred over or under `base` with `gap` between them, reserving
/// the extra ascent/descent. Shared by the `m:limUpp`/`m:limLow` paths (the same
/// centred over/under geometry the ∑ limits use).
fn stack_over_under(base: MathBox, lim: MathBox, side: StackSide, gap: f64) -> MathBox {
    let col_w = base.width.max(lim.width);
    let base_w = base.width;
    let base_asc = base.ascent;
    let base_desc = base.descent;
    let lim_w = lim.width;
    let lim_asc = lim.ascent;
    let lim_desc = lim.descent;

    let mut out = MathBox::empty();
    out.absorb(base, (col_w - base_w) / 2.0, 0.0);
    match side {
        StackSide::Over => {
            out.absorb(lim, (col_w - lim_w) / 2.0, -(base_asc + gap + lim_desc));
            out.ascent = base_asc + gap + lim_desc + lim_asc;
            out.descent = base_desc;
        }
        StackSide::Under => {
            out.absorb(lim, (col_w - lim_w) / 2.0, base_desc + gap + lim_asc);
            out.ascent = base_asc;
            out.descent = base_desc + gap + lim_asc + lim_desc;
        }
    }
    out.width = col_w;
    out
}

/// Equation array (`m:eqArr`): a single centred column of full-size rows. Mirrors
/// the single-column matrix path — every row at BASE size, centred on the math
/// axis — so wrapping the result in the already-working [`layout_delimiter`]
/// grows the brackets to the full stacked height (that is the binomial). Aligned
/// systems (the `&` alignment points) are not implemented; each row is
/// centred for now, which is exact for the 2-row binomial.
fn layout_equation_array(e: &EquationArray, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let em = pt_to_px(style.font_pt);
    let axis = ctx.consts.axis(style.font_pt); // real MATH axis_height (else proxy)
    // stack_gap_min from the MATH table (proxy 0.35em) — the inter-row spacing.
    let row_gap = ctx.consts.stack_gap * em;

    let rows: Vec<MathBox> = e.equations.iter().map(|z| layout_zone(z, style, ctx)).collect();
    let n = rows.len();
    if n == 0 {
        return MathBox::empty();
    }
    let max_w = rows.iter().map(|r| r.width).fold(0.0, f64::max);
    let total_h: f64 = rows.iter().map(|r| r.ascent + r.descent).sum::<f64>()
        + row_gap * n.saturating_sub(1) as f64;

    // Centre the stack on the math axis (mirrors layout_matrix).
    let ascent = total_h / 2.0 + axis;
    let descent = total_h / 2.0 - axis;

    let mut out = MathBox::empty();
    let mut y_top = -ascent;
    for r in rows {
        let asc_i = r.ascent;
        let desc_i = r.descent;
        let w_i = r.width;
        let baseline_dy = y_top + asc_i;
        out.absorb(r, (max_w - w_i) / 2.0, baseline_dy);
        y_top += asc_i + desc_i + row_gap;
    }

    out.width = max_w;
    out.ascent = ascent;
    out.descent = descent;
    out
}

/// Function application (`m:func`): the function NAME (rendered upright — sin/
/// log/tan are operators, not variables), a thin space, then the argument laid
/// out with normal italic classing, all on a shared baseline.
fn layout_function(f: &Function, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let thin = 0.17 * pt_to_px(style.font_pt); // advance-only thin space

    // Name: force every run upright; keep 2D for any non-run child (a fraction in
    // a function name still stacks).
    let name = row(f.name.elements.iter().map(|el| match el {
        MathElement::Run(r) => layout_run_with(r, style, ctx, Some(false)),
        other => layout_element(other, style, ctx),
    }));
    let arg = layout_zone(&f.argument, style, ctx);

    // `row` seats the three boxes on one baseline and takes the max asc/desc.
    row(vec![name, spacer(thin), arg])
}

/// Map an OMML accent codepoint to a SPACING display glyph the snapped-rect /
/// glyph paint path can render (combining marks won't stand alone). An unmapped
/// accent degrades to `^` rather than vanishing.
fn accent_glyph(chr: Option<&str>) -> &'static str {
    let c = chr.and_then(|s| s.chars().next()).unwrap_or('\u{0302}');
    match c as u32 {
        0x0302 => "^",
        0x0303 => "~",
        0x0304 | 0x0305 => "\u{203E}", // ‾ overline
        0x0307 => "\u{02D9}",          // ˙ dot above
        0x0308 => "\u{00A8}",          // ¨ diaeresis
        0x0300 => "`",
        0x0301 => "\u{00B4}",  // ´ acute
        0x20D7 => "\u{2192}",  // → vector-arrow proxy
        _ => "^",
    }
}

/// Accent (`m:acc`): an overlay glyph centred horizontally ABOVE the base, a
/// small gap above the base ascent. Overlay ⇒ adds NO advance width (box width =
/// base width); ascent grows by the accent height. Nested bases keep 2D.
fn layout_accent(a: &Accent, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let gap = ctx.consts.accent_gap * pt_to_px(style.font_pt);
    let base = layout_zone(&a.base, style, ctx);
    let accent = glyph_box(accent_glyph(a.character.as_deref()), &ctx.route(style), ctx);

    let base_w = base.width;
    let base_asc = base.ascent;
    let base_desc = base.descent;
    let acc_w = accent.width;
    let acc_asc = accent.ascent;
    let acc_desc = accent.descent;

    let mut out = MathBox::empty();
    out.absorb(base, 0.0, 0.0);
    out.absorb(accent, (base_w - acc_w) / 2.0, -(base_asc + gap + acc_desc));
    out.width = base_w; // overlay: no advance
    out.ascent = base_asc + gap + acc_asc + acc_desc;
    out.descent = base_desc;
    out
}

/// Bar (`m:bar`): a horizontal rule spanning the base width, above (overline,
/// default) or below (underline) the base with a small gap. Uses the existing
/// [`MathNode::Rule`] (fraction/vinculum convention). The parser does not yet
/// read `barPr/m:pos`, so `position` is `None` (overline) until the fork adds it.
fn layout_bar(b: &Bar, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let em = pt_to_px(style.font_pt);
    let base = layout_zone(&b.base, style, ctx);
    let base_w = base.width;
    let base_asc = base.ascent;
    let base_desc = base.descent;

    let mut out = MathBox::empty();
    out.absorb(base, 0.0, 0.0);
    if matches!(b.position, Some(VerticalPosition::Bottom)) {
        // underbar_* from the MATH table (proxy 0.06/0.08em).
        let thickness = (ctx.consts.underbar_thickness * em).max(1.0);
        let gap = ctx.consts.underbar_gap * em;
        out.nodes.push(MathNode::Rule { x: 0.0, y: base_desc + gap, w: base_w, thickness });
        out.ascent = base_asc;
        out.descent = base_desc + gap + thickness;
    } else {
        // overbar_* from the MATH table (proxy 0.06/0.08em).
        let thickness = (ctx.consts.overbar_thickness * em).max(1.0);
        let gap = ctx.consts.overbar_gap * em;
        let extra = ctx.consts.overbar_extra_ascender * em;
        out.nodes.push(MathNode::Rule { x: 0.0, y: -(base_asc + gap), w: base_w, thickness });
        out.ascent = base_asc + gap + thickness + extra;
        out.descent = base_desc;
    }
    out.width = base_w;
    out
}

/// Default group-char side by glyph class when `groupChrPr` has no pos (the
/// current parser): overbraces/overbrackets/over-arrows sit ABOVE, everything
/// else (underbrace default, underbracket) BELOW.
fn group_char_default_top(ch: &str) -> bool {
    matches!(
        ch.chars().next().map(|c| c as u32),
        Some(0x23DE | 0x23B4 | 0x0305 | 0x203E | 0x2190 | 0x2192 | 0x2194 | 0x20D7 | 0x00AF)
    )
}

/// Group character (`m:groupChr`): a brace (default underbrace U+23DF) stretched
/// to the group WIDTH and placed above or below it (overlay, no advance). The
/// parser reads `chr` but not pos/vertJc, so the side defaults by glyph class.
fn layout_group_char(g: &GroupChar, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let gap = 0.06 * pt_to_px(style.font_pt);
    let base = layout_zone(&g.base, style, ctx);
    let base_w = base.width;
    let base_asc = base.ascent;
    let base_desc = base.descent;

    let ch = g.character.as_deref().unwrap_or("\u{23DF}"); // ⏟ underbrace
    let top = match g.position {
        Some(VerticalPosition::Top) => true,
        Some(VerticalPosition::Bottom) => false,
        None => group_char_default_top(ch),
    };
    // Stretchy brace: MATH horizontal size variant sized to the group width with
    // a math face; the point-scale proxy without one.
    let brace = ctx.hstretch(ch, base_w, style);
    let ch_w = brace.width;
    let ch_asc = brace.ascent;
    let ch_desc = brace.descent;

    let mut out = MathBox::empty();
    out.absorb(base, 0.0, 0.0);
    if top {
        out.absorb(brace, (base_w - ch_w) / 2.0, -(base_asc + gap + ch_desc));
        out.ascent = base_asc + gap + ch_asc + ch_desc;
        out.descent = base_desc;
    } else {
        out.absorb(brace, (base_w - ch_w) / 2.0, base_desc + gap + ch_asc);
        out.ascent = base_asc;
        out.descent = base_desc + gap + ch_asc + ch_desc;
    }
    out.width = base_w; // overlay
    out
}

/// Lower limit (`m:limLow`): a smaller limit expression stacked centred BELOW the
/// base (e.g. `lim` with `x→0` under it). Reuses [`stack_over_under`].
fn layout_lower_limit(l: &Limit, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let gap = ctx.consts.lower_limit_gap * pt_to_px(style.font_pt);
    let base = layout_zone(&l.base, style, ctx);
    let lim = layout_zone(&l.limit, &scaled(style, ctx.consts.script_percent), ctx);
    stack_over_under(base, lim, StackSide::Under, gap)
}

/// Upper limit (`m:limUpp`): a smaller limit expression stacked centred ABOVE the
/// base. Reuses [`stack_over_under`].
fn layout_upper_limit(l: &Limit, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let gap = ctx.consts.upper_limit_gap * pt_to_px(style.font_pt);
    let base = layout_zone(&l.base, style, ctx);
    let lim = layout_zone(&l.limit, &scaled(style, ctx.consts.script_percent), ctx);
    stack_over_under(base, lim, StackSide::Over, gap)
}

/// Pre-script (`m:sPre`): a sub/super-script column seated to the LEFT of the
/// base, right-aligned so it hugs the base (the isotope ⁿₖX shape). Shares the
/// superscript/subscript baseline shifts (from [`MathConstantsPx`]) with the
/// trailing scripts.
fn layout_pre_script(p: &PreScript, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let em = pt_to_px(style.font_pt);
    let sup_shift = ctx.consts.superscript_shift_up * em;
    let sub_shift = ctx.consts.subscript_shift_down * em;
    let small = scaled(style, ctx.consts.script_percent);
    let sub = layout_zone(&p.subscript, &small, ctx);
    let sup = layout_zone(&p.superscript, &small, ctx);
    let body = layout_zone(&p.base, style, ctx);

    let col_w = sub.width.max(sup.width);
    let sub_w = sub.width;
    let sub_desc = sub.descent;
    let sup_w = sup.width;
    let sup_asc = sup.ascent;
    let body_w = body.width;
    let body_asc = body.ascent;
    let body_desc = body.descent;

    let mut out = MathBox::empty();
    // Right-align the column against the base (hug it), scripts shifted up/down.
    out.absorb(sup, col_w - sup_w, -sup_shift);
    out.absorb(sub, col_w - sub_w, sub_shift);
    out.absorb(body, col_w, 0.0);
    out.width = col_w + body_w;
    out.ascent = body_asc.max(sup_shift + sup_asc);
    out.descent = body_desc.max(sub_shift + sub_desc);
    out
}

/// Box (`m:box`): a transparent grouping wrapper — pure pass-through (break/align
/// hints are ignored for layout).
fn layout_box(b: &MathBoxElem, style: &RunStyle, ctx: &MathContext) -> MathBox {
    layout_zone(&b.content, style, ctx)
}

/// Border box (`m:borderBox`): the inner content plus a rectangle drawn from the
/// horizontal [`MathNode::Rule`] (top/bottom) and vertical [`MathNode::VRule`]
/// (left/right), each suppressible via the `hide_*` flags (default: all shown).
/// Diagonal strikes are a documented floor (the snapped-rect painter can't draw
/// crisp rotated lines); through-strikes await `borderBoxPr` parsing in the fork.
fn layout_border_box(b: &BorderBox, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let em = pt_to_px(style.font_pt);
    let pad = 0.15 * em;
    let thickness = (ctx.consts.overbar_thickness * em).max(1.0);
    let inner = layout_zone(&b.content, style, ctx);
    let inner_w = inner.width;
    let inner_asc = inner.ascent;
    let inner_desc = inner.descent;

    let full_w = inner_w + 2.0 * (pad + thickness);
    let asc = inner_asc + pad + thickness;
    let desc = inner_desc + pad + thickness;
    let full_h = asc + desc;

    let mut out = MathBox::empty();
    out.absorb(inner, pad + thickness, 0.0);
    if !b.hide_top {
        out.nodes.push(MathNode::Rule { x: 0.0, y: -(inner_asc + pad), w: full_w, thickness });
    }
    if !b.hide_bottom {
        out.nodes.push(MathNode::Rule { x: 0.0, y: inner_desc + pad, w: full_w, thickness });
    }
    if !b.hide_left {
        out.nodes.push(MathNode::VRule { x: 0.0, y: -asc, thickness, height: full_h });
    }
    if !b.hide_right {
        out.nodes.push(MathNode::VRule { x: full_w, y: -asc, thickness, height: full_h });
    }
    out.width = full_w;
    out.ascent = asc;
    out.descent = desc;
    out
}

/// Phantom (`m:phant`): lay the inner content for DIMENSIONS only, then DROP all
/// its primitives — an invisible spacer that reserves width/ascent/descent while
/// painting nothing. The `phantPr` zeroWid/zeroAsc/zeroDesc flags await fork
/// parsing; until then a phantom is always a full invisible box.
fn layout_phantom(p: &Phantom, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let inner = layout_zone(&p.content, style, ctx);
    MathBox {
        width: inner.width,
        ascent: inner.ascent,
        descent: inner.descent,
        nodes: Vec::new(), // render nothing
        is_display: false,
    }
}

// ─── graceful degradation for deferred constructs ────────────────────────────

/// Lay out a deferred construct as a horizontal row at base size, RECURSING
/// into real layout for its child zones (so nested armed constructs keep their
/// 2D geometry). Never vanishes: an empty result falls back to the construct's
/// plain-text glyph run. Never panics: the `MathElement` enum is matched
/// exhaustively via the catch-all.
fn linearize(el: &MathElement, style: &RunStyle, ctx: &MathContext) -> MathBox {
    let small = scaled(style, ctx.consts.script_percent);
    let routed = ctx.route(style);
    let gb = |t: &str| glyph_box(t, &routed, ctx);
    let mut parts: Vec<MathBox> = Vec::new();

    match el {
        MathElement::Radical(r) => {
            if !r.degree.elements.is_empty() {
                parts.push(layout_zone(&r.degree, &small, ctx));
            }
            parts.push(gb("\u{221A}")); // √
            parts.push(layout_zone(&r.base, style, ctx));
        }
        MathElement::Nary(n) => {
            parts.push(gb(n.operator.as_deref().unwrap_or("\u{2211}"))); // ∑
            parts.push(layout_zone(&n.subscript, &small, ctx));
            parts.push(layout_zone(&n.superscript, &small, ctx));
            parts.push(layout_zone(&n.base, style, ctx));
        }
        MathElement::Delimiter(d) => {
            parts.push(gb(d.begin_char.as_deref().unwrap_or("(")));
            let sep = d.separator_char.as_deref().unwrap_or(",");
            for (i, inner) in d.elements.iter().enumerate() {
                if i > 0 {
                    parts.push(gb(sep));
                }
                parts.push(layout_zone(inner, style, ctx));
            }
            parts.push(gb(d.end_char.as_deref().unwrap_or(")")));
        }
        MathElement::Function(f) => {
            parts.push(layout_zone(&f.name, style, ctx));
            parts.push(gb("("));
            parts.push(layout_zone(&f.argument, style, ctx));
            parts.push(gb(")"));
        }
        MathElement::PreScript(p) => {
            parts.push(layout_zone(&p.subscript, &small, ctx));
            parts.push(layout_zone(&p.superscript, &small, ctx));
            parts.push(layout_zone(&p.base, style, ctx));
        }
        MathElement::LowerLimit(l) | MathElement::UpperLimit(l) => {
            parts.push(layout_zone(&l.base, style, ctx));
            parts.push(layout_zone(&l.limit, &small, ctx));
        }
        MathElement::Accent(a) => parts.push(layout_zone(&a.base, style, ctx)),
        MathElement::Bar(b) => parts.push(layout_zone(&b.base, style, ctx)),
        MathElement::GroupChar(g) => parts.push(layout_zone(&g.base, style, ctx)),
        MathElement::Box(b) => parts.push(layout_zone(&b.content, style, ctx)),
        MathElement::BorderBox(b) => parts.push(layout_zone(&b.content, style, ctx)),
        MathElement::Phantom(p) => parts.push(layout_zone(&p.content, style, ctx)),
        MathElement::Matrix(m) => {
            for (ri, r) in m.rows.iter().enumerate() {
                if ri > 0 {
                    parts.push(gb(" "));
                }
                for (ci, cell) in r.iter().enumerate() {
                    if ci > 0 {
                        parts.push(gb(" "));
                    }
                    parts.push(layout_zone(cell, style, ctx));
                }
            }
        }
        MathElement::EquationArray(e) => {
            for (i, eq) in e.equations.iter().enumerate() {
                if i > 0 {
                    parts.push(gb(" "));
                }
                parts.push(layout_zone(eq, style, ctx));
            }
        }
        // Run / Fraction / scripts never reach here (handled by layout_element),
        // but the compiler needs the arm — degrade to text.
        _ => {}
    }

    let out = row(parts);
    if out.nodes.is_empty() {
        // Nothing rendered (e.g. every child zone was empty) — show the text.
        let text = el.text();
        if !text.is_empty() {
            return gb(&text);
        }
    }
    out
}

// ─── rendering ───────────────────────────────────────────────────────────────

/// Paint a laid-out math atom whose origin sits at `(x0, baseline)`. Reuses the
/// text glyph paint path wholesale (font + fill + shaped cluster walk) and draws
/// each fraction rule as a device-snapped `fill_rect`.
pub(crate) fn render_math_box(
    p: &dyn Painter,
    m: &MathBox,
    x0: f64,
    baseline: f64,
) -> Result<(), JsValue> {
    for node in &m.nodes {
        match node {
            MathNode::Glyph { dx, dy, style, text, shaped } => {
                p.set_font(&style.font_css());
                p.set_fill_style_str(&style.color);
                let gx = x0 + dx;
                let gy = baseline + dy;
                match shaped {
                    Some(sh) if sh.is_simple => {
                        p.paint_glyph_run(style, text, gx, gy)?;
                    }
                    Some(sh) => paint_shaped_clusters(p, style, sh, gx, gy)?,
                    None => {
                        p.paint_glyph_run(style, text, gx, gy)?;
                    }
                }
            }
            MathNode::Rule { x, y, w, thickness } => {
                p.set_fill_style_str(crate::DEFAULT_TEXT_COLOR);
                p.fill_rect(x0 + x, baseline + y - thickness / 2.0, *w, *thickness);
            }
            MathNode::VRule { x, y, thickness, height } => {
                // Centre the stroke on `x` (mirrors Rule centring `y` on its line)
                // so the vertical edge paints crisply through the same device-snap
                // Painter path as every horizontal rule and glyph.
                p.set_fill_style_str(crate::DEFAULT_TEXT_COLOR);
                p.fill_rect(x0 + x - thickness / 2.0, baseline + y, *thickness, *height);
            }
            MathNode::GlyphOutline { dx, dy, color, segs, .. } => {
                // A MATH size-variant glyph (no cmap entry) painted by replaying
                // its outline through the EXISTING Painter path vocabulary — so it
                // rides DevicePainter's offset, NullPainter's no-op, and the
                // worker/replay display list with zero new opcodes.
                render_glyph_outline(p, segs, x0 + dx, baseline + dy, color);
            }
        }
    }
    Ok(())
}

/// Replay a flattened glyph outline (device px, box-relative) offset to
/// `(ox, oy)` and fill it in `color`. This is the `fill_glyph_outline` primitive
/// realized on the existing trait path ops (no Path2D FFI, no new replay op).
fn render_glyph_outline(p: &dyn Painter, segs: &[PathSeg], ox: f64, oy: f64, color: &str) {
    p.set_fill_style_str(color);
    p.begin_path();
    for seg in segs {
        match *seg {
            PathSeg::Move(x, y) => p.move_to(ox + x, oy + y),
            PathSeg::Line(x, y) => p.line_to(ox + x, oy + y),
            PathSeg::Quad(cx, cy, x, y) => p.quadratic_curve_to(ox + cx, oy + cy, ox + x, oy + y),
            PathSeg::Cubic(c1x, c1y, c2x, c2y, x, y) => {
                p.bezier_curve_to(ox + c1x, oy + c1y, ox + c2x, oy + c2y, ox + x, oy + y)
            }
            PathSeg::Close => p.close_path(),
        }
    }
    p.fill();
}

/// Cluster-by-cluster paint for a non-simple shaped run (mirrors the text
/// branch in `render_line`), so ligatures / offsets render correctly.
fn paint_shaped_clusters(
    p: &dyn Painter,
    style: &RunStyle,
    shaped: &ShapedRun,
    x: f64,
    y: f64,
) -> Result<(), JsValue> {
    let mut gx = x;
    let mut cluster_start = 0;
    while cluster_start < shaped.glyphs.len() {
        let cluster_byte = shaped.glyphs[cluster_start].cluster as usize;
        let mut cluster_end = cluster_start + 1;
        while cluster_end < shaped.glyphs.len()
            && shaped.glyphs[cluster_end].cluster == shaped.glyphs[cluster_start].cluster
        {
            cluster_end += 1;
        }
        let next_byte = if cluster_end < shaped.glyphs.len() {
            shaped.glyphs[cluster_end].cluster as usize
        } else {
            shaped.text.len()
        };
        if cluster_byte < shaped.text.len() && next_byte <= shaped.text.len() {
            let cluster_text: String = shaped.text[cluster_byte..next_byte]
                .chars()
                .filter(|c| !crate::wml::drawing::is_layout_control(*c))
                .collect();
            if !cluster_text.is_empty() {
                let glyph = &shaped.glyphs[cluster_start];
                p.paint_glyph_run(style, &cluster_text, gx + glyph.x_offset, y - glyph.y_offset)?;
            }
        }
        for gi in cluster_start..cluster_end {
            gx += shaped.glyphs[gi].x_advance;
        }
        cluster_start = cluster_end;
    }
    Ok(())
}
