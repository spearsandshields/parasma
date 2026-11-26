//! Colour resolution shared across formats: the DrawingML colour models and
//! transforms (one resolver for every host), WML theme tint/shade, Excel tint,
//! theme-XML hex extraction, VML colour names, and WML highlight colours.

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

// ─── DrawingML colour (§20.1.2.3) ────────────────────────────────────────────

/// One DrawingML colour transform (§20.1.2.3.1-34). A colour element's
/// transforms apply one after another in document order, each to the result of
/// the one before. Percentages are fractions (`0.5` = 50 %); angles are degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum DmlTransform {
    Tint(f64),
    Shade(f64),
    Comp,
    Inv,
    Gray,
    Alpha(f64),
    AlphaOff(f64),
    AlphaMod(f64),
    Hue(f64),
    HueOff(f64),
    HueMod(f64),
    Sat(f64),
    SatOff(f64),
    SatMod(f64),
    Lum(f64),
    LumOff(f64),
    LumMod(f64),
    Red(f64),
    RedOff(f64),
    RedMod(f64),
    Green(f64),
    GreenOff(f64),
    GreenMod(f64),
    Blue(f64),
    BlueOff(f64),
    BlueMod(f64),
    Gamma,
    InvGamma,
}

impl DmlTransform {
    /// The transform an element with local name `name` and `val` attribute
    /// stands for, or `None` when the name is not a transform or its value
    /// does not parse. Angles (`hue`, `hueOff`) are in 60,000ths of a degree
    /// (§20.1.10.3, §20.1.10.44); everything else is a percentage.
    pub(crate) fn parse(name: &str, val: Option<&str>) -> Option<Self> {
        use DmlTransform as T;
        let pct = || val.and_then(parse_dml_pct);
        let deg = || {
            val.and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite())
                .map(|v| v / 60_000.0)
        };
        Some(match name {
            "tint" => T::Tint(pct()?),
            "shade" => T::Shade(pct()?),
            "comp" => T::Comp,
            "inv" => T::Inv,
            "gray" => T::Gray,
            "alpha" => T::Alpha(pct()?),
            "alphaOff" => T::AlphaOff(pct()?),
            "alphaMod" => T::AlphaMod(pct()?),
            "hue" => T::Hue(deg()?),
            "hueOff" => T::HueOff(deg()?),
            "hueMod" => T::HueMod(pct()?),
            "sat" => T::Sat(pct()?),
            "satOff" => T::SatOff(pct()?),
            "satMod" => T::SatMod(pct()?),
            "lum" => T::Lum(pct()?),
            "lumOff" => T::LumOff(pct()?),
            "lumMod" => T::LumMod(pct()?),
            "red" => T::Red(pct()?),
            "redOff" => T::RedOff(pct()?),
            "redMod" => T::RedMod(pct()?),
            "green" => T::Green(pct()?),
            "greenOff" => T::GreenOff(pct()?),
            "greenMod" => T::GreenMod(pct()?),
            "blue" => T::Blue(pct()?),
            "blueOff" => T::BlueOff(pct()?),
            "blueMod" => T::BlueMod(pct()?),
            "gamma" => T::Gamma,
            "invGamma" => T::InvGamma,
            _ => return None,
        })
    }

    /// [`DmlTransform::parse`] for a parsed transform.
    pub(crate) fn from_typed(t: &ooxml_dml::types::EGColorTransform) -> Option<Self> {
        use ooxml_dml::types::EGColorTransform as E;
        let (name, val) = match t {
            E::Tint(p) => ("tint", p.value.clone()),
            E::Shade(p) => ("shade", p.value.clone()),
            E::Comp(_) => ("comp", String::new()),
            E::Inv(_) => ("inv", String::new()),
            E::Gray(_) => ("gray", String::new()),
            E::Alpha(p) => ("alpha", p.value.clone()),
            E::AlphaOff(p) => ("alphaOff", p.value.clone()),
            E::AlphaMod(p) => ("alphaMod", p.value.clone()),
            E::Hue(p) => ("hue", p.value.to_string()),
            E::HueOff(p) => ("hueOff", p.value.to_string()),
            E::HueMod(p) => ("hueMod", p.value.clone()),
            E::Sat(p) => ("sat", p.value.clone()),
            E::SatOff(p) => ("satOff", p.value.clone()),
            E::SatMod(p) => ("satMod", p.value.clone()),
            E::Lum(p) => ("lum", p.value.clone()),
            E::LumOff(p) => ("lumOff", p.value.clone()),
            E::LumMod(p) => ("lumMod", p.value.clone()),
            E::Red(p) => ("red", p.value.clone()),
            E::RedOff(p) => ("redOff", p.value.clone()),
            E::RedMod(p) => ("redMod", p.value.clone()),
            E::Green(p) => ("green", p.value.clone()),
            E::GreenOff(p) => ("greenOff", p.value.clone()),
            E::GreenMod(p) => ("greenMod", p.value.clone()),
            E::Blue(p) => ("blue", p.value.clone()),
            E::BlueOff(p) => ("blueOff", p.value.clone()),
            E::BlueMod(p) => ("blueMod", p.value.clone()),
            E::Gamma(_) => ("gamma", String::new()),
            E::InvGamma(_) => ("invGamma", String::new()),
        };
        Self::parse(name, Some(&val))
    }
}

