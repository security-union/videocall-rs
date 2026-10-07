import { test, expect, Browser, BrowserContext, Page } from "@playwright/test";
import { createAuthenticatedContext, pinWebSocketTransport } from "../helpers/auth-context";
import { wakeControls } from "../helpers/controls";
import { enableDiagnosticsTileIndicators } from "../helpers/diagnostics-tile-indicators";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { setRuntimeConfig } from "../helpers/runtime-config";
import { clickJoinAndEnterGrid } from "../helpers/screen-share-meeting";
import { waitForServices } from "../helpers/wait-for-services";

// Copied from dioxus-ui/src/components/transport_fallback.rs and the Network
// section of device_settings_modal.rs.
const TOAST_TITLE = "Using WebSocket";
const TOAST_DETAIL = "WebTransport, your preferred protocol, isn't working well here.";
const FALLBACK_TOAST_MS = 8_000;
const FALLBACK_NOTE =
  "This call is using WebSocket because WebTransport isn't working well on this device or network.";
const NOTE_TITLE = "Not using your preferred protocol";

const UNREACHABLE_WT_HOST = "https://127.0.0.1:4499";
const NEGATIVE_SETTLE_MS = 5_000;

const SEL = {
  toast: '[data-testid="transport-fallback-toast"]',
  status: '[data-testid="transport-fallback-status"]',
  networkPanel: "#settings-panel-network",
  note: '#settings-panel-network [data-testid="transport-in-use-mismatch"]',
  wtRadio: '[data-testid="transport-radio-webtransport"]',
  wsRadio: '[data-testid="transport-radio-websocket"]',
  selfBadgeWs: '.transport-badge[aria-label="Your connection transport: WebSocket"]',
  selfBadgeWt: '.transport-badge[aria-label="Your connection transport: WebTransport"]',
};

interface ToastRecord {
  shows: number;
  shownAt: number | null;
  goneAt: number | null;
}

// Installed before the app boots, so "never shown" covers the whole join and
// the shown-for duration is the app's own timer, not Playwright's polling.
const TOAST_RECORDER = `(() => {
  const rec = { shows: 0, shownAt: null, goneAt: null };
  window.__e2eFallbackToast = rec;
  let present = false;
  new MutationObserver(() => {
    const now = document.querySelector('${SEL.toast}') !== null;
    if (now && !present) {
      rec.shows += 1;
      if (rec.shownAt === null) rec.shownAt = performance.now();
    }
    if (!now && present && rec.goneAt === null) rec.goneAt = performance.now();
    present = now;
  }).observe(document, { childList: true, subtree: true });
})();`;

async function readToastRecord(page: Page): Promise<ToastRecord> {
  const rec = await page.evaluate(
    () => (window as unknown as { __e2eFallbackToast?: ToastRecord }).__e2eFallbackToast ?? null,
  );
  expect(rec, "the toast recorder must be installed").not.toBeNull();
  return rec as ToastRecord;
}

interface Joined {
  context: BrowserContext;
  page: Page;
  prefLine: string;
}

async function joinAlone(
  browser: Browser,
  baseURL: string,
  tag: string,
  config: Record<string, string>,
  opts: { pinWebSocket?: boolean } = {},
): Promise<Joined> {
  const name = `Fallback${tag}`;
  const unique = `${Date.now()}_${Math.random().toString(36).slice(2, 8)}`;
  const meetingId = `e2e_2895_${tag}_${unique}`;
  const context = await createAuthenticatedContext(
    browser,
    `fallback-${tag}-${unique}@videocall.rs`,
    name,
    baseURL,
  );
  const keys = { transportBadgeEnabled: "true", ...config };
  await setRuntimeConfig(context, keys);
  await enableDiagnosticsTileIndicators(context);
  if (opts.pinWebSocket) {
    await pinWebSocketTransport(context);
  }
  await context.addInitScript(TOAST_RECORDER);

  const page = await context.newPage();
  const prefLines: string[] = [];
  page.on("console", (msg) => {
    if (msg.text().includes("Transport preference applied:")) prefLines.push(msg.text());
  });

  await fillAndSubmitJoinForm(page, meetingId, name);
  await clickJoinAndEnterGrid(page);

  const applied = await page.evaluate((wanted) => {
    const cfg = (window as unknown as { __APP_CONFIG?: Record<string, unknown> }).__APP_CONFIG;
    return Object.fromEntries(wanted.map((key) => [key, cfg?.[key]]));
  }, Object.keys(keys));
  expect(applied, "the runtime config patch must reach the app").toEqual(keys);

  await expect.poll(() => prefLines.length, { timeout: 20_000 }).toBeGreaterThan(0);
  return { context, page, prefLine: prefLines[0] };
}

async function openNetworkSettings(page: Page): Promise<void> {
  await wakeControls(page);
  await page.locator('[data-testid="open-settings"]').click({ timeout: 10_000 });
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  await page.locator('[data-testid="settings-nav-network"]').click({ timeout: 10_000 });
  await expect(page.locator(SEL.networkPanel)).toBeVisible({ timeout: 10_000 });
}

