//! Windowed-render culling — the web-sys-free geometry the windowed
//! paint path (`render_page_window`) uses to skip items that do not intersect
//! the visible viewport.
//!
//! The point of the windowed path is to bound the *item walk* cost: `render_page`
//! walks every positioned item on a page (O(whole page)); `render_page_window`
//! walks the same vectors but `continue`s past any item whose page-logical-px
//! bbox misses the window (O(visible)). The cull predicate is a plain
//! axis-aligned-rectangle INTERSECTION test (straddling items still paint), so
//! it lives here — pure `f64` geometry, no canvas — and is unit-tested
//! natively (no wasm/canvas needed).
//!
//! `render_page` threads `clip: Option<&Rect>` through the same painters;
//! `clip == None` means "no window" and every `keep` returns `true`, so the
//! full-page draw sequence is byte-identical to an unculled paint. The window
//! arm passes `clip = Some(win)` and the predicate does the culling.

/// A window in page-logical px (the same coordinate space every WML page item
/// and every PML shape/table/picture box lives in). Half-open is irrelevant —
/// the intersection test below is inclusive on the shared edge, which is the
/// safe choice for a *paint* cull (paint one pixel too many, never one too few).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    /// The window covered by a viewport, in page-logical px. `clip_x`/`clip_y`
    /// is the scroll offset (top-left of the viewport in page space); the
    /// viewport spans `view_w`/`view_h` *device* px, which at `zoom` covers
    /// `view_w/zoom` × `view_h/zoom` page-logical px.
    pub(crate) fn from_viewport(clip_x: f64, clip_y: f64, view_w: f64, view_h: f64, zoom: f64) -> Self {
        let z = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
        Rect {
            x0: clip_x,
            y0: clip_y,
            x1: clip_x + view_w / z,
            y1: clip_y + view_h / z,
        }
    }

    /// Does `self` intersect the axis-aligned box `[bx0,bx1] × [by0,by1]`?
    /// INTERSECTION, not containment: an item straddling the window edge still
    /// reports `true` so its visible slice paints. Degenerate (zero-area) boxes
    /// still test by their edge, which is correct for thin rules / separators.
    #[inline]
    pub(crate) fn intersects(&self, bx0: f64, by0: f64, bx1: f64, by1: f64) -> bool {
        // Normalize the box (callers may pass an inverted pair for an empty
        // run); a NaN coordinate makes every comparison false → culled, which
        // is the safe direction (a NaN-bbox item can't be placed anyway).
        let (lo_x, hi_x) = if bx0 <= bx1 { (bx0, bx1) } else { (bx1, bx0) };
        let (lo_y, hi_y) = if by0 <= by1 { (by0, by1) } else { (by1, by0) };
        hi_x >= self.x0 && lo_x <= self.x1 && hi_y >= self.y0 && lo_y <= self.y1
    }
}

/// `clip`-aware keep test: `None` (no window) always keeps; `Some(win)` keeps
/// only when the box intersects. This is the single chokepoint the windowed
/// painters call so the `clip == None` path is provably the identity (every
/// item kept → byte-identical full-page draw sequence).
#[inline]
pub(crate) fn keep(clip: Option<&Rect>, bx0: f64, by0: f64, bx1: f64, by1: f64) -> bool {
    match clip {
        None => true,
        Some(win) => win.intersects(bx0, by0, bx1, by1),
    }
}

