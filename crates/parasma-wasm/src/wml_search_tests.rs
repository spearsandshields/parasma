//! Native unit tests for the WML full-text-search core (`collect_search_rects`).
//!
//! These build hand-crafted `PageLayout`s (no async layout — the streaming
//! layout path is wasm-only) and assert the read-only substring-match rect
//! computation: case-insensitivity, whole-word gating, table-cell text, and the
//! load-bearing multi-line case where a logical match spans a soft-wrapped line
//! and must yield ONE rect per line-portion.
//!
//! Kept in a SEPARATE file (like `image_extraction_tests`) so adding/removing
//! cases never shifts `lib.rs` source line numbers — those feed the release
//! wasm's panic-location table (check-wasm-identity byte-identity).

use crate::wml::linebreak::LineSegment;
use crate::wml::page::{PageElement, PageLayout, PagedLine, PagedTableCell, PagedTableRow, PARA_SENTINEL};
use crate::{Item, Viewer};
use crate::engine::paint::RunStyle;

/// A text `Item` of the given width (px). The width need not match the glyph
/// metrics — `collect_search_rects` advances by `item.width` only, so a fixed
/// per-char width makes the expected rect geometry exact and font-independent.
fn item(text: &str, width: f64) -> Item {
    Item {
        text: text.to_string(),
        style: RunStyle::marker(),
        width,
        content_width: width,
        char_start: 0,
        char_len: text.len() as u32,
        ..Default::default()
    }
}

/// One single-segment display line (the non-float case) at `baseline_y`,
/// `is_first_line` marking a paragraph boundary.
fn line(items: Vec<Item>, baseline_y: f64, is_first_line: bool) -> PagedLine {
    PagedLine {
        segments: vec![LineSegment { x_start: 0.0, items }],
        para: PARA_SENTINEL,
        baseline_y,
        para_shading: None,
        line_height: 20.0,
        content_left: 0.0,
        content_right: 600.0,
        border_top: None,
        border_bottom: None,
        border_left: None,
        border_right: None,
        bar_tabs: Vec::new(),
        is_first_line,
        is_last_line: false,
    }
}

/// A Viewer with no document loaded — `collect_search_rects` reads only the
/// passed `PageLayout`, never `self.loaded`, so the None state is sufficient.
fn viewer() -> Viewer {
    Viewer::new().expect("viewer")
}

fn opts() -> crate::SearchOpts {
    crate::SearchOpts::default()
}

#[test]
fn case_insensitive_hit_within_a_line() {
    let v = viewer();
    let mut page = PageLayout::new();
    // Deliberately set the 1-based page_number to something OTHER than the 0-based
    // index we query at, to pin that the result's `page` is the queried 0-based
    // index (what scrollToPage / the active-match compare expect), NOT page_number.
    page.page_number = 99;
    // "Hello World" as two items on one line; query "world" (lowercase).
    page.push(PageElement::Line(line(
        vec![item("Hello ", 60.0), item("World", 50.0)],
        100.0,
        true,
    )));
    let matches = v.collect_search_rects(3, &page, "world", opts());
    assert_eq!(matches.len(), 1, "one match");
    assert_eq!(matches[0].rects.len(), 1, "single line → single rect");
    assert_eq!(matches[0].page, 3, "result page = queried 0-based index, not page_number");
    // The match covers the "World" item: x starts at 60 (after "Hello "),
    // width 50, band top = baseline_y - line_height*0.85 = 100 - 17 = 83.
    let r = matches[0].rects[0];
    assert!((r.x - 60.0).abs() < 1e-6, "x={}", r.x);
    assert!((r.w - 50.0).abs() < 1e-6, "w={}", r.w);
    assert!((r.y - 83.0).abs() < 1e-6, "y={}", r.y);
    assert!((matches[0].y - 83.0).abs() < 1e-6, "scroll y = first rect top");
}

#[test]
fn whole_word_gates_substrings() {
    let v = viewer();
    let mut page = PageLayout::new();
    page.push(PageElement::Line(line(vec![item("cat caterpillar", 150.0)], 50.0, true)));
    // Substring "cat" matches twice (the word + inside "caterpillar")...
    let sub = v.collect_search_rects(0, &page, "cat", opts());
    assert_eq!(sub.len(), 2, "substring matches the word AND inside caterpillar");
    // ...whole-word matches only the standalone "cat".
    let ww = crate::SearchOpts { whole_word: true, ..crate::SearchOpts::default() };
    let whole = v.collect_search_rects(0, &page, "cat", ww);
    assert_eq!(whole.len(), 1, "whole-word matches only the standalone token");
}

