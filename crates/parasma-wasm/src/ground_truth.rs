//! The Word ground-truth harness.
//!
//! Every WML/SML fixture in this suite is **synthetic**, built inline from XML
//! string literals, so the suite measures engine self-consistency rather than
//! agreement with Word. This module is the one place that measures agreement
//! with Word, and it is the only place allowed to state what Word did.
//!
//! # The split
//!
//! The manifest below is **tracked** and names nothing: it is keyed by opaque
//! calibration ID (`docs/calibration-ids.md`), and carries only the
//! Word-observed expectation, where that observation came from, and how much it
//! is worth. The ID → filename and ID → sha256 bindings live in
//! `.calibration/manifest.private.json`, which is gitignored; the harness
//! resolves an ID through that file at run time and checks the digest from it.
//! So the repository carries page-break expectations that identify no document.
//!
//! # The one rule
//!
//! **The expectation is always Word's value.** Where Parasma is known to
//! disagree, the Word value stays the expectation and Parasma's current output
//! is recorded beside it as a `Deviation` — a pin, explicitly not ground truth.
//! Recording Parasma's own output as the expectation would make this module
//! worthless, which is the failure mode it exists to prevent.
//!
//! # States, and why absence is the interesting one
//!
//! Calibration documents are not distributed with the repository (`fixtures/`
//! is gitignored in full), so on CI and on most machines they are simply
//! absent — and unlike `fixtures/`, they are **not regenerable**: three of the
//! documents behind recorded measurements are already gone. So neither
//! in-tree convention works here. A hard panic turns the gate
//! red on every clean checkout; the `eprintln!`-and-return skip used by
//! `sml/tests.rs` passes green with `cargo test` swallowing the stderr.
//!
//! This harness therefore reports **through the assertion channel**, and
//! distinguishes six outcomes per entry:
//!
//! | State | Meaning | Gate |
//! |---|---|---|
//! | `Checked` | present, hash verified, expectation compared | red on disagreement |
//! | `HashMismatch` | present but a *different* document under this ID | **always red** |
//! | `Absent` | not on this machine | green, counted, `GROUND_TRUTH_REQUIRE` makes it red |
//! | `Unresolvable` | no row in the private manifest — an ID that *looks* resolvable and is not | same as `Absent` |
//! | `Lost` | recorded as gone: the measurements survive, the document does not | green (it is a record, not a check) |
//! | `NoExpectation` | the document is here and nobody has opened it in Word | green, reported every run |
//!
//! A run where **zero** entries were checked prints `⚠ GROUND-TRUTH: 0 CHECKED`
//! and is meant to be visually unmistakable from a run that verified anything.
//!
//! # Env switches
//!
//! - `GROUND_TRUTH_REQUIRE=1` — absence, an unresolvable ID, or a missing
//!   `.calibration/` become failures. For the machine that holds the documents.
//! - `GROUND_TRUTH_REPORT=1` — report, never fail. Use it to read current
//!   numbers when *recording* a deviation; never to make a red run go away.
//!
//! # What the harness can prove without Word
//!
//! Three of the documents were authored by Word and carry Word's own arithmetic
//! in their bytes: `docProps/app.xml <Pages>` (Word's page count at last save)
//! and `w:lastRenderedPageBreak` (§17.3.3.13 — where Word's *renderer* broke the
//! page at last save). Those are asserted from the bytes on every checked entry,
//! which also self-checks the ID binding: a swapped document fails the hash, and
//! a re-saved one fails the corroboration.
//!
//! The break map is compared structurally, not by page number: each
//! `lastRenderedPageBreak` sits between two stretches of text that are
//! *adjacent* in document order (the marker itself contributes no text), so the
//! 60-character window spanning it must also appear in Parasma's rendered text,
//! and a Parasma page boundary must fall at exactly the same point inside it.
//! That works for an incomplete marker set (C1 has 15 markers for 21 pages) and
//! inside tables, where "text X starts page N" would not.
//!
//! # The font ceiling — read this before trusting a page count
//!
//! The font basis is the SHIPPING substitution set: `load_test_fonts` (Arimo +
//! XITS, the oracle basis) plus `load_substitute_fonts` (Carlito for Calibri,
//! Tinos for Times New Roman, real DejaVu Sans and Open Sans — the same
//! resolutions `font-resolver.ts` ships). Vendoring that set on 2026-07-27 is
//! what resolved C1's page count (20 → 21 = Word) and delivered the first two
//! FontCeiling verdicts. Fidelity beyond it is bounded by faces that CANNOT be
//! vendored (Word's own Segoe UI / Verdana substitutions), so a disagreement
//! here is still not automatically an engine defect. Deviations are
//! therefore classified: `Engine` (Parasma is wrong) vs `FontCeiling` (the
//! harness cannot render what Word rendered). Do not "fix" a `FontCeiling`
//! deviation by changing layout.

use crate::wml::page::{PageElement, PageLayout};
use crate::Viewer;
use std::io::Read;

// ─── the tracked manifest: C-IDs, hashes, Word's values. No filenames. ──────

#[derive(Clone, Copy)]
enum Confidence {
    /// Machine-verifiable from the document's own bytes, or a recorded,
    /// dated observation of the document open in desktop Word.
    High,
    /// A written observation only — someone recorded what they saw, and nothing
    /// in the bytes corroborates it.
    Medium,
    /// Nobody has said Word was consulted. Never asserted.
    Unverified,
}

/// `FontCeiling` gained its first members on 2026-07-27, when the shipping
/// substitution set was vendored and each deviation's prescribed experiment ran:
/// C1's page count resolved outright, C5's break map moved closer and hit the
/// unvendorable-faces wall (Word rendered it in Segoe UI/Verdana). `Engine` is
/// still unconstructed and that remains the finding, not dead code — no
/// deviation has yet been pinned on the engine. The two still-`Unattributed`
/// break maps name what would settle them in their `why`.
#[derive(Clone, Copy)]
#[allow(dead_code)]
enum DeviationClass {
    /// Parasma disagrees with Word and the engine is why.
    Engine,
    /// Parasma cannot reproduce Word's render because the faces Word used are
    /// not vendored. Not an engine defect; do not fix by changing layout.
    FontCeiling,
    /// Attribution not established. State what would settle it in `why`.
    Unattributed,
}

struct Deviation {
    /// What Parasma produces *today*, under the vendored font set. A PIN, not
    /// ground truth: it exists so drift is caught while the gap is open.
    parasma: u32,
    class: DeviationClass,
    why: &'static str,
}

