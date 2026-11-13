//! PowerPoint (PresentationML) renderer.
//!
//! Slide = print page sized
//! from `p:sldSz`, positioned shapes (own `a:xfrm`, else inherited layout
//! placeholder geometry, else the MASTER placeholder geometry, else a
//! `sldSz`-derived default box), and text rendering (txBody paragraphs/runs
//! through `engine::fonts` + `PaintCtx`, sizes/bold/italic/colour from `rPr`,
//! rPr-less runs through the lstStyle→txStyles chain, fonts via the latin
//! typeface). Shape fills are painted behind the text. Transitions are
//! deferred.
//!
//! Spec grounding (ECMA-376 Part 1):
//!  - §19  PresentationML — the slide envelope (`p:presentation`, `p:sld`,
//!    `p:sldSz`). A slide IS a print page.
//!  - §19.2.1.39  `p:sldSz cx/cy` — the slide size in EMU (default 9144000 ×
//!    6858000 = 4:3 screen). EMU → px via `engine::units::emu_to_px`.
//!  - §19.3.1.x  placeholder inheritance — a slide placeholder with no `a:xfrm`
//!    inherits position/size from the matching layout placeholder,
//!    else the MASTER placeholder (matched by `ph@type`/`@idx`,
//!    §19.7.10); rPr-less run text inherits size/bold/font/
//!    bullet/indent through the lstStyle → layout lstStyle → master
//!    `txStyles` chain (§19.3.1.52).
//!  - §20/§21  DrawingML — slide CONTENT (shapes `p:sp`, text bodies `p:txBody`,
//!    runs `a:r`); painted via `engine::fonts` + `PaintCtx`.
//!
//! The renderer owns the parsed presentation + the original `.pptx` bytes. The
//! high-level `SlideLayout` struct exposes only path/name/type (NOT `cSld`/
//! `spTree`) and `Presentation::read_part` is private, so resolving an inherited
//! placeholder box requires reading the layout part XML ourselves: every
//! load-time self-parse reads through ONE shared `engine::opc::OpcPackage` per
//! byte image and parses each layout via the generated
//! `SlideLayout::from_xml` (a self-parse, so the fork needs no new
//! accessor). Pages stream into the Viewer's SHARED `pml_pages`
//! (borrowed shared state, same discipline as SML — the renderer holds
//! NO back-reference to `Viewer`).

#![cfg_attr(not(test), allow(dead_code))]

pub(crate) mod diagram;
pub(crate) mod select;
pub(crate) mod shape;
pub(crate) mod table;

#[cfg(test)]
pub(crate) mod tests;

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Cursor;

use wasm_bindgen::JsValue;

use crate::engine::fonts::{FaceCache, FontCache};
use crate::engine::opc;
use crate::engine::paint::{Painter, RunStyle, Underline, VertAlign};
use crate::engine::units::emu_to_px;

use ooxml_dml::ext::TextParagraphExt;
use ooxml_pml::ext::{GroupShapeExt, ShapeExt};

use shape::{LayoutGeom, SlidePage, SlideRun};

/// Search-highlight descriptor for the PML slide painter (full-text search)
/// — the PML twin of `sml::render::SearchHighlight`. Paints a
/// translucent `fill` rect (at `alpha`) over every painted RUN whose text
/// matches `needle`/`opts`; the single ACTIVE match (the one next/prev focuses)
/// gets the hotter `crate::SEARCH_ACTIVE_*` emphasis, mirroring WML.
///
/// `active` is the SLIDE-GLOBAL match index of the focused match (`None` when the
/// active match is on another slide). It is global because `search_pml_slide`
/// numbers a slide's matches in ONE flat list across all shapes then all table
/// cells; the paint walk visits shapes/cells one at a time, so it threads a
/// running counter (the `match_base` arg to `paint_paragraph_stack`) in that same
/// shapes-then-cells order to recover the global index per match.
pub(crate) struct PmlSearchHighlight<'a> {
    pub needle: &'a str,
    pub opts: crate::SearchOpts,
    pub fill: &'a str,
    pub alpha: f64,
    pub active: Option<usize>,
}

/// Default slide size in EMU when `p:sldSz` is absent: 9144000 × 6858000, the
/// 4:3 `screen4x3` size (§19.2.1.39).
const DEFAULT_SLD_CX_EMU: i64 = 9_144_000;
const DEFAULT_SLD_CY_EMU: i64 = 6_858_000;

// The officeDocument + theme relationship types come from `opc::rel_type`
// (the fork already spells them; no format module keeps its own copy of
// either). The PML-specific types below are not among
// the fork's consts, so they stay here.
/// OPC relationship type for a slide-layout part (§19.3.1.39 / OPC rel types).
const REL_SLIDE_LAYOUT: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout";
/// OPC relationship type for a slide part.
const REL_SLIDE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";
/// OPC relationship type for a slide-master part (§19.3.1.42 / OPC rel types).
const REL_SLIDE_MASTER: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster";
/// OPC relationship type for the table-styles part (§21.1.3.11).
const REL_TABLE_STYLES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/tableStyles";

/// One slide's parsed content + the layout geometry it inherits from. Built once
/// in `from_pptx`; `run_layout` turns each into a `SlidePage` when fonts are
/// loaded. Owned so the renderer needs no `&mut` at layout time.
struct SlideData {
    shapes: Vec<ooxml_pml::types::Shape>,
    /// Per-shape cumulative group transform (§19.3.1.6), index-aligned with
    /// `shapes`. [`shape::GroupTransform::IDENTITY`] for a top-level (un-grouped)
    /// shape; a `<p:grpSp>`-descendant carries the composed off/ext/chOff/chExt
    /// translate+scale (with nesting) that maps its CHILD-space `a:xfrm` box into
    /// absolute slide space.
    shape_transforms: Vec<shape::GroupTransform>,
    /// Connection shapes (§19.3.1.19 `p:cxnSp`): the slide's connector
    /// lines (with dash + arrowheads), captured from `spTree.connectors()` at
    /// load. Empty for slides with no `cxnSp`.
    connectors: Vec<ooxml_pml::types::Connector>,
    /// Positioned picture draw items (§19.3.1.x `p:pic`), each carrying its EMU
    /// box + the slide-namespaced image key its bytes are stored under.
    pictures: Vec<shape::PictureItem>,
    /// Table draw items (DML `a:tbl` in a `p:graphicFrame`, §21.1.3): the parsed
    /// table paired with its graphicFrame `a:xfrm` box (captured at load while
    /// the fork `Presentation` is alive — `Slide::tables()` gives the table but
    /// not the frame box).
    tables: Vec<table::TableItem>,
    /// Chart draw items (DML chart part referenced from a `p:graphicFrame` via
    /// `c:chart@r:id`, §19.3.1.16 → §21.2): each carries the frame `a:xfrm` box
    /// (EMU) + the slide-namespaced chart-XML key its raw `c:chartSpace` bytes
    /// are stored under. Discovered by walking the same `graphic_frame` list the
    /// table walk uses (the fork's `chart_rel_ids()` gives the r:id but not the
    /// frame box, so we pair them ourselves).
    charts: Vec<shape::ChartItem>,
    layout: LayoutGeom,
    /// Slide background fill (solid or linear gradient), resolved at load from
    /// the slide → layout → master `cSld/bg/bgPr` chain (§19.3.1.2).
    /// `None` = no resolvable bg (blip/absent) → white default.
    bg: Option<shape::SlideBg>,
    /// Per-`<p:sp>` raw `<a:custGeom>…</a:custGeom>` markup, raw-scanned from
    /// the ORIGINAL (pre-repair-strip) slide bytes and keyed by spTree document
    /// order — i.e. `custgeom_paths[i]` belongs to `shapes[i]`. `Some(xml)` for a
    /// shape carrying a custom freeform (RULE 3 replaced it with a rect in the
    /// PARSED `shapes`, so we overlay the real path here); `None` otherwise. Empty
    /// for valid decks (no `<a:custGeom>` → no repair → nothing to scan).
    custgeom_paths: Vec<Option<String>>,
}

/// The PowerPoint renderer's document-specific state. Owns the parsed slides +
/// their inherited layout geometry + the slide-size-derived page box.
pub(crate) struct PmlRenderer {
    /// Per-slide parsed shapes + inherited layout placeholder geometry, in
    /// `p:sldIdLst` order.
    slides: Vec<SlideData>,
    /// Slide size in EMU (§19.2.1.39) — feeds the page box AND the
    /// default-placeholder-box derivation.
    sld_cx_emu: i64,
    sld_cy_emu: i64,
    /// Page width/height in CSS px (slide size, EMU → px).
    page_w: f64,
    page_h: f64,
    /// Picture media bytes collected from the slide rels at load, keyed by the
    /// slide-namespaced image key (`pml::{slideIdx}::{relId}`). The Viewer pulls
    /// these into its shared `images` map so the JS decode pipeline turns them
    /// into `ImageBitmap`s (mirrors the WML/SML extract-at-load contract).
    images: Vec<(String, crate::engine::images::ImageBytes)>,
    /// Raw chart part XML collected from the slide rels at load, keyed by the
    /// slide-namespaced chart key (`chart::pml::{slideIdx}::{relId}`). The Viewer
    /// drains these into its shared `chart_xml` map; `render_page` feeds the
    /// bytes to the SHARED `crate::chart::render_chart` (the pptx chart part is
    /// the same `c:chartSpace` schema WML/SML render — §21.2, format-agnostic).
    chart_xml: Vec<(String, Vec<u8>)>,
    /// The presentation's resolved colour theme (master `<p:clrMap>` + theme
    /// `<a:clrScheme>`). Threaded into `build_slide_page` so `<a:schemeClr>`
    /// fills/text/lines resolve to real theme colours. Default (empty) when the
    /// master/theme parts can't be self-parsed — then `schemeClr` → `None` (scheme
    /// colours drop out).
    theme: shape::SlideTheme,
    /// The presentation's parsed table-style table (`ppt/tableStyles.xml`,
    /// §21.1.3.11), self-parsed once (the same OPC route as the theme part).
    /// Threaded into `build_slide_page` → `build_slide_table` so a table's
    /// `<a:tableStyleId>` resolves header/banding/etc. Empty when the part
    /// is absent / unparsable → every table renders the plain hairline grid.
    table_styles: table::TableStyles,
    /// The slide master's placeholder geometry — the THIRD §19.7.10
    /// inheritance tier (own xfrm → layout → master → sldSz default). Captured
    /// by the same master self-parse that feeds `theme`. Empty on a parse miss.
    master_geom: LayoutGeom,
    /// The master `<p:txStyles>` per-role list styles (§19.3.1.52) — the
    /// master layer of the rPr-less text-property chain. Empty on a parse miss.
    master_txstyles: shape::MasterTextStyles,
}

