import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

import pkg from "./package.json" with { type: "json" };

/**
 * Vitest config for the dashboard. jsdom + @testing-library setup
 * lives in `src/test/setup.ts`. Self-contained — does NOT interfere
 * with the parent `e2e/` vitest config (which scopes to
 * `bots-app/**` under tsx, not React).
 *
 * Mirrors the `__APP_VERSION__` define from `vite.config.ts` so
 * component tests see the same compile-time constant as the
 * production bundle.
 */
export default defineConfig({
  plugins: [react()],
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  test: {
    globals: true,
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
    css: false,
  },
});
