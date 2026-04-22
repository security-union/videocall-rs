import { test, expect, Page, chromium } from "@playwright/test";
import { injectSessionCookie, generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Meeting settings – Options toggles", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    // Meeting settings page only exists in Dioxus UI (port 3001)
    test.skip(!baseURL?.includes("3001"), "Meeting settings page is Dioxus-only");
    await injectSessionCookie(context, { baseURL });
  });

  /**
   * Create a meeting by joining from the home page, then navigate to the
   * settings page for that meeting. Returns once the Options card is visible.
   */
  async function createMeetingAndOpenSettings(
    page: Page,
    meetingId: string,
    username: string,
  ): Promise<void> {
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially(username, { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    // Wait for meeting page to load and auto-join API (creates the meeting)
    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
      timeout: 10_000,
    });
    await expect(page.getByText(/Start Meeting|Join Meeting/)).toBeVisible({
      timeout: 20_000,
    });

    // Navigate to the settings page
    await page.goto(`/meeting/${meetingId}/settings`);
    await page.waitForTimeout(1500);

    await expect(page.getByText("Meeting Settings")).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.getByText("Options")).toBeVisible({ timeout: 5_000 });
  }

  /** Locate the toggle button inside a settings-option-row by its label text. */
  function optionToggle(page: Page, label: string) {
    return page
      .locator(".settings-option-row")
      .filter({ hasText: label })
      .locator('button[role="switch"]');
  }

  test("settings page displays both Waiting Room and Participants can admit others toggles", async ({
    page,
  }) => {
    const meetingId = `e2e_opt_show_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "opt-show-user");

    await expect(page.getByText("Waiting Room")).toBeVisible();
    await expect(page.getByText("Participants can admit others")).toBeVisible();

    const wrToggle = optionToggle(page, "Waiting Room");
    const acaToggle = optionToggle(page, "Participants can admit others");

    await expect(wrToggle).toBeVisible();
    await expect(acaToggle).toBeVisible();

    // Default: waiting room ON, admitted_can_admit OFF
    await expect(wrToggle).toHaveAttribute("aria-checked", "true");
    await expect(acaToggle).toHaveAttribute("aria-checked", "false");
  });

  test("Waiting Room toggle can be toggled off and on", async ({ page }) => {
    const meetingId = `e2e_opt_wr_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "wr-toggle-user");

    const wrToggle = optionToggle(page, "Waiting Room");

    // Starts ON
    await expect(wrToggle).toHaveAttribute("aria-checked", "true");

    // Toggle OFF
    await wrToggle.click();
    await expect(wrToggle).toHaveAttribute("aria-checked", "false", {
      timeout: 5_000,
    });

    // Toggle back ON
    await wrToggle.click();
    await expect(wrToggle).toHaveAttribute("aria-checked", "true", {
      timeout: 5_000,
    });
  });

  test("Participants can admit others toggle is disabled when Waiting Room is off", async ({
    page,
  }) => {
    const meetingId = `e2e_opt_aca_dis_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "aca-dis-user");

    const wrToggle = optionToggle(page, "Waiting Room");
    const acaToggle = optionToggle(page, "Participants can admit others");

    // Turn waiting room OFF first
    await wrToggle.click();
    await expect(wrToggle).toHaveAttribute("aria-checked", "false", {
      timeout: 5_000,
    });

    // Admitted-can-admit should be disabled and off
    await expect(acaToggle).toBeDisabled();
    await expect(acaToggle).toHaveAttribute("aria-checked", "false");
  });

  test("Participants can admit others toggle can be enabled when Waiting Room is on", async ({
    page,
  }) => {
    const meetingId = `e2e_opt_aca_on_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "aca-on-user");

    const wrToggle = optionToggle(page, "Waiting Room");
    const acaToggle = optionToggle(page, "Participants can admit others");

    // Waiting room should be ON by default
    await expect(wrToggle).toHaveAttribute("aria-checked", "true");

    // Admitted-can-admit should be enabled (not disabled) but OFF
    await expect(acaToggle).not.toBeDisabled();
    await expect(acaToggle).toHaveAttribute("aria-checked", "false");

    // Toggle admitted-can-admit ON
    await acaToggle.click();
    await expect(acaToggle).toHaveAttribute("aria-checked", "true", {
      timeout: 5_000,
    });

    // Toggle it back OFF
    await acaToggle.click();
    await expect(acaToggle).toHaveAttribute("aria-checked", "false", {
      timeout: 5_000,
    });
  });

  test("disabling Waiting Room also turns off Participants can admit others", async ({ page }) => {
    const meetingId = `e2e_opt_cascade_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "cascade-user");

    const wrToggle = optionToggle(page, "Waiting Room");
    const acaToggle = optionToggle(page, "Participants can admit others");

    // Enable admitted-can-admit (waiting room is already ON by default)
    await acaToggle.click();
    await expect(acaToggle).toHaveAttribute("aria-checked", "true", {
      timeout: 5_000,
    });

    // Now disable waiting room — admitted-can-admit should also turn off
    await wrToggle.click();
    await expect(wrToggle).toHaveAttribute("aria-checked", "false", {
      timeout: 5_000,
    });
    await expect(acaToggle).toHaveAttribute("aria-checked", "false", {
      timeout: 5_000,
    });
    await expect(acaToggle).toBeDisabled();
  });

  test("toggle state persists after page reload", async ({ page }) => {
    const meetingId = `e2e_opt_persist_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "persist-user");

    const acaToggle = optionToggle(page, "Participants can admit others");

    // Enable admitted-can-admit
    await acaToggle.click();
    await expect(acaToggle).toHaveAttribute("aria-checked", "true", {
      timeout: 5_000,
    });

    // Reload the page
    await page.reload();
    await page.waitForTimeout(2000);
    await expect(page.getByText("Options")).toBeVisible({ timeout: 10_000 });

    // Both toggles should reflect the persisted state
    await expect(optionToggle(page, "Waiting Room")).toHaveAttribute("aria-checked", "true");
    await expect(optionToggle(page, "Participants can admit others")).toHaveAttribute(
      "aria-checked",
      "true",
    );
  });

  test("End meeting when host leaves toggle appears and persists", async ({ page }) => {
    const meetingId = `e2e_eohl_show_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "eohl-show-user");

    const eohToggle = optionToggle(page, "End meeting when host leaves");
    await expect(eohToggle).toBeVisible();

    // Record default state and toggle it
    const defaultChecked = await eohToggle.getAttribute("aria-checked");
    await eohToggle.click();
    const afterToggle = defaultChecked === "true" ? "false" : "true";
    await expect(eohToggle).toHaveAttribute("aria-checked", afterToggle, { timeout: 5_000 });

    // Reload and verify state persists
    await page.reload();
    await page.waitForTimeout(2000);
    await expect(page.getByText("Options")).toBeVisible({ timeout: 10_000 });
    await expect(optionToggle(page, "End meeting when host leaves")).toHaveAttribute(
      "aria-checked",
      afterToggle,
    );
  });
});

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
];

async function createAuthenticatedContext(
  browser: Awaited<ReturnType<typeof chromium.launch>>,
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
 * Navigate the host to the meeting, enable "End meeting when host leaves",
 * then return. Used by both host-leave lifecycle tests.
 */
async function hostJoinAndConfigureEohl(
  hostPage: Page,
  meetingId: string,
  eohl: boolean,
): Promise<void> {
  await hostPage.goto("/");
  await hostPage.waitForTimeout(1500);

  await hostPage.locator("#meeting-id").click();
  await hostPage.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await hostPage.locator("#username").click();
  await hostPage.locator("#username").fill("");
  await hostPage.locator("#username").pressSequentially("HostUser", { delay: 50 });
  await hostPage.waitForTimeout(500);
  await hostPage.locator("#username").press("Enter");
  await expect(hostPage).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

  // Go to settings and configure the toggle
  await hostPage.goto(`/meeting/${meetingId}/settings`);
  await hostPage.waitForTimeout(1500);
  await expect(hostPage.getByText("Options")).toBeVisible({ timeout: 10_000 });

  const eohToggle = hostPage
    .locator(".settings-option-row")
    .filter({ hasText: "End meeting when host leaves" })
    .locator('button[role="switch"]');
  await expect(eohToggle).toBeVisible({ timeout: 5_000 });

  const currentChecked = await eohToggle.getAttribute("aria-checked");
  const shouldBe = eohl ? "true" : "false";
  if (currentChecked !== shouldBe) {
    await eohToggle.click();
    await expect(eohToggle).toHaveAttribute("aria-checked", shouldBe, { timeout: 5_000 });
  }

  // Navigate back to the meeting page
  await hostPage.goto(`/meeting/${meetingId}`);
  await hostPage.waitForTimeout(1500);

  // Click Join/Start Meeting
  const joinButton = hostPage.getByText(/Start Meeting|Join Meeting/);
  await expect(joinButton).toBeVisible({ timeout: 20_000 });
  await joinButton.click();
  await hostPage.waitForTimeout(3000);
  await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
}

/**
 * Join as a guest, handling the waiting room if present.
 */
async function guestJoinMeeting(
  guestPage: Page,
  hostPage: Page,
  meetingId: string,
): Promise<void> {
  await guestPage.goto("/");
  await guestPage.waitForTimeout(1500);

  await guestPage.locator("#meeting-id").click();
  await guestPage.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await guestPage.locator("#username").click();
  await guestPage.locator("#username").fill("");
  await guestPage.locator("#username").pressSequentially("GuestUser", { delay: 50 });
  await guestPage.waitForTimeout(500);
  await guestPage.locator("#username").press("Enter");
  await expect(guestPage).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await guestPage.waitForTimeout(1500);

  const joinButton = guestPage.getByText(/Start Meeting|Join Meeting/);
  const waitingRoom = guestPage.getByText("Waiting to be admitted");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 20_000 }).then(() => "waiting" as const),
  ]);

  if (result === "waiting") {
    // Admit guest from the host page
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);

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
      await expect(guestGrid).toBeVisible({ timeout: 15_000 });
    }
  } else {
    await guestPage.waitForTimeout(1000);
    await joinButton.click();
    await guestPage.waitForTimeout(3000);
    await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
  }
}

test.describe("Meeting settings – End meeting when host leaves lifecycle", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("host leave with end_on_host_leave ON ends meeting for all participants", async ({
    baseURL,
  }) => {
    test.skip(!baseURL?.includes("3001"), "Meeting lifecycle tests are Dioxus-only");
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_eohl_on_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "eohl-host@videocall.rs",
        "HostUser",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "eohl-guest@videocall.rs",
        "GuestUser",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host creates meeting and enables "End meeting when host leaves"
      await hostJoinAndConfigureEohl(hostPage, meetingId, true);

      // Guest joins
      await guestJoinMeeting(guestPage, hostPage, meetingId);

      // Give time for peer discovery
      await hostPage.waitForTimeout(3000);

      // Host hangs up — triggers Leave message → MEETING_ENDED broadcast
      await hostPage.locator("button.video-control-button.danger").click();
      await hostPage.waitForTimeout(2000);

      // Guest should see the meeting-ended overlay
      await expect(guestPage.locator(".meeting-ended-overlay")).toBeVisible({ timeout: 20_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("host leave with end_on_host_leave OFF lets participants continue", async ({ baseURL }) => {
    test.skip(!baseURL?.includes("3001"), "Meeting lifecycle tests are Dioxus-only");
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_eohl_off_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "eohl-off-host@videocall.rs",
        "HostUser",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "eohl-off-guest@videocall.rs",
        "GuestUser",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host creates meeting with "End meeting when host leaves" OFF
      await hostJoinAndConfigureEohl(hostPage, meetingId, false);

      // Guest joins
      await guestJoinMeeting(guestPage, hostPage, meetingId);

      // Give time for peer discovery
      await hostPage.waitForTimeout(3000);

      // Host hangs up
      await hostPage.locator("button.video-control-button.danger").click();
      await hostPage.waitForTimeout(5000);

      // Guest should NOT see the meeting-ended overlay — meeting continues
      await expect(guestPage.locator(".meeting-ended-overlay")).not.toBeVisible({
        timeout: 8_000,
      });
      // Guest's grid should still be present
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 5_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});
