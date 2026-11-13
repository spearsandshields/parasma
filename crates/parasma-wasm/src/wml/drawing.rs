//! DrawingML / VML float + text-box PARSE cluster (§20.4.2 anchored drawings,
//! §17.3.3 pictures). Resolves anchor position + text-wrap data (`WrapInfo`/
//! `WrapMode`/`WrapTextSide`) from a `<w:drawing>`/`<w:pict>`, and extracts
//! text-box content + shape chrome (fill/border) for both DrawingML shapes
//! (`wsp`) and legacy VML shapes. Verbatim relocation from `lib.rs`;
//! consumed cross-module by `wml::linebreak` (anchor/textbox extraction) and
//! `wml::float` (the wrap data types). The exclusion geometry + line packer
//! that *consume* `WrapInfo` live in `wml::float`; the anchor-position model
//! types (`AnchorPos`/`AnchorAlign`/`AnchorHRelFrom`/`AnchorVRelFrom`) also
//! live in `wml::float` and are imported here via `crate::`.
//!
//! Purity is enforced by the oracle snapshot tests — their geometry hashes
//! must not drift on a pure move.

use crate::engine::color::vml_color_to_css;
use crate::engine::units::{css_length_to_px, emu_to_px};
use crate::engine::vml::{vml_attr, vml_find_descendant, xml_local_name};
use crate::wml::float::{AnchorAlign, AnchorHRelFrom, AnchorPos, AnchorVRelFrom};
use ooxml_wml::types::CTTxbxContent;

/// §20.4.2 text-wrap mode for a floating drawing object. `None` = the object
/// overlaps text (no exclusion); the rest create exclusion regions.
#[derive(Clone)]
pub(crate) enum WrapMode {
    /// §20.4.2.15 wrapNone — no exclusion; object floats over/behind text.
    None,
    /// §20.4.2.17 wrapSquare — text avoids the object's bounding rectangle.
    Square,
    /// §20.4.2.16/§20.4.2.19 wrapTight/wrapThrough — text follows a polygon
    /// outline. `polygon` is the closed vertex ring in *page px* (already
    /// mapped from the 21600-relative coords against the object box). When
    /// absent (malformed doc), callers fall back to Square.
    Polygon { polygon: Option<Vec<(f64, f64)>> },
    /// §20.4.2.18 wrapTopAndBottom — text stops above and resumes below.
    TopAndBottom,
}

/// §20.4.3.7 ST_WrapText — side(s) of a float text may flow on.
#[derive(Clone, Copy)]
pub(crate) enum WrapTextSide { BothSides, Left, Right, Largest }

/// Resolved wrap data for one floating object. Distances are in px; the
/// polygon (if any) is already mapped to absolute object-box-relative px
/// offsets (origin = object upper-left), ready to translate by the object's
/// resolved page position.
#[derive(Clone)]
pub(crate) struct WrapInfo {
    pub(crate) mode: WrapMode,
    pub(crate) wrap_text: WrapTextSide,
    pub(crate) dist_t: f64,
    pub(crate) dist_b: f64,
    pub(crate) dist_l: f64,
    pub(crate) dist_r: f64,
}

impl WrapInfo {
    /// Default for an anchor with no recognised wrap child: behave as wrapNone.
    fn none() -> Self {
        WrapInfo {
            mode: WrapMode::None,
            wrap_text: WrapTextSide::BothSides,
            dist_t: 0.0, dist_b: 0.0, dist_l: 0.0, dist_r: 0.0,
        }
    }
}

pub(crate) fn is_layout_control(c: char) -> bool {
    matches!(c, '\n' | '\r')
}

