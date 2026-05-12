import { defineConfig, devices } from "@playwright/test";

const CHROME_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
];

process.env.DISPLAY_NAME_RATE_LIMIT_DISABLED ??= "true";

export default defineConfig({
  globalSetup: "./global-setup.ts",
  testDir: "./tests",
  fullyParallel: false,
  workers: 2,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: [["html", { open: "never" }]],
  use: {
    ignoreHTTPSErrors: true,
  },
  projects: [
    {
      name: "dioxus",
      use: {
        baseURL: "http://localhost:3001",
        ...devices["Desktop Chrome"],
        launchOptions: { args: CHROME_ARGS },
      },
    },
  ],
});
