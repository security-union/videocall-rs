import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E tests for the participant announcement toggles in the device-settings
 * Preferences panel (Notifications section — the 2×2 announcement matrix).
 *
 * The four cells map to their AppearanceSettings storage keys as:
 *   - joins · Message   -> `vc_appearance_entry_notifications`
 *   - leaves · Message  -> `vc_appearance_exit_notifications`
 *   - joins · Sound     -> `vc_appearance_entry_sound`
 *   - leaves · Sound    -> `vc_appearance_exit_sound`
 * Each defaults to `true` (enabled).
 *
 * The join (entry) toggles gate `on_peer_joined`; the leave (exit) toggles gate
 * `on_peer_left` (see attendants.rs). The toast UI renders inside the
 * `.peer-toasts` container with class `.peer-toast`.
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const ENTRY_NOTIFICATIONS_KEY = "vc_appearance_entry_notifications";
const EXIT_NOTIFICATIONS_KEY = "vc_appearance_exit_notifications";
const ENTRY_SOUND_KEY = "vc_appearance_entry_sound";
const EXIT_SOUND_KEY = "vc_appearance_exit_sound";

// These appearance keys are stored as PLAIN TEXT (see read_local_storage /
// write_local_storage in context.rs) — the app writes `bool.to_string()` and
// reads `value != "false"`. So the disabled value on the wire is literally
// "false", not a CBOR/zlib blob.
const STORED_FALSE = "false";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
];

async function createAuthenticatedContext(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  email: string,
  name: string,
  uiURL: string,
): Promise<BrowserContext> {
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

async function navigateToMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  // Display name is a controlled input -- clear before typing to handle any pre-fill
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });
  await page.waitForTimeout(1500);
}

/**
 * From the meeting page, wait for the meeting UI to load and click through
 * "Start Meeting" / "Join Meeting" to enter the grid.
 */
async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    return "waiting";
  }

  if (result === "waiting-for-meeting") {
    return "waiting-for-meeting";
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
 * Admit a guest from the waiting room if needed. Returns once the guest is
 * fully in the meeting (grid visible).
 */
async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (guestResult === "in-meeting") {
    return;
  }

  if (guestResult === "waiting") {
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
  }
}

