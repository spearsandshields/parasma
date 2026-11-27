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

use crate::engine::border::{Edge, LineStyle};
use crate::engine::color::{apply_color_transforms, css_color, dml_base_color, vml_color_to_css, DmlTransform, DML_COLOR_MODELS};
use crate::engine::fonts::ThemeFonts;
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
    matches!(c, '\n' | '\r' | crate::wml::linebreak::PAGE_BREAK_CHAR | crate::wml::linebreak::COLUMN_BREAK_CHAR)
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
        // §20.4.2.3: the anchor's own distances, which apply to every side
        // its wrap element does not set. Word writes them here (typically
        // 0.125" left and right) with a bare <wp:wrapSquare/>.
        let anchor_dist = |key: &str| vml_attr(elem, key).and_then(|v| v.parse::<i64>().ok()).map(emu_to_px).unwrap_or(0.0);
        let anchor_dists = (anchor_dist("distL"), anchor_dist("distT"), anchor_dist("distR"), anchor_dist("distB"));

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
                    wrap = parse_wrap_child(ie, obj_w_px, obj_h_px, anchor_dists);
                }
                _ => {}
            }
        }
        break;
    }
    (h_pos, v_pos, h_rel, v_rel, behind, wrap)
}

/// The image key a SmartArt diagram is laid out under until diagrams are
/// drawn: it names no picture, so it paints as the missing-picture placeholder.
pub(crate) const UNDRAWN_DIAGRAM_KEY: &str = "\u{0}diagram";

/// Whether a drawing is a SmartArt diagram: its `a:graphicData` has the
/// diagram URI (§21.4).
pub(crate) fn is_diagram(drawing: &ooxml_wml::types::CTDrawing) -> bool {
    fn has_diagram(e: &ooxml_xml::RawXmlElement) -> bool {
        (xml_local_name(&e.name) == "graphicData"
            && vml_attr(e, "uri").is_some_and(|u| u.ends_with("/drawingml/2006/diagram")))
            || e.children.iter().any(|c| matches!(c, ooxml_xml::RawXmlNode::Element(ce) if has_diagram(ce)))
    }
    drawing.extra_children.iter().any(|c| matches!(&c.node, ooxml_xml::RawXmlNode::Element(e) if has_diagram(e)))
}

/// Whether a drawing is anchored (`wp:anchor`) rather than inline.
pub(crate) fn is_anchored(drawing: &ooxml_wml::types::CTDrawing) -> bool {
    drawing.extra_children.iter().any(|c| matches!(&c.node, ooxml_xml::RawXmlNode::Element(e) if xml_local_name(&e.name) == "anchor"))
}

/// Whether a DrawingML object is hidden: present in the document but not
/// displayed (`wp:docPr@hidden`, §20.4.2.5). Word hides it; so does layout.
pub(crate) fn drawing_is_hidden(drawing: &ooxml_wml::types::CTDrawing) -> bool {
    drawing.extra_children.iter().any(|child| {
        let ooxml_xml::RawXmlNode::Element(ref elem) = child.node else { return false };
        matches!(xml_local_name(&elem.name), "inline" | "anchor")
            && elem.children.iter().any(|c| match c {
                ooxml_xml::RawXmlNode::Element(e) if xml_local_name(&e.name) == "docPr" => {
                    vml_attr(e, "hidden").is_some_and(crate::on_off)
                }
                _ => false,
            })
    })
}

