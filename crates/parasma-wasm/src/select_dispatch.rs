//! The unified selection / hit-test / extract verb set.
//!
//! ONE format-neutral verb family (`docHitTest`, `docHitTestObject`,
//! `docSelectionRects`, `docSetSelection`, `docSetSelectedObject`,
//! `docExtractRange`) replaces the sixteen per-format exports that spelled the
//! same four concepts three times (WML `hitTest…`, SML `gridHitTest…`, PML
//! `pmlHitTest…`). The LOADED doc kind (`self.loaded`) is the discriminant — no
//! format tag crosses the wire. ECMA-376 defines no cross-format coordinate
//! model (WML §17 pages / SML §18 sheets / PML §19 slides); this union is an
//! engineering seam, declared as such.
//!
//! WIRE SHAPES (plain JS objects; structured-clone-safe so the worker's generic
//! dispatcher ships them unchanged):
//!
//! - `DocLoc` — `{ surface, scrollX?, scrollY?, clamp? }`. `surface` is the
//!   0-based page (WML) / sheet (SML grid) / slide (PML). `scrollX`/`scrollY`
//!   (BOTH present) address the SML grid's content space — they must cross the
//!   boundary because frozen panes make viewport→content non-affine
//!   (§18.3.1.66 quadrant mapping, `sml/pane.rs`). An SML doc IGNORES a
//!   scroll-less loc (returns null/[]): the grid is the sole selectable SML
//!   surface, which preserves the legacy behavior where WML-shaped probes
//!   no-op'd on a spreadsheet. WML/PML ignore scroll offsets. `clamp`
//!   (`{targetKind, targetA, targetB}`) is the PML drag clamp — resolve within
//!   that shape/cell (a drag never escapes its anchor target).
//! - `DocSelection` — the ONE cross-boundary selection currency:
//!   `{ kind:"text", start:{para,byte}, end:{para,byte} }` |
//!   `{ kind:"cells", sheet, range:{r0,c0,r1,c1}, anchor?:{row,col} }` |
//!   `{ kind:"pmlText", slide, target, start, end }`. `null` CLEARS — replacing
//!   the three legacy sentinel conventions (negative para / negative sheet /
//!   negative slide).
//! - `DocHit` — `{ kind:"text", surface, pos, range?, target? }` (WML: no
//!   `target`; PML: `target` present) | `{ kind:"cell", surface, zone, row,
//!   col, mergeAnchor? }` (SML grid).
//! - `DocObjectHit` — `{ surface, kind, key, rect }`; `kind` is the ObjectRef
//!   kind ("inlineImage" | "anchoredImage" | "chart" | "picture") and `key` the
//!   shared relId/chart-key namespace (`chartData` already treats WML relIds
//!   and SML/PML keys as one map).
//!
//! Every read is a pure READ (layout/render byte-neutral); the two setters
//! write the SAME state cells the legacy setters wrote (`None` ⇒ the paint
//! pass is skipped ⇒ byte-identical render). The per-format logic lives
//! untouched in `wml::select` / `sml::select` / `pml::select` — those modules
//! encode DIFFERENT rules (bidi/cluster snapping vs merge expansion vs target
//! walks), which is knowledge, not duplication; only the verb surface above
//! them collapsed.
//!
//! `granularity` ("char" | "word" | "paragraph") is accepted for every format
//! but expands ranges for WML only today (PML word/paragraph selection is a
//! declared v1 deferral; the grid ignores it) — same behavior as the legacy
//! exports, now documented in one place.

use wasm_bindgen::prelude::*;

use crate::wml::select::TextPos;
use crate::{js_set, LoadedDoc, Viewer};

// ─── Parsed wire types ───────────────────────────────────────────────────────

/// A parsed `DocLoc`. `scroll` is `Some` only when BOTH offsets were present
/// (the grid adapter always sends both, 0-valued at origin).
pub(crate) struct DocLocArg {
    pub(crate) surface: usize,
    pub(crate) scroll: Option<(f64, f64)>,
    #[cfg(feature = "pml")]
    pub(crate) clamp: Option<crate::pml::select::PmlTarget>,
}

