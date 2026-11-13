//! Line-dash and pattern-fill style helpers (`<a:ln>` dash + `pattFill`), PML-only.
//!
//! Two pure, natively-testable pieces feed the PML painter:
//!
//! * [`preset_dash`] / [`cust_dash`] — `<a:ln>`'s dash style
//!   (§20.1.10.48 `ST_PresetLineDashVal` / §20.1.8.21 `custDash`) → a canonical
//!   on/off array in MULTIPLES OF THE LINE WIDTH (the OOXML convention: a dash
//!   segment's length is a multiple of the pen width). The painter scales each
//!   entry by the stroke width before `setLineDash`.
//! * [`PatternTile`] + [`pattern_tile`] — a `<a:pattFill prst="…">` preset
//!   (§20.1.10.50 `ST_PresetPatternVal`) → a tiny tile spec (size + the
//!   foreground strokes/dots to stamp over a background-filled tile). The
//!   painter rasterises one offscreen tile per `(prst, fg, bg)` and
//!   `createPattern`s it. Unknown presets return `None` → the caller keeps the
//!   default 50/50 fg/bg blend (logged under debug-probe).

#![cfg_attr(not(test), allow(dead_code))]

/// Canonical on/off dash array for an `<a:prstDash val="…">` value, in
/// MULTIPLES of the line width (§20.1.10.48). `solid` → `None` (no dash).
/// Unknown / unmapped values → `None` (solid stroke — honest fallback).
///
/// The arrays follow PowerPoint's rendering: `dot` = 1-on/3-off, `dash` =
/// 4-on/3-off, `lgDash` = 8-on/3-off, the `…Dot` families append a dot+gap,
/// and the `sys…` variants are the tighter system pen patterns (1-unit dots).
pub(crate) fn preset_dash(val: &str) -> Option<Vec<f64>> {
    let a: &[f64] = match val {
        "solid" => return None,
        "dot" => &[1.0, 3.0],
        "dash" => &[4.0, 3.0],
        "lgDash" => &[8.0, 3.0],
        "dashDot" => &[4.0, 3.0, 1.0, 3.0],
        "lgDashDot" => &[8.0, 3.0, 1.0, 3.0],
        "lgDashDotDot" => &[8.0, 3.0, 1.0, 3.0, 1.0, 3.0],
        "sysDash" => &[2.0, 2.0],
        "sysDot" => &[1.0, 1.0],
        "sysDashDot" => &[2.0, 2.0, 1.0, 2.0],
        "sysDashDotDot" => &[2.0, 2.0, 1.0, 2.0, 1.0, 2.0],
        _ => return None,
    };
    Some(a.to_vec())
}

/// A `<a:custDash>` stop list → on/off array in MULTIPLES of the line width.
/// Each `<a:ds d="…" sp="…"/>` carries a dash length `d` and a space `sp` as
/// DML positive-percentage FRACTIONS (already parsed: 1.0 = 100% = one
/// line-width unit), expressed relative to the line width. Empty / all-zero
/// list → `None` (solid).
pub(crate) fn cust_dash(stops: &[(f64, f64)]) -> Option<Vec<f64>> {
    if stops.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(stops.len() * 2);
    for &(d, sp) in stops {
        out.push(d.max(0.0));
        out.push(sp.max(0.0));
    }
    if out.iter().all(|v| *v == 0.0) {
        return None;
    }
    Some(out)
}

/// A single primitive in a pattern tile, in tile-local px (0..=size on each
/// axis). The painter strokes/fills these in the FOREGROUND colour over a tile
/// already flood-filled with the BACKGROUND colour.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TilePrim {
    /// A stroked line segment (foreground), `lw` px wide.
    Line { x1: f64, y1: f64, x2: f64, y2: f64, lw: f64 },
    /// A filled foreground dot (square of side `s` at its top-left).
    Dot { x: f64, y: f64, s: f64 },
}

/// A pattern tile: a `size × size` px cell whose background is flood-filled,
/// then the foreground primitives stamped on top. The painter wraps it in a
/// `createPattern(…, "repeat")`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PatternTile {
    pub size: f64,
    pub prims: Vec<TilePrim>,
}

