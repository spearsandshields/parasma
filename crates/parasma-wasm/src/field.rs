//! OOXML field instruction parsing and result formatting (§17.16).
//!
//! A complex field's instruction string (from `instrText` runs) or a simple
//! field's `instr` attribute is parsed into a [`FieldInstruction`], and field
//! results are formatted via the `\*` (general), `\#` (numeric), and `\@`
//! (date) switches.
//!
//! Clean-room implementation from ECMA-376 §17.16. The formatter covers the
//! general, numeric and date switches in full: case-aware roman
//! and alphabetic numbering (`\* roman` vs `\* ROMAN`), ordinal / cardinal-text
//! / ordinal-text numbering, numeric picture clauses with grouping and
//! sign sections, and weekday-aware date formatting (`ddd`/`dddd`).

// The numeric-picture / spelled-number formatters (`to_roman`, `to_card_text`,
// `format_numeric_picture`, `render_picture_section`, …) are a partially-wired
// field-code feature: implemented clean-room from §17.16 but not yet reachable
// from the live field-result path. Kept intact (not deleted) pending wire-up.
#![allow(dead_code)]

// ─── Date / Time value types ────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: i32,
    pub month: u32, // 1-12
    pub day: u32,   // 1-31
    pub hour: u32,  // 0-23
    pub minute: u32,
    pub second: u32,
}

impl DateTime {
    /// Day of week: 0=Sunday … 6=Saturday (Sakamoto's algorithm).
    pub fn weekday(&self) -> u32 {
        let t = [0u32, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
        let mut y = self.year;
        if self.month < 3 {
            y -= 1;
        }
        let m = self.month as usize;
        let d = self.day as i32;
        (((y + y / 4 - y / 100 + y / 400 + t[m - 1] as i32 + d) % 7) + 7) as u32 % 7
    }

    /// Convert Unix epoch milliseconds (UTC) to a `DateTime`. Used for the
    /// JS-supplied `now` value for DATE/TIME fields. Civil-from-days per
    /// Howard Hinnant's algorithm; no leap seconds, proleptic Gregorian.
    pub fn from_epoch_millis(millis: i64) -> DateTime {
        let secs = millis.div_euclid(1000);
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let hour = (rem / 3600) as u32;
        let minute = ((rem % 3600) / 60) as u32;
        let second = (rem % 60) as u32;

        // days since 1970-01-01 → civil date (Hinnant).
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097; // [0, 146096]
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
        let mp = (5 * doy + 2) / 153; // [0, 11]
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
        let year = (y + if month <= 2 { 1 } else { 0 }) as i32;

        DateTime { year, month, day, hour, minute, second }
    }

    /// Parse an ISO-8601 timestamp as used in OOXML core properties
    /// (`dcterms:created`/`modified`), e.g. `2026-05-30T14:05:09Z`. Lenient:
    /// time portion optional. Returns None if the date portion is malformed.
    pub fn parse_iso8601(s: &str) -> Option<DateTime> {
        let s = s.trim();
        let (date_part, time_part) = match s.split_once('T') {
            Some((d, t)) => (d, Some(t)),
            None => (s, None),
        };
        let mut dp = date_part.split('-');
        let year = dp.next()?.parse::<i32>().ok()?;
        let month = dp.next()?.parse::<u32>().ok()?;
        let day = dp.next()?.parse::<u32>().ok()?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let (mut hour, mut minute, mut second) = (0u32, 0u32, 0u32);
        if let Some(tp) = time_part {
            // Strip trailing zone designator (Z or ±hh:mm) — we treat as wall clock.
            let tp = tp.trim_end_matches('Z');
            let tp = tp.split(['+']).next().unwrap_or(tp);
            // Avoid splitting the negative-zone case incorrectly: only cut a
            // '-' that appears after the time digits (position > 0).
            let tp = if let Some(pos) = tp.find('-') { &tp[..pos] } else { tp };
            let mut t = tp.split(':');
            hour = t.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            minute = t.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            second = t.next().and_then(|x| {
                // seconds may carry a fraction
                x.split('.').next()?.parse().ok()
            }).unwrap_or(0);
        }
        Some(DateTime { year, month, day, hour, minute, second })
    }
}

// ─── Common formatting switches ─────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommonSwitches {
    /// `\*` general format (e.g. "ROMAN", "Ordinal", "MERGEFORMAT").
    pub general: Option<String>,
    /// `\#` numeric picture (e.g. "#,##0.00").
    pub numeric: Option<String>,
    /// `\@` date/time picture (e.g. "dddd, MMMM d, yyyy").
    pub date: Option<String>,
}