// Call only after a positive proof the call connected (the self transport badge).
async function expectNoFallbackToast(page: Page): Promise<void> {
  const status = page.locator(SEL.status);
  await expect(status, "the fallback notice must be mounted").toHaveCount(1);
  await page.waitForTimeout(NEGATIVE_SETTLE_MS);
  await expect(page.locator("#grid-container"), "still in the call").toBeVisible();
  expect((await readToastRecord(page)).shows, "no fallback toast may appear").toBe(0);
  await expect(status).toHaveText("");
}

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

test.describe("WebTransport to WebSocket fallback notice (#2895)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a WebTransport preference that lands on WebSocket toasts once, then notes it in Network settings @bvt1", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const { context, page, prefLine } = await joinAlone(browser, baseURL!, "ws", {
      webTransportEnabled: "true",
      webTransportHost: UNREACHABLE_WT_HOST,
    });
    try {
      const toast = page.locator(SEL.toast);
      await expect(toast).toBeVisible({ timeout: 30_000 });
      await expect(toast.locator(".toast-name")).toHaveText(TOAST_TITLE);
      await expect(toast.locator(".toast-action")).toHaveText(TOAST_DETAIL);
      const status = page.locator(SEL.status);
      await expect(status).toHaveAttribute("role", "status");
      await expect(status).toHaveText(
        new RegExp(`^${escapeRegExp(`${TOAST_TITLE}. ${TOAST_DETAIL}`)}`),
      );

      expect(prefLine).toContain("pref=webtransport source=default");
      expect(prefLine).toMatch(/wt_urls=[1-9]/);
      await expect(page.locator(SEL.selfBadgeWs)).toHaveCount(1, { timeout: 15_000 });

      // The CSS fade leaves an opacity-0 node that Playwright still counts as
      // visible; the timer's dismissal removes it.
      await expect(toast).toHaveCount(0, { timeout: FALLBACK_TOAST_MS + 4_000 });
      const rec = await readToastRecord(page);
      expect(rec.shows).toBe(1);
      expect(rec.shownAt).not.toBeNull();
      expect(rec.goneAt).not.toBeNull();
      const shownFor = (rec.goneAt as number) - (rec.shownAt as number);
      expect(shownFor).toBeGreaterThan(FALLBACK_TOAST_MS - 1_000);
      expect(shownFor).toBeLessThan(FALLBACK_TOAST_MS + 3_000);
      await expect(status).toHaveText("");

      await openNetworkSettings(page);
      const note = page.locator(SEL.note);
      await expect(note).toBeVisible();
      await expect(note).toHaveAttribute("role", "note");
      await expect(note.locator(".settings-info-panel-title")).toHaveText(NOTE_TITLE);
      await expect(note.locator(".settings-info-panel-text")).toHaveText(FALLBACK_NOTE);
    } finally {
      await context.close();
    }
  });

  test("no notice when WebTransport is elected @bvt1", async ({ browser, baseURL }) => {
    test.setTimeout(120_000);
    const { context, page, prefLine } = await joinAlone(browser, baseURL!, "wt", {});
    try {
      expect(prefLine).toContain("pref=webtransport source=default");
      expect(prefLine).toMatch(/wt_urls=[1-9]/);
      await expect(
        page.locator(SEL.selfBadgeWt),
        "the call must have ELECTED WebTransport",
      ).toHaveCount(1, { timeout: 30_000 });
      await expectNoFallbackToast(page);

      await openNetworkSettings(page);
      await expect(page.locator(SEL.wtRadio)).toHaveAttribute("aria-checked", "true");
      await expect(page.locator(SEL.note)).toHaveCount(0);
    } finally {
      await context.close();
    }
  });

  test("no notice when the user prefers WebSocket", async ({ browser, baseURL }) => {
    test.setTimeout(120_000);
    const { context, page, prefLine } = await joinAlone(
      browser,
      baseURL!,
      "pinws",
      { webTransportEnabled: "true", webTransportHost: UNREACHABLE_WT_HOST },
      { pinWebSocket: true },
    );
    try {
      expect(prefLine).toContain("pref=websocket source=sticky");
      await expect(page.locator(SEL.selfBadgeWs)).toHaveCount(1, { timeout: 30_000 });
      await expectNoFallbackToast(page);

      await openNetworkSettings(page);
      await expect(page.locator(SEL.wsRadio)).toHaveAttribute("aria-checked", "true");
      await expect(page.locator(SEL.note)).toHaveCount(0);
    } finally {
      await context.close();
    }
  });

  test("no notice when the deployment disables WebTransport", async ({ browser, baseURL }) => {
    test.setTimeout(120_000);
    const { context, page, prefLine } = await joinAlone(browser, baseURL!, "wtoff", {
      webTransportEnabled: "false",
      webTransportHost: UNREACHABLE_WT_HOST,
    });
    try {
      expect(prefLine).toContain("pref=webtransport source=default");
      expect(prefLine).toContain("wt_urls=0");
      await expect(page.locator(SEL.selfBadgeWs)).toHaveCount(1, { timeout: 30_000 });
      await expectNoFallbackToast(page);

      await openNetworkSettings(page);
      await expect(page.locator(SEL.wtRadio)).toHaveText("WebTransport (unavailable)");
      await expect(page.locator(SEL.note)).toHaveCount(0);
    } finally {
      await context.close();
    }
  });
});
