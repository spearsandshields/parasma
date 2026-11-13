/**
 * contextMenu seam — the snippet side (the BYO-chrome context-menu hook).
 *
 * Runtime: `invokeMenuItem` is the adapter's pure dispatch helper — a consumer
 * item's `onSelect` wins (and runs INSIDE the menu click's user-activation
 * window, the same guarantee Copy's clipboard write relies on); a built-in item
 * without one id-dispatches ("copy" → the host onCopy); a throwing onSelect is
 * contained (warn, no rethrow) so the menu still dismisses.
 *
 * Type pins (event-map.test.ts style, enforced by the tsc gate): the seam is
 * threaded as `ContextMenuSeam | undefined` through BOTH public configs, and an
 * async seam (Promise return) is rejected at compile time — "sync only" is a
 * type-level contract, not just a runtime fallback.
 */
import { describe, it, expect, expectTypeOf, vi } from "vitest";
import type { ContextMenuItem, ContextMenuSeam } from "@parasma/headless";
import type { MountOptions, HeadlessViewerConfig } from "../types";
import { invokeMenuItem } from "./dom-context-menu-adapter";

// ── type pins (checked by tsc; never executed) ────────────────────────────────
// eslint-disable-next-line @typescript-eslint/no-unused-vars
function _typeLevelChecks(): void {
  expectTypeOf<MountOptions["contextMenu"]>().toEqualTypeOf<ContextMenuSeam | undefined>();
  expectTypeOf<HeadlessViewerConfig["contextMenu"]>().toEqualTypeOf<
    ContextMenuSeam | undefined
  >();
  // The seam is SYNC-ONLY: an async seam returns a Promise, not an item list.
  const bad: MountOptions = {
    // @ts-expect-error an async contextMenu seam (Promise return) must not compile
    contextMenu: async () => [],
  };
  void bad;
}
void _typeLevelChecks;

// ── runtime: invokeMenuItem dispatch ──────────────────────────────────────────
describe("invokeMenuItem", () => {
  const item = (over: Partial<ContextMenuItem>): ContextMenuItem => ({
    id: "x",
    label: "X",
    enabled: true,
    ...over,
  });

  it("a consumer onSelect wins over built-in id dispatch (even for id 'copy')", () => {
    const onSelect = vi.fn();
    const copy = vi.fn();
    invokeMenuItem(item({ id: "copy", onSelect }), { copy });
    expect(onSelect).toHaveBeenCalledTimes(1);
    expect(copy).not.toHaveBeenCalled();
  });

  it("id 'copy' without onSelect dispatches to the host copy action", () => {
    const copy = vi.fn();
    invokeMenuItem(item({ id: "copy" }), { copy });
    expect(copy).toHaveBeenCalledTimes(1);
  });

  it("an unknown id without onSelect is a no-op", () => {
    const copy = vi.fn();
    invokeMenuItem(item({ id: "host.mystery" }), { copy });
    expect(copy).not.toHaveBeenCalled();
  });

  it("a throwing onSelect is contained: one warn, no rethrow", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    try {
      const copy = vi.fn();
      expect(() =>
        invokeMenuItem(
          item({
            onSelect: () => {
              throw new Error("host bug");
            },
          }),
          { copy },
        ),
      ).not.toThrow();
      expect(copy).not.toHaveBeenCalled();
      expect(warn).toHaveBeenCalledTimes(1);
      expect(warn.mock.calls[0]![0]).toContain("[parasma-snippet]");
    } finally {
      warn.mockRestore();
    }
  });
});
