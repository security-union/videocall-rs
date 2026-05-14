import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

/**
 * Vitest config for the dashboard. jsdom + @testing-library setup
 * lives in `src/test/setup.ts`. Self-contained — does NOT interfere
 * with the parent `e2e/` vitest config (which scopes to
 * `bots-app/**` under tsx, not React).
 */
export default defineConfig({
  plugins: [react()],
  test: {
    globals: true,
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
    css: false,
  },
});
