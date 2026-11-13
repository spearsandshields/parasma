//! Preset-geometry path builder (ECMA-376 §20.1.9.18 `a:prstGeom`).
//!
//! Turns a preset shape name (`ST_ShapeType`, §20.1.10.56) + its adjust values
//! (`avLst/gd`, §20.1.9.5) into a renderer-agnostic path IR ([`PathCmd`]) in
//! SHAPE-LOCAL px (origin = shape box top-left, extent = `w × h`). The PML
//! painter walks the commands into a canvas path and fills+strokes the SAME
//! path; the IR is web-sys-free so every preset is natively golden-testable.
//!
//! TIER-1 coverage (~69 presets — the Zipfian bulk of real decks, by a
//! census of preset usage): the rect family via one parameterized
//! corner routine, the ellipse/arc/pie/donut family, a regular-n-gon generator
//! + small constant polygons, the block-arrow set (adj-parameterized polygons),
//! arrow callouts, stars, frame/cube/can/teardrop/foldedCorner/wedge callouts,
//! and connector lines. We deliberately do NOT interpret the spec's 187-preset
//! guide-formula tables (`presetShapeDefinitions.xml`) — hand-coded top-N
//! covers the presets that occur in practice. Unknown preset → `None` (caller
//! falls back to the plain rectangle and may log the name under debug-probe).
//!
//! HOLES (donut/frame/noSmoking): the inner contour is emitted with REVERSED
//! winding so the default canvas `nonzero` fill rule leaves the hole empty —
//! fill-equivalent to `evenodd` for disjoint nested contours, without arming
//! the `CanvasWindingRule` web-sys feature (one more Cargo.toml dependency feature).
//!
//! ANGLES: OOXML and the y-down canvas agree — 0° = 3 o'clock, positive =
//! clockwise. Adjust ANGLES arrive in 60000ths of a degree (`ST_Angle`);
//! adjust FRACTIONS in 100000ths (the guide `val` convention). Curved-arrow
//! presets are best-effort quarter-ring bands (recognizable, not formula-exact).

#![cfg_attr(not(test), allow(dead_code))]

/// One path command in shape-local px. The painter maps these 1:1 onto canvas
/// `moveTo/lineTo/bezierCurveTo/quadraticCurveTo/ellipse/closePath`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PathCmd {
    MoveTo { x: f64, y: f64 },
    LineTo { x: f64, y: f64 },
    Cubic { x1: f64, y1: f64, x2: f64, y2: f64, x: f64, y: f64 },
    Quad { x1: f64, y1: f64, x: f64, y: f64 },
    /// Elliptical arc: centre + radii + start angle (deg, 0° = 3 o'clock,
    /// y-down) + sweep (deg, positive = clockwise). The canvas draws an
    /// implicit line from the current point to the arc start, so builders
    /// `MoveTo` exactly onto arc endpoints.
    Arc {
        cx: f64,
        cy: f64,
        rx: f64,
        ry: f64,
        start_deg: f64,
        sweep_deg: f64,
    },
    Close,
}

use PathCmd::*;

/// One shaded interior face of a 3-D preset (cube/can/bevel). `path` is a
/// closed contour in the SAME shape-local px space as [`preset_path`]; `shade`
/// is a lightness MULTIPLIER applied to the shape's solid base fill in HSL
/// space (`>1` lightens toward white, `<1` darkens toward black, `1.0` = base).
/// The painter fills the faces in list order (painter's algorithm) with the
/// shaded base colour, THEN strokes the silhouette outline from [`preset_path`].
/// Faces only render when the shape has a SOLID base fill (a gradient/none has
/// no single colour to shade — the painter then falls back to the flat
/// silhouette). See [`preset_faces`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ShadedFace {
    pub path: Vec<PathCmd>,
    pub shade: f64,
}

// ─── adjust-value helpers ───────────────────────────────────────────────────

/// Look up an adjust value by guide name, falling back through `alt` (single-
/// adj presets write `adj`, multi-adj write `adj1..adjN`; files vary) then the
/// preset's spec `default`.
fn adj(adj: &[(String, f64)], name: &str, alt: &str, default: f64) -> f64 {
    adj.iter()
        .find(|(n, _)| n == name)
        .or_else(|| adj.iter().find(|(n, _)| n == alt))
        .map(|(_, v)| *v)
        .unwrap_or(default)
}

/// A `val N` fraction (100000ths) → 0..=1-ish factor (not clamped — some
/// presets allow >100000, e.g. teardrop).
fn frac(v: f64) -> f64 {
    v / 100_000.0
}

/// A `val N` angle (60000ths of a degree) → degrees.
fn deg(v: f64) -> f64 {
    v / 60_000.0
}

/// Point on an axis-aligned ellipse at `deg` degrees (0° = 3 o'clock, y-down
/// clockwise) — where `Arc` starts/ends, for exact `MoveTo`/`LineTo` joins.
fn ell_pt(cx: f64, cy: f64, rx: f64, ry: f64, deg: f64) -> (f64, f64) {
    let r = deg.to_radians();
    (cx + rx * r.cos(), cy + ry * r.sin())
}

/// Close a clockwise polygon from a vertex list (dedupes consecutive equal
/// points so degenerate corners don't emit zero-length segments).
fn polygon(pts: &[(f64, f64)]) -> Vec<PathCmd> {
    let mut out = Vec::with_capacity(pts.len() + 1);
    let mut last: Option<(f64, f64)> = None;
    for &(x, y) in pts {
        if last == Some((x, y)) {
            continue;
        }
        if last.is_none() {
            out.push(MoveTo { x, y });
        } else {
            out.push(LineTo { x, y });
        }
        last = Some((x, y));
    }
    out.push(Close);
    out
}

// ─── geometric transforms (reuse one builder for mirrored/rotated kin) ──────

/// Mirror a path horizontally inside a `w`-wide box (`x → w - x`). Arc angles
/// reflect (`θ → 180° - θ`) and the sweep flips sign.
fn mirror_x(cmds: Vec<PathCmd>, w: f64) -> Vec<PathCmd> {
    cmds.into_iter()
        .map(|c| match c {
            MoveTo { x, y } => MoveTo { x: w - x, y },
            LineTo { x, y } => LineTo { x: w - x, y },
            Cubic { x1, y1, x2, y2, x, y } => Cubic { x1: w - x1, y1, x2: w - x2, y2, x: w - x, y },
            Quad { x1, y1, x, y } => Quad { x1: w - x1, y1, x: w - x, y },
            Arc { cx, cy, rx, ry, start_deg, sweep_deg } => Arc {
                cx: w - cx,
                cy,
                rx,
                ry,
                start_deg: 180.0 - start_deg,
                sweep_deg: -sweep_deg,
            },
            Close => Close,
        })
        .collect()
}

/// Mirror a path vertically inside an `h`-tall box (`y → h - y`). Arc angles
/// negate and the sweep flips sign.
fn mirror_y(cmds: Vec<PathCmd>, h: f64) -> Vec<PathCmd> {
    cmds.into_iter()
        .map(|c| match c {
            MoveTo { x, y } => MoveTo { x, y: h - y },
            LineTo { x, y } => LineTo { x, y: h - y },
            Cubic { x1, y1, x2, y2, x, y } => Cubic { x1, y1: h - y1, x2, y2: h - y2, x, y: h - y },
            Quad { x1, y1, x, y } => Quad { x1, y1: h - y1, x, y: h - y },
            Arc { cx, cy, rx, ry, start_deg, sweep_deg } => Arc {
                cx,
                cy: h - cy,
                rx,
                ry,
                start_deg: -start_deg,
                sweep_deg: -sweep_deg,
            },
            Close => Close,
        })
        .collect()
}

/// Swap x/y (build in a transposed `h × w` box, then transpose back into
/// `w × h`). Arc: centre/radii swap, `θ → 90° - θ`, sweep flips sign.
fn transpose(cmds: Vec<PathCmd>) -> Vec<PathCmd> {
    cmds.into_iter()
        .map(|c| match c {
            MoveTo { x, y } => MoveTo { x: y, y: x },
            LineTo { x, y } => LineTo { x: y, y: x },
            Cubic { x1, y1, x2, y2, x, y } => Cubic { x1: y1, y1: x1, x2: y2, y2: x2, x: y, y: x },
            Quad { x1, y1, x, y } => Quad { x1: y1, y1: x1, x: y, y: x },
            Arc { cx, cy, rx, ry, start_deg, sweep_deg } => Arc {
                cx: cy,
                cy: cx,
                rx: ry,
                ry: rx,
                start_deg: 90.0 - start_deg,
                sweep_deg: -sweep_deg,
            },
            Close => Close,
        })
        .collect()
}

// ─── the builder ────────────────────────────────────────────────────────────

