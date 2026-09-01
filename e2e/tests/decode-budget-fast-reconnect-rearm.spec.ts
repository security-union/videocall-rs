import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import {
  assertProxyUp,
  routeDownlinkThroughProxy,
  severWsTransport,
  restoreWsTransport,
} from "../helpers/downlink-impair";
import { BUDGET } from "../helpers/rust-mirrored-constants";

/**
 * The decode-budget cascade re-arms on a FAST reconnect (#2271). The cascade ALSO
 * re-arms from the `BudgetStep::Up` / growth arms, which would re-arm the UN-FIXED
 * build too, so the injected FPS is MILD (Down fires, recovery cannot) and DENSE,
 * and the witness below must have NO preceding `dir=growth`.
 *
 * Needs MOCK_PEERS_ENABLED + `make e2e-up-impair`, so UNTAGGED. WebSocket only.
 */

const USER_EMAIL = "budget-rearm@videocall.rs";
const USER_NAME = "BudgetRearm";

// Down is magnitude 1 here, keeping a margin above MIN_CAP where Down stops firing.
const MILD_FPS = 18;
const INJECT_INTERVAL_MS = 400;
const MOCK_PEERS = 12;
// Below this, no paused tile can exist while the cap is still above MIN_CAP.
const MIN_RENDERED_TILES = 4;

const SESSION_ASSIGNED_LOG = "Received SESSION_ASSIGNED: session_id=";
const FAILED_STATE_LOG = "Connection state changed: Failed";
const LOWER_LAYER_LOG = "DecodeBudget: cascade=lower_layer";
const GROWTH_LOG = "dir=growth";

function collectConsole(page: Page): string[] {
  const lines: string[] = [];
  page.on("console", (msg) => {
    lines.push(msg.text());
  });
  return lines;
}

const countLines = (lines: string[], needle: string): number =>
  lines.filter((l) => l.includes(needle)).length;