pub(crate) fn parse_anchor_position(drawing: &ooxml_wml::types::CTDrawing, obj_w_px: f64, obj_h_px: f64)
    -> (AnchorPos, AnchorPos, AnchorHRelFrom, AnchorVRelFrom, bool, WrapInfo)
{
    let mut h_pos = AnchorPos::Offset(0.0);
    let mut v_pos = AnchorPos::Offset(0.0);
    let mut h_rel = AnchorHRelFrom::Column;
    let mut v_rel = AnchorVRelFrom::Paragraph;
    let mut behind = false;
    let mut wrap = WrapInfo::none();

    for child in &drawing.extra_children {
        let ooxml_xml::RawXmlNode::Element(ref elem) = child.node else { continue };
        if xml_local_name(&elem.name) != "anchor" { continue; }

        for (a, v) in &elem.attributes {
            if xml_local_name(a) == "behindDoc" && (v == "1" || v == "true") {
                behind = true;
            }
        }

        for inner in &elem.children {
            let ooxml_xml::RawXmlNode::Element(ref ie) = inner else { continue };
            match xml_local_name(&ie.name) {
                "positionH" => {
                    for (a, v) in &ie.attributes {
                        if xml_local_name(a) == "relativeFrom" {
                            h_rel = match v.as_str() {
                                "page" => AnchorHRelFrom::Page,
                                "margin" | "leftMargin" | "rightMargin"
                                    | "insideMargin" | "outsideMargin" => AnchorHRelFrom::Margin,
                                "character" => AnchorHRelFrom::Character,
                                _ => AnchorHRelFrom::Column,
                            };
                        }
                    }
                    h_pos = parse_pos_child(ie);
                }
                "positionV" => {
                    for (a, v) in &ie.attributes {
                        if xml_local_name(a) == "relativeFrom" {
                            v_rel = match v.as_str() {
                                "page" => AnchorVRelFrom::Page,
                                "margin" | "topMargin" | "bottomMargin"
                                    | "insideMargin" | "outsideMargin" => AnchorVRelFrom::Margin,
                                "line" => AnchorVRelFrom::Line,
                                _ => AnchorVRelFrom::Paragraph,
                            };
                        }
                    }
                    v_pos = parse_pos_child(ie);
                }
                "wrapNone" => {
                    wrap = WrapInfo { mode: WrapMode::None, ..WrapInfo::none() };
                }
                "wrapSquare" | "wrapTight" | "wrapThrough" | "wrapTopAndBottom" => {
                    wrap = parse_wrap_child(ie, obj_w_px, obj_h_px);
                }
                _ => {}
            }
        }
        break;
    }
    (h_pos, v_pos, h_rel, v_rel, behind, wrap)
}

/// Read the text-distance padding `(distL, distT, distR, distB)` in CSS px
/// from a drawing's first `<wp:inline>` (or `<wp:anchor>`) element — the
/// `distL`/`distT`/`distR`/`distB` attributes (ECMA-376 §20.4.2.8 / §20.4.2.3,
/// type ST_WrapDistance §20.4.3.6). These specify the minimum gap Word keeps
/// between the drawing's edges and the surrounding text/lines. Missing
/// attributes default to 0 (the ST_WrapDistance default). Mirrors the raw-XML
/// `extra_children` walk used by the rest of this module rather than adding a
/// parser-fork accessor (a single new walker stays in-crate until the pattern
/// repeats).
pub(crate) fn inline_dist_px(drawing: &ooxml_wml::types::CTDrawing) -> (f64, f64, f64, f64) {
    for child in &drawing.extra_children {
        let ooxml_xml::RawXmlNode::Element(ref elem) = child.node else { continue };
        let local = xml_local_name(&elem.name);
        if local != "inline" && local != "anchor" { continue; }
        let read = |key: &str| -> f64 {
            elem.attributes
                .iter()
                .find(|(a, _)| xml_local_name(a) == key)
                .and_then(|(_, v)| v.parse::<i64>().ok())
                .map(emu_to_px)
                .unwrap_or(0.0)
        };
        return (read("distL"), read("distT"), read("distR"), read("distB"));
    }
    (0.0, 0.0, 0.0, 0.0)
}

