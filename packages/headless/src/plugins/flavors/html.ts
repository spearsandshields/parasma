/**
 * text/html serialization. The RTE ceiling: computed
 * DIRECT formatting via inline styles + semantic tags (NO named
 * styles, NO font-variant:small-caps). ONE shared block+list walker
 * (`blocksToHtml`) feeds both the top level AND table cells so structure cannot
 * diverge (the same discipline as text.ts). LOGICAL order by construction.
 *
 * NO-BYPASS INVARIANT (grep-verifiable): every doc-controlled string reaching
 * output flows through ./sanitize (escapeText / escapeAttr / normalizeColor /
 * normalizeHighlight / sanitizeFontFamily / hrefFromLink); every doc number
 * through clampNumber/clampInt; enum-ish fields (underline/vertAlign/alignment/
 * dir/fmt/border-style) are mapped to a FIXED literal token set, never emitted
 * raw. Style values are built ONLY from validated primitives → NOT escapeAttr'd.
 *
 * Call-site table (the no-bypass checklist — every doc-derived field → its sink):
 *   FragRun.text, .footnoteMark (as [N])            → escapeText
 *   FragRun.fontFamily                              → sanitizeFontFamily
 *   FragRun.color/.runShading, FragCell.fill,
 *     FragBorder.color                              → normalizeColor
 *   FragRun.highlight                               → normalizeHighlight
 *   FragRun.fontSizePt, FragBorder.widthPx          → clampNumber
 *   FragList.value, FragCell.colspan/.rowspan       → clampInt
 *   FragRun.link                                    → hrefFromLink → escapeAttr
 *   underline/vertAlign/alignment/dir/fmt/style     → FIXED literal token maps
 *   FragList.marker                                 → NOT emitted in HTML
 */
import type {
  DocFragment, FragBlock, FragCell, FragImage, FragList, FragPara, FragRow, FragRun, FragTable,
} from "@parasma/core";
import { DEFAULT_BUDGETS, type CopyBudgets, type ImageResolver } from "./types";
import {
  clampInt, clampNumber, escapeAttr, escapeText, hrefFromLink,
  normalizeColor, normalizeHighlight, sanitizeFontFamily,
} from "./sanitize";

const FONT_PT_MIN = 1;
const FONT_PT_MAX = 1638; // Word sz half-point ceiling
const SPAN_MAX = 1_000_000; // absurd-value guard for spans/values
const BORDER_PX_MAX = 100;
const MAX_LIST_DEPTH = 32; // list-nesting cap (Word max is 9); bounds doc-controlled FragList.level

// ── inline image ─────────────────────────────────────────────────────────────
/** ` width="W" height="H"` (intrinsic or display px), zero/absurd dims omitted. */
export function imgDims(wPx: number, hPx: number): string {
  const w = clampInt(wPx, 0, SPAN_MAX);
  const h = clampInt(hPx, 0, SPAN_MAX);
  let s = "";
  if (w != null && w > 0) s += ` width="${w}"`;
  if (h != null && h > 0) s += ` height="${h}"`;
  return s;
}

/** An inline image run → `<img src="data:…">` (resolver hit) or `<img alt>` (miss).
 *  The resolver's URL is pre-validated (allowlisted mime + base64 alphabet), so
 *  it is emitted UNescaped (documented invariant, same as validated style values). */
function imgToHtml(image: FragImage, images?: ImageResolver): string {
  const dim = imgDims(image.wPx, image.hPx); // inline uses DISPLAY px
  const url = images?.(image.relId) ?? null;
  return url ? `<img src="${url}"${dim}>` : `<img alt="image"${dim}>`;
}

// ── run ────────────────────────────────────────────────────────────────────
function underlineCssStyle(u: string): string | undefined {
  if (u === "double") return "double";
  if (u.startsWith("dot")) return "dotted";
  if (u.startsWith("dash")) return "dashed";
  if (u.startsWith("wav") || u === "wave") return "wavy";
  return undefined; // thick/thin/etc → plain underline line, no CSS style
}

