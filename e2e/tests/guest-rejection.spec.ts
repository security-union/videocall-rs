import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async function createAuthenticatedContext(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  email: string,
  name: string,
  uiURL: string,
) {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
  });
  const token = generateSessionToken(email, name);
  const url = new URL(uiURL);
  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: token,
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: false,
      sameSite: "Lax",
    },
  ]);
  return context;
}

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

/** Have the host join via the UI home-page flow, activating the meeting. */
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
 * Fetch the participant status for a given user directly from the API.
 * Used to assert the DB-level rejection without relying solely on UI state.
 */
async function getParticipantStatus(
  hostEmail: string,
  hostName: string,
  meetingId: string,
  userId: string,
): Promise<string | null> {
  const token = generateSessionToken(hostEmail, hostName);
  const res = await fetch(`${API_URL}/api/v1/meetings/${meetingId}/participants`, {
    headers: { Cookie: `${COOKIE_NAME}=${token}` },
  });
  if (!res.ok) return null;
  const json = await res.json();
  const participants: Array<{ user_id: string; status: string }> = json?.result?.participants ?? [];
  return participants.find((p) => p.user_id === userId)?.status ?? null;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Guest rejection flow", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Happy-path rejection test:
   *   1. Host activates a meeting with WR=on and guests allowed.
   *   2. Guest navigates to the /guest page, fills in their name, and lands in
   *      the waiting room ("Waiting to be admitted").
   *   3. Host sees the guest in the host-controls panel and clicks Reject.
   *   4. Guest's page transitions to the rejection state, showing:
   *        - "Entry denied" heading.
   *        - "The meeting host has denied your request to join." body.
   *        - "Return to Home" button.
   *   5. The guest is NOT visible in the meeting grid on the host side.
   */
  test("host rejects waiting guest, guest sees Entry denied UI", async ({ baseURL }) => {
    test.setTimeout(120_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_wr_reject_ui_${Date.now()}`;
    const hostEmail = "host-wr-reject@videocall.rs";
    const hostName = "HostWRReject";
    const guestDisplayName = "RejectedGuest";

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: true,
      });

      // ── Host activates the meeting ────────────────────────────────────────
      const { hostPage } = await hostStartsMeeting(
        hostBrowser,
        hostEmail,
        hostName,
        meetingId,
        uiURL,
      );
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // ── Guest navigates to the /guest page and enters the WR ─────────────
      const guestCtx = await guestBrowser.newContext({ baseURL: uiURL, ignoreHTTPSErrors: true });
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      await guestPage.locator("#guest-name").click();
      await guestPage.locator("#guest-name").pressSequentially(guestDisplayName, { delay: 50 });
      await guestPage.waitForTimeout(500);
      await guestPage.locator("#guest-name").press("Enter");

      // Guest must show the waiting room message (user-visible state #1).
      await expect(guestPage.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 20_000,
      });

      // ── Host sees the guest in the waiting panel and clicks Reject ────────
      // The reject button is `button[title="Reject"]` rendered by the
      // HostControls component.
      const rejectButton = hostPage.getByTitle("Reject").first();
      await expect(rejectButton).toBeVisible({ timeout: 20_000 });
      await hostPage.waitForTimeout(1000);
      await rejectButton.dispatchEvent("click");
      await hostPage.waitForTimeout(3000);

      // ── Guest's page must show the rejection UI (user-visible state #2) ───
      // The GuestJoinPage renders a `rejected-container` div when the server
      // pushes a "participant_rejected" event via the observer WebSocket.
      await expect(guestPage.getByText("Entry denied")).toBeVisible({ timeout: 20_000 });
      await expect(
        guestPage.getByText("The meeting host has denied your request to join."),
      ).toBeVisible({ timeout: 5_000 });

      // The "Return to Home" button must be present so the guest can exit.
      await expect(guestPage.getByText("Return to Home")).toBeVisible({ timeout: 5_000 });

      // ── Rejected guest must NOT appear in the host's in-meeting grid ─────
      // No `.floating-name` tile matching the guest's display name should be
      // visible; the guest never received a room_token and was never connected
      // to the media bus.
      await expect(
        hostPage.locator(".floating-name", { hasText: guestDisplayName }),
      ).toHaveCount(0);
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });

  /**
   * API-level guard: the rejected guest's participant record must have
   * status="rejected" and must NOT be "admitted".
   *
   * This test drives the rejection via the API (not the UI Reject button)
   * so that the assertion is isolated from timing of WebSocket push delivery.
   * It verifies the backend enforces rejection and does not allow a rejected
   * user to re-appear as admitted.
   */
  test("rejected guest participant record has status=rejected, not admitted (API guard)", async () => {
    test.setTimeout(60_000);

    const meetingId = `e2e_wr_reject_api_${Date.now()}`;
    const hostEmail = "host-reject-api@videocall.rs";
    const hostName = "HostRejectAPI";

    // 1. Create the meeting.
    await createMeetingViaApi(hostEmail, hostName, meetingId, {
      allowGuests: true,
      waitingRoomEnabled: true,
    });

    // 2. Host joins (activates the meeting) via the API directly.
    const hostToken = generateSessionToken(hostEmail, hostName);
    const hostJoinRes = await fetch(`${API_URL}/api/v1/meetings/${meetingId}/join`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Cookie: `${COOKIE_NAME}=${hostToken}`,
      },
      body: JSON.stringify({ display_name: hostName }),
    });
    expect(hostJoinRes.ok, "host join should succeed").toBe(true);

    // 3. Guest joins via the guest endpoint (WR=on → ends up in "waiting").
    const guestJoinRes = await fetch(`${API_URL}/api/v1/meetings/${meetingId}/join-guest`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ display_name: "APIRejectedGuest" }),
    });
    expect(guestJoinRes.ok, "guest join should succeed").toBe(true);
    const guestJoinJson = await guestJoinRes.json();
    const guestUserId: string = guestJoinJson.result.user_id;
    expect(guestJoinJson.result.status, "guest should be in waiting state").toBe("waiting");

    // 4. Host rejects the guest via the API.
    const rejectRes = await fetch(`${API_URL}/api/v1/meetings/${meetingId}/reject`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Cookie: `${COOKIE_NAME}=${hostToken}`,
      },
      body: JSON.stringify({ user_id: guestUserId }),
    });
    expect(rejectRes.ok, "reject should succeed").toBe(true);

    // 5. Verify the participant record is "rejected", not "admitted".
    const participantStatus = await getParticipantStatus(
      hostEmail,
      hostName,
      meetingId,
      guestUserId,
    );
    expect(participantStatus, "rejected participant must have status=rejected").toBe("rejected");

    // 6. A second join attempt by the same guest session must land back in
    //    "waiting" (the server resets their row) rather than bypassing
    //    rejection entirely.  This guards against a status-reset bypass.
    const rejoinRes = await fetch(`${API_URL}/api/v1/meetings/${meetingId}/join-guest`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        display_name: "APIRejectedGuest",
        guest_session_id: guestUserId,
      }),
    });
    expect(rejoinRes.ok, "re-join after rejection should succeed").toBe(true);
    const rejoinJson = await rejoinRes.json();
    // After rejection, re-joining puts the guest back in "waiting" (not
    // "admitted") — the host must explicitly re-admit them.
    expect(
      ["waiting", "rejected"].includes(rejoinJson.result.status),
      `re-join after rejection must be 'waiting' or 'rejected', got '${rejoinJson.result.status}'`,
    ).toBe(true);
  });
});
