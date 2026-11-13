//! Colour resolution shared across formats: theme tint/shade math (DML),
//! theme-XML hex extraction, VML colour names, and WML highlight colours.
//! Moved verbatim from `lib.rs`.

/// Apply OOXML `themeTint` / `themeShade` to a resolved theme color hex
/// (§17.3.2.6). Both are a single byte 0–255 (already hex-decoded by the
/// parser). Tint lightens toward white; shade darkens toward black. Word
/// applies these in RGB space:
///   tint:  c' = c·t + 255·(1−t)        where t = byte/255
///   shade: c' = c·s                    where s = byte/255
/// `tint` takes precedence if both are somehow present (tint is the common one).
pub(crate) fn apply_tint_shade(hex: &str, tint: Option<&[u8]>, shade: Option<&[u8]>) -> String {
    let parse_rgb = |h: &str| -> Option<(f64, f64, f64)> {
        let h = h.trim_start_matches('#');
        // `len()` is BYTES, but the slices below are byte ranges into a `str`:
        // a 6-BYTE value that is not 6 ASCII chars (e.g. the document-controlled
        // theme1.xml `<a:srgbClr val="aé字"/>`, 6 bytes / 3 chars) would split a
        // UTF-8 sequence and panic. `ST_HexColor` (§17.18.38 / §20.1.10.19) is
        // exactly six ASCII hex digits, so requiring `is_ascii_hexdigit` rejects
        // nothing a conformant document can carry — and every value it rejects
        // would already have failed `from_str_radix` and returned `None` here.
        if h.len() != 6 || !h.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) { return None; }
        let r = u8::from_str_radix(&h[0..2], 16).ok()? as f64;
        let g = u8::from_str_radix(&h[2..4], 16).ok()? as f64;
        let b = u8::from_str_radix(&h[4..6], 16).ok()? as f64;
        Some((r, g, b))
    };
    let (r, g, b) = match parse_rgb(hex) {
        Some(v) => v,
        None => return hex.to_string(),
    };
    let (nr, ng, nb) = if let Some(t) = tint.and_then(|v| v.first()).copied() {
        let t = t as f64 / 255.0;
        (r * t + 255.0 * (1.0 - t), g * t + 255.0 * (1.0 - t), b * t + 255.0 * (1.0 - t))
    } else if let Some(s) = shade.and_then(|v| v.first()).copied() {
        let s = s as f64 / 255.0;
        (r * s, g * s, b * s)
    } else {
        return hex.to_string();
    };
    format!("{:02X}{:02X}{:02X}",
        nr.round().clamp(0.0, 255.0) as u8,
        ng.round().clamp(0.0, 255.0) as u8,
        nb.round().clamp(0.0, 255.0) as u8)
}

pub(crate) fn extract_theme_color_hex(xml_fragment: &str) -> Option<String> {
    // Try srgbClr first
    if let Some(idx) = xml_fragment.find("srgbClr") {
        let after = &xml_fragment[idx..];
        if let Some(vi) = after.find("val=\"") {
            let val_start = vi + "val=\"".len();
            if let Some(val_end) = after[val_start..].find('"') {
                return Some(after[val_start..val_start + val_end].to_uppercase());
            }
        }
    }
    // Try sysClr with lastClr
    if let Some(idx) = xml_fragment.find("sysClr") {
        let after = &xml_fragment[idx..];
        if let Some(li) = after.find("lastClr=\"") {
            let val_start = li + "lastClr=\"".len();
            if let Some(val_end) = after[val_start..].find('"') {
                return Some(after[val_start..val_start + val_end].to_uppercase());
            }
        }
    }
    None
}

/// One DrawingML colour transform (§20.1.2.3) we apply to a resolved theme
/// colour: luminance/saturation modulate+offset, DML tint/shade (toward
/// white/black), and alpha. Percentages arrive as `ST_Percentage` /
/// `ST_PositiveFixedPercentage` *strings* (e.g. `"60000"` = 60 %, or `"60%"`),
/// so [`parse_dml_pct`] normalises both. Hue/red/green/blue/gamma/comp/inv are
/// intentionally omitted (rare on themed fills).
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg(feature = "pml")]
pub(crate) enum DmlTransform {
    LumMod(f64),
    LumOff(f64),
    SatMod(f64),
    SatOff(f64),
    Tint(f64),
    Shade(f64),
    Alpha(f64),
}

