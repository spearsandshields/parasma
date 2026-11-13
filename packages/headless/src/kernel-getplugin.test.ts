/**
 * Typed getPlugin<T> escape hatch (officialized, not removed).
 *
 * getPlugin stays the documented ESCAPE HATCH; officializing it only improves its TYPE from
 * `Plugin`/`unknown` to a generic `<T>` with a backward-compatible default. The
 * runtime is unchanged: it returns the same booted plugin record.
 *
 * RED condition (documented): revert the generic to `getPlugin(id): Plugin` and
 * the `.greet()` access below fails to compile (Plugin has no `greet`), and the
 * default-parameter call loses its typed return. The generic is what makes the
 * cast-free typed read compile.
 */
import { describe, it, expect } from "vitest";
import { createKernel } from "./kernel";
import type { Plugin } from "./registry";

interface GreeterApi extends Plugin {
  greet(): string;
}

function greeterPlugin(): GreeterApi {
  return {
    id: "greeter",
    greet: () => "hello",
  };
}

describe("kernel.getPlugin<T>", () => {
  it("returns a typed T | undefined for a known id", async () => {
    const kernel = createKernel();
    kernel.use(greeterPlugin());
    await kernel.boot();

    // Typed read — no cast. `.greet()` is a typed member of GreeterApi.
    const g = kernel.getPlugin<GreeterApi>("greeter");
    expect(g?.greet()).toBe("hello");

    await kernel.destroy();
  });

  it("returns undefined for an unknown id (default T = Plugin)", async () => {
    const kernel = createKernel();
    await kernel.boot();
    const p = kernel.getPlugin("nope");
    expect(p).toBeUndefined();
    // Default T=Plugin keeps the backward-compatible shape for existing callers.
    const _id: string | undefined = p?.id;
    expect(_id).toBeUndefined();
    await kernel.destroy();
  });
});
