import { defineConfig } from "tsdown";

// Library build (publishable dist/). @parasma/core, xstate and @xstate/store are
// package deps → auto-externalized (not bundled).
export default defineConfig({
  entry: { index: "src/index.ts", internal: "src/internal.ts" },
  format: "esm",
  dts: true,
  clean: true,
  publint: true,
  // Preserve the source module structure in the output (one file per module,
  // extensions rewritten to `.js`). REQUIRED here: the plugins declaration-merge
  // into CommandMap/BusEventMap via `declare module "../commands"` / "../bus",
  // and those relative augmentations only resolve if the target modules survive
  // as sibling files. Bundling collapses them into one module and the
  // augmentations go dead in the published .d.ts (verified). core/snippet have
  // no such augmentations, so they stay bundled.
  unbundle: true,
  // package.json is `type: module`, so ESM output is plain `.js` / `.d.ts`.
  outExtensions: () => ({ js: ".js", dts: ".d.ts" }),
});
