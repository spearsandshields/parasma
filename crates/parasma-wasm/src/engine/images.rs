//! Images, for every format: the media part types, the placeholder drawn for
//! an image not yet decoded, and how a picture draws: its crop, opacity,
//! outline and turn, the paint that draws them, and where a turned picture
//! lands.

use wasm_bindgen::JsValue;

use crate::engine::border::{Edge, LineStyle};
use crate::engine::color::parse_dml_pct;
use crate::engine::paint::Painter;
use crate::engine::units::emu_to_px;

/// Derive an image mime type from a media part's file extension. Lives in
/// engine/ (always compiled, format-agnostic) so both the `sml` and `pml`
/// feature builds share ONE copy — the previous per-format duplicates existed
/// only because `pml::mime_from_path` was feature-gated out of an sml-only
/// build.
pub(crate) fn mime_from_path(path: &str) -> String {
    let ext = path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Raw image data extracted from the OOXML package.
pub(crate) struct ImageBytes {
    pub(crate) mime: String,
    pub(crate) data: Vec<u8>,
}

/// Grey rectangle with "[image]" label for images that haven't yet decoded
/// (or decoded to an unsupported format), so a missing image is never blank.
pub(crate) fn draw_image_placeholder(
    p: &dyn Painter,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<(), JsValue> {
    p.set_fill_style_str("#e6edf3");
    p.fill_rect(x, y, w, h);
    p.set_stroke_style_str("#7d8590");
    p.set_line_width(1.0);
    p.stroke_rect(x, y, w, h);
    p.set_fill_style_str("#7d8590");
    p.set_font("11px -apple-system, sans-serif");
    p.set_text_baseline("alphabetic");
    p.fill_text("[image]", x + 8.0, y + 18.0)?;
    Ok(())
}

/// A rectangle placed by the offset of each edge from the same edge of a
/// reference box (`CT_RelativeRect`): `l` and `r` are fractions of the box's
/// width, `t` and `b` of its height. A positive offset is an inset, a
/// negative one an outset.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct RelativeRect {
    pub(crate) l: f64,
    pub(crate) t: f64,
    pub(crate) r: f64,
    pub(crate) b: f64,
}

impl RelativeRect {
    /// The rectangle `attr` reads: its `l`, `t`, `r` and `b` attributes, each
    /// a percentage (`ST_Percentage`, §20.1.10.40), an absent or unreadable
    /// one zero.
    pub(crate) fn from_attrs<'a>(attr: impl Fn(&str) -> Option<&'a str>) -> Self {
        let edge = |name| attr(name).and_then(parse_dml_pct).unwrap_or(0.0);
        RelativeRect { l: edge("l"), t: edge("t"), r: edge("r"), b: edge("b") }
    }
}

/// What part of a picture's bitmap is drawn, and where in the picture's frame
/// (`a:blipFill` with `a:stretch`, §20.1.8.14). All zero is the whole bitmap
/// stretched over the whole frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ImageCrop {
    /// `a:srcRect` (§20.1.8.55): the part of the bitmap drawn, relative to
    /// the bitmap.
    pub(crate) src: RelativeRect,
    /// `a:fillRect` (§20.1.8.30): the rectangle that part is stretched over,
    /// relative to the frame.
    pub(crate) fill: RelativeRect,
}

/// A rectangle as `(x, y, width, height)`.
pub(crate) type Xywh = (f64, f64, f64, f64);

/// How a picture turns on its frame (`a:xfrm`, §20.1.7.6): mirrored left to
/// right about the frame's centre when `flip_h`, top to bottom when `flip_v`,
/// then rotated clockwise about the centre by `rot` radians. The frame is
/// where the picture lies before it turns; the default turns nothing.
///
/// The flips come before the rotation, so a mirrored picture keeps its
/// rotation: the spec names the three without ordering them, and this is
/// the order the canvas transform in [`Transform::paint`] applies.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Transform {
    pub(crate) rot: f64,
    pub(crate) flip_h: bool,
    pub(crate) flip_v: bool,
}

impl Transform {
    /// The transform of an `a:xfrm` whose `@rot` is `rot` (`ST_Angle`,
    /// §20.1.10.3: 60000ths of a degree, positive clockwise; absent is 0)
    /// and whose `@flipH` and `@flipV` are the two flips. Whole turns are
    /// dropped, so a picture turned 360° is upright.
    pub(crate) fn of(rot: Option<i64>, flip_h: bool, flip_v: bool) -> Self {
        let rot = rot.unwrap_or(0).rem_euclid(21_600_000);
        Transform { rot: (rot as f64 / 60_000.0).to_radians(), flip_h, flip_v }
    }

