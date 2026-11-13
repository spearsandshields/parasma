//! MS-EMF record types + decoders. The iType constants below are the
//! AUTHORITATIVE MS-EMF §2.1.1 `RecordType` values (unit-tested in
//! `super::tests::itype_constants_are_authoritative`). Note in particular the
//! primitives whose values are easily confused: `EMR_ELLIPSE = 42`, `EMR_RECTANGLE = 43`,
//! `EMR_ROUNDRECT = 44`, `EMR_ARC = 45`, `EMR_ANGLEARC = 41`, and
//! `EMR_EXTCREATEPEN = 95` (94 = `EMR_CREATEDIBPATTERNBRUSHPT`).

use super::reader::Body;

// ── iType constants (MS-EMF §2.1.1 RecordType) ──────────────────────────────
pub(crate) const EMR_HEADER: u32 = 1;
pub(crate) const EMR_POLYBEZIER: u32 = 2;
pub(crate) const EMR_POLYGON: u32 = 3;
pub(crate) const EMR_POLYLINE: u32 = 4;
pub(crate) const EMR_POLYBEZIERTO: u32 = 5;
pub(crate) const EMR_POLYLINETO: u32 = 6;
pub(crate) const EMR_POLYPOLYLINE: u32 = 7;
pub(crate) const EMR_POLYPOLYGON: u32 = 8;
pub(crate) const EMR_SETWINDOWEXTEX: u32 = 9;
pub(crate) const EMR_SETWINDOWORGEX: u32 = 10;
pub(crate) const EMR_SETVIEWPORTEXTEX: u32 = 11;
pub(crate) const EMR_SETVIEWPORTORGEX: u32 = 12;
pub(crate) const EMR_SETBRUSHORGEX: u32 = 13;
pub(crate) const EMR_EOF: u32 = 14;
pub(crate) const EMR_SETMAPMODE: u32 = 17;
pub(crate) const EMR_SETBKMODE: u32 = 18;
pub(crate) const EMR_SETPOLYFILLMODE: u32 = 19;
pub(crate) const EMR_SETROP2: u32 = 20;
pub(crate) const EMR_SETSTRETCHBLTMODE: u32 = 21;
pub(crate) const EMR_SETTEXTALIGN: u32 = 22;
pub(crate) const EMR_SETTEXTCOLOR: u32 = 24;
pub(crate) const EMR_SETBKCOLOR: u32 = 25;
pub(crate) const EMR_MOVETOEX: u32 = 27;
pub(crate) const EMR_INTERSECTCLIPRECT: u32 = 30;
pub(crate) const EMR_SAVEDC: u32 = 33;
pub(crate) const EMR_RESTOREDC: u32 = 34;
pub(crate) const EMR_SETWORLDTRANSFORM: u32 = 35;
pub(crate) const EMR_MODIFYWORLDTRANSFORM: u32 = 36;
pub(crate) const EMR_SELECTOBJECT: u32 = 37;
pub(crate) const EMR_CREATEPEN: u32 = 38;
pub(crate) const EMR_CREATEBRUSHINDIRECT: u32 = 39;
pub(crate) const EMR_DELETEOBJECT: u32 = 40;
pub(crate) const EMR_ANGLEARC: u32 = 41;
pub(crate) const EMR_ELLIPSE: u32 = 42;
pub(crate) const EMR_RECTANGLE: u32 = 43;
pub(crate) const EMR_ROUNDRECT: u32 = 44;
pub(crate) const EMR_ARC: u32 = 45;
pub(crate) const EMR_LINETO: u32 = 54;
pub(crate) const EMR_BITBLT: u32 = 76;
pub(crate) const EMR_STRETCHDIBITS: u32 = 81;
pub(crate) const EMR_EXTCREATEFONTINDIRECTW: u32 = 82;
pub(crate) const EMR_EXTTEXTOUTA: u32 = 83;
pub(crate) const EMR_EXTTEXTOUTW: u32 = 84;
pub(crate) const EMR_POLYBEZIER16: u32 = 85;
pub(crate) const EMR_POLYGON16: u32 = 86;
pub(crate) const EMR_POLYLINE16: u32 = 87;
pub(crate) const EMR_POLYBEZIERTO16: u32 = 88;
pub(crate) const EMR_POLYLINETO16: u32 = 89;
pub(crate) const EMR_POLYPOLYLINE16: u32 = 90;
pub(crate) const EMR_POLYPOLYGON16: u32 = 91;
pub(crate) const EMR_EXTCREATEPEN: u32 = 95;

