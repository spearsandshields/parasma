/**
 * Vendored Lucide icons (https://lucide.dev, ISC-licensed) — the small fixed set
 * the chrome uses, built as inline SVG so they inherit the button colour via
 * `stroke=currentColor` and scale with `font-size`. Vendored (not the `lucide`
 * npm package) to keep @parasma/snippet dependency-free for source consumers and
 * the embed footprint minimal — only these ~15 glyphs ship.
 *
 * Stroke width is LOCKED to 1.75 (Lucide draws at 2 on a 24px grid; 1.75 is the
 * precision-instrument weight, matching the pre-existing select/hand tool icons
 * this module now also owns). Built via createElementNS — no innerHTML — so it
 * stays clean under a strict CSP (no inline-HTML injection). These replace the former Unicode
 * glyphs (‹ › − + ▾ ⤢ ◐ ☰), which fell back to emoji/tofu on Windows/Linux and
 * rendered at inconsistent weights.
 */

const SVG_NS = "http://www.w3.org/2000/svg";

type Seg = [tag: string, attrs: Record<string, string>];

// Each icon = its child elements, copied verbatim from the Lucide source.
const ICONS: Record<string, Seg[]> = {
  "chevron-left": [["path", { d: "m15 18-6-6 6-6" }]],
  "chevron-right": [["path", { d: "m9 18 6-6-6-6" }]],
  "chevron-down": [["path", { d: "m6 9 6 6 6-6" }]],
  minus: [["path", { d: "M5 12h14" }]],
  plus: [
    ["path", { d: "M5 12h14" }],
    ["path", { d: "M12 5v14" }],
  ],
  maximize: [
    ["path", { d: "M8 3H5a2 2 0 0 0-2 2v3" }],
    ["path", { d: "M21 8V5a2 2 0 0 0-2-2h-3" }],
    ["path", { d: "M3 16v3a2 2 0 0 0 2 2h3" }],
    ["path", { d: "M16 21h3a2 2 0 0 0 2-2v-3" }],
  ],
  sun: [
    ["circle", { cx: "12", cy: "12", r: "4" }],
    ["path", { d: "M12 2v2" }],
    ["path", { d: "M12 20v2" }],
    ["path", { d: "m4.93 4.93 1.41 1.41" }],
    ["path", { d: "m17.66 17.66 1.41 1.41" }],
    ["path", { d: "M2 12h2" }],
    ["path", { d: "M20 12h2" }],
    ["path", { d: "m6.34 17.66-1.41 1.41" }],
    ["path", { d: "m19.07 4.93-1.41 1.41" }],
  ],
  moon: [["path", { d: "M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z" }]],
  "panel-left": [
    ["rect", { width: "18", height: "18", x: "3", y: "3", rx: "2" }],
    ["path", { d: "M9 3v18" }],
  ],
  "panel-left-close": [
    ["rect", { width: "18", height: "18", x: "3", y: "3", rx: "2" }],
    ["path", { d: "M9 3v18" }],
    ["path", { d: "m16 15-3-3 3-3" }],
  ],
  search: [
    ["circle", { cx: "11", cy: "11", r: "8" }],
    ["path", { d: "m21 21-4.3-4.3" }],
  ],
  "message-square": [
    ["path", { d: "M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" }],
  ],
  // The tool-toggle pair (select / hand), owned here so the chrome has ONE icon
  // system (attach-dom's select/hand buttons render these via icon()).
  "mouse-pointer-2": [
    [
      "path",
      {
        d: "M4.037 4.688a.495.495 0 0 1 .651-.651l16 6.5a.5.5 0 0 1-.063.947l-6.124 1.58a2 2 0 0 0-1.438 1.435l-1.579 6.126a.5.5 0 0 1-.947.063z",
      },
    ],
  ],
  printer: [
    ["path", { d: "M6 18H4a2 2 0 0 1-2-2v-5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2v5a2 2 0 0 1-2 2h-2" }],
    ["path", { d: "M6 9V3a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v6" }],
    ["rect", { x: "6", y: "14", width: "12", height: "8", rx: "1" }],
  ],
  hand: [
    ["path", { d: "M18 11V6a2 2 0 0 0-2-2 2 2 0 0 0-2 2" }],
    ["path", { d: "M14 10V4a2 2 0 0 0-2-2 2 2 0 0 0-2 2v2" }],
    ["path", { d: "M10 10.5V6a2 2 0 0 0-2-2 2 2 0 0 0-2 2v8" }],
    [
      "path",
      { d: "M18 8a2 2 0 1 1 4 0v6a8 8 0 0 1-8 8h-2c-2.8 0-4.5-.86-5.99-2.34l-3.6-3.6a2 2 0 0 1 2.83-2.82L7 15" },
    ],
  ],
};

export type IconName = keyof typeof ICONS;

/**
 * Build an inline Lucide `<svg>` for `name` at `size` px (default 16). The SVG
 * inherits colour via `stroke=currentColor`; callers style it through the parent
 * button's `color`. Decorative by default (`aria-hidden`) — the button's own
 * `aria-label` carries the accessible name.
 */
export function icon(name: IconName, size = 16): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "1.75");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  for (const [tag, attrs] of ICONS[name]) {
    const child = document.createElementNS(SVG_NS, tag);
    for (const [k, v] of Object.entries(attrs)) child.setAttribute(k, v);
    svg.append(child);
  }
  return svg;
}