    /// The transform of an `a:xfrm` whose attributes `attr` reads: [`of`](Self::of)
    /// its `rot`, read as an integer, and its `flipH` and `flipV`, each on
    /// when `1` or `true` (`xsd:boolean`). An absent or unreadable one is 0
    /// or off.
    pub(crate) fn from_attrs<'a>(attr: impl Fn(&str) -> Option<&'a str>) -> Self {
        let flip = |name| attr(name).is_some_and(|v| matches!(v.trim(), "1" | "true"));
        Self::of(attr("rot").and_then(|v| v.trim().parse().ok()), flip("flipH"), flip("flipV"))
    }

    /// The transform of a picture drawn with `style`.
    pub(crate) fn of_style(style: Option<&PictureStyle>) -> Self {
        style.map_or_else(Self::default, |s| s.transform)
    }

    /// Whether it rotates the frame: a flip alone leaves the frame where it is.
    pub(crate) fn rotates(&self) -> bool {
        self.rot != 0.0
    }

    /// `(x, y)` rotated by `angle` radians, clockwise on the page, about the
    /// centre of `frame`.
    fn rotate_about(frame: Xywh, (x, y): (f64, f64), angle: f64) -> (f64, f64) {
        let (cx, cy) = (frame.0 + frame.2 / 2.0, frame.1 + frame.3 / 2.0);
        let (sin, cos) = angle.sin_cos();
        let (dx, dy) = (x - cx, y - cy);
        (cx + dx * cos - dy * sin, cy + dx * sin + dy * cos)
    }

    /// Where the four corners of `frame` land once it turns. A flip only
    /// swaps the corners among themselves, so the rotation alone places them.
    pub(crate) fn corners(&self, frame: Xywh) -> [(f64, f64); 4] {
        let (x, y, w, h) = frame;
        [(x, y), (x + w, y), (x + w, y + h), (x, y + h)].map(|pt| Self::rotate_about(frame, pt, self.rot))
    }

    /// The upright box `frame` covers once it turns: `frame` itself when it
    /// is not rotated.
    pub(crate) fn bounds(&self, frame: Xywh) -> Xywh {
        if !self.rotates() {
            return frame;
        }
        let corners = self.corners(frame);
        let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (x, y) in corners {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
        (x0, y0, x1 - x0, y1 - y0)
    }

    /// The frame of size `(w, h)` whose turned box is `held`: centred in it,
    /// as a frame turns about its centre. An unrotated frame is `held` itself.
    pub(crate) fn frame_within(&self, held: Xywh, (w, h): (f64, f64)) -> Xywh {
        if !self.rotates() {
            return held;
        }
        let (cx, cy) = (held.0 + held.2 / 2.0, held.1 + held.3 / 2.0);
        (cx - w / 2.0, cy - h / 2.0, w, h)
    }

    /// Whether `frame`, once it turns, covers `(x, y)`: the point turned back
    /// about the frame's centre lies in the frame, its left and top edges in
    /// and its right and bottom edges out.
    pub(crate) fn contains(&self, frame: Xywh, x: f64, y: f64) -> bool {
        let (x, y) = if self.rotates() { Self::rotate_about(frame, (x, y), -self.rot) } else { (x, y) };
        let (fx, fy, w, h) = frame;
        x >= fx && x < fx + w && y >= fy && y < fy + h
    }

    /// Run `draw`, which paints in page coordinates as if the picture in
    /// `frame` were upright, with what it paints turned with the frame: under
    /// a canvas transform that moves the frame's centre to the origin, mirrors
    /// and rotates about it and moves it back, restored after. A transform
    /// that turns nothing adds nothing to what `draw` paints.
    ///
    /// The first translate absorbs a windowed painter's offset and the rest
    /// pass through it unchanged, so the frame turns about the same page
    /// point in a window as on the whole page.
    pub(crate) fn paint(&self, p: &dyn Painter, frame: Xywh, draw: impl FnOnce() -> Result<(), JsValue>) -> Result<(), JsValue> {
        if *self == Self::default() {
            return draw();
        }
        let (cx, cy) = (frame.0 + frame.2 / 2.0, frame.1 + frame.3 / 2.0);
        let sign = |flip: bool| if flip { -1.0 } else { 1.0 };
        p.save();
        let drawn = p
            .translate(cx, cy)
            .and_then(|()| if self.rotates() { p.rotate(self.rot) } else { Ok(()) })
            .and_then(|()| if self.flip_h || self.flip_v { p.scale(sign(self.flip_h), sign(self.flip_v)) } else { Ok(()) })
            .and_then(|()| p.translate(-cx, -cy))
            .and_then(|()| draw());
        p.restore();
        drawn
    }
}