/// Whether a VML picture's shape is hidden: `visibility:hidden` in its CSS
/// `style`, a property every VML shape element defines (Part 4 §19.1.2, e.g.
/// §19.1.2.1).
pub(crate) fn pict_is_hidden(pict: &ooxml_wml::types::CTPicture) -> bool {
    pict.extra_children.iter().any(|child| {
        let ooxml_xml::RawXmlNode::Element(ref elem) = child.node else { return false };
        matches!(xml_local_name(&elem.name), "shape" | "rect" | "roundrect" | "oval" | "group" | "line")
            && vml_style_value(elem, "visibility") == Some("hidden")
    })
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
/// from the object's upper-left corner. `anchor_dists` (left, top, right,
/// bottom px) are the anchor's distances, used for each side the wrap element
/// does not set.
///
/// Declared: `wp:effectExtent` (§20.4.2.6, room for shadows and glows) is not
/// added to the wrap area.
fn parse_wrap_child(
    elem: &ooxml_xml::RawXmlElement,
    obj_w_px: f64,
    obj_h_px: f64,
    anchor_dists: (f64, f64, f64, f64),
) -> WrapInfo {
    let local = xml_local_name(&elem.name);

    // §20.4.2: distT/distB/distL/distR are EMU attributes on the wrap element.
    let (mut dist_l, mut dist_t, mut dist_r, mut dist_b) = anchor_dists;
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
    pub(crate) frame: TextBoxFrame,
}

/// Where a text box's content sits between its top and bottom margins
/// (`bodyPr@anchor`, ST_TextAnchoringType §20.1.10.60; VML `v-text-anchor`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextBoxAnchor {
    Top,
    Center,
    Bottom,
}

/// A text box's inner margins, px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Insets {
    pub(crate) left: f64,
    pub(crate) top: f64,
    pub(crate) right: f64,
    pub(crate) bottom: f64,
}

impl Insets {
    /// 0.1 inch left and right, 0.05 inch top and bottom: the default of both
    /// DrawingML (`bodyPr` lIns/tIns/rIns/bIns, §20.4.2.22, §21.1.2.1.1) and VML
    /// (`v:textbox@inset`, Part 4 §19.1.2.22).
    pub(crate) fn default_px() -> Self {
        Insets { left: emu_to_px(91_440), top: emu_to_px(45_720), right: emu_to_px(91_440), bottom: emu_to_px(45_720) }
    }
}

/// How a text box is drawn around its content: fill, outline, inner margins
/// and the content's vertical position. One value, carried whole from the
/// markup to the paint.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TextBoxFrame {
    /// A CSS colour, or `None` for no fill.
    pub(crate) fill_color: Option<String>,
    /// A CSS colour, or `None` for no outline.
    pub(crate) border_color: Option<String>,
    pub(crate) border_width_px: f64,
    pub(crate) insets: Insets,
    pub(crate) anchor: TextBoxAnchor,
    /// The colour of text that sets none of its own: a shape style's
    /// `fontRef` colour (§20.1.4.1.17), white on Word's coloured shapes.
    /// `None` keeps the document's default text colour.
    pub(crate) text_color: Option<String>,
}

impl Default for TextBoxFrame {
    fn default() -> Self {
        TextBoxFrame {
            fill_color: None,
            border_color: None,
            border_width_px: 0.0,
            insets: Insets::default_px(),
            anchor: TextBoxAnchor::Top,
            text_color: None,
        }
    }
}

#[cfg(test)]
impl TextBoxFrame {
    /// Serialize every field into the oracle ITEM stream. The exhaustive
    /// destructure fails to compile when a field is added and not emitted.
    pub(crate) fn dump_into(&self, out: &mut String) {
        use std::fmt::Write;
        let TextBoxFrame { fill_color, border_color, border_width_px, insets, anchor, text_color } = self;
        let Insets { left, top, right, bottom } = insets;
        let _ = write!(out, "fill{} border{}/{border_width_px:.2} ins{left:.2},{top:.2},{right:.2},{bottom:.2} anc{anchor:?}",
            fill_color.as_deref().unwrap_or("-"),
            border_color.as_deref().unwrap_or("-"));
        // Written only when set, so frames without one dump as before.
        if let Some(c) = text_color {
            let _ = write!(out, " text{c}");
        }
    }
}

pub(crate) fn extract_drawing_text_box(
    drawing: &ooxml_wml::types::CTDrawing,
    theme: &ThemeFonts,
) -> Option<ExtractedTextBox> {
    for child in &drawing.extra_children {
        let ooxml_xml::RawXmlNode::Element(ref elem) = child.node else { continue };
        let ln = xml_local_name(&elem.name);
        let is_anchored = ln == "anchor";
        if ln != "anchor" && ln != "inline" { continue; }

        // Find wsp element to get shape properties + text box content
        if let Some(wsp) = vml_find_descendant(elem, "wsp") {
            let content = find_txbx_content_in(wsp)?;
            return Some(ExtractedTextBox { content, is_anchored, frame: wsp_frame(wsp, theme) });
        }
    }
    None
}

