import { test, expect, Page, chromium, Locator } from "@playwright/test";
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
    await expect(page.getByRole("button", { name: /Start Meeting|Join Meeting/ })).toBeVisible({
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

  /**
   * Locate an Activity-card stat row by the EXACT text of its
   * `.settings-stat-label` span. Anchored on the RSX structure
   * (`<div class="settings-stat-row"><span class="settings-stat-label">…`) so
   * the match is stable against timestamp/value text and copy changes elsewhere
   * on the page. The exact-match regex avoids "Started"/"Ended" substring
   * collisions with any other row.
   */
  function statRow(page: Page, label: string): Locator {
    return page.locator(".settings-stat-row").filter({
      has: page.locator(".settings-stat-label", { hasText: new RegExp(`^${label}$`) }),
    });
  }

  // Issue 1672: the Activity card renders the meeting time as SEPARATE labeled
  // field-lines ("Started", optionally "Ended", "Duration") instead of the old
  // single-line "started – ended" range that overflowed the dialog at narrow
  // widths. These two tests pin the new per-state row structure.
  test("open meeting Activity card shows Started and Duration rows but no Ended row (issue 1672)", async ({
    page,
  }) => {
    const meetingId = `e2e_activity_open_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "activity-open-user");

    // Open (idle/active) meeting: "Started" and "Duration" each render as their
    // own labeled row with a non-empty value.
    const startedRow = statRow(page, "Started");
    await expect(startedRow).toBeVisible();
    await expect(startedRow.locator(".settings-stat-value")).toHaveText(/\S/);

    const durationRow = statRow(page, "Duration");
    await expect(durationRow).toBeVisible();
    await expect(durationRow.locator(".settings-stat-value")).toHaveText(/\S/);

    // No "Ended" row while the meeting is still open (ended_at is None).
    await expect(page.locator(".settings-stat-label", { hasText: /^Ended$/ })).toHaveCount(0);

    // The pre-1672 combined single-line range is gone: no "Time" label and no
    // ".settings-stat-separator" span. (For an OPEN meeting the pre-fix label
    // was already "Started", so this documents the new structure — the ended
    // test below carries the discriminating assertion.)
    await expect(page.locator(".settings-stat-label", { hasText: /^Time$/ })).toHaveCount(0);
    await expect(page.locator(".settings-stat-separator")).toHaveCount(0);
  });

  test("ended meeting Activity card shows distinct Started, Ended, and Duration rows (issue 1672)", async ({
    page,
  }) => {
    const meetingId = `e2e_activity_ended_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "activity-ended-user");

    // Drive the real end-meeting flow from the settings page. The page uses
    // window.confirm, which Playwright auto-DISMISSES unless we explicitly
    // accept it, so register the handler before clicking.
    page.once("dialog", (dialog) => dialog.accept());
    await page.getByRole("button", { name: /End Meeting/ }).click();

    // Once ended, the Activity card renders THREE separate labeled rows. The
    // distinct "Ended" row is the discriminator: the pre-1672 code rendered a
    // single combined "Time" label whose value was a ".settings-stat-separator"
    // range and had NO "Ended" label — so this row's presence (and the absence
    // of "Time" / the separator asserted below) fails on the un-fixed code.
    const endedRow = statRow(page, "Ended");
    await expect(endedRow).toBeVisible({ timeout: 10_000 });
    await expect(endedRow.locator(".settings-stat-value")).toHaveText(/\S/);

    const startedRow = statRow(page, "Started");
    await expect(startedRow).toBeVisible();
    await expect(startedRow.locator(".settings-stat-value")).toHaveText(/\S/);

    const durationRow = statRow(page, "Duration");
    await expect(durationRow).toBeVisible();
    await expect(durationRow.locator(".settings-stat-value")).toHaveText(/\S/);

    // The old single-line combined range must be fully gone in the new layout:
    // no "Time" label (it is now "Started"/"Ended") and no range separator span.
    await expect(page.locator(".settings-stat-label", { hasText: /^Time$/ })).toHaveCount(0);
    await expect(page.locator(".settings-stat-separator")).toHaveCount(0);
  });

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

  // Regression: disabling Waiting Room optimistically clears "Admitted can
  // admit" (ACA). If the update_meeting PATCH then FAILS, the client must roll
  // BOTH toggles back — pre-fix, only Waiting Room was restored, leaving ACA
  // stuck OFF on screen while the server still had it ON. This drives the
  // error-branch wiring (secondary rollback) that the pure unit tests cannot:
  // reverting the rollback wiring in meeting_options_controls.rs makes the
  // final ACA assertion below time out.
  test("failed Waiting Room disable restores Admitted can admit", async ({ page }) => {
    const meetingId = `e2e_opt_rollback_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "rollback-user");

    const wrToggle = optionToggle(page, "Waiting Room");
    const acaToggle = optionToggle(page, "Admitted can admit");

    // Precondition: Waiting Room ON (default) and ACA turned ON via a REAL
    // (succeeding) PATCH — the route interceptor is installed only afterwards.
    await expect(wrToggle).toHaveAttribute("aria-checked", "true");
    await acaToggle.click();
    await expect(acaToggle).toHaveAttribute("aria-checked", "true", {
      timeout: 5_000,
    });

    // Force the NEXT update_meeting PATCH (the Waiting-Room disable) to fail.
    // Installed after the ACA-enable PATCH already succeeded, so only the
    // disable call fails; all other meeting API traffic passes through.
    await page.route("**/api/v1/meetings/**", async (route) => {
      if (route.request().method() === "PATCH") {
        await route.fulfill({
          status: 500,
          contentType: "application/json",
          body: JSON.stringify({ success: false, error: "e2e forced failure" }),
        });
        return;
      }
      await route.continue();
    });

    // Disable Waiting Room: optimistically clears both toggles, PATCH fails,
    // client rolls BOTH back to ON.
    await wrToggle.click();

    await expect(wrToggle).toHaveAttribute("aria-checked", "true", {
      timeout: 5_000,
    });
    await expect(acaToggle).toHaveAttribute("aria-checked", "true", {
      timeout: 5_000,
    });
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
    await expect(page.getByText("Options")).toBeVisible({ timeout: 10_000 });
    await expect(optionToggle(page, "End meeting when host leaves")).toHaveAttribute(
      "aria-checked",
      afterToggle,
    );
  });

  // Issue 1551 (bug 1): the guest join link is a full meeting URL and used to
  // overflow its card because `.settings-field-value` forces `white-space:
  // nowrap`. The `.settings-guest-link` modifier restores wrapping so the URL
  // stays inside the card. We assert the link wraps (no horizontal overflow)
  // and that the wrapping CSS is actually applied.
  test("guest join link wraps and does not overflow its card", async ({ page }) => {
    const meetingId = `e2e_guest_link_wrap_${Date.now()}`;
    await createMeetingAndOpenSettings(page, meetingId, "guest-link-user");

    // Enable "Allow Guests" so the join-link row renders.
    const allowGuestsToggle = optionToggle(page, "Allow Guests");
    await expect(allowGuestsToggle).toBeVisible();
    if ((await allowGuestsToggle.getAttribute("aria-checked")) !== "true") {
      await allowGuestsToggle.click();
      await expect(allowGuestsToggle).toHaveAttribute("aria-checked", "true", { timeout: 5_000 });
    }

    const link = page.locator(".settings-guest-link");
    await expect(link).toBeVisible({ timeout: 5_000 });

    // The link must contain the full guest URL (a long string that would
    // overflow without wrapping).
    await expect(link).toContainText(`/meeting/${meetingId}/guest`);

    // Wrapping CSS must be applied: not the single-line nowrap from
    // `.settings-field-value`.
    const whiteSpace = await link.evaluate((el) => getComputedStyle(el).whiteSpace);
    expect(whiteSpace).not.toBe("nowrap");
    const overflowWrap = await link.evaluate((el) => getComputedStyle(el).overflowWrap);
    expect(overflowWrap).toBe("anywhere");

    // No horizontal overflow: the rendered content fits within the element's
    // own box (which is itself constrained to the card width).
    const overflows = await link.evaluate((el) => el.scrollWidth > el.clientWidth + 1);
    expect(overflows).toBe(false);
  });
});

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
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
  const joinButton = hostPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  await expect(joinButton).toBeVisible({ timeout: 20_000 });
  await joinButton.click();
  await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
}