/// Build the path for `preset` in a `w × h` px box. `adjs` is the shape's
/// parsed `avLst` as `(gd@name, value)` pairs (only `fmla="val N"` guides —
/// guide FORMULAS fall back to the preset's spec default; never an error).
/// Unknown preset → `None` (rect fallback at the call site).
pub(crate) fn preset_path(preset: &str, w: f64, h: f64, adjs: &[(String, f64)]) -> Option<Vec<PathCmd>> {
    let ss = w.min(h); // "shortest side", the spec's `ss` guide base
    let a = |name: &str, alt: &str, d: f64| adj(adjs, name, alt, d);

    Some(match preset {
        // ── rect family ×9 — one parameterized corner routine ──────────────
        "rect" | "flowChartProcess" | "actionButtonBlank" => corner_rect(w, h, [Corner::Square; 4]),
        "roundRect" | "flowChartAlternateProcess" => {
            let r = ss * frac(a("adj", "adj1", 16_667.0));
            corner_rect(w, h, [Corner::Round(r); 4])
        }
        "round1Rect" => {
            // top-right rounded only.
            let r = ss * frac(a("adj", "adj1", 16_667.0));
            corner_rect(w, h, [Corner::Square, Corner::Round(r), Corner::Square, Corner::Square])
        }
        "round2SameRect" => {
            // top two by adj1, bottom two by adj2 (default 0).
            let r1 = ss * frac(a("adj1", "adj", 16_667.0));
            let r2 = ss * frac(a("adj2", "", 0.0));
            corner_rect(w, h, [Corner::Round(r1), Corner::Round(r1), Corner::Round(r2), Corner::Round(r2)])
        }
        "round2DiagRect" => {
            // TL+BR by adj1, TR+BL by adj2 (default 0).
            let r1 = ss * frac(a("adj1", "adj", 16_667.0));
            let r2 = ss * frac(a("adj2", "", 0.0));
            corner_rect(w, h, [Corner::Round(r1), Corner::Round(r2), Corner::Round(r1), Corner::Round(r2)])
        }
        "snip1Rect" => {
            let s = ss * frac(a("adj", "adj1", 16_667.0));
            corner_rect(w, h, [Corner::Square, Corner::Snip(s), Corner::Square, Corner::Square])
        }
        "snip2SameRect" => {
            let s1 = ss * frac(a("adj1", "adj", 16_667.0));
            let s2 = ss * frac(a("adj2", "", 0.0));
            corner_rect(w, h, [Corner::Snip(s1), Corner::Snip(s1), Corner::Snip(s2), Corner::Snip(s2)])
        }
        "snip2DiagRect" => {
            // TL+BR by adj1 (default 0), TR+BL by adj2.
            let s1 = ss * frac(a("adj1", "adj", 0.0));
            let s2 = ss * frac(a("adj2", "", 16_667.0));
            corner_rect(w, h, [Corner::Snip(s1), Corner::Snip(s2), Corner::Snip(s1), Corner::Snip(s2)])
        }
        "snipRoundRect" => {
            // TL rounded by adj1, TR snipped by adj2.
            let r = ss * frac(a("adj1", "adj", 16_667.0));
            let s = ss * frac(a("adj2", "", 16_667.0));
            corner_rect(w, h, [Corner::Round(r), Corner::Snip(s), Corner::Square, Corner::Square])
        }

        // ── ellipse / arc / pie family ──────────────────────────────────────
        "ellipse" | "flowChartConnector" => ellipse_path(w, h),
        "pie" => {
            // §20.1.9.18 presetGeometry avLst: adj1 = start angle, adj2 = end angle (60000ths). Defaults 0°→270°.
            let st = deg(a("adj1", "adj", 0.0));
            let sweep = sweep_cw(st, deg(a("adj2", "", 16_200_000.0)));
            let (cx, cy, rx, ry) = (w / 2.0, h / 2.0, w / 2.0, h / 2.0);
            let (sx, sy) = ell_pt(cx, cy, rx, ry, st);
            vec![
                MoveTo { x: sx, y: sy },
                Arc { cx, cy, rx, ry, start_deg: st, sweep_deg: sweep },
                LineTo { x: cx, y: cy },
                Close,
            ]
        }
        "chord" => {
            let st = deg(a("adj1", "adj", 2_700_000.0));
            let sweep = sweep_cw(st, deg(a("adj2", "", 16_200_000.0)));
            let (cx, cy, rx, ry) = (w / 2.0, h / 2.0, w / 2.0, h / 2.0);
            let (sx, sy) = ell_pt(cx, cy, rx, ry, st);
            vec![
                MoveTo { x: sx, y: sy },
                Arc { cx, cy, rx, ry, start_deg: st, sweep_deg: sweep },
                Close,
            ]
        }
        "arc" => {
            // Open arc (stroke is the shape; the fill chord-closes — honest v1).
            let st = deg(a("adj1", "adj", 16_200_000.0));
            let sweep = sweep_cw(st, deg(a("adj2", "", 0.0)));
            let (cx, cy, rx, ry) = (w / 2.0, h / 2.0, w / 2.0, h / 2.0);
            let (sx, sy) = ell_pt(cx, cy, rx, ry, st);
            vec![
                MoveTo { x: sx, y: sy },
                Arc { cx, cy, rx, ry, start_deg: st, sweep_deg: sweep },
            ]
        }
        "donut" => ring(w, h, ss * frac(a("adj", "adj1", 25_000.0))),
        // noSmoking's diagonal bar is omitted (v1 — ring only, recognizable).
        "noSmoking" => ring(w, h, ss * frac(a("adj", "adj1", 18_750.0))),
        "blockArc" => {
            // adj1 = start angle (default 180°), adj2 = end angle (default 0° —
            // the upper half-ring), adj3 = thickness fraction.
            let st = deg(a("adj1", "adj", 10_800_000.0));
            let sweep = sweep_cw(st, deg(a("adj2", "", 0.0)));
            let t = ss * frac(a("adj3", "", 25_000.0));
            let (cx, cy, rx, ry) = (w / 2.0, h / 2.0, w / 2.0, h / 2.0);
            let (irx, iry) = ((rx - t).max(0.0), (ry - t).max(0.0));
            let (sx, sy) = ell_pt(cx, cy, rx, ry, st);
            let (iex, iey) = ell_pt(cx, cy, irx, iry, st + sweep);
            vec![
                MoveTo { x: sx, y: sy },
                Arc { cx, cy, rx, ry, start_deg: st, sweep_deg: sweep },
                LineTo { x: iex, y: iey },
                Arc { cx, cy, rx: irx, ry: iry, start_deg: st + sweep, sweep_deg: -sweep },
                Close,
            ]
        }
        "teardrop" => {
            // Circle with the top-right pulled to a point (adj=100000 → the
            // corner). Best-effort: 270° arc + two quads through the tip.
            let pull = frac(a("adj", "adj1", 100_000.0)).clamp(0.0, 2.0);
            let (cx, cy, rx, ry) = (w / 2.0, h / 2.0, w / 2.0, h / 2.0);
            let tip = (cx + rx * 0.5 * (1.0 + pull), cy - ry * 0.5 * (1.0 + pull));
            vec![
                MoveTo { x: w, y: cy }, // 0° point
                Arc { cx, cy, rx, ry, start_deg: 0.0, sweep_deg: 270.0 },
                // top point (270°) → tip → back to the 0° point.
                Quad { x1: (cx + tip.0) / 2.0, y1: tip.1, x: tip.0, y: tip.1 },
                Quad { x1: tip.0, y1: (cy + tip.1) / 2.0, x: w, y: cy },
                Close,
            ]
        }

        // ── constant / generated polygons ───────────────────────────────────
        "triangle" => {
            let ax = w * frac(a("adj", "adj1", 50_000.0));
            polygon(&[(ax, 0.0), (w, h), (0.0, h)])
        }
        "rtTriangle" => polygon(&[(0.0, 0.0), (w, h), (0.0, h)]),
        "diamond" | "flowChartDecision" => {
            polygon(&[(w / 2.0, 0.0), (w, h / 2.0), (w / 2.0, h), (0.0, h / 2.0)])
        }
        "parallelogram" | "flowChartInputOutput" => {
            let off = (ss * frac(a("adj", "adj1", 25_000.0))).min(w);
            polygon(&[(off, 0.0), (w, 0.0), (w - off, h), (0.0, h)])
        }
        "trapezoid" => {
            let off = (ss * frac(a("adj", "adj1", 25_000.0))).min(w / 2.0);
            polygon(&[(off, 0.0), (w - off, 0.0), (w, h), (0.0, h)])
        }
        "pentagon" => regular_polygon(w, h, 5),
        "hexagon" => {
            let off = (ss * frac(a("adj", "adj1", 25_000.0))).min(w / 2.0);
            polygon(&[
                (off, 0.0),
                (w - off, 0.0),
                (w, h / 2.0),
                (w - off, h),
                (off, h),
                (0.0, h / 2.0),
            ])
        }
        "heptagon" => regular_polygon(w, h, 7),
        "octagon" => {
            let off = (ss * frac(a("adj", "adj1", 29_289.0))).min(ss / 2.0);
            polygon(&[
                (off, 0.0),
                (w - off, 0.0),
                (w, off),
                (w, h - off),
                (w - off, h),
                (off, h),
                (0.0, h - off),
                (0.0, off),
            ])
        }
        "decagon" => regular_polygon(w, h, 10),
        "dodecagon" => regular_polygon(w, h, 12),
        "chevron" => {
            let off = (ss * frac(a("adj", "adj1", 50_000.0))).min(w);
            polygon(&[
                (0.0, 0.0),
                (w - off, 0.0),
                (w, h / 2.0),
                (w - off, h),
                (0.0, h),
                (off, h / 2.0),
            ])
        }
        "homePlate" => {
            let off = (ss * frac(a("adj", "adj1", 50_000.0))).min(w);
            polygon(&[(0.0, 0.0), (w - off, 0.0), (w, h / 2.0), (w - off, h), (0.0, h)])
        }
        "plus" => {
            let off = (ss * frac(a("adj", "adj1", 25_000.0))).min(ss / 2.0);
            let (x1, x2, y1, y2) = (off, w - off, off, h - off);
            polygon(&[
                (x1, 0.0),
                (x2, 0.0),
                (x2, y1),
                (w, y1),
                (w, y2),
                (x2, y2),
                (x2, h),
                (x1, h),
                (x1, y2),
                (0.0, y2),
                (0.0, y1),
                (x1, y1),
            ])
        }
        "star4" => star(w, h, 4, frac(a("adj", "adj1", 12_500.0)) * 2.0),
        "star5" => star(w, h, 5, frac(a("adj", "adj1", 19_098.0)) * 2.0),
        "star6" => star(w, h, 6, frac(a("adj", "adj1", 28_868.0)) * 2.0),

        // ── block arrows (adj-parameterized polygons + transforms) ─────────
        "rightArrow" => right_arrow(w, h, ss, a("adj1", "adj", 50_000.0), a("adj2", "", 50_000.0)),
        "leftArrow" => mirror_x(
            right_arrow(w, h, ss, a("adj1", "adj", 50_000.0), a("adj2", "", 50_000.0)),
            w,
        ),
        "downArrow" => transpose(right_arrow(h, w, ss, a("adj1", "adj", 50_000.0), a("adj2", "", 50_000.0))),
        "upArrow" => mirror_y(
            transpose(right_arrow(h, w, ss, a("adj1", "adj", 50_000.0), a("adj2", "", 50_000.0))),
            h,
        ),
        "leftRightArrow" => {
            left_right_arrow(w, h, ss, a("adj1", "adj", 50_000.0), a("adj2", "", 50_000.0))
        }
        "upDownArrow" => transpose(left_right_arrow(
            h,
            w,
            ss,
            a("adj1", "adj", 50_000.0),
            a("adj2", "", 50_000.0),
        )),
        "quadArrow" => quad_arrow(
            w,
            h,
            ss * frac(a("adj1", "adj", 22_500.0)),
            ss * frac(a("adj2", "", 22_500.0)),
            ss * frac(a("adj3", "", 22_500.0)),
        ),
        "leftRightUpArrow" => left_right_up_arrow(
            w,
            h,
            ss * frac(a("adj1", "adj", 25_000.0)),
            ss * frac(a("adj2", "", 25_000.0)),
            ss * frac(a("adj3", "", 25_000.0)),
        ),
        "bentArrow" => bent_arrow(
            w,
            h,
            ss * frac(a("adj1", "adj", 25_000.0)),
            ss * frac(a("adj2", "", 25_000.0)),
            ss * frac(a("adj3", "", 25_000.0)),
        ),
        "uturnArrow" => uturn_arrow(
            w,
            h,
            ss * frac(a("adj1", "adj", 25_000.0)),
            ss * frac(a("adj2", "", 25_000.0)),
            ss * frac(a("adj3", "", 25_000.0)),
        ),
        "leftUpArrow" => left_up_arrow(
            w,
            h,
            ss * frac(a("adj1", "adj", 25_000.0)),
            ss * frac(a("adj2", "", 25_000.0)),
            ss * frac(a("adj3", "", 25_000.0)),
        ),
        "bentUpArrow" => bent_up_arrow(
            w,
            h,
            ss * frac(a("adj1", "adj", 25_000.0)),
            ss * frac(a("adj2", "", 25_000.0)),
            ss * frac(a("adj3", "", 25_000.0)),
        ),
        // Curved arrows: best-effort quarter-ring band + head, one base build
        // (down) + transforms. Spec formulas are arc towers — out of tier 1.
        "curvedDownArrow" => curved_down_arrow(w, h, ss),
        "curvedUpArrow" => mirror_y(curved_down_arrow(w, h, ss), h),
        "curvedRightArrow" => transpose(curved_down_arrow(h, w, ss)),
        "curvedLeftArrow" => mirror_x(transpose(curved_down_arrow(h, w, ss)), w),
        "stripedRightArrow" => striped_right_arrow(
            w,
            h,
            ss,
            a("adj1", "adj", 50_000.0),
            a("adj2", "", 50_000.0),
        ),
        "notchedRightArrow" => notched_right_arrow(
            w,
            h,
            ss,
            a("adj1", "adj", 50_000.0),
            a("adj2", "", 50_000.0),
        ),

        // ── arrow callouts (best-effort: box + arrow as one polygon) ────────
        "rightArrowCallout" => right_arrow_callout(
            w,
            h,
            ss * frac(a("adj1", "adj", 25_000.0)),
            ss * frac(a("adj2", "", 25_000.0)),
            ss * frac(a("adj3", "", 25_000.0)),
            w * frac(a("adj4", "", 64_977.0)),
        ),
        "leftArrowCallout" => mirror_x(
            right_arrow_callout(
                w,
                h,
                ss * frac(a("adj1", "adj", 25_000.0)),
                ss * frac(a("adj2", "", 25_000.0)),
                ss * frac(a("adj3", "", 25_000.0)),
                w * frac(a("adj4", "", 64_977.0)),
            ),
            w,
        ),
        "downArrowCallout" => transpose(right_arrow_callout(
            h,
            w,
            ss * frac(a("adj1", "adj", 25_000.0)),
            ss * frac(a("adj2", "", 25_000.0)),
            ss * frac(a("adj3", "", 25_000.0)),
            h * frac(a("adj4", "", 64_977.0)),
        )),
        "upArrowCallout" => mirror_y(
            transpose(right_arrow_callout(
                h,
                w,
                ss * frac(a("adj1", "adj", 25_000.0)),
                ss * frac(a("adj2", "", 25_000.0)),
                ss * frac(a("adj3", "", 25_000.0)),
                h * frac(a("adj4", "", 64_977.0)),
            )),
            h,
        ),
        "leftRightArrowCallout" => left_right_arrow_callout(
            w,
            h,
            ss * frac(a("adj1", "adj", 25_000.0)),
            ss * frac(a("adj2", "", 25_000.0)),
            ss * frac(a("adj3", "", 25_000.0)),
            w * frac(a("adj4", "", 48_123.0)),
        ),
        "upDownArrowCallout" => transpose(left_right_arrow_callout(
            h,
            w,
            ss * frac(a("adj1", "adj", 25_000.0)),
            ss * frac(a("adj2", "", 25_000.0)),
            ss * frac(a("adj3", "", 25_000.0)),
            h * frac(a("adj4", "", 48_123.0)),
        )),
        "quadArrowCallout" => quad_arrow_callout(
            w,
            h,
            ss * frac(a("adj1", "adj", 18_515.0)),
            ss * frac(a("adj2", "", 18_515.0)),
            ss * frac(a("adj3", "", 18_515.0)),
            frac(a("adj4", "", 48_123.0)),
        ),

        // ── misc tier-1 constants ───────────────────────────────────────────
        "frame" => {
            // Rect with a rect hole — inner contour reversed (see module docs).
            let t = ss * frac(a("adj1", "adj", 12_500.0));
            let mut p = polygon(&[(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]);
            // counter-clockwise inner rect.
            p.extend(polygon(&[(t, t), (t, h - t), (w - t, h - t), (w - t, t)]));
            p
        }
        "corner" => {
            // L-bracket (frame-corner): a vertical bar down the left + a
            // horizontal bar across the bottom, meeting at the bottom-left.
            // adj1 = horizontal-bar thickness ÷ ss, adj2 = vertical-bar
            // thickness ÷ ss (§20.1.9.18 presetGeometry 'corner' avLst, defaults 50000).
            let dy1 = (ss * frac(a("adj1", "adj", 50_000.0))).min(h);
            let dx1 = (ss * frac(a("adj2", "", 50_000.0))).min(w);
            polygon(&[
                (0.0, 0.0),
                (dx1, 0.0),
                (dx1, h - dy1),
                (w, h - dy1),
                (w, h),
                (0.0, h),
            ])
        }
        "halfFrame" => {
            // Picture-frame corner: a full-width top bar + a full-height left
            // bar, the two inner edges mitred 45° at the shared corner.
            // adj1 = top-bar thickness ÷ ss, adj2 = left-bar thickness ÷ ss
            // (§20.1.9.18 presetGeometry 'halfFrame' avLst, defaults 33333).
            let dy1 = (ss * frac(a("adj1", "adj", 33_333.0))).min(h);
            let dx1 = (ss * frac(a("adj2", "", 33_333.0))).min(w);
            // Inner mitre offsets: the 45° cut where the bars meet.
            let dx2 = if h > 0.0 { (dy1 * w / h).min(w) } else { 0.0 };
            let dy2 = if w > 0.0 { (dx1 * h / w).min(h) } else { 0.0 };
            polygon(&[
                (0.0, 0.0),
                (w, 0.0),
                (w - dx2, dy1),
                (dx1, dy1),
                (dx1, h - dy2),
                (0.0, h),
            ])
        }
        "diagStripe" => {
            // Diagonal stripe: a parallelogram band whose two long edges run
            // parallel to the BL→TR diagonal, sweeping corner-to-corner across
            // the box. adj1 = stripe width as a fraction of the box (§
            // presetShapeDefinitions 'diagStripe', default 50000): the lower
            // edge passes through the bottom-left corner, the upper edge is
            // shifted toward the top-right by `f` along each axis.
            let f = frac(a("adj", "adj1", 50_000.0)).clamp(0.0, 1.0);
            let (xw, yh) = (f * w, f * h);
            polygon(&[
                (0.0, h),       // bottom-left corner (on the lower edge)
                (0.0, h - yh),  // up the left edge to the upper edge
                (w - xw, 0.0),  // across to the top edge
                (w, 0.0),       // top-right corner (on the upper edge)
                (w, yh),        // down the right edge to the lower edge
                (xw, h),        // back across the bottom edge
            ])
        }
        "cube" => {
            // Silhouette hexagon (interior edges deferred — strokes only).
            let off = (ss * frac(a("adj", "adj1", 25_000.0))).min(ss);
            polygon(&[
                (0.0, off),
                (off, 0.0),
                (w, 0.0),
                (w, h - off),
                (w - off, h),
                (0.0, h),
            ])
        }
        "can" | "flowChartMagneticDisk" => {
            let ey = (ss * frac(a("adj", "adj1", 25_000.0))).min(h / 2.0);
            let (cx, ry) = (w / 2.0, ey / 2.0);
            let mut p = vec![
                // body: left edge down, bottom half-ellipse, right edge up.
                MoveTo { x: 0.0, y: ry },
                LineTo { x: 0.0, y: h - ry },
                Arc { cx, cy: h - ry, rx: w / 2.0, ry, start_deg: 180.0, sweep_deg: -180.0 },
                LineTo { x: w, y: ry },
                Arc { cx, cy: ry, rx: w / 2.0, ry, start_deg: 0.0, sweep_deg: -180.0 },
                Close,
            ];
            // full top ellipse rim (the can lid, stroked + filled same colour).
            p.push(MoveTo { x: w, y: ry });
            p.push(Arc { cx, cy: ry, rx: w / 2.0, ry, start_deg: 0.0, sweep_deg: 360.0 });
            p.push(Close);
            p
        }
        // bevel paints its outer silhouette (the bevel facets are interior
        // detail — fill/outline read correctly without them).
        "bevel" => corner_rect(w, h, [Corner::Square; 4]),
        "foldedCorner" => {
            let f = (ss * frac(a("adj", "adj1", 16_667.0))).min(ss);
            let mut p = polygon(&[(0.0, 0.0), (w, 0.0), (w, h - f), (w - f, h), (0.0, h)]);
            // the fold flap (same winding — fills over the body, stroke shows
            // the fold line).
            p.extend(polygon(&[(w - f, h), (w - f, h - f), (w, h - f)]));
            p
        }
        "wedgeRectCallout" => wedge_rect_callout(
            w,
            h,
            w * frac(a("adj1", "adj", -20_833.0)),
            h * frac(a("adj2", "", 62_500.0)),
            [Corner::Square; 4],
        ),
        "wedgeRoundRectCallout" => {
            let r = ss * frac(a("adj3", "", 16_667.0));
            wedge_rect_callout(
                w,
                h,
                w * frac(a("adj1", "adj", -20_833.0)),
                h * frac(a("adj2", "", 62_500.0)),
                [Corner::Round(r); 4],
            )
        }
        "wedgeEllipseCallout" => {
            let mut p = ellipse_path(w, h);
            // tail triangle, same winding (union look; v1).
            let tx = w / 2.0 + w * frac(a("adj1", "adj", -20_833.0));
            let ty = h / 2.0 + h * frac(a("adj2", "", 62_500.0));
            let (cx, cy) = (w / 2.0, h / 2.0);
            let (b1, b2) = (
                ell_pt(cx, cy, w / 2.0, h / 2.0, 75.0),
                ell_pt(cx, cy, w / 2.0, h / 2.0, 115.0),
            );
            p.extend(polygon(&[b1, (tx, ty), b2]));
            p
        }

        // ── connectors / lines (stroke-only paths) ──────────────────────────
        "line" | "straightConnector1" | "bentConnector2" | "bentConnector3" | "curvedConnector3" => {
            vec![MoveTo { x: 0.0, y: 0.0 }, LineTo { x: w, y: h }]
        }

        _ => return None,
    })
}

// ─── 3-D per-face shading (the cube/can/bevel interior facets) ───────────────

/// The shaded interior faces for a 3-D preset (`cube`/`can`/`bevel`) in a
/// `w × h` px box, or `None` for any other preset (the painter then fills the
/// flat silhouette from [`preset_path`]). Each [`ShadedFace`] carries a
/// shade-multiplier the painter applies to the shape's SOLID base fill, in
/// painter's-algorithm order (later faces draw over earlier). The silhouette
/// outline from [`preset_path`] is still stroked on top — these faces only
/// replace the single flat geometry FILL.
///
/// Calibrated to PowerPoint's rendering (brown cube / green can / blue
/// bevel): the lit faces (cube top, can lid, bevel top+left) lighten ~1.25×,
/// the shaded faces (cube right, bevel bottom+right) darken ~0.75×.
pub(crate) fn preset_faces(preset: &str, w: f64, h: f64, adjs: &[(String, f64)]) -> Option<Vec<ShadedFace>> {
    let ss = w.min(h);
    let a = |name: &str, alt: &str, d: f64| adj(adjs, name, alt, d);

    Some(match preset {
        "cube" => {
            // adj = depth (default 25000): the offset that the top + right faces
            // are inset by. Three visible faces — front (base), top (lighter),
            // right (darker) — sharing the silhouette hexagon's vertices.
            let off = (ss * frac(a("adj", "adj1", 25_000.0))).min(ss);
            // Front face: the lower-left rectangle.
            let front = polygon(&[
                (0.0, off),
                (w - off, off),
                (w - off, h),
                (0.0, h),
            ]);
            // Top face: the parallelogram across the top (lighter — lit).
            let top = polygon(&[
                (0.0, off),
                (off, 0.0),
                (w, 0.0),
                (w - off, off),
            ]);
            // Right face: the parallelogram down the right (darker — shaded).
            let right = polygon(&[
                (w - off, off),
                (w, 0.0),
                (w, h - off),
                (w - off, h),
            ]);
            vec![
                ShadedFace { path: front, shade: 1.0 },
                ShadedFace { path: top, shade: 1.25 },
                ShadedFace { path: right, shade: 0.75 },
            ]
        }
        "can" | "flowChartMagneticDisk" => {
            // adj = lid ellipse height (default 25000, of ss). Body = the
            // cylinder silhouette (base); lid = the top ellipse (lighter — lit).
            let ey = (ss * frac(a("adj", "adj1", 25_000.0))).min(h / 2.0);
            let (cx, ry) = (w / 2.0, ey / 2.0);
            // Body: the silhouette (closed left-edge + bottom half-ellipse +
            // right-edge + top half rim) — the same outer contour preset_path
            // builds, filled at the base shade.
            let body = vec![
                MoveTo { x: 0.0, y: ry },
                LineTo { x: 0.0, y: h - ry },
                Arc { cx, cy: h - ry, rx: w / 2.0, ry, start_deg: 180.0, sweep_deg: -180.0 },
                LineTo { x: w, y: ry },
                Arc { cx, cy: ry, rx: w / 2.0, ry, start_deg: 0.0, sweep_deg: -180.0 },
                Close,
            ];
            // Lid: the full top ellipse, lighter.
            let lid = vec![
                MoveTo { x: w, y: ry },
                Arc { cx, cy: ry, rx: w / 2.0, ry, start_deg: 0.0, sweep_deg: 360.0 },
                Close,
            ];
            vec![
                ShadedFace { path: body, shade: 1.0 },
                ShadedFace { path: lid, shade: 1.2 },
            ]
        }
        "bevel" => {
            // adj = bevel width (default 12500, of ss): the chamfer inset. Center
            // = the inner rect (base); the 4 trapezoidal facets read TOP+LEFT
            // lighter (lit), BOTTOM+RIGHT darker (shaded) for a raised button.
            let b = (ss * frac(a("adj", "adj1", 12_500.0))).min(ss / 2.0);
            let center = polygon(&[(b, b), (w - b, b), (w - b, h - b), (b, h - b)]);
            let top = polygon(&[(0.0, 0.0), (w, 0.0), (w - b, b), (b, b)]);
            let left = polygon(&[(0.0, 0.0), (b, b), (b, h - b), (0.0, h)]);
            let bottom = polygon(&[(b, h - b), (w - b, h - b), (w, h), (0.0, h)]);
            let right = polygon(&[(w - b, b), (w, 0.0), (w, h), (w - b, h - b)]);
            // Paint base + lit facets + shaded facets; the center fills last so it
            // reads as the flat top of the button.
            vec![
                ShadedFace { path: top, shade: 1.2 },
                ShadedFace { path: left, shade: 1.2 },
                ShadedFace { path: bottom, shade: 0.8 },
                ShadedFace { path: right, shade: 0.8 },
                ShadedFace { path: center, shade: 1.0 },
            ]
        }
        _ => return None,
    })
}

// ─── custom geometry (§20.1.9.8 `a:custGeom`) ────────────────────────────────

/// Parse one shape's `<a:custGeom>` markup into the SAME [`PathCmd`] IR the
/// preset builder emits, scaled from each `<a:path w h>`'s own coordinate space
/// into the shape's `w_px × h_px` box (origin = box top-left — identical to
/// `preset_path` output, so the existing preset painter fills+strokes it unchanged).
///
/// The path-command grammar (§20.1.9): `<a:moveTo><a:pt x y/>` → [`MoveTo`];
/// `<a:lnTo><a:pt/>` → [`LineTo`]; `<a:cubicBezTo>` (3×`<a:pt/>`) → [`Cubic`];
/// `<a:quadBezTo>` (2×`<a:pt/>`) → [`Quad`]; `<a:arcTo wR hR stAng swAng/>` →
/// [`Arc`] (centre derived from the current point — see [`arc_to`]);
/// `<a:close/>` → [`Close`]. Multiple `<a:path>` elements are sub-paths and are
/// concatenated (a hole's reversed winding empties via the painter's nonzero
/// rule, fill-equivalent to evenodd for disjoint nested contours).
///
/// `xml` is the raw `<a:custGeom>…</a:custGeom>` slice (numeric coordinates, no
/// entity decoding needed). Returns `None` if no usable path command was parsed
/// (caller keeps the rect fallback + a debug-probe log). An UNKNOWN/garbled
/// command aborts the parse (→ `None`) rather than emitting a half path.
pub(crate) fn custom_path(xml: &str, w_px: f64, h_px: f64) -> Option<Vec<PathCmd>> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();

    // Per-`<a:path>` scale: path-local coords (w×h, EMU-ish but unitless here)
    // → shape box px. Updated at each `<a:path>` open; the pre-path default
    // (1:1) is replaced before any command emits.
    let mut sx = 1.0f64;
    let mut sy = 1.0f64;

    let mut out: Vec<PathCmd> = Vec::new();
    // The cursor for `arcTo` centre derivation (last emitted endpoint, in px).
    let mut cur = (0.0f64, 0.0f64);
    // Pending child `<a:pt>` coords for the multi-point commands.
    let mut pts: Vec<(f64, f64)> = Vec::new();
    // Which command we are currently inside (so a child `<a:pt>` knows its role).
    let mut cmd: Option<CustCmd> = None;
    // The `<a:arcTo>` attributes (it carries no child `<a:pt>`).
    let mut aborted = false;

    // Two exit conditions (reader Err and `Event::Eof`); a `while let` would only
    // express the first, so the explicit loop is kept.
    #[allow(clippy::while_let_loop)]
    loop {
        let ev = match reader.read_event_into(&mut buf) {
            Ok(e) => e,
            Err(_) => break,
        };
        match ev {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"path" => {
                        let (mut pw, mut ph) = (0.0, 0.0);
                        for a in e.attributes().flatten() {
                            match a.key.as_ref() {
                                b"w" => pw = attr_f64(&a),
                                b"h" => ph = attr_f64(&a),
                                _ => {}
                            }
                        }
                        sx = if pw > 0.0 { w_px / pw } else { w_px };
                        sy = if ph > 0.0 { h_px / ph } else { h_px };
                    }
                    b"moveTo" => {
                        cmd = Some(CustCmd::Move);
                        pts.clear();
                    }
                    b"lnTo" => {
                        cmd = Some(CustCmd::Line);
                        pts.clear();
                    }
                    b"cubicBezTo" => {
                        cmd = Some(CustCmd::Cubic);
                        pts.clear();
                    }
                    b"quadBezTo" => {
                        cmd = Some(CustCmd::Quad);
                        pts.clear();
                    }
                    b"close" => {
                        out.push(Close);
                    }
                    b"arcTo" => {
                        // arcTo carries its params as attributes (no child pt).
                        let (mut wr, mut hr, mut st, mut sw) = (0.0, 0.0, 0.0, 0.0);
                        for a in e.attributes().flatten() {
                            match a.key.as_ref() {
                                b"wR" => wr = attr_f64(&a),
                                b"hR" => hr = attr_f64(&a),
                                b"stAng" => st = attr_f64(&a),
                                b"swAng" => sw = attr_f64(&a),
                                _ => {}
                            }
                        }
                        if let Some(a) = arc_to(cur, wr * sx, hr * sy, st, sw) {
                            // Update the cursor to the arc end for any follow-on.
                            cur = arc_end(&a);
                            out.push(a);
                        }
                    }
                    b"pt" => {
                        let (mut x, mut y) = (0.0, 0.0);
                        for a in e.attributes().flatten() {
                            match a.key.as_ref() {
                                b"x" => x = attr_f64(&a),
                                b"y" => y = attr_f64(&a),
                                _ => {}
                            }
                        }
                        pts.push((x * sx, y * sy));
                    }
                    _ => {}
                }
            }
            Event::End(e) => {
                let local = e.local_name();
                match (local.as_ref(), cmd) {
                    (b"moveTo", Some(CustCmd::Move)) => {
                        if let Some(&(x, y)) = pts.first() {
                            out.push(MoveTo { x, y });
                            cur = (x, y);
                        }
                        cmd = None;
                    }
                    (b"lnTo", Some(CustCmd::Line)) => {
                        if let Some(&(x, y)) = pts.first() {
                            out.push(LineTo { x, y });
                            cur = (x, y);
                        }
                        cmd = None;
                    }
                    (b"cubicBezTo", Some(CustCmd::Cubic)) => {
                        if pts.len() >= 3 {
                            let (x1, y1) = pts[0];
                            let (x2, y2) = pts[1];
                            let (x, y) = pts[2];
                            out.push(Cubic { x1, y1, x2, y2, x, y });
                            cur = (x, y);
                        } else {
                            aborted = true;
                        }
                        cmd = None;
                    }
                    (b"quadBezTo", Some(CustCmd::Quad)) => {
                        if pts.len() >= 2 {
                            let (x1, y1) = pts[0];
                            let (x, y) = pts[1];
                            out.push(Quad { x1, y1, x, y });
                            cur = (x, y);
                        } else {
                            aborted = true;
                        }
                        cmd = None;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        buf.clear();
        if aborted {
            return None;
        }
    }

    // Need at least a MoveTo + one drawing command to be a real path.
    let drew = out
        .iter()
        .any(|c| matches!(c, LineTo { .. } | Cubic { .. } | Quad { .. } | Arc { .. }));
    if drew {
        Some(out)
    } else {
        None
    }
}

#[derive(Clone, Copy)]
enum CustCmd {
    Move,
    Line,
    Cubic,
    Quad,
}

/// Parse an attribute value as f64 (custGeom coords/radii/angles are integers
/// but we keep f64 throughout). Garbled → 0.0 (graceful, never an error).
fn attr_f64(a: &quick_xml::events::attributes::Attribute) -> f64 {
    std::str::from_utf8(&a.value)
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// `<a:arcTo>` (§20.1.9.4): a relative elliptical arc whose START is the path's
/// current point. `wr`/`hr` are the (already px-scaled) radii; `st_ang`/`sw_ang`
/// are in 60000ths of a degree. The spec derives the arc CENTRE from the start
/// point and start angle: `centre = start - (wr·cos st, hr·sin st)`. We emit an
/// [`Arc`] in the same degree convention the painter uses (0° = 3 o'clock,
/// positive = clockwise, y-down). Returns `None` for a degenerate (zero-radius)
/// arc.
fn arc_to(start: (f64, f64), wr: f64, hr: f64, st_ang: f64, sw_ang: f64) -> Option<PathCmd> {
    if wr <= 0.0 && hr <= 0.0 {
        return None;
    }
    let st = st_ang / 60_000.0;
    let sw = sw_ang / 60_000.0;
    let r = st.to_radians();
    let cx = start.0 - wr * r.cos();
    let cy = start.1 - hr * r.sin();
    Some(Arc { cx, cy, rx: wr, ry: hr, start_deg: st, sweep_deg: sw })
}

/// End point of an [`Arc`] (start_deg + sweep_deg on the ellipse) — the cursor
/// after an `arcTo`, so a following command joins correctly.
fn arc_end(a: &PathCmd) -> (f64, f64) {
    if let Arc { cx, cy, rx, ry, start_deg, sweep_deg } = *a {
        ell_pt(cx, cy, rx, ry, start_deg + sweep_deg)
    } else {
        (0.0, 0.0)
    }
}

/// Clockwise sweep (degrees, in `(0, 360]`) from `st` to `en` — OOXML pie/arc
/// angles always travel clockwise from start to end.
fn sweep_cw(st: f64, en: f64) -> f64 {
    let mut s = (en - st) % 360.0;
    if s <= 0.0 {
        s += 360.0;
    }
    s
}

/// Full-ellipse path (single 360° arc).
fn ellipse_path(w: f64, h: f64) -> Vec<PathCmd> {
    let (cx, cy, rx, ry) = (w / 2.0, h / 2.0, w / 2.0, h / 2.0);
    vec![
        MoveTo { x: w, y: cy },
        Arc { cx, cy, rx, ry, start_deg: 0.0, sweep_deg: 360.0 },
        Close,
    ]
}

/// Ellipse ring (donut/noSmoking): outer contour clockwise + inner contour
/// COUNTER-clockwise so the nonzero fill rule leaves the hole (module docs).
fn ring(w: f64, h: f64, t: f64) -> Vec<PathCmd> {
    let (cx, cy, rx, ry) = (w / 2.0, h / 2.0, w / 2.0, h / 2.0);
    let (irx, iry) = ((rx - t).max(0.0), (ry - t).max(0.0));
    vec![
        MoveTo { x: w, y: cy },
        Arc { cx, cy, rx, ry, start_deg: 0.0, sweep_deg: 360.0 },
        Close,
        MoveTo { x: cx + irx, y: cy },
        Arc { cx, cy, rx: irx, ry: iry, start_deg: 0.0, sweep_deg: -360.0 },
        Close,
    ]
}

/// Regular n-gon inscribed in the box, apex at 12 o'clock.
fn regular_polygon(w: f64, h: f64, n: usize) -> Vec<PathCmd> {
    let (cx, cy, rx, ry) = (w / 2.0, h / 2.0, w / 2.0, h / 2.0);
    let pts: Vec<(f64, f64)> = (0..n)
        .map(|k| ell_pt(cx, cy, rx, ry, -90.0 + k as f64 * 360.0 / n as f64))
        .collect();
    polygon(&pts)
}

/// n-point star, apex at 12 o'clock; `inner` = inner/outer radius ratio.
fn star(w: f64, h: f64, n: usize, inner: f64) -> Vec<PathCmd> {
    let (cx, cy, rx, ry) = (w / 2.0, h / 2.0, w / 2.0, h / 2.0);
    let inner = inner.clamp(0.0, 1.0);
    let step = 360.0 / n as f64;
    let mut pts = Vec::with_capacity(2 * n);
    for k in 0..n {
        let outer_deg = -90.0 + k as f64 * step;
        pts.push(ell_pt(cx, cy, rx, ry, outer_deg));
        pts.push(ell_pt(cx, cy, rx * inner, ry * inner, outer_deg + step / 2.0));
    }
    polygon(&pts)
}

/// Corner styles for the rect-family routine, in TL/TR/BR/BL order.
#[derive(Clone, Copy)]
enum Corner {
    Square,
    Round(f64),
    Snip(f64),
}

impl Corner {
    fn size(&self) -> f64 {
        match self {
            Corner::Square => 0.0,
            Corner::Round(s) | Corner::Snip(s) => s.max(0.0),
        }
    }
}

/// The parameterized rect: each corner square, rounded (quarter arc) or
/// snipped (45° cut). Clockwise from the top edge; corner size clamped to
/// half the shorter side so opposing corners never cross.
fn corner_rect(w: f64, h: f64, corners: [Corner; 4]) -> Vec<PathCmd> {
    let clamp = (w / 2.0).min(h / 2.0).max(0.0);
    let sz = |c: &Corner| c.size().min(clamp);
    let [tl, tr, br, bl] = &corners;
    let (stl, str_, sbr, sbl) = (sz(tl), sz(tr), sz(br), sz(bl));
    let mut p = vec![MoveTo { x: stl, y: 0.0 }];
    let line = |p: &mut Vec<PathCmd>, x: f64, y: f64| {
        // skip zero-length edges so degenerate corners stay clean.
        if !matches!(p.last(), Some(MoveTo { x: lx, y: ly } | LineTo { x: lx, y: ly }) if *lx == x && *ly == y)
        {
            p.push(LineTo { x, y });
        }
    };
    // top edge → TR corner.
    line(&mut p, w - str_, 0.0);
    match tr {
        Corner::Round(_) => p.push(Arc {
            cx: w - str_,
            cy: str_,
            rx: str_,
            ry: str_,
            start_deg: 270.0,
            sweep_deg: 90.0,
        }),
        Corner::Snip(_) => line(&mut p, w, str_),
        Corner::Square => {}
    }
    // right edge → BR corner.
    line(&mut p, w, h - sbr);
    match br {
        Corner::Round(_) => p.push(Arc {
            cx: w - sbr,
            cy: h - sbr,
            rx: sbr,
            ry: sbr,
            start_deg: 0.0,
            sweep_deg: 90.0,
        }),
        Corner::Snip(_) => line(&mut p, w - sbr, h),
        Corner::Square => {}
    }
    // bottom edge → BL corner.
    line(&mut p, sbl, h);
    match bl {
        Corner::Round(_) => p.push(Arc {
            cx: sbl,
            cy: h - sbl,
            rx: sbl,
            ry: sbl,
            start_deg: 90.0,
            sweep_deg: 90.0,
        }),
        Corner::Snip(_) => line(&mut p, 0.0, h - sbl),
        Corner::Square => {}
    }
    // left edge → TL corner.
    line(&mut p, 0.0, stl);
    match tl {
        Corner::Round(_) => p.push(Arc {
            cx: stl,
            cy: stl,
            rx: stl,
            ry: stl,
            start_deg: 180.0,
            sweep_deg: 90.0,
        }),
        // snip TL closes via the Close edge back to the MoveTo point.
        Corner::Snip(_) | Corner::Square => {}
    }
    // drop a redundant final edge back onto the MoveTo point (Close draws it).
    if matches!(
        (p.first(), p.last()),
        (Some(MoveTo { x: fx, y: fy }), Some(LineTo { x: lx, y: ly })) if fx == lx && fy == ly
    ) {
        p.pop();
    }
    p.push(Close);
    p
}

/// rightArrow: `adj1` = shaft thickness ÷ height (100000ths of FULL height,
/// spec halves it per side), `adj2` = head length ÷ ss. The base build the
/// left/up/down arrows transform from.
fn right_arrow(w: f64, h: f64, ss: f64, adj1: f64, adj2: f64) -> Vec<PathCmd> {
    let dy = (h * frac(adj1) / 2.0).min(h / 2.0);
    let dx = (ss * frac(adj2)).min(w);
    polygon(&[
        (0.0, h / 2.0 - dy),
        (w - dx, h / 2.0 - dy),
        (w - dx, 0.0),
        (w, h / 2.0),
        (w - dx, h),
        (w - dx, h / 2.0 + dy),
        (0.0, h / 2.0 + dy),
    ])
}

fn left_right_arrow(w: f64, h: f64, ss: f64, adj1: f64, adj2: f64) -> Vec<PathCmd> {
    let dy = (h * frac(adj1) / 2.0).min(h / 2.0);
    let dx = (ss * frac(adj2)).min(w / 2.0);
    polygon(&[
        (0.0, h / 2.0),
        (dx, 0.0),
        (dx, h / 2.0 - dy),
        (w - dx, h / 2.0 - dy),
        (w - dx, 0.0),
        (w, h / 2.0),
        (w - dx, h),
        (w - dx, h / 2.0 + dy),
        (dx, h / 2.0 + dy),
        (dx, h),
    ])
}

/// quadArrow: `a1` = shaft width, `a2` = head width, `a3` = head length (px).
fn quad_arrow(w: f64, h: f64, a1: f64, a2: f64, a3: f64) -> Vec<PathCmd> {
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (s, hw, hl) = (a1 / 2.0, a2 / 2.0, a3);
    polygon(&[
        (0.0, cy),
        (hl, cy - hw),
        (hl, cy - s),
        (cx - s, cy - s),
        (cx - s, hl),
        (cx - hw, hl),
        (cx, 0.0),
        (cx + hw, hl),
        (cx + s, hl),
        (cx + s, cy - s),
        (w - hl, cy - s),
        (w - hl, cy - hw),
        (w, cy),
        (w - hl, cy + hw),
        (w - hl, cy + s),
        (cx + s, cy + s),
        (cx + s, h - hl),
        (cx + hw, h - hl),
        (cx, h),
        (cx - hw, h - hl),
        (cx - s, h - hl),
        (cx - s, cy + s),
        (hl, cy + s),
        (hl, cy + hw),
    ])
}

/// leftRightUpArrow: quadArrow minus the bottom arm; horizontal shaft sits at
/// the bottom of the box.
fn left_right_up_arrow(w: f64, h: f64, a1: f64, a2: f64, a3: f64) -> Vec<PathCmd> {
    let cx = w / 2.0;
    let (s, hw, hl) = (a1 / 2.0, a2 / 2.0, a3);
    let yc = h - hw; // horizontal shaft centreline (head bottom = h).
    polygon(&[
        (0.0, yc),
        (hl, yc - hw),
        (hl, yc - s),
        (cx - s, yc - s),
        (cx - s, hl),
        (cx - hw, hl),
        (cx, 0.0),
        (cx + hw, hl),
        (cx + s, hl),
        (cx + s, yc - s),
        (w - hl, yc - s),
        (w - hl, yc - hw),
        (w, yc),
        (w - hl, yc + hw),
        (hl, yc + hw),
    ])
}

/// bentArrow (best-effort, square bend): vertical tail at the left rising into
/// a horizontal shaft with a right-pointing head.
fn bent_arrow(w: f64, h: f64, th: f64, hw: f64, hl: f64) -> Vec<PathCmd> {
    let yc = hw / 2.0; // horizontal arm centreline.
    polygon(&[
        (0.0, h),
        (0.0, (yc - th / 2.0).max(0.0)),
        (w - hl, (yc - th / 2.0).max(0.0)),
        (w - hl, yc - hw / 2.0),
        (w, yc),
        (w - hl, yc + hw / 2.0),
        (w - hl, yc + th / 2.0),
        (th, yc + th / 2.0),
        (th, h),
    ])
}

/// uturnArrow (best-effort, square bend): up the left shaft, across the top,
/// down the right shaft into a downward head.
fn uturn_arrow(w: f64, h: f64, th: f64, hw: f64, hl: f64) -> Vec<PathCmd> {
    let xc = w - th.max(hw) / 2.0 - (hw - th).max(0.0) / 2.0; // right shaft centre.
    let xr_out = (xc + th / 2.0).min(w);
    let xr_in = xc - th / 2.0;
    polygon(&[
        (0.0, h),
        (0.0, 0.0),
        (xr_out, 0.0),
        (xr_out, h - hl),
        ((xc + hw / 2.0).min(w), h - hl),
        (xc, h),
        (xc - hw / 2.0, h - hl),
        (xr_in, h - hl),
        (xr_in, th),
        (th, th),
        (th, h),
    ])
}

/// leftUpArrow: L-shaped shaft with heads pointing left (bottom arm) and up
/// (right arm).
fn left_up_arrow(w: f64, h: f64, th: f64, hw: f64, hl: f64) -> Vec<PathCmd> {
    let (s, hwh) = (th / 2.0, hw / 2.0);
    let xc = w - hwh; // vertical arm centre (head right edge = w).
    let yc = h - hwh; // horizontal arm centre (bottom edge = h).
    polygon(&[
        (0.0, yc),
        (hl, yc - hwh),
        (hl, yc - s),
        (xc - s, yc - s),
        (xc - s, hl),
        (xc - hwh, hl),
        (xc, 0.0),
        (xc + hwh, hl),
        (xc + s, hl),
        (xc + s, yc + s),
        (hl, yc + s),
        (hl, yc + hwh),
    ])
}

/// bentUpArrow: horizontal shaft along the bottom bending up at the right
/// into an upward head.
fn bent_up_arrow(w: f64, h: f64, th: f64, hw: f64, hl: f64) -> Vec<PathCmd> {
    let hwh = hw / 2.0;
    let xc = w - hwh;
    polygon(&[
        (0.0, h - th),
        (xc - th / 2.0, h - th),
        (xc - th / 2.0, hl),
        (xc - hwh, hl),
        (xc, 0.0),
        (xc + hwh, hl),
        ((xc + th / 2.0).min(w), hl),
        ((xc + th / 2.0).min(w), h),
        (0.0, h),
    ])
}

/// curvedDownArrow (best-effort): a quarter-ellipse band from the top-left
/// sweeping right, ending in a down-pointing head at the bottom-right. The
/// other three curved arrows are transforms of this build.
fn curved_down_arrow(w: f64, h: f64, ss: f64) -> Vec<PathCmd> {
    let t = (ss * 0.25).min(w / 2.0).min(h / 2.0); // band thickness
    let hl = (ss * 0.4).min(h / 2.0); // head length
    let ry = (h - hl).max(t);
    let cx = 0.0;
    let cy = ry;
    let head_w = t * 1.5;
    vec![
        MoveTo { x: 0.0, y: 0.0 },
        // outer quarter arc: top-left → right edge at the head base.
        Arc { cx, cy, rx: w, ry, start_deg: 270.0, sweep_deg: 90.0 },
        // head: right base is the arc end (w, ry).
        LineTo { x: w - head_w / 2.0, y: h },
        LineTo { x: w - head_w, y: ry },
        LineTo { x: w - t, y: ry },
        // inner quarter arc back: (w - t, ry) → (0, t).
        Arc { cx, cy, rx: (w - t).max(0.0), ry: (ry - t).max(0.0), start_deg: 0.0, sweep_deg: -90.0 },
        Close,
    ]
}

/// stripedRightArrow: the rightArrow body starting at x = 5·ss/32 plus the two
/// signature stripes at the left edge (same-winding subpaths).
fn striped_right_arrow(w: f64, h: f64, ss: f64, adj1: f64, adj2: f64) -> Vec<PathCmd> {
    let dy = (h * frac(adj1) / 2.0).min(h / 2.0);
    let dx = (ss * frac(adj2)).min(w);
    let x0 = ss * 5.0 / 32.0;
    let mut p = polygon(&[
        (x0, h / 2.0 - dy),
        (w - dx, h / 2.0 - dy),
        (w - dx, 0.0),
        (w, h / 2.0),
        (w - dx, h),
        (w - dx, h / 2.0 + dy),
        (x0, h / 2.0 + dy),
    ]);
    let (y1, y2) = (h / 2.0 - dy, h / 2.0 + dy);
    p.extend(polygon(&[(0.0, y1), (ss / 32.0, y1), (ss / 32.0, y2), (0.0, y2)]));
    p.extend(polygon(&[
        (ss / 16.0, y1),
        (ss / 8.0, y1),
        (ss / 8.0, y2),
        (ss / 16.0, y2),
    ]));
    p
}

/// notchedRightArrow: rightArrow with a triangular notch cut into the tail
/// (notch depth proportional to the head, §spec `x1 = dy2·dx2/hd2`).
fn notched_right_arrow(w: f64, h: f64, ss: f64, adj1: f64, adj2: f64) -> Vec<PathCmd> {
    let dy = (h * frac(adj1) / 2.0).min(h / 2.0);
    let dx = (ss * frac(adj2)).min(w);
    let notch = if h > 0.0 { dx * dy / (h / 2.0) } else { 0.0 };
    polygon(&[
        (0.0, h / 2.0 - dy),
        (w - dx, h / 2.0 - dy),
        (w - dx, 0.0),
        (w, h / 2.0),
        (w - dx, h),
        (w - dx, h / 2.0 + dy),
        (0.0, h / 2.0 + dy),
        (notch, h / 2.0),
    ])
}

/// rightArrowCallout: callout box (width `wb`) + right arrow (shaft `sh`,
/// head `hw × hl`) as one polygon. The other directional callouts transform it.
fn right_arrow_callout(w: f64, h: f64, sh: f64, hw: f64, hl: f64, wb: f64) -> Vec<PathCmd> {
    let yc = h / 2.0;
    let wb = wb.min(w - hl);
    polygon(&[
        (0.0, 0.0),
        (wb, 0.0),
        (wb, yc - sh / 2.0),
        (w - hl, yc - sh / 2.0),
        (w - hl, yc - hw / 2.0),
        (w, yc),
        (w - hl, yc + hw / 2.0),
        (w - hl, yc + sh / 2.0),
        (wb, yc + sh / 2.0),
        (wb, h),
        (0.0, h),
    ])
}

/// leftRightArrowCallout: centred box + arrows both sides.
fn left_right_arrow_callout(w: f64, h: f64, sh: f64, hw: f64, hl: f64, wbw: f64) -> Vec<PathCmd> {
    let yc = h / 2.0;
    let wbw = wbw.min(w - 2.0 * hl);
    let x0 = (w - wbw) / 2.0;
    let x1 = x0 + wbw;
    polygon(&[
        (x0, 0.0),
        (x1, 0.0),
        (x1, yc - sh / 2.0),
        (w - hl, yc - sh / 2.0),
        (w - hl, yc - hw / 2.0),
        (w, yc),
        (w - hl, yc + hw / 2.0),
        (w - hl, yc + sh / 2.0),
        (x1, yc + sh / 2.0),
        (x1, h),
        (x0, h),
        (x0, yc + sh / 2.0),
        (hl, yc + sh / 2.0),
        (hl, yc + hw / 2.0),
        (0.0, yc),
        (hl, yc - hw / 2.0),
        (hl, yc - sh / 2.0),
        (x0, yc - sh / 2.0),
    ])
}

/// quadArrowCallout: centred box (fraction `bf` of the box per axis) + four
/// arrows.
fn quad_arrow_callout(w: f64, h: f64, sh: f64, hw: f64, hl: f64, bf: f64) -> Vec<PathCmd> {
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (bw, bh) = (w * bf / 2.0, h * bf / 2.0);
    let (s, hwh) = (sh / 2.0, hw / 2.0);
    polygon(&[
        (cx - bw, cy - bh),
        (cx - s, cy - bh),
        (cx - s, hl),
        (cx - hwh, hl),
        (cx, 0.0),
        (cx + hwh, hl),
        (cx + s, hl),
        (cx + s, cy - bh),
        (cx + bw, cy - bh),
        (cx + bw, cy - s),
        (w - hl, cy - s),
        (w - hl, cy - hwh),
        (w, cy),
        (w - hl, cy + hwh),
        (w - hl, cy + s),
        (cx + bw, cy + s),
        (cx + bw, cy + bh),
        (cx + s, cy + bh),
        (cx + s, h - hl),
        (cx + hwh, h - hl),
        (cx, h),
        (cx - hwh, h - hl),
        (cx - s, h - hl),
        (cx - s, cy + bh),
        (cx - bw, cy + bh),
        (cx - bw, cy + s),
        (hl, cy + s),
        (hl, cy + hwh),
        (0.0, cy),
        (hl, cy - hwh),
        (hl, cy - s),
        (cx - bw, cy - s),
    ])
}

/// wedgeRectCallout / wedgeRoundRectCallout: the rect-family body with a
/// triangular tail toward `(w/2 + tail_dx, h/2 + tail_dy)`, inserted into the
/// bottom edge (best-effort — the spec picks the nearest edge; the default
/// tail points down-left, i.e. bottom).
fn wedge_rect_callout(w: f64, h: f64, tail_dx: f64, tail_dy: f64, corners: [Corner; 4]) -> Vec<PathCmd> {
    let tip = (w / 2.0 + tail_dx, h / 2.0 + tail_dy);
    let base_c = tip.0.clamp(w * 0.15, w * 0.85);
    let half = w / 12.0;
    let mut body = corner_rect(w, h, corners);
    // splice the tail into the bottom edge: find the bottom-edge LineTo (the
    // segment heading left along y == h) and insert base→tip→base before it
    // completes. Simpler v1: append the tail as a same-winding subpath (fill
    // unions; the stroke draws the tail outline over the body edge).
    body.extend(polygon(&[
        ((base_c + half).min(w), h),
        (tip.0, tip.1),
        ((base_c - half).max(0.0), h),
    ]));
    body
}

// ─── connectors (§19.3.1.19 `p:cxnSp`) ──────────────────────────────────────

/// A built connector: the stroked path (in SHAPE-LOCAL px, the painter
/// translates to the box like [`preset_path`]) plus the two endpoints and their
/// OUTWARD tangent directions (unit vectors pointing AWAY from the line, along
/// which an arrowhead points). `head` is the line START (`a:headEnd`), `tail`
/// the line END (`a:tailEnd`) — the spec's connection order, §20.1.8.30/.43.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConnectorGeom {
    pub path: Vec<PathCmd>,
    /// Line start point (where `headEnd` sits) + its outward unit direction.
    pub head: ((f64, f64), (f64, f64)),
    /// Line end point (where `tailEnd` sits) + its outward unit direction.
    pub tail: ((f64, f64), (f64, f64)),
}

/// Build a connector's geometry for `preset` in a `w × h` px box, honouring the
/// `a:xfrm` `flipH`/`flipV` (which decide the line's actual direction — a
/// horizontal `cy=0` connector is a degenerate box, so the flips are what make
/// head/tail meaningful). TIER: straight (`line`/`straightConnector1`) is the
/// box-diagonal segment; bent (`bentConnector2/3`) is an elbow polyline; curved
/// (`curvedConnector2/3`) is a best-effort single cubic. An unknown connector
/// preset falls back to the straight diagonal (connectors are line-like by
/// definition — never a rect). The outward directions point along the segment
/// AWAY from the shape so [`arrowhead_path`] orients correctly.
pub(crate) fn connector_path(
    preset: &str,
    w: f64,
    h: f64,
    flip_h: bool,
    flip_v: bool,
) -> ConnectorGeom {
    // Endpoints in the UN-flipped box: start top-left, end bottom-right. flipH
    // swaps the x ends, flipV the y ends — so the line still spans the box but
    // its start/end (hence head/tail) follow the transform (§20.1.7.6).
    let (x0, x1) = if flip_h { (w, 0.0) } else { (0.0, w) };
    let (y0, y1) = if flip_v { (h, 0.0) } else { (0.0, h) };
    let start = (x0, y0);
    let end = (x1, y1);

    let path: Vec<PathCmd> = match preset {
        // Elbow connectors: a single right-angle bend at the box mid (v1 — the
        // true 2/3-segment routing depends on the connected shapes' sides,
        // deferred). bentConnector2 = one bend, bentConnector3 = two (we draw
        // the dominant single mid-bend for both — recognizable).
        "bentConnector2" | "bentConnector3" | "bentConnector4" | "bentConnector5" => {
            let midx = (x0 + x1) / 2.0;
            vec![
                MoveTo { x: x0, y: y0 },
                LineTo { x: midx, y: y0 },
                LineTo { x: midx, y: y1 },
                LineTo { x: x1, y: y1 },
            ]
        }
        // Curved connectors: one cubic bowing through the box (best-effort —
        // the spec's multi-segment S-curve is deferred).
        "curvedConnector2" | "curvedConnector3" | "curvedConnector4" | "curvedConnector5" => {
            vec![
                MoveTo { x: x0, y: y0 },
                Cubic { x1, y1: y0, x2: x0, y2: y1, x: x1, y: y1 },
            ]
        }
        // line / straightConnector1 / anything else: the box-diagonal segment.
        _ => vec![MoveTo { x: x0, y: y0 }, LineTo { x: x1, y: y1 }],
    };

    // Outward directions point AWAY from the line at each end (the direction an
    // arrowhead at that tip aims): at the head, from the first-segment neighbour
    // back toward the start; at the tail, from the last-segment neighbour toward
    // the end.
    let head_dir = unit(segment_neighbor(&path, true), start);
    let tail_dir = unit(segment_neighbor(&path, false), end);
    ConnectorGeom {
        path,
        head: (start, head_dir),
        tail: (end, tail_dir),
    }
}

/// The point adjacent to an endpoint along the path: for the head, the SECOND
/// path point (so head→neighbor is the first segment); for the tail, the
/// SECOND-TO-LAST. Used to derive the endpoint tangent for arrowhead aiming.
fn segment_neighbor(path: &[PathCmd], head: bool) -> (f64, f64) {
    let pt = |c: &PathCmd| match *c {
        MoveTo { x, y } | LineTo { x, y } => Some((x, y)),
        Cubic { x, y, .. } => Some((x, y)),
        Quad { x, y, .. } => Some((x, y)),
        _ => None,
    };
    let pts: Vec<(f64, f64)> = path.iter().filter_map(pt).collect();
    if head {
        pts.get(1).copied().unwrap_or((0.0, 0.0))
    } else {
        pts.iter().rev().nth(1).copied().unwrap_or((0.0, 0.0))
    }
}

/// Unit vector from `a` to `b` (the outward arrow direction at `b`). Zero-length
/// → points right (a degenerate connector still draws a sane head).
fn unit(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-9 {
        (1.0, 0.0)
    } else {
        (dx / len, dy / len)
    }
}

/// A filled arrowhead at `tip`, pointing along the unit direction `dir`, sized
/// `len` (along the axis) × `w` (across). Tier-1 head TYPES (`triangle`/`arrow`/
/// `stealth`/`diamond`) all render as a filled triangle of this size — the
/// recognizable v1 (true stealth concavity / diamond rhombus / oval disc are
/// deferred); `oval` also approximates as a triangle here. The path is a closed
/// triangle in SHAPE-LOCAL px (same space as the connector path), so the painter
/// fills it with the line colour after stroking the line.
pub(crate) fn arrowhead_path(tip: (f64, f64), dir: (f64, f64), len: f64, w: f64) -> Vec<PathCmd> {
    // Base centre is `len` back along -dir from the tip; the two base corners
    // are ±(w/2) along the perpendicular.
    let (dx, dy) = dir;
    let (px, py) = (-dy, dx); // perpendicular (left normal)
    let base = (tip.0 - dx * len, tip.1 - dy * len);
    let half = w / 2.0;
    polygon(&[
        (tip.0, tip.1),
        (base.0 + px * half, base.1 + py * half),
        (base.0 - px * half, base.1 - py * half),
    ])
}

// ─── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod testutil {
    //! Native pixel-proxy: flatten a PathCmd list to polylines and answer
    //! point-in-fill queries with the NONZERO winding rule — the native twin
    //! of what the canvas painter does, so geometry tests need no browser.
    use super::PathCmd;

    /// Flatten to closed polylines (one per subpath). Arcs sample every 3°,
    /// curves at 24 segments.
    pub(crate) fn flatten(cmds: &[PathCmd]) -> Vec<Vec<(f64, f64)>> {
        let mut subs: Vec<Vec<(f64, f64)>> = Vec::new();
        let mut cur: Vec<(f64, f64)> = Vec::new();
        for c in cmds {
            match *c {
                PathCmd::MoveTo { x, y } => {
                    if cur.len() > 1 {
                        subs.push(std::mem::take(&mut cur));
                    } else {
                        cur.clear();
                    }
                    cur.push((x, y));
                }
                PathCmd::LineTo { x, y } => cur.push((x, y)),
                PathCmd::Cubic { x1, y1, x2, y2, x, y } => {
                    let (sx, sy) = *cur.last().unwrap_or(&(0.0, 0.0));
                    for i in 1..=24 {
                        let t = i as f64 / 24.0;
                        let mt = 1.0 - t;
                        cur.push((
                            mt * mt * mt * sx + 3.0 * mt * mt * t * x1 + 3.0 * mt * t * t * x2 + t * t * t * x,
                            mt * mt * mt * sy + 3.0 * mt * mt * t * y1 + 3.0 * mt * t * t * y2 + t * t * t * y,
                        ));
                    }
                }
                PathCmd::Quad { x1, y1, x, y } => {
                    let (sx, sy) = *cur.last().unwrap_or(&(0.0, 0.0));
                    for i in 1..=24 {
                        let t = i as f64 / 24.0;
                        let mt = 1.0 - t;
                        cur.push((
                            mt * mt * sx + 2.0 * mt * t * x1 + t * t * x,
                            mt * mt * sy + 2.0 * mt * t * y1 + t * t * y,
                        ));
                    }
                }
                PathCmd::Arc { cx, cy, rx, ry, start_deg, sweep_deg } => {
                    let steps = ((sweep_deg.abs() / 3.0).ceil() as usize).max(2);
                    for i in 0..=steps {
                        let d = start_deg + sweep_deg * i as f64 / steps as f64;
                        let r = d.to_radians();
                        cur.push((cx + rx * r.cos(), cy + ry * r.sin()));
                    }
                }
                PathCmd::Close => {
                    if cur.len() > 1 {
                        subs.push(std::mem::take(&mut cur));
                    } else {
                        cur.clear();
                    }
                }
            }
        }
        if cur.len() > 1 {
            subs.push(cur);
        }
        subs
    }

    /// Nonzero-winding point-in-fill over the flattened subpaths (canvas
    /// `fill()` semantics, including reversed-winding holes).
    pub(crate) fn point_in_path(cmds: &[PathCmd], px: f64, py: f64) -> bool {
        let mut winding = 0i32;
        for sub in flatten(cmds) {
            let n = sub.len();
            for i in 0..n {
                let (x1, y1) = sub[i];
                let (x2, y2) = sub[(i + 1) % n];
                if y1 <= py {
                    if y2 > py && cross(x1, y1, x2, y2, px, py) > 0.0 {
                        winding += 1;
                    }
                } else if y2 <= py && cross(x1, y1, x2, y2, px, py) < 0.0 {
                    winding -= 1;
                }
            }
        }
        winding != 0
    }

    fn cross(x1: f64, y1: f64, x2: f64, y2: f64, px: f64, py: f64) -> f64 {
        (x2 - x1) * (py - y1) - (px - x1) * (y2 - y1)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::point_in_path;
    use super::*;

    fn av(pairs: &[(&str, f64)]) -> Vec<(String, f64)> {
        pairs.iter().map(|(n, v)| (n.to_string(), *v)).collect()
    }

    #[test]
    fn rect_is_a_plain_closed_rectangle() {
        let p = preset_path("rect", 100.0, 50.0, &[]).unwrap();
        assert_eq!(
            p,
            vec![
                MoveTo { x: 0.0, y: 0.0 },
                LineTo { x: 100.0, y: 0.0 },
                LineTo { x: 100.0, y: 50.0 },
                LineTo { x: 0.0, y: 50.0 },
                Close,
            ]
        );
    }

    #[test]
    fn round_rect_has_four_quarter_arcs_with_spec_default_radius() {
        let p = preset_path("roundRect", 200.0, 100.0, &[]).unwrap();
        let arcs: Vec<_> = p
            .iter()
            .filter_map(|c| match c {
                Arc { rx, ry, sweep_deg, .. } => Some((*rx, *ry, *sweep_deg)),
                _ => None,
            })
            .collect();
        assert_eq!(arcs.len(), 4, "four rounded corners");
        let r = 100.0 * 16_667.0 / 100_000.0; // ss * adj/100000
        for (rx, ry, sweep) in arcs {
            assert!((rx - r).abs() < 1e-9 && (ry - r).abs() < 1e-9, "radius {rx} != {r}");
            assert_eq!(sweep, 90.0, "quarter arc");
        }
        // explicit adj halves the radius.
        let p2 = preset_path("roundRect", 200.0, 100.0, &av(&[("adj", 8_333.5)])).unwrap();
        let r2 = p2
            .iter()
            .find_map(|c| match c {
                Arc { rx, .. } => Some(*rx),
                _ => None,
            })
            .unwrap();
        assert!((r2 - r / 2.0).abs() < 1e-6);
    }

    #[test]
    fn snip_rect_cuts_the_top_right_corner() {
        let p = preset_path("snip1Rect", 100.0, 100.0, &[]).unwrap();
        // the 45° cut: corner pixel excluded, body included.
        assert!(!point_in_path(&p, 99.0, 1.0), "snipped corner is empty");
        assert!(point_in_path(&p, 1.0, 1.0), "TL corner kept");
        assert!(point_in_path(&p, 50.0, 50.0), "body filled");
    }

    #[test]
    fn ellipse_is_one_full_arc_and_misses_box_corners() {
        let p = preset_path("ellipse", 100.0, 50.0, &[]).unwrap();
        let arcs: Vec<_> = p.iter().filter(|c| matches!(c, Arc { .. })).collect();
        assert_eq!(arcs.len(), 1);
        assert!(matches!(arcs[0], Arc { sweep_deg, .. } if *sweep_deg == 360.0));
        for (x, y) in [(1.0, 1.0), (99.0, 1.0), (99.0, 49.0), (1.0, 49.0)] {
            assert!(!point_in_path(&p, x, y), "ellipse must not fill box corner ({x},{y})");
        }
        assert!(point_in_path(&p, 50.0, 25.0));
    }

    #[test]
    fn pie_default_wedge_misses_the_open_quadrant() {
        // defaults: 0° → 270° clockwise — the OPEN quarter is top-right.
        let p = preset_path("pie", 100.0, 100.0, &[]).unwrap();
        assert_eq!(p.first(), Some(&MoveTo { x: 100.0, y: 50.0 }), "arc starts at 0°");
        assert!(p.contains(&LineTo { x: 50.0, y: 50.0 }), "wedge closes via the centre");
        assert!(point_in_path(&p, 30.0, 70.0), "bottom-left quadrant covered");
        assert!(point_in_path(&p, 30.0, 30.0), "top-left quadrant covered");
        assert!(!point_in_path(&p, 70.0, 30.0), "top-right quadrant open");
        // explicit angles: 0..90° wedge covers ONLY bottom-right.
        let p2 = preset_path(
            "pie",
            100.0,
            100.0,
            &av(&[("adj1", 0.0), ("adj2", 5_400_000.0)]),
        )
        .unwrap();
        assert!(point_in_path(&p2, 70.0, 70.0));
        assert!(!point_in_path(&p2, 30.0, 30.0));
    }

    #[test]
    fn right_arrow_polygon_matches_spec_defaults_and_adj() {
        // w=200 h=100 ss=100: dy = 100*0.5/2 = 25, dx = 100*0.5 = 50.
        let p = preset_path("rightArrow", 200.0, 100.0, &[]).unwrap();
        assert_eq!(
            p,
            vec![
                MoveTo { x: 0.0, y: 25.0 },
                LineTo { x: 150.0, y: 25.0 },
                LineTo { x: 150.0, y: 0.0 },
                LineTo { x: 200.0, y: 50.0 },
                LineTo { x: 150.0, y: 100.0 },
                LineTo { x: 150.0, y: 75.0 },
                LineTo { x: 0.0, y: 75.0 },
                Close,
            ]
        );
        // adj1=100000 → full-height shaft (dy = h/2): tail corners hit the box.
        let p2 = preset_path("rightArrow", 200.0, 100.0, &av(&[("adj1", 100_000.0)])).unwrap();
        assert_eq!(p2[0], MoveTo { x: 0.0, y: 0.0 });
        // corner above the default shaft is empty; arrow tip area is filled.
        assert!(!point_in_path(&p, 10.0, 10.0));
        assert!(point_in_path(&p, 195.0, 50.0));
    }

    #[test]
    fn up_arrow_points_up_via_transforms() {
        let p = preset_path("upArrow", 100.0, 200.0, &[]).unwrap();
        // tip at top-centre, tail at the bottom.
        assert!(point_in_path(&p, 50.0, 5.0), "tip filled at top");
        assert!(point_in_path(&p, 50.0, 195.0), "shaft filled at bottom");
        assert!(!point_in_path(&p, 5.0, 195.0), "bottom-left outside the shaft");
    }

    #[test]
    fn donut_hole_is_empty_via_reversed_inner_winding() {
        let p = preset_path("donut", 100.0, 100.0, &[]).unwrap();
        // inner contour must wind opposite to the outer.
        let sweeps: Vec<f64> = p
            .iter()
            .filter_map(|c| match c {
                Arc { sweep_deg, .. } => Some(*sweep_deg),
                _ => None,
            })
            .collect();
        assert_eq!(sweeps, vec![360.0, -360.0]);
        assert!(!point_in_path(&p, 50.0, 50.0), "hole empty (nonzero rule)");
        assert!(point_in_path(&p, 50.0, 95.0), "ring filled");
        assert!(!point_in_path(&p, 2.0, 2.0), "outside the outer ellipse");
    }

    #[test]
    fn frame_inner_rect_is_a_hole() {
        let p = preset_path("frame", 100.0, 100.0, &[]).unwrap();
        assert!(!point_in_path(&p, 50.0, 50.0), "frame centre is a hole");
        assert!(point_in_path(&p, 5.0, 50.0), "frame border filled");
    }

    #[test]
    fn block_arc_upper_half_ring() {
        let p = preset_path("blockArc", 200.0, 100.0, &[]).unwrap();
        assert!(point_in_path(&p, 100.0, 5.0), "top of the ring filled");
        assert!(!point_in_path(&p, 100.0, 45.0), "ring interior open");
        assert!(!point_in_path(&p, 100.0, 75.0), "lower half open");
    }

    #[test]
    fn heptagon_has_seven_vertices() {
        let p = preset_path("heptagon", 100.0, 100.0, &[]).unwrap();
        let lines = p.iter().filter(|c| matches!(c, LineTo { .. })).count();
        assert_eq!(lines, 6, "MoveTo + 6 LineTo + Close = 7 vertices");
        assert!(point_in_path(&p, 50.0, 50.0));
    }

    #[test]
    fn star5_alternates_outer_and_inner_radii() {
        let p = preset_path("star5", 100.0, 100.0, &[]).unwrap();
        let verts = 1 + p.iter().filter(|c| matches!(c, LineTo { .. })).count();
        assert_eq!(verts, 10);
        assert!(point_in_path(&p, 50.0, 4.0), "top spike filled");
        assert!(!point_in_path(&p, 8.0, 8.0), "between spikes empty");
    }

    #[test]
    fn corner_is_an_l_bracket_not_a_rect() {
        // w=200 h=100 ss=100: dy1 = 50, dx1 = 50. L = left bar + bottom bar.
        let p = preset_path("corner", 200.0, 100.0, &[]).unwrap();
        // six vertices (MoveTo + 5 LineTo), a non-rect polygon.
        let verts = 1 + p.iter().filter(|c| matches!(c, LineTo { .. })).count();
        assert_eq!(verts, 6, "corner L has six vertices");
        assert!(p.iter().all(|c| !matches!(c, Arc { .. })), "no arcs");
        // The inner top-right quadrant (outside both bars) is empty…
        assert!(!point_in_path(&p, 150.0, 25.0), "inner notch empty");
        // …while the left bar and the bottom bar are filled.
        assert!(point_in_path(&p, 25.0, 25.0), "left bar filled");
        assert!(point_in_path(&p, 150.0, 90.0), "bottom bar filled");
    }

    #[test]
    fn half_frame_is_a_mitred_l_not_a_rect() {
        let p = preset_path("halfFrame", 120.0, 120.0, &[]).unwrap();
        let verts = 1 + p.iter().filter(|c| matches!(c, LineTo { .. })).count();
        assert_eq!(verts, 6, "halfFrame has six vertices (top bar + left bar)");
        assert!(p.iter().all(|c| !matches!(c, Arc { .. })), "no arcs");
        // top bar filled, left bar filled, inner bottom-right open.
        assert!(point_in_path(&p, 60.0, 5.0), "top bar filled");
        assert!(point_in_path(&p, 5.0, 60.0), "left bar filled");
        assert!(!point_in_path(&p, 100.0, 100.0), "inner corner empty");
    }

    #[test]
    fn diag_stripe_is_a_diagonal_band_not_a_rect() {
        let p = preset_path("diagStripe", 100.0, 100.0, &[]).unwrap();
        let verts = 1 + p.iter().filter(|c| matches!(c, LineTo { .. })).count();
        assert!(verts >= 4, "stripe parallelogram has ≥4 vertices");
        assert!(p.iter().all(|c| !matches!(c, Arc { .. })), "no arcs");
        // The band crosses the box on the BL→TR diagonal: the bottom-left
        // corner is on the band, the top-left and bottom-right corners are NOT.
        assert!(point_in_path(&p, 50.0, 50.0), "centre of the stripe filled");
        assert!(!point_in_path(&p, 2.0, 2.0), "top-left corner outside the stripe");
        assert!(!point_in_path(&p, 98.0, 98.0), "bottom-right corner outside the stripe");
    }

    #[test]
    fn unknown_preset_returns_none() {
        assert!(preset_path("mathPlus", 100.0, 100.0, &[]).is_none());
        assert!(preset_path("gear9", 100.0, 100.0, &[]).is_none());
    }

    // ── 3-D per-face shading (cube/can/bevel) ────────────────────────────────

    #[test]
    fn cube_has_three_faces_lit_top_shaded_right() {
        let faces = preset_faces("cube", 100.0, 100.0, &[]).expect("cube has faces");
        assert_eq!(faces.len(), 3, "front + top + right");
        // Order is front (base), top (lighter), right (darker).
        assert!((faces[0].shade - 1.0).abs() < 1e-9, "front = base");
        assert!(faces[1].shade > 1.0, "top lit (lighter)");
        assert!(faces[2].shade < 1.0, "right shaded (darker)");
        // The three faces tile the silhouette without leaving the front empty:
        // a point low-left is in the front face, the top strip is in the top
        // face, the right strip in the right face. off = ss*0.25 = 25.
        assert!(point_in_path(&faces[0].path, 10.0, 90.0), "front low-left");
        assert!(point_in_path(&faces[1].path, 50.0, 5.0), "top strip");
        assert!(point_in_path(&faces[2].path, 95.0, 50.0), "right strip");
        // The top-right cut corner of the silhouette is outside the front face.
        assert!(!point_in_path(&faces[0].path, 95.0, 5.0), "front excludes top-right");
    }

    #[test]
    fn can_has_body_plus_lighter_lid() {
        let faces = preset_faces("can", 100.0, 120.0, &[]).expect("can has faces");
        assert_eq!(faces.len(), 2, "body + lid");
        assert!((faces[0].shade - 1.0).abs() < 1e-9, "body = base");
        assert!(faces[1].shade > 1.0, "lid lit (lighter)");
        // ey = ss*0.25 = 25, ry = 12.5: the lid ellipse sits at the top, the body
        // fills the trunk below it.
        assert!(point_in_path(&faces[1].path, 50.0, 12.5), "lid centre");
        assert!(point_in_path(&faces[0].path, 50.0, 100.0), "body trunk");
    }

    #[test]
    fn bevel_has_center_plus_four_facets() {
        let faces = preset_faces("bevel", 100.0, 100.0, &[]).expect("bevel has faces");
        assert_eq!(faces.len(), 5, "4 facets + center");
        // Exactly one face is the un-shaded center (shade 1.0); two are lit
        // (>1, top+left), two shaded (<1, bottom+right).
        let center = faces.iter().filter(|f| (f.shade - 1.0).abs() < 1e-9).count();
        let lit = faces.iter().filter(|f| f.shade > 1.0).count();
        let shaded = faces.iter().filter(|f| f.shade < 1.0).count();
        assert_eq!((center, lit, shaded), (1, 2, 2));
        // b = ss*0.125 = 12.5: the inner rect (center) fills the middle.
        let c = faces.iter().find(|f| (f.shade - 1.0).abs() < 1e-9).unwrap();
        assert!(point_in_path(&c.path, 50.0, 50.0), "center is the inner rect");
        assert!(!point_in_path(&c.path, 2.0, 50.0), "center excludes left facet");
    }

    #[test]
    fn non_3d_preset_has_no_faces() {
        assert!(preset_faces("rect", 100.0, 100.0, &[]).is_none());
        assert!(preset_faces("plus", 100.0, 100.0, &[]).is_none());
        assert!(preset_faces("donut", 100.0, 100.0, &[]).is_none());
    }

    #[cfg(feature = "pml")]
    #[test]
    fn cube_faces_shade_to_distinct_colours_the_painter_path() {
        // The painter shades each face's path with the shape's solid base colour:
        // the three cube faces must resolve to THREE distinct hex strings (a flat
        // single-colour fill would collapse to one) — front, lighter, darker.
        let base = "#C0712E";
        let faces = preset_faces("cube", 100.0, 100.0, &[]).unwrap();
        let cols: Vec<String> = faces
            .iter()
            .map(|f| crate::engine::color::shade_hex(base, f.shade))
            .collect();
        assert_eq!(cols.len(), 3);
        assert!(cols[0] != cols[1] && cols[1] != cols[2] && cols[0] != cols[2], "{cols:?}");
    }

    #[test]
    fn chevron_and_homeplate_point_right() {
        for preset in ["chevron", "homePlate"] {
            let p = preset_path(preset, 200.0, 100.0, &[]).unwrap();
            assert!(point_in_path(&p, 195.0, 50.0), "{preset}: nose filled");
            assert!(!point_in_path(&p, 195.0, 5.0), "{preset}: top-right cut");
        }
        // chevron tail is notched, homePlate tail is flat.
        assert!(!point_in_path(
            &preset_path("chevron", 200.0, 100.0, &[]).unwrap(),
            5.0,
            50.0
        ));
        assert!(point_in_path(
            &preset_path("homePlate", 200.0, 100.0, &[]).unwrap(),
            5.0,
            50.0
        ));
    }

    #[test]
    fn quad_arrow_fills_four_tips_not_corners() {
        let p = preset_path("quadArrow", 100.0, 100.0, &[]).unwrap();
        for (x, y) in [(50.0, 2.0), (50.0, 98.0), (2.0, 50.0), (98.0, 50.0)] {
            assert!(point_in_path(&p, x, y), "tip ({x},{y}) filled");
        }
        for (x, y) in [(5.0, 5.0), (95.0, 5.0), (95.0, 95.0), (5.0, 95.0)] {
            assert!(!point_in_path(&p, x, y), "corner ({x},{y}) empty");
        }
    }

    #[test]
    fn curved_arrows_transform_consistently() {
        // head end filled, opposite corner of the band empty — for all four.
        let r = preset_path("curvedRightArrow", 100.0, 100.0, &[]).unwrap();
        assert!(point_in_path(&r, 97.0, 80.0), "curvedRight head at the right");
        let l = preset_path("curvedLeftArrow", 100.0, 100.0, &[]).unwrap();
        assert!(point_in_path(&l, 3.0, 80.0), "curvedLeft head at the left");
        let d = preset_path("curvedDownArrow", 100.0, 100.0, &[]).unwrap();
        assert!(point_in_path(&d, 80.0, 97.0), "curvedDown head at the bottom");
        let u = preset_path("curvedUpArrow", 100.0, 100.0, &[]).unwrap();
        assert!(point_in_path(&u, 80.0, 3.0), "curvedUp head at the top");
    }

    #[test]
    fn callout_box_plus_arrow() {
        let p = preset_path("rightArrowCallout", 200.0, 100.0, &[]).unwrap();
        assert!(point_in_path(&p, 50.0, 10.0), "callout box filled");
        assert!(point_in_path(&p, 197.0, 50.0), "arrow tip filled");
        assert!(!point_in_path(&p, 197.0, 5.0), "above the arrow empty");
    }

    #[test]
    fn line_connector_is_an_open_diagonal() {
        let p = preset_path("line", 80.0, 40.0, &[]).unwrap();
        assert_eq!(p, vec![MoveTo { x: 0.0, y: 0.0 }, LineTo { x: 80.0, y: 40.0 }]);
    }

    #[test]
    fn folded_corner_keeps_body_and_cuts_corner() {
        let p = preset_path("foldedCorner", 100.0, 100.0, &[]).unwrap();
        assert!(point_in_path(&p, 50.0, 50.0));
        assert!(!point_in_path(&p, 99.0, 99.0), "folded corner cut");
    }

    #[test]
    fn custom_path_parses_movelinecubicclose_in_order_and_scales() {
        // path-local 100×100 → shape box 200×100: sx=2, sy=1.
        let xml = r#"<a:custGeom><a:pathLst><a:path w="100" h="100">
            <a:moveTo><a:pt x="0" y="0"/></a:moveTo>
            <a:lnTo><a:pt x="100" y="0"/></a:lnTo>
            <a:cubicBezTo><a:pt x="100" y="50"/><a:pt x="50" y="100"/><a:pt x="0" y="100"/></a:cubicBezTo>
            <a:close/>
        </a:path></a:pathLst></a:custGeom>"#;
        let p = custom_path(xml, 200.0, 100.0).unwrap();
        assert_eq!(
            p,
            vec![
                MoveTo { x: 0.0, y: 0.0 },
                LineTo { x: 200.0, y: 0.0 },
                Cubic { x1: 200.0, y1: 50.0, x2: 100.0, y2: 100.0, x: 0.0, y: 100.0 },
                Close,
            ]
        );
    }

    #[test]
    fn custom_path_paints_non_rect_pixels() {
        // A right triangle (TL, TR, BL) in a 100×100 box: the bottom-RIGHT corner
        // is OUTSIDE the path (a rect fallback would fill it), the body inside.
        let xml = r#"<a:custGeom><a:pathLst><a:path w="100" h="100">
            <a:moveTo><a:pt x="0" y="0"/></a:moveTo>
            <a:lnTo><a:pt x="100" y="0"/></a:lnTo>
            <a:lnTo><a:pt x="0" y="100"/></a:lnTo>
            <a:close/>
        </a:path></a:pathLst></a:custGeom>"#;
        let p = custom_path(xml, 100.0, 100.0).unwrap();
        assert!(point_in_path(&p, 10.0, 10.0), "near TL filled");
        assert!(!point_in_path(&p, 95.0, 95.0), "bottom-right corner empty (not a rect)");
    }

    #[test]
    fn custom_path_quad_and_multiple_subpaths_concatenate() {
        let xml = r#"<a:custGeom><a:pathLst>
            <a:path w="100" h="100">
              <a:moveTo><a:pt x="0" y="0"/></a:moveTo>
              <a:quadBezTo><a:pt x="50" y="0"/><a:pt x="100" y="50"/></a:quadBezTo>
              <a:close/>
            </a:path>
            <a:path w="100" h="100">
              <a:moveTo><a:pt x="20" y="20"/></a:moveTo>
              <a:lnTo><a:pt x="80" y="80"/></a:lnTo>
            </a:path>
        </a:pathLst></a:custGeom>"#;
        let p = custom_path(xml, 100.0, 100.0).unwrap();
        // two MoveTo (one per sub-path), a Quad, a LineTo.
        assert_eq!(p.iter().filter(|c| matches!(c, MoveTo { .. })).count(), 2);
        assert!(p.contains(&Quad { x1: 50.0, y1: 0.0, x: 100.0, y: 50.0 }));
        assert!(p.contains(&LineTo { x: 80.0, y: 80.0 }));
    }

    #[test]
    fn custom_path_no_drawing_command_is_none() {
        // A lone moveTo (no line/curve/arc) is not a paintable path.
        let xml = r#"<a:custGeom><a:pathLst><a:path w="100" h="100">
            <a:moveTo><a:pt x="0" y="0"/></a:moveTo>
        </a:path></a:pathLst></a:custGeom>"#;
        assert!(custom_path(xml, 100.0, 100.0).is_none());
        // garbage / no pathLst → None too.
        assert!(custom_path("<a:custGeom/>", 100.0, 100.0).is_none());
    }

    #[test]
    fn adj_lookup_prefers_exact_name_then_alt_then_default() {
        let pairs = av(&[("adj", 1.0), ("adj1", 2.0)]);
        assert_eq!(adj(&pairs, "adj1", "adj", 9.0), 2.0);
        assert_eq!(adj(&pairs, "adj2", "adj", 9.0), 1.0);
        assert_eq!(adj(&pairs, "adj2", "adj3", 9.0), 9.0);
    }

    // ── connectors ─────────────────────────────────────────────────────────

    #[test]
    fn straight_connector_spans_box_and_aims_outward() {
        // No flip: head at the top-left (0,0), tail at the bottom-right (w,h).
        let g = connector_path("straightConnector1", 100.0, 0.0, false, false);
        assert_eq!(
            g.path,
            vec![MoveTo { x: 0.0, y: 0.0 }, LineTo { x: 100.0, y: 0.0 }]
        );
        assert_eq!(g.head.0, (0.0, 0.0));
        assert_eq!(g.tail.0, (100.0, 0.0));
        // Outward unit dirs: head points left (-1,0), tail points right (+1,0).
        assert!((g.head.1 .0 - -1.0).abs() < 1e-9 && g.head.1 .1.abs() < 1e-9);
        assert!((g.tail.1 .0 - 1.0).abs() < 1e-9 && g.tail.1 .1.abs() < 1e-9);
    }

    #[test]
    fn flip_h_swaps_connector_endpoints() {
        let g = connector_path("line", 100.0, 0.0, true, false);
        assert_eq!(g.head.0, (100.0, 0.0), "flipH puts the START at the right");
        assert_eq!(g.tail.0, (0.0, 0.0));
    }

    #[test]
    fn bent_connector_has_a_midpoint_elbow() {
        let g = connector_path("bentConnector3", 100.0, 60.0, false, false);
        // Four points: start, two elbow points at the mid-x, end.
        assert_eq!(g.path.len(), 4);
        assert!(g.path.contains(&LineTo { x: 50.0, y: 0.0 }));
        assert!(g.path.contains(&LineTo { x: 50.0, y: 60.0 }));
    }

    #[test]
    fn arrowhead_is_a_triangle_at_the_tip_pointing_along_dir() {
        // Tip at (100,0) pointing right; len 10, width 8.
        let head = arrowhead_path((100.0, 0.0), (1.0, 0.0), 10.0, 8.0);
        // The tip is in the path; the base sits 10px back at x=90, ±4 in y.
        assert!(head.contains(&MoveTo { x: 100.0, y: 0.0 }));
        // The triangle fills just behind the tip but not far ahead of it.
        assert!(point_in_path(&head, 95.0, 0.0), "filled just behind the tip");
        assert!(!point_in_path(&head, 100.0, 6.0), "nothing across the tip apex");
    }
}
