/**
 * text/html flavor tests. Pure-TS over frozen DocFragment
 * fixtures (no wasm). Uses the REAL ./sanitize so the adversarial cases (20-25)
 * are the html↔sanitize contract gate. Pins the RTE dialect: semantic tags +
 * inline styles, list grouping (<ol type>/<li value>), table spans/borders/
 * fill, [N] footnotes, dir=rtl, <meta charset> envelope, and sanitization.
 */
import { describe, it, expect } from "vitest";
import type {
  DocFragment, FragBlock, FragCell, FragList, FragPara, FragRun, FragTable,
} from "@parasma/core";
import { serializeTextHtml, blocksToHtml, paraToHtml, runToHtml, tableToHtml } from "./html";

function run(text: string, over: Partial<FragRun> = {}): FragRun {
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
    ...over,
  };
}

function para(runs: FragRun[], over: Partial<FragPara> = {}): FragBlock {
  return { kind: "para", para: { runs, alignment: "start", dir: "ltr", list: null, ...over } };
}

function list(over: Partial<FragList> = {}): FragList {
  return { ordered: true, numId: 1, level: 0, value: 1, fmt: "decimal", marker: "1.", ...over };
}

function listPara(text: string, l: FragList): FragBlock {
  return para([run(text)], { list: l });
}

function cell(blocks: FragBlock[], over: Partial<FragCell> = {}): FragCell {
  return {
    rowspan: 1,
    colspan: 1,
    fill: null,
    borders: [null, null, null, null],
    vMergedAway: false,
    blocks,
    ...over,
  };
}

function table(rows: FragCell[][]): FragTable {
  return { rows: rows.map((cells) => ({ cells })) };
}

function frag(blocks: FragBlock[]): DocFragment {
  return { blocks };
}

/** The default-styled run span (font-family + font-size, black suppressed). */
function sp(text: string): string {
  return `<span style="font-family:'Calibri';font-size:11pt">${text}</span>`;
}

// ── structure / run formatting ──────────────────────────────────────────────
describe("runToHtml — direct formatting", () => {
  it("emits font-family + font-size, suppresses default #000000 color", () => {
    expect(runToHtml(run("Hello"))).toBe(
      `<span style="font-family:'Calibri';font-size:11pt">Hello</span>`,
    );
  });

  it("nests bold + italic outside the span (stable order span<b<i)", () => {
    expect(runToHtml(run("Hi", { bold: true, italic: true }))).toBe(
      `<span style="font-family:'Calibri';font-size:11pt"><b><i>Hi</i></b></span>`,
    );
  });

  it("single underline → <u>; strike → <s>", () => {
    expect(runToHtml(run("x", { underline: "single" }))).toContain("<u>");
    expect(runToHtml(run("x", { strike: true }))).toContain("<s>");
  });

  it("double underline → text-decoration longhands", () => {
    const h = runToHtml(run("x", { underline: "double" }));
    expect(h).toContain("text-decoration-line:underline");
    expect(h).toContain("text-decoration-style:double");
    expect(h).not.toContain("<u>");
  });

  it("doubleStrike → line-through / double", () => {
    const h = runToHtml(run("x", { doubleStrike: true }));
    expect(h).toContain("text-decoration-line:line-through");
    expect(h).toContain("text-decoration-style:double");
  });

  it("color #FF0000 → color:#ff0000; #000000 → no color decl", () => {
    expect(runToHtml(run("x", { color: "#FF0000" }))).toContain("color:#ff0000");
    expect(runToHtml(run("x", { color: "#000000" }))).not.toContain("color:#");
  });

  it("highlight yellow → background-color:#ffff00; runShading fallback; highlight wins", () => {
    expect(runToHtml(run("x", { highlight: "yellow" }))).toContain("background-color:#ffff00");
    expect(runToHtml(run("x", { runShading: "#EEEEEE" }))).toContain("background-color:#eeeeee");
    expect(runToHtml(run("x", { highlight: "yellow", runShading: "#EEEEEE" }))).toContain(
      "background-color:#ffff00",
    );
  });

  it("superscript → <sup>; subscript → <sub>", () => {
    expect(runToHtml(run("x", { vertAlign: "superscript" }))).toContain("<sup>");
    expect(runToHtml(run("x", { vertAlign: "subscript" }))).toContain("<sub>");
  });

  it("url: link → <a href>; anchor: and null → no <a>", () => {
    expect(runToHtml(run("x", { link: "url:https://x" }))).toContain(`<a href="https://x">`);
    expect(runToHtml(run("x", { link: "anchor:bkm" }))).not.toContain("<a");
    expect(runToHtml(run("x", { link: null }))).not.toContain("<a");
  });

  it("footnoteMark → [N] literal text", () => {
    expect(runToHtml(run("", { footnoteMark: "1" }))).toContain("[1]");
  });

  it("an image run WITHOUT a resolver degrades to <img alt>", () => {
    expect(runToHtml(run("", { image: { relId: "rId5", wPx: 10, hPx: 10 } }))).toBe(
      `<img alt="image" width="10" height="10">`,
    );
  });
});