/// A parsed `DocSelection` — the native currency behind `docSetSelection` /
/// `docSelectionRects` / `docExtractRange`. Range endpoints stay RAW here;
/// normalization (start ≤ end) happens only in the setter, exactly where the
/// legacy setters normalized.
pub(crate) enum DocSelectionArg {
    Text {
        start: TextPos,
        end: TextPos,
    },
    #[cfg(feature = "sml")]
    Cells {
        sheet: u32,
        range: crate::sml::select::CellRange,
        anchor_row: u32,
        anchor_col: u32,
    },
    #[cfg(feature = "pml")]
    PmlText {
        sel: crate::pml::select::PmlSelection,
    },
}

fn get(obj: &JsValue, key: &str) -> Option<JsValue> {
    js_sys::Reflect::get(obj, &JsValue::from_str(key)).ok().filter(|v| !v.is_undefined() && !v.is_null())
}

fn get_f64(obj: &JsValue, key: &str) -> Option<f64> {
    get(obj, key)?.as_f64()
}

fn get_str(obj: &JsValue, key: &str) -> Option<String> {
    get(obj, key)?.as_string()
}

fn parse_loc(loc: &JsValue) -> Option<DocLocArg> {
    if !loc.is_object() {
        return None;
    }
    // `filter(>= 0)` rejects negative AND NaN surfaces (NaN fails the compare).
    let surface = get_f64(loc, "surface").filter(|s| *s >= 0.0)?;
    let scroll = match (get_f64(loc, "scrollX"), get_f64(loc, "scrollY")) {
        (Some(sx), Some(sy)) => Some((sx, sy)),
        _ => None,
    };
    #[cfg(feature = "pml")]
    let clamp = get(loc, "clamp").and_then(|c| {
        let kind = get_f64(&c, "targetKind")? as i32;
        let a = get_f64(&c, "targetA").unwrap_or(0.0) as u32;
        let b = get_f64(&c, "targetB").unwrap_or(0.0) as u32;
        crate::pml::select::PmlTarget::from_wire(kind, a, b)
    });
    Some(DocLocArg {
        surface: surface as usize,
        scroll,
        #[cfg(feature = "pml")]
        clamp,
    })
}

/// The VALIDATION POLICY for a wire text position, split from the JsValue
/// plumbing so it can be pinned natively — `js_sys::Reflect` needs a JS
/// runtime, which is why the legacy `set_selection_negative_para_clears` pin
/// could not survive the move to a JsValue signature as-is.
///
/// `filter(>= 0)` on BOTH fields, matching the surface/sheet/slide filters
/// above: `as u32` SATURATES, so an unfiltered −3 would store a selection at
/// para 0 instead of clearing. The legacy sentinel wires cleared on a negative
/// index and `doc_set_selection`'s docblock promises that fail-safe — this is
/// where it lives. NaN is rejected too (NaN fails the compare).
pub(crate) fn textpos_from_wire(para: Option<f64>, byte: Option<f64>) -> Option<TextPos> {
    let para = para.filter(|p| *p >= 0.0)?;
    let byte = byte.filter(|b| *b >= 0.0)?;
    Some(TextPos::new(para as u32, byte as u32))
}

fn parse_textpos(v: &JsValue) -> Option<TextPos> {
    textpos_from_wire(get_f64(v, "para"), get_f64(v, "byte"))
}

