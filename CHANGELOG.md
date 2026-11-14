# Changelog

Notable changes to the published packages. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Until 1.0, a minor version may
change the API; [docs/stability.md](docs/stability.md) lists the surfaces that
are held stable regardless.

## [Unreleased]

## [0.1.1] — 2026-11-13

No change in rendering.

### Changed

- Dependencies: `@xstate/store` 4 and `xstate` 5.33 (`@parasma/headless`), the
  fontsource faces 5.3 (`@parasma/snippet`), and zip 9.4 inside the
  WebAssembly engine, which grows by 3.4 KB gzipped.
- The declaration files are built with TypeScript 7. They export the same
  names and type-check in projects on TypeScript 5.9 or 7.
- Source comments and test descriptions were rewritten so they stand on their
  own.

## [0.1.0] — 2026-11-13

The first public release.

### Formats

- **Word (`.docx`)**: paginated layout following ECMA-376, including sections
  and columns, tables, floating objects with text wrap, headers and footers,
  footnotes, numbering, fields, page borders, watermarks, Office Math, charts,
  EMF pictures, and East Asian and right-to-left text.
- **Excel (`.xlsx`) and CSV**: a continuous, windowed grid with sheet tabs,
  frozen panes, hidden rows and columns, and charts, plus a paginated view.
- **PowerPoint (`.pptx`)**: slides with master and layout inheritance, theme
  colours, shapes, tables, pictures and charts.

### Viewer

- Canvas rendering with text and object selection, copy as plain text and
  HTML, full-text search, zoom, and printing.
- Off-main-thread rendering in a worker, the default for `mountViewer` and
  `createHeadlessViewer`. A worker that stalls is ended and replaced.
- `@parasma/snippet`: `mountViewer` and the `<parasma-viewer>` custom element.
- `@parasma/react` (React 19) and `@parasma/vue` (Vue 3): `useParasma` and
  `<ParasmaViewer>`, both bound to `createViewerSession`, a framework-neutral
  state layer in `@parasma/snippet/headless`.
- All packages share one version and are released together.

### Safeguards

- Packages whose parts expand past the decompression limits are refused
  before parsing (see the README).
- A memory budget stops layout of very large documents and shows the pages
  laid out so far.
