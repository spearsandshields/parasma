import { defineConfig } from "vite";

// The Vue demo page (scripts/build-demo-site.sh publishes it under /vue/). No SFCs in
// this package → no @vitejs/plugin-vue needed; the esm-bundler feature flags
// are defined explicitly instead.
export default defineConfig({
  root: "demo",
  assetsInclude: ["**/*.wasm"],
  define: {
    __VUE_OPTIONS_API__: "true",
    __VUE_PROD_DEVTOOLS__: "false",
    __VUE_PROD_HYDRATION_MISMATCH_DETAILS__: "false",
  },
  build: { outDir: "dist", emptyOutDir: true },
});