describe("paraToHtml", () => {
  it("empty para → <p>&nbsp;</p>", () => {
    expect(paraToHtml({ runs: [], alignment: "start", dir: "ltr", list: null })).toBe(
      "<p>&nbsp;</p>",
    );
  });

  it("image-only para (no resolver) → <p><img alt></p>, not &nbsp;", () => {
    const p: FragPara = {
      runs: [run("", { image: { relId: "r", wPx: 1, hPx: 1 } })],
      alignment: "start",
      dir: "ltr",
      list: null,
    };
    expect(paraToHtml(p)).toBe(`<p><img alt="image" width="1" height="1"></p>`);
  });

  it("alignment center → text-align:center; both → justify; start → none", () => {
    expect(paraToHtml({ runs: [run("x")], alignment: "center", dir: "ltr", list: null })).toContain(
      `text-align:center`,
    );
    expect(paraToHtml({ runs: [run("x")], alignment: "both", dir: "ltr", list: null })).toContain(
      `text-align:justify`,
    );
    expect(paraToHtml({ runs: [run("x")], alignment: "start", dir: "ltr", list: null })).not.toContain(
      "text-align",
    );
  });

  it("dir rtl → <p ... dir=\"rtl\">", () => {
    expect(paraToHtml({ runs: [run("x")], alignment: "start", dir: "rtl", list: null })).toContain(
      `dir="rtl"`,
    );
  });
});

// ── list grouping ────────────────────────────────────────────────────────────
describe("blocksToHtml — list grouping", () => {
  it("nested ol: decimal(l0,v1) → lowerLetter(l1,v2) → decimal(l0,v3), deferred </li>", () => {
    const html = blocksToHtml([
      listPara("A", list({ level: 0, value: 1, fmt: "decimal" })),
      listPara("B", list({ level: 1, value: 2, fmt: "lowerLetter" })),
      listPara("C", list({ level: 0, value: 3, fmt: "decimal" })),
    ]);
    expect(html).toBe(
      `<ol type="1"><li value="1">${sp("A")}` +
        `<ol type="a"><li value="2">${sp("B")}</li></ol></li>` +
        `<li value="3">${sp("C")}</li></ol>`,
    );
  });

  it("mid-list first item keeps its value (survives mid-list copy)", () => {
    const html = blocksToHtml([listPara("X", list({ value: 3 }))]);
    expect(html).toContain(`<li value="3">`);
  });

  it("bullet list → <ul> + plain <li> (no value)", () => {
    const html = blocksToHtml([listPara("Item", list({ ordered: false, fmt: "bullet" }))]);
    expect(html).toBe(`<ul><li>${sp("Item")}</li></ul>`);
  });

  it("numId change between two l0 items closes and reopens the list", () => {
    const html = blocksToHtml([
      listPara("A", list({ numId: 1, value: 1 })),
      listPara("B", list({ numId: 2, value: 1 })),
    ]);
    expect(html).toBe(
      `<ol type="1"><li value="1">${sp("A")}</li></ol>` +
        `<ol type="1"><li value="1">${sp("B")}</li></ol>`,
    );
  });

  it("list → non-list para → list: the first list fully closes before <p>", () => {
    const html = blocksToHtml([
      listPara("A", list({ value: 1 })),
      para([run("mid")]),
      listPara("B", list({ value: 1 })),
    ]);
    expect(html).toBe(
      `<ol type="1"><li value="1">${sp("A")}</li></ol>` +
        `<p>${sp("mid")}</p>` +
        `<ol type="1"><li value="1">${sp("B")}</li></ol>`,
    );
  });

  it("olType map: lowerRoman→i, upperRoman→I, upperLetter→A, lowerLetter→a; decimalZero→untyped", () => {
    expect(blocksToHtml([listPara("x", list({ fmt: "lowerRoman" }))])).toContain(`<ol type="i">`);
    expect(blocksToHtml([listPara("x", list({ fmt: "upperRoman" }))])).toContain(`<ol type="I">`);
    expect(blocksToHtml([listPara("x", list({ fmt: "upperLetter" }))])).toContain(`<ol type="A">`);
    expect(blocksToHtml([listPara("x", list({ fmt: "lowerLetter" }))])).toContain(`<ol type="a">`);
    expect(blocksToHtml([listPara("x", list({ fmt: "decimalZero" }))])).toContain(`<ol>`);
    expect(blocksToHtml([listPara("x", list({ fmt: "decimalZero" }))])).not.toContain("type=");
  });
});

