//! Raw-XML / VML traversal utilities (format-agnostic): local-name lookup,
//! entity decoding, attribute/descendant search, opacity and shape-box CSS
//! parsing. VML occurs in WML headers (watermarks, legacy text boxes) AND in
//! SpreadsheetML legacy comments, so these are engine-level. Moved
//! verbatim from `lib.rs`. `xml_local_name` is a generic XML util that
//! lives here until a dedicated xml module is warranted.
//!
//! Spec grounding: VML is ECMA-376 Part 4 (Transitional), the VML annex. The
//! VML-specific fixed-point encodings parsed here follow that annex: `f`
//! opacity is 16.16 fixed-point (value / 65536) and `fd` rotation is in
//! 1/60000 of a degree.

use super::units::css_length_to_px;

pub(crate) fn xml_local_name(s: &str) -> &str {
    s.rsplit_once(':').map(|(_, l)| l).unwrap_or(s)
}

/// Decode the FIVE predefined XML entities that survive in raw (unparsed)
/// attribute values; numeric character references stay verbatim. Critical for
/// VML CSS `style`: Word writes `font-family:&quot;Calibri&quot;`, and
/// `&quot;` *ends in a semicolon* — splitting the style on `;` before decoding
/// shreds the entity (and yields an invalid font string). Single-pass, so an
/// already-decoded `&` is never re-expanded (`&amp;quot;` → `&quot;`) — the
/// property the retired replace-chain spelling got from ordering `&amp;` last
/// (equivalence pinned by `unified_decoder_matches_the_retired_replace_chain`).
pub(crate) fn decode_xml_entities(s: &str) -> String {
    decode(s, false, false)
}

/// Decode the five named entities PLUS decimal/hex numeric character
/// references (`&#8226;` / `&#x2022;`). A malformed/unknown reference emits
/// the `&` and rescans from the next char (`a&unknown;b` stays verbatim).
/// Used where the fork hands attribute text through raw and the value may
/// carry char refs (PML bullet chars).
#[cfg_attr(not(any(feature = "pml", test)), allow(dead_code))]
pub(crate) fn decode_char_refs(s: &str) -> String {
    decode(s, true, false)
}

/// Like [`decode_char_refs`] but a malformed/unknown `&…;` reference is kept
/// VERBATIM through its `;` (never re-examined for nested entities). The WML
/// numbering `lvlText` posture — pinned by
/// `wml::numbering::tests::decodes_numeric_and_named_lvl_text_entities`.
///
/// The two malformed-input recoveries differ only on pathological nesting
/// (`&&amp;`: rescan → `&&`, verbatim → `&&amp;`). Both persist as the merge was
/// byte-preserving; collapsing them is a separate, declared behavior change.
pub(crate) fn decode_char_refs_verbatim(s: &str) -> String {
    decode(s, true, true)
}

/// The ONE entity-decode spelling (it replaces the four divergent copies in
/// engine/vml, sml/model, wml/numbering and pml/shape). `numeric` arms
/// `&#NN;`/`&#xHH;` decoding; `keep_malformed` selects the unknown-reference
/// recovery (see [`decode_char_refs_verbatim`]).
fn decode(s: &str, numeric: bool, keep_malformed: bool) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let decoded = after.find(';').and_then(|semi| {
            let entity = &after[1..semi];
            let c = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if numeric => entity
                    .strip_prefix('#')
                    .and_then(|num| {
                        if let Some(hex) = num.strip_prefix(['x', 'X']) {
                            u32::from_str_radix(hex, 16).ok()
                        } else {
                            num.parse::<u32>().ok()
                        }
                    })
                    .and_then(char::from_u32),
                _ => None,
            };
            c.map(|c| (c, semi))
        });
        match (decoded, keep_malformed, after.find(';')) {
            (Some((c, semi)), _, _) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            // Malformed with a closing `;` in reach: keep the whole span.
            (None, true, Some(semi)) => {
                out.push_str(&after[..=semi]);
                rest = &after[semi + 1..];
            }
            // Malformed: emit the `&` and rescan from the next char.
            (None, ..) => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// First attribute (by local name) on a raw VML element.
pub(crate) fn vml_attr<'a>(elem: &'a ooxml_xml::RawXmlElement, name: &str) -> Option<&'a str> {
    elem.attributes
        .iter()
        .find(|(a, _)| xml_local_name(a) == name)
        .map(|(_, v)| v.as_str())
}

