/**
 * style-adopt — runtime style delivery that works everywhere.
 *
 * The old delivery was a side-effect `import "./snippet.css"`: it required the
 * consumer's bundler to extract CSS, landed the styles in the PAGE stylesheet
 * only — invisible inside a shadow root — and forced every shadow-DOM consumer
 * to hand-roll injection (fetch-or-inline, dedupe, teardown). This module
 * replaces it: at mount time the snippet's CSS is adopted into the scope's own
 * root — `document` or the consumer's `ShadowRoot` — via constructable
 * stylesheets, with a `<style>`-element fallback where they're unsupported.
 *
 * Properties, each deliberate:
 *  - IDEMPOTENT per root (a second viewer in the same root adopts nothing new);
 *  - NO disposer: styles are shared root-level infrastructure — removing them
 *    while a sibling instance lives would unstyle it, and an orphaned sheet on
 *    a root the consumer is about to discard costs nothing;
 *  - realm-correct (constructable sheets must come from the scope's OWN
 *    window: a sheet constructed in the top realm cannot be adopted by an
 *    iframe's document);
 *  - `styles: "manual"` opts out entirely — for consumers who import the
 *    exported `@parasma/snippet/snippet.css` subpath and own delivery.
 */
import type { RootScope } from "./root-scope";
import { SNIPPET_CSS } from "./snippet-styles.generated";

/** Roots that already carry the snippet styles (adopted or fallback). */
const delivered = new WeakSet<Document | ShadowRoot>();
/** One constructed sheet per realm window (shareable across that realm's roots). */
const sheetByWin = new WeakMap<Window, CSSStyleSheet>();

export function ensureSnippetStyles(scope: RootScope): void {
  const root = scope.rootNode;
  if (delivered.has(root)) return;

  const win = scope.win as Window & { CSSStyleSheet?: typeof CSSStyleSheet };
  const supportsConstructable =
    typeof win.CSSStyleSheet === "function" &&
    "replaceSync" in win.CSSStyleSheet.prototype &&
    "adoptedStyleSheets" in root;

  if (supportsConstructable) {
    let sheet = sheetByWin.get(scope.win);
    if (!sheet) {
      sheet = new win.CSSStyleSheet!();
      sheet.replaceSync(SNIPPET_CSS);
      sheetByWin.set(scope.win, sheet);
    }
    root.adoptedStyleSheets = [...root.adoptedStyleSheets, sheet];
  } else {
    // Fallback (older engines): a marked <style> in the root's head position.
    const style = scope.doc.createElement("style");
    style.setAttribute("data-parasma-styles", "");
    style.textContent = SNIPPET_CSS;
    if (root instanceof scope.doc.defaultView!.Document) {
      root.head.appendChild(style);
    } else {
      root.appendChild(style);
    }
  }
  delivered.add(root);
}
