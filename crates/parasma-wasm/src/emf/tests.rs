//! Native EMF tests — no browser, no external files. Every EMF byte buffer is
//! synthesized in-memory. The `RecordingPainter` mock captures the primitive
//! stream so the record→Painter mapping AND the transform composition order are
//! pinned red-green (a wrong mapping FAILS, not merely a panic).

use std::cell::RefCell;

use wasm_bindgen::JsValue;

use crate::engine::paint::Painter;

use super::records::{self, EmrRecord};
use super::{dib, parse, render};

// ── the recording mock ──────────────────────────────────────────────────────
#[derive(Clone, Debug, PartialEq)]
enum Op {
    Save,
    Restore,
    BeginPath,
    ClosePath,
    MoveTo(f64, f64),
    LineTo(f64, f64),
    Rect(f64, f64, f64, f64),
    Bezier(f64, f64, f64, f64, f64, f64),
    Ellipse(f64, f64, f64, f64),
    Fill,
    Stroke,
    Clip,
    FillRect(f64, f64, f64, f64),
    FillText(String, f64, f64),
    Font(String),
    TextAlign(String),
    TextBaseline(String),
    FillStyle(String),
    StrokeStyle(String),
    LineWidth(f64),
    DrawImage(String, f64, f64, f64, f64),
    SetLineDash(Vec<f64>),
    ClearLineDash,
}

#[derive(Default)]
struct RecordingPainter {
    ops: RefCell<Vec<Op>>,
}

impl RecordingPainter {
    fn ops(&self) -> Vec<Op> {
        self.ops.borrow().clone()
    }
    fn push(&self, op: Op) {
        self.ops.borrow_mut().push(op);
    }
}

impl Painter for RecordingPainter {
    fn save(&self) {
        self.push(Op::Save);
    }
    fn restore(&self) {
        self.push(Op::Restore);
    }
    fn begin_path(&self) {
        self.push(Op::BeginPath);
    }
    fn close_path(&self) {
        self.push(Op::ClosePath);
    }
    fn move_to(&self, x: f64, y: f64) {
        self.push(Op::MoveTo(x, y));
    }
    fn line_to(&self, x: f64, y: f64) {
        self.push(Op::LineTo(x, y));
    }
    fn rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.push(Op::Rect(x, y, w, h));
    }
    fn arc(&self, _x: f64, _y: f64, _r: f64, _s: f64, _e: f64) -> Result<(), JsValue> {
        Ok(())
    }
    fn arc_with_anticlockwise(
        &self,
        _x: f64,
        _y: f64,
        _r: f64,
        _s: f64,
        _e: f64,
        _a: bool,
    ) -> Result<(), JsValue> {
        Ok(())
    }
    fn ellipse_with_anticlockwise(
        &self,
        x: f64,
        y: f64,
        rx: f64,
        ry: f64,
        _rot: f64,
        _s: f64,
        _e: f64,
        _a: bool,
    ) -> Result<(), JsValue> {
        self.push(Op::Ellipse(x, y, rx, ry));
        Ok(())
    }
    fn quadratic_curve_to(&self, _cpx: f64, _cpy: f64, _x: f64, _y: f64) {}
    fn bezier_curve_to(&self, c1x: f64, c1y: f64, c2x: f64, c2y: f64, x: f64, y: f64) {
        self.push(Op::Bezier(c1x, c1y, c2x, c2y, x, y));
    }
    fn fill(&self) {
        self.push(Op::Fill);
    }
    fn stroke(&self) {
        self.push(Op::Stroke);
    }
    fn clip(&self) {
        self.push(Op::Clip);
    }
    fn fill_rect(&self, x: f64, y: f64, w: f64, h: f64) {
        self.push(Op::FillRect(x, y, w, h));
    }
    fn stroke_rect(&self, _x: f64, _y: f64, _w: f64, _h: f64) {}
    fn fill_text(&self, text: &str, x: f64, y: f64) -> Result<(), JsValue> {
        self.push(Op::FillText(text.to_string(), x, y));
        Ok(())
    }
    fn stroke_text(&self, _text: &str, _x: f64, _y: f64) -> Result<(), JsValue> {
        Ok(())
    }
    fn set_font(&self, v: &str) {
        self.push(Op::Font(v.to_string()));
    }
    fn set_text_align(&self, v: &str) {
        self.push(Op::TextAlign(v.to_string()));
    }
    fn set_text_baseline(&self, v: &str) {
        self.push(Op::TextBaseline(v.to_string()));
    }
    fn set_fill_style_str(&self, v: &str) {
        self.push(Op::FillStyle(v.to_string()));
    }
    fn set_stroke_style_str(&self, v: &str) {
        self.push(Op::StrokeStyle(v.to_string()));
    }
    fn set_line_width(&self, v: f64) {
        self.push(Op::LineWidth(v));
    }
    fn set_global_alpha(&self, _v: f64) {}
    fn set_shadow_color(&self, _v: &str) {}
    fn set_shadow_offset_x(&self, _v: f64) {}
    fn set_shadow_offset_y(&self, _v: f64) {}
    fn set_shadow_blur(&self, _v: f64) {}
    fn translate(&self, _x: f64, _y: f64) -> Result<(), JsValue> {
        Ok(())
    }
    fn rotate(&self, _a: f64) -> Result<(), JsValue> {
        Ok(())
    }
    fn scale(&self, _x: f64, _y: f64) -> Result<(), JsValue> {
        Ok(())
    }
    fn set_transform(&self, _a: f64, _b: f64, _c: f64, _d: f64, _e: f64, _f: f64) -> Result<(), JsValue> {
        Ok(())
    }
    fn draw_image(&self, key: &str, dx: f64, dy: f64, dw: f64, dh: f64) -> Result<(), JsValue> {
        self.push(Op::DrawImage(key.to_string(), dx, dy, dw, dh));
        Ok(())
    }
    fn measure_text_width(&self, _text: &str) -> f64 {
        0.0
    }
    fn set_linear_gradient(&self, _x0: f64, _y0: f64, _x1: f64, _y1: f64, _stops: &[(f64, String)]) -> Result<(), JsValue> {
        Ok(())
    }
    fn set_radial_gradient(
        &self,
        _x0: f64,
        _y0: f64,
        _r0: f64,
        _x1: f64,
        _y1: f64,
        _r1: f64,
        _stops: &[(f64, String)],
    ) -> Result<(), JsValue> {
        Ok(())
    }
    fn set_pattern_fill(&self, _key: &str, _tile: &crate::engine::pattern::PatternTile, _fg: &str, _bg: &str) -> Result<(), JsValue> {
        Ok(())
    }
    fn set_line_dash(&self, segments: &[f64]) {
        self.push(Op::SetLineDash(segments.to_vec()));
    }
    fn clear_line_dash(&self) {
        self.push(Op::ClearLineDash);
    }
}

