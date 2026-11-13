//! SmartArt diagram rendering via the pre-rendered DrawingML fallback.
//!
//! A SmartArt graphic is authored as a `<p:graphicFrame>` whose
//! `<a:graphic><a:graphicData uri=".../diagram">` carries `<dgm:relIds>` pointing
//! at the four diagram parts (data / layout / quickStyle / colors). PowerPoint
//! ALSO ships a PRE-RENDERED fallback — `ppt/diagrams/drawingN.xml`, a
//! `<dsp:drawing>` (namespace `…/office/drawing/2008/diagram`) whose `<dsp:spTree>`
//! holds `<dsp:sp>` shapes that are STRUCTURALLY IDENTICAL to `<p:sp>` (each has a
//! `<dsp:spPr>` with `a:xfrm` + `prstGeom`/`custGeom` + fills + `a:ln`, plus a
//! `<dsp:txBody>`). We render that fallback rather than running the (out-of-scope)
//! live diagram layout algorithm.
//!
//! Resolution chain (§ none — Microsoft extension):
//!   graphicFrame → `dgm:relIds@r:dm` (rId of the diagram DATA part)
//!     → `ppt/diagrams/dataN.xml` → its `_rels` `dsp:dataModelExt@relId`
//!     → the SLIDE's rels relationship of type `…/diagramDrawing`
//!     → `ppt/diagrams/drawingN.xml`.
//! (The drawing relationship lives on the SLIDE part — `data1.xml`'s own
//! `dataModelExt@relId` names the rId, but that rId is resolved against the slide's
//! relationships, which is where the `diagramDrawing` rel is declared.)
//!
//! Positioning: the `dsp:sp` `a:xfrm` are EMU in the DRAWING's own coordinate
//! space, which equals the graphicFrame's `ext` box. We map drawing-space →
//! slide-space with a [`GroupTransform`] (exactly the group flatten): child
//! space = the drawing extent (bounding box of all `dsp:sp`, or the frame ext when
//! they coincide), parent box = the frame `off`/`ext`. Each `dsp:sp` then flows
//! through the SAME per-shape builder (geometry via `preset_path` / the raw
//! custGeom, schemeClr-resolved fills/outlines, txBody runs) that top-level and
//! grouped shapes use — no diagram-specific painter.

use crate::engine::opc;

use super::shape::GroupTransform;
use ooxml_pml::types::Shape;

/// The graphicData `uri` that marks a `<p:graphicFrame>` as a SmartArt diagram
/// (vs. the table / chart uris — those keep their own paths).
const DIAGRAM_URI: &str = "http://schemas.openxmlformats.org/drawingml/2006/diagram";
/// OPC relationship type for the pre-rendered diagram DRAWING fallback part.
const REL_DIAGRAM_DRAWING: &str =
    "http://schemas.microsoft.com/office/2007/relationships/diagramDrawing";

/// One diagram shape resolved from a drawing fallback: a parsed [`Shape`] (its
/// `dsp:spPr`/`dsp:txBody` parsed by the namespace-agnostic `Shape::from_xml`),
/// the [`GroupTransform`] that maps its drawing-space `a:xfrm` box into absolute
/// slide space, and the raw `<a:custGeom>` markup for it (if any — overlaid the
/// same way a slide shape's custom geometry is overlaid).
pub(crate) struct DiagramShape {
    pub shape: Shape,
    pub transform: GroupTransform,
    pub custgeom: Option<String>,
}

/// A graphicFrame referencing a diagram: its frame box (EMU) + the diagram DATA
/// part relId (`dgm:relIds@r:dm`), raw-scanned from the slide part.
struct DiagramFrame {
    off_x: i64,
    off_y: i64,
    ext_cx: i64,
    ext_cy: i64,
    data_rel_id: String,
}