// ─── Parsed instruction ─────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum FieldInstruction {
    Page(CommonSwitches),
    NumPages(CommonSwitches),
    Section(CommonSwitches),
    SectionPages(CommonSwitches),
    Date(CommonSwitches),
    Time(CommonSwitches),
    CreateDate(CommonSwitches),
    SaveDate(CommonSwitches),
    PrintDate(CommonSwitches),
    /// Any field we don't evaluate live — its cached result is rendered as-is.
    Other,
}

impl FieldInstruction {
    pub fn switches(&self) -> Option<&CommonSwitches> {
        match self {
            FieldInstruction::Page(s)
            | FieldInstruction::NumPages(s)
            | FieldInstruction::Section(s)
            | FieldInstruction::SectionPages(s)
            | FieldInstruction::Date(s)
            | FieldInstruction::Time(s)
            | FieldInstruction::CreateDate(s)
            | FieldInstruction::SaveDate(s)
            | FieldInstruction::PrintDate(s) => Some(s),
            FieldInstruction::Other => None,
        }
    }
}

// ─── Tokenizer ──────────────────────────────────────────────────────────────

enum Token {
    Word(String),
    Quoted(String),
    Switch(char),
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '"' {
            i += 1;
            let start = i;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            tokens.push(Token::Quoted(chars[start..i].iter().collect()));
            if i < chars.len() {
                i += 1; // closing quote
            }
        } else if c == '\\' {
            i += 1;
            if i < chars.len() && !chars[i].is_whitespace() {
                tokens.push(Token::Switch(chars[i]));
                i += 1;
            }
        } else {
            let start = i;
            while i < chars.len()
                && !chars[i].is_whitespace()
                && chars[i] != '"'
                && chars[i] != '\\'
            {
                i += 1;
            }
            tokens.push(Token::Word(chars[start..i].iter().collect()));
        }
    }
    tokens
}

/// Parse a field instruction string into a typed instruction.
pub fn parse(input: &str) -> FieldInstruction {
    let tokens = tokenize(input);
    let mut iter = tokens.iter().peekable();
    let keyword = match iter.next() {
        Some(Token::Word(w)) => w.to_ascii_uppercase(),
        _ => return FieldInstruction::Other,
    };

    // Extract \* \# \@ switches; ignore field-specific tokens.
    let mut sw = CommonSwitches::default();
    while let Some(tok) = iter.next() {
        if let Token::Switch(c) = tok {
            match c {
                '*' => sw.general = next_value(&mut iter),
                '#' => sw.numeric = next_value(&mut iter),
                '@' => sw.date = next_value(&mut iter),
                _ => {
                    // field-specific switch may take a value; skip a following value token
                    let _ = iter.peek();
                }
            }
        }
    }

    match keyword.as_str() {
        "PAGE" => FieldInstruction::Page(sw),
        "NUMPAGES" => FieldInstruction::NumPages(sw),
        "SECTION" => FieldInstruction::Section(sw),
        "SECTIONPAGES" => FieldInstruction::SectionPages(sw),
        "DATE" => FieldInstruction::Date(sw),
        "TIME" => FieldInstruction::Time(sw),
        "CREATEDATE" => FieldInstruction::CreateDate(sw),
        "SAVEDATE" => FieldInstruction::SaveDate(sw),
        "PRINTDATE" => FieldInstruction::PrintDate(sw),
        _ => FieldInstruction::Other,
    }
}

