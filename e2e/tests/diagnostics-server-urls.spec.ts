import { test, expect, BrowserContext, Locator, Page } from "@playwright/test";
import { createAuthenticatedContext } from "../helpers/auth-context";
import { wakeActionBar } from "../helpers/controls";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { DRAWER } from "../helpers/rust-mirrored-constants";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Diagnostics drawer, Connection Manager section (#2867): the Active Connection
 * "Server:" value and every Servers card show the whole server URL as a link
 * (ws -> http, wss -> https, http(s) unchanged) that stays inside the drawer.
 */

const DEFAULT_UI_URL = "http://localhost:3001";
const LS_RIGHT_WIDTH = "vc_drawer_right_width";

// Chromium resolves every `*.localhost` name to loopback, so this reaches the
// same relay as `ws://localhost:8080` with a URL far wider than a 300px drawer.
const LONG_WS_HOST = "diagnostics-server-url-wrap-check.videocall-e2e-relay.localhost";
const LONG_WS_BASE = `ws://${LONG_WS_HOST}:8080`;

const NAVIGABLE_SCHEME: Record<string, string> = {
  ws: "http",
  wss: "https",
  http: "http",
  https: "https",
};

const escapeRe = (s: string): string => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

function expectedHref(text: string): string {
  const match = /^(wss?|https?):\/\//i.exec(text);
  expect(match, `"${text}" must be a ws://, wss://, http:// or https:// URL`).not.toBeNull();
  const [prefix, scheme] = match as RegExpExecArray;
  return `${NAVIGABLE_SCHEME[scheme.toLowerCase()]}://${text.slice(prefix.length)}`;
}

async function overrideWsUrl(context: BrowserContext, wsUrl: string): Promise<void> {
  const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},{"wsUrl":${JSON.stringify(
    wsUrl,
  )}});`;
  await context.route("**/config.js", async (route) => {
    const original = await (await route.fetch()).text();
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: original + injection,
    });
  });
  // The e2e stack's generated `config.local.js` runs after `config.js` and
  // re-sets `wsUrl`; elsewhere it is absent (404 or the SPA's HTML fallback).
  await context.route("**/config.local.js", async (route) => {
    const response = await route.fetch();
    const body = response.ok() ? (await response.text()).trim() : "";
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: (body.startsWith("<") ? "" : body) + injection,
    });
  });
}

async function joinSoloMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await fillAndSubmitJoinForm(page, meetingId, username);
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");
  const which = await waitForVisibleState(
    [
      { name: "join", locator: joinButton },
      { name: "grid", locator: grid },
    ],
    30_000,
  );
  if (which === "join") {
    await joinButton.click({ timeout: 10_000 }).catch(() => {});
  }
  await expect(grid).toBeVisible({ timeout: 20_000 });
}

async function openDiagnostics(page: Page): Promise<Locator> {
  await wakeActionBar(page);
  await page.locator("button#diagnostics-trigger").click({ timeout: 10_000 });
  const drawer = page.locator("#diagnostics-sidebar");
  await expect(drawer).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
  return drawer;
}

function connectionManager(drawer: Locator): Locator {
  return drawer.locator(
    'section[aria-labelledby="diag-h-connection-manager"] > .connection-manager-display',
  );
}

function serverCardUrls(drawer: Locator): Locator {
  return connectionManager(drawer).locator(
    ".servers-list > .servers-grid > .server-card > .server-details > .server-url",
  );
}

async function expectWholeLink(url: Locator, drawer: Locator, what: string): Promise<void> {
  await url.scrollIntoViewIfNeeded({ timeout: 10_000 });
  await expect(url, `${what} is not visible`).toBeVisible({ timeout: 10_000 });

  const el = await url.evaluate((node) => ({
    tag: node.tagName,
    text: node.textContent,
    href: node.getAttribute("href"),
    target: node.getAttribute("target"),
    rel: node.getAttribute("rel"),
    title: node.getAttribute("title"),
    scrollWidth: node.scrollWidth,
    clientWidth: node.clientWidth,
  }));
  expect(el.tag, `${what} must be a link, not a <${el.tag.toLowerCase()}>`).toBe("A");
  expect(el.text, `${what} text`).toMatch(/^(wss?|https?):\/\/\S+$/i);
  const text = el.text as string;
  const href = expectedHref(text);
  expect(el.href, `${what} href for ${text}`).toBe(href);
  expect(el.title, `${what} title`).toBe(`Open ${href} in a new tab`);
  expect(el.target, `${what} target`).toBe("_blank");
  expect(el.rel?.split(/\s+/), `${what} rel`).toEqual(
    expect.arrayContaining(["noopener", "noreferrer"]),
  );
  expect(el.clientWidth, `${what} has no width`).toBeGreaterThan(0);
  expect(el.scrollWidth, `${what} is clipped: ${text}`).toBeLessThanOrEqual(el.clientWidth);

  const box = await url.boundingBox();
  const outer = await drawer.boundingBox();
  if (!box || !outer) throw new Error(`${what}: missing bounding box`);
  expect(box.x, `${what} starts left of the drawer`).toBeGreaterThanOrEqual(outer.x - 0.5);
  expect(box.x + box.width, `${what} overflows the drawer's right edge`).toBeLessThanOrEqual(
    outer.x + outer.width + 0.5,
  );
  expect(box.y, `${what} starts above the drawer`).toBeGreaterThanOrEqual(outer.y - 0.5);
  expect(box.y + box.height, `${what} overflows the drawer's bottom edge`).toBeLessThanOrEqual(
    outer.y + outer.height + 0.5,
  );
}

