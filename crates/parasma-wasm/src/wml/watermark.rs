//! Watermarks — VML WordArt text (`v:textpath`) and image (`v:imagedata`)
//! shapes in a header, resolved to a drawable `Watermark` at load time and
//! painted page-centered behind content in `render_page` (which stays in
//! lib.rs until the paint relocation). Verbatim relocation from `lib.rs`.
//! Deliberately limited to WordArt text and image shapes; other VML is ignored.
//!
//! Spec grounding: VML is ECMA-376 Part 4 (Transitional), the VML annex —
//! `v:shape` / `v:textpath` (WordArt path, the `t136` preset) / `v:imagedata`
//! (image fill) / `v:fill` washout. Part 4 is a separate volume from the §17
//! WordprocessingML body extracts; a header's watermark is a legacy VML shape
//! anchored via `w:pict` (§17.3.3.9).

use std::collections::HashSet;

use ooxml_wml::ext::ParagraphExt;

use crate::engine::color::vml_color_to_css;
use crate::engine::vml::{
    decode_xml_entities, parse_vml_opacity, vml_attr, vml_find_descendant, vml_shape_box,
    xml_local_name,
};

/// A resolved document watermark — a VML `<v:shape>` in a header, either WordArt
/// text or an image. Rendered page-centered, rotated, behind body content.
pub(crate) enum Watermark {
    Text(TextWatermark),
    Image(ImageWatermark),
}

/// Word's diagonal "DRAFT"/"CONFIDENTIAL" WordArt (`<v:shape type="#_x0000_t136">`
/// with a `<v:textpath>`). The glyphs are stretched to fill the WordArt box (the
/// t136 "plain text" behaviour).
pub(crate) struct TextWatermark {
    pub(crate) text: String,
    pub(crate) font_family: String,
    /// Resolved CSS colour (hex or VML named colour → hex), default Word silver.
    pub(crate) color: String,
    /// Fill opacity 0..=1 (from `<v:fill opacity>`; default 1 — silver is faint).
    pub(crate) opacity: f64,
    /// Clockwise rotation in degrees (VML style `rotation`; 315 = the standard
    /// uphill diagonal).
    pub(crate) rotation_deg: f64,
    /// WordArt box the glyphs are scaled to fill, in px.
    pub(crate) width_px: f64,
    pub(crate) height_px: f64,
}

/// An image watermark (`<v:shape>` with `<v:imagedata r:id>`). `image_key` is the
/// namespaced `{part}::{relId}` into the decoded-bitmap map. `washout` reflects
/// the VML `gain`/`blacklevel` brightening Word applies — approximated by drawing
/// at reduced opacity.
pub(crate) struct ImageWatermark {
    pub(crate) image_key: String,
    pub(crate) rotation_deg: f64,
    pub(crate) width_px: f64,
    pub(crate) height_px: f64,
    pub(crate) washout: bool,
}

/// Resolve a watermark from a `<w:pict>` — a WordArt textpath, else an image.
/// `part_paths`/`image_keys` resolve a `<v:imagedata r:id>` to its namespaced
/// decoded-bitmap key (`{part}::{relId}`).
pub(crate) fn extract_watermark(
    pict: &ooxml_wml::types::CTPicture,
    part_paths: &[String],
    image_keys: &HashSet<String>,
) -> Option<Watermark> {
    for child in &pict.extra_children {
        let ooxml_xml::RawXmlNode::Element(ref shape) = child.node else { continue };
        if xml_local_name(&shape.name) != "shape" {
            continue;
        }
        let (width_px, height_px, rotation_deg) = vml_shape_box(shape);

        // ── Text WordArt ──────────────────────────────────────────────────
        if let Some(textpath) = vml_find_descendant(shape, "textpath") {
            let text = decode_xml_entities(vml_attr(textpath, "string").unwrap_or("").trim());
            if !text.is_empty() {
                let color = vml_attr(shape, "fillcolor")
                    .map(vml_color_to_css)
                    .unwrap_or_else(|| "#C0C0C0".to_string());
                let opacity = vml_find_descendant(shape, "fill")
                    .and_then(|f| vml_attr(f, "opacity"))
                    .map(parse_vml_opacity)
                    .unwrap_or(1.0);
                let mut font_family = "sans-serif".to_string();
                for part in decode_xml_entities(vml_attr(textpath, "style").unwrap_or("")).split(';')
                {
                    if let Some((k, v)) = part.split_once(':') {
                        if k.trim() == "font-family" {
                            let fam = v.trim().trim_matches('"').trim_matches('\'').trim();
                            if !fam.is_empty() {
                                font_family = fam.to_string();
                            }
                        }
                    }
                }
                return Some(Watermark::Text(TextWatermark {
                    text,
                    font_family,
                    color,
                    opacity,
                    rotation_deg,
                    width_px: width_px.max(1.0),
                    height_px: height_px.max(1.0),
                }));
            }
        }

        // ── Image ─────────────────────────────────────────────────────────
        if let Some(imagedata) = vml_find_descendant(shape, "imagedata") {
            if let Some(rel_id) = vml_attr(imagedata, "id") {
                // Resolve the local rId against the header part(s) it could live in.
                if let Some(key) = part_paths
                    .iter()
                    .map(|p| format!("{p}::{rel_id}"))
                    .find(|k| image_keys.contains(k))
                {
                    // Washout = Word's brightened/low-contrast watermark fill.
                    let washout = vml_attr(imagedata, "gain").is_some()
                        || vml_attr(imagedata, "blacklevel").is_some();
                    return Some(Watermark::Image(ImageWatermark {
                        image_key: key,
                        rotation_deg,
                        width_px: width_px.max(1.0),
                        height_px: height_px.max(1.0),
                        washout,
                    }));
                }
            }
        }
    }
    None
}

/// Scan top-level header paragraphs for the first watermark.
pub(crate) fn find_watermark_in_blocks(
    blocks: &[ooxml_wml::types::BlockContent],
    part_paths: &[String],
    image_keys: &HashSet<String>,
) -> Option<Watermark> {
    for b in blocks {
        if let ooxml_wml::types::BlockContent::P(p) = b {
            for run in p.runs() {
                for rc in &run.run_content {
                    if let ooxml_wml::types::RunContent::Pict(pict) = rc {
                        if let Some(wm) = extract_watermark(pict, part_paths, image_keys) {
                            return Some(wm);
                        }
                    }
                }
            }
        }
    }
    None
}

