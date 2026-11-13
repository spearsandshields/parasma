import { defineConfig } from "vite";
import { localFontsDevPlugin } from "./vite-plugin-local-fonts";

export default defineConfig({
  root: "demo",
  // DEV-only: serve OS-installed fonts at /__local-fonts/ for gesture-free
  // Word-fidelity (automated browser runs + local dev). apply:"serve" keeps it out of builds.
  plugins: [localFontsDevPlugin()],
  server: { port: 5173, strictPort: true },
  // @parasma/core ships a .wasm fetched at runtime; let Vite serve it as-is
  // rather than pre-bundling the module graph.
  optimizeDeps: { exclude: ["@parasma/core"] },
  assetsInclude: ["**/*.wasm"],
});
