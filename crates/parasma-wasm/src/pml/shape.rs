//! Slide page model + fixed-position shape layout (PresentationML).
//!
//! A slide IS a print page (§19.3.1.38 `p:sld` → one page sized from the
//! presentation's `p:sldSz`, §19.2.1.39). This module holds the genuinely-new
//! geometry versus Word flow (§17) and the Excel grid (§18): every shape carries
//! its own `a:xfrm` offset/extent (§20.1.7.5–.6 — DrawingML), OR inherits it from
//! the matching slide-layout placeholder when the slide shape omits `a:xfrm`
//! (the fidelity floor, §19.3.1.x placeholder inheritance).
//!
//! Everything here is web-sys-free so the layout is natively unit-testable: the
//! renderer ([`super::PmlRenderer`]) feeds parsed slides + the layout placeholder
//! geometry through [`build_slide_page`], which produces a [`SlidePage`] draw
//! list; `mod.rs::render_page` paints that list through `engine::paint`.

#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashMap;

use crate::engine::fonts::{self, font_metrics, DEFAULT_FONT_FAMILY, FontCache};
use crate::engine::geom::{preset_path, PathCmd};
use crate::engine::units::{emu_to_px, pt_to_px};

use ooxml_dml::ext::{TextParagraphExt, TextRunExt};
use ooxml_pml::ext::ShapeExt;
use ooxml_pml::types::Shape;

/// The resolved colour context for a slide: the 12-slot theme `clrScheme`
/// (§20.1.6.2 — `dk1 lt1 dk2 lt2 accent1-6 hlink folHlink`, each already a
/// `#rrggbb`/`RRGGBB` hex with `dk1/lt1` sysClr resolved via `lastClr`) plus the
/// slideMaster `<p:clrMap>` (§19.3.1.6) mapping the placeholder names
/// `bg1/tx1/bg2/tx2` to scheme slots (possibly SWAPPED). A `<a:schemeClr>`
/// resolves against this: `bg1/tx1/bg2/tx2` go through `clrmap` first, every
/// other slot (and `dk1/lt1/dk2/lt2`) reads `scheme` directly. Built once per
/// presentation in `from_pptx` (single master/theme — per-slide theme override
/// is a follow-on) and threaded into the colour sites. An empty theme (parse
/// miss) yields `None` for every `schemeClr` (scheme colours then drop out).
#[derive(Debug, Clone, Default)]
pub(crate) struct SlideTheme {
    /// slot name (`dk1`/`lt1`/`accent1`/…) → `RRGGBB` (no `#`).
    pub scheme: HashMap<String, String>,
    /// placeholder name (`bg1`/`tx1`/`bg2`/`tx2`/…) → scheme slot name.
    pub clrmap: HashMap<String, String>,
    /// The theme `<a:fontScheme>` typefaces (§20.1.4.1.18). Runs whose
    /// `rPr` names a `+mj-…`/`+mn-…` theme ref (or no font at all) resolve
    /// through here.
    pub fonts: ThemeFontScheme,
}

/// The six theme fontScheme typefaces (§20.1.4.1.24/.25 majorFont/minorFont ×
/// latin/ea/cs). Each is `None` when the theme part was missing/garbled, and
/// `Some("")` when the theme writes an EMPTY typeface (PowerPoint's CJK themes
/// write `<a:ea typeface=""/>` and delegate EA to the latin face via the
/// script-supplemental table — we approximate that delegation by falling back
/// to the latin slot, see [`run_doc_east_asia`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct ThemeFontScheme {
    pub major_latin: Option<String>,
    pub minor_latin: Option<String>,
    pub major_ea: Option<String>,
    pub minor_ea: Option<String>,
    pub major_cs: Option<String>,
    pub minor_cs: Option<String>,
}

impl ThemeFontScheme {
    /// Resolve a DML theme-font reference (`+mj-lt`/`+mn-ea`/… — §20.1.4.1) to
    /// its typeface, or pass a literal name through. Returns `None` for an
    /// empty/unfilled slot (callers then apply their slot-specific fallback).
    pub(crate) fn resolve_ref(&self, name: &str) -> Option<String> {
        let mapped = match name {
            "+mj-lt" => self.major_latin.as_deref(),
            "+mn-lt" => self.minor_latin.as_deref(),
            "+mj-ea" => self.major_ea.as_deref(),
            "+mn-ea" => self.minor_ea.as_deref(),
            "+mj-cs" => self.major_cs.as_deref(),
            "+mn-cs" => self.minor_cs.as_deref(),
            other => Some(other),
        };
        mapped.filter(|s| !s.is_empty()).map(str::to_owned)
    }
}

impl SlideTheme {
    /// Resolve a scheme slot name (`dk1`/`accent1`/…) to its `#rrggbb` hex.
    fn slot_hex(&self, slot: &str) -> Option<String> {
        self.scheme.get(slot).map(|h| format!("#{}", h.to_lowercase()))
    }
}

/// Default outline width in CSS px when a shape carries an `a:ln` with a colour
/// but no explicit `@w` (§20.1.2.2.24 — the spec default is a hairline). EMU
/// widths (`STLineWidth`) convert via `emu_to_px`; a 0-width line is dropped.
pub(crate) const DEFAULT_OUTLINE_PX: f64 = 1.0;

/// Default run colour when a run sets no `solidFill` (near-black, matches the
/// SML cell-text tone). PowerPoint's true default comes from the theme/txStyles
/// chain; this constant is the honest final fallback.
const DEFAULT_RUN_COLOR: &str = "#1f2328";

/// Type-based size FINAL fallback for runs whose `rPr` carries no
/// explicit `sz` AND whose lstStyle→txStyles chain provides none: titles read
/// large, body text mid — an rPr-less run never becomes unreadable.
const TITLE_DEFAULT_PT: f64 = 44.0;
pub(crate) const BODY_DEFAULT_PT: f64 = 18.0;

/// A resolved slide background fill (§19.3.1.2 `cSld/bg/bgPr`), slide → layout
/// → master inherited: solid fills, the linear
/// gradient (`a:gradFill`, §20.1.8.33); path gradients (`<a:path>`, §20.1.8.46)
/// paint as a radial focused at the `fillToRect` centre.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SlideBg {
    /// `a:solidFill` → `#rrggbb`.
    Solid(String),
    /// `a:gradFill` → ordered colour stops + linear angle.
    Gradient {
        /// `(pos, #rrggbb)` stops sorted by `pos` ∈ 0..=1 (`a:gs@pos` / 100000).
        stops: Vec<(f64, String)>,
        /// Gradient direction in degrees, clockwise from 3-o'clock (`a:lin@ang`
        /// / 60000 — §20.1.8.41). No `a:lin` and no `a:path` → 45.0, the
        /// top-left → bottom-right default feel.
        angle_deg: f64,
    },
    /// `a:gradFill` with `<a:path>` — painted as a radial: stop 0 at the
    /// `fillToRect` focus, the last stop at the farthest page corner. The
    /// common corner-focus case (fillToRect l/t=100000 → focus bottom-right,
    /// a common authored form) reads radial in PowerPoint's own render; the
    /// previous 45°-linear approximation put the colours at OPPOSITE corners.
    Radial {
        /// `(pos, #rrggbb)` stops sorted by `pos` ∈ 0..=1.
        stops: Vec<(f64, String)>,
        /// Focus point as page fractions (x, y) ∈ 0..=1 — the centre of the
        /// `a:fillToRect` inset rectangle (missing attrs default to 0).
        focus: (f64, f64),
    },
}

/// One slide laid out as a print page: page geometry + a positioned
/// shape draw list + a positioned picture draw list (§19.3.1.x
/// `p:pic`); pictures are painted BEFORE the placeholder shapes so any shape
/// fill/text sits above the picture (the honest z-order floor — true spTree
/// interleaving of `p:pic` among `p:sp` is a follow-on, since the shape
/// list is placeholder-derived rather than the raw spTree).
#[derive(Debug, Clone)]
pub(crate) struct SlidePage {
    /// Page width in CSS px (from `p:sldSz@cx`, §19.2.1.39, EMU → px).
    pub width: f64,
    /// Page height in CSS px (from `p:sldSz@cy`, §19.2.1.39, EMU → px).
    pub height: f64,
    /// Slide background fill (solid or linear gradient), resolved from the
    /// slide → layout → master `cSld/bg/bgPr` inheritance chain (solid +
    /// gradient, §19.3.1.2). `None` = no resolvable background (e.g. a blip
    /// bg, or no `p:bg` at all) → `render_page` paints the white default.
    pub bg: Option<SlideBg>,
    /// The pictures to paint (§19.3.1.x `p:pic`), drawn first (behind shapes).
    pub pictures: Vec<SlidePicture>,
    /// The shapes to paint, in document (z) order.
    pub shapes: Vec<SlideShape>,
    /// The connection shapes to paint (§19.3.1.19 `p:cxnSp`): dashed/solid
    /// connector lines + arrowhead ends. Painted after the shapes (the z-order
    /// floor — true spTree interleaving is a follow-on), since connectors
    /// typically sit visually ON TOP of the shape rows they join.
    pub connectors: Vec<SlideConnector>,
    /// The tables to paint (DML `a:tbl` in a `p:graphicFrame`, §21.1.3). Each is
    /// a fixed-position cell grid; painted after the placeholder shapes (the
    /// z-order floor — true spTree interleaving of the graphicFrame among `p:sp`
    /// is a follow-on, mirroring the picture-before-shapes ordering floor).
    pub tables: Vec<super::table::SlideTable>,
    /// The charts to paint (DML chart part referenced from a `p:graphicFrame`,
    /// §19.3.1.16 → §21.2). Each carries the frame box (EMU → px) + the slide-
    /// namespaced chart-XML key the renderer feeds to the SHARED `crate::chart::
    /// render_chart` (the pptx chart part is the SAME `c:chartSpace` schema WML/
    /// SML render, so no PML-specific chart code). Painted with the tables (after
    /// the placeholder shapes — the z-order floor).
    pub charts: Vec<SlideChart>,
}

/// One positioned chart on a slide: the frame box (px) plus the slide-namespaced
/// chart-XML key (`chart::pml::{slideIdx}::{relId}`) the renderer stored the raw
/// chart part bytes under. `mod.rs::render_page` looks the bytes up and calls
/// `crate::chart::render_chart` at this box (the chart part is the standard DML
/// `c:chartSpace`, format-agnostic — §21.2). A chart whose frame omits a box
/// draws at (0,0)-origin sized 0 → render_chart no-ops on a zero rect.
#[derive(Debug, Clone)]
pub(crate) struct SlideChart {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Slide-namespaced chart-XML key (`chart::pml::{slideIdx}::{relId}`).
    pub chart_key: String,
}

/// One chart's raw geometry + chart-XML key, captured from the slide's
/// graphicFrame walk before layout. `chart_key` is already slide-namespaced.
#[derive(Debug, Clone)]
pub(crate) struct ChartItem {
    pub off_x: i64,
    pub off_y: i64,
    pub ext_cx: i64,
    pub ext_cy: i64,
    pub chart_key: String,
}

/// One positioned picture on a slide (`p:pic`, §19.3.1.x). Carries the pixel
/// box (from the picture's `a:xfrm` off/ext, EMU → px) plus the slide-namespaced
/// image key the renderer looks the decoded `ImageBitmap` up under. A picture
/// with no resolvable `a:xfrm` is dropped at build time (no box to draw into).
#[derive(Debug, Clone)]
pub(crate) struct SlidePicture {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Slide-namespaced image key (`pml::{slideIdx}::{relId}`) — matches the key
    /// the renderer stored the picture's media bytes under (rel ids restart per
    /// slide, so the bare rel id would collide across slides).
    pub image_key: String,
}

/// One picture's raw geometry + embed rel id, captured from the fork-parsed
/// slide before layout. `image_key` is already slide-namespaced.
#[derive(Debug, Clone)]
pub(crate) struct PictureItem {
    pub off_x: i64,
    pub off_y: i64,
    pub ext_cx: i64,
    pub ext_cy: i64,
    pub image_key: String,
}

/// A resolved shape fill. Mirrors [`SlideBg`]'s solid/gradient split
/// but is shape-scoped: the painter spans the gradient across the SHAPE box,
/// not the page. Sources, in precedence order (§20.1.8 / §19.3.1.49):
/// explicit `spPr` fill (`solidFill` → `Solid`, `gradFill` → `Gradient`,
/// `pattFill` → fg/bg 50/50-blend `Solid` — documented v1) → the style-matrix
/// `fillRef` (idx ≥ 1 → solid of the ref's phClr-substitute colour — v1, no
/// fmtScheme fill-style lookup). An explicit `noFill` yields no fill and does
/// NOT fall back to the style.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ShapeFill {
    /// `#rrggbb`.
    Solid(String),
    /// 2+-stop linear gradient: `(pos ∈ 0..=1, #rrggbb)` sorted by pos +
    /// degrees clockwise from 3-o'clock (`a:lin@ang` / 60000; absent or a
    /// path/radial gradient → 45° — same v1 approximation as [`SlideBg`]).
    Gradient {
        stops: Vec<(f64, String)>,
        angle_deg: f64,
    },
    /// A repeating preset pattern (`a:pattFill`, §20.1.8.47): the preset name
    /// (`ST_PresetPatternVal`) + resolved foreground/background `#rrggbb`. The
    /// painter rasterises a small offscreen tile (via
    /// [`crate::engine::pattern::pattern_tile`]) and `createPattern`s it; a
    /// preset outside the tier-1 set falls back to the 50/50 fg/bg blend
    /// (carried as `Solid`, so this variant is only ever a KNOWN pattern).
    Pattern {
        prst: String,
        fg: String,
        bg: String,
    },
}

/// Vertical anchoring of a shape's text block within its box (`bodyPr@anchor`,
/// §21.1.2.1.1). Absent → top (the DrawingML default); `ctr` → centred; `b` →
/// bottom. `just`/`dist` collapse to top (no vertical justification).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VAnchor {
    Top,
    Center,
    Bottom,
}