/// First descendant element (depth-first, self included) with the given local name.
pub(crate) fn vml_find_descendant<'a>(
    elem: &'a ooxml_xml::RawXmlElement,
    local: &str,
) -> Option<&'a ooxml_xml::RawXmlElement> {
    if xml_local_name(&elem.name) == local {
        return Some(elem);
    }
    for child in &elem.children {
        if let ooxml_xml::RawXmlNode::Element(ref e) = child {
            if let Some(found) = vml_find_descendant(e, local) {
                return Some(found);
            }
        }
    }
    None
}

/// VML opacity attribute → 0..=1. Word writes either a fraction (`".5"`) or a
/// 16.16 fixed-point value out of 65536 (`"32768f"`).
pub(crate) fn parse_vml_opacity(s: &str) -> f64 {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('f') {
        return (num.trim().parse::<f64>().unwrap_or(65536.0) / 65536.0).clamp(0.0, 1.0);
    }
    s.parse::<f64>().unwrap_or(1.0).clamp(0.0, 1.0)
}

/// Parse a VML shape's CSS `style` → (width_px, height_px, rotation_deg).
pub(crate) fn vml_shape_box(shape: &ooxml_xml::RawXmlElement) -> (f64, f64, f64) {
    let (mut w, mut h, mut rot) = (0.0_f64, 0.0_f64, 0.0_f64);
    for part in decode_xml_entities(vml_attr(shape, "style").unwrap_or("")).split(';') {
        if let Some((k, v)) = part.split_once(':') {
            match k.trim() {
                "width" => w = css_length_to_px(v.trim()),
                "height" => h = css_length_to_px(v.trim()),
                "rotation" => rot = v.trim().trim_end_matches("fd").parse::<f64>().unwrap_or(0.0),
                _ => {}
            }
        }
    }
    (w, h, rot)
}

#[cfg(test)]
mod watermark_tests {
    use super::{decode_xml_entities, parse_vml_opacity};
    use crate::engine::color::vml_color_to_css;

    #[test]
    fn entity_decode_unbreaks_font_family_split() {
        // `&quot;` ends in ';' — decoding MUST precede splitting the CSS on ';',
        // else the font name is shredded (an earlier bug that distorted the watermark).
        let s = decode_xml_entities("font-family:&quot;Calibri&quot;;font-size:1pt");
        assert_eq!(s, "font-family:\"Calibri\";font-size:1pt");
        let parts: Vec<&str> = s.split(';').collect();
        assert_eq!(parts.len(), 2);
        let fam = parts[0].split_once(':').unwrap().1.trim_matches('"');
        assert_eq!(fam, "Calibri");
    }

    /// Pin: `decode_xml_entities` was a five-step `replace` chain
    /// (`&amp;` last); it is now a wrapper over the ONE single-pass decoder.
    /// This proves the two spellings agree, including on adversarial inputs —
    /// double-encoding, nested/dangling ampersands, and numeric refs (which
    /// the named-5 decoder must leave VERBATIM).
    #[test]
    fn unified_decoder_matches_the_retired_replace_chain() {
        fn chain(s: &str) -> String {
            s.replace("&quot;", "\"")
                .replace("&apos;", "'")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&")
        }
        for s in [
            "font-family:&quot;Calibri&quot;;font-size:1pt",
            "&amp;quot;", // double-encoded: must stay &quot;
            "&amp;amp;",
            "&&amp;",
            "&lt&gt;",
            "&quot", // no terminating semicolon
            "x&y",
            "a&amp;&lt;b",
            "&apos;&gt;",
            "&quot;&", // trailing bare ampersand
            "&am&lt;p;",
            "&#8226;", // numeric ref: verbatim in the named-5 decoder
            "&unknown;",
            "&;",
            "",
        ] {
            assert_eq!(decode_xml_entities(s), chain(s), "input {s:?}");
        }
    }

    #[test]
    fn vml_color_hex_named_passthrough() {
        assert_eq!(vml_color_to_css("#C00000"), "#C00000");
        assert_eq!(vml_color_to_css("C0C0C0"), "#C0C0C0");
        assert_eq!(vml_color_to_css("silver"), "silver"); // CSS keyword
    }

    #[test]
    fn vml_opacity_fraction_and_fixed_point() {
        assert!((parse_vml_opacity(".5") - 0.5).abs() < 1e-9);
        assert!((parse_vml_opacity("32768f") - 0.5).abs() < 1e-3); // 16.16 fixed
        assert_eq!(parse_vml_opacity("1"), 1.0);
    }
}

