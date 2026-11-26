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
- Documents: cover pages and other content kept in a content control now
  show their pictures and charts, use their own fonts, and keep their section
  breaks, and a watermark inside a content control is found.
- Documents: links take their colour and underline from their text's
  formatting, as Word does, instead of always drawing blue and underlined, so
  table of contents entries are no longer blue. Text styled as a hyperlink in
  a document that does not define that style uses Word's built-in look.
- Documents: pictures and shapes marked hidden are no longer drawn.
- Documents: text in a shape takes the font colour of the shape's style, such
  as white text on a coloured shape.
- Documents: tracked deletions keep their line breaks.
- Documents: SmartArt diagrams take their space on the page, so the text
  after one no longer moves up over it. The diagram itself is not drawn yet;
  a placeholder marks where it is.
- Documents: text keeps its gap from floating pictures and text boxes
  (usually an eighth of an inch at each side, as Word writes it on the
  object), including tight and through wrapping. It used to run up against
  them, so line breaks and page breaks around floating objects now match
  Word more closely. Room for shadows and glows is not added yet.
- Documents: DATE, TIME, CREATEDATE and SAVEDATE fields show the viewer's
  local time instead of UTC, so header times are right and dates no longer
  change a day early or late near midnight.
- Documents: columns are balanced only where Word balances them, before a
  continuous section break; at the end of a document or before a page break
  they fill one after another, and the "don't balance columns" setting is
  read. Columns of custom widths no longer get a half-inch gap they did not
  ask for.
- Documents: the binding gutter margin is applied, and a negative top or
  bottom margin lets the text run under the header or footer as in Word.
- Documents: hanging indents (references, glossaries, definitions) line up
  as in Word: a hanging indent replaces a style's first-line indent instead
  of adding to it, a tab after the term goes to the hanging indent, and
  tabs on the first line are measured from where that line starts.
  Automatic tab stops in an indented paragraph fall on the same grid as
  Word's ruler, measured from the margin.
- Documents: "Don't add space between paragraphs of the same style" now
  works as in Word: between list items, between paragraphs that use the
  default style, in table cells and in headers, and when only the earlier
  paragraph asks for it. Lists and tables are no longer too tall.
- Documents: pictures in a header or footer table, content control or link
  appear; they were missing, or showed another header's picture when two
  headers both had one. A different first page with no first-page header
  now shows a blank header, as Word does, instead of the normal one, and
  settings turned off explicitly (such as different odd and even pages set
  to "0") stay off.
- Documents: underlined tabs are drawn, so fill-in blanks ("Name: ____")
  reappear. Dotted, dashed, dot-dash, long-dash and wavy underlines, and their
  heavy and double variants, draw in their own style instead of as a plain
  line; an underline's own colour is used; and "words only" underlines skip
  the spaces.
- Documents: shading draws its pattern: "solid" shading in its foreground
  colour (it was missing), percentage shading as the right shade of grey or
  colour, and stripes and crosses as their average tone. Shading, borders
  and page borders given as theme colours use the theme, and documents that
  remap the theme's text and background colours, as dark templates do, are
  drawn with that mapping. Shading set to clear hides shading inherited from
  a style.
- Documents: text with automatic colour is black, as in Word, and turns
  white on a dark background (cell, paragraph or text shading, or a dark
  page colour), so dark table headers and callouts are readable. It was a
  fixed dark grey everywhere.
- Documents: sections set to centre or bottom-align their text vertically,
  as cover and title pages often are, now place it there instead of at the
  top. Text spread over the page ("justified") still starts at the top.
- Documents: the page colour set in Word's Design > Page Color is drawn. Page
  backgrounds made of a gradient, texture or picture show only their solid
  colour, if they have one.
- Documents: the faint grey rectangle around the text area is gone; Word
  does not show one.
- Charts (all formats): horizontal bar charts are drawn horizontally;
  stacked and 100 % stacked bar, column, line and area charts stack; bars
  have the widths and spacing the chart sets; a legend appears only when the
  chart has one, where it says (bottom, top, left, right) and without
  deleted entries; axes the chart hides are hidden, and fixed axis ranges,
  steps, reversed order and logarithmic scales are honoured; line markers
  follow the chart (including none), smoothed lines are curves, and lines
  use their own colour and width; blank cells leave gaps instead of
  shifting later values; categories given as numbers (such as years), or
  not given, are labelled; unnamed series are named "Series1", "Series2" as
  in Office and keep their colours; 3-D bar, line and area charts are drawn
  flat instead of not at all; doughnuts with several series draw every
  ring.