/// How a picture is drawn other than as its whole bitmap, opaque, upright and
/// stretched over its frame: its crop and opacity, read from its fill
/// (`a:blipFill`, §20.1.8.14), its outline, read from its shape properties,
/// and how it turns, read from their transform. One value for every format,
/// carried whole from the markup to the paint by every picture carrier, which
/// holds `None` for a picture that sets none of them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PictureStyle {
    /// The outline (`a:ln`, §20.1.2.2.24), drawn over the picture.
    pub(crate) outline: Option<Edge>,
    /// The crop, `None` when it is all zero.
    pub(crate) crop: Option<ImageCrop>,
    /// The opacity the bitmap draws at, from 0 (invisible) to 1 (opaque).
    /// The outline draws at its own colour's opacity instead.
    pub(crate) alpha: f64,
    /// How the picture, bitmap and outline together, turns on its frame.
    pub(crate) transform: Transform,
}

impl PictureStyle {
    /// The style of a picture cropped by `crop`, whose blip (`a:blip`,
    /// §20.1.8.13) carries an alpha modulate fixed effect (`a:alphaModFix`,
    /// §20.1.8.6) with each `amt` of `alpha_mod_fix`, whose line (`a:ln`,
    /// §20.1.2.2.24) draws in `line_color` (`None` for no line, a `noFill`
    /// line, or a fill not drawn here) and is `line_width_emu` (`@w`) wide,
    /// and that turns by `transform`. `None` when it draws none of them, as
    /// most pictures do.
    ///
    /// Each effect multiplies the bitmap's opacity by its `amt`, 100 % when
    /// absent (the schema default) or unreadable, the result kept within 0
    /// to 100 % after each.
    ///
    /// Deviations, declared: a line without `@w` is drawn 9525 EMU (1px)
    /// wide, where the spec reads an absent width as 0; not measured in
    /// Office. A line of zero width draws nothing. The dash, compound line,
    /// pen alignment, caps and joins are not read: the outline is a solid
    /// single stroke. The opacity applies to the whole bitmap at once, so an
    /// `amt` above 100 % cannot raise a translucent pixel of the bitmap
    /// itself and draws it as it is. The blip's other effects are not read.
    pub(crate) fn of<'a>(
        crop: ImageCrop,
        alpha_mod_fix: impl IntoIterator<Item = Option<&'a str>>,
        line_color: Option<String>,
        line_width_emu: Option<i64>,
        transform: Transform,
    ) -> Option<Box<Self>> {
        let width_px = emu_to_px(line_width_emu.unwrap_or(9_525));
        let outline = line_color
            .filter(|_| width_px > 0.0)
            .map(|color| Edge { color, width_px, dash: None, style: LineStyle::Single });
        let crop = (crop != ImageCrop::default()).then_some(crop);
        let alpha = alpha_mod_fix
            .into_iter()
            .map(|amt| amt.and_then(parse_dml_pct).unwrap_or(1.0))
            .fold(1.0, |alpha, amt| (alpha * amt).clamp(0.0, 1.0));
        (outline.is_some() || crop.is_some() || alpha < 1.0 || transform != Transform::default())
            .then(|| Box::new(PictureStyle { outline, crop, alpha, transform }))
    }
}

