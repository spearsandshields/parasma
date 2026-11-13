//! Enhanced Metafile (MS-EMF) vector renderer — OFF by default behind the `emf`
//! Cargo feature. Parses an inline `image/x-emf` picture part into a cached
//! drawing program ([`EmfPicture`]) once, then replays it onto the engine
//! `Painter` (all geometry pre-transformed in Rust). DIB blits are decoded to
//! RGBA and handed to the existing `createImageBitmap` pipeline via a synthetic
//! key. Unsupported records / DIB formats degrade gracefully (grey nothing, the
//! vector layer still shows); a malformed stream aborts to `None` and the caller
//! falls back to the grey placeholder.
//!
//! Spec grounding: [MS-EMF] (the Enhanced Metafile Format), record framing
//! §2.1.1 + §2.3, DIB §2.2.2.

pub(crate) mod dc;
pub(crate) mod dib;
pub(crate) mod reader;
pub(crate) mod records;
pub(crate) mod render;
pub(crate) mod transform;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use reader::Cursor;
use records::{BlitHeader, EmrRecord, RectL};
use transform::MapState;

/// `dSignature` in EMR_HEADER — the ASCII " EMF" (little-endian u32).
const ENHMETA_SIGNATURE: u32 = 0x464D_4520;

/// A decoded DIB awaiting a JS `ImageBitmap` upload. Referenced by its index in
/// [`EmfPicture::blits`]; the full synthetic cache key is `emf:{rel}:{index}`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EmfBlit {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// A parsed EMF drawing program, cached once per picture part.
#[derive(Clone, Debug)]
pub(crate) struct EmfPicture {
    /// Device-unit drawing extent (drives the placement matrix).
    pub bounds: RectL,
    /// `.01mm` frame (retained for degenerate-bounds fallback / metric modes).
    #[allow(dead_code)]
    pub frame: RectL,
    pub szl_device: (i32, i32),
    pub szl_mm: (i32, i32),
    pub records: Vec<EmrRecord>,
    pub blits: Vec<EmfBlit>,
    /// The parent image rel-id — the synthetic-key prefix, set by the Viewer at
    /// cache time (empty until then).
    pub rel: String,
}

impl EmfPicture {
    /// The initial map-mode seed for a fresh replay (MM_TEXT + header device
    /// metrics so metric map modes can scale).
    pub(crate) fn map_seed(&self) -> MapState {
        MapState { szl_device: self.szl_device, szl_mm: self.szl_mm, ..MapState::default() }
    }

    /// Number of PAINTABLE ops — used to detect a near-empty downlevel stream in
    /// an EMF+-primary file (degrade to the grey placeholder when ~0).
    pub(crate) fn paintable_op_count(&self) -> usize {
        self.records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    EmrRecord::MoveToEx(_)
                        | EmrRecord::LineTo(_)
                        | EmrRecord::Poly { .. }
                        | EmrRecord::PolyPoly { .. }
                        | EmrRecord::Bezier { .. }
                        | EmrRecord::Rectangle(_)
                        | EmrRecord::Ellipse(_)
                        | EmrRecord::RoundRect(_)
                        | EmrRecord::ExtTextOut(_)
                        | EmrRecord::Blit(_)
                )
            })
            .count()
    }
}

/// Parse an EMF byte stream into a cached drawing program. Returns `None` on any
/// framing error (bad `nSize`, misalignment, overflow) or when there is nothing
/// paintable to draw.
pub(crate) fn parse(bytes: &[u8]) -> Option<EmfPicture> {
    let mut cur = Cursor::new(bytes);

    // ── EMR_HEADER (iType 1) ────────────────────────────────────────────────
    let itype = cur.u32()?;
    let n_size = cur.u32()? as usize;
    if itype != records::EMR_HEADER || n_size < 8 || !n_size.is_multiple_of(4) || n_size > bytes.len()
    {
        return None;
    }
    let hdr = reader::Body::new(bytes.get(8..n_size)?);
    let bounds = RectL {
        left: hdr.i32(0)?,
        top: hdr.i32(4)?,
        right: hdr.i32(8)?,
        bottom: hdr.i32(12)?,
    };
    let frame = RectL {
        left: hdr.i32(16)?,
        top: hdr.i32(20)?,
        right: hdr.i32(24)?,
        bottom: hdr.i32(28)?,
    };
    if hdr.u32(32)? != ENHMETA_SIGNATURE {
        return None;
    }
    // szlDevice / szlMillimeters (optional; 0 if the header stopped short).
    let szl_device = (hdr.i32(64).unwrap_or(0), hdr.i32(68).unwrap_or(0));
    let szl_mm = (hdr.i32(72).unwrap_or(0), hdr.i32(76).unwrap_or(0));

    let mut records_out: Vec<EmrRecord> = Vec::new();
    let mut blits: Vec<EmfBlit> = Vec::new();

    // ── record framing loop ─────────────────────────────────────────────────
    cur.seek(n_size)?;
    while cur.remaining() >= 8 {
        let rec_start = cur.pos();
        let itype = cur.u32()?;
        let n_size = cur.u32()? as usize;
        if n_size < 8 || !n_size.is_multiple_of(4) {
            return None; // malformed framing — abort to the grey placeholder
        }
        let rec_end = rec_start.checked_add(n_size)?;
        if rec_end > bytes.len() {
            return None;
        }
        let body = &bytes[rec_start + 8..rec_end];
        let mut rec = records::decode(itype, body);

        // Blit records: decode the inline DIB from its record-relative offsets.
        if let EmrRecord::Blit(ref mut hdr) = rec {
            decode_blit(bytes, rec_start, rec_end, hdr, &mut blits);
        }

        let is_eof = matches!(rec, EmrRecord::Eof);
        records_out.push(rec);
        cur.seek(rec_end)?;
        if is_eof {
            break;
        }
    }

    let pic = EmfPicture {
        bounds,
        frame,
        szl_device,
        szl_mm,
        records: records_out,
        blits,
        rel: String::new(),
    };

    // Degrade an EMF+-primary file with a near-empty downlevel stream.
    if pic.paintable_op_count() == 0 {
        return None;
    }
    Some(pic)
}

