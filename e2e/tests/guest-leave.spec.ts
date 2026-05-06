import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";

// ---------------------------------------------------------------------------
// Helpers (same patterns as guest-join.spec.ts)
// ---------------------------------------------------------------------------

async function createMeetingViaApi(
  hostEmail: string,
  hostName: string,
  meetingId: string,
  opts: { allowGuests: boolean; waitingRoomEnabled?: boolean },
): Promise<string> {
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
      allow_guests: opts.allowGuests,
      waiting_room_enabled: opts.waitingRoomEnabled ?? true,
    }),
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`POST /api/v1/meetings failed (${res.status}): ${body}`);
  }
  const json = await res.json();
  return json.result.meeting_id as string;
}

async function hostStartsMeeting(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  hostEmail: string,
  hostName: string,
  meetingId: string,
  uiURL: string,
): Promise<{ hostPage: Page }> {
  const hostContext = await createAuthenticatedContext(browser, hostEmail, hostName, uiURL);
  const hostPage = await hostContext.newPage();

  await hostPage.goto("/");
  await hostPage.waitForTimeout(1500);

  await hostPage.locator("#meeting-id").click();
  await hostPage.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await hostPage.locator("#username").click();
  await hostPage.locator("#username").fill("");
  await hostPage.locator("#username").pressSequentially(hostName, { delay: 50 });
  await hostPage.waitForTimeout(500);
  await hostPage.locator("#username").press("Enter");
  await expect(hostPage).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });
  await hostPage.waitForTimeout(1500);

  const joinButton = hostPage.getByText(/Start Meeting|Join Meeting/);
  await joinButton.waitFor({ timeout: 20_000 });
  await hostPage.waitForTimeout(1000);
  await joinButton.click();
  await hostPage.waitForTimeout(3000);
  await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

  return { hostPage };
}

/**
 * Navigate a guest to the guest-join page, fill in their display name, and
 * drive them through to the in-meeting grid.  Returns the page once the grid
 * is visible.
 *
 * Works for both WR=off (auto-admitted) and WR=on (waits for admission via
 * the caller admitting separately).
 */
async function guestJoinsToGrid(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  meetingId: string,
  displayName: string,
  uiURL: string,
): Promise<Page> {
  const ctx = await browser.newContext({ baseURL: uiURL, ignoreHTTPSErrors: true });
  const page = await ctx.newPage();

  await page.goto(`/meeting/${meetingId}/guest`);
  await page.waitForTimeout(1500);

  await page.locator("#guest-name").click();
  await page.locator("#guest-name").pressSequentially(displayName, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#guest-name").press("Enter");

  const joinButton = page.getByText(/Join Meeting|Start Meeting/);
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 25_000 }).then(() => "join-button" as const),
    grid.waitFor({ timeout: 25_000 }).then(() => "grid" as const),
  ]);

  if (result === "join-button") {
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
  return page;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Guest leave flow", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Happy-path leave test:
   *   1. Host starts a meeting (WR disabled so the guest is auto-admitted).
   *   2. Guest joins via the /guest page and reaches the in-meeting grid.
   *   3. Peer tiles are exchanged over the signalling channel — host sees the
   *      guest's floating-name overlay in the grid.
   *   4. Guest clicks the Hang Up button.
   *   5. Guest's page navigates to the home page ("/").
   *   6. Host's grid no longer contains the guest's display-name tile within
   *      the reconnect-grace window (≤ 30 s).
   */
  test("admitted guest leaves, host sees their tile removed within grace period", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_leave_${Date.now()}`;
    const hostEmail = "host-guest-leave@videocall.rs";
    const hostName = "HostGuestLeave";
    const guestDisplayName = "GuestLeaver";

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      // Create meeting with WR=off so the guest is auto-admitted without
      // requiring an extra host-admit click.
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: false,
      });

      // ── Host enters the meeting ──────────────────────────────────────────
      const { hostPage } = await hostStartsMeeting(
        hostBrowser,
        hostEmail,
        hostName,
        meetingId,
        uiURL,
      );
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // ── Guest joins via /guest page and reaches the grid ─────────────────
      const guestPage = await guestJoinsToGrid(guestBrowser, meetingId, guestDisplayName, uiURL);

      // Allow a few seconds for the WebRTC peer announcement to propagate so
      // the host sees the guest's floating-name tile in the grid.
      await hostPage.waitForTimeout(4000);

      // Host must see the guest's display-name overlay before the leave.
      const guestTileOnHost = hostPage.locator(".floating-name", {
        hasText: guestDisplayName,
      });
      await expect(guestTileOnHost.first()).toBeVisible({ timeout: 20_000 });

      // ── Guest clicks Hang Up ─────────────────────────────────────────────
      // The HangUp button is rendered as `.video-control-button.danger` inside
      // the attendants toolbar.  It has no `title` attribute — the tooltip is
      // a CSS `::before` pseudo-element from the `.tooltip` child span.
      const hangUpButton = guestPage.locator("button.video-control-button.danger").first();
      await expect(hangUpButton).toBeVisible({ timeout: 10_000 });
      await hangUpButton.click();

      // ── Guest's page must navigate to the home page ──────────────────────
      await expect(guestPage).toHaveURL("/", { timeout: 15_000 });

      // ── Host must see the guest tile disappear within the grace period ────
      // The grace period is determined by WebRTC connection-state change events
      // propagating through the videocall-client peer-removal path.
      await expect(guestTileOnHost.first()).not.toBeVisible({ timeout: 30_000 });
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });
});
