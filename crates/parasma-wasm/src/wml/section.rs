//! Sections & page geometry (§17.6 `w:sectPr`). Resolves a section's physical
//! page box — page size, margins, header/footer offsets, columns (§17.6.4),
//! the document line grid (§17.6.5 `w:docGrid`), and page borders (§17.6.10,
//! resolved via `wml::border`). Also the section break kind (§17.6.1
//! `w:type/sectionMark`). Verbatim relocation from `lib.rs`.
//!
//! The section *loader* (`build_sections` / `load_section_hdr_ftr` /
//! `extract_hdr_ftr_images`) and the `Section`/`SectionHdrFtr` model types live
//! in `wml::loader` — they walk/mutate the parsed doc at load time, and
//! `SectionHdrFtr` carries header/footer block content (§17.10).
//!

use ooxml_wml::ext::{BodyExt, SectionPropertiesExt};

use crate::engine::units::{parse_twips, parse_twips_signed, twips_to_px};
use crate::wml::border::{page_border_edge, PageBorders};
use crate::{
    DEFAULT_FOOTER_MARGIN_TWIPS, DEFAULT_HEADER_MARGIN_TWIPS, DEFAULT_MARGIN_TWIPS,
    DEFAULT_PAGE_HEIGHT_TWIPS, DEFAULT_PAGE_WIDTH_TWIPS,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SectionBreakType {
    NextPage,
    Continuous,
    EvenPage,
    OddPage,
}

// ─── Page geometry (from sectPr or Letter default) ───────────────────────────

/// One column's geometry within a page's content area.
#[derive(Clone)]
pub(crate) struct ColumnDef {
    /// Absolute x position of the column's left edge on the page (px).
    pub(crate) x_offset_px: f64,
    /// Column content width (px), not including gutters.
    pub(crate) width_px: f64,
}

pub(crate) struct PageGeometry {
    pub(crate) width_px: f64,
    pub(crate) height_px: f64,
    pub(crate) margin_top_px: f64,
    pub(crate) margin_right_px: f64,
    pub(crate) margin_bottom_px: f64,
    pub(crate) margin_left_px: f64,
    pub(crate) header_margin_px: f64,
    pub(crate) footer_margin_px: f64,
    /// Resolved column layout. Always ≥ 1. Single-column docs have one
    /// element spanning the full content width — behavior-preserving fallback.
    pub(crate) columns: Vec<ColumnDef>,
    /// Draw a vertical separator line between columns (§17.6.4 `w:sep`).
    pub(crate) col_sep: bool,
    /// Document line-grid pitch in px (§17.6.5 `w:docGrid/@w:linePitch`) when the
    /// section defines one. `None` = no line grid. Whether it is actually applied
    /// also depends on the document not using Far-East layout (see `grid_enabled`).
    pub(crate) grid_pitch_px: Option<f64>,
    /// Resolved page borders (§17.6.10 `w:pgBorders`). `None` = section draws no
    /// page border. Drawn per-page in `render_page`, gated by `display`.
    pub(crate) page_borders: Option<PageBorders>,
}

/// The largest page dimension rendered, per axis: 22" = 31 680 twips — Word's
/// own page-size ceiling (a deliberate DoS bound).
///
/// `w:pgSz/@w:w`/`@w:h` (§17.6.13) are `ST_TwipsMeasure` (§22.9.2.14), a union
/// of `ST_UnsignedDecimalNumber` (`xsd:unsignedLong`) and
/// `ST_PositiveUniversalMeasure` — **ECMA-376 states no maximum for either
/// member**, so this bound is OURS, chosen for Word compatibility, and larger
/// values are a DECLARED DEVIATION: they render at the ceiling. It exists
/// because the page width is the number every downstream geometry bound rests
/// on: `equal_width_columns` refuses column counts "per pixel of content
/// width", so `w:w="10000000000"` (~6.7×10⁸ px of content) would re-open the
/// allocation bomb that guard closes — 9.34 GB RSS / 9.7 s from a 923-byte docx
/// via `w:cols w:num="600000000"`, measured ONCE on one machine (an earlier,
/// unreproduced measurement gave 3.91 GB / 37.8 s); treat both as
/// order-of-magnitude, not as data.
///
/// CLAMP, not the SML REJECT policy (`sml::model::MAX_ADDRESSABLE_COL`),
/// because the two bounds guard different things: an unaddressable CELL is one
/// coordinate among many and dropping it renders the document incomplete
/// rather than wrong, but `pgSz` is the whole document's canvas — refusing it
/// refuses the file. Clamping to the largest page Word itself can author keeps
/// the document openable, and no page authored by Word or another common
/// generator exceeds 22", so every legitimate document is byte-identical. The floor of 1 twip
/// keeps the (schema-valid) `w:w="0"` degenerate page, and any value above
/// `i64::MAX`, from wrapping negative through the `as i64` cast below.
const MAX_PAGE_DIMENSION_TWIPS: u64 = 31_680;

impl PageGeometry {
    pub(crate) fn us_letter() -> Self {
        let margin_px = twips_to_px(DEFAULT_MARGIN_TWIPS);
        let content_w = twips_to_px(DEFAULT_PAGE_WIDTH_TWIPS - 2 * DEFAULT_MARGIN_TWIPS);
        Self {
            width_px: twips_to_px(DEFAULT_PAGE_WIDTH_TWIPS),
            height_px: twips_to_px(DEFAULT_PAGE_HEIGHT_TWIPS),
            margin_top_px: margin_px,
            margin_right_px: margin_px,
            margin_bottom_px: margin_px,
            margin_left_px: margin_px,
            header_margin_px: twips_to_px(DEFAULT_HEADER_MARGIN_TWIPS),
            footer_margin_px: twips_to_px(DEFAULT_FOOTER_MARGIN_TWIPS),
            columns: vec![ColumnDef { x_offset_px: margin_px, width_px: content_w }],
            col_sep: false,
            grid_pitch_px: None,
            page_borders: None,
        }
    }

    pub(crate) fn from_sect_pr(sect_pr: &ooxml_wml::types::SectionProperties) -> Self {
        // Clamp BEFORE the i64 cast — see `MAX_PAGE_DIMENSION_TWIPS` for the
        // §22.9.2.14 no-spec-maximum grounding and the clamp-vs-reject call.
        //
        // The FLOOR of 1 is a SECOND deviation and is declared here rather than
        // riding along silently: a schema-valid `w:w="0"` (ST_TwipsMeasure has
        // no minimum facet either) now renders at 1 twip instead of 0. A
        // zero-width page has no content area at all, so every downstream
        // divisor and available-width subtraction would work on 0 or go
        // negative; 1 twip is degenerate in the same way but keeps the geometry
        // arithmetic total. `MAX_PAGE_DIMENSION_TWIPS` covers the ceiling; this sentence
        // covers the floor.
        let width_twips = sect_pr
            .page_width_twips()
            .map(|w| w.clamp(1, MAX_PAGE_DIMENSION_TWIPS) as i64)
            .unwrap_or(DEFAULT_PAGE_WIDTH_TWIPS);
        let height_twips = sect_pr
            .page_height_twips()
            .map(|h| h.clamp(1, MAX_PAGE_DIMENSION_TWIPS) as i64)
            .unwrap_or(DEFAULT_PAGE_HEIGHT_TWIPS);
        let margins = sect_pr.page_margins();
        let top = margins
            .and_then(|m| parse_twips_signed(&m.top))
            .unwrap_or(DEFAULT_MARGIN_TWIPS);
        let right = margins
            .and_then(|m| parse_twips(&m.right))
            .unwrap_or(DEFAULT_MARGIN_TWIPS);
        let bottom = margins
            .and_then(|m| parse_twips_signed(&m.bottom))
            .unwrap_or(DEFAULT_MARGIN_TWIPS);
        let left = margins
            .and_then(|m| parse_twips(&m.left))
            .unwrap_or(DEFAULT_MARGIN_TWIPS);
        let header = margins
            .and_then(|m| parse_twips(&m.header))
            .unwrap_or(DEFAULT_HEADER_MARGIN_TWIPS);
        let footer = margins
            .and_then(|m| parse_twips(&m.footer))
            .unwrap_or(DEFAULT_FOOTER_MARGIN_TWIPS);
        let margin_left_px = twips_to_px(left);
        let content_w = twips_to_px(width_twips) - margin_left_px - twips_to_px(right);
        let (columns, col_sep) = columns_from_sect_pr(sect_pr, margin_left_px, content_w);
        // §17.6.5 / §17.18.14: only the line-snapping grid types (`lines`,
        // `linesAndChars`) constrain the line pitch. `default` (type omitted —
        // common in documents in the wild) and `snapToChars` do NOT snap line
        // height; Word leaves such lines at the font's natural height. Snapping
        // a `default` grid floored every line up to linePitch (e.g. 360tw = 24px),
        // overshooting the natural ~20px and pushing content — and trailing
        // inline objects like charts — down the page.
        let grid_pitch_px = sect_pr.doc_grid.as_deref()
            .filter(|g| matches!(
                g.r#type,
                Some(ooxml_wml::types::STDocGrid::Lines) | Some(ooxml_wml::types::STDocGrid::LinesAndChars)
            ))
            .and_then(|g| g.line_pitch)
            .filter(|&lp| lp > 0)
            .map(twips_to_px);
        // §17.6.10 page borders. The three edge element types share val/color/
        // sz/space; `page_border_edge` resolves each. A `pgBorders` element with
        // no drawable edge resolves to `None`.
        let page_borders = sect_pr.pg_borders.as_deref().and_then(|pgb| {
            let top = pgb
                .top
                .as_deref()
                .and_then(|b| page_border_edge(&b.value, b.color.as_deref(), b.size, b.space));
            let bottom = pgb
                .bottom
                .as_deref()
                .and_then(|b| page_border_edge(&b.value, b.color.as_deref(), b.size, b.space));
            let left = pgb
                .left
                .as_deref()
                .and_then(|b| page_border_edge(&b.value, b.color.as_deref(), b.size, b.space));
            let right = pgb
                .right
                .as_deref()
                .and_then(|b| page_border_edge(&b.value, b.color.as_deref(), b.size, b.space));
            if top.is_none() && bottom.is_none() && left.is_none() && right.is_none() {
                return None;
            }
            Some(PageBorders {
                top,
                bottom,
                left,
                right,
                offset_from_page: matches!(
                    pgb.offset_from,
                    Some(ooxml_wml::types::STPageBorderOffset::Page)
                ),
                display: pgb
                    .display
                    .unwrap_or(ooxml_wml::types::STPageBorderDisplay::AllPages),
            })
        });
        Self {
            width_px: twips_to_px(width_twips),
            height_px: twips_to_px(height_twips),
            margin_top_px: twips_to_px(top),
            margin_right_px: twips_to_px(right),
            margin_bottom_px: twips_to_px(bottom),
            margin_left_px,
            header_margin_px: twips_to_px(header),
            footer_margin_px: twips_to_px(footer),
            columns,
            col_sep,
            grid_pitch_px,
            page_borders,
        }
    }

    pub(crate) fn from_doc<R: std::io::Read + std::io::Seek>(doc: &ooxml_wml::Document<R>) -> Self {
        match doc.body().section_properties() {
            Some(sect_pr) => Self::from_sect_pr(sect_pr),
            None => Self::us_letter(),
        }
    }

    pub(crate) fn content_width_px(&self) -> f64 {
        self.width_px - self.margin_left_px - self.margin_right_px
    }
    pub(crate) fn content_height_px(&self) -> f64 {
        self.height_px - self.margin_top_px - self.margin_bottom_px
    }
}

/// Geometry of an equal-width column set (§17.6.4): `(column count, column
/// width px)`, or `None` when a content area this wide cannot host `num`
/// columns at all.
///
/// **The spec does not bound `@w:num`.** Its type is `ST_DecimalNumber`
/// (§17.18.10) — a signed, arbitrary-precision `xsd:integer` — and ECMA-376
/// states no maximum for the attribute anywhere; the sibling `<w:col>` list is
/// capped (`maxOccurs="45"` on `CT_Columns/col`) but that cap is on the
/// EXPLICIT-widths element, which §17.6.4 says is only consulted "if all
/// columns are not of equal width". So no numeric clamp on `@w:num` is
/// grounded, and none is applied: `w:num="60" w:space="0"` on Letter is sixty
/// positive ~10.4px columns and renders as sixty columns.
///
/// What IS bounded is the geometry the value has to fit into, and §17.6.4's own
/// equal-width algorithm is where it bites: *"Take width of page (from margin to
/// margin) / Divide by number of columns specified in num attribute / For each
/// column, leave space after as defined in the space attribute / Remaining width
/// of each column is the text column width."* Run that division and the request
/// is unrenderable when either
///
///   (a) `num` exceeds the content area's width in pixels — two columns cannot
///       share a pixel, so a page W px wide cannot show more than W of them; or
///   (b) the remaining width per column is not positive — the gutters have eaten
///       the page, and every column is a zero-or-negative-width box no line can
///       be laid into.
///
/// Both are refusals, not rewrites: the caller falls back to the schema default
/// of one column (Annex A.1, `CT_Columns/@num default="1"`). Rewriting `@w:num`
/// to some other count would silently corrupt a document whose columns the spec
/// permits; refusing to render a column set that has no pixels to live in is
/// honest and is the only behaviour that also bounds the work — (a) caps the
/// materialised `ColumnDef` vector at one entry per pixel of page width, i.e.
/// well under a single scanline of the page bitmap it belongs to.
///
/// This is what `w:num="2000000000"` (~32 GB of `ColumnDef`) needed: it hung
/// natively past 2 GB RSS and traps on wasm32, where the 32-bit `usize` multiply
/// overflows `RawVec`'s checked path.
fn equal_width_columns(num: i64, gutter_px: f64, content_width_px: f64) -> Option<(usize, f64)> {
    // A multi-column set starts at two; `num ≤ 1` is the caller's schema-default
    // path (one full-width column), never this one.
    if num < 2 {
        return None;
    }
    // Degenerate page geometry (NaN/±inf content width) has no answer at all.
    if !content_width_px.is_finite() || !gutter_px.is_finite() {
        return None;
    }
    // (a) at most one column per pixel of content width.
    if (num as f64) > content_width_px {
        return None;
    }
    // Cannot fail after (a) on any real page, but `try_from` (not `as`) keeps it
    // true on wasm32, where `usize` is 32 bits and `as` would truncate.
    let count = usize::try_from(num).ok()?;
    let total_gutter = gutter_px * (count - 1) as f64;
    let width = (content_width_px - total_gutter) / count as f64;
    // (b) §17.6.4's "remaining width of each column" must be positive.
    if width <= 0.0 {
        return None;
    }
    Some((count, width))
}

/// Resolve `<w:cols>` into `(Vec<ColumnDef>, col_sep)` in absolute page px.
/// Returns a single full-width column when `cols` is absent or `num ≤ 1`.
/// `OnOff = String`; truthy values are "1", "true", "on".
pub(crate) fn columns_from_sect_pr(
    sect_pr: &ooxml_wml::types::SectionProperties,
    margin_left_px: f64,
    content_width_px: f64,
) -> (Vec<ColumnDef>, bool) {
    let single = || vec![ColumnDef { x_offset_px: margin_left_px, width_px: content_width_px }];
    let on = |s: &str| matches!(s, "1" | "true" | "on");

    let cols = match sect_pr.cols.as_deref() {
        Some(c) => c,
        None => return (single(), false),
    };

    let sep = cols.sep.as_deref().is_some_and(on);

    // Default gutter: 720 twips = 0.5"
    let default_gutter_px = twips_to_px(
        cols.space.as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(720),
    );

    // equal_width defaults to true; explicit cols[] only when equal_width=false.
    let use_explicit = cols.equal_width.as_deref()
        .is_some_and(|v| !on(v))
        && !cols.col.is_empty();

    if use_explicit {
        let mut result = Vec::with_capacity(cols.col.len());
        let mut x = margin_left_px;
        for (i, col) in cols.col.iter().enumerate() {
            let w = col.width.as_deref()
                .and_then(|s| s.parse::<i64>().ok())
                .map(twips_to_px)
                .unwrap_or(content_width_px / cols.col.len() as f64);
            result.push(ColumnDef { x_offset_px: x, width_px: w });
            if i + 1 < cols.col.len() {
                let gap = col.space.as_deref()
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(twips_to_px)
                    .unwrap_or(default_gutter_px);
                x += w + gap;
            }
        }
        return (result, sep);
    }

    // Equal-width: `num` columns separated by the gutter.
    // STDecimalNumber = i64, direct value.
    //
    // §17.6.4 gives `@w:num` NO ceiling, and Annex A.1 makes the absent case its
    // own answer: `<xsd:attribute name="num" type="ST_DecimalNumber"
    // use="optional" default="1"/>`. So `num ≤ 1` — a value the spec assigns no
    // meaning — resolves to the single full-width column an absent attribute
    // yields, and anything the page cannot host degrades to that SAME default
    // rather than being rewritten to some other count (see
    // `equal_width_columns`).
    let requested = cols.num.unwrap_or(1);
    if requested <= 1 {
        return (single(), sep);
    }
    let Some((num, col_w)) = equal_width_columns(requested, default_gutter_px, content_width_px)
    else {
        return (single(), sep);
    };

    let result = (0..num)
        .map(|i| ColumnDef {
            x_offset_px: margin_left_px + i as f64 * (col_w + default_gutter_px),
            width_px: col_w,
        })
        .collect();
    (result, sep)
}

#[cfg(test)]
mod tests {
    use super::{columns_from_sect_pr, equal_width_columns, PageGeometry, MAX_PAGE_DIMENSION_TWIPS};
    use crate::engine::units::twips_to_px;
    use ooxml_wml::types::{Columns, PageSize, SectionProperties};

    /// US Letter with 1" margins: 12240 − 2×1440 twips = 624 px of content.
    const LETTER_CONTENT_PX: f64 = 624.0;

    fn sect_pr(num: i64, space: Option<&str>) -> SectionProperties {
        SectionProperties {
            cols: Some(Box::new(Columns {
                num: Some(num),
                space: space.map(|s| s.to_string()),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn column_count(num: i64, space: Option<&str>) -> usize {
        columns_from_sect_pr(&sect_pr(num, space), 96.0, LETTER_CONTENT_PX).0.len()
    }

    /// `<w:cols w:num="2000000000"/>` (§17.6.4), a verified crash vector.
    /// The equal-width branch materialises one `ColumnDef`
    /// per column: 32 GB of intent from a 1.2 KB file. Natively it hung past
    /// 2 GB RSS; on wasm32 the 32-bit `usize` multiply overflows `RawVec`'s
    /// checked path and traps the module. The spec puts no ceiling on `@w:num`,
    /// so the value is NOT rewritten — the request is refused (no page has
    /// 2×10⁹ pixels of content width to host it) and the section falls back to
    /// the schema default of one column.
    #[test]
    fn column_count_the_page_cannot_host_degrades_to_the_schema_default() {
        for num in [2_000_000_000i64, 4_294_967_295, 4_294_967_296, i64::MAX] {
            assert_eq!(column_count(num, Some("720")), 1, "w:num={num} (default gutter)");
            assert_eq!(column_count(num, Some("0")), 1, "w:num={num} (zero gutter)");
        }
    }

    /// THE REGRESSION THIS GUARD MUST NOT INTRODUCE. A 45-column clamp was
    /// rejected because `<w:cols w:num="60" w:space="0"/>` is a perfectly
    /// renderable Letter layout — sixty positive ~10.4 px columns — and clamping
    /// would have silently rendered 45 of them. Nothing in §17.6.4 caps `@w:num`,
    /// so every count whose columns have room stays exactly as authored.
    #[test]
    fn sixty_zero_gutter_columns_render_as_sixty() {
        let (cols, _) = columns_from_sect_pr(&sect_pr(60, Some("0")), 96.0, LETTER_CONTENT_PX);
        assert_eq!(cols.len(), 60, "a 60-column zero-gutter Letter section keeps 60 columns");
        assert!((cols[0].width_px - LETTER_CONTENT_PX / 60.0).abs() < 1e-9);
        assert!((cols[59].x_offset_px - (96.0 + 59.0 * LETTER_CONTENT_PX / 60.0)).abs() < 1e-9);
        // …and the whole family above Word's own 45-column ceiling survives.
        for num in [46i64, 50, 100, 300, 624] {
            assert_eq!(column_count(num, Some("0")), num as usize, "w:num={num}");
        }
    }

    /// Ordinary sections are untouched, and the two shapes §17.6.4 cannot render
    /// both land on the schema default (`CT_Columns/@num default="1"`):
    /// non-positive counts, and counts whose per-column width the gutters have
    /// eaten ("remaining width of each column").
    #[test]
    fn ordinary_counts_pass_through_and_unrenderable_ones_degrade() {
        for num in [2i64, 3, 4, 13] {
            assert_eq!(column_count(num, Some("720")), num as usize, "w:num={num}");
        }
        for num in [0i64, -1, i64::MIN] {
            assert_eq!(column_count(num, Some("720")), 1, "w:num={num} ⇒ absent ⇒ one column");
        }
        // 14 columns × 720tw gutters = 624px of gutter on a 624px page: every
        // column would be zero-width, so the set is refused, not resized.
        assert_eq!(column_count(14, Some("720")), 1);
        assert_eq!(column_count(20, Some("720")), 1);
    }

    fn pg_sz_sect_pr(w: &str, h: &str, cols: Option<(i64, &str)>) -> SectionProperties {
        SectionProperties {
            pg_sz: Some(Box::new(PageSize {
                width: Some(w.to_string()),
                height: Some(h.to_string()),
                ..Default::default()
            })),
            cols: cols.map(|(num, space)| {
                Box::new(Columns {
                    num: Some(num),
                    space: Some(space.to_string()),
                    ..Default::default()
                })
            }),
            ..Default::default()
        }
    }

    /// `w:pgSz/@w:w`/`@w:h` (§17.6.13) are `ST_TwipsMeasure`
    /// (§22.9.2.14) with NO spec maximum; both axes clamp to the 22" Word
    /// ceiling (a deliberate bound), and every real page size passes through
    /// untouched. See `MAX_PAGE_DIMENSION_TWIPS`.
    #[test]
    fn oversized_pg_sz_clamps_to_the_word_ceiling() {
        let max_px = twips_to_px(MAX_PAGE_DIMENSION_TWIPS as i64);
        // The measured bomb's page size, and u64 extremes: `u64::MAX` would
        // wrap negative through the `as i64` cast without the pre-cast clamp.
        for w in ["10000000000", "18446744073709551615"] {
            let g = PageGeometry::from_sect_pr(&pg_sz_sect_pr(w, w, None));
            assert_eq!(g.width_px, max_px, "w={w}");
            assert_eq!(g.height_px, max_px, "h={w}");
        }
        // The floor: a schema-valid zero page stays positive (1 twip).
        let g = PageGeometry::from_sect_pr(&pg_sz_sect_pr("0", "0", None));
        assert_eq!(g.width_px, twips_to_px(1));
        assert_eq!(g.height_px, twips_to_px(1));
        // Byte-identity for legitimate pages: Letter, A4 portrait, and the 22"
        // ceiling itself all pass through exactly.
        for (w, h) in [(12240i64, 15840i64), (11907, 16839), (31680, 31680)] {
            let g = PageGeometry::from_sect_pr(&pg_sz_sect_pr(&w.to_string(), &h.to_string(), None));
            assert_eq!(g.width_px, twips_to_px(w), "w={w}");
            assert_eq!(g.height_px, twips_to_px(h), "h={h}");
        }
    }

    /// The bomb itself: `pgSz w=1e10` + `cols num=6e8 space=0` measured
    /// 9.34 GB RSS / 9.7 s (release) — the unclamped width put ~6.7×10⁸ px of
    /// content under the one-column-per-pixel guard, so 6×10⁸ `ColumnDef`s
    /// materialised. Clamped, content is ~2 000 px, the count is refused, and
    /// the section falls back to the schema-default single column.
    #[test]
    fn pg_sz_column_bomb_is_refused_not_materialised() {
        let sp = pg_sz_sect_pr("10000000000", "10000000000", Some((600_000_000, "0")));
        let g = PageGeometry::from_sect_pr(&sp);
        assert_eq!(g.columns.len(), 1, "6e8 columns on a clamped page degrade to one");
        assert_eq!(g.width_px, twips_to_px(MAX_PAGE_DIMENSION_TWIPS as i64));
    }

    /// The two refusal criteria, at the helper. Both are geometric; neither
    /// rewrites the requested count.
    #[test]
    fn equal_width_columns_refuses_only_on_geometry() {
        assert_eq!(equal_width_columns(60, 0.0, 624.0), Some((60, 624.0 / 60.0)));
        assert_eq!(equal_width_columns(2, 48.0, 624.0), Some((2, 288.0)));
        // (a) more columns than the content area has pixels.
        assert_eq!(equal_width_columns(625, 0.0, 624.0), None);
        // (b) gutters have eaten the page.
        assert_eq!(equal_width_columns(14, 48.0, 624.0), None);
        // Degenerate page geometry refuses rather than producing NaN columns.
        assert_eq!(equal_width_columns(2, 48.0, f64::NAN), None);
        assert_eq!(equal_width_columns(2, 48.0, -10.0), None);
    }
}
