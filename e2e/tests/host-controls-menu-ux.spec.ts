import { test, expect, chromium, BrowserContext, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Coverage for two recent UX changes to the host-control three-dot menus:
 *
 * 1. Click-outside-to-close
 *    Every open three-dot context menu (in-call peer-list header, per-tile
 *    grid menu, peer-list-item menu) now renders a full-screen transparent
 *    backdrop div (`position: fixed; inset: 0`) just before the menu itself.
 *    Clicking the backdrop closes the menu via the backdrop's `onclick`
 *    handler. This spec exercises the in-call header menu.
 *
 * 2. Host controls hidden on the host's *own* sibling-session tile
 *    `is_self_peer` in `peer_tile.rs` is now true when EITHER the tile's
 *    session_id matches the local session_id, OR the tile's user_id matches
 *    the local user_id. The user_id branch catches the case where the host
 *    has two tabs open authenticated as the same account — the second tab's
 *    tile must not show Mute / Disable video / Remove from meeting on the first tab.
 */

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

async function joinMeetingFromPage(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const which = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "grid" as const),
  ]);

  if (which === "join") {
    await page.waitForTimeout(500);
    if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
      await joinButton.click().catch(() => undefined);
    }
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/**
 * Open the peer-list sidebar by clicking the "Open Peers" video-controls
 * button. The button is gated by the auto-hide behaviour of
 * `.controls-secondary` (collapses after 1s of mouse-inactivity), so we wake
 * the controls bar by hovering and moving the mouse before clicking.
 */
async function openPeerListSidebar(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.mouse.move(400, 400);
  await page.waitForTimeout(300);

  const openPeersBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Open Peers" }),
  });
  await expect(openPeersBtn).toBeVisible({ timeout: 10_000 });
  await openPeersBtn.click();
  await page.waitForTimeout(800);
}

// ---------------------------------------------------------------------------
// Same-user multi-session helpers (mirrors same-user-multi-session.spec.ts)
// ---------------------------------------------------------------------------

const SAME_USER_EMAIL = "host-multi-tab@videocall.rs";
const SAME_USER_NAME = "HostMultiTab";

async function openSameUserContext(
  uiURL: string,
): Promise<{ context: BrowserContext; page: Page }> {
  const browser = await chromium.launch({ args: BROWSER_ARGS });
  const context = await createAuthenticatedContext(browser, SAME_USER_EMAIL, SAME_USER_NAME, uiURL);
  const page = await context.newPage();
  (context as unknown as { _browser?: typeof browser })._browser = browser;
  return { context, page };
}

