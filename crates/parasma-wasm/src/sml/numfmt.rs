//! Excel number-format interpreter.
//!
//! Turns a cell's raw value (a number serial, a boolean, a string) into its
//! *displayed* text by applying the `numFmt` format code resolved from the
//! cell's `xf` (§18.8.30). Pure + web-sys-free so it natively unit-tests with no
//! wasm-bindgen (as the oracle tests are); the model layer calls
//! [`format_value`] at model-build time and stamps the result onto the cell.
//!
//! Spec grounding (ECMA-376 Part 1, §18 SpreadsheetML):
//!  - §18.8.30   `numFmt` — the built-in ID → formatCode table (IDs 0..49) and
//!    the "General" format. Custom codes (IDs ≥ 164) live in
//!    `<numFmts>`; we resolve custom-first, then the built-in table
//!    (reusing the fork's `builtin_format_code`).
//!  - §18.17.4.x date serial representation — routed through the fork's
//!    `excel_date_to_ymd` / `excel_datetime_to_ymdhms` (the 1900
//!    date system incl. the Lotus 1900-leap-year compatibility
//!    quirk: serial 60 = 1900-02-29; serials > 60 shift by −1). The
//!    fork already has passing tests for this; we REUSE, never
//!    reimplement.
//!  - §18.8.31   `numFmts` — the custom format collection, IDs ≥ 164.
//!
//! SCOPE (fixture-driven; the deferred items are listed below):
//!  - date/time tokens `y m d h s` + `AM/PM` (built-in 14..22, 45..47),
//!  - numeric `0` `#` `,` (thousands) `.` (decimals) `%` (×100, built-in 9/10),
//!  - quoted/escaped literals (`"$"`, `\$`) for currency (custom 164),
//!  - `General` fallback, `@` text passthrough (built-in 49),
//!  - four `;`-sections (positive;negative;zero;text) with a quote/bracket-aware
//!    split (never split on a `;` inside `"…"` / `[…]`); only the
//!    minimum the fixtures need for negatives + text is interpreted.
//!
//! DEFERRED: `[Red]`/color codes, `[h]`/elapsed-time, bracketed
//! conditionals (`[>100]…`), fractions (`# ?/?`), scientific (`0.00E+00`), the
//! `_` width-padding and `*` fill tokens, and `date1904` (the fork helper
//! assumes the 1900 system; all the format fixtures are 1900).

use ooxml_sml::workbook::{builtin_format_code, excel_date_to_ymd, excel_datetime_to_ymdhms};

/// A raw cell value, value-kind tagged, as the model hands it to the formatter.
/// Numbers carry the raw serial (a date is just a number until a date format
/// code interprets it); booleans/errors/strings pass through untouched.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RawValue<'a> {
    Number(f64),
    Boolean(bool),
    /// Shared/inline string or error text — never numeric-formatted; only the
    /// `@` text-section (if present) wraps it.
    Text(&'a str),
}

/// Resolve a cell's effective format code from its `numFmtId`.
///
/// Custom-first (§18.8.31: IDs ≥ 164 live in `<numFmts>`), then the built-in
/// table (§18.8.30 via the fork's `builtin_format_code`). Returns `None` only
/// for an unknown ID with no matching custom entry — the caller then falls back
/// to `General`.
pub(crate) fn resolve_format_code<'a>(
    num_fmt_id: u32,
    custom: impl IntoIterator<Item = (u32, &'a str)>,
) -> Option<String> {
    for (id, code) in custom {
        if id == num_fmt_id {
            return Some(code.to_string());
        }
    }
    // The locale-sensitive built-in date/time IDs (§18.8.30: 14 "short date",
    // 15/16/17 "long-ish date", 22 "datetime") are LOCALE placeholders — Excel
    // substitutes the system locale's pattern, NOT the spec-literal string the
    // fork's table returns (e.g. 14 → "mm-dd-yy"). We render them as the US
    // (en-US) locale shapes, which is what a default Excel install displays and
    // what users expect. SIMPLIFICATION: en-US assumed; true locale resolution
    // (workbook/system locale → date pattern) is deferred.
    let us_locale = match num_fmt_id {
        14 => Some("m/d/yyyy"),
        15 => Some("d-mmm-yy"),
        16 => Some("d-mmm"),
        17 => Some("mmm-yy"),
        22 => Some("m/d/yyyy h:mm"),
        _ => None,
    };
    if let Some(code) = us_locale {
        return Some(code.to_string());
    }
    builtin_format_code(num_fmt_id).map(|s| s.to_string())
}

