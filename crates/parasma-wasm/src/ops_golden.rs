//! Byte-identity goldens for RECORDED PAINT STREAMS — the SML and PML half of
//! the safety net.
//!
//! The WML oracle pins three streams (LINE hash, ITEM golden, GLYPH golden) and
//! every one of them is WML-only: they walk `PagedLine`/`Item`, structures the
//! other two formats do not have.
//!
//! Measured before this module existed: inflating the value returned by PML's ONE shaping
//! call (`measure_segment_advance`'s `shaped.total_advance`) by 5% — and again
//! by 4× — left all 96 pre-existing PML tests GREEN. Replacing a painted run's
//! text left the same 96 green. PML's shaping site was reached by tests and
//! asserted by none, so any change to it was free.
//!
//! What both formats DO have is a `Painter`. `RecordingPainter` turns a paint
//! into an ordered `Vec<DrawCmd>`, and `FillText{text,x,y}` carries the
//! shaping-derived pen position — so a golden over the recorded stream is the
//! cheapest instrument that can see SML and PML shaping at all. That is the
//! whole claim: the OPS stream pins where the glyphs were asked to go, and the
//! text asked for. It does NOT pin the glyphs themselves — `DrawCmd::FillText`
//! records the run's string, not its glyph ids, advances and clusters. A
//! GLYPH-stream equivalent needs a retention change in both formats (both
//! discard the glyph vector in the same expression that produces it) and is
//! deliberately NOT attempted here.
//!
//! DEVIATION (declared, not silenced): `oracle_tests.rs` carries a private
//! `assert_golden` with the identical `ORACLE_BLESS` contract. This is a second
//! spelling of one rule and a DRY-of-knowledge violation. It has not yet been
//! collapsed; the two should become one when `oracle_tests.rs` is next
//! reworked. This one is the reusable home (crate-root, format-neutral), so the
//! move is `oracle_tests.rs` calling in, not the reverse.
//!

use std::fmt::Write as _;

use crate::engine::displaylist::DrawCmd;

/// Directory holding the committed goldens — the SAME directory as the WML
/// ITEM/GLYPH streams. One place to look for "what is pinned", and the `.ops`
/// infix keeps the sets disjoint (`every_golden_stream_is_on_disk` counts
/// `.items.txt`/`.glyphs.txt` only, and is unaffected).
const SNAPSHOT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/oracle-snapshots");