/// Parse a DML `ST_Percentage` / `ST_PositiveFixedPercentage` value (kept as a
/// raw string by the parser) into a 0.0–1.0+ fraction. Accepts both the integer
/// thousandths form (`"60000"` → 0.60) and the trailing-percent form
/// (`"60%"` → 0.60) and a plain decimal-percent fallback. Returns `None` on a
/// non-numeric value.
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

fn rgb_hex(r: f64, g: f64, b: f64) -> String {
    let ch = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    format!("#{:02X}{:02X}{:02X}", ch(r), ch(g), ch(b))
}

/// sRGB-encoded channel (0–1) to linear light (IEC 61966-2-1).
fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// Linear-light channel (0–1) to sRGB encoding.
fn linear_to_srgb(v: f64) -> f64 {
    if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// RGB (0–255) → HSL with H in 0–360, S/L in 0.0–1.0.
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

/// Apply DrawingML colour transforms (§20.1.2.3) to a base `#rrggbb` (or
/// `rrggbb`) hex, in order, returning `#RRGGBB` and an alpha in 0.0–1.0
/// (1.0 unless an alpha transform says otherwise). No transforms, or a
/// malformed base, return the base unchanged with alpha 1.0.
///
/// The colour space each transform works in follows the spec's own worked
/// examples:
/// - `tint` and `shade` mix with white or black in linear light: "a 10% tint
///   is 10% of the input color combined with 90% white" (§20.1.2.3.34), and
///   tint 50% on `00FF00` gives `BCFFBC`.
/// - `hue`, `sat` and `lum` and their offsets and modulations work in HSL of
///   the sRGB values (`lumOff -20%` on `00FF00` gives `009900`), as do
///   `red`/`green`/`blue` and theirs (`redMod 50%` on `FF0000` gives
///   `800000`), `comp` and `inv`.
/// - `gamma` and `invGamma` apply and remove the sRGB curve.
/// - `gray` weighs the sRGB values by ITU-R BT.601 luma.
///
/// Every component is clamped after each step (0-100 %, hue 0-360°), as the
/// element descriptions require.
pub(crate) fn apply_color_transforms(hex: &str, xs: &[DmlTransform]) -> (String, f64) {
    let Some((r, g, b)) = hex_to_rgb(hex) else {
        return (hex.to_string(), 1.0);
    };
    if xs.is_empty() {
        return (hex.to_string(), 1.0);
    }
    use DmlTransform as T;
    // sRGB channels in 0–1.
    let mut c = [r / 255.0, g / 255.0, b / 255.0];
    let mut alpha = 1.0f64;
    let unit = |v: f64| v.clamp(0.0, 1.0);
    let with_hsl = |c: &mut [f64; 3], f: &dyn Fn(&mut f64, &mut f64, &mut f64)| {
        let (mut h, mut s, mut l) = rgb_to_hsl(c[0] * 255.0, c[1] * 255.0, c[2] * 255.0);
        f(&mut h, &mut s, &mut l);
        let (r, g, b) = hsl_to_rgb(h.clamp(0.0, 360.0), s.clamp(0.0, 1.0), l.clamp(0.0, 1.0));
        *c = [r / 255.0, g / 255.0, b / 255.0];
    };
    for t in xs {
        match *t {
            T::Tint(v) => {
                let v = unit(v);
                c = c.map(|x| linear_to_srgb(srgb_to_linear(x) * v + (1.0 - v)));
            }
            T::Shade(v) => {
                let v = unit(v);
                c = c.map(|x| linear_to_srgb(srgb_to_linear(x) * v));
            }
            T::Comp => {
                let (max, min) = (c[0].max(c[1]).max(c[2]), c[0].min(c[1]).min(c[2]));
                c = c.map(|x| max + min - x);
            }
            T::Inv => c = c.map(|x| 1.0 - x),
            T::Gray => {
                let y = 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
                c = [y, y, y];
            }
            T::Alpha(v) => alpha = unit(v),
            T::AlphaOff(v) => alpha = unit(alpha + v),
            T::AlphaMod(v) => alpha = unit(alpha * v),
            T::Hue(v) => with_hsl(&mut c, &|h, _, _| *h = v),
            T::HueOff(v) => with_hsl(&mut c, &|h, _, _| *h += v),
            T::HueMod(v) => with_hsl(&mut c, &|h, _, _| *h *= v),
            T::Sat(v) => with_hsl(&mut c, &|_, s, _| *s = v),
            T::SatOff(v) => with_hsl(&mut c, &|_, s, _| *s += v),
            T::SatMod(v) => with_hsl(&mut c, &|_, s, _| *s *= v),
            T::Lum(v) => with_hsl(&mut c, &|_, _, l| *l = v),
            T::LumOff(v) => with_hsl(&mut c, &|_, _, l| *l += v),
            T::LumMod(v) => with_hsl(&mut c, &|_, _, l| *l *= v),
            T::Red(v) => c[0] = unit(v),
            T::RedOff(v) => c[0] = unit(c[0] + v),
            T::RedMod(v) => c[0] = unit(c[0] * v),
            T::Green(v) => c[1] = unit(v),
            T::GreenOff(v) => c[1] = unit(c[1] + v),
            T::GreenMod(v) => c[1] = unit(c[1] * v),
            T::Blue(v) => c[2] = unit(v),
            T::BlueOff(v) => c[2] = unit(c[2] + v),
            T::BlueMod(v) => c[2] = unit(c[2] * v),
            T::Gamma => c = c.map(linear_to_srgb),
            T::InvGamma => c = c.map(srgb_to_linear),
        }
    }
    (rgb_hex(c[0] * 255.0, c[1] * 255.0, c[2] * 255.0), alpha)
}

/// A resolved colour as CSS: the `#rrggbb` itself when opaque, else
/// `rgba(r,g,b,a)`.
pub(crate) fn css_color(hex: &str, alpha: f64) -> String {
    if alpha >= 1.0 {
        return hex.to_string();
    }
    match hex_to_rgb(hex) {
        Some((r, g, b)) => format!("rgba({},{},{},{:.3})", r as u8, g as u8, b as u8, alpha.max(0.0)),
        None => hex.to_string(),
    }
}

/// The local names of the DrawingML colour elements (§20.1.2.3).
pub(crate) const DML_COLOR_MODELS: [&str; 6] = ["srgbClr", "schemeClr", "scrgbClr", "hslClr", "prstClr", "sysClr"];

/// The base colour of a DrawingML colour element other than `schemeClr`
/// (which needs the host's theme), as `#RRGGBB` (§20.1.2.3): `srgbClr@val`,
/// `scrgbClr` (linear-light percentages, gamma 1.0, so 50 % is `BC`),
/// `hslClr` (hue in 60,000ths of a degree), `prstClr` (§20.1.10.48) and
/// `sysClr` (`lastClr`, else [`system_color`]). `attr` reads the element's
/// attributes.
pub(crate) fn dml_base_color(model: &str, attr: impl Fn(&str) -> Option<String>) -> Option<String> {
    match model {
        "srgbClr" => {
            let v = attr("val")?;
            hex_to_rgb(&v).map(|(r, g, b)| rgb_hex(r, g, b))
        }
        "scrgbClr" => {
            let ch = |n| linear_to_srgb(attr(n).as_deref().and_then(parse_dml_pct).unwrap_or(0.0).clamp(0.0, 1.0)) * 255.0;
            Some(rgb_hex(ch("r"), ch("g"), ch("b")))
        }
        "hslClr" => {
            let hue = attr("hue").and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite())? / 60_000.0;
            let pct = |n| attr(n).as_deref().and_then(parse_dml_pct).unwrap_or(0.0);
            let (r, g, b) = hsl_to_rgb(hue, pct("sat"), pct("lum"));
            Some(rgb_hex(r, g, b))
        }
        "prstClr" => attr("val").and_then(|v| preset_color(&v)).map(|h| format!("#{h}")),
        "sysClr" => attr("lastClr")
            .and_then(|v| hex_to_rgb(&v).map(|(r, g, b)| rgb_hex(r, g, b)))
            .or_else(|| attr("val").and_then(|v| system_color(&v)).map(|h| format!("#{h}"))),
        _ => None,
    }
}

