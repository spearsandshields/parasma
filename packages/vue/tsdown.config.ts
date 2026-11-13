import { defineConfig } from "tsdown";

// Library build, mirroring the sibling packages: ESM, dts, publint-verified.
// `vue` is a peer — never bundled; @parasma/* are package deps → externalized.
export default defineConfig({
  entry: { index: "src/index.ts" },
  format: "esm",
  dts: true,
  clean: true,
  publint: true,
  outExtensions: () => ({ js: ".js", dts: ".d.ts" }),
});