async function closeSameUserContext(context: BrowserContext): Promise<void> {
  const browser = (context as unknown as { _browser?: Awaited<ReturnType<typeof chromium.launch>> })
    ._browser;
  await context.close().catch(() => undefined);
  if (browser) {
    await browser.close().catch(() => undefined);
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Host context menus — click-outside-to-close", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Behavior 1: clicking outside an open three-dot menu closes it.
   *
   * Setup: a single host joins an empty meeting and opens the in-call header
   * three-dot menu (`button.menu-button[aria-label="Host actions"]` inside
   * `.in-call-header`). This menu renders Mute-all / Disable-video-for-all
   * items and is the menu that received the backdrop change.
   *
   * Acting host-alone keeps the test deterministic — no guest is required for
   * the in-call header menu to render (it depends only on
   * `is_current_user_host`, not on the presence of peers).
   *
   * Assertions:
   *  - Before opening: the `.context-menu` element is absent.
   *  - After clicking the menu-button: `.context-menu` is visible AND a
   *    sibling fixed-inset backdrop div is present.
   *  - After clicking outside (we click a coordinate well away from the menu,
   *    near the top-left corner): the `.context-menu` disappears and the
   *    backdrop disappears.
   *  - Clicking the toggle button a second time re-opens the menu (sanity
   *    check that the close path did not break the open path).
   */
  test("clicking outside the in-call header menu closes it", async ({ baseURL }) => {
    test.setTimeout(90_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_menu_backdrop_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser,
        "host-menu-backdrop@videocall.rs",
        "BackdropHost",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();

      // ---- Host joins alone ----
      await navigateToMeeting(hostPage, meetingId, "BackdropHost");
      await joinMeetingFromPage(hostPage);
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // ---- Open the peer-list sidebar so the in-call header is visible ----
      await openPeerListSidebar(hostPage);

      // ---- Locate the in-call header host-actions toggle ----
      // Two `.menu-button` elements live inside the sidebar:
      //   - `.sidebar-header .menu-button` (aria-label="More options"), and
      //   - `.in-call-header .menu-button[aria-label="Host actions"]`.
      // Scope to the in-call one explicitly — it is the menu that received
      // the new backdrop behaviour.
      const inCallToggle = hostPage
        .locator(".in-call-header")
        .locator('button.menu-button[aria-label="Host actions"]');
      await expect(inCallToggle).toBeVisible({ timeout: 10_000 });

      // Scope assertions to the in-call menu wrapper to avoid matching the
      // sidebar-header's context menu (which is a different feature).
      const inCallMenuWrapper = hostPage.locator(".in-call-menu-wrapper");
      const menuItems = inCallMenuWrapper.locator(".context-menu");
      // The backdrop has no class; it is identified by its inline style.
      const backdrop = inCallMenuWrapper.locator(
        'div[style*="position: fixed"][style*="inset: 0"]',
      );

      // ---- Pre-open: menu and backdrop are not in the DOM ----
      await expect(menuItems).toHaveCount(0);
      await expect(backdrop).toHaveCount(0);

      // ---- Open the menu ----
      await inCallToggle.click();
      await expect(menuItems).toBeVisible({ timeout: 5_000 });
      await expect(backdrop).toHaveCount(1);

      // Confirm the menu rendered its host-only items so we know we have the
      // right menu open (not a sibling popover the test infra might surface).
      await expect(
        inCallMenuWrapper.locator("button.context-menu-item", { hasText: "Mute all" }),
      ).toBeVisible({ timeout: 5_000 });

      // ---- Click outside the menu via the backdrop ----
      // The backdrop covers the entire viewport (inset: 0). Click near the
      // top-left corner — well away from both the toggle and the menu
      // itself, which are anchored to the right side of the sidebar header.
      await hostPage.mouse.click(20, 20);

      // ---- Menu and backdrop are removed from the DOM ----
      await expect(menuItems).toHaveCount(0, { timeout: 5_000 });
      await expect(backdrop).toHaveCount(0, { timeout: 5_000 });

      // ---- Re-open works (open path was not broken by the close handler) ----
      await inCallToggle.click();
      await expect(menuItems).toBeVisible({ timeout: 5_000 });
      await expect(backdrop).toHaveCount(1);
    } finally {
      await browser.close();
    }
  });

  /**
   * Behavior 2: a host with two browser tabs (same user_id, different
   * session_id) does NOT see Mute / Disable video / Remove from meeting on their own
   * sibling-session tile.
   *
   * `is_self_peer` in `peer_tile.rs` is now true when EITHER the tile's
   * session_id matches the local session_id OR the tile's user_id matches
   * the local user_id. The on_mute / on_disable_video / on_kick handlers are
   * all gated by `!is_self_peer`, so none of the three host actions should
   * surface on a sibling-tab tile.
   *
   * The CSS for the menu wrapper is hover-gated (`.tile-mute-btn` has
   * `visibility: hidden` until `.grid-item:hover`), so we hover the
   * sibling-session tile before asserting. The assertion is on element
   * COUNT — even after hover the button must not exist in the DOM when
   * `on_mute`/`on_disable_video`/`on_kick` are all `None` (the entire
   * `.tile-mute-menu-wrapper` is gated by `if on_mute.is_some() || …`).
   *
   * Implementation note: the host is the meeting owner (first to join), so
   * `is_current_user_host` is true for both tabs. Without the user_id
   * branch of `is_self_peer`, the second tab's tile WOULD render the
   * `.tile-mute-btn` because its session_id differs from the viewing tab's
   * session_id. This test fails if the user_id branch is reverted.
   */
  test("host's sibling-tab tile does not show Mute / Disable video / Remove from meeting", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_host_self_sibling_${Date.now()}`;

    const sessionA = await openSameUserContext(uiURL);
    const sessionB = await openSameUserContext(uiURL);

    try {
      // ---- Session A joins first → becomes meeting host ----
      await navigateToMeeting(sessionA.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionA.page);
      await expect(sessionA.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // ---- Session B (same user_id, different session_id) joins ----
      await navigateToMeeting(sessionB.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionB.page);
      await expect(sessionB.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // ---- Wait for peer-tile discovery: each side must see the sibling
      // tile (peer-video-{session_id}-div) ----
      await expect(sessionA.page.locator('[id^="peer-video-"][id$="-div"]').first()).toBeVisible({
        timeout: 30_000,
      });
      await expect(sessionB.page.locator('[id^="peer-video-"][id$="-div"]').first()).toBeVisible({
        timeout: 30_000,
      });

      // Allow PARTICIPANT_JOINED fan-out + host-state propagation.
      await sessionA.page.waitForTimeout(3_000);

      // ---- On session A: hover the sibling (session B) tile, assert NO
      // host-actions button is rendered ----
      // The peer tile id is `peer-video-{other_session_id}-div`. With only
      // two same-user sessions in the room there is exactly one peer tile
      // per side — that tile is the sibling.
      const siblingTileA = sessionA.page.locator('[id^="peer-video-"][id$="-div"]').first();
      await expect(siblingTileA).toBeVisible({ timeout: 15_000 });
      await siblingTileA.hover();
      await sessionA.page.waitForTimeout(500);

      // The whole .tile-mute-menu-wrapper is suppressed when on_mute,
      // on_disable_video, AND on_kick are all None — i.e. when is_self_peer.
      // Assert zero `.tile-mute-btn` inside the sibling tile.
      await expect(siblingTileA.locator(".tile-mute-btn")).toHaveCount(0);
      await expect(siblingTileA.getByTitle("Host actions")).toHaveCount(0);

      // ---- Symmetric: on session B, the session-A tile must also not show
      // host actions (both sides are the same authed user) ----
      const siblingTileB = sessionB.page.locator('[id^="peer-video-"][id$="-div"]').first();
      await expect(siblingTileB).toBeVisible({ timeout: 15_000 });
      await siblingTileB.hover();
      await sessionB.page.waitForTimeout(500);

      await expect(siblingTileB.locator(".tile-mute-btn")).toHaveCount(0);
      await expect(siblingTileB.getByTitle("Host actions")).toHaveCount(0);

      // ---- Sanity: each side's self-tile also must not show host actions
      // (same-session is_self_peer branch — pre-existing behaviour). With
      // only two same-user tabs and `is_self_peer` true for both peer tiles
      // and the self tile, there must be zero `.tile-mute-btn` anywhere on
      // either page. ----
      await expect(sessionA.page.locator(".tile-mute-btn")).toHaveCount(0);
      await expect(sessionB.page.locator(".tile-mute-btn")).toHaveCount(0);
    } finally {
      await closeSameUserContext(sessionA.context);
      await closeSameUserContext(sessionB.context);
    }
  });
});
