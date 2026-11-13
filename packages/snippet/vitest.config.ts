// Separate from vite.config.ts deliberately: the app config sets root:"demo"
// (the demo IS the dev surface), which would make vitest scan demo/ only and
// find no tests. Unit tests live in src/.
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["src/**/*.test.ts"],
    environment: "node",
  },
});
