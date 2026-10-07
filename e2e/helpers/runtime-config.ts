import type { BrowserContext, Route } from "@playwright/test";

// Patches BOTH `config.js` and the `config.local.js` shim served after it; call before the first navigation.
export async function setRuntimeConfig(
  context: BrowserContext,
  keys: Record<string, string>,
): Promise<void> {
  const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},${JSON.stringify(
    keys,
  )});`;
  const patch = async (route: Route) => {
    let original = "";
    try {
      const response = await route.fetch();
      if (response.status() === 200) {
        const body = await response.text();
        if (body.trim() && body.trim().charAt(0) !== "<") {
          original = body;
        }
      }
    } catch {
      /* layer absent on this serve — emit the override alone */
    }
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: `window.__APP_CONFIG=window.__APP_CONFIG||{};${original}${injection}`,
    });
  };
  await context.route("**/config.js", patch);
  await context.route("**/config.local.js", patch);
}
