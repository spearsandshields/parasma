//! EMF coordinate transforms — MS-EMF §2.2.x world/map-mode geometry folded into
//! ONE composed 2×3 affine that is applied in Rust per primitive point, before
//! it reaches the `Painter`. Keeping the math here (rather than on the canvas via
//! `set_transform`) preserves the page's outer transform and keeps line-width /
//! font scaling explicit.
//!
//! Per-point pipeline (column-vector convention, GDI World→Page→Device order):
//!   `p_page = M_place · M_map · M_world · p_logical`
//! i.e. the world transform maps logical→page FIRST, the map mode maps
//! page→device SECOND, and the placement matrix maps device→page-px LAST.

use super::records::{RectL, XForm};

/// A 2×3 affine in column-vector form:
///   `x' = a·x + c·y + e`
///   `y' = b·x + d·y + f`
/// (matches `CanvasRenderingContext2d.setTransform(a,b,c,d,e,f)`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Mat {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Mat {
    pub(crate) const IDENTITY: Mat = Mat { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    pub(crate) fn scale_translate(sx: f64, sy: f64, e: f64, f: f64) -> Mat {
        Mat { a: sx, b: 0.0, c: 0.0, d: sy, e, f }
    }

    /// `self ∘ other` — the result applies `other` FIRST, then `self`.
    pub(crate) fn compose(&self, o: &Mat) -> Mat {
        Mat {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            e: self.a * o.e + self.c * o.f + self.e,
            f: self.b * o.e + self.d * o.f + self.f,
        }
    }

    /// Transform a point (translation applied).
    pub(crate) fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (self.a * x + self.c * y + self.e, self.b * x + self.d * y + self.f)
    }

    /// Magnitude of the x-basis vector — the per-unit horizontal scale used to
    /// scale text intercharacter `dx` advances.
    pub(crate) fn x_scale(&self) -> f64 {
        self.a.hypot(self.b)
    }

    /// `sqrt(|det|)` — the uniform-scale approximation used for pen width.
    pub(crate) fn uniform_scale(&self) -> f64 {
        (self.a * self.d - self.b * self.c).abs().sqrt()
    }
}

/// A GDI `XForm` (row-vector `eM11,eM12,eM21,eM22,eDx,eDy`) as our column `Mat`.
/// Row-vector `p·M` with `x' = x·eM11 + y·eM21 + eDx` ⇒ column `a=eM11, c=eM21,
/// e=eDx` (and the `M12/M22/Dy` row for `y'`).
pub(crate) fn xform_to_mat(x: &XForm) -> Mat {
    Mat {
        a: x.m11 as f64,
        b: x.m12 as f64,
        c: x.m21 as f64,
        d: x.m22 as f64,
        e: x.dx as f64,
        f: x.dy as f64,
    }
}

/// EMF map modes (MS-EMF §2.1.21 `MapMode`).
pub(crate) const MM_TEXT: u32 = 1;
pub(crate) const MM_LOMETRIC: u32 = 2;
pub(crate) const MM_HIMETRIC: u32 = 3;
pub(crate) const MM_LOENGLISH: u32 = 4;
pub(crate) const MM_HIENGLISH: u32 = 5;
pub(crate) const MM_TWIPS: u32 = 6;
pub(crate) const MM_ISOTROPIC: u32 = 7;
pub(crate) const MM_ANISOTROPIC: u32 = 8;

/// The map-mode window/viewport state that builds `M_map`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MapState {
    pub mode: u32,
    pub win_org: (i32, i32),
    pub win_ext: (i32, i32),
    pub vp_org: (i32, i32),
    pub vp_ext: (i32, i32),
    /// From the header — device px / millimetres, for the metric modes.
    pub szl_device: (i32, i32),
    pub szl_mm: (i32, i32),
}

impl Default for MapState {
    fn default() -> Self {
        MapState {
            mode: MM_TEXT,
            win_org: (0, 0),
            win_ext: (1, 1),
            vp_org: (0, 0),
            vp_ext: (1, 1),
            szl_device: (0, 0),
            szl_mm: (0, 0),
        }
    }
}

impl MapState {
    /// Build `M_map` (logical → EMF device units) for the current mode.
    pub(crate) fn matrix(&self) -> Mat {
        match self.mode {
            MM_ANISOTROPIC | MM_ISOTROPIC => {
                let wx = self.win_ext.0 as f64;
                let wy = self.win_ext.1 as f64;
                let mut sx = if wx != 0.0 { self.vp_ext.0 as f64 / wx } else { 1.0 };
                let mut sy = if wy != 0.0 { self.vp_ext.1 as f64 / wy } else { 1.0 };
                if self.mode == MM_ISOTROPIC {
                    // Equal magnitude on both axes; keep the signs.
                    let m = sx.abs().min(sy.abs());
                    sx = m.copysign(sx);
                    sy = m.copysign(sy);
                }
                let e = self.vp_org.0 as f64 - self.win_org.0 as f64 * sx;
                let f = self.vp_org.1 as f64 - self.win_org.1 as f64 * sy;
                Mat::scale_translate(sx, sy, e, f)
            }
            MM_LOMETRIC | MM_HIMETRIC | MM_LOENGLISH | MM_HIENGLISH | MM_TWIPS => {
                // Physical modes: 1 logical unit = a fixed physical length, and y
                // increases UPWARD (so sy is negated). px-per-mm comes from the
                // header szlDevice/szlMillimeters (fallback to 96dpi ≈ 3.7795
                // px/mm if the header omitted them).
                let px_per_mm_x = if self.szl_mm.0 != 0 {
                    self.szl_device.0 as f64 / self.szl_mm.0 as f64
                } else {
                    96.0 / 25.4
                };
                let px_per_mm_y = if self.szl_mm.1 != 0 {
                    self.szl_device.1 as f64 / self.szl_mm.1 as f64
                } else {
                    96.0 / 25.4
                };
                // mm per logical unit for each mode.
                let mm_per_unit = match self.mode {
                    MM_LOMETRIC => 0.1,          // 0.1 mm
                    MM_HIMETRIC => 0.01,         // 0.01 mm
                    MM_LOENGLISH => 0.254,       // 0.01 in
                    MM_HIENGLISH => 0.0254,      // 0.001 in
                    MM_TWIPS => 25.4 / 1440.0,   // 1/1440 in
                    _ => unreachable!(),
                };
                let sx = px_per_mm_x * mm_per_unit;
                let sy = -(px_per_mm_y * mm_per_unit); // y-up
                Mat::scale_translate(sx, sy, 0.0, 0.0)
            }
            // MM_TEXT (and any unknown mode) — 1 logical unit = 1 device px,
            // y-down, origin top-left.
            _ => Mat::IDENTITY,
        }
    }
}

/// Build `M_place` (EMF device units → page px) mapping the header `rclBounds`
/// into the inline destination rect `(dx,dy,dw,dh)`. Guards a degenerate bounds
/// rect (zero/negative extent) — falls back to a 1:1 device→dest mapping rather
/// than dividing by zero.
pub(crate) fn placement(bounds: &RectL, dx: f64, dy: f64, dw: f64, dh: f64) -> Mat {
    let bw = (bounds.right - bounds.left) as f64;
    let bh = (bounds.bottom - bounds.top) as f64;
    let sx = if bw.abs() > f64::EPSILON { dw / bw } else { 1.0 };
    let sy = if bh.abs() > f64::EPSILON { dh / bh } else { 1.0 };
    let e = dx - bounds.left as f64 * sx;
    let f = dy - bounds.top as f64 * sy;
    Mat::scale_translate(sx, sy, e, f)
}