/// Build the tile for a `<a:pattFill prst="…">` preset, or `None` for presets
/// outside the tier-1 set (caller falls back to the 50/50 blend). Tiles use an
/// 8 px cell — large enough to read as texture at slide zoom, small enough to
/// tessellate cleanly.
///
/// Tier-1 coverage (the common ~15 + percentage shades): pct5/10/20/25/50 dot
/// screens; horz/vert/cross line grids (+ lt/dk weight variants); the four
/// diagonals (ltUpDiag/ltDnDiag/wdUpDiag + dnDiag/upDiag) and diagCross;
/// smGrid/lgGrid/dotGrid; trellis. Everything else → `None`.
pub(crate) fn pattern_tile(prst: &str) -> Option<PatternTile> {
    use TilePrim::*;
    const N: f64 = 8.0; // tile size
    let line = |x1, y1, x2, y2, lw| Line { x1, y1, x2, y2, lw };
    // A dot screen: foreground squares on a grid, `gap` apart, `s` px each.
    let dots = |gap: f64, s: f64| -> Vec<TilePrim> {
        let mut v = Vec::new();
        let mut y = 0.0;
        while y < N {
            let mut x = 0.0;
            while x < N {
                v.push(Dot { x, y, s });
                x += gap;
            }
            y += gap;
        }
        v
    };

    let prims = match prst {
        // ── percentage dot screens (denser = higher pct) ──────────────────
        "pct5" => dots(4.0, 1.0),
        "pct10" => dots(4.0, 1.0).into_iter().chain(dots(4.0, 1.0).into_iter().map(|p| {
            if let Dot { x, y, s } = p { Dot { x: x + 2.0, y: y + 2.0, s } } else { p }
        })).collect(),
        "pct20" | "pct25" | "pct30" => dots(2.0, 1.0),
        "pct40" | "pct50" | "pct60" => dots(2.0, 1.0)
            .into_iter()
            .chain(std::iter::once(Dot { x: 1.0, y: 1.0, s: 1.0 }))
            .collect(),

        // ── horizontal / vertical line grids ──────────────────────────────
        "horz" | "narHorz" | "dashHorz" => vec![line(0.0, 4.0, N, 4.0, 1.0)],
        "ltHorz" => vec![line(0.0, 4.0, N, 4.0, 1.0)],
        "dkHorz" => vec![line(0.0, 4.0, N, 4.0, 2.0)],
        "vert" | "narVert" | "dashVert" => vec![line(4.0, 0.0, 4.0, N, 1.0)],
        "ltVert" => vec![line(4.0, 0.0, 4.0, N, 1.0)],
        "dkVert" => vec![line(4.0, 0.0, 4.0, N, 2.0)],
        "cross" => vec![line(0.0, 4.0, N, 4.0, 1.0), line(4.0, 0.0, 4.0, N, 1.0)],

        // ── diagonals ─────────────────────────────────────────────────────
        // up-diagonal = bottom-left → top-right (a "/" stroke).
        "upDiag" | "ltUpDiag" | "dashUpDiag" => up_diag(N, 1.0),
        "wdUpDiag" | "dkUpDiag" => up_diag(N, 2.0),
        // down-diagonal = top-left → bottom-right (a "\" stroke).
        "dnDiag" | "ltDnDiag" | "dashDnDiag" => dn_diag(N, 1.0),
        "wdDnDiag" | "dkDnDiag" => dn_diag(N, 2.0),
        "diagCross" => up_diag(N, 1.0).into_iter().chain(dn_diag(N, 1.0)).collect(),

        // ── grids / trellis ───────────────────────────────────────────────
        "smGrid" => vec![
            line(0.0, 4.0, N, 4.0, 1.0),
            line(4.0, 0.0, 4.0, N, 1.0),
            line(0.0, 0.0, N, 0.0, 1.0),
            line(0.0, 0.0, 0.0, N, 1.0),
        ],
        "lgGrid" => vec![line(0.0, 0.0, N, 0.0, 1.0), line(0.0, 0.0, 0.0, N, 1.0)],
        "dotGrid" => vec![
            Dot { x: 0.0, y: 0.0, s: 1.0 },
            Dot { x: 4.0, y: 0.0, s: 1.0 },
            Dot { x: 0.0, y: 4.0, s: 1.0 },
            Dot { x: 4.0, y: 4.0, s: 1.0 },
        ],
        "trellis" => up_diag(N, 1.0).into_iter().chain(dn_diag(N, 1.0)).collect(),

        _ => return None,
    };
    Some(PatternTile { size: N, prims })
}

