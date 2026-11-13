/**
 * text/plain serialization. ONE shared block walker
 * (`blockToText`) fed by the para/table emitters so flavors cannot structurally
 * diverge (the engine's shared-walker discipline, applied in TS). LOGICAL order by construction
 * (the DocFragment is already logical — the engine never hands us visual order).
 *
 * Rules: WML paragraphs join with `\n`; list markers prefix the line (e.g.
 * "1. " / "vii. " / "• "); an empty §17.3.1.29 paragraph is a blank line;
 * footnote refs emit `[N]`; inline images are omitted (no plain-text form);
 * smallCaps text is emitted verbatim UPPERCASE (already transformed). Runs
 * concatenate with NO separator; a link run emits its text only.
 */
import type { DocFragment, FragBlock, FragPara } from "@parasma/core";
import { serializeTable, WML_TABLE } from "./table";

/** One paragraph → plain text: optional marker prefix + concatenated run text. */
export function paraToText(p: FragPara): string {
  const prefix = p.list ? `${p.list.marker} ` : "";
  const body = p.runs
    .map((r) => (r.footnoteMark ? `[${r.footnoteMark}]` : r.text))
    .join("");
  return prefix + body;
}

/** One block → plain text (paragraph or nested table). The shared walker. */
export function blockToText(b: FragBlock): string {
  if (b.kind === "para") return paraToText(b.para);
  return serializeTable(b.table, WML_TABLE);
}

/** A whole fragment → text/plain: blocks joined by `\n`. */
export function serializeTextPlain(frag: DocFragment): string {
  return frag.blocks.map(blockToText).join("\n");
}