fn parse_selection(sel: &JsValue) -> Option<DocSelectionArg> {
    if !sel.is_object() {
        return None;
    }
    match get_str(sel, "kind")?.as_str() {
        "text" => {
            let start = parse_textpos(&get(sel, "start")?)?;
            let end = parse_textpos(&get(sel, "end")?)?;
            Some(DocSelectionArg::Text { start, end })
        }
        #[cfg(feature = "sml")]
        "cells" => {
            // `filter(>= 0)` rejects negative AND NaN (NaN fails the compare).
            let sheet = get_f64(sel, "sheet").filter(|s| *s >= 0.0)?;
            let r = get(sel, "range")?;
            let range = crate::sml::select::CellRange::new(
                get_f64(&r, "r0")? as u32,
                get_f64(&r, "c0")? as u32,
                get_f64(&r, "r1")? as u32,
                get_f64(&r, "c1")? as u32,
            );
            // `anchor` is optional on the wire (extract ignores it); default to
            // the range corner so a set without one still paints a valid active
            // cell (the legacy `.max(1)` floor is applied in the setter core).
            let (anchor_row, anchor_col) = match get(sel, "anchor") {
                Some(a) => (
                    get_f64(&a, "row").unwrap_or(range.r0 as f64) as u32,
                    get_f64(&a, "col").unwrap_or(range.c0 as f64) as u32,
                ),
                None => (range.r0, range.c0),
            };
            Some(DocSelectionArg::Cells { sheet: sheet as u32, range, anchor_row, anchor_col })
        }
        #[cfg(feature = "pml")]
        "pmlText" => {
            // `filter(>= 0)` rejects negative AND NaN (NaN fails the compare).
            let slide = get_f64(sel, "slide").filter(|s| *s >= 0.0)?;
            let t = get(sel, "target")?;
            let target = crate::pml::select::PmlTarget::from_wire(
                get_f64(&t, "targetKind")? as i32,
                get_f64(&t, "targetA").unwrap_or(0.0) as u32,
                get_f64(&t, "targetB").unwrap_or(0.0) as u32,
            )?;
            let start = parse_textpos(&get(sel, "start")?)?;
            let end = parse_textpos(&get(sel, "end")?)?;
            Some(DocSelectionArg::PmlText {
                sel: crate::pml::select::PmlSelection {
                    slide: slide as usize,
                    target,
                    start_para: start.para as usize,
                    start_byte: start.byte as usize,
                    end_para: end.para as usize,
                    end_byte: end.byte as usize,
                },
            })
        }
        _ => None,
    }
}

// ─── JS result builders ──────────────────────────────────────────────────────

fn textpos_js(para: u32, byte: u32) -> js_sys::Object {
    let o = js_sys::Object::new();
    js_set(&o, "para", &JsValue::from_f64(para as f64));
    js_set(&o, "byte", &JsValue::from_f64(byte as f64));
    o
}

fn rect_js(x: f64, y: f64, w: f64, h: f64) -> js_sys::Object {
    let o = js_sys::Object::new();
    for (k, v) in [("x", x), ("y", y), ("w", w), ("h", h)] {
        js_set(&o, k, &JsValue::from_f64(v));
    }
    o
}

fn object_hit_js(surface: usize, kind: &str, key: &str, x: f64, y: f64, w: f64, h: f64) -> JsValue {
    let o = js_sys::Object::new();
    js_set(&o, "surface", &JsValue::from_f64(surface as f64));
    js_set(&o, "kind", &JsValue::from_str(kind));
    js_set(&o, "key", &JsValue::from_str(key));
    js_set(&o, "rect", &rect_js(x, y, w, h));
    o.into()
}

// ─── The native setter cores (child-module access to the private state cells;
//     natively testable — the JsValue shells below only parse + delegate) ─────

