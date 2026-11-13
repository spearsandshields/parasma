//! Slide table layout (DrawingML `a:tbl` inside a `p:graphicFrame`).
//! Web-sys-free so the geometry is natively unit-testable: the renderer
//! ([`super::PmlRenderer`]) captures each table's DML [`CTTable`] paired with its
//! containing graphicFrame `a:xfrm` box at load, and [`build_slide_table`] turns
//! that into a fixed-position [`SlideTable`] draw list; `mod.rs::render_page`
//! paints it through `engine::paint`.
//!
//! Spec grounding (ECMA-376 Part 1, DrawingML):
//!  - §21.1.3.13  `a:tbl` — the table (grid + rows + cells).
//!  - §21.1.3.14  `a:tblGrid` / §21.1.3.2 `a:gridCol@w` — column widths (EMU).
//!  - §21.1.3.18  `a:tr@h` — row height (EMU); §21.1.3.16 `a:tc` — a cell.
//!  - §21.1.3.17  `a:tcPr` — cell properties: `lnL/R/T/B` borders
//!    (§20.1.2.2.24 LineProperties), `solidFill`, `marL/R/T/B`,
//!    `anchor`. §20.1.2.2.43 `a:txBody` — the cell text (the SAME
//!    DML TextBody as a shape, so the slide text painter is reused).
//!  - §19.3.1.16  `p:graphicFrame/p:xfrm` — the table's position/size on the
//!    slide (the frame box; tables are absolutely positioned — no
//!    flow, no pagination).
//!
//! Floor (honest, recognizable):
//!  - Column x-rails = `gridCol@w` prefix sums; row y-rails = `tr@h` prefix sums;
//!    origin = the graphicFrame `xfrm` off. Per-cell solid fill + 4-edge borders
//!    (dash captured AND painted — §20.1.10.48, via the shared
//!    `apply_line_dash` painter) + cell text via the shared
//!    `build_paragraphs_from_styled`, inset by `tcPr` margins.
//!  - MERGES: `gridSpan` widens the spanning cell across its columns and
//!    `rowSpan` grows it down its rows (§21.1.3.16); the `hMerge`/`vMerge`
//!    continuation cells are skipped so the grown box fills their gap. A table
//!    missing the strict-required `gridCol@w` / `tr@h` is dropped whole at
//!    parse time (`parse_as::<CTTable>().ok()` in the fork extractor) — the
//!    strict-attribute tolerance: dropped, not repaired.
//!
//! TABLE STYLES (§21.1.3): `<a:tblPr>` carries the part flags
//! (`firstRow`/`firstCol`/`lastRow`/`lastCol`/`bandRow`/`bandCol`) + a
//! `<a:tableStyleId>` GUID into `ppt/tableStyles.xml` (the OPC part, self-parsed
//! once per deck like the theme/diagram parts — [`parse_table_styles`]). A
//! matched `<a:tblStyle>` is a set of PARTS (`wholeTbl`, `band1H`/`band2H` row
//! bands, `band1V`/`band2V` col bands, `firstRow`/`lastRow`/`firstCol`/`lastCol`,
//! `nw/ne/sw/seCell` corners). Per cell we pick the parts that apply by
//! row/col position + the tblPr flags + banding parity, then LAYER them in the
//! §21.1.3 precedence ([`resolve_cell_style`]) so the resolved fill / text colour
//! / bold / borders become the cell's DEFAULTS. An explicit `tcPr` solidFill /
//! `ln` / run colour STILL wins (a cell with its own fill keeps that fill;
//! explicit run text keeps its own colour) — the override precedence. A built-in
//! GUID not present in the part leaves the table unstyled (the built-in style
//! table is out of scope). Banding / first-row / etc. apply only when their
//! tblPr flag is set. Scheme colours resolve through the shared [`SlideTheme`]
//! schemeClr machinery (tint/lumMod/shade transforms included).

#![cfg_attr(not(test), allow(dead_code))]

use crate::engine::fonts::FontCache;
use crate::engine::units::emu_to_px;

use ooxml_dml::types::CTTable;

use super::shape::{
    build_paragraphs_from_styled, color_choice_to_hex, solid_fill_hex, solid_line_fill_hex,
    CellTextDefaults, ShapeOutline, SlideParagraph, SlideTheme, BODY_DEFAULT_PT, DEFAULT_OUTLINE_PX,
};

/// Default cell text inset (EMU) when `tcPr` omits a `marL/R/T/B` (§21.1.3.17 —
/// PowerPoint's defaults are 0.1in L/R = 91440 EMU, 0.05in T/B = 45720 EMU).
const DEFAULT_MAR_LR_EMU: i64 = 91_440;
const DEFAULT_MAR_TB_EMU: i64 = 45_720;

