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
//! `draw_image` records only the KEY: the worker holds no bitmaps, so the
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
        p.draw_image("rId7", 10.0, 20.0, 30.0, 40.0);
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
}
