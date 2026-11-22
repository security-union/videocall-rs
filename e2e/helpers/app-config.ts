import { BrowserContext } from "@playwright/test";

/**
 * Override fields of the UI's runtime config (`config.js`, which sets
 * `window.__APP_CONFIG`) for every page in `context`.
 *
 * The e2e stack writes `config.js` once at start-up (nix/dev-stack.nix
 * `e2eUiRun`). Some tests need a different value, such as a higher video
 * bitrate cap, without a second stack, so this rewrites the served file:
 * each `key: value` line for an overridden key gets the new value.
 */
export async function overrideAppConfig(
  context: BrowserContext,
  overrides: Record<string, number | string>,
): Promise<void> {
  await context.route("**/config.js", async (route) => {
    const response = await route.fetch();
    let body = await response.text();
    for (const [key, value] of Object.entries(overrides)) {
      const literal = typeof value === "number" ? String(value) : JSON.stringify(value);
      const pattern = new RegExp(`(\\b${key}\\s*:\\s*)("[^"]*"|[^,\\n}]+)`);
      if (!pattern.test(body)) {
        throw new Error(`config.js has no "${key}" to override`);
      }
      body = body.replace(pattern, `$1${literal}`);
    }
    await route.fulfill({
      status: response.status(),
      contentType: "application/javascript",
      body,
    });
  });
}