/// The deck's parsed table-style table (`ppt/tableStyles.xml`, §21.1.3.11),
/// self-parsed once per presentation in `PmlRenderer::from_pptx` and threaded
/// into [`build_slide_table`]. Empty when the part is absent / unparsable — every
/// table then renders unstyled (the plain hairline grid). Wrapped so a table
/// whose `tableStyleId` matches no `tblStyle` here (a built-in GUID not written
/// into the part) also falls back to unstyled.
#[derive(Debug, Clone, Default)]
pub(crate) struct TableStyles {
    /// styleId GUID (verbatim, incl. braces) → the parsed `<a:tblStyle>`.
    by_id: std::collections::HashMap<String, ooxml_dml::types::CTTableStyle>,
}

impl TableStyles {
    /// Look up the `<a:tblStyle>` for a `<a:tableStyleId>` GUID (verbatim match,
    /// braces included — both are written the same by PowerPoint). `None` when the
    /// GUID is absent (a built-in style not in the part) → the caller renders the
    /// table unstyled.
    fn get(&self, id: &str) -> Option<&ooxml_dml::types::CTTableStyle> {
        self.by_id.get(id)
    }
}

/// Self-parse `ppt/tableStyles.xml` into a [`TableStyles`] lookup (the OPC-part
/// self-parse precedent the theme/diagram parts use — the fork exposes no typed
/// accessor for this part). Parses the whole `<a:tblStyleLst>` via the fork's
/// `CTTableStyleList::from_xml`, then keys each `<a:tblStyle>` by its `styleId`.
/// A parse miss / absent part yields an empty table (every table unstyled). The
/// `xmlns:a` namespace prefix is irrelevant — the fork parser keys on
/// `local_name()`, exactly like the diagram `dsp:` self-parse.
pub(crate) fn parse_table_styles(xml: &str) -> TableStyles {
    use ooxml_dml::parsers::FromXml;
    use ooxml_dml::types::CTTableStyleList;
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"tblStyleLst" => {
                if let Ok(list) = CTTableStyleList::from_xml(&mut reader, &e, false) {
                    let by_id = list
                        .tbl_style
                        .into_iter()
                        .map(|s| (s.style_id.clone(), s))
                        .collect();
                    return TableStyles { by_id };
                }
                return TableStyles::default();
            }
            Ok(Event::Eof) | Err(_) => return TableStyles::default(),
            _ => {}
        }
        buf.clear();
    }
}

/// The `<a:tblPr>` part-application flags (§21.1.3.15) — which conditional table-
/// style parts the deck enabled. Banding/first-row/etc. parts apply ONLY when
/// their flag is set (an unset flag suppresses that part even if the style
/// defines it). Read off `CTTableProperties`; all default `false` (no `tblPr` →
/// every flag off → only `wholeTbl` applies).
#[derive(Debug, Clone, Copy, Default)]
struct TblPrFlags {
    first_row: bool,
    first_col: bool,
    last_row: bool,
    last_col: bool,
    band_row: bool,
    band_col: bool,
}

impl TblPrFlags {
    fn from_tbl_pr(pr: Option<&ooxml_dml::types::CTTableProperties>) -> Self {
        match pr {
            None => Self::default(),
            Some(p) => Self {
                first_row: p.first_row.unwrap_or(false),
                first_col: p.first_col.unwrap_or(false),
                last_row: p.last_row.unwrap_or(false),
                last_col: p.last_col.unwrap_or(false),
                band_row: p.band_row.unwrap_or(false),
                band_col: p.band_col.unwrap_or(false),
            },
        }
    }
}

/// A cell's resolved table-style defaults: the fill, text colour + bold, and the
/// four edge borders the layered style parts produced. These become the cell's
/// DEFAULTS — an explicit `tcPr` fill / `ln` / run colour overrides them.
#[derive(Debug, Clone, Default)]
struct ResolvedCellStyle {
    fill: Option<String>,
    text: CellTextDefaults,
    border_l: Option<ShapeOutline>,
    border_r: Option<ShapeOutline>,
    border_t: Option<ShapeOutline>,
    border_b: Option<ShapeOutline>,
}