#[test]
fn match_spanning_a_wrapped_line_yields_two_rects() {
    let v = viewer();
    let mut page = PageLayout::new();
    // One PARAGRAPH wrapped across two display lines: "Hello " on line 1,
    // "World" on line 2. `is_first_line` true only on the first → they group
    // into one paragraph, so the logical text "Hello World" matches "lo wo"
    // across the wrap and emits one rect per line-portion.
    page.push(PageElement::Line(line(vec![item("Hello ", 60.0)], 100.0, true)));
    page.push(PageElement::Line(line(vec![item("World", 50.0)], 120.0, false)));
    let matches = v.collect_search_rects(0, &page, "lo Wo", opts());
    assert_eq!(matches.len(), 1, "one logical match across the wrap");
    assert_eq!(
        matches[0].rects.len(),
        2,
        "a match spanning a wrapped line yields one rect PER line-portion"
    );
    // First rect on line 1 (baseline 100), second on line 2 (baseline 120).
    assert!((matches[0].rects[0].y - 83.0).abs() < 1e-6);
    assert!((matches[0].rects[1].y - 103.0).abs() < 1e-6);
}

#[test]
fn table_cell_text_participates() {
    let v = viewer();
    let mut page = PageLayout::new();
    let cell = PagedTableCell {
        lines: vec![line(vec![item("Invoice", 70.0)], 200.0, true)],
        x: 10.0,
        width_px: 100.0,
        fill_color: None,
        v_merged_away: false,
        border_top: None,
        border_bottom: None,
        border_left: None,
        border_right: None,
    };
    page.push(PageElement::TableRow(PagedTableRow {
        cells: vec![cell],
        y_top: 190.0,
        height_px: 24.0,
        border_color: "#000".to_string(),
        has_borders: false,
        cell_margin_h: 0.0,
        cell_margin_v: 0.0,
    }));
    let matches = v.collect_search_rects(0, &page, "invoice", opts());
    assert_eq!(matches.len(), 1, "table-cell text is searched");
    assert_eq!(matches[0].rects.len(), 1);
}

#[test]
fn no_match_and_empty_query_are_empty() {
    let v = viewer();
    let mut page = PageLayout::new();
    page.push(PageElement::Line(line(vec![item("Hello World", 110.0)], 50.0, true)));
    assert!(v.collect_search_rects(0, &page, "zzz", opts()).is_empty(), "no match → empty");
    assert!(v.collect_search_rects(0, &page, "", opts()).is_empty(), "empty query → empty");
}

#[test]
fn highlight_slices_to_the_matched_term_not_the_whole_item() {
    let v = viewer();
    let mut page = PageLayout::new();
    // One item "category" (width 80, 8 chars); search "cat" (the first 3 chars).
    // The rect must hug "cat" — NOT the whole word "category" (just-the-term find
    // standard). `item()` is unshaped, so the proportional fallback gives 80·3/8.
    page.push(PageElement::Line(line(vec![item("category", 80.0)], 50.0, true)));
    let matches = v.collect_search_rects(0, &page, "cat", opts());
    assert_eq!(matches.len(), 1);
    let r = matches[0].rects[0];
    assert!((r.x - 0.0).abs() < 1e-6, "starts at the matched chars' left, x={}", r.x);
    assert!((r.w - 30.0).abs() < 1e-6, "width hugs 'cat' (80·3/8=30), not 80; w={}", r.w);
}

#[test]
fn case_fold_expanding_chars_are_utf8_safe() {
    // `İ` (U+0130) is 2 BYTES but 1 char, and its default lowercase is TWO chars
    // ("i" + combining dot above), so a folded-char index does not align with the
    // original chars either way round. The old byte-remap indexed an original-char
    // table with a lowered-char index and PANICKED (index out of bounds) — and that
    // runs inside the render paint pass. The multi-byte half of that hazard is live
    // regardless of how İ folds; assert no panic, and that the returned byte range
    // indexes the ORIGINAL string.
    let m = crate::find_matches("İab", "ab", opts());
    assert_eq!(m, vec![(2, 4)], "match maps back to original bytes, no panic");
    assert_eq!(&"İab"[2..4], "ab");
    // Case-insensitive hit on an upper-case source char too.
    let m2 = crate::find_matches("ABC", "bc", opts());
    assert_eq!(m2, vec![(1, 3)]);
}