/// Resolve a parsed colour choice: its base colour (a `schemeClr` through
/// `scheme`, which applies the host's theme and colour map) with its
/// transforms applied. Returns `#rrggbb` and alpha; see
/// [`apply_color_transforms`].
pub(crate) fn resolve_color_choice(
    choice: &ooxml_dml::types::EGColorChoice,
    scheme: impl FnOnce(&ooxml_dml::types::STSchemeColorVal) -> Option<String>,
) -> Option<(String, f64)> {
    use ooxml_dml::types::EGColorChoice as C;
    let hex3 = |v: &[u8]| (v.len() == 3).then(|| format!("#{:02x}{:02x}{:02x}", v[0], v[1], v[2]));
    let (base, xs) = match choice {
        C::SrgbClr(c) => (hex3(&c.value)?, &c.color_transform),
        C::SchemeClr(c) => (scheme(&c.value)?, &c.color_transform),
        C::ScrgbClr(c) => {
            let attr = |n: &str| Some(match n { "r" => c.relationship_id.clone(), "g" => c.g.clone(), _ => c.b.clone() });
            (dml_base_color("scrgbClr", attr)?, &c.color_transform)
        }
        C::HslClr(c) => {
            let attr = |n: &str| Some(match n { "hue" => c.hue.to_string(), "sat" => c.sat.clone(), _ => c.lum.clone() });
            (dml_base_color("hslClr", attr)?, &c.color_transform)
        }
        C::SysClr(c) => {
            let last = c.last_clr.as_deref().and_then(hex3);
            let base = last.or_else(|| system_color(&c.value.to_string()).map(|h| format!("#{h}")))?;
            (base, &c.color_transform)
        }
        C::PrstClr(c) => (preset_color(&c.value.to_string()).map(|h| format!("#{h}"))?, &c.color_transform),
    };
    let xs: Vec<DmlTransform> = xs.iter().filter_map(DmlTransform::from_typed).collect();
    Some(apply_color_transforms(&base, &xs))
}