/// Layer the table-style parts that apply to cell `(r, c)` (0-based) of an
/// `nrows × ncols` grid, in the DrawingML §21.1.3 precedence (LATER wins):
///   `wholeTbl` → `band1H`/`band2H` → `band1V`/`band2V` → `lastCol` → `firstCol`
///            → `lastRow` → `firstRow` → `se/sw/ne/nwCell` (corners — strongest).
/// The first/last EDGE parts sit ABOVE the bands (matching PowerPoint), so a
/// banded body cell that is ALSO the first column/row takes the SOLID edge fill
/// (PowerPoint paints such a first column solid over the body band).
/// A part contributes only when its tblPr flag is set ([`TblPrFlags`]); banding
/// excludes the first/last row (H) and first/last col (V) and alternates by
/// parity (band1 = the FIRST banded slot). Each part's `tcStyle` fill /
/// `tcTxStyle` colour+bold / `tcBdr` borders overlay the accumulator (a `None`
/// in a later part leaves the earlier value — overlay, not replace).
fn resolve_cell_style(
    style: &ooxml_dml::types::CTTableStyle,
    flags: &TblPrFlags,
    r: usize,
    c: usize,
    nrows: usize,
    ncols: usize,
    theme: &SlideTheme,
) -> ResolvedCellStyle {
    let mut acc = ResolvedCellStyle::default();

    let is_first_row = flags.first_row && r == 0;
    let is_last_row = flags.last_row && r + 1 == nrows;
    let is_first_col = flags.first_col && c == 0;
    let is_last_col = flags.last_col && c + 1 == ncols;

    // Body region for banding = rows/cols NOT consumed by first/last. band1 is the
    // FIRST banded slot in that region (parity on the in-body index).
    let row_band = if flags.band_row && !is_first_row && !is_last_row {
        let first_excl = if flags.first_row { 1 } else { 0 };
        Some((r - first_excl).is_multiple_of(2)) // true → band1H, false → band2H
    } else {
        None
    };
    let col_band = if flags.band_col && !is_first_col && !is_last_col {
        let first_excl = if flags.first_col { 1 } else { 0 };
        Some((c - first_excl).is_multiple_of(2)) // true → band1V, false → band2V
    } else {
        None
    };

    // Precedence order (weakest → strongest); `overlay_part` no-ops on `None`.
    // The DrawingML schema / PowerPoint order: wholeTbl → row bands → col bands →
    // lastCol → firstCol → lastRow → firstRow → corners. The row/col EDGE parts
    // (first/last) sit ABOVE the bands, so a banded body cell that is ALSO the
    // first column takes the SOLID firstCol fill (PowerPoint paints such a first
    // column solid over the body band — bands lose to firstCol/firstRow).
    overlay_part(&mut acc, style.whole_tbl.as_deref(), theme);
    if let Some(b1) = row_band {
        let part = if b1 { &style.band1_h } else { &style.band2_h };
        overlay_part(&mut acc, part.as_deref(), theme);
    }
    if let Some(b1) = col_band {
        let part = if b1 { &style.band1_v } else { &style.band2_v };
        overlay_part(&mut acc, part.as_deref(), theme);
    }
    if is_last_col {
        overlay_part(&mut acc, style.last_col.as_deref(), theme);
    }
    if is_first_col {
        overlay_part(&mut acc, style.first_col.as_deref(), theme);
    }
    if is_last_row {
        overlay_part(&mut acc, style.last_row.as_deref(), theme);
    }
    if is_first_row {
        overlay_part(&mut acc, style.first_row.as_deref(), theme);
    }
    // Corners (strongest): the four grid corners when BOTH their row+col flags hit.
    if is_first_row && is_first_col {
        overlay_part(&mut acc, style.nw_cell.as_deref(), theme);
    }
    if is_first_row && is_last_col {
        overlay_part(&mut acc, style.ne_cell.as_deref(), theme);
    }
    if is_last_row && is_first_col {
        overlay_part(&mut acc, style.sw_cell.as_deref(), theme);
    }
    if is_last_row && is_last_col {
        overlay_part(&mut acc, style.se_cell.as_deref(), theme);
    }

    acc
}