#[test]
fn greek_final_sigma_is_findable() {
    // The needle and the haystack must be case-folded the SAME WAY. Unicode's
    // final-sigma rule (SpecialCasing.txt) is a STRING-level rule: `str::to_lowercase`
    // renders a word-final Σ as ς (U+03C2), while `char::to_lowercase` always
    // yields σ (U+03C3). The haystack has always been lowered per char; the needle
    // used to be lowered per str, so the two sides disagreed on the last letter and
    // a .docx containing "ΟΔΟΣ" ("street") could not be found by searching for it.
    //
    // The bug survived because 100% of this file's other test data is ASCII, where
    // the two foldings coincide.
    assert_eq!(
        crate::find_matches("ΟΔΟΣ", "ΟΔΟΣ", opts()),
        vec![(0, 8)],
        "an all-caps Greek word must find itself"
    );
    // Lower-case needle against upper-case haystack, and the reverse.
    assert_eq!(crate::find_matches("ΟΔΟΣ", "οδος", opts()), vec![(0, 8)]);
    assert_eq!(crate::find_matches("οδος", "ΟΔΟΣ", opts()), vec![(0, 8)]);
    // Non-final sigma inside a word still matches (σ is medial here).
    assert_eq!(crate::find_matches("ΚΟΣΜΟΣ", "κοσμος", opts()), vec![(0, 12)]);
    // Embedded in a sentence — the byte range indexes the ORIGINAL string.
    let hay = "Λεωφόρος ΟΔΟΣ 12";
    let m = crate::find_matches(hay, "ΟΔΟΣ", opts());
    assert_eq!(m.len(), 1, "one hit in {hay:?}");
    assert_eq!(&hay[m[0].0..m[0].1], "ΟΔΟΣ");
    // The SML path (`find_matches_any`) already agreed; pin that they now CONCUR,
    // since the whole defect was one rule with two spellings giving opposite answers.
    assert!(crate::find_matches_any("ΟΔΟΣ", "ΟΔΟΣ", opts()));
}

#[test]
fn fold_lowered_matches_its_derivation() {
    // Most of `fold_lowered`'s table is not hand-picked: it is DERIVED from the
    // rule "fold by round-tripping through uppercase, skipping non-1:1 case
    // pairs". Re-derive it here over every input the fn can ever see — the fn
    // is only ever applied to the OUTPUT of `char::to_lowercase` — so the table
    // can neither drift from the rule nor rot silently when a rustc bump brings
    // a new Unicode version (this test starts failing instead, forcing a look).
    fn round_trip(c: char) -> char {
        let mut up = c.to_uppercase();
        let u = match (up.next(), up.next()) {
            (Some(u), None) => u,
            // Multi-char uppercase (ß → "SS", ﬁ → "FI", …). The rule cannot see
            // these, which is also exactly what Unicode simple folding does for
            // most of them (they are F-only entries in CaseFolding.txt) — the
            // three that DO have a 1:1 S line are the exceptions listed below.
            _ => return c,
        };
        match {
            let mut lo = u.to_lowercase();
            (lo.next(), lo.next())
        } {
            (Some(l), None) => l,
            _ => c, // multi-char lowercase (only İ): same 1:1 guard
        }
    }
    // The arms the rule CANNOT produce, each a 1:1 `S` line of CaseFolding.txt
    // quoted verbatim. Hand-listed, so pinned by value rather than by derivation.
    const HAND_ADDED: &[(char, char, &str)] = &[
        ('\u{1FD3}', '\u{0390}', "1FD3; S; 0390"),
        ('\u{1FE3}', '\u{03B0}', "1FE3; S; 03B0"),
        ('\u{FB05}', '\u{FB06}', "FB05; S; FB06"),
    ];
    let mut remapped = 0u32;
    for cp in 0..=0x10FFFFu32 {
        let Some(c) = char::from_u32(cp) else { continue };
        for l in c.to_lowercase() {
            let expect = HAND_ADDED
                .iter()
                .find(|(from, _, _)| *from == l)
                .map_or_else(|| round_trip(l), |(_, to, _)| *to);
            assert_eq!(
                crate::fold_lowered(l),
                expect,
                "fold_lowered(U+{:04X}) diverges from the derivation rule",
                l as u32
            );
            if expect != l {
                remapped += 1;
            }
        }
    }
    // The rule remaps a KNOWN, small set; a jump here means the Unicode tables
    // moved under us and the match arms in fold_lowered need re-deriving.
    assert!(remapped > 0, "derivation found no remapped chars — probe is broken");
    // Every hand-added arm must still be EARNING its place: reachable by the fn at
    // all, and genuinely beyond the rule's reach. If a stdlib Unicode bump ever
    // made the round trip produce one of these on its own, it should be deleted
    // from this list rather than left as a silent duplicate of the derivation.
    for (from, to, line) in HAND_ADDED {
        assert_eq!(crate::fold_lowered(*from), *to, "hand-added arm `{line}` is not in effect");
        assert!(
            from.to_lowercase().eq(std::iter::once(*from)),
            "U+{:04X} is not in the image of to_lowercase — fold_lowered can never see it",
            *from as u32
        );
        assert_ne!(
            round_trip(*from),
            *to,
            "`{line}` is now derivable by the round-trip rule — drop the hand-added arm"
        );
    }
}

