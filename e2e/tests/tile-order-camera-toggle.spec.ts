import { test, expect, Page, chromium } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Tile-ordering stability across camera toggles.
 *
 * BEHAVIOR UNDER TEST:
 *   BEFORE the fix: tiles were rendered in three sequential loops (decoded,
 *   avatar, camera-off). Toggling a peer's camera moved it between loops,
 *   changing its DOM position and visually reordering the grid.
 *
 *   AFTER the fix: all tiles are merged into a SINGLE join-time-ordered
 *   render list via `build_unified_render_list()`. Camera on/off changes
 *   WHAT renders in a grid cell (live video vs camera-off placeholder) but
 *   NOT WHERE in the grid the peer appears. Tile order is stable.
 *
 * WHY REAL BROWSER CONTEXTS (not mock peers):
 *   Mock peers injected via the debug control bypass the camera partition
 *   entirely — they are layout-only placeholders appended via `mock_ids`
 *   and always feed the decode budget. They cannot reproduce a camera toggle
 *   because they have no actual camera state. Reproducing the reorder bug
 *   requires genuine remote peers with controllable cameras (via the
 *   `vc_prejoin_camera_on` localStorage flag and the in-meeting camera
 *   toggle button).
 *
 * DOM contract:
 *   - Peer tiles are `#grid-container .grid-item` elements.
 *   - Each tile's display name is in `h4.floating-name` (or its child
 *     `span.floating-name-text`).
 *   - DOM order of `.grid-item` children == visual grid order.
 *   - The camera toggle button has `data-testid="camera-toggle-button"`.
 */

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

  if (result === "waiting") return "waiting";
  if (result === "waiting-for-meeting") return "waiting-for-meeting";
  if (result === "auto-joined") return "in-meeting";

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (guestResult !== "waiting") return;

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

/**
 * Read the ordered list of display names from the grid tiles on the given page.
 * Returns names in DOM order (which == visual grid order).
 */
async function readTileOrder(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const tiles = Array.from(document.querySelectorAll("#grid-container .grid-item"));
    return tiles
      .map((tile) => {
        const nameEl =
          tile.querySelector<HTMLElement>(".floating-name-text") ||
          tile.querySelector<HTMLElement>("h4.floating-name");
        return nameEl?.textContent?.trim() ?? "";
      })
      .filter((name) => name.length > 0);
  });
}

