//! RULE 1 — SML worksheet `<col>` `@min`/`@max` injection (§18.3.1.13).
//!
//! `sml-styling` makes `Column.start_column`/`end_column` (the `<col>`
//! `@min`/`@max` attrs) REQUIRED non-`Option` `u32` fields, so a worksheet whose
//! `<col>` omits either attribute makes the strict generated parser return
//! `ParseError::MissingAttribute("min"|"max")` and fail the whole sheet.
//!
//! This rule (the Layer-2 render-only pre-repair of the missing-`min`/`max` shim,
//! MIGRATED here verbatim from `sml/shim.rs`) scans every `<col …/>` start tag
//! and injects the missing `@min`/`@max` (min = running 1-based column index,
//! advancing one per repaired `<col>`; max == min) so the strict parser
//! succeeds. Everything else is byte-preserved; only deficient `<col>` tags are
//! touched, only the missing attribute(s) added.
//!
//! Behavior is IDENTICAL to the pre-migration `shim::repair_col_attrs`; `shim.rs`
//! now delegates here. Removed once the fork codegen is lenient.

use super::xml_scan::{attr_present, find_tag_end, utf8_len};

/// Scan `<col …/>` / `<col …>` start tags and inject any missing `@min`/`@max`.
/// See the module docs for the running-index contract. Conservative: operates on
/// raw bytes (numeric attrs need no entity decoding), only rewrites `<col` start
/// tags, never removes or reorders anything. Non-UTF8 input is returned
/// untouched.
pub(crate) fn repair_col_attrs(xml: &[u8]) -> Vec<u8> {
    let s = match std::str::from_utf8(xml) {
        Ok(s) => s,
        Err(_) => return xml.to_vec(),
    };
    let mut out = String::with_capacity(s.len() + 32);
    let mut next_col: u32 = 1;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' && starts_with_col_tag(&bytes[i..]) {
            let Some(end_rel) = find_tag_end(&bytes[i..]) else {
                out.push_str(&s[i..]); // unterminated: `find_tag_end`'s contract
                break;
            };
            let tag = &s[i..i + end_rel + 1];
            let (rewritten, advanced) = repair_one_col(tag, next_col);
            next_col = advanced;
            out.push_str(&rewritten);
            i += end_rel + 1;
            continue;
        }
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    out.into_bytes()
}

/// True if `b` begins `<col` as an element start (next char is space, `/`, or
/// `>`).
fn starts_with_col_tag(b: &[u8]) -> bool {
    if b.len() < 5 || &b[1..4] != b"col" {
        return false;
    }
    matches!(b[4], b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>')
}

/// Inject missing `@min`/`@max` into a single `<col …>` / `<col …/>` tag.
/// Returns the (possibly rewritten) tag and the next running column index.
fn repair_one_col(tag: &str, mut next_col: u32) -> (String, u32) {
    let has_min = attr_present(tag, "min");
    let has_max = attr_present(tag, "max");
    if has_min && has_max {
        return (tag.to_string(), next_col);
    }

    let min_val = next_col;
    next_col += 1;
    let max_val = min_val;

    let insert_at = "<col".len();
    let mut injected = String::new();
    if !has_min {
        injected.push_str(&format!(" min=\"{min_val}\""));
    }
    if !has_max {
        injected.push_str(&format!(" max=\"{max_val}\""));
    }

    let mut rewritten = String::with_capacity(tag.len() + injected.len());
    rewritten.push_str(&tag[..insert_at]);
    rewritten.push_str(&injected);
    rewritten.push_str(&tag[insert_at..]);
    (rewritten, next_col)
}
