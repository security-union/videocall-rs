import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Create a meeting, returning the meeting_id. */
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

/** PATCH meeting settings (ownership check enforced by the API). */
async function patchMeetingSettings(
  hostEmail: string,
  hostName: string,
  meetingId: string,
  settings: {
    admitted_can_admit?: boolean;
    waiting_room_enabled?: boolean;
    end_on_host_leave?: boolean;
  },
): Promise<void> {
  const token = generateSessionToken(hostEmail, hostName);
  const res = await fetch(`${API_URL}/api/v1/meetings/${meetingId}`, {
    method: "PATCH",
    headers: {
      "Content-Type": "application/json",
      Cookie: `${COOKIE_NAME}=${token}`,
    },
    body: JSON.stringify(settings),
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`PATCH /api/v1/meetings/${meetingId} failed (${res.status}): ${body}`);
  }
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
 * Navigate a guest to the /guest page, enter their display name, submit the
 * form, and wait until the waiting room message is visible.
 */
async function guestEntersWaitingRoom(
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

  await expect(page.getByText("Waiting to be admitted")).toBeVisible({ timeout: 20_000 });
  return page;
}

/**
 * Drive a page from the post-admission state ("Join Meeting"/"Start Meeting"
 * button or direct grid) into the in-meeting grid.
 */
async function waitForGrid(page: Page): Promise<void> {
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Guest waiting room — multiple guests and admitted_can_admit", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Scenario: two guests queue in the waiting room and the host admits them
   * one at a time.  Each guest must land in the in-meeting grid after
   * admission; the other guest's waiting room UI must remain unaffected until
   * their own admission button is clicked.
   */
  test("multiple guests queue in waiting room, host admits each individually", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_wr_multi_${Date.now()}`;
    const hostEmail = "host-wr-multi@videocall.rs";
    const hostName = "HostWRMulti";
    const guestName1 = "GuestQueue1";
    const guestName2 = "GuestQueue2";

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser1 = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser2 = await chromium.launch({ args: BROWSER_ARGS });

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

      // ── Both guests navigate to the /guest page and land in the WR ───────
      const guestPage1 = await guestEntersWaitingRoom(guestBrowser1, meetingId, guestName1, uiURL);
      const guestPage2 = await guestEntersWaitingRoom(guestBrowser2, meetingId, guestName2, uiURL);

      // Both must show the waiting room message.
      await expect(guestPage1.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 5_000,
      });
      await expect(guestPage2.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 5_000,
      });

      // ── Host admits Guest 1 ───────────────────────────────────────────────
      // The host controls show one Admit button per waiting participant; click
      // the first visible one.
      const admitButton1 = hostPage.getByTitle("Admit").first();
      await expect(admitButton1).toBeVisible({ timeout: 20_000 });
      await hostPage.waitForTimeout(1000);
      await admitButton1.dispatchEvent("click");
      await hostPage.waitForTimeout(3000);

      // Guest 1 transitions from the waiting room into the meeting grid.
      await waitForGrid(guestPage1);
      // Guest 1 must be in the grid.
      await expect(guestPage1.locator("#grid-container")).toBeVisible({ timeout: 5_000 });

      // Guest 2 is still waiting.
      await expect(guestPage2.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 5_000,
      });

      // ── Host admits Guest 2 ───────────────────────────────────────────────
      const admitButton2 = hostPage.getByTitle("Admit").first();
      await expect(admitButton2).toBeVisible({ timeout: 20_000 });
      await hostPage.waitForTimeout(1000);
      await admitButton2.dispatchEvent("click");
      await hostPage.waitForTimeout(3000);

      // Guest 2 transitions into the meeting grid.
      await waitForGrid(guestPage2);
      await expect(guestPage2.locator("#grid-container")).toBeVisible({ timeout: 5_000 });
    } finally {
      await hostBrowser.close();
      await guestBrowser1.close();
      await guestBrowser2.close();
    }
  });

  /**
   * Scenario: admitted_can_admit=true path.
   *
   * When the host pre-configures the meeting with `admitted_can_admit=true`,
   * an already-admitted authenticated participant (non-host) sees the Admit
   * button in their host-controls panel and can use it to let in a waiting
   * guest — without requiring any action from the host.
   *
   * Steps:
   *   1. Create meeting with WR=on, allow_guests=true.
   *   2. PATCH to enable admitted_can_admit BEFORE the meeting starts.
   *   3. Host starts meeting.
   *   4. Authenticated non-host joins → enters WR → host admits them.
   *   5. Non-host is now in the grid with admit authority.
   *   6. A guest joins → enters WR.
   *   7. Non-host sees the Admit button and admits the guest.
   *   8. Guest lands in the grid.
   */
  test("admitted_can_admit=true: admitted non-host user can admit a queued guest", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_wr_aca_${Date.now()}`;
    const hostEmail = "host-wr-aca@videocall.rs";
    const hostName = "HostWRACA";
    const nonHostEmail = "nonhost-wr-aca@videocall.rs";
    const nonHostName = "NonHostACA";
    const guestDisplayName = "GuestACA";

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const nonHostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      // Create the meeting with WR=on.
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: true,
      });

      // Pre-configure admitted_can_admit=true via PATCH so that once the
      // non-host is admitted they immediately have admit authority without
      // needing a live settings toggle.
      await patchMeetingSettings(hostEmail, hostName, meetingId, {
        admitted_can_admit: true,
      });

      // ── Host starts the meeting ───────────────────────────────────────────
      const { hostPage } = await hostStartsMeeting(
        hostBrowser,
        hostEmail,
        hostName,
        meetingId,
        uiURL,
      );
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // ── Non-host joins → WR → host admits them ────────────────────────────
      const nonHostContext = await createAuthenticatedContext(
        nonHostBrowser,
        nonHostEmail,
        nonHostName,
        uiURL,
      );
      const nonHostPage = await nonHostContext.newPage();
      await nonHostPage.goto("/");
      await nonHostPage.waitForTimeout(1500);
      await nonHostPage.locator("#meeting-id").click();
      await nonHostPage.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
      await nonHostPage.locator("#username").click();
      await nonHostPage.locator("#username").fill("");
      await nonHostPage.locator("#username").pressSequentially(nonHostName, { delay: 50 });
      await nonHostPage.waitForTimeout(500);
      await nonHostPage.locator("#username").press("Enter");
      await expect(nonHostPage).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
        timeout: 10_000,
      });

      // Non-host enters WR (WR=on); host admits them.
      await expect(nonHostPage.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 20_000,
      });
      const admitNonHost = hostPage.getByTitle("Admit").first();
      await expect(admitNonHost).toBeVisible({ timeout: 20_000 });
      await admitNonHost.dispatchEvent("click");
      await hostPage.waitForTimeout(3000);

      // Non-host transitions to the meeting grid.
      await waitForGrid(nonHostPage);
      await expect(nonHostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // ── Guest joins and enters the WR ─────────────────────────────────────
      const guestCtx = await guestBrowser.newContext({ baseURL: uiURL, ignoreHTTPSErrors: true });
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);
      await guestPage.locator("#guest-name").click();
      await guestPage.locator("#guest-name").pressSequentially(guestDisplayName, { delay: 50 });
      await guestPage.waitForTimeout(500);
      await guestPage.locator("#guest-name").press("Enter");
      await expect(guestPage.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 20_000,
      });

      // ── Non-host (with admitted_can_admit=true) admits the guest ──────────
      // The Admit button must be visible on the non-host's page because
      // admitted_can_admit was pre-configured.
      const nonHostAdmitButton = nonHostPage
        .locator('button[title="Admit"], button.btn-admit')
        .first();
      await expect(nonHostAdmitButton).toBeVisible({ timeout: 30_000 });
      await nonHostAdmitButton.dispatchEvent("click");
      await nonHostPage.waitForTimeout(3000);

      // ── Guest lands in the meeting grid ───────────────────────────────────
      await waitForGrid(guestPage);
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
    } finally {
      await hostBrowser.close();
      await nonHostBrowser.close();
      await guestBrowser.close();
    }
  });
});
