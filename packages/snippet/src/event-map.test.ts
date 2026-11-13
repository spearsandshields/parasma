/**
 * SnippetEventMap — per-event payload typing for the BYO-chrome handle.
 * These assertions are enforced by BOTH tsc (the
 * snippet typecheck gate compiles src/**\/*.ts, including this file — the
 * `@ts-expect-error` lines FAIL the build if the flagged error disappears) and
 * by the vitest run (the runtime block proves emit→on delivers the payload
 * through the generic surface).
 *
 * RED conditions:
 *   - change `load: [DocumentInfo]` to `[unknown]` in the map → `info.documentKind`
 *     in `_typeLevelChecks` goes RED (unknown has no property) → tsc fails.
 *   - delete a `@ts-expect-error` and its line still errors → tsc fails.
 */
import { describe, it, expect, expectTypeOf } from "vitest";
import type { DocumentInfo } from "@parasma/core";
import type { HeadlessViewerHandle, SnippetEvent, SnippetEventMap, Theme } from "./types";

/**
 * TYPE-LEVEL assertions on the real handle surface. NEVER executed (the handle is
 * a `declare const`, undefined at runtime) — tsc checks the body; vitest never
 * calls it. Referencing `h.on`/`h.emit` here probes the exact generic argument
 * typing a BYO-chrome parent gets.
 */
// eslint-disable-next-line @typescript-eslint/no-unused-vars
function _typeLevelChecks(h: HeadlessViewerHandle): void {
  // on() gives each handler the exact payload type.
  h.on("zoomChange", (n) => expectTypeOf(n).toEqualTypeOf<number>());
  h.on("pageChange", (n) => expectTypeOf(n).toEqualTypeOf<number>());
  h.on("sheetChange", (n) => expectTypeOf(n).toEqualTypeOf<number>());
  h.on("themeChange", (t) => expectTypeOf(t).toEqualTypeOf<Theme>());
  h.on("error", (e) => expectTypeOf(e).toEqualTypeOf<unknown>());
  h.on("unload", (...args) => expectTypeOf(args).toEqualTypeOf<[]>());
  h.on("load", (info) => {
    expectTypeOf(info).toEqualTypeOf<DocumentInfo>();
    // Would be RED if load's payload were widened back to [unknown].
    const kind: DocumentInfo["documentKind"] = info.documentKind;
    void kind;
  });

  // BACKWARD-COMPAT: the documented loose consumer form still compiles — a
  // handler that ignores its args is valid for every event (contravariant:
  // every payload tuple ⊆ unknown[]).
  h.on("load", (...args: unknown[]) => void args);
  h.on("zoomChange", () => {});

  // emit() rejects a wrong payload at compile time.
  // @ts-expect-error themeChange expects a Theme, not an arbitrary string
  h.emit("themeChange", "nope");
  // @ts-expect-error pageChange expects a number, not a string
  h.emit("pageChange", "3");
  // @ts-expect-error load requires the DocumentInfo payload
  h.emit("load");
  // Valid emits still type-check.
  h.emit("themeChange", "dark");
  h.emit("pageChange", 2);
}
void _typeLevelChecks; // keep referenced (no dead-code prune concerns)

describe("SnippetEventMap (typed on/emit)", () => {
  it("keeps the payload tuple types in lockstep with the map", () => {
    expectTypeOf<SnippetEventMap["load"]>().toEqualTypeOf<[DocumentInfo]>();
    expectTypeOf<SnippetEventMap["unload"]>().toEqualTypeOf<[]>();
    expectTypeOf<SnippetEventMap["error"]>().toEqualTypeOf<[unknown]>();
    expectTypeOf<SnippetEventMap["pageChange"]>().toEqualTypeOf<[number]>();
    expectTypeOf<SnippetEventMap["sheetChange"]>().toEqualTypeOf<[number]>();
    expectTypeOf<SnippetEventMap["zoomChange"]>().toEqualTypeOf<[number]>();
    expectTypeOf<SnippetEventMap["themeChange"]>().toEqualTypeOf<[Theme]>();
    // SnippetEvent stays the exact key set.
    expectTypeOf<SnippetEvent>().toEqualTypeOf<keyof SnippetEventMap>();
    expect(true).toBe(true);
  });
});

/**
 * The RUNTIME emitter contract (mirrors headless-viewer.ts's `listeners` +
 * `on`/`emit` trio — the same isolate-the-seam approach fanout.test.ts uses; the
 * full boot is browser-verified). The generic `on` is faithful to the handle; the
 * internal `emit` mirrors the impl's `unknown[]` rest. Proves `on` DELIVERS the
 * payload through the `const fn = cb` storage-cast and that the returned
 * unsubscribe removes the SAME `fn`. TYPE-ONLY change → behavior unchanged.
 */
function makeEmitter() {
  const listeners: Record<SnippetEvent, Set<(...a: unknown[]) => void>> = {
    load: new Set(),
    unload: new Set(),
    error: new Set(),
    pageChange: new Set(),
    layoutTruncated: new Set(),
    sheetChange: new Set(),
    zoomChange: new Set(),
    themeChange: new Set(),
    linkActivate: new Set(),
    cellSelect: new Set(),
    commentFocus: new Set(),
    printProgress: new Set(),
  };
  function emit(event: SnippetEvent, ...args: unknown[]): void {
    for (const cb of listeners[event]) cb(...args);
  }
  function on<E extends SnippetEvent>(
    event: E,
    cb: (...args: SnippetEventMap[E]) => void,
  ): () => void {
    const fn = cb as (...a: unknown[]) => void;
    listeners[event].add(fn);
    return () => listeners[event].delete(fn);
  }
  return { on, emit };
}

describe("typed emitter delivery (runtime)", () => {
  it("emit('pageChange', 3) reaches an on('pageChange') handler with 3", () => {
    const { on, emit } = makeEmitter();
    const seen: number[] = [];
    on("pageChange", (n) => seen.push(n));
    emit("pageChange", 3);
    expect(seen).toEqual([3]);
  });

  it("the returned unsubscribe removes the same fn", () => {
    const { on, emit } = makeEmitter();
    const seen: number[] = [];
    const off = on("zoomChange", (n) => seen.push(n));
    emit("zoomChange", 1);
    off();
    emit("zoomChange", 2);
    expect(seen).toEqual([1]);
  });

  it("delivers the DocumentInfo payload for load", () => {
    const { on, emit } = makeEmitter();
    const info: DocumentInfo = { pageSize: { widthPx: 1, heightPx: 1 }, documentKind: "word" };
    let got: DocumentInfo | null = null;
    on("load", (i) => {
      got = i;
    });
    emit("load", info);
    expect(got).toBe(info);
  });
});
