//! SML embedded drawings (pictures and charts).
//!
//! A worksheet references its DrawingML part via `<drawing r:id>` (§18.3.1.36);
//! the drawing part is an `xdr:wsDr` SpreadsheetDrawing (§20.5) whose
//! `twoCellAnchor` / `oneCellAnchor` children position pictures on the cell
//! grid in (column, EMU-offset) anchor coordinates (§20.5.2.33 / §20.5.2.24).
//!
//! The fork has NO xdr parser, so this module RAW-SCANS the drawing XML (the
//! `engine/vml.rs` raw-scan precedent): reading through the load's shared
//! `engine::opc::OpcPackage` (one archive open per load), it walks
//! `xl/worksheets/_rels/sheetN.xml.rels` → `xl/drawings/drawingN.xml`, scans
//! the anchors for `a:blip r:embed`, and resolves each embed through the
//! drawing part's own rels to the `xl/media/*` bytes. Anchor → pixel
//! conversion happens later in [`super::grid::paginate`] on the EXISTING grid
//! col/row rails (EMU/9525 = px, `engine::units::EMU_PER_PX`).
//!
//! Pure (no web-sys) so the whole walk is natively unit-testable.

use std::collections::HashMap;

use super::model::{MAX_ADDRESSABLE_COL, MAX_ADDRESSABLE_ROW};
use crate::engine::images::ImageBytes;
use crate::engine::opc;

/// One xdr anchor point (§20.5.2.15/.16/.32/.33): a 0-based column/row index
/// plus an EMU offset INTO that column/row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AnchorPoint {
    pub(crate) col: u32,
    pub(crate) col_off_emu: i64,
    pub(crate) row: u32,
    pub(crate) row_off_emu: i64,
}

/// One anchored picture parsed from a sheet's drawing part. `to` is present
/// for `twoCellAnchor`; `ext_emu` (cx, cy) for `oneCellAnchor`'s `xdr:ext`
/// (also kept as a fallback size when the `to` rails degenerate).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SheetDrawing {
    pub(crate) from: AnchorPoint,
    pub(crate) to: Option<AnchorPoint>,
    pub(crate) ext_emu: Option<(i64, i64)>,
    /// Sheet-namespaced image key (`sml::{sheetIdx}::{relId}`) — the shared
    /// `images` / `image_bitmaps` maps key on this (the PML `image_key`
    /// precedent, `pml::shape::image_key`).
    pub(crate) image_key: String,
}

/// Relationship type of a worksheet part (workbook rels → sheet part).
const REL_WORKSHEET: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet";
/// Relationship type of a drawing part (sheet rels → `xl/drawings/drawingN.xml`).
const REL_DRAWING: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing";
/// Relationship type of a chart part (drawing rels → `xl/charts/chartN.xml`).
const REL_CHART: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart";

/// Everything the second-OPC drawing walk yields: anchored pictures + their
/// media bytes and anchored CHARTS + their raw chart-part XML (embedded
/// xlsx charts). Chart anchors reuse [`SheetDrawing`] with the
/// sheet-namespaced CHART key in `image_key`; the part bytes feed the shared
/// `crate::chart` painter through the Viewer's `chart_xml` map (the exact
/// `PmlRenderer::take_charts` contract).
pub(crate) struct WorkbookDrawings {
    /// Anchored pictures per SHEET NAME.
    pub(crate) drawings: HashMap<String, Vec<SheetDrawing>>,
    /// Picture media bytes keyed by `sml::{sheetIdx}::{relId}`.
    pub(crate) images: Vec<(String, ImageBytes)>,
    /// Anchored charts per SHEET NAME (`image_key` = the chart key).
    pub(crate) charts: HashMap<String, Vec<SheetDrawing>>,
    /// Raw chart-part XML keyed by `chart::sml::{sheetIdx}::{relId}`.
    pub(crate) chart_parts: Vec<(String, Vec<u8>)>,
}

