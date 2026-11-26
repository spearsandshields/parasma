//! Enumerated numbering: how a counter value is written in the number styles
//! of Word list levels, page and note numbers and field results
//! (§17.18.59 `ST_NumberFormat`, §17.16.4.3 `\*` switches) and PowerPoint
//! auto-numbered paragraphs (§20.1.10.61 `ST_TextAutonumberScheme`).
//!
//! One formatter for every host, so "item 27" reads the same everywhere.

/// A counter style. Hosts map their own names onto these.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CounterStyle {
    #[default]
    Decimal,
    /// At least this many digits, padded with zeros: `decimalZero` is 2
    /// (01, 02, …, 10), Word's custom "001, 002, 003, ..." is 3.
    DecimalPadded(u8),
    UpperRoman,
    LowerRoman,
    /// A, B, …, Z, then AA, BB, …: the letter repeated (§17.18.59).
    UpperLetter,
    LowerLetter,
    /// 1st, 2nd, 3rd.
    Ordinal,
    /// One, Two, Three.
    CardinalText,
    /// First, Second, Third.
    OrdinalText,
    /// Twenty-three and 00/100.
    DollarText,
    Hex,
    /// *, †, ‡, §, then doubled.
    Chicago,
    /// - 1 -
    NumberInDash,
    DecimalEnclosedCircle,
    DecimalEnclosedFullstop,
    DecimalEnclosedParen,
    DecimalFullWidth,
    /// Each digit as an ideograph: 一〇.
    IdeographDigital,
    /// Chinese and Japanese counting: 一, 十, 十一, 二十, 百.
    IdeographCounting,
    /// The ten heavenly stems: 甲, 乙, 丙.
    IdeographTraditional,
    /// The twelve earthly branches: 子, 丑, 寅.
    IdeographZodiac,
    /// Katakana in gojūon order, half width.
    Aiueo,
    AiueoFullWidth,
    /// Katakana in iroha order, half width.
    Iroha,
    IrohaFullWidth,
    /// Hangul syllables: 가, 나, 다.
    Ganada,
    /// Hangul initial consonants: ㄱ, ㄴ, ㄷ.
    Chosung,
    /// Each digit in Sino-Korean: 일영.
    KoreanDigital,
    /// Sino-Korean counting: 일, 십, 십일.
    KoreanCounting,
    /// Hebrew numerals (gematria): א, ב, …, י, יא.
    Hebrew1,
    /// The Hebrew alphabet in order, repeated past the end.
    Hebrew2,
    ArabicAlpha,
    ArabicAbjad,
    RussianLower,
    RussianUpper,
    ThaiLetters,
    ThaiNumbers,
    HindiNumbers,
    /// No number at all.
    None,
}