/// Format a raw value through a format code (§18.8.30). `code = "General"` (or an
/// empty code) routes to the General representation; `@` passes text through.
pub(crate) fn format_value(value: &RawValue, code: &str) -> String {
    let trimmed = code.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("General") {
        return general(value);
    }

    let sections = split_sections(trimmed);

    match *value {
        RawValue::Text(s) => {
            // The 4th (text) section, if present, controls string display; `@`
            // is the verbatim text placeholder. With no text section, strings
            // pass through unformatted (Excel's behavior — numeric sections
            // never apply to text).
            match sections.get(3) {
                Some(sec) => apply_text_section(sec, s),
                None => s.to_string(),
            }
        }
        RawValue::Boolean(b) => {
            // Booleans are not numeric-formatted; display canonically.
            (if b { "TRUE" } else { "FALSE" }).to_string()
        }
        RawValue::Number(n) => format_number_sections(n, &sections),
    }
}

/// The "General" representation (§18.8.30 General Format): integers print
/// without a trailing `.0`, fractionals print terse; strings/bools pass through.
fn general(value: &RawValue) -> String {
    match *value {
        RawValue::Text(s) => s.to_string(),
        RawValue::Boolean(b) => (if b { "TRUE" } else { "FALSE" }).to_string(),
        RawValue::Number(n) => general_number(n),
    }
}

/// General numeric rendering: integers without `.0`; otherwise a terse decimal
/// (Rust's shortest round-trip, which avoids 0.1+0.2 noise tails).
fn general_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Pick + apply the right numeric section for a value's sign (§18.8.30 four
/// sections: positive;negative;zero;text). With fewer sections Excel reuses the
/// positive section for negatives (prefixing `-`) and for zero.
fn format_number_sections(n: f64, sections: &[String]) -> String {
    // Section selection by sign. We only special-case an explicit negative
    // (section[1]) and zero (section[2]) section when present; otherwise the
    // positive section (section[0]) handles all magnitudes and the sign is
    // synthesized.
    let neg = n < 0.0;
    let is_zero = n == 0.0;

    if is_zero {
        if let Some(sec) = sections.get(2).filter(|s| !s.is_empty()) {
            return apply_numeric_section(0.0, sec, false);
        }
        // fall through to positive section with value 0
        return apply_numeric_section(0.0, &sections[0], false);
    }

    if neg {
        if let Some(sec) = sections.get(1).filter(|s| !s.is_empty()) {
            // An explicit negative section formats the MAGNITUDE; the section's
            // own literals (e.g. parentheses) supply the sign decoration.
            return apply_numeric_section(n.abs(), sec, false);
        }
        // No negative section: positive section + a synthesized leading '-'.
        return apply_numeric_section(n.abs(), &sections[0], true);
    }

    apply_numeric_section(n, &sections[0], false)
}

// ─── format-code section split (quote/bracket aware) ───────────────────────────

/// Split a format code into its `;`-separated sections, never splitting on a
/// `;` inside a `"…"` quoted literal, a `[…]` bracket group, or after a `\`
/// escape (bare `;`-splitting is wrong whenever the delimiter can appear
/// inside a quoted literal). Returns 1..=4 sections.
fn split_sections(code: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut cur = String::new();
    let mut chars = code.chars().peekable();
    let mut in_quote = false;
    let mut bracket_depth = 0u32;

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // Escaped literal: keep the backslash + next char verbatim.
                cur.push(c);
                if let Some(&nc) = chars.peek() {
                    cur.push(nc);
                    chars.next();
                }
            }
            '"' => {
                in_quote = !in_quote;
                cur.push(c);
            }
            '[' if !in_quote => {
                bracket_depth += 1;
                cur.push(c);
            }
            ']' if !in_quote => {
                bracket_depth = bracket_depth.saturating_sub(1);
                cur.push(c);
            }
            ';' if !in_quote && bracket_depth == 0 => {
                sections.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    sections.push(cur);
    sections
}

