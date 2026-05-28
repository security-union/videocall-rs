import { test, expect, Page, BrowserContext } from "@playwright/test";
import { chromium } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Signal-meter popup — survives a peer leave (HCL bug #8).
 *
 * Pre-fix behaviour: every open signal-meter popup in the host's grid
 * collapsed the instant ANY peer left the meeting. The host's per-tile
 * `show_signal_popup = use_signal(false)` was owned by the leaving
 * peer's PeerTile component, but the parent re-render also tore down
 * the sibling PeerTiles' state because the for-loop's key set changed.
 *
 * Post-fix behaviour:
 *   - Popup state is owned by a parent-level `SignalPopupStateMap`
 *     context keyed on `(peer_id, meter_mode)`.
 *   - When a peer leaves, `on_peer_removed` strips ONLY that peer's
 *     entries from the map; every other peer's popup survives the
 *     parent re-render untouched.
 *
 * What this spec asserts:
 *
 *   1. Host opens signal-meter popups for both peers.
 *   2. Two popups are visible at once.
 *   3. Peer A leaves the meeting (page.close()).
 *   4. Peer A's popup is gone (the anchored peer left).
 *   5. Peer B's popup is STILL visible — the leave did NOT clobber it.
 *
 * Mirrors the auth + meeting setup in `peer-signal-popup-portal.spec.ts`.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

async function joinMeetingAs(
  context: BrowserContext,
  meetingId: string,
  username: string,
): Promise<Page> {
  const page = await context.newPage();
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

  return page;
}

async function clickJoinAndEnterGrid(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "join") {
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

async function admitGuestIfNeeded(hostPage: Page, guestPage: Page): Promise<void> {
  const joinButton = guestPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = guestPage.getByText("Waiting to be admitted");
  const guestGrid = guestPage.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    guestGrid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);
  }

  if (result !== "auto-joined") {
    await clickJoinAndEnterGrid(guestPage);
  } else {
    await expect(guestGrid).toBeVisible({ timeout: 15_000 });
  }
}

test.describe("Signal-meter popup — survives peer leave (HCL bug #8)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("opening two popups, one peer leaves: other popup stays open", async ({ baseURL }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_sigm_leave_${Date.now()}`;

    // Three browsers: host + 2 peers.
    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-sigml@videocall.rs", name: "SigMLHost" },
        { email: "guest1-sigml@videocall.rs", name: "SigMLGuest1" },
        { email: "guest2-sigml@videocall.rs", name: "SigMLGuest2" },
      ];

      for (let i = 0; i < 3; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      // Host joins first, then admits each guest.
      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);

      for (let i = 1; i < 3; i++) {
        members[i].page = await joinMeetingAs(members[i].context, meetingId, profiles[i].name);
        await admitGuestIfNeeded(members[0].page, members[i].page);
      }

      // Let the mesh settle so both guest tiles + their signal-meter
      // buttons are mounted on the host side.
      await members[0].page.waitForTimeout(12_000);

      const hostPage = members[0].page;
      const signalButtons = hostPage.locator(
        '#grid-container button[aria-label="Show signal quality"]',
      );
      await expect(signalButtons).toHaveCount(2, { timeout: 30_000 });

      // Open both popups.
      await signalButtons.nth(0).click();
      const popups = hostPage.locator(".signal-quality-popup");
      await expect(popups).toHaveCount(1, { timeout: 10_000 });

      await signalButtons.nth(1).click();
      await expect(popups).toHaveCount(2, { timeout: 10_000 });

      // Capture both popup IDs so we can identify which one survives the
      // leave. The DOM id encodes the peer's session_id, so a stable
      // mapping is "first popup's id" -> peer A.
      const popupAId = await popups.nth(0).getAttribute("id");
      const popupBId = await popups.nth(1).getAttribute("id");
      expect(popupAId).toMatch(/^signal-quality-popup-/);
      expect(popupBId).toMatch(/^signal-quality-popup-/);
      expect(popupAId).not.toBe(popupBId);

      // Determine which popup belongs to which guest by reading the
      // popup title (the host's signal-meter button picks tiles in
      // top-to-bottom DOM order, but we don't depend on it — we'll
      // close the guest whose name matches popupA's title).
      const popupATitle = await popups.nth(0).locator(".popup-title").textContent();
      expect(popupATitle).toBeTruthy();

      // Close peer 1's page — they "leave" the meeting. The match between
      // popupA / popupB and the guest index is opaque (depends on join
      // order + session_id assignment), so we close BOTH guest pages one
      // at a time and verify the host's popup count tracks correctly.
      //
      // Approach:
      //   1. Find guest1's tile name in the host's grid.
      //   2. Decide which of popupA / popupB it owns.
      //   3. Close guest1's page; assert the matching popup is gone AND
      //      the other popup is still visible.

      // Simpler heuristic: close guest1's page first, wait for the host
      // to react (the host's grid drops from 2 to 1 peer tile), then
      // assert exactly one popup remains AND that popup is NOT the one
      // whose title contained guest1's display name.
      const guest1Name = profiles[1].name;
      const guest1IsPopupA = popupATitle?.includes(guest1Name) ?? false;

      // Close guest1's page (simulates leave).
      await members[1].page.close().catch(() => undefined);
      members[1].page = null as unknown as Page;
      await members[1].context.close().catch(() => undefined);

      // Wait for the host to react to the leave. We allow up to 15s for
      // the peer-removal callback to fire and the parent re-render to
      // settle. Pre-fix this would have collapsed BOTH popups; post-fix
      // exactly one should remain (the popup for the surviving peer).
      await expect(popups).toHaveCount(1, { timeout: 30_000 });

      // The surviving popup is the one for the OTHER guest (popupB if
      // popupA belonged to guest1, else popupA).
      const survivingId = await popups.first().getAttribute("id");
      expect(survivingId).toMatch(/^signal-quality-popup-/);
      if (guest1IsPopupA) {
        expect(survivingId).toBe(popupBId);
      } else {
        expect(survivingId).toBe(popupAId);
      }

      // Sanity: the popup is still visible (not just present in the DOM).
      await expect(popups.first()).toBeVisible();
    } finally {
      for (const m of members) {
        if (m.page) {
          await m.page.close().catch(() => undefined);
        }
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });
});
