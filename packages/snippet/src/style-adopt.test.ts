import { describe, it, expect } from "vitest";
import { ensureSnippetStyles } from "./style-adopt";
import { SNIPPET_CSS } from "./snippet-styles.generated";
import type { RootScope } from "./root-scope";

// (The generated⇔authored agreement check lives in the GATE — scripts/
// gen-snippet-styles.mjs --check — because vitest's css pipeline swallows
// `.css?raw` into an empty string, which made a vitest version of the check
// vacuously red. See the script.)

// ── adoption behavior over fake scopes (node env, realm-correct by design) ──
class FakeSheet {
  text = "";
  replaceSync(t: string): void {
    this.text = t;
  }
}
function fakeScope() {
  const rootNode = { adoptedStyleSheets: [] as unknown[] } as unknown as Document;
  const win = { CSSStyleSheet: FakeSheet } as unknown as Window;
  return { rootNode, win, doc: {} } as unknown as RootScope;
}

describe("ensureSnippetStyles", () => {
  it("adopts one constructed sheet into the root, idempotently", () => {
    const scope = fakeScope();
    ensureSnippetStyles(scope);
    const sheets = (scope.rootNode as Document).adoptedStyleSheets;
    expect(sheets.length).toBe(1);
    expect((sheets[0] as unknown as FakeSheet).text).toBe(SNIPPET_CSS);
    ensureSnippetStyles(scope); // second instance, same root → no double-adopt
    expect((scope.rootNode as Document).adoptedStyleSheets.length).toBe(1);
  });

  it("shares ONE sheet across roots of the same realm window", () => {
    const scopeA = fakeScope();
    const scopeB = { ...fakeScope(), win: scopeA.win } as RootScope;
    ensureSnippetStyles(scopeA);
    ensureSnippetStyles(scopeB);
    const a = (scopeA.rootNode as Document).adoptedStyleSheets[0];
    const b = (scopeB.rootNode as Document).adoptedStyleSheets[0];
    expect(a).toBe(b); // same realm ⇒ same constructed sheet object
  });
});