/// The rect for the ONE page/slide-spanning background fill under a windowed
/// paint. `None` (full page) returns the whole page box — byte-identical to the
/// pre-window `fill_rect(0,0,page_w,page_h)`. `Some(win)` returns the window ∩
/// page box, so the fill's device extent is bounded by the SLICE (never the
/// page → no Skia overflow at high zoom) WITHOUT repainting page-white into the
/// off-page margin gutter (clamped to the page edge). Returns `None` when the
/// intersection is empty (the window is entirely off-page → nothing to fill).
///
/// Under the `OffsetPainter` the returned `(x0,y0,·,·)` is shifted by `-win.x0`/
/// `-win.y0`, so a normal in-page window fills device `(0,0)…(≤s·win_w, ≤s·win_h)`.
#[inline]
pub(crate) fn bg_rect(clip: Option<&Rect>, page_w: f64, page_h: f64) -> Option<(f64, f64, f64, f64)> {
    match clip {
        None => Some((0.0, 0.0, page_w, page_h)),
        Some(win) => {
            let x0 = win.x0.max(0.0);
            let y0 = win.y0.max(0.0);
            let x1 = win.x1.min(page_w);
            let y1 = win.y1.min(page_h);
            if x1 > x0 && y1 > y0 {
                Some((x0, y0, x1 - x0, y1 - y0))
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win() -> Rect {
        // A 100×100 window at (50,50).
        Rect { x0: 50.0, y0: 50.0, x1: 150.0, y1: 150.0 }
    }

    #[test]
    fn from_viewport_divides_by_zoom() {
        // 200 device px at 2× zoom covers 100 page px.
        let r = Rect::from_viewport(10.0, 20.0, 200.0, 200.0, 2.0);
        assert_eq!(r, Rect { x0: 10.0, y0: 20.0, x1: 110.0, y1: 120.0 });
        // zoom 0 / NaN clamps to 1.
        let r0 = Rect::from_viewport(0.0, 0.0, 80.0, 40.0, 0.0);
        assert_eq!(r0, Rect { x0: 0.0, y0: 0.0, x1: 80.0, y1: 40.0 });
    }

    #[test]
    fn contained_box_intersects() {
        assert!(win().intersects(60.0, 60.0, 90.0, 90.0));
    }

    #[test]
    fn straddling_edge_intersects() {
        // Box overlaps only the left edge band — must still keep (paint slice).
        assert!(win().intersects(0.0, 70.0, 60.0, 80.0));
        // Touching exactly on the edge counts as intersecting (inclusive).
        assert!(win().intersects(150.0, 100.0, 200.0, 110.0));
    }

    #[test]
    fn fully_outside_does_not_intersect() {
        assert!(!win().intersects(0.0, 0.0, 40.0, 40.0)); // above-left
        assert!(!win().intersects(200.0, 200.0, 300.0, 300.0)); // below-right
        assert!(!win().intersects(60.0, 0.0, 90.0, 40.0)); // directly above
        assert!(!win().intersects(160.0, 60.0, 200.0, 90.0)); // directly right
    }

    #[test]
    fn keep_none_is_identity() {
        // No window → every box (even a wildly-outside one) is kept.
        assert!(keep(None, -1e9, -1e9, -1e9 + 1.0, -1e9 + 1.0));
        assert!(keep(None, 1e9, 1e9, 1e9 + 1.0, 1e9 + 1.0));
    }

    #[test]
    fn keep_some_culls_outside() {
        let w = win();
        assert!(keep(Some(&w), 60.0, 60.0, 90.0, 90.0));
        assert!(!keep(Some(&w), 0.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn inverted_box_is_normalized() {
        // A caller passing (x1,x0) for an empty run still tests correctly.
        assert!(win().intersects(90.0, 90.0, 60.0, 60.0));
    }

    // ── Per-format item-walk cull ───────────────────────────────────────────
    // The windowed painters cull each positioned item by the SAME bbox the
    // real painter would use (`render_page_window` reproduces these boxes from
    // the live item structs). These tests assert the load-bearing property of
    // the windowed walk WITHOUT a canvas: for a FULL-PAGE window every item is
    // kept (== `render_page` output set), and for a TINY window the off-screen
    // items are provably dropped while the on-screen ones survive. The boxes
    // below match the painter formulae:
    //   WML line   : [content_left, baseline-line_height] .. [content_right, baseline+0.3·line_height]
    //   WML row    : [min cell.x, y_top] .. [max cell.x+w, y_top+height]
    //   WML img/tb : [x, y] .. [x+w, y+h]
    //   PML/SML box: [x, y] .. [x+w, y+h]  (cells, shapes, pictures, charts)

    /// (x0,y0,x1,y1) bbox of a WML line from its baseline/height/extents.
    fn wml_line_box(content_left: f64, content_right: f64, baseline: f64, lh: f64) -> (f64, f64, f64, f64) {
        (content_left, baseline - lh, content_right, baseline + lh * 0.3)
    }

    #[test]
    fn full_page_window_keeps_every_item_all_formats() {
        // A window covering the whole 816×1056 US-Letter page.
        let page = Rect { x0: 0.0, y0: 0.0, x1: 816.0, y1: 1056.0 };
        let clip = Some(&page);

        // WML lines stacked down the page — every one kept.
        for i in 0..50 {
            let baseline = 80.0 + i as f64 * 18.0;
            let (a, b, c, d) = wml_line_box(72.0, 744.0, baseline, 16.0);
            assert!(keep(clip, a, b, c, d), "line {i} dropped under full window");
        }
        // WML table row, anchored image, text box.
        assert!(keep(clip, 72.0, 400.0, 744.0, 460.0)); // row
        assert!(keep(clip, 300.0, 500.0, 500.0, 650.0)); // image x,y,w,h box
        // PML/SML boxes (shape, picture, chart, cell) anywhere on the page.
        assert!(keep(clip, 10.0, 10.0, 110.0, 60.0));
        assert!(keep(clip, 700.0, 980.0, 800.0, 1040.0));
    }

    #[test]
    fn tiny_window_drops_offscreen_keeps_onscreen() {
        // A 40×40 window near the top-left of the page.
        let tiny = Rect { x0: 60.0, y0: 60.0, x1: 100.0, y1: 100.0 };
        let clip = Some(&tiny);

        // ON-screen WML line crossing the window — KEPT.
        let (a, b, c, d) = wml_line_box(72.0, 744.0, 90.0, 16.0); // band 74..94.8
        assert!(keep(clip, a, b, c, d), "on-screen line wrongly culled");

        // OFF-screen WML line far below — DROPPED.
        let (a, b, c, d) = wml_line_box(72.0, 744.0, 900.0, 16.0); // band 884..904.8
        assert!(!keep(clip, a, b, c, d), "off-screen line wrongly kept");

        // OFF-screen WML table row below the window — DROPPED.
        assert!(!keep(clip, 72.0, 400.0, 744.0, 460.0));
        // ON-screen table row overlapping the window band — KEPT.
        assert!(keep(clip, 72.0, 80.0, 744.0, 120.0));

        // PML/SML boxes: a shape inside the window kept, one far away dropped.
        assert!(keep(clip, 70.0, 70.0, 90.0, 90.0));
        assert!(!keep(clip, 400.0, 400.0, 500.0, 500.0)); // picture/chart/cell off-screen
        // A shape straddling the window's right edge still paints (INTERSECTION).
        assert!(keep(clip, 90.0, 70.0, 300.0, 85.0));
    }

    #[test]
    fn watermark_rotation_safe_box_keeps_when_visible() {
        // The watermark cull box is the half-diagonal radius about the page
        // centre (rotation-safe). A page-centred watermark is visible from a
        // centre window and culled from a far-corner tiny window.
        let (pw, ph) = (816.0_f64, 1056.0_f64);
        let (bw, bh) = (600.0_f64, 120.0_f64);
        let r = 0.5 * (bw * bw + bh * bh).sqrt();
        let cx = pw / 2.0;
        let cy = ph / 2.0;

        let centre = Rect { x0: cx - 20.0, y0: cy - 20.0, x1: cx + 20.0, y1: cy + 20.0 };
        assert!(keep(Some(&centre), cx - r, cy - r, cx + r, cy + r));

        // A 10×10 window in the very top-left corner — outside the radius box.
        let corner = Rect { x0: 0.0, y0: 0.0, x1: 10.0, y1: 10.0 };
        assert!(!keep(Some(&corner), cx - r, cy - r, cx + r, cy + r));
    }

    #[test]
    fn footnote_band_culled_above_keeps_at_bottom() {
        // The footnote band's cull box is [margin_left, margin_top] ..
        // [page_w - margin_right, content_bottom] (anchored at the page bottom,
        // bounded above by the top margin). A window in the bottom margin keeps
        // it; a window confined to the top header strip... still keeps it,
        // because the band's possible range starts at the top margin — the
        // genuine off-screen case is a window ENTIRELY below content_bottom.
        let (pw, ph, ml, mt, mr, mb) = (816.0, 1056.0, 72.0, 72.0, 72.0, 72.0);
        let band = (ml, mt, pw - mr, ph - mb); // (72,72,744,984)

        // Window over the page bottom (where footnotes paint) — KEEP.
        let bottom = Rect { x0: 72.0, y0: 900.0, x1: 744.0, y1: 984.0 };
        assert!(keep(Some(&bottom), band.0, band.1, band.2, band.3));

        // Window entirely BELOW the content bottom (in the bottom margin gutter
        // under the band) — DROP (the band can't extend past content_bottom).
        let below = Rect { x0: 72.0, y0: 1000.0, x1: 744.0, y1: 1050.0 };
        assert!(!keep(Some(&below), band.0, band.1, band.2, band.3));
    }
}