/// Render one recorded display list as a reviewable text stream — one record
/// per `DrawCmd`, floats at `{:.2}` (the harness-wide precision, matching every
/// WML dump).
///
/// Records carry NO index prefix, deliberately: an inserted command would
/// otherwise renumber every following line and turn a one-op diff into a
/// whole-file diff. `assert_ops_golden` reports the record number itself.
///
/// The match is EXHAUSTIVE over `DrawCmd` with no `_` arm and no rest-dots, so
/// this doubles as a completeness guard in the `CellStyle::hash_into` family: a
/// new painter primitive — or a new field on an existing one — cannot compile
/// until it is folded into what the goldens see. That is the failure this module
/// exists to close; a variant that records but never dumps is a pin with a hole
/// in it.
fn dump_ops(cmds: &[DrawCmd]) -> String {
    let mut out = String::with_capacity(cmds.len() * 32);
    for cmd in cmds {
        match cmd {
            DrawCmd::Save => out.push_str("Save"),
            DrawCmd::Restore => out.push_str("Restore"),
            DrawCmd::BeginPath => out.push_str("BeginPath"),
            DrawCmd::ClosePath => out.push_str("ClosePath"),
            DrawCmd::MoveTo { x, y } => { let _ = write!(out, "MoveTo {x:.2} {y:.2}"); }
            DrawCmd::LineTo { x, y } => { let _ = write!(out, "LineTo {x:.2} {y:.2}"); }
            DrawCmd::Rect { x, y, w, h } => { let _ = write!(out, "Rect {x:.2} {y:.2} {w:.2} {h:.2}"); }
            DrawCmd::Arc { x, y, radius, start, end } => {
                let _ = write!(out, "Arc {x:.2} {y:.2} r{radius:.2} {start:.2}..{end:.2}");
            }
            DrawCmd::ArcAnticlockwise { x, y, radius, start, end, anticlockwise } => {
                let _ = write!(out, "ArcACW {x:.2} {y:.2} r{radius:.2} {start:.2}..{end:.2} acw{}", u8::from(*anticlockwise));
            }
            DrawCmd::Ellipse { x, y, rx, ry, rotation, start, end, anticlockwise } => {
                let _ = write!(out, "Ellipse {x:.2} {y:.2} rx{rx:.2} ry{ry:.2} rot{rotation:.2} {start:.2}..{end:.2} acw{}", u8::from(*anticlockwise));
            }
            DrawCmd::QuadraticCurveTo { cpx, cpy, x, y } => {
                let _ = write!(out, "QuadTo {cpx:.2} {cpy:.2} {x:.2} {y:.2}");
            }
            DrawCmd::BezierCurveTo { cp1x, cp1y, cp2x, cp2y, x, y } => {
                let _ = write!(out, "BezierTo {cp1x:.2} {cp1y:.2} {cp2x:.2} {cp2y:.2} {x:.2} {y:.2}");
            }
            DrawCmd::Fill => out.push_str("Fill"),
            DrawCmd::Stroke => out.push_str("Stroke"),
            DrawCmd::Clip => out.push_str("Clip"),
            DrawCmd::FillRect { x, y, w, h } => { let _ = write!(out, "FillRect {x:.2} {y:.2} {w:.2} {h:.2}"); }
            DrawCmd::StrokeRect { x, y, w, h } => { let _ = write!(out, "StrokeRect {x:.2} {y:.2} {w:.2} {h:.2}"); }
            // The load-bearing record: `x` is where shaping put the pen.
            DrawCmd::FillText { text, x, y } => { let _ = write!(out, "FillText {x:.2} {y:.2} {text:?}"); }
            DrawCmd::StrokeText { text, x, y } => { let _ = write!(out, "StrokeText {x:.2} {y:.2} {text:?}"); }
            DrawCmd::SetFont(s) => { let _ = write!(out, "SetFont {s:?}"); }
            DrawCmd::SetTextAlign(s) => { let _ = write!(out, "SetTextAlign {s:?}"); }
            DrawCmd::SetTextBaseline(s) => { let _ = write!(out, "SetTextBaseline {s:?}"); }
            DrawCmd::SetFillStyle(s) => { let _ = write!(out, "SetFillStyle {s:?}"); }
            DrawCmd::SetStrokeStyle(s) => { let _ = write!(out, "SetStrokeStyle {s:?}"); }
            DrawCmd::SetLineWidth(v) => { let _ = write!(out, "SetLineWidth {v:.2}"); }
            DrawCmd::SetGlobalAlpha(v) => { let _ = write!(out, "SetGlobalAlpha {v:.2}"); }
            DrawCmd::SetShadowColor(s) => { let _ = write!(out, "SetShadowColor {s:?}"); }
            DrawCmd::SetShadowOffsetX(v) => { let _ = write!(out, "SetShadowOffsetX {v:.2}"); }
            DrawCmd::SetShadowOffsetY(v) => { let _ = write!(out, "SetShadowOffsetY {v:.2}"); }
            DrawCmd::SetShadowBlur(v) => { let _ = write!(out, "SetShadowBlur {v:.2}"); }
            DrawCmd::Translate { x, y } => { let _ = write!(out, "Translate {x:.2} {y:.2}"); }
            DrawCmd::Rotate { angle } => { let _ = write!(out, "Rotate {angle:.2}"); }
            DrawCmd::Scale { x, y } => { let _ = write!(out, "Scale {x:.2} {y:.2}"); }
            DrawCmd::SetTransform { a, b, c, d, e, f } => {
                let _ = write!(out, "SetTransform {a:.2} {b:.2} {c:.2} {d:.2} {e:.2} {f:.2}");
            }
            DrawCmd::DrawImage { key, dx, dy, dw, dh } => {
                let _ = write!(out, "DrawImage {key:?} {dx:.2} {dy:.2} {dw:.2} {dh:.2}");
            }
            DrawCmd::SetLinearGradient { x0, y0, x1, y1, stops } => {
                let _ = write!(out, "SetLinearGradient {x0:.2} {y0:.2} {x1:.2} {y1:.2}");
                for (pos, hex) in stops {
                    let _ = write!(out, " {pos:.2}:{hex}");
                }
            }
            DrawCmd::SetRadialGradient { x0, y0, r0, x1, y1, r1, stops } => {
                let _ = write!(out, "SetRadialGradient {x0:.2} {y0:.2} r{r0:.2} {x1:.2} {y1:.2} r{r1:.2}");
                for (pos, hex) in stops {
                    let _ = write!(out, " {pos:.2}:{hex}");
                }
            }
            // `prims` dumps through Debug rather than field-by-field so this
            // module never NAMES `engine::pattern` — that module carries a
            // `LAYERING-EXCEPTION(pattern)` declaring it PML-only, and a
            // crate-root import would silently make the exception stale and
            // fail RULE 4. Debug is full-precision, i.e. stricter, not looser.
            DrawCmd::SetPatternFill { key, fg, bg, size, prims } => {
                let _ = write!(out, "SetPatternFill {key:?} fg{fg} bg{bg} s{size:.2} {prims:?}");
            }
            DrawCmd::SetLineDash(segs) => {
                let _ = write!(out, "SetLineDash");
                for s in segs {
                    let _ = write!(out, " {s:.2}");
                }
            }
            DrawCmd::ClearLineDash => out.push_str("ClearLineDash"),
        }
        out.push('\n');
    }
    out
}