- Charts: charts in Word headers, footers and text boxes appear, floating
  charts in Word are positioned and wrapped like floating pictures instead
  of sitting in the text, and charts inside PowerPoint groups are drawn. A
  Word chart that cannot be drawn shows the picture saved with it, when
  there is one. Pie-of-pie, stock and bubble charts are drawn as a pie,
  lines and a scatter; surface charts are not drawn yet.
- Charts (all formats): pies and doughnuts start their first slice where the
  chart says, pull out exploded slices, and draw the doughnut hole at its
  set size.
- Charts (all formats): value-axis labels use the chart's number format, or
  the data's (percentages show as 50%, currency keeps its symbol and
  thousands separators); display units such as millions divide the labels
  and show their caption; categories that are dates or numbers show in their
  format.
- Charts (all formats): series, points and chart backgrounds given as theme
  colours use the document's theme, and series without a colour cycle the
  theme's accents as Office does, so charts in documents with a non-Office
  theme get their own colours. Individually coloured bars and pie slices
  keep their colour, pies set not to vary colours are one colour, negative
  bars set to invert draw white, and chart and plot area fills and borders
  are drawn (a chart with no fill is transparent, in every format).
- Colours: lighter and darker theme colour variants made with a tint or
  shade are mixed with white or black as the standard defines, so the banded
  rows of PowerPoint's table styles and other tinted fills are pale, not
  saturated. Colour adjustments apply in the order the file lists them, and
  the ones that were ignored now apply: hue changes (which give SmartArt's
  colourful styles their different colours), transparency changes,
  red/green/blue changes, complement, inverse, greyscale and gamma.
- Colours: colours given by preset name, system colour, HSL or linear RGB now
  resolve everywhere, including Word drawings and chart series. Linear RGB
  colours are no longer too dark.
- Presentations: transparent shapes, lines, text and table cells are drawn
  transparent instead of opaque. Pattern fills and slide backgrounds still
  draw opaque, and transparent 3-D shapes are drawn without face shading.
- Fonts: names the font map had silently dropped, such as Consolas, Palatino
  Linotype, Futura and Menlo, now reach their substitute, so monospaced text
  stays monospaced. Georgia uses Gelasio, its metric-compatible equivalent;
  Lato and Poppins load as themselves; Comic Sans uses Comic Neue; Chinese,
  Japanese and Korean serif font names render in a serif face.
- Lists: nested list items indent to their own level even when the paragraph
  style or the document defaults set an indent, and a hanging indent replaces
  a first-line indent instead of adding to it. Text after a number set with a
  tab moves to the next tab stop when the number is wider than the hang, as in
  Word; numbers followed by a space or by nothing are no longer padded out to
  the hang.
- Lists: legal numbering writes every level in digits (1.1, not I.1), and a
  level that refers to a deeper level no longer shows it. A bullet level with
  no bullet text draws no bullet.
- Lists and fields: numbers can be written as 01, 1st, One, First, circled
  and full-width digits, Chicago-style symbols, Chinese, Japanese and Korean
  numerals and syllables, Hebrew, Arabic, Russian, Thai and Hindi letters and
  digits. Letters continue past z as aa, bb, as in Word; this also changes
  the ALPHABETIC field format, which counted on like spreadsheet columns.
- Lists: numbering continues across separate lists built on the same list
  definition, as Word does after copy and paste or merged documents, and
  "restart numbering" restarts the shared count once instead of starting a
  separate list.
- Lists: a paragraph can take its list from its style and its level from
  itself, or the other way round; a list number switched off on the paragraph
  stays off; and heading styles that name a list but no level are numbered,
  or not, as in Word. Tab stops a list level defines now position the text
  after its number.
- Lists: spacing, alignment and right indent set on a list level apply to its
  paragraphs, and numbers aligned right or centred (common for roman
  numerals) sit where Word puts them. Word's custom 001, 002, 003 format is
  shown instead of 1, 2, 3.
- Page numbers: sections restart and format their own page numbers, so
  front matter shows i, ii, iii and a chapter can restart at 1. Page number
  fields honour their own formats (roman, letters, "- 1 -" and the others
  Word offers, also when MERGEFORMAT follows), and section number and
  section page count fields show live values instead of the saved ones.
- Page numbers: a "Page X of Y" footer stays inside the margin once the
  numbers gain digits, because headers and footers are laid out with each
  page's own numbers. Headers and footers span the page's full text width in
  multi-column sections.
- Sections that must start on an odd or even page get one blank page before
  them when needed, judged by the page number shown, and odd and even
  headers follow the page number shown rather than the page's position in
  its section.

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