/// Walk the workbook (through the load's SHARED [`opc::OpcPackage`] — one open per load)
/// for every sheet's anchored pictures AND charts.
///
/// Drawings/charts are keyed by SHEET NAME (not index) because the `<col>` shim may
/// DROP an unrecoverable sheet — the renderer's `sheets` vec indices then
/// diverge from workbook order, but names stay stable (`ResolvedSheet::name`).
/// Any structural miss (unopened package, no rels, no drawing, unreadable
/// part) skips that sheet/picture/chart — graceful by construction, mirroring
/// `pml::resolve_picture_images`.
pub(crate) fn parse_workbook_drawings(pkg: Option<&mut opc::OpcPackage<'_>>) -> WorkbookDrawings {
    let mut out = WorkbookDrawings {
        drawings: HashMap::new(),
        images: Vec::new(),
        charts: HashMap::new(),
        chart_parts: Vec::new(),
    };

    let Some(pkg) = pkg else {
        return out;
    };

    // Root rels → workbook part path (fallback: the conventional location) —
    // the same walk as `super::parse_workbook_theme`.
    let wb_path = pkg
        .office_document_target()
        .map(|t| t.trim_start_matches('/').to_string())
        .unwrap_or_else(|| "xl/workbook.xml".to_string());

    // Workbook XML → ordered (sheet name, r:id) pairs (§18.2.19 <sheet>).
    let Some(wb_xml) = pkg.read_part_string(&wb_path) else {
        return out;
    };
    let sheet_entries = scan_workbook_sheets(&wb_xml);

    // Workbook rels: r:id → sheet part path.
    let wb_rels = pkg.rels(&wb_path);

    for (sheet_idx, (sheet_name, rid)) in sheet_entries.iter().enumerate() {
        let Some(rel) = wb_rels
            .iter()
            .find(|r| &r.id == rid && r.relationship_type == REL_WORKSHEET)
        else {
            continue;
        };
        let sheet_path = opc::resolve_target(&wb_path, &rel.target);

        // Sheet rels → drawing part(s) (§18.3.1.36; one per sheet in practice).
        let drawing_paths: Vec<String> = pkg
            .rels(&sheet_path)
            .iter()
            .filter(|r| r.relationship_type == REL_DRAWING)
            .map(|r| opc::resolve_target(&sheet_path, &r.target))
            .collect();

        for drawing_path in drawing_paths {
            let Some(xml) = pkg.read_part_string(&drawing_path) else {
                continue;
            };
            let drawing_rels = pkg.rels(&drawing_path);

            for (anchor, embed_rel) in scan_anchors(&xml) {
                let key = image_key(sheet_idx, &embed_rel);
                // Resolve the media bytes ONCE per key (a drawing reusing one
                // rel id across anchors still decodes a single bitmap).
                if !out.images.iter().any(|(k, _)| k == &key) {
                    let resolved = drawing_rels.get(&embed_rel).and_then(|r| {
                        let part_path = opc::resolve_target(&drawing_path, &r.target);
                        let data = pkg.read_part(&part_path)?;
                        Some(ImageBytes { mime: crate::engine::images::mime_from_path(&part_path), data })
                    });
                    if let Some(img) = resolved {
                        out.images.push((key.clone(), img));
                    }
                }
                out.drawings
                    .entry(sheet_name.clone())
                    .or_default()
                    .push(SheetDrawing { image_key: key, ..anchor });
            }

            // Chart anchors (graphicFrame → c:chart r:id, §20.5.2.13/§21.2):
            // same anchor geometry, chart part read via the DRAWING rels'
            // `/chart` relationship. Keyed `chart::sml::{sheetIdx}::{relId}` —
            // the PML `chart::pml::{slideIdx}::{relId}` precedent, so the
            // Viewer's shared `chart_xml` map + the copy seams work unchanged.
            for (anchor, chart_rel) in scan_chart_anchors(&xml) {
                let key = chart_key(sheet_idx, &chart_rel);
                if !out.chart_parts.iter().any(|(k, _)| k == &key) {
                    let resolved = drawing_rels
                        .get(&chart_rel)
                        .filter(|r| r.relationship_type == REL_CHART)
                        .and_then(|r| {
                            let part_path = opc::resolve_target(&drawing_path, &r.target);
                            pkg.read_part(&part_path)
                        });
                    match resolved {
                        // Validate at capture: only place a chart the
                        // shared painter can actually draw (parses + non-empty
                        // series). Without this, the paint's white chart-area
                        // ground draws BEFORE render_chart returns false → a bare
                        // white rectangle for an unparseable/empty part. One parse
                        // at load; zero per-frame cost.
                        Some(xml_bytes) if crate::chart::copy_data(&xml_bytes).is_some() => {
                            out.chart_parts.push((key.clone(), xml_bytes))
                        }
                        // Unresolvable/undrawable chart part → skip the anchor
                        // entirely (never place a chart the painter can't draw).
                        _ => continue,
                    }
                }
                out.charts
                    .entry(sheet_name.clone())
                    .or_default()
                    .push(SheetDrawing { image_key: key, ..anchor });
            }
        }
    }

    out
}