// ── EMF byte writer ─────────────────────────────────────────────────────────
struct EmfWriter {
    buf: Vec<u8>,
}

impl EmfWriter {
    fn new() -> Self {
        EmfWriter { buf: Vec::new() }
    }
    fn record(&mut self, itype: u32, body: &[u8]) {
        assert_eq!(body.len() % 4, 0, "record body must be 4-byte aligned");
        self.buf.extend_from_slice(&itype.to_le_bytes());
        self.buf.extend_from_slice(&((8 + body.len()) as u32).to_le_bytes());
        self.buf.extend_from_slice(body);
    }
    fn finish(self) -> Vec<u8> {
        self.buf
    }
}

fn b_u32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}
fn b_i32(v: i32) -> [u8; 4] {
    v.to_le_bytes()
}
fn b_f32(v: f32) -> [u8; 4] {
    v.to_le_bytes()
}

/// Header body (80 bytes) with the given device-unit bounds and (aniso) device
/// metrics.
fn header_body(bounds: (i32, i32, i32, i32)) -> Vec<u8> {
    let mut b = Vec::new();
    // rclBounds
    b.extend_from_slice(&b_i32(bounds.0));
    b.extend_from_slice(&b_i32(bounds.1));
    b.extend_from_slice(&b_i32(bounds.2));
    b.extend_from_slice(&b_i32(bounds.3));
    // rclFrame (.01mm) — arbitrary
    b.extend_from_slice(&b_i32(0));
    b.extend_from_slice(&b_i32(0));
    b.extend_from_slice(&b_i32(2645));
    b.extend_from_slice(&b_i32(1322));
    // dSignature " EMF"
    b.extend_from_slice(&b_u32(0x464D_4520));
    b.extend_from_slice(&b_u32(0x0001_0000)); // nVersion
    b.extend_from_slice(&b_u32(0)); // nBytes
    b.extend_from_slice(&b_u32(0)); // nRecords
    b.extend_from_slice(&2u16.to_le_bytes()); // nHandles
    b.extend_from_slice(&0u16.to_le_bytes()); // sReserved
    b.extend_from_slice(&b_u32(0)); // nDescription
    b.extend_from_slice(&b_u32(0)); // offDescription
    b.extend_from_slice(&b_u32(0)); // nPalEntries
    b.extend_from_slice(&b_i32(1920)); // szlDevice.cx
    b.extend_from_slice(&b_i32(1080)); // szlDevice.cy
    b.extend_from_slice(&b_i32(320)); // szlMillimeters.cx
    b.extend_from_slice(&b_i32(240)); // szlMillimeters.cy
    assert_eq!(b.len(), 80);
    b
}

