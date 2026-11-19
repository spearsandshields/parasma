# Parasma

[![npm](https://img.shields.io/npm/v/@parasma/snippet.svg)](https://www.npmjs.com/package/@parasma/snippet)
[![CI](https://github.com/spearsandshields/parasma/actions/workflows/ci.yml/badge.svg)](https://github.com/spearsandshields/parasma/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Show Word, Excel and PowerPoint files in your web application, laid out page for page,
without a server and without the document ever leaving the browser.**

Parasma is a read-only viewer for `.docx`, `.xlsx`, `.pptx` and CSV. Its core is a document
layout engine written in Rust and compiled to WebAssembly: it reads the file, breaks lines
and pages by the rules of ECMA-376 as Word applies them, and paints the result to a canvas.
Around the engine sits a framework-agnostic TypeScript viewer that you embed with one element.

The goal is to be for Office documents what PDF.js is for PDF: the component you reach for when
a document has to appear inside a web page, as its author saw it.

**[Try the demo](https://spearsandshields.github.io/parasma/)**. It opens your own files too,
and they stay in your browser.

```html
<parasma-viewer src="/files/report.docx" controls style="height: 80vh"></parasma-viewer>
```

## Why Parasma

**No conversion service.** The usual ways to show an Office file on the web are to upload it to
a third party, or to run a headless office suite that turns it into PDF or images. Parasma needs
neither: the file is parsed, laid out and painted on the user's device. There is nothing to
deploy, nothing to scale, and no copy of a confidential document on someone else's machine.

**Layout, not reflow.** Turning a document into HTML lets the browser decide where lines and
pages break, so page 7 in the viewer is not page 7 in Word. Parasma computes the layout itself:
sections and columns, tables, floating objects with text wrap, footnotes,
numbering, fields. Where the standard leaves a behaviour open, the rule was measured on real
documents in Word and is recorded beside the code that implements it. Where the engine knowingly
does less than the standard, the deviation is declared, not hidden.

**Canvas-native.** A page keeps the geometry of every glyph, so painting, zooming, selecting
text, copying as plain text or HTML and searching work on positions the engine computed, not on
whatever a DOM text layer happened to produce.

**Off the main thread.** Parsing, layout and paint run in a worker by default, so a long report
does not freeze the page that embeds it. A worker that stalls is ended and replaced.

**Documents are untrusted input.** The engine runs in the WebAssembly sandbox: a memory-safety
failure traps and cannot touch the host page. A package is refused before parsing when its parts
inflate past fixed limits (counted on the bytes actually inflated, not on what the zip directory
claims). Word layout watches a memory budget and stops gracefully with the pages laid out so far
instead of taking the tab down. [SECURITY.md](SECURITY.md) has the threat model, and
[docs/memory-architecture.md](docs/memory-architecture.md) the measurements behind the limits.

**Made to be embedded.** One custom element, or `mountViewer`, or a headless viewer under your
own toolbar; adapters for React and Vue; theming through CSS tokens. The names you build on are listed in a [stability contract](docs/stability.md) and pinned
by tests.

**No telemetry.** The library makes no analytics requests. The only network traffic is the
engine and the fonts, both of which you can host yourself.

## Features

- Paginated Word layout: sections and columns, tables, floating objects with
  text wrap, headers and footers, footnotes, numbering, fields, page borders,
  watermarks, Office Math, charts, EMF pictures, East Asian and right-to-left
  text.
- Spreadsheets as a continuous, windowed grid with sheet tabs, frozen panes,
  hidden rows and columns, and charts, plus a paginated view.
- Slides with master and layout inheritance, theme colours, shapes, tables,
  pictures and charts.
- Text and object selection, copy as plain text and HTML, full-text search,
  zoom, printing, light and dark themes.
- Rendering off the main thread in a worker by default.
- A default toolbar you can keep, restyle through `--ps-*` tokens, or replace
  with your own.

## Install

```bash
npm install @parasma/snippet
```

| Package | Use it for |
|---|---|
| `@parasma/snippet` | The viewer: `<parasma-viewer>`, `mountViewer`, and `createHeadlessViewer` for your own UI. Most applications need only this. |
| `@parasma/react` | React 19: `useParasma` and `<ParasmaViewer>`. |
| `@parasma/vue` | Vue 3: `useParasma` and `<ParasmaViewer>`. |
| `@parasma/headless`, `@parasma/core` | The plugin kernel and the engine handle, installed with the above. |

The engine is about 1.8 MB of WebAssembly (gzipped) and is fetched when the
first viewer starts. Fonts are fetched on demand; see [Privacy](#privacy).

**Vite:** add `optimizeDeps: { exclude: ["@parasma/core"] }` to your Vite
config. The dev server pre-bundles dependencies into a cache folder where the
engine can no longer find its WebAssembly file. Production builds need no
configuration.

## Quickstart

With a bundler (Vite, webpack, Rollup, esbuild):

```ts
import "@parasma/snippet"; // registers <parasma-viewer>
```

```html
<parasma-viewer src="/files/report.docx" controls style="height: 80vh"></parasma-viewer>
```

Or from code, with bytes you already have:

```ts
import { mountViewer } from "@parasma/snippet";

const viewer = await mountViewer("#viewer", { controls: true });
const bytes = new Uint8Array(await (await fetch("/files/report.xlsx")).arrayBuffer());
await viewer.load(bytes, "report.xlsx");
```

The viewer fills its container, so give the container a height. For a viewer
with no built-in chrome, use `createHeadlessViewer` from
`@parasma/snippet/headless` and drive it from your own UI.

## Frameworks

`<parasma-viewer>` works from any framework that renders custom elements;
[packages/snippet](packages/snippet/README.md) documents its properties,
attributes and `parasma-*` events. The React and Vue adapters add a hook or
composable that returns the viewer state (status, page, zoom, sheets, search
progress, …) and typed commands, plus a `<ParasmaViewer>` component:

```tsx
import { ParasmaViewer } from "@parasma/react";

<ParasmaViewer source={file} onPageChange={setPage} style={{ height: "80vh" }} />
```

```vue
<ParasmaViewer :source="file" v-model:page="page" style="height: 80vh" />
```

See [packages/react](packages/react/README.md) and
[packages/vue](packages/vue/README.md). Both adapters are built on one
framework-neutral state layer, so they behave the same way.

## Browser support

Parasma targets current versions of Chrome, Edge, Firefox and Safari. It needs
WebAssembly; if the render worker cannot start, the viewer falls back to
rendering on the main thread.

## Status and limitations

- **Version 0.1.** Until 1.0 a minor release may change the API;
  [docs/stability.md](docs/stability.md) lists what is held stable
  regardless. All packages share one version number.
- **Read-only.** There is no editing and no saving.
- **No accessibility layer yet.** Rendering is canvas-only with no DOM text
  layer, so screen readers cannot read a document.
- **Fidelity depends on fonts.** Word lays out with the fonts installed on the
  author's machine. Parasma substitutes metric-compatible open fonts when the
  named font is unavailable, and a substitute a few percent narrower or wider
  can move line and page breaks.
- **The test suite checks the engine against itself.** Fixtures are synthetic;
  agreement with Word is established by calibration on real documents, not by
  an automated comparison.
- **Legacy binary formats** (`.doc`, `.xls`, `.ppt`) are not supported.
- **Untrusted documents.** Rendered in-process, a hostile file can hang or
  crash the tab. Worker mode (the default for `mountViewer` and `createHeadlessViewer`) confines that to the
  worker and ends a worker that stalls. See [SECURITY.md](SECURITY.md) for the
  threat model and how to report a problem.
- **Decompression limits.** A package is refused before parsing if any part
  larger than 4 MiB expands to more than 100 times its compressed size, or if
  its parts expand to more than 1 GiB in total. No real document in our test
  corpus comes near either limit; deliberately repetitive generated files can.

## Privacy

Parasma has no telemetry and makes no analytics requests. Documents are parsed
and rendered in the browser and are never uploaded by the library.

By default, fonts are fetched on demand from the jsDelivr CDN, which means
jsDelivr sees the requesting IP address and which font files were requested.
To avoid any third-party request, host the font files yourself and set
`fontBaseUrl` (or supply a `fontResolver`); `assetBaseUrl` relocates the
wasm binary and fonts together.

## How it works

A document goes through four stages, all inside the browser:

1. **Open.** The package is unzipped with its inflated size metered, and parsed by a Rust OOXML
   parser into typed structures for WordprocessingML, SpreadsheetML, PresentationML and
   DrawingML.
2. **Lay out.** The engine resolves styles, numbering and themes, measures text with the fonts
   available (the named font, or a metric-compatible open substitute), and breaks lines, columns
   and pages. A Word document becomes a list of pages; a workbook a windowed grid that
   materialises only what is scrolled into view; a deck a list of slides.
3. **Paint.** Each page is a display list that the worker replays onto a canvas.
4. **Interact.** Selection, copy, search, zoom and printing are plugins of a headless kernel
   that has no DOM and no UI framework; the default toolbar is one more client of it.

The engine is about 77,000 lines of Rust with some 800 native tests, built as one
WebAssembly module with a feature per format.

The codebase is layered, with dependencies pointing strictly downward:

```
crates/parasma-wasm   Rust layout + paint engine  →  WebAssembly
        │
        ▼
@parasma/core         Viewer wrapper over the wasm handle; font resolution;
        │             optional off-main-thread worker transport
        ▼
@parasma/headless     Framework-agnostic kernel + plugins (loader, viewport,
        │             selection, clipboard, search, zoom, fonts, grid, …).
        │             No DOM, no UI framework (enforced by the gate).
        ▼
@parasma/snippet      The reference integration: DOM adapters + default chrome +
        │             the `mountViewer` entry point + `<parasma-viewer>`.
        ▼
@parasma/vue          Framework adapters: `useParasma` + `<ParasmaViewer>`,
@parasma/react        bound to the snippet's framework-neutral viewer session.
```

## Building from source

The OOXML parser is Parasma's public fork of pterror/ooxml,
[`spearsandshields/parasma-ooxml`](https://github.com/spearsandshields/parasma-ooxml),
pinned by tag in the root `Cargo.toml`; no credentials are needed to build.

Prerequisites: [rustup](https://rustup.rs) (the compiler version, the
`wasm32-unknown-unknown` target and clippy are selected by
`rust-toolchain.toml`), [wasm-pack](https://rustwasm.github.io/wasm-pack/)
0.15, [Bun](https://bun.sh) 1.4 and Node 20 or newer.

```bash
bun install
./scripts/fetch-test-fonts.sh       # once: the pinned substitute fonts the tests measure with
bun run scripts/fetch-fixtures.ts   # once: the sample documents the tests and the demo read
bun run build:wasm:full             # the engine at the shipping feature set (sml, pml, emf)
bun run ci                          # the gate: layering rules, clippy, native tests, tsc, TS tests
```

The wasm build comes before the type-checks and tests because the TypeScript
packages are typed against its generated bindings. For the engine alone:

```bash
cargo test -p parasma-wasm --features sml,pml,emf
```

To run the demo at `localhost:5173`:

```bash
bun run dev
```

Its explorer lists the documents under `fixtures/`, the same set
`scripts/fetch-fixtures.ts` produced above: the parser fork's test documents
plus the ones `scripts/generate-test-fixtures.ts` writes.

`.github/workflows/ci.yml` runs the same gate on a clean machine for every
push and pull request, then packs every package and installs the tarballs
into a throwaway project (`./scripts/pack-smoke.sh`).

All packages share one version and are released together with
`./scripts/publish.sh`.

## Trademarks and affiliation

Parasma is an independent project. It is not affiliated with,
sponsored by, or endorsed by Microsoft or by any other company or organisation.

Microsoft, Word, Excel, PowerPoint and Office are trademarks of the Microsoft
group of companies. They are used here only to identify the file formats and
applications that Parasma interoperates with.

## Acknowledgements

The OOXML parser is a fork of [pterror/ooxml](https://github.com/pterror/ooxml).

## License

Licensed under the [Apache License 2.0](LICENSE). Third-party components and
fonts are attributed in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
