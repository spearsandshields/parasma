/**
 * CSV round-trip — the CSV→grid half. CSV is converted
 * browser-side to a one-sheet .xlsx (`csvToXlsx`), so it flows through the SAME
 * SheetIndex + grid + cell-selection path as any xlsx — grid selection/copy
 * covers CSV for free.
 *
 * This pins that `csvToXlsx` embeds the CSV values into the worksheet the grid
 * reads (single sheet "CSV", inline strings + numbers, no merges/styles — the
 * clean baseline). The engine EXTRACT half is pinned natively by
 * `sml::tests::extract_tests::csv_baseline_extract_matches_the_cell_values`; the
 * grid→TSV serialization half by `flavors/sml.test.ts` (the CSV-shaped case). The
 * full wasm drag-select round-trip is a browser step (no vitest wasm harness).
 */
import { describe, expect, it } from "vitest";
import { unzipSync, strFromU8 } from "fflate";
import { csvToXlsx, parseCsv } from "./csv";

const CSV = 'name,qty\r\nApple,5\r\n"Pie, ""special""",2\r\n';

describe("CSV round-trip — CSV→grid", () => {
  it("parseCsv honors RFC 4180 quoting", () => {
    expect(parseCsv(CSV)).toEqual([
      ["name", "qty"],
      ["Apple", "5"],
      ['Pie, "special"', "2"],
    ]);
  });

  it("csvToXlsx embeds the CSV values into the sheet the grid reads", () => {
    const xlsx = csvToXlsx(CSV);
    // A real zip (PK signature) routed through the SAME loadXlsx path as any xlsx.
    expect(xlsx[0]).toBe(0x50);
    expect(xlsx[1]).toBe(0x4b);
    const files = unzipSync(xlsx);
    const sheetBytes = files["xl/worksheets/sheet1.xml"];
    const wbBytes = files["xl/workbook.xml"];
    expect(sheetBytes).toBeDefined();
    expect(wbBytes).toBeDefined();
    const sheet = strFromU8(sheetBytes!);
    expect(sheet).toContain(">name</t>");
    expect(sheet).toContain("<v>5</v>"); // numeric cell → right-aligned number
    expect(sheet).toContain("Pie, &quot;special&quot;"); // quote-bearing field, XML-escaped
    // Single sheet named "CSV" (no merges/styles/hidden — the clean baseline).
    expect(strFromU8(wbBytes!)).toContain('name="CSV"');
  });
});
