# @parasma/headless — Command & Event Contract

The stable public surface for consumers building on the kernel directly.
Plugins never reach each other by import — only through these two channels:

- **Commands** — `await kernel.exec('namespace.command', ...args)` (always returns a `Promise`).
- **Events** — `kernel.bus.subscribe('event', handler)`. Sticky events replay catch-up
  history to late subscribers (see the per-event replay policy in `bus.ts`).

## Off-thread invariant

`exec` is **always async** (`commands.ts`: `exec<R>(...): Promise<R>`), and a command
handler may be a sync or async function — `exec` awaits it either way. Every command that
**reads** from the core `Viewer` now `await`s a `Promise`-returning Viewer method
(`pageCount()`, `pageSize()`, `cellAt()`, `sheetInfo()`, `gridExtent()`, `gridCellAt()`,
`linkRects()`, `bookmarkPosition()`, `comments()`, `commentRects()`, `searchPage()`).
These are **worker-ready seams**: in-process they resolve immediately; a worker RPC
transport slots UNDER the seam, leaving every command shape and every consumer
call site unchanged. **Consumers must already `await kernel.exec(...)`** — there is no
sync read path. The only token guarding document version is the internal `#loadGeneration`
in core (and `plugin-search`'s own generation token); it is never a public command parameter.

`cursor.resolve` does three Viewer reads (linkRects + commentRects + cellAt) and awaits
all three **internally** — it is ONE command, one `exec` round-trip, not three.

---

## Commands

### `loader.*` — document load lifecycle
| Command | Signature | Notes |
|---|---|---|
| `loader.open` | `(bytes: Uint8Array \| ArrayBuffer) => Promise<DocumentInfo>` | Serialized in core; one parse+layout at a time. Emits `pageReady`, then `loadComplete` (or `loader:error`). |
| `loader.openUrl` | `(url: string) => Promise<DocumentInfo>` | Fetch + `loader.open`. |
| `loader.status` | `() => LoaderStatus` | `"idle" \| "planning" \| "loading" \| "ready" \| "error"`. |

### `viewport.*` — page model + visible window
| Command | Signature | Notes |
|---|---|---|
| `viewport.pageModel` | `() => PageModel` | `{ pageCount, pageSize, loadComplete }`. |
| `viewport.setVisible` | `(indices: number[]) => void` | Consumer's virtual scroller reports visible pages; emits a `page` paint intent per newly-visible index. |
| `viewport.invalidateVisible` | `() => void` | Force-repaint every visible page (late font/image). |
| `viewport.reset` | `() => void` | Clear the page model on doc switch. |

### `zoom.*` — zoom control (two-tier, `ZOOM_MIN`..`ZOOM_MAX`)
| Command | Signature | Notes |
|---|---|---|
| `zoom.scale` | `() => number` | Current scale. |
| `zoom.set` | `(scale: number) => void` | Set exact (clamped). |
| `zoom.by` | `(factor: number) => void` | Multiply (wheel/pinch relative). |
| `zoom.in` / `zoom.out` | `() => void` | Step ≈1.25×. |
| `zoom.fit` | `(opts) => void` | Fit width/page; sets sticky `fitMode`. |
| `zoom.fitMode` | `() => FitMode` | `"none" \| "width" \| "page"`. |
| `zoom.setGesturing` | `(g: boolean) => void` | `true` during pinch (Tier A CSS), `false` at rest (Tier B crisp re-render). |
| `zoom.reset` | `() => void` | Clear fit mode + scale. |

### `sheets.*` — Excel sheet model (empty for non-spreadsheets)
| Command | Signature | Notes |
|---|---|---|
| `sheets.list` | `() => SheetInfo[]` | `[{ name, firstPage, pageCount }]`. |
| `sheets.active` | `() => number` | Active sheet index. |
| `sheets.select` | `(idx: number) => void` | Switch active sheet; emits `sheets:changed`. |

### `grid.*` — continuous grid vs print-page mode (Excel)
| Command | Signature | Notes |
|---|---|---|
| `grid.mode` | `() => GridMode` | `"grid" \| "pages"`. |
| `grid.setMode` | `(on: boolean) => void` | Excel-only; ignored for word/ppt. Emits `grid:mode-changed` (+ `grid:resetup`). |
| `grid.toggle` | `() => void` | Toggle grid ⇄ pages. |
| `grid.extent` | `(sheet: number) => Promise<{ width; height } \| null>` | Async (Viewer seam); spacer sizing. |
| `grid.cellAt` | `(q: GridCellQuery) => Promise<{ ref; raw; display } \| null>` | Async; formula-bar hit-test. |
| `grid.paintWindow` | `(p) => void` | Force a `grid-viewport` paint intent. |
| `grid.saveScroll` | `(p: { sheet; x; y }) => void` | Remember per-sheet scroll (content-space px). |
| `grid.scrollFor` | `(sheet: number) => GridScrollPos` | Restore remembered scroll. |

### `cursor.*` — single-owner page-cursor decision (pure)
| Command | Signature | Notes |
|---|---|---|
| `cursor.resolve` | `(q: CursorQuery) => Promise<string>` | CSS cursor keyword by priority: link/comment → `"pointer"`, excel cell → `"cell"`, else `""`. Awaits 3 Viewer reads internally — ONE round-trip. |

### `links.*` — link/anchor click decision (pure)
| Command | Signature | Notes |
|---|---|---|
| `links.linkAt` | `(q: LinkQuery) => Promise<LinkDecision>` | `{ kind:"url"; href } \| { kind:"anchor"; name; page; y } \| null`. Bookmark resolved internally. |

### `comments.*` — Word comment model + highlight↔card (empty for non-Word)
| Command | Signature | Notes |
|---|---|---|
| `comments.list` | `() => Comment[]` | Built on `loadComplete`. |
| `comments.at` | `(q: CommentAtQuery) => Promise<number[]>` | Async; comment ids whose rects cover (x,y). |
| `comments.focused` | `() => number \| null` | Focused id. |
| `comments.focus` | `(id: number) => void` | Focus; emits `comments:changed`. |

### `search.*` — full-text search engine logic
| Command | Signature | Notes |
|---|---|---|
| `search.query` | `(text: string) => void` | Debounced, generation-guarded chunked loop; streams `search:changed`. |
| `search.setScope` | `(scope: SearchScope) => void` | `"sheet" \| "workbook"` (SML; inert for WML). |
| `search.opts` | `(opts: { caseSensitive; wholeWord }) => void` | Re-runs the loop. |
| `search.next` / `search.prev` | `() => void` | Wrap-around; emits `search:navigate`. |
| `search.goto` | `(i: number) => void` | Jump (clamped). |
| `search.clear` | `() => void` | Esc/clear. |
| `search.results` | `() => SearchMatch[]` | Live (partial) results. |
| `search.active` | `() => number \| null` | Active match index. |

### `fonts.*` — font lifecycle (2nd XState machine)
| Command | Signature | Notes |
|---|---|---|
| `fonts.status` | `() => FontsStatus` | `"idle" \| "loading" \| "ready"`. |

---

## Events (`kernel.bus`)

The canonical typed contract is `BusEventMap` in `bus.ts` (each member carries its own
TSDoc + replay policy). Summary by replay policy:

### Sticky — `"latest"` (a late subscriber replays the last payload)
`loadComplete` · `loader:loaded` · `loader:status-changed` · `loader:load-started` ·
`fonts:status-changed` · `fonts:ready` · `zoom:changed` · `sheets:changed` ·
`grid:mode-changed` · `grid:resetup` · `comments:changed` · `search:changed`

### Accumulating — `"accumulate"` (replays every retained payload, in order)
`pageReady` (one per page index laid out — mirrors the Viewer's per-index catch-up)

### Transient — `"none"` (never replayed; the effect already happened)
`imageReady` · `loader:error` · `fonts:variant-arrived` · `search:navigate`

> Plugin → bus → consumer is the only path. Plugins NEVER touch the DOM; the consumer's
> adapters apply paint intents and bus events to pixels. The command/event names and
> payload shapes are versioned with the package.
