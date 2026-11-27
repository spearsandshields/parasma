//! Images, for every format: the media part types, the placeholder drawn for
//! an image not yet decoded, and where a cropped picture draws.

use wasm_bindgen::JsValue;

use crate::engine::paint::Painter;

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
    use super::{crop_draw, ImageCrop, RelativeRect, Xywh};

    const FRAME: Xywh = (10.0, 20.0, 200.0, 100.0);

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
