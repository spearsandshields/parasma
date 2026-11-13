//! Walk an [`EmfPicture`]'s records against a live [`Dc`], compose the
//! per-primitive matrix, and emit `Painter` calls. All geometry is transformed
//! in Rust before it reaches the `Painter` (see [`super::transform`]).

use wasm_bindgen::JsValue;

use crate::engine::paint::Painter;

use super::dc::{BkMode, DcStack, GdiObject, HAlign, VBaseline};
use super::records::{
    pen_dash, BlitHeader, EmrRecord, EmrText, LogFont, LogPen, PointL, RectL, BS_NULL,
    ETO_GLYPH_INDEX, ETO_OPAQUE, PS_NULL, PS_STYLE_MASK, TA_BASELINE, TA_BOTTOM, TA_CENTER,
    TA_RIGHT, TA_UPDATECP,
};
use super::transform::{xform_to_mat, Mat};
use super::EmfPicture;

/// MODIFYWORLDTRANSFORM modes (MS-EMF §2.1.24 ModifyWorldTransformMode).
const MWT_IDENTITY: u32 = 1;
const MWT_LEFTMULTIPLY: u32 = 2;
const MWT_RIGHTMULTIPLY: u32 = 4;

/// Raster ops used for DIB-less BITBLT floods (MS-WMF ternary rasterops).
const ROP_BLACKNESS: u32 = 0x0000_0042;
const ROP_WHITENESS: u32 = 0x00FF_0062;
const ROP_PATCOPY: u32 = 0x00F0_0021;

