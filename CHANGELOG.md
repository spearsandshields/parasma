# Changelog

Notable changes to the published packages. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Until 1.0, a minor version may
change the API; [docs/stability.md](docs/stability.md) lists the surfaces that
are held stable regardless.

## [Unreleased]

### Fixed

- Documents: paragraphs with no named style now take the document's default
  paragraph style (usually Normal), so body text gets the template's font,
  size and spacing. On/off paragraph settings in the document defaults, such
  as widow control, now apply too.
- Documents: text is kerned only where the document asks for it, as Word does,
  and is drawn exactly as it was measured. A kerning value of 0 switches
  kerning off.
- Documents: Word's ligature setting is read, so documents from current Word,
  which turn standard ligatures on, show them ("fi", "fl" joined), and Safari
  now draws their kerning as measured. Without the setting, ligatures stay
  off.
- Documents: text in the Symbol, Wingdings, Wingdings 2, Wingdings 3 and
  Webdings fonts draws the character the font means: check boxes, arrows and
  dingbat bullets instead of letters. Symbol characters inserted with Word's
  symbol command, and non-breaking hyphens, are no longer dropped.
- Documents: text boxes from Word 2010 and later now appear with their text.
  Word writes each one inside a compatibility wrapper that was being skipped,
  so their text was missing from the page. Text boxes that keep their
  paragraphs in a content control, as Word's built-in quote and sidebar boxes
  do, show their text too. Text boxes inside shape groups and drawing canvases
  are not shown yet.
- Documents: text boxes use their own inner margins and vertical alignment
  (top, centre or bottom), and their fill and outline colours, including theme
  colours with Word's lighter and darker variants, transparency, and shapes
  coloured by the theme's shape styles. Text in a shape that is not a
  rectangle shows without a frame until shape outlines are drawn.
- Documents: text inside smart tags, custom XML and direction overrides is no
  longer dropped, and wrappers nested in each other (a hyperlink inside a
  content control, a smart tag inside a tracked insertion) keep their text and
  formatting. Tracked moves draw as insertions and deletions. Table rows and
  cells wrapped in content controls, and tables inside a content control in a
  cell, are laid out. Fonts used only inside such wrappers are now fetched.
- Documents: pictures and text boxes anchored in a table cell and set in
  front of or behind the text are drawn; they were dropped.
- Documents: tracked deletions keep their line breaks.
- Fonts: names the font map had silently dropped, such as Consolas, Palatino
  Linotype, Futura and Menlo, now reach their substitute, so monospaced text
  stays monospaced. Georgia uses Gelasio, its metric-compatible equivalent;
  Lato and Poppins load as themselves; Comic Sans uses Comic Neue; Chinese,
  Japanese and Korean serif font names render in a serif face.

### Changed

- Fonts: documents that use those symbol fonts download Noto Sans Symbols 2
  on demand. The WebAssembly engine grows by about 20 KB gzipped in all.

## [0.1.2] — 2026-11-19

### Fixed

- Spreadsheets: while scrolling, the grid and its column headers no longer
  slide out of view and snap back. The grid canvas is now pinned to the scroll
  corner by the browser (`position: sticky`) instead of by a transform written
  on each scroll event, which always trailed the scroll.

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
