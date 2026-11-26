//! Border-edge resolution shared by WML border features: paragraph borders
//! (§17.3.1.24), page borders (§17.6.10 `w:pgBorders`). Table cell borders
//! (§17.4) keep their own CellBorderEdge in the tables domain. Verbatim
//! relocation from `lib.rs`.

use crate::engine::border::{Edge, LineStyle};
use crate::engine::units::pt_to_px;
use ooxml_wml::types::STBorder;

/// A paragraph or page border edge: its line, and its distance from the
/// text (or the page edge) it frames.
#[derive(Clone)]
pub(crate) struct BorderEdge {
    pub(crate) line: Edge,
    pub(crate) space_px: f64,
}

/// A WML border's drawn line (§17.18.2 `ST_Border`), or `None` for none and
/// nil. `sz` is in eighths of a point (§17.18.3), 2 to 96 for a line style;
/// an art border's `sz` is in points (1 to 31), and the art itself is drawn
/// as a single line (FLOOR). A line is never thinner than 1px, the nearest
/// this CSS-pixel layout comes to Word's one-device-pixel hairline.
pub(crate) fn border_line(value: STBorder, color: String, size: Option<u64>) -> Option<Edge> {
    use LineStyle as S;
    use STBorder as B;
    let style = match value {
        B::None | B::Nil => return None,
        B::Single => S::Single,
        B::Thick => S::Thick,
        B::Double => S::Double,
        B::Dotted => S::Dotted,
        B::Dashed => S::Dashed,
        B::DotDash => S::DotDash,
        B::DotDotDash => S::DotDotDash,
        B::Triple => S::Triple,
        B::ThinThickSmallGap => S::ThinThickSmallGap,
        B::ThickThinSmallGap => S::ThickThinSmallGap,
        B::ThinThickThinSmallGap => S::ThinThickThinSmallGap,
        B::ThinThickMediumGap => S::ThinThickMediumGap,
        B::ThickThinMediumGap => S::ThickThinMediumGap,
        B::ThinThickThinMediumGap => S::ThinThickThinMediumGap,
        B::ThinThickLargeGap => S::ThinThickLargeGap,
        B::ThickThinLargeGap => S::ThickThinLargeGap,
        B::ThinThickThinLargeGap => S::ThinThickThinLargeGap,
        B::Wave => S::Wave,
        B::DoubleWave => S::DoubleWave,
        B::DashSmallGap => S::DashSmallGap,
        B::DashDotStroked => S::DashDotStroked,
        B::ThreeDEmboss => S::ThreeDEmboss,
        B::ThreeDEngrave => S::ThreeDEngrave,
        B::Outset => S::Outset,
        B::Inset => S::Inset,
        _ => {
            let pt = size.unwrap_or(1).clamp(1, 31) as f64;
            return Some(Edge { color, width_px: pt_to_px(pt).max(1.0), dash: None, style: S::Single });
        }
    };
    let eighths = size.unwrap_or(4).clamp(2, 96) as f64;
    Some(Edge { color, width_px: pt_to_px(eighths / 8.0).max(1.0), dash: None, style })
}

/// A `CT_Border` (paragraph or table border element) as a drawn line.
pub(crate) fn border_element_line(
    b: &ooxml_wml::types::CTBorder,
    theme: &crate::engine::fonts::ThemeFonts,
) -> Option<Edge> {
    let color = theme.wml_border_color(b.color.as_deref(), b.theme_color.as_ref(), b.theme_tint.as_deref(), b.theme_shade.as_deref());
    border_line(b.value, color, b.size)
}

/// A paragraph border edge (§17.3.1.24): its line and its `space`, the
/// distance from the text in points (§17.18.68).
pub(crate) fn resolve_border_edge(
    border: Option<&ooxml_wml::types::CTBorder>,
    theme: &crate::engine::fonts::ThemeFonts,
) -> Option<Box<BorderEdge>> {
    let b = border?;
    let line = border_element_line(b, theme)?;
    Some(Box::new(BorderEdge { line, space_px: pt_to_px(b.space.unwrap_or(0) as f64) }))
}

