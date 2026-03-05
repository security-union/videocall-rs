import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  globalSetup: "./global-setup.ts",
  testDir: "./tests",
  fullyParallel: false,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: [["html", { open: "never" }]],
  use: {
    baseURL: process.env.UI_URL || "http://localhost:80",
    ignoreHTTPSErrors: true,
  },
  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        launchOptions: {
          args: [
            "--ignore-certificate-errors",
            "--origin-to-force-quic-on=127.0.0.1:4433",
            "--use-fake-device-for-media-stream",
            "--use-fake-ui-for-media-stream",
            "--disable-gpu",
          ],
        },
      },
    },
  ],
});
