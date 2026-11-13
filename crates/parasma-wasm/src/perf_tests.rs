//! Native layout MEASUREMENT harness.
//!
//! Performance work here is judged by a NUMBER that anyone can re-run. A
//! performance figure that cannot be reproduced is prose — nobody can re-run
//! it, so nobody can prove a delta. Two `#[ignore]`d entry points:
//!
//!   * [`perf_layout_timing`] — the durable baseline. Per document: parse once,
//!     then N timed `run_layout_native` repeats, reporting time-to-FIRST-page
//!     (the tracked metric) and whole-layout wall time with the spread.
//!     Re-run it before and after a change; a claimed speedup
//!     that does not clear the reported standard deviation is not a speedup.
//!   * [`perf_face_construction_share`] — how much of layout
//!     is `rustybuzz::Face::from_slice`, measured two independent ways.
//!
//! Both are `#[ignore]`d: they are tools, never part of the gate. Neither
//! asserts a timing threshold — a wall-clock assertion on a shared laptop is a
//! flaky test, and the point here is to produce a number a human compares, not
//! to fail a build.
//!
//! ```text
//! cargo test --release -p parasma-wasm --features sml,pml,emf \
//!     perf_layout_timing -- --ignored --nocapture --test-threads=1
//! ```
//!
//! **`--release` is not optional.** A debug build's shaping cost is dominated by
//! unoptimised `rustybuzz`, so the *ratio* between phases — the only thing the
//! face-construction measurement turns on — is not the shipped one. **`--test-threads=1` is not optional
//! either** for `perf_face_construction_share`: the probe counters in
//! `engine::fonts::face_probe` are process-global.
//!
//! Document set: a synthetic prose document built in-source (so the harness
//! reports a number on a CLEAN CHECKOUT, with no gitignored input), plus any of
//! a default list of real documents that happen to be present — the third-party
//! `.corpus/` robustness corpus (public test documents from the Apache POI and
//! LibreOffice repositories) and the public `file-sample_1MB.docx`. Override
//! with `PERF_DOCS=/abs/a.docx,/abs/b.docx`. No calibration document is named
//! here, and none should be added (`docs/calibration-ids.md`).
//!
//! Fonts are the ORACLE basis (`oracle_tests::load_test_fonts` — Arimo + XITS
//! Math), so every document is shaped through the same faces whatever it
//! declares. That makes runs comparable to each other, which is the job; it does
//! NOT make them a model of what a browser with the real font set would cost.

use super::Viewer;
use std::time::{Duration, Instant};

// ─── document set ───────────────────────────────────────────────────────────

/// Repo root, from this crate's manifest dir.
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR")))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// A document under measurement, already read into memory (file I/O is not part
/// of what is being timed).
struct Doc {
    name: String,
    bytes: Vec<u8>,
}

/// The default set: the synthetic document always, then whichever of the listed
/// real documents exist. Absent ones are reported, not skipped silently — a
/// baseline whose document set is unknown is not comparable to the next one.
fn docs() -> Vec<Doc> {
    if let Ok(list) = std::env::var("PERF_DOCS") {
        return list
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| Doc {
                name: p.to_string(),
                bytes: std::fs::read(p).unwrap_or_else(|e| panic!("read {p}: {e}")),
            })
            .collect();
    }
    let root = repo_root();
    let mut out = vec![Doc { name: "synthetic-prose-400p".into(), bytes: synthetic_prose_docx(400) }];
    for rel in [
        ".corpus/poi/bib-chernigovka.netdo.ru_download_docs_17459.docx",
        ".corpus/poi/bug59058.docx",
        ".corpus/lo-export/fdo74605.docx",
        "fixtures/user/file-sample_1MB.docx",
    ] {
        let path = root.join(rel);
        match std::fs::read(&path) {
            Ok(bytes) => out.push(Doc { name: rel.into(), bytes }),
            Err(_) => println!("PERF_SKIP {rel} (not present)"),
        }
    }
    out
}

