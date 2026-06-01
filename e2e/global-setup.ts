import { chromium } from "@playwright/test";
import { waitForServices } from "./helpers/wait-for-services";

const DIOXUS_UI_URL = process.env.DIOXUS_UI_URL || "http://localhost:3001";
const WARMUP_TIMEOUT_MS = 60_000;
const BACKOFF_DELAYS = [1000, 2000, 4000, 8000, 15000, 30000];

/**
 * Warm up the wasm app by loading it in a real browser and confirming
 * the Dioxus wasm has hydrated (renders the meeting-id input).
 *
 * This pre-warms the filesystem cache and triggers Chromium's wasm JIT
 * compilation before any real tests run. Without this, the first few
 * tests hit a CPU spike from concurrent wasm compilation + multiple
 * Chrome processes, causing "Target page, context or browser has been
 * closed" crashes.
 *
 * Retries with exponential backoff up to 60s total.
 */
async function warmupWasm(): Promise<void> {
  const browser = await chromium.launch({
    args: ["--ignore-certificate-errors", "--disable-gpu", "--disable-dev-shm-usage"],
  });

  const deadline = Date.now() + WARMUP_TIMEOUT_MS;
  let attempt = 0;

  try {
    while (Date.now() < deadline) {
      const context = await browser.newContext({ ignoreHTTPSErrors: true });
      const page = await context.newPage();

      try {
        await page.goto(DIOXUS_UI_URL, { timeout: 15_000 });
        await page.locator("#meeting-id").waitFor({ timeout: 10_000 });
        console.log(`Wasm warmup succeeded after ${attempt + 1} attempt(s)`);
        await context.close();
        return;
      } catch {
        await context.close();
        attempt++;
        const delay = BACKOFF_DELAYS[Math.min(attempt - 1, BACKOFF_DELAYS.length - 1)];
        if (Date.now() + delay >= deadline) {
          break;
        }
        console.log(`Wasm warmup attempt ${attempt} failed, retrying in ${delay}ms...`);
        await new Promise((r) => setTimeout(r, delay));
      }
    }
    throw new Error(
      `Wasm warmup failed after ${attempt} attempts (${WARMUP_TIMEOUT_MS / 1000}s deadline)`,
    );
  } finally {
    await browser.close();
  }
}

export default async function globalSetup() {
  await waitForServices();
  await warmupWasm();
}
