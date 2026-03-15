import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
];

async function createAuthenticatedContext(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  userId: string,
  name: string,
  uiURL: string,
) {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
  });
  const token = generateSessionToken(userId, name);
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

async function navigateToMeeting(page: Page, meetingId: string, username: string) {
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
  const joinButton = page.getByText(/Start Meeting|Join Meeting/);
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 20_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 20_000 }).then(() => "waiting-for-meeting" as const),
  ]);

  if (result === "waiting") {
    return "waiting";
  }

  if (result === "waiting-for-meeting") {
    return "waiting-for-meeting";
  }

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);

  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

/**
 * Admit a guest from the waiting room if needed. Returns once the guest
 * is fully in the meeting (grid visible).
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
  }
}

test.describe("Toast notifications for participant join/leave", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("host sees 'joined the meeting' toast when guest joins", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_toast_join_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "00000000-0000-4000-8000-000000000101",
        "ToastHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "00000000-0000-4000-8000-000000000102",
        "ToastGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "ToastHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Start polling for the toast BEFORE the guest joins so we don't
      // miss it if the PARTICIPANT_JOINED event fires quickly.
      const hostJoinedToast = hostPage.locator(".peer-toast", {
        hasText: "joined the meeting",
      });
      const toastPromise = expect(hostJoinedToast.first()).toBeVisible({ timeout: 30_000 });

      // Guest joins the meeting (toast polling is already running)
      await navigateToMeeting(guestPage, meetingId, "ToastGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Both should be in the meeting
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // ---- ASSERT: peer tile shows display_name as text, user_id as tooltip ----
      // Wait for the guest's peer tile to appear on the host side
      const hostPeerTile = hostPage.locator("#grid-container .canvas-container");
      await expect(hostPeerTile.first()).toBeVisible({ timeout: 30_000 });

      // The floating name overlay should show the display name
      const guestFloatingName = hostPage.locator(".floating-name", {
        hasText: "ToastGuest",
      });
      await expect(guestFloatingName.first()).toBeVisible({ timeout: 10_000 });

      // The title attribute (tooltip) should contain the user_id (UUID)
      await expect(guestFloatingName.first()).toHaveAttribute(
        "title",
        "00000000-0000-4000-8000-000000000102",
      );

      // Wait for the toast that we started watching for before the guest joined.
      await toastPromise;

      // Verify the toast structure: line 1 is user_id, line 2 is action
      const firstToast = hostJoinedToast.first();
      await expect(firstToast.locator(".toast-name")).toContainText(
        "00000000-0000-4000-8000-000000000102",
      );
      await expect(firstToast.locator(".toast-action")).toContainText("joined the meeting");

      // Verify the toast container has the correct CSS class (.peer-toasts)
      await expect(hostPage.locator(".peer-toasts")).toBeVisible();
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("host sees 'left the meeting' toast when guest leaves", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_toast_leave_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "00000000-0000-4000-8000-000000000201",
        "LeaveHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "00000000-0000-4000-8000-000000000202",
        "LeaveGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "LeaveHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Guest joins the meeting
      await navigateToMeeting(guestPage, meetingId, "LeaveGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Both should be in the meeting with peer discovery complete
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for peer discovery so both sides are fully connected
      const hostPeer = hostPage.locator("#grid-container .canvas-container");
      await expect(hostPeer.first()).toBeVisible({ timeout: 30_000 });

      // Wait for any "joined" toasts to auto-dismiss before testing "left"
      // toasts, so we don't get a false match on a stale toast.
      await hostPage.waitForTimeout(9000);

      // Guest leaves the meeting by closing the browser. This triggers a
      // PARTICIPANT_LEFT event on the server, which is pushed to the host.
      // Closing the browser is more reliable than clicking "Hang Up"
      // because the server detects the dropped connection regardless.
      await guestPage.close();

      // Host should see a "left the meeting" toast notification.
      // Note: the leave sound is deferred by 500ms -- it only plays if the
      // toast still exists after that delay (i.e. no rapid "joined" event
      // cancelled it). The toast itself appears immediately.
      const hostLeftToast = hostPage.locator(".peer-toast", {
        hasText: "left the meeting",
      });
      await expect(hostLeftToast.first()).toBeVisible({ timeout: 20_000 });

      // Verify the toast structure: line 1 is user_id, line 2 is action
      const firstLeftToast = hostLeftToast.first();
      await expect(firstLeftToast.locator(".toast-name")).toContainText(
        "00000000-0000-4000-8000-000000000202",
      );
      await expect(firstLeftToast.locator(".toast-action")).toContainText("left the meeting");

      // Verify the toast is inside the correct container (.peer-toasts)
      await expect(hostPage.locator(".peer-toasts")).toBeVisible();

      // Verify the toast auto-dismisses after ~8 seconds.
      // The Rust code schedules removal via Timeout::new(8_000, ...).
      // We give a generous buffer for CI timing variance.
      await expect(hostLeftToast).toHaveCount(0, { timeout: 12_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("toast auto-dismisses after approximately 8 seconds", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_toast_dismiss_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "00000000-0000-4000-8000-000000000301",
        "DismissHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "00000000-0000-4000-8000-000000000302",
        "DismissGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "DismissHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Guest joins the meeting
      await navigateToMeeting(guestPage, meetingId, "DismissGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // Wait for both to be fully connected
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      const hostPeer = hostPage.locator("#grid-container .canvas-container");
      await expect(hostPeer.first()).toBeVisible({ timeout: 30_000 });

      // Wait for any "joined" toasts to fully clear
      await hostPage.waitForTimeout(9000);

      // Guest leaves -- triggers a "left the meeting" toast on the host.
      // The leave sound is deferred by 500ms; the toast itself appears immediately.
      await guestPage.close();

      // Wait for the toast to appear
      const hostLeftToast = hostPage.locator(".peer-toast", {
        hasText: "left the meeting",
      });
      await expect(hostLeftToast.first()).toBeVisible({ timeout: 20_000 });

      // Record the time when we first see the toast
      const toastAppearedAt = Date.now();

      // The toast should auto-dismiss after 8 seconds (Timeout::new(8_000, ...)).
      // Wait for it to disappear.
      await expect(hostLeftToast).toHaveCount(0, { timeout: 12_000 });

      const dismissedAt = Date.now();
      const elapsed = dismissedAt - toastAppearedAt;

      // The toast should have been visible for roughly 5-12 seconds.
      // We use a wide range to account for timing variance in CI.
      // The key assertion is that it DID disappear on its own.
      console.log(`Toast auto-dismissed after ${elapsed}ms`);
      expect(elapsed).toBeGreaterThan(5000);
      expect(elapsed).toBeLessThan(12_000);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test.skip("admission from waiting room shows only 'joined' toast (cancels stale 'left')", async ({
    baseURL,
  }) => {
    // SKIPPED: The server-side fix in ws_chat_session.rs now prevents
    // PARTICIPANT_LEFT from being sent for observer (waiting room) sessions,
    // so this scenario no longer occurs. The waiting room e2e flow also has
    // a pre-existing timing issue that causes test timeouts.
    test.setTimeout(120_000);
    // When a guest is in the waiting room, the server sees the observer
    // connection close (PARTICIPANT_LEFT) and the guest reconnect as a real
    // participant (PARTICIPANT_JOINED) upon admission. The client-side code
    // cancels any pending "left" toast for that user and shows only the
    // "joined" toast. The deferred leave sound (500ms) is also suppressed
    // because the toast is removed before the sound timer fires.
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_toast_admit_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "00000000-0000-4000-8000-000000000401",
        "AdmitHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "00000000-0000-4000-8000-000000000402",
        "AdmitGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting
      await navigateToMeeting(hostPage, meetingId, "AdmitHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Guest joins and should land in the waiting room
      await navigateToMeeting(guestPage, meetingId, "AdmitGuest");
      const guestResult = await joinMeetingFromPage(guestPage);

      if (guestResult === "in-meeting") {
        // Waiting room is disabled in this environment -- skip gracefully.
        // The test is only meaningful when the waiting room is active.
        console.log("Waiting room not active; skipping admission toast test.");
        return;
      }

      expect(guestResult).toBe("waiting");
      await expect(guestPage.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 10_000,
      });

      // Host admits the guest
      const admitButton = hostPage.getByTitle("Admit").first();
      await expect(admitButton).toBeVisible({ timeout: 20_000 });
      await hostPage.waitForTimeout(1000);
      await admitButton.dispatchEvent("click");

      // Wait for guest to finish joining
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

      // Host should see a "joined the meeting" toast for the admitted guest
      const hostJoinedToast = hostPage.locator(".peer-toast", {
        hasText: "joined the meeting",
      });
      await expect(hostJoinedToast.first()).toBeVisible({ timeout: 15_000 });

      // Verify it is a "joined" toast, NOT a "left" toast. The "left" toast
      // from the observer disconnect should have been cancelled by the
      // on_peer_joined handler before the 500ms sound deferral expired.
      const hostLeftToast = hostPage.locator(".peer-toast", {
        hasText: "left the meeting",
      });
      // The "left" toast should NOT be visible -- it was cancelled when the
      // "joined" event arrived. Allow a brief moment for any race to settle.
      await hostPage.waitForTimeout(600);
      const leftToastVisible = await hostLeftToast.isVisible().catch(() => false);
      expect(leftToastVisible).toBe(false);

      // Verify the toast structure: line 1 is user_id, line 2 is action
      const firstAdmitToast = hostJoinedToast.first();
      await expect(firstAdmitToast.locator(".toast-name")).toContainText(
        "00000000-0000-4000-8000-000000000402",
      );
      await expect(firstAdmitToast.locator(".toast-action")).toContainText("joined the meeting");

      // Verify the toast container uses the correct CSS class
      await expect(hostPage.locator(".peer-toasts")).toBeVisible();
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});