fn ext_text_out_w_body(refx: i32, refy: i32, s: &str) -> Vec<u8> {
    let mut b = Vec::new();
    // rclBounds (0,0,0,0) — no opaque box
    b.extend_from_slice(&[0u8; 16]);
    b.extend_from_slice(&b_u32(2)); // iGraphicsMode
    b.extend_from_slice(&b_f32(1.0)); // exScale
    b.extend_from_slice(&b_f32(1.0)); // eyScale
    // EMRTEXT
    b.extend_from_slice(&b_i32(refx)); // ptlReference.x
    b.extend_from_slice(&b_i32(refy)); // ptlReference.y
    let n = s.encode_utf16().count() as u32;
    b.extend_from_slice(&b_u32(n)); // nChars
    // offString from RECORD start = 8(header) + 68(fixed body so far after this) …
    // fixed EMRTEXT+preamble length in the body is 68 bytes; string follows.
    b.extend_from_slice(&b_u32(8 + 68)); // offString
    b.extend_from_slice(&b_u32(0)); // fOptions
    b.extend_from_slice(&[0u8; 16]); // rcl
    b.extend_from_slice(&b_u32(0)); // offDx
    debug_assert_eq!(b.len(), 68);
    for u in s.encode_utf16() {
        b.extend_from_slice(&u.to_le_bytes());
    }
    while b.len() % 4 != 0 {
        b.push(0);
    }
    b
}

/// A small canonical EMF: a pen, a MOVETO/LINETO polyline, and a text run.
fn synth_emf() -> Vec<u8> {
    let mut w = EmfWriter::new();
    w.record(records::EMR_HEADER, &header_body((0, 0, 100, 50)));
    w.record(records::EMR_SETMAPMODE, &b_u32(1)); // MM_TEXT
    // CREATEPEN ih=1, PS_SOLID, width 2, red (COLORREF 0x000000FF)
    let mut pen = Vec::new();
    pen.extend_from_slice(&b_u32(1)); // ihPen
    pen.extend_from_slice(&b_u32(0)); // style PS_SOLID
    pen.extend_from_slice(&b_i32(2)); // width.x
    pen.extend_from_slice(&b_i32(0)); // width.y
    pen.extend_from_slice(&b_u32(0x0000_00FF)); // color = red
    w.record(records::EMR_CREATEPEN, &pen);
    w.record(records::EMR_SELECTOBJECT, &b_u32(1));
    // MOVETOEX(10,10)
    let mut mv = Vec::new();
    mv.extend_from_slice(&b_i32(10));
    mv.extend_from_slice(&b_i32(10));
    w.record(records::EMR_MOVETOEX, &mv);
    // LINETO(90,40)
    let mut ln = Vec::new();
    ln.extend_from_slice(&b_i32(90));
    ln.extend_from_slice(&b_i32(40));
    w.record(records::EMR_LINETO, &ln);
    // SETTEXTCOLOR blue (COLORREF 0x00FF0000)
    w.record(records::EMR_SETTEXTCOLOR, &b_u32(0x00FF_0000));
    // EXTTEXTOUTW ref (5,5) "Hi"
    w.record(records::EMR_EXTTEXTOUTW, &ext_text_out_w_body(5, 5, "Hi"));
    // EOF
    let mut eof = Vec::new();
    eof.extend_from_slice(&b_u32(0));
    eof.extend_from_slice(&b_u32(0));
    eof.extend_from_slice(&b_u32(0));
    w.record(records::EMR_EOF, &eof);
    w.finish()
}

// ── (1)+(2) parse assertions ────────────────────────────────────────────────
#[test]
fn parses_synthetic_emf_records() {
    let bytes = synth_emf();
    let pic = parse(&bytes).expect("parse should succeed");
    assert_eq!(pic.bounds.left, 0);
    assert_eq!(pic.bounds.top, 0);
    assert_eq!(pic.bounds.right, 100);
    assert_eq!(pic.bounds.bottom, 50);

    // Pen decoded to the expected color + width.
    let pen = pic.records.iter().find_map(|r| match r {
        EmrRecord::CreatePen(_, p) => Some(p),
        _ => None,
    });
    let pen = pen.expect("a CreatePen record");
    assert_eq!(pen.color, "#ff0000");
    assert_eq!(pen.width, 2);

    // The UTF-16 string decoded to "Hi".
    let txt = pic.records.iter().find_map(|r| match r {
        EmrRecord::ExtTextOut(t) => Some(t),
        _ => None,
    });
    let txt = txt.expect("an ExtTextOut record");
    assert_eq!(txt.text, "Hi");
    assert_eq!(txt.reference.x, 5);
    assert_eq!(txt.reference.y, 5);

    assert!(pic.records.iter().any(|r| matches!(r, EmrRecord::LineTo(p) if p.x == 90 && p.y == 40)));
    assert!(pic.records.iter().any(|r| matches!(r, EmrRecord::Eof)));
}