impl PmlRenderer {
    /// Build a renderer from raw `.pptx` bytes. Parses the presentation
    /// (`Presentation::from_reader`), self-parses `p:sldSz` + each slide's
    /// inherited layout placeholder geometry through ONE shared
    /// [`opc::OpcPackage`] per byte image (the fork exposes neither
    /// `slide_size()` nor a public layout-part reader, hence the
    /// self-parse), and owns the parsed slide
    /// shapes for layout-time page building.
    pub(crate) fn from_pptx(bytes: Vec<u8>) -> Result<Self, JsValue> {
        // (0) Tolerant parse (RULE 3): try the ORIGINAL bytes; ONLY on a
        // missing-attribute failure (the custGeom `<a:moveTo>`/`<a:lnTo>` x-attr
        // codegen bug) do we strip every `<a:custGeom>` to a rect placeholder and
        // re-parse. The repaired bytes (if any) then drive BOTH the OPC geometry
        // self-parse and the fork Presentation, so the two stay consistent. Valid
        // decks parse on the first attempt and never reach the repair.
        let (parsed_slides, bytes, original_bytes) = parse_presentation_tolerant(bytes)?;

        // (0b) ONE shared OPC open per BYTE IMAGE. Up to two images are
        // live per deck — the EFFECTIVE bytes (original, or RULE-3 custGeom-
        // repaired: sites 1/1b/1b'/1d/4/5 + the diagram walk read these, staying
        // consistent with the fork `Presentation`) and the ORIGINAL pre-repair
        // bytes (the custGeom scan MUST read these — RULE 3 replaced the real
        // freeform paths with rect placeholders in the effective image). When no
        // repair fired the images are identical and the effective package serves
        // both; `None` (unopenable) hands every pass its own empty default.
        let mut opc_pkg = opc::OpcPackage::open(&bytes);
        let mut orig_pkg =
            if original_bytes != bytes { opc::OpcPackage::open(&original_bytes) } else { None };

        // (1) Self-parse sldSz + per-slide layout geometry from our own OPC view
        // of the (possibly repaired) bytes.
        let (sld_cx_emu, sld_cy_emu, layout_geoms, slide_paths) =
            parse_opc_geometry(opc_pkg.as_mut())
                .unwrap_or((DEFAULT_SLD_CX_EMU, DEFAULT_SLD_CY_EMU, Vec::new(), Vec::new()));

        // (1b) Self-parse the colour theme (master clrMap + theme clrScheme) and
        // each slide's resolved background colour (slide → layout → master).
        // An empty theme (parse miss) leaves every schemeClr → None (scheme
        // colours drop out).
        let (theme, slide_backgrounds, master_geom, master_txstyles) =
            parse_opc_theme(opc_pkg.as_mut(), &slide_paths);

        // (1b') Table styles: self-parse `ppt/tableStyles.xml`. Empty on
        // absence/parse miss → every table renders unstyled (the plain
        // hairline grid).
        let table_styles = parse_opc_table_styles(opc_pkg.as_mut());

        // (1c) Raw-scan each slide's ORIGINAL (pre-repair-strip) part for
        // per-`<p:sp>` `<a:custGeom>` markup, keyed by spTree document order so it
        // lines up with `s.shapes()`. RULE 3 replaced these custGeoms with rect
        // placeholders in the PARSED shapes (the fork parse bug is live), so the
        // real freeform paths only exist in the original bytes. When NO repair
        // fired (a valid deck), the effective package IS the original view and
        // the scan finds nothing.
        let slide_custgeoms = parse_opc_custgeom(
            if orig_pkg.is_some() { orig_pkg.as_mut() } else { opc_pkg.as_mut() },
            &slide_paths,
        );

        // (3) Pair each slide (sldIdLst order) with its layout geometry (same
        // order — both walk the presentation rels / sldIdLst identically) and its
        // positioned picture items (§19.3.1.x `p:pic`, EMU box + namespaced image
        // key). A slide with no matching layout geom gets an empty map (shapes
        // then fall back to their own xfrm or the sldSz-derived default).
        //
        // We ALSO record, per slide, the (image_key → slide_path, rel_id) tuples
        // so a single OPC pass below can resolve each picture's media bytes
        // through the same shared package handle `parse_opc_geometry` reads
        // (the renderer holds neither bytes nor the fork `Presentation`).
        use ooxml_pml::ext::PictureExt;
        let mut image_specs: Vec<(String, String, String)> = Vec::new(); // (key, slide_path, rel_id)
        let mut chart_specs: Vec<(String, String, String)> = Vec::new(); // (key, slide_path, rel_id)
        let slides: Vec<SlideData> = parsed_slides
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                let pictures = shape::slide_picture_items(s.pictures(), i);
                let tables = slide_table_items(&s);
                let charts = slide_chart_items(&s, i);
                // The slide part path comes from our OPC walk (the fork's
                // `slide_path()` is `pub(crate)`); `parse_opc_geometry` returns
                // paths in the SAME sldIdLst order as `slides()`, so index-pair.
                if let Some(slide_path) = slide_paths.get(i) {
                    for pic in s.pictures() {
                        if let Some(rel) = pic.embed_rel_id() {
                            image_specs.push((
                                shape::image_key(i, rel),
                                slide_path.clone(),
                                rel.to_string(),
                            ));
                        }
                    }
                    // Each chart r:id → (namespaced key, slide path, rel id) so a
                    // single OPC pass below resolves its chart part XML bytes.
                    for c in &charts {
                        if let Some(rel) = c.chart_key.rsplit("::").next() {
                            chart_specs.push((
                                c.chart_key.clone(),
                                slide_path.clone(),
                                rel.to_string(),
                            ));
                        }
                    }
                }
                // The slide's draw shapes = the top-level `p:sp` (identity
                // transform) FOLLOWED BY every `p:grpSp`-descendant `p:sp`,
                // flattened with its cumulative off/ext/chOff/chExt transform
                // (nested groups compose). `Slide::shapes()` returns only the
                // top-level `sp`, so we walk the spTree's `group_shape` list
                // ourselves. Document order within: top-level first, then groups
                // (the painter's z-order floor — group clusters draw above the
                // slide bg; exact spTree interleaving is not modelled).
                let (mut shapes, mut shape_transforms) = collect_slide_shapes(&s);
                // SmartArt diagram graphicFrames render via their pre-rendered
                // DrawingML fallback (`ppt/diagrams/drawingN.xml`). Each `dsp:sp`
                // becomes a `Shape` positioned by a frame-translate transform and
                // appended to the slide's draw list — flowing through the SAME
                // per-shape builder (geometry/fill/text) as top-level + grouped
                // shapes. Its raw custGeom (if any) is appended to `custgeom_paths`
                // index-aligned so the custGeom overlay applies. Table/chart graphicFrames
                // keep their own paths (diagram.rs filters on the diagram uri).
                let mut diagram_custgeoms: Vec<Option<String>> = Vec::new();
                if let Some(slide_path) = slide_paths.get(i) {
                    // Reads through the SHARED effective package (an earlier
                    // version re-opened the whole archive once per slide here).
                    for ds in diagram::slide_diagram_shapes(opc_pkg.as_mut(), slide_path) {
                        shapes.push(ds.shape);
                        shape_transforms.push(ds.transform);
                        diagram_custgeoms.push(ds.custgeom);
                    }
                }
                // Extend the spTree-ordered custGeom overlay with the diagram
                // shapes' entries so `custgeom_paths[i]` stays aligned with
                // `shapes[i]` across both the slide shapes and the appended
                // diagram shapes.
                let mut custgeom_paths =
                    slide_custgeoms.get(i).cloned().unwrap_or_default();
                custgeom_paths.extend(diagram_custgeoms);
                SlideData {
                    shapes,
                    shape_transforms,
                    // The slide's `p:cxnSp` connection shapes, taken from the
                    // ALREADY-PARSED slide (`Slide::inner()` is public — the same
                    // accessor `collect_slide_shapes` uses for `group_shape`).
                    // An earlier version re-read and re-typed-parse every slide
                    // part a second time through the OPC package, on the premise
                    // that `inner` was private. It is not, and the re-parse cost
                    // 8.3 ms of a 44.6 ms 200-slide load (19%) to reproduce a
                    // value already in hand. Non-recursive (spTree direct
                    // children) exactly as before — grpSp connector recursion
                    // is still not done.
                    connectors: s.inner().common_slide_data.shape_tree.connectors().to_vec(),
                    pictures,
                    tables,
                    charts,
                    layout: layout_geoms.get(i).cloned().unwrap_or_default(),
                    bg: slide_backgrounds.get(i).cloned().flatten(),
                    custgeom_paths,
                }
            })
            .collect();

        // (4) Resolve each picture's media bytes from the slide rels (the same
        // shared package — its rels cache makes the per-spec lookups O(1)).
        // Missing / unreadable parts are skipped (the picture then paints a
        // placeholder).
        let images = resolve_picture_images(opc_pkg.as_mut(), &image_specs);
        // (5) Resolve each chart's raw `c:chartSpace` part XML the same way (the
        // chart part is fed AS-IS to the shared `render_chart`; an unresolved
        // chart draws nothing).
        let chart_xml = resolve_chart_xml(opc_pkg.as_mut(), &chart_specs);

        Ok(PmlRenderer {
            slides,
            sld_cx_emu,
            sld_cy_emu,
            page_w: emu_to_px(sld_cx_emu),
            page_h: emu_to_px(sld_cy_emu),
            images,
            chart_xml,
            theme,
            table_styles,
            master_geom,
            master_txstyles,
        })
    }

    /// The picture media bytes collected at load, keyed by slide-namespaced image
    /// key. The Viewer drains these into its shared `images` map (then JS decodes
    /// them to `ImageBitmap`s). Returns owned pairs (the renderer keeps none).
    pub(crate) fn take_images(&mut self) -> Vec<(String, crate::engine::images::ImageBytes)> {
        std::mem::take(&mut self.images)
    }

    /// The raw chart part XML collected at load, keyed by slide-namespaced chart
    /// key (`chart::pml::{slideIdx}::{relId}`). The Viewer drains these into its
    /// shared `chart_xml` map so `render_page` can feed the bytes to the shared
    /// `crate::chart::render_chart`. Returns owned pairs (the renderer keeps none).
    pub(crate) fn take_charts(&mut self) -> Vec<(String, Vec<u8>)> {
        std::mem::take(&mut self.chart_xml)
    }

    /// Page width/height in CSS px (uniform across slides; the slide size
    /// is presentation-wide). The Viewer mirrors this into its shared
    /// `page_size` so `pageSize()` returns the slide geometry.
    pub(crate) fn page_box(&self) -> (f64, f64) {
        (self.page_w, self.page_h)
    }

    /// Number of slides (§19.3.1.x). Drives the page count.
    pub(crate) fn slide_count(&self) -> usize {
        self.slides.len()
    }

    /// Build all slide pages synchronously (test-only — the production path
    /// streams them through `run_layout` with abort/yield). Lets native tests pin
    /// the resolved geometry + text without a wasm event loop.
    #[cfg(test)]
    pub(crate) fn build_pages_for_test(&self, font_cache: &FontCache) -> Vec<SlidePage> {
        self.slides
            .iter()
            .map(|sd| {
                shape::build_slide_page(
                    &sd.shapes,
                    &sd.shape_transforms,
                    &sd.connectors,
                    &sd.pictures,
                    &sd.tables,
                    &sd.charts,
                    &sd.layout,
                    &self.master_geom,
                    &self.master_txstyles,
                    self.sld_cx_emu,
                    self.sld_cy_emu,
                    sd.bg.clone(),
                    &self.theme,
                    &self.table_styles,
                    &sd.custgeom_paths,
                    font_cache,
                )
            })
            .collect()
    }

    /// Every font family the deck paints with: the RESOLVED latin typeface
    /// of every run (theme `+mj-lt`/`+mn-lt` refs resolved — the literal ref
    /// string would miss the JS font-map), the theme major/minor latin seeds
    /// (mirroring the WML seed in `lib.rs::font_families_used`), and — script-
    /// aware, like WML's `collect_para_fonts` — the resolved East Asian /
    /// Complex Script slot family for runs that actually CONTAIN those scripts,
    /// so the heavy CJK font is fetched on-demand and Latin decks pay nothing.
    /// The JS font plan loads substitutes before layout, then the engine
    /// default backs anything unresolved — same contract as SML's
    /// `font_families`.
    pub(crate) fn font_families(&self) -> Vec<String> {
        use crate::engine::fonts::{text_has_complex, text_has_east_asia};
        use ooxml_dml::ext::TextRunExt;
        let mut set: HashSet<String> = HashSet::new();
        for f in [&self.theme.fonts.major_latin, &self.theme.fonts.minor_latin].into_iter().flatten() {
            if !f.is_empty() {
                set.insert(f.clone());
            }
        }
        // Any typeface the inheritance chain can hand an rPr-less run —
        // master txStyles + layout placeholder lstStyles + shape lstStyles
        // (theme refs resolved; the `+mj/+mn` common case folds into the seeds
        // above, a LITERAL chain typeface is seeded here).
        let mut seed_ls = |ls: &shape::ListStyle| {
            for lvl in std::iter::once(&ls.def)
                .chain(ls.levels.iter().flatten())
            {
                if let Some(f) = lvl.latin.as_deref().and_then(|n| self.theme.fonts.resolve_ref(n))
                {
                    set.insert(f);
                }
            }
        };
        for ls in self.master_txstyles.list_styles() {
            seed_ls(ls);
        }
        for sd in &self.slides {
            for ls in sd.layout.list_styles() {
                seed_ls(ls);
            }
            for sp in &sd.shapes {
                if let Some(own) =
                    sp.text_body.as_deref().and_then(|tb| tb.lst_style.as_deref())
                {
                    seed_ls(&shape::list_style_from(own));
                }
            }
        }
        for sd in &self.slides {
            for sp in &sd.shapes {
                for p in sp.paragraphs() {
                    for r in p.runs() {
                        if let Some(f) = shape::run_explicit_latin(r, &self.theme) {
                            set.insert(f);
                        }
                        let text = r.text();
                        if text.is_empty() {
                            continue;
                        }
                        if text_has_east_asia(text) {
                            set.insert(shape::run_doc_east_asia(r, &self.theme));
                        }
                        if text_has_complex(text) {
                            set.insert(shape::run_doc_complex(r, &self.theme));
                        }
                    }
                }
            }
        }
        set.into_iter().collect()
    }

    /// Stream one page per slide into the Viewer's SHARED `pml_pages`, firing
    /// `on_page` per page and honoring `layout_gen` for abort. Mirrors the SML
    /// streaming contract exactly (clear, build, push, fire, yield, re-check
    /// gen). Each page is built here (shape boxes resolved + text shaped) with
    /// the loaded `font_cache`.
    pub(crate) async fn run_layout(
        &self,
        pml_pages: &RefCell<Vec<SlidePage>>,
        layout_gen: &Cell<u64>,
        font_cache: &FontCache,
        mut on_page: impl FnMut(u32) -> Result<(), JsValue>,
    ) -> Result<u32, JsValue> {
        let my_gen = layout_gen.get();
        pml_pages.borrow_mut().clear();

        let mut count: u32 = 0;
        for sd in &self.slides {
            // Abort if a new document started loading (gen bumped). Never hold a
            // borrow across the await below; only read the Cell here.
            if layout_gen.get() != my_gen {
                break;
            }
            let page = shape::build_slide_page(
                &sd.shapes,
                &sd.shape_transforms,
                &sd.connectors,
                &sd.pictures,
                &sd.tables,
                &sd.charts,
                &sd.layout,
                &self.master_geom,
                &self.master_txstyles,
                self.sld_cx_emu,
                self.sld_cy_emu,
                sd.bg.clone(),
                &self.theme,
                &self.table_styles,
                &sd.custgeom_paths,
                font_cache,
            );
            let idx = {
                let mut p = pml_pages.borrow_mut();
                p.push(page);
                p.len() as u32 - 1
            };
            on_page(idx)?;
            count = idx + 1;
            crate::yield_to_browser().await;
            if layout_gen.get() != my_gen {
                break;
            }
        }
        Ok(count)
    }

    /// Paint slide page `idx`: a white slide background, then each shape's solid
    /// fill (if any), then its text paragraphs stacked on baselines through
    /// `engine::paint::paint_glyph_run`, wrapped per each shape's `bodyPr@wrap`;
    /// the box clips nothing (overflow is left visible, honest).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_page(
        &self,
        idx: usize,
        pml_pages: &RefCell<Vec<SlidePage>>,
        font_cache: &FontCache,
        chart_xml: &RefCell<std::collections::HashMap<String, Vec<u8>>>,
        p: &dyn Painter,
        search: Option<&PmlSearchHighlight>,
        selection: Option<&select::PmlSelection>,
        selected_object: Option<&(usize, String)>,
    ) -> Result<(), JsValue> {
        // Full-slide paint = the windowed paint with NO window (`clip = None`):
        // every cull predicate keeps, so the draw sequence is identical.
        self.render_page_clipped(
            idx, pml_pages, font_cache, chart_xml, p, None, search, selection, selected_object,
        )
    }

    /// Shared slide paint, threaded by `clip` (the windowed paint). `None` = full slide
    /// (byte-identical to the original `render_page`); `Some(win)` = windowed —
    /// each positioned item (picture / shape / table cell / chart) is culled by
    /// its box ∩ `win` (`crate::cull::keep`), so the item walk is O(visible).
    /// The slide background fill is O(1) chrome → unconditional. `SlideConnector`
    /// carries only `x,y` + a shape-local path (no `w/h`), so connectors paint
    /// UNCONDITIONALLY for v1 (a path-bbox cull is a follow-on).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_page_clipped(
        &self,
        idx: usize,
        pml_pages: &RefCell<Vec<SlidePage>>,
        font_cache: &FontCache,
        chart_xml: &RefCell<std::collections::HashMap<String, Vec<u8>>>,
        p: &dyn Painter,
        clip: Option<&crate::cull::Rect>,
        search: Option<&PmlSearchHighlight>,
        selection: Option<&select::PmlSelection>,
        selected_object: Option<&(usize, String)>,
    ) -> Result<(), JsValue> {
        let pages = pml_pages.borrow();
        let page = pages
            .get(idx)
            .ok_or_else(|| JsValue::from_str("page index out of range"))?;

        // The whole slide paints through the injected `p: &dyn Painter` (canvas
        // backend for live render, recording backend for the off-thread display
        // list) — mirroring `paint_wml_page`. The painter owns image resolution
        // (`draw_image` looks up its bitmap cache) and the pattFill tile cache; the
        // gradient/pattern/dash helpers END in the matching `p.set_*` trait method
        // (canvas builds the real gradient/pattern/dash; recorder emits an opcode).

        // Slide background: the resolved `cSld/bg/bgPr` fill (slide → layout →
        // master; solid or linear/radial gradient), else white. A blip bg still
        // falls back to white.
        // The ONE slide-spanning background fill is reduced to the window ∩ slide
        // box under a windowed paint (`cull::bg_rect`) so its device extent stays
        // bounded by the SLICE at high zoom (offset alone leaves it slide-sized and
        // still overflows Skia). Gradient/radial keep their FULL-slide axis (the
        // gradient is sampled correctly over any sub-rect); only the fill rect
        // shrinks. `clip == None` → full slide box, byte-identical.
        let bg = crate::cull::bg_rect(clip, page.width, page.height);
        match &page.bg {
            Some(shape::SlideBg::Solid(hex)) => {
                p.set_fill_style_str(hex);
                if let Some((bx, by, bw, bh)) = bg {
                    p.fill_rect(bx, by, bw, bh);
                }
            }
            Some(shape::SlideBg::Gradient { stops, angle_deg }) => {
                if paint_bg_gradient(p, page.width, page.height, stops, *angle_deg, bg).is_err() {
                    // Gradient surface unavailable → first stop as a solid (the
                    // dominant colour beats white-on-white).
                    let hex = stops.first().map(|(_, h)| h.as_str()).unwrap_or("#ffffff");
                    p.set_fill_style_str(hex);
                    if let Some((bx, by, bw, bh)) = bg {
                        p.fill_rect(bx, by, bw, bh);
                    }
                }
            }
            Some(shape::SlideBg::Radial { stops, focus }) => {
                if paint_bg_radial(p, page.width, page.height, stops, *focus, bg).is_err() {
                    // Radial surface unavailable → the LAST stop as a solid (it
                    // covers most of a corner-focused page, so it is the dominant
                    // colour) — beats white-on-white.
                    let hex = stops.last().map(|(_, h)| h.as_str()).unwrap_or("#ffffff");
                    p.set_fill_style_str(hex);
                    if let Some((bx, by, bw, bh)) = bg {
                        p.fill_rect(bx, by, bw, bh);
                    }
                }
            }
            None => {
                p.set_fill_style_str("#ffffff");
                if let Some((bx, by, bw, bh)) = bg {
                    p.fill_rect(bx, by, bw, bh);
                }
            }
        }
        p.set_text_baseline("alphabetic");

        // Pictures first (behind the placeholder shapes, §19.3.1.x `p:pic`). The
        // painter resolves the slide-namespaced image key: a decoded `ImageBitmap`
        // draws at the picture's box, a miss draws the shared grey placeholder (the
        // canvas backend does the lookup; the recorder records the key + the
        // main-thread replay resolves it).
        for pic in &page.pictures {
            if !crate::cull::keep(clip, pic.x, pic.y, pic.x + pic.w, pic.y + pic.h) {
                continue;
            }
            let _ = p.draw_image(&pic.image_key, pic.x, pic.y, pic.w, pic.h);
        }
        p.set_text_baseline("alphabetic");

        // Running slide-global search-match counter, advanced in `search_pml_slide`
        // order (all shapes' paragraphs, then all table cells') so the active-match
        // emphasis can recover each match's global index. Advanced even for CULLED
        // shapes/cells (via `count_paragraph_matches`) so the index never drifts.
        let mut matched_so_far: usize = 0;
        for sh in &page.shapes {
            // Windowed cull: skip a shape whose box (and its text, which is inset
            // INSIDE the box) misses the window. The culled shape's matches still
            // advance the global counter (cull-blind numbering in search_pml_slide).
            if !crate::cull::keep(clip, sh.x, sh.y, sh.x + sh.w, sh.y + sh.h) {
                matched_so_far += count_paragraph_matches(&sh.paragraphs, search);
                continue;
            }
            if let Some(cmds) = &sh.geometry {
                // The preset-geometry path painter — ONE path, filled AND
                // stroked (§20.1.9.18). Commands are shape-local px; save/
                // translate puts them at the box (and makes future rot/flipH/
                // flipV free). Holes (donut/frame) carry reversed-winding
                // inner contours, so the default nonzero fill leaves them empty.
                p.save();
                let _ = p.translate(sh.x, sh.y);
                // A 3-D preset (cube/can/bevel) with a SOLID base fill paints
                // its per-face shaded regions (painter's order) instead of the
                // single flat geometry fill — each face's base colour scaled by
                // its shade multiplier. A gradient/none base fill has no single
                // colour to shade, so we fall through to the flat silhouette fill.
                let painted_faces = match (&sh.faces, sh.solid_fill_hex()) {
                    (Some(faces), Some(base)) if !faces.is_empty() => {
                        for face in faces {
                            p.begin_path();
                            trace_path(p, &face.path);
                            let hex = crate::engine::color::shade_hex(base, face.shade);
                            p.set_fill_style_str(&hex);
                            p.fill();
                        }
                        true
                    }
                    _ => false,
                };
                p.begin_path();
                trace_path(p, cmds);
                // Solid → fillStyle string; gradient → a canvas linear
                // gradient spanning the SHAPE box (shape-local space after the
                // translate). Gradient-surface miss → first stop as a solid.
                if !painted_faces && set_shape_fill_style(p, &sh.fill, 0.0, 0.0, sh.w, sh.h) {
                    p.fill();
                }
                if let Some(out) = &sh.outline {
                    p.set_stroke_style_str(&out.color);
                    p.set_line_width(out.width_px);
                    // Dashed stroke — scale the canonical line-width-multiple
                    // array by the pen width, then clear it after so the next
                    // shape strokes solid.
                    apply_line_dash(p, out.dash.as_deref(), out.width_px);
                    p.stroke();
                    p.clear_line_dash();
                }
                p.restore();
            } else {
                // Fallback: no prstGeom / unparseable custGeom / unknown
                // preset → the honest rectangle.
                #[cfg(feature = "debug-probe")]
                if let Some(name) = &sh.unknown_preset {
                    crate::console_log(&format!(
                        "pml geom: unknown preset '{name}' → rect fallback"
                    ));
                }
                // Shape fill behind the text (solid or box-spanning
                // linear gradient).
                if set_shape_fill_style(p, &sh.fill, sh.x, sh.y, sh.w, sh.h) {
                    p.fill_rect(sh.x, sh.y, sh.w, sh.h);
                }
                // Solid outline stroked on the box edge (§20.1.2.2.24). Inset by
                // half the stroke so the line sits on the box rather than
                // straddling it.
                if let Some(out) = &sh.outline {
                    p.set_stroke_style_str(&out.color);
                    p.set_line_width(out.width_px);
                    apply_line_dash(p, out.dash.as_deref(), out.width_px);
                    let half = out.width_px / 2.0;
                    p.stroke_rect(
                        sh.x + half,
                        sh.y + half,
                        sh.w - out.width_px,
                        sh.h - out.width_px,
                    );
                    p.clear_line_dash();
                }
            }

            // The inset text box (bodyPr@lIns/tIns/rIns/bIns): wrapping fills
            // `text_w`, the text origin sits inside the insets, and the vertical
            // anchor offsets the WRAP-AWARE block (bodyPr@anchor). The
            // formula is the shared `shape_text_box` so paint + the text
            // selection reads use identical geometry.
            let (text_x, text_y, text_w) = shape_text_box(sh, font_cache);
            matched_so_far += paint_paragraph_stack(
                p, &sh.paragraphs, text_x, text_y, text_w, sh.wrap, font_cache, search,
                matched_so_far,
            )?;
        }

        // Connectors (§19.3.1.19 `p:cxnSp`) paint after the shapes: stroke
        // the connector path (with its dash), then fill the head/tail
        // arrowhead triangles with the SAME line colour. Each is shape-local px
        // at the connector box, so save/translate like a shape (and clear the
        // dash before filling the heads so the arrowheads stay solid).
        for cn in &page.connectors {
            p.save();
            let _ = p.translate(cn.x, cn.y);
            p.begin_path();
            trace_path(p, &cn.path);
            p.set_stroke_style_str(&cn.outline.color);
            p.set_line_width(cn.outline.width_px);
            apply_line_dash(p, cn.outline.dash.as_deref(), cn.outline.width_px);
            p.stroke();
            p.clear_line_dash();
            // Arrowheads: filled triangles in the line colour.
            p.set_fill_style_str(&cn.outline.color);
            for head in [&cn.head_arrow, &cn.tail_arrow].into_iter().flatten() {
                p.begin_path();
                trace_path(p, head);
                p.fill();
            }
            p.restore();
        }

        // Tables paint after the placeholder shapes (the z-order floor). Each cell:
        // solid fill → 4 solid borders → inset text (DML/Excel paint order).
        for tbl in &page.tables {
            for cell in &tbl.cells {
                // Windowed cull per cell (a table is a grid of independently-boxed
                // cells; the text is inset inside the cell box). A culled cell's
                // matches still advance the global counter (cull-blind numbering).
                if !crate::cull::keep(clip, cell.x, cell.y, cell.x + cell.w, cell.y + cell.h) {
                    matched_so_far += count_paragraph_matches(&cell.paragraphs, search);
                    continue;
                }
                if let Some(fill) = &cell.fill_hex {
                    p.set_fill_style_str(fill);
                    p.fill_rect(cell.x, cell.y, cell.w, cell.h);
                }
                // Four edge borders, each a solid stroke along the cell edge.
                paint_cell_edges(p, cell);

                // Cell text: PowerPoint always wraps cell text to the inset
                // text-box width (`text_w`), anchored vertically within the cell
                // (top/centre/bottom). The formula is the shared
                // `cell_text_box` so paint + the selection reads agree.
                let (cell_x, top, cell_w) = cell_text_box(cell, font_cache);
                matched_so_far += paint_paragraph_stack(
                    p,
                    &cell.paragraphs,
                    cell_x,
                    top,
                    cell_w,
                    shape::WrapMode::Square,
                    font_cache,
                    search,
                    matched_so_far,
                )?;
            }
        }

        // Charts paint with the tables (after the placeholder shapes — the
        // z-order floor). The raw chart part XML is the standard DML
        // `c:chartSpace` (§21.2), so the SHARED `crate::chart::render_chart`
        // renders it AS-IS at the graphicFrame box. A chart whose bytes are
        // missing / unparseable / has empty series draws nothing (render_chart
        // returns false — honest, no placeholder box).
        {
            let charts = chart_xml.borrow();
            for ch in &page.charts {
                if !crate::cull::keep(clip, ch.x, ch.y, ch.x + ch.w, ch.y + ch.h) {
                    continue;
                }
                if let Some(bytes) = charts.get(&ch.chart_key) {
                    let _ = crate::chart::render_chart(bytes, p, ch.x, ch.y, ch.w, ch.h);
                }
            }
        }

        // Selection overlay — the ADDITIVE selection passes, painted LAST so they
        // read over the glyphs (translucent fill) / around the picture (outline).
        // Each fires only when a selection/object is set for THIS slide, so a
        // no-selection render emits zero extra draws and stays byte-identical.
        select::paint_selection_pass(p, page, idx, selection, font_cache);
        select::paint_object_outline_pass(p, page, idx, selected_object);
        Ok(())
    }
}