/** Waits for ELECTED, then checks the active URL and every server card's URL. */
async function expectConnectionManagerUrls(page: Page, drawer: Locator): Promise<void> {
  const cm = connectionManager(drawer);
  await expect(cm.locator(".connection-status .status-value.status-elected")).toHaveText(
    "ELECTED",
    { timeout: 30_000 },
  );

  const activeUrl = cm
    .locator(".active-connection > .connection-details > .detail-item", {
      has: page.locator(".detail-label", { hasText: /^Server:$/ }),
    })
    .locator(".server-url");
  await expect(activeUrl).toHaveCount(1, { timeout: 30_000 });

  const configured = cm
    .locator(".connection-status .status-grid > .status-item", {
      has: page.locator(".status-label", { hasText: /^Configured Servers:$/ }),
    })
    .locator(".status-value");
  await expect(configured).toHaveText(/^[1-9]\d*$/, { timeout: 10_000 });
  const configuredCount = Number(await configured.textContent());

  const cardUrls = serverCardUrls(drawer);
  await expect(cardUrls, "one server card per configured server").toHaveCount(configuredCount, {
    timeout: 15_000,
  });

  await expectWholeLink(activeUrl, drawer, "Active Connection Server:");
  for (let i = 0; i < configuredCount; i++) {
    await expectWholeLink(cardUrls.nth(i), drawer, `server card ${i}`);
  }
}

test.describe("Diagnostics Connection Manager server URLs (#2867)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("the active and per-server URLs are whole links inside the drawer @bvt1", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const context = await createAuthenticatedContext(
      browser,
      "diag-server-urls@videocall.rs",
      "DiagUrls",
      baseURL || DEFAULT_UI_URL,
    );
    try {
      const page = await context.newPage();
      await joinSoloMeeting(page, `e2e_diag_server_urls_${Date.now()}`, "DiagUrls");
      const drawer = await openDiagnostics(page);
      await expectConnectionManagerUrls(page, drawer);
    } finally {
      await context.close();
    }
  });

  test("at the minimum drawer width a long server URL wraps instead of being clipped", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const context = await createAuthenticatedContext(
      browser,
      "diag-server-urls-narrow@videocall.rs",
      "DiagUrlsNarrow",
      baseURL || DEFAULT_UI_URL,
    );
    try {
      await overrideWsUrl(context, LONG_WS_BASE);
      await context.addInitScript(
        ({ key, width }) => {
          try {
            localStorage.setItem(key, width);
          } catch {
            /* opaque-origin about:blank */
          }
        },
        { key: LS_RIGHT_WIDTH, width: String(DRAWER.DRAWER_MIN_WIDTH) },
      );
      const page = await context.newPage();
      await joinSoloMeeting(page, `e2e_diag_server_urls_narrow_${Date.now()}`, "DiagUrlsNarrow");
      const drawer = await openDiagnostics(page);
      await expect(drawer).toHaveAttribute(
        "style",
        new RegExp(`width:\\s*${DRAWER.DRAWER_MIN_WIDTH}px`),
        { timeout: 10_000 },
      );

      await expectConnectionManagerUrls(page, drawer);

      const longCard = serverCardUrls(drawer).filter({ hasText: LONG_WS_HOST });
      await expect(longCard).toHaveCount(1, { timeout: 10_000 });
      await expect(longCard).toHaveText(new RegExp(`^${escapeRe(LONG_WS_BASE)}/`));
      await expect(longCard).toHaveAttribute(
        "href",
        new RegExp(`^http://${escapeRe(LONG_WS_HOST)}:8080/`),
      );
      const lineBoxes = await longCard.evaluate((node) => {
        const range = document.createRange();
        range.selectNodeContents(node);
        return range.getClientRects().length;
      });
      expect(lineBoxes, "the long URL must wrap onto several lines").toBeGreaterThan(1);
    } finally {
      await context.close();
    }
  });
});