/// Draw picture `key`'s bitmap into its frame at its opacity, then its
/// outline over it, both turned with the frame by its transform.
///
/// A cropped picture draws the part of its bitmap the crop keeps, where
/// [`crop_draw`] places it, and nothing when it keeps none; an uncropped one
/// draws the whole bitmap over the frame.
///
/// Declared, not measured in Office: the outline is a solid line centred on
/// the frame's edge (the `ctr` pen of §20.1.10.39; `a:ln@algn` is not read),
/// so half of it lies outside the picture. It takes no space in the layout.
pub(crate) fn draw_picture(p: &dyn Painter, key: &str, frame: Xywh, style: Option<&PictureStyle>) -> Result<(), JsValue> {
    let (x, y, w, h) = frame;
    Transform::of_style(style).paint(p, frame, || {
        let alpha = style.map_or(1.0, |s| s.alpha);
        if alpha < 1.0 {
            p.save();
            p.set_global_alpha(alpha);
        }
        let drawn = match style.and_then(|s| s.crop.as_ref()) {
            None => p.draw_image(key, x, y, w, h),
            Some(crop) => match crop_draw(frame, crop) {
                Some(((sx, sy, sw, sh), (dx, dy, dw, dh))) => p.draw_image_crop(key, sx, sy, sw, sh, dx, dy, dw, dh),
                None => Ok(()),
            },
        };
        if alpha < 1.0 {
            p.restore();
        }
        drawn?;
        if let Some(line) = style.and_then(|s| s.outline.as_ref()) {
            p.set_stroke_style_str(&line.color);
            p.set_line_width(line.width_px);
            p.stroke_rect(x, y, w, h);
        }
        Ok(())
    })
}

/// Where a cropped picture in `frame` draws: the part of the bitmap as
/// fractions of its width and height `(sx, sy, sw, sh)`, each within 0 to 1,
/// and the page rectangle it fills `(dx, dy, dw, dh)`. `None` when none of the
/// bitmap shows.
///
/// The fill rectangle is the frame inset by `crop.fill` (§20.1.8.30), and the
/// source rectangle `crop.src` (§20.1.8.55) is stretched over it. Where the
/// source reaches past the bitmap's edge, nothing is drawn there: the source
/// is clamped to the bitmap and the destination shrinks by the same
/// proportion, so the frame shows empty where the outset is. Where the fill
/// rectangle reaches past the frame, the part outside is cut off the same
/// way, as the picture is the frame's fill.
pub(crate) fn crop_draw(frame: Xywh, crop: &ImageCrop) -> Option<(Xywh, Xywh)> {
    let (x, y, w, h) = frame;
    let (sx, sw, dx, dw) = crop_span(x, w, (crop.fill.l, crop.fill.r), (crop.src.l, crop.src.r))?;
    let (sy, sh, dy, dh) = crop_span(y, h, (crop.fill.t, crop.fill.b), (crop.src.t, crop.src.b))?;
    Some(((sx, sy, sw, sh), (dx, dy, dw, dh)))
}

/// One axis of [`crop_draw`]: the frame span `at..at + len`, the fill
/// rectangle's two edge offsets (fractions of `len`) and the source's (of the
/// bitmap), to the drawn source span `(start, len)` in bitmap fractions and
/// destination span `(start, len)`.
fn crop_span(
    at: f64,
    len: f64,
    (fill0, fill1): (f64, f64),
    (src0, src1): (f64, f64),
) -> Option<(f64, f64, f64, f64)> {
    let fill_at = at + fill0 * len;
    let fill_len = len * (1.0 - fill0 - fill1);
    let src_len = 1.0 - src0 - src1;
    // Written so a NaN fails it too.
    if !(fill_len > 0.0 && src_len > 0.0) {
        return None;
    }
    let scale = fill_len / src_len;
    // The source that lies on the bitmap (0 to 1) and, once placed, in the frame.
    let start = src0.max(0.0).max(src0 - fill0 * len / scale);
    let end = (src0 + src_len)
        .min(1.0)
        .min(src0 + (1.0 - fill0) * len / scale);
    (end > start).then_some((
        start,
        end - start,
        fill_at + (start - src0) * scale,
        (end - start) * scale,
    ))
}

#[cfg(test)]
mod tests {
    use super::{crop_draw, ImageCrop, PictureStyle, RelativeRect, Transform, Xywh};

    const FRAME: Xywh = (10.0, 20.0, 200.0, 100.0);

    /// The opacity of an otherwise plain picture whose blip carries an
    /// `a:alphaModFix` with each of `amts`, 1 for one with no style.
    fn opacity(amts: &[Option<&str>]) -> f64 {
        PictureStyle::of(ImageCrop::default(), amts.iter().copied(), None, None, Transform::default()).map_or(1.0, |s| s.alpha)
    }