/// Text wrapping of a shape's text body (`bodyPr@wrap`, §21.1.2.1.1). `square`
/// (the default when absent) wraps text to the text box (the shape box inset by
/// the bodyPr insets); `none` lays each paragraph on a single line that may
/// overflow the box (no wrapping). Table cells always wrap (their
/// text box is the cell inset by the tcPr margins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WrapMode {
    Square,
    None,
}

/// The text-box insets (`bodyPr@lIns/tIns/rIns/bIns`, §21.1.2.1.1) in CSS px.
/// PowerPoint defaults: lIns/rIns = 91440 EMU (~0.1in), tIns/bIns = 45720 EMU
/// (~0.05in). The available text width is `box_w - left - right`; the text-box
/// origin is `(box_x + left, box_y + top)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TextInsets {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

/// PowerPoint default body insets in EMU (§21.1.2.1.1).
pub(crate) const DEFAULT_L_INS_EMU: i64 = 91_440;
pub(crate) const DEFAULT_T_INS_EMU: i64 = 45_720;
pub(crate) const DEFAULT_R_INS_EMU: i64 = 91_440;
pub(crate) const DEFAULT_B_INS_EMU: i64 = 45_720;

/// One positioned shape on a slide. Carries the pixel box (resolved from the
/// shape's own `a:xfrm`, else the inherited layout-placeholder box, else a
/// `sldSz`-derived default) plus the laid-out text lines. Fill/outline are
/// optional (painted behind the text when present).
#[derive(Debug, Clone)]
pub(crate) struct SlideShape {
    /// Box top-left x in CSS px.
    pub x: f64,
    /// Box top-left y in CSS px.
    pub y: f64,
    /// Box width in CSS px.
    pub w: f64,
    /// Box height in CSS px.
    pub h: f64,
    /// Shape fill (solid or linear gradient), resolved from the explicit
    /// `spPr` fill or the `<p:style>` fillRef.
    pub fill: Option<ShapeFill>,
    /// Outline (`#rrggbb`, width-px) stroked around the box when the shape
    /// carries an `a:ln` with a solid line fill (§20.1.2.2.24). `None` = no
    /// outline / a `noFill` line.
    pub outline: Option<ShapeOutline>,
    /// The shape's preset-geometry path (§20.1.9.18 `a:prstGeom` → tier-1
    /// `preset_path`), in SHAPE-LOCAL px (the painter translates to `(x, y)`).
    /// `None` = no `prstGeom` (placeholders without explicit geometry),
    /// an unparseable custGeom, or an unknown preset — all paint the
    /// rectangle fallback.
    pub geometry: Option<Vec<PathCmd>>,
    /// The 3-D per-face shaded interior regions (cube/can/bevel presets), in the
    /// same shape-local px space as [`Self::geometry`]. `Some` only for those
    /// presets; the painter, when this is present AND the base fill is solid,
    /// fills each face with its shade-multiplied base colour (painter's order)
    /// instead of the single flat geometry fill, then strokes the silhouette.
    /// A gradient/none base fill falls back to the flat silhouette.
    pub faces: Option<Vec<crate::engine::geom::ShadedFace>>,
    /// The preset name when a `prstGeom` was present but NOT in the tier-1
    /// builder (geometry stayed `None`): the measurable field-gap signal the
    /// painter logs under debug-probe.
    pub unknown_preset: Option<String>,
    /// Text paragraphs, each a list of styled runs. Empty for fill-only shapes.
    pub paragraphs: Vec<SlideParagraph>,
    /// Vertical anchoring of the text block within the box (`bodyPr@anchor`).
    /// Most slide shapes carry `anchor="ctr"`; the painter offsets the text
    /// stack accordingly (an always-top-anchored stack misplaces captions).
    pub anchor: VAnchor,
    /// Text wrapping (`bodyPr@wrap`, §21.1.2.1.1). `Square` (default) wraps each
    /// paragraph's runs to the inset text-box width; `None` keeps the unwrapped
    /// single-line-per-paragraph layout.
    pub wrap: WrapMode,
    /// Text-box insets (`bodyPr@lIns/tIns/rIns/bIns`) in CSS px. The wrap width
    /// is `w - insets.left - insets.right`; the text origin is offset by
    /// `(insets.left, insets.top)`.
    pub insets: TextInsets,
}

impl SlideShape {
    /// The solid fill hex when the fill is [`ShapeFill::Solid`] (`None` for a
    /// gradient/no fill) — the painter's fast path + the test pin surface.
    pub(crate) fn solid_fill_hex(&self) -> Option<&str> {
        match &self.fill {
            Some(ShapeFill::Solid(h)) => Some(h.as_str()),
            _ => None,
        }
    }
}

/// A shape outline: a colour + a stroke width in CSS px (§20.1.2.2.24) + an
/// optional dash pattern (on/off array in MULTIPLES OF THE LINE WIDTH from
/// `<a:ln>`'s `prstDash`/`custDash`, §20.1.10.48 / §20.1.8.21; `None` = solid,
/// the painter scales each entry by `width_px` before `setLineDash`). This is
/// the shared [`Edge`]; PML is the only format that populates `dash`.
pub(crate) use crate::engine::border::Edge as ShapeOutline;

/// One connection shape on a slide (§19.3.1.19 `p:cxnSp`): a positioned
/// connector line with its outline (colour + width + dash) and optional
/// arrowhead triangles at each end. The painter strokes [`path`] then fills the
/// head/tail arrowhead paths with the line colour. All geometry is SHAPE-LOCAL
/// px at the connector box (`x`,`y`); the painter translates like a shape.
#[derive(Debug, Clone)]
pub(crate) struct SlideConnector {
    /// Box top-left x/y in CSS px (the `a:xfrm` off, EMU → px).
    pub x: f64,
    pub y: f64,
    /// The connector path (straight diagonal / elbow / curved), shape-local px.
    pub path: Vec<PathCmd>,
    /// The stroke colour + width + dash (same model as a shape outline). A
    /// connector with no resolvable line colour is dropped at build time.
    pub outline: ShapeOutline,
    /// Filled arrowhead triangle at the line START (`a:headEnd`), if any.
    pub head_arrow: Option<Vec<PathCmd>>,
    /// Filled arrowhead triangle at the line END (`a:tailEnd`), if any.
    pub tail_arrow: Option<Vec<PathCmd>>,
}

/// Horizontal alignment of a slide paragraph (`pPr@algn`, §21.1.2.2.1).
/// Resolves left/centre/right/justify; `justLow`/`dist`/`thaiDist` collapse to
/// the nearest of these (justify→justify, the script-specific ones→left).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParaAlign {
    Left,
    Center,
    Right,
    Justify,
}

/// One paragraph of slide text — a sequence of styled runs, wrapped to the
/// text box per the shape's `bodyPr@wrap` ([`WrapMode`]; auto-fit is not
/// applied). It carries horizontal alignment
/// (`pPr@algn`), an optional bullet, and the first-line / continuation
/// indents.
#[derive(Debug, Clone)]
pub(crate) struct SlideParagraph {
    pub runs: Vec<SlideRun>,
    /// Horizontal alignment within the shape box (§21.1.2.2.1). Defaults to
    /// `Left` when `pPr@algn` is absent.
    pub align: ParaAlign,
    /// Bullet text to draw at the first line's start: a `buChar` glyph
    /// (§21.1.2.4.3) or a FORMATTED `buAutoNum` ordinal (§21.1.2.4.1, e.g.
    /// "1." / "a)"), resolved through the pPr → lstStyle → txStyles chain
    /// (inheritance chain). `None` = no bullet.
    pub bullet: Option<String>,
    /// First-line left offset in px from the text-box edge — `marL + indent`
    /// (§21.1.2.2.7 pPr@marL/@indent, the hanging-bullet line), floored at 0.
    /// 0.0 when the chain provides no indent (the pre-#62 layout).
    pub indent_first_px: f64,
    /// Wrapped-continuation-line left offset in px — `marL` (the text edge).
    pub indent_rest_px: f64,
}

/// One styled text run.
#[derive(Debug, Clone)]
pub(crate) struct SlideRun {
    pub text: String,
    pub font_pt: f64,
    pub bold: bool,
    pub italic: bool,
    /// Resolved (substitute-aware) font family for canvas + shaping (the
    /// latin/ascii slot, §21.1.2.3.24 / §17.3.2.26).
    pub family: String,
    /// Resolved East Asian font-slot family (`rPr/ea` → theme `+m?-ea` →
    /// theme-delegated latin → engine CJK default, §17.3.2.26). CJK
    /// segments measure AND paint with this face so measured == painted.
    pub family_east_asia: String,
    /// Resolved Complex Script font-slot family (`rPr/cs` chain, §17.3.2.26).
    pub family_complex: String,
    pub color: String,
}

/// A cumulative group-shape transform (§19.3.1.23 `p:grpSpPr` /
/// §20.1.7.5 `a:xfrm` "2D Transform for Grouped Objects")
/// that maps a descendant shape's EMU coordinates from the group's CHILD
/// coordinate space into absolute slide space. Composed multiplicatively while
/// recursing nested groups; the identity transform ([`GroupTransform::IDENTITY`])
/// is threaded for top-level shapes (no enclosing group).
///
/// A child point/extent maps as:
///   `parent_x = off.x + (child_x - chOff.x) * (ext.cx / chExt.cx)`
/// i.e. translate by `tx`/`ty` then scale by `sx`/`sy`, where this struct stores
/// the already-collapsed `(scale, translate)`. `flip_h`/`flip_v` negate the axis
/// scale about the group box centre (Tier-1 best-effort — applied to the box, so
/// preset/custGeom path scaling that derives from the box follows for free).
#[derive(Debug, Clone, Copy)]
pub(crate) struct GroupTransform {
    /// X scale = `ext.cx / chExt.cx` (signed; negative when `flip_h`).
    pub sx: f64,
    /// Y scale = `ext.cy / chExt.cy` (signed; negative when `flip_v`).
    pub sy: f64,
    /// X translate (EMU): the constant term so `parent_x = sx*child_x + tx`.
    pub tx: f64,
    /// Y translate (EMU): the constant term so `parent_y = sy*child_y + ty`.
    pub ty: f64,
}

impl GroupTransform {
    /// The no-op transform threaded for top-level (un-grouped) shapes.
    pub(crate) const IDENTITY: GroupTransform = GroupTransform {
        sx: 1.0,
        sy: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    /// Map a child-space EMU box (off/ext) into this transform's parent space.
    /// The extent is scaled by `|scale|` (a box always has positive size); a
    /// flip is reflected by translating the origin to the far edge so the box
    /// still lands correctly (the painter doesn't mirror glyphs — Tier-1).
    pub(crate) fn apply_box(&self, off_x: i64, off_y: i64, ext_cx: i64, ext_cy: i64) -> PhBox {
        let (ox, oy) = (off_x as f64, off_y as f64);
        let (cx, cy) = (ext_cx as f64, ext_cy as f64);
        // Map the two opposite corners, then normalise so width/height stay
        // positive even under a negated (flipped) scale.
        let x0 = self.sx * ox + self.tx;
        let y0 = self.sy * oy + self.ty;
        let x1 = self.sx * (ox + cx) + self.tx;
        let y1 = self.sy * (oy + cy) + self.ty;
        PhBox {
            off_x: x0.min(x1).round() as i64,
            off_y: y0.min(y1).round() as i64,
            ext_cx: (x1 - x0).abs().round() as i64,
            ext_cy: (y1 - y0).abs().round() as i64,
        }
    }

    /// Compose `self` (outer) with `inner` (a nested group's own transform):
    /// the result maps the inner group's CHILD space straight to `self`'s parent
    /// space. Transforms are affine `p' = S·p + T`, so composition multiplies
    /// the scales and threads the inner translate through the outer scale.
    pub(crate) fn compose(&self, inner: &GroupTransform) -> GroupTransform {
        GroupTransform {
            sx: self.sx * inner.sx,
            sy: self.sy * inner.sy,
            tx: self.sx * inner.tx + self.tx,
            ty: self.sy * inner.ty + self.ty,
        }
    }
}

/// Build the [`GroupTransform`] for one `p:grpSp` from its `grpSpPr/a:xfrm`
/// (§19.3.1.23 grpSpPr / §20.1.7.5 group xfrm): off/ext = the group box on the
/// parent, chOff/chExt = the child
/// coordinate space. Returns the identity when the xfrm (or its child space) is
/// missing or degenerate (zero child extent → no resolvable scale), so a
/// malformed group still flattens its children 1:1 rather than collapsing them.
pub(crate) fn group_transform(
    g: &ooxml_pml::types::GroupShape,
) -> GroupTransform {
    let Some(xfrm) = g.grp_sp_pr.transform.as_deref() else {
        return GroupTransform::IDENTITY;
    };
    let (Some(off), Some(ext), Some(ch_off), Some(ch_ext)) = (
        xfrm.offset.as_deref(),
        xfrm.extents.as_deref(),
        xfrm.child_offset.as_deref(),
        xfrm.child_extents.as_deref(),
    ) else {
        return GroupTransform::IDENTITY;
    };
    let off_x = off.x.trim().parse::<i64>().unwrap_or(0) as f64;
    let off_y = off.y.trim().parse::<i64>().unwrap_or(0) as f64;
    let ext_cx = ext.cx as f64;
    let ext_cy = ext.cy as f64;
    let ch_off_x = ch_off.x.trim().parse::<i64>().unwrap_or(0) as f64;
    let ch_off_y = ch_off.y.trim().parse::<i64>().unwrap_or(0) as f64;
    let ch_ext_cx = ch_ext.cx as f64;
    let ch_ext_cy = ch_ext.cy as f64;
    // A zero child extent gives no scale — fall back to identity (children then
    // keep their own coords, the least-wrong degenerate behaviour).
    if ch_ext_cx == 0.0 || ch_ext_cy == 0.0 {
        return GroupTransform::IDENTITY;
    }
    let mut sx = ext_cx / ch_ext_cx;
    let mut sy = ext_cy / ch_ext_cy;
    // Group-level flipH/flipV (Tier-1 best-effort): negate the axis scale.
    if xfrm.flip_h.unwrap_or(false) {
        sx = -sx;
    }
    if xfrm.flip_v.unwrap_or(false) {
        sy = -sy;
    }
    // parent = off + (child - chOff) * scale  ⇒  parent = scale*child + (off - chOff*scale)
    GroupTransform {
        sx,
        sy,
        tx: off_x - ch_off_x * sx,
        ty: off_y - ch_off_y * sy,
    }
}

/// Flatten a `p:grpSp` group tree into a document-order list of `(Shape,
/// GroupTransform)` pairs. Each descendant `p:sp` is paired with the
/// CUMULATIVE transform from the slide root to its enclosing group; nested
/// `p:grpSp` compose multiplicatively and recurse. The shape itself is borrowed
/// (not cloned) — the caller clones only when it needs an owned copy.
///
/// Tier-1 covers `p:sp` and nested `p:grpSp` (the common authored cases
/// + the synthetic nested case). Group-child `p:pic`/`p:cxnSp`/`p:graphicFrame`
///   are NOT flattened here (uncommon inside groups) — a measured field
///   gap, not a silent drop of top-level pics/connectors/tables, which keep their
///   own paths.
pub(crate) fn flatten_group_shapes<'a>(
    group: &'a ooxml_pml::types::GroupShape,
    outer: GroupTransform,
    out: &mut Vec<(&'a Shape, GroupTransform)>,
) {
    let xform = outer.compose(&group_transform(group));
    for sp in &group.shape {
        out.push((sp, xform));
    }
    for nested in &group.group_shape {
        flatten_group_shapes(nested, xform, out);
    }
}

/// A resolved placeholder geometry box from a slide layout, in EMU. Built by
/// [`layout_placeholder_geom`] from a parsed `SlideLayout` spTree.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PhBox {
    pub off_x: i64,
    pub off_y: i64,
    pub ext_cx: i64,
    pub ext_cy: i64,
}

