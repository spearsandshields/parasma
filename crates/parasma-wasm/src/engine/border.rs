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
/// only PML currently populates it; WML/SML leave it `None`), and the line
/// style WML borders draw in (`stroke_styled` composes it).
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Edge {
    pub color: String,
    pub width_px: f64,
    pub dash: Option<Vec<f64>>,
    pub style: LineStyle,
}

/// A border's line style (§17.18.2 `ST_Border`, the line values; the art
/// borders are not drawn as art). Ordered as Word's conflict precedence lists
/// them, first highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LineStyle {
    #[default]
    Single,
    Thick,
    Double,
    Dotted,
    Dashed,
    DotDash,
    DotDotDash,
    Triple,
    ThinThickSmallGap,
    ThickThinSmallGap,
    ThinThickThinSmallGap,
    ThinThickMediumGap,
    ThickThinMediumGap,
    ThinThickThinMediumGap,
    ThinThickLargeGap,
    ThickThinLargeGap,
    ThinThickThinLargeGap,
    Wave,
    DoubleWave,
    DashSmallGap,
    DashDotStroked,
    ThreeDEmboss,
    ThreeDEngrave,
    Outset,
    Inset,
}

/// How light or dark a part of a 3-D border draws against the border colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Base,
    Light,
    Dark,
}

/// One stroke of a composed border line: its width, its offset across the
/// line from the line's centre (positive toward the outside of the box),
/// its dash pattern, tone, and whether it waves.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LinePart {
    pub offset: f64,
    pub width: f64,
    pub dash: Option<Vec<f64>>,
    pub tone: Tone,
    pub wave: bool,
}

impl LineStyle {
    /// Word's border number for the conflict weight ([MS-OI29500] §17.4.66).
    pub(crate) fn number(self) -> f64 {
        use LineStyle as S;
        match self {
            S::Single => 1.0,
            S::Thick => 2.0,
            S::Double => 3.0,
            S::Dotted | S::Dashed => 0.0,
            S::DotDash => 8.0,
            S::DotDotDash => 9.0,
            S::Triple => 10.0,
            S::ThinThickSmallGap => 11.0,
            S::ThickThinSmallGap => 12.0,
            S::ThinThickThinSmallGap => 13.0,
            S::ThinThickMediumGap => 14.0,
            S::ThickThinMediumGap => 15.0,
            S::ThinThickThinMediumGap => 16.0,
            S::ThinThickLargeGap => 17.0,
            S::ThickThinLargeGap => 18.0,
            S::ThinThickThinLargeGap => 19.0,
            S::Wave => 20.0,
            S::DoubleWave => 21.0,
            S::DashSmallGap => 22.0,
            S::DashDotStroked => 23.0,
            S::ThreeDEmboss => 24.0,
            S::ThreeDEngrave => 25.0,
            S::Outset => 26.0,
            S::Inset => 27.0,
        }
    }
}

impl Edge {
    /// The parallel strokes this edge draws on `side`, from its line style
    /// and its base width `w` (§17.18.2). A compound style's lines are `w`
    /// thick with `w` gaps (double, triple). DEVIATION, declared: Word's
    /// thin/thick proportions are not published; the thin line is a quarter
    /// of `w` (at least 1px) and the small, medium and large gaps are the thin
    /// line, half `w` and `w`; dash lengths are multiples of `w`.
    pub(crate) fn parts(&self, side: EdgeSide) -> Vec<LinePart> {
        use LineStyle as S;
        let w = self.width_px;
        let unit = w.max(1.0);
        let line = |offset: f64, width: f64| LinePart { offset, width, dash: None, tone: Tone::Base, wave: false };
        let dashed = |dash: Vec<f64>| vec![LinePart { dash: Some(dash), ..line(0.0, w) }];
        let thin = (w / 4.0).max(1.0);
        // Lines listed from the outside in, with the gaps between them, laid
        // out about the centre.
        let stack = |widths: &[f64], gap: f64| -> Vec<LinePart> {
            let total: f64 = widths.iter().sum::<f64>() + gap * (widths.len() - 1) as f64;
            let mut edge = total / 2.0;
            widths.iter().map(|&lw| {
                let part = line(edge - lw / 2.0, lw);
                edge -= lw + gap;
                part
            }).collect()
        };
        let toned = |outer: Tone, inner: Tone| vec![
            LinePart { tone: outer, ..line(w / 4.0, w / 2.0) },
            LinePart { tone: inner, ..line(-w / 4.0, w / 2.0) },
        ];
        // Outset lights the top and left and shades the bottom and right;
        // inset the reverse.
        let lit = matches!(side, EdgeSide::Top | EdgeSide::Left);
        match self.style {
            S::Single | S::Thick => vec![line(0.0, w)],
            S::Double => stack(&[w, w], w),
            S::Triple => stack(&[w, w, w], w),
            S::Dotted => dashed(vec![unit, unit]),
            S::Dashed => dashed(vec![unit * 4.0, unit * 2.0]),
            S::DashSmallGap => dashed(vec![unit * 4.0, unit]),
            S::DotDash | S::DashDotStroked => dashed(vec![unit * 4.0, unit, unit, unit]),
            S::DotDotDash => dashed(vec![unit * 4.0, unit, unit, unit, unit, unit]),
            S::ThinThickSmallGap => stack(&[thin, w], thin),
            S::ThickThinSmallGap => stack(&[w, thin], thin),
            S::ThinThickThinSmallGap => stack(&[thin, w, thin], thin),
            S::ThinThickMediumGap => stack(&[thin, w], w / 2.0),
            S::ThickThinMediumGap => stack(&[w, thin], w / 2.0),
            S::ThinThickThinMediumGap => stack(&[thin, w, thin], w / 2.0),
            S::ThinThickLargeGap => stack(&[thin, w], w),
            S::ThickThinLargeGap => stack(&[w, thin], w),
            S::ThinThickThinLargeGap => stack(&[thin, w, thin], w),
            S::Wave => vec![LinePart { wave: true, ..line(0.0, w) }],
            S::DoubleWave => stack(&[w, w], w * 2.0).into_iter().map(|p| LinePart { wave: true, ..p }).collect(),
            S::ThreeDEmboss => toned(Tone::Light, Tone::Dark),
            S::ThreeDEngrave => toned(Tone::Dark, Tone::Light),
            S::Outset => vec![LinePart { tone: if lit { Tone::Light } else { Tone::Dark }, ..line(0.0, w) }],
            S::Inset => vec![LinePart { tone: if lit { Tone::Dark } else { Tone::Light }, ..line(0.0, w) }],
        }
    }

