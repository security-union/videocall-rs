import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { enableDiagnosticsTileIndicators } from "../helpers/diagnostics-tile-indicators";
import { setTransportBadgeFlag } from "../helpers/transport-badge-config";
import { waitForServices } from "../helpers/wait-for-services";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";

const PIN_WEBTRANSPORT_INIT_SCRIPT = `(() => {
  try {
    localStorage.setItem("vc_transport_preference", "webtransport");
    localStorage.setItem("vc_transport_sticky", "true");
  } catch (_) {}
})();`;

async function createMeetingViaApi(
  hostEmail: string,
  hostName: string,
  meetingId: string,
): Promise<void> {
  const token = generateSessionToken(hostEmail, hostName);
  const res = await fetch(`${API_URL}/api/v1/meetings`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Cookie: `${COOKIE_NAME}=${token}`,
    },
    body: JSON.stringify({
      meeting_id: meetingId,
      attendees: [],
      allow_guests: false,
      waiting_room_enabled: true,
    }),
  });
  if (!res.ok) {
    throw new Error(`POST /api/v1/meetings failed (${res.status}): ${await res.text()}`);
  }
}

async function navigateToMeeting(page: Page, meetingId: string, name: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);
  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(name, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);
}

/** Races four real locators, so a missing element fails as a locator timeout. */
async function settleOnJoinScreen(
  page: Page,
): Promise<"in-meeting" | "waiting-room" | "waiting-for-meeting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.locator('[data-testid="meeting-waiting-room"]');
  const waitingForMeeting = page.locator('[data-testid="meeting-waiting-for-host"]');
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting-room" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting-room" || result === "waiting-for-meeting") {
    return result;
  }
  if (result === "auto-joined") {
    return "in-meeting";
  }
  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

/**
 * Regression for #2711: a WebTransport host also opens a candidate on the
 * websocket relay, and before the fix that candidate's expiry idled the
 * meeting. Tagged `@bvt1` because it is the only WebTransport-exercising spec
 * that runs per-PR; the others are all untagged.
 */
test.describe("WebTransport host does not strand later joiners (#2711)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a second participant reaches the waiting room while the host is on WebTransport @bvt1", async ({
    baseURL,
  }) => {
    // The declared locator budgets alone total 70s against the 60s default.
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_2711_wthost_${Date.now()}`;
    const hostEmail = "wt-host-2711@videocall.rs";
    const hostName = "WtHost2711";
    const joinerEmail = "later-joiner-2711@videocall.rs";
    const joinerName = "LaterJoiner2711";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId);

      const hostContext = await createAuthenticatedContext(browser, hostEmail, hostName, uiURL);
      await hostContext.addInitScript(PIN_WEBTRANSPORT_INIT_SCRIPT);
      // Both gates must be on before the first navigation.
      await setTransportBadgeFlag(hostContext, "true");
      await enableDiagnosticsTileIndicators(hostContext);
      const hostPage = await hostContext.newPage();
      await navigateToMeeting(hostPage, meetingId, hostName);

      const hostJoin = hostPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
      await hostJoin.waitFor({ timeout: 20_000 });
      await hostPage.waitForTimeout(1000);
      await hostJoin.click();
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 20_000 });

      // The stored preference cannot establish this: a failed QUIC handshake
      // falls back to WebSocket and leaves the seeded key untouched.
      await expect(
        hostPage.locator('.transport-badge[aria-label="Your connection transport: WebTransport"]'),
        "the host must have ELECTED WebTransport, not merely preferred it",
      ).toHaveCount(1, { timeout: 30_000 });

      // Past `RECONNECT_GRACE_PERIOD` (3s), so the losing candidate's expiry —
      // the moment the defect fires — precedes the second join.
      await hostPage.waitForTimeout(8000);

      const joinerContext = await createAuthenticatedContext(
        browser,
        joinerEmail,
        joinerName,
        uiURL,
      );
      const joinerPage = await joinerContext.newPage();
      await navigateToMeeting(joinerPage, meetingId, joinerName);

      const state = await settleOnJoinScreen(joinerPage);

      expect(
        state,
        "the meeting must still be active: a WebTransport host's losing WebSocket " +
          "candidate must not idle it (#2711)",
      ).not.toBe("waiting-for-meeting");
      expect(["waiting-room", "in-meeting"]).toContain(state);

      // Control: the host page never fell back to a waiting screen. Says
      // nothing about its transport — Admitted has no transport-drop exit.
      await expect(hostPage.locator("#grid-container")).toBeVisible();
    } finally {
      await browser.close();
    }
  });
});