test.describe("Entry/exit notifications preference", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("join notification appears by default", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_jln_default_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-jln-default@videocall.rs",
        "JlnHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-jln-default@videocall.rs",
        "JlnGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "JlnHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // With no stored preference the default (enabled) applies. The behavioral
      // assertion below (toast appears) is the real verification.

      // Start polling for the toast BEFORE the guest joins so we don't miss
      // it if PARTICIPANT_JOINED fires quickly.
      const hostJoinedToast = hostPage.locator(".peer-toast", {
        hasText: "joined the meeting",
      });
      const toastPromise = expect(hostJoinedToast.first()).toBeVisible({ timeout: 30_000 });

      // Guest joins the meeting
      await navigateToMeeting(guestPage, meetingId, "JlnGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Both should be in the meeting
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the toast to appear and verify the container is visible.
      await toastPromise;
      await expect(hostPage.locator(".peer-toasts")).toBeVisible();

      const firstToast = hostJoinedToast.first();
      await expect(firstToast.locator(".toast-action")).toContainText("joined the meeting");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("join notification suppressed when disabled", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_jln_off_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-jln-off@videocall.rs",
        "JlnOffHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-jln-off@videocall.rs",
        "JlnOffGuest",
        uiURL,
      );

      // Inject the disabled preference BEFORE any navigation so it is
      // present on first page load, before the appearance settings context
      // reads from localStorage during initialization. These appearance keys
      // are plain text (see context.rs read/write_local_storage), so the
      // disabled value on the wire is literally "false".
      await hostCtx.addInitScript((value: string) => {
        localStorage.setItem("vc_appearance_entry_notifications", value);
      }, STORED_FALSE);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "JlnOffHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Confirm the preference was actually persisted in this origin's
      // localStorage (init script ran).
      const stored = await hostPage.evaluate(
        (key) => localStorage.getItem(key),
        ENTRY_NOTIFICATIONS_KEY,
      );
      expect(stored).toBe(STORED_FALSE);

      // Guest joins the meeting
      await navigateToMeeting(guestPage, meetingId, "JlnOffGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Both in the meeting — wait for peer discovery so we can be sure the
      // PARTICIPANT_JOINED event was processed on the host side.
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the host to see the guest tile, which confirms the join
      // signal was received and routed through the attendant callbacks.
      const hostPeerTile = hostPage.locator("#grid-container .canvas-container");
      await expect(hostPeerTile.first()).toBeVisible({ timeout: 30_000 });

      // Now assert NO join toast was rendered. Give a brief grace window in
      // case the toast were to appear with a slight delay.
      const hostJoinedToast = hostPage.locator(".peer-toast", {
        hasText: "joined the meeting",
      });
      await hostPage.waitForTimeout(1500);
      await expect(hostJoinedToast).toHaveCount(0, { timeout: 5_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("toggle persists across navigation", async ({ baseURL }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_jln_toggle_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-jln-toggle@videocall.rs",
        "JlnToggleHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-jln-toggle@videocall.rs",
        "JlnToggleGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting (default: notifications ON)
      await navigateToMeeting(hostPage, meetingId, "JlnToggleHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Hover the action bar to reveal controls (autohide may be active)
      await hostPage.locator(".video-controls-container").hover();

      // Open the device settings modal from the bottom toolbar.
      await hostPage.locator('[data-testid="open-settings"]').click();
      await expect(hostPage.locator(".device-settings-modal")).toBeVisible({
        timeout: 10_000,
      });

      // Navigate to the Preferences tab (the notifications matrix lives here,
      // not on the Appearance tab).
      await hostPage.locator(".settings-nav-button").filter({ hasText: "Preferences" }).click();
      await expect(hostPage.locator("#settings-panel-preferences")).toBeVisible({
        timeout: 5_000,
      });

      // Locate the joins·Message toggle in the announcement matrix. Its stable
      // id is preserved; the visible switch is the wrapping `label.glow-switch`.
      const toggleInput = hostPage.locator("#entry-notifications-toggle");
      await expect(toggleInput).toBeVisible();
      await expect(toggleInput).toBeChecked();

      // Verify the section heading and matrix axis labels are present.
      await expect(
        hostPage
          .locator("#settings-panel-preferences .appearance-section-title")
          .filter({ hasText: "Notifications" }),
      ).toBeVisible();
      await expect(
        hostPage.locator('[data-testid="announce-matrix"] #announce-row-join'),
      ).toHaveText("Participant joins");
      await expect(
        hostPage.locator('[data-testid="announce-matrix"] #announce-col-message'),
      ).toHaveText("Message");

      // Uncheck the toggle. The checkbox is visually hidden behind a custom
      // switch UI, so click the wrapping `label.glow-switch`, which natively
      // forwards activation to the associated input.
      const toggleLabel = hostPage.locator("label.glow-switch:has(#entry-notifications-toggle)");
      await toggleLabel.click();
      await expect(toggleInput).not.toBeChecked({ timeout: 5_000 });

      // Verify the preference was persisted to localStorage as plain "false".
      await expect
        .poll(
          () => hostPage.evaluate((key) => localStorage.getItem(key), ENTRY_NOTIFICATIONS_KEY),
          {
            timeout: 5_000,
          },
        )
        .toBe(STORED_FALSE);

      // Close the settings modal.
      await hostPage.locator('button[aria-label="Close settings"]').click();
      await expect(hostPage.locator(".device-settings-modal")).not.toBeVisible({
        timeout: 5_000,
      });

      // Now the second user joins. The host should NOT see a join toast.
      await navigateToMeeting(guestPage, meetingId, "JlnToggleGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the host to see the guest tile so we know the join was
      // processed.
      const hostPeerTile = hostPage.locator("#grid-container .canvas-container");
      await expect(hostPeerTile.first()).toBeVisible({ timeout: 30_000 });

      // Brief grace window then assert no toast appeared.
      const hostJoinedToast = hostPage.locator(".peer-toast", {
        hasText: "joined the meeting",
      });
      await hostPage.waitForTimeout(1500);
      await expect(hostJoinedToast).toHaveCount(0, { timeout: 5_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("sounds toggle is present in settings", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_jls_present_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "sounds-host@videocall.rs",
        "SoundsHost",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();

      // Host starts the meeting alone.
      await navigateToMeeting(hostPage, meetingId, "SoundsHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Hover the action bar to reveal controls (autohide may be active)
      await hostPage.locator(".video-controls-container").hover();

      // Open the device settings modal from the bottom toolbar.
      await hostPage.locator('[data-testid="open-settings"]').click();
      await expect(hostPage.locator(".device-settings-modal")).toBeVisible({
        timeout: 10_000,
      });

      // Navigate to the Preferences tab (the notifications matrix lives here).
      await hostPage.locator(".settings-nav-button").filter({ hasText: "Preferences" }).click();
      await expect(hostPage.locator("#settings-panel-preferences")).toBeVisible({
        timeout: 5_000,
      });

      // Both sound toggle inputs should be present (visually hidden behind
      // the custom switch UI, so use a presence check rather than
      // `toBeVisible`).
      await expect(hostPage.locator("#entry-sound-toggle")).toHaveCount(1);
      await expect(hostPage.locator("#exit-sound-toggle")).toHaveCount(1);

      // The Sound-column toggles are named via aria-labelledby from the row and
      // column labels — one distinct accessible name per row.
      await expect(
        hostPage.getByRole("checkbox", { name: "Participant joins Sound", exact: true }),
      ).toHaveCount(1);
      await expect(
        hostPage.getByRole("checkbox", { name: "Participant leaves Sound", exact: true }),
      ).toHaveCount(1);
    } finally {
      await browser1.close();
    }
  });

  test("disabling sounds does not suppress the toast", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_jls_no_sounds_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "sounds-host@videocall.rs",
        "SoundsHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "sounds-guest@videocall.rs",
        "SoundsGuest",
        uiURL,
      );

      // Pre-seed the entry sound preference to "false" BEFORE any navigation so
      // it is present on first page load. The entry message preference is left
      // at its default (true), so the join toast should still appear --
      // verifying the message and sound toggles are independent. These keys are
      // plain text (see context.rs), so the disabled value is literally "false".
      await hostCtx.addInitScript((value: string) => {
        localStorage.setItem("vc_appearance_entry_sound", value);
      }, STORED_FALSE);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting.
      await navigateToMeeting(hostPage, meetingId, "SoundsHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // The behavioral assertions below (toast appears despite the entry sound
      // being off) are the real verification that message and sound are
      // independent. Sound playback itself has no DOM signal to assert on.

      // Start polling for the toast BEFORE the guest joins so we don't miss
      // it if PARTICIPANT_JOINED fires quickly.
      const hostJoinedToast = hostPage.locator(".peer-toast", {
        hasText: "joined the meeting",
      });
      const toastPromise = expect(hostJoinedToast.first()).toBeVisible({ timeout: 30_000 });

      // Guest joins the meeting.
      await navigateToMeeting(guestPage, meetingId, "SoundsGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the host to see the guest tile, which confirms the join
      // signal was received and routed through the attendant callbacks.
      const hostPeerTile = hostPage.locator("#grid-container .canvas-container");
      await expect(hostPeerTile.first()).toBeVisible({ timeout: 30_000 });

      // The toast MUST still appear -- sounds and toasts are independent.
      await toastPromise;
      await expect(hostPage.locator(".peer-toasts")).toBeVisible();

      const firstToast = hostJoinedToast.first();
      await expect(firstToast.locator(".toast-action")).toContainText("joined the meeting");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("sounds toggle persists to localStorage", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_jls_persist_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "sounds-host@videocall.rs",
        "SoundsHost",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();

      // Host starts the meeting alone.
      await navigateToMeeting(hostPage, meetingId, "SoundsHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Hover the action bar to reveal controls (autohide may be active)
      await hostPage.locator(".video-controls-container").hover();

      // Open the device settings modal from the bottom toolbar.
      await hostPage.locator('[data-testid="open-settings"]').click();
      await expect(hostPage.locator(".device-settings-modal")).toBeVisible({
        timeout: 10_000,
      });

      // Navigate to the Preferences tab (the notifications matrix lives here).
      await hostPage.locator(".settings-nav-button").filter({ hasText: "Preferences" }).click();
      await expect(hostPage.locator("#settings-panel-preferences")).toBeVisible({
        timeout: 5_000,
      });

      // The entry sound toggle should default to checked (enabled).
      const soundsToggleInput = hostPage.locator("#entry-sound-toggle");
      await expect(soundsToggleInput).toBeChecked();

      // Click the wrapping `label.glow-switch` to uncheck the toggle (the
      // checkbox is visually hidden behind the custom switch UI).
      const soundsToggleLabel = hostPage.locator("label.glow-switch:has(#entry-sound-toggle)");
      await soundsToggleLabel.click();
      await expect(soundsToggleInput).not.toBeChecked({ timeout: 5_000 });

      // Close the settings modal.
      await hostPage.locator('button[aria-label="Close settings"]').click();
      await expect(hostPage.locator(".device-settings-modal")).not.toBeVisible({
        timeout: 5_000,
      });

      // Verify the preference was persisted to localStorage as plain "false".
      await expect
        .poll(() => hostPage.evaluate((key) => localStorage.getItem(key), ENTRY_SOUND_KEY), {
          timeout: 5_000,
        })
        .toBe(STORED_FALSE);
    } finally {
      await browser1.close();
    }
  });

  test("exit sound toggle persists to localStorage", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_jls_exit_persist_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "exit-sound-host@videocall.rs",
        "ExitSoundHost",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();

      // Host starts the meeting alone.
      await navigateToMeeting(hostPage, meetingId, "ExitSoundHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Hover the action bar to reveal controls (autohide may be active).
      await hostPage.locator(".video-controls-container").hover();

      // Open the device settings modal and go to the Appearance tab.
      await hostPage.locator('[data-testid="open-settings"]').click();
      await expect(hostPage.locator(".device-settings-modal")).toBeVisible({
        timeout: 10_000,
      });
      await hostPage.locator(".settings-nav-button").filter({ hasText: "Preferences" }).click();
      await expect(hostPage.locator("#settings-panel-preferences")).toBeVisible({
        timeout: 5_000,
      });

      // The exit sound toggle should default to checked (enabled).
      const exitSoundInput = hostPage.locator("#exit-sound-toggle");
      await expect(exitSoundInput).toBeChecked();

      // Click the wrapping `label.glow-switch` to uncheck it (the checkbox is
      // visually hidden behind the custom switch UI).
      await hostPage.locator("label.glow-switch:has(#exit-sound-toggle)").click();
      await expect(exitSoundInput).not.toBeChecked({ timeout: 5_000 });

      // Close the settings modal.
      await hostPage.locator('button[aria-label="Close settings"]').click();
      await expect(hostPage.locator(".device-settings-modal")).not.toBeVisible({
        timeout: 5_000,
      });

      // Verify the exit-sound preference was persisted (plain "false"), and
      // that toggling it did NOT touch the entry-sound key.
      await expect
        .poll(() => hostPage.evaluate((key) => localStorage.getItem(key), EXIT_SOUND_KEY), {
          timeout: 5_000,
        })
        .toBe(STORED_FALSE);
      const entrySoundStored = await hostPage.evaluate(
        (key) => localStorage.getItem(key),
        ENTRY_SOUND_KEY,
      );
      expect(entrySoundStored).not.toBe(STORED_FALSE);
    } finally {
      await browser1.close();
    }
  });

  test("disabling the exit message leaves the entry message firing", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_jln_exit_off_entry_on_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    // This is independence guarantee, exercised in the direction the e2e
    // harness can reliably observe: the join toast.
    //
    // We disable ONLY the exit message and leave the entry message at its
    // default (enabled). The host must STILL see a join toast when a guest
    // arrives. If `on_peer_joined` were cross-wired to the exit flag (or the
    // two directions shared a single flag, the pre-split behavior), disabling
    // exit would suppress the join toast and this test would fail -- so it is
    // mutation-sensitive to a regression of the split. Its complement, the
    // "join notification suppressed when disabled" test above, seeds the entry
    // key and asserts the join toast is gone; together they pin both directions.
    //
    // The mirror behavioral direction (exit message fires while entry is off)
    // cannot be asserted in e2e: the "left the meeting" toast is currently
    // suppressed in the harness by a videocall-client callback-ordering bug --
    // see the two skipped leave-toast tests in toast-notifications.spec.ts. The
    // exit gating + legacy migration are instead pinned by the pure-function
    // unit tests in context.rs (notification_prefs_*).
    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-exitoff@videocall.rs",
        "ExitOffHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-exitoff@videocall.rs",
        "ExitOffGuest",
        uiURL,
      );

      // Disable ONLY the exit message before load (plain-text "false").
      await hostCtx.addInitScript((value: string) => {
        localStorage.setItem("vc_appearance_exit_notifications", value);
      }, STORED_FALSE);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting.
      await navigateToMeeting(hostPage, meetingId, "ExitOffHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Confirm the seed actually landed as the disabled value the app reads
      // (guards against a re-encoding regression making the seed a no-op).
      const exitStored = await hostPage.evaluate(
        (key) => localStorage.getItem(key),
        EXIT_NOTIFICATIONS_KEY,
      );
      expect(exitStored).toBe(STORED_FALSE);

      // Start polling for the join toast BEFORE the guest joins so we don't
      // miss it if PARTICIPANT_JOINED fires quickly.
      const hostJoinedToast = hostPage.locator(".peer-toast", {
        hasText: "joined the meeting",
      });
      const toastPromise = expect(hostJoinedToast.first()).toBeVisible({ timeout: 30_000 });

      // Guest joins and is admitted.
      await navigateToMeeting(guestPage, meetingId, "ExitOffGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // The join toast MUST still appear despite the exit message being off.
      await toastPromise;
      const firstToast = hostJoinedToast.first();
      await expect(firstToast.locator(".toast-action")).toContainText("joined the meeting");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});
