/**
 * Sanitizer primitives. The SOLE choke through which
 * doc-controlled strings/numbers reach the text/html flavor. Pure, dependency-
 * free, adversarially tested. html.ts must never interpolate a raw doc string
 * into markup except via one of these (see the call-site table in html.ts).
 */

/** Escape a TEXT node: & < > (ampersand FIRST). */
export function escapeText(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/** Escape a double-quoted ATTRIBUTE value: text set + " '. */
export function escapeAttr(s: string): string {
  return escapeText(s).replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

const HREF_SCHEMES = new Set(["http", "https", "mailto", "tel"]);

/** True if the string carries any C0 control char (0x00–0x1f) — a scheme smuggle vector. */
function hasControlChar(s: string): boolean {
  for (let i = 0; i < s.length; i++) {
    if (s.charCodeAt(i) <= 0x1f) return true;
  }
  return false;
}

/** Scheme-allowlist an href candidate → the URL, or null (⇒ caller emits text-only). */
export function sanitizeHref(raw: string): string | null {
  const url = raw.trim();
  if (url === "") return null;
  // Control chars let "java\tscript:" smuggle a scheme past naive parsers → reject.
  if (hasControlChar(url)) return null;
  const scheme = /^([a-zA-Z][a-zA-Z0-9+.-]*):/.exec(url)?.[1];
  if (scheme == null) return null; // no scheme: relative/anchor unresolvable in a paste target
  return HREF_SCHEMES.has(scheme.toLowerCase()) ? url : null;
}

/** Map a FragRun.link ("url:…" / "anchor:…") → a validated href, or null (text-only). */
export function hrefFromLink(link: string | null): string | null {
  if (!link) return null;
  if (link.startsWith("url:")) return sanitizeHref(link.slice(4));
  return null; // anchor:/other → no external URL in v1 (documented follow-on)
}

/** Normalize a color to #rrggbb (optional #, 3- or 6-hex, case-insensitive), else null. */
export function normalizeColor(raw: string | null | undefined): string | null {
  if (raw == null) return null;
  const s = raw.trim();
  const hex6 = /^#?([0-9a-fA-F]{6})$/.exec(s)?.[1];
  if (hex6) return "#" + hex6.toLowerCase();
  const hex3 = /^#?([0-9a-fA-F]{3})$/.exec(s)?.[1];
  if (hex3) {
    const h = hex3.toLowerCase();
    return `#${h[0]}${h[0]}${h[1]}${h[1]}${h[2]}${h[2]}`;
  }
  return null; // "red", "auto", "red;}body{", "rgb(...)" → dropped
}

// Byte-parity with engine/color.rs highlight_color_to_css (FragRun.highlight is the ENUM NAME).
const HIGHLIGHT: Record<string, string> = {
  yellow: "#ffff00", green: "#00ff00", cyan: "#00ffff", magenta: "#ff00ff",
  blue: "#0000ff", red: "#ff0000", darkblue: "#000080", darkcyan: "#008080",
  darkgreen: "#008000", darkmagenta: "#800080", darkred: "#800000",
  darkyellow: "#808000", darkgray: "#808080", lightgray: "#c0c0c0",
  black: "#000000", white: "#ffffff",
};

/** Map a ST_HighlightColor name → #rrggbb; "none"/unknown → null. */
export function normalizeHighlight(name: string | null): string | null {
  if (name == null) return null;
  return HIGHLIGHT[name.trim().toLowerCase()] ?? null;
}

/** Quote a font-family, STRIPPING quotes/semicolons/colons/angle-brackets/backslash/parens/braces. */
export function sanitizeFontFamily(raw: string | null): string | null {
  if (raw == null) return null;
  const cleaned = raw.replace(/["'\\;:<>(){}]/g, "").replace(/\s+/g, " ").trim();
  return cleaned ? `'${cleaned}'` : null;
}

/** Finite number clamped to [min,max]; non-finite → null (caller omits the decl). */
export function clampNumber(v: number, min: number, max: number): number | null {
  if (!Number.isFinite(v)) return null;
  return Math.min(max, Math.max(min, v));
}

/** Finite integer clamped to [min,max]; non-finite → null. */
export function clampInt(v: number, min: number, max: number): number | null {
  if (!Number.isFinite(v)) return null;
  return Math.min(max, Math.max(min, Math.round(v)));
}

// The raster MIME allowlist for image data-URLs. Only formats
// `createImageBitmap` reliably decodes; svg is EXCLUDED (script vector), as are
// tiff/x-emf/x-wmf (silent null-bitmap skips). A validated mime is the sole
// gate before assembling `data:{mime};base64,…` — a crafted mime cannot smuggle
// a scheme past the paste target.
const IMAGE_MIME = new Set(["image/png", "image/jpeg", "image/gif", "image/bmp", "image/webp"]);

/** Validate/normalize an embedded-image mime → the canonical type, or null. */
export function sanitizeImageMime(raw: string | null | undefined): string | null {
  if (raw == null) return null;
  const s = raw.trim().toLowerCase();
  const m = s === "image/jpg" ? "image/jpeg" : s;
  return IMAGE_MIME.has(m) ? m : null;
}