/// Parse a DML `ST_Percentage` / `ST_PositiveFixedPercentage` value (kept as a
/// raw string by the parser) into a 0.0–1.0+ fraction. Accepts both the integer
/// thousandths form (`"60000"` → 0.60) and the trailing-percent form
/// (`"60%"` → 0.60) and a plain decimal-percent fallback. Returns `None` on a
/// non-numeric value.
#[cfg(feature = "pml")]
pub(crate) fn parse_dml_pct(raw: &str) -> Option<f64> {
    let s = raw.trim();
    if let Some(stripped) = s.strip_suffix('%') {
        return stripped
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|v| v / 100.0);
    }
    // Integer thousandths-of-a-percent: val="60000" == 60% == 0.60. Reject
    // non-finite (Rust parses "nan"/"inf" to f64) so a garbage attr yields None.
    s.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .map(|v| v / 100_000.0)
}

/// Convert an `#rrggbb`/`rrggbb` hex string to RGB f64 in 0.0–255.0.
#[cfg(any(feature = "pml", feature = "sml"))]
fn hex_to_rgb(hex: &str) -> Option<(f64, f64, f64)> {
    let h = hex.trim_start_matches('#');
    // ASCII-hex gate before the byte slices: `len()` counts BYTES, so a 6-byte
    // multi-byte value (`"aé字"`) would otherwise slice mid-codepoint and panic.
    // `ST_HexColor` is six ASCII hex digits (§20.1.10.19), so this admits every
    // legitimate colour and rejects only values `from_str_radix` already refused.
    if h.len() != 6 || !h.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()? as f64;
    let g = u8::from_str_radix(&h[2..4], 16).ok()? as f64;
    let b = u8::from_str_radix(&h[4..6], 16).ok()? as f64;
    Some((r, g, b))
}

/// RGB (0–255) → HSL with H in 0–360, S/L in 0.0–1.0.
#[cfg(any(feature = "pml", feature = "sml"))]
fn rgb_to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let (r, g, b) = (r / 255.0, g / 255.0, b / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d.abs() < f64::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < f64::EPSILON {
        ((g - b) / d) % 6.0
    } else if (max - g).abs() < f64::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    let mut h = h * 60.0;
    if h < 0.0 {
        h += 360.0;
    }
    (h, s, l)
}

/// HSL (H 0–360, S/L 0–1) → RGB 0–255.
#[cfg(any(feature = "pml", feature = "sml"))]
fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (f64, f64, f64) {
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    if s.abs() < f64::EPSILON {
        let v = l * 255.0;
        return (v, v, v);
    }
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    ((r1 + m) * 255.0, (g1 + m) * 255.0, (b1 + m) * 255.0)
}

/// Apply a list of DML colour transforms to a base `#rrggbb` (or `rrggbb`) hex,
/// returning the transformed `#rrggbb` plus a resolved alpha in 0.0–1.0
/// (default 1.0 when no `alpha` transform is present). Luminance/saturation
/// modulate+offset run in HSL space (the dominant themed-shape transform);
/// DML tint moves the channel toward white (`c + (255-c)*amt`) and shade scales
/// toward black (`c * amt`) in RGB — deliberately distinct from the WML byte
/// [`apply_tint_shade`] above. A malformed base hex passes through unchanged
/// with alpha 1.0.
#[cfg(feature = "pml")]
pub(crate) fn apply_color_transforms(hex: &str, xs: &[DmlTransform]) -> (String, f64) {
    let Some((mut r, mut g, mut b)) = hex_to_rgb(hex) else {
        return (hex.to_string(), 1.0);
    };
    let mut alpha = 1.0f64;
    // HSL-space transforms (lum*/sat*) accumulate before RGB tint/shade so the
    // luminance math matches PowerPoint's order (modulate then offset).
    let needs_hsl = xs.iter().any(|t| {
        matches!(
            t,
            DmlTransform::LumMod(_)
                | DmlTransform::LumOff(_)
                | DmlTransform::SatMod(_)
                | DmlTransform::SatOff(_)
        )
    });
    if needs_hsl {
        let (h, mut s, mut l) = rgb_to_hsl(r, g, b);
        for t in xs {
            match t {
                DmlTransform::LumMod(m) => l *= m,
                DmlTransform::LumOff(o) => l += o,
                DmlTransform::SatMod(m) => s *= m,
                DmlTransform::SatOff(o) => s += o,
                _ => {}
            }
        }
        let (nr, ng, nb) = hsl_to_rgb(h, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0));
        r = nr;
        g = ng;
        b = nb;
    }
    for t in xs {
        match t {
            DmlTransform::Tint(amt) => {
                // DML tint moves the channel a fraction `amt` toward white.
                let a = amt.clamp(0.0, 1.0);
                r += (255.0 - r) * a;
                g += (255.0 - g) * a;
                b += (255.0 - b) * a;
            }
            DmlTransform::Shade(amt) => {
                let a = amt.clamp(0.0, 1.0);
                r *= a;
                g *= a;
                b *= a;
            }
            DmlTransform::Alpha(a) => alpha = a.clamp(0.0, 1.0),
            _ => {}
        }
    }
    (
        format!(
            "#{:02X}{:02X}{:02X}",
            r.round().clamp(0.0, 255.0) as u8,
            g.round().clamp(0.0, 255.0) as u8,
            b.round().clamp(0.0, 255.0) as u8
        ),
        alpha,
    )
}