/// The frame of a Word 2010 shape (`wps:wsp`): fill and outline from its
/// `spPr`, falling back to its `wps:style` references, and margins and
/// vertical position from its `bodyPr`.
///
/// Deviations, declared:
/// - A fill or outline that comes only from the theme's style matrix
///   (`fillRef`/`lnRef` with no `spPr` fill or line) is drawn as a solid of the
///   reference's colour, the line at the default Office theme's width for its
///   index; the theme's style lists themselves are not read yet.
/// - A gradient fill is drawn as its first stop's colour; picture and pattern
///   fills are not drawn.
/// - Only a rectangle (or no geometry, or a rounded rectangle, drawn square)
///   gets a fill and outline. Any other preset would need its real outline,
///   which Word shapes do not draw yet, so its text shows without a frame
///   rather than inside a wrong rectangle.
/// - `bodyPr` autofit is not applied: Word stores the grown size in the
///   extent when it saves.
fn wsp_frame(wsp: &ooxml_xml::RawXmlElement, theme: &ThemeFonts) -> TextBoxFrame {
    let mut frame = TextBoxFrame::default();
    let sp_pr = child(wsp, "spPr");
    let style = child(wsp, "style");
    let style_ref = |name: &str| -> Option<(u32, Option<String>)> {
        let r = child(style?, name)?;
        let idx = vml_attr(r, "idx").and_then(|v| v.parse().ok()).unwrap_or(0);
        Some((idx, dml_color(r, theme)))
    };

    let framed = match sp_pr.and_then(|sp| child(sp, "prstGeom")) {
        Some(g) => matches!(vml_attr(g, "prst"), Some("rect" | "roundRect") | None),
        None => sp_pr.and_then(|sp| child(sp, "custGeom")).is_none(),
    };

    if framed {
        // Fill: the shape's own, else the style's reference.
        let fills = ["noFill", "solidFill", "gradFill", "blipFill", "pattFill", "grpFill"];
        match sp_pr.and_then(|sp| sp.children.iter().find_map(|c| match c {
            ooxml_xml::RawXmlNode::Element(e) if fills.contains(&xml_local_name(&e.name)) => Some(e),
            _ => None,
        })) {
            Some(f) => frame.fill_color = fill_color(f, theme),
            None => {
                if let Some((idx, color)) = style_ref("fillRef") {
                    frame.fill_color = if idx == 0 { None } else { color };
                }
            }
        }

        // Outline: the shape's own `ln`, its colour and width falling back to
        // the style's line reference, else the reference alone.
        let ln_ref = style_ref("lnRef");
        match sp_pr.and_then(|sp| child(sp, "ln")) {
            Some(ln) => {
                let color = ln_ref.as_ref().filter(|r| r.0 > 0).and_then(|r| r.1.clone());
                let width = ln_ref.as_ref().map_or(12_700, |r| theme_line_width_emu(r.0));
                if let Some(line) = line_edge(ln, theme, color, width) {
                    frame.border_width_px = line.width_px;
                    frame.border_color = Some(line.color);
                }
            }
            None => {
                if let Some((idx, Some(color))) = ln_ref {
                    if idx > 0 {
                        frame.border_width_px = emu_to_px(theme_line_width_emu(idx));
                        frame.border_color = Some(color);
                    }
                }
            }
        }
    }

    // Text colour from the style's font reference, whatever the geometry.
    frame.text_color = style.and_then(|st| child(st, "fontRef")).and_then(|f| dml_color(f, theme));

    if let Some(body) = child(wsp, "bodyPr") {
        let emu = |name: &str, default: f64| {
            vml_attr(body, name).and_then(|v| v.parse::<i64>().ok()).map_or(default, emu_to_px)
        };
        let d = Insets::default_px();
        frame.insets = Insets {
            left: emu("lIns", d.left),
            top: emu("tIns", d.top),
            right: emu("rIns", d.right),
            bottom: emu("bIns", d.bottom),
        };
        // `just` and `dist` spread lines vertically; they are drawn centred.
        frame.anchor = match vml_attr(body, "anchor") {
            Some("ctr" | "just" | "dist") => TextBoxAnchor::Center,
            Some("b") => TextBoxAnchor::Bottom,
            _ => TextBoxAnchor::Top,
        };
    }
    frame
}

