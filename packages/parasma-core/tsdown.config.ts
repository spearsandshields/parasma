import { existsSync, statSync } from "node:fs";
import { defineConfig } from "tsdown";

// Library build (publishable dist/). `bun run dev` still consumes src/*.ts via
// the workspace exports; this only runs at build/publish time.
export default defineConfig({
  entry: { index: "src/index.ts", worker: "src/worker.ts" },
  format: "esm",
  dts: true,
  clean: true,
  publint: true,
  // package.json is `type: module`, so ESM output is plain `.js` / `.d.ts`.
  outExtensions: () => ({ js: ".js", dts: ".d.ts" }),
  // The wasm-pack glue must NOT be bundled: it fetches parasma_wasm_bg.wasm via
  // `new URL(..., import.meta.url)`, which has to resolve relative to the copied
  // dist/wasm/ dir at consumer runtime. Keep the `./wasm/parasma_wasm.js` import
  // literal so it points at the sibling copy.
  deps: { neverBundle: [/[\\/]wasm[\\/]parasma_wasm\.js$/] },
  // Copy only the wasm-pack artifacts (skip the dir's own .gitignore/package.json)
  // into dist/wasm/, next to the bundle.
  copy: [{ from: "src/wasm/parasma_wasm*", to: "dist/wasm" }],
  // The 6MB binary is runtime-fetched (not type-referenced), so `dts` and
  // `publint` are structurally blind to it — a missing/truncated src/wasm binary
  // would otherwise pack a green-but-runtime-broken tarball. Assert it landed.
  onSuccess: () => {
    const bin = "dist/wasm/parasma_wasm_bg.wasm";
    if (!existsSync(bin) || statSync(bin).size < 100_000) {
      throw new Error(
        `[parasma-core] ${bin} is missing or truncated — run \`bun run build:wasm:full\` before building/packing.`,
      );
    }
  },
});