/// Assert a recorded paint stream against `oracle-snapshots/{name}.ops.txt`.
///
/// The `ORACLE_BLESS` contract is the WML oracle's, unchanged and deliberately
/// asymmetric: a MISSING golden is written and passes (a genuinely new fixture
/// cannot bless anything away, which is what makes adding one frictionless);
/// an EXISTING golden is only overwritten when this fixture is NAMED
/// (`ORACLE_BLESS=sml-grid-oracle`) or under `ORACLE_BLESS=all`. The `git diff`
/// is the review artifact.
///
/// On drift the panic names the FIRST differing record with its number and both
/// sides — a red pin must say WHAT moved, which is the entire reason these are
/// files and not the hex digests SML used to carry.
pub(crate) fn assert_ops_golden(name: &str, cmds: &[DrawCmd], diagnosis: &str) {
    let got = dump_ops(cmds);
    let path = std::path::Path::new(SNAPSHOT_DIR).join(format!("{name}.ops.txt"));
    let bless = std::env::var("ORACLE_BLESS").unwrap_or_default();
    let named = bless == "all" || bless.split(',').any(|f| f.trim() == name);
    if !path.exists() || named {
        std::fs::create_dir_all(SNAPSHOT_DIR).expect("create oracle-snapshots/");
        if path.exists() && named {
            let prev = std::fs::read_to_string(&path).unwrap_or_default();
            if prev != got {
                eprintln!(
                    "[ORACLE_BLESS] {name}.ops: OVERWRITING an existing golden \
                     ({} → {} records). Review the `git diff` before committing.",
                    prev.lines().count(),
                    got.lines().count(),
                );
            }
        }
        std::fs::write(&path, got.as_bytes()).expect("write golden");
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("{name}: cannot read ops golden {}: {e} — delete it to re-record", path.display())
    });
    if got == want {
        return;
    }
    let (mut gl, mut wl) = (got.lines(), want.lines());
    let mut n = 0usize;
    loop {
        n += 1;
        match (gl.next(), wl.next()) {
            (Some(a), Some(b)) if a == b => continue,
            (a, b) => panic!(
                "{name}: ops stream drifted at record {n} — {diagnosis}.\n  golden: {}\n  \
                 actual: {}\n({} records golden vs {} actual; if this change is INTENDED, \
                 re-record just this fixture with ORACLE_BLESS={name} and review the git diff)",
                b.unwrap_or("<end of file>"),
                a.unwrap_or("<end of file>"),
                want.lines().count(),
                got.lines().count(),
            ),
        }
    }
}

/// Every OPS golden the suite asserts must EXIST on disk and be tracked.
///
/// `assert_ops_golden` records a missing golden and passes, which is right for
/// a new fixture and wrong for the far likelier mistake: committing the test
/// and forgetting to `git add` its golden leaves a pin that re-records itself
/// on every machine and is vacuously green forever. This is the same floor the
/// WML streams carry (`every_golden_stream_is_on_disk`), for the same reason.
///
/// A FLOOR, not an exact count — adding a fixture must not red this.
#[test]
fn every_ops_golden_is_on_disk() {
    let dir = std::path::Path::new(SNAPSHOT_DIR);
    let mut ops = 0usize;
    for e in std::fs::read_dir(dir).expect("oracle-snapshots/ must exist") {
        let n = e.expect("dir entry").file_name().to_string_lossy().into_owned();
        if n.ends_with(".ops.txt") {
            ops += 1;
        }
    }
    assert!(ops >= 5, "only {ops} .ops.txt goldens on disk — expected >= 5 (4 SML + 1 PML)");

    // The floor above cannot see the failure this test's name promises. A
    // missing golden is auto-recorded by `assert_ops_golden`, so by the time
    // this runs its siblings have re-created every file and the count passes on
    // a machine where nothing was ever committed. Tracking is the real property.
    if let Some(untracked) = crate::engine::untracked_goldens(".ops.txt") {
        assert!(
            untracked.is_empty(),
            "ops goldens exist on disk but are NOT tracked by git: {untracked:?} — \
             they will re-record themselves on every machine and this pin is \
             vacuously green. `git add` them."
        );
    }
}