/// The first child element of `parent` named `name`.
fn child<'a>(parent: &'a ooxml_xml::RawXmlElement, name: &str) -> Option<&'a ooxml_xml::RawXmlElement> {
    parent.children.iter().find_map(|c| match c {
        ooxml_xml::RawXmlNode::Element(e) if xml_local_name(&e.name) == name => Some(e),
        _ => None,
    })
}

/// The line a DrawingML outline draws (`a:ln`, §20.1.2.2.24): none for
/// `noFill`; else the colour of its solid fill, or of a gradient's first
/// stop, else `color`, and its width `@w` in EMU, else `width_emu`. A line
/// with no colour draws nothing.
///
/// Deviations, declared: the spec reads an absent `@w` as 0; each caller
/// names the width drawn instead. The dash, compound line, pen alignment,
/// caps and joins are not read: every line is a solid single stroke.
fn line_edge(ln: &ooxml_xml::RawXmlElement, theme: &ThemeFonts, color: Option<String>, width_emu: i64) -> Option<Edge> {
    if child(ln, "noFill").is_some() {
        return None;
    }
    let color = child(ln, "solidFill").and_then(|f| dml_color(f, theme))
        .or_else(|| child(ln, "gradFill").and_then(|f| fill_color(f, theme)))
        .or(color)?;
    let width = vml_attr(ln, "w").and_then(|v| v.parse::<i64>().ok()).unwrap_or(width_emu);
    Some(Edge { color, width_px: emu_to_px(width), dash: None, style: LineStyle::Single })
}

/// How a picture is drawn beyond its bitmap, read from its shape properties
/// (`pic:spPr`, §20.2.2.6). One value, carried whole from the markup to the
/// paint by every picture carrier, which holds `None` for a picture that
/// sets none of it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PictureStyle {
    /// The outline (`a:ln`, §20.1.2.2.24), drawn over the picture.
    pub(crate) outline: Option<Edge>,
}

/// The style of the picture a drawing holds (`wp:inline` or `wp:anchor` →
/// `a:graphic` → `a:graphicData` → `pic:pic`, §20.4.2.8, §20.4.2.3,
/// §20.2.2.5), or `None` when it sets nothing drawn here.
///
/// Deviations, declared: a line without `@w` is drawn 9525 EMU (1px) wide,
/// not measured in Word. A line of zero width draws nothing. A picture's
/// style reference (`pic:style`) is not read, so a theme line alone draws no
/// outline. Pictures inside a group or a shape take no style.
pub(crate) fn picture_style(drawing: &ooxml_wml::types::CTDrawing, theme: &ThemeFonts) -> Option<Box<PictureStyle>> {
    let frame = drawing.extra_children.iter().find_map(|c| match &c.node {
        ooxml_xml::RawXmlNode::Element(e) if matches!(xml_local_name(&e.name), "inline" | "anchor") => Some(e),
        _ => None,
    })?;
    let sp_pr = ["graphic", "graphicData", "pic", "spPr"].iter().try_fold(frame, |e, name| child(e, name))?;
    let outline = child(sp_pr, "ln")
        .and_then(|ln| line_edge(ln, theme, None, 9_525))
        .filter(|line| line.width_px > 0.0);
    outline.is_some().then(|| Box::new(PictureStyle { outline }))
}

/// The default Office theme's line widths, by `lnRef@idx` (its `lnStyleLst`):
/// 0.5, 1 and 1.5 pt.
fn theme_line_width_emu(idx: u32) -> i64 {
    match idx {
        0 | 1 => 6_350,
        2 => 12_700,
        _ => 19_050,
    }
}