/// §17.6.10 `w:pgBorders` resolved for rendering: up to four edges plus the
/// placement rules that position the frame on each page.
pub(crate) struct PageBorders {
    pub(crate) top: Option<BorderEdge>,
    pub(crate) bottom: Option<BorderEdge>,
    pub(crate) left: Option<BorderEdge>,
    pub(crate) right: Option<BorderEdge>,
    /// `offsetFrom="page"` → the edge `space` is measured inward from the
    /// physical page edge; `offsetFrom="text"` (the default) → measured outward
    /// from the text margin. §17.6.10.
    pub(crate) offset_from_page: bool,
    /// Which pages render the border (allPages / firstPage / notFirstPage).
    pub(crate) display: ooxml_wml::types::STPageBorderDisplay,
}

/// Resolve one page-border edge (`<w:top>`/`<w:left>`/… under `w:pgBorders`) to a
/// drawable `BorderEdge`. The three edge element types (`CTTopPageBorder`,
/// `CTBottomPageBorder`, `CTPageBorder`) share the `val`/`color`/`sz`/`space`
/// fields, so callers pass those four directly, `color` already resolved
/// (`ThemeFonts::wml_border_color`). `space` (ST_PointMeasure, points) is
/// converted to px (×96/72) because page-border offsets are large and visibly
/// position the frame.
pub(crate) fn page_border_edge(
    value: &STBorder,
    color: String,
    size: Option<u64>,
    space: Option<u64>,
) -> Option<BorderEdge> {
    let line = border_line(*value, color, size)?;
    Some(BorderEdge { line, space_px: pt_to_px(space.unwrap_or(0) as f64) })
}

#[cfg(test)]
mod page_border_tests {
    use super::page_border_edge;
    use ooxml_wml::types::STBorder;

    #[test]
    fn single_edge_resolves_color_width_space() {
        // sz=24 eighths = 3pt → 4px; space=24pt → 32px; explicit hex colour.
        let e = page_border_edge(&STBorder::Single, "#C00000".into(), Some(24), Some(24)).unwrap();
        assert_eq!(e.line.color, "#C00000");
        assert!((e.line.width_px - 4.0).abs() < 1e-9);
        assert!((e.space_px - 32.0).abs() < 1e-9); // 24 * 96/72
    }

    #[test]
    fn widths_are_eighths_of_a_point_clamped_and_art_sizes_are_points() {
        use super::border_line;
        use crate::engine::border::LineStyle;
        let w = |v: STBorder, sz| border_line(v, "#000000".into(), Some(sz)).unwrap();
        assert!((w(STBorder::Single, 12).width_px - 2.0).abs() < 1e-9, "1.5pt = 2px");
        assert!((w(STBorder::Single, 400).width_px - 16.0).abs() < 1e-9, "clamped to 96 eighths = 12pt");
        assert_eq!(w(STBorder::Double, 4).style, LineStyle::Double);
        // An art border's sz=20 is 20pt, not 2.5pt.
        assert!((w(STBorder::Apples, 20).width_px - 80.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn none_and_nil_yield_no_edge() {
        assert!(page_border_edge(&STBorder::None, "#000000".into(), Some(24), Some(24)).is_none());
        assert!(page_border_edge(&STBorder::Nil, "#000000".into(), None, None).is_none());
    }

    #[test]
    fn auto_colour_falls_back_to_black_and_defaults() {
        // "auto" colour → black; absent sz defaults to 4 (0.5pt, at the 1px
        // floor); absent space → 0.
        let auto = crate::engine::fonts::ThemeFonts::empty().wml_border_color(Some("auto"), None, None, None);
        let e = page_border_edge(&STBorder::Single, auto, None, None).unwrap();
        assert_eq!(e.line.color, "#000000");
        assert!((e.line.width_px - 1.0).abs() < 1e-9);
        assert_eq!(e.space_px, 0.0);
    }
}