/// Shade a base `#rrggbb`/`rrggbb` hex by a lightness MULTIPLIER `shade` (for
/// the cube/can/bevel per-face shading): `shade > 1` lightens the face's
/// luminance toward white, `shade < 1` darkens it toward black, `1.0` returns
/// the base unchanged. The multiply runs on the HSL luminance (hue/saturation
/// preserved), so a brown cube top reads as a lighter brown — not a desaturated
/// grey. Returns `#RRGGBB`; a malformed base hex passes through unchanged.
#[cfg(feature = "pml")]
pub(crate) fn shade_hex(hex: &str, shade: f64) -> String {
    let Some((r, g, b)) = hex_to_rgb(hex) else {
        return hex.to_string();
    };
    let fmt = |r: f64, g: f64, b: f64| {
        format!(
            "#{:02X}{:02X}{:02X}",
            r.round().clamp(0.0, 255.0) as u8,
            g.round().clamp(0.0, 255.0) as u8,
            b.round().clamp(0.0, 255.0) as u8
        )
    };
    if shade == 1.0 || !shade.is_finite() {
        return fmt(r, g, b);
    }
    let (h, s, l) = rgb_to_hsl(r, g, b);
    let (r, g, b) = hsl_to_rgb(h, s, (l * shade).clamp(0.0, 1.0));
    fmt(r, g, b)
}

/// Apply an Excel `@tint` to a hex colour per §18.8.19 (SpreadsheetML tint —
/// distinct from BOTH the WML byte [`apply_tint_shade`] and the DML percentage
/// [`DmlTransform::Tint`]). The tint operates on the colour's HLS LUMINANCE:
///   tint < 0:  L' = L · (1 + tint)                  (darken toward black)
///   tint > 0:  L' = L · (1 − tint) + tint           (lighten toward white)
/// with hue/saturation preserved. Accepts `RRGGBB`/`#rrggbb`, returns
/// `#RRGGBB`. The spec's worked algorithm uses integer HLS (HLSMAX=255); we run
/// the same luminance formula in continuous HSL, which matches Excel to within
/// ±1/255 per channel (e.g. accent1 4472C4 @ tint 0.6 → #B4C7E7, Excel's
/// "Lighter 60 %" swatch exactly). A malformed hex passes through unchanged.
#[cfg(feature = "sml")]
pub(crate) fn apply_excel_tint(hex: &str, tint: f64) -> String {
    let Some((r, g, b)) = hex_to_rgb(hex) else {
        return hex.to_string();
    };
    let fmt = |r: f64, g: f64, b: f64| {
        format!(
            "#{:02X}{:02X}{:02X}",
            r.round().clamp(0.0, 255.0) as u8,
            g.round().clamp(0.0, 255.0) as u8,
            b.round().clamp(0.0, 255.0) as u8
        )
    };
    if tint == 0.0 || !tint.is_finite() {
        return fmt(r, g, b);
    }
    let (h, s, l) = rgb_to_hsl(r, g, b);
    let t = tint.clamp(-1.0, 1.0);
    let l = if t < 0.0 { l * (1.0 + t) } else { l * (1.0 - t) + t };
    let (r, g, b) = hsl_to_rgb(h, s, l.clamp(0.0, 1.0));
    fmt(r, g, b)
}

/// VML `fillcolor` → CSS. Accepts `#RRGGBB`, bare `RRGGBB`, and the handful of
/// VML named colours Word emits for watermarks. Unknown names pass through (the
/// browser resolves CSS colour keywords).
pub(crate) fn vml_color_to_css(s: &str) -> String {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return format!("#{hex}");
    }
    if s.len() == 6 && s.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
        return format!("#{s}");
    }
    s.to_string()
}

