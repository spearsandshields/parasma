//! parasma-wasm — wasm-bindgen entry crate.
//!
//! Owns the document model and exposes a typed surface to the
//! `@parasma/core` TypeScript package.
//!
//! Per-page rendering:
//!   * `<w:sectPr>` is read from the document body; page size + margins flow
//!     through to the content rectangle. US Letter is the default.
//!   * Paragraphs are laid out into break-bounded items and packed into
//!     lines.
//!   * Lines are stacked onto fixed-size pages.
//!   * `run_layout()` streams page-ready events to JS with a task yield
//!     (`yield_to_browser`) between every page so the browser stays responsive.
//!   * `render_page(idx, canvas)` draws a specific page into a specific
//!     canvas — the consumer attaches a canvas per visible page.

mod chart;
// Windowed-render culling: the web-sys-free `Rect`/`keep` predicate the
// `render_page_window` paint path uses to skip items outside the viewport
// (O(visible) item walk). `render_page` threads `clip: None` through the same
// painters → byte-identical full-page output.
mod cull;
mod engine;
mod field;
// Consolidated parser-repair stopgap (dispatch-on-FAILURE, mirrors sml/shim.rs):
// makes real-but-technically-malformed Office files PARSE by byte-rewriting the
// offending package part BEFORE the fork parses, ONLY on a specific failure.
// RULE 1 (sml <col>), RULE 2 (wml pgMar gutter), RULE 3 (pml custGeom). Each is a
// deliberate workaround, removed by making the fork codegen lenient (which
// deletes this module). See src/repair/mod.rs.
mod repair;
// Decompression-bomb guard: every loader meters the package's real inflated
// size before the first parse. See src/package_limits.rs.
mod package_limits;
// The unified selection/hit/extract verb set (`docHitTest`,
// `docSetSelection`, …): ONE format-neutral wire over the per-format
// `wml/sml/pml::select` internals. The `#[wasm_bindgen] impl Viewer` block in
// there is the ONLY selection FFI surface; the setter cores are natively
// pinned in its tests.
mod select_dispatch;
mod wml;
// Excel (SpreadsheetML) renderer core. Web-sys-free grid model + geometry,
// natively tested; reached from the Viewer facade through `LoadedDoc::Sml`.
#[cfg(feature = "sml")]
mod sml;
// PowerPoint (PresentationML) renderer core: parses a .pptx, lays out one
// page per slide, and paints its shapes, text, pictures, tables and charts.
// Reached through `LoadedDoc::Pml`. Gated behind the `pml` feature so docx-only
// builds never link ooxml-pml (verified by check-wasm-identity.sh).
#[cfg(feature = "pml")]
mod pml;
// Enhanced Metafile (MS-EMF) vector renderer for inline `image/x-emf` pictures.
// Gated behind the `emf` feature so docx-only builds never compile emf/ — a
// build WITHOUT the feature is byte-identical (verified by check-wasm-identity.sh).
#[cfg(feature = "emf")]
mod emf;
#[cfg(test)]
mod oracle_tests;
// Test-only counting allocator: ground truth for the memory census
// (`memory_census`, oracle_tests.rs). Wraps System with current/peak byte
// counters. Process-global, so the census test must run ALONE
// (--test-threads=1 + a filter) or its numbers include sibling tests.
#[cfg(test)]
mod alloc_census {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    pub static CURRENT: AtomicUsize = AtomicUsize::new(0);
    pub static PEAK: AtomicUsize = AtomicUsize::new(0);
    pub struct Counting;
    fn add(n: usize) {
        let c = CURRENT.fetch_add(n, Relaxed) + n;
        PEAK.fetch_max(c, Relaxed);
    }
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let p = unsafe { System.alloc(l) };
            if !p.is_null() { add(l.size()); }
            p
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            let p = unsafe { System.alloc_zeroed(l) };
            if !p.is_null() { add(l.size()); }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            CURRENT.fetch_sub(l.size(), Relaxed);
            unsafe { System.dealloc(p, l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            let q = unsafe { System.realloc(p, l, new) };
            if !q.is_null() {
                CURRENT.fetch_sub(l.size(), Relaxed);
                add(new);
            }
            q
        }
    }
    /// Peak restarts from the CURRENT level (not zero) so `peak - snapshot`
    /// measures the phase you bracket, not the process's history.
    pub fn reset_peak() {
        PEAK.store(CURRENT.load(Relaxed), Relaxed);
    }
}
#[cfg(test)]
#[global_allocator]
static ALLOC_CENSUS: alloc_census::Counting = alloc_census::Counting;
// The one harness that measures agreement with WORD rather than engine
// self-consistency. Keyed by opaque calibration ID; names no document.
#[cfg(test)]
mod ground_truth;

use engine::color::{apply_tint_shade, highlight_color_to_css, ooxml_color_to_css};
use engine::fonts::{
    parse_theme_fonts, resolve_font_with_fallbacks,
    text_has_complex, text_has_east_asia, DEFAULT_COMPLEX_FAMILY, DEFAULT_EAST_ASIA_FAMILY,
    DEFAULT_FONT_FAMILY, DEFAULT_FONT_PT, FontCache, ThemeFonts,
};
use engine::images::{render_anchored_image, ImageBytes};
use engine::units::{pt_to_px, twips_to_px};
use wml::border::BorderEdge;
use wml::drawing::{WrapInfo, WrapMode};
use wml::float::{
    build_active_float, clear_below_floats, min_float_top, pack_lines_around_floats,
    ActiveFloat, AnchorVRelFrom, WRAP_EPS,
};
use wml::float_resolve::{resolve_anchor_position, FrameCtx};
use wml::hdr_ftr::{
    flatten_blocks, layout_section_hdr_ftr, position_hdr_ftr_lines, FlatBlock, LaidOutHdrFtr,
};
use wml::linebreak::{
    author_color, comment_anchor_name, justify_line, pack_lines, LineSegment, OPEN_COMMENT_IDS,
};
use wml::loader::{
    build_numbering_lookup, build_sections, extract_core_date, extract_hdr_ftr_images,
    inject_builtin_hdr_ftr_styles, Section,
};
// Re-export so `crate::Item` keeps resolving for `wml::bidi` and `wml::float`
// (a canary: bidi.rs's `use crate::Item;` must compile unedited) and for
// lib.rs's own page-assembly/render references.
pub(crate) use wml::linebreak::Item;
use wml::numbering::{resolve_numbering_marker, NumberingLevelDef};
// Re-export the page-model leaf types whose `crate::X` imports live in sibling
// modules (canaries): `wml::table` → `CellBlock`; `wml::linebreak` →
// `PagedLine`; `engine::images` → `AnchoredImageLayout`. lib.rs's own
// page-assembly/render/dbg code uses the rest directly.
pub(crate) use wml::page::{AnchoredImageLayout, CellBlock, PagedLine, PARA_SENTINEL};
use wml::page::{
    BlockCache, PageElement, PageLayout, PagedTableCell, PagedTableRow,
    PositionedTextBox,
};
// Re-export paragraph-cluster symbols at the crate root so the unedited
// `crate::X` imports in sibling modules keep resolving (canaries):
// `wml::table` → `layout_paragraph`; `wml::numbering` →
// `collect_paragraph_style_chain`; `wml::linebreak` →
// `collect_paragraph_style_rprs`/`ResolvedTabStops`/`TabAlignment`/`TabLeader`.
// lib.rs's own page-assembly/render code also uses several of these.
pub(crate) use wml::paragraph::{
    collect_paragraph_style_chain, collect_paragraph_style_rprs, layout_paragraph,
    ParagraphCache, ResolvedTabStops, TabAlignment, TabLeader,
};
use wml::render::{layout_text_box, render_line, render_table_row, render_text_box, substitute_page_fields};
use wml::section::{PageGeometry, SectionBreakType};
use wml::table::{
    collect_cell_paragraphs, collect_row_cells, layout_table, CellFloatDraw, CellVAlign,
};
use wml::watermark::{find_watermark_in_blocks, Watermark};
use engine::paint::{RunStyle, Underline, VertAlign};

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageBitmap};

use crate::engine::paint::Painter;

use ooxml_wml::ext::{
    DrawingChartExt, DrawingExt, MathExt, ParagraphExt, RunExt,
    StyleContext, TableExt,
};
// `BodyExt`/`SectionPropertiesExt` are needed only by the `#[cfg(test)]`
// footer-id probe below (`doc.body().section_properties()`); the section
// loader that used them in non-test code moved to `wml::loader`.
#[cfg(test)]
use ooxml_wml::ext::{BodyExt, SectionPropertiesExt};
use ooxml_wml::types::{
    BlockContent, FootnoteEndnote, STJc,
    STJcTable,
};

const CSS_FONT_FALLBACK: &str = "-apple-system, BlinkMacSystemFont, sans-serif";

/// Default layout memory budget (2 GiB). PROVISIONAL; see the
/// `memory_budget_bytes` field doc for the derivation and why the value
/// may still change.
const DEFAULT_MEMORY_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Default text colour when a run has no explicit `<w:color>`. Picked for
/// dark text on the white page background.
pub(crate) const DEFAULT_TEXT_COLOR: &str = "#1f2328";

/// Alpha for the comment-highlight additive paint pass. Bounded so
/// body text keeps a 4.5:1 contrast ratio over the densest expected shading;
/// the per-author colour is the engine suggestion, this alpha is the canvas
/// baseline. The DOM/TS layer owns any richer active-highlight emphasis.
const COMMENT_FILL_ALPHA: f64 = 0.30;

/// Search-highlight paint constants (full-text search). A SEPARATE
/// additive translucent pass after the comment pass — every match gets the base
/// swipe (`SEARCH_FILL_*`); the single ACTIVE match (the one next/prev focuses)
/// gets a hotter colour + alpha so it reads as the current hit, mirroring
/// Word's Find. Bounded like the comment alpha so body text stays legible
/// underneath. The DOM/TS layer owns query state and active-match identity; this
/// canvas pass is the persistent baseline.
const SEARCH_FILL_ALPHA: f64 = 0.40;
const SEARCH_FILL_COLOR: &str = "#FFE066";
const SEARCH_ACTIVE_ALPHA: f64 = 0.55;
const SEARCH_ACTIVE_COLOR: &str = "#FF8C00";

// US Letter portrait (Word's default in en-US locale) — used when <w:sectPr>
// is absent or doesn't specify page size / margins. Twips.
pub(crate) const DEFAULT_PAGE_WIDTH_TWIPS: i64 = 12240; // 8.5 in
pub(crate) const DEFAULT_PAGE_HEIGHT_TWIPS: i64 = 15840; // 11 in
pub(crate) const DEFAULT_MARGIN_TWIPS: i64 = 1440; // 1 in on every side
pub(crate) const DEFAULT_HEADER_MARGIN_TWIPS: i64 = 720; // 0.5 in from page edge
pub(crate) const DEFAULT_FOOTER_MARGIN_TWIPS: i64 = 720;

// Word's default cell margins: 0 top/bottom, 108 twips (0.15") left/right.
pub(crate) const DEFAULT_CELL_MARGIN_H_TWIPS: i64 = 108;
pub(crate) const DEFAULT_CELL_MARGIN_V_TWIPS: i64 = 0;

/// Platform-split console logging. On wasm32 this is the browser console; on
/// native (the oracle/probe harness) it is stderr — `web_sys` externs PANIC
/// off-wasm32 ("function not implemented on non-wasm32 targets"), which made a
/// mere image-extract WARNING abort the native probe and masquerade as a load
/// failure (corpus: tdf115883 — the file loads fine in the browser).
pub(crate) fn console_warn(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    web_sys::console::warn_1(&msg.into());
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("[parasma warn] {msg}");
}

/// See [`console_warn`].
pub(crate) fn console_log(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    web_sys::console::log_1(&msg.into());
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("[parasma] {msg}");
}

/// Install a panic hook so a wasm trap reports its MESSAGE and SOURCE LOCATION
/// instead of an opaque `RuntimeError: unreachable`.
///
/// Why this matters: a panic in wasm aborts the instance. Without a hook there
/// is no message, no location, and — critically — **nothing distinguishes a trap
/// from a hang**, which is exactly the ambiguity that made the confirmed
/// crash/hang vectors (gridSpan="-1", w:sz="0", the zero-line-height float loop)
/// indistinguishable from the outside. Every one of those is reachable from a
/// hostile document, so this is the diagnostic floor the other fixes sit on.
///
/// WASM32 ONLY, deliberately — same rationale as [`console_warn`]'s platform
/// split, and the stakes are higher here: the native oracle/probe harness
/// *relies* on the default hook printing to stderr and the panic failing the
/// test. Overriding it natively would silence `probe_open_arbitrary_docx`, i.e.
/// blind the very harness we use to confirm these vectors.
///
/// Hand-rolled rather than pulling in `console_error_panic_hook`: the message +
/// location is the whole diagnostic value, `PanicHookInfo`'s `Display` already
/// formats exactly that, and a JS-stack backtrace through wasm is mostly noise.
/// A dependency would also cost the (already over-budget) wasm size for output
/// the browser console gives us for free.
#[cfg(target_arch = "wasm32")]
fn install_panic_hook() {
    use std::sync::Once;
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            // PanicHookInfo's Display is "panicked at <file>:<line>:<col>:\n<msg>".
            // The [parasma PANIC] tag is the string the TS layer looks for when
            // reporting an engine trap, so keep it stable.
            web_sys::console::error_1(&format!("[parasma PANIC] {info}").into());
        }));
    });
}

#[wasm_bindgen(start)]
fn start() {
    #[cfg(target_arch = "wasm32")]
    install_panic_hook();
    console_log("parasma-wasm loaded");
}

#[wasm_bindgen(js_name = parserInfo)]
pub fn parser_info() -> String {
    // sml is reported only when the (future) `sml` feature links it — this was
    // the sole unconditional ooxml_sml reference, which forced the crate into
    // every docx-only binary (feature-off must truly exclude it).
    #[cfg(feature = "sml")]
    let sml = std::any::type_name::<ooxml_sml::types::Workbook>();
    #[cfg(not(feature = "sml"))]
    let sml = "(not linked)";
    // pml is reported only when the `pml` feature links ooxml-pml:
    // feature-off must truly exclude the crate so docx-only stays byte-identical.
    #[cfg(feature = "pml")]
    let pml = std::any::type_name::<ooxml_pml::types::Slide>();
    #[cfg(not(feature = "pml"))]
    let pml = "(not linked)";
    format!(
        "linked: sml={}, pml={}, wml={}, dml={}",
        sml,
        pml,
        std::any::type_name::<ooxml_wml::types::Document>(),
        std::any::type_name::<ooxml_dml::types::TextBody>(),
    )
}

// ─── Viewer ─────────────────────────────────────────────────────────────────

/// Which document (if any) the viewer holds. The facade dispatch point for
/// multi-format support: each
/// format owns a renderer struct holding its format-specific state; engine
/// state (fonts, images, pages, abort token) stays shared on `Viewer`.
/// Every dispatch site matches on the renderer variant explicitly.
// Renderer variants differ greatly in size; boxing the large variant is a
// deferred off-thread-enablement change (touches every dispatch match arm).
#[allow(clippy::large_enum_variant)]
enum LoadedDoc {
    None,
    Wml(WmlRenderer),
    /// Excel (SpreadsheetML). The renderer owns the workbook +
    /// tolerant-loaded sheets; its paginated pages live in the Viewer's shared
    /// `sml_pages` (borrowed shared state — the renderer holds no
    /// back-reference to `Viewer`).
    #[cfg(feature = "sml")]
    Sml(sml::SmlRenderer),
    /// PowerPoint (PresentationML). One slide = one print page; the
    /// renderer owns the parsed presentation and its paginated pages live in the
    /// Viewer's shared `pml_pages` (borrowed shared state, same
    /// discipline as `Sml`).
    #[cfg(feature = "pml")]
    Pml(pml::PmlRenderer),
}

/// One comment's display metadata, pre-extracted from `word/comments.xml` at
/// load time (so the `comments()` accessor needs neither `&mut doc` nor the
/// feature-gated raw `Comment` fields at call time). The range-start `{page,y}`
/// is *not* stored here — it is resolved on demand from the laid-out pages via
/// the `bookmark_position` page-walk, because positions only exist after
/// layout. Named generically (no `kind` enum shipped in v1) so a future
/// search-highlight / track-change annotation can reuse the carrier.
#[derive(Clone)]
pub(crate) struct CommentMeta {
    /// `w:id` — the stable comment id, also the key the range-start anchor
    /// (`comment_anchor_name(id)`) resolves through.
    id: i64,
    author: String,
    /// `w:date` (ISO-8601 string) if present; the TS layer formats it.
    date: Option<String>,
    /// `w:initials` if present.
    initials: Option<String>,
    /// Flattened body text (paragraphs joined by `\n`).
    text: String,
}

/// One comment-highlight rectangle — the post-layout pairing output
/// of `collect_comment_rects`. Layout px. `ids` is the comment-range id set
/// covering this run (one element normally; multiple when ranges overlap);
/// `color` is the per-author suggestion for the first id. Named generically (no
/// `kind`) so a future annotation carrier can reuse it.
struct CommentRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    ids: Vec<i64>,
    color: &'static str,
}

/// One match rectangle (full-text search). Layout px, the same space
/// `CommentRect` lives in. A single logical match that crosses a soft-wrapped
/// display line yields ONE `SearchRect` per line-portion (industry-standard:
/// search matches logical text, highlights paint per display line) — the same
/// multi-line behaviour the comment core already produces.
#[derive(Clone, Copy)]
struct SearchRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// One full-text-search match on a page — the cross-boundary result of
/// `search_page`. `rects` are the per-line-portion highlight rects (≥1; >1 when
/// the match wraps). `y` is the top of the first rect, the scroll target the TS
/// navigation layer routes through `scrollToPage(page, y)`. `sheet` is the
/// Excel tab name (`Some` for SML, `None` for WML's single logical document).
struct SearchMatch {
    page: usize,
    rects: Vec<SearchRect>,
    y: f64,
    sheet: Option<String>,
    /// SML-only navigation aids (`None`/`0.0` for WML & PML, serialized as
    /// `null` / absent so the JS shape is a superset). `cell_ref` is the matched
    /// cell's A1 ref (the active-emphasis identity); `grid_x`/`grid_y` are the
    /// CONTENT-space (gutter-exclusive, pre-zoom) grid coordinates the continuous
    /// grid scroller jumps to — re-sourced from the row/col rails, since the
    /// print-page rect above is laid out on an off-to-the-side print page.
    cell_ref: Option<String>,
    grid_x: f64,
    grid_y: f64,
}

/// Read-only match semantics for `search_page` (full-text search).
/// Default is case-insensitive substring; toggles mirror Word's Find. `scope`
/// is the SML Find "Within": active sheet (default) vs whole workbook — inert
/// for WML (one logical document).
#[derive(Clone, Copy, Default)]
struct SearchOpts {
    case_sensitive: bool,
    whole_word: bool,
    /// SML Find "Within: Workbook" — only consulted by the feature-gated SML
    /// arm, so it is dead in the docx-only build (WML is one logical document).
    #[cfg_attr(not(feature = "sml"), allow(dead_code))]
    workbook: bool,
}

impl SearchOpts {
    /// Parse the JS opts bag (`{ caseSensitive?, wholeWord?, scope? }`) into the
    /// native struct. Absent / non-object → all defaults (case-insensitive,
    /// substring, active-sheet scope). `scope: "workbook"` opts into the
    /// whole-workbook SML search; anything else stays active-sheet.
    fn from_js(opts: &JsValue) -> Self {
        let get_bool = |k: &str| {
            js_sys::Reflect::get(opts, &k.into())
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };
        let workbook = js_sys::Reflect::get(opts, &"scope".into())
            .ok()
            .and_then(|v| v.as_string())
            .is_some_and(|s| s == "workbook");
        SearchOpts {
            case_sensitive: get_bool("caseSensitive"),
            whole_word: get_bool("wholeWord"),
            workbook,
        }
    }
}

/// Collapse an already-lowercased char onto the one representative its case class
/// shares, so case-INSENSITIVE comparison sees identical-looking spellings as equal.
///
/// Lowercasing alone is not enough, because 21 uppercase letters have MORE THAN
/// ONE lowercase form. Sigma is the motivating one: real Greek writes ς (U+03C2)
/// word-finally and σ (U+03C3) elsewhere, and both upper-case to Σ, so a document
/// containing "ΟΔΟΣ" lowers to …σ while one containing "οδος" already holds ς —
/// the same word, two code points, no match either way round. µ/μ, ı/i, ſ/s, the
/// Greek symbol letters ϐϑϕϖϰϱϵ and the Cyrillic historic forms ᲀ-ᲈ are all the
/// same silent-no-match defect.
///
/// Rather than hand-pick letters, the bulk of this table is DERIVED from a rule:
/// fold by round-tripping through uppercase, `lowercase(uppercase(c))`, skipping
/// any char whose case pair is not 1:1. Over the entire image of
/// `char::to_lowercase` — the only inputs this fn ever sees — that rule remaps
/// exactly 23 chars. Three further arms (U+1FD3, U+1FE3, U+FB05) cannot come from
/// the rule, because their uppercase is a multi-char sequence; they are the 1:1
/// `S` lines of CaseFolding.txt, added by hand and pinned individually.
///
/// **Conformance, verified rather than asserted.** With those three arms,
/// [`fold_char`] induces EXACTLY the equivalence relation of the Unicode
/// `Simple_Case_Folding` property (CaseFolding.txt status C+S) — this is the
/// "simple, default Unicode case-insensitive matching" that UTS #18 §1.5 requires
/// at Level 1 (RL1.5), and the rung Firefox's find-in-page occupies. Pinned by
/// `fold_char_matches_vendored_scf` against the vendored CaseFolding.txt
/// C+S lines (testdata/), partitioning every code point: identical but for
/// one deliberate deviation, the Turkic merge in [`fold_char`] below. Note that
/// agreement is up to CHOICE OF REPRESENTATIVE, not code point: 173 code points
/// map to a different VALUE than `scf` does, of which 172 are Cherokee — Unicode
/// folds it UPWARD (U+AB70 → U+13A0) where we fold down, which changes nothing
/// about which strings match. The 173rd is U+0131 ı, and that one IS behavioural:
/// it is the deliberate Turkic merge described in [`fold_char`]. So a naive
/// value-diff against CaseFolding.txt shows 173 "mismatches" and 172 of them are
/// non-bugs — compare PARTITIONS, not mappings.
///
/// Deliberate non-goals, with their standard names, so a later reader does not
/// mistake them for oversights: FULL folding (D144, the `F` lines — ß ≡ ss) and
/// canonical caseless matching (D145, fold over NFD) are the next two rungs and
/// are declined — the D145 decline is deliberate (Word behaves
/// identically; the real cost is the normalized↔raw
/// offset map for highlights, not the tables); accent-insensitivity (é ≡ e, what Chrome gets from an ICU
/// collator at primary strength) is a different sanctioned branch, UTS #10 §11,
/// not an omission. Match Case remains the exact-distinction escape hatch for all
/// of them.
///
/// `fold_lowered_matches_its_derivation` (wml_search_tests.rs) re-derives the
/// table from the rule, catching RULE drift; `fold_char_matches_vendored_scf`
/// pins it to CaseFolding.txt itself, version-locked to rustc's (SPEC drift).
fn fold_lowered(c: char) -> char {
    // ASCII fast path. The lowest arm below is U+00B5, so every char under it is
    // the identity — which covers essentially all real search text. Measured on
    // the fold step: 3.66 → 2.83 µs per 576-char ASCII paragraph (−23%), 5.97 →
    // 5.23 µs for Greek. The bound is NOT a magic number to keep in step by hand:
    // `fold_lowered_matches_its_derivation` re-derives the whole
    // table over every reachable code point, so an arm added below this guard
    // fails that test rather than silently going dead.
    if c < '\u{00B5}' {
        return c;
    }
    match c {
        '\u{00B5}' => '\u{03BC}', // µ MICRO SIGN → μ GREEK SMALL MU
        '\u{0131}' => '\u{0069}', // ı LATIN SMALL DOTLESS I → i
        '\u{017F}' => '\u{0073}', // ſ LATIN SMALL LONG S → s
        '\u{0345}' => '\u{03B9}', // COMBINING GREEK YPOGEGRAMMENI → ι
        '\u{03C2}' => '\u{03C3}', // ς GREEK SMALL FINAL SIGMA → σ
        '\u{03D0}' => '\u{03B2}', // ϐ GREEK BETA SYMBOL → β
        '\u{03D1}' => '\u{03B8}', // ϑ GREEK THETA SYMBOL → θ
        '\u{03D5}' => '\u{03C6}', // ϕ GREEK PHI SYMBOL → φ
        '\u{03D6}' => '\u{03C0}', // ϖ GREEK PI SYMBOL → π
        '\u{03F0}' => '\u{03BA}', // ϰ GREEK KAPPA SYMBOL → κ
        '\u{03F1}' => '\u{03C1}', // ϱ GREEK RHO SYMBOL → ρ
        '\u{03F5}' => '\u{03B5}', // ϵ GREEK LUNATE EPSILON SYMBOL → ε
        '\u{1C80}' => '\u{0432}', // ᲀ CYRILLIC SMALL ROUNDED VE → в
        '\u{1C81}' => '\u{0434}', // ᲁ CYRILLIC SMALL LONG-LEGGED DE → д
        '\u{1C82}' => '\u{043E}', // ᲂ CYRILLIC SMALL NARROW O → о
        '\u{1C83}' => '\u{0441}', // ᲃ CYRILLIC SMALL WIDE ES → с
        '\u{1C84}' => '\u{0442}', // ᲄ CYRILLIC SMALL TALL TE → т
        '\u{1C85}' => '\u{0442}', // ᲅ CYRILLIC SMALL THREE-LEGGED TE → т
        '\u{1C86}' => '\u{044A}', // ᲆ CYRILLIC SMALL TALL HARD SIGN → ъ
        '\u{1C87}' => '\u{0463}', // ᲇ CYRILLIC SMALL TALL YAT → ѣ
        '\u{1C88}' => '\u{A64B}', // ᲈ CYRILLIC SMALL UNBLENDED UK → ꙋ
        '\u{1E9B}' => '\u{1E61}', // ẛ LATIN SMALL LONG S WITH DOT ABOVE → ṡ
        '\u{1FBE}' => '\u{03B9}', // GREEK PROSGEGRAMMENI → ι
        // Not derivable from the round trip (uppercase(U+1FD3) is the THREE-char
        // "Ϊ́"), but 1:1 and in CaseFolding.txt as `1FD3; S; 0390`. ΐ and ΰ each
        // have two identical-looking encodings — the precomposed-with-tonos form
        // Word emits and the NFC-duplicate with oxia — and without these arms the
        // two spellings of the same Greek word do not match each other.
        '\u{1FD3}' => '\u{0390}', // ΐ IOTA W/ DIALYTIKA AND OXIA → …AND TONOS
        '\u{1FE3}' => '\u{03B0}', // ΰ UPSILON W/ DIALYTIKA AND OXIA → …AND TONOS
        '\u{FB05}' => '\u{FB06}', // ﬅ LONG S T LIGATURE → ﬆ ST LIGATURE
        _ => c,
    }
}

/// Case-fold ONE source char for matching: the Turkic pre-map below, then
/// `char::to_lowercase`, then [`fold_lowered`] on each result. This is the SINGLE
/// definition of "case-insensitive equivalence" for search: `find_matches` and
/// `find_matches_any`, needle and haystack alike, all fold through here. The
/// original sigma bug was one rule with two spellings — `str::to_lowercase` on the
/// needle vs `char::to_lowercase` on the haystack — giving opposite answers;
/// routing every side through this one fn makes that divergence structurally
/// impossible, not merely unlikely.
///
/// **The Turkic tailoring, and why it is here rather than in [`fold_lowered`].**
/// Unicode gives dotted/dotless I two sanctioned configurations selected by
/// LOCALE: the default (I ≡ i, ı and İ apart) and the Turkic `T` lines
/// `0049; T; 0131` and `0130; T; 0069`. A document viewer has no reliable locale
/// signal, so we take the UNION — {I, i, ı, İ} is one class — which is a superset
/// of both and the only choice that cannot make text unfindable. ı ≡ i falls out
/// of the derivation rule; İ (U+0130) cannot, because it is not in the image of
/// `char::to_lowercase` and its DEFAULT lowercase is the two chars "i" + U+0307.
/// So it is pre-mapped here, ahead of `to_lowercase` — structurally where Firefox
/// puts the same special case (`GetFoldedcase`: `if (aCh == 0x0130 || aCh ==
/// 0x0131) return 'i';`). Without it, typing "istanbul" finds nothing in a
/// document that spells it "İstanbul" — verified before the fix.
///
/// Declared deviation, the price of that arm: the DECOMPOSED spelling "i" + U+0307
/// no longer matches "İ" (each is now a distinct fold). That spelling is what a
/// non-Turkish-locale lowercasing of İ produces; it is far rarer than İ itself,
/// renders with a visibly doubled dot, and Firefox makes the same trade. Removing
/// a stray U+0307 instead is not an option — it is meaningful in Lithuanian.
///
/// Consequence worth stating plainly: U+0130 was the ONLY char in all of Unicode
/// whose `char::to_lowercase` is longer than one char, so with it pre-mapped this
/// fn is now strictly 1:1 over every code point — pinned exhaustively by
/// `fold_char_is_strictly_one_to_one`. The n:1 loop in `find_matches` is kept
/// anyway: `to_lowercase` only CONTRACTS to yield an iterator, and it is correct
/// for n:1 regardless (see that fn's comment).
fn fold_char(c: char) -> impl Iterator<Item = char> {
    let c = if c == '\u{0130}' { '\u{0069}' } else { c };
    c.to_lowercase().map(fold_lowered)
}

/// Find every occurrence of `needle` in `haystack` honouring the case / whole-
/// word toggles, returning each match's BYTE range `(start, end)` in `haystack`.
/// Case-insensitivity folds both sides through [`fold_char`].
///
/// The byte map is built PER FOLDED CHAR, not per source char: for each folded
/// char we record the original byte range of the source char that produced it, so
/// one source char may contribute several entries all sharing its range. A match
/// over folded chars `[i, j)` maps to original bytes `[start[i], end[j-1]]` —
/// always on original char boundaries. This is deliberately stronger than the fold
/// currently needs: the older map was indexed the other way round, over ORIGINAL
/// chars with a FOLDED index, and PANICKED on `İ` inside the render paint pass.
/// `fold_char` is 1:1 today, so the n:1 case is unreachable — but `to_lowercase`
/// only contracts to return an iterator, so the shape is kept and pinned rather
/// than being re-derived under pressure later.
///
/// What an EXPANDING fold would actually cost is worth recording accurately,
/// because the obvious guess is wrong. It is not memory safety: with this map,
/// folding ß → "ss" returns valid char boundaries and does not panic (fuzzed:
/// 200k randomised cases over aßsİiΣσςμµKk, 37312 ranges, all valid,
/// zero panics). The cost is match SEMANTICS — searching "s" in "Straße" yields
/// `[(0,1), (4,6), (4,6)]`, the same range TWICE, so the UI reports three hits,
/// next/prev stalls on one rect, and the highlight covers a whole ß for half a
/// match. That, not a crash, is why ß ≡ ss is declined here, as it is in Firefox.
///
/// Whole-word gates a hit on its neighbours not being alphanumeric
/// (`char::is_alphanumeric`),
/// mirroring Word's "Find whole words only". Empty `needle` → no matches.
fn find_matches(haystack: &str, needle: &str, opts: SearchOpts) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    // Lowered haystack chars + per-lowered-char original byte range. For case-
    // sensitive search each original char maps 1:1 to itself (single code path).
    let mut hay_chars: Vec<char> = Vec::new();
    let mut start_b: Vec<usize> = Vec::new();
    let mut end_b: Vec<usize> = Vec::new();
    for (b, c) in haystack.char_indices() {
        let (cb, ce) = (b, b + c.len_utf8());
        if opts.case_sensitive {
            hay_chars.push(c);
            start_b.push(cb);
            end_b.push(ce);
        } else {
            for lc in fold_char(c) {
                hay_chars.push(lc);
                start_b.push(cb);
                end_b.push(ce);
            }
        }
    }
    // The needle must be folded the SAME WAY as the haystack above — per CHAR,
    // not per str. `str::to_lowercase` applies Unicode's final-sigma rule
    // (SpecialCasing.txt): a word-final Σ becomes ς (U+03C2), while
    // `char::to_lowercase` always yields σ (U+03C3). Lowering the two sides
    // differently made a .docx containing "ΟΔΟΣ" unfindable by searching for
    // "ΟΔΟΣ" — the haystack held σ and the needle held ς. `find_matches_any`
    // (the SML path) already lowers both sides per char, which is why the same
    // text was findable in an .xlsx and not in a .docx.
    let ndl_chars: Vec<char> = if opts.case_sensitive {
        needle.chars().collect()
    } else {
        needle.chars().flat_map(fold_char).collect()
    };
    let mut out = Vec::new();
    if ndl_chars.is_empty() || ndl_chars.len() > hay_chars.len() {
        return out;
    }
    let mut i = 0usize;
    while i + ndl_chars.len() <= hay_chars.len() {
        if hay_chars[i..i + ndl_chars.len()] == ndl_chars[..] {
            let after_i = i + ndl_chars.len();
            let ok = !opts.whole_word || {
                let before_ok = i == 0 || !hay_chars[i - 1].is_alphanumeric();
                let after_ok =
                    after_i == hay_chars.len() || !hay_chars[after_i].is_alphanumeric();
                before_ok && after_ok
            };
            if ok {
                out.push((start_b[i], end_b[after_i - 1]));
                i += ndl_chars.len();
                continue;
            }
        }
        i += 1;
    }
    out
}

/// `true` if `needle` occurs in `haystack` under `opts` — the allocation-light,
/// EARLY-EXIT counterpart to [`find_matches`]. Used wherever we only need "does
/// this cell match" (cell-level search highlight, the SML count scan): it skips
/// the per-match output `Vec` AND the byte maps, and returns on the FIRST hit.
/// Critically, it does ZERO text shaping — a single common char in a 13k-char cell
/// is O(cell len) here, not the O(matches·len) that per-occurrence highlighting was.
fn find_matches_any(haystack: &str, needle: &str, opts: SearchOpts) -> bool {
    if needle.is_empty() {
        return false;
    }
    // Fast path: case-sensitive, no whole-word → stdlib substring search (memchr).
    if opts.case_sensitive && !opts.whole_word {
        return haystack.contains(needle);
    }
    // General path mirrors find_matches' char scan but early-exits and keeps only
    // the lowered char sequence (no start_b/end_b byte maps).
    let hay_chars: Vec<char> = if opts.case_sensitive {
        haystack.chars().collect()
    } else {
        haystack.chars().flat_map(fold_char).collect()
    };
    let ndl_chars: Vec<char> = if opts.case_sensitive {
        needle.chars().collect()
    } else {
        needle.chars().flat_map(fold_char).collect()
    };
    if ndl_chars.is_empty() || ndl_chars.len() > hay_chars.len() {
        return false;
    }
    let mut i = 0usize;
    while i + ndl_chars.len() <= hay_chars.len() {
        if hay_chars[i..i + ndl_chars.len()] == ndl_chars[..] {
            let after_i = i + ndl_chars.len();
            let ok = !opts.whole_word || {
                let before_ok = i == 0 || !hay_chars[i - 1].is_alphanumeric();
                let after_ok =
                    after_i == hay_chars.len() || !hay_chars[after_i].is_alphanumeric();
                before_ok && after_ok
            };
            if ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Width (px) from a text item's left edge to byte offset `byte` within its text:
/// the sum of the shaped glyph advances up to that byte (`cluster` = byte offset),
/// with a proportional char-count fallback when the item is unshaped. Lets the
/// search highlight slice to the matched CHARACTERS instead of the whole item
/// (just-the-term highlighting, the find standard). `byte` must be a char
/// boundary in `it.text` (search match offsets always are). Mirrors the partial-
/// width math in `decimal_pre_separator_width`.
fn item_width_to_byte(it: &Item, byte: usize) -> f64 {
    if byte == 0 {
        return 0.0;
    }
    if byte >= it.text.len() {
        return it.width; // whole item (incl. any justification stretch)
    }
    if let Some(shaped) = &it.shaped {
        shaped
            .glyphs
            .iter()
            .filter(|g| (g.cluster as usize) < byte)
            .map(|g| g.x_advance)
            .sum()
    } else {
        let total = it.text.chars().count().max(1) as f64;
        let before = it.text[..byte].chars().count() as f64;
        it.width * (before / total)
    }
}

/// The Word renderer's document-specific state — everything `loadDocx` derives
/// from the parsed package that only WML layout/render reads. Field docs are
/// verbatim from the pre-split `Viewer`.
pub(crate) struct WmlRenderer {
    // Retained: owns the parsed package the renderer was built from (kept alive
    // for future on-demand re-reads; not read on the current layout path).
    #[allow(dead_code)]
    doc: ooxml_wml::Document<Cursor<Vec<u8>>>,
    sections: Vec<Section>,
    /// Document watermark (§ VML WordArt in a header), if any. Drawn page-centered
    /// behind body content. One per document (first found across section headers).
    watermark: Option<Watermark>,
    default_tab_stop_px: f64,
    /// Whether the document grid's line pitch may be applied to (Latin) body
    /// text. True only when the document does NOT use Far-East layout
    /// (`useFELayout`). NOTE: necessary but not sufficient — the grid `type`
    /// must also be `lines`/`linesAndChars` to actually snap (see the
    /// `grid_pitch_px` filter). A `type="default"` grid does NOT snap, so its
    /// lines stay at the font's natural height. (On a calibration document
    /// set in DejaVu, Word shows one page more only because it substitutes the
    /// uninstalled DejaVu with the taller Verdana per the doc's altName — a
    /// font-substitution artifact, not a grid effect.)
    grid_enabled: bool,
    /// `<w:ulTrailSpace/>` compat setting (§17.15.3.8): when set, underline is
    /// drawn beneath end-of-line trailing spaces too. Default (absent) = off,
    /// i.e. trailing spaces are not underlined.
    ul_trail_space: bool,
    /// `compatibilityMode` < 15 (Word 2007/2010 legacy layout). Word's legacy
    /// line-fitter reserves part of a word's trailing space when deciding fit
    /// (the `wrapTrailSpaces` / "Word-compatible trailing blanks" behavior),
    /// which the modern (mode-15) engine and standard hanging-space layout do
    /// not. Gates the half-space reserve in `pack_lines`. Absent → false
    /// (treat as modern/standard).
    reserve_trailing_blanks: bool,
    /// §17.15.1.x `suppressTopSpacing` — when set, Word suppresses the
    /// before-spacing of the first paragraph at the top of a page. Absent (the
    /// common case, incl. compat-less docs) → Word DOES apply the first
    /// paragraph's before-spacing at a page top. Default false (= apply).
    suppress_top_spacing: bool,
    /// Numbering level definitions keyed by `(num_id, ilvl)`. Built from
    /// `word/numbering.xml` during `load_docx` by resolving the chain:
    /// `NumberingInstance.num_id` → `abstract_num_id` → `Level[ilvl]`.
    numbering_levels: HashMap<(i64, i64), NumberingLevelDef>,
    /// Style context for resolving run properties through the style chain.
    style_ctx: StyleContext,
    /// Default paragraph properties from `docDefaults/pPrDefault`.
    default_ppr: Option<ooxml_wml::types::CTPPrGeneral>,
    /// Theme font mappings (major/minor → actual font family name).
    theme_fonts: ThemeFonts,
    /// Footnotes keyed by id. Loaded from word/footnotes.xml during load_docx.
    footnotes: HashMap<i64, FootnoteEndnote>,
    /// Endnotes keyed by id. Loaded from word/endnotes.xml during load_docx.
    endnotes: HashMap<i64, FootnoteEndnote>,
    /// Comment display metadata in document order, parsed from
    /// `word/comments.xml` at load time. Empty when the document has no
    /// comments part. Joined to range-start positions by the `comments()`
    /// accessor.
    comments: Vec<CommentMeta>,
}

/// Per-layout fixpoint counters. `checkpoints_captured` counts
/// PageCheckpoint captures (gated — 0 for sections with no cursor-independent
/// float), `pages_relaid` counts fixpoint rollback+re-lays, `floats_demoted`
/// counts ledger demotions, `violations_detected` counts sealed pages with ≥1
/// cursor-independent-float violation (PER-PAGE; the dark shadow of
/// `pages_relaid` — the fixpoint re-lays each violating page once for the batched
/// union). The perf contract asserts on these: `checkpoints_captured`/
/// `pages_relaid`/`floats_demoted` must be 0 for float-free AND
/// paragraph-relative-float docs (and `violations_detected` too — those docs
/// carry no cursor-independent float), and `pages_relaid <= 3*pages + floats`
/// on page/margin-float docs.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct LayoutStats {
    pub(crate) checkpoints_captured: u32,
    pub(crate) pages_relaid: u32,
    pub(crate) floats_demoted: u32,
    pub(crate) violations_detected: u32,
}

// ─── PageCheckpoint (capture + restartability) ───────────────────────────────

/// Anchored images whose wrap band overflows the page they're anchored to are
/// deferred and re-emitted at the top of the *next* page (§20.4.2.17
/// wrapSquare: text must wrap around — not under — the object; `locked="0"`
/// permits moving it "from one page to another as needed"). Without this an
/// over-tall anchored image is clipped at the page bottom and lost.
/// Module-scoped (hoisted from `run_layout_impl`) so `PageCheckpoint` can own
/// a clone of the pending queue; `Clone` exists solely for that capture.
#[derive(Clone)]
struct DeferredFloat { rel_id: String, fx: f64, width_px: f64, height_px: f64, wrap: WrapInfo, behind_text: bool, target: usize, id: FloatId, anchor_para: Option<u32> }

/// Which per-paragraph anchored-float Vec an id addresses.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum AnchorKind { Image, TextBox }

/// Doc-unique identity of ONE anchored float instance.
/// `block_idx` is SECTION-RELATIVE (`block_caches` is rebuilt per section
/// in `run_layout_impl`), so `section_idx` is load-bearing for global uniqueness;
/// `within_idx` is the enumerate index inside the paragraph's per-kind Vec
/// (`anchored_images` / `anchored_text_boxes`), so a `rel_id` reused across
/// anchors (non-unique; textboxes have none) still yields distinct ids.
/// Stamped at registration/deferral and read by the LIVE demotion ledger
/// (per-page fixpoint) to decide whether an over-tall float
/// demotes once per page.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
struct FloatId { section_idx: usize, block_idx: usize, kind: AnchorKind, within_idx: usize }

/// A cursor-independent float's FROZEN draw payload — cloned
/// once at first resolution and re-emitted verbatim on every re-lay of the page
/// (the frozen-rect rule). `Image`/`TextBox` mirror the two `current` draw
/// arrays. All three carriers (AnchoredImageLayout, PositionedTextBox,
/// ActiveFloat) already derive Clone, so no field-by-field enum is needed.
#[derive(Clone)]
enum FrozenDraw { Image(AnchoredImageLayout), TextBox(PositionedTextBox) }

/// One frozen cursor-independent float: its identity, exclusion band, and draw.
/// Carried in `page_preset` (pre-registered floats for the page under fixpoint)
/// and `demoted_floats` (force-registered on the anchor's page).
#[derive(Clone)]
struct FrozenFloat { id: FloatId, af: ActiveFloat, draw: FrozenDraw }

/// Fixpoint seal verdict: the batched set of cursor-independent floats
/// to PRE-REGISTER on a page re-lay (their band overlaps content emitted above
/// their anchor) and the set to DEMOTE (a pre-registered float pushed its own
/// anchor off the page — follow the anchor to the page it lands on).
struct SealVerdict { pre_register: Vec<FloatId>, demote: Vec<FloatId> }

// Per-page/column float state, two-tier.
// `column_floats` = today's `active`: wrapping-float exclusions
// (absolute page px) cleared on EVERY column AND page advance.
// `page_floats` = a tier that PERSISTS across column advances
// within a page (page/margin-V floats cross all columns — Word wraps
// them in every column). Under the per-page fixpoint:
// page/margin floats route into it (see floats()/clear_column), and
// every reader sees column_floats ∪ page_floats. `block_tops` = (block_idx,
// y_in_page-at-top) of emitted blocks, seal_page's overlap targets;
// it is PAGE-scoped — cleared only on page/section
// advance, NOT on column advance, so a page float in column 1 sees
// column-0 tops (byte-identical on single-column docs and float-free
// docs, where block_tops is empty or never advances a column mid-
// page). `pending_floats` (cross-page
// deferral) stays separate below — it has a different lifecycle
// (drained, never per-page cleared).
struct FloatScope {
    column_floats: Vec<ActiveFloat>,
    // Under the per-page fixpoint: page/margin floats land
    // here and survive a column advance.
    page_floats: Vec<ActiveFloat>,
    // Parallel to *_floats (LOCKSTEP push/clear): maps a scope
    // position → its FloatId. LIVE — `seal_page` zips both pairs to
    // recover each float's anchor block, and the demotion ledger is
    // keyed on the ids it yields. The kernel still receives only the
    // `ActiveFloat`s (via `floats()`), never the ids.
    column_ids: Vec<FloatId>,
    page_ids: Vec<FloatId>,
    block_tops: Vec<(usize, f64)>,
}
impl FloatScope {
    // Column advance: clear ONLY the column tier. page_floats AND
    // block_tops persist so a page float discovered in col 0 keeps
    // excluding col 1, and seal_page sees col-0 block tops while
    // laying col 1 (the cross-column fixpoint win). Byte-identical on
    // single-column docs (clear_column is never called mid-page) and
    // on float-free docs (block_tops empty — gated on
    // doc_has_page_margin_wrap_float).
    fn clear_column(&mut self) {
        debug_assert_eq!(self.column_floats.len(), self.column_ids.len());
        self.column_floats.clear();
        self.column_ids.clear();
    }
    // Genuine page/section start (and balancing full-page rebuild):
    // clear BOTH float tiers AND the now-PAGE-scoped block_tops.
    fn clear_page(&mut self) {
        debug_assert_eq!(self.page_floats.len(), self.page_ids.len());
        self.page_floats.clear();
        self.page_ids.clear();
        self.clear_column();
        self.block_tops.clear();
    }
    fn is_empty(&self) -> bool {
        self.column_floats.is_empty() && self.page_floats.is_empty()
            // Lockstep implies these, asserted for
            // the byte-identity capture invariant.
            && self.column_ids.is_empty() && self.page_ids.is_empty()
    }
    // The unified exclusion set every kernel reader (clear_below_floats,
    // pack_lines_around_floats, overlap scans) must use.
    // page_floats is LIVE (page/margin floats persist
    // across column advances). When it is empty (the common case,
    // and EVERY float-free / column-only page) this is exactly
    // `&column_floats` — Cow::Borrowed, zero-alloc, byte-identical to
    // pre-split. Only a page that carries a live page float pays the
    // concat (order: column-then-page, deterministic). Readers hold
    // the Cow with `.as_ref()` (fn-arg sites) or deref it directly
    // (`.iter()`/`.is_empty()`).
    fn floats(&self) -> std::borrow::Cow<'_, [ActiveFloat]> {
        if self.page_floats.is_empty() {
            std::borrow::Cow::Borrowed(&self.column_floats)
        } else if self.column_floats.is_empty() {
            std::borrow::Cow::Borrowed(&self.page_floats)
        } else {
            let mut v = Vec::with_capacity(
                self.column_floats.len() + self.page_floats.len());
            v.extend_from_slice(&self.column_floats);
            v.extend_from_slice(&self.page_floats);
            std::borrow::Cow::Owned(v)
        }
    }
}

// Per-page fixpoint seam: the page-end verdict (accept vs roll back
// and re-lay). `seal_or_rollback!` calls this and must be the FIRST
// statement of every page-emission sequence — strictly before
// `finish_page`/`push_page`, so a page is never externally visible
// until sealed. Parity and relocation blank pages bypass
// `finish_page` and are exempt by construction.
// Cursor-independent-float violation detector. Under the per-page
// fixpoint, the verdict it returns drives the re-lay. It
// fires on the STRUCTURAL condition — a cursor-independent
// (positionV Page|Margin) WRAPPING float
// whose FROZEN exclusion band overlaps content emitted ABOVE its
// anchor on this page. Reads ONLY from (a) the frozen `ActiveFloat`
// geometry (via `overlaps_band`), (b) the anchor block index carried
// in `FloatId`, and (c) `scope.block_tops` — NEVER `current.elements`
// line y's. Reading post-repair PIXEL overlap would be the recall
// trap: a former bounded inset reflow pass repaired some
// overlaps in place before this seal ran, and mutated block_tops
// while doing it. That pass is gone (subsumed by the fixpoint — see
// `run_layout_impl`), and the one reflow that survives, the empty-para full-width reflow, does not
// touch block_tops. So within a page `block_tops` is APPEND-ONLY:
// cleared on page/section advance (`clear_page`), pushed once at each
// Paragraph block's top (in `run_layout_impl`), touched nowhere else.
// Known limitation (out of scope): block_tops records only Paragraph
// blocks, so table content above an anchor is invisible.
// block_tops is PAGE-scoped and the scan covers BOTH float
// tiers, so a col-1 page float above col-0 text IS seen.
//
// DEFERRED (over-tall) FLOATS ARE CORRECTLY SILENT HERE — and that
// is load-bearing, because it is what lets `apply_pending_floats`'s
// drain register a FloatId that has NO `resolved_this_page` entry
// (see the tripwire at the `newly` loop in `seal_or_rollback!`).
// Three independent legs, all of which hold today:
//   (1) a deferred float's anchor was emitted on a PRIOR page
//       (`target = section_page_idx + 1` at deferral), so on the
//       drain page it is absent from block_tops → the `None` arm,
//       which only demotes floats already in `page_preset`;
//   (2) even if the anchor paragraph CONTINUES onto the drain page,
//       the block_tops push sits inside `if !skip_reg_and_reflow`,
//       which a ParaLines/ParaItems continuation skips wholesale —
//       so the anchor is still absent;
//   (3) a continuation is that page's FIRST block, and blocks are
//       emitted in increasing index order, so the `bi < id.block_idx`
//       scan below would find nothing even if (1) and (2) failed.
// Nothing enforces any leg. The tripwire is what notices if a future
// change breaks all three at once.
#[inline(always)]
fn seal_page(scope: &FloatScope,
             block_caches: &[BlockCache], margin_top: f64,
             ledger: &HashSet<FloatId>, page_preset: &[FrozenFloat]) -> SealVerdict {
    let mut pre_register = Vec::new();
    let mut demote = Vec::new();
    // Scan BOTH tiers. page_floats persists across
    // column advances (a page float in col 1 above col-0 text is only
    // visible via this tier); column_floats is today's per-column set.
    for (af, id) in scope.column_floats.iter().zip(scope.column_ids.iter())
        .chain(scope.page_floats.iter().zip(scope.page_ids.iter()))
    {
        // (1) cursor-independent? recover v_rel via the anchor block cache.
        let independent = matches!(
            match block_caches.get(id.block_idx) {
                Some(BlockCache::Paragraph(c)) => match id.kind {
                    AnchorKind::Image => c.anchored_images.get(id.within_idx).map(|a| a.v_relative_from),
                    AnchorKind::TextBox => c.anchored_text_boxes.get(id.within_idx).map(|t| t.v_relative_from),
                },
                _ => None,
            },
            Some(AnchorVRelFrom::Page | AnchorVRelFrom::Margin)
        );
        if !independent { continue; }
        // (2) anchor block present on this page?
        match scope.block_tops.iter().rev()
            .find(|&&(bi, _)| bi == id.block_idx).map(|&(_, y)| y)
        {
            None => {
                // Anchor pushed OFF this page. If this float is one we
                // pre-registered here (its exclusion shoved its own
                // anchor away) AND it has not already demoted, DEMOTE
                // it (follow the anchor). A ledgered float re-registers
                // unconditionally on its demoted page — never re-flags.
                // A not-ours (naturally-off-page) anchor is skipped.
                if page_preset.iter().any(|fr| fr.id == *id) && !ledger.contains(id) {
                    demote.push(*id);
                }
            }
            Some(anchor_top_y) => {
                // (3) content emitted ABOVE the anchor on this page.
                if let Some(earlier_top_y) = scope.block_tops.iter()
                    .filter(|&&(bi, _)| bi < id.block_idx).map(|&(_, y)| y).reduce(f64::min)
                {
                    // (4) frozen band overlaps [topmost earlier .. anchor top]?
                    if af.overlaps_band(margin_top + earlier_top_y, margin_top + anchor_top_y) {
                        pre_register.push(*id);
                    }
                }
            }
        }
    }
    SealVerdict { pre_register, demote }
}

/// The top of a page's body band: `w:pgMar/@top` (§17.6.11), pushed down when
/// the header is taller than the gap between `w:pgMar/@header` and it
/// (§17.6.10 — Word grows the body rather than letting the header overlap it).
///
/// PURE in `(laid_out_hf, geo, section_page_idx)`, which is why it is a free
/// function and not a closure or a `PageCursor` method: the placement macros
/// call it while `PageCursor` fields are already mutably borrowed, and a method
/// on `&self` would borrow the whole cursor.
fn effective_margin_top(laid_out_hf: &LaidOutHdrFtr, geo: &PageGeometry,
                        section_page_idx: usize) -> f64 {
    let header_height = laid_out_hf.header_for_page(section_page_idx)
        .map_or(0.0, |h| h.total_height);
    let header_gap = geo.margin_top_px - geo.header_margin_px;
    if header_height > header_gap {
        geo.header_margin_px + header_height
    } else {
        geo.margin_top_px
    }
}

/// The height of a page's body band — `effective_margin_top`'s rule applied at
/// both edges, subtracted from the page height. Same purity contract.
///
/// NOT `page_height - effective_margin_top(..) - effective_bottom`: it is
/// spelled as one subtraction chain because the arithmetic is pinned by the
/// byte-identity oracle and f64 addition is not associative.
fn effective_content_height(laid_out_hf: &LaidOutHdrFtr, geo: &PageGeometry,
                           section_page_idx: usize) -> f64 {
    let header_height = laid_out_hf.header_for_page(section_page_idx)
        .map_or(0.0, |h| h.total_height);
    let footer_height = laid_out_hf.footer_for_page(section_page_idx)
        .map_or(0.0, |f| f.total_height);
    let header_gap = geo.margin_top_px - geo.header_margin_px;
    let effective_top = if header_height > header_gap {
        geo.header_margin_px + header_height
    } else {
        geo.margin_top_px
    };
    let footer_gap = geo.margin_bottom_px - geo.footer_margin_px;
    let effective_bottom = if footer_height > footer_gap {
        geo.footer_margin_px + footer_height
    } else {
        geo.margin_bottom_px
    };
    geo.height_px - effective_top - effective_bottom
}

/// Emit any image deferred from an earlier page at the top of the *current*
/// page and register its wrap band, so this page's text flows below it.
///
/// Called both at each block's loop-top AND on every page advance inside
/// `overflow_col!` — the latter is essential: a paragraph that breaks
/// mid-iteration onto the float's target page must see the float in
/// `scope.column_floats` *before* it places its continuation, or that
/// continuation overlaps the image (§20.4.2.17 — text wraps around, never
/// under, the object).
///
/// A free function and not a macro — the first of the `run_layout_impl`
/// macros to become a function (the other free fns nearby only prepared the
/// ground). This one is separable because it neither escapes its
/// caller (no `continue`/`break`/`return`/`.await`) nor expands another macro,
/// so the caller's locals can simply be threaded in. `pending`, `scope` and
/// `current` are three distinct locals of `run_layout_impl`, so the three `&mut`
/// stay disjoint at both call sites. Seven arguments is exactly clippy's default
/// ceiling; an eighth would need a grouping type, not an `#[allow]`.
fn apply_pending_floats(pending: &mut Vec<DeferredFloat>, scope: &mut FloatScope,
                        current: &mut PageLayout, laid_out_hf: &LaidOutHdrFtr,
                        geo: &PageGeometry, section_page_idx: usize, col_idx: usize) {
    if pending.is_empty() { return; }
    let ct = effective_margin_top(laid_out_hf, geo, section_page_idx);
    let cur_col = &geo.columns[col_idx];
    let col_ml = cur_col.x_offset_px;
    let col_mr = geo.width_px - col_ml - cur_col.width_px;
    let mut keep = Vec::new();
    for df in std::mem::take(pending) {
        if df.target <= section_page_idx {
            if let Some(af) = build_active_float(&df.wrap, df.fx, ct, df.width_px, df.height_px, col_ml, geo.width_px - col_mr) {
                scope.column_floats.push(af);
                // Carry the deferral-time id
                // (Copy — read before df.rel_id moves below),
                // NOT recomputed: the demoted-page and anchor-
                // page registrations must share ONE id.
                //
                // WHY THE PRE-REGISTRATION LOOKUP IS SAFE. Corrected twice;
                // both earlier versions of this argument were wrong, so the
                // reasoning matters more than the tally. Of the eight pushes
                // into `column_ids`/`page_ids`, FOUR carry no
                // `resolved_this_page` entry (re-derive with
                // `grep -n 'column_ids\.push\|page_ids\.push'` — line numbers
                // rot on every edit):
                //   - the two COLUMN-tier pushes, which sit in the `else` of
                //     `if independent`. The `.entry().or_insert_with` lives in
                //     the `if`, so only the PAGE tier is populated.
                //   - the page-start preset injection.
                //   - this drain.
                //
                // The lookup is nonetheless total WHERE IT IS REACHED, and the
                // column-tier case is the load-bearing part: `seal_page`
                // iterates both tiers (`column_ids.chain(page_ids)`) but
                // immediately does `if !independent { continue }` — the SAME
                // predicate that routed the float to the page tier in the first
                // place. A column-tier id is by construction not
                // page/margin-relative, so it can never reach `pre_register`,
                // so its missing entry is never consulted. The preset injection
                // is excluded a different way: its ids are in `page_preset` by
                // construction and are filtered out of `newly` before the
                // lookup.
                //
                // That leaves this drain as the only reachable gap. It is so
                // deliberately: the
                // frozen draw payload for a deferred float was
                // emitted on its anchor page, not here. It is safe
                // only because `seal_page` can never nominate a
                // drained float — see the three-leg argument in its
                // docblock — and the tripwire in that lookup is
                // what fires if that ever stops being true.
                scope.column_ids.push(df.id);
            }
            current.anchored_images.push(AnchoredImageLayout {
                rel_id: df.rel_id, x: df.fx, y: ct,
                width_px: df.width_px, height_px: df.height_px, behind_text: df.behind_text,
                anchor_para: df.anchor_para,
            });
        } else { keep.push(df); }
    }
    *pending = keep;
}

/// Discriminates the three `ParaLines` writers — their resume landing pads are
/// observably different, and the routing inputs (`para_floated`, computed
/// against the block-entry page's floats) no longer exist at resume time.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ParaLineSrc {
    /// Fast-path should_break/widow overflow (resumes MID-iteration: the
    /// deferred-float handoff check only, then place next_line
    /// unconditionally at the current y).
    OverflowBreak,
    /// ends_with_page_break overflow (resumes at the NEXT iteration TOP:
    /// should_break AND the widow branch — which lacks a !current.is_empty()
    /// guard and can fire on an empty page — are re-evaluated).
    ExplicitBreak,
    /// place_empty_para_around_floats! overflow (resumes mid-iteration: skip
    /// clear_below_floats AND the fit check for line next_line; remaining
    /// lines use the MACRO-ENTRY content_height carried here — a second,
    /// distinct stale value, never the block-loop hoist).
    EmptyPara { macro_content_height: f64 },
}

/// Discriminates the two `ParaItems` writers: an unequal-cols section with a
/// floated paragraph takes `place_float_aware!`, so `cols_unequal` alone
/// cannot pick the landing pad.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ParaItemsSrc { FloatAware, UnequalCols }
// ⚠️ RESIDUAL, declared: `ResumeAt::ParaItems` does NOT carry the
// `$logical` flag `place_float_aware!` was entered with. The mid-paragraph
// relay in `run_layout_impl` enters with `false` — its remainder is flattened
// out of `ParagraphCache::lines`, which `pack_lines` already permuted into
// VISUAL order — while the paragraph-entry site enters with `true`. A
// remainder that checkpoints on the relay path and resumes therefore comes back
// without that distinction. Reaching it needs a bidi paragraph that both wraps
// around a float AND overflows a page mid-paragraph on the relay route; no
// fixture does, so this is UNPINNED, not proven benign. Carrying the flag in the
// variant is the fix — deliberately deferred because it touches the
// page-checkpoint schema, which is a separate blast radius from the packer.

/// ── ResumeCursor ── where the most recently started
/// column/page's content resumes, reified out of macro-locals so the
/// PageCheckpoint owns it. Cursors/counts into the immutable
/// block_caches only — never item data (RowSplit's per-cell Vec is O(#cells));
/// the ParaItems live remainder rides `CheckpointResume::para_remainder`.
/// Every `overflow_col!` call site must state its continuation as the macro's
/// second argument (a missed site is a compile error, not a silent gap); the
/// page_break_before inline advance and the section flush write BlockBoundary
/// directly; a completed block clears it at the block-loop bottom.
#[derive(Clone, Debug)]
// RowSplit fields are read by the replay path, which compiles UNCONDITIONALLY
// into production (byte-identity pins one code path); only the CONSTRUCTION of
// a resume directive is test-only TODAY (the fixpoint constructs them in production).
#[allow(dead_code)]
enum ResumeAt {
    // Page/column starts at a block boundary: `next_block` runs its
    // full block preamble (inter-block spacing, at_page_top test,
    // float registration) on the new page. NEVER written for a page
    // created mid-arm: a block whose advance precedes its own
    // epilogue (spacing/style carry, footnote ids, wrapNone draws —
    // all of which execute on the NEW page) must record a cursor
    // that re-enters its arm (see the trailing-page-break sites,
    // which record ParaItems{items_remaining: 0} / ParaLines{len}).
    BlockBoundary { next_block: usize },
    // page_break_before inline advance: `block` runs its full
    // preamble on the new page, but this page start — uniquely —
    // does NOT drain pending_floats (real flow defers the drain to
    // the NEXT block's loop-top, so the break-bearing paragraph
    // lays float-blind). A replay must reproduce that deferral,
    // not the canonical drain-at-page-start recipe.
    PageBreakBeforeTop { block: usize },
    // keepLines/keepNext push (need_break): `block` starts the new
    // page at its first line, but its inter-block spacing was
    // already consumed — and discarded by the y reset — on the
    // PREVIOUS page, and float registration has not yet run.
    // Distinct from BlockBoundary AND from ParaLines{next_line: 0}.
    ParaTop { block: usize },
    // Fast-path line loop / empty-para mark lines: resume emitting
    // cache.lines[next_line..] (spacing + float registration already
    // done). next_line == lines.len() after a trailing page-break
    // line: the block completes immediately on the new page. `src`
    // picks the landing pad (see ParaLineSrc).
    ParaLines { block: usize, next_line: usize, src: ParaLineSrc },
    // place_float_aware! / unequal-columns re-pack: `items_remaining`
    // items are unplaced and re-pack on the new page; `is_first_batch`
    // gates the continuation's first-line indent. items_remaining == 0
    // means the block's content is fully placed but its arm epilogue
    // has not run (trailing page break) — replay re-enters the arm so
    // the epilogue executes on the new page.
    //
    // CONTRACT LIMIT (resolved by the owned remainder — do not build on the count
    // alone): the live continuation is NOT the last-N items of the
    // pristine flattened stream. Three lossiness vectors, all
    // fixture-reachable: (1) whitespace-only items are dropped at
    // wrap edges in the UNPLACED tail too (the placed/unplaced cut
    // happens after packing); (2) justification mutates item widths
    // in place on non-last lines including unplaced ones, and the
    // carried tail keeps those widths; (3) the unequal-columns path
    // rebuilds the remainder from packed lines AFTER per-line bidi
    // reordering (visual order, not a logical-stream suffix). So the
    // checkpoint OWNS the rebuilt remainder Vec (bounded: one
    // straddling paragraph per page start; oversized remainders make
    // the checkpoint INVALID per MAX_CHECKPOINT_REMAINDER_ITEMS).
    ParaItems { block: usize, items_remaining: usize, is_first_batch: bool, src: ParaItemsSrc },
    // Whole-row table path: resume at table.rows[next_row].
    RowWhole { block: usize, next_row: usize },
    // place_split_row! mid-row break: per-cell cursors into the
    // row's flattened cell_protos plus the emitted row-content
    // height; the fragment flags (is_first_fragment, borders,
    // margin insets) derive from `consumed`. cell_protos/cell_xs
    // are recomputable from the immutable TableCache PLUS
    // `entry_col`: the macro computes cell x geometry once at entry
    // and continuation fragments reuse it verbatim across column
    // and page advances (today's quirk — a split row continues at
    // the entry column's x), so the entry column, not the
    // checkpoint's col_idx, reproduces it.
    RowSplit { block: usize, row: usize, entry_col: usize, cursors: Vec<usize>, consumed: f64 },
}

impl ResumeAt {
    /// The block the re-entered loop starts at (== the continuation's block
    /// for mid-block variants; the next block for BlockBoundary).
    fn entry_block(&self) -> usize {
        match self {
            ResumeAt::BlockBoundary { next_block } => *next_block,
            ResumeAt::PageBreakBeforeTop { block }
            | ResumeAt::ParaTop { block }
            | ResumeAt::ParaLines { block, .. }
            | ResumeAt::ParaItems { block, .. }
            | ResumeAt::RowWhole { block, .. }
            | ResumeAt::RowSplit { block, .. } => *block,
        }
    }
    /// Every mid-block variant replays the checkpoint's STALE block-entry
    /// content_height (the original run's hoist was computed on an earlier
    /// page and never refreshed — byte contract). Only a BlockBoundary entry
    /// recomputes, exactly like a pristine block top. PageBreakBeforeTop
    /// keeps stale too: the original hoist preceded the inline advance.
    fn keeps_stale_content_height(&self) -> bool {
        !matches!(self, ResumeAt::BlockBoundary { .. })
    }
    /// Census label (rollback instrument): the variant name alone — no fields
    /// (RowSplit's `cursors` Vec is unbounded, Debug would spam the log).
    /// Native-only: read by the PARASMA_STATS_CENSUS rollback log.
    #[cfg(not(target_arch = "wasm32"))]
    fn census_label(&self) -> &'static str {
        match self {
            ResumeAt::BlockBoundary { .. } => "BlockBoundary",
            ResumeAt::PageBreakBeforeTop { .. } => "PageBreakBeforeTop",
            ResumeAt::ParaTop { .. } => "ParaTop",
            ResumeAt::ParaLines { .. } => "ParaLines",
            ResumeAt::ParaItems { .. } => "ParaItems",
            ResumeAt::RowWhole { .. } => "RowWhole",
            ResumeAt::RowSplit { .. } => "RowSplit",
        }
    }
}

/// One-shot re-entry continuation, consumed at the re-entered block's loop
/// top. The replay path compiles UNCONDITIONALLY into production —
/// byte-identity pins one code path, and the fixpoint constructs EntryPlans in
/// production; only the construction of a resume directive (the self-test's
/// `resume_directive`) is test-only TODAY.
struct EntryPlan {
    resume: ResumeAt,
    /// ParaItems only: the checkpoint-owned live remainder.
    para_remainder: Option<Vec<Item>>,
    /// The checkpoint's block_entry_content_height, replayed verbatim for
    /// mid-block variants (see keeps_stale_content_height).
    stale_content_height: f64,
}

/// A paragraph's items in LOGICAL order for a whole-paragraph RE-PACK.
///
/// `ParagraphCache::lines` hold UAX#9 VISUAL order — `pack_lines` permutes each
/// finished line — so flattening them and re-packing re-breaks the words in the
/// wrong order, and a second L2 pass does not repair it (L2 is not idempotent on
/// a visual sequence; it hands back logical order). `logical_items` carries the
/// pristine stream for exactly the paragraphs L2 can touch; everywhere else the
/// flatten IS logical, so this costs nothing on the Latin/CJK majority.
fn logical_para_items(cache: &ParagraphCache) -> Vec<Item> {
    match &cache.logical_items {
        Some(items) => items.clone(),
        None => cache.lines.iter().flat_map(|l| l.items.clone()).collect(),
    }
}

/// Paragraph-arm routing decision: pristine runs derive it
/// from para_floated/cols_unequal exactly as before; a mid-block re-entry
/// derives it from the recorded ResumeAt source — the original routing
/// inputs (`para_floated` against the block-entry page's floats) no longer
/// exist at resume time.
enum ParaRoute {
    /// Unequal-column re-pack loop; `items` is the paragraph's LOGICAL stream
    /// (`logical_para_items`) or the checkpoint's owned remainder (post-bidi/
    /// justify — why the checkpoint owns it).
    UnequalCols { items: Vec<Item>, first_batch: bool },
    /// Fast-path line loop from `start`. `resume_after_break`: land
    /// MID-iteration (OverflowBreak) — replay only the deferred-float handoff,
    /// then place `start` unconditionally.
    FastPath { start: usize, resume_after_break: bool },
    /// Empty/whitespace-only paragraph mark lines from `start`;
    /// `resume_ch` = the captured MACRO-ENTRY content_height on re-entry
    /// (skip the resumed line's clearance + fit check), None when pristine.
    EmptyPara { start: usize, resume_ch: Option<f64> },
    /// place_float_aware! with `items` (pristine flatten or owned remainder;
    /// empty remainder re-enters the arm so the epilogue replays).
    FloatAware { items: Vec<Item>, first_batch: bool },
}

/// A ParaItems checkpoint whose live remainder exceeds this is
/// marked INVALID (`resume: None`) instead of cloned — decided on the COUNT,
/// the oversized remainder is never cloned. Caps capture at O(1) amortized and
/// defeats the O(n²) straddling-paragraph adversary (one giant paragraph would
/// otherwise clone O(n) items at every page seam it crosses); the self-test
/// skips the page, a future fixpoint aborts-to-inert.
pub(crate) const MAX_CHECKPOINT_REMAINDER_ITEMS: usize = 4096;

#[allow(dead_code)] // consumed by the restartability self-test / the fixpoint
#[derive(Clone)]
struct CheckpointResume {
    at: ResumeAt,
    /// ParaItems owns a clone of the LIVE rebuilt remainder (ws drops in
    /// the unplaced tail, in-place justify mutation, bidi reorder — see the
    /// ParaItems variant doc). `Some` (possibly empty) iff `at` is ParaItems.
    para_remainder: Option<Vec<Item>>,
}

/// Everything a page-start re-entry needs. Captured at the three page-start
/// shapes: (a) overflow_col!'s page arm after the resets, before the
/// pending-float drain; (b) the page_break_before inline advance; (c) section
/// entry before the block loop (the carry-seeded first page of every section,
/// including document page 1). Owned data only — no borrows into block_caches,
/// no RefCell borrows (rollback-safe across awaits by construction).
///
/// Deliberately ABSENT: `scope` (FloatScope) — empty at every capture site by
/// construction (post clear_page, pre drain; debug_asserted at capture) — and
/// `list_counters`, which are cache-build-scoped: re-entry re-runs every
/// section prologue in document order, so numbering advances deterministically
/// without a snapshot.
#[allow(dead_code)]
#[derive(Clone)]
struct PageCheckpoint {
    sec_idx: usize,
    /// == self.pages.len() at every capture site (the upcoming page's index).
    global_page_idx: usize,
    section_page_idx: usize,
    /// Non-zero on a carry-seeded section entry (continuous §17.6.22).
    y_in_page: f64,
    /// Verbatim (0 at page starts today) — never assumed.
    col_idx: usize,
    /// "First block that BEGINS on this page" — NOT the resume pointer
    /// (mid-block continuations point one past the true first block).
    last_page_start_block: usize,
    /// Whole-struct clone; empty except carry-seeded site (c).
    current: PageLayout,
    prev_para_style: Option<(String, bool)>,
    prev_space_after_px: f64,
    /// Pre-drain clone (capture precedes apply_pending_floats); un-drained
    /// by design at a page_break_before start (variant contract).
    pending_floats: Vec<DeferredFloat>,
    /// Some only under the balancing-consumption predicate (mirrors the sole
    /// consumption guard in the balancing pass: lpsb == 0 &&
    /// section_page_idx == carried_in_spi; both are monotone within a
    /// section, so a mid-section None is provably restore-safe and keeps
    /// capture O(1)). Cross-ref comment at the balancing consumption site.
    carried_in: Option<(Vec<PageElement>, f64)>,
    carried_in_spi: usize,
    /// The block-loop-top hoisted content_height, VERBATIM — deliberately
    /// STALE for a block continuing onto this page (byte-identity trap;
    /// consumers: need_break, fast-path should_break/widow, unequal-cols,
    /// whole-row tests, FrameCtx, epilogue). BlockBoundary re-entry
    /// recomputes it; every mid-block variant replays the stale value.
    block_entry_content_height: f64,
    /// Geometry-INVISIBLE: in-loop layout_text_box calls mutate the
    /// OPEN_COMMENT_IDS thread-local; the self-test compares this explicitly.
    open_comment_ids: Vec<i64>,
    /// None ⇔ INVALID (oversized remainder). All other fields still
    /// captured (O(1)); the self-test counts the skip, a fixpoint aborts.
    resume: Option<CheckpointResume>,
}

/// The enclosing-layout state `build_checkpoint` reads, named rather than
/// positional. It exists so the list is spelled ONCE — in
/// `capture_checkpoint!`, which stays a forwarding shim precisely so its three
/// call sites do not each repeat fifteen field inits.
///
/// All borrows are SHARED and live only for the call expression, so a capture
/// never conflicts with the `&mut` uses that surround its call sites (notably
/// the `apply_pending_floats` drain on the very next statement of site (a)).
/// Nothing here is cloned by construction: of 15 fields, **9 are `Copy`** and
/// **6 are shared borrows** — counted, not estimated; re-count when adding a
/// field. The borrowed ones are
/// cloned inside the body under the caller's gate exactly as
/// they were when this was an inline macro body.
///
/// `doc_has_page_margin_wrap_float` is deliberately NOT a field: the capture
/// gate stays in the shim. Were the gate to move in here, the function would
/// have to return `Option<PageCheckpoint>`, and assigning that would overwrite
/// `page_ckpt` on a gated-OFF capture — where today it is left untouched, which
/// is what lets the resume path seed it.
struct CheckpointInputs<'a> {
    scope: &'a FloatScope,
    resume_at: &'a ResumeAt,
    current: &'a PageLayout,
    prev_para_style: &'a Option<(String, bool)>,
    pending_floats: &'a Vec<DeferredFloat>,
    carried_in: &'a Option<(Vec<PageElement>, f64)>,
    sec_idx: usize,
    global_page_idx: usize,
    section_page_idx: usize,
    y_in_page: f64,
    col_idx: usize,
    last_page_start_block: usize,
    prev_space_after_px: f64,
    carried_in_spi: usize,
    block_entry_content_height: f64,
}

/// Per-push snapshot for the restartability self-test: the page as pushed
/// (BEFORE the on_page sink and the two post-loop passes mutate it) plus the
/// OPEN_COMMENT_IDS state at push time. The comparator reads these, never
/// final `self.pages`.
#[cfg(test)]
struct PushRecord { page: PageLayout, open_comment_ids: Vec<i64>, section_page_idx: usize }

/// Plain (non-wasm_bindgen) impl: the capture-gate test override. Kept out of
/// the `#[wasm_bindgen]` blocks so the cfg-paired pair can't perturb codegen.
impl Viewer {
    /// Test-only capture forcing. Production is `false` by construction
    /// — capture stays gated on `doc_has_page_margin_wrap_float` alone, which
    /// the unforced zero-asserts pin.
    #[cfg(test)]
    fn capture_forced(&self) -> bool {
        self.force_capture.get()
    }
    #[cfg(not(test))]
    #[inline(always)]
    fn capture_forced(&self) -> bool {
        false
    }

    /// Build one page-start `PageCheckpoint`.
    ///
    /// Hoisted out of `run_layout_impl`; `capture_checkpoint!` remains
    /// as the shim that owns the CAPTURE GATE and the `CheckpointInputs` field
    /// list. This function is unconditional — every caller has already decided
    /// to capture — and it also owns the two side effects that must stay
    /// paired with a build: the activation counter and the self-test log.
    ///
    /// `rem` is the live ParaItems remainder. It arrives already-evaluated
    /// because the whole call sits inside the shim's gated branch, so no clone
    /// happens on a column advance or in an ungated document.
    fn build_checkpoint(&self, i: CheckpointInputs<'_>, rem: Option<&Vec<Item>>) -> PageCheckpoint {
        let CheckpointInputs {
            scope, resume_at, current, prev_para_style, pending_floats, carried_in,
            sec_idx, global_page_idx, section_page_idx, y_in_page, col_idx,
            last_page_start_block, prev_space_after_px, carried_in_spi,
            block_entry_content_height,
        } = i;
        // FloatScope is empty at every page start by construction (post
        // clear_page, pre drain) — the checkpoint deliberately does not carry it.
        debug_assert!(
            scope.is_empty() && scope.block_tops.is_empty(),
            "FloatScope must be empty at checkpoint capture"
        );
        // Totality: exactly the ParaItems sites pass a remainder, and its
        // length mirrors the cursor.
        debug_assert_eq!(
            rem.map(|r| r.len()),
            match resume_at {
                ResumeAt::ParaItems { items_remaining, .. } => Some(*items_remaining),
                _ => None,
            },
            "remainder must accompany exactly ParaItems, with matching length"
        );
        // Decide on the COUNT before cloning — an oversized straddler
        // makes the checkpoint INVALID (resume: None), never an O(n) clone
        // per page.
        let resume = match (resume_at, rem) {
            (ResumeAt::ParaItems { items_remaining, .. }, _)
                if *items_remaining > MAX_CHECKPOINT_REMAINDER_ITEMS => None,
            (ResumeAt::ParaItems { .. }, Some(r)) => Some(CheckpointResume {
                at: resume_at.clone(),
                para_remainder: Some(r.clone()),
            }),
            (ResumeAt::ParaItems { .. }, None) => None,
            (_, _) => Some(CheckpointResume {
                at: resume_at.clone(),
                para_remainder: None,
            }),
        };
        let ckpt = PageCheckpoint {
            sec_idx,
            global_page_idx,
            section_page_idx,
            y_in_page,
            col_idx,
            last_page_start_block,
            current: current.clone(),
            prev_para_style: prev_para_style.clone(),
            prev_space_after_px,
            pending_floats: pending_floats.clone(),
            // Mirror of the balancing consumption guard — carried_in is
            // only ever read while the carried band is still on the section's
            // first page; both predicate inputs are monotone within a section,
            // so a mid-section None restores safely.
            carried_in: if last_page_start_block == 0 && section_page_idx == carried_in_spi {
                carried_in.clone()
            } else {
                None
            },
            carried_in_spi,
            block_entry_content_height,
            open_comment_ids: OPEN_COMMENT_IDS.with(|c| c.borrow().clone()),
            resume,
        };
        // Counts machinery ACTIVATIONS — INVALID and forced captures
        // included (the zero-asserts run unforced, so production gating
        // stays pinned).
        let mut st = self.layout_stats.get();
        st.checkpoints_captured += 1;
        self.layout_stats.set(st);
        #[cfg(test)]
        self.checkpoint_log.borrow_mut().push((global_page_idx, ckpt.clone()));
        ckpt
    }
}

#[wasm_bindgen]
pub struct Viewer {
    /// The loaded document, if any — the per-format dispatch point.
    loaded: LoadedDoc,
    page_size: PageGeometry,
    /// Each fully laid-out page, populated as `run_layout()` streams.
    ///
    /// Interior mutability matters: `run_layout` takes `&self` (not
    /// `&mut self`) so that the JS event loop is free to call `render_page`
    /// during the `await` yields between pages. Without the RefCell, the
    /// `&mut self` held by `run_layout` would conflict with `render_page`'s
    /// `&self` at wasm-bindgen's runtime borrow tracker, panicking the
    /// render attempt and leaving canvases blank until the viewer is
    /// scrolled out and back in.
    ///
    /// Single-threaded WASM means we only need to ensure no `borrow_mut`
    /// is held across an `await` — RefCell's runtime check enforces the
    /// rest.
    pages: RefCell<Vec<PageLayout>>,
    /// The ParaMeta side table captured during the cache-build
    /// walk (the extraction source of truth). Indexed by document-order para id
    /// (`para_meta[para_id]`); rebuilt every layout generation. Interior
    /// mutability like `pages` so the additive capture never needs `&mut self`.
    para_meta: RefCell<Vec<crate::wml::select::ParaMeta>>,
    /// Top-level table STRUCTURE side table, keyed
    /// `(sec_idx, block_idx)`, captured from each `TableCache` in the cache-build
    /// walk. Holds spans/borders/fill/v-merge so `extractRange` can emit a full
    /// grid; cell CONTENT arrives from `para_meta`. Rebuilt every layout
    /// generation, cleared alongside `para_meta`.
    table_meta: RefCell<HashMap<(usize, usize), crate::wml::select::TableMeta>>,
    /// Font cache for text shaping. Fonts are pushed from JS via `loadFont()`.
    font_cache: FontCache,

    /// Image bytes extracted from the OOXML package, keyed by relationship
    /// ID. Populated synchronously during `load_docx` so the dimensions
    /// (from `<wp:extent>`) and rendering can be wired without async
    /// coordination. Decoded `ImageBitmap`s arrive asynchronously via
    /// `set_image_bitmap` once JS finishes `createImageBitmap`.
    images: RefCell<HashMap<String, ImageBytes>>,
    /// The SET of relationship IDs that ARE images
    /// (i.e. had raw bytes in `images` at load), retained INDEPENDENTLY of the
    /// bytes. `clearImageBytes` frees the `ImageBytes` values but must keep the
    /// *knowledge* of which rels are images so the object hit-test (`is_image`)
    /// still works in the post-decode steady state. Excludes charts (chart XML
    /// is never inserted into `images`). Cleared only on doc switch.
    image_rel_ids: RefCell<HashSet<String>>,
    image_bitmaps: RefCell<HashMap<String, ImageBitmap>>,
    /// Chart XML bytes keyed by "chart::<chartRelId>". Stored during load_docx
    /// so render_page can parse and render charts directly from the XML.
    chart_xml: RefCell<HashMap<String, Vec<u8>>>,
    /// Parsed EMF drawing programs keyed by image rel-id. Populated ONCE in the
    /// image-extraction loop (parse-once, not per repaint/zoom) when the picture
    /// part is `image/x-emf`; cleared alongside `chart_xml`. `Rc` so the
    /// per-page render registry (`emf::set_active`) takes cheap pointer clones.
    /// Feature-gated: absent (no field, no branch) in a docx-only build, keeping
    /// the compiled wasm byte-identical.
    #[cfg(feature = "emf")]
    emf_pics: RefCell<HashMap<String, std::rc::Rc<emf::EmfPicture>>>,
    /// Document-level dates for field evaluation. `created`/`modified` come
    /// from core properties (set in `load_docx`); `now` is supplied by JS via
    /// `setNow`. Optional → unset date fields fall back to cached result.
    doc_dates: RefCell<field::DocDates>,
    /// Monotonic layout generation. Bumped by `abort_layout` (called from JS
    /// before loading a new document) so an in-flight `run_layout` future bails
    /// at its next page boundary instead of holding the Viewer's wasm-bindgen
    /// borrow — which would make the subsequent `loadDocx` (&mut self) panic
    /// with "recursive use of an object".
    layout_gen: std::cell::Cell<u64>,
    /// Memory backpressure — stop laying out BEFORE the wasm memory wall.
    /// `memory_budget_bytes` bounds total LINEAR MEMORY (not per-document
    /// allocation: doc bytes, images and caches all count, because the wall
    /// they share is the same wall). Checked at the three page-boundary yield
    /// points; when exceeded, layout returns the pages laid so far and stamps
    /// `layout_truncated_at = Some(page_count)`. Default 2 GiB: a
    /// measured 2.31 MB/page puts that near 900 pages, and it leaves the same
    /// again in headroom to the 4 GiB wasm32 wall for paint buffers and
    /// images. THE NUMBER IS PROVISIONAL and may be retuned; embedders can
    /// override it. `set_memory_budget(0)` disables.
    memory_budget_bytes: std::cell::Cell<u64>,
    /// `Some(n)` iff the LAST layout stopped early (budget or allocation
    /// failure) after emitting `n` pages. Reset to `None` at every layout
    /// start; `None` therefore also means "layout was never truncated", so
    /// JS can render "showing the first n pages" exactly when it should.
    layout_truncated_at: std::cell::Cell<Option<u32>>,
    /// Native stand-in for `memory_size` (tests inject pressure through it;
    /// default 0 = never over budget, so the existing suite is untouched).
    #[cfg(not(target_arch = "wasm32"))]
    test_memory_bytes: std::cell::Cell<u64>,

    /// Fixpoint observability, reset per layout. The counters are written only
    /// by the checkpoint and fixpoint machinery; docs without such floats keep
    /// them at 0, which the zero-asserts in oracle_tests gate.
    /// `Cell` (not RefCell): incremented from
    /// inside the layout future while the Viewer borrow is held.
    layout_stats: std::cell::Cell<LayoutStats>,

    /// Restartability self-test hooks (native tests only; the wasm build
    /// carries none of this). `force_capture` bypasses the
    /// doc_has_page_margin_wrap_float gate so EVERY fixture exercises capture
    /// (production gating stays honest via the unforced zero-asserts).
    #[cfg(test)]
    force_capture: std::cell::Cell<bool>,
    /// (upcoming page's global_page_idx, checkpoint), in capture order. A
    /// carry-shared page appears TWICE (page arm of section S + entry of
    /// S+1) — both are valid re-entry points; this is a Vec, never a page
    /// map.
    #[cfg(test)]
    checkpoint_log: RefCell<Vec<(usize, PageCheckpoint)>>,
    /// Per-sealed-page violation verdicts (global_page_idx,
    /// violating cursor-independent FloatIds), in seal order; only pages with
    /// ≥1 violation are pushed (an empty log == a SILENT doc). Native tests
    /// only — the wasm build carries none of this.
    #[cfg(test)]
    violation_log: RefCell<Vec<(usize, Vec<FloatId>)>>,
    /// Rollback instrument: the resume-variant label (`ResumeAt::census_label`) of
    /// the checkpoint each fixpoint rollback re-entered through, in rollback
    /// order. Written on the ROLLBACK branch only, so it cannot be satisfied by
    /// a document that merely contains a float. The five per-variant coverage
    /// fixtures and the self-baselining census
    /// (`rollback_census_covers_every_resume_variant`) assert on this —
    /// a fixture that drifts to rolling back through a different variant must
    /// FAIL loudly, not keep pinning nothing. Native tests only.
    #[cfg(test)]
    rollback_log: RefCell<Vec<&'static str>>,
    /// One-shot: taken at layout entry; the run restores this checkpoint and
    /// re-lays from it to document end (replay runs are normal layout runs —
    /// pages cleared, capture stays live).
    #[cfg(test)]
    resume_directive: RefCell<Option<PageCheckpoint>>,
    /// Per-push snapshots taken INSIDE push_page BEFORE the on_page sink and
    /// the two post-loop passes — the comparator reads these, never
    /// `self.pages`.
    #[cfg(test)]
    push_records: RefCell<Vec<PushRecord>>,

    /// Active full-text-search highlight state, set by
    /// `set_search_highlight` from the TS `plugin-search` layer and read by the
    /// `render_wml_page` search paint pass. `query` empty / `None` → no
    /// highlights. `active_page`/`active_index` identify the single focused match
    /// (the one next/prev lands on) so it can paint with the hotter emphasis
    /// colour. Interior mutability (like `pages`) so the per-keystroke setter
    /// takes `&self` and never collides with an in-flight render borrow; the
    /// query NEVER touches layout, so a keystroke can never invalidate geometry.
    search_query: RefCell<String>,
    search_opts: std::cell::Cell<SearchOpts>,
    search_active: std::cell::Cell<Option<(usize, usize)>>,

    /// Active WML text selection, set fire-and-forget
    /// by `docSetSelection` (mirrors `search_active`) and read by the
    /// `paint_wml_page` selection pass. `None` = cleared → the pass is FULLY
    /// SKIPPED, so a no-selection render is BYTE-IDENTICAL. Stored NORMALIZED
    /// (`start <= end` via `TextPos` Ord). A COLLAPSED range (caret) is stored as
    /// `Some` but paints nothing (`selection_rects_impl` yields zero rects) —
    /// reserved for future caret use. `Cell` (not `RefCell`) because `TextPos`
    /// is `Copy`, exactly like `search_active`; interior mutability keeps the
    /// setter `&self`. Logical (para,byte) ⇒ a set never touches layout/geometry.
    selection: std::cell::Cell<Option<(crate::wml::select::TextPos, crate::wml::select::TextPos)>>,

    /// The selected IMAGE OBJECT (its relId), set
    /// fire-and-forget by `set_selected_object` and read by the
    /// `paint_object_outline_pass`. `None` = no object selected → the outline
    /// pass is FULLY SKIPPED, so a no-object render is BYTE-IDENTICAL. Read-only,
    /// outline-only (no handles). `RefCell<Option<String>>` (relId is not `Copy`)
    /// with `&self` interior mutability, exactly like `search_query`. Dropped on
    /// doc switch beside `selection`.
    selected_object: RefCell<Option<String>>,

    /// SML active-match identity: `(flat_page, A1 ref)` of the focused cell. SML
    /// uses cell IDENTITY (not the `search_active` index) because the continuous
    /// grid paints a SCROLL-WINDOWED slice of the page's cells and skips
    /// merge-covered cells — a bare per-page index would mis-emphasise after any
    /// vertical scroll or covered-cell skip. WML/PML keep using `search_active`.
    search_active_cell: RefCell<Option<(usize, String)>>,

    /// Active SML CELL selection, set
    /// fire-and-forget by `set_cell_selection` and read by the
    /// `paint_grid_viewport` selection pass. `None` = cleared → the pass is FULLY
    /// SKIPPED, so a no-selection grid render is BYTE-IDENTICAL (the SML
    /// equivalence pins hold). Grid-view only (print/pages selection is out of
    /// scope). `Cell` (the selection is `Copy`), mirroring the WML `selection`.
    /// SML-only; the docx-only binary never sees it (cfg-gated).
    #[cfg(feature = "sml")]
    cell_selection: std::cell::Cell<Option<sml::select::CellSelection>>,

    /// Streamed Excel print pages — the SML analogue of `pages`, populated by
    /// `SmlRenderer::run_layout`. A parallel cfg-gated field
    /// keeps the WML `PageLayout` model byte-identical for the docx-only build
    /// (no SML fields on `PageLayout`/`Item`); `pageCount`/`renderPage` match on
    /// `LoadedDoc` to pick the right vec. Excluded from the docx-only binary.
    #[cfg(feature = "sml")]
    sml_pages: RefCell<Vec<sml::grid::SheetPage>>,
    /// Streamed PowerPoint slide pages — the PML analogue of `pages` /
    /// `sml_pages`, populated by `PmlRenderer::run_layout`. A
    /// parallel cfg-gated field; the `render_page` / `pageCount` dispatch matches
    /// on `LoadedDoc` to pick the right vec. Excluded from the docx-only binary.
    #[cfg(feature = "pml")]
    pml_pages: RefCell<Vec<pml::shape::SlidePage>>,

    /// The active PML TEXT selection (clamped to one
    /// shape/table-cell on one slide). `Copy`, so a `Cell` beside the
    /// WML `selection` / SML `cell_selection`. `None` ⇒ the PML selection paint
    /// pass is skipped ⇒ byte-identical slide render.
    #[cfg(feature = "pml")]
    pml_selection: std::cell::Cell<Option<pml::select::PmlSelection>>,
    /// The selected PML picture object `(slide, image_key)`
    /// (outline-only, read-only). `RefCell` (the key is not `Copy`). `None` ⇒ the
    /// object-outline pass is skipped ⇒ byte-identical render.
    #[cfg(feature = "pml")]
    pml_selected_object: RefCell<Option<(usize, String)>>,
}

/// A single cell line flattened for row-splitting (§17.4.6/§17.4.7,
/// split at line granularity). `rel_top` is the line's top offset from the cell's
/// *content* top (i.e. excluding the cell's top margin and any vertical-align
/// offset); `height` is its line height. `line` is the fully-built `PagedLine`
/// whose `baseline_y` is stored *relative* to the same content top — a fragment
/// places it by adding its content-top page position. Flattening lets the
/// splitter partition a cell's lines into the lines that fit on the current
/// page and the remainder carried to a continuation fragment.
struct CellLineProto {
    rel_top: f64,
    height: f64,
    line: PagedLine,
}

/// Flatten a table cell's blocks into content-relative line protos, mirroring
/// the whole-row emit walk (paragraphs with before/after gaps + nested tables)
/// but recording each line's offset from the cell content top instead of an
/// absolute baseline. `cell_x` is the cell's left edge on the page (constant
/// across a split, since fragments stay in the same column). Used only on the
/// split path; the whole-row path keeps its original inline emit byte-for-byte.
fn flatten_cell_lines(cell: &crate::wml::table::TableCellCache, cell_x: f64) -> Vec<CellLineProto> {
    let mut protos = Vec::new();
    let mut cur: f64 = 0.0; // running offset from cell content top
    let mut prev_after: f64 = 0.0;
    for block in &cell.blocks {
        match block {
            CellBlock::Paragraph(pc) => {
                cur += prev_after.max(pc.layout.space_before_px);
                for (li, line) in pc.lines.iter().enumerate() {
                    let lh = pc.layout.line_height(line.max_font_pt, line.max_ascent + line.max_descent, line.max_image_height);
                    let indent_extra = if li == 0 { pc.layout.indent_first_line_px } else { 0.0 };
                    let x_start = pc.layout.x_start(
                        cell_x + cell.width_px,
                        cell_x + cell.mar_left,
                        cell.mar_left,
                        line.width(),
                        indent_extra,
                    );
                    protos.push(CellLineProto {
                        rel_top: cur,
                        height: lh,
                        line: PagedLine {
                            segments: PagedLine::single(line.items.clone(), x_start),
                            para: pc.para_id.unwrap_or(PARA_SENTINEL),
                            baseline_y: cur + line.max_ascent,
                            para_shading: None,
                            line_height: lh,
                            content_left: x_start,
                            content_right: cell_x + cell.width_px - cell.mar_right,
                            border_top: None, border_bottom: None,
                            border_left: None, border_right: None,
                            bar_tabs: pc.tab_stops.bar_xs(
                                cell_x + cell.mar_left + pc.layout.indent_left_px,
                                cell_x + cell.width_px - cell.mar_right - pc.layout.indent_right_px,
                                pc.layout.base_rtl),
                            is_first_line: false, is_last_line: false,
                        },
                    });
                    cur += lh;
                }
                prev_after = pc.layout.space_after_px;
            }
            CellBlock::Table(nested) => {
                cur += prev_after;
                let nested_x = cell_x + cell.mar_left;
                for nrow in &nested.rows {
                    let nrow_top = cur;
                    for ncell in &nrow.cells {
                        if ncell.v_merged_away { continue; }
                        let ncell_x = nested_x + nested.col_widths_px[..ncell.col_start].iter().sum::<f64>();
                        let mut ny = nrow_top + nested.cell_margin_v;
                        for nb in &ncell.blocks {
                            if let CellBlock::Paragraph(npc) = nb {
                                for (nli, nline) in npc.lines.iter().enumerate() {
                                    let nlh = npc.layout.line_height(nline.max_font_pt, nline.max_ascent + nline.max_descent, nline.max_image_height);
                                    let nindent = if nli == 0 { npc.layout.indent_first_line_px } else { 0.0 };
                                    let nx = npc.layout.x_start(
                                        ncell_x + ncell.width_px,
                                        ncell_x + nested.cell_margin_h,
                                        nested.cell_margin_h,
                                        nline.width(), nindent,
                                    );
                                    protos.push(CellLineProto {
                                        rel_top: ny,
                                        height: nlh,
                                        line: PagedLine {
                                            segments: PagedLine::single(nline.items.clone(), nx),
                                            para: npc.para_id.unwrap_or(PARA_SENTINEL),
                                            baseline_y: ny + nline.max_ascent,
                                            para_shading: None,
                                            line_height: nlh,
                                            content_left: ncell_x + nested.cell_margin_h,
                                            content_right: ncell_x + ncell.width_px - nested.cell_margin_h,
                                            border_top: None, border_bottom: None,
                                            border_left: None, border_right: None,
                                            bar_tabs: npc.tab_stops.bar_xs(
                                                ncell_x + nested.cell_margin_h + npc.layout.indent_left_px,
                                                ncell_x + ncell.width_px - nested.cell_margin_h - npc.layout.indent_right_px,
                                                npc.layout.base_rtl),
                                            is_first_line: false, is_last_line: false,
                                        },
                                    });
                                    ny += nlh;
                                }
                            }
                        }
                    }
                    cur += nrow.height_px;
                }
                prev_after = 0.0;
            }
        }
    }
    protos
}

/// Resolve a bookmark (or synthetic comment) anchor `name` to its top `y` on a
/// laid-out page, if any line on the page carries it. Walks table-cell lines so
/// anchors inside tables resolve too. Shared by `bookmark_position` and
/// `comments()` (the comment range-start join). Read-only; never touches the
/// geometry-affecting line-packing loops.
fn bookmark_page_y(page: &PageLayout, name: &str) -> Option<f64> {
    fn line_has(line: &PagedLine, name: &str) -> bool {
        line.segments.iter().any(|seg| {
            seg.items.iter().any(|it| it.bookmark.as_deref() == Some(name))
        })
    }
    for el in &page.elements {
        match el {
            PageElement::Line(line) => {
                if line_has(line, name) {
                    return Some(line.baseline_y - line.line_height * 0.85);
                }
            }
            PageElement::TableRow(row) => {
                for cell in &row.cells {
                    for line in &cell.lines {
                        if line_has(line, name) {
                            return Some(line.baseline_y - line.line_height * 0.85);
                        }
                    }
                }
            }
        }
    }
    None
}

#[wasm_bindgen]
impl Viewer {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<Viewer, JsValue> {
        Ok(Viewer {
            loaded: LoadedDoc::None,
            page_size: PageGeometry::us_letter(),
            pages: RefCell::new(Vec::new()),
            para_meta: RefCell::new(Vec::new()),
            table_meta: RefCell::new(HashMap::new()),
            font_cache: FontCache::new(),
            images: RefCell::new(HashMap::new()),
            image_rel_ids: RefCell::new(HashSet::new()),
            image_bitmaps: RefCell::new(HashMap::new()),
            chart_xml: RefCell::new(HashMap::new()),
            #[cfg(feature = "emf")]
            emf_pics: RefCell::new(HashMap::new()),
            doc_dates: RefCell::new(field::DocDates::default()),
            layout_gen: std::cell::Cell::new(0),
            memory_budget_bytes: std::cell::Cell::new(DEFAULT_MEMORY_BUDGET_BYTES),
            layout_truncated_at: std::cell::Cell::new(None),
            #[cfg(not(target_arch = "wasm32"))]
            test_memory_bytes: std::cell::Cell::new(0),
            layout_stats: std::cell::Cell::new(LayoutStats::default()),
            #[cfg(test)]
            force_capture: std::cell::Cell::new(false),
            #[cfg(test)]
            checkpoint_log: RefCell::new(Vec::new()),
            #[cfg(test)]
            violation_log: RefCell::new(Vec::new()),
            #[cfg(test)]
            rollback_log: RefCell::new(Vec::new()),
            #[cfg(test)]
            resume_directive: RefCell::new(None),
            #[cfg(test)]
            push_records: RefCell::new(Vec::new()),
            search_query: RefCell::new(String::new()),
            search_opts: std::cell::Cell::new(SearchOpts::default()),
            search_active: std::cell::Cell::new(None),
            selection: std::cell::Cell::new(None),
            selected_object: RefCell::new(None),
            search_active_cell: RefCell::new(None),
            #[cfg(feature = "sml")]
            cell_selection: std::cell::Cell::new(None),
            #[cfg(feature = "sml")]
            sml_pages: RefCell::new(Vec::new()),
            #[cfg(feature = "pml")]
            pml_pages: RefCell::new(Vec::new()),
            #[cfg(feature = "pml")]
            pml_selection: std::cell::Cell::new(None),
            #[cfg(feature = "pml")]
            pml_selected_object: RefCell::new(None),
        })
    }

    /// Signal any in-flight `run_layout` to stop at its next page boundary.
    /// Called from JS before `loadDocx` when switching documents, so the prior
    /// layout's borrow is released before a new (&mut self) load runs. `&self`
    /// is safe to call while `run_layout` (also &self) is suspended at an await.
    #[wasm_bindgen(js_name = abortLayout)]
    pub fn abort_layout(&self) {
        self.layout_gen.set(self.layout_gen.get().wrapping_add(1));
    }

    /// Fixpoint counters for the MOST RECENT layout run. Test hook
    /// — the perf contract's zero-asserts and re-lay bounds read this.
    #[allow(dead_code)] // read only by the perf-gate tests (cfg(test))
    pub(crate) fn layout_stats(&self) -> LayoutStats {
        self.layout_stats.get()
    }

    /// Total linear memory right now. On wasm this is the real,
    /// irreversible number (`memory.grow` has no inverse, so it only rises);
    /// natively it is the test-injected stand-in, default 0.
    fn linear_memory_bytes(&self) -> u64 {
        #[cfg(target_arch = "wasm32")]
        {
            core::arch::wasm32::memory_size::<0>() as u64 * 65536
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.test_memory_bytes.get()
        }
    }

    /// True once THIS layout run must stop at its next page boundary: either
    /// the budget is exceeded now, or a prior check already stamped the
    /// truncation (an allocation failure in `push_page` sets the stamp
    /// directly, and the loop must not keep laying pages it cannot store).
    /// A budget of 0 disables the check entirely.
    fn should_truncate_layout(&self) -> bool {
        if self.layout_truncated_at.get().is_some() {
            return true;
        }
        let budget = self.memory_budget_bytes.get();
        budget != 0 && self.linear_memory_bytes() > budget
    }

    /// Total linear memory in bytes, for the main thread's
    /// worker-recycle decision (a worker whose PEAK has grown large is worth
    /// replacing on the next document switch, because `memory.grow` has no
    /// inverse and the peak is otherwise resident forever). f64 because u64
    /// does not cross wasm-bindgen losslessly; exact up to 2^53, far above
    /// the 4 GiB wall.
    #[wasm_bindgen(js_name = memoryBytes)]
    pub fn memory_bytes_js(&self) -> f64 {
        self.linear_memory_bytes() as f64
    }

    /// `Some(n)` iff the most recent layout stopped early after `n` pages —
    /// the signal behind a "showing the first n pages" UI. `None` = complete.
    #[wasm_bindgen(js_name = layoutTruncatedAt)]
    pub fn layout_truncated_at_js(&self) -> Option<u32> {
        self.layout_truncated_at.get()
    }

    /// Set the memory budget in bytes (0 disables; see the field doc — the
    /// default is provisional). Takes effect at the next page boundary, so it
    /// may be called while a layout is in flight.
    ///
    /// Headroom note: there is deliberately NO link-time memory cap
    /// (declined — the wall traps identically with or without one). Keep the
    /// budget ≥ ~1.5 GiB below the 4 GiB wasm32 wall; the margin is
    /// legitimate transient staging (doc bytes, unzip, image bytes
    /// pre-decode) that must fit ABOVE where layout stops.
    #[wasm_bindgen(js_name = setMemoryBudget)]
    pub fn set_memory_budget(&self, bytes: f64) {
        self.memory_budget_bytes.set(bytes as u64);
    }

    /// Native test hook: inject linear-memory pressure.
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(dead_code)] // read only by the truncation tests (cfg(test))
    pub(crate) fn set_test_memory_bytes(&self, bytes: u64) {
        self.test_memory_bytes.set(bytes);
    }

    /// Parse a DOCX from raw bytes. Layout does not run here — call
    /// `run_layout()` (which can stream pages back via callback).
    #[wasm_bindgen(js_name = loadDocx)]
    pub fn load_docx(&mut self, bytes: Vec<u8>) -> Result<(), JsValue> {
        package_limits::check(&bytes).map_err(|e| JsValue::from_str(&e.to_string()))?;
        // Tolerant load (RULE 2): try the original bytes first; ONLY on a
        // missing-required-attribute failure (e.g. a gutter-less
        // `<w:pgMar>` from a third-party generator) do we inject the
        // missing pgMar attrs and re-parse. Valid files parse on the first
        // attempt and never reach the repair.
        let mut doc = repair::load_docx_tolerant(bytes)
            .map_err(|e| JsValue::from_str(&format!("docx parse error: {e}")))?;
        self.page_size = PageGeometry::from_doc(&doc);
        // The document line grid only snaps Latin body text when the document is
        // NOT in Far-East layout (§17.6.5). Detect `useFELayout` from settings.xml
        // with the same cheap string match used for `evenAndOddHeaders`.
        let grid_enabled = doc.package_mut().read_part("word/settings.xml")
            .map_or(true, |xml| {
                std::str::from_utf8(&xml).map_or(true, |s| !s.contains("useFELayout"))
            });
        // §17.15.3.8 ulTrailSpace: underline end-of-line trailing spaces too
        // (default off when the element is absent).
        let ul_trail_space = doc.package_mut().read_part("word/settings.xml")
            .is_ok_and(|xml| {
                std::str::from_utf8(&xml).is_ok_and(|s| s.contains("ulTrailSpace"))
            });
        // `compatibilityMode` < 15 → Word's legacy line-fitter, which reserves
        // part of a word's trailing space at line end (gates pack_lines'
        // half-space reserve). Parse the value of the `compatibilityMode`
        // compatSetting; absent → treat as legacy (reserve ON): Word opens a
        // compatibilityMode-less docx in Compatibility Mode (e.g. files from
        // third-party generators), so the legacy fitter is the Word-matching default.
        // NB: `compatibilityMode` is a Word/MS-DOCX convention, not ECMA-376
        // Part 1 normative text — the absent-default is implementation-defined.
        let reserve_trailing_blanks = doc.package_mut().read_part("word/settings.xml").ok()
            .and_then(|xml| {
                let s = std::str::from_utf8(&xml).ok()?;
                let after = &s[s.find("compatibilityMode")?..];
                let vi = after.find("w:val=\"")? + "w:val=\"".len();
                let ve = after[vi..].find('"')?;
                after[vi..vi + ve].trim().parse::<u32>().ok()
            })
            .is_none_or(|mode| mode < 15);
        // §17.15.1.x suppressTopSpacing: suppress the first paragraph's
        // before-spacing at a page top. Absent → false → apply it (Word's
        // default; the prior unconditional page-top suppression dropped e.g. a
        // Title's 12pt before-spacing at the document top).
        let suppress_top_spacing = doc.package_mut().read_part("word/settings.xml")
            .is_ok_and(|xml| {
                std::str::from_utf8(&xml).is_ok_and(|s| s.contains("suppressTopSpacing"))
            });
        self.pages.borrow_mut().clear();
        self.layout_truncated_at.set(None); // fresh doc/run starts untruncated
        self.para_meta.borrow_mut().clear(); // drop stale selection metadata
        self.table_meta.borrow_mut().clear(); // drop stale table structure
        self.selection.set(None); // drop stale selection on doc switch
        #[cfg(feature = "sml")]
        self.cell_selection.set(None); // drop stale SML cell selection on doc switch
        self.selected_object.borrow_mut().take(); // drop object selection on doc switch
        self.images.borrow_mut().clear();
        self.image_rel_ids.borrow_mut().clear(); // clear the retained image-rel set
        self.image_bitmaps.borrow_mut().clear();
        self.chart_xml.borrow_mut().clear();
        #[cfg(feature = "emf")]
        self.emf_pics.borrow_mut().clear();

        let rel_ids: Vec<String> = {
            let mut ids: Vec<String> = Vec::new();
            let mut collect_from_para = |p: &ooxml_wml::types::Paragraph| {
                for run in p.runs() {
                    for drawing in run.drawings() {
                        for rel_id in drawing.all_image_rel_ids() {
                            ids.push(rel_id.to_string());
                        }
                    }
                }
            };
            for block in &doc.body().block_content {
                match block {
                    BlockContent::P(p) => collect_from_para(p),
                    BlockContent::Tbl(t) => {
                        for row in t.rows() {
                            for cell in collect_row_cells(row) {
                                for para in collect_cell_paragraphs(cell) {
                                    collect_from_para(para);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            ids
        };

        for rel_id in rel_ids {
            if self.images.borrow().contains_key(&rel_id) {
                continue; // dedupe — same image referenced multiple times
            }
            match doc.get_image_data(&rel_id) {
                Ok(image_data) => {
                    // Parse an inline EMF ONCE into the cached drawing program
                    // (parse-once, not per repaint/zoom). Feature-off: this cfg
                    // block vanishes and the arm is byte-identical to today.
                    #[cfg(feature = "emf")]
                    if image_data.content_type == "image/x-emf" {
                        if let Some(mut pic) = emf::parse(&image_data.data) {
                            pic.rel = rel_id.clone();
                            self.emf_pics
                                .borrow_mut()
                                .insert(rel_id.clone(), std::rc::Rc::new(pic));
                        }
                    }
                    self.images.borrow_mut().insert(
                        rel_id,
                        ImageBytes {
                            mime: image_data.content_type,
                            data: image_data.data,
                        },
                    );
                }
                Err(e) => {
                    // Log + continue — render time falls back to placeholder.
                    console_warn(&format!("image extract failed for {rel_id}: {e}"));
                }
            }
        }

        // Extract chart fallback images. Each chart part has its own .rels
        // that may reference a fallback PNG. We store these with a namespaced
        // key and record the mapping so collect_items can resolve chart drawings.
        {
            let mut chart_rel_ids: Vec<String> = Vec::new();
            let mut collect_charts = |p: &ooxml_wml::types::Paragraph| {
                for run in p.runs() {
                    for drawing in run.drawings() {
                        for rel_id in drawing.all_chart_rel_ids() {
                            chart_rel_ids.push(rel_id.to_string());
                        }
                    }
                }
            };
            for block in &doc.body().block_content {
                match block {
                    BlockContent::P(p) => collect_charts(p),
                    BlockContent::Tbl(t) => {
                        for row in t.rows() {
                            for cell in collect_row_cells(row) {
                                for para in collect_cell_paragraphs(cell) {
                                    collect_charts(para);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }

            for chart_rel_id in chart_rel_ids {
                let chart_path = match doc.doc_relationships().get(&chart_rel_id) {
                    Some(rel) => {
                        if rel.target.starts_with('/') {
                            rel.target.trim_start_matches('/').to_string()
                        } else {
                            format!("word/{}", rel.target.trim_start_matches("word/").trim_start_matches('/'))
                        }
                    }
                    None => continue,
                };

                let chart_key = format!("chart::{}", chart_rel_id);
                if let Ok(xml_bytes) = doc.package_mut().read_part(&chart_path) {
                    self.chart_xml.borrow_mut().insert(chart_key.clone(), xml_bytes);
                }

                let chart_rels = match doc.package_mut().read_part_relationships(&chart_path) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                for rel in chart_rels.iter() {
                    if !rel.relationship_type.contains("/image") { continue; }
                    let image_path = if rel.target.starts_with('/') {
                        rel.target.trim_start_matches('/').to_string()
                    } else if let Some(slash) = chart_path.rfind('/') {
                        format!("{}/{}", &chart_path[..slash], &rel.target)
                    } else {
                        rel.target.clone()
                    };

                    let fallback_key = format!("chart::{}", chart_rel_id);
                    if self.images.borrow().contains_key(&fallback_key) { break; }

                    if let Ok(data) = doc.package_mut().read_part(&image_path) {
                        let ext = image_path.rsplit('.').next().unwrap_or("png");
                        let mime = match ext {
                            "jpg" | "jpeg" => "image/jpeg",
                            "gif" => "image/gif",
                            _ => "image/png",
                        };
                        self.images.borrow_mut().insert(fallback_key, ImageBytes {
                            mime: mime.to_string(),
                            data,
                        });
                    }
                    break; // one fallback per chart
                }
            }
        }

        let theme_fonts = doc.package_mut()
            .read_part_string("word/theme/theme1.xml")
            .map(|xml| parse_theme_fonts(&xml))
            .unwrap_or_else(|_| ThemeFonts::empty());

        // Core properties: created / modified dates for CREATEDATE / SAVEDATE.
        if let Ok(core_xml) = doc.package_mut().read_part_string("docProps/core.xml") {
            let mut dates = self.doc_dates.borrow_mut();
            dates.created = extract_core_date(&core_xml, "created");
            dates.modified = extract_core_date(&core_xml, "modified");
        }

        // StyleContext is built BEFORE the numbering lookup so the §17.9.21
        // numStyleLink chase can read numbering styles (`doc.styles()` is `&self`
        // over the eagerly-parsed styles; `from_styles` deep-clones into an owned
        // map, so the borrow ends before the `&mut doc` calls below).
        let mut style_ctx = StyleContext::from_styles(doc.styles());
        // Synthesize Word's built-in Header/Footer styles when styles.xml
        // does not define them (insert-only-when-absent — see the fn docs).
        // BEFORE build_numbering_lookup is harmless: the injected ids are
        // disjoint from numbering-style ids and the map is only ever read by
        // keyed pStyle lookups, never iterated.
        inject_builtin_hdr_ftr_styles(&mut style_ctx);
        let numbering_levels = build_numbering_lookup(&mut doc, &style_ctx);

        let mut footnotes_map: HashMap<i64, FootnoteEndnote> = HashMap::new();
        let mut endnotes_map: HashMap<i64, FootnoteEndnote> = HashMap::new();
        if let Ok(footnotes) = doc.get_footnotes() {
            for fn_entry in footnotes.footnote {
                if fn_entry.id >= 0 {
                    footnotes_map.insert(fn_entry.id, fn_entry);
                }
            }
        }
        if let Ok(endnotes) = doc.get_endnotes() {
            for en_entry in endnotes.endnote {
                if en_entry.id >= 0 {
                    endnotes_map.insert(en_entry.id, en_entry);
                }
            }
        }
        // Comments (word/comments.xml). Display metadata only — positions are
        // resolved post-layout by the `comments()` accessor. `get_comments`
        // needs `&mut doc`, so we extract here (the &mut-self load path) into an
        // owned `Vec<CommentMeta>`. `date`/`initials` come from the
        // `wml-comments` feature (enabled on the ooxml-wml dep).
        let comments_meta: Vec<CommentMeta> = match doc.get_comments() {
            Ok(comments) => comments
                .comment
                .iter()
                .map(|c| {
                    let text = c
                        .block_content
                        .iter()
                        .filter_map(|b| match b {
                            ooxml_wml::types::BlockContent::P(p) => Some(p.text()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    CommentMeta {
                        id: c.id,
                        author: c.author.clone(),
                        date: c.date.clone(),
                        initials: c.initials.clone(),
                        text,
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        };

        let default_ppr = doc.styles()
            .doc_defaults.as_ref()
            .and_then(|dd| dd.p_pr_default.as_ref())
            .and_then(|ppd| ppd.p_pr.as_ref())
            .map(|p| p.as_ref().clone());
        let default_tab_stop_px = doc.get_settings().ok()
            .and_then(|s| s.default_tab_stop)
            .map(|v| twips_to_px(v as i64))
            .unwrap_or(twips_to_px(720));

        // Map hyperlink relationship ids → external URL targets for the
        // clickable-link layer (read in collect_items via HYPERLINK_RELS).
        // EXTERNAL rels only: per OPC (ECMA-376 Part 2) an absent TargetMode
        // is Internal, and an Internal target is a package part path resolved
        // relative to this part — reporting it as a URL misled consumers into
        // treating "attachedPart.xml" (or worse) as a navigable href.
        {
            let mut map = HashMap::new();
            for rel in doc.doc_relationships().iter() {
                if rel.relationship_type.contains("/hyperlink") && rel.is_external() {
                    map.insert(rel.id.clone(), rel.target.clone());
                }
            }
            HYPERLINK_RELS.with(|h| *h.borrow_mut() = map);
        }

        let mut sections = build_sections(&mut doc);

        // Extract images referenced from header/footer parts. Each header/footer
        // has its own .rels namespace, so rIds are prefixed with the part path
        // to avoid collisions with document-level rIds.
        for section in &mut sections {
            extract_hdr_ftr_images(
                &mut doc, &mut section.hdr_ftr, &mut self.images.borrow_mut(),
            );
        }

        // Document watermark: scan every section's headers (default/first/even)
        // for the first WordArt textpath or image. One watermark per document is
        // the overwhelmingly common authoring (whole-doc DRAFT/CONFIDENTIAL).
        let image_keys: HashSet<String> = self.images.borrow().keys().cloned().collect();
        // Retain the image-rel SET independently of
        // the bytes: `clearImageBytes` frees the values but this survives so the
        // object hit-test (`is_image`) keeps working post-decode. Charts are
        // absent from `images`, so this set naturally excludes them.
        *self.image_rel_ids.borrow_mut() = image_keys.clone();
        let watermark = sections.iter().find_map(|section| {
            let parts = &section.hdr_ftr.part_paths;
            [
                section.hdr_ftr.default_header.as_deref(),
                section.hdr_ftr.first_header.as_deref(),
                section.hdr_ftr.even_header.as_deref(),
            ]
            .into_iter()
            .flatten()
            .find_map(|blocks| find_watermark_in_blocks(blocks, parts, &image_keys))
        });

        self.loaded = LoadedDoc::Wml(WmlRenderer {
            doc,
            sections,
            watermark,
            default_tab_stop_px,
            grid_enabled,
            ul_trail_space,
            reserve_trailing_blanks,
            suppress_top_spacing,
            numbering_levels,
            style_ctx,
            default_ppr,
            theme_fonts,
            footnotes: footnotes_map,
            endnotes: endnotes_map,
            comments: comments_meta,
        });
        Ok(())
    }

    /// Parse an XLSX from raw bytes. Additive sibling of
    /// `loadDocx`: clears shared state, builds an `SmlRenderer` (#56-tolerant
    /// shim; never `resolved_sheets()`), drains its picture media, sets
    /// `page_size`, installs `LoadedDoc::Sml`. Layout runs via `runLayout()`.
    #[cfg(feature = "sml")]
    #[wasm_bindgen(js_name = loadXlsx)]
    pub fn load_xlsx(&mut self, bytes: Vec<u8>) -> Result<(), JsValue> {
        package_limits::check(&bytes).map_err(|e| JsValue::from_str(&e.to_string()))?;
        // Clear shared engine state (matches loadDocx's prologue).
        self.pages.borrow_mut().clear();
        self.layout_truncated_at.set(None); // fresh doc/run starts untruncated
        self.para_meta.borrow_mut().clear(); // drop stale selection metadata
        self.table_meta.borrow_mut().clear(); // drop stale table structure
        self.selection.set(None); // drop stale selection on doc switch
        #[cfg(feature = "sml")]
        self.cell_selection.set(None); // drop stale SML cell selection on doc switch
        self.selected_object.borrow_mut().take(); // drop object selection on doc switch
        self.sml_pages.borrow_mut().clear();
        self.images.borrow_mut().clear();
        self.image_rel_ids.borrow_mut().clear(); // clear the retained image-rel set
        self.image_bitmaps.borrow_mut().clear();
        self.chart_xml.borrow_mut().clear();
        let mut renderer = sml::SmlRenderer::from_xlsx(bytes)?;
        // Drain the picture media bytes into the shared `images` map for the
        // JS decode pipeline (imageRefs → createImageBitmap → setImageBitmap) —
        // the exact loadPptx contract; keys are `sml::{sheetIdx}::{relId}`.
        for (key, img) in renderer.take_images() {
            self.images.borrow_mut().insert(key, img);
        }
        // Drain the renderer's chart part XML into the shared `chart_xml` map so
        // the grid/print paint and the copy seams (`chartData` /
        // `renderChartToCanvas`) feed the SHARED `crate::chart::render_chart`
        // (the exact loadPptx contract). Keys are sheet-namespaced
        // (`chart::sml::{sheetIdx}::{relId}`) and cleared on every doc switch.
        for (key, xml) in renderer.take_charts() {
            self.chart_xml.borrow_mut().insert(key, xml);
        }
        // The SML print-page box → shared page_size (not yet per-sheet).
        let pb = renderer.page_box();
        let mut geo = PageGeometry::us_letter();
        geo.width_px = pb.width;
        geo.height_px = pb.height;
        self.page_size = geo;
        self.loaded = LoadedDoc::Sml(renderer);
        Ok(())
    }

    /// Parse a PPTX from raw bytes. Additive sibling of
    /// `loadDocx`/`loadXlsx`: clears the shared page/image/chart state, builds
    /// a `PmlRenderer` (parses the presentation, reads the slide size), sets
    /// `page_size` to the slide box (§19.2.1.39, EMU → px), and installs
    /// `LoadedDoc::Pml`. Layout runs via `runLayout()` (one page per slide).
    #[cfg(feature = "pml")]
    #[wasm_bindgen(js_name = loadPptx)]
    pub fn load_pptx(&mut self, bytes: Vec<u8>) -> Result<(), JsValue> {
        package_limits::check(&bytes).map_err(|e| JsValue::from_str(&e.to_string()))?;
        // Clear shared engine state (matches loadDocx/loadXlsx's prologue).
        self.pages.borrow_mut().clear();
        self.layout_truncated_at.set(None); // fresh doc/run starts untruncated
        self.para_meta.borrow_mut().clear(); // drop stale selection metadata
        self.table_meta.borrow_mut().clear(); // drop stale table structure
        self.selection.set(None); // drop stale selection on doc switch
        #[cfg(feature = "sml")]
        self.cell_selection.set(None); // drop stale SML cell selection on doc switch
        self.selected_object.borrow_mut().take(); // drop object selection on doc switch
        self.pml_pages.borrow_mut().clear();
        self.images.borrow_mut().clear();
        self.image_rel_ids.borrow_mut().clear(); // clear the retained image-rel set
        self.image_bitmaps.borrow_mut().clear();
        self.chart_xml.borrow_mut().clear();

        let mut renderer = pml::PmlRenderer::from_pptx(bytes)?;
        // Drain the renderer's picture media bytes into the shared `images` map
        // so the JS decode pipeline (imageRefs → createImageBitmap →
        // setImageBitmap) turns them into `ImageBitmap`s the slide paint draws
        // (mirrors the WML/SML extract-at-load contract). Keys are already
        // slide-namespaced (`pml::{slideIdx}::{relId}`).
        for (key, img) in renderer.take_images() {
            self.images.borrow_mut().insert(key, img);
        }
        // Drain the renderer's chart part XML into the shared `chart_xml` map so
        // the slide paint can feed it to the shared `crate::chart::render_chart`
        // (the WML chart registry, extended to slide-namespaced keys
        // `chart::pml::{slideIdx}::{relId}`).
        for (key, xml) in renderer.take_charts() {
            self.chart_xml.borrow_mut().insert(key, xml);
        }
        // The slide box → shared page_size (uniform across slides).
        let (w, h) = renderer.page_box();
        let mut geo = PageGeometry::us_letter();
        geo.width_px = w;
        geo.height_px = h;
        self.page_size = geo;
        self.loaded = LoadedDoc::Pml(renderer);
        Ok(())
    }

    /// Load a font file into WASM memory for text shaping. Must be called
    /// before `runLayout()`. The same font bytes should also be registered
    /// in the browser via the FontFace API for `fillText` rendering.
    #[wasm_bindgen(js_name = loadFont)]
    pub fn load_font(&mut self, family: &str, data: Vec<u8>, weight: u16, italic: bool) -> Result<(), JsValue> {
        self.font_cache.insert(family, data, weight, italic)
            .map_err(|e| JsValue::from_str(&e))
    }

    /// Register a font family alias so that runs referencing `from_family`
    /// use fonts loaded under `to_family`.
    #[wasm_bindgen(js_name = addFontAlias)]
    pub fn add_font_alias(&mut self, from_family: &str, to_family: &str) {
        self.font_cache.add_alias(from_family, to_family);
    }

    /// Scan the loaded document and return the set of unique font family
    /// names referenced by runs (including style chain resolution).
    #[wasm_bindgen(js_name = fontFamiliesUsed)]
    pub fn font_families_used(&self) -> JsValue {
        let array = js_sys::Array::new();
        for family in self.families_used_native() {
            array.push(&JsValue::from_str(&family));
        }
        array.into()
    }

    /// Rust-side family collection behind `fontFamiliesUsed` (and the
    /// debug-probe `debugFonts` per-family report, which must ask about the SAME
    /// requested names the TS font plan was built from). No js_sys — natively
    /// testable and callable from other exports.
    fn families_used_native(&self) -> Vec<String> {
        let mut families = HashSet::new();
        // SML: the workbook reports its stylesheet fonts directly (normal +
        // per-cell families) so the JS font plan loads substitutes before
        // layout — same contract as the WML scan below.
        #[cfg(feature = "sml")]
        if let LoadedDoc::Sml(s) = &self.loaded {
            let mut v = s.font_families();
            v.push(DEFAULT_FONT_FAMILY.to_string());
            return v;
        }
        // PML: slide runs report their latin typefaces, plus the engine
        // default. Same contract as the SML arm above.
        #[cfg(feature = "pml")]
        if let LoadedDoc::Pml(p) = &self.loaded {
            let mut v = p.font_families();
            v.push(DEFAULT_FONT_FAMILY.to_string());
            return v;
        }
        let Some(w) = self.wml() else {
            return Vec::new();
        };

        let collect_from_blocks = |blocks: &[BlockContent], families: &mut HashSet<String>, w: &WmlRenderer| {
            for block in blocks {
                match block {
                    BlockContent::P(para) => w.collect_para_fonts(para, families),
                    BlockContent::Tbl(table) => {
                        for row in table.rows() {
                            for cell in collect_row_cells(row) {
                                for para in collect_cell_paragraphs(cell) {
                                    w.collect_para_fonts(para, families);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        };

        for section in &w.sections {
            collect_from_blocks(&section.blocks, &mut families, w);
            for blocks in [
                &section.hdr_ftr.default_header, &section.hdr_ftr.first_header,
                &section.hdr_ftr.even_header, &section.hdr_ftr.default_footer,
                &section.hdr_ftr.first_footer, &section.hdr_ftr.even_footer,
            ].into_iter().flatten() {
                collect_from_blocks(blocks, &mut families, w);
            }
        }

        // Always include the fonts the layout falls back to when a run declares
        // no explicit font (collect_para_fonts only emits *explicit* families).
        // A doc with no styles.xml / no rFonts (e.g. the bookmark fixture)
        // otherwise yields an empty set, so #loadFonts loads nothing, the
        // FontCache stays empty, and shaping produces zero-advance glyphs —
        // text collapses and overlaps until some *other* document warms the
        // (persistent) cache. Seed the default family plus the theme major/minor
        // Latin fonts (the asciiTheme/docDefaults fallbacks) so they are always
        // fetched and present for the very first layout.
        families.insert(DEFAULT_FONT_FAMILY.to_string());
        if let Some(ref minor) = w.theme_fonts.minor_latin {
            families.insert(minor.clone());
        }
        if let Some(ref major) = w.theme_fonts.major_latin {
            families.insert(major.clone());
        }

        // OMML: seed the math family so the JS font plan fetches XITS
        // Math (with its real MATH table) and registers a FontFace before layout
        // — but ONLY when the document actually contains an oMath/oMathPara, so a
        // math-free doc never pays the ~548 KB fetch.
        if self.wml_has_math() {
            families.insert(crate::engine::fonts::MATH_FAMILY.to_string());
        }

        families.into_iter().collect()
    }

    /// True when the loaded WML document contains any OMML zone — the gate for
    /// seeding [`crate::engine::fonts::MATH_FAMILY`] into the font plan.
    fn wml_has_math(&self) -> bool {
        let Some(w) = self.wml() else {
            return false;
        };
        let block_has_math = |blocks: &[BlockContent]| -> bool {
            blocks.iter().any(|block| match block {
                BlockContent::P(para) => crate::wml::math::paragraph_has_math(para),
                BlockContent::Tbl(table) => table.rows().iter().any(|row| {
                    collect_row_cells(row).into_iter().any(|cell| {
                        collect_cell_paragraphs(cell)
                            .into_iter()
                            .any(crate::wml::math::paragraph_has_math)
                    })
                }),
                _ => false,
            })
        };
        w.sections.iter().any(|section| {
            block_has_math(&section.blocks)
                || [
                    &section.hdr_ftr.default_header, &section.hdr_ftr.first_header,
                    &section.hdr_ftr.even_header, &section.hdr_ftr.default_footer,
                    &section.hdr_ftr.first_footer, &section.hdr_ftr.even_footer,
                ]
                .into_iter()
                .flatten()
                .any(|blocks| block_has_math(blocks))
        })
    }

    /// Scan the loaded document and return codepoints used per font family.
    /// Returns a JS object: `{ "Calibri": Uint32Array([0x41, 0x42, ...]), ... }`
    #[wasm_bindgen(js_name = fontCodepointsUsed)]
    pub fn font_codepoints_used(&self) -> JsValue {
        let mut family_codepoints: HashMap<String, HashSet<u32>> = HashMap::new();
        let Some(w) = self.wml() else {
            return JsValue::from(js_sys::Object::new());
        };

        let collect_from_blocks = |blocks: &[BlockContent],
                                    fcp: &mut HashMap<String, HashSet<u32>>,
                                    w: &WmlRenderer| {
            for block in blocks {
                match block {
                    BlockContent::P(para) => w.collect_para_codepoints(para, fcp),
                    BlockContent::Tbl(table) => {
                        for row in table.rows() {
                            for cell in collect_row_cells(row) {
                                for para in collect_cell_paragraphs(cell) {
                                    w.collect_para_codepoints(para, fcp);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        };

        for section in &w.sections {
            collect_from_blocks(&section.blocks, &mut family_codepoints, w);
            for blocks in [
                &section.hdr_ftr.default_header, &section.hdr_ftr.first_header,
                &section.hdr_ftr.even_header, &section.hdr_ftr.default_footer,
                &section.hdr_ftr.first_footer, &section.hdr_ftr.even_footer,
            ].into_iter().flatten() {
                collect_from_blocks(blocks, &mut family_codepoints, w);
            }
        }

        let obj = js_sys::Object::new();
        for (family, cps) in &family_codepoints {
            let mut sorted: Vec<u32> = cps.iter().copied().collect();
            sorted.sort();
            let arr = js_sys::Uint32Array::new_with_length(sorted.len() as u32);
            arr.copy_from(&sorted);
            let _ = js_sys::Reflect::set(&obj, &JsValue::from_str(family), &arr.into());
        }
        obj.into()
    }

    /// Return all extracted image references for JS to decode via
    /// `createImageBitmap`. Each entry: `{ relId, mime, bytes: Uint8Array }`.
    #[wasm_bindgen(js_name = imageRefs)]
    pub fn image_refs(&self) -> JsValue {
        let array = js_sys::Array::new();
        for (rel_id, img) in self.images.borrow().iter() {
            // A raw EMF part is a vector metafile, not a decodable bitmap —
            // `createImageBitmap` would reject it. When the `emf` feature is on we
            // route it through `emf::render::paint` and emit its DECODED DIB blits
            // below instead; skip the raw bytes here so JS never fires a
            // guaranteed-failing decode + console warn. (Feature-off: this cfg
            // block vanishes, so the loop is byte-identical to today.)
            #[cfg(feature = "emf")]
            if img.mime == "image/x-emf" {
                continue;
            }
            let obj = js_sys::Object::new();
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("relId"),
                &JsValue::from_str(rel_id),
            );
            let _ = js_sys::Reflect::set(
                &obj,
                &JsValue::from_str("mime"),
                &JsValue::from_str(&img.mime),
            );
            let bytes = js_sys::Uint8Array::new_with_length(img.data.len() as u32);
            bytes.copy_from(&img.data);
            let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("bytes"), &bytes);
            array.push(&obj);
        }
        // Decoded EMF DIB blits as raw-RGBA refs. The JS decode loop branches on
        // the `application/x-emf-dib-rgba` mime to build an `ImageData` →
        // `createImageBitmap(imagedata)` (raw RGBA in a Blob would not decode) and
        // registers each under the synthetic `emf:{rel}:{n}` key the renderer's
        // `draw_image` uses. This mime is never emitted without the feature, so
        // the JS branch is inert for docx-only builds.
        #[cfg(feature = "emf")]
        for (rel, pic) in self.emf_pics.borrow().iter() {
            for (i, blit) in pic.blits.iter().enumerate() {
                let obj = js_sys::Object::new();
                let _ = js_sys::Reflect::set(
                    &obj,
                    &JsValue::from_str("relId"),
                    &JsValue::from_str(&format!("emf:{rel}:{i}")),
                );
                let _ = js_sys::Reflect::set(
                    &obj,
                    &JsValue::from_str("mime"),
                    &JsValue::from_str("application/x-emf-dib-rgba"),
                );
                let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("wPx"), &JsValue::from_f64(blit.w as f64));
                let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("hPx"), &JsValue::from_f64(blit.h as f64));
                let bytes = js_sys::Uint8Array::new_with_length(blit.rgba.len() as u32);
                bytes.copy_from(&blit.rgba);
                let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("bytes"), &bytes);
                array.push(&obj);
            }
        }
        array.into()
    }

    /// Store a decoded ImageBitmap from JS. `&self` because it doesn't need
    /// exclusive access — the bitmap is held in a RefCell.
    #[wasm_bindgen(js_name = setImageBitmap)]
    pub fn set_image_bitmap(&self, rel_id: String, bitmap: ImageBitmap) {
        self.image_bitmaps.borrow_mut().insert(rel_id, bitmap);
    }

    /// Drop the raw image bytes from WASM memory. Called by JS once all
    /// bitmaps have been decoded — after that point only the GPU-backed
    /// `ImageBitmap` handles are needed for rendering.
    #[wasm_bindgen(js_name = clearImageBytes)]
    pub fn clear_image_bytes(&self) {
        self.images.borrow_mut().clear();
    }

    /// Compute layout for the entire document, streaming `pageReady` events
    /// to the supplied JS callback as each page becomes available. Yields to
    /// the browser between pages so the UI stays responsive even on large
    /// documents. Resolves with the final page count when layout completes.
    /// Set the "current time" for DATE/TIME fields, as Unix epoch milliseconds
    /// (UTC). JS passes `Date.now()`; tests pass a fixed value for determinism.
    /// Call before `runLayout`. Without it, DATE/TIME fall back to cached text.
    #[wasm_bindgen(js_name = setNow)]
    pub fn set_now(&self, millis: f64) {
        self.doc_dates.borrow_mut().now = Some(field::DateTime::from_epoch_millis(millis as i64));
    }

    #[wasm_bindgen(js_name = runLayout)]
    pub async fn run_layout(&self, on_page_ready: js_sys::Function) -> Result<u32, JsValue> {
        // Per-format dispatch: the Wml arm is the verbatim original;
        // the Sml arm streams the grid into the shared `sml_pages` via the same
        // page-ready contract. Cloning the JS callback per arm keeps each body
        // self-contained.
        match &self.loaded {
            LoadedDoc::Wml(_) => {
                self.run_layout_impl(move |page_idx| {
                    on_page_ready
                        .call1(&JsValue::null(), &JsValue::from(page_idx))
                        .map(|_| ())
                })
                .await
            }
            #[cfg(feature = "sml")]
            LoadedDoc::Sml(s) => {
                s.run_layout(
                    &self.sml_pages,
                    &self.layout_gen,
                    &self.font_cache,
                    move |page_idx| {
                        on_page_ready
                            .call1(&JsValue::null(), &JsValue::from(page_idx))
                            .map(|_| ())
                    },
                )
                .await
            }
            #[cfg(feature = "pml")]
            LoadedDoc::Pml(p) => {
                p.run_layout(
                    &self.pml_pages,
                    &self.layout_gen,
                    &self.font_cache,
                    move |page_idx| {
                        on_page_ready
                            .call1(&JsValue::null(), &JsValue::from(page_idx))
                            .map(|_| ())
                    },
                )
                .await
            }
            LoadedDoc::None => {
                Err(JsValue::from_str("no document loaded; call loadDocx/loadXlsx/loadPptx first"))
            }
        }
    }
}

impl Viewer {
    /// The loaded Word renderer, if a docx is loaded. The WML match arm used by
    /// every method whose body is Word-specific.
    fn wml(&self) -> Option<&WmlRenderer> {
        match &self.loaded {
            LoadedDoc::Wml(w) => Some(w),
            LoadedDoc::None => None,
            #[cfg(feature = "sml")]
            LoadedDoc::Sml(_) => None,
            #[cfg(feature = "pml")]
            LoadedDoc::Pml(_) => None,
        }
    }

    /// The `PageGeometry` for a page's section (§17.6). A mixed-size
    /// multi-section doc renders each section at its own dimensions; this
    /// resolves the geometry for the section a page was stamped with
    /// (`PageLayout::section_idx`). Falls back to the doc-level `self.page_size`
    /// (= body/last sectPr, or the SML/PML sheet/slide size) when there are no
    /// WML sections or the index is out of range (e.g. a trailing relocation
    /// blank). For a single-section doc `sections[0].geometry` is field-for-field
    /// equal to `self.page_size`, so paint stays byte-identical.
    fn page_geometry(&self, section_idx: usize) -> &PageGeometry {
        self.wml()
            .and_then(|w| w.sections.get(section_idx))
            .map(|s| &s.geometry)
            .unwrap_or(&self.page_size)
    }

    /// Resolve the geometry for a given *page* index (not section index): looks
    /// up the page's stamped `section_idx`, then that section's geometry. An
    /// out-of-range page index falls back to section 0. This is the native core
    /// of `page_size_js` (which returns an opaque `JsValue` and so cannot be
    /// exercised from a native test), extracted so the page→section→geometry
    /// composition — the exact path a mixed-size multi-section doc depends on —
    /// is guarded without JS.
    fn page_geometry_at(&self, page_idx: usize) -> &PageGeometry {
        let section_idx = self
            .pages
            .borrow()
            .get(page_idx)
            .map(|p| p.section_idx)
            .unwrap_or(0);
        self.page_geometry(section_idx)
    }

    /// The entire streaming pagination loop, generic over the page-ready sink
    /// so the native regression harness can drive it without any JS (see
    /// `run_layout_native`). The wasm `runLayout` above passes the JS callback.
    /// Body is the original `run_layout` verbatim; only the callback line in
    /// `push_page` changed.
    async fn run_layout_impl(
        &self,
        on_page: impl FnMut(u32) -> Result<(), JsValue>,
    ) -> Result<u32, JsValue> {
        // RefCell so `push_page` can stay a `Fn` closure (the body calls it
        // from several places, including macros) while the sink is `FnMut`.
        let on_page = RefCell::new(on_page);
        let LoadedDoc::Wml(w) = &self.loaded else {
            return Err(JsValue::from_str("no document loaded; call loadDocx first"));
        };
        if w.sections.is_empty() {
            return Err(JsValue::from_str("no document loaded; call loadDocx first"));
        }
        // Publish document dates so collect_items can resolve DATE/TIME/etc.
        FIELD_DATES.with(|d| d.set(*self.doc_dates.borrow()));
        RESERVE_TRAILING_BLANKS.with(|c| c.set(w.reserve_trailing_blanks));
        // Reset the cross-paragraph open-comment-range carrier so this layout
        // (a fresh document, or a re-layout after a switch) starts with none open.
        OPEN_COMMENT_IDS.with(|c| c.borrow_mut().clear());
        self.layout_stats.set(LayoutStats::default());
        // Restartability self-test state: logs are per-run (a replay run
        // records its own pushes/captures; the driver iterates clones).
        #[cfg(test)]
        {
            self.checkpoint_log.borrow_mut().clear();
            self.violation_log.borrow_mut().clear();
            self.rollback_log.borrow_mut().clear();
            self.push_records.borrow_mut().clear();
        }
        // One-shot resume directive: re-enter from a checkpoint and lay to
        // DOCUMENT END. A replay run is a NORMAL layout run — pages
        // cleared below, capture stays live (also the fixpoint's production
        // shape). OPEN_COMMENT_IDS deliberately starts from the clear above,
        // NOT the checkpoint: the snapshot is a post-prologue state, and the
        // prologues (cache builds) re-run in document order, reproducing the
        // carrier's build-phase evolution exactly like list_counters — a
        // pre-prologue seed would double-open every range the resumed
        // section's own build re-opens, stamping paragraphs BEFORE the range
        // start. restore_checkpoint! seeds the loop phase post-prologue.
        // (Contract limit, same as every prologue side effect: a textbox that
        // leaves a NET carrier change from a SKIPPED section's block loop is
        // not reproduced — degenerate unclosed-range-in-textbox shape.)
        #[cfg(test)]
        let resume_from: Option<PageCheckpoint> = self.resume_directive.borrow_mut().take();
        // Statically dead in production, but USED (the re-entry plumbing
        // below compiles unconditionally so byte-identity pins one code path).
        #[cfg(not(test))]
        let resume_from: Option<PageCheckpoint> = None;

        // Snapshot the layout generation. If `abort_layout` bumps it (a new
        // document is being loaded), bail at the next page boundary so this
        // future releases the Viewer borrow before the new `loadDocx`.
        let my_gen = self.layout_gen.get();

        self.pages.borrow_mut().clear();
        self.layout_truncated_at.set(None); // fresh doc/run starts untruncated
        self.para_meta.borrow_mut().clear(); // drop stale selection metadata
        self.table_meta.borrow_mut().clear(); // drop stale table structure
        self.selection.set(None); // drop stale selection on doc switch
        #[cfg(feature = "sml")]
        self.cell_selection.set(None); // drop stale SML cell selection on doc switch
        self.selected_object.borrow_mut().take(); // drop object selection on doc switch

        // `_section_page_idx`: the pushed page's section-local index at push
        // time — the header/footer variant selector (first/even/odd bands can
        // share heights and differ only in content). Underscore-named: read
        // only by the cfg(test) snapshot below.
        let push_page = |mut finished: PageLayout, _section_page_idx: usize, sec_idx: usize| -> Result<u32, JsValue> {
            // Stamp the section this page belongs to (§17.6) so paint + the
            // pageSize API can resolve the page's OWN geometry. Deterministic
            // per the 'sec loop; rides the page-checkpoint clone. Every
            // single-section page stamps 0 → byte-identity preserved.
            finished.section_idx = sec_idx;
            // Restartability self-test ground truth: snapshot the page AS PUSHED,
            // before the on_page sink and before the two post-loop passes
            // (relocation, PAGE/NUMPAGES) mutate it. Relocation-appended blank
            // pages bypass push_page symmetrically and must never be routed
            // through it. OPEN_COMMENT_IDS rides along per page — the
            // geometry-invisible drift channel the dump cannot see.
            // The last-resort allocation failure is a Result, not an
            // abort. Checked BEFORE the cfg(test) PushRecord so a dropped page
            // is dropped everywhere: not stored, not emitted, not recorded (a
            // record for a page that does not exist would corrupt the restartability
            // replay comparison). The truncation is stamped and the next
            // yield-point check ends the run; returning the previous last index
            // keeps the caller's arithmetic harmless for the <1 page it
            // continues before that check fires.
            if self.pages.borrow_mut().try_reserve(1).is_err() {
                let n = self.pages.borrow().len() as u32;
                self.layout_truncated_at.set(Some(n));
                return Ok(n.saturating_sub(1));
            }
            #[cfg(test)]
            self.push_records.borrow_mut().push(PushRecord {
                page: finished.clone(),
                open_comment_ids: OPEN_COMMENT_IDS.with(|c| c.borrow().clone()),
                section_page_idx: _section_page_idx,
            });
            let page_idx = {
                let mut pages = self.pages.borrow_mut();
                pages.push(finished);
                pages.len() as u32 - 1
            };
            (on_page.borrow_mut())(page_idx)?;
            Ok(page_idx)
        };

        let mut global_page_idx: usize = 0;
        let mut list_counters: HashMap<(i64, i64), i64> = HashMap::new();
        // Document-order paragraph id counter + ParaMeta sink,
        // threaded as an EXPLICIT parameter (never a thread-local) through the
        // cache-build walk only. Declared beside `list_counters`, OUTSIDE the
        // 'sec loop, so a full run reseeds both from 0 and the checkpoint resume
        // path (which still rebuilds ALL sections' caches before its skip)
        // produces identical ids. Moved into `self.para_meta` after the loop.
        let mut next_para_id: u32 = 0;
        let mut para_sink: Vec<crate::wml::select::ParaMeta> = Vec::new();
        // Table structure sink, keyed (sec, block),
        // filled beside `para_sink` in the FlatBlock::Table arm and published to
        // `self.table_meta` after the loop (co-located with the para_meta publish).
        let mut table_sink: HashMap<(usize, usize), crate::wml::select::TableMeta> = HashMap::new();

        // Carried across a continuous section break (§17.6.22): the in-progress
        // page and vertical cursor, so the next section continues on the same
        // page. `None` means the next section starts fresh (NextPage).
        let mut carry: Option<(PageLayout, f64, usize)> = None;

        // Demotion ledger. Doc-scoped HashSet keyed by
        // the doc-unique FloatId; a float may demote at most once EVER (the
        // lifetime cap — an over-tall page-relative float re-materializes at the
        // same Y every page and would otherwise demote forever). FloatId embeds
        // section_idx, so a doc-scoped set is collision-free across sections and
        // needs no per-section reset. Written on demote, read by seal_page's
        // demote guard.
        let mut demotion_ledger: HashSet<FloatId> = HashSet::new();

        'sec: for (sec_idx, section) in w.sections.iter().enumerate() {
            let geo = &section.geometry;
            let page_w = geo.width_px;
            let _margin_left = geo.margin_left_px;
            let _margin_right = geo.margin_right_px;
            // For cache building use the narrowest column width so text fits
            // in any column it lands in. Equal-width docs: exact. Unequal: re-packed
            // per-column at placement time (see cols_unequal path below).
            let content_width = geo.columns.iter()
                .map(|c| c.width_px)
                .fold(f64::INFINITY, f64::min)
                .min(geo.content_width_px());
            // Unequal columns need per-column re-wrapping at placement; equal/single
            // columns use the verified pre-packed path unchanged.
            let cols_unequal = geo.columns.len() > 1
                && geo.columns.iter().any(|c| (c.width_px - geo.columns[0].width_px).abs() > 0.5);

            // Layout header/footer content for this section
            let laid_out_hf = layout_section_hdr_ftr(
                &section.hdr_ftr, &self.font_cache, content_width,
                &w.style_ctx, w.default_ppr.as_ref(),
                w.default_tab_stop_px, &w.theme_fonts,
            )?;

            // Build block caches for this section's body content
            let mut block_caches: Vec<BlockCache> = Vec::new();
            let mut flat_blocks = Vec::new();
            flatten_blocks(&section.blocks, &mut flat_blocks);
            for (block_idx, block) in flat_blocks.iter().enumerate() {
                match block {
                    FlatBlock::Para(p) => {
                        let marker = resolve_numbering_marker(
                            p, &w.numbering_levels, &mut list_counters,
                            &w.style_ctx,
                        );
                        let mut cap = crate::wml::select::ParaCapture {
                            next_id: &mut next_para_id,
                            sink: &mut para_sink,
                            location: crate::wml::select::ParaLocation::Body { sec: sec_idx, block: block_idx },
                            nested: false,
                        };
                        let cache = layout_paragraph(
                            &self.font_cache, p, content_width, marker.as_ref(),
                            &w.style_ctx, w.default_ppr.as_ref(),
                            w.default_tab_stop_px, &w.theme_fonts, None, &[], &[],
                            if w.grid_enabled { geo.grid_pitch_px } else { None },
                            false,
                            Some(&mut cap),
                        )?;
                        block_caches.push(BlockCache::Paragraph(cache));
                    }
                    FlatBlock::Table(t) => {
                        let mut cap = crate::wml::select::ParaCapture {
                            next_id: &mut next_para_id,
                            sink: &mut para_sink,
                            location: crate::wml::select::ParaLocation::TableCell { sec: sec_idx, block: block_idx, row: 0, cell: 0 },
                            nested: false,
                        };
                        let cache = layout_table(&self.font_cache, t, content_width, &w.style_ctx, w.default_ppr.as_ref(), w.default_tab_stop_px, &w.theme_fonts, &w.numbering_levels, &mut list_counters, Some(&mut cap))?;
                        // Capture the top-level table's
                        // structure now that its cache is built (read-only).
                        table_sink.insert((sec_idx, block_idx), crate::wml::table::build_table_meta(&cache));
                        block_caches.push(BlockCache::Table(cache));
                    }
                }
            }

            // Phase 2: lay out text box content inside items
            for block in &mut block_caches {
                if let BlockCache::Paragraph(ref mut cache) = block {
                    for line in &mut cache.lines {
                        for item in &mut line.items {
                            if let Some(ref mut tb) = item.text_box {
                                if let Some(raw) = tb.raw.take() {
                                    tb.lines = layout_text_box(
                                        &raw, tb.width_px,
                                        &self.font_cache, &w.style_ctx,
                                        w.default_ppr.as_ref(),
                                        w.default_tab_stop_px, &w.theme_fonts,
                                        &w.numbering_levels,
                                    );
                                }
                            }
                        }
                    }
                }
            }

            // Checkpoint re-entry: pre-checkpoint sections contribute ONLY their
            // prologue side effects (list_counters, cache-phase
            // OPEN_COMMENT_IDS advance deterministically in document order).
            // Their block loops, balancing, carry seam, flush and parity are
            // skipped — no page may be emitted, and `carry` stays None (the
            // checkpoint's current/carried_in embody any carried band). A
            // prologue NEVER runs twice for one section.
            if resume_from.as_ref().is_some_and(|c| sec_idx < c.sec_idx) {
                continue;
            }
            let resuming_here = resume_from.as_ref().is_some_and(|c| sec_idx == c.sec_idx);

            // Seed from a carried continuous break, or start fresh.
            let (mut current, mut y_in_page, mut section_page_idx) = match carry.take() {
                Some((page, y, spi)) => (page, y, spi),
                None => (PageLayout::new(), 0.0_f64, 0usize),
            };
            // Snapshot of the band carried in from a preceding continuous
            // section (the title/intro that lives in `current.elements`, NOT in
            // this section's `block_caches`). Last-page column balancing rebuilds
            // the page from `block_caches` alone; without this snapshot it would
            // discard the carried-in band and float the balanced columns to the
            // page top (dropped-leading-section bug). `carried_in` is consumed
            // only while the carried band is still on the section's first page
            // (`last_page_start_block == 0` and `section_page_idx` unchanged).
            // On re-entry the fresh-empty `current` above would derive None —
            // wrong for a carry-shared page; the checkpoint's conditional
            // snapshot is the ground truth.
            let (carried_in, carried_in_spi): (Option<(Vec<PageElement>, f64)>, usize) =
                if resuming_here {
                    let ck = resume_from.as_ref().unwrap();
                    (ck.carried_in.clone(), ck.carried_in_spi)
                } else if !current.elements.is_empty() {
                    (Some((current.elements.clone(), y_in_page)), section_page_idx)
                } else {
                    (None, section_page_idx)
                };
            // Per-page/column float state — the two-tier `FloatScope` and the
            // `seal_page` verdict that reads it are module-scope (hoisted out of
            // this loop so a module-scope cursor type can name
            // them); their contracts live at the definitions.
            let mut scope = FloatScope {
                column_floats: Vec::new(), page_floats: Vec::new(),
                column_ids: Vec::new(), page_ids: Vec::new(), block_tops: Vec::new(),
            };

            let hf_font_cache = &self.font_cache;
            let finish_page = |current: &mut PageLayout, section_page_idx: &mut usize,
                               global_page_idx: &mut usize, _y_in_page: &mut f64,
                               laid_out_hf: &LaidOutHdrFtr, geo: &PageGeometry|
                               -> Result<(), JsValue> {
                // Stamp header/footer lines onto the page
                let page_num = *global_page_idx + 1;
                if let Some(hdr) = laid_out_hf.header_for_page(*section_page_idx) {
                    let (lines, rows) = position_hdr_ftr_lines(
                        hdr, geo.header_margin_px, geo.width_px, geo.margin_left_px, geo.margin_right_px,
                        page_num, None, hf_font_cache,
                    );
                    current.header_lines = lines;
                    current.header_rows = rows;
                }
                if let Some(ftr) = laid_out_hf.footer_for_page(*section_page_idx) {
                    let footer_y = geo.height_px - geo.footer_margin_px - ftr.total_height;
                    let (lines, rows) = position_hdr_ftr_lines(
                        ftr, footer_y, geo.width_px, geo.margin_left_px, geo.margin_right_px,
                        page_num, None, hf_font_cache,
                    );
                    current.footer_lines = lines;
                    current.footer_rows = rows;
                }
                current.page_number = page_num;
                // Column separator lines (§17.6.4 w:sep).
                if geo.col_sep && geo.columns.len() > 1 {
                    // The band's TOP is `effective_margin_top` — one rule, one
                    // spelling, now that it is a free fn. The
                    // BOTTOM is deliberately NOT `y_start + content_height`:
                    // that is `top + (h - top - bottom)`, and f64 addition is
                    // not associative, so it is not the pinned `h - bottom`.
                    let y_start = effective_margin_top(laid_out_hf, geo, *section_page_idx);
                    let ftr_h = laid_out_hf.footer_for_page(*section_page_idx)
                        .map_or(0.0, |f| f.total_height);
                    let ftr_gap = geo.margin_bottom_px - geo.footer_margin_px;
                    let eff_bottom = if ftr_h > ftr_gap { geo.footer_margin_px + ftr_h } else { geo.margin_bottom_px };
                    let y_end = geo.height_px - eff_bottom;
                    for i in 0..geo.columns.len() - 1 {
                        let col = &geo.columns[i];
                        let next_col = &geo.columns[i + 1];
                        let gutter_mid = col.x_offset_px + col.width_px
                            + (next_col.x_offset_px - col.x_offset_px - col.width_px) / 2.0;
                        current.col_separator_lines.push((gutter_mid, y_start, y_end));
                    }
                }
                Ok(())
            };

            // Owned, not `&str` borrowed from `block_caches` — the
            // PageCheckpoint must hold no borrows across a rollback. Cloned only
            // on assignment; compared via `as_str`.
            let mut prev_para_style: Option<(String, bool)> = None;
            let mut prev_space_after_px: f64 = 0.0;
            // Current column index within the page (0-based). Advances before a new page
            // when the section has multiple columns.
            let mut col_idx: usize = 0;
            // First block index of the current page (used for last-page balancing).
            let mut last_page_start_block: usize = 0;

            // Fast-path gate for floats above their anchor: does ANY paragraph anchor a WRAPPING float
            // that is page/margin-relative-V (cursor-independent Y)? Only then can
            // a float's top sit above text already laid earlier on its page. When
            // false (float-free docs AND the common paragraph/line-relative case),
            // none of the bounded inset-reflow bookkeeping below runs — the block
            // loop stays byte-identical. Computed once, O(n).
            let doc_has_page_margin_wrap_float = block_caches.iter().any(|b| {
                if let BlockCache::Paragraph(c) = b {
                    let pm = |vr: AnchorVRelFrom| matches!(vr, AnchorVRelFrom::Page | AnchorVRelFrom::Margin);
                    c.anchored_images.iter().any(|a| !matches!(a.wrap.mode, WrapMode::None) && pm(a.v_relative_from))
                        || c.anchored_text_boxes.iter().any(|t| !matches!(t.wrap.mode, WrapMode::None) && pm(t.v_relative_from))
                } else { false }
            });

            // Belt-and-braces per-page iteration cap
            // (2·f_P + 1). f_P is over-approximated per SECTION here — cheap and
            // sound (the monotone lattice is the real termination proof; the hard
            // cap only guards against a design bug, aborting to the inert
            // single-pass result). Counts cursor-independent (Page|Margin,
            // non-wrapNone) anchors — the exact fixpoint trigger class.
            let sec_cursor_indep_float_count = block_caches.iter().map(|b| {
                if let BlockCache::Paragraph(c) = b {
                    let pm = |vr: AnchorVRelFrom| matches!(vr, AnchorVRelFrom::Page | AnchorVRelFrom::Margin);
                    c.anchored_images.iter().filter(|a| !matches!(a.wrap.mode, WrapMode::None) && pm(a.v_relative_from)).count()
                        + c.anchored_text_boxes.iter().filter(|t| !matches!(t.wrap.mode, WrapMode::None) && pm(t.v_relative_from)).count()
                } else { 0 }
            }).sum::<usize>();
            let hard_cap: u32 = (2 * sec_cursor_indep_float_count + 1) as u32;

            // Cross-page deferral queue (see the module-scope `DeferredFloat`
            // doc). Declared above the macros below so they can reference it
            // (macro_rules! resolves outer locals at the definition site,
            // which must be textually before the macro).
            let mut pending_floats: Vec<DeferredFloat> = Vec::new();

            // ResumeCursor state (see the module-scope `ResumeAt` doc — the
            // variant contracts and replay recipes live there and are binding).
            let mut resume_at: ResumeAt = ResumeAt::BlockBoundary { next_block: 0 };
            // The block-loop-top hoisted content_height, reified so the
            // checkpoint can capture it VERBATIM. It is deliberately STALE for
            // a block continuing across a mid-block page advance (byte-identity
            // trap — see the PageCheckpoint field doc); the block loop
            // reassigns it at each block's top. Declared before
            // capture_checkpoint! (macro_rules! resolves outer locals at the
            // definition site).
            let mut block_entry_content_height: f64 = effective_content_height(&laid_out_hf, geo, section_page_idx);
            // The checkpoint captured for the page being STARTED, threaded to
            // seal_page at the next emission (the fixpoint's
            // splice point). None while no capture fired (gate off).
            let mut page_ckpt: Option<PageCheckpoint> = None;

            // ── Per-page fixpoint state ───────────────────────────────────────────
            // Declared before seal_or_rollback! (below) so the macro captures
            // them, and before the 'relay loop so a `continue 'relay` never
            // re-runs their declarations. `page_preset`/`resolved_this_page`/
            // `relay_iters`/`preset_injected` are PER-PAGE (reset on ACCEPT);
            // `demoted_floats` is SECTION-scoped (float + anchor always share a
            // section — the `scope` binding itself re-declares per section). The doc-scoped
            // `demotion_ledger` (above the section loop) does NOT reset.
            //   page_preset:        floats pre-registered for THIS page's fixpoint
            //                       (ACCUMULATES across a page's iterations).
            //   resolved_this_page: id → (frozen band, frozen draw), captured at a
            //                       float's FIRST resolution and reused every re-lay.
            //   demoted_floats:     id → frozen carrier, force-registered at the
            //                       anchor when the block loop reaches it.
            let mut page_preset: Vec<FrozenFloat> = Vec::new();
            let mut resolved_this_page: HashMap<FloatId, (ActiveFloat, FrozenDraw)> = HashMap::new();
            let mut demoted_floats: HashMap<FloatId, FrozenFloat> = HashMap::new();
            let mut relay_iters: u32 = 0;
            let mut preset_injected: bool = false;

            // The per-page fixpoint verdict at each of
            // the three page-emission sites. seal_page is a module-scope fn (so
            // it captures nothing), so the site-locals are threaded in at
            // expansion. Defined AFTER `page_ckpt` and the fixpoint state —
            // macro_rules! binds surrounding locals at the DEFINITION scope. On a
            // rollback it restores the page checkpoint and `continue $relay`s,
            // unwinding all nested loops to the block-loop top. The rollback
            // window is PURE (no .await) and does not hold any RefCell across a
            // suspend; the sole layout await (yield_to_browser) stays on the
            // ACCEPT path with its layout_gen gen-check intact.
            macro_rules! seal_or_rollback { ($relay:lifetime) => {{
                let verdict = seal_page(&scope, &block_caches,
                    effective_margin_top(&laid_out_hf, geo, section_page_idx), &demotion_ledger, &page_preset);
                // violations_detected / violation_log are PER-PAGE observability:
                // count only the FIRST seal of a page (relay_iters == 0). A page's
                // re-lays keep structurally detecting the same float (the band
                // still overlaps above-anchor content — the fixpoint terminates
                // via page_preset dedup, not by the overlap vanishing), and a
                // demotion is an internal transition; neither is a NEW violating
                // page. At the first seal demote is always empty (a float can only
                // demote after a prior pre-registration), so pre_register is the
                // detected set — matching the violation-detector battery.
                if relay_iters == 0 && !verdict.pre_register.is_empty() {
                    let mut st = self.layout_stats.get();
                    st.violations_detected += 1; // PER-PAGE
                    self.layout_stats.set(st);
                    #[cfg(test)]
                    self.violation_log.borrow_mut().push((global_page_idx, verdict.pre_register.clone()));
                }
                // Demotion driver: follow the anchor. A pre-registered float
                // whose exclusion pushed its own anchor off THIS page unregisters
                // here and force-registers (via the demoted_floats carrier in the
                // registration loop) on whatever page the anchor lands. Ledgered
                // once ⇒ never demotes again (an over-tall page float would else
                // demote forever). floats_demoted counts these.
                for id in &verdict.demote {
                    demotion_ledger.insert(*id);
                    page_preset.retain(|fr| fr.id != *id);
                    // TOTAL, not a silent-drop risk like the `newly` lookup below:
                    // `verdict.demote` only ever names ids that were in
                    // `page_preset` (seal_page's `None` arm gates on exactly
                    // that), `page_preset` is only ever pushed from a
                    // `resolved_this_page` HIT (the loop below), and the two maps
                    // clear in lockstep on ACCEPT — a rollback clears neither. So
                    // `page_preset ⊆ keys(resolved_this_page)` holds by
                    // construction and this arm cannot miss.
                    if let Some((af, draw)) = resolved_this_page.get(id) {
                        demoted_floats.insert(*id, FrozenFloat { id: *id, af: af.clone(), draw: draw.clone() });
                    }
                    let mut st = self.layout_stats.get();
                    st.floats_demoted += 1;
                    self.layout_stats.set(st);
                }
                // Batched pre-registration: accumulate every NEWLY-violating float
                // into page_preset (deduped against the current preset AND the
                // ledger — a demoted float rides the demoted_floats carrier at its
                // anchor, NOT the page-start injection, so it must not also enter
                // page_preset or it would double-register on re-lay).
                let newly: Vec<FloatId> = verdict.pre_register.iter().copied()
                    .filter(|id| !page_preset.iter().any(|fr| fr.id == *id))
                    .filter(|id| !demotion_ledger.contains(id))
                    .collect();
                // ⚠️ TRIPWIRE — the fixpoint termination argument's silent-failure
                // mode, and the reason for the debug_assert.
                //
                // A miss here does NOT abort the fixpoint: the float stays out of
                // `page_preset`, so the next seal re-nominates it, and the page
                // spins to `hard_cap` re-lays and is then accepted UNCHANGED.
                // Nothing goes red — plausible output, `pages_relaid` merely
                // higher. Verified by mutation: forcing the lookup
                // to miss turns 81 rollbacks into 3-per-page spins and fails 12 of
                // the 13 kill-switch tests, but only because those pin geometry.
                //
                // It is UNREACHABLE today. Every `column_ids`/`page_ids` push is
                // paired with a `resolved_this_page` insert except the
                // `apply_pending_floats` drain, and `seal_page` provably cannot
                // nominate a drained float (its docblock's three legs). Measured:
                // 0 trips across the 768-test suite and the 1,122-document corpus.
                //
                // NOT AN `assert_eq!(misses, 0)` TEST, deliberately: in debug the
                // assert below fires first, so such a test would be unreachable —
                // vacuous by construction. And NOT a `LayoutStats` counter either:
                // `layout_stats()` is `pub(crate)`/test-only with no JS binding,
                // so a release-side counter would be read by nothing. The suite
                // and the corpus probe both build debug, so this assert IS the
                // instrument on every input we have.
                //
                // SCOPE, stated because "guards it" is the wrong word: the
                // shipped wasm is built by `wasm-pack build` (release by
                // default) and `[profile.release]` does NOT set
                // `debug-assertions`, so this is COMPILED OUT of what users
                // run. It is a development tripwire over our own inputs, not a
                // runtime guard. In release the silent-spin failure above is
                // exactly as silent as it was before this landed — what the
                // tripwire buys is that we find out first.
                for id in &newly {
                    let resolved = resolved_this_page.get(id);
                    debug_assert!(resolved.is_some(),
                        "fixpoint: seal_page nominated {id:?} for pre-registration but it has no \
                         resolved_this_page entry — the page will now spin to hard_cap and accept \
                         unchanged. Either a new float registration path skipped the resolution \
                         map, or seal_page can now reach a deferred (drained) float.");
                    if let Some((af, draw)) = resolved {
                        page_preset.push(FrozenFloat { id: *id, af: af.clone(), draw: draw.clone() });
                    }
                }
                // A checkpoint is rollback-eligible only when it is USABLE, i.e.
                // it carries a resume directive. The capture path still
                // assigns `page_ckpt = Some(..)` for an INVALID capture — the
                // size decision lives in `Viewer::build_checkpoint`, which
                // `capture_checkpoint!` now only forwards to (there,
                // a ParaItems remainder above MAX_CHECKPOINT_REMAINDER_ITEMS is
                // stored as `resume: None` rather than cloned), so testing
                // `is_some()` alone would send an unusable checkpoint into
                // `restore_checkpoint!`, whose `.expect("resume directive must
                // be a VALID checkpoint")` then panics and takes the whole
                // layout down. MALFORMED/ADVERSARIAL INPUT: a single paragraph
                // whose live unplaced remainder exceeds 4096 items straddles a
                // page start that also carries a page-relative wrapSquare
                // anchor (§17.3.1.13 / §20.4.2.17), so the page both seals with
                // a float violation and holds an INVALID checkpoint.
                // Falling through to ACCEPT is the behaviour the checkpoint
                // contract already specifies ("a fixpoint aborts", see the
                // `resume` field doc): the page is kept exactly as laid, which
                // is the pre-fixpoint rendering, and the loop terminates
                // because ACCEPT never `continue $relay`s. It cannot corrupt a
                // valid document: a checkpoint is only ever INVALID for a
                // remainder over the size cap, so every document whose page-start
                // straddlers stay under it keeps the full fixpoint.
                // ⚠️ WHERE THE ROLLBACK PATH IS ACTUALLY GUARDED.
                //
                // Before dissolving these macros into `ControlFlow` methods, know
                // which tests can SEE a rollback that stops happening. Measured by
                // forcing this branch to ACCEPT unconditionally (a kill-switch on
                // the condition below) and running the suite:
                //
                //   BLIND — all 33 `restart_*` restartability tests pass.
                //   BLIND — `violation_detector_precision_recall` passes.
                //   SIGHTED — exactly 13 tests fail, and they are the net.
                //   RE-MEASURED 2026-07-28 (earlier counts in this comment
                //   went stale as tests were added, which is why the
                //   re-measure instruction below exists):
                //     fullwidth_float_above_anchor_wraps_below_fixpoint
                //     multicol_page_float_excludes_other_column
                //     multicol_page_float_cascades_to_demotion
                //     overtall_page_float_demotes
                //     page_float_excludes_text_above_anchor
                //     page_relative_inset_wraps
                //     two_page_floats_same_page
                //     rollback_reenters_through_paraitems_checkpoint
                //     rollback_reenters_through_rowsplit_checkpoint
                //     rollback_reenters_through_paratop_checkpoint
                //     rollback_reenters_through_rowwhole_checkpoint
                //     rollback_reenters_through_pagebreakbefore_checkpoint
                //     rollback_census_covers_every_resume_variant
                //
                // The checkpoint/replay batteries are NOT the safety net for this path,
                // contrary to what their names suggest — they exercise REPLAY
                // (resume-from-checkpoint), not the DECISION to roll back. Run
                // the kill-switch measurement after EVERY refactor here and update
                // this list: a step that SHRINKS it removed guard, not macro.
                // Do not trust the count without re-running it — it has been
                // wrong twice.
                let can_rollback = page_ckpt.as_ref().is_some_and(|c| c.resume.is_some());
                // The relay_iters/preset_injected resets below are read by the NEXT
                // page's seal/injection, but are dead on the section's FINAL
                // accepted page (section ends, locals re-declared) — allow it, like
                // the resume_at invariant stores.
                #[allow(unused_assignments)]
                if (newly.is_empty() && verdict.demote.is_empty())
                    || !can_rollback || relay_iters >= hard_cap
                {
                    // ACCEPT: this page's fixpoint is stable (or ineligible/abort-
                    // to-inert). Reset the per-page state before the finish_page →
                    // push_page → yield sequence (which stays byte-for-byte).
                    page_preset.clear();
                    resolved_this_page.clear();
                    relay_iters = 0;
                    preset_injected = false;
                } else {
                    // ROLL BACK: re-lay this page with the accumulated union pre-
                    // registered (and any demoted float force-registered at its
                    // anchor). restore_checkpoint! resets scope + indices + block
                    // cursor to the page start; preset_injected=false so the loop-
                    // top drain re-injects; continue unwinds to the 'relay top.
                    relay_iters += 1;
                    let mut st = self.layout_stats.get();
                    st.pages_relaid += 1;
                    self.layout_stats.set(st);
                    // Rollback instrument (native-only, env-gated): which resume
                    // shape each rollback actually re-enters through. The
                    // refactor's riskiest call sites are the mid-block variants
                    // (ParaItems/RowSplit) — this log is how coverage of them
                    // is PROVEN rather than assumed.
                    #[cfg(not(target_arch = "wasm32"))]
                    if std::env::var_os("PARASMA_STATS_CENSUS").is_some() {
                        eprintln!("STATSROLLBACK test={} resume={}",
                            std::thread::current().name().unwrap_or("?"),
                            page_ckpt.as_ref().unwrap().resume.as_ref().unwrap().at.census_label());
                    }
                    #[cfg(test)]
                    self.rollback_log.borrow_mut().push(
                        page_ckpt.as_ref().unwrap().resume.as_ref().unwrap().at.census_label());
                    restore_checkpoint!(page_ckpt.as_ref().unwrap());
                    preset_injected = false;
                    continue $relay;
                }
            }};}

            // ── capture_checkpoint! — a FORWARDING SHIM. The
            //    body lives in `Viewer::build_checkpoint`; what stays here is
            //    the capture GATE and the 15-field input list, spelled ONCE
            //    rather than at each of the three call sites.
            //    (restartable page-start capture) ──
            // One canonical capture for the three page-start shapes:
            //   (a) overflow_col!'s page arm — AFTER the resets +
            //       last_page_start_block, BEFORE apply_pending_floats (the
            //       drain must replay on re-entry);
            //   (b) the page_break_before inline advance (no drain — variant
            //       contract);
            //   (c) section entry before the block loop (carry-seeded page
            //       starts, including document page 1).
            // NON-sites, by design: the section flush pushes a page but starts
            // none; parity/relocation blank pages bypass finish_page; the
            // balancing pass re-lays an un-pushed page; column advances are
            // sub-page. Capture sits after the gen-check inside overflow_col!,
            // so an aborted layout never captures. No awaits, all borrows
            // synchronous. `$rem`: the live ParaItems remainder, passed
            // by ident and evaluated ONLY inside the gated branch — lazy by
            // construction, never cloned on column advances or ungated docs.
            //
            // The BODY is `Viewer::build_checkpoint`. What is left here
            // is the capture GATE — which must stay outside the function so a
            // gated-off capture leaves `page_ckpt` untouched — and the
            // `CheckpointInputs` field list, spelled once here rather than
            // three times at the call sites.
            macro_rules! capture_checkpoint {
                () => { capture_checkpoint!(@impl None) };
                (remainder = $rem:ident) => { capture_checkpoint!(@impl Some(&$rem)) };
                (@impl $rem:expr) => {{
                    if doc_has_page_margin_wrap_float || self.capture_forced() {
                        page_ckpt = Some(self.build_checkpoint(CheckpointInputs {
                            scope: &scope, resume_at: &resume_at, current: &current,
                            prev_para_style: &prev_para_style,
                            pending_floats: &pending_floats, carried_in: &carried_in,
                            sec_idx, global_page_idx, section_page_idx, y_in_page,
                            col_idx, last_page_start_block, prev_space_after_px,
                            carried_in_spi, block_entry_content_height,
                        }, $rem));
                    }
                }};
            }

            // Advance col_idx, or start a new page when all columns are full.
            // Must be a macro (not a closure) because it uses .await.
            // `$resume` states where the new column/page's content resumes;
            // recorded before `seal_page`/`finish_page` so the checkpoint can
            // capture it into the PageCheckpoint at page start.
            macro_rules! overflow_col {
                ($relay:lifetime, $block_idx:expr, $resume:expr) => {
                    overflow_col!(@impl $relay, $block_idx, $resume,)
                };
                // `remainder = <ident>`: the live ParaItems remainder at the
                // overflow site (a macro-local), threaded through to the page
                // arm's capture so the remainder clone happens lazily — only on a page
                // advance, only when capture is gated on.
                ($relay:lifetime, $block_idx:expr, $resume:expr, remainder = $rem:ident) => {
                    overflow_col!(@impl $relay, $block_idx, $resume, remainder = $rem)
                };
                (@impl $relay:lifetime, $block_idx:expr, $resume:expr, $(remainder = $rem:ident)?) => {{
                    resume_at = $resume;
                    if col_idx + 1 < geo.columns.len() {
                        col_idx += 1;
                        y_in_page = 0.0;
                        scope.clear_column();
                    } else {
                        seal_or_rollback!($relay);
                        finish_page(&mut current, &mut section_page_idx, &mut global_page_idx, &mut y_in_page, &laid_out_hf, geo)?;
                        let finished = std::mem::replace(&mut current, PageLayout::new());
                        push_page(finished, section_page_idx, sec_idx)?;
                        yield_to_browser().await;
                        if self.layout_gen.get() != my_gen { return Ok(self.pages.borrow().len() as u32); }
                        // Stop BEFORE the memory wall, at the same page
                        // boundary the doc-switch abort uses. Pages so far stay
                        // valid and visible; the post-loop passes (relocation,
                        // PAGE/NUMPAGES) are skipped exactly as on abort, so a
                        // NUMPAGES field in a truncated view shows its cached
                        // value — declared, not silent.
                        if self.should_truncate_layout() {
                            let n = self.pages.borrow().len() as u32;
                            self.layout_truncated_at.set(Some(n));
                            return Ok(n);
                        }
                        section_page_idx += 1;
                        global_page_idx += 1;
                        y_in_page = 0.0;
                        col_idx = 0;
                        scope.clear_page();
                        last_page_start_block = $block_idx + 1;
                        // Site (a): capture the new page's checkpoint AFTER the
                        // resets + last_page_start_block, BEFORE the drain
                        // below (the drain must replay on re-entry).
                        capture_checkpoint!($(remainder = $rem)?);
                        // A float deferred to this page must be registered now,
                        // before the current paragraph places its continuation.
                        apply_pending_floats(&mut pending_floats, &mut scope, &mut current,
                                             &laid_out_hf, geo, section_page_idx, col_idx);
                    }
                }};
            }

            // Float-aware paragraph placement (unified "B" model): pack the given
            // items against the CURRENT page's floats, place lines until the page
            // fills, then re-pack the REMAINDER against the next page's floats.
            // This is what makes wrapSquare correct across a page break — a
            // paragraph that crosses onto a page carrying a float (e.g. a deferred
            // full-width image) re-flows below/around it instead of overlapping
            // (§20.4.2.17). Used for paragraphs floated from the start AND for the
            // remainder of a paragraph that breaks into a float (see normal path).
            // `cache` (the current Paragraph block) is resolved at each call site.
            macro_rules! place_float_aware {
                // `$logical` — does `$items` arrive in LOGICAL order? True for the
                // whole-paragraph routes, which read `ParagraphCache::logical_items`.
                // FALSE for the mid-paragraph relay below, which can only flatten a
                // SUFFIX of `cache.lines` and those are already UAX#9 VISUAL: a
                // logical suffix cannot be recovered from them, because `pack_lines`
                // drops wrap-edge whitespace items and the index correspondence is
                // therefore not exact (the same limit the ResumeAt::ParaItems
                // note records). On that route the L2 reorder is skipped rather than
                // applied to an already-reordered sequence — today's behavior,
                // unchanged and now stated.
                ($relay:lifetime, $cache:ident, $block_idx:expr, $items:expr, $first_batch:expr, $logical:expr) => {{
                    let mut remaining: Vec<Item> = $items;
                    // ONE spelling of the reorder gate. `$logical` is true exactly
                    // when `ParagraphCache::logical_items` was populated, which is
                    // gated on `bidi::may_reorder` (base_rtl OR any complex-script
                    // run). Re-gating here on `base_rtl` ALONE — as this line did —
                    // meant an LTR-base paragraph containing Arabic or Hebrew kept
                    // its items in LOGICAL order and never had L2 re-applied: RTL
                    // text rendered in the wrong order on every float path. Two
                    // spellings of one predicate, three lines apart in effect.
                    // `bidi_reorder_line` self-guards with the same `may_reorder`
                    // (bidi.rs:72), so this needs no second condition at all.
                    let reorder_on_place: bool = $logical;
                    let mut is_first_batch: bool = $first_batch;
                    while !remaining.is_empty() {
                        let content_height = effective_content_height(&laid_out_hf, geo, section_page_idx);
                        let margin_top = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                        let cur_col = &geo.columns[col_idx];
                        let col_ml = cur_col.x_offset_px;
                        let col_mr = page_w - cur_col.x_offset_px - cur_col.width_px;
                        let dleft = col_ml + $cache.layout.indent_left_px;
                        let dright = page_w - col_mr - $cache.layout.indent_right_px;
                        let fli = if is_first_batch { $cache.layout.indent_first_line_px } else { 0.0 };
                        // nominal_lh drives only the float-band geometry; each placed
                        // line uses its own height (step 1).
                        let nominal_lh = $cache.lines.first()
                            .map(|l| $cache.layout.line_height(l.max_font_pt, l.max_ascent + l.max_descent, l.max_image_height))
                            .unwrap_or_else(|| $cache.layout.line_height(DEFAULT_FONT_PT, 0.0, 0.0));
                        // Clear past any float that leaves no usable room on this
                        // column line (e.g. a full-width image — whether it fills the
                        // column exactly, or sits offset leaving only a sub-pixel
                        // sliver under wrapText="largest"): drop the cursor straight to
                        // the float's bottom so text resumes BELOW it. "No usable room"
                        // = the widest free interval can't fit even the first word, so
                        // we don't force-pack one word per line into a 0.4px gap beside
                        // the image (the §20.4.2.17 violation seen with an offset
                        // image on calibration doc C5). Partial floats that leave room for a word are
                        // left to the packer to wrap around.
                        // A wordless line (whitespace-only run) still needs REAL room
                        // beside a float: default to a tiny positive so the clear-below
                        // loop drops it past a full-width float (zero free interval)
                        // instead of stranding it in the float band. Partial
                        // floats (widest > WRAP_EPS) still break out and wrap beside.
                        let first_word_w = remaining.iter()
                            .find(|it| !it.is_only_whitespace())
                            .map(|it| it.content_width)
                            .unwrap_or(WRAP_EPS * 2.0);
                        clear_below_floats(
                            scope.floats().as_ref(), &mut y_in_page, margin_top, nominal_lh,
                            dleft, dright, first_word_w, content_height, !current.is_empty());
                        let y_first_top = margin_top + y_in_page;
                        let batch = std::mem::take(&mut remaining);
                        let wrapped = pack_lines_around_floats(
                            batch, scope.floats().as_ref(), dleft, dright,
                            fli, nominal_lh, y_first_top, $cache.layout.alignment, &self.font_cache,
                            &$cache.tab_stops,
                        );
                        let n = wrapped.len();
                        let mut overflowed = false;
                        let mut trailing_pb = false;
                        for (i, wl) in wrapped.into_iter().enumerate() {
                            // A band-walk line states its own advance: it stands for a
                            // RUN of fully-blocked bands collapsed into one line, so its
                            // height is that run's total. Geometry is unchanged versus
                            // the N empty lines it replaces (N·pitch == walk_height, and
                            // an empty line draws nothing) — see WrappedLine::walk_height.
                            let lh = wl.walk_height.unwrap_or_else(|| $cache.layout.line_height(
                                wl.max_font_pt, wl.max_ascent + wl.max_descent, wl.max_image_height));
                            if !overflowed && y_in_page + lh > content_height && !current.is_empty() {
                                overflowed = true;
                            }
                            if overflowed {
                                // Carry every unplaced line's items into the remainder
                                // to be re-packed against the next page's floats.
                                for seg in wl.segments { remaining.extend(seg.items); }
                                continue;
                            }
                            let baseline_y = margin_top + y_in_page + wl.max_ascent;
                            let pb = wl.ends_with_page_break;
                            // UAX#9 L2 — the last moment before paint, and the first
                            // at which this line is final. DEVIATION, declared: the
                            // algorithm reorders a LINE, and a float splits one
                            // logical line into several visual runs, so reordering
                            // each segment independently is an approximation — an
                            // RTL phrase spanning both sides of a float does not read
                            // across the gap. Strictly better than this path's
                            // previous state, which applied no reorder at all.
                            let mut segments = wl.segments;
                            if reorder_on_place {
                                for seg in &mut segments {
                                    crate::wml::bidi::bidi_reorder_line(
                                        &mut seg.items, $cache.layout.base_rtl);
                                }
                            }
                            current.push(PageElement::Line(PagedLine {
                                segments,
                                para: $cache.para_id.unwrap_or(PARA_SENTINEL),
                                baseline_y,
                                para_shading: $cache.layout.shading_color.clone(),
                                line_height: lh,
                                content_left: dleft,
                                content_right: dright,
                                border_top: $cache.layout.border_top.clone(),
                                border_bottom: $cache.layout.border_bottom.clone(),
                                border_left: $cache.layout.border_left.clone(),
                                border_right: $cache.layout.border_right.clone(),
                                bar_tabs: $cache.tab_stops.bar_xs(dleft, dright, $cache.layout.base_rtl),
                                is_first_line: is_first_batch && i == 0,
                                is_last_line: i + 1 == n,
                            }));
                            y_in_page += lh;
                            if pb {
                                if i + 1 < n { overflowed = true; } else { trailing_pb = true; }
                            }
                        }
                        if remaining.is_empty() {
                            // Trailing page break on the block's last line: content
                            // is fully placed, but the arm's epilogue (spacing/style
                            // carry, footnote ids, wrapNone draws) runs AFTER this
                            // advance, on the NEW page — so record an empty remainder
                            // that re-enters the arm, not BlockBoundary{+1} (which
                            // would skip the epilogue on replay).
                            if trailing_pb {
                                // `remaining` is empty here — passed anyway so
                                // the capture's remainder/cursor totality
                                // debug_assert stays total.
                                overflow_col!($relay, $block_idx, ResumeAt::ParaItems {
                                    block: $block_idx,
                                    items_remaining: 0,
                                    is_first_batch: false,
                                    src: ParaItemsSrc::FloatAware,
                                }, remainder = remaining);
                            }
                        } else {
                            overflow_col!($relay, $block_idx, ResumeAt::ParaItems {
                                block: $block_idx,
                                items_remaining: remaining.len(),
                                is_first_batch: false,
                                src: ParaItemsSrc::FloatAware,
                            }, remainder = remaining);
                            is_first_batch = false;
                        }
                    }
                }};
            }

            // Empty / whitespace-only paragraph (paragraph-mark line(s) only,
            // §17.3.1.29) that overlaps a float band. place_float_aware! discards it
            // — its flattened items are empty so `while !remaining.is_empty()` is
            // false and the whole macro is skipped — dropping the mark line's height
            // AND its clearance, so the NEXT paragraph resumes INSIDE the float band
            // rather than below it. Place each
            // mark line: drop below any float that leaves no usable room on this line
            // (a full-width image — distB=0 is only a MINIMUM per §20.4.2.17, so
            // resuming below is the [WORD] line-grid behaviour), then advance and emit
            // an empty PagedLine so spacing/borders stay coherent.
            macro_rules! place_empty_para_around_floats {
                ($relay:lifetime, $cache:ident, $block_idx:expr) => {
                    place_empty_para_around_floats!($relay, $cache, $block_idx, 0, false,
                        effective_content_height(&laid_out_hf, geo, section_page_idx))
                };
                // Checkpoint re-entry form: start at mark line `$start`,
                // skipping its clearance + fit check once (`$skip_first_check`
                // — the original run already ran both and fired the advance);
                // `$ch` is the MACRO-ENTRY content_height — the captured stale
                // value on re-entry, freshly evaluated on the pristine path.
                ($relay:lifetime, $cache:ident, $block_idx:expr, $start:expr, $skip_first_check:expr, $ch:expr) => {{
                    let content_height = $ch;
                    let mut skip_check: bool = $skip_first_check;
                    let n = $cache.lines.len();
                    for (i, line) in $cache.lines.iter().enumerate().skip($start) {
                        let lh = $cache.layout.line_height(
                            line.max_font_pt, line.max_ascent + line.max_descent, line.max_image_height);
                        if !std::mem::replace(&mut skip_check, false) {
                        {
                            // margin_top / dleft / dright are loop-invariant within a
                            // page (effective_margin_top reads only section_page_idx;
                            // bounds derive from geo.columns[col_idx]) — hoisted out of
                            // the former clear loop.
                            let margin_top = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                            let cur_col = &geo.columns[col_idx];
                            let dleft = cur_col.x_offset_px + $cache.layout.indent_left_px;
                            let dright = page_w - (page_w - cur_col.x_offset_px - cur_col.width_px)
                                - $cache.layout.indent_right_px;
                            clear_below_floats(
                                scope.floats().as_ref(), &mut y_in_page, margin_top, lh,
                                dleft, dright, 2.0 * WRAP_EPS, content_height, !current.is_empty());
                        }
                        if y_in_page + lh > content_height && !current.is_empty() {
                            // Mark line `i` did not fit; it opens the new page.
                            // EmptyPara carries the MACRO-ENTRY content_height
                            // (this macro's own hoist) — a second stale value,
                            // distinct from the block-loop hoist.
                            overflow_col!($relay, $block_idx,
                                ResumeAt::ParaLines { block: $block_idx, next_line: i,
                                    src: ParaLineSrc::EmptyPara { macro_content_height: content_height } });
                        }
                        }
                        let margin_top = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                        let cur_col = &geo.columns[col_idx];
                        let col_ml = cur_col.x_offset_px;
                        let col_mr = page_w - cur_col.x_offset_px - cur_col.width_px;
                        let fli = if i == 0 { $cache.layout.indent_first_line_px } else { 0.0 };
                        let x_start = $cache.layout.x_start(page_w, col_ml, col_mr, line.width(), fli);
                        let baseline_y = margin_top + y_in_page + line.max_ascent;
                        current.push(PageElement::Line(PagedLine {
                            segments: PagedLine::single(line.items.clone(), x_start),
                            para: $cache.para_id.unwrap_or(PARA_SENTINEL),
                            baseline_y,
                            para_shading: $cache.layout.shading_color.clone(),
                            line_height: lh,
                            content_left: col_ml + $cache.layout.indent_left_px,
                            content_right: page_w - col_mr - $cache.layout.indent_right_px,
                            border_top: $cache.layout.border_top.clone(),
                            border_bottom: $cache.layout.border_bottom.clone(),
                            border_left: $cache.layout.border_left.clone(),
                            border_right: $cache.layout.border_right.clone(),
                            bar_tabs: $cache.tab_stops.bar_xs(
                                col_ml + $cache.layout.indent_left_px,
                                page_w - col_mr - $cache.layout.indent_right_px,
                                $cache.layout.base_rtl),
                            is_first_line: i == 0,
                            is_last_line: i + 1 == n,
                        }));
                        y_in_page += lh;
                    }
                }};
            }

            // Split a table row that is taller than the page remainder across the
            // page break (§17.4.6 — splitting at line granularity; §17.4.7
            // cantSplit rows never reach here, they take the whole-row path).
            // Each fragment is a `PagedTableRow` carrying the
            // subset of cell lines that fit on the current page: top/side borders
            // repeat on every fragment, the bottom border is DEFERRED (None) until
            // the final fragment, and the cell fill repeats so the column rules and
            // shading stay continuous through the break. Split cells are treated
            // top-aligned (a vertically-centred cell cannot be meaningfully centred
            // across a page boundary — Word tops them too). vMerge-away cells emit
            // as empty same-height border/fill cells per fragment so the column
            // rules continue. Page advance reuses `overflow_col!` (whence the
            // `.await`-bearing macro form). The row is exhausted when every cell
            // cursor reaches the end.
            macro_rules! place_split_row {
                ($relay:lifetime, $table:ident, $row:ident, $row_idx:expr, $block_idx:expr) => {
                    place_split_row!($relay, $table, $row, $row_idx, $block_idx,
                        col_idx, vec![0usize; $row.cells.len()], 0.0)
                };
                // Checkpoint re-entry form: `$entry_col`/`$cursors`/`$consumed`
                // come from a RowSplit checkpoint; the pristine forward above
                // passes today's entry values. is_first_fragment (borders/
                // margin insets) re-derives from `consumed` inside the loop.
                ($relay:lifetime, $table:ident, $row:ident, $row_idx:expr, $block_idx:expr,
                 $entry_col:expr, $cursors:expr, $consumed:expr) => {{
                    let table = $table;
                    let row = $row;
                    // Cell x geometry is computed once from the ENTRY column and
                    // reused verbatim by continuation fragments across column/page
                    // advances — RowSplit records it (see the variant doc): a
                    // resumed fragment keeps the ORIGINAL entry column's x, not
                    // the checkpoint's col_idx (today's multi-column quirk).
                    let entry_col = $entry_col;
                    let col = &geo.columns[entry_col];
                    let col_ml = col.x_offset_px;
                    let col_cw = col.width_px;
                    let table_width: f64 = table.col_widths_px.iter().sum();
                    let table_offset = match table.alignment {
                        STJcTable::Center => (col_cw - table_width) / 2.0,
                        STJcTable::End | STJcTable::Right => col_cw - table_width,
                        _ => table.indent_px,
                    };
                    let cell_x_start = col_ml + table_offset;
                    let tbl_cm_h = table.cell_margin_h;
                    let tbl_cm_v = table.cell_margin_v;

                    // Flatten every cell once (content-relative line protos +
                    // structural metadata). `cell_protos[i]` is the flat line list
                    // for `row.cells[i]`; vMerge-away cells have an empty list.
                    let mut cell_x = cell_x_start;
                    let mut cell_protos: Vec<Vec<CellLineProto>> = Vec::with_capacity(row.cells.len());
                    let mut cell_xs: Vec<f64> = Vec::with_capacity(row.cells.len());
                    for cell in &row.cells {
                        cell_xs.push(cell_x);
                        cell_protos.push(flatten_cell_lines(cell, cell_x));
                        cell_x += cell.width_px;
                    }

                    // Total row content extent (max cell content bottom) and how
                    // much we have emitted so far (in row-content coordinates).
                    let row_content_bottom: f64 = cell_protos.iter()
                        .flat_map(|ps| ps.last())
                        .map(|p| p.rel_top + p.height)
                        .fold(0.0_f64, f64::max);
                    let mut consumed: f64 = $consumed;
                    // Per-cell cursor into its proto list.
                    let mut cursors: Vec<usize> = $cursors;

                    while consumed + 0.01 < row_content_bottom {
                        let margin_top = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                        let content_height_now = effective_content_height(&laid_out_hf, geo, section_page_idx);
                        let avail = content_height_now - y_in_page;
                        // First fragment carries the top margin; the lines have
                        // `rel_top` from the cell content top, so the usable line
                        // window is `avail - mar_top`. Use the max cell mar_top as a
                        // conservative shared top inset on the first fragment only.
                        let is_first_fragment = consumed <= 0.0;
                        let mar_top_inset = if is_first_fragment {
                            row.cells.iter().filter(|c| !c.v_merged_away)
                                .map(|c| c.mar_top).fold(0.0_f64, f64::max)
                        } else { 0.0 };
                        let window_bottom = consumed + (avail - mar_top_inset).max(0.0);

                        // Largest line-bottom that still fits this fragment.
                        let mut frag_bottom = consumed;
                        for (ci, ps) in cell_protos.iter().enumerate() {
                            let mut k = cursors[ci];
                            while k < ps.len() {
                                let p = &ps[k];
                                let lb = p.rel_top + p.height;
                                if lb <= window_bottom + 0.01 { frag_bottom = frag_bottom.max(lb); k += 1; }
                                else { break; }
                            }
                        }
                        // Progress guarantee: if no line fit (e.g. a single huge
                        // line that exceeds even a fresh page), force the next
                        // unplaced line in onto this fragment so the loop advances.
                        if frag_bottom <= consumed + 0.01 {
                            for (ci, ps) in cell_protos.iter().enumerate() {
                                if let Some(p) = ps.get(cursors[ci]) {
                                    frag_bottom = frag_bottom.max(p.rel_top + p.height);
                                }
                            }
                            if frag_bottom <= consumed + 0.01 {
                                // Nothing left anywhere — done.
                                break;
                            }
                        }

                        // Does the FINAL fragment land here (no more lines beyond
                        // frag_bottom in any cell)?
                        let is_last_fragment = frag_bottom + 0.01 >= row_content_bottom;
                        let frag_top_page = margin_top + y_in_page;
                        // Fragment content top in page space (after top margin on
                        // the first fragment).
                        let content_top_page = frag_top_page + mar_top_inset;
                        let mar_bottom_inset = if is_last_fragment {
                            row.cells.iter().filter(|c| !c.v_merged_away)
                                .map(|c| c.mar_bottom).fold(0.0_f64, f64::max)
                        } else { 0.0 };
                        let frag_height = mar_top_inset + (frag_bottom - consumed) + mar_bottom_inset;

                        let mut paged_cells = Vec::with_capacity(row.cells.len());
                        for (ci, cell) in row.cells.iter().enumerate() {
                            let mut cell_lines = Vec::new();
                            let ps = &cell_protos[ci];
                            let mut k = cursors[ci];
                            while k < ps.len() {
                                let p = &ps[k];
                                if p.rel_top + p.height <= frag_bottom + 0.01 {
                                    let mut pl = p.line.clone();
                                    // Re-anchor: relative baseline → page space,
                                    // shifting by the slice's consumed origin.
                                    pl.baseline_y = content_top_page + (p.line.baseline_y - consumed);
                                    cell_lines.push(pl);
                                    k += 1;
                                } else { break; }
                            }
                            cursors[ci] = k;
                            // Continuation top border: a mid-row fragment has no
                            // real top edge; repeat the cell's top border only on
                            // the FIRST fragment. The bottom border is deferred to
                            // the LAST fragment (None elsewhere) so it never draws
                            // a spurious rule mid-row. Left/right repeat always.
                            let border_top = if is_first_fragment { cell.border_top.clone() } else { None };
                            let border_bottom = if is_last_fragment { cell.border_bottom.clone() } else { None };
                            paged_cells.push(PagedTableCell {
                                lines: cell_lines,
                                x: cell_xs[ci],
                                width_px: cell.width_px,
                                fill_color: cell.fill_color.clone(),
                                v_merged_away: cell.v_merged_away,
                                border_top,
                                border_bottom,
                                border_left: cell.border_left.clone(),
                                border_right: cell.border_right.clone(),
                            });
                        }

                        current.push(PageElement::TableRow(PagedTableRow {
                            cells: paged_cells,
                            y_top: frag_top_page,
                            height_px: frag_height,
                            border_color: table.border_color.clone(),
                            has_borders: table.has_borders,
                            cell_margin_h: tbl_cm_h,
                            cell_margin_v: tbl_cm_v,
                        }));

                        consumed = frag_bottom;
                        if is_last_fragment {
                            y_in_page += frag_height;
                        } else {
                            // The fragment consumed the rest of the page; advance.
                            y_in_page = content_height_now;
                            overflow_col!($relay, $block_idx, ResumeAt::RowSplit {
                                block: $block_idx,
                                row: $row_idx,
                                entry_col,
                                cursors: cursors.clone(),
                                consumed,
                            });
                        }
                    }
                    prev_para_style = None;
                    prev_space_after_px = 0.0;
                }};
            }

            // Index-driven block-loop cursor + one-shot continuation for
            // checkpoint re-entry. Declared before restore_checkpoint! (macro_rules!
            // resolves outer locals at the definition site). Pristine runs
            // enter at 0 with no continuation — iteration order is
            // byte-identical to the former `for block_idx in 0..len`.
            let mut block_idx: usize = 0;
            let mut entry: Option<EntryPlan> = None;

            // ── restore_checkpoint! ── re-enter the
            // resumed section's block loop from the directive's checkpoint.
            // Clones from &PageCheckpoint (a future fixpoint rolls back
            // repeatedly from one checkpoint). scope is cleared, never
            // restored (empty at capture by construction); pending_floats is
            // the pre-drain snapshot, so the loop-top drain below replays the
            // original page-arm drain.
            // ⚠️ THIS ONE STAYS A MACRO, DELIBERATELY. Recorded here so
            // the next reader does not re-litigate it — the reason is not
            // visible from the code, and without this note it has to be
            // re-derived from scratch.
            //
            // It is escape-free, so `ControlFlow` is not the obstacle. The
            // obstacle is that it WRITES TWELVE enclosing locals, two of which
            // are `block_idx` and `entry` — the block loop's own induction
            // variable and its entry plan. A function cannot assign a caller's
            // locals, so the function form must return all twelve and every
            // call site must destructure them. That call site reads worse than
            // the macro, and worse in the specific way that matters here: a
            // returned tuple that is bound and silently dropped would mean the
            // block loop does not rewind, which no pin and no counter can see.
            // The macro's implicit assignment is the safer spelling.
            //
            // Revisit only as part of the `PageCursor` migration, where those
            // twelve become fields of one owned struct and the assignment
            // becomes `*cursor = restored` — one value, not twelve.
            macro_rules! restore_checkpoint {
                ($ck:expr) => {{
                    let ck: &PageCheckpoint = $ck;
                    current = ck.current.clone();
                    y_in_page = ck.y_in_page;
                    section_page_idx = ck.section_page_idx;
                    global_page_idx = ck.global_page_idx;
                    col_idx = ck.col_idx;
                    last_page_start_block = ck.last_page_start_block;
                    prev_para_style = ck.prev_para_style.clone();
                    prev_space_after_px = ck.prev_space_after_px;
                    pending_floats = ck.pending_floats.clone();
                    scope.clear_page();
                    // Loop-phase carrier seed: the prologues re-ran from empty
                    // and reproduced the build-phase evolution; the snapshot
                    // now overwrites with the PAGE-START state (build state +
                    // net in-loop textbox mutations of this section's earlier
                    // pages) so in-loop layout_text_box stamps replay exactly.
                    OPEN_COMMENT_IDS.with(|c| *c.borrow_mut() = ck.open_comment_ids.clone());
                    let resume = ck.resume.as_ref()
                        .expect("resume directive must be a VALID checkpoint");
                    block_idx = resume.at.entry_block();
                    entry = match &resume.at {
                        ResumeAt::BlockBoundary { .. } => None,
                        other => Some(EntryPlan {
                            resume: other.clone(),
                            para_remainder: resume.para_remainder.clone(),
                            stale_content_height: ck.block_entry_content_height,
                        }),
                    };
                }};
            }

            if resuming_here {
                restore_checkpoint!(resume_from.as_ref().unwrap());
                // The resumed page is itself fixpoint-eligible —
                // seed page_ckpt with its own resume checkpoint so a violation on
                // this first re-entered page can roll back to it (the canonical
                // path sets page_ckpt via capture_checkpoint! in the else arm;
                // a resume skips that self-capture, so seed it here). Replay-only
                // (production resume_from is always None).
                page_ckpt = resume_from.clone();
            } else {
                // Canonical section-entry capture (site c). The
                // carry-seeded first page of every section, including document
                // page 1, has no page-advance site of its own — captured here,
                // after every loop-carried declaration. Precedes the loop-top
                // apply_pending_floats so the drain replays deterministically.
                // (`resume_at` is still BlockBoundary{0}; the carried_in
                // predicate is true here and only here within the section.)
                // A replay-target section must NOT self-capture at its entry —
                // capture during the rest of the replay stays live.
                capture_checkpoint!();
            }
            // The increment is at the loop BOTTOM: a bare `continue` targeting
            // this loop never advances block_idx and hangs layout — use a
            // labeled inner loop or restructure instead.
            //
            // The per-page fixpoint's unwind channel. The block
            // loop + last-page balancing + section flush are wrapped in
            // `'relay: loop`; a rollback verdict inside `seal_or_rollback!`
            // (which expands DEEP inside the placement macros' nested loops)
            // restores the page checkpoint and `continue 'relay`s — unwinding all
            // nested loops back here in one statement. A pristine (no-rollback)
            // run falls through to `break 'relay` after the flush, so this loop
            // executes its body exactly once — byte-identical to the former flat
            // structure. `block_idx`/`entry` are declared ABOVE this loop so a
            // rollback's restore_checkpoint! (which sets them) survives the
            // `continue 'relay`.
            'relay: loop {
            while block_idx < block_caches.len() {
                let entry_now = entry.take();
                let block = &block_caches[block_idx];
                // Reified hoist: one evaluation, captured verbatim by the next
                // checkpoint. Never recompute it after a mid-block advance —
                // the staleness is part of today's byte contract; mid-block
                // re-entries replay the checkpoint's stale value verbatim.
                block_entry_content_height = match &entry_now {
                    Some(e) if e.resume.keeps_stale_content_height() => e.stale_content_height,
                    _ => effective_content_height(&laid_out_hf, geo, section_page_idx),
                };
                let content_height = block_entry_content_height;

                // Emit any image deferred from an earlier page at the top of this
                // page and register its wrap band, so this page's text flows below it.
                // PageBreakBeforeTop pages uniquely DEFER the drain to the NEXT
                // block's loop-top (variant contract); every other entry replays
                // here the drain overflow_col!'s page arm performed right after
                // capture — equivalent: nothing touches layout state between the
                // two points on either path.
                if !matches!(&entry_now,
                    Some(e) if matches!(e.resume, ResumeAt::PageBreakBeforeTop { .. }))
                {
                    apply_pending_floats(&mut pending_floats, &mut scope, &mut current,
                                         &laid_out_hf, geo, section_page_idx, col_idx);
                }
                // Fixpoint pre-set injection. AFTER the pending
                // drain (required ordering), once per page (`preset_injected`
                // guards; reset on every page advance + in the rollback path). On
                // a re-lay the page float is now in scope from the page's first
                // block, so pre-anchor paragraphs take the ordinary
                // place_float_aware! path and wrap BESIDE/BELOW it — the anchor's
                // own registration is deduped (pre-injected) when the block loop reaches it.
                if !preset_injected && !page_preset.is_empty() {
                    for fr in &page_preset {
                        scope.page_floats.push(fr.af.clone());
                        scope.page_ids.push(fr.id);
                        match &fr.draw {
                            FrozenDraw::Image(im) => current.anchored_images.push(im.clone()),
                            FrozenDraw::TextBox(tb) => current.positioned_text_boxes.push(tb.clone()),
                        }
                    }
                }
                // Set even when page_preset is empty: a page with no preset has
                // "injected" its (empty) set, so a mid-page block never re-checks.
                preset_injected = true;

                match block {
                    BlockCache::Paragraph(cache) => {
                        debug_assert!(
                            !matches!(&entry_now, Some(e) if matches!(e.resume,
                                ResumeAt::RowWhole { .. } | ResumeAt::RowSplit { .. })),
                            "table continuation cannot land on a Paragraph block"
                        );
                        // Replay guards: mid-block entries
                        // skip the preamble their original run executed at block
                        // entry on an EARLIER page. ParaTop skips the spacing steps
                        // (spacing was consumed and discarded by the y reset —
                        // re-adding is the classic replay drift) but re-runs
                        // registration; ParaLines/ParaItems also skip registration
                        // (float registration must NEVER re-run: double draw + band).
                        let skip_spacing_and_break = matches!(&entry_now,
                            Some(e) if matches!(e.resume,
                                ResumeAt::ParaTop { .. }
                                | ResumeAt::ParaLines { .. }
                                | ResumeAt::ParaItems { .. }));
                        let skip_reg_and_reflow = matches!(&entry_now,
                            Some(e) if matches!(e.resume,
                                ResumeAt::ParaLines { .. } | ResumeAt::ParaItems { .. }));
                        // A PageBreakBeforeTop entry has ALREADY performed this
                        // advance; re-firing it emits a spurious extra page.
                        // `!current.is_empty()` was the whole guard and it is NOT
                        // sound on the fixpoint ROLLBACK path: the page-preset
                        // injection above pushes the frozen float DRAWS into
                        // `current` before the block loop resumes, and
                        // `PageLayout::is_empty` counts `positioned_text_boxes` /
                        // `anchored_images` (wml/page.rs), so a re-laid
                        // PageBreakBeforeTop page is non-empty. The comment here
                        // used to claim the opposite ("resumes on an empty page by
                        // construction") and a debug_assert enforced the claim —
                        // both fired as soon as a rollback fixture reached
                        // this variant. Test the entry plan, which is exact.
                        let resumed_page_break = matches!(&entry_now,
                            Some(e) if matches!(e.resume,
                                ResumeAt::PageBreakBeforeTop { .. }));
                        // What IS true by construction: the re-entered page has
                        // emitted no CONTENT yet (preset draws aside).
                        debug_assert!(
                            !resumed_page_break || current.elements.is_empty(),
                            "PageBreakBeforeTop resumes on a page with no emitted content"
                        );
                        if !skip_spacing_and_break && !resumed_page_break
                            && cache.layout.page_break_before && !current.is_empty() {
                            // page_break_before always forces a new page, not just next column.
                            // The break-bearing paragraph itself opens the new page and runs
                            // its full block preamble there (unlike need_break's ParaTop) —
                            // but this page start skips the pending-float drain (see the
                            // PageBreakBeforeTop variant doc), so it gets its own cursor.
                            resume_at = ResumeAt::PageBreakBeforeTop { block: block_idx };
                            seal_or_rollback!('relay);
                            finish_page(&mut current, &mut section_page_idx, &mut global_page_idx, &mut y_in_page, &laid_out_hf, geo)?;
                            let finished = std::mem::replace(&mut current, PageLayout::new());
                            push_page(finished, section_page_idx, sec_idx)?;
                            yield_to_browser().await;
                            if self.layout_gen.get() != my_gen { return Ok(self.pages.borrow().len() as u32); }
                        // Stop BEFORE the memory wall, at the same page
                        // boundary the doc-switch abort uses. Pages so far stay
                        // valid and visible; the post-loop passes (relocation,
                        // PAGE/NUMPAGES) are skipped exactly as on abort, so a
                        // NUMPAGES field in a truncated view shows its cached
                        // value — declared, not silent.
                        if self.should_truncate_layout() {
                            let n = self.pages.borrow().len() as u32;
                            self.layout_truncated_at.set(Some(n));
                            return Ok(n);
                        }
                            section_page_idx += 1;
                            global_page_idx += 1;
                            y_in_page = 0.0;
                            col_idx = 0;
                            scope.clear_page();
                            last_page_start_block = block_idx + 1;
                            prev_space_after_px = 0.0;
                            // Site (b): this page start is pending_floats-un-
                            // drained by design (variant contract), and
                            // prev_space_after_px is captured verbatim as 0.0.
                            capture_checkpoint!();
                        }
                        let at_page_top = current.is_empty() && y_in_page == 0.0;
                        let suppress_spacing = cache.layout.contextual_spacing
                            && prev_para_style.as_ref().is_some_and(|(id, _)|
                                cache.layout.style_id.as_deref() == Some(id.as_str()));
                        if !skip_spacing_and_break && !suppress_spacing {
                            if at_page_top {
                                // Word applies the FIRST paragraph's OWN before-spacing
                                // at a page top (it drops the previous page's trailing
                                // after-spacing, so use space_before, not the max),
                                // UNLESS suppressTopSpacing is set (§17.15.1.x). The
                                // prior code suppressed it unconditionally, dropping a
                                // page-top Title/Heading's before-spacing.
                                //
                                // EXCEPTION: an AUTO before-margin (beforeAutospacing,
                                // §17.3.1.33's HTML emulation) is collapsed away at the
                                // body/page top — CSS first-child margin-collapse at a
                                // block-formatting root. Explicit before-spacing is still
                                // honored.
                                if !w.suppress_top_spacing && !cache.layout.before_auto {
                                    y_in_page += cache.layout.space_before_px;
                                }
                            } else {
                                let gap = prev_space_after_px.max(cache.layout.space_before_px);
                                y_in_page += gap;
                            }
                        }

                        let para_height: f64 = cache.lines.iter()
                            .map(|l| cache.layout.line_height(l.max_font_pt, l.max_ascent + l.max_descent, l.max_image_height))
                            .sum();

                        if !skip_spacing_and_break {
                            let need_break = if cache.layout.keep_lines && y_in_page + para_height > content_height {
                                true
                            } else if cache.layout.keep_next {
                                let next_first_line_h = block_caches.get(block_idx + 1).and_then(|next| match next {
                                    BlockCache::Paragraph(nc) =>
                                        nc.lines.first().map(|l| nc.layout.line_height(l.max_font_pt, l.max_ascent + l.max_descent, l.max_image_height)),
                                    // §17.3.1.15: keepNext also binds the paragraph to the
                                    // FIRST row of a following table. Reserve that (non-
                                    // splittable) row's height so a keepNext heading breaks
                                    // to the next page WITH its table's first row instead of
                                    // orphaning at the page bottom. Empty table -> no rows ->
                                    // None -> 0.0 reserve (behaviour-identical to pre-fix).
                                    //
                                    // DEVIATIONS (accepted, non-looping, follow-on-tracked):
                                    // (i) For a SPLITTABLE multi-line first row this reserves
                                    //     the WHOLE row height, not the spec-literal first
                                    //     LINE; it can push the heading one page earlier than
                                    //     Word. Errs toward keeping heading+table together.
                                    // (ii) A cantSplit first row TALLER than a full page
                                    //     leaves the heading on a lonely page (the row then
                                    //     whole-pushes again at its own break test); this is
                                    //     an unsolvable pathology, not a loop (the
                                    //     `!current.is_empty()` guard below holds).
                                    BlockCache::Table(tc) => tc.rows.first().map(|r| r.height_px),
                                }).unwrap_or(0.0);
                                y_in_page + para_height + next_first_line_h > content_height
                            } else {
                                false
                            };

                            if need_break && !current.is_empty() {
                                // NOT BlockBoundary: the inter-block spacing was already
                                // added to y_in_page above and is discarded by the reset —
                                // the block resumes at its first line, spacing-consumed.
                                overflow_col!('relay, block_idx, ResumeAt::ParaTop { block: block_idx });
                            }
                        }

                        // The registration steps (block_tops, float registration,
                        // retroactive reflow, para_floated): skipped wholesale for a
                        // ParaLines/ParaItems entry — registration already ran
                        // at the block's true entry on an earlier page, and
                        // routing comes from the recorded source. The inert
                        // defaults below are never read on that path (the
                        // wrapper block is deliberately unindented — the body
                        // is byte-identical moved code, like the fast-path
                        // else-branch below).
                        // Dead store on the non-skip path (immediately
                        // overwritten below) and unread on the skip path —
                        // same shape as the resume_at invariant stores.
                        #[allow(unused_assignments)]
                        let mut para_top_page = f64::NAN;
                        let mut para_floated = false;
                        if !skip_reg_and_reflow {
                        // Record this block's top on the current page (post any
                        // break above) as a rewind target for the inset reflow.
                        if doc_has_page_margin_wrap_float {
                            scope.block_tops.push((block_idx, y_in_page));
                        }

                        // Resolve this paragraph's *wrapping* floats to page
                        // coords, register them as active exclusions, and draw
                        // them now. wrapNone floats stay on the existing
                        // post-loop draw path (behavior-preserving — pre-wrap
                        // docs only had wrapNone).
                        para_top_page = effective_margin_top(&laid_out_hf, geo, section_page_idx) + y_in_page;
                        let cur_col = &geo.columns[col_idx];
                        let col_ml = cur_col.x_offset_px;
                        let col_mr = page_w - cur_col.x_offset_px - cur_col.width_px;
                        let dom_left = col_ml + cache.layout.indent_left_px;
                        let dom_right = page_w - col_mr - cache.layout.indent_right_px;
                        let floats_before = scope.column_floats.len();
                        {
                            let eff_margin_top = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                            for (img_idx, ai) in cache.anchored_images.iter().enumerate() {
                                if matches!(ai.wrap.mode, WrapMode::None) { continue; }
                                let id = FloatId { section_idx: sec_idx, block_idx, kind: AnchorKind::Image, within_idx: img_idx };
                                // Cursor-independent (Page|Margin-V) floats are the
                                // fixpoint class: they route to the PAGE tier and
                                // are frozen. Everything else keeps the column tier.
                                let independent = matches!(ai.v_relative_from,
                                    AnchorVRelFrom::Page | AnchorVRelFrom::Margin);
                                if independent {
                                    // Demoted carrier — this float was demoted on
                                    // an earlier page; force-register the FROZEN rect
                                    // + draw on the anchor's page, bypassing defer/
                                    // re-resolve (final float page == final anchor page).
                                    if let Some(fr) = demoted_floats.get(&id) {
                                        scope.page_floats.push(fr.af.clone());
                                        scope.page_ids.push(id);
                                        if let FrozenDraw::Image(im) = &fr.draw {
                                            current.anchored_images.push(im.clone());
                                        }
                                        resolved_this_page.insert(id, (fr.af.clone(), fr.draw.clone()));
                                        continue;
                                    }
                                    // Already pre-injected at page start (this
                                    // float is in page_preset) — skip register AND draw.
                                    if scope.page_ids.contains(&id) { continue; }
                                }
                                let fctx = FrameCtx {
                                    col_x_offset: col_ml,
                                    col_width: cur_col.width_px,
                                    page_w,
                                    page_height: geo.height_px,
                                    eff_margin_top,
                                    y_in_page,
                                    content_height,
                                };
                                let (fx, fy) = resolve_anchor_position(
                                    &ai.pos_h, &ai.pos_v,
                                    ai.h_relative_from, ai.v_relative_from,
                                    ai.width_px, ai.height_px, &fctx,
                                );
                                // If the wrap band overflows this page but the image
                                // would fit on a fresh one, defer it to the next page
                                // (clipping it here loses most of the image). Deferred
                                // floats keep routing to the column tier on drain
                                // (the over-tall single-page deferral is
                                // orthogonal to the cross-column fixpoint).
                                let page_bottom = eff_margin_top + content_height;
                                if fy + ai.height_px > page_bottom
                                    && ai.height_px <= content_height
                                    && fy > eff_margin_top + 1.0
                                {
                                    pending_floats.push(DeferredFloat {
                                        rel_id: ai.rel_id.clone(), fx,
                                        width_px: ai.width_px, height_px: ai.height_px,
                                        wrap: ai.wrap.clone(), behind_text: ai.behind_text,
                                        target: section_page_idx + 1,
                                        id,
                                        anchor_para: cache.para_id,
                                    });
                                } else {
                                    // First discovery — resolve, tier-select, and
                                    // (for the page tier) FREEZE the band + draw so
                                    // every re-lay reuses this exact resolution.
                                    let layout = AnchoredImageLayout {
                                        rel_id: ai.rel_id.clone(),
                                        x: fx, y: fy,
                                        width_px: ai.width_px, height_px: ai.height_px,
                                        behind_text: ai.behind_text,
                                        anchor_para: cache.para_id,
                                    };
                                    if let Some(af) = build_active_float(
                                        &ai.wrap, fx, fy, ai.width_px, ai.height_px, dom_left, dom_right,
                                    ) {
                                        if independent {
                                            // STRICT FREEZE: first resolution wins.
                                            // .entry().or_insert_with keeps a prior
                                            // re-discovery's frozen band/draw immutable
                                            // (a positionH=column/positionV=page float's
                                            // fx could otherwise shift on re-lay).
                                            resolved_this_page.entry(id)
                                                .or_insert_with(|| (af.clone(), FrozenDraw::Image(layout.clone())));
                                            scope.page_floats.push(af);
                                            scope.page_ids.push(id);
                                        } else {
                                            scope.column_floats.push(af);
                                            scope.column_ids.push(id);
                                        }
                                    }
                                    current.anchored_images.push(layout);
                                }
                            }
                            // Anchored text boxes with a wrap mode register an
                            // exclusion here (pre-text), exactly like wrapping
                            // images, so body text flows around — never under —
                            // them (§20.4.2.17). wrapNone text
                            // boxes are placed later (post-text) unchanged.
                            // Oversized-textbox cross-page deferral is not handled
                            // (rare; a known limitation): it registers + draws
                            // on its anchor page.
                            for (tb_idx, tb) in cache.anchored_text_boxes.iter().enumerate() {
                                if matches!(tb.wrap.mode, WrapMode::None) { continue; }
                                let id = FloatId { section_idx: sec_idx, block_idx, kind: AnchorKind::TextBox, within_idx: tb_idx };
                                let independent = matches!(tb.v_relative_from,
                                    AnchorVRelFrom::Page | AnchorVRelFrom::Margin);
                                if independent {
                                    // Demoted carrier (see the image loop).
                                    if let Some(fr) = demoted_floats.get(&id) {
                                        scope.page_floats.push(fr.af.clone());
                                        scope.page_ids.push(id);
                                        if let FrozenDraw::TextBox(tbx) = &fr.draw {
                                            current.positioned_text_boxes.push(tbx.clone());
                                        }
                                        resolved_this_page.insert(id, (fr.af.clone(), fr.draw.clone()));
                                        continue;
                                    }
                                    // Already pre-injected at page start.
                                    if scope.page_ids.contains(&id) { continue; }
                                }
                                let fctx = FrameCtx {
                                    col_x_offset: col_ml,
                                    col_width: cur_col.width_px,
                                    page_w,
                                    page_height: geo.height_px,
                                    eff_margin_top,
                                    y_in_page,
                                    content_height,
                                };
                                let (tx, ty) = resolve_anchor_position(
                                    &tb.pos_h, &tb.pos_v,
                                    tb.h_relative_from, tb.v_relative_from,
                                    tb.width_px, tb.height_px, &fctx,
                                );
                                let tb_lines = layout_text_box(
                                    &tb.raw, tb.width_px,
                                    &self.font_cache, &w.style_ctx,
                                    w.default_ppr.as_ref(),
                                    w.default_tab_stop_px, &w.theme_fonts,
                                    &w.numbering_levels,
                                );
                                let ptb = PositionedTextBox {
                                    lines: tb_lines,
                                    x: tx, y: ty,
                                    width_px: tb.width_px,
                                    height_px: tb.height_px,
                                    behind_text: tb.behind_text,
                                    fill_color: tb.fill_color.clone(),
                                    border_color: tb.border_color.clone(),
                                    border_width_px: tb.border_width_px,
                                };
                                // First discovery — tier-select + FREEZE.
                                if let Some(af) = build_active_float(
                                    &tb.wrap, tx, ty, tb.width_px, tb.height_px, dom_left, dom_right,
                                ) {
                                    if independent {
                                        // STRICT FREEZE: first resolution wins (see the
                                        // image first-discovery site) — frozen-rect is immutable.
                                        resolved_this_page.entry(id)
                                            .or_insert_with(|| (af.clone(), FrozenDraw::TextBox(ptb.clone())));
                                        scope.page_floats.push(af);
                                        scope.page_ids.push(id);
                                    } else {
                                        scope.column_floats.push(af);
                                        scope.column_ids.push(id);
                                    }
                                }
                                current.positioned_text_boxes.push(ptb);
                            }
                        }

                        // ── RETROACTIVE REFLOW (empty-para full-width) ──
                        // Cursor-DEPENDENT paragraph-relative full-width floats:
                        // an empty paragraph emitted ABOVE the float must be pushed
                        // BELOW it (Word behavior). This is the ONE reflow that
                        // survives the per-page fixpoint: it fires for paragraph-relative floats
                        // that the per-page fixpoint's trigger class (cursor-
                        // INDEPENDENT Page|Margin floats) does not cover, and it is
                        // pinned by oracle geometry (the page-4 case observed on
                        // calibration doc C6). Cursor-independent inset AND full-width real-
                        // text floats are now the per-page fixpoint;
                        // the former bounded inset reflow is DELETED — its
                        // cases are subsumed by the fixpoint's pre-registration.
                        //
                        // BLOCK 3: when this paragraph registers a NEW full-width float,
                        // an empty paragraph that PRECEDED it was already emitted ABOVE
                        // the float. Word pushes such empties BELOW it. Inert unless:
                        // (1) a NEW full-width float was added; (2) only whitespace-only
                        // lines are popped; (3) only lines above the float top; (4)
                        // cross-page floats go to pending_floats, never scope.column_floats.
                        if scope.column_floats.len() > floats_before {
                            let reflow_top = min_float_top(
                                &scope.column_floats[floats_before..],
                                |f| f.spans_full_width(dom_left, dom_right));
                            if reflow_top.is_finite() {
                                let margin_top = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                                let content_height = effective_content_height(&laid_out_hf, geo, section_page_idx);
                                // Pop trailing empty mark lines above the float top.
                                // Capture each line's source-paragraph after-spacing so
                                // the §17.3.1.33 inter-paragraph gap survives the reflow
                                // (the bare clear loop alone would drop it). The popped
                                // lines map 1:1 onto the consecutive empty paragraph
                                // blocks ending at block_idx-1 (one line each).
                                let mut popped: Vec<(PagedLine, f64, f64)> = Vec::new();
                                let mut sidx = block_idx;
                                #[allow(clippy::while_let_loop)] // pops while trailing empty
                                loop {
                                    let (lh, baseline_y, is_empty) = match current.elements.last() {
                                        Some(PageElement::Line(pl)) => (
                                            pl.line_height, pl.baseline_y,
                                            pl.segments.iter()
                                                .all(|s| s.items.iter().all(|it| it.is_only_whitespace())),
                                        ),
                                        _ => break,
                                    };
                                    if !is_empty { break; }
                                    let line_top = margin_top + (y_in_page - lh);
                                    if line_top >= reflow_top - WRAP_EPS { break; }
                                    let ascent = baseline_y - line_top;
                                    sidx = sidx.saturating_sub(1);
                                    // Only reflow a line mapping to a genuinely EMPTY,
                                    // single-line paragraph. Refuses to fragment a
                                    // non-empty paragraph whose trailing line is
                                    // whitespace-only, and avoids sidx drift on a
                                    // multi-line empty (<w:br/>) — either leaves the
                                    // layout untouched (conservative).
                                    let after = match block_caches.get(sidx) {
                                        Some(BlockCache::Paragraph(c))
                                            if c.lines.len() == 1
                                                && c.lines.iter().all(|l| {
                                                    l.items.iter().all(|it| it.is_only_whitespace())
                                                }) =>
                                        {
                                            c.layout.space_after_px
                                        }
                                        _ => break,
                                    };
                                    if let Some(PageElement::Line(pl)) = current.elements.pop() {
                                        y_in_page -= pl.line_height;
                                        popped.push((pl, ascent, after));
                                    } else {
                                        break;
                                    }
                                }
                                // Re-emit in document order, clearing below the float and
                                // re-applying each paragraph's after-spacing as the gap to
                                // the next (matching Word's stacked below-float empties).
                                let reflowed = !popped.is_empty();
                                let mut last_after = 0.0_f64;
                                for (mut pl, ascent, after) in popped.into_iter().rev() {
                                    let lh = pl.line_height;
                                    // Start at/below the new float top so the clear loop's
                                    // overlap test fires (the popped line's old position was
                                    // ABOVE the float, where there is free width). This seed
                                    // MUST stay before each call.
                                    y_in_page = y_in_page.max(reflow_top - margin_top);
                                    // page_has_content is re-evaluated each iteration: the
                                    // `current.push` below mutates it between popped lines.
                                    clear_below_floats(
                                        scope.floats().as_ref(), &mut y_in_page, margin_top, lh,
                                        dom_left, dom_right, 2.0 * WRAP_EPS, content_height,
                                        !current.is_empty());
                                    pl.baseline_y = margin_top + y_in_page + ascent;
                                    current.push(PageElement::Line(pl));
                                    y_in_page += lh + after;
                                    last_after = after;
                                }
                                // §17.3.1.33: the float-bearing paragraph's OWN
                                // before-spacing (its pre-float before-gap was discarded by
                                // the clear). Apply max(last_empty.after, this.before),
                                // netting the `after` already added above. Zero on
                                // calibration doc C6 (float-bearer before=0).
                                if reflowed {
                                    y_in_page += (cache.layout.space_before_px - last_after).max(0.0);
                                }
                            }
                        }

                        // ── Bounded inset reflow — DELETED (subsumed by the per-page fixpoint).
                        // Cursor-independent inset floats are now the per-page fixpoint: on
                        // re-lay the float is in scope from page start, so pre-anchor paragraphs
                        // take the ordinary place_float_aware! path (proven by block4_inert_
                        // under_live_fixpoint before deletion). The kernel (min_float_top,
                        // clear_below_floats, block_tops, seal_page) stays — block_tops now feeds
                        // seal_page, not this block.

                        para_floated = scope.floats().iter()
                            .any(|f| f.overlaps_band(para_top_page, para_top_page + para_height));
                        }

                        let route = match &entry_now {
                            Some(EntryPlan { resume: ResumeAt::ParaLines { next_line, src, .. }, .. }) => match src {
                                ParaLineSrc::OverflowBreak =>
                                    ParaRoute::FastPath { start: *next_line, resume_after_break: true },
                                ParaLineSrc::ExplicitBreak =>
                                    ParaRoute::FastPath { start: *next_line, resume_after_break: false },
                                ParaLineSrc::EmptyPara { macro_content_height } =>
                                    ParaRoute::EmptyPara { start: *next_line, resume_ch: Some(*macro_content_height) },
                            },
                            Some(EntryPlan { resume: ResumeAt::ParaItems { is_first_batch, src, .. }, para_remainder, .. }) => {
                                let items = para_remainder.clone()
                                    .expect("ParaItems checkpoint owns its remainder");
                                match src {
                                    ParaItemsSrc::FloatAware =>
                                        ParaRoute::FloatAware { items, first_batch: *is_first_batch },
                                    ParaItemsSrc::UnequalCols =>
                                        ParaRoute::UnequalCols { items, first_batch: *is_first_batch },
                                }
                            }
                            // BlockBoundary / PageBreakBeforeTop / ParaTop ran the
                            // full preamble above — pristine routing.
                            _ => {
                                if !para_floated && cols_unequal {
                                    ParaRoute::UnequalCols {
                                        items: logical_para_items(cache),
                                        first_batch: true,
                                    }
                                } else if !para_floated {
                                    ParaRoute::FastPath { start: 0, resume_after_break: false }
                                } else {
                                    let items: Vec<Item> = logical_para_items(cache);
                                    if items.is_empty() {
                                        ParaRoute::EmptyPara { start: 0, resume_ch: None }
                                    } else {
                                        ParaRoute::FloatAware { items, first_batch: true }
                                    }
                                }
                            }
                        };

                        match route {
                            ParaRoute::UnequalCols { items, first_batch } => {
                            // Unequal columns: re-pack the paragraph at each column's
                            // actual width, re-wrapping at column boundaries. Items are
                            // flattened from the pre-packed lines (inter-word spaces live
                            // inside word items, so this is lossless — same approach as the
                            // float reflow path). The paragraph's RESOLVED tab stops are
                            // carried on its cache so this
                            // re-pack aligns tabs exactly like the equal-column path.
                            let tab_stops = &cache.tab_stops;
                            let mut remaining: Vec<Item> = items;
                            let mut is_first_batch = first_batch;
                            while !remaining.is_empty() {
                                let col = &geo.columns[col_idx];
                                let col_ml = col.x_offset_px;
                                let col_mr = page_w - col.x_offset_px - col.width_px;
                                let fli = if is_first_batch { cache.layout.indent_first_line_px } else { 0.0 };
                                let avail = cache.layout.available_width(page_w, col_ml, col_mr, 0.0);
                                let packed = pack_lines(remaining.clone(), avail, fli, &self.font_cache, tab_stops, cache.layout.base_rtl);
                                let mut placed = 0usize;
                                for (pidx, line) in packed.iter().enumerate() {
                                    let lh = cache.layout.line_height(
                                        line.max_font_pt, line.max_ascent + line.max_descent, line.max_image_height);
                                    if y_in_page + lh > content_height && !current.is_empty() {
                                        break;
                                    }
                                    let cur = &geo.columns[col_idx];
                                    let cml = cur.x_offset_px;
                                    let cmr = page_w - cur.x_offset_px - cur.width_px;
                                    let margin_top = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                                    let lie = if is_first_batch && pidx == 0 { cache.layout.indent_first_line_px } else { 0.0 };
                                    let is_last_line = pidx == packed.len() - 1;
                                    let mut items = line.items.clone();
                                    let avail2 = cache.layout.available_width(page_w, cml, cmr, lie);
                                    if cache.layout.alignment == STJc::Both && !is_last_line {
                                        justify_line(&mut items, avail2);
                                    }
                                    let x_start = cache.layout.x_start(page_w, cml, cmr, line.width(), lie);
                                    let baseline_y = margin_top + y_in_page + line.max_ascent;
                                    current.push(PageElement::Line(PagedLine {
                                        segments: PagedLine::single(items, x_start),
                                        para: cache.para_id.unwrap_or(PARA_SENTINEL),
                                        baseline_y,
                                        para_shading: cache.layout.shading_color.clone(),
                                        line_height: lh,
                                        content_left: cml + cache.layout.indent_left_px,
                                        content_right: page_w - cmr - cache.layout.indent_right_px,
                                        border_top: cache.layout.border_top.clone(),
                                        border_bottom: cache.layout.border_bottom.clone(),
                                        border_left: cache.layout.border_left.clone(),
                                        border_right: cache.layout.border_right.clone(),
                                        bar_tabs: cache.tab_stops.bar_xs(
                                            cml + cache.layout.indent_left_px,
                                            page_w - cmr - cache.layout.indent_right_px,
                                            cache.layout.base_rtl),
                                        is_first_line: is_first_batch && pidx == 0,
                                        is_last_line,
                                    }));
                                    y_in_page += lh;
                                    placed += 1;
                                }
                                if placed == packed.len() {
                                    remaining.clear();
                                } else {
                                    remaining = packed[placed..].iter().flat_map(|l| l.items.clone()).collect();
                                    overflow_col!('relay, block_idx, ResumeAt::ParaItems {
                                        block: block_idx,
                                        items_remaining: remaining.len(),
                                        is_first_batch: false,
                                        src: ParaItemsSrc::UnequalCols,
                                    }, remainder = remaining);
                                    is_first_batch = false;
                                }
                            }
                            }
                            ParaRoute::FastPath { start, resume_after_break } => {
                        // One-shot: true only for the first iteration of an
                        // OverflowBreak re-entry, which lands MID-iteration —
                        // the original advance already happened, so
                        // should_break AND widow are skipped and ONLY the
                        // deferred-float handoff replays before the line is
                        // placed unconditionally at the current y.
                        let mut resume_after_break = resume_after_break;
                        for (line_idx, line) in cache.lines.iter().enumerate().skip(start) {
                            let line_height = cache.layout.line_height(line.max_font_pt, line.max_ascent + line.max_descent, line.max_image_height);
                            let run_handoff = if std::mem::replace(&mut resume_after_break, false) {
                                true
                            } else {
                            let mut should_break = y_in_page + line_height > content_height && !current.is_empty();

                            if !should_break && cache.layout.widow_control && cache.lines.len() > 2 {
                                let remaining = cache.lines.len() - line_idx;
                                if remaining == 2 {
                                    let next_lh = cache.layout.line_height(
                                        cache.lines[line_idx + 1].max_font_pt,
                                        cache.lines[line_idx + 1].max_ascent + cache.lines[line_idx + 1].max_descent,
                                        cache.lines[line_idx + 1].max_image_height,
                                    );
                                    if y_in_page + line_height + next_lh > content_height {
                                        should_break = true;
                                    }
                                }
                            }

                            if should_break {
                                // Line `line_idx` opens the new page — whether via this
                                // loop or the float-aware handoff below, which consumes
                                // the same cache.lines[line_idx..] content.
                                overflow_col!('relay, block_idx,
                                    ResumeAt::ParaLines { block: block_idx, next_line: line_idx,
                                        src: ParaLineSrc::OverflowBreak });
                            }
                            should_break
                            };
                            if run_handoff {
                                // The page advance may have brought a deferred float
                                // onto this page (apply_pending_floats runs inside
                                // overflow_col!, and a re-entry's loop-top drain
                                // replays it). If it overlaps the band where the
                                // rest of this paragraph would flow, hand the
                                // remainder to the float-aware re-pack path so it
                                // wraps around the image instead of overlapping it
                                // (§20.4.2.17). The no-float fast path (this loop —
                                // widow-control + custom tabs) is untouched whenever
                                // the exclusion set (scope.floats()) is empty, i.e. for all non-float docs.
                                if !scope.floats().is_empty() {
                                    let mtop = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                                    let band_top = mtop + y_in_page;
                                    let rem_h: f64 = cache.lines[line_idx..].iter()
                                        .map(|l| cache.layout.line_height(
                                            l.max_font_pt, l.max_ascent + l.max_descent, l.max_image_height))
                                        .sum();
                                    if scope.floats().iter().any(|f| f.overlaps_band(band_top, band_top + rem_h)) {
                                        let remaining: Vec<Item> = cache.lines[line_idx..].iter()
                                            .flat_map(|l| l.items.clone()).collect();
                                        place_float_aware!('relay, cache, block_idx, remaining, line_idx == 0, false);
                                        break;
                                    }
                                }
                            }
                            // Re-read col after potential column/page advance.
                            let col = &geo.columns[col_idx];
                            let col_ml = col.x_offset_px;
                            let col_mr = page_w - col.x_offset_px - col.width_px;
                            let margin_top = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                            let line_indent_extra = if line_idx == 0 {
                                cache.layout.indent_first_line_px
                            } else {
                                0.0
                            };
                            let is_last_line = line_idx == cache.lines.len() - 1
                                || line.ends_with_page_break;
                            let mut items = line.items.clone();
                            let avail = cache.layout.available_width(
                                page_w, col_ml, col_mr, line_indent_extra,
                            );
                            if cache.layout.alignment == STJc::Both && !is_last_line {
                                justify_line(&mut items, avail);
                            }
                            let x_start = cache.layout.x_start(
                                page_w, col_ml, col_mr,
                                line.width(), line_indent_extra,
                            );
                            let baseline_y = margin_top + y_in_page + line.max_ascent;
                            let is_first = line_idx == 0;
                            current.push(PageElement::Line(PagedLine {
                                segments: PagedLine::single(items, x_start),
                                para: cache.para_id.unwrap_or(PARA_SENTINEL),
                                baseline_y,
                                para_shading: cache.layout.shading_color.clone(),
                                line_height,
                                content_left: col_ml + cache.layout.indent_left_px,
                                content_right: page_w - col_mr - cache.layout.indent_right_px,
                                border_top: cache.layout.border_top.clone(),
                                border_bottom: cache.layout.border_bottom.clone(),
                                border_left: cache.layout.border_left.clone(),
                                border_right: cache.layout.border_right.clone(),
                                bar_tabs: cache.tab_stops.bar_xs(
                                    col_ml + cache.layout.indent_left_px,
                                    page_w - col_mr - cache.layout.indent_right_px,
                                    cache.layout.base_rtl),
                                is_first_line: is_first,
                                is_last_line,
                            }));
                            y_in_page += line_height;

                            if line.ends_with_page_break {
                                // Explicit break AFTER placing line_idx; next_line ==
                                // lines.len() when it was the last line (block done).
                                overflow_col!('relay, block_idx,
                                    ResumeAt::ParaLines { block: block_idx, next_line: line_idx + 1,
                                        src: ParaLineSrc::ExplicitBreak });
                            }
                        }
                            }
                            ParaRoute::EmptyPara { start, resume_ch } => {
                                // Truly-empty paragraph: place_float_aware! would drop
                                // it (empty batch). Handle its mark line(s) directly so
                                // they clear below a full-width float. A resume
                                // re-enters the macro at the mark line that fired the
                                // advance, replaying the captured macro-entry
                                // content_height (see ParaLineSrc::EmptyPara).
                                match resume_ch {
                                    None => place_empty_para_around_floats!('relay, cache, block_idx),
                                    Some(ch) => place_empty_para_around_floats!('relay,
                                        cache, block_idx, start, true, ch),
                                }
                            }
                            ParaRoute::FloatAware { items, first_batch } => {
                                // Float-aware path: re-pack this paragraph's items
                                // around active floats, re-flowing the remainder onto
                                // each subsequent page (step 2). Floated from the start,
                                // so the first batch keeps the first-line indent; a
                                // ParaItems re-entry passes the checkpoint-owned
                                // remainder with first_batch = false (an EMPTY remainder
                                // re-enters just so the arm epilogue replays).
                                place_float_aware!('relay, cache, block_idx, items, first_batch, true);
                            }
                        }
                        prev_space_after_px = if suppress_spacing {
                            0.0
                        } else {
                            cache.layout.space_after_px
                        };
                        prev_para_style = cache.layout.style_id.clone()
                            .map(|id| (id, cache.layout.contextual_spacing));
                        for &fn_id in &cache.footnote_ids {
                            if !current.footnote_ids.contains(&fn_id) {
                                current.footnote_ids.push(fn_id);
                            }
                        }
                        for &en_id in &cache.endnote_ids {
                            if !current.endnote_ids.contains(&en_id) {
                                current.endnote_ids.push(en_id);
                            }
                        }
                        let eff_margin_top = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                        for ai in &cache.anchored_images {
                            // Wrapping floats were already resolved + drawn
                            // pre-loop; this path handles only wrapNone.
                            if !matches!(ai.wrap.mode, WrapMode::None) { continue; }
                            let cur_col2 = &geo.columns[col_idx];
                            let fctx = FrameCtx {
                                col_x_offset: cur_col2.x_offset_px,
                                col_width: cur_col2.width_px,
                                page_w,
                                page_height: geo.height_px,
                                eff_margin_top,
                                y_in_page,
                                content_height,
                            };
                            let (x, y) = resolve_anchor_position(
                                &ai.pos_h, &ai.pos_v,
                                ai.h_relative_from, ai.v_relative_from,
                                ai.width_px, ai.height_px, &fctx,
                            );
                            current.anchored_images.push(AnchoredImageLayout {
                                rel_id: ai.rel_id.clone(),
                                x, y,
                                width_px: ai.width_px,
                                height_px: ai.height_px,
                                behind_text: ai.behind_text,
                                anchor_para: cache.para_id,
                            });
                        }
                        for tb in &cache.anchored_text_boxes {
                            // Wrapping text boxes were resolved + registered in the
                            // pre-loop (with their exclusion); this path handles
                            // only wrapNone (drawn over/under text, no exclusion).
                            if !matches!(tb.wrap.mode, WrapMode::None) { continue; }
                            let tb_lines = layout_text_box(
                                &tb.raw, tb.width_px,
                                &self.font_cache, &w.style_ctx,
                                w.default_ppr.as_ref(),
                                w.default_tab_stop_px, &w.theme_fonts,
                                &w.numbering_levels,
                            );
                            let cur_col3 = &geo.columns[col_idx];
                            let fctx = FrameCtx {
                                col_x_offset: cur_col3.x_offset_px,
                                col_width: cur_col3.width_px,
                                page_w,
                                page_height: geo.height_px,
                                eff_margin_top,
                                y_in_page,
                                content_height,
                            };
                            let (tx, ty) = resolve_anchor_position(
                                &tb.pos_h, &tb.pos_v,
                                tb.h_relative_from, tb.v_relative_from,
                                tb.width_px, tb.height_px, &fctx,
                            );
                            current.positioned_text_boxes.push(PositionedTextBox {
                                lines: tb_lines,
                                x: tx, y: ty,
                                width_px: tb.width_px,
                                height_px: tb.height_px,
                                behind_text: tb.behind_text,
                                fill_color: tb.fill_color.clone(),
                                border_color: tb.border_color.clone(),
                                border_width_px: tb.border_width_px,
                            });
                        }
                    }
                    BlockCache::Table(table) => {
                        debug_assert!(
                            !matches!(&entry_now, Some(e) if matches!(e.resume,
                                ResumeAt::ParaTop { .. }
                                | ResumeAt::ParaLines { .. }
                                | ResumeAt::ParaItems { .. })),
                            "paragraph continuation cannot land on a Table block"
                        );
                        // Re-entry state: the resumed row's already-made
                        // decisions (want_split / whole-row overflow) must not
                        // re-run; a RowSplit resume re-enters the fragment loop
                        // with the checkpoint cursors. Each matches at most one
                        // row_idx, so both are one-shot by construction.
                        let mut split_resume: Option<(usize, usize, Vec<usize>, f64)> = None;
                        let mut whole_resume: Option<usize> = None;
                        match &entry_now {
                            Some(EntryPlan { resume: ResumeAt::RowSplit { row, entry_col, cursors, consumed, .. }, .. }) => {
                                split_resume = Some((*row, *entry_col, cursors.clone(), *consumed));
                            }
                            Some(EntryPlan { resume: ResumeAt::RowWhole { next_row, .. }, .. }) => {
                                whole_resume = Some(*next_row);
                            }
                            _ => {}
                        }
                        let tbl_resumed = split_resume.is_some() || whole_resume.is_some();
                        let start_row = split_resume.as_ref().map(|(r, ..)| *r)
                            .or(whole_resume)
                            .unwrap_or(0);
                        // Consume the preceding paragraph's after-spacing before
                        // the table. The paragraph arm only HOLDS prev_space_after_px
                        // for the next block; a <w:tbl> must honor it like a paragraph
                        // would, else the table sits too high (e.g. calibration doc C6's
                        // page-2 table, preceded by an empty para with after=225, was
                        // ~10.6pt too high). Suppressed at a page top, like paragraphs.
                        // Consumed ONCE at table entry: a mid-table re-entry's original
                        // run consumed it on an earlier page — never re-add.
                        if !tbl_resumed {
                            let table_at_page_top = current.is_empty() && y_in_page == 0.0;
                            if !table_at_page_top {
                                y_in_page += prev_space_after_px;
                            }
                            prev_space_after_px = 0.0;
                        }
                        'row_loop: for (row_idx, row) in table.rows.iter().enumerate().skip(start_row) {
                            // RowSplit re-entry: resume the fragment loop with
                            // the checkpoint cursors; cell x geometry comes from
                            // the ENTRY column (the split-row quirk contract).
                            if split_resume.as_ref().is_some_and(|(r, ..)| *r == row_idx) {
                                let (_, entry_col, cursors, consumed) = split_resume.take().unwrap();
                                place_split_row!('relay, table, row, row_idx, block_idx,
                                    entry_col, cursors, consumed);
                                continue 'row_loop;
                            }
                            // RowWhole re-entry: want_split and the whole-row
                            // overflow test were decided PRE-advance on the
                            // original run — emit the row verbatim.
                            let resume_whole = whole_resume == Some(row_idx);
                            if !resume_whole {
                            // ── Row-split trigger (§17.4.6/§17.4.7, line
                            // granularity). The default whole-row path is taken UNLESS all
                            // of: (a) the row is taller than the page remainder;
                            // (b) it is not cantSplit (§17.4.7); (c) it has ≥2
                            // splittable lines total; (d) at least the first line
                            // fits in the remainder. The ≥2-lines clause is the
                            // single-line firewall — a single-line row has no
                            // remainder to carry, so it whole-pushes exactly as
                            // before (keeps the oracle-flow 36-row pin ORIGINAL).
                            let remaining = content_height - y_in_page;
                            let row_top_chrome = {
                                // Top margin + first-line ascent budget of the
                                // tallest cell — the minimum height one line needs.
                                row.cells.iter()
                                    .filter(|c| !c.v_merged_away)
                                    .map(|c| {
                                        let first_h = flatten_cell_lines(c, 0.0).first().map(|p| p.height).unwrap_or(0.0);
                                        c.mar_top + first_h
                                    })
                                    .fold(0.0_f64, f64::max)
                            };
                            let total_lines: usize = row.cells.iter()
                                .filter(|c| !c.v_merged_away)
                                .map(|c| flatten_cell_lines(c, 0.0).len())
                                .sum();
                            let first_line_fits = row_top_chrome <= remaining;
                            let want_split = !row.cant_split
                                && row.height_px > remaining
                                && total_lines >= 2
                                && first_line_fits
                                && !current.is_empty()
                                // A cell-hosted float is laid out cell-locally for
                                // the WHOLE-row path only; split-row cell floats
                                // are not yet supported (a known limitation).
                                && !row.cells.iter().any(|c| c.float_layout.is_some());

                            if want_split {
                                place_split_row!('relay, table, row, row_idx, block_idx);
                                continue 'row_loop;
                            }

                            if y_in_page + row.height_px > content_height && !current.is_empty() {
                                // Whole-row push: row `row_idx` opens the new page.
                                overflow_col!('relay, block_idx,
                                    ResumeAt::RowWhole { block: block_idx, next_row: row_idx });
                            }
                            }

                            let col = &geo.columns[col_idx];
                            let col_ml = col.x_offset_px;
                            let col_cw = col.width_px;
                            let margin_top = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                            let row_y_top = margin_top + y_in_page;
                            let mut paged_cells = Vec::new();
                            let table_width: f64 = table.col_widths_px.iter().sum();
                            let table_offset = match table.alignment {
                                STJcTable::Center => (col_cw - table_width) / 2.0,
                                STJcTable::End | STJcTable::Right => col_cw - table_width,
                                _ => table.indent_px,
                            };
                            let mut cell_x = col_ml + table_offset;

                            let tbl_cm_h = table.cell_margin_h;
                            let tbl_cm_v = table.cell_margin_v;

                            for cell in &row.cells {
                                let mut cell_lines = Vec::new();
                                let cell_vmar = cell.mar_top + cell.mar_bottom;
                                let v_offset = match cell.v_align {
                                    CellVAlign::Center => {
                                        let ch = cell.content_height(0.0);
                                        ((row.height_px - cell_vmar) - ch).max(0.0) / 2.0
                                    }
                                    CellVAlign::Bottom => {
                                        let ch = cell.content_height(0.0);
                                        ((row.height_px - cell_vmar) - ch).max(0.0)
                                    }
                                    CellVAlign::Top => 0.0,
                                };
                                let cell_y_origin = row_y_top + cell.mar_top + v_offset;
                                // Cell-hosted wrapping float (§20.4.2.17): emit
                                // the pre-resolved cell-local layout translated to
                                // the cell's page origin. Float-free cells fall
                                // through to the verbatim block loop below
                                // (byte-identical — branch, do not merge).
                                if let Some(fl) = &cell.float_layout {
                                    for d in &fl.draws {
                                        match d {
                                            CellFloatDraw::Image { rel_id, x, y, width_px, height_px, behind_text, anchor_para } => {
                                                current.anchored_images.push(AnchoredImageLayout {
                                                    rel_id: rel_id.clone(),
                                                    x: cell_x + x, y: cell_y_origin + y,
                                                    width_px: *width_px, height_px: *height_px,
                                                    behind_text: *behind_text,
                                                    anchor_para: *anchor_para,
                                                });
                                            }
                                            CellFloatDraw::TextBox {
                                                lines, x, y, width_px, height_px, behind_text,
                                                fill_color, border_color, border_width_px,
                                            } => {
                                                current.positioned_text_boxes.push(PositionedTextBox {
                                                    lines: lines.clone(),
                                                    x: cell_x + x, y: cell_y_origin + y,
                                                    width_px: *width_px, height_px: *height_px,
                                                    behind_text: *behind_text,
                                                    fill_color: fill_color.clone(),
                                                    border_color: border_color.clone(),
                                                    border_width_px: *border_width_px,
                                                });
                                            }
                                        }
                                    }
                                    for pl in &fl.lines {
                                        let mut pl = pl.clone();
                                        for s in &mut pl.segments {
                                            s.x_start += cell_x;
                                        }
                                        pl.baseline_y += cell_y_origin;
                                        pl.content_left += cell_x;
                                        pl.content_right += cell_x;
                                        for bx in &mut pl.bar_tabs {
                                            *bx += cell_x;
                                        }
                                        cell_lines.push(pl);
                                    }
                                }

                                let mut cell_y = cell_y_origin;
                                let mut cell_prev_after: f64 = 0.0;
                                for block in if cell.float_layout.is_some() {
                                    &[][..]
                                } else {
                                    &cell.blocks[..]
                                } {
                                    match block {
                                        CellBlock::Paragraph(pc) => {
                                            let gap = cell_prev_after.max(pc.layout.space_before_px);
                                            cell_y += gap;
                                            for (li, line) in pc.lines.iter().enumerate() {
                                                let lh = pc.layout.line_height(line.max_font_pt, line.max_ascent + line.max_descent, line.max_image_height);
                                                let indent_extra = if li == 0 {
                                                    pc.layout.indent_first_line_px
                                                } else {
                                                    0.0
                                                };
                                                let x_start = pc.layout.x_start(
                                                    cell_x + cell.width_px,
                                                    cell_x + cell.mar_left,
                                                    cell.mar_left,
                                                    line.width(),
                                                    indent_extra,
                                                );
                                                let baseline_y = cell_y + line.max_ascent;
                                                cell_lines.push(PagedLine {
                                                    segments: PagedLine::single(line.items.clone(), x_start),
                                                    para: pc.para_id.unwrap_or(PARA_SENTINEL),
                                                    baseline_y,
                                                    para_shading: None,
                                                    line_height: lh,
                                                    content_left: x_start,
                                                    content_right: cell_x + cell.width_px - cell.mar_right,
                                                    border_top: None, border_bottom: None,
                                                    border_left: None, border_right: None,
                                                    bar_tabs: pc.tab_stops.bar_xs(
                                                        cell_x + cell.mar_left + pc.layout.indent_left_px,
                                                        cell_x + cell.width_px - cell.mar_right - pc.layout.indent_right_px,
                                                        pc.layout.base_rtl),
                                                    is_first_line: false, is_last_line: false,
                                                });
                                                cell_y += lh;
                                            }
                                            cell_prev_after = pc.layout.space_after_px;
                                        }
                                        CellBlock::Table(nested) => {
                                            cell_y += cell_prev_after;
                                            let nested_x = cell_x + cell.mar_left;
                                            for nrow in &nested.rows {
                                                let nrow_top = cell_y;
                                                for ncell in &nrow.cells {
                                                    if ncell.v_merged_away { continue; }
                                                    let ncell_x = nested_x + nested.col_widths_px[..ncell.col_start].iter().sum::<f64>();
                                                    let mut ny = nrow_top + nested.cell_margin_v;
                                                    for nb in &ncell.blocks {
                                                        if let CellBlock::Paragraph(npc) = nb {
                                                            for (nli, nline) in npc.lines.iter().enumerate() {
                                                                let nlh = npc.layout.line_height(nline.max_font_pt, nline.max_ascent + nline.max_descent, nline.max_image_height);
                                                                let nindent = if nli == 0 { npc.layout.indent_first_line_px } else { 0.0 };
                                                                let nx = npc.layout.x_start(
                                                                    ncell_x + ncell.width_px,
                                                                    ncell_x + nested.cell_margin_h,
                                                                    nested.cell_margin_h,
                                                                    nline.width(), nindent,
                                                                );
                                                                cell_lines.push(PagedLine {
                                                                    segments: PagedLine::single(nline.items.clone(), nx),
                                                                    para: npc.para_id.unwrap_or(PARA_SENTINEL),
                                                                    baseline_y: ny + nline.max_ascent,
                                                                    para_shading: None,
                                                                    line_height: nlh,
                                                                    content_left: ncell_x + nested.cell_margin_h,
                                                                    content_right: ncell_x + ncell.width_px - nested.cell_margin_h,
                                                                    border_top: None, border_bottom: None,
                                                                    border_left: None, border_right: None,
                                                                    bar_tabs: npc.tab_stops.bar_xs(
                                                                        ncell_x + nested.cell_margin_h + npc.layout.indent_left_px,
                                                                        ncell_x + ncell.width_px - nested.cell_margin_h - npc.layout.indent_right_px,
                                                                        npc.layout.base_rtl),
                                                                    is_first_line: false, is_last_line: false,
                                                                });
                                                                ny += nlh;
                                                            }
                                                        }
                                                    }
                                                }
                                                cell_y += nrow.height_px;
                                            }
                                            cell_prev_after = 0.0;
                                        }
                                    }
                                }

                                paged_cells.push(PagedTableCell {
                                    lines: cell_lines,
                                    x: cell_x,
                                    width_px: cell.width_px,
                                    fill_color: cell.fill_color.clone(),
                                    v_merged_away: cell.v_merged_away,
                                    border_top: cell.border_top.clone(),
                                    border_bottom: cell.border_bottom.clone(),
                                    border_left: cell.border_left.clone(),
                                    border_right: cell.border_right.clone(),
                                });
                                cell_x += cell.width_px;
                            }

                            current.push(PageElement::TableRow(PagedTableRow {
                                cells: paged_cells,
                                y_top: row_y_top,
                                height_px: row.height_px,
                                border_color: table.border_color.clone(),
                                has_borders: table.has_borders,
                                cell_margin_h: tbl_cm_h,
                                cell_margin_v: tbl_cm_v,
                            }));
                            y_in_page += row.height_px;
                        }
                        prev_para_style = None;
                        prev_space_after_px = 0.0;
                    }
                }
                // Block complete: any mid-block resume position is now stale.
                // A dead store TODAY (every capture site writes resume_at at
                // its own advance first) — kept as the invariant's authority:
                // a future capture point between blocks must never read a
                // stale mid-block cursor.
                #[allow(unused_assignments)]
                {
                    resume_at = ResumeAt::BlockBoundary { next_block: block_idx + 1 };
                }
                block_idx += 1;
            }

            // Last-page column balancing (§17.6.3).
            // If the final page has unused columns, reflow its blocks with a
            // reduced column height so all columns have approximately equal content.
            // Balancing is skipped for single-column sections and when all columns
            // were used (content fills the page completely).
            let num_cols = geo.columns.len();
            if num_cols > 1 && col_idx < num_cols - 1
                && last_page_start_block < block_caches.len()
            {
                let total_h: f64 = block_caches[last_page_start_block..]
                    .iter()
                    .map(|b| match b {
                        BlockCache::Paragraph(c) => c.lines.iter()
                            .map(|l| c.layout.line_height(
                                l.max_font_pt, l.max_ascent + l.max_descent, l.max_image_height))
                            .sum::<f64>()
                            + c.layout.space_before_px + c.layout.space_after_px,
                        BlockCache::Table(t) => t.rows.iter().map(|r| r.height_px).sum(),
                    })
                    .sum();
                let content_height_now = effective_content_height(&laid_out_hf, geo, section_page_idx);
                let target_col_h = (total_h / num_cols as f64).max(1.0);

                if target_col_h < content_height_now {
                    // Discard the in-progress last page and re-place from last_page_start_block
                    // with target_col_h as the column height cap.
                    current = PageLayout::new();
                    y_in_page = 0.0;
                    col_idx = 0;
                    scope.clear_page();
                    // If this final page is still the section's first page and the
                    // section was seeded from a preceding continuous section, the
                    // carried-in band (e.g. a full-width title) lives in the old
                    // `current.elements` — NOT in `block_caches[last_page_start_block..]`.
                    // Restore it on top of the rebuilt page and push BOTH balanced
                    // columns below it by `balance_band_offset` (the carried band
                    // height), so the title stays full-width on top and the 2
                    // columns balance under it — matching Word (dropped-leading-
                    // section fix). `y_in_page` stays the per-column cursor (0 at the
                    // band top); the offset is added only when computing baselines.
                    // Only when the carried content is still on this page (no
                    // intra-section page advance: last_page_start_block == 0 and
                    // section_page_idx unchanged from seed time).
                    // This guard is the SOLE carried_in read;
                    // capture_checkpoint! byte-mirrors it to decide whether a
                    // checkpoint snapshots carried_in (both inputs are monotone
                    // within a section, so a mid-section None restores safely).
                    // Changing this predicate requires changing the capture.
                    let mut balance_band_offset = 0.0_f64;
                    if last_page_start_block == 0 && section_page_idx == carried_in_spi {
                        if let Some((ref elems, carried_y)) = carried_in {
                            for el in elems {
                                current.push(el.clone());
                            }
                            balance_band_offset = carried_y;
                        }
                    }

                    // Absolute block index into `block_caches`; range-loop kept
                    // (the body indexes and reasons about the absolute position).
                    #[allow(clippy::needless_range_loop)]
                    for b_idx in last_page_start_block..block_caches.len() {
                        let block = &block_caches[b_idx];
                        match block {
                            BlockCache::Paragraph(cache) => {
                                if y_in_page > 0.0 {
                                    let gap = cache.layout.space_before_px;
                                    y_in_page += gap;
                                }
                                for (line_idx, line) in cache.lines.iter().enumerate() {
                                    let line_height = cache.layout.line_height(
                                        line.max_font_pt, line.max_ascent + line.max_descent, line.max_image_height);
                                    if y_in_page + line_height > target_col_h && !current.is_empty()
                                        && col_idx + 1 < num_cols {
                                            col_idx += 1;
                                            y_in_page = 0.0;
                                            scope.clear_column();
                                        }
                                    let col = &geo.columns[col_idx];
                                    let col_ml = col.x_offset_px;
                                    let col_mr = page_w - col.x_offset_px - col.width_px;
                                    let margin_top = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                                    let line_indent_extra = if line_idx == 0 { cache.layout.indent_first_line_px } else { 0.0 };
                                    let is_last_line = line_idx == cache.lines.len() - 1;
                                    let mut items = line.items.clone();
                                    let avail = cache.layout.available_width(page_w, col_ml, col_mr, line_indent_extra);
                                    if cache.layout.alignment == STJc::Both && !is_last_line {
                                        justify_line(&mut items, avail);
                                    }
                                    let x_start = cache.layout.x_start(page_w, col_ml, col_mr, line.width(), line_indent_extra);
                                    let baseline_y = margin_top + balance_band_offset + y_in_page + line.max_ascent;
                                    current.push(PageElement::Line(PagedLine {
                                        segments: PagedLine::single(items, x_start),
                                        para: cache.para_id.unwrap_or(PARA_SENTINEL),
                                        baseline_y,
                                        para_shading: cache.layout.shading_color.clone(),
                                        line_height,
                                        content_left: col_ml + cache.layout.indent_left_px,
                                        content_right: page_w - col_mr - cache.layout.indent_right_px,
                                        border_top: cache.layout.border_top.clone(),
                                        border_bottom: cache.layout.border_bottom.clone(),
                                        border_left: cache.layout.border_left.clone(),
                                        border_right: cache.layout.border_right.clone(),
                                        bar_tabs: cache.tab_stops.bar_xs(
                                            col_ml + cache.layout.indent_left_px,
                                            page_w - col_mr - cache.layout.indent_right_px,
                                            cache.layout.base_rtl),
                                        is_first_line: line_idx == 0,
                                        is_last_line,
                                    }));
                                    y_in_page += line_height;
                                }
                                y_in_page += cache.layout.space_after_px;
                            }
                            BlockCache::Table(table) => {
                                for row in &table.rows {
                                    if y_in_page + row.height_px > target_col_h && !current.is_empty()
                                        && col_idx + 1 < num_cols {
                                            col_idx += 1;
                                            y_in_page = 0.0;
                                            scope.clear_column();
                                        }
                                    let col = &geo.columns[col_idx];
                                    let col_ml = col.x_offset_px;
                                    let col_cw = col.width_px;
                                    let tbl_cm_h = table.cell_margin_h;
                                    let tbl_cm_v = table.cell_margin_v;
                                    let margin_top = effective_margin_top(&laid_out_hf, geo, section_page_idx);
                                    let row_y_top = margin_top + balance_band_offset + y_in_page;
                                    let table_width: f64 = table.col_widths_px.iter().sum();
                                    let table_offset = match table.alignment {
                                        STJcTable::Center => (col_cw - table_width) / 2.0,
                                        STJcTable::End | STJcTable::Right => col_cw - table_width,
                                        _ => table.indent_px,
                                    };
                                    let cell_x_start = col_ml + table_offset;
                                    // Rebuild paged cells at the new column position.
                                    let mut paged_cells = Vec::new();
                                    let mut cell_x = cell_x_start;
                                    for cell in &row.cells {
                                        let mut cell_lines = Vec::new();
                                        let mut cell_y = row_y_top + cell.mar_top;
                                        let mut cell_prev_after = 0.0_f64;
                                        for cb in &cell.blocks {
                                            if let CellBlock::Paragraph(pc) = cb {
                                                cell_y += cell_prev_after.max(pc.layout.space_before_px);
                                                for (li, line) in pc.lines.iter().enumerate() {
                                                    let lh = pc.layout.line_height(line.max_font_pt, line.max_ascent + line.max_descent, line.max_image_height);
                                                    let ie = if li == 0 { pc.layout.indent_first_line_px } else { 0.0 };
                                                    let xs = pc.layout.x_start(cell_x + cell.width_px, cell_x + cell.mar_left, cell.mar_left, line.width(), ie);
                                                    cell_lines.push(PagedLine {
                                                        segments: PagedLine::single(line.items.clone(), xs),
                                                        para: pc.para_id.unwrap_or(PARA_SENTINEL),
                                                        baseline_y: cell_y + line.max_ascent,
                                                        para_shading: None, line_height: lh,
                                                        content_left: xs, content_right: cell_x + cell.width_px - cell.mar_right,
                                                        border_top: None, border_bottom: None, border_left: None, border_right: None,
                                                        bar_tabs: pc.tab_stops.bar_xs(
                                                            cell_x + cell.mar_left + pc.layout.indent_left_px,
                                                            cell_x + cell.width_px - cell.mar_right - pc.layout.indent_right_px,
                                                            pc.layout.base_rtl),
                                                        is_first_line: false, is_last_line: false,
                                                    });
                                                    cell_y += lh;
                                                }
                                                cell_prev_after = pc.layout.space_after_px;
                                            }
                                        }
                                        paged_cells.push(PagedTableCell {
                                            lines: cell_lines, x: cell_x,
                                            width_px: cell.width_px, fill_color: cell.fill_color.clone(),
                                            v_merged_away: cell.v_merged_away,
                                            border_top: cell.border_top.clone(), border_bottom: cell.border_bottom.clone(),
                                            border_left: cell.border_left.clone(), border_right: cell.border_right.clone(),
                                        });
                                        cell_x += cell.width_px;
                                    }
                                    current.push(PageElement::TableRow(PagedTableRow {
                                        cells: paged_cells, y_top: row_y_top, height_px: row.height_px,
                                        border_color: table.border_color.clone(), has_borders: table.has_borders,
                                        cell_margin_h: tbl_cm_h, cell_margin_v: tbl_cm_v,
                                    }));
                                    y_in_page += row.height_px;
                                }
                            }
                        }
                    }
                }
            }

            // Does the NEXT section begin continuously (§17.6.22)? Its `w:type`
            // describes how it starts relative to this one. A continuous start
            // means it shares this page — but only if the page *size* matches
            // (margins may differ; page dimensions physically cannot change
            // mid-sheet, so a size change is promoted to NextPage).
            let next = w.sections.get(sec_idx + 1);
            let next_is_continuous = next.is_some_and(|n| {
                matches!(n.break_type, SectionBreakType::Continuous)
                    && (n.geometry.width_px - geo.width_px).abs() < 0.5
                    && (n.geometry.height_px - geo.height_px).abs() < 0.5
            });

            if next_is_continuous {
                // Carry the in-progress page + cursor into the next section;
                // do NOT flush. The next section's content (often a different
                // column layout) continues below on the same page.
                carry = Some((std::mem::replace(&mut current, PageLayout::new()), y_in_page, section_page_idx));
                // Exits BOTH 'relay and continues the section loop (continuous-
                // carry pages are v1-excluded from the fixpoint — they never seal
                // here). Bare `continue` would target 'relay and hang.
                continue 'sec;
            }

            // End of section — flush current page. MIN-ONE-PAGE: an EMPTY body
            // (corpus: Bug60341.docx — bare sectPr + footerReference, zero
            // paragraphs) must still yield one blank page, as Word renders it
            // (headers/footers included via the normal finish_page path); gate
            // on the LAST section so intermediate empty sections stay unflushed.
            let force_empty_page =
                self.pages.borrow().is_empty() && sec_idx + 1 == w.sections.len();
            if !current.is_empty() || force_empty_page {
                // Past this section's blocks: the next page (if any) is the
                // following section's first, captured at its block-loop top.
                // Dead store today (no capture follows the flush — it pushes a
                // page but starts none); kept so any future consumer at this
                // seam reads a truthful cursor.
                #[allow(unused_assignments)]
                {
                    resume_at = ResumeAt::BlockBoundary { next_block: block_caches.len() };
                }
                seal_or_rollback!('relay);
                finish_page(&mut current, &mut section_page_idx, &mut global_page_idx, &mut y_in_page, &laid_out_hf, geo)?;
                let finished = std::mem::replace(&mut current, PageLayout::new());
                push_page(finished, section_page_idx, sec_idx)?;
                yield_to_browser().await;
                if self.layout_gen.get() != my_gen { return Ok(self.pages.borrow().len() as u32); }
                        // Stop BEFORE the memory wall, at the same page
                        // boundary the doc-switch abort uses. Pages so far stay
                        // valid and visible; the post-loop passes (relocation,
                        // PAGE/NUMPAGES) are skipped exactly as on abort, so a
                        // NUMPAGES field in a truncated view shows its cached
                        // value — declared, not silent.
                        if self.should_truncate_layout() {
                            let n = self.pages.borrow().len() as u32;
                            self.layout_truncated_at.set(Some(n));
                            return Ok(n);
                        }
                global_page_idx += 1;
            }

            // The section's page fixpoint reached its terminal
            // (accepted) flush — leave the relay loop. A pristine run reaches here
            // after one pass; a rollback path never does (it `continue 'relay`s).
            break 'relay;
            } // 'relay: loop

            // Section break handling for next section
            // (nextPage is implicit — we already flushed the page above)
            // evenPage/oddPage: insert blank pages until page parity matches
            match section.break_type {
                SectionBreakType::EvenPage => {
                    while !global_page_idx.is_multiple_of(2) {
                        let mut blank = PageLayout::new();
                        blank.page_number = global_page_idx + 1;
                        push_page(blank, section_page_idx, sec_idx)?;
                        global_page_idx += 1;
                    }
                }
                SectionBreakType::OddPage => {
                    while global_page_idx % 2 != 1 {
                        let mut blank = PageLayout::new();
                        blank.page_number = global_page_idx + 1;
                        push_page(blank, section_page_idx, sec_idx)?;
                        global_page_idx += 1;
                    }
                }
                _ => {}
            }
        }

        // Publish the captured ParaMeta side table for this
        // layout generation (additive; never touches page geometry).
        *self.para_meta.borrow_mut() = para_sink;
        // Publish the table structure side table.
        *self.table_meta.borrow_mut() = table_sink;

        // Place anchored objects on the page their resolved vertical position
        // actually lands on. During assembly an anchored image is pinned to its
        // anchor paragraph's page at `paragraph_top + positionV offset`; a
        // `relativeFrom="paragraph"` object with a large posOffset (or anchored
        // near a page bottom) resolves to a `y` past the page's content area, so
        // it would be drawn off the page and vanish. Word flows such an object
        // onto the page its position falls on, so we move it down by whole
        // content heights until its top sits within a content area. (The wrap
        // exclusion is not reflowed across the break — a known
        // limitation; in the off-page case the exclusion was already beyond the content
        // and inert.)
        //
        // DEVIATION (bounded relocation): an object moves at most as many pages
        // as the document had before this pass, so the pass can at most double
        // the page count. `posOffset` (§20.4.3.3) is document-supplied and parsed
        // as i64; unbounded, a 1 KB file placed one object 10^6 pages down, and a
        // larger value never terminated. An object pushed past the bound keeps
        // the residual `y` and is drawn off its page, as it was before this pass
        // existed.
        {
            let content_top = self.page_size.margin_top_px;
            let content_h = self.page_size.content_height_px();
            let content_bottom = content_top + content_h;
            let mut pages = self.pages.borrow_mut();
            let mut relocations: Vec<(usize, AnchoredImageLayout)> = Vec::new();
            let max_steps = pages.len();
            for p in 0..pages.len() {
                let imgs = std::mem::take(&mut pages[p].anchored_images);
                let mut keep = Vec::with_capacity(imgs.len());
                for mut img in imgs {
                    let steps = wml::page::relocation_steps(img.y, content_bottom, content_h, max_steps);
                    img.y -= steps as f64 * content_h;
                    let target = p + steps;
                    if target == p { keep.push(img); } else { relocations.push((target, img)); }
                }
                pages[p].anchored_images = keep;
            }
            for (target, img) in relocations {
                while target >= pages.len() {
                    let mut blank = PageLayout::new();
                    blank.page_number = pages.len() + 1;
                    pages.push(blank);
                }
                pages[target].anchored_images.push(img);
            }
        }

        // Substitute PAGE / NUMPAGES fields now that the total page count is
        // known. Covers header/footer, body lines, and table cells — complex
        // page fields can appear anywhere, not just in headers.
        let total = self.pages.borrow().len();
        {
            let mut pages = self.pages.borrow_mut();
            for page in pages.iter_mut() {
                let page_num = page.page_number;
                for line in page.header_lines.iter_mut().chain(page.footer_lines.iter_mut()) {
                    substitute_page_fields(line, page_num, total, &self.font_cache);
                }
                // Header/footer TABLE cells can also carry PAGE/NUMPAGES fields.
                for row in page.header_rows.iter_mut().chain(page.footer_rows.iter_mut()) {
                    for cell in &mut row.cells {
                        for line in &mut cell.lines {
                            substitute_page_fields(line, page_num, total, &self.font_cache);
                        }
                    }
                }
                for el in &mut page.elements {
                    match el {
                        PageElement::Line(line) => substitute_page_fields(line, page_num, total, &self.font_cache),
                        PageElement::TableRow(row) => {
                            for cell in &mut row.cells {
                                for line in &mut cell.lines {
                                    substitute_page_fields(line, page_num, total, &self.font_cache);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(total as u32)
    }
}

// ─── DocFragment → JsValue (hand-rolled js_sys) ──────────────────────────────
//
// The crate has no serde dep; every existing read (`hit_test`, `selection_rects`)
// uses the `Object::new()`/`Reflect::set` idiom. The JS property names here ARE
// the cross-boundary contract the TS `DocFragment` types mirror. The Rust
// `DocFragment` is the tested surface; this shim is thin.

fn js_set(obj: &js_sys::Object, key: &str, val: &JsValue) {
    let _ = js_sys::Reflect::set(obj, &JsValue::from_str(key), val);
}

fn js_opt_str(s: &Option<String>) -> JsValue {
    match s {
        Some(v) => JsValue::from_str(v),
        None => JsValue::NULL,
    }
}

fn image_ref_to_js(img: &crate::engine::fragment::ImageRef) -> JsValue {
    let o = js_sys::Object::new();
    js_set(&o, "relId", &JsValue::from_str(&img.rel_id));
    js_set(&o, "wPx", &JsValue::from_f64(img.w_px));
    js_set(&o, "hPx", &JsValue::from_f64(img.h_px));
    o.into()
}

fn frag_run_to_js(r: &crate::engine::fragment::FragRun) -> JsValue {
    let o = js_sys::Object::new();
    js_set(&o, "text", &JsValue::from_str(&r.text));
    js_set(&o, "bold", &JsValue::from_bool(r.bold));
    js_set(&o, "italic", &JsValue::from_bool(r.italic));
    js_set(&o, "underline", &JsValue::from_str(r.underline));
    js_set(&o, "strike", &JsValue::from_bool(r.strike));
    js_set(&o, "doubleStrike", &JsValue::from_bool(r.double_strike));
    js_set(&o, "fontFamily", &JsValue::from_str(&r.font_family));
    js_set(&o, "fontSizePt", &JsValue::from_f64(r.font_size_pt));
    js_set(&o, "color", &JsValue::from_str(&r.color));
    js_set(&o, "highlight", &js_opt_str(&r.highlight));
    js_set(&o, "runShading", &js_opt_str(&r.run_shading));
    js_set(&o, "vertAlign", &JsValue::from_str(r.vert_align));
    js_set(&o, "link", &js_opt_str(&r.link));
    js_set(
        &o,
        "image",
        &r.image.as_ref().map(image_ref_to_js).unwrap_or(JsValue::NULL),
    );
    js_set(&o, "footnoteMark", &js_opt_str(&r.footnote_mark));
    o.into()
}

fn frag_list_to_js(l: &crate::engine::fragment::FragList) -> JsValue {
    let o = js_sys::Object::new();
    js_set(&o, "ordered", &JsValue::from_bool(l.ordered));
    js_set(&o, "numId", &JsValue::from_f64(l.num_id as f64));
    js_set(&o, "level", &JsValue::from_f64(l.level as f64));
    js_set(&o, "value", &JsValue::from_f64(l.value as f64));
    js_set(&o, "fmt", &JsValue::from_str(&l.fmt));
    js_set(&o, "marker", &JsValue::from_str(&l.marker));
    o.into()
}

fn frag_para_to_js(p: &crate::engine::fragment::FragPara) -> JsValue {
    let o = js_sys::Object::new();
    let runs = js_sys::Array::new();
    for r in &p.runs {
        runs.push(&frag_run_to_js(r));
    }
    js_set(&o, "runs", &runs);
    js_set(&o, "alignment", &JsValue::from_str(&p.alignment));
    js_set(&o, "dir", &JsValue::from_str(p.dir));
    js_set(
        &o,
        "list",
        &p.list.as_ref().map(frag_list_to_js).unwrap_or(JsValue::NULL),
    );
    o.into()
}

fn frag_border_to_js(b: &crate::engine::fragment::FragBorder) -> JsValue {
    let o = js_sys::Object::new();
    js_set(&o, "style", &JsValue::from_str(&b.style));
    js_set(&o, "color", &JsValue::from_str(&b.color));
    js_set(&o, "widthPx", &JsValue::from_f64(b.width_px));
    o.into()
}

fn frag_cell_to_js(c: &crate::engine::fragment::FragCell) -> JsValue {
    let o = js_sys::Object::new();
    js_set(&o, "rowspan", &JsValue::from_f64(c.rowspan as f64));
    js_set(&o, "colspan", &JsValue::from_f64(c.colspan as f64));
    js_set(&o, "fill", &js_opt_str(&c.fill));
    let borders = js_sys::Array::new();
    for b in &c.borders {
        borders.push(&b.as_ref().map(frag_border_to_js).unwrap_or(JsValue::NULL));
    }
    js_set(&o, "borders", &borders);
    js_set(&o, "vMergedAway", &JsValue::from_bool(c.v_merged_away));
    let blocks = js_sys::Array::new();
    for b in &c.blocks {
        blocks.push(&block_to_js(b));
    }
    js_set(&o, "blocks", &blocks);
    o.into()
}

fn frag_table_to_js(t: &crate::engine::fragment::FragTable) -> JsValue {
    let o = js_sys::Object::new();
    let rows = js_sys::Array::new();
    for row in &t.rows {
        let ro = js_sys::Object::new();
        let cells = js_sys::Array::new();
        for c in &row.cells {
            cells.push(&frag_cell_to_js(c));
        }
        js_set(&ro, "cells", &cells);
        rows.push(&ro);
    }
    js_set(&o, "rows", &rows);
    o.into()
}

fn block_to_js(b: &crate::engine::fragment::Block) -> JsValue {
    use crate::engine::fragment::Block;
    let o = js_sys::Object::new();
    match b {
        Block::Para(p) => {
            js_set(&o, "kind", &JsValue::from_str("para"));
            js_set(&o, "para", &frag_para_to_js(p));
        }
        Block::Table(t) => {
            js_set(&o, "kind", &JsValue::from_str("table"));
            js_set(&o, "table", &frag_table_to_js(t));
        }
    }
    o.into()
}

fn frag_to_jsvalue(frag: &crate::engine::fragment::DocFragment) -> JsValue {
    let o = js_sys::Object::new();
    let blocks = js_sys::Array::new();
    for b in &frag.blocks {
        blocks.push(&block_to_js(b));
    }
    js_set(&o, "blocks", &blocks);
    o.into()
}

/// Serialize a [`sml::select::CellFragment`] to the JS shape
/// the TS `docExtractRange` cells arm + the SML flavor serializers consume:
/// `{ rows: [{ cells: [{ display, rowspan, colspan, covered }] }], rowCount,
/// totalRows, truncated, reason }`.
#[cfg(feature = "sml")]
fn cell_fragment_to_jsvalue(frag: &sml::select::CellFragment) -> JsValue {
    let o = js_sys::Object::new();
    let rows = js_sys::Array::new();
    for row in &frag.rows {
        let cells = js_sys::Array::new();
        for c in row {
            let cell = js_sys::Object::new();
            js_set(&cell, "display", &JsValue::from_str(&c.display));
            js_set(&cell, "rowspan", &JsValue::from_f64(c.rowspan as f64));
            js_set(&cell, "colspan", &JsValue::from_f64(c.colspan as f64));
            js_set(&cell, "covered", &JsValue::from_bool(c.covered));
            cells.push(&cell);
        }
        let row_obj = js_sys::Object::new();
        js_set(&row_obj, "cells", &cells);
        rows.push(&row_obj);
    }
    js_set(&o, "rows", &rows);
    js_set(&o, "rowCount", &JsValue::from_f64(frag.row_count as f64));
    js_set(&o, "totalRows", &JsValue::from_f64(frag.total_rows as f64));
    js_set(&o, "truncated", &JsValue::from_bool(frag.truncated));
    js_set(&o, "reason", &frag.reason.map(JsValue::from_str).unwrap_or(JsValue::NULL));
    o.into()
}

#[wasm_bindgen]
impl Viewer {
    /// The chart COPY MODEL for a `chart::<rId>` key:
    /// `{ categories: [...], series: [{name, values: [...]}], wPx, hPx }`, parsed
    /// from the stored chart XML via the SHARED painter pipeline
    /// (`chart::copy_data` — the TSV numbers are always the painted numbers; NO
    /// new XML parsing). `wPx`/`hPx` are the chart's DISPLAY px (the first
    /// painted placement's object box via `object_outline_rects`), the basis for
    /// the 4× raster and the `<img width/height>` the html flavor emits; 0 when
    /// the chart has no painted placement. For an SML doc the placement is the
    /// chart's grid anchor box (`chart::sml::…` keys). Structured-clone-safe
    /// (plain object graph) so the worker dispatcher ships it directly.
    /// `JsValue::NULL` for an unknown key / parse failure / empty series. A NEW
    /// READ — layout/render byte-neutral. SYNC.
    #[wasm_bindgen(js_name = chartData)]
    pub fn chart_data(&self, rel_key: String) -> JsValue {
        // Doc-kind-agnostic MAP lookup (the chart registry is shared across
        // WML/SML/PML — keys are format-namespaced, cleared on doc switch).
        let charts = self.chart_xml.borrow();
        let Some(xml) = charts.get(&rel_key) else { return JsValue::NULL };
        let Some(data) = crate::chart::copy_data(xml) else { return JsValue::NULL };
        // Display px of the first painted placement: WML reads the page-model
        // outline rects; SML reads the chart's content-space anchor box (the
        // grid's painted size at zoom 1). Placement-less keys stay 0×0.
        let (mut w_px, mut h_px) = (0.0f64, 0.0f64);
        if matches!(self.loaded, LoadedDoc::Wml(_)) {
            let pages = self.pages.borrow();
            for pg in pages.iter() {
                if let Some(r) = crate::wml::select::object_outline_rects(pg, &rel_key).first() {
                    w_px = r.w;
                    h_px = r.h;
                    break;
                }
            }
        }
        #[cfg(feature = "sml")]
        if let LoadedDoc::Sml(s) = &self.loaded {
            if let Some((w, h)) = s.chart_display_px(&rel_key, &self.font_cache) {
                w_px = w;
                h_px = h;
            }
        }
        let obj = js_sys::Object::new();
        let cats = js_sys::Array::new();
        for c in &data.categories {
            cats.push(&JsValue::from_str(c));
        }
        let _ = js_sys::Reflect::set(&obj, &"categories".into(), &cats);
        let series = js_sys::Array::new();
        for (name, values) in &data.series {
            let s = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&s, &"name".into(), &JsValue::from_str(name));
            let vals = js_sys::Array::new();
            for &v in values {
                vals.push(&JsValue::from_f64(v));
            }
            let _ = js_sys::Reflect::set(&s, &"values".into(), &vals);
            series.push(&s);
        }
        let _ = js_sys::Reflect::set(&obj, &"series".into(), &series);
        let _ = js_sys::Reflect::set(&obj, &"wPx".into(), &JsValue::from_f64(w_px));
        let _ = js_sys::Reflect::set(&obj, &"hPx".into(), &JsValue::from_f64(h_px));
        obj.into()
    }

    /// The chart RASTER seam: paint chart `rel_key` into a
    /// TS-provided 2D context at `(w_px, h_px)` CANVAS px with a uniform `scale`
    /// (the 4× print-grade raster: canvas = 4 × display px, scale = 4, so the
    /// vector painter's fixed margins/fonts scale WITH the chart instead of
    /// shrinking relatively). Fills the canvas white first (Word charts sit on a
    /// white page; a transparent PNG pastes poorly into dark-themed editors),
    /// then drives the SHARED `crate::chart::render_chart` through the SAME
    /// `CanvasPainter` the in-process page paint uses. `ctx` is typed
    /// `CanvasRenderingContext2d` but an `OffscreenCanvasRenderingContext2D`
    /// works identically (duck-typed method surface — the established web-sys
    /// pattern). Doc-kind-agnostic (WML `chart::rIdN` and SML `chart::sml::…`
    /// keys raster identically). Returns `false` (clean skip) for an
    /// unknown key / non-positive geometry / unsupported chart. Draws ONLY on
    /// the provided context — the page render stays byte-identical.
    #[wasm_bindgen(js_name = renderChartToCanvas)]
    // The `!(x > 0.0)` guards below are DELIBERATE NaN rejection: `x <= 0.0`
    // would admit NaN (all NaN comparisons are false), silently rasterizing a
    // degenerate chart. Do not "simplify" to a partial_cmp form.
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    pub fn render_chart_to_canvas(
        &self,
        rel_key: String,
        ctx: &CanvasRenderingContext2d,
        w_px: f64,
        h_px: f64,
        scale: f64,
    ) -> bool {
        // Doc-kind-agnostic (the map lookup below is the only doc read — an
        // SML chart rasterizes through the SAME seam as a WML one).
        if !(w_px > 0.0) || !(h_px > 0.0) || !(scale > 0.0) {
            return false;
        }
        let charts = self.chart_xml.borrow();
        let Some(xml) = charts.get(&rel_key) else { return false };
        let images = self.image_bitmaps.borrow();
        let painter = crate::engine::paint::CanvasPainter::new(ctx, &images);
        ctx.save();
        let _ = ctx.scale(scale, scale);
        // White ground under the vectors (see doc comment).
        ctx.set_fill_style_str("#ffffff");
        ctx.fill_rect(0.0, 0.0, w_px / scale, h_px / scale);
        let ok = crate::chart::render_chart(xml, &painter, 0.0, 0.0, w_px / scale, h_px / scale);
        ctx.restore();
        ok
    }

    /// The EMF RASTER seam (mirrors `renderChartToCanvas`): paint the cached
    /// EMF drawing program for `rel_key` (the synthetic `"emf-pic:{rel}"` the
    /// page display list emits — a bare rel is tolerated) into a TS-provided 2D
    /// context at `(w_px, h_px)` canvas px with a uniform `scale`. Resolves from
    /// the PERSISTENT `emf_pics` map (never the per-page-paint `emf::set_active`
    /// registry, which may be stale / pointed at another page when this async
    /// raster runs). DELIBERATE deviation from the chart hook: NO white ground —
    /// the pre-cache inline paint composited transparently over the page, and
    /// the cached bitmap must preserve that (a white fill would opaque-out
    /// anything behind a behind-text EMF). `ctx` is typed
    /// `CanvasRenderingContext2d` but an `OffscreenCanvasRenderingContext2D`
    /// works identically (duck-typed method surface — the established web-sys
    /// pattern). Resolution stays TS-side: this hook paints whatever pixel size
    /// the adapter computed; no dpr/zoom enters the engine (standing boundary
    /// rule). Returns `false` (clean skip) for an unknown key / non-positive
    /// geometry — the caller must NOT register the (fully transparent) canvas.
    #[cfg(feature = "emf")]
    #[wasm_bindgen(js_name = renderEmfToCanvas)]
    // `!(x > 0.0)` is DELIBERATE NaN rejection (see renderChartToCanvas).
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    pub fn render_emf_to_canvas(
        &self,
        rel_key: String,
        ctx: &CanvasRenderingContext2d,
        w_px: f64,
        h_px: f64,
        scale: f64,
    ) -> bool {
        if !(w_px > 0.0) || !(h_px > 0.0) || !(scale > 0.0) {
            return false;
        }
        let rel = rel_key.strip_prefix("emf-pic:").unwrap_or(&rel_key);
        let pics = self.emf_pics.borrow();
        let Some(pic) = pics.get(rel) else { return false };
        // The painter resolves the INTERNAL DIB blit keys `emf:{rel}:{n}` from
        // the same bitmap map the page paint uses; a not-yet-decoded blit draws
        // the grey placeholder (the caller re-rasters when it arrives).
        let images = self.image_bitmaps.borrow();
        let painter = crate::engine::paint::CanvasPainter::new(ctx, &images);
        ctx.save();
        let _ = ctx.scale(scale, scale);
        let ok = emf::render::paint(pic, &painter, 0.0, 0.0, w_px / scale, h_px / scale).is_ok();
        ctx.restore();
        ok
    }

    /// The decoded DIB blit refs for one EMF picture, `[{ relId:
    /// "emf:{rel}:{n}", wPx, hPx, bytes }]` (the raw-RGBA shape `imageRefs`
    /// emits) — used by the WORKER to lazily decode blits into its own wasm
    /// bitmap map before rasterizing. Reads the persistent `emf_pics` (NOT
    /// `self.images`, which `clearImageBytes` empties). Accepts the synthetic
    /// `"emf-pic:{rel}"` key or a bare rel; unknown ⇒ empty array.
    #[cfg(feature = "emf")]
    #[wasm_bindgen(js_name = emfBlitRefs)]
    pub fn emf_blit_refs(&self, rel_key: String) -> JsValue {
        let rel = rel_key.strip_prefix("emf-pic:").unwrap_or(&rel_key);
        let array = js_sys::Array::new();
        if let Some(pic) = self.emf_pics.borrow().get(rel) {
            for (i, blit) in pic.blits.iter().enumerate() {
                let obj = js_sys::Object::new();
                let _ = js_sys::Reflect::set(
                    &obj,
                    &JsValue::from_str("relId"),
                    &JsValue::from_str(&format!("emf:{rel}:{i}")),
                );
                let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("wPx"), &JsValue::from_f64(blit.w as f64));
                let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("hPx"), &JsValue::from_f64(blit.h as f64));
                let bytes = js_sys::Uint8Array::new_with_length(blit.rgba.len() as u32);
                bytes.copy_from(&blit.rgba);
                let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("bytes"), &bytes);
                array.push(&obj);
            }
        }
        array.into()
    }

    /// Drain the per-paint EMF paint-request log: `[{ key: "emf-pic:{rel}",
    /// dw, dh }]` with `dw`/`dh` in PAGE-LOGICAL px (the TS drain multiplies by
    /// zoom·dpr exactly once — resolution policy never enters the engine). The
    /// in-process Viewer drains after each synchronous page paint; worker mode
    /// never calls this (replay-side draw detection instead), and
    /// `emf::set_active` clears the log each page paint so it stays bounded.
    #[cfg(feature = "emf")]
    #[wasm_bindgen(js_name = takeEmfPaintRequests)]
    pub fn take_emf_paint_requests(&self) -> JsValue {
        let array = js_sys::Array::new();
        for (key, dw, dh) in emf::take_requests() {
            let obj = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("key"), &JsValue::from_str(&key));
            let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("dw"), &JsValue::from_f64(dw));
            let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("dh"), &JsValue::from_f64(dh));
            array.push(&obj);
        }
        array.into()
    }

    /// Formula-bar readout (spreadsheets): hit-test a page coordinate to its
    /// cell, returning `{ref, raw, display}` or null. Page coordinates are the
    /// same space `renderPage` paints (canvas logical px).
    #[cfg(feature = "sml")]
    #[wasm_bindgen(js_name = cellAt)]
    pub fn cell_at(&self, page: u32, x: f64, y: f64) -> JsValue {
        if let LoadedDoc::Sml(s) = &self.loaded {
            let (cx, cy) = (x - self.page_size.margin_left_px, y - self.page_size.margin_top_px);
            s.ensure_page(page as usize, &self.sml_pages); // fill before the hit-test
            if let Some((r, raw, display)) =
                sml::SmlRenderer::cell_at(&self.sml_pages, page as usize, cx, cy)
            {
                let obj = js_sys::Object::new();
                let _ = js_sys::Reflect::set(&obj, &"ref".into(), &r.into());
                let _ = js_sys::Reflect::set(&obj, &"raw".into(), &raw.into());
                let _ = js_sys::Reflect::set(&obj, &"display".into(), &display.into());
                return obj.into();
            }
        }
        JsValue::NULL
    }

    /// Per-sheet → page mapping for the viewer's sheet-tab strip.
    /// Returns `[{name, firstPage, pageCount}]` by run-length grouping the
    /// streamed `sml_pages` on `sheet_name`. The pages already stream in
    /// document order, sheets back-to-back (no interleaving — see
    /// `SmlRenderer::paginate_all`), so consecutive-run grouping is exact and
    /// needs no extra storage. Empty sheets produce zero pages and are omitted
    /// (a UI that wants disabled tabs lists ALL workbook sheets from a separate
    /// source; v1 derives tabs from this mapping alone).
    ///
    /// This keeps the wasm contract intact: pages still stream in document
    /// order; the UI filters the page list client-side on tab switch
    /// (no re-layout). Reads the shared `sml_pages` directly (the renderer holds
    /// no page back-reference — shared state), so the grouping lives
    /// here in the facade, not in `SmlRenderer`.
    #[cfg(feature = "sml")]
    #[wasm_bindgen(js_name = sheetInfo)]
    pub fn sheet_info(&self) -> JsValue {
        let arr = js_sys::Array::new();
        if let LoadedDoc::Sml(s) = &self.loaded {
            let pages = self.sml_pages.borrow();
            for span in sml::grid::sheet_spans(&pages) {
                let obj = js_sys::Object::new();
                let _ = js_sys::Reflect::set(&obj, &"name".into(), &span.name.into());
                let _ = js_sys::Reflect::set(
                    &obj,
                    &"firstPage".into(),
                    &JsValue::from_f64(span.first_page as f64),
                );
                let _ = js_sys::Reflect::set(
                    &obj,
                    &"pageCount".into(),
                    &JsValue::from_f64(span.page_count as f64),
                );
                // maxRow: the sheet's last row, so the TS plugin can build row
                // windows to chunk a huge sheet across ticks. Sourced from geom via
                // first_page → sheet index; once per sheet (sheets are few).
                let max_row = s
                    .sheet_of_flat_page(span.first_page)
                    .and_then(|si| s.ensure_geom(si, &self.font_cache))
                    .map(|gs| gs.geom.max_row)
                    .unwrap_or(0);
                let _ =
                    js_sys::Reflect::set(&obj, &"maxRow".into(), &JsValue::from_f64(max_row as f64));
                arr.push(&obj);
            }
        }
        arr.into()
    }

    /// Continuous grid view (virtual scroller): full extent of a sheet's grid
    /// including gutters — the scroll container's spacer size.
    #[cfg(feature = "sml")]
    #[wasm_bindgen(js_name = gridExtent)]
    pub fn grid_extent(&self, sheet: u32) -> JsValue {
        if let LoadedDoc::Sml(s) = &self.loaded {
            if let Some((w, h)) = s.grid_extent(sheet as usize, &self.font_cache) {
                let obj = js_sys::Object::new();
                let _ = js_sys::Reflect::set(&obj, &"width".into(), &w.into());
                let _ = js_sys::Reflect::set(&obj, &"height".into(), &h.into());
                return obj.into();
            }
        }
        JsValue::NULL
    }

    /// Paint one scroll-window of a sheet's continuous grid onto a viewport
    /// canvas (per scroll frame; O(visible rows); images via the shared map).
    /// `zoom` (zoom-in-grid): `scroll_x`/`scroll_y` are CONTENT-space px and
    /// `view_w`/`view_h` CSS px; the engine applies one uniform ctx scale. NB:
    /// edits in this file must be LINE-NEUTRAL — docx-path `panic::Location`
    /// line numbers below shift otherwise, breaking wasm byte-identity.
    #[cfg(feature = "sml")]
    #[wasm_bindgen(js_name = paintGridViewport)]
    #[allow(clippy::too_many_arguments)]
    pub fn paint_grid_viewport(
        &self, sheet: u32, canvas: HtmlCanvasElement, scroll_x: f64,
        scroll_y: f64, view_w: f64, view_h: f64, zoom: f64,
    ) -> Result<(), JsValue> {
        if let LoadedDoc::Sml(s) = &self.loaded {
            let ctx = canvas
                .get_context("2d")?
                .ok_or_else(|| JsValue::from_str("no 2d context"))?
                .dyn_into::<CanvasRenderingContext2d>()?;
            let sq = self.search_query.borrow();
            let active = self.sml_active_cell_for_sheet(sheet as usize);
            let sh = self.sml_search_highlight(&sq, active.as_deref());
            // The stored cell selection, but only when it
            // belongs to the sheet being painted (grid view is one sheet).
            let cell_sel = self.cell_selection.get().filter(|c| c.sheet == sheet);
            // Selected chart-object outline (keys are sheet-namespaced, so a
            // selection never leaks onto another sheet's paint).
            let sel_obj = self.selected_object.borrow();
            return s.paint_grid_viewport(
                sheet as usize, &ctx, scroll_x, scroll_y, view_w, view_h, zoom, &self.font_cache, &self.image_bitmaps,
                &self.chart_xml, sel_obj.as_deref(), sh.as_ref(), cell_sel,
            );
        }
        Ok(())
    }

    /// Formula-bar hit-test in the continuous grid view.
    #[cfg(feature = "sml")]
    #[wasm_bindgen(js_name = gridCellAt)]
    pub fn grid_cell_at(&self, sheet: u32, x: f64, y: f64, scroll_x: f64, scroll_y: f64) -> JsValue {
        if let LoadedDoc::Sml(s) = &self.loaded {
            if let Some((r, raw, display)) =
                s.grid_cell_at(sheet as usize, x, y, scroll_x, scroll_y, &self.font_cache)
            {
                let obj = js_sys::Object::new();
                let _ = js_sys::Reflect::set(&obj, &"ref".into(), &r.into());
                let _ = js_sys::Reflect::set(&obj, &"raw".into(), &raw.into());
                let _ = js_sys::Reflect::set(&obj, &"display".into(), &display.into());
                return obj.into();
            }
        }
        JsValue::NULL
    }

    /// The USED-EXTENT box a header click selects,
    /// keyed off `content_bounds`. Returns `{ r0, c0, r1, c1 }` (1-based
    /// inclusive) or `null`. The TS grid adapter builds a whole-row/col selection
    /// from this on a header click. A NEW READ — byte-neutral. SYNC.
    #[cfg(feature = "sml")]
    #[wasm_bindgen(js_name = gridContentBounds)]
    pub fn grid_content_bounds(&self, sheet: u32) -> JsValue {
        if let LoadedDoc::Sml(s) = &self.loaded {
            if let Some((r0, c0, r1, c1)) = s.content_bounds(sheet as usize) {
                let obj = js_sys::Object::new();
                for (k, v) in [("r0", r0), ("c0", c0), ("r1", r1), ("c1", c1)] {
                    let _ = js_sys::Reflect::set(&obj, &k.into(), &JsValue::from_f64(v as f64));
                }
                return obj.into();
            }
        }
        JsValue::NULL
    }

    /// Frozen panes (§18.3.1.66, grid view): a sheet's pane info for the TS grid
    /// adapter — `{ xSplit, ySplit, bandW, bandH, scrollX, scrollY }` (band sizes
    /// in CONTENT px; `scrollX`/`scrollY` the `topLeftCell`-derived initial BODY
    /// scroll, 0 when absent → A1) or `null` when the sheet has no frozen pane
    /// (or `state="split"`, ignored by design). A pure READ — byte-neutral. SYNC.
    #[cfg(feature = "sml")]
    #[wasm_bindgen(js_name = gridPaneInfo)]
    pub fn grid_pane_info(&self, sheet: u32) -> JsValue {
        if let LoadedDoc::Sml(s) = &self.loaded {
            if let Some((info, bands, sx, sy)) =
                s.grid_pane_info(sheet as usize, &self.font_cache)
            {
                let obj = js_sys::Object::new();
                let _ = js_sys::Reflect::set(&obj, &"xSplit".into(), &JsValue::from_f64(info.x_split as f64));
                let _ = js_sys::Reflect::set(&obj, &"ySplit".into(), &JsValue::from_f64(info.y_split as f64));
                let _ = js_sys::Reflect::set(&obj, &"bandW".into(), &JsValue::from_f64(bands.band_w));
                let _ = js_sys::Reflect::set(&obj, &"bandH".into(), &JsValue::from_f64(bands.band_h));
                let _ = js_sys::Reflect::set(&obj, &"scrollX".into(), &JsValue::from_f64(sx));
                let _ = js_sys::Reflect::set(&obj, &"scrollY".into(), &JsValue::from_f64(sy));
                return obj.into();
            }
        }
        JsValue::NULL
    }

    #[wasm_bindgen(js_name = pageCount)]
    pub fn page_count(&self) -> u32 {
        // Per-format dispatch: the SML pages live in the parallel `sml_pages`
        // vec (the WML `pages` stays empty for an xlsx).
        #[cfg(feature = "sml")]
        if let LoadedDoc::Sml(_) = &self.loaded {
            return self.sml_pages.borrow().len() as u32;
        }
        #[cfg(feature = "pml")]
        if let LoadedDoc::Pml(_) = &self.loaded {
            return self.pml_pages.borrow().len() as u32;
        }
        self.pages.borrow().len() as u32
    }

    /// Return `{ widthPx, heightPx }` for page `idx`. Per-page: resolves the
    /// page's section geometry (§17.6), falling back to the shared `page_size`
    /// (SML/PML, or an out-of-range index) — so a mixed-size multi-section WML
    /// doc reports each section's own dimensions, while a doc whose pages all
    /// share a size returns identical numbers for every idx.
    #[wasm_bindgen(js_name = pageSize)]
    pub fn page_size_js(&self, idx: u32) -> JsValue {
        let geo = self.page_geometry_at(idx as usize);
        let obj = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("widthPx"),
            &JsValue::from_f64(geo.width_px),
        );
        let _ = js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("heightPx"),
            &JsValue::from_f64(geo.height_px),
        );
        obj.into()
    }

    /// Draw page `idx` into `canvas`. The canvas's CSS size is assumed to
    /// match the page size; the consumer is responsible for sizing the
    /// backing store (CSS × devicePixelRatio) and applying `ctx.scale(DPR)`
    /// before calling.
    #[wasm_bindgen(js_name = renderPage)]
    pub fn render_page(&self, idx: u32, canvas: HtmlCanvasElement) -> Result<(), JsValue> {
        // Per-format dispatch: SML paints its own grid page (gridlines + cell
        // text via engine::paint/engine::fonts) and returns early; the WML
        // painting prologue/tail below is byte-identical for the docx path.
        #[cfg(feature = "sml")]
        if let LoadedDoc::Sml(s) = &self.loaded {
            let ctx = canvas
                .get_context("2d")?
                .ok_or_else(|| JsValue::from_str("no 2d context"))?
                .dyn_into::<CanvasRenderingContext2d>()?;
            // Pin the grid below the page margins (the SML print-page reuses the
            // shared us-letter geometry for its size; margins give breathing room).
            let sq = self.search_query.borrow();
            let active = self.sml_active_cell_for(idx as usize);
            let sh = self.sml_search_highlight(&sq, active.as_deref());
            return s.render_page(
                idx as usize,
                &self.sml_pages,
                self.page_size.margin_left_px,
                self.page_size.margin_top_px,
                self.page_size.width_px,
                self.page_size.height_px,
                &self.font_cache, &self.image_bitmaps,
                &self.chart_xml,
                &ctx,
                sh.as_ref(),
            );
        }
        // PML: paints its own slide page (background, then the positioned
        // shape/text list) and returns early.
        #[cfg(feature = "pml")]
        if let LoadedDoc::Pml(p) = &self.loaded {
            let ctx = canvas
                .get_context("2d")?
                .ok_or_else(|| JsValue::from_str("no 2d context"))?
                .dyn_into::<CanvasRenderingContext2d>()?;
            let sq = self.search_query.borrow();
            let sh = self.pml_search_highlight(&sq, idx as usize);
            let sel = self.pml_selection.get();
            let obj = self.pml_selected_object.borrow();
            // Build the real-canvas backend (owns the bitmap cache for `draw_image`
            // key resolution + the pattFill tile cache); the slide body is
            // backend-agnostic (`&dyn Painter`), mirroring `render_wml_page`.
            let images = self.image_bitmaps.borrow();
            let painter = crate::engine::paint::CanvasPainter::new(&ctx, &images);
            return p.render_page(
                idx as usize,
                &self.pml_pages,
                &self.font_cache,
                &self.chart_xml,
                &painter,
                sh.as_ref(),
                sel.as_ref(),
                obj.as_ref(),
            );
        }

        // WML (docx) path: the full-page paint is the windowed paint with NO
        // window (`clip = None`) — every `cull::keep` returns `true`, so the
        // draw sequence is byte-identical to the pre-windowing code. The shared body
        // lives in `render_wml_page`; `render_page` (this) and
        // `render_page_window` (appended at the end of the impl) are its two
        // callers, differing ONLY in the `clip` argument.
        let ctx = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("no 2d context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;
        self.render_wml_page(idx, &ctx, None)
    }

    /// Thin canvas wrapper over `paint_wml_page`: builds the real-canvas backend
    /// (`CanvasPainter`, which owns the bitmap cache for `draw_image` key
    /// resolution) and runs the shared page paint through it. This is the ONLY
    /// site that touches the concrete `ctx` / `image_bitmaps`; the page body
    /// itself is backend-agnostic (`&dyn Painter`). `render_page` and
    /// `render_page_window` are its two callers, differing only in `clip`.
    fn render_wml_page(
        &self,
        idx: u32,
        ctx: &CanvasRenderingContext2d,
        clip: Option<&cull::Rect>,
    ) -> Result<(), JsValue> {
        let images = self.image_bitmaps.borrow();
        let painter = crate::engine::paint::CanvasPainter::new(ctx, &images);
        // Windowed paints render in WINDOW-LOCAL coordinates (offset = window
        // origin) so `device = s·(logical − off)` is bounded by the slice — the
        // decorator wraps the canvas painter ONLY when `clip = Some(win)`. The
        // full-page (`clip == None`) path passes the bare `CanvasPainter` → the
        // draw stream is byte-identical to the pre-window code.
        match clip {
            Some(win) => {
                let off = crate::engine::paint::OffsetPainter::new(&painter, win.x0, win.y0);
                self.paint_wml_page(idx, &off, clip)
            }
            None => self.paint_wml_page(idx, &painter, clip),
        }
    }


    /// PML record arm, shared by the full-page and windowed display-list
    /// exports: resolves the LIVE search/selection/object state and paints slide
    /// `idx` through `render_page_clipped` onto `painter`. ONE spelling of the
    /// state-threading rule — the previous two inline copies both passed `None`
    /// for all three, which shipped as "PML search: right counts, no visible
    /// highlights" in worker mode. Native-testable (no JsValue construction on
    /// the success path).
    #[cfg(feature = "pml")]
    fn record_pml_page(
        &self,
        idx: u32,
        painter: &dyn crate::engine::paint::Painter,
        win: Option<&cull::Rect>,
    ) -> Result<bool, JsValue> {
        let LoadedDoc::Pml(p) = &self.loaded else { return Ok(false) };
        let sq = self.search_query.borrow();
        let sh = self.pml_search_highlight(&sq, idx as usize);
        let sel = self.pml_selection.get();
        let obj = self.pml_selected_object.borrow();
        p.render_page_clipped(
            idx as usize,
            &self.pml_pages,
            &self.font_cache,
            &self.chart_xml,
            painter,
            win,
            sh.as_ref(),
            sel.as_ref(),
            obj.as_ref(),
        )?;
        Ok(true)
    }

    /// Record WML page `idx` as a serialized display list for off-thread replay:
    /// build a `RecordingPainter`, run the SAME page paint as `render_page`, and
    /// return the flat wire form `{ opcodes: Float64Array, strings: Array<string>
    /// }`. The main thread (or worker→main replay) reconstructs the page from
    /// this. WML-only for now — SML/PML display-list export is a follow-on.
    #[wasm_bindgen(js_name = renderPageDisplayList)]
    pub fn render_page_display_list(&self, idx: u32) -> Result<JsValue, JsValue> {
        // SML print-page display-list export is a follow-on (grid: `record_grid`).
        #[cfg(feature = "sml")]
        if matches!(self.loaded, LoadedDoc::Sml(_)) {
            return Err(JsValue::from_str("display-list export is WML/PML-only"));
        }
        let rec = crate::engine::displaylist::RecordingPainter::new();
        // PML: record the slide through the SAME `render_page_clipped` the
        // live canvas path uses — clip = None — so the off-thread render is the
        // real slide (gradients/patterns/dashes recorded for real via the PML
        // style opcodes, not solid approximations). Search/selection/object state
        // is threaded EXACTLY as the in-process arm does: an earlier "no
        // search/selection/object" floor here became a bug once
        // PML search landed — worker-recorded slides returned correct match
        // counts with NO visible highlight rects, while docx (whose shared WML
        // body reads this state internally) painted fine through the identical
        // consumer pipeline. The worker holds `pml_pages` + `chart_xml`; the
        // main thread owns the bitmap cache at replay time. WML uses the shared
        // page body. Both drive the SAME RecordingPainter; the pack below is
        // format-neutral.
        #[cfg(feature = "pml")]
        {
            if !self.record_pml_page(idx, &rec, None)? {
                self.paint_wml_page(idx, &rec, None)?;
            }
        }
        #[cfg(not(feature = "pml"))]
        self.paint_wml_page(idx, &rec, None)?;
        Self::pack_display_list(rec.take())
    }

    /// Pack a recorded display list into the JS wire object `{ opcodes:
    /// Float64Array, strings: Array<string> }` — the shared tail of every
    /// `record*` export (`renderPageDisplayList` +
    /// `renderPageWindowDisplayList`). The worker's `collectTransferables`
    /// duck-types `{ opcodes: Float64Array }` and transfers the buffer zero-copy.
    fn pack_display_list(list: crate::engine::displaylist::DisplayList) -> Result<JsValue, JsValue> {
        let (opcodes, strings) = crate::engine::displaylist::encode_display_list(&list);
        let f = js_sys::Float64Array::new_with_length(opcodes.len() as u32);
        f.copy_from(&opcodes);
        let arr = js_sys::Array::new();
        for s in &strings {
            arr.push(&JsValue::from_str(s));
        }
        let out = js_sys::Object::new();
        js_sys::Reflect::set(&out, &JsValue::from_str("opcodes"), &f)?;
        js_sys::Reflect::set(&out, &JsValue::from_str("strings"), &arr)?;
        Ok(out.into())
    }

    /// WINDOWED display-list record (worker-mode `renderPageWindow`): run the SAME
    /// windowed page paint the in-process canvas path runs — `paint_wml_page` /
    /// `render_page_clipped` with `clip = Some(win)` — through an
    /// [`crate::engine::paint::OffsetPainter`] wrapping a `RecordingPainter`, so
    /// the recorded ops are culled + WINDOW-LOCAL + background-reduced, IDENTICAL
    /// to the in-process `render_page_window`. The main thread replays SCALE-ONLY
    /// (no giant CTM translate). This is the second half of the window-local fix:
    /// in worker mode `#localPaint` is null, so the in-process arm never runs and
    /// this record path is the ONLY thing that renders a docx/pptx slice.
    ///
    /// WML+PML only (they draw through a `&dyn Painter`); SML's windowed grid has
    /// its own recorder (`record_grid`), and the SML print-page display-list export
    /// remains a follow-on.
    #[allow(clippy::too_many_arguments)]
    #[wasm_bindgen(js_name = renderPageWindowDisplayList)]
    pub fn render_page_window_display_list(
        &self,
        idx: u32,
        clip_x: f64,
        clip_y: f64,
        view_w: f64,
        view_h: f64,
        zoom: f64,
    ) -> Result<JsValue, JsValue> {
        self.record_page_window(idx, clip_x, clip_y, view_w, view_h, zoom)
    }

    /// Native core of `renderPageWindowDisplayList` (the `#[wasm_bindgen]` export
    /// returns `JsValue`, so the record logic is factored here for native
    /// testability + to share the WML/PML dispatch under one cfg).
    fn record_page_window(
        &self,
        idx: u32,
        clip_x: f64,
        clip_y: f64,
        view_w: f64,
        view_h: f64,
        zoom: f64,
    ) -> Result<JsValue, JsValue> {
        #[cfg(feature = "sml")]
        if matches!(self.loaded, LoadedDoc::Sml(_)) {
            return Err(JsValue::from_str("windowed display-list export is WML/PML-only"));
        }
        let z = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
        let win = cull::Rect::from_viewport(clip_x, clip_y, view_w, view_h, z);
        let rec = crate::engine::displaylist::RecordingPainter::new();
        // Scope the decorator so its borrow of `rec` ends before `rec.take()`.
        {
            let off = crate::engine::paint::OffsetPainter::new(&rec, win.x0, win.y0);
            #[cfg(feature = "pml")]
            {
                if !self.record_pml_page(idx, &off, Some(&win))? {
                    self.paint_wml_page(idx, &off, Some(&win))?;
                }
            }
            #[cfg(not(feature = "pml"))]
            self.paint_wml_page(idx, &off, Some(&win))?;
        }
        Self::pack_display_list(rec.take())
    }

    /// The whole WML page body, painted through an injected `p: &dyn Painter`
    /// (canvas backend for live render, recording backend for the display-list
    /// export). Threaded by `clip`: `None` = full page (byte-identical to the
    /// original `render_page` body), `Some(win)` = windowed — each positioned
    /// item is culled by its page-logical-px bbox ∩ `win` (`cull::keep`), so the
    /// item walk is O(visible). O(1) full-page chrome paints unconditionally.
    /// References NO concrete `ctx` and NO `self.image_bitmaps`: the painter owns
    /// image resolution (`draw_image` resolves keys), so the free fns are passed
    /// an empty bitmap map (a dead arg they no longer read).
    fn paint_wml_page(
        &self,
        idx: u32,
        p: &dyn Painter,
        clip: Option<&cull::Rect>,
    ) -> Result<(), JsValue> {
        // Per-format dispatch: the paint walk reads only the shared
        // page model; the WML-specific bits (watermark, footnote re-layout,
        // ulTrailSpace) read the Wml arm, defaulting safely when absent.
        let wml = self.wml();
        let ul_trail_space = wml.is_some_and(|w| w.ul_trail_space);
        let pages = self.pages.borrow();
        let page = pages
            .get(idx as usize)
            .ok_or_else(|| JsValue::from_str("page index out of range"))?;

        // The render free fns draw through `p: &dyn Painter` and resolve images
        // via `p.draw_image(key, …)` — they no longer read the bitmap cache, so
        // an empty map satisfies their (now-dead) `&bitmaps` argument. The real
        // bitmap cache lives only in the `render_wml_page` wrapper's painter.
        let bitmaps: HashMap<String, ImageBitmap> = HashMap::new();

        // §17.6 — draw this page at ITS section's geometry (page size + margins),
        // so a mixed-size multi-section doc renders each section at its own dims.
        // Single-section: `geo` == `self.page_size` field-for-field (byte-identical).
        let geo = self.page_geometry(page.section_idx);

        let w = geo.width_px;
        let h = geo.height_px;

        // Page background. Under a windowed paint this ONE page-spanning fill is
        // reduced to the window ∩ page box (`cull::bg_rect`) so its device extent
        // stays bounded by the SLICE at high zoom (the offset alone would leave it
        // page-sized and still overflow Skia). `clip == None` returns the full
        // page box → byte-identical.
        p.set_fill_style_str("#ffffff");
        if let Some((bx, by, bw, bh)) = cull::bg_rect(clip, w, h) {
            p.fill_rect(bx, by, bw, bh);
        }

        // Faint margin guide.
        p.set_stroke_style_str("#e6edf3");
        p.set_line_width(0.5);
        p.stroke_rect(
            geo.margin_left_px,
            geo.margin_top_px,
            geo.content_width_px(),
            geo.content_height_px(),
        );

        // §17.6.10 page borders. Drawn over the (empty) margin band, so the
        // simplified z-order (always front) is moot in practice. `space` is taken
        // from the first present edge — Word writes a uniform offset across edges.
        if let Some(pb) = &geo.page_borders {
            let show = match pb.display {
                ooxml_wml::types::STPageBorderDisplay::AllPages => true,
                ooxml_wml::types::STPageBorderDisplay::FirstPage => idx == 0,
                ooxml_wml::types::STPageBorderDisplay::NotFirstPage => idx != 0,
            };
            if show {
                let space = pb
                    .top
                    .as_ref()
                    .or(pb.bottom.as_ref())
                    .or(pb.left.as_ref())
                    .or(pb.right.as_ref())
                    .map(|e| e.space_px)
                    .unwrap_or(0.0);
                let (x0, y0, x1, y1) = if pb.offset_from_page {
                    (space, space, w - space, h - space)
                } else {
                    (
                        geo.margin_left_px - space,
                        geo.margin_top_px - space,
                        w - geo.margin_right_px + space,
                        h - geo.margin_bottom_px + space,
                    )
                };
                let draw_edge = |edge: &Option<BorderEdge>, ax: f64, ay: f64, bx: f64, by: f64| {
                    if let Some(e) = edge {
                        p.set_stroke_style_str(&e.color);
                        p.set_line_width(e.width_px);
                        p.begin_path();
                        p.move_to(ax, ay);
                        p.line_to(bx, by);
                        p.stroke();
                    }
                };
                draw_edge(&pb.top, x0, y0, x1, y0);
                draw_edge(&pb.bottom, x0, y1, x1, y1);
                draw_edge(&pb.left, x0, y0, x0, y1);
                draw_edge(&pb.right, x1, y0, x1, y1);
            }
        }

        // Watermark (VML WordArt / image) — page-centered, behind all content.
        // CULL FIRST: the text watermark calls `p.measure_text_width`, a
        // SYNC browser round-trip on the canvas backend, so it must NOT run when
        // the page-centred box is off-screen. The box is centred at (w/2, h/2) sized width×height,
        // possibly rotated — cull by the rotation-safe AABB (half-diagonal
        // radius about the centre), so any rotation still keeps when visible.
        let watermark_visible = |bw: f64, bh: f64| -> bool {
            let r = 0.5 * (bw * bw + bh * bh).sqrt();
            cull::keep(clip, w / 2.0 - r, h / 2.0 - r, w / 2.0 + r, h / 2.0 + r)
        };
        if let Some(wm) = wml.and_then(|w| w.watermark.as_ref()).filter(|wm| match wm {
            Watermark::Text(t) => watermark_visible(t.width_px, t.height_px),
            Watermark::Image(im) => watermark_visible(im.width_px, im.height_px),
        }) {
            match wm {
                Watermark::Text(t) => {
                    p.save();
                    p.set_global_alpha(t.opacity);
                    p.set_fill_style_str(&t.color);
                    let _ = p.translate(w / 2.0, h / 2.0);
                    let _ = p.rotate(t.rotation_deg * std::f64::consts::PI / 180.0);
                    p.set_text_align("center");
                    p.set_text_baseline("middle");
                    // Box height drives the glyph size; cap height fills ~0.7 em,
                    // so scale the em up to fill the box vertically, then stretch
                    // horizontally to fill the box width (t136 "plain text").
                    let font_px = (t.height_px / 0.72).max(1.0);
                    p.set_font(&format!("{font_px}px {}", t.font_family));
                    // `measure_text_width` is the painter's only sync browser
                    // query; the recording backend returns 0.0 (≤ 1.0 → no scale),
                    // so the worker path simply skips the horizontal fit.
                    let natural = Some(p.measure_text_width(&t.text)).filter(|w| *w > 1.0);
                    if let Some(nat) = natural {
                        let _ = p.scale(t.width_px / nat, 1.0);
                    }
                    let _ = p.fill_text(&t.text, 0.0, 0.0);
                    p.restore();
                }
                Watermark::Image(im) => {
                    p.save();
                    // Approximate Word's gain/blacklevel washout via opacity.
                    p.set_global_alpha(if im.washout { 0.45 } else { 1.0 });
                    let _ = p.translate(w / 2.0, h / 2.0);
                    let _ = p.rotate(im.rotation_deg * std::f64::consts::PI / 180.0);
                    // The painter resolves the image key (dims come from `im`, not
                    // the bitmap) — no bitmap lookup here, so the worker path can
                    // record the same draw.
                    let _ = p.draw_image(
                        &im.image_key,
                        -im.width_px / 2.0,
                        -im.height_px / 2.0,
                        im.width_px,
                        im.height_px,
                    );
                    p.restore();
                }
            }
        }

        p.set_text_baseline("alphabetic");

        let charts = self.chart_xml.borrow();
        // Make this page's EMF pictures reachable to the WML render path's image
        // dispatch (parse-once cache → cheap Rc clones). Feature-off: no field,
        // no call — byte-identical.
        #[cfg(feature = "emf")]
        emf::set_active(&self.emf_pics.borrow());

        // ── Per-item bbox cull predicates (page-logical px). `clip ==
        // None` ⇒ every `keep` returns true ⇒ this code paints every item in
        // the same order as before (byte-identical). `clip == Some(win)` skips
        // any item whose page-logical bbox misses the window (O(visible) walk).
        // A line's band is [baseline - line_height, baseline + 0.3·line_height]
        // (descenders/underline sit below the baseline) × [content_left,
        // content_right]; a table row spans [y_top, y_top + height] across its
        // cells; an anchored image / text box is its own x/y/w/h box.
        let keep_line = |l: &PagedLine| {
            cull::keep(
                clip,
                l.content_left,
                l.baseline_y - l.line_height,
                l.content_right,
                l.baseline_y + l.line_height * 0.3,
            )
        };
        let keep_row = |row: &PagedTableRow| {
            let (mut x0, mut x1) = (f64::INFINITY, f64::NEG_INFINITY);
            for c in &row.cells {
                x0 = x0.min(c.x);
                x1 = x1.max(c.x + c.width_px);
            }
            cull::keep(clip, x0, row.y_top, x1, row.y_top + row.height_px)
        };
        let keep_ai = |ai: &AnchoredImageLayout| {
            cull::keep(clip, ai.x, ai.y, ai.x + ai.width_px, ai.y + ai.height_px)
        };
        let keep_tb = |tb: &PositionedTextBox| {
            cull::keep(clip, tb.x, tb.y, tb.x + tb.width_px, tb.y + tb.height_px)
        };

        // Render behind-text anchored images and text boxes
        for ai in &page.anchored_images {
            if ai.behind_text && keep_ai(ai) {
                render_anchored_image(p, ai, &bitmaps);
            }
        }
        for tb in &page.positioned_text_boxes {
            if tb.behind_text && keep_tb(tb) {
                render_text_box(p, tb, &bitmaps, &charts, ul_trail_space)?;
            }
        }

        // Render header (lines + tables §17.10)
        for line in &page.header_lines {
            if keep_line(line) {
                render_line(p, line, &bitmaps, &charts, ul_trail_space)?;
            }
        }
        for row in &page.header_rows {
            if keep_row(row) {
                render_table_row(p, row, &bitmaps, &charts, ul_trail_space)?;
            }
        }

        // Render body content
        for element in &page.elements {
            match element {
                PageElement::Line(line) => {
                    if keep_line(line) {
                        render_line(p, line, &bitmaps, &charts, ul_trail_space)?;
                    }
                }
                PageElement::TableRow(row) => {
                    if keep_row(row) {
                        render_table_row(p, row, &bitmaps, &charts, ul_trail_space)?;
                    }
                }
            }
        }

        // Comment highlights — a SEPARATE additive translucent pass,
        // painted AFTER body text so it stacks over run-shading/highlight as a
        // marker swipe (NOT the mutually-exclusive `highlight.or(run_shading)`
        // block in `render_line`). Rects come from `collect_comment_rects`, the
        // read-only post-layout pairing accessor — geometry is never touched.
        // The engine emits the per-author colour; the fixed alpha here is bounded
        // (~0.30) so body text keeps a 4.5:1 contrast over the densest expected
        // shading. (Final colour/alpha tuning is a TS concern in the hybrid
        // model; this canvas pass is the persistent baseline.) `globalAlpha` is
        // saved/restored so nothing downstream is affected.
        let comment_rects = self.collect_comment_rects(page);
        if !comment_rects.is_empty() {
            p.save();
            p.set_global_alpha(COMMENT_FILL_ALPHA);
            for r in &comment_rects {
                // Windowed cull: skip a highlight rect that misses the window.
                if !cull::keep(clip, r.x, r.y, r.x + r.w, r.y + r.h) {
                    continue;
                }
                p.set_fill_style_str(r.color);
                p.fill_rect(r.x, r.y, r.w, r.h);
            }
            p.restore();
        }

        // Search highlights (full-text search) — a SECOND additive
        // translucent pass after the comment pass, painted over body text the
        // same way (read-only over the page model; `globalAlpha`/fill saved &
        // restored). The active query comes from the viewer-held search state set
        // by `set_search_highlight` — so a keystroke repaints highlights without
        // ever touching layout. Every match gets the base swipe; the single
        // ACTIVE match (focused by next/prev) gets the hotter emphasis, so alpha
        // + colour are set PER rect here (unlike the comment pass's once-set
        // alpha). Culled by the same `cull::keep`, so the windowed path
        // inherits it. `active` indexes `collect_search_rects`'s match order,
        // which is identical to the JS `searchPage` order (same accessor core).
        let query = self.search_query.borrow();
        if !query.is_empty() {
            let opts = self.search_opts.get();
            let active = match self.search_active.get() {
                Some((pg, i)) if pg == idx as usize => Some(i),
                _ => None,
            };
            let matches = self.collect_search_rects(idx as usize, page, &query, opts);
            if !matches.is_empty() {
                // Through the painter (not the raw ctx) so the worker's
                // RecordingPainter captures these highlight rects into the
                // display list — the search canary. SML/PML
                // already paint their highlights through `&dyn Painter`.
                p.save();
                for (mi, m) in matches.iter().enumerate() {
                    let is_active = active == Some(mi);
                    p.set_global_alpha(if is_active { SEARCH_ACTIVE_ALPHA } else { SEARCH_FILL_ALPHA });
                    p.set_fill_style_str(if is_active { SEARCH_ACTIVE_COLOR } else { SEARCH_FILL_COLOR });
                    for r in &m.rects {
                        if !cull::keep(clip, r.x, r.y, r.x + r.w, r.y + r.h) {
                            continue;
                        }
                        p.fill_rect(r.x, r.y, r.w, r.h);
                    }
                }
                p.restore();
            }
        }

        // Selection highlight — THIRD additive translucent
        // pass, AFTER the search pass. Through `&dyn Painter`, so the worker
        // RecordingPainter records it identically and
        // slice-culling is inherited via `cull::keep`. Empty/None ⇒ no-op ⇒
        // byte-identical render. Selection is doc-global; `selection_rects_impl`
        // filters by para, so a cross-page selection paints on every page owning
        // covered paras (each page re-invokes this).
        crate::wml::select::paint_selection_pass(p, page, self.selection.get(), clip);

        // Render footnotes and endnotes at page bottom
        let has_notes = !page.footnote_ids.is_empty() || !page.endnote_ids.is_empty();
        // Windowed paint: the footnote/endnote band RE-LAYS-OUT every call (a real cost),
        // so cull it BEFORE the layout loop when the window can't see it. The
        // band is anchored at the page bottom and grows upward, bounded above by
        // the top content margin — so its possible y-range is
        // [margin_top, content_bottom]. If the window misses that band, skip the
        // whole re-layout. (`clip == None` keeps it, unchanged.)
        let band_bottom = geo.height_px - geo.margin_bottom_px;
        let band_visible = cull::keep(
            clip,
            geo.margin_left_px,
            geo.margin_top_px,
            geo.width_px - geo.margin_right_px,
            band_bottom,
        );
        if let (true, true, Some(w)) = (has_notes, band_visible, wml) {
            let content_bottom = band_bottom;
            let mut fn_lines: Vec<PagedLine> = Vec::new();

            for (idx, &fn_id) in page.footnote_ids.iter().enumerate() {
                if let Some(fn_entry) = w.footnotes.get(&fn_id) {
                    // Footnotes with id 0 or -1 are separators — skip
                    if fn_id <= 0 { continue; }
                    let fn_num = format!("{}  ", idx + 1);
                    let fn_content_width = geo.content_width_px();
                    for block in &fn_entry.block_content {
                        if let BlockContent::P(p) = block {
                            if let Ok(cache) = layout_paragraph(
                                &self.font_cache, p, fn_content_width, None,
                                &w.style_ctx, w.default_ppr.as_ref(),
                                w.default_tab_stop_px, &w.theme_fonts,
                                None, &[], &[],
                                if w.grid_enabled { geo.grid_pitch_px } else { None },
                                false,
                                None, // footnote/endnote band re-lays out at paint time (no ParaMeta capture)
                            ) {
                                for (li, line) in cache.lines.iter().enumerate() {
                                    let lh = cache.layout.line_height(
                                        line.max_font_pt, line.max_ascent + line.max_descent,
                                        line.max_image_height,
                                    );
                                    let mut items = line.items.clone();
                                    // Prepend footnote number to first line
                                    if li == 0 {
                                        let num_style = RunStyle {
                                            font_pt: 8.0,
                                            family: DEFAULT_FONT_FAMILY.to_owned(),
                                            family_east_asia: DEFAULT_FONT_FAMILY.to_owned(),
                                            family_complex: DEFAULT_FONT_FAMILY.to_owned(),
                                            vert_align: VertAlign::Superscript,
                                            ..Default::default()
                                        };
                                        let num_width = pt_to_px(8.0) * 0.5 * fn_num.len() as f64;
                                        items.insert(0, Item {
                                            text: fn_num.clone(),
                                            style: num_style,
                                            width: num_width,
                                            content_width: num_width,
                                            // Paint-time footnote number, not paragraph text.
                                            ..Default::default()
                                        });
                                    }
                                    fn_lines.push(PagedLine {
                                        segments: PagedLine::single(items, 0.0),
                                        para: PARA_SENTINEL, // footnote/endnote band — non-captured stream
                                        baseline_y: 0.0,
                                        para_shading: None,
                                        line_height: lh,
                                        content_left: geo.margin_left_px,
                                        content_right: geo.width_px - geo.margin_right_px,
                                        border_top: None,
                                        border_bottom: None,
                                        border_left: None,
                                        border_right: None,
                                        // Bar tabs (§17.18.84) in the footnote/endnote band:
                                        // documented floor — the band re-synthesizes its lines
                                        // without the paragraph cache in scope, so bar rules are
                                        // not painted here.
                                        bar_tabs: Vec::new(),
                                        is_first_line: false,
                                        is_last_line: false,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            // Endnotes — same layout as footnotes
            for (idx, &en_id) in page.endnote_ids.iter().enumerate() {
                if let Some(en_entry) = w.endnotes.get(&en_id) {
                    if en_id <= 0 { continue; }
                    let en_num = format!("{}  ", idx + 1);
                    let fn_content_width = geo.content_width_px();
                    for block in &en_entry.block_content {
                        if let BlockContent::P(p) = block {
                            if let Ok(cache) = layout_paragraph(
                                &self.font_cache, p, fn_content_width, None,
                                &w.style_ctx, w.default_ppr.as_ref(),
                                w.default_tab_stop_px, &w.theme_fonts,
                                None, &[], &[],
                                if w.grid_enabled { geo.grid_pitch_px } else { None },
                                false,
                                None, // footnote/endnote band re-lays out at paint time (no ParaMeta capture)
                            ) {
                                for (li, line) in cache.lines.iter().enumerate() {
                                    let lh = cache.layout.line_height(
                                        line.max_font_pt, line.max_ascent + line.max_descent,
                                        line.max_image_height,
                                    );
                                    let mut items = line.items.clone();
                                    if li == 0 {
                                        let num_style = RunStyle {
                                            font_pt: 8.0,
                                            family: DEFAULT_FONT_FAMILY.to_owned(),
                                            family_east_asia: DEFAULT_FONT_FAMILY.to_owned(),
                                            family_complex: DEFAULT_FONT_FAMILY.to_owned(),
                                            vert_align: VertAlign::Superscript,
                                            ..Default::default()
                                        };
                                        let num_width = pt_to_px(8.0) * 0.5 * en_num.len() as f64;
                                        items.insert(0, Item {
                                            text: en_num.clone(),
                                            style: num_style,
                                            width: num_width,
                                            content_width: num_width,
                                            // Paint-time endnote number, not paragraph text.
                                            ..Default::default()
                                        });
                                    }
                                    fn_lines.push(PagedLine {
                                        segments: PagedLine::single(items, 0.0),
                                        para: PARA_SENTINEL, // footnote/endnote band — non-captured stream
                                        baseline_y: 0.0,
                                        para_shading: None,
                                        line_height: lh,
                                        content_left: geo.margin_left_px,
                                        content_right: geo.width_px - geo.margin_right_px,
                                        border_top: None,
                                        border_bottom: None,
                                        border_left: None,
                                        border_right: None,
                                        // Bar tabs (§17.18.84) in the footnote/endnote band:
                                        // documented floor — the band re-synthesizes its lines
                                        // without the paragraph cache in scope, so bar rules are
                                        // not painted here.
                                        bar_tabs: Vec::new(),
                                        is_first_line: false,
                                        is_last_line: false,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            if !fn_lines.is_empty() {
                let total_fn_height: f64 = fn_lines.iter().map(|l| l.line_height).sum();
                let separator_gap = 6.0;
                let fn_start_y = content_bottom - total_fn_height - separator_gap;

                // Separator line
                p.set_stroke_style_str("#7d8590");
                p.set_line_width(0.5);
                p.begin_path();
                p.move_to(geo.margin_left_px, fn_start_y);
                p.line_to(geo.margin_left_px + geo.content_width_px() * 0.33, fn_start_y);
                p.stroke();

                // Render footnote/endnote lines
                let mut y = fn_start_y + separator_gap;
                for fn_line in &mut fn_lines {
                    fn_line.baseline_y = y + fn_line.line_height * 0.8;
                    if let Some(seg) = fn_line.segments.first_mut() {
                        seg.x_start = geo.margin_left_px;
                    }
                    render_line(p, fn_line, &bitmaps, &charts, ul_trail_space)?;
                    y += fn_line.line_height;
                }
            }
        }

        // Render footer (lines + tables §17.10)
        for line in &page.footer_lines {
            if keep_line(line) {
                render_line(p, line, &bitmaps, &charts, ul_trail_space)?;
            }
        }
        for row in &page.footer_rows {
            if keep_row(row) {
                render_table_row(p, row, &bitmaps, &charts, ul_trail_space)?;
            }
        }

        // Column separator lines (§17.6.4 w:sep) — O(1) per page (one short
        // vertical rule per column gap), so paint unconditionally (windowed
        // paint rule: O(1) full-page chrome is not culled).
        for &(x, y_start, y_end) in &page.col_separator_lines {
            p.set_stroke_style_str("#000000");
            p.set_line_width(0.5);
            p.begin_path();
            p.move_to(x, y_start);
            p.line_to(x, y_end);
            p.stroke();
        }

        // Render in-front anchored images and text boxes
        for ai in &page.anchored_images {
            if !ai.behind_text && keep_ai(ai) {
                render_anchored_image(p, ai, &bitmaps);
            }
        }
        for tb in &page.positioned_text_boxes {
            if !tb.behind_text && keep_tb(tb) {
                render_text_box(p, tb, &bitmaps, &charts, ul_trail_space)?;
            }
        }

        // Anchored-float selection TINT (anchored-image copy) — must
        // paint AFTER the in-front anchored image draw above, or the opaque
        // image buries its own tint (the z-order bug that motivated the split
        // from paint_selection_pass). No selection ⇒ byte-identical no-op.
        crate::wml::select::paint_anchored_selection_tint(p, page, self.selection.get(), clip);

        // Selected-object OUTLINE — additive pass painted
        // LAST so the outline is topmost and never occluded. Through `&dyn
        // Painter` so the worker RecordingPainter records it and slice-culling is
        // inherited. `None` ⇒ zero draw commands ⇒ byte-identical render.
        crate::wml::select::paint_object_outline_pass(
            p,
            page,
            self.selected_object.borrow().as_deref(),
            clip,
        );

        Ok(())
    }

    /// Clickable hyperlink rectangles on page `idx`, as a flat JS array of
    /// objects `{ x, y, w, h, target }` (layout px; target is `"url:…"` or
    /// `"anchor:…"`). The TS layer hit-tests clicks against these. Adjacent
    /// link items on the same line are merged into one rect.
    #[wasm_bindgen(js_name = linkRects)]
    pub fn link_rects(&self, idx: u32) -> js_sys::Array {
        let out = js_sys::Array::new();
        let pages = self.pages.borrow();
        let Some(page) = pages.get(idx as usize) else { return out; };
        let emit = |x: f64, y: f64, w: f64, h: f64, target: &str| {
            let o = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&o, &"x".into(), &x.into());
            let _ = js_sys::Reflect::set(&o, &"y".into(), &y.into());
            let _ = js_sys::Reflect::set(&o, &"w".into(), &w.into());
            let _ = js_sys::Reflect::set(&o, &"h".into(), &h.into());
            let _ = js_sys::Reflect::set(&o, &"target".into(), &target.into());
            out.push(&o);
        };
        let collect_line = |line: &PagedLine| {
            let top = line.baseline_y - line.line_height * 0.85;
            let h = line.line_height;
            for seg in &line.segments {
                let mut x = seg.x_start;
                // Merge consecutive items sharing the same link target.
                let mut run_start: Option<(f64, String)> = None;
                let mut run_w = 0.0_f64;
                for item in &seg.items {
                    match (&item.link, &mut run_start) {
                        (Some(t), Some((_, cur))) if cur == t => { run_w += item.width; }
                        (Some(t), _) => {
                            if let Some((rx, rt)) = run_start.take() {
                                emit(rx, top, run_w, h, &rt);
                            }
                            run_start = Some((x, t.clone()));
                            run_w = item.width;
                        }
                        (None, _) => {
                            if let Some((rx, rt)) = run_start.take() {
                                emit(rx, top, run_w, h, &rt);
                            }
                            run_w = 0.0;
                        }
                    }
                    x += item.width;
                }
                if let Some((rx, rt)) = run_start.take() {
                    emit(rx, top, run_w, h, &rt);
                }
            }
        };
        for el in &page.elements {
            match el {
                PageElement::Line(line) => collect_line(line),
                PageElement::TableRow(row) => {
                    for cell in &row.cells {
                        for line in &cell.lines {
                            collect_line(line);
                        }
                    }
                }
            }
        }
        out
    }

    /// Comment-range highlight rectangles on page `idx`, as a flat JS
    /// array of objects `{ x, y, w, h, ids: [i64], color: "#rrggbb" }` in layout
    /// px. This is the post-layout READ-ONLY accessor that does ALL comment
    /// pairing/overlap — the geometry hot loop is never touched (the load-bearing
    /// invariant: the docx oracle pins must not move). It mirrors `link_rects`
    /// but, instead of merging on a shared link target, merges consecutive items
    /// sharing the same comment-id *set* (`Item::comment_ids`). Because every item
    /// is stamped with the FULL set of ranges open at its position, an overlap
    /// region (covered by two ranges) is its own group carrying both ids — so
    /// overlapping highlights stack as distinct rects without extra pairing
    /// logic. Walks table-cell lines like `link_rects`, so table-cell comments get
    /// highlight parity for free. `color` is the per-author suggestion
    /// (`author_color` of the first id's author) — the TS layer decides the final
    /// translucent paint. Named generically (no `kind`) so a future
    /// search-highlight / track-change carrier can reuse this accessor shape.
    #[wasm_bindgen(js_name = commentRects)]
    pub fn comment_rects(&self, idx: u32) -> js_sys::Array {
        let out = js_sys::Array::new();
        let pages = self.pages.borrow();
        let Some(page) = pages.get(idx as usize) else { return out; };
        for r in self.collect_comment_rects(page) {
            let o = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&o, &"x".into(), &r.x.into());
            let _ = js_sys::Reflect::set(&o, &"y".into(), &r.y.into());
            let _ = js_sys::Reflect::set(&o, &"w".into(), &r.w.into());
            let _ = js_sys::Reflect::set(&o, &"h".into(), &r.h.into());
            let id_arr = js_sys::Array::new();
            for &id in &r.ids {
                id_arr.push(&JsValue::from_f64(id as f64));
            }
            let _ = js_sys::Reflect::set(&o, &"ids".into(), &id_arr);
            let _ = js_sys::Reflect::set(&o, &"color".into(), &r.color.into());
            out.push(&o);
        }
        out
    }

    /// Full-text search over ONE page — a bounded,
    /// READ-ONLY query the TS `plugin-search` chunked loop drives one page per
    /// tick (one page = one bounded wasm call; the loop yields + checks its
    /// generation token between ticks). Returns a JS array of
    /// `{ page, y, sheet, rects: [{x,y,w,h}] }` — one object per match, mirroring
    /// the `commentRects` serialization template. `rects` carry one rect per
    /// soft-wrapped line-portion; `y` is the scroll target (top of the first
    /// rect). Never mutates layout/page state.
    ///
    /// - **WML:** matches the paragraph's logical text (see
    ///   `collect_search_rects`); table-cell text participates. `sheet` is null.
    /// - **SML:** matches the cell MODEL strings (not laid-out pages, the
    ///   key perf lever); for matching cells only it lays out just that page to
    ///   resolve the rect. `opts.scope == "workbook"` widens past the active
    ///   sheet; default is active-sheet only. `page` is then ignored as a sheet
    ///   selector — JS passes the active sheet via `opts` (see SML core).
    ///
    /// `opts` is `{ caseSensitive?: bool, wholeWord?: bool, scope?: "sheet" |
    /// "workbook" }`; absent → case-insensitive substring, active-sheet.
    /// `row_start`/`row_end`: an OPTIONAL half-open 1-based row window the TS
    /// plugin uses to chunk a huge SML sheet across ticks (so the scan yields + can
    /// abort mid-sheet). `None`/`undefined` = the whole sheet (the only mode WML/PML
    /// support — they ignore the window).
    #[wasm_bindgen(js_name = searchPage)]
    pub fn search_page(
        &self,
        idx: u32,
        query: &str,
        opts: JsValue,
        row_start: Option<u32>,
        row_end: Option<u32>,
    ) -> js_sys::Array {
        let opts = SearchOpts::from_js(&opts);
        let matches = match &self.loaded {
            LoadedDoc::Wml(_) => {
                let pages = self.pages.borrow();
                match pages.get(idx as usize) {
                    Some(page) => self.collect_search_rects(idx as usize, page, query, opts),
                    None => Vec::new(),
                }
            }
            #[cfg(feature = "sml")]
            LoadedDoc::Sml(_) => self.search_sml_sheet(idx, query, opts, row_start, row_end),
            #[cfg(feature = "pml")]
            LoadedDoc::Pml(_) => {
                let pages = self.pml_pages.borrow();
                match pages.get(idx as usize) {
                    Some(page) => self.search_pml_slide(idx as usize, page, query, opts),
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        Self::search_matches_to_js(&matches)
    }

    /// Lazily resolve the print rect for the ONE SML match the user navigates to in
    /// Pages view — replaces the per-match `fill_plan_page` that search used to
    /// run M times. `sheet` is the match's FLAT PAGE index (mapped through
    /// `sheet_of_flat_page` like `search_sml_sheet`); `cell_ref` is its A1 ref.
    /// Returns `{page, x, y, w, h}` in paint space (page margins added, mirroring
    /// the facade), or NULL for non-SML / a cell beyond the print range (the
    /// consumer falls back to the page origin — harmless, it can't be in print view).
    #[cfg(feature = "sml")]
    #[wasm_bindgen(js_name = resolveSearchRect)]
    pub fn resolve_search_rect(&self, sheet: u32, cell_ref: &str) -> JsValue {
        let LoadedDoc::Sml(s) = &self.loaded else { return JsValue::NULL };
        let Some(si) = s.sheet_of_flat_page(sheet as usize) else { return JsValue::NULL };
        let Some((col, row)) = sml::model::parse_a1(cell_ref) else { return JsValue::NULL };
        let Some((flat_page, x, y, w, h)) = s.locate_cell_rect(si, row, col, &self.font_cache)
        else {
            return JsValue::NULL;
        };
        let o = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&o, &"page".into(), &JsValue::from_f64(flat_page as f64));
        let _ = js_sys::Reflect::set(
            &o,
            &"x".into(),
            &JsValue::from_f64(self.page_size.margin_left_px + x),
        );
        let _ = js_sys::Reflect::set(
            &o,
            &"y".into(),
            &JsValue::from_f64(self.page_size.margin_top_px + y),
        );
        let _ = js_sys::Reflect::set(&o, &"w".into(), &JsValue::from_f64(w));
        let _ = js_sys::Reflect::set(&o, &"h".into(), &JsValue::from_f64(h));
        o.into()
    }

    /// Serialize `Vec<SearchMatch>` to the JS array shape `searchPage` returns —
    /// the `commentRects` `Reflect::set` template, extended with the nested
    /// `rects` array + the optional `sheet`. Shared by the WML and SML arms.
    fn search_matches_to_js(matches: &[SearchMatch]) -> js_sys::Array {
        let out = js_sys::Array::new();
        for m in matches {
            let o = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&o, &"page".into(), &JsValue::from_f64(m.page as f64));
            let _ = js_sys::Reflect::set(&o, &"y".into(), &m.y.into());
            let _ = js_sys::Reflect::set(
                &o,
                &"sheet".into(),
                &match &m.sheet {
                    Some(s) => JsValue::from_str(s),
                    None => JsValue::NULL,
                },
            );
            let rect_arr = js_sys::Array::new();
            for r in &m.rects {
                let ro = js_sys::Object::new();
                let _ = js_sys::Reflect::set(&ro, &"x".into(), &r.x.into());
                let _ = js_sys::Reflect::set(&ro, &"y".into(), &r.y.into());
                let _ = js_sys::Reflect::set(&ro, &"w".into(), &r.w.into());
                let _ = js_sys::Reflect::set(&ro, &"h".into(), &r.h.into());
                rect_arr.push(&ro);
            }
            let _ = js_sys::Reflect::set(&o, &"rects".into(), &rect_arr);
            // SML navigation aids — a SUPERSET of the WML/PML shape (these are
            // `null`/`0` there). `cellRef` is the active-emphasis identity; the
            // grid scroller jumps to (`gridX`, `gridY`) (content-space, pre-zoom).
            let _ = js_sys::Reflect::set(
                &o,
                &"cellRef".into(),
                &match &m.cell_ref {
                    Some(r) => JsValue::from_str(r),
                    None => JsValue::NULL,
                },
            );
            let _ = js_sys::Reflect::set(&o, &"gridX".into(), &m.grid_x.into());
            let _ = js_sys::Reflect::set(&o, &"gridY".into(), &m.grid_y.into());
            out.push(&o);
        }
        out
    }

    /// Set / clear the active full-text-search highlight state read by the
    /// `render_wml_page` search paint pass (full-text search). The TS
    /// `plugin-search` layer calls this whenever the query, options, or active
    /// match changes, then re-renders the visible pages — highlights repaint
    /// WITHOUT a re-layout (the query never touches geometry). An empty `query`
    /// clears all highlights. `active_page` < 0 (or no active match) → no rect is
    /// emphasised. `opts` is the same bag `searchPage` takes.
    /// `active_cell_ref` is the SML active-match identity (the focused cell's A1
    /// ref, e.g. `"C7"`); `None`/`null` for WML & PML, which emphasise via the
    /// `active_index` instead. It is paired with `active_page` (the flat page the
    /// cell lives on) so the SML paint pass narrows to the page being painted —
    /// mirroring WML's `(page, index)` page check.
    #[wasm_bindgen(js_name = setSearchHighlight)]
    pub fn set_search_highlight(
        &self,
        query: &str,
        opts: JsValue,
        active_page: i32,
        active_index: i32,
        active_cell_ref: Option<String>,
    ) {
        *self.search_query.borrow_mut() = query.to_string();
        self.search_opts.set(SearchOpts::from_js(&opts));
        self.search_active.set(if active_page >= 0 && active_index >= 0 {
            Some((active_page as usize, active_index as usize))
        } else {
            None
        });
        // SML active-match identity: pair the focused cell's A1 ref with its flat
        // page (so the paint pass can narrow to the page it paints). Cleared when
        // there is no active page or JS passed no ref (WML/PML, which use the index).
        *self.search_active_cell.borrow_mut() = match active_cell_ref {
            Some(r) if active_page >= 0 => Some((active_page as usize, r)),
            _ => None,
        };
    }

    /// The active cell's A1 ref IF the focused SML match lives on flat page `idx`,
    /// else `None`. Returned owned so the caller can hold it and lend a `&str` into
    /// the (lifetime-bound) `SearchHighlight` — the RefCell can't be borrowed
    /// across the render call. Mirrors WML's `p == idx` page narrowing.
    #[cfg(feature = "sml")]
    fn sml_active_cell_for(&self, idx: usize) -> Option<String> {
        match &*self.search_active_cell.borrow() {
            Some((p, a1)) if *p == idx => Some(a1.clone()),
            _ => None,
        }
    }

    /// Grid-view twin of `sml_active_cell_for`: the active cell's A1 ref IF its
    /// flat page belongs to the SHEET being painted (the continuous grid is one
    /// scrolling sheet, not a single flat page, and an A1 ref is unique within a
    /// sheet). `None` otherwise (no active match, or it lives on another sheet).
    #[cfg(feature = "sml")]
    fn sml_active_cell_for_sheet(&self, sheet: usize) -> Option<String> {
        let LoadedDoc::Sml(s) = &self.loaded else { return None };
        match &*self.search_active_cell.borrow() {
            Some((p, a1)) if s.sheet_of_flat_page(*p) == Some(sheet) => Some(a1.clone()),
            _ => None,
        }
    }

    /// `idx` is the flat page being painted; `active_ref` is `sml_active_cell_for`'s
    /// result for that page (the caller resolves & holds it so its `&str` outlives
    /// the borrow). `active_ref = Some` ⇒ that cell on THIS page paints hotter.
    #[cfg(feature = "sml")]
    fn sml_search_highlight<'a>(
        &self,
        query: &'a str,
        active_ref: Option<&'a str>,
    ) -> Option<sml::render::SearchHighlight<'a>> {
        if query.is_empty() {
            return None;
        }
        Some(sml::render::SearchHighlight {
            needle: query,
            opts: self.search_opts.get(),
            fill: SEARCH_FILL_COLOR,
            alpha: SEARCH_FILL_ALPHA,
            active: SEARCH_ACTIVE_ALPHA,
            active_fill: SEARCH_ACTIVE_COLOR,
            active_ref,
        })
    }

    /// SML arm of `search_page` (full-text search). `sheet` is the
    /// ACTIVE sheet index JS owns (mirroring `cellAt`/`gridCellAt`). Default
    /// (active-sheet) scope searches only that sheet's cell model; `workbook`
    /// scope loops every surviving sheet (each independent, carrying its own
    /// flat-page offset). Converts each `SheetSearchHit`'s page-local cell rect
    /// into the page's PAINT coordinate space (`+ margin + gutter`), the same
    /// space `cellAt` inverts — so a JS overlay positioned at a match rect lands
    /// on the painted cell. Materializes only matching cells' pages (the model-
    /// not-layout invariant the native test pins).
    #[cfg(feature = "sml")]
    fn search_sml_sheet(
        &self,
        sheet: u32,
        query: &str,
        opts: SearchOpts,
        row_start: Option<u32>,
        row_end: Option<u32>,
    ) -> Vec<SearchMatch> {
        let LoadedDoc::Sml(s) = &self.loaded else { return Vec::new() };
        // A row window only narrows a SINGLE sheet (the plugin chunks the active
        // sheet); workbook scope spans whole sheets, so it ignores the window.
        let row_window = if opts.workbook { None } else { row_start.zip(row_end) };
        let sheets: Vec<usize> = if opts.workbook {
            (0..s.sheet_count()).collect()
        } else {
            // `sheet` is a FLAT PAGE INDEX from the plugin (the active sheet's
            // firstPage), NOT a sheet index — map it through the same plan-order
            // accumulation the page stream uses. (firstPage 0 → sheet 0 worked by
            // accident; any later sheet's firstPage was cast to a bogus sheet idx.)
            s.sheet_of_flat_page(sheet as usize).map(|si| vec![si]).unwrap_or_default()
        };
        let mut out = Vec::new();
        for si in sheets {
            for hit in s.search_sheet_window(si, query, opts, &self.font_cache, row_window) {
                // `hit.x`/`hit.y` are now DEFERRED (0) at search time — the
                // facade still adds the page margins, but the meaningful print rect
                // is resolved lazily by `resolveSearchRect` on Pages-view navigate.
                let x = self.page_size.margin_left_px + hit.x;
                let y = self.page_size.margin_top_px + hit.y;
                out.push(SearchMatch {
                    page: hit.flat_page,
                    rects: vec![SearchRect { x, y, w: hit.w, h: hit.h }],
                    y,
                    sheet: Some(hit.sheet_name),
                    cell_ref: Some(hit.ref_a1),
                    grid_x: hit.grid_x,
                    grid_y: hit.grid_y,
                });
            }
        }
        out
    }

    /// Build the PML slide painter's search-highlight descriptor from the
    /// viewer-held search state — `None` when the query is empty (no highlight).
    /// Shared by the PML `render_page` / `render_page_window` dispatch. The PML
    /// twin of `sml_search_highlight`.
    /// `idx` is the slide being painted; `active` is its slide-global focused-match
    /// index resolved from `search_active` (the SAME `(page, index)` state WML
    /// reads), `None` unless the active match is on THIS slide — mirroring WML's
    /// `p == idx` page narrowing at the `render_wml_page` search pass.
    #[cfg(feature = "pml")]
    fn pml_search_highlight<'a>(
        &self,
        query: &'a str,
        idx: usize,
    ) -> Option<pml::PmlSearchHighlight<'a>> {
        if query.is_empty() {
            return None;
        }
        let active = match self.search_active.get() {
            Some((p, i)) if p == idx => Some(i),
            _ => None,
        };
        Some(pml::PmlSearchHighlight {
            needle: query,
            opts: self.search_opts.get(),
            fill: SEARCH_FILL_COLOR,
            alpha: SEARCH_FILL_ALPHA,
            active,
        })
    }

    /// PML arm of `search_page` (full-text search). Walks slide `idx`'s
    /// shapes (+ table cells) → paragraphs → runs, reusing `find_matches` for
    /// the case/whole-word/UTF-8 logic. Emits one `SearchMatch` per match so the
    /// per-slide count is right; v1 rects are the SHAPE/CELL text-box bbox (not
    /// per-line/per-glyph), which is fine because navigation only consumes the
    /// match `y` to scroll the slide into view. READ-ONLY over the page model —
    /// no layout is run (the exact per-line y would need the paint walk). `sheet`
    /// is `None` (PML has no Excel-tab concept; matches WML).
    #[cfg(feature = "pml")]
    fn search_pml_slide(
        &self,
        idx: usize,
        page: &pml::shape::SlidePage,
        query: &str,
        opts: SearchOpts,
    ) -> Vec<SearchMatch> {
        let mut out = Vec::new();
        let mut emit = |x: f64, y: f64, w: f64, h: f64, text: &str| {
            for _ in find_matches(text, query, opts) {
                out.push(SearchMatch {
                    page: idx,
                    rects: vec![SearchRect { x, y, w, h }],
                    y,
                    sheet: None,
                    cell_ref: None,
                    grid_x: 0.0,
                    grid_y: 0.0,
                });
            }
        };
        // Shapes: the text-box bbox is the shape box inset by its bodyPr insets
        // (mirroring the paint-space `text_x`/`text_w` in `render_page_clipped`).
        // The `y` is the `Top`-anchor text-box top (`sh.y + insets.top`); the
        // exact anchored per-line y needs the paint layout search does not run.
        // Match each PARAGRAPH's logical text (its runs concatenated), NOT each
        // run alone — so a phrase spanning runs/formatting is found, and the count
        // matches the paint pass (which highlights per-paragraph logical text too).
        for sh in &page.shapes {
            let tx = sh.x + sh.insets.left;
            let ty = sh.y + sh.insets.top;
            let tw = (sh.w - sh.insets.left - sh.insets.right).max(0.0);
            for para in &sh.paragraphs {
                let ptext: String = para.runs.iter().map(|r| r.text.as_str()).collect();
                emit(tx, ty, tw, sh.h, &ptext);
            }
        }
        // Tables: each cell already stores its inset text-box origin/width.
        for tbl in &page.tables {
            for cell in &tbl.cells {
                for para in &cell.paragraphs {
                    let ptext: String = para.runs.iter().map(|r| r.text.as_str()).collect();
                    emit(cell.text_x, cell.text_y, cell.text_w, cell.h, &ptext);
                }
            }
        }
        out
    }

    /// Native core of `comment_rects` (and the `render_page` highlight paint
    /// pass): the post-layout comment pairing/overlap over one page, with NO
    /// geometry mutation. Returns one rect per maximal run of consecutive items
    /// sharing the same `Item::comment_ids` set, on body lines AND table-cell
    /// lines. The set-equality merge makes overlap regions (carrying both ids)
    /// their own rects, so highlights stack. Read-only over the page model —
    /// this is where the load-bearing "comments never touch the hot loop"
    /// invariant is honoured.
    fn collect_comment_rects(&self, page: &PageLayout) -> Vec<CommentRect> {
        // id → author colour, for the per-rect colour suggestion.
        let id_color: HashMap<i64, &'static str> = match &self.loaded {
            LoadedDoc::Wml(w) => w
                .comments
                .iter()
                .map(|c| (c.id, author_color(&c.author)))
                .collect(),
            _ => HashMap::new(),
        };
        let mut rects: Vec<CommentRect> = Vec::new();
        let collect_line = |line: &PagedLine, rects: &mut Vec<CommentRect>| {
            // Highlight band: span the full line height (matches the run-shading
            // band) so a multi-line range reads as a continuous marker.
            let top = line.baseline_y - line.line_height * 0.85;
            let h = line.line_height;
            for seg in &line.segments {
                let mut x = seg.x_start;
                // Merge consecutive items sharing the SAME comment-id set into one
                // rect; a membership change (incl. → empty) flushes — so an
                // overlap region (its own distinct set) becomes its own rect.
                let mut run_start: Option<(f64, Vec<i64>)> = None;
                let mut run_w = 0.0_f64;
                for item in &seg.items {
                    let ids: &[i64] = &item.comment_ids;
                    let same = matches!(&run_start, Some((_, cur)) if cur.as_slice() == ids);
                    if same {
                        run_w += item.width;
                    } else {
                        if let Some((rx, rids)) = run_start.take() {
                            let color = rids
                                .first()
                                .and_then(|id| id_color.get(id).copied())
                                .unwrap_or("#FFD400");
                            rects.push(CommentRect { x: rx, y: top, w: run_w, h, ids: rids, color });
                        }
                        run_w = 0.0;
                        if !ids.is_empty() {
                            run_start = Some((x, ids.to_vec()));
                            run_w = item.width;
                        }
                    }
                    x += item.width;
                }
                if let Some((rx, rids)) = run_start.take() {
                    let color = rids
                        .first()
                        .and_then(|id| id_color.get(id).copied())
                        .unwrap_or("#FFD400");
                    rects.push(CommentRect { x: rx, y: top, w: run_w, h, ids: rids, color });
                }
            }
        };
        for el in &page.elements {
            match el {
                PageElement::Line(line) => collect_line(line, &mut rects),
                PageElement::TableRow(row) => {
                    for cell in &row.cells {
                        for line in &cell.lines {
                            collect_line(line, &mut rects);
                        }
                    }
                }
            }
        }
        rects
    }

    /// Native core of the WML `search_page` accessor (and the `render_wml_page`
    /// search paint pass): the post-layout substring match over one page, with NO
    /// geometry mutation — the read-only sibling of `collect_comment_rects`.
    ///
    /// Match semantics (industry-standard find): the query is matched against
    /// each PARAGRAPH's LOGICAL text — the concatenation of its `Item::text`
    /// across every soft-wrapped display line in reading order — so a match may
    /// span a wrapped line break (the same multi-line behaviour comment ranges
    /// produce). A paragraph is the maximal run of consecutive body `PagedLine`s
    /// starting at an `is_first_line` line (the per-paragraph boundary the line
    /// packer stamps); each table cell's lines form their own paragraph group.
    /// Each match's char-range is mapped back to the covering items and SPLIT by
    /// display line, emitting one `SearchMatch` whose `rects` carry one rect per
    /// line-portion (band math reused from `collect_comment_rects`: top =
    /// `baseline_y − line_height·0.85`, height = `line_height`). The first/last
    /// covered item are SLICED to the matched characters via `item_width_to_byte`
    /// (shaped glyph advances), so the rect hugs the search TERM, not the whole
    /// word — the find standard. (Comment rects stay whole-item.)
    fn collect_search_rects(&self, page_idx: usize, page: &PageLayout, needle: &str, opts: SearchOpts) -> Vec<SearchMatch> {
        let mut matches: Vec<SearchMatch> = Vec::new();
        if needle.is_empty() {
            return matches;
        }
        // Group a flat slice of display lines into paragraphs and match each
        // paragraph's logical text. Reused for body lines and per-cell lines.
        // Group a flat line slice into paragraphs (maximal run from an
        // `is_first_line`) and search each.
        let search_para_block = |lines: &[&PagedLine], out: &mut Vec<SearchMatch>| {
            let mut p = 0usize;
            while p < lines.len() {
                let mut q = p + 1;
                while q < lines.len() && !lines[q].is_first_line {
                    q += 1;
                }
                self.search_paragraph(&lines[p..q], needle, opts, out);
                p = q;
            }
        };
        let search_lines = |lines: &[&PagedLine], out: &mut Vec<SearchMatch>| {
            // The slice's own paragraphs. A TableRow / cell boundary is a hard
            // paragraph break because we only ever pass one paragraph-source's
            // lines per call site.
            search_para_block(lines, out);
            // INLINE text boxes (§17.3.3 `w:txbxContent` nested in a run) — built
            // at PAINT time (render.rs) and never stored in the layout, so their
            // text is otherwise unfindable. Their lines are BOX-RELATIVE (the
            // painter offsets each by the item's x + the line top), so search them
            // and translate the rects into page coords. One level deep (the common
            // case); doubly-nested boxes are not recursed.
            for line in lines {
                let top = line.baseline_y - line.line_height;
                for seg in &line.segments {
                    let mut x = seg.x_start;
                    for item in &seg.items {
                        if let Some(tb) = &item.text_box {
                            let tb_lines: Vec<&PagedLine> = tb.lines.iter().collect();
                            let before = out.len();
                            search_para_block(&tb_lines, out);
                            for m in out[before..].iter_mut() {
                                for r in &mut m.rects {
                                    r.x += x;
                                    r.y += top;
                                }
                            }
                        }
                        x += item.width;
                    }
                }
            }
        };
        // Header/footer table rows (§17.10) — their cells' lines, searched the
        // same way as body table cells.
        let search_rows = |rows: &[PagedTableRow], out: &mut Vec<SearchMatch>| {
            for row in rows {
                for cell in &row.cells {
                    let cell_lines: Vec<&PagedLine> = cell.lines.iter().collect();
                    search_lines(&cell_lines, out);
                }
            }
        };
        // HEADER first (it sits at the top of the page) so matches come back in
        // reading order. Headers/footers are laid out SEPARATELY from
        // `page.elements`, so without these walks their paragraph + table text is
        // never searched (a table in a header/footer yielded zero hits).
        let header_lines: Vec<&PagedLine> = page.header_lines.iter().collect();
        search_lines(&header_lines, &mut matches);
        search_rows(&page.header_rows, &mut matches);

        // Body paragraphs: walk runs of consecutive `PageElement::Line`s, a table
        // row flushing the current run (cells handled separately below).
        let mut body_run: Vec<&PagedLine> = Vec::new();
        for el in &page.elements {
            match el {
                PageElement::Line(line) => body_run.push(line),
                PageElement::TableRow(row) => {
                    if !body_run.is_empty() {
                        search_lines(&body_run, &mut matches);
                        body_run.clear();
                    }
                    for cell in &row.cells {
                        let cell_lines: Vec<&PagedLine> = cell.lines.iter().collect();
                        search_lines(&cell_lines, &mut matches);
                    }
                }
            }
        }
        if !body_run.is_empty() {
            search_lines(&body_run, &mut matches);
        }

        // Floating text boxes (§17.3.3 `w:txbxContent`) — their lines are stored
        // BOX-RELATIVE (render_text_box offsets each by tb.x/tb.y), so search the
        // lines then translate the resulting rects into page coords. Common in
        // callouts/sidebars; without this their text is visible but unfindable.
        for tb in &page.positioned_text_boxes {
            let tb_lines: Vec<&PagedLine> = tb.lines.iter().collect();
            let before = matches.len();
            search_lines(&tb_lines, &mut matches);
            for m in matches[before..].iter_mut() {
                for r in &mut m.rects {
                    r.x += tb.x;
                    r.y += tb.y;
                }
            }
        }

        // FOOTER last (bottom of the page).
        let footer_lines: Vec<&PagedLine> = page.footer_lines.iter().collect();
        search_lines(&footer_lines, &mut matches);
        search_rows(&page.footer_rows, &mut matches);
        // Stamp the 0-based page index + scroll-y (top of the first rect) on each
        // match. The index MUST be the 0-based vector index (what `searchPage` is
        // called with, what `scrollToPage` and the render pass's active-match
        // compare expect) — NOT `page.page_number`, which is 1-based.
        for m in &mut matches {
            m.page = page_idx;
            m.y = m.rects.first().map_or(0.0, |r| r.y);
        }
        matches
    }

    /// Match `needle` against ONE paragraph's logical text (its display lines'
    /// items concatenated in reading order) and emit one `SearchMatch` per hit,
    /// each carrying one rect per covered display-line portion. The char→item
    /// map records, per item, its byte range in the concatenated string and its
    /// (line index, x-left, width) so a match's byte range maps back to the
    /// covering items, split at line boundaries. `page`/`y` are filled by the
    /// caller. Items are already in visual order post-bidi, so the cumulative
    /// `seg.x_start + Σ width` advance (mirroring `collect_comment_rects`) gives
    /// each item's left edge.
    fn search_paragraph(
        &self,
        lines: &[&PagedLine],
        needle: &str,
        opts: SearchOpts,
        out: &mut Vec<SearchMatch>,
    ) {
        // Per-item span in the concatenated logical string, with the geometry +
        // the item itself (for sub-item width slicing) needed to draw the rect.
        struct ItemSpan<'a> {
            byte_start: usize,
            byte_end: usize,
            line: usize, // index into `lines`
            x_left: f64,
            item: &'a Item,
        }
        let mut logical = String::new();
        let mut spans: Vec<ItemSpan> = Vec::new();
        for (li, line) in lines.iter().enumerate() {
            for seg in &line.segments {
                let mut x = seg.x_start;
                for item in &seg.items {
                    let byte_start = logical.len();
                    logical.push_str(&item.text);
                    let byte_end = logical.len();
                    if byte_end > byte_start {
                        spans.push(ItemSpan {
                            byte_start,
                            byte_end,
                            line: li,
                            x_left: x,
                            item,
                        });
                    }
                    x += item.width;
                }
            }
        }
        for (m_start, m_end) in find_matches(&logical, needle, opts) {
            // Items whose text byte-range overlaps the match range. Spans are in
            // reading order, so the covered items are contiguous and already
            // grouped by display line.
            let mut rects: Vec<SearchRect> = Vec::new();
            let mut cur_line: Option<usize> = None;
            let mut rx0 = 0.0_f64; // left edge of the matched span on the current line
            let mut rx1 = 0.0_f64; // right edge so far on the current line
            let flush = |line: usize, rx0: f64, rx1: f64, rects: &mut Vec<SearchRect>| {
                let l = lines[line];
                rects.push(SearchRect {
                    x: rx0,
                    y: l.baseline_y - l.line_height * 0.85,
                    w: (rx1 - rx0).max(0.0),
                    h: l.line_height,
                });
            };
            for sp in &spans {
                if sp.byte_end <= m_start || sp.byte_start >= m_end {
                    continue; // item fully outside the match
                }
                // Slice THIS item to the matched byte range: x0/x1 are the match's
                // start/end WITHIN the item (whole-item bounds for a fully-covered
                // middle item), so the rect hugs the matched characters, not the
                // whole word/run.
                let rel_start = m_start.saturating_sub(sp.byte_start);
                let rel_end = m_end.min(sp.byte_end) - sp.byte_start;
                let x0 = sp.x_left + item_width_to_byte(sp.item, rel_start);
                let x1 = sp.x_left + item_width_to_byte(sp.item, rel_end);
                match cur_line {
                    Some(cl) if cl == sp.line => rx1 = x1,
                    Some(cl) => {
                        flush(cl, rx0, rx1, &mut rects);
                        cur_line = Some(sp.line);
                        rx0 = x0;
                        rx1 = x1;
                    }
                    None => {
                        cur_line = Some(sp.line);
                        rx0 = x0;
                        rx1 = x1;
                    }
                }
            }
            if let Some(cl) = cur_line {
                flush(cl, rx0, rx1, &mut rects);
            }
            if !rects.is_empty() {
                out.push(SearchMatch {
                    page: 0,
                    rects,
                    y: 0.0,
                    sheet: None,
                    cell_ref: None,
                    grid_x: 0.0,
                    grid_y: 0.0,
                });
            }
        }
    }

    /// Resolve an internal anchor (bookmark name) to its on-screen position.
    /// Returns `{ page, y }` (page index + the bookmark line's top in layout px)
    /// for the JS scroll-to-anchor handler, or `null` if the name is unknown.
    #[wasm_bindgen(js_name = bookmarkPosition)]
    pub fn bookmark_position(&self, name: &str) -> JsValue {
        let pages = self.pages.borrow();
        for (pi, page) in pages.iter().enumerate() {
            if let Some(y) = bookmark_page_y(page, name) {
                let o = js_sys::Object::new();
                let _ = js_sys::Reflect::set(&o, &"page".into(), &(pi as f64).into());
                let _ = js_sys::Reflect::set(&o, &"y".into(), &y.into());
                return o.into();
            }
        }
        JsValue::NULL
    }

    /// Comment display metadata joined to each range's on-screen start position
    /// Returns a flat JS array of objects
    /// `{ id, author, date, initials, text, page, anchorY, color }` in document
    /// order — one per `word/comments.xml` entry whose `commentRangeStart` was
    /// laid out. `page`/`anchorY` come from resolving the synthetic anchor
    /// `comment_anchor_name(id)` through the same page-walk as
    /// `bookmark_position` (positions exist only after `loadComplete`).
    /// `color` is the per-author suggestion (`author_color`) — the TS layer
    /// decides final presentation. Empty for docx-only / non-WML / comment-free
    /// documents. This accessor never touches the geometry hot loop.
    #[wasm_bindgen(js_name = comments)]
    pub fn comments(&self) -> js_sys::Array {
        let out = js_sys::Array::new();
        for (meta, page, anchor_y) in self.resolve_comments() {
            let o = js_sys::Object::new();
            let set = |k: &str, v: JsValue| {
                let _ = js_sys::Reflect::set(&o, &k.into(), &v);
            };
            set("id", JsValue::from_f64(meta.id as f64));
            set("author", JsValue::from_str(&meta.author));
            set("date", match &meta.date {
                Some(d) => JsValue::from_str(d),
                None => JsValue::NULL,
            });
            set("initials", match &meta.initials {
                Some(i) => JsValue::from_str(i),
                None => JsValue::NULL,
            });
            set("text", JsValue::from_str(&meta.text));
            set("page", JsValue::from_f64(page as f64));
            set("anchorY", JsValue::from_f64(anchor_y));
            set("color", JsValue::from_str(author_color(&meta.author)));
            out.push(&o);
        }
        out
    }

    /// Comment metadata joined to range-start `{page, y}`, in document order.
    /// The format-agnostic core of the `comments()` accessor (the wasm method is
    /// the JS-object envelope around this). Comments whose range start never
    /// laid out are dropped. Read-only over `pages`; touches no geometry loop.
    pub(crate) fn resolve_comments(&self) -> Vec<(CommentMeta, usize, f64)> {
        let LoadedDoc::Wml(w) = &self.loaded else { return Vec::new(); };
        if w.comments.is_empty() {
            return Vec::new();
        }
        let pages = self.pages.borrow();
        let mut out = Vec::new();
        for meta in &w.comments {
            let anchor = comment_anchor_name(meta.id);
            // Resolve the range-start to {page, y}. A comment whose anchor never
            // laid out (e.g. range start inside skipped/unsupported content) is
            // dropped — the panel only shows comments it can navigate to.
            for (pi, page) in pages.iter().enumerate() {
                if let Some(y) = bookmark_page_y(page, &anchor) {
                    out.push((meta.clone(), pi, y));
                    break;
                }
            }
        }
        out
    }

    /// Debug-only layout-introspection probe, layer (a): the
    /// positioned layout. Two-tier:
    ///   `debugLayout(page)`            → Tier 1: a cheap, ordered, shallow index
    ///                                    of the page (each body element's kind +
    ///                                    bbox + id + preview; header/footer text;
    ///                                    float counts).
    ///   `debugLayout(page, nodeId)`    → Tier 2: full detail for one body element
    ///                                    (line → segments → items w/ x/w/link/
    ///                                    bookmark/style; tableRow → cells w/
    ///                                    box/text/fill).
    /// `nodeId` is the element's stable index in `page.elements`.
    #[cfg(feature = "debug-probe")]
    #[wasm_bindgen(js_name = debugLayout)]
    pub fn debug_layout(&self, page: u32, node: Option<u32>) -> JsValue {
        let pages = self.pages.borrow();
        let Some(pg) = pages.get(page as usize) else { return JsValue::NULL; };
        match node {
            None => dbg_page_index(pg, &self.font_cache),
            Some(n) => match pg.elements.get(n as usize) {
                Some(PageElement::Line(l)) => dbg_line_detail(l, n as usize),
                Some(PageElement::TableRow(r)) => dbg_row_detail(r, n as usize),
                None => JsValue::NULL,
            },
        }
    }

    /// Debug-only font-resolution probe (layer b): the resolved font
    /// table — declared→effective family aliases + every loaded face's hhea
    /// metrics + a per-REQUESTED-family FINAL-STATE report. The TS `debugFonts()`
    /// wrapper overlays each face's byte SOURCE (local / CDN) and freshness.
    /// Returns `{ aliases: [{declared, effective}],
    /// faces: [{family, weight, italic, unitsPerEm, ascentEm, descentEm,
    /// lineGapEm}],
    /// families: [{requested, alias, resolvesNow, fellBackToDefault}] }` — one
    /// `families` row per name from `fontFamiliesUsed()` (sorted), answering
    /// "what would a run declaring this paint with RIGHT NOW" via the same
    /// `resolve_font_with_fallbacks` layout bakes into `RunStyle.family`.
    #[cfg(feature = "debug-probe")]
    #[wasm_bindgen(js_name = debugFonts)]
    pub fn debug_fonts(&self) -> JsValue {
        let snap = self.font_cache.dbg_resolutions();
        let o = js_sys::Object::new();

        let aliases = js_sys::Array::new();
        for (declared, effective) in &snap.aliases {
            let e = js_sys::Object::new();
            dbg_set(&e, "declared", declared.clone().into());
            dbg_set(&e, "effective", effective.clone().into());
            aliases.push(&e);
        }
        dbg_set(&o, "aliases", aliases.into());

        let faces = js_sys::Array::new();
        for f in &snap.faces {
            let e = js_sys::Object::new();
            dbg_set(&e, "family", f.family.clone().into());
            dbg_set(&e, "weight", (f.weight as f64).into());
            dbg_set(&e, "italic", f.italic.into());
            dbg_set(&e, "unitsPerEm", (f.units_per_em as f64).into());
            dbg_set(&e, "ascentEm", f.ascent_em.into());
            dbg_set(&e, "descentEm", f.descent_em.into());
            dbg_set(&e, "lineGapEm", f.line_gap_em.into());
            faces.push(&e);
        }
        dbg_set(&o, "faces", faces.into());

        // Per-requested-family FINAL-STATE report: re-resolve each name the
        // document uses against the font cache AS IT IS NOW (sorted for a
        // deterministic probe; fontFamiliesUsed itself stays set-ordered).
        let mut requested = self.families_used_native();
        requested.sort();
        requested.dedup();
        let families = js_sys::Array::new();
        for req in &requested {
            let r = crate::engine::fonts::dbg_family_report(req, &self.font_cache);
            let e = js_sys::Object::new();
            dbg_set(&e, "requested", r.requested.into());
            dbg_set(&e, "alias", r.alias.map_or(JsValue::NULL, JsValue::from));
            dbg_set(&e, "resolvesNow", r.resolves_now.into());
            dbg_set(&e, "fellBackToDefault", r.fell_back_to_default.into());
            families.push(&e);
        }
        dbg_set(&o, "families", families.into());
        o.into()
    }

    /// WINDOWED page render: paint ONLY the items intersecting the visible
    /// viewport (O(visible)), so zoom/scroll render cost is bounded regardless of
    /// document size. APPENDED at the end of `impl Viewer` so the existing
    /// `panic::Location` lines above do not shift (the byte-identity rule).
    ///
    /// Transform chain (paint-only — layout is untouched): the canvas is set up
    /// in DEVICE px (`zoom·dpr` scale), then translated by `-clip_x,-clip_y` so
    /// page-logical coordinates map to the viewport, then CLIPPED to the window
    /// rect (page-logical px). The window in page-logical px is
    /// `{clip_x, clip_y, clip_x + view_w/zoom, clip_y + view_h/zoom}`; every
    /// positioned item is culled against it by `crate::cull`. `render_page`
    /// (unchanged) remains the full-page / oracle / print path — this is a pure
    /// ADDITION that never reroutes it.
    ///
    /// `view_w`/`view_h` are the viewport size in DEVICE px; `zoom` is the
    /// page-logical→CSS scale; `dpr` is devicePixelRatio. All three format arms
    /// (SML / PML / WML) are windowed.
    #[allow(clippy::too_many_arguments)]
    #[wasm_bindgen(js_name = renderPageWindow)]
    pub fn render_page_window(
        &self,
        idx: u32,
        canvas: HtmlCanvasElement,
        clip_x: f64,
        clip_y: f64,
        view_w: f64,
        view_h: f64,
        zoom: f64,
        dpr: f64,
    ) -> Result<(), JsValue> {
        let ctx = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("no 2d context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;

        let z = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
        let d = if dpr.is_finite() && dpr > 0.0 { dpr } else { 1.0 };
        // The window in PAGE-LOGICAL px (the space every item bbox lives in).
        let win = cull::Rect::from_viewport(clip_x, clip_y, view_w, view_h, z);

        // Paint transform: SCALE-ONLY (`device = s · window_local`). We do NOT
        // bake the clip offset into the CTM (the old `ctx.translate(-clip)`): at
        // high zoom that translate pushed device coordinates past the Chrome/Skia
        // ~16384 px limit and Skia SILENTLY dropped the draws (the growing white
        // block). Instead every drawn coordinate is emitted WINDOW-LOCAL by the
        // `OffsetPainter` (offset = the window origin `win.x0/win.y0`), so
        // `device = s·(logical − off)` is bounded by the SLICE at ANY zoom.
        //
        // HARD INVARIANT: the clip rect is authored WINDOW-LOCAL at (0,0) using
        // the SAME `win` whose origin is the painter offset — `rect(0,0,win_w,
        // win_h)` → device `(0,0)…(s·win_w, s·win_h)`, always bounded. This clip
        // is set DIRECTLY on `ctx` (before/independent of the painter), so it does
        // NOT pass through the offset machinery — it is already window-local.
        let s = z * d;
        ctx.set_transform(s, 0.0, 0.0, s, 0.0, 0.0)?;
        ctx.save();
        ctx.begin_path();
        ctx.rect(0.0, 0.0, win.x1 - win.x0, win.y1 - win.y0);
        ctx.clip();

        // Per-format dispatch, mirroring `render_page` — all three arms windowed.
        let res = (|| -> Result<(), JsValue> {
            #[cfg(feature = "sml")]
            if let LoadedDoc::Sml(s) = &self.loaded {
                let sq = self.search_query.borrow();
                let active = self.sml_active_cell_for(idx as usize);
                let sh = self.sml_search_highlight(&sq, active.as_deref());
                return s.render_page_window(
                    idx as usize,
                    &self.sml_pages,
                    self.page_size.margin_left_px,
                    self.page_size.margin_top_px,
                    self.page_size.width_px,
                    self.page_size.height_px,
                    &self.font_cache,
                    &self.image_bitmaps,
                    &self.chart_xml,
                    &ctx,
                    &win,
                    sh.as_ref(),
                );
            }
            #[cfg(feature = "pml")]
            if let LoadedDoc::Pml(p) = &self.loaded {
                let sq = self.search_query.borrow();
                let sh = self.pml_search_highlight(&sq, idx as usize);
                let sel = self.pml_selection.get();
                let obj = self.pml_selected_object.borrow();
                let images = self.image_bitmaps.borrow();
                let painter = crate::engine::paint::CanvasPainter::new(&ctx, &images);
                // Window-local via the offset decorator (offset = window origin);
                // the per-shape `save;translate(sh.x,sh.y);…` transition rule keeps
                // shape-local geometry un-offset (see `OffsetPainter`).
                let off = crate::engine::paint::OffsetPainter::new(&painter, win.x0, win.y0);
                return p.render_page_clipped(
                    idx as usize,
                    &self.pml_pages,
                    &self.font_cache,
                    &self.chart_xml,
                    &off,
                    Some(&win),
                    sh.as_ref(),
                    sel.as_ref(),
                    obj.as_ref(),
                );
            }
            // WML inline arm: the shared body, windowed.
            self.render_wml_page(idx, &ctx, Some(&win))
        })();

        ctx.restore();
        res
    }

    /// Record ONE grid scroll-window of an SML sheet as a serialized display
    /// list (the worker-mode grid path, mirroring
    /// [`Self::render_page_display_list`]). Builds a `RecordingPainter`, runs the
    /// SAME `paint_grid_into` core the in-process canvas path uses (windowed,
    /// O(visible cells); frozen panes + sticky gutters record through the SAME
    /// painter for free), and returns the flat `{ opcodes, strings }` wire form.
    /// The main thread scales by `zoom` and replays onto the grid canvas.
    ///
    /// The recorded stream is CONTENT-space CSS px with `scroll_x`/`scroll_y`
    /// baked into positioning and zoom-INDEPENDENT (NO OP_SCALE) — the TS replay
    /// applies `ctx.scale(zoom)` externally, matching the in-process shim. Live
    /// overlays (selection / search highlight / object outline) are sourced from
    /// engine state on `self` — RPC-synced by `docSetSelection` /
    /// `setSearchHighlight` / `docSetSelectedObject` (FIFO before this record) — and
    /// recorded into the display list, so the worker replay matches the in-process
    /// grid exactly. No worker-mode deviation remains.
    /// Native record core (the recordGridViewport body minus the JsValue pack): the
    /// SML guard + the 3 live overlays sourced from engine state on `self` +
    /// paint_grid_into over a RecordingPainter, returning the DisplayList. Extracted
    /// so the overlay sourcing (the worker-vs-in-process parity contract) is
    /// natively testable — recordGridViewport is `#[wasm_bindgen]` and returns
    /// `JsValue`, so it cannot be driven from a native test.
    #[cfg(feature = "sml")]
    pub(crate) fn record_grid(
        &self,
        sheet: u32,
        scroll_x: f64,
        scroll_y: f64,
        view_w: f64,
        view_h: f64,
        zoom: f64,
    ) -> Result<crate::engine::displaylist::DisplayList, JsValue> {
        let LoadedDoc::Sml(s) = &self.loaded else {
            return Err(JsValue::from_str("recordGridViewport is SML-only"));
        };
        // Source the 3 live overlays from engine state on `self`, mirroring the
        // in-process `paint_grid_viewport` path exactly (all RPC-synced to the
        // worker before this record via setSearchHighlight/docSetSelection/
        // docSetSelectedObject). `sh` borrows `sq` + `active`, so both must outlive
        // the paint_grid_into call — keep them as named locals.
        let sq = self.search_query.borrow();
        let active = self.sml_active_cell_for_sheet(sheet as usize);
        let sh = self.sml_search_highlight(&sq, active.as_deref());
        let cell_sel = self.cell_selection.get().filter(|c| c.sheet == sheet);
        let sel_obj = self.selected_object.borrow();
        let rec = crate::engine::displaylist::RecordingPainter::new();
        s.paint_grid_into(
            &rec,
            sheet as usize,
            scroll_x,
            scroll_y,
            view_w,
            view_h,
            zoom,
            &self.font_cache,
            &self.chart_xml.borrow(),
            sel_obj.as_deref(),
            sh.as_ref(),
            cell_sel,
        )?;
        Ok(rec.take())
    }

    #[cfg(feature = "sml")]
    #[wasm_bindgen(js_name = recordGridViewport)]
    pub fn record_grid_viewport(
        &self,
        sheet: u32,
        scroll_x: f64,
        scroll_y: f64,
        view_w: f64,
        view_h: f64,
        zoom: f64,
    ) -> Result<JsValue, JsValue> {
        let list = self.record_grid(sheet, scroll_x, scroll_y, view_w, view_h, zoom)?;
        let (opcodes, strings) = crate::engine::displaylist::encode_display_list(&list);
        let f = js_sys::Float64Array::new_with_length(opcodes.len() as u32);
        f.copy_from(&opcodes);
        let arr = js_sys::Array::new();
        for s in &strings {
            arr.push(&JsValue::from_str(s));
        }
        let out = js_sys::Object::new();
        js_sys::Reflect::set(&out, &JsValue::from_str("opcodes"), &f)?;
        js_sys::Reflect::set(&out, &JsValue::from_str("strings"), &arr)?;
        Ok(out.into())
    }
}

// ─── Debug probe helpers (layer a) ──────────────────────────────────────────
#[cfg(feature = "debug-probe")]
fn dbg_set(o: &js_sys::Object, k: &str, v: JsValue) {
    let _ = js_sys::Reflect::set(o, &k.into(), &v);
}

#[cfg(feature = "debug-probe")]
fn dbg_line_text(l: &PagedLine) -> String {
    let mut s = String::new();
    for seg in &l.segments {
        for it in &seg.items {
            s.push_str(&it.text);
        }
    }
    s
}

#[cfg(feature = "debug-probe")]
fn dbg_truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(feature = "debug-probe")]
fn dbg_lines_array(lines: &[PagedLine]) -> JsValue {
    let arr = js_sys::Array::new();
    for l in lines {
        let e = js_sys::Object::new();
        dbg_set(&e, "x", l.content_left.into());
        dbg_set(&e, "y", (l.baseline_y - l.line_height * 0.85).into());
        dbg_set(&e, "w", (l.content_right - l.content_left).into());
        dbg_set(&e, "h", l.line_height.into());
        dbg_set(&e, "text", dbg_line_text(l).into());
        arr.push(&e);
    }
    arr.into()
}

#[cfg(feature = "debug-probe")]
/// Per-line probe fields (geometry + the dominant run's resolved font/metrics),
/// shared by body lines AND table-cell lines so the RCA can diff table-heavy docs.
#[cfg(feature = "debug-probe")]
fn dbg_line_fields(e: &js_sys::Object, l: &PagedLine, fc: &FontCache) {
    dbg_set(e, "x", l.content_left.into());
    dbg_set(e, "y", (l.baseline_y - l.line_height * 0.85).into());
    dbg_set(e, "w", (l.content_right - l.content_left).into());
    dbg_set(e, "h", l.line_height.into());
    dbg_set(e, "baselineY", l.baseline_y.into());
    dbg_set(e, "preview", dbg_truncate(&dbg_line_text(l), 60).into());
    if let Some(it) = l.segments.iter().flat_map(|s| &s.items).find(|it| !it.text.is_empty()) {
        dbg_set(e, "font", it.style.family.clone().into());
        dbg_set(e, "fontPt", it.style.font_pt.into());
        let weight = if it.style.bold { 700 } else { 400 };
        if let Some(font) = fc.resolve(&it.style.family, weight, it.style.italic) {
            let m = crate::engine::fonts::font_metrics(font, it.style.font_pt);
            dbg_set(e, "ascentPx", m.ascent.into());
            dbg_set(e, "descentPx", m.descent.into());
            dbg_set(e, "naturalPx", (m.ascent + m.descent).into());
        }
    }
}

#[cfg(feature = "debug-probe")]
fn dbg_page_index(pg: &PageLayout, fc: &FontCache) -> JsValue {
    let o = js_sys::Object::new();
    dbg_set(&o, "pageNumber", (pg.page_number as f64).into());
    let body = js_sys::Array::new();
    for (i, el) in pg.elements.iter().enumerate() {
        let e = js_sys::Object::new();
        dbg_set(&e, "id", (i as f64).into());
        match el {
            PageElement::Line(l) => {
                dbg_set(&e, "kind", "line".into());
                dbg_line_fields(&e, l, fc);
            }
            PageElement::TableRow(r) => {
                dbg_set(&e, "kind", "tableRow".into());
                let x0 = r.cells.iter().map(|c| c.x).fold(f64::INFINITY, f64::min);
                let x1 = r.cells.iter().map(|c| c.x + c.width_px).fold(f64::NEG_INFINITY, f64::max);
                dbg_set(&e, "x", (if x0.is_finite() { x0 } else { 0.0 }).into());
                dbg_set(&e, "y", r.y_top.into());
                dbg_set(&e, "w", (if x1.is_finite() { x1 - x0 } else { 0.0 }).into());
                dbg_set(&e, "h", r.height_px.into());
                dbg_set(&e, "cols", (r.cells.len() as f64).into());
                let t: String = r.cells.iter()
                    .flat_map(|c| c.lines.iter())
                    .flat_map(|l| l.segments.iter())
                    .flat_map(|s| s.items.iter())
                    .map(|it| it.text.as_str())
                    .collect::<Vec<_>>().join(" ");
                dbg_set(&e, "preview", dbg_truncate(&t, 60).into());
            }
        }
        body.push(&e);
    }
    dbg_set(&o, "body", body.into());
    dbg_set(&o, "header", dbg_lines_array(&pg.header_lines));
    dbg_set(&o, "footer", dbg_lines_array(&pg.footer_lines));
    let ai_arr = js_sys::Array::new();
    for ai in &pg.anchored_images {
        let e = js_sys::Object::new();
        dbg_set(&e, "x", ai.x.into());
        dbg_set(&e, "y", ai.y.into());
        dbg_set(&e, "w", ai.width_px.into());
        dbg_set(&e, "h", ai.height_px.into());
        dbg_set(&e, "behind", ai.behind_text.into());
        dbg_set(&e, "relId", ai.rel_id.clone().into());
        ai_arr.push(&e);
    }
    dbg_set(&o, "anchoredImages", ai_arr.into());
    // Flat list of EVERY table-cell line (positions + per-line breakdown), so the
    // RCA captures table-heavy forms whose content the body-line array misses.
    let table_lines = js_sys::Array::new();
    for el in &pg.elements {
        if let PageElement::TableRow(r) = el {
            for c in &r.cells {
                for l in &c.lines {
                    let le = js_sys::Object::new();
                    dbg_set(&le, "kind", "cellLine".into());
                    dbg_line_fields(&le, l, fc);
                    table_lines.push(&le);
                }
            }
        }
    }
    dbg_set(&o, "tableLines", table_lines.into());
    dbg_set(&o, "textBoxes", (pg.positioned_text_boxes.len() as f64).into());
    o.into()
}

#[cfg(feature = "debug-probe")]
fn dbg_line_detail(l: &PagedLine, id: usize) -> JsValue {
    let o = js_sys::Object::new();
    dbg_set(&o, "id", (id as f64).into());
    dbg_set(&o, "kind", "line".into());
    dbg_set(&o, "baselineY", l.baseline_y.into());
    dbg_set(&o, "lineHeight", l.line_height.into());
    dbg_set(&o, "contentLeft", l.content_left.into());
    dbg_set(&o, "contentRight", l.content_right.into());
    let segs = js_sys::Array::new();
    for seg in &l.segments {
        let so = js_sys::Object::new();
        dbg_set(&so, "xStart", seg.x_start.into());
        let items = js_sys::Array::new();
        let mut x = seg.x_start;
        for it in &seg.items {
            let io = js_sys::Object::new();
            dbg_set(&io, "text", it.text.clone().into());
            dbg_set(&io, "x", x.into());
            dbg_set(&io, "w", it.width.into());
            if let Some(ref lk) = it.link { dbg_set(&io, "link", lk.clone().into()); }
            if let Some(ref bm) = it.bookmark { dbg_set(&io, "bookmark", bm.clone().into()); }
            dbg_set(&io, "fontPt", it.style.font_pt.into());
            dbg_set(&io, "color", it.style.color.clone().into());
            if it.style.bold { dbg_set(&io, "bold", true.into()); }
            if it.style.italic { dbg_set(&io, "italic", true.into()); }
            items.push(&io);
            x += it.width;
        }
        dbg_set(&so, "items", items.into());
        segs.push(&so);
    }
    dbg_set(&o, "segments", segs.into());
    o.into()
}

#[cfg(feature = "debug-probe")]
fn dbg_row_detail(r: &PagedTableRow, id: usize) -> JsValue {
    let o = js_sys::Object::new();
    dbg_set(&o, "id", (id as f64).into());
    dbg_set(&o, "kind", "tableRow".into());
    dbg_set(&o, "yTop", r.y_top.into());
    dbg_set(&o, "height", r.height_px.into());
    let cells = js_sys::Array::new();
    for c in &r.cells {
        let co = js_sys::Object::new();
        dbg_set(&co, "x", c.x.into());
        dbg_set(&co, "w", c.width_px.into());
        if c.v_merged_away { dbg_set(&co, "vMergedAway", true.into()); }
        if let Some(ref f) = c.fill_color { dbg_set(&co, "fill", f.clone().into()); }
        let t: String = c.lines.iter()
            .flat_map(|l| l.segments.iter())
            .flat_map(|s| s.items.iter())
            .map(|it| it.text.as_str())
            .collect::<Vec<_>>().join(" ");
        dbg_set(&co, "text", t.into());
        dbg_set(&co, "lineCount", (c.lines.len() as f64).into());
        cells.push(&co);
    }
    dbg_set(&o, "cells", cells.into());
    o.into()
}

impl WmlRenderer {
    fn collect_para_codepoints(
        &self,
        paragraph: &ooxml_wml::types::Paragraph,
        family_codepoints: &mut HashMap<String, HashSet<u32>>,
    ) {
        let theme = &self.theme_fonts;
        for run in paragraph.runs() {
            let family: Option<String> = resolve_run_option(
                &run.r_pr,
                &collect_char_style_rprs(run, &self.style_ctx),
                &collect_paragraph_style_rprs(paragraph, &self.style_ctx),
                &[],
                &self.style_ctx,
                |rpr| rpr.fonts.as_ref().and_then(|f|
                    f.ascii.clone().or_else(||
                        f.ascii_theme.as_ref()
                            .and_then(|t| theme.resolve(t))
                            .map(str::to_owned)
                    )
                ),
            );
            if let Some(f) = family {
                let primary = f.split(';').next().unwrap_or(&f).trim().to_string();
                if !primary.is_empty() {
                    let text = run.text();
                    if !text.is_empty() {
                        let cps = family_codepoints.entry(primary).or_default();
                        for ch in text.chars() {
                            cps.insert(ch as u32);
                        }
                    }
                }
            }
        }
        // OMML math (§22.1): math glyphs render in the run/body font but live in
        // `extra_children`, invisible to `runs()`. Fold each math atom's text into
        // the body family's codepoint set so the face actually loads (else math
        // glyphs blank / fall to estimate widths — the analogue of the "markers
        // never enter fontCodepointsUsed" gotcha). The body font is the math
        // fallback face, so the primary family is the correct bucket.
        if !paragraph.extra_children.is_empty() {
            // Resolve the body font exactly as an empty run would (docDefaults /
            // pStyle ascii slot) — the same bucket the math RunStyle lands in.
            let empty_run = ooxml_wml::types::Run::default();
            let body_family: String = resolve_run_option(
                &empty_run.r_pr,
                &collect_char_style_rprs(&empty_run, &self.style_ctx),
                &collect_paragraph_style_rprs(paragraph, &self.style_ctx),
                &[],
                &self.style_ctx,
                |rpr| rpr.fonts.as_ref().and_then(|f|
                    f.ascii.clone().or_else(||
                        f.ascii_theme.as_ref()
                            .and_then(|t| theme.resolve(t))
                            .map(str::to_owned)
                    )
                ),
            )
            .unwrap_or_else(|| DEFAULT_FONT_FAMILY.to_owned());
            let primary = body_family.split(';').next().unwrap_or(&body_family).trim().to_string();
            if !primary.is_empty() {
                for me in paragraph.math_expressions() {
                    let text = me.text();
                    if !text.is_empty() {
                        let cps = family_codepoints.entry(primary.clone()).or_default();
                        for ch in text.chars() {
                            cps.insert(ch as u32);
                        }
                    }
                }
            }
        }
    }

    fn collect_para_fonts(
        &self,
        paragraph: &ooxml_wml::types::Paragraph,
        families: &mut HashSet<String>,
    ) {
        let theme = &self.theme_fonts;
        for run in paragraph.runs() {
            let family: Option<String> = resolve_run_option(
                &run.r_pr,
                &collect_char_style_rprs(run, &self.style_ctx),
                &collect_paragraph_style_rprs(paragraph, &self.style_ctx),
                &[],
                &self.style_ctx,
                |rpr| rpr.fonts.as_ref().and_then(|f|
                    f.ascii.clone().or_else(||
                        f.ascii_theme.as_ref()
                            .and_then(|t| theme.resolve(t))
                            .map(str::to_owned)
                    )
                ),
            );
            if let Some(f) = family {
                for part in f.split(';') {
                    let trimmed = part.trim();
                    if !trimmed.is_empty() {
                        families.insert(trimmed.to_string());
                    }
                }
            }
            // Script-aware (§17.3.2.26): report the East Asian / Complex Script
            // slot family only when this run actually contains those scripts, so
            // the heavy CJK font is fetched on-demand and Latin docs pay nothing.
            let text = run.text();
            if !text.is_empty() {
                let slot_family = |get: &dyn Fn(&ooxml_wml::types::Fonts) -> Option<String>, deflt: &str| {
                    resolve_run_option(
                        &run.r_pr,
                        &collect_char_style_rprs(run, &self.style_ctx),
                        &collect_paragraph_style_rprs(paragraph, &self.style_ctx),
                        &[], &self.style_ctx,
                        |rpr| rpr.fonts.as_ref().and_then(|f| get(f)),
                    ).unwrap_or_else(|| deflt.to_string())
                };
                if text_has_east_asia(&text) {
                    let ea = slot_family(&|f| f.east_asia.clone().or_else(||
                        f.east_asia_theme.as_ref().and_then(|t| theme.resolve(t)).map(str::to_owned)),
                        DEFAULT_EAST_ASIA_FAMILY);
                    for part in ea.split(';') { let t = part.trim(); if !t.is_empty() { families.insert(t.to_string()); } }
                }
                if text_has_complex(&text) {
                    let cs = slot_family(&|f| f.cs.clone().or_else(||
                        f.cstheme.as_ref().and_then(|t| theme.resolve(t)).map(str::to_owned)),
                        DEFAULT_COMPLEX_FAMILY);
                    for part in cs.split(';') { let t = part.trim(); if !t.is_empty() { families.insert(t.to_string()); } }
                }
            }
        }
    }
}

#[cfg(test)]
mod hyperlink_parse_tests {
    use super::*;
    use std::io::Cursor;

    // Guards the wml-hyperlinks feature + relationship resolution: the parser
    // must surface both an external (r:id → rel target) and an internal
    // (w:anchor) hyperlink, and the hyperlink relationship must resolve to its
    // external URL. Regression for the feature-gate gotcha where ungated enum
    // variants compile but the parser silently drops hyperlink content.
    #[test]
    fn fixture_yields_external_and_anchor_hyperlinks() {
        let bytes = std::fs::read(
            "../../fixtures/hyperlink/clickable-links.docx",
        )
        .expect("fixture present");
        let doc = ooxml_wml::Document::from_reader(Cursor::new(bytes)).expect("parse");

        let url = doc
            .doc_relationships()
            .iter()
            .find(|r| r.relationship_type.contains("/hyperlink"))
            .map(|r| r.target.clone());
        assert_eq!(url.as_deref(), Some("https://example.com/"));

        let mut has_external = false;
        let mut has_anchor = false;
        for block in &doc.body().block_content {
            if let BlockContent::P(p) = block {
                for pc in &p.paragraph_content {
                    if let ooxml_wml::types::ParagraphContent::Hyperlink(h) = pc {
                        if h.id.is_some() { has_external = true; }
                        if h.anchor.is_some() { has_anchor = true; }
                    }
                }
            }
        }
        assert!(has_external, "external (r:id) hyperlink missing");
        assert!(has_anchor, "internal (w:anchor) hyperlink missing");
    }

    #[test]
    #[cfg(feature = "sml")]
    fn record_grid_sources_the_synced_cell_selection_overlay() {
        // Guards the worker overlay-parity contract: recordGridViewport (via the
        // native record_grid seam) must SOURCE the live cell selection from engine
        // state on `self` and record it, so worker-mode xlsx shows the selection the
        // SAME as in-process. Reverting record_grid's sourcing to None (an earlier
        // gap) drops the Excel-green SEL_TINT accent -> RED. This drives
        // the ACTUAL record_grid path (not paint_grid_into directly), so it catches the regression.
        use crate::engine::displaylist::DrawCmd;
        let mut v = Viewer::new().unwrap();
        let bytes = std::fs::read("../../fixtures/spreadsheet/frozen.xlsx").expect("fixture present");
        v.load_xlsx(bytes).expect("frozen.xlsx loads");
        let has_sel_accent = |v: &Viewer| {
            v.record_grid(0, 0.0, 0.0, 640.0, 480.0, 1.0)
                .expect("grid records")
                .iter()
                .any(|c| matches!(c, DrawCmd::SetStrokeStyle(s) if s == "#217346"))
        };
        // No selection set -> record has NO green accent (byte-identical base grid).
        assert!(!has_sel_accent(&v), "no selection => no SEL_TINT accent");
        // A selection (RPC-synced to the worker in worker mode) -> record_grid must
        // source it from self.cell_selection and record the SEL_TINT accent.
        v.doc_set_selection_core(Some(crate::select_dispatch::DocSelectionArg::Cells {
            sheet: 0,
            range: sml::select::CellRange::new(0, 0, 1, 1),
            anchor_row: 0,
            anchor_col: 0,
        }));
        assert!(
            has_sel_accent(&v),
            "record_grid must source the cell selection from self and record the SEL_TINT accent"
        );
    }

    #[test]
    #[cfg(feature = "sml")]
    fn record_grid_records_charts_as_vector_ops_off_thread() {
        // Grid charts render OFF-THREAD as VECTOR opcodes: render_chart draws only
        // through the Painter trait (fill/arc/text), so the recorded display list
        // carries the chart itself — NOT a rasterized placeholder (renderChart-
        // ToCanvas / localChartRaster is the clipboard-COPY seam, never grid paint).
        // record_grid over a chart sheet must contain the series-bar fill
        // (#4472c4 = chart DEFAULT_COLORS[0]), so the worker replay draws the real
        // chart. Guards that grid charts stay recordable off-thread.
        use crate::engine::displaylist::DrawCmd;
        let mut v = Viewer::new().unwrap();
        let bytes = std::fs::read("../../fixtures/spreadsheet/chart.xlsx").expect("fixture present");
        v.load_xlsx(bytes).expect("chart.xlsx loads");
        let list = v.record_grid(0, 0.0, 0.0, 1200.0, 900.0, 1.0).expect("grid records");
        let chart_marks = list
            .iter()
            .filter(|c| matches!(c, DrawCmd::SetFillStyle(s) if s == "#4472c4"))
            .count();
        assert!(
            chart_marks >= 1,
            "record_grid must carry the chart series bars as vector ops off-thread, not a placeholder"
        );
    }
}

// ─── Run-level formatting ────────────────────────────────────────────────────

impl RunStyle {
    /// Neutral style for zero-width, non-rendering marker items (e.g. bookmark
    /// anchors). Values are inert since the item carries no visible text. Now a
    /// thin alias for [`RunStyle::default`] — production code spreads the default
    /// directly, so this survives only as a test-helper spelling.
    #[cfg(test)]
    pub(crate) fn marker() -> Self {
        Self::default()
    }

    pub(crate) fn from_resolved(
        run: &ooxml_wml::types::Run,
        ctx: &StyleContext,
        para_style_rprs: &[&ooxml_wml::types::RunProperties],
        table_style_rprs: &[&ooxml_wml::types::RunProperties],
        font_cache: &FontCache,
        theme_fonts: &ThemeFonts,
    ) -> Self {
        let char_style_rprs = collect_char_style_rprs(run, ctx);

        let bold = resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| &rpr.bold,
        );
        let italic = resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| &rpr.italic,
        );

        let font_pt = resolve_run_option(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| rpr.size.as_ref().and_then(|sz| sz.value.parse::<u32>().ok()),
        )
        .map(|hp| hp as f64 / 2.0)
        .unwrap_or(DEFAULT_FONT_PT);

        let doc_family = resolve_run_option(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| rpr.fonts.as_ref().and_then(|f|
                f.ascii.clone().or_else(||
                    f.ascii_theme.as_ref()
                        .and_then(|t| theme_fonts.resolve(t))
                        .map(str::to_owned)
                )
            ),
        )
        .unwrap_or_else(|| DEFAULT_FONT_FAMILY.to_owned());
        let family = resolve_font_with_fallbacks(&doc_family, font_cache);

        // East Asian (eastAsia) and Complex Script (cs) font slots (§17.3.2.26).
        // Each falls back to the doc's ascii font when its slot is unset, then
        // through the normal substitution chain.
        let doc_family_ea = resolve_run_option(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| rpr.fonts.as_ref().and_then(|f|
                f.east_asia.clone().or_else(||
                    f.east_asia_theme.as_ref().and_then(|t| theme_fonts.resolve(t)).map(str::to_owned)
                )
            ),
        ).unwrap_or_else(|| DEFAULT_EAST_ASIA_FAMILY.to_owned());
        let family_east_asia = resolve_font_with_fallbacks(&doc_family_ea, font_cache);
        let doc_family_cs = resolve_run_option(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| rpr.fonts.as_ref().and_then(|f|
                f.cs.clone().or_else(||
                    f.cstheme.as_ref().and_then(|t| theme_fonts.resolve(t)).map(str::to_owned)
                )
            ),
        ).unwrap_or_else(|| DEFAULT_COMPLEX_FAMILY.to_owned());
        let family_complex = resolve_font_with_fallbacks(&doc_family_cs, font_cache);
        // Step 2 of §17.3.2.26: run carries <w:cs/> or <w:rtl/>.
        let run_is_complex = resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx, |rpr| &rpr.cs,
        ) || resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx, |rpr| &rpr.rtl,
        );

        let color = resolve_run_option(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| {
                let c = rpr.color.as_ref()?;
                // Theme color takes precedence if available, with themeTint /
                // themeShade luminance modulation applied (§17.3.2.6).
                if let Some(ref tc) = c.theme_color {
                    if let Some(hex) = theme_fonts.resolve_color(tc) {
                        return Some(apply_tint_shade(
                            hex,
                            c.theme_tint.as_deref(),
                            c.theme_shade.as_deref(),
                        ));
                    }
                }
                Some(c.value.clone())
            },
        )
        .filter(|h| h.as_bytes().iter().all(|b| b.is_ascii_hexdigit()))
        .map(|h| format!("#{h}"))
        .unwrap_or_else(|| DEFAULT_TEXT_COLOR.to_owned());

        let underline = resolve_run_option(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| rpr.underline.as_ref().and_then(|u| u.value),
        )
        .map(map_underline)
        .unwrap_or(Underline::None);

        let strikethrough = resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| &rpr.strikethrough,
        );

        let double_strike = resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| &rpr.dstrike,
        );

        let vert_align = resolve_run_option(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| rpr.vert_align.as_ref().map(|va| va.value),
        )
        .map(|va| match va {
            ooxml_wml::types::STVerticalAlignRun::Superscript => VertAlign::Superscript,
            ooxml_wml::types::STVerticalAlignRun::Subscript => VertAlign::Subscript,
            _ => VertAlign::Baseline,
        })
        .unwrap_or(VertAlign::Baseline);

        let font_pt = if vert_align != VertAlign::Baseline {
            font_pt * 0.583
        } else {
            font_pt
        };

        let char_spacing_px = resolve_run_option(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| rpr.spacing.as_ref().and_then(|s| s.value.parse::<i64>().ok()),
        )
        .map(twips_to_px)
        .unwrap_or(0.0);

        let highlight = resolve_run_option(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| rpr.highlight.as_ref().map(|h| h.value),
        )
        .and_then(highlight_color_to_css);

        let run_shading = if highlight.is_some() {
            None
        } else {
            resolve_run_option(
                &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
                |rpr| rpr.shading.as_ref()
                    .and_then(|shd| shd.fill.as_deref())
                    .and_then(ooxml_color_to_css),
            )
        };

        let all_caps = resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| &rpr.caps,
        );
        let small_caps = !all_caps && resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| &rpr.small_caps,
        );
        let vanish = resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| &rpr.vanish,
        );

        let outline = resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| &rpr.outline,
        );
        let shadow = resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| &rpr.shadow,
        );
        let emboss = resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| &rpr.emboss,
        );
        let imprint = resolve_run_toggle(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| &rpr.imprint,
        );

        // w:position (§17.3.2.24): signed half-points; + raises, − lowers.
        let baseline_shift_px = resolve_run_option(
            &run.r_pr, &char_style_rprs, para_style_rprs, table_style_rprs, ctx,
            |rpr| rpr.position.as_ref().and_then(|p| p.value.parse::<f64>().ok()),
        )
        .map(|half_pts| pt_to_px(half_pts / 2.0))
        .unwrap_or(0.0);

        Self { bold, italic, font_pt, family, family_east_asia, family_complex, run_is_complex, color, underline, strikethrough, double_strike, vert_align, highlight, run_shading, char_spacing_px, all_caps, small_caps, vanish, outline, shadow, emboss, imprint, baseline_shift_px }
    }

    fn font_css(&self) -> String {
        let style = if self.italic { "italic" } else { "normal" };
        let weight = if self.bold { "bold" } else { "normal" };
        format!(
            "{style} {weight} {size}pt \"{family}\", {fallback}",
            size = self.font_pt,
            family = self.family,
            fallback = CSS_FONT_FALLBACK,
        )
    }
}

thread_local! {
    /// Document-level dates for field evaluation, published at `run_layout`
    /// start (single-threaded WASM) and read in `collect_items`. Avoids
    /// threading a context param through `layout_paragraph`'s many call sites.
    pub(crate) static FIELD_DATES: std::cell::Cell<field::DocDates> =
        const { std::cell::Cell::new(field::DocDates { now: None, created: None, modified: None }) };

    /// Maps a hyperlink relationship id → external URL target, populated from
    /// the document's relationships at load time. Read in `collect_items` to
    /// resolve `<w:hyperlink r:id="...">` external links. Same rationale as
    /// FIELD_DATES (avoids threading the doc through layout).
    pub(crate) static HYPERLINK_RELS: std::cell::RefCell<HashMap<String, String>> =
        std::cell::RefCell::new(HashMap::new());

    /// Whether the document uses Word's legacy line-fitter (compatibilityMode
    /// < 15), which reserves part of a word's trailing space at line end (the
    /// `wrapTrailSpaces`/"Word-compatible trailing blanks" behavior). Published
    /// at `run_layout` start and read in `pack_lines`; same rationale as
    /// FIELD_DATES (avoids threading through `layout_paragraph`'s call sites).
    pub(crate) static RESERVE_TRAILING_BLANKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// External leading (hhea lineGap) folded into a line's descent so the single
/// line height = ascent + descent + lineGap (the font's true line height, the
/// Windows/Word "external leading below the line" model). Word includes the gap;
/// it is non-zero only for Arial/Arimo (67/2048em) and Times/Tinos (87/2048em),
/// zero for Calibri/DejaVu/Verdana — so only those families' line heights change.
pub(crate) fn effective_line_gap(m: &crate::engine::fonts::FontMetrics) -> f64 {
    m.line_gap
}


// ─── Paragraph style chain helpers ──────────────────────────────────────────


fn collect_char_style_rprs<'a>(
    run: &ooxml_wml::types::Run,
    ctx: &'a StyleContext,
) -> Vec<&'a ooxml_wml::types::RunProperties> {
    let style_id = run.r_pr.as_ref()
        .and_then(|rpr| rpr.run_style.as_ref())
        .map(|rs| rs.value.as_str());

    let Some(mut current_id) = style_id.map(str::to_owned) else {
        return Vec::new();
    };

    let mut result = Vec::new();
    let mut depth = 0;
    loop {
        if depth >= 20 { break; }
        let Some(style) = ctx.style(&current_id) else { break };
        if let Some(ref rpr) = style.r_pr {
            result.push(rpr.as_ref());
        }
        match style.based_on.as_ref() {
            Some(base) => current_id = base.value.clone(),
            None => break,
        }
        depth += 1;
    }
    result
}

/// Map a parsed `w:u` `ST_Underline` value to the paint-level `Underline`
/// style. Extracted verbatim from the run-style resolution (mechanical — the
/// arms must stay byte-identical to the pre-extraction inline match); shared
/// with the §17.9.24 numbering-marker rPr digest (`MarkerRunOverrides`).
#[allow(unreachable_patterns)] // exhaustive today; `_` kept as the pre-extraction fallback
pub(crate) fn map_underline(v: ooxml_wml::types::STUnderline) -> Underline {
    match v {
        ooxml_wml::types::STUnderline::Single | ooxml_wml::types::STUnderline::Words => Underline::Single,
        ooxml_wml::types::STUnderline::Double => Underline::Double,
        ooxml_wml::types::STUnderline::Thick => Underline::Thick,
        ooxml_wml::types::STUnderline::Dotted | ooxml_wml::types::STUnderline::DottedHeavy => Underline::Dotted,
        ooxml_wml::types::STUnderline::Dash | ooxml_wml::types::STUnderline::DashedHeavy |
        ooxml_wml::types::STUnderline::DashLong | ooxml_wml::types::STUnderline::DashLongHeavy |
        ooxml_wml::types::STUnderline::DotDash | ooxml_wml::types::STUnderline::DashDotHeavy |
        ooxml_wml::types::STUnderline::DotDotDash | ooxml_wml::types::STUnderline::DashDotDotHeavy => Underline::Dash,
        ooxml_wml::types::STUnderline::Wave | ooxml_wml::types::STUnderline::WavyHeavy |
        ooxml_wml::types::STUnderline::WavyDouble => Underline::Wave,
        ooxml_wml::types::STUnderline::None => Underline::None,
        _ => Underline::Single,
    }
}

pub(crate) fn check_toggle(field: &Option<Box<ooxml_wml::types::OnOffElement>>) -> Option<bool> {
    field.as_ref().map(|ct| match &ct.value {
        None => true,
        Some(v) => matches!(v.as_str(), "1" | "true" | "on"),
    })
}

/// Resolve a toggle (bold, italic, etc.) through the full OOXML cascade
/// per §17.7.3.
///
/// Toggle resolution differs from ordinary "last/first value wins" cascade:
///   1. If direct (run-level) formatting explicitly sets the property, that
///      value is used as-is — no XOR (§17.7.3 first bullet).
///   2. Otherwise each *level* of the style hierarchy (table style, paragraph
///      style, character style) contributes a single value: the FIRST value
///      encountered walking that style's `basedOn` chain (the vecs here are
///      already ordered most-derived → base, so this is the first hit).
///   3. The document-default value short-circuits: if docDefaults is true the
///      effective value is true regardless of the rest (§17.7.3:
///      "If the value specified by the document defaults is true, the
///      effective value is true").
///   4. Otherwise the three level values combine by Boolean XOR:
///      value_effective = val_table XOR val_paragraph XOR val_character
///      i.e. true iff an ODD number of levels assert it. A run re-asserting a
///      style's bold therefore TOGGLES IT OFF.
///
/// The common case (a single non-redundant toggle at one level) is preserved:
/// one true across the three levels XORs to true (on).
fn resolve_run_toggle(
    direct_rpr: &Option<Box<ooxml_wml::types::RunProperties>>,
    char_style_rprs: &[&ooxml_wml::types::RunProperties],
    para_style_rprs: &[&ooxml_wml::types::RunProperties],
    table_style_rprs: &[&ooxml_wml::types::RunProperties],
    ctx: &StyleContext,
    accessor: impl Fn(&ooxml_wml::types::RunProperties) -> &Option<Box<ooxml_wml::types::OnOffElement>>,
) -> bool {
    // (1) Direct formatting wins as-is, no XOR.
    if let Some(rpr) = direct_rpr {
        if let Some(val) = check_toggle(accessor(rpr)) {
            return val;
        }
    }

    // (2) Per-level value = first value down that level's basedOn chain.
    let level_value = |rprs: &[&ooxml_wml::types::RunProperties]| -> Option<bool> {
        rprs.iter().find_map(|rpr| check_toggle(accessor(rpr)))
    };
    let val_table = level_value(table_style_rprs);
    let val_para = level_value(para_style_rprs);
    let val_char = level_value(char_style_rprs);

    // (3) docDefaults true short-circuits the whole resolution to true.
    let doc_default = ctx
        .default_run_properties
        .as_ref()
        .and_then(|d| check_toggle(accessor(d)));
    if doc_default == Some(true) {
        return true;
    }

    // (4) XOR the three level values (None contributes false).
    val_table.unwrap_or(false) ^ val_para.unwrap_or(false) ^ val_char.unwrap_or(false)
}

#[cfg(test)]
mod toggle_resolution_tests {
    use super::*;
    use ooxml_wml::types::{OnOffElement, RunProperties};

    /// A run properties block whose bold toggle is set to the given on/off
    /// state (`None` => element absent).
    fn rpr_bold(on: Option<bool>) -> RunProperties {
        RunProperties {
            bold: on.map(|v| {
                Box::new(OnOffElement {
                    value: Some(if v { "true".to_string() } else { "false".to_string() }),
                    ..Default::default()
                })
            }),
            ..Default::default()
        }
    }

    fn empty_ctx() -> StyleContext {
        StyleContext {
            styles: std::collections::HashMap::new(),
            default_run_properties: None,
        }
    }

    fn resolve_bold(
        direct: &Option<Box<RunProperties>>,
        char_rprs: &[&RunProperties],
        para_rprs: &[&RunProperties],
        table_rprs: &[&RunProperties],
        ctx: &StyleContext,
    ) -> bool {
        resolve_run_toggle(direct, char_rprs, para_rprs, table_rprs, ctx, |r| &r.bold)
    }

    /// The overwhelmingly common case: a single toggle at one level resolves on.
    #[test]
    fn single_char_style_bold_is_on() {
        let ctx = empty_ctx();
        let char_rpr = rpr_bold(Some(true));
        assert!(resolve_bold(&None, &[&char_rpr], &[], &[], &ctx));
    }

    /// §17.7.3 example: paragraph-style bold + character-style bold (both true,
    /// non-direct) XOR to OFF — a run re-asserting a style's bold toggles it off.
    #[test]
    fn redundant_para_and_char_bold_cancels() {
        let ctx = empty_ctx();
        let para_rpr = rpr_bold(Some(true));
        let char_rpr = rpr_bold(Some(true));
        assert!(
            !resolve_bold(&None, &[&char_rpr], &[&para_rpr], &[], &ctx),
            "two true toggles across levels must XOR off"
        );
    }

    /// Three levels all true => odd count => ON again (§17.7.3 XOR).
    #[test]
    fn three_levels_true_is_on() {
        let ctx = empty_ctx();
        let table_rpr = rpr_bold(Some(true));
        let para_rpr = rpr_bold(Some(true));
        let char_rpr = rpr_bold(Some(true));
        assert!(resolve_bold(&None, &[&char_rpr], &[&para_rpr], &[&table_rpr], &ctx));
    }

    /// Direct formatting wins as-is regardless of style XOR (no toggle).
    #[test]
    fn direct_false_overrides_style_bold() {
        let ctx = empty_ctx();
        let para_rpr = rpr_bold(Some(true));
        let direct = Some(Box::new(rpr_bold(Some(false))));
        assert!(!resolve_bold(&direct, &[], &[&para_rpr], &[], &ctx));

        let direct_true = Some(Box::new(rpr_bold(Some(true))));
        let para1 = rpr_bold(Some(true));
        let char1 = rpr_bold(Some(true));
        // Style levels XOR to off, but direct true wins → on.
        assert!(resolve_bold(&direct_true, &[&char1], &[&para1], &[], &ctx));
    }

    /// docDefaults true short-circuits to true even when style XOR would be off.
    #[test]
    fn doc_default_true_short_circuits() {
        let mut ctx = empty_ctx();
        ctx.default_run_properties = Some(rpr_bold(Some(true)));
        // Two style-level trues would XOR off, but docDefault true forces on.
        let para_rpr = rpr_bold(Some(true));
        let char_rpr = rpr_bold(Some(true));
        assert!(resolve_bold(&None, &[&char_rpr], &[&para_rpr], &[], &ctx));
    }

    /// Within one level, the FIRST value in the basedOn chain wins (not XOR).
    /// Vec is ordered most-derived → base, so index 0 is decisive.
    #[test]
    fn first_value_in_based_on_chain_wins_within_level() {
        let ctx = empty_ctx();
        let derived = rpr_bold(Some(true)); // most-derived style sets bold true
        let base = rpr_bold(Some(false)); // base style sets bold false
        // First (derived) true wins for the level; single level true => on.
        assert!(resolve_bold(&None, &[], &[&derived, &base], &[], &ctx));
    }
}

/// Resolve an optional property (font-size, font-family, color) through the full OOXML cascade:
/// direct rPr → character style chain → paragraph style rPr chain → docDefaults.
pub(crate) fn resolve_run_option<T>(
    direct_rpr: &Option<Box<ooxml_wml::types::RunProperties>>,
    char_style_rprs: &[&ooxml_wml::types::RunProperties],
    para_style_rprs: &[&ooxml_wml::types::RunProperties],
    table_style_rprs: &[&ooxml_wml::types::RunProperties],
    ctx: &StyleContext,
    accessor: impl Fn(&ooxml_wml::types::RunProperties) -> Option<T>,
) -> Option<T> {
    if let Some(rpr) = direct_rpr {
        if let val @ Some(_) = accessor(rpr) {
            return val;
        }
    }
    for rpr in char_style_rprs {
        if let val @ Some(_) = accessor(rpr) {
            return val;
        }
    }
    for rpr in para_style_rprs {
        if let val @ Some(_) = accessor(rpr) {
            return val;
        }
    }
    for rpr in table_style_rprs {
        if let val @ Some(_) = accessor(rpr) {
            return val;
        }
    }
    if let Some(defaults) = &ctx.default_run_properties {
        if let val @ Some(_) = accessor(defaults) {
            return val;
        }
    }
    None
}


/// Resolve a Promise on the next event-loop TASK — used to release control
/// between pages during streaming layout so the UI stays responsive and a
/// doc-switch `abortLayout` can actually be delivered (every caller re-checks
/// `layout_gen` immediately after awaiting this; that check is the sole abort
/// seam).
///
/// It must be a **task**, not a microtask. A microtask (`queueMicrotask`, a
/// bare resolved promise) runs before the event loop regains control: it paints
/// no frames and dispatches no `postMessage`, so it would starve exactly what
/// this call exists to protect. `scheduler.yield()` is a task but deliberately
/// prioritises the continuation over other task sources, which starves them
/// too, and it is Chrome-only.
///
/// **Not `setTimeout(…, 0)`.** Nested timers are clamped to 4 ms
/// by HTML §8.6 ("timer nesting level > 5"), and streaming layout is nothing but
/// nested timers, so every page paid the clamp (passing 0 or 1 makes no
/// difference — the clamp is on nesting). The "~4.4 ms/page" figure is an
/// earlier estimate, NOT re-measured here — treat it as the right order
/// of magnitude, not a datum. What this change was measured against is a
/// browser A/B of whole-document layout; that measurement has no committed
/// harness and no reported variance, so it is directional evidence, not a pin.
/// A `MessageChannel` round trip carries the same task-scheduling guarantee
/// (it yields to the event loop, so rendering is not starved) without the
/// nesting clamp.
///
/// A fresh channel per yield, deliberately: one long-lived port would need a
/// shared resolve slot, and two concurrent layouts (doc switch mid-pass) would
/// clobber each other's. The construction is a rounding error next to the clamp
/// it replaces.
///
/// ⚠️ The port must NOT be dropped before the message lands. It isn't: setting
/// `onmessage` implicitly `start()`s the port, and HTML keeps an entangled,
/// started port with a `message` listener alive until its queue drains — which
/// is also why the handler is the promise's own `resolve` rather than a Rust
/// `Closure` we would have to keep alive ourselves.
#[cfg(target_arch = "wasm32")]
pub(crate) async fn yield_to_browser() {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        match web_sys::MessageChannel::new() {
            Ok(chan) => {
                chan.port1().set_onmessage(Some(&resolve));
                let _ = chan.port2().post_message(&JsValue::NULL);
            }
            // No MessageChannel in this scope. Degrade to `setTimeout`, NOT to
            // a bare resolve: a microtask does not yield to the event loop, so
            // it would starve rendering — the exact failure this function
            // exists to prevent, and what an earlier
            // implementation did in EVERY worker, since `web_sys::window()` is
            // None there. Clamped is far better than not yielding. The final
            // synchronous resolve is a liveness guard only: hanging the layout
            // would be worse than a starved frame, and no known browser or
            // worker scope reaches it.
            Err(_) => match web_sys::window() {
                Some(w) => {
                    let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0);
                }
                None => {
                    let _ = resolve.call0(&JsValue::null());
                }
            },
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// Native build: no event loop to yield to — immediately ready, so the layout
/// future never suspends and `run_layout_native` can poll it to completion.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn yield_to_browser() {}

/// Native (non-wasm) regression-harness entry points. Compiled only for
/// `cargo test` / native builds — the wasm binary is unaffected.
#[cfg(not(target_arch = "wasm32"))]
impl Viewer {
    /// Drive the full streaming layout synchronously. The native
    /// `yield_to_browser` is an immediately-ready no-op, so the layout future
    /// never suspends and a single no-op-waker poll completes it.
    pub fn run_layout_native(&self) -> Result<u32, String> {
        self.run_layout_native_with(|_| {})
    }

    /// [`Self::run_layout_native`] with the per-page sink exposed: `on_page` fires as
    /// each page is sealed, in order. Added for the timing harness, whose
    /// tracked metric is time-to-FIRST-page and not
    /// whole-layout wall time — the plain entry point cannot see the first page
    /// land. Both forms share the one no-op-waker driver, so there is still
    /// exactly one of those in the crate.
    pub fn run_layout_native_with(
        &self,
        mut on_page: impl FnMut(u32),
    ) -> Result<u32, String> {
        use std::future::Future;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn noop_raw_waker() -> RawWaker {
            fn no_op(_: *const ()) {}
            fn clone(_: *const ()) -> RawWaker {
                noop_raw_waker()
            }
            static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(self.run_layout_impl(move |page_idx| {
            on_page(page_idx);
            Ok(())
        }));
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(r) => {
                // Rollback instrument (env-gated): per-run LayoutStats census. The
                // four fixpoint counters are the only observer of rollback
                // correctness that pins/goldens cannot see — a dropped rollback
                // moves pages_relaid even when geometry does not. Diffed
                // fixture-by-fixture against a recorded baseline at every
                // refactor step.
                if std::env::var_os("PARASMA_STATS_CENSUS").is_some() {
                    let st = self.layout_stats.get();
                    eprintln!("STATSCENSUS test={} ckpt={} relaid={} demoted={} viol={}",
                        std::thread::current().name().unwrap_or("?"),
                        st.checkpoints_captured, st.pages_relaid, st.floats_demoted, st.violations_detected);
                }
                // JsValue errors are opaque on native (stringifying would call
                // into JS); layout errors fail the harness either way.
                r.map_err(|_| "layout failed (opaque JsValue error)".to_string())
            }
            Poll::Pending => unreachable!("native layout future must not suspend"),
        }
    }
}

// Test-only module declared LAST so its presence never shifts source line
// numbers of code above it — those feed the release wasm's panic-location table.
#[cfg(test)]
mod image_extraction_tests;
#[cfg(test)]
mod wml_search_tests;
#[cfg(test)]
mod chart_copy_tests;
#[cfg(test)]
mod perf_tests;
/// Recorded-paint-stream goldens shared by the SML and PML pins — the
/// format-neutral home, so neither format owns the other's harness.
#[cfg(test)]
mod ops_golden;
/// Regression: the per-page fixpoint must never roll back into
/// an INVALID checkpoint. `capture_checkpoint!` stores `resume: None` when
/// the straddling paragraph's live remainder exceeds
/// `MAX_CHECKPOINT_REMAINDER_ITEMS`, yet still assigns `page_ckpt = Some(..)`,
/// so `seal_or_rollback!`'s eligibility test must inspect `resume`, not merely
/// `is_some()`. Before the fix this document panicked in `restore_checkpoint!`
/// at `.expect("resume directive must be a VALID checkpoint")`.
#[cfg(test)]
mod fixpoint_invalid_checkpoint_tests {
    use super::Viewer;
    use std::io::Write;

    fn load_test_fonts(viewer: &mut Viewer) {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
        for (file, weight, italic) in [
            ("Arimo-Regular.ttf", 400u16, false),
            ("Arimo-Bold.ttf", 700, false),
            ("Arimo-Italic.ttf", 400, true),
            ("Arimo-BoldItalic.ttf", 700, true),
        ] {
            let path = format!("{dir}/{file}");
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|_| panic!("test font missing: {path}\n  run: ./scripts/fetch-test-fonts.sh"));
            assert!(viewer.load_font("Arimo", bytes, weight, italic).is_ok(), "load_font failed for {file}");
        }
    }

    /// 1x1 transparent PNG — the anchored floats' image part.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
        0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc, 0xcf, 0xc0, 0x50,
        0x0f, 0x00, 0x04, 0x85, 0x01, 0x80, 0x84, 0xa9, 0x8c, 0x21, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
        0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// A page-relative `wrapSquare` anchor: `pos_v` EMU from the page top,
    /// `cy` EMU tall (§20.4.2.17 / §17.3.1.13).
    fn page_anchor(id: u32, pos_v: i64, cx: i64, cy: i64) -> String {
        format!(
            r#"<w:drawing><wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0" relativeHeight="251658240" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
<wp:simplePos x="0" y="0"/>
<wp:positionH relativeFrom="page"><wp:posOffset>0</wp:posOffset></wp:positionH>
<wp:positionV relativeFrom="page"><wp:posOffset>{pos_v}</wp:posOffset></wp:positionV>
<wp:extent cx="{cx}" cy="{cy}"/>
<wp:effectExtent l="0" t="0" r="0" b="0"/>
<wp:wrapSquare wrapText="bothSides"/>
<wp:docPr id="{id}" name="Img"/>
<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">
<pic:pic><pic:nvPicPr><pic:cNvPr id="{id}" name="Img"/><pic:cNvPicPr/></pic:nvPicPr>
<pic:blipFill><a:blip r:embed="rId10"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>
<pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm>
<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic>
</a:graphicData></a:graphic></wp:anchor></w:drawing>"#
        )
    }

    /// The adversarial shape: a >4096-item paragraph remainder straddling a page
    /// start (⇒ INVALID checkpoint) on a page that also seals with a
    /// page-relative float positioned ABOVE its own anchor (⇒ a fixpoint
    /// violation asking for a rollback).
    fn oversized_remainder_with_page_float_docx() -> Vec<u8> {
        // 6000 whitespace-separated tokens at 1pt: far above the 4096-item cap,
        // and short enough per line that the paragraph straddles a page start.
        let filler = vec!["a"; 6000].join(" ");
        let body = format!(
            r#"<w:p><w:pPr><w:spacing w:line="12840" w:lineRule="exact"/></w:pPr><w:r>{top_anchor}</w:r><w:r><w:t>filler</w:t></w:r></w:p>
<w:p><w:pPr><w:rPr><w:sz w:val="2"/></w:rPr></w:pPr><w:r><w:rPr><w:sz w:val="2"/></w:rPr><w:t xml:space="preserve">{filler}</w:t></w:r></w:p>
<w:p><w:pPr><w:spacing w:line="1500" w:lineRule="exact"/></w:pPr><w:r><w:t>above the anchor</w:t></w:r></w:p>
<w:p><w:pPr><w:spacing w:line="1500" w:lineRule="exact"/></w:pPr><w:r>{low_anchor}</w:r><w:r><w:t>anchor paragraph</w:t></w:r></w:p>
<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr>"#,
            top_anchor = page_anchor(1, 9067800, 76200, 76200),
            low_anchor = page_anchor(2, 952500, 190500, 3810000),
        );
        let document = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"><w:body>{body}</w:body></w:document>"#
        );
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        for (name, bytes) in [
            ("[Content_Types].xml", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="png" ContentType="image/png"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#.to_vec()),
            ("_rels/.rels", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#.to_vec()),
            ("word/_rels/document.xml.rels", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId10" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/img.png"/>
</Relationships>"#.to_vec()),
            ("word/document.xml", document.into_bytes()),
            ("word/media/img.png", PNG_1X1.to_vec()),
        ] {
            zw.start_file(name, opts).unwrap();
            zw.write_all(&bytes).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    /// Regression for the `.expect("resume directive must be a VALID
    /// checkpoint")` panic: an unusable checkpoint must degrade to ACCEPT (the
    /// page is kept as laid) and the layout must terminate normally.
    #[test]
    fn invalid_checkpoint_degrades_instead_of_panicking() {
        let mut viewer = Viewer::new().expect("Viewer::new");
        load_test_fonts(&mut viewer);
        assert!(viewer.load_docx(oversized_remainder_with_page_float_docx()).is_ok(), "load_docx");
        let pages = viewer.run_layout_native().expect("layout must not fail");
        assert!(pages > 0, "layout must still produce pages, got {pages}");
        // The fixpoint really was exercised (a violation was detected) — without
        // this the test could pass vacuously if the document stopped reaching
        // the seal-violation path.
        let stats = viewer.layout_stats.get();
        assert!(stats.violations_detected > 0, "expected a sealed float violation, stats={stats:?}");
        // ...and the INVALID checkpoint was the one on the violating page, so no
        // rollback was possible: the page was accepted as laid.
        assert_eq!(stats.pages_relaid, 0, "an INVALID checkpoint must not be rolled back into");
    }
}


