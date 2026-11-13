# @parasma/react

React 19 bindings for the [Parasma](https://github.com/spearsandshields/parasma)
document viewer (docx / xlsx / pptx / csv). Hook-first: `useParasma` returns
the viewer state, typed commands and the raw handle, so a toolbar anywhere in
your tree can render from it. `<ParasmaViewer>` is a component built on the
hook.

```bash
npm install @parasma/react
```

**Vite:** add `optimizeDeps: { exclude: ["@parasma/core"] }` to your Vite
config. The dev server pre-bundles dependencies into a cache folder where the
engine can no longer find its WebAssembly file. Production builds need no
configuration.

## The component

```tsx
import { ParasmaViewer } from "@parasma/react";

function Preview({ file }: { file: File }) {
  const [page, setPage] = useState(1);
  return (
    <ParasmaViewer
      source={file}
      page={page}
      onPageChange={setPage}
      onError={(e) => console.error(e)}
      options={{ theme: "auto" }}
      style={{ height: "80vh" }}
    />
  );
}
```

`source` takes a `File`, `Blob`, `ArrayBuffer` or `Uint8Array`; change it to
load another document. `page` and `zoom` push down to the viewer, and the
viewer's own changes come back through `onPageChange` and `onZoomChange`.
Other callbacks: `onReady`, `onError`, `onTruncated`, `onLinkActivate`,
`onCellSelect`, `onCommentFocus`. `children` render beside the viewer, inside
its root. A `ref` receives the full hook result.

`options` (theme, worker, fonts, search, …) is read when the viewer starts.
To change it, remount the component with a new `key`.

## The hook

```tsx
import { useParasma } from "@parasma/react";

function Viewer({ bytes }: { bytes: Uint8Array }) {
  const p = useParasma({ theme: "auto" });
  useEffect(() => {
    void p.load(bytes, "report.docx").catch(() => {}); // status/error carry it
  }, [bytes, p.load]);
  return (
    <div style={{ height: "80vh", display: "flex", flexDirection: "column" }}>
      <div>
        <button onClick={p.commands.zoomOut}>−</button>
        <button onClick={p.commands.zoomIn}>+</button>
        {p.status === "ready" && `${p.currentPage} / ${p.pageCount}`}
      </div>
      <div ref={p.stageRef} style={{ flex: 1 }} />
    </div>
  );
}
```

The result holds the snapshot (`status`, `error`, `documentKind`, `pageSize`,
`pageCount`, `currentPage`, `truncatedAt`, `zoom`, `theme`, `sheets`,
`activeSheet`, `gridMode`, `cell`, `comments`, `focusedCommentId`,
`fontsLoading`, `tool`, `printProgress`, `search`), the verbs (`load`,
`unload`, `commands`, `viewState`, `applyViewState`), and the escape hatch
(`handle`, `ready`, `isReady`).

- **The stage element must stay empty.** The engine renders into it; put
  overlays and empty states beside it, never inside.
- **The stage needs a definite height**, on itself or from its parent. The
  engine scrolls internally.
- `p.status` moves `idle → loading → ready | error`. A load that a newer one
  supersedes never reports. Bytes in no supported format reject with
  `UnsupportedFormatError` before reaching the engine.
- StrictMode is supported: the second mount waits for the first viewer's
  teardown, so two viewers never share the stage.

## Server rendering

Importing the package touches no DOM, and the bundle carries
`"use client"`, so it works in a Next.js App Router tree. The viewer itself
starts only in the browser.

## React 18

This package needs React 19. On React 18, use the `<parasma-viewer>` element
from `@parasma/snippet` with a ref: set `el.options` as a property and attach
`parasma-*` event listeners in an effect.

## Versioning

Released with every other `@parasma` package at the same version. This
adapter's own API sits in the adapter tier of
[docs/stability.md](https://github.com/spearsandshields/parasma/blob/main/docs/stability.md):
it may change in any minor release, listed in the changelog. Its state rules
live in `createViewerSession` (`@parasma/snippet/headless`), shared with
`@parasma/vue`.
