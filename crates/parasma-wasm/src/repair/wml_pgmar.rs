//! RULE 2 — WML `<w:pgMar>` required-attribute injection (§17.6.11).
//!
//! ECMA-376 Strict `wml.xsd` `CT_PageMar` marks all seven page-margin attrs
//! (`top right bottom left header footer gutter`) `use="required"`, and §17.6.11
//! gives NO default for `gutter` (a `use="required"` attr cannot carry a schema
//! default). Several third-party generators routinely
//! emit a `<w:pgMar>` WITHOUT `w:gutter`, so the file is genuinely schema-invalid
//! — and the strict generated parser returns `ParseError::MissingAttribute`
//! (surfaced as `Error::Invalid("gutter")`), failing the WHOLE document → blank
//! page.
//!
//! This rule is dispatch-on-FAILURE ([`load_docx_tolerant`]): the original bytes
//! are parsed first; ONLY when that fails with the missing-attribute signature do
//! we open the package, inject `="0"` for any of the 7 attrs missing on each
//! `<w:pgMar>`, rebuild the `.docx` with only `word/document.xml` rewritten, and
//! re-parse. Word's own "missing gutter = 0" is exactly this tolerance, so the
//! injected 0 matches Word's rendering. Removed once the fork codegen is lenient (with
//! the default applied at use-time so an editor round-trips absence faithfully).

use std::io::Cursor;

use ooxml_wml::Document;

use super::xml_scan::{attr_present, find_tag_end, utf8_len};
use super::zip_rewrite::rebuild_zip_replacing_part;

/// The seven `CT_PageMar` required attributes (§17.6.11), namespace-prefixed as
/// Word writes them. Any of these missing on a `<w:pgMar>` is injected as `="0"`.
const PGMAR_REQUIRED: [&str; 7] = [
    "w:top",
    "w:right",
    "w:bottom",
    "w:left",
    "w:header",
    "w:footer",
    "w:gutter",
];

/// Try to load a `.docx` from `bytes`, falling back to the pgMar repair on a
/// missing-attribute parse failure. The happy path (valid file) parses on the
/// FIRST attempt and never touches the repair. On a missing-attribute failure we
/// repair `word/document.xml`'s `<w:pgMar>` and re-parse ONCE; if the repaired
/// bytes are byte-identical to the original (no deficient pgMar found) we do NOT
/// retry, so the original error is returned unchanged (never masking an
/// unrelated parse bug).
pub(crate) fn load_docx_tolerant(
    bytes: Vec<u8>,
) -> Result<Document<Cursor<Vec<u8>>>, ooxml_wml::Error> {
    // RULE 7 — EAGER (a stack overflow is an uncatchable abort, so this one
    // cannot dispatch on failure): cap pathological table nesting BEFORE the
    // first parse. Within-cap files pass through byte-identically.
    let bytes = super::wml_nesting::cap_package_nesting(bytes);
    let first = Document::from_reader(Cursor::new(bytes.clone()));
    let err = match first {
        Ok(doc) => return Ok(doc),
        Err(e) => e,
    };

    // RULE 6 — ISO Strict packages fail FIRST at the root rels (the Strict
    // officeDocument relationship type never matches) with the
    // missing-main-relationship signature. Two-factor gate: the signature AND
    // an actual Strict URI in the package. The downgraded bytes re-enter this
    // pipeline (a Strict writer can also emit RULE-2/5 deficiencies). The
    // recursion terminates because `downgrade_strict_package` returns `None`
    // unless it changed some part: a second pass finds only the URIs the first
    // could not map, changes nothing, and falls through to the original error.
    if is_missing_main_relationship(&err) && super::wml_strict::looks_strict(&bytes) {
        if let Some(downgraded) = super::wml_strict::downgrade_strict_package(&bytes) {
            return load_docx_tolerant(downgraded);
        }
        return Err(err);
    }

    // Dispatch only on the missing-attribute class (the gutter-less `pgMar` signature). The fork
    // maps `ParseError::MissingAttribute` to `Error::Invalid(attr_name)`; other
    // error variants are real failures we must not paper over.
    if !is_missing_attr(&err) {
        return Err(err);
    }

    // Resolve + repair word/document.xml, rebuild the package, re-parse once.
    let Some(repaired) = repair_package(&bytes) else {
        return Err(err);
    };
    if repaired == bytes {
        // Nothing was deficient (the failure was elsewhere) — keep the original
        // error rather than re-parsing identical bytes.
        return Err(err);
    }
    Document::from_reader(Cursor::new(repaired))
}

/// `true` when `err` is the strict missing-required-attribute failure. The
/// fork collapses `ParseError::MissingAttribute(name)` into `Error::Invalid(name)`
/// at the wml boundary, so we match the `Invalid` variant.
fn is_missing_attr(err: &ooxml_wml::Error) -> bool {
    matches!(err, ooxml_wml::Error::Invalid(_))
}

/// `true` for the Strict-package first-failure signature: the root rels named
/// no (recognizable) main-document relationship.
fn is_missing_main_relationship(err: &ooxml_wml::Error) -> bool {
    matches!(err, ooxml_wml::Error::MissingPart(p) if p == "main document relationship")
}