/// Corroboration readable from the document's own bytes. Both are Word's own
/// output, written at last save, so they check the ID binding as a side effect.
struct InFile {
    /// `docProps/app.xml <Pages>`. `None` = the part has no `<Pages>`.
    app_pages: Option<u32>,
    /// Count of `w:lastRenderedPageBreak` in `word/document.xml` (§17.3.3.13).
    markers: u32,
}

struct Entry {
    id: &'static str,
    /// The document is not on any machine and no digest survives. Recorded so
    /// the measurements are not lost with it.
    lost: bool,
    /// Word's page count. `None` = nobody has opened it in Word (`NoExpectation`).
    word_pages: Option<u32>,
    /// file:line, or the date of the observation, the Word value comes from.
    /// Never a filename.
    source: &'static str,
    confidence: Confidence,
    /// Corroboration to assert from the bytes. `None` for `Lost`.
    in_file: Option<InFile>,
    /// Parasma's page count today, when it differs from `word_pages`.
    pages_deviation: Option<Deviation>,
    /// How many of the `lastRenderedPageBreak` windows Parasma breaks at
    /// exactly. Word's expectation is always "all of them"; this records how
    /// many it manages today, when that is fewer.
    breaks_deviation: Option<Deviation>,
    /// Faces Word rendered with, against the vendored Arimo + XITS set.
    fonts: &'static str,
    /// Word-observed facts that this harness cannot check mechanically. Printed
    /// every run so they stay visible rather than being lost to history.
    notes: &'static [&'static str],
}

