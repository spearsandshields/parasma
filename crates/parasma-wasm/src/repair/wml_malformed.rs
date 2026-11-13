//! RULE 5 — WML malformed-input normalization (Word-tolerance for schema-invalid
//! files real writers produce).
//!
//! Unlike the retired RULE 4 (which worked around a PARSER BUG on legal input,
//! and died when the fork fixed the root), these five normalizations handle
//! input that is GENUINELY schema-invalid — there is no upstream "durable fix"
//! to wait for, because the defect is in the writers (interop bugs in
//! assorted exporters). The parser fork stays a strict schema mirror on
//! purpose; "open what Word opens" is VIEWER policy, so the tolerance lives
//! here, exactly as Word itself normalizes broken files on load.
//!
//! Corpus provenance (1122-file POI/LibreOffice sweep, 2026-07-20) — each
//! normalization maps to observed files Word opens and we rejected:
//!
//!  1. `<w:tbl>` missing its required `<w:tblPr>` child (§17.4.38; CT_Tbl
//!     sequence) → inject `<w:tblPr/>` (Word applies default table props).
//!     [fdo53985, tdf108714, tdf111550]
//!  2. `<w:tbl>` missing its required `<w:tblGrid>` child (§17.4.48) → inject
//!     `<w:tblGrid/>` before the first row (Word infers the grid from cells;
//!     the engine's table-border code tolerates an empty grid).
//!     [tdf131203, tdf141969]
//!  3. `<w:moveFromRangeStart>`/`<w:moveToRangeStart>` missing the required
//!     `w:date` (§17.13.5.{16,26}; CT_MoveBookmark) → inject a placeholder
//!     epoch date (revision metadata the viewer never renders).
//!     [tdf104797, Tika-792]
//!  4. `<w:pPrChange>` missing its required `<w:pPr>` child (§17.13.5.29) →
//!     inject `<w:pPr/>` (an empty prior-properties record).
//!     [UnknownStyleInRedline]
//!  5. `w:type="odd"` on `<w:headerReference>`/`<w:footerReference>` — not a
//!     legal `ST_HdrFtr` token (§17.10.{5,2}: default|even|first) → rewrite to
//!     `"default"` (Word's odd-page mapping). [60293]
//!
//! Dispatch stays the RULE-2 shape (see `load_docx_tolerant`): the ORIGINAL
//! bytes parse first; only on the strict `Error::Invalid` signature do these
//! normalizers run, and a byte-identical result short-circuits to the original
//! error (valid files are never rewritten, unrelated failures never masked).
//! Every pass is idempotent and byte-preserving outside the deficient spans.

use super::xml_scan::{attr_present, find_tag_end, utf8_len};

/// Placeholder for a required-but-absent revision timestamp. Deliberately the
/// epoch: obviously synthetic to anyone inspecting, never rendered by us.
const EPOCH_DATE: &str = "1970-01-01T00:00:00Z";

/// Run all five normalizations over `word/document.xml` bytes. Returns the
/// input unchanged (same bytes) when nothing is deficient; non-UTF8 input is
/// returned untouched (same posture as RULE 2).
pub(crate) fn normalize_malformed_wml(xml: &[u8]) -> Vec<u8> {
    let s = match std::str::from_utf8(xml) {
        Ok(s) => s,
        Err(_) => return xml.to_vec(),
    };
    let s = inject_missing_move_bookmark_dates(s);
    let s = inject_missing_ppr_in_pprchange(&s);
    let s = fix_hdrftr_type_odd(&s);
    let s = inject_missing_table_children(&s);
    s.into_bytes()
}