/// Decode a blit's DIB (offsets are record-relative) and, on success, register
/// it in `blits` and point `hdr.blit` at it.
fn decode_blit(bytes: &[u8], rec_start: usize, rec_end: usize, hdr: &mut BlitHeader, blits: &mut Vec<EmfBlit>) {
    let slice = |off: usize, len: usize| -> Option<&[u8]> {
        if off == 0 || len == 0 {
            return None;
        }
        let a = rec_start.checked_add(off)?;
        let b = a.checked_add(len)?;
        if b <= rec_end {
            bytes.get(a..b)
        } else {
            None
        }
    };
    let bmi = match slice(hdr.off_bmi, hdr.cb_bmi) {
        Some(s) => s,
        None => return,
    };
    let bits = match slice(hdr.off_bits, hdr.cb_bits) {
        Some(s) => s,
        None => return,
    };
    if let Some(d) = dib::decode(bmi, bits) {
        hdr.blit = Some(blits.len());
        blits.push(EmfBlit { w: d.w, h: d.h, rgba: d.rgba });
    }
}

// ── live-render dispatch (thread-local registry) ────────────────────────────
//
// The Viewer sets the active pictures before painting a page (parse-once cache →
// cheap `Rc` clones here) so the WML render path can dispatch an `image/x-emf`
// rel WITHOUT threading a new parameter through every render function.
// Populated per page render; overwritten each time so it never goes stale
// across documents.
//
// Raster-cache model: the page paint no longer inlines the EMF vector program
// (`render::paint`) — it emits ONE `draw_image` op under the synthetic
// [`pic_key`] and logs a paint REQUEST (drawn CSS footprint). The TS adapter
// rasterizes the program off the paint hot path via the `renderEmfToCanvas`
// export and registers the resulting `ImageBitmap` under the same key, so both
// the in-process `CanvasPainter` and the worker display-list replay blit it
// like any ordinary image.
thread_local! {
    static ACTIVE: RefCell<HashMap<String, Rc<EmfPicture>>> = RefCell::new(HashMap::new());
    /// Per-paint EMF draw requests `(pic_key, dw, dh)` in PAGE-LOGICAL px (no
    /// device/zoom awareness — resolution policy is a TS-adapter concern).
    /// Cleared by [`set_active`] at the start of every page paint so requests
    /// can never accumulate unbounded where nothing drains them (the worker);
    /// drained by the `takeEmfPaintRequests` export in-process.
    static PAINT_REQUESTS: RefCell<Vec<(String, f64, f64)>> = const { RefCell::new(Vec::new()) };
}

/// Set the pictures reachable during the current page render. Also clears the
/// paint-request log (a new paint's requests start fresh; an undrained prior
/// paint's requests are dropped — bounded by one paint's worth).
pub(crate) fn set_active(pics: &HashMap<String, Rc<EmfPicture>>) {
    ACTIVE.with(|a| {
        let mut m = a.borrow_mut();
        m.clear();
        for (k, v) in pics {
            m.insert(k.clone(), Rc::clone(v));
        }
    });
    PAINT_REQUESTS.with(|r| r.borrow_mut().clear());
}

/// The synthetic display-list key a whole EMF picture draws under
/// (`"emf-pic:{rel}"`). Prefix-DISTINCT from the internal DIB blit keys
/// `"emf:{rel}:{n}"` (hyphen + colon — `"emf-pic:…".starts_with("emf:")` is
/// false), which stay internal to the rasterize pass.
pub(crate) fn pic_key(rel: &str) -> String {
    format!("emf-pic:{rel}")
}

/// True when `rel_id` names an active EMF picture on this page paint.
pub(crate) fn is_active(rel_id: &str) -> bool {
    ACTIVE.with(|a| a.borrow().contains_key(rel_id))
}

/// Log one EMF draw request: the picture's drawn footprint `(dw, dh)` in
/// page-logical px. The TS drain multiplies by zoom·dpr exactly once.
pub(crate) fn note_request(rel_id: &str, dw: f64, dh: f64) {
    PAINT_REQUESTS.with(|r| r.borrow_mut().push((pic_key(rel_id), dw, dh)));
}

/// Drain the paint-request log (in-process requested-px computation; worker
/// mode never calls this — it detects draws replay-side instead).
pub(crate) fn take_requests() -> Vec<(String, f64, f64)> {
    PAINT_REQUESTS.with(|r| std::mem::take(&mut *r.borrow_mut()))
}