// ─── numeric section interpreter ──────────────────────────────────────────────

/// The static shape of a numeric format section after a one-pass scan: the
/// integer/decimal digit-placeholder counts, whether thousands grouping is on,
/// the ×100 percent scaling, and the ordered literal/placeholder token stream
/// used to reconstruct the output around the formatted number.
#[derive(Debug, Default)]
struct NumericSpec {
    int_zeros: usize,    // count of `0` before the decimal point
    int_hashes: usize,   // count of `#` before the decimal point
    dec_places: usize,   // count of `0`/`#` after the decimal point
    thousands: bool,     // a `,` grouping comma is present
    percent: bool,       // a `%` is present → ×100
    tokens: Vec<Token>,  // ordered output template
}

/// One token in the output template: a literal run, the integer-number slot, or
/// the decimal-number slot (the `.frac` part). The number is split into its
/// integer and fractional renderings so leading currency literals land before
/// the digits and a trailing `%`/`)` lands after.
#[derive(Debug)]
enum Token {
    Literal(String),
    /// The integer portion (grouped, with sign-relevant digits) — emitted once.
    IntegerSlot,
    /// The decimal point + fractional digits — emitted once if `dec_places>0`.
    FractionSlot,
}

/// Apply a numeric section to a (already sign-resolved) magnitude. `force_sign`
/// prepends a `-` (used when the positive section is reused for a negative with
/// no explicit negative section).
fn apply_numeric_section(value: f64, section: &str, force_sign: bool) -> String {
    if is_date_section(section) {
        return apply_date_section(value, section);
    }

    let spec = scan_numeric(section);

    let mut v = value;
    if spec.percent {
        v *= 100.0;
    }

    // Round to the declared decimal places.
    let factor = 10f64.powi(spec.dec_places as i32);
    let rounded = (v * factor).round() / factor;

    let int_part = rounded.trunc().abs() as i64;
    let frac_part = (rounded.abs() - int_part as f64) * factor;
    let frac_int = frac_part.round() as i64;

    // Integer rendering with optional grouping + zero-padding.
    let mut int_str = group_integer(int_part, spec.thousands);
    if int_str.len() < spec.int_zeros {
        let pad = spec.int_zeros - int_str.len();
        int_str = format!("{}{}", "0".repeat(pad), int_str);
    }

    // Fractional rendering, zero-padded to dec_places.
    let frac_str = if spec.dec_places > 0 {
        format!(".{:0width$}", frac_int, width = spec.dec_places)
    } else {
        String::new()
    };

    // Reassemble through the token template so literals keep their position.
    let mut out = String::new();
    if force_sign {
        out.push('-');
    }
    for tok in &spec.tokens {
        match tok {
            Token::Literal(s) => out.push_str(s),
            Token::IntegerSlot => out.push_str(&int_str),
            Token::FractionSlot => out.push_str(&frac_str),
        }
    }
    out
}

