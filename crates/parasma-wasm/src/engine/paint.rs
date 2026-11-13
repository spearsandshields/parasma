//! The canvas paint seam (a trait so draws can be recorded off-thread): `Painter` is THE
//! backend-abstract drawing surface. The direct impl is `CanvasPainter` (a
//! thin forwarder to `CanvasRenderingContext2d`, byte-for-byte the old direct
//! `ctx.*` calls). Off-main-thread rendering adds a second impl — a
//! `RecordingPainter` that captures
//! the same primitive stream as a serializable display list — so the WML/SML/PML
//! render code (which now takes `p: &dyn Painter` instead of a concrete ctx) can
//! be driven inside a Web Worker that has no `CanvasRenderingContext2d`. The
//! main thread then REPLAYS that list onto its real canvas (GPU preserved, no
//! font fork). Trait methods are `&self` + interior-mutable backends (the
//! recording impl uses a `RefCell<Vec<…>>`) so the render call sites stay an
//! exact `ctx.` → `p.` rename with no borrow-checker churn — `&dyn Painter` is a
//! drop-in for the old `&CanvasRenderingContext2d`.
//!
//! Method names mirror `web_sys::CanvasRenderingContext2d` 1:1 so the relocation
//! is mechanical. The trait is deliberately web_sys-FREE so a `RecordingPainter`
//! can implement it in a Web Worker (which has no `CanvasRenderingContext2d`):
//! `draw_image` takes a string KEY, not an `ImageBitmap` (the worker holds no
//! bitmaps — the main thread resolves the key at replay time). One method is not
//! a primitive draw and gates the worker port: `measure_text_width` (a sync
//! browser query — only the watermark uses it; the recording backend returns
//! 0.0). `RunStyle` lives here as the format-neutral "what to draw" payload (SML
//! cell text constructs one too); the WML style-cascade `impl RunStyle` block
//! stays in lib.rs (classification/policy split).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use wasm_bindgen::JsValue;
use web_sys::{CanvasRenderingContext2d, ImageBitmap};

use super::units::pt_to_px;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Underline {
    None,
    Single,
    Double,
    Thick,
    Dotted,
    Dash,
    Wave,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum VertAlign {
    Baseline,
    Superscript,
    Subscript,
}

#[derive(Clone)]
pub(crate) struct RunStyle {
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) font_pt: f64,
    pub(crate) family: String,
    /// East Asian (`eastAsia`) and Complex Script (`cs`) font-slot families,
    /// resolved per §17.3.2.26; default to `family` when the run sets no such
    /// slot. `run_is_complex` records `<w:cs/>`/`<w:rtl/>` (step 2 of the slot
    /// algorithm). `collect_items` segments a run by Unicode script and stamps
    /// the slot family onto each segment's item via `family`.
    pub(crate) family_east_asia: String,
    pub(crate) family_complex: String,
    pub(crate) run_is_complex: bool,
    pub(crate) color: String,
    pub(crate) underline: Underline,
    pub(crate) strikethrough: bool,
    pub(crate) double_strike: bool,
    pub(crate) vert_align: VertAlign,
    pub(crate) highlight: Option<String>,
    pub(crate) run_shading: Option<String>,
    pub(crate) char_spacing_px: f64,
    pub(crate) all_caps: bool,
    pub(crate) small_caps: bool,
    pub(crate) vanish: bool,
    /// Hollow glyphs — painted as a stroked outline instead of filled (§17.3.2.23).
    pub(crate) outline: bool,
    /// Drop shadow behind the text (§17.3.2.31).
    pub(crate) shadow: bool,
    /// Raised / embossed appearance (§17.3.2.13).
    pub(crate) emboss: bool,
    /// Engraved / pressed-in appearance (§17.3.2.18).
    pub(crate) imprint: bool,
    /// Baseline raise (+) / lower (−) in px from `w:position` (§17.3.2.24).
    /// Independent of super/subscript, which scale the font; this just shifts.
    pub(crate) baseline_shift_px: f64,
}

impl Default for RunStyle {
    /// The neutral run style: no emphasis, the default font size/text colour, and
    /// empty font-slot families. Cannot be `#[derive]`d because `font_pt` and
    /// `color` are non-Rust-defaults (`DEFAULT_FONT_PT` / `DEFAULT_TEXT_COLOR`).
    /// This is the single source for the `RunStyle::marker` test helper and every
    /// neutral-style literal that previously spelled these values out by hand.
    fn default() -> Self {
        RunStyle {
            bold: false,
            italic: false,
            font_pt: crate::engine::fonts::DEFAULT_FONT_PT,
            family: String::new(),
            family_east_asia: String::new(),
            family_complex: String::new(),
            run_is_complex: false,
            color: crate::DEFAULT_TEXT_COLOR.to_owned(),
            underline: Underline::None,
            strikethrough: false,
            double_strike: false,
            vert_align: VertAlign::Baseline,
            highlight: None,
            run_shading: None,
            char_spacing_px: 0.0,
            all_caps: false,
            small_caps: false,
            vanish: false,
            outline: false,
            shadow: false,
            emboss: false,
            imprint: false,
            baseline_shift_px: 0.0,
        }
    }
}

#[cfg(test)]
impl RunStyle {
    /// Serialize every field into the oracle ITEM stream.
    ///
    /// The opening exhaustive destructure (NO rest-dots) is a COMPILE-TIME
    /// completeness guard, the pattern `CellStyle::hash_into` (sml/model.rs)
    /// established: adding a field to `RunStyle` fails to compile until it is
    /// bound and emitted here. Before this existed the oracle read ZERO of these
    /// 24 fields, so re-flagging every painted run bold, magenta, hidden or Comic
    /// Sans left all 21 pinned hashes green (measured).
    ///
    /// Purely descriptive — no coordinates — because the ITEM stream is pinned
    /// separately from the LINE stream precisely so a repositioning cannot move it.
    pub(crate) fn dump_into(&self, out: &mut String) {
        use std::fmt::Write;
        let RunStyle {
            bold, italic, font_pt, family, family_east_asia, family_complex,
            run_is_complex, color, underline, strikethrough, double_strike,
            vert_align, highlight, run_shading, char_spacing_px, all_caps,
            small_caps, vanish, outline, shadow, emboss, imprint, baseline_shift_px,
        } = self;
        let u = match underline {
            Underline::None => "none",
            Underline::Single => "single",
            Underline::Double => "double",
            Underline::Thick => "thick",
            Underline::Dotted => "dotted",
            Underline::Dash => "dash",
            Underline::Wave => "wave",
        };
        let va = match vert_align {
            VertAlign::Baseline => "base",
            VertAlign::Superscript => "sup",
            VertAlign::Subscript => "sub",
        };
        let _ = write!(
            out,
            "b{} i{} pt{font_pt:.2} fam{family:?} ea{family_east_asia:?} cs{family_complex:?} \
             rcx{} col{color:?} u={u} st{} ds{} va={va} hl{} rs{} csp{char_spacing_px:.2} \
             caps{} smcaps{} vanish{} outline{} shadow{} emboss{} imprint{} bshift{baseline_shift_px:.2}",
            *bold as u8, *italic as u8, *run_is_complex as u8, *strikethrough as u8,
            *double_strike as u8,
            highlight.as_deref().unwrap_or("-"), run_shading.as_deref().unwrap_or("-"),
            *all_caps as u8, *small_caps as u8, *vanish as u8, *outline as u8,
            *shadow as u8, *emboss as u8, *imprint as u8,
        );
    }
}