/// Pass 3 — `w:date` injection on the two CT_MoveBookmark start tags.
fn inject_missing_move_bookmark_dates(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 64);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<'
            && (starts_with_tag(&bytes[i..], b"w:moveFromRangeStart")
                || starts_with_tag(&bytes[i..], b"w:moveToRangeStart"))
        {
            let Some(end_rel) = find_tag_end(&bytes[i..]) else {
                out.push_str(&s[i..]); // unterminated: `find_tag_end`'s contract
                break;
            };
            let tag = &s[i..i + end_rel + 1];
            if !attr_present(tag, "w:date") {
                // Insert before the tag's closing `>` or `/>`.
                let insert_at = tag.rfind("/>").unwrap_or(tag.len() - 1);
                out.push_str(&tag[..insert_at]);
                out.push_str(&format!(" w:date=\"{EPOCH_DATE}\""));
                out.push_str(&tag[insert_at..]);
            } else {
                out.push_str(tag);
            }
            i += end_rel + 1;
            continue;
        }
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Pass 4 — an empty `<w:pPrChange …></w:pPrChange>` (or self-closing) gains
/// its required empty `<w:pPr/>` child. A pPrChange that already contains a
/// `<w:pPr` is untouched.
fn inject_missing_ppr_in_pprchange(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 32);
    let mut rest = s;
    loop {
        let Some(p) = rest.find("<w:pPrChange") else {
            out.push_str(rest);
            return out;
        };
        // Guard against name-prefix collisions (none exist in WML, but stay
        // surgical): next char must terminate the name.
        let after = rest.as_bytes().get(p + "<w:pPrChange".len());
        if !matches!(after, Some(b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>')) {
            out.push_str(&rest[..p + "<w:pPrChange".len()]);
            rest = &rest[p + "<w:pPrChange".len()..];
            continue;
        }
        let Some(tag_end) = find_tag_end(&rest.as_bytes()[p..]) else {
            out.push_str(rest);
            return out;
        };
        let tag = &rest[p..p + tag_end + 1];
        if let Some(open) = tag.strip_suffix("/>") {
            // Self-closing: expand with the required child.
            out.push_str(&rest[..p]);
            out.push_str(open);
            out.push_str("><w:pPr/></w:pPrChange>");
            rest = &rest[p + tag_end + 1..];
            continue;
        }
        // Open tag: if the content up to the matching close lacks <w:pPr, inject.
        let content_start = p + tag_end + 1;
        let Some(close_rel) = rest[content_start..].find("</w:pPrChange>") else {
            out.push_str(rest);
            return out;
        };
        let content = &rest[content_start..content_start + close_rel];
        out.push_str(&rest[..content_start]);
        if !content.contains("<w:pPr") {
            out.push_str("<w:pPr/>");
        }
        out.push_str(content);
        rest = &rest[content_start + close_rel..];
    }
}

/// Pass 5 — the illegal `w:type="odd"` token on header/footer references
/// becomes `"default"` (ST_HdrFtr has no "odd"; Word maps odd pages to it).
fn fix_hdrftr_type_odd(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<'
            && (starts_with_tag(&bytes[i..], b"w:headerReference")
                || starts_with_tag(&bytes[i..], b"w:footerReference"))
        {
            let Some(end_rel) = find_tag_end(&bytes[i..]) else {
                out.push_str(&s[i..]); // unterminated: `find_tag_end`'s contract
                break;
            };
            let tag = &s[i..i + end_rel + 1];
            out.push_str(&tag.replace("w:type=\"odd\"", "w:type=\"default\""));
            i += end_rel + 1;
            continue;
        }
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Passes 1+2 — each `<w:tbl>` gains missing required children: `<w:tblPr/>`
/// right after the start tag, `<w:tblGrid/>` right before the first `<w:tr`.
/// The deficiency window is the span from the `<w:tbl>` start tag to its first
/// `<w:tr` (or nested content start) — schema order is (markup*, tblPr,
/// tblGrid, rows), and the observed writers put nothing between.
fn inject_missing_table_children(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 64);
    let mut rest = s;
    loop {
        let Some(p) = rest.find("<w:tbl>") else {
            out.push_str(rest);
            return out;
        };
        let head_start = p + "<w:tbl>".len();
        // Window: up to the first row (or the table's close for a row-less tbl).
        let win_end = rest[head_start..]
            .find("<w:tr")
            .or_else(|| rest[head_start..].find("</w:tbl>"))
            .map(|w| head_start + w)
            .unwrap_or(rest.len());
        let window = &rest[head_start..win_end];
        out.push_str(&rest[..head_start]);
        if !window.contains("<w:tblPr") {
            out.push_str("<w:tblPr/>");
        }
        if !window.contains("<w:tblGrid") {
            // After the (possibly just-injected) tblPr, before the first row —
            // appending at the window end preserves (tblPr, …, tblGrid, rows).
            out.push_str(window);
            out.push_str("<w:tblGrid/>");
        } else {
            out.push_str(window);
        }
        rest = &rest[win_end..];
    }
}

/// True if `b` begins `<{name}` as an element start (next char terminates the
/// name) — the RULE-2 `starts_with_pgmar_tag` shape, generalized.
fn starts_with_tag(b: &[u8], name: &[u8]) -> bool {
    if b.len() < name.len() + 2 || &b[1..1 + name.len()] != name {
        return false;
    }
    matches!(b[1 + name.len()], b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>')
}