/// Sheet-namespaced image key (the `pml::shape::image_key` precedent —
/// `sml::{sheetIdx}::{relId}`; rel ids are only unique per drawing part).
pub(crate) fn image_key(sheet_idx: usize, rel_id: &str) -> String {
    format!("sml::{sheet_idx}::{rel_id}")
}

/// Sheet-namespaced CHART key (the `pml::shape::chart_key` precedent —
/// `chart::sml::{sheetIdx}::{relId}`). The `chart::` prefix keeps chart keys
/// out of every image-bytes path (`is_image` = images minus charts; charts
/// copy through the object path) while addressing the Viewer's shared `chart_xml` map.
pub(crate) fn chart_key(sheet_idx: usize, rel_id: &str) -> String {
    format!("chart::sml::{sheet_idx}::{rel_id}")
}

/// Raw-scan the workbook XML's `<sheet name=… r:id=…>` entries in document
/// order (§18.2.19) — the order `Workbook::sheet_names` / the shim load in.
fn scan_workbook_sheets(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(pos) = rest.find("<sheet ") {
        let tag_start = &rest[pos..];
        let Some(end) = tag_start.find('>') else { break };
        let tag = &tag_start[..end];
        if let (Some(name), Some(rid)) = (attr_value(tag, "name"), attr_value(tag, "r:id")) {
            out.push((
                crate::engine::vml::decode_xml_entities(name),
                rid.to_string(),
            ));
        }
        rest = &tag_start[end..];
    }
    out
}

/// Raw-scan an `xdr:wsDr` drawing for picture anchors: every `twoCellAnchor` /
/// `oneCellAnchor` block (any prefix) containing an `a:blip r:embed`. Returns
/// the anchor geometry (image_key left empty — the caller namespaces it) and
/// the embed rel id. Anchors without a blip (shapes, charts) are skipped.
fn scan_anchors(xml: &str) -> Vec<(SheetDrawing, String)> {
    let mut out = Vec::new();
    for local in ["twoCellAnchor", "oneCellAnchor"] {
        for block in element_blocks(xml, local) {
            let Some(embed) = blip_embed(block) else { continue };
            let Some(from_block) = element_blocks(block, "from").into_iter().next() else {
                continue;
            };
            let Some(from) = anchor_point(from_block) else { continue };
            let to = element_blocks(block, "to")
                .into_iter()
                .next()
                .and_then(anchor_point);
            // `xdr:ext` is the oneCellAnchor size; ALSO read the picture's
            // `a:ext` (xfrm extent) as a size fallback for degenerate `to`
            // rails. Both share the local name "ext" with cx/cy attrs — take
            // the first that yields a positive extent.
            let ext_emu = ext_cx_cy(block);
            out.push((
                SheetDrawing { from, to, ext_emu, image_key: String::new() },
                embed,
            ));
        }
    }
    out
}