/// The backend-abstract drawing surface. Method names mirror
/// `CanvasRenderingContext2d` so render code relocates as a pure `ctx.` → `p.`
/// rename. `&self` + interior-mutable backends keep the call sites borrow-clean.
/// Must stay object-safe (`&dyn Painter`): no generics, no `Self` returns.
pub(crate) trait Painter {
    // ── state stack ─────────────────────────────────────────────────────────
    fn save(&self);
    fn restore(&self);

    // ── paths ───────────────────────────────────────────────────────────────
    fn begin_path(&self);
    fn close_path(&self);
    fn move_to(&self, x: f64, y: f64);
    fn line_to(&self, x: f64, y: f64);
    fn rect(&self, x: f64, y: f64, w: f64, h: f64);
    fn arc(&self, x: f64, y: f64, radius: f64, start_angle: f64, end_angle: f64) -> Result<(), JsValue>;
    fn arc_with_anticlockwise(
        &self,
        x: f64,
        y: f64,
        radius: f64,
        start_angle: f64,
        end_angle: f64,
        anticlockwise: bool,
    ) -> Result<(), JsValue>;
    // Curve/transform ops: a complete Painter vocabulary; not yet called on the
    // live render path (kept for display-list / off-thread parity).
    #[allow(clippy::too_many_arguments, dead_code)]
    fn ellipse_with_anticlockwise(
        &self,
        x: f64,
        y: f64,
        radius_x: f64,
        radius_y: f64,
        rotation: f64,
        start_angle: f64,
        end_angle: f64,
        anticlockwise: bool,
    ) -> Result<(), JsValue>;
    #[allow(dead_code)]
    fn quadratic_curve_to(&self, cpx: f64, cpy: f64, x: f64, y: f64);
    #[allow(dead_code)]
    fn bezier_curve_to(&self, cp1x: f64, cp1y: f64, cp2x: f64, cp2y: f64, x: f64, y: f64);
    fn fill(&self);
    fn stroke(&self);
    fn clip(&self);

    // ── rects ─────────────────────────────────────────────────────────────--
    fn fill_rect(&self, x: f64, y: f64, w: f64, h: f64);
    fn stroke_rect(&self, x: f64, y: f64, w: f64, h: f64);

    // ── text ──────────────────────────────────────────────────────────────--
    fn fill_text(&self, text: &str, x: f64, y: f64) -> Result<(), JsValue>;
    fn stroke_text(&self, text: &str, x: f64, y: f64) -> Result<(), JsValue>;

    // ── style state ─────────────────────────────────────────────────────────
    fn set_font(&self, value: &str);
    fn set_text_align(&self, value: &str);
    fn set_text_baseline(&self, value: &str);
    fn set_fill_style_str(&self, value: &str);
    fn set_stroke_style_str(&self, value: &str);
    fn set_line_width(&self, value: f64);
    fn set_global_alpha(&self, value: f64);
    fn set_shadow_color(&self, value: &str);
    fn set_shadow_offset_x(&self, value: f64);
    fn set_shadow_offset_y(&self, value: f64);
    fn set_shadow_blur(&self, value: f64);