// ── TA_* text-align flags (MS-EMF §2.1.31) ──────────────────────────────────
pub(crate) const TA_RIGHT: u32 = 0x0002;
pub(crate) const TA_CENTER: u32 = 0x0006; // RIGHT|(0x0004) — mask with 0x0006
pub(crate) const TA_BOTTOM: u32 = 0x0008;
pub(crate) const TA_BASELINE: u32 = 0x0018;
pub(crate) const TA_UPDATECP: u32 = 0x0001;

// ── ETO_* ExtTextOut option flags (MS-EMF §2.1.11) ──────────────────────────
pub(crate) const ETO_OPAQUE: u32 = 0x0002;
pub(crate) const ETO_GLYPH_INDEX: u32 = 0x0010;
pub(crate) const ETO_PDY: u32 = 0x2000;

// ── pen / brush styles (MS-WMF) ─────────────────────────────────────────────
pub(crate) const PS_NULL: u32 = 5;
pub(crate) const PS_DASH: u32 = 1;
pub(crate) const PS_DOT: u32 = 2;
pub(crate) const PS_DASHDOT: u32 = 3;
pub(crate) const PS_STYLE_MASK: u32 = 0x0000000F;
pub(crate) const BS_NULL: u32 = 1;

// ── stock objects (MS-EMF §2.1.31 StockObject; high bit 0x80000000 set) ─────
pub(crate) const STOCK_FLAG: u32 = 0x8000_0000;
pub(crate) const WHITE_BRUSH: u32 = 0x8000_0000;
pub(crate) const BLACK_BRUSH: u32 = 0x8000_0004;
pub(crate) const NULL_BRUSH: u32 = 0x8000_0005;
pub(crate) const BLACK_PEN: u32 = 0x8000_0007;
pub(crate) const NULL_PEN: u32 = 0x8000_0008;

