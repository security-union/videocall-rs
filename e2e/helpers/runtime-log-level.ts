/** Pins `logLevel` on ONE BrowserContext, before its first navigation. Route interception, not
 *  `addInitScript`: `config.js` REASSIGNS `window.__APP_CONFIG` and `config.local.js` (#1883)
 *  assigns onto it afterwards. */

import { BrowserContext, Route } from "@playwright/test";

export const LOG_LEVEL_KEY = "logLevel";

export type RuntimeLogLevel = "off" | "error" | "warn" | "info" | "debug" | "trace";

/** Appending to an HTML body loses the injection — index.html drops a body starting with "<". */
async function jsShapedBody(route: Route): Promise<string> {
  const response = await route.fetch().catch(() => null);
  if (!response || !response.ok()) return "";
  const body = (await response.text()).trim();
  return body.startsWith("<") ? "" : body;
}

export async function setRuntimeLogLevel(
  context: BrowserContext,
  level: RuntimeLogLevel,
): Promise<void> {
  const entry = `${JSON.stringify(LOG_LEVEL_KEY)}:${JSON.stringify(level)}`;
  const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},{${entry}});`;

  await context.route("**/config.js", async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: (await jsShapedBody(route)) + injection,
    });
  });

  await context.route("**/config.local.js", async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: (await jsShapedBody(route)) + injection,
    });
  });
}