/// Render an integer with optional thousands grouping (`,`).
fn group_integer(n: i64, thousands: bool) -> String {
    let digits = n.to_string();
    if !thousands || digits.len() <= 3 {
        return digits;
    }
    let bytes = digits.as_bytes();
    let mut out = String::new();
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Scan a numeric section into its [`NumericSpec`]: counts the digit
/// placeholders, detects `,`/`%`, and records the ordered literal/slot template.
/// Quoted (`"…"`) and escaped (`\x`) literals are emitted verbatim.
fn scan_numeric(section: &str) -> NumericSpec {
    let mut spec = NumericSpec::default();
    let mut chars = section.chars().peekable();
    let mut after_decimal = false;
    let mut pending_literal = String::new();
    let mut integer_slot_emitted = false;
    let mut fraction_slot_emitted = false;

    // First pass also needs the comma to be recognized as grouping only when it
    // appears among the integer digit placeholders (a trailing comma is a
    // scaling token we don't support → treated as a literal here, but the
    // fixtures don't use it).
    macro_rules! flush_literal {
        () => {
            if !pending_literal.is_empty() {
                spec.tokens
                    .push(Token::Literal(std::mem::take(&mut pending_literal)));
            }
        };
    }

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(nc) = chars.next() {
                    pending_literal.push(nc);
                }
            }
            '"' => {
                // Quoted literal run until the closing quote.
                for qc in chars.by_ref() {
                    if qc == '"' {
                        break;
                    }
                    pending_literal.push(qc);
                }
            }
            '[' => {
                // Bracket group (locale `[$-409]`, color `[Red]`, condition
                // `[>100]`) — non-printing in our scope; consume + drop it.
                for bc in chars.by_ref() {
                    if bc == ']' {
                        break;
                    }
                }
            }
            '0' | '#' | '?' => {
                if after_decimal {
                    spec.dec_places += 1;
                    if !fraction_slot_emitted {
                        flush_literal!();
                        // Integer slot must precede the fraction slot if it
                        // hasn't been emitted yet (e.g. ".00" with no int digit).
                        if !integer_slot_emitted {
                            spec.tokens.push(Token::IntegerSlot);
                            integer_slot_emitted = true;
                        }
                        spec.tokens.push(Token::FractionSlot);
                        fraction_slot_emitted = true;
                    }
                } else {
                    if c == '0' {
                        spec.int_zeros += 1;
                    } else if c == '#' {
                        spec.int_hashes += 1;
                    }
                    if !integer_slot_emitted {
                        flush_literal!();
                        spec.tokens.push(Token::IntegerSlot);
                        integer_slot_emitted = true;
                    }
                }
            }
            '.' => {
                after_decimal = true;
            }
            ',' => {
                // Grouping comma when between integer digit placeholders.
                if !after_decimal && integer_slot_emitted {
                    spec.thousands = true;
                } else {
                    pending_literal.push(',');
                }
            }
            '%' => {
                spec.percent = true;
                pending_literal.push('%');
            }
            other => pending_literal.push(other),
        }
    }
    flush_literal!();

    // A pure-literal section (no digit placeholders, e.g. a `;"zero"` zero
    // section) renders its literal verbatim — no number is substituted. The
    // token stream already holds the literal; we leave it slot-free so the
    // number does not bleed in.

    spec
}

// ─── text section ─────────────────────────────────────────────────────────────

/// Apply a text section to a string: `@` is the verbatim-text placeholder;
/// quoted/escaped literals surround it.
fn apply_text_section(section: &str, text: &str) -> String {
    let mut out = String::new();
    let mut chars = section.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '@' => out.push_str(text),
            '\\' => {
                if let Some(nc) = chars.next() {
                    out.push(nc);
                }
            }
            '"' => {
                for qc in chars.by_ref() {
                    if qc == '"' {
                        break;
                    }
                    out.push(qc);
                }
            }
            other => out.push(other),
        }
    }
    out
}

// ─── date/time section ────────────────────────────────────────────────────────

/// True when a section contains date/time tokens (`y m d h s` or `AM/PM`)
/// outside quoted/bracketed literals → it must be routed through the date path
/// rather than the numeric path. Mirrors the fork's `is_date_format_code` intent
/// but operates per-section (after the `;` split).
fn is_date_section(section: &str) -> bool {
    let mut in_quote = false;
    let mut in_bracket = false;
    let mut chars = section.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => in_quote = !in_quote,
            '[' if !in_quote => in_bracket = true,
            ']' if !in_quote => in_bracket = false,
            _ if in_quote || in_bracket => {}
            'y' | 'Y' | 'd' | 'D' | 'h' | 'H' | 's' | 'S' => return true,
            // `m` is ambiguous (month vs. minute), but its mere presence still
            // marks a date/time section.
            'm' | 'M' => return true,
            _ => {}
        }
    }
    false
}