#[test]
fn fold_char_matches_vendored_scf() {
    // Pin the fold against UNICODE, not against our own rule.
    // `fold_lowered_matches_its_derivation` re-derives the table from the
    // round-trip heuristic, so it catches RULE drift but is structurally blind
    // to SPEC drift: the heuristic merely COINCIDES with `Simple_Case_Folding`
    // today, with no stability guarantee, and if the two ever part ways the fn
    // and that test move together and stay green. This test compares against
    // the spec text itself: the C and S lines of CaseFolding.txt, vendored
    // VERBATIM (grep-extracted, lines untouched) at testdata/. Proven to catch
    // what the derivation test cannot: deleting the FB05 arm from BOTH
    // `fold_lowered` and the derivation test's HAND_ADDED list leaves that test
    // green and turns this one red.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/CaseFolding-17.0.0.txt");
    let data = std::fs::read_to_string(path).expect("vendored CaseFolding C+S lines must exist");

    // THE VERSION PAIR. The vendored file and rustc's case tables version
    // independently, so this oracle is only sound with a version check. The
    // file names its own Unicode version on line 1; assert it against the
    // version whose tables `char::to_lowercase` implements, so a toolchain
    // upgrade that moves Unicode fails HERE, loudly, instead of the oracle and
    // the implementation silently drifting apart.
    let named = data
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("# CaseFolding-"))
        .and_then(|l| l.strip_suffix(".txt"))
        .expect("vendored file must open with `# CaseFolding-<version>.txt`");
    let mut fields = named.split('.').map(|p| p.parse::<u8>().expect("numeric version field"));
    let vendored = (
        fields.next().expect("major"),
        fields.next().expect("minor"),
        fields.next().expect("update"),
    );
    assert_eq!(
        vendored,
        char::UNICODE_VERSION,
        "vendored CaseFolding-{named}.txt vs rustc Unicode {:?}: a toolchain bump moved \
         Unicode — re-vendor the new version's C+S lines into testdata/, re-audit \
         fold_lowered's table against it, and update the counts pinned below",
        char::UNICODE_VERSION
    );

    // Parse `<code>; <status>; <mapping>; # <name>`. C/S mappings are single
    // code points by definition (S is "mappings to single characters").
    let mut scf: std::collections::HashMap<char, char> = std::collections::HashMap::new();
    for line in data.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        let mut f = line.split(';').map(str::trim);
        let (code, status, mapping) =
            (f.next().unwrap(), f.next().unwrap(), f.next().unwrap_or_default());
        assert!(matches!(status, "C" | "S"), "non-C/S line vendored: {line}");
        let from = char::from_u32(u32::from_str_radix(code, 16).unwrap()).unwrap();
        let to = char::from_u32(u32::from_str_radix(mapping, 16).unwrap()).unwrap();
        assert!(scf.insert(from, to).is_none(), "duplicate C/S entry for U+{code}");
    }
    // Anti-truncation: Unicode 17.0.0 ships exactly 1512 C+S lines. A partial
    // read must not pass vacuously. Update alongside a re-vendor.
    assert_eq!(scf.len(), 1512, "vendored C+S line count moved — file truncated or corrupt");

    // The ONE declared deviation (see fold_char's doc): the Turkic union folds
    // ı and İ into the I ≡ i class. First assert it IS a deviation — the spec's
    // merges for those two are `T` lines, which are deliberately not vendored,
    // so simple folding leaves both alone — then apply it to the oracle and
    // require agreement EVERYWHERE, with no code point excluded.
    for turkic in ['\u{0131}', '\u{0130}'] {
        assert!(
            !scf.contains_key(&turkic),
            "U+{:04X} grew a C/S line — the Turkic deviation is no longer a deviation, \
             revisit fold_char's union",
            turkic as u32
        );
        scf.insert(turkic, 'i');
    }
    let scf_of = |c: char| *scf.get(&c).unwrap_or(&c);

    // fold_char must induce EXACTLY the equivalence relation of Simple_Case_
    // Folding — agreement up to CHOICE OF REPRESENTATIVE, not mapped value
    // (Cherokee folds upward under scf, U+AB70 → U+13A0, downward here; same
    // classes either way, and only classes decide what matches). Two folds
    // partition identically iff their representatives map 1:1 onto each other:
    // walk every code point and demand the (ours ↔ scf) pairing never forks.
    let ours = |c: char| {
        let mut f = crate::fold_char(c);
        let one = f.next().expect("fold_char yields at least one char");
        assert!(f.next().is_none(), "fold_char expanded at U+{:04X}", c as u32);
        one
    };
    let mut ours_to_scf: std::collections::HashMap<char, char> = std::collections::HashMap::new();
    let mut scf_to_ours: std::collections::HashMap<char, char> = std::collections::HashMap::new();
    let mut diverged: Vec<u32> = Vec::new();
    for cp in 0..=0x10FFFFu32 {
        let Some(c) = char::from_u32(cp) else { continue };
        let (o, s) = (ours(c), scf_of(c));
        if *ours_to_scf.entry(o).or_insert(s) != s || *scf_to_ours.entry(s).or_insert(o) != o {
            diverged.push(cp);
        }
    }
    assert!(
        diverged.is_empty(),
        "fold_char's equivalence classes diverge from CaseFolding.txt C+S at U+{diverged:04X?}"
    );
}

