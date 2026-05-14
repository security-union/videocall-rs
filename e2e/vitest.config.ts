import { defineConfig } from "vitest/config";

/**
 * Top-level Vitest config for the `e2e/` package. Scopes test
 * discovery to `bots-app/src/**` — the dashboard subtree has its own
 * self-contained Vitest setup (jsdom + React Testing Library) under
 * `bots-app/dashboard/` and pulls in dependencies the parent
 * package.json deliberately does NOT carry.
 *
 * Run the dashboard's tests from inside `bots-app/dashboard/` with
 * `npm run test`.
 */
export default defineConfig({
  test: {
    include: ["bots-app/src/**/*.test.ts"],
    exclude: ["node_modules/**", "bots-app/dashboard/**"],
  },
});