    // ── transform ─────────────────────────────────────────────────────────--
    fn translate(&self, x: f64, y: f64) -> Result<(), JsValue>;
    fn rotate(&self, angle: f64) -> Result<(), JsValue>;
    fn scale(&self, x: f64, y: f64) -> Result<(), JsValue>;
    #[allow(dead_code)] // absolute-transform op, kept for display-list parity
    fn set_transform(&self, a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Result<(), JsValue>;

    // ── images ────────────────────────────────────────────────────────────--
    /// Draw image `key` (a rel-id / cache key) into the dest rect. The backend
    /// resolves the key: `CanvasPainter` looks up its bitmap cache and falls back
    /// to the `[image]` placeholder on a miss; `RecordingPainter` records the key
    /// and the main-thread replay resolves it (the worker holds no bitmaps).
    fn draw_image(&self, key: &str, dx: f64, dy: f64, dw: f64, dh: f64) -> Result<(), JsValue>;

    // ── worker-gating query (no recording equivalent) ───────────────────────────--
    /// Width of `text` under the currently-set font. A SYNC browser query with no
    /// recording equivalent — only the watermark uses it. The recording backend
    /// returns 0.0 (watermark sizing is resolved on the main thread for now).
    fn measure_text_width(&self, text: &str) -> f64;

    // ── gradient / pattern / dash (PML fidelity) ──────────────────────────────--
    // The five PML-only style primitives the display list carries for
    // real (no solid approximation). The canvas backend builds the concrete
    // `CanvasGradient` / `CanvasPattern` via `js_sys::Reflect` (the dash/gradient/
    // pattern methods aren't in the armed web-sys surface); the recording backend
    // emits an opcode. Colours flow as resolved hex strings so the recorder never
    // holds a (main-thread-only, non-`Send`) `CanvasGradient`. `#[allow(dead_code)]`
    // — only PML paint calls these, so a docx-only build (ooxml-pml not linked)
    // leaves them uncalled, exactly like the curve/transform parity ops above.

    /// Set `fillStyle` to a LINEAR gradient with axis `(x0,y0)→(x1,y1)` and the
    /// given already-sorted `(position, colour)` stops (the caller resolves the
    /// box + angle into the endpoints). Canvas backend builds the gradient;
    /// recorder emits the opcode. `Err` = gradient surface unavailable — the
    /// caller then paints a solid.
    #[allow(dead_code)]
    fn set_linear_gradient(&self, x0: f64, y0: f64, x1: f64, y1: f64, stops: &[(f64, String)]) -> Result<(), JsValue>;

    /// Set `fillStyle` to a RADIAL gradient (inner circle `(x0,y0,r0)` → outer
    /// circle `(x1,y1,r1)`) with the given stops. See [`Painter::set_linear_gradient`].
    #[allow(dead_code, clippy::too_many_arguments)]
    fn set_radial_gradient(
        &self,
        x0: f64,
        y0: f64,
        r0: f64,
        x1: f64,
        y1: f64,
        r1: f64,
        stops: &[(f64, String)],
    ) -> Result<(), JsValue>;

    /// Set `fillStyle` to a repeating PATTERN rasterised from `tile` (a `bg`
    /// flood then the `fg` primitives), cached by `key` (`prst|fg|bg`). `Err` =
    /// pattern surface unavailable (`createPattern` null) — the caller then paints
    /// the fg/bg blend.
    #[allow(dead_code)]
    fn set_pattern_fill(&self, key: &str, tile: &crate::engine::pattern::PatternTile, fg: &str, bg: &str) -> Result<(), JsValue>;

    /// Set the line dash to `segments` (already scaled to px by the caller).
    #[allow(dead_code)]
    fn set_line_dash(&self, segments: &[f64]);

    /// Clear any line dash so following strokes are solid (`setLineDash([])`).
    #[allow(dead_code)]
    fn clear_line_dash(&self);

    /// Paint one glyph run at a baseline, honoring the outline/shadow/emboss/imprint
    /// run effects (§17.3.2.13/.18/.23/.31). The caller has already set the font and
    /// the run's fill colour; this leaves the fill colour unchanged for the normal
    /// path and restores it after the fake-3D paths so following draws are unaffected.
    fn paint_glyph_run(&self, style: &RunStyle, text: &str, x: f64, y: f64) -> Result<(), JsValue> {
        let font_px = pt_to_px(style.font_pt);

        if style.outline {
            // Hollow glyphs — stroke the outline in the run colour, no fill.
            self.set_stroke_style_str(&style.color);
            self.set_line_width((font_px * 0.03).max(0.5));
            return self.stroke_text(text, x, y);
        }

        if style.emboss || style.imprint {
            // Fake 3D: a light highlight copy and a dark shadow copy offset in
            // opposite directions, with the glyph in a mid tone. On the white page
            // this reads as raised (emboss) or pressed-in (imprint, offsets flipped).
            let d = (font_px * 0.04).max(0.7);
            let (hi, sh) = if style.emboss { ((-d, -d), (d, d)) } else { ((d, d), (-d, -d)) };
            self.set_fill_style_str("rgba(255,255,255,0.75)");
            self.fill_text(text, x + hi.0, y + hi.1)?;
            self.set_fill_style_str("rgba(0,0,0,0.35)");
            self.fill_text(text, x + sh.0, y + sh.1)?;
            self.set_fill_style_str("#9a9a9a");
            self.fill_text(text, x, y)?;
            self.set_fill_style_str(&style.color);
            return Ok(());
        }

        if style.shadow {
            self.set_shadow_color("rgba(0,0,0,0.45)");
            self.set_shadow_offset_x(font_px * 0.06);
            self.set_shadow_offset_y(font_px * 0.06);
            self.set_shadow_blur(font_px * 0.03);
            self.fill_text(text, x, y)?;
            // Clear so the shadow does not bleed into later draws on this context.
            self.set_shadow_color("rgba(0,0,0,0)");
            self.set_shadow_offset_x(0.0);
            self.set_shadow_offset_y(0.0);
            self.set_shadow_blur(0.0);
            return Ok(());
        }

        self.fill_text(text, x, y)
    }
}

/// The real-canvas backend: every method forwards verbatim to the wrapped
/// `CanvasRenderingContext2d`, so output is byte-for-byte the pre-trait code.
/// Holds the bitmap cache so `draw_image` can resolve a key (the lookup +
/// placeholder fallback that used to live at each render call site).
pub(crate) struct CanvasPainter<'a> {
    ctx: &'a CanvasRenderingContext2d,
    images: &'a HashMap<String, ImageBitmap>,
    /// pattFill tile cache: one `CanvasPattern` per `prst|fg|bg` key, per render.
    /// Holds what the old render-local `pattern_cache` in `pml::render_page_clipped`
    /// held (one rasterised tile per key); interior-mutable so the trait stays
    /// `&self`. Inert for WML/SML callers (they never request a pattern fill), so
    /// docx-only builds never read it (`#[allow(dead_code)]`).
    #[allow(dead_code)]
    pattern_cache: RefCell<HashMap<String, JsValue>>,
}

impl<'a> CanvasPainter<'a> {
    pub(crate) fn new(ctx: &'a CanvasRenderingContext2d, images: &'a HashMap<String, ImageBitmap>) -> Self {
        Self { ctx, images, pattern_cache: RefCell::new(HashMap::new()) }
    }

    /// Set the canvas line dash to `arr` via `js_sys::Reflect` (the dash method is
    /// not in the armed web-sys surface). Moved verbatim from the old
    /// `pml::call_set_line_dash`; drives both `set_line_dash` and `clear_line_dash`
    /// (both PML-only, so docx-only builds never call this — `#[allow(dead_code)]`).
    #[allow(dead_code)]
    fn call_set_line_dash(&self, arr: &js_sys::Array) -> Result<(), JsValue> {
        use wasm_bindgen::JsCast;
        let ctx_js: &JsValue = self.ctx.as_ref();
        let f: js_sys::Function = js_sys::Reflect::get(ctx_js, &"setLineDash".into())?.dyn_into()?;
        f.call1(ctx_js, arr)?;
        Ok(())
    }
}

