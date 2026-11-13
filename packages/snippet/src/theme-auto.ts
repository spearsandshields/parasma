/**
 * theme-auto — the live `theme: "auto"` tracker.
 *
 * "Auto" means: mirror the HOST's declared theme, falling back to the system
 * preference — and keep mirroring as either changes. The host's declaration is
 * the `data-theme` attribute on the nearest shadow host (when the viewer lives
 * inside a consumer's custom element) or on `document.documentElement`; an app
 * that themes itself that way gets a viewer that follows along with ZERO
 * consumer wiring. Absent/foreign attribute values mean "the host has no
 * opinion" → `prefers-color-scheme` decides, live (the media listener fires on
 * OS-level flips).
 *
 * One shared implementation for both composition entries: `mountViewer`
 * (default chrome) and `createHeadlessViewer` (BYO chrome — where consumers
 * previously hand-rolled this exact mirror: source discovery, explicit-value
 * check, system fallback, MutationObserver, teardown).
 */
import type { RootScope } from "./root-scope";
import type { Theme } from "./types";

/** The element whose `data-theme` attribute speaks for the host: the shadow
 *  host when the viewer lives in a shadow tree (scope.rootNode already made
 *  that call once, realm-correctly), else the document element. */
function themeSource(scope: RootScope): Element {
  const rn = scope.rootNode;
  // Realm-correct instanceof: the constructor from the scope's own window (an
  // iframe's ShadowRoot is a different function object than the top realm's).
  const ShadowRootCtor = (scope.win as Window & { ShadowRoot: typeof ShadowRoot }).ShadowRoot;
  return rn instanceof ShadowRootCtor ? (rn as ShadowRoot).host : scope.doc.documentElement;
}

function resolve(scope: RootScope, source: Element): Theme {
  const explicit = source.getAttribute("data-theme");
  if (explicit === "dark" || explicit === "light") return explicit;
  return scope.win.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

/**
 * Start mirroring; returns a disposer. `apply` receives every resolved theme
 * (including the initial one, synchronously) — the caller sets `data-theme`
 * on its wrapper and emits `themeChange`, so emission policy stays with the
 * composition entry, not here.
 */
export function startThemeAuto(
  scope: RootScope,
  apply: (theme: Theme) => void,
): () => void {
  const source = themeSource(scope);
  let last: Theme | null = null;
  const sync = (): void => {
    const t = resolve(scope, source);
    if (t === last) return; // attribute churn that resolves identically = no-op
    last = t;
    apply(t);
  };
  sync();

  const MO = (scope.win as Window & { MutationObserver: typeof MutationObserver }).MutationObserver;
  const observer = new MO(sync);
  observer.observe(source, { attributes: true, attributeFilter: ["data-theme"] });

  // The system half: only decisive while the host has no explicit value, but
  // listening unconditionally is harmless (sync() re-resolves; explicit wins).
  const mq = scope.win.matchMedia?.("(prefers-color-scheme: dark)");
  mq?.addEventListener?.("change", sync);

  return () => {
    observer.disconnect();
    mq?.removeEventListener?.("change", sync);
  };
}
