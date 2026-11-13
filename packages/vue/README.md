# @parasma/vue

Vue 3 bindings for the [Parasma](https://github.com/spearsandshields/parasma) document viewer (docx / xlsx / pptx /
csv). Composable-first: state and verbs are plain refs on `useParasma`'s
return — consumable from any component, sibling, or store — and the raw
handle is always exposed. `<ParasmaViewer>` is sugar built on it.

```bash
npm install @parasma/vue
```

**Vite:** add `optimizeDeps: { exclude: ["@parasma/core"] }` to your Vite
config. The dev server pre-bundles dependencies into a cache folder where the
engine can no longer find its WebAssembly file. Production builds need no
configuration.

```ts
const stage = ref<HTMLElement>();
const p = useParasma(stage, { theme: "auto" });
// p.status  p.pageCount  p.currentPage  p.sheets  p.cell  p.search  p.comments …
// p.commands.{goToPage,zoomIn,fitWidth,search,scrollToAnchorPos,toggleFullscreen,…}
// p.load(bytes, name)  p.viewState()  p.handle (the full headless handle)
```

```vue
<ParasmaViewer :source="file" v-model:page="page" :options="{ theme: 'auto' }" />
```

A complete BYO-chrome viewer (toolbar, formula bar, sheet tabs, search with
counter, tool toggle, print, truncation notice) fits a test-enforced 70-line
ceiling: [`examples/mini-viewer.ts`](https://github.com/spearsandshields/parasma/blob/main/packages/vue/examples/mini-viewer.ts).

## The status machine

`p.status`: `idle → loading → ready | error`; back to `loading` on the next
`load()`. A superseded in-flight load never fires its tail (no stale
`ready`/`error`). `unload()` → `idle`. Unrecognizable bytes reject with the
typed `UnsupportedFormatError` before reaching the engine; `sniffKind` and
`supportedFormats` are exported for pickers and dispatch shells.

Error taxonomy (three classes, three surfaces):
1. **Unsupported bytes** — `load()` rejects with `UnsupportedFormatError`
   before the engine runs; branch "unsupported" vs "broken" UI on
   `instanceof`.
2. **Load failure** (corrupt/unparseable document) — `load()` rejects,
   `status === "error"`, `p.error` holds the engine's rejection.
3. **Runtime engine errors after a successful load** (worker death, panic) —
   the handle's `error` event (`@error` on the component); the rendered view
   stays as-is and `p.status` does not change.

## Recipes

- **App-owned toolbar**: hand `p.commands` to your toolbar; display state
  from `p.currentPage`/`p.zoom`. The select↔hand tool pair (PDF-viewer
  style) is `p.tool` + `p.commands.setTool`/`toggleTool`; selection is the
  default, space-hold pans in either mode, pinch/trackpad zoom is
  tool-independent.
- **View persistence**: store `p.viewState()` anywhere; restore via
  `options.initialViewState`.
- **Theme**: `theme: "auto"` live-mirrors your app's `data-theme` (shadow
  hosts included) — your own toggle is the dynamic path.
- **Fullscreen incl. your chrome**: `p.commands.toggleFullscreen(wrapperEl)`;
  pair with any fullscreen composable for reactive state.
- **Custom element**: `defineCustomElement(ParasmaViewer)` works — boots in
  the shadow root, styles adopt into it, tokens pierce it ([docs/theming.md](https://github.com/spearsandshields/parasma/blob/main/docs/theming.md)).
- **Link confirmation**: `linkActivate` is cancelable. Veto synchronously,
  confirm, open yourself — the confirm click supplies fresh user activation,
  so the open is not popup-blocked. Hrefs are pre-sanitized
  (http/https/mailto/tel only); default navigation is `noopener,noreferrer`.

  ```ts
  p.handle.value?.on("linkActivate", (e) => {
    if (e.kind !== "url") return; // in-document anchors: let them scroll
    e.preventDefault();
    myDialog.confirm(`Open ${e.href}?`).then((ok) => {
      if (ok) window.open(e.href, "_blank", "noopener,noreferrer");
    });
  }); // or @link-activate / parasma-link-activate
  ```
- **Sizing contract**: the element you hand `useParasma` needs a definite
  height — on itself (your rule wins; the shipped root rule is
  zero-specificity) or from its parent (default `height:100%`). The engine
  scrolls internally; with no definite height the element grows to content
  height and the spreadsheet grid's million-px scroll spacer blanks the
  canvas.
- **Empty state**: render idle content gated on `p.status === "idle"` as an
  overlay or sibling — **never as a child of the stage ref** (Vue reconciling
  children there clobbers the engine's DOM; the mini viewer shows the
  overlay pattern).
- **Loading skeleton**: `p.pageSize` carries real page dimensions the moment
  `load()` resolves (post-parse, pre-layout) — spinner during parse only,
  then a correctly-sized skeleton; `p.pageCount` streams in.
- **Print** (word/pptx): `p.commands.print({ from?, to?, scale?, format? })`
  — rasterizes pages sequentially (one live canvas; memory is flat in
  document length) into a hidden iframe and opens the dialog; your app needs
  no `@media print` CSS. `p.printProgress` streams exact `{done, total}` for
  a determinate bar. Prefer page ranges for very long documents — the
  browser's own print spooler is the bottleneck near 1k pages. Spreadsheets
  reject for now. Download needs no API: you own the bytes you passed to
  `load()` — Blob-URL them.
- **Search**: `p.commands.search(q)` / `searchNext()` / `searchPrev()`;
  `p.search` streams `{ query, total, activeIdx, status, scope }`.
  Spreadsheet scope defaults to the active sheet (Excel's Within:Sheet);
  `p.commands.setSearchScope("workbook")` widens. Reveal and repaint are the
  engine's job. Match geometry stays on the bus (`search:changed`); no-op
  streaming ticks are deduped, so effects fire only on real movement.
- **BYO comments panel / custom reveals**: `p.comments` +
  `p.focusedCommentId` render the panel; `@comment-focus` (or
  `handle.on("commentFocus")`) is the doc→panel click pulse (an event —
  re-clicking the same comment fires again); card click →
  `p.commands.scrollToAnchorPos(page, anchorY)`; `scrollToGridContent(x, y)`
  is the grid-mode sibling; `@cell-select` mirrors `p.cell` as an event.

## SSR

Import-safe by construction (no DOM access at import time; work starts at
mount). Rendering itself is client-only.

## Versioning

Released with every other `@parasma` package at the same version. This
adapter's own API sits in the adapter tier of
[docs/stability.md](https://github.com/spearsandshields/parasma/blob/main/docs/stability.md): it may change in any
minor release, listed in the changelog. Its state rules live in
`createViewerSession` (`@parasma/snippet/headless`), shared with
`@parasma/react`.
