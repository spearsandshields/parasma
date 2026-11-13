import { defineConfig } from "tsdown";

// Library build, mirroring the sibling packages: ESM, dts, publint-verified.
// `react` is a peer — never bundled; @parasma/* are package deps → externalized.
export default defineConfig({
  entry: { index: "src/index.ts" },
  format: "esm",
  dts: true,
  clean: true,
  publint: true,
  outExtensions: () => ({ js: ".js", dts: ".d.ts" }),
  // The bundle drops module-level directives, so the client boundary for
  // React Server Components frameworks (Next.js App Router) is added here:
  // the hook and the component both need the browser.
  outputOptions: { banner: '"use client";' },
});
