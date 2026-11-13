/**
 * Table plain-text/TSV flavor tests. Pins WML tab
 * cells / newline rows, vMergedAway skipping, multi-para cell joins, and the
 * SML-style TSV (CRLF + Excel quote-wrapping) sharing ONE emitter.
 */
import { describe, it, expect } from "vitest";
import type { FragBlock, FragCell, FragRun, FragTable } from "@parasma/core";
import { serializeTable, WML_TABLE, SML_TSV } from "./table";

function run(text: string): FragRun {
  return {
    text,
    bold: false,
    italic: false,
    underline: "none",
    strike: false,
    doubleStrike: false,
    fontFamily: "Calibri",
    fontSizePt: 11,
    color: "#000000",
    highlight: null,
    runShading: null,
    vertAlign: "baseline",
    link: null,
    image: null,
    footnoteMark: null,
  };
}

function cellText(...texts: string[]): FragCell {
  return {
    rowspan: 1,
    colspan: 1,
    fill: null,
    borders: [null, null, null, null],
    vMergedAway: false,
    blocks: texts.map((t): FragBlock => ({
      kind: "para",
      para: { runs: t === "" ? [] : [run(t)], alignment: "start", dir: "ltr", list: null },
    })),
  };
}

function grid(rows: FragCell[][]): FragTable {
  return { rows: rows.map((cells) => ({ cells })) };
}

describe("serializeTable (WML plain text)", () => {
  it("2×2 grid uses tab between cells and newline between rows", () => {
    const t = grid([
      [cellText("A"), cellText("B")],
      [cellText("C"), cellText("D")],
    ]);
    expect(serializeTable(t, WML_TABLE)).toBe("A\tB\nC\tD");
  });

  it("an empty cell contributes an empty field", () => {
    const t = grid([[cellText(""), cellText("B")]]);
    expect(serializeTable(t, WML_TABLE)).toBe("\tB");
  });

  it("a single 1×1 cell has no separators", () => {
    expect(serializeTable(grid([[cellText("only")]]), WML_TABLE)).toBe("only");
  });

  it("vMergedAway continuation cells are skipped", () => {
    const merged: FragCell = { ...cellText(""), vMergedAway: true };
    const t = grid([[cellText("A"), merged, cellText("C")]]);
    expect(serializeTable(t, WML_TABLE)).toBe("A\tC");
  });

  it("a multi-paragraph cell joins its paragraphs with \\n", () => {
    const t = grid([[cellText("line1", "line2"), cellText("B")]]);
    expect(serializeTable(t, WML_TABLE)).toBe("line1\nline2\tB");
  });
});

describe("serializeTable (SML-style TSV, not a registered flavor)", () => {
  it("uses CRLF rows and quote-wraps cells containing tabs/newlines/quotes", () => {
    const t = grid([
      [cellText("plain"), cellText('has"quote')],
      [cellText("a\tb"), cellText("x")],
    ]);
    // Row sep CRLF; the quote cell doubles interior quotes; the tab cell wraps.
    expect(serializeTable(t, SML_TSV)).toBe('plain\t"has""quote"\r\n"a\tb"\tx');
  });
});
