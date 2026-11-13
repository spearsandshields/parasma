//! The flavor-neutral extraction schema — `DocFragment` and its
//! satellite types, plus the bare selection rect (`SelRect`).
//!
//! Format-agnostic by contract: WML (`wml::select::extract_range_impl`) and PML
//! (`pml::select::extract_range`) both BUILD these; `lib.rs` serializes
//! them across the wasm boundary (`frag_to_jsvalue`). Rust field names are
//! internal; the JS property names set by `frag_to_jsvalue` (lib.rs) are the
//! cross-boundary contract the TS types mirror. The engine populates EVERY
//! field (so the HTML serializer is a pure-TS consumer), even though
//! text/plain only reads `text` + `list.marker` + `footnoteMark`.
//!
//! This module imports NOTHING from the format modules (RULE 3): builders that
//! map format-side metadata into these types live with that metadata (e.g.
//! `FragTable::from_meta` in `wml::select`, beside `TableMeta`).

/// An inline image reference on a run. Bytes never re-cross the wasm
/// boundary; only the rel-id + intrinsic display
/// size travel. `w_px` is the item's display width, `h_px` the intrinsic height.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ImageRef {
    pub(crate) rel_id: String,
    pub(crate) w_px: f64,
    pub(crate) h_px: f64,
}

/// Bare selection rect (page-absolute px), exposed for tests + the paint
/// pass; per-rect direction is deferred until handles exist.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct SelRect {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) w: f64,
    pub(crate) h: f64,
}

/// A resolved cell/table border edge (§17.4.42). Parasma tracks solid-only, so
/// `style` is always "single"; color/width are the resolved values.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FragBorder {
    pub(crate) style: String,
    pub(crate) color: String,
    pub(crate) width_px: f64,
}

/// One logical-order run of styled text (or a zero-width object) in a fragment.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FragRun {
    pub(crate) text: String,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) underline: &'static str,
    pub(crate) strike: bool,
    pub(crate) double_strike: bool,
    pub(crate) font_family: String,
    pub(crate) font_size_pt: f64,
    pub(crate) color: String,
    pub(crate) highlight: Option<String>,
    pub(crate) run_shading: Option<String>,
    pub(crate) vert_align: &'static str,
    pub(crate) link: Option<String>,
    pub(crate) image: Option<ImageRef>,
    pub(crate) footnote_mark: Option<String>,
}

impl FragRun {
    /// The neutral zero-width run: empty text, no emphasis, empty font/color. NOT
    /// `#[derive(Default)]` — `underline` defaults to `"none"` and `vert_align` to
    /// `"baseline"` (non-empty `&'static str`s a derive would blank to `""`).
    pub(crate) fn neutral() -> Self {
        FragRun {
            text: String::new(),
            bold: false,
            italic: false,
            underline: "none",
            strike: false,
            double_strike: false,
            font_family: String::new(),
            font_size_pt: 0.0,
            color: String::new(),
            highlight: None,
            run_shading: None,
            vert_align: "baseline",
            link: None,
            image: None,
            footnote_mark: None,
        }
    }
}

/// A resolved list marker on a fragment paragraph. `value` = the live
/// count (survives mid-list `lvlRestart`); `marker` = the exact rendered text.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FragList {
    pub(crate) ordered: bool,
    pub(crate) num_id: i64,
    pub(crate) level: i64,
    pub(crate) value: i64,
    pub(crate) fmt: String,
    pub(crate) marker: String,
}

/// A paragraph in a fragment. `runs` empty ⇒ §17.3.1.29 empty paragraph.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FragPara {
    pub(crate) runs: Vec<FragRun>,
    pub(crate) alignment: String,
    pub(crate) dir: &'static str,
    pub(crate) list: Option<FragList>,
}

/// One table cell. Partial-table selection emits the FULL grid — cells
/// outside the covered paras are present-but-empty (`blocks` empty). Borders are
/// `[top, right, bottom, left]` (CSS order). `v_merged_away` continuation cells
/// are present-but-empty (the HTML rowspan skips them; text/plain filters them).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FragCell {
    pub(crate) rowspan: u32,
    pub(crate) colspan: u32,
    pub(crate) fill: Option<String>,
    pub(crate) borders: [Option<FragBorder>; 4],
    pub(crate) v_merged_away: bool,
    pub(crate) blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FragRow {
    pub(crate) cells: Vec<FragCell>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FragTable {
    pub(crate) rows: Vec<FragRow>,
}

/// A top-level fragment block: a body paragraph or a table.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Block {
    Para(FragPara),
    Table(FragTable),
}

/// The extraction result. `blocks` in LOGICAL document order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DocFragment {
    pub(crate) blocks: Vec<Block>,
}