/// Overlay one `<a:tblStylePr>`-style PART (`wholeTbl`/`band1H`/…) onto the
/// accumulator: its `tcStyle/fill/solidFill`, `tcTxStyle` colour+bold, and the
/// four `tcBdr` edge lines each replace the accumulator's value WHEN PRESENT (a
/// missing sub-element leaves the earlier value — overlay, not reset).
fn overlay_part(
    acc: &mut ResolvedCellStyle,
    part: Option<&ooxml_dml::types::CTTablePartStyle>,
    theme: &SlideTheme,
) {
    let Some(part) = part else {
        return;
    };

    // Fill: tcStyle/fill is an EGThemeableFillStyle::Fill(CTFillProperties) — its
    // inner `fill_properties` is the SAME EGFillProperties the shared
    // `solid_fill_hex` consumes (solidFill → schemeClr/srgbClr → #rrggbb).
    if let Some(tc_style) = part.tc_style.as_deref() {
        if let Some(fill) = part_fill_hex(tc_style.themeable_fill_style.as_deref(), theme) {
            acc.fill = Some(fill);
        }
        // Borders: each tcBdr edge is a CTThemeableLineStyle with an `ln` whose
        // solid line fill → ShapeOutline (the same width/colour logic as a cell
        // edge). A part edge present → it replaces that edge's accumulator value.
        if let Some(bdr) = tc_style.tc_bdr.as_deref() {
            if let Some(o) = themeable_line_outline(bdr.left.as_deref(), theme) {
                acc.border_l = Some(o);
            }
            if let Some(o) = themeable_line_outline(bdr.right.as_deref(), theme) {
                acc.border_r = Some(o);
            }
            if let Some(o) = themeable_line_outline(bdr.top.as_deref(), theme) {
                acc.border_t = Some(o);
            }
            if let Some(o) = themeable_line_outline(bdr.bottom.as_deref(), theme) {
                acc.border_b = Some(o);
            }
        }
    }

    // Text colour + bold: tcTxStyle/color_choice → #rrggbb; @b="on" → bold.
    if let Some(tx) = part.tc_tx_style.as_deref() {
        if let Some(hex) =
            color_choice_to_hex(tx.color_choice.as_deref(), theme).map(|(h, _a)| h)
        {
            acc.text.color = Some(hex);
        }
        use ooxml_dml::types::STOnOffStyleType;
        match tx.b {
            Some(STOnOffStyleType::On) => acc.text.bold = true,
            Some(STOnOffStyleType::Off) => acc.text.bold = false,
            _ => {} // Def / absent → leave the accumulator's bold
        }
    }
}

/// Resolve a table-style part's `tcStyle/fill` to `#rrggbb`. The part fill is an
/// `EGThemeableFillStyle::Fill(CTFillProperties)`; its inner `fill_properties` is
/// the standard `EGFillProperties` (solidFill → scheme/srgb), so the shared
/// [`solid_fill_hex`] does the resolve (incl. tint/lumMod/shade transforms). A
/// `fillRef` (style-matrix indirection) is out of scope → `None`.
fn part_fill_hex(
    fill: Option<&ooxml_dml::types::EGThemeableFillStyle>,
    theme: &SlideTheme,
) -> Option<String> {
    use ooxml_dml::types::EGThemeableFillStyle;
    match fill? {
        EGThemeableFillStyle::Fill(cf) => solid_fill_hex(cf.fill_properties.as_deref(), theme),
        EGThemeableFillStyle::FillRef(_) => None,
    }
}

/// Convert a `CTThemeableLineStyle` (a tcBdr edge) with a solid `ln` into a
/// [`ShapeOutline`] — its `ln` is a standard `LineProperties`, reused through the
/// existing [`line_to_outline`]. An `lnRef` (style-matrix) or a no-fill line →
/// `None` (no border on that edge).
fn themeable_line_outline(
    edge: Option<&ooxml_dml::types::CTThemeableLineStyle>,
    theme: &SlideTheme,
) -> Option<ShapeOutline> {
    line_to_outline(edge?.line.as_deref(), theme)
}

/// One table captured before layout: the DML table paired with its containing
/// graphicFrame `a:xfrm` box (EMU). Built in `PmlRenderer::from_pptx` while the
/// fork `Presentation` is still alive (the frame xfrm is read off
/// `slide.inner()` — `Slide::tables()` returns the `CTTable` but NOT the frame
/// box, so the renderer walks the graphic_frame list itself to pair them).
#[derive(Debug, Clone)]
pub(crate) struct TableItem {
    /// Frame box top-left in EMU (`p:graphicFrame/p:xfrm/a:off`).
    pub off_x: i64,
    pub off_y: i64,
    /// Frame box size in EMU (`.../a:ext`). Unused for the cell grid (the grid
    /// is sized by gridCol/tr sums) but kept for a future fit/clip pass.
    pub ext_cx: i64,
    pub ext_cy: i64,
    /// The parsed DML table.
    pub table: CTTable,
}

/// One table laid out as a fixed grid of cell boxes (the draw list). Painted as
/// one ordered item among the slide's shapes (after the placeholder shapes).
#[derive(Debug, Clone)]
pub(crate) struct SlideTable {
    pub cells: Vec<TableCell>,
}

