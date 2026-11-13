/**
 * External link activation must not leak the embedder's URL.
 *
 * `attachDom`'s click handler opens a `kind:"url"` link with
 * `win.open(href, "_blank", <features>)`. `noopener` alone severs the opener
 * reference but still sends the embedder's page URL to the destination in the
 * `Referer` header; `noreferrer` is what suppresses it (and implies noopener).
 *
 * This is a SOURCE guard, not a behavioural one: `attachDom` builds the whole
 * stage with `document.createElement` and the snippet suite runs under vitest's
 * `node` environment (no DOM), so the click path cannot be driven here. The
 * behaviour itself was verified in the browser — a click on the link in
 * hyperlink-external.docx produced
 * `window.open("https://example.com", "_blank", "noopener,noreferrer")`.
 * Deleting the word from attach-dom.ts fails this test.
 */
/// <reference types="vite/client" />
import { describe, it, expect } from "vitest";
// Vite's ?raw gives the SOURCE without node:fs (tsconfig "types":[] has no
// @types/node — the same pattern chrome-barrel.test.ts uses).
import attachDomSource from "./attach-dom.ts?raw";

describe("a refused link never reaches a consumer", () => {
  /**
   * The security property, stated precisely: sanitization happens in HEADLESS
   * (`links.linkAt` routes through `sanitizeHref` and returns `null` for a
   * target outside {http,https,mailto,tel}), and `attachDom` returns on `null`
   * BEFORE emitting. So a consumer's `linkActivate` handler — and the
   * `parasma-link-activate` DOM event — can only ever see an allowlisted href.
   *
   * The behaviour of the sanitizer itself is pinned in
   * headless/src/plugins/links.test.ts (11 cases; 7 go red without it). What is
   * pinned HERE is the ORDERING, which those tests cannot see: moving the emit
   * above the null-guard would hand every refused target straight to consumers
   * while leaving the headless suite green.
   */
  it("guards on a null decision BEFORE emitting linkActivate", () => {
    const guard = attachDomSource.indexOf("if (!d) return;");
    const emit = attachDomSource.indexOf("emitLinkActivate?.(");
    expect(guard, "the `if (!d) return;` null-decision guard").toBeGreaterThan(-1);
    expect(emit, "the emitLinkActivate call").toBeGreaterThan(-1);
    expect(guard, "a refused link must be dropped BEFORE the consumer sees it").toBeLessThan(emit);
  });

  it("emits from exactly ONE site, so the guard cannot be bypassed", () => {
    const sites = attachDomSource.match(/emitLinkActivate\?\.\(/g) ?? [];
    expect(sites).toHaveLength(1);
  });

  it("navigates only after the veto is read", () => {
    const emit = attachDomSource.indexOf("emitLinkActivate?.(");
    const veto = attachDomSource.indexOf("if (prevented) return;");
    const open = attachDomSource.indexOf("win.open(d.href");
    expect(veto, "the preventDefault() veto check").toBeGreaterThan(emit);
    expect(open, "win.open must follow the veto check").toBeGreaterThan(veto);
  });
});

describe("link activation window features", () => {
  it("passes BOTH noopener and noreferrer to win.open", () => {
    const call = /win\.open\(\s*d\.href\s*,\s*"_blank"\s*,\s*"([^"]*)"\s*\)/.exec(attachDomSource);
    expect(call, "the win.open(d.href, ...) link-activation call").not.toBeNull();
    const features = call![1].split(",").map((f) => f.trim());
    expect(features).toContain("noopener");
    expect(features).toContain("noreferrer");
  });
});
