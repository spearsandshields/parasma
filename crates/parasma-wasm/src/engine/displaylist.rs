//! The display-list recording backend.
//!
//! `RecordingPainter` is the second `Painter` impl (alongside `CanvasPainter`):
//! every draw call pushes a `DrawCmd` onto an ordered list instead of touching a
//! canvas. Off-main-thread rendering runs the render code against a
//! `RecordingPainter` INSIDE a Web Worker (which has no `CanvasRenderingContext2d`)
//! and ships the resulting `DisplayList` to the main thread, which REPLAYS it
//! onto the real (GPU-backed) `<canvas>` via the browser's 2D API. Recording is
//! the worker's whole job; replay keeps the GPU and the font set on the main
//! thread (no font fork).
//!
//! This module is web_sys-free and natively testable. `DrawCmd` mirrors the
//! `Painter` primitive alphabet 1:1. The over-the-wire ENCODING (a flat
//! `Float64Array` opcode stream + a string table, chosen over serde to avoid the
//! dependency's gzip cost and to make replay an index-based switch) is defined
//! below (`encode_display_list`, opcode contract) — `DrawCmd` is the in-memory IR; the
//! wire format is a separate layer over it.
//!
//! The image draws record only the KEY: the worker holds no bitmaps, so the
//! present-vs-placeholder decision is deferred to the main-thread replay (which
//! owns the bitmap cache). `measure_text_width` returns 0.0 — the only sync
//! browser query, used by the watermark, whose sizing is resolved main-thread.

use std::cell::RefCell;

use wasm_bindgen::JsValue;

use super::paint::Painter;
use super::pattern::TilePrim;

/// One recorded draw primitive. Variants mirror the `Painter` trait methods 1:1
/// (a `Set*` for each style mutation, the path ops, the rects, the text runs, the
/// transform ops, and the key-only image draw). Plain data — no web_sys, no
/// serde — so it records in a worker and unit-tests natively.
// Ellipse / QuadraticCurveTo / BezierCurveTo / SetTransform are a complete
// display-list vocabulary; several variants are only emitted by the (currently
// dead) curve/transform Painter methods, kept for off-thread render parity.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DrawCmd {
    // state stack
    Save,
    Restore,
    // paths
    BeginPath,
    ClosePath,
    MoveTo { x: f64, y: f64 },
    LineTo { x: f64, y: f64 },
    Rect { x: f64, y: f64, w: f64, h: f64 },
    Arc { x: f64, y: f64, radius: f64, start: f64, end: f64 },
    ArcAnticlockwise { x: f64, y: f64, radius: f64, start: f64, end: f64, anticlockwise: bool },
    Ellipse { x: f64, y: f64, rx: f64, ry: f64, rotation: f64, start: f64, end: f64, anticlockwise: bool },
    QuadraticCurveTo { cpx: f64, cpy: f64, x: f64, y: f64 },
    BezierCurveTo { cp1x: f64, cp1y: f64, cp2x: f64, cp2y: f64, x: f64, y: f64 },
    Fill,
    Stroke,
    Clip,
    // rects
    FillRect { x: f64, y: f64, w: f64, h: f64 },
    StrokeRect { x: f64, y: f64, w: f64, h: f64 },
    // text
    FillText { text: String, x: f64, y: f64 },
    StrokeText { text: String, x: f64, y: f64 },
    // style state
    SetFont(String),
    SetTextAlign(String),
    SetTextBaseline(String),
    SetFillStyle(String),
    SetStrokeStyle(String),
    SetLineWidth(f64),
    SetGlobalAlpha(f64),
    SetShadowColor(String),
    SetShadowOffsetX(f64),
    SetShadowOffsetY(f64),
    SetShadowBlur(f64),
    // transform
    Translate { x: f64, y: f64 },
    Rotate { angle: f64 },
    Scale { x: f64, y: f64 },
    SetTransform { a: f64, b: f64, c: f64, d: f64, e: f64, f: f64 },
    // images (key only — resolved at main-thread replay)
    DrawImage { key: String, dx: f64, dy: f64, dw: f64, dh: f64 },
    // PML style primitives. Gradients carry inline `(pos, colour)`
    // stops; the pattern carries its rasterisable tile (fg/bg + `TilePrim`s) so
    // replay re-rasterises the SAME offscreen tile the `CanvasPainter` builds.
    SetLinearGradient { x0: f64, y0: f64, x1: f64, y1: f64, stops: Vec<(f64, String)> },
    SetRadialGradient { x0: f64, y0: f64, r0: f64, x1: f64, y1: f64, r1: f64, stops: Vec<(f64, String)> },
    SetPatternFill { key: String, fg: String, bg: String, size: f64, prims: Vec<TilePrim> },
    SetLineDash(Vec<f64>),
    ClearLineDash,
    /// `fontKerning` on (`true`) or off.
    SetFontKerning(bool),
    /// Part of an image (key only, like `DrawImage`): the source in
    /// fractions of the bitmap's size, resolved to pixels at replay.
    DrawImageCrop { key: String, sx: f64, sy: f64, sw: f64, sh: f64, dx: f64, dy: f64, dw: f64, dh: f64 },
}

/// An ordered list of draw commands for one render (page / window / grid slice).
pub(crate) type DisplayList = Vec<DrawCmd>;

// ── Flat wire encoding (Rust encoder ↔ TS decoder CONTRACT) ─────────────────-
//
// A `DisplayList` crosses wasm→JS as TWO arrays: a flat `Vec<f64>` opcode stream
// and a deduplicated `Vec<String>` string table. Each command encodes as
// `[OPCODE, arg, arg, …]`; string args are encoded as their f64 INDEX into the
// string table; bool args as 0.0/1.0. The TS `replayDisplayList` decoder MUST use
// the IDENTICAL opcode numbers + arg order below (it walks the f64 stream, reads
// an opcode, then the fixed arg count for that opcode). Chosen over serde to
// spare the gzip budget and to make replay an index-based switch (no per-command
// allocation). Opcodes are append-only — never renumber an existing one.
pub(crate) const OP_SAVE: f64 = 0.0;
pub(crate) const OP_RESTORE: f64 = 1.0;
pub(crate) const OP_BEGIN_PATH: f64 = 2.0;
pub(crate) const OP_CLOSE_PATH: f64 = 3.0;
pub(crate) const OP_MOVE_TO: f64 = 4.0;
pub(crate) const OP_LINE_TO: f64 = 5.0;
pub(crate) const OP_RECT: f64 = 6.0;
pub(crate) const OP_ARC: f64 = 7.0;
pub(crate) const OP_ARC_ACW: f64 = 8.0;
pub(crate) const OP_ELLIPSE: f64 = 9.0;
pub(crate) const OP_QUAD_TO: f64 = 10.0;
pub(crate) const OP_BEZIER_TO: f64 = 11.0;
pub(crate) const OP_FILL: f64 = 12.0;
pub(crate) const OP_STROKE: f64 = 13.0;
pub(crate) const OP_CLIP: f64 = 14.0;
pub(crate) const OP_FILL_RECT: f64 = 15.0;
pub(crate) const OP_STROKE_RECT: f64 = 16.0;
pub(crate) const OP_FILL_TEXT: f64 = 17.0;
pub(crate) const OP_STROKE_TEXT: f64 = 18.0;
pub(crate) const OP_SET_FONT: f64 = 19.0;
pub(crate) const OP_SET_TEXT_ALIGN: f64 = 20.0;
pub(crate) const OP_SET_TEXT_BASELINE: f64 = 21.0;
pub(crate) const OP_SET_FILL_STYLE: f64 = 22.0;
pub(crate) const OP_SET_STROKE_STYLE: f64 = 23.0;
pub(crate) const OP_SET_LINE_WIDTH: f64 = 24.0;
pub(crate) const OP_SET_GLOBAL_ALPHA: f64 = 25.0;
pub(crate) const OP_SET_SHADOW_COLOR: f64 = 26.0;
pub(crate) const OP_SET_SHADOW_OFFSET_X: f64 = 27.0;
pub(crate) const OP_SET_SHADOW_OFFSET_Y: f64 = 28.0;
pub(crate) const OP_SET_SHADOW_BLUR: f64 = 29.0;
pub(crate) const OP_TRANSLATE: f64 = 30.0;
pub(crate) const OP_ROTATE: f64 = 31.0;
pub(crate) const OP_SCALE: f64 = 32.0;
pub(crate) const OP_SET_TRANSFORM: f64 = 33.0;
pub(crate) const OP_DRAW_IMAGE: f64 = 34.0;
// PML style primitives — the FIRST variable-length opcodes (count-prefixed).
pub(crate) const OP_SET_LINEAR_GRADIENT: f64 = 35.0;
pub(crate) const OP_SET_RADIAL_GRADIENT: f64 = 36.0;
pub(crate) const OP_SET_PATTERN_FILL: f64 = 37.0;
pub(crate) const OP_SET_LINE_DASH: f64 = 38.0;
pub(crate) const OP_CLEAR_LINE_DASH: f64 = 39.0;
/// `fontKerning`: 1 = "normal", 0 = "none".
pub(crate) const OP_SET_FONT_KERNING: f64 = 40.0;
/// `key, sx, sy, sw, sh, dx, dy, dw, dh`: the 9-argument `drawImage` order,
/// the source in fractions of the bitmap's size.
pub(crate) const OP_DRAW_IMAGE_CROP: f64 = 41.0;
// pattern-tile sub-tags (module-private; discriminate the count-prefixed prims).
const TILE_LINE: f64 = 0.0;
const TILE_DOT: f64 = 1.0;

