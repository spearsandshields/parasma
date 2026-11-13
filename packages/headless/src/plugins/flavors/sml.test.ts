/**
 * SML cells-range flavor tests: TSV (Excel quoting,
 * covered-cell empty columns) + HTML `<table>` (colspan/rowspan, covered skip,
 * escaping). Pure — frozen CellFragment fixtures, no wasm.
 */
import { describe, expect, it } from "vitest";
import type { CellFragment } from "@parasma/core";
import { quoteTsvCell, serializeCellsHtml, serializeCellsTsv } from "./sml";

const cell = (display: string, extra: Partial<CellFragment["rows"][0]["cells"][0]> = {}) => ({
  display,
  rowspan: 1,
  colspan: 1,
  covered: false,
  ...extra,
});

const frag = (rows: CellFragment["rows"], over: Partial<CellFragment> = {}): CellFragment => ({
  rows,
  rowCount: rows.length,
  totalRows: rows.length,
  truncated: false,
  reason: null,
  ...over,
});

describe("serializeCellsTsv", () => {
  it("tabs between cells and CRLF between rows", () => {
    const f = frag([
      { cells: [cell("A1"), cell("B1")] },
      { cells: [cell("A2"), cell("B2")] },
    ]);
    expect(serializeCellsTsv(f)).toBe("A1\tB1\r\nA2\tB2");
  });

  it("emits empty columns for model-less and covered cells (alignment preserved)", () => {
    const f = frag([{ cells: [cell("x"), cell(""), cell("", { covered: true })] }]);
    expect(serializeCellsTsv(f)).toBe("x\t\t");
  });

  it("Excel-quotes cells containing tab, newline, or quote", () => {
    expect(quoteTsvCell("plain")).toBe("plain");
    expect(quoteTsvCell("a\tb")).toBe('"a\tb"');
    expect(quoteTsvCell("line1\r\nline2")).toBe('"line1\r\nline2"');
    expect(quoteTsvCell('say "hi"')).toBe('"say ""hi"""');
    const f = frag([{ cells: [cell('has "q"'), cell("a\tb")] }]);
    expect(serializeCellsTsv(f)).toBe('"has ""q"""\t"a\tb"');
  });
});

describe("serializeCellsHtml", () => {
  it("wraps rows/cells in a border-collapse table", () => {
    const f = frag([{ cells: [cell("A"), cell("B")] }]);
    expect(serializeCellsHtml(f)).toBe(
      '<table style="border-collapse:collapse"><tr><td>A</td><td>B</td></tr></table>',
    );
  });

  it("carries colspan/rowspan on the anchor and SKIPS covered cells", () => {
    // A1:C1 merge — anchor colspan 3, B1/C1 covered (skipped).
    const f = frag([
      {
        cells: [
          cell("Invoice", { colspan: 3 }),
          cell("", { covered: true }),
          cell("", { covered: true }),
        ],
      },
    ]);
    expect(serializeCellsHtml(f)).toBe(
      '<table style="border-collapse:collapse"><tr><td colspan="3">Invoice</td></tr></table>',
    );
  });

  it("escapes text nodes", () => {
    const f = frag([{ cells: [cell("<b>&x</b>")] }]);
    expect(serializeCellsHtml(f)).toBe(
      '<table style="border-collapse:collapse"><tr><td>&lt;b&gt;&amp;x&lt;/b&gt;</td></tr></table>',
    );
  });
});

describe("CSV round-trip — grid→TSV", () => {
  it("the extracted CSV field values re-serialize to the expected TSV", () => {
    // The CellFragment the engine extracts for the full rect of the CSV
    // "name,qty / Apple,5 / Pie, \"special\",2" (values pinned natively). TSV
    // joins them with tabs / CRLF and Excel-quotes the comma+quote field, so the
    // copy a user pastes back matches the CSV.
    const csvGrid = [
      ["name", "qty"],
      ["Apple", "5"],
      ['Pie, "special"', "2"],
    ];
    const f = frag(csvGrid.map((row) => ({ cells: row.map((v) => cell(v)) })));
    expect(serializeCellsTsv(f)).toBe('name\tqty\r\nApple\t5\r\n"Pie, ""special"""\t2');
  });
});