/// One laid-out table cell: its pixel box, optional solid fill, four optional
/// solid borders, and the inset text paragraphs. Cells that are merge
/// continuations (`hMerge`/`vMerge`) are skipped at build time (the spanning
/// cell's grown box covers them) so they contribute no box.
#[derive(Debug, Clone)]
pub(crate) struct TableCell {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub fill_hex: Option<String>,
    pub border_l: Option<ShapeOutline>,
    pub border_r: Option<ShapeOutline>,
    pub border_t: Option<ShapeOutline>,
    pub border_b: Option<ShapeOutline>,
    /// Inset text box top-left (px) — the cell box inset by the `tcPr` margins.
    pub text_x: f64,
    pub text_y: f64,
    /// Inset text box width (px) — the cell width minus L+R margins, floored at 0.
    pub text_w: f64,
    /// Vertical anchoring of the text block within the cell (`tcPr@anchor`).
    pub anchor: CellAnchor,
    pub paragraphs: Vec<SlideParagraph>,
}

/// Vertical anchoring of a cell's text block (`tcPr@anchor`, §21.1.3.17).
/// Resolves top/centre/bottom; `just`/`dist` collapse to top (honest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellAnchor {
    Top,
    Center,
    Bottom,
}

/// Build the [`SlideTable`] draw list from a captured [`TableItem`].
///
/// Geometry: column x-rails are the prefix sums of `gridCol@w` offset by the
/// frame `off_x`; row y-rails the prefix sums of `tr@h` offset by `off_y`
/// (EMU → px). Each cell's box is `[col_x[c], col_x[c+1]) × [row_y[r], row_y[r+1])`.
/// A cell that is a horizontal/vertical merge continuation is skipped.
pub(crate) fn build_slide_table(
    item: &TableItem,
    styles: &TableStyles,
    theme: &SlideTheme,
    font_cache: &FontCache,
) -> SlideTable {
    use ooxml_dml::TableExt;

    let t = &item.table;

    // Table styles: resolve the `<a:tblPr>` flags + the matched `<a:tblStyle>`
    // once for the whole table. A missing/empty tableStyleId, an empty style
    // table, or a GUID not in the part → `None` (every cell renders unstyled, the
    // plain hairline grid). `nrows`/`ncols` drive last-row/last-col detection.
    let tbl_pr = t.tbl_pr.as_deref();
    let flags = TblPrFlags::from_tbl_pr(tbl_pr);
    let style = tbl_pr
        .and_then(|pr| pr.table_style_id.as_deref())
        .filter(|id| !id.is_empty())
        .and_then(|id| styles.get(id));
    let nrows = t.rows().len();
    let ncols = t.tbl_grid.grid_col.len();

    // Column x-rails (EMU) — prefix sums of gridCol@w, origin at the frame off.
    let mut col_x = Vec::with_capacity(t.tbl_grid.grid_col.len() + 1);
    let mut acc = item.off_x;
    col_x.push(acc);
    for gc in &t.tbl_grid.grid_col {
        acc += parse_coord(&gc.width);
        col_x.push(acc);
    }

    // Row y-rails (EMU) — prefix sums of tr@h, origin at the frame off.
    let mut row_y = Vec::with_capacity(t.rows().len() + 1);
    let mut accy = item.off_y;
    row_y.push(accy);
    for tr in t.rows() {
        accy += parse_coord(&tr.height);
        row_y.push(accy);
    }

    let mut cells = Vec::new();
    for (r, tr) in t.rows().iter().enumerate() {
        // Walk the cells in document order, advancing a column cursor by each
        // cell's gridSpan so an un-merged grid still lands in the right columns.
        let mut c = 0usize;
        for tc in &tr.tc {
            let span = tc.grid_span.map(|s| s.max(1) as usize).unwrap_or(1);
            let col_start = c;
            let col_end = (c + span).min(t.tbl_grid.grid_col.len());
            c = col_end;

            // Skip merge continuations: an hMerge/vMerge
            // cell is the tail of a span and draws nothing of its own.
            let is_merge_cont = tc.h_merge.unwrap_or(false) || tc.v_merge.unwrap_or(false);
            if is_merge_cont {
                continue;
            }

            // The cell box spans [col_start, col_end) horizontally (gridSpan)
            // and [r, r + rowSpan) vertically (§21.1.3.16 tc@rowSpan: the
            // spanning cell's box grows down its covered rows; the vMerge
            // continuation cells were already skipped above, so the grown box
            // fills the gap they leave).
            let rspan = tc.row_span.map(|s| s.max(1) as usize).unwrap_or(1);
            let row_end = (r + rspan).min(t.rows().len());
            let (Some(&x0), Some(&x1)) = (col_x.get(col_start), col_x.get(col_end)) else {
                continue;
            };
            let (Some(&y0), Some(&y1)) = (row_y.get(r), row_y.get(row_end.max(r + 1))) else {
                continue;
            };

            let x = emu_to_px(x0);
            let y = emu_to_px(y0);
            let w = emu_to_px(x1) - x;
            let h = emu_to_px(y1) - y;

            // Margins (EMU → px) — tcPr marL/R/T/B, else the PowerPoint defaults.
            let (mar_l, mar_r, mar_t, _mar_b) = cell_margins(tc);
            let text_x = x + emu_to_px(mar_l);
            let text_y = y + emu_to_px(mar_t);
            let text_w = (w - emu_to_px(mar_l) - emu_to_px(mar_r)).max(0.0);

            // Table styles: resolve the layered style for THIS cell position
            // (col_start = the gridSpan's first column drives band/first/last-col
            // parity), then apply the EXPLICIT `tcPr` over it (explicit fill / ln /
            // run colour wins — a cell with its own fill + any explicit run text).
            let resolved = style.map(|s| {
                resolve_cell_style(s, &flags, r, col_start, nrows, ncols, theme)
            });
            let rs = resolved.as_ref();

            // Fill: explicit tcPr solidFill wins; else the style fill.
            let fill_hex = cell_fill(tc, theme).or_else(|| rs.and_then(|s| s.fill.clone()));
            // Borders: an explicit tcPr edge wins per-edge; else the style edge.
            let (e_l, e_r, e_t, e_b) = cell_borders(tc, theme);
            let border_l = e_l.or_else(|| rs.and_then(|s| s.border_l.clone()));
            let border_r = e_r.or_else(|| rs.and_then(|s| s.border_r.clone()));
            let border_t = e_t.or_else(|| rs.and_then(|s| s.border_t.clone()));
            let border_b = e_b.or_else(|| rs.and_then(|s| s.border_b.clone()));
            let anchor = cell_anchor(tc);
            // Text: thread the style's resolved colour + bold as the run DEFAULTS
            // (an explicit `rPr` colour/bold still wins inside the builder).
            let text_defaults = rs.map(|s| s.text.clone());
            let paragraphs = tc
                .tx_body
                .as_deref()
                .map(|tb| {
                    build_paragraphs_from_styled(
                        &tb.p,
                        BODY_DEFAULT_PT,
                        text_defaults.as_ref(),
                        theme,
                        font_cache,
                    )
                })
                .unwrap_or_default();

            cells.push(TableCell {
                x,
                y,
                w,
                h,
                fill_hex,
                border_l,
                border_r,
                border_t,
                border_b,
                text_x,
                text_y,
                text_w,
                anchor,
                paragraphs,
            });
        }
    }

    SlideTable { cells }
}