/// The placeholder geometry of one slide layout (or slide master — the
/// master hop reuses this shape): a lookup from a placeholder's (type, idx)
/// identity to its box. Slide placeholders that omit `a:xfrm` inherit from here
/// (§19.3.1.x). Keys: `type_idx` is the exact match (e.g. `"body|1"`),
/// `by_type` is the type-only fallback, `by_idx` the idx-only.
///
/// The SAME per-placeholder lookup also carries each placeholder's
/// `txBody/lstStyle` (§21.1.2.4.12) so an `rPr`-less slide run can inherit
/// size/bold/font/bullet/indent from the matching layout placeholder — one
/// self-parse, both facets.
#[derive(Debug, Clone, Default)]
pub(crate) struct LayoutGeom {
    by_type_idx: HashMap<String, PhBox>,
    by_type: HashMap<String, PhBox>,
    by_idx: HashMap<u32, PhBox>,
    lst_by_type_idx: HashMap<String, ListStyle>,
    lst_by_type: HashMap<String, ListStyle>,
    lst_by_idx: HashMap<u32, ListStyle>,
}

impl LayoutGeom {
    /// Resolve an inherited box for a slide placeholder identified by its
    /// (type, idx). Match precedence (§19.7.10 ph matching): exact (type,idx) →
    /// type-only → idx-only. Returns `None` when nothing matches (caller falls
    /// back to a `sldSz`-derived default box so the shape never lands at 0,0).
    pub(crate) fn resolve(&self, ph_type: Option<&str>, ph_idx: Option<u32>) -> Option<PhBox> {
        if let (Some(t), Some(i)) = (ph_type, ph_idx) {
            if let Some(b) = self.by_type_idx.get(&format!("{t}|{i}")) {
                return Some(*b);
            }
        }
        if let Some(t) = ph_type {
            if let Some(b) = self.by_type.get(t) {
                return Some(*b);
            }
        }
        if let Some(i) = ph_idx {
            if let Some(b) = self.by_idx.get(&i) {
                return Some(*b);
            }
        }
        None
    }

    /// Resolve the inherited `lstStyle` for a placeholder (type, idx) — the
    /// text-property twin of [`Self::resolve`], same §19.7.10 match precedence.
    pub(crate) fn resolve_lst(
        &self,
        ph_type: Option<&str>,
        ph_idx: Option<u32>,
    ) -> Option<&ListStyle> {
        if let (Some(t), Some(i)) = (ph_type, ph_idx) {
            if let Some(l) = self.lst_by_type_idx.get(&format!("{t}|{i}")) {
                return Some(l);
            }
        }
        if let Some(t) = ph_type {
            if let Some(l) = self.lst_by_type.get(t) {
                return Some(l);
            }
        }
        if let Some(i) = ph_idx {
            if let Some(l) = self.lst_by_idx.get(&i) {
                return Some(l);
            }
        }
        None
    }

    /// Every captured placeholder `lstStyle` (for the font-plan collector — the
    /// JS side must load any typeface a chain-inherited run can paint with).
    pub(crate) fn list_styles(&self) -> impl Iterator<Item = &ListStyle> {
        self.lst_by_type_idx
            .values()
            .chain(self.lst_by_type.values())
            .chain(self.lst_by_idx.values())
    }
}

/// Build a [`LayoutGeom`] from a layout's (or master's) spTree shapes:
/// index every placeholder that carries an `a:xfrm` by its (type, idx)
/// identity, AND every placeholder's `txBody/lstStyle` (§21.1.2.4.12) by
/// the same keys. A placeholder without an `a:xfrm` still contributes its
/// `lstStyle` (text properties inherit independently of geometry). Same `Shape`
/// type as a slide's, so `ShapeExt` works directly on layout/master shapes.
pub(crate) fn layout_placeholder_geom(layout_shapes: &[Shape]) -> LayoutGeom {
    let mut geom = LayoutGeom::default();
    for sp in layout_shapes {
        if !sp.is_placeholder() {
            continue;
        }
        let ph_type = sp.placeholder_type().map(|t| t.to_string());
        let ph_idx = sp.placeholder_index();
        if let (Some((ox, oy)), Some((cx, cy))) = (sp.offset_emu(), sp.extent_emu()) {
            let b = PhBox {
                off_x: ox,
                off_y: oy,
                ext_cx: cx,
                ext_cy: cy,
            };
            if let Some(t) = &ph_type {
                geom.by_type.entry(t.clone()).or_insert(b);
                if let Some(i) = ph_idx {
                    geom.by_type_idx.insert(format!("{t}|{i}"), b);
                }
            }
            if let Some(i) = ph_idx {
                geom.by_idx.entry(i).or_insert(b);
            }
        }
        if let Some(ls) = sp
            .text_body
            .as_deref()
            .and_then(|tb| tb.lst_style.as_deref())
            .map(list_style_from)
        {
            if let Some(t) = &ph_type {
                geom.lst_by_type.entry(t.clone()).or_insert_with(|| ls.clone());
                if let Some(i) = ph_idx {
                    geom.lst_by_type_idx.insert(format!("{t}|{i}"), ls.clone());
                }
            }
            if let Some(i) = ph_idx {
                geom.lst_by_idx.entry(i).or_insert(ls);
            }
        }
    }
    geom
}

// ─── The lstStyle → txStyles text-property inheritance chain ─────────────────

/// A resolved bullet directive from a `pPr`/`lvlXpPr` (§21.1.2.4.x): an explicit
/// suppression (`buNone`, §21.1.2.4.8), a character bullet (`buChar`,
/// §21.1.2.4.3), or an auto-numbered bullet (`buAutoNum`, §21.1.2.4.1) with its
/// scheme + `@startAt`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BulletSpec {
    /// `buNone` — the author suppressed the bullet at this level.
    None,
    /// `buChar@char`, XML-entity decoded.
    Char(String),
    /// `buAutoNum@type` (the scheme token, e.g. `arabicPeriod`) + `@startAt`
    /// (spec default 1).
    AutoNum { scheme: String, start_at: i32 },
}

/// One level's inheritable text properties, extracted from a
/// `TextParagraphProperties` (a `lvlXpPr`/`defPPr` in a `lstStyle`/`txStyles`
/// part, or a paragraph's own `pPr`). Every field is `Option` — a `None` falls
/// through to the next chain layer.
#[derive(Debug, Clone, Default)]
pub(crate) struct LevelStyle {
    /// `defRPr@sz` in points (§21.1.2.3.9 — hundredths of a point in XML).
    pub sz_pt: Option<f64>,
    /// `defRPr@b`.
    pub bold: Option<bool>,
    /// `defRPr/latin@typeface` (may be a `+mj-lt`/`+mn-lt` theme ref).
    pub latin: Option<String>,
    /// `@marL` — the level's left margin (text edge) in EMU (§21.1.2.2.7).
    pub mar_l_emu: Option<i64>,
    /// `@indent` — the first-line (bullet) offset relative to `marL`, EMU
    /// (negative = hanging bullet).
    pub indent_emu: Option<i64>,
    /// The level's bullet directive.
    pub bullet: Option<BulletSpec>,
}

impl LevelStyle {
    /// Overlay `other` UNDER `self`: any field `self` leaves `None` takes
    /// `other`'s value (first-Some-wins chain merge).
    fn or(mut self, other: &LevelStyle) -> LevelStyle {
        self.sz_pt = self.sz_pt.or(other.sz_pt);
        self.bold = self.bold.or(other.bold);
        if self.latin.is_none() {
            self.latin = other.latin.clone();
        }
        self.mar_l_emu = self.mar_l_emu.or(other.mar_l_emu);
        self.indent_emu = self.indent_emu.or(other.indent_emu);
        if self.bullet.is_none() {
            self.bullet = other.bullet.clone();
        }
        self
    }

    fn is_empty(&self) -> bool {
        self.sz_pt.is_none()
            && self.bold.is_none()
            && self.latin.is_none()
            && self.mar_l_emu.is_none()
            && self.indent_emu.is_none()
            && self.bullet.is_none()
    }
}

/// A 9-level list style (`lvl1pPr`..`lvl9pPr` + `defPPr`, §21.1.2.4.12) — one
/// layer of the text-property inheritance chain (a shape's own `lstStyle`, a layout
/// placeholder's, or one of the master `txStyles` §19.3.1.52).
#[derive(Debug, Clone, Default)]
pub(crate) struct ListStyle {
    /// `defPPr` — the base every level falls through to within this layer.
    pub def: LevelStyle,
    /// `lvl1pPr`..`lvl9pPr`, indexed by 0-based level.
    pub levels: [Option<LevelStyle>; 9],
}

impl ListStyle {
    /// This layer's contribution for 0-based level `lvl`: the level entry
    /// overlaid on the layer's `defPPr`.
    fn level(&self, lvl: usize) -> LevelStyle {
        match self.levels.get(lvl).and_then(|l| l.as_ref()) {
            Some(l) => l.clone().or(&self.def),
            None => self.def.clone(),
        }
    }
}

/// Extract a [`LevelStyle`] from one `TextParagraphProperties`.
pub(crate) fn level_style_from(p: &ooxml_dml::types::TextParagraphProperties) -> LevelStyle {
    use ooxml_dml::types::EGTextBullet;
    let def = p.def_r_pr.as_deref();
    let bullet = p.text_bullet.as_deref().map(|b| match b {
        EGTextBullet::BuNone(_) => BulletSpec::None,
        EGTextBullet::BuChar(bc) => {
            BulletSpec::Char(crate::engine::vml::decode_char_refs(&bc.char))
        }
        EGTextBullet::BuAutoNum(an) => BulletSpec::AutoNum {
            scheme: an.r#type.to_string(),
            start_at: an.start_at.unwrap_or(1),
        },
        // Image bullets stay the flat round glyph (a documented floor).
        EGTextBullet::BuBlip(_) => BulletSpec::Char(DEFAULT_BODY_BULLET.to_string()),
    });
    LevelStyle {
        sz_pt: def.and_then(|d| d.sz).map(|sz| f64::from(sz) / 100.0),
        bold: def.and_then(|d| d.b),
        latin: def.and_then(|d| d.latin.as_ref()).map(|f| f.typeface.clone()),
        mar_l_emu: p.mar_l.map(i64::from),
        indent_emu: p.indent.map(i64::from),
        bullet,
    }
}

/// Build a [`ListStyle`] from a fork-parsed `CTTextListStyle`.
pub(crate) fn list_style_from(ls: &ooxml_dml::types::CTTextListStyle) -> ListStyle {
    let lvl = |p: &Option<Box<ooxml_dml::types::TextParagraphProperties>>| {
        p.as_deref().map(level_style_from).filter(|l| !l.is_empty())
    };
    ListStyle {
        def: ls.def_p_pr.as_deref().map(level_style_from).unwrap_or_default(),
        levels: [
            lvl(&ls.lvl1p_pr),
            lvl(&ls.lvl2p_pr),
            lvl(&ls.lvl3p_pr),
            lvl(&ls.lvl4p_pr),
            lvl(&ls.lvl5p_pr),
            lvl(&ls.lvl6p_pr),
            lvl(&ls.lvl7p_pr),
            lvl(&ls.lvl8p_pr),
            lvl(&ls.lvl9p_pr),
        ],
    }
}

/// The slide master's `<p:txStyles>` (§19.3.1.52): the per-role default list
/// styles rPr-less placeholder text inherits when neither the shape's own
/// `lstStyle` nor the layout placeholder's provides a value. Parsed once per
/// presentation from the SAME master self-parse the clrMap/theme scan uses.
#[derive(Debug, Clone, Default)]
pub(crate) struct MasterTextStyles {
    /// `titleStyle` (§19.3.1.49) — title-family placeholders.
    pub title: Option<ListStyle>,
    /// `bodyStyle` (§19.3.1.5) — the body/content family.
    pub body: Option<ListStyle>,
    /// `otherStyle` (§19.3.1.35) — the remaining placeholder text (dt/ftr/…).
    pub other: Option<ListStyle>,
}

impl MasterTextStyles {
    /// The master style layer for a shape: title-family → `titleStyle`;
    /// body/content placeholders (body, subTitle, obj, or an untyped
    /// content ph) → `bodyStyle`; every OTHER placeholder type (dt/ftr/
    /// sldNum/…) → `otherStyle`. Non-placeholder shapes take NO master layer
    /// (their defaults come from the presentation `defaultTextStyle`, out of
    /// scope — the type-based fallback covers them).
    fn for_placeholder(&self, ph_type: Option<&str>) -> Option<&ListStyle> {
        if is_title_ph(ph_type) {
            return self.title.as_ref();
        }
        match ph_type {
            None | Some("body") | Some("subTitle") | Some("obj") => self.body.as_ref(),
            _ => self.other.as_ref(),
        }
    }