/// Paint `pic` into the inline destination rect `(dx,dy,dw,dh)` in page px.
pub(crate) fn paint(
    pic: &EmfPicture,
    p: &dyn Painter,
    dx: f64,
    dy: f64,
    dw: f64,
    dh: f64,
) -> Result<(), JsValue> {
    let place = super::transform::placement(&pic.bounds, dx, dy, dw, dh);

    let mut dcs = DcStack::new(pic.map_seed());
    // Whether an accumulated MOVETO/LINETO polyline is awaiting its stroke.
    let mut pending_stroke = false;

    for rec in &pic.records {
        // A run of MOVETO/LINETO accumulates; ANY other op flushes its stroke.
        if pending_stroke && !matches!(rec, EmrRecord::MoveToEx(_) | EmrRecord::LineTo(_)) {
            flush_polyline(p, &dcs.dc.pen, composed(&place, &dcs));
            pending_stroke = false;
        }

        match rec {
            EmrRecord::MoveToEx(pt) => {
                dcs.dc.cur_pt = *pt;
                let m = composed(&place, &dcs);
                let (x, y) = m.apply(pt.x as f64, pt.y as f64);
                if !pending_stroke {
                    p.begin_path();
                    pending_stroke = true;
                }
                p.move_to(x, y);
            }
            EmrRecord::LineTo(pt) => {
                dcs.dc.cur_pt = *pt;
                let m = composed(&place, &dcs);
                let (x, y) = m.apply(pt.x as f64, pt.y as f64);
                if !pending_stroke {
                    p.begin_path();
                    let (sx, sy) = m.apply(0.0, 0.0);
                    p.move_to(sx, sy);
                    pending_stroke = true;
                }
                p.line_to(x, y);
            }

            EmrRecord::Poly { points, close, from_cur } => {
                paint_poly(p, &dcs, composed(&place, &dcs), points, *close, *from_cur);
                if let Some(last) = points.last() {
                    dcs.dc.cur_pt = *last;
                }
            }
            EmrRecord::PolyPoly { polys, close } => {
                paint_polypoly(p, &dcs, composed(&place, &dcs), polys, *close);
            }
            EmrRecord::Bezier { points, from_cur } => {
                paint_bezier(p, &dcs, composed(&place, &dcs), points, *from_cur);
                if let Some(last) = points.last() {
                    dcs.dc.cur_pt = *last;
                }
            }

            EmrRecord::Rectangle(r) => paint_rect_path(p, &dcs, composed(&place, &dcs), r, true)?,
            EmrRecord::Ellipse(r) => paint_ellipse(p, &dcs, composed(&place, &dcs), r)?,
            EmrRecord::RoundRect(r) => paint_rect_path(p, &dcs, composed(&place, &dcs), r, true)?,

            EmrRecord::SetWindowExtEx(cx, cy) => dcs.dc.map.win_ext = (*cx, *cy),
            EmrRecord::SetWindowOrgEx(x, y) => dcs.dc.map.win_org = (*x, *y),
            EmrRecord::SetViewportExtEx(cx, cy) => dcs.dc.map.vp_ext = (*cx, *cy),
            EmrRecord::SetViewportOrgEx(x, y) => dcs.dc.map.vp_org = (*x, *y),
            EmrRecord::SetMapMode(m) => dcs.dc.map.mode = *m,

            EmrRecord::SetWorldTransform(x) => dcs.dc.world = xform_to_mat(x),
            EmrRecord::ModifyWorldTransform(x, mode) => {
                let xf = xform_to_mat(x);
                dcs.dc.world = match *mode {
                    MWT_IDENTITY => Mat::IDENTITY,
                    // GDI row-vector LEFTMULTIPLY (specified·current) becomes a
                    // column-vector world·xform; RIGHTMULTIPLY inverts.
                    MWT_LEFTMULTIPLY => dcs.dc.world.compose(&xf),
                    MWT_RIGHTMULTIPLY => xf.compose(&dcs.dc.world),
                    _ => dcs.dc.world,
                };
            }

            EmrRecord::SetTextColor(c) => dcs.dc.text_color = c.clone(),
            EmrRecord::SetBkColor(c) => dcs.dc.bk_color = c.clone(),
            EmrRecord::SetBkMode(m) => {
                dcs.dc.bk_mode = if *m == 1 { BkMode::Transparent } else { BkMode::Opaque };
            }
            EmrRecord::SetTextAlign(flags) => apply_text_align(&mut dcs.dc, *flags),
            EmrRecord::SetPolyFillMode(_) => {}

            EmrRecord::ExtTextOut(t) => paint_text(p, &dcs, composed(&place, &dcs), t)?,

            EmrRecord::CreatePen(ih, pen) => dcs.create(*ih, GdiObject::Pen(pen.clone())),
            EmrRecord::CreateBrush(ih, b) => dcs.create(*ih, GdiObject::Brush(b.clone())),
            EmrRecord::CreateFont(ih, f) => dcs.create(*ih, GdiObject::Font(f.clone())),
            EmrRecord::SelectObject(ih) => dcs.select(*ih),
            EmrRecord::DeleteObject(ih) => dcs.delete(*ih),

            EmrRecord::SaveDc => dcs.save(),
            EmrRecord::RestoreDc(n) => {
                let undo = dcs.restore(*n);
                for _ in 0..undo {
                    p.restore();
                }
            }
            EmrRecord::IntersectClipRect(r) => {
                let m = composed(&place, &dcs);
                let (x, y, w, h) = axis_aligned(&m, r);
                p.save();
                p.begin_path();
                p.rect(x, y, w, h);
                p.clip();
                dcs.note_clip_opened();
            }

            EmrRecord::Blit(hdr) => paint_blit(p, pic, &dcs, &place, hdr)?,

            EmrRecord::Eof | EmrRecord::Skip => {}
        }
    }

    if pending_stroke {
        flush_polyline(p, &dcs.dc.pen, composed(&place, &dcs));
    }
    // Balance any clip saves that leaked (unbalanced RESTOREDC at picture end).
    for _ in 0..dcs.take_open_clips() {
        p.restore();
    }
    Ok(())
}

fn composed(place: &Mat, dcs: &DcStack) -> Mat {
    // p_page = M_place · M_map · M_world · p_logical
    place.compose(&dcs.dc.map.matrix()).compose(&dcs.dc.world)
}

