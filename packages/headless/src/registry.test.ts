import { describe, expect, it } from "vitest";
import { createKernel } from "./kernel";
import { asDynamic } from "./commands";
import { topoSort, type Plugin } from "./registry";
import type { KernelContext } from "./kernel-context";

/** Build a plugin that records its lifecycle calls into a shared log. */
function tracer(id: string, deps: string[], log: string[]): Plugin {
  return {
    id,
    deps,
    initialize: () => {
      log.push(`init:${id}`);
    },
    ready: () => {
      log.push(`ready:${id}`);
    },
    destroy: () => {
      log.push(`destroy:${id}`);
    },
  };
}

describe("topoSort", () => {
  it("orders deps before dependents", () => {
    const a: Plugin = { id: "a" };
    const b: Plugin = { id: "b", deps: ["a"] };
    const c: Plugin = { id: "c", deps: ["b"] };
    // Register out of order; topo must fix it.
    const order = topoSort([c, b, a]).map((p) => p.id);
    expect(order).toEqual(["a", "b", "c"]);
  });

  it("throws on an unknown dependency", () => {
    expect(() => topoSort([{ id: "x", deps: ["missing"] }])).toThrow(/unknown plugin: missing/);
  });

  it("throws on a dependency cycle", () => {
    const p: Plugin = { id: "p", deps: ["q"] };
    const q: Plugin = { id: "q", deps: ["p"] };
    expect(() => topoSort([p, q])).toThrow(/cycle/);
  });
});

describe("kernel two-phase boot", () => {
  it("initializes in dependency order, then readies AFTER all initialize", async () => {
    const log: string[] = [];
    const kernel = createKernel();
    // viewport depends on loader; register dependents first to prove topo.
    kernel.use(tracer("viewport", ["loader"], log));
    kernel.use(tracer("loader", [], log));
    await kernel.boot();

    // Phase 1 entirely before phase 2: all inits precede all readys.
    expect(log).toEqual([
      "init:loader",
      "init:viewport",
      "ready:loader",
      "ready:viewport",
    ]);
  });

  it("destroys in REVERSE boot order", async () => {
    const log: string[] = [];
    const kernel = createKernel();
    kernel.use(tracer("a", [], log));
    kernel.use(tracer("b", ["a"], log));
    await kernel.boot();
    log.length = 0; // ignore boot lines
    await kernel.destroy();
    expect(log).toEqual(["destroy:b", "destroy:a"]);
  });

  it("boot is idempotent and use() is rejected after boot", async () => {
    const log: string[] = [];
    const kernel = createKernel();
    kernel.use(tracer("a", [], log));
    await kernel.boot();
    await kernel.boot(); // no-op
    expect(log).toEqual(["init:a", "ready:a"]);
    expect(() => kernel.use(tracer("b", [], log))).toThrow(/after boot/);
  });

  it("ready() can observe commands a dependency registered in initialize()", async () => {
    // The whole point of two-phase boot: cross-plugin wiring is safe in ready()
    // because every plugin's initialize() already ran.
    let sawCommand = false;
    const provider: Plugin = {
      id: "provider",
      initialize: (ctx: KernelContext) => {
        // A test-only id not in CommandMap → the dynamic-id escape hatch.
        ctx.commands.register(asDynamic("provider.ping"), () => "pong");
      },
    };
    const consumer: Plugin = {
      id: "consumer",
      deps: ["provider"],
      ready: (ctx: KernelContext) => {
        sawCommand = ctx.commands.has("provider.ping");
      },
    };
    const kernel = createKernel();
    kernel.use(consumer).use(provider);
    await kernel.boot();
    expect(sawCommand).toBe(true);
    expect(await kernel.exec(asDynamic("provider.ping"))).toBe("pong");
  });
});
