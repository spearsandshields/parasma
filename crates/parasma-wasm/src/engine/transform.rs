//! How a drawing turns on its frame (`a:xfrm`, §20.1.7.6), for every format
//! and every kind of drawing: pictures, shapes and the text they hold,
//! connectors, graphic frames and Word text boxes. Where a turned frame
//! lands, where a point on the page lies in it, and the paint that turns
//! what it draws.

use wasm_bindgen::JsValue;

use crate::engine::paint::Painter;

/// A rectangle as `(x, y, width, height)`.
pub(crate) type Xywh = (f64, f64, f64, f64);

/// How a drawing turns on its frame (`a:xfrm`, §20.1.7.6): mirrored left to
/// right about the frame's centre when `flip_h`, top to bottom when `flip_v`,
/// then rotated clockwise about the centre by `rot` radians. The frame is
/// where the drawing lies before it turns; the default turns nothing.
///
/// The flips come before the rotation, so a mirrored drawing keeps its
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
    /// dropped, so a drawing turned 360° is upright.
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

    /// The transform of the text a shape that turns by `self` holds
    /// (`txBody/bodyPr`, §21.1.2.1.1): the shape's rotation and a further
    /// `body_rot` (`bodyPr@rot`, `ST_Angle`), or none at all when `upright`
    /// (`bodyPr@upright`), which keeps the text upright whatever turns the
    /// shape or the text.
    ///
    /// Neither flip mirrors the text: a horizontal flip leaves it as it was,
    /// and a vertical flip turns it half round, as mirroring top to bottom
    /// does to a frame that is then mirrored back left to right. This is what
    /// PowerPoint draws, observed in its UI and not measured; the spec says
    /// nothing of text in a flipped shape.
    pub(crate) fn text(&self, body_rot: Option<i64>, upright: bool) -> Self {
        if upright {
            return Self::default();
        }
        let half_turn = if self.flip_v { 10_800_000 } else { 0 };
        Self::of(Some(self.angle() + half_turn + body_rot.unwrap_or(0)), false, false)
    }

    /// The rotation in whole 60000ths of a degree (`ST_Angle`), so angles
    /// compare and add exactly.
    pub(crate) fn angle(&self) -> i64 {
        (self.rot.to_degrees() * 60_000.0).round() as i64
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

    /// `(x, y)` mirrored about the centre of `frame` by each flip.
    fn flip_about(&self, frame: Xywh, (x, y): (f64, f64)) -> (f64, f64) {
        let x = if self.flip_h { 2.0 * frame.0 + frame.2 - x } else { x };
        let y = if self.flip_v { 2.0 * frame.1 + frame.3 - y } else { y };
        (x, y)
    }

    /// Where `(x, y)`, drawn in `frame` as if it were upright, lands once the
    /// frame turns: the same point when nothing turns.
    pub(crate) fn map(&self, frame: Xywh, pt: (f64, f64)) -> (f64, f64) {
        let pt = self.flip_about(frame, pt);
        if self.rotates() { Self::rotate_about(frame, pt, self.rot) } else { pt }
    }

    /// The point drawn in `frame` as if it were upright that lands at
    /// `(x, y)` once the frame turns: [`map`](Self::map) undone.
    pub(crate) fn unmap(&self, frame: Xywh, pt: (f64, f64)) -> (f64, f64) {
        let pt = if self.rotates() { Self::rotate_about(frame, pt, -self.rot) } else { pt };
        self.flip_about(frame, pt)
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
        Self::box_of(self.corners(frame))
    }

    /// The upright box `rect`, drawn in `frame` as if it were upright,
    /// covers once the frame turns: `rect` itself when nothing turns.
    pub(crate) fn bounds_in(&self, frame: Xywh, rect: Xywh) -> Xywh {
        if *self == Self::default() {
            return rect;
        }
        let (x, y, w, h) = rect;
        Self::box_of([(x, y), (x + w, y), (x + w, y + h), (x, y + h)].map(|pt| self.map(frame, pt)))
    }

    /// The upright box holding `points`.
    fn box_of(points: [(f64, f64); 4]) -> Xywh {
        let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (x, y) in points {
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

    /// Run `draw`, which paints in page coordinates as if the drawing in
    /// `frame` were upright, with what it paints turned with the frame: under
    /// a canvas transform that moves the frame's centre to the origin, mirrors
    /// and rotates about it and moves it back, restored after. A transform
    /// that turns nothing adds nothing to what `draw` paints.
    ///
    /// The first translate absorbs a windowed painter's offset and the rest
    /// pass through it unchanged, so the frame turns about the same page
    /// point in a window as on the whole page.
    pub(crate) fn paint<T>(&self, p: &dyn Painter, frame: Xywh, draw: impl FnOnce() -> Result<T, JsValue>) -> Result<T, JsValue> {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::{Transform, Xywh};

    const QUARTER: f64 = std::f64::consts::FRAC_PI_2;

    pub(crate) fn assert_points(got: &[(f64, f64)], want: &[(f64, f64)], what: &str) {
        assert_eq!(got.len(), want.len(), "{what}");
        for (g, w) in got.iter().zip(want) {
            assert!((g.0 - w.0).abs() < 1e-9 && (g.1 - w.1).abs() < 1e-9, "{what}: got {got:?}, want {want:?}");
        }
    }

    /// Where the corners of `frame` (top-left, top-right, bottom-right,
    /// bottom-left) land a quarter turn clockwise about its centre: the
    /// top-left goes to the top-right of the box it then stands in.
    pub(crate) fn quarter_turned((x, y, w, h): Xywh) -> [(f64, f64); 4] {
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
        for angle in [0, 1, 5_400_000, 13_500_000, 21_599_999] {
            assert_eq!(Transform::of(Some(angle), false, false).angle(), angle);
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

    /// A point drawn in a frame lands where the frame takes it, and lands
    /// back: a quarter turn takes the frame's top-left corner to the top
    /// right of the box it stands in, `flipH` takes it to the frame's top
    /// right, and the flip comes before the turn. Nothing turns a point
    /// nowhere, exactly.
    #[test]
    fn a_point_in_a_turned_frame_lands_and_lands_back() {
        let frame = (10.0, 20.0, 200.0, 50.0);
        let q = quarter_turned(frame);
        for (what, turn, want) in [
            ("a quarter turn", Transform::of(Some(5_400_000), false, false), q[0]),
            ("flipH", Transform::of(None, true, false), (210.0, 20.0)),
            ("flipV", Transform::of(None, false, true), (10.0, 70.0)),
            ("flipH, then a quarter turn", Transform::of(Some(5_400_000), true, false), q[1]),
        ] {
            let landed = turn.map(frame, (10.0, 20.0));
            assert_points(&[landed], &[want], what);
            assert_points(&[turn.unmap(frame, landed)], &[(10.0, 20.0)], what);
        }
        let odd = (0.1 + 0.2, 1.0 / 3.0);
        assert_eq!(Transform::default().map(frame, odd), odd);
        assert_eq!(Transform::default().unmap(frame, odd), odd);
    }

    /// A rect drawn in a turned frame covers the upright box its corners
    /// land in: the left half of a frame turned a quarter covers its top
    /// half, and mirrored covers its right half. Nothing turns it nowhere.
    #[test]
    fn a_rect_in_a_turned_frame_covers_where_it_lands() {
        let frame = (0.0, 0.0, 200.0, 100.0);
        let left_half = (0.0, 0.0, 100.0, 100.0);
        let (x, y, w, h) = Transform::of(Some(5_400_000), false, false).bounds_in(frame, left_half);
        assert_points(&[(x, y), (w, h)], &[(50.0, -50.0), (100.0, 100.0)], "a quarter turn");
        assert_eq!(Transform::of(None, true, false).bounds_in(frame, left_half), (100.0, 0.0, 100.0, 100.0), "flipH");
        assert_eq!(Transform::default().bounds_in(frame, (0.3, 0.1, 7.0, 9.0)), (0.3, 0.1, 7.0, 9.0));
    }

    /// The text of a shape turns with the shape and by its body's own `rot`;
    /// `upright` keeps it upright; neither flip mirrors it, and a vertical
    /// flip turns it half round.
    #[test]
    fn a_shape_text_turns_with_the_shape_but_never_mirrors() {
        let turn = |rot, h, v| Transform::of(rot, h, v);
        let quarter = Some(5_400_000);
        assert_eq!(turn(quarter, false, false).text(None, false), turn(quarter, false, false));
        assert_eq!(turn(quarter, false, false).text(Some(-5_400_000), false), Transform::default(), "the spec's example");
        assert_eq!(turn(quarter, true, true).text(Some(2_700_000), true), Transform::default(), "upright");
        assert_eq!(turn(None, true, false).text(None, false), Transform::default(), "flipH");
        assert_eq!(turn(None, false, true).text(None, false), turn(Some(10_800_000), false, false), "flipV");
        assert_eq!(turn(quarter, true, true).text(None, false), turn(Some(16_200_000), false, false), "both flips");
        assert_eq!(Transform::default().text(None, false), Transform::default());
    }
}