// ── (3) Painter-op mapping (placement scaling + record→Painter) ─────────────
#[test]
fn renders_synthetic_emf_to_painter_ops() {
    let bytes = synth_emf();
    let pic = parse(&bytes).unwrap();
    let rec = RecordingPainter::default();
    // dest is 2× the bounds (200×100 vs 100×50) → placement scale ×2.
    render::paint(&pic, &rec, 0.0, 0.0, 200.0, 100.0).unwrap();
    let ops = rec.ops();

    // Coordinate composition: (10,10)→(20,20), (90,40)→(180,80).
    let i_move = idx(&ops, &Op::MoveTo(20.0, 20.0));
    let i_line = idx(&ops, &Op::LineTo(180.0, 80.0));
    // Pen stroke: red, width 2×scale(2) = 4.
    let i_stroke_style = idx(&ops, &Op::StrokeStyle("#ff0000".into()));
    let i_lw = idx(&ops, &Op::LineWidth(4.0));
    let i_stroke = idx(&ops, &Op::Stroke);
    // Text: blue fill, "Hi" at mapped ref (5,5)→(10,10).
    let i_fill_style = idx(&ops, &Op::FillStyle("#0000ff".into()));
    let i_text = idx(&ops, &Op::FillText("Hi".into(), 10.0, 10.0));

    // Ordering: the polyline strokes BEFORE the text draws.
    assert!(i_move < i_line, "move before line");
    assert!(i_line < i_stroke_style, "line accumulated before stroke flush");
    assert!(i_stroke_style < i_stroke, "stroke style before stroke");
    assert!(i_lw < i_stroke, "line width before stroke");
    assert!(i_stroke < i_fill_style, "polyline flushed before text fill");
    assert!(i_fill_style < i_text, "text color before text");

    // A font was set for the text run.
    assert!(ops.iter().any(|o| matches!(o, Op::Font(_))), "font set for text");
}

fn idx(ops: &[Op], target: &Op) -> usize {
    ops.iter().position(|o| o == target).unwrap_or_else(|| panic!("op not found: {target:?} in {ops:?}"))
}

// ── (composition order) MM_ANISOTROPIC + world transform ────────────────────
#[test]
fn composition_order_world_then_map_then_place() {
    // bounds == dest ⇒ M_place is identity, isolating M_map · M_world.
    let mut w = EmfWriter::new();
    w.record(records::EMR_HEADER, &header_body((0, 0, 10, 10)));
    w.record(records::EMR_SETMAPMODE, &b_u32(8)); // MM_ANISOTROPIC
    // window ext (10,10), viewport ext (20,20) ⇒ M_map scales ×2.
    let mut we = Vec::new();
    we.extend_from_slice(&b_i32(10));
    we.extend_from_slice(&b_i32(10));
    w.record(records::EMR_SETWINDOWEXTEX, &we);
    let mut ve = Vec::new();
    ve.extend_from_slice(&b_i32(20));
    ve.extend_from_slice(&b_i32(20));
    w.record(records::EMR_SETVIEWPORTEXTEX, &ve);
    // world transform = translate +3 in logical x (applied to the LOGICAL point
    // BEFORE the map scale).
    let mut xf = Vec::new();
    xf.extend_from_slice(&b_f32(1.0)); // eM11
    xf.extend_from_slice(&b_f32(0.0)); // eM12
    xf.extend_from_slice(&b_f32(0.0)); // eM21
    xf.extend_from_slice(&b_f32(1.0)); // eM22
    xf.extend_from_slice(&b_f32(3.0)); // eDx
    xf.extend_from_slice(&b_f32(0.0)); // eDy
    w.record(records::EMR_SETWORLDTRANSFORM, &xf);
    // MOVETOEX(1,1)
    let mut mv = Vec::new();
    mv.extend_from_slice(&b_i32(1));
    mv.extend_from_slice(&b_i32(1));
    w.record(records::EMR_MOVETOEX, &mv);
    let mut ln = Vec::new();
    ln.extend_from_slice(&b_i32(1));
    ln.extend_from_slice(&b_i32(1));
    w.record(records::EMR_LINETO, &ln);
    w.record(records::EMR_EOF, &[0u8; 12]);
    let bytes = w.finish();

    let pic = parse(&bytes).unwrap();
    let rec = RecordingPainter::default();
    render::paint(&pic, &rec, 0.0, 0.0, 10.0, 10.0).unwrap();
    let ops = rec.ops();

    // Correct order p = M_place·M_map·M_world·p_logical:
    //   world: (1,1)+(3,0) = (4,1); map ×2 = (8,2); place identity = (8,2).
    // The REVERSED (map-then-world) order would give (5,2) — this test fails if
    // the composition is inverted.
    assert!(
        ops.iter().any(|o| matches!(o, Op::MoveTo(x, y) if *x == 8.0 && *y == 2.0)),
        "expected MoveTo(8,2) (world→map→place); got {ops:?}"
    );
    assert!(
        !ops.iter().any(|o| matches!(o, Op::MoveTo(x, _) if *x == 5.0)),
        "reversed composition (5,2) must NOT appear: {ops:?}"
    );
}

