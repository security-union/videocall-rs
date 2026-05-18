import { test, expect, chromium, BrowserContext, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Regression coverage for HCL issue #828 — "same authed user multiple times not
 * shown as separate instance in same meeting".
 *
 * Before the fix, `actix-api/src/actors/chat_server.rs` evicted the prior
 * session of the same `user_id` in the `JoinRoom` handler via
 * `evict_same_user_session()`. The eviction was silent (no PARTICIPANT_LEFT
 * broadcast), so when the second tab joined, neither tab saw the other:
 *  - the first tab was kicked off the room without notification,
 *  - the second tab joined a "room of one" and saw only its own self-tile.
 *
 * After the fix:
 *  - the server allows multiple sessions per `user_id` and broadcasts a
 *    PARTICIPANT_JOINED for each session (distinct `session_id`),
 *  - `is_duplicate_peer_event` in `videocall-client/src/client/video_call_client.rs`
 *    now keys on `(event_type, user_id, Some(session_id))` for JOINED/LEFT so
 *    the second session's join is no longer dedup'd into the first session's.
 *
 * Peer tiles in the Dioxus UI are keyed by `session_id` (see
 * `dioxus-ui/src/components/canvas_generator.rs` — tile id is
 * `peer-video-{session_id}-div`), so two sessions of the same user render as
 * two distinct peer tiles.
 *
 * This spec opens N independent BrowserContexts, all authenticated as the
 * *same* `user_id` via the same JWT cookie, joins them into one meeting, and
 * asserts each side sees a peer tile for every other session (N total tiles =
 * self + (N-1) peers).
 *
 * If the fix is reverted, the server would evict the prior session on each
 * JoinRoom, and `expect(tileCount).toBeGreaterThanOrEqual(2)` would fail on
 * every side.
 *
 * Both tests are restricted to the Dioxus UI (port 3001). The Yew UI is no
 * longer the active deployment, and the multi-session render path is part of
 * the Dioxus surface.
 */

const SAME_USER_EMAIL = "same-user-828@videocall.rs";
const SAME_USER_NAME = "MultiSessionUser";

const PEER_TILE_SELECTOR = ".split-peer-tile, #grid-container .canvas-container";

/**
 * Navigate to the home page, fill in the meeting id + display name, and submit.
 * Mirrors `navigateToMeeting` from `display-name-update.spec.ts` /
 * `two-users-meeting.spec.ts` so behaviour stays consistent across specs.
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
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });
  await page.waitForTimeout(1500);
}

/**
 * Wait for the pre-join card to appear (Start Meeting / Join Meeting) or for
 * the grid to be auto-mounted, then click Join if needed. Returns once the
 * grid container is visible.
 *
 * This is the same shape as the helper used in the other multi-context specs
 * but trimmed — same-user sessions cannot end up in the waiting room (the
 * authenticated path is shared by all sessions of the same user, so no
 * admission step is required).
 */
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
      await joinButton.click().catch(() => {
        // Swallow click-after-detach: the auto-join effect already
        // transitioned past NotJoined and unmounted the button.
      });
    }
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/**
 * Count visible peer/self tiles on a page. Uses the union selector that the
 * other specs use (`speaker-highlight.spec.ts:251`,
 * `display-name-update.spec.ts:278`) so both grid layout and split layout
 * count correctly.
 *
 * `peer-video-*-div` and `screen-share-*-div` are the only ids the canvas
 * generator emits; mock-peer infrastructure does not render through this
 * code path in the E2E stack, so the union selector counts only real tiles.
 */
async function countVisibleTiles(page: Page): Promise<number> {
  // Wait for at least one tile, then count all visible ones.
  await expect(page.locator(PEER_TILE_SELECTOR).first()).toBeVisible({ timeout: 30_000 });
  return await page.locator(PEER_TILE_SELECTOR).count();
}

/**
 * Open a new BrowserContext authenticated as the canonical same-user identity
 * (`SAME_USER_EMAIL` / `SAME_USER_NAME`). All contexts share the same JWT
 * `sub`/`user_id`, so the meeting-api treats every tab as the same authed user.
 */