impl Painter for CanvasPainter<'_> {
    fn save(&self) {
        self.ctx.save();
    }
    fn restore(&self) {
        self.ctx.restore();
    }
    fn begin_path(&self) {
        self.ctx.begin_path();
    }
    fn close_path(&self) {
        self.ctx.close_path();
    }
    fn move_to(&self, x: f64, y: f64) {
        self.ctx.move_to(x, y);
    }
    fn line_to(&self, x: f64, y: f64) {
        self.ctx.line_to(x, y);
    }
    fn rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.ctx.rect(x, y, w, h);
    }
    fn arc(&self, x: f64, y: f64, radius: f64, start_angle: f64, end_angle: f64) -> Result<(), JsValue> {
        self.ctx.arc(x, y, radius, start_angle, end_angle)
    }
    fn arc_with_anticlockwise(
        &self,
        x: f64,
        y: f64,
        radius: f64,
        start_angle: f64,
        end_angle: f64,
        anticlockwise: bool,
    ) -> Result<(), JsValue> {
        self.ctx
            .arc_with_anticlockwise(x, y, radius, start_angle, end_angle, anticlockwise)
    }
    fn ellipse_with_anticlockwise(
        &self,
        x: f64,
        y: f64,
        radius_x: f64,
        radius_y: f64,
        rotation: f64,
        start_angle: f64,
        end_angle: f64,
        anticlockwise: bool,
    ) -> Result<(), JsValue> {
        self.ctx.ellipse_with_anticlockwise(
            x, y, radius_x, radius_y, rotation, start_angle, end_angle, anticlockwise,
        )
    }
    fn quadratic_curve_to(&self, cpx: f64, cpy: f64, x: f64, y: f64) {
        self.ctx.quadratic_curve_to(cpx, cpy, x, y);
    }
    fn bezier_curve_to(&self, cp1x: f64, cp1y: f64, cp2x: f64, cp2y: f64, x: f64, y: f64) {
        self.ctx.bezier_curve_to(cp1x, cp1y, cp2x, cp2y, x, y);
    }
    fn fill(&self) {
        self.ctx.fill();
    }
    fn stroke(&self) {
        self.ctx.stroke();
    }
    fn clip(&self) {
        self.ctx.clip();
    }
    fn fill_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.ctx.fill_rect(x, y, w, h);
    }
    fn stroke_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.ctx.stroke_rect(x, y, w, h);
    }
    fn fill_text(&self, text: &str, x: f64, y: f64) -> Result<(), JsValue> {
        self.ctx.fill_text(text, x, y)
    }
    fn stroke_text(&self, text: &str, x: f64, y: f64) -> Result<(), JsValue> {
        self.ctx.stroke_text(text, x, y)
    }
    fn set_font(&self, value: &str) {
        self.ctx.set_font(value);
    }
    fn set_text_align(&self, value: &str) {
        self.ctx.set_text_align(value);
    }
    fn set_text_baseline(&self, value: &str) {
        self.ctx.set_text_baseline(value);
    }
    fn set_fill_style_str(&self, value: &str) {
        self.ctx.set_fill_style_str(value);
    }
    fn set_stroke_style_str(&self, value: &str) {
        self.ctx.set_stroke_style_str(value);
    }
    fn set_line_width(&self, value: f64) {
        self.ctx.set_line_width(value);
    }
    fn set_global_alpha(&self, value: f64) {
        self.ctx.set_global_alpha(value);
    }
    fn set_shadow_color(&self, value: &str) {
        self.ctx.set_shadow_color(value);
    }
    fn set_shadow_offset_x(&self, value: f64) {
        self.ctx.set_shadow_offset_x(value);
    }
    fn set_shadow_offset_y(&self, value: f64) {
        self.ctx.set_shadow_offset_y(value);
    }
    fn set_shadow_blur(&self, value: f64) {
        self.ctx.set_shadow_blur(value);
    }
    fn translate(&self, x: f64, y: f64) -> Result<(), JsValue> {
        self.ctx.translate(x, y)
    }
    fn rotate(&self, angle: f64) -> Result<(), JsValue> {
        self.ctx.rotate(angle)
    }
    fn scale(&self, x: f64, y: f64) -> Result<(), JsValue> {
        self.ctx.scale(x, y)
    }
    fn set_transform(&self, a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Result<(), JsValue> {
        self.ctx.set_transform(a, b, c, d, e, f)
    }
    fn draw_image(&self, key: &str, dx: f64, dy: f64, dw: f64, dh: f64) -> Result<(), JsValue> {
        // Resolve the key against the bitmap cache; on a miss draw the same
        // `[image]` placeholder the render call sites used to draw inline.
        match self.images.get(key) {
            Some(bm) => self
                .ctx
                .draw_image_with_image_bitmap_and_dw_and_dh(bm, dx, dy, dw, dh),
            None => super::images::draw_image_placeholder(self, dx, dy, dw, dh),
        }
    }
    fn measure_text_width(&self, text: &str) -> f64 {
        self.ctx.measure_text(text).map(|m| m.width()).unwrap_or(0.0)
    }

    fn set_linear_gradient(&self, x0: f64, y0: f64, x1: f64, y1: f64, stops: &[(f64, String)]) -> Result<(), JsValue> {
        // Verbatim from the old `pml::set_linear_gradient_fill_style` (minus the
        // axis math, now resolved by the caller): createLinearGradient(endpoints)
        // + addColorStop loop (source order) + set fillStyle. Reflect for the same
        // web-sys-feature reason as the old helper.
        use wasm_bindgen::JsCast;
        let ctx_js: &JsValue = self.ctx.as_ref();
        let create: js_sys::Function = js_sys::Reflect::get(ctx_js, &"createLinearGradient".into())?.dyn_into()?;
        let args = js_sys::Array::of4(&x0.into(), &y0.into(), &x1.into(), &y1.into());
        let grad = create.apply(ctx_js, &args)?;
        let add_stop: js_sys::Function = js_sys::Reflect::get(&grad, &"addColorStop".into())?.dyn_into()?;
        for (pos, hex) in stops {
            add_stop.call2(&grad, &(*pos).into(), &hex.as_str().into())?;
        }
        js_sys::Reflect::set(ctx_js, &"fillStyle".into(), &grad)?;
        Ok(())
    }

    fn set_radial_gradient(
        &self,
        x0: f64,
        y0: f64,
        r0: f64,
        x1: f64,
        y1: f64,
        r1: f64,
        stops: &[(f64, String)],
    ) -> Result<(), JsValue> {
        // Verbatim from the old `pml::paint_bg_radial` Reflect body (minus the
        // focus/radius math, now resolved by the caller).
        use wasm_bindgen::JsCast;
        let ctx_js: &JsValue = self.ctx.as_ref();
        let create: js_sys::Function = js_sys::Reflect::get(ctx_js, &"createRadialGradient".into())?.dyn_into()?;
        let args = js_sys::Array::new();
        for v in [x0, y0, r0, x1, y1, r1] {
            args.push(&v.into());
        }
        let grad = create.apply(ctx_js, &args)?;
        let add_stop: js_sys::Function = js_sys::Reflect::get(&grad, &"addColorStop".into())?.dyn_into()?;
        for (pos, hex) in stops {
            add_stop.call2(&grad, &(*pos).into(), &hex.as_str().into())?;
        }
        js_sys::Reflect::set(ctx_js, &"fillStyle".into(), &grad)?;
        Ok(())
    }

    fn set_pattern_fill(&self, key: &str, tile: &crate::engine::pattern::PatternTile, fg: &str, bg: &str) -> Result<(), JsValue> {
        // Verbatim from the old `pml::set_pattern_fill_style`: cache-hit → set the
        // stored pattern; else rasterise a same-size offscreen tile (bg flood, then
        // fg strokes/dots with lineCap "butt"), createPattern(…, "repeat"), set
        // fillStyle, and cache. Null pattern → Err (caller blends).
        use wasm_bindgen::JsCast;
        let ctx_js: &JsValue = self.ctx.as_ref();
        if let Some(pat) = self.pattern_cache.borrow().get(key) {
            js_sys::Reflect::set(ctx_js, &"fillStyle".into(), pat)?;
            return Ok(());
        }
        let win = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
        let doc = win.document().ok_or_else(|| JsValue::from_str("no document"))?;
        let canvas: web_sys::HtmlCanvasElement =
            doc.create_element("canvas")?.dyn_into().map_err(JsValue::from)?;
        let n = tile.size as u32;
        canvas.set_width(n);
        canvas.set_height(n);
        let tctx: web_sys::CanvasRenderingContext2d = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("no 2d ctx"))?
            .dyn_into()
            .map_err(JsValue::from)?;
        tctx.set_fill_style_str(bg);
        tctx.fill_rect(0.0, 0.0, tile.size, tile.size);
        tctx.set_stroke_style_str(fg);
        tctx.set_fill_style_str(fg);
        tctx.set_line_cap("butt");
        for prim in &tile.prims {
            use crate::engine::pattern::TilePrim;
            match *prim {
                TilePrim::Line { x1, y1, x2, y2, lw } => {
                    tctx.set_line_width(lw);
                    tctx.begin_path();
                    tctx.move_to(x1, y1);
                    tctx.line_to(x2, y2);
                    tctx.stroke();
                }
                TilePrim::Dot { x, y, s } => {
                    tctx.fill_rect(x, y, s, s);
                }
            }
        }
        let create: js_sys::Function = js_sys::Reflect::get(ctx_js, &"createPattern".into())?.dyn_into()?;
        let pat = create.call2(ctx_js, canvas.as_ref(), &"repeat".into())?;
        if pat.is_null() || pat.is_undefined() {
            return Err(JsValue::from_str("createPattern returned null"));
        }
        js_sys::Reflect::set(ctx_js, &"fillStyle".into(), &pat)?;
        self.pattern_cache.borrow_mut().insert(key.to_owned(), pat);
        Ok(())
    }

    fn set_line_dash(&self, segments: &[f64]) {
        let arr = js_sys::Array::new();
        for seg in segments {
            arr.push(&(*seg).into());
        }
        let _ = self.call_set_line_dash(&arr);
    }

    fn clear_line_dash(&self) {
        let _ = self.call_set_line_dash(&js_sys::Array::new());
    }
}