/// `n` written in `style`. Values a style cannot write (zero or negative for
/// letters and numerals, past the end of a fixed table) are written in
/// decimal, as Word does.
pub(crate) fn format(n: i64, style: CounterStyle) -> String {
    use CounterStyle as S;
    match style {
        S::None => String::new(),
        S::Decimal => n.to_string(),
        S::DecimalPadded(width) if n >= 0 => format!("{n:0width$}", width = usize::from(width)),
        S::DecimalPadded(_) => n.to_string(),
        S::UpperRoman => roman(n).unwrap_or_else(|| n.to_string()),
        S::LowerRoman => roman(n).map_or_else(|| n.to_string(), |r| r.to_lowercase()),
        S::UpperLetter => repeated(n, &LATIN_UPPER),
        S::LowerLetter => repeated(n, &LATIN_LOWER),
        S::Ordinal => ordinal(n),
        S::CardinalText => capitalize(&cardinal_text(n)),
        S::OrdinalText => capitalize(&ordinal_text(n)),
        S::DollarText => capitalize(&format!("{} and 00/100", cardinal_text(n))),
        S::Hex => format!("{n:X}"),
        S::Chicago => repeated(n, &["*", "\u{2020}", "\u{2021}", "\u{a7}"]),
        S::NumberInDash => format!("- {n} -"),
        S::DecimalEnclosedCircle => match n {
            1..=20 => char_at(0x2460, n - 1),
            21..=35 => char_at(0x3251, n - 21),
            36..=50 => char_at(0x32B1, n - 36),
            _ => n.to_string(),
        },
        S::DecimalEnclosedFullstop => if (1..=20).contains(&n) { char_at(0x2488, n - 1) } else { n.to_string() },
        S::DecimalEnclosedParen => if (1..=20).contains(&n) { char_at(0x2474, n - 1) } else { n.to_string() },
        S::DecimalFullWidth => digits(n, |d| char::from_u32(0xFF10 + d)),
        S::IdeographDigital => digits(n, |d| IDEOGRAPH_DIGITS[d as usize].chars().next()),
        S::IdeographCounting => counting(n, &IDEOGRAPH_DIGITS, ["十", "百", "千"]),
        S::IdeographTraditional => cycled(n, &STEMS),
        S::IdeographZodiac => cycled(n, &BRANCHES),
        S::Aiueo => repeated(n, &split(AIUEO_HALF)),
        S::AiueoFullWidth => repeated(n, &split(AIUEO_FULL)),
        S::Iroha => repeated(n, &split(IROHA_HALF)),
        S::IrohaFullWidth => repeated(n, &split(IROHA_FULL)),
        S::Ganada => repeated(n, &split("가나다라마바사아자차카타파하")),
        S::Chosung => repeated(n, &split("ㄱㄴㄷㄹㅁㅂㅅㅇㅈㅊㅋㅌㅍㅎ")),
        S::KoreanDigital => digits(n, |d| KOREAN_DIGITS[d as usize].chars().next()),
        S::KoreanCounting => counting(n, &KOREAN_DIGITS, ["십", "백", "천"]),
        S::Hebrew1 => hebrew_numeral(n).unwrap_or_else(|| n.to_string()),
        S::Hebrew2 => repeated(n, &split("אבגדהוזחטיכלמנסעפצקרשת")),
        S::ArabicAlpha => repeated(n, &split("أبتثجحخدذرزسشصضطظعغفقكلمنهوي")),
        S::ArabicAbjad => repeated(n, &split("أبجدهوزحطيكلمنسعفصقرشتثخذضظغ")),
        S::RussianLower => repeated(n, &split(RUSSIAN)),
        S::RussianUpper => repeated(n, &split(&RUSSIAN.to_uppercase())),
        S::ThaiLetters => repeated(n, &split("กขคงจฉชซฌญฎฏฐฑฒณดตถทธนบปผฝพฟภมยรลวศษสหฬอฮ")),
        S::ThaiNumbers => digits(n, |d| char::from_u32(0x0E50 + d)),
        S::HindiNumbers => digits(n, |d| char::from_u32(0x0966 + d)),
    }
}

const LATIN_UPPER: [&str; 26] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z",
];
const LATIN_LOWER: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s",
    "t", "u", "v", "w", "x", "y", "z",
];
const IDEOGRAPH_DIGITS: [&str; 10] = ["〇", "一", "二", "三", "四", "五", "六", "七", "八", "九"];
const KOREAN_DIGITS: [&str; 10] = ["영", "일", "이", "삼", "사", "오", "육", "칠", "팔", "구"];
const STEMS: [&str; 10] = ["甲", "乙", "丙", "丁", "戊", "己", "庚", "辛", "壬", "癸"];
const BRANCHES: [&str; 12] = ["子", "丑", "寅", "卯", "辰", "巳", "午", "未", "申", "酉", "戌", "亥"];
const AIUEO_HALF: &str = "ｱｲｳｴｵｶｷｸｹｺｻｼｽｾｿﾀﾁﾂﾃﾄﾅﾆﾇﾈﾉﾊﾋﾌﾍﾎﾏﾐﾑﾒﾓﾔﾕﾖﾗﾘﾙﾚﾛﾜｦﾝ";
const AIUEO_FULL: &str = "アイウエオカキクケコサシスセソタチツテトナニヌネノハヒフヘホマミムメモヤユヨラリルレロワヲン";
const IROHA_HALF: &str = "ｲﾛﾊﾆﾎﾍﾄﾁﾘﾇﾙｦﾜｶﾖﾀﾚｿﾂﾈﾅﾗﾑｳｲﾉｵｸﾔﾏｹﾌｺｴﾃｱｻｷﾕﾒﾐｼｴﾋﾓｾｽ";
const IROHA_FULL: &str = "イロハニホヘトチリヌルヲワカヨタレソツネナラムウヰノオクヤマケフコエテアサキユメミシヱヒモセス";
/// Word's Russian list letters leave out ё, й, ъ, ы and ь.
const RUSSIAN: &str = "абвгдежзиклмнопрстуфхцчшщэюя";

fn split(s: &str) -> Vec<&str> {
    s.char_indices().map(|(i, c)| &s[i..i + c.len_utf8()]).collect()
}

fn char_at(base: u32, offset: i64) -> String {
    char::from_u32(base + offset as u32).map(String::from).unwrap_or_default()
}

