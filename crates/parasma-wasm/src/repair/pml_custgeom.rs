//! RULE 3 — PML `<a:custGeom>` strip (the custom-geometry `moveTo`/`lnTo` x-attr
//! parse failure).
//!
//! ## The failure (diagnosed, not guessed)
//!
//! A slide custom-geometry path uses the spec-correct, schema-VALID markup
//! `<a:moveTo><a:pt x="…" y="…"/></a:moveTo>` — the coordinates live on the CHILD
//! `<a:pt>`, and `<a:moveTo>` itself is an attribute-less path-command wrapper.
//! But the fork's generated `CTPath2D` parser hands the `<a:moveTo>` (and
//! `<a:lnTo>`) START TAG directly to `CTAdjPoint2D::from_xml`, which then looks
//! for x/y as ATTRIBUTES on the wrapper (there are none) and errors
//! `missing attribute: x`. Root cause: the schema models
//! `Path2DMoveToElement = Box<CTAdjPoint2D>` (a type alias that elides the
//! wrapper element), so codegen treats `moveTo` AS the point instead of
//! descending into its child `<a:pt>`. This is a FORK/CODEGEN bug, NOT a
//! malformed file — so "inject x" would be a no-op (every `<a:pt>` already
//! carries x). The correct parasma-side repair is to STRIP the unsupported
//! subtree.
//!
//! ## The repair
//!
//! On a missing-attribute parse failure ([`load_pptx_tolerant`]), byte-replace
//! every `<a:custGeom>…</a:custGeom>` in each slide part with a
//! `<a:prstGeom prst="rect"><a:avLst/></a:prstGeom>` placeholder, then re-parse.
//! Nothing rendered is lost: the PML loader raw-scans each slide's custom
//! geometry from the ORIGINAL pre-repair bytes (see `engine/opc.rs` and
//! `pml/mod.rs`), so the placeholder only keeps the fork parse alive and the
//! real path still paints. (Diagnosis: instrumenting the fork's
//! `MissingAttribute("x")` site prints
//! `x@CTAdjPoint2D[elem=a:moveTo attrs=[]]`.) The fix: correct the type-alias elision in codegen so
//! the parser descends into the child `<a:pt>`).

use std::io::Cursor;

use wasm_bindgen::JsValue;

use super::zip_rewrite::rebuild_zip_replacing_part;

/// Placeholder geometry substituted for each stripped `<a:custGeom>`: an
/// attribute-less rectangle preset, which every fork `prstGeom` parser accepts.
const PRST_RECT: &str = r#"<a:prstGeom prst="rect"><a:avLst/></a:prstGeom>"#;

/// Try to repair `.pptx` bytes for the custGeom parse failure, returning the
/// repaired package bytes ONLY when a strip actually changed at least one slide
/// part. `None` means "no custGeom found / nothing to repair" (the caller then
/// keeps the original parse error — never masking an unrelated failure). The
/// dispatch on WHETHER to call this lives in the loader (it is only invoked after
/// the original-bytes parse failed with the missing-attribute signature).
pub(crate) fn load_pptx_tolerant(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;

    // Collect the slide-part entry names (ppt/slides/slideN.xml — NOT the
    // slideLayouts/ or slideMasters/ siblings, which are read separately and do
    // not drive the slides() parse failure).
    let slide_names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().and_then(|e| e.name().ok().map(|n| n.into_owned())))
        .filter(|n| is_slide_part(n))
        .collect();

    let mut replacements: Vec<(String, Vec<u8>)> = Vec::new();
    for name in slide_names {
        let Some(xml) = read_entry(&mut archive, &name) else {
            continue;
        };
        let stripped = strip_custgeom(&xml);
        if stripped != xml {
            replacements.push((name, stripped));
        }
    }

    if replacements.is_empty() {
        return None; // no custGeom present → not our failure to repair
    }
    rebuild_zip_replacing_part(bytes, &replacements)
}

/// Build the JsValue error a PML loader surfaces when even the repaired bytes
/// fail (so the loader can report honestly). Kept here so the loader's error
/// string and the repair stay co-located.
pub(crate) fn repaired_parse_error(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&format!("pptx parse failed (after custGeom repair): {e}"))
}

/// True for a main slide part `ppt/slides/slideN.xml` (excludes
/// `ppt/slides/_rels/…` and the layout/master parts).
fn is_slide_part(name: &str) -> bool {
    name.starts_with("ppt/slides/slide")
        && name.ends_with(".xml")
        && !name.contains("/_rels/")
}

/// Read a named entry's bytes. `None` if absent/unreadable.
fn read_entry(archive: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<Vec<u8>> {
    let mut entry = archive.by_name(name).ok()?;
    let declared = entry.size();
    super::zip_rewrite::read_entry_bytes(&mut entry, declared)
}

/// Replace every `<a:custGeom>…</a:custGeom>` element span with the rectangle
/// placeholder, byte-preserving everything else. Handles both the paired form and
/// the (rare) self-closing `<a:custGeom/>`. Operates over the full element span
/// (numeric coordinates need no entity decoding). Idempotent / non-UTF8-safe:
/// input with no `custGeom` (or non-UTF8 input) is returned unchanged.
pub(crate) fn strip_custgeom(xml: &[u8]) -> Vec<u8> {
    let s = match std::str::from_utf8(xml) {
        Ok(s) => s,
        Err(_) => return xml.to_vec(),
    };
    if !s.contains("custGeom") {
        return xml.to_vec();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("<a:custGeom") {
        // Copy everything before the custGeom element verbatim.
        out.push_str(&rest[..open]);
        let after_open = &rest[open..];
        // Determine the start-tag end and whether it is self-closing.
        let Some(gt) = after_open.find('>') else {
            // Unterminated start tag — copy the remainder and stop.
            out.push_str(after_open);
            rest = "";
            break;
        };
        let start_tag = &after_open[..=gt];
        out.push_str(PRST_RECT);
        if start_tag.ends_with("/>") {
            // Self-closing custGeom — consume just the start tag.
            rest = &after_open[gt + 1..];
        } else if let Some(close) = after_open.find("</a:custGeom>") {
            // Paired — consume through the matching close tag.
            rest = &after_open[close + "</a:custGeom>".len()..];
        } else {
            // Open with no close — consume the start tag, copy the rest.
            rest = &after_open[gt + 1..];
        }
    }
    out.push_str(rest);
    out.into_bytes()
}
