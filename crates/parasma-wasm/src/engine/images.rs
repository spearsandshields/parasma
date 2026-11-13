//! Image registry + canvas image painting.
//!
//! Verbatim relocation from `lib.rs` — behavior changes during extraction are
//! forbidden, enforced by the oracle snapshot tests (`oracle_tests.rs`) whose
//! geometry hashes must not drift on a pure move.

use std::collections::HashMap;
use wasm_bindgen::JsValue;
use web_sys::ImageBitmap;

use crate::engine::paint::Painter;
use crate::AnchoredImageLayout;

/// Derive an image mime type from a media part's file extension. Lives in
/// engine/ (always compiled, format-agnostic) so both the `sml` and `pml`
/// feature builds share ONE copy — the previous per-format duplicates existed
/// only because `pml::mime_from_path` was feature-gated out of an sml-only
/// build.
pub(crate) fn mime_from_path(path: &str) -> String {
    let ext = path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Raw image data extracted from the OOXML package.
pub(crate) struct ImageBytes {
    pub(crate) mime: String,
    pub(crate) data: Vec<u8>,
}

/// Grey rectangle with "[image]" label for images that haven't yet decoded
/// (or decoded to an unsupported format), so a missing image is never blank.
pub(crate) fn draw_image_placeholder(
    p: &dyn Painter,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<(), JsValue> {
    p.set_fill_style_str("#e6edf3");
    p.fill_rect(x, y, w, h);
    p.set_stroke_style_str("#7d8590");
    p.set_line_width(1.0);
    p.stroke_rect(x, y, w, h);
    p.set_fill_style_str("#7d8590");
    p.set_font("11px -apple-system, sans-serif");
    p.set_text_baseline("alphabetic");
    p.fill_text("[image]", x + 8.0, y + 18.0)?;
    Ok(())
}

pub(crate) fn render_anchored_image(
    p: &dyn Painter,
    ai: &AnchoredImageLayout,
    _bitmaps: &HashMap<String, ImageBitmap>,
) {
    // EMF dispatch (mirrors the inline-image arm in wml/render.rs): an anchored
    // `image/x-emf` rel draws under the synthetic `emf-pic:{rel}` raster key —
    // previously it fell straight through to `draw_image(rel)` and painted a
    // placeholder forever (anchored EMFs had NO emf dispatch at all). Feature-off
    // this cfg block is absent: byte-identical.
    #[cfg(feature = "emf")]
    if crate::emf::is_active(&ai.rel_id) {
        crate::emf::note_request(&ai.rel_id, ai.width_px, ai.height_px);
        let _ = p.draw_image(&crate::emf::pic_key(&ai.rel_id), ai.x, ai.y, ai.width_px, ai.height_px);
        return;
    }
    let _ = p.draw_image(&ai.rel_id, ai.x, ai.y, ai.width_px, ai.height_px);
}
