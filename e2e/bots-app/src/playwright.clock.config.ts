import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  testMatch: "clock-source.browser.spec.ts",
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 15_000,
  use: {
    ...devices["Desktop Chrome"],
    headless: true,
  },
});
