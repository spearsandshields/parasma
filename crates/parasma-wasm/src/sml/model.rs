//! Owned, web-sys-free spreadsheet model.
//!
//! Built from `ooxml_sml`'s parsed `ResolvedSheet`. Everything here is plain
//! data + pure functions so it can be natively unit-tested with no
//! wasm-bindgen / web-sys (as the oracle tests are). The renderer-facing
//! geometry lives in [`super::grid`]; this file is purely the *content* model:
//! cell values, default alignment, column/row metadata, and Maximum-Digit-Width
//! measurement.

use std::collections::HashMap;

use ooxml_sml::ext::{
    // NOTE: `CellExt` is deliberately NOT imported — its `column_number()` is the
    // unchecked A1 column parse; `build_rows` uses the bounded one below.
    BorderExt, BorderPropertiesExt, CellAlignmentExt, CellValue, FillExt, FontExt,
    FormatExt, PatternFillExt, ResolvedSheet,
};
use ooxml_sml::types::{Cell, Stylesheet};

use crate::engine::fonts::{self, FontCache};

use super::numfmt::{self, RawValue};

/// Documented fallback Maximum Digit Width when the normal-style font is not
/// loaded (§18.3.1.13). Calibri 11pt @96dpi measures 7px; Excel's worked
/// example uses exactly this value, so it is the safe default when shaping is
/// unavailable. SIMPLIFICATION: when the font *is* loaded we measure;
/// otherwise we fall back to 7 rather than failing.
pub(crate) const FALLBACK_MDW: f64 = 7.0;

/// Default normal-style font when the stylesheet declares none (§18 normal
/// style). SIMPLIFICATION: we read `fonts[0].name` or fall back to Calibri;
/// resolving the *actual* normal style via cellStyleXfs/cellXfs fontId is not done.
pub(crate) const DEFAULT_NORMAL_FONT: &str = "Calibri";

/// Horizontal alignment a cell gets from Excel's *general* alignment rule
/// (§18, general horizontal alignment): text is left, numbers right,
/// booleans/errors centered, empty cells left. This is what makes a bare grid
/// read like a spreadsheet, so the painter applies it even without explicit
/// `cellXfs` alignment (that overrides it via `CellStyle::h_align_override`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HAlign {
    Left,
    Right,
    Center,
}

/// One painted border edge (§18.8.4 CT_Border): the shared [`Edge`] carrying a
/// pixel width and hex colour (`dash` unused by SML). `None` (the array slot)
/// means the edge has no drawn border. SIMPLIFICATION: line *style*
/// (dashed/dotted/double…) collapses to a solid stroke whose width approximates
/// the style class (thin/medium/thick); dash patterns and double rails deferred.
pub(crate) use crate::engine::border::Edge as BorderEdge;

/// The fully-resolved per-cell visual style, built once per distinct
/// `style_index` from `cellXfs[s]` → font/fill/border/alignment + numFmt
/// (§18.8.45 CT_Xf). Plain owned data so the model stays web-sys-free; the
/// renderer turns it into a `RunStyle` and a fill/border paint at draw time.
///
/// Colours resolve through [`ColorCtx`] with precedence rgb → theme(+tint §18.8.19)
/// → indexed (§18.8.27) → auto/`None` (resolving `Color.rgb` alone would
/// white out every theme-styled workbook).
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct CellStyle {
    /// Effective number-format code (custom-first, then built-in §18.8.30), or
    /// `None` when the cell uses General (no formatting applied).
    pub(crate) num_fmt_code: Option<String>,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) strikethrough: bool,
    /// Explicit font size in points, or `None` → the normal-style default 11pt.
    pub(crate) font_size_pt: Option<f64>,
    /// Explicit font family, or `None` → the normal-style family.
    pub(crate) font_family: Option<String>,
    /// Font colour `#RRGGBB`, or `None` → the default near-black cell text.
    pub(crate) font_color_hex: Option<String>,
    /// Solid fill colour `#RRGGBB` (`patternType="solid"` fgColor), or `None`.
    pub(crate) fill_hex: Option<String>,
    /// Border edges in [left, right, top, bottom] order (§18.8.4).
    pub(crate) borders: [Option<BorderEdge>; 4],
    /// Explicit horizontal alignment from `<alignment>`, overriding the
    /// value-kind general alignment (§18.8.1). `None` → keep general.
    pub(crate) h_align_override: Option<HAlign>,
}

impl CellStyle {
    /// Fold every field into the geometry hash `h` via the caller's FNV closures
    /// (`byte`/`feed_f`/`feed_s` — the constants live once at the call site).
    ///
    /// The opening exhaustive destructure (NO rest-dots) is a COMPILE-TIME
    /// completeness guard: adding a field to `CellStyle` fails to compile until it
    /// is bound and folded here, closing the blind spot where a new style field
    /// rendered differently yet left the pinned geometry hash identical. The fold
    /// order/calls reproduce the historical stream byte-for-byte (`dash` stays
    /// unfolded — SML is solid-only, model.rs above).
    pub(crate) fn hash_into(
        &self,
        byte: &impl Fn(&mut u64, u8),
        feed_f: &impl Fn(&mut u64, f64),
        feed_s: &impl Fn(&mut u64, &str),
        h: &mut u64,
    ) {
        let CellStyle {
            num_fmt_code,
            bold,
            italic,
            strikethrough,
            font_size_pt,
            font_family,
            font_color_hex,
            fill_hex,
            borders,
            h_align_override,
        } = self;
        feed_s(h, num_fmt_code.as_deref().unwrap_or(""));
        byte(h, *bold as u8);
        byte(h, *italic as u8);
        byte(h, *strikethrough as u8);
        feed_f(h, font_size_pt.unwrap_or(0.0));
        feed_s(h, font_family.as_deref().unwrap_or(""));
        feed_s(h, font_color_hex.as_deref().unwrap_or(""));
        feed_s(h, fill_hex.as_deref().unwrap_or(""));
        for edge in borders {
            match edge {
                Some(e) => {
                    byte(h, 1);
                    feed_f(h, e.width_px);
                    feed_s(h, &e.color);
                }
                None => byte(h, 0),
            }
        }
        match h_align_override {
            Some(a) => {
                byte(h, 1);
                byte(h, *a as u8);
            }
            None => byte(h, 0),
        }
    }
}

