//! Format-agnostic rendering core.
//!
//! Code shared across the WML/SML/PML renderers. The first core was a VERBATIM
//! relocation from `lib.rs`; later modules (`geom`, `pattern`, `border`,
//! `fragment`) arrived the same way — as pure moves or new shared code, never
//! behavior changes, enforced by the oracle snapshot tests (`oracle_tests.rs`)
//! whose geometry hashes must not drift on a pure move.

pub(crate) mod border;
pub(crate) mod color;
/// Display-list recording backend: a `Painter` impl that
/// captures the primitive draw stream as a serializable `Vec<DrawCmd>` instead
/// of touching a canvas, so the render code can run in a Web Worker and the main
/// thread replays the list onto its real (GPU-backed) canvas. web_sys-free +
/// natively testable.
pub(crate) mod displaylist;
pub(crate) mod fonts;
/// The flavor-neutral extraction schema — `DocFragment` + its
/// satellite types + `SelRect`. Built by WML and PML selection, serialized by
/// `lib.rs`; moving it here deleted the crate's only cross-format lateral
/// import (pml::select ← wml::select).
pub(crate) mod fragment;
/// Preset-geometry path builder — the PathCmd IR is format-agnostic by
/// design (§20.1 preset geometry is defined once for all three formats;
/// custGeom + connectors land on the same painter). Feature-gated so the
/// docx-only build stays byte-identical.
///
/// LAYERING-EXCEPTION(geom): PML-only today — WML parses no `a:prstGeom` and
/// draws every autoshape as a rectangle (`wml/render.rs` `render_text_box`),
/// so only `pml` reaches this module. Widening that DrawingML gate is the
/// declared FOLLOW-ON (behavior, its own change, needs fixtures); when it
/// lands, this exception and the `pml` feature gate below both come off — the
/// stale-exception ratchet in `scripts/check-layering.mjs` will insist. Kept
/// in engine/ rather than moved to pml/ because it is meant to serve all
/// three formats, and a 2,229-line
/// round-trip move would churn `git blame` + the wasm panic-location table.
#[cfg(feature = "pml")]
pub(crate) mod geom;
pub(crate) mod images;
/// Shared OPC package layer: one archive open per byte image + a
/// parse-once `.rels` cache + the ONE rel-target resolver, replacing the 11
/// per-site fork `Package::open(bytes.to_vec())` spellings in sml/ and pml/.
/// Feature-gated exactly like its `dep:ooxml-opc` dependency (armed by `sml`
/// and `pml` only) so docx-only builds stay byte-identical.
#[cfg(any(feature = "sml", feature = "pml"))]
pub(crate) mod opc;
pub(crate) mod paint;
/// Line-dash + pattern-fill style helpers (`<a:ln>` dash + `pattFill`) — pure, web_sys-free,
/// natively testable (the painter rasterises the returned tile specs / dash
/// arrays).
///
/// LAYERING-EXCEPTION(pattern): callers are PML-only, but the module cannot
/// move to pml/ — the `PatternTile`/`TilePrim` DATA types are named by the
/// always-compiled `Painter` trait (`set_pattern_fill`) and the display-list
/// encoder, so it stays unconditional (docx-only builds keep the
/// `pattern_tile`/`preset_dash` mappers dead — `#![…allow(dead_code)]` — and
/// never CALL the pattern path, so docx pixels stay byte-identical). The
/// mappers await a second caller (WML `w:shd` patterns / SML pattern fills).
pub(crate) mod pattern;
pub(crate) mod units;
pub(crate) mod vml;

/// Goldens that exist on disk but are NOT tracked by git — the failure a
/// count-only floor cannot see.
///
/// A missing golden is auto-recorded by `assert_*_golden` (correct: that is what
/// makes adding a fixture frictionless). So by the time any floor test runs, its
/// sibling tests have already re-created every file, and "N files on disk" is
/// satisfied on a machine where the goldens were never committed. The pin then
/// re-records itself forever and is vacuously green — the exact hazard both
/// floor tests were written to prevent, and which neither implemented.
///
/// Returns `None` outside a git checkout (a vendored crate, a release tarball),
/// where the question is unanswerable and skipping is correct.
#[cfg(test)]
pub(crate) fn untracked_goldens(suffix: &str) -> Option<Vec<String>> {
    let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/oracle-snapshots"));
    // NOT `--error-unmatch`: it makes git FAIL when nothing matches, which is
    // exactly the worst case (zero goldens tracked) — the check would then
    // return None and silently pass. Ask "is this a repo?" separately from
    // "what is tracked?", so an empty answer stays an answer.
    let is_repo = std::process::Command::new("git")
        .arg("-C").arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .ok()
        .is_some_and(|o| o.status.success());
    if !is_repo {
        return None; // vendored crate / release tarball — unanswerable
    }
    let out = std::process::Command::new("git")
        .arg("-C").arg(dir)
        .args(["ls-files", "--"])
        .arg(".")
        .output()
        .ok()?;
    let tracked: std::collections::HashSet<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.rsplit('/').next().unwrap_or(l).to_string())
        .collect();
    let mut missing = Vec::new();
    for e in std::fs::read_dir(dir).ok()? {
        let n = e.ok()?.file_name().to_string_lossy().into_owned();
        if n.ends_with(suffix) && !tracked.contains(&n) {
            missing.push(n);
        }
    }
    missing.sort();
    Some(missing)
}