/// Resolve the first DrawingML colour element in an XML fragment (any of the
/// six colour models, with its transform children in document order), for
/// readers that scan raw part XML. `scheme` resolves a `schemeClr@val` to its
/// base colour.
#[cfg(feature = "pml")]
pub(crate) fn resolve_dml_color_xml(
    fragment: &str,
    scheme: impl FnOnce(&str) -> Option<String>,
) -> Option<(String, f64)> {
    use quick_xml::events::{BytesStart, Event};
    let local = |e: &BytesStart| String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
    let attr = |e: &BytesStart, name: &str| {
        e.attributes()
            .flatten()
            .find(|a| a.key.local_name().as_ref() == name.as_bytes())
            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
    };
    let mut reader = quick_xml::Reader::from_str(fragment);
    let (element, has_children) = loop {
        match reader.read_event().ok()? {
            Event::Start(e) if DML_COLOR_MODELS.contains(&local(&e).as_str()) => break (e.into_owned(), true),
            Event::Empty(e) if DML_COLOR_MODELS.contains(&local(&e).as_str()) => break (e.into_owned(), false),
            Event::Eof => return None,
            _ => {}
        }
    };
    let model = local(&element);
    let base = if model == "schemeClr" {
        scheme(&attr(&element, "val")?)?
    } else {
        dml_base_color(&model, |n| attr(&element, n))?
    };
    let mut xs = Vec::new();
    if has_children {
        let mut depth = 0usize;
        loop {
            match reader.read_event().ok()? {
                Event::Start(e) => {
                    if depth == 0 {
                        xs.extend(DmlTransform::parse(&local(&e), attr(&e, "val").as_deref()));
                    }
                    depth += 1;
                }
                Event::Empty(e) if depth == 0 => {
                    xs.extend(DmlTransform::parse(&local(&e), attr(&e, "val").as_deref()));
                }
                Event::End(_) if depth == 0 => break,
                Event::End(_) => depth -= 1,
                Event::Eof => break,
                _ => {}
            }
        }
    }
    Some(apply_color_transforms(&base, &xs))
}

/// The RGB value of a preset colour name (`ST_PresetColorVal`, §20.1.10.48).
///
/// Declared: the spec gives `ltGoldenrodYellow` as (250,250,120) but its
/// long spelling `lightGoldenrodYellow` as (250,250,210); 210 is used for both.
pub(crate) fn preset_color(name: &str) -> Option<&'static str> {
    PRESET_COLORS.iter().find(|(n, _)| *n == name).map(|(_, hex)| *hex)
}

