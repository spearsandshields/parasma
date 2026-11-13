/**
 * plugin-links — the SINGLE OWNER of the link/anchor CLICK DECISION.
 *
 * A DOM-free, store-less PURE-DECISION plugin (mirrors plugin-cursor): it holds
 * no slice and never touches the DOM, window, or canvas. It exposes ONE query
 * command, `links.linkAt({page,x,y}) => LinkDecision`, that maps a CONTENT
 * coordinate to what a click there would activate — an external url, an internal
 * anchor (with its bookmark already resolved to a {page,y} target), or nothing.
 *
 * Plugin DECIDES; consumer APPLIES. The CONSUMER (e.g. @parasma/snippet) owns the single
 * `click` applier: it maps the click event → page + content-coord (dividing by
 * the zoom scale — see plugin-zoom, same correction as the cursor applier), calls
 * links.linkAt, and applies the decision (window.open for a url, scroll for an
 * anchor) after announcing it on the `linkActivate` EVENT, which a consumer
 * vetoes with `preventDefault()`. It is an event, not a callback option,
 * because a plain-markup `<parasma-viewer>` consumer cannot pass a callback:
 * functions cannot ride HTML attributes.
 *
 * The activation POLICY is the consumer's, not the plugin's. The DEFAULT is to
 * open, which is safe to say only because this plugin refuses anything outside
 * the {http,https,mailto,tel} allowlist below — a refused target is reported as
 * NO LINK, never as a link the consumer must know to block. The plugin stays
 * pure: it would need window/DOM knowledge to activate, which it must not have.
 *
 * The link CURSOR (pointer over a link rect) is NOT here — that is rule 1 of
 * plugin-cursor (cursor.resolve). plugin-links owns the CLICK only; both plugins
 * independently read the shared public `viewer.linkRects` hit-test (cursor for
 * the cursor shape, links for the activation target) — the outputs differ, so it
 * is the shared hit-test, not duplicated decision logic.
 *
 * deps: NONE. linkRects guards documentKind in core (returns [] off-document /
 * for non-link kinds), and bookmarkPosition is read on demand, so the decision
 * needs no loader cache, no documentKind, and no viewport runtime state. (The
 * content coords the caller passes come from the consumer's content-coord source
 * — the same one the cursor applier uses — not a runtime plugin dependency.)
 */

import { sanitizeHref } from "./flavors/sanitize";
import type { Plugin } from "../registry";
import type { KernelContext } from "../kernel-context";
import type { LinksViewer } from "./ports";

/**
 * Commands OWNED by this plugin. Declaration-merged into the open
 * {@link CommandMap} (targeting `../commands`, the barrel-re-exported kernel
 * module → program-wide) so `exec(id, …)` is arg/return type-checked and an
 * undeclared id fails to register. Signatures authored from the handler bodies.
 */
declare module "../commands" {
  interface CommandMap {
    "links.linkAt": (q: LinkQuery) => Promise<LinkDecision>;
  }
}

/** Content-coordinate query for `links.linkAt` (layout px, already unscaled). */
export interface LinkQuery {
  /** 0-based page index. */
  page: number;
  /** X in layout px within the page (caller divides client offset by zoom scale). */
  x: number;
  /** Y in layout px within the page. */
  y: number;
}

/**
 * The pure decision returned by `links.linkAt`:
 *  - a `url` variant carrying the external href, or
 *  - an `anchor` variant carrying the bookmark name + its RESOLVED {page,y}, or
 *  - `null` for no link (miss, or a dead bookmark that resolves to nothing).
 */
export type LinkDecision =
  | { kind: "url"; href: string }
  | { kind: "anchor"; name: string; page: number; y: number }
  | null;

/** Inclusive-bounds hit-test — the SAME helper shape plugin-cursor uses. */
function contains(r: { x: number; y: number; w: number; h: number }, x: number, y: number): boolean {
  return x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h;
}

/**
 * The links plugin. Construct with the SAME Viewer the consumer renders into
 * (mirrors cursorPlugin(viewer) / viewportPlugin(viewer)) so it can read the live
 * `viewer.linkRects` / `viewer.bookmarkPosition` on demand.
 */
export function linksPlugin(viewer: LinksViewer): Plugin {
  const unsubs: Array<() => void> = [];
  return {
    id: "links",

    initialize(ctx: KernelContext) {
      // The single PURE decision. Now async: the Viewer hit-tests (linkRects,
      // bookmarkPosition) are worker-ready seams — awaited internally so
      // links.linkAt stays ONE command (a click tolerates the microtask hop).
      unsubs.push(
        ctx.commands.register("links.linkAt", async (q: LinkQuery): Promise<LinkDecision> => {
        const { page, x, y } = q;

        // First rect under (x,y) wins — same inclusive contains as cursor.
        const hit = (await viewer.linkRects(page)).find((r) => contains(r, x, y));
        if (!hit) return null;

        const { target } = hit;

        // External link: "url:<href>" → the url variant, THROUGH the same
        // allowlist the clipboard flavor uses ({http,https,mailto,tel}, C0
        // control chars rejected, scheme-less rejected). An earlier version
        // returned the href RAW while only the clipboard sanitized — so a
        // crafted `javascript:` target, or an External rel carrying a relative
        // target that `window.open` resolves against the HOST's origin, reached
        // the consumer unfiltered. `null` (no link) is the correct answer for a
        // target we will not navigate to: a rejected link is not a link.
        if (target.startsWith("url:")) {
          const href = sanitizeHref(target.slice(4));
          return href ? { kind: "url", href } : null;
        }

        // Internal link: "anchor:<bookmark>" → resolve the bookmark to {page,y}
        // HERE (single place for link-decision logic — the consumer no longer
        // round-trips bookmarkPosition). A dead bookmark (null) is a no-op: we
        // return null rather than an anchor with no target.
        if (target.startsWith("anchor:")) {
          const name = target.slice(7);
          const pos = await viewer.bookmarkPosition(name);
          if (!pos) return null;
          return { kind: "anchor", name, page: pos.page, y: pos.y };
        }

        // Unknown target prefix → no decision.
        return null;
      }),
      );
    },

    destroy(_ctx: KernelContext) {
      for (const u of unsubs.splice(0)) u();
    },
  };
}
