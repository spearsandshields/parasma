//! Shared "coloured, width-ed box edge" abstraction.
//!
//! WML table cells (§17.4), SML cell borders (§18.8.4) and PML shape/table
//! outlines (§20.1.2.2.24) all describe the same thing: one drawable edge with a
//! colour, a pixel stroke width, and an optional dash pattern. Before this
//! module each format re-declared its own struct (`CellBorderEdge`, `BorderEdge`,
//! `ShapeOutline`) with drifting field names, and re-implemented the four-edge
//! cell-stroke loop. The [`Edge`] struct unifies the data; [`stroke_box_edges`]
//! unifies the geometry.
//!
//! CRITICAL — byte-identity is per-caller. The helper owns ONLY the coordinate
//! math; the backend calls (which Painter/`web_sys` context, dash on/off) stay in
//! a caller-supplied closure, and the stroke order + device-pixel snap are
//! caller-controlled. Each format therefore reproduces its exact pre-existing
//! pixel output:
//!   - WML: order [top, bottom, left, right], `snap = false`, Painter closure.
//!   - SML: order [left, right, top, bottom], `snap = true` (its `.round()`
//!     + `+0.5` odd-width device-pixel discipline), Painter closure.
//!   - PML: order [top, bottom, left, right], `snap = false`, raw `web_sys`
//!     context closure that additionally applies the `<a:ln>` line dash.

/// One drawable box edge: a colour + pixel stroke width, plus an optional dash
/// pattern (an on/off array already scaled in units the caller understands —
/// only PML currently populates it; WML/SML leave it `None`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Edge {
    pub color: String,
    pub width_px: f64,
    pub dash: Option<Vec<f64>>,
}

/// Which side of the cell/box rect an [`Edge`] paints.
#[derive(Clone, Copy)]
pub(crate) enum EdgeSide {
    Top,
    Bottom,
    Left,
    Right,
}

/// Stroke a box's edges, in the caller's `ordered` sequence, invoking `stroke`
/// (the caller's backend) with each present edge's endpoints.
///
/// The rect is `(x, y, w, h)`; endpoints are derived per [`EdgeSide`]. When
/// `snap` is false the endpoints are the raw corner-to-corner coordinates
/// (WML/PML). When `snap` is true the SML device-pixel discipline is reproduced
/// verbatim: every coordinate is `.round()`ed, and the fixed axis gets `+0.5`
/// when the stroke width is odd (so a 1px/3px rule lands on a device-pixel
/// centre; even widths already straddle the integer boundary). `stroke` receives
/// `(edge, x0, y0, x1, y1)` and performs the actual backend draw.
pub(crate) fn stroke_box_edges<F>(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    ordered: [(EdgeSide, Option<&Edge>); 4],
    snap: bool,
    mut stroke: F,
) where
    F: FnMut(&Edge, f64, f64, f64, f64),
{
    let (x0, y0, x1, y1) = (x, y, x + w, y + h);
    for (side, edge) in ordered {
        let Some(e) = edge else { continue };
        let vertical = matches!(side, EdgeSide::Left | EdgeSide::Right);
        // Raw corner-to-corner endpoints for this side.
        let (ax, ay, bx, by) = match side {
            EdgeSide::Top => (x0, y0, x1, y0),
            EdgeSide::Bottom => (x0, y1, x1, y1),
            EdgeSide::Left => (x0, y0, x0, y1),
            EdgeSide::Right => (x1, y0, x1, y1),
        };
        if snap {
            let off = if (e.width_px as i64) % 2 == 1 { 0.5 } else { 0.0 };
            if vertical {
                let xv = ax.round() + off; // ax == bx on a vertical edge
                stroke(e, xv, ay.round(), xv, by.round());
            } else {
                let yv = ay.round() + off; // ay == by on a horizontal edge
                stroke(e, ax.round(), yv, bx.round(), yv);
            }
        } else {
            stroke(e, ax, ay, bx, by);
        }
    }
}