/// The colour of a system colour name (`ST_SystemColorVal`, §20.1.10.58)
/// when the file has no `lastClr`.
///
/// Declared: system colours are the viewer's operating system's, which the
/// spec does not list; these are Windows' default values.
pub(crate) fn system_color(name: &str) -> Option<&'static str> {
    Some(match name {
        "scrollBar" => "C8C8C8",
        "background" => "000000",
        "activeCaption" => "99B4D1",
        "inactiveCaption" => "BFCDDB",
        "menu" => "F0F0F0",
        "window" => "FFFFFF",
        "windowFrame" => "646464",
        "menuText" => "000000",
        "windowText" => "000000",
        "captionText" => "000000",
        "activeBorder" => "B4B4B4",
        "inactiveBorder" => "F4F7FC",
        "appWorkspace" => "ABABAB",
        "highlight" => "0078D7",
        "highlightText" => "FFFFFF",
        "btnFace" => "F0F0F0",
        "btnShadow" => "A0A0A0",
        "grayText" => "6D6D6D",
        "btnText" => "000000",
        "inactiveCaptionText" => "000000",
        "btnHighlight" => "FFFFFF",
        "3dDkShadow" => "696969",
        "3dLight" => "E3E3E3",
        "infoText" => "000000",
        "infoBk" => "FFFFE1",
        "hotLight" => "0066CC",
        "gradientActiveCaption" => "B9D1EA",
        "gradientInactiveCaption" => "D7E4F2",
        "menuHighlight" => "3399FF",
        "menuBar" => "F0F0F0",
        _ => return None,
    })
}

