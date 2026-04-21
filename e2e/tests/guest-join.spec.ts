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

/**
 * Create a meeting via the API with specific settings.
 * Returns the meeting_id.
 */
async function createMeetingViaApi(
  hostEmail: string,
  hostName: string,
  meetingId: string,
  opts: { allowGuests: boolean; waitingRoomEnabled?: boolean },
): Promise<string> {
  const token = generateSessionToken(hostEmail, hostName);
  const url = `${API_URL}/api/v1/meetings`;
  const res = await fetch(url, {
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
  return json.result.meeting_id;
}

/**
 * Have the host join and start the meeting so it transitions from "idle" to "active".
 */
async function hostStartsMeeting(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  hostEmail: string,
  hostName: string,
  meetingId: string,
  uiURL: string,
): Promise<{
  hostPage: Page;
  hostContext: Awaited<ReturnType<typeof createAuthenticatedContext>>;
}> {
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

  return { hostPage, hostContext };
}

test.describe("Guest join flow", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("guest sees error when guests are not allowed", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_noallow_${Date.now()}`;
    const hostEmail = "host-noguest@videocall.rs";
    const hostName = "HostNoGuest";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: false,
      });

      // Host must start the meeting for it to become active
      const { hostPage } = await hostStartsMeeting(browser, hostEmail, hostName, meetingId, uiURL);
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Open the guest join page in a new context (no auth cookie)
      const guestCtx = await browser.newContext({
        baseURL: uiURL,
        ignoreHTTPSErrors: true,
      });
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // Fill in name and submit
      await guestPage.locator("#guest-name").click();
      await guestPage.locator("#guest-name").pressSequentially("TestGuest", { delay: 50 });
      await guestPage.waitForTimeout(500);
      await guestPage.locator("#guest-name").press("Enter");

      // Should see the error state
      await expect(guestPage.getByText("Unable to join")).toBeVisible({ timeout: 15_000 });
      await expect(guestPage.getByText("Guests are not allowed in this meeting")).toBeVisible({
        timeout: 5_000,
      });
      await expect(guestPage.getByText("Return to Home")).toBeVisible({ timeout: 5_000 });
    } finally {
      await browser.close();
    }
  });

  test("guest sees error for non-existent meeting", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_nonexist_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const guestCtx = await browser.newContext({
        baseURL: uiURL,
        ignoreHTTPSErrors: true,
      });
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // Fill in name and submit
      await guestPage.locator("#guest-name").click();
      await guestPage.locator("#guest-name").pressSequentially("LostGuest", { delay: 50 });
      await guestPage.waitForTimeout(500);
      await guestPage.locator("#guest-name").press("Enter");

      // The API returns 403 GUESTS_NOT_ALLOWED for non-existent meetings
      // (to prevent meeting enumeration). The UI shows the error state.
      await expect(guestPage.getByText("Unable to join")).toBeVisible({ timeout: 15_000 });
      await expect(guestPage.getByText("Return to Home")).toBeVisible({ timeout: 5_000 });
    } finally {
      await browser.close();
    }
  });

  test("guest joins directly when allow_guests=true and no waiting room", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_direct_${Date.now()}`;
    const hostEmail = "host-directguest@videocall.rs";
    const hostName = "HostDirect";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: false,
      });

      const { hostPage } = await hostStartsMeeting(browser, hostEmail, hostName, meetingId, uiURL);
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Open guest join page
      const guestCtx = await browser.newContext({
        baseURL: uiURL,
        ignoreHTTPSErrors: true,
      });
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // Fill in name and submit
      await guestPage.locator("#guest-name").click();
      await guestPage.locator("#guest-name").pressSequentially("DirectGuest", { delay: 50 });
      await guestPage.waitForTimeout(500);
      await guestPage.locator("#guest-name").press("Enter");

      // Guest should be admitted directly (no waiting room).
      // The AttendantsComponent renders either a "Join Meeting"/"Start Meeting"
      // button or goes straight to the grid.
      const joinButton = guestPage.getByText(/Join Meeting|Start Meeting/);
      const grid = guestPage.locator("#grid-container");

      const guestResult = await Promise.race([
        joinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
        grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
      ]);

      if (guestResult === "join-button") {
        await guestPage.waitForTimeout(1000);
        await joinButton.click();
        await guestPage.waitForTimeout(3000);
      }

      await expect(grid).toBeVisible({ timeout: 15_000 });
    } finally {
      await browser.close();
    }
  });

  test("guest enters waiting room, host admits, guest transitions to admitted", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_wr_${Date.now()}`;
    const hostEmail = "host-wrguest@videocall.rs";
    const hostName = "HostWR";

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: true,
      });

      const { hostPage } = await hostStartsMeeting(browser1, hostEmail, hostName, meetingId, uiURL);
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Open guest join page in a separate browser
      const guestCtx = await browser2.newContext({
        baseURL: uiURL,
        ignoreHTTPSErrors: true,
      });
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // Fill in name and submit
      await guestPage.locator("#guest-name").click();
      await guestPage.locator("#guest-name").pressSequentially("WaitingGuest", { delay: 50 });
      await guestPage.waitForTimeout(500);
      await guestPage.locator("#guest-name").press("Enter");

      // Guest should enter the waiting room
      await expect(guestPage.getByText("Waiting to be admitted")).toBeVisible({ timeout: 20_000 });

      // Host admits the guest
      const admitButton = hostPage.getByTitle("Admit").first();
      await expect(admitButton).toBeVisible({ timeout: 20_000 });
      await hostPage.waitForTimeout(1000);
      await admitButton.dispatchEvent("click");
      await hostPage.waitForTimeout(3000);

      // Guest should transition to admitted state
      const guestJoinButton = guestPage.getByText(/Join Meeting|Start Meeting/);
      const guestGrid = guestPage.locator("#grid-container");

      const postAdmit = await Promise.race([
        guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
        guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
      ]);

      if (postAdmit === "join-button") {
        await guestPage.waitForTimeout(1000);
        await guestJoinButton.click();
        await guestPage.waitForTimeout(3000);
      }

      await expect(guestGrid).toBeVisible({ timeout: 15_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("join button is disabled when display name is empty", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_empty_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const guestCtx = await browser.newContext({
        baseURL: uiURL,
        ignoreHTTPSErrors: true,
      });
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // The guest-name input should start empty
      await expect(guestPage.locator("#guest-name")).toHaveValue("");

      // The submit button should be disabled when the name is empty
      const submitButton = guestPage.locator('button[type="submit"]');
      await expect(submitButton).toBeDisabled();

      // Type a name — button should become enabled
      await guestPage.locator("#guest-name").click();
      await guestPage.locator("#guest-name").pressSequentially("SomeName", { delay: 50 });
      await guestPage.waitForTimeout(500);
      await expect(submitButton).toBeEnabled();

      // Clear the name — button should be disabled again
      await guestPage.locator("#guest-name").fill("");
      await guestPage.waitForTimeout(500);
      await expect(submitButton).toBeDisabled();

      // Type only whitespace — button should remain disabled
      await guestPage.locator("#guest-name").fill("   ");
      await guestPage.waitForTimeout(500);
      await expect(submitButton).toBeDisabled();
    } finally {
      await browser.close();
    }
  });

  test("guest join page shows form elements correctly", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_form_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const guestCtx = await browser.newContext({
        baseURL: uiURL,
        ignoreHTTPSErrors: true,
      });
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // Page title
      await expect(guestPage.getByText("Join as Guest")).toBeVisible({ timeout: 5_000 });
      await expect(guestPage.getByText("Join Meeting as Guest")).toBeVisible({ timeout: 5_000 });

      // Meeting ID is displayed
      await expect(guestPage.getByText(meetingId)).toBeVisible({ timeout: 5_000 });

      // Name input with correct label
      await expect(guestPage.getByText("Your Name")).toBeVisible({ timeout: 5_000 });
      await expect(guestPage.locator("#guest-name")).toBeVisible();
      await expect(guestPage.locator("#guest-name")).toHaveAttribute(
        "placeholder",
        "Enter your display name",
      );

      // Validation hint
      await expect(
        guestPage.getByText("Allowed: letters, numbers, spaces, hyphens, underscores, apostrophes"),
      ).toBeVisible({ timeout: 5_000 });

      // Guest disclaimer
      await expect(guestPage.getByText("You are joining without an account")).toBeVisible({
        timeout: 5_000,
      });
    } finally {
      await browser.close();
    }
  });
});