/// Encode a display list to the flat wire form: `(opcodes, string_table)`.
/// Strings are deduplicated (a fill style / font repeated across a page collapses
/// to one table entry) to shrink the transfer. See the opcode contract above.
pub(crate) fn encode_display_list(list: &DisplayList) -> (Vec<f64>, Vec<String>) {
    let mut ops: Vec<f64> = Vec::with_capacity(list.len() * 4);
    let mut strings: Vec<String> = Vec::new();
    let mut interned: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
    // Intern a string, returning its table index as f64.
    macro_rules! s {
        ($v:expr) => {{
            let key: &str = $v;
            *interned.entry(key).or_insert_with(|| {
                let idx = strings.len() as f64;
                strings.push(key.to_owned());
                idx
            })
        }};
    }
    for cmd in list {
        match cmd {
            DrawCmd::Save => ops.push(OP_SAVE),
            DrawCmd::Restore => ops.push(OP_RESTORE),
            DrawCmd::BeginPath => ops.push(OP_BEGIN_PATH),
            DrawCmd::ClosePath => ops.push(OP_CLOSE_PATH),
            DrawCmd::MoveTo { x, y } => ops.extend_from_slice(&[OP_MOVE_TO, *x, *y]),
            DrawCmd::LineTo { x, y } => ops.extend_from_slice(&[OP_LINE_TO, *x, *y]),
            DrawCmd::Rect { x, y, w, h } => ops.extend_from_slice(&[OP_RECT, *x, *y, *w, *h]),
            DrawCmd::Arc { x, y, radius, start, end } => {
                ops.extend_from_slice(&[OP_ARC, *x, *y, *radius, *start, *end])
            }
            DrawCmd::ArcAnticlockwise { x, y, radius, start, end, anticlockwise } => ops
                .extend_from_slice(&[
                    OP_ARC_ACW, *x, *y, *radius, *start, *end, bool_f64(*anticlockwise),
                ]),
            DrawCmd::Ellipse { x, y, rx, ry, rotation, start, end, anticlockwise } => ops
                .extend_from_slice(&[
                    OP_ELLIPSE, *x, *y, *rx, *ry, *rotation, *start, *end, bool_f64(*anticlockwise),
                ]),
            DrawCmd::QuadraticCurveTo { cpx, cpy, x, y } => {
                ops.extend_from_slice(&[OP_QUAD_TO, *cpx, *cpy, *x, *y])
            }
            DrawCmd::BezierCurveTo { cp1x, cp1y, cp2x, cp2y, x, y } => {
                ops.extend_from_slice(&[OP_BEZIER_TO, *cp1x, *cp1y, *cp2x, *cp2y, *x, *y])
            }
            DrawCmd::Fill => ops.push(OP_FILL),
            DrawCmd::Stroke => ops.push(OP_STROKE),
            DrawCmd::Clip => ops.push(OP_CLIP),
            DrawCmd::FillRect { x, y, w, h } => ops.extend_from_slice(&[OP_FILL_RECT, *x, *y, *w, *h]),
            DrawCmd::StrokeRect { x, y, w, h } => {
                ops.extend_from_slice(&[OP_STROKE_RECT, *x, *y, *w, *h])
            }
            DrawCmd::FillText { text, x, y } => {
                ops.extend_from_slice(&[OP_FILL_TEXT, s!(text), *x, *y])
            }
            DrawCmd::StrokeText { text, x, y } => {
                ops.extend_from_slice(&[OP_STROKE_TEXT, s!(text), *x, *y])
            }
            DrawCmd::SetFont(v) => ops.extend_from_slice(&[OP_SET_FONT, s!(v)]),
            DrawCmd::SetTextAlign(v) => ops.extend_from_slice(&[OP_SET_TEXT_ALIGN, s!(v)]),
            DrawCmd::SetTextBaseline(v) => ops.extend_from_slice(&[OP_SET_TEXT_BASELINE, s!(v)]),
            DrawCmd::SetFillStyle(v) => ops.extend_from_slice(&[OP_SET_FILL_STYLE, s!(v)]),
            DrawCmd::SetStrokeStyle(v) => ops.extend_from_slice(&[OP_SET_STROKE_STYLE, s!(v)]),
            DrawCmd::SetLineWidth(v) => ops.extend_from_slice(&[OP_SET_LINE_WIDTH, *v]),
            DrawCmd::SetGlobalAlpha(v) => ops.extend_from_slice(&[OP_SET_GLOBAL_ALPHA, *v]),
            DrawCmd::SetShadowColor(v) => ops.extend_from_slice(&[OP_SET_SHADOW_COLOR, s!(v)]),
            DrawCmd::SetShadowOffsetX(v) => ops.extend_from_slice(&[OP_SET_SHADOW_OFFSET_X, *v]),
            DrawCmd::SetShadowOffsetY(v) => ops.extend_from_slice(&[OP_SET_SHADOW_OFFSET_Y, *v]),
            DrawCmd::SetShadowBlur(v) => ops.extend_from_slice(&[OP_SET_SHADOW_BLUR, *v]),
            DrawCmd::Translate { x, y } => ops.extend_from_slice(&[OP_TRANSLATE, *x, *y]),
            DrawCmd::Rotate { angle } => ops.extend_from_slice(&[OP_ROTATE, *angle]),
            DrawCmd::Scale { x, y } => ops.extend_from_slice(&[OP_SCALE, *x, *y]),
            DrawCmd::SetTransform { a, b, c, d, e, f } => {
                ops.extend_from_slice(&[OP_SET_TRANSFORM, *a, *b, *c, *d, *e, *f])
            }
            DrawCmd::DrawImage { key, dx, dy, dw, dh } => {
                ops.extend_from_slice(&[OP_DRAW_IMAGE, s!(key), *dx, *dy, *dw, *dh])
            }
            DrawCmd::SetLinearGradient { x0, y0, x1, y1, stops } => {
                ops.extend_from_slice(&[OP_SET_LINEAR_GRADIENT, *x0, *y0, *x1, *y1, stops.len() as f64]);
                for (pos, color) in stops {
                    ops.push(*pos);
                    ops.push(s!(color));
                }
            }
            DrawCmd::SetRadialGradient { x0, y0, r0, x1, y1, r1, stops } => {
                ops.extend_from_slice(&[
                    OP_SET_RADIAL_GRADIENT, *x0, *y0, *r0, *x1, *y1, *r1, stops.len() as f64,
                ]);
                for (pos, color) in stops {
                    ops.push(*pos);
                    ops.push(s!(color));
                }
            }
            DrawCmd::SetPatternFill { key, fg, bg, size, prims } => {
                ops.extend_from_slice(&[
                    OP_SET_PATTERN_FILL, s!(key), s!(fg), s!(bg), *size, prims.len() as f64,
                ]);
                for prim in prims {
                    match *prim {
                        TilePrim::Line { x1, y1, x2, y2, lw } => {
                            ops.extend_from_slice(&[TILE_LINE, x1, y1, x2, y2, lw]);
                        }
                        // `s: side` — the tile-dot field name `s` collides visually
                        // with the `s!` intern macro; bind it out to `side`.
                        TilePrim::Dot { x, y, s: side } => {
                            ops.extend_from_slice(&[TILE_DOT, x, y, side]);
                        }
                    }
                }
            }
            DrawCmd::SetLineDash(segs) => {
                ops.push(OP_SET_LINE_DASH);
                ops.push(segs.len() as f64);
                ops.extend_from_slice(segs);
            }
            DrawCmd::ClearLineDash => ops.push(OP_CLEAR_LINE_DASH),
            DrawCmd::SetFontKerning(on) => {
                ops.push(OP_SET_FONT_KERNING);
                ops.push(if *on { 1.0 } else { 0.0 });
            }
            DrawCmd::DrawImageCrop { key, sx, sy, sw, sh, dx, dy, dw, dh } => {
                ops.extend_from_slice(&[OP_DRAW_IMAGE_CROP, s!(key), *sx, *sy, *sw, *sh, *dx, *dy, *dw, *dh])
            }
        }
    }
    (ops, strings)
}