#[test]
fn fold_char_is_strictly_one_to_one() {
    // The invariant `find_matches`' byte map is built around, pinned exhaustively
    // rather than assumed. U+0130 İ was the ONE char in all of Unicode whose
    // `char::to_lowercase` is longer than one char; `fold_char` pre-maps it to i
    // (the Turkic tailoring), so the fold is now 1:1 over every code point. A
    // Unicode bump that adds another expanding lowercase fails HERE, at a clear
    // statement of the invariant, instead of surfacing as duplicate highlight
    // rects and an inflated match count somewhere in the paint pass.
    let mut expanding: Vec<u32> = Vec::new();
    for cp in 0..=0x10FFFFu32 {
        let Some(c) = char::from_u32(cp) else { continue };
        if crate::fold_char(c).count() != 1 {
            expanding.push(cp);
        }
    }
    assert!(expanding.is_empty(), "fold_char is not 1:1 for {expanding:04X?}");
    // U+0130 is still the only multi-char lowercase — i.e. the pre-map above is
    // the ONLY reason this holds, and it is doing exactly one job.
    let multi: Vec<u32> = (0..=0x10FFFFu32)
        .filter_map(char::from_u32)
        .filter(|c| c.to_lowercase().count() > 1)
        .map(|c| c as u32)
        .collect();
    assert_eq!(multi, vec![0x0130], "the set of expanding lowercases moved");
}

