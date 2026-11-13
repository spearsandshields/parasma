//! RULE 6 — ISO Strict OOXML → Transitional namespace downgrade (translation
//! shim).
//!
//! ECMA-376 defines TWO conformance classes with identical structure but
//! different namespace URI stems: Transitional (what Word writes,
//! `http://schemas.openxmlformats.org/{family}/2006/…`) and Strict
//! (`http://purl.oclc.org/ooxml/{family}/…`). The parser fork matches literal
//! prefixed names plus TRANSITIONAL relationship-type URIs, so a Strict
//! package fails at the very first step — the root rels' officeDocument
//! relationship type never matches → `MissingPart("main document
//! relationship")`.
//!
//! Unlike RULE 5 (malformed input), Strict files are FULLY LEGAL — the root
//! fix would be namespace-aware parsing in the fork, which is deliberately
//! deferred (an aggressive, cross-cutting parser change). Until then this rule
//! translates on import: rewrite every
//! `.xml`/`.rels` part's Strict URI stems to their Transitional equivalents —
//! the ECMA mapping is mechanical:
//!
//!   `http://purl.oclc.org/ooxml/{family}/{rest}`
//!     → `http://schemas.openxmlformats.org/{family}/2006/{rest}`
//!
//! Corpus proof (2026-07-21): all four Strict files (strict, strict-smartart,
//! strict-lockedcanvas, tdf116410 — SmartArt and locked-canvas content
//! included) parse and render after this transform alone; element/attribute
//! names are identical across the classes, and the generated parsers match by
//! prefix, not resolved URI.
//!
//! Dispatch is on-FAILURE with a two-factor gate (the repair never-mask rule):
//! the original bytes must have failed with the missing-main-relationship
//! signature AND the package must actually contain a Strict URI. Valid
//! transitional files are never rewritten; a Strict file whose downgraded form
//! still fails re-enters the RULE-2/5 pipeline (a Strict writer can also emit
//! the malformed shapes those rules fix). The re-entry terminates because
//! `downgrade_strict_package` reports a rewrite only when some part changed.
//!
//! RETIREMENT CONDITION: if the fork gains native Strict-namespace support
//! (both rel types and body namespaces), this rule can be deleted — pin
//! coverage lives in the repair test battery's strict-package tests.

use std::io::{Cursor, Read};

/// The one mechanical URI mapping. Applied to textual parts only.
fn downgrade_strict_uris(text: &str) -> String {
    const NEEDLE: &str = "http://purl.oclc.org/ooxml/";
    const TRANSITIONAL: &str = "http://schemas.openxmlformats.org/";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(p) = rest.find(NEEDLE) {
        out.push_str(&rest[..p]);
        let tail = &rest[p + NEEDLE.len()..];
        // {family}/  → {family}/2006/
        if let Some(slash) = tail.find('/') {
            let family = &tail[..slash];
            out.push_str(TRANSITIONAL);
            out.push_str(family);
            out.push_str("/2006/");
            rest = &tail[slash + 1..];
        } else {
            // A bare stem with no family segment — leave untouched.
            out.push_str(NEEDLE);
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// `true` when the package plausibly IS a Strict package: its root rels carry a
/// Strict URI. Cheap single-entry read; `false` on any structural problem
/// (the caller then keeps the original error).
pub(crate) fn looks_strict(bytes: &[u8]) -> bool {
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return false;
    };
    let Ok(mut entry) = archive.by_name("_rels/.rels") else {
        return false;
    };
    let mut buf = String::new();
    if entry.read_to_string(&mut buf).is_err() {
        return false;
    }
    buf.contains("http://purl.oclc.org/ooxml/")
}

/// Rewrite every textual part's Strict URIs to Transitional, rebuilding the
/// package. `None` on any structural failure, and `None` when no part changed
/// (caller keeps the original error either way).
pub(crate) fn downgrade_strict_package(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let names: Vec<String> = archive.file_names().filter_map(|n| n.ok().map(String::from)).collect();
    let mut replacements: Vec<(String, Vec<u8>)> = Vec::new();
    for name in names {
        if !(name.ends_with(".xml") || name.ends_with(".rels")) {
            continue;
        }
        let mut entry = archive.by_name(&name).ok()?;
        let declared = entry.size();
        let data = super::zip_rewrite::read_entry_bytes(&mut entry, declared)?;
        drop(entry);
        let Ok(text) = std::str::from_utf8(&data) else {
            continue; // non-UTF8 "xml" — leave untouched
        };
        if text.contains("http://purl.oclc.org/ooxml/") {
            // Only a part the rewrite actually CHANGED counts. A Strict-looking
            // URI with no family segment is left as-is by `downgrade_strict_uris`;
            // counting it would hand the caller a package that still looks
            // Strict, and the caller would downgrade it again without end.
            let downgraded = downgrade_strict_uris(text);
            if downgraded != text {
                replacements.push((name, downgraded.into_bytes()));
            }
        }
    }
    if replacements.is_empty() {
        return None; // nothing was rewritten — keep the original error
    }
    super::zip_rewrite::rebuild_zip_replacing_part(bytes, &replacements)
}

#[cfg(test)]
mod tests {
    use super::downgrade_strict_uris;

    #[test]
    fn maps_each_family_with_2006_inserted() {
        assert_eq!(
            downgrade_strict_uris("http://purl.oclc.org/ooxml/wordprocessingml/main"),
            "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
        );
        assert_eq!(
            downgrade_strict_uris(
                "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument"
            ),
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
        );
        assert_eq!(
            downgrade_strict_uris("http://purl.oclc.org/ooxml/drawingml/wordprocessingDrawing"),
            "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
        );
    }

    #[test]
    fn transitional_and_foreign_uris_untouched() {
        let s = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:v="urn:schemas-microsoft-com:vml"/>"#;
        assert_eq!(downgrade_strict_uris(s), s);
    }

    #[test]
    fn idempotent() {
        let strict = r#"<Relationship Type="http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument"/>"#;
        let once = downgrade_strict_uris(strict);
        assert_eq!(downgrade_strict_uris(&once), once);
    }
}