/// A synthetic prose document of `paragraphs` paragraphs — plain text with a
/// bold and an italic run per paragraph, so the shaping path is exercised with
/// more than one resolved face. Deterministic: the word stream is a fixed lorem
/// rotated by paragraph index, so two runs of this harness on two checkouts
/// measure the same work.
fn synthetic_prose_docx(paragraphs: usize) -> Vec<u8> {
    const WORDS: &[&str] = &[
        "lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit",
        "sed", "do", "eiusmod", "tempor", "incididunt", "ut", "labore", "et", "dolore",
        "magna", "aliqua", "enim", "ad", "minim", "veniam", "quis", "nostrud",
        "exercitation", "ullamco", "laboris", "nisi", "aliquip", "ex", "ea", "commodo",
    ];
    let mut body = String::new();
    for i in 0..paragraphs {
        let mut text = String::new();
        for w in 0..60 {
            if w > 0 {
                text.push(' ');
            }
            text.push_str(WORDS[(i * 7 + w * 3) % WORDS.len()]);
        }
        body.push_str(&format!(
            "<w:p><w:r><w:t xml:space=\"preserve\">{text} </w:t></w:r>\
             <w:r><w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">emphasis {i} </w:t></w:r>\
             <w:r><w:rPr><w:i/></w:rPr><w:t>and a closing clause.</w:t></w:r></w:p>"
        ));
    }
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>{body}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/>
<w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/></w:sectPr></w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
    super::oracle_tests::build_docx(&[
        ("[Content_Types].xml", content_types.to_string()),
        ("_rels/.rels", root_rels.to_string()),
        ("word/document.xml", document),
    ])
}

// ─── measurement plumbing ───────────────────────────────────────────────────

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// min / median / mean / max / standard deviation, all in ms. Population sd
/// (not sample): these are all the observations there are, and the number is
/// only ever read as "how noisy was this machine", never inferentially.
struct Stats { min: f64, med: f64, mean: f64, max: f64, sd: f64 }

fn stats(samples: &[Duration]) -> Stats {
    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in a duration"));
    let n = ms.len() as f64;
    let mean = ms.iter().sum::<f64>() / n;
    let var = ms.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    Stats {
        min: ms[0],
        med: ms[ms.len() / 2],
        mean,
        max: ms[ms.len() - 1],
        sd: var.sqrt(),
    }
}

impl Stats {
    /// Relative standard deviation, %. The honest read of a claimed speedup:
    /// a delta smaller than this is machine noise.
    fn rsd(&self) -> f64 {
        if self.mean == 0.0 { 0.0 } else { 100.0 * self.sd / self.mean }
    }
}

/// Parse `doc` once and hand back a laid-out-capable viewer on the oracle font
/// basis. `None` when the loader rejects the file — the robustness corpus has
/// members that do not open, and a measurement harness must not turn that into
/// a failure.
fn prepared(doc: &Doc) -> Option<Viewer> {
    let mut viewer = Viewer::new().expect("Viewer::new");
    super::oracle_tests::load_test_fonts(&mut viewer);
    if viewer.load_docx(doc.bytes.clone()).is_err() {
        println!("PERF_LOAD_ERR {} (loader rejected it)", doc.name);
        return None;
    }
    Some(viewer)
}

/// One timed layout: (time to the FIRST page callback, whole-layout time).
/// The first-page time is the native analogue of the browser's first-page metric —
/// it excludes the browser's per-page yield (`yield_to_browser`), absent here.
fn timed_layout(viewer: &Viewer) -> (Duration, Duration) {
    let mut first: Option<Duration> = None;
    let t0 = Instant::now();
    viewer
        .run_layout_native_with(|_| {
            if first.is_none() {
                first = Some(t0.elapsed());
            }
        })
        .expect("run_layout_native_with");
    (first.unwrap_or_default(), t0.elapsed())
}

// ─── (A) the durable baseline ───────────────────────────────────────────────