/// The colour a DrawingML fill draws (`solidFill`, or a `gradFill`'s first
/// stop), or `None` for `noFill` and fills drawn as nothing here.
fn fill_color(fill: &ooxml_xml::RawXmlElement, theme: &ThemeFonts) -> Option<String> {
    match xml_local_name(&fill.name) {
        "solidFill" => dml_color(fill, theme),
        "gradFill" => vml_find_descendant(fill, "gs").and_then(|gs| dml_color(gs, theme)),
        _ => None,
    }
}

/// The colour of the first DrawingML colour element inside `parent`
/// (§20.1.2.3), `schemeClr` through the theme, with its transforms applied in
/// order (see [`apply_color_transforms`]). A colour with alpha below 1 is
/// returned as `rgba(…)`.
pub(crate) fn dml_color(parent: &ooxml_xml::RawXmlElement, theme: &ThemeFonts) -> Option<String> {
    for c in &parent.children {
        let ooxml_xml::RawXmlNode::Element(e) = c else { continue };
        let model = xml_local_name(&e.name);
        if !DML_COLOR_MODELS.contains(&model) {
            continue;
        }
        let base = if model == "schemeClr" {
            vml_attr(e, "val")
                .and_then(|v| theme.scheme_color(v))
                .map(|hex| format!("#{}", hex.trim_start_matches('#')))
        } else {
            dml_base_color(model, |n| vml_attr(e, n).map(str::to_owned))
        }?;
        let transforms: Vec<DmlTransform> = e.children.iter().filter_map(|t| {
            let ooxml_xml::RawXmlNode::Element(t) = t else { return None };
            DmlTransform::parse(xml_local_name(&t.name), vml_attr(t, "val"))
        }).collect();
        let (hex, alpha) = apply_color_transforms(&base, &transforms);
        return Some(css_color(&hex, alpha));
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
            let textbox = vml_find_descendant(elem, "textbox");
            return Some(ExtractedTextBox {
                content,
                is_anchored: false,
                frame: TextBoxFrame {
                    fill_color: if no_fill { None } else { fill_color },
                    border_color: if no_stroke { None } else { stroke_color },
                    border_width_px: if no_stroke { 0.0 } else { 1.0 },
                    insets: textbox.and_then(|t| vml_attr(t, "inset")).map_or_else(Insets::default_px, vml_insets),
                    anchor: match vml_style_value(elem, "v-text-anchor") {
                        Some(v) if v.starts_with("middle") => TextBoxAnchor::Center,
                        Some(v) if v.starts_with("bottom") => TextBoxAnchor::Bottom,
                        _ => TextBoxAnchor::Top,
                    },
                    text_color: None,
                },
            });
        }
    }
    None
}

/// `v:textbox@inset` (Part 4 §19.1.2.22): left, top, right, bottom, separated
/// by commas or spaces; a missing value takes its default.
fn vml_insets(raw: &str) -> Insets {
    let d = Insets::default_px();
    // Commas, when present, delimit (an empty slot keeps its default);
    // otherwise whitespace does.
    let parts: Vec<&str> = if raw.contains(',') {
        raw.split(',').map(str::trim).collect()
    } else {
        raw.split_whitespace().collect()
    };
    let mut parts = parts.into_iter();
    let mut next = |default: f64| match parts.next() {
        Some(v) if !v.is_empty() => css_length_to_px(v),
        _ => default,
    };
    Insets { left: next(d.left), top: next(d.top), right: next(d.right), bottom: next(d.bottom) }
}