    /// §20.1.8.6: each effect multiplies the opacity by its `amt`, 100 % when
    /// absent or unreadable, kept within 0 to 100 % after each step.
    #[test]
    fn alpha_mod_fix_multiplies_the_opacity() {
        assert_eq!(opacity(&[Some("40000")]), 0.4);
        assert_eq!(opacity(&[Some("40%")]), 0.4);
        assert_eq!(opacity(&[Some("50000"), Some("50000")]), 0.25);
        assert_eq!(opacity(&[Some("-10000")]), 0.0);
        // Above 100 % the opaque bitmap stays opaque, so a later 50 % halves it.
        assert_eq!(opacity(&[Some("200000"), Some("50000")]), 0.5);
        let opaque: [&[Option<&str>]; 5] = [&[], &[None], &[Some("100000")], &[Some("200000")], &[Some("x")]];
        for opaque in opaque {
            assert!(PictureStyle::of(ImageCrop::default(), opaque.iter().copied(), None, None, Transform::default()).is_none(), "{opaque:?} is no style");
        }
    }

    // ── Turned pictures (`a:xfrm`, §20.1.7.6) ───────────────────────────────

    const QUARTER: f64 = std::f64::consts::FRAC_PI_2;

    fn assert_points(got: &[(f64, f64)], want: &[(f64, f64)], what: &str) {
        assert_eq!(got.len(), want.len(), "{what}");
        for (g, w) in got.iter().zip(want) {
            assert!((g.0 - w.0).abs() < 1e-9 && (g.1 - w.1).abs() < 1e-9, "{what}: got {got:?}, want {want:?}");
        }
    }

    /// Where the corners of `frame` (top-left, top-right, bottom-right,
    /// bottom-left) land a quarter turn clockwise about its centre: the
    /// top-left goes to the top-right of the box it then stands in.
    fn quarter_turned((x, y, w, h): Xywh) -> [(f64, f64); 4] {
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        [(cx + h / 2.0, cy - w / 2.0), (cx + h / 2.0, cy + w / 2.0), (cx - h / 2.0, cy + w / 2.0), (cx - h / 2.0, cy - w / 2.0)]
    }

    /// `ST_Angle` (§20.1.10.3) is in 60000ths of a degree, positive clockwise;
    /// a whole turn is upright.
    #[test]
    fn an_xfrm_angle_is_sixty_thousandths_of_a_degree() {
        assert!((Transform::of(Some(5_400_000), false, false).rot - QUARTER).abs() < 1e-12);
        assert!((Transform::of(Some(-5_400_000), false, false).rot - 3.0 * QUARTER).abs() < 1e-12);
        for upright in [None, Some(0), Some(21_600_000), Some(-43_200_000)] {
            assert_eq!(Transform::of(upright, false, false), Transform::default(), "{upright:?}");
        }
    }

    /// A 200×50 frame turned a quarter stands 50 wide and 200 tall about the
    /// same centre; an unturned frame is its own box, exactly.
    #[test]
    fn a_quarter_turn_stands_the_frame_on_end() {
        let frame = (10.0, 20.0, 200.0, 50.0);
        let turn = Transform::of(Some(5_400_000), false, false);
        assert_points(&turn.corners(frame), &quarter_turned(frame), "corners");
        let (x, y, w, h) = turn.bounds(frame);
        assert_points(&[(x, y), (w, h)], &[(85.0, -55.0), (50.0, 200.0)], "bounds");
        for upright in [Transform::default(), Transform::of(None, true, true)] {
            assert_eq!(upright.bounds(frame), frame);
        }
    }

    /// A frame turned 45° is a diamond in its box: a point it covers is in,
    /// also outside the unturned frame, and a corner of its box it leaves
    /// bare is out, also inside the unturned frame.
    #[test]
    fn a_turned_frame_covers_only_its_own_area() {
        let frame = (0.0, 0.0, 100.0, 100.0);
        let turn = Transform::of(Some(2_700_000), false, false);
        let (bx, by, bw, bh) = turn.bounds(frame);
        assert_points(&[(bx, by), (bw, bh)], &[(50.0 - 50.0 * 2f64.sqrt(), 50.0 - 50.0 * 2f64.sqrt()), (100.0 * 2f64.sqrt(), 100.0 * 2f64.sqrt())], "bounds");
        assert!(turn.contains(frame, 50.0, 50.0), "the centre");
        assert!(turn.contains(frame, -15.0, 50.0), "the left tip, outside the unturned frame");
        assert!(!turn.contains(frame, 5.0, 5.0), "the unturned frame's corner, left bare");
        assert!(!turn.contains(frame, bx + 1.0, by + 1.0), "the corner of the box");
        // Unturned, the edges are as for any box: left and top in, right and bottom out.
        let upright = Transform::default();
        assert!(upright.contains(frame, 0.0, 0.0) && !upright.contains(frame, 100.0, 50.0) && !upright.contains(frame, 50.0, 100.0));
    }

