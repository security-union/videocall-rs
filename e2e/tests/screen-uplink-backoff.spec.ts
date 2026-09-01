import { test, expect, chromium, Page, BrowserContext, Browser } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";
import { routeDownlinkThroughProxy, impairUplink, healUplink } from "../helpers/downlink-impair";

/**
 * Screen-share uplink backoff (issue #2343): a shaped SENDER uplink must lower
 * the encoder's bit budget while its resolution stays pinned at native.
 *
 * The mock is 1920x1080 (2488 kbps; ladder 2488/1617/1051/683/500/500) and
 * repaints continuously: a smaller share would sit AT the 500 kbps floor and a
 * static one emits no deltas, either way vacuous.
 *
 * REQUIRES the `impair` profile: `make e2e-up-impair`.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

/** Native capture size of the mocked share. See the doc comment above. */
const SHARE_WIDTH = 1920;
const SHARE_HEIGHT = 1080;
/** `screen_bitrate_kbps_for(1920, 1080, 10)` — the healthy geometry baseline. */
const BASELINE_KBPS = 2488;
/** Step 3 of the ladder. A reduction must land at or below this. */
const STEP_3_KBPS = 683;

/** ~720 kbps: below the 2488 kbps baseline, above the 500 kbps floor. */
const UPLINK_CAP_KB = 90;

const ANIMATED_SHARE_MOCK = `
  (() => {
    const md = navigator.mediaDevices;
    if (!md) return;
    const makeStream = () => {
      const c = document.createElement('canvas');
      c.width = ${SHARE_WIDTH}; c.height = ${SHARE_HEIGHT};
      const ctx = c.getContext('2d');
      let n = 0;
      (function paint() {
        n++;
        for (let y = 0; y < ${SHARE_HEIGHT}; y += 16) {
          for (let x = 0; x < ${SHARE_WIDTH}; x += 16) {
            const h = (x * 131 + y * 197 + n * 29) ^ ((x >> 4) * (y >> 4) * 2654435761);
            const v = ((h >>> 3) % 251);
            const g = ((h >>> 11) % 251);
            const b = ((h >>> 19) % 251);
            ctx.fillStyle = 'rgb(' + v + ',' + g + ',' + b + ')';
            ctx.fillRect(x, y, 16, 16);
          }
        }
        ctx.fillStyle = '#fff'; ctx.font = '48px sans-serif';
        ctx.fillText('Mock Screen Share ' + n, 120, 240);
        requestAnimationFrame(paint);
      })();
      return c.captureStream(10);
    };
    Object.defineProperty(md, 'getDisplayMedia', {
      configurable: true, value: async () => makeStream(),
    });
  })();
`;

async function navigateToMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);
}

async function joinMeetingFromPage(page: Page): Promise<"in-meeting" | "waiting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") return "waiting";
  if (result === "join") {
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: "in-meeting" | "waiting",
): Promise<void> {
  if (guestResult !== "waiting") return;
  const admitButton = hostPage.getByTitle("Admit").first();
  await expect(admitButton).toBeVisible({ timeout: 20_000 });
  await hostPage.waitForTimeout(1000);
  await admitButton.dispatchEvent("click");
  await hostPage.waitForTimeout(3000);
  await joinMeetingFromPage(guestPage);
}

async function startScreenShare(sharerPage: Page, viewerPage: Page): Promise<boolean> {
  await wakeControls(sharerPage);
  await sharerPage.waitForTimeout(300);
  const shareButton = sharerPage.locator("button.video-control-button", {
    has: sharerPage.locator(".tooltip", { hasText: "Share Screen" }),
  });
  await expect(shareButton).toBeVisible({ timeout: 10_000 });
  await shareButton.click();
  try {
    await expect(viewerPage.locator(".split-screen-tile")).toBeVisible({ timeout: 20_000 });
    return true;
  } catch {
    return false;
  }
}

/** Open the diagnostics drawer and wait for the SEND screen meter to mount. */
async function openPerfDrawer(page: Page): Promise<void> {
  await wakeControls(page);
  const diagButton = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
  await expect(diagButton).toBeVisible({ timeout: 10_000 });
  await diagButton.click();
  const drawer = page.locator("#diagnostics-sidebar");
  await expect(drawer).toBeVisible({ timeout: 10_000 });
  await expect(drawer.locator('[data-testid="perf-vu-screen"]')).toBeVisible({ timeout: 10_000 });
}

/** Parse the SEND screen readout: `1920x1080·10fps·2488kbps[ · capped]`. */
async function readScreenSend(
  page: Page,
): Promise<{ width: number; height: number; kbps: number } | null> {
  const text = (await page.locator("#perf-vu-screen-readout").textContent()) || "";
  const m = text.match(/(\d+)x(\d+)\D+(\d+)fps\D+(\d+)kbps/);
  if (!m) return null;
  return { width: Number(m[1]), height: Number(m[2]), kbps: Number(m[4]) };
}

