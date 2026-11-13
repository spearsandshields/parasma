//! Consolidated parser-repair module — the TEMPORARY stopgap that makes
//! real-but-technically-malformed Office files PARSE.
//!
//! ## Why this exists
//!
//! The forked OOXML codegen emits, for every `use="required"` schema attribute,
//! a hard `ok_or_else(|| ParseError::MissingAttribute("x"))?`. A real file that
//! omits such an attribute (several third-party generators routinely
//! do) makes the WHOLE-part parse return `Err` → the document fails to load →
//! blank page. The strict parser is *correctly* honoring the schema; it is just
//! too strict for a viewer (Postel's law: be liberal in what you accept).
//!
//! ## Design — TARGETED, dispatch-on-FAILURE (mirrors `sml/shim.rs`)
//!
//! The happy path is NEVER touched. We try the fork parse on the ORIGINAL bytes
//! first; ONLY when it fails with the specific signature a repair rule targets do
//! we apply that rule's byte-rewrite to the offending part and RE-parse. A valid
//! file never reaches a repair, so a valid file can never be corrupted or have a
//! real bug masked. Each repair operates on raw package bytes BEFORE the fork
//! parses them, so NO fork change is needed (the repair is parasma-side only).
//!
//! ## Rules (each is a deliberate workaround, removed by the fix below)
//!
//!  - **RULE 1 — [`sml_col_attrs`]** (§18.3.1.13). Worksheet `<col>` missing
//!    `@min`/`@max` → inject a running 1-based column index. MIGRATED here from
//!    `sml/shim.rs` (behavior identical); `shim.rs` now calls this fn.
//!  - **RULE 2 — [`wml_pgmar_attrs`]** (§17.6.11). `<w:pgMar>` missing
//!    `@w:gutter` (or any other of the 7 required attrs) → inject `="0"`. Wired
//!    via [`load_docx_tolerant`].
//!  - **RULE 3 — [`pml_strip_custgeom`]** (§20.1.9.x). A `<a:custGeom>` custom
//!    geometry subtree drives the fork's `CTAdjPoint2D` parser to look for x/y as
//!    ATTRIBUTES on the attribute-less `<a:moveTo>`/`<a:lnTo>` path-command
//!    wrapper (the coordinates actually live on the child `<a:pt>`), so it errors
//!    `missing attribute: x`. We replace each `custGeom` with a `prstGeom`
//!    rectangle placeholder so the slide parses. Wired via [`load_pptx_tolerant`].
//!
//! ## The fix: lenient fork codegen (deletes this module)
//!
//! Make the fork codegen lenient: funnel `use="required"` through the existing
//! `is_optional` flag (which already drives `Option<T>` type + skip-if-none
//! serializer) so a listed attr becomes a faithful, round-trip-safe `Option` in
//! ONE edit, with defaults applied at USE time (computed/render layer), never at
//! parse time. For RULE 3, fix the `Path2DMoveToElement = Box<CTAdjPoint2D>`
//! type-alias elision so codegen descends into the child `<a:pt>` instead of
//! reading x/y off the wrapper. When that lands, this whole module + its `zip` dep
//! are deleted and the loaders call the fork directly.

/// Shared byte-scanning primitives (find tag end honoring quotes, attr-present,
/// UTF-8 lead-byte length) used by every rule's tag-scan-inject discipline.
/// `pub(crate)`: the `sml::index` grid pre-scan reuses `find_tag_end` for
/// its quote-aware sheetData/row scanning (the same discipline).
pub(crate) mod xml_scan;

/// In-memory zip rebuild: replace ONE part's bytes, copy every other entry
/// unchanged. Used by the WML/PML rules whose repair target is an interior
/// package part (`word/document.xml` / `ppt/slides/slideN.xml`).
mod zip_rewrite;
// Re-exported only for the repair test battery (unused by the lib itself).
#[cfg(test)]
pub(crate) use zip_rewrite::rebuild_zip_replacing_part;

// RULE 1 — SML `<col>` @min/@max (feature `sml`; migrated from sml/shim.rs).
#[cfg(feature = "sml")]
mod sml_col;
#[cfg(feature = "sml")]
pub(crate) use sml_col::repair_col_attrs as sml_col_attrs;

// RULE 3 — PML custGeom strip (feature `pml`).
#[cfg(feature = "pml")]
mod pml_custgeom;
#[cfg(feature = "pml")]
pub(crate) use pml_custgeom::{
    load_pptx_tolerant, repaired_parse_error as pml_custgeom_error,
};
// Re-exported only for the repair test battery (unused by the lib itself).
#[cfg(all(feature = "pml", test))]
pub(crate) use pml_custgeom::strip_custgeom as pml_strip_custgeom;

// RULE 4 (opc_abs_target) RETIRED: absolute rels Targets are now resolved by
// the parser itself (ooxml-opc resolve_part_path, fork durable fix) — the
// eager .rels pre-scan and open_xlsx_tolerant are gone. The end-to-end shapes
// stay pinned in sml/tests.rs (renderer_from_xlsx_renders_absolute_*).

// RULE 7 — table-nesting depth cap (EAGER: stack overflow is uncatchable).
mod wml_nesting;

// RULE 6 — ISO Strict → Transitional namespace downgrade (translation shim;
// retirable if the fork gains native Strict-namespace support).
mod wml_strict;

// RULE 5 — WML malformed-input normalization (always compiled, same pipeline
// as RULE 2: dispatch-on-Invalid, one document.xml rewrite, one re-parse).
mod wml_malformed;

// RULE 2 — WML pgMar gutter (always compiled; the docx path is never feature-
// gated).
mod wml_pgmar;
pub(crate) use wml_pgmar::load_docx_tolerant;
// Re-exported only for the repair test battery (unused by the lib itself).
#[cfg(test)]
pub(crate) use wml_pgmar::repair_pgmar_attrs as wml_pgmar_attrs;

#[cfg(test)]
mod tests;