impl Viewer {
    /// Route a selection to its state cell BY KIND, clearing the sibling kinds
    /// so the engine cells mirror the TS protocol (the selection kinds are
    /// mutually exclusive; `None` clears all three). Same cells, same
    /// normalization, same `.max(1)` anchor floor as the legacy setters —
    /// `None` ⇒ every selection paint pass is skipped ⇒ byte-identical render.
    pub(crate) fn doc_set_selection_core(&self, sel: Option<DocSelectionArg>) {
        // Clear-all first: every arm overwrites its own cell after, so a set is
        // "clear siblings + set mine" and `None` is just the clear.
        self.selection.set(None);
        #[cfg(feature = "sml")]
        self.cell_selection.set(None);
        #[cfg(feature = "pml")]
        self.pml_selection.set(None);
        match sel {
            None => {}
            Some(DocSelectionArg::Text { start, end }) => {
                // Normalize start <= end via TextPos's lexicographic Ord. A
                // collapsed range (caret) is a valid Some that paints nothing.
                self.selection.set(Some(if end < start { (end, start) } else { (start, end) }));
            }
            #[cfg(feature = "sml")]
            Some(DocSelectionArg::Cells { sheet, range, anchor_row, anchor_col }) => {
                self.cell_selection.set(Some(crate::sml::select::CellSelection {
                    sheet,
                    range,
                    anchor_row: anchor_row.max(1),
                    anchor_col: anchor_col.max(1),
                }));
            }
            #[cfg(feature = "pml")]
            Some(DocSelectionArg::PmlText { sel }) => {
                // Normalize start <= end lexicographically over (para, byte).
                let a = (sel.start_para, sel.start_byte);
                let b = (sel.end_para, sel.end_byte);
                let (s, e) = if b < a { (b, a) } else { (a, b) };
                self.pml_selection.set(Some(crate::pml::select::PmlSelection {
                    start_para: s.0,
                    start_byte: s.1,
                    end_para: e.0,
                    end_byte: e.1,
                    ..sel
                }));
            }
        }
    }

    /// Route an object selection to its state cell by the LOADED doc kind (a
    /// PML picture key needs its slide; WML/SML outlines are keyed by the
    /// relId/chart-key alone), clearing the sibling cell. `None` clears both.
    pub(crate) fn doc_set_selected_object_core(&self, obj: Option<(usize, String)>) {
        *self.selected_object.borrow_mut() = None;
        #[cfg(feature = "pml")]
        {
            *self.pml_selected_object.borrow_mut() = None;
        }
        let Some((surface, key)) = obj else { return };
        if key.is_empty() {
            return; // empty key = nothing to outline (legacy clear signal)
        }
        #[cfg(feature = "pml")]
        if matches!(self.loaded, LoadedDoc::Pml(_)) {
            *self.pml_selected_object.borrow_mut() = Some((surface, key));
            return;
        }
        let _ = surface; // WML/SML: an outline only paints on the key's own page/sheet
        *self.selected_object.borrow_mut() = Some(key);
    }
}

// ─── The six unified exports ─────────────────────────────────────────────────

