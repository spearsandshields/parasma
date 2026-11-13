import { describe, it, expect } from "vitest";
import { startThemeAuto } from "./theme-auto";
import type { RootScope } from "./root-scope";

/**
 * Node-env unit tests over hand-built fakes (the root-scope.test.ts pattern):
 * theme-auto must read everything through the scope, so a fake scope drives
 * the full contract — source discovery (shadow host vs documentElement),
 * explicit-beats-system, live re-resolution, dedupe, and teardown.
 */

class FakeShadowRoot {
  constructor(public host: FakeSourceEl) {}
}

class FakeSourceEl {
  attrs = new Map<string, string>();
  getAttribute(k: string): string | null {
    return this.attrs.get(k) ?? null;
  }
  set(k: string, v: string | null): void {
    if (v === null) this.attrs.delete(k);
    else this.attrs.set(k, v);
  }
}

function fakeScope(opts: { shadowHost?: FakeSourceEl; prefersDark?: boolean }) {
  const docEl = new FakeSourceEl();
  const observers: Array<{ target: unknown; cb: () => void; disconnected: boolean }> = [];
  const mediaListeners: Array<() => void> = [];
  const media = {
    matches: opts.prefersDark ?? false,
    addEventListener: (_: string, cb: () => void) => mediaListeners.push(cb),
    removeEventListener: (_: string, cb: () => void) => {
      const i = mediaListeners.indexOf(cb);
      if (i >= 0) mediaListeners.splice(i, 1);
    },
  };
  class FakeMutationObserver {
    entry: { target: unknown; cb: () => void; disconnected: boolean };
    constructor(cb: () => void) {
      this.entry = { target: null, cb, disconnected: false };
      observers.push(this.entry);
    }
    observe(target: unknown): void {
      this.entry.target = target;
    }
    disconnect(): void {
      this.entry.disconnected = true;
    }
  }
  const scope = {
    rootNode: opts.shadowHost ? new FakeShadowRoot(opts.shadowHost) : ({ tag: "document" } as unknown),
    win: {
      ShadowRoot: FakeShadowRoot,
      MutationObserver: FakeMutationObserver,
      matchMedia: () => media,
    },
    doc: { documentElement: docEl },
  } as unknown as RootScope;
  return { scope, docEl, observers, mediaListeners, media };
}

describe("theme-auto", () => {
  it("mirrors the SHADOW HOST's explicit data-theme and follows live flips", () => {
    const host = new FakeSourceEl();
    host.set("data-theme", "dark");
    const { scope, observers } = fakeScope({ shadowHost: host, prefersDark: false });
    const applied: string[] = [];
    startThemeAuto(scope, (t) => applied.push(t));
    expect(applied).toEqual(["dark"]); // explicit beats the light system pref
    expect(observers[0]?.target).toBe(host); // observing the HOST, not documentElement
    host.set("data-theme", "light");
    observers[0].cb();
    expect(applied).toEqual(["dark", "light"]);
  });

  it("falls back to the system preference when the host has no opinion — and tracks media changes", () => {
    const { scope, docEl, observers, mediaListeners, media } = fakeScope({ prefersDark: true });
    const applied: string[] = [];
    startThemeAuto(scope, (t) => applied.push(t));
    expect(applied).toEqual(["dark"]);
    expect(observers[0]?.target).toBe(docEl); // no shadow → documentElement speaks
    media.matches = false;
    mediaListeners.forEach((cb) => cb());
    expect(applied).toEqual(["dark", "light"]);
    // An explicit value arriving later WINS over the media state.
    docEl.set("data-theme", "dark");
    observers[0].cb();
    expect(applied).toEqual(["dark", "light", "dark"]);
  });

  it("dedupes attribute churn that resolves identically", () => {
    const { scope, docEl, observers } = fakeScope({ prefersDark: false });
    const applied: string[] = [];
    startThemeAuto(scope, (t) => applied.push(t));
    docEl.set("data-theme", "light"); // explicit light == resolved light
    observers[0].cb();
    expect(applied).toEqual(["light"]); // no second apply
  });

  it("the disposer disconnects the observer and the media listener", () => {
    const { scope, docEl, observers, mediaListeners } = fakeScope({ prefersDark: false });
    const applied: string[] = [];
    const stop = startThemeAuto(scope, (t) => applied.push(t));
    stop();
    expect(observers[0].disconnected).toBe(true);
    expect(mediaListeners.length).toBe(0);
    docEl.set("data-theme", "dark");
    // The platform contract: a disconnected observer never fires again. The
    // fake honors it, so no straggler apply is possible.
    if (!observers[0].disconnected) observers[0].cb();
    expect(applied).toEqual(["light"]);
  });
});