/// `ST_PresetColorVal` (§20.1.10.48): name → RGB.
const PRESET_COLORS: &[(&str, &str)] = &[
    ("aliceBlue", "F0F8FF"),
    ("antiqueWhite", "FAEBD7"),
    ("aqua", "00FFFF"),
    ("aquamarine", "7FFFD4"),
    ("azure", "F0FFFF"),
    ("beige", "F5F5DC"),
    ("bisque", "FFE4C4"),
    ("black", "000000"),
    ("blanchedAlmond", "FFEBCD"),
    ("blue", "0000FF"),
    ("blueViolet", "8A2BE2"),
    ("brown", "A52A2A"),
    ("burlyWood", "DEB887"),
    ("cadetBlue", "5F9EA0"),
    ("chartreuse", "7FFF00"),
    ("chocolate", "D2691E"),
    ("coral", "FF7F50"),
    ("cornflowerBlue", "6495ED"),
    ("cornsilk", "FFF8DC"),
    ("crimson", "DC143C"),
    ("cyan", "00FFFF"),
    ("darkBlue", "00008B"),
    ("darkCyan", "008B8B"),
    ("darkGoldenrod", "B8860B"),
    ("darkGray", "A9A9A9"),
    ("darkGreen", "006400"),
    ("darkGrey", "A9A9A9"),
    ("darkKhaki", "BDB76B"),
    ("darkMagenta", "8B008B"),
    ("darkOliveGreen", "556B2F"),
    ("darkOrange", "FF8C00"),
    ("darkOrchid", "9932CC"),
    ("darkRed", "8B0000"),
    ("darkSalmon", "E9967A"),
    ("darkSeaGreen", "8FBC8F"),
    ("darkSlateBlue", "483D8B"),
    ("darkSlateGray", "2F4F4F"),
    ("darkSlateGrey", "2F4F4F"),
    ("darkTurquoise", "00CED1"),
    ("darkViolet", "9400D3"),
    ("deepPink", "FF1493"),
    ("deepSkyBlue", "00BFFF"),
    ("dimGray", "696969"),
    ("dimGrey", "696969"),
    ("dkBlue", "00008B"),
    ("dkCyan", "008B8B"),
    ("dkGoldenrod", "B8860B"),
    ("dkGray", "A9A9A9"),
    ("dkGreen", "006400"),
    ("dkGrey", "A9A9A9"),
    ("dkKhaki", "BDB76B"),
    ("dkMagenta", "8B008B"),
    ("dkOliveGreen", "556B2F"),
    ("dkOrange", "FF8C00"),
    ("dkOrchid", "9932CC"),
    ("dkRed", "8B0000"),
    ("dkSalmon", "E9967A"),
    ("dkSeaGreen", "8FBC8B"),
    ("dkSlateBlue", "483D8B"),
    ("dkSlateGray", "2F4F4F"),
    ("dkSlateGrey", "2F4F4F"),
    ("dkTurquoise", "00CED1"),
    ("dkViolet", "9400D3"),
    ("dodgerBlue", "1E90FF"),
    ("firebrick", "B22222"),
    ("floralWhite", "FFFAF0"),
    ("forestGreen", "228B22"),
    ("fuchsia", "FF00FF"),
    ("gainsboro", "DCDCDC"),
    ("ghostWhite", "F8F8FF"),
    ("gold", "FFD700"),
    ("goldenrod", "DAA520"),
    ("gray", "808080"),
    ("green", "008000"),
    ("greenYellow", "ADFF2F"),
    ("grey", "808080"),
    ("honeydew", "F0FFF0"),
    ("hotPink", "FF69B4"),
    ("indianRed", "CD5C5C"),
    ("indigo", "4B0082"),
    ("ivory", "FFFFF0"),
    ("khaki", "F0E68C"),
    ("lavender", "E6E6FA"),
    ("lavenderBlush", "FFF0F5"),
    ("lawnGreen", "7CFC00"),
    ("lemonChiffon", "FFFACD"),
    ("lightBlue", "ADD8E6"),
    ("lightCoral", "F08080"),
    ("lightCyan", "E0FFFF"),
    ("lightGoldenrodYellow", "FAFAD2"),
    ("lightGray", "D3D3D3"),
    ("lightGreen", "90EE90"),
    ("lightGrey", "D3D3D3"),
    ("lightPink", "FFB6C1"),
    ("lightSalmon", "FFA07A"),
    ("lightSeaGreen", "20B2AA"),
    ("lightSkyBlue", "87CEFA"),
    ("lightSlateGray", "778899"),
    ("lightSlateGrey", "778899"),
    ("lightSteelBlue", "B0C4DE"),
    ("lightYellow", "FFFFE0"),
    ("lime", "00FF00"),
    ("limeGreen", "32CD32"),
    ("linen", "FAF0E6"),
    ("ltBlue", "ADD8E6"),
    ("ltCoral", "F08080"),
    ("ltCyan", "E0FFFF"),
    ("ltGoldenrodYellow", "FAFAD2"),
    ("ltGray", "D3D3D3"),
    ("ltGreen", "90EE90"),
    ("ltGrey", "D3D3D3"),
    ("ltPink", "FFB6C1"),
    ("ltSalmon", "FFA07A"),
    ("ltSeaGreen", "20B2AA"),
    ("ltSkyBlue", "87CEFA"),
    ("ltSlateGray", "778899"),
    ("ltSlateGrey", "778899"),
    ("ltSteelBlue", "B0C4DE"),
    ("ltYellow", "FFFFE0"),
    ("magenta", "FF00FF"),
    ("maroon", "800000"),
    ("medAquamarine", "66CDAA"),
    ("medBlue", "0000CD"),
    ("mediumAquamarine", "66CDAA"),
    ("mediumBlue", "0000CD"),
    ("mediumOrchid", "BA55D3"),
    ("mediumPurple", "9370DB"),
    ("mediumSeaGreen", "3CB371"),
    ("mediumSlateBlue", "7B68EE"),
    ("mediumSpringGreen", "00FA9A"),
    ("mediumTurquoise", "48D1CC"),
    ("mediumVioletRed", "C71585"),
    ("medOrchid", "BA55D3"),
    ("medPurple", "9370DB"),
    ("medSeaGreen", "3CB371"),
    ("medSlateBlue", "7B68EE"),
    ("medSpringGreen", "00FA9A"),
    ("medTurquoise", "48D1CC"),
    ("medVioletRed", "C71585"),
    ("midnightBlue", "191970"),
    ("mintCream", "F5FFFA"),
    ("mistyRose", "FFE4E1"),
    ("moccasin", "FFE4B5"),
    ("navajoWhite", "FFDEAD"),
    ("navy", "000080"),
    ("oldLace", "FDF5E6"),
    ("olive", "808000"),
    ("oliveDrab", "6B8E23"),
    ("orange", "FFA500"),
    ("orangeRed", "FF4500"),
    ("orchid", "DA70D6"),
    ("paleGoldenrod", "EEE8AA"),
    ("paleGreen", "98FB98"),
    ("paleTurquoise", "AFEEEE"),
    ("paleVioletRed", "DB7093"),
    ("papayaWhip", "FFEFD5"),
    ("peachPuff", "FFDAB9"),
    ("peru", "CD853F"),
    ("pink", "FFC0CB"),
    ("plum", "DDA0DD"),
    ("powderBlue", "B0E0E6"),
    ("purple", "800080"),
    ("red", "FF0000"),
    ("rosyBrown", "BC8F8F"),
    ("royalBlue", "4169E1"),
    ("saddleBrown", "8B4513"),
    ("salmon", "FA8072"),
    ("sandyBrown", "F4A460"),
    ("seaGreen", "2E8B57"),
    ("seaShell", "FFF5EE"),
    ("sienna", "A0522D"),
    ("silver", "C0C0C0"),
    ("skyBlue", "87CEEB"),
    ("slateBlue", "6A5ACD"),
    ("slateGray", "708090"),
    ("slateGrey", "708090"),
    ("snow", "FFFAFA"),
    ("springGreen", "00FF7F"),
    ("steelBlue", "4682B4"),
    ("tan", "D2B48C"),
    ("teal", "008080"),
    ("thistle", "D8BFD8"),
    ("tomato", "FF6347"),
    ("turquoise", "40E0D0"),
    ("violet", "EE82EE"),
    ("wheat", "F5DEB3"),
    ("white", "FFFFFF"),
    ("whiteSmoke", "F5F5F5"),
    ("yellow", "FFFF00"),
    ("yellowGreen", "9ACD32"),
];

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