fn next_value<'a, I: Iterator<Item = &'a Token>>(
    iter: &mut std::iter::Peekable<I>,
) -> Option<String> {
    match iter.next() {
        Some(Token::Word(w)) => Some(w.clone()),
        Some(Token::Quoted(q)) => Some(q.clone()),
        _ => None,
    }
}

// ─── Number formatting ──────────────────────────────────────────────────────

/// Format an integer field result, honoring the `\*` general switch and `\#`
/// numeric picture. Defaults to plain decimal.
pub fn format_number(n: i64, sw: &CommonSwitches) -> String {
    if let Some(g) = &sw.general {
        let r = apply_general_number(n, g);
        if let Some(r) = r {
            return r;
        }
    }
    if let Some(pic) = &sw.numeric {
        return format_numeric_picture(n as f64, pic);
    }
    n.to_string()
}

/// Apply a `\*` general format to a number. Returns None if the format isn't a
/// recognized numeric format (caller falls back to decimal / picture).
fn apply_general_number(n: i64, fmt: &str) -> Option<String> {
    // Case matters: "roman" vs "ROMAN", "alphabetic" vs "ALPHABETIC".
    let upper = fmt.eq_ignore_ascii_case("roman")
        || fmt.eq_ignore_ascii_case("alphabetic")
        || fmt.eq_ignore_ascii_case("arabic")
        || fmt.eq_ignore_ascii_case("ordinal")
        || fmt.eq_ignore_ascii_case("cardtext")
        || fmt.eq_ignore_ascii_case("ordtext")
        || fmt.eq_ignore_ascii_case("hex")
        || fmt.eq_ignore_ascii_case("dollartext");
    if !upper {
        return None;
    }
    let lower = fmt.chars().next().is_some_and(|c| c.is_ascii_lowercase());
    let s = match fmt.to_ascii_uppercase().as_str() {
        "ARABIC" => n.to_string(),
        "ROMAN" => to_roman(n),
        "ALPHABETIC" => to_alphabetic(n),
        "ORDINAL" => to_ordinal(n),
        "CARDTEXT" => to_card_text(n),
        "ORDTEXT" => to_ord_text(n),
        "HEX" => format!("{:X}", n),
        "DOLLARTEXT" => to_dollar_text(n),
        _ => return None,
    };
    Some(if lower { s.to_ascii_lowercase() } else { s })
}