#[wasm_bindgen]
impl Viewer {
    /// Unified hit-test across all three formats. Maps a `DocLoc`-addressed
    /// point to a `DocHit` (see the module doc for both wire shapes) or `null`
    /// on a miss / an unaddressable loc / a scroll-less loc on a spreadsheet.
    /// A pure READ. SYNC (the worker dispatcher does not await).
    #[wasm_bindgen(js_name = docHitTest)]
    pub fn doc_hit_test(&self, loc: JsValue, x: f64, y: f64, granularity: &str) -> JsValue {
        let Some(loc) = parse_loc(&loc) else { return JsValue::NULL };
        match &self.loaded {
            LoadedDoc::Wml(_) => {
                use crate::wml::select::{hit_test_impl, word_bounds, Granularity};
                let pages = self.pages.borrow();
                let Some(pg) = pages.get(loc.surface) else { return JsValue::NULL };
                let Some(pos) = hit_test_impl(pg, x, y) else { return JsValue::NULL };
                let obj = js_sys::Object::new();
                js_set(&obj, "kind", &JsValue::from_str("text"));
                js_set(&obj, "surface", &JsValue::from_f64(loc.surface as f64));
                js_set(&obj, "pos", &textpos_js(pos.para, pos.byte));
                // Word/paragraph expansion against the captured logical text
                // boundaries live HERE, never recomputed in TS.
                let range = match Granularity::from_str(granularity) {
                    Granularity::Char => None,
                    Granularity::Word => {
                        let meta = self.para_meta.borrow();
                        let lt = meta.get(pos.para as usize).map(|m| m.logical_text.as_str()).unwrap_or("");
                        Some(word_bounds(lt, pos.byte as usize))
                    }
                    Granularity::Paragraph => {
                        let meta = self.para_meta.borrow();
                        let end = meta.get(pos.para as usize).map(|m| m.logical_text.len()).unwrap_or(0);
                        Some((0usize, end))
                    }
                };
                if let Some((s, e)) = range {
                    // range endpoints share `pos.para` — word/paragraph never
                    // cross paragraphs.
                    let range_obj = js_sys::Object::new();
                    js_set(&range_obj, "start", &textpos_js(pos.para, s as u32));
                    js_set(&range_obj, "end", &textpos_js(pos.para, e as u32));
                    js_set(&obj, "range", &range_obj);
                }
                obj.into()
            }
            #[cfg(feature = "sml")]
            LoadedDoc::Sml(s) => {
                // Grid content space only — a scroll-less loc is a page-space
                // probe, unsupported on a spreadsheet (see module doc).
                let Some((sx, sy)) = loc.scroll else { return JsValue::NULL };
                let Some(hit) = s.grid_hit_test(loc.surface, x, y, sx, sy, &self.font_cache) else {
                    return JsValue::NULL;
                };
                let obj = js_sys::Object::new();
                js_set(&obj, "kind", &JsValue::from_str("cell"));
                js_set(&obj, "surface", &JsValue::from_f64(loc.surface as f64));
                js_set(&obj, "zone", &JsValue::from_str(hit.zone.as_str()));
                js_set(&obj, "row", &JsValue::from_f64(hit.row as f64));
                js_set(&obj, "col", &JsValue::from_f64(hit.col as f64));
                if let Some((ar, ac)) = hit.merge_anchor {
                    let ma = js_sys::Object::new();
                    js_set(&ma, "row", &JsValue::from_f64(ar as f64));
                    js_set(&ma, "col", &JsValue::from_f64(ac as f64));
                    js_set(&obj, "mergeAnchor", &ma);
                }
                obj.into()
            }
            #[cfg(feature = "pml")]
            LoadedDoc::Pml(_) => {
                // Clamp: `loc.clamp` pins resolution to that target (a drag
                // never escapes its anchor shape/cell); absent = topmost wins.
                let pages = self.pml_pages.borrow();
                let Some(pg) = pages.get(loc.surface) else { return JsValue::NULL };
                let Some(hit) =
                    crate::pml::select::hit_test(pg, loc.surface, x, y, loc.clamp, &self.font_cache)
                else {
                    return JsValue::NULL;
                };
                let (kind, a, b) = hit.target.to_wire();
                let obj = js_sys::Object::new();
                js_set(&obj, "kind", &JsValue::from_str("text"));
                js_set(&obj, "surface", &JsValue::from_f64(hit.slide as f64));
                let target = js_sys::Object::new();
                js_set(&target, "targetKind", &JsValue::from_f64(kind as f64));
                js_set(&target, "targetA", &JsValue::from_f64(a as f64));
                js_set(&target, "targetB", &JsValue::from_f64(b as f64));
                js_set(&obj, "target", &target);
                js_set(&obj, "pos", &textpos_js(hit.para as u32, hit.byte as u32));
                obj.into()
            }
            _ => JsValue::NULL,
        }
    }

