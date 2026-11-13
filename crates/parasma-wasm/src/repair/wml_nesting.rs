//! RULE 7 — table-nesting depth cap (stack-overflow guard).
//!
//! The fork's generated parsers are recursive-descent: a `<w:tbl>` nested N
//! deep recurses N frames. A torture file (corpus: POI deep-table-cell.docx,
//! 5000 levels) overflows the stack — natively an abort, in the browser a
//! fatal wasm trap that KILLS THE VIEWER INSTANCE. Layout recursion has the
//! same shape one stage later.
//!
//! A stack overflow is an UNCATCHABLE abort, not an `Err` — so unlike RULE
//! 2/5/6 this rule cannot dispatch on failure. It is dispatch-on-SIGNATURE
//! (eager pre-scan, the retired RULE-4 shape, kept for the same reason it was
//! chosen there): before the FIRST parse attempt, scan `word/document.xml`
//! for `<w:tbl>` nesting depth; beyond [`MAX_TBL_DEPTH`] the offending
//! subtree is replaced with a marker paragraph. Legitimate documents nest
//! single digits deep — 32 is far past any real file while keeping parser AND
//! layout recursion trivially inside the wasm stack (~1 MB dies around ~600
//! levels). Files at or under the cap are returned BYTE-IDENTICAL; every load
//! pays one extra inflate of document.xml plus ONE linear tag-event scan (the
//! depth gate) — the rewrite itself runs only on genuinely deep nesting.
//!
//! Word opens such files (slowly); no real document needs 5000
//! levels — the graceful degrade (everything above the cap intact, a visible
//! `[nested table truncated]` marker at the cap) is the honest rendering.

use std::io::{Cursor, Read};

/// Maximum permitted `<w:tbl>` nesting depth. Beyond this, subtrees truncate.
pub(crate) const MAX_TBL_DEPTH: usize = 32;

/// The block-level marker that replaces a truncated subtree — a `<w:tc>`
/// requires at least one block element, and a paragraph satisfies every
/// context a table may appear in.
const MARKER: &str = "<w:p><w:r><w:t>[nested table truncated]</w:t></w:r></w:p>";

/// Cap the package's table nesting: returns the original bytes when within
/// the cap (or on any structural problem — the parse then proceeds and
/// surfaces its own error), or a rebuilt package with `word/document.xml`
/// truncated at [`MAX_TBL_DEPTH`].
pub(crate) fn cap_package_nesting(bytes: Vec<u8>) -> Vec<u8> {
    let Some(doc_xml) = read_document_xml(&bytes) else {
        return bytes;
    };
    let Ok(text) = std::str::from_utf8(&doc_xml) else {
        return bytes;
    };
    // Cheap gate on the REAL max depth (one linear event scan, no string
    // building) — a report with hundreds of FLAT tables never proceeds past
    // this line; only genuinely deep nesting pays the rewrite.
    if max_tbl_depth(&tbl_events(text)) <= MAX_TBL_DEPTH {
        return bytes;
    }
    let capped = cap_table_nesting(text);
    if capped.as_bytes() == doc_xml.as_slice() {
        return bytes;
    }
    let doc_path = document_path(&bytes).unwrap_or_else(|| "word/document.xml".to_string());
    match super::zip_rewrite::rebuild_zip_replacing_part(
        &bytes,
        &[(doc_path, capped.into_bytes())],
    ) {
        Some(rebuilt) => rebuilt,
        None => bytes,
    }
}

