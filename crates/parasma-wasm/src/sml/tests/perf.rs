//! Grid v2 — synthetic stamped-row fixture GENERATOR + native perf harness
//! (equivalence pins and perf gates for the lazy model).
//!
//! NO BINARIES IN GIT: the fixture is committed as this GENERATOR
//! rather than as a binary; the produced `.xlsx` is built on demand
//! (in-memory for native tests, or written under `target/` — gitignored — by
//! the `--ignored` writer test for browser perf checks). The fixture stands in
//! for large real-world workbooks, which are not distributed with the repo.
//!
//! The edge-case inventory baked into the generated sheet:
//!   - stamped `<dimension>` `A1:J{rows}` covering EVERY row (extent fidelity),
//!   - ~1M physically-present EMPTY stamped rows (`<row r="N"/>`, self-closing —
//!     the .NET-exporter class that makes the eager path O(sheet)),
//!   - ~200 scattered value rows (deterministic LCG scatter),
//!   - custom heights §18.3.1.73 (`ht` + `customHeight`) including an EMPTY
//!     custom-height row (a height exception with no cells),
//!   - a wrap-text cell WITHOUT `ht` (auto row height from wrapping, xf 1),
//!   - an r-less row run §18.3.1.73 (implied previous+1 numbering),
//!   - merges §18.3.1.55 including one spanning the 256-row LRU-chunk boundary
//!     (A255:A258 — the merged-cells-across-chunk case in window_tests)
//!     and one whose covered row is a bare stamped row,
//!   - a bottom-right SENTINEL at `J{rows}` (scroll-to-end browser smoke).
//!
//! Perf harness: `measure_baseline` wraps native timers around the exact
//! surfaces Grid v2 changed — `from_xlsx` (load: the pre-scan + husk parse),
//! first `grid_extent` (closed-form off the index), the bottom-window
//! geometry query (the frame-paint path's native analog: `visible_row_bands` +
//! `visible_cells` over a windowed page), and `grid_cell_at` (closed-form). The
//! perf gates are asserted by the `--ignored` 1M tests below.
//! Run the 1M tests explicitly, release-profiled:
//!   cargo test -p parasma-wasm --features sml --release -- --ignored perf_
//!   cargo test -p parasma-wasm --features sml --release -- --ignored write_stamped
//!
//! Baseline of the EAGER path Grid v2 replaced, recorded 2026-06-10 (native
//! release, one development machine; browser/wasm paid this on the MAIN THREAD and slower):
//!   generate 141 ms (2.6 MB .xlsx) | from_xlsx 97.8 ms | grid_extent(first)
//!   56.6 ms | bottom-window geometry 0.002 ms | grid_cell_at 0.34 ms |
//!   extent 921×20,971,660 px (1M rows ≈ 21 M px — the spacer-cap case).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{Cursor, Write as _};
use std::time::Instant;

use crate::engine::fonts::FontCache;
use crate::sml::grid;
use crate::sml::SmlRenderer;

use super::calibri_font_cache;

/// The full Excel row space — the 1M perf fixture's row count.
pub(super) const STAMPED_1M_ROWS: u32 = 1_048_576;
/// CI-fast variant: small enough to run un-ignored, large enough to span
/// several 256-row chunks and contain the whole edge-case inventory (which needs
/// rows ≥ 2048: the wrap row sits at 2000).
pub(super) const STAMPED_CI_ROWS: u32 = 2048;

// ─── generator ────────────────────────────────────────────────────────────────

const STAMPED_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>"#;

const STAMPED_ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;

const STAMPED_WORKBOOK: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Stamped" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;

const STAMPED_WORKBOOK_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

/// xf 0 = default; xf 1 = wrapText (the wrap-without-ht row, auto height).
const STAMPED_STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>
  <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
  <borders count="1"><border/></borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  <cellXfs count="2">
    <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>
    <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0" applyAlignment="1"><alignment wrapText="1"/></xf>
  </cellXfs>