#[test]
fn nfc_duplicate_greek_and_ligature_spellings_match() {
    // The three hand-added CaseFolding.txt `S` arms, at the level a user sees.
    // ΐ and ΰ each have two identical-looking encodings — U+0390/U+03B0 (with
    // TONOS, the NFC form) and U+1FD3/U+1FE3 (with OXIA) — which is the same
    // silent-no-match defect as final sigma, one canonical-duplicate layer down.
    assert_eq!(crate::find_matches("\u{1FD3}", "\u{0390}", opts()), vec![(0, 3)]);
    assert_eq!(crate::find_matches("\u{0390}", "\u{1FD3}", opts()), vec![(0, 2)]);
    assert_eq!(crate::find_matches("\u{1FE3}", "\u{03B0}", opts()), vec![(0, 3)]);
    assert_eq!(crate::find_matches("\u{03B0}", "\u{1FE3}", opts()), vec![(0, 2)]);
    // ﬅ (long s + t) and ﬆ (st) are the two ligature spellings of "st".
    assert_eq!(crate::find_matches("\u{FB05}", "\u{FB06}", opts()), vec![(0, 3)]);
    // The SML path must CONCUR — one shared fold_char, the lesson of the bug.
    assert!(crate::find_matches_any("\u{1FD3}", "\u{0390}", opts()));
    // Still NOT normalization: the DECOMPOSED spelling is a separate rung
    // (D145 canonical caseless matching) and is deliberately not reached.
    assert!(crate::find_matches("\u{03B9}\u{0308}\u{0301}", "\u{0390}", opts()).is_empty());
}

#[test]
fn turkish_dotted_capital_i_finds_plain_i() {
    // İ (U+0130) completes the Turkic union begun by ı: Unicode's T lines are
    // `0049; T; 0131` (I → ı) and `0130; T; 0069` (İ → i), and we take the union
    // of default and Turkic, so {I, i, ı, İ} is ONE class. Before this arm İ folded
    // to the default two chars "i" + U+0307, and the combining dot blocked every
    // match: typing "istanbul" found nothing in a document spelling it "İstanbul",
    // plausibly the most common proper noun in Turkish text.
    assert_eq!(crate::find_matches("İstanbul", "istanbul", opts()), vec![(0, 9)]);
    assert_eq!(crate::find_matches("İSTANBUL", "istanbul", opts()), vec![(0, 9)]);
    assert_eq!(crate::find_matches("İZMİR", "izmir", opts()), vec![(0, 7)]);
    assert_eq!(crate::find_matches("izmir", "İZMİR", opts()), vec![(0, 5)]);
    // …and it joins the class ı is already in, both directions.
    assert_eq!(crate::find_matches("İ", "ı", opts()), vec![(0, 2)]);
    assert!(crate::find_matches_any("İstanbul", "istanbul", opts()));
    // Byte ranges still index the ORIGINAL string across the 2-byte İ.
    let hay = "Merhaba İstanbul!";
    let m = crate::find_matches(hay, "istanbul", opts());
    assert_eq!(m.len(), 1);
    assert_eq!(&hay[m[0].0..m[0].1], "İstanbul");
    // DECLARED DEVIATION, the price of the arm: the decomposed spelling "i" +
    // U+0307 (what a non-Turkish-locale lowercasing of İ produces) no longer
    // matches İ. Rarer than İ itself, visibly doubled-dotted, and Firefox makes
    // the same trade — pinned so a future change flips it on purpose.
    assert!(crate::find_matches("i\u{0307}stanbul", "İstanbul", opts()).is_empty());
    // Match Case remains the exact-distinction escape hatch.
    let cs = crate::SearchOpts { case_sensitive: true, ..crate::SearchOpts::default() };
    assert!(crate::find_matches("İstanbul", "istanbul", cs).is_empty());
}

#[test]
fn micro_sign_and_greek_mu_are_equivalent() {
    // U+00B5 MICRO SIGN (the char keyboards and Windows-1252 legacy conversions
    // produce, e.g. Option+M) and U+03BC GREEK SMALL MU (the char Greek keyboards
    // and equations produce) are identical-looking and BOTH uppercase to Μ —
    // the same defect class as final sigma, and the likeliest one in real
    // documents ("µm", "µg" in scientific text). Both directions must match.
    let m = crate::find_matches("5 µm", "μm", opts());
    assert_eq!(m, vec![(2, 5)], "Greek-mu needle finds micro-sign haystack");
    assert_eq!(&"5 µm"[2..5], "µm", "byte range indexes the ORIGINAL string");
    assert_eq!(crate::find_matches("5 μm", "µm", opts()), vec![(2, 5)]);
    // Uppercase Μ needle reaches both lowercase spellings.
    assert_eq!(crate::find_matches("µm", "Μm", opts()), vec![(0, 3)]);
    // The SML path must CONCUR (one shared fold_char — the lesson of the bug).
    assert!(crate::find_matches_any("5 µm", "μm", opts()));
    assert!(crate::find_matches_any("5 μm", "µm", opts()));
}