    /// Unified object hit-test (object-first z-order): the topmost
    /// image/chart/picture under the point as a `DocObjectHit`, or `null`.
    /// A pure READ. SYNC.
    #[wasm_bindgen(js_name = docHitTestObject)]
    pub fn doc_hit_test_object(&self, loc: JsValue, x: f64, y: f64) -> JsValue {
        let Some(loc) = parse_loc(&loc) else { return JsValue::NULL };
        match &self.loaded {
            LoadedDoc::Wml(_) => {
                use crate::wml::select::hit_test_object_impl;
                let pages = self.pages.borrow();
                let Some(pg) = pages.get(loc.surface) else { return JsValue::NULL };
                // Use the RETAINED image-rel set (not `self.images`): the raw
                // bytes are freed by `clearImageBytes` in the post-decode steady
                // state, but the KNOWLEDGE of which rels are images survives.
                let image_rels = self.image_rel_ids.borrow();
                let charts = self.chart_xml.borrow();
                let is_image = |rel: &str| image_rels.contains(rel) && !charts.contains_key(rel);
                let is_chart = |rel: &str| charts.contains_key(rel);
                let Some(hit) = hit_test_object_impl(pg, x, y, &is_image, &is_chart) else {
                    return JsValue::NULL;
                };
                object_hit_js(loc.surface, hit.kind, &hit.rel_id, hit.rect.x, hit.rect.y, hit.rect.w, hit.rect.h)
            }
            #[cfg(feature = "sml")]
            LoadedDoc::Sml(s) => {
                let Some((sx, sy)) = loc.scroll else { return JsValue::NULL };
                let Some((key, cx, cy, cw, ch)) =
                    s.grid_chart_hit(loc.surface, x, y, sx, sy, &self.font_cache)
                else {
                    return JsValue::NULL;
                };
                object_hit_js(loc.surface, "chart", &key, cx, cy, cw, ch)
            }
            #[cfg(feature = "pml")]
            LoadedDoc::Pml(_) => {
                let pages = self.pml_pages.borrow();
                let Some(pg) = pages.get(loc.surface) else { return JsValue::NULL };
                let Some(hit) = crate::pml::select::hit_test_object(pg, x, y) else {
                    return JsValue::NULL;
                };
                object_hit_js(loc.surface, "picture", &hit.key, hit.rect.x, hit.rect.y, hit.rect.w, hit.rect.h)
            }
            _ => JsValue::NULL,
        }
    }

    /// Unified selection rects: the highlight rectangles (layout px) a
    /// `DocSelection` covers on the `loc`-addressed surface. `[]` for a cells
    /// selection — the grid paints its selection fully in-engine
    /// (`cell_sel_paint` inside the grid paint), so there is nothing to
    /// return. Endpoints pass through RAW (normalization is the per-format
    /// impl's concern, unchanged). A pure READ. SYNC.
    #[wasm_bindgen(js_name = docSelectionRects)]
    pub fn doc_selection_rects(&self, loc: JsValue, sel: JsValue) -> js_sys::Array {
        let arr = js_sys::Array::new();
        let Some(loc) = parse_loc(&loc) else { return arr };
        let Some(sel) = parse_selection(&sel) else { return arr };
        match (&self.loaded, sel) {
            (LoadedDoc::Wml(_), DocSelectionArg::Text { start, end }) => {
                use crate::wml::select::selection_rects_impl;
                let pages = self.pages.borrow();
                let Some(pg) = pages.get(loc.surface) else { return arr };
                for r in selection_rects_impl(pg, start, end) {
                    arr.push(&rect_js(r.x, r.y, r.w, r.h));
                }
                arr
            }
            #[cfg(feature = "pml")]
            (LoadedDoc::Pml(_), DocSelectionArg::PmlText { sel }) => {
                // The selection carries its own slide scope; `loc` addresses
                // the same surface for a shipped caller.
                let pages = self.pml_pages.borrow();
                let Some(pg) = pages.get(sel.slide) else { return arr };
                for r in crate::pml::select::selection_rects(pg, &sel, &self.font_cache) {
                    arr.push(&rect_js(r.x, r.y, r.w, r.h));
                }
                arr
            }
            _ => arr,
        }
    }

    /// Set (or clear) THE selection (fire-and-forget, SYNC `&self` interior
    /// mutability). `sel` is a `DocSelection` or `null` — `null` clears every
    /// selection kind (ONE clear replaces the three legacy sentinel
    /// conventions), and setting one kind clears the siblings (the kinds are
    /// mutually exclusive). Does NOT self-repaint: the TS plugin drives a
    /// repaint on its `selection:changed` edge, and the next paint draws the
    /// highlight. `None` ⇒ the selection paint passes are skipped ⇒
    /// byte-identical render.
    #[wasm_bindgen(js_name = docSetSelection)]
    pub fn doc_set_selection(&self, sel: JsValue) {
        // A malformed selection parses to `None` and clears — the fail-safe the
        // legacy sentinel wires had (an unknown target / negative index cleared).
        self.doc_set_selection_core(parse_selection(&sel));
    }