#[cfg(test)]
mod dml_transform_tests {
    use super::{apply_color_transforms, dml_base_color, parse_dml_pct, preset_color, DmlTransform};

    /// Apply `(name, val)` transforms the way a document spells them.
    fn tx(base: &str, xs: &[(&str, &str)]) -> (String, f64) {
        let xs: Vec<DmlTransform> = xs.iter().map(|(n, v)| DmlTransform::parse(n, Some(v)).unwrap()).collect();
        apply_color_transforms(base, &xs)
    }
    fn hex(base: &str, xs: &[(&str, &str)]) -> String {
        tx(base, xs).0
    }
    /// Within one step per channel: the spec's examples round their HSL
    /// arithmetic differently in places (0.75 × 255 = 191.25 written as `C0`).
    fn near(got: &str, want: &str) {
        let (a, b) = (super::hex_to_rgb(got).unwrap(), super::hex_to_rgb(want).unwrap());
        let d = (a.0 - b.0).abs().max((a.1 - b.1).abs()).max((a.2 - b.2).abs());
        assert!(d <= 1.0, "{got} vs {want}");
    }

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

    /// §20.1.2.3.34 and §20.1.2.3.31: "a 10% tint is 10% of the input color
    /// combined with 90% white", mixed in linear light.
    #[test]
    fn tint_and_shade_keep_their_share_of_the_input_in_linear_light() {
        assert_eq!(hex("00FF00", &[("tint", "50%")]), "#BCFFBC");
        assert_eq!(hex("00FF00", &[("shade", "50%")]), "#00BC00");
        // PowerPoint's default table bands: accent 1 at tint 20 % and 40 %.
        assert_eq!(hex("4472C4", &[("tint", "20000")]), "#E9EBF5");
        assert_eq!(hex("4472C4", &[("tint", "40000")]), "#CFD5EA");
    }

    #[test]
    fn hsl_transforms_follow_the_spec_examples() {
        near(&hex("00FF00", &[("lumOff", "-20%")]), "009900");
        near(&hex("00FF00", &[("lum", "20%")]), "006600");
        near(&hex("00FF00", &[("sat", "50%")]), "40C040");
        near(&hex("00FF00", &[("satMod", "20%")]), "669966");
        near(&hex("00FF00", &[("satOff", "-20%")]), "19E519");
        near(&hex("00FF00", &[("hueMod", "50%")]), "FFFF00");
        near(&hex("00FF00", &[("hue", "14400000")]), "0000FF");
        // PowerPoint's "Lighter 40%" swatch of accent 1.
        assert_eq!(hex("4472C4", &[("lumMod", "60000"), ("lumOff", "40000")]), "#8FAADC");
    }

    /// §20.1.2.3.16: the shifted hue is clamped to 0-360°, not wrapped.
    #[test]
    fn hue_offset_shifts_and_clamps() {
        near(&hex("FF0000", &[("hueOff", "7200000")]), "00FF00");
        near(&hex("00FF00", &[("hueOff", "18000000")]), "FF0000");
    }

    #[test]
    fn rgb_component_transforms_follow_the_spec_examples() {
        assert_eq!(hex("00FF00", &[("red", "100%")]), "#FFFF00");
        assert_eq!(hex("FF0000", &[("redMod", "50%")]), "#800000");
        assert_eq!(hex("FF0000", &[("redOff", "-20%")]), "#CC0000");
        assert_eq!(hex("0000FF", &[("green", "100%")]), "#00FFFF");
        assert_eq!(hex("00FF00", &[("greenMod", "50%")]), "#008000");
        assert_eq!(hex("00FF00", &[("greenOff", "-20%")]), "#00CC00");
        assert_eq!(hex("00FF00", &[("blue", "100%")]), "#00FFFF");
        assert_eq!(hex("0000FF", &[("blueMod", "50%")]), "#000080");
    }

    #[test]
    fn complement_inverse_and_gray() {
        assert_eq!(hex("FF0000", &[("comp", "")]), "#00FFFF");
        assert_eq!(hex("FF0000", &[("inv", "")]), "#00FFFF");
        // The complement keeps lightness and saturation; the inverse does not.
        assert_eq!(hex("4472C4", &[("comp", "")]), "#C49644");
        assert_eq!(hex("4472C4", &[("inv", "")]), "#BB8D3B");
        let g = hex("4472C4", &[("gray", "")]);
        assert_eq!(&g[1..3], &g[3..5]);
        assert_eq!(&g[3..5], &g[5..7]);
    }