async function openSameUserContext(
  uiURL: string,
): Promise<{ context: BrowserContext; page: Page }> {
  const browser = await chromium.launch({ args: BROWSER_ARGS });
  const context = await createAuthenticatedContext(browser, SAME_USER_EMAIL, SAME_USER_NAME, uiURL);
  const page = await context.newPage();
  // Keep the browser reachable for cleanup via the context.
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

test.describe("Same authed user — multiple sessions in one meeting", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Two tabs / two browser contexts authenticated as the same user join the
   * same meeting. Each side must see the other's session as a distinct peer
   * tile (self + 1 peer = 2 tiles per side).
   *
   * Reverting `actix-api/src/actors/chat_server.rs` (re-introducing the
   * `evict_same_user_session` call) makes this assertion fail: the second
   * session's join evicts the first session silently, the first context's
   * grid never sees a peer tile, and the second context only ever sees its
   * own self-tile.
   */
  test("two same-user sessions render as two distinct peer tiles on each side", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_same_user_2_${Date.now()}`;

    const sessionA = await openSameUserContext(uiURL);
    const sessionB = await openSameUserContext(uiURL);

    try {
      // ---- Session A joins first ----
      await navigateToMeeting(sessionA.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionA.page);
      await expect(sessionA.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // ---- Session B joins the same meeting as the same user ----
      await navigateToMeeting(sessionB.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionB.page);
      await expect(sessionB.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Allow time for PARTICIPANT_JOINED broadcasts and peer discovery on both sides.
      await sessionA.page.waitForTimeout(5_000);

      // ---- Each side must see at least 2 tiles (self + the other session) ----
      const tileCountA = await countVisibleTiles(sessionA.page);
      const tileCountB = await countVisibleTiles(sessionB.page);

      expect(
        tileCountA,
        "Session A should see its own self-tile plus session B's peer tile",
      ).toBeGreaterThanOrEqual(2);
      expect(
        tileCountB,
        "Session B should see its own self-tile plus session A's peer tile",
      ).toBeGreaterThanOrEqual(2);

      // ---- The peer tile id on each side must include the OTHER session_id ----
      // Peer tiles are rendered with id="peer-video-{session_id}-div" (see
      // canvas_generator.rs). Each side should have at least one peer tile
      // div, and the ids on the two sides must be distinct (different session_id).
      const peerIdsA = await sessionA.page
        .locator('[id^="peer-video-"][id$="-div"]')
        .evaluateAll((els) => els.map((e) => e.id));
      const peerIdsB = await sessionB.page
        .locator('[id^="peer-video-"][id$="-div"]')
        .evaluateAll((els) => els.map((e) => e.id));

      expect(
        peerIdsA.length,
        "Session A must render a peer tile for session B",
      ).toBeGreaterThanOrEqual(1);
      expect(
        peerIdsB.length,
        "Session B must render a peer tile for session A",
      ).toBeGreaterThanOrEqual(1);

      // The peer tile A sees (session B's session_id) must not match any peer
      // tile id on B (which would be A's session_id). They must be disjoint —
      // a same-id on both sides would mean only one session is registered.
      const overlap = peerIdsA.filter((id) => peerIdsB.includes(id));
      expect(
        overlap,
        "Session A and Session B must have disjoint peer-tile ids (distinct session_ids)",
      ).toEqual([]);

      // ---- Neither side should have been silently evicted ----
      // The pre-fix behaviour evicted the first session without a
      // PARTICIPANT_LEFT, which surfaced in the client as "Connection lost"
      // / reconnect spinners. Assert neither symptom is present.
      await expect(sessionA.page.getByText("Connection lost")).toHaveCount(0);
      await expect(sessionB.page.getByText("Connection lost")).toHaveCount(0);
    } finally {
      await closeSameUserContext(sessionA.context);
      await closeSameUserContext(sessionB.context);
    }
  });

  /**
   * Three tabs / three browser contexts authenticated as the same user join the
   * same meeting. This locks in the exact bug-report scenario verbatim:
   * "I used 3 laptops … expected to see 2 attendees + self (3 total)".
   *
   * Each side should see ≥ 3 tiles (self + 2 peers) and all three peer-tile
   * session ids should be distinct.
   */
  test("three same-user sessions render as three distinct peer tiles per side", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_same_user_3_${Date.now()}`;

    const sessionA = await openSameUserContext(uiURL);
    const sessionB = await openSameUserContext(uiURL);
    const sessionC = await openSameUserContext(uiURL);

    try {
      // ---- All three sessions join sequentially ----
      await navigateToMeeting(sessionA.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionA.page);
      await expect(sessionA.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      await navigateToMeeting(sessionB.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionB.page);
      await expect(sessionB.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      await navigateToMeeting(sessionC.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionC.page);
      await expect(sessionC.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Allow time for PARTICIPANT_JOINED fan-out to all three sessions.
      await sessionA.page.waitForTimeout(7_000);

      // ---- Each side must see at least 3 tiles (self + 2 peers) ----
      const tileCountA = await countVisibleTiles(sessionA.page);
      const tileCountB = await countVisibleTiles(sessionB.page);
      const tileCountC = await countVisibleTiles(sessionC.page);

      expect(
        tileCountA,
        "Session A should see itself plus 2 other sessions",
      ).toBeGreaterThanOrEqual(3);
      expect(
        tileCountB,
        "Session B should see itself plus 2 other sessions",
      ).toBeGreaterThanOrEqual(3);
      expect(
        tileCountC,
        "Session C should see itself plus 2 other sessions",
      ).toBeGreaterThanOrEqual(3);

      // ---- Each side must render 2 peer tiles, each with a distinct session_id ----
      const peerIdsA = await sessionA.page
        .locator('[id^="peer-video-"][id$="-div"]')
        .evaluateAll((els) => els.map((e) => e.id));
      const peerIdsB = await sessionB.page
        .locator('[id^="peer-video-"][id$="-div"]')
        .evaluateAll((els) => els.map((e) => e.id));
      const peerIdsC = await sessionC.page
        .locator('[id^="peer-video-"][id$="-div"]')
        .evaluateAll((els) => els.map((e) => e.id));

      expect(peerIdsA.length, "Session A should render 2 peer tiles").toBeGreaterThanOrEqual(2);
      expect(peerIdsB.length, "Session B should render 2 peer tiles").toBeGreaterThanOrEqual(2);
      expect(peerIdsC.length, "Session C should render 2 peer tiles").toBeGreaterThanOrEqual(2);

      // Each side's peer-tile id set must contain distinct ids (no duplicates),
      // i.e. the two peers are not collapsed into one tile.
      expect(new Set(peerIdsA).size).toBe(peerIdsA.length);
      expect(new Set(peerIdsB).size).toBe(peerIdsB.length);
      expect(new Set(peerIdsC).size).toBe(peerIdsC.length);

      // No session should report a "Connection lost" reconnect — the pre-fix
      // eviction would surface as a connection-lost indicator on the
      // earlier-joining session.
      await expect(sessionA.page.getByText("Connection lost")).toHaveCount(0);
      await expect(sessionB.page.getByText("Connection lost")).toHaveCount(0);
      await expect(sessionC.page.getByText("Connection lost")).toHaveCount(0);
    } finally {
      await closeSameUserContext(sessionA.context);
      await closeSameUserContext(sessionB.context);
      await closeSameUserContext(sessionC.context);
    }
  });
});