/// A `Painter` DECORATOR that translates page-logical coordinates into
/// WINDOW-LOCAL coordinates before forwarding to an inner painter, so a windowed
/// (`Some(win)`) render emits `device = scale · (logical − off)` bounded by the
/// SLICE size — never the whole page. This replaces the old giant CTM translate
/// (`ctx.translate(-clip)`) that made high-zoom device coordinates exceed the
/// Chrome/Skia ~16384 px canvas-coordinate limit, where Skia SILENTLY dropped
/// the draws and the white back-buffer blitted over the content (the growing
/// white block bug).
///
/// It is a DECORATOR (wraps `&dyn Painter`), NOT a set of fields on the concrete
/// painters, for three load-bearing reasons:
///   1. `draw_image` on a bitmap MISS re-enters `draw_image_placeholder`, which
///      calls `fill_rect`/`stroke_rect`/`fill_text` again. If the offset lived on
///      `CanvasPainter` those re-entrant calls would subtract the offset a SECOND
///      time (placeholder off-screen at high zoom = exactly the blank this prevents).
///      Here `draw_image` subtracts ONCE and forwards to the INNER un-offset
///      painter, whose placeholder re-entry draws with no further subtraction.
///   2. `CanvasPainter` and `RecordingPainter` stay byte-for-byte untouched, so
///      every full-page / `clip == None` caller is byte-identical BY
///      CONSTRUCTION — the decorator is simply not wrapped for them.
///   3. ONE offset+transition implementation is shared by the in-process canvas
///      path and the worker recording path (DRY-of-knowledge).
///
/// TRANSITION RULE (the subtlety): WML content + SML cells draw absolute at the
/// ROOT frame, but the WML watermark and PML shapes/connectors do
/// `save(); translate(abs_x, abs_y); rotate/scale; draw(local_coords)`. Once the
/// caller mutates the CTM, subsequent coords are frame-LOCAL and must NOT be
/// re-offset. So `translate` while `live` consumes the offset into the local
/// origin (`inner.translate(x−off, y−off)`) then flips `live=false`;
/// `rotate/scale/set_transform` forward verbatim and flip `live=false` (they only
/// ever follow a translate-to-pivot in real usage). `save`/`restore` push/pop the
/// `live` flag exactly like the CTM they shadow, so per-shape
/// `save/translate/…/restore` composes: the root offset frame is restored on
/// `restore`.
pub(crate) struct OffsetPainter<'a> {
    inner: &'a dyn Painter,
    off_x: f64,
    off_y: f64,
    /// When true, emitted coords are ROOT-frame page-logical and get the offset
    /// subtracted. Flipped false the moment the caller mutates the CTM (the
    /// coords are now frame-local and already positioned by the CTM).
    live: Cell<bool>,
    /// Shadows the canvas save/restore stack for `live` (pushed on `save`, popped
    /// on `restore`) so a per-shape frame restores the root offset frame.
    frame_stack: RefCell<Vec<bool>>,
}

impl<'a> OffsetPainter<'a> {
    /// Wrap `inner`, subtracting `(off_x, off_y)` — the WINDOW ORIGIN,
    /// `(win.x0, win.y0)` — from every root-frame coordinate. HARD INVARIANT:
    /// `off` MUST equal the window origin used to author the clip rect (which is
    /// emitted window-local at `(0,0)`), or the offset and the clip desync.
    pub(crate) fn new(inner: &'a dyn Painter, off_x: f64, off_y: f64) -> Self {
        Self {
            inner,
            off_x,
            off_y,
            live: Cell::new(true),
            frame_stack: RefCell::new(Vec::new()),
        }
    }

    #[inline]
    fn ax(&self, x: f64) -> f64 {
        if self.live.get() { x - self.off_x } else { x }
    }
    #[inline]
    fn ay(&self, y: f64) -> f64 {
        if self.live.get() { y - self.off_y } else { y }
    }
}

