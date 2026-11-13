/**
 * CSV → minimal-xlsx converter (browser-side).
 *
 * CSV is not OOXML — Excel "opens" it by importing the text into its normal
 * grid. We do the equivalent one step earlier: build a minimal SpreadsheetML
 * package in memory and feed it to the existing `loadXlsx` path, so the Rust
 * engine needs zero CSV knowledge and every downstream feature (grid geometry,
 * pagination, alignment, overflow semantics, the formula bar) comes free.
 *
 * Scope (v1, documented):
 *  - RFC 4180 parsing: quoted fields, `""` escapes, newlines inside quotes,
 *    CRLF/LF, UTF-8 BOM strip.
 *  - Delimiter sniffing: `,` `;` or tab — whichever occurs most, unquoted, on
 *    the first data line (Excel handles regional variants similarly).
 *  - Type inference, deliberately conservative: numeric strings → numbers
 *    (right-aligned), TRUE/FALSE → booleans. NO date coercion — strings that
 *    look like dates stay text (avoiding Excel's infamous auto-conversion).
 *  - Column widths: content-fit in §18.3.1.13 character units, clamped to
 *    [8.43, 60] — what Excel's import preview effectively shows.
 */

import { zipSync, strToU8 } from "fflate";

/** Not a zip, decodes as text, and the first line contains a plausible
 * delimiter → treat as CSV. (docx/xlsx are always zips, so this can never
 * misroute them.) */
export function sniffIsCsv(bytes: Uint8Array): boolean {
  if (bytes.length === 0) return false;
  if (bytes[0] === 0x50 && bytes[1] === 0x4b) return false; // PK zip
  const head = bytes.subarray(0, Math.min(bytes.length, 4096));
  // Binary guard: NUL bytes mean "not text".
  for (const b of head) if (b === 0) return false;
  let text: string;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(head);
  } catch {
    return false;
  }
  const firstLine = text.split(/\r?\n/, 1)[0] ?? "";
  return /[,;\t]/.test(firstLine);
}

function sniffDelimiter(firstLine: string): string {
  let best = ",";
  let bestCount = -1;
  for (const d of [",", ";", "\t"]) {
    let count = 0;
    let inQuotes = false;
    for (const ch of firstLine) {
      if (ch === '"') inQuotes = !inQuotes;
      else if (ch === d && !inQuotes) count++;
    }
    if (count > bestCount) {
      best = d;
      bestCount = count;
    }
  }
  return best;
}

/** RFC 4180 parse into rows of fields. */
export function parseCsv(text: string): string[][] {
  if (text.charCodeAt(0) === 0xfeff) text = text.slice(1); // BOM
  const delim = sniffDelimiter(text.split(/\r?\n/, 1)[0] ?? "");
  const rows: string[][] = [];
  let row: string[] = [];
  let field = "";
  let inQuotes = false;
  for (let i = 0; i < text.length; i++) {
    const ch = text[i];
    if (inQuotes) {
      if (ch === '"') {
        if (text[i + 1] === '"') {
          field += '"';
          i++;
        } else {
          inQuotes = false;
        }
      } else {
        field += ch;
      }
    } else if (ch === '"') {
      inQuotes = true;
    } else if (ch === delim) {
      row.push(field);
      field = "";
    } else if (ch === "\n" || ch === "\r") {
      if (ch === "\r" && text[i + 1] === "\n") i++;
      row.push(field);
      field = "";
      rows.push(row);
      row = [];
    } else {
      field += ch;
    }
  }
  if (field.length > 0 || row.length > 0) {
    row.push(field);
    rows.push(row);
  }
  // Drop a single trailing fully-empty row (file ending in newline).
  if (rows.length && (rows[rows.length - 1] ?? []).every((f) => f === "")) rows.pop();
  return rows;
}

const NUMERIC = /^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/;

function xmlEscape(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function colLetter(n: number): string {
  // 1-based column number → A1 letters.
  let s = "";
  while (n > 0) {
    const r = (n - 1) % 26;
    s = String.fromCharCode(65 + r) + s;
    n = Math.floor((n - 1) / 26);
  }
  return s;
}

const SML_NS = `xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"`;

/** Convert CSV text into a minimal one-sheet .xlsx byte vector. */
export function csvToXlsx(text: string): Uint8Array {
  const rows = parseCsv(text);
  const nCols = rows.reduce((m, r) => Math.max(m, r.length), 0) || 1;
  const nRows = rows.length || 1;

  // Content-fit column widths in §18.3.1.13 character units.
  const widths: number[] = new Array(nCols).fill(0);
  for (const r of rows) {
    r.forEach((f, c) => {
      widths[c] = Math.max(widths[c] ?? 0, f.length);
    });
  }
  const cols = widths
    .map((w, i) => {
      const chars = Math.min(Math.max(w + 2, 8.43), 60);
      return `<col min="${i + 1}" max="${i + 1}" width="${chars.toFixed(2)}" customWidth="1"/>`;
    })
    .join("");

  const body = rows
    .map((r, ri) => {
      const cells = r
        .map((f, ci) => {
          if (f === "") return "";
          const ref = `${colLetter(ci + 1)}${ri + 1}`;
          const t = f.trim();
          if (NUMERIC.test(t)) return `<c r="${ref}"><v>${t}</v></c>`;
          if (/^(TRUE|FALSE)$/i.test(t))
            return `<c r="${ref}" t="b"><v>${/^TRUE$/i.test(t) ? 1 : 0}</v></c>`;
          return `<c r="${ref}" t="inlineStr"><is><t xml:space="preserve">${xmlEscape(f)}</t></is></c>`;
        })
        .join("");
      return `<row r="${ri + 1}">${cells}</row>`;
    })
    .join("\n");

  const sheet = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet ${SML_NS}>
  <dimension ref="A1:${colLetter(nCols)}${nRows}"/>
  <sheetFormatPr defaultRowHeight="15" baseColWidth="8"/>
  <cols>${cols}</cols>
  <sheetData>
${body}
  </sheetData>
</worksheet>`;

  const contentTypes = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>`;
  const rootRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>`;
  const workbook = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook ${SML_NS} xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="CSV" sheetId="1" r:id="rId1"/></sheets>
</workbook>`;
  const wbRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>`;
  const styles = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet ${SML_NS}>
  <fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>
  <fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills>
  <borders count="1"><border/></borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  <cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs>
</styleSheet>`;

  return zipSync({
    "[Content_Types].xml": strToU8(contentTypes),
    "_rels/.rels": strToU8(rootRels),
    "xl/workbook.xml": strToU8(workbook),
    "xl/_rels/workbook.xml.rels": strToU8(wbRels),
    "xl/styles.xml": strToU8(styles),
    "xl/worksheets/sheet1.xml": strToU8(sheet),
  });
}