/**
 * Join as a guest, handling the waiting room if present.
 */
async function guestJoinMeeting(guestPage: Page, hostPage: Page, meetingId: string): Promise<void> {
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

  const joinButton = guestPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
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

    const guestJoinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
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

async function navigateToMeetingFromHome(
  page: Page,
  meetingId: string,
  username: string,
): Promise<void> {
  await page.goto("/");
  await expect(page.locator("#meeting-id")).toBeVisible({ timeout: 20_000 });

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  // Accept any of the three valid post-navigation states: the pre-join
  // Start/Join button, the waiting-room message, or — if the prejoin flow
  // auto-advanced — the in-meeting grid itself. Without the grid fallback the
  // helper hangs whenever a peer skips the prejoin screen.
  await expect(
    page
      .getByRole("button", { name: /Start Meeting|Join Meeting/ })
      .or(page.getByText("Waiting to be admitted"))
      .or(page.locator("#grid-container")),
  ).toBeVisible({
    timeout: 20_000,
  });
}

async function joinMeetingWhenReady(page: Page): Promise<"in-meeting" | "waiting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 20_000 }).then(() => "waiting" as const),
  ]);

  if (result === "waiting") {
    return "waiting";
  }

  const grid = page.locator("#grid-container");
  if (!(await grid.isVisible())) {
    try {
      await joinButton.click({ timeout: 5_000 });
    } catch (error) {
      if (!(await grid.isVisible())) {
        throw error;
      }
    }
  }
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

