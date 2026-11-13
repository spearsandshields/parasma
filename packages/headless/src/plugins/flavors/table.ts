/**
 * Table plain-text/TSV serialization. ONE parameterized
 * emitter shared by WML plain-text (`\t` between cells, `\n` per row — Word
 * convention, pastes into Excel as a grid) and an SML-style TSV (`\r\n` rows +
 * Excel quote-wrapping). `vMergedAway` continuation cells are skipped (text/plain
 * filters them); a multi-paragraph cell joins its blocks with `\n`.
 *
 * The cell walker uses the shared `blockToText` (text.ts) so a cell paragraph and
 * a body paragraph serialize identically and nested tables recurse.
 */
import type { FragTable } from "@parasma/core";
import { blockToText } from "./text";

/** How a table flattens to text: the row/cell separators + Excel quoting. */
export interface TableTextOpts {
  rowSep: string;
  cellSep: string;
  quote: boolean;
}

/** WML plain-text tables: tab cells, newline rows, no quoting. */
export const WML_TABLE: TableTextOpts = { rowSep: "\n", cellSep: "\t", quote: false };

/**
 * SML-style TSV (NOT registered as a flavor — the cells-range flavor lives in
 * sml.ts): CRLF rows + Excel quote-wrapping. Kept here so the shared emitter is
 * proven parameterizable.
 */
export const SML_TSV: TableTextOpts = { rowSep: "\r\n", cellSep: "\t", quote: true };

/** Excel-style quote-wrap: wrap in `"`, doubling interior quotes, if needed. */
function quoteIf(s: string, o: TableTextOpts): string {
  if (o.quote && /[\t\r\n"]/.test(s)) {
    return `"${s.replace(/"/g, '""')}"`;
  }
  return s;
}

/** Serialize a table with the given row/cell separators + quoting policy. */
export function serializeTable(t: FragTable, o: TableTextOpts): string {
  return t.rows
    .map((row) =>
      row.cells
        .filter((c) => !c.vMergedAway)
        .map((c) => quoteIf(c.blocks.map(blockToText).join("\n"), o))
        .join(o.cellSep),
    )
    .join(o.rowSep);
}