/// Resolve every SmartArt diagram on one slide to a flat list of [`DiagramShape`]s
/// built from its pre-rendered drawing fallback. Returns an empty vec when the
/// slide has no diagram frames, no drawing fallback exists (older decks — the live
/// layout algorithm is out of scope), or any part is unreadable (graceful — the
/// frame then renders blank). `pkg` is the load's SHARED effective
/// [`opc::OpcPackage`] (an earlier version re-opened the whole archive once PER
/// SLIDE here, the single largest redundant cost of a load).
pub(crate) fn slide_diagram_shapes(
    pkg: Option<&mut opc::OpcPackage<'_>>,
    slide_path: &str,
) -> Vec<DiagramShape> {
    if slide_path.is_empty() {
        return Vec::new();
    }
    let Some(pkg) = pkg else {
        return Vec::new();
    };
    let Some(slide_xml) = pkg.read_part_string(slide_path) else {
        return Vec::new();
    };
    let frames = scan_diagram_frames(&slide_xml);
    if frames.is_empty() {
        return Vec::new();
    }
    let slide_rels = pkg.rels(slide_path);

    let mut out = Vec::new();
    for frame in &frames {
        // graphicFrame → data part (dgm:relIds@r:dm).
        let Some(data_rel) = slide_rels.get(&frame.data_rel_id) else {
            continue;
        };
        let data_path = opc::resolve_target(slide_path, &data_rel.target);
        // data part → its dataModelExt@relId (the drawing rId, resolved on the
        // SLIDE's rels). If the data part lacks the ext (no fallback authored),
        // fall back to scanning the slide rels for ANY diagramDrawing rel.
        let drawing_rel_id = pkg
            .read_part_string(&data_path)
            .and_then(|xml| scan_data_model_ext_rel(&xml));
        let drawing_path = drawing_rel_id
            .as_deref()
            .and_then(|id| slide_rels.get(id))
            .filter(|r| r.relationship_type == REL_DIAGRAM_DRAWING)
            .map(|r| opc::resolve_target(slide_path, &r.target));
        let Some(drawing_path) = drawing_path else {
            continue;
        };
        let Some(drawing_xml) = pkg.read_part_string(&drawing_path) else {
            continue;
        };
        out.extend(build_drawing_shapes(&drawing_xml, frame));
    }
    out
}

/// Parse a `<dsp:drawing>` fallback and build one [`DiagramShape`] per top-level
/// `<dsp:sp>`. Each `dsp:sp` is parsed by the namespace-agnostic `Shape::from_xml`
/// (it keys on `local_name()`, so `dsp:spPr`/`dsp:txBody`/`dsp:style` parse like
/// the `p:` originals), positioned by a [`GroupTransform`] mapping the drawing's
/// coordinate space (the bounding box of all `dsp:sp`) onto the frame box.
fn build_drawing_shapes(drawing_xml: &str, frame: &DiagramFrame) -> Vec<DiagramShape> {
    let sp_spans = scan_dsp_shape_spans(drawing_xml);
    if sp_spans.is_empty() {
        return Vec::new();
    }

    // The drawing coordinate space: the bounding box of all dsp:sp xfrm boxes.
    // The fallback drawing has no group xfrm (`<dsp:grpSpPr/>`), so the shapes ARE
    // in frame-ext space — but a bounding box that DIFFERS from the frame ext is
    // scaled to fit (drawing-space is scaled onto frame-space).
    let (draw_off_x, draw_off_y, draw_ext_cx, draw_ext_cy) = drawing_bounds(&sp_spans);
    let transform = frame_transform(
        frame,
        draw_off_x,
        draw_off_y,
        draw_ext_cx,
        draw_ext_cy,
    );

    let mut out = Vec::new();
    for span in &sp_spans {
        let Some(shape) = parse_dsp_shape(span) else {
            continue;
        };
        let custgeom = super::extract_custgeom(span);
        out.push(DiagramShape {
            shape,
            transform,
            custgeom,
        });
    }
    out
}

