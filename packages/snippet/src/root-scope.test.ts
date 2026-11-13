import { describe, it, expect } from "vitest";
import { createRootScope } from "./root-scope";

/**
 * These tests run under the `node` vitest env (no DOM). That is a FEATURE here:
 * createRootScope must read NOTHING from a global window/document, so it works
 * over hand-built fakes. If it ever reached for a real global these would throw
 * (ReferenceError: window/document is not defined) rather than pass on a shim.
 */

// A distinct-realm constructor set (an iframe's window has its OWN Element/…),
// so a node minted here fails a bare `instanceof HTMLElement` in the "main" realm.
class ForeignElement {}
class ForeignHTMLElement extends ForeignElement {}
class ForeignHTMLCanvasElement extends ForeignHTMLElement {}

function fakeWin(over: Partial<Record<string, unknown>> = {}) {
  return {
    Element: ForeignElement,
    HTMLElement: ForeignHTMLElement,
    HTMLCanvasElement: ForeignHTMLCanvasElement,
    devicePixelRatio: 2.5,
    getSelection: () => ({ tag: "win-selection" }) as unknown as Selection,
    ...over,
  } as unknown as Window;
}

/** A host element whose ownerDocument.defaultView is `win`. */
function fakeHost(doc: Document) {
  return {
    ownerDocument: doc,
    getBoundingClientRect: () => ({ x: 5, y: 6, width: 100, height: 200 }) as DOMRect,
  } as unknown as HTMLElement;
}

describe("createRootScope — shadow root", () => {
  it("routes elementFromPoint / activeElement / getSelection to the shadow rootNode, not the document", () => {
    const shadowActive = { id: "shadow-active" } as unknown as Element;
    const shadowHit = { id: "shadow-hit" } as unknown as Element;
    const shadowSelection = { tag: "shadow-selection" } as unknown as Selection;
    const win = fakeWin();
    const doc = {
      defaultView: win,
      // The DOCUMENT's own values differ — if the scope wrongly used the document
      // these would surface instead of the shadow ones.
      activeElement: { id: "doc-active" } as unknown as Element,
      elementFromPoint: () => ({ id: "doc-hit" }) as unknown as Element,
      createRange: () => ({ tag: "range" }) as unknown as Range,
    } as unknown as Document;
    const host = fakeHost(doc);
    // rootNode = the shadow root; it has its OWN hit-test/active/selection scope.
    const shadow = {
      host,
      activeElement: shadowActive,
      elementFromPoint: () => shadowHit,
      getSelection: () => shadowSelection,
      getRootNode() {
        return shadow;
      },
    } as unknown as ShadowRoot;

    const scope = createRootScope(shadow);

    expect(scope.rootNode).toBe(shadow);
    expect(scope.doc).toBe(doc);
    expect(scope.win).toBe(win);
    expect(scope.elementFromPoint(1, 2)).toBe(shadowHit);
    expect(scope.activeElement()).toBe(shadowActive);
    // shadow branch: rootNode !== doc AND rootNode.getSelection is a function.
    expect(scope.getSelection()).toBe(shadowSelection);
    // viewportRect reads the HOST rect (a ShadowRoot has no getBoundingClientRect).
    expect(scope.viewportRect().width).toBe(100);
  });

  it("falls back to win.getSelection() when the ShadowRoot has no getSelection (non-Chrome)", () => {
    const winSelection = { tag: "win-selection" } as unknown as Selection;
    const win = fakeWin({ getSelection: () => winSelection });
    const doc = { defaultView: win } as unknown as Document;
    const host = fakeHost(doc);
    const shadow = {
      host,
      activeElement: null,
      elementFromPoint: () => null,
      // no getSelection — the Firefox/Safari case
      getRootNode() {
        return shadow;
      },
    } as unknown as ShadowRoot;
    const scope = createRootScope(shadow);
    expect(scope.getSelection()).toBe(winSelection);
  });
});