// ── (4) DIB decode: 2×2 24bpp bottom-up ─────────────────────────────────────
#[test]
fn decodes_bottom_up_24bpp_dib() {
    // BITMAPINFOHEADER (40 bytes)
    let mut bmi = Vec::new();
    bmi.extend_from_slice(&b_u32(40)); // biSize
    bmi.extend_from_slice(&b_i32(2)); // biWidth
    bmi.extend_from_slice(&b_i32(2)); // biHeight (bottom-up)
    bmi.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    bmi.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
    bmi.extend_from_slice(&b_u32(0)); // biCompression BI_RGB
    bmi.extend_from_slice(&b_u32(0)); // biSizeImage
    bmi.extend_from_slice(&b_i32(0)); // biXPels
    bmi.extend_from_slice(&b_i32(0)); // biYPels
    bmi.extend_from_slice(&b_u32(0)); // biClrUsed
    bmi.extend_from_slice(&b_u32(0)); // biClrImportant

    // Row stride = ((2*24+31)/32)*4 = 8 bytes (2 pixels + 2 pad).
    // src row0 (bottom): B,G,R = (1,2,3),(4,5,6)
    // src row1 (top):    B,G,R = (7,8,9),(10,11,12)
    let bits: Vec<u8> = vec![
        1, 2, 3, 4, 5, 6, 0, 0, // row0 (bottom)
        7, 8, 9, 10, 11, 12, 0, 0, // row1 (top)
    ];
    let d = dib::decode(&bmi, &bits).expect("decode");
    assert_eq!(d.w, 2);
    assert_eq!(d.h, 2);
    // Top-down output: dest row0 = src top row (B/R swapped), dest row1 = bottom.
    assert_eq!(
        d.rgba,
        vec![
            9, 8, 7, 255, 12, 11, 10, 255, // top row
            3, 2, 1, 255, 6, 5, 4, 255, // bottom row
        ]
    );
}

// ── (5) tolerance: truncated / bogus never panics, returns None ─────────────
#[test]
fn truncated_and_bogus_return_none() {
    assert!(parse(&[]).is_none());
    assert!(parse(&[1, 0, 0, 0]).is_none()); // header iType but no size
    // Valid header, then a record with a bogus (too-small, unaligned) nSize.
    let mut w = EmfWriter::new();
    w.record(records::EMR_HEADER, &header_body((0, 0, 10, 10)));
    let mut bytes = w.finish();
    bytes.extend_from_slice(&b_u32(54)); // iType LINETO
    bytes.extend_from_slice(&b_u32(3)); // nSize = 3 (< 8, unaligned)
    assert!(parse(&bytes).is_none());

    // Random junk after a header truncated mid-record must not panic.
    let mut w2 = EmfWriter::new();
    w2.record(records::EMR_HEADER, &header_body((0, 0, 10, 10)));
    let mut bytes2 = w2.finish();
    bytes2.extend_from_slice(&[0x36, 0x00, 0x00, 0x00, 0xFF]); // partial record
    let _ = parse(&bytes2); // may be None; must not panic
}

// ── (constants) authoritative MS-EMF iType table ────────────────────────────
#[test]
fn itype_constants_are_authoritative() {
    assert_eq!(records::EMR_ANGLEARC, 41);
    assert_eq!(records::EMR_ELLIPSE, 42);
    assert_eq!(records::EMR_RECTANGLE, 43);
    assert_eq!(records::EMR_ROUNDRECT, 44);
    assert_eq!(records::EMR_ARC, 45);
    assert_eq!(records::EMR_EXTCREATEPEN, 95);
    // 94 is CREATEDIBPATTERNBRUSHPT — must NOT collide with EXTCREATEPEN.
    assert_ne!(records::EMR_EXTCREATEPEN, 94);
}