fn axis_aligned(m: &Mat, r: &RectL) -> (f64, f64, f64, f64) {
    let (x0, y0) = m.apply(r.left as f64, r.top as f64);
    let (x1, y1) = m.apply(r.right as f64, r.bottom as f64);
    (x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs())
}

fn stroke_width(pen: &LogPen, m: &Mat) -> f64 {
    let scaled = pen.width.max(0) as f64 * m.uniform_scale();
    if scaled < 1.0 {
        1.0
    } else {
        scaled
    }
}

fn flush_polyline(p: &dyn Painter, pen: &LogPen, m: Mat) {
    if pen.style & PS_STYLE_MASK == PS_NULL {
        return;
    }
    p.set_stroke_style_str(&pen.color);
    let lw = stroke_width(pen, &m);
    p.set_line_width(lw);
    let dash = pen_dash(pen.style, lw);
    if dash.is_empty() {
        p.clear_line_dash();
    } else {
        p.set_line_dash(&dash);
    }
    p.stroke();
    p.clear_line_dash();
}

fn build_path(p: &dyn Painter, m: &Mat, cur: PointL, points: &[PointL], from_cur: bool, close: bool) {
    p.begin_path();
    let start: &[PointL];
    if from_cur {
        let (sx, sy) = m.apply(cur.x as f64, cur.y as f64);
        p.move_to(sx, sy);
        start = points;
    } else if let Some((first, rest)) = points.split_first() {
        let (sx, sy) = m.apply(first.x as f64, first.y as f64);
        p.move_to(sx, sy);
        start = rest;
    } else {
        return;
    }
    for pt in start {
        let (x, y) = m.apply(pt.x as f64, pt.y as f64);
        p.line_to(x, y);
    }
    if close {
        p.close_path();
    }
}

fn paint_poly(p: &dyn Painter, dcs: &DcStack, m: Mat, points: &[PointL], close: bool, from_cur: bool) {
    if points.is_empty() {
        return;
    }
    build_path(p, &m, dcs.dc.cur_pt, points, from_cur, close);
    if close && dcs.dc.brush.style != BS_NULL {
        p.set_fill_style_str(&dcs.dc.brush.color);
        p.fill();
    }
    if dcs.dc.pen.style & PS_STYLE_MASK != PS_NULL {
        p.set_stroke_style_str(&dcs.dc.pen.color);
        p.set_line_width(stroke_width(&dcs.dc.pen, &m));
        p.stroke();
    }
}

fn paint_polypoly(p: &dyn Painter, dcs: &DcStack, m: Mat, polys: &[Vec<PointL>], close: bool) {
    p.begin_path();
    for sub in polys {
        if let Some((first, rest)) = sub.split_first() {
            let (sx, sy) = m.apply(first.x as f64, first.y as f64);
            p.move_to(sx, sy);
            for pt in rest {
                let (x, y) = m.apply(pt.x as f64, pt.y as f64);
                p.line_to(x, y);
            }
            if close {
                p.close_path();
            }
        }
    }
    if close && dcs.dc.brush.style != BS_NULL {
        p.set_fill_style_str(&dcs.dc.brush.color);
        p.fill();
    }
    if dcs.dc.pen.style & PS_STYLE_MASK != PS_NULL {
        p.set_stroke_style_str(&dcs.dc.pen.color);
        p.set_line_width(stroke_width(&dcs.dc.pen, &m));
        p.stroke();
    }
}