/// Build the drawing-space → slide-space [`GroupTransform`]: child space =
/// `(draw_off, draw_ext)`, parent box = the frame `(off, ext)`. Degenerate drawing
/// extent (zero on an axis) → a pure translation by the frame off (no scale), so a
/// malformed drawing still lands at the frame rather than collapsing.
fn frame_transform(
    frame: &DiagramFrame,
    draw_off_x: i64,
    draw_off_y: i64,
    draw_ext_cx: i64,
    draw_ext_cy: i64,
) -> GroupTransform {
    if draw_ext_cx <= 0 || draw_ext_cy <= 0 {
        return GroupTransform {
            sx: 1.0,
            sy: 1.0,
            tx: frame.off_x as f64,
            ty: frame.off_y as f64,
        };
    }
    let sx = frame.ext_cx as f64 / draw_ext_cx as f64;
    let sy = frame.ext_cy as f64 / draw_ext_cy as f64;
    // parent = off + (child - draw_off) * scale = scale*child + (off - draw_off*scale)
    GroupTransform {
        sx,
        sy,
        tx: frame.off_x as f64 - draw_off_x as f64 * sx,
        ty: frame.off_y as f64 - draw_off_y as f64 * sy,
    }
}

/// The bounding box (off_x, off_y, ext_cx, ext_cy) in EMU of every `<dsp:sp>`'s
/// `a:xfrm` off/ext. A span with no parseable xfrm contributes nothing. With no
/// boxes at all, returns `(0, 0, 0, 0)` (caller then uses a pure translation).
fn drawing_bounds(spans: &[&str]) -> (i64, i64, i64, i64) {
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    for span in spans {
        if let Some((ox, oy, cx, cy)) = scan_sp_xfrm(span) {
            min_x = min_x.min(ox);
            min_y = min_y.min(oy);
            max_x = max_x.max(ox + cx);
            max_y = max_y.max(oy + cy);
        }
    }
    if min_x == i64::MAX {
        return (0, 0, 0, 0);
    }
    (min_x, min_y, max_x - min_x, max_y - min_y)
}

/// Raw-scan a `<dsp:sp>` span for its `spPr/a:xfrm` `off`/`ext` (EMU). Returns
/// `(off_x, off_y, ext_cx, ext_cy)` or `None` when either is absent. Used only for
/// the bounding box (the parsed `Shape` carries the same xfrm for the box itself).
fn scan_sp_xfrm(span: &str) -> Option<(i64, i64, i64, i64)> {
    let off = span.find("<a:off ")?;
    let (ox, oy) = parse_xy(&span[off..])?;
    let (cx, cy) = find_extent(span)?;
    Some((ox, oy, cx, cy))
}

/// Find the TRANSFORM extent (`<a:ext cx="…" cy="…"/>`) in a span, skipping any
/// extension-list `<a:ext uri="…">` elements (which share the `a:ext` tag name but
/// carry NO `cx`). Scans each `<a:ext ` occurrence and returns the first whose tag
/// head parses a `cx`/`cy` pair. `None` if there is no transform ext.
fn find_extent(span: &str) -> Option<(i64, i64)> {
    let mut from = 0usize;
    while let Some(rel) = span[from..].find("<a:ext ") {
        let at = from + rel;
        if let Some(pair) = parse_xy(&span[at..]) {
            return Some(pair);
        }
        from = at + "<a:ext ".len();
    }
    None
}

/// Parse the first `x="…" y="…"` (or `cx="…" cy="…"`) attribute pair out of an
/// `<a:off …>` / `<a:ext …>` tag head. Returns `None` if either is missing/
/// non-numeric.
fn parse_xy(s: &str) -> Option<(i64, i64)> {
    let a = attr_i64(s, " x=").or_else(|| attr_i64(s, " cx="))?;
    let b = attr_i64(s, " y=").or_else(|| attr_i64(s, " cy="))?;
    Some((a, b))
}