test.describe("Decode-budget cascade re-arm on fast reconnect (#2271)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a reconnect that never emits Failed re-arms the cascade", async ({ baseURL }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_budget_rearm_${Date.now()}`;

    await assertProxyUp();

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const context = await createAuthenticatedContext(browser, USER_EMAIL, USER_NAME, uiURL);
      await routeDownlinkThroughProxy(context);

      const page = await context.newPage();
      const consoleLines = collectConsole(page);

      await navigateToMeeting(page, meetingId);
      await joinMeeting(page);

      if (!(await setMockPeers(page, MOCK_PEERS))) {
        test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      }
      if (!(await hasInjectHook(page))) {
        test.skip(true, "__videocall_inject_render_fps absent; MOCK_PEERS_ENABLED is off");
      }

      // A viewport lid, NOT MOCK_PEERS (`natural` is the uncapped mock count).
      const rendered = await allTiles(page).count();
      expect(
        rendered,
        `only ${rendered} tiles render; too few to pause one above MIN_CAP`,
      ).toBeGreaterThanOrEqual(MIN_RENDERED_TILES);

      const reachedFloor = await injectUntil(
        page,
        120,
        async () => (await offBudgetTiles(page).count()) > 0,
      );
      expect(reachedFloor, "cascade never paused a tile under sustained mild pressure").toBe(true);

      const sessionsBefore = countLines(consoleLines, SESSION_ASSIGNED_LOG);
      const failedBefore = countLines(consoleLines, FAILED_STATE_LOG);
      // Baseline holds a latch-edge `lower_layer` line; slicing from here is what
      // makes the witness attributable to the reconnect.
      const severMark = consoleLines.length;

      await severWsTransport();
      await injectUntil(page, 1, async () => false);
      await restoreWsTransport();

      const reconnected = await injectUntil(
        page,
        150,
        async () => countLines(consoleLines, SESSION_ASSIGNED_LOG) > sessionsBefore,
      );
      expect(reconnected, "no new relay session was assigned after restoring the proxy").toBe(true);

      // Pins the PREMISE: never `Failed`, so `clear_all_peers` never ran.
      expect(countLines(consoleLines, FAILED_STATE_LOG)).toBe(failedBefore);
      expect(await allTiles(page).count()).toBe(rendered);
      expect(
        await decodedTiles(page).count(),
        "cap hit MIN_CAP before the reconnect completed; widen the descent margin",
      ).toBeGreaterThan(BUDGET.MIN_CAP);

      const reconnectIdx = consoleLines.findIndex(
        (l, i) => i >= severMark && l.includes(SESSION_ASSIGNED_LOG),
      );
      expect(reconnectIdx, "post-sever SESSION_ASSIGNED not located").toBeGreaterThanOrEqual(
        severMark,
      );
      const sawLowerLayer = await injectUntil(page, 30, async () =>
        consoleLines.slice(reconnectIdx).some((l) => l.includes(LOWER_LAYER_LOG)),
      );

      // Un-fixed, at-floor escalates straight to PauseTiles: no lower_layer ever.
      expect(sawLowerLayer, "no lower_layer step after the reconnect — cascade not re-armed").toBe(
        true,
      );

      const after = consoleLines.slice(reconnectIdx);
      const lowerAt = after.findIndex((l) => l.includes(LOWER_LAYER_LOG));
      const growthAt = after.findIndex((l) => l.includes(GROWTH_LOG));
      expect(
        growthAt === -1 || growthAt > lowerAt,
        "a recovery/growth step preceded the lower_layer line, so it does not attribute to #2271",
      ).toBe(true);

      await expect(offBudgetTiles(page)).toHaveCount(0);
    } finally {
      await restoreWsTransport().catch(() => {
        /* proxy already up / stack down — nothing to restore */
      });
      await browser.close();
    }
  });
});

const allTiles = (page: Page) => page.locator("#grid-container .grid-item");
const decodedTiles = (page: Page) =>
  page.locator('#grid-container .grid-item[data-off-budget="false"]');
const offBudgetTiles = (page: Page) => page.locator("#grid-container .grid-item.off-budget-tile");

const hasInjectHook = (page: Page) =>
  page.evaluate(
    () =>
      typeof (window as unknown as { __videocall_inject_render_fps?: unknown })
        .__videocall_inject_render_fps === "function",
  );

const injectFps = (page: Page) =>
  page.evaluate(
    (v) =>
      (
        window as unknown as { __videocall_inject_render_fps: (n: number) => void }
      ).__videocall_inject_render_fps(v),
    MILD_FPS,
  );

/** Inject one mild sample per `INJECT_INTERVAL_MS` until `done()` or `maxSamples`. */
async function injectUntil(
  page: Page,
  maxSamples: number,
  done: () => Promise<boolean>,
): Promise<boolean> {
  for (let i = 0; i < maxSamples; i++) {
    await injectFps(page);
    await page.waitForTimeout(INJECT_INTERVAL_MS);
    if (await done()) {
      return true;
    }
  }
  return false;
}

async function navigateToMeeting(page: Page, meetingId: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 60 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(USER_NAME, { delay: 60 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 15_000 });
}

async function joinMeeting(page: Page): Promise<void> {
  const joinButton = page.getByText(/Start Meeting|Join Meeting/);
  const grid = page.locator("#grid-container");
  await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).catch(() => undefined),
    grid.waitFor({ timeout: 30_000 }).catch(() => undefined),
  ]);
  if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
    await joinButton
      .first()
      .click()
      .catch(() => undefined);
  }
  await expect(grid).toBeVisible({ timeout: 20_000 });
}

/** Set the mock-peer count; false when MOCK_PEERS_ENABLED is off. */
async function setMockPeers(page: Page, count: number): Promise<boolean> {
  await page.locator(".video-controls-container").hover();
  const mockBtn = page
    .locator(".video-controls-container button")
    .filter({ has: page.locator('.tooltip:has-text("Mock Peers")') });
  if ((await mockBtn.count()) === 0) {
    return false;
  }
  await mockBtn.first().click();
  await expect(page.locator(".mock-peers-popover")).toBeVisible({ timeout: 5_000 });
  const input = page.locator("#mock-count-input");
  await input.fill(String(count));
  await input.dispatchEvent("input");
  await page.waitForTimeout(300);
  await page.locator("#grid-container").click({ position: { x: 10, y: 10 } });
  await expect(page.locator(".mock-peers-popover")).not.toBeVisible({ timeout: 3_000 });
  return true;
}
