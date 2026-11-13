import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // node env on purpose (the sibling posture): the composable must read all
    // DOM through the mounted element/scope, so hand-built fakes suffice and a
    // global-DOM shim can't mask a stray `window` read.
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