</styleSheet>"#;

/// SST: 0=Alpha 1=Beta 2=SENTINEL 3=the long wrap text.
const STAMPED_SST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="4" uniqueCount="4"><si><t>Alpha</t></si><si><t>Beta</t></si><si><t>SENTINEL</t></si><si><t>This wrap-text cell has no ht attribute so the row height must come from wrapping this deliberately long sentence onto several lines.</t></si></sst>"#;

/// 1-based column number → A1 letters + row → full ref (tiny local copy to keep
/// the generator self-contained; ≤ J here so single-letter always).
fn cell_ref(col: u32, row: u32) -> String {
    format!("{}{row}", grid::col_letters(col))
}

/// Deterministic ~200-row scatter in `[16, rows-16)` (LCG — no rand dep).
fn scattered_rows(rows: u32) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    let mut s: u64 = 0x9e37_79b9_7f4a_7c15;
    while out.len() < 200 {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        out.insert(16 + ((s >> 33) as u32) % (rows - 32));
    }
    out
}

/// Build the stamped fixture as in-memory `.xlsx` bytes. `rows` ≥ 2048.
pub(super) fn stamped_xlsx(rows: u32) -> Vec<u8> {
    assert!(rows >= STAMPED_CI_ROWS, "inventory placement needs rows >= 2048");
    let half = rows / 2;
    let scattered = scattered_rows(rows);

    // Worksheet XML — emitted row-ascending so the r-less run (1000..=1004)
    // implies the right numbers from explicit row 999.
    let mut ws = String::with_capacity(rows as usize * 16 + 4096);
    ws.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">");
    let _ = write!(ws, "<dimension ref=\"A1:J{rows}\"/>");
    ws.push_str("<sheetFormatPr defaultRowHeight=\"15\"/>");
    ws.push_str("<cols><col min=\"1\" max=\"1\" width=\"12\" customWidth=\"1\"/><col min=\"10\" max=\"10\" width=\"14\" customWidth=\"1\"/></cols>");
    ws.push_str("<sheetData>");
    for r in 1..=rows {
        if r <= 8 {
            // Dense header block: numbers across A..J.
            let _ = write!(ws, "<row r=\"{r}\">");
            for col in 1..=10u32 {
                let _ = write!(ws, "<c r=\"{}\" t=\"n\"><v>{}</v></c>", cell_ref(col, r), r * 100 + col);
            }
            ws.push_str("</row>");
        } else if r == 10 {
            // Custom height WITH content.
            let _ = write!(
                ws,
                "<row r=\"10\" ht=\"30\" customHeight=\"1\"><c r=\"A10\" t=\"s\"><v>0</v></c><c r=\"B10\" t=\"n\"><v>10</v></c></row>"
            );
        } else if (255..=258).contains(&r) {
            // The chunk-boundary merge A255:A258 (anchor 255; covered cells
            // present-but-empty, the shape Excel itself writes) + B/C numbers.
            let a = if r == 255 {
                "<c r=\"A255\" t=\"s\"><v>1</v></c>".to_string()
            } else {
                format!("<c r=\"A{r}\"/>")
            };
            let _ = write!(
                ws,
                "<row r=\"{r}\">{a}<c r=\"B{r}\" t=\"n\"><v>{r}</v></c><c r=\"C{r}\" t=\"n\"><v>{}</v></c></row>",
                r * 2
            );
        } else if r == 999 {
            let _ = write!(ws, "<row r=\"999\"><c r=\"A999\" t=\"n\"><v>999</v></c></row>");
        } else if (1000..=1004).contains(&r) {
            // R-LESS run (§18.3.1.73): no @r — implied previous+1 from row 999.
            let _ = write!(
                ws,
                "<row><c r=\"A{r}\" t=\"inlineStr\"><is><t>implied {r}</t></is></c></row>"
            );
        } else if r == 2000 {
            // Wrap text WITHOUT ht — auto row height from wrapping (xf 1).
            ws.push_str("<row r=\"2000\"><c r=\"B2000\" s=\"1\" t=\"s\"><v>3</v></c></row>");
        } else if r == half {
            // Custom height + the mid-sheet merge anchor C{half}:E{half+1}.
            let _ = write!(
                ws,
                "<row r=\"{r}\" ht=\"45\" customHeight=\"1\"><c r=\"C{r}\" t=\"s\"><v>0</v></c><c r=\"F{r}\" t=\"n\"><v>{r}</v></c></row>"
            );
        } else if r == rows - 6 {
            // Custom-height EMPTY row (a height exception with no cells).
            let _ = write!(ws, "<row r=\"{r}\" ht=\"60\" customHeight=\"1\"/>");
        } else if r >= rows - 3 {
            // Bottom block: H..J numbers; merge H{rows-2}:J{rows-1} anchored at
            // H{rows-2}; the SENTINEL at J{rows}.
            let _ = write!(ws, "<row r=\"{r}\">");
            if r == rows {
                let _ = write!(ws, "<c r=\"A{r}\" t=\"n\"><v>{r}</v></c><c r=\"J{r}\" t=\"s\"><v>2</v></c>");
            } else {
                for col in 8..=10u32 {
                    let _ = write!(ws, "<c r=\"{}\" t=\"n\"><v>{}</v></c>", cell_ref(col, r), r + col);
                }
            }
            ws.push_str("</row>");
        } else if scattered.contains(&r) {
            // Scattered single value: column from the row number, A..J.
            let col = (r % 10) + 1;
            let _ = write!(ws, "<row r=\"{r}\"><c r=\"{}\" t=\"n\"><v>{r}</v></c></row>", cell_ref(col, r));
        } else {
            // The ~1M empty STAMPED rows — physically present, self-closing.
            let _ = write!(ws, "<row r=\"{r}\"/>");
        }
    }
    ws.push_str("</sheetData>");
    let _ = write!(
        ws,
        "<mergeCells count=\"4\"><mergeCell ref=\"B2:D3\"/><mergeCell ref=\"A255:A258\"/><mergeCell ref=\"C{half}:E{}\"/><mergeCell ref=\"H{}:J{}\"/></mergeCells>",
        half + 1,
        rows - 2,
        rows - 1
    );
    ws.push_str("</worksheet>");

    // Deflated zip (the realistic OOXML compression class — `from_xlsx` timing
    // must include real inflation). zip 7's default options are deterministic
    // (fixed 1980 DOS timestamp), so the generator output is byte-stable.
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, content) in [
        ("[Content_Types].xml", STAMPED_CONTENT_TYPES),
        ("_rels/.rels", STAMPED_ROOT_RELS),
        ("xl/workbook.xml", STAMPED_WORKBOOK),
        ("xl/_rels/workbook.xml.rels", STAMPED_WORKBOOK_RELS),
        ("xl/styles.xml", STAMPED_STYLES),
        ("xl/sharedStrings.xml", STAMPED_SST),
        ("xl/worksheets/sheet1.xml", ws.as_str()),
    ] {
        zw.start_file(name, opts).expect("zip start_file");
        zw.write_all(content.as_bytes()).expect("zip write");
    }
    zw.finish().expect("zip finish").into_inner()
}