    /// Set (or clear) the selected OBJECT (image / chart / picture) so the
    /// NEXT paint draws its outline (fire-and-forget, SYNC). `obj` is
    /// `{ surface, key } | null`; `null` clears BOTH object cells (WML/SML and
    /// PML). `None` ⇒ the outline pass is skipped ⇒ byte-identical render.
    #[wasm_bindgen(js_name = docSetSelectedObject)]
    pub fn doc_set_selected_object(&self, obj: JsValue) {
        let parsed = if obj.is_object() {
            match (get_f64(&obj, "surface"), get_str(&obj, "key")) {
                (Some(s), Some(key)) if s >= 0.0 => Some((s as usize, key)),
                _ => None,
            }
        } else {
            None
        };
        self.doc_set_selected_object_core(parsed);
    }

    /// Unified extract across all three formats: the flavor-neutral
    /// fragment for a `DocSelection`. A text selection yields the `DocFragment`
    /// schema (document-scoped for WML; target-scoped for PML — the selection
    /// carries its own scope, so there is NO loc argument); a cells selection
    /// yields the `CellFragment` schema (merge-expanded, copy-capped).
    /// `null` for a selection kind the loaded doc cannot serve. ONE synchronous
    /// pass (finishes inside the copy-event window). A pure READ. SYNC.
    #[wasm_bindgen(js_name = docExtractRange)]
    pub fn doc_extract_range(&self, sel: JsValue) -> JsValue {
        let Some(sel) = parse_selection(&sel) else { return JsValue::NULL };
        match (&self.loaded, sel) {
            (LoadedDoc::Wml(_), DocSelectionArg::Text { start, end }) => {
                use crate::wml::select::extract_range_impl;
                // A pure READ over the layout-time para/table side tables; never
                // walks the paged/visual output, so a byte range maps to a
                // contiguous LOGICAL slice.
                let paras = self.para_meta.borrow();
                let tables = self.table_meta.borrow();
                crate::frag_to_jsvalue(&extract_range_impl(&paras, &tables, start, end))
            }
            #[cfg(feature = "sml")]
            (LoadedDoc::Sml(s), DocSelectionArg::Cells { sheet, range, .. }) => {
                match s.extract_cell_range(sheet as usize, range, &self.font_cache) {
                    Some(frag) => crate::cell_fragment_to_jsvalue(&frag),
                    None => JsValue::NULL,
                }
            }
            #[cfg(feature = "pml")]
            (LoadedDoc::Pml(_), DocSelectionArg::PmlText { sel }) => {
                let pages = self.pml_pages.borrow();
                let Some(pg) = pages.get(sel.slide) else { return JsValue::NULL };
                crate::frag_to_jsvalue(&crate::pml::select::extract_range(pg, &sel, &self.font_cache))
            }
            _ => JsValue::NULL,
        }
    }
}