/// One resolved cell: its 1-based column, the display text, the general
/// alignment implied by its value kind, and the resolved per-cell style.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CellModel {
    /// 1-based column number (parsed from the A1 reference, §18.3.1.35).
    pub(crate) col: u32,
    /// The resolved display string: shared-string / inline-string / a number
    /// run through its `numFmt` code (§18.8.30) at model-build time.
    pub(crate) text: String,
    /// General horizontal alignment derived from the value kind (an explicit
    /// `<alignment>` in [`CellStyle::h_align_override`] supersedes it at paint).
    pub(crate) align: HAlign,
    /// The resolved per-cell visual style, or `None` for cells with no
    /// `style_index` / the default xf (font/fill/border default normal style).
    pub(crate) style: Option<CellStyle>,
    /// The RAW (pre-numFmt) value for the formula-bar readout: a number's
    /// General representation, TRUE/FALSE, or the string itself.
    pub(crate) raw: String,
}

/// A merged-cell region (§18.3.1.55 mergeCell `@ref`, e.g. `"A1:C1"`), parsed
/// into an inclusive 1-based row/column box. The top-left (`min_row,min_col`) is
/// the ANCHOR: it holds the merged region's content and formatting; every other
/// covered cell is suppressed and the region renders as one box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MergeRegion {
    pub(crate) min_row: u32,
    pub(crate) min_col: u32,
    pub(crate) max_row: u32,
    pub(crate) max_col: u32,
}

impl MergeRegion {
    /// True if `(row, col)` is the anchor (top-left) of this region.
    pub(crate) fn is_anchor(&self, row: u32, col: u32) -> bool {
        row == self.min_row && col == self.min_col
    }

    /// True if `(row, col)` falls inside this region (anchor included).
    pub(crate) fn contains(&self, row: u32, col: u32) -> bool {
        row >= self.min_row && row <= self.max_row && col >= self.min_col && col <= self.max_col
    }
}

/// One non-empty row: its 1-based row number plus the sparse list of populated
/// cells (missing cells are simply absent — empty, never zero-width, §18.3.1.35).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RowData {
    /// 1-based row number (`<row r="…">`, §18.3.1.73).
    pub(crate) row: u32,
    /// Explicit row height in POINTS if the file set `@ht` (§18.3.1.73).
    /// `None` → use the sheet default (§18.3.1.81).
    pub(crate) height_pt: Option<f64>,
    /// `@hidden` (§18.3.1.73) OR the Excel-equivalence `ht="0"` +
    /// `customHeight`. A hidden row contributes ZERO height to the shared
    /// prefix sums (a height-0 exception) and is skipped by search.
    pub(crate) hidden: bool,
    /// Populated cells, in document order.
    pub(crate) cells: Vec<CellModel>,
}

/// A `<col>` width run (`@min..=@max` inclusive, 1-based) carrying its file
/// width in character units (§18.3.1.13). `width` is `None` when the column
/// declares no explicit width (then the sheet default applies).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColRun {
    pub(crate) min: u32,
    pub(crate) max: u32,
    /// File `@width` in character units (§18.3.1.13); `None` → sheet default.
    pub(crate) width_chars: Option<f64>,
    /// `@hidden` (§18.3.1.13) OR the Excel-equivalence `width="0"` +
    /// `customWidth` (how Excel persists a drag-to-zero column). Hidden columns
    /// contribute ZERO width to the shared prefix sums — indices untouched.
    pub(crate) hidden: bool,
}

/// The owned sheet model: used range, per-row data, column-width runs, sheet
/// defaults, gridline flag, and the measured Maximum Digit Width.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SheetModel {
    pub(crate) name: String,
    /// Used range `(min_row, min_col, max_row, max_col)` (§18.3.1.35), or `None`
    /// when the sheet is empty.
    pub(crate) dimensions: Option<(u32, u32, u32, u32)>,
    pub(crate) rows: Vec<RowData>,
    pub(crate) col_runs: Vec<ColRun>,
    /// `sheetFormatPr@defaultRowHeight` in POINTS (REQUIRED attr, §18.3.1.81).
    pub(crate) default_row_height_pt: f64,
    /// `sheetFormatPr@defaultColWidth` in character units (§18.3.1.81), or
    /// derived from `baseColWidth` when absent.
    pub(crate) default_col_width_chars: f64,
    /// `sheetView@showGridLines` (§18.3.1.88); `true` by default.
    pub(crate) show_grid_lines: bool,
    /// Maximum Digit Width of the normal-style font, in whole pixels
    /// (§18.3.1.13). Measured when the font is loaded, else [`FALLBACK_MDW`].
    pub(crate) mdw: f64,
    /// Whether this sheet was recovered via the `<col>` render-only pre-repair (its
    /// `<col>` widths defaulted). Surfaced later via documentInfo.
    pub(crate) col_widths_degraded: bool,
    /// Merged-cell regions (§18.3.1.55), parsed from `<mergeCells>/<mergeCell>`.
    /// Each region renders as one box: its anchor (top-left) spans the full
    /// extent and every other covered cell is suppressed at pagination time.
    pub(crate) merges: Vec<MergeRegion>,
    /// Anchored pictures from the sheet's drawing part (§18.3.1.36 +
    /// §20.5). `from_resolved` leaves this EMPTY — the fork's `ResolvedSheet`
    /// carries no drawing data; `SmlRenderer` attaches its OPC-parsed anchors
    /// (matched by sheet name) after the model is built. `paginate` converts
    /// the (col,row,EMU) anchors to pixel boxes on the grid rails.
    pub(crate) drawings: Vec<super::drawings::SheetDrawing>,
    /// Anchored CHARTS from the sheet's drawing part (embedded charts) —
    /// attached exactly like `drawings` (matched by sheet name); `paginate`
    /// converts them to pixel boxes on the same rails, into `SheetPage::charts`.
    pub(crate) charts: Vec<super::drawings::SheetDrawing>,
}

/// Excel's default `baseColWidth` when `sheetFormatPr` declares none
/// (§18.3.1.81: the attribute's default is 8 characters).
const DEFAULT_BASE_COL_WIDTH: u32 = 8;
/// Excel's default row height in points when no `sheetFormatPr` is present.
/// SIMPLIFICATION: the spec marks `defaultRowHeight` REQUIRED inside
/// `sheetFormatPr`, but `sheetFormatPr` itself is optional; 15pt is the
/// conventional Calibri-11 default Excel writes.
const DEFAULT_ROW_HEIGHT_PT: f64 = 15.0;