/// Parse a wrapSquare/wrapTight/wrapThrough/wrapTopAndBottom element into
/// `WrapInfo`. `obj_w_px`/`obj_h_px` are the object's box dimensions, needed
/// to map the 21600-relative wrapPolygon coords (§20.4.2.10) into px offsets
/// from the object's upper-left corner.
fn parse_wrap_child(elem: &ooxml_xml::RawXmlElement, obj_w_px: f64, obj_h_px: f64) -> WrapInfo {
    let local = xml_local_name(&elem.name);

    // §20.4.2: distT/distB/distL/distR are EMU attributes on the wrap element.
    let mut dist_t = 0.0;
    let mut dist_b = 0.0;
    let mut dist_l = 0.0;
    let mut dist_r = 0.0;
    let mut wrap_text = WrapTextSide::BothSides;
    for (a, v) in &elem.attributes {
        match xml_local_name(a) {
            "distT" => dist_t = v.parse::<i64>().map(emu_to_px).unwrap_or(0.0),
            "distB" => dist_b = v.parse::<i64>().map(emu_to_px).unwrap_or(0.0),
            "distL" => dist_l = v.parse::<i64>().map(emu_to_px).unwrap_or(0.0),
            "distR" => dist_r = v.parse::<i64>().map(emu_to_px).unwrap_or(0.0),
            "wrapText" => {
                wrap_text = match v.as_str() {
                    "left" => WrapTextSide::Left,
                    "right" => WrapTextSide::Right,
                    "largest" => WrapTextSide::Largest,
                    _ => WrapTextSide::BothSides,
                };
            }
            _ => {}
        }
    }

    let mode = match local {
        "wrapTopAndBottom" => WrapMode::TopAndBottom,
        "wrapTight" | "wrapThrough" => {
            let polygon = parse_wrap_polygon(elem, obj_w_px, obj_h_px);
            WrapMode::Polygon { polygon }
        }
        // wrapSquare (and any unexpected value) → rectangle exclusion.
        _ => WrapMode::Square,
    };

    WrapInfo { mode, wrap_text, dist_t, dist_b, dist_l, dist_r }
}

/// §20.4.2.10 CT_WrapPath — parse `<wp:wrapPolygon>` into a closed vertex ring
/// in px, relative to the object's upper-left corner. Polygon coords are in a
/// 21600-based fraction of the object box (matching VML), so each coordinate
/// maps as `px = (coord / 21600) * object_dimension`. Returns None when the
/// polygon is missing or degenerate (< 3 points) — caller falls back to Square.
fn parse_wrap_polygon(wrap_elem: &ooxml_xml::RawXmlElement, obj_w_px: f64, obj_h_px: f64)
    -> Option<Vec<(f64, f64)>>
{
    const VML_UNITS: f64 = 21600.0;
    let poly_elem = wrap_elem.children.iter().find_map(|n| {
        if let ooxml_xml::RawXmlNode::Element(ref e) = n {
            if xml_local_name(&e.name) == "wrapPolygon" { return Some(e); }
        }
        None
    })?;

    let map_pt = |e: &ooxml_xml::RawXmlElement| -> Option<(f64, f64)> {
        let mut x = None;
        let mut y = None;
        for (a, v) in &e.attributes {
            match xml_local_name(a) {
                "x" => x = v.parse::<f64>().ok(),
                "y" => y = v.parse::<f64>().ok(),
                _ => {}
            }
        }
        Some((x? / VML_UNITS * obj_w_px, y? / VML_UNITS * obj_h_px))
    };

    let mut points: Vec<(f64, f64)> = Vec::new();
    for n in &poly_elem.children {
        let ooxml_xml::RawXmlNode::Element(ref e) = n else { continue };
        match xml_local_name(&e.name) {
            "start" | "lineTo" => {
                if let Some(p) = map_pt(e) { points.push(p); }
            }
            _ => {}
        }
    }

    // Drop an explicit closing vertex if it duplicates the start; the ring is
    // treated as implicitly closed downstream.
    if points.len() >= 2 && points.first() == points.last() {
        points.pop();
    }
    if points.len() < 3 { return None; }
    Some(points)
}

fn parse_pos_child(parent: &ooxml_xml::RawXmlElement) -> AnchorPos {
    for ch in &parent.children {
        let ooxml_xml::RawXmlNode::Element(ref pe) = ch else { continue };
        match xml_local_name(&pe.name) {
            "posOffset" => {
                if let Some(ooxml_xml::RawXmlNode::Text(t)) = pe.children.first() {
                    if let Ok(emu) = t.trim().parse::<i64>() {
                        return AnchorPos::Offset(emu_to_px(emu));
                    }
                }
            }
            "align" => {
                if let Some(ooxml_xml::RawXmlNode::Text(t)) = pe.children.first() {
                    return AnchorPos::Align(match t.trim() {
                        "center" => AnchorAlign::Center,
                        "right" => AnchorAlign::Right,
                        "top" => AnchorAlign::Top,
                        "bottom" => AnchorAlign::Bottom,
                        _ => AnchorAlign::Left,
                    });
                }
            }
            _ => {}
        }
    }
    AnchorPos::Offset(0.0)
}

