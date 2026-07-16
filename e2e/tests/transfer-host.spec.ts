import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

/**
 * E2E coverage for the transfer-host feature.
 *
 * Host capability is authoritative in the DB (`meeting_participants.is_host`)
 * and surfaced to clients via the room-access JWT. A host can transfer host
 * (promote the target + step down atomically). The transfer is broadcast as
 * `HOST_GRANTED` / `HOST_REVOKED`:
 *
 *   - The TARGET re-fetches its status so the `is_owner` prop flips and host UI
 *     appears WITHOUT a reload or rejoin (the meeting view re-renders in place —
 *     no remount), including a one-shot "You are now a host" toast.
 *   - EVERY client (incl. the target) updates the live host set so the
 *     promoted peer's `(Host)` indicator / crown appears immediately.
 *
 * The action lives in the peer-list sidebar's per-row three-dot menu
 * (`peer_list_item.rs`) and fires immediately (no confirmation dialog).
 */

// ---------------------------------------------------------------------------
// Shared helpers (mirrors host-kick.spec.ts / host-controls-menu-ux.spec.ts)
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
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);
}

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

  if (result === "waiting" || result === "waiting-for-meeting") {
    return result;
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

/** Admit a waiting participant from the host's waiting-room controls. */
async function admitIfNeeded(
  hostPage: Page,
  participantPage: Page,
  result: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (result === "in-meeting") {
    return;
  }
  if (result === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.click();
    await hostPage.waitForTimeout(3000);

    const joinButton = participantPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
    const grid = participantPage.locator("#grid-container");
    const postAdmit = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (postAdmit === "join") {
      await participantPage.waitForTimeout(1000);
      await joinButton.click();
      await participantPage.waitForTimeout(3000);
      await expect(grid).toBeVisible({ timeout: 15_000 });
    }
  }
}

/**
 * Open the peer-list sidebar via the "Open Peers" video-controls button. The
 * controls bar auto-hides, so wake it with a hover + mouse move first.
 */
async function openPeerListSidebar(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await wakeControls(page);
  await page.waitForTimeout(300);

  const openPeersBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Open Peers" }),
  });
  await expect(openPeersBtn).toBeVisible({ timeout: 10_000 });
  await openPeersBtn.click();
  await page.waitForTimeout(800);
}

/**
 * Open a remote peer's three-dot menu in the peer-list sidebar and click a
 * host action by its label. `hasText` on `.peer_item` scopes to the row that
 * shows the peer's display name (the host's own self row carries the host's
 * name, so a distinct peer name targets the correct row). The actions fire
 * immediately — there is no confirmation dialog.
 */
async function clickPeerRowAction(page: Page, peerName: string, actionText: string): Promise<void> {
  const row = page.locator(".peer_item", { hasText: peerName });
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.locator(".peer_item_menu_btn").click();

  const item = page.locator(".peer_item_context_menu .context-menu-item", { hasText: actionText });
  await expect(item).toBeVisible({ timeout: 5_000 });
  await item.click();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const HOST_NAME = "TransferHostOwner";
const PEER_NAME = "TransferHostPeer";

test.describe("Transfer host", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Transfer host — the target becomes host and the original host steps
   * down atomically. After the transfer the original host's self row is no
   * longer "(You/Host)" (they are a plain participant) and the target's self row
   * becomes "(You/Host)".
   */
  test("host transfers host and steps down", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_transfer_host_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "transfer-owner@videocall.rs",
        HOST_NAME,
        uiURL,
      );
      const peerCtx = await createAuthenticatedContext(
        browser2,
        "transfer-peer@videocall.rs",
        PEER_NAME,
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const peerPage = await peerCtx.newPage();

      await navigateToMeeting(hostPage, meetingId, HOST_NAME);
      expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");
      await navigateToMeeting(peerPage, meetingId, PEER_NAME);
      await admitIfNeeded(hostPage, peerPage, await joinMeetingFromPage(peerPage));

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(peerPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // ---- Host transfers host to the participant ----
      await openPeerListSidebar(hostPage);
      await clickPeerRowAction(hostPage, PEER_NAME, "Transfer host");

      // ---- Target becomes host ----
      await openPeerListSidebar(peerPage);
      await expect(
        peerPage.locator(".peer_item", { hasText: PEER_NAME }).locator(".peer-indicator"),
      ).toHaveText("(You/Host)", { timeout: 30_000 });

      // ---- Original host steps down: own self row is now just "(You)" and the
      // in-call host-actions menu is gone (updated in place, no reload). ----
      await expect(
        hostPage.locator(".peer_item", { hasText: HOST_NAME }).first().locator(".peer-indicator"),
      ).toHaveText("(You)", { timeout: 30_000 });
      await expect(
        hostPage.locator('.in-call-header button.menu-button[aria-label="Host actions"]'),
      ).toHaveCount(0, { timeout: 10_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});
