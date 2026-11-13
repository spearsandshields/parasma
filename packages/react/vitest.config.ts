import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // react-dom needs a document; the headless handle is a hand-built fake,
    // so nothing here touches the engine.
    environment: "happy-dom",
    include: ["src/**/*.test.ts"],
  },
});
