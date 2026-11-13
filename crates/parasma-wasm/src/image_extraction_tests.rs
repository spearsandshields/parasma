//! Regression tests for embedded-image extraction (the "blank image" report).
//!
//! These pin the namespace-agnostic image extraction for BOTH the inline
//! (`wp:inline`) and anchored (`wp:anchor`) drawing markup shapes — the two
//! fixtures whose rendered output (a 1×1 PNG stretched to 96×96px) looks blank
//! on visual inspection, which once led to a mistaken "extraction yields
//! nothing" diagnosis. The extraction works end-to-end; these tests guard that.
//!
//! Kept in a SEPARATE file (rather than inline in `lib.rs`) so adding/removing
//! cases never shifts `lib.rs` source line numbers — those line numbers are
//! embedded in the release wasm's panic-location table, and a shift there
//! changes the wasm bytes for a test-only edit (tripping check-wasm-identity).

use crate::BlockContent;
use ooxml_wml::ext::{DrawingExt, ParagraphExt, RunExt};
use std::io::Cursor;

/// Asserts a docx fixture carries the embedded PNG and that the body drawing's
/// `all_image_rel_ids()` surfaces exactly `rId1`:
///   (a) the doc carries an image rel `rId1 → media/image1.png`,
///   (b) the body drawing's `all_image_rel_ids()` == `["rId1"]`, and
///   (c) `get_image_data("rId1")` returns the 69-byte `image/png` payload.
/// `all_image_rel_ids` is namespace-agnostic (`local_name_of`), so this covers
/// both root-declared and element-local namespace styles.
fn assert_image_fixture(path: &str) {
    let bytes = std::fs::read(path).expect("fixture present");
    let mut doc = ooxml_wml::Document::from_reader(Cursor::new(bytes)).expect("parse");

    // (a) image relationship rId1 → media/image1.png
    let rel = doc
        .doc_relationships()
        .iter()
        .find(|r| r.relationship_type.contains("/image"))
        .cloned()
        .expect("image relationship present");
    assert_eq!(rel.id, "rId1", "{path}: image rel id");
    assert_eq!(rel.target, "media/image1.png", "{path}: image rel target");

    // (b) the body drawing surfaces exactly rId1
    let mut rel_ids: Vec<String> = Vec::new();
    for block in &doc.body().block_content {
        if let BlockContent::P(p) = block {
            for run in p.runs() {
                for drawing in run.drawings() {
                    for id in drawing.all_image_rel_ids() {
                        rel_ids.push(id.to_string());
                    }
                }
            }
        }
    }
    assert_eq!(rel_ids, vec!["rId1".to_string()], "{path}: drawing image rel ids");

    // (c) bytes resolve to a 69-byte image/png
    let img = doc.get_image_data("rId1").expect("image data present");
    assert_eq!(img.content_type, "image/png", "{path}: content type");
    assert_eq!(img.data.len(), 69, "{path}: byte length");
}

#[test]
fn inline_image_extracts_rid1_png() {
    assert_image_fixture("../../fixtures/image/image-inline.docx");
}

#[test]
fn anchored_image_extracts_rid1_png() {
    assert_image_fixture("../../fixtures/image/image-anchored.docx");
}