/// Ground truth, by calibration ID. Everything here is a statement about **what
/// Word did**. Nothing here may be derived from a Parasma run except the
/// explicitly-labelled `Deviation` pins.
const MANIFEST: &[Entry] = &[
    Entry {
        id: "C1",
        lost: false,
        word_pages: Some(21),
        source: "docProps/app.xml <Pages>21</Pages> (Word 16.0, in-file); \
                 verified against desktop Word on 2026-06-20",
        confidence: Confidence::High,
        in_file: Some(InFile {
            app_pages: Some(21),
            markers: 15,
        }),
        // RESOLVED 2026-07-27 — the deviation recorded here (Parasma 20 vs Word
        // 21) prescribed its own experiment: vendor Carlito + Tinos and re-run;
        // "if it goes to 21, this is FontCeiling". It went to 21. The native
        // harness now loads the shipping substitution set (load_substitute_fonts)
        // and this document paginates identically to Word. Class: FontCeiling,
        // closed by closing the font gap — the engine was never wrong here.
        pages_deviation: None,
        breaks_deviation: Some(Deviation {
            parasma: 11,
            class: DeviationClass::Unattributed,
            why: "11 exact, 1 off by +17 characters, 3 windows unlocatable in Parasma's text \
                  (fields or symbol-font runs render differently, so the comparison — not the \
                  layout — fails there). UNCHANGED by the 2026-07-27 shipping-font-basis run \
                  that resolved the page count, so fonts are eliminated as the variable: what \
                  remains open is the text-extraction comparison for field/symbol runs, not \
                  attribution to a face. Note this is ELEVEN OF FIFTEEN with the page count \
                  now exact, so most of Word's break map is reproduced exactly.",
        }),
        fonts: "Arial, Calibri, Calibri Light, Segoe UI, Times New Roman",
        notes: &[
            "2026-06-20: in an hRule=\"exact\" header row, a NormalWeb title with 1em auto \
             before-spacing sits at the SAME level as the plain date cells — Word does not \
             push it below. (§17.4.80; fixed in wml/paragraph.rs.)",
            "2026-06-20: globally collapsing a table cell's first-paragraph auto before-spacing \
             gives 20pp, diverging from Word's 21 — which is why the 2026-06-14 and 2026-06-17 attempts \
             were both reverted. Do not re-try it.",
            "wml/paragraph.rs:388-397: sizing empty-paragraph marks from the mark rPr rather \
             than the style chain gives 20pp here. compatMode=15.",
        ],
    },
    Entry {
        id: "C2",
        lost: false,
        word_pages: Some(11),
        source: "docProps/app.xml <Pages>11</Pages> (Word 16.0, in-file); 2026-06-01",
        confidence: Confidence::High,
        in_file: Some(InFile {
            app_pages: Some(11),
            markers: 10,
        }),
        pages_deviation: None,
        breaks_deviation: Some(Deviation {
            parasma: 7,
            class: DeviationClass::Unattributed,
            why: "the page COUNT agrees with Word exactly, and 2 of Word's 10 breaks still \
                  land somewhere else entirely (nearest boundary −130 and +173 characters \
                  away); 1 window is unlocatable. Total agreement is therefore not evidence of \
                  distribution agreement — this entry is the proof of that. 2026-07-27: the \
                  prescribed Carlito+Tinos run happened (load_substitute_fonts) and changed \
                  NOTHING here, so Calibri/Times attribution is eliminated. Still open: \
                  Cambria→Caladea, Courier→Cousine, Inter, Montserrat and MS Gothic/Mincho \
                  are unvendored, so attribution is narrowed to those faces or the engine. \
                  SETTLE IT BY: vendoring Caladea + Cousine (both SIL OFL) and re-running.",
        }),
        fonts: "Calibri, Cambria, Times New Roman, Courier, Inter, Montserrat, MS Gothic/Mincho",
        notes: &[
            "2026-06-01: the Figure-1 inline image is declared 618.8px and must NOT be scaled to \
             711.7px by 1.15x auto line spacing — those 93px are what made this render 12pp \
             against Word's 11. Pinned by auto_spacing_does_not_scale_inline_image.",
            "2026-06-01: the document sets useFELayout, so Word does not apply its linePitch=360 \
             docGrid to Latin text.",
            "10 markers for an 11-page document is consistent with a COMPLETE Word break map, \
             but that Word writes one at every break is not proven.",
        ],
    },
    Entry {
        id: "C3",
        lost: false,
        // No page expectation. The 2026-07-19 observation says this document "renders 5pp" —
        // that is PARASMA's count. Word's contribution in that record is a BREAK
        // POSITION, not a total, and recording the count as Word's would be the
        // inversion this module exists to prevent. The break observation survives
        // in the notes; the number does not become ground truth by proximity.
        word_pages: None,
        source: "2026-07-19 — Word statement is a BREAK POSITION only, not a count",
        confidence: Confidence::Unverified,
        in_file: Some(InFile {
            app_pages: None,
            markers: 0,
        }),
        pages_deviation: None,
        breaks_deviation: None,
        fonts: "Open Sans (Word substitutes Segoe UI), DejaVu Sans (Word substitutes Verdana)",
        notes: &[
            "No <Pages> and zero lastRenderedPageBreak: nothing in the bytes corroborates 5, \
             and the break map cannot be checked at all.",
            "2026-07-19: Word's 4→5 break falls at \"Cras id eros sed enim…\". On the bundled \
             substitutes Parasma breaks at \"blandit sit amet tristique…\"; loading Segoe UI \
             (as Open Sans) + Verdana (as DejaVu Sans) reproduces Word's break exactly. That \
             observation is environment-bound — Segoe UI ships inside Office (CloudFonts) and \
             cannot be vendored, so this is permanently unreachable natively.",
        ],
    },
    Entry {
        id: "C4",
        lost: false,
        word_pages: Some(26),
        source: "2026-07-15 — the assertion \"matching Word's total\"; 2026-07-14",
        confidence: Confidence::Medium,
        in_file: Some(InFile {
            app_pages: Some(1),
            markers: 0,
        }),
        pages_deviation: None,
        breaks_deviation: None,
        fonts: "Arial, Arial Nova, Calibri, Calibri Light, Cambria, Courier New, Tahoma, \
                Times/Times New Roman, Wingdings",
        notes: &[
            "CORRECTED 2026-07-27 — the earlier source read \"'Page N of 26' out of the \
             document's own footer\". That is PARASMA's render, not Word's: the 2026-07-15 record says the \
             footer \"now reads 'Page N of 26' (NUMPAGES self-corrects)\" AFTER its own fix. \
             The document's CACHED field result is 20 and its <Pages> is 1; both are stale, \
             and zero lastRenderedPageBreak. So NOTHING in the bytes corroborates 26 — the \
             only support is the same record's separate claim that the total matched Word.",
            "2026-07-15: Word's middle body occupies p7–p16; Parasma flows roughly one page \
             ahead through that stretch even at the same total. Total page count agreeing is \
             therefore NOT evidence the distribution agrees.",
            "2026-07-15: the Appendix-D Heading1+keepNext heading rides with its table at the top \
             of Word's page 22. With Arimo Parasma now puts it on p22 y=132; with the Carlito \
             substitute it stays on p21 y=929 — the documented ~6.9% width drift.",
            "2026-07-15: the Appendix-E landscape figure has drawing extent 563.45px and text \
             descent 4.53px. Word measures that line at the extent, not extent+descent \
             (568.66) — the 5.21px tipped a page. Pinned by \
             auto_spacing_image_line_does_not_add_text_descent. (The \"Word measures the \
             extent\" reading is INFERRED from the page-count outcome, not observed directly.)",
            "2026-07-14, against Word's PDF, qualitative: borderless tables must not draw grid \
             lines; indented right-tab stops must not drop TOC leaders/page numbers; late \
             sections must inherit hdr/ftr bands.",
        ],
    },
    Entry {
        id: "C5",
        lost: false,
        word_pages: Some(5),
        source: "docProps/app.xml <Pages>5</Pages> (Word 16.0, in-file); 2026-06-02",
        confidence: Confidence::High,
        in_file: Some(InFile {
            app_pages: Some(5),
            markers: 3,
        }),
        pages_deviation: None,
        breaks_deviation: Some(Deviation {
            parasma: 0,
            class: DeviationClass::FontCeiling,
            why: "ZERO of Word's 3 breaks are reproduced, at a page count that matches Word \
                  exactly (5 = 5). All three windows are located unambiguously and none has a \
                  Parasma boundary in it: the nearest are +426, +568 and −998 characters away. \
                  This is the harness's sharpest result — the page count that every regression \
                  list in this project quotes says nothing at all about where the pages break. \
                  2026-07-27: the prescribed experiment ran — real DejaVu Sans + Open \
                  Sans now load (load_substitute_fonts) and the windows moved to 1 near + 2 \
                  missed: closer, not closed. Class FontCeiling: per the 2026-06-26 font list of \
                  Word's own PDF export, Word actually rendered this document in VERDANA and SEGOE UI (its \
                  substitutions for the declared faces), neither vendorable — Segoe UI ships \
                  only inside Office. Word's exact line widths are permanently unreachable \
                  natively AND in the shipping browser, which loads the same real faces this \
                  harness now does. Reclassify only if a metric-compatible Verdana/Segoe \
                  clone appears.",
        }),
        fonts: "DejaVu Sans, Open Sans;Arial, Liberation Sans;Arial, Liberation Serif, Aptos",
        notes: &[
            "THE line-breaking calibration. 2026-06-02, wml/linebreak.rs:1292-1305: Word's exact \
             line breaks L1–L9 of the calibration paragraph — \"luctus\" wraps, \"tellus\" \
             stays. The diverging word had 0.69px of slack and the next kept word 3.26px; a \
             half-space is ~2.2px and sits in that window, which is where trail_reserve = 0.5 \
             comes from. compatMode=12, so Word opens it in Compatibility Mode and applies \
             the legacy wrapTrailSpaces fitter.",
            "2026-06-02: a wrapSquare image at posOffset=-3810 EMU leaves a ~0.4px right sliver, \
             and wrapText=\"largest\" force-packed one word per line over the image. Word puts \
             the text underneath.",
        ],
    },
    Entry {
        id: "C6",
        lost: true,
        // The 2026-06-01 record names only the sample series, which does not distinguish this missing
        // smaller variant from the in-tree C3. An expectation that cannot be tied to
        // a specific document is not ground truth.
        word_pages: Some(5),
        source: "2026-06-01 (names only the sample series — document not distinguished from C3)",
        confidence: Confidence::Unverified,
        in_file: None,
        pages_deviation: None,
        breaks_deviation: None,
        fonts: "DejaVu Sans, Open Sans (Word substituted Segoe UI — confirmed by the font list \
                of Word's own PDF: no Open Sans embedded)",
        notes: &[
            "LOST — meaning the DOCUMENT is unavailable and no hash for it survives, \
             so nothing here can be re-measured. Its identity is known (a publicly \
             available sample document), so the \
             gap is the bytes, not the identity. Recorded so the numbers are not lost with it. \
             Do NOT invent a binding — an ID that looks resolvable and is not is worse than a \
             gap.",
            "2026-06-26: Word's Open Sans lines on page 3 measure 16.8pt; Parasma went 17.16 → \
             16.76pt and page 3 then matched Word line-for-line, 33 = 33, median −0.05pt, no \
             spill.",
            "2026-06-26: the font list of Word's PDF shows the body text is SegoeUI. Segoe UI's hhea \
             (1.3301em) is ~2.4% tighter than real Open Sans (1.3618em) — that is the whole \
             residual, not a line-height-model bug (font-resolver.ts:252-261).",
            "2026-06-27: page 4 image top Word 195.3 (Parasma 195.1); \"Maecenas\" paragraph top \
             Word 582.1 (Parasma 553 → 579.6; the ~2.5pt residual is Segoe-UI substitute \
             metrics).",
            "2026-06-26: page 4 text after the full-width image resumes 65.3pt below it in Word \
             (Parasma 3.5 → 38.5pt; the remaining ~27pt is the §17.3.1.29 mark-size deviation \
             deliberately left unfixed per C1).",
            "oracle_tests.rs:1284-1287: page-1 top spacing delta closed −16pt → −4pt; page-2 \
             table delta closed −10.6pt → 0pt.",
            "2026-06-01: a DejaVu Sans 10.5pt body paragraph is 9 lines in Word. Parasma gave 8 \
             with the Arimo substitute and 9 with the real DejaVu face (first four breaks \
             exact, later lines drift a word).",
            "2026-06-26: the embedded chart's data max is 9.6 and Word/Excel choose axis max 12, \
             not the bare nice-ceil 10 (chart.rs:437-442).",
            "⚠ IN-TREE CONTRADICTION, unresolved. The 2026-06-01 change used \"4 pages vs Word's 5\" here to \
             justify implementing docGrid linePitch. wml/section.rs now says a type=\"default\" \
             grid does NOT snap, that this renders 4pp, and that Word's 5th page is a \
             font-substitution artifact (uninstalled DejaVu → taller Verdana per the doc's \
             altName). Both cannot be the operative explanation. Word = 5 is recorded; \
             \"Parasma must produce 5\" is NOT.",
        ],
    },
    Entry {
        id: "C7",
        lost: false,
        word_pages: None,
        source: "none — see notes",
        confidence: Confidence::Unverified,
        in_file: Some(InFile {
            app_pages: None,
            markers: 0,
        }),
        pages_deviation: None,
        breaks_deviation: None,
        fonts: "unexamined",
        notes: &[
            "NO GROUND TRUTH. \"sample 3\" appears in ~10 regression lists from 2026-06-01 on, \
             but that 3 is PARASMA's own output quoted back at itself — no record says Word \
             was consulted, there is no <Pages>, and there are zero lastRenderedPageBreak. \
             Recording 3 here would be exactly the inversion this module exists to prevent.",
            "TO RESOLVE: open it in Word, record the page count and the source, and add \
             word_pages. Until then this entry deliberately asserts nothing.",
        ],
    },
];