// ── raster-cache dispatch: the page paint emits ONE image op ────────────────

/// A single-segment PagedLine holding one inline-image item for `rel`.
fn image_line(rel: &str) -> crate::wml::page::PagedLine {
    use crate::wml::linebreak::{ImageItem, Item};
    use crate::wml::page::PagedLine;
    let item = Item {
        width: 100.0,
        content_width: 100.0,
        image: Some(ImageItem {
            rel_id: rel.to_string(),
            height_px: 50.0,
            dist_l_px: 0.0,
            dist_t_px: 0.0,
            dist_r_px: 0.0,
            dist_b_px: 0.0,
        }),
        ..Item::default()
    };
    PagedLine {
        segments: PagedLine::single(vec![item], 0.0),
        baseline_y: 60.0,
        line_height: 50.0,
        content_left: 0.0,
        content_right: 200.0,
        ..PagedLine::default()
    }
}

/// RED-GREEN pin for the raster-cache fix: an ACTIVE EMF rel must record
/// EXACTLY ONE DrawImage under the synthetic `emf-pic:{rel}` key and ZERO
/// vector ops from the EMF program (FillText/strokes leaking back = the old
/// per-paint inline replay regressed). A non-EMF rel still records DrawImage
/// with the BARE rel (fallback chain intact).
#[test]
fn dispatch_records_one_image_op_for_active_emf_rel() {
    use std::collections::HashMap;
    use std::rc::Rc;

    let pic = Rc::new(parse(&synth_emf()).expect("parse"));
    let mut active: HashMap<String, Rc<super::EmfPicture>> = HashMap::new();
    active.insert("rIdE".to_string(), pic);
    super::set_active(&active);

    let rec = RecordingPainter::default();
    crate::wml::render::render_line(&rec, &image_line("rIdE"), &HashMap::new(), &HashMap::new(), false)
        .unwrap();
    let ops = rec.ops();

    let draws: Vec<&Op> = ops.iter().filter(|o| matches!(o, Op::DrawImage(..))).collect();
    assert_eq!(draws.len(), 1, "exactly ONE image op, got {ops:?}");
    assert!(
        matches!(draws[0], Op::DrawImage(k, _, _, w, h) if k == "emf-pic:rIdE" && *w == 100.0 && *h == 50.0),
        "the op must carry the synthetic pic key + drawn box, got {:?}",
        draws[0]
    );
    // The EMF program's ops (synth_emf has a red polyline + "Hi" text) must NOT
    // leak into the page paint.
    assert!(
        !ops.iter().any(|o| matches!(
            o,
            Op::FillText(..) | Op::Stroke | Op::MoveTo(..) | Op::LineTo(..) | Op::Fill
        )),
        "EMF vector ops leaked into the page display list: {ops:?}"
    );

    // The drawn footprint was logged for the TS resolution policy (page-logical
    // px), and draining empties the log.
    let reqs = super::take_requests();
    assert_eq!(reqs, vec![("emf-pic:rIdE".to_string(), 100.0, 50.0)]);
    assert!(super::take_requests().is_empty(), "drain must empty the log");

    // Fallback chain intact: a rel NOT in the active registry records a plain
    // DrawImage with the bare rel.
    let rec2 = RecordingPainter::default();
    crate::wml::render::render_line(&rec2, &image_line("rIdX"), &HashMap::new(), &HashMap::new(), false)
        .unwrap();
    let ops2 = rec2.ops();
    assert!(
        ops2.iter().any(|o| matches!(o, Op::DrawImage(k, ..) if k == "rIdX")),
        "non-EMF rel must keep the bare-rel draw: {ops2:?}"
    );
    assert!(!ops2.iter().any(|o| matches!(o, Op::DrawImage(k, ..) if k.starts_with("emf-pic:"))));
}

/// Anchored-image rider: an active EMF rel drawn through
/// `render_anchored_image` maps to the same synthetic key (previously it drew
/// the bare rel → permanent placeholder).
#[test]
fn anchored_image_dispatches_active_emf_rel() {
    use std::collections::HashMap;
    use std::rc::Rc;

    let pic = Rc::new(parse(&synth_emf()).expect("parse"));
    let mut active: HashMap<String, Rc<super::EmfPicture>> = HashMap::new();
    active.insert("rIdA".to_string(), pic);
    super::set_active(&active);

    let ai = crate::AnchoredImageLayout {
        rel_id: "rIdA".to_string(),
        x: 10.0,
        y: 20.0,
        width_px: 80.0,
        height_px: 40.0,
        behind_text: true,
        anchor_para: None,
    };
    let rec = RecordingPainter::default();
    crate::engine::images::render_anchored_image(&rec, &ai, &HashMap::new());
    let ops = rec.ops();
    assert!(
        ops.iter().any(|o| matches!(o, Op::DrawImage(k, x, y, w, h)
            if k == "emf-pic:rIdA" && *x == 10.0 && *y == 20.0 && *w == 80.0 && *h == 40.0)),
        "anchored EMF must draw the synthetic key: {ops:?}"
    );
    let reqs = super::take_requests();
    assert_eq!(reqs, vec![("emf-pic:rIdA".to_string(), 80.0, 40.0)]);
}