#[inline]
fn bool_f64(b: bool) -> f64 {
    if b { 1.0 } else { 0.0 }
}

/// A `Painter` that records its primitive stream instead of drawing. Interior
/// mutability (`RefCell`) keeps the trait `&self` so the render call sites stay
/// an exact match for the `CanvasPainter` path (no borrow-checker divergence).
pub(crate) struct RecordingPainter {
    cmds: RefCell<Vec<DrawCmd>>,
}

impl RecordingPainter {
    pub(crate) fn new() -> Self {
        Self { cmds: RefCell::new(Vec::new()) }
    }

    /// Take the recorded list, leaving the painter empty (one render → one list).
    pub(crate) fn take(self) -> DisplayList {
        self.cmds.into_inner()
    }

    /// Borrow the recorded list (for assertions without consuming the painter).
    #[cfg(test)]
    pub(crate) fn cmds(&self) -> std::cell::Ref<'_, Vec<DrawCmd>> {
        self.cmds.borrow()
    }

    #[inline]
    fn push(&self, cmd: DrawCmd) {
        self.cmds.borrow_mut().push(cmd);
    }
}

impl Default for RecordingPainter {
    fn default() -> Self {
        Self::new()
    }
}

impl Painter for RecordingPainter {
    fn save(&self) {
        self.push(DrawCmd::Save);
    }
    fn restore(&self) {
        self.push(DrawCmd::Restore);
    }
    fn begin_path(&self) {
        self.push(DrawCmd::BeginPath);
    }
    fn close_path(&self) {
        self.push(DrawCmd::ClosePath);
    }
    fn move_to(&self, x: f64, y: f64) {
        self.push(DrawCmd::MoveTo { x, y });
    }
    fn line_to(&self, x: f64, y: f64) {
        self.push(DrawCmd::LineTo { x, y });
    }
    fn rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.push(DrawCmd::Rect { x, y, w, h });
    }
    fn arc(&self, x: f64, y: f64, radius: f64, start_angle: f64, end_angle: f64) -> Result<(), JsValue> {
        self.push(DrawCmd::Arc { x, y, radius, start: start_angle, end: end_angle });
        Ok(())
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
        self.push(DrawCmd::ArcAnticlockwise {
            x,
            y,
            radius,
            start: start_angle,
            end: end_angle,
            anticlockwise,
        });
        Ok(())
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
        self.push(DrawCmd::Ellipse {
            x,
            y,
            rx: radius_x,
            ry: radius_y,
            rotation,
            start: start_angle,
            end: end_angle,
            anticlockwise,
        });
        Ok(())
    }
    fn quadratic_curve_to(&self, cpx: f64, cpy: f64, x: f64, y: f64) {
        self.push(DrawCmd::QuadraticCurveTo { cpx, cpy, x, y });
    }
    fn bezier_curve_to(&self, cp1x: f64, cp1y: f64, cp2x: f64, cp2y: f64, x: f64, y: f64) {
        self.push(DrawCmd::BezierCurveTo { cp1x, cp1y, cp2x, cp2y, x, y });
    }
    fn fill(&self) {
        self.push(DrawCmd::Fill);
    }
    fn stroke(&self) {
        self.push(DrawCmd::Stroke);
    }
    fn clip(&self) {
        self.push(DrawCmd::Clip);
    }
    fn fill_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.push(DrawCmd::FillRect { x, y, w, h });
    }
    fn stroke_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.push(DrawCmd::StrokeRect { x, y, w, h });
    }
    fn fill_text(&self, text: &str, x: f64, y: f64) -> Result<(), JsValue> {
        self.push(DrawCmd::FillText { text: text.to_owned(), x, y });
        Ok(())
    }
    fn stroke_text(&self, text: &str, x: f64, y: f64) -> Result<(), JsValue> {
        self.push(DrawCmd::StrokeText { text: text.to_owned(), x, y });
        Ok(())
    }
    fn set_font(&self, value: &str) {
        self.push(DrawCmd::SetFont(value.to_owned()));
    }
    fn set_text_align(&self, value: &str) {
        self.push(DrawCmd::SetTextAlign(value.to_owned()));
    }
    fn set_text_baseline(&self, value: &str) {
        self.push(DrawCmd::SetTextBaseline(value.to_owned()));
    }
    fn set_fill_style_str(&self, value: &str) {
        self.push(DrawCmd::SetFillStyle(value.to_owned()));
    }
    fn set_stroke_style_str(&self, value: &str) {
        self.push(DrawCmd::SetStrokeStyle(value.to_owned()));
    }
    fn set_line_width(&self, value: f64) {
        self.push(DrawCmd::SetLineWidth(value));
    }
    fn set_global_alpha(&self, value: f64) {
        self.push(DrawCmd::SetGlobalAlpha(value));
    }
    fn set_shadow_color(&self, value: &str) {
        self.push(DrawCmd::SetShadowColor(value.to_owned()));
    }
    fn set_shadow_offset_x(&self, value: f64) {
        self.push(DrawCmd::SetShadowOffsetX(value));
    }
    fn set_shadow_offset_y(&self, value: f64) {
        self.push(DrawCmd::SetShadowOffsetY(value));
    }
    fn set_shadow_blur(&self, value: f64) {
        self.push(DrawCmd::SetShadowBlur(value));
    }
    fn translate(&self, x: f64, y: f64) -> Result<(), JsValue> {
        self.push(DrawCmd::Translate { x, y });
        Ok(())
    }
    fn rotate(&self, angle: f64) -> Result<(), JsValue> {
        self.push(DrawCmd::Rotate { angle });
        Ok(())
    }
    fn scale(&self, x: f64, y: f64) -> Result<(), JsValue> {
        self.push(DrawCmd::Scale { x, y });
        Ok(())
    }
    fn set_transform(&self, a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Result<(), JsValue> {
        self.push(DrawCmd::SetTransform { a, b, c, d, e, f });
        Ok(())
    }
    fn draw_image(&self, key: &str, dx: f64, dy: f64, dw: f64, dh: f64) -> Result<(), JsValue> {
        // Record the key only — the worker holds no bitmaps. The main-thread
        // replay resolves it (drawing the bitmap, or the placeholder on a miss).
        self.push(DrawCmd::DrawImage { key: key.to_owned(), dx, dy, dw, dh });
        Ok(())
    }
    fn draw_image_crop(&self, key: &str, sx: f64, sy: f64, sw: f64, sh: f64, dx: f64, dy: f64, dw: f64, dh: f64) -> Result<(), JsValue> {
        self.push(DrawCmd::DrawImageCrop { key: key.to_owned(), sx, sy, sw, sh, dx, dy, dw, dh });
        Ok(())
    }
    fn measure_text_width(&self, _text: &str) -> f64 {
        // No recording equivalent; watermark sizing is resolved main-thread.
        0.0
    }

    fn set_linear_gradient(&self, x0: f64, y0: f64, x1: f64, y1: f64, stops: &[(f64, String)]) -> Result<(), JsValue> {
        self.push(DrawCmd::SetLinearGradient { x0, y0, x1, y1, stops: stops.to_vec() });
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
        self.push(DrawCmd::SetRadialGradient { x0, y0, r0, x1, y1, r1, stops: stops.to_vec() });
        Ok(())
    }
    fn set_pattern_fill(&self, key: &str, tile: &crate::engine::pattern::PatternTile, fg: &str, bg: &str) -> Result<(), JsValue> {
        // Encode the tile PRIMITIVES (never a `CanvasPattern` — that is not `Send`
        // and is main-thread-only); replay re-rasterises them. The recorder ALWAYS
        // records a mapped preset (it has no `createPattern` to fail), so a replay
        // surface-miss must degrade to the SAME fg/bg blend as the in-process Err.
        self.push(DrawCmd::SetPatternFill {
            key: key.to_owned(),
            fg: fg.to_owned(),
            bg: bg.to_owned(),
            size: tile.size,
            prims: tile.prims.clone(),
        });
        Ok(())
    }
    fn set_line_dash(&self, segments: &[f64]) {
        self.push(DrawCmd::SetLineDash(segments.to_vec()));
    }
    fn clear_line_dash(&self) {
        self.push(DrawCmd::ClearLineDash);
    }
    fn set_font_kerning(&self, on: bool) {
        self.push(DrawCmd::SetFontKerning(on));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::paint::{Painter, RunStyle};

    fn plain_style(color: &str) -> RunStyle {
        RunStyle {
            font_pt: 11.0,
            family: "Calibri".into(),
            family_east_asia: "Calibri".into(),
            family_complex: "Calibri".into(),
            color: color.into(),
            ..Default::default()
        }
    }

    #[test]
    fn records_primitives_in_order() {
        let p = RecordingPainter::new();
        p.save();
        p.set_fill_style_str("#FFE066");
        p.fill_rect(1.0, 2.0, 3.0, 4.0);
        p.draw_image("rId7", 10.0, 20.0, 30.0, 40.0).unwrap();
        p.restore();

        assert_eq!(
            *p.cmds(),
            vec![
                DrawCmd::Save,
                DrawCmd::SetFillStyle("#FFE066".into()),
                DrawCmd::FillRect { x: 1.0, y: 2.0, w: 3.0, h: 4.0 },
                DrawCmd::DrawImage { key: "rId7".into(), dx: 10.0, dy: 20.0, dw: 30.0, dh: 40.0 },
                DrawCmd::Restore,
            ]
        );
    }

    #[test]
    fn search_highlight_records_alpha_color_rect_sequence() {
        // The search-highlight chrome's exact primitive sequence (matches the
        // SEARCH_FILL_* / SEARCH_ACTIVE_* constants painted in lib.rs render_page)
        // — proves the canary's highlight is captured into the display list.
        let p = RecordingPainter::new();
        p.set_global_alpha(0.40);
        p.set_fill_style_str("#FFE066");
        p.fill_rect(5.0, 6.0, 50.0, 12.0);
        let list = p.take();
        assert_eq!(
            list,
            vec![
                DrawCmd::SetGlobalAlpha(0.40),
                DrawCmd::SetFillStyle("#FFE066".into()),
                DrawCmd::FillRect { x: 5.0, y: 6.0, w: 50.0, h: 12.0 },
            ]
        );
    }

    #[test]
    fn glyph_run_default_method_records_fill_text() {
        // The `paint_glyph_run` default trait method drives the recording backend
        // through `self.*` — a plain run records a single FillText.
        let p = RecordingPainter::new();
        p.paint_glyph_run(&plain_style("#1a1a1a"), "Region", 12.0, 24.0).unwrap();
        assert_eq!(*p.cmds(), vec![DrawCmd::FillText { text: "Region".into(), x: 12.0, y: 24.0 }]);
    }

    /// Test-only reference decoder — walks the flat op stream back to DrawCmds,
    /// mirroring the opcode contract the TS `replayDisplayList` decoder follows.
    /// Round-tripping a list through encode→decode proves the encoding is lossless
    /// and self-consistent (the TS decoder is held to this same op→arity table).
    fn decode_display_list(ops: &[f64], strings: &[String]) -> DisplayList {
        let mut out = Vec::new();
        let mut i = 0usize;
        let st = |idx: f64| strings[idx as usize].clone();
        while i < ops.len() {
            let op = ops[i];
            i += 1;
            macro_rules! a {
                () => {{
                    let v = ops[i];
                    i += 1;
                    v
                }};
            }
            out.push(match op {
                OP_SAVE => DrawCmd::Save,
                OP_RESTORE => DrawCmd::Restore,
                OP_BEGIN_PATH => DrawCmd::BeginPath,
                OP_CLOSE_PATH => DrawCmd::ClosePath,
                OP_MOVE_TO => DrawCmd::MoveTo { x: a!(), y: a!() },
                OP_LINE_TO => DrawCmd::LineTo { x: a!(), y: a!() },
                OP_RECT => DrawCmd::Rect { x: a!(), y: a!(), w: a!(), h: a!() },
                OP_ARC => DrawCmd::Arc { x: a!(), y: a!(), radius: a!(), start: a!(), end: a!() },
                OP_ARC_ACW => DrawCmd::ArcAnticlockwise {
                    x: a!(), y: a!(), radius: a!(), start: a!(), end: a!(), anticlockwise: a!() != 0.0,
                },
                OP_ELLIPSE => DrawCmd::Ellipse {
                    x: a!(), y: a!(), rx: a!(), ry: a!(), rotation: a!(), start: a!(), end: a!(),
                    anticlockwise: a!() != 0.0,
                },
                OP_QUAD_TO => DrawCmd::QuadraticCurveTo { cpx: a!(), cpy: a!(), x: a!(), y: a!() },
                OP_BEZIER_TO => DrawCmd::BezierCurveTo {
                    cp1x: a!(), cp1y: a!(), cp2x: a!(), cp2y: a!(), x: a!(), y: a!(),
                },
                OP_FILL => DrawCmd::Fill,
                OP_STROKE => DrawCmd::Stroke,
                OP_CLIP => DrawCmd::Clip,
                OP_FILL_RECT => DrawCmd::FillRect { x: a!(), y: a!(), w: a!(), h: a!() },
                OP_STROKE_RECT => DrawCmd::StrokeRect { x: a!(), y: a!(), w: a!(), h: a!() },
                OP_FILL_TEXT => DrawCmd::FillText { text: st(a!()), x: a!(), y: a!() },
                OP_STROKE_TEXT => DrawCmd::StrokeText { text: st(a!()), x: a!(), y: a!() },
                OP_SET_FONT => DrawCmd::SetFont(st(a!())),
                OP_SET_TEXT_ALIGN => DrawCmd::SetTextAlign(st(a!())),
                OP_SET_TEXT_BASELINE => DrawCmd::SetTextBaseline(st(a!())),
                OP_SET_FILL_STYLE => DrawCmd::SetFillStyle(st(a!())),
                OP_SET_STROKE_STYLE => DrawCmd::SetStrokeStyle(st(a!())),
                OP_SET_LINE_WIDTH => DrawCmd::SetLineWidth(a!()),
                OP_SET_GLOBAL_ALPHA => DrawCmd::SetGlobalAlpha(a!()),
                OP_SET_SHADOW_COLOR => DrawCmd::SetShadowColor(st(a!())),
                OP_SET_SHADOW_OFFSET_X => DrawCmd::SetShadowOffsetX(a!()),
                OP_SET_SHADOW_OFFSET_Y => DrawCmd::SetShadowOffsetY(a!()),
                OP_SET_SHADOW_BLUR => DrawCmd::SetShadowBlur(a!()),
                OP_TRANSLATE => DrawCmd::Translate { x: a!(), y: a!() },
                OP_ROTATE => DrawCmd::Rotate { angle: a!() },
                OP_SCALE => DrawCmd::Scale { x: a!(), y: a!() },
                OP_SET_TRANSFORM => DrawCmd::SetTransform {
                    a: a!(), b: a!(), c: a!(), d: a!(), e: a!(), f: a!(),
                },
                OP_DRAW_IMAGE => DrawCmd::DrawImage {
                    key: st(a!()), dx: a!(), dy: a!(), dw: a!(), dh: a!(),
                },
                OP_SET_LINEAR_GRADIENT => {
                    let (x0, y0, x1, y1) = (a!(), a!(), a!(), a!());
                    let n = a!() as usize; // read count BEFORE the stop payload
                    let mut stops = Vec::with_capacity(n);
                    for _ in 0..n {
                        let pos = a!();
                        stops.push((pos, st(a!())));
                    }
                    DrawCmd::SetLinearGradient { x0, y0, x1, y1, stops }
                }
                OP_SET_RADIAL_GRADIENT => {
                    let (x0, y0, r0, x1, y1, r1) = (a!(), a!(), a!(), a!(), a!(), a!());
                    let n = a!() as usize;
                    let mut stops = Vec::with_capacity(n);
                    for _ in 0..n {
                        let pos = a!();
                        stops.push((pos, st(a!())));
                    }
                    DrawCmd::SetRadialGradient { x0, y0, r0, x1, y1, r1, stops }
                }
                OP_SET_PATTERN_FILL => {
                    let key = st(a!());
                    let fg = st(a!());
                    let bg = st(a!());
                    let size = a!();
                    let pc = a!() as usize;
                    let mut prims = Vec::with_capacity(pc);
                    for _ in 0..pc {
                        let tag = a!();
                        if tag == TILE_LINE {
                            prims.push(TilePrim::Line { x1: a!(), y1: a!(), x2: a!(), y2: a!(), lw: a!() });
                        } else {
                            prims.push(TilePrim::Dot { x: a!(), y: a!(), s: a!() });
                        }
                    }
                    DrawCmd::SetPatternFill { key, fg, bg, size, prims }
                }
                OP_SET_LINE_DASH => {
                    let n = a!() as usize;
                    let mut segs = Vec::with_capacity(n);
                    for _ in 0..n {
                        segs.push(a!());
                    }
                    DrawCmd::SetLineDash(segs)
                }
                OP_CLEAR_LINE_DASH => DrawCmd::ClearLineDash,
                OP_SET_FONT_KERNING => DrawCmd::SetFontKerning(a!() != 0.0),
                OP_DRAW_IMAGE_CROP => DrawCmd::DrawImageCrop {
                    key: st(a!()), sx: a!(), sy: a!(), sw: a!(), sh: a!(), dx: a!(), dy: a!(), dw: a!(), dh: a!(),
                },
                other => panic!("unknown opcode {other}"),
            });
        }
        out
    }

    #[test]
    fn encode_decode_round_trips_a_varied_list() {
        let list = vec![
            DrawCmd::Save,
            DrawCmd::SetTransform { a: 2.0, b: 0.0, c: 0.0, d: 2.0, e: -10.0, f: -20.0 },
            DrawCmd::SetFillStyle("#1a1a1a".into()),
            DrawCmd::SetFont("11px Calibri".into()),
            DrawCmd::FillText { text: "Region".into(), x: 12.0, y: 24.0 },
            DrawCmd::SetFillStyle("#1a1a1a".into()), // dup string → same table index
            DrawCmd::FillRect { x: 1.0, y: 2.0, w: 3.0, h: 4.0 },
            DrawCmd::BeginPath,
            DrawCmd::MoveTo { x: 0.5, y: 0.5 },
            DrawCmd::LineTo { x: 100.5, y: 0.5 },
            DrawCmd::Stroke,
            DrawCmd::Arc { x: 5.0, y: 6.0, radius: 7.0, start: 0.0, end: 6.28 },
            DrawCmd::ArcAnticlockwise { x: 1.0, y: 2.0, radius: 3.0, start: 0.0, end: 1.0, anticlockwise: true },
            DrawCmd::DrawImage { key: "rId7".into(), dx: 10.0, dy: 20.0, dw: 30.0, dh: 40.0 },
            DrawCmd::Restore,
        ];
        let (ops, strings) = encode_display_list(&list);
        // Dedup: "#1a1a1a" appears twice but the table holds it once.
        assert_eq!(strings.iter().filter(|s| *s == "#1a1a1a").count(), 1);
        assert_eq!(decode_display_list(&ops, &strings), list);
    }

    #[test]
    fn encode_decode_round_trips_pml_style_opcodes() {
        // The five PML style primitives, incl. the variable-length count edges: a
        // 2-stop linear, a 3-stop radial, a pattern (1 Line + 1 Dot), a dash, a
        // clear, and an EMPTY-stops linear (n=0 guard). A colour reused as BOTH a
        // solid fill and a gradient stop must intern to ONE table entry.
        let list = vec![
            DrawCmd::SetLinearGradient {
                x0: 0.0, y0: 0.0, x1: 100.0, y1: 50.0,
                stops: vec![(0.0, "#ff0000".into()), (1.0, "#0000ff".into())],
            },
            DrawCmd::SetRadialGradient {
                x0: 10.0, y0: 10.0, r0: 0.0, x1: 10.0, y1: 10.0, r1: 40.0,
                stops: vec![(0.0, "#ffffff".into()), (0.5, "#808080".into()), (1.0, "#000000".into())],
            },
            DrawCmd::SetPatternFill {
                key: "horz|#123456|#abcdef".into(),
                fg: "#123456".into(),
                bg: "#abcdef".into(),
                size: 8.0,
                prims: vec![
                    TilePrim::Line { x1: 0.0, y1: 4.0, x2: 8.0, y2: 4.0, lw: 1.0 },
                    TilePrim::Dot { x: 2.0, y: 2.0, s: 1.0 },
                ],
            },
            DrawCmd::SetLineDash(vec![4.0, 3.0, 1.0, 3.0]),
            DrawCmd::ClearLineDash,
            // n=0 edge: an empty-stops linear must round-trip (off-by-one guard).
            DrawCmd::SetLinearGradient { x0: 1.0, y0: 2.0, x1: 3.0, y1: 4.0, stops: vec![] },
            // dedup proof: "#ff0000" appeared as a gradient stop above; as a solid
            // fill here it interns to the SAME table entry (one row, like #1a1a1a).
            DrawCmd::SetFillStyle("#ff0000".into()),
        ];
        let (ops, strings) = encode_display_list(&list);
        assert_eq!(strings.iter().filter(|s| *s == "#ff0000").count(), 1);
        assert_eq!(decode_display_list(&ops, &strings), list);
    }

    #[test]
    fn outline_run_records_stroke_path() {
        // An outline run takes the stroke branch: set stroke style + width + stroke_text.
        let mut style = plain_style("#1a1a1a");
        style.outline = true;
        let p = RecordingPainter::new();
        p.paint_glyph_run(&style, "X", 0.0, 0.0).unwrap();
        let list = p.take();
        assert!(matches!(list[0], DrawCmd::SetStrokeStyle(_)));
        assert!(matches!(list[1], DrawCmd::SetLineWidth(_)));
        assert!(matches!(list[2], DrawCmd::StrokeText { .. }));
        assert_eq!(list.len(), 3);
    }

    // ── Cross-language wire golden ───────────────────────────────────────────
    //
    // The round trips above prove the encoder agrees with the Rust reference
    // decoder. Neither proves it agrees with the decoder that actually runs:
    // `replayDisplayList` in packages/parasma-core/src/replay.ts, whose opcode
    // numbers and argument order are a hand-kept copy of the ones in this file.
    // A wrong arity there desyncs the rest of the stream without an error.
    //
    // So one checked-in file is shared by both languages. This test writes it:
    // the opcode table, the encoded stream and string table of a list that holds
    // every `DrawCmd` variant, and each command's arguments in wire order.
    // `display-list-golden.test.ts` reads it, replays the stream into a logging
    // context and checks every call against the decoded list.

    /// The shared fixture. It sits in the TS package's `src/` so vitest imports
    /// it directly; the package publishes only `dist/`, so it never ships.
    const WIRE_GOLDEN: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages/parasma-core/src/display-list-golden.json");

    /// Build the coverage list, and refuse to compile until every `DrawCmd`
    /// variant has at least one sample. The generated `match` lists one arm
    /// per pattern with no wildcard, so a new variant is a non-exhaustive-match
    /// error here, not a gap in the golden.
    macro_rules! every_variant {
        ($($pat:pat => [$($sample:expr),+ $(,)?]),+ $(,)?) => {{
            #[allow(dead_code)]
            fn every_draw_cmd_variant_has_a_sample(cmd: &DrawCmd) {
                match cmd {
                    $($pat => {})+
                }
            }
            let mut list: DisplayList = Vec::new();
            $($(
                let sample: DrawCmd = $sample;
                assert!(matches!(sample, $pat), "sample {sample:?} is filed under the wrong variant");
                list.push(sample);
            )+)+
            list
        }};
    }

    /// Every variant, with values a swapped or dropped argument cannot hide in:
    /// distinct per slot, negative and fractional, both bool values, and the
    /// count-prefixed payloads at zero and non-zero length. `#1a1a1a` recurs as a
    /// fill, a stroke, a gradient stop and a pattern foreground, so the string
    /// table must dedup it. Each image op has one bitmap hit and one miss in the
    /// TS test (it supplies a bitmap for `rId7` only).
    fn wire_coverage() -> DisplayList {
        use std::f64::consts::{FRAC_PI_4, PI, TAU};
        every_variant! {
            DrawCmd::Save => [DrawCmd::Save],
            DrawCmd::Restore => [DrawCmd::Restore],
            DrawCmd::BeginPath => [DrawCmd::BeginPath],
            DrawCmd::ClosePath => [DrawCmd::ClosePath],
            DrawCmd::MoveTo { .. } => [DrawCmd::MoveTo { x: -1.5, y: 2.25 }],
            DrawCmd::LineTo { .. } => [DrawCmd::LineTo { x: 100.125, y: -0.75 }],
            DrawCmd::Rect { .. } => [DrawCmd::Rect { x: 3.0, y: -4.5, w: 50.25, h: 12.0 }],
            DrawCmd::Arc { .. } => [DrawCmd::Arc { x: 5.5, y: 6.5, radius: 7.25, start: 0.4, end: PI }],
            DrawCmd::ArcAnticlockwise { .. } => [
                DrawCmd::ArcAnticlockwise { x: 1.0, y: -2.0, radius: 3.5, start: -0.5, end: TAU, anticlockwise: true },
                DrawCmd::ArcAnticlockwise { x: 9.0, y: 8.0, radius: 0.25, start: 1.5, end: 2.5, anticlockwise: false },
            ],
            DrawCmd::Ellipse { .. } => [
                DrawCmd::Ellipse { x: 10.0, y: 20.0, rx: 30.5, ry: 4.75, rotation: FRAC_PI_4, start: 0.0, end: PI, anticlockwise: false },
                DrawCmd::Ellipse { x: -3.0, y: -6.0, rx: 1.0, ry: 2.0, rotation: -0.125, start: 0.5, end: 1.25, anticlockwise: true },
            ],
            DrawCmd::QuadraticCurveTo { .. } => [DrawCmd::QuadraticCurveTo { cpx: 1.25, cpy: -2.5, x: 3.75, y: 4.5 }],
            DrawCmd::BezierCurveTo { .. } => [
                DrawCmd::BezierCurveTo { cp1x: -1.0, cp1y: 2.0, cp2x: 3.5, cp2y: -4.5, x: 5.25, y: 6.75 },
            ],
            DrawCmd::Fill => [DrawCmd::Fill],
            DrawCmd::Stroke => [DrawCmd::Stroke],
            DrawCmd::Clip => [DrawCmd::Clip],
            DrawCmd::FillRect { .. } => [DrawCmd::FillRect { x: 1.5, y: 2.5, w: 3.5, h: -4.5 }],
            DrawCmd::StrokeRect { .. } => [DrawCmd::StrokeRect { x: -7.0, y: 8.25, w: 9.5, h: 10.75 }],
            DrawCmd::FillText { .. } => [DrawCmd::FillText { text: "Région \"q\" ₂".into(), x: 12.5, y: -24.0 }],
            DrawCmd::StrokeText { .. } => [DrawCmd::StrokeText { text: "Outline".into(), x: -0.5, y: 36.125 }],
            DrawCmd::SetFont(_) => [DrawCmd::SetFont("bold 11px Calibri".into())],
            DrawCmd::SetTextAlign(_) => [DrawCmd::SetTextAlign("right".into())],
            DrawCmd::SetTextBaseline(_) => [DrawCmd::SetTextBaseline("middle".into())],
            DrawCmd::SetFillStyle(_) => [DrawCmd::SetFillStyle("#1a1a1a".into())],
            DrawCmd::SetStrokeStyle(_) => [DrawCmd::SetStrokeStyle("#1a1a1a".into())],
            DrawCmd::SetLineWidth(_) => [DrawCmd::SetLineWidth(0.75)],
            DrawCmd::SetGlobalAlpha(_) => [DrawCmd::SetGlobalAlpha(0.4)],
            DrawCmd::SetShadowColor(_) => [DrawCmd::SetShadowColor("rgba(0, 0, 0, 0.5)".into())],
            DrawCmd::SetShadowOffsetX(_) => [DrawCmd::SetShadowOffsetX(-2.5)],
            DrawCmd::SetShadowOffsetY(_) => [DrawCmd::SetShadowOffsetY(3.25)],
            DrawCmd::SetShadowBlur(_) => [DrawCmd::SetShadowBlur(4.5)],
            DrawCmd::Translate { .. } => [DrawCmd::Translate { x: -10.0, y: 20.5 }],
            DrawCmd::Rotate { .. } => [DrawCmd::Rotate { angle: -FRAC_PI_4 }],
            DrawCmd::Scale { .. } => [DrawCmd::Scale { x: 1.5, y: -1.0 }],
            DrawCmd::SetTransform { .. } => [
                DrawCmd::SetTransform { a: 2.0, b: 0.125, c: -0.25, d: 1.75, e: -10.0, f: -20.5 },
            ],
            DrawCmd::DrawImage { .. } => [
                DrawCmd::DrawImage { key: "rId7".into(), dx: 10.5, dy: -20.0, dw: 30.25, dh: 40.0 },
                DrawCmd::DrawImage { key: "emf-pic:rId9".into(), dx: -1.0, dy: 2.0, dw: 64.5, dh: 48.25 },
            ],
            DrawCmd::SetLinearGradient { .. } => [
                DrawCmd::SetLinearGradient {
                    x0: -5.0, y0: 0.5, x1: 100.0, y1: 50.25,
                    stops: vec![(0.0, "#ff0000".into()), (0.375, "#1a1a1a".into())],
                },
                DrawCmd::SetLinearGradient { x0: 1.0, y0: 2.0, x1: 3.0, y1: 4.0, stops: vec![] },
            ],
            DrawCmd::SetRadialGradient { .. } => [DrawCmd::SetRadialGradient {
                x0: 10.0, y0: 11.0, r0: 0.5, x1: 12.0, y1: 13.0, r1: 40.25,
                stops: vec![(0.0, "#ffffff".into()), (0.5, "#808080".into()), (1.0, "#000000".into())],
            }],
            DrawCmd::SetPatternFill { .. } => [DrawCmd::SetPatternFill {
                key: "dkHorz|#1a1a1a|#fafafa".into(),
                fg: "#1a1a1a".into(),
                bg: "#fafafa".into(),
                size: 8.0,
                prims: vec![
                    TilePrim::Line { x1: 0.0, y1: 4.5, x2: 8.0, y2: 3.5, lw: 1.25 },
                    TilePrim::Dot { x: 2.0, y: 6.0, s: 1.5 },
                ],
            }],
            DrawCmd::SetLineDash(_) => [
                DrawCmd::SetLineDash(vec![4.0, 3.0, 1.5, 3.25]),
                DrawCmd::SetLineDash(vec![]),
            ],
            DrawCmd::ClearLineDash => [DrawCmd::ClearLineDash],
            DrawCmd::SetFontKerning(_) => [DrawCmd::SetFontKerning(true), DrawCmd::SetFontKerning(false)],
            DrawCmd::DrawImageCrop { .. } => [
                DrawCmd::DrawImageCrop {
                    key: "rId7".into(), sx: 0.25, sy: 0.125, sw: 0.5, sh: 0.625, dx: -3.5, dy: 4.25, dw: 80.75, dh: 60.5,
                },
                DrawCmd::DrawImageCrop {
                    key: "emf-pic:rId9".into(), sx: 0.0, sy: 0.375, sw: 0.75, sh: 0.5, dx: 12.0, dy: -8.5, dw: 64.5, dh: 24.25,
                },
            ],
        }
    }

    /// One argument as a decoder sees it: a number, a string-table entry
    /// (resolved to its text), or a 0/1 flag.
    #[derive(Debug)]
    enum WireArg {
        Num(f64),
        Str(String),
        Bool(bool),
    }

    /// The wire schema of one command: its opcode's name and number, and its
    /// arguments in stream order, counts and tile tags included. Exhaustive with
    /// no wildcard, so a new variant cannot compile without stating its frame.
    /// `wire_golden_matches_the_fixture` checks every frame against what
    /// `encode_display_list` actually emitted, so this cannot drift silently.
    fn wire_frame(cmd: &DrawCmd) -> (&'static str, f64, Vec<WireArg>) {
        use WireArg::{Bool as B, Num as N, Str as S};
        let stops = |args: &mut Vec<WireArg>, stops: &[(f64, String)]| {
            args.push(N(stops.len() as f64));
            for (pos, color) in stops {
                args.push(N(*pos));
                args.push(S(color.clone()));
            }
        };
        match cmd {
            DrawCmd::Save => ("OP_SAVE", OP_SAVE, vec![]),
            DrawCmd::Restore => ("OP_RESTORE", OP_RESTORE, vec![]),
            DrawCmd::BeginPath => ("OP_BEGIN_PATH", OP_BEGIN_PATH, vec![]),
            DrawCmd::ClosePath => ("OP_CLOSE_PATH", OP_CLOSE_PATH, vec![]),
            DrawCmd::MoveTo { x, y } => ("OP_MOVE_TO", OP_MOVE_TO, vec![N(*x), N(*y)]),
            DrawCmd::LineTo { x, y } => ("OP_LINE_TO", OP_LINE_TO, vec![N(*x), N(*y)]),
            DrawCmd::Rect { x, y, w, h } => ("OP_RECT", OP_RECT, vec![N(*x), N(*y), N(*w), N(*h)]),
            DrawCmd::Arc { x, y, radius, start, end } => {
                ("OP_ARC", OP_ARC, vec![N(*x), N(*y), N(*radius), N(*start), N(*end)])
            }
            DrawCmd::ArcAnticlockwise { x, y, radius, start, end, anticlockwise } => (
                "OP_ARC_ACW",
                OP_ARC_ACW,
                vec![N(*x), N(*y), N(*radius), N(*start), N(*end), B(*anticlockwise)],
            ),
            DrawCmd::Ellipse { x, y, rx, ry, rotation, start, end, anticlockwise } => (
                "OP_ELLIPSE",
                OP_ELLIPSE,
                vec![N(*x), N(*y), N(*rx), N(*ry), N(*rotation), N(*start), N(*end), B(*anticlockwise)],
            ),
            DrawCmd::QuadraticCurveTo { cpx, cpy, x, y } => {
                ("OP_QUAD_TO", OP_QUAD_TO, vec![N(*cpx), N(*cpy), N(*x), N(*y)])
            }
            DrawCmd::BezierCurveTo { cp1x, cp1y, cp2x, cp2y, x, y } => (
                "OP_BEZIER_TO",
                OP_BEZIER_TO,
                vec![N(*cp1x), N(*cp1y), N(*cp2x), N(*cp2y), N(*x), N(*y)],
            ),
            DrawCmd::Fill => ("OP_FILL", OP_FILL, vec![]),
            DrawCmd::Stroke => ("OP_STROKE", OP_STROKE, vec![]),
            DrawCmd::Clip => ("OP_CLIP", OP_CLIP, vec![]),
            DrawCmd::FillRect { x, y, w, h } => ("OP_FILL_RECT", OP_FILL_RECT, vec![N(*x), N(*y), N(*w), N(*h)]),
            DrawCmd::StrokeRect { x, y, w, h } => {
                ("OP_STROKE_RECT", OP_STROKE_RECT, vec![N(*x), N(*y), N(*w), N(*h)])
            }
            DrawCmd::FillText { text, x, y } => ("OP_FILL_TEXT", OP_FILL_TEXT, vec![S(text.clone()), N(*x), N(*y)]),
            DrawCmd::StrokeText { text, x, y } => {
                ("OP_STROKE_TEXT", OP_STROKE_TEXT, vec![S(text.clone()), N(*x), N(*y)])
            }
            DrawCmd::SetFont(v) => ("OP_SET_FONT", OP_SET_FONT, vec![S(v.clone())]),
            DrawCmd::SetTextAlign(v) => ("OP_SET_TEXT_ALIGN", OP_SET_TEXT_ALIGN, vec![S(v.clone())]),
            DrawCmd::SetTextBaseline(v) => ("OP_SET_TEXT_BASELINE", OP_SET_TEXT_BASELINE, vec![S(v.clone())]),
            DrawCmd::SetFillStyle(v) => ("OP_SET_FILL_STYLE", OP_SET_FILL_STYLE, vec![S(v.clone())]),
            DrawCmd::SetStrokeStyle(v) => ("OP_SET_STROKE_STYLE", OP_SET_STROKE_STYLE, vec![S(v.clone())]),
            DrawCmd::SetLineWidth(v) => ("OP_SET_LINE_WIDTH", OP_SET_LINE_WIDTH, vec![N(*v)]),
            DrawCmd::SetGlobalAlpha(v) => ("OP_SET_GLOBAL_ALPHA", OP_SET_GLOBAL_ALPHA, vec![N(*v)]),
            DrawCmd::SetShadowColor(v) => ("OP_SET_SHADOW_COLOR", OP_SET_SHADOW_COLOR, vec![S(v.clone())]),
            DrawCmd::SetShadowOffsetX(v) => ("OP_SET_SHADOW_OFFSET_X", OP_SET_SHADOW_OFFSET_X, vec![N(*v)]),
            DrawCmd::SetShadowOffsetY(v) => ("OP_SET_SHADOW_OFFSET_Y", OP_SET_SHADOW_OFFSET_Y, vec![N(*v)]),
            DrawCmd::SetShadowBlur(v) => ("OP_SET_SHADOW_BLUR", OP_SET_SHADOW_BLUR, vec![N(*v)]),
            DrawCmd::Translate { x, y } => ("OP_TRANSLATE", OP_TRANSLATE, vec![N(*x), N(*y)]),
            DrawCmd::Rotate { angle } => ("OP_ROTATE", OP_ROTATE, vec![N(*angle)]),
            DrawCmd::Scale { x, y } => ("OP_SCALE", OP_SCALE, vec![N(*x), N(*y)]),
            DrawCmd::SetTransform { a, b, c, d, e, f } => (
                "OP_SET_TRANSFORM",
                OP_SET_TRANSFORM,
                vec![N(*a), N(*b), N(*c), N(*d), N(*e), N(*f)],
            ),
            DrawCmd::DrawImage { key, dx, dy, dw, dh } => (
                "OP_DRAW_IMAGE",
                OP_DRAW_IMAGE,
                vec![S(key.clone()), N(*dx), N(*dy), N(*dw), N(*dh)],
            ),
            DrawCmd::SetLinearGradient { x0, y0, x1, y1, stops: s } => {
                let mut args = vec![N(*x0), N(*y0), N(*x1), N(*y1)];
                stops(&mut args, s);
                ("OP_SET_LINEAR_GRADIENT", OP_SET_LINEAR_GRADIENT, args)
            }
            DrawCmd::SetRadialGradient { x0, y0, r0, x1, y1, r1, stops: s } => {
                let mut args = vec![N(*x0), N(*y0), N(*r0), N(*x1), N(*y1), N(*r1)];
                stops(&mut args, s);
                ("OP_SET_RADIAL_GRADIENT", OP_SET_RADIAL_GRADIENT, args)
            }
            DrawCmd::SetPatternFill { key, fg, bg, size, prims } => {
                let mut args = vec![S(key.clone()), S(fg.clone()), S(bg.clone()), N(*size), N(prims.len() as f64)];
                for prim in prims {
                    match *prim {
                        TilePrim::Line { x1, y1, x2, y2, lw } => {
                            args.extend([N(TILE_LINE), N(x1), N(y1), N(x2), N(y2), N(lw)])
                        }
                        TilePrim::Dot { x, y, s: side } => args.extend([N(TILE_DOT), N(x), N(y), N(side)]),
                    }
                }
                ("OP_SET_PATTERN_FILL", OP_SET_PATTERN_FILL, args)
            }
            DrawCmd::SetLineDash(segs) => {
                let mut args = vec![N(segs.len() as f64)];
                args.extend(segs.iter().map(|v| N(*v)));
                ("OP_SET_LINE_DASH", OP_SET_LINE_DASH, args)
            }
            DrawCmd::ClearLineDash => ("OP_CLEAR_LINE_DASH", OP_CLEAR_LINE_DASH, vec![]),
            DrawCmd::SetFontKerning(on) => ("OP_SET_FONT_KERNING", OP_SET_FONT_KERNING, vec![B(*on)]),
            DrawCmd::DrawImageCrop { key, sx, sy, sw, sh, dx, dy, dw, dh } => (
                "OP_DRAW_IMAGE_CROP",
                OP_DRAW_IMAGE_CROP,
                vec![S(key.clone()), N(*sx), N(*sy), N(*sw), N(*sh), N(*dx), N(*dy), N(*dw), N(*dh)],
            ),
        }
    }

    /// A JSON number. Rust's `Display` for f64 is the shortest text that parses
    /// back to the same value, which is also what `JSON.parse` reads. NaN, the
    /// infinities and -0 have no faithful JSON form, so a sample may not use them.
    fn json_num(v: f64) -> String {
        assert!(v.is_finite() && !(v == 0.0 && v.is_sign_negative()), "{v} has no exact JSON form");
        format!("{v}")
    }

    fn json_str(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    fn json_arg(arg: &WireArg) -> String {
        match arg {
            WireArg::Num(v) => json_num(*v),
            WireArg::Str(s) => json_str(s),
            WireArg::Bool(b) => b.to_string(),
        }
    }

    /// Render the fixture. One entry per line, and one stream frame per line, so
    /// a drift shows in `git diff` as the commands that moved.
    fn wire_golden_json(list: &DisplayList, ops: &[f64], strings: &[String]) -> String {
        let frames: Vec<_> = list.iter().map(wire_frame).collect();
        let mut table: Vec<(&str, f64)> = Vec::new();
        for (name, op, _) in &frames {
            if !table.iter().any(|(n, _)| n == name) {
                table.push((name, *op));
            }
        }
        table.sort_by(|a, b| a.1.total_cmp(&b.1));
        for pair in table.windows(2) {
            assert_ne!(pair[0].1, pair[1].1, "{} and {} share an opcode", pair[0].0, pair[1].0);
        }
        let lines = |items: Vec<String>| items.join(",\n    ");
        let mut stream = Vec::new();
        let mut at = 0usize;
        for (_, _, args) in &frames {
            let width = 1 + args.len();
            stream.push(ops[at..at + width].iter().map(|v| json_num(*v)).collect::<Vec<_>>().join(", "));
            at += width;
        }
        format!(
            concat!(
                "{{\n",
                "  \"about\": \"Display-list wire golden shared by the Rust encoder and the TS replay. Generated; do not edit. Rewrite with: DISPLAY_LIST_BLESS=1 cargo test -p parasma-wasm --features sml,pml,emf wire_golden\",\n",
                "  \"opcodes\": {{\n    {}\n  }},\n",
                "  \"tileTags\": {{\n    {}\n  }},\n",
                "  \"strings\": [\n    {}\n  ],\n",
                "  \"stream\": [\n    {}\n  ],\n",
                "  \"decoded\": [\n    {}\n  ]\n",
                "}}\n",
            ),
            lines(table.iter().map(|(n, op)| format!("{}: {}", json_str(n), json_num(*op))).collect()),
            lines(vec![
                format!("\"TILE_LINE\": {}", json_num(TILE_LINE)),
                format!("\"TILE_DOT\": {}", json_num(TILE_DOT)),
            ]),
            lines(strings.iter().map(|s| json_str(s)).collect()),
            lines(stream),
            lines(
                frames
                    .iter()
                    .map(|(name, _, args)| {
                        let args: Vec<_> = args.iter().map(json_arg).collect();
                        format!("{{ \"op\": {}, \"args\": [{}] }}", json_str(name), args.join(", "))
                    })
                    .collect()
            ),
        )
    }

    #[test]
    fn wire_golden_matches_the_fixture() {
        let list = wire_coverage();
        let (ops, strings) = encode_display_list(&list);
        assert_eq!(decode_display_list(&ops, &strings), list, "encoder and reference decoder disagree");
        assert_eq!(strings.iter().filter(|s| *s == "#1a1a1a").count(), 1, "the string table must dedup");

        // The fixture's decoded list is `wire_frame`, so prove `wire_frame` is
        // what the encoder emitted: walk the real stream frame by frame.
        let mut at = 0usize;
        for cmd in &list {
            let (name, op, args) = wire_frame(cmd);
            assert_eq!(ops[at], op, "{name}: opcode at {at}");
            at += 1;
            for arg in &args {
                let got = ops[at];
                match arg {
                    WireArg::Num(v) => assert_eq!(got.to_bits(), v.to_bits(), "{name}: argument at {at}"),
                    WireArg::Bool(b) => assert_eq!(got, if *b { 1.0 } else { 0.0 }, "{name}: flag at {at}"),
                    WireArg::Str(s) => assert_eq!(&strings[got as usize], s, "{name}: string at {at}"),
                }
                at += 1;
            }
        }
        assert_eq!(at, ops.len(), "the frames do not account for the whole stream");

        let got = wire_golden_json(&list, &ops, &strings);
        if std::env::var("DISPLAY_LIST_BLESS").as_deref() == Ok("1") {
            std::fs::write(WIRE_GOLDEN, &got).expect("write the display-list wire golden");
            return;
        }
        let want = std::fs::read_to_string(WIRE_GOLDEN).unwrap_or_else(|e| {
            panic!("cannot read {WIRE_GOLDEN}: {e}. Write it with DISPLAY_LIST_BLESS=1")
        });
        if got == want {
            return;
        }
        let line = got.lines().zip(want.lines()).position(|(a, b)| a != b).unwrap_or(0);
        panic!(
            "display-list wire golden drifted at line {}.\n  golden: {}\n  actual: {}\n\
             The TS replay is tested against this file. If the encoding change is intended, \
             rewrite it with DISPLAY_LIST_BLESS=1, update replay.ts to match, and review the git diff.",
            line + 1,
            want.lines().nth(line).unwrap_or("<end of file>"),
            got.lines().nth(line).unwrap_or("<end of file>"),
        );
    }
}
