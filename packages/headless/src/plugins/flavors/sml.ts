/**
 * SML cells-range serialization. Two flavors over the
 * engine's flavor-neutral {@link CellFragment}:
 *
 *  - text/plain IS the TSV: `\t` between cells, `\r\n` per row, Excel
 *    quote-wrapping (double the quote and wrap when a cell holds a tab / newline /
 *    quote). A merge's COVERED cells emit an EMPTY column so alignment is
 *    preserved (Excel-parity — the anchor holds the value, covered cells are
 *    empty tabs).
 *  - text/html is a `<table>` with colspan/rowspan on merge anchors, covered
 *    cells SKIPPED (spanned under the anchor), and formatted display text escaped
 *    through the shared `escapeText` primitive.
 *
 * Formatted `CellModel.text` only (no raw value in v1). Pure + dependency-free
 * (unit-testable on frozen fragments), mutually exclusive with the WML/PML
 * text/object flavors (gated on the `sml-range` SelectionKind).
 */
import type { CellFragment } from "@parasma/core";
import { escapeText } from "./sanitize";

/** Excel-style TSV quote-wrap: wrap in `"`, doubling interior quotes, when
 *  the cell contains a tab / CR / LF / quote. */
export function quoteTsvCell(s: string): string {
  return /[\t\r\n"]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
}

/** SML text/plain = TSV. Every cell in the row is emitted (covered merge
 *  cells as an empty column), so paste-into-a-grid column alignment holds. */
export function serializeCellsTsv(frag: CellFragment): string {
  return frag.rows
    .map((row) => row.cells.map((c) => quoteTsvCell(c.display)).join("\t"))
    .join("\r\n");
}

/** SML text/html = a `<table>`: merge anchors carry colspan/rowspan, covered
 *  cells are skipped (spanned under the anchor), text escaped. */
export function serializeCellsHtml(frag: CellFragment): string {
  const rows = frag.rows
    .map((row) => {
      const cells = row.cells
        .filter((c) => !c.covered)
        .map((c) => {
          const span =
            (c.colspan > 1 ? ` colspan="${c.colspan}"` : "") +
            (c.rowspan > 1 ? ` rowspan="${c.rowspan}"` : "");
          return `<td${span}>${escapeText(c.display)}</td>`;
        })
        .join("");
      return `<tr>${cells}</tr>`;
    })
    .join("");
  return `<table style="border-collapse:collapse">${rows}</table>`;
}
