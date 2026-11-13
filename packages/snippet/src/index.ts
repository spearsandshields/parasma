/**
 * @parasma/snippet — the batteries-included drop-in viewer tier.
 *
 * Layers: @parasma/core (engine) → @parasma/headless (plugins) → @parasma/snippet
 * (default UI). Composes the 13 headless plugins + the DOM adapters behind a
 * polished, format-aware default chrome with a toggleable developer instrument.
 *
 *   import { mountViewer } from "@parasma/snippet";
 *   const v = mountViewer("#app", { src: "/doc.docx", theme: "auto" });
 *
 *   // or the web component:
 *   import { registerElement } from "@parasma/snippet";
 *   registerElement();
 *   // <parasma-viewer src="/doc.docx" theme="dark" controls dev></parasma-viewer>
 */
export { mountViewer } from "./mount";
// SNIPPET_DOM_EVENTS / ParasmaViewerEventMap are ELEMENT-TIER API (the
// CustomEvent fan-out contract) — exported from this barrel only, not from
// ./headless or ./chrome.
export {
  registerElement,
  ParasmaViewerElement,
  SNIPPET_DOM_EVENTS,
  type ParasmaViewerEventMap,
} from "./element";
// RootScope is a PUBLIC export so a parent authoring its OWN
// DOM adapters derives doc/win/hit-test/activeElement the shadow+realm-correct
// way — the documented "author-your-own-adapter" contract.
export { createRootScope, type RootScope } from "./root-scope";
// The chrome-free composition seam. Additively re-exported here
// for discoverability — the BLESSED side-effect-free import is the subpath
// `@parasma/snippet/headless` (this barrel self-registers the custom element,
// the subpath does NOT).
export { createHeadlessViewer } from "./headless-viewer";
export { attachDom } from "./attach-dom";
export type {
  MountOptions,
  ViewerHandle,
  ViewerState,
  CoreViewerState,
  HeadlessViewerConfig,
  HeadlessViewerHandle,
  DomBinding,
  AttachDomOptions,
  DevHooks,
  SnippetEvent,
  SnippetEventMap,
  Theme,
  ThemeOption,
  ExplorerConfig,
  ExplorerEntry,
  ExplorerFormat,
} from "./types";
// The `parasma-load` CustomEvent detail (the Vue recipe types its handler with
// this) — public at the core tier; re-exported so a recipe consumer never needs
// a second package import.
export type { DocumentInfo } from "@parasma/core";

// Auto-register the element when the bundle loads in a browser (side-effect),
// so `<parasma-viewer>` works from a plain script import without a manual call.
import { registerElement } from "./element";
if (typeof window !== "undefined") registerElement();
