import { defineConfig } from "vitest/config";

/**
 * Top-level Vitest config for the `e2e/` package. Scopes test
 * discovery to the parent package's own unit tests: the bot CLI under
 * `bots-app/src/**` and the Playwright helper modules under `helpers/**`
 * (pure-function tests, e.g. `frame-liveness.test.ts`). The dashboard
 * subtree has its own self-contained Vitest setup (jsdom + React Testing
 * Library) under `bots-app/dashboard/` and pulls in dependencies the parent
 * package.json deliberately does NOT carry, so it stays excluded here.
 *
 * Run the dashboard's tests from inside `bots-app/dashboard/` with
 * `npm run test`.
 */
export default defineConfig({
  test: {
    include: ["bots-app/src/**/*.test.ts", "helpers/**/*.test.ts"],
    exclude: ["node_modules/**", "bots-app/dashboard/**"],
  },
});