// ── pic_key / is_active / request-log units ─────────────────────────────────
#[test]
fn pic_key_is_prefix_distinct_from_blit_keys() {
    assert_eq!(super::pic_key("rId7"), "emf-pic:rId7");
    // The colon matters: the pic key must NOT be mistaken for an internal DIB
    // blit key ("emf:{rel}:{n}") by any "emf:"-prefix test.
    assert!(!super::pic_key("rId7").starts_with("emf:"));
}

#[test]
fn is_active_tracks_the_registry_and_set_active_clears_requests() {
    use std::collections::HashMap;
    use std::rc::Rc;

    let pic = Rc::new(parse(&synth_emf()).expect("parse"));
    let mut active: HashMap<String, Rc<super::EmfPicture>> = HashMap::new();
    active.insert("rId1".to_string(), pic);
    super::set_active(&active);
    assert!(super::is_active("rId1"));
    assert!(!super::is_active("rId2"));

    super::note_request("rId1", 12.0, 34.0);
    // A new page paint (set_active) clears any undrained requests — the worker
    // never drains, so this bound is what keeps the log from growing forever.
    super::set_active(&active);
    assert!(super::take_requests().is_empty(), "set_active must clear the request log");

    super::set_active(&HashMap::new());
    assert!(!super::is_active("rId1"));
}

// ── polygon fill uses the brush; rectangle path closes ──────────────────────
#[test]
fn polygon_fills_with_brush_then_strokes() {
    let mut w = EmfWriter::new();
    w.record(records::EMR_HEADER, &header_body((0, 0, 10, 10)));
    // CREATEBRUSHINDIRECT ih=1 BS_SOLID green (0x0000FF00 → g=0xff)
    let mut br = Vec::new();
    br.extend_from_slice(&b_u32(1)); // ihBrush
    br.extend_from_slice(&b_u32(0)); // BS_SOLID
    br.extend_from_slice(&b_u32(0x0000_FF00)); // green
    br.extend_from_slice(&b_u32(0)); // hatch
    w.record(records::EMR_CREATEBRUSHINDIRECT, &br);
    w.record(records::EMR_SELECTOBJECT, &b_u32(1));
    // POLYGON16 triangle (0,0),(10,0),(0,10)
    let mut pg = Vec::new();
    pg.extend_from_slice(&[0u8; 16]); // rclBounds
    pg.extend_from_slice(&b_u32(3)); // count
    for (x, y) in [(0i16, 0i16), (10, 0), (0, 10)] {
        pg.extend_from_slice(&x.to_le_bytes());
        pg.extend_from_slice(&y.to_le_bytes());
    }
    w.record(records::EMR_POLYGON16, &pg);
    w.record(records::EMR_EOF, &[0u8; 12]);
    let bytes = w.finish();

    let pic = parse(&bytes).unwrap();
    let rec = RecordingPainter::default();
    render::paint(&pic, &rec, 0.0, 0.0, 10.0, 10.0).unwrap();
    let ops = rec.ops();
    let i_close = ops.iter().position(|o| matches!(o, Op::ClosePath)).expect("close_path");
    let i_fill_style = ops.iter().position(|o| *o == Op::FillStyle("#00ff00".into())).expect("green fill");
    let i_fill = ops.iter().position(|o| matches!(o, Op::Fill)).expect("fill");
    assert!(i_close < i_fill, "path closed before fill");
    assert!(i_fill_style < i_fill, "brush color before fill");
}

// ── (6) file-supplied counts: a record that claims more than it holds ───────
// Each of these reserved memory from a count before checking the body could
// hold it: a 40-byte record could ask for tens of gigabytes and abort the
// engine. Each must now fail only its own record and leave the picture intact.

/// A picture holding the given record, then a LINETO so it stays paintable.
/// The header is not kept as a record, so the given one is `records[0]`.
fn emf_with(itype: u32, body: &[u8]) -> Vec<u8> {
    let mut w = EmfWriter::new();
    w.record(records::EMR_HEADER, &header_body((0, 0, 10, 10)));
    w.record(itype, body);
    w.record(records::EMR_LINETO, &[b_i32(5), b_i32(5)].concat());
    w.record(records::EMR_EOF, &[0u8; 12]);
    w.finish()
}