// ─── Native pins for the setter cores (the reads' pins live on the per-format
//     `*_impl` functions, unchanged by this seam) ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_selection_normalizes_via_textpos_ord() {
        let v = Viewer::new().expect("Viewer::new");
        // Passed end-before-start; must store normalized start <= end.
        v.doc_set_selection_core(Some(DocSelectionArg::Text {
            start: TextPos::new(1, 5),
            end: TextPos::new(0, 3),
        }));
        assert_eq!(
            v.selection.get(),
            Some((TextPos::new(0, 3), TextPos::new(1, 5))),
            "selection must be normalized (start <= end) via TextPos Ord"
        );
    }

    /// Relocated pin for the deleted `set_selection_negative_para_clears`. The
    /// legacy engine took plain `i32`s and cleared on a negative index; the
    /// unified verb takes a `JsValue`, so the fail-safe now lives in the wire
    /// validation and is pinned there. Without the `>= 0` filters `as u32`
    /// saturates and a −3 para STORES a selection at para 0 — the opposite of
    /// what `doc_set_selection`'s docblock promises.
    #[test]
    fn negative_or_nan_wire_textpos_clears() {
        assert_eq!(textpos_from_wire(Some(-3.0), Some(0.0)), None, "negative para must clear");
        assert_eq!(textpos_from_wire(Some(0.0), Some(-1.0)), None, "negative byte must clear");
        assert_eq!(textpos_from_wire(Some(f64::NAN), Some(0.0)), None, "NaN para must clear");
        assert_eq!(textpos_from_wire(Some(0.0), Some(f64::NAN)), None, "NaN byte must clear");
        assert_eq!(textpos_from_wire(None, Some(0.0)), None, "a missing para must clear");
        assert_eq!(
            textpos_from_wire(Some(2.0), Some(7.0)),
            Some(TextPos::new(2, 7)),
            "a valid wire position still parses"
        );
    }

    #[test]
    fn none_clears_every_selection_kind() {
        let v = Viewer::new().expect("Viewer::new");
        v.doc_set_selection_core(Some(DocSelectionArg::Text {
            start: TextPos::new(0, 0),
            end: TextPos::new(2, 10),
        }));
        assert!(v.selection.get().is_some(), "a real set should store Some");
        v.doc_set_selection_core(None);
        assert_eq!(v.selection.get(), None, "None must clear the text selection");
        #[cfg(feature = "sml")]
        assert_eq!(v.cell_selection.get(), None, "None must clear the cell selection");
        #[cfg(feature = "pml")]
        assert_eq!(v.pml_selection.get(), None, "None must clear the PML selection");
    }

    #[test]
    fn collapsed_text_selection_stores_some_caret() {
        let v = Viewer::new().expect("Viewer::new");
        // A collapsed range (caret) is retained as Some — NOT coerced to None.
        // It paints nothing because selection_rects_impl yields zero rects for
        // a zero-width range (reserved for caret use).
        v.doc_set_selection_core(Some(DocSelectionArg::Text {
            start: TextPos::new(2, 7),
            end: TextPos::new(2, 7),
        }));
        assert_eq!(
            v.selection.get(),
            Some((TextPos::new(2, 7), TextPos::new(2, 7))),
            "collapsed caret must be retained as Some"
        );
    }

    #[cfg(feature = "sml")]
    #[test]
    fn setting_one_kind_clears_the_siblings() {
        use crate::sml::select::CellRange;
        let v = Viewer::new().expect("Viewer::new");
        v.doc_set_selection_core(Some(DocSelectionArg::Text {
            start: TextPos::new(0, 0),
            end: TextPos::new(1, 1),
        }));
        v.doc_set_selection_core(Some(DocSelectionArg::Cells {
            sheet: 0,
            range: CellRange::new(1, 1, 2, 2),
            anchor_row: 1,
            anchor_col: 1,
        }));
        assert_eq!(v.selection.get(), None, "a cells set must clear the text selection");
        let cells = v.cell_selection.get().expect("cells stored");
        assert_eq!((cells.range.r0, cells.range.c0, cells.range.r1, cells.range.c1), (1, 1, 2, 2));
        // The anchor floor (`.max(1)`) is preserved from the legacy setter.
        v.doc_set_selection_core(Some(DocSelectionArg::Cells {
            sheet: 0,
            range: CellRange::new(0, 0, 1, 1),
            anchor_row: 0,
            anchor_col: 0,
        }));
        let cells = v.cell_selection.get().expect("cells stored");
        assert_eq!((cells.anchor_row, cells.anchor_col), (1, 1), "anchor floors to 1-based");
    }

    #[test]
    fn object_core_sets_and_clears_the_wml_cell() {
        let v = Viewer::new().expect("Viewer::new");
        // No doc loaded ⇒ the WML/SML arm (PML routing needs LoadedDoc::Pml).
        v.doc_set_selected_object_core(Some((0, "rId7".to_string())));
        assert_eq!(v.selected_object.borrow().as_deref(), Some("rId7"));
        v.doc_set_selected_object_core(None);
        assert!(v.selected_object.borrow().is_none(), "None must clear the object cell");
        // An empty key is the legacy clear signal — never stored.
        v.doc_set_selected_object_core(Some((0, String::new())));
        assert!(v.selected_object.borrow().is_none(), "empty key must not store");
    }
}