/// Extract a quoted integer attribute value following `needle` (e.g. ` x=`) at the
/// START of `s`'s tag, stopping at the tag's `>`. `None` if absent/non-numeric.
fn attr_i64(s: &str, needle: &str) -> Option<i64> {
    let tag_end = s.find('>').unwrap_or(s.len());
    let head = &s[..tag_end];
    let at = head.find(needle)?;
    let rest = &head[at + needle.len()..];
    let rest = rest.trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    rest[..end].trim().parse::<i64>().ok()
}

/// Parse one `<dsp:sp>…</dsp:sp>` span into a [`Shape`] via the fork's
/// namespace-agnostic `Shape::from_xml`. The parser keys on `local_name()` so
/// `dsp:` children parse exactly like `p:` — EXCEPT a `dsp:nvSpPr` carries no
/// `nvPr` child (it is presentation-level, absent on diagram shapes), which the
/// fork's `ShapeNonVisual` parser REQUIRES. We inject a synthetic `<p:nvPr/>` into
/// the `nvSpPr` envelope before parsing (an empty nvPr is semantically a no-op:
/// it carries no placeholder/media — the shape draws purely from its `spPr` +
/// `txBody`). Returns `None` on a parse error (the shape is dropped — graceful).
fn parse_dsp_shape(span: &str) -> Option<Shape> {
    use ooxml_pml::parsers::FromXml;
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let patched = inject_nv_pr(span);
    let mut reader = Reader::from_str(&patched);
    reader.config_mut().check_end_names = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"sp" => {
                return Shape::from_xml(&mut reader, &e, false).ok();
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// Insert a synthetic empty `<p:nvPr/>` just before the `nvSpPr` close tag so the
/// fork's `ShapeNonVisual` parser (which requires an `nvPr`) accepts a diagram
/// shape's `dsp:nvSpPr` (which has none). Handles both the `dsp:`-prefixed and
/// bare `</nvSpPr>` close. When neither close is present (no nvSpPr — unusual),
/// returns the span unchanged (the parser then fails on the missing nvSpPr, and
/// the shape is dropped — graceful). The `p:` prefix is inert: the parser keys on
/// the `nvPr` local name. Cow-style: only allocates when an injection happens.
fn inject_nv_pr(span: &str) -> std::borrow::Cow<'_, str> {
    for close in ["</dsp:nvSpPr>", "</nvSpPr>"] {
        if let Some(at) = span.find(close) {
            let mut s = String::with_capacity(span.len() + 12);
            s.push_str(&span[..at]);
            s.push_str("<p:nvPr/>");
            s.push_str(&span[at..]);
            return std::borrow::Cow::Owned(s);
        }
    }
    std::borrow::Cow::Borrowed(span)
}

/// Raw-scan a `<dsp:drawing>` and return the raw `<dsp:sp>…</dsp:sp>` span of each
/// TOP-LEVEL drawing shape (those directly under `<dsp:spTree>`, skipping the
/// `nvGrpSpPr`/`grpSpPr` envelope and any nested groups — diagram fallbacks are
/// flat, but we guard depth like the slide custGeom scan). Numeric coordinates +
/// schemeClr names need no entity decoding (raw byte slice).
fn scan_dsp_shape_spans(xml: &str) -> Vec<&str> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    let mut buf = Vec::new();

    let mut out: Vec<&str> = Vec::new();
    let mut grp_depth = 0i32; // nested group depth below spTree
    let mut in_sp_tree = false;
    let mut sp_start: Option<usize> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"spTree" => in_sp_tree = true,
                // A nested <dsp:grpSp> below the spTree increases depth (skip its
                // children — the flat fallback has none, but guard anyway).
                b"grpSp" if in_sp_tree => grp_depth += 1,
                b"sp" if in_sp_tree && grp_depth == 0 && sp_start.is_none() => {
                    sp_start = Some(reader.buffer_position() as usize - e.len() - 2);
                }
                _ => {}
            },
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"spTree" => in_sp_tree = false,
                b"grpSp" if in_sp_tree => grp_depth = (grp_depth - 1).max(0),
                b"sp" if in_sp_tree && grp_depth == 0 => {
                    if let Some(start) = sp_start.take() {
                        let end = reader.buffer_position() as usize;
                        if let Some(span) = xml.get(start..end) {
                            out.push(span);
                        }
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

/// Raw-scan a slide part for every `<p:graphicFrame>` whose `<a:graphicData uri>`
/// is the diagram uri, capturing its `<p:xfrm>` off/ext (EMU) + the `dgm:relIds`
/// data-part rId (`@r:dm`). Table / chart graphicFrames (other uris) are skipped
/// so their existing paths (table.rs / chart) stay intact.
fn scan_diagram_frames(xml: &str) -> Vec<DiagramFrame> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    let mut buf = Vec::new();

    let mut out = Vec::new();
    let mut frame_start: Option<usize> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"graphicFrame" => {
                frame_start = Some(reader.buffer_position() as usize - e.len() - 2);
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"graphicFrame" => {
                if let Some(start) = frame_start.take() {
                    let end = reader.buffer_position() as usize;
                    if let Some(span) = xml.get(start..end) {
                        if let Some(frame) = parse_diagram_frame(span) {
                            out.push(frame);
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Parse one `<p:graphicFrame>` span into a [`DiagramFrame`], or `None` if it is
/// NOT a diagram frame (wrong/absent graphicData uri) or carries no `dgm:relIds`
/// data rId. The frame box comes from the `<p:xfrm>` `a:off`/`a:ext`.
fn parse_diagram_frame(span: &str) -> Option<DiagramFrame> {
    // Must be the diagram uri (skip table/chart frames).
    let uri_at = span.find("graphicData")?;
    let uri = attr_str(&span[uri_at..], " uri=")?;
    if uri != DIAGRAM_URI {
        return None;
    }
    // dgm:relIds@r:dm — the data part relId.
    let rel_at = span.find("relIds")?;
    let data_rel_id = attr_str(&span[rel_at..], ":dm=")
        .or_else(|| attr_str(&span[rel_at..], " dm="))?;
    // Frame box (<p:xfrm> off/ext). The TRANSFORM ext is found via `find_extent`,
    // which skips the `<a:ext uri="…">` extension-list elements PowerPoint emits in
    // `nvGraphicFramePr` (they share the `a:ext` tag but carry no `cx`). Absent
    // off/ext → (0,0) (honest — PowerPoint always writes the box).
    let (off_x, off_y) = span
        .find("<a:off ")
        .and_then(|i| parse_xy(&span[i..]))
        .unwrap_or((0, 0));
    let (ext_cx, ext_cy) = find_extent(span).unwrap_or((0, 0));
    Some(DiagramFrame {
        off_x,
        off_y,
        ext_cx,
        ext_cy,
        data_rel_id,
    })
}

/// Scan a diagram DATA part for `<dsp:dataModelExt relId="…">` — the rId of the
/// pre-rendered drawing fallback (resolved against the SLIDE's rels). `None` when
/// the ext is absent (no fallback authored → live layout, out of scope).
fn scan_data_model_ext_rel(xml: &str) -> Option<String> {
    let at = xml.find("dataModelExt")?;
    attr_str(&xml[at..], " relId=")
}

/// Extract a quoted string attribute value following `needle` (e.g. ` uri=`,
/// ` relId=`, `:dm=`), stopping at the enclosing tag's `>`. `None` if absent.
fn attr_str(s: &str, needle: &str) -> Option<String> {
    let tag_end = s.find('>').unwrap_or(s.len());
    let head = &s[..tag_end];
    let at = head.find(needle)?;
    let rest = &head[at + needle.len()..];
    let rest = rest.trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}