    /// The corners of each rect `draw_picture` records for a 1px-outlined
    /// picture turned by `turn`, on the page and in a window whose origin is
    /// at `(30, 40)`, put back on the page.
    fn drawn_corners(frame: Xywh, turn: Transform) -> [Vec<[(f64, f64); 4]>; 2] {
        use crate::engine::displaylist::{corners_of_each_rect, RecordingPainter};
        let style = PictureStyle::of(ImageCrop::default(), [], Some("#ff0000".into()), None, turn);
        let page = RecordingPainter::new();
        super::draw_picture(&page, "pic", frame, style.as_deref()).unwrap();
        let window = RecordingPainter::new();
        super::draw_picture(&crate::engine::paint::OffsetPainter::new(&window, 30.0, 40.0), "pic", frame, style.as_deref()).unwrap();
        let page = corners_of_each_rect(&page.take()).into_iter().map(|(_, c)| c).collect();
        let window = corners_of_each_rect(&window.take()).into_iter().map(|(_, c)| c.map(|(x, y)| (x + 30.0, y + 40.0))).collect();
        [page, window]
    }

    /// The bitmap and its outline turn together about the frame's centre, on
    /// the page and in a window: a quarter turn stands them on end, `flipH`
    /// puts the bitmap's left edge on the frame's right, and the flip comes
    /// before the turn.
    #[test]
    fn a_turned_picture_draws_its_bitmap_and_outline_turned() {
        let frame = (10.0, 20.0, 200.0, 50.0);
        let (x, y, w, h) = frame;
        let q = quarter_turned(frame);
        for (what, turn, want) in [
            ("a quarter turn", Transform::of(Some(5_400_000), false, false), q),
            ("flipH", Transform::of(None, true, false), [(x + w, y), (x, y), (x, y + h), (x + w, y + h)]),
            ("flipV", Transform::of(None, false, true), [(x, y + h), (x + w, y + h), (x + w, y), (x, y)]),
            ("flipH, then a quarter turn", Transform::of(Some(5_400_000), true, false), [q[1], q[0], q[3], q[2]]),
        ] {
            for drawn in drawn_corners(frame, turn) {
                assert_eq!(drawn.len(), 2, "{what}: the bitmap and its outline");
                for rect in drawn {
                    assert_points(&rect, &want, what);
                }
            }
        }
    }

    /// A picture that does not turn records exactly what it did before
    /// pictures could turn: no save, no transform.
    #[test]
    fn an_unturned_picture_records_no_transform() {
        use crate::engine::displaylist::{DrawCmd, RecordingPainter};
        let rec = RecordingPainter::new();
        let style = PictureStyle::of(ImageCrop::default(), [], Some("#ff0000".into()), None, Transform::default());
        super::draw_picture(&rec, "pic", FRAME, style.as_deref()).unwrap();
        let (x, y, w, h) = FRAME;
        assert_eq!(
            rec.take(),
            [
                DrawCmd::DrawImage { key: "pic".into(), dx: x, dy: y, dw: w, dh: h },
                DrawCmd::SetStrokeStyle("#ff0000".into()),
                DrawCmd::SetLineWidth(1.0),
                DrawCmd::StrokeRect { x, y, w, h },
            ]
        );
    }

    /// A crop from its source and fill edges, each `(l, t, r, b)`.
    fn crop(src: (f64, f64, f64, f64), fill: (f64, f64, f64, f64)) -> ImageCrop {
        let rect = |(l, t, r, b)| RelativeRect { l, t, r, b };
        ImageCrop {
            src: rect(src),
            fill: rect(fill),
        }
    }