/// Open the `.docx` bytes, locate `word/document.xml` (via the root rels'
/// office-document relationship, falling back to the conventional path), inject
/// the missing `<w:pgMar>` attrs, and rebuild the package with only that part
/// rewritten. `None` on any structural read error or if the document part can't
/// be found/read (caller keeps the original failure).
fn repair_package(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let doc_path = office_document_path(&mut archive)
        .unwrap_or_else(|| "word/document.xml".to_string());
    let doc_xml = read_entry(&mut archive, &doc_path)?;
    // RULE 2 (pgMar attrs) + RULE 5 (malformed-input normalization) share the
    // one rewrite pass — both are Invalid-signature classes on document.xml.
    let repaired_xml =
        super::wml_malformed::normalize_malformed_wml(&repair_pgmar_attrs(&doc_xml));
    if repaired_xml == doc_xml {
        return Some(bytes.to_vec()); // signals "nothing deficient found"
    }
    rebuild_zip_replacing_part(bytes, &[(doc_path, repaired_xml)])
}

/// Read a named entry's bytes from an open archive. `None` if absent/unreadable.
fn read_entry(archive: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<Vec<u8>> {
    let mut entry = archive.by_name(name).ok()?;
    let declared = entry.size();
    super::zip_rewrite::read_entry_bytes(&mut entry, declared)
}

/// Resolve the office-document part path from `_rels/.rels` by a byte scan for the
/// `<Relationship>` whose `Type` ends in `/officeDocument`, returning its
/// `Target` (leading `/` stripped to a package-relative path). `None` if the rels
/// part is missing or the relationship can't be found.
fn office_document_path(archive: &mut zip::ZipArchive<Cursor<&[u8]>>) -> Option<String> {
    let rels = read_entry(archive, "_rels/.rels")?;
    let text = String::from_utf8_lossy(&rels);
    // Walk each <Relationship …> start tag; return the Target of the one whose
    // Type ends with "/officeDocument".
    let mut rest = text.as_ref();
    while let Some(p) = rest.find("<Relationship") {
        let tag_end = rest[p..].find('>').map(|e| p + e + 1).unwrap_or(rest.len());
        let tag = &rest[p..tag_end];
        let ty = scan_attr(tag, "Type");
        if ty.map(|t| t.ends_with("/officeDocument")).unwrap_or(false) {
            let target = scan_attr(tag, "Target")?;
            return Some(target.trim_start_matches('/').to_string());
        }
        rest = &rest[tag_end.min(rest.len())..];
    }
    None
}

/// Read a double-quoted attribute value `name="…"` out of a single start tag.
fn scan_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}=\"");
    let i = tag.find(&key)? + key.len();
    let j = tag[i..].find('"')? + i;
    Some(&tag[i..j])
}

/// Scan every `<w:pgMar …>` start tag and inject any of the seven required
/// `CT_PageMar` attrs that are missing, each as `="0"` (§17.6.11; Word's own
/// missing-attr tolerance). Everything else is byte-preserved; only deficient
/// `<w:pgMar>` tags are touched, only the missing attribute(s) added. Non-UTF8
/// input is returned untouched. Idempotent: a fully-specified `<w:pgMar>` (or a
/// document with none) is returned unchanged.
pub(crate) fn repair_pgmar_attrs(xml: &[u8]) -> Vec<u8> {
    let s = match std::str::from_utf8(xml) {
        Ok(s) => s,
        Err(_) => return xml.to_vec(),
    };
    let mut out = String::with_capacity(s.len() + 64);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' && starts_with_pgmar_tag(&bytes[i..]) {
            let Some(end_rel) = find_tag_end(&bytes[i..]) else {
                out.push_str(&s[i..]); // unterminated: `find_tag_end`'s contract
                break;
            };
            let tag = &s[i..i + end_rel + 1];
            out.push_str(&repair_one_pgmar(tag));
            i += end_rel + 1;
            continue;
        }
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    out.into_bytes()
}

/// True if `b` begins `<w:pgMar` as an element start (next char is space, `/`, or
/// `>`).
fn starts_with_pgmar_tag(b: &[u8]) -> bool {
    const TAG: &[u8] = b"w:pgMar";
    if b.len() < TAG.len() + 2 || &b[1..1 + TAG.len()] != TAG {
        return false;
    }
    matches!(b[1 + TAG.len()], b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>')
}

/// Inject any missing required attr (as `="0"`) into a single `<w:pgMar …>` tag,
/// right after `<w:pgMar`. Returns the (possibly rewritten) tag.
fn repair_one_pgmar(tag: &str) -> String {
    let mut injected = String::new();
    for attr in PGMAR_REQUIRED {
        if !attr_present(tag, attr) {
            injected.push_str(&format!(" {attr}=\"0\""));
        }
    }
    if injected.is_empty() {
        return tag.to_string();
    }
    let insert_at = "<w:pgMar".len();
    let mut rewritten = String::with_capacity(tag.len() + injected.len());
    rewritten.push_str(&tag[..insert_at]);
    rewritten.push_str(&injected);
    rewritten.push_str(&tag[insert_at..]);
    rewritten
}
