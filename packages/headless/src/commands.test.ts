import { describe, expect, it } from "vitest";
import type { Viewer } from "@parasma/core";
import { asDynamic, createCommandBus } from "./commands";
import { createKernel } from "./kernel";
import {
  loaderPlugin,
  viewportPlugin,
  zoomPlugin,
  cursorPlugin,
  linksPlugin,
  sheetsPlugin,
  gridPlugin,
  commentsPlugin,
  searchPlugin,
  selectionPlugin,
  toolPlugin,
  clipboardPlugin,
  fontsPlugin,
} from "./index";

/**
 * A stub Viewer sufficient for a bare boot→destroy cycle. Booting drives the
 * loader actor to emit `loader:status-changed` ("idle"), which cascades a
 * `sheets:changed` reset into plugin-grid (→ viewer.gridExtent) etc. — so every
 * method must exist. A Proxy returns a no-op for every accessor; `on` returns an
 * unsubscribe thunk (loader + fonts push those onto their `unsubs`).
 */
function stubViewer(): Viewer {
  return new Proxy(
    {},
    {
      get(_t, prop) {
        if (prop === "on") return () => () => {};
        return () => undefined;
      },
    },
  ) as unknown as Viewer;
}

/** All 13 plugins, wired to the same stub Viewer (mirrors the snippet boot). */
function useAllPlugins(kernel: ReturnType<typeof createKernel>, v: Viewer): void {
  kernel
    .use(loaderPlugin(v))
    .use(viewportPlugin(v))
    .use(zoomPlugin())
    .use(cursorPlugin(v))
    .use(linksPlugin(v))
    .use(sheetsPlugin(v))
    .use(gridPlugin(v))
    .use(commentsPlugin(v))
    .use(searchPlugin(v))
    .use(selectionPlugin(v))
    .use(toolPlugin())
    .use(clipboardPlugin(v))
    .use(fontsPlugin(v));
}

describe("command bus — auto-unregister teardown", () => {
  it("register returns an unregister thunk that removes the command", () => {
    const bus = createCommandBus();
    const off = bus.register(asDynamic("x.test"), () => 1);
    expect(typeof off).toBe("function");
    expect(bus.has("x.test")).toBe(true);
    off();
    expect(bus.has("x.test")).toBe(false);
  });

  it("the returned thunk is idempotent (calling it twice is a no-op)", () => {
    const bus = createCommandBus();
    const off = bus.register(asDynamic("x.test"), () => 1);
    off();
    expect(() => off()).not.toThrow();
    expect(bus.has("x.test")).toBe(false);
  });

  it("a full 13-plugin boot → destroy → boot cycle on the SAME kernel does NOT throw 'already registered'", async () => {
    const v = stubViewer();

    // Re-boot the SAME kernel: destroy() clears `booted` but keeps the registry,
    // so the second boot re-registers every command onto the SAME command bus.
    // (A fresh createKernel() allocates a NEW command bus and could never
    // collide — which made the earlier version of this test vacuous.) If any
    // plugin's destroy failed to unregister, the second boot's register() throws
    // "already registered".
    const kernel = createKernel();
    useAllPlugins(kernel, v);
    await kernel.boot();
    await kernel.destroy();
    await expect(kernel.boot()).resolves.toBeUndefined();
    await kernel.destroy();
  });
});