/// One `(byte_pos, is_open)` event per `<w:tbl>`/`</w:tbl>` tag, in document
/// order. Built in two LINEAR passes + one sort — the previous paired-`find`
/// loops re-scanned to the (distant) first close on EVERY open, which is
/// quadratic on the pathological all-opens-then-all-closes shape (7s on the
/// 5000-level corpus file; ~50ms with the event walk).
fn tbl_events(s: &str) -> Vec<(usize, bool)> {
    let mut ev: Vec<(usize, bool)> = Vec::new();
    let mut i = 0;
    while let Some(p) = s[i..].find("<w:tbl>") {
        let at = i + p;
        ev.push((at, true));
        i = at + "<w:tbl>".len();
    }
    i = 0;
    while let Some(p) = s[i..].find("</w:tbl>") {
        let at = i + p;
        ev.push((at, false));
        i = at + "</w:tbl>".len();
    }
    ev.sort_unstable_by_key(|e| e.0);
    ev
}

/// Maximum nesting depth over an event list (saturating on stray closes).
fn max_tbl_depth(events: &[(usize, bool)]) -> usize {
    let mut depth = 0usize;
    let mut max = 0usize;
    for &(_, is_open) in events {
        if is_open {
            depth += 1;
            max = max.max(depth);
        } else {
            depth = depth.saturating_sub(1);
        }
    }
    max
}

/// Truncate every `<w:tbl>` subtree that OPENS at depth > [`MAX_TBL_DEPTH`],
/// replacing it with [`MARKER`]. Content outside truncated subtrees is
/// byte-preserved; a document within the cap round-trips unchanged. UNBALANCED
/// markup returns the input untouched (the parser then surfaces its own
/// error). Single O(n) event walk — see [`tbl_events`].
pub(crate) fn cap_table_nesting(s: &str) -> String {
    let events = tbl_events(s);
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize; // copied-up-to byte position
    let mut depth = 0usize;
    let mut skipping = 0usize; // >0 → inside a truncated subtree (open count)
    for &(pos, is_open) in &events {
        if skipping > 0 {
            if is_open {
                skipping += 1;
            } else {
                skipping -= 1;
                if skipping == 0 {
                    // Resume copying just past the matching close.
                    cursor = pos + "</w:tbl>".len();
                }
            }
            continue;
        }
        if is_open {
            if depth >= MAX_TBL_DEPTH {
                out.push_str(&s[cursor..pos]);
                out.push_str(MARKER);
                skipping = 1;
            } else {
                depth += 1;
            }
        } else {
            depth = depth.saturating_sub(1);
        }
    }
    if skipping > 0 {
        // A truncated subtree never closed — unbalanced markup. Hand the
        // ORIGINAL to the parser rather than shipping a half-rewrite.
        return s.to_string();
    }
    out.push_str(&s[cursor..]);
    out
}

fn read_document_xml(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let path = document_path_in(&mut archive).unwrap_or_else(|| "word/document.xml".to_string());
    let mut entry = archive.by_name(&path).ok()?;
    let declared = entry.size();
    super::zip_rewrite::read_entry_bytes(&mut entry, declared)
}

fn document_path(bytes: &[u8]) -> Option<String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    document_path_in(&mut archive)
}

/// Root-rels officeDocument target (leading slash stripped) — the same
/// resolution the RULE-2 repair uses.
fn document_path_in(archive: &mut zip::ZipArchive<Cursor<&[u8]>>) -> Option<String> {
    let mut entry = archive.by_name("_rels/.rels").ok()?;
    let mut rels = String::new();
    entry.read_to_string(&mut rels).ok()?;
    let mut rest = rels.as_str();
    while let Some(p) = rest.find("<Relationship") {
        let tag_end = rest[p..].find('>').map(|e| p + e + 1).unwrap_or(rest.len());
        let tag = &rest[p..tag_end];
        let ty_is_doc = tag
            .split_whitespace()
            .any(|a| a.starts_with("Type=") && a.contains("/officeDocument\""));
        if ty_is_doc {
            let key = "Target=\"";
            let i = tag.find(key)? + key.len();
            let j = tag[i..].find('"')? + i;
            return Some(tag[i..j].trim_start_matches('/').to_string());
        }
        rest = &rest[tag_end.min(rest.len())..];
    }
    None
}