impl SheetModel {
    /// Build an owned model from a parsed `ResolvedSheet`, measuring the
    /// Maximum Digit Width from `normal_font` via the shared [`FontCache`].
    pub(crate) fn from_resolved(
        sheet: &ResolvedSheet,
        normal_font: &str,
        font_cache: &FontCache,
        col_widths_degraded: bool,
        stylesheet: &Stylesheet,
        colors: &ColorCtx,
    ) -> Self {
        let mdw = measure_mdw(normal_font, font_cache);
        let mut styles = StyleResolver::new(stylesheet, colors);

        // Rows + cells (sparse) — the shared row builder, also driven by the
        // Grid-v2 window materializer over synthetic-envelope parses.
        let rows = build_rows(sheet, &mut styles);

        // ── Column-width runs (sml-styling) ──────────────────────────────────
        let col_runs = collect_col_runs(sheet);

        // ── Sheet format defaults (§18.3.1.81) ───────────────────────────────
        let (default_row_height_pt, default_col_width_chars) = sheet_defaults(sheet);

        // ── Gridlines (§18.3.1.88, default true) ─────────────────────────────
        let show_grid_lines = grid_lines_enabled(sheet);

        // ── Merged-cell regions (§18.3.1.55) ─────────────────────────────────
        let merges = collect_merges(sheet);

        SheetModel {
            name: sheet.name().to_string(),
            dimensions: sheet.dimensions(),
            rows,
            col_runs,
            default_row_height_pt,
            default_col_width_chars,
            show_grid_lines,
            mdw,
            col_widths_degraded,
            merges,
            drawings: Vec::new(),
            charts: Vec::new(),
        }
    }

    /// File width (character units) of column `col` (1-based) — its `<col>` run
    /// `@width` if any, else the sheet default (§18.3.1.13 / §18.3.1.81).
    pub(crate) fn col_width_chars(&self, col: u32) -> f64 {
        for run in &self.col_runs {
            if col >= run.min && col <= run.max {
                if let Some(w) = run.width_chars {
                    return w;
                }
            }
        }
        self.default_col_width_chars
    }

    /// Whether column `col` is HIDDEN (§18.3.1.13 `@hidden` / width-0 +
    /// customWidth). Hidden columns contribute ZERO pixels to the rails — the
    /// §18.3.1.13 formula must NOT run (it adds a 5px padding floor).
    pub(crate) fn col_hidden(&self, col: u32) -> bool {
        self.col_runs.iter().any(|r| col >= r.min && col <= r.max && r.hidden)
    }
}

/// Build the owned row models from a parsed sheet's rows (sparse: only
/// populated rows/cells, §18.3.1.35). §18.3.1.73: `@r` is OPTIONAL on `<row>` —
/// when omitted the row is implied to follow the previous one (.NET exporters
/// write r-less `<x:row>` streams; skipping them rendered such sheets blank).
///
/// Extracted verbatim from `from_resolved` so the Grid-v2 window materializer
/// (`sml::window`) builds IDENTICAL `RowData`/`CellModel` rows from a
/// synthetic-envelope parse (the v1/v2 equivalence invariant): per-cell style
/// resolution (cached by `style_index`), the empty-cell skip UNLESS the style
/// paints ink (fill/border — the swatch-board rule), and the row skip when
/// it has neither cells nor an explicit height.
pub(crate) fn build_rows(sheet: &ResolvedSheet, styles: &mut StyleResolver) -> Vec<RowData> {
    let mut rows = Vec::new();
    let mut next_row: u32 = 1;
    for row in sheet.rows() {
        let row_num = row.reference.unwrap_or(next_row);
        // `saturating_add`: `<row r="4294967295">` would otherwise overflow the
        // implied-row counter (debug panic / release wrap). The row itself is
        // dropped just below; the counter only has to stay monotone.
        next_row = row_num.saturating_add(1);
        // An UNADDRESSABLE row is dropped here, not snapped into the grid.
        // Every range that could reach it (`used_range_v1`, `content_bounds`,
        // `print_range`) drops it too, so the materializer and the index agree;
        // snapping instead would relocate its cells onto row 1 048 576, whose
        // contents it would then be competing with. See `MAX_ADDRESSABLE_ROW`.
        if row_num > MAX_ADDRESSABLE_ROW {
            continue;
        }
        let mut cells = Vec::new();
        for cell in &row.cells {
            // NOT `cell.column_number()`. The fork's `parse_column` behind
            // it accumulates `col * 26 + d` unchecked, so `<c r="AATYHWUU1"/>`
            // panics in debug and WRAPS in release — silently painting the cell
            // in an arbitrary in-grid column. This is the path every RENDERED
            // cell takes (the index pre-scan's `parse_a1` does not reach it), so
            // the bound has to be here as well. Column-only parse, so a
            // digit-less `@r` still resolves exactly as the fork resolved it.
            let col = match cell.reference.as_deref().and_then(|r| parse_a1_column(r)) {
                Some((c, _)) => c,
                None => continue,
            };
            // Resolve the per-cell style once (cached by style_index).
            let style = cell.style_index.and_then(|s| styles.resolve(s).cloned());
            let num_fmt_code = style.as_ref().and_then(|s| s.num_fmt_code.as_deref());
            let (text, raw, align) = resolve_cell(cell, sheet, num_fmt_code);
            // Skip truly-empty cells: they contribute no ink and no
            // geometry (an empty cell is NOT a zero-width column) — UNLESS
            // their resolved style itself paints ink (a fill or border).
            // Value-less styled cells (`<c r="A12" s="26"/>`) are how Excel
            // writes colour swatches / banded blank regions (a graduated
            // theme×tint swatch board can be built entirely of them);
            // dropping them would render the board white.
            if text.is_empty() {
                let paints_ink = style.as_ref().is_some_and(|s| {
                    s.fill_hex.is_some() || s.borders.iter().any(|b| b.is_some())
                });
                if !paints_ink {
                    continue;
                }
            }
            cells.push(CellModel { col, text, align, style, raw });
        }
        // Height is sml-styling-gated; absent → sheet default at geometry time.
        let height_pt = row_height_pt(row);
        // A HIDDEN row is kept even when empty and height-less: it must carry
        // its zero-height exception into the geometry (hidden-gated — the
        // hidden-less skip rule is unchanged).
        let hidden = row_hidden(row);
        if cells.is_empty() && height_pt.is_none() && !hidden {
            continue;
        }
        rows.push(RowData { row: row_num, height_pt, hidden, cells });
    }
    rows
}