    #[test]
    fn gamma_applies_and_removes_the_srgb_curve() {
        let up = hex("808080", &[("gamma", "")]);
        assert!(up.as_str() > "#808080", "{up}");
        assert_eq!(hex("808080", &[("gamma", ""), ("invGamma", "")]), "#808080");
    }

    /// Transforms apply in document order, each to the previous result.
    #[test]
    fn order_matters() {
        let a = hex("4472C4", &[("tint", "50%"), ("lumMod", "50%")]);
        let b = hex("4472C4", &[("lumMod", "50%"), ("tint", "50%")]);
        assert_ne!(a, b);
    }

    /// §20.1.2.3.1-3: alpha sets, offsets and modulates opacity, clamped.
    #[test]
    fn alpha_sets_offsets_and_modulates() {
        let a = |xs: &[(&str, &str)]| tx("00FF00", xs).1;
        assert!((a(&[("alpha", "50%")]) - 0.5).abs() < 1e-9);
        assert!((a(&[("alphaOff", "-10%")]) - 0.9).abs() < 1e-9);
        assert!((a(&[("alpha", "50%"), ("alphaMod", "50%")]) - 0.25).abs() < 1e-9);
        assert!((a(&[("alphaOff", "10%")]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn colour_models() {
        let attrs = |pairs: &'static [(&'static str, &'static str)]| {
            move |n: &str| pairs.iter().find(|(k, _)| *k == n).map(|(_, v)| v.to_string())
        };
        // §20.1.2.3.30: scrgbClr 50 % grey is srgbClr BCBCBC.
        assert_eq!(dml_base_color("scrgbClr", attrs(&[("r", "50%"), ("g", "50%"), ("b", "50%")])).as_deref(), Some("#BCBCBC"));
        assert_eq!(dml_base_color("hslClr", attrs(&[("hue", "14400000"), ("sat", "100%"), ("lum", "50%")])).as_deref(), Some("#0000FF"));
        assert_eq!(dml_base_color("prstClr", attrs(&[("val", "coral")])).as_deref(), Some("#FF7F50"));
        assert_eq!(dml_base_color("sysClr", attrs(&[("val", "windowText"), ("lastClr", "123456")])).as_deref(), Some("#123456"));
        assert_eq!(dml_base_color("sysClr", attrs(&[("val", "window")])).as_deref(), Some("#FFFFFF"));
        assert_eq!(dml_base_color("schemeClr", attrs(&[("val", "accent1")])), None);
    }

    /// The raw-XML reader keeps the document's transform order.
    #[cfg(feature = "pml")]
    #[test]
    fn xml_fragment_reader_keeps_document_order() {
        let frag = |xs: &str| format!(r#"<a:solidFill xmlns:a="a"><a:srgbClr val="4472C4">{xs}</a:srgbClr></a:solidFill>"#);
        let a = super::resolve_dml_color_xml(&frag(r#"<a:tint val="50000"/><a:lumMod val="50000"/>"#), |_| None).unwrap();
        let b = super::resolve_dml_color_xml(&frag(r#"<a:lumMod val="50000"/><a:tint val="50000"/>"#), |_| None).unwrap();
        assert_eq!(a.0, hex("4472C4", &[("tint", "50000"), ("lumMod", "50000")]));
        assert_eq!(b.0, hex("4472C4", &[("lumMod", "50000"), ("tint", "50000")]));
        let s = super::resolve_dml_color_xml(r#"<a:gs pos="0"><a:schemeClr val="accent1"><a:alpha val="40%"/></a:schemeClr></a:gs>"#, |v| (v == "accent1").then(|| "#4472C4".to_owned())).unwrap();
        assert_eq!(s, ("#4472C4".to_owned(), 0.4));
        assert_eq!(super::resolve_dml_color_xml(r#"<a:prstClr val="red"/>"#, |_| None).unwrap().0, "#FF0000");
    }

    #[test]
    fn preset_table_is_the_full_enumeration() {
        assert_eq!(super::PRESET_COLORS.len(), 190);
        assert_eq!(preset_color("ltGoldenrodYellow"), preset_color("lightGoldenrodYellow"));
        assert_eq!(preset_color("nope"), None);
    }

    #[test]
    fn unknown_transform_names_are_not_transforms() {
        assert_eq!(DmlTransform::parse("lumMod", Some("x")), None);
        assert_eq!(DmlTransform::parse("solidFill", Some("1")), None);
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
        assert_eq!(shade_hex("aé字", 1.4), "aé字");
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