// ── tables ───────────────────────────────────────────────────────────────────
describe("tableToHtml", () => {
  it("2×2 grid → border-collapse table with rows/cells", () => {
    const t = table([
      [cell([para([run("A")])]), cell([para([run("B")])])],
      [cell([para([run("C")])]), cell([para([run("D")])])],
    ]);
    const html = tableToHtml(t);
    expect(html).toContain(`<table style="border-collapse:collapse"><tr><td>`);
    expect((html.match(/<tr>/g) ?? []).length).toBe(2);
    expect((html.match(/<td>/g) ?? []).length).toBe(4);
  });

  it("colspan/rowspan only when > 1", () => {
    const t = table([[cell([para([run("A")])], { colspan: 2, rowspan: 3 }), cell([para([run("B")])])]]);
    const html = tableToHtml(t);
    expect(html).toContain(`colspan="2"`);
    expect(html).toContain(`rowspan="3"`);
    // The 1×1 sibling has neither.
    expect((html.match(/colspan/g) ?? []).length).toBe(1);
  });

  it("vMergedAway cells are skipped (span lives on the anchor)", () => {
    const t = table([[cell([para([run("A")])]), cell([para([run("")])], { vMergedAway: true })]]);
    expect((tableToHtml(t).match(/<td/g) ?? []).length).toBe(1);
  });

  it("border {style:single,color:#FF0000,widthPx:1} → border-top:1px solid #ff0000; fill → bg", () => {
    const t = table([
      [
        cell([para([run("A")])], {
          borders: [{ style: "single", color: "#FF0000", widthPx: 1 }, null, null, null],
          fill: "#00FF00",
        }),
      ],
    ]);
    const html = tableToHtml(t);
    expect(html).toContain("border-top:1px solid #ff0000");
    expect(html).toContain("background-color:#00ff00");
  });

  it("a cell with a nested list recurses through the shared walker", () => {
    const t = table([[cell([listPara("A", list({ value: 1 }))])]]);
    expect(tableToHtml(t)).toContain(`<ol type="1"><li value="1">${sp("A")}</li></ol>`);
  });

  it("an empty cell → <td>&nbsp;</td>", () => {
    const t = table([[cell([])]]);
    expect(tableToHtml(t)).toContain("<td>&nbsp;</td>");
  });
});

describe("serializeTextHtml — envelope", () => {
  it("prefixes the <meta charset> envelope", () => {
    expect(serializeTextHtml(frag([para([run("Hi")])]))).toMatch(/^<meta charset="utf-8">/);
  });
});

// ── adversarial sanitization ─────────────────────────────────────────────────
describe("sanitization (adversarial)", () => {
  it("escapes hostile run text — no live tag survives", () => {
    const h = runToHtml(run(`<script>alert(1)</script>&"`));
    expect(h).toContain("&lt;script&gt;alert(1)&lt;/script&gt;&amp;");
    expect(h).not.toContain("<script>");
  });

  it("javascript:/data:/file: links emit text only, no <a>, no scheme substring", () => {
    for (const link of ["url:javascript:alert(1)", "url:data:text/html,x", "url:file:///"]) {
      const h = runToHtml(run("click", { link }));
      expect(h).not.toContain("<a");
      expect(h).not.toContain("javascript:");
      expect(h).not.toContain("data:");
      expect(h).not.toContain("file:");
    }
  });

  it("style-breakout color drops the whole decl", () => {
    const h = runToHtml(run("x", { color: "red;} body{display:none" }));
    expect(h).not.toContain("color:");
    expect(h).not.toContain("red;}");
  });

  it("font-family CSS breakout is stripped to a safe quoted token", () => {
    const h = runToHtml(run("x", { fontFamily: `Arial";x:expression(</style>` }));
    expect(h).not.toContain(`"`.repeat(1) + ";");
    expect(h).not.toContain("expression(");
    expect(h).not.toContain("</style>");
    expect(h).toContain("font-family:'");
  });

  it("crafted font-family cannot break out of the style attribute into a tag", () => {
    const h = runToHtml(run("x", { fontFamily: `x"></span><img src=x onerror=alert(1)>` }));
    expect(h).not.toContain("<img");
    expect(h).not.toContain(`"><`);
  });

  it("non-finite / out-of-range numbers are coerced or omitted", () => {
    expect(runToHtml(run("x", { fontSizePt: Infinity }))).not.toContain("font-size");
    expect(runToHtml(run("x", { fontSizePt: NaN }))).not.toContain("NaNpt");
    expect(runToHtml(run("x", { fontSizePt: -5 }))).toContain("font-size:1pt");
    expect(runToHtml(run("x", { fontSizePt: 1e9 }))).toContain("font-size:1638pt");

    // colspan 1e9 clamps; NaN omits the attr.
    const clamped = tableToHtml(table([[cell([para([run("A")])], { colspan: 1e9 })]]));
    expect(clamped).toContain(`colspan="1000000"`);
    const nanSpan = tableToHtml(table([[cell([para([run("A")])], { colspan: NaN })]]));
    expect(nanSpan).not.toContain("colspan");

    // list value NaN → <li value="0">.
    expect(blocksToHtml([listPara("A", list({ value: NaN }))])).toContain(`<li value="0">`);
  });

  it("angle brackets in the list marker never surface (marker not emitted in HTML)", () => {
    const html = blocksToHtml([listPara("A", list({ marker: "<b>1.</b>" }))]);
    expect(html).not.toContain("<b>1.");
  });
});