    /// Every list style (for the font-plan collector).
    pub(crate) fn list_styles(&self) -> impl Iterator<Item = &ListStyle> {
        [self.title.as_ref(), self.body.as_ref(), self.other.as_ref()]
            .into_iter()
            .flatten()
    }
}

/// The merged text-property chain for ONE shape, layered weakest-last:
/// the shape's own `txBody/lstStyle` → the matching LAYOUT placeholder's
/// `lstStyle` → the master `txStyles` role style. Theme font refs stay
/// UNresolved here (resolved at use through [`ThemeFontScheme::resolve_ref`]);
/// the type-based size default remains the caller's FINAL fallback.
#[derive(Debug, Clone, Default)]
pub(crate) struct TextStyleChain {
    layers: Vec<ListStyle>,
}

impl TextStyleChain {
    /// The merged properties for 0-based level `lvl`: per-field first-Some
    /// across the layers (each layer already overlays its level on its defPPr).
    pub(crate) fn level(&self, lvl: usize) -> LevelStyle {
        let mut acc = LevelStyle::default();
        for layer in &self.layers {
            acc = acc.or(&layer.level(lvl));
        }
        acc
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }
}

/// Build the [`TextStyleChain`] for a slide shape: own `lstStyle` (strongest)
/// → layout placeholder `lstStyle` (§19.7.10 match) → master `txStyles` role
/// style (§19.3.1.52). Non-placeholder shapes carry only their own `lstStyle`.
pub(crate) fn resolve_text_chain(
    sp: &Shape,
    ph_type: Option<&str>,
    ph_idx: Option<u32>,
    layout: &LayoutGeom,
    master_txs: &MasterTextStyles,
) -> TextStyleChain {
    let mut layers = Vec::new();
    if let Some(own) = sp.text_body.as_deref().and_then(|tb| tb.lst_style.as_deref()) {
        layers.push(list_style_from(own));
    }
    if sp.is_placeholder() {
        if let Some(l) = layout.resolve_lst(ph_type, ph_idx) {
            layers.push(l.clone());
        }
        if let Some(m) = master_txs.for_placeholder(ph_type) {
            layers.push(m.clone());
        }
    }
    TextStyleChain { layers }
}

/// Resolve a slide placeholder's box against the MASTER geometry (the
/// third §19.7.10 tier, after the layout). The master's placeholders use the
/// base type names (`title`, `body`), so the slide's specialised types alias
/// onto them when the direct match misses: `ctrTitle → title`,
/// `subTitle → body` (§19.7.10 — the master allows only the base set).
pub(crate) fn resolve_master_ph(
    master: &LayoutGeom,
    ph_type: Option<&str>,
    ph_idx: Option<u32>,
) -> Option<PhBox> {
    if let Some(b) = master.resolve(ph_type, ph_idx) {
        return Some(b);
    }
    let alias = match ph_type {
        Some("ctrTitle") => Some("title"),
        Some("subTitle") => Some("body"),
        _ => None,
    }?;
    master.resolve(Some(alias), ph_idx)
}

/// Format an auto-numbered bullet (§21.1.2.4.1) for 1-based ordinal `n` under
/// `scheme`. The BASIC schemes render faithfully (arabic/alpha/roman ×
/// period/parenR/parenBoth/plain); every exotic scheme (CJK/thai/hindi/
/// hebrew/circle…) falls back to `arabicPeriod` — a documented floor.
pub(crate) fn format_autonum(scheme: &str, n: i32) -> String {
    // The BASIC §21.1.2.4.1 set: arabic/alpha/roman × Period/ParenR/ParenBoth
    // (+ arabicPlain). Everything else (CJK/thai/hindi/hebrew/circle…) falls
    // back to `arabicPeriod` — a documented floor.
    let known = matches!(
        scheme,
        "arabicPeriod"
            | "arabicParenR"
            | "arabicParenBoth"
            | "arabicPlain"
            | "alphaLcPeriod"
            | "alphaUcPeriod"
            | "alphaLcParenR"
            | "alphaUcParenR"
            | "alphaLcParenBoth"
            | "alphaUcParenBoth"
            | "romanLcPeriod"
            | "romanUcPeriod"
            | "romanLcParenR"
            | "romanUcParenR"
            | "romanLcParenBoth"
            | "romanUcParenBoth"
    );
    let scheme = if known { scheme } else { "arabicPeriod" };
    let n = n.max(1);
    let body = match scheme {
        s if s.starts_with("alphaLc") => int_to_alpha(n, false),
        s if s.starts_with("alphaUc") => int_to_alpha(n, true),
        s if s.starts_with("romanLc") => int_to_roman(n, false),
        s if s.starts_with("romanUc") => int_to_roman(n, true),
        _ => n.to_string(),
    };
    // Suffix family: Period → "n.", ParenR → "n)", ParenBoth → "(n)",
    // Plain → bare.
    if scheme.ends_with("ParenBoth") {
        format!("({body})")
    } else if scheme.ends_with("ParenR") {
        format!("{body})")
    } else if scheme.ends_with("Plain") {
        body
    } else {
        format!("{body}.")
    }
}

/// 1-based REPEATED-LETTER alpha numbering (§21.1.2.4.1: past 26 the letter
/// repeats — 27→aa, 28→bb, 53→aaa — NOT bijective base-26, where 28 would be
/// "ab").
fn int_to_alpha(n: i32, upper: bool) -> String {
    if n < 1 {
        return String::new();
    }
    let idx = ((n - 1) % 26) as u8;
    let reps = ((n - 1) / 26 + 1) as usize;
    let c = b'a' + idx;
    let ch = if upper { c.to_ascii_uppercase() } else { c } as char;
    std::iter::repeat_n(ch, reps).collect()
}

