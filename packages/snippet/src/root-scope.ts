/**
 * RootScope — the ONE DOM seam every snippet adapter derives its browser
 * concerns from (the root-scoping contract). Instead of reading the
 * page-global `window` / `document` / bare `instanceof HTML*`, an adapter is
 * handed a RootScope built ONCE (in mount.ts) from the parent-supplied root, and
 * reads `scope.doc` / `scope.win` / `scope.elementFromPoint` / `scope.activeElement`
 * / `scope.isHTMLElement` … from it. This makes the adapters correct inside a
 * shadow DOM (hit-test + focus resolve to the shadow's own root, not the host),
 * inside an iframe (realm-safe type guards + the iframe's own window/listeners),
 * and with multiple instances on one page (no cross-fire, one minted instanceId).
 *
 * STANDALONE IDENTITY (the enforced regression contract): createRootScope reads
 * NOTHING from the global window/document — every accessor is a closure over the
 * {host, rootNode, doc, win} derived purely from the passed root. In the demo the
 * root lives in the main document, so host.ownerDocument === document and
 * doc.defaultView === window, and every accessor collapses to exactly today's
 * global call (scope.elementFromPoint === document.elementFromPoint, scope.win ===
 * window, scope.isHTMLElement(n) === `n instanceof HTMLElement` for a main-realm
 * node, …). RootScope is a seam that HAPPENS to equal the globals standalone.
 *
 * PROMOTED to a PUBLIC export: a parent
 * authoring its OWN adapters constructs a RootScope over its root and derives
 * doc / win / hit-test / activeElement the shadow-correct way — RootScope is the
 * documented "author-your-own-adapter" contract.
 */

/**
 * The realm + root a snippet adapter reads every DOM concern from. Built once by
 * {@link createRootScope} from the parent-supplied root; threaded to each adapter.
 */
export interface RootScope {
  /** The root the viewer mounts into (a normal element, or a ShadowRoot). */
  readonly root: HTMLElement | ShadowRoot;
  /** The owning document (host.ownerDocument — iframe-correct). */
  readonly doc: Document;
  /** The realm window (doc.defaultView — the iframe/shadow-host's own window). */
  readonly win: Window;
  /**
   * The hit-test / active-element / selection scope (root.getRootNode()): the
   * Document standalone, or the ShadowRoot when mounted in a shadow tree — a
   * ShadowRoot has its OWN elementFromPoint/activeElement that resolve the inner
   * target, where document.* would return the shadow HOST.
   */
  readonly rootNode: Document | ShadowRoot;
  /** A per-mount id (coexistence token + the per-instance font-family suffix source). */
  readonly instanceId: string;
  /** win.devicePixelRatio (the device grid factor for canvas snapping). */
  dpr(): number;
  /** rootNode.elementFromPoint — shadow-correct (returns the inner target). */
  elementFromPoint(x: number, y: number): Element | null;
  /** rootNode.activeElement — a ShadowRoot's own focus, fixes the copy focus-gate. */
  activeElement(): Element | null;
  /** ShadowRoot.getSelection() when present (Chrome), else win.getSelection(). */
  getSelection(): Selection | null;
  /** doc.createRange() (the realm's document). */
  createRange(): Range;
  /** host.getBoundingClientRect() (the mounted slot's viewport rect). */
  viewportRect(): DOMRect;
  /** Realm-safe `x instanceof win.Element`. */
  isElement(x: unknown): x is Element;
  /** Realm-safe `x instanceof win.HTMLElement` (THE iframe cross-realm fix). */
  isHTMLElement(x: unknown): x is HTMLElement;
  /** Realm-safe `x instanceof win.HTMLCanvasElement`. */
  isCanvas(x: unknown): x is HTMLCanvasElement;
}

/** Monotonic per-module counter → the human-readable prefix of each instanceId. */
let moduleSeq = 0;

/** ShadowRoot.getSelection() is a non-standard (Chrome) extension — model it. */
type ShadowRootWithSelection = ShadowRoot & { getSelection?: () => Selection | null };

/**
 * Build a {@link RootScope} over `root` (a mount element or a ShadowRoot). Reads
 * NOTHING from the global window/document: the realm (win) is `root`'s owning
 * document's defaultView, so an iframe/shadow root yields ITS window. Throws a
 * clear error for a detached document (no defaultView).
 */
export function createRootScope(root: HTMLElement | ShadowRoot): RootScope {
  // A ShadowRoot has a `host` element (whose ownerDocument is the real document);
  // a plain element IS its own host. getBoundingClientRect / ownerDocument live on
  // the host, never on a ShadowRoot — hence deriving the host internally.
  const host: Element = "host" in root ? root.host : root;
  const rootNode = root.getRootNode() as Document | ShadowRoot;
  const doc = host.ownerDocument;
  const win = doc.defaultView;
  if (!win) {
    throw new Error(
      "createRootScope: the root's document has no defaultView (detached document) — " +
        "mount into a live, attached root.",
    );
  }
  const uuid = globalThis.crypto?.randomUUID?.() ?? Math.random().toString(36).slice(2, 10);
  const instanceId = "p" + (moduleSeq++).toString(36) + "-" + uuid.slice(0, 8);

  return {
    root,
    doc,
    win,
    rootNode,
    instanceId,
    dpr: () => win.devicePixelRatio || 1,
    elementFromPoint: (x, y) => rootNode.elementFromPoint(x, y),
    activeElement: () => rootNode.activeElement,
    getSelection: () => {
      // A ShadowRoot has its OWN selection scope on Chrome; fall back to the
      // realm window elsewhere (documented Chrome-only shadow limitation).
      if (rootNode !== doc) {
        const sr = rootNode as ShadowRootWithSelection;
        if (typeof sr.getSelection === "function") return sr.getSelection();
      }
      return win.getSelection();
    },
    createRange: () => doc.createRange(),
    viewportRect: () => host.getBoundingClientRect(),
    isElement: (x): x is Element => x instanceof win.Element,
    isHTMLElement: (x): x is HTMLElement => x instanceof win.HTMLElement,
    isCanvas: (x): x is HTMLCanvasElement => x instanceof win.HTMLCanvasElement,
  };
}