/// A "/" stroke tessellating across an `n×n` tile (drawn as two corner-to-edge
/// segments so the repeat is seamless at both wraps).
fn up_diag(n: f64, lw: f64) -> Vec<TilePrim> {
    vec![
        TilePrim::Line { x1: 0.0, y1: n, x2: n, y2: 0.0, lw },
        TilePrim::Line { x1: -1.0, y1: 1.0, x2: 1.0, y2: -1.0, lw },
        TilePrim::Line { x1: n - 1.0, y1: n + 1.0, x2: n + 1.0, y2: n - 1.0, lw },
    ]
}

/// A "\" stroke tessellating across an `n×n` tile.
fn dn_diag(n: f64, lw: f64) -> Vec<TilePrim> {
    vec![
        TilePrim::Line { x1: 0.0, y1: 0.0, x2: n, y2: n, lw },
        TilePrim::Line { x1: -1.0, y1: n - 1.0, x2: 1.0, y2: n + 1.0, lw },
        TilePrim::Line { x1: n - 1.0, y1: -1.0, x2: n + 1.0, y2: 1.0, lw },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_has_no_dash() {
        assert_eq!(preset_dash("solid"), None);
        assert_eq!(preset_dash("garbageVal"), None);
    }

    #[test]
    fn dash_families_map_to_canonical_arrays() {
        assert_eq!(preset_dash("dash"), Some(vec![4.0, 3.0]));
        assert_eq!(preset_dash("dot"), Some(vec![1.0, 3.0]));
        assert_eq!(preset_dash("lgDash"), Some(vec![8.0, 3.0]));
        // dashDot = a dash then a dot, each followed by a gap.
        assert_eq!(preset_dash("dashDot"), Some(vec![4.0, 3.0, 1.0, 3.0]));
        // sysDot is the tight 1-on/1-off system pen.
        assert_eq!(preset_dash("sysDot"), Some(vec![1.0, 1.0]));
        // every …Dot/…DotDot array has an even length (on/off pairs).
        for v in ["dashDot", "lgDashDot", "lgDashDotDot", "sysDashDot", "sysDashDotDot"] {
            let arr = preset_dash(v).unwrap();
            assert_eq!(arr.len() % 2, 0, "{v} must be on/off pairs");
        }
    }

    #[test]
    fn cust_dash_scales_percentages_to_line_width_units() {
        // 8.0 on, 3.0 off line-width units (fractions: 100% == 1 unit).
        assert_eq!(cust_dash(&[(8.0, 3.0)]), Some(vec![8.0, 3.0]));
        // empty / all-zero → solid (None).
        assert_eq!(cust_dash(&[]), None);
        assert_eq!(cust_dash(&[(0.0, 0.0)]), None);
        // multi-stop concatenates.
        assert_eq!(
            cust_dash(&[(4.0, 3.0), (1.0, 3.0)]),
            Some(vec![4.0, 3.0, 1.0, 3.0])
        );
    }

    #[test]
    fn known_patterns_build_a_tile_unknown_returns_none() {
        // a representative sample across the tiers all build non-empty tiles.
        for prst in [
            "pct5", "pct10", "pct25", "pct50", "horz", "vert", "cross", "ltHorz",
            "dkHorz", "ltUpDiag", "ltDnDiag", "wdUpDiag", "diagCross", "smGrid",
            "lgGrid", "dotGrid", "trellis",
        ] {
            let t = pattern_tile(prst).expect(prst);
            assert!(t.size > 0.0);
            assert!(!t.prims.is_empty(), "{prst} tile must stamp something");
        }
        // an unimplemented preset falls back (None → caller blends).
        assert_eq!(pattern_tile("sphere"), None);
        assert_eq!(pattern_tile("weave"), None);
        assert_eq!(pattern_tile("zigZag"), None);
    }

    #[test]
    fn denser_percent_has_more_dots() {
        let n = |p: &str| pattern_tile(p).unwrap().prims.len();
        // pct5 (sparse) < pct20 (dense) — the screen darkens with the percentage.
        assert!(n("pct5") < n("pct20"), "pct5 {} !< pct20 {}", n("pct5"), n("pct20"));
    }
}