export function runToHtml(r: FragRun, images?: ImageResolver): string {
  if (r.image) return imgToHtml(r.image, images); // inline-image data-URL
  const content = r.footnoteMark != null ? `[${r.footnoteMark}]` : r.text;
  if (content === "") return "";
  let h = escapeText(content);

  const style: string[] = [];
  const ff = sanitizeFontFamily(r.fontFamily);
  if (ff) style.push(`font-family:${ff}`);
  const size = clampNumber(r.fontSizePt, FONT_PT_MIN, FONT_PT_MAX);
  if (size != null) style.push(`font-size:${size}pt`);
  const col = normalizeColor(r.color);
  if (col && col !== "#000000") style.push(`color:${col}`); // suppress default black
  const bg = r.highlight != null ? normalizeHighlight(r.highlight) : normalizeColor(r.runShading);
  if (bg) style.push(`background-color:${bg}`); // highlight WINS over shading

  const decoLines: string[] = [];
  let decoStyle: string | undefined;
  if (r.underline !== "none" && r.underline !== "single") {
    decoLines.push("underline");
    decoStyle = underlineCssStyle(r.underline);
  }
  if (r.doubleStrike) { decoLines.push("line-through"); decoStyle = "double"; }
  if (decoLines.length) {
    style.push(`text-decoration-line:${decoLines.join(" ")}`);
    if (decoStyle) style.push(`text-decoration-style:${decoStyle}`);
  }

  if (r.vertAlign === "superscript") h = `<sup>${h}</sup>`;
  else if (r.vertAlign === "subscript") h = `<sub>${h}</sub>`;
  if (r.strike) h = `<s>${h}</s>`;
  if (r.underline === "single") h = `<u>${h}</u>`;
  if (r.italic) h = `<i>${h}</i>`;
  if (r.bold) h = `<b>${h}</b>`;
  if (style.length) h = `<span style="${style.join(";")}">${h}</span>`; // pre-validated → not escapeAttr'd
  const href = hrefFromLink(r.link);
  if (href) h = `<a href="${escapeAttr(href)}">${h}</a>`;
  return h;
}

function runsToHtml(runs: FragRun[], images?: ImageResolver): string {
  return runs.map((r) => runToHtml(r, images)).join("") || "&nbsp;"; // empty paragraph → nbsp
}

// ── paragraph ──────────────────────────────────────────────────────────────
function cssAlign(a: string): string | null {
  switch (a) {
    case "left": case "right": case "center": return a;
    case "both": return "justify";
    default: return null; // start/end/distribute → default (omit)
  }
}

export function paraToHtml(p: FragPara, images?: ImageResolver): string {
  const al = cssAlign(p.alignment);
  const style = al ? ` style="text-align:${al}"` : "";
  const dir = p.dir === "rtl" ? ` dir="rtl"` : "";
  return `<p${style}${dir}>${runsToHtml(p.runs, images)}</p>`;
}

// ── lists ──────────────────────────────────────────────────────────────────
function olType(fmt: string): string | null {
  switch (fmt) {
    case "decimal": return "1";
    case "lowerLetter": return "a";
    case "upperLetter": return "A";
    case "lowerRoman": return "i";
    case "upperRoman": return "I";
    default: return null; // decimalZero/ordinal/… → untyped <ol>
  }
}
function listOpenTag(l: FragList): string {
  if (!l.ordered) return "<ul>";
  const t = olType(l.fmt);
  return t ? `<ol type="${t}">` : "<ol>";
}
function liOpenTag(l: FragList): string {
  return l.ordered ? `<li value="${clampInt(l.value, 0, SPAN_MAX) ?? 0}">` : "<li>";
}