/// Raw-scan an `xdr:wsDr` drawing for CHART anchors: every `twoCellAnchor` /
/// `oneCellAnchor` block (any prefix) whose `graphicFrame` carries a
/// `<c:chart r:id>` (§21.2 — the graphicData uri `…/chart` payload). Returns
/// the anchor geometry (image_key left empty — the caller stamps the
/// namespaced CHART key) and the chart rel id. Picture anchors (blip-bearing)
/// never match: a graphicFrame has no `a:blip`, and `chart_rid` only matches
/// the `chart` local name.
fn scan_chart_anchors(xml: &str) -> Vec<(SheetDrawing, String)> {
    let mut out = Vec::new();
    for local in ["twoCellAnchor", "oneCellAnchor"] {
        for block in element_blocks(xml, local) {
            let Some(rid) = chart_rid(block) else { continue };
            let Some(from_block) = element_blocks(block, "from").into_iter().next() else {
                continue;
            };
            let Some(from) = anchor_point(from_block) else { continue };
            let to = element_blocks(block, "to")
                .into_iter()
                .next()
                .and_then(anchor_point);
            // oneCellAnchor size / degenerate-`to` fallback — the graphicFrame
            // `a:xfrm` `a:ext` doubles as the frame extent (same rule as pics).
            let ext_emu = ext_cx_cy(block);
            out.push((
                SheetDrawing { from, to, ext_emu, image_key: String::new() },
                rid,
            ));
        }
    }
    out
}

/// The `r:id` of the first `<c:chart>` reference in the block (§21.2 — the
/// graphicData chart payload; self-closing, so an open-tag scan finds it).
fn chart_rid(block: &str) -> Option<String> {
    for tag in element_open_tags(block, "chart") {
        if let Some(v) = attr_value(tag, "r:id").or_else(|| attr_value(tag, "id")) {
            return Some(v.to_string());
        }
    }
    None
}

/// Parse one `<xdr:from>`/`<xdr:to>` block's col/colOff/row/rowOff children
/// (§20.5.2.15: `<xdr:col>1</xdr:col><xdr:colOff>139700</xdr:colOff>…`).
///
/// A coordinate outside the addressable grid is REJECTED here, at the parse
/// boundary — `None`, never a nearer coordinate — so every consumer inherits
/// the bound. `<xdr:col>` is `ST_ColID` (§20.5.3.1), `<xdr:row>` is
/// `ST_RowID` (§20.5.3.3): both restrict `xsd:int` with ONLY
/// `minInclusive = 0` — **ECMA-376 sets no maximum** (the "never exceed the
/// number of total columns" sentence is prose guidance, not a facet) — so the
/// bound is OURS: the same `MAX_ADDRESSABLE_{COL,ROW}` implementation limit
/// that cell references carry, with the same REJECT-never-CLAMP policy
/// (`sml::model::MAX_ADDRESSABLE_COL` documents why clamping — moving a
/// drawing onto a different, possibly occupied cell — is worse than dropping
/// it). This is the `parse_a1` bound restated for anchors, NOT the
/// `saturating_add` guard: `2000000000` never overflows, but both `col_left`
/// (`grid::paginate`) and `col_left_any` (`index::geom_for`) walk per-column
/// rails up to the anchor — ~2×10⁹ linear `col_px` scans, ~3 s release / ~78 s
/// debug for ONE anchor (measured once; an order of magnitude, not a
/// benchmark) — and `u32::MAX` additionally overflows their 0-based→1-based
/// `+ 1` (the debug panic is observed at `grid.rs`'s `to_box` closure, not
/// in `index.rs`). One
/// parse-time reject bounds the walk (≤ 16 385 columns) and the arithmetic at
/// once.
///
/// `>` (not `>=`) because xdr coordinates are 0-BASED: `col == 16_383` is the
/// last cell column (XFD) and `col == 16_384` is the one-past-XFD boundary a
/// `to` rail legitimately names for a picture flush against the grid edge —
/// both are kept, and `+ 1` on an admitted value stays ≤ 16 385, in bounds.
/// A rejected `from` drops the whole anchor (the callers `continue`); a
/// rejected `to` alone degrades that drawing to its `ext_emu` size fallback.
fn anchor_point(block: &str) -> Option<AnchorPoint> {
    let col: u32 = element_text(block, "col")?.trim().parse().ok()?;
    let row: u32 = element_text(block, "row")?.trim().parse().ok()?;
    if col > MAX_ADDRESSABLE_COL || row > MAX_ADDRESSABLE_ROW {
        return None;
    }
    Some(AnchorPoint {
        col,
        col_off_emu: element_text(block, "colOff")
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0),
        row,
        row_off_emu: element_text(block, "rowOff")
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0),
    })
}

