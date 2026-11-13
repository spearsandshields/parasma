//! Containment shim for strict `<col>` `@min`/`@max` parse failures.
//!
//! `sml-styling` makes `Column.start_column`/`end_column` (the `<col>`
//! `@min`/`@max` attrs) REQUIRED, non-`Option` `u32` fields. A real-world
//! worksheet whose `<col>` omits either attribute makes the strict generated
//! parser return `ParseError::MissingAttribute("min"|"max")` — and
//! `Workbook::resolved_sheets()` propagates that to fail the WHOLE workbook.
//!
//! This shim NEVER calls `resolved_sheets()`. It loads sheets one index at a
//! time and applies two layers of tolerance:
//!
//!  - **Layer 1 — per-sheet containment.** Each sheet is loaded independently
//!    via `Workbook::resolved_sheet(i)`; one sheet's parse error can never abort
//!    the others. A sheet that still fails after Layer 2 is dropped with a
//!    recorded warning rather than failing the workbook.
//!  - **Layer 2 — render-only `<col>` pre-repair.** On a `MissingAttribute`
//!    failure we fetch the RAW worksheet XML (`Workbook::sheet_xml(i)`), inject
//!    the missing `@min`/`@max` (min = running column index, max = min) into each
//!    deficient `<col>` start tag, re-parse with `parse_worksheet`, and rebuild
//!    a `ResolvedSheet` carrying the recovered widths.
//!
//! This is a deliberate, **sml-scoped, render-only** departure from the parser's
//! "never normalize the XML" editability guarantee: that guarantee protects the
//! *persisted* model, and the SML render path is read-only (it never
//! re-serializes the worksheet). Documented with a §18.3.1.13 citation.

use std::io::{Read, Seek};

use ooxml_sml::ext::{parse_worksheet, ResolvedSheet};
use ooxml_sml::parsers::ParseError;
use ooxml_sml::workbook::Workbook;

/// A warning recorded while loading a sheet, for later surfacing via
/// documentInfo.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SheetWarning {
    /// A sheet's `<col>` widths were recovered via the render-only pre-repair
    /// (§18.3.1.13); column widths fell back to defaults for the repaired runs.
    ColWidthsRepaired { sheet: usize },
    /// A sheet could not be parsed even after repair and was dropped.
    SheetDropped { sheet: usize, reason: String },
}

/// The outcome of loading one sheet: the resolved sheet plus whether its column
/// widths were degraded by the repair.
pub(crate) struct LoadedSheet {
    pub(crate) sheet: ResolvedSheet,
    pub(crate) col_widths_degraded: bool,
}

/// Load every sheet of a workbook with `<col>` containment. Returns the loaded
/// sheets (in index order, skipping any that could not be recovered) and any
/// warnings.
pub(crate) fn load_sheets_tolerant<R: Read + Seek>(
    wb: &mut Workbook<R>,
) -> (Vec<LoadedSheet>, Vec<SheetWarning>) {
    let count = wb.sheet_count();
    let mut out = Vec::with_capacity(count);
    let mut warnings = Vec::new();

    // Shared strings captured from the first successful sheet, so a repaired
    // sheet (which we rebuild ourselves) still resolves shared-string cells.
    let mut shared_strings: Option<Vec<String>> = None;

    for i in 0..count {
        match wb.resolved_sheet(i) {
            Ok(sheet) => {
                if shared_strings.is_none() {
                    shared_strings = Some(sheet.context().shared_strings.clone());
                }
                out.push(LoadedSheet { sheet, col_widths_degraded: false });
            }
            Err(e) => {
                // Layer 2: render-only pre-repair, but only for the `<col>` path.
                match try_repair_sheet(wb, i, shared_strings.as_deref()) {
                    Some(sheet) => {
                        warnings.push(SheetWarning::ColWidthsRepaired { sheet: i });
                        out.push(LoadedSheet { sheet, col_widths_degraded: true });
                    }
                    None => {
                        warnings.push(SheetWarning::SheetDropped {
                            sheet: i,
                            reason: format!("{e}"),
                        });
                    }
                }
            }
        }
    }

    (out, warnings)
}

/// Attempt Layer-2 recovery for sheet `i`: fetch raw XML, repair `<col>`
/// `@min`/`@max`, re-parse, and rebuild a `ResolvedSheet`. Returns `None` if the
/// repaired XML still fails to parse.
fn try_repair_sheet<R: Read + Seek>(
    wb: &mut Workbook<R>,
    i: usize,
    shared_strings: Option<&[String]>,
) -> Option<ResolvedSheet> {
    let raw = wb.sheet_xml(i).ok()?;
    let repaired = crate::repair::sml_col_attrs(&raw);
    let worksheet = match parse_worksheet(&repaired) {
        Ok(ws) => ws,
        Err(ParseError::MissingAttribute(_)) => {
            // A second pass in case multiple <col>s were deficient and the
            // first repair missed one; repair_col_attrs already handles all, so
            // this is belt-and-suspenders — give up if it still fails.
            return None;
        }
        Err(_) => return None,
    };
    let name = wb
        .sheet_names()
        .get(i)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("Sheet{}", i + 1));
    let strings = shared_strings.map(|s| s.to_vec()).unwrap_or_default();
    Some(ResolvedSheet::new(name, worksheet, strings))
}

/// Render-only `<col>` `@min`/`@max` pre-repair (§18.3.1.13). MIGRATED to the
/// consolidated repair module ([`crate::repair::sml_col_attrs`], RULE 1); this
/// is a thin delegating wrapper kept so `shim`'s callers + tests (which name
/// `shim::repair_col_attrs`) are unchanged. Behavior is identical. Removed by
/// lenient fork codegen, at which point both this wrapper and the repair rule
/// are deleted.
pub(crate) fn repair_col_attrs(xml: &[u8]) -> Vec<u8> {
    crate::repair::sml_col_attrs(xml)
}