/// OOXML hex colour attribute (`ST_HexColor`: a fill/shading/border `val`,
/// `fill`, or border `color`) → CSS `#RRGGBB`. Returns `None` for the sentinel
/// `"auto"` (let-the-consumer-decide) and for any value that is not pure ASCII
/// hex — matching the stricter run-colour path (in lib.rs), which relies on
/// the hex check to also reject `"auto"`. Callers that need a default colour
/// (e.g. borders defaulting to black) apply their own `unwrap_or_else`.
pub(crate) fn ooxml_color_to_css(raw: &str) -> Option<String> {
    if raw == "auto" {
        return None;
    }
    if !raw.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{raw}"))
}

pub(crate) fn highlight_color_to_css(color: ooxml_wml::types::STHighlightColor) -> Option<String> {
    use ooxml_wml::types::STHighlightColor::*;
    let css = match color {
        Yellow => "#ffff00",
        Green => "#00ff00",
        Cyan => "#00ffff",
        Magenta => "#ff00ff",
        Blue => "#0000ff",
        Red => "#ff0000",
        DarkBlue => "#000080",
        DarkCyan => "#008080",
        DarkGreen => "#008000",
        DarkMagenta => "#800080",
        DarkRed => "#800000",
        DarkYellow => "#808000",
        DarkGray => "#808080",
        LightGray => "#c0c0c0",
        Black => "#000000",
        White => "#ffffff",
        None => return Option::None,
    };
    Some(css.to_string())
}

#[cfg(test)]
mod tint_shade_tests {
    use super::apply_tint_shade;

    #[test]
    fn no_modifier_returns_base() {
        assert_eq!(apply_tint_shade("FF0000", None, None), "FF0000");
    }

    #[test]
    fn full_tint_is_white() {
        // tint=255 → t=1.0 → c·1 + 255·0 = base (no lightening at full)
        assert_eq!(apply_tint_shade("000000", Some(&[255]), None), "000000");
        // tint=0 → t=0 → fully white
        assert_eq!(apply_tint_shade("000000", Some(&[0]), None), "FFFFFF");
    }

    #[test]
    fn half_tint_lightens_halfway() {
        // black, t=128/255≈0.502 → 0·t + 255·(1-t) ≈ 127
        assert_eq!(apply_tint_shade("000000", Some(&[128]), None), "7F7F7F");
    }

    #[test]
    fn shade_darkens() {
        // white, shade=128 → s≈0.502 → 255·s ≈ 128
        assert_eq!(apply_tint_shade("FFFFFF", None, Some(&[128])), "808080");
        // full shade=0 → black
        assert_eq!(apply_tint_shade("FFFFFF", None, Some(&[0])), "000000");
    }

    #[test]
    fn tint_applied_per_channel() {
        // accent blue 4472C4, tint 153 (0x99 ≈ 0.6): lightened blue
        let out = apply_tint_shade("4472C4", Some(&[153]), None);
        // R: 68·0.6+255·0.4=142.8→143(8F); G:114·0.6+102=170.4→170(AA); B:196·0.6+102=219.6→220(DC)
        assert_eq!(out, "8FAADC");
    }

    #[test]
    fn malformed_hex_passthrough() {
        assert_eq!(apply_tint_shade("xyz", Some(&[128]), None), "xyz");
    }

    /// A theme1.xml `<a:srgbClr val="aé字"/>` is 6 BYTES but 3 CHARS, so
    /// the old `h.len() != 6` byte-length gate let it through to `&h[0..2]`,
    /// which panics ("not a char boundary"). Must pass through unchanged.
    #[test]
    fn multibyte_six_byte_hex_does_not_panic() {
        let bad = "aé字"; // 1 + 2 + 3 = 6 bytes, 3 chars
        assert_eq!(bad.len(), 6);
        assert_eq!(bad.chars().count(), 3);
        assert_eq!(apply_tint_shade(bad, Some(&[153]), None), bad);
        assert_eq!(apply_tint_shade(bad, None, Some(&[128])), bad);
        // Uppercased by the theme scan (engine::fonts) before it reaches here.
        assert_eq!(apply_tint_shade("AÉ字", Some(&[153]), None), "AÉ字");
        // …and the `#`-prefixed spelling, which trims to the same 6 bytes.
        assert_eq!(apply_tint_shade("#aé字", Some(&[153]), None), "#aé字");
    }
}

#[cfg(all(test, feature = "pml"))]
mod dml_transform_tests {
    use super::{apply_color_transforms, parse_dml_pct, DmlTransform};