/// Walk a [`engine::geom::PathCmd`] list onto the canvas (the geometry path painter).
/// Commands are shape-local px — the caller has already translated the ctx to
/// the shape origin. `Arc` maps to `ellipse()` (start/sweep degrees → start/end
/// radians, negative sweep = anticlockwise); the canvas draws the implicit
/// line from the current point to the arc start, which the builders rely on
/// (pie radii, blockArc ends).
fn trace_path(p: &dyn Painter, cmds: &[crate::engine::geom::PathCmd]) {
    use crate::engine::geom::PathCmd;
    for c in cmds {
        match *c {
            PathCmd::MoveTo { x, y } => p.move_to(x, y),
            PathCmd::LineTo { x, y } => p.line_to(x, y),
            PathCmd::Cubic { x1, y1, x2, y2, x, y } => p.bezier_curve_to(x1, y1, x2, y2, x, y),
            PathCmd::Quad { x1, y1, x, y } => p.quadratic_curve_to(x1, y1, x, y),
            PathCmd::Arc { cx, cy, rx, ry, start_deg, sweep_deg } => {
                let _ = p.ellipse_with_anticlockwise(
                    cx,
                    cy,
                    rx.max(0.0),
                    ry.max(0.0),
                    0.0,
                    start_deg.to_radians(),
                    (start_deg + sweep_deg).to_radians(),
                    sweep_deg < 0.0,
                );
            }
            PathCmd::Close => p.close_path(),
        }
    }
}

/// One laid-out visual line: the styled sub-runs painted on a single baseline,
/// plus that line's vertical metrics (tallest run's ascent / natural height)
/// and its left indent (`marL + indent` for the first/bullet line,
/// `marL` for wrapped continuations; 0.0 for un-indented paragraphs, the
/// pre-indent layout). A wrapped paragraph yields several of these; an unwrapped
/// one yields exactly one (the whole paragraph's runs).
struct LineLayout {
    runs: Vec<SlideRun>,
    ascent: f64,
    height: f64,
    /// Left offset in px from the text-box edge (§21.1.2.2.7 marL/indent).
    indent: f64,
}

impl LineLayout {
    /// Total horizontal advance of this line's sub-runs (for alignment).
    fn advance<'f>(&self, font_cache: &'f FontCache, faces: &FaceCache<'f>) -> f64 {
        self.runs.iter().map(|r| measure_run_advance(r, font_cache, faces)).sum()
    }
}

/// A break unit: a maximal chunk of a run that the greedy line filler treats as
/// atomic. `breakable_before` marks whether a line break may be inserted BEFORE
/// this unit (a CJK ideograph, or a Latin word that follows whitespace). The
/// first unit of a paragraph is never breakable-before.
struct BreakUnit {
    text: String,
    breakable_before: bool,
}

/// Tokenize a string into greedy break units per a tier-1 UAX#14 + CJK model:
///   - each East-Asian ideograph is its own unit, breakable before it (CJK text
///     breaks between any two characters);
///   - a run of Latin/other text up to and including its trailing spaces is one
///     unit; a break may occur before a unit that STARTS a new word after
///     whitespace (don't break mid-word). Leading whitespace attaches to the
///     following word so trailing spaces never start a line.
///     `first_overall` suppresses breakable-before on the very first unit so a
///     paragraph never begins with an empty leading line.
fn break_units(text: &str, first_overall: bool) -> Vec<BreakUnit> {
    use crate::engine::fonts::{font_slot_class, FontSlot};
    let mut units: Vec<BreakUnit> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if matches!(font_slot_class(c), FontSlot::EastAsia) {
            // One ideograph per unit; break allowed before it (unless first).
            let breakable = !(units.is_empty() && first_overall);
            units.push(BreakUnit { text: c.to_string(), breakable_before: breakable });
            i += 1;
            continue;
        }
        // Latin/other word: optional leading whitespace + the word body up to the
        // next whitespace or CJK boundary, then any trailing whitespace (so the
        // word and its following spaces stay together — a trailing space never
        // starts a line). A break may precede this unit whenever it follows a
        // preceding unit (a real word boundary; the previous unit ended at a
        // space or a CJK glyph) and it is not the paragraph's first unit.
        let word_start = i;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len()
            && !chars[i].is_whitespace()
            && !matches!(font_slot_class(chars[i]), FontSlot::EastAsia)
        {
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let unit_text: String = chars[word_start..i].iter().collect();
        let breakable = !(units.is_empty() && first_overall);
        units.push(BreakUnit { text: unit_text, breakable_before: breakable });
    }
    units
}

