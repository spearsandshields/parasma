# @parasma/snippet

The [Parasma](https://github.com/spearsandshields/parasma) document viewer for the browser: Word (`.docx`), Excel
(`.xlsx`), PowerPoint (`.pptx`) and CSV, rendered to a canvas by a
WebAssembly layout engine. This package is the one most applications
install: a drop-in `<parasma-viewer>` element, `mountViewer` for code, and a
chrome-free viewer for building your own UI.

```bash
npm install @parasma/snippet
```

The package root imports its stylesheet, so it needs a bundler (Vite,
webpack, Rollup, esbuild). The `@parasma/snippet/headless` and
`@parasma/snippet/chrome` subpaths import under plain Node.

**Vite:** add `optimizeDeps: { exclude: ["@parasma/core"] }` to your Vite
config. The dev server pre-bundles dependencies into a cache folder where the
engine can no longer find its WebAssembly file. Production builds need no
configuration.

## Usage

```ts
import "@parasma/snippet"; // registers <parasma-viewer>
```

```html
<parasma-viewer src="/files/report.docx" controls style="height: 80vh"></parasma-viewer>
```

From code:

```ts
import { mountViewer } from "@parasma/snippet";

const viewer = await mountViewer("#viewer", { controls: true });
await viewer.load(bytes, "report.docx"); // a Uint8Array or ArrayBuffer
```

Your own UI, no built-in chrome:

```ts
import { createHeadlessViewer } from "@parasma/snippet/headless";

const viewer = await createHeadlessViewer({ root: document.getElementById("stage")! });
await viewer.load(bytes, "report.pptx");
viewer.on("pageChange", (page) => updateMyToolbar(page));
```

The viewer fills its container, so give the container a definite height.

| Subpath | Contents |
|---|---|
| `@parasma/snippet` | `<parasma-viewer>`, `mountViewer`, `registerElement`, types |
| `@parasma/snippet/headless` | `createHeadlessViewer`, `createViewerSession` (the state layer the React and Vue adapters bind), types |
| `@parasma/snippet/chrome` | The default toolbar and chrome factory, side-effect free |
| `@parasma/snippet/snippet.css` | The stylesheet, for setups that load CSS separately |

Styling: the `--ps-*` tokens and stable `.ps-*` class names are documented in
[docs/theming.md](https://github.com/spearsandshields/parasma/blob/main/docs/theming.md).

## The `<parasma-viewer>` element

The element registers and boots client-only. Attributes: `src`, `theme`
(`light`, `dark` or `auto`), `controls`, `dev`, `local-fonts`, `explorer`.

- The `options` **property** accepts a full `Partial<MountOptions>` — including
  the function-valued fields (`contextMenu`, `slots`, `onLinkActivate`,
  `explorer`) that can never ride string attributes. Set it before connect (or
  same-tick after append) for a single mount; **every live assignment
  remounts** — hoist the object, and assign a *new* object to change options
  (in-place mutation never re-applies).
- Every viewer event fans out as a DOM CustomEvent (bubbles + composed, payload
  as `detail`):

  | handle event | DOM event               | `detail`       |
  |--------------|-------------------------|----------------|
  | load         | `parasma-load`          | `DocumentInfo` |
  | unload       | `parasma-unload`        | `null`         |
  | error        | `parasma-error`         | `unknown`      |
  | pageChange   | `parasma-page-change`   | `number` (1-based) |
  | sheetChange  | `parasma-sheet-change`  | `number`       |
  | zoomChange   | `parasma-zoom-change`   | `number`       |
  | themeChange  | `parasma-theme-change`  | `"light" \| "dark"` |

  Events are **deltas after mount** — re-sync initial/reset state from
  `el.viewerHandle.state` in your `parasma-load`/`parasma-unload` handlers (the
  doc-switch page reset is contractually silent; teardown fires no
  `parasma-unload`).

Listening: Svelte `on:parasma-page-change`; Angular `(parasma-page-change)`;
Lit and plain DOM `addEventListener` (typed via the exported
`ParasmaViewerEventMap`). For Vue, configure
`isCustomElement: (t) => t.startsWith("parasma-")`, or use
[`@parasma/vue`](https://github.com/spearsandshields/parasma/tree/main/packages/vue).
For React, use [`@parasma/react`](https://github.com/spearsandshields/parasma/tree/main/packages/react).

**Precedence:** `options` is the base; *present* observed attributes
(`src`/`theme` incl. explicit `"auto"`/`controls`/`dev`/`local-fonts`) override
their fields. Boolean attributes are asymmetric: presence overrides to `true`,
absence is "unset" (defers to options) — to force `false`, omit the attribute
and set the option. Removing `dev`/`theme` live restores the options base
(`theme` only when concrete — `"auto"` cannot re-resolve without a remount);
removing `src` is a no-op. The `explorer` bare attribute carries no data:
`explorerConfig` property > `options.explorer` > empty-state panel.

## More

The [repository README](https://github.com/spearsandshields/parasma#readme) covers browser support, limitations,
privacy and building from source. API stability:
[docs/stability.md](https://github.com/spearsandshields/parasma/blob/main/docs/stability.md).
