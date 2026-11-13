/**
 * Sanitizer-primitive tests. Pure, adversarial. These
 * pin the SOLE security choke for the text/html flavor: escaping, the href
 * scheme allowlist (incl. control-char smuggling), color/highlight normalization,
 * font-family CSS-breakout stripping, and finite/bounded number coercion.
 */
import { describe, it, expect } from "vitest";
import {
  clampInt,
  clampNumber,
  escapeAttr,
  escapeText,
  hrefFromLink,
  normalizeColor,
  normalizeHighlight,
  sanitizeFontFamily,
  sanitizeHref,
} from "./sanitize";

describe("escapeText", () => {
  it("escapes & < > with ampersand FIRST", () => {
    expect(escapeText("&<>")).toBe("&amp;&lt;&gt;");
    expect(escapeText("&lt;")).toBe("&amp;lt;");
  });
  it("does NOT touch quotes", () => {
    expect(escapeText(`"'`)).toBe(`"'`);
  });
});

describe("escapeAttr", () => {
  it("adds quote escaping on top of the text set", () => {
    expect(escapeAttr(`"'`)).toBe("&quot;&#39;");
    expect(escapeAttr(`a<b>"c'`)).toBe("a&lt;b&gt;&quot;c&#39;");
  });
});

describe("sanitizeHref", () => {
  it("passes the allowlisted schemes unchanged", () => {
    expect(sanitizeHref("http://x")).toBe("http://x");
    expect(sanitizeHref("https://x")).toBe("https://x");
    expect(sanitizeHref("mailto:a@b.com")).toBe("mailto:a@b.com");
    expect(sanitizeHref("tel:+15551234")).toBe("tel:+15551234");
  });
  it("is case-insensitive on the scheme", () => {
    expect(sanitizeHref("HTTPS://x")).toBe("HTTPS://x");
  });
  it("rejects dangerous schemes → null", () => {
    expect(sanitizeHref("javascript:alert(1)")).toBeNull();
    expect(sanitizeHref("data:text/html,x")).toBeNull();
    expect(sanitizeHref("file:///")).toBeNull();
    expect(sanitizeHref("vbscript:msgbox")).toBeNull();
  });
  it("rejects leading-whitespace-cloaked javascript:", () => {
    expect(sanitizeHref("  javascript:alert(1)")).toBeNull();
  });
  it("rejects control-char smuggling", () => {
    expect(sanitizeHref("java\tscript:alert(1)")).toBeNull();
    expect(sanitizeHref("java\nscript:alert(1)")).toBeNull();
  });
  it("rejects schemeless / relative / anchor / empty / protocol-relative", () => {
    expect(sanitizeHref("/rel")).toBeNull();
    expect(sanitizeHref("#a")).toBeNull();
    expect(sanitizeHref("")).toBeNull();
    expect(sanitizeHref("//host")).toBeNull();
  });
});

describe("hrefFromLink", () => {
  it("unwraps a url: prefix through the allowlist", () => {
    expect(hrefFromLink("url:https://x")).toBe("https://x");
    expect(hrefFromLink("url:javascript:alert(1)")).toBeNull();
  });
  it("drops anchor: and null", () => {
    expect(hrefFromLink("anchor:foo")).toBeNull();
    expect(hrefFromLink(null)).toBeNull();
  });
});

describe("normalizeColor", () => {
  it("normalizes 6-hex to lowercase #rrggbb", () => {
    expect(normalizeColor("#FF0000")).toBe("#ff0000");
    expect(normalizeColor("00ff00")).toBe("#00ff00");
  });
  it("expands 3-hex shorthand", () => {
    expect(normalizeColor("#abc")).toBe("#aabbcc");
  });
  it("rejects non-hex / malformed → null", () => {
    expect(normalizeColor("red")).toBeNull();
    expect(normalizeColor("auto")).toBeNull();
    expect(normalizeColor("red;}body{")).toBeNull();
    expect(normalizeColor("rgb(0,0,0)")).toBeNull();
    expect(normalizeColor("#12")).toBeNull();
    expect(normalizeColor("#1234567")).toBeNull();
    expect(normalizeColor("")).toBeNull();
    expect(normalizeColor(null)).toBeNull();
    expect(normalizeColor(undefined)).toBeNull();
  });
});

describe("normalizeHighlight", () => {
  it("maps ST_HighlightColor names to hex, case-insensitively", () => {
    expect(normalizeHighlight("yellow")).toBe("#ffff00");
    expect(normalizeHighlight("darkBlue")).toBe("#000080");
    expect(normalizeHighlight("YELLOW")).toBe("#ffff00");
  });
  it("rejects none / unknown / breakout / null", () => {
    expect(normalizeHighlight("none")).toBeNull();
    expect(normalizeHighlight("chartreuse")).toBeNull();
    expect(normalizeHighlight("evil;}x{")).toBeNull();
    expect(normalizeHighlight(null)).toBeNull();
  });
});

describe("sanitizeFontFamily", () => {
  it("quotes a clean family", () => {
    expect(sanitizeFontFamily("Calibri")).toBe("'Calibri'");
  });
  it("strips CSS-breakout characters", () => {
    expect(sanitizeFontFamily(`Ari"al`)).toBe("'Arial'");
    expect(sanitizeFontFamily("x;} body {")).toBe("'x body'");
    // Angle brackets + quotes stripped (no live tag / breakout); the harmless
    // slash survives inside the quotes.
    expect(sanitizeFontFamily("<b>Arial</b>")).toBe("'bArial/b'");
    expect(sanitizeFontFamily("a'b")).toBe("'ab'");
    expect(sanitizeFontFamily("expression(alert(1))")).toBe("'expressionalert1'");
  });
  it("returns null when nothing survives", () => {
    expect(sanitizeFontFamily("")).toBeNull();
    expect(sanitizeFontFamily("   ")).toBeNull();
    expect(sanitizeFontFamily(null)).toBeNull();
  });
});

describe("clampNumber", () => {
  it("rejects non-finite → null", () => {
    expect(clampNumber(NaN, 1, 1638)).toBeNull();
    expect(clampNumber(Infinity, 1, 1638)).toBeNull();
    expect(clampNumber(-Infinity, 1, 1638)).toBeNull();
  });
  it("clamps to bounds", () => {
    expect(clampNumber(5000, 1, 1638)).toBe(1638);
    expect(clampNumber(-3, 1, 1638)).toBe(1);
    expect(clampNumber(12, 1, 1638)).toBe(12);
  });
});

describe("clampInt", () => {
  it("rejects non-finite → null", () => {
    expect(clampInt(NaN, 1, 1_000_000)).toBeNull();
  });
  it("clamps and rounds", () => {
    expect(clampInt(1e9, 1, 1_000_000)).toBe(1_000_000);
    expect(clampInt(1.6, 0, 100)).toBe(2);
    expect(clampInt(0.4, 0, 100)).toBe(0);
  });
});