function settingsToggle(page: Page, label: string) {
  return page
    .locator(".settings-option-row")
    .filter({ hasText: label })
    .locator('button[role="switch"]');
}

async function ensureJoinedFromTransition(joinButton: Locator, grid: Locator): Promise<void> {
  if (!(await grid.isVisible())) {
    try {
      await joinButton.click({ timeout: 5_000 });
    } catch (error) {
      if (!(await grid.isVisible())) {
        throw error;
      }
    }
  }
  await expect(grid).toBeVisible({ timeout: 15_000 });
}

test.describe("Meeting settings – admitted_can_admit live propagation", () => {
  test.describe.configure({ timeout: 240_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  test("host toggles Participants can admit others mid-meeting and admitted participant gains controls live", async ({
    baseURL,
  }) => {
    test.skip(!baseURL?.includes("3001"), "Meeting settings tests are Dioxus-only");
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_aca_live_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const participantBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const waitingGuestBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        hostBrowser,
        "aca-live-host@videocall.rs",
        "HostUser",
        uiURL,
      );
      const participantCtx = await createAuthenticatedContext(
        participantBrowser,
        "aca-live-participant@videocall.rs",
        "ParticipantUser",
        uiURL,
      );
      const waitingGuestCtx = await createAuthenticatedContext(
        waitingGuestBrowser,
        "aca-live-waiting@videocall.rs",
        "WaitingUser",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const participantPage = await participantCtx.newPage();
      const waitingGuestPage = await waitingGuestCtx.newPage();

      // Host starts the meeting.
      await navigateToMeetingFromHome(hostPage, meetingId, "HostUser");
      await expect(joinMeetingWhenReady(hostPage)).resolves.toBe("in-meeting");

      // Admitted participant joins and is admitted by host.
      await navigateToMeetingFromHome(participantPage, meetingId, "ParticipantUser");
      const participantJoinState = await joinMeetingWhenReady(participantPage);
      if (participantJoinState === "waiting") {
        const hostAdmitParticipant = hostPage.getByTitle("Admit").first();
        await expect(hostAdmitParticipant).toBeVisible({ timeout: 20_000 });
        await hostAdmitParticipant.dispatchEvent("click");

        const participantJoinButton = participantPage.getByRole("button", {
          name: /Start Meeting|Join Meeting/,
        });
        const participantGrid = participantPage.locator("#grid-container");
        const participantTransition = await Promise.race([
          participantJoinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
          participantGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
        ]);

        if (participantTransition === "join") {
          await ensureJoinedFromTransition(participantJoinButton, participantGrid);
        }
      }

      await expect(participantPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Another user joins and remains in waiting room.
      await navigateToMeetingFromHome(waitingGuestPage, meetingId, "WaitingUser");
      await expect(joinMeetingWhenReady(waitingGuestPage)).resolves.toBe("waiting");
      await expect(waitingGuestPage.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 10_000,
      });

      // With admitted_can_admit OFF, admitted participant should not have attendant controls.
      const participantAdmitButton = participantPage.getByTitle("Admit").first();
      await expect(participantAdmitButton).not.toBeVisible({ timeout: 5_000 });

      // Host toggles admitted_can_admit ON mid-meeting.
      await hostPage.goto(`/meeting/${meetingId}/settings`);
      await expect(hostPage.getByText("Options")).toBeVisible({ timeout: 10_000 });
      const admittedCanAdmitToggle = settingsToggle(hostPage, "Participants can admit others");
      await expect(admittedCanAdmitToggle).toHaveAttribute("aria-checked", "false", {
        timeout: 5_000,
      });
      await admittedCanAdmitToggle.click();
      await expect(admittedCanAdmitToggle).toHaveAttribute("aria-checked", "true", {
        timeout: 5_000,
      });

      // Participant page should update live without reload and show attendant control.
      await expect(participantAdmitButton).toBeVisible({ timeout: 20_000 });

      // Server remains source of truth: participant can now admit the waiting user.
      await participantAdmitButton.dispatchEvent("click");

      const waitingGuestJoinButton = waitingGuestPage.getByRole("button", {
        name: /Start Meeting|Join Meeting/,
      });
      const waitingGuestGrid = waitingGuestPage.locator("#grid-container");
      const waitingGuestTransition = await Promise.race([
        waitingGuestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
        waitingGuestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
      ]);

      if (waitingGuestTransition === "join") {
        await ensureJoinedFromTransition(waitingGuestJoinButton, waitingGuestGrid);
      }

      await expect(waitingGuestPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
    } finally {
      await hostBrowser.close();
      await participantBrowser.close();
      await waitingGuestBrowser.close();
    }
  });
});

/** Locate the "Participants" stat value on the settings Activity card. */
function participantsStatValue(page: Page): Locator {
  return page
    .locator(".settings-stat-row")
    .filter({ hasText: "Participants" })
    .locator(".settings-stat-value");
}

test.describe("Meeting settings – live participant count refresh (issue 1551)", () => {
  test.describe.configure({ timeout: 180_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  // Issue 1551 (bug 2): the Activity "Participants" count was fetched once at
  // page load and never refreshed. The page now polls `get_meeting_info` on a
  // 12s interval and updates the read-only stats in place. We open the host's
  // settings page, have a second browser peer join the live meeting, and assert
  // the count rises WITHOUT a reload within the poll window.
  test("Activity participant count updates while a peer joins, without reload", async ({
    baseURL,
  }) => {
    test.skip(!baseURL?.includes("3001"), "Meeting settings tests are Dioxus-only");
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_live_count_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const peerBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        hostBrowser,
        "live-count-host@videocall.rs",
        "HostUser",
        uiURL,
      );
      const peerCtx = await createAuthenticatedContext(
        peerBrowser,
        "live-count-peer@videocall.rs",
        "PeerUser",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const peerPage = await peerCtx.newPage();

      // Host joins the live meeting in its own tab and STAYS there for the whole
      // test. The host must remain present in the grid so the meeting never goes
      // idle/host-less — otherwise a joining peer has no host to admit it and
      // the participant-count dynamics become racy (count could drop to 0 before
      // the peer arrives). This mirrors the proven host+peer sequence used by the
      // passing "admitted_can_admit live propagation" test above.
      await navigateToMeetingFromHome(hostPage, meetingId, "HostUser");
      await expect(joinMeetingWhenReady(hostPage)).resolves.toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Open the settings page in a SEPARATE tab so the host's meeting tab keeps
      // the meeting active. The Activity count read here should already be >= 1
      // with the host present.
      const settingsPage = await hostCtx.newPage();
      await settingsPage.goto(`/meeting/${meetingId}/settings`);
      await expect(settingsPage.getByText("Options")).toBeVisible({ timeout: 10_000 });

      const countValue = participantsStatValue(settingsPage);
      await expect(countValue).toBeVisible({ timeout: 5_000 });
      const initialCount = parseInt((await countValue.textContent())?.trim() || "0", 10);

      // A second peer joins the live meeting. Waiting-room default is ON and the
      // host is present, so the peer deterministically lands in the waiting room
      // and is admitted from the host's meeting tab (which is in the grid and has
      // the Admit UI). Admitting moves the peer to present and bumps the count.
      await navigateToMeetingFromHome(peerPage, meetingId, "PeerUser");
      const peerJoinState = await joinMeetingWhenReady(peerPage);
      if (peerJoinState === "waiting") {
        const admit = hostPage.getByTitle("Admit").first();
        await expect(admit).toBeVisible({ timeout: 20_000 });
        await admit.dispatchEvent("click");

        const peerJoinButton = peerPage.getByRole("button", {
          name: /Start Meeting|Join Meeting/,
        });
        const peerGrid = peerPage.locator("#grid-container");
        const peerTransition = await Promise.race([
          peerJoinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
          peerGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
        ]);
        if (peerTransition === "join") {
          await ensureJoinedFromTransition(peerJoinButton, peerGrid);
        }
      }
      await expect(peerPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // The settings page (never reloaded) must reflect the higher count once a
      // poll tick (12s) fires. Allow generous slack for poll + network.
      await expect
        .poll(async () => parseInt((await countValue.textContent())?.trim() || "0", 10), {
          timeout: 40_000,
        })
        .toBeGreaterThan(initialCount);
    } finally {
      await hostBrowser.close();
      await peerBrowser.close();
    }
  });
});

test.describe("Meeting settings – in-call Meeting Options panel", () => {
  test.describe.configure({ timeout: 240_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  /** Open the host's in-call Meeting Options panel and set the Waiting Room
   *  toggle to `on`, then close the panel. Asserts the toggle reached the
   *  requested state before closing. */
  async function setWaitingRoomFromInCallPanel(hostPage: Page, on: boolean): Promise<void> {
    const optionsButton = hostPage.locator('[data-testid="open-meeting-options"]');
    await expect(optionsButton).toBeVisible({ timeout: 10_000 });
    await optionsButton.click();

    const wrToggle = settingsToggle(hostPage, "Waiting Room");
    await expect(wrToggle).toBeVisible({ timeout: 5_000 });
    const want = on ? "true" : "false";
    if ((await wrToggle.getAttribute("aria-checked")) !== want) {
      await wrToggle.click();
      await expect(wrToggle).toHaveAttribute("aria-checked", want, { timeout: 5_000 });
    }

    await hostPage.getByRole("button", { name: "Done" }).click();
    await expect(wrToggle).not.toBeVisible({ timeout: 5_000 });
  }

  // The host changes meeting options live from INSIDE the call (without going
  // to the separate settings page / another tab) via the new Meeting Options
  // control. This test is discriminating in BOTH directions so it cannot pass
  // on a no-op panel: because the waiting room defaults to ON, we first switch
  // it OFF in-call and prove a joiner is AUTO-ADMITTED (only true if OFF
  // actually reached the server), then switch it back ON in-call and prove a
  // later joiner is PLACED IN THE WAITING ROOM with the host getting the admit
  // prompt — the headline scenario.
  test("host toggles the waiting room live from the in-call panel and joiners follow the new state", async ({
    baseURL,
  }) => {
    test.skip(!baseURL?.includes("3001"), "In-call options panel is Dioxus-only");
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_incall_opts_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const earlyPeerBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const latePeerBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        hostBrowser,
        "incall-opts-host@videocall.rs",
        "HostUser",
        uiURL,
      );
      const earlyPeerCtx = await createAuthenticatedContext(
        earlyPeerBrowser,
        "incall-opts-early@videocall.rs",
        "EarlyPeer",
        uiURL,
      );
      const latePeerCtx = await createAuthenticatedContext(
        latePeerBrowser,
        "incall-opts-late@videocall.rs",
        "LatePeer",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const earlyPeerPage = await earlyPeerCtx.newPage();
      const latePeerPage = await latePeerCtx.newPage();

      // Host starts the meeting and lands in the grid.
      await navigateToMeetingFromHome(hostPage, meetingId, "HostUser");
      await expect(joinMeetingWhenReady(hostPage)).resolves.toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // ── Direction 1: switch waiting room OFF in-call → joiner auto-admits ──
      // The default is ON, so an auto-admit here can ONLY happen if the in-call
      // panel actually persisted waiting_room=false to the server.
      await setWaitingRoomFromInCallPanel(hostPage, false);

      await navigateToMeetingFromHome(earlyPeerPage, meetingId, "EarlyPeer");
      await expect(joinMeetingWhenReady(earlyPeerPage)).resolves.toBe("in-meeting");
      await expect(earlyPeerPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // ── Direction 2: switch waiting room ON in-call → later joiner waits ──
      await setWaitingRoomFromInCallPanel(hostPage, true);

      await navigateToMeetingFromHome(latePeerPage, meetingId, "LatePeer");
      await expect(joinMeetingWhenReady(latePeerPage)).resolves.toBe("waiting");
      await expect(latePeerPage.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 10_000,
      });

      // The host receives the admit prompt (the "message to allow peer into
      // meeting") and admits the waiting peer.
      const admitButton = hostPage.getByTitle("Admit").first();
      await expect(admitButton).toBeVisible({ timeout: 20_000 });
      await admitButton.dispatchEvent("click");

      const lateJoinButton = latePeerPage.getByRole("button", {
        name: /Start Meeting|Join Meeting/,
      });
      const lateGrid = latePeerPage.locator("#grid-container");
      const lateTransition = await Promise.race([
        lateJoinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
        lateGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
      ]);
      if (lateTransition === "join") {
        await ensureJoinedFromTransition(lateJoinButton, lateGrid);
      }
      await expect(lateGrid).toBeVisible({ timeout: 15_000 });
    } finally {
      await hostBrowser.close();
      await earlyPeerBrowser.close();
      await latePeerBrowser.close();
    }
  });
});

// Issue 1845: the four verbose notification toggle rows were consolidated into a
// 2×2 "announcement matrix" (rows: participant joins/leaves; columns: the
// Message and Sound channels) inside the device-settings modal's Preferences
// tab. These tests pin the new structure, the exact per-cell state mapping (a
// swap here is the worst-case bug), the reused help-icon tooltips, and the four
// distinct accessible names. Selectors are anchored to the RSX in
// preferences_settings_panel.rs.
test.describe("Meeting settings – announcement notifications matrix (issue 1845)", () => {
  // testid → the exact AppearanceSettings storage key it must flip, plus the
  // accessible name the aria-labelledby row/column ids must compose. Column
  // order in each row is Message then Sound (matches the headers).
  const ANNOUNCE_TOGGLES = [
    {
      testid: "announce-join-message",
      key: "vc_appearance_entry_notifications",
      name: "Participant joins Message",
    },
    {
      testid: "announce-join-sound",
      key: "vc_appearance_entry_sound",
      name: "Participant joins Sound",
    },
    {
      testid: "announce-leave-message",
      key: "vc_appearance_exit_notifications",
      name: "Participant leaves Message",
    },
    {
      testid: "announce-leave-sound",
      key: "vc_appearance_exit_sound",
      name: "Participant leaves Sound",
    },
  ] as const;

  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    // The device-settings modal / Preferences panel only exists in the Dioxus UI.
    test.skip(!baseURL?.includes("3001"), "Preferences panel is Dioxus-only");
    await injectSessionCookie(context, { baseURL });
  });

  /**
   * Join the meeting as a solo host, then open the device-settings modal and
   * switch to the Preferences tab where the announcement matrix renders. Mirrors
   * the proven in-call open-settings sequence from
   * join-leave-notifications.spec.ts, but targets the Preferences tab (the
   * matrix lives in PreferencesSettingsPanel, not the Appearance tab).
   */
  async function openPreferencesPanel(
    page: Page,
    meetingId: string,
    username: string,
  ): Promise<void> {
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 60 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially(username, { delay: 60 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
    await page.waitForTimeout(1500);

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    const grid = page.locator("#grid-container");
    const state = await Promise.race([
      joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 30_000 }).then(() => "grid" as const),
    ]);
    if (state === "join") {
      await joinButton.click();
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });

    // Reveal the (possibly auto-hidden) action bar, then open device settings.
    await page.locator(".video-controls-container").hover();
    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });

    // The announcement matrix lives in the Preferences tab.
    await page.locator(".settings-nav-button").filter({ hasText: "Preferences" }).click();
    await expect(page.locator("#settings-panel-preferences")).toBeVisible({ timeout: 5_000 });
  }

  test("renders the 2x2 matrix structure and drops the legacy helper sentences", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    const meetingId = `e2e_announce_structure_${Date.now()}`;
    await openPreferencesPanel(page, meetingId, "announce-structure-user");

    const matrix = page.locator('[data-testid="announce-matrix"]');
    await expect(matrix).toHaveCount(1);
    await expect(matrix).toHaveAttribute("role", "group");
    await expect(matrix).toHaveAttribute("aria-label", "Participant announcements");

    // Exactly four channel toggles, one per testid.
    await expect(matrix.locator('input[type="checkbox"]')).toHaveCount(4);
    for (const { testid } of ANNOUNCE_TOGGLES) {
      await expect(matrix.locator(`[data-testid="${testid}"]`)).toHaveCount(1);
    }

    // Axis labels carry the per-cell meaning.
    await expect(matrix.locator("#announce-col-message")).toHaveText("Message");
    await expect(matrix.locator("#announce-col-sound")).toHaveText("Sound");
    await expect(matrix.locator("#announce-row-join")).toHaveText("Participant joins");
    await expect(matrix.locator("#announce-row-leave")).toHaveText("Participant leaves");

    // The four pre-1845 helper sentences are gone from the DOM.
    for (const sentence of [
      "Show a message when a participant joins.",
      "Show a message when a participant leaves.",
      "Play a sound when a participant joins.",
      "Play a sound when a participant leaves.",
    ]) {
      await expect(page.getByText(sentence)).toHaveCount(0);
    }
  });

  test("each toggle flips exactly its own preference key (swap regression guard)", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    const meetingId = `e2e_announce_parity_${Date.now()}`;
    await openPreferencesPanel(page, meetingId, "announce-parity-user");

    const matrix = page.locator('[data-testid="announce-matrix"]');

    // All four default to checked (enabled).
    for (const { testid } of ANNOUNCE_TOGGLES) {
      await expect(matrix.locator(`[data-testid="${testid}"]`)).toBeChecked();
    }

    const allKeys = ANNOUNCE_TOGGLES.map((t) => t.key);

    // Uncheck each toggle in isolation and assert ONLY its mapped key becomes
    // "false"; then restore it. Persistence is a 300ms-debounced effect and
    // writes all keys at once (context.rs save_appearance_settings_to_storage),
    // so a poll accommodates the debounce. If any two cells were cross-wired,
    // unchecking one would write the wrong key and this assertion would fail.
    for (const { testid, key } of ANNOUNCE_TOGGLES) {
      const input = matrix.locator(`[data-testid="${testid}"]`);
      const label = matrix.locator(`label.glow-switch:has([data-testid="${testid}"])`);

      await label.click();
      await expect(input).not.toBeChecked({ timeout: 5_000 });
      await expect
        .poll(() => page.evaluate((k) => localStorage.getItem(k), key), { timeout: 5_000 })
        .toBe("false");

      for (const other of allKeys) {
        if (other === key) continue;
        const value = await page.evaluate((k) => localStorage.getItem(k), other);
        expect(value, `${testid} must not disable ${other}`).not.toBe("false");
      }

      // Restore to a clean all-on baseline before the next toggle.
      await label.click();
      await expect(input).toBeChecked({ timeout: 5_000 });
      await expect
        .poll(() => page.evaluate((k) => localStorage.getItem(k), key), { timeout: 5_000 })
        .toBe("true");
    }
  });

  test("saved preferences round-trip onto the correct cells on load", async ({ page, context }) => {
    test.setTimeout(120_000);
    // Seed a mixed pattern in the CURRENT plain-text storage format (context.rs
    // writes bool.to_string() and reads value != "false") BEFORE navigation, so
    // the appearance context reads it during init. A swap in the load/checked
    // binding would light up the wrong cells here.
    await context.addInitScript(() => {
      localStorage.setItem("vc_appearance_entry_notifications", "false"); // joins · Message OFF
      localStorage.setItem("vc_appearance_exit_sound", "false"); // leaves · Sound OFF
      // entry_sound and exit_notifications left unset → default ON.
    });

    const meetingId = `e2e_announce_roundtrip_${Date.now()}`;
    await openPreferencesPanel(page, meetingId, "announce-roundtrip-user");

    const matrix = page.locator('[data-testid="announce-matrix"]');
    await expect(matrix.locator('[data-testid="announce-join-message"]')).not.toBeChecked();
    await expect(matrix.locator('[data-testid="announce-leave-sound"]')).not.toBeChecked();
    await expect(matrix.locator('[data-testid="announce-join-sound"]')).toBeChecked();
    await expect(matrix.locator('[data-testid="announce-leave-message"]')).toBeChecked();
  });

  test("channel help icons reveal on focus and dismiss on Escape", async ({ page }) => {
    test.setTimeout(120_000);
    const meetingId = `e2e_announce_help_${Date.now()}`;
    await openPreferencesPanel(page, meetingId, "announce-help-user");

    const matrix = page.locator('[data-testid="announce-matrix"]');

    const channels = [
      {
        help: "announce-help-message",
        tip: "announce-tip-message",
        ariaLabel: "About message announcements",
        text: "Show an on-screen message when someone joins or leaves.",
      },
      {
        help: "announce-help-sound",
        tip: "announce-tip-sound",
        ariaLabel: "About sound announcements",
        text: "Play a chime when someone joins or leaves.",
      },
    ];

    for (const channel of channels) {
      const help = matrix.locator(`[data-testid="${channel.help}"]`);
      const tip = page.locator(`#${channel.tip}`);

      await expect(help).toHaveAttribute("aria-label", channel.ariaLabel);
      await expect(help).toHaveAttribute("aria-describedby", channel.tip);
      await expect(tip).toHaveAttribute("role", "tooltip");
      await expect(tip).toHaveText(channel.text);

      // Hidden until focused; focus reveals it via CSS :focus-within.
      await expect(tip).toBeHidden();
      await help.focus();
      await expect(tip).toBeVisible();

      // Escape dismisses ONLY the tooltip: it must (a) hide, while (b) the
      // settings modal stays OPEN and (c) focus stays on the trigger. The
      // handler stops propagation so the modal's own Escape-to-close does not
      // fire, and does not blur. On the pre-fix code (Escape bubbled to the
      // modal handler and blurred the icon) the modal closed and focus fell to
      // <body>, so (b) and (c) fail — and the sound iteration then can't find
      // its icon. This pins the scoped-dismissal fix.
      await page.keyboard.press("Escape");
      await expect(tip).toBeHidden();
      await expect(page.locator("#device-settings-dialog")).toBeVisible();
      await expect(help).toBeFocused();
    }
  });

  test("each toggle exposes a distinct accessible name", async ({ page }) => {
    test.setTimeout(120_000);
    const meetingId = `e2e_announce_a11y_${Date.now()}`;
    await openPreferencesPanel(page, meetingId, "announce-a11y-user");

    const matrix = page.locator('[data-testid="announce-matrix"]');
    for (const { name } of ANNOUNCE_TOGGLES) {
      await expect(matrix.getByRole("checkbox", { name, exact: true })).toHaveCount(1);
    }
  });
});
