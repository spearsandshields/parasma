# Theming — the `--ps-*` contract

Chrome styling only. In-canvas visuals (document pixels, search-highlight and
selection colors) are engine-side and deliberately not CSS-reachable.

## 1. Design tokens (`--ps-*` custom properties)

Set at any scope above the viewer (`:root`, app shell, shadow host, or the
`.parasma-snippet` wrapper — custom properties inherit across shadow
boundaries). Dark variants: scope overrides under `[data-theme="dark"]`.
Consumer rules win ties against the shipped (adopted) sheet: its token
defaults and root layout rule are `:where()`-wrapped (zero specificity) —
this also covers sizing the root element (`height`, `max-height`, …).

| Token | Role |
|---|---|
| `--ps-text` / `--ps-text-muted` / `--ps-text-faint` | Foreground hierarchy |
| `--ps-accent` / `--ps-accent-soft` / `--ps-accent-press` | Accent family (buttons, active states, focus) |
| `--ps-panel` / `--ps-paper` / `--ps-canvas` / `--ps-stage` | Surfaces: chrome panels, page paper, page surround, stage backdrop |
| `--ps-hairline` / `--ps-hairline-strong` | Borders/dividers |
| `--ps-shadow` / `--ps-ring` / `--ps-inset` | Elevation, focus ring, inset accents |
| `--ps-icon` | Icon stroke |
| `--ps-err` / `--ps-ok` | Status colors |
| `--ps-font-ui` / `--ps-font-mono` | Chrome typography |
| `--ps-s1` … `--ps-s5` | Spacing scale |
| `--ps-toolbar-h` / `--ps-explorer-w` | Layout knobs |
| `--ps-fmt-docx` / `--ps-fmt-xlsx` / `--ps-fmt-pptx` / `--ps-fmt-csv` | Format badge colors |

**Internal, not contract**: `--ps-d` (reveal-stagger delay, set inline) and
`--ps-cc` (per-comment color carrier) — may change without notice.

## 2. Stable class names

Chrome renders in light DOM; every element carries a `.ps-*` class
(`.ps-viewer`, `.ps-btn`, `.ps-context-menu`, `.ps-context-menu-item` …).
Ordinary selectors reach everything. Names are contract: renames are breaking
changes.

## 3. Stylesheet ownership

`styles: "auto"` (default): adopted at create time into the mounting root
(`document` or `ShadowRoot`) via constructable stylesheets. `styles:
"manual"`: nothing delivered; import `@parasma/snippet/snippet.css` and own
delivery.

## 4. Theme mode

`theme: "light" | "dark" | "auto"`. `"auto"` live-mirrors the host's
`data-theme` (nearest shadow host, else `documentElement`), with a
`prefers-color-scheme` fallback, until destroy.

## Migration (2026-07-30)

One-time mechanical rename: all 33 custom properties gained the `ps-` prefix
(`--accent` → `--ps-accent`; suffixes unchanged). Class names did not change.