/// One property of a VML element's `style` attribute.
fn vml_style_value<'a>(elem: &'a ooxml_xml::RawXmlElement, key: &str) -> Option<&'a str> {
    vml_attr(elem, "style")?.split(';').find_map(|part| {
        let (k, v) = part.split_once(':')?;
        (k.trim() == key).then(|| v.trim())
    })
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

    /// §20.4.2.3: an anchor's distances apply to each side its wrap element
    /// does not set; a side the wrap element sets keeps its own.
    #[test]
    fn a_bare_wrap_element_takes_the_anchors_distances() {
        let mut d = drawing_with("wp:anchor", &[("distL", "114300"), ("distR", "114300"), ("distT", "0"), ("distB", "0")]);
        let ooxml_xml::RawXmlNode::Element(ref mut anchor) = d.extra_children[0].node else { unreachable!() };
        anchor.self_closing = false;
        anchor.children.push(RawXmlNode::Element(RawXmlElement {
            name: "wp:wrapSquare".into(),
            attributes: vec![("wrapText".into(), "bothSides".into()), ("distR".into(), "228600".into())],
            children: Vec::new(),
            self_closing: true,
        }));
        let (_, _, _, _, _, wrap) = parse_anchor_position(&d, 100.0, 100.0);
        assert_eq!((wrap.dist_l, wrap.dist_r), (12.0, 24.0), "left from the anchor, right from the wrap element");
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

#[cfg(test)]
mod text_box_frame_tests {
    use super::*;

    /// Parse one element (and its subtree) from markup.
    fn raw(xml: &str) -> ooxml_xml::RawXmlElement {
        let mut reader = quick_xml::Reader::from_str(xml);
        loop {
            match reader.read_event().expect("xml") {
                quick_xml::events::Event::Start(e) => {
                    return ooxml_xml::RawXmlElement::from_reader(&mut reader, &e).expect("element");
                }
                quick_xml::events::Event::Eof => panic!("no element"),
                _ => {}
            }
        }
    }

    fn theme() -> ThemeFonts {
        ThemeFonts::with_colors(&[("dk1", "000000"), ("lt1", "FFFFFF"), ("accent1", "4472C4")])
    }

    fn wsp(inner: &str) -> TextBoxFrame {
        wsp_frame(&raw(&format!("<wps:wsp>{inner}</wps:wsp>")), &theme())
    }

    #[test]
    fn no_body_properties_means_the_spec_defaults() {
        let f = wsp("<wps:spPr/>");
        assert_eq!(f.insets, Insets::default_px());
        assert_eq!(f.anchor, TextBoxAnchor::Top);
        assert!((f.insets.left - 9.6).abs() < 1e-9 && (f.insets.top - 4.8).abs() < 1e-9, "0.1 and 0.05 inch");
    }

    #[test]
    fn body_properties_set_margins_and_vertical_position() {
        let f = wsp(r#"<wps:bodyPr lIns="0" tIns="91440" rIns="182880" anchor="ctr"/>"#);
        assert_eq!(f.insets.left, 0.0);
        assert!((f.insets.top - 9.6).abs() < 1e-9);
        assert!((f.insets.right - 19.2).abs() < 1e-9);
        assert!((f.insets.bottom - 4.8).abs() < 1e-9, "an omitted margin keeps its default");
        assert_eq!(f.anchor, TextBoxAnchor::Center);
        assert_eq!(wsp(r#"<wps:bodyPr anchor="b"/>"#).anchor, TextBoxAnchor::Bottom);
    }

    #[test]
    fn word_text_box_colours_come_from_the_theme() {
        // Word's Insert > Text Box: a white (lt1) fill and a 0.5 pt black outline.
        let f = wsp(r#"<wps:spPr><a:prstGeom prst="rect"/><a:solidFill><a:schemeClr val="lt1"/></a:solidFill><a:ln w="6350"><a:solidFill><a:prstClr val="black"/></a:solidFill></a:ln></wps:spPr>"#);
        assert_eq!(f.fill_color.as_deref(), Some("#FFFFFF"));
        assert_eq!(f.border_color.as_deref(), Some("#000000"));
        assert!((f.border_width_px - emu_to_px(6350)).abs() < 1e-9);
        // tx1 maps to dk1; lumMod/lumOff and alpha apply.
        let tinted = wsp(r#"<wps:spPr><a:solidFill><a:schemeClr val="accent1"><a:lumMod val="20000"/><a:lumOff val="80000"/></a:schemeClr></a:solidFill></wps:spPr>"#);
        assert_ne!(tinted.fill_color.as_deref(), Some("#4472C4"), "modifiers apply");
        let clear = wsp(r#"<wps:spPr><a:solidFill><a:srgbClr val="FF0000"><a:alpha val="50000"/></a:srgbClr></a:solidFill></wps:spPr>"#);
        assert_eq!(clear.fill_color.as_deref(), Some("rgba(255,0,0,0.500)"));
    }

    /// Every colour model and transform reaches a text-box frame through the
    /// shared resolver (§20.1.2.3).
    #[test]
    fn frame_colours_use_every_colour_model() {
        let fill = |clr: &str| wsp(&format!("<wps:spPr><a:solidFill>{clr}</a:solidFill></wps:spPr>")).fill_color;
        // scRGB is linear light: 50 % is BC, not 80.
        assert_eq!(fill(r#"<a:scrgbClr r="50%" g="50%" b="50%"/>"#).as_deref(), Some("#BCBCBC"));
        assert_eq!(fill(r#"<a:hslClr hue="14400000" sat="100%" lum="50%"/>"#).as_deref(), Some("#0000FF"));
        assert_eq!(fill(r#"<a:prstClr val="cornflowerBlue"/>"#).as_deref(), Some("#6495ED"));
        assert_eq!(fill(r#"<a:sysClr val="btnFace"/>"#).as_deref(), Some("#F0F0F0"));
        assert_eq!(fill(r#"<a:srgbClr val="00FF00"><a:tint val="50000"/></a:srgbClr>"#).as_deref(), Some("#BCFFBC"));
        assert_eq!(fill(r#"<a:srgbClr val="FF0000"><a:comp/></a:srgbClr>"#).as_deref(), Some("#00FFFF"));
    }

    #[test]
    fn a_shape_without_its_own_fill_or_line_uses_its_style() {
        // Word's Insert > Shapes rectangle: accent1 fill, accent1-shaded line.
        let f = wsp(r#"<wps:spPr><a:prstGeom prst="rect"/></wps:spPr><wps:style><a:lnRef idx="2"><a:schemeClr val="accent1"><a:shade val="50000"/></a:schemeClr></a:lnRef><a:fillRef idx="1"><a:schemeClr val="accent1"/></a:fillRef><a:effectRef idx="0"/><a:fontRef idx="minor"/></wps:style>"#);
        assert_eq!(f.fill_color.as_deref(), Some("#4472C4"));
        assert!(f.border_color.is_some());
        assert!((f.border_width_px - emu_to_px(12_700)).abs() < 1e-9, "lnRef 2 is 1 pt");
        let none = wsp(r#"<wps:spPr><a:noFill/><a:ln><a:noFill/></a:ln></wps:spPr><wps:style><a:lnRef idx="2"><a:schemeClr val="accent1"/></a:lnRef><a:fillRef idx="1"><a:schemeClr val="accent1"/></a:fillRef></wps:style>"#);
        assert_eq!((none.fill_color, none.border_color), (None, None), "the shape's own noFill wins");
    }

    #[test]
    fn a_shape_that_is_not_a_rectangle_gets_no_frame() {
        let f = wsp(r#"<wps:spPr><a:prstGeom prst="ellipse"/><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill><a:ln><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln></wps:spPr><wps:bodyPr lIns="0"/>"#);
        assert_eq!((f.fill_color, f.border_color), (None, None));
        assert_eq!(f.insets.left, 0.0, "margins still apply to the text");
    }

    #[test]
    fn vml_insets_keep_defaults_for_missing_values() {
        let d = Insets::default_px();
        assert_eq!(vml_insets("0,0,0,0"), Insets { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 });
        let partial = vml_insets("32pt,35pt,,");
        assert!((partial.left - 32.0 * 4.0 / 3.0).abs() < 1e-9 && (partial.top - 35.0 * 4.0 / 3.0).abs() < 1e-9);
        assert_eq!((partial.right, partial.bottom), (d.right, d.bottom));
        let comma_space = vml_insets("20pt, 30pt");
        assert!((comma_space.top - 40.0).abs() < 1e-9, "a space after a comma is not a separator");
        let spaced = vml_insets("1mm 2mm 3mm 4mm");
        assert!((spaced.bottom - 4.0 * 96.0 / 25.4).abs() < 1e-9);
    }
}
