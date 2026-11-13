/**
 * formats — exported format knowledge.
 *
 * What this library can open, as data — so file pickers, drag-drop validators
 * and multi-engine dispatch shells stop hardcoding `.docx/.xlsx/...` lists
 * that go stale the day a format lands. `sniffKind` is the same bytes-level
 * classification `Viewer.load()` routes on, exported; `UnsupportedFormatError`
 * is the typed rejection a wrapper can throw/branch on, so "not previewable"
 * and "previewable but broken" stop being the same catch-all.
 */
import { sniffIsCsv } from "./csv";
import type { DocumentKind } from "./index";

// ── the one spelling of the OPC sniff rule ───────────────────────────────────
// Every OOXML format is an OPC ZIP whose MAIN PART name is stored verbatim
// (uncompressed) in the ZIP's local-file + central-directory headers, so a
// byte-substring scan classifies without inflating any entry. Moved here from
// index.ts (which now imports these) so the rule and its needles live once.

function hasNeedle(bytes: Uint8Array, ascii: string): boolean {
  if (bytes.length < 4 || bytes[0] !== 0x50 || bytes[1] !== 0x4b) return false; // "PK"
  const needle = Array.from(ascii, (c) => c.charCodeAt(0));
  const last = bytes.length - needle.length;
  for (let i = 0; i <= last; i++) {
    let j = 0;
    while (j < needle.length && bytes[i + j] === needle[j]) j++;
    if (j === needle.length) return true;
  }
  return false;
}

/** ZIP magic ("PK") — the OPC container precondition. */
export function sniffIsZip(bytes: Uint8Array): boolean {
  return bytes.length >= 4 && bytes[0] === 0x50 && bytes[1] === 0x4b;
}
/** SpreadsheetML main part present. */
export function sniffIsXlsxBytes(bytes: Uint8Array): boolean {
  return hasNeedle(bytes, "xl/workbook.xml");
}
/** PresentationML main part present. */
export function sniffIsPptxBytes(bytes: Uint8Array): boolean {
  return hasNeedle(bytes, "ppt/presentation.xml");
}
/** WordprocessingML main part present. */
export function sniffHasWordPart(bytes: Uint8Array): boolean {
  return hasNeedle(bytes, "word/document.xml");
}

/** One openable format's metadata. */
export interface SupportedFormat {
  kind: DocumentKind;
  /** Lowercase extensions, dot-less. */
  extensions: readonly string[];
  /** MIME types commonly attached to the format. */
  mimeTypes: readonly string[];
}

/** Everything `Viewer.load()` accepts, as data. */
export const supportedFormats: readonly SupportedFormat[] = [
  {
    kind: "word",
    extensions: ["docx"],
    mimeTypes: ["application/vnd.openxmlformats-officedocument.wordprocessingml.document"],
  },
  {
    kind: "excel",
    extensions: ["xlsx", "csv"],
    mimeTypes: [
      "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
      "text/csv",
    ],
  },
  {
    kind: "ppt",
    extensions: ["pptx"],
    mimeTypes: ["application/vnd.openxmlformats-officedocument.presentationml.presentation"],
  },
] as const;

/**
 * Classify bytes the way `load()` routes them. `null` = no recognizable
 * signature (not a ZIP with a known main part, not CSV-like) — the caller can
 * reject early with {@link UnsupportedFormatError} instead of feeding the
 * engine bytes it will reject with a parse error.
 *
 * Note the deliberate asymmetry with `load()`: load() falls through unknown
 * ZIPs to the docx parser (which surfaces the real error); sniffKind reports
 * `word` only when the WordprocessingML main part is actually present, so a
 * ZIP that is none of the three reads as `null` here.
 */
export function sniffKind(bytes: Uint8Array): DocumentKind | null {
  if (sniffIsXlsxBytes(bytes)) return "excel";
  if (sniffIsPptxBytes(bytes)) return "ppt";
  if (sniffIsZip(bytes)) return sniffHasWordPart(bytes) ? "word" : null;
  if (sniffIsCsv(bytes)) return "excel";
  return null;
}

/** Typed "exists but not previewable" — routed to an `unsupported` UI state,
 *  distinct from a load/parse `error`. */
export class UnsupportedFormatError extends Error {
  constructor(public readonly fileName?: string) {
    super(fileName ? `${fileName} is not a previewable format` : "not a previewable format");
    this.name = "UnsupportedFormatError";
  }
}