/// Lay out one paragraph into visual lines. `wrap = Square` greedily fills break
/// units until the next would exceed the line's available width, then breaks at
/// the last valid opportunity (a run may split across lines, each sub-run
/// keeping its parent's style). `wrap = None` (or non-positive `avail_w`) emits
/// a single line of all runs. The bullet prefix (if any) is prepended to the
/// first line and counts toward its width.
///
/// Indent: the first (bullet) line is inset by `para.indent_first_px`
/// (`marL + indent` — the hanging-bullet position) and wrapped continuation
/// lines by `para.indent_rest_px` (`marL` — the text edge), each reducing that
/// line's available width. Un-indented paragraphs (both 0.0) lay out exactly as
/// before.
fn layout_paragraph_lines<'f>(
    para: &shape::SlideParagraph,
    avail_w: f64,
    wrap: shape::WrapMode,
    font_cache: &'f FontCache,
    faces: &FaceCache<'f>,
) -> Vec<LineLayout> {
    // The bullet (char glyph or formatted autoNum ordinal) is drawn as a prefix
    // run cloned from the first run's style.
    let bullet_prefix = para
        .bullet
        .as_ref()
        .zip(para.runs.first())
        .map(|(glyph, r0)| {
            let mut p = r0.clone();
            p.text = format!("{glyph} ");
            p
        });

    // Clamp the indents so an over-wide authored marL never produces a negative
    // line box (the line then starts at the box edge and overflows honestly).
    let ind_first = para.indent_first_px.clamp(0.0, avail_w.max(0.0));
    let ind_rest = para.indent_rest_px.clamp(0.0, avail_w.max(0.0));

    // No-wrap (or no usable width): one line, all runs (bullet first).
    if wrap == shape::WrapMode::None || avail_w <= 0.0 {
        let mut runs = Vec::new();
        if let Some(b) = bullet_prefix {
            runs.push(b);
        }
        runs.extend(para.runs.iter().cloned());
        let (ascent, height) = line_metrics(&runs, para, font_cache);
        return vec![LineLayout { runs, ascent, height, indent: ind_first }];
    }

    // Greedy fill across all runs. The pen tracks the current line's used width;
    // each run is tokenized into break units that carry their parent style. The
    // first line fills `avail_w - ind_first`; continuations `avail_w - ind_rest`.
    let mut lines: Vec<Vec<SlideRun>> = Vec::new();
    let mut cur: Vec<SlideRun> = Vec::new();
    let mut cur_w = 0.0_f64;
    let mut cur_avail = avail_w - ind_first;
    let mut first_unit = true;

    // Push a unit's text onto the current line as a sub-run of `style`, merging
    // with the trailing sub-run when it shares the same style source.
    let emit = |cur: &mut Vec<SlideRun>, style: &SlideRun, text: &str| {
        cur.push({
            let mut r = style.clone();
            r.text = text.to_string();
            r
        });
    };

    // Seed the line with the bullet prefix (atomic, never broken).
    if let Some(b) = &bullet_prefix {
        cur.push(b.clone());
        cur_w += measure_run_advance(b, font_cache, faces);
        first_unit = false;
    }

    for run in &para.runs {
        for unit in break_units(&run.text, first_unit) {
            first_unit = false;
            let mut probe = run.clone();
            probe.text = unit.text.clone();
            let uw = measure_run_advance(&probe, font_cache, faces);
            // Break before this unit when it would overflow AND a break is
            // permitted here AND the line already has content (never emit an
            // empty line — an over-long single unit stays put and overflows).
            if unit.breakable_before && !cur.is_empty() && cur_w + uw > cur_avail {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0.0;
                cur_avail = avail_w - ind_rest;
                // A word's leading space is dropped at a line start.
                let trimmed = unit.text.trim_start();
                if !trimmed.is_empty() {
                    let mut p = run.clone();
                    p.text = trimmed.to_string();
                    let tw = measure_run_advance(&p, font_cache, faces);
                    emit(&mut cur, run, trimmed);
                    cur_w += tw;
                }
                continue;
            }
            emit(&mut cur, run, &unit.text);
            cur_w += uw;
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }

    lines
        .into_iter()
        .enumerate()
        .map(|(i, runs)| {
            let (ascent, height) = line_metrics(&runs, para, font_cache);
            let indent = if i == 0 { ind_first } else { ind_rest };
            LineLayout { runs, ascent, height, indent }
        })
        .collect()
}

/// A line's vertical metrics: the tallest sub-run's ascent and natural height.
/// An empty line (e.g. a blank paragraph) falls back to the paragraph's first
/// run, else a zero box — so a blank paragraph still advances by one line.
fn line_metrics(
    runs: &[SlideRun],
    para: &shape::SlideParagraph,
    font_cache: &FontCache,
) -> (f64, f64) {
    let from = |rs: &[SlideRun]| {
        rs.iter()
            .map(|r| shape::run_line_metrics(r, font_cache))
            .fold((0.0_f64, 0.0_f64), |(a, h), (ra, rh)| (a.max(ra), h.max(rh)))
    };
    if !runs.is_empty() {
        from(runs)
    } else {
        from(&para.runs)
    }
}

// Paragraph stacks (shared by slide shapes and table cells — a cell's text is
// the same DML `TextBody`) stack on baselines from the top of a `box_w`-wide
// text box, each visual line placed horizontally per its `align`. `wrap =
// Square` breaks each paragraph's runs into lines that fit `box_w`; `wrap =
// None` keeps one line per paragraph.
/// Count the search matches a paragraph list contributes to the slide-global
/// tally — the SAME per-paragraph `find_matches` over the runs-concatenated
/// logical text that `search_pml_slide` and `paint_paragraph_stack` use. Needed
/// so a window-CULLED shape/cell (whose `paint_paragraph_stack` never runs) still
/// advances the running global match index, keeping it in lockstep with the
/// (cull-blind) `search_pml_slide` numbering. Returns 0 when no search is active.
fn count_paragraph_matches(
    paragraphs: &[shape::SlideParagraph],
    search: Option<&PmlSearchHighlight>,
) -> usize {
    let Some(sh) = search else { return 0 };
    if sh.needle.is_empty() {
        return 0;
    }
    paragraphs
        .iter()
        .map(|para| {
            let logical: String = para.runs.iter().map(|r| r.text.as_str()).collect();
            crate::find_matches(&logical, sh.needle, sh.opts).len()
        })
        .sum()
}

/// Paint a paragraph stack and, when `search` is set, the search highlights over
/// it. `match_base` is the number of slide-global search matches consumed by
/// EARLIER shapes/cells (in `search_pml_slide`'s shapes-then-cells order), so the
/// running per-paragraph match ordinal recovers the slide-global index used by
/// the active-match emphasis. Returns the number of matches this call processed,
/// so the caller can advance its accumulator for the next shape/cell. (Returns 0
/// when `search` is `None`.)
#[allow(clippy::too_many_arguments)]
fn paint_paragraph_stack(
    p: &dyn Painter,
    paragraphs: &[shape::SlideParagraph],
    box_x: f64,
    box_y: f64,
    box_w: f64,
    wrap: shape::WrapMode,
    font_cache: &FontCache,
    search: Option<&PmlSearchHighlight>,
    match_base: usize,
) -> Result<usize, JsValue> {
    // This stack's shaping scope (search-highlight x measurement).
    let faces = FaceCache::new();
    // Slide-global running match index: `match_base` + matches seen so far in THIS
    // call. Incremented once per logical match (per `(ms,me)`), NOT per covered
    // sub-run — a multi-run match is ONE match in `search_pml_slide`, and all its
    // rects take the active styling together (the WML invariant).
    let mut global_mi = match_base;
    // The line geometry is the SHARED `walk_paragraph_stack`
    // (the same walk the text-selection reads consume). Paint iterates its
    // per-sub-run boxes in the identical line/run order, and its x-advance is
    // `measure_run_advance` — exactly what `paint_run_segments` advances the pen
    // by — so the glyph draw sequence is byte-identical to a dedicated paint loop.
    let geoms = walk_paragraph_stack(paragraphs, box_x, box_y, box_w, wrap, font_cache);
    for (geom, para) in geoms.iter().zip(paragraphs) {
        // Paint each sub-run's glyphs at its walked left edge + baseline.
        for wr in &geom.runs {
            paint_run_segments(p, &wr.run, wr.x0, wr.baseline, font_cache, &faces)?;
        }
        // ── Search highlight (full-text search) ──────────────────────────────
        // Match the paragraph's LOGICAL text, then paint a rect SLICED to the
        // matched characters within each covered sub-run (run_width_to_byte) —
        // so the highlight hugs the search TERM, not the whole word (find
        // standard), painted AFTER the glyphs as a marker. Read-only; reuses the
        // UTF-8-safe find_matches. The focused (active) match paints hotter — see
        // the per-match `is_active` swap below (the WML active-emphasis twin).
        //
        // The walked logical is BULLET-PREFIXED ("• " / "1. "), but
        // `search_pml_slide` and `count_paragraph_matches` number matches over
        // the RAW runs — so the bullet prefix is EXCLUDED here (matched on the
        // suffix, offsets shifted back) to keep the slide-global active index
        // in lockstep. Find never matches list numbering (the Word/PowerPoint
        // standard); #62's real "1." ordinals made this collision likely where
        // the old '•' glyph rarely did.
        if let Some(sh) = search {
            let prefix_len = match (&para.bullet, para.runs.is_empty()) {
                (Some(b), false) => b.len() + 1, // the "{glyph} " prefix run
                _ => 0,
            };
            let searchable = geom.logical.get(prefix_len..).unwrap_or("");
            if !sh.needle.is_empty() && !searchable.is_empty() {
                for (ms, me) in crate::find_matches(searchable, sh.needle, sh.opts) {
                    let (ms, me) = (ms + prefix_len, me + prefix_len);
                    // The focused (active) match paints hotter — ALL its sub-run
                    // rects together. `global_mi` is the slide-global index this
                    // match was assigned by `search_pml_slide`.
                    let is_active = sh.active == Some(global_mi);
                    let (fill, alpha) = if is_active {
                        (crate::SEARCH_ACTIVE_COLOR, crate::SEARCH_ACTIVE_ALPHA)
                    } else {
                        (sh.fill, sh.alpha)
                    };
                    for wr in &geom.runs {
                        if wr.byte_start < me && wr.byte_end > ms {
                            let rel_start = ms.saturating_sub(wr.byte_start);
                            let rel_end = me.min(wr.byte_end) - wr.byte_start;
                            let hx0 = wr.x0 + run_width_to_byte(&wr.run, rel_start, font_cache, &faces);
                            let hx1 = wr.x0 + run_width_to_byte(&wr.run, rel_end, font_cache, &faces);
                            if hx1 > hx0 {
                                p.save();
                                p.set_global_alpha(alpha);
                                p.set_fill_style_str(fill);
                                p.fill_rect(hx0, wr.top, hx1 - hx0, wr.height);
                                p.restore();
                            }
                        }
                    }
                    global_mi += 1;
                }
            }
        }
    }
    Ok(global_mi - match_base)
}

/// Total stacked height (px) of a paragraph list once wrapped to `box_w` (the
/// sum of every visual line's natural height), used to vertically anchor the
/// text block (shape `bodyPr@anchor` / cell anchor). `wrap = None` collapses to
/// one line per paragraph (no wrapping).
fn table_block_height(
    paragraphs: &[shape::SlideParagraph],
    box_w: f64,
    wrap: shape::WrapMode,
    font_cache: &FontCache,
) -> f64 {
    // One shaping scope for the whole cell measurement.
    let faces = FaceCache::new();
    paragraphs
        .iter()
        .flat_map(|para| layout_paragraph_lines(para, box_w, wrap, font_cache, &faces))
        .map(|line| line.height)
        .sum()
}

// ─── Shared text-box geometry + line walk ────────────────────────────────────
//
// The inset text-box origin/width for a shape or table cell, and the paint-order
// line walk that yields per-sub-run boxes. BOTH the paint (`render_page_clipped`
// / `paint_paragraph_stack`) AND the selection reads (`pml::select`) call these,
// so their geometry can never drift — the byte-identity guarantee (the paint
// consumes the SAME walk, and the walk's x-advance is `measure_run_advance`,
// which is exactly what `paint_run_segments` advances the pen by).

/// The inset text box `(text_x, text_y, text_w)` for a shape's paragraph stack —
/// the bodyPr insets plus the vertical anchor offset (§21.1.2.1.1). Extracted
/// verbatim from `render_page_clipped`'s per-shape block so paint + selection
/// share one formula.
pub(crate) fn shape_text_box(sh: &shape::SlideShape, font_cache: &FontCache) -> (f64, f64, f64) {
    let text_x = sh.x + sh.insets.left;
    let text_w = (sh.w - sh.insets.left - sh.insets.right).max(0.0);
    let avail_h = (sh.h - sh.insets.top - sh.insets.bottom).max(0.0);
    let band_top = sh.y + sh.insets.top;
    let text_y = match sh.anchor {
        shape::VAnchor::Top => band_top,
        shape::VAnchor::Center => {
            let block_h = table_block_height(&sh.paragraphs, text_w, sh.wrap, font_cache);
            band_top + ((avail_h - block_h) / 2.0).max(0.0)
        }
        shape::VAnchor::Bottom => {
            let block_h = table_block_height(&sh.paragraphs, text_w, sh.wrap, font_cache);
            band_top + (avail_h - block_h).max(0.0)
        }
    };
    (text_x, text_y, text_w)
}

/// The inset text box `(text_x, text_y, text_w)` for a table cell's paragraph
/// stack — the cell inset origin plus the vertical anchor offset (§21.1.3.17).
/// Extracted verbatim from `render_page_clipped`'s per-cell block. Cell text
/// always wraps (`WrapMode::Square`).
pub(crate) fn cell_text_box(cell: &table::TableCell, font_cache: &FontCache) -> (f64, f64, f64) {
    let block_h =
        table_block_height(&cell.paragraphs, cell.text_w, shape::WrapMode::Square, font_cache);
    let avail = (cell.h - (cell.text_y - cell.y) - (cell.text_y - cell.y)).max(0.0);
    let top = match cell.anchor {
        table::CellAnchor::Top => cell.text_y,
        table::CellAnchor::Center => cell.text_y + ((avail - block_h) / 2.0).max(0.0),
        table::CellAnchor::Bottom => cell.text_y + (avail - block_h).max(0.0),
    };
    (cell.text_x, top, cell.text_w)
}

/// One painted sub-run's geometry + style within a shape/cell text stack — the
/// shared unit of [`walk_paragraph_stack`]. `byte_start`/`byte_end` index the
/// paragraph's PAINTED logical text (the run-concatenation the search path and
/// the selection reads agree on). `x0`/`x1` are the sub-run's left/right edges,
/// `baseline` its glyph baseline, `top`/`height` its highlight band.
pub(crate) struct WalkRun {
    pub(crate) byte_start: usize,
    pub(crate) byte_end: usize,
    pub(crate) x0: f64,
    pub(crate) x1: f64,
    pub(crate) baseline: f64,
    pub(crate) top: f64,
    pub(crate) height: f64,
    pub(crate) run: SlideRun,
}

/// One paragraph's walked geometry: its PAINTED logical text (post-wrap,
/// bullet-prefixed) plus the per-sub-run boxes in paint (line, then run) order.
pub(crate) struct WalkPara {
    pub(crate) logical: String,
    pub(crate) runs: Vec<WalkRun>,
}

/// Walk a paragraph stack into per-sub-run geometry WITHOUT painting — the
/// geometry half of `paint_paragraph_stack`, extracted so paint and the
/// text-selection reads share one line-layout. The x-advance is
/// `measure_run_advance`, which is EXACTLY what `paint_run_segments` advances the
/// pen by, so the boxes tile the glyphs pixel-for-pixel. Baseline stacking
/// mirrors the paint loop verbatim (`+= ascent`, then `+= height − ascent`).
pub(crate) fn walk_paragraph_stack(
    paragraphs: &[shape::SlideParagraph],
    box_x: f64,
    box_y: f64,
    box_w: f64,
    wrap: shape::WrapMode,
    font_cache: &FontCache,
) -> Vec<WalkPara> {
    // One built face per (font, walked stack) instead of one per
    // greedy-fill probe. PML measures a whole break unit per candidate word, so
    // this is the format's densest `Face::from_slice` site.
    let faces = FaceCache::new();
    let mut out = Vec::with_capacity(paragraphs.len());
    let mut baseline_y = box_y;
    for para in paragraphs {
        let lines = layout_paragraph_lines(para, box_w, wrap, font_cache, &faces);
        let mut logical = String::new();
        let mut runs: Vec<WalkRun> = Vec::new();
        for line in &lines {
            baseline_y += line.ascent;
            let line_top = baseline_y - line.ascent;
            let line_w = line.advance(font_cache, &faces);
            // Indent: the line box starts at `box_x + line.indent` and is
            // `box_w - line.indent` wide; alignment places the line within it.
            // Un-indented lines (indent 0.0) keep the pre-indent geometry.
            let (line_x, line_avail) = (box_x + line.indent, box_w - line.indent);
            let mut pen_x = match para.align {
                shape::ParaAlign::Center => line_x + (line_avail - line_w) / 2.0,
                shape::ParaAlign::Right => line_x + (line_avail - line_w),
                shape::ParaAlign::Left | shape::ParaAlign::Justify => line_x,
            };
            for run in &line.runs {
                let run_x0 = pen_x;
                let run_x1 = pen_x + measure_run_advance(run, font_cache, &faces);
                pen_x = run_x1;
                let bs = logical.len();
                logical.push_str(&run.text);
                runs.push(WalkRun {
                    byte_start: bs,
                    byte_end: logical.len(),
                    x0: run_x0,
                    x1: run_x1,
                    baseline: baseline_y,
                    top: line_top,
                    height: line.height,
                    run: run.clone(),
                });
            }
            baseline_y += line.height - line.ascent;
        }
        out.push(WalkPara { logical, runs });
    }
    out
}

/// Stroke a table cell's four edge borders (each an optional line). A border
/// line is drawn ON the cell edge (the cell box is the grid line, not an
/// inset). Mirrors the shape-outline stroke discipline, INCLUDING the dash
/// (`tcPr/lnX/prstDash` §20.1.10.48 is honoured, not painted solid — via the
/// same `apply_line_dash`/`clear_line_dash` pair the shape outlines and
/// connectors already use).
fn paint_cell_edges(p: &dyn Painter, cell: &table::TableCell) {
    use crate::engine::border::{stroke_box_edges, Edge, EdgeSide};
    // Edges stroke corner-to-corner (no snap), in the order top, bottom, left,
    // right; PML additionally applies the line dash around each stroke.
    stroke_box_edges(
        cell.x,
        cell.y,
        cell.w,
        cell.h,
        [
            (EdgeSide::Top, cell.border_t.as_ref()),
            (EdgeSide::Bottom, cell.border_b.as_ref()),
            (EdgeSide::Left, cell.border_l.as_ref()),
            (EdgeSide::Right, cell.border_r.as_ref()),
        ],
        false,
        |o: &Edge, x0, y0, x1, y1| {
            p.set_stroke_style_str(&o.color);
            p.set_line_width(o.width_px);
            apply_line_dash(p, o.dash.as_deref(), o.width_px);
            p.begin_path();
            p.move_to(x0, y0);
            p.line_to(x1, y1);
            p.stroke();
            p.clear_line_dash();
        },
    );
}

/// Collect a slide's draw shapes + their group transforms (§19.3.1.6). The
/// result is the top-level `p:sp` (each with `GroupTransform::IDENTITY`) followed
/// by every `p:grpSp`-descendant `p:sp` paired with the CUMULATIVE transform from
/// the slide root to its enclosing group (nested groups compose multiplicatively).
/// `Slide::shapes()` returns ONLY the top-level `sp`, so the group descendants
/// (which `build_slide_page` previously dropped → blank group slides) are walked
/// off the spTree's `group_shape` list here. The two returned vecs are index-
/// aligned. Shapes are cloned (owned by `SlideData`, the same as the prior
/// `s.shapes().to_vec()`).
fn collect_slide_shapes(
    slide: &ooxml_pml::Slide,
) -> (Vec<ooxml_pml::types::Shape>, Vec<shape::GroupTransform>) {
    let mut shapes = Vec::new();
    let mut transforms = Vec::new();
    // Top-level shapes: identity transform (un-grouped).
    for sp in slide.shapes() {
        shapes.push(sp.clone());
        transforms.push(shape::GroupTransform::IDENTITY);
    }
    // Group descendants: flatten each top-level `p:grpSp`, threading the
    // root→group cumulative transform onto each descendant `p:sp`.
    let mut grouped: Vec<(&ooxml_pml::types::Shape, shape::GroupTransform)> = Vec::new();
    for g in &slide.inner().common_slide_data.shape_tree.group_shape {
        shape::flatten_group_shapes(g, shape::GroupTransform::IDENTITY, &mut grouped);
    }
    for (sp, xform) in grouped {
        shapes.push(sp.clone());
        transforms.push(xform);
    }
    (shapes, transforms)
}

/// Collect a slide's table draw items (DML `a:tbl` inside a `p:graphicFrame`,
/// §21.1.3), pairing each parsed table with its containing graphicFrame `a:xfrm`
/// box.
///
/// `Slide::tables()` returns the parsed `CTTable`s in graphicFrame order (one
/// per frame that contains a `tbl` in its `extra_children`), but NOT the frame
/// `xfrm` — the table's position/size on the slide. So we walk the
/// `shape_tree.graphic_frame` list ourselves, keep only the frames that contain
/// a table node, and zip them positionally with `tables()` (both visit the same
/// frames in the same order). The frame box comes from `frame.xfrm.offset/
/// extents` (`p:graphicFrame/p:xfrm`, EMU — `Transform2D` off/ext are String
/// coordinates). A frame whose xfrm omits off/ext yields a (0,0)-origin item
/// (the grid is still sized by gridCol/tr sums; honest).
fn slide_table_items(slide: &ooxml_pml::Slide) -> Vec<table::TableItem> {
    let parsed = slide.tables();
    if parsed.is_empty() {
        return Vec::new();
    }
    // The frames that contain a table node, in document order — paired 1:1 with
    // `parsed` (the fork's extractor walks these same frames in the same order).
    let frames: Vec<&ooxml_pml::types::GraphicalObjectFrame> = slide
        .inner()
        .common_slide_data
        .shape_tree
        .graphic_frame
        .iter()
        .filter(|f| frame_has_table(f))
        .collect();

    parsed
        .iter()
        .enumerate()
        .map(|(i, tbl)| {
            let xfrm = frames.get(i).map(|f| f.xfrm.as_ref());
            let (off_x, off_y) = xfrm
                .and_then(|x| x.offset.as_deref())
                .map(|o| (parse_emu(&o.x), parse_emu(&o.y)))
                .unwrap_or((0, 0));
            let (ext_cx, ext_cy) = xfrm
                .and_then(|x| x.extents.as_deref())
                .map(|e| (e.cx, e.cy))
                .unwrap_or((0, 0));
            table::TableItem {
                off_x,
                off_y,
                ext_cx,
                ext_cy,
                table: tbl.inner().clone(),
            }
        })
        .collect()
}

/// Collect a slide's chart draw items (a DML chart part referenced from a
/// `p:graphicFrame` via `c:chart@r:id`, §19.3.1.16 → §21.2), pairing each
/// chart's r:id with its containing graphicFrame `a:xfrm` box.
///
/// The fork's `Slide::chart_rel_ids()` returns the chart r:ids but NOT the frame
/// box (the table situation), so we walk the `shape_tree.graphic_frame` list
/// ourselves: for each frame whose `extra_children` contain a `c:chart` element,
/// capture its r:id + the frame `xfrm` off/ext. A frame may carry MULTIPLE
/// charts (rare) — we emit one item per `c:chart`, all sharing that frame's box.
/// The chart key is slide-namespaced (`chart::pml::{slide_idx}::{relId}`) because
/// rel ids restart per slide.
fn slide_chart_items(slide: &ooxml_pml::Slide, slide_idx: usize) -> Vec<shape::ChartItem> {
    if slide.chart_rel_ids().is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for frame in &slide.inner().common_slide_data.shape_tree.graphic_frame {
        let mut rel_ids = Vec::new();
        for n in &frame.extra_children {
            collect_chart_rel_ids(&n.node, &mut rel_ids);
        }
        if rel_ids.is_empty() {
            continue;
        }
        let xfrm = frame.xfrm.as_ref();
        let (off_x, off_y) = xfrm
            .offset
            .as_deref()
            .map(|o| (parse_emu(&o.x), parse_emu(&o.y)))
            .unwrap_or((0, 0));
        let (ext_cx, ext_cy) = xfrm
            .extents
            .as_deref()
            .map(|e| (e.cx, e.cy))
            .unwrap_or((0, 0));
        for rel in rel_ids {
            out.push(shape::ChartItem {
                off_x,
                off_y,
                ext_cx,
                ext_cy,
                chart_key: shape::chart_key(slide_idx, &rel),
            });
        }
    }
    out
}

/// Recursively collect `c:chart@r:id` (`r:id` of any `chart` element) from a raw
/// XML node tree — mirrors the fork's `collect_chart_and_smartart_ids` (chart
/// arm only; SmartArt `dgm:relIds` is deferred, no `pml-charts`). Namespace-
/// prefix agnostic on both the element local-name and the `id` attribute.
fn collect_chart_rel_ids(node: &ooxml_xml::RawXmlNode, out: &mut Vec<String>) {
    use ooxml_xml::RawXmlNode;
    if let RawXmlNode::Element(elem) = node {
        let local = elem.name.split(':').next_back().unwrap_or(&elem.name);
        if local == "chart" {
            for (attr_name, attr_val) in &elem.attributes {
                let attr_local = attr_name.split(':').next_back().unwrap_or(attr_name);
                if attr_local == "id" {
                    out.push(attr_val.clone());
                }
            }
        } else {
            for child in &elem.children {
                collect_chart_rel_ids(child, out);
            }
        }
    }
}

/// True when a graphicFrame's captured `extra_children` contain a table node
/// (`a:tbl`/`tbl`) — the same detection the fork's `find_table_in_node` uses to
/// decide a frame holds a table. Reading `extra_children` requires the
/// `extra-children` feature the `pml` feature arms.
fn frame_has_table(frame: &ooxml_pml::types::GraphicalObjectFrame) -> bool {
    frame
        .extra_children
        .iter()
        .any(|n| node_contains_table(&n.node))
}

/// Recursively search a raw XML node tree for a `tbl` element (namespace-prefix
/// agnostic — `a:tbl` or bare `tbl`).
fn node_contains_table(node: &ooxml_xml::RawXmlNode) -> bool {
    use ooxml_xml::RawXmlNode;
    match node {
        RawXmlNode::Element(elem) => {
            let local = elem.name.split(':').next_back().unwrap_or(&elem.name);
            if local == "tbl" {
                return true;
            }
            elem.children.iter().any(node_contains_table)
        }
        _ => false,
    }
}

/// Parse an `STCoordinate` (a String EMU value — `Transform2D` off/ext) to i64,
/// 0 on a non-numeric / absent value.
fn parse_emu(s: &str) -> i64 {
    s.trim().parse::<i64>().unwrap_or(0)
}

/// CSS `font` shorthand for a slide run (mirrors SML's `font_css` — same shape
/// so slide text renders identically to WML/SML body text).
fn font_css(run: &SlideRun) -> String {
    let s = if run.italic { "italic" } else { "normal" };
    let w = if run.bold { "bold" } else { "normal" };
    format!(
        "{s} {w} {size}pt \"{family}\", -apple-system, BlinkMacSystemFont, sans-serif",
        size = run.font_pt,
        family = run.family,
    )
}

/// A minimal `RunStyle` for `paint_glyph_run` (the fill colour + font are set on
/// the context by the caller; the effect flags are all off).
fn run_style(run: &SlideRun) -> RunStyle {
    RunStyle {
        bold: run.bold,
        italic: run.italic,
        font_pt: run.font_pt,
        family: run.family.clone(),
        family_east_asia: run.family_east_asia.clone(),
        family_complex: run.family_complex.clone(),
        run_is_complex: false,
        color: run.color.clone(),
        underline: Underline::None,
        strikethrough: false,
        double_strike: false,
        vert_align: VertAlign::Baseline,
        highlight: None,
        run_shading: None,
        char_spacing_px: 0.0,
        all_caps: false,
        small_caps: false,
        vanish: false,
        outline: false,
        shadow: false,
        emboss: false,
        imprint: false,
        baseline_shift_px: 0.0,
    }
}

/// Split a run's text into maximal contiguous segments sharing one §17.3.2.26
/// font slot, pairing each with the run's family for that slot (the PML
/// twin of `wml/linebreak.rs`'s per-segment slot resolution; PML borrows only
/// the segmentation, not the line breaker). PML runs carry no `<w:cs/>`/
/// `<w:rtl/>` equivalent, so step 2 resolves with `run_is_complex = false`
/// (EastAsia stays EastAsia; a step-1 ComplexScript char folds into the
/// ascii/default slot per the spec table).
fn run_slot_segments(run: &SlideRun) -> Vec<(String, String)> {
    use crate::engine::fonts::{font_slot_class, resolve_font_slot, FontSlot};
    let mut segs: Vec<(String, FontSlot)> = Vec::new();
    for c in run.text.chars() {
        let slot = match resolve_font_slot(font_slot_class(c), false) {
            FontSlot::EastAsia => FontSlot::EastAsia,
            FontSlot::ComplexScript => FontSlot::ComplexScript,
            // Ascii + HighAnsi both use the run's default/latin family.
            _ => FontSlot::Ascii,
        };
        match segs.last_mut() {
            Some((text, s)) if *s == slot => text.push(c),
            _ => segs.push((c.to_string(), slot)),
        }
    }
    segs.into_iter()
        .map(|(text, slot)| {
            let family = match slot {
                FontSlot::EastAsia => run.family_east_asia.clone(),
                FontSlot::ComplexScript => run.family_complex.clone(),
                _ => run.family.clone(),
            };
            (text, family)
        })
        .collect()
}

/// Paint one run as its font-slot segments: each segment sets ITS slot
/// family on the canvas and advances the pen by the SAME advance the measure
/// path computes — measured == painted, so a latin run never starts inside the
/// preceding CJK run's painted glyphs. Returns the advanced pen x.
fn paint_run_segments<'f>(
    p: &dyn Painter,
    run: &SlideRun,
    mut pen_x: f64,
    baseline_y: f64,
    font_cache: &'f FontCache,
    faces: &FaceCache<'f>,
) -> Result<f64, JsValue> {
    for (text, family) in run_slot_segments(run) {
        let mut seg = run.clone();
        seg.text = text;
        seg.family = family;
        let style = run_style(&seg);
        p.set_font(&font_css(&seg));
        p.set_fill_style_str(&seg.color);
        p.paint_glyph_run(&style, &seg.text, pen_x, baseline_y)?;
        pen_x += measure_segment_advance(&seg.text, &seg.family, run, font_cache, faces);
    }
    Ok(pen_x)
}