    fn assert_draw(got: Option<(Xywh, Xywh)>, src: [f64; 4], dst: [f64; 4]) {
        let ((sx, sy, sw, sh), (dx, dy, dw, dh)) = got.expect("part of the bitmap shows");
        for (what, got, want) in [
            ("source", [sx, sy, sw, sh], src),
            ("destination", [dx, dy, dw, dh], dst),
        ] {
            for (g, w) in got.iter().zip(want) {
                assert!((g - w).abs() < 1e-9, "{what}: got {got:?}, want {want:?}");
            }
        }
    }

    /// A 25% left crop draws the right three quarters of the bitmap over the
    /// whole frame.
    #[test]
    fn a_left_crop_draws_the_rest_of_the_bitmap_over_the_frame() {
        let got = crop_draw(FRAME, &crop((0.25, 0.0, 0.0, 0.0), (0.0, 0.0, 0.0, 0.0)));
        assert_draw(got, [0.25, 0.0, 0.75, 1.0], [10.0, 20.0, 200.0, 100.0]);
    }

    #[test]
    fn a_crop_on_all_four_sides_draws_the_middle() {
        let got = crop_draw(FRAME, &crop((0.1, 0.2, 0.3, 0.4), (0.0, 0.0, 0.0, 0.0)));
        assert_draw(got, [0.1, 0.2, 0.6, 0.4], [10.0, 20.0, 200.0, 100.0]);
    }

    /// A negative left offset (Word's "Fit" leaves one) puts empty space
    /// before the bitmap: the source is a quarter wider than the bitmap, so
    /// the bitmap fills the frame's right four fifths.
    #[test]
    fn a_source_outset_leaves_the_frame_empty_there() {
        let got = crop_draw(FRAME, &crop((-0.25, 0.0, 0.0, -0.25), (0.0, 0.0, 0.0, 0.0)));
        assert_draw(got, [0.0, 0.0, 1.0, 1.0], [50.0, 20.0, 160.0, 80.0]);
    }

    /// `fillRect b="10000" r="25000"`, the spec's example: the whole bitmap
    /// fills all but the frame's bottom 10% and right 25%.
    #[test]
    fn a_fill_inset_shrinks_where_the_bitmap_draws() {
        let got = crop_draw(FRAME, &crop((0.0, 0.0, 0.0, 0.0), (0.0, 0.0, 0.25, 0.1)));
        assert_draw(got, [0.0, 0.0, 1.0, 1.0], [10.0, 20.0, 150.0, 90.0]);
    }

    /// The source half of the bitmap is stretched over the fill rectangle,
    /// the frame's middle half.
    #[test]
    fn a_crop_and_a_fill_inset_together() {
        let got = crop_draw(FRAME, &crop((0.5, 0.0, 0.0, 0.5), (0.25, 0.25, 0.25, 0.25)));
        assert_draw(got, [0.5, 0.0, 0.5, 0.5], [60.0, 45.0, 100.0, 50.0]);
    }

    /// A fill rectangle wider than the frame is cut at the frame: half of it
    /// lies left of the frame, so the left third of the bitmap is not drawn.
    #[test]
    fn a_fill_outset_is_cut_at_the_frame() {
        let got = crop_draw(FRAME, &crop((0.0, 0.0, 0.0, 0.0), (-0.5, 0.0, 0.0, 0.0)));
        assert_draw(
            got,
            [1.0 / 3.0, 0.0, 2.0 / 3.0, 1.0],
            [10.0, 20.0, 200.0, 100.0],
        );
    }

    /// A source or fill rectangle with no width or height, or turned inside
    /// out, draws nothing (and divides by nothing).
    #[test]
    fn an_empty_rectangle_draws_nothing() {
        let none = (0.0, 0.0, 0.0, 0.0);
        for (src, fill) in [
            ((0.5, 0.0, 0.5, 0.0), none),
            ((0.6, 0.0, 0.6, 0.0), none),
            ((0.0, 0.7, 0.0, 0.3), none),
            (none, (0.5, 0.0, 0.5, 0.0)),
            (none, (0.0, 0.0, 0.0, 1.5)),
            // The bitmap lies wholly outside the source: nothing of it shows.
            ((1.0, 0.0, -2.0, 0.0), none),
        ] {
            assert_eq!(
                crop_draw(FRAME, &crop(src, fill)),
                None,
                "src {src:?}, fill {fill:?}"
            );
        }
        assert_eq!(
            crop_draw((0.0, 0.0, 0.0, 50.0), &crop((0.25, 0.0, 0.0, 0.0), none)),
            None,
            "a frame with no width"
        );
    }
}