impl Painter for OffsetPainter<'_> {
    fn save(&self) {
        self.frame_stack.borrow_mut().push(self.live.get());
        self.inner.save();
    }
    fn restore(&self) {
        self.inner.restore();
        if let Some(prev) = self.frame_stack.borrow_mut().pop() {
            self.live.set(prev);
        }
    }
    fn begin_path(&self) {
        self.inner.begin_path();
    }
    fn close_path(&self) {
        self.inner.close_path();
    }
    fn move_to(&self, x: f64, y: f64) {
        self.inner.move_to(self.ax(x), self.ay(y));
    }
    fn line_to(&self, x: f64, y: f64) {
        self.inner.line_to(self.ax(x), self.ay(y));
    }
    fn rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.inner.rect(self.ax(x), self.ay(y), w, h);
    }
    fn arc(&self, x: f64, y: f64, radius: f64, start_angle: f64, end_angle: f64) -> Result<(), JsValue> {
        self.inner.arc(self.ax(x), self.ay(y), radius, start_angle, end_angle)
    }
    fn arc_with_anticlockwise(
        &self,
        x: f64,
        y: f64,
        radius: f64,
        start_angle: f64,
        end_angle: f64,
        anticlockwise: bool,
    ) -> Result<(), JsValue> {
        self.inner
            .arc_with_anticlockwise(self.ax(x), self.ay(y), radius, start_angle, end_angle, anticlockwise)
    }
    fn ellipse_with_anticlockwise(
        &self,
        x: f64,
        y: f64,
        radius_x: f64,
        radius_y: f64,
        rotation: f64,
        start_angle: f64,
        end_angle: f64,
        anticlockwise: bool,
    ) -> Result<(), JsValue> {
        self.inner.ellipse_with_anticlockwise(
            self.ax(x), self.ay(y), radius_x, radius_y, rotation, start_angle, end_angle, anticlockwise,
        )
    }
    fn quadratic_curve_to(&self, cpx: f64, cpy: f64, x: f64, y: f64) {
        self.inner
            .quadratic_curve_to(self.ax(cpx), self.ay(cpy), self.ax(x), self.ay(y));
    }
    fn bezier_curve_to(&self, cp1x: f64, cp1y: f64, cp2x: f64, cp2y: f64, x: f64, y: f64) {
        self.inner.bezier_curve_to(
            self.ax(cp1x), self.ay(cp1y), self.ax(cp2x), self.ay(cp2y), self.ax(x), self.ay(y),
        );
    }
    fn fill(&self) {
        self.inner.fill();
    }
    fn stroke(&self) {
        self.inner.stroke();
    }
    fn clip(&self) {
        self.inner.clip();
    }
    fn fill_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.inner.fill_rect(self.ax(x), self.ay(y), w, h);
    }
    fn stroke_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.inner.stroke_rect(self.ax(x), self.ay(y), w, h);
    }
    fn fill_text(&self, text: &str, x: f64, y: f64) -> Result<(), JsValue> {
        self.inner.fill_text(text, self.ax(x), self.ay(y))
    }
    fn stroke_text(&self, text: &str, x: f64, y: f64) -> Result<(), JsValue> {
        self.inner.stroke_text(text, self.ax(x), self.ay(y))
    }
    fn set_font(&self, value: &str) {
        self.inner.set_font(value);
    }
    fn set_text_align(&self, value: &str) {
        self.inner.set_text_align(value);
    }
    fn set_text_baseline(&self, value: &str) {
        self.inner.set_text_baseline(value);
    }
    fn set_fill_style_str(&self, value: &str) {
        self.inner.set_fill_style_str(value);
    }
    fn set_stroke_style_str(&self, value: &str) {
        self.inner.set_stroke_style_str(value);
    }
    fn set_line_width(&self, value: f64) {
        self.inner.set_line_width(value);
    }
    fn set_global_alpha(&self, value: f64) {
        self.inner.set_global_alpha(value);
    }
    fn set_shadow_color(&self, value: &str) {
        self.inner.set_shadow_color(value);
    }
    fn set_shadow_offset_x(&self, value: f64) {
        self.inner.set_shadow_offset_x(value);
    }
    fn set_shadow_offset_y(&self, value: f64) {
        self.inner.set_shadow_offset_y(value);
    }
    fn set_shadow_blur(&self, value: f64) {
        self.inner.set_shadow_blur(value);
    }
    // ── transform: the transition rule ───────────────────────────────────────
    fn translate(&self, x: f64, y: f64) -> Result<(), JsValue> {
        // Consume the window offset into the local origin ONCE, then the frame is
        // local — subsequent coords are NOT re-offset.
        let r = self.inner.translate(self.ax(x), self.ay(y));
        self.live.set(false);
        r
    }
    fn rotate(&self, angle: f64) -> Result<(), JsValue> {
        self.live.set(false);
        self.inner.rotate(angle)
    }
    fn scale(&self, x: f64, y: f64) -> Result<(), JsValue> {
        self.live.set(false);
        self.inner.scale(x, y)
    }
    fn set_transform(&self, a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Result<(), JsValue> {
        // Absolute matrix — the offset cannot be composed into it. No windowed
        // CONTENT path calls this while `live` (asserted by the source grep in the
        // regression test); forward verbatim and drop out of the offset frame.
        self.live.set(false);
        self.inner.set_transform(a, b, c, d, e, f)
    }
    fn draw_image(&self, key: &str, dx: f64, dy: f64, dw: f64, dh: f64) -> Result<(), JsValue> {
        // Subtract ONCE here; forward to the inner painter so its placeholder-miss
        // re-entry (fill_rect/stroke_rect/fill_text) does NOT subtract again.
        self.inner.draw_image(key, self.ax(dx), self.ay(dy), dw, dh)
    }
    fn measure_text_width(&self, text: &str) -> f64 {
        self.inner.measure_text_width(text)
    }
    fn set_linear_gradient(&self, x0: f64, y0: f64, x1: f64, y1: f64, stops: &[(f64, String)]) -> Result<(), JsValue> {
        self.inner
            .set_linear_gradient(self.ax(x0), self.ay(y0), self.ax(x1), self.ay(y1), stops)
    }
    fn set_radial_gradient(
        &self,
        x0: f64,
        y0: f64,
        r0: f64,
        x1: f64,
        y1: f64,
        r1: f64,
        stops: &[(f64, String)],
    ) -> Result<(), JsValue> {
        // Centres shift with the offset; radii are lengths and do NOT.
        self.inner
            .set_radial_gradient(self.ax(x0), self.ay(y0), r0, self.ax(x1), self.ay(y1), r1, stops)
    }
    fn set_pattern_fill(&self, key: &str, tile: &crate::engine::pattern::PatternTile, fg: &str, bg: &str) -> Result<(), JsValue> {
        // Tile coords are pattern-local (repeat space), not page coords — verbatim.
        self.inner.set_pattern_fill(key, tile, fg, bg)
    }
    fn set_line_dash(&self, segments: &[f64]) {
        self.inner.set_line_dash(segments);
    }
    fn clear_line_dash(&self) {
        self.inner.clear_line_dash();
    }
}

#[cfg(test)]
mod offset_tests {
    use super::*;
    use std::cell::Cell;