// ── POD structs ─────────────────────────────────────────────────────────────
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub(crate) struct RectL {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PointL {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct XForm {
    pub m11: f32,
    pub m12: f32,
    pub m21: f32,
    pub m22: f32,
    pub dx: f32,
    pub dy: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LogPen {
    pub style: u32,
    pub width: i32,
    pub color: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LogBrush {
    pub style: u32,
    pub color: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LogFont {
    pub height: i32,
    pub weight: i32,
    pub italic: bool,
    pub face: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EmrText {
    pub reference: PointL,
    pub text: String,
    pub dx: Vec<i32>,
    pub options: u32,
    pub bounds: RectL,
}

/// Common source-blit header — the fields the renderer needs, unified across
/// BITBLT and STRETCHDIBITS. `blit` is the index into `EmfPicture::blits` filled
/// in by the parse loop after the DIB is decoded (`None` = unsupported DIB).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BlitHeader {
    pub dest: RectL, // (left,top,right,bottom) = (xDest,yDest,xDest+cxDest,...)
    pub blit: Option<usize>,
    pub rop: u32,
    /// The DIB byte offsets (from the record start) so the parse loop can decode.
    pub off_bmi: usize,
    pub cb_bmi: usize,
    pub off_bits: usize,
    pub cb_bits: usize,
}

/// The decoded record variants the renderer walks. Everything unhandled decodes
/// to `Skip` so parsing continues gracefully.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum EmrRecord {
    MoveToEx(PointL),
    LineTo(PointL),
    /// (points, close: fill+close as a polygon, from_cur: start at cur_pt).
    Poly { points: Vec<PointL>, close: bool, from_cur: bool },
    /// One or more sub-polys sharing a path. `close` fills them as polygons.
    PolyPoly { polys: Vec<Vec<PointL>>, close: bool },
    Bezier { points: Vec<PointL>, from_cur: bool },
    Rectangle(RectL),
    Ellipse(RectL),
    RoundRect(RectL),
    SetWindowExtEx(i32, i32),
    SetWindowOrgEx(i32, i32),
    SetViewportExtEx(i32, i32),
    SetViewportOrgEx(i32, i32),
    SetMapMode(u32),
    SetWorldTransform(XForm),
    ModifyWorldTransform(XForm, u32),
    SetTextColor(String),
    SetBkColor(String),
    SetBkMode(u32),
    SetTextAlign(u32),
    SetPolyFillMode(u32),
    ExtTextOut(EmrText),
    CreatePen(u32, LogPen),
    CreateBrush(u32, LogBrush),
    CreateFont(u32, LogFont),
    SelectObject(u32),
    DeleteObject(u32),
    SaveDc,
    RestoreDc(i32),
    IntersectClipRect(RectL),
    Blit(BlitHeader),
    Eof,
    Skip,
}

fn colorref(v: u32) -> String {
    // COLORREF = 0x00BBGGRR — low byte is red.
    let r = v & 0xFF;
    let g = (v >> 8) & 0xFF;
    let b = (v >> 16) & 0xFF;
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn rectl(b: &Body, off: usize) -> Option<RectL> {
    Some(RectL {
        left: b.i32(off)?,
        top: b.i32(off + 4)?,
        right: b.i32(off + 8)?,
        bottom: b.i32(off + 12)?,
    })
}

// Every count below comes from the file. Each reader takes the whole array as
// one bounds-checked slice BEFORE allocating, so a count the record body cannot
// hold fails the record instead of reserving gigabytes from a 40-byte record.

fn read_points_l(b: &Body, off: usize, n: usize) -> Option<Vec<PointL>> {
    let bytes = b.slice(off, n.checked_mul(8)?)?;
    Some(
        bytes
            .chunks_exact(8)
            .map(|c| PointL {
                x: i32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                y: i32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            })
            .collect(),
    )
}

fn read_points_s(b: &Body, off: usize, n: usize) -> Option<Vec<PointL>> {
    let bytes = b.slice(off, n.checked_mul(4)?)?;
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| PointL {
                x: i16::from_le_bytes([c[0], c[1]]) as i32,
                y: i16::from_le_bytes([c[2], c[3]]) as i32,
            })
            .collect(),
    )
}

/// Decode ONE record from its `body` (the bytes AFTER the 8-byte iType/nSize
/// header). For blit records the caller (`super::mod`) additionally decodes the
/// inline DIB; here we only surface the header offsets. Unknown types → `Skip`.
pub(crate) fn decode(itype: u32, body: &[u8]) -> EmrRecord {
    let b = Body::new(body);
    decode_inner(itype, &b).unwrap_or(EmrRecord::Skip)
}

fn point_l(b: &Body, off: usize) -> Option<PointL> {
    Some(PointL { x: b.i32(off)?, y: b.i32(off + 4)? })
}

fn poly_l(b: &Body, from_cur: bool, close: bool) -> Option<EmrRecord> {
    // rclBounds(16) + count(u32) + PointL[]
    let count = b.u32(16)? as usize;
    let points = read_points_l(b, 20, count)?;
    Some(EmrRecord::Poly { points, close, from_cur })
}

fn poly_s(b: &Body, from_cur: bool, close: bool) -> Option<EmrRecord> {
    let count = b.u32(16)? as usize;
    let points = read_points_s(b, 20, count)?;
    Some(EmrRecord::Poly { points, close, from_cur })
}

fn bezier_l(b: &Body, from_cur: bool) -> Option<EmrRecord> {
    let count = b.u32(16)? as usize;
    let points = read_points_l(b, 20, count)?;
    Some(EmrRecord::Bezier { points, from_cur })
}

fn bezier_s(b: &Body, from_cur: bool) -> Option<EmrRecord> {
    let count = b.u32(16)? as usize;
    let points = read_points_s(b, 20, count)?;
    Some(EmrRecord::Bezier { points, from_cur })
}

fn poly_poly(b: &Body, close: bool, small: bool) -> Option<EmrRecord> {
    // rclBounds(16) + nPolys(u32) + cptl(u32) + aPolyCounts(u32[nPolys]) + pts[]
    // `cptl` (offset 20) is not consulted: the per-poly counts are trusted when
    // the two disagree, and each count is checked against the body as it is read.
    let n_polys = b.u32(16)? as usize;
    let counts = b.slice(24, n_polys.checked_mul(4)?)?;
    let mut off = 24 + counts.len();
    let mut polys = Vec::with_capacity(n_polys);
    for c in counts.chunks_exact(4) {
        let c = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as usize;
        let pts = if small { read_points_s(b, off, c)? } else { read_points_l(b, off, c)? };
        off += pts.len() * if small { 4 } else { 8 };
        polys.push(pts);
    }
    Some(EmrRecord::PolyPoly { polys, close })
}

fn decode_ext_text(b: &Body, wide: bool) -> Option<EmrRecord> {
    // rclBounds(16) iGraphicsMode(4) exScale(4) eyScale(4) then EMRTEXT at 28:
    //   ptlReference(8) nChars(4) offString(4) fOptions(4) rcl(16) offDx(4)
    // offString / offDx are offsets from the RECORD start (iType). body starts 8
    // bytes in, so subtract 8.
    let reference = point_l(b, 28)?;
    let n_chars = b.u32(36)? as usize;
    let off_string = b.u32(40)? as usize;
    let options = b.u32(44)?;
    let bounds = rectl(b, 48)?;
    let off_dx = b.u32(64)? as usize;

    let text = if off_string >= 8 {
        let s = off_string - 8;
        if wide {
            let slice = b.slice(s, n_chars.checked_mul(2)?)?;
            let u16s: Vec<u16> = slice
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&u16s)
        } else {
            let slice = b.slice(s, n_chars)?;
            slice.iter().map(|&c| cp1252(c)).collect()
        }
    } else {
        String::new()
    };

    let dx = if off_dx >= 8 && n_chars > 0 {
        let s = off_dx - 8;
        // The loop below stops at the end of the body, so never reserve more
        // entries than the body could hold.
        let mut v = Vec::with_capacity(n_chars.min(b.bytes.len() / 4));
        let mut off = s;
        for _ in 0..n_chars {
            match b.i32(off) {
                Some(d) => v.push(d),
                None => break,
            }
            off += 4;
            // ETO_PDY doubles the array (x,y pairs); we read the x only and skip y.
            if options & ETO_PDY != 0 {
                off += 4;
            }
        }
        v
    } else {
        Vec::new()
    };

    Some(EmrRecord::ExtTextOut(EmrText { reference, text, dx, options, bounds }))
}

fn cp1252(c: u8) -> char {
    // ASCII passthrough; the 0x80..0x9F band has the Windows-1252 specials, but a
    // lossless-enough mapping is fine for a viewer — fall back to the code point.
    c as char
}

fn decode_inner(itype: u32, b: &Body) -> Option<EmrRecord> {
    Some(match itype {
        EMR_MOVETOEX => EmrRecord::MoveToEx(point_l(b, 0)?),
        EMR_LINETO => EmrRecord::LineTo(point_l(b, 0)?),

        EMR_POLYLINE => poly_l(b, false, false)?,
        EMR_POLYLINETO => poly_l(b, true, false)?,
        EMR_POLYGON => poly_l(b, false, true)?,
        EMR_POLYLINE16 => poly_s(b, false, false)?,
        EMR_POLYLINETO16 => poly_s(b, true, false)?,
        EMR_POLYGON16 => poly_s(b, false, true)?,

        EMR_POLYBEZIER => bezier_l(b, false)?,
        EMR_POLYBEZIERTO => bezier_l(b, true)?,
        EMR_POLYBEZIER16 => bezier_s(b, false)?,
        EMR_POLYBEZIERTO16 => bezier_s(b, true)?,

        EMR_POLYPOLYLINE => poly_poly(b, false, false)?,
        EMR_POLYPOLYGON => poly_poly(b, true, false)?,
        EMR_POLYPOLYLINE16 => poly_poly(b, false, true)?,
        EMR_POLYPOLYGON16 => poly_poly(b, true, true)?,

        EMR_RECTANGLE => EmrRecord::Rectangle(rectl(b, 0)?),
        EMR_ELLIPSE => EmrRecord::Ellipse(rectl(b, 0)?),
        EMR_ROUNDRECT => EmrRecord::RoundRect(rectl(b, 0)?),

        EMR_SETWINDOWEXTEX => EmrRecord::SetWindowExtEx(b.i32(0)?, b.i32(4)?),
        EMR_SETWINDOWORGEX => EmrRecord::SetWindowOrgEx(b.i32(0)?, b.i32(4)?),
        EMR_SETVIEWPORTEXTEX => EmrRecord::SetViewportExtEx(b.i32(0)?, b.i32(4)?),
        EMR_SETVIEWPORTORGEX => EmrRecord::SetViewportOrgEx(b.i32(0)?, b.i32(4)?),
        EMR_SETMAPMODE => EmrRecord::SetMapMode(b.u32(0)?),

        EMR_SETWORLDTRANSFORM => EmrRecord::SetWorldTransform(XForm {
            m11: b.f32(0)?,
            m12: b.f32(4)?,
            m21: b.f32(8)?,
            m22: b.f32(12)?,
            dx: b.f32(16)?,
            dy: b.f32(20)?,
        }),
        EMR_MODIFYWORLDTRANSFORM => EmrRecord::ModifyWorldTransform(
            XForm {
                m11: b.f32(0)?,
                m12: b.f32(4)?,
                m21: b.f32(8)?,
                m22: b.f32(12)?,
                dx: b.f32(16)?,
                dy: b.f32(20)?,
            },
            b.u32(24)?,
        ),

        EMR_SETTEXTCOLOR => EmrRecord::SetTextColor(colorref(b.u32(0)?)),
        EMR_SETBKCOLOR => EmrRecord::SetBkColor(colorref(b.u32(0)?)),
        EMR_SETBKMODE => EmrRecord::SetBkMode(b.u32(0)?),
        EMR_SETTEXTALIGN => EmrRecord::SetTextAlign(b.u32(0)?),
        EMR_SETPOLYFILLMODE => EmrRecord::SetPolyFillMode(b.u32(0)?),

        EMR_EXTTEXTOUTW => decode_ext_text(b, true)?,
        EMR_EXTTEXTOUTA => decode_ext_text(b, false)?,

        EMR_CREATEPEN => {
            // ihPen(u32) LOGPEN{style(u32) width.x(i32) width.y(i32) color(u32)}
            let ih = b.u32(0)?;
            EmrRecord::CreatePen(
                ih,
                LogPen { style: b.u32(4)?, width: b.i32(8)?, color: colorref(b.u32(16)?) },
            )
        }
        EMR_EXTCREATEPEN => {
            // ihPen(u32) off/cb x4(16) then EXTLOGPEN{style(u32) width(u32)
            //   brushStyle(u32) color(u32) ...}
            let ih = b.u32(0)?;
            let style = b.u32(20)?;
            let width = b.u32(24)? as i32;
            let color = colorref(b.u32(32)?);
            EmrRecord::CreatePen(ih, LogPen { style, width, color })
        }
        EMR_CREATEBRUSHINDIRECT => {
            // ihBrush(u32) LOGBRUSH{style(u32) color(u32) hatch(u32)}
            let ih = b.u32(0)?;
            EmrRecord::CreateBrush(ih, LogBrush { style: b.u32(4)?, color: colorref(b.u32(8)?) })
        }
        EMR_EXTCREATEFONTINDIRECTW => {
            // ihFont(u32) LOGFONT{height(i32) width(i32) esc(i32) orient(i32)
            //   weight(i32) italic(u8) ... face[32]*u16}
            let ih = b.u32(0)?;
            let height = b.i32(4)?;
            let weight = b.i32(20)?;
            let italic = b.u16(24).map(|v| (v & 0xFF) != 0).unwrap_or(false);
            // lfFaceName starts 28 bytes into LOGFONT ⇒ body offset 4+28 = 32.
            let mut chars = Vec::new();
            let mut off = 32;
            for _ in 0..32 {
                match b.u16(off) {
                    Some(0) | None => break,
                    Some(u) => chars.push(u),
                }
                off += 2;
            }
            let face = String::from_utf16_lossy(&chars);
            EmrRecord::CreateFont(ih, LogFont { height, weight, italic, face })
        }

        EMR_SELECTOBJECT => EmrRecord::SelectObject(b.u32(0)?),
        EMR_DELETEOBJECT => EmrRecord::DeleteObject(b.u32(0)?),
        EMR_SAVEDC => EmrRecord::SaveDc,
        EMR_RESTOREDC => EmrRecord::RestoreDc(b.i32(0)?),
        EMR_INTERSECTCLIPRECT => EmrRecord::IntersectClipRect(rectl(b, 0)?),

        EMR_BITBLT => {
            // rclBounds(16) xDest yDest cxDest cyDest dwRop xSrc ySrc
            //   xformSrc(24) crBkColorSrc iUsageSrc offBmiSrc cbBmiSrc
            //   offBitsSrc cbBitsSrc
            let xd = b.i32(16)?;
            let yd = b.i32(20)?;
            let cxd = b.i32(24)?;
            let cyd = b.i32(28)?;
            let rop = b.u32(32)?;
            let off_bmi = b.u32(76)? as usize;
            let cb_bmi = b.u32(80)? as usize;
            let off_bits = b.u32(84)? as usize;
            let cb_bits = b.u32(88)? as usize;
            EmrRecord::Blit(BlitHeader {
                dest: RectL { left: xd, top: yd, right: xd + cxd, bottom: yd + cyd },
                blit: None,
                rop,
                off_bmi,
                cb_bmi,
                off_bits,
                cb_bits,
            })
        }
        EMR_STRETCHDIBITS => {
            // rclBounds(16) xDest yDest xSrc ySrc cxSrc cySrc offBmiSrc cbBmiSrc
            //   offBitsSrc cbBitsSrc iUsageSrc dwRop cxDest cyDest
            let xd = b.i32(16)?;
            let yd = b.i32(20)?;
            let off_bmi = b.u32(44)? as usize;
            let cb_bmi = b.u32(48)? as usize;
            let off_bits = b.u32(52)? as usize;
            let cb_bits = b.u32(56)? as usize;
            let rop = b.u32(64)?;
            let cxd = b.i32(68)?;
            let cyd = b.i32(72)?;
            EmrRecord::Blit(BlitHeader {
                dest: RectL { left: xd, top: yd, right: xd + cxd, bottom: yd + cyd },
                blit: None,
                rop,
                off_bmi,
                cb_bmi,
                off_bits,
                cb_bits,
            })
        }

        EMR_EOF => EmrRecord::Eof,

        // Recognised but deliberately degraded to source-copy / no-op (by design):
        // SETROP2 R2_COPYPEN is the only honoured mode; the stretch-blt mode and
        // brush origin do not affect our axis-aligned blits. ARC/ANGLEARC (curved
        // outline primitives) are a documented rendering floor — skipped, not
        // approximated.
        EMR_SETROP2 | EMR_SETSTRETCHBLTMODE | EMR_SETBRUSHORGEX | EMR_ARC | EMR_ANGLEARC => {
            EmrRecord::Skip
        }

        _ => EmrRecord::Skip,
    })
}

/// Resolve a pen `style` to a canvas dash pattern (empty = solid).
pub(crate) fn pen_dash(style: u32, width: f64) -> Vec<f64> {
    let w = width.max(1.0);
    match style & PS_STYLE_MASK {
        PS_DASH => vec![w * 4.0, w * 2.0],
        PS_DOT => vec![w, w],
        PS_DASHDOT => vec![w * 4.0, w * 2.0, w, w * 2.0],
        _ => Vec::new(),
    }
}