/// Resolve a cell's display text + general alignment (§18, general alignment).
///
/// Reads inline strings from `Cell.is` directly (a known pitfall: the resolver's
/// `<v>`-only path returns Empty for inline-string cells), and shared strings /
/// numbers / booleans / errors via the embedded resolve context. When the
/// cell's xf carries an effective number-format code (`num_fmt_code`), the
/// value is routed through the §18.8.30 interpreter; otherwise it falls back to
/// the raw General representation.
fn resolve_cell(cell: &Cell, sheet: &ResolvedSheet, num_fmt_code: Option<&str>) -> (String, String, HAlign) {
    // Inline string (<is>): content lives in Cell.is, NOT <v>. Recover it
    // regardless of cell_type so the InlineString failure-mode (empty cell) is
    // avoided.
    if let Some(rs) = &cell.is {
        let text = inline_string_text(rs);
        if !text.is_empty() {
            let out = match num_fmt_code {
                Some(code) => numfmt::format_value(&RawValue::Text(&text), code),
                None => text.clone(),
            };
            return (out, text, HAlign::Left);
        }
    }

    match sheet.cell_value(cell) {
        CellValue::Empty => (String::new(), String::new(), HAlign::Left),
        CellValue::String(s) => {
            let out = match num_fmt_code {
                Some(code) => numfmt::format_value(&RawValue::Text(&s), code),
                None => s.clone(),
            };
            (out, s, HAlign::Left)
        }
        CellValue::Number(n) => {
            // Apply the cell's number format (§18.8.30) when present, else the
            // raw General representation. A date code makes
            // 45292 read as 2024-01-01; numbers stay right-aligned regardless.
            let out = match num_fmt_code {
                Some(code) => numfmt::format_value(&RawValue::Number(n), code),
                None => format_number_raw(n),
            };
            (out, format_number_raw(n), HAlign::Right)
        }
        CellValue::Boolean(b) => {
            // Excel displays booleans as upper-case TRUE/FALSE, centered.
            let t = (if b { "TRUE" } else { "FALSE" }).to_string();
            (t.clone(), t, HAlign::Center)
        }
        CellValue::Error(e) => (e.clone(), e, HAlign::Center),
    }
}

/// Concatenate the text of an inline `<is>` RichString: either the single `<t>`
/// child (`RichString.cell_type`) or the concatenation of all rich-run `<t>`
/// texts (`RichString.reference[*].cell_type`) — per the verified field layout.
fn inline_string_text(rs: &ooxml_sml::types::RichString) -> String {
    let mut out = rs.cell_type.clone().unwrap_or_default();
    for run in &rs.reference {
        out.push_str(&run.cell_type);
    }
    out
}

/// Render a number raw (the General fallback: no number format; integers print
/// without a trailing `.0`, e.g. the date serial 45292 stays 45292).
fn format_number_raw(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        // Trim to a stable, terse representation (avoids 0.1+0.2 noise tails).
        let s = format!("{n}");
        s
    }
}

/// Measure the Maximum Digit Width of `family` in whole pixels: the max advance
/// over digits '0'..='9' at the normal 11pt size, rounded UP to a whole pixel
/// (§18.3.1.13). Falls back to [`FALLBACK_MDW`] when the font is not loaded.
pub(crate) fn measure_mdw(family: &str, font_cache: &FontCache) -> f64 {
    let font = match font_cache.resolve(family, 400, false) {
        Some(f) => f,
        None => return FALLBACK_MDW,
    };
    // Ten digits, one built face (rather than parsing the whole OpenType
    // face ten times per column-width query).
    let faces = fonts::FaceCache::new();
    let mut max_adv: f64 = 0.0;
    let mut any = false;
    for d in '0'..='9' {
        if let Some(run) = faces.shape(&d.to_string(), font, fonts::DEFAULT_FONT_PT) {
            max_adv = max_adv.max(run.total_advance);
            any = true;
        }
    }
    if !any || max_adv <= 0.0 {
        return FALLBACK_MDW;
    }
    max_adv.ceil()
}

/// Explicit row height in points (`@ht`), sml-styling-gated field. The whole
/// `sml` module compiles only when `sml` (and thus `sml-styling`) is armed, so
/// the field is always present here.
fn row_height_pt(row: &ooxml_sml::types::Row) -> Option<f64> {
    row.height
}

/// Float-dust guard for the Excel hidden-equivalences: a `width="0"` /
/// `ht="0"` written with FP noise (e.g. `1e-9`) still counts as zero.
pub(crate) const HIDDEN_ZERO_EPS: f64 = 0.001;

/// Whether a `<row>` displays as HIDDEN: `@hidden` (§18.3.1.73), or the Excel
/// equivalence `ht="0"` + `customHeight` (how Excel persists a drag-to-zero
/// row). outlineLevel/collapsed groups need no extra handling — Excel writes
/// `hidden="1"` on the collapsed members themselves.
fn row_hidden(row: &ooxml_sml::types::Row) -> bool {
    row.hidden.unwrap_or(false)
        || (row.custom_height.unwrap_or(false)
            && row.height.is_some_and(|h| h.abs() < HIDDEN_ZERO_EPS))
}

/// Column-width runs from `<cols>/<col>` (sml-styling-gated accessor).
/// `pub(crate)`: reused verbatim by the Grid-v2 husk parse (`sml::index`).
/// `hidden`: `@hidden` (§18.3.1.13), or the Excel equivalence `width="0"` +
/// `customWidth` (float-dust guarded).
pub(crate) fn collect_col_runs(sheet: &ResolvedSheet) -> Vec<ColRun> {
    let mut runs = Vec::new();
    for cols in sheet.columns() {
        for col in &cols.col {
            let hidden = col.hidden.unwrap_or(false)
                || (col.custom_width.unwrap_or(false)
                    && col.width.is_some_and(|w| w.abs() < HIDDEN_ZERO_EPS));
            runs.push(ColRun {
                min: col.start_column,
                max: col.end_column,
                width_chars: col.width,
                hidden,
            });
        }
    }
    runs
}

/// Parse the `<mergeCells>/<mergeCell>` collection (§18.3.1.55) into owned
/// [`MergeRegion`]s. Each `@ref` is an A1 range `"A1:C1"`; a single-cell ref
/// (no `:`) collapses to a 1×1 region (harmless, it covers only its anchor).
/// Malformed refs are silently dropped (render-only tolerance).
/// `pub(crate)`: reused verbatim by the Grid-v2 husk parse (`sml::index`).
pub(crate) fn collect_merges(sheet: &ResolvedSheet) -> Vec<MergeRegion> {
    let mut out = Vec::new();
    if let Some(mc) = sheet.merged_cells() {
        for m in &mc.merge_cell {
            if let Some(region) = parse_merge_ref(&m.reference) {
                out.push(region);
            }
        }
    }
    out
}

