import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

async function navigateToMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
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

  if (result === "waiting" || result === "waiting-for-meeting") {
    return result;
  }

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);

  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

/**
 * Admit a guest from the waiting room if needed.
 * Returns once the guest is fully in the meeting (grid visible).
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

/**
 * Enable the local microphone by clicking the "Unmute" control button.
 * The MicButton uses accessible name from its tooltip span, so
 * getByRole("button", { name: "Unmute" }) reliably targets it.
 */
async function enableMic(page: Page): Promise<void> {
  const unmuteBtn = page.getByRole("button", { name: "Unmute" }).first();
  await expect(unmuteBtn).toBeVisible({ timeout: 10_000 });
  await unmuteBtn.click();
  // Brief settle so the state propagates before the next action.
  await page.waitForTimeout(500);
}

/**
 * Open the participant sidebar by clicking the "Open Peers" tooltip button.
 * The PeerListButton is in the secondary controls section which expands on
 * hover; Playwright's click mechanism triggers that hover automatically.
 */
async function openPeerList(page: Page): Promise<void> {
  const openPeersBtn = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Peers" }),
  });
  await openPeersBtn.click();
  await expect(page.locator("#peer-list-container.visible")).toBeVisible({
    timeout: 5_000,
  });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Host mute controls", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Test 1: Host mutes a single participant via the peer list item button.
   *
   * The `button.peer_item_mute_btn` appears only when:
   *   - the viewer is the host (is_owner = true), AND
   *   - the peer's audio_enabled is reported as true by diagnostics.
   * Therefore the guest must have their mic on before the host can see the
   * mute button in the sidebar.
   */
  test("host mutes a single participant", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostmute_single_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-mute@videocall.rs",
        "MuteHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-mute@videocall.rs",
        "MuteGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "MuteHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "MuteGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the peer connection to establish (host sees guest's tile).
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // Guest enables their microphone so the host's peer-list diagnostics
      // reflects audio_enabled=true and renders the mute button.
      await enableMic(guestPage);

      // ---- Host opens the participant sidebar ----
      await openPeerList(hostPage);

      // Host waits for the "Mute participant" button to appear.
      // The button only renders when diagnostics report the guest's mic is on
      // (typically visible within 1–2 diagnostic cycles, ~1–2 s after open).
      const mutePeerBtn = hostPage.getByTitle("Mute participant").first();
      await expect(mutePeerBtn).toBeVisible({ timeout: 15_000 });

      // ---- Host clicks the mute button ----
      await mutePeerBtn.dispatchEvent("click");

      // ---- Guest receives the host-mute NATS event and sees the toast ----
      const guestMuteToast = guestPage.locator(".peer-toast .toast-name", {
        hasText: "Host muted your microphone",
      });
      await expect(guestMuteToast.first()).toBeVisible({ timeout: 15_000 });

      // ---- Mute button disappears from host's view (peer is now muted) ----
      // Once the guest is muted the on_mute callback becomes None, so the
      // button is no longer rendered in the peer list item.
      await expect(hostPage.getByTitle("Mute participant")).toHaveCount(0, {
        timeout: 10_000,
      });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Test 2: "Mute all" broadcasts to every guest but NOT to the host.
   *
   * The host's VideoCallClient is constructed with on_host_mute: None when
   * is_owner=true, so the host never receives the mute callback even though
   * the NATS broadcast reaches their transport layer.
   */
  test("host mute-all mutes all guests but not the host", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostmute_all_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-muteall@videocall.rs",
        "MuteAllHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-muteall@videocall.rs",
        "MuteAllGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "MuteAllHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "MuteAllGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Enable mic on both sides so we can verify the host's stays active.
      await enableMic(guestPage);
      await enableMic(hostPage);

      // Confirm host mic is currently on — "Mute" tooltip means it's active.
      const hostActiveMicBtn = hostPage.getByRole("button", { name: "Mute" }).first();
      await expect(hostActiveMicBtn).toBeVisible({ timeout: 5_000 });

      // ---- Host clicks "Mute all" ----
      // The btn-mute-all button lives in HostControls which is always rendered
      // for the meeting owner — it is not subject to controls-nav auto-hide.
      const muteAllBtn = hostPage.locator("button.btn-mute-all");
      await expect(muteAllBtn).toBeVisible({ timeout: 10_000 });
      await muteAllBtn.click();

      // ---- Guest receives the NATS broadcast and sees the toast ----
      const guestMuteToast = guestPage.locator(".peer-toast .toast-name", {
        hasText: "Host muted your microphone",
      });
      await expect(guestMuteToast.first()).toBeVisible({ timeout: 15_000 });

      // ---- Host does NOT see the mute toast (on_host_mute is None for owner) ----
      // Assert immediately after the guest's toast confirmed propagation, so
      // the NATS event has had time to arrive at the host transport layer too.
      await expect(
        hostPage.locator(".peer-toast .toast-name", {
          hasText: "Host muted your microphone",
        }),
      ).toHaveCount(0);

      // ---- Host's own mic remains active after mute-all ----
      await expect(hostActiveMicBtn).toBeVisible();
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Test 3: A participant can self-unmute after being muted by the host.
   *
   * After the host mutes the guest, the guest's on_host_mute callback sets
   * mic_enabled=false. The guest can re-enable their mic by clicking the
   * "Unmute" button (same toggle they use for self-mute).
   */
  test("participant can self-unmute after being muted by host", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_selfunmute_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-selfunmute@videocall.rs",
        "SelfUnmuteHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-selfunmute@videocall.rs",
        "SelfUnmuteGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "SelfUnmuteHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "SelfUnmuteGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for peer connection
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // ---- Guest enables their mic ----
      await enableMic(guestPage);

      // Confirm guest mic is active ("Mute" tooltip visible = mic on).
      const guestMuteBtn = guestPage.getByRole("button", { name: "Mute" }).first();
      await expect(guestMuteBtn).toBeVisible({ timeout: 5_000 });

      // ---- Host mutes the guest via peer list ----
      await openPeerList(hostPage);
      const mutePeerBtn = hostPage.getByTitle("Mute participant").first();
      await expect(mutePeerBtn).toBeVisible({ timeout: 15_000 });
      await mutePeerBtn.dispatchEvent("click");

      // ---- Guest sees the mute toast ----
      const guestMuteToast = guestPage.locator(".peer-toast .toast-name", {
        hasText: "Host muted your microphone",
      });
      await expect(guestMuteToast.first()).toBeVisible({ timeout: 15_000 });

      // ---- Guest's mic button now shows "Unmute" (mic is off) ----
      const guestUnmuteBtn = guestPage.getByRole("button", { name: "Unmute" }).first();
      await expect(guestUnmuteBtn).toBeVisible({ timeout: 10_000 });

      // ---- Guest self-unmutes ----
      await guestUnmuteBtn.click();

      // ---- Guest's mic is active again — "Mute" button reappears ----
      await expect(guestPage.getByRole("button", { name: "Mute" }).first()).toBeVisible({
        timeout: 10_000,
      });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});