fn paint_bezier(p: &dyn Painter, dcs: &DcStack, m: Mat, points: &[PointL], from_cur: bool) {
    p.begin_path();
    let mut idx = 0;
    if from_cur {
        let (sx, sy) = m.apply(dcs.dc.cur_pt.x as f64, dcs.dc.cur_pt.y as f64);
        p.move_to(sx, sy);
    } else if !points.is_empty() {
        let (sx, sy) = m.apply(points[0].x as f64, points[0].y as f64);
        p.move_to(sx, sy);
        idx = 1;
    }
    while idx + 2 < points.len() {
        let c1 = m.apply(points[idx].x as f64, points[idx].y as f64);
        let c2 = m.apply(points[idx + 1].x as f64, points[idx + 1].y as f64);
        let end = m.apply(points[idx + 2].x as f64, points[idx + 2].y as f64);
        p.bezier_curve_to(c1.0, c1.1, c2.0, c2.1, end.0, end.1);
        idx += 3;
    }
    if dcs.dc.pen.style & PS_STYLE_MASK != PS_NULL {
        p.set_stroke_style_str(&dcs.dc.pen.color);
        p.set_line_width(stroke_width(&dcs.dc.pen, &m));
        p.stroke();
    }
}

fn paint_rect_path(
    p: &dyn Painter,
    dcs: &DcStack,
    m: Mat,
    r: &RectL,
    close: bool,
) -> Result<(), JsValue> {
    // Transform each corner so rotation/shear is honoured.
    let corners = [
        (r.left, r.top),
        (r.right, r.top),
        (r.right, r.bottom),
        (r.left, r.bottom),
    ];
    p.begin_path();
    let (sx, sy) = m.apply(corners[0].0 as f64, corners[0].1 as f64);
    p.move_to(sx, sy);
    for c in &corners[1..] {
        let (x, y) = m.apply(c.0 as f64, c.1 as f64);
        p.line_to(x, y);
    }
    if close {
        p.close_path();
    }
    if dcs.dc.brush.style != BS_NULL {
        p.set_fill_style_str(&dcs.dc.brush.color);
        p.fill();
    }
    if dcs.dc.pen.style & PS_STYLE_MASK != PS_NULL {
        p.set_stroke_style_str(&dcs.dc.pen.color);
        p.set_line_width(stroke_width(&dcs.dc.pen, &m));
        p.stroke();
    }
    Ok(())
}

fn paint_ellipse(p: &dyn Painter, dcs: &DcStack, m: Mat, r: &RectL) -> Result<(), JsValue> {
    let cx = (r.left + r.right) as f64 / 2.0;
    let cy = (r.top + r.bottom) as f64 / 2.0;
    let (ccx, ccy) = m.apply(cx, cy);
    let rx = ((r.right - r.left) as f64 / 2.0 * m.x_scale()).abs();
    let ry = ((r.bottom - r.top) as f64 / 2.0 * m.a.hypot(m.c).max(m.b.hypot(m.d))).abs();
    p.begin_path();
    p.ellipse_with_anticlockwise(ccx, ccy, rx, ry, 0.0, 0.0, std::f64::consts::TAU, false)?;
    if dcs.dc.brush.style != BS_NULL {
        p.set_fill_style_str(&dcs.dc.brush.color);
        p.fill();
    }
    if dcs.dc.pen.style & PS_STYLE_MASK != PS_NULL {
        p.set_stroke_style_str(&dcs.dc.pen.color);
        p.set_line_width(stroke_width(&dcs.dc.pen, &m));
        p.stroke();
    }
    Ok(())
}

fn apply_text_align(dc: &mut super::dc::Dc, flags: u32) {
    dc.h_align = if flags & TA_CENTER == TA_CENTER {
        HAlign::Center
    } else if flags & TA_RIGHT != 0 {
        HAlign::Right
    } else {
        HAlign::Left
    };
    dc.v_baseline = if flags & TA_BASELINE == TA_BASELINE {
        VBaseline::Base
    } else if flags & TA_BOTTOM != 0 {
        VBaseline::Bottom
    } else {
        VBaseline::Top
    };
    dc.update_cp = flags & TA_UPDATECP != 0;
}