/// Parse an A1 range reference (`"A1:C1"`, §18.3.1.55) into a normalised
/// [`MergeRegion`]. A bare single-cell ref (`"B4"`) yields a 1×1 region. Returns
/// `None` when either endpoint fails to parse (col letters + row digits).
/// `pub(crate)`: `sml::index` reuses it for the stamped `<dimension>` ref too.
pub(crate) fn parse_merge_ref(reference: &str) -> Option<MergeRegion> {
    let (start, end) = match reference.split_once(':') {
        Some((a, b)) => (a, b),
        None => (reference, reference),
    };
    let (c0, r0) = parse_a1(start)?;
    let (c1, r1) = parse_a1(end)?;
    Some(MergeRegion {
        min_row: r0.min(r1),
        min_col: c0.min(c1),
        max_row: r0.max(r1),
        max_col: c0.max(c1),
    })
}

/// The last COLUMN this renderer can address: 16 384 (`XFD`).
///
/// **ECMA-376 DOES NOT BOUND THIS, and the comment must not pretend otherwise.**
/// The types behind every A1 reference — `ST_CellRef` (§18.18.7), `ST_Ref`
/// (§18.18.62), `ST_RefA` (§18.18.63) — are declared as bare `xsd:string` with
/// ZERO facets (no pattern, no length; the whole SpreadsheetML schema contains
/// no `xsd:pattern` at all), and the formal A1 grammar (§18.17.2.3.1) admits
/// `A1-relative-column = letter, {letter}` — arbitrarily many letters. The one
/// place the standard names the familiar grid is §18.17.5.1 (Limits), inside the
/// FORMULAS chapter, and it explicitly declines to make it a ceiling:
///
/// > "In SpreadsheetML, cell references range from column A1–A1048576 (column
/// > A:A) to column XFD1–XFD1048576 (column XFD:XFD). **An implementation can
/// > extend this range.**"
///
/// and §18.17.2.3 pushes the other way outright:
///
/// > "[Guidance An implementation is encouraged to not unnecessarily limit the
/// > number of rows and columns in a worksheet. end guidance]"
///
/// So this is a documented **implementation limit**, chosen for Excel
/// compatibility, not a conformance bound. It exists because Parasma's column
/// model is a DENSE rail array — `IndexGeom::col_x` holds one `f64` per column
/// in the range, and `split_col_bands`/`geom_for` walk it one column at a time —
/// so the addressable extent is exactly what the renderer can materialize. A
/// document-supplied `"ZZZZZZ1"` (column 321 272 406, schema-valid text) asks
/// for ~2.5 GB of rails; `"AATYHWUU1"` overflows `u32` outright.
///
/// **Because the bound is OURS and not the standard's, the policy attached to it
/// is REJECT, never CLAMP.** An unaddressable coordinate is DROPPED — the cell
/// is not rendered, the merge/dimension ref is ignored, the row falls out of
/// every layout range — and is never rewritten to a nearer one. Clamping would
/// move an author's cell into a *different, possibly occupied* cell and make a
/// document render wrong rather than incomplete. Every coordinate the bound
/// admits, up to and including the `XFD1048576` corner Excel itself writes for a
/// select-all merge, is passed through untouched.
pub(crate) const MAX_ADDRESSABLE_COL: u32 = 16_384;

/// The last ROW this renderer can address: 1 048 576. Same standing as
/// [`MAX_ADDRESSABLE_COL`] — see that comment for the §18.17.5.1 / §18.17.2.3
/// citations and for why the policy is REJECT rather than CLAMP. The row model
/// is a per-row walk (`plan_pages`, `print_range`, `paginate`, `build_window_page`
/// all iterate `min_row..=max_row`), so a `<row r="4000000000">` is four billion
/// iterations of work for a row that can hold nothing.
pub(crate) const MAX_ADDRESSABLE_ROW: u32 = 1_048_576;

/// Parse the leading COLUMN-letter run of an A1 reference into a 1-based column
/// number plus the remainder of the string. Mirrors the fork's private
/// `parse_column` (re-implemented here because it is not exported) EXCEPT that
/// the accumulation is checked and bounded:
///
///  * `checked_mul`/`checked_add` — the fork's `col * 26 + d` overflows `u32` on
///    an 8-letter run, which PANICS in debug and silently WRAPS in release,
///    landing the cell in an arbitrary (possibly occupied) in-grid column. A
///    reference this implementation cannot represent is one it declines to read.
///  * `> MAX_ADDRESSABLE_COL` — the renderer's documented addressable limit; see
///    that constant for why the standard does not supply one and why the policy
///    is to drop the reference rather than clamp it. The check sits INSIDE the
///    loop because `col` only ever grows, so leaving the grid is terminal.
///
/// `None` when there are no leading letters at all (the fork's `col == 0` case).
fn parse_a1_column(reference: &str) -> Option<(u32, &str)> {
    let mut col: u32 = 0;
    let mut seen_letter = false;
    let mut rest = reference;
    for (idx, ch) in reference.char_indices() {
        if ch.is_ascii_alphabetic() {
            col = col.checked_mul(26)?.checked_add(ch.to_ascii_uppercase() as u32 - 'A' as u32 + 1)?;
            if col > MAX_ADDRESSABLE_COL {
                return None;
            }
            seen_letter = true;
        } else {
            rest = &reference[idx..];
            break;
        }
    }
    if !seen_letter || col == 0 {
        return None;
    }
    Some((col, rest))
}

/// Parse a single A1 cell reference (`"A1"`, `"AB12"`) into `(col, row)`, both
/// 1-based (§18.3.1.35). Returns `None` when the reference has no column letters
/// or no row digits.
/// `pub(crate)`: the Grid-v2 pre-scan (`sml::index`) parses `<c r>` refs too.
///
/// The row part is rejected when it is `0` — that one IS the standard's rule
/// (§18.17.2.3.1: "A1-relative-row = digit-sequence ; (* shall be non-zero *)")
/// — when it does not fit `u32` (`str::parse` already refuses; nothing is
/// saturated), and when it exceeds [`MAX_ADDRESSABLE_ROW`] (this renderer's
/// documented limit, dropped rather than clamped — see that constant).
pub(crate) fn parse_a1(reference: &str) -> Option<(u32, u32)> {
    let (col, rest) = parse_a1_column(reference)?;
    // Strip any '$' absolute-reference markers from the row part before parsing.
    let row: u32 = rest.trim_start_matches('$').parse().ok()?;
    if row == 0 || row > MAX_ADDRESSABLE_ROW {
        return None;
    }
    Some((col, row))
}