/// Apply a date/time section to a serial value. Routes the date components
/// through the fork's `excel_date_to_ymd` (1900 system incl. the leap quirk) and
/// the time components through `excel_datetime_to_ymdhms` — REUSED, never
/// reimplemented. Token recognition is greedy (`yyyy`, `mmmm`, `mmm`, `mm`, `m`,
/// `dddd`, `ddd`, `dd`, `d`, `hh`, `h`, `ss`, `s`, `AM/PM`); a literal `m`/`mm`
/// is disambiguated to MINUTES when it neighbours an hour/second token.
fn apply_date_section(serial: f64, section: &str) -> String {
    // Date components require a serial ≥ 1 (the fork helper assumes the 1900
    // system). A time-only value (a fraction of a day, serial < 1 — e.g. the
    // built-in 21 `h:mm:ss` fixture B7 = 0.5625 = 13:30:00) has no valid date
    // but a perfectly good time, so fall back to (1900,1,0)-ish date parts that
    // never reach the output (time-only codes carry no y/m/d tokens) while still
    // rendering the clock.
    let (y, mo, d) = excel_date_to_ymd(serial).unwrap_or((1900, 1, 1));
    let (h24, min, sec) = match excel_datetime_to_ymdhms(serial) {
        Some((_, _, _, h, m, s)) => (h, m, s),
        None => time_from_fraction(serial),
    };

    // Detect a 12-hour clock (AM/PM present anywhere) for hour rendering.
    let has_ampm = contains_ampm(section);
    let (h12, ampm) = to_12h(h24);

    let chars: Vec<char> = section.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // Track whether the previous emitted date/time token was time-ish, to
    // disambiguate `m`/`mm` (month vs minute).
    let mut prev_was_hour = false;

    while i < chars.len() {
        let c = chars[i];
        let lc = c.to_ascii_lowercase();
        match lc {
            '\\' => {
                if i + 1 < chars.len() {
                    out.push(chars[i + 1]);
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    out.push(chars[i]);
                    i += 1;
                }
                i += 1; // skip closing quote
                continue;
            }
            'y' => {
                let n = run_len(&chars, i, 'y');
                if n >= 4 {
                    out.push_str(&format!("{:04}", y));
                } else {
                    out.push_str(&format!("{:02}", y % 100));
                }
                i += n;
                prev_was_hour = false;
                continue;
            }
            'd' => {
                let n = run_len(&chars, i, 'd');
                match n {
                    1 => out.push_str(&d.to_string()),
                    2 => out.push_str(&format!("{:02}", d)),
                    3 => out.push_str(weekday_short(y, mo, d)),
                    _ => out.push_str(weekday_long(y, mo, d)),
                }
                i += n;
                prev_was_hour = false;
                continue;
            }
            'h' => {
                let n = run_len(&chars, i, 'h');
                let hv = if has_ampm { h12 } else { h24 };
                if n >= 2 {
                    out.push_str(&format!("{:02}", hv));
                } else {
                    out.push_str(&hv.to_string());
                }
                i += n;
                prev_was_hour = true;
                continue;
            }
            's' => {
                let n = run_len(&chars, i, 's');
                if n >= 2 {
                    out.push_str(&format!("{:02}", sec));
                } else {
                    out.push_str(&sec.to_string());
                }
                i += n;
                prev_was_hour = false;
                continue;
            }
            'm' => {
                let n = run_len(&chars, i, 'm');
                // Disambiguate month vs minute: a run of m immediately after an
                // hour token (or immediately before a seconds token) is MINUTES.
                let next_is_seconds = {
                    let j = i + n;
                    j < chars.len() && chars[j].eq_ignore_ascii_case(&'s')
                };
                let is_minute = prev_was_hour || next_is_seconds;
                if is_minute {
                    if n >= 2 {
                        out.push_str(&format!("{:02}", min));
                    } else {
                        out.push_str(&min.to_string());
                    }
                } else {
                    match n {
                        1 => out.push_str(&mo.to_string()),
                        2 => out.push_str(&format!("{:02}", mo)),
                        3 => out.push_str(month_short(mo)),
                        _ => out.push_str(month_long(mo)),
                    }
                }
                i += n;
                prev_was_hour = false;
                continue;
            }
            'a' | 'p' => {
                // AM/PM marker (`AM/PM` or `A/P`). Greedily consume the known
                // marker forms.
                if let Some(len) = match_ampm(&chars, i) {
                    out.push_str(ampm);
                    i += len;
                    prev_was_hour = false;
                    continue;
                }
                out.push(c);
                i += 1;
                continue;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Count the run length of identical (case-insensitive) chars starting at `i`.
fn run_len(chars: &[char], i: usize, target: char) -> usize {
    let mut n = 0;
    while i + n < chars.len() && chars[i + n].to_ascii_lowercase() == target {
        n += 1;
    }
    n
}

/// Whether the section contains an `AM/PM` (or `A/P`) marker.
fn contains_ampm(section: &str) -> bool {
    let chars: Vec<char> = section.chars().collect();
    (0..chars.len()).any(|i| match_ampm(&chars, i).is_some())
}

/// Match an AM/PM marker at `i`, returning its char length: `AM/PM` (5) or
/// `A/P` (3), case-insensitive.
fn match_ampm(chars: &[char], i: usize) -> Option<usize> {
    let up: String = chars[i..].iter().take(5).collect::<String>().to_uppercase();
    if up.starts_with("AM/PM") {
        Some(5)
    } else if up.starts_with("A/P") {
        Some(3)
    } else {
        None
    }
}

/// Extract (hour24, minute, second) from a serial's fractional day, for
/// time-only values (serial < 1) the fork's datetime helper rejects. Mirrors
/// `excel_datetime_to_ymdhms`'s arithmetic (round the day-fraction to whole
/// seconds).
fn time_from_fraction(serial: f64) -> (u32, u32, u32) {
    let frac = serial.fract().abs();
    let total = (frac * 86400.0).round() as u32 % 86400;
    (total / 3600, (total % 3600) / 60, total % 60)
}

/// Convert a 24-hour value to (12-hour value, "AM"/"PM").
fn to_12h(h24: u32) -> (u32, &'static str) {
    let ampm = if h24 < 12 { "AM" } else { "PM" };
    let h = h24 % 12;
    let h = if h == 0 { 12 } else { h };
    (h, ampm)
}

const MONTHS_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTHS_LONG: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];

fn month_short(m: u32) -> &'static str {
    MONTHS_SHORT.get((m as usize).wrapping_sub(1)).copied().unwrap_or("")
}
fn month_long(m: u32) -> &'static str {
    MONTHS_LONG.get((m as usize).wrapping_sub(1)).copied().unwrap_or("")
}

const WEEKDAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WEEKDAYS_LONG: [&str; 7] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];

/// Zeller-style weekday index (0 = Sunday) for a proleptic Gregorian date. Good
/// enough for the post-1900 dates Excel produces; not used by the format fixtures
/// but exercised by tests for the `ddd`/`dddd` tokens.
fn weekday_index(y: i32, m: u32, d: u32) -> usize {
    let (mut y, m) = if m < 3 { (y - 1, m + 12) } else { (y, m) };
    let k = y % 100;
    let j = y / 100;
    let mm = m as i32;
    let dd = d as i32;
    // Zeller's congruence (Gregorian). h: 0=Saturday,1=Sunday,...
    let h = (dd + (13 * (mm + 1)) / 5 + k + k / 4 + j / 4 + 5 * j) % 7;
    // Convert to 0=Sunday.
    let _ = &mut y;
    ((h + 6) % 7) as usize
}

fn weekday_short(y: i32, m: u32, d: u32) -> &'static str {
    WEEKDAYS_SHORT[weekday_index(y, m, d)]
}
fn weekday_long(y: i32, m: u32, d: u32) -> &'static str {
    WEEKDAYS_LONG[weekday_index(y, m, d)]
}