/// Text content of the FIRST `<pfx:local>…</pfx:local>` element in `xml`
/// (exact local-name match, any prefix). `None` when absent.
fn element_text<'a>(xml: &'a str, local: &str) -> Option<&'a str> {
    element_blocks(xml, local).into_iter().next()
}

/// First `<…:ext cx=… cy=…>` in the block with a positive extent (the
/// oneCellAnchor `xdr:ext` comes before the pic's `a:xfrm` `a:ext`, and both
/// carry the picture size; extLst `a:ext` has a `uri` attr and no cx/cy).
fn ext_cx_cy(block: &str) -> Option<(i64, i64)> {
    for tag in element_open_tags(block, "ext") {
        let cx = attr_value(tag, "cx").and_then(|v| v.trim().parse::<i64>().ok());
        let cy = attr_value(tag, "cy").and_then(|v| v.trim().parse::<i64>().ok());
        if let (Some(cx), Some(cy)) = (cx, cy) {
            if cx > 0 && cy > 0 {
                return Some((cx, cy));
            }
        }
    }
    None
}

/// The `r:embed` rel id of the first `<a:blip>` in the block (§20.1.8.13).
fn blip_embed(block: &str) -> Option<String> {
    for tag in element_open_tags(block, "blip") {
        if let Some(v) = attr_value(tag, "r:embed").or_else(|| attr_value(tag, "embed")) {
            return Some(v.to_string());
        }
    }
    None
}

/// Attribute value on a raw open tag, matching `name="…"` exactly (the tag
/// slice never spans elements, so a plain substring probe is safe). Tries the
/// given (possibly prefixed) name verbatim.
fn attr_value<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    // Match ` name="` (preceded by whitespace) so e.g. `cx` never matches the
    // tail of another attribute name.
    let needle = format!(" {name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Every open tag `<pfx:local …>`/`<local …>` (exact local-name match, any
/// prefix) in `xml`, returned as the tag slice up to (excluding) `>`.
fn element_open_tags<'a>(xml: &'a str, local: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    for (pos, _) in xml.match_indices('<') {
        let rest = &xml[pos + 1..];
        if rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('!') {
            continue;
        }
        let Some(end) = rest.find('>') else { continue };
        let tag = &rest[..end];
        let name_end = tag
            .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .unwrap_or(tag.len());
        let name = &tag[..name_end];
        let lname = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
        if lname == local {
            out.push(tag);
        }
    }
    out
}

/// Inner content of every `<pfx:local …>…</pfx:local>` element (exact
/// local-name match, any prefix). Self-closing elements yield an empty slice.
/// xdr anchors never nest same-named elements, so the first matching close tag
/// after the open is the element's end.
fn element_blocks<'a>(xml: &'a str, local: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    for (pos, _) in xml.match_indices('<') {
        let rest = &xml[pos + 1..];
        if rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('!') {
            continue;
        }
        let Some(open_end) = rest.find('>') else { continue };
        let tag = &rest[..open_end];
        let name_end = tag
            .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .unwrap_or(tag.len());
        let name = &tag[..name_end];
        let lname = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
        if lname != local {
            continue;
        }
        if tag.ends_with('/') {
            out.push(&rest[open_end..open_end]); // self-closing → empty inner
            continue;
        }
        let inner = &rest[open_end + 1..];
        let close = format!("</{name}>");
        if let Some(close_pos) = inner.find(&close) {
            out.push(&inner[..close_pos]);
        }
    }
    out
}