/// Parse an `STCoordinate` (a String EMU value) to i64. A non-numeric / absent
/// value (the fork keeps these as raw strings) yields 0 so a malformed single
/// dimension degrades to a zero-width column/row rather than panicking. (A
/// table missing the strict-required `@w`/`@h` never reaches here — it fails the
/// fork's `parse_as::<CTTable>()` and is dropped whole, #63.)
fn parse_coord(s: &str) -> i64 {
    s.trim().parse::<i64>().unwrap_or(0)
}

/// A cell's solid fill (`tcPr/solidFill/srgbClr`) as `#rrggbb`, if present.
/// Reuses the shared DML fill→srgbClr extraction. Theme/gradient deferred.
fn cell_fill(tc: &ooxml_dml::types::CTTableCell, theme: &SlideTheme) -> Option<String> {
    let pr = tc.tc_pr.as_deref()?;
    solid_fill_hex(pr.fill_properties.as_deref(), theme)
}

/// The four edge borders of a cell (`tcPr/lnL,lnR,lnT,lnB`, §20.1.2.2.24) as
/// optional solid outlines (colour + width-px). A `noFill` / theme / gradient
/// line, or an absent edge, returns `None` for that edge. Width is `ln@w`
/// (EMU) → px, floored to the 1px hairline (the same floor as the shape
/// outline), absent → the hairline default.
fn cell_borders(
    tc: &ooxml_dml::types::CTTableCell,
    theme: &SlideTheme,
) -> (
    Option<ShapeOutline>,
    Option<ShapeOutline>,
    Option<ShapeOutline>,
    Option<ShapeOutline>,
) {
    let Some(pr) = tc.tc_pr.as_deref() else {
        return (None, None, None, None);
    };
    (
        line_to_outline(pr.ln_l.as_deref(), theme),
        line_to_outline(pr.ln_r.as_deref(), theme),
        line_to_outline(pr.ln_t.as_deref(), theme),
        line_to_outline(pr.ln_b.as_deref(), theme),
    )
}