/// Measure a run's painted advance: the sum of its font-slot segments, each
/// shaped with its own slot face.
pub(crate) fn measure_run_advance<'f>(run: &SlideRun, font_cache: &'f FontCache, faces: &FaceCache<'f>) -> f64 {
    run_slot_segments(run)
        .iter()
        .map(|(text, family)| measure_segment_advance(text, family, run, font_cache, faces))
        .sum()
}

/// Width (px) from a slide run's left edge to byte offset `byte` within its text
/// — re-measures a clone truncated to `byte` (the runs aren't pre-shaped here).
/// Lets the search highlight slice to the matched chars, not the whole word.
/// `byte` must be a char boundary (search match offsets always are).
pub(crate) fn run_width_to_byte<'f>(run: &SlideRun, byte: usize, font_cache: &'f FontCache, faces: &FaceCache<'f>) -> f64 {
    if byte == 0 {
        return 0.0;
    }
    if byte >= run.text.len() {
        return measure_run_advance(run, font_cache, faces);
    }
    let mut prefix = run.clone();
    prefix.text = run.text[..byte].to_string();
    measure_run_advance(&prefix, font_cache, faces)
}

/// Measure one slot-homogeneous segment: shape with the loaded slot face when
/// available (accurate), else estimate — ~1.0 em/char for East Asian segments
/// (CJK glyphs are full-width, and the canvas system-font fallback paints them
/// at ~1 em, so the pen stays aligned with what's on screen even before the
/// CJK face loads) and ~0.5 em/char otherwise (the WML estimate).
fn measure_segment_advance<'f>(
    text: &str,
    family: &str,
    run: &SlideRun,
    font_cache: &'f FontCache,
    faces: &FaceCache<'f>,
) -> f64 {
    use crate::engine::fonts::{font_slot_class, FontSlot};
    let weight = if run.bold { 700 } else { 400 };
    if let Some(f) = font_cache
        .resolve(family, weight, run.italic)
        .or_else(|| font_cache.resolve(crate::engine::fonts::DEFAULT_FONT_FAMILY, 400, false))
    {
        if let Some(shaped) = faces.shape(text, f, run.font_pt) {
            return shaped.total_advance;
        }
    }
    let em = crate::engine::units::pt_to_px(run.font_pt);
    let per_char = if text
        .chars()
        .next()
        .map(|c| matches!(font_slot_class(c), FontSlot::EastAsia))
        .unwrap_or(false)
    {
        1.0
    } else {
        0.5
    };
    text.chars().count() as f64 * em * per_char
}