fn font_css(font: &LogFont, px: f64) -> String {
    let mut css = String::new();
    if font.italic {
        css.push_str("italic ");
    }
    if font.weight >= 700 {
        css.push_str("bold ");
    }
    css.push_str(&format!("{px}px "));
    if font.face.is_empty() {
        css.push_str("sans-serif");
    } else {
        css.push_str(&format!("\"{}\", sans-serif", font.face));
    }
    css
}

fn paint_text(p: &dyn Painter, dcs: &DcStack, m: Mat, t: &EmrText) -> Result<(), JsValue> {
    // ETO_GLYPH_INDEX: the "string" is font-specific glyph IDs, not Unicode —
    // degrade (do not render glyph IDs as text).
    if t.options & ETO_GLYPH_INDEX != 0 {
        return Ok(());
    }
    if t.text.is_empty() {
        return Ok(());
    }

    // Opaque background box (ETO_OPAQUE forces it regardless of bk_mode; else an
    // OPAQUE bk_mode with a non-empty bounds rect).
    let has_bounds = t.bounds.right > t.bounds.left && t.bounds.bottom > t.bounds.top;
    let opaque = (t.options & ETO_OPAQUE != 0) || dcs.dc.bk_mode == BkMode::Opaque;
    if opaque && has_bounds {
        let (x, y, w, h) = axis_aligned(&m, &t.bounds);
        p.set_fill_style_str(&dcs.dc.bk_color);
        p.fill_rect(x, y, w, h);
    }

    let px = (dcs.dc.font.height.unsigned_abs() as f64 * m.uniform_scale()).max(1.0);
    p.set_font(&font_css(&dcs.dc.font, px));
    p.set_fill_style_str(&dcs.dc.text_color);
    p.set_text_baseline(match dcs.dc.v_baseline {
        VBaseline::Top => "top",
        VBaseline::Base => "alphabetic",
        VBaseline::Bottom => "bottom",
    });

    let (rx, ry) = m.apply(t.reference.x as f64, t.reference.y as f64);

    if !t.dx.is_empty() {
        // Per-glyph placement (kerning-faithful): left-align each glyph and
        // advance by the scaled dx.
        p.set_text_align("left");
        let xs = m.x_scale();
        let mut x = rx;
        for (i, ch) in t.text.chars().enumerate() {
            let mut buf = [0u8; 4];
            p.fill_text(ch.encode_utf8(&mut buf), x, ry)?;
            if let Some(d) = t.dx.get(i) {
                x += *d as f64 * xs;
            }
        }
    } else {
        p.set_text_align(match dcs.dc.h_align {
            HAlign::Left => "left",
            HAlign::Center => "center",
            HAlign::Right => "right",
        });
        p.fill_text(&t.text, rx, ry)?;
    }
    Ok(())
}

fn paint_blit(
    p: &dyn Painter,
    pic: &EmfPicture,
    dcs: &DcStack,
    place: &Mat,
    hdr: &BlitHeader,
) -> Result<(), JsValue> {
    // DIB blits use only the translate+scale part (axis-aligned placement); a
    // rotated/sheared world transform degrades to the axis-aligned bbox.
    let m = place.compose(&dcs.dc.map.matrix()).compose(&dcs.dc.world);
    let (x, y, w, h) = axis_aligned(&m, &hdr.dest);

    if let Some(idx) = hdr.blit {
        let key = format!("emf:{}:{}", pic.rel, idx);
        p.draw_image(&key, x, y, w, h)?;
        return Ok(());
    }

    // DIB-less raster-op flood.
    match hdr.rop {
        ROP_BLACKNESS => {
            p.set_fill_style_str("#000000");
            p.fill_rect(x, y, w, h);
        }
        ROP_WHITENESS => {
            p.set_fill_style_str("#ffffff");
            p.fill_rect(x, y, w, h);
        }
        ROP_PATCOPY if dcs.dc.brush.style != BS_NULL => {
            p.set_fill_style_str(&dcs.dc.brush.color);
            p.fill_rect(x, y, w, h);
        }
        _ => {}
    }
    Ok(())
}