/// The `n`th symbol, then each repeated: a … z, aa … zz, aaa.
fn repeated<S: AsRef<str>>(n: i64, symbols: &[S]) -> String {
    if n < 1 || symbols.is_empty() {
        return n.to_string();
    }
    let k = symbols.len() as i64;
    symbols[((n - 1) % k) as usize].as_ref().repeat(((n - 1) / k + 1) as usize)
}

/// The `n`th symbol of a cycle that starts again after the last.
fn cycled(n: i64, symbols: &[&str]) -> String {
    if n < 1 {
        return n.to_string();
    }
    symbols[((n - 1) % symbols.len() as i64) as usize].to_string()
}

/// Each decimal digit of `n` written by `digit`.
fn digits(n: i64, digit: impl Fn(u32) -> Option<char>) -> String {
    if n < 0 {
        return n.to_string();
    }
    n.to_string().chars().filter_map(|c| c.to_digit(10).and_then(&digit)).collect()
}

fn roman(n: i64) -> Option<String> {
    if !(1..4000).contains(&n) {
        return None;
    }
    const TABLE: &[(i64, &str)] = &[
        (1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"), (50, "L"),
        (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I"),
    ];
    let mut n = n;
    let mut out = String::new();
    for &(value, numeral) in TABLE {
        while n >= value {
            out.push_str(numeral);
            n -= value;
        }
    }
    Some(out)
}

/// Chinese, Japanese or Sino-Korean counting below 10 000: a digit before
/// each of ten, hundred and thousand, a leading one left out (十, 百, 千;
/// 십, 백, 천).
fn counting(n: i64, digits: &[&str; 10], units: [&str; 3]) -> String {
    if !(1..10_000).contains(&n) {
        return n.to_string();
    }
    let mut out = String::new();
    for (place, unit) in [(1000, units[2]), (100, units[1]), (10, units[0])] {
        let d = (n / place) % 10;
        if d > 1 {
            out.push_str(digits[d as usize]);
        }
        if d > 0 {
            out.push_str(unit);
        }
    }
    let ones = n % 10;
    if ones > 0 {
        out.push_str(digits[ones as usize]);
    }
    out
}

/// Hebrew numerals below 1 000 (gematria): hundreds, tens and units as
/// letters, with 15 and 16 written טו and טז.
fn hebrew_numeral(n: i64) -> Option<String> {
    if !(1..1000).contains(&n) {
        return None;
    }
    const HUNDREDS: [&str; 10] = ["", "ק", "ר", "ש", "ת", "תק", "תר", "תש", "תת", "תתק"];
    const TENS: [&str; 10] = ["", "י", "כ", "ל", "מ", "נ", "ס", "ע", "פ", "צ"];
    const UNITS: [&str; 10] = ["", "א", "ב", "ג", "ד", "ה", "ו", "ז", "ח", "ט"];
    let mut out = HUNDREDS[(n / 100) as usize].to_string();
    match n % 100 {
        15 => out.push_str("טו"),
        16 => out.push_str("טז"),
        r => {
            out.push_str(TENS[(r / 10) as usize]);
            out.push_str(UNITS[(r % 10) as usize]);
        }
    }
    Some(out)
}

fn ordinal(n: i64) -> String {
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
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen",
    "nineteen",
];
const TENS_WORDS: &[&str] = &["", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];

fn below_thousand(n: i64) -> String {
    let mut parts = Vec::new();
    let (hundreds, rem) = (n / 100, n % 100);
    if hundreds > 0 {
        parts.push(format!("{} hundred", ONES[hundreds as usize]));
    }
    if rem >= 20 {
        let t = TENS_WORDS[(rem / 10) as usize];
        parts.push(if rem % 10 > 0 { format!("{t}-{}", ONES[(rem % 10) as usize]) } else { t.to_string() });
    } else if rem > 0 {
        parts.push(ONES[rem as usize].to_string());
    }
    parts.join(" ")
}

/// A number in English words, lower case: "twenty-three".
pub(crate) fn cardinal_text(n: i64) -> String {
    if n == 0 {
        return "zero".to_string();
    }
    if n < 0 {
        return format!("negative {}", cardinal_text(-n));
    }
    let mut n = n;
    let mut parts = Vec::new();
    for (scale, name) in [(1_000_000_000, "billion"), (1_000_000, "million"), (1_000, "thousand")] {
        if n >= scale {
            parts.push(format!("{} {name}", below_thousand(n / scale)));
            n %= scale;
        }
    }
    if n > 0 {
        parts.push(below_thousand(n));
    }
    parts.join(" ")
}

/// A number as an English ordinal word, lower case: "twenty-third".
pub(crate) fn ordinal_text(n: i64) -> String {
    let card = cardinal_text(n);
    let (prefix, last) = match card.rsplit_once(' ') {
        Some((p, l)) => (Some(p), l),
        None => (None, card.as_str()),
    };
    let last = ordinal_word(last);
    match prefix {
        Some(p) => format!("{p} {last}"),
        None => last,
    }
}

fn ordinal_word(w: &str) -> String {
    if let Some((head, tail)) = w.rsplit_once('-') {
        return format!("{head}-{}", ordinal_word(tail));
    }
    match w {
        "one" => "first".into(),
        "two" => "second".into(),
        "three" => "third".into(),
        "five" => "fifth".into(),
        "eight" => "eighth".into(),
        "nine" => "ninth".into(),
        "twelve" => "twelfth".into(),
        _ => match w.strip_suffix('y') {
            Some(stem) => format!("{stem}ieth"),
            None => format!("{w}th"),
        },
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{format, CounterStyle as S};

    #[test]
    fn latin_numerals_and_letters() {
        assert_eq!(format(4, S::UpperRoman), "IV");
        assert_eq!(format(14, S::LowerRoman), "xiv");
        assert_eq!(format(27, S::LowerLetter), "aa", "§17.18.59: the letter repeats");
        assert_eq!(format(28, S::UpperLetter), "BB");
        assert_eq!(format(7, S::DecimalPadded(2)), "07");
        assert_eq!(format(12, S::DecimalPadded(2)), "12");
        assert_eq!(format(7, S::DecimalPadded(3)), "007");
        assert_eq!(format(1234, S::DecimalPadded(3)), "1234");
        assert_eq!(format(3, S::None), "");
    }

    #[test]
    fn words_and_symbols() {
        assert_eq!(format(22, S::Ordinal), "22nd");
        assert_eq!(format(13, S::Ordinal), "13th");
        assert_eq!(format(21, S::CardinalText), "Twenty-one");
        assert_eq!(format(21, S::OrdinalText), "Twenty-first");
        assert_eq!(format(6, S::Chicago), "\u{2020}\u{2020}");
        assert_eq!(format(3, S::NumberInDash), "- 3 -");
        assert_eq!(format(255, S::Hex), "FF");
    }

    #[test]
    fn enclosed_and_full_width() {
        assert_eq!(format(1, S::DecimalEnclosedCircle), "①");
        assert_eq!(format(21, S::DecimalEnclosedCircle), "㉑");
        assert_eq!(format(2, S::DecimalEnclosedParen), "⑵");
        assert_eq!(format(3, S::DecimalEnclosedFullstop), "⒊");
        assert_eq!(format(12, S::DecimalFullWidth), "１２");
    }

    #[test]
    fn east_asian() {
        assert_eq!(format(10, S::IdeographCounting), "十");
        assert_eq!(format(11, S::IdeographCounting), "十一");
        assert_eq!(format(23, S::IdeographCounting), "二十三");
        assert_eq!(format(105, S::IdeographCounting), "百五");
        assert_eq!(format(10, S::IdeographDigital), "一〇");
        assert_eq!(format(2, S::IdeographTraditional), "乙");
        assert_eq!(format(13, S::IdeographZodiac), "子", "the branches cycle");
        assert_eq!(format(1, S::Aiueo), "ｱ");
        assert_eq!(format(2, S::IrohaFullWidth), "ロ");
        assert_eq!(format(3, S::Ganada), "다");
        assert_eq!(format(1, S::Chosung), "ㄱ");
        assert_eq!(format(15, S::KoreanCounting), "십오");
    }

    #[test]
    fn right_to_left_and_others() {
        assert_eq!(format(1, S::Hebrew1), "א");
        assert_eq!(format(15, S::Hebrew1), "טו");
        assert_eq!(format(2, S::Hebrew2), "ב");
        assert_eq!(format(1, S::ArabicAlpha), "أ");
        assert_eq!(format(3, S::ArabicAbjad), "ج");
        assert_eq!(format(3, S::RussianLower), "в");
        assert_eq!(format(1, S::ThaiLetters), "ก");
        assert_eq!(format(12, S::ThaiNumbers), "๑๒");
        assert_eq!(format(3, S::HindiNumbers), "३");
    }

    #[test]
    fn values_a_style_cannot_write_fall_back_to_decimal() {
        assert_eq!(format(0, S::UpperRoman), "0");
        assert_eq!(format(0, S::LowerLetter), "0");
        assert_eq!(format(51, S::DecimalEnclosedCircle), "51");
    }
}