/// Convert a `LineProperties` (a cell edge or any DML line) with a solid
/// `srgbClr` line fill into a [`ShapeOutline`] (colour + stroke width px). A
/// line without a solid srgbClr fill returns `None` (no border drawn). Mirrors
/// the shape-outline width logic: `ln@w` EMU → px, floored/defaulted to the 1px
/// hairline.
fn line_to_outline(
    ln: Option<&ooxml_dml::types::LineProperties>,
    theme: &SlideTheme,
) -> Option<ShapeOutline> {
    let ln = ln?;
    let color = solid_line_fill_hex(ln.line_fill_properties.as_deref(), theme)?;
    let width_px = ln
        .width
        .map(|w| emu_to_px(w as i64))
        .filter(|w| *w > 0.0)
        .unwrap_or(DEFAULT_OUTLINE_PX)
        .max(DEFAULT_OUTLINE_PX);
    let dash = super::shape::line_dash(ln.line_dash_properties.as_deref());
    Some(ShapeOutline { color, width_px, dash })
}

/// Cell margins (`tcPr/@marL,@marR,@marT,@marB`, EMU) with PowerPoint defaults
/// when absent. The margins are `STCoordinate32` (String EMU); non-numeric →
/// the default.
fn cell_margins(tc: &ooxml_dml::types::CTTableCell) -> (i64, i64, i64, i64) {
    let Some(pr) = tc.tc_pr.as_deref() else {
        return (
            DEFAULT_MAR_LR_EMU,
            DEFAULT_MAR_LR_EMU,
            DEFAULT_MAR_TB_EMU,
            DEFAULT_MAR_TB_EMU,
        );
    };
    let l = pr
        .mar_l
        .as_deref()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT_MAR_LR_EMU);
    let r = pr
        .mar_r
        .as_deref()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT_MAR_LR_EMU);
    let t = pr
        .mar_t
        .as_deref()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT_MAR_TB_EMU);
    let b = pr
        .mar_b
        .as_deref()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT_MAR_TB_EMU);
    (l, r, t, b)
}