    /// A `Painter` that folds min/max over every coordinate it RECEIVES (so, as
    /// the INNER painter of an `OffsetPainter`, it sees post-offset window-local
    /// coords). On `draw_image` it re-enters `super::super::images::
    /// draw_image_placeholder(self, …)` EXACTLY as `CanvasPainter` does on a
    /// bitmap miss — the double-subtraction regression site. Being the inner
    /// painter, that re-entry is NOT re-offset (the decorator already subtracted
    /// once), which is the property under test.
    struct CapturingPainter {
        min_x: Cell<f64>,
        min_y: Cell<f64>,
        max_x: Cell<f64>,
        max_y: Cell<f64>,
    }
    impl CapturingPainter {
        fn new() -> Self {
            Self {
                min_x: Cell::new(f64::INFINITY),
                min_y: Cell::new(f64::INFINITY),
                max_x: Cell::new(f64::NEG_INFINITY),
                max_y: Cell::new(f64::NEG_INFINITY),
            }
        }
        fn fold_x(&self, x: f64) {
            self.min_x.set(self.min_x.get().min(x));
            self.max_x.set(self.max_x.get().max(x));
        }
        fn fold_y(&self, y: f64) {
            self.min_y.set(self.min_y.get().min(y));
            self.max_y.set(self.max_y.get().max(y));
        }
        /// The largest absolute coordinate magnitude seen (∞ if nothing drew).
        fn max_abs(&self) -> f64 {
            self.min_x
                .get()
                .abs()
                .max(self.max_x.get().abs())
                .max(self.min_y.get().abs())
                .max(self.max_y.get().abs())
        }
    }
    impl Painter for CapturingPainter {
        fn save(&self) {}
        fn restore(&self) {}
        fn begin_path(&self) {}
        fn close_path(&self) {}
        fn move_to(&self, x: f64, y: f64) {
            self.fold_x(x);
            self.fold_y(y);
        }
        fn line_to(&self, x: f64, y: f64) {
            self.fold_x(x);
            self.fold_y(y);
        }
        fn rect(&self, x: f64, y: f64, w: f64, h: f64) {
            self.fold_x(x);
            self.fold_y(y);
            self.fold_x(x + w);
            self.fold_y(y + h);
        }
        fn arc(&self, x: f64, y: f64, _r: f64, _s: f64, _e: f64) -> Result<(), JsValue> {
            self.fold_x(x);
            self.fold_y(y);
            Ok(())
        }
        fn arc_with_anticlockwise(&self, x: f64, y: f64, _r: f64, _s: f64, _e: f64, _a: bool) -> Result<(), JsValue> {
            self.fold_x(x);
            self.fold_y(y);
            Ok(())
        }
        fn ellipse_with_anticlockwise(&self, x: f64, y: f64, _rx: f64, _ry: f64, _rot: f64, _s: f64, _e: f64, _a: bool) -> Result<(), JsValue> {
            self.fold_x(x);
            self.fold_y(y);
            Ok(())
        }
        fn quadratic_curve_to(&self, cpx: f64, cpy: f64, x: f64, y: f64) {
            self.fold_x(cpx);
            self.fold_y(cpy);
            self.fold_x(x);
            self.fold_y(y);
        }
        fn bezier_curve_to(&self, cp1x: f64, cp1y: f64, cp2x: f64, cp2y: f64, x: f64, y: f64) {
            self.fold_x(cp1x);
            self.fold_y(cp1y);
            self.fold_x(cp2x);
            self.fold_y(cp2y);
            self.fold_x(x);
            self.fold_y(y);
        }
        fn fill(&self) {}
        fn stroke(&self) {}
        fn clip(&self) {}
        fn fill_rect(&self, x: f64, y: f64, w: f64, h: f64) {
            self.fold_x(x);
            self.fold_y(y);
            self.fold_x(x + w);
            self.fold_y(y + h);
        }
        fn stroke_rect(&self, x: f64, y: f64, w: f64, h: f64) {
            self.fold_x(x);
            self.fold_y(y);
            self.fold_x(x + w);
            self.fold_y(y + h);
        }
        fn fill_text(&self, _t: &str, x: f64, y: f64) -> Result<(), JsValue> {
            self.fold_x(x);
            self.fold_y(y);
            Ok(())
        }
        fn stroke_text(&self, _t: &str, x: f64, y: f64) -> Result<(), JsValue> {
            self.fold_x(x);
            self.fold_y(y);
            Ok(())
        }
        fn set_font(&self, _v: &str) {}
        fn set_text_align(&self, _v: &str) {}
        fn set_text_baseline(&self, _v: &str) {}
        fn set_fill_style_str(&self, _v: &str) {}
        fn set_stroke_style_str(&self, _v: &str) {}
        fn set_line_width(&self, _v: f64) {}
        fn set_global_alpha(&self, _v: f64) {}
        fn set_shadow_color(&self, _v: &str) {}
        fn set_shadow_offset_x(&self, _v: f64) {}
        fn set_shadow_offset_y(&self, _v: f64) {}
        fn set_shadow_blur(&self, _v: f64) {}
        fn translate(&self, _x: f64, _y: f64) -> Result<(), JsValue> {
            Ok(())
        }
        fn rotate(&self, _a: f64) -> Result<(), JsValue> {
            Ok(())
        }
        fn scale(&self, _x: f64, _y: f64) -> Result<(), JsValue> {
            Ok(())
        }
        fn set_transform(&self, _a: f64, _b: f64, _c: f64, _d: f64, _e: f64, _f: f64) -> Result<(), JsValue> {
            Ok(())
        }
        fn draw_image(&self, _key: &str, dx: f64, dy: f64, dw: f64, dh: f64) -> Result<(), JsValue> {
            // Mirror CanvasPainter's bitmap-MISS branch: re-enter the placeholder
            // on SELF (the inner painter). If the offset lived on the concrete
            // painter this re-entry would double-subtract; here it does not.
            super::super::images::draw_image_placeholder(self, dx, dy, dw, dh)
        }
        fn measure_text_width(&self, _t: &str) -> f64 {
            0.0
        }
        fn set_linear_gradient(&self, x0: f64, y0: f64, x1: f64, y1: f64, _s: &[(f64, String)]) -> Result<(), JsValue> {
            self.fold_x(x0);
            self.fold_y(y0);
            self.fold_x(x1);
            self.fold_y(y1);
            Ok(())
        }
        fn set_radial_gradient(&self, x0: f64, y0: f64, _r0: f64, x1: f64, y1: f64, _r1: f64, _s: &[(f64, String)]) -> Result<(), JsValue> {
            self.fold_x(x0);
            self.fold_y(y0);
            self.fold_x(x1);
            self.fold_y(y1);
            Ok(())
        }
        fn set_pattern_fill(&self, _k: &str, _t: &crate::engine::pattern::PatternTile, _fg: &str, _bg: &str) -> Result<(), JsValue> {
            Ok(())
        }
        fn set_line_dash(&self, _s: &[f64]) {}
        fn clear_line_dash(&self) {}
    }

    /// Drive a representative page paint (background + absolute content + a
    /// watermark-style `save;translate;rotate;centred fill_text` + an anchored
    /// image MISS that re-enters the placeholder) through `inner`, using `off` as
    /// the window origin and `bg` as the (already window-reduced) background rect.
    /// Returns the max absolute coordinate the inner painter saw.
    fn drive(off: (f64, f64), bg: Option<(f64, f64, f64, f64)>, page_w: f64, page_h: f64) -> f64 {
        let cap = CapturingPainter::new();
        let p = OffsetPainter::new(&cap, off.0, off.1);

        // 1. Background (the one page-spanning fill — MUST be window-reduced).
        if let Some((bx, by, bw, bh)) = bg {
            p.set_fill_style_str("#ffffff");
            p.fill_rect(bx, by, bw, bh);
        }
        // 2. Absolute root-frame content near the window (deep down the page).
        p.fill_text("body", off.0 + 5.0, off.1 + 10.0).unwrap();
        p.fill_rect(off.0 + 2.0, off.1 + 2.0, 40.0, 12.0);
        // 3. Watermark: page-centred save/translate/rotate/centred text. The
        //    transition rule consumes the offset in translate; the centred local
        //    coords (0,0) must NOT be re-offset.
        p.save();
        let _ = p.translate(page_w / 2.0, page_h / 2.0);
        let _ = p.rotate(0.5);
        p.fill_text("WATERMARK", 0.0, 0.0).unwrap();
        p.restore();
        // 4. Anchored image MISS near the window → placeholder re-entry.
        let _ = p.draw_image("missing-rel", off.0 + 3.0, off.1 + 4.0, 30.0, 20.0);

        cap.max_abs()
    }