test.describe("Tile ordering stability across camera toggles", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // ──────────────────────────────────────────────────────────────────────
  // Camera toggle does NOT reorder tiles.
  //
  // Three real browser peers join (host + guestA + guestB), all with
  // cameras ON. The host's grid shows two remote peer tiles in join-time
  // order. GuestA then toggles their camera OFF mid-call. The test asserts
  // that the tile order on the host's grid is UNCHANGED after the toggle.
  //
  // HOW THIS FAILS IF THE FIX IS REVERTED:
  //   If the three sequential render loops (visible_tiles, avatar_tiles,
  //   camera_off_tiles) were restored instead of the unified
  //   join-time-sorted loop, toggling GuestA's camera OFF would move its
  //   tile from the decoded bucket to the camera_off bucket, which renders
  //   AFTER the decoded bucket. GuestA's tile would jump to the end of the
  //   grid — the order assertion would fail because the names would be
  //   swapped or shifted.
  // ──────────────────────────────────────────────────────────────────────
  test("camera toggle does not reorder tiles in the grid @bvt1", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `tile_order_cam_toggle_${Date.now()}`;

    const browserHost = await chromium.launch({ args: BROWSER_ARGS });
    const browserA = await chromium.launch({ args: BROWSER_ARGS });
    const browserB = await chromium.launch({ args: BROWSER_ARGS });

    const hostCtx = await createAuthenticatedContext(
      browserHost,
      "tileorderhost@videocall.rs",
      "TileOrderHost",
      uiURL,
    );
    // Camera ON for host.
    await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

    const guestACtx = await createAuthenticatedContext(
      browserA,
      "tileordera@videocall.rs",
      "TileOrderGuestA",
      uiURL,
    );
    await guestACtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

    const guestBCtx = await createAuthenticatedContext(
      browserB,
      "tileorderb@videocall.rs",
      "TileOrderGuestB",
      uiURL,
    );
    await guestBCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

    const hostPage = await hostCtx.newPage();
    const guestAPage = await guestACtx.newPage();
    const guestBPage = await guestBCtx.newPage();

    try {
      // Host joins first.
      await navigateToMeeting(hostPage, meetingId, "TileOrderHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // GuestA joins second.
      await navigateToMeeting(guestAPage, meetingId, "TileOrderGuestA");
      const guestAResult = await joinMeetingFromPage(guestAPage);
      await admitGuestIfNeeded(hostPage, guestAPage, guestAResult);

      // GuestB joins last.
      await navigateToMeeting(guestBPage, meetingId, "TileOrderGuestB");
      const guestBResult = await joinMeetingFromPage(guestBPage);
      await admitGuestIfNeeded(hostPage, guestBPage, guestBResult);

      // Wait for both remote peer tiles to appear on the host's grid.
      // The host sees 2 remote tiles (its own tile is not rendered in the grid).
      const gridTiles = hostPage.locator("#grid-container .grid-item");
      await expect(gridTiles).toHaveCount(2, { timeout: 45_000 });

      // Wait for both tiles to have visible names (confirms rendering is
      // settled). In CI with fake media devices there may be no canvas, so
      // we check for the floating name instead of a live canvas.
      const tileWithName = hostPage.locator(
        "#grid-container .grid-item:has(.floating-name-text, h4.floating-name)",
      );
      await expect(tileWithName).toHaveCount(2, { timeout: 30_000 });

      // Record the tile order BEFORE the camera toggle.
      const orderBefore = await readTileOrder(hostPage);
      expect(
        orderBefore.length,
        `Precondition: expected 2 named tiles on the host grid, got ${orderBefore.length}`,
      ).toBe(2);

      // Verify that both guest names are present (order may vary depending on
      // join-time recording, but both must be visible).
      expect(orderBefore).toContain("TileOrderGuestA");
      expect(orderBefore).toContain("TileOrderGuestB");

      // ---- Toggle GuestA's camera OFF ----
      const guestACameraToggle = guestAPage.locator('[data-testid="camera-toggle-button"]');
      await expect(guestACameraToggle).toBeVisible({ timeout: 15_000 });
      await guestACameraToggle.click();

      // Wait for the host to register the camera-off state. The camera-off
      // peer's tile switches from a live canvas to a placeholder. We poll
      // until the tile for GuestA no longer contains a canvas (or shows the
      // camera-off placeholder text), confirming the camera state propagated.
      const guestATile = hostPage.locator("#grid-container .grid-item", {
        has: hostPage.locator(`text="TileOrderGuestA"`),
      });
      await expect(guestATile).toBeVisible({ timeout: 30_000 });

      // Wait for the camera-off state to propagate: GuestA's tile should
      // show a placeholder ("Video Disabled" or "Camera Off") instead of a
      // live canvas. Use poll to avoid a hard timeout.
      await expect
        .poll(
          async () => {
            const hasPlaceholder = await guestATile
              .locator(".placeholder-text")
              .isVisible()
              .catch(() => false);
            const hasCanvas =
              (await guestATile
                .locator("canvas")
                .count()
                .catch(() => 1)) > 0;
            // Camera-off state is confirmed when there's a placeholder OR no canvas.
            return hasPlaceholder || !hasCanvas;
          },
          {
            timeout: 30_000,
            message: "GuestA's tile on the host did not transition to camera-off state",
          },
        )
        .toBe(true);

      // Allow a brief settle for any render cycle to complete.
      await hostPage.waitForTimeout(2000);

      // ---- ASSERT: tile order is UNCHANGED after camera toggle ----
      const orderAfter = await readTileOrder(hostPage);
      expect(
        orderAfter.length,
        `Expected 2 tiles after camera toggle, got ${orderAfter.length}`,
      ).toBe(2);

      // The critical assertion: tile order must be identical before and after
      // the camera toggle. If the old three-loop rendering were restored,
      // GuestA (now camera-off) would move to the end of the grid.
      expect(orderAfter).toEqual(orderBefore);
    } finally {
      await browserHost.close();
      await browserA.close();
      await browserB.close();
    }
  });
});