/// The re-runnable layout baseline. Prints one `PERF_DOC` line per document:
///
/// ```text
/// PERF_DOC <name> pages=N reps=R first_ms=<med> total_ms min/med/mean/max sd=<> rsd=<>%
/// ```
///
/// Tunables: `PERF_REPS` (default 9), `PERF_WARMUP` (default 2), `PERF_DOCS`.
#[test]
#[ignore]
fn perf_layout_timing() {
    let reps = env_usize("PERF_REPS", 9);
    let warmup = env_usize("PERF_WARMUP", 2);
    println!("PERF_CONFIG reps={reps} warmup={warmup} profile={}",
             if cfg!(debug_assertions) { "debug (RATIOS ARE NOT THE SHIPPED ONES)" } else { "release" });
    for doc in docs() {
        let Some(viewer) = prepared(&doc) else { continue };
        for _ in 0..warmup {
            timed_layout(&viewer);
        }
        // Re-layout must be idempotent for repeats to measure the same work;
        // a memoised second pass would report a fake speedup. Pinned here.
        let pages_first = viewer.page_count();
        let mut firsts = Vec::with_capacity(reps);
        let mut totals = Vec::with_capacity(reps);
        for _ in 0..reps {
            let (f, t) = timed_layout(&viewer);
            assert_eq!(
                viewer.page_count(), pages_first,
                "{}: re-layout changed the page count — the repeats are not measuring \
                 the same work", doc.name
            );
            firsts.push(f);
            totals.push(t);
        }
        let f = stats(&firsts);
        let t = stats(&totals);
        println!(
            "PERF_DOC {} pages={pages_first} reps={reps} \
             first_ms={:.2} (min {:.2} max {:.2}) = {:.0}% of total \
             total_ms med={:.2} min={:.2} mean={:.2} max={:.2} sd={:.2} rsd={:.1}%",
            doc.name, f.med, f.min, f.max,
            if t.med > 0.0 { 100.0 * f.med / t.med } else { 0.0 },
            t.med, t.min, t.mean, t.max, t.sd, t.rsd(),
        );
    }
}

// ─── (B) the face-construction share, measured ──────────────────────────────

/// An earlier estimate put `rustybuzz::Face::from_slice` (then in the free
/// `shape_text`, now in `FaceCache::shape`) at "90.4% of all
/// layout time". This measures it TWO independent ways, because each has a
/// known bias and they bracket the truth:
///
///  1. **In-situ** — `engine::fonts::face_probe` brackets the construction with
///     `Instant::now()`. Biased HIGH on the share for two reasons: the two clock
///     reads land inside the measured region's own denominator, and the probe
///     inflates the total it is divided by. Exact on the call COUNT.
///  2. **Projected** — an uninstrumented microbenchmark of `Face::from_slice`
///     on the same face, multiplied by the call count from (1), over the
///     UNINSTRUMENTED total from a clean pass. No clock in the hot loop.
///
/// Neither is a promise about the achievable win: removing the construction
/// does not make its cost zero, it makes it once-per-face-per-pass. The number
/// to compare a face-caching patch against is `perf_layout_timing`, not this.
#[test]
#[ignore]
fn perf_face_construction_share() {
    use super::engine::fonts::face_probe;

    // (0) Microbenchmark the construction in isolation, on the face every
    //     document here shapes through.
    let arimo = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR")))
        .join("test-fonts/Arimo-Regular.ttf");
    let bytes = std::fs::read(&arimo)
        .unwrap_or_else(|e| panic!("{}: {e}\n  run: ./scripts/fetch-test-fonts.sh", arimo.display()));
    let iters = env_usize("PERF_FACE_ITERS", 20_000);
    for _ in 0..1_000 {
        std::hint::black_box(rustybuzz::Face::from_slice(std::hint::black_box(&bytes), 0));
    }
    let t0 = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(rustybuzz::Face::from_slice(std::hint::black_box(&bytes), 0));
    }
    let per_face_ns = t0.elapsed().as_nanos() as f64 / iters as f64;
    println!(
        "PERF_FACE_CTOR font=Arimo-Regular.ttf bytes={} iters={iters} ns_per_construction={per_face_ns:.1}",
        bytes.len()
    );

    for doc in docs() {
        let Some(viewer) = prepared(&doc) else { continue };
        // Warm, then an UNINSTRUMENTED pass for the denominator of (2).
        face_probe::arm(false);
        timed_layout(&viewer);
        let (_, clean) = timed_layout(&viewer);

        // Instrumented pass: exact call count, in-situ nanoseconds.
        face_probe::arm(true);
        let (_, instrumented) = timed_layout(&viewer);
        let (calls, face_ns) = face_probe::read();
        face_probe::arm(false);

        let clean_ns = clean.as_secs_f64() * 1e9;
        let instrumented_ns = instrumented.as_secs_f64() * 1e9;
        let projected_ns = calls as f64 * per_face_ns;
        println!(
            "PERF_FACE_SHARE {} shape_calls={calls} \
             clean_ms={:.2} instrumented_ms={:.2} \
             insitu_share={:.1}% projected_share={:.1}%",
            doc.name,
            clean_ns / 1e6,
            instrumented_ns / 1e6,
            if instrumented_ns > 0.0 { 100.0 * face_ns as f64 / instrumented_ns } else { 0.0 },
            if clean_ns > 0.0 { 100.0 * projected_ns / clean_ns } else { 0.0 },
        );
    }
}