// ─── OPC self-parse (sldSz + per-slide layout placeholder geometry) ──────────

/// Resolve picture media bytes from the slide relationships through the load's
/// SHARED [`opc::OpcPackage`] (the rels cache means one slide's `.rels`
/// parses once no matter how many picture specs it carries). For each
/// `(image_key, slide_path, rel_id)` spec: read the slide's rels, find the embed
/// rel, resolve its target path, read the part, and derive the mime from the
/// extension. A spec whose rel/part is missing or unreadable is silently skipped
/// (the picture then paints a placeholder — honest). Deduplicates by key (a slide
/// reusing one image under several rel ids would still resolve each key once).
fn resolve_picture_images(
    pkg: Option<&mut opc::OpcPackage<'_>>,
    specs: &[(String, String, String)],
) -> Vec<(String, crate::engine::images::ImageBytes)> {
    if specs.is_empty() {
        return Vec::new();
    }
    let Some(pkg) = pkg else {
        return Vec::new();
    };

    let mut out = Vec::with_capacity(specs.len());
    let mut seen = HashSet::new();
    for (key, slide_path, rel_id) in specs {
        if !seen.insert(key.clone()) {
            continue;
        }
        let resolved = (|| {
            let rel = pkg.rels(slide_path).get(rel_id).cloned()?;
            let part_path = opc::resolve_target(slide_path, &rel.target);
            let data = pkg.read_part(&part_path)?;
            let mime = crate::engine::images::mime_from_path(&part_path);
            Some(crate::engine::images::ImageBytes { mime, data })
        })();
        if let Some(img) = resolved {
            out.push((key.clone(), img));
        }
    }
    out
}

/// Resolve each chart's raw `c:chartSpace` part XML from the slide rels through
/// the load's SHARED [`opc::OpcPackage`] (the same route `resolve_picture_images`
/// uses). For each `(chart_key, slide_path, rel_id)`: read the slide's rels,
/// find the chart rel, resolve its target path, read the part bytes. A spec
/// whose rel/part is missing/unreadable is silently skipped (the chart then
/// draws nothing — honest). The bytes are handed AS-IS to the shared
/// `crate::chart::render_chart` (the pptx chart part is the standard DML chart
/// schema, §21.2). Deduplicates by key.
fn resolve_chart_xml(
    pkg: Option<&mut opc::OpcPackage<'_>>,
    specs: &[(String, String, String)],
) -> Vec<(String, Vec<u8>)> {
    if specs.is_empty() {
        return Vec::new();
    }
    let Some(pkg) = pkg else {
        return Vec::new();
    };

    let mut out = Vec::with_capacity(specs.len());
    let mut seen = HashSet::new();
    for (key, slide_path, rel_id) in specs {
        if !seen.insert(key.clone()) {
            continue;
        }
        let resolved = (|| {
            let rel = pkg.rels(slide_path).get(rel_id).cloned()?;
            let part_path = opc::resolve_target(slide_path, &rel.target);
            pkg.read_part(&part_path)
        })();
        if let Some(data) = resolved {
            out.push((key.clone(), data));
        }
    }
    out
}

/// Parse every slide of a `.pptx`, returning them alongside the EFFECTIVE bytes
/// (original, or custGeom-repaired) and the ORIGINAL bytes. Dispatch-on-FAILURE
/// (RULE 3): parse the original bytes; on a missing-attribute failure, apply the
/// custGeom strip and re-parse ONCE over the repaired bytes. A non-missing-
/// attribute failure (or a repair that finds no custGeom) surfaces the original
/// error unchanged — the valid-file happy path is never touched.
///
/// This returns the PARSED SLIDES, not a fresh `Presentation` for the
/// caller to drain. `Presentation::slides()` is uncached (`load_slide` re-reads
/// and re-parses each part on every call), so the previous shape — probe with
/// `slides()`, drop, hand back a second `Presentation`, caller calls `slides()`
/// again — parsed every slide part TWICE. Measured on a 200-slide synthetic
/// deck: 20.2 ms for the probe+re-parse pair against 9.3 ms for one parse, i.e.
/// 10.9 ms (24% of the whole load) spent producing a value that was dropped. It
/// also held three copies of the deck bytes at once (caller + two `Cursor`
/// owners); now two.
#[allow(clippy::type_complexity)] // one-off return: (slides, effective, orig bytes)
fn parse_presentation_tolerant(
    bytes: Vec<u8>,
) -> Result<(Vec<ooxml_pml::Slide>, Vec<u8>, Vec<u8>), JsValue> {
    // First attempt on the original bytes.
    let first_err = match load_slides(bytes.clone()) {
        // No repair: effective == original; the original is returned for the
        // custGeom scan too (a valid deck simply has no `<a:custGeom>` to find).
        Ok(slides) => return Ok((slides, bytes.clone(), bytes)),
        Err(e) => e,
    };

    // Dispatch only on the missing-attribute class (the custGeom x-attr bug
    // surfaces as `Invalid content: x`). Other errors are real failures.
    if !is_missing_attr(&first_err) {
        return Err(JsValue::from_str(&format!("pptx parse failed: {first_err}")));
    }

    // Strip custGeom from every slide part, rebuild, re-parse once. The ORIGINAL
    // (pre-strip) bytes are kept so the custGeom pass can raw-scan the real `<a:custGeom>` paths
    // the strip just replaced with rect placeholders (RULE 3 stays — the fork
    // parse bug is live).
    let Some(repaired) = crate::repair::load_pptx_tolerant(&bytes) else {
        return Err(JsValue::from_str(&format!("pptx parse failed: {first_err}")));
    };
    match load_slides(repaired.clone()) {
        Ok(slides) => Ok((slides, repaired, bytes)),
        Err(e) => Err(crate::repair::pml_custgeom_error(e)),
    }
}