/// SML and PML have **no numeric ground truth anywhere in the tree**. Every
/// Excel/PowerPoint mention under `sml/` and `pml/` (40+ sites) states the
/// application's *rule*, not a measurement. The one live-app comparison
/// (2026-06-09, against Excel) was entirely qualitative and its two
/// documents are gone. This is a finding, not a search failure.
const SML_PML_GROUND_TRUTH: &[Entry] = &[];

// ─── resolution ────────────────────────────────────────────────────────────

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../..")))
}

/// Roots a bare filename from the private manifest is looked up under. A
/// `"path"` field in the private manifest (absolute or repo-relative) takes
/// precedence over all of these.
const FIXTURE_ROOTS: &[&str] = &["fixtures/user", "fixtures", ".calibration"];

/// Pull one string field out of the object bound to `documents.<id>`, without a
/// JSON dependency. Brace-depth-scoped and string-aware, so a missing field
/// cannot silently pick up the next document's value.
fn private_field(json: &str, id: &str, field: &str) -> Option<String> {
    let key = format!("\"{id}\"");
    let mut at = json.find(&key)? + key.len();
    at += json[at..].find('{')?;
    let bytes = json.as_bytes();
    let (mut depth, mut i, mut in_str, mut esc) = (0usize, at, false, false);
    let mut obj_end = json.len();
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
        } else if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                obj_end = i;
                break;
            }
        }
        i += 1;
    }
    let obj = &json[at..obj_end];
    let fkey = format!("\"{field}\"");
    let fat = obj.find(&fkey)? + fkey.len();
    let rest = &obj[fat..];
    let colon = rest.find(':')? + 1;
    let rest = &rest[colon..];
    // A null value has no quotes, so `find('"')` would run on to the NEXT
    // field's KEY and return that as if it were this field's value — the LOST
    // row (`"file": null`) reported the literal string "sha256" as its filename.
    if rest.trim_start().starts_with("null") {
        return None;
    }
    let open = rest.find('"')? + 1;
    // Values in this manifest are plain filenames; no escape handling needed
    // beyond refusing to run past the closing quote.
    let close = rest[open..].find('"')?;
    Some(rest[open..open + close].to_string())
}

enum Resolved {
    Path(std::path::PathBuf),
    /// The private manifest has a row but the document is not on this machine.
    Absent,
    /// The private manifest has no row for this ID.
    NoRow,
}

fn resolve(private: &str, id: &str) -> Resolved {
    let root = repo_root();
    if let Some(p) = private_field(private, id, "path") {
        let path = std::path::Path::new(&p).to_path_buf();
        let path = if path.is_absolute() {
            path
        } else {
            root.join(&p)
        };
        return if path.exists() {
            Resolved::Path(path)
        } else {
            Resolved::Absent
        };
    }
    let Some(file) = private_field(private, id, "file") else {
        return Resolved::NoRow;
    };
    for r in FIXTURE_ROOTS {
        let path = root.join(r).join(&file);
        if path.exists() {
            return Resolved::Path(path);
        }
    }
    Resolved::Absent
}

// ─── reading Word's own arithmetic out of the package ──────────────────────

fn zip_part(bytes: &[u8], name: &str) -> Option<String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).ok()?;
    let mut f = zip.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

fn app_xml_pages(bytes: &[u8]) -> Option<u32> {
    let xml = zip_part(bytes, "docProps/app.xml")?;
    let at = xml.find("<Pages>")? + "<Pages>".len();
    let end = xml[at..].find("</Pages>")?;
    xml[at..at + end].trim().parse().ok()
}

/// Word's rendered text, plus the offset of each `w:lastRenderedPageBreak`
/// (§17.3.3.13) into it. Offsets are into the NORMALIZED text, so they line up
/// with `parasma_text`.
struct WordText {
    text: String,
    breaks: Vec<usize>,
}