test.describe("Screen-share uplink backoff (#2343)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a shaped uplink lowers the screen bitrate, never the resolution @impair", async ({
    baseURL,
  }) => {
    test.setTimeout(600_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_ss_backoff_${Date.now()}`;

    let hostBrowser: Browser | undefined;
    let guestBrowser: Browser | undefined;

    try {
      hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
      guestBrowser = await chromium.launch({ args: BROWSER_ARGS });

      const hostCtx: BrowserContext = await createAuthenticatedContext(
        hostBrowser,
        "host-ssbackoff@videocall.rs",
        "SsBackoffHost",
        uiURL,
      );
      const guestCtx: BrowserContext = await createAuthenticatedContext(
        guestBrowser,
        "guest-ssbackoff@videocall.rs",
        "SsBackoffGuest",
        uiURL,
      );

      // Prejoin camera defaults OFF; seed both so the socket carries real load.
      await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      await guestCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      // The GUEST shares, so its uplink is the one shaped. Must precede nav.
      await routeDownlinkThroughProxy(guestCtx);
      await guestCtx.addInitScript(ANIMATED_SHARE_MOCK);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      const backoffLogs: string[] = [];
      guestPage.on("console", (msg) => {
        const t = msg.text();
        if (t.includes("ScreenEncoder: uplink backoff")) backoffLogs.push(t);
      });
      const observedStep = () =>
        backoffLogs.reduce((max, line) => {
          const m = line.match(/step=(\d+)/);
          return m ? Math.max(max, Number(m[1])) : max;
        }, -1);
      const latestStep = () => {
        const last = backoffLogs[backoffLogs.length - 1];
        const m = last?.match(/step=(\d+)/);
        return m ? Number(m[1]) : -1;
      };

      await navigateToMeeting(hostPage, meetingId, "SsBackoffHost");
      expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "SsBackoffGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });
      await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });
      await hostPage.waitForTimeout(3000);

      const shared = await startScreenShare(guestPage, hostPage);
      expect(
        shared,
        "Viewer never entered the screen-share split layout — the getDisplayMedia mock " +
          "may not have taken effect, or the peers never meshed.",
      ).toBe(true);

      await openPerfDrawer(guestPage);

      // Poll: a camera keyframe burst on the SHARED socket can blip the
      // governor before the link settles.
      await expect
        .poll(async () => JSON.stringify(await readScreenSend(guestPage)), { timeout: 60_000 })
        // The geometry baseline, never the tier ceiling (7740 kbps).
        .toBe(JSON.stringify({ width: SHARE_WIDTH, height: SHARE_HEIGHT, kbps: BASELINE_KBPS }));

      const logsBeforeImpair = backoffLogs.length;
      await impairUplink({ rateKb: UPLINK_CAP_KB });

      await expect
        .poll(() => backoffLogs.slice(logsBeforeImpair).some((l) => /step=1\b/.test(l)), {
          timeout: 120_000,
        })
        .toBe(true);
      await expect.poll(() => observedStep(), { timeout: 45_000 }).toBeGreaterThanOrEqual(3);

      await expect
        .poll(async () => (await readScreenSend(guestPage))?.kbps ?? BASELINE_KBPS, {
          timeout: 30_000,
        })
        .toBeLessThanOrEqual(STEP_3_KBPS);

      const reduced = await readScreenSend(guestPage);
      expect(reduced!.width).toBe(SHARE_WIDTH);
      expect(reduced!.height).toBe(SHARE_HEIGHT);

      // The receiver agrees. Its tile overlay is opt-in, so enable it first.
      await wakeControls(hostPage);
      const hostDiag = hostPage.locator("button", {
        has: hostPage.locator("span.tooltip", { hasText: "Open Diagnostics" }),
      });
      await hostDiag.click();
      const overlayToggle = hostPage.locator('[data-testid="media-metrics-overlay-toggle"]');
      await expect(overlayToggle).toBeVisible({ timeout: 10_000 });
      await overlayToggle.check();
      const screenOverlay = hostPage
        .locator(".split-screen-tile")
        .first()
        .locator('[data-testid="media-metrics-overlay-screen"]');
      await expect(screenOverlay).toBeVisible({ timeout: 20_000 });
      await expect
        .poll(
          async () =>
            new RegExp(`${SHARE_WIDTH}×${SHARE_HEIGHT}`).test(
              (await screenOverlay.textContent()) || "",
            ),
          { timeout: 30_000 },
        )
        .toBe(true);

      await healUplink();

      // Recovery is paced at 8s quiet + 8s per step: ~48s for five steps.
      await expect.poll(() => latestStep(), { timeout: 120_000 }).toBe(0);
      await expect
        .poll(async () => (await readScreenSend(guestPage))?.kbps ?? 0, { timeout: 60_000 })
        .toBe(BASELINE_KBPS);

      const recovered = await readScreenSend(guestPage);
      expect(recovered!.width).toBe(SHARE_WIDTH);
      expect(recovered!.height).toBe(SHARE_HEIGHT);
    } finally {
      await healUplink().catch(() => {});
      await hostBrowser?.close();
      await guestBrowser?.close();
    }
  });
});
