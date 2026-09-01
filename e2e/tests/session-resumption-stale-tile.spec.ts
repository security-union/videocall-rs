import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { enterTwoUserMeeting } from "../helpers/two-user-meeting";
import { installMediaSocketRecorder, severMediaWebSocket } from "../helpers/media-socket-sever";

const HOST_EMAIL = "resume-host@videocall.rs";
const PEER_EMAIL = "resume-peer@videocall.rs";
const HOST_NAME = "HostUser";
const PEER_NAME = "GuestUser";

const PEER_TILE = '[id^="peer-video-"][id$="-div"]';

// Milliseconds, bracketed by the ~1s reconnect and the ~15s watchdog floor.
const STALE_TILE_BUDGET_MS = 8_000;

const JOIN_LEAVE_TOAST_RE = /joined the meeting|left the meeting/;

interface ToastWindow {
  __e2269_toasts?: string[];
}

async function startToastRecorder(page: Page): Promise<void> {
  await page.evaluate(() => {
    const seen: string[] = [];
    (window as unknown as ToastWindow).__e2269_toasts = seen;
    const capture = () => {
      document.querySelectorAll(".peer-toast .toast-action").forEach((el) => {
        const text = (el.textContent || "").trim();
        if (text && !seen.includes(text)) {
          seen.push(text);
        }
      });
    };
    capture();
    new MutationObserver(capture).observe(document.body, { childList: true, subtree: true });
  });
}

// `null`, not `[]`, when the recorder was never installed: defaulted to `[]`, an
// uninstalled recorder and a quiet reconnect would be indistinguishable.
async function recordedToasts(page: Page): Promise<string[] | null> {
  return page.evaluate(() => (window as unknown as ToastWindow).__e2269_toasts ?? null);
}

async function peerTileIds(page: Page): Promise<string[]> {
  return page.locator(PEER_TILE).evaluateAll((els) => els.map((el) => el.id));
}

test.describe("Session resumption across a transport reconnect (#2269)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("an in-grace reconnect retires the peer's stale tile and carries its display name", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_resume_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const peerBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(hostBrowser, HOST_EMAIL, HOST_NAME, uiURL);
      const peerCtx = await createAuthenticatedContext(peerBrowser, PEER_EMAIL, PEER_NAME, uiURL);
      // Camera defaults OFF in E2E; without this seed neither side publishes
      // video and every tile assertion below would be vacuous.
      await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      await peerCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      await installMediaSocketRecorder(peerCtx);

      const hostPage = await hostCtx.newPage();
      const peerPage = await peerCtx.newPage();
      const hostConsole: string[] = [];
      hostPage.on("console", (msg) => hostConsole.push(msg.text()));

      await enterTwoUserMeeting(hostPage, peerPage, meetingId);

      const hostTiles = hostPage.locator(PEER_TILE);
      await expect(hostTiles, "the host must hold exactly one remote tile").toHaveCount(1, {
        timeout: 30_000,
      });
      const [staleTileId] = await peerTileIds(hostPage);
      expect(staleTileId).toMatch(/^peer-video-\d+-div$/);

      const staleName = hostPage.locator(`#${staleTileId} .floating-name-text`);
      await expect(staleName).toBeVisible({ timeout: 15_000 });
      await expect(staleName, "the pre-drop tile must already show the peer's name").toHaveText(
        PEER_NAME,
      );

      // Let the peer's own join toast expire before watching for a new one.
      await expect(hostPage.locator(".peer-toast")).toHaveCount(0, { timeout: 20_000 });
      await startToastRecorder(hostPage);

      const sever = await severMediaWebSocket(peerPage);
      const severedAt = Date.now();
      expect(
        sever.severed,
        `no live media socket was closed (recorder saw ${sever.recorded} socket(s)) — ` +
          `everything below would assert against an undisturbed call`,
      ).toBeGreaterThanOrEqual(1);

      const staleTile = hostPage.locator(`#${staleTileId}`);
      const deadline = severedAt + STALE_TILE_BUDGET_MS;
      let staleGoneAfterMs: number | null = null;
      while (Date.now() < deadline && staleGoneAfterMs === null) {
        if ((await staleTile.count()) === 0) {
          staleGoneAfterMs = Date.now() - severedAt;
          break;
        }
        await hostPage.waitForTimeout(200);
      }

      await expect
        .poll(async () => (await peerTileIds(hostPage)).filter((id) => id !== staleTileId).length, {
          timeout: 45_000,
          intervals: [500],
          message: "the peer never re-registered under a new session id",
        })
        .toBeGreaterThanOrEqual(1);

      const resumedIds = (await peerTileIds(hostPage)).filter((id) => id !== staleTileId);
      expect(resumedIds, "the reconnect must produce exactly one new session").toHaveLength(1);
      const resumedTileId = resumedIds[0];

      const toasts = await recordedToasts(hostPage);
      expect(
        toasts,
        "the toast recorder never installed — its verdict means nothing",
      ).not.toBeNull();
      expect(
        (toasts ?? []).filter((t) => JOIN_LEAVE_TOAST_RE.test(t)),
        "an in-grace reconnect must be invisible as churn: no leave and no join toast",
      ).toEqual([]);

      expect(
        staleGoneAfterMs,
        `the superseded tile ${staleTileId} was still in the host's grid ` +
          `${STALE_TILE_BUDGET_MS}ms after the drop. Un-fixed, only the 3-miss / 5s ` +
          `heartbeat watchdog reaps it (~15s); the fix retires it on ` +
          `PARTICIPANT_SESSION_RESUMED.`,
      ).not.toBeNull();

      const resumedName = hostPage.locator(`#${resumedTileId} .floating-name-text`);
      await expect(resumedName).toBeVisible({ timeout: 15_000 });
      await expect(
        resumedName,
        "the resumed session's PARTICIPANT_JOINED is suppressed, so only the " +
          "carry-over in adopt_resumed_session can name this tile — un-fixed it " +
          "falls back to the raw user_id",
      ).toHaveText(PEER_NAME);
      expect(await resumedName.textContent()).not.toContain("@");

      expect(
        hostConsole.some((line) => line.includes("Session resumed for")),
        "expected the client's PARTICIPANT_SESSION_RESUMED log line on the host",
      ).toBe(true);
    } finally {
      await hostBrowser.close();
      await peerBrowser.close();
    }
  });
});