    /// The width the composed line covers across itself.
    pub(crate) fn band_width(&self) -> f64 {
        self.parts(EdgeSide::Top).iter()
            .map(|p| (p.offset.abs() + p.width / 2.0) * 2.0)
            .fold(0.0, f64::max)
    }
}

/// Stroke `edge` along `(x0, y0)`-`(x1, y1)` as the `side` edge of a box, in
/// its line style: each part offset outward from the line, dashed, toned or
/// waved as its style composes it.
pub(crate) fn stroke_styled(
    p: &dyn crate::engine::paint::Painter,
    edge: &Edge,
    side: EdgeSide,
    (x0, y0): (f64, f64),
    (x1, y1): (f64, f64),
) {
    let (nx, ny) = match side {
        EdgeSide::Top => (0.0, -1.0),
        EdgeSide::Bottom => (0.0, 1.0),
        EdgeSide::Left => (-1.0, 0.0),
        EdgeSide::Right => (1.0, 0.0),
    };
    for part in edge.parts(side) {
        let color = match part.tone {
            Tone::Base => edge.color.clone(),
            Tone::Light => crate::engine::color::mix_hex(&edge.color, "#FFFFFF", 0.6),
            Tone::Dark => crate::engine::color::mix_hex(&edge.color, "#000000", 0.5),
        };
        let (ax, ay) = (x0 + nx * part.offset, y0 + ny * part.offset);
        let (bx, by) = (x1 + nx * part.offset, y1 + ny * part.offset);
        p.set_stroke_style_str(&color);
        p.set_line_width(part.width);
        if let Some(dash) = &part.dash {
            p.set_line_dash(dash);
        }
        p.begin_path();
        p.move_to(ax, ay);
        if part.wave {
            // Half waves of 3w, swinging w either side of the line.
            let len = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
            let half = (part.width * 3.0).max(3.0);
            let amp = part.width.max(1.0);
            let (dx, dy) = ((bx - ax) / len.max(1e-9), (by - ay) / len.max(1e-9));
            let mut t = 0.0;
            let mut up = true;
            while t < len {
                let next = (t + half).min(len);
                let mid = (t + next) / 2.0;
                let swing = if up { amp } else { -amp } * 2.0;
                p.quadratic_curve_to(
                    ax + dx * mid + nx * swing, ay + dy * mid + ny * swing,
                    ax + dx * next, ay + dy * next,
                );
                t = next;
                up = !up;
            }
        } else {
            p.line_to(bx, by);
        }
        p.stroke();
        if part.dash.is_some() {
            p.clear_line_dash();
        }
    }
}

/// Which side of the cell/box rect an [`Edge`] paints.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

#[cfg(test)]
mod style_tests {
    use super::*;

    fn edge(style: LineStyle, w: f64) -> Edge {
        Edge { color: "#000000".into(), width_px: w, dash: None, style }
    }

    #[test]
    fn compound_lines_stack_their_parts_about_the_centre() {
        // Double: two w lines with a w gap — 3w across, centres at ±w.
        let d = edge(LineStyle::Double, 2.0).parts(EdgeSide::Top);
        assert_eq!(d.iter().map(|p| (p.offset, p.width)).collect::<Vec<_>>(), [(2.0, 2.0), (-2.0, 2.0)]);
        assert_eq!(edge(LineStyle::Double, 2.0).band_width(), 6.0);
        assert_eq!(edge(LineStyle::Triple, 1.0).band_width(), 5.0);
        // Thin-thick: the thin line outside, the thick one inside.
        let tt = edge(LineStyle::ThinThickSmallGap, 4.0).parts(EdgeSide::Top);
        assert!(tt[0].offset > tt[1].offset && tt[0].width < tt[1].width);
    }

    #[test]
    fn dashes_waves_and_tones() {
        assert_eq!(edge(LineStyle::Dotted, 2.0).parts(EdgeSide::Top)[0].dash, Some(vec![2.0, 2.0]));
        assert!(edge(LineStyle::Wave, 1.0).parts(EdgeSide::Top)[0].wave);
        assert_eq!(edge(LineStyle::Inset, 2.0).parts(EdgeSide::Top)[0].tone, Tone::Dark);
        assert_eq!(edge(LineStyle::Inset, 2.0).parts(EdgeSide::Bottom)[0].tone, Tone::Light);
    }

    #[test]
    fn a_styled_stroke_draws_each_part() {
        use crate::engine::displaylist::{DrawCmd, RecordingPainter};
        let rec = RecordingPainter::new();
        stroke_styled(&rec, &edge(LineStyle::Double, 1.0), EdgeSide::Top, (0.0, 10.0), (50.0, 10.0));
        let moves: Vec<f64> = rec.take().iter().filter_map(|c| match c {
            DrawCmd::MoveTo { y, .. } => Some(*y),
            _ => None,
        }).collect();
        assert_eq!(moves, [9.0, 11.0], "the outer line above, the inner below");
    }
}