#[test]
fn point_count_beyond_the_body_skips_the_record() {
    for itype in [records::EMR_POLYLINE, records::EMR_POLYLINE16, records::EMR_POLYBEZIER16] {
        let mut body = vec![0u8; 16]; // rclBounds
        body.extend_from_slice(&b_u32(u32::MAX)); // count
        body.extend_from_slice(&[0u8; 8]); // one point's worth of data
        let pic = parse(&emf_with(itype, &body)).expect("picture survives");
        assert!(matches!(pic.records[0], EmrRecord::Skip), "itype {itype}");
    }
}

#[test]
fn poly_count_beyond_the_body_skips_the_record() {
    for (n_polys, per_poly) in [(u32::MAX, 1u32), (1, u32::MAX)] {
        let mut body = vec![0u8; 16]; // rclBounds
        body.extend_from_slice(&b_u32(n_polys));
        body.extend_from_slice(&b_u32(u32::MAX)); // cptl
        body.extend_from_slice(&b_u32(per_poly)); // aPolyCounts[0]
        body.extend_from_slice(&[0u8; 8]);
        let pic = parse(&emf_with(records::EMR_POLYPOLYGON16, &body)).expect("picture survives");
        assert!(matches!(pic.records[0], EmrRecord::Skip), "n_polys {n_polys}, count {per_poly}");
    }
}

#[test]
fn poly_poly_still_reads_well_formed_counts() {
    let mut body = vec![0u8; 16]; // rclBounds
    body.extend_from_slice(&b_u32(2)); // nPolys
    body.extend_from_slice(&b_u32(3)); // cptl
    body.extend_from_slice(&b_u32(1));
    body.extend_from_slice(&b_u32(2));
    for (x, y) in [(1i16, 2i16), (3, 4), (5, 6)] {
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());
    }
    let pic = parse(&emf_with(records::EMR_POLYPOLYGON16, &body)).unwrap();
    let EmrRecord::PolyPoly { polys, .. } = &pic.records[0] else { panic!("{:?}", pic.records[0]) };
    let xs: Vec<Vec<i32>> = polys.iter().map(|p| p.iter().map(|q| q.x).collect()).collect();
    assert_eq!(xs, vec![vec![1], vec![3, 5]]);
}

#[test]
fn text_char_count_beyond_the_body_reads_what_is_there() {
    let mut body = ext_text_out_w_body(0, 0, "");
    body[36..40].copy_from_slice(&b_u32(u32::MAX)); // nChars
    body[40..44].copy_from_slice(&b_u32(0)); // offString: none
    body[64..68].copy_from_slice(&b_u32(8 + 64)); // offDx: one entry in the body
    let pic = parse(&emf_with(records::EMR_EXTTEXTOUTW, &body)).expect("picture survives");
    assert!(matches!(pic.records[0], EmrRecord::ExtTextOut(_)), "{:?}", pic.records[0]);
}

#[test]
fn object_index_beyond_sixteen_bits_is_ignored() {
    let mut pen = Vec::new();
    pen.extend_from_slice(&b_u32(0x7FFF_FFFF)); // ihPen
    pen.extend_from_slice(&b_u32(0)); // PS_SOLID
    pen.extend_from_slice(&b_i32(1));
    pen.extend_from_slice(&b_i32(0));
    pen.extend_from_slice(&b_u32(0x0000_00FF)); // red
    let pic = parse(&emf_with(records::EMR_CREATEPEN, &pen)).unwrap();
    let rec = RecordingPainter::default();
    render::paint(&pic, &rec, 0.0, 0.0, 10.0, 10.0).unwrap();
    assert!(!rec.ops().contains(&Op::StrokeStyle("#ff0000".into())), "the pen was never stored");
}

#[test]
fn dib_whose_row_size_would_wrap_is_refused() {
    // 0x0AAA_AAAB × 24 bits = 2^32 + 8: in 32-bit arithmetic the stride wraps
    // to 4 bytes and four bytes of pixel data would pass the length check.
    let mut bmi = vec![0u8; 40];
    bmi[0..4].copy_from_slice(&b_u32(40));
    bmi[4..8].copy_from_slice(&b_i32(0x0AAA_AAAB));
    bmi[8..12].copy_from_slice(&b_i32(1));
    bmi[14..16].copy_from_slice(&24u16.to_le_bytes());
    assert_eq!(dib::decode(&bmi, &[0u8; 4]), None);
}
