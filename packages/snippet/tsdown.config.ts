import { defineConfig } from "tsdown";

// Library build (publishable dist/). The demo app is a separate `build:demo`
// (vite). @parasma/* and @fontsource-* are package deps → auto-externalized.
export default defineConfig({
  entry: {
    index: "src/index.ts",
    "headless-viewer": "src/headless-viewer.ts",
    "chrome/index": "src/chrome/index.ts",
  },
  format: "esm",
  dts: true,
  clean: true,
  publint: true,
  // package.json is `type: module`, so ESM output is plain `.js` / `.d.ts`.
  outExtensions: () => ({ js: ".js", dts: ".d.ts" }),
  // snippet.css is shipped as a copied runtime asset; keep the `import
  // "./snippet.css"` side-effect literal so it resolves to the sibling
  // dist/snippet.css at consumer build time (and stays importable as the
  // `@parasma/snippet/snippet.css` subpath export).
  deps: { neverBundle: [/\.css$/] },
  copy: [{ from: "src/snippet.css", to: "dist" }],
});