    #[test]
    fn pct_parses_thousandths_and_percent() {
        assert!((parse_dml_pct("60000").unwrap() - 0.60).abs() < 1e-9);
        assert!((parse_dml_pct("40%").unwrap() - 0.40).abs() < 1e-9);
        assert!((parse_dml_pct("100000").unwrap() - 1.00).abs() < 1e-9);
        assert_eq!(parse_dml_pct("nan"), None);
    }

    #[test]
    fn no_transform_returns_base() {
        let (hex, a) = apply_color_transforms("#4472C4", &[]);
        assert_eq!(hex, "#4472C4");
        assert!((a - 1.0).abs() < 1e-9);
    }

    #[test]
    fn lummod_lumoff_lightens() {
        // accent1 4472C4 with lumMod 60% + lumOff 40% is PowerPoint's "lighter
        // 40%" — luminance scaled then offset up; result is a markedly lighter
        // blue (this is the dominant themed-shape transform).
        let (hex, _) = apply_color_transforms(
            "#4472C4",
            &[DmlTransform::LumMod(0.60), DmlTransform::LumOff(0.40)],
        );
        assert_eq!(hex, "#8FAADC");
    }

    #[test]
    fn lummod_darkens() {
        // lumMod 50% halves luminance → darker accent (PowerPoint "darker 25%"
        // family). Just assert it got darker than the base.
        let (hex, _) = apply_color_transforms("#4472C4", &[DmlTransform::LumMod(0.50)]);
        assert_eq!(hex, "#203864");
    }

    #[test]
    fn shade_scales_toward_black() {
        // shade 15% (val="15000") on accent1 → 15% of each channel.
        let (hex, _) = apply_color_transforms("#4472C4", &[DmlTransform::Shade(0.15)]);
        // 0x44*0.15=10(0A), 0x72*0.15=17(11), 0xC4*0.15=29(1D)
        assert_eq!(hex, "#0A111D");
    }

    #[test]
    fn tint_moves_toward_white() {
        // tint 40% → each channel 40% of the way to 255.
        let (hex, _) = apply_color_transforms("#000000", &[DmlTransform::Tint(0.40)]);
        assert_eq!(hex, "#666666");
    }

    #[test]
    fn alpha_is_captured() {
        let (_, a) = apply_color_transforms("#4472C4", &[DmlTransform::Alpha(0.50)]);
        assert!((a - 0.50).abs() < 1e-9);
    }

    #[test]
    fn malformed_hex_passthrough() {
        let (hex, a) = apply_color_transforms("xyz", &[DmlTransform::LumMod(0.5)]);
        assert_eq!(hex, "xyz");
        assert!((a - 1.0).abs() < 1e-9);
    }

    /// `hex_to_rgb`'s old byte-length gate let a 6-BYTE / 3-CHAR value reach
    /// `&h[0..2]`, panicking on the char boundary. Must pass through instead.
    #[test]
    fn multibyte_six_byte_hex_does_not_panic() {
        let (hex, a) = apply_color_transforms("aé字", &[DmlTransform::LumMod(0.5)]);
        assert_eq!(hex, "aé字");
        assert!((a - 1.0).abs() < 1e-9);
        assert_eq!(super::shade_hex("aé字", 1.4), "aé字");
    }
}

#[cfg(all(test, feature = "pml"))]
mod shade_hex_tests {
    use super::shade_hex;

    fn lum(hex: &str) -> i32 {
        let h = hex.trim_start_matches('#');
        let r = u8::from_str_radix(&h[0..2], 16).unwrap() as i32;
        let g = u8::from_str_radix(&h[2..4], 16).unwrap() as i32;
        let b = u8::from_str_radix(&h[4..6], 16).unwrap() as i32;
        r + g + b
    }

    #[test]
    fn unit_shade_returns_base_normalized() {
        assert_eq!(shade_hex("C0712E", 1.0), "#C0712E");
        assert_eq!(shade_hex("#c0712e", 1.0), "#C0712E");
    }

    #[test]
    fn cube_face_ordering_lighter_base_darker() {
        // The cube's three faces over a brown base (#C0712E): the lit TOP face
        // (shade 1.25) must be lighter than the base front (1.0), which must be
        // lighter than the shaded RIGHT face (0.75).
        let base = "C0712E";
        let top = shade_hex(base, 1.25);
        let front = shade_hex(base, 1.0);
        let right = shade_hex(base, 0.75);
        assert!(
            lum(&top) > lum(&front) && lum(&front) > lum(&right),
            "lighter>base>darker: top={top} front={front} right={right}"
        );
    }

