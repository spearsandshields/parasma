# Stability contract

The index of promises. Promised names live in the cited type declarations and
are enforced by the cited tests — not duplicated here. Ratified 2026-07-30.

## Tier A — promised (renames/removals are breaking changes)

Downstream projects do not need their own rename-tripwire tests against
these; the pins live in this repository's suite.

| Surface | Authority | Enforcement |
|---|---|---|
| `@parasma/snippet/headless` — `createHeadlessViewer(config)` + every top-level `HeadlessViewerHandle` member (incl. `scrollToGridContent` / `scrollToAnchorPos`) | `packages/snippet/src/types.ts` | snippet suite |
| `SnippetEventMap` names + payload shapes, and their `parasma-*` DOM names | `types.ts`; `SNIPPET_DOM_EVENTS` in `element.ts` | `satisfies` exhaustiveness + name-lock in `element.test.ts` |
| `.ps-*` classes and `--ps-*` tokens | `docs/theming.md` (pointer, not a copy) | `element.test.ts` class pins; styles-agreement gate step |
| `@parasma/core` — `sniffKind`, `supportedFormats`, `UnsupportedFormatError` | `packages/parasma-core/src/formats.ts` | core suite |

## Adapter tier — `@parasma/vue`, `@parasma/react`

The framework adapters, and `createViewerSession` in
`@parasma/snippet/headless` that both are built on. They are released with
every other package at the same version, built and tested against it, so the
same version number always means they work together. Beyond that they make
no promise: names and shapes may change in any minor release, listed in
`CHANGELOG.md`. The `<parasma-viewer>` element and `createHeadlessViewer`
(Tier A) are the stable route from any framework.

| Surface | Authority | Enforcement |
|---|---|---|
| `@parasma/vue` — `useParasma`, `<ParasmaViewer>`, re-exports | `packages/vue/src/index.ts` | `packages/vue/src/*.test.ts` |
| `@parasma/react` — `useParasma`, `<ParasmaViewer>`, re-exports | `packages/react/src/index.ts` | `packages/react/src/*.test.ts` |
| `createViewerSession` + `ViewerSnapshot` | `packages/snippet/src/viewer-session.ts` | `viewer-session.test.ts`; both adapter suites run on the real session |

## Tier B — the escape hatch (reachable forever, names not frozen)

`handle.kernel` (command strings, bus events) and `handle.dom`: reachability
is Tier A (layer-never-gate); names/shapes are the surface plugins evolve
through. Payloads only grow (fields added, never repurposed). A consumer
depending on Tier B keeps a thin funnel; when a Tier B member proves
load-bearing, the remedy is promotion into Tier A, not freezing Tier B.

## How the surface grows (the capability rule)

1. **The session snapshot is the universal tier.** `ViewerSnapshot` holds
   lifecycle + what a generic BYO toolbar renders (status, pages, zoom,
   theme, search counter, sheet tabs, cell feed), and every adapter's
   `useParasma` exposes exactly that. No new format-specific members.
2. **Growth lands in capability lenses mirroring kernel plugins 1:1** —
   `useGrid(p)`, `useComments(p)`, named after the owning plugin, introduced
   only once they have content, written once in the session layer and bound
   by each adapter. Lenses are additive.
3. **Format is data, never structure.** `p.documentKind` says what's loaded;
   nothing is named after a file type — capabilities cross formats, so the
   public surface follows the kernel's capability axis.

At the handle layer: new hit-notifications become `SnippetEventMap` events,
new consumer-initiated verbs become handle methods; both Tier A from birth.
