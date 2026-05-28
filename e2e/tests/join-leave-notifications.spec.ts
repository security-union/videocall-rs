import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E tests for the "Entry/exit notifications" preference toggle in the
 * Appearance settings (Preferences section).
 *
 * The preference is persisted in localStorage under the key
 * `vc_appearance_join_leave_notifications` and defaults to `true` (enabled).
 *
 * When enabled: peer-joined and peer-left toasts appear (rendered inside the
 * `.peer-toasts` container with class `.peer-toast`) and the corresponding
 * join/leave sounds play.
 *
 * When disabled: both the toast UI and the sound are suppressed entirely
 * inside `on_peer_joined` / `on_peer_left` (see attendants.rs).
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const STORAGE_KEY = "vc_appearance_join_leave_notifications";
const SOUNDS_STORAGE_KEY = "vc_appearance_join_leave_sounds";

// dioxus_sdk_storage serializes values as CBOR + zlib + hex. This is the
// encoded form of the Rust string literal "false".
const ENCODED_FALSE = "78da4b4d4bcc294e050008750271";

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

      // Note: dioxus_sdk_storage serializes values as zlib-compressed serde
      // blobs, so raw localStorage.getItem() does NOT return plain "true"/"false".
      // The behavioral assertion below (toast appears) is the real verification.

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

      // Allow peer discovery to propagate through signaling
      await hostPage.waitForTimeout(5000);

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
      // reads from localStorage during initialization.
      // dioxus_sdk_storage uses CBOR+zlib+hex encoding.
      await hostCtx.addInitScript((encoded: string) => {
        localStorage.setItem("vc_appearance_join_leave_notifications", encoded);
      }, ENCODED_FALSE);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "JlnOffHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Confirm the preference was actually persisted in this origin's
      // localStorage (init script ran).
      const stored = await hostPage.evaluate((key) => localStorage.getItem(key), STORAGE_KEY);
      expect(stored).toBe(ENCODED_FALSE);

      // Guest joins the meeting
      await navigateToMeeting(guestPage, meetingId, "JlnOffGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Both in the meeting — wait for peer discovery so we can be sure the
      // PARTICIPANT_JOINED event was processed on the host side.
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Allow peer discovery to propagate through signaling
      await hostPage.waitForTimeout(5000);

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

      // Navigate to the Appearance tab.
      await hostPage.locator(".settings-nav-button").filter({ hasText: "Appearance" }).click();
      await expect(hostPage.locator("#settings-panel-appearance")).toBeVisible({
        timeout: 5_000,
      });

      // Locate the "Entry/exit notifications" toggle in the Preferences
      // section. The input has a stable id and the visible label points
      // to it via `for=`.
      const toggleInput = hostPage.locator("#join-leave-notifications-toggle");
      await expect(toggleInput).toBeVisible();
      await expect(toggleInput).toBeChecked();

      // Verify the section heading is present.
      await expect(
        hostPage
          .locator("#settings-panel-appearance .appearance-section-title")
          .filter({ hasText: "Preferences" }),
      ).toBeVisible();
      await expect(
        hostPage
          .locator('#settings-panel-appearance label[for="join-leave-notifications-toggle"]')
          .filter({ hasText: "Entry/exit notifications" }),
      ).toBeVisible();

      // Uncheck the toggle. The input is hidden behind a custom switch UI,
      // so use the label's click target (the surrounding `.glow-switch`
      // label) which forwards activation to the input.
      const toggleSwitch = hostPage.locator(
        '#settings-panel-appearance label[aria-label="Toggle entry and exit notifications"]',
      );
      await toggleSwitch.click();
      await expect(toggleInput).not.toBeChecked({ timeout: 5_000 });

      // Verify the preference was persisted to localStorage.
      await expect
        .poll(() => hostPage.evaluate((key) => localStorage.getItem(key), STORAGE_KEY), {
          timeout: 5_000,
        })
        .toBe(ENCODED_FALSE);

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

      // Allow peer discovery to propagate through signaling
      await hostPage.waitForTimeout(5000);

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

      // Navigate to the Appearance tab.
      await hostPage.locator(".settings-nav-button").filter({ hasText: "Appearance" }).click();
      await expect(hostPage.locator("#settings-panel-appearance")).toBeVisible({
        timeout: 5_000,
      });

      // The sounds toggle input should be present (visually hidden behind
      // the custom switch UI, so use a presence check rather than
      // `toBeVisible`).
      const soundsToggleInput = hostPage.locator("#join-leave-sounds-toggle");
      await expect(soundsToggleInput).toHaveCount(1);

      // The accessible label must exist and be visible -- it's the
      // user-facing click target for the toggle.
      const soundsToggleLabel = hostPage.locator(
        '#settings-panel-appearance label[aria-label="Toggle entry and exit sounds"]',
      );
      await expect(soundsToggleLabel).toBeVisible();
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

      // Pre-seed the sounds preference to "false" BEFORE any navigation so
      // it is present on first page load. The notifications (toasts)
      // preference is left at its default (true), so toasts should still
      // appear -- verifying the two toggles are independent.
      // dioxus_sdk_storage uses CBOR+zlib+hex encoding.
      await hostCtx.addInitScript((encoded: string) => {
        localStorage.setItem("vc_appearance_join_leave_sounds", encoded);
      }, ENCODED_FALSE);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting.
      await navigateToMeeting(hostPage, meetingId, "SoundsHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Note: dioxus_sdk_storage serializes values as zlib-compressed serde
      // blobs, so raw localStorage.getItem() does NOT return plain strings.
      // The behavioral assertions below (toast appears despite sounds off)
      // are the real verification that the two toggles are independent.

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

      // Allow peer discovery to propagate through signaling
      await hostPage.waitForTimeout(5000);

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

      // Navigate to the Appearance tab.
      await hostPage.locator(".settings-nav-button").filter({ hasText: "Appearance" }).click();
      await expect(hostPage.locator("#settings-panel-appearance")).toBeVisible({
        timeout: 5_000,
      });

      // The sounds toggle should default to checked (enabled).
      const soundsToggleInput = hostPage.locator("#join-leave-sounds-toggle");
      await expect(soundsToggleInput).toBeChecked();

      // Click the label to uncheck the toggle (input is visually hidden
      // behind the custom switch UI).
      const soundsToggleLabel = hostPage.locator(
        '#settings-panel-appearance label[aria-label="Toggle entry and exit sounds"]',
      );
      await soundsToggleLabel.click();
      await expect(soundsToggleInput).not.toBeChecked({ timeout: 5_000 });

      // Close the settings modal.
      await hostPage.locator('button[aria-label="Close settings"]').click();
      await expect(hostPage.locator(".device-settings-modal")).not.toBeVisible({
        timeout: 5_000,
      });

      // Verify the preference was persisted to localStorage.
      await expect
        .poll(() => hostPage.evaluate((key) => localStorage.getItem(key), SOUNDS_STORAGE_KEY), {
          timeout: 5_000,
        })
        .toBe(ENCODED_FALSE);
    } finally {
      await browser1.close();
    }
  });
});