    /// GREEN: with the window-local offset (= window origin) and the background
    /// reduced to the window, every emitted coordinate — including the watermark
    /// (translated to the page centre) and the placeholder re-entry — lands within
    /// the SLICE, bounded by the window size, NOT the page height. This is the
    /// pure-f64 invariant that makes `device = s·coord` stay under the Skia limit.
    #[test]
    fn high_zoom_window_coordinate_bounds() {
        // US-Letter page; a tiny window near the page BOTTOM simulating deep clip
        // at high zoom (≈30 logical px tall at the very bottom).
        let (page_w, page_h) = (816.0_f64, 1056.0_f64);
        let win = crate::cull::Rect { x0: 0.0, y0: 1020.0, x1: 40.0, y1: 1050.0 };
        let win_w = win.x1 - win.x0;
        let win_h = win.y1 - win.y0;
        let bg = crate::cull::bg_rect(Some(&win), page_w, page_h);

        // Background must land inside [0,win_w]×[0,win_h] window-local.
        // bg_rect returns (win.x0, win.y0, min(win_w,·), min(win_h,·)); after the
        // offset subtracts (win.x0,win.y0) the fill is at (0,0,≤win_w,≤win_h).
        let (bx, by, bw, bh) = bg.expect("window intersects page");
        assert!((bx - win.x0).abs() < 1e-9 && (by - win.y0).abs() < 1e-9);
        assert!(bw <= win_w + 1e-9 && bh <= win_h + 1e-9);

        let bound = drive((win.x0, win.y0), bg, page_w, page_h);
        // Slice size + generous single-item slack (font ascent, image box, the
        // watermark string width). Crucially FAR below the page height (1056).
        let slack = 300.0;
        assert!(
            bound <= win_w.max(win_h) + slack,
            "windowed coords exceeded the slice: max_abs={bound}, win={win_w}x{win_h}"
        );
        assert!(bound < page_h, "windowed coords must not reach page height (device would overflow Skia)");
    }

    /// RED (documents the pre-fix mechanism, permanently): the OLD approach —
    /// NO offset (off=(0,0), like the giant CTM translate that left absolute page
    /// coords in place) with the background at FULL page dims — emits coordinates
    /// up to the PAGE height. At high zoom `device = s·page_h` blows past the Skia
    /// ~16384 px limit and Skia drops the draws (the white block). This asserts the
    /// pre-fix parameters VIOLATE the slice bound, so the green test above is a
    /// genuine regression guard, not a vacuous one.
    #[test]
    fn pre_fix_form_violates_slice_bound() {
        let (page_w, page_h) = (816.0_f64, 1056.0_f64);
        let win = crate::cull::Rect { x0: 0.0, y0: 1020.0, x1: 40.0, y1: 1050.0 };
        let win_w = win.x1 - win.x0;
        let win_h = win.y1 - win.y0;

        // Pre-fix: off=(0,0) (no subtraction) + FULL-page background.
        let bound = drive((0.0, 0.0), Some((0.0, 0.0, page_w, page_h)), page_w, page_h);
        assert!(
            bound > win_w.max(win_h) + 300.0,
            "pre-fix form should exceed the slice bound but got {bound}"
        );
        // Specifically reaches the page height — the coordinate that overflows.
        assert!(bound >= page_h - 1.0, "pre-fix background should reach page height, got {bound}");
    }

    /// Worker record path: `OffsetPainter` wrapping a real `RecordingPainter`
    /// (the exact composition `renderPageWindowDisplayList` builds) records
    /// WINDOW-LOCAL ops for a deep clip — the recorded background `FillRect` and a
    /// deep content `FillText` land near the window origin, NOT at page depth, so
    /// the main-thread scale-only replay stays under the Skia limit. Covers the
    /// worker render path, not only the in-process CanvasPainter.
    #[test]
    fn windowed_recording_ops_are_window_local() {
        use crate::engine::displaylist::{DrawCmd, RecordingPainter};
        let (page_w, page_h) = (816.0_f64, 1056.0_f64);
        let win = crate::cull::Rect { x0: 0.0, y0: 1020.0, x1: 40.0, y1: 1050.0 };
        let win_h = win.y1 - win.y0;

        let rec = RecordingPainter::new();
        {
            let p = OffsetPainter::new(&rec, win.x0, win.y0);
            // Background reduced to window ∩ page, then deep content + a placeholder
            // key (RecordingPainter records the key; no re-entry — but the point is
            // the recorded COORD is window-local).
            let (bx, by, bw, bh) = crate::cull::bg_rect(Some(&win), page_w, page_h).unwrap();
            p.fill_rect(bx, by, bw, bh);
            p.fill_text("deep", win.x0 + 5.0, win.y0 + 12.0).unwrap();
        }
        let list = rec.take();
        for cmd in &list {
            match cmd {
                DrawCmd::FillRect { y, h, .. } => {
                    assert!(*y >= -1e-9 && *y + *h <= win_h + 1e-9, "bg FillRect not window-local: y={y} h={h}");
                }
                DrawCmd::FillText { y, .. } => {
                    assert!(*y >= -1e-9 && *y <= win_h + 50.0, "deep FillText not window-local: y={y}");
                }
                _ => {}
            }
        }
    }

    /// Byte-identity guard: with a ZERO offset AND the full-page background (the
    /// `clip == None` full-page path), the `OffsetPainter` subtraction is `x-0.0`,
    /// a no-op — the inner painter sees the exact page-logical coordinates. (The
    /// full-page path does not wrap the decorator at all, but this pins that a
    /// zero offset is an identity, should any future caller wrap with off=0.)
    #[test]
    fn zero_offset_is_identity() {
        let cap = CapturingPainter::new();
        let p = OffsetPainter::new(&cap, 0.0, 0.0);
        p.fill_rect(72.0, 900.0, 100.0, 20.0);
        p.fill_text("x", 72.0, 918.0).unwrap();
        // Inner saw the untouched coords (900, 918, 920) — no subtraction.
        assert_eq!(cap.max_x.get(), 172.0);
        assert_eq!(cap.max_y.get(), 920.0);
    }
}