pub(crate) struct ExtractedTextBox {
    pub(crate) content: CTTxbxContent,
    pub(crate) is_anchored: bool,
    pub(crate) fill_color: Option<String>,
    pub(crate) border_color: Option<String>,
    pub(crate) border_width_px: f64,
}

pub(crate) fn extract_drawing_text_box(drawing: &ooxml_wml::types::CTDrawing) -> Option<ExtractedTextBox> {
    for child in &drawing.extra_children {
        let ooxml_xml::RawXmlNode::Element(ref elem) = child.node else { continue };
        let ln = xml_local_name(&elem.name);
        let is_anchored = ln == "anchor";
        if ln != "anchor" && ln != "inline" { continue; }

        // Find wsp element to get shape properties + text box content
        if let Some(wsp) = vml_find_descendant(elem, "wsp") {
            let content = find_txbx_content_in(wsp)?;
            let (fill_color, border_color, border_width_px) = extract_shape_chrome(wsp);
            return Some(ExtractedTextBox {
                content,
                is_anchored,
                fill_color,
                border_color,
                border_width_px,
            });
        }
    }
    None
}

fn extract_shape_chrome(wsp: &ooxml_xml::RawXmlElement) -> (Option<String>, Option<String>, f64) {
    let mut fill_color = None;
    let mut border_color = None;
    let mut border_width_px = 0.0;

    // Find spPr child
    for child in &wsp.children {
        let ooxml_xml::RawXmlNode::Element(ref e) = child else { continue };
        if xml_local_name(&e.name) != "spPr" { continue; }

        for sp_child in &e.children {
            let ooxml_xml::RawXmlNode::Element(ref se) = sp_child else { continue };
            match xml_local_name(&se.name) {
                "solidFill" => {
                    fill_color = extract_color_from_fill(se);
                }
                "noFill" => {
                    fill_color = None;
                }
                "ln" => {
                    let width_emu: i64 = vml_attr(se, "w")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(12700);
                    border_width_px = emu_to_px(width_emu);
                    for ln_child in &se.children {
                        let ooxml_xml::RawXmlNode::Element(ref le) = ln_child else { continue };
                        match xml_local_name(&le.name) {
                            "solidFill" => {
                                border_color = extract_color_from_fill(le);
                            }
                            "noFill" => {
                                border_color = None;
                                border_width_px = 0.0;
                            }
                            _ => {}
                        }
                    }
                    if border_color.is_none() && border_width_px > 0.0 {
                        border_color = Some("#000000".to_string());
                    }
                }
                _ => {}
            }
        }
        break;
    }
    (fill_color, border_color, border_width_px)
}

fn extract_color_from_fill(fill_elem: &ooxml_xml::RawXmlElement) -> Option<String> {
    for child in &fill_elem.children {
        let ooxml_xml::RawXmlNode::Element(ref e) = child else { continue };
        match xml_local_name(&e.name) {
            "srgbClr" => {
                return vml_attr(e, "val").map(|v| format!("#{}", v));
            }
            "schemeClr" | "sysClr" => {
                // Simplified: use lastClr if available, else default
                if let Some((_, v)) = e.attributes.iter().find(|(a, _)| xml_local_name(a) == "lastClr") {
                    return Some(format!("#{}", v));
                }
            }
            _ => {}
        }
    }
    None
}

fn find_txbx_content_in(elem: &ooxml_xml::RawXmlElement) -> Option<CTTxbxContent> {
    let ln = xml_local_name(&elem.name);
    if ln == "txbxContent" {
        return elem.parse_as::<CTTxbxContent>().ok();
    }
    for child in &elem.children {
        let ooxml_xml::RawXmlNode::Element(ref e) = child else { continue };
        if let Some(content) = find_txbx_content_in(e) {
            return Some(content);
        }
    }
    None
}