/// A cell's vertical text anchoring (`tcPr@anchor`). Absent → top (the
/// PowerPoint default for table cells). `just`/`dist` collapse to top.
fn cell_anchor(tc: &ooxml_dml::types::CTTableCell) -> CellAnchor {
    use ooxml_dml::types::STTextAnchoringType;
    match tc.tc_pr.as_deref().and_then(|pr| pr.anchor) {
        Some(STTextAnchoringType::Ctr) => CellAnchor::Center,
        Some(STTextAnchoringType::B) => CellAnchor::Bottom,
        _ => CellAnchor::Top,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `gridCol@w` prefix-sum rail check via a built table (geometry pin).
    #[test]
    fn col_and_row_rails_are_prefix_sums() {
        // 2 columns (w 1000000, 2000000), 2 rows (h 500000, 700000), frame off
        // at (100000, 200000). Cell (0,0) box = off..off+col0, off..off+row0.
        let item = TableItem {
            off_x: 100_000,
            off_y: 200_000,
            ext_cx: 3_000_000,
            ext_cy: 1_200_000,
            table: build_test_table(
                &[1_000_000, 2_000_000],
                &[(500_000, 2), (700_000, 2)],
            ),
        };
        let st = build_slide_table(&item, &TableStyles::default(), &SlideTheme::default(), &FontCache::new());
        assert_eq!(st.cells.len(), 4, "2×2 un-merged grid → 4 cells");

        // Cell (0,0): x = off_x, y = off_y, w = col0, h = row0.
        let c00 = &st.cells[0];
        assert_eq!(c00.x, emu_to_px(100_000));
        assert_eq!(c00.y, emu_to_px(200_000));
        assert_eq!(c00.w, emu_to_px(1_000_000));
        assert_eq!(c00.h, emu_to_px(500_000));

        // Cell (1,1) is the 4th in row-major order: x = off_x+col0,
        // y = off_y+row0, w = col1, h = row1.
        let c11 = &st.cells[3];
        assert_eq!(c11.x, emu_to_px(100_000 + 1_000_000));
        assert_eq!(c11.y, emu_to_px(200_000 + 500_000));
        assert_eq!(c11.w, emu_to_px(2_000_000));
        assert_eq!(c11.h, emu_to_px(700_000));
    }

    /// The text inset box is the cell box inset by the default margins when tcPr
    /// omits them.
    #[test]
    fn default_margins_inset_the_text_box() {
        let item = TableItem {
            off_x: 0,
            off_y: 0,
            ext_cx: 1_000_000,
            ext_cy: 500_000,
            table: build_test_table(&[1_000_000], &[(500_000, 1)]),
        };
        let st = build_slide_table(&item, &TableStyles::default(), &SlideTheme::default(), &FontCache::new());
        let c = &st.cells[0];
        assert_eq!(c.text_x, c.x + emu_to_px(DEFAULT_MAR_LR_EMU));
        assert_eq!(c.text_y, c.y + emu_to_px(DEFAULT_MAR_TB_EMU));
        assert_eq!(
            c.text_w,
            c.w - emu_to_px(DEFAULT_MAR_LR_EMU) - emu_to_px(DEFAULT_MAR_LR_EMU)
        );
    }

    /// An hMerge continuation cell contributes no box (the span covers it).
    #[test]
    fn merge_continuation_cell_is_skipped() {
        // One row, two columns. The second cell is an hMerge continuation of the
        // first (which gridSpans 2). The grid renders ONE box (the spanning cell)
        // — the continuation is skipped.
        let mut table = build_test_table(&[1_000_000, 1_000_000], &[(500_000, 2)]);
        table.tr[0].tc[0].grid_span = Some(2);
        table.tr[0].tc[1].h_merge = Some(true);
        let item = TableItem {
            off_x: 0,
            off_y: 0,
            ext_cx: 2_000_000,
            ext_cy: 500_000,
            table,
        };
        let st = build_slide_table(&item, &TableStyles::default(), &SlideTheme::default(), &FontCache::new());
        assert_eq!(st.cells.len(), 1, "only the spanning cell draws a box");
        // The spanning cell covers both columns.
        assert_eq!(st.cells[0].w, emu_to_px(2_000_000));
    }

    /// A `rowSpan=N` cell's box grows down its covered rows (§21.1.3.16);
    /// the vMerge continuation below it is skipped, so the grown box fills the
    /// gap and the cell count shrinks accordingly.
    #[test]
    fn rowspan_grows_the_spanning_cell_box() {
        // One column, two rows (500k + 700k EMU). The top cell rowSpans 2; the
        // bottom cell is its vMerge continuation.
        let mut table = build_test_table(&[1_000_000], &[(500_000, 1), (700_000, 1)]);
        table.tr[0].tc[0].row_span = Some(2);
        table.tr[1].tc[0].v_merge = Some(true);
        let item = TableItem {
            off_x: 0,
            off_y: 0,
            ext_cx: 1_000_000,
            ext_cy: 1_200_000,
            table,
        };
        let st = build_slide_table(
            &item,
            &TableStyles::default(),
            &SlideTheme::default(),
            &FontCache::new(),
        );
        assert_eq!(st.cells.len(), 1, "only the spanning cell draws a box");
        assert_eq!(
            st.cells[0].h,
            emu_to_px(1_200_000),
            "rowSpan=2 grows the box over both rows"
        );
        // A rowSpan overshooting the grid clamps to the last row rail.
        let mut over = build_test_table(&[1_000_000], &[(500_000, 1)]);
        over.tr[0].tc[0].row_span = Some(9);
        let item = TableItem { off_x: 0, off_y: 0, ext_cx: 1_000_000, ext_cy: 500_000, table: over };
        let st = build_slide_table(
            &item,
            &TableStyles::default(),
            &SlideTheme::default(),
            &FontCache::new(),
        );
        assert_eq!(st.cells[0].h, emu_to_px(500_000), "overshoot clamps to the grid");
    }

    // ── test table builder ──────────────────────────────────────────────────

    /// Build a bare `CTTable` with the given column widths (EMU) and rows
    /// `(height_emu, cell_count)`. Cells carry no tcPr/txBody (the geometry
    /// tests pin rails/boxes, not fills/text).
    fn build_test_table(col_w: &[i64], rows: &[(i64, usize)]) -> CTTable {
        use ooxml_dml::types::{CTTableCell, CTTableCol, CTTableGrid, CTTableRow};
        // NOTE: the transitive ooxml-dml feature set (extra-attrs/extra-children/
        // dml-extensions are armed via the workspace + the pml feature) makes the
        // roundtrip/ext-list fields present on these generated structs. parasma-
        // wasm has no matching cfg flags of its own, so we initialize the fields
        // unconditionally here — the simplest correct construction for the test
        // builder. (These structs derive no `Default`, so there is no `..` base.)
        let grid_col = col_w
            .iter()
            .map(|w| CTTableCol {
                width: w.to_string(),
                ext_lst: None,
                extra_attrs: Default::default(),
                extra_children: Vec::new(),
            })
            .collect();
        let tr = rows
            .iter()
            .map(|(h, n)| CTTableRow {
                height: h.to_string(),
                tc: (0..*n).map(|_| CTTableCell::default()).collect(),
                ext_lst: None,
                extra_attrs: Default::default(),
                extra_children: Vec::new(),
            })
            .collect();
        CTTable {
            tbl_pr: None,
            tbl_grid: Box::new(CTTableGrid {
                grid_col,
                extra_children: Vec::new(),
            }),
            tr,
            extra_children: Vec::new(),
        }
    }
}