// ── table ──────────────────────────────────────────────────────────────────
const BORDER_EDGE = ["top", "right", "bottom", "left"] as const;
function borderStyleCss(s: string): string {
  if (s === "double") return "double";
  if (s === "dashed" || s === "dash") return "dashed";
  if (s === "dotted") return "dotted";
  return "solid"; // Parasma tracks solid-only
}
function cellStyle(c: FragCell): string {
  const s: string[] = [];
  c.borders.forEach((b, i) => {
    if (!b || i > 3) return;
    const col = normalizeColor(b.color);
    if (!col) return;
    const w = clampNumber(b.widthPx, 0, BORDER_PX_MAX);
    if (w == null) return;
    s.push(`border-${BORDER_EDGE[i]}:${w}px ${borderStyleCss(b.style)} ${col}`);
  });
  const fill = normalizeColor(c.fill);
  if (fill) s.push(`background-color:${fill}`);
  return s.join(";");
}
function cellToHtml(c: FragCell, images?: ImageResolver): string {
  const a: string[] = [];
  const cs = clampInt(c.colspan, 1, SPAN_MAX);
  if (cs != null && cs > 1) a.push(` colspan="${cs}"`);
  const rs = clampInt(c.rowspan, 1, SPAN_MAX);
  if (rs != null && rs > 1) a.push(` rowspan="${rs}"`);
  const style = cellStyle(c);
  if (style) a.push(` style="${style}"`);
  return `<td${a.join("")}>${blocksToHtml(c.blocks, images) || "&nbsp;"}</td>`;
}
function rowToHtml(row: FragRow, images?: ImageResolver): string {
  return `<tr>${row.cells.filter((c) => !c.vMergedAway).map((c) => cellToHtml(c, images)).join("")}</tr>`;
}
export function tableToHtml(t: FragTable, images?: ImageResolver): string {
  return `<table style="border-collapse:collapse">${t.rows.map((r) => rowToHtml(r, images)).join("")}</table>`;
}

// ── the shared block + list-grouping walker ──────────────────────────────────
interface OpenLevel { ordered: boolean; hasOpenLi: boolean }

export function blocksToHtml(blocks: FragBlock[], images?: ImageResolver): string {
  const out: string[] = [];
  const stack: OpenLevel[] = [];
  let activeNumId: number | null = null;
  const closeTo = (target: number) => {
    while (stack.length > target) {
      const top = stack[stack.length - 1];
      if (top?.hasOpenLi) out.push("</li>"); // deferred close (nested list lived inside this li)
      out.push(top?.ordered ? "</ol>" : "</ul>");
      stack.pop();
    }
    if (target === 0) activeNumId = null;
  };
  for (const b of blocks) {
    if (b.kind === "para" && b.para.list) {
      const l = b.para.list;
      // Clamp nesting depth: FragList.level is the one doc-controlled number that
      // otherwise reaches output unclamped — a crafted huge level would open that
      // many <ol>/<ul> tags (memory blowup / budget). Word's max is 9; 32 is
      // generous. Non-finite/negative → 0.
      const lvl = Math.min(Math.max(0, Math.trunc(l.level) || 0), MAX_LIST_DEPTH);
      if (activeNumId !== null && activeNumId !== l.numId) closeTo(0); // new list
      if (stack.length > lvl + 1) closeTo(lvl + 1); // shallower
      while (stack.length < lvl + 1) { // deeper (nest inside open li)
        out.push(listOpenTag(l));
        stack.push({ ordered: l.ordered, hasOpenLi: false });
      }
      const top = stack[stack.length - 1];
      if (top?.hasOpenLi) out.push("</li>"); // sibling: close prev li
      out.push(liOpenTag(l) + runsToHtml(b.para.runs, images)); // leave li OPEN
      if (top) top.hasOpenLi = true;
      activeNumId = l.numId;
    } else {
      closeTo(0); // non-list closes all open lists
      out.push(b.kind === "para" ? paraToHtml(b.para, images) : tableToHtml(b.table, images));
    }
  }
  closeTo(0);
  return out.join("");
}

/** The full text/html flavor: <meta charset> envelope + the serialized fragment body. The
 *  optional `images` resolver (built at COPY TIME) fills inline-image
 *  data-URLs; without it, image runs degrade to `<img alt>` placeholders (the
 *  perf invariant for the text-only pre-serialization cache). */
export function serializeTextHtml(
  frag: DocFragment,
  _budgets: CopyBudgets = DEFAULT_BUDGETS,
  images?: ImageResolver,
): string {
  // _budgets kept for signature stability; budget enforcement lives in
  // buildImageResolver (image.ts), which produces `images`.
  return `<meta charset="utf-8">${blocksToHtml(frag.blocks, images)}`;
}