#[test]
fn greek_symbol_letters_fold_to_their_base_letters() {
    // Physics/math text (and OMML equations, which Parasma renders) uses the
    // Greek SYMBOL letterforms ϕ ϑ ϵ ϱ ϖ ϰ ϐ; prose uses φ θ ε ρ π κ β. Both
    // spellings uppercase to the same letter, so search must merge them.
    assert_eq!(crate::find_matches("ϕ = 1.618", "φ", opts()), vec![(0, 2)]);
    assert_eq!(crate::find_matches("cos ϑ", "θ", opts()), vec![(4, 6)]);
    assert_eq!(crate::find_matches("angle θ here", "ϑ", opts()), vec![(6, 8)]);
    assert!(crate::find_matches_any("ϕ = 1.618", "Φ", opts()));
}

#[test]
fn turkish_dotless_i_merges_with_i() {
    // DELIBERATE deviation from Unicode default folding: ı (U+0131) folds to i.
    // This is the union of default and Turkic folding — without it, ALL-CAPS
    // Turkish text is unsearchable: the haystack "KIRMIZI" lowers I→i while the
    // needle "kırmızı" keeps ı, so strict simple folding (Firefox's behaviour)
    // finds NOTHING. Chrome's locale collation also merges them. Cost: a Turkish
    // user searching "kir" over-matches "kır" — visible and dismissible, and
    // Match Case remains the exact-distinction escape hatch.
    assert_eq!(crate::find_matches("KIRMIZI", "kırmızı", opts()), vec![(0, 7)]);
    assert_eq!(crate::find_matches("kırmızı", "kirmizi", opts()), vec![(0, 10)]);
    assert_eq!(crate::find_matches("kırmızı", "KIRMIZI", opts()), vec![(0, 10)]);
    assert!(crate::find_matches_any("KIRMIZI", "kırmızı", opts()));
    // Match Case still distinguishes the two i's.
    let cs = crate::SearchOpts { case_sensitive: true, ..crate::SearchOpts::default() };
    assert!(crate::find_matches("kırmızı", "kirmizi", cs).is_empty());
}

#[test]
fn sharp_s_stays_unfolded_but_long_s_folds() {
    // ß is the one common char the fold DELIBERATELY skips: 'ß'.to_uppercase()
    // is "SS" (two chars), so it has no 1:1 case pair and Unicode simple folding
    // (C+S) leaves it alone too — ß ≡ ss is an `F` line, the FULL-folding rung
    // (D144) we decline. Not because expanding would crash: the byte map handles
    // n:1 fine. Because it would return the SAME range twice ("s" in "Straße" →
    // [(0,1), (4,6), (4,6)]), inflating the hit count and half-highlighting a ß.
    // See find_matches' docstring. Consequence, pinned here as a CONSCIOUS choice:
    // "strasse" does NOT find "Straße" (status quo, and Firefox's simple-fold
    // behaviour). A future full-folding attempt must flip these asserts on
    // purpose, not by accident.
    assert!(crate::find_matches("Straße", "strasse", opts()).is_empty());
    assert!(crate::find_matches("ß", "ss", opts()).is_empty());
    // Capital ẞ (U+1E9E) needs no fold arm — char::to_lowercase already maps it
    // to ß — so the two sharp-s forms DO match each other.
    assert_eq!(crate::find_matches("ẞ", "ß", opts()), vec![(0, 3)]);
    // Long s ſ (U+017F) IS in the fold (1:1 → s): historic German/English text.
    assert_eq!(crate::find_matches("ſtraße", "straße", opts()), vec![(0, 8)]);
    assert_eq!(&"ſtraße"[0..8], "ſtraße");
}

#[test]
fn matches_token_bracketed_by_slashes() {
    // Regression for the "v1 between slashes yields no result" report: the matcher
    // is a plain contiguous substring scan over the paragraph's logical text, so
    // punctuation like '/' is NOT a token boundary — "v1" inside "/v1/" matches.
    // (If a real doc still misses it, the text is OUTSIDE the searched set —
    // e.g. a header/footer, now covered by collect_search_rects — not the matcher.)
    let m = crate::find_matches("GET /api/v1/users", "v1", opts());
    assert_eq!(m, vec![(9, 11)]);
    assert_eq!(&"GET /api/v1/users"[9..11], "v1");
    // Whole "/v1/" path token matches too.
    assert_eq!(crate::find_matches("a/v1/b", "/v1/", opts()), vec![(1, 5)]);
}