#[cfg(test)]
thread_local! {
    /// `Presentation::slides()` calls performed on THIS thread — the
    /// parse-once pin, mirroring [`opc::OPEN_COUNT`]'s discipline (thread-local,
    /// not a process atomic, so parallel tests never see each other's loads).
    /// A valid deck must reach exactly ONE; a RULE-3-repaired deck two (the
    /// failed original attempt, then the repaired one).
    pub(crate) static SLIDE_PARSE_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// Build a `Presentation` over `bytes` and parse every slide. A missing-attribute
/// slide-parse failure surfaces here, where [`parse_presentation_tolerant`]'s
/// dispatch can react to it. The `Presentation` (and its owning copy of the deck
/// bytes) is dropped on return — the slides are owned values.
fn load_slides(bytes: Vec<u8>) -> Result<Vec<ooxml_pml::Slide>, ooxml_pml::Error> {
    #[cfg(test)]
    SLIDE_PARSE_COUNT.with(|c| c.set(c.get() + 1));
    ooxml_pml::Presentation::from_reader(Cursor::new(bytes))?.slides()
}

/// `true` when `err` is the strict missing-required-attribute failure (the
/// custGeom x-attr codegen bug). The pml boundary maps
/// `ParseError::MissingAttribute` to `Error::Invalid(_)`.
fn is_missing_attr(err: &ooxml_pml::Error) -> bool {
    matches!(err, ooxml_pml::Error::Invalid(_))
}

/// Extract (1) the `p:sldSz` slide size and (2) per-slide inherited layout
/// placeholder geometry from the shared OPC view of the (possibly repaired)
/// bytes, in `p:sldIdLst` order so the result aligns index-for-index with the
/// fork's `Presentation::slides()`. Returns `None` on any structural read
/// error (the caller then uses defaults — graceful, mirroring the fork's
/// `if let Ok` tolerance for masters/layouts).
fn parse_opc_geometry(
    pkg: Option<&mut opc::OpcPackage<'_>>,
) -> Option<(i64, i64, Vec<LayoutGeom>, Vec<String>)> {
    let pkg = pkg?;

    // Root rels → presentation part path.
    let presentation_path = pkg.office_document_target()?;

    // presentation.xml → sldSz + slide rId order.
    let pres_xml = pkg.read_part(&presentation_path)?;
    let (cx, cy) = scan_sld_sz(&pres_xml);
    let slide_rids = scan_sld_id_lst(&pres_xml);

    // presentation rels → rId → slide part path. `try_rels`: a GARBLED
    // presentation `.rels` bails to full defaults (an absent one proceeds
    // empty) — a stricter posture than every other consumer's.
    let pres_rels = pkg.try_rels(&presentation_path)?;

    // Build (rId → slide path) for slide rels.
    let mut rid_to_slide: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for rel in pres_rels.iter() {
        if rel.relationship_type == REL_SLIDE {
            let path = opc::resolve_target(&presentation_path, &rel.target);
            rid_to_slide.insert(rel.id.clone(), path);
        }
    }

    // For each slide (in sldIdLst order) resolve its part path + layout part and
    // parse the placeholder geometry. Missing layout / unreadable part → empty
    // geom. `slide_paths` is collected in the SAME order so the caller can pair a
    // slide index to its part path (the fork's `slide_path()` is `pub(crate)`).
    let mut geoms = Vec::with_capacity(slide_rids.len());
    let mut slide_paths: Vec<String> = Vec::with_capacity(slide_rids.len());
    for rid in &slide_rids {
        let slide_path = rid_to_slide.get(rid).cloned().unwrap_or_default();
        slide_paths.push(slide_path.clone());
        let geom = (|| {
            if slide_path.is_empty() {
                return None;
            }
            let layout_rel = pkg.rels(&slide_path).get_by_type(REL_SLIDE_LAYOUT).cloned()?;
            let layout_path = opc::resolve_target(&slide_path, &layout_rel.target);
            let layout_xml = pkg.read_part(&layout_path)?;
            parse_layout_geom(&layout_xml)
        })()
        .unwrap_or_default();
        geoms.push(geom);
    }

    Some((cx, cy, geoms, slide_paths))
}

/// For each slide part (in `slide_paths` / sldIdLst order), raw-scan its
/// per-`<p:sp>` `<a:custGeom>` markup. `pkg` MUST be the package over the
/// ORIGINAL (pre-repair-strip) byte image — when a RULE-3 repair fired that is
/// a distinct `OpcPackage` from the effective one every other pass reads
/// (the two-package case); with no repair the caller hands the shared
/// effective package. Returns one `Vec<Option<String>>` per slide, keyed by
/// spTree document order so `result[slide][shapeIdx]` aligns with
/// `Slide::shapes()[shapeIdx]`. A missing/unreadable part → an empty vec
/// for that slide (every shape then keeps its rect). Never errors (graceful).
fn parse_opc_custgeom(
    pkg: Option<&mut opc::OpcPackage<'_>>,
    slide_paths: &[String],
) -> Vec<Vec<Option<String>>> {
    let mut out = Vec::with_capacity(slide_paths.len());
    let Some(pkg) = pkg else {
        return vec![Vec::new(); slide_paths.len()];
    };
    for path in slide_paths {
        let scanned = if path.is_empty() {
            Vec::new()
        } else {
            pkg.read_part_string(path)
                .map(|xml| scan_shape_custgeoms(&xml))
                .unwrap_or_default()
        };
        out.push(scanned);
    }
    out
}

/// Walk a slide part's `<p:spTree>` and return, per direct-child `<p:sp>` (in
/// document order — matching the fork's non-recursive `GroupShape::shapes()`),
/// the raw `<a:custGeom>…</a:custGeom>` slice it contains, or `None`. Shapes
/// nested inside a `<p:grpSp>` are NOT emitted (the fork's `shapes()` is
/// non-recursive; grouped shapes get no overlay), so we track group depth and
/// only capture top-level `<p:sp>`.
fn scan_shape_custgeoms(xml: &str) -> Vec<Option<String>> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    let mut buf = Vec::new();

    let mut out: Vec<Option<String>> = Vec::new();
    let mut grp_depth = 0i32; // nested <p:grpSp> depth (spTree itself excluded)
    // When inside a top-level <p:sp>, the byte offset where it began (to slice
    // its raw markup for a custGeom search).
    let mut sp_start: Option<usize> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                match e.local_name().as_ref() {
                    b"grpSp" => grp_depth += 1,
                    b"sp" if grp_depth == 0 && sp_start.is_none() => {
                        // Position of this <p:sp>'s '<' in the source.
                        sp_start = Some(reader.buffer_position() as usize - e.len() - 2);
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"grpSp" => grp_depth = (grp_depth - 1).max(0),
                b"sp" if grp_depth == 0 => {
                    if let Some(start) = sp_start.take() {
                        let end = reader.buffer_position() as usize;
                        let span = xml.get(start..end).unwrap_or("");
                        out.push(extract_custgeom(span));
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Slice the first `<a:custGeom>…</a:custGeom>` element out of a `<p:sp>` span,
/// or `None`. Numeric coordinates need no entity decoding (raw byte slice).
fn extract_custgeom(sp_xml: &str) -> Option<String> {
    let open = sp_xml.find("<a:custGeom")?;
    let after = &sp_xml[open..];
    after.find("</a:custGeom>").map(|close| after[..close + "</a:custGeom>".len()].to_string())
}

/// Parse a slide-layout part's XML into a [`LayoutGeom`] via the generated
/// `SlideLayout::from_xml`, walking `cSld/spTree` placeholders. Mirrors the
/// fork's `parse_slide_xml` reader-entry pattern (find the root element, hand the
/// start tag to the generated parser). Returns `None` on a parse error (caller
/// treats as empty geom — graceful, like the fork's layout tolerance).
fn parse_layout_geom(xml: &[u8]) -> Option<LayoutGeom> {
    use ooxml_pml::parsers::FromXml;
    use ooxml_pml::types::SlideLayout;
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_reader(Cursor::new(xml));
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e))
                if e.local_name().as_ref() == b"sldLayout" => {
                    let layout = SlideLayout::from_xml(&mut reader, &e, false).ok()?;
                    let shapes = &layout.common_slide_data.shape_tree.shape;
                    return Some(shape::layout_placeholder_geom(shapes));
                }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// Scan `<p:sldSz cx="…" cy="…">` (§19.2.1.39) out of the presentation part. A
/// raw byte scan suffices: `read_part` returns the INFLATED XML. Falls back to
/// the 4:3 default if absent/garbled.
fn scan_sld_sz(xml: &[u8]) -> (i64, i64) {
    let text = String::from_utf8_lossy(xml);
    let Some(start) = text.find("<p:sldSz") else {
        return (DEFAULT_SLD_CX_EMU, DEFAULT_SLD_CY_EMU);
    };
    let tag = &text[start..text[start..].find('>').map(|e| start + e).unwrap_or(text.len())];
    let cx = scan_attr_i64(tag, "cx").unwrap_or(DEFAULT_SLD_CX_EMU);
    let cy = scan_attr_i64(tag, "cy").unwrap_or(DEFAULT_SLD_CY_EMU);
    (cx, cy)
}

/// Scan the `r:id` of every `<p:sldId …>` inside `<p:sldIdLst>` in document
/// order (§19.3.1.42). Mirrors the fork's `parse_presentation_slides` ordering
/// so geometry-by-index aligns with `Presentation::slides()`.
fn scan_sld_id_lst(xml: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(xml);
    let Some(lst_start) = text.find("<p:sldIdLst") else {
        return Vec::new();
    };
    let lst_end = text[lst_start..]
        .find("</p:sldIdLst>")
        .map(|e| lst_start + e)
        .unwrap_or(text.len());
    let section = &text[lst_start..lst_end];
    let mut rids = Vec::new();
    let mut rest = section;
    while let Some(p) = rest.find("<p:sldId") {
        let tag_start = p;
        let tag_end = rest[tag_start..]
            .find('>')
            .map(|e| tag_start + e)
            .unwrap_or(rest.len());
        let tag = &rest[tag_start..tag_end];
        if let Some(rid) = scan_attr_str(tag, "r:id") {
            rids.push(rid);
        }
        rest = &rest[tag_end.min(rest.len())..];
    }
    rids
}

/// Self-parse the presentation's colour theme: walk the FIRST slide master
/// (presentation rels → slideMaster) for its `<p:clrMap>` and its `<a:clrScheme>`
/// (master rels → theme), and ALSO resolve each slide's background colour from
/// the slide → layout → master `cSld/bg/bgPr/solidFill` chain. Mirrors the
/// `parse_opc_geometry` OPC discipline (one `Package` over the held bytes) and
/// the WML theme self-parse (raw string scan via [`extract_theme_color_hex`]).
///
/// EXTENDED for #61/#62 (same master part, no third
/// parse): the master XML this scan already reads is ALSO typed-parsed via the
/// generated `SlideMaster::from_xml` to capture (a) the master's placeholder
/// geometry (`LayoutGeom`, the §19.7.10 third inheritance tier) and (b) its
/// `<p:txStyles>` per-role list styles (§19.3.1.52 — the master layer of the
/// text-property chain). A typed-parse miss degrades both to empty (graceful).
///
/// Returns the resolved [`SlideTheme`] (empty on a master/theme miss → every
/// `schemeClr` resolves to `None`, so scheme colours drop out), a per-slide bg vec
/// aligned to `slide_paths`, the master placeholder geometry, and the master
/// txStyles. A single presentation-level theme/master is used (single
/// master in real decks); per-slide override is a follow-on.
fn parse_opc_theme(
    pkg: Option<&mut opc::OpcPackage<'_>>,
    slide_paths: &[String],
) -> (
    shape::SlideTheme,
    Vec<Option<shape::SlideBg>>,
    LayoutGeom,
    shape::MasterTextStyles,
) {
    let empty = (
        shape::SlideTheme::default(),
        vec![None; slide_paths.len()],
        LayoutGeom::default(),
        shape::MasterTextStyles::default(),
    );
    let Some(pkg) = pkg else {
        return empty;
    };

    // Presentation part path (root rels → officeDocument).
    let Some(presentation_path) = pkg.office_document_target() else {
        return empty;
    };

    // Presentation rels → first slideMaster part path.
    let Some(master_path) = pkg
        .rels(&presentation_path)
        .get_by_type(REL_SLIDE_MASTER)
        .map(|r| opc::resolve_target(&presentation_path, &r.target))
    else {
        return empty;
    };

    // Master part → <p:clrMap>.
    let master_xml = pkg.read_part_string(&master_path).unwrap_or_default();
    let clrmap = scan_clr_map(&master_xml);

    // #61/#62: typed-parse the SAME master XML once for its placeholder
    // geometry (the §19.7.10 third tier) + its `<p:txStyles>` (§19.3.1.52).
    let (master_geom, master_txstyles) = parse_master_styles(master_xml.as_bytes());

    // Master rels → theme part → <a:clrScheme> 12 slots (reuse the WML scan)
    // + <a:fontScheme> major/minor latin/ea/cs typefaces.
    let theme_xml = pkg
        .rels(&master_path)
        .get_by_type(opc::rel_type::THEME)
        .map(|r| opc::resolve_target(&master_path, &r.target))
        .and_then(|theme_path| pkg.read_part_string(&theme_path));
    let scheme = theme_xml.as_deref().map(scan_clr_scheme).unwrap_or_default();
    let fonts = theme_xml.as_deref().map(scan_font_scheme).unwrap_or_default();

    let theme = shape::SlideTheme { scheme, clrmap, fonts };

    // Master bg (theme-aware: a schemeClr master bg resolves now).
    let master_bg = scan_bg_fill(&master_xml, &theme);

    // Per-slide bg: slide → layout → master solid/gradient (first that resolves).
    let mut backgrounds = Vec::with_capacity(slide_paths.len());
    for slide_path in slide_paths {
        let bg = (|| -> Option<shape::SlideBg> {
            if slide_path.is_empty() {
                return None;
            }
            let slide_xml = pkg.read_part_string(slide_path)?;
            if let Some(bg) = scan_bg_fill(&slide_xml, &theme) {
                return Some(bg);
            }
            // Layout bg (the slide `.rels` is cache-shared with the geometry
            // walk — parsed once per load).
            let layout_rel = pkg.rels(slide_path).get_by_type(REL_SLIDE_LAYOUT).cloned();
            if let Some(layout_rel) = layout_rel {
                let layout_path = opc::resolve_target(slide_path, &layout_rel.target);
                if let Some(layout_xml) = pkg.read_part_string(&layout_path) {
                    if let Some(bg) = scan_bg_fill(&layout_xml, &theme) {
                        return Some(bg);
                    }
                }
            }
            // Master bg (already scanned).
            master_bg.clone()
        })()
        // A slide with no resolvable bg falls back to the master bg so a
        // master-level background still paints.
        .or_else(|| master_bg.clone());
        backgrounds.push(bg);
    }

    (theme, backgrounds, master_geom, master_txstyles)
}

/// Typed-parse a slide-master part for its placeholder geometry + `txStyles`
/// (#61/#62) via the generated `SlideMaster::from_xml` — the master twin of
/// [`parse_layout_geom`] (same reader-entry pattern, root `sldMaster`). The
/// geometry reuses [`shape::layout_placeholder_geom`] (a master `p:sp` is the
/// same `Shape` type); the txStyles map to per-role [`shape::ListStyle`]s. A
/// parse miss returns both empty (graceful — placeholders then skip the master
/// tier and rPr-less runs keep the type-based defaults).
fn parse_master_styles(xml: &[u8]) -> (LayoutGeom, shape::MasterTextStyles) {
    use ooxml_pml::parsers::FromXml;
    use ooxml_pml::types::SlideMaster;
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_reader(Cursor::new(xml));
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e))
                if e.local_name().as_ref() == b"sldMaster" => {
                    let Ok(master) = SlideMaster::from_xml(&mut reader, &e, false) else {
                        break;
                    };
                    let geom = shape::layout_placeholder_geom(
                        &master.common_slide_data.shape_tree.shape,
                    );
                    let txs = master
                        .tx_styles
                        .as_deref()
                        .map(|t| shape::MasterTextStyles {
                            title: t.title_style.as_deref().map(shape::list_style_from),
                            body: t.body_style.as_deref().map(shape::list_style_from),
                            other: t.other_style.as_deref().map(shape::list_style_from),
                        })
                        .unwrap_or_default();
                    return (geom, txs);
                }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    (LayoutGeom::default(), shape::MasterTextStyles::default())
}

/// Self-parse `ppt/tableStyles.xml` into a [`table::TableStyles`] lookup (see
/// §21.1.3.11) through the load's SHARED [`opc::OpcPackage`]. Resolves the
/// part via the presentation rels (`relationships/tableStyles`); falls back to
/// the conventional `ppt/tableStyles.xml` path when the rel is absent
/// (PowerPoint always writes the conventional path). An absent / unparsable
/// part yields an empty table → every table renders the plain hairline grid
/// (graceful, PML-gated).
fn parse_opc_table_styles(pkg: Option<&mut opc::OpcPackage<'_>>) -> table::TableStyles {
    let Some(pkg) = pkg else {
        return table::TableStyles::default();
    };

    // Presentation part path (root rels → officeDocument), then its rels →
    // tableStyles part; else the conventional `ppt/tableStyles.xml`.
    let part_path = pkg
        .office_document_target()
        .and_then(|pres_path| {
            pkg.rels(&pres_path)
                .get_by_type(REL_TABLE_STYLES)
                .map(|r| opc::resolve_target(&pres_path, &r.target))
        })
        .unwrap_or_else(|| "ppt/tableStyles.xml".to_string());

    match pkg.read_part_string(&part_path) {
        Some(xml) => table::parse_table_styles(&xml),
        None => table::TableStyles::default(),
    }
}

/// Resolve a `schemeClr@val` against a freshly-parsed clrMap+scheme, applying
/// any DML colour transforms. Used by the bg scan (the bg `solidFill` can carry
/// a `schemeClr`). Returns `#rrggbb`.
fn resolve_scheme_hex(theme: &shape::SlideTheme, val: &str, transforms: &[(&str, &str)]) -> Option<String> {
    use ooxml_dml::types::STSchemeColorVal as V;
    let v = match val {
        "bg1" => V::Bg1,
        "tx1" => V::Tx1,
        "bg2" => V::Bg2,
        "tx2" => V::Tx2,
        "dk1" => V::Dk1,
        "lt1" => V::Lt1,
        "dk2" => V::Dk2,
        "lt2" => V::Lt2,
        "accent1" => V::Accent1,
        "accent2" => V::Accent2,
        "accent3" => V::Accent3,
        "accent4" => V::Accent4,
        "accent5" => V::Accent5,
        "accent6" => V::Accent6,
        "hlink" => V::Hlink,
        "folHlink" => V::FolHlink,
        _ => return None,
    };
    let base = shape::resolve_scheme_color(&v, theme)?;
    Some(apply_transform_pairs(&base, transforms))
}

/// Apply scanned `(name, val-string)` DML colour-transform pairs (lumMod/tint/…)
/// to a `#rrggbb` base. Unknown names / unparseable values are skipped; no
/// transforms → the base unchanged. Shared by the schemeClr resolver and the
/// gradient-stop scan (a `srgbClr` stop can carry transforms too).
fn apply_transform_pairs(base: &str, transforms: &[(&str, &str)]) -> String {
    use crate::engine::color::{apply_color_transforms, parse_dml_pct, DmlTransform};
    let mut xs = Vec::new();
    for (name, val) in transforms {
        let mapped = match *name {
            "lumMod" => parse_dml_pct(val).map(DmlTransform::LumMod),
            "lumOff" => parse_dml_pct(val).map(DmlTransform::LumOff),
            "satMod" => parse_dml_pct(val).map(DmlTransform::SatMod),
            "satOff" => parse_dml_pct(val).map(DmlTransform::SatOff),
            "tint" => parse_dml_pct(val).map(DmlTransform::Tint),
            "shade" => parse_dml_pct(val).map(DmlTransform::Shade),
            "alpha" => parse_dml_pct(val).map(DmlTransform::Alpha),
            _ => None,
        };
        if let Some(m) = mapped {
            xs.push(m);
        }
    }
    if xs.is_empty() {
        return base.to_string();
    }
    apply_color_transforms(base, &xs).0
}

/// Scan the DML colour-transform children (`lumMod`/`lumOff`/`satMod`/`satOff`/
/// `tint`/`shade`/`alpha` `@val`) out of one colour element's chunk.
fn scan_clr_transforms(chunk: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in ["lumMod", "lumOff", "satMod", "satOff", "tint", "shade", "alpha"] {
        let needle = format!("<a:{name}");
        if let Some(i) = chunk.find(&needle) {
            if let Some(v) = scan_attr_str(&chunk[i..], "val") {
                out.push((name.to_string(), v));
            }
        }
    }
    out
}

/// Scan a slide-master `<p:clrMap .../>` (§19.3.1.6) into a `placeholder → slot`
/// map (`bg1/tx1/bg2/tx2/accent1-6/hlink/folHlink` → scheme slot, possibly
/// swapped). Absent/garbled → an empty map (every `bg/tx` then resolves to its
/// own name as a slot, the identity fallback).
fn scan_clr_map(xml: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(start) = xml.find("<p:clrMap") else {
        return map;
    };
    let tag = &xml[start..];
    let end = tag.find('>').map(|e| &tag[..e]).unwrap_or(tag);
    for ph in [
        "bg1", "tx1", "bg2", "tx2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ] {
        if let Some(slot) = scan_attr_str(end, ph) {
            map.insert(ph.to_string(), slot);
        }
    }
    map
}

/// Scan a theme part's `<a:clrScheme>` (§20.1.6.2) into a `slot → RRGGBB` map
/// for the 12 slots. Reuses [`crate::engine::color::extract_theme_color_hex`]
/// (handles `srgbClr@val` AND `sysClr@lastClr` → `dk1/lt1` windowText/window).
/// Identical scan to `engine::fonts::parse_theme_fonts`'s colour loop.
fn scan_clr_scheme(xml: &str) -> std::collections::HashMap<String, String> {
    use crate::engine::color::extract_theme_color_hex;
    let mut colors = std::collections::HashMap::new();
    let slots = [
        "dk1", "lt1", "dk2", "lt2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ];
    for slot in slots {
        let open = format!("<a:{slot}");
        if let Some(start) = xml.find(&open) {
            let chunk = &xml[start..];
            if let Some(end) = chunk.find(&format!("</a:{slot}")) {
                if let Some(hex) = extract_theme_color_hex(&chunk[..end]) {
                    colors.insert(slot.to_string(), hex);
                }
            }
        }
    }
    colors
}

/// Scan a theme part's `<a:fontScheme>` (§20.1.4.1.18) into the six
/// major/minor × latin/ea/cs typefaces. Raw string scan, mirroring
/// `scan_clr_scheme` (and the WML `parse_theme_fonts` latin scan). An attribute
/// that EXISTS but is empty (`<a:ea typeface=""/>`) is kept as `Some("")` —
/// the resolution chain distinguishes "theme delegates EA to latin" from "no
/// theme at all" (see `shape::run_doc_east_asia`).
fn scan_font_scheme(xml: &str) -> shape::ThemeFontScheme {
    let scan_slot = |container: &str, tag: &str| -> Option<String> {
        let start = xml.find(&format!("<a:{container}"))?;
        let chunk = &xml[start..];
        let end = chunk
            .find(&format!("</a:{container}"))
            .unwrap_or(chunk.len());
        let chunk = &chunk[..end];
        let t = chunk.find(&format!("<a:{tag} "))?;
        scan_attr_str(&chunk[t..], "typeface")
    };
    shape::ThemeFontScheme {
        major_latin: scan_slot("majorFont", "latin"),
        major_ea: scan_slot("majorFont", "ea"),
        major_cs: scan_slot("majorFont", "cs"),
        minor_latin: scan_slot("minorFont", "latin"),
        minor_ea: scan_slot("minorFont", "ea"),
        minor_cs: scan_slot("minorFont", "cs"),
    }
}

/// Scan a slide/layout/master part's `cSld/bg/bgPr/solidFill` (§19.3.1.2) for a
/// solid background colour. Handles an `srgbClr@val` directly. A `schemeClr`/
/// `sysClr`/gradient/blip bg returns `None` here (the schemeClr bg is resolved
/// later against the theme — see [`scan_bg_solid_fill_with_theme`]). Returns
/// `#rrggbb`.
fn scan_bg_solid_fill(xml: &str) -> Option<String> {
    // Find the <p:bg> ... </p:bg> envelope, then its first solidFill.
    let bg_start = xml.find("<p:bg>")?;
    let bg_end = xml[bg_start..].find("</p:bg>").map(|e| bg_start + e)?;
    let bg = &xml[bg_start..bg_end];
    let sf_start = bg.find("<a:solidFill>")?;
    let sf = &bg[sf_start..];
    let sf_end = sf.find("</a:solidFill>").map(|e| &sf[..e]).unwrap_or(sf);
    // Only a direct srgbClr is resolved at scan time (no theme here); a schemeClr
    // bg is handled by the theme-aware pass in `parse_opc_theme` via
    // `scan_bg_solid_fill_with_theme`.
    if let Some(idx) = sf_end.find("<a:srgbClr") {
        let tag = &sf_end[idx..];
        if let Some(val) = scan_attr_str(tag, "val") {
            return Some(format!("#{}", val.to_lowercase()));
        }
    }
    None
}

/// Theme-aware bg scan: like [`scan_bg_solid_fill`] but also resolves a
/// `schemeClr` background (with its DML transforms) against the slide theme.
fn scan_bg_solid_fill_with_theme(xml: &str, theme: &shape::SlideTheme) -> Option<String> {
    if let Some(hex) = scan_bg_solid_fill(xml) {
        return Some(hex);
    }
    let bg_start = xml.find("<p:bg>")?;
    let bg_end = xml[bg_start..].find("</p:bg>").map(|e| bg_start + e)?;
    let bg = &xml[bg_start..bg_end];
    let sf_start = bg.find("<a:solidFill>")?;
    let sf = &bg[sf_start..];
    let sf_end = sf.find("</a:solidFill>").map(|e| &sf[..e]).unwrap_or(sf);
    let sc_idx = sf_end.find("<a:schemeClr")?;
    let sc = &sf_end[sc_idx..];
    let val = scan_attr_str(sc, "val")?;
    // Pull any child transforms (lumMod/lumOff/shade/tint/satMod/alpha @val).
    let transforms = scan_clr_transforms(sc);
    let pairs: Vec<(&str, &str)> = transforms.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    resolve_scheme_hex(theme, &val, &pairs)
}

/// Resolve a slide/layout/master part's `cSld/bg/bgPr` fill to a [`shape::SlideBg`]:
/// a solid fill (srgbClr or theme-resolved schemeClr) wins, else a gradient
/// fill. A blip/pattern bg (or no `p:bg`) returns `None` → white default.
fn scan_bg_fill(xml: &str, theme: &shape::SlideTheme) -> Option<shape::SlideBg> {
    if let Some(hex) = scan_bg_solid_fill_with_theme(xml, theme) {
        return Some(shape::SlideBg::Solid(hex));
    }
    scan_bg_grad_fill(xml, theme)
}

/// Scan a part's `cSld/bg/bgPr/gradFill` (§20.1.8.33) into a gradient bg: every
/// `<a:gs pos="…">` stop (srgbClr or theme-resolved schemeClr, with DML colour
/// transforms) plus the `<a:lin ang="…">` linear angle (60000ths of a degree,
/// clockwise from 3-o'clock — §20.1.8.41). No `a:lin` (or an `a:path` radial)
/// → 45° linear, the top-left → bottom-right approximation (v1). A single
/// resolvable stop degrades to a solid; zero stops → `None`.
fn scan_bg_grad_fill(xml: &str, theme: &shape::SlideTheme) -> Option<shape::SlideBg> {
    let bg_start = xml.find("<p:bg>")?;
    let bg_end = xml[bg_start..].find("</p:bg>").map(|e| bg_start + e)?;
    let bg = &xml[bg_start..bg_end];
    let gf_start = bg.find("<a:gradFill")?;
    let gf = &bg[gf_start..];
    let gf = gf.find("</a:gradFill>").map(|e| &gf[..e]).unwrap_or(gf);

    // Stops: each <a:gs pos="…">…</a:gs> with one child colour element.
    let mut stops: Vec<(f64, String)> = Vec::new();
    let mut rest = gf;
    while let Some(i) = rest.find("<a:gs ") {
        let gs = &rest[i..];
        let gs_end = gs.find("</a:gs>").unwrap_or(gs.len());
        let chunk = &gs[..gs_end];
        if let (Some(pos), Some(hex)) = (
            scan_attr_str(chunk, "pos").and_then(|p| p.parse::<f64>().ok()),
            scan_gs_color(chunk, theme),
        ) {
            stops.push(((pos / 100_000.0).clamp(0.0, 1.0), hex));
        }
        rest = &gs[gs_end.min(gs.len())..];
    }
    stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    match stops.len() {
        0 => None,
        1 => Some(shape::SlideBg::Solid(stops.remove(0).1)),
        _ => {
            // A PATH gradient (§20.1.8.46): radial at the fillToRect focus.
            // fillToRect l/t/r/b are percent insets (1000ths of a percent) of
            // the rect the gradient converges to; its centre is the focus.
            // Missing attrs default to 0 — so the common corner case
            // l=t=100000 → focus (1.0, 1.0), the bottom-right.
            if let Some(p) = gf.find("<a:path") {
                let path_tag = &gf[p..];
                let inset = |name: &str| -> f64 {
                    path_tag
                        .find("<a:fillToRect")
                        .and_then(|i| scan_attr_str(&path_tag[i..], name))
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(0.0)
                        / 100_000.0
                };
                let (l, t, r, b) = (inset("l"), inset("t"), inset("r"), inset("b"));
                let focus = (
                    ((l + (1.0 - r)) / 2.0).clamp(0.0, 1.0),
                    ((t + (1.0 - b)) / 2.0).clamp(0.0, 1.0),
                );
                return Some(shape::SlideBg::Radial { stops, focus });
            }
            // Linear angle; absent → 45° default.
            let angle_deg = gf
                .find("<a:lin")
                .and_then(|i| scan_attr_str(&gf[i..], "ang"))
                .and_then(|a| a.parse::<f64>().ok())
                .map(|a| a / 60_000.0)
                .unwrap_or(45.0);
            Some(shape::SlideBg::Gradient { stops, angle_deg })
        }
    }
}

/// Resolve one gradient stop's colour: a direct `srgbClr@val` or a theme-mapped
/// `schemeClr@val`, either with DML colour transforms applied. Returns `#rrggbb`.
fn scan_gs_color(chunk: &str, theme: &shape::SlideTheme) -> Option<String> {
    if let Some(idx) = chunk.find("<a:srgbClr") {
        let clr = &chunk[idx..];
        let val = scan_attr_str(clr, "val")?;
        let transforms = scan_clr_transforms(clr);
        let pairs: Vec<(&str, &str)> =
            transforms.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        return Some(apply_transform_pairs(&format!("#{}", val.to_lowercase()), &pairs));
    }
    let idx = chunk.find("<a:schemeClr")?;
    let clr = &chunk[idx..];
    let val = scan_attr_str(clr, "val")?;
    let transforms = scan_clr_transforms(clr);
    let pairs: Vec<(&str, &str)> = transforms.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    resolve_scheme_hex(theme, &val, &pairs)
}

/// Paint a linear-gradient slide background onto the canvas: a
/// `createLinearGradient` whose axis runs through the page centre at
/// `angle_deg` (DML clockwise-from-3-o'clock — canvas y points down, so
/// `(cos θ, sin θ)` maps directly), spanning the page's projection onto that
/// axis so stop 0/1 land exactly on the page corners/edges. Stops are added in
/// (already sorted) `pos` order.
///
/// The gradient endpoints go to `p.set_linear_gradient` (the canvas backend
/// builds a real `CanvasGradient` via `js_sys::Reflect` — arming web-sys's
/// `CanvasGradient` feature is forbidden by the docx-only identity discipline;
/// the recording backend emits the opcode). Any miss returns `Err` → the caller
/// paints the first stop as a solid.
fn paint_bg_gradient(
    p: &dyn Painter,
    w: f64,
    h: f64,
    stops: &[(f64, String)],
    angle_deg: f64,
    bg: Option<(f64, f64, f64, f64)>,
) -> Result<(), JsValue> {
    // Gradient axis stays FULL-slide (0,0,w,h); only the fill rect is windowed.
    set_linear_gradient_fill_style(p, 0.0, 0.0, w, h, stops, angle_deg)?;
    if let Some((bx, by, bw, bh)) = bg {
        p.fill_rect(bx, by, bw, bh);
    }
    Ok(())
}

/// Paint a PATH-gradient background (§20.1.8.46) as a radial gradient: centre at
/// the `fillToRect` focus (page fractions), inner radius 0, outer radius the
/// distance to the farthest page corner so the last stop lands exactly at that
/// corner. The pure focus/radius math stays here; the endpoints/stops go to
/// `p.set_radial_gradient`.
fn paint_bg_radial(
    p: &dyn Painter,
    w: f64,
    h: f64,
    stops: &[(f64, String)],
    focus: (f64, f64),
    bg: Option<(f64, f64, f64, f64)>,
) -> Result<(), JsValue> {
    let (fx, fy) = (focus.0 * w, focus.1 * h);
    // Farthest corner distance = the radius at which pos=1.0 must land.
    let r = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)]
        .iter()
        .map(|(cx, cy)| ((cx - fx).powi(2) + (cy - fy).powi(2)).sqrt())
        .fold(0.0_f64, f64::max)
        .max(1.0);
    // Radial focus/radius stay FULL-slide; only the fill rect is windowed.
    p.set_radial_gradient(fx, fy, 0.0, fx, fy, r, stops)?;
    if let Some((bx, by, bw, bh)) = bg {
        p.fill_rect(bx, by, bw, bh);
    }
    Ok(())
}

/// Set the `fillStyle` for a shape fill: a solid sets the colour string; a
/// gradient builds a linear gradient spanning the `(x, y, w, h)` box (falling
/// back to the first stop as a solid on a surface miss); a pattern rasterises a
/// tile (falling back to the 50/50 fg/bg blend). Returns `false` when there is
/// no fill to paint.
fn set_shape_fill_style(
    p: &dyn Painter,
    fill: &Option<shape::ShapeFill>,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> bool {
    match fill {
        Some(shape::ShapeFill::Solid(hex)) => {
            p.set_fill_style_str(hex);
            true
        }
        Some(shape::ShapeFill::Gradient { stops, angle_deg }) => {
            if set_linear_gradient_fill_style(p, x, y, w, h, stops, *angle_deg).is_err() {
                let hex = stops.first().map(|(_, h)| h.as_str()).unwrap_or("#ffffff");
                p.set_fill_style_str(hex);
            }
            true
        }
        Some(shape::ShapeFill::Pattern { prst, fg, bg }) => {
            // TWO distinct misses, both degrading to the 50/50 fg/bg blend:
            //   1. UNMAPPED preset — decided HERE (before any `set_pattern_fill`),
            //      so both backends emit a plain solid fill (byte-identical). The
            //      recorder records no pattern opcode.
            //   2. MAPPED preset but the pattern surface misses (`createPattern`
            //      null) — the canvas backend Errs and the `unwrap_or_else` blends
            //      (today's flow). The recorder ALWAYS records the pattern opcode
            //      for a mapped preset; replay applies the SAME blend on a miss.
            match crate::engine::pattern::pattern_tile(prst) {
                None => p.set_fill_style_str(&blend_hex_50_str(fg, bg)),
                Some(tile) => {
                    let key = format!("{prst}|{fg}|{bg}");
                    p.set_pattern_fill(&key, &tile, fg, bg)
                        .unwrap_or_else(|_| p.set_fill_style_str(&blend_hex_50_str(fg, bg)));
                }
            }
            true
        }
        None => false,
    }
}

/// Scale a dash array (line-width multiples) by the pen width and set it via
/// `p.set_line_dash` (the canvas backend forwards to `setLineDash`; the recorder
/// emits the opcode). A `None`/empty array clears the dash (the canonical
/// empty-dash op, matching `clear_line_dash`). Floors each segment at a
/// sub-pixel minimum so a hairline dash still renders.
fn apply_line_dash(p: &dyn Painter, dash: Option<&[f64]>, width_px: f64) {
    match dash {
        Some(d) if !d.is_empty() => {
            let scale = width_px.max(1.0);
            let scaled: Vec<f64> = d.iter().map(|seg| (seg * scale).max(0.1)).collect();
            p.set_line_dash(&scaled);
        }
        // None or empty ⇒ ONE canonical empty-dash opcode (matches the in-process
        // `setLineDash([])` and the `ClearLineDash` op the caller's `p.clear_line_dash()` emits).
        _ => p.clear_line_dash(),
    }
}

/// 50/50 blend of two `#rrggbb` colours (the pattFill surface-miss fallback,
/// mirroring `shape::blend_hex_50`). Defined here to keep the painter
/// self-contained; on a bad hex it returns the foreground.
fn blend_hex_50_str(a: &str, b: &str) -> String {
    let parse = |h: &str| -> Option<(u16, u16, u16)> {
        let h = h.trim_start_matches('#');
        // ASCII-hex gate before the byte slices: `len()` counts BYTES, so a
        // 6-byte multi-byte value (`"aé字"`, 6 bytes / 3 chars) reaching here
        // from a document-controlled `<a:srgbClr val=…>` would slice
        // mid-codepoint and panic. `ST_HexColorRGB` is six ASCII hex digits
        // (§20.1.10.19), so this rejects nothing a real document can carry.
        if h.len() != 6 || !h.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some((
            u16::from(u8::from_str_radix(&h[0..2], 16).ok()?),
            u16::from(u8::from_str_radix(&h[2..4], 16).ok()?),
            u16::from(u8::from_str_radix(&h[4..6], 16).ok()?),
        ))
    };
    match (parse(a), parse(b)) {
        (Some((r1, g1, b1)), Some((r2, g2, b2))) => {
            format!("#{:02x}{:02x}{:02x}", (r1 + r2) / 2, (g1 + g2) / 2, (b1 + b2) / 2)
        }
        _ => a.to_string(),
    }
}

/// Resolve the linear-gradient axis through the centre of the `(x, y, w, h)` box
/// at `angle_deg` into concrete endpoints, then hand them + the stops to
/// `p.set_linear_gradient` (shared by the slide background and the shape
/// fills). The pure axis math lives here; the backend builds the gradient.
fn set_linear_gradient_fill_style(
    p: &dyn Painter,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    stops: &[(f64, String)],
    angle_deg: f64,
) -> Result<(), JsValue> {
    let theta = angle_deg.to_radians();
    let (dx, dy) = (theta.cos(), theta.sin());
    // Half-length of the box's projection onto the gradient axis.
    let half = ((w * dx).abs() + (h * dy).abs()) / 2.0;
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (x0, y0) = (cx - dx * half, cy - dy * half);
    let (x1, y1) = (cx + dx * half, cy + dy * half);
    p.set_linear_gradient(x0, y0, x1, y1, stops)
}

/// Read a string attribute `name="…"` out of a single start tag.
fn scan_attr_str(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let i = tag.find(&key)? + key.len();
    let j = tag[i..].find('"')? + i;
    Some(tag[i..j].to_string())
}

/// Read an integer attribute `name="…"` out of a single start tag.
fn scan_attr_i64(tag: &str, name: &str) -> Option<i64> {
    scan_attr_str(tag, name)?.parse().ok()
}