pub(crate) fn extract_pict_text_box(pict: &ooxml_wml::types::CTPicture) -> Option<ExtractedTextBox> {
    for child in &pict.extra_children {
        let ooxml_xml::RawXmlNode::Element(ref elem) = child.node else { continue };
        let ln = xml_local_name(&elem.name);
        if !matches!(ln, "shape" | "rect" | "roundrect" | "oval") { continue; }
        if let Some(content) = find_txbx_content_in(elem) {
            let fill_color = vml_attr(elem, "fillcolor").map(vml_color_to_css);
            let stroke_color = vml_attr(elem, "strokecolor")
                .map(vml_color_to_css)
                .or(Some("#000000".to_string()));
            let no_fill = elem.attributes.iter()
                .any(|(a, v)| xml_local_name(a) == "filled" && (v == "f" || v == "false"));
            let no_stroke = elem.attributes.iter()
                .any(|(a, v)| xml_local_name(a) == "stroked" && (v == "f" || v == "false"));
            return Some(ExtractedTextBox {
                content,
                is_anchored: false,
                fill_color: if no_fill { None } else { fill_color },
                border_color: if no_stroke { None } else { stroke_color },
                border_width_px: if no_stroke { 0.0 } else { 1.0 },
            });
        }
    }
    None
}

/// Extract width/height from VML shape's CSS style attribute.
pub(crate) fn extract_vml_dimensions(pict: &ooxml_wml::types::CTPicture) -> Option<(f64, f64)> {
    for child in &pict.extra_children {
        let ooxml_xml::RawXmlNode::Element(ref elem) = child.node else { continue };
        let ln = xml_local_name(&elem.name);
        if !matches!(ln, "shape" | "rect" | "roundrect" | "oval") { continue; }
        let style_str = vml_attr(elem, "style").unwrap_or("");
        let mut w = None;
        let mut h = None;
        for part in style_str.split(';') {
            let part = part.trim();
            if let Some((k, v)) = part.split_once(':') {
                match k.trim() {
                    "width" => w = Some(css_length_to_px(v.trim())),
                    "height" => h = Some(css_length_to_px(v.trim())),
                    _ => {}
                }
            }
        }
        if let (Some(ww), Some(hh)) = (w, h) {
            return Some((ww, hh));
        }
    }
    None
}

#[cfg(test)]
mod dist_tests {
    use super::*;
    use ooxml_xml::{PositionedNode, RawXmlElement, RawXmlNode};

    /// Build a `CTDrawing` whose first `extra_child` is `<wp:NAME ...attrs/>`.
    fn drawing_with(name: &str, attrs: &[(&str, &str)]) -> ooxml_wml::types::CTDrawing {
        let elem = RawXmlElement {
            name: name.to_string(),
            attributes: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            children: Vec::new(),
            self_closing: true,
        };
        let mut d = ooxml_wml::types::CTDrawing::default();
        d.extra_children
            .push(PositionedNode::new(0, RawXmlNode::Element(elem)));
        d
    }

    #[test]
    fn reads_all_four_dist_attrs_from_inline_in_emu_to_px() {
        // 457200 EMU = 0.5" = 48px (9525 EMU/px); 228600 = 0.25" = 24px.
        let d = drawing_with(
            "wp:inline",
            &[
                ("distL", "457200"),
                ("distT", "228600"),
                ("distR", "457200"),
                ("distB", "228600"),
            ],
        );
        assert_eq!(inline_dist_px(&d), (48.0, 24.0, 48.0, 24.0));
    }

    #[test]
    fn missing_dist_attrs_default_to_zero() {
        // §20.4.2.8 / ST_WrapDistance §20.4.3.6 default is 0.
        let d = drawing_with("wp:inline", &[("distL", "95250")]);
        assert_eq!(inline_dist_px(&d), (10.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn also_reads_from_anchor_element() {
        let d = drawing_with("wp:anchor", &[("distT", "95250"), ("distB", "190500")]);
        assert_eq!(inline_dist_px(&d), (0.0, 10.0, 0.0, 20.0));
    }

    #[test]
    fn no_inline_or_anchor_yields_all_zero() {
        let d = ooxml_wml::types::CTDrawing::default();
        assert_eq!(inline_dist_px(&d), (0.0, 0.0, 0.0, 0.0));
    }
}