fn to_roman(n: i64) -> String {
    if n <= 0 || n >= 4000 {
        return n.to_string();
    }
    const TABLE: &[(i64, &str)] = &[
        (1000, "M"), (900, "CM"), (500, "D"), (400, "CD"),
        (100, "C"), (90, "XC"), (50, "L"), (40, "XL"),
        (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I"),
    ];
    let mut n = n;
    let mut out = String::new();
    for &(v, s) in TABLE {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    out
}

fn to_alphabetic(n: i64) -> String {
    if n <= 0 {
        return n.to_string();
    }
    // 1→A, 26→Z, 27→AA (spreadsheet-style bijective base-26).
    let mut n = n as u64;
    let mut out = Vec::new();
    while n > 0 {
        let rem = ((n - 1) % 26) as u8;
        out.push(b'A' + rem);
        n = (n - 1) / 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

fn to_ordinal(n: i64) -> String {
    let suffix = match (n % 100, n % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

const ONES: &[&str] = &[
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight",
    "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen",
    "sixteen", "seventeen", "eighteen", "nineteen",
];
const TENS: &[&str] = &[
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy",
    "eighty", "ninety",
];

fn spell_below_thousand(n: i64) -> String {
    let mut parts = Vec::new();
    let hundreds = n / 100;
    let rem = n % 100;
    if hundreds > 0 {
        parts.push(format!("{} hundred", ONES[hundreds as usize]));
    }
    if rem >= 20 {
        let t = TENS[(rem / 10) as usize].to_string();
        if rem % 10 > 0 {
            parts.push(format!("{}-{}", t, ONES[(rem % 10) as usize]));
        } else {
            parts.push(t);
        }
    } else if rem > 0 {
        parts.push(ONES[rem as usize].to_string());
    }
    parts.join(" ")
}

fn to_card_text(n: i64) -> String {
    if n == 0 {
        return "zero".to_string();
    }
    if n < 0 {
        return format!("negative {}", to_card_text(-n));
    }
    const SCALES: &[(i64, &str)] = &[
        (1_000_000_000, "billion"),
        (1_000_000, "million"),
        (1_000, "thousand"),
    ];
    let mut n = n;
    let mut parts = Vec::new();
    for &(scale, name) in SCALES {
        if n >= scale {
            let count = n / scale;
            parts.push(format!("{} {}", spell_below_thousand(count), name));
            n %= scale;
        }
    }
    if n > 0 {
        parts.push(spell_below_thousand(n));
    }
    parts.join(" ")
}

fn to_ord_text(n: i64) -> String {
    // Cardinal text, with the final word made ordinal.
    let card = to_card_text(n);
    let words: Vec<&str> = card.rsplitn(2, ' ').collect();
    let (last, prefix) = match words.as_slice() {
        [last, prefix] => (*last, Some(*prefix)),
        [last] => (*last, None),
        _ => return card,
    };
    let ord_last = ordinalize_word(last);
    match prefix {
        Some(p) => format!("{p} {ord_last}"),
        None => ord_last,
    }
}

fn ordinalize_word(w: &str) -> String {
    // Handle hyphenated tens like "twenty-one" → "twenty-first".
    if let Some((head, tail)) = w.rsplit_once('-') {
        return format!("{}-{}", head, ordinalize_word(tail));
    }
    match w {
        "one" => "first",
        "two" => "second",
        "three" => "third",
        "five" => "fifth",
        "eight" => "eighth",
        "nine" => "ninth",
        "twelve" => "twelfth",
        _ => {
            return if let Some(stripped) = w.strip_suffix('y') {
                format!("{stripped}ieth") // twenty → twentieth
            } else {
                format!("{w}th")
            };
        }
    }
    .to_string()
}

fn to_dollar_text(n: i64) -> String {
    // e.g. 23 → "twenty-three and 00/100"
    format!("{} and 00/100", to_card_text(n))
}

/// Format a number against a `\#` numeric picture clause (§17.16.4.3).
/// Supports `0`/`#` digit placeholders, `,` grouping, `.` decimal, and up to
/// three `;`-separated sections (positive;negative;zero). Literal characters
/// (currency symbols, spaces) pass through.
fn format_numeric_picture(value: f64, pic: &str) -> String {
    let sections: Vec<&str> = pic.split(';').collect();
    let (section, val) = match sections.as_slice() {
        [pos] => (*pos, value),
        [pos, neg] => {
            if value < 0.0 {
                (*neg, value.abs())
            } else {
                (*pos, value)
            }
        }
        [pos, neg, zero, ..] => {
            if value > 0.0 {
                (*pos, value)
            } else if value < 0.0 {
                (*neg, value.abs())
            } else {
                (*zero, 0.0)
            }
        }
        _ => return value.to_string(),
    };
    render_picture_section(section, val)
}

fn render_picture_section(section: &str, val: f64) -> String {
    // Split into integer and fractional pattern at the decimal point.
    let (int_pat, frac_pat) = match section.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (section, None),
    };

    let frac_digits = frac_pat
        .map(|f| f.chars().filter(|c| *c == '0' || *c == '#').count())
        .unwrap_or(0);

    let rounded = if frac_digits > 0 {
        let m = 10f64.powi(frac_digits as i32);
        (val * m).round() / m
    } else {
        val.round()
    };

    let int_part = rounded.trunc().abs() as i64;
    let frac_part = ((rounded.fract().abs()) * 10f64.powi(frac_digits as i32)).round() as i64;

    let grouped = int_pat.contains(',');
    let int_str = if grouped {
        group_thousands(int_part)
    } else {
        int_part.to_string()
    };

    // Leading literals from the integer pattern (e.g. "$" or "(").
    let is_ph = |c: char| c == '0' || c == '#' || c == ',';
    let mut out = String::new();
    let int_lit_prefix: String = int_pat.chars().take_while(|c| !is_ph(*c)).collect();
    out.push_str(&int_lit_prefix);
    out.push_str(&int_str);

    if let Some(fp) = frac_pat {
        out.push('.');
        let s = format!("{:0width$}", frac_part, width = frac_digits);
        out.push_str(&s);
        // trailing literal after fractional digits
        let frac_lit_suffix: String = fp
            .chars()
            .skip_while(|c| *c == '0' || *c == '#')
            .collect();
        out.push_str(&frac_lit_suffix);
    } else {
        // No fractional pattern: append trailing literal from the integer
        // pattern (e.g. the ")" in "(0)" or a "%").
        let int_lit_suffix: String = int_pat
            .chars()
            .rev()
            .take_while(|c| !is_ph(*c))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        out.push_str(&int_lit_suffix);
    }
    out
}

fn group_thousands(n: i64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::new();
    let len = bytes.len();
    for (idx, b) in bytes.iter().enumerate() {
        if idx > 0 && (len - idx).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

// ─── Date / Time formatting ─────────────────────────────────────────────────

const SHORT_MONTHS: &[&str] = &[
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct",
    "Nov", "Dec",
];
const LONG_MONTHS: &[&str] = &[
    "January", "February", "March", "April", "May", "June", "July", "August",
    "September", "October", "November", "December",
];
const SHORT_DAYS: &[&str] = &["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const LONG_DAYS: &[&str] = &[
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday",
    "Saturday",
];

/// Format a date/time against a `\@` picture (§17.16.4.1). Supports
/// y/yy/yyyy, M/MM/MMM/MMMM, d/dd/ddd/dddd, H/HH, h/hh, m/mm, s/ss, AM/PM,
/// and single-quoted literals.
pub fn format_datetime(dt: &DateTime, pat: &str) -> String {
    let chars: Vec<char> = pat.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let n = chars.len();
    while i < n {
        let c = chars[i];
        // Literal in single quotes
        if c == '\'' {
            i += 1;
            while i < n && chars[i] != '\'' {
                out.push(chars[i]);
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        // AM/PM marker (case-sensitive variants)
        if chars[i..].starts_with(&['A', 'M', '/', 'P', 'M']) {
            out.push_str(if dt.hour < 12 { "AM" } else { "PM" });
            i += 5;
            continue;
        }
        if chars[i..].starts_with(&['a', 'm', '/', 'p', 'm']) {
            out.push_str(if dt.hour < 12 { "am" } else { "pm" });
            i += 5;
            continue;
        }
        let run = count_run(&chars, i, c);
        match c {
            'y' => {
                if run >= 4 {
                    out.push_str(&format!("{:04}", dt.year));
                } else {
                    out.push_str(&format!("{:02}", (dt.year % 100).abs()));
                }
            }
            'M' => match run {
                1 => out.push_str(&dt.month.to_string()),
                2 => out.push_str(&format!("{:02}", dt.month)),
                3 => out.push_str(month_name(SHORT_MONTHS, dt.month)),
                _ => out.push_str(month_name(LONG_MONTHS, dt.month)),
            },
            'd' => match run {
                1 => out.push_str(&dt.day.to_string()),
                2 => out.push_str(&format!("{:02}", dt.day)),
                3 => out.push_str(SHORT_DAYS[dt.weekday() as usize]),
                _ => out.push_str(LONG_DAYS[dt.weekday() as usize]),
            },
            'H' => {
                if run >= 2 {
                    out.push_str(&format!("{:02}", dt.hour));
                } else {
                    out.push_str(&dt.hour.to_string());
                }
            }
            'h' => {
                let h12 = match dt.hour % 12 {
                    0 => 12,
                    h => h,
                };
                if run >= 2 {
                    out.push_str(&format!("{:02}", h12));
                } else {
                    out.push_str(&h12.to_string());
                }
            }
            'm' => {
                if run >= 2 {
                    out.push_str(&format!("{:02}", dt.minute));
                } else {
                    out.push_str(&dt.minute.to_string());
                }
            }
            's' => {
                if run >= 2 {
                    out.push_str(&format!("{:02}", dt.second));
                } else {
                    out.push_str(&dt.second.to_string());
                }
            }
            _ => {
                for _ in 0..run {
                    out.push(c);
                }
            }
        }
        i += run;
    }
    out
}

fn month_name<'a>(table: &'a [&'a str], month: u32) -> &'a str {
    table.get((month.saturating_sub(1)) as usize).copied().unwrap_or("")
}

fn count_run(chars: &[char], start: usize, c: char) -> usize {
    chars[start..].iter().take_while(|&&x| x == c).count()
}

/// Default date picture when DATE/CREATEDATE/etc. omit `\@` (US English).
pub fn default_date_picture() -> &'static str {
    "M/d/yyyy"
}
/// Default time picture when TIME omits `\@`.
pub fn default_time_picture() -> &'static str {
    "h:mm AM/PM"
}

/// Dates available to the field evaluator for one document. `now` is the
/// JS-supplied current time (for DATE/TIME); `created`/`modified` come from
/// core properties (for CREATEDATE/SAVEDATE). Any may be absent.
#[derive(Clone, Copy, Default, Debug)]
pub struct DocDates {
    pub now: Option<DateTime>,
    pub created: Option<DateTime>,
    pub modified: Option<DateTime>,
}

/// Resolve a *document-level* field instruction (DATE/TIME/CREATEDATE/
/// SAVEDATE/PRINTDATE) to its formatted string, applying the `\@` picture and
/// `\*` general switch. Returns None when the field isn't a date field or the
/// required date is unavailable — caller then falls back to the cached result.
/// PAGE/NUMPAGES/SECTION are NOT handled here (they need layout context).
pub fn resolve_doc_field(instr: &str, dates: &DocDates) -> Option<String> {
    let parsed = parse(instr);
    let (dt, default_pic, is_time) = match &parsed {
        FieldInstruction::Date(_) => (dates.now?, default_date_picture(), false),
        FieldInstruction::Time(_) => (dates.now?, default_time_picture(), true),
        FieldInstruction::CreateDate(_) => (dates.created?, default_date_picture(), false),
        FieldInstruction::SaveDate(_) => (dates.modified?, default_date_picture(), false),
        FieldInstruction::PrintDate(_) => return None, // no print-time available in a viewer
        _ => return None,
    };
    let sw = parsed.switches()?;
    let pic = sw.date.as_deref().unwrap_or(default_pic);
    let mut out = format_datetime(&dt, pic);
    if let Some(g) = &sw.general {
        out = apply_general_text(&out, g);
    }
    let _ = is_time;
    Some(out)
}

/// Apply `\*` Upper/Lower/FirstCap/Caps to a text value (non-numeric formats).
fn apply_general_text(value: &str, fmt: &str) -> String {
    match fmt.to_ascii_uppercase().as_str() {
        "UPPER" => value.to_uppercase(),
        "LOWER" => value.to_lowercase(),
        "FIRSTCAP" => {
            let mut c = value.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
            }
        }
        "CAPS" => value
            .split(' ')
            .map(|w| {
                let mut c = w.chars();
                match c.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => value.to_string(),
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sw_general(g: &str) -> CommonSwitches {
        CommonSwitches { general: Some(g.to_string()), ..Default::default() }
    }
    fn sw_numeric(p: &str) -> CommonSwitches {
        CommonSwitches { numeric: Some(p.to_string()), ..Default::default() }
    }

    #[test]
    fn parse_recognizes_keywords() {
        assert!(matches!(parse(" PAGE "), FieldInstruction::Page(_)));
        assert!(matches!(parse(" page "), FieldInstruction::Page(_)));
        assert!(matches!(parse(" NUMPAGES "), FieldInstruction::NumPages(_)));
        assert!(matches!(parse(" DATE "), FieldInstruction::Date(_)));
        assert!(matches!(parse(" SAVEDATE "), FieldInstruction::SaveDate(_)));
        assert!(matches!(parse(" FOOBAR x "), FieldInstruction::Other));
    }

    #[test]
    fn parse_extracts_switches() {
        let f = parse(r#" PAGE \* ROMAN "#);
        assert_eq!(f.switches().unwrap().general.as_deref(), Some("ROMAN"));
        let f = parse(r#" DATE \@ "dddd, MMMM d, yyyy" "#);
        assert_eq!(f.switches().unwrap().date.as_deref(), Some("dddd, MMMM d, yyyy"));
        let f = parse(r##" PAGE \# "#,##0" "##);
        // numeric picture captured (quoted)
        assert!(f.switches().unwrap().numeric.is_some());
    }

    // — Case-aware roman / alphabetic —

    #[test]
    fn roman_is_case_aware() {
        assert_eq!(format_number(4, &sw_general("ROMAN")), "IV");
        assert_eq!(format_number(4, &sw_general("roman")), "iv");
        assert_eq!(format_number(2026, &sw_general("roman")), "mmxxvi");
    }

    #[test]
    fn alphabetic_is_case_aware() {
        assert_eq!(format_number(1, &sw_general("ALPHABETIC")), "A");
        assert_eq!(format_number(27, &sw_general("ALPHABETIC")), "AA");
        assert_eq!(format_number(2, &sw_general("alphabetic")), "b");
    }

    // — Ordinal and cardinal text formats —

    #[test]
    fn ordinal_format() {
        assert_eq!(format_number(1, &sw_general("Ordinal")), "1st");
        assert_eq!(format_number(2, &sw_general("Ordinal")), "2nd");
        assert_eq!(format_number(11, &sw_general("Ordinal")), "11th");
        assert_eq!(format_number(23, &sw_general("Ordinal")), "23rd");
    }

    #[test]
    fn cardinal_text_format() {
        assert_eq!(format_number(1, &sw_general("CardText")), "one");
        assert_eq!(format_number(21, &sw_general("CardText")), "twenty-one");
        assert_eq!(format_number(105, &sw_general("CardText")), "one hundred five");
        assert_eq!(format_number(1000, &sw_general("CardText")), "one thousand");
    }

    #[test]
    fn ordinal_text_format() {
        assert_eq!(format_number(1, &sw_general("OrdText")), "first");
        assert_eq!(format_number(2, &sw_general("OrdText")), "second");
        assert_eq!(format_number(21, &sw_general("OrdText")), "twenty-first");
        assert_eq!(format_number(30, &sw_general("OrdText")), "thirtieth");
    }

    // — Numeric picture clauses —

    #[test]
    fn numeric_grouping() {
        assert_eq!(format_number(1234567, &sw_numeric("#,##0")), "1,234,567");
    }

    #[test]
    fn numeric_decimal_and_grouping() {
        assert_eq!(format_numeric_picture(1234.5, "#,##0.00"), "1,234.50");
    }

    #[test]
    fn numeric_currency_literal() {
        assert_eq!(format_numeric_picture(1234.0, "$#,##0.00"), "$1,234.00");
    }

    #[test]
    fn numeric_negative_section() {
        // positive;negative — negatives use the 2nd section (parenthesized)
        assert_eq!(format_numeric_picture(-5.0, "0;(0)"), "(5)");
        assert_eq!(format_numeric_picture(5.0, "0;(0)"), "5");
    }

    // — Date formatting, including weekday names (ddd/dddd) —

    fn dt() -> DateTime {
        // 2026-05-30 14:05:09 — May 30 2026 is a Saturday.
        DateTime { year: 2026, month: 5, day: 30, hour: 14, minute: 5, second: 9 }
    }

    #[test]
    fn epoch_millis_to_datetime() {
        // 2026-05-30T14:05:09Z = 1780149909 s = 1780149909000 ms
        let d = DateTime::from_epoch_millis(1_780_149_909_000);
        assert_eq!(d, DateTime { year: 2026, month: 5, day: 30, hour: 14, minute: 5, second: 9 });
        // Unix epoch itself.
        let e = DateTime::from_epoch_millis(0);
        assert_eq!(e, DateTime { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 0 });
        // A leap day: 2024-02-29T12:00:00Z = 1709208000 s.
        let leap = DateTime::from_epoch_millis(1_709_208_000_000);
        assert_eq!(leap.year, 2024);
        assert_eq!(leap.month, 2);
        assert_eq!(leap.day, 29);
    }

    #[test]
    fn resolve_doc_fields() {
        let dates = DocDates {
            now: Some(dt()),
            created: Some(DateTime { year: 2019, month: 12, day: 25, hour: 9, minute: 0, second: 0 }),
            modified: Some(DateTime { year: 2026, month: 1, day: 2, hour: 0, minute: 0, second: 0 }),
        };
        assert_eq!(resolve_doc_field(" DATE \\@ \"dddd, MMMM d, yyyy\" ", &dates).as_deref(),
            Some("Saturday, May 30, 2026"));
        assert_eq!(resolve_doc_field(" DATE ", &dates).as_deref(), Some("5/30/2026"));
        assert_eq!(resolve_doc_field(" TIME ", &dates).as_deref(), Some("2:05 PM"));
        assert_eq!(resolve_doc_field(" CREATEDATE \\@ \"yyyy-MM-dd\" ", &dates).as_deref(),
            Some("2019-12-25"));
        assert_eq!(resolve_doc_field(" SAVEDATE \\@ \"M/d/yyyy\" ", &dates).as_deref(),
            Some("1/2/2026"));
        // Upper general switch.
        assert_eq!(resolve_doc_field(" DATE \\@ \"MMMM\" \\* Upper ", &dates).as_deref(),
            Some("MAY"));
        // Non-date field → None (handled elsewhere).
        assert_eq!(resolve_doc_field(" PAGE ", &dates), None);
        // Missing date → None (fall back to cached).
        let empty = DocDates::default();
        assert_eq!(resolve_doc_field(" DATE ", &empty), None);
    }

    #[test]
    fn iso8601_parsing() {
        let d = DateTime::parse_iso8601("2026-05-30T14:05:09Z").unwrap();
        assert_eq!(d, DateTime { year: 2026, month: 5, day: 30, hour: 14, minute: 5, second: 9 });
        // Date only.
        let d2 = DateTime::parse_iso8601("2019-12-25").unwrap();
        assert_eq!((d2.year, d2.month, d2.day), (2019, 12, 25));
        // With timezone offset (treated as wall clock).
        let d3 = DateTime::parse_iso8601("2020-06-15T08:30:00+05:00").unwrap();
        assert_eq!((d3.hour, d3.minute), (8, 30));
        // Fractional seconds.
        let d4 = DateTime::parse_iso8601("2021-01-01T00:00:00.123Z").unwrap();
        assert_eq!(d4.second, 0);
        assert!(DateTime::parse_iso8601("garbage").is_none());
    }

    #[test]
    fn weekday_computation() {
        assert_eq!(dt().weekday(), 6); // Saturday
        // 2000-01-01 was a Saturday
        let d = DateTime { year: 2000, month: 1, day: 1, hour: 0, minute: 0, second: 0 };
        assert_eq!(d.weekday(), 6);
    }

    #[test]
    fn date_full_with_weekday() {
        assert_eq!(format_datetime(&dt(), "dddd, MMMM d, yyyy"), "Saturday, May 30, 2026");
        assert_eq!(format_datetime(&dt(), "ddd"), "Sat");
    }

    #[test]
    fn date_numeric_patterns() {
        assert_eq!(format_datetime(&dt(), "M/d/yyyy"), "5/30/2026");
        assert_eq!(format_datetime(&dt(), "dd/MM/yy"), "30/05/26");
    }

    #[test]
    fn time_patterns() {
        assert_eq!(format_datetime(&dt(), "h:mm AM/PM"), "2:05 PM");
        assert_eq!(format_datetime(&dt(), "HH:mm:ss"), "14:05:09");
    }

    #[test]
    fn date_literals() {
        assert_eq!(format_datetime(&dt(), "'Updated' MMMM yyyy"), "Updated May 2026");
    }

    #[test]
    fn mergeformat_passthrough_keeps_decimal() {
        // \* MERGEFORMAT is not a numeric format → falls back to decimal.
        assert_eq!(format_number(7, &sw_general("MERGEFORMAT")), "7");
    }
}



