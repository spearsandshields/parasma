/**
 * Type-level contract assertions for the typed command bus and the
 * augmentable event map. This is NOT a vitest file (no `.test.` in the
 * name) — it is compiled by the headless `tsc --noEmit` gate, and each
 * `@ts-expect-error` IS the assertion: a passing state has NO stray directive
 * errors (an unused `@ts-expect-error` is itself a compile error, so a probe that
 * stops erroring FAILS the gate). Exported + never called; it only needs to type.
 */
import { createKernel } from "./kernel";
import { asDynamic } from "./commands";
import type { ToolMode } from "./plugins/tool";

export async function __commandBusContracts(): Promise<void> {
  const kernel = createKernel();

  // (5) Unknown command id must NOT compile — a typo is a COMPILE error, not a
  // runtime throw. RED here = a permissive `(id: string, …)` overload crept back.
  // @ts-expect-error — "slection.begin" is not a CommandId (typo of selection.begin)
  await kernel.exec("slection.begin");

  // (6) Wrong ARG type must NOT compile.
  // @ts-expect-error — tool.setMode takes a ToolMode, not a number
  await kernel.exec("tool.setMode", 123);

  // (7) POSITIVE — a good typed call compiles AND infers the return (no `<T>`).
  const mode: ToolMode = await kernel.exec("tool.get");
  void mode;
  // A correct arg to a parametrised command also compiles.
  await kernel.exec("tool.setMode", "hand");

  // (9) The dynamic hatch is OPT-IN: a BRANDED id compiles (→ Promise<unknown>)…
  const dyn: unknown = await kernel.exec(asDynamic("anything.built.at.runtime"));
  void dyn;
  // …but a RAW string does NOT (so a typo can never launder through the hatch).
  const raw = "anything.built.at.runtime";
  // @ts-expect-error — a raw string is neither a CommandId nor a DynamicCommandId
  await kernel.exec(raw);

  // (8) Bus events: an unknown event must NOT compile…
  // @ts-expect-error — "zoom:changde" is not a BusEventMap key (typo of zoom:changed)
  kernel.bus.subscribe("zoom:changde", () => {});
  // …and a known event's payload is typed (a plugin-augmented key resolves here
  // only because the barrel re-exports every plugin ⇒ the augmentation is
  // program-wide).
  kernel.bus.subscribe("zoom:changed", (p) => {
    const scale: number = p.scale;
    void scale;
  });
}
