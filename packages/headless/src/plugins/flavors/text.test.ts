/**
 * text/plain flavor tests. Pure-TS over frozen
 * DocFragment fixtures (no wasm). Pins list-marker prefixes, empty paras,
 * verbatim uppercase small caps, footnote `[N]`, LOGICAL order, image omission.
 */
import { describe, it, expect } from "vitest";
import type { DocFragment, FragBlock, FragPara, FragRun } from "@parasma/core";
import { serializeTextPlain, paraToText } from "./text";

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

function frag(blocks: FragBlock[]): DocFragment {
  return { blocks };
}

describe("serializeTextPlain", () => {
  it("a single paragraph has no trailing newline", () => {
    expect(serializeTextPlain(frag([para([run("Hello")])]))).toBe("Hello");
  });

  it("two paragraphs join with \\n", () => {
    expect(serializeTextPlain(frag([para([run("A")]), para([run("B")])]))).toBe("A\nB");
  });

  it("an empty middle paragraph is a blank line", () => {
    expect(serializeTextPlain(frag([para([run("A")]), para([]), para([run("C")])]))).toBe("A\n\nC");
  });

  it("a numbered marker prefixes the line", () => {
    const p = para([run("Foo")], {
      list: { ordered: true, numId: 1, level: 0, value: 1, fmt: "decimal", marker: "1." },
    });
    expect(serializeTextPlain(frag([p]))).toBe("1. Foo");
  });

  it("a bullet marker prefixes the line", () => {
    const p = para([run("Item")], {
      list: { ordered: false, numId: 2, level: 0, value: 0, fmt: "bullet", marker: "•" },
    });
    expect(paraToText(p.kind === "para" ? p.para : ({} as FragPara))).toBe("• Item");
  });

  it("multiple runs concatenate with NO separator; a link run emits text only", () => {
    const p = para([run("Hello "), run("world", { link: "url:https://x" }), run("!")]);
    expect(serializeTextPlain(frag([p]))).toBe("Hello world!");
  });

  it("mixed LTR/RTL runs concatenate in LOGICAL order (not reversed)", () => {
    // The fragment is ALREADY logical order; serialization must not reverse it.
    const p = para([run("abc"), run("שלום")], { dir: "rtl" });
    expect(serializeTextPlain(frag([p]))).toBe("abcשלום");
  });

  it("pre-uppercased smallCaps text is emitted verbatim", () => {
    expect(serializeTextPlain(frag([para([run("HELLO")])]))).toBe("HELLO");
  });

  it("a footnote run emits [N] and omits nothing else", () => {
    const p = para([run("Text"), run("", { footnoteMark: "1" }), run(" more")]);
    expect(serializeTextPlain(frag([p]))).toBe("Text[1] more");
  });

  it("an inline image run contributes no characters (omitted)", () => {
    const p = para([run("A"), run("", { image: { relId: "rId5", wPx: 10, hPx: 10 } }), run("B")]);
    expect(serializeTextPlain(frag([p]))).toBe("AB");
  });

  it("an APPENDED anchored-image run leaves text/plain unchanged", () => {
    // The engine appends a trailing zero-width image run when a floating
    // image's anchor paragraph is swept into the copied range; text/plain
    // must serialize byte-identically to the text-only paragraph.
    const anchored = run("", {
      fontFamily: "",
      fontSizePt: 0,
      color: "",
      image: { relId: "rId9", wPx: 200, hPx: 150 },
    });
    const withFloat = frag([para([run("around the float"), anchored])]);
    const textOnly = frag([para([run("around the float")])]);
    expect(serializeTextPlain(withFloat)).toBe(serializeTextPlain(textOnly));
  });
});