/// `sheetFormatPr` defaults: `(defaultRowHeight pt, defaultColWidth chars)`
/// (§18.3.1.81). `defaultColWidth`, when absent, derives from
/// `baseColWidth + 5px padding`, expressed back in character units.
/// `pub(crate)`: reused verbatim by the Grid-v2 husk parse (`sml::index`).
pub(crate) fn sheet_defaults(sheet: &ResolvedSheet) -> (f64, f64) {
    if let Some(fmt) = sheet_format(sheet) {
        let row_h = fmt.default_row_height; // REQUIRED, points.
        let col_w = match fmt.default_col_width {
            Some(w) => w,
            None => derive_default_col_width(fmt.base_col_width.unwrap_or(DEFAULT_BASE_COL_WIDTH)),
        };
        (row_h, col_w)
    } else {
        (
            DEFAULT_ROW_HEIGHT_PT,
            derive_default_col_width(DEFAULT_BASE_COL_WIDTH),
        )
    }
}

/// `Worksheet.sheet_format` accessor (sml-styling-gated field; always present
/// under the `sml` feature).
fn sheet_format(sheet: &ResolvedSheet) -> Option<&ooxml_sml::types::SheetFormat> {
    sheet.worksheet().sheet_format.as_deref()
}

/// §18.3.1.81: when `defaultColWidth` is absent it derives from
/// `baseColWidth + 4px margin + 1px gridline`. We return the default in
/// CHARACTER units (= `baseColWidth`); the +5px padding is not double-counted
/// here because the grid layer converts character widths to pixels through the
/// §18.3.1.13 file-width formula, which itself adds the 5px padding. Keeping the
/// default in character units lets every column (default or explicit `@width`)
/// flow through the same single pixel-conversion path.
fn derive_default_col_width(base_col_width: u32) -> f64 {
    base_col_width as f64
}

/// `sheetView@showGridLines` (§18.3.1.88); default true when absent.
/// `pub(crate)`: reused verbatim by the Grid-v2 husk parse (`sml::index`).
pub(crate) fn grid_lines_enabled(sheet: &ResolvedSheet) -> bool {
    match sheet.sheet_views() {
        Some(views) => match views.sheet_view.first() {
            Some(view) => show_grid_lines(view).unwrap_or(true),
            None => true,
        },
        None => true,
    }
}

/// `SheetView.show_grid_lines` accessor (sml-styling-gated field; always
/// present under the `sml` feature).
fn show_grid_lines(view: &ooxml_sml::types::SheetView) -> Option<bool> {
    view.show_grid_lines
}

/// The sheet's FROZEN pane (§18.3.1.66), if any — mirrors `grid_lines_enabled`'s
/// first-sheetView read (the same husk-parse seam). `state="frozen"` and
/// `"frozenSplit"` are honored (the frozen part of a frozenSplit behaves the
/// same); `state="split"` (a draggable non-frozen splitter, 1/20-pt units) and
/// an absent state are IGNORED — no pane. For the frozen states `xSplit`/
/// `ySplit` are COLUMN/ROW COUNTS; weird values (negative, NaN, fractional)
/// are guarded by truncation-to-zero-floor. Both-zero splits → `None`.
pub(crate) fn frozen_pane(sheet: &ResolvedSheet) -> Option<super::pane::PaneInfo> {
    use ooxml_sml::types::PaneState;
    let view = sheet.sheet_views()?.sheet_view.first()?;
    let pane = view.pane.as_deref()?;
    if !matches!(pane.state, Some(PaneState::Frozen | PaneState::FrozenSplit)) {
        return None;
    }
    // Counts: guard non-finite/negative (→ 0); `as u32` truncates fractions and
    // saturates huge values (Rust float→int cast semantics).
    let count = |v: Option<f64>| -> u32 {
        match v {
            Some(x) if x.is_finite() && x > 0.0 => x as u32,
            _ => 0,
        }
    };
    let x_split = count(pane.x_split);
    let y_split = count(pane.y_split);
    if x_split == 0 && y_split == 0 {
        return None;
    }
    // `topLeftCell` — the body pane's saved top-left (`parse_a1` → (col, row)).
    let top_left = pane
        .top_left_cell
        .as_deref()
        .and_then(parse_a1)
        .map(|(col, row)| (row, col));
    Some(super::pane::PaneInfo { x_split, y_split, top_left })
}

/// Resolve the normal-style font family name from the workbook stylesheet
/// (`fonts[0].name`), or [`DEFAULT_NORMAL_FONT`] when absent.
/// SIMPLIFICATION: the *true* normal style is the first cellStyleXfs/cellXfs fontId (not resolved).
pub(crate) fn normal_font_name(stylesheet: &ooxml_sml::types::Stylesheet) -> String {
    stylesheet_first_font_name(stylesheet).unwrap_or_else(|| DEFAULT_NORMAL_FONT.to_string())
}

fn stylesheet_first_font_name(stylesheet: &ooxml_sml::types::Stylesheet) -> Option<String> {
    stylesheet
        .fonts
        .as_ref()
        .and_then(|fonts| fonts.font.first())
        .and_then(|font| font.name.as_ref())
        .map(|n| n.value.clone())
}

// ─── per-cell style resolution (§18.8.45 CT_Xf) ───────────────────────────────

/// Resolves a cell `style_index` (`@s`) into a fully-built [`CellStyle`] by
/// walking `cellXfs[s]` → the font/fill/border/numFmt sub-tables (§18.8). Built
/// styles are cached by index so a sheet with N distinct styles does the lookup
/// work N times, not once per cell.
///
/// `apply_*` flags: Excel writes `applyNumberFormat`/`applyAlignment` to gate
/// *those* overrides, but in practice applies the referenced font/fill/border
/// whenever a non-default id is set (the `apply*` flags are hints, frequently
/// omitted — the acceptance fixtures set fontId/fillId/borderId WITHOUT
/// applyFont/applyFill/applyBorder yet must render their styling). So we apply a
/// referenced font/fill/border when its id is non-zero OR its apply flag is on,
/// and gate the number format on `applyNumberFormat` (or a non-zero id).
pub(crate) struct StyleResolver<'a> {
    stylesheet: &'a Stylesheet,
    colors: &'a ColorCtx,
    cache: HashMap<u32, Option<CellStyle>>,
}

impl<'a> StyleResolver<'a> {
    pub(crate) fn new(stylesheet: &'a Stylesheet, colors: &'a ColorCtx) -> Self {
        StyleResolver { stylesheet, colors, cache: HashMap::new() }
    }