/// Collapse whitespace runs (including NBSP) to one space. Both sides of every
/// comparison go through this, so a line break in Parasma's item stream and a
/// space in Word's XML compare equal.
fn push_normalized(out: &mut String, s: &str) {
    for ch in s.chars() {
        let ch = if ch.is_whitespace() || ch == '\u{a0}' {
            ' '
        } else {
            ch
        };
        if ch == ' ' && out.ends_with(' ') {
            continue;
        }
        out.push(ch);
    }
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn word_text(bytes: &[u8]) -> Option<WordText> {
    let xml = zip_part(bytes, "word/document.xml")?;
    let mut out = WordText {
        text: String::new(),
        breaks: Vec::new(),
    };
    let b = xml.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let Some(gt) = xml[i..].find('>') else { break };
        let tag = &xml[i + 1..i + gt];
        let name = tag.split([' ', '/', '\t', '\n']).next().unwrap_or("");
        if name == "w:lastRenderedPageBreak" {
            out.breaks.push(out.text.len());
            i += gt + 1;
            continue;
        }
        if name == "w:t" && !tag.ends_with('/') {
            let body_at = i + gt + 1;
            let Some(close) = xml[body_at..].find("</w:t>") else {
                break;
            };
            push_normalized(
                &mut out.text,
                &decode_entities(&xml[body_at..body_at + close]),
            );
            i = body_at + close + "</w:t>".len();
            continue;
        }
        i += gt + 1;
    }
    Some(out)
}

// ─── Parasma's side ────────────────────────────────────────────────────────