describe("createRootScope — realm-safe predicates", () => {
  it("uses the derived window's constructors, not the main realm's", () => {
    const win = fakeWin();
    const doc = { defaultView: win } as unknown as Document;
    const host = fakeHost(doc);
    // A plain element in the main document; rootNode is that document.
    const el = {
      ownerDocument: doc,
      getBoundingClientRect: () => ({ width: 1, height: 1 }) as DOMRect,
      getRootNode() {
        return doc;
      },
    } as unknown as HTMLElement;
    const scope = createRootScope(el);

    const foreignEl = new ForeignHTMLElement();
    const foreignCanvas = new ForeignHTMLCanvasElement();
    // The scope's realm-aware guard is TRUE for the foreign-realm node …
    expect(scope.isHTMLElement(foreignEl)).toBe(true);
    expect(scope.isElement(foreignEl)).toBe(true);
    expect(scope.isCanvas(foreignCanvas)).toBe(true);
    expect(scope.isCanvas(foreignEl)).toBe(false);
    // … while a bare instanceof against the main realm's globals would be FALSE
    // (the iframe cross-realm case the guards exist for). `HTMLElement` may be
    // undefined in node — either way it is NOT ForeignHTMLElement.
    const mainHtmlEl = (globalThis as { HTMLElement?: unknown }).HTMLElement;
    expect(mainHtmlEl === ForeignHTMLElement).toBe(false);
  });
});

describe("createRootScope — standalone identity (pure passthrough)", () => {
  it("collapses every accessor to the derived (host-owned) globals", () => {
    const winSelection = { tag: "sel" } as unknown as Selection;
    const range = { tag: "range" } as unknown as Range;
    const active = { id: "active" } as unknown as Element;
    const hit = { id: "hit" } as unknown as Element;
    const win = fakeWin({ devicePixelRatio: 1.75, getSelection: () => winSelection });
    const doc = {
      defaultView: win,
      activeElement: active,
      elementFromPoint: () => hit,
      createRange: () => range,
    } as unknown as Document;
    const host = {
      ownerDocument: doc,
      getBoundingClientRect: () => ({ width: 42, height: 7 }) as DOMRect,
      getRootNode() {
        return doc; // main-document element → rootNode === doc
      },
    } as unknown as HTMLElement;

    const scope = createRootScope(host);

    // rootNode === doc, so NO shadow branch: every accessor is the document/window.
    expect(scope.doc).toBe(doc);
    expect(scope.win).toBe(win);
    expect(scope.rootNode).toBe(doc);
    expect(scope.dpr()).toBe(1.75);
    expect(scope.elementFromPoint(0, 0)).toBe(hit);
    expect(scope.activeElement()).toBe(active);
    expect(scope.getSelection()).toBe(winSelection); // win.getSelection(), not shadow
    expect(scope.createRange()).toBe(range);
    expect(scope.viewportRect().width).toBe(42);
    expect(typeof scope.instanceId).toBe("string");
    expect(scope.instanceId.length).toBeGreaterThan(0);
  });

  it("mints a UNIQUE instanceId per scope", () => {
    const win = fakeWin();
    const doc = { defaultView: win } as unknown as Document;
    const mk = () => {
      const host = {
        ownerDocument: doc,
        getBoundingClientRect: () => ({}) as DOMRect,
        getRootNode() {
          return doc;
        },
      } as unknown as HTMLElement;
      return createRootScope(host).instanceId;
    };
    expect(mk()).not.toBe(mk());
  });
});

describe("createRootScope — detached document guard", () => {
  it("throws a clear error when defaultView is null", () => {
    const doc = { defaultView: null } as unknown as Document;
    const host = {
      ownerDocument: doc,
      getBoundingClientRect: () => ({}) as DOMRect,
      getRootNode() {
        return doc;
      },
    } as unknown as HTMLElement;
    expect(() => createRootScope(host)).toThrow(/detached document/);
  });
});
