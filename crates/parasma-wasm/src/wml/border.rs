//! Border-edge resolution shared by WML border features: paragraph borders
//! (§17.3.1.24), page borders (§17.6.10 `w:pgBorders`). Table cell borders
//! (§17.4) keep their own CellBorderEdge in the tables domain. Verbatim
//! relocation from `lib.rs`.

use crate::engine::color::ooxml_color_to_css;
use crate::engine::units::pt_to_px;
use ooxml_wml::types::STBorder;

#[derive(Clone)]
pub(crate) struct BorderEdge {
    pub(crate) color: String,
    pub(crate) width_px: f64,
    pub(crate) space_px: f64,
}

pub(crate) fn resolve_border_edge(border: Option<&ooxml_wml::types::CTBorder>) -> Option<BorderEdge> {
    let b = border?;
    if b.value == STBorder::None || b.value == STBorder::Nil {
        return None;
    }
    let color = b.color.as_deref()
        .and_then(ooxml_color_to_css)
        .unwrap_or_else(|| "#000000".to_string());
    // `sz` is in eighths of a point (ST_EighthPointMeasure, §17.18.3). The /8.0
    // yields POINTS and is used directly as px (no ×96/72 unlike `space_px`
    // below) — a deliberate thin-hairline convention shared with every border
    // in the codebase; do not "fix" it to pt_to_px without re-baselining pins.
    let sz = b.size.unwrap_or(4) as f64;
    let width_px = (sz / 8.0).max(0.5);
    let space = b.space.unwrap_or(0) as f64;
    Some(BorderEdge { color, width_px, space_px: space })
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
/// fields, so callers pass those four directly. Mirrors `resolve_border_edge`'s
/// colour + width handling; `space` (ST_PointMeasure, points) is converted to px
/// (×96/72) because page-border offsets are large and visibly position the frame.
pub(crate) fn page_border_edge(
    value: &STBorder,
    color: Option<&str>,
    size: Option<u64>,
    space: Option<u64>,
) -> Option<BorderEdge> {
    if *value == STBorder::None || *value == STBorder::Nil {
        return None;
    }
    let color = color
        .and_then(ooxml_color_to_css)
        .unwrap_or_else(|| "#000000".to_string());
    // sz = eighths of a point (ST_EighthPointMeasure, §17.18.3) — see
    // `resolve_border_edge` for the thin-hairline (point-as-px) rationale.
    let sz = size.unwrap_or(4) as f64;
    let width_px = (sz / 8.0).max(0.5);
    let space_px = pt_to_px(space.unwrap_or(0) as f64); // ×96/72
    Some(BorderEdge { color, width_px, space_px })
}

#[cfg(test)]
mod page_border_tests {
    use super::page_border_edge;
    use ooxml_wml::types::STBorder;

    #[test]
    fn single_edge_resolves_color_width_space() {
        // sz=24 eighths → 3px; space=24pt → 32px; explicit hex colour.
        let e = page_border_edge(&STBorder::Single, Some("C00000"), Some(24), Some(24)).unwrap();
        assert_eq!(e.color, "#C00000");
        assert!((e.width_px - 3.0).abs() < 1e-9);
        assert!((e.space_px - 32.0).abs() < 1e-9); // 24 * 96/72
    }

    #[test]
    fn none_and_nil_yield_no_edge() {
        assert!(page_border_edge(&STBorder::None, Some("000000"), Some(24), Some(24)).is_none());
        assert!(page_border_edge(&STBorder::Nil, None, None, None).is_none());
    }

    #[test]
    fn auto_colour_falls_back_to_black_and_defaults() {
        // "auto" colour → black; absent sz defaults to 4 (0.5px); absent space → 0.
        let e = page_border_edge(&STBorder::Single, Some("auto"), None, None).unwrap();
        assert_eq!(e.color, "#000000");
        assert!((e.width_px - 0.5).abs() < 1e-9);
        assert_eq!(e.space_px, 0.0);
    }
}