/// Roman numerals (1-based, standard subtractive form).
fn int_to_roman(n: i32, upper: bool) -> String {
    const PAIRS: [(i32, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut n = n.max(1);
    let mut out = String::new();
    for (v, s) in PAIRS {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    if upper {
        out.to_uppercase()
    } else {
        out
    }
}

/// True when a placeholder type renders title-sized text (drives the default
/// font size and the default-box position when no layout geometry resolves).
fn is_title_ph(ph_type: Option<&str>) -> bool {
    matches!(ph_type, Some("title") | Some("ctrTitle"))
}

/// A `sldSz`-derived default box for a placeholder that resolved to NO layout
/// geometry — so nothing collapses to (0,0). Titles get a band near the top;
/// everything else a body band below it. Insets ~7% horizontally (a common
/// PowerPoint content margin). EMU in, EMU out.
fn default_ph_box(ph_type: Option<&str>, sld_cx: i64, sld_cy: i64) -> PhBox {
    let inset_x = sld_cx / 14; // ~7%
    let ext_cx = sld_cx - 2 * inset_x;
    if is_title_ph(ph_type) {
        PhBox {
            off_x: inset_x,
            off_y: sld_cy / 15,
            ext_cx,
            ext_cy: sld_cy / 6,
        }
    } else {
        PhBox {
            off_x: inset_x,
            off_y: sld_cy / 4,
            ext_cx,
            ext_cy: sld_cy * 2 / 3,
        }
    }
}

/// Collect the positioned-picture items for a slide: each `p:pic` with a
/// resolvable `a:xfrm` off/ext and an embed rel id (§19.3.1.x) becomes a
/// [`PictureItem`] keyed by `pml::{slide_idx}::{relId}`. Pictures missing the
/// xfrm box or the embed rel id are dropped (nothing to draw). The same
/// `Picture` type as a slide layout's, so `PictureExt` works directly.
pub(crate) fn slide_picture_items(
    pictures: &[ooxml_pml::types::Picture],
    slide_idx: usize,
) -> Vec<PictureItem> {
    use ooxml_pml::ext::PictureExt;
    let mut out = Vec::new();
    for pic in pictures {
        let (Some((ox, oy)), Some((cx, cy)), Some(rel)) =
            (pic.offset_emu(), pic.extent_emu(), pic.embed_rel_id())
        else {
            continue;
        };
        out.push(PictureItem {
            off_x: ox,
            off_y: oy,
            ext_cx: cx,
            ext_cy: cy,
            image_key: image_key(slide_idx, rel),
        });
    }
    out
}

/// The slide-namespaced image key for a picture's embed rel id. Rel ids restart
/// per slide (each slide's `.rels` numbers from rId1), so the bare rel id would
/// collide across slides — namespace it with the slide index (mirrors the
/// `chart::{relId}` namespacing already used for WML charts).
pub(crate) fn image_key(slide_idx: usize, rel_id: &str) -> String {
    format!("pml::{slide_idx}::{rel_id}")
}

/// The slide-namespaced chart-XML key for a chart's `c:chart@r:id`. Mirrors the
/// `chart::{relId}` namespacing WML uses (lib.rs), extended with the slide index
/// because PML rel ids restart per slide. The renderer stores the raw chart part
/// bytes under this key in the Viewer's shared `chart_xml` map.
pub(crate) fn chart_key(slide_idx: usize, rel_id: &str) -> String {
    format!("chart::pml::{slide_idx}::{rel_id}")
}

/// Lay out one slide into a [`SlidePage`]: size the page from `sldSz` (EMU → px),
/// then for each picture resolve its pixel box and for each shape resolve its box
/// (own `a:xfrm` → inherited layout placeholder → inherited MASTER placeholder
/// (the third §19.7.10 tier) → `sldSz`-derived default) and shape its text
/// runs through `engine::fonts`. `layout`/`master_geom` supply the placeholder
/// geometry tiers; `master_txs` the master `txStyles` layer of the text chain;
/// `pictures` the per-slide picture draw items.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_slide_page(
    shapes: &[Shape],
    shape_transforms: &[GroupTransform],
    connectors: &[ooxml_pml::types::Connector],
    pictures: &[PictureItem],
    tables: &[super::table::TableItem],
    charts: &[ChartItem],
    layout: &LayoutGeom,
    master_geom: &LayoutGeom,
    master_txs: &MasterTextStyles,
    sld_cx_emu: i64,
    sld_cy_emu: i64,
    bg: Option<SlideBg>,
    theme: &SlideTheme,
    table_styles: &super::table::TableStyles,
    custgeom_paths: &[Option<String>],
    font_cache: &FontCache,
) -> SlidePage {
    let pics = pictures
        .iter()
        .map(|p| SlidePicture {
            x: emu_to_px(p.off_x),
            y: emu_to_px(p.off_y),
            w: emu_to_px(p.ext_cx),
            h: emu_to_px(p.ext_cy),
            image_key: p.image_key.clone(),
        })
        .collect();

    let slide_charts = charts
        .iter()
        .map(|c| SlideChart {
            x: emu_to_px(c.off_x),
            y: emu_to_px(c.off_y),
            w: emu_to_px(c.ext_cx),
            h: emu_to_px(c.ext_cy),
            chart_key: c.chart_key.clone(),
        })
        .collect();

    let mut out = Vec::new();
    for (sp_idx, sp) in shapes.iter().enumerate() {
        let ph_type = sp.placeholder_type().map(|t| t.to_string());
        let ph_type_ref = ph_type.as_deref();
        let ph_idx = sp.placeholder_index();
        // The cumulative group transform for this shape (identity for a
        // top-level shape). A group child's own `a:xfrm` is in the group's
        // CHILD coordinate space — map it to absolute slide space.
        let xform = shape_transforms
            .get(sp_idx)
            .copied()
            .unwrap_or(GroupTransform::IDENTITY);

        // Box resolution: own xfrm wins; else inherit from the layout; else
        // from the MASTER placeholder (§19.7.10's third tier — the
        // layout's master restates geometry the layout omits); else a
        // sldSz-derived default so the shape never lands at (0,0). A group
        // child always carries its own xfrm, mapped through `xform`; inherited /
        // default boxes only ever apply to top-level (identity) shapes.
        let bx = match (sp.offset_emu(), sp.extent_emu()) {
            (Some((ox, oy)), Some((cx, cy))) => xform.apply_box(ox, oy, cx, cy),
            _ => layout
                .resolve(ph_type_ref, ph_idx)
                .or_else(|| resolve_master_ph(master_geom, ph_type_ref, ph_idx))
                .unwrap_or_else(|| default_ph_box(ph_type_ref, sld_cx_emu, sld_cy_emu)),
        };

        let paragraphs =
            build_paragraphs(sp, ph_type_ref, ph_idx, layout, master_txs, theme, font_cache);
        let fill = shape_fill(sp, theme);
        let outline = shape_outline(sp, theme);
        let (w_px, h_px) = (emu_to_px(bx.ext_cx), emu_to_px(bx.ext_cy));
        // A raw-scanned `<a:custGeom>` for THIS spTree-ordered shape (RULE 3
        // stripped it to a rect in the parsed `sp`, so the real path lives only
        // in the original bytes). Keyed by `sp_idx` = the `Slide::shapes()` index.
        let custgeom_xml = custgeom_paths.get(sp_idx).and_then(|o| o.as_deref());
        let (geometry, unknown_preset) = shape_geometry(sp, w_px, h_px, custgeom_xml);
        let faces = shape_faces(sp, w_px, h_px, custgeom_xml);
        // Skip shapes contributing nothing: no text, no fill, no outline, AND
        // neither a geometry nor a `<p:style>` ref (a style-matrix shape always
        // renders, and a geometry-only shape is kept so a later fill source
        // can't silently vanish it).
        if paragraphs.is_empty()
            && fill.is_none()
            && outline.is_none()
            && geometry.is_none()
            && sp.style.is_none()
        {
            continue;
        }

        out.push(SlideShape {
            x: emu_to_px(bx.off_x),
            y: emu_to_px(bx.off_y),
            w: w_px,
            h: h_px,
            fill,
            outline,
            geometry,
            faces,
            unknown_preset,
            paragraphs,
            anchor: text_anchor(sp),
            wrap: text_wrap(sp),
            insets: text_insets(sp),
        });
    }

    let slide_tables = tables
        .iter()
        .map(|t| super::table::build_slide_table(t, table_styles, theme, font_cache))
        .collect();

    let slide_connectors = connectors
        .iter()
        .filter_map(|c| build_slide_connector(c, theme))
        .collect();

    SlidePage {
        width: emu_to_px(sld_cx_emu),
        height: emu_to_px(sld_cy_emu),
        bg,
        pictures: pics,
        shapes: out,
        connectors: slide_connectors,
        tables: slide_tables,
        charts: slide_charts,
    }
}

/// Build one [`SlideConnector`] from a parsed `p:cxnSp` (§19.3.1.19). Reads
/// the connector's `a:xfrm` box (off/ext/flipH/flipV), its `prstGeom` preset
/// (`line`/`straightConnector1`/`bentConnector*`/`curvedConnector*`), its `a:ln`
/// outline (colour → explicit solid → `<p:style>` lnRef, width, dash), and
/// its `a:headEnd`/`a:tailEnd` arrowheads. Returns `None` when no line colour
/// resolves (nothing to stroke) or the box has zero extent (no segment).
fn build_slide_connector(
    c: &ooxml_pml::types::Connector,
    theme: &SlideTheme,
) -> Option<SlideConnector> {
    use ooxml_dml::ext::ShapePropertiesExt;
    use ooxml_dml::types::{EGGeometry, EGLineFillProperties as LF};

    let spr = c.shape_properties.as_ref();
    let (ox, oy) = spr.offset_emu()?;
    let (cx, cy) = spr.extent_emu()?;
    let (w_px, h_px) = (emu_to_px(cx), emu_to_px(cy));
    // A zero-area box is still a valid connector IF one axis is non-zero (a
    // horizontal/vertical line has cy=0 / cx=0). Reject only the truly empty.
    if w_px <= 0.0 && h_px <= 0.0 {
        return None;
    }

    let (flip_h, flip_v) = spr
        .transform
        .as_deref()
        .map(|t| (t.flip_h.unwrap_or(false), t.flip_v.unwrap_or(false)))
        .unwrap_or((false, false));

    let preset = match spr.geometry.as_deref() {
        Some(EGGeometry::PrstGeom(pg)) => pg.preset.to_string(),
        _ => "line".to_string(),
    };
    let geom = crate::engine::geom::connector_path(&preset, w_px, h_px, flip_h, flip_v);

    // Outline: explicit solid line fill wins; else the style lnRef colour.
    // No resolvable colour → no connector (a connector is its line).
    let ln = spr.line.as_deref();
    if let Some(l) = ln {
        if matches!(l.line_fill_properties.as_deref(), Some(LF::NoFill(_))) {
            return None;
        }
    }
    let explicit = ln.and_then(|l| solid_line_fill_hex(l.line_fill_properties.as_deref(), theme));
    let color = explicit.or_else(|| connector_style_ln_color(c, theme))?;
    let width_px = ln
        .and_then(|l| l.width)
        .map(|w| emu_to_px(w as i64))
        .filter(|w| *w > 0.0)
        .unwrap_or(DEFAULT_OUTLINE_PX)
        .max(DEFAULT_OUTLINE_PX);
    let dash = ln.and_then(|l| line_dash(l.line_dash_properties.as_deref()));
    let outline = ShapeOutline { color, width_px, dash };

    // Arrowheads (§20.1.8.30/.43): headEnd at the line START, tailEnd at the END.
    // The triangle size scales with the pen width and the sm/med/lg @w/@len
    // hints; `none`/absent → no head.
    let head_arrow = ln
        .and_then(|l| l.head_end.as_deref())
        .and_then(|e| arrowhead_for(e, geom.head, width_px));
    let tail_arrow = ln
        .and_then(|l| l.tail_end.as_deref())
        .and_then(|e| arrowhead_for(e, geom.tail, width_px));

    Some(SlideConnector {
        x: emu_to_px(ox),
        y: emu_to_px(oy),
        path: geom.path,
        outline,
        head_arrow,
        tail_arrow,
    })
}

/// The style-matrix line colour for a CONNECTOR (the `Connector`-typed twin of
/// [`style_ln_color`] — `Connector.style` is the same `ShapeStyle`, but it is a
/// distinct type so the shape helper can't take it). idx 0 → no style colour.
fn connector_style_ln_color(
    c: &ooxml_pml::types::Connector,
    theme: &SlideTheme,
) -> Option<String> {
    let style = c.style.as_deref()?;
    if style.ln_ref.idx == 0 {
        return None;
    }
    color_choice_to_hex(style.ln_ref.color_choice.as_deref(), theme).map(|(hex, _alpha)| hex)
}

/// Build the filled arrowhead triangle for one line end (`a:headEnd`/`a:tailEnd`)
/// at `(tip, dir)` (the connector endpoint + its outward unit direction).
/// `type="none"` (or absent type) → `None`. All other types
/// (triangle/arrow/stealth/diamond/oval) render as a filled triangle (v1, see
/// [`crate::engine::geom::arrowhead_path`]). The triangle is sized from the pen
/// width × the sm/med/lg `@w`/`@len` multipliers (PowerPoint's defaults are ~3–5
/// pen widths for a med head).
fn arrowhead_for(
    e: &ooxml_dml::types::CTLineEndProperties,
    endpoint: ((f64, f64), (f64, f64)),
    width_px: f64,
) -> Option<Vec<PathCmd>> {
    use ooxml_dml::types::{STLineEndLength as L, STLineEndType as T, STLineEndWidth as W};
    match e.r#type {
        None | Some(T::None) => return None,
        _ => {}
    }
    // sm/med/lg → small multipliers on the pen width (med is the spec default).
    let wmul = match e.width {
        Some(W::Sm) => 3.0,
        Some(W::Lg) => 6.0,
        _ => 4.5,
    };
    let lmul = match e.len {
        Some(L::Sm) => 3.0,
        Some(L::Lg) => 6.0,
        _ => 4.5,
    };
    // Floor the pen width so a hairline still yields a visible head.
    let pen = width_px.max(1.0);
    let (tip, dir) = endpoint;
    Some(crate::engine::geom::arrowhead_path(tip, dir, pen * lmul, pen * wmul))
}

/// Build the styled paragraph list for a shape's text body. Each `a:p` becomes
/// one [`SlideParagraph`]; each `a:r` one [`SlideRun`]. An `rPr`-less run's
/// size/bold/font now resolve through the inheritance chain (own `lstStyle` → layout
/// placeholder `lstStyle` → master `txStyles`, per the paragraph's `pPr@lvl`);
/// the type-based size default remains the FINAL fallback so an rPr-less run
/// never becomes unreadable.
fn build_paragraphs(
    sp: &Shape,
    ph_type: Option<&str>,
    ph_idx: Option<u32>,
    layout: &LayoutGeom,
    master_txs: &MasterTextStyles,
    theme: &SlideTheme,
    font_cache: &FontCache,
) -> Vec<SlideParagraph> {
    let default_pt = if is_title_ph(ph_type) {
        TITLE_DEFAULT_PT
    } else {
        BODY_DEFAULT_PT
    };
    // A body-placeholder paragraph whose CHAIN provides no bullet keeps the
    // flat '•' default (the final fallback). Title placeholders never take the
    // default bullet.
    let is_body_ph = is_body_placeholder(ph_type, sp.is_placeholder());
    let chain = resolve_text_chain(sp, ph_type, ph_idx, layout, master_txs);
    build_paragraphs_from_inner(
        sp.paragraphs(),
        default_pt,
        is_body_ph,
        None,
        &chain,
        theme,
        font_cache,
    )
}

/// True when a shape is a placeholder whose paragraphs default to a bullet: a
/// body / subTitle / object / generic-content placeholder (NOT a title /
/// ctrTitle, which is never bulleted). A placeholder with no `ph@type` is a
/// generic content body in the default layout, so it bullets too. Non-
/// placeholder shapes (free text boxes, rects) do NOT get a default bullet —
/// only their explicit `buChar` (matching the existing behaviour).
fn is_body_placeholder(ph_type: Option<&str>, is_ph: bool) -> bool {
    if !is_ph {
        return false;
    }
    !matches!(ph_type, Some("title") | Some("ctrTitle"))
}

/// Build the styled paragraph list from a slice of `a:p` paragraphs (the shared
/// core of [`build_paragraphs`]). Reused by the table painter ([`super::table`])
/// over a cell's `txBody` paragraphs — a table cell's text is the SAME DML
/// `TextBody`, so the full slide text painter (runs, sizes, bold/italic, colour,
/// alignment, char bullets) applies unchanged. `default_pt` is the size a run
/// with no explicit `rPr/sz` takes (body-tier for cells).
///
/// Table styles (§21.1.3): a run with NO explicit `rPr` colour / bold inherits
/// the resolved table-style cell `defaults` (`tcTxStyle` colour + `b="on"`)
/// instead of the bare `DEFAULT_RUN_COLOR`/non-bold. An EXPLICIT run colour/bold
/// still wins (the override precedence — explicit cell text keeps its own
/// styling). `defaults` is `None` for an unstyled table.
pub(crate) fn build_paragraphs_from_styled(
    paragraphs: &[ooxml_dml::types::TextParagraph],
    default_pt: f64,
    defaults: Option<&CellTextDefaults>,
    theme: &SlideTheme,
    font_cache: &FontCache,
) -> Vec<SlideParagraph> {
    // Table cells have no placeholder chain (§21.1.3 cell text inherits from
    // table styles, threaded via `defaults`) — an EMPTY chain reproduces the
    // pre-#62 behaviour exactly.
    build_paragraphs_from_inner(
        paragraphs,
        default_pt,
        false,
        defaults,
        &TextStyleChain::default(),
        theme,
        font_cache,
    )
}

/// Resolved table-style text defaults for a cell (§21.1.3 `tcTxStyle`): the
/// `#rrggbb` colour and bold flag a run inherits when its `rPr` sets neither.
#[derive(Debug, Clone, Default)]
pub(crate) struct CellTextDefaults {
    /// Resolved `tcTxStyle` colour (`schemeClr`/`srgbClr` → `#rrggbb`), or `None`
    /// when the part set no colour (then the bare `DEFAULT_RUN_COLOR` applies).
    pub color: Option<String>,
    /// `tcTxStyle@b="on"` → a run with no explicit `rPr@b` renders bold.
    pub bold: bool,
}

/// The shared paragraph builder threading `is_body_ph` (whether a bullet-less
/// paragraph should take the default body bullet) and the `chain` (empty
/// for table cells / chain-less callers — then every chain lookup is `None`
/// and the pre-#62 behaviour holds). Slide body placeholders pass `true`;
/// titles, free text boxes, and table cells pass `false`.
fn build_paragraphs_from_inner(
    paragraphs: &[ooxml_dml::types::TextParagraph],
    default_pt: f64,
    is_body_ph: bool,
    cell_defaults: Option<&CellTextDefaults>,
    chain: &TextStyleChain,
    theme: &SlideTheme,
    font_cache: &FontCache,
) -> Vec<SlideParagraph> {
    let mut paras = Vec::new();
    // buAutoNum counters, keyed by 0-based level — numbering restarts per text
    // body (this call), `@startAt` seeds the first occurrence (§21.1.2.4.1).
    let mut autonum: HashMap<usize, i32> = HashMap::new();
    for p in paragraphs {
        // The paragraph's 0-based outline level (`pPr@lvl`, §21.1.2.2.7;
        // absent → 0) selects the chain level to merge.
        let lvl = p
            .p_pr
            .as_ref()
            .and_then(|pr| pr.lvl)
            .unwrap_or(0)
            .clamp(0, 8) as usize;
        let ls = chain.level(lvl);
        // The chain's latin typeface (theme refs resolved) for rPr-less runs.
        let chain_latin = ls
            .latin
            .as_deref()
            .and_then(|n| theme.fonts.resolve_ref(n))
            .filter(|s| !s.is_empty());
        let mut runs = Vec::new();
        for r in p.runs() {
            let text = r.text();
            if text.is_empty() {
                continue;
            }
            // font_size() is hundredths of a point (§ DML sz). Explicit rPr →
            // the chain level size → the type-based FINAL fallback.
            let font_pt = r
                .font_size()
                .map(|sz| sz as f64 / 100.0)
                .or(ls.sz_pt)
                .unwrap_or(default_pt);
            // Document-level family per font slot (theme refs resolved,
            // chain typeface for rPr-less runs, theme minor fallback),
            // then the substitute chain (font-map aliases + loaded-face
            // check) — mirrors WML's from_resolved.
            let family = fonts::resolve_font_with_fallbacks(
                &run_doc_latin_styled(r, theme, chain_latin.as_deref()),
                font_cache,
            );
            let family_east_asia =
                fonts::resolve_font_with_fallbacks(&run_doc_east_asia(r, theme), font_cache);
            let family_complex =
                fonts::resolve_font_with_fallbacks(&run_doc_complex(r, theme), font_cache);
            // Bold precedence: explicit `rPr@b` → table-style cell default →
            // the chain level `defRPr@b` → non-bold. Same for colour —
            // `run_color` is `None` when the run has no `solidFill`, so the
            // cell style's `tcTxStyle` colour fills in (else the bare default;
            // the chain carries no colour).
            let bold = run_explicit_bold(r)
                .or_else(|| cell_defaults.map(|d| d.bold))
                .or(ls.bold)
                .unwrap_or(false);
            let color = run_color(r, theme)
                .or_else(|| cell_defaults.and_then(|d| d.color.clone()))
                .unwrap_or_else(|| DEFAULT_RUN_COLOR.to_string());
            runs.push(SlideRun {
                text: text.to_string(),
                font_pt,
                bold,
                italic: run_is_italic(r),
                family,
                family_east_asia,
                family_complex,
                color,
            });
        }
        if !runs.is_empty() {
            // Indent: explicit pPr@marL/@indent → the chain level →
            // none (0). `marL` positions the text edge (continuation lines);
            // `marL + indent` positions the first (bullet) line — the
            // hanging-bullet layout (§21.1.2.2.7).
            let mar_l = p
                .p_pr
                .as_ref()
                .and_then(|pr| pr.mar_l)
                .map(i64::from)
                .or(ls.mar_l_emu)
                .unwrap_or(0);
            let indent = p
                .p_pr
                .as_ref()
                .and_then(|pr| pr.indent)
                .map(i64::from)
                .or(ls.indent_emu)
                .unwrap_or(0);
            // autoNum restart (§21.1.2.4.1): returning to a
            // shallower level restarts every DEEPER level's counter — a second
            // run of sub-items numbers a. b. again, not c. d.
            autonum.retain(|&k, _| k <= lvl);
            paras.push(SlideParagraph {
                runs,
                align: paragraph_align(p),
                bullet: paragraph_bullet(p, is_body_ph, &ls, lvl, &mut autonum),
                indent_first_px: emu_to_px(mar_l + indent).max(0.0),
                indent_rest_px: emu_to_px(mar_l).max(0.0),
            });
        }
    }
    paras
}

/// The paragraph's horizontal alignment (`pPr@algn`, §21.1.2.2.1) mapped to
/// [`ParaAlign`]. Absent → `Left` (the PowerPoint default for explicit text).
/// `justLow` collapses to `Justify`; `dist`/`thaiDist` (script-specific
/// distribution) collapse to `Left` (honest — true distribution deferred).
fn paragraph_align(p: &ooxml_dml::types::TextParagraph) -> ParaAlign {
    use ooxml_dml::types::STTextAlignType;
    match p.alignment() {
        Some(STTextAlignType::Ctr) => ParaAlign::Center,
        Some(STTextAlignType::R) => ParaAlign::Right,
        Some(STTextAlignType::Just) | Some(STTextAlignType::JustLow) => ParaAlign::Justify,
        _ => ParaAlign::Left,
    }
}

/// The default body bullet glyph applied to a body-placeholder paragraph when
/// NEITHER its explicit `pPr` NOR the lstStyle→txStyles chain provides a
/// bullet (the flat '•' FINAL fallback).
const DEFAULT_BODY_BULLET: &str = "\u{2022}";

/// The paragraph's bullet text (§21.1.2.4.x), resolved through the inheritance chain:
///  - the explicit `pPr` bullet wins (`buChar` → glyph, XML-entity decoded;
///    `buNone` → suppressed; `buAutoNum` → the formatted ordinal, basic
///    schemes; `buBlip` → the flat '•' stand-in);
///  - else the CHAIN level's bullet (`lvlXpPr` from the shape's `lstStyle` →
///    the layout placeholder's → the master `txStyles`), same semantics;
///  - else the flat default '•' WHEN the owning shape is a body placeholder
///    (the pre-chain floor, kept as the FINAL fallback), else `None`.
///
/// `buAutoNum` (§21.1.2.4.1) takes the next ordinal from `counters[lvl]`
/// (seeded by `@startAt`, numbering restarts per text body) and renders it via
/// [`format_autonum`].
///
/// The fork parser keeps attribute values with their XML entities UNDECODED (the
/// same #59-family risk SML hit on `numFmt@formatCode`), so a `char="&#8226;"`
/// arrives literally as the 8-char string. We decode numeric/`amp`/`lt`/`gt`/
/// `quot`/`apos` entities so the painted glyph is the real bullet.
fn paragraph_bullet(
    p: &ooxml_dml::types::TextParagraph,
    is_body_ph: bool,
    ls: &LevelStyle,
    lvl: usize,
    counters: &mut HashMap<usize, i32>,
) -> Option<String> {
    use ooxml_dml::types::EGTextBullet;
    // Explicit pPr bullet → BulletSpec (the same mapping level_style_from uses).
    let explicit = p
        .p_pr
        .as_ref()
        .and_then(|pr| pr.text_bullet.as_deref())
        .map(|b| match b {
            EGTextBullet::BuNone(_) => BulletSpec::None,
            EGTextBullet::BuChar(bc) => {
                BulletSpec::Char(crate::engine::vml::decode_char_refs(&bc.char))
            }
            EGTextBullet::BuAutoNum(an) => BulletSpec::AutoNum {
                scheme: an.r#type.to_string(),
                start_at: an.start_at.unwrap_or(1),
            },
            EGTextBullet::BuBlip(_) => BulletSpec::Char(DEFAULT_BODY_BULLET.to_string()),
        });
    let spec = explicit.or_else(|| ls.bullet.clone());
    match spec {
        // The author (or the chain) suppressed the bullet.
        Some(BulletSpec::None) => None,
        Some(BulletSpec::Char(c)) => Some(c),
        Some(BulletSpec::AutoNum { scheme, start_at }) => {
            let n = *counters.entry(lvl).or_insert(start_at);
            counters.insert(lvl, n + 1);
            Some(format_autonum(&scheme, n))
        }
        // No bullet anywhere in the chain: body placeholders keep the default
        // round bullet; titles / free text boxes / table cells do not.
        None => is_body_ph.then(|| DEFAULT_BODY_BULLET.to_string()),
    }
}

/// The run's EXPLICIT bold flag (`rPr@b`), or `None` when the run sets no `b`.
/// PowerPoint always writes an explicit `b="1"`/`b="0"` value so the val-less-
/// toggle risk that bites SML does not apply here. The `None` case lets the
/// table-style cell default fill in (an absent `b` inherits the `tcTxStyle`
/// bold); a present `b` always wins (the explicit-text override precedence).
fn run_explicit_bold(r: &ooxml_dml::types::TextRun) -> Option<bool> {
    r.r_pr.as_ref().and_then(|pr| pr.b)
}

fn run_is_italic(r: &ooxml_dml::types::TextRun) -> bool {
    r.is_italic()
}

/// The run's latin typeface (`rPr/latin@typeface`, §21.1.2.3.24) — the font
/// family slot used for Latin script. May be a `+mj-lt`/`+mn-lt` theme ref or
/// an empty string; callers resolve via [`ThemeFontScheme::resolve_ref`].
fn run_latin_family(r: &ooxml_dml::types::TextRun) -> Option<String> {
    r.r_pr
        .as_ref()
        .and_then(|p| p.latin.as_ref())
        .map(|f| f.typeface.clone())
}

/// The run's East Asian typeface (`rPr/ea@typeface`, §21.1.2.3.3) — the EA twin
/// of [`run_latin_family`].
fn run_ea_family(r: &ooxml_dml::types::TextRun) -> Option<String> {
    r.r_pr
        .as_ref()
        .and_then(|p| p.ea.as_ref())
        .map(|f| f.typeface.clone())
}

/// The run's Complex Script typeface (`rPr/cs@typeface`, §21.1.2.3.1).
fn run_cs_family(r: &ooxml_dml::types::TextRun) -> Option<String> {
    r.r_pr
        .as_ref()
        .and_then(|p| p.cs.as_ref())
        .map(|f| f.typeface.clone())
}

/// The run's DOCUMENT-level latin family: explicit `rPr/latin` (theme
/// `+mj-lt`/`+mn-lt` refs resolved) → the theme MINOR latin → the engine
/// default. Chain-aware callers (the paragraph builder) use
/// [`run_doc_latin_styled`] to slot the lstStyle/txStyles typeface between the
/// explicit and minor tiers. This is the name the JS font plan sees (e.g.
/// `等线`), BEFORE the FontCache substitute chain.
pub(crate) fn run_doc_latin(r: &ooxml_dml::types::TextRun, theme: &SlideTheme) -> String {
    run_doc_latin_styled(r, theme, None)
}

/// [`run_doc_latin`] with the inheritance-chain typeface: explicit `rPr/latin` (theme
/// refs resolved) → the chain's `defRPr/latin` (`style_latin`, already
/// theme-resolved by the caller) → the theme MINOR latin → the engine default.
pub(crate) fn run_doc_latin_styled(
    r: &ooxml_dml::types::TextRun,
    theme: &SlideTheme,
    style_latin: Option<&str>,
) -> String {
    run_latin_family(r)
        .and_then(|n| theme.fonts.resolve_ref(&n))
        .or_else(|| style_latin.map(str::to_owned))
        .or_else(|| theme.fonts.minor_latin.clone().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| DEFAULT_FONT_FAMILY.to_string())
}

/// The run's DOCUMENT-level East Asian family (§17.3.2.26 ea slot):
/// explicit `rPr/ea` (theme refs resolved) → theme minor ea → fallbacks. An
/// EMPTY ea typeface (`<a:ea typeface=""/>` — how PowerPoint's CJK themes
/// delegate EA to the script-supplemental table) falls back to the LATIN slot
/// family (which in those themes IS the CJK-capable face, e.g. 等线). Only a
/// deck with NO theme ea slot at all falls to the engine CJK default
/// (mirroring WML's `DEFAULT_EAST_ASIA_FAMILY` fallback).
pub(crate) fn run_doc_east_asia(r: &ooxml_dml::types::TextRun, theme: &SlideTheme) -> String {
    if let Some(name) = run_ea_family(r) {
        if let Some(resolved) = theme.fonts.resolve_ref(&name) {
            return resolved;
        }
        // Explicit ea present but empty (or a theme ref whose slot is empty):
        // the theme delegates EA to the latin face.
        return run_doc_latin(r, theme);
    }
    match &theme.fonts.minor_ea {
        Some(ea) if !ea.is_empty() => ea.clone(),
        Some(_) => run_doc_latin(r, theme),
        None => fonts::DEFAULT_EAST_ASIA_FAMILY.to_string(),
    }
}

/// The run's DOCUMENT-level Complex Script family (§17.3.2.26 cs slot) — the
/// cs twin of [`run_doc_east_asia`].
pub(crate) fn run_doc_complex(r: &ooxml_dml::types::TextRun, theme: &SlideTheme) -> String {
    if let Some(name) = run_cs_family(r) {
        if let Some(resolved) = theme.fonts.resolve_ref(&name) {
            return resolved;
        }
        return run_doc_latin(r, theme);
    }
    match &theme.fonts.minor_cs {
        Some(cs) if !cs.is_empty() => cs.clone(),
        Some(_) => run_doc_latin(r, theme),
        None => fonts::DEFAULT_COMPLEX_FAMILY.to_string(),
    }
}

/// The run's explicit latin name resolved against the theme (`None` when the
/// run sets no latin / it resolves empty) — the `font_families()` collector's
/// per-run latin emission (theme major/minor are seeded separately).
pub(crate) fn run_explicit_latin(
    r: &ooxml_dml::types::TextRun,
    theme: &SlideTheme,
) -> Option<String> {
    run_latin_family(r).and_then(|n| theme.fonts.resolve_ref(&n))
}

/// The run's explicit `solidFill` colour as `#rrggbb`, if present. Reads
/// `rPr/fill_properties → solidFill → {srgbClr|schemeClr|sysClr}` — this resolves
/// `<a:schemeClr>` against the slide theme (clrMap + scheme + DML transforms) in
/// addition to the explicit `srgbClr`.
fn run_color(r: &ooxml_dml::types::TextRun, theme: &SlideTheme) -> Option<String> {
    let rpr = r.r_pr.as_ref()?;
    solid_fill_hex(rpr.fill_properties.as_deref(), theme)
}

// Shape fill resolution (`shape_fill`, below): the explicit `spPr` fill wins
// (`solidFill` → solid, `gradFill` → gradient, `pattFill` → tile or blend —
// documented v1; `noFill` → none, NO style fallback per §20.1.8 explicit-
// beats-inherited); a shape with NO explicit fill falls back to its
// `<p:style>` fillRef (§19.3.1.49 → §20.1.4.2.10): idx ≥ 1 → a solid of the
// ref's colour child (the phClr substitute, transforms applied) — the v1
// approximation of the theme fmtScheme fill-style lookup. blip (picture) and
// group fills stay unrendered.
/// A shape's vertical text anchor (`txBody/bodyPr@anchor`, §21.1.2.1.1).
/// Mirrors the table cell's [`super::table::cell_anchor`]; absent/`t`/`just`/
/// `dist` → top, `ctr` → centre, `b` → bottom.
fn text_anchor(sp: &Shape) -> VAnchor {
    use ooxml_dml::types::STTextAnchoringType;
    match sp.text_body.as_deref().and_then(|tb| tb.body_pr.anchor) {
        Some(STTextAnchoringType::Ctr) => VAnchor::Center,
        Some(STTextAnchoringType::B) => VAnchor::Bottom,
        _ => VAnchor::Top,
    }
}

/// A shape's text wrapping (`txBody/bodyPr@wrap`, §21.1.2.1.1). Absent → `square`
/// (the DrawingML default — text wraps to the text box); `none` → no wrap.
fn text_wrap(sp: &Shape) -> WrapMode {
    use ooxml_dml::types::STTextWrappingType;
    match sp.text_body.as_deref().and_then(|tb| tb.body_pr.wrap) {
        Some(STTextWrappingType::None) => WrapMode::None,
        _ => WrapMode::Square,
    }
}

/// A shape's text-box insets (`txBody/bodyPr@lIns/tIns/rIns/bIns`, §21.1.2.1.1),
/// EMU → px, with the PowerPoint defaults when an attribute is absent. The insets
/// are `STCoordinate32` (String EMU); a non-numeric value falls back to the
/// default for that edge.
fn text_insets(sp: &Shape) -> TextInsets {
    let bp = sp.text_body.as_deref().map(|tb| &tb.body_pr);
    let coord = |v: &Option<String>, dflt: i64| -> f64 {
        emu_to_px(v.as_deref().and_then(|s| s.trim().parse::<i64>().ok()).unwrap_or(dflt))
    };
    match bp {
        Some(bp) => TextInsets {
            left: coord(&bp.l_ins, DEFAULT_L_INS_EMU),
            top: coord(&bp.t_ins, DEFAULT_T_INS_EMU),
            right: coord(&bp.r_ins, DEFAULT_R_INS_EMU),
            bottom: coord(&bp.b_ins, DEFAULT_B_INS_EMU),
        },
        None => TextInsets {
            left: emu_to_px(DEFAULT_L_INS_EMU),
            top: emu_to_px(DEFAULT_T_INS_EMU),
            right: emu_to_px(DEFAULT_R_INS_EMU),
            bottom: emu_to_px(DEFAULT_B_INS_EMU),
        },
    }
}

fn shape_fill(sp: &Shape, theme: &SlideTheme) -> Option<ShapeFill> {
    use ooxml_dml::types::EGFillProperties as F;
    match sp.shape_properties.fill_properties.as_deref() {
        Some(F::NoFill(_)) => None,
        Some(F::SolidFill(sf)) => color_choice_to_hex(sf.color_choice.as_deref(), theme)
            .map(|(hex, _alpha)| ShapeFill::Solid(hex)),
        Some(F::GradFill(gf)) => gradient_shape_fill(gf, theme),
        Some(F::PattFill(pf)) => pattern_shape_fill(pf, theme),
        Some(F::BlipFill(_)) | Some(F::GrpFill(_)) => None,
        None => style_fill_ref(sp, theme),
    }
}

/// The style-matrix fill: `<p:style>/<a:fillRef>`. `idx="0"` means "no
/// fill from the style"; any other idx (fill or bg-fill matrix column) paints
/// a solid of the ref's colour child — v1, no fmtScheme gradient lookup.
fn style_fill_ref(sp: &Shape, theme: &SlideTheme) -> Option<ShapeFill> {
    let style = sp.style.as_deref()?;
    if style.fill_ref.idx == 0 {
        return None;
    }
    color_choice_to_hex(style.fill_ref.color_choice.as_deref(), theme)
        .map(|(hex, _alpha)| ShapeFill::Solid(hex))
}

/// An explicit `spPr/gradFill` (§20.1.8.33) on a SHAPE: every `gsLst/gs`
/// stop (`@pos` is a DML positive-fixed-percentage — thousandths or `%`) with
/// a resolvable colour, sorted by pos; the `a:lin@ang` angle (60000ths of a
/// degree clockwise from 3-o'clock), 45° when absent or a path/radial
/// gradient. One stop degrades to a solid; zero resolvable stops → `None`.
fn gradient_shape_fill(
    gf: &ooxml_dml::types::GradientFill,
    theme: &SlideTheme,
) -> Option<ShapeFill> {
    use crate::engine::color::parse_dml_pct;
    use ooxml_dml::types::EGShadeProperties;
    let mut stops: Vec<(f64, String)> = gf
        .gs_lst
        .as_ref()?
        .gs
        .iter()
        .filter_map(|g| {
            let pos = parse_dml_pct(&g.pos)?.clamp(0.0, 1.0);
            let (hex, _alpha) = color_choice_to_hex(g.color_choice.as_deref(), theme)?;
            Some((pos, hex))
        })
        .collect();
    stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    match stops.len() {
        0 => None,
        1 => Some(ShapeFill::Solid(stops.remove(0).1)),
        _ => {
            let angle_deg = match gf.shade_properties.as_deref() {
                Some(EGShadeProperties::Lin(l)) => {
                    l.ang.map(|a| f64::from(a) / 60_000.0).unwrap_or(45.0)
                }
                _ => 45.0,
            };
            Some(ShapeFill::Gradient { stops, angle_deg })
        }
    }
}

/// An explicit `spPr/pattFill` (§20.1.8.47) on a SHAPE: a tier-1 preset
/// (`@prst`) with BOTH colours resolves to a real repeating [`ShapeFill::Pattern`]
/// the painter rasterises; otherwise (an unmapped preset, or a single colour)
/// it degrades to the 50/50 fg/bg blend `Solid` — the documented
/// stand-in. One colour missing → the default (`bgClr` → white, `fgClr` →
/// black) per §20.1.8.47 so the pattern still reads.
fn pattern_shape_fill(
    pf: &ooxml_dml::types::PatternFill,
    theme: &SlideTheme,
) -> Option<ShapeFill> {
    let resolve = |c: Option<&ooxml_dml::types::CTColor>| {
        c.and_then(|c| color_choice_to_hex(c.color_choice.as_deref(), theme))
            .map(|(hex, _alpha)| hex)
    };
    let fg = resolve(pf.fg_clr.as_deref());
    let bg = resolve(pf.bg_clr.as_deref());
    // Both colours absent → no fill at all (nothing to blend or tile).
    if fg.is_none() && bg.is_none() {
        return None;
    }
    let fg = fg.unwrap_or_else(|| "#000000".to_string());
    let bg = bg.unwrap_or_else(|| "#ffffff".to_string());
    // A tier-1 preset → a real tile; an unmapped one → the blend fallback.
    if let Some(prst) = pf.preset.as_ref() {
        let name = prst.to_string();
        if crate::engine::pattern::pattern_tile(&name).is_some() {
            return Some(ShapeFill::Pattern { prst: name, fg, bg });
        }
        #[cfg(feature = "debug-probe")]
        crate::console_log(&format!(
            "pml pattFill: unmapped preset '{name}' → 50/50 blend fallback"
        ));
    }
    Some(ShapeFill::Solid(blend_hex_50(&fg, &bg)))
}

/// 50/50 per-channel blend of two `#rrggbb` colours (the pattFill v1 tone).
fn blend_hex_50(a: &str, b: &str) -> String {
    let parse = |h: &str| {
        let h = h.trim_start_matches('#');
        // ASCII-hex gate before the byte slices: `len()` counts BYTES, so a
        // 6-byte multi-byte value (`"aé字"`, 6 bytes / 3 chars) reaching here
        // from a document-controlled `<a:srgbClr val=…>` would slice
        // mid-codepoint and panic. `ST_HexColorRGB` is six ASCII hex digits
        // (§20.1.10.19), so this rejects nothing a real document can carry.
        if h.len() != 6 || !h.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let r = u8::from_str_radix(&h[0..2], 16).ok()?;
        let g = u8::from_str_radix(&h[2..4], 16).ok()?;
        let b = u8::from_str_radix(&h[4..6], 16).ok()?;
        Some((u16::from(r), u16::from(g), u16::from(b)))
    };
    match (parse(a), parse(b)) {
        (Some((r1, g1, b1)), Some((r2, g2, b2))) => format!(
            "#{:02x}{:02x}{:02x}",
            (r1 + r2) / 2,
            (g1 + g2) / 2,
            (b1 + b2) / 2
        ),
        _ => a.to_string(),
    }
}

#[cfg(test)]
mod blend_hex_tests {
    use super::blend_hex_50;

    #[test]
    fn blends_valid_hex() {
        assert_eq!(blend_hex_50("#000000", "#FFFFFF"), "#7f7f7f");
    }

    /// A document-controlled `<a:srgbClr val="aé字"/>` is 6 BYTES but 3
    /// CHARS; a byte-length-only gate slices mid-codepoint and panics.
    #[test]
    fn multibyte_six_byte_hex_does_not_panic() {
        assert_eq!(blend_hex_50("aé字", "#FFFFFF"), "aé字");
        assert_eq!(blend_hex_50("#FFFFFF", "aé字"), "#FFFFFF");
    }
}

/// A shape's outline (`spPr/ln`, §20.1.2.2.24) as a colour + stroke width. An
/// explicit solid line fill wins; a line with NO fill child (or no `a:ln` at
/// all) falls back to the `<p:style>` lnRef colour (the ref's colour
/// child stands in for the theme lnStyleLst entry, so heavily-shaded theme
/// lines read slightly lighter, v1). An explicit `noFill` line kills the
/// outline and does NOT consult the style. The width is `ln@w` (EMU,
/// `STLineWidth`) → px, floored to a 1px hairline; absent → the spec hairline
/// default. Reading `ln` requires the `dml-lines` feature the `pml` feature
/// arms.
fn shape_outline(sp: &Shape, theme: &SlideTheme) -> Option<ShapeOutline> {
    use ooxml_dml::types::EGLineFillProperties as LF;
    let ln = sp.shape_properties.line.as_deref();
    if let Some(l) = ln {
        if matches!(l.line_fill_properties.as_deref(), Some(LF::NoFill(_))) {
            return None;
        }
    }
    let explicit = ln.and_then(|l| solid_line_fill_hex(l.line_fill_properties.as_deref(), theme));
    let color = explicit.or_else(|| style_ln_color(sp, theme))?;
    // `ln@w` is an EMU line width; convert to px. A 0 or absent width falls back
    // to a 1px hairline so a present outline always renders visibly.
    let width_px = ln
        .and_then(|l| l.width)
        .map(|w| emu_to_px(w as i64))
        .filter(|w| *w > 0.0)
        .unwrap_or(DEFAULT_OUTLINE_PX)
        .max(DEFAULT_OUTLINE_PX);
    let dash = ln.and_then(|l| line_dash(l.line_dash_properties.as_deref()));
    Some(ShapeOutline { color, width_px, dash })
}

/// The line's dash pattern: `<a:ln>/<a:prstDash val="…">` → a canonical
/// on/off array (line-width multiples) via [`crate::engine::pattern::preset_dash`],
/// or `<a:custDash>` → the `<a:ds d sp>` list mapped through
/// [`crate::engine::pattern::cust_dash`]. Absent / `solid` / an unmapped preset
/// → `None` (a solid stroke).
pub(super) fn line_dash(
    dash: Option<&ooxml_dml::types::EGLineDashProperties>,
) -> Option<Vec<f64>> {
    use ooxml_dml::types::EGLineDashProperties as D;
    match dash? {
        D::PrstDash(p) => crate::engine::pattern::preset_dash(&p.value.as_ref()?.to_string()),
        D::CustDash(c) => {
            let stops: Vec<(f64, f64)> = c
                .ds
                .iter()
                .filter_map(|s| {
                    Some((
                        crate::engine::color::parse_dml_pct(&s.d)?,
                        crate::engine::color::parse_dml_pct(&s.sp)?,
                    ))
                })
                .collect();
            crate::engine::pattern::cust_dash(&stops)
        }
    }
}

/// The style-matrix line colour: `<p:style>/<a:lnRef>`, idx ≥ 1 → the
/// ref's colour child with its transforms (the `<a:shade>` PowerPoint writes
/// on lnRef children lands here, so the v1 line tone tracks the deck's).
fn style_ln_color(sp: &Shape, theme: &SlideTheme) -> Option<String> {
    let style = sp.style.as_deref()?;
    if style.ln_ref.idx == 0 {
        return None;
    }
    color_choice_to_hex(style.ln_ref.color_choice.as_deref(), theme).map(|(hex, _alpha)| hex)
}

/// Resolve a shape's preset geometry: read the fork-parsed
/// `spPr/prstGeom` (`CTShapeProperties.geometry` — the fork parses
/// `prstGeom@prst` into the full `STShapeType` enum + `avLst` unconditionally,
/// fork generated.rs:12674/:12892; there is NO ext-trait accessor for it, so we
/// read the pub field directly, the same pattern as `fill_properties` above)
/// and build the tier-1 path in shape-local px.
///
/// Returns `(geometry, unknown_preset)`:
///  - `(Some(path), None)` — tier-1 preset or raw custGeom, path built;
///  - `(None, Some(name))` — a preset NOT in tier 1, or `"custGeom"` for an
///    unparseable freeform (the debug-probe field-gap signal); rectangle;
///  - `(None, None)` — no `prstGeom` and no raw custGeom at all.
///
/// Adjust values: only `gd@fmla="val N"` guides are honoured (covers
/// PowerPoint's own output); guide FORMULA chains fall back to the preset's
/// spec default inside `preset_path` — never an error.
fn shape_geometry(
    sp: &Shape,
    w_px: f64,
    h_px: f64,
    custgeom_xml: Option<&str>,
) -> (Option<Vec<PathCmd>>, Option<String>) {
    use ooxml_dml::types::EGGeometry;
    // A raw-scanned `<a:custGeom>` (from the ORIGINAL pre-repair bytes) wins
    // over the parsed `prstGeom` — which RULE 3 forced to a "rect" placeholder
    // because the fork mis-parses `<a:moveTo>`/`<a:pt>`. Parse the real freeform
    // path here so the SAME preset painter fills+strokes it. An unparseable / over-
    // complex custGeom falls through to the placeholder rect + a debug-probe
    // signal (`unknown_preset = "custGeom"`).
    if let Some(xml) = custgeom_xml {
        match crate::engine::geom::custom_path(xml, w_px, h_px) {
            Some(path) => return (Some(path), None),
            None => return (None, Some("custGeom".to_string())),
        }
    }
    let Some(EGGeometry::PrstGeom(pg)) = sp.shape_properties.geometry.as_deref() else {
        return (None, None);
    };
    let preset = pg.preset.to_string();
    let adj: Vec<(String, f64)> = pg
        .av_lst
        .as_ref()
        .map(|l| {
            l.gd
                .iter()
                .filter_map(|g| parse_val_fmla(&g.fmla).map(|v| (g.name.clone(), v)))
                .collect()
        })
        .unwrap_or_default();
    match preset_path(&preset, w_px, h_px, &adj) {
        Some(path) => (Some(path), None),
        None => (None, Some(preset)),
    }
}

/// The 3-D per-face shaded regions for a shape whose `prstGeom` is a
/// cube/can/bevel (else `None`). Reads the SAME preset name + `avLst` guides as
/// [`shape_geometry`]; a custGeom shape never has preset faces (its silhouette
/// is freeform). The painter shades each face against the shape's solid base
/// fill — see [`SlideShape::faces`].
fn shape_faces(
    sp: &Shape,
    w_px: f64,
    h_px: f64,
    custgeom_xml: Option<&str>,
) -> Option<Vec<crate::engine::geom::ShadedFace>> {
    use ooxml_dml::types::EGGeometry;
    if custgeom_xml.is_some() {
        return None;
    }
    let EGGeometry::PrstGeom(pg) = sp.shape_properties.geometry.as_deref()? else {
        return None;
    };
    let preset = pg.preset.to_string();
    let adj: Vec<(String, f64)> = pg
        .av_lst
        .as_ref()
        .map(|l| {
            l.gd
                .iter()
                .filter_map(|g| parse_val_fmla(&g.fmla).map(|v| (g.name.clone(), v)))
                .collect()
        })
        .unwrap_or_default();
    crate::engine::geom::preset_faces(&preset, w_px, h_px, &adj)
}

/// Parse a `gd@fmla` of the literal form `val N` (§20.1.9.11 — the only guide
/// form PowerPoint writes into `avLst`). Anything else (`*/ 2 1`, `+- a b c`,
/// …) returns `None` so the preset's spec default applies.
fn parse_val_fmla(fmla: &str) -> Option<f64> {
    let rest = fmla.trim().strip_prefix("val")?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    rest.parse::<f64>().ok()
}

/// Extract `#rrggbb` from an `EGFillProperties` that is a `solidFill`. This
/// resolves `srgbClr` AND themed `schemeClr`/`sysClr` (via the slide theme);
/// gradient + pattern fills still return `None`.
pub(crate) fn solid_fill_hex(
    fill: Option<&ooxml_dml::types::EGFillProperties>,
    theme: &SlideTheme,
) -> Option<String> {
    use ooxml_dml::types::EGFillProperties;
    let EGFillProperties::SolidFill(sf) = fill? else {
        return None;
    };
    color_choice_to_hex(sf.color_choice.as_deref(), theme).map(|(hex, _alpha)| hex)
}

/// Extract `#rrggbb` from an `EGLineFillProperties` that is a `solidFill` (the
/// line-fill twin of [`solid_fill_hex`]), resolving themed colours.
pub(crate) fn solid_line_fill_hex(
    fill: Option<&ooxml_dml::types::EGLineFillProperties>,
    theme: &SlideTheme,
) -> Option<String> {
    use ooxml_dml::types::EGLineFillProperties;
    let EGLineFillProperties::SolidFill(sf) = fill? else {
        return None;
    };
    color_choice_to_hex(sf.color_choice.as_deref(), theme).map(|(hex, _alpha)| hex)
}

/// Resolve any `EGColorChoice` to a CSS `#rrggbb` + resolved alpha (0.0–1.0):
/// `srgbClr` is the decoded `[r,g,b]` triple; `schemeClr` resolves through the
/// slide theme (clrMap for `bg/tx`, direct scheme slot otherwise) then applies
/// its DML colour transforms (lumMod/lumOff/tint/shade/satMod/alpha); `sysClr`
/// uses `@lastClr`. `phClr` (placeholder colour) and `hsl/scrgb/prst` are
/// best-effort `None` (style-matrix fills out of scope). The alpha is
/// returned for callers that can honour transparency; the fill sites currently
/// use the hex only.
pub(crate) fn color_choice_to_hex(
    choice: Option<&ooxml_dml::types::EGColorChoice>,
    theme: &SlideTheme,
) -> Option<(String, f64)> {
    use ooxml_dml::types::EGColorChoice;
    match choice? {
        EGColorChoice::SrgbClr(c) => {
            let rgb = &c.value;
            if rgb.len() != 3 {
                return None;
            }
            let hex = format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
            Some(apply_transforms(hex, &c.color_transform))
        }
        EGColorChoice::SchemeClr(c) => {
            let base = resolve_scheme_color(&c.value, theme)?;
            Some(apply_transforms(base, &c.color_transform))
        }
        EGColorChoice::SysClr(c) => {
            let rgb = c.last_clr.as_ref()?;
            if rgb.len() != 3 {
                return None;
            }
            let hex = format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
            Some(apply_transforms(hex, &c.color_transform))
        }
        // scrgb/hsl/prst/phClr: theme-matrix + named colours out of scope.
        _ => None,
    }
}

/// Map an `EGColorTransform` slice to our [`DmlTransform`] set and apply it to a
/// base `#rrggbb`, returning `(hex, alpha)`.
fn apply_transforms(
    hex: String,
    xs: &[ooxml_dml::types::EGColorTransform],
) -> (String, f64) {
    use crate::engine::color::{apply_color_transforms, parse_dml_pct, DmlTransform};
    use ooxml_dml::types::EGColorTransform as T;
    let mut out = Vec::new();
    for x in xs {
        let mapped = match x {
            T::LumMod(p) => parse_dml_pct(&p.value).map(DmlTransform::LumMod),
            T::LumOff(p) => parse_dml_pct(&p.value).map(DmlTransform::LumOff),
            T::SatMod(p) => parse_dml_pct(&p.value).map(DmlTransform::SatMod),
            T::SatOff(p) => parse_dml_pct(&p.value).map(DmlTransform::SatOff),
            T::Tint(p) => parse_dml_pct(&p.value).map(DmlTransform::Tint),
            T::Shade(p) => parse_dml_pct(&p.value).map(DmlTransform::Shade),
            T::Alpha(p) => parse_dml_pct(&p.value).map(DmlTransform::Alpha),
            _ => None,
        };
        if let Some(m) = mapped {
            out.push(m);
        }
    }
    if out.is_empty() {
        return (hex, 1.0);
    }
    apply_color_transforms(&hex, &out)
}

/// Resolve a `<a:schemeClr@val>` to a base `#rrggbb` against the slide theme.
/// `bg1/tx1/bg2/tx2` (and `phClr`) route through the master `<p:clrMap>` to a
/// scheme slot; every other value reads the scheme slot directly. `dk1/lt1`
/// resolve through the theme's sysClr `lastClr` (already baked into `scheme`).
pub(crate) fn resolve_scheme_color(
    val: &ooxml_dml::types::STSchemeColorVal,
    theme: &SlideTheme,
) -> Option<String> {
    use ooxml_dml::types::STSchemeColorVal as V;
    // Map the val to a placeholder name (clrMap-routed) or a direct slot name.
    let via_clrmap = |ph: &str, theme: &SlideTheme| -> Option<String> {
        let slot = theme.clrmap.get(ph).cloned().unwrap_or_else(|| ph.to_string());
        theme.slot_hex(&slot)
    };
    match val {
        V::Bg1 => via_clrmap("bg1", theme),
        V::Tx1 => via_clrmap("tx1", theme),
        V::Bg2 => via_clrmap("bg2", theme),
        V::Tx2 => via_clrmap("tx2", theme),
        V::Dk1 => theme.slot_hex("dk1"),
        V::Lt1 => theme.slot_hex("lt1"),
        V::Dk2 => theme.slot_hex("dk2"),
        V::Lt2 => theme.slot_hex("lt2"),
        V::Accent1 => theme.slot_hex("accent1"),
        V::Accent2 => theme.slot_hex("accent2"),
        V::Accent3 => theme.slot_hex("accent3"),
        V::Accent4 => theme.slot_hex("accent4"),
        V::Accent5 => theme.slot_hex("accent5"),
        V::Accent6 => theme.slot_hex("accent6"),
        V::Hlink => theme.slot_hex("hlink"),
        V::FolHlink => theme.slot_hex("folHlink"),
        // phClr (placeholder/style-matrix colour) is out of scope.
        V::PhClr => None,
    }
}

/// Vertical metrics (ascent/line-height px) for a run, used to stack paragraph
/// baselines. A run containing East Asian text takes the taller of the latin /
/// EA faces (the CJK face paints those glyphs, so it drives the line box).
/// Falls back to a font-pt-proportional estimate when no font is loaded (so
/// layout is sane before the JS font plan arrives).
pub(crate) fn run_line_metrics(run: &SlideRun, font_cache: &FontCache) -> (f64, f64) {
    let weight = if run.bold { 700 } else { 400 };
    let metrics_for = |family: &str| {
        font_cache
            .resolve(family, weight, run.italic)
            .or_else(|| font_cache.resolve(DEFAULT_FONT_FAMILY, 400, false))
            .map(|f| font_metrics(f, run.font_pt))
    };
    let mut m = metrics_for(&run.family);
    if run.family_east_asia != run.family && fonts::text_has_east_asia(&run.text) {
        if let Some(ea) = metrics_for(&run.family_east_asia) {
            m = Some(match m {
                Some(l) => crate::engine::fonts::FontMetrics {
                    ascent: l.ascent.max(ea.ascent),
                    descent: l.descent.max(ea.descent),
                    line_gap: l.line_gap.max(ea.line_gap),
                },
                None => ea,
            });
        }
    }
    match m {
        Some(m) => (m.ascent, m.ascent + m.descent),
        None => {
            let px = pt_to_px(run.font_pt);
            (px * 0.8, px * 1.2)
        }
    }
}

#[cfg(test)]
mod geom_tests {
    use super::*;

    #[test]
    fn resolve_prefers_exact_type_idx() {
        let mut g = LayoutGeom::default();
        let exact = PhBox { off_x: 1, off_y: 2, ext_cx: 3, ext_cy: 4 };
        let type_only = PhBox { off_x: 9, off_y: 9, ext_cx: 9, ext_cy: 9 };
        g.by_type_idx.insert("body|1".into(), exact);
        g.by_type.insert("body".into(), type_only);
        let got = g.resolve(Some("body"), Some(1)).unwrap();
        assert_eq!((got.off_x, got.off_y), (1, 2));
    }

    #[test]
    fn resolve_falls_back_to_type_then_idx() {
        let mut g = LayoutGeom::default();
        let t = PhBox { off_x: 5, off_y: 6, ext_cx: 7, ext_cy: 8 };
        let i = PhBox { off_x: 50, off_y: 60, ext_cx: 70, ext_cy: 80 };
        g.by_type.insert("title".into(), t);
        g.by_idx.insert(2, i);
        // type matches even when idx differs
        assert_eq!(g.resolve(Some("title"), Some(99)).unwrap().off_x, 5);
        // idx-only fallback when type absent from map
        assert_eq!(g.resolve(Some("missing"), Some(2)).unwrap().off_x, 50);
        // nothing matches
        assert!(g.resolve(Some("missing"), Some(7)).is_none());
    }

    /// Pins the bullet-char decode posture (the shared engine decoder):
    /// named + numeric refs decode; malformed refs emit `&` + rescan.
    #[test]
    fn decode_char_refs_handles_numeric_and_named() {
        use crate::engine::vml::decode_char_refs;
        assert_eq!(decode_char_refs("&#8226;"), "\u{2022}");
        assert_eq!(decode_char_refs("&#x2022;"), "\u{2022}");
        assert_eq!(decode_char_refs("&amp;"), "&");
        assert_eq!(decode_char_refs("\u{2022}"), "\u{2022}"); // already literal
        assert_eq!(decode_char_refs("a&unknown;b"), "a&unknown;b");
        assert_eq!(decode_char_refs("x&y"), "x&y"); // bare amp passthrough
    }

    #[test]
    fn default_box_titles_sit_above_body() {
        let title = default_ph_box(Some("ctrTitle"), 9_144_000, 6_858_000);
        let body = default_ph_box(Some("body"), 9_144_000, 6_858_000);
        assert!(title.off_y < body.off_y, "title band must be above body band");
        assert!(title.off_x > 0 && body.off_x > 0, "both inset from the left edge");
    }

    #[test]
    fn format_autonum_basic_schemes() {
        // §21.1.2.4.1: the basic arabic/alpha/roman × period/paren formats.
        assert_eq!(format_autonum("arabicPeriod", 1), "1.");
        assert_eq!(format_autonum("arabicParenR", 2), "2)");
        assert_eq!(format_autonum("arabicParenBoth", 3), "(3)");
        assert_eq!(format_autonum("arabicPlain", 4), "4");
        assert_eq!(format_autonum("alphaLcPeriod", 1), "a.");
        assert_eq!(format_autonum("alphaLcPeriod", 27), "aa.");
        assert_eq!(format_autonum("alphaUcParenR", 2), "B)");
        assert_eq!(format_autonum("romanLcPeriod", 4), "iv.");
        assert_eq!(format_autonum("romanLcPeriod", 9), "ix.");
        assert_eq!(format_autonum("romanUcParenBoth", 14), "(XIV)");
    }

    #[test]
    fn format_autonum_exotic_falls_back_to_arabic_period() {
        // Exotic schemes (CJK/circle/thai/…) fall back to arabicPeriod whole —
        // suffix included (NOT a hybrid of the exotic suffix on an arabic body).
        assert_eq!(format_autonum("circleNumDbPlain", 3), "3.");
        assert_eq!(format_autonum("ea1ChsPeriod", 2), "2.");
        assert_eq!(format_autonum("thaiAlphaParenR", 1), "1.");
    }

    #[test]
    fn master_ph_resolution_aliases_ctr_title_to_title() {
        // §19.7.10: the master carries only the BASE placeholder types, so a
        // slide ctrTitle/subTitle hops onto the master title/body entry.
        let mut m = LayoutGeom::default();
        let t = PhBox { off_x: 1, off_y: 2, ext_cx: 3, ext_cy: 4 };
        let b = PhBox { off_x: 5, off_y: 6, ext_cx: 7, ext_cy: 8 };
        m.by_type.insert("title".into(), t);
        m.by_type.insert("body".into(), b);
        assert_eq!(resolve_master_ph(&m, Some("ctrTitle"), None).unwrap().off_x, 1);
        assert_eq!(resolve_master_ph(&m, Some("subTitle"), None).unwrap().off_x, 5);
        // A direct type match still wins without aliasing.
        assert_eq!(resolve_master_ph(&m, Some("title"), None).unwrap().off_x, 1);
        assert!(resolve_master_ph(&m, Some("pic"), None).is_none());
    }

    #[test]
    fn text_style_chain_merges_first_some_per_field() {
        // Layer 0 (shape lstStyle) sets size only; layer 1 (master bodyStyle)
        // sets size+bold+bullet. The merged level takes layer-0's size and
        // layer-1's bold/bullet (per-field first-Some, weakest-last).
        let mut l0 = ListStyle::default();
        l0.levels[0] = Some(LevelStyle { sz_pt: Some(20.0), ..Default::default() });
        let mut l1 = ListStyle::default();
        l1.levels[0] = Some(LevelStyle {
            sz_pt: Some(28.0),
            bold: Some(true),
            bullet: Some(BulletSpec::Char("\u{2022}".into())),
            ..Default::default()
        });
        // Level 2 only in the master layer; defPPr backs a missing level.
        l1.def = LevelStyle { sz_pt: Some(18.0), ..Default::default() };
        let chain = TextStyleChain { layers: vec![l0, l1] };
        let lvl0 = chain.level(0);
        assert_eq!(lvl0.sz_pt, Some(20.0), "shape layer size wins");
        assert_eq!(lvl0.bold, Some(true), "master bold fills the gap");
        assert_eq!(lvl0.bullet, Some(BulletSpec::Char("\u{2022}".into())));
        // A level neither layer defines falls to the master defPPr.
        assert_eq!(chain.level(4).sz_pt, Some(18.0), "defPPr backs missing levels");
    }
}