    /// Resolve (and cache) the [`CellStyle`] for `style_index`. Returns `None`
    /// when the xf resolves to the bare normal style (no font/fill/border/numFmt
    /// override) — those cells keep the default render path.
    pub(crate) fn resolve(&mut self, style_index: u32) -> Option<&CellStyle> {
        if !self.cache.contains_key(&style_index) {
            let built = self.build(style_index);
            self.cache.insert(style_index, built);
        }
        self.cache.get(&style_index).and_then(|o| o.as_ref())
    }

    fn build(&self, style_index: u32) -> Option<CellStyle> {
        let xf = self
            .stylesheet
            .cell_xfs
            .as_ref()?
            .xf
            .get(style_index as usize)?;

        let mut style = CellStyle::default();

        // ── Number format (§18.8.30) — applied when a non-default id is set ──
        // (`applyNumberFormat` is the hint flag, read off the Format field; we
        // apply whenever the id is non-zero, matching the acceptance fixtures.)
        let num_fmt_id = xf.number_format_id();
        if num_fmt_id != 0 {
            // Custom format codes arrive with XML entities UNDECODED (the parser
            // hands back `&quot;$&quot;#,##0.00`); decode them before the numfmt
            // interpreter splits on `;` (decode first, then split).
            let decoded: Vec<(u32, String)> = self
                .stylesheet
                .num_fmts
                .as_ref()
                .map(|nfs| {
                    nfs.num_fmt
                        .iter()
                        // The parser hands the format code through with entities
                        // undecoded (e.g. `&quot;`); the numfmt interpreter must
                        // see the literal `"` so its quote-aware `;`-split and
                        // quoted-literal handling work.
                        .map(|nf| {
                            (
                                nf.number_format_id,
                                crate::engine::vml::decode_xml_entities(&nf.format_code),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let custom = decoded.iter().map(|(id, code)| (*id, code.as_str()));
            style.num_fmt_code = numfmt::resolve_format_code(num_fmt_id, custom);
        }

        // ── Font (§18.8.22) — apply when fontId set or applyFont on ──────────
        // §18.8.45: apply the referenced font formatting when a non-default font
        // is set (fontId != 0, the common case) OR `applyFont` is explicitly on
        // (id==0 & applyFont="1" = "apply the default font" — rare but valid).
        // The fork's `apply_font()` is `unwrap_or(false)`, so an ABSENT applyFont
        // with a non-zero fontId still applies (id != 0 covers it), matching the
        // numFmt heuristic above.
        let font_id = xf.font_id();
        if font_id != 0 || xf.apply_font() {
            if let Some(font) = self
                .stylesheet
                .fonts
                .as_ref()
                .and_then(|fs| fs.font.get(font_id as usize))
            {
                // §18.8.2/.5: a present boolean property with no `@val` means
                // `true` (`<b/>` ⇒ bold). The fork's `is_bold()` reads
                // `val.unwrap_or(false)`, which would mis-read the common
                // val-less form — so we read the element presence directly.
                style.bold = bool_prop_on(font.b.as_deref());
                style.italic = bool_prop_on(font.i.as_deref());
                style.strikethrough = bool_prop_on(font.strike.as_deref());
                style.font_size_pt = font.font_size();
                style.font_family = font.font_name().map(|s| s.to_string());
                style.font_color_hex = font.font_color().and_then(|c| self.colors.resolve(c));
            }
        }

        // ── Fill (§18.8.20) — solid pattern fgColor; apply when fillId set ───
        // §18.8.45: fillId != 0 OR applyFill on (see the font note above).
        let fill_id = xf.fill_id();
        if fill_id != 0 || xf.apply_fill() {
            if let Some(fill) = self
                .stylesheet
                .fills
                .as_ref()
                .and_then(|fs| fs.fill.get(fill_id as usize))
            {
                style.fill_hex = solid_fill_hex(fill, self.colors);
            }
        }

        // ── Border (§18.8.4) — per-edge style+colour; apply when borderId set ─
        // §18.8.45: borderId != 0 OR applyBorder on (see the font note above).
        let border_id = xf.border_id();
        if border_id != 0 || xf.apply_border() {
            if let Some(border) = self
                .stylesheet
                .borders
                .as_ref()
                .and_then(|bs| bs.border.get(border_id as usize))
            {
                style.borders = [
                    border.left_border().and_then(|b| border_edge(b, self.colors)),
                    border.right_border().and_then(|b| border_edge(b, self.colors)),
                    border.top_border().and_then(|b| border_edge(b, self.colors)),
                    border.bottom_border().and_then(|b| border_edge(b, self.colors)),
                ];
            }
        }

        // ── Alignment (§18.8.1) — gated on applyAlignment ────────────────────
        if xf.apply_alignment() {
            if let Some(al) = xf.alignment() {
                style.h_align_override = al.horizontal_alignment().and_then(halign_from_sml);
            }
        }

        // Nothing meaningful resolved → no style (keep the default path).
        if style == CellStyle::default() {
            None
        } else {
            Some(style)
        }
    }
}

/// An OOXML boolean toggle property (`<b/>`, `<i/>`, `<strike/>`): the element's
/// mere presence means `true` unless `@val="0"` explicitly turns it off
/// (§18.8.2). The element being absent is `false`.
fn bool_prop_on(prop: Option<&ooxml_sml::types::BooleanProperty>) -> bool {
    match prop {
        Some(p) => p.value.unwrap_or(true),
        None => false,
    }
}

// ─── colour resolution context (rgb → theme(+tint) → indexed → auto) ─────────

/// The §18.8.27 default indexed palette (legacy colour indexes 0–63). Entries
/// 0–7 duplicate 8–15 per the spec's table. Indexes 64 (system foreground) and
/// 65 (system background) are NOT palette entries — they resolve to `None`
/// (auto), letting each callsite keep its own default ink.
const DEFAULT_INDEXED_PALETTE: [&str; 64] = [
    "#000000", "#FFFFFF", "#FF0000", "#00FF00", "#0000FF", "#FFFF00", "#FF00FF", "#00FFFF",
    "#000000", "#FFFFFF", "#FF0000", "#00FF00", "#0000FF", "#FFFF00", "#FF00FF", "#00FFFF",
    "#800000", "#008000", "#000080", "#808000", "#800080", "#008080", "#C0C0C0", "#808080",
    "#9999FF", "#993366", "#FFFFCC", "#CCFFFF", "#660066", "#FF8080", "#0066CC", "#CCCCFF",
    "#000080", "#FF00FF", "#FFFF00", "#00FFFF", "#800080", "#800000", "#008080", "#0000FF",
    "#00CCFF", "#CCFFFF", "#CCFFCC", "#FFFF99", "#99CCFF", "#FF99CC", "#CC99FF", "#FFCC99",
    "#3366FF", "#33CCCC", "#99CC00", "#FFCC00", "#FF9900", "#FF6600", "#666699", "#969696",
    "#003366", "#339966", "#003300", "#333300", "#993300", "#993366", "#333399", "#333333",
];

/// Workbook-level colour resolution context: the theme palette in Excel's
/// `color@theme` INDEX order plus the effective indexed palette. Built once per
/// workbook in `SmlRenderer::from_xlsx` (the theme self-parsed from
/// `xl/theme/theme1.xml` through the load's shared OPC package) and
/// threaded into every [`StyleResolver`].
///
/// `theme` slot order is the documented Excel swap of the §20.1.6.2 clrScheme:
/// index 0→lt1, 1→dk1, 2→lt2, 3→dk2 (dk1/lt1 and dk2/lt2 SWAPPED vs the XML's
/// dk-first order), 4–9→accent1-6, 10→hlink, 11→folHlink. An empty string marks
/// an unresolved slot (missing theme part) → that colour resolves to `None`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColorCtx {
    pub(crate) theme: [String; 12],
    pub(crate) indexed: Vec<String>,
}

impl Default for ColorCtx {
    fn default() -> Self {
        ColorCtx {
            theme: Default::default(),
            indexed: DEFAULT_INDEXED_PALETTE.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl ColorCtx {
    /// Build from the self-parsed theme slots plus the stylesheet's optional
    /// `<colors>/<indexedColors>` overlay (§18.8.27: when present, `rgbColor`
    /// entries REPLACE the default palette by position).
    pub(crate) fn build(theme: [String; 12], stylesheet: &Stylesheet) -> Self {
        let mut ctx = ColorCtx { theme, ..ColorCtx::default() };
        if let Some(ic) = stylesheet.colors.as_ref().and_then(|c| c.indexed_colors.as_ref()) {
            for (i, rc) in ic.rgb_color.iter().enumerate() {
                if let Some(hex) = rgb_bytes_to_hex(rc.rgb.as_ref()) {
                    if i < ctx.indexed.len() {
                        ctx.indexed[i] = hex;
                    } else {
                        ctx.indexed.push(hex);
                    }
                }
            }
        }
        ctx
    }

    /// Resolve an SML `Color` (§18.3.1.15) to `#RRGGBB` with this precedence:
    /// explicit `@rgb` → `@theme` (+`@tint`, §18.8.19 HSL-luminance math) →
    /// `@indexed` (§18.8.27) → auto/`None`. Indexed 64/65 (system fore/back)
    /// fall off the palette and resolve `None` (auto) — callers keep their own
    /// defaults (near-black text, `#808080` borders).
    pub(crate) fn resolve(&self, color: &ooxml_sml::types::Color) -> Option<String> {
        if let Some(hex) = rgb_bytes_to_hex(color.rgb.as_ref()) {
            return Some(hex);
        }
        if let Some(t) = color.theme {
            let slot = self.theme.get(t as usize).filter(|s| !s.is_empty())?;
            return Some(crate::engine::color::apply_excel_tint(
                slot,
                color.tint.unwrap_or(0.0),
            ));
        }
        if let Some(i) = color.indexed {
            return self.indexed.get(i as usize).cloned();
        }
        None
    }
}

/// Decoded ARGB/RGB bytes (`STUnsignedIntHex`) → `#RRGGBB` (the 4-byte ARGB
/// form drops its leading alpha byte; a bare 3-byte RGB is used as-is).
fn rgb_bytes_to_hex(bytes: Option<&impl AsRef<[u8]>>) -> Option<String> {
    let (r, g, b) = match bytes?.as_ref() {
        [_a, r, g, b] => (*r, *g, *b),
        [r, g, b] => (*r, *g, *b),
        _ => return None,
    };
    Some(format!("#{r:02X}{g:02X}{b:02X}"))
}

/// The solid-fill colour of a `Fill`: only `patternType="solid"` contributes a
/// flat fill, taken from its `fgColor` (§18.8.32) resolved through the
/// [`ColorCtx`]. Other pattern types (`gray125`, `none`) and gradient fills are
/// not drawn (DEFERRED).
fn solid_fill_hex(fill: &ooxml_sml::types::Fill, colors: &ColorCtx) -> Option<String> {
    let pf = fill.pattern_fill()?;
    if pf.pattern_type() != Some(ooxml_sml::types::PatternType::Solid) {
        return None;
    }
    pf.foreground_color().and_then(|c| colors.resolve(c))
}

/// Map a `BorderProperties` to a drawn [`BorderEdge`]: a styled edge becomes a
/// solid stroke whose pixel width approximates its style class (§18.8.4). The
/// colour resolves through the [`ColorCtx`] (rgb/theme+tint/indexed) and
/// defaults to a mid-gray when the edge declares none or it resolves auto
/// (Excel's default auto border colour reads as a dark gridline).
/// SIMPLIFICATION: dash patterns/double rails collapse to a solid stroke
/// of the class width.
fn border_edge(bp: &ooxml_sml::types::BorderProperties, colors: &ColorCtx) -> Option<BorderEdge> {
    use ooxml_sml::types::BorderStyle;
    let style = bp.border_style()?;
    let width_px = match style {
        BorderStyle::None => return None,
        BorderStyle::Hair => 0.5,
        BorderStyle::Thin | BorderStyle::Dashed | BorderStyle::Dotted | BorderStyle::DashDot
        | BorderStyle::DashDotDot => 1.0,
        BorderStyle::Medium
        | BorderStyle::MediumDashed
        | BorderStyle::MediumDashDot
        | BorderStyle::MediumDashDotDot
        | BorderStyle::SlantDashDot
        | BorderStyle::Double => 2.0,
        BorderStyle::Thick => 3.0,
    };
    let color = bp
        .border_color()
        .and_then(|c| colors.resolve(c))
        .unwrap_or_else(|| "#808080".to_string());
    Some(BorderEdge { width_px, color, dash: None })
}

/// Map an SML horizontal alignment to the model's [`HAlign`] (§18.8.1). Only
/// the three common placements are honoured; `fill`/`justify`/`distributed`/
/// `centerContinuous` fall back to their nearest match (`general` → `None`).
fn halign_from_sml(h: ooxml_sml::types::HorizontalAlignment) -> Option<HAlign> {
    use ooxml_sml::types::HorizontalAlignment as H;
    match h {
        H::Left => Some(HAlign::Left),
        H::Right => Some(HAlign::Right),
        H::Center | H::CenterContinuous => Some(HAlign::Center),
        // general / fill / justify / distributed → keep value-kind general.
        _ => None,
    }
}