    #[test]
    fn shade_preserves_hue_not_grey() {
        // Lightening a brown stays brown (R dominates), not a desaturated grey.
        let top = shade_hex("C0712E", 1.25);
        let h = top.trim_start_matches('#');
        let r = u8::from_str_radix(&h[0..2], 16).unwrap();
        let b = u8::from_str_radix(&h[4..6], 16).unwrap();
        assert!(r > b, "warm hue preserved: {top}");
    }

    #[test]
    fn malformed_hex_passthrough() {
        assert_eq!(shade_hex("xyz", 1.25), "xyz");
    }
}

#[cfg(all(test, feature = "sml"))]
mod excel_tint_tests {
    use super::apply_excel_tint;

    #[test]
    fn zero_tint_normalizes_only() {
        // No tint → the colour passes through, normalised to `#RRGGBB`.
        assert_eq!(apply_excel_tint("4472C4", 0.0), "#4472C4");
        assert_eq!(apply_excel_tint("#4472c4", 0.0), "#4472C4");
    }

    #[test]
    fn positive_tint_matches_excel_lighter_60() {
        // Excel's "Accent 1, Lighter 60 %" swatch: 4472C4 @ tint 0.6 = B4C7E7
        // (Excel writes tint="0.5999…"; the HSL-luminance math lands exactly).
        assert_eq!(apply_excel_tint("4472C4", 0.6), "#B4C7E7");
    }

    #[test]
    fn negative_tint_darkens_luminance() {
        // "Accent 1, Darker 25 %": L' = L·0.75. Continuous HSL puts this at
        // #2F5597 (Excel's integer-HLS rounding reads 2F5496 — ±1/channel).
        assert_eq!(apply_excel_tint("4472C4", -0.25), "#2F5597");
    }

    #[test]
    fn tint_on_black_and_white_extremes() {
        // Black + tint t → pure grey L=t (hue/sat zero).
        assert_eq!(apply_excel_tint("000000", 0.5), "#808080");
        assert_eq!(apply_excel_tint("000000", 1.0), "#FFFFFF");
        // White + negative tint → grey L=1+t.
        assert_eq!(apply_excel_tint("FFFFFF", -0.5), "#808080");
        assert_eq!(apply_excel_tint("FFFFFF", -1.0), "#000000");
    }

    #[test]
    fn tint_steps_are_monotone() {
        // The swatch board's graduated ramp: successive tints must DIFFER and
        // get monotonically lighter (the swatch board samples this ramp).
        let steps = [0.8, 0.6, 0.4, 0.2, 0.0];
        let lum = |hex: &str| {
            let h = hex.trim_start_matches('#');
            let r = u8::from_str_radix(&h[0..2], 16).unwrap() as i32;
            let g = u8::from_str_radix(&h[2..4], 16).unwrap() as i32;
            let b = u8::from_str_radix(&h[4..6], 16).unwrap() as i32;
            r + g + b
        };
        let ramp: Vec<i32> = steps.iter().map(|t| lum(&apply_excel_tint("ED7D31", *t))).collect();
        for w in ramp.windows(2) {
            assert!(w[0] > w[1], "tint ramp must strictly lighten: {ramp:?}");
        }
    }

    #[test]
    fn malformed_hex_passthrough() {
        assert_eq!(apply_excel_tint("xyz", 0.5), "xyz");
    }

    /// 6 BYTES, 3 CHARS — the old byte-length gate sliced mid-codepoint.
    #[test]
    fn multibyte_six_byte_hex_does_not_panic() {
        assert_eq!(apply_excel_tint("aé字", 0.5), "aé字");
    }
}

#[cfg(test)]
mod ooxml_color_tests {
    use super::ooxml_color_to_css;

    #[test]
    fn auto_is_none() {
        assert_eq!(ooxml_color_to_css("auto"), None);
    }

    #[test]
    fn valid_hex_is_prefixed() {
        assert_eq!(ooxml_color_to_css("FF0000").as_deref(), Some("#FF0000"));
        assert_eq!(ooxml_color_to_css("4472c4").as_deref(), Some("#4472c4"));
    }

    #[test]
    fn non_hex_is_rejected() {
        // matches the run-colour path: anything with non-hex bytes → None
        assert_eq!(ooxml_color_to_css("red"), None);
        assert_eq!(ooxml_color_to_css("#FF0000"), None); // a '#' is not hex
    }
}