// ── inline images ────────────────────────────────────────────────────────────
describe("runToHtml — inline images", () => {
  const imgRun = (relId: string) =>
    run("", { image: { relId, wPx: 120, hPx: 80 } });

  it("WITH a resolver emits <img src=data:…> at DISPLAY px", () => {
    const resolver = (r: string) => (r === "rId9" ? "data:image/png;base64,AAEC" : null);
    expect(runToHtml(imgRun("rId9"), resolver)).toBe(
      `<img src="data:image/png;base64,AAEC" width="120" height="80">`,
    );
  });

  it("WITHOUT a resolver degrades to <img alt> — never base64 (cache invariant)", () => {
    const out = runToHtml(imgRun("rId9"));
    expect(out).toBe(`<img alt="image" width="120" height="80">`);
    expect(out).not.toContain("base64");
  });

  it("an unresolved relId (resolver miss) degrades to <img alt>", () => {
    expect(runToHtml(imgRun("rIdX"), () => null)).toBe(
      `<img alt="image" width="120" height="80">`,
    );
  });

  it("an image-only paragraph emits the <img>, not &nbsp;", () => {
    const resolver = () => "data:image/png;base64,AAEC";
    const html = serializeTextHtml(frag([para([run("", { image: { relId: "r", wPx: 10, hPx: 10 } })])]), undefined, resolver);
    expect(html).toContain("<img src=");
    expect(html).not.toContain("&nbsp;");
  });

  it("serializeTextHtml without a resolver never emits base64 (per-selection cache stays image-free)", () => {
    const html = serializeTextHtml(frag([para([run("", { image: { relId: "r", wPx: 10, hPx: 10 } })])]));
    expect(html).not.toContain("base64");
    expect(html).toContain(`<img alt="image"`);
  });
});

// ── ANCHORED (floating) images ride along on text copy ───────────────────────
// The engine appends a trailing zero-width image run to a paragraph whose
// anchor is swept into the copied range (para-level v1). The TS serializers
// need ZERO new arms — these tests pin that the appended shape flows through
// the EXISTING image-run path.
describe("anchored-image runs appended by extractRange", () => {
  /** The engine's appended anchored-image run: empty text + neutral style. */
  const anchoredRun = (relId: string): FragRun =>
    run("", {
      fontFamily: "",
      fontSizePt: 0,
      color: "",
      image: { relId, wPx: 200, hPx: 150 },
    });

  it("copy time (resolver present) → text runs then <img src=data:…> at DISPLAY px", () => {
    const resolver = (r: string) => (r === "rId9" ? "data:image/png;base64,AAEC" : null);
    const html = serializeTextHtml(
      frag([para([run("around the float"), anchoredRun("rId9")])]),
      undefined,
      resolver,
    );
    expect(html).toContain("around the float");
    expect(html).toContain(`<img src="data:image/png;base64,AAEC" width="200" height="150">`);
  });

  it("pre-serialized cache (no resolver) → <img alt>, never base64", () => {
    const html = serializeTextHtml(frag([para([run("text"), anchoredRun("rId9")])]));
    expect(html).toContain(`<img alt="image" width="200" height="150">`);
    expect(html).not.toContain("base64");
  });
});
