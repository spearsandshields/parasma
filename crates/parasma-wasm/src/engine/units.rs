//! Measurement-unit conversions (format-agnostic): twips, points, EMU and CSS
//! lengths → CSS pixels at 96 DPI. Moved verbatim from `lib.rs`.

// English Metric Units: 914400 EMU = 1 inch = 96 CSS pixels at 96 DPI.
// 1 EMU = 96 / 914400 = 1 / 9525 px.
pub(crate) const EMU_PER_PX: f64 = 9525.0;

/// Parse an unsigned twips measure (ST_TwipsMeasure, §17.18.87). The impl is
/// deliberately lenient — it accepts any i64 rather than rejecting negatives —
/// so it and `parse_twips_signed` share a body; the two names document which
/// spec type the CALL SITE expects (a positive-only width vs a signed margin),
/// which is why both are kept despite the identical implementation.
pub(crate) fn parse_twips(s: &str) -> Option<i64> {
    s.parse::<i64>().ok()
}
/// Parse a signed twips measure (ST_SignedTwipsMeasure, §17.18.81) — e.g. page
/// margins, which may be negative for overlapping headers/footers. Same lenient
/// body as `parse_twips`.
pub(crate) fn parse_twips_signed(s: &str) -> Option<i64> {
    s.parse::<i64>().ok()
}

pub(crate) fn pt_to_px(pt: f64) -> f64 {
    pt * 4.0 / 3.0
}

pub(crate) fn twips_to_px(twips: i64) -> f64 {
    twips as f64 / 15.0
}

pub(crate) fn emu_to_px(emu: i64) -> f64 {
    emu as f64 / EMU_PER_PX
}

pub(crate) fn css_length_to_px(s: &str) -> f64 {
    let s = s.trim();
    if let Some(num) = s.strip_suffix("pt") {
        num.trim().parse::<f64>().unwrap_or(0.0) * 4.0 / 3.0
    } else if let Some(num) = s.strip_suffix("in") {
        num.trim().parse::<f64>().unwrap_or(0.0) * 96.0
    } else if let Some(num) = s.strip_suffix("cm") {
        num.trim().parse::<f64>().unwrap_or(0.0) * 96.0 / 2.54
    } else if let Some(num) = s.strip_suffix("mm") {
        num.trim().parse::<f64>().unwrap_or(0.0) * 96.0 / 25.4
    } else if let Some(num) = s.strip_suffix("px") {
        num.trim().parse::<f64>().unwrap_or(0.0)
    } else {
        s.parse::<f64>().unwrap_or(0.0) * 4.0 / 3.0
    }
}