fn page_text_into(out: &mut String, page: &PageLayout) {
    for el in &page.elements {
        match el {
            PageElement::Line(l) => {
                for seg in &l.segments {
                    for it in &seg.items {
                        push_normalized(out, &it.text);
                    }
                }
            }
            PageElement::TableRow(row) => {
                for cell in &row.cells {
                    for l in &cell.lines {
                        for seg in &l.segments {
                            for it in &seg.items {
                                push_normalized(out, &it.text);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The whole body stream, plus the offset at which each page starts. Headers,
/// footers and footnote bands are excluded — they live in other package parts,
/// so `word/document.xml` has nothing to compare them against.
fn parasma_text(viewer: &Viewer) -> (String, Vec<usize>) {
    let pages = viewer.pages.borrow();
    let mut text = String::new();
    let mut starts = Vec::with_capacity(pages.len());
    for page in pages.iter() {
        starts.push(text.len());
        page_text_into(&mut text, page);
    }
    (text, starts)
}

// ─── the break-map comparison ──────────────────────────────────────────────

/// Characters taken either side of a marker to make the comparison window,
/// widest first. A wide window is unambiguous but sinks on a single run Parasma
/// renders differently (a field result, a symbol-font glyph); a narrow one
/// survives that but recurs in boilerplate. So: widest that resolves wins.
const WINDOWS: &[usize] = &[30, 18, 10];

enum BreakVerdict {
    /// A Parasma page starts at exactly the character Word broke at.
    Exact,
    /// The window was located and a page boundary falls inside it, but not at
    /// Word's character. Off by N characters of text (not pixels).
    Near(isize),
    /// The window was located and no Parasma page boundary is inside it —
    /// Parasma runs straight through a break Word made. Carries the offset of
    /// the *nearest* boundary, which says whether the layout is a hair out or a
    /// whole page adrift.
    NoBreak(isize),
    /// The window could not be located at any width. NOT counted against
    /// Parasma: it means the comparison failed, not the layout.
    Unresolved(&'static str),
}

/// Byte offset of the `n`-th occurrence (0-based), scanning from the left.
fn nth_occurrence(hay: &str, needle: &str, n: usize) -> Option<usize> {
    hay.match_indices(needle).nth(n).map(|(i, _)| i)
}

fn compare_break(word: &WordText, at: usize, ptext: &str, starts: &[usize]) -> BreakVerdict {
    let mut why = "window is blank on one side";
    for &w in WINDOWS {
        let before_start = word.text[..at]
            .char_indices()
            .rev()
            .nth(w)
            .map_or(0, |(i, _)| i);
        let after_end = word.text[at..]
            .char_indices()
            .nth(w)
            .map_or(word.text.len(), |(i, _)| at + i);
        let before = &word.text[before_start..at];
        let after = &word.text[at..after_end];
        if before.trim().is_empty() || after.trim().is_empty() {
            continue;
        }
        // The marker itself contributes no text, so `before` and `after` are
        // ADJACENT in Word's stream: the window is one contiguous string that
        // must also appear, contiguous, in Parasma's.
        let combined = format!("{before}{after}");
        // Boilerplate recurs, so identity is by ORDINAL, not by uniqueness: the
        // k-th occurrence in Word's text is compared against the k-th in
        // Parasma's. Two streams of the same document agree on that ordering.
        let ordinal = word
            .text
            .match_indices(&combined)
            .take_while(|(i, _)| *i < before_start)
            .count();
        let Some(found) = nth_occurrence(ptext, &combined, ordinal) else {
            why = "window not present in Parasma's text at the same ordinal";
            continue;
        };
        let want = found + before.len();
        let (lo, hi) = (found, found + combined.len());
        let nearest = starts
            .iter()
            .copied()
            .filter(|&s| s > 0)
            .min_by_key(|&s| s.abs_diff(want))
            .map_or(0, |s| s as isize - want as isize);
        return match starts
            .iter()
            .copied()
            .filter(|&s| s > lo && s < hi)
            .min_by_key(|&s| s.abs_diff(want))
        {
            Some(s) if s == want => BreakVerdict::Exact,
            Some(s) => BreakVerdict::Near(s as isize - want as isize),
            None => BreakVerdict::NoBreak(nearest),
        };
    }
    BreakVerdict::Unresolved(why)
}

// ─── SHA-256 (FIPS 180-4) ──────────────────────────────────────────────────
//
// Hand-rolled because the crate has no hash dependency and this is the only
// caller. Test-only: nothing here reaches a wasm build. Self-checked against
// the FIPS vectors by `sha256_matches_published_vectors` below — an unverified
// hash function would make the identity check worse than none.

mod sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    pub(super) fn hex(data: &[u8]) -> String {
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut msg = data.to_vec();
        let bit_len = (data.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());

        let mut w = [0u32; 64];
        for chunk in msg.chunks_exact(64) {
            for (i, word) in chunk.chunks_exact(4).enumerate() {
                w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
            let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
                *slot = slot.wrapping_add(v);
            }
        }
        h.iter().map(|v| format!("{v:08x}")).collect()
    }
}

/// FIPS 180-4 published vectors. If this fails, every identity check in this
/// module is meaningless — which is exactly why the hash is not trusted unchecked.
#[test]
fn sha256_matches_published_vectors() {
    assert_eq!(
        sha256::hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256::hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256::hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

/// The window comparison must be able to FAIL, or its green means nothing. Each
/// verdict is provoked deliberately over a synthetic stream.
#[test]
fn break_comparison_distinguishes_every_verdict() {
    // 12 distinct 40-char paragraphs: long enough that a 60-char window is
    // unambiguous, and every character is placed on purpose.
    let text: String = (0..12)
        .map(|i| format!("paragraph {i:02} of the synthetic ground truth "))
        .collect();
    let word = WordText {
        text: text.clone(),
        breaks: vec![160, 280],
    };

    // Parasma broke at exactly 160 → Exact. It also broke at 300, twenty
    // characters past Word's 280 but still inside that window → Near(+20).
    let starts = vec![0usize, 160, 300];
    assert!(matches!(
        compare_break(&word, 160, &text, &starts),
        BreakVerdict::Exact
    ));
    assert!(matches!(
        compare_break(&word, 280, &text, &starts),
        BreakVerdict::Near(20)
    ));

    // No boundary anywhere near 280 → NoBreak (the layout ran straight through).
    assert!(matches!(
        compare_break(&word, 280, &text, &[0, 160]),
        BreakVerdict::NoBreak(-120)
    ));

    // Parasma never rendered this text at all → Unresolved, and NOT counted as
    // a Parasma failure.
    assert!(matches!(
        compare_break(&word, 160, "something else entirely", &[0]),
        BreakVerdict::Unresolved(_)
    ));

    // Ordinal disambiguation: a stream where every paragraph is identical, so
    // every window recurs. The 3rd break must still resolve to the 3rd copy.
    let rep: String = std::iter::repeat_n("lorem ipsum dolor sit amet consectetur ", 12).collect();
    let repw = WordText {
        text: rep.clone(),
        breaks: vec![117],
    };
    assert!(matches!(
        compare_break(&repw, 117, &rep, &[0, 117]),
        BreakVerdict::Exact
    ));
    assert!(matches!(
        compare_break(&repw, 117, &rep, &[0, 78]),
        BreakVerdict::NoBreak(-39)
    ));
}

// ─── the harness ───────────────────────────────────────────────────────────

#[derive(PartialEq, Eq)]
enum Outcome {
    Checked,
    Absent,
    Unresolvable,
    Lost,
    NoExpectation,
}

#[test]
fn word_ground_truth() {
    let require = std::env::var("GROUND_TRUTH_REQUIRE").is_ok_and(|v| v != "0");
    let report_only = std::env::var("GROUND_TRUTH_REPORT").is_ok_and(|v| v != "0");
    // Failures GROUND_TRUTH_REPORT may suppress, and failures it may not.
    let mut hard_fail: Vec<String> = Vec::new();
    let mut out = String::new();
    let mut fail: Vec<String> = Vec::new();
    let mut counts = [0usize; 5];

    let priv_path = repo_root().join(".calibration/manifest.private.json");
    let private = std::fs::read_to_string(&priv_path).ok();
    out.push_str("── Word ground-truth harness ──────────────────────────────────\n");
    match &private {
        Some(_) => out.push_str(&format!("private manifest: {}\n", priv_path.display())),
        None => {
            out.push_str(&format!(
                "private manifest: ABSENT ({}) — every C-ID is unresolvable on this machine\n",
                priv_path.display()
            ));
            if require {
                fail.push(format!(
                    "GROUND_TRUTH_REQUIRE=1 but {} is absent",
                    priv_path.display()
                ));
            }
        }
    }
    out.push_str(
        "font basis: Arimo + XITS Math + Carlito/Tinos/DejaVu Sans/Open Sans (the shipping \
         substitution set — see load_substitute_fonts). No Word observation below \
         was made against it — read the module's font-ceiling note.\n\n",
    );

    for e in MANIFEST.iter().chain(SML_PML_GROUND_TRUTH) {
        let resolved = match (&private, e.lost) {
            (_, true) => Resolved::NoRow,
            (Some(p), false) => resolve(p, e.id),
            (None, false) => Resolved::NoRow,
        };
        let outcome = match (&resolved, e.lost) {
            (_, true) => Outcome::Lost,
            (Resolved::Path(_), _) if e.word_pages.is_none() => Outcome::NoExpectation,
            (Resolved::Path(_), _) => Outcome::Checked,
            (Resolved::Absent, _) => Outcome::Absent,
            (Resolved::NoRow, _) => Outcome::Unresolvable,
        };
        let tag = match outcome {
            Outcome::Checked => "CHECKED",
            Outcome::Absent => "ABSENT",
            Outcome::Unresolvable => "UNRESOLVABLE",
            Outcome::Lost => "LOST",
            Outcome::NoExpectation => "NO-EXPECTATION",
        };
        let conf = match e.confidence {
            Confidence::High => "HIGH",
            Confidence::Medium => "MED",
            Confidence::Unverified => "UNVERIFIED",
        };
        out.push_str(&format!(
            "{} [{tag}] Word pages: {}  confidence {conf}\n     source: {}\n     fonts:  {}\n",
            e.id,
            e.word_pages
                .map_or("— none recorded —".to_string(), |p| p.to_string()),
            e.source,
            e.fonts,
        ));

        match outcome {
            Outcome::Absent => {
                if require {
                    fail.push(format!("{}: ABSENT and GROUND_TRUTH_REQUIRE=1", e.id));
                }
            }
            Outcome::Unresolvable => {
                if require {
                    fail.push(format!(
                        "{}: no row in the private manifest and GROUND_TRUTH_REQUIRE=1 — \
                         an ID that looks resolvable and is not",
                        e.id
                    ));
                }
            }
            Outcome::Lost => {}
            Outcome::NoExpectation => {
                // The entry asserts no page count, but it still CLAIMS to be bound
                // to a particular document. Skipping the hash here let C7 be bound
                // to the wrong file and stay green — which contradicts the
                // "sha256 mismatch is always red" row in this module's own table.
                if let Resolved::Path(path) = &resolved {
                    let expected = private.as_deref().and_then(|p| private_field(p, e.id, "sha256"));
                    match std::fs::read(path) {
                        Ok(bytes) => {
                            let got = sha256::hex(&bytes);
                            if expected.is_some() && expected.as_deref() != Some(got.as_str()) {
                                hard_fail.push(format!(
                                    "{}: sha256 MISMATCH on a NO-EXPECTATION entry — it is \
                                     bound to a document it was never recorded against.\n       \
                                     manifest {}\n       actual   {got}",
                                    e.id,
                                    expected.as_deref().unwrap_or("<none>"),
                                ));
                            } else {
                                out.push_str("     sha256: OK\n");
                            }
                        }
                        Err(err) => out.push_str(&format!("     sha256: unreadable ({err})\n")),
                    }
                }
            }
            Outcome::Checked => {
                let Resolved::Path(path) = &resolved else {
                    unreachable!()
                };
                let bytes = match std::fs::read(path) {
                    Ok(b) => b,
                    Err(err) => {
                        fail.push(format!("{}: resolved but unreadable: {err}", e.id));
                        continue;
                    }
                };
                // 1. Identity. A DIFFERENT document under this ID is worse than
                //    no document, and is exactly what a skip would hide. The
                //    digest comes from the private manifest, beside the filename
                //    it binds.
                let expected = private.as_deref().and_then(|p| private_field(p, e.id, "sha256"));
                let got = sha256::hex(&bytes);
                if expected.as_deref() != Some(got.as_str()) {
                    // NOT suppressible by GROUND_TRUTH_REPORT: a mismatch means the
                    // ID is bound to the WRONG document, so every expectation under
                    // it is being compared against something it was never measured
                    // on. That is worse than absence — absence at least knows it
                    // checked nothing. Reported separately from `fail` so the
                    // report-only path cannot swallow it.
                    hard_fail.push(format!(
                        "{}: sha256 MISMATCH — the document bound to this ID is not the one \
                         these expectations were measured on.\n       manifest {}\n       actual   {got}",
                        e.id,
                        expected.as_deref().unwrap_or("<none>"),
                    ));
                    continue;
                }
                out.push_str("     sha256: OK\n");

                // 2. Word's own arithmetic, from the bytes.
                let word = word_text(&bytes);
                if let Some(inf) = &e.in_file {
                    let app = app_xml_pages(&bytes);
                    if app != inf.app_pages {
                        fail.push(format!(
                            "{}: docProps/app.xml <Pages> is {:?}, manifest records {:?}",
                            e.id, app, inf.app_pages
                        ));
                    }
                    let markers = word.as_ref().map_or(0, |w| w.breaks.len() as u32);
                    if markers != inf.markers {
                        fail.push(format!(
                            "{}: {markers} w:lastRenderedPageBreak in word/document.xml, \
                             manifest records {}",
                            e.id, inf.markers
                        ));
                    }
                    out.push_str(&format!(
                        "     in-file: app.xml <Pages>={}  lastRenderedPageBreak={markers}\n",
                        inf.app_pages.map_or("—".into(), |p| p.to_string()),
                    ));
                }

                // 3. Lay it out and compare against Word.
                let mut viewer = Viewer::new().expect("Viewer::new");
                crate::oracle_tests::load_test_fonts(&mut viewer);
                load_substitute_fonts(&mut viewer);
                if viewer.load_docx(bytes).is_err() {
                    fail.push(format!("{}: load_docx failed", e.id));
                    continue;
                }
                if let Err(err) = viewer.run_layout_native() {
                    fail.push(format!("{}: run_layout_native failed: {err:?}", e.id));
                    continue;
                }
                let pages = viewer.page_count();
                let want = e.word_pages.expect("Checked implies word_pages");
                out.push_str(&format!("     pages:  Word {want} | Parasma {pages} → "));
                if pages == want {
                    out.push_str("MATCH\n");
                    if let Some(d) = &e.pages_deviation {
                        fail.push(format!(
                            "{}: pages now AGREE with Word ({want}) but a deviation of {} is \
                             still recorded. Delete the deviation — a stale one hides the next \
                             regression.",
                            e.id, d.parasma
                        ));
                    }
                } else {
                    match &e.pages_deviation {
                        Some(d) if d.parasma == pages => {
                            out.push_str(&format!(
                                "KNOWN DEVIATION ({})\n            why: {}\n",
                                class_name(d.class),
                                d.why
                            ));
                        }
                        Some(d) => fail.push(format!(
                            "{}: Word says {want}, Parasma says {pages}, and the recorded \
                             deviation is {}. Neither agreement nor the known gap — something \
                             drifted.",
                            e.id, d.parasma
                        )),
                        None => fail.push(format!(
                            "{}: Word says {want} pages, Parasma renders {pages}, and no \
                             deviation is recorded. This is a real disagreement with Word.",
                            e.id
                        )),
                    }
                }

                // 4. Word's break map, window by window.
                let Some(word) = word else { continue };
                if word.breaks.is_empty() {
                    out.push_str(
                        "     breaks: none — this document carries no lastRenderedPageBreak, \
                         so Word's break positions cannot be checked from the bytes\n",
                    );
                } else {
                    let (ptext, starts) = parasma_text(&viewer);
                    let (mut exact, mut near, mut nobreak, mut unres) = (0u32, 0u32, 0u32, 0u32);
                    let mut detail = String::new();
                    for (n, &at) in word.breaks.iter().enumerate() {
                        match compare_break(&word, at, &ptext, &starts) {
                            BreakVerdict::Exact => exact += 1,
                            BreakVerdict::Near(d) => {
                                near += 1;
                                detail.push_str(&format!(
                                    "            #{n}: page boundary {d:+} bytes off Word's\n"
                                ));
                            }
                            BreakVerdict::NoBreak(d) => {
                                nobreak += 1;
                                detail.push_str(&format!(
                                    "            #{n}: Parasma does not break here — nearest \
                                     boundary {d:+} bytes away\n"
                                ));
                            }
                            BreakVerdict::Unresolved(r) => {
                                unres += 1;
                                detail.push_str(&format!("            #{n}: unresolved — {r}\n"));
                            }
                        }
                    }
                    let total = word.breaks.len() as u32;
                    out.push_str(&format!(
                        "     breaks: Word {total} | exact {exact}, near {near}, missed \
                         {nobreak}, unresolved {unres} → "
                    ));
                    if exact == total {
                        out.push_str("MATCH\n");
                        if let Some(d) = &e.breaks_deviation {
                            fail.push(format!(
                                "{}: all {total} break windows now match Word but a deviation \
                                 of {} is still recorded. Delete it.",
                                e.id, d.parasma
                            ));
                        }
                    } else {
                        match &e.breaks_deviation {
                            Some(d) if d.parasma == exact => out.push_str(&format!(
                                "KNOWN DEVIATION ({})\n            why: {}\n",
                                class_name(d.class),
                                d.why
                            )),
                            Some(d) => fail.push(format!(
                                "{}: {exact}/{total} break windows match Word; the recorded \
                                 deviation is {}. Something drifted.",
                                e.id, d.parasma
                            )),
                            None => fail.push(format!(
                                "{}: only {exact}/{total} of Word's break windows match and no \
                                 deviation is recorded. This is a real disagreement with Word.",
                                e.id
                            )),
                        }
                        out.push_str(&detail);
                    }
                }
            }
        }

        for n in e.notes {
            out.push_str(&format!("     · {n}\n"));
        }
        out.push('\n');
        counts[match outcome {
            Outcome::Checked => 0,
            Outcome::Absent => 1,
            Outcome::Unresolvable => 2,
            Outcome::Lost => 3,
            Outcome::NoExpectation => 4,
        }] += 1;
    }

    let checked = counts[0];
    out.push_str(&format!(
        "GROUND-TRUTH SUMMARY: {checked} checked, {} absent, {} unresolvable, {} lost, \
         {} present-but-never-opened-in-Word\n",
        counts[1], counts[2], counts[3], counts[4],
    ));
    if checked == 0 {
        out.push_str(
            "⚠ GROUND-TRUTH: 0 CHECKED. Nothing in this run compared anything to Word. A green \
             result here means the harness found no documents, NOT that the engine agrees with \
             Word. Set GROUND_TRUTH_REQUIRE=1 on the machine that holds them.\n",
        );
    }
    out.push_str(
        "SML/PML: no numeric Excel or PowerPoint ground truth exists anywhere in the tree — \
         see SML_PML_GROUND_TRUTH.\n",
    );

    // A PASSING test's stdout is swallowed by libtest unless `--nocapture`, so a
    // `println!` here would make the whole report — including the 0-CHECKED
    // banner — invisible on exactly the runs that need it: a clean checkout, or
    // CI, where every document is absent and the harness silently compares
    // nothing. The report is therefore always written to a FILE, and the gate
    // (`scripts/ci-local.sh`) prints its summary line on every run. Visibility
    // lives where somebody actually looks, not behind a flag they must know.
    let report_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/ground-truth-report.txt");
    if let Some(dir) = report_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&report_path, out.as_bytes());
    println!("{out}");

    if !hard_fail.is_empty() {
        let mut msg = out;
        msg.push_str(&format!(
            "\n{} UNSUPPRESSIBLE FAILURE(S) — GROUND_TRUTH_REPORT does not apply:\n",
            hard_fail.len()
        ));
        for f in &hard_fail {
            msg.push_str(&format!("  ✗ {f}\n"));
        }
        panic!("{msg}");
    }
    if fail.is_empty() || report_only {
        if report_only && !fail.is_empty() {
            let mut extra = String::from(&out);
            extra.push_str(&format!(
                "\nGROUND_TRUTH_REPORT=1 — {} failure(s) suppressed:\n",
                fail.len()
            ));
            for f in &fail {
                extra.push_str(&format!("  ✗ {f}\n"));
            }
            let _ = std::fs::write(&report_path, extra.as_bytes());
            println!("{extra}");
        }
        return;
    }
    let mut msg = out;
    msg.push_str(&format!("\n{} GROUND-TRUTH FAILURE(S):\n", fail.len()));
    for f in &fail {
        msg.push_str(&format!("  ✗ {f}\n"));
    }
    msg.push_str(
        "\nDo NOT resolve a failure by editing the expectation to match Parasma. Either fix \
         the engine, or record the gap as a Deviation with its class and its `why`.\n",
    );
    panic!("{msg}");
}


/// The shipping font basis, natively. `load_test_fonts` (Arimo + XITS — the
/// ORACLE basis, deliberately untouched so every pinned baseline keeps its
/// meaning) is extended here with the four families the calibration documents
/// declare, resolved exactly as the browser resolves them (`font-resolver.ts`
/// FONT_URLS + font-map.json): Carlito for Calibri, Tinos for Times New Roman,
/// and the REAL DejaVu Sans / Open Sans since both are libre. The aliases
/// mirror font-map.json's `mappings` — same substitution, same direction.
/// Fetched sha-pinned by `scripts/fetch-test-fonts.sh`.
fn load_substitute_fonts(viewer: &mut Viewer) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fonts");
    for (family, stem, bold_stem, italic_stem, bold_italic_stem) in [
        ("Carlito", "Carlito-Regular", "Carlito-Bold", "Carlito-Italic", "Carlito-BoldItalic"),
        ("Tinos", "Tinos-Regular", "Tinos-Bold", "Tinos-Italic", "Tinos-BoldItalic"),
        ("DejaVu Sans", "DejaVuSans", "DejaVuSans-Bold", "DejaVuSans-Oblique",
         "DejaVuSans-BoldOblique"),
        ("Open Sans", "OpenSans-Regular", "OpenSans-Bold", "OpenSans-Italic",
         "OpenSans-BoldItalic"),
    ] {
        for (stem, weight, italic) in [
            (stem, 400u16, false),
            (bold_stem, 700, false),
            (italic_stem, 400, true),
            (bold_italic_stem, 700, true),
        ] {
            let path = format!("{dir}/{stem}.ttf");
            let bytes = std::fs::read(&path).unwrap_or_else(|_| {
                panic!("test font missing: {path}\n  run: ./scripts/fetch-test-fonts.sh")
            });
            if viewer.load_font(family, bytes, weight, italic).is_err() {
                panic!("load_font failed for {stem}");
            }
        }
    }
    // font-map.json mappings, restricted to families the calibration docs use.
    for (from, to) in [
        ("Arial", "Arimo"),
        ("Arial Nova", "Arimo"),
        ("Tahoma", "Arimo"),
        ("Calibri", "Carlito"),
        ("Calibri Light", "Carlito"),
        ("Times New Roman", "Tinos"),
        ("Times", "Tinos"),
    ] {
        viewer.add_font_alias(from, to);
    }
}

fn class_name(c: DeviationClass) -> &'static str {
    match c {
        DeviationClass::Engine => "engine",
        DeviationClass::FontCeiling => "font ceiling",
        DeviationClass::Unattributed => "UNATTRIBUTED",
    }
}