// ─── native perf harness ─────────────────────────────────────────────────────

/// Wall-clock timings (ms) around the surfaces Grid v2 changed.
#[derive(Debug)]
pub(super) struct BaselineTimings {
    /// `SmlRenderer::from_xlsx` — load (pre-scan + husk parse per sheet).
    pub(super) from_xlsx_ms: f64,
    /// First `grid_extent` — closed-form off the index.
    pub(super) extent_ms: f64,
    /// Bottom-window geometry query (`visible_row_bands` + `visible_cells`) —
    /// the native analog of the frame-phase paint of a scrolled-to window.
    pub(super) window_ms: f64,
    /// `grid_cell_at` near the bottom-right corner — closed-form.
    pub(super) cell_at_ms: f64,
    /// The reported full grid extent (w, h) px, gutters included.
    pub(super) extent: (f64, f64),
    /// Cells intersecting the probed bottom window.
    pub(super) window_cells: usize,
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// Time the grid surfaces over one workbook. Viewport analog: 1280×800.
pub(super) fn measure_baseline(bytes: Vec<u8>, fc: &FontCache) -> BaselineTimings {
    let t = Instant::now();
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
    let from_xlsx_ms = ms(t);

    let t = Instant::now();
    let extent = r.grid_extent(0, fc).expect("grid_extent");
    let extent_ms = ms(t);

    // Scroll-to-bottom window (content-space): the last ~800px of the sheet.
    // Grid v2: index → window rows → chunk materialization → windowed page
    // (frame + fill, the full cold-scroll path the 100ms gate covers).
    let y1 = extent.1;
    let y0 = (y1 - 800.0).max(0.0);
    let t = Instant::now();
    let gs = r.ensure_geom(0, fc).expect("geom");
    let (r0, r1) = (gs.geom.row_at_y(y0), gs.geom.row_at_y(y1));
    let page = r.window_page(0, fc, r0, r1, 1280.0).expect("window page");
    let (a, b) = grid::visible_row_bands(&page, y0, y1);
    assert!(b >= a);
    let window_cells = grid::visible_cells(&page, y0, y1).len();
    let window_ms = ms(t);

    let t = Instant::now();
    // Bottom-right corner hit (view 1280×800 scrolled to the extent corner).
    let _hit = r.grid_cell_at(
        0,
        1200.0,
        780.0,
        (extent.0 - 1280.0).max(0.0),
        (extent.1 - 800.0).max(0.0),
        fc,
    );
    let cell_at_ms = ms(t);

    BaselineTimings { from_xlsx_ms, extent_ms, window_ms, cell_at_ms, extent, window_cells }
}

// ─── generator tests (CI-fast, un-ignored) ───────────────────────────────────

#[test]
fn stamped_fixture_bytes_deterministic() {
    assert_eq!(stamped_xlsx(STAMPED_CI_ROWS), stamped_xlsx(STAMPED_CI_ROWS));
}

#[test]
fn stamped_fixture_parses_with_full_extent_sentinel_and_merges() {
    let fc = calibri_font_cache();
    let r = SmlRenderer::from_xlsx(stamped_xlsx(STAMPED_CI_ROWS)).expect("from_xlsx");
    let (w, h) = r.grid_extent(0, &fc).expect("grid_extent");
    // Extent fidelity: every stamped row contributes a band — height ≥
    // rows × 20px (15pt default) plus the custom-height exceptions.
    assert!(h >= STAMPED_CI_ROWS as f64 * 20.0, "stamped extent too short: {h}");
    assert!(w > 0.0);
    // The v2 FULL-RANGE window (the v1/v2 equivalence surface).
    let p = r.window_page(0, &fc, 1, STAMPED_CI_ROWS, 0.0).expect("window page");
    assert_eq!(p.row_rails.len() as u32, STAMPED_CI_ROWS + 1, "one band per stamped row");
    assert_eq!(p.merges.len(), 4, "all four merge regions project onto the page");
    let sentinel = p
        .cells
        .iter()
        .find(|c| c.ref_a1 == format!("J{STAMPED_CI_ROWS}"))
        .expect("bottom-right sentinel cell present");
    assert_eq!(sentinel.text, "SENTINEL");
    // The r-less run resolved to implied numbers 1000..=1004.
    assert!(p.cells.iter().any(|c| c.ref_a1 == "A1004" && c.text == "implied 1004"));
    // The chunk-boundary merge anchor expanded down across 256|257.
    let anchor = p.cells.iter().find(|c| c.ref_a1 == "A255").expect("A255 anchor");
    assert!(anchor.is_merged_anchor && anchor.h > 20.0 * 3.0, "A255:A258 spans 4 bands");
    // Wrap-without-ht row: the engine has no wrap
    // auto-height — a row without `ht` gets the 20px default regardless of
    // wrapText, and the equivalence pin below freezes exactly that. The
    // cell itself must be present with its wrapText style resolved.
    let wrap = p.cells.iter().find(|c| c.ref_a1 == "B2000").expect("wrap cell present");
    assert!(wrap.text.starts_with("This wrap-text cell"));
    let i2000 = p.row_numbers.iter().position(|&n| n == 2000).expect("row 2000 band");
    let band_h = p.row_rails[i2000 + 1] - p.row_rails[i2000];
    assert!((band_h - 20.0).abs() < 0.01, "no auto-grow today, got {band_h}px");
}

/// THE EQUIVALENCE PIN over the generated fixture: the v2 window
/// path, materialized over the FULL row range, must reproduce the v1
/// `continuous_page` geometry hash EXACTLY (the constant was pinned
/// off the eager path and is reproduced by the lazy one). Re-pinning = broken
/// equivalence.
#[test]
fn stamped_fixture_continuous_geometry_hash_pinned() {
    let fc = calibri_font_cache();
    let hash = |bytes: Vec<u8>| {
        let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");
        let p = r.window_page(0, &fc, 1, STAMPED_CI_ROWS, 0.0).expect("window page");
        grid::geometry_hash(std::slice::from_ref(&p))
    };
    let a = hash(stamped_xlsx(STAMPED_CI_ROWS));
    let b = hash(stamped_xlsx(STAMPED_CI_ROWS));
    assert_eq!(a, b, "window geometry must be deterministic");
    if std::env::var("SML_DUMP").is_ok() {
        eprintln!("stamped-{STAMPED_CI_ROWS} v2 window geometry_hash=0x{a:016x}");
    }
    // PINNED baseline (off the EAGER continuous page — the v2 window path
    // must reproduce it hash-identically; re-pinning = broken equivalence).
    assert_eq!(a, STAMPED_CI_GEOMETRY_HASH, "stamped fixture geometry drift (0x{a:016x})");
}

const STAMPED_CI_GEOMETRY_HASH: u64 = 0xd934bb7cfc50513c;

// ─── 1M-row perf baseline + fixture writer (explicit, --ignored) ────────────

#[test]
#[ignore = "perf gate over the 1M-row fixture — run explicitly with --release"]
fn perf_baseline_stamped_1m() {
    let fc = calibri_font_cache();
    let t = Instant::now();
    let bytes = stamped_xlsx(STAMPED_1M_ROWS);
    eprintln!("[perf] generate: {:.0} ms, {} bytes (.xlsx)", ms(t), bytes.len());
    let b = measure_baseline(bytes, &fc);
    eprintln!(
        "[perf] from_xlsx: {:.1} ms | grid_extent(first): {:.1} ms | bottom-window (frame+fill): {:.3} ms ({} cells) | grid_cell_at: {:.3} ms | extent {:.0}×{:.0}px",
        b.from_xlsx_ms, b.extent_ms, b.window_ms, b.window_cells, b.cell_at_ms, b.extent.0, b.extent.1
    );
    // PERF GATES (native release; the browser pays
    // a small constant factor — these bounds leave headroom under the 2s/100ms
    // budgets; measurements when the gates were set were ~an order below each bound):
    //  - interactive class: load (pre-scan, the only O(bytes) step) + first
    //    extent (closed-form) — the engine share of the <2s gate.
    assert!(
        b.from_xlsx_ms + b.extent_ms < 1500.0,
        "interactive budget blown: from_xlsx {:.1}ms + extent {:.1}ms",
        b.from_xlsx_ms,
        b.extent_ms
    );
    //  - scroll-to-anywhere cold window (frame + fill, incl. the slice parse).
    assert!(b.window_ms < 100.0, "cold bottom-window too slow: {:.2}ms", b.window_ms);
    //  - hit-test stays interactive-class (pointermove calls it).
    assert!(b.cell_at_ms < 10.0, "grid_cell_at too slow: {:.2}ms", b.cell_at_ms);
    // Extent fidelity: the full stamped extent, closed-form.
    assert!(b.extent.1 > 20_900_000.0, "1M-row extent lost: {:.0}px", b.extent.1);
}

#[test]
#[ignore = "perf gate over the 1M-row fixture — run explicitly with --release"]
fn perf_lazy_pagination_stamped_1m() {
    // The lazy-pagination win, natively pinned: the stamped-1M fixture's print
    // range covers ALL 1M rows (the J1048576 sentinel is value-bearing), so
    // an eager run_layout would do one synchronous 1M-row parse + eager pagination
    // at load (a ~140 s browser stall). The LAZY path must (a) stream
    // the full clipped-to-content page count as pure metadata fast, (b) parse
    // NO cells at load, and (c) materialize any single page ms-class.
    let fc = calibri_font_cache();
    let r = SmlRenderer::from_xlsx(stamped_xlsx(STAMPED_1M_ROWS)).expect("from_xlsx");

    let t = Instant::now();
    let (count, pages_cell) = super::window_tests::run_layout_native(&r, &fc);
    let plan_ms = ms(t);
    eprintln!("[perf] lazy run_layout (skeletons): {:.1} ms, {count} pages", plan_ms);
    assert!(count > 14_000, "the sentinel-extended print range paginates large: {count}");
    assert_eq!(r.chunk_count(), 0, "no cell parse at load");
    assert!(plan_ms < 1000.0, "metadata pagination too slow: {plan_ms:.1} ms");

    // First paint of the LAST page (sentinel) — one chunk fill.
    let t = Instant::now();
    r.ensure_page(count as usize - 1, &pages_cell);
    let fill_ms = ms(t);
    eprintln!("[perf] last-page materialization: {:.2} ms", fill_ms);
    let pages = pages_cell.borrow();
    let last = pages.last().expect("last page");
    assert!(last.cells.iter().any(|c| c.text == "SENTINEL"), "sentinel on the last page");
    assert!(fill_ms < 100.0, "per-page materialization too slow: {fill_ms:.2} ms");
}

/// Opt-in repro for the "search a single common char freezes the page" bug
/// on a large workbook. Not a committed fixture — the bytes are read
/// at runtime from the path in env `SML_FREEZE_XLSX`. The test SKIPS (passes)
/// when the variable is unset, so the gate never depends on it.
///
/// Times the active-sheet search for "s" (matches ~every text cell) and breaks
/// the cost into: the whole `search_sheet` walk vs. a baseline that runs ONLY
/// `find_matches` over every cell (no `locate_cell`/`fill_plan_page`). The gap
/// is the per-match `fill_plan_page` cost — the suspected O(M·page) blow-up.
#[test]
#[ignore = "opt-in repro — run with SML_FREEZE_XLSX set to an .xlsx path"]
fn perf_search_common_char_freeze_local_workbook() {
    // Path comes ONLY from the env var — no local path is baked into git.
    let Ok(path) = std::env::var("SML_FREEZE_XLSX") else {
        eprintln!("[freeze] SKIP — set SML_FREEZE_XLSX to a local .xlsx to run this repro");
        return;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("[freeze] SKIP — file not found at {path}");
        return;
    };
    eprintln!("[freeze] loaded {} bytes from {path}", bytes.len());

    let fc = calibri_font_cache();
    let r = SmlRenderer::from_xlsx(bytes).expect("from_xlsx");

    // Active-sheet scope (what the snippet's search box passes for the open sheet).
    let opts = crate::SearchOpts { case_sensitive: false, whole_word: false, workbook: false };

    // --- Baseline: ONLY find_matches over every cell (no locate_cell). ---
    let (mut n_cells, mut m_matches) = (0usize, 0usize);
    let t = Instant::now();
    {
        use crate::sml::window;
        let gs = r.ensure_geom(0, &fc).expect("geom");
        let max_row = gs.geom.max_row;
        let last = window::chunk_of(max_row.max(1));
        let mut cid = window::chunk_of(1);
        while cid <= last {
            if let Some(chunk) = r.chunk(0, cid) {
                for rd in chunk.iter() {
                    for cm in &rd.cells {
                        n_cells += 1;
                        if !crate::find_matches(&cm.text, "s", opts).is_empty() {
                            m_matches += 1;
                        }
                    }
                }
            }
            cid += 1;
        }
    }
    let matches_only_ms = ms(t);

    // Plan shape for sheet 0 (bands × pages — the fill_plan_page-per-match cost).
    if let Some(plan) = r.sheet_plan(0, &fc) {
        eprintln!(
            "[freeze] sheet0 plan: {} pages, {} bands, max page rows={}",
            plan.pages.len(),
            plan.bands.len(),
            plan.pages.iter().map(|p| p.r1 - p.r0 + 1).max().unwrap_or(0),
        );
    }

    // --- Full search_sheet (count path: find_matches + locate_cell per match). ---
    // Build it on a FRESH renderer too, so neither plan nor geom is warm — the
    // worst case the browser pays when search runs before run_layout.
    let cold = SmlRenderer::from_xlsx(std::fs::read(&path).unwrap()).expect("from_xlsx");
    let t = Instant::now();
    let cold_hits = cold.search_sheet(0, "s", opts, &fc);
    let cold_ms = ms(t);
    eprintln!("[freeze] COLD search_sheet (no warm plan/geom): {cold_ms:.1} ms, {} hits", cold_hits.len());

    let t = Instant::now();
    let hits = r.search_sheet(0, "s", opts, &fc);
    let search_sheet_ms = ms(t);

    // Workbook scope: all sheets.
    let t = Instant::now();
    let mut wb_hits = 0usize;
    for si in 0..r.sheet_count() {
        wb_hits += r.search_sheet(si, "s", opts, &fc).len();
    }
    let wb_ms = ms(t);
    eprintln!("[freeze] WORKBOOK-scope search (all {} sheets): {wb_ms:.1} ms, {wb_hits} hits", r.sheet_count());

    eprintln!(
        "[freeze] N(cells)={n_cells} M(matches for 's')={m_matches} hits={} | \
         find_matches-only: {matches_only_ms:.1} ms | full search_sheet: {search_sheet_ms:.1} ms | \
         per-match locate_cell: {:.2} ms",
        hits.len(),
        if m_matches > 0 {
            (search_sheet_ms - matches_only_ms) / m_matches as f64
        } else {
            0.0
        }
    );
    assert_eq!(hits.len(), m_matches, "count path must agree with find_matches");
}

#[test]
#[ignore = "writes the 1M fixture to target/sml-perf/ (gitignored) for browser perf checks"]
fn write_stamped_1m_fixture() {
    let out = std::env::var("SML_FIXTURE_OUT").unwrap_or_else(|_| {
        format!("{}/../../target/sml-perf/stamped-1m-rows.xlsx", env!("CARGO_MANIFEST_DIR"))
    });
    let bytes = stamped_xlsx(STAMPED_1M_ROWS);
    let path = std::path::Path::new(&out);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("mkdir -p");
    }
    std::fs::write(path, &bytes).expect("write fixture");
    eprintln!("[perf] wrote {} ({} bytes)", path.display(), bytes.len());
}
